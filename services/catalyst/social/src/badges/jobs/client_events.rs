//! `social.client_events` → the explorer badges, in `seq` order. The
//! cursor stays above the purge horizon while the job runs (it passes rows
//! within seconds of their insert; the purge deletes after
//! `CLIENT_EVENTS_RETENTION_DAYS`).
//!
//! Traveler's session cache (upstream: Redis, two hours per `(address,
//! sessionId)`, refreshed on each titled move) is the session's own
//! `move-to-parcel` rows read back through `client_events_session_moves_idx`,
//! cut at the first gap of more than two hours; a parcel's scene title is the
//! title of the scene its active pointer names in `content` (upstream fetched
//! it from a catalyst per event).

use std::collections::{HashMap, HashSet};

use chrono::NaiveDateTime;
use serde_json::Value;
use sqlx::PgConnection;

use super::super::evaluators::{self as ev, Move};
use super::{IdSource, Keyed, Pass, award, completed, read_ids};
use crate::identity::Address;

pub const SOURCE: &str = "client_events";

pub(crate) const READ: IdSource = IdSource {
    name: SOURCE,
    select: "SELECT e.seq, e.source, e.sub_type, e.address, e.session_id, e.reported_at, e.metadata \
             FROM social.client_events e",
    key: "e.seq",
    head: "SELECT coalesce(max(seq), 0) FROM social.client_events",
};

/// `traveler.ts` `CACHE_TTL_IN_SECONDS`.
const SESSION_GAP_MS: i64 = 2 * 3_600_000;
/// The session moves read back for one traveler event at most.
const SESSION_MOVES: i64 = 1_000;
/// The steps one `walked-distance` event counts at most. The explorer
/// reports on step thresholds, far below this; a larger count (a client
/// asserting ten million steps) counts this much, a Walkabout Starter.
pub(crate) const MAX_STEPS_PER_EVENT: f64 = 10_000.0;
/// The highest `vertical-height-reached` height taken, in meters; above it
/// the event is passed. Scene height grows with the parcel count (20 m
/// times log2 of parcels + 1, about 330 m for a 90,000-parcel scene), so
/// no scene reaches it.
pub(crate) const MAX_HEIGHT: f64 = 5_000.0;

#[derive(sqlx::FromRow)]
struct Row {
    seq: i64,
    source: String,
    sub_type: String,
    address: String,
    session_id: Option<String>,
    reported_at: NaiveDateTime,
    metadata: Value,
}

impl Keyed for Row {
    fn key(&self) -> i64 {
        self.seq
    }
}

/// Drains the rows past the cursor and in its gaps; returns the rows passed.
pub(super) async fn run(pass: &Pass) -> anyhow::Result<u64> {
    let passed = super::drain(|| batch(pass)).await?;
    super::report_id_lag(&pass.pool, &READ).await;
    Ok(passed)
}

