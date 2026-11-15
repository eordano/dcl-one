//! Packs, purchases and the Stripe side of the ledger: the active-pack memo,
//! the pending purchase rows the intent/session flows insert, the webhook's
//! `paid` flip (locked row, expected currency and amount, one statement), the reversals
//! (bounded by the PURCHASE through `revoked_credits <= credits`, never by
//! spend rows) and the completion-based event de-duplication.

use serde_json::Value as JsonValue;
use sqlx::Row;

use super::errors::ApiError;
use super::store::Credits;

pub(crate) fn validate_sku(raw: &str) -> Result<String, ApiError> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 100 {
        return Err(ApiError::bad_request("invalid sku"));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_graphic() && c != '/' && c != '\\')
    {
        return Err(ApiError::bad_request("invalid sku"));
    }
    Ok(s.to_string())
}

/// A purchase a settled event just flipped to `paid`, read from its row:
/// what the grant credits, and the keys its ledger row and its recorded
/// reversals go by, whichever Stripe event settled it.
#[derive(Debug, Clone)]
pub struct SettledPurchase {
    pub address: String,
    pub credits: String,
    pub sku: String,
    /// Set for a pack order opened as a Checkout Session.
    pub order_id: Option<String>,
    pub checkout_session: Option<String>,
    /// The payment intent the purchase holds after the flip: the one it
    /// already had (an unpaid completion records it), else the event's.
    pub payment_intent: Option<String>,
}

#[derive(Debug, Clone)]
pub enum MarkPaidOutcome {
    Granted(SettledPurchase),
    AmountMismatch {
        expected_cents: i64,
        charged_cents: i64,
    },
    /// The charge is in another currency: its minor units are not the
    /// purchase's, so no amount comparison can approve it.
    CurrencyMismatch {
        expected: String,
        charged: String,
    },
    /// No purchase goes by the event's key.
    NoPurchase,
    /// The purchase is no longer settleable ([`SETTLEABLE_STATUSES`]): an
    /// earlier event credited it (`paid`, `refunded`, `disputed`), or its
    /// payment failed (`failed`).
    NotSettleable {
        status: String,
    },
}

/// A reversal returns the buyer's money, so our side must REMOVE the credits
/// that purchase granted. This used to be modelled as a "refund" and executed
/// through the wallet refund path, which ADDS credits -- paying the buyer twice.
#[derive(Debug, Clone)]
pub enum ReversalOutcome {
    Reversed {
        address: String,
        /// Credits charged back against the purchase. Bounded by the purchase:
        /// cumulative charge-backs can never exceed the credits it granted.
        charged_back: String,
        /// `charged_back` minus whatever the buyer had already spent.
        removed: String,
        /// Credits that could not be clawed back -- an unrecovered loss.
        shortfall: String,
        /// Whether `shortfall` is strictly positive (decided in NUMERIC).
        has_shortfall: bool,
    },
    NothingToReverse,
    NoPaidPurchase,
}

enum ReversalTarget {
    /// Stripe `charge.refunded` carries the CUMULATIVE refunded minor units.
    Cumulative(i64),
    /// A dispute/chargeback reverses the whole charge.
    Full,
}

#[derive(Debug, Clone)]
pub struct PackRow {
    pub sku: String,
    pub title: String,
    pub credits: String,
    pub price_cents: i64,
    pub currency: String,
    pub sort_order: i32,
}

/// How many of one pack a single checkout may buy (upstream's default cap):
/// a creator paying a fee larger than the biggest pack needs two or three,
/// and each extra unit is exposure on one card charge.
pub const MAX_PACK_QUANTITY: u32 = 5;

/// Every surface that buys credits; the checkout records which one started
/// it and returns the buyer there. An unrecognised value is unknown (`None`),
/// never the Shop: defaulting it would credit the Shop with another surface's
/// purchases.
pub const MARKETPLACE_SOURCES: [&str; 3] = ["website", "client", "builder"];

pub fn parse_marketplace_source(value: Option<&JsonValue>) -> Option<&'static str> {
    let value = value?.as_str()?;
    MARKETPLACE_SOURCES
        .iter()
        .copied()
        .find(|source| *source == value)
}

