//! The meta-transaction relay policy, unchanged from the transactions-server
//! port: the request schema, the `executeMetaTransaction` selector allowlist,
//! the `from`/`userAddress` binding, the EIP-712 signature check against the
//! target's nonce and domain, the contract allowlist, the minimum sale
//! price, the gas-price ceiling, the caller's simulation, and the per-day
//! quota reserved in `economy.transactions` under a per-user advisory lock
//! (the count bound to the request clock) before any broadcast.

use std::sync::Arc;

use alloy_primitives::{Address, U256};
use chrono::NaiveDateTime;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use super::abi;
use super::admin::{RuntimeConfig, SignerPreference};
use super::chain::{ChainComponent, ChainError};
use super::contracts::ContractsComponent;
use super::errors::ApiError;
use super::meta_tx::{
    MetaTxStruct, audited_domain_for, build_domain_separator, meta_tx_digest,
    recover_meta_tx_signer,
};
use super::relayer::Relayer;
use super::signer::DirectSigner;
use super::transfer_shortfall;
use super::upstream::UpstreamForwarder;
use super::{QUOTA_WINDOW_SECS, Settings};
use crate::types::is_hex_address;

#[derive(Debug, Clone)]
pub struct TransactionData {
    pub from: String,
    pub params: Vec<String>,
}

pub fn parse_send_transaction_request(body: &[u8]) -> Result<TransactionData, ApiError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| ApiError::MalformedBody(e.to_string()))?;
    let transaction_data = match value.get("transactionData") {
        Some(v) if !v.is_null() => v,
        _ => {
            return Err(ApiError::MissingTransactionData(
                "Missing transaction data. Please add it to the body of the request as `transactionData`".into(),
            ));
        }
    };
    validate_transaction_data(transaction_data)
}

const ADDRESS_PATTERN: &str = "^0x[a-fA-F0-9]{40}$";
const CALLDATA_PATTERN: &str = "^0x[a-fA-F0-9]+$";
const CALLDATA_MAX_LENGTH: usize = 200_002;

fn schema_error(
    instance_path: &str,
    schema_path: &str,
    keyword: &str,
    params: Value,
    message: &str,
) -> ApiError {
    let s = |v: &str| serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_string());
    let error_object = format!(
        "{{\"instancePath\":{},\"schemaPath\":{},\"keyword\":{},\"params\":{},\"message\":{}}}",
        s(instance_path),
        s(schema_path),
        s(keyword),
        serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string()),
        s(message)
    );
    ApiError::InvalidSchema(format!(
        "Invalid transaction data. Errors: [{error_object}]"
    ))
}

fn validate_transaction_data(data: &Value) -> Result<TransactionData, ApiError> {
    let obj = match data {
        Value::Object(map) => map,
        _ => {
            return Err(schema_error(
                "",
                "#/type",
                "type",
                json!({ "type": "object" }),
                "must be object",
            ));
        }
    };
    for required in ["from", "params"] {
        if !obj.contains_key(required) {
            return Err(schema_error(
                "",
                "#/required",
                "required",
                json!({ "missingProperty": required }),
                &format!("must have required property '{required}'"),
            ));
        }
    }
    if let Some(extra) = obj
        .keys()
        .find(|k| k.as_str() != "from" && k.as_str() != "params")
    {
        return Err(schema_error(
            "",
            "#/additionalProperties",
            "additionalProperties",
            json!({ "additionalProperty": extra }),
            "must NOT have additional properties",
        ));
    }
    let from = match obj.get("from") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(schema_error(
                "/from",
                "#/properties/from/type",
                "type",
                json!({ "type": "string" }),
                "must be string",
            ));
        }
    };
    if !is_hex_address(&from) {
        return Err(schema_error(
            "/from",
            "#/properties/from/pattern",
            "pattern",
            json!({ "pattern": ADDRESS_PATTERN }),
            &format!("must match pattern \"{ADDRESS_PATTERN}\""),
        ));
    }
    let arr = match obj.get("params") {
        Some(Value::Array(a)) => a,
        _ => {
            return Err(schema_error(
                "/params",
                "#/properties/params/type",
                "type",
                json!({ "type": "array" }),
                "must be array",
            ));
        }
    };
    if arr.len() < 2 {
        return Err(schema_error(
            "/params",
            "#/properties/params/minItems",
            "minItems",
            json!({ "limit": 2 }),
            "must NOT have fewer than 2 items",
        ));
    }
    if arr.len() > 2 {
        return Err(schema_error(
            "/params",
            "#/properties/params/maxItems",
            "maxItems",
            json!({ "limit": 2 }),
            "must NOT have more than 2 items",
        ));
    }
    let mut params = Vec::with_capacity(2);
    for (i, item) in arr.iter().enumerate() {
        let s = match item {
            Value::String(s) => s,
            _ => {
                return Err(schema_error(
                    &format!("/params/{i}"),
                    &format!("#/properties/params/items/{i}/type"),
                    "type",
                    json!({ "type": "string" }),
                    "must be string",
                ));
            }
        };
        if i == 1 && s.chars().count() > CALLDATA_MAX_LENGTH {
            return Err(schema_error(
                "/params/1",
                "#/properties/params/items/1/maxLength",
                "maxLength",
                json!({ "limit": CALLDATA_MAX_LENGTH }),
                &format!("must NOT have more than {CALLDATA_MAX_LENGTH} characters"),
            ));
        }
        let (matches, pattern) = if i == 0 {
            (is_hex_address(s), ADDRESS_PATTERN)
        } else {
            (is_hex_data(s), CALLDATA_PATTERN)
        };
        if !matches {
            return Err(schema_error(
                &format!("/params/{i}"),
                &format!("#/properties/params/items/{i}/pattern"),
                "pattern",
                json!({ "pattern": pattern }),
                &format!("must match pattern \"{pattern}\""),
            ));
        }
        params.push(s.clone());
    }
    Ok(TransactionData { from, params })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaTxSender(String);

