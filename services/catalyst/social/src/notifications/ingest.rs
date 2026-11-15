// from: notifications-workers@e2af2ee processor/src/controllers/handlers/publish-notification-handler.ts, processor/src/adapters/notifications-service.ts, common/src/adapters/db.ts (Apache-2.0)
//! `POST /notifications/notifications`: the external producers (governance,
//! rewards) publish already-mapped notifications. Each producer has its own
//! bearer and may only publish its own type prefix. Rows are keyed by a v5
//! id over `(type, address, eventKey)`; a repeat updates metadata and time,
//! keeps the read state and, as upstream, emails only on first insert. An
//! entry without `address` is a broadcast: one `broadcast_notifications` row
//! every wallet's inbox lists, with per-wallet `broadcast_reads`, never emailed.
//! Unlike upstream, only [`BROADCAST_TYPES`] may be address-less, timestamps
//! may not run ahead of the clock, and metadata is capped.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{NaiveDateTime, Utc};
use serde_json::{Map, Value as Json};
use sqlx::PgPool;
use uuid::Uuid;

use super::errors::{ApiError, NotificationsError, parse_json};
use super::store::{DELIVERY_ROWS_SQL, community_opt_out, ms_to_naive, uuid_or_null};
use super::types::is_known_type;
use crate::auth::admin_bearer::{bearer_token, timing_safe_eq};
use crate::identity::Address;

type AppState = Arc<super::State>;

/// Batches of many recipients (a proposal's voters) exceed the 256 KiB default.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

const INGEST_NS: Uuid = Uuid::from_bytes([
    0x3f, 0x8a, 0x52, 0xd4, 0x9c, 0x1e, 0x4b, 0x07, 0xa6, 0x2d, 0x71, 0xe0, 0x5c, 0x93, 0x18, 0xbb,
]);

/// Joi's `greater(1672531200000)` (2023-01-01T00:00:00Z).
const MIN_TIMESTAMP_MS: i64 = 1_672_531_200_000;
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;
const ADDRESS_PATTERN: &str = "/^0x[a-fA-F0-9]{40}$/";
const SCOPE_COMMUNITY: &str = "community";

/// The only address-less type a producer sends: governance's announcement
/// (`src/routes/notification.ts`, `address: undefined`); rewards sends none.
pub const BROADCAST_TYPES: [&str; 1] = ["governance_announcement"];
/// How far a timestamp may run ahead of the server clock.
pub const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;
/// Serialized `metadata` per item.
pub const MAX_METADATA_BYTES: usize = 16 * 1024;
/// Rows per INSERT statement and transaction.
const WRITE_CHUNK: usize = 1_000;

/// A producer and the type prefix its token may publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Producer {
    Governance,
    Rewards,
}

impl Producer {
    pub fn prefix(self) -> &'static str {
        match self {
            Producer::Governance => "governance_",
            Producer::Rewards => "reward_",
        }
    }

    pub fn allows(self, kind: &str) -> bool {
        kind.starts_with(self.prefix()) && is_known_type(kind)
    }
}

/// One validated entry of the body.
#[derive(Debug, Clone, PartialEq)]
pub struct Ingested {
    pub kind: String,
    pub address: Option<Address>,
    pub event_key: String,
    pub metadata: Json,
    pub occurred_at: NaiveDateTime,
    pub opt_out: Option<(String, String)>,
}

impl Ingested {
    /// v5 over the JSON `[type, address, eventKey]` (`address` null for a broadcast).
    pub fn id(&self) -> Uuid {
        let address = self.address.as_ref().map(Address::as_str);
        let identity = serde_json::to_vec(&(&self.kind, address, &self.event_key))
            .expect("the identity holds only strings");
        Uuid::new_v5(&INGEST_NS, &identity)
    }
}

/// The producer whose configured token the request presents.
fn authenticate(settings: &super::Settings, headers: &HeaderMap) -> Result<Producer, ApiError> {
    let Some(presented) = bearer_token(headers) else {
        return Err(if headers.contains_key("authorization") {
            ApiError::unauthorized("Invalid authorization header")
        } else {
            ApiError::unauthorized("Authorization header is missing")
        });
    };
    let matches = |token: &Option<String>| {
        token
            .as_deref()
            .filter(|t| !t.is_empty())
            .is_some_and(|t| timing_safe_eq(presented.as_bytes(), t.as_bytes()))
    };
    // Both compared so the answer's timing does not tell which producer exists.
    let governance = matches(&settings.governance_token);
    let rewards = matches(&settings.rewards_token);
    match (governance, rewards) {
        (true, _) => Ok(Producer::Governance),
        (false, true) => Ok(Producer::Rewards),
        _ => Err(ApiError::unauthorized("Invalid authorization header")),
    }
}

