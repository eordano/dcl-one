use super::*;
#[test]
fn photo_requires_the_owner_and_another_emoting_person() {
    let payload = json!({"metadata":{"isPublic":false,"users":[{"address":"0xABC","isEmoting":true},{"address":"0xdef","isEmoting":true}]}});
    assert!(qualifies(&payload, "0xabc"));
    assert!(!qualifies(&payload, "0x999"));
    assert!(!qualifies(
        &json!({"metadata":{"users":[{"address":"0xABC","isEmoting":true},{"address":"0xabc","isEmoting":true}]}}),
        "0xabc"
    ));
}
#[test]
fn wei_above_i64_is_exact_until_wire_presentation() {
    let amount = "100000000000000000000";
    assert!(uint(amount).unwrap() > U256::from(i64::MAX as u64));
    assert_eq!(uint(amount).unwrap().to_string(), amount);
    assert_eq!(wire_number(amount).unwrap(), 1e20);
    assert_eq!(mana_number(amount).unwrap(), 100.0);
}
#[test]
fn monday_weeks_are_clamped_to_the_season() {
    let date = |v: &str| NaiveDateTime::parse_from_str(v, "%Y-%m-%d %H:%M:%S").unwrap();
    let season = Season {
        id: 1,
        name: "test".into(),
        start_date: date("2026-11-04 12:00:00"),
        end_date: date("2026-11-17 12:00:00"),
        max_mana_wei: "0".into(),
        grace_days: 14,
    };
    assert_eq!(
        season.week(date("2026-11-05 12:00:00")),
        (1, season.start_date, date("2026-11-09 00:00:00"))
    );
    assert_eq!(
        season.week(date("2026-11-16 00:00:00")),
        (3, date("2026-11-16 00:00:00"), season.end_date)
    );
}

