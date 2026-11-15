//! Credits fulfillment claims outbox work, buys through the broker and
//! compensates undelivered units after terminal failure. Each pass retains
//! its fleet lease, per-line transactions and chain-time expiry bounds.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as JsonValue;
use sqlx::Row;

use super::errors::ApiError;
use super::escrow::{Broker, BrokerError, BuyRequest};
use super::pricing::{
    Pricing, TradeLookup, charge_is_positive, payment_is_positive, split_validated_decimal,
};
use super::store::{Credits, now};
use crate::jobs::Jobs;

struct OutboxRow {
    id: i64,
    checkout_id: i64,
    item_id: String,
    collection: String,
    urn: String,
    token_id: Option<String>,
    trade_id: Option<String>,
    basis_wei: Option<String>,
    unit_price_credits: String,
    mode: String,
    attempts: i32,
    buyer: String,
}

enum BrokerResult {
    Confirmed {
        tx_hash: String,
        category: String,
        collection: String,
    },
    Retryable(String),
    Terminal(String),
}

pub const FULFILLMENT_JOB_NAME: &str = "credits-fulfillment";

/// The `credits-fulfillment` job.
#[derive(Clone)]
pub struct OutboxWorker {
    pub credits: Credits,
    pub pricing: Arc<Pricing>,
    pub broker: Arc<dyn Broker>,
    pub max_attempts: i32,
    pub escrow_lock_days: i32,
    pub mock_fulfillment: bool,
}

/// Captcha challenges older than this past their expiry are purged by the
/// tick (consumed and expired rows accumulate one per `GET /captcha`;
/// nothing reads them after expiry).
pub const CAPTCHA_PURGE_AFTER: chrono::Duration = chrono::Duration::days(1);
/// Rows one tick purges at most, so the sweep never holds a long lock.
pub const CAPTCHA_PURGE_BATCH: i64 = 5_000;

/// One tick's bookkeeping: expire stale authorizations (bounded by the
/// CreditUsed index's folded chain time when one is configured, `$5` its
/// chain: `usage::expiry_bound_sql`), purge one batch of old captcha
/// challenges (`expires_at < $3`, the `captcha_challenges_expires_idx`
/// statement), list the checkouts awaiting compensation and the outbox lines
/// awaiting a broker attempt, all in one statement; the clock is a bind.
pub(crate) const OUTBOX_TICK_SQL: &str = "WITH expired AS ( \
         UPDATE credits.authorizations SET status = CASE status WHEN 'retired' THEN 'released' ELSE 'expired' END \
         WHERE status IN ('authorized', 'retired') AND expires_at <= LEAST($2, CASE WHEN $5::bigint IS NULL THEN $2 ELSE \
         COALESCE((SELECT c.folded_block_time FROM credits.usage_index_cursor c WHERE c.chain_id = $5::bigint \
                   AND (credits.authorizations.contract IS NULL OR c.contract = credits.authorizations.contract)), \
                  '-infinity'::timestamp) END) RETURNING 1 \
     ), purged AS ( \
         DELETE FROM credits.captcha_challenges WHERE ctid = ANY (ARRAY( \
             SELECT ctid FROM credits.captcha_challenges WHERE expires_at < $3 ORDER BY expires_at LIMIT $4)) \
         RETURNING 1 \
     ), reversing AS ( \
         SELECT id, address FROM credits.checkouts WHERE status = 'reversing' ORDER BY id LIMIT 50 \
     ), pending AS ( \
         SELECT o.id \
         FROM credits.fulfillment_outbox o \
         JOIN credits.checkouts c ON c.id = o.checkout_id \
         WHERE o.status = 'pending' AND o.attempts < $1 AND c.status = 'fulfilling' \
         ORDER BY o.id LIMIT 50 \
     ) \
     SELECT 'expired' AS kind, count(*)::bigint AS id, NULL::text AS address FROM expired \
     UNION ALL SELECT 'captcha_purged', count(*)::bigint, NULL FROM purged \
     UNION ALL SELECT 'reversing', id, address FROM reversing \
     UNION ALL SELECT 'pending', id, NULL FROM pending";

