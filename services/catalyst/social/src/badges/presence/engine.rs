//! Applies one stored room event to the presence badges (upstream's
//! `logic/presence/presence.ts` and `adapters/presence-store.ts`), in the
//! transaction that deletes the event row. Every presence transaction (this
//! and the sweeper's) first takes one advisory lock, so the flow has a single
//! writer fleet-wide and upstream's per-session locks and retry loops are not
//! needed; progress itself is written by [`apply_progress`], which serializes
//! with every other badge writer.
//!
//! The session of a connection is LiveKit's participant sid. A join or a
//! leave inside a `visit` badge's window grants it; a `stay` badge counts
//! only what a join and the leave of the same session prove, merged across
//! connections, clamped to the window and capped at the first observation
//! that missed the wallet, and completes at the instant the union reaches
//! the required seconds.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDateTime};
use serde_json::Value;
use sqlx::PgConnection;

use super::Settings;
use super::intervals::{self, Interval};
use super::sink::PresenceEvent;
use crate::badges::progress::{Applied, ApplyOptions, apply_progress};
use crate::identity::Address;

/// How far ahead of this clock an event's time may be.
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;
/// More connections of one wallet to one room is not a person: the oldest go.
const MAX_OPEN_CONNECTIONS: usize = 16;

pub(super) async fn lock_flow(conn: &mut PgConnection) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('badges-presence', 0))")
        .execute(conn)
        .await
        .map(|_| ())
}

pub(super) fn naive(ms: i64) -> NaiveDateTime {
    DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .naive_utc()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A scene by one of its parcels (`x,y`), whatever entity is deployed.
    Scene(String),
    /// Any room of a world, by its name.
    World(String),
}

#[derive(Debug, Clone)]
pub struct BadgeConfig {
    pub badge_id: String,
    pub location: Location,
    /// `[start, end)` in epoch ms.
    pub window: Interval,
    pub stay: bool,
    pub required_seconds: i64,
    /// Why the catalog row is not one presence may write, if it is not.
    pub invalid: Option<&'static str>,
}

type ConfigRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    NaiveDateTime,
    NaiveDateTime,
    String,
    i32,
    bool,
    Option<i32>,
);

const SELECT_BADGES: &str = "SELECT p.badge_id, p.location_kind, p.parcel, p.world_name, \
       p.window_start, p.window_end, p.mode, p.required_seconds, d.is_tier, d.criteria_steps \
     FROM social.presence_badges p \
     JOIN social.badge_definitions d ON d.id = p.badge_id \
      AND d.published_at IS NOT NULL AND d.archived_at IS NULL";

/// Upstream's catalog rule (`badge-catalog.ts`): presence writes the whole
/// progress of a single badge whose steps are its rule's (the seconds of a
/// stay, 1 for a visit), never one an evaluator or an import of its own writes.
fn invalid_reason(
    badge_id: &str,
    is_tier: bool,
    criteria_steps: Option<i32>,
    stay: bool,
    required_seconds: i32,
) -> Option<&'static str> {
    use crate::badges::backfill::{Kind, kind};
    // Unlike "can be granted at all", a one-time import is not an owner: presence replaces it.
    let owned = crate::badges::evaluators::EVALUATED.contains(&badge_id)
        || kind(badge_id).is_some_and(|k| !matches!(k, Kind::OneTime | Kind::Unsupported));
    if owned {
        Some("evaluated elsewhere")
    } else if is_tier {
        Some("tiered")
    } else if criteria_steps.unwrap_or(1) != if stay { required_seconds } else { 1 } {
        Some("steps differ from its rule")
    } else {
        None
    }
}

fn config(
    (badge_id, kind, parcel, world, start, end, mode, seconds, is_tier, criteria_steps): ConfigRow,
) -> Option<BadgeConfig> {
    let location = match kind.as_str() {
        "scene" => Location::Scene(parcel?.trim().to_lowercase()),
        _ => Location::World(world?.to_lowercase()),
    };
    let stay = mode == "stay";
    Some(BadgeConfig {
        invalid: invalid_reason(&badge_id, is_tier, criteria_steps, stay, seconds),
        badge_id,
        location,
        window: (
            start.and_utc().timestamp_millis(),
            end.and_utc().timestamp_millis(),
        ),
        stay,
        required_seconds: i64::from(seconds),
    })
}