/// One checkout of `quantity` units of a pack. The order stores the TOTALS,
/// so the webhook's amount check and grant read one number and never
/// re-multiply; `quantity` itself moves no money.
#[derive(Debug, Clone)]
pub struct PackOrder<'a> {
    pub pack: &'a PackRow,
    pub quantity: u32,
    pub credits: String,
    pub amount_cents: i64,
    /// The surface that opened the checkout (`MARKETPLACE_SOURCES`).
    pub source: Option<&'static str>,
}

impl PackRow {
    /// `None` when the totals do not fit (never for a real pack).
    pub fn order(&self, quantity: u32) -> Option<PackOrder<'_>> {
        Some(PackOrder {
            pack: self,
            quantity,
            credits: multiply_decimal(&self.credits, quantity)?,
            amount_cents: self.price_cents.checked_mul(i64::from(quantity))?,
            source: None,
        })
    }
}

impl PackOrder<'_> {
    pub fn from_source(self, source: Option<&'static str>) -> Self {
        Self { source, ..self }
    }
}

/// `text × factor`, exactly, for PostgreSQL's plain `numeric::text` output
/// (`[-]digits[.digits]`); `None` for anything else.
fn multiply_decimal(text: &str, factor: u32) -> Option<String> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int_part, frac_part) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if int_part.is_empty()
        || !int_part
            .bytes()
            .chain(frac_part.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut digits: Vec<u64> = int_part
        .bytes()
        .chain(frac_part.bytes())
        .map(|b| u64::from(b - b'0'))
        .collect();
    let mut carry = 0u64;
    for digit in digits.iter_mut().rev() {
        let product = *digit * u64::from(factor) + carry;
        *digit = product % 10;
        carry = product / 10;
    }
    let mut head = Vec::new();
    while carry > 0 {
        head.push(carry % 10);
        carry /= 10;
    }
    head.reverse();
    head.extend(digits);
    let split = head.len() - frac_part.len();
    let int_digits: String = head[..split]
        .iter()
        .map(|d| char::from(b'0' + *d as u8))
        .collect();
    let int_digits = int_digits.trim_start_matches('0');
    let int_digits = if int_digits.is_empty() {
        "0"
    } else {
        int_digits
    };
    let frac_digits: String = head[split..]
        .iter()
        .map(|d| char::from(b'0' + *d as u8))
        .collect();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(int_digits);
    if !frac_digits.is_empty() {
        out.push('.');
        out.push_str(&frac_digits);
    }
    Some(out)
}

#[derive(Debug, Clone)]
pub struct OrderRow {
    pub order_id: String,
    pub address: String,
    pub sku: String,
    pub credits: String,
    pub amount_cents: i64,
    pub status: String,
    pub failure_reason: Option<String>,
}

impl Credits {
    pub async fn list_active_packs(&self) -> Result<Vec<PackRow>, ApiError> {
        self.packs
            .get_or_fetch((), || self.list_active_packs_uncached())
            .await
    }

