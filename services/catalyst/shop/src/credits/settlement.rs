// from: credits-server@465e628bc:src/logic/usd-reconciler.ts (settle on indexed consumption, recover a released intent), src/logic/credits-revoker.ts (revokeCredits by credit hash), offchain-marketplace-contract@87350c63e:src/credits/CreditsManagerPolygon.sol (the flags, roles and allowlist the gate reads); docs/shop/CONTRACTS.md ("Checks a dedicated manager must pass", "No spendable remainder")
//! Everything after the signature:
//!
//! - **The issuance gate** ([`IssuanceGate`], [`ChainGate`]): before any
//!   credit is signed, the configured CreditsManager is read on chain:
//!   `secondarySalesAllowed()` false, `primarySalesAllowed()` true, not
//!   paused, our signer holds `CREDITS_SIGNER_ROLE`, the known pass-through
//!   NAME executors are not allowlisted, the manager is not one of the shared
//!   upstream managers, and the endpoint serves the configured chain. Any
//!   failed or unreadable check refuses issuance (fail closed); the verdict
//!   is memoised for [`GATE_TTL`], so a flipped flag stops issuance within a
//!   minute.
//! - **Settlement** ([`settle`]): the CreditUsed logs the index wrote
//!   (`credits.usages`) become ledger effects exactly once per log
//!   (`credits.usage_settlements`, PK = the log's key, written in the
//!   transaction of the debit). The first log of a salt settles the group's
//!   purchase (its charged cents, the ledger key `credit-used:<salt>`), any
//!   later log a remainder the buyer spent directly, at the credit's oracle
//!   rate rounded up to whole credits. The authorization becomes `consumed`
//!   whatever it was (`retired`, `released`, `expired`: a cancel or an expiry
//!   is not proof a signed credit went unused), and its hash is queued for
//!   revocation. A debit the balance cannot cover rolls back and is retried
//!   by the next pass and the reconciler.
//! - **Revocation** ([`Revoker`], the `credits-settlement` job): the outbox
//!   `credits.revocations` is drained with `revokeCredits(bytes32[])` from
//!   the `CREDITS_REVOKER_KEY_FILE` signer when one is configured. The row is
//!   claimed, the broadcast runs outside the claim, and `revokeCredits` is
//!   idempotent on chain, so a kill between broadcast and the `sent` write
//!   costs a harmless re-send, never a missed revocation.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolValue, sol};
use async_trait::async_trait;
use sqlx::Row;

use super::errors::ApiError;
use super::reconcile::CreditUsageDiff;
use super::store::{Credits, now};
use crate::evm::chain::ChainComponent;
use crate::evm::signer::{DirectSigner, ReceiptOutcome};
use crate::jobs::{JobContext, Jobs, PeriodicCfg};
use crate::policy_entry::EXECUTOR_EXECUTE_SELECTOR;
use crate::shared::cache::TtlMap;
use crate::shared::writes::{PassGate, WRITES_DISABLED_MESSAGE, ops_refusal};
use crate::types::{ManaUsdRate, ManaWei, UsdCents};

pub const GATE_SOURCE: &str = "credits-manager-gate";
pub const GATE_CONFIGURE: &str = "CREDITS_MANAGER_CONTRACT";
pub const GATE_TTL: Duration = Duration::from_secs(60);
pub const JOB_NAME: &str = "credits-settlement";
/// Revocations one pass drives at most.
const REVOCATIONS_PER_PASS: i64 = 20;
const REVOCATION_MAX_ATTEMPTS: i32 = 10;
const REVOCATION_GAS_CAP: u64 = 300_000;
/// Logs one settlement statement lists (keyset over the log key).
const SETTLE_BATCH: i64 = 200;

/// The shared upstream managers v2 refuses to issue credits for
/// (docs/shop/CONTRACTS.md, "Refuse shared managers").
pub const SHARED_MANAGERS: [&str; 4] = [
    "0x8b3a40ca1b6f5cafc99d112a4d02e897d1fd8cc5",
    "0xe9f961e6ded4e1476bbee4faab886d63a2493eb9",
    "0x8052a560e6e6ac86eeb7e711a4497f639b322fb3",
    "0x037566bc90f85e76587e1b07f9184585f09c1420",
];

