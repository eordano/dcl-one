//! Upstream's presence cases (badges #246 `test/integration/presence.spec.ts`)
//! on an isolated database each (`CATALYST_TEST_DATABASE_URL`; skipped
//! without it): events go through `social.presence_events` and the job's
//! pass, the sweeper reads a fake occupancy.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use serde_json::{Value, json};
use sqlx::PgPool;

use super::sink::{self, PresenceEvent};
use super::{Flow, Occupancy, Settings, SweepSummary, apply_pending, drain, sweep};
use crate::comms::gatekeeper::events::RoomEventStore;
use crate::comms::protocol::bus::room_events::{RoomEvent, RoomEventKind, RoomType};

const T0: i64 = 1_800_000_000_000;
const SEC: i64 = 1000;
const HOUR: i64 = 3_600_000;
const ALICE: &str = "0x00000000000000000000000000000000000a11ce";
const BOB: &str = "0x0000000000000000000000000000000000000b0b";
const SCENE: &str = "bafkreisceneaaaaaaaaaaaaaaaaaaaaa";
const SCENE_REDEPLOYED: &str = "bafkreisceneredeployedbbbbbbbbbbbbbb";
const OTHER_SCENE: &str = "bafkreiotherscenecccccccccccccccccc";
const PARCEL: &str = "10,20";
const WORLD: &str = "party.dcl.eth";
const REALM: &str = "main";
const CDN: &str = "https://cdn.test";