/// The invalid badges already logged, with the reason logged.
static WARNED_INVALID: std::sync::LazyLock<std::sync::Mutex<HashMap<String, &'static str>>> =
    std::sync::LazyLock::new(Default::default);

/// False, and counted, for a badge whose catalog row presence may not write;
/// logged once per badge and reason.
fn writable(badge: &BadgeConfig, out: &mut EventOutcome) -> bool {
    let mut warned = WARNED_INVALID.lock().unwrap_or_else(|e| e.into_inner());
    let Some(reason) = badge.invalid else {
        warned.remove(&badge.badge_id);
        return true;
    };
    if warned.insert(badge.badge_id.clone(), reason) != Some(reason) {
        tracing::warn!(
            badge_id = badge.badge_id,
            reason,
            "presence badge has an invalid catalog row; not counted"
        );
    }
    out.outcomes.push("badge_invalid");
    false
}

/// The awardable badges whose window contains `at`, or starts within
/// `lookahead_ms` after it.
pub async fn active_badges(
    conn: &mut PgConnection,
    at: i64,
    lookahead_ms: i64,
) -> sqlx::Result<Vec<BadgeConfig>> {
    let rows: Vec<ConfigRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT_BADGES} WHERE p.window_end > $1 AND p.window_start <= $2 ORDER BY p.badge_id"
    )))
    .bind(naive(at))
    .bind(naive(at.saturating_add(lookahead_ms)))
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().filter_map(config).collect())
}