use crate::credits::{RawSettings, Settings, State};
use crate::test_db::OwnDatabase;
const OWNER: &str = "0x00000000000000000000000000000000000000a1";
const OTHER: &str = "0x00000000000000000000000000000000000000a2";
const MANAGER: &str = "0x00000000000000000000000000000000000000b1";
fn fixture_state(pool: sqlx::PgPool, signing: bool) -> SharedState {
    let settings = Settings::parse(RawSettings {
        writes_enabled: true,
        credits_signer_key: signing.then(|| "11".repeat(32)),
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
    state.goal_flags.set_documents(
        Some(json!({"flags":{"dapps-credits-server":true},"variants":{}})),
        Some(json!({"flags":{},"variants":{}})),
    );
    state
}
async fn seed(pool: &sqlx::PgPool, steps: i32, reward: &str, cap: &str) {
    sqlx::query("INSERT INTO explore_api.feature_flags(app,document,fetched_at) VALUES('dapps',$1,timezone('UTC',now())),('explorer',$2,timezone('UTC',now())) ON CONFLICT(app) DO UPDATE SET document=excluded.document")
        .bind(json!({"flags":{"dapps-credits-server":true},"variants":{}})).bind(json!({"flags":{},"variants":{}})).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO credits.seasons(id,name,start_date,end_date,max_mana_wei) VALUES(1,'Test',timezone('UTC',now())-interval '1 day',timezone('UTC',now())+interval '21 days',$1::numeric)").bind(cap).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO credits.season_goals(season_id,goal_id,title,thumbnail,total_steps,reward_wei,start_week,end_week) VALUES(1,'take-photo','Take a photo','photo.png',$1,$2::numeric,1,20)").bind(steps).bind(reward).execute(pool).await.unwrap();
    for address in [OWNER, OTHER] {
        sqlx::query("INSERT INTO credits.wallets(address,has_started_program) VALUES($1,true)")
            .bind(address)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO social.subscriptions(address,email) VALUES($1,NULL)")
            .bind(address)
            .execute(pool)
            .await
            .unwrap();
    }
}
fn payload(owner: &str) -> Value {
    json!({"type":"camera","subType":"photo-taken","metadata":{"userAddress":owner,"isPublic":false,"users":[{"address":owner.to_uppercase(),"isEmoting":true},{"address":"0x00000000000000000000000000000000000000ff","isEmoting":true}]}})
}
async fn photo<'e>(executor: impl sqlx::PgExecutor<'e>, owner: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO camera_reel.photo_events(kind,photo_id,user_address,payload) VALUES('photo-taken',$1,$2,$3) RETURNING id")
        .bind(Uuid::new_v4()).bind(owner).bind(payload(owner)).fetch_one(executor).await.unwrap()
}
async fn count(pool: &sqlx::PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn photo_completion_claim_signature_consumption_and_usd_isolation() {
    let Some(db) = OwnDatabase::create("photo_goal_lifecycle").await else {
        return;
    };
    seed(
        &db.pool,
        2,
        "100000000000000000000",
        "1000000000000000000000",
    )
    .await;
    let state = fixture_state(db.pool.clone(), true);
    let first = photo(&db.pool, OWNER).await;
    assert!(
        process_event(&state, first, super::super::store::now())
            .await
            .unwrap()
    );
    assert!(
        !process_event(&state, first, super::super::store::now())
            .await
            .unwrap()
    );
    let second = photo(&db.pool, OWNER).await;
    let (a, b) = tokio::join!(
        process_event(&state, second, super::super::store::now()),
        process_event(&state, second, super::super::store::now())
    );
    assert_eq!(u8::from(a.unwrap()) + u8::from(b.unwrap()), 1);
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        1
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.ledger").await,
        0
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM social.notifications WHERE type='credits_goal_completed'"
        )
        .await,
        1
    );
    assert_eq!(count(&db.pool,"SELECT count(*) FROM social.notification_deliveries WHERE state='skipped' AND last_error='no email template'").await,1);
    let grant = sqlx::query("SELECT * FROM credits.goal_grants")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let hash = B256::from_str(&grant.get::<String, _>("credit_hash")).unwrap();
    let signer = TreasuryWallet::from_hex(&"11".repeat(32)).unwrap();
    assert_eq!(
        super::super::digest::recover_credit_signer(hash, &grant.get::<String, _>("signature")),
        Some(signer.signer())
    );
    assert!(
        user_credits(&state, OWNER, None)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert_eq!(claim(&state, OWNER).await.unwrap(), 100.0);
    assert!(claim(&state, OWNER).await.is_err());
    let (items, total, _) = user_credits(&state, OWNER, None).await.unwrap();
    assert_eq!(total, 1e20);
    assert_eq!(items[0].amount, "100000000000000000000");
    assert!(opt_out(&state, OWNER).await.is_err());
    let report = progress(&state, OWNER).await.unwrap();
    assert_eq!(report.credits.available, 100.0);
    assert!(report.goals[0].is_claimed);
    assert!(report.goals[0].is_completed);
    assert_eq!(report.goals[0].progress.metadata.photos_taken.len(), 2);
    sqlx::query("INSERT INTO credits.usages(chain_id,block_number,log_index,contract,tx_hash,sender,credit_id,salt,credit_value,credit_expires_at,value_wei) SELECT chain_id,1,1,contract,$1,address,credit_hash,salt,amount_wei,extract(epoch FROM expires_at),40000000000000000000 FROM credits.goal_grants")
        .bind(format!("0x{}","12".repeat(32))).execute(&db.pool).await.unwrap();
    let (items, total, _) = user_credits(&state, OWNER, None).await.unwrap();
    assert_eq!(total, 6e19);
    assert_eq!(items[0].available_amount, "60000000000000000000");
    assert_eq!(items[0].status, "PARTIALLY_USED");
    assert!(
        super::super::settlement::settle(
            &state.credits,
            state.settings.usage.chain_id,
            MANAGER,
            0,
            1
        )
        .await
        .unwrap()
        .is_empty(),
        "seasonal usage must not settle as a USD purchase pledge"
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.ledger").await,
        0
    );
    sqlx::query("INSERT INTO credits.usages(chain_id,block_number,log_index,contract,tx_hash,sender,credit_id,salt,credit_value,credit_expires_at,value_wei) SELECT chain_id+1,1,1,contract,$1,address,credit_hash,salt,amount_wei,extract(epoch FROM expires_at),amount_wei FROM credits.goal_grants").bind(format!("0x{}","13".repeat(32))).execute(&db.pool).await.unwrap();
    assert_eq!(
        user_credits(&state, OWNER, None).await.unwrap().1,
        6e19,
        "another chain cannot consume this signature"
    );
    sqlx::query("INSERT INTO camera_reel.photo_events(kind,photo_id,user_address,payload) SELECT 'photo-privacy-changed',photo_id,user_address,'{}'::jsonb FROM camera_reel.photo_events LIMIT 1").execute(&db.pool).await.unwrap();
    assert_eq!(process_batch(&state, 100).await.unwrap(), 0);
    sqlx::query("DELETE FROM camera_reel.photo_events")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        progress(&state, OWNER).await.unwrap().goals[0]
            .progress
            .completed_steps,
        2
    );
    let available: String =
        sqlx::query_scalar("SELECT available::text FROM credits.wallets WHERE address=$1")
            .bind(OWNER)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(available, "0");
    db.drop().await;
}

#[tokio::test]
async fn completion_rolls_back_without_signer_and_late_commits_are_not_skipped() {
    let Some(db) = OwnDatabase::create("photo_goal_recovery").await else {
        return;
    };
    seed(&db.pool, 1, "1000000000000000000", "1000000000000000000000").await;
    let missing = fixture_state(db.pool.clone(), false);
    let mut late = db.pool.begin().await.unwrap();
    let older = photo(&mut *late, OWNER).await;
    let newer = photo(&db.pool, OTHER).await;
    assert!(
        process_event(&missing, newer, super::super::store::now())
            .await
            .is_err()
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.photo_goal_receipts").await,
        0
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_progress").await,
        0
    );
    let signed = fixture_state(db.pool.clone(), true);
    assert_eq!(process_batch(&signed, 100).await.unwrap(), 1);
    late.commit().await.unwrap();
    assert!(older < newer);
    // The late commit sits behind the cursor; the next reconcile round, five
    // minutes after the last one, picks it up.
    assert_eq!(process_batch(&signed, 100).await.unwrap(), 0);
    signed
        .goal_scan_reconcile_at_ms
        .store(0, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(process_batch(&signed, 100).await.unwrap(), 1);
    assert_eq!(process_batch(&signed, 100).await.unwrap(), 0);
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        2
    );
    db.drop().await;
}

