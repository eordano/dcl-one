//! `POST /topup/mana`: a MANA transfer to the relayer verified through the
//! economy module (the old `/v1/payments/verify` hop), granted once per
//! transaction hash at the memoised oracle rate, floored to whole Credits;
//! `GET /topup/mana/quote` suggests the wei for N Credits with a 2% buffer.
//!
//! A transfer mined while no MANA/USD rate was recorded (a price poller down
//! longer than the freshness window) answers 422 for good. An operator credits
//! it with `POST /admin/credits/grant` and `idempotencyKey` `mana-topup:<txHash>`
//! (lowercase): the grant is booked as a purchase, like the claim, and a later
//! claim of that hash replays it instead of granting again.

use axum::Json;
use axum::extract::{OriginalUri, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

use super::signer_from;
use crate::credits::SharedState;
use crate::credits::admin_store::GrantRequest;
use crate::credits::errors::ApiError;
use crate::credits::escrow::{ManaPaymentVerification, TransferBlock};
use crate::credits::pricing::CREDIT_USD;

const QUOTE_MAX_CREDITS: i64 = 100_000;

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct ManaTopupOut {
    #[serde(rename = "creditsGranted")]
    credits_granted: String,
    available: String,
    #[serde(rename = "txHash")]
    tx_hash: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct ManaTopupQuoteOut {
    credits: String,
    #[serde(rename = "weiSuggested")]
    wei_suggested: String,
    #[serde(rename = "manaUsd")]
    mana_usd: String,
}

#[derive(Debug, Deserialize)]
pub struct ManaTopupBody {
    #[serde(rename = "txHash")]
    pub tx_hash: String,
}

fn validate_tx_hash(raw: &str) -> Result<String, ApiError> {
    crate::types::normalize_tx_hash(raw)
        .ok_or_else(|| ApiError::bad_request("invalid txHash: expected 0x + 64 hex chars"))
}

/// The grant idempotency-key prefix of a MANA top-up; the transaction hash follows.
pub const MANA_TOPUP_KEY_PREFIX: &str = "mana-topup:";

fn topup_idempotency_key(tx_hash_lowercase: &str) -> String {
    format!("{MANA_TOPUP_KEY_PREFIX}{tx_hash_lowercase}")
}

/// An operator's key for recovering a top-up: `Some(canonical key)` when it
/// names one (prefix in any case, a valid hash), the same key a claim uses.
pub fn recovered_topup_key(key: &str) -> Option<Result<String, ApiError>> {
    let prefix = key.get(..MANA_TOPUP_KEY_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(MANA_TOPUP_KEY_PREFIX) {
        return None;
    }
    Some(
        validate_tx_hash(&key[MANA_TOPUP_KEY_PREFIX.len()..])
            .map(|hash| topup_idempotency_key(&hash)),
    )
}

/// Which confirmed MANA transfers a top-up credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopupPolicy {
    /// `CREDITS_TOPUP_START_BLOCK`: transfers mined earlier are refused.
    pub start_block: Option<u64>,
    /// `CREDITS_TOPUP_MIN_CONFIRMATIONS`: until then the claim answers pending.
    pub min_confirmations: u64,
}

/// A transfer with no recorded MANA/USD rate at its block is credited only this soon after it.
const UNPRICED_MAX_AGE_SECS: i64 = crate::price::store::SNAPSHOT_MAX_AGE.as_secs() as i64;

#[derive(Debug, PartialEq, Eq)]
enum TopupDecision {
    Pending,
    Granted {
        value_wei: String,
        block: TransferBlock,
    },
}

fn decide(
    verification: ManaPaymentVerification,
    signer: &str,
    policy: &TopupPolicy,
    operator: &[&str],
) -> Result<TopupDecision, ApiError> {
    match verification {
        ManaPaymentVerification::Pending => Ok(TopupDecision::Pending),
        ManaPaymentVerification::Reverted => Err(ApiError::unprocessable(
            "the transaction reverted on-chain; no MANA was transferred",
        )),
        ManaPaymentVerification::NoPayment => Err(ApiError::unprocessable(
            "the transaction is confirmed but contains no MANA transfer to the top-up address",
        )),
        ManaPaymentVerification::Confirmed {
            from,
            to,
            value_wei,
            block,
        } => {
            if from.to_lowercase() != signer.to_lowercase() {
                return Err(ApiError::forbidden(
                    "the MANA payment was sent by a different wallet; only the payer can claim this top-up",
                ));
            }
            if from.eq_ignore_ascii_case(&to)
                || operator.iter().any(|a| a.eq_ignore_ascii_case(&from))
            {
                return Err(ApiError::forbidden(
                    "transfers from the Shop's own wallets are not top-ups",
                ));
            }
            if policy.start_block.is_some_and(|start| block.number < start) {
                return Err(ApiError::unprocessable(
                    "the MANA payment predates MANA top-ups on this node",
                ));
            }
            if block.confirmations < policy.min_confirmations {
                return Ok(TopupDecision::Pending);
            }
            Ok(TopupDecision::Granted { value_wei, block })
        }
    }
}

/// The recorded MANA/USD rate when `block_timestamp` was mined: the newest
/// snapshot at or before it, no older than the snapshot freshness budget.
async fn rate_at(pool: &sqlx::PgPool, block_timestamp: i64) -> Result<Option<String>, ApiError> {
    let Some(at) = chrono::DateTime::from_timestamp(block_timestamp, 0) else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT mana_usd::text AS rate FROM economy.price_snapshots \
         WHERE taken_at <= $1 AND taken_at > $1 - make_interval(secs => $2) \
         ORDER BY taken_at DESC LIMIT 1",
    )
    .bind(at.naive_utc())
    .bind(UNPRICED_MAX_AGE_SECS as f64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get::<String, _>("rate")))
}