/// The pass-through NAME executors (CORAL/Squid and Across), whose
/// `execute` selector is [`EXECUTOR_EXECUTE_SELECTOR`]: a manager that
/// allowlists one lets a custom call
/// reach a resale (CONTRACTS.md, "Can a custom call reach a resale?").
pub const PASS_THROUGH_EXECUTORS: [&str; 3] = [
    "0x84c682b1b862859730446bca5b853a5693aefc84",
    "0xc9a9c416fbfd424d09e3f905142799abb10c2fa7",
    "0x5e359ebd4cc379f46020fac720261dc9c1d80404",
];

sol! {
    interface ICreditsManager {
        function primarySalesAllowed() external view returns (bool);
        function secondarySalesAllowed() external view returns (bool);
        function paused() external view returns (bool);
        function hasRole(bytes32 role, address account) external view returns (bool);
        function allowedCustomExternalCalls(address target, bytes4 selector) external view returns (bool);
        function revokeCredits(bytes32[] creditHashes) external;
        function isRevoked(bytes32 creditHash) external view returns (bool);
    }
}

pub fn credits_signer_role() -> B256 {
    keccak256(b"CREDITS_SIGNER_ROLE")
}

/// `revokeCredits([hash])` calldata.
pub fn revoke_calldata(credit_hash: B256) -> Bytes {
    Bytes::from(
        ICreditsManager::revokeCreditsCall {
            creditHashes: vec![credit_hash],
        }
        .abi_encode(),
    )
}

/// Whether the configured manager may have credits issued against it now.
#[async_trait]
pub trait IssuanceGate: Send + Sync {
    /// `Err(reason)` refuses; the reason names the check, never a URL.
    async fn check(&self, manager: Address, signer: Address) -> Result<(), String>;
}

/// The gate over a JSON-RPC endpoint of the credits chain.
pub struct ChainGate {
    chain: Arc<ChainComponent>,
    chain_id: u64,
    memo: TtlMap<(Address, Address), Result<(), String>>,
}

impl ChainGate {
    pub fn new(chain: Arc<ChainComponent>, chain_id: u64) -> Self {
        Self {
            chain,
            chain_id,
            memo: TtlMap::new(GATE_TTL),
        }
    }

    async fn read_bool(&self, manager: Address, data: Vec<u8>, what: &str) -> Result<bool, String> {
        let bytes = self
            .chain
            .eth_call(manager, &data)
            .await
            .map_err(|e| format!("{what} could not be read ({})", redact(&e.to_string())))?;
        bool::abi_decode(&bytes)
            .map_err(|_| format!("{what} answered {} bytes, not a bool", bytes.len()))
    }

