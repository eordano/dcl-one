use super::*;
use crate::credits::{RawSettings, Settings, State};
use crate::test_db::OwnDatabase;

fn at(v: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(v, "%Y-%m-%d %H:%M").unwrap()
}

#[test]
fn reminders_follow_the_source_schedule() {
    // 2026-11-19 is a Thursday.
    assert!(!Kind::DoNotMissOut.due(at("2026-11-19 08:59")));
    assert!(Kind::DoNotMissOut.due(at("2026-11-19 09:00")));
    assert!(!Kind::DoNotMissOut.due(at("2026-11-20 10:00")));
    assert!(!Kind::CompleteGoals.due(at("2026-11-20 11:00")));
    assert!(Kind::CompleteGoals.due(at("2026-11-20 12:00")));
    assert!(Kind::Claim.due(at("2026-11-22 09:30")));
    assert!(!Kind::Claim.due(at("2026-11-21 09:30")));
    assert!(Kind::Usage.due(at("2026-11-17 09:00")));
    assert!(!Kind::Usage.due(at("2026-11-17 08:00")));
    assert!(Kind::Usage24Hours.due(at("2026-11-17 00:00")));
}

#[test]
fn keys_are_per_season_and_week_like_the_source() {
    assert_eq!(
        Kind::Claim.key("0xa", 3, Some(2)),
        "0xa-claim-credits-reminder-3-2"
    );
    assert_eq!(Kind::Usage.key("0xa", 3, None), "0xa-usage-reminder-3");
    assert_eq!(
        Kind::Usage24Hours.key("0xa", 3, None),
        "0xa-usage-24-hours-reminder-3"
    );
}

#[test]
fn balances_render_like_format_ether() {
    assert_eq!(ether_number("100000000000000000000").unwrap(), json!(100));
    assert_eq!(ether_number("1500000000000000000").unwrap(), json!(1.5));
    assert_eq!(ether_number("0").unwrap(), json!(0));
}

#[test]
fn the_gate_applies_the_switch_and_allowlist() {
    let open = Gate {
        disabled: false,
        allowlist: None,
    };
    assert_eq!(open.refusal("0xa"), None);
    let listed = Gate {
        disabled: false,
        allowlist: Some(vec!["0xa".into()]),
    };
    assert_eq!(listed.refusal("0xa"), None);
    assert_eq!(listed.refusal("0xb"), Some("not-allowed"));
    let off = Gate {
        disabled: true,
        allowlist: None,
    };
    assert_eq!(off.refusal("0xa"), Some("disabled"));
}

const MANAGER: &str = "0x00000000000000000000000000000000000000b1";
const PLAY: &str = "https://decentraland.org/play";
const SHOP: &str = "https://decentraland.org/shop";
/// Thursday of the season's third week (weeks start on Monday 2026-11-02).
fn now() -> NaiveDateTime {
    at("2026-11-19 10:00")
}
fn addr(n: u8) -> String {
    format!("0x{n:040x}")
}
fn state(pool: sqlx::PgPool, dapps: Value, explorer: Value) -> SharedState {
    let settings = Settings::parse(RawSettings {
        writes_enabled: true,
        credits_manager_contract: Some(MANAGER.into()),
        ..RawSettings::default()
    })
    .unwrap();
    let state = Arc::new(State::with_sources(
        pool,
        crate::http::Outbound::for_tests(),
        settings,
        Arc::new(crate::credits::tests::FakeMarket::default()),
        Arc::new(crate::credits::tests::MockBroker::configured()),
        None,
    ));
    state.goal_flags.set_documents(Some(dapps), Some(explorer));
    state
}
fn enabled(pool: &sqlx::PgPool) -> SharedState {
    state(
        pool.clone(),
        json!({"flags":{"dapps-credits-server":true},"variants":{}}),
        json!({"flags":{},"variants":{}}),
    )
}
async fn season(pool: &sqlx::PgPool, id: i32, start: &str, end: &str) {
    sqlx::query("INSERT INTO credits.seasons(id,name,start_date,end_date,max_mana_wei) VALUES($1,'Test',$2,$3,0)")
        .bind(id).bind(at(start)).bind(at(end)).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO credits.season_goals(season_id,goal_id,title,thumbnail,total_steps,reward_wei,start_week,end_week) VALUES($1,'take-photo','Take a photo','p.png',2,0,1,20)")
        .bind(id).execute(pool).await.unwrap();
}
async fn wallet(pool: &sqlx::PgPool, n: u8) -> String {
    let a = addr(n);
    sqlx::query("INSERT INTO credits.wallets(address) VALUES($1)")
        .bind(&a)
        .execute(pool)
        .await
        .unwrap();
    a
}
async fn progress(pool: &sqlx::PgPool, a: &str, season: i32, week: i32, steps: i32) {
    sqlx::query("INSERT INTO credits.goal_progress(address,season_id,week_id,goal_id,title,thumbnail,total_steps,reward_wei,completed_steps,completed_at) VALUES($1,$2,$3,'take-photo','Take a photo','p.png',2,0,$4,CASE WHEN $4=2 THEN $5 END)")
        .bind(a).bind(season).bind(week).bind(steps).bind(now()).execute(pool).await.unwrap();
}
async fn grant(
    pool: &sqlx::PgPool,
    a: &str,
    season: i32,
    week: i32,
    wei: &str,
    expires: NaiveDateTime,
    claimed: bool,
) {
    let salt = format!("0x{}{}", "0".repeat(24), &a[2..]);
    sqlx::query("INSERT INTO credits.goal_grants(address,season_id,week_id,goal_id,salt,chain_id,contract,credit_hash,amount_wei,signature,created_at,expires_at,claimed_at) VALUES($1,$2,$3,'take-photo',$4,137,$5,$4,$6::numeric,'0x',$7,$8,$9)")
        .bind(a).bind(season).bind(week).bind(&salt).bind(MANAGER).bind(wei).bind(now()).bind(expires).bind(claimed.then(now)).execute(pool).await.unwrap();
}
async fn sent(pool: &sqlx::PgPool) -> Vec<(String, String, Value)> {
    sqlx::query_as("SELECT type,address,metadata FROM social.notifications WHERE type LIKE 'credits_reminder_%' ORDER BY type,address")
        .fetch_all(pool).await.unwrap()
}
fn kinds(rows: &[(String, String, Value)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(t, a, _)| (t.clone(), a.clone()))
        .collect()
}
fn pair(t: &str, a: &str) -> (String, String) {
    (format!("credits_reminder_{t}"), a.to_string())
}

