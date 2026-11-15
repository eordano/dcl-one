//! The queries of the social RPC over the `social` schema (schemas/40_social.sql).
//! Addresses are bound lowercase; the current
//! time is always a bind (`$now`), never `now()` in an index predicate;
//! offset pages carry `COUNT(*) OVER ()` and re-count only when the offset
//! lands past the end, as the old service did.
//!
//! Differences from the old `social_rpc` database, all from the schema
//! design: `blocks` has no surrogate id (PK is the pair); settings live in
//! `social.user_settings`, where a wallet without a row reads
//! `private_messages_privacy` as `all` (upstream's `getDefaultSettings`) and
//! a row a partial upsert creates stores the column default `only_friends`
//! (upstream's migration `1741805019806`, the schema's DEFAULT);
//! a private call's expired row is
//! reclaimed inside the start transaction, since the caller/callee UNIQUEs
//! would otherwise refuse the insert the busy probe already passed; the
//! voice presence probes read `social.private_voice_chat_participants` and
//! `social.community_voice_chat_users` (the old comms gatekeeper tables, now
//! in this schema) instead of asking the gatekeeper over HTTP.

use crate::comms::gatekeeper::voice::db::{PrivateVoiceChatRow, voice_row};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, Utc};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use uuid::Uuid;

/// `private_messages_privacy` of a wallet with no `social.user_settings` row,
/// wherever it is read (upstream `src/logic/settings/utils.ts`,
/// `DEFAULT_DB_PRIVATE_MESSAGES_PRIVACY`).
pub const ABSENT_PRIVATE_MESSAGES_PRIVACY: &str = "all";
/// What a partial upsert stores when it creates the row: the column default
/// (`schemas/40_social.sql`, upstream's social-settings migration).
pub const NEW_ROW_PRIVATE_MESSAGES_PRIVACY: &str = "only_friends";
pub const DEFAULT_BLOCKED_USERS_MESSAGES_VISIBILITY: &str = "show_messages";
pub const DEFAULT_SHOW_SITUATION_REACTIONS: &str = "show";

/// Liveness of a presence row that never confirmed / lost its connection
/// (the comms gatekeeper's `VOICE_CHAT_CONNECTION_INTERRUPTED_TTL` and
/// `VOICE_CHAT_INITIAL_CONNECTION_TTL` defaults).
pub const VOICE_CONNECTION_INTERRUPTED_TTL_MS: i64 = 300_000;
pub const VOICE_INITIAL_CONNECTION_TTL_MS: i64 = 300_000;

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
    /// Unix ms before which no private voice chat known to this process can
    /// expire; the periodic sweep issues no DELETE until then.
    voice_due: Arc<AtomicI64>,
}

/// The counterparty of `$1` in the pair: the requester on the received list,
/// the requested party on the sent list (upstream `getReceivedFriendshipRequests`
/// selects `fa.acting_user`; the old port computed it relative to the actor
/// and named the reader on the received list, which this fixes).
const REQUEST_ADDRESS_SELECT: &str = r#"CASE
    WHEN f.address_requester = $1 THEN f.address_requested
    ELSE f.address_requester
  END AS address"#;
const SENT_ACTOR_COND: &str = "f.last_actor = $1";
const RECEIVED_ACTOR_COND: &str = "f.last_actor <> $1";
/// `FROM … WHERE` of the friends `$1` and `$2` share: active friendships on
/// both sides, minus any friend with a block in either direction to its side.
fn mutual_friends_from() -> String {
    let friends_of = |p: &str| {
        format!(
            "SELECT CASE WHEN f.address_requester = {p} THEN f.address_requested \
                         ELSE f.address_requester END AS friend \
             FROM social.friendships f \
             WHERE f.is_active AND {p} IN (f.address_requester, f.address_requested) AND {}",
            BLOCKING_CONDITION.replace("$1", p)
        )
    };
    format!(
        "FROM ({}) f1 JOIN ({}) f2 ON f1.friend = f2.friend WHERE f1.friend NOT IN ($1, $2)",
        friends_of("$1"),
        friends_of("$2")
    )
}

const BLOCKING_CONDITION: &str = r#"NOT EXISTS (
    SELECT 1 FROM social.blocks b
    WHERE (b.blocker_address = $1 AND b.blocked_address = CASE
              WHEN f.address_requester = $1 THEN f.address_requested
              ELSE f.address_requester END)
       OR (b.blocked_address = $1 AND b.blocker_address = CASE
              WHEN f.address_requester = $1 THEN f.address_requested
              ELSE f.address_requester END)
  )"#;

/// The pair probe on the expressions of `friendships_unordered_pair_uidx`.
const PAIR_COND: &str = "LEAST(address_requester, address_requested) = LEAST($1, $2) \
     AND GREATEST(address_requester, address_requested) = GREATEST($1, $2)";