/// One transaction: (rows passed, a full batch was passed).
pub(crate) async fn batch(pass: &Pass) -> Result<(u64, bool), sqlx::Error> {
    let mut tx = pass.pool.begin().await?;
    let Some(read) = read_ids::<Row>(&mut tx, pass, &READ).await? else {
        tx.commit().await?;
        return Ok((0, false));
    };
    if read.is_idle() {
        tx.rollback().await?;
        return Ok((0, false));
    }
    let rows = &read.rows;
    let addresses: Vec<String> = rows
        .iter()
        .map(|r| r.address.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let done = completed(&mut tx, &addresses).await?;
    let mut titles = Titles::default();
    let now = crate::shared::now_utc();
    for row in rows {
        if row.source != "client" || pass.too_old(row.reported_at, now) {
            continue;
        }
        let Ok(address) = Address::parse(&row.address) else {
            tracing::warn!(
                seq = row.seq,
                "client event with an invalid address: passed"
            );
            continue;
        };
        let skip = |badge: &str| done.contains(&(row.address.clone(), badge.to_string()));
        let notify = Some(pass.asset_base.as_str());
        let m = &row.metadata;
        match row.sub_type.as_str() {
            "move-to-parcel" => {
                let parcel = &m["parcel"];
                if !skip(ev::DCL_CITIZEN) {
                    let new_parcel = parcel.get("newParcel").cloned().unwrap_or(Value::Null);
                    award(&mut tx, &address, ev::DCL_CITIZEN, notify, |p| {
                        ev::dcl_citizen(p, &new_parcel)
                    })
                    .await?;
                }
                if !skip(ev::TRAVELER) && scene_pointer(parcel).is_some() {
                    let moves = session_moves(&mut tx, &mut titles, row).await?;
                    if !moves.is_empty() {
                        award(&mut tx, &address, ev::TRAVELER, notify, |p| {
                            ev::traveler(p, &moves)
                        })
                        .await?;
                    }
                }
            }
            "walked-distance" => {
                let Some(count) = number(&m["stepCount"]) else {
                    tracing::debug!(
                        seq = row.seq,
                        "walked-distance without a step count: passed"
                    );
                    continue;
                };
                if !skip(ev::WALKABOUT_WANDERER) {
                    let count = count.min(MAX_STEPS_PER_EVENT) as i64;
                    award(&mut tx, &address, ev::WALKABOUT_WANDERER, notify, |p| {
                        ev::walkabout_wanderer(p, count)
                    })
                    .await?;
                }
            }
            "used-emote" => {
                if !skip(ev::MOVES_MASTER) {
                    let at = row.reported_at.and_utc().timestamp_millis();
                    award(&mut tx, &address, ev::MOVES_MASTER, notify, |p| {
                        ev::moves_master(p, at)
                    })
                    .await?;
                }
            }
            "passport-opened" => {
                let Some(receiver) = m["passport"]["receiver"]
                    .as_str()
                    .and_then(|r| Address::parse(r).ok())
                else {
                    tracing::debug!(
                        seq = row.seq,
                        "passport-opened without a wallet receiver: passed"
                    );
                    continue;
                };
                if !skip(ev::SOCIAL_BUTTERFLY) {
                    award(&mut tx, &address, ev::SOCIAL_BUTTERFLY, notify, |p| {
                        ev::social_butterfly(p, address.as_str(), receiver.as_str())
                    })
                    .await?;
                }
            }
            "vertical-height-reached" => {
                let Some(height) = number(&m["height"]).filter(|h| *h <= MAX_HEIGHT) else {
                    tracing::debug!(
                        seq = row.seq,
                        "vertical-height-reached without a plausible height: passed"
                    );
                    continue;
                };
                if !skip(ev::VERTICAL_VOYAGER) {
                    award(&mut tx, &address, ev::VERTICAL_VOYAGER, notify, |p| {
                        ev::vertical_voyager(p, height)
                    })
                    .await?;
                }
            }
            _ => {}
        }
    }
    read.finish(&mut tx).await?;
    tx.commit().await?;
    Ok((rows.len() as u64, read.more))
}

/// A JSON number, or a numeric string (the explorer sends either).
fn number(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    n.filter(|f| f.is_finite())
}

/// The pointer of a move's scene parcel (`traveler.ts`: not an empty parcel,
/// `newParcel` without parentheses or spaces).
fn scene_pointer(parcel: &Value) -> Option<String> {
    let empty = match parcel.get("isEmptyParcel") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    };
    if empty {
        return None;
    }
    let raw = parcel.get("newParcel")?;
    let raw = match raw {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(","),
        _ => return None,
    };
    let pointer: String = raw
        .chars()
        .filter(|c| !matches!(c, '(' | ')') && !c.is_whitespace())
        .collect();
    (!pointer.is_empty()).then_some(pointer)
}

/// Scene titles by parcel pointer, read once per batch.
#[derive(Default)]
struct Titles(HashMap<String, Option<String>>);