static EVENT_IDS: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
enum At<'a> {
    Scene(&'a str),
    World(Option<&'a str>),
}

struct T {
    pool: PgPool,
    settings: Settings,
    /// The process clock of the next pass.
    now: i64,
}

impl T {
    async fn new() -> Option<Self> {
        let pool = crate::db::scratch::isolated_pool(4).await?;
        for (entity, pointers) in [
            (SCENE, vec!["10,20", "10,21"]),
            (SCENE_REDEPLOYED, vec!["10,20", "10,21", "11,20"]),
            (OTHER_SCENE, vec!["-5,7"]),
        ] {
            sqlx::query(
                "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, \
                   entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
                 VALUES ($1, 'v3', 'scene', $2, '{}', now(), $3, now(), '[]')",
            )
            .bind(ALICE)
            .bind(entity)
            .bind(&pointers)
            .execute(&pool)
            .await
            .unwrap();
        }
        Some(Self {
            pool,
            settings: Settings::default(),
            now: T0 + 10 * HOUR,
        })
    }

    /// A published badge with its presence configuration, window `[T0, T0 + 2h)`.
    async fn badge(&self, id: &str, location: At<'_>, stay_seconds: i32) {
        sqlx::query(
            "INSERT INTO social.badge_definitions (id, name, description, category, is_tier, \
               criteria_steps, assets, published_at) \
             VALUES ($1, $1, 'presence', 'Explorer', false, $2, \
                     jsonb_build_object('2d', jsonb_build_object('normal', $1 || '/2d/normal.png')), \
                     timezone('UTC', now()))",
        )
        .bind(id)
        .bind(stay_seconds.max(1))
        .execute(&self.pool)
        .await
        .unwrap();
        let (kind, parcel, world) = match location {
            At::Scene(parcel) => ("scene", Some(parcel), None),
            At::World(_) => ("world", None, Some(WORLD)),
        };
        sqlx::query(
            "INSERT INTO social.presence_badges (badge_id, location_kind, parcel, world_name, \
               window_start, window_end, mode, required_seconds) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(kind)
        .bind(parcel)
        .bind(world)
        .bind(super::engine::naive(T0))
        .bind(super::engine::naive(T0 + 2 * HOUR))
        .bind(if stay_seconds > 0 { "stay" } else { "visit" })
        .bind(stay_seconds)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    fn event(
        &self,
        join: bool,
        wallet: &str,
        session: &str,
        at: i64,
        place: At<'_>,
    ) -> PresenceEvent {
        let (world, realm, scene) = match place {
            At::Scene(scene) => (false, REALM, Some(scene)),
            At::World(scene) => (true, WORLD, scene),
        };
        PresenceEvent {
            event_id: format!("EV_{}", EVENT_IDS.fetch_add(1, Ordering::Relaxed)),
            join,
            world,
            realm_name: realm.into(),
            scene_id: scene.map(String::from),
            wallet: wallet.into(),
            session_id: session.into(),
            event_time_ms: at,
        }
    }

    async fn deliver(&self, events: &[PresenceEvent]) {
        sink::insert(&self.pool, events).await.unwrap();
        apply_pending(
            &self.pool,
            &self.settings,
            CDN,
            &|| self.now,
            &mut HashMap::new(),
        )
        .await
        .unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM social.presence_events")
            .fetch_one(&self.pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "every stored event is applied and deleted");
    }

    async fn join(&self, wallet: &str, session: &str, at: i64, place: At<'_>) -> PresenceEvent {
        let event = self.event(true, wallet, session, at, place);
        self.deliver(std::slice::from_ref(&event)).await;
        event
    }

    async fn leave(&self, wallet: &str, session: &str, at: i64, place: At<'_>) -> PresenceEvent {
        let event = self.event(false, wallet, session, at, place);
        self.deliver(std::slice::from_ref(&event)).await;
        event
    }

    async fn sweep(&self, now: i64, occupancy: Option<&dyn Occupancy>) -> SweepSummary {
        let flow = Flow {
            settings: &self.settings,
            now,
            asset_base: CDN,
        };
        sweep(&self.pool, &flow, occupancy).await.unwrap()
    }

    /// (steps, document, completed_at ms) of the wallet on the badge.
    async fn progress(&self, badge: &str, wallet: &str) -> Option<(i32, Value, Option<i64>)> {
        let row: Option<(i32, Value, Option<chrono::NaiveDateTime>)> = sqlx::query_as(
            "SELECT steps_done, progress, completed_at FROM social.user_badge_progress \
             WHERE address = $1 AND badge_id = $2",
        )
        .bind(wallet)
        .bind(badge)
        .fetch_optional(&self.pool)
        .await
        .unwrap();
        row.map(|(s, d, c)| (s, d, c.map(|c| c.and_utc().timestamp_millis())))
    }

    async fn notifications(&self, wallet: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT metadata->>'id' FROM social.notifications \
             WHERE address = $1 AND type = 'badge_granted' ORDER BY occurred_at",
        )
        .bind(wallet)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn visits(&self) -> Vec<(String, String, Vec<String>, Option<i64>)> {
        sqlx::query_as(
            "SELECT badge_id, room, sessions, absent_since FROM social.presence_visits ORDER BY badge_id, room",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

struct Seen(Option<(i64, HashSet<String>)>);
impl Occupancy for Seen {
    fn occupants(&self, _: &str, _: Option<&str>) -> Option<(i64, HashSet<String>)> {
        self.0.clone()
    }
}
fn seen(at: i64, wallets: &[&str]) -> Seen {
    Seen(Some((at, wallets.iter().map(|w| w.to_string()).collect())))
}

fn proven(intervals: &[(i64, i64)]) -> Value {
    json!(
        intervals
            .iter()
            .map(|(s, e)| [T0 + s * SEC, T0 + e * SEC])
            .collect::<Vec<_>>()
    )
}

// ── stays ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_stay_below_the_threshold_keeps_the_proven_time_and_grants_nothing() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    t.leave(ALICE, "s1", T0 + 200 * SEC, At::Scene(SCENE)).await;
    assert_eq!(
        t.progress("stay", ALICE).await,
        Some((
            200,
            json!({"steps": 200, "intervals": proven(&[(0, 200)])}),
            None
        ))
    );
    assert!(t.visits().await.is_empty());
    assert!(t.notifications(ALICE).await.is_empty());
}

#[tokio::test]
async fn a_stay_is_granted_when_the_union_of_visits_crosses_the_threshold_at_the_crossing() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    t.leave(ALICE, "s1", T0 + 200 * SEC, At::Scene(SCENE)).await;
    // a redeploy changes the entity id, not the scene the badge counts
    t.join(ALICE, "s2", T0 + 400 * SEC, At::Scene(SCENE_REDEPLOYED))
        .await;
    t.leave(ALICE, "s2", T0 + 600 * SEC, At::Scene(SCENE_REDEPLOYED))
        .await;
    assert_eq!(
        t.progress("stay", ALICE).await,
        Some((
            300,
            json!({"steps": 300, "intervals": proven(&[(0, 200), (400, 600)])}),
            Some(T0 + 500 * SEC)
        ))
    );
    assert_eq!(t.notifications(ALICE).await, ["stay"]);
    // a wallet is granted once, however often it comes back
    t.join(ALICE, "s3", T0 + 700 * SEC, At::Scene(SCENE)).await;
    t.leave(ALICE, "s3", T0 + 1500 * SEC, At::Scene(SCENE))
        .await;
    assert_eq!(
        t.progress("stay", ALICE).await.unwrap().2,
        Some(T0 + 500 * SEC)
    );
    assert_eq!(t.notifications(ALICE).await, ["stay"]);
    // and every wallet's time is its own
    assert_eq!(t.progress("stay", BOB).await, None);
}

#[tokio::test]
async fn overlapping_connections_count_once() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    t.join(ALICE, "s2", T0 + 60 * SEC, At::Scene(SCENE)).await;
    let visits = t.visits().await;
    assert_eq!(visits.len(), 1, "one visit, two connections");
    assert_eq!(visits[0].2, ["s1", "s2"]);
    t.leave(ALICE, "s2", T0 + 120 * SEC, At::Scene(SCENE)).await;
    t.leave(ALICE, "s1", T0 + 240 * SEC, At::Scene(SCENE)).await;
    assert_eq!(
        t.progress("stay", ALICE).await,
        Some((
            240,
            json!({"steps": 240, "intervals": proven(&[(0, 240)])}),
            None
        ))
    );
}

#[tokio::test]
async fn a_late_leave_of_an_earlier_session_does_not_close_the_new_one() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    t.join(ALICE, "s2", T0 + 100 * SEC, At::Scene(SCENE)).await;
    t.leave(ALICE, "s1", T0 + 90 * SEC, At::Scene(SCENE)).await;
    assert_eq!(t.visits().await[0].2, ["s2"]);
    let persisted: (Vec<i64>, Vec<String>, i64) = sqlx::query_as(
        "SELECT joins, sessions, opened_at FROM social.presence_visits WHERE wallet = $1",
    )
    .bind(ALICE)
    .fetch_one(&t.pool)
    .await
    .unwrap();
    assert_eq!(
        persisted,
        (vec![T0 + 100 * SEC], vec!["s2".into()], T0 + 100 * SEC)
    );
    t.leave(ALICE, "s2", T0 + 400 * SEC, At::Scene(SCENE)).await;
    assert!(t.visits().await.is_empty());
    let (_, doc, completed) = t.progress("stay", ALICE).await.unwrap();
    assert_eq!(doc["intervals"], proven(&[(0, 90), (100, 400)]));
    assert_eq!(completed, Some(T0 + 310 * SEC));
}

// ── out of order and redelivery ─────────────────────────────────────────

#[tokio::test]
async fn a_leave_that_arrives_before_its_join_pairs_with_it_for_every_badge_of_the_place() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.badge("stay_short", At::Scene(PARCEL), 60).await;
    t.leave(ALICE, "s1", T0 + 400 * SEC, At::Scene(SCENE)).await;
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM social.presence_pending_leaves")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(pending, 1, "the leave is kept as its session's fact");
    // a join of another session does not pair with it
    t.join(ALICE, "s0", T0 + 10 * SEC, At::Scene(SCENE)).await;
    assert_eq!(t.progress("stay", ALICE).await, None);
    let join = t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    assert_eq!(
        t.progress("stay", ALICE).await.unwrap().2,
        Some(T0 + 300 * SEC)
    );
    assert_eq!(
        t.progress("stay_short", ALICE).await.unwrap().2,
        Some(T0 + 60 * SEC)
    );
    assert_eq!(
        t.visits()
            .await
            .iter()
            .map(|v| v.2.clone())
            .collect::<Vec<_>>(),
        [vec!["s0".to_string()], vec!["s0".into()]]
    );
    // a redelivery of the join, and a distinct delivery of it, change nothing
    let before = t.progress("stay", ALICE).await;
    t.deliver(std::slice::from_ref(&join)).await;
    t.deliver(&[PresenceEvent {
        event_id: "EV_other".into(),
        ..join
    }])
    .await;
    assert_eq!(t.progress("stay", ALICE).await, before);
    assert_eq!(t.visits().await.len(), 2, "the ended session never reopens");
}

#[tokio::test]
async fn a_redelivered_leave_does_not_consume_a_connection_and_a_redelivered_join_does_not_reopen()
{
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    let join = t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    let leave = t.leave(ALICE, "s1", T0 + 100 * SEC, At::Scene(SCENE)).await;
    t.deliver(&[join]).await;
    assert!(
        t.visits().await.is_empty(),
        "the join after its leave does not bring the visit back"
    );
    t.join(ALICE, "s2", T0 + 200 * SEC, At::Scene(SCENE)).await;
    t.deliver(&[leave]).await;
    assert_eq!(t.visits().await[0].2, ["s2"]);
    assert_eq!(t.progress("stay", ALICE).await.unwrap().0, 100);
}

#[tokio::test]
async fn a_leave_older_than_its_join_or_with_no_open_connection_proves_nothing() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.leave(ALICE, "nobody", T0 + 100 * SEC, At::Scene(OTHER_SCENE))
        .await;
    t.join(ALICE, "s1", T0 + 100 * SEC, At::Scene(SCENE)).await;
    t.leave(ALICE, "s1", T0 + 50 * SEC, At::Scene(SCENE)).await;
    assert_eq!(t.progress("stay", ALICE).await, None);
    assert_eq!(t.visits().await.len(), 1);
}