    async fn evaluate(&self, manager: Address, signer: Address) -> Result<(), String> {
        let lower = format!("{manager:#x}");
        if SHARED_MANAGERS.contains(&lower.as_str()) {
            return Err(format!(
                "CREDITS_MANAGER_CONTRACT {lower} is a shared upstream manager (docs/shop/CONTRACTS.md)"
            ));
        }
        let chain =
            self.chain.get_chain_id().await.map_err(|e| {
                format!("eth_chainId could not be read ({})", redact(&e.to_string()))
            })?;
        if chain != self.chain_id {
            return Err(format!(
                "the RPC endpoint serves chain {chain}, the credits chain is {}",
                self.chain_id
            ));
        }
        if self
            .read_bool(
                manager,
                ICreditsManager::pausedCall {}.abi_encode(),
                "paused()",
            )
            .await?
        {
            return Err("the CreditsManager is paused".to_string());
        }
        if !self
            .read_bool(
                manager,
                ICreditsManager::primarySalesAllowedCall {}.abi_encode(),
                "primarySalesAllowed()",
            )
            .await?
        {
            return Err(
                "primarySalesAllowed() is false: no primary purchase can settle".to_string(),
            );
        }
        if self
            .read_bool(
                manager,
                ICreditsManager::secondarySalesAllowedCall {}.abi_encode(),
                "secondarySalesAllowed()",
            )
            .await?
        {
            return Err(
                "secondarySalesAllowed() is true: a credit could be spent on a resale".to_string(),
            );
        }
        let role = ICreditsManager::hasRoleCall {
            role: credits_signer_role(),
            account: signer,
        }
        .abi_encode();
        if !self
            .read_bool(manager, role, "hasRole(CREDITS_SIGNER_ROLE, signer)")
            .await?
        {
            return Err(format!(
                "the configured signer {signer:#x} does not hold CREDITS_SIGNER_ROLE on the manager"
            ));
        }
        for executor in PASS_THROUGH_EXECUTORS {
            let target = Address::from_str(executor).expect("a pinned address");
            let call = ICreditsManager::allowedCustomExternalCallsCall {
                target,
                selector: EXECUTOR_EXECUTE_SELECTOR.into(),
            }
            .abi_encode();
            if self
                .read_bool(
                    manager,
                    call,
                    "allowedCustomExternalCalls(executor, execute)",
                )
                .await?
            {
                return Err(format!(
                    "the manager allowlists the pass-through executor {executor}: a custom call could reach a resale"
                ));
            }
        }
        Ok(())
    }
}

fn redact(message: &str) -> String {
    let mut out = String::new();
    for word in message.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(if word.contains("://") { "<url>" } else { word });
    }
    out.chars().take(200).collect()
}

#[async_trait]
impl IssuanceGate for ChainGate {
    async fn check(&self, manager: Address, signer: Address) -> Result<(), String> {
        let verdict: Result<Result<(), String>, ()> = self
            .memo
            .get_or_fetch((manager, signer), || async {
                Ok::<_, ()>(self.evaluate(manager, signer).await)
            })
            .await;
        verdict.unwrap_or_else(|_| Err("gate unavailable".to_string()))
    }
}

/// No endpoint to read the manager from: issuance stays refused.
pub struct ClosedGate;

#[async_trait]
impl IssuanceGate for ClosedGate {
    async fn check(&self, _manager: Address, _signer: Address) -> Result<(), String> {
        Err("no RPC endpoint reads the CreditsManager (RPC_ENDPOINT_POLYGON with CREDITS_USAGE_INDEX_ENABLED)".to_string())
    }
}

/// The tests' gate: a fixed verdict.
pub struct FixedGate(pub Result<(), String>);

#[async_trait]
impl IssuanceGate for FixedGate {
    async fn check(&self, _manager: Address, _signer: Address) -> Result<(), String> {
        self.0.clone()
    }
}

/// One CreditUsed log awaiting settlement.
#[derive(Debug, Clone)]
struct Candidate {
    block_number: i64,
    log_index: i32,
    salt: String,
    sender: String,
    value_wei: String,
}

const CANDIDATES_SQL: &str = "SELECT u.block_number, u.log_index, u.salt, u.sender, u.value_wei::text AS value_wei \
     FROM credits.usages u \
     JOIN credits.authorizations a ON a.id = u.salt \
     LEFT JOIN credits.usage_settlements s ON s.chain_id = u.chain_id AND s.block_number = u.block_number AND s.log_index = u.log_index \
     WHERE u.chain_id = $1 AND u.contract = $2 AND u.block_number > $3 AND u.block_number <= $4 \
       AND s.chain_id IS NULL AND (u.block_number, u.log_index) > ($5, $6) \
     ORDER BY u.block_number, u.log_index \
     LIMIT $7";