fn shown(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn string_field<'a>(
    obj: &'a Map<String, Json>,
    at: &str,
    key: &str,
) -> Result<Option<&'a str>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Json::String(s)) if s.is_empty() => {
            Err(format!("\"{at}.{key}\" is not allowed to be empty"))
        }
        Some(Json::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("\"{at}.{key}\" must be a string")),
    }
}

fn parse_opt_out(value: &Json, at: &str) -> Result<(String, String), String> {
    let at = format!("{at}.optOutScope");
    let Json::Object(obj) = value else {
        return Err(format!("\"{at}\" must be of type object"));
    };
    let scope = match obj.get("scope") {
        None => return Err(format!("\"{at}.scope\" is required")),
        Some(Json::String(s)) if s == SCOPE_COMMUNITY => s.clone(),
        Some(other) => return Err(format!("Invalid notification scope: {}", shown(other))),
    };
    let scope_id = string_field(obj, &at, "scopeId")?
        .ok_or_else(|| format!("\"{at}.scopeId\" is required"))?;
    if let Some(extra) = obj.keys().find(|k| *k != "scope" && *k != "scopeId") {
        return Err(format!("\"{at}.{extra}\" is not allowed"));
    }
    Ok((scope, scope_id.to_string()))
}

/// The upstream Joi schema plus this node's limits, first error only
/// (`abortEarly`), in Joi's words; `now_ms` bounds the timestamp.
pub fn validate(body: &Json, producer: Producer, now_ms: i64) -> Result<Vec<Ingested>, String> {
    let Json::Array(items) = body else {
        return Err("\"value\" must be an array".into());
    };
    let max_ts = now_ms.saturating_add(MAX_CLOCK_SKEW_MS);
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let at = format!("[{i}]");
        let Json::Object(obj) = item else {
            return Err(format!("\"{at}\" must be of type object"));
        };
        let kind = match obj.get("type") {
            None => return Err(format!("\"{at}.type\" is required")),
            Some(Json::String(s)) if producer.allows(s) => s.clone(),
            Some(other) => return Err(format!("Invalid notification type: {}", shown(other))),
        };
        let address = match obj.get("address") {
            None => None,
            Some(Json::String(s)) => {
                if !crate::auth::is_eth_address(s) {
                    return Err(format!(
                        "\"{at}.address\" with value \"{s}\" fails to match the required pattern: {ADDRESS_PATTERN}"
                    ));
                }
                Some(Address::parse(s).map_err(|e| e.to_string())?)
            }
            Some(_) => return Err(format!("\"{at}.address\" must be a string")),
        };
        if address.is_none() && !BROADCAST_TYPES.contains(&kind.as_str()) {
            return Err(format!("\"{at}.address\" is required"));
        }
        let event_key = string_field(obj, &at, "eventKey")?
            .ok_or_else(|| format!("\"{at}.eventKey\" is required"))?
            .to_string();
        let metadata = match obj.get("metadata") {
            None => return Err(format!("\"{at}.metadata\" is required")),
            Some(m @ Json::Object(_)) => m.clone(),
            Some(_) => return Err(format!("\"{at}.metadata\" must be of type object")),
        };
        if serde_json::to_vec(&metadata).map_or(0, |v| v.len()) > MAX_METADATA_BYTES {
            return Err(format!(
                "\"{at}.metadata\" must be less than or equal to {MAX_METADATA_BYTES} bytes"
            ));
        }
        let timestamp = match obj.get("timestamp") {
            None => return Err(format!("\"{at}.timestamp\" is required")),
            Some(Json::Number(n)) => match n.as_i64() {
                Some(ms) if ms.abs() > MAX_SAFE_INTEGER => {
                    return Err(format!("\"{at}.timestamp\" must be a safe number"));
                }
                Some(ms) if ms <= MIN_TIMESTAMP_MS => {
                    return Err(format!(
                        "\"{at}.timestamp\" must be greater than {MIN_TIMESTAMP_MS}"
                    ));
                }
                Some(ms) if ms > max_ts => {
                    return Err(format!(
                        "\"{at}.timestamp\" must be less than or equal to {max_ts}"
                    ));
                }
                Some(ms) => ms,
                None => return Err(format!("\"{at}.timestamp\" must be an integer")),
            },
            Some(_) => return Err(format!("\"{at}.timestamp\" must be a number")),
        };
        let occurred_at = ms_to_naive(timestamp)
            .ok_or_else(|| format!("\"{at}.timestamp\" must be a safe number"))?;
        // A broadcast has no recipient whose opt-outs could apply.
        if address.is_none() && obj.contains_key("optOutScope") {
            return Err(format!("\"{at}.optOutScope\" is not allowed"));
        }
        let opt_out = obj
            .get("optOutScope")
            .map(|v| parse_opt_out(v, &at))
            .transpose()?;
        const KEYS: [&str; 6] = [
            "type",
            "address",
            "eventKey",
            "metadata",
            "timestamp",
            "optOutScope",
        ];
        if let Some(extra) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(format!("\"{at}.{extra}\" is not allowed"));
        }
        out.push(Ingested {
            kind,
            address,
            event_key,
            metadata,
            occurred_at,
            opt_out,
        });
    }
    Ok(out)
}