#[tokio::test]
async fn weekly_and_season_claim_caps_serialize_across_wallets() {
    let Some(db) = OwnDatabase::create("photo_goal_caps").await else {
        return;
    };
    seed(
        &db.pool,
        1,
        "25000000000000000000000",
        "100000000000000000000000",
    )
    .await;
    let state = fixture_state(db.pool.clone(), true);
    photo(&db.pool, OWNER).await;
    photo(&db.pool, OTHER).await;
    process_batch(&state, 100).await.unwrap();
    let (a, b) = tokio::join!(claim(&state, OWNER), claim(&state, OTHER));
    assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1);
    let unclaimed = if a.is_ok() { OTHER } else { OWNER };
    sqlx::query("UPDATE credits.seasons SET max_mana_wei=25000000000000000000000")
        .execute(&db.pool)
        .await
        .unwrap();
    let error = claim(&state, unclaimed).await.unwrap_err();
    assert!(error.to_string().contains("cap"));
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM credits.goal_grants WHERE claimed_at IS NOT NULL"
        )
        .await,
        1
    );
    db.drop().await;
}

#[tokio::test]
async fn terminal_skips_do_not_replay_after_policy_or_season_changes() {
    let Some(db) = OwnDatabase::create("photo_goal_skips").await else {
        return;
    };
    let state = fixture_state(db.pool.clone(), true);
    let event = photo(&db.pool, OWNER).await;
    process_event(&state, event, super::super::store::now())
        .await
        .unwrap();
    seed(&db.pool, 1, "1000000000000000000", "1000000000000000000000").await;
    assert_eq!(process_batch(&state, 100).await.unwrap(), 0);
    for (sql, reason) in [
        (
            "INSERT INTO explore_api.feature_flags_local(app,name,value) VALUES('dapps','dapps-credits-server',false) ON CONFLICT(app,name) DO UPDATE SET value=false",
            "disabled",
        ),
        (
            "UPDATE credits.wallets SET has_started_program=false",
            "not-enrolled",
        ),
        (
            "INSERT INTO credits.wallet_flags(address,source) VALUES('0x00000000000000000000000000000000000000a1','test')",
            "flagged",
        ),
        (
            "DELETE FROM social.subscriptions WHERE address='0x00000000000000000000000000000000000000a1'",
            "no-subscription",
        ),
    ] {
        sqlx::query(sql).execute(&db.pool).await.unwrap();
        let event = photo(&db.pool, OWNER).await;
        process_event(&state, event, super::super::store::now())
            .await
            .unwrap();
        let actual: String =
            sqlx::query_scalar("SELECT outcome FROM credits.photo_goal_receipts WHERE event_id=$1")
                .bind(event)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(actual, reason);
        sqlx::query("DELETE FROM explore_api.feature_flags_local WHERE app='dapps' AND name='dapps-credits-server'").execute(&db.pool).await.unwrap();
        sqlx::query("UPDATE credits.wallets SET has_started_program=true")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM credits.wallet_flags")
            .execute(&db.pool)
            .await
            .unwrap();
    }
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        0
    );
    db.drop().await;
}

#[tokio::test]
async fn notification_failure_rolls_back_completion_and_concurrent_photos_count_once() {
    let Some(db) = OwnDatabase::create("photo_goal_atomic").await else {
        return;
    };
    seed(&db.pool, 2, "1000000000000000000", "1000000000000000000000").await;
    let state = fixture_state(db.pool.clone(), true);
    let first = photo(&db.pool, OWNER).await;
    process_event(&state, first, super::super::store::now())
        .await
        .unwrap();
    sqlx::query("ALTER TABLE social.notifications ADD CONSTRAINT refuse_photo_test CHECK(type <> 'credits_goal_completed')").execute(&db.pool).await.unwrap();
    let second = photo(&db.pool, OWNER).await;
    assert!(
        process_event(&state, second, super::super::store::now())
            .await
            .is_err()
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.photo_goal_receipts").await,
        1
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        0
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT completed_steps::bigint FROM credits.goal_progress"
        )
        .await,
        1
    );
    sqlx::query("ALTER TABLE social.notifications DROP CONSTRAINT refuse_photo_test")
        .execute(&db.pool)
        .await
        .unwrap();
    let third = photo(&db.pool, OWNER).await;
    let (a, b) = tokio::join!(
        process_event(&state, second, super::super::store::now()),
        process_event(&state, third, super::super::store::now())
    );
    assert!(a.unwrap() && b.unwrap());
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        1
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM credits.photo_goal_receipts WHERE outcome='already-complete'"
        )
        .await,
        1
    );
    db.drop().await;
}

#[tokio::test]
async fn enrollment_opt_out_and_missing_policy_remain_separate_from_paid_usd() {
    let Some(db) = OwnDatabase::create("photo_goal_enrollment").await else {
        return;
    };
    seed(&db.pool, 1, "1000000000000000000", "1000000000000000000000").await;
    let state = fixture_state(db.pool.clone(), true);
    assert!(enroll(&state, OWNER).await.is_err());
    sqlx::query("UPDATE social.subscriptions SET email='test@example.org' WHERE address=$1")
        .bind(OWNER)
        .execute(&db.pool)
        .await
        .unwrap();
    enroll(&state, OWNER).await.unwrap();
    opt_out(&state, OWNER).await.unwrap();
    assert_eq!(
        status(&state, OWNER).await.unwrap()["data"]["status"],
        "opted_out"
    );
    let id = photo(&db.pool, OWNER).await;
    process_event(&state, id, super::super::store::now())
        .await
        .unwrap();
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        0
    );
    enroll(&state, OWNER).await.unwrap();
    assert_eq!(
        status(&state, OWNER).await.unwrap()["data"]["status"],
        "enrolled"
    );
    state
        .goal_flags
        .set_documents(None, Some(json!({"flags":{},"variants":{}})));
    let id = photo(&db.pool, OWNER).await;
    assert!(
        process_event(&state, id, super::super::store::now())
            .await
            .is_err()
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.photo_goal_receipts").await,
        1
    );
    sqlx::query("INSERT INTO explore_api.feature_flags_local(app,name,value) VALUES('dapps','dapps-credits-server',false)").execute(&db.pool).await.unwrap();
    assert!(
        process_event(&state, id, super::super::store::now())
            .await
            .unwrap()
    );
    db.drop().await;
}