/// The comms gatekeeper's "connected" predicate over a presence row `p`,
/// with the bind names of `now`, the interrupted TTL and the initial TTL.
fn presence_live_cond(now: &str, interrupted_ttl: &str, initial_ttl: &str) -> String {
    format!(
        "(p.status = 'connected' \
         OR (p.status = 'connection_interrupted' AND p.status_updated_at > {now} - ({interrupted_ttl} * interval '1 millisecond')) \
         OR (p.status = 'not_connected' AND p.joined_at > {now} - ({initial_ttl} * interval '1 millisecond')))"
    )
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// The active community a voice gate reads (`Db::community_voice_gate`).
#[derive(Debug, Clone)]
pub struct VoiceCommunity {
    pub name: String,
    pub private: bool,
    pub unlisted: bool,
}

/// One wallet's standing in it: the stored role and a live ban.
#[derive(Debug, Clone, Default)]
pub struct VoiceParty {
    pub role: Option<String>,
    pub banned: bool,
}

#[derive(Debug, Clone)]
pub struct VoiceGate {
    pub community: Option<VoiceCommunity>,
    pub a: VoiceParty,
    pub b: VoiceParty,
}

#[derive(Debug, Clone)]
pub struct LastAction {
    pub friendship_id: Uuid,
    pub action: String,
    pub acting_user: String,
    pub is_active: bool,
}

/// `last_friendship_action` for one pair, plus whether either side blocks the other.
#[derive(Debug, Clone, Default)]
pub struct FriendshipProbe {
    pub last: Option<LastAction>,
    pub blocked: bool,
    pub blocked_by: bool,
}

const LAST_ACTION_SELECT: &str = r#"
SELECT f.id AS friendship_id, f.is_active AS is_active,
       fa.action AS action, fa.acting_user AS acting_user
FROM social.friendships f
LEFT JOIN LATERAL (
  SELECT action, acting_user FROM social.friendship_actions
  WHERE friendship_id = f.id ORDER BY "timestamp" DESC, id DESC LIMIT 1
) fa ON TRUE
WHERE LEAST(f.address_requester, f.address_requested) = LEAST($1, $2)
  AND GREATEST(f.address_requester, f.address_requested) = GREATEST($1, $2)
LIMIT 1"#;

fn last_action_from_row(r: &sqlx::postgres::PgRow) -> LastAction {
    LastAction {
        friendship_id: r.get::<Uuid, _>("friendship_id"),
        action: r.try_get::<String, _>("action").unwrap_or_default(),
        acting_user: r.try_get::<String, _>("acting_user").unwrap_or_default(),
        is_active: r.try_get::<bool, _>("is_active").unwrap_or(false),
    }
}

#[derive(Debug, Clone)]
pub struct FriendshipRequestRow {
    pub id: Uuid,
    pub address: String,
    pub timestamp: DateTime<Utc>,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BlockedRow {
    pub address: String,
    pub blocked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocialSettingsRow {
    pub private_messages_privacy: String,
    pub blocked_users_messages_visibility: String,
    pub show_situation_reactions: String,
}

impl Default for SocialSettingsRow {
    fn default() -> Self {
        Self {
            private_messages_privacy: ABSENT_PRIVATE_MESSAGES_PRIVACY.into(),
            blocked_users_messages_visibility: DEFAULT_BLOCKED_USERS_MESSAGES_VISIBILITY.into(),
            show_situation_reactions: DEFAULT_SHOW_SITUATION_REACTIONS.into(),
        }
    }
}

/// Everything `start_private_voice_chat` reads before it inserts, in one statement.
#[derive(Debug, Clone)]
pub struct PrivateVoicePreflight {
    pub blocked: bool,
    pub caller_privacy: String,
    pub callee_privacy: String,
    /// `None` when no friendship row exists in either direction.
    pub friends: Option<bool>,
    pub busy: bool,
    /// Either party is in a live community voice room.
    pub caller_in_community_voice: bool,
    pub callee_in_community_voice: bool,
    /// Either party has live presence in a private call room.
    pub caller_in_voice: bool,
    pub callee_in_voice: bool,
}

fn now_naive() -> NaiveDateTime {
    Utc::now().naive_utc()
}

fn expiry_bound(now: NaiveDateTime, expiration_ms: i64) -> NaiveDateTime {
    now - ChronoDuration::milliseconds(expiration_ms)
}

impl Db {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            voice_due: Arc::new(AtomicI64::new(0)),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// A call expiring at `due_ms` exists: the sweep runs no later than that.
    pub fn note_private_voice_due(&self, due_ms: i64) {
        self.voice_due.fetch_min(due_ms, Ordering::Relaxed);
    }

    pub fn private_voice_sweep_due(&self, now_ms: i64) -> bool {
        self.voice_due.load(Ordering::Relaxed) <= now_ms
    }

    /// Takes the pending due mark before a sweep; calls started meanwhile re-arm it.
    pub fn claim_private_voice_sweep(&self) {
        self.voice_due.store(i64::MAX, Ordering::Relaxed);
    }

    /// Milliseconds until the earliest pending call expires (negative when one
    /// already has); `None` when there is none. An accepted call (it has
    /// participant rows) never expires, so it does not arm the sweep.
    pub async fn private_voice_next_expiry_ms(
        &self,
        expiration_ms: i64,
    ) -> Result<Option<i64>, DbError> {
        let earliest: Option<NaiveDateTime> =
            sqlx::query_scalar(
                "SELECT MIN(created_at) FROM social.private_voice_chats \
                 WHERE NOT EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.call_id = private_voice_chats.id)",
            )
                .fetch_one(&self.pool)
                .await?;
        Ok(earliest.map(|created| {
            let expires = created + ChronoDuration::milliseconds(expiration_ms);
            (expires - now_naive()).num_milliseconds()
        }))
    }

    /// One page of friends plus the full count.
    pub async fn get_friends(
        &self,
        address: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<String>, i64), DbError> {
        let addr = address.to_lowercase();
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"
            SELECT CASE WHEN f.address_requester = $1 THEN f.address_requested
                        ELSE f.address_requester END AS friend,
                   COUNT(*) OVER () AS total
            FROM social.friendships f
            WHERE f.is_active = TRUE AND (f.address_requester = $1 OR f.address_requested = $1)
              AND {BLOCKING_CONDITION}
            ORDER BY f.created_at DESC, friend
            LIMIT $2 OFFSET $3
            "#
        )))
        .bind(&addr)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total = match rows.first() {
            Some(r) => r.get::<i64, _>("total"),
            None if offset > 0 => self.count_friends(&addr).await?,
            None => 0,
        };
        Ok((
            rows.into_iter()
                .map(|r| r.get::<String, _>("friend"))
                .collect(),
            total,
        ))
    }

    pub async fn count_friends(&self, address: &str) -> Result<i64, DbError> {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM social.friendships f \
             WHERE f.is_active = TRUE AND (f.address_requester = $1 OR f.address_requested = $1) \
               AND {BLOCKING_CONDITION}"
        )))
        .bind(address.to_lowercase())
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    pub async fn get_mutual_friends(
        &self,
        a: &str,
        b: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<String>, i64), DbError> {
        let a = a.to_lowercase();
        let b = b.to_lowercase();
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT f1.friend, COUNT(*) OVER () AS total {} ORDER BY f1.friend LIMIT $3 OFFSET $4",
            mutual_friends_from()
        )))
        .bind(&a)
        .bind(&b)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total = match rows.first() {
            Some(r) => r.get::<i64, _>("total"),
            None if offset > 0 => self.count_mutual_friends(&a, &b).await?,
            None => 0,
        };
        Ok((
            rows.into_iter()
                .map(|r| r.get::<String, _>("friend"))
                .collect(),
            total,
        ))
    }

    pub async fn count_mutual_friends(&self, a: &str, b: &str) -> Result<i64, DbError> {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) {}",
            mutual_friends_from()
        )))
        .bind(a.to_lowercase())
        .bind(b.to_lowercase())
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    /// Pending requests received (`incoming`) or sent by `address`: the
    /// friendships whose projected `last_action` is `request`, newest first,
    /// with the counterparty's blocks filtered. The page and its total read
    /// the partial pending indexes (bounded by the wallet's pending count,
    /// not its degree); the request message is one action probe per page
    /// row, after the LIMIT.
    pub async fn get_friendship_requests(
        &self,
        address: &str,
        incoming: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<FriendshipRequestRow>, i64), DbError> {
        let addr = address.to_lowercase();
        let sql = format!(
            r#"
            WITH page AS (
                SELECT f.id, f.address_requester, f.address_requested, f.last_action_at,
                       COUNT(*) OVER () AS total
                FROM social.friendships f
                WHERE f.last_action = 'request'
                  AND (f.address_requester = $1 OR f.address_requested = $1)
                  AND {actor_cond}
                  AND {blocking}
                ORDER BY f.last_action_at DESC, f.id DESC
                LIMIT $2 OFFSET $3
            )
            SELECT fa.id AS id,
                   {address_select},
                   f.last_action_at AS ts,
                   fa.metadata AS metadata,
                   f.total
            FROM page f
            LEFT JOIN LATERAL (
                SELECT id, metadata FROM social.friendship_actions
                WHERE friendship_id = f.id ORDER BY "timestamp" DESC, id DESC LIMIT 1
            ) fa ON TRUE
            ORDER BY f.last_action_at DESC, f.id DESC
            "#,
            address_select = REQUEST_ADDRESS_SELECT,
            actor_cond = if incoming {
                RECEIVED_ACTOR_COND
            } else {
                SENT_ACTOR_COND
            },
            blocking = BLOCKING_CONDITION,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&addr)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        let total = match rows.first() {
            Some(r) => r.get::<i64, _>("total"),
            None if offset > 0 => self.count_friendship_requests(&addr, incoming).await?,
            None => 0,
        };
        let page = rows
            .into_iter()
            .map(|r| {
                let metadata: Option<serde_json::Value> = r.try_get("metadata").ok();
                let message = metadata
                    .as_ref()
                    .and_then(|m| m.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string());
                FriendshipRequestRow {
                    id: r.get::<Uuid, _>("id"),
                    address: r.get::<String, _>("address"),
                    timestamp: r.get::<NaiveDateTime, _>("ts").and_utc(),
                    message,
                }
            })
            .collect();
        Ok((page, total))
    }

    pub async fn count_friendship_requests(
        &self,
        address: &str,
        incoming: bool,
    ) -> Result<i64, DbError> {
        let sql = format!(
            r#"
            SELECT COUNT(*)
            FROM social.friendships f
            WHERE f.last_action = 'request'
              AND (f.address_requester = $1 OR f.address_requested = $1)
              AND {actor_cond}
              AND {blocking}
            "#,
            actor_cond = if incoming {
                RECEIVED_ACTOR_COND
            } else {
                SENT_ACTOR_COND
            },
            blocking = BLOCKING_CONDITION,
        );
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(address.to_lowercase())
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    pub async fn last_friendship_action(
        &self,
        a: &str,
        b: &str,
    ) -> Result<Option<LastAction>, DbError> {
        let row = sqlx::query(LAST_ACTION_SELECT)
            .bind(a.to_lowercase())
            .bind(b.to_lowercase())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(last_action_from_row))
    }

    /// The pair's friendship row with its latest action plus both block
    /// directions, in one statement.
    pub async fn friendship_probe(
        &self,
        me: &str,
        other: &str,
    ) -> Result<FriendshipProbe, DbError> {
        friendship_probe_on(&self.pool, me, other).await
    }

    /// Records the block and reads the previous action; the transactional
    /// block_user_recording_action also appends the matching friendship action.
    #[cfg(test)]
    pub(super) async fn block_user_and_last(
        &self,
        blocker: &str,
        blocked: &str,
    ) -> Result<(DateTime<Utc>, Option<LastAction>), DbError> {
        let mut tx = self.pool.begin().await?;
        lock_friendship_pair(&mut tx, blocker, blocked).await?;
        let out = block_and_last(&mut *tx, blocker, blocked).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Removes a block and reads the previous action; use
    /// unblock_user_recording_action to append its delete action atomically.
    #[cfg(test)]
    pub(super) async fn unblock_user_and_last(
        &self,
        blocker: &str,
        blocked: &str,
    ) -> Result<Option<Option<LastAction>>, DbError> {
        let mut tx = self.pool.begin().await?;
        lock_friendship_pair(&mut tx, blocker, blocked).await?;
        let out = unblock_and_last(&mut *tx, blocker, blocked).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// The block and, when the pair has a friendship, its `block` action
    /// (deactivating it), in one transaction: a failed action write rolls
    /// the block back with it (upstream `applyBlock`'s `executeTx`).
    /// Answers when the block was first recorded and whether a friendship
    /// action was written.
    pub async fn block_user_recording_action(
        &self,
        blocker: &str,
        blocked: &str,
    ) -> Result<(DateTime<Utc>, bool), DbError> {
        let mut tx = self.pool.begin().await?;
        lock_friendship_pair(&mut tx, blocker, blocked).await?;
        let (blocked_at, last) = block_and_last(&mut *tx, blocker, blocked).await?;
        if let Some(last) = &last {
            record_friendship_action(
                &mut *tx,
                Uuid::new_v4(),
                blocker,
                blocked,
                "block",
                false,
                Some(last.friendship_id),
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok((blocked_at, last.is_some()))
    }

    /// Removes the block and, when the pair has a friendship, records its
    /// `delete` action, in one transaction (upstream `applyUnblock`).
    /// `None` when there was no block to remove; else whether an action was
    /// written.
    pub async fn unblock_user_recording_action(
        &self,
        blocker: &str,
        blocked: &str,
    ) -> Result<Option<bool>, DbError> {
        let mut tx = self.pool.begin().await?;
        lock_friendship_pair(&mut tx, blocker, blocked).await?;
        let Some(last) = unblock_and_last(&mut *tx, blocker, blocked).await? else {
            return Ok(None);
        };
        if let Some(last) = &last {
            record_friendship_action(
                &mut *tx,
                Uuid::new_v4(),
                blocker,
                blocked,
                "delete",
                false,
                Some(last.friendship_id),
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(Some(last.is_some()))
    }

    /// Upserts the friendship row and appends the action in one statement
    /// ([`record_friendship_action`] with a fresh action id); answers the
    /// friendship id, its original creation time and the action id.
    pub async fn apply_friendship_action(
        &self,
        acting_user: &str,
        other: &str,
        action: &str,
        is_active: bool,
        existing: Option<Uuid>,
        message: Option<&str>,
    ) -> Result<(Uuid, DateTime<Utc>, Uuid), DbError> {
        let action_id = Uuid::new_v4();
        let mut tx = self.pool.begin().await?;
        lock_friendship_pair(&mut tx, acting_user, other).await?;
        let (id, at) = record_friendship_action(
            &mut *tx,
            action_id,
            acting_user,
            other,
            action,
            is_active,
            existing,
            message,
        )
        .await?;
        tx.commit().await?;
        Ok((id, at, action_id))
    }

    pub async fn is_blocked(&self, blocker: &str, blocked: &str) -> Result<bool, DbError> {
        let found: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM social.blocks WHERE blocker_address = $1 AND blocked_address = $2)")
                .bind(blocker.to_lowercase())
                .bind(blocked.to_lowercase())
                .fetch_one(&self.pool)
                .await?;
        Ok(found)
    }

    #[cfg(test)]
    pub(super) async fn block_user(&self, blocker: &str, blocked: &str) -> Result<(), DbError> {
        self.block_user_and_last(blocker, blocked).await.map(|_| ())
    }

    #[cfg(test)]
    pub(super) async fn unblock_user(&self, blocker: &str, blocked: &str) -> Result<(), DbError> {
        self.unblock_user_and_last(blocker, blocked)
            .await
            .map(|_| ())
    }

    pub async fn get_blocked_users(
        &self,
        blocker: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<BlockedRow>, i64), DbError> {
        let blocker = blocker.to_lowercase();
        let rows = sqlx::query(
            "SELECT blocked_address, blocked_at, COUNT(*) OVER () AS total FROM social.blocks \
             WHERE blocker_address = $1 \
             ORDER BY blocked_at DESC, blocked_address ASC LIMIT $2 OFFSET $3",
        )
        .bind(&blocker)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total = match rows.first() {
            Some(r) => r.get::<i64, _>("total"),
            None if offset > 0 => self.count_blocked_users(&blocker).await?,
            None => 0,
        };
        let page = rows
            .into_iter()
            .map(|r| BlockedRow {
                address: r.get::<String, _>("blocked_address"),
                blocked_at: r.get::<NaiveDateTime, _>("blocked_at").and_utc(),
            })
            .collect();
        Ok((page, total))
    }

    pub async fn count_blocked_users(&self, blocker: &str) -> Result<i64, DbError> {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM social.blocks WHERE blocker_address = $1")
                .bind(blocker.to_lowercase())
                .fetch_one(&self.pool)
                .await?;
        Ok(n)
    }

    /// `(who I blocked, who blocked me)`.
    pub async fn get_blocking_status(
        &self,
        address: &str,
    ) -> Result<(Vec<String>, Vec<String>), DbError> {
        let rows = sqlx::query(
            "SELECT blocked_address AS address, TRUE AS outgoing FROM social.blocks WHERE blocker_address = $1 \
             UNION ALL \
             SELECT blocker_address, FALSE FROM social.blocks WHERE blocked_address = $1",
        )
        .bind(address.to_lowercase())
        .fetch_all(&self.pool)
        .await?;
        let (mut blocked, mut blocked_by) = (Vec::new(), Vec::new());
        for r in rows {
            let address = r.get::<String, _>("address");
            if r.get::<bool, _>("outgoing") {
                blocked.push(address)
            } else {
                blocked_by.push(address)
            }
        }
        Ok((blocked, blocked_by))
    }

    pub async fn get_social_settings(
        &self,
        address: &str,
    ) -> Result<Option<SocialSettingsRow>, DbError> {
        let row = sqlx::query(
            "SELECT private_messages_privacy, blocked_users_messages_visibility, show_situation_reactions \
             FROM social.user_settings WHERE address = $1",
        )
        .bind(address.to_lowercase())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SocialSettingsRow {
            private_messages_privacy: r.get("private_messages_privacy"),
            blocked_users_messages_visibility: r.get("blocked_users_messages_visibility"),
            show_situation_reactions: r.get("show_situation_reactions"),
        }))
    }

    /// Absent fields keep the stored value, or the schema default on a new row
    /// (so a new row's privacy is `only_friends`, not the absent-row `all`).
    pub async fn upsert_social_settings(
        &self,
        address: &str,
        private_messages_privacy: Option<&str>,
        blocked_users_messages_visibility: Option<&str>,
        show_situation_reactions: Option<&str>,
    ) -> Result<SocialSettingsRow, DbError> {
        let row = sqlx::query(
            r#"
            INSERT INTO social.user_settings (address, private_messages_privacy,
                   blocked_users_messages_visibility, show_situation_reactions)
            VALUES ($1, COALESCE($2, $5), COALESCE($3, $6), COALESCE($4, $7))
            ON CONFLICT (address) DO UPDATE SET
              private_messages_privacy = COALESCE($2, social.user_settings.private_messages_privacy),
              blocked_users_messages_visibility =
                COALESCE($3, social.user_settings.blocked_users_messages_visibility),
              show_situation_reactions = COALESCE($4, social.user_settings.show_situation_reactions),
              updated_at = timezone('UTC', now())
            RETURNING private_messages_privacy, blocked_users_messages_visibility, show_situation_reactions
            "#,
        )
        .bind(address.to_lowercase())
        .bind(private_messages_privacy)
        .bind(blocked_users_messages_visibility)
        .bind(show_situation_reactions)
        .bind(NEW_ROW_PRIVATE_MESSAGES_PRIVACY)
        .bind(DEFAULT_BLOCKED_USERS_MESSAGES_VISIBILITY)
        .bind(DEFAULT_SHOW_SITUATION_REACTIONS)
        .fetch_one(&self.pool)
        .await?;
        Ok(SocialSettingsRow {
            private_messages_privacy: row.get("private_messages_privacy"),
            blocked_users_messages_visibility: row.get("blocked_users_messages_visibility"),
            show_situation_reactions: row.get("show_situation_reactions"),
        })
    }

    pub async fn reset_social_settings(&self, address: &str) -> Result<bool, DbError> {
        let res = sqlx::query("DELETE FROM social.user_settings WHERE address = $1")
            .bind(address.to_lowercase())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// `(address, privacy, is_friend)` per target, in input order; a target
    /// blocked in either direction is not a friend for this purpose.
    pub async fn private_messages_settings(
        &self,
        caller: &str,
        targets: &[String],
    ) -> Result<Vec<(String, String, bool)>, DbError> {
        let targets: Vec<String> = targets.iter().map(|t| t.to_lowercase()).collect();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT t.addr AS address,
                   COALESCE(s.private_messages_privacy, $3) AS privacy,
                   EXISTS (
                     SELECT 1 FROM social.friendships f
                     WHERE f.is_active = TRUE
                       AND LEAST(f.address_requester, f.address_requested) = LEAST($1, t.addr)
                       AND GREATEST(f.address_requester, f.address_requested) = GREATEST($1, t.addr)
                       AND NOT EXISTS (
                         SELECT 1 FROM social.blocks b
                         WHERE (b.blocker_address = $1 AND b.blocked_address = t.addr)
                            OR (b.blocked_address = $1 AND b.blocker_address = t.addr))
                   ) AS is_friend
            FROM unnest($2::text[]) WITH ORDINALITY AS t(addr, ord)
            LEFT JOIN social.user_settings s ON s.address = t.addr
            ORDER BY t.ord
            "#,
        )
        .bind(caller.to_lowercase())
        .bind(&targets)
        .bind(ABSENT_PRIVATE_MESSAGES_PRIVACY)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("address"),
                    r.get::<String, _>("privacy"),
                    r.get::<bool, _>("is_friend"),
                )
            })
            .collect())
    }

    pub async fn are_users_being_called_or_calling_someone(
        &self,
        addresses: &[String],
        _expiration_ms: i64,
    ) -> Result<bool, DbError> {
        let normalized: Vec<String> = addresses.iter().map(|a| a.to_lowercase()).collect();
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM social.private_voice_chats \
             WHERE (caller_address = ANY($1) OR callee_address = ANY($1)) \
               AND NOT EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.call_id=private_voice_chats.id))",
        )
        .bind(&normalized)
        .fetch_one(&self.pool)
        .await?;
        Ok(busy)
    }

    pub async fn friendship_is_active(&self, a: &str, b: &str) -> Result<Option<bool>, DbError> {
        let row: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT is_active FROM social.friendships WHERE {PAIR_COND} LIMIT 1"
        )))
        .bind(a.to_lowercase())
        .bind(b.to_lowercase())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// One read for every gate of `start_private_voice_chat`: blocks, both
    /// privacy settings, the friendship, the busy probe and the four presence
    /// probes the old service asked the gatekeeper for.
    pub async fn private_voice_preflight(
        &self,
        caller: &str,
        callee: &str,
        _expiration_ms: i64,
        presence: crate::comms::gatekeeper::voice::db::VoiceDbConfig,
    ) -> Result<PrivateVoicePreflight, DbError> {
        let caller = caller.to_lowercase();
        let callee = callee.to_lowercase();
        let sql = format!(
            r#"SELECT
                 EXISTS (
                   SELECT 1 FROM social.blocks
                   WHERE (blocker_address = $1 AND blocked_address = $2)
                      OR (blocker_address = $2 AND blocked_address = $1)
                 ) AS blocked,
                 (SELECT private_messages_privacy FROM social.user_settings WHERE address = $1) AS caller_privacy,
                 (SELECT private_messages_privacy FROM social.user_settings WHERE address = $2) AS callee_privacy,
                 (SELECT is_active FROM social.friendships WHERE {PAIR_COND} LIMIT 1) AS friends,
                 EXISTS (
                   SELECT 1 FROM social.private_voice_chats c
                   WHERE NOT EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.call_id=c.id)
                     AND (caller_address IN ($1, $2) OR callee_address IN ($1, $2))
                 ) AS busy,
                 EXISTS (SELECT 1 FROM social.community_voice_chat_users p WHERE p.address = $1 AND {live}) AS caller_in_community_voice,
                 EXISTS (SELECT 1 FROM social.community_voice_chat_users p WHERE p.address = $2 AND {live}) AS callee_in_community_voice,
                 EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.address = $1 AND {live}) AS caller_in_voice,
                 EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.address = $2 AND {live}) AS callee_in_voice"#,
            live =
                crate::comms::gatekeeper::voice::db::VoiceDbConfig::connected_sql("p", "$3", "$4"),
        );
        let (interrupted, initial) = presence.sql_cutoffs();
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&caller)
            .bind(&callee)
            .bind(interrupted)
            .bind(initial)
            .fetch_one(&self.pool)
            .await?;
        Ok(PrivateVoicePreflight {
            blocked: row.get("blocked"),
            caller_privacy: row
                .get::<Option<String>, _>("caller_privacy")
                .unwrap_or_else(|| ABSENT_PRIVATE_MESSAGES_PRIVACY.into()),
            callee_privacy: row
                .get::<Option<String>, _>("callee_privacy")
                .unwrap_or_else(|| ABSENT_PRIVATE_MESSAGES_PRIVACY.into()),
            friends: row.get("friends"),
            busy: row.get("busy"),
            caller_in_community_voice: row.get("caller_in_community_voice"),
            callee_in_community_voice: row.get("callee_in_community_voice"),
            caller_in_voice: row.get("caller_in_voice"),
            callee_in_voice: row.get("callee_in_voice"),
        })
    }

    /// The comms gatekeeper's `/users/{address}/voice-chat-status`: live
    /// presence in a private call room.
    pub async fn is_user_in_private_voice_chat(&self, address: &str) -> Result<bool, DbError> {
        let found: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.address = $1 AND {})",
            presence_live_cond("$2", "$3", "$4")
        )))
        .bind(address.to_lowercase())
        .bind(now_naive())
        .bind(VOICE_CONNECTION_INTERRUPTED_TTL_MS)
        .bind(VOICE_INITIAL_CONNECTION_TTL_MS)
        .fetch_one(&self.pool)
        .await?;
        Ok(found)
    }

    /// The gatekeeper's `/users/{address}/community-voice-chat-status`.
    pub async fn is_user_in_community_voice_chat(&self, address: &str) -> Result<bool, DbError> {
        let found: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM social.community_voice_chat_users p WHERE p.address = $1 AND {})",
            presence_live_cond("$2", "$3", "$4")
        )))
        .bind(address.to_lowercase())
        .bind(now_naive())
        .bind(VOICE_CONNECTION_INTERRUPTED_TTL_MS)
        .bind(VOICE_INITIAL_CONNECTION_TTL_MS)
        .fetch_one(&self.pool)
        .await?;
        Ok(found)
    }

    pub async fn get_private_voice_chat(
        &self,
        call_id: Uuid,
    ) -> Result<Option<PrivateVoiceChatRow>, DbError> {
        let row = sqlx::query("SELECT id, caller_address, callee_address FROM social.private_voice_chats c WHERE id = $1 AND NOT EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.call_id=c.id)")
            .bind(call_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(voice_row))
    }

    pub async fn incoming_private_voice_chat(
        &self,
        callee: &str,
    ) -> Result<Option<PrivateVoiceChatRow>, DbError> {
        let row = sqlx::query(
            "SELECT id, caller_address, callee_address FROM social.private_voice_chats \
             WHERE callee_address = $1 AND NOT EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.call_id=private_voice_chats.id) ORDER BY created_at DESC LIMIT 1",
        )
        .bind(callee.to_lowercase())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(voice_row))
    }

    /// The pending call this address is on, on either side.
    pub async fn get_private_voice_chat_of_user(
        &self,
        address: &str,
    ) -> Result<Option<PrivateVoiceChatRow>, DbError> {
        let row = sqlx::query(
            "SELECT id, caller_address, callee_address FROM social.private_voice_chats \
             WHERE (caller_address = $1 OR callee_address = $1) AND NOT EXISTS (SELECT 1 FROM social.private_voice_chat_participants p WHERE p.call_id=private_voice_chats.id) ORDER BY created_at DESC LIMIT 1",
        )
        .bind(address.to_lowercase())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(voice_row))
    }

    /// The live calls, newest first, with their expiry (the admin read).
    pub async fn list_active_private_voice_chats(
        &self,
        limit: i64,
        expiration_ms: i64,
    ) -> Result<Vec<(Uuid, String, String, DateTime<Utc>, DateTime<Utc>)>, DbError> {
        let rows = sqlx::query(
            "SELECT id, caller_address, callee_address, created_at FROM social.private_voice_chats \
             WHERE created_at >= $2 ORDER BY created_at DESC LIMIT $1",
        )
        .bind(limit)
        .bind(expiry_bound(now_naive(), expiration_ms))
        .fetch_all(&self.pool)
        .await?;
        let ttl = ChronoDuration::milliseconds(expiration_ms);
        Ok(rows
            .into_iter()
            .map(|r| {
                let created_at = r.get::<NaiveDateTime, _>("created_at").and_utc();
                (
                    r.get::<Uuid, _>("id"),
                    r.get::<String, _>("caller_address"),
                    r.get::<String, _>("callee_address"),
                    created_at,
                    created_at + ttl,
                )
            })
            .collect())
    }

    /// Live ban status from `community_bans`; a ban deletes the membership
    /// row, so a role captured before the ban is the stale value being raced.
    pub async fn is_member_banned(
        &self,
        community_id: &str,
        address: &str,
    ) -> Result<bool, DbError> {
        let Ok(cid) = Uuid::parse_str(community_id) else {
            return Ok(false);
        };
        let active: Option<bool> =
            sqlx::query_scalar("SELECT active FROM social.community_bans WHERE community_id = $1 AND banned_address = $2")
                .bind(cid)
                .bind(address.to_lowercase())
                .fetch_optional(&self.pool)
                .await?;
        Ok(active.unwrap_or(false))
    }

    pub async fn friend_addresses(&self, address: &str) -> Result<Vec<String>, DbError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT CASE WHEN address_requester = $1 THEN address_requested ELSE address_requester END \
             FROM social.friendships WHERE is_active = TRUE AND ($1 IN (address_requester, address_requested))",
        )
        .bind(address.to_lowercase())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Keyset pages bound recipient allocation before fan-out; the address pair indexes
    /// select one wallet's active relationships. No schema/index change is needed.
    pub async fn connectivity_friends_page(
        &self,
        address: &str,
        after: &str,
        unblocked: bool,
    ) -> Result<Vec<String>, DbError> {
        let query = format!(
            "SELECT friend FROM (SELECT CASE WHEN f.address_requester = $1 THEN f.address_requested ELSE f.address_requester END AS friend \
             FROM social.friendships f WHERE f.is_active AND $1 IN (f.address_requester, f.address_requested) \
             AND (NOT $3 OR ({BLOCKING_CONDITION}))) candidates WHERE friend > $2 ORDER BY friend LIMIT 256"
        );
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .bind(address.to_lowercase())
            .bind(after)
            .bind(unblocked)
            .fetch_all(&self.pool)
            .await?)
    }

    /// The same active/no-block rule as initial and live pages, checked for one queued recipient.
    pub async fn connectivity_friend_allowed(
        &self,
        actor: &str,
        recipient: &str,
    ) -> Result<bool, DbError> {
        let query = format!(
            "SELECT EXISTS(SELECT 1 FROM social.friendships f WHERE f.is_active AND $1 IN (f.address_requester, f.address_requested) AND $2 = CASE WHEN f.address_requester = $1 THEN f.address_requested ELSE f.address_requester END AND ({BLOCKING_CONDITION}))"
        );
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .bind(actor.to_lowercase())
            .bind(recipient.to_lowercase())
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn connectivity_member_allowed(
        &self,
        actor: &str,
        recipient: &str,
        community: Uuid,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.communities c JOIN social.community_members a ON a.community_id=c.id AND a.member_address=$1 JOIN social.community_members r ON r.community_id=c.id AND r.member_address=$2 WHERE c.id=$3 AND c.active)")
            .bind(actor.to_lowercase()).bind(recipient.to_lowercase()).bind(community).fetch_one(&self.pool).await?)
    }

    /// The membership PK (community_id, member_address) is the stable two-column cursor.
    pub async fn connectivity_members_page(
        &self,
        address: &str,
        after: Option<(Uuid, String)>,
    ) -> Result<Vec<(Uuid, String)>, DbError> {
        let (community, member) = after.unwrap_or((Uuid::nil(), String::new()));
        Ok(sqlx::query_as(
            "SELECT m.community_id, m.member_address FROM social.community_members m \
             JOIN social.communities c ON c.id = m.community_id AND c.active \
             JOIN social.community_members own ON own.community_id = m.community_id AND own.member_address = $1 \
             WHERE m.member_address <> $1 AND (m.community_id, m.member_address) > ($2, $3) \
             ORDER BY m.community_id, m.member_address LIMIT 256"
        ).bind(address.to_lowercase()).bind(community).bind(member).fetch_all(&self.pool).await?)
    }

    /// One read behind every community-voice gate: the ACTIVE community's
    /// name, privacy and listing (`None` when it is missing or soft-deleted),
    /// and for `a` and `b` their role in it and whether a live ban holds
    /// them. Upstream reads the role (`getCommunityMemberRoles`, active
    /// communities only) and the ban (`isMemberBanned`,
    /// `getBannedMemberAddresses`) on every gate: a ban is not guaranteed to
    /// clear the role row.
    pub async fn community_voice_gate(
        &self,
        community_id: Uuid,
        a: &str,
        b: &str,
    ) -> Result<VoiceGate, DbError> {
        let row = sqlx::query(
            "SELECT c.name, c.private, c.unlisted, \
                    (SELECT role FROM social.community_members \
                      WHERE community_id = c.id AND member_address = $2) AS role_a, \
                    (SELECT role FROM social.community_members \
                      WHERE community_id = c.id AND member_address = $3) AS role_b, \
                    EXISTS (SELECT 1 FROM social.community_bans \
                             WHERE community_id = $1 AND banned_address = $2 AND active) AS banned_a, \
                    EXISTS (SELECT 1 FROM social.community_bans \
                             WHERE community_id = $1 AND banned_address = $3 AND active) AS banned_b \
             FROM (SELECT 1) one \
             LEFT JOIN social.communities c ON c.id = $1 AND c.active",
        )
        .bind(community_id)
        .bind(a.to_lowercase())
        .bind(b.to_lowercase())
        .fetch_one(&self.pool)
        .await?;
        let community = row
            .get::<Option<String>, _>("name")
            .map(|name| VoiceCommunity {
                name,
                private: row.get("private"),
                unlisted: row.get("unlisted"),
            });
        Ok(VoiceGate {
            community,
            a: VoiceParty {
                role: row.get("role_a"),
                banned: row.get("banned_a"),
            },
            b: VoiceParty {
                role: row.get("role_b"),
                banned: row.get("banned_b"),
            },
        })
    }

    /// `(community_id, member_address)` for every other member of every
    /// active community this address belongs to (the presence fan-out); an
    /// inactive one's members (a deactivated mirrored community keeps them)
    /// are inert.
    pub async fn community_co_members(
        &self,
        address: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        let rows = sqlx::query(
            "SELECT m.community_id, m.member_address FROM social.community_members m \
             JOIN social.communities c ON c.id = m.community_id AND c.active \
             WHERE m.member_address <> $1 \
               AND m.community_id IN (SELECT community_id FROM social.community_members WHERE member_address = $1)",
        )
        .bind(address.to_lowercase())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<Uuid, _>("community_id").to_string(),
                    r.get::<String, _>("member_address"),
                )
            })
            .collect())
    }
}