/// One entry per key, the last occurrence winning as upstream's sequential
/// upserts did, in id order so overlapping batches lock rows alike.
pub(crate) fn dedup(batch: Vec<Ingested>) -> Vec<Ingested> {
    let mut at: HashMap<Uuid, usize> = HashMap::new();
    let mut out: Vec<Ingested> = Vec::with_capacity(batch.len());
    for n in batch {
        match at.get(&n.id()) {
            Some(&i) => out[i] = n,
            None => {
                at.insert(n.id(), out.len());
                out.push(n);
            }
        }
    }
    out.sort_by_cached_key(Ingested::id);
    out
}

/// Rows a batch wrote, as upstream logs them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Upserted {
    pub inserted: u64,
    pub updated: u64,
}

impl std::ops::AddAssign for Upserted {
    fn add_assign(&mut self, other: Self) {
        self.inserted += other.inserted;
        self.updated += other.updated;
    }
}

/// Upserts the batch one [`WRITE_CHUNK`] transaction at a time, so no row
/// stays locked for the whole body; a retry after a partial failure redoes
/// the rest idempotently. Each id lands in one chunk, so a new row gets its
/// email delivery row in its own chunk's transaction; opted-out recipients
/// are dropped.
pub async fn upsert(pool: &PgPool, batch: Vec<Ingested>) -> Result<Upserted, sqlx::Error> {
    let batch = dedup(batch);
    let (addressed, broadcast): (Vec<_>, Vec<_>) =
        batch.into_iter().partition(|n| n.address.is_some());
    let chunks = addressed.len().div_ceil(WRITE_CHUNK) + broadcast.len().div_ceil(WRITE_CHUNK);
    let mut written = Upserted::default();
    let mut committed = 0;
    let outcome = write_chunks(pool, &addressed, &broadcast, &mut written, &mut committed).await;
    if let Err(error) = &outcome {
        // The committed chunks stay; a retry counts their rows as updated.
        tracing::warn!(
            %error,
            inserted = written.inserted,
            updated = written.updated,
            committed_chunks = committed,
            total_chunks = chunks,
            "notification ingest stopped part-way"
        );
    }
    outcome.map(|()| written)
}

async fn write_chunks(
    pool: &PgPool,
    addressed: &[Ingested],
    broadcast: &[Ingested],
    written: &mut Upserted,
    committed: &mut usize,
) -> Result<(), sqlx::Error> {
    for chunk in addressed.chunks(WRITE_CHUNK) {
        let mut tx = pool.begin().await?;
        let chunk_written = upsert_addressed(&mut tx, chunk).await?;
        tx.commit().await?;
        *written += chunk_written;
        *committed += 1;
    }
    for chunk in broadcast.chunks(WRITE_CHUNK) {
        let mut tx = pool.begin().await?;
        let chunk_written = upsert_broadcasts(&mut tx, chunk).await?;
        tx.commit().await?;
        *written += chunk_written;
        *committed += 1;
    }
    Ok(())
}