#[tokio::test]
async fn photo_allowlist_assignment_and_emote_gates_are_terminal() {
    let Some(db) = OwnDatabase::create("photo_goal_gates").await else {
        return;
    };
    seed(&db.pool, 1, "1000000000000000000", "1000000000000000000000").await;
    let state = fixture_state(db.pool.clone(), true);
    state.goal_flags.set_documents(Some(json!({"flags":{"dapps-credits-server":true},"variants":{}})),Some(json!({"flags":{"explorer-alfa-marketplace-credits":true},"variants":{"explorer-alfa-marketplace-credits":{"payload":{"value":OTHER}}}})));
    let id = photo(&db.pool, OWNER).await;
    process_event(&state, id, super::super::store::now())
        .await
        .unwrap();
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM credits.photo_goal_receipts WHERE outcome='not-allowed'"
        )
        .await,
        1
    );
    state.goal_flags.set_documents(
        Some(json!({"flags":{"dapps-credits-server":true},"variants":{}})),
        Some(json!({"flags":{},"variants":{}})),
    );
    sqlx::query("UPDATE credits.season_goals SET start_week=10,end_week=20")
        .execute(&db.pool)
        .await
        .unwrap();
    let id = photo(&db.pool, OWNER).await;
    process_event(&state, id, super::super::store::now())
        .await
        .unwrap();
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM credits.photo_goal_receipts WHERE outcome='no-goal'"
        )
        .await,
        1
    );
    sqlx::query("UPDATE credits.season_goals SET start_week=1")
        .execute(&db.pool)
        .await
        .unwrap();
    let id = photo(&db.pool, OWNER).await;
    sqlx::query("UPDATE camera_reel.photo_events SET payload=$1 WHERE id=$2").bind(json!({"metadata":{"users":[{"address":OWNER,"isEmoting":false},{"address":OTHER,"isEmoting":true}]}})).bind(id).execute(&db.pool).await.unwrap();
    process_event(&state, id, super::super::store::now())
        .await
        .unwrap();
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM credits.photo_goal_receipts WHERE outcome='not-qualifying'"
        )
        .await,
        1
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        0
    );
    db.drop().await;
}

/// Existing HTTP claim tests use an isolated DB and a real signed photo reward.
pub(crate) async fn seed_http_claim(pool: &sqlx::PgPool, address: &str) {
    seed(pool, 1, "1000000000000000000", "1000000000000000000000").await;
    sqlx::query("INSERT INTO credits.wallets(address,has_started_program) VALUES($1,true) ON CONFLICT(address) DO UPDATE SET has_started_program=true").bind(address).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO social.subscriptions(address) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(address)
        .execute(pool)
        .await
        .unwrap();
    let state = fixture_state(pool.clone(), true);
    let event = photo(pool, address).await;
    process_event(&state, event, super::super::store::now())
        .await
        .unwrap();
}

#[tokio::test]
async fn season_cap_and_expired_grants_use_exact_wei_and_signed_expiry() {
    let Some(db) = OwnDatabase::create("photo_goal_season_cap").await else {
        return;
    };
    seed(
        &db.pool,
        1,
        "100000000000000000000",
        "150000000000000000000",
    )
    .await;
    let state = fixture_state(db.pool.clone(), true);
    photo(&db.pool, OWNER).await;
    photo(&db.pool, OTHER).await;
    process_batch(&state, 100).await.unwrap();
    let (a, b) = tokio::join!(claim(&state, OWNER), claim(&state, OTHER));
    assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1);
    let error = if let Err(e) = a { e } else { b.unwrap_err() };
    assert!(matches!(error, ApiError::ClaimCap { weekly: false }));
    db.drop().await;

    let Some(db) = OwnDatabase::create("photo_goal_expiry").await else {
        return;
    };
    seed(
        &db.pool,
        1,
        "100000000000000000000",
        "150000000000000000000",
    )
    .await;
    let state = fixture_state(db.pool.clone(), true);
    let now = super::super::store::now();
    sqlx::query("UPDATE credits.seasons SET start_date=$1,end_date=$2")
        .bind(now - TimeDelta::days(30))
        .bind(now - TimeDelta::days(15))
        .execute(&db.pool)
        .await
        .unwrap();
    let id = photo(&db.pool, OWNER).await;
    // The season and week come from the photo's time, not the processing time.
    sqlx::query("UPDATE camera_reel.photo_events SET created_at=$2 WHERE id=$1")
        .bind(id)
        .bind((now - TimeDelta::days(20)).and_utc())
        .execute(&db.pool)
        .await
        .unwrap();
    process_event(&state, id, now - TimeDelta::days(20))
        .await
        .unwrap();
    sqlx::query("UPDATE credits.goal_grants SET claimed_at=created_at")
        .execute(&db.pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT *,amount_wei::text AS amount FROM credits.goal_grants")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let identity = CreditIdentity {
        sender: Address::from_str(OWNER).unwrap(),
        chain_id: row.get::<i64, _>("chain_id") as u64,
        manager: Address::from_str(MANAGER).unwrap(),
        value: uint(&row.get::<String, _>("amount")).unwrap(),
        expires_at: row
            .get::<NaiveDateTime, _>("expires_at")
            .and_utc()
            .timestamp() as u64,
        salt: B256::from_str(&row.get::<String, _>("salt")).unwrap(),
    };
    assert_eq!(
        format!("{:#x}", identity.credit_hash()),
        row.get::<String, _>("credit_hash")
    );
    assert_eq!(
        identity.expires_at,
        (now - TimeDelta::days(1)).and_utc().timestamp() as u64
    );
    let (items, total, expires) = user_credits(&state, OWNER, None).await.unwrap();
    assert!(items.is_empty());
    assert_eq!(total, 0.0);
    assert_eq!(expires, 0);
    assert!(claim(&state, OWNER).await.is_err());
    assert!(progress(&state, OWNER).await.unwrap().goals.is_empty());
    db.drop().await;
}

