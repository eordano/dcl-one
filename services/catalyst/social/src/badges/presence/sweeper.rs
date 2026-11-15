//! The fallback for visits whose leave never arrives (upstream's
//! `presence-sweeper.ts`), against this process's LiveKit occupancy
//! observation (`comms::presence`) instead of the gatekeeper's HTTP
//! `/scene-participants`. An observation only cleans up, never credits:
//! a wallet missing from a non-empty observation leaves its visit open but
//! unconfirmed (a late leave still counts join to leave, capped at the
//! absence), and the visit is closed with no time once no leave came within
//! `absent_expire_ms`. No observation, or an empty one, is no information.
//! Connections older than `max_age_ms` are forgotten with no time, and the
//! applied keys, pending leaves and absence facts are pruned after the
//! redelivery window, whether or not observations are available.

use std::collections::{BTreeMap, HashSet};

use sqlx::PgConnection;

use super::engine::{
    Flow, SELECT_VISITS, Visit, connections, drop_visit, persist_remaining_connections,
};

/// Prune batches per table per sweep: with the default batch, 20,000 rows
/// per table a minute, the bound on one sweep's transaction.
pub(super) const PRUNE_PASSES: usize = 20;

/// Who is in a scene or world now. `scene` `None` asks for any room of the
/// world `realm`. `None` answers "unknown"; the instant is when the rooms
/// were read, in epoch ms.
pub trait Occupancy: Send + Sync {
    fn occupants(&self, realm: &str, scene: Option<&str>) -> Option<(i64, HashSet<String>)>;
}