    async fn list_active_packs_uncached(&self) -> Result<Vec<PackRow>, ApiError> {
        let rows = sqlx::query(
            "SELECT sku, title, credits::text AS credits, price_cents, currency, sort_order \
             FROM credits.packs WHERE active = TRUE \
             ORDER BY sort_order, price_cents, sku",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_pack).collect())
    }

    /// An active pack by sku, served from the same memoized row set as the listing.
    pub async fn get_pack(&self, sku: &str) -> Result<Option<PackRow>, ApiError> {
        Ok(self
            .list_active_packs()
            .await?
            .into_iter()
            .find(|p| p.sku == sku))
    }

    pub(crate) fn invalidate_packs(&self) {
        self.packs.clear();
    }

    pub async fn insert_pending_purchase(
        &self,
        address: &str,
        pack: &PackRow,
        payment_intent_id: &str,
    ) -> Result<i64, ApiError> {
        let row = sqlx::query(
            "INSERT INTO credits.purchases \
                 (address, sku, credits, amount_cents, currency, stripe_payment_intent, method, status) \
             VALUES ($1, $2, $3::numeric, $4, $5, $6, 'card', 'pending') \
             RETURNING id",
        )
        .bind(address)
        .bind(&pack.sku)
        .bind(&pack.credits)
        .bind(pack.price_cents)
        .bind(&pack.currency)
        .bind(payment_intent_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("id"))
    }

    pub async fn insert_pending_order(
        &self,
        order_id: &str,
        address: &str,
        order: &PackOrder<'_>,
        checkout_session_id: &str,
    ) -> Result<(), ApiError> {
        sqlx::query(
            "INSERT INTO credits.purchases \
                 (order_id, address, sku, credits, amount_cents, quantity, source, currency, stripe_checkout_session, method, status) \
             VALUES ($1, $2, $3, $4::numeric, $5, $6, $7, $8, $9, 'card', 'pending')",
        )
        .bind(order_id)
        .bind(address)
        .bind(&order.pack.sku)
        .bind(&order.credits)
        .bind(order.amount_cents)
        .bind(order.quantity as i32)
        .bind(order.source)
        .bind(&order.pack.currency)
        .bind(checkout_session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_order(
        &self,
        order_id: &str,
        address: &str,
    ) -> Result<Option<OrderRow>, ApiError> {
        let row = sqlx::query(
            "SELECT order_id, address, sku, credits::text AS credits, amount_cents, status, failure_reason \
             FROM credits.purchases WHERE order_id = $1 AND address = $2",
        )
        .bind(order_id)
        .bind(address)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(map_order))
    }

    /// [`Self::get_order`] plus the signer's balance in whole credits (its
    /// floor; 0 without a wallet row), decided in NUMERIC.
    pub async fn get_order_with_balance(
        &self,
        order_id: &str,
        address: &str,
    ) -> Result<Option<(OrderRow, i64)>, ApiError> {
        let row = sqlx::query(
            "SELECT o.order_id, o.address, o.sku, o.credits::text AS credits, o.amount_cents, \
                    o.status, o.failure_reason, \
                    COALESCE(floor(w.available)::bigint, 0) AS available \
             FROM credits.purchases o \
             LEFT JOIN credits.wallets w ON w.address = o.address \
             WHERE o.order_id = $1 AND o.address = $2",
        )
        .bind(order_id)
        .bind(address)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let available: i64 = r.get("available");
            (map_order(r), available)
        }))
    }

    /// Only for a session whose payment has SETTLED (the caller checks
    /// `payment_status`). `payment_intent` is the session's `payment_intent`,
    /// stored on the order when it has none yet, so a later `charge.refunded`
    /// / `charge.dispute.*` (keyed by payment intent) finds the purchase to
    /// revoke; it is locked first ([`Self::lock_payment_in_tx`]). After a
    /// grant the caller applies [`Self::apply_recorded_reversals_in_tx`] for
    /// the intent the purchase holds ([`SettledPurchase::payment_intent`]).
    /// When that one was already stored, no further lock is needed: a
    /// reversal's own apply locks this same purchase row, so it either ran
    /// before this flip (and committed its record first) or waits for this
    /// transaction and then finds the purchase paid.
    pub(crate) async fn mark_order_paid_by_session_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session_id: &str,
        event_id: &str,
        charged: Charge<'_>,
        payment_intent: Option<&str>,
    ) -> Result<MarkPaidOutcome, ApiError> {
        if let Some(payment_intent) = payment_intent {
            Self::lock_payment_in_tx(tx, payment_intent).await?;
        }
        Self::mark_paid_in_tx(
            tx,
            MARK_PAID_BY_SESSION,
            session_id,
            event_id,
            charged,
            payment_intent,
        )
        .await
    }

    pub async fn mark_order_status_by_session(
        &self,
        session_id: &str,
        status: &str,
        failure_reason: Option<&str>,
    ) -> Result<bool, ApiError> {
        Self::mark_order_status_by_session_on(&self.pool, session_id, status, failure_reason).await
    }

    /// Retires a pending order. One whose payment is processing (see
    /// [`Self::mark_order_processing_by_session_in_tx`]) is only ever retired
    /// as `failed`, by its own payment's outcome; a cancel, an expiry or a
    /// resume leaves it pending. The payment's failure also closes an order
    /// already retired as `abandoned`: otherwise an unpaid completion
    /// retried after the failure would mark it processing again, with no
    /// outcome left to settle it. A `failed` order is not settleable
    /// ([`SETTLEABLE_STATUSES`]), so nothing revives it, and a credited one
    /// (`paid`, `refunded`, `disputed`) is never moved here.
    pub(crate) async fn mark_order_status_by_session_on<'e, E>(
        executor: E,
        session_id: &str,
        status: &str,
        failure_reason: Option<&str>,
    ) -> Result<bool, ApiError>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let res = sqlx::query(
            "UPDATE credits.purchases \
             SET status = $2, failure_reason = COALESCE($3, failure_reason), updated_at = timezone('UTC', now()) \
             WHERE stripe_checkout_session = $1 \
               AND (status = 'pending' OR ($2 = 'failed' AND status = 'abandoned')) \
               AND (stripe_payment_intent IS NULL OR $2 = 'failed')",
        )
        .bind(session_id)
        .bind(status)
        .bind(failure_reason)
        .execute(executor)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// The session completed but its payment is still processing (a bank
    /// debit takes days): the order records the session's payment intent,
    /// which marks it as awaiting that payment's outcome, and stays (or, if
    /// a sweep or cancel retired it meanwhile, is again) `pending`, shown as
    /// `processing`. Nothing but the outcome moves it from there.
    pub(crate) async fn mark_order_processing_by_session_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session_id: &str,
        payment_intent: &str,
    ) -> Result<bool, ApiError> {
        let res = sqlx::query(
            "UPDATE credits.purchases \
             SET status = 'pending', failure_reason = NULL, \
                 stripe_payment_intent = COALESCE(stripe_payment_intent, $2), \
                 updated_at = timezone('UTC', now()) \
             WHERE stripe_checkout_session = $1 AND status = ANY($3::text[])",
        )
        .bind(session_id)
        .bind(payment_intent)
        .bind(&SETTLEABLE_STATUSES[..])
        .execute(&mut **tx)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// The current status of a session's order, for the webhook's logs.
    pub(crate) async fn order_status_by_session_on<'e, E>(
        executor: E,
        session_id: &str,
    ) -> Result<Option<String>, ApiError>
    where
        E: sqlx::PgExecutor<'e>,
    {
        Ok(sqlx::query_scalar(
            "SELECT status FROM credits.purchases WHERE stripe_checkout_session = $1",
        )
        .bind(session_id)
        .fetch_optional(executor)
        .await?)
    }

    /// Any purchase holding this PaymentIntent, a Checkout order's included
    /// (its unpaid completion records it). Locks the PaymentIntent first;
    /// after a grant the caller applies
    /// [`Self::apply_recorded_reversals_in_tx`].
    pub(crate) async fn mark_purchase_paid_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        payment_intent_id: &str,
        event_id: &str,
        charged: Charge<'_>,
    ) -> Result<MarkPaidOutcome, ApiError> {
        Self::lock_payment_in_tx(tx, payment_intent_id).await?;
        Self::mark_paid_in_tx(
            tx,
            MARK_PAID_BY_PAYMENT_INTENT,
            payment_intent_id,
            event_id,
            charged,
            None,
        )
        .await
    }

    /// Locks the purchase row and flips it to `paid` in one statement when it
    /// is still unpaid ([`SETTLEABLE_STATUSES`]) and the charge matches its
    /// currency and amount; the pre-update row decides the outcome exactly as
    /// the separate SELECT FOR UPDATE + UPDATE did.
    async fn mark_paid_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        sql: &'static str,
        key: &str,
        event_id: &str,
        charged: Charge<'_>,
        payment_intent: Option<&str>,
    ) -> Result<MarkPaidOutcome, ApiError> {
        let row = sqlx::query(sql)
            .bind(key)
            .bind(event_id)
            .bind(charged.cents)
            .bind(payment_intent)
            .bind(charged.currency)
            .bind(&SETTLEABLE_STATUSES[..])
            .fetch_optional(&mut **tx)
            .await?;
        let Some(row) = row else {
            return Ok(MarkPaidOutcome::NoPurchase);
        };
        let amount_cents: i64 = row.get("amount_cents");
        let currency: String = row.get("currency");
        let status: String = row.get("status");
        let existing_event: Option<String> = row.get("stripe_event_id");
        let updated: bool = row.get("updated");

        if updated || (status == "paid" && existing_event.as_deref() == Some(event_id)) {
            return Ok(MarkPaidOutcome::Granted(SettledPurchase {
                address: row.get("address"),
                credits: row.get("credits"),
                sku: row.get("sku"),
                order_id: row.get("order_id"),
                checkout_session: row.get("stripe_checkout_session"),
                payment_intent: row.get("payment_intent"),
            }));
        }
        if !SETTLEABLE_STATUSES.contains(&status.as_str()) {
            return Ok(MarkPaidOutcome::NotSettleable { status });
        }
        if !currency.eq_ignore_ascii_case(charged.currency) {
            return Ok(MarkPaidOutcome::CurrencyMismatch {
                expected: currency,
                charged: charged.currency.to_string(),
            });
        }
        Ok(MarkPaidOutcome::AmountMismatch {
            expected_cents: amount_cents,
            charged_cents: charged.cents,
        })
    }

    /// Stripe `charge.refunded`: `cumulative_refunded_cents` is the running
    /// total for the charge, so each delivery reverses only the increment.
    pub async fn record_charge_refund(
        &self,
        payment_intent_id: &str,
        cumulative_refunded_cents: i64,
        event_id: &str,
    ) -> Result<ReversalOutcome, ApiError> {
        let mut tx = self.pool.begin().await?;
        let outcome = self
            .record_charge_refund_in_tx(
                &mut tx,
                payment_intent_id,
                cumulative_refunded_cents,
                event_id,
            )
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    pub(crate) async fn record_charge_refund_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        payment_intent_id: &str,
        cumulative_refunded_cents: i64,
        event_id: &str,
    ) -> Result<ReversalOutcome, ApiError> {
        let target = ReversalTarget::Cumulative(cumulative_refunded_cents);
        Self::persist_reversal_in_tx(tx, payment_intent_id, &target).await?;
        self.apply_reversal(tx, payment_intent_id, target, None, event_id)
            .await
    }

    /// Stripe `charge.dispute.*`: the whole charge goes back to the buyer.
    pub async fn record_full_reversal(
        &self,
        payment_intent_id: &str,
        status_label: &str,
        event_id: &str,
    ) -> Result<ReversalOutcome, ApiError> {
        let mut tx = self.pool.begin().await?;
        let outcome = self
            .record_full_reversal_in_tx(&mut tx, payment_intent_id, status_label, event_id)
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    pub(crate) async fn record_full_reversal_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        payment_intent_id: &str,
        status_label: &str,
        event_id: &str,
    ) -> Result<ReversalOutcome, ApiError> {
        Self::persist_reversal_in_tx(tx, payment_intent_id, &ReversalTarget::Full).await?;
        self.apply_reversal(
            tx,
            payment_intent_id,
            ReversalTarget::Full,
            Some(status_label),
            event_id,
        )
        .await
    }

    /// Serializes the Stripe events of one PaymentIntent (its settlement and
    /// its reversals) until the transaction ends. Taken before the purchase
    /// row, so a reversal and the grant it must reduce never miss each other:
    /// whichever commits second sees the other.
    pub(crate) async fn lock_payment_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        payment_intent: &str,
    ) -> Result<(), ApiError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('stripe-payment:' || $1, 0))")
            .bind(payment_intent)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Records a reversal against its PaymentIntent whether or not a credited
    /// purchase exists yet (`credits.payment_reversals`): a refund or dispute
    /// can reach us before the settlement that credits the purchase.
    async fn persist_reversal_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        payment_intent: &str,
        target: &ReversalTarget,
    ) -> Result<(), ApiError> {
        Self::lock_payment_in_tx(tx, payment_intent).await?;
        let cumulative = match target {
            ReversalTarget::Cumulative(c) => Some(*c),
            ReversalTarget::Full => None,
        };
        sqlx::query(
            "INSERT INTO credits.payment_reversals (payment_intent, refunded_cents, disputed) \
             VALUES ($1, GREATEST(COALESCE($2::bigint, 0), 0), $2::bigint IS NULL) \
             ON CONFLICT (payment_intent) DO UPDATE \
                 SET refunded_cents = GREATEST(credits.payment_reversals.refunded_cents, EXCLUDED.refunded_cents), \
                     disputed = credits.payment_reversals.disputed OR EXCLUDED.disputed, \
                     updated_at = timezone('UTC', now())",
        )
        .bind(payment_intent)
        .bind(cumulative)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Right after a grant, in its transaction: the reversals recorded for
    /// this PaymentIntent before it was credited, applied to the purchase
    /// just flipped, so returned money never becomes spendable Credits. A
    /// dispute reverses the whole purchase; otherwise the highest cumulative
    /// refund does, bounded by the purchase like any reversal.
    pub(crate) async fn apply_recorded_reversals_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        payment_intent: &str,
        event_id: &str,
    ) -> Result<Option<ReversalOutcome>, ApiError> {
        let recorded: Option<(i64, bool)> = sqlx::query_as(
            "SELECT refunded_cents, disputed FROM credits.payment_reversals WHERE payment_intent = $1",
        )
        .bind(payment_intent)
        .fetch_optional(&mut **tx)
        .await?;
        let (target, status_label) = match recorded {
            Some((_, true)) => (ReversalTarget::Full, Some("disputed")),
            Some((refunded_cents, false)) if refunded_cents > 0 => {
                (ReversalTarget::Cumulative(refunded_cents), None)
            }
            _ => return Ok(None),
        };
        self.apply_reversal(tx, payment_intent, target, status_label, event_id)
            .await
            .map(Some)
    }

    /// The lock, the clamp and the conditional UPDATE are one statement:
    /// `cur` is the locked pre-update row, `upd` only fires for a paid
    /// purchase whose reversed amount grows, and the RETURNING compares new
    /// against old. `revoked_credits` accumulates the charge-backs and is held
    /// to `[0, credits]` by both `LEAST` here and the table's CHECK, so
    /// cumulative charge-backs can never exceed what the purchase granted no
    /// matter how the webhook is replayed or reordered; the terminal event
    /// settles on the exact remaining `credits`, no sub-cent dust left behind.
    async fn apply_reversal(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        payment_intent_id: &str,
        target: ReversalTarget,
        status_label: Option<&str>,
        event_id: &str,
    ) -> Result<ReversalOutcome, ApiError> {
        let cumulative: Option<i64> = match target {
            ReversalTarget::Cumulative(c) => Some(c),
            ReversalTarget::Full => None,
        };
        let row = sqlx::query(
            "WITH cur AS ( \
                 SELECT id, address, amount_cents, refunded_cents, revoked_credits, status, \
                        CASE WHEN $2::bigint IS NULL THEN amount_cents \
                             ELSE LEAST(GREATEST($2::bigint, refunded_cents), amount_cents) END \
                            AS new_refunded \
                 FROM credits.purchases WHERE stripe_payment_intent = $1 FOR UPDATE \
             ), upd AS ( \
                 UPDATE credits.purchases p \
                 SET refunded_cents = c.new_refunded, \
                     revoked_credits = LEAST(p.credits, \
                         CASE WHEN p.amount_cents <= 0 OR c.new_refunded >= p.amount_cents \
                                  THEN p.credits \
                              ELSE p.revoked_credits \
                                   + (p.credits * (c.new_refunded - c.refunded_cents)::numeric \
                                      / p.amount_cents) END), \
                     status = CASE WHEN $3::text IS NOT NULL THEN $3 \
                                   WHEN c.new_refunded >= p.amount_cents THEN 'refunded' \
                                   ELSE p.status END, \
                     updated_at = timezone('UTC', now()) \
                 FROM cur c \
                 WHERE p.id = c.id AND c.status = 'paid' AND c.new_refunded > c.refunded_cents \
                 RETURNING (p.revoked_credits - c.revoked_credits)::text AS charged_back, \
                           (p.revoked_credits > c.revoked_credits) AS has_charge \
             ) \
             SELECT c.address, c.amount_cents, c.status, c.new_refunded, \
                    u.charged_back, u.has_charge \
             FROM cur c LEFT JOIN upd u ON true",
        )
        .bind(payment_intent_id)
        .bind(cumulative)
        .bind(status_label)
        .fetch_optional(&mut **tx)
        .await?;

        let Some(row) = row else {
            return Ok(ReversalOutcome::NoPaidPurchase);
        };
        let status: String = row.get("status");
        if status != "paid" {
            return Ok(ReversalOutcome::NoPaidPurchase);
        }
        let address: String = row.get("address");
        let amount_cents: i64 = row.get("amount_cents");
        let new_refunded: i64 = row.get("new_refunded");
        let charged_back: Option<String> = row.get("charged_back");
        let has_charge: Option<bool> = row.get("has_charge");
        let (Some(charged_back), Some(true)) = (charged_back, has_charge) else {
            return Ok(ReversalOutcome::NothingToReverse);
        };

        let detail = serde_json::json!({
            "source": "stripe",
            "eventId": event_id,
            "paymentIntent": payment_intent_id,
            "refundedCents": new_refunded,
            "amountCents": amount_cents,
        });
        let outcome = self
            .revoke_in_tx(
                tx,
                &address,
                &charged_back,
                &format!("stripe:{}", event_id),
                "stripe payment reversed (refund/dispute)",
                &detail,
            )
            .await?;
        Ok(ReversalOutcome::Reversed {
            address,
            charged_back,
            removed: outcome.removed,
            shortfall: outcome.shortfall,
            has_shortfall: outcome.has_shortfall,
        })
    }

    /// Claims the event for this transaction: a fresh or never-finished event
    /// comes back `true` with `processed_at` already stamped (it only lands if
    /// the caller commits), one finished by an earlier transaction comes back
    /// `false`.
    pub(crate) async fn record_stripe_event_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        event_id: &str,
        event_type: &str,
        payload: &JsonValue,
    ) -> Result<bool, ApiError> {
        let row = sqlx::query(
            "INSERT INTO credits.stripe_events (event_id, type, payload, processed_at) \
             VALUES ($1, $2, $3, timezone('UTC', now())) \
             ON CONFLICT (event_id) DO UPDATE \
                 SET processed_at = COALESCE(credits.stripe_events.processed_at, timezone('UTC', now())) \
             RETURNING (processed_at = timezone('UTC', now())) AS fresh",
        )
        .bind(event_id)
        .bind(event_type)
        .bind(payload)
        .fetch_one(&mut **tx)
        .await?;
        Ok(row.get::<bool, _>("fresh"))
    }
}