/// Settles the logs of `(since, through]` not yet settled, in log order;
/// every attempt is reported, a refused debit with `consumed: false`.
pub async fn settle(
    credits: &Credits,
    chain_id: i64,
    contract: &str,
    since: i64,
    through: i64,
) -> Result<Vec<CreditUsageDiff>, ApiError> {
    let mut out = Vec::new();
    let (mut after_block, mut after_log) = (-1i64, -1i32);
    loop {
        let rows = sqlx::query(CANDIDATES_SQL)
            .bind(chain_id)
            .bind(contract)
            .bind(since)
            .bind(through)
            .bind(after_block)
            .bind(after_log)
            .bind(SETTLE_BATCH)
            .fetch_all(&credits.pool)
            .await?;
        let n = rows.len();
        for r in rows {
            let candidate = Candidate {
                block_number: r.get("block_number"),
                log_index: r.get("log_index"),
                salt: r.get("salt"),
                sender: r.get("sender"),
                value_wei: r.get("value_wei"),
            };
            after_block = candidate.block_number;
            after_log = candidate.log_index;
            out.push(credits.settle_usage(chain_id, contract, &candidate).await?);
        }
        if (n as i64) < SETTLE_BATCH {
            return Ok(out);
        }
    }
}

/// Whole credits a remainder of `value_wei` costs at the credit's oracle
/// rate: USD cents rounded up, then up to a whole credit; a positive
/// remainder below a cent (the rate conversion floors at a USD-wei) still
/// costs one, so no spend settles free. `None` for a row without a rate (a
/// pre-protocol pledge, never spendable on chain).
pub fn remainder_credits(
    value_wei: &str,
    oracle_rate: Option<&str>,
    oracle_decimals: Option<i16>,
) -> Option<i64> {
    let rate = ManaUsdRate::new(
        U256::from_str_radix(oracle_rate?, 10).ok()?,
        u8::try_from(oracle_decimals?).ok()?,
    )?;
    let value = ManaWei::new(U256::from_str_radix(value_wei, 10).ok()?);
    Some(match value.usd_cents_ceil(rate)?.charge_for_quote() {
        Some(charge) => charge.credits_ceil(),
        None => i64::from(!value.is_zero()),
    })
}