#[tokio::test]
async fn reminders_pick_eligible_wallets_once_with_source_metadata() {
    let Some(db) = OwnDatabase::create("credits_reminders").await else {
        return;
    };
    let pool = &db.pool;
    season(pool, 1, "2026-11-02 00:00", "2026-11-30 00:00").await;
    let untouched = wallet(pool, 1).await;
    progress(pool, &untouched, 1, 3, 0).await;
    let started = wallet(pool, 2).await;
    progress(pool, &started, 1, 3, 1).await;
    let unclaimed = wallet(pool, 3).await;
    progress(pool, &unclaimed, 1, 3, 2).await;
    grant(pool, &unclaimed, 1, 3, "1", at("2026-12-14 00:00"), false).await;
    let opted_out = wallet(pool, 4).await;
    progress(pool, &opted_out, 1, 3, 0).await;
    sqlx::query("INSERT INTO credits.program_opt_outs(address,opted_out_at) VALUES($1,$2)")
        .bind(&opted_out)
        .bind(now())
        .execute(pool)
        .await
        .unwrap();
    let tomorrow = wallet(pool, 5).await;
    progress(pool, &tomorrow, 1, 3, 2).await;
    let in_12h = now() + TimeDelta::hours(12);
    grant(pool, &tomorrow, 1, 3, "1500000000000000000", in_12h, true).await;
    let in_3d = now() + TimeDelta::days(3);
    let later = wallet(pool, 6).await;
    progress(pool, &later, 1, 3, 2).await;
    grant(pool, &later, 1, 3, "100000000000000000000", in_3d, true).await;
    let used = wallet(pool, 7).await;
    progress(pool, &used, 1, 3, 2).await;
    grant(pool, &used, 1, 3, "1000000000000000000", in_3d, true).await;
    let salt = format!("0x{}{}", "0".repeat(24), &used[2..]);
    sqlx::query("INSERT INTO credits.usages(chain_id,block_number,log_index,contract,tx_hash,sender,credit_id,salt,credit_value,credit_expires_at,value_wei) VALUES(137,1,0,$1,$2,$3,$2,$2,1,0,1)")
        .bind(MANAGER).bind(&salt).bind(&used).execute(pool).await.unwrap();
    // No progress row this week: dormant, or only last week's row.
    wallet(pool, 8).await;
    let last_week = wallet(pool, 9).await;
    progress(pool, &last_week, 1, 2, 0).await;
    let blocked = wallet(pool, 10).await;
    progress(pool, &blocked, 1, 3, 0).await;
    sqlx::query("UPDATE credits.wallets SET is_blocked_for_claiming=true WHERE address=$1")
        .bind(&blocked)
        .execute(pool)
        .await
        .unwrap();
    let flagged = wallet(pool, 11).await;
    progress(pool, &flagged, 1, 3, 0).await;
    sqlx::query("INSERT INTO credits.wallet_flags(address,source) VALUES($1,'test')")
        .bind(&flagged)
        .execute(pool)
        .await
        .unwrap();

    let state = enabled(pool);
    let gate = gate(&state, &mut pool.acquire().await.unwrap())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    for _ in 0..2 {
        for kind in Kind::ALL {
            run(&state, &gate, kind, now(), &cancel).await.unwrap();
        }
    }
    let rows = sent(pool).await;
    assert_eq!(
        kinds(&rows),
        vec![
            pair("claim_credits", &unclaimed),
            pair("complete_goals", &started),
            pair("do_not_miss_out", &untouched),
            pair("usage", &tomorrow),
            pair("usage", &later),
            pair("usage_24_hours", &tomorrow),
        ]
    );
    assert_eq!(rows[0].2, json!({"seasonId":1,"weekNumber":3,"link":PLAY}));
    assert_eq!(
        rows[1].2,
        json!({"seasonId":1,"weekNumber":3,"pendingGoalIds":["take-photo"],"link":PLAY})
    );
    assert_eq!(rows[2].2, json!({"link":PLAY}));
    assert_eq!(
        rows[3].2,
        json!({"expirationDate":"November 19","expirationDay":"Thursday","balance":1.5,"link":SHOP})
    );
    assert_eq!(
        rows[4].2,
        json!({"expirationDate":"November 22","expirationDay":"Sunday","balance":100,"link":SHOP})
    );
    assert_eq!(
        rows[5].2,
        json!({"expirationDate":"November 19","balance":1.5,"link":SHOP})
    );
    let keys: Vec<String> = sqlx::query_scalar("SELECT broadcast_key FROM social.notifications WHERE type LIKE 'credits_reminder_%' ORDER BY broadcast_key")
        .fetch_all(pool).await.unwrap();
    assert!(keys.contains(&format!("{untouched}-do-not-miss-out-reminder-1-3")));
    assert!(keys.contains(&format!("{tomorrow}-usage-24-hours-reminder-1")));
    assert_eq!(keys.len(), 6);

    // Weekly reminders stop once the season is over.
    sqlx::query("DELETE FROM social.notifications")
        .execute(pool)
        .await
        .unwrap();
    let after_end = at("2026-11-30 10:00");
    for kind in [Kind::DoNotMissOut, Kind::CompleteGoals, Kind::Claim] {
        assert_eq!(
            run(&state, &gate, kind, after_end, &cancel).await.unwrap(),
            0
        );
    }
    db.drop().await;
}