/// Serializes friendship and block writes on one unordered pair until the
/// transaction ends, so a probe and the write it decides stay consistent.
/// The wait is bounded like the membership lock's.
pub(crate) async fn lock_friendship_pair(
    tx: &mut sqlx::PgConnection,
    a: &str,
    b: &str,
) -> Result<(), DbError> {
    sqlx::query("SET LOCAL lock_timeout = '750ms'")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('social.friendship:' || LEAST($1, $2) || ':' || GREATEST($1, $2), 0))",
    )
    .bind(a.to_lowercase())
    .bind(b.to_lowercase())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// [`Db::friendship_probe`] on any executor.
pub(crate) async fn friendship_probe_on<'e, E>(
    executor: E,
    me: &str,
    other: &str,
) -> Result<FriendshipProbe, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        SELECT f.id AS friendship_id, f.is_active AS is_active,
               fa.action AS action, fa.acting_user AS acting_user,
               EXISTS (SELECT 1 FROM social.blocks
                       WHERE blocker_address = $1 AND blocked_address = $2) AS blocked,
               EXISTS (SELECT 1 FROM social.blocks
                       WHERE blocker_address = $2 AND blocked_address = $1) AS blocked_by
        FROM (SELECT 1) AS one
        LEFT JOIN LATERAL (
          SELECT id, is_active FROM social.friendships
          WHERE {PAIR_COND}
          LIMIT 1
        ) f ON TRUE
        LEFT JOIN LATERAL (
          SELECT action, acting_user FROM social.friendship_actions
          WHERE friendship_id = f.id ORDER BY "timestamp" DESC, id DESC LIMIT 1
        ) fa ON TRUE
        "#
    )))
    .bind(me.to_lowercase())
    .bind(other.to_lowercase())
    .fetch_one(executor)
    .await?;
    Ok(FriendshipProbe {
        last: row
            .get::<Option<Uuid>, _>("friendship_id")
            .map(|_| last_action_from_row(&row)),
        blocked: row.get::<bool, _>("blocked"),
        blocked_by: row.get::<bool, _>("blocked_by"),
    })
}