/// The statuses a payment may still move: a settled one flips them to
/// `paid`, a processing one back to `pending`. `abandoned` is one because
/// the stale-order sweep (`orders::ABANDON_AFTER`) or a cancel can retire
/// an order before Stripe's events for it arrive; a verified settled
/// payment means the money arrived, so the order is credited. `failed`,
/// `refunded` and `disputed` never come back.
pub const SETTLEABLE_STATUSES: [&str; 2] = ["pending", "abandoned"];

/// What a Stripe event says was charged: `cents` is in `currency`'s minor
/// unit, so it only compares against a purchase in the same currency.
#[derive(Debug, Clone, Copy)]
pub struct Charge<'a> {
    pub cents: i64,
    pub currency: &'a str,
}

macro_rules! mark_paid_sql {
    ($column:literal) => {
        concat!(
            "WITH cur AS ( \
                 SELECT id, address, credits::text AS credits, amount_cents, currency, status, stripe_event_id, \
                        sku, order_id, stripe_checkout_session, stripe_payment_intent \
                 FROM credits.purchases WHERE ",
            $column,
            " = $1 FOR UPDATE \
             ), upd AS ( \
                 UPDATE credits.purchases p \
                 SET status = 'paid', stripe_event_id = $2, stripe_payment_intent = COALESCE(p.stripe_payment_intent, $4::text), \
                     failure_reason = NULL, updated_at = timezone('UTC', now()) \
                 FROM cur WHERE p.id = cur.id AND cur.status = ANY($6::text[]) \
                     AND cur.amount_cents = $3 AND lower(cur.currency) = lower($5::text) \
                 RETURNING p.id \
             ) \
             SELECT cur.address, cur.credits, cur.amount_cents, cur.currency, cur.status, cur.stripe_event_id, \
                    cur.sku, cur.order_id, cur.stripe_checkout_session, \
                    COALESCE(cur.stripe_payment_intent, $4::text) AS payment_intent, \
                    EXISTS (SELECT 1 FROM upd) AS updated \
             FROM cur"
        )
    };
}
const MARK_PAID_BY_PAYMENT_INTENT: &str = mark_paid_sql!("stripe_payment_intent");
const MARK_PAID_BY_SESSION: &str = mark_paid_sql!("stripe_checkout_session");