async fn badges_by_id(
    conn: &mut PgConnection,
    ids: &[String],
) -> sqlx::Result<HashMap<String, BadgeConfig>> {
    let rows: Vec<ConfigRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT_BADGES} WHERE p.badge_id = ANY($1)"
    )))
    .bind(ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(config)
        .map(|b| (b.badge_id.clone(), b))
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Visit {
    pub badge_id: String,
    pub wallet: String,
    pub room: String,
    pub opened_at: i64,
    pub joins: Vec<i64>,
    pub sessions: Vec<String>,
    pub realm_name: String,
    pub parcel: Option<String>,
    pub is_world: bool,
    pub last_checked_at: Option<i64>,
    pub absent_since: Option<i64>,
    pub absences: Vec<i64>,
}

/// joins, sessions, absent_since, absences
type ConnectionRow = (Vec<i64>, Vec<String>, Option<i64>, Vec<i64>);

pub(super) const SELECT_VISITS: &str = "SELECT badge_id, wallet, room, opened_at, joins, sessions, \
       realm_name, parcel, is_world, last_checked_at, absent_since, absences \
     FROM social.presence_visits";

/// A visit's open connections, oldest first.
pub(super) fn connections(joins: &[i64], sessions: &[String]) -> Vec<(i64, String)> {
    let mut all: Vec<_> = joins
        .iter()
        .copied()
        .zip(sessions.iter().cloned())
        .collect();
    all.sort_by_key(|(joined, _)| *joined);
    all
}

fn split(connections: &[(i64, String)]) -> (Vec<i64>, Vec<String>) {
    connections.iter().cloned().unzip()
}

/// Persists ordered remaining pairs on the caller's locked connection.
pub(super) async fn persist_remaining_connections(
    conn: &mut PgConnection,
    visit: &Visit,
    remaining: &[(i64, String)],
) -> sqlx::Result<()> {
    if remaining.is_empty() {
        sqlx::query(
            "DELETE FROM social.presence_visits WHERE badge_id = $1 AND wallet = $2 AND room = $3",
        )
        .bind(&visit.badge_id)
        .bind(&visit.wallet)
        .bind(&visit.room)
        .execute(&mut *conn)
        .await?;
    } else {
        let (joins, sessions) = split(remaining);
        sqlx::query(
            "UPDATE social.presence_visits SET joins = $4, sessions = $5, opened_at = $6 \
         WHERE badge_id = $1 AND wallet = $2 AND room = $3",
        )
        .bind(&visit.badge_id)
        .bind(&visit.wallet)
        .bind(&visit.room)
        .bind(&joins)
        .bind(&sessions)
        .bind(joins[0])
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The room of a visit: scene and world rooms only, keyed as upstream.
pub fn room_key(event: &PresenceEvent) -> Option<String> {
    let realm = &event.realm_name;
    match (event.world, &event.scene_id) {
        (true, Some(scene)) => Some(format!("world:{realm}:{scene}")),
        (true, None) => Some(format!("world:{realm}")),
        (false, Some(scene)) => Some(format!("scene:{realm}:{scene}")),
        (false, None) => None,
    }
}

/// Session facts and locks of a world share one key with or without the scene.
fn session_room(room: &str, realm: &str) -> String {
    if room.starts_with("world:") {
        format!("world:{realm}")
    } else {
        room.to_owned()
    }
}

fn ended_key(badge_id: &str, wallet: &str, room: &str, session: &str) -> String {
    format!("ended|{badge_id}|{wallet}|{room}|{session}")
}

/// The whole event identifies a redelivery: the LiveKit id with what it means.
fn event_key(kind: &str, badge_id: &str, event: &PresenceEvent, room: &str) -> String {
    format!(
        "{kind}|{badge_id}|{}|{room}|{}|{}|{}",
        event.wallet, event.session_id, event.event_time_ms, event.event_id
    )
}

/// What a scene badge needs of the event: the deployed scene's parcels,
/// read from `content.deployments` once per event and only when needed. An
/// entity this node does not hold matches no parcel.
struct Place<'a> {
    event: &'a PresenceEvent,
    pointers: Option<Vec<String>>,
}

impl<'a> Place<'a> {
    fn new(event: &'a PresenceEvent) -> Self {
        Self {
            event,
            pointers: None,
        }
    }

    async fn matches(
        &mut self,
        conn: &mut PgConnection,
        badge: &BadgeConfig,
    ) -> sqlx::Result<bool> {
        match &badge.location {
            Location::World(name) => Ok(self.event.world && self.event.realm_name == *name),
            Location::Scene(parcel) => {
                let Some(scene) = self.event.scene_id.as_deref().filter(|_| !self.event.world)
                else {
                    return Ok(false);
                };
                if self.pointers.is_none() {
                    let pointers: Option<Vec<String>> = sqlx::query_scalar(
                        "SELECT entity_pointers FROM content.deployments \
                         WHERE entity_id = $1 AND entity_type = 'scene'",
                    )
                    .bind(scene)
                    .fetch_optional(&mut *conn)
                    .await?;
                    self.pointers = Some(
                        pointers
                            .unwrap_or_default()
                            .iter()
                            .map(|p| p.trim().to_lowercase())
                            .collect(),
                    );
                }
                Ok(self.pointers.as_ref().is_some_and(|p| p.contains(parcel)))
            }
        }
    }
}

/// What one event did, for the metrics and the tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EventOutcome {
    pub outcomes: Vec<&'static str>,
    /// Badges this event completed.
    pub granted: Vec<String>,
}

pub struct Flow<'a> {
    pub settings: &'a Settings,
    /// This process's clock, epoch ms: the bookkeeping times and the future guard.
    pub now: i64,
    /// The badge art base of the `badge_granted` notifications.
    pub asset_base: &'a str,
}