#[tokio::test]
async fn an_event_from_the_future_opens_and_ends_nothing() {
    let Some(mut t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.now = T0;
    t.join(ALICE, "s1", T0 + 10 * 60 * SEC, At::Scene(SCENE))
        .await;
    assert!(t.visits().await.is_empty());
}

#[tokio::test]
async fn a_long_drain_reads_the_clock_per_event() {
    let Some(mut t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.settings.event_batch = 2;
    let min = 60 * SEC;
    let events: Vec<_> = [
        (true, "s1", 0),
        (false, "s1", 100 * SEC),
        (true, "s2", 6 * min),
        (false, "s2", 8 * min),
        (true, "s3", 10 * min),
        (false, "s3", 12 * min),
    ]
    .into_iter()
    .map(|(join, session, at)| t.event(join, ALICE, session, T0 + at, At::Scene(SCENE)))
    .collect();
    sink::insert(&t.pool, &events).await.unwrap();
    // a slow backlog: the clock moves two minutes per event, past the skew bound
    let ticks = AtomicI64::new(0);
    let clock = || T0 + ticks.fetch_add(1, Ordering::Relaxed) * 2 * min;
    drain(&t.pool, &t.settings, CDN, &clock, &mut HashMap::new())
        .await
        .unwrap();
    assert_eq!(ticks.load(Ordering::Relaxed), 6, "one reading per event");
    assert_eq!(
        t.progress("stay", ALICE).await,
        Some((
            300,
            json!({"steps": 300, "intervals": proven(&[(0, 100), (360, 480), (600, 720)])}),
            Some(T0 + 680 * SEC)
        ))
    );
    let newest: i64 =
        sqlx::query_scalar("SELECT max(applied_at) FROM social.presence_applied_events")
            .fetch_one(&t.pool)
            .await
            .unwrap();
    assert_eq!(newest, T0 + 10 * min, "bookkeeping follows the clock");
}

#[derive(Debug)]
struct DbError(&'static str);
impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SQLSTATE {}", self.0)
    }
}
impl std::error::Error for DbError {}
impl sqlx::error::DatabaseError for DbError {
    fn message(&self) -> &str {
        self.0
    }
    fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(self.0.into())
    }
    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }
    fn kind(&self) -> sqlx::error::ErrorKind {
        sqlx::error::ErrorKind::Other
    }
}
fn db_error(code: &'static str) -> sqlx::Error {
    sqlx::Error::Database(Box::new(DbError(code)))
}