impl Credits {
    /// One log's settlement in one transaction: the authorization row is
    /// locked, the kind decided from the settlements already recorded, the
    /// debit made under an idempotency key, the settlement row inserted, the
    /// status flipped to `consumed` and the revocation queued. A concurrent
    /// pass loses on the settlement PK and moves nothing.
    async fn settle_usage(
        &self,
        chain_id: i64,
        contract: &str,
        c: &Candidate,
    ) -> Result<CreditUsageDiff, ApiError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT address, usd_cents, oracle_rate::text AS oracle_rate, oracle_decimals, credit_hash, status, \
                    (SELECT count(*) FROM credits.usage_settlements s WHERE s.authorization_id = a.id)::bigint AS settled \
             FROM credits.authorizations a WHERE id = $1 FOR UPDATE",
        )
        .bind(&c.salt)
        .fetch_one(&mut *tx)
        .await?;
        let address: String = row.get("address");
        let usd_cents: i64 = row.get("usd_cents");
        let oracle_rate: Option<String> = row.get("oracle_rate");
        let oracle_decimals: Option<i16> = row.get("oracle_decimals");
        let credit_hash: Option<String> = row.get("credit_hash");
        let settled: i64 = row.get("settled");
        if c.sender != address {
            tracing::warn!(credit = %c.salt, %address, sender = %c.sender, "credits-settlement: a credit this node signed for one wallet was used by another; debiting the signer's wallet");
        }
        let (kind, credits) = if settled == 0 {
            ("purchase", UsdCents::new(usd_cents).credits_ceil())
        } else {
            match remainder_credits(&c.value_wei, oracle_rate.as_deref(), oracle_decimals) {
                Some(credits) => ("remainder", credits),
                None => {
                    tracing::error!(credit = %c.salt, value = %c.value_wei, "credits-settlement: a remainder was spent on a credit without an oracle rate; nothing to price it with");
                    ("remainder", 0)
                }
            }
        };
        let amount = credits.to_string();
        let key = if kind == "purchase" {
            format!("credit-used:{}", c.salt)
        } else {
            format!("credit-used:{}:{}:{}", c.salt, c.block_number, c.log_index)
        };
        let spent = match self
            .settle_in_tx(&mut tx, &address, &amount, &key, Some(&key))
            .await
        {
            Ok(spent) => spent,
            Err(ApiError::Database(e)) => return Err(ApiError::Database(e)),
            Err(e) => {
                tx.rollback().await?;
                tracing::error!(credit = %c.salt, %address, kind, error = %e, "credit-used debit FAILED: the credit was used on chain but the wallet cannot cover it; retried by the next pass and the reconciler");
                return Ok(CreditUsageDiff {
                    credit_id: c.salt.clone(),
                    address,
                    debited: amount,
                    consumed: false,
                    replayed: false,
                    new_balance: String::new(),
                    kind: kind.to_string(),
                });
            }
        };
        sqlx::query(
            "INSERT INTO credits.usage_settlements (chain_id, block_number, log_index, authorization_id, address, kind, value_wei, debited_credits, tx_ref) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::numeric, $8::numeric, $9)",
        )
        .bind(chain_id)
        .bind(c.block_number)
        .bind(c.log_index)
        .bind(&c.salt)
        .bind(&address)
        .bind(kind)
        .bind(&c.value_wei)
        .bind(&amount)
        .bind(&key)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE credits.authorizations SET status = 'consumed', consumed_at = COALESCE(consumed_at, $2) WHERE id = $1")
            .bind(&c.salt)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        if let Some(hash) = credit_hash {
            enqueue_revocation(&mut tx, &hash, &c.salt, chain_id, contract, "consumed").await?;
        }
        tx.commit().await?;
        Ok(CreditUsageDiff {
            credit_id: c.salt.clone(),
            address,
            debited: amount,
            consumed: true,
            replayed: spent.replayed,
            new_balance: spent.available,
            kind: kind.to_string(),
        })
    }

    /// The lines of a group in request order (the batch echo, the history).
    pub async fn authorization_lines(
        &self,
        id: &str,
    ) -> Result<Vec<super::authorize::GroupLine>, ApiError> {
        let rows = sqlx::query(
            "SELECT quoted_cents, charged_cents, sale_kind, asset_kind, asset_chain_id, asset_contract, asset_item_id::text AS asset_item_id, \
                    asset_token_id::text AS asset_token_id, trade_id, contract_address, item_id, registered_name \
             FROM credits.authorization_lines WHERE authorization_id = $1 ORDER BY position",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| super::authorize::GroupLine {
                quoted_cents: r.get("quoted_cents"),
                charged_cents: r.get("charged_cents"),
                sale_kind: leak(r.get::<String, _>("sale_kind")),
                asset_kind: leak(r.get::<String, _>("asset_kind")),
                asset_chain_id: r.get("asset_chain_id"),
                asset_contract: r.get("asset_contract"),
                asset_item_id: r.get("asset_item_id"),
                asset_token_id: r.get("asset_token_id"),
                trade_id: r.get("trade_id"),
                contract_address: r.get("contract_address"),
                item_id: r.get("item_id"),
                registered_name: r.get("registered_name"),
            })
            .collect())
    }
}

/// The store's small closed vocabularies back to their static names.
fn leak(s: String) -> &'static str {
    match s.as_str() {
        "primary_mint" => "primary_mint",
        "primary_name_registration" => "primary_name_registration",
        "secondary" => "secondary",
        "wearable" => "wearable",
        "emote" => "emote",
        "name" => "name",
        _ => "other",
    }
}

/// Queues `credit_hash` for `revokeCredits`; a hash already queued keeps its row.
pub async fn enqueue_revocation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    credit_hash: &str,
    authorization_id: &str,
    chain_id: i64,
    contract: &str,
    reason: &str,
) -> Result<bool, ApiError> {
    let res = sqlx::query(
        "INSERT INTO credits.revocations (credit_hash, authorization_id, chain_id, contract, reason) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (credit_hash) DO NOTHING",
    )
    .bind(credit_hash)
    .bind(authorization_id)
    .bind(chain_id)
    .bind(contract)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// What one revocation pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RevocationOutcome {
    pub sent: usize,
    pub confirmed: usize,
    pub failed: usize,
    pub errors: usize,
}