impl Flow<'_> {
    pub async fn apply(
        &self,
        conn: &mut PgConnection,
        event: &PresenceEvent,
    ) -> sqlx::Result<EventOutcome> {
        let mut out = EventOutcome::default();
        if event.event_time_ms > self.now + MAX_CLOCK_SKEW_MS {
            out.outcomes.push("invalid_timestamp");
        } else if event.join {
            self.join(conn, event, &mut out).await?;
        } else {
            self.leave(conn, event, &mut out).await?;
        }
        Ok(out)
    }

    async fn join(
        &self,
        conn: &mut PgConnection,
        event: &PresenceEvent,
        out: &mut EventOutcome,
    ) -> sqlx::Result<()> {
        let Some(room) = room_key(event) else {
            out.outcomes.push("ignored_room");
            return Ok(());
        };
        let at = event.event_time_ms;
        let mut place = Place::new(event);
        let mut matching = Vec::new();
        for badge in active_badges(conn, at, self.settings.lookahead_ms).await? {
            if place.matches(conn, &badge).await? && writable(&badge, out) {
                matching.push(badge);
            }
        }
        for badge in unearned(conn, matching, &event.wallet).await? {
            // A visit is proven by the join itself inside the window.
            if !badge.stay && at >= badge.window.0 {
                if self.grant_visit(conn, &badge, &event.wallet, at).await? {
                    out.granted.push(badge.badge_id.clone());
                }
                continue;
            }
            let outcome = self.join_visit(conn, event, &room, &badge, out).await?;
            out.outcomes.push(outcome);
        }
        Ok(())
    }

    async fn join_visit(
        &self,
        conn: &mut PgConnection,
        event: &PresenceEvent,
        room: &str,
        badge: &BadgeConfig,
        out: &mut EventOutcome,
    ) -> sqlx::Result<&'static str> {
        let (wallet, session, joined) = (&event.wallet, &event.session_id, event.event_time_ms);
        let badge_id = badge.badge_id.as_str();
        if !self
            .record(conn, &event_key("join", badge_id, event, room))
            .await?
        {
            return Ok("join_ignored");
        }
        // A terminal session never opens again, whatever its delivery id.
        if was_recorded(conn, &ended_key(badge_id, wallet, room, session)).await? {
            return Ok("join_session_reused");
        }
        // The leave of this session arrived first: the two prove the stretch.
        let pending: Option<i64> = sqlx::query_scalar(
            "SELECT left_at FROM social.presence_pending_leaves \
             WHERE wallet = $1 AND room = $2 AND session_id = $3 AND left_at >= $4 \
             ORDER BY left_at LIMIT 1",
        )
        .bind(wallet)
        .bind(session_room(room, &event.realm_name))
        .bind(session)
        .bind(joined)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(left_at) = pending {
            let row: Option<(Vec<i64>, Option<i64>)> = sqlx::query_as(
                "SELECT absences, absent_since FROM social.presence_visits \
                 WHERE badge_id = $1 AND wallet = $2 AND room = $3 FOR UPDATE",
            )
            .bind(badge_id)
            .bind(wallet)
            .bind(room)
            .fetch_optional(&mut *conn)
            .await?;
            let paired = self
                .record(
                    conn,
                    &format!("pending-leave|{badge_id}|{wallet}|{room}|{session}"),
                )
                .await?;
            self.remember_ended(conn, badge_id, wallet, room, std::slice::from_ref(session))
                .await?;
            if !paired || left_at - joined > self.settings.max_age_ms {
                return Ok("join_ignored");
            }
            let (absences, absent_since) = row.unwrap_or_default();
            let absences =
                absence_facts(conn, wallet, room, joined, &absences, absent_since).await?;
            if self
                .credit_stretch(conn, wallet, badge, joined, left_at, &absences)
                .await?
            {
                out.granted.push(badge_id.to_owned());
            }
            return Ok("join_paired_with_pending_leave");
        }

        let inserted = sqlx::query(
            "INSERT INTO social.presence_visits \
               (badge_id, wallet, room, opened_at, joins, sessions, realm_name, parcel, is_world) \
             VALUES ($1, $2, $3, $4, ARRAY[$4::bigint], ARRAY[$5::text], $6, $7, $8) \
             ON CONFLICT (badge_id, wallet, room) DO NOTHING",
        )
        .bind(badge_id)
        .bind(wallet)
        .bind(room)
        .bind(joined)
        .bind(session)
        .bind(&event.realm_name)
        .bind(match &badge.location {
            Location::Scene(parcel) => Some(parcel.as_str()),
            Location::World(_) => None,
        })
        .bind(event.world)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if inserted == 1 {
            return Ok("join_opened");
        }

        let (joins, sessions, absent_since): (Vec<i64>, Vec<String>, Option<i64>) = sqlx::query_as(
            "SELECT joins, sessions, absent_since FROM social.presence_visits \
             WHERE badge_id = $1 AND wallet = $2 AND room = $3 FOR UPDATE",
        )
        .bind(badge_id)
        .bind(wallet)
        .bind(room)
        .fetch_one(&mut *conn)
        .await?;
        let open = connections(&joins, &sessions);
        if open.iter().any(|(_, s)| s == session) {
            return Ok("join_ignored");
        }
        let latest = open.last().map_or(i64::MIN, |(j, _)| *j);
        let mut all = open;
        all.push((joined, session.clone()));
        all.sort_by_key(|(j, _)| *j);
        let kept = all.split_off(all.len().saturating_sub(MAX_OPEN_CONNECTIONS));
        let dropped: Vec<String> = all.into_iter().map(|(_, s)| s).collect();
        self.remember_ended(conn, badge_id, wallet, room, &dropped)
            .await?;
        if !kept.iter().any(|(_, s)| s == session) {
            return Ok("join_over_cap");
        }
        let (joins, sessions) = split(&kept);
        // An unconfirmed visit whose wallet connects again is confirmed: the
        // absence is forgotten, the earlier joins (and absences) stay.
        let reconnected = absent_since.is_some() && joined > latest;
        sqlx::query(
            "UPDATE social.presence_visits SET opened_at = $4, joins = $5, sessions = $6, \
               absent_since = CASE WHEN $7 THEN NULL ELSE absent_since END, \
               last_checked_at = CASE WHEN $7 THEN $8 ELSE last_checked_at END \
             WHERE badge_id = $1 AND wallet = $2 AND room = $3",
        )
        .bind(badge_id)
        .bind(wallet)
        .bind(room)
        .bind(joins[0])
        .bind(&joins)
        .bind(&sessions)
        .bind(reconnected)
        .bind(joined)
        .execute(&mut *conn)
        .await?;
        Ok(if reconnected {
            "join_reconnected"
        } else {
            "join_connection_added"
        })
    }

    async fn leave(
        &self,
        conn: &mut PgConnection,
        event: &PresenceEvent,
        out: &mut EventOutcome,
    ) -> sqlx::Result<()> {
        let (wallet, at) = (&event.wallet, event.event_time_ms);
        let open: Vec<Visit> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{SELECT_VISITS} WHERE wallet = $1 AND realm_name = $2 AND opened_at <= $3 ORDER BY opened_at"
        )))
        .bind(wallet)
        .bind(&event.realm_name)
        .bind(at)
        .fetch_all(&mut *conn)
        .await?;
        let candidates = visits_of_leave(open, event);
        let known = room_key(event);
        // A leave pairs with a delayed join only where a configured badge
        // could have counted it: windows a bounded connection can cross.
        let max_age = self.settings.max_age_ms;
        let mut place = Place::new(event);
        let mut matching = Vec::new();
        for badge in active_badges(conn, at - max_age, max_age).await? {
            if place.matches(conn, &badge).await? && writable(&badge, out) {
                matching.push(badge);
            }
        }
        // A wallet with nothing left to earn here keeps no pending leave.
        let matching = unearned(conn, matching, wallet).await?;
        let rooms: Vec<String> = if event.world && !(matching.is_empty() && candidates.is_empty()) {
            vec![format!("world:{}", event.realm_name)]
        } else if !candidates.is_empty() {
            let mut rooms: Vec<String> = candidates.iter().map(|v| v.room.clone()).collect();
            rooms.sort();
            rooms.dedup();
            rooms
        } else {
            known.filter(|_| !matching.is_empty()).into_iter().collect()
        };

        let mut ended = false;
        for room in rooms {
            let holding = self.record_pending_leave(conn, event, &room).await?;
            ended |= self.end_connections(conn, event, holding, out).await?;
        }
        // The leave proves presence at this instant: a visit badge whose window
        // holds it is granted even when no connection of its session was open.
        for badge in &matching {
            if badge.stay || at < badge.window.0 || at >= badge.window.1 {
                continue;
            }
            if self.grant_visit(conn, badge, wallet, at).await? {
                out.granted.push(badge.badge_id.clone());
            }
        }
        if !ended {
            out.outcomes.push("leave_without_visit");
        }
        Ok(())
    }

    /// Keeps the leave as its session's fact and returns the visits holding
    /// a connection of that session.
    async fn record_pending_leave(
        &self,
        conn: &mut PgConnection,
        event: &PresenceEvent,
        room: &str,
    ) -> sqlx::Result<Vec<Visit>> {
        let fact_room = if event.world {
            format!("world:{}", event.realm_name)
        } else {
            room.to_owned()
        };
        sqlx::query(
            "INSERT INTO social.presence_pending_leaves (wallet, room, session_id, left_at, recorded_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (wallet, room, session_id) DO NOTHING",
        )
        .bind(&event.wallet)
        .bind(&fact_room)
        .bind(&event.session_id)
        .bind(event.event_time_ms)
        .bind(self.now)
        .execute(&mut *conn)
        .await?;
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{SELECT_VISITS} WHERE wallet = $1 \
               AND (room = $2 OR ($3 AND realm_name = $4 AND starts_with(room, $2 || ':'))) \
               AND $5 = ANY(sessions)"
        )))
        .bind(&event.wallet)
        .bind(&fact_room)
        .bind(event.world)
        .bind(&event.realm_name)
        .bind(&event.session_id)
        .fetch_all(conn)
        .await
    }

    /// Ends the session's connection in each visit; true when one matched.
    async fn end_connections(
        &self,
        conn: &mut PgConnection,
        event: &PresenceEvent,
        visits: Vec<Visit>,
        out: &mut EventOutcome,
    ) -> sqlx::Result<bool> {
        if visits.is_empty() {
            return Ok(false);
        }
        let ids: Vec<String> = visits
            .iter()
            .map(|v| v.badge_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let badges = badges_by_id(conn, &ids).await?;
        let mut ended = false;
        for visit in visits {
            let Some(badge) = badges.get(&visit.badge_id).filter(|b| writable(b, out)) else {
                // archived, unpublished or made invalid while the visit was open
                drop_visit(conn, self, &visit).await?;
                continue;
            };
            let (matched, closed, granted) =
                self.end_connection(conn, &visit, event, badge).await?;
            if matched {
                ended = true;
                out.outcomes.push(if closed {
                    "visit_closed"
                } else {
                    "connection_closed"
                });
            }
            if granted {
                out.granted.push(badge.badge_id.clone());
            }
        }
        Ok(ended)
    }

    async fn end_connection(
        &self,
        conn: &mut PgConnection,
        visit: &Visit,
        event: &PresenceEvent,
        badge: &BadgeConfig,
    ) -> sqlx::Result<(bool, bool, bool)> {
        let ended_at = event.event_time_ms;
        let row: Option<ConnectionRow> = sqlx::query_as(
            "SELECT joins, sessions, absent_since, absences FROM social.presence_visits \
             WHERE badge_id = $1 AND wallet = $2 AND room = $3 FOR UPDATE",
        )
        .bind(&visit.badge_id)
        .bind(&visit.wallet)
        .bind(&visit.room)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((joins, sessions, absent_since, absences)) = row else {
            return Ok((false, false, false));
        };
        let open = connections(&joins, &sessions);
        // Only the connection of the leave's own session ends, and a leave
        // older than its join proves nothing.
        let Some(joined) = open
            .iter()
            .find(|(_, s)| *s == event.session_id)
            .map(|(j, _)| *j)
            .filter(|j| *j <= ended_at)
        else {
            return Ok((false, false, false));
        };
        let absences = absence_facts(
            conn,
            &visit.wallet,
            &visit.room,
            joined,
            &absences,
            absent_since,
        )
        .await?;
        if !self
            .record(
                conn,
                &event_key("leave", &badge.badge_id, event, &visit.room),
            )
            .await?
        {
            return Ok((false, false, false));
        }
        let remaining: Vec<_> = open
            .into_iter()
            .filter(|(_, s)| *s != event.session_id)
            .collect();
        persist_remaining_connections(conn, &visit, &remaining).await?;
        self.remember_ended(
            conn,
            &visit.badge_id,
            &visit.wallet,
            &visit.room,
            std::slice::from_ref(&event.session_id),
        )
        .await?;
        let granted = ended_at - joined <= self.settings.max_age_ms
            && self
                .credit_stretch(conn, &visit.wallet, badge, joined, ended_at, &absences)
                .await?;
        Ok((true, remaining.is_empty(), granted))
    }

    /// Adds the stretch a join and its leave prove (capped at the first
    /// absence after the join, clamped to the window) to the wallet's
    /// `intervals` and grants what that reaches. True when it completed the badge.
    async fn credit_stretch(
        &self,
        conn: &mut PgConnection,
        wallet: &str,
        badge: &BadgeConfig,
        joined: i64,
        ended: i64,
        absences: &[i64],
    ) -> sqlx::Result<bool> {
        let absent = absences.iter().copied().filter(|a| *a > joined).min();
        let proven_end = absent.map_or(ended, |a| a.min(ended));
        let Some(proven) = intervals::clamp_to_window((joined, proven_end), badge.window) else {
            return Ok(false);
        };
        let Some(address) = lock_progress(conn, wallet, &badge.badge_id).await? else {
            return Ok(false);
        };
        let stored: Option<(Value, Option<NaiveDateTime>)> = sqlx::query_as(
            "SELECT progress, completed_at FROM social.user_badge_progress \
             WHERE address = $1 AND badge_id = $2",
        )
        .bind(&address)
        .bind(&badge.badge_id)
        .fetch_optional(&mut *conn)
        .await?;
        if stored
            .as_ref()
            .is_some_and(|(_, completed)| completed.is_some())
        {
            return Ok(false);
        }
        let mut all =
            intervals::from_document(stored.as_ref().and_then(|(doc, _)| doc.get("intervals")));
        all.push(proven);
        let merged = intervals::merge(all);
        let (completion, steps) = if badge.stay {
            let seconds = intervals::whole_seconds(&merged).min(badge.required_seconds);
            (
                intervals::threshold_crossing(&merged, badge.required_seconds),
                i32::try_from(seconds).unwrap_or(i32::MAX),
            )
        } else {
            (Some(proven.0), 1)
        };
        let document = intervals::to_document(&merged);
        self.write(
            conn,
            &address,
            &badge.badge_id,
            completion.unwrap_or(ended),
            |p| {
                p.state.clear();
                p.state.insert("intervals".into(), document);
                p.steps = steps;
            },
        )
        .await
    }

    /// Grants a visit badge at `at` (upstream's document: `{ "steps": 1 }`).
    async fn grant_visit(
        &self,
        conn: &mut PgConnection,
        badge: &BadgeConfig,
        wallet: &str,
        at: i64,
    ) -> sqlx::Result<bool> {
        let Some(address) = lock_progress(conn, wallet, &badge.badge_id).await? else {
            return Ok(false);
        };
        if has_badge(conn, &badge.badge_id, wallet).await? {
            return Ok(false);
        }
        self.write(conn, &address, &badge.badge_id, at, |p| {
            p.state.clear();
            p.steps = 1;
        })
        .await
    }

    async fn write(
        &self,
        conn: &mut PgConnection,
        address: &Address,
        badge_id: &str,
        at: i64,
        update: impl FnOnce(&mut crate::badges::Progress),
    ) -> sqlx::Result<bool> {
        let applied = apply_progress(
            conn,
            address,
            badge_id,
            &ApplyOptions {
                at: naive(at),
                notify: self.settings.grant_notifications.then_some(self.asset_base),
                granted_by: None,
            },
            update,
        )
        .await?;
        let granted = matches!(&applied, Applied::Written(change) if change.completed);
        if granted {
            metrics::counter!("badges_presence_grants_total").increment(1);
            tracing::info!(
                badge_id,
                wallet = address.as_str(),
                "presence badge granted"
            );
        }
        Ok(granted)
    }

    async fn record(&self, conn: &mut PgConnection, key: &str) -> sqlx::Result<bool> {
        record_event(conn, key, self.now).await
    }

    pub(super) async fn remember_ended(
        &self,
        conn: &mut PgConnection,
        badge_id: &str,
        wallet: &str,
        room: &str,
        sessions: &[String],
    ) -> sqlx::Result<()> {
        for session in sessions {
            self.record(conn, &ended_key(badge_id, wallet, room, session))
                .await?;
        }
        Ok(())
    }
}