#[tokio::test]
async fn captcha_requires_solve_time_and_blocks_three_failures_for_a_day() {
    use super::super::captcha::{self, Challenge};
    let Some(db) = OwnDatabase::create("photo_goal_captcha_limits").await else {
        return;
    };
    use chrono::Timelike;
    // Deliberately retain sub-microsecond precision that PostgreSQL drops.
    let now = super::super::store::now()
        .with_nanosecond(123_456_789)
        .unwrap();
    let answer = match captcha::challenge(&db.pool, OWNER, now).await.unwrap() {
        Challenge::Ready(answer) => answer,
        _ => panic!("unexpected block"),
    };
    assert_eq!(
        captcha::solve(&db.pool, OWNER, answer, now + TimeDelta::seconds(1))
            .await
            .unwrap(),
        (false, false),
        "one second exactly is too fast"
    );
    for (offset, expected_block) in [(2, false), (5, true)] {
        assert!(matches!(
            captcha::challenge(&db.pool, OWNER, now + TimeDelta::seconds(offset))
                .await
                .unwrap(),
            Challenge::Ready(_)
        ));
        // Both operands are exactly representable: a random decimal plus 3.0
        // can subtract to 2.999999999999993 in IEEE-754 arithmetic.
        sqlx::query("UPDATE credits.captcha_challenges SET answer_x=50 WHERE address=$1 AND consumed_at IS NULL").bind(OWNER).execute(&db.pool).await.unwrap();
        assert_eq!(
            captcha::solve(&db.pool, OWNER, 53.0, now + TimeDelta::seconds(offset + 2))
                .await
                .unwrap(),
            (false, expected_block),
            "three units exactly fails"
        );
    }
    assert!(matches!(
        captcha::challenge(&db.pool, OWNER, now + TimeDelta::seconds(8))
            .await
            .unwrap(),
        Challenge::Blocked(_)
    ));
    assert_eq!(
        captcha::solve(&db.pool, OWNER, 0.0, now + TimeDelta::seconds(9))
            .await
            .unwrap(),
        (false, true)
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT failed_attempts::bigint FROM credits.captcha_attempts"
        )
        .await,
        3
    );
    let tomorrow = now + TimeDelta::days(1) + TimeDelta::seconds(8);
    let answer = match captcha::challenge(&db.pool, OWNER, tomorrow).await.unwrap() {
        Challenge::Ready(answer) => answer,
        _ => panic!("block did not expire"),
    };
    let repeated = match captcha::challenge(&db.pool, OWNER, tomorrow + TimeDelta::seconds(1))
        .await
        .unwrap()
    {
        Challenge::Ready(answer) => answer,
        _ => panic!("unexpected block"),
    };
    assert_eq!(answer, repeated, "GET reuses and refreshes the challenge");
    assert_eq!(
        captcha::solve(
            &db.pool,
            OWNER,
            answer,
            tomorrow + TimeDelta::milliseconds(2001)
        )
        .await
        .unwrap(),
        (true, false)
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.captcha_attempts").await,
        0
    );
    db.drop().await;
}

#[tokio::test]
async fn feature_flags_use_the_credits_referer_and_not_the_social_mirror() {
    use axum::{Json, Router, extract::Path, http::HeaderMap, routing::get};
    async fn serve(Path(app): Path<String>, headers: HeaderMap) -> Json<Value> {
        let credits = headers.get("referer").and_then(|v| v.to_str().ok())
            == Some("https://credits.example.org");
        Json(if app == "dapps.json" {
            json!({"flags":{"dapps-credits-server":credits},"variants":{}})
        } else {
            json!({"flags":{"explorer-alfa-marketplace-credits":true},"variants":{"explorer-alfa-marketplace-credits":{"payload":{"value":if credits{OWNER}else{OTHER}}}}})
        })
    }
    let Some(db) = OwnDatabase::create("photo_goal_flags_context").await else {
        return;
    };
    seed(&db.pool, 1, "1000000000000000000", "1000000000000000000000").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/{app}", get(serve)))
            .await
            .unwrap()
    });
    let mut state = fixture_state(db.pool.clone(), true);
    Arc::get_mut(&mut state).unwrap().goal_flags = flags::Flags::new(
        flags::Source {
            url: base.clone(),
            origin: "credits.example.org".into(),
            ..Default::default()
        }
        .normalize()
        .unwrap(),
        crate::http::Outbound::for_tests(),
    );
    state.goal_flags.refresh().await.unwrap();
    sqlx::query("UPDATE explore_api.feature_flags SET document=$1")
        .bind(json!({"flags":{"dapps-credits-server":false},"variants":{}}))
        .execute(&db.pool)
        .await
        .unwrap();
    let owner = photo(&db.pool, OWNER).await;
    let other = photo(&db.pool, OTHER).await;
    process_event(&state, owner, super::super::store::now())
        .await
        .unwrap();
    process_event(&state, other, super::super::store::now())
        .await
        .unwrap();
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        1
    );
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM credits.photo_goal_receipts WHERE outcome='not-allowed'"
        )
        .await,
        1
    );
    // An observed disabled Credits-context response is terminal even without Explorer.
    state.goal_flags.set_documents(
        Some(json!({"flags":{"dapps-credits-server":false},"variants":{}})),
        None,
    );
    let id = photo(&db.pool, OWNER).await;
    process_event(&state, id, super::super::store::now())
        .await
        .unwrap();
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM credits.photo_goal_receipts WHERE outcome='disabled'"
        )
        .await,
        1
    );
    state.goal_flags.set_documents(
        Some(json!({"flags":{"dapps-credits-server":true},"variants":{}})),
        None,
    );
    let id = photo(&db.pool, OWNER).await;
    assert!(
        process_event(&state, id, super::super::store::now())
            .await
            .is_err()
    );
    server.abort();
    db.drop().await;
}