impl OutboxWorker {
    /// One fleet-wide job under the shared lifecycle (the outbox is shared
    /// rows, so one process drains it at a time): a pass every `interval` and
    /// whenever a write path in this process calls `Credits::kick_fulfillment`
    /// (a kick in another process waits for that process's next tick); a
    /// failing or panicking pass is reported through the job trace, never
    /// fatal.
    pub fn spawn(self, jobs: &Jobs, interval: Duration) {
        let kick = self.credits.kick.clone();
        let period = interval.max(Duration::from_secs(1));
        let worker = Arc::new(self);
        jobs.spawn_leased(FULFILLMENT_JOB_NAME, move |ctx| {
            let worker = worker.clone();
            let kick = kick.clone();
            async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut pass: u64 = 0;
            loop {
                tokio::select! {
                    _ = ctx.cancel.cancelled() => return Ok(()),
                    _ = ticker.tick() => {}
                    _ = kick.notified() => {}
                }
                pass += 1;
                use futures::FutureExt;
                match std::panic::AssertUnwindSafe(worker.run_once())
                    .catch_unwind()
                    .await
                {
                    Ok(Ok(n)) => ctx.progress(format!("pass {pass}: {n} lines attempted")),
                    Ok(Err(e)) => {
                        tracing::error!(worker = FULFILLMENT_JOB_NAME, error = %e, "pass failed");
                        ctx.error(format!("pass {pass}: {e}"));
                    }
                    Err(_) => {
                        tracing::error!(worker = FULFILLMENT_JOB_NAME, "pass panicked");
                        ctx.error(format!("pass {pass}: panic"));
                    }
                }
                ctx.pass_done();
            }
            }
        });
    }

    pub async fn run_once(&self) -> Result<usize, ApiError> {
        let now = now();
        let rows = sqlx::query(OUTBOX_TICK_SQL)
            .bind(self.max_attempts)
            .bind(now)
            .bind(now - CAPTCHA_PURGE_AFTER)
            .bind(CAPTCHA_PURGE_BATCH)
            .bind(self.credits.usage_chain)
            .fetch_all(&self.credits.pool)
            .await?;
        let mut expired = 0i64;
        let mut captcha_purged = 0i64;
        let mut reversing: Vec<(i64, String)> = Vec::new();
        let mut candidates: Vec<i64> = Vec::new();
        for r in rows {
            match r.get::<String, _>("kind").as_str() {
                "expired" => expired = r.get("id"),
                "captcha_purged" => captcha_purged = r.get("id"),
                "reversing" => reversing.push((r.get("id"), r.get("address"))),
                _ => candidates.push(r.get("id")),
            }
        }
        if expired > 0 {
            tracing::info!(expired, "swept stale credit authorizations to 'expired'");
        }
        if captcha_purged > 0 {
            tracing::info!(captcha_purged, "purged expired captcha challenges");
        }
        reversing.sort_by_key(|(id, _)| *id);
        candidates.sort_unstable();

        for (checkout_id, buyer) in reversing {
            self.try_compensate(checkout_id, &buyer).await;
        }

        if candidates.is_empty() {
            return Ok(0);
        }

        if !self.mock_fulfillment && !self.broker.configured() {
            tracing::warn!(
                pending = candidates.len(),
                "fulfilment outbox has pending rows but the economy broker is not configured; idling"
            );
            return Ok(0);
        }

        let mut processed = 0usize;
        for id in candidates {
            if self.process_one(id).await? {
                processed += 1;
            }
        }
        Ok(processed)
    }

    #[tracing::instrument(skip(self), fields(outbox_id = id, checkout_id = tracing::field::Empty))]
    async fn process_one(&self, id: i64) -> Result<bool, ApiError> {
        let mut tx = self.credits.pool.begin().await?;

        let claimed = sqlx::query(
            "SELECT o.id, o.checkout_id, o.item_id, o.collection, \
                    o.urn, o.token_id, o.trade_id, o.basis_wei::text AS basis_wei, \
                    o.unit_price_credits::text AS unit_price_credits, \
                    o.mode, o.attempts, \
                    c.address AS buyer \
             FROM credits.fulfillment_outbox o \
             JOIN credits.checkouts c ON c.id = o.checkout_id \
             WHERE o.id = $1 AND o.status = 'pending' AND c.status = 'fulfilling' \
             FOR UPDATE OF o SKIP LOCKED",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(r) = claimed else {
            tx.rollback().await?;
            return Ok(false);
        };
        let row = OutboxRow {
            id: r.get("id"),
            checkout_id: r.get("checkout_id"),
            item_id: r.get("item_id"),
            collection: r.get("collection"),
            urn: r.get("urn"),
            token_id: r.get("token_id"),
            trade_id: r.get("trade_id"),
            basis_wei: r.get("basis_wei"),
            unit_price_credits: r.get("unit_price_credits"),
            mode: r.get("mode"),
            attempts: r.get("attempts"),
            buyer: r.get("buyer"),
        };
        tracing::Span::current().record("checkout_id", row.checkout_id);

        let result = self.attempt_broker_buy(&row).await;

        match result {
            BrokerResult::Confirmed {
                tx_hash,
                category,
                collection,
            } => {
                sqlx::query(
                    "UPDATE credits.fulfillment_outbox \
                     SET status = 'confirmed', external_ref = $2, updated_at = timezone('UTC', now()) \
                     WHERE id = $1",
                )
                .bind(row.id)
                .bind(&tx_hash)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    "UPDATE credits.checkouts SET status = 'fulfilled', updated_at = timezone('UTC', now()) \
                     WHERE id = $1 AND status = 'fulfilling' \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM credits.fulfillment_outbox \
                           WHERE checkout_id = $1 AND status <> 'confirmed')",
                )
                .bind(row.checkout_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;

                tracing::info!(checkout_id = row.checkout_id, urn = %row.urn, tx_hash = %tx_hash, "fulfilment line confirmed on-chain");

                self.credits
                    .write_usage_grant(
                        &row.buyer,
                        &row.urn,
                        row.token_id.as_deref(),
                        &category,
                        &collection,
                        &tx_hash,
                        self.escrow_lock_days,
                    )
                    .await;
            }
            BrokerResult::Retryable(err) => {
                let new_attempts = row.attempts + 1;
                if new_attempts >= self.max_attempts {
                    self.fail_and_freeze(&mut tx, row.id, row.checkout_id, &err)
                        .await?;
                    tx.commit().await?;
                    self.try_compensate(row.checkout_id, &row.buyer).await;
                } else {
                    sqlx::query(
                        "UPDATE credits.fulfillment_outbox \
                         SET attempts = $2, last_error = $3, updated_at = timezone('UTC', now()) \
                         WHERE id = $1",
                    )
                    .bind(row.id)
                    .bind(new_attempts)
                    .bind(truncate(&err, 1000))
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                }
            }
            BrokerResult::Terminal(err) => {
                self.fail_and_freeze(&mut tx, row.id, row.checkout_id, &err)
                    .await?;
                tx.commit().await?;
                self.try_compensate(row.checkout_id, &row.buyer).await;
            }
        }

        Ok(true)
    }

    async fn fail_and_freeze(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        outbox_id: i64,
        checkout_id: i64,
        err: &str,
    ) -> Result<(), ApiError> {
        sqlx::query(
            "UPDATE credits.fulfillment_outbox \
             SET status = 'failed', attempts = attempts + 1, last_error = $2, updated_at = timezone('UTC', now()) \
             WHERE id = $1",
        )
        .bind(outbox_id)
        .bind(truncate(err, 1000))
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE credits.checkouts SET status = 'reversing', updated_at = timezone('UTC', now()) \
             WHERE id = $1 AND status = 'fulfilling'",
        )
        .bind(checkout_id)
        .execute(&mut **tx)
        .await?;
        self.credits.kick_fulfillment();
        Ok(())
    }

    async fn try_compensate(&self, checkout_id: i64, buyer: &str) {
        if let Err(e) = self.compensate(checkout_id, buyer).await {
            tracing::warn!(checkout_id, error = %e, "compensation failed; checkout stays 'reversing' and will retry next tick");
        }
    }

    /// Refunds the undelivered portion of a frozen checkout (one idempotent
    /// refund per checkout) and resolves it to `reversed` (nothing delivered)
    /// or `failed` (partial).
    #[tracing::instrument(skip(self, buyer))]
    pub async fn compensate(&self, checkout_id: i64, buyer: &str) -> Result<(), ApiError> {
        let agg = sqlx::query(
            "SELECT COALESCE(SUM(unit_price_credits) FILTER (WHERE status <> 'confirmed'), 0)::text AS refund_amt, \
                    COALESCE(SUM(unit_price_credits) FILTER (WHERE status <> 'confirmed'), 0) > 0 AS has_refund, \
                    bool_or(status = 'confirmed') AS any_confirmed \
             FROM credits.fulfillment_outbox WHERE checkout_id = $1",
        )
        .bind(checkout_id)
        .fetch_one(&self.credits.pool)
        .await?;
        let refund_amt: String = agg.get("refund_amt");
        let has_refund: bool = agg.get("has_refund");
        let any_confirmed: bool = agg.get::<Option<bool>, _>("any_confirmed").unwrap_or(false);

        if has_refund {
            let idem = format!("reversal:{}", checkout_id);
            let tx_ref = format!("checkout:{}", checkout_id);
            self.credits
                .refund(buyer, &refund_amt, &tx_ref, Some(&idem))
                .await?;
        }

        let new_status = if any_confirmed { "failed" } else { "reversed" };
        sqlx::query(
            "UPDATE credits.checkouts SET status = $2, updated_at = timezone('UTC', now()) \
             WHERE id = $1 AND status = 'reversing'",
        )
        .bind(checkout_id)
        .bind(new_status)
        .execute(&self.credits.pool)
        .await?;

        if any_confirmed {
            tracing::error!(
                checkout_id,
                refund_credits = %refund_amt,
                "PARTIAL fulfilment reversed: a sibling line was already minted to escrow; the undelivered Credits were refunded and the confirmed line(s) need escrow reconciliation"
            );
        } else {
            tracing::warn!(checkout_id, refund_credits = %refund_amt, "checkout fulfilment failed terminally; Credits refunded and checkout reversed");
        }
        Ok(())
    }

    async fn attempt_broker_buy(&self, row: &OutboxRow) -> BrokerResult {
        let info = match self.pricing.fetch_item(&row.collection, &row.item_id).await {
            Ok(i) => i,
            Err(ApiError::NotFound(m)) | Err(ApiError::BadRequest(m)) => {
                return BrokerResult::Terminal(format!("catalog rejected item: {m}"));
            }
            Err(e) => return BrokerResult::Retryable(format!("catalog fetch failed: {e}")),
        };

        let (price_wei, token_id, trade_payload): (String, Option<String>, Option<JsonValue>) =
            if row.mode == "trade" {
                let Some(trade_id) = row.trade_id.as_deref().filter(|t| !t.trim().is_empty())
                else {
                    return BrokerResult::Terminal(
                    "trade line without a pinned trade id \u{2014} cannot fulfil; compensation will refund the line".to_string(),
                );
                };
                let Some(basis) = row.basis_wei.as_deref().map(str::trim).filter(|b| {
                    !b.is_empty() && b.bytes().all(|x| x.is_ascii_digit()) && payment_is_positive(b)
                }) else {
                    return BrokerResult::Terminal("trade line without a pinned positive basis_wei \u{2014} refusing an unpriced buy".to_string());
                };
                let trade = match self.pricing.fetch_trade(trade_id).await {
                    Ok(TradeLookup::Open(trade)) => trade,
                    Ok(TradeLookup::NotFound) => {
                        return BrokerResult::Terminal(format!(
                            "trade {trade_id} vanished from the market book; compensation will refund the line"
                        ));
                    }
                    Ok(TradeLookup::Closed(why)) => {
                        return BrokerResult::Terminal(format!(
                            "trade {trade_id} is no longer open in the market book ({why}); refusing a doomed on-chain accept \u{2014} compensation will refund the line"
                        ));
                    }
                    Err(e) => return BrokerResult::Retryable(format!("trade fetch failed: {e}")),
                };
                (basis.to_string(), None, Some(trade))
            } else if row.mode == "secondary" {
                match secondary_source(row.token_id.as_deref(), row.basis_wei.as_deref()) {
                    SecondarySource::Pinned {
                        token_id,
                        price_wei,
                    } => (price_wei, Some(token_id), None),
                    SecondarySource::Fresh => {
                        let order = match self
                            .pricing
                            .fetch_open_order(&info.contract_address, &row.item_id, &row.buyer)
                            .await
                        {
                            Ok(Some(order)) => order,
                            Ok(None) => {
                                return BrokerResult::Terminal(
                                    "no fillable MarketplaceV2 listing for this item (secondary)"
                                        .to_string(),
                                );
                            }
                            Err(e) => {
                                return BrokerResult::Retryable(format!(
                                    "order lookup failed: {e}"
                                ));
                            }
                        };
                        let mana_usd = match self.pricing.fetch_mana_usd().await {
                            Ok(v) => v,
                            Err(e) => {
                                return BrokerResult::Retryable(format!(
                                    "oracle fetch for legacy-row reprice failed: {e}"
                                ));
                            }
                        };
                        let fresh_credits = match self
                            .pricing
                            .compute_credit_price(&self.credits.pool, &order.price_wei, &mana_usd)
                            .await
                        {
                            Ok(v) => v,
                            Err(e) => {
                                return BrokerResult::Retryable(format!(
                                    "legacy-row reprice failed: {e}"
                                ));
                            }
                        };
                        if fresh_price_exceeds_charge(&row.unit_price_credits, &fresh_credits) {
                            return BrokerResult::Terminal(format!(
                                "legacy unpinned line: current cheapest listing reprices to {} Credits, above the {} Credits the buyer was charged; refusing to overpay \u{2014} compensation will refund the line",
                                fresh_credits, row.unit_price_credits
                            ));
                        }
                        (order.price_wei, Some(order.token_id), None)
                    }
                }
            } else {
                if !info.store_mintable {
                    return BrokerResult::Terminal(
                    "this item's mint is no longer available from its collection store (off sale, sold out, or moved to a trade); refusing a doomed on-chain buy \u{2014} compensation will refund the line".to_string(),
                );
                }
                let price = match primary_source(row.basis_wei.as_deref()) {
                    PrimarySource::Pinned(basis) => basis,
                    PrimarySource::Fresh => info.price_wei.clone(),
                };
                (price, None, None)
            };

        if payment_is_positive(&price_wei) && !charge_is_positive(&row.unit_price_credits) {
            return BrokerResult::Terminal(format!(
                "charge/payment mismatch: line was charged {} Credits but fulfillment would pay {} wei; refusing to buy at the buyer's expense",
                row.unit_price_credits, price_wei
            ));
        }

        let kind = match row.mode.as_str() {
            "trade" => crate::guards::credits::Kind::Trade {
                trade: trade_payload.clone().unwrap_or(JsonValue::Null),
            },
            "secondary" => crate::guards::credits::Kind::LegacyListing {
                token_id: token_id.clone().unwrap_or_default(),
            },
            _ => crate::guards::credits::Kind::Mint,
        };
        let buyer = match crate::types::EvmAddress::parse(&row.buyer.to_ascii_lowercase()) {
            Ok(buyer) => buyer,
            Err(e) => {
                return BrokerResult::Terminal(format!(
                    "the checkout's buyer is not an address: {e}"
                ));
            }
        };
        let charge = crate::guards::charge_cents(&row.unit_price_credits)
            .unwrap_or(crate::guards::broker::CREDIT_FUNDED);
        let judged = crate::guards::credits::line(
            crate::guards::signed_session(buyer),
            &info,
            &price_wei,
            kind,
        )
        .and_then(|rule| rule.guard_spend(charge).map_err(ApiError::from));
        if let Err(refused) = judged {
            return BrokerResult::Terminal(format!(
                "payment rule: {refused}; compensation will refund the line"
            ));
        }

        if self.mock_fulfillment {
            tracing::info!(checkout_id = %row.checkout_id, outbox_id = %row.id, "CREDITS_MOCK_FULFILLMENT on \u{2014} delivering off-chain, no broker call");
            return BrokerResult::Confirmed {
                tx_hash: format!("mock:checkout:{}:{}", row.checkout_id, row.id),
                category: info.category.clone(),
                collection: info.contract_address.clone(),
            };
        }

        let req = BuyRequest {
            collection: info.contract_address.clone(),
            item_id: row.item_id.clone(),
            token_id,
            price_wei,
            mode: row.mode.clone(),
            buyer: row.buyer.clone(),
            trade: trade_payload,
            idempotency_key: format!("checkout:{}:{}", row.checkout_id, row.id),
            charge_cents: crate::guards::charge_cents(&row.unit_price_credits).map(|c| c.get()),
        };
        match self.broker.buy(&req).await {
            Ok(tx_hash) => BrokerResult::Confirmed {
                tx_hash,
                category: info.category.clone(),
                collection: info.contract_address.clone(),
            },
            Err(BrokerError::Retryable(m)) => BrokerResult::Retryable(m),
            Err(BrokerError::Terminal(m)) => BrokerResult::Terminal(m),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SecondarySource {
    Pinned { token_id: String, price_wei: String },
    Fresh,
}

fn fresh_price_exceeds_charge(charged_credits: &str, fresh_credits: &str) -> bool {
    match (
        parse_nonneg_decimal(charged_credits),
        parse_nonneg_decimal(fresh_credits),
    ) {
        (Some(charged), Some(fresh)) => {
            cmp_decimal(&fresh, &charged) == std::cmp::Ordering::Greater
        }
        _ => true,
    }
}

fn parse_nonneg_decimal(s: &str) -> Option<(String, String)> {
    let (int_part, frac_part) = split_validated_decimal(s)?;
    Some((
        int_part.trim_start_matches('0').to_string(),
        frac_part.trim_end_matches('0').to_string(),
    ))
}

fn cmp_decimal(a: &(String, String), b: &(String, String)) -> std::cmp::Ordering {
    a.0.len()
        .cmp(&b.0.len())
        .then_with(|| a.0.cmp(&b.0))
        .then_with(|| a.1.cmp(&b.1))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrimarySource {
    Pinned(String),
    Fresh,
}

fn primary_source(basis_wei: Option<&str>) -> PrimarySource {
    match basis_wei.map(str::trim) {
        Some(b) if !b.is_empty() && b.bytes().all(|x| x.is_ascii_digit()) => {
            PrimarySource::Pinned(b.to_string())
        }
        _ => PrimarySource::Fresh,
    }
}

fn secondary_source(token_id: Option<&str>, basis_wei: Option<&str>) -> SecondarySource {
    match (token_id, basis_wei) {
        (Some(t), Some(b)) if !t.trim().is_empty() && payment_is_positive(b) => {
            SecondarySource::Pinned {
                token_id: t.to_string(),
                price_wei: b.to_string(),
            }
        }
        _ => SecondarySource::Fresh,
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_listing_wins_when_checkout_recorded_it() {
        assert_eq!(
            secondary_source(Some("2901"), Some("10000000000000000")),
            SecondarySource::Pinned {
                token_id: "2901".into(),
                price_wei: "10000000000000000".into()
            }
        );
    }

    #[test]
    fn pinned_mint_basis_wins_for_primary_rows() {
        assert_eq!(
            primary_source(Some("2500000000000000000")),
            PrimarySource::Pinned("2500000000000000000".into())
        );
        assert_eq!(primary_source(Some("0")), PrimarySource::Pinned("0".into()));
        assert_eq!(
            primary_source(Some(" 123 ")),
            PrimarySource::Pinned("123".into())
        );
    }

    #[test]
    fn unpinned_primary_rows_fall_back_to_the_catalog_price() {
        for raw in [
            None,
            Some(""),
            Some("  "),
            Some("1.5"),
            Some("-1"),
            Some("1e18"),
        ] {
            assert_eq!(primary_source(raw), PrimarySource::Fresh, "{raw:?}");
        }
    }

    #[test]
    fn legacy_rows_fall_back_to_fresh_selection() {
        assert_eq!(secondary_source(None, None), SecondarySource::Fresh);
        assert_eq!(secondary_source(Some("2901"), None), SecondarySource::Fresh);
        assert_eq!(
            secondary_source(None, Some("10000000000000000")),
            SecondarySource::Fresh
        );
        assert_eq!(
            secondary_source(Some("2901"), Some("0")),
            SecondarySource::Fresh
        );
        assert_eq!(
            secondary_source(Some(" "), Some("10000000000000000")),
            SecondarySource::Fresh
        );
    }

    #[test]
    fn fresh_listing_within_charge_proceeds() {
        assert!(
            !fresh_price_exceeds_charge("3", "3"),
            "equal \u{2192} proceed"
        );
        assert!(
            !fresh_price_exceeds_charge("3", "2"),
            "cheaper \u{2192} proceed"
        );
        assert!(!fresh_price_exceeds_charge("3", "2.99"));
        assert!(!fresh_price_exceeds_charge("1.5", "1.5"));
        assert!(!fresh_price_exceeds_charge("10", "9.999"));
    }

    #[test]
    fn fresh_listing_above_charge_is_refused_terminally() {
        assert!(fresh_price_exceeds_charge("3", "4"));
        assert!(fresh_price_exceeds_charge("3", "3.01"));
        assert!(fresh_price_exceeds_charge("1.5", "2"));
        assert!(fresh_price_exceeds_charge("9.999", "10"));
        assert!(fresh_price_exceeds_charge("abc", "1"));
        assert!(fresh_price_exceeds_charge("3", "-1"));
        assert!(fresh_price_exceeds_charge("", ""));
        assert!(fresh_price_exceeds_charge("3", "1e2"));
    }

    #[test]
    fn decimal_comparator_handles_zero_padding_and_lengths() {
        assert!(
            !fresh_price_exceeds_charge("03.50", "3.5"),
            "normalized equal"
        );
        assert!(!fresh_price_exceeds_charge("100", "99"));
        assert!(fresh_price_exceeds_charge("99", "100"));
        assert!(!fresh_price_exceeds_charge("0.5", "0.45"));
        assert!(fresh_price_exceeds_charge("0.45", "0.5"));
        assert!(fresh_price_exceeds_charge("0.4", "0.45"));
        assert!(!fresh_price_exceeds_charge("2", "2.000"));
    }

    #[test]
    fn worker_refuses_to_pay_for_a_zero_charged_line() {
        let refuses = |credits: &str, pay_wei: &str| {
            payment_is_positive(pay_wei) && !charge_is_positive(credits)
        };
        assert!(
            refuses("0", "10000000000000000"),
            "charged 0, pays >0 \u{2192} refuse"
        );
        assert!(
            refuses("0.00", "1"),
            "charged 0.00, pays >0 \u{2192} refuse"
        );
        assert!(
            !refuses("3", "10000000000000000"),
            "charged >0 \u{2192} proceed"
        );
        assert!(!refuses("0", "0"), "free mint: pays 0 \u{2192} proceed");
    }

    /// `parse_nonneg_decimal` shares its grammar with `charge_is_positive`:
    /// scientific notation and a stray extra `.` are rejected, there is no
    /// magnitude bound, and unlike `CreditAmount` surrounding whitespace is
    /// tolerated.
    #[test]
    fn characterization_parse_nonneg_decimal() {
        assert_eq!(parse_nonneg_decimal("1e18"), None);
        assert_eq!(parse_nonneg_decimal("1E18"), None);
        assert_eq!(
            parse_nonneg_decimal(" 1.5 "),
            Some(("1".to_string(), "5".to_string()))
        );
        assert_eq!(
            parse_nonneg_decimal(".5"),
            Some(("".to_string(), "5".to_string()))
        );
        assert_eq!(
            parse_nonneg_decimal("5."),
            Some(("5".to_string(), "".to_string()))
        );
        assert_eq!(
            parse_nonneg_decimal("01.50"),
            Some(("1".to_string(), "5".to_string()))
        );
        assert_eq!(parse_nonneg_decimal(""), None);
        assert_eq!(parse_nonneg_decimal("-1"), None);
        assert_eq!(parse_nonneg_decimal("1.2.3"), None);
        let huge = "9".repeat(50);
        assert_eq!(
            parse_nonneg_decimal(&huge),
            Some((huge.clone(), "".to_string())),
            "no magnitude bound here, unlike CreditAmount"
        );
    }
}