/// Records a block and reads the previous action on the supplied executor.
async fn block_and_last<'e, E>(
    executor: E,
    blocker: &str,
    blocked: &str,
) -> Result<(DateTime<Utc>, Option<LastAction>), DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let sql = format!(
        r#"WITH ins AS (
             INSERT INTO social.blocks (blocker_address, blocked_address) VALUES ($1, $2)
             ON CONFLICT (blocker_address, blocked_address) DO NOTHING
             RETURNING blocked_at
           ), last AS ({LAST_ACTION_SELECT})
           SELECT COALESCE(
                    (SELECT blocked_at FROM ins),
                    (SELECT blocked_at FROM social.blocks
                      WHERE blocker_address = $1 AND blocked_address = $2)
                  ) AS blocked_at,
                  last.*
           FROM (SELECT 1) one LEFT JOIN last ON TRUE"#
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(blocker.to_lowercase())
        .bind(blocked.to_lowercase())
        .fetch_one(executor)
        .await?;
    let blocked_at = row.get::<chrono::NaiveDateTime, _>("blocked_at").and_utc();
    let has_friendship = row.try_get::<Option<Uuid>, _>("friendship_id")?.is_some();
    Ok((
        blocked_at,
        has_friendship.then(|| last_action_from_row(&row)),
    ))
}

