//! The operator's money and catalogue writes and their listings: grants
//! (also the path every purchase grant takes, `kind` = `purchase`), revokes,
//! the claim block, the pack catalogue, the purchase/checkout/ledger pages,
//! the force-fulfil re-arm and the escrow-lease lookups over
//! `shop.usage_grants` (the old side pool, a local table now). Every
//! mutation writes its `ops.admin_audit` row in the same transaction.

use serde_json::Value as JsonValue;
use sqlx::Row;

use super::errors::ApiError;
use super::store::{AuditEntry, Credits, now};
use super::wallet::{GrantOutcome, LEDGER_SPLIT_INSERT_WITH_AUDIT};

#[derive(Debug, Clone)]
pub struct GrantReplayRow {
    pub address: String,
    pub amount: String,
    pub available: String,
}

#[derive(Debug, Clone)]
pub struct PackAdminRow {
    pub sku: String,
    pub title: String,
    pub credits: String,
    pub price_cents: i64,
    pub currency: String,
    pub active: bool,
    pub sort_order: i32,
}

#[derive(Debug, Clone)]
pub struct PurchaseAdminRow {
    pub id: i64,
    pub address: String,
    pub sku: String,
    pub credits: String,
    pub amount_cents: i64,
    pub currency: String,
    pub stripe_payment_intent: Option<String>,
    pub method: String,
    pub status: String,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OutboxLineRow {
    pub id: i64,
    pub item_id: String,
    pub urn: String,
    pub token_id: Option<String>,
    pub unit_price_credits: String,
    pub mode: String,
    pub status: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub external_ref: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CheckoutAdminRow {
    pub id: i64,
    pub address: String,
    pub total_credits: String,
    pub status: String,
    pub created_at: chrono::NaiveDateTime,
    pub lines: Vec<OutboxLineRow>,
}

#[derive(Debug, Clone)]
pub struct LedgerEntryRow {
    pub id: i64,
    pub address: String,
    pub kind: String,
    pub amount: String,
    pub tx_ref: Option<String>,
    pub created_at: chrono::NaiveDateTime,
}

pub struct WalletFlagRow {
    pub address: String,
    pub source: String,
    pub ip: Option<String>,
    pub linked_addresses: Vec<String>,
    pub flagged_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone)]
pub struct UsageGrantRow {
    pub grantee_address: String,
    pub urn: String,
    pub token_id: Option<String>,
    pub collection: Option<String>,
    pub status: String,
    pub unlock_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Copy)]
pub struct GrantRequest<'a> {
    pub address: &'a str,
    pub amount: &'a str,
    pub kind: &'a str,
    pub reason: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
    pub detail: &'a JsonValue,
    /// The studio giving an operator grant: its notice reads "A gift from {studio}".
    pub studio_name: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct PackSpec<'a> {
    pub sku: &'a str,
    pub title: &'a str,
    pub credits: &'a str,
    pub price_cents: i64,
    pub currency: &'a str,
    pub active: bool,
    pub sort_order: i32,
}

fn with_fields(detail: &JsonValue, fields: &[(&str, JsonValue)]) -> JsonValue {
    let mut detail = detail.clone();
    if let JsonValue::Object(map) = &mut detail {
        for (k, v) in fields {
            map.insert((*k).to_string(), v.clone());
        }
    }
    detail
}

const CREDITS_GRANT_NS: uuid::Uuid =
    uuid::Uuid::from_u128(0x8d2f_41c7_5a90_4e3b_b7d6_0c1e_9a4f_62d8);
/// The old processor's `${CDN_URL}credits/notification-icon.png`.
pub(crate) const CREDITS_NOTIFICATION_ICON_PATH: &str = "credits/notification-icon.png";

/// The longest studio name, as credits-server's studio admin allows.
pub const MAX_STUDIO_NAME_CHARS: usize = 80;

/// The credits-server USD `ON_DEMAND_CREDITS_GRANTED` event as the old
/// parser stored it, keyed on the grant's audit row.
struct GrantNotice<'a> {
    pub address: catalyst_social::Address,
    pub audit_id: i64,
    pub amount: &'a str,
    pub usd_cents: Option<i64>,
    pub studio_name: Option<&'a str>,
    pub shop_base_url: &'a str,
    pub cdn_url: &'a str,
}