impl Occupancy for crate::comms::presence::Presence {
    fn occupants(&self, realm: &str, scene: Option<&str>) -> Option<(i64, HashSet<String>)> {
        self.room_occupants(realm, scene)
            .map(|(at, wallets)| (at.timestamp_millis(), wallets))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepSummary {
    pub checked: usize,
    pub still_connected: usize,
    pub unconfirmed: usize,
    pub closed_without_time: usize,
    /// Connections forgotten at the maximum age.
    pub expired: usize,
    pub retried: usize,
    pub pruned: u64,
}

impl Flow<'_> {
    pub async fn sweep(
        &self,
        conn: &mut PgConnection,
        occupancy: Option<&dyn Occupancy>,
    ) -> sqlx::Result<SweepSummary> {
        let s = self.settings;
        let now = self.now;
        let mut summary = SweepSummary::default();
        let listed: Vec<Visit> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{SELECT_VISITS} WHERE opened_at < $1 \
               AND (last_checked_at IS NULL OR last_checked_at < $1 OR opened_at < $2) \
             ORDER BY opened_at LIMIT $3"
        )))
        .bind(now - s.check_after_ms)
        .bind(now - s.max_age_ms)
        .bind(s.sweep_batch)
        .fetch_all(&mut *conn)
        .await?;

        let mut places: BTreeMap<(String, Option<String>), Vec<Visit>> = BTreeMap::new();
        for visit in listed {
            if visit.opened_at < now - s.max_age_ms {
                summary.expired += self.expire_joins(conn, &visit, now - s.max_age_ms).await?;
            } else {
                let scene = visit
                    .room
                    .strip_prefix(&format!("scene:{}:", visit.realm_name))
                    .map(str::to_owned);
                places
                    .entry((visit.realm_name.clone(), scene))
                    .or_default()
                    .push(visit);
            }
        }
        summary.pruned = self.prune(conn, now - s.applied_events_ttl_ms).await?;

        let Some(occupancy) = occupancy else {
            return Ok(summary);
        };
        for ((realm, scene), visits) in places {
            summary.checked += visits.len();
            let observed = occupancy
                .occupants(&realm, scene.as_deref())
                .filter(|(_, wallets)| !wallets.is_empty());
            for visit in visits {
                let latest_join = visit.joins.iter().copied().max().unwrap_or(visit.opened_at);
                match &observed {
                    // read before this visit's newest join: says nothing of it
                    Some((at, wallets)) if *at >= latest_join => {
                        if wallets.contains(&visit.wallet) {
                            self.mark_checked(conn, &visit, *at).await?;
                            summary.still_connected += 1;
                        } else {
                            let absent_since = self.mark_absent(conn, &visit, *at).await?;
                            if *at - absent_since >= s.absent_expire_ms {
                                if drop_visit(conn, self, &visit).await? {
                                    summary.closed_without_time += 1;
                                }
                            } else {
                                summary.unconfirmed += 1;
                            }
                        }
                    }
                    _ => {
                        sqlx::query(
                            "UPDATE social.presence_visits SET last_checked_at = $5 \
                             WHERE badge_id = $1 AND wallet = $2 AND room = $3 AND joins = $4",
                        )
                        .bind(&visit.badge_id)
                        .bind(&visit.wallet)
                        .bind(&visit.room)
                        .bind(&visit.joins)
                        .bind(now)
                        .execute(&mut *conn)
                        .await?;
                        summary.retried += 1;
                    }
                }
            }
        }
        Ok(summary)
    }

    /// Forgets the visit's connections that joined before `joined_before`.
    async fn expire_joins(
        &self,
        conn: &mut PgConnection,
        visit: &Visit,
        joined_before: i64,
    ) -> sqlx::Result<usize> {
        let open = connections(&visit.joins, &visit.sessions);
        let (old, kept): (Vec<_>, Vec<_>) = open.into_iter().partition(|(j, _)| *j < joined_before);
        if old.is_empty() {
            return Ok(0);
        }
        let old_sessions: Vec<String> = old.iter().map(|(_, s)| s.clone()).collect();
        self.remember_ended(
            conn,
            &visit.badge_id,
            &visit.wallet,
            &visit.room,
            &old_sessions,
        )
        .await?;
        persist_remaining_connections(conn, visit, &kept).await?;
        Ok(old.len())
    }

    /// Deletes rows older than `before` from each table, `prune_batch` at a
    /// time until a batch comes back short, at most [`PRUNE_PASSES`] batches.
    async fn prune(&self, conn: &mut PgConnection, before: i64) -> sqlx::Result<u64> {
        let batch = self.settings.prune_batch;
        let mut pruned = 0;
        for (table, column) in [
            ("presence_applied_events", "applied_at"),
            ("presence_pending_leaves", "recorded_at"),
            ("presence_absence_facts", "recorded_at"),
        ] {
            for _ in 0..PRUNE_PASSES {
                let deleted = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "WITH stale AS (SELECT ctid FROM social.{table} WHERE {column} < $1 \
                       ORDER BY {column} LIMIT $2) \
                     DELETE FROM social.{table} t USING stale WHERE t.ctid = stale.ctid"
                )))
                .bind(before)
                .bind(batch)
                .execute(&mut *conn)
                .await?
                .rows_affected();
                pruned += deleted;
                if (deleted as i64) < batch {
                    break;
                }
            }
        }
        Ok(pruned)
    }

    /// Listed: the visit stays open and its absence is forgotten (the
    /// absences stay for the connections that joined before them).
    async fn mark_checked(
        &self,
        conn: &mut PgConnection,
        visit: &Visit,
        at: i64,
    ) -> sqlx::Result<()> {
        sqlx::query(
            "UPDATE social.presence_visits SET last_checked_at = $5, absent_since = NULL, \
               absences = ARRAY(SELECT a FROM unnest(absences) AS a \
                                WHERE a > (SELECT min(j) FROM unnest(joins) AS j)) \
             WHERE badge_id = $1 AND wallet = $2 AND room = $3 AND joins = $4",
        )
        .bind(&visit.badge_id)
        .bind(&visit.wallet)
        .bind(&visit.room)
        .bind(&visit.joins)
        .bind(at)
        .execute(conn)
        .await
        .map(|_| ())
    }

    /// Not listed: unconfirmed since the first absence, which is also kept
    /// as a fact. Returns when the wallet was first seen absent.
    async fn mark_absent(
        &self,
        conn: &mut PgConnection,
        visit: &Visit,
        at: i64,
    ) -> sqlx::Result<i64> {
        let absent_since: Option<i64> = sqlx::query_scalar(
            "UPDATE social.presence_visits SET last_checked_at = $5, \
               absences = CASE WHEN absent_since IS NULL THEN array_append(absences, $5) ELSE absences END, \
               absent_since = COALESCE(absent_since, $5) \
             WHERE badge_id = $1 AND wallet = $2 AND room = $3 AND joins = $4 \
             RETURNING absent_since",
        )
        .bind(&visit.badge_id)
        .bind(&visit.wallet)
        .bind(&visit.room)
        .bind(&visit.joins)
        .bind(at)
        .fetch_optional(&mut *conn)
        .await?;
        if visit.absent_since.is_none() && absent_since == Some(at) {
            sqlx::query(
                "INSERT INTO social.presence_absence_facts (wallet, room, absence_at, recorded_at) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (wallet, room, absence_at) DO UPDATE \
                 SET recorded_at = GREATEST(social.presence_absence_facts.recorded_at, EXCLUDED.recorded_at)",
            )
            .bind(&visit.wallet)
            .bind(&visit.room)
            .bind(at)
            .bind(self.now)
            .execute(conn)
            .await?;
        }
        Ok(absent_since.unwrap_or(at))
    }
}