/// The `revokeCredits` sender: the revoker key over the credits chain.
pub struct Revoker {
    pub credits: Credits,
    pub signer: Arc<DirectSigner>,
    pub chain_id: i64,
}

impl Revoker {
    /// `CREDITS_REVOKER_KEY_FILE` over the usage index's endpoint; the error
    /// names the variable, never the key.
    pub fn new(
        credits: Credits,
        chain: Arc<ChainComponent>,
        key: &str,
        chain_id: i64,
    ) -> Result<Self, String> {
        let signer = DirectSigner::new(
            chain,
            key,
            chain_id as u64,
            REVOCATION_GAS_CAP,
            Duration::from_secs(1),
            Duration::from_secs(60),
        )
        .map_err(|_| "invalid CREDITS_REVOKER_KEY: not a 32-byte secp256k1 key".to_string())?;
        Ok(Self {
            credits,
            signer: Arc::new(signer),
            chain_id,
        })
    }

    /// One pass: claim due rows one at a time (`FOR UPDATE SKIP LOCKED`),
    /// broadcast a `pending` one, poll a `sent` one; every chain call runs
    /// with no transaction open.
    pub async fn run_pass(&self, ctx: Option<&JobContext>) -> Result<RevocationOutcome, ApiError> {
        let mut outcome = RevocationOutcome::default();
        let rows = sqlx::query(
            "SELECT credit_hash, contract, status, tx_hash, attempts FROM credits.revocations \
             WHERE status IN ('pending', 'sent') AND chain_id = $1 AND attempts < $2 ORDER BY created_at LIMIT $3",
        )
        .bind(self.chain_id)
        .bind(REVOCATION_MAX_ATTEMPTS)
        .bind(REVOCATIONS_PER_PASS)
        .fetch_all(&self.credits.pool)
        .await?;
        for r in rows {
            if ctx.is_some_and(|c| c.is_cancelled()) {
                break;
            }
            let credit_hash: String = r.get("credit_hash");
            let contract: String = r.get("contract");
            let status: String = r.get("status");
            let tx_hash: Option<String> = r.get("tx_hash");
            match status.as_str() {
                "pending" => match self.broadcast(&credit_hash, &contract).await {
                    Ok(hash) => {
                        self.mark(&credit_hash, "sent", Some(&hash), None).await?;
                        outcome.sent += 1;
                    }
                    Err(e) => {
                        self.mark(&credit_hash, "pending", None, Some(&e)).await?;
                        outcome.errors += 1;
                    }
                },
                _ => {
                    let Some(hash) = tx_hash else { continue };
                    match self.signer.fetch_receipt_once(&hash).await {
                        Ok((ReceiptOutcome::Confirmed, _)) => {
                            self.mark(&credit_hash, "confirmed", Some(&hash), None)
                                .await?;
                            outcome.confirmed += 1;
                        }
                        Ok((ReceiptOutcome::Reverted, _)) => {
                            self.mark(
                                &credit_hash,
                                "failed",
                                Some(&hash),
                                Some("revokeCredits reverted"),
                            )
                            .await?;
                            outcome.failed += 1;
                        }
                        Ok((ReceiptOutcome::Pending, _)) => {}
                        Err(e) => {
                            self.mark(&credit_hash, "sent", Some(&hash), Some(&e.to_string()))
                                .await?;
                            outcome.errors += 1;
                        }
                    }
                }
            }
        }
        if let Some(ctx) = ctx {
            ctx.progress(format!(
                "revocations: {} sent, {} confirmed, {} failed, {} errors",
                outcome.sent, outcome.confirmed, outcome.failed, outcome.errors
            ));
        }
        Ok(outcome)
    }