#[tokio::test]
async fn usage_reminders_follow_the_credits_season_and_the_allowlist() {
    let Some(db) = OwnDatabase::create("credits_reminders_gate").await else {
        return;
    };
    let pool = &db.pool;
    season(pool, 1, "2026-10-05 00:00", "2026-11-09 00:00").await;
    season(pool, 2, "2026-11-09 00:00", "2026-12-21 00:00").await;
    let listed = wallet(pool, 1).await;
    let other = wallet(pool, 2).await;
    for a in [&listed, &other] {
        progress(pool, a, 1, 5, 2).await;
        grant(
            pool,
            a,
            1,
            5,
            "2000000000000000000",
            now() + TimeDelta::days(2),
            true,
        )
        .await;
        progress(pool, a, 2, 2, 0).await;
    }
    let state = state(
        pool.clone(),
        json!({"flags":{"dapps-credits-server":true},"variants":{}}),
        json!({"flags":{"explorer-alfa-marketplace-credits":true},"variants":{"explorer-alfa-marketplace-credits":{"name":"wallets","enabled":true,"payload":{"type":"string","value":listed}}}}),
    );
    let gate = gate(&state, &mut pool.acquire().await.unwrap())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    for kind in [Kind::Usage, Kind::DoNotMissOut] {
        run(&state, &gate, kind, now(), &cancel).await.unwrap();
    }
    assert_eq!(
        kinds(&sent(pool).await),
        vec![pair("do_not_miss_out", &listed), pair("usage", &listed)]
    );
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT broadcast_key FROM social.notifications ORDER BY broadcast_key")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        keys,
        vec![
            format!("{listed}-do-not-miss-out-reminder-2-2"),
            format!("{listed}-usage-reminder-1"),
        ]
    );
    db.drop().await;
}