/// Scene leaves need an unambiguous room; a world leave finds its world
/// connections by session.
fn visits_of_leave(visits: Vec<Visit>, event: &PresenceEvent) -> Vec<Visit> {
    if event.world {
        return visits
            .into_iter()
            .filter(|v| v.room.starts_with("world:") && v.sessions.contains(&event.session_id))
            .collect();
    }
    let room = room_key(event);
    let (exact, others): (Vec<_>, Vec<_>) = visits
        .into_iter()
        .partition(|v| room.as_deref() == Some(v.room.as_str()));
    if !exact.is_empty() || event.scene_id.is_some() {
        return exact;
    }
    // Without its scene id a leave is applied only if one scene room is open.
    let scenes: Vec<_> = others
        .into_iter()
        .filter(|v| v.room.starts_with("scene:"))
        .collect();
    let rooms: HashSet<_> = scenes.iter().map(|v| v.room.as_str()).collect();
    if rooms.len() == 1 { scenes } else { Vec::new() }
}

async fn lock_progress(
    conn: &mut PgConnection,
    wallet: &str,
    badge_id: &str,
) -> sqlx::Result<Option<Address>> {
    let Ok(address) = Address::parse_untrimmed(wallet) else {
        return Ok(None);
    };
    // apply_progress's key: the read below and its write see the same row.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("badge-progress:{}:{badge_id}", address.as_str()))
        .execute(conn)
        .await?;
    Ok(Some(address))
}