    async fn broadcast(&self, credit_hash: &str, contract: &str) -> Result<String, String> {
        let hash =
            B256::from_str(credit_hash).map_err(|_| "credit hash is not bytes32".to_string())?;
        let to =
            Address::from_str(contract).map_err(|_| "manager is not an address".to_string())?;
        self.signer
            .send_direct_call(to, revoke_calldata(hash))
            .await
            .map_err(|e| e.to_string())
    }

    async fn mark(
        &self,
        credit_hash: &str,
        status: &str,
        tx_hash: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), ApiError> {
        sqlx::query(
            "UPDATE credits.revocations SET status = $2, tx_hash = COALESCE($3, tx_hash), last_error = $4, \
                 attempts = attempts + CASE WHEN $4 IS NULL THEN 0 ELSE 1 END, updated_at = timezone('UTC', now()) \
             WHERE credit_hash = $1",
        )
        .bind(credit_hash)
        .bind(status)
        .bind(tx_hash)
        .bind(error)
        .execute(&self.credits.pool)
        .await?;
        Ok(())
    }
}

/// How often the settlement job reports the processing orders past
/// [`super::orders::REPORT_PROCESSING_AFTER`]: daily, not on every pass.
pub(super) const REPORT_STALE_PROCESSING_EVERY: Duration = Duration::from_secs(24 * 3600);

/// The settlement job's report of processing orders that outlived their
/// payment's delay: an error log, at most once per `every`, so the order
/// nothing else will move reaches a person without a line every pass. The
/// limit is held in memory, per process: a restart, or another replica
/// taking over the job's lease, reports again at its first pass.
#[derive(Default)]
pub(super) struct StaleProcessingReport {
    last: std::sync::Mutex<Option<std::time::Instant>>,
}

impl StaleProcessingReport {
    /// What it counted when the report was due and the count succeeded;
    /// `None` when it was not due yet, or the count failed (retried next pass).
    pub(super) async fn run(
        &self,
        credits: &Credits,
        every: Duration,
    ) -> Option<super::orders::StaleProcessing> {
        let due = {
            let last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            last.is_none_or(|at| at.elapsed() >= every)
        };
        if !due {
            return None;
        }
        let stale = match credits
            .stale_processing_orders(super::orders::REPORT_PROCESSING_AFTER)
            .await
        {
            Ok(stale) => stale,
            Err(e) => {
                tracing::warn!(error = %e, "could not count the pack orders still processing");
                return None;
            }
        };
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
        if stale.count > 0 {
            tracing::error!(
                count = stale.count,
                oldest_order = stale.oldest_order.as_deref().unwrap_or(""),
                "pack orders still processing long after their session completed: their payment's outcome event was likely lost; check each payment at Stripe"
            );
        }
        Some(stale)
    }
}

/// The `credits-settlement` job: every `interval`, the pack-order abandon
/// sweep and, with a revoker, the revocation pass; both write local
/// authority, so neither runs while `OPS_WRITES_ENABLED` is off, and a pass
/// is skipped while the restore stamps hold the operator plane. Each pass
/// also reports, daily, the processing orders nothing moved
/// ([`settlement_job_pass`]).
pub fn spawn_settlement_job(
    jobs: &Jobs,
    credits: Credits,
    revoker: Option<Arc<Revoker>>,
    writes_enabled: bool,
    interval: Duration,
) {
    if !writes_enabled {
        tracing::info!(
            "OPS_WRITES_ENABLED=false: the credits settlement job (pack-order sweep, revocations) is not running"
        );
        return;
    }
    if revoker.is_none() {
        tracing::info!(
            "CREDITS_REVOKER_KEY_FILE unset: credit revocations queue in credits.revocations and are not sent"
        );
    }
    let gate = Arc::new(PassGate::new(JOB_NAME, ops_refusal));
    let report = Arc::new(StaleProcessingReport::default());
    jobs.spawn_periodic_leased(
        JOB_NAME,
        PeriodicCfg::every(interval)
            .with_jitter(Duration::from_secs(5))
            .with_error_backoff(Duration::from_secs(300)),
        move |ctx| {
            let credits = credits.clone();
            let revoker = revoker.clone();
            let gate = gate.clone();
            let report = report.clone();
            async move {
                settlement_job_pass(
                    &credits,
                    revoker.as_deref(),
                    &gate,
                    writes_enabled,
                    Some(&ctx),
                    &report,
                )
                .await
                .map(|_| ())
            }
        },
    );
}

