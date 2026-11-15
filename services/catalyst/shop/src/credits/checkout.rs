//! The cart and the checkout saga. `POST /checkout` (Idempotency-Key)
//! re-prices the cart server-side, debits the total ONCE as a `spend` row
//! (reject-if-short, never a clamp), moves the checkout to `fulfilling` and
//! expands each line into `credits.fulfillment_outbox` rows (one per unit,
//! one broker buy each, the secondary listing's `token_id` + `basis_wei` or
//! the v3 `trade_id` pinned). Commit completes before fulfillment is woken.
//! The fulfillment module owns broker attempts and compensation.

use serde_json::Value as JsonValue;
use sqlx::Row;

use super::errors::ApiError;
use super::store::Credits;
use super::wallet::GrantOutcome;

#[derive(Debug, Clone)]
pub struct CartItemRow {
    pub item_id: String,
    pub collection: String,
    pub urn: String,
    pub category: String,
    pub qty: i32,
    pub unit_price_credits: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CartLine<'a> {
    pub address: &'a str,
    pub item_id: &'a str,
    pub collection: &'a str,
    pub urn: &'a str,
    pub category: &'a str,
    pub qty: i32,
}

#[derive(Debug, Clone)]
pub struct CartView {
    pub items: Vec<CartItemRow>,
    pub total_credits: String,
}

#[derive(Debug, Clone)]
pub struct RepricedLine {
    pub item_id: String,
    pub collection: String,
    pub urn: String,
    pub category: String,
    pub qty: i32,
    pub unit_price_credits: String,
    pub token_id: Option<String>,
    pub trade_id: Option<String>,
    pub basis_wei: Option<String>,
    pub mode: String,
}

#[derive(Debug, Clone)]
pub struct CheckoutOutcome {
    pub id: i64,
    pub status: String,
    pub replayed: bool,
}

#[derive(Debug, Clone)]
pub struct CheckoutRow {
    pub id: i64,
    pub address: String,
    pub total_credits: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct CheckoutIdemRow {
    pub id: i64,
    pub address: String,
    pub status: String,
}

/// A checkout's `Idempotency-Key` is the wallet's own namespace.
pub(crate) fn scoped_checkout_key(address: &str, idempotency_key: &str) -> String {
    format!("{}:{idempotency_key}", address.to_ascii_lowercase())
}

impl Credits {
    pub async fn get_or_create_cart(&self, address: &str) -> Result<i64, ApiError> {
        let row = sqlx::query(
            "INSERT INTO credits.carts (address) VALUES ($1) \
             ON CONFLICT (address) DO UPDATE SET updated_at = timezone('UTC', now()) \
             RETURNING id",
        )
        .bind(address)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("id"))
    }

    pub async fn add_item(
        &self,
        line: CartLine<'_>,
        unit_price_credits: &str,
    ) -> Result<(), ApiError> {
        sqlx::query(
            "WITH c AS ( \
                 INSERT INTO credits.carts (address) VALUES ($1) \
                 ON CONFLICT (address) DO UPDATE SET updated_at = timezone('UTC', now()) \
                 RETURNING id \
             ) \
             INSERT INTO credits.cart_items \
                 (cart_id, item_id, collection, urn, category, qty, unit_price_credits) \
             SELECT c.id, $2::text, $3::text, $4::text, $5::text, $6::int, $7::numeric FROM c \
             ON CONFLICT (cart_id, collection, item_id) DO UPDATE \
                 SET qty = EXCLUDED.qty, \
                     urn = EXCLUDED.urn, \
                     category = EXCLUDED.category, \
                     unit_price_credits = EXCLUDED.unit_price_credits",
        )
        .bind(line.address)
        .bind(line.item_id)
        .bind(line.collection)
        .bind(line.urn)
        .bind(line.category)
        .bind(line.qty)
        .bind(unit_price_credits)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// [`Self::add_item`] with the unit price computed in the same statement
    /// (the `ceil(...)` of `Pricing::compute_credit_price`, byte-identical).
    /// The line is only written when the charge covers a positive payment;
    /// the computed price is returned either way so the caller can raise the
    /// same conflict `ensure_charge_covers_payment` would.
    pub async fn add_item_priced(
        &self,
        line: CartLine<'_>,
        basis_wei: &str,
        mana_usd: &str,
        markup_bps: i64,
        payment_positive: bool,
    ) -> Result<String, ApiError> {
        let row = sqlx::query(
            "WITH p AS ( \
                 SELECT ceil( \
                     ($7::numeric / 1e18) * $8::numeric \
                     * (1 + ($9::numeric / 10000)) \
                     / $10::numeric \
                 ) AS credit_price \
             ), c AS ( \
                 INSERT INTO credits.carts (address) VALUES ($1) \
                 ON CONFLICT (address) DO UPDATE SET updated_at = timezone('UTC', now()) \
                 RETURNING id \
             ), i AS ( \
                 INSERT INTO credits.cart_items \
                     (cart_id, item_id, collection, urn, category, qty, unit_price_credits) \
                 SELECT c.id, $2::text, $3::text, $4::text, $5::text, $6::int, p.credit_price \
                 FROM c, p \
                 WHERE NOT ($11::bool AND NOT (p.credit_price > 0)) \
                 ON CONFLICT (cart_id, collection, item_id) DO UPDATE \
                     SET qty = EXCLUDED.qty, \
                         urn = EXCLUDED.urn, \
                         category = EXCLUDED.category, \
                         unit_price_credits = EXCLUDED.unit_price_credits \
                 RETURNING 1 \
             ) \
             SELECT p.credit_price::text AS credit_price FROM p",
        )
        .bind(line.address)
        .bind(line.item_id)
        .bind(line.collection)
        .bind(line.urn)
        .bind(line.category)
        .bind(line.qty)
        .bind(basis_wei)
        .bind(mana_usd)
        .bind(markup_bps)
        .bind(super::pricing::CREDIT_USD)
        .bind(payment_positive)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<String, _>("credit_price"))
    }