/// The rate a transfer is credited at: the lower of the rate at its block and
/// now; with no rate recorded at its block, only a recent transfer is priced.
fn pricing_rate(
    now_rate: &str,
    rate_at_block: Option<String>,
    block_timestamp: i64,
    now_s: i64,
) -> Result<PricingRate, ApiError> {
    match rate_at_block {
        Some(then) => Ok(PricingRate::Lower {
            now: now_rate.to_string(),
            then,
        }),
        None if now_s.saturating_sub(block_timestamp) <= UNPRICED_MAX_AGE_SECS => {
            Ok(PricingRate::Now(now_rate.to_string()))
        }
        None => Err(ApiError::unprocessable(
            "the MANA payment is too old to price: no MANA/USD rate is recorded for its block",
        )),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PricingRate {
    Now(String),
    Lower { now: String, then: String },
}

impl PricingRate {
    async fn resolve(self, pool: &sqlx::PgPool) -> Result<String, ApiError> {
        match self {
            Self::Now(rate) => Ok(rate),
            Self::Lower { now, then } => Ok(sqlx::query(
                "SELECT least($1::numeric, $2::numeric)::text AS rate",
            )
            .bind(now)
            .bind(then)
            .fetch_one(pool)
            .await?
            .get::<String, _>("rate")),
        }
    }
}

pub async fn credits_for_wei(
    pool: &sqlx::PgPool,
    value_wei: &str,
    mana_usd: &str,
) -> Result<String, ApiError> {
    let row = sqlx::query(
        "SELECT floor(($1::numeric / 1e18) * $2::numeric / $3::numeric)::text AS credits",
    )
    .bind(value_wei)
    .bind(mana_usd)
    .bind(CREDIT_USD)
    .fetch_one(pool)
    .await?;
    Ok(row.get::<String, _>("credits"))
}

pub async fn quote_wei(
    pool: &sqlx::PgPool,
    credits: i64,
    mana_usd: &str,
) -> Result<String, ApiError> {
    let row = sqlx::query("SELECT ceil(ceil($1::numeric * $2::numeric / $3::numeric * 1e18) * 102 / 100)::text AS wei")
        .bind(credits)
        .bind(CREDIT_USD)
        .bind(mana_usd)
        .fetch_one(pool)
        .await?;
    Ok(row.get::<String, _>("wei"))
}

/// A trial (`CREDITS_TRIAL_MODE`) takes no MANA for credits: its buyers spend the credits they
/// were granted, and nothing a top-up would credit is part of it.
fn refuse_in_a_trial(state: &SharedState) -> Result<(), ApiError> {
    if state.settings.trial.is_some() {
        return Err(ApiError::forbidden(
            "MANA top-ups are disabled in this test (CREDITS_TRIAL_MODE)",
        ));
    }
    Ok(())
}

pub async fn mana_topup(
    State(state): State<SharedState>,
    original: OriginalUri,
    headers: HeaderMap,
    body: Result<Json<ManaTopupBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    refuse_in_a_trial(&state)?;
    let signer = signer_from(&state, &headers, "post", "/topup/mana", &original).await?;
    let Json(body) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let tx_hash = validate_tx_hash(&body.tx_hash)?;
    let idem = topup_idempotency_key(&tx_hash);

    if let Some(prior) = state.credits.find_grant_by_idempotency_key(&idem).await? {
        if prior.address.to_lowercase() != signer.as_str() {
            return Err(ApiError::forbidden(
                "this transaction already granted Credits to a different wallet",
            ));
        }
        tracing::info!(tx_hash = %tx_hash, signer = %signer.as_str(), "MANA top-up replay served");
        return Ok(Json(ManaTopupOut {
            credits_granted: prior.amount,
            available: prior.available,
            tx_hash,
        })
        .into_response());
    }

    let (verification, mana_usd) = tokio::join!(
        state.broker.verify_mana_payment(&tx_hash),
        state.pricing.fetch_mana_usd()
    );

    let escrow = state.broker.escrow_address();
    let operator: Vec<&str> = escrow.iter().map(String::as_str).collect();
    let (value_wei, block) = match decide(
        verification?,
        signer.as_str(),
        &state.settings.topup,
        &operator,
    )? {
        TopupDecision::Pending => {
            return Ok((StatusCode::ACCEPTED, Json(json!({ "status": "pending" }))).into_response());
        }
        TopupDecision::Granted { value_wei, block } => (value_wei, block),
    };

    let rate_at_block = rate_at(&state.credits.pool, block.timestamp).await?;
    let mana_usd = pricing_rate(
        &mana_usd?,
        rate_at_block,
        block.timestamp,
        chrono::Utc::now().timestamp(),
    )?
    .resolve(&state.credits.pool)
    .await?;
    let credits = credits_for_wei(&state.credits.pool, &value_wei, &mana_usd).await?;
    if !crate::credits::pricing::charge_is_positive(&credits) {
        return Err(ApiError::unprocessable(format!(
            "the MANA payment ({value_wei} wei at {mana_usd} USD/MANA) is worth less than 1 whole Credit"
        )));
    }

    let detail = json!({ "source": "mana-topup", "txHash": tx_hash, "valueWei": value_wei, "manaUsd": mana_usd, "blockNumber": block.number });
    let outcome = state
        .credits
        .admin_grant_credits(GrantRequest {
            address: signer.as_str(),
            amount: &credits,
            kind: "purchase",
            reason: Some("MANA top-up"),
            actor: Some("mana-topup"),
            idempotency_key: Some(&idem),
            detail: &detail,
            studio_name: None,
        })
        .await?;

    tracing::info!(tx_hash = %tx_hash, signer = %signer.as_str(), credits = %outcome.applied, value_wei = %value_wei, replayed = outcome.replayed, "MANA top-up granted");

    Ok(Json(ManaTopupOut {
        credits_granted: outcome.applied,
        available: outcome.available,
        tx_hash,
    })
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct QuoteParams {
    #[serde(default)]
    credits: String,
}

fn validate_quote_credits(raw: &str) -> Result<i64, ApiError> {
    let n: i64 = raw
        .trim()
        .parse()
        .map_err(|_| ApiError::bad_request("credits must be a whole number"))?;
    if n <= 0 {
        return Err(ApiError::bad_request("credits must be positive"));
    }
    if n > QUOTE_MAX_CREDITS {
        return Err(ApiError::bad_request(format!(
            "credits must be at most {QUOTE_MAX_CREDITS}"
        )));
    }
    Ok(n)
}

pub async fn mana_topup_quote(
    State(state): State<SharedState>,
    Query(params): Query<QuoteParams>,
) -> Result<Json<ManaTopupQuoteOut>, ApiError> {
    refuse_in_a_trial(&state)?;
    let credits = validate_quote_credits(&params.credits)?;
    let mana_usd = state.pricing.fetch_mana_usd().await?;
    let wei_suggested = quote_wei(&state.credits.pool, credits, &mana_usd).await?;
    Ok(Json(ManaTopupQuoteOut {
        credits: credits.to_string(),
        wei_suggested,
        mana_usd,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNER: &str = "0x2222222222222222222222222222222222222222";
    const OTHER: &str = "0x3333333333333333333333333333333333333333";

    const PAY_TO: &str = "0x1111111111111111111111111111111111111111";
    const ESCROW: &str = "0x4444444444444444444444444444444444444444";
    const POLICY: TopupPolicy = TopupPolicy {
        start_block: Some(1_000),
        min_confirmations: 64,
    };
    const BLOCK: TransferBlock = TransferBlock {
        number: 2_000,
        timestamp: 1_700_000_000,
        confirmations: 64,
    };

    fn confirmed_at(from: &str, wei: &str, block: TransferBlock) -> ManaPaymentVerification {
        ManaPaymentVerification::Confirmed {
            from: from.into(),
            to: PAY_TO.into(),
            value_wei: wei.into(),
            block,
        }
    }

    fn confirmed(from: &str, wei: &str) -> ManaPaymentVerification {
        confirmed_at(from, wei, BLOCK)
    }

    fn decide(v: ManaPaymentVerification, signer: &str) -> Result<TopupDecision, ApiError> {
        super::decide(v, signer, &POLICY, &[ESCROW])
    }

    #[test]
    fn mainnet_top_ups_need_a_start_block_and_testnets_do_not() {
        use crate::credits::{RawSettings, Settings};
        let raw = |chain: i64, start: Option<u64>| RawSettings {
            topups_enabled: true,
            usage_chain_id: chain,
            topup_start_block: start,
            ..RawSettings::default()
        };
        let err = Settings::parse(raw(137, None)).err().expect("refused");
        assert!(
            err.to_string().contains("CREDITS_TOPUP_START_BLOCK"),
            "{err}"
        );
        assert!(Settings::parse(raw(137, Some(70_000_000))).is_ok());
        assert!(Settings::parse(raw(80_002, None)).is_ok());
        assert!(
            Settings::parse(RawSettings {
                topups_enabled: false,
                ..raw(137, None)
            })
            .is_ok(),
            "a node without the top-up rail starts as before"
        );
    }

    #[test]
    fn shallow_transfers_stay_pending_until_deep_enough() {
        let shallow = TransferBlock {
            confirmations: 63,
            ..BLOCK
        };
        assert_eq!(
            decide(confirmed_at(SIGNER, "42", shallow), SIGNER).unwrap(),
            TopupDecision::Pending
        );
        assert!(matches!(
            decide(confirmed_at(SIGNER, "42", BLOCK), SIGNER).unwrap(),
            TopupDecision::Granted { .. }
        ));
    }

    #[test]
    fn transfers_before_the_start_block_are_refused() {
        let early = TransferBlock {
            number: 999,
            ..BLOCK
        };
        let err = decide(confirmed_at(SIGNER, "42", early), SIGNER).unwrap_err();
        assert!(matches!(err, ApiError::Unprocessable(_)), "got {err:?}");
        let at_start = TransferBlock {
            number: 1_000,
            ..BLOCK
        };
        assert!(decide(confirmed_at(SIGNER, "42", at_start), SIGNER).is_ok());
    }

    #[test]
    fn operator_wallets_cannot_claim_their_funding() {
        for operator in [PAY_TO, ESCROW] {
            let err = decide(confirmed(operator, "42"), operator).unwrap_err();
            assert!(matches!(err, ApiError::Forbidden(_)), "{operator}: {err:?}");
        }
    }

    #[test]
    fn an_unrecorded_rate_prices_only_recent_transfers() {
        let ts = BLOCK.timestamp;
        assert_eq!(
            pricing_rate("0.3", None, ts, ts + UNPRICED_MAX_AGE_SECS).unwrap(),
            PricingRate::Now("0.3".into())
        );
        let err = pricing_rate("0.3", None, ts, ts + UNPRICED_MAX_AGE_SECS + 1).unwrap_err();
        assert!(matches!(err, ApiError::Unprocessable(_)), "got {err:?}");
        assert_eq!(
            pricing_rate("0.3", Some("0.2".into()), ts, ts + 86_400).unwrap(),
            PricingRate::Lower {
                now: "0.3".into(),
                then: "0.2".into()
            },
            "a recorded rate prices any age at the lower of then and now"
        );
    }

    #[test]
    fn wire_identity_mana_topup_out() {
        let out = ManaTopupOut {
            credits_granted: "250".into(),
            available: "300".into(),
            tx_hash: "0xabc".into(),
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({ "creditsGranted": "250", "available": "300", "txHash": "0xabc" })
        );
    }

    #[test]
    fn wire_identity_mana_topup_quote_out() {
        let out = ManaTopupQuoteOut {
            credits: "250".into(),
            wei_suggested: "102000000000000000000".into(),
            mana_usd: "0.25".into(),
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({ "credits": "250", "weiSuggested": "102000000000000000000", "manaUsd": "0.25" })
        );
    }

    #[test]
    fn pending_maps_to_a_202_marker() {
        assert_eq!(
            decide(ManaPaymentVerification::Pending, SIGNER).unwrap(),
            TopupDecision::Pending
        );
    }

    #[test]
    fn reverted_and_no_payment_are_422() {
        for v in [
            ManaPaymentVerification::Reverted,
            ManaPaymentVerification::NoPayment,
        ] {
            let err = decide(v, SIGNER).unwrap_err();
            assert!(matches!(err, ApiError::Unprocessable(_)), "got {err:?}");
        }
    }

    #[test]
    fn someone_elses_payment_is_403() {
        let err = decide(confirmed(OTHER, "1000000000000000000"), SIGNER).unwrap_err();
        assert!(matches!(err, ApiError::Forbidden(_)), "got {err:?}");
    }

    #[test]
    fn own_payment_grants_case_insensitively() {
        let got = decide(
            confirmed(&SIGNER.to_uppercase().replace("0X", "0x"), "42"),
            SIGNER,
        )
        .unwrap();
        assert_eq!(
            got,
            TopupDecision::Granted {
                value_wei: "42".into(),
                block: BLOCK,
            }
        );
    }

    #[test]
    fn tx_hash_validation_and_idempotency_key() {
        let mixed = format!("0x{}", "Ab".repeat(32));
        let lower = mixed.to_lowercase();
        assert_eq!(validate_tx_hash(&mixed).unwrap(), lower);
        assert_eq!(
            topup_idempotency_key(&validate_tx_hash(&mixed).unwrap()),
            topup_idempotency_key(&validate_tx_hash(&lower).unwrap())
        );
        assert_eq!(topup_idempotency_key(&lower), format!("mana-topup:{lower}"));
        assert!(validate_tx_hash("0x1234").is_err());
        assert!(validate_tx_hash(&format!("0x{}", "g".repeat(64))).is_err());
        assert!(validate_tx_hash(&"a".repeat(66)).is_err());
        assert!(validate_tx_hash("").is_err());
    }

    #[test]
    fn quote_credits_validation() {
        assert_eq!(validate_quote_credits("1").unwrap(), 1);
        assert_eq!(validate_quote_credits(" 250 ").unwrap(), 250);
        assert_eq!(validate_quote_credits("100000").unwrap(), 100_000);
        assert!(validate_quote_credits("0").is_err());
        assert!(validate_quote_credits("-5").is_err());
        assert!(validate_quote_credits("2.5").is_err());
        assert!(validate_quote_credits("").is_err());
        assert!(validate_quote_credits("100001").is_err());
        assert!(validate_quote_credits("nope").is_err());
    }
}