fn credits_granted_notification(
    notice: GrantNotice<'_>,
) -> catalyst_social::notifications::NewNotification {
    let GrantNotice {
        address,
        audit_id,
        amount,
        usd_cents,
        studio_name,
        shop_base_url,
        cdn_url,
    } = notice;
    let credits = amount
        .parse::<serde_json::Number>()
        .map_or_else(|_| JsonValue::String(amount.into()), JsonValue::Number);
    let mut metadata = serde_json::json!({ "creditsGranted": credits, "denomination": "USD" });
    if let Some(cents) = usd_cents {
        metadata["usdCents"] = cents.into();
    }
    let studio = studio_name.map(str::trim).filter(|s| !s.is_empty());
    if let Some(studio) = studio {
        metadata["studioName"] = studio.into();
    }
    metadata["image"] =
        catalyst_social::notifications::cdn_asset(cdn_url, CREDITS_NOTIFICATION_ICON_PATH).into();
    let amount = format!("{} Credits", en_us_amount(amount));
    let (title, description) = match studio {
        Some(studio) => (
            format!("A gift from {studio}"),
            format!("{studio} gifted you {amount}. They never expire: spend them in the Shop."),
        ),
        None => (
            "Credits added to your account".to_string(),
            format!("You received {amount}. They never expire: spend them in the Shop."),
        ),
    };
    metadata["title"] = title.into();
    metadata["description"] = description.into();
    metadata["link"] = shop_base_url.into();
    let mut n = catalyst_social::notifications::NewNotification::now(
        address,
        "credits_on_demand_granted",
        metadata,
    );
    n.id = uuid::Uuid::new_v5(
        &CREDITS_GRANT_NS,
        format!("credits-grant:{audit_id}").as_bytes(),
    );
    n
}

