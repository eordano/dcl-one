//! `GET /v1/friends` (the caller's accepted friends, read from the
//! `friendships.is_active` projection, with profile summaries) and the
//! plaintext direct-message history between accepted friends. The
//! conversation reads are keyset (`before`) on the `friend_messages_pair_idx`
//! expressions, one index for both directions. Sends carry the old per-signer
//! budget (60 a minute, `State::dm_limiter`, 429 past it), the
//! accepted-friends gate and the 1000-character cap.

use axum::Json;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::Address;
use crate::comms::limits::RateLimitDecision;

use super::friendship::{friendship_status, try_friendship_statuses};
use super::http::CommError;
use super::{SharedState, signed_path};

#[derive(Debug, Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "communities/")
)]
pub struct FriendSummary {
    pub address: String,
    pub name: Option<String>,
    #[serde(rename = "hasClaimedName")]
    pub has_claimed_name: bool,
    #[serde(rename = "avatarUrl")]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "communities/")
)]
pub struct FriendsResponse {
    pub friends: Vec<FriendSummary>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "communities/")
)]
pub struct DirectMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub body: String,
    #[serde(rename = "sentAt")]
    pub sent_at: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "communities/")
)]
pub struct MessagesResponse {
    pub messages: Vec<DirectMessage>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "communities/")
)]
pub struct SendMessageResponse {
    pub message: DirectMessage,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageBody {
    #[serde(default)]
    pub body: String,
}

const FRIENDS_CAP: i64 = 200;
const MAX_MESSAGE_LEN: usize = 1000;

const MESSAGES_PAGE_SQL: &str = "SELECT id, sender_address, recipient_address, body, created_at \
     FROM social.friend_messages \
     WHERE LEAST(sender_address, recipient_address) = LEAST($1, $2) \
       AND GREATEST(sender_address, recipient_address) = GREATEST($1, $2) \
     ORDER BY created_at DESC, id DESC \
     LIMIT $3";
const MESSAGES_PAGE_BEFORE_SQL: &str = "SELECT id, sender_address, recipient_address, body, created_at \
     FROM social.friend_messages \
     WHERE LEAST(sender_address, recipient_address) = LEAST($1, $2) \
       AND GREATEST(sender_address, recipient_address) = GREATEST($1, $2) \
       AND created_at < $3 \
     ORDER BY created_at DESC, id DESC \
     LIMIT $4";

fn iso8601(ts: NaiveDateTime) -> String {
    ts.and_utc().to_rfc3339_opts(SecondsFormat::Millis, true)
}

async fn require_accepted_friend(
    pool: &PgPool,
    me: &Address,
    peer: &Address,
) -> Result<(), CommError> {
    // A failed lookup is a server error, as the block query's below: an
    // empty answer would refuse a friend with 403.
    let statuses =
        try_friendship_statuses(pool, me, std::slice::from_ref(&peer.as_str().to_string()))
            .await
            .map_err(super::http::database_error)?;
    let blocked: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM social.blocks \
         WHERE (blocker_address = $1 AND blocked_address = $2) \
            OR (blocker_address = $2 AND blocked_address = $1))",
    )
    .bind(me)
    .bind(peer)
    .fetch_one(pool)
    .await
    .map_err(super::http::database_error)?;
    if !blocked && statuses.get(peer.as_str()).copied() == Some(friendship_status::ACCEPTED) {
        Ok(())
    } else {
        Err(CommError::status(
            StatusCode::FORBIDDEN,
            "not accepted friends",
        ))
    }
}

fn to_direct_message(
    id: i64,
    sender: String,
    recipient: String,
    body: String,
    created_at: NaiveDateTime,
) -> DirectMessage {
    DirectMessage {
        id: id.to_string(),
        from: sender,
        to: recipient,
        body,
        sent_at: iso8601(created_at),
    }
}

fn parse_peer(raw: &str) -> Result<Address, CommError> {
    Address::parse(raw).map_err(|_| CommError::bad_request("invalid peer address"))
}