#[test]
fn only_deterministic_failures_count_toward_dropping_a_fresh_event() {
    let mut failures = HashMap::new();
    let transient = [
        sqlx::Error::PoolTimedOut,
        sqlx::Error::PoolClosed,
        sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
        db_error("57014"), // statement timeout
        db_error("57P01"), // admin shutdown
        db_error("08006"), // connection failure
        db_error("40001"), // serialization
        db_error("40P01"), // deadlock
        db_error("55P03"), // lock not available
        db_error("25006"), // read-only after a failover
    ];
    for error in &transient {
        for _ in 0..5 {
            assert!(!super::gives_up(&mut failures, 1, error, false), "{error}");
        }
    }
    assert!(failures.is_empty(), "a transient failure is not counted");
    for error in [db_error("23514"), sqlx::Error::RowNotFound] {
        assert!(!super::gives_up(&mut failures, 2, &error, false));
        assert!(!super::gives_up(&mut failures, 2, &error, false));
        assert!(super::gives_up(&mut failures, 2, &error, false), "{error}");
        assert!(failures.is_empty(), "a dropped event is forgotten");
    }
}

#[test]
fn a_transient_failure_of_an_event_past_the_grace_counts() {
    let mut failures = HashMap::new();
    let timeout = db_error("57014");
    assert!(!super::gives_up(&mut failures, 1, &timeout, true));
    assert!(!super::gives_up(&mut failures, 1, &timeout, true));
    assert!(super::gives_up(&mut failures, 1, &timeout, true));
}

#[tokio::test]
async fn an_event_failing_transiently_past_the_grace_is_dropped_and_the_queue_moves_on() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.window_around_now("stay", -1, 1).await;
    t.badge("visit", At::Scene("-5,7"), 0).await;
    t.window_around_now("visit", -1, 1).await;
    // every visit Alice opens fails as a statement timeout would
    sqlx::raw_sql(
        "CREATE FUNCTION public.time_out_alice() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.wallet = '0x00000000000000000000000000000000000a11ce' THEN \
           RAISE EXCEPTION 'statement timeout' USING ERRCODE = '57014'; END IF; \
         RETURN NEW; END $$; \
         CREATE TRIGGER time_out_alice BEFORE INSERT ON social.presence_visits \
         FOR EACH ROW EXECUTE FUNCTION public.time_out_alice();",
    )
    .execute(&t.pool)
    .await
    .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    sink::insert(
        &t.pool,
        &[
            t.event(true, ALICE, "a", now - 60 * SEC, At::Scene(SCENE)),
            t.event(true, BOB, "b", now - 60 * SEC, At::Scene(OTHER_SCENE)),
        ],
    )
    .await
    .unwrap();
    let mut failures = HashMap::new();
    let clock = move || now;
    for _ in 0..5 {
        let pass = apply_pending(&t.pool, &t.settings, CDN, &clock, &mut failures).await;
        assert!(pass.is_err(), "a fresh event waits out the timeout");
    }
    assert!(failures.is_empty());
    sqlx::query("UPDATE social.presence_events SET received_at = received_at - interval '2 hours'")
        .execute(&t.pool)
        .await
        .unwrap();
    for _ in 0..2 {
        let pass = apply_pending(&t.pool, &t.settings, CDN, &clock, &mut failures).await;
        assert!(pass.is_err());
    }
    let pass = apply_pending(&t.pool, &t.settings, CDN, &clock, &mut failures).await;
    assert_eq!(
        pass.unwrap(),
        1,
        "dropped on the third pass, then Bob's join applies"
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM social.presence_events")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert!(t.progress("visit", BOB).await.is_some());
}

// ── windows ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn only_the_part_of_a_stay_inside_the_window_counts() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "early", T0 - 60 * SEC, At::Scene(SCENE))
        .await;
    t.leave(ALICE, "early", T0 + 100 * SEC, At::Scene(SCENE))
        .await;
    t.join(ALICE, "late", T0 + 2 * HOUR - 50 * SEC, At::Scene(SCENE))
        .await;
    t.leave(ALICE, "late", T0 + 2 * HOUR + 500 * SEC, At::Scene(SCENE))
        .await;
    let (steps, doc, _) = t.progress("stay", ALICE).await.unwrap();
    assert_eq!(steps, 150);
    assert_eq!(
        doc["intervals"],
        json!([
            [T0, T0 + 100 * SEC],
            [T0 + 2 * HOUR - 50 * SEC, T0 + 2 * HOUR]
        ])
    );
}

#[tokio::test]
async fn joins_at_the_window_end_or_before_the_lookahead_open_nothing_and_a_stay_ending_at_the_start_adds_nothing()
 {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "end", T0 + 2 * HOUR, At::Scene(SCENE)).await;
    t.join(ALICE, "too_early", T0 - 7 * HOUR, At::Scene(SCENE))
        .await;
    assert!(t.visits().await.is_empty());
    t.join(BOB, "edge", T0 - 100 * SEC, At::Scene(SCENE)).await;
    t.leave(BOB, "edge", T0, At::Scene(SCENE)).await;
    assert_eq!(t.progress("stay", BOB).await, None);
}