#[tokio::test]
async fn retryable_events_do_not_starve_later_photos() {
    let Some(db) = OwnDatabase::create("photo_goal_fairness").await else {
        return;
    };
    seed(&db.pool, 1, "1000000000000000000", "1000000000000000000000").await;
    let missing = fixture_state(db.pool.clone(), false);
    // Signing failures remain unreceipted; another wallet still gets its terminal
    // eligibility decision beyond a full failing batch, and a restart retries all.
    for _ in 0..3 {
        photo(&db.pool, OWNER).await;
    }
    sqlx::query("UPDATE credits.wallets SET has_started_program=false WHERE address=$1")
        .bind(OTHER)
        .execute(&db.pool)
        .await
        .unwrap();
    photo(&db.pool, OTHER).await;
    assert!(process_batch(&missing, 2).await.is_err());
    assert!(process_batch(&missing, 2).await.is_err());
    assert_eq!(
        count(
            &db.pool,
            "SELECT count(*) FROM credits.photo_goal_receipts WHERE outcome='not-enrolled'"
        )
        .await,
        1
    );
    let resumed = fixture_state(db.pool.clone(), true);
    assert_eq!(process_batch(&resumed, 100).await.unwrap(), 3);
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        1
    );
    db.drop().await;
}

#[tokio::test]
async fn signed_http_progress_claim_credits_and_native_aliases_keep_source_units() {
    use super::super::tests::{call, scratch_wallet, seed_wallet, send};
    use axum::http::{HeaderMap, StatusCode};
    let Some(db) = OwnDatabase::create("photo_goal_http").await else {
        return;
    };
    let wallet = scratch_wallet();
    let address = &wallet.address;
    seed_wallet(&db.pool, address, "0", "12").await;
    seed(
        &db.pool,
        1,
        "100000000000000000000",
        "1000000000000000000000",
    )
    .await;
    sqlx::query("UPDATE credits.wallets SET has_started_program=true WHERE address=$1")
        .bind(address)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO social.subscriptions(address,email) VALUES($1,'test@example.org')")
        .bind(address)
        .execute(&db.pool)
        .await
        .unwrap();
    let state = fixture_state(db.pool.clone(), true);
    let id = photo(&db.pool, address).await;
    process_event(&state, id, super::super::store::now())
        .await
        .unwrap();
    let storage_root =
        std::env::temp_dir().join(format!("shop-goals-http-{}", uuid::Uuid::new_v4()));
    let app = crate::paths::tests::mounted_with_credits(&storage_root, state.clone()).await;
    let (status, body) = call(
        &app,
        &wallet,
        "get",
        &format!("/credits/users/{address}/progress"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["season"]["seasonState"], "RUNNING");
    assert!(body["currentWeek"]["timeLeft"].as_i64().unwrap() > 0);
    assert_eq!(body["credits"]["available"], 0);
    assert_eq!(body["goals"][0]["id"], "take-photo");
    assert_eq!(body["goals"][0]["reward"], 100);
    assert_eq!(body["goals"][0]["description"], Value::Null);
    assert_eq!(body["goals"][0]["isCompleted"], true);
    assert_eq!(body["goals"][0]["isClaimed"], false);
    assert!(body["goals"][0]["completedAt"].is_number());
    assert_eq!(
        body["goals"][0]["progress"]["metadata"]["photosTaken"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (status, body) = call(
        &app,
        &wallet,
        "get",
        &format!("/shop-api/v1/users/{address}/status"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"ok":true,"data":{"status":"enrolled","optedOutAt":null}})
    );
    sqlx::query("INSERT INTO credits.captcha_challenges(address,answer_x,generated_at,expires_at) VALUES($1,50,timezone('UTC',now())-interval '2 seconds',timezone('UTC',now())+interval '2 hours')").bind(address).execute(&db.pool).await.unwrap();
    let (status, body) = call(
        &app,
        &wallet,
        "post",
        "/credits/captcha",
        &[],
        Some(json!({"x":50})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["credits_granted"], 100);
    assert_eq!(body["message"], "Credit successfully claimed");
    let (status, body) = call(
        &app,
        &wallet,
        "get",
        &format!("/shop-api/v1/users/{address}/credits"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totalCredits"].as_f64(), Some(1e20));
    assert_eq!(body["totals"]["expiring"].as_f64(), Some(1e20));
    assert_eq!(body["usd"], json!({"balanceCents":120,"credits":12}));
    let grant = &body["credits"][0];
    assert_eq!(grant["amount"], "100000000000000000000");
    assert_eq!(grant["availableAmount"], grant["amount"]);
    assert_eq!(grant["creditSource"], "goal");
    assert_eq!(grant["status"], "AVAILABLE");
    assert!(
        grant["timestamp"].is_string()
            && grant["claimedAt"].is_string()
            && grant["expiresAt"].is_string()
    );
    let (_, progress) = call(
        &app,
        &wallet,
        "get",
        &format!("/credits/users/{address}/progress"),
        &[],
        None,
    )
    .await;
    assert_eq!(progress["credits"]["available"], 100);
    assert_eq!(progress["goals"][0]["isClaimed"], true);
    let (status, _, body) = send(
        &app,
        "GET",
        "/shop-api/v1/credit-program/season",
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], 1);
    assert_eq!(body["isPaused"], false);
    let (_, _, body) = send(&app, "GET", "/credits/seasons", HeaderMap::new(), None).await;
    assert!(body["lastSeason"].is_null() && body["nextSeason"].is_null());
    assert_eq!(
        body["currentSeason"]["season"]["maxMana"],
        "1000000000000000000000"
    );
    sqlx::query("INSERT INTO credits.wallet_flags(address,source,linked_addresses) VALUES($1,'explorer_ip_multi_accounting',ARRAY[$1,$2])").bind(address).bind(OTHER).execute(&db.pool).await.unwrap();
    let (status, body) = call(
        &app,
        &wallet,
        "get",
        &format!("/credits/users/{address}/credits"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["credits"], json!([]));
    assert_eq!(body["totalCredits"], 0);
    assert_eq!(body["isBlockedForClaiming"], true);
    assert_eq!(body["reason"]["linkedAddresses"], json!([OTHER]));
    assert_eq!(body["usd"], json!({"balanceCents":120,"credits":12}));
    // A paid wallet can leave the seasonal program without losing its USD
    // balance. Exercise both aliases through the entire mounted Shop tree.
    let paid_wallet = scratch_wallet();
    let paid_address = &paid_wallet.address;
    seed_wallet(&db.pool, paid_address, "0", "12").await;
    let (status, body) = call(
        &app,
        &paid_wallet,
        "get",
        &format!("/credits/users/{paid_address}/status"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["status"], "not_registered");
    sqlx::query("UPDATE credits.wallets SET has_started_program=true WHERE address=$1")
        .bind(paid_address)
        .execute(&db.pool)
        .await
        .unwrap();
    let (status, body) = call(
        &app,
        &paid_wallet,
        "delete",
        &format!("/shop-api/v1/users/{paid_address}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ok":true}));
    for path in [
        format!("/credits/users/{paid_address}/status"),
        format!("/shop-api/v1/users/{paid_address}/status"),
    ] {
        let (status, body) = call(&app, &paid_wallet, "get", &path, &[], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["status"], "opted_out");
        assert!(body["data"]["optedOutAt"].is_string());
    }
    let (status, body) = call(
        &app,
        &paid_wallet,
        "get",
        &format!("/shop-api/v1/users/{paid_address}/credits"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["usd"], json!({"balanceCents":120,"credits":12}));
    drop(app);
    std::fs::remove_dir_all(storage_root).unwrap();
    db.drop().await;
}

#[tokio::test]
async fn fixed_scan_round_retries_recovered_events_under_continuous_arrivals() {
    let Some(db) = OwnDatabase::create("photo_goal_continuous_arrivals").await else {
        return;
    };
    seed(&db.pool, 1, "1000000000000000000", "1000000000000000000000").await;
    let state = fixture_state(db.pool.clone(), true);
    let failed = photo(&db.pool, OWNER).await;
    photo(&db.pool, OTHER).await;
    sqlx::query("ALTER TABLE social.notifications ADD CONSTRAINT reject_goal_test CHECK(type <> 'credits_goal_completed')")
        .execute(&db.pool).await.unwrap();
    assert!(process_batch(&state, 1).await.is_err());
    sqlx::query("ALTER TABLE social.notifications DROP CONSTRAINT reject_goal_test")
        .execute(&db.pool)
        .await
        .unwrap();

    // There is always a committed event above the previous pass's scan hint.
    // Wrapping only when that suffix is empty would never retry `failed`.
    for _ in 0..5 {
        photo(&db.pool, OTHER).await;
        process_batch(&state, 1).await.unwrap();
    }
    let outcome: Option<String> =
        sqlx::query_scalar("SELECT outcome FROM credits.photo_goal_receipts WHERE event_id=$1")
            .bind(failed)
            .fetch_optional(&db.pool)
            .await
            .unwrap();
    assert_eq!(outcome.as_deref(), Some("counted"));
    let grants: i64 =
        sqlx::query_scalar("SELECT count(*) FROM credits.goal_grants WHERE address=$1")
            .bind(OWNER)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(grants, 1);
    db.drop().await;
}

#[tokio::test]
async fn explorer_override_cannot_enable_an_unavailable_allowlist() {
    let Some(db) = OwnDatabase::create("photo_goal_override_policy").await else {
        return;
    };
    seed(&db.pool, 2, "1000000000000000000", "1000000000000000000000").await;
    let state = fixture_state(db.pool.clone(), true);
    state.goal_flags.set_documents(None, None);
    sqlx::query("INSERT INTO explore_api.feature_flags_local(app,name,value) VALUES('dapps','dapps-credits-server',true),('explorer','explorer-alfa-marketplace-credits',true)")
        .execute(&db.pool).await.unwrap();
    let event = photo(&db.pool, OWNER).await;
    assert!(
        process_event(&state, event, super::super::store::now())
            .await
            .is_err()
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.photo_goal_receipts").await,
        0
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_progress").await,
        0
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        0
    );

    // Explicitly disabling the allowlist needs no missing payload.
    sqlx::query("UPDATE explore_api.feature_flags_local SET value=false WHERE app='explorer'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        process_event(&state, event, super::super::store::now())
            .await
            .unwrap()
    );

    // A successful fetch with no payload is an observed open gate in source.
    sqlx::query("UPDATE explore_api.feature_flags_local SET value=true WHERE app='explorer'")
        .execute(&db.pool)
        .await
        .unwrap();
    state.goal_flags.set_documents(
        None,
        Some(json!({
            "flags": {"explorer-alfa-marketplace-credits": true}, "variants": {}
        })),
    );
    let event = photo(&db.pool, OWNER).await;
    assert!(
        process_event(&state, event, super::super::store::now())
            .await
            .unwrap()
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        1
    );

    // An observed empty string likewise means no gate, not an empty allowlist.
    state.goal_flags.set_documents(
        None,
        Some(json!({
            "flags": {"explorer-alfa-marketplace-credits": true},
            "variants": {"explorer-alfa-marketplace-credits": {"payload": {"value": ""}}}
        })),
    );
    let event = photo(&db.pool, OTHER).await;
    assert!(
        process_event(&state, event, super::super::store::now())
            .await
            .unwrap()
    );

    // Nonempty invalid entries parse to an empty list, which permits nobody.
    state.goal_flags.set_documents(None, Some(json!({
        "flags": {"explorer-alfa-marketplace-credits": true},
        "variants": {"explorer-alfa-marketplace-credits": {"payload": {"value": "not-a-wallet"}}}
    })));
    let event = photo(&db.pool, OTHER).await;
    assert!(
        process_event(&state, event, super::super::store::now())
            .await
            .unwrap()
    );
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM credits.photo_goal_receipts WHERE event_id=$1")
            .bind(event)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(outcome, "not-allowed");
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        1
    );
    db.drop().await;
}

#[tokio::test]
async fn captcha_get_without_an_active_season_keeps_the_source_error() {
    use super::super::tests::{call, scratch_wallet};
    use axum::{Router, http::StatusCode};
    let Some(db) = OwnDatabase::create("photo_goal_no_season_captcha").await else {
        return;
    };
    let state = fixture_state(db.pool.clone(), true);
    let app = Router::new().nest("/credits", super::super::router(state));
    let wallet = scratch_wallet();
    let (status, body) = call(&app, &wallet, "get", "/credits/captcha", &[], None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        json!({"ok": false, "error": "Error retrieving captcha"})
    );
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.captcha_challenges").await,
        0
    );
    db.drop().await;
}

#[tokio::test]
async fn backlog_diagnostic_failure_does_not_fail_committed_processing_or_retry_every_tick() {
    let Some(db) = OwnDatabase::create("photo_goal_diagnostic_failure").await else {
        return;
    };
    seed(&db.pool, 1, "1000000000000000000", "1000000000000000000000").await;
    let state = fixture_state(db.pool.clone(), true);
    photo(&db.pool, OWNER).await;
    assert_eq!(process_batch(&state, 100).await.unwrap(), 1);
    assert_eq!(
        count(&db.pool, "SELECT count(*) FROM credits.goal_grants").await,
        1
    );

    // With nothing new to process the pass leaves the receipts alone; the
    // oldest-age query needs them and times out behind this lock.
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE credits.photo_goal_receipts IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let timer = parking_lot::Mutex::new(None);
    let (processed, sample) = process_pass(&state, &timer).await.unwrap();
    assert_eq!(processed, 0);
    assert!(
        sample.is_none(),
        "a failed query cannot invent an oldest age"
    );
    blocker.rollback().await.unwrap();

    photo(&db.pool, OTHER).await;
    let (processed, sample) = process_pass(&state, &timer).await.unwrap();
    assert_eq!(processed, 1);
    assert!(
        sample.is_none(),
        "a failed diagnostic still waits five minutes before retrying"
    );
    assert!(
        sample_oldest(&db.pool).await.is_some(),
        "the restored diagnostic itself works"
    );
    db.drop().await;
}

#[tokio::test]
async fn backlog_diagnostic_timeout_is_bounded_and_keeps_the_pool_usable() {
    let Some(db) = OwnDatabase::create("photo_goal_diagnostic_timeout").await else {
        return;
    };
    let original_timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut lock = db.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE camera_reel.photo_events IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let sample = tokio::time::timeout(Duration::from_secs(3), sample_oldest(&db.pool))
        .await
        .expect("diagnostic must stop waiting for a locked table");
    assert!(sample.is_none());
    lock.rollback().await.unwrap();
    assert!(sample_oldest(&db.pool).await.is_some());
    let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        timeout, original_timeout,
        "transaction-local diagnostic limit must not leak"
    );
    db.drop().await;
}