async fn upsert_addressed(
    tx: &mut sqlx::PgConnection,
    addressed: &[Ingested],
) -> Result<Upserted, sqlx::Error> {
    let sql = format!(
        r#"
        WITH src AS (
            SELECT n.* FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::jsonb[],
                                   $5::timestamp[], $6::text[], $7::text[])
                AS n(id, address, type, metadata, occurred_at, scope, scope_id)
            WHERE NOT EXISTS (
                SELECT 1 FROM social.subscription_opt_outs o
                WHERE o.address = n.address AND o.scope = n.scope AND o.scope_id = n.scope_id
            )
            AND NOT (n.scope = 'community' AND EXISTS ({community_opt_out}))
        ),
        ups AS (
            INSERT INTO social.notifications AS t (id, address, type, metadata, occurred_at)
            SELECT id, address, type, metadata, occurred_at FROM src ORDER BY id
            ON CONFLICT (id) DO UPDATE
                SET metadata = EXCLUDED.metadata, occurred_at = EXCLUDED.occurred_at
                WHERE t.address = EXCLUDED.address AND t.type = EXCLUDED.type
            RETURNING id, address, type, (xmax = 0) AS inserted
        ),
        ins AS (SELECT id, address, type FROM ups WHERE inserted),
        delivered AS ({delivery})
        SELECT count(*) FILTER (WHERE inserted), count(*) FILTER (WHERE NOT inserted) FROM ups
        "#,
        delivery = *DELIVERY_ROWS_SQL,
        community_opt_out = community_opt_out("n.address", &uuid_or_null("n.scope_id")),
    );
    let ids: Vec<Uuid> = addressed.iter().map(Ingested::id).collect();
    let addresses: Vec<&str> = addressed
        .iter()
        .filter_map(|n| n.address.as_ref().map(Address::as_str))
        .collect();
    let kinds: Vec<&str> = addressed.iter().map(|n| n.kind.as_str()).collect();
    let metadata: Vec<&Json> = addressed.iter().map(|n| &n.metadata).collect();
    let occurred: Vec<NaiveDateTime> = addressed.iter().map(|n| n.occurred_at).collect();
    let scopes: Vec<Option<&str>> = addressed
        .iter()
        .map(|n| n.opt_out.as_ref().map(|(s, _)| s.as_str()))
        .collect();
    let scope_ids: Vec<Option<&str>> = addressed
        .iter()
        .map(|n| n.opt_out.as_ref().map(|(_, id)| id.as_str()))
        .collect();
    let (inserted, updated): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&ids)
        .bind(&addresses)
        .bind(&kinds)
        .bind(&metadata)
        .bind(&occurred)
        .bind(&scopes)
        .bind(&scope_ids)
        .fetch_one(tx)
        .await?;
    Ok(Upserted {
        inserted: inserted as u64,
        updated: updated as u64,
    })
}

async fn upsert_broadcasts(
    tx: &mut sqlx::PgConnection,
    broadcast: &[Ingested],
) -> Result<Upserted, sqlx::Error> {
    // Stored once, listed in every inbox; never emailed (upstream sent none).
    let ids: Vec<Uuid> = broadcast.iter().map(Ingested::id).collect();
    let kinds: Vec<&str> = broadcast.iter().map(|n| n.kind.as_str()).collect();
    let metadata: Vec<&Json> = broadcast.iter().map(|n| &n.metadata).collect();
    let occurred: Vec<NaiveDateTime> = broadcast.iter().map(|n| n.occurred_at).collect();
    let (inserted, updated): (i64, i64) = sqlx::query_as(
        r#"
        WITH ups AS (
            INSERT INTO social.broadcast_notifications AS t (id, type, metadata, occurred_at)
            SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::jsonb[], $4::timestamp[])
                AS b(id, type, metadata, occurred_at)
            ORDER BY id
            ON CONFLICT (id) DO UPDATE
                SET metadata = EXCLUDED.metadata, occurred_at = EXCLUDED.occurred_at
                WHERE t.type = EXCLUDED.type
            RETURNING (xmax = 0) AS inserted
        )
        SELECT count(*) FILTER (WHERE inserted), count(*) FILTER (WHERE NOT inserted) FROM ups
        "#,
    )
    .bind(&ids)
    .bind(&kinds)
    .bind(&metadata)
    .bind(&occurred)
    .fetch_one(tx)
    .await?;
    Ok(Upserted {
        inserted: inserted as u64,
        updated: updated as u64,
    })
}

pub async fn post_notifications(
    State(state): State<AppState>,
    request: Request,
) -> Result<StatusCode, NotificationsError> {
    let producer = match authenticate(&state.settings, request.headers()) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("notification ingest bearer refused");
            return Err(e.into());
        }
    };
    let bytes = axum::body::to_bytes(Body::new(request.into_body()), MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::http(413, "Payload Too Large"))?;
    let body = parse_json(&bytes)?;
    let batch = validate(&body, producer, Utc::now().timestamp_millis()).map_err(|message| {
        tracing::warn!(?producer, %message, "invalid notification batch");
        ApiError::bad_request(message)
    })?;
    let received = batch.len();
    let written = upsert(state.store.pool(), batch).await?;
    tracing::info!(
        ?producer,
        received,
        inserted = written.inserted,
        updated = written.updated,
        "notifications ingested"
    );
    Ok(StatusCode::NO_CONTENT)
}