pub async fn list_friends(
    State(state): State<SharedState>,
    headers: HeaderMap,
    original: OriginalUri,
) -> Result<impl IntoResponse, CommError> {
    let me = state
        .require_signer(&headers, "get", &signed_path(&original, "/v1/friends"))
        .await?;
    // `is_active` is by the schema's invariant exactly "the latest action is
    // accept", so the projection is read (the RPC `get_friends` shape, 386
    // buffers at 10^4 degree) instead of recomputing it from the actions
    // (1,156 buffers). The 200 cap and `total = friends.len()` are the old
    // wire's.
    let peers: Vec<(String,)> = sqlx::query_as(
        "SELECT CASE WHEN address_requester = $1 THEN address_requested ELSE address_requester END AS peer \
         FROM social.friendships \
         WHERE is_active = TRUE AND (address_requester = $1 OR address_requested = $1) \
         ORDER BY peer \
         LIMIT $2",
    )
    .bind(&me)
    .bind(FRIENDS_CAP)
    .fetch_all(&state.pool)
    .await?;
    let addresses: Vec<String> = peers.into_iter().map(|(p,)| p).collect();
    let profiles = state.profiles.get_profiles(&addresses).await;
    let mut friends: Vec<FriendSummary> = addresses
        .into_iter()
        .map(|address| {
            let info = profiles.get(&address);
            FriendSummary {
                name: info.map(|i| i.name.clone()),
                has_claimed_name: info.map(|i| i.has_claimed_name).unwrap_or(false),
                avatar_url: info.map(|i| i.profile_picture_url.clone()),
                address,
            }
        })
        .collect();
    friends.sort_by(|a, b| {
        let key = |f: &FriendSummary| {
            (
                f.name.is_none(),
                f.name.clone().unwrap_or_default().to_lowercase(),
                f.address.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    let total = friends.len() as i64;
    Ok(Json(FriendsResponse { friends, total }))
}

pub async fn get_messages(
    State(state): State<SharedState>,
    headers: HeaderMap,
    original: OriginalUri,
    Path(peer): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<impl IntoResponse, CommError> {
    let me = state
        .require_signer(
            &headers,
            "get",
            &signed_path(&original, format!("/v1/friends/{peer}/messages")),
        )
        .await?;
    let peer = parse_peer(&peer)?;
    require_accepted_friend(&state.pool, &me, &peer).await?;

    let mut limit: i64 = 50;
    let mut before: Option<NaiveDateTime> = None;
    for (k, v) in &pairs {
        match k.as_str() {
            "limit" => {
                if let Ok(n) = v.parse::<i64>()
                    && n > 0
                {
                    limit = n.min(100);
                }
            }
            "before" => {
                let parsed = DateTime::parse_from_rfc3339(v)
                    .map(|dt| dt.with_timezone(&Utc).naive_utc())
                    .map_err(|_| CommError::bad_request("invalid before timestamp"))?;
                before = Some(parsed);
            }
            _ => {}
        }
    }
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM social.friend_messages \
         WHERE LEAST(sender_address, recipient_address) = LEAST($1, $2) \
           AND GREATEST(sender_address, recipient_address) = GREATEST($1, $2)",
    )
    .bind(&me)
    .bind(&peer)
    .fetch_one(&state.pool)
    .await?;
    // The keyset bound is spelled only when present: a `($3 IS NULL OR
    // created_at < $3)` fold leaves the generic plan's index condition
    // (the same shape the notifications poll was measured on, METRICS.md).
    let mut query =
        sqlx::query_as::<_, (i64, String, String, String, NaiveDateTime)>(if before.is_some() {
            MESSAGES_PAGE_BEFORE_SQL
        } else {
            MESSAGES_PAGE_SQL
        })
        .bind(&me)
        .bind(&peer);
    if let Some(before) = before {
        query = query.bind(before);
    }
    let rows = query.bind(limit).fetch_all(&state.pool).await?;
    let mut messages: Vec<DirectMessage> = rows
        .into_iter()
        .map(|(id, sender, recipient, body, created_at)| {
            to_direct_message(id, sender, recipient, body, created_at)
        })
        .collect();
    messages.reverse();
    Ok(Json(MessagesResponse { messages, total }))
}

pub async fn send_message(
    State(state): State<SharedState>,
    headers: HeaderMap,
    original: OriginalUri,
    Path(peer): Path<String>,
    Json(body): Json<SendMessageBody>,
) -> Result<impl IntoResponse, CommError> {
    let me = state
        .require_signer(
            &headers,
            "post",
            &signed_path(&original, format!("/v1/friends/{peer}/messages")),
        )
        .await?;
    let peer = parse_peer(&peer)?;
    if state.dm_limiter.check(me.as_str()) == RateLimitDecision::Deny {
        return Err(CommError::status(
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded",
        ));
    }
    let text = body.body.trim();
    if text.is_empty() {
        return Err(CommError::bad_request("message body is empty"));
    }
    if text.chars().count() > MAX_MESSAGE_LEN {
        return Err(CommError::bad_request("message body too long"));
    }
    require_accepted_friend(&state.pool, &me, &peer).await?;
    let (id, created_at): (i64, NaiveDateTime) = sqlx::query_as(
        "INSERT INTO social.friend_messages (sender_address, recipient_address, body) \
         VALUES ($1, $2, $3) RETURNING id, created_at",
    )
    .bind(&me)
    .bind(&peer)
    .bind(text)
    .fetch_one(&state.pool)
    .await?;
    let message = to_direct_message(
        id,
        me.into_string(),
        peer.into_string(),
        text.to_string(),
        created_at,
    );
    Ok(Json(SendMessageResponse { message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn accepted_for(peer: &str, statuses: &HashMap<String, i32>) -> bool {
        statuses.get(peer).copied() == Some(friendship_status::ACCEPTED)
    }

    #[test]
    fn accepted_gate_true_only_for_accepted_status() {
        let mut statuses = HashMap::new();
        statuses.insert("0xpeer".to_string(), friendship_status::ACCEPTED);
        assert!(accepted_for("0xpeer", &statuses));
        for other in [
            friendship_status::REQUEST_SENT,
            friendship_status::DELETED,
            friendship_status::BLOCKED,
        ] {
            statuses.insert("0xpeer".to_string(), other);
            assert!(!accepted_for("0xpeer", &statuses));
        }
        assert!(!accepted_for("0xpeer", &HashMap::new()));
    }

    #[tokio::test]
    async fn a_failed_friendship_lookup_is_a_server_error_not_a_refusal() {
        // A database of this test's own: the fault is a missing table.
        let Some(pool) = crate::db::scratch::isolated_pool(1).await else {
            return;
        };
        sqlx::query("DROP TABLE social.friendship_actions CASCADE")
            .execute(&pool)
            .await
            .unwrap();
        let me = Address::parse("0x00000000000000000000000000000000000000a1").unwrap();
        let peer = Address::parse("0x00000000000000000000000000000000000000b2").unwrap();
        let error = require_accepted_friend(&pool, &me, &peer)
            .await
            .expect_err("the lookup failed");
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // The community handlers' database envelope: one `message` string.
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let fields = body.as_object().expect("a JSON object");
        assert_eq!(fields.len(), 1, "{body}");
        assert!(fields["message"].is_string(), "{body}");
        pool.close().await;
    }

    #[test]
    fn sent_at_is_utc_millis_rfc3339() {
        let ts = NaiveDateTime::parse_from_str("2026-01-02T03:04:05.678", "%Y-%m-%dT%H:%M:%S%.3f")
            .unwrap();
        assert_eq!(iso8601(ts), "2026-01-02T03:04:05.678Z");
    }
}