async fn has_badge(conn: &mut PgConnection, badge_id: &str, wallet: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM social.user_badge_progress \
         WHERE address = $1 AND badge_id = $2 AND completed_at IS NOT NULL)",
    )
    .bind(wallet)
    .bind(badge_id)
    .fetch_one(conn)
    .await
}

/// The badges the wallet has not completed, in one statement.
async fn unearned(
    conn: &mut PgConnection,
    badges: Vec<BadgeConfig>,
    wallet: &str,
) -> sqlx::Result<Vec<BadgeConfig>> {
    if badges.is_empty() {
        return Ok(badges);
    }
    let ids: Vec<&str> = badges.iter().map(|b| b.badge_id.as_str()).collect();
    let held: HashSet<String> = sqlx::query_scalar(
        "SELECT badge_id FROM social.user_badge_progress \
         WHERE address = $1 AND badge_id = ANY($2) AND completed_at IS NOT NULL",
    )
    .bind(wallet)
    .bind(&ids)
    .fetch_all(conn)
    .await?
    .into_iter()
    .collect();
    Ok(badges
        .into_iter()
        .filter(|b| !held.contains(&b.badge_id))
        .collect())
}

/// False when the key was already recorded: a redelivery changes nothing.
pub(super) async fn record_event(
    conn: &mut PgConnection,
    key: &str,
    now: i64,
) -> sqlx::Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO social.presence_applied_events (event_id, applied_at) VALUES ($1, $2) \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(key)
    .bind(now)
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}