    /// Deletes the line and returns the remaining cart in the same statement.
    pub async fn remove_item(
        &self,
        address: &str,
        collection: &str,
        item_id: &str,
    ) -> Result<CartView, ApiError> {
        let rows = sqlx::query(
            "WITH d AS ( \
                 DELETE FROM credits.cart_items ci \
                 USING credits.carts c \
                 WHERE ci.cart_id = c.id AND c.address = $1 \
                   AND ci.item_id = $2 AND ci.collection = $3 \
                 RETURNING ci.id \
             ) \
             SELECT ci.item_id, ci.collection, ci.urn, ci.category, ci.qty, \
                    ci.unit_price_credits::text AS unit_price_credits, \
                    COALESCE(SUM(ci.unit_price_credits * ci.qty) OVER (), 0)::text AS total \
             FROM credits.cart_items ci \
             JOIN credits.carts c ON c.id = ci.cart_id \
             WHERE c.address = $1 AND ci.id NOT IN (SELECT id FROM d) \
             ORDER BY ci.added_at, ci.id",
        )
        .bind(address)
        .bind(item_id)
        .bind(collection)
        .fetch_all(&self.pool)
        .await?;
        Ok(cart_view_from_rows(rows))
    }

    pub async fn get_cart(&self, address: &str) -> Result<CartView, ApiError> {
        let rows = sqlx::query(
            "SELECT ci.item_id, ci.collection, ci.urn, ci.category, ci.qty, \
                    ci.unit_price_credits::text AS unit_price_credits, \
                    COALESCE(SUM(ci.unit_price_credits * ci.qty) OVER (), 0)::text AS total \
             FROM credits.cart_items ci \
             JOIN credits.carts c ON c.id = ci.cart_id \
             WHERE c.address = $1 \
             ORDER BY ci.added_at, ci.id",
        )
        .bind(address)
        .fetch_all(&self.pool)
        .await?;
        Ok(cart_view_from_rows(rows))
    }