/// Removes a block and reads the previous action on the supplied executor.
async fn unblock_and_last<'e, E>(
    executor: E,
    blocker: &str,
    blocked: &str,
) -> Result<Option<Option<LastAction>>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let sql = format!(
        r#"WITH del AS (
             DELETE FROM social.blocks WHERE blocker_address = $1 AND blocked_address = $2
             RETURNING 1
           ), last AS ({LAST_ACTION_SELECT})
           SELECT EXISTS (SELECT 1 FROM del) AS removed, last.*
           FROM (SELECT 1) one LEFT JOIN last ON TRUE"#
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(blocker.to_lowercase())
        .bind(blocked.to_lowercase())
        .fetch_one(executor)
        .await?;
    if !row.get::<bool, _>("removed") {
        return Ok(None);
    }
    let has_friendship = row.try_get::<Option<Uuid>, _>("friendship_id")?.is_some();
    Ok(Some(has_friendship.then(|| last_action_from_row(&row))))
}

/// Upserts the friendship row and appends the action `action_id` in one
/// statement: `is_active`, `last_action`, `last_actor`, `last_action_at`
/// and the action row move together, stamped with the statement's one
/// `now()`. A concurrent insert on the same unordered pair resolves through
/// `friendships_unordered_pair_uidx`. Answers the friendship id and the
/// action's time.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_friendship_action<'e, E>(
    executor: E,
    action_id: Uuid,
    acting_user: &str,
    other: &str,
    action: &str,
    is_active: bool,
    existing: Option<Uuid>,
    message: Option<&str>,
) -> Result<(Uuid, DateTime<Utc>), DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let acting_user = acting_user.to_lowercase();
    let other = other.to_lowercase();
    let metadata = message.map(|m| serde_json::json!({ "message": m }));
    let row = match existing {
        Some(id) => {
            sqlx::query(
                r#"WITH f AS (
                     UPDATE social.friendships
                     SET is_active = $1, last_action = $4, last_actor = $5,
                         last_action_at = timezone('UTC', now()), updated_at = timezone('UTC', now())
                     WHERE id = $2 RETURNING id, created_at
                   ), recorded AS (
                   INSERT INTO social.friendship_actions (id, friendship_id, action, acting_user, metadata, "timestamp")
                   SELECT $3::uuid, f.id, $4::text, $5::text, $6::json, timezone('UTC', now()) FROM f
                   RETURNING friendship_id
                   ) SELECT recorded.friendship_id, f.created_at FROM recorded JOIN f ON f.id=recorded.friendship_id"#,
            )
            .bind(is_active)
            .bind(id)
            .bind(action_id)
            .bind(action)
            .bind(&acting_user)
            .bind(metadata)
            .fetch_one(executor)
            .await?
        }
        None => {
            sqlx::query(
                r#"WITH f AS (
                     INSERT INTO social.friendships (id, address_requester, address_requested, is_active,
                                                     last_action, last_actor, last_action_at)
                     VALUES ($1, $2, $3, $4, $6, $7, timezone('UTC', now()))
                     ON CONFLICT ((LEAST(address_requester, address_requested)),
                                  (GREATEST(address_requester, address_requested)))
                     DO UPDATE SET is_active = EXCLUDED.is_active, last_action = EXCLUDED.last_action,
                                   last_actor = EXCLUDED.last_actor, last_action_at = EXCLUDED.last_action_at,
                                   updated_at = timezone('UTC', now())
                     RETURNING id, created_at
                   ), recorded AS (
                   INSERT INTO social.friendship_actions (id, friendship_id, action, acting_user, metadata, "timestamp")
                   SELECT $5::uuid, f.id, $6::text, $7::text, $8::json, timezone('UTC', now()) FROM f
                   RETURNING friendship_id
                   ) SELECT recorded.friendship_id, f.created_at FROM recorded JOIN f ON f.id=recorded.friendship_id"#,
            )
            .bind(Uuid::new_v4())
            .bind(&acting_user)
            .bind(&other)
            .bind(is_active)
            .bind(action_id)
            .bind(action)
            .bind(&acting_user)
            .bind(metadata)
            .fetch_one(executor)
            .await?
        }
    };
    Ok((
        row.get::<Uuid, _>("friendship_id"),
        row.get::<NaiveDateTime, _>("created_at").and_utc(),
    ))
}