impl Titles {
    async fn resolve(
        &mut self,
        conn: &mut PgConnection,
        pointers: &[String],
    ) -> Result<(), sqlx::Error> {
        let missing: Vec<String> = pointers
            .iter()
            .filter(|p| !self.0.contains_key(*p))
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let found: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT ap.pointer, coalesce(m.doc->'v'->'display'->>'title', m.doc->'display'->>'title') \
             FROM content.active_pointers ap \
             JOIN content.deployments d ON d.entity_id = ap.entity_id AND d.entity_type = 'scene' \
             CROSS JOIN LATERAL (SELECT d.entity_metadata::jsonb AS doc) m \
             WHERE ap.pointer = ANY($1)",
        )
        .bind(&missing)
        .fetch_all(conn)
        .await?;
        for p in missing {
            self.0.insert(p, None);
        }
        for (pointer, title) in found {
            self.0.insert(pointer, title.filter(|t| !t.is_empty()));
        }
        Ok(())
    }

    fn get(&self, pointer: &str) -> Option<&String> {
        self.0.get(pointer).and_then(Option::as_ref)
    }
}

/// The session's titled moves up to and including `row`, oldest first, from
/// the last gap of more than two hours; empty when `row` itself has no title.
async fn session_moves(
    conn: &mut PgConnection,
    titles: &mut Titles,
    row: &Row,
) -> Result<Vec<Move>, sqlx::Error> {
    let session = if row.session_id.is_some() {
        "session_id = $2"
    } else {
        "session_id IS NULL AND $2::text IS NULL"
    };
    let history: Vec<(NaiveDateTime, Value)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT reported_at, metadata->'parcel' FROM social.client_events \
         WHERE address = $1 AND {session} AND sub_type = 'move-to-parcel' AND seq <= $3 \
         ORDER BY seq DESC LIMIT $4"
    )))
    .bind(&row.address)
    .bind(&row.session_id)
    .bind(row.seq)
    .bind(SESSION_MOVES)
    .fetch_all(&mut *conn)
    .await?;
    let pointed: Vec<(i64, String)> = history
        .iter()
        .rev()
        .filter_map(|(at, parcel)| Some((at.and_utc().timestamp_millis(), scene_pointer(parcel)?)))
        .collect();
    let pointers: Vec<String> = pointed.iter().map(|(_, p)| p.clone()).collect();
    titles.resolve(conn, &pointers).await?;
    let titled: Vec<Move> = pointed
        .iter()
        .filter_map(|(on_ms, p)| {
            Some(Move {
                scene_title: titles.get(p)?.clone(),
                on_ms: *on_ms,
            })
        })
        .collect();
    let current_titled = scene_pointer(&row.metadata["parcel"])
        .and_then(|p| titles.get(&p).cloned())
        .is_some();
    if !current_titled {
        return Ok(Vec::new());
    }
    let mut start = titled.len().saturating_sub(1);
    while start > 0 && titled[start].on_ms - titled[start - 1].on_ms <= SESSION_GAP_MS {
        start -= 1;
    }
    Ok(titled[start..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scene_pointers_follow_upstream() {
        assert_eq!(
            scene_pointer(&json!({"newParcel": "(10, -3)", "isEmptyParcel": false})),
            Some("10,-3".into())
        );
        assert_eq!(
            scene_pointer(&json!({"newParcel": "1,1", "isEmptyParcel": true})),
            None
        );
        assert_eq!(scene_pointer(&json!({"isEmptyParcel": false})), None);
        assert_eq!(scene_pointer(&json!({"newParcel": ""})), None);
    }

    #[test]
    fn numbers_read_from_numbers_or_strings() {
        assert_eq!(number(&json!(12)), Some(12.0));
        assert_eq!(number(&json!("500.5")), Some(500.5));
        assert_eq!(number(&json!("x")), None);
        assert_eq!(number(&Value::Null), None);
    }
}