#[tokio::test]
async fn a_pass_runs_due_kinds_once_per_day() {
    let Some(db) = OwnDatabase::create("credits_reminders_pass").await else {
        return;
    };
    let pool = &db.pool;
    season(pool, 1, "2026-11-02 00:00", "2026-11-30 00:00").await;
    let a = wallet(pool, 1).await;
    progress(pool, &a, 1, 3, 0).await;
    let ran = Ran::default();
    let cancel = CancellationToken::new();
    let state = enabled(pool);
    let names = |done: Vec<(Kind, u64)>| done.into_iter().map(|(k, _)| k).collect::<Vec<_>>();

    assert_eq!(
        names(
            pass(&state, &ran, at("2026-11-19 08:00"), &cancel)
                .await
                .unwrap()
        ),
        vec![Kind::Usage24Hours]
    );
    let done = pass(&state, &ran, at("2026-11-19 09:05"), &cancel)
        .await
        .unwrap();
    assert_eq!(done, vec![(Kind::DoNotMissOut, 1), (Kind::Usage, 0)]);
    assert!(
        pass(&state, &ran, at("2026-11-19 23:00"), &cancel)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        names(
            pass(&state, &ran, at("2026-11-20 00:30"), &cancel)
                .await
                .unwrap()
        ),
        vec![Kind::Usage24Hours]
    );

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let fresh = Ran::default();
    assert!(
        pass(&state, &fresh, at("2026-11-20 13:00"), &cancelled)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(fresh.lock().is_empty());

    let off = state_off(pool);
    assert!(
        pass(&off, &fresh, at("2026-11-20 13:00"), &cancel)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(fresh.lock().is_empty());
    db.drop().await;
}
fn state_off(pool: &sqlx::PgPool) -> SharedState {
    state(
        pool.clone(),
        json!({"flags":{"dapps-credits-server":false},"variants":{}}),
        json!({"flags":{},"variants":{}}),
    )
}

/// Five wallets due a do-not-miss-out reminder this week.
async fn untouched_wallets(pool: &sqlx::PgPool) {
    season(pool, 1, "2026-11-02 00:00", "2026-11-30 00:00").await;
    for n in 1..=5 {
        let a = wallet(pool, n).await;
        progress(pool, &a, 1, 3, 0).await;
    }
}

#[tokio::test]
async fn a_failing_kind_does_not_stop_the_others() {
    let Some(db) = OwnDatabase::create("credits_reminders_fail").await else {
        return;
    };
    let pool = &db.pool;
    untouched_wallets(pool).await;
    let a = addr(1);
    grant(pool, &a, 1, 3, "1", now() + TimeDelta::days(2), true).await;
    sqlx::query("CREATE FUNCTION pg_temp_fail() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN IF NEW.type='credits_reminder_do_not_miss_out' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END$$")
        .execute(pool).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_reminder BEFORE INSERT ON social.notifications FOR EACH ROW EXECUTE FUNCTION pg_temp_fail()")
        .execute(pool).await.unwrap();
    let ran = Ran::default();
    let state = enabled(pool);
    let thursday = at("2026-11-19 09:05");
    assert!(
        pass(&state, &ran, thursday, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(kinds(&sent(pool).await), vec![pair("usage", &a)]);
    assert_eq!(ran.lock().get(&Kind::DoNotMissOut), None);
    assert_eq!(ran.lock().get(&Kind::Usage), Some(&thursday.date()));
    db.drop().await;
}

#[tokio::test]
async fn cancellation_stops_a_kind_between_pages() {
    let Some(db) = OwnDatabase::create("credits_reminders_cancel").await else {
        return;
    };
    let pool = &db.pool;
    untouched_wallets(pool).await;
    sqlx::query("CREATE FUNCTION pg_temp_slow() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN PERFORM pg_sleep(0.3); RETURN NEW; END$$")
        .execute(pool).await.unwrap();
    sqlx::query("CREATE TRIGGER slow_reminder BEFORE INSERT ON social.notifications FOR EACH ROW EXECUTE FUNCTION pg_temp_slow()")
        .execute(pool).await.unwrap();
    let ran = Ran::default();
    let state = enabled(pool);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        stop.cancel();
    });
    let done = pass(&state, &ran, at("2026-11-19 09:05"), &cancel)
        .await
        .unwrap();
    assert!(done.is_empty());
    assert!(ran.lock().is_empty());
    // At most the page in flight when cancelled (two rows under test).
    assert!(sent(pool).await.len() <= 2);
    db.drop().await;
}