#[derive(Clone, Debug)]
pub(crate) struct VoiceNotification {
    pub raw_community: String,
    pub community: Uuid,
    pub generation: Uuid,
    pub scope: super::fanout::Scope,
    pub fresh: bool,
    pub expires_at: NaiveDateTime,
}

impl Db {
    pub(crate) async fn community_voice_scope(
        &self,
        community: Uuid,
    ) -> Result<Option<super::fanout::Scope>, DbError> {
        let all: Option<bool> = sqlx::query_scalar(
            "SELECT NOT private AND NOT unlisted FROM social.communities WHERE id=$1 AND active",
        )
        .bind(community)
        .fetch_optional(&self.pool)
        .await?;
        Ok(all.map(|all| {
            if all {
                super::fanout::Scope::All
            } else {
                super::fanout::Scope::Members
            }
        }))
    }

    pub(crate) async fn record_voice_notification(
        &self,
        community: Uuid,
        raw: &str,
        scope: super::fanout::Scope,
    ) -> Result<Uuid, DbError> {
        let generation = Uuid::new_v4();
        sqlx::query("INSERT INTO social.community_voice_notifications(raw_community_id,community_id,notification_scope,generation,expires_at) VALUES($1,$2,$3,$4,timezone('UTC',now())+interval '24 hours') ON CONFLICT(raw_community_id) DO UPDATE SET notification_scope=EXCLUDED.notification_scope,generation=EXCLUDED.generation,expires_at=EXCLUDED.expires_at")
            .bind(raw).bind(community).bind(if scope == super::fanout::Scope::All { "all" } else { "members" }).bind(generation).execute(&self.pool).await?;
        Ok(generation)
    }
    pub(crate) async fn voice_notification(
        &self,
        raw: &str,
    ) -> Result<Option<VoiceNotification>, DbError> {
        let row: Option<(Uuid, Uuid, String, bool, NaiveDateTime)> = sqlx::query_as("SELECT community_id,generation,notification_scope,expires_at>timezone('UTC',now()),expires_at FROM social.community_voice_notifications WHERE raw_community_id=$1")
            .bind(raw).fetch_optional(&self.pool).await?;
        Ok(row.map(
            |(community, generation, scope, fresh, expires_at)| VoiceNotification {
                raw_community: raw.to_string(),
                community,
                generation,
                fresh,
                expires_at,
                scope: if scope == "all" {
                    super::fanout::Scope::All
                } else {
                    super::fanout::Scope::Members
                },
            },
        ))
    }
    pub(crate) async fn voice_notifications_page(
        &self,
        after: &str,
    ) -> Result<Vec<VoiceNotification>, DbError> {
        let rows: Vec<(String, Uuid, Uuid, String, bool, NaiveDateTime)> = sqlx::query_as("SELECT raw_community_id,community_id,generation,notification_scope,expires_at>timezone('UTC',now()),expires_at FROM social.community_voice_notifications WHERE raw_community_id>$1 ORDER BY raw_community_id LIMIT 64")
            .bind(after).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(
                |(raw_community, community, generation, scope, fresh, expires_at)| {
                    VoiceNotification {
                        raw_community,
                        community,
                        generation,
                        fresh,
                        expires_at,
                        scope: if scope == "all" {
                            super::fanout::Scope::All
                        } else {
                            super::fanout::Scope::Members
                        },
                    }
                },
            )
            .collect())
    }
    pub(crate) async fn refresh_voice_notification(
        &self,
        entry: &VoiceNotification,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query("UPDATE social.community_voice_notifications SET expires_at=timezone('UTC',now())+interval '24 hours' WHERE raw_community_id=$1 AND generation=$2")
            .bind(&entry.raw_community).bind(entry.generation).execute(&self.pool).await?.rows_affected() == 1)
    }
    pub(crate) async fn restore_voice_notification(
        &self,
        entry: &VoiceNotification,
    ) -> Result<(), DbError> {
        sqlx::query("INSERT INTO social.community_voice_notifications(raw_community_id,community_id,notification_scope,generation,expires_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(raw_community_id) DO NOTHING")
            .bind(&entry.raw_community).bind(entry.community).bind(if entry.scope == super::fanout::Scope::All { "all" } else { "members" }).bind(entry.generation).bind(entry.expires_at).execute(&self.pool).await?;
        Ok(())
    }
    pub(crate) async fn clear_voice_notification(
        &self,
        entry: &VoiceNotification,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query("DELETE FROM social.community_voice_notifications WHERE raw_community_id=$1 AND generation=$2")
            .bind(&entry.raw_community).bind(entry.generation).execute(&self.pool).await?.rows_affected() == 1)
    }
    pub(crate) async fn voice_generation_is(
        &self,
        raw: &str,
        generation: Option<Uuid>,
    ) -> Result<bool, DbError> {
        let current: Option<Uuid> = sqlx::query_scalar(
            "SELECT generation FROM social.community_voice_notifications WHERE raw_community_id=$1",
        )
        .bind(raw)
        .fetch_optional(&self.pool)
        .await?;
        Ok(current == generation)
    }
}

impl Db {
    pub(crate) async fn store_social_event_payload(
        &self,
        payload: Vec<u8>,
    ) -> Result<Uuid, DbError> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO social.event_payloads (id,payload,expires_at) VALUES ($1,$2,timezone('UTC',now())+interval '24 hours')")
            .bind(id).bind(payload).execute(&self.pool).await?;
        Ok(id)
    }
    pub(crate) async fn social_event_payload(&self, id: Uuid) -> Result<Option<Vec<u8>>, DbError> {
        Ok(sqlx::query_scalar("SELECT payload FROM social.event_payloads WHERE id=$1 AND expires_at>timezone('UTC',now())")
            .bind(id).fetch_optional(&self.pool).await?)
    }
    pub(crate) async fn expire_social_event_payloads(&self) -> Result<u64, DbError> {
        Ok(sqlx::query("DELETE FROM social.event_payloads WHERE id IN (SELECT id FROM social.event_payloads WHERE expires_at<=timezone('UTC',now()) ORDER BY expires_at,id LIMIT 64 FOR UPDATE SKIP LOCKED)")
            .execute(&self.pool).await?.rows_affected())
    }
}