fn map_order(r: sqlx::postgres::PgRow) -> OrderRow {
    OrderRow {
        order_id: r.get("order_id"),
        address: r.get("address"),
        sku: r.get("sku"),
        credits: r.get("credits"),
        amount_cents: r.get("amount_cents"),
        status: r.get("status"),
        failure_reason: r.get("failure_reason"),
    }
}

fn map_pack(r: sqlx::postgres::PgRow) -> PackRow {
    PackRow {
        sku: r.get("sku"),
        title: r.get("title"),
        credits: r.get("credits"),
        price_cents: r.get("price_cents"),
        currency: r.get("currency"),
        sort_order: r.get("sort_order"),
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;

    fn pack(credits: &str, price_cents: i64) -> PackRow {
        PackRow {
            sku: "pack_5".into(),
            title: "5 Credits".into(),
            credits: credits.into(),
            price_cents,
            currency: "usd".into(),
            sort_order: 0,
        }
    }

    #[test]
    fn an_order_stores_the_multiplied_totals() {
        let p = pack("50", 500);
        let one = p.order(1).unwrap();
        assert_eq!(
            (one.credits.as_str(), one.amount_cents, one.quantity),
            ("50", 500, 1)
        );
        let three = p.order(3).unwrap();
        assert_eq!((three.credits.as_str(), three.amount_cents), ("150", 1500));
        assert!(
            pack("1", i64::MAX).order(2).is_none(),
            "an overflowing charge is no order"
        );
    }

    #[test]
    fn the_source_is_one_of_the_known_surfaces_or_unknown() {
        use serde_json::json;
        assert_eq!(
            parse_marketplace_source(Some(&json!("builder"))),
            Some("builder")
        );
        assert_eq!(
            parse_marketplace_source(Some(&json!("website"))),
            Some("website")
        );
        assert_eq!(
            parse_marketplace_source(Some(&json!("client"))),
            Some("client")
        );
        assert_eq!(parse_marketplace_source(Some(&json!("Builder"))), None);
        assert_eq!(parse_marketplace_source(Some(&json!(3))), None);
        assert_eq!(parse_marketplace_source(None), None);
        assert_eq!(
            pack("50", 500)
                .order(1)
                .unwrap()
                .from_source(Some("builder"))
                .source,
            Some("builder")
        );
    }

    #[test]
    fn decimal_credits_multiply_exactly() {
        assert_eq!(multiply_decimal("12.50", 3).as_deref(), Some("37.50"));
        assert_eq!(multiply_decimal("0.1", 5).as_deref(), Some("0.5"));
        assert_eq!(
            multiply_decimal("99999999999999999999999999.999", 5).as_deref(),
            Some("499999999999999999999999999.995")
        );
        assert_eq!(multiply_decimal("0", 4).as_deref(), Some("0"));
        assert_eq!(multiply_decimal("1e3", 2), None);
        assert_eq!(multiply_decimal(".5", 2), None);
    }
}