/// `Number(x).toLocaleString('en-US')` on a plain decimal: grouped
/// thousands, at most three fraction digits (truncated).
fn en_us_amount(amount: &str) -> String {
    let (int, frac) = amount.split_once('.').unwrap_or((amount, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return amount.to_string();
    }
    let int = match int.trim_start_matches('0') {
        "" => "0",
        t => t,
    };
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    let frac = frac
        .get(..frac.len().min(3))
        .unwrap_or("")
        .trim_end_matches('0');
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

impl Credits {
    pub async fn find_grant_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<GrantReplayRow>, ApiError> {
        let row = sqlx::query(
            "SELECT address, amount::text AS amount, available::text AS available \
             FROM credits.idempotency WHERE scope = 'grant' AND idempotency_key = $1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| GrantReplayRow {
            address: r.get("address"),
            amount: r.get("amount"),
            available: r.get("available"),
        }))
    }

    pub async fn admin_grant_credits(
        &self,
        grant: GrantRequest<'_>,
    ) -> Result<GrantOutcome, ApiError> {
        let mut tx = self.pool.begin().await?;
        let outcome = self.admin_grant_credits_in_tx(&mut tx, grant).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// A paid-bucket credit: an operator grant, a Stripe purchase, a MANA
    /// top-up or a mock. The wallet row is upserted (an unenrolled wallet
    /// stays `has_started_program = false`), the ledger row names the audit
    /// row, and the idempotency snapshot lands in the same statement.
    pub(crate) async fn admin_grant_credits_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        grant: GrantRequest<'_>,
    ) -> Result<GrantOutcome, ApiError> {
        let GrantRequest {
            address,
            amount,
            kind,
            reason,
            actor,
            idempotency_key,
            detail,
            studio_name,
        } = grant;
        if !matches!(kind, "grant" | "purchase" | "refund") {
            return Err(ApiError::bad_request(
                "admin_grant_credits kind must be one of grant|purchase|refund",
            ));
        }
        let amount = super::money::CreditAmount::parse_positive(amount)?;
        let amount = amount.as_str();
        let actor = actor.unwrap_or(super::store::SYSTEM_ACTOR);

        if let Some(key) = idempotency_key {
            let claimed = sqlx::query(
                "INSERT INTO credits.idempotency \
                     (scope, idempotency_key, address, amount, available, actor) \
                 VALUES ('grant', $1, $2, $3::numeric, 0, $4) \
                 ON CONFLICT (scope, idempotency_key) DO NOTHING \
                 RETURNING idempotency_key",
            )
            .bind(key)
            .bind(address)
            .bind(amount)
            .bind(actor)
            .fetch_optional(&mut **tx)
            .await?
            .is_some();

            if !claimed {
                let prior = sqlx::query(
                    "SELECT available::text AS available, amount::text AS amount, \
                            (lower(address) = lower($2)) AS addr_match, \
                            (amount = $3::numeric) AS amount_match \
                     FROM credits.idempotency WHERE scope = 'grant' AND idempotency_key = $1",
                )
                .bind(key)
                .bind(address)
                .bind(amount)
                .fetch_one(&mut **tx)
                .await?;
                let addr_match: bool = prior.get("addr_match");
                let amount_match: bool = prior.get("amount_match");
                if !addr_match || !amount_match {
                    return Err(ApiError::conflict(
                        "idempotency key already used for a different grant (address/amount mismatch)",
                    ));
                }
                return Ok(GrantOutcome {
                    available: prior.get("available"),
                    applied: prior.get("amount"),
                    replayed: true,
                });
            }
        }

        let row = sqlx::query(
            "INSERT INTO credits.wallets (address, available, has_started_program, updated_at) \
             VALUES ($1, $2::numeric, FALSE, timezone('UTC', now())) \
             ON CONFLICT (address) DO UPDATE \
                 SET available = credits.wallets.available + $2::numeric, updated_at = timezone('UTC', now()) \
             RETURNING available::text AS available",
        )
        .bind(address)
        .bind(amount)
        .fetch_one(&mut **tx)
        .await?;
        let available: String = row.get("available");

        let detail = with_fields(
            detail,
            &[
                ("amount", JsonValue::String(amount.to_string())),
                ("reason", serde_json::json!(reason)),
                ("kind", JsonValue::String(kind.to_string())),
            ],
        );
        let (audit_id, usd_cents): (i64, Option<i64>) = sqlx::query_as(
            "WITH a AS ( \
                 INSERT INTO ops.admin_audit (actor, domain, action, target, detail, result) \
                 VALUES ($5, 'credits', 'grant', $1, $6, 'ok') RETURNING id \
             ), l AS ( \
                 INSERT INTO credits.ledger (address, kind, amount, bucket, captcha_ok, admin_audit_id) \
                 SELECT $1, $3, $2::numeric, 'paid', FALSE, a.id FROM a \
             ), i AS ( \
                 UPDATE credits.idempotency \
                 SET available = $4::numeric, applied = $2::numeric \
                 WHERE scope = 'grant' AND idempotency_key = $7 \
             ) \
             SELECT a.id, CASE WHEN c = trunc(c) AND c < 9e18 THEN c::int8 END \
             FROM a, (SELECT $2::numeric * 10 AS c) cents",
        )
        .bind(address)
        .bind(amount)
        .bind(kind)
        .bind(&available)
        .bind(actor)
        .bind(&detail)
        .bind(idempotency_key)
        .fetch_one(&mut **tx)
        .await?;

        // Only an operator grant notifies; purchases and refunds do not.
        if kind == "grant"
            && let Ok(recipient) = catalyst_social::Address::parse(address)
        {
            let notification = credits_granted_notification(GrantNotice {
                address: recipient,
                audit_id,
                amount,
                usd_cents,
                studio_name,
                shop_base_url: &self.shop_base_url,
                cdn_url: &self.notifications_cdn_url,
            });
            catalyst_social::notifications::insert_notifications(&mut **tx, &[notification])
                .await?;
        }

        Ok(GrantOutcome {
            available,
            applied: amount.to_string(),
            replayed: false,
        })
    }

    pub async fn admin_revoke_credits(
        &self,
        address: &str,
        amount: &str,
        reason: Option<&str>,
        actor: Option<&str>,
        detail: &JsonValue,
    ) -> Result<GrantOutcome, ApiError> {
        let amount = super::money::CreditAmount::parse_positive(amount)?;
        let amount = amount.as_str();
        let mut tx = self.pool.begin().await?;

        let current = sqlx::query(
            "SELECT available::text AS available, earned_available::text AS earned \
             FROM credits.wallets WHERE address = $1 FOR UPDATE",
        )
        .bind(address)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(current) = current else {
            tx.rollback().await?;
            return Err(ApiError::not_found("user has no credits balance"));
        };
        let available_before: String = current.get("available");
        let earned_before: String = current.get("earned");

        let row = sqlx::query(
            "UPDATE credits.wallets \
             SET available = GREATEST(available - $2::numeric, 0), \
                 earned_available = LEAST(earned_available, GREATEST(available - $2::numeric, 0)), \
                 updated_at = timezone('UTC', now()) \
             WHERE address = $1 \
             RETURNING available::text AS available, \
                       ($3::numeric - GREATEST($3::numeric - $2::numeric, 0))::text AS removed, \
                       ($4::numeric - earned_available)::text AS earned_removed, \
                       GREATEST(($3::numeric - GREATEST($3::numeric - $2::numeric, 0)) \
                                - ($4::numeric - earned_available), 0)::text AS paid_removed",
        )
        .bind(address)
        .bind(amount)
        .bind(&available_before)
        .bind(&earned_before)
        .fetch_one(&mut *tx)
        .await?;
        let available_after: String = row.get("available");
        let removed: String = row.get("removed");
        let earned_removed: String = row.get("earned_removed");
        let paid_removed: String = row.get("paid_removed");

        let detail = with_fields(
            detail,
            &[
                ("requested", JsonValue::String(amount.to_string())),
                ("removed", JsonValue::String(removed.clone())),
                ("amount", JsonValue::String(removed.clone())),
                ("reason", serde_json::json!(reason)),
            ],
        );
        sqlx::query(LEDGER_SPLIT_INSERT_WITH_AUDIT)
            .bind(address)
            .bind(&earned_removed)
            .bind(&paid_removed)
            .bind(None::<&str>)
            .bind("consume")
            .bind("revoke")
            .bind(actor.unwrap_or(super::store::SYSTEM_ACTOR))
            .bind(&detail)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(GrantOutcome {
            available: available_after,
            applied: removed,
            replayed: false,
        })
    }

    /// `user.block` and `user.unblock`. An unblock also deletes the wallet's
    /// multi-accounting flag (`credits.wallet_flags`, in the same
    /// transaction), so the claim opens again.
    pub async fn admin_set_blocked(
        &self,
        address: &str,
        blocked: bool,
        reason: Option<&str>,
        actor: Option<&str>,
        detail: &JsonValue,
    ) -> Result<bool, ApiError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO credits.wallets (address, is_blocked_for_claiming, has_started_program, updated_at) \
             VALUES ($1, $2, FALSE, timezone('UTC', now())) \
             ON CONFLICT (address) DO UPDATE \
                 SET is_blocked_for_claiming = $2, updated_at = timezone('UTC', now())",
        )
        .bind(address)
        .bind(blocked)
        .execute(&mut *tx)
        .await?;
        if !blocked {
            sqlx::query("DELETE FROM credits.wallet_flags WHERE address = $1")
                .bind(address)
                .execute(&mut *tx)
                .await?;
        }

        let action = if blocked {
            "user.block"
        } else {
            "user.unblock"
        };
        let detail = with_fields(detail, &[("reason", serde_json::json!(reason))]);
        Self::audit(
            &mut *tx,
            AuditEntry {
                action,
                actor: actor.unwrap_or(super::store::SYSTEM_ACTOR),
                target: Some(address),
                detail: &detail,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(blocked)
    }

    /// `user.flag`: one `credits.wallet_flags` row per address, the body's
    /// addresses as every row's linked set; a wallet already flagged keeps
    /// its first flag's time and takes the new source, IP and links. One
    /// audit row per wallet, in the flag's transaction.
    pub async fn admin_flag_wallets(
        &self,
        addresses: &[String],
        source: &str,
        ip: Option<&str>,
        actor: &str,
        detail: &JsonValue,
    ) -> Result<(), ApiError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO credits.wallet_flags (address, source, ip, linked_addresses) \
             SELECT unnest($1::text[]), $2, $3, $1 \
             ON CONFLICT (address) DO UPDATE \
                 SET source = EXCLUDED.source, ip = EXCLUDED.ip, \
                     linked_addresses = EXCLUDED.linked_addresses",
        )
        .bind(addresses)
        .bind(source)
        .bind(ip)
        .execute(&mut *tx)
        .await?;
        for address in addresses {
            Self::audit(
                &mut *tx,
                AuditEntry {
                    action: "user.flag",
                    actor,
                    target: Some(address),
                    detail,
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// `user.unflag`: deletes the wallet's flag; `false` when it had none.
    pub async fn admin_unflag_wallet(
        &self,
        address: &str,
        actor: &str,
        detail: &JsonValue,
    ) -> Result<bool, ApiError> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM credits.wallet_flags WHERE address = $1")
            .bind(address)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        Self::audit(
            &mut *tx,
            AuditEntry {
                action: "user.unflag",
                actor,
                target: Some(address),
                detail,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(deleted)
    }

    pub async fn admin_list_flags(
        &self,
        address: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<WalletFlagRow>, ApiError> {
        let rows = sqlx::query(
            "SELECT address, source, ip, linked_addresses, flagged_at FROM credits.wallet_flags \
             WHERE $1::text IS NULL OR address = $1 \
             ORDER BY flagged_at DESC, address LIMIT $2 OFFSET $3",
        )
        .bind(address)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| WalletFlagRow {
                address: r.get("address"),
                source: r.get("source"),
                ip: r.get("ip"),
                linked_addresses: r.get("linked_addresses"),
                flagged_at: r.get("flagged_at"),
            })
            .collect())
    }

    pub async fn admin_list_packs(&self) -> Result<Vec<PackAdminRow>, ApiError> {
        let rows = sqlx::query(
            "SELECT sku, title, credits::text AS credits, price_cents, currency, active, sort_order \
             FROM credits.packs ORDER BY sort_order, price_cents, sku",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_pack_admin).collect())
    }

    pub async fn admin_create_pack(
        &self,
        pack: PackSpec<'_>,
        actor: &str,
        detail: &JsonValue,
    ) -> Result<PackAdminRow, ApiError> {
        let PackSpec {
            sku,
            title,
            credits,
            price_cents,
            currency,
            active,
            sort_order,
        } = pack;
        let mut tx = self.pool.begin().await?;
        let exists = sqlx::query("SELECT 1 FROM credits.packs WHERE sku = $1")
            .bind(sku)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if exists {
            tx.rollback().await?;
            return Err(ApiError::conflict("pack sku already exists"));
        }
        let row = sqlx::query(
            "INSERT INTO credits.packs (sku, title, credits, price_cents, currency, active, sort_order) \
             VALUES ($1, $2, $3::numeric, $4, $5, $6, $7) \
             RETURNING sku, title, credits::text AS credits, price_cents, currency, active, sort_order",
        )
        .bind(sku)
        .bind(title)
        .bind(credits)
        .bind(price_cents)
        .bind(currency)
        .bind(active)
        .bind(sort_order)
        .fetch_one(&mut *tx)
        .await?;
        let pack = map_pack_admin(row);
        Self::audit(
            &mut *tx,
            AuditEntry {
                action: "pack.create",
                actor,
                target: Some(sku),
                detail,
            },
        )
        .await?;
        tx.commit().await?;
        self.invalidate_packs();
        Ok(pack)
    }

    pub async fn admin_update_pack(
        &self,
        pack: PackSpec<'_>,
        actor: &str,
        detail: &JsonValue,
    ) -> Result<PackAdminRow, ApiError> {
        let PackSpec {
            sku,
            title,
            credits,
            price_cents,
            currency,
            active,
            sort_order,
        } = pack;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "UPDATE credits.packs SET \
                 title = $2, credits = $3::numeric, price_cents = $4, currency = $5, active = $6, sort_order = $7 \
             WHERE sku = $1 \
             RETURNING sku, title, credits::text AS credits, price_cents, currency, active, sort_order",
        )
        .bind(sku)
        .bind(title)
        .bind(credits)
        .bind(price_cents)
        .bind(currency)
        .bind(active)
        .bind(sort_order)
        .fetch_optional(&mut *tx)
        .await?;
        let row = row.ok_or_else(|| ApiError::not_found("pack not found"))?;
        let pack = map_pack_admin(row);
        Self::audit(
            &mut *tx,
            AuditEntry {
                action: "pack.update",
                actor,
                target: Some(sku),
                detail,
            },
        )
        .await?;
        tx.commit().await?;
        self.invalidate_packs();
        Ok(pack)
    }

    pub async fn admin_delete_pack(
        &self,
        sku: &str,
        actor: &str,
        detail: &JsonValue,
    ) -> Result<(), ApiError> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query("DELETE FROM credits.packs WHERE sku = $1")
            .bind(sku)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(ApiError::not_found("pack not found"));
        }
        Self::audit(
            &mut *tx,
            AuditEntry {
                action: "pack.delete",
                actor,
                target: Some(sku),
                detail,
            },
        )
        .await?;
        tx.commit().await?;
        self.invalidate_packs();
        Ok(())
    }

    pub async fn admin_list_purchases(
        &self,
        status: Option<&str>,
        address: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PurchaseAdminRow>, ApiError> {
        let rows = sqlx::query(
            "SELECT id, address, sku, credits::text AS credits, amount_cents, currency, \
                    stripe_payment_intent, method, status, created_at \
             FROM credits.purchases \
             WHERE ($1::text IS NULL OR status = $1) \
               AND ($2::text IS NULL OR address = $2) \
             ORDER BY id DESC LIMIT $3 OFFSET $4",
        )
        .bind(status)
        .bind(address)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PurchaseAdminRow {
                id: r.get("id"),
                address: r.get("address"),
                sku: r.get("sku"),
                credits: r.get("credits"),
                amount_cents: r.get("amount_cents"),
                currency: r.get("currency"),
                stripe_payment_intent: r.get("stripe_payment_intent"),
                method: r.get("method"),
                status: r.get("status"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    /// The checkouts page with the outbox lines folded in (one statement).
    pub async fn admin_list_checkouts(
        &self,
        address: Option<&str>,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<CheckoutAdminRow>, ApiError> {
        let rows = sqlx::query(
            "SELECT c.id, c.address, c.total_credits::text AS total_credits, c.status, c.created_at, l.lines \
             FROM credits.checkouts c \
             LEFT JOIN LATERAL ( \
                 SELECT json_agg(json_build_object( \
                            'id', o.id, 'item_id', o.item_id, 'urn', o.urn, \
                            'token_id', o.token_id, \
                            'unit_price_credits', o.unit_price_credits::text, \
                            'mode', o.mode, 'status', o.status, 'attempts', o.attempts, \
                            'last_error', o.last_error, 'external_ref', o.external_ref) \
                        ORDER BY o.id) AS lines \
                 FROM credits.fulfillment_outbox o WHERE o.checkout_id = c.id \
             ) l ON TRUE \
             WHERE ($1::text IS NULL OR c.address = $1) \
               AND ($2::text IS NULL OR c.status = $2) \
             ORDER BY c.id DESC LIMIT $3 OFFSET $4",
        )
        .bind(address)
        .bind(status)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                let lines: Vec<OutboxLineRow> = match r.get::<Option<JsonValue>, _>("lines") {
                    Some(v) => serde_json::from_value(v)
                        .map_err(|e| ApiError::Internal(format!("outbox lines decode: {e}")))?,
                    None => Vec::new(),
                };
                Ok(CheckoutAdminRow {
                    id: r.get("id"),
                    address: r.get("address"),
                    total_credits: r.get("total_credits"),
                    status: r.get("status"),
                    created_at: r.get("created_at"),
                    lines,
                })
            })
            .collect()
    }

    pub async fn admin_list_ledger(
        &self,
        address: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<LedgerEntryRow>, ApiError> {
        let rows = sqlx::query(
            "SELECT id, address, kind, amount::text AS amount, tx_ref, created_at \
             FROM credits.ledger WHERE address = $1 \
             ORDER BY id DESC LIMIT $2 OFFSET $3",
        )
        .bind(address)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| LedgerEntryRow {
                id: r.get("id"),
                address: r.get("address"),
                kind: r.get("kind"),
                amount: r.get("amount"),
                tx_ref: r.get("tx_ref"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    pub async fn admin_audit_op(
        &self,
        action: &str,
        target: Option<&str>,
        actor: &str,
        detail: &JsonValue,
    ) -> Result<i64, ApiError> {
        Self::audit(
            &self.pool,
            AuditEntry {
                action,
                actor,
                target,
                detail,
            },
        )
        .await
    }

    pub async fn admin_force_fulfill(&self, checkout_id: i64) -> Result<u64, ApiError> {
        let mut tx = self.pool.begin().await?;
        let exists = sqlx::query("SELECT status FROM credits.checkouts WHERE id = $1 FOR UPDATE")
            .bind(checkout_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = exists else {
            tx.rollback().await?;
            return Err(ApiError::not_found("checkout not found"));
        };
        let status: String = row.get("status");
        if status != "fulfilling" {
            tx.rollback().await?;
            return Err(ApiError::conflict(
                "checkout is not in a re-armable 'fulfilling' state",
            ));
        }

        let res = sqlx::query(
            "UPDATE credits.fulfillment_outbox \
             SET status = 'pending', attempts = 0, last_error = NULL, updated_at = timezone('UTC', now()) \
             WHERE checkout_id = $1 AND status <> 'confirmed'",
        )
        .bind(checkout_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE credits.checkouts SET status = 'fulfilling', updated_at = timezone('UTC', now()) WHERE id = $1")
            .bind(checkout_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.kick_fulfillment();
        Ok(res.rows_affected())
    }

    pub async fn find_confirmed_line_by_ref(
        &self,
        escrow_ref: &str,
    ) -> Result<Option<(i64, String, String)>, ApiError> {
        let row = sqlx::query(
            "SELECT c.id AS checkout_id, c.address AS address, o.unit_price_credits::text AS amount \
             FROM credits.fulfillment_outbox o \
             JOIN credits.checkouts c ON c.id = o.checkout_id \
             WHERE o.external_ref = $1 AND o.status = 'confirmed' \
             ORDER BY o.id DESC LIMIT 1",
        )
        .bind(escrow_ref)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            (
                r.get::<i64, _>("checkout_id"),
                r.get::<String, _>("address"),
                r.get::<String, _>("amount"),
            )
        }))
    }

    pub async fn revoke_usage_grant(&self, escrow_ref: &str) -> Result<u64, ApiError> {
        let res = sqlx::query("UPDATE shop.usage_grants SET status = 'revoked' WHERE escrow_ref = $1 AND status = 'active'")
            .bind(escrow_ref)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    pub async fn fetch_usage_grant(
        &self,
        escrow_ref: &str,
    ) -> Result<Option<UsageGrantRow>, ApiError> {
        let row = sqlx::query(
            "SELECT grantee_address, urn, token_id, collection, status, unlock_at \
             FROM shop.usage_grants WHERE escrow_ref = $1 \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(escrow_ref)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| UsageGrantRow {
            grantee_address: r.get("grantee_address"),
            urn: r.get("urn"),
            token_id: r.get("token_id"),
            collection: r.get("collection"),
            status: r.get("status"),
            unlock_at: r.get("unlock_at"),
        }))
    }

    pub async fn mark_usage_grant_released(&self, escrow_ref: &str) -> Result<u64, ApiError> {
        let res = sqlx::query("UPDATE shop.usage_grants SET status = 'released' WHERE escrow_ref = $1 AND status = 'active'")
            .bind(escrow_ref)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// The escrow lease the confirmed line represents, idempotent on
    /// `(escrow_ref, urn, token_id)`; a failure is logged, never fatal (the
    /// line is confirmed on-chain; the reconciler's escrow pass lists the gap).
    #[allow(
        clippy::too_many_arguments,
        reason = "one argument per shop.usage_grants column the checkout line supplies"
    )]
    pub(crate) async fn write_usage_grant(
        &self,
        buyer: &str,
        urn: &str,
        token_id: Option<&str>,
        category: &str,
        collection: &str,
        escrow_ref: &str,
        lock_days: i32,
    ) {
        let res = sqlx::query(
            "INSERT INTO shop.usage_grants \
                 (grantee_address, urn, token_id, category, collection, escrow_ref, unlock_at, status) \
             VALUES (lower($1), $2, $3, $4, $5, $6, $7 + make_interval(days => $8), 'active') \
             ON CONFLICT DO NOTHING",
        )
        .bind(buyer)
        .bind(urn)
        .bind(token_id)
        .bind(category)
        .bind(collection)
        .bind(escrow_ref)
        .bind(now())
        .bind(lock_days)
        .execute(&self.pool)
        .await;

        match res {
            Ok(_) => {
                tracing::info!(buyer = %buyer.to_lowercase(), urn = %urn, escrow_ref = %escrow_ref, "usage_grant written (escrow lease active)")
            }
            Err(e) => {
                tracing::error!(error = %e, urn = %urn, escrow_ref = %escrow_ref, "usage_grant write FAILED (line is confirmed; the reconciler's escrow pass lists it)")
            }
        }
    }
}

fn map_pack_admin(r: sqlx::postgres::PgRow) -> PackAdminRow {
    PackAdminRow {
        sku: r.get("sku"),
        title: r.get("title"),
        credits: r.get("credits"),
        price_cents: r.get("price_cents"),
        currency: r.get("currency"),
        active: r.get("active"),
        sort_order: r.get("sort_order"),
    }
}