impl MetaTxSender {
    pub fn from_meta_tx_calldata(from: &str, calldata: &str) -> Result<Self, ApiError> {
        let data = hex_to_bytes(calldata).ok_or_else(|| {
            ApiError::InvalidTransaction(
                "Invalid transaction data. The calldata is not a hex byte string.".into(),
            )
        })?;
        let signed_user = abi::decode_meta_tx_user_address(&data).ok_or_else(|| {
            ApiError::InvalidTransaction(
                "Invalid transaction data. The executeMetaTransaction calldata could not be decoded, so the userAddress it \
                 was signed for cannot be established."
                    .into(),
            )
        })?;
        let claimed = from.trim().parse::<Address>().map_err(|_| {
            ApiError::InvalidTransaction(format!("Invalid from address. Received: {from}"))
        })?;
        if signed_user != claimed {
            return Err(ApiError::InvalidTransaction(format!(
                "The from address does not match the userAddress signed into the executeMetaTransaction calldata. from: \
                 {claimed} - userAddress: {signed_user}"
            )));
        }
        Ok(Self(format!("{signed_user:#x}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationDisposition {
    Release,
    Keep,
}

pub fn reservation_disposition(err: &ApiError) -> ReservationDisposition {
    if is_post_broadcast(err) {
        return ReservationDisposition::Keep;
    }
    if is_pre_broadcast(err) {
        ReservationDisposition::Release
    } else {
        ReservationDisposition::Keep
    }
}

fn is_post_broadcast(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::RelayReverted(_) | ApiError::RelayerTimeout(_)
    )
}

fn is_pre_broadcast(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::InvalidSchema(_)
            | ApiError::InvalidSalePrice(_)
            | ApiError::InvalidContractAddress(_)
            | ApiError::InvalidTransaction(_)
            | ApiError::HighCongestion(_)
            | ApiError::RelayerFailed(_)
            | ApiError::RelayerUnavailable(_)
    )
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TransactionRow {
    pub id: i64,
    pub tx_hash: String,
    pub user_address: String,
    #[serde(serialize_with = "serialize_created_at")]
    pub created_at: NaiveDateTime,
}

fn serialize_created_at<S>(dt: &NaiveDateTime, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_str(&dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// Under the Auto preference, locally-provisioned signers win over the
/// upstream forward, which is the fallback for nodes without real relayer
/// creds. An explicit Oz/Direct preference is local-or-fail: it never rides
/// the upstream, so the admin can force local broadcasting while
/// TRANSACTIONS_UPSTREAM_URL is set. The operator toggle beats everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastRoute {
    Disabled,
    Oz,
    Direct,
    Upstream,
    None,
}

pub fn select_broadcast_route(
    enabled: bool,
    pref: SignerPreference,
    has_oz: bool,
    has_direct: bool,
    has_upstream: bool,
) -> BroadcastRoute {
    if !enabled {
        return BroadcastRoute::Disabled;
    }
    match pref {
        SignerPreference::Oz if has_oz => BroadcastRoute::Oz,
        SignerPreference::Direct if has_direct => BroadcastRoute::Direct,
        SignerPreference::Oz | SignerPreference::Direct => BroadcastRoute::None,
        SignerPreference::Auto if has_oz => BroadcastRoute::Oz,
        SignerPreference::Auto if has_direct => BroadcastRoute::Direct,
        SignerPreference::Auto if has_upstream => BroadcastRoute::Upstream,
        SignerPreference::Auto => BroadcastRoute::None,
    }
}

pub struct TransactionComponent {
    pool: PgPool,
    relayer: Option<Relayer>,
    signer: Option<Arc<DirectSigner>>,
    upstream: Option<UpstreamForwarder>,
    chain: Option<Arc<ChainComponent>>,
    runtime: Arc<RuntimeConfig>,
}

impl TransactionComponent {
    pub fn new(
        pool: PgPool,
        relayer: Option<Relayer>,
        signer: Option<Arc<DirectSigner>>,
        upstream: Option<UpstreamForwarder>,
        chain: Option<Arc<ChainComponent>>,
        runtime: Arc<RuntimeConfig>,
    ) -> Self {
        Self {
            pool,
            relayer,
            signer,
            upstream,
            chain,
            runtime,
        }
    }

    pub fn has_oz_relayer(&self) -> bool {
        self.relayer.is_some()
    }

    pub fn has_direct_signer(&self) -> bool {
        self.signer.is_some()
    }

    pub fn has_upstream_forwarder(&self) -> bool {
        self.upstream.is_some()
    }

    pub fn has_chain(&self) -> bool {
        self.chain.is_some()
    }

    pub fn broadcast_route(&self) -> BroadcastRoute {
        select_broadcast_route(
            self.runtime.relayer_enabled(),
            self.runtime.signer_preference(),
            self.relayer.is_some(),
            self.signer.is_some(),
            self.upstream.is_some(),
        )
    }

    /// Some only when the current selection resolves to the upstream forward;
    /// the handler branches on this before calling `send_meta_transaction`.
    pub fn upstream_route(&self) -> Option<&UpstreamForwarder> {
        match self.broadcast_route() {
            BroadcastRoute::Upstream => self.upstream.as_ref(),
            _ => None,
        }
    }

    pub fn direct_signer(&self) -> Option<&Arc<DirectSigner>> {
        self.signer.as_ref()
    }

    /// One transaction: the per-user advisory lock, then the count of the
    /// user's rows inside the day before `now` (the request clock, never
    /// `now()`) and the reservation insert when under the cap; the lock
    /// stays its own statement so the count runs on a snapshot taken after
    /// it is held.
    pub async fn reserve_quota(
        &self,
        max_transactions_per_day: i64,
        sender: &MetaTxSender,
        session_id: &str,
        chain_id: u64,
        contract_address: &str,
        now: NaiveDateTime,
    ) -> Result<(), ApiError> {
        self.reserve(
            max_transactions_per_day,
            None,
            sender,
            session_id,
            chain_id,
            contract_address,
            now,
        )
        .await
    }

    /// [`Self::reserve_quota`] for a call that is no sale or credits spend: it
    /// also counts toward the relayer's daily budget across every address
    /// (`free_budget`, 0 is no limit), checked and taken under one global lock.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserve_free_call(
        &self,
        max_transactions_per_day: i64,
        free_budget: i64,
        sender: &MetaTxSender,
        session_id: &str,
        chain_id: u64,
        contract_address: &str,
        now: NaiveDateTime,
    ) -> Result<(), ApiError> {
        self.reserve(
            max_transactions_per_day,
            Some(free_budget),
            sender,
            session_id,
            chain_id,
            contract_address,
            now,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn reserve(
        &self,
        max_transactions_per_day: i64,
        free_budget: Option<i64>,
        sender: &MetaTxSender,
        session_id: &str,
        chain_id: u64,
        contract_address: &str,
        now: NaiveDateTime,
    ) -> Result<(), ApiError> {
        let user_address = sender.as_str();
        if !is_hex_address(user_address) || !is_session_id(session_id) {
            return Err(ApiError::Internal(
                "reserve_quota: reservation keys are not in their canonical form".into(),
            ));
        }
        let contract = contract_address.to_lowercase();
        let window_start = now - chrono::Duration::seconds(QUOTA_WINDOW_SECS);
        let mut tx = self.pool.begin().await?;
        if let Some(budget) = free_budget.filter(|b| *b > 0) {
            // Global lock first, then the user's: one order, no deadlock.
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext('relay-free-budget'))")
                .execute(&mut *tx)
                .await?;
            let used: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM (SELECT 1 FROM economy.transactions \
                 WHERE free_call AND created_at >= $1 LIMIT $2) used",
            )
            .bind(window_start)
            .bind(budget)
            .fetch_one(&mut *tx)
            .await?;
            if used >= budget {
                tracing::warn!(budget, "relay budget for free calls is spent for the day");
                return Err(ApiError::RelayBudgetSpent(
                    "The relayer's daily budget for non-sale transactions is spent; try again later"
                        .into(),
                ));
            }
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(user_address)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(
            "WITH used AS (\
               SELECT COUNT(*) AS n FROM economy.transactions \
               WHERE user_address = $1 AND created_at >= $5\
             ), ins AS (\
               INSERT INTO economy.transactions (user_address, session_id, chain_id, contract_address, created_at, free_call) \
               SELECT $1, $2, $3, $4, $6, $8 FROM used \
               WHERE used.n < $7 \
               RETURNING id\
             ) \
             SELECT used.n AS n, EXISTS (SELECT 1 FROM ins) AS reserved FROM used",
        )
        .bind(user_address)
        .bind(session_id)
        .bind(chain_id as i32)
        .bind(&contract)
        .bind(window_start)
        .bind(now)
        .bind(max_transactions_per_day)
        .bind(free_budget.is_some())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        let count: i64 = row.try_get("n")?;
        let reserved: bool = row.try_get("reserved")?;
        if !reserved {
            return Err(ApiError::QuotaReached(format!(
                "Max amount of transactions reached for address. Quota: {count}"
            )));
        }
        Ok(())
    }

    pub async fn confirm_reservation(
        &self,
        session_id: &str,
        tx_hash: &str,
    ) -> Result<(), ApiError> {
        sqlx::query(
            "UPDATE economy.transactions SET tx_hash = $1, session_id = NULL WHERE session_id = $2",
        )
        .bind(tx_hash)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn release_reservation(&self, session_id: &str) -> Result<(), ApiError> {
        sqlx::query("DELETE FROM economy.transactions WHERE session_id = $1")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_by_user_address(
        &self,
        user_address: &str,
    ) -> Result<Vec<TransactionRow>, ApiError> {
        let rows = sqlx::query_as::<_, TransactionRow>(
            "SELECT id, tx_hash, user_address, created_at FROM economy.transactions \
             WHERE user_address = $1 AND tx_hash IS NOT NULL ORDER BY id",
        )
        .bind(user_address.to_lowercase())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn check_data(
        &self,
        settings: &Settings,
        contracts: &ContractsComponent,
        tx: &TransactionData,
    ) -> Result<MetaTxSender, ApiError> {
        self.check_function_selector(tx)?;
        check_nested_meta_tx(tx)?;
        let sender = MetaTxSender::from_meta_tx_calldata(&tx.from, &tx.params[1])?;
        // A trial's relayer pays gas for the trial's buyers only, judged on the userAddress the
        // meta-transaction is signed for (its signature is checked below): any other wallet is
        // refused before the daily quota or a broadcast.
        if let Some(buyers) = &settings.trial_buyers
            && !buyers.admits(sender.as_str())
        {
            return Err(ApiError::Forbidden(
                "this relay serves only the test's buyers (CREDITS_TRIAL_MODE)".into(),
            ));
        }
        self.check_self_relay(&sender)?;
        self.check_contract_address(contracts, tx).await?;
        check_sale_price(settings, tx)?;
        if self.chain.is_some() {
            tokio::try_join!(
                self.check_meta_tx_signature(tx),
                self.check_gas_price(settings),
                self.check_transaction(tx)
            )?;
        } else {
            self.check_meta_tx_signature(tx).await?;
        }
        Ok(sender)
    }

    /// Refuses a meta-transaction whose `userAddress` is one of this node's
    /// own relayer EOAs: relaying it would let a caller spend a signature
    /// this service holds the key for. Only the direct signer's address is
    /// known locally (the OZ relayer's EOAs live behind its API).
    fn check_self_relay(&self, sender: &MetaTxSender) -> Result<(), ApiError> {
        if let Some(signer) = self.signer.as_ref() {
            let relayer = format!("{:#x}", signer.relayer_address());
            if sender.as_str().eq_ignore_ascii_case(&relayer) {
                return Err(ApiError::InvalidTransaction(
                    "Invalid transaction data.".into(),
                ));
            }
        }
        Ok(())
    }

    /// Recovers the EIP-712 signature carried in the executeMetaTransaction
    /// calldata and refuses the request unless it was produced by the
    /// `userAddress` the payload acts for. This is the authentication step,
    /// so it precedes quota: nothing is written for an address that did not
    /// sign. Without a chain a route that makes this node the relayer
    /// (Oz/Direct) fails closed; the upstream forward defers to the
    /// upstream, which verifies before it broadcasts.
    async fn check_meta_tx_signature(&self, tx: &TransactionData) -> Result<(), ApiError> {
        let Some(chain) = self.chain.as_ref() else {
            return match self.broadcast_route() {
                BroadcastRoute::Oz | BroadcastRoute::Direct => Err(ApiError::RelayerUnavailable(
                    "Meta-transaction signatures cannot be verified without an RPC endpoint. Set RPC_ENDPOINT_POLYGON so \
                     the relayer can rebuild the EIP-712 digest before broadcasting."
                        .into(),
                )),
                _ => Ok(()),
            };
        };
        let contract = tx.params[0].trim().parse::<Address>().map_err(|_| {
            ApiError::InvalidContractAddress(format!(
                "Invalid contract address. Contract address: {}",
                tx.params[0]
            ))
        })?;
        let data = hex_to_bytes(&tx.params[1]).ok_or_else(|| {
            ApiError::InvalidTransaction(
                "Invalid transaction data. The calldata is not a hex byte string.".into(),
            )
        })?;
        let decoded = abi::decode_meta_tx_full(&data).ok_or_else(|| {
            ApiError::InvalidTransaction("Invalid transaction data. The executeMetaTransaction calldata could not be decoded.".into())
        })?;

        let (nonce, reported_separator) = tokio::try_join!(
            async {
                chain
                    .get_meta_transaction_nonce(contract, decoded.user_address)
                    .await
                    .map_err(chain_unavailable)
            },
            async {
                chain
                    .get_domain_separator(contract)
                    .await
                    .map_err(chain_unavailable)
            },
        )?;
        let nonce = nonce.ok_or_else(|| {
            ApiError::InvalidContractAddress(format!(
                "The target does not implement meta-transactions. Contract address: {contract:#x}"
            ))
        })?;
        let (domain_separator, struct_kind) = match reported_separator {
            Some(reported) => (reported, MetaTxStruct::FunctionSignature),
            None => {
                let chain_id = chain.get_chain_id().await.map_err(chain_unavailable)?;
                let audited = audited_domain_for(chain_id, contract).ok_or_else(|| {
                    ApiError::InvalidContractAddress(format!(
                        "The target reports no EIP-712 domain separator and none is recorded for it, so the signature \
                         cannot be verified. Contract address: {contract:#x}"
                    ))
                })?;
                (
                    build_domain_separator(audited.name, audited.version, contract, chain_id),
                    audited.struct_kind,
                )
            }
        };
        let digest = meta_tx_digest(
            domain_separator,
            nonce,
            decoded.user_address,
            &decoded.function_signature,
            struct_kind,
        );
        let recovered = recover_meta_tx_signer(digest, &decoded.signature).map_err(|_| {
            ApiError::InvalidTransaction(
                "The meta-transaction signature was not produced by the userAddress it carries."
                    .into(),
            )
        })?;
        if recovered != decoded.user_address {
            return Err(ApiError::InvalidTransaction(format!(
                "The meta-transaction signature was not produced by the userAddress it carries, checking it against that \
                 account's nonce {nonce} on the target."
            )));
        }
        Ok(())
    }

    fn check_function_selector(&self, tx: &TransactionData) -> Result<(), ApiError> {
        let data = hex_to_bytes(&tx.params[1]).unwrap_or_default();
        if !abi::is_execute_meta_tx(&data) {
            let sel: String = tx.params[1]
                .chars()
                .take(10)
                .collect::<String>()
                .to_lowercase();
            return Err(ApiError::InvalidTransaction(format!(
                "Invalid function selector. Only executeMetaTransaction (0x0c53c51c or 0xd8ed1acc) is allowed. Received: {sel}"
            )));
        }
        Ok(())
    }

    async fn check_contract_address(
        &self,
        contracts: &ContractsComponent,
        tx: &TransactionData,
    ) -> Result<(), ApiError> {
        let contract_address = &tx.params[0];
        if !contracts.is_valid_address(contract_address).await? {
            return Err(ApiError::InvalidContractAddress(format!(
                "Invalid contract address. Contract address: {contract_address}"
            )));
        }
        Ok(())
    }

    async fn check_transaction(&self, tx: &TransactionData) -> Result<(), ApiError> {
        let chain = self.chain.as_ref().expect("chain gated");
        let from = tx.from.trim().parse::<Address>().ok();
        let to = tx.params[0].trim().parse::<Address>().map_err(|_| {
            ApiError::InvalidContractAddress(format!(
                "Invalid contract address. Contract address: {}",
                tx.params[0]
            ))
        })?;
        let data = hex_to_bytes(&tx.params[1]).unwrap_or_default();
        match chain.estimate_gas(from, to, &data).await {
            Ok(_) => Ok(()),
            Err(ChainError::Revert(message)) => {
                let detail = self
                    .transfer_shortfall(chain, to, &data)
                    .await
                    .unwrap_or(message);
                Err(ApiError::InvalidTransaction(format!(
                    "Error simulating transaction: {detail}"
                )))
            }
            Err(e) => Err(ApiError::InvalidTransaction(format!(
                "Error simulating transaction: {e}"
            ))),
        }
    }

    async fn transfer_shortfall(
        &self,
        chain: &ChainComponent,
        to: Address,
        data: &[u8],
    ) -> Option<String> {
        let intent = transfer_shortfall::transfer_intent(data)?;
        let calldata = hex_to_bytes(&transfer_shortfall::balance_of_calldata(intent.user))?;
        let result = chain.eth_call(to, &calldata).await.ok()?;
        let balance = transfer_shortfall::decode_balance(&format!("0x{}", hex::encode(result)))?;
        transfer_shortfall::shortfall_message(&intent, balance)
    }

    async fn check_gas_price(&self, settings: &Settings) -> Result<(), ApiError> {
        let Some(max_allowed) = settings.max_gas_price_allowed_in_wei else {
            return Ok(());
        };
        let chain = self.chain.as_ref().expect("chain gated");
        let current = chain
            .gas_price()
            .await
            .map_err(|e| ApiError::Internal(format!("Could not get current gas price: {e}")))?;
        if current > max_allowed {
            return Err(ApiError::HighCongestion(format!(
                "Current network gas price {current} exceeds max gas price allowed {max_allowed}"
            )));
        }
        Ok(())
    }

    pub async fn send_meta_transaction(&self, tx: &TransactionData) -> Result<String, ApiError> {
        match self.broadcast_route() {
            BroadcastRoute::Disabled => Err(ApiError::RelayerUnavailable(
                "Broadcasting is currently disabled by the operator (relayer toggle is OFF). Validation passed; the \
                 meta-transaction was not broadcast."
                    .into(),
            )),
            BroadcastRoute::Oz => {
                self.relayer.as_ref().expect("route Oz implies a provisioned relayer").send_meta_transaction(tx).await
            }
            BroadcastRoute::Direct => {
                self.signer.as_ref().expect("route Direct implies a provisioned signer").send_meta_transaction(tx).await
            }
            BroadcastRoute::Upstream => Err(ApiError::RelayerUnavailable(
                "Upstream forwarding is handled at the HTTP layer; no local broadcast provider is provisioned.".into(),
            )),
            BroadcastRoute::None => Err(ApiError::RelayerUnavailable(
                "No relayer is provisioned for the selected signer preference. Validation passed; broadcast is \
                 unavailable. Set OZ_RELAYER_URL/OZ_RELAYER_ID/OZ_RELAYER_API_KEY, META_TX_BROADCAST_ENABLED=true with \
                 RELAYER_PRIVATE_KEY_FILE (+ RPC_ENDPOINT_POLYGON), or TRANSACTIONS_UPSTREAM_URL, to enable broadcasting."
                    .into(),
            )),
        }
    }
}

fn chain_unavailable(err: ChainError) -> ApiError {
    ApiError::RelayerUnavailable(format!(
        "Could not verify the meta-transaction signature: {err}"
    ))
}

/// Refuses a meta-transaction whose inner call is another
/// `executeMetaTransaction`: every check here reads one level, so the wrapped
/// call would go unpriced and the quota would bind the outer signer while the
/// inner one acts. Refused rather than unwrapped; nothing legitimate nests.
fn check_nested_meta_tx(tx: &TransactionData) -> Result<(), ApiError> {
    let data = hex_to_bytes(&tx.params[1]).unwrap_or_default();
    if let Some(selector) = abi::nested_meta_tx_selector(&data) {
        return Err(ApiError::InvalidTransaction(format!(
            "The meta-transaction wraps another meta-transaction, which this service does not relay. Inner selector: 0x{}",
            hex::encode(selector)
        )));
    }
    Ok(())
}

/// The minimum sale price, on every registered sale target of the relay's
/// chain (`abi::sale_target`); a target outside them has nothing to price.
fn check_sale_price(settings: &Settings, tx: &TransactionData) -> Result<(), ApiError> {
    let Ok(contract_address) = tx.params[0].trim().parse::<Address>() else {
        return Ok(());
    };
    let Some(kind) = abi::sale_target(settings.collections_chain_id, contract_address) else {
        return Ok(());
    };
    let full_data = hex_to_bytes(&tx.params[1]).unwrap_or_default();
    let Some(sale_price) = abi::get_sale_price(&full_data, settings.collections_chain_id, kind)
    else {
        return Ok(());
    };
    let min = U256::from_str_radix(&settings.min_sale_value_in_wei, 10)
        .map_err(|e| ApiError::Internal(format!("bad MIN_SALE_VALUE_IN_WEI: {e}")))?;
    if sale_price < min {
        return Err(ApiError::InvalidSalePrice(format!(
            "The transaction data contains a sale price that's lower than the allowed minimum. Sale price: {sale_price} - \
             Minimum price: {min}"
        )));
    }
    Ok(())
}

/// A sale that passes the floor, or a credits manager's `useCredits`: never
/// held back by the relayer's budget for free calls. A sale at the floor
/// between two wallets of one owner counts as paid too: the floor is its cost.
pub fn is_paid_call(settings: &Settings, tx: &TransactionData) -> bool {
    let Ok(contract) = tx.params[0].trim().parse::<Address>() else {
        return false;
    };
    let full_data = hex_to_bytes(&tx.params[1]).unwrap_or_default();
    let credits_manager = super::meta_tx::audited_domains_for(settings.collections_chain_id)
        .iter()
        .any(|(a, d)| *a == contract && d.label.starts_with("CreditsManager"));
    if credits_manager {
        // Only a credits spend; the manager's other functions are free calls.
        // Both executeMetaTransaction forms (split and combined signature).
        return abi::decode_meta_tx_full(&full_data).is_some_and(|m| {
            m.function_signature.get(..4) == Some(&crate::policy_entry::USE_CREDITS_SELECTOR[..])
        });
    }
    let Some(kind) = abi::sale_target(settings.collections_chain_id, contract) else {
        return false;
    };
    abi::get_sale_price(&full_data, settings.collections_chain_id, kind).is_some()
}

fn is_hex_data(s: &str) -> bool {
    match s.strip_prefix("0x") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

fn is_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    crate::auth::decode_hex_0x(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "0xe539E0AED3C1971560517D58277f8dd9aC296281";
    const CONTRACT: &str = "0x7ad72b9f944ea9793cf4055d88f81138cc2c63a0";
    const CALLDATA: &str = "0x0c53c51cffffffffffffffff";

    fn tx(from: &str, params: Value) -> Value {
        json!({ "from": from, "params": params })
    }

    fn schema_msg(res: Result<TransactionData, ApiError>) -> String {
        match res {
            Err(ApiError::InvalidSchema(m)) => m,
            other => panic!("expected ApiError::InvalidSchema, got {other:?}"),
        }
    }

    #[test]
    fn accepts_well_formed_transaction_data() {
        let data = tx(FROM, json!([CONTRACT, CALLDATA]));
        let parsed = validate_transaction_data(&data).expect("valid payload should parse");
        assert_eq!(parsed.from, FROM);
        assert_eq!(
            parsed.params,
            vec![CONTRACT.to_string(), CALLDATA.to_string()]
        );
    }

    #[test]
    fn rejects_from_that_does_not_match_address_pattern() {
        let data = tx("0x1234", json!([CONTRACT, CALLDATA]));
        let msg = schema_msg(validate_transaction_data(&data));
        assert!(msg.contains(r#""instancePath":"/from""#), "{msg}");
        assert!(
            msg.contains(r##""schemaPath":"#/properties/from/pattern""##),
            "{msg}"
        );
        assert!(msg.contains(r#""keyword":"pattern""#), "{msg}");
        assert!(msg.contains(r#""pattern":"^0x[a-fA-F0-9]{40}$""#), "{msg}");
        assert!(
            msg.contains(r#"must match pattern \"^0x[a-fA-F0-9]{40}$\""#),
            "{msg}"
        );
    }

    #[test]
    fn rejects_from_missing_0x_prefix() {
        let data = tx(
            "e539E0AED3C1971560517D58277f8dd9aC296281",
            json!([CONTRACT, CALLDATA]),
        );
        let msg = schema_msg(validate_transaction_data(&data));
        assert!(
            msg.contains(r##""schemaPath":"#/properties/from/pattern""##),
            "{msg}"
        );
    }

    #[test]
    fn rejects_from_with_non_hex_char() {
        let data = tx(
            "0xg539E0AED3C1971560517D58277f8dd9aC296281",
            json!([CONTRACT, CALLDATA]),
        );
        let msg = schema_msg(validate_transaction_data(&data));
        assert!(
            msg.contains(r##""schemaPath":"#/properties/from/pattern""##),
            "{msg}"
        );
    }

    #[test]
    fn rejects_contract_param_that_does_not_match_address_pattern() {
        let data = tx(FROM, json!([CALLDATA, CALLDATA]));
        let msg = schema_msg(validate_transaction_data(&data));
        assert!(msg.contains(r#""instancePath":"/params/0""#), "{msg}");
        assert!(
            msg.contains(r##""schemaPath":"#/properties/params/items/0/pattern""##),
            "{msg}"
        );
        assert!(msg.contains(r#""pattern":"^0x[a-fA-F0-9]{40}$""#), "{msg}");
    }

    #[test]
    fn rejects_calldata_param_that_does_not_match_hex_pattern() {
        let data = tx(FROM, json!([CONTRACT, "0xnothex"]));
        let msg = schema_msg(validate_transaction_data(&data));
        assert!(msg.contains(r#""instancePath":"/params/1""#), "{msg}");
        assert!(
            msg.contains(r##""schemaPath":"#/properties/params/items/1/pattern""##),
            "{msg}"
        );
        assert!(msg.contains(r#""pattern":"^0x[a-fA-F0-9]+$""#), "{msg}");
        assert!(
            msg.contains(r#"must match pattern \"^0x[a-fA-F0-9]+$\""#),
            "{msg}"
        );
    }

    #[test]
    fn rejects_empty_calldata_param() {
        let data = tx(FROM, json!([CONTRACT, "0x"]));
        let msg = schema_msg(validate_transaction_data(&data));
        assert!(
            msg.contains(r##""schemaPath":"#/properties/params/items/1/pattern""##),
            "{msg}"
        );
    }

    #[test]
    fn rejects_oversized_calldata_before_pattern() {
        let oversized = format!("0x{}", "a".repeat(200_001));
        assert_eq!(oversized.chars().count(), 200_003);
        let data = tx(FROM, json!([CONTRACT, oversized]));
        let msg = schema_msg(validate_transaction_data(&data));
        assert!(msg.contains(r#""instancePath":"/params/1""#), "{msg}");
        assert!(
            msg.contains(r##""schemaPath":"#/properties/params/items/1/maxLength""##),
            "{msg}"
        );
        assert!(msg.contains(r#""keyword":"maxLength""#), "{msg}");
        assert!(msg.contains(r#""limit":200002"#), "{msg}");
        assert!(
            msg.contains("must NOT have more than 200002 characters"),
            "{msg}"
        );
    }

    #[test]
    fn accepts_calldata_at_max_length_boundary() {
        let at_limit = format!("0x{}", "a".repeat(200_000));
        assert_eq!(at_limit.chars().count(), CALLDATA_MAX_LENGTH);
        let data = tx(FROM, json!([CONTRACT, at_limit.clone()]));
        let parsed = validate_transaction_data(&data).expect("payload at cap should parse");
        assert_eq!(parsed.params[1], at_limit);
    }

    #[test]
    fn oversized_non_hex_calldata_reports_maxlength_not_pattern() {
        let oversized = format!("0x{}", "z".repeat(200_001));
        let data = tx(FROM, json!([CONTRACT, oversized]));
        let msg = schema_msg(validate_transaction_data(&data));
        assert!(
            msg.contains(r##""schemaPath":"#/properties/params/items/1/maxLength""##),
            "{msg}"
        );
    }

    fn meta_tx_calldata(user_address: &str) -> String {
        let call = abi::executeMetaTransactionCall {
            userAddress: user_address.parse().expect("address"),
            functionSignature: alloy_primitives::Bytes::from_static(&[0xaa]),
            sigR: alloy_primitives::FixedBytes::<32>::repeat_byte(0x11),
            sigS: alloy_primitives::FixedBytes::<32>::repeat_byte(0x22),
            sigV: 27,
        };
        format!(
            "0x{}",
            hex::encode(alloy_sol_types::SolCall::abi_encode(&call))
        )
    }

    #[test]
    fn sender_is_the_address_signed_into_the_calldata() {
        let sender = MetaTxSender::from_meta_tx_calldata(FROM, &meta_tx_calldata(FROM))
            .expect("matching from and userAddress");
        assert_eq!(sender.as_str(), FROM.to_lowercase());
    }

    #[test]
    fn a_from_that_disagrees_with_the_signed_user_address_is_rejected() {
        let victim = "0x1111111111111111111111111111111111111111";
        let attacker = "0x2222222222222222222222222222222222222222";
        let err = MetaTxSender::from_meta_tx_calldata(victim, &meta_tx_calldata(attacker))
            .expect_err("a forged from must not yield a sender");
        let msg = match err {
            ApiError::InvalidTransaction(m) => m,
            other => panic!("expected ApiError::InvalidTransaction, got {other:?}"),
        };
        assert!(msg.contains(victim), "{msg}");
        assert!(msg.contains(attacker), "{msg}");
    }

    #[test]
    fn calldata_with_an_unrecoverable_user_address_yields_no_sender() {
        assert!(matches!(
            MetaTxSender::from_meta_tx_calldata(FROM, CALLDATA),
            Err(ApiError::InvalidTransaction(_))
        ));
    }

    #[test]
    fn pre_broadcast_errors_release_the_slot() {
        for err in [
            ApiError::InvalidTransaction("bad".into()),
            ApiError::InvalidSchema("bad".into()),
            ApiError::InvalidContractAddress("bad".into()),
            ApiError::InvalidSalePrice("bad".into()),
            ApiError::HighCongestion("busy".into()),
            ApiError::RelayerFailed("network".into()),
            ApiError::RelayerUnavailable("off".into()),
        ] {
            assert_eq!(
                reservation_disposition(&err),
                ReservationDisposition::Release,
                "{err:?} is a pre-broadcast failure and must refund the slot"
            );
        }
    }

    #[test]
    fn post_broadcast_errors_keep_the_slot() {
        for err in [
            ApiError::RelayReverted("reverted".into()),
            ApiError::RelayerTimeout("too slow".into()),
        ] {
            assert_eq!(
                reservation_disposition(&err),
                ReservationDisposition::Keep,
                "{err:?} is post-broadcast/indeterminate and must keep the slot consumed"
            );
        }
    }

    #[test]
    fn upstream_is_the_auto_fallback_after_local_providers() {
        use SignerPreference::*;
        assert_eq!(
            select_broadcast_route(false, Auto, true, true, true),
            BroadcastRoute::Disabled
        );
        assert_eq!(
            select_broadcast_route(true, Auto, true, false, true),
            BroadcastRoute::Oz
        );
        assert_eq!(
            select_broadcast_route(true, Auto, false, true, true),
            BroadcastRoute::Direct
        );
        assert_eq!(
            select_broadcast_route(true, Oz, true, true, true),
            BroadcastRoute::Oz
        );
        assert_eq!(
            select_broadcast_route(true, Direct, true, true, true),
            BroadcastRoute::Direct
        );
        assert_eq!(
            select_broadcast_route(true, Auto, false, false, true),
            BroadcastRoute::Upstream
        );
        assert_eq!(
            select_broadcast_route(true, Auto, false, false, false),
            BroadcastRoute::None
        );
    }

    #[test]
    fn explicit_preference_without_its_provider_fails_instead_of_forwarding() {
        use SignerPreference::*;
        assert_eq!(
            select_broadcast_route(true, Oz, false, true, true),
            BroadcastRoute::None
        );
        assert_eq!(
            select_broadcast_route(true, Direct, true, false, true),
            BroadcastRoute::None
        );
        assert_eq!(
            select_broadcast_route(true, Oz, false, false, false),
            BroadcastRoute::None
        );
        assert_eq!(
            select_broadcast_route(true, Direct, false, false, false),
            BroadcastRoute::None
        );
    }

    #[test]
    fn unclassified_errors_default_to_keep() {
        assert_eq!(
            reservation_disposition(&ApiError::Internal("boom".into())),
            ReservationDisposition::Keep
        );
        assert_eq!(
            reservation_disposition(&ApiError::Conflict("dup".into())),
            ReservationDisposition::Keep
        );
    }

    fn wrap(inner: Vec<u8>) -> String {
        format!(
            "0x{}",
            hex::encode(alloy_sol_types::SolCall::abi_encode(
                &abi::executeMetaTransactionCall {
                    userAddress: Address::ZERO,
                    functionSignature: inner.into(),
                    sigR: Default::default(),
                    sigS: Default::default(),
                    sigV: 27,
                }
            ))
        )
    }

    #[test]
    fn a_meta_transaction_wrapping_another_is_refused_before_any_pricing() {
        let order = alloy_sol_types::SolCall::abi_encode(&abi::executeOrderCall {
            nftAddress: Address::ZERO,
            assetId: U256::from(1u64),
            price: U256::from(5u64),
        });
        let once = TransactionData {
            from: FROM.into(),
            params: vec![CONTRACT.into(), wrap(order.clone())],
        };
        assert!(check_nested_meta_tx(&once).is_ok());
        let nested_calldata = wrap(hex_to_bytes(&wrap(order)).unwrap());
        let nested = TransactionData {
            from: FROM.into(),
            params: vec![CONTRACT.into(), nested_calldata],
        };
        match check_nested_meta_tx(&nested) {
            Err(ApiError::InvalidTransaction(m)) => assert!(m.contains("0x0c53c51c"), "{m}"),
            other => panic!("expected InvalidTransaction, got {other:?}"),
        }
    }

    #[test]
    fn sale_price_floor_applies_to_the_registered_sale_targets_only() {
        use crate::evm::contracts_addrs::DclContracts;
        let settings = Settings::for_tests();
        let contracts = DclContracts::for_chain(137).unwrap();
        let inner = alloy_sol_types::SolCall::abi_encode(&abi::executeOrderCall {
            nftAddress: Address::ZERO,
            assetId: U256::from(1u64),
            price: U256::from(5u64),
        });
        let calldata = format!(
            "0x{}",
            hex::encode(alloy_sol_types::SolCall::abi_encode(
                &abi::executeMetaTransactionCall {
                    userAddress: Address::ZERO,
                    functionSignature: inner.into(),
                    sigR: Default::default(),
                    sigS: Default::default(),
                    sigV: 27,
                }
            ))
        );
        let cheap = TransactionData {
            from: FROM.into(),
            params: vec![format!("{:#x}", contracts.marketplace_v2), calldata.clone()],
        };
        assert!(matches!(
            check_sale_price(&settings, &cheap),
            Err(ApiError::InvalidSalePrice(_))
        ));
        let elsewhere = TransactionData {
            from: FROM.into(),
            params: vec![CONTRACT.into(), calldata],
        };
        assert!(
            check_sale_price(&settings, &elsewhere).is_ok(),
            "the floor is a rule of the registered sale targets"
        );
    }
}