// ── visits ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_visit_is_granted_at_the_join_inside_the_window_once() {
    let Some(t) = T::new().await else { return };
    t.badge("visit", At::Scene("-5,7"), 0).await;
    t.join(ALICE, "s1", T0 + 30 * SEC, At::Scene(OTHER_SCENE))
        .await;
    t.join(ALICE, "s2", T0 + 60 * SEC, At::Scene(OTHER_SCENE))
        .await;
    assert_eq!(
        t.progress("visit", ALICE).await,
        Some((1, json!({"steps": 1}), Some(T0 + 30 * SEC)))
    );
    assert_eq!(t.notifications(ALICE).await, ["visit"]);
    assert!(t.visits().await.is_empty(), "a granted visit opens nothing");
}

#[tokio::test]
async fn a_visit_joined_before_the_window_is_granted_at_its_start_by_the_leave_inside_it() {
    let Some(t) = T::new().await else { return };
    t.badge("visit", At::Scene("-5,7"), 0).await;
    t.join(ALICE, "s1", T0 - 60 * SEC, At::Scene(OTHER_SCENE))
        .await;
    assert_eq!(t.visits().await.len(), 1);
    t.leave(ALICE, "s1", T0 + 60 * SEC, At::Scene(OTHER_SCENE))
        .await;
    let (steps, _, completed) = t.progress("visit", ALICE).await.unwrap();
    assert_eq!((steps, completed), (1, Some(T0)));
    // a leave inside the window proves a visit even with no open connection
    t.leave(BOB, "unknown", T0 + 90 * SEC, At::Scene(OTHER_SCENE))
        .await;
    assert_eq!(
        t.progress("visit", BOB).await.unwrap().2,
        Some(T0 + 90 * SEC)
    );
}

// ── places ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_wallet_with_nothing_left_to_earn_keeps_no_pending_leave() {
    let Some(t) = T::new().await else { return };
    t.badge("visit", At::Scene(PARCEL), 0).await;
    t.join(ALICE, "s1", T0 + SEC, At::Scene(SCENE)).await;
    assert!(t.progress("visit", ALICE).await.unwrap().2.is_some());
    t.leave(ALICE, "s2", T0 + 2 * SEC, At::Scene(SCENE)).await;
    t.leave(BOB, "s3", T0 + 2 * SEC, At::Scene(SCENE)).await;
    let pending: Vec<String> =
        sqlx::query_scalar("SELECT session_id FROM social.presence_pending_leaves")
            .fetch_all(&t.pool)
            .await
            .unwrap();
    assert_eq!(pending, ["s3"]);
}

#[tokio::test]
async fn scenes_match_by_their_deployed_parcels_and_worlds_by_name() {
    let Some(t) = T::new().await else { return };
    t.badge("scene_visit", At::Scene(PARCEL), 0).await;
    t.badge("world_visit", At::World(None), 0).await;
    // a world room never counts for a Genesis parcel, an unknown scene matches nothing
    t.join(ALICE, "s1", T0, At::World(Some(SCENE))).await;
    t.join(BOB, "s2", T0, At::Scene("bafkreiunknownscene"))
        .await;
    t.join(BOB, "s3", T0, At::Scene(OTHER_SCENE)).await;
    assert!(t.progress("scene_visit", ALICE).await.is_none());
    assert!(t.progress("world_visit", ALICE).await.is_some());
    assert!(t.progress("scene_visit", BOB).await.is_none());
    assert!(t.progress("world_visit", BOB).await.is_none());
}

#[tokio::test]
async fn a_world_stay_unions_its_scene_rooms_and_a_world_leave_ends_its_own_session() {
    let Some(t) = T::new().await else { return };
    t.badge("world_stay", At::World(None), 300).await;
    t.join(ALICE, "a", T0, At::World(Some("scene-a"))).await;
    t.join(ALICE, "b", T0 + 100 * SEC, At::World(Some("scene-b")))
        .await;
    assert_eq!(t.visits().await.len(), 2, "a visit per world room");
    // the leave of `a` omits its scene: matched by session, `b` stays open
    t.leave(ALICE, "a", T0 + 150 * SEC, At::World(None)).await;
    assert_eq!(
        t.visits()
            .await
            .iter()
            .map(|v| v.1.clone())
            .collect::<Vec<_>>(),
        [format!("world:{WORLD}:scene-b")]
    );
    t.leave(ALICE, "b", T0 + 350 * SEC, At::World(Some("scene-b")))
        .await;
    let (_, doc, completed) = t.progress("world_stay", ALICE).await.unwrap();
    assert_eq!(doc["intervals"], proven(&[(0, 350)]));
    assert_eq!(completed, Some(T0 + 300 * SEC));
}

// ── the catalog ─────────────────────────────────────────────────────────

#[tokio::test]
async fn an_archived_badge_is_not_granted_and_its_open_visit_is_dropped() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 60).await;
    t.badge("visit", At::Scene("-5,7"), 0).await;
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    sqlx::query("UPDATE social.badge_definitions SET archived_at = timezone('UTC', now())")
        .execute(&t.pool)
        .await
        .unwrap();
    t.leave(ALICE, "s1", T0 + 120 * SEC, At::Scene(SCENE)).await;
    t.join(ALICE, "s2", T0 + 130 * SEC, At::Scene(OTHER_SCENE))
        .await;
    assert_eq!(t.progress("stay", ALICE).await, None);
    assert_eq!(t.progress("visit", ALICE).await, None);
    assert!(t.visits().await.is_empty());
    assert!(t.notifications(ALICE).await.is_empty());
}

#[tokio::test]
async fn a_badge_whose_steps_differ_from_its_rule_is_not_counted() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    sqlx::query("UPDATE social.badge_definitions SET criteria_steps = 100 WHERE id = 'stay'")
        .execute(&t.pool)
        .await
        .unwrap();
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    t.leave(ALICE, "s1", T0 + 200 * SEC, At::Scene(SCENE)).await;
    assert_eq!(t.progress("stay", ALICE).await, None);
    assert!(t.visits().await.is_empty());
}