    pub async fn get_checkout(&self, id: i64) -> Result<Option<CheckoutRow>, ApiError> {
        let row = sqlx::query("SELECT id, address, total_credits::text AS total_credits, status FROM credits.checkouts WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| CheckoutRow {
            id: r.get("id"),
            address: r.get("address"),
            total_credits: r.get("total_credits"),
            status: r.get("status"),
        }))
    }

    pub async fn refund_checkout_manual(
        &self,
        id: i64,
        address: &str,
        amount: &str,
    ) -> Result<(GrantOutcome, bool), ApiError> {
        let tx_ref = format!("checkout:{}", id);
        let idem = format!("admin:refund:{}", id);
        let mut tx = self.pool.begin().await?;
        let outcome = self
            .refund_in_tx(&mut tx, address, amount, &tx_ref, Some(&idem))
            .await?;
        let closed = sqlx::query(
            "UPDATE credits.checkouts SET status = 'failed', updated_at = timezone('UTC', now()) \
             WHERE id = $1 AND status = 'fulfilling'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        Ok((outcome, closed))
    }

    /// [`Self::refund_checkout_manual`] with the operator's audit row written
    /// by the statement that closes the checkout; SQL stamps `closed` into `detail`.
    pub async fn refund_checkout_manual_audited(
        &self,
        id: i64,
        address: &str,
        amount: &str,
        actor: &str,
        detail: impl FnOnce(&GrantOutcome) -> JsonValue,
    ) -> Result<(GrantOutcome, bool), ApiError> {
        let tx_ref = format!("checkout:{}", id);
        let idem = format!("admin:refund:{}", id);
        let mut tx = self.pool.begin().await?;
        let outcome = self
            .refund_in_tx(&mut tx, address, amount, &tx_ref, Some(&idem))
            .await?;
        let detail = detail(&outcome);
        let closed: bool = sqlx::query_scalar(
            "WITH u AS ( \
                 UPDATE credits.checkouts SET status = 'failed', updated_at = timezone('UTC', now()) \
                 WHERE id = $1 AND status = 'fulfilling' RETURNING 1 \
             ), a AS ( \
                 INSERT INTO ops.admin_audit (actor, domain, action, target, detail, result) \
                 SELECT $3, 'credits', 'checkout.refund', $2, \
                        $4::jsonb || jsonb_build_object('closed', EXISTS (SELECT 1 FROM u)), 'ok' \
             ) \
             SELECT EXISTS (SELECT 1 FROM u)",
        )
        .bind(id)
        .bind(id.to_string())
        .bind(actor)
        .bind(&detail)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((outcome, closed))
    }

    /// `address`'s checkout under its `Idempotency-Key`: the wallet-scoped key,
    /// or an unscoped key stored before keys were scoped, if it is the same wallet's.
    pub async fn find_checkout_by_idempotency_key(
        &self,
        address: &str,
        idempotency_key: &str,
    ) -> Result<Option<CheckoutIdemRow>, ApiError> {
        let row = sqlx::query(
            "SELECT id, address, status FROM credits.checkouts \
             WHERE idempotency_key IN ($1, $2) AND lower(address) = lower($3) \
             ORDER BY id LIMIT 1",
        )
        .bind(scoped_checkout_key(address, idempotency_key))
        .bind(idempotency_key)
        .bind(address)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| CheckoutIdemRow {
            id: r.get("id"),
            address: r.get("address"),
            status: r.get("status"),
        }))
    }

    /// The checkout transaction: claim the key, total the lines in SQL, debit
    /// once (a shortfall records the checkout as `failed`, the cart untouched),
    /// then expand the outbox, empty the cart lines and flip to `fulfilling`
    /// in one statement.
    #[tracing::instrument(skip_all, fields(address = %address, checkout_id = tracing::field::Empty))]
    pub async fn run_checkout(
        &self,
        address: &str,
        idempotency_key: &str,
        repriced: &[RepricedLine],
    ) -> Result<CheckoutOutcome, ApiError> {
        if repriced.is_empty() {
            return Err(ApiError::bad_request("cart is empty"));
        }
        let bare = idempotency_key;
        let scoped = scoped_checkout_key(address, idempotency_key);
        let idempotency_key = scoped.as_str();

        let mut tx = self.pool.begin().await?;

        // A node still storing the unscoped key may have taken this checkout.
        if let Some(prior) = sqlx::query(
            "SELECT id, status FROM credits.checkouts \
             WHERE idempotency_key = $1 AND lower(address) = lower($2)",
        )
        .bind(bare)
        .bind(address)
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(CheckoutOutcome {
                id: prior.get("id"),
                status: prior.get("status"),
                replayed: true,
            });
        }

        let claimed = sqlx::query(
            "INSERT INTO credits.checkouts (idempotency_key, address, status) \
             VALUES ($1, $2, 'reserving') \
             ON CONFLICT (idempotency_key) DO NOTHING \
             RETURNING id, (SELECT c.id FROM credits.carts c WHERE c.address = $2) AS cart_id",
        )
        .bind(idempotency_key)
        .bind(address)
        .fetch_optional(&mut *tx)
        .await?;

        let (checkout_id, cart_id): (i64, Option<i64>) = match claimed {
            Some(r) => (r.get("id"), r.get("cart_id")),
            None => {
                // The scoped key names this wallet, so the row is its own.
                let prior = sqlx::query(
                    "SELECT id, status FROM credits.checkouts WHERE idempotency_key = $1",
                )
                .bind(idempotency_key)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(CheckoutOutcome {
                    id: prior.get("id"),
                    status: prior.get("status"),
                    replayed: true,
                });
            }
        };
        tracing::Span::current().record("checkout_id", checkout_id);

        let Some(cart_id) = cart_id else {
            sqlx::query("UPDATE credits.checkouts SET status = 'failed', updated_at = timezone('UTC', now()) WHERE id = $1")
                .bind(checkout_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(ApiError::bad_request("cart is empty"));
        };

        let item_ids: Vec<String> = repriced.iter().map(|l| l.item_id.clone()).collect();
        let collections: Vec<String> = repriced
            .iter()
            .map(|l| l.collection.to_ascii_lowercase())
            .collect();
        let urns: Vec<String> = repriced.iter().map(|l| l.urn.clone()).collect();
        let token_ids: Vec<Option<String>> = repriced.iter().map(|l| l.token_id.clone()).collect();
        let trade_ids: Vec<Option<String>> = repriced.iter().map(|l| l.trade_id.clone()).collect();
        let basis_weis: Vec<Option<String>> =
            repriced.iter().map(|l| l.basis_wei.clone()).collect();
        let prices: Vec<String> = repriced
            .iter()
            .map(|l| l.unit_price_credits.clone())
            .collect();
        let qtys: Vec<i32> = repriced.iter().map(|l| l.qty).collect();
        let modes: Vec<String> = repriced.iter().map(|l| l.mode.clone()).collect();

        let total: String = sqlx::query(
            "UPDATE credits.checkouts \
             SET total_credits = (SELECT COALESCE(SUM(p::numeric * q), 0) \
                                  FROM unnest($2::text[], $3::int[]) AS t(p, q)), \
                 updated_at = timezone('UTC', now()) \
             WHERE id = $1 \
             RETURNING total_credits::text AS total",
        )
        .bind(checkout_id)
        .bind(&prices)
        .bind(&qtys)
        .fetch_one(&mut *tx)
        .await?
        .get("total");

        let checkout_ref = format!("checkout:{}", checkout_id);
        let ledger_id = match self
            .spend_in_tx_with_ledger(
                &mut tx,
                address,
                &total,
                &checkout_ref,
                Some(idempotency_key),
            )
            .await
        {
            Ok((_, ledger_id)) => ledger_id,
            Err(ApiError::PaymentRequired(_)) => {
                sqlx::query("UPDATE credits.checkouts SET status = 'failed', updated_at = timezone('UTC', now()) WHERE id = $1")
                    .bind(checkout_id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Err(ApiError::payment_required("insufficient credits balance"));
            }
            Err(e) => return Err(e),
        };

        sqlx::query(
            "WITH o AS ( \
                 INSERT INTO credits.fulfillment_outbox \
                     (checkout_id, item_id, collection, urn, token_id, trade_id, basis_wei, unit_price_credits, mode, status) \
                 SELECT $1, t.item_id, t.collection, t.urn, t.token_id, t.trade_id, t.basis_wei::numeric, t.price::numeric, t.mode, 'pending' \
                 FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::text[], $9::int[], $10::text[]) \
                          AS t(item_id, collection, urn, token_id, trade_id, basis_wei, price, qty, mode) \
                 CROSS JOIN LATERAL generate_series(1, t.qty) AS g \
             ), d AS ( \
                 DELETE FROM credits.cart_items ci \
                 USING unnest($3::text[], $2::text[]) AS t(collection, item_id) \
                 WHERE ci.cart_id = $11 \
                   AND ci.item_id = t.item_id \
                   AND ci.collection = t.collection \
             ) \
             UPDATE credits.checkouts SET status = 'fulfilling', ledger_id = $12, updated_at = timezone('UTC', now()) \
             WHERE id = $1",
        )
        .bind(checkout_id)
        .bind(&item_ids)
        .bind(&collections)
        .bind(&urns)
        .bind(&token_ids)
        .bind(&trade_ids)
        .bind(&basis_weis)
        .bind(&prices)
        .bind(&qtys)
        .bind(&modes)
        .bind(cart_id)
        .bind(ledger_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        self.kick_fulfillment();

        Ok(CheckoutOutcome {
            id: checkout_id,
            status: "fulfilling".to_string(),
            replayed: false,
        })
    }
}

fn cart_view_from_rows(rows: Vec<sqlx::postgres::PgRow>) -> CartView {
    let total: String = rows
        .first()
        .map(|r| r.get("total"))
        .unwrap_or_else(|| "0".to_string());
    let items: Vec<CartItemRow> = rows
        .into_iter()
        .map(|r| CartItemRow {
            item_id: r.get("item_id"),
            collection: r.get("collection"),
            urn: r.get("urn"),
            category: r.get("category"),
            qty: r.get("qty"),
            unit_price_credits: r.get("unit_price_credits"),
        })
        .collect();
    CartView {
        items,
        total_credits: total,
    }
}