/// One pass of the settlement job: [`settlement_pass`], then the daily
/// report. The report only reads, so a pass that failed (its sweep, or its
/// revocations) still runs it: an order stuck processing is not silenced by
/// another failure. A pass the restore stamps held skips it, as it skips
/// every write: this node does not speak for the ledger yet.
pub(super) async fn settlement_job_pass(
    credits: &Credits,
    revoker: Option<&Revoker>,
    gate: &PassGate,
    writes_enabled: bool,
    ctx: Option<&JobContext>,
    report: &StaleProcessingReport,
) -> anyhow::Result<bool> {
    let ran = settlement_pass(credits, revoker, gate, writes_enabled, ctx).await;
    if !matches!(ran, Ok(false)) {
        report.run(credits, REPORT_STALE_PROCESSING_EVERY).await;
    }
    ran
}

/// One settlement pass; `false` when `gate` held it and nothing was written.
pub(super) async fn settlement_pass(
    credits: &Credits,
    revoker: Option<&Revoker>,
    gate: &PassGate,
    writes_enabled: bool,
    ctx: Option<&JobContext>,
) -> anyhow::Result<bool> {
    if let Some(refusal) = gate.check(writes_enabled) {
        if let Some(ctx) = ctx {
            ctx.progress(refusal.message("operator-plane", WRITES_DISABLED_MESSAGE));
        }
        return Ok(false);
    }
    let abandoned = credits
        .abandon_stale_pack_orders(super::orders::ABANDON_AFTER)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if abandoned > 0 {
        tracing::info!(
            abandoned,
            "retired pack orders nobody paid within a session's lifetime"
        );
    }
    if let Some(revoker) = revoker {
        revoker
            .run_pass(ctx)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_role_and_calldata_match_the_contract() {
        assert_eq!(credits_signer_role(), keccak256(b"CREDITS_SIGNER_ROLE"));
        let has_role = ICreditsManager::hasRoleCall {
            role: credits_signer_role(),
            account: Address::ZERO,
        }
        .abi_encode();
        assert_eq!(&has_role[..4], &keccak256(b"hasRole(bytes32,address)")[..4]);
        let data = revoke_calldata(B256::repeat_byte(0x11));
        assert_eq!(&data[..4], &keccak256(b"revokeCredits(bytes32[])")[..4]);
        assert_eq!(data.len(), 4 + 32 * 3, "offset, length, one hash");
        assert_eq!(
            EXECUTOR_EXECUTE_SELECTOR,
            keccak256(b"execute((address,bytes,bytes))")[..4],
            "the executors' execute selector (docs/shop/CONTRACTS.md)"
        );
    }

    #[test]
    fn a_remainder_is_priced_at_the_credits_rate_and_rounds_up_to_whole_credits() {
        assert_eq!(
            remainder_credits("3000000000000000000", Some("300000000000000000"), Some(18)),
            Some(9),
            "3 MANA at 0.30 USD is 90 cents: 9 credits"
        );
        assert_eq!(
            remainder_credits("3000000000000000010", Some("300000000000000000"), Some(18)),
            Some(10),
            "a USD-wei over 90 cents rounds up to 91, and the cent to the next credit"
        );
        assert_eq!(
            remainder_credits("1", Some("300000000000000000"), Some(18)),
            Some(1),
            "any positive remainder costs a credit"
        );
        assert_eq!(remainder_credits("3000000000000000000", None, None), None);
        assert_eq!(remainder_credits("x", Some("1"), Some(18)), None);
    }

    #[test]
    fn redaction_hides_urls() {
        assert_eq!(
            redact("post https://rpc.example/v1/SECRET failed"),
            "post <url> failed"
        );
    }
}