#[tokio::test]
async fn a_presence_row_on_an_evaluated_badge_leaves_its_progress_alone() {
    let Some(t) = T::new().await else { return };
    sqlx::query(
        "INSERT INTO social.presence_badges (badge_id, location_kind, world_name, \
           window_start, window_end, mode, required_seconds) \
         VALUES ('profile_pro', 'world', $1, $2, $3, 'visit', 0)",
    )
    .bind(WORLD)
    .bind(super::engine::naive(T0))
    .bind(super::engine::naive(T0 + 2 * HOUR))
    .execute(&t.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO social.user_badge_progress (address, badge_id, steps_done, progress) \
         VALUES ($1, 'profile_pro', 0, '{\"steps\": 0, \"filled\": [\"name\"]}')",
    )
    .bind(ALICE)
    .execute(&t.pool)
    .await
    .unwrap();
    t.join(ALICE, "s1", T0, At::World(None)).await;
    t.leave(ALICE, "s1", T0 + 10 * SEC, At::World(None)).await;
    assert_eq!(
        t.progress("profile_pro", ALICE).await,
        Some((0, json!({"steps": 0, "filled": ["name"]}), None))
    );
    assert!(t.notifications(ALICE).await.is_empty());
}

#[tokio::test]
async fn an_imported_intervals_document_is_resumed() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    // upstream's document as the TS import writes it
    sqlx::query(
        "INSERT INTO social.user_badge_progress (address, badge_id, steps_done, progress) \
         VALUES ($1, 'stay', 250, $2)",
    )
    .bind(ALICE)
    .bind(json!({"steps": 250, "intervals": proven(&[(1000, 1250)])}))
    .execute(&t.pool)
    .await
    .unwrap();
    t.join(ALICE, "s1", T0 + 1200 * SEC, At::Scene(SCENE)).await;
    t.leave(ALICE, "s1", T0 + 1400 * SEC, At::Scene(SCENE))
        .await;
    assert_eq!(
        t.progress("stay", ALICE).await,
        Some((
            300,
            json!({"steps": 300, "intervals": proven(&[(1000, 1400)])}),
            Some(T0 + 1300 * SEC)
        ))
    );
}

// ── the sweeper ─────────────────────────────────────────────────────────

#[tokio::test]
async fn an_absence_caps_a_late_leave_and_a_reconnect_after_it_is_counted_in_full() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 3000).await;
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    let s = t
        .sweep(T0 + 1000 * SEC, Some(&seen(T0 + 1000 * SEC, &[BOB])))
        .await;
    assert_eq!((s.checked, s.unconfirmed), (1, 1));
    assert_eq!(t.visits().await[0].3, Some(T0 + 1000 * SEC));
    t.join(ALICE, "s2", T0 + 1100 * SEC, At::Scene(SCENE)).await;
    assert_eq!(
        t.visits().await[0].3,
        None,
        "a reconnect confirms the visit again"
    );
    t.leave(ALICE, "s1", T0 + 1500 * SEC, At::Scene(SCENE))
        .await;
    t.leave(ALICE, "s2", T0 + 1600 * SEC, At::Scene(SCENE))
        .await;
    let (_, doc, _) = t.progress("stay", ALICE).await.unwrap();
    assert_eq!(doc["intervals"], proven(&[(0, 1000), (1100, 1600)]));
    // the absence outlives the visit row for a join older than it
    let facts: Vec<i64> =
        sqlx::query_scalar("SELECT absence_at FROM social.presence_absence_facts")
            .fetch_all(&t.pool)
            .await
            .unwrap();
    assert_eq!(facts, [T0 + 1000 * SEC]);
}

#[tokio::test]
async fn an_unconfirmed_visit_is_closed_without_time_once_no_leave_came_within_the_bound() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    t.sweep(T0 + 1000 * SEC, Some(&seen(T0 + 1000 * SEC, &[BOB])))
        .await;
    let later = T0 + 1000 * SEC + t.settings.absent_expire_ms;
    let s = t.sweep(later, Some(&seen(later, &[BOB]))).await;
    assert_eq!(s.closed_without_time, 1);
    assert!(t.visits().await.is_empty());
    // its leave arriving now proves nothing, and the session never reopens
    t.leave(ALICE, "s1", later, At::Scene(SCENE)).await;
    t.join(ALICE, "s1", T0 + 10 * SEC, At::Scene(SCENE)).await;
    assert_eq!(t.progress("stay", ALICE).await, None);
    assert!(t.visits().await.is_empty());
}

#[tokio::test]
async fn a_listed_wallet_keeps_its_visit_and_no_information_changes_nothing() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "s1", T0, At::Scene(SCENE)).await;
    let early = t
        .sweep(T0 + 100 * SEC, Some(&seen(T0 + 100 * SEC, &[BOB])))
        .await;
    assert_eq!(early.checked, 0, "not open for the bound yet");
    let at = T0 + 1000 * SEC;
    assert_eq!(
        t.sweep(at, Some(&seen(at, &[ALICE]))).await.still_connected,
        1
    );
    assert_eq!(
        t.sweep(at + SEC, Some(&seen(at, &[BOB]))).await.checked,
        0,
        "checked recently"
    );
    let at = at + 1000 * SEC;
    assert_eq!(t.sweep(at, Some(&Seen(None))).await.retried, 1);
    let at = at + 1000 * SEC;
    assert_eq!(
        t.sweep(at, Some(&seen(at, &[]))).await.retried,
        1,
        "an empty list is no information"
    );
    assert_eq!(t.visits().await[0].3, None);
    t.leave(ALICE, "s1", T0 + 400 * SEC, At::Scene(SCENE)).await;
    assert!(t.progress("stay", ALICE).await.unwrap().2.is_some());
}