async fn was_recorded(conn: &mut PgConnection, key: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM social.presence_applied_events WHERE event_id = $1)",
    )
    .bind(key)
    .fetch_one(conn)
    .await
}

/// The absences after `joined`: the visit row's and the kept facts.
async fn absence_facts(
    conn: &mut PgConnection,
    wallet: &str,
    room: &str,
    joined: i64,
    row_absences: &[i64],
    absent_since: Option<i64>,
) -> sqlx::Result<Vec<i64>> {
    let facts: Vec<i64> = sqlx::query_scalar(
        "SELECT absence_at FROM social.presence_absence_facts \
         WHERE wallet = $1 AND room = $2 AND absence_at > $3",
    )
    .bind(wallet)
    .bind(room)
    .bind(joined)
    .fetch_all(conn)
    .await?;
    let mut all: Vec<i64> = row_absences
        .iter()
        .copied()
        .chain(absent_since)
        .chain(facts)
        .collect();
    all.sort_unstable();
    all.dedup();
    Ok(all)
}

/// Closes the visit without time (its stretch is not proven); false when it
/// changed since it was read.
pub(super) async fn drop_visit(
    conn: &mut PgConnection,
    flow: &Flow<'_>,
    visit: &Visit,
) -> sqlx::Result<bool> {
    let deleted = sqlx::query(
        "DELETE FROM social.presence_visits \
         WHERE badge_id = $1 AND wallet = $2 AND room = $3 AND joins = $4",
    )
    .bind(&visit.badge_id)
    .bind(&visit.wallet)
    .bind(&visit.room)
    .bind(&visit.joins)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if deleted != 1 {
        return Ok(false);
    }
    flow.remember_ended(
        conn,
        &visit.badge_id,
        &visit.wallet,
        &visit.room,
        &visit.sessions,
    )
    .await?;
    Ok(true)
}
