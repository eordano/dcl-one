//! Community notifications: the rows notifications-workers writes for each
//! community event social-service-ea broadcasts, produced here in process.
//!
//! The recipients follow upstream's broadcaster
//! (`src/logic/community/broadcaster.ts` of `decentraland/social-service-ea`
//! at `b6b2cef`) and the metadata follows the processor's event parser
//! (`processor/src/logic/event-parser.ts` of
//! `decentraland/notifications-workers` at `0f113fb`). A recipient who opted
//! out of the community (`subscription_opt_outs`, scope `community`) gets no
//! row, as the processor's opt-out filter drops it. Each row's id is a v5 uuid
//! over (type, recipient, event key), so a repeated event (a re-ban, whose key
//! is stable upstream) is the same row. A failure is logged and never fails
//! the request that caused it, as upstream's fire-and-forget broadcast.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{NewNotification, community_opt_out, insert_notifications};
use crate::{Address, CommunityId};

const COMMUNITY_NS: Uuid = Uuid::from_bytes([
    0x5b, 0x0c, 0x9e, 0x41, 0x2a, 0x7d, 0x4f, 0x18, 0x8e, 0x63, 0xc1, 0x0a, 0x97, 0x24, 0xd3, 0x6f,
]);

/// Who an event reaches (upstream's broadcasting registry).
pub enum Audience<'a> {
    /// One wallet (`directBroadcast` and the single-recipient events).
    One(&'a Address),
    /// Every member (`broadcastToAllMembers`: RENAMED).
    Members,
    /// Moderators and members (`broadcastToAllMembersButOwner`: DELETED).
    MembersButOwner,
    /// Owners and moderators (`broadcastToOwnersAndModerators`:
    /// REQUEST_TO_JOIN_RECEIVED).
    Managers,
    /// Every member but the author (`broadcastToAllMembersButPostAuthor`).
    MembersExcept(&'a Address),
    /// The online members but the opener (`broadcastVoiceChatStarted`).
    OnlineMembersExcept {
        online: &'a BTreeSet<String>,
        except: &'a str,
    },
}

/// One community event: its notification type, upstream's event key and the
/// metadata the processor stores.
pub struct CommunityEvent {
    pub kind: &'static str,
    pub key: String,
    pub community_id: CommunityId,
    pub metadata: Value,
}

/// One wallet, unless it opted out of community `$2`.
static ONE_RECIPIENT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT $1::text WHERE NOT EXISTS ({})",
        community_opt_out("$1", "$2")
    )
});

/// Community `$1`'s members in roles `$2` but `$3`, unless opted out of
/// community `$4`.
static MEMBER_RECIPIENTS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT m.member_address FROM social.community_members m \
         WHERE m.community_id = $1 AND m.role = ANY($2) \
           AND m.member_address IS DISTINCT FROM lower($3) \
           AND NOT EXISTS ({}) \
         ORDER BY m.member_address",
        community_opt_out("m.member_address", "$4")
    )
});

async fn recipients(
    pool: &PgPool,
    community: CommunityId,
    audience: &Audience<'_>,
) -> sqlx::Result<Vec<String>> {
    let (roles, except): (&[&str], Option<&str>) = match audience {
        Audience::One(address) => {
            return sqlx::query_scalar(sqlx::AssertSqlSafe(ONE_RECIPIENT_SQL.as_str()))
                .bind(address.as_str())
                .bind(community)
                .fetch_all(pool)
                .await;
        }
        Audience::Members => (&["owner", "moderator", "member"], None),
        Audience::MembersButOwner => (&["moderator", "member"], None),
        Audience::Managers => (&["owner", "moderator"], None),
        Audience::MembersExcept(author) => {
            (&["owner", "moderator", "member"], Some(author.as_str()))
        }
        Audience::OnlineMembersExcept { except, .. } => {
            (&["owner", "moderator", "member"], Some(*except))
        }
    };
    let roles: Vec<&str> = roles.to_vec();
    let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(MEMBER_RECIPIENTS_SQL.as_str()))
        .bind(community)
        .bind(&roles)
        .bind(except)
        .bind(community)
        .fetch_all(pool)
        .await?;
    Ok(match audience {
        Audience::OnlineMembersExcept { online, .. } => rows
            .into_iter()
            .filter(|a| online.contains(a.as_str()))
            .collect(),
        _ => rows,
    })
}

/// Writes the event's notification rows; returns how many were new.
pub async fn publish(
    pool: &PgPool,
    event: &CommunityEvent,
    audience: Audience<'_>,
) -> sqlx::Result<u64> {
    let batch: Vec<NewNotification> = recipients(pool, event.community_id, &audience)
        .await?
        .into_iter()
        .filter_map(|address| Address::parse(&address).ok())
        .map(|address| {
            let identity = serde_json::to_vec(&(event.kind, address.as_str(), &event.key))
                .expect("the identity holds only strings");
            NewNotification {
                id: Uuid::new_v5(&COMMUNITY_NS, &identity),
                ..NewNotification::now(address, event.kind, event.metadata.clone())
            }
        })
        .collect();
    insert_notifications(pool, &batch).await
}

/// [`publish`], its failure logged: the request that caused the event has
/// already succeeded.
pub async fn notify(pool: &PgPool, event: CommunityEvent, audience: Audience<'_>) {
    if let Err(error) = publish(pool, &event, audience).await {
        tracing::warn!(%error, kind = event.kind, community_id = %event.community_id, "community notification not written");
    }
}

/// The milliseconds upstream suffixes onto event keys that are unique per
/// occurrence (`Date.now()`).
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