#[tokio::test]
async fn connections_past_the_maximum_age_are_forgotten_and_old_facts_pruned() {
    let Some(t) = T::new().await else { return };
    t.badge("stay", At::Scene(PARCEL), 300).await;
    t.join(ALICE, "old", T0, At::Scene(SCENE)).await;
    t.join(ALICE, "live", T0 + HOUR, At::Scene(SCENE)).await;
    let at = T0 + t.settings.max_age_ms + 10 * SEC;
    let s = t.sweep(at, None).await;
    assert_eq!(s.expired, 1);
    assert_eq!(t.visits().await[0].2, ["live"]);
    let persisted: (Vec<i64>, Vec<String>, i64) = sqlx::query_as(
        "SELECT joins, sessions, opened_at FROM social.presence_visits WHERE wallet = $1",
    )
    .bind(ALICE)
    .fetch_one(&t.pool)
    .await
    .unwrap();
    assert_eq!(persisted, (vec![T0 + HOUR], vec!["live".into()], T0 + HOUR));
    assert_eq!(
        t.progress("stay", ALICE).await,
        None,
        "expiry grants no badge time"
    );
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM social.presence_applied_events")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    let pruned = t
        .sweep(at + t.settings.applied_events_ttl_ms + 1, None)
        .await
        .pruned;
    assert_eq!(pruned as i64, applied, "every key is older than the window");
}

#[tokio::test]
async fn the_prune_continues_until_a_short_batch_up_to_its_bound() {
    let Some(mut t) = T::new().await else { return };
    t.settings.prune_batch = 1;
    let passes = super::sweeper::PRUNE_PASSES as i64;
    sqlx::query(
        "INSERT INTO social.presence_applied_events (event_id, applied_at) \
         SELECT 'key-' || i, $1 FROM generate_series(1, $2) i",
    )
    .bind(T0)
    .bind(passes + 5)
    .execute(&t.pool)
    .await
    .unwrap();
    let at = T0 + t.settings.applied_events_ttl_ms + 1;
    assert_eq!(t.sweep(at, None).await.pruned as i64, passes);
    assert_eq!(t.sweep(at, None).await.pruned, 5);
}

// ── the sink ────────────────────────────────────────────────────────────

impl T {
    /// Moves a badge's window to `[now + from_h, now + to_h)` of the real
    /// clock, which is what the sink's places read.
    async fn window_around_now(&self, id: &str, from_h: i32, to_h: i32) {
        sqlx::query(
            "UPDATE social.presence_badges SET \
               window_start = timezone('UTC', now()) + make_interval(hours => $2), \
               window_end = timezone('UTC', now()) + make_interval(hours => $3) \
             WHERE badge_id = $1",
        )
        .bind(id)
        .bind(from_h)
        .bind(to_h)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

fn room_event(identity: &str, event_id: &str) -> RoomEvent {
    RoomEvent {
        kind: RoomEventKind::Joined as i32,
        room: format!("world-{WORLD}"),
        room_type: RoomType::World as i32,
        realm_name: WORLD.into(),
        scene_id: String::new(),
        identity: identity.into(),
        participant_sid: "PA_1".into(),
        event_time_ms: T0 as u64,
        event_id: event_id.into(),
    }
}

#[test]
fn only_wallet_joins_and_leaves_of_scene_and_world_rooms_are_inputs() {
    let wallet = "0x00000000000000000000000000000000000A11CE";
    assert_eq!(
        PresenceEvent::from_room_event(&room_event(wallet, "EV")).map(|e| e.wallet),
        Some(ALICE.to_string())
    );
    for identity in [
        "authoritative-server",
        "stream:party",
        " 0x00000000000000000000000000000000000a11ce",
        "",
    ] {
        assert_eq!(
            PresenceEvent::from_room_event(&room_event(identity, "EV")),
            None,
            "{identity}"
        );
    }
    let mut unknown_room = room_event(wallet, "EV");
    unknown_room.room_type = RoomType::Unknown as i32;
    let mut no_session = room_event(wallet, "EV");
    no_session.participant_sid.clear();
    for event in [unknown_room, no_session, room_event(wallet, "")] {
        assert_eq!(PresenceEvent::from_room_event(&event), None);
    }
}

async fn stored_events(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT event_id FROM social.presence_events ORDER BY event_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_sink_stores_each_webhook_event_once_before_it_is_acknowledged() {
    let Some(t) = T::new().await else { return };
    t.badge("world", At::World(None), 0).await;
    t.window_around_now("world", -1, 1).await;
    let sink = super::EventSink::new(t.pool.clone(), Settings::default());
    for event in [
        room_event(ALICE, "EV_a"),
        room_event(ALICE, "EV_a"),
        room_event("authoritative-server", "EV_b"),
    ] {
        sink.store_room_event(&event).await.unwrap();
    }
    let rows: Vec<(String, String, String, String)> =
        sqlx::query_as("SELECT event_id, kind, room_type, session_id FROM social.presence_events")
            .fetch_all(&t.pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [("EV_a".into(), "join".into(), "world".into(), "PA_1".into())],
        "the redelivery is one row"
    );
}

#[tokio::test]
async fn a_failed_insert_fails_the_webhook() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(50))
        .connect_lazy("postgres://postgres@127.0.0.1:1/postgres")
        .unwrap();
    let sink = super::EventSink::new(pool, Settings::default());
    assert!(
        sink.store_room_event(&room_event(ALICE, "EV_a"))
            .await
            .is_err(),
        "LiveKit delivers it again"
    );
}

#[tokio::test]
async fn the_sink_stores_only_events_in_places_a_badge_could_count() {
    let Some(t) = T::new().await else { return };
    t.badge("scene", At::Scene(PARCEL), 0).await;
    t.window_around_now("scene", -1, 1).await;
    // a world badge whose window closed days ago counts nothing now
    t.badge("world", At::World(None), 0).await;
    t.window_around_now("world", -240, -216).await;
    let sink = super::EventSink::new(t.pool.clone(), Settings::default());
    let scene = |id: &str, scene: &str| RoomEvent {
        room: format!("scene-{scene}"),
        room_type: RoomType::Scene as i32,
        realm_name: REALM.into(),
        scene_id: scene.into(),
        ..room_event(ALICE, id)
    };
    for event in [
        room_event(ALICE, "EV_world"),
        scene("EV_scene", SCENE),
        scene("EV_redeployed", SCENE_REDEPLOYED),
        scene("EV_other", OTHER_SCENE),
    ] {
        sink.store_room_event(&event).await.unwrap();
    }
    assert_eq!(stored_events(&t.pool).await, ["EV_redeployed", "EV_scene"]);
}

/// Every join and leave the gatekeeper verified reaches the store, so the
/// sessions of a reconnect each count from their own join to their own leave.
#[tokio::test]
async fn a_reconnect_credits_the_old_session_when_its_leave_follows_the_new_join() {
    let Some(mut t) = T::new().await else { return };
    t.badge("stay", At::World(None), 1000).await;
    // the sink's snapshot reads the real clock
    t.window_around_now("stay", -2, 1).await;
    t.now = chrono::Utc::now().timestamp_millis();
    let b = t.now - HOUR;
    let sink = super::EventSink::new(t.pool.clone(), Settings::default());
    let event = |id: &str, join: bool, sid: &str, at: i64| RoomEvent {
        kind: if join {
            RoomEventKind::Joined
        } else {
            RoomEventKind::Left
        } as i32,
        participant_sid: sid.into(),
        event_time_ms: at as u64,
        ..room_event(ALICE, id)
    };
    for e in [
        event("EV_old_join", true, "PA_old", b),
        event("EV_new_join", true, "PA_new", b + 10 * SEC),
        event("EV_old_leave", false, "PA_old", b + 400 * SEC),
        // the leave of a third session arrives before its join and still pairs
        event("EV_late_leave", false, "PA_late", b + 700 * SEC),
        event("EV_late_join", true, "PA_late", b + 600 * SEC),
    ] {
        sink.store_room_event(&e).await.unwrap();
    }
    t.deliver(&[]).await;
    let (steps, doc, completed) = t.progress("stay", ALICE).await.unwrap();
    assert_eq!((steps, completed), (500, None));
    assert_eq!(
        doc["intervals"],
        json!([[b, b + 400 * SEC], [b + 600 * SEC, b + 700 * SEC]])
    );
    assert_eq!(t.visits().await[0].2, ["PA_new"]);
}

#[tokio::test]
async fn the_sink_keeps_events_of_a_scene_redeployed_after_its_snapshot() {
    let Some(t) = T::new().await else { return };
    t.badge("scene", At::Scene(PARCEL), 0).await;
    t.window_around_now("scene", -1, 1).await;
    let sink = super::EventSink::new(t.pool.clone(), Settings::default());
    let scene = |id: &str, scene: &str| RoomEvent {
        room: format!("scene-{scene}"),
        room_type: RoomType::Scene as i32,
        realm_name: REALM.into(),
        scene_id: scene.into(),
        ..room_event(ALICE, id)
    };
    assert_eq!(
        sink.record(PresenceEvent::from_room_event(&scene("EV_before", SCENE)).unwrap())
            .await
            .unwrap(),
        1,
        "the snapshot is read"
    );
    for (entity, pointers) in [
        (
            "bafkreiredeployafterdddddddddddddddd",
            vec![PARCEL, "10,21"],
        ),
        ("bafkreiunrelatedafterdeeeeeeeeeeeeee", vec!["-40,40"]),
    ] {
        sqlx::query(
            "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, \
               entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
             VALUES ($1, 'v3', 'scene', $2, '{}', now(), $3, now(), '[]')",
        )
        .bind(ALICE)
        .bind(entity)
        .bind(&pointers)
        .execute(&t.pool)
        .await
        .unwrap();
    }
    for event in [
        scene("EV_redeployed", "bafkreiredeployafterdddddddddddddddd"),
        scene("EV_unrelated", "bafkreiunrelatedafterdeeeeeeeeeeeeee"),
        // a scene this node has not synced yet: the engine decides
        scene("EV_unsynced", "bafkreinotsyncedyetfffffffffffffffff"),
    ] {
        sink.store_room_event(&event).await.unwrap();
    }
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM social.presence_events WHERE event_id <> 'EV_before' ORDER BY event_id",
    )
    .fetch_all(&t.pool)
    .await
    .unwrap();
    assert_eq!(stored, ["EV_redeployed", "EV_unsynced"]);
}
