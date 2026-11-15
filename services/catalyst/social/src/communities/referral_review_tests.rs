use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::mpsc;

use crate::db::scratch::{OwnedDatabase as RewardDatabase, Schema};

// A drainer intentionally scans every pending grant. Each drainer fixture
// therefore needs its own database so it cannot issue another test's rewards.
async fn reward_database() -> Option<RewardDatabase> {
    reward_database_with_legacy_receipts(false).await
}

async fn reward_database_with_legacy_receipts(legacy: bool) -> Option<RewardDatabase> {
    let Some(database) = RewardDatabase::new("referral", Schema::Blank, 12).await else {
        eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
        return None;
    };
    let pool = &database.pool;
    if legacy {
        // Reproduce the earlier PR bootstrap in a new, owned database. No
        // existing database is reset or altered to manufacture the old shape.
        let column = "    received_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', clock_timestamp()),\n";
        assert_eq!(crate::db::SCHEMA_SQL.matches(column).count(), 1);
        let schema = crate::db::SCHEMA_SQL.replace(column, "");
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(schema))
            .execute(&mut *tx)
            .await
            .unwrap();
        let steps = crate::db::schema_migrate::fresh_steps(crate::db::schema_migrate::parse_steps(
            crate::db::MIGRATIONS_SQL,
        ))
        .unwrap();
        let earlier: Vec<_> = steps
            .into_iter()
            .filter(|step| step.name != "0025_social_referral_login_received_at")
            .collect();
        crate::db::schema_migrate::record_steps(&mut *tx, &earlier, true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    } else {
        crate::db::apply_schema(pool).await.unwrap();
    }
    Some(database)
}

fn wallet() -> Address {
    Address::parse(&format!("0x{}00000000", uuid::Uuid::new_v4().simple())).unwrap()
}
async fn repeatable_read_writers(pool: &sqlx::PgPool) -> sqlx::PgPool {
    let writers = sqlx::postgres::PgPoolOptions::new()
        .max_connections(12)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query(
                    "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                )
                .execute(connection)
                .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let isolation: String = sqlx::query_scalar("SHOW default_transaction_isolation")
        .fetch_one(&writers)
        .await
        .unwrap();
    assert_eq!(isolation, "repeatable read");
    writers
}
async fn advisory_key(pool: &sqlx::PgPool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT hashtextextended($1,0)")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn wait_for_locks(pool: &sqlx::PgPool, keys: &[i64], expected: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND NOT granted AND ((classid::bigint << 32) | objid::bigint)=ANY($1)").bind(keys).fetch_one(pool).await.unwrap();
            if count == expected { break; }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.expect("concurrent operations reached their transaction reservations");
}

#[tokio::test]
async fn concurrent_ip_creates_enforce_the_cap_and_keep_rejected_retry_receipts() {
    // Trigger DDL must not queue a table lock behind another test's grant
    // reservations while that test waits on its own advisory-lock barrier.
    let Some(database) = reward_database().await else {
        return;
    };
    let pool = database.pool.clone();
    let referral = Arc::new(Referral::new(
        repeatable_read_writers(&pool).await,
        Outbound::for_tests(),
        Settings {
            suspicious_window_ms: 0.0,
            ..Settings::for_tests()
        },
    ));
    let referrer = wallet();
    let invited = (0..8).map(|_| wallet()).collect::<Vec<_>>();
    let gate = advisory_key(&pool, &uuid::Uuid::new_v4().to_string()).await;
    let quota = advisory_key(&pool, &format!("social-referral-ip:{referrer}:192.0.2.1")).await;
    let function = format!("referral_ip_gate_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE FUNCTION social.{function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.referrer='{referrer}' THEN PERFORM pg_advisory_xact_lock({gate}); END IF; RETURN NEW; END $$"))).execute(&pool).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TRIGGER {function} BEFORE INSERT ON social.referral_progress FOR EACH ROW EXECUTE FUNCTION social.{function}()"))).execute(&pool).await.unwrap();
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(gate)
        .execute(&mut *held)
        .await
        .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for invited in invited {
        let referral = referral.clone();
        let referrer = referrer.clone();
        tasks.spawn(async move { referral.create(&referrer, &invited, "192.0.2.1").await });
    }
    wait_for_locks(&pool, &[gate, quota], 8).await;
    held.commit().await.unwrap();
    let mut accepted = 0;
    while let Some(result) = tasks.join_next().await {
        accepted += usize::from(result.unwrap().is_ok());
    }
    assert_eq!(accepted, 2);
    let statuses:Vec<(String,String)>=sqlx::query_as("SELECT invited_user,status FROM social.referral_progress WHERE referrer=$1 ORDER BY invited_user").bind(&referrer).fetch_all(&pool).await.unwrap();
    assert_eq!(statuses.len(), 8);
    assert_eq!(statuses.iter().filter(|row| row.1 == "pending").count(), 2);
    assert_eq!(
        statuses
            .iter()
            .filter(|row| row.1 == "rejected_ip_match")
            .count(),
        6
    );
    for (invited, status) in &statuses {
        if status == "rejected_ip_match" {
            referral
                .create(&referrer, &Address::parse(invited).unwrap(), "192.0.2.1")
                .await
                .unwrap();
        }
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {function} ON social.referral_progress"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP FUNCTION social.{function}()"
    )))
    .execute(&pool)
    .await
    .unwrap();
    referral.pool.close().await;
    drop(referral);
    database.drop().await;
}

#[tokio::test]
async fn concurrent_referrer_grants_observe_distinct_totals_and_release_sql_before_rewards() {
    let Some(pool) = crate::db::scratch::pool(12).await else {
        return;
    };
    let referrer = wallet();
    let invited = (0..8).map(|_| wallet()).collect::<Vec<_>>();
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = calls.clone();
    let observed_pool = pool.clone();
    let observed_referrer = referrer.clone();
    let app=axum::Router::new().route("/api/rewards",axum::routing::post(move || {let calls=requests.clone();let pool=observed_pool.clone();let referrer=observed_referrer.clone();async move {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let available:bool=sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,0))").bind(format!("social-referral-tier:{referrer}")).fetch_one(&pool).await.unwrap();
                if available { break; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("no transaction reservation spans the external reward request");
        calls.fetch_add(1,Ordering::SeqCst);
        Json(json!({"data":[{"image":"https://reward.example.org/tier.png","rarity":"rare"}]}))
    }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let referral = Arc::new(Referral::new(
        repeatable_read_writers(&pool).await,
        Outbound::for_tests(),
        Settings {
            min_login_days: 0.0,
            max_ip_matches: 100.0,
            suspicious_window_ms: 0.0,
            reward_server_url: Some(url),
            ..Settings::for_tests()
        },
    ));
    sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted) SELECT $1,'0x'||replace(gen_random_uuid()::text,'-','')||'00000000','tier_granted',true FROM generate_series(1,4)").bind(&referrer).execute(&pool).await.unwrap();
    for invited in &invited {
        referral
            .create(&referrer, invited, "192.0.2.1")
            .await
            .unwrap();
    }
    let key = advisory_key(&pool, &format!("social-referral-tier:{referrer}")).await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *held)
        .await
        .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for invited in invited {
        let referral = referral.clone();
        tasks.spawn(async move { referral.finalize_referral(invited.as_str()).await });
    }
    wait_for_locks(&pool, &[key], 8).await;
    let before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM social.referral_progress WHERE referrer=$1 AND tier_granted",
    )
    .bind(&referrer)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, 4);
    held.commit().await.unwrap();
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    let totals:Vec<i64>=sqlx::query_scalar("SELECT (metadata->>'invitedUsers')::bigint FROM social.notifications WHERE address=$1 AND type='referral_invited_users_accepted' ORDER BY (metadata->>'invitedUsers')::bigint").bind(&referrer).fetch_all(&pool).await.unwrap();
    assert_eq!(totals, (5..=12).collect::<Vec<_>>());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let images: Vec<i32> = sqlx::query_scalar(
        "SELECT tier FROM social.referral_reward_images WHERE referrer=$1 ORDER BY tier",
    )
    .bind(&referrer)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(images, [5, 10]);
    server.abort();
}

/// A restore hold blocks both new provider claims and confirmed downstream
/// retries. Refreshing the stamps resumes the same durable work.
#[tokio::test]
async fn reward_drain_respects_restore_holds_before_claims_and_follow_ups() {
    use crate::shared::writes::restore::{
        Stamp,
        lab::{HOUR, StampLab},
        scoped,
    };
    let Some(database) = reward_database().await else {
        return;
    };
    let pool = database.pool.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let app = axum::Router::new().route(
        "/api/rewards",
        axum::routing::post(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Json(json!({"data":[{"image":"https://reward.example.org/new.png"}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let referral = Referral::new(
        pool.clone(),
        Outbound::for_tests(),
        Settings {
            reward_server_url: Some(url),
            ..Settings::for_tests()
        },
    );
    for confirmed in [false, true] {
        sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted,reward_tier,reward_requested_at,reward_response) VALUES($1,$2,'tier_granted',true,5,CASE WHEN $3 THEN timezone('UTC',now()) END,$4)")
            .bind(wallet()).bind(wallet()).bind(confirmed)
            .bind(confirmed.then(|| json!({"image":"https://reward.example.org/confirmed.png"})))
            .execute(&pool).await.unwrap();
    }
    let before: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(r) FROM social.referral_progress r ORDER BY invited_user",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for kind in ["missing", "stale", "no_blobs"] {
        let lab = if kind == "missing" {
            StampLab::new()
        } else {
            StampLab::fresh()
        };
        match kind {
            "stale" => lab.advance(30 * HOUR),
            "no_blobs" => {
                let mut stamp: Value = serde_json::from_slice(
                    &std::fs::read(lab.dir.join(Stamp::Backup.file())).unwrap(),
                )
                .unwrap();
                stamp["blobs"]["included"] = json!(false);
                lab.write_json(Stamp::Backup, stamp);
            }
            _ => {}
        }
        scoped(lab.stamps.clone(), async {
            assert!(crate::shared::writes::refusal(true).is_some(), "{kind}");
            referral.drain_pending_rewards().await.unwrap();
        })
        .await;
        let after: Vec<Value> = sqlx::query_scalar(
            "SELECT to_jsonb(r) FROM social.referral_progress r ORDER BY invited_user",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            after, before,
            "{kind}: no claim, retry timestamp or receipt mutation"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{kind}: no provider POST");
        let effects: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM social.notifications), (SELECT count(*) FROM social.referral_reward_images)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(effects, (0, 0), "{kind}: no downstream effects");
    }
    let fresh = StampLab::fresh();
    scoped(fresh.stamps.clone(), referral.drain_pending_rewards())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let completed: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM social.referral_progress WHERE reward_tier IS NOT NULL), (SELECT count(*) FROM social.notifications), (SELECT count(*) FROM social.referral_reward_images)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(completed, (0, 2, 2));
    drop(referral);
    server.abort();
    database.drop().await;
}

/// Legacy paid receipts prove payment independently of the image metadata.
/// Repairing that metadata resumes image delivery without another reward POST.
#[tokio::test]
async fn imported_paid_rewards_with_incomplete_images_are_not_requested_again() {
    let Some(database) = reward_database().await else {
        return;
    };
    let pool = database.pool.clone();
    let rewards = Arc::new(AtomicUsize::new(0));
    let calls = rewards.clone();
    let app = axum::Router::new().route(
        "/api/rewards",
        axum::routing::post(move || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({"data":[{"image":"https://reward.example.org/unexpected.png"}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let referral = Referral::new(
        pool.clone(),
        Outbound::for_tests(),
        Settings {
            reward_server_url: Some(url),
            ..Settings::for_tests()
        },
    );
    for reward in [json!({}), json!({"image":42})] {
        let (referrer, invited) = (wallet(), wallet());
        let paid = json!({"tier":5,"reward":reward,"notified":true});
        sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted,reward_paid) VALUES($1,$2,'tier_granted',true,$3)")
            .bind(&referrer).bind(&invited).bind(&paid).execute(&pool).await.unwrap();
        sqlx::raw_sql(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/schemas/migrations/0024_social_referral_reward_receipt.sql"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let receipt: (Option<Value>, Option<Value>) = sqlx::query_as("SELECT reward_response,reward_paid FROM social.referral_progress WHERE invited_user=$1")
            .bind(&invited).fetch_one(&pool).await.unwrap();
        assert_eq!(receipt, (Some(reward), Some(paid)));
        referral.drain_pending_rewards().await.unwrap();
        assert_eq!(rewards.load(Ordering::SeqCst), 0);
        // Only image metadata needs reconciliation; the confirmed payment
        // and the already-sent notification must never be replayed.
        sqlx::query("UPDATE social.referral_progress SET reward_response=jsonb_set(reward_response,'{image}','\"https://reward.example.org/repaired.png\"'),reward_retry_at=NULL WHERE invited_user=$1")
            .bind(&invited).execute(&pool).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                referral.finalize_referral(invited.as_str()).await.unwrap();
                let pending: Option<i32> = sqlx::query_scalar(
                    "SELECT reward_tier FROM social.referral_progress WHERE invited_user=$1",
                )
                .bind(&invited)
                .fetch_one(&pool)
                .await
                .unwrap();
                if pending.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let image: String = sqlx::query_scalar("SELECT reward_image_url FROM social.referral_reward_images WHERE referrer=$1 AND tier=5")
            .bind(&referrer).fetch_one(&pool).await.unwrap();
        assert_eq!(image, "https://reward.example.org/repaired.png");
        let notices: i64 = sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE address=$1 AND type='referral_new_tier_reached'")
            .bind(&referrer).fetch_one(&pool).await.unwrap();
        assert_eq!(
            notices, 0,
            "the imported receipt already confirms notification delivery"
        );
        assert_eq!(rewards.load(Ordering::SeqCst), 0);
    }
    server.abort();
    database.drop().await;
}

#[tokio::test]
async fn ambiguous_reward_responses_are_not_reissued_after_restart_or_login() {
    for failure in ["http", "malformed", "missing_image"] {
        let Some(database) = reward_database().await else {
            return;
        };
        let pool = database.pool.clone();
        let (referrer, invited) = (wallet(), wallet());
        let rewards = Arc::new(AtomicUsize::new(0));
        let calls = rewards.clone();
        let app = axum::Router::new().route(
            "/api/rewards",
            axum::routing::post(move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    // The provider can have granted before either failure reply.
                    match failure {
                        "http" => (StatusCode::INTERNAL_SERVER_ERROR, "unconfirmed response"),
                        "malformed" => (StatusCode::OK, "unconfirmed response"),
                        _ => (StatusCode::OK, r#"{"data":[{}]}"#),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let settings = Settings {
            reward_server_url: Some(url),
            ..Settings::for_tests()
        };
        sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted,reward_tier) VALUES($1,$2,'tier_granted',true,5)")
            .bind(&referrer).bind(&invited).execute(&pool).await.unwrap();
        let before = Referral::new(pool.clone(), Outbound::for_tests(), settings.clone());
        assert!(before.finalize_referral(invited.as_str()).await.is_err());
        drop(before);
        let after = Referral::new(pool.clone(), Outbound::for_tests(), settings);
        sqlx::query(
            "UPDATE social.referral_progress SET reward_retry_at=NULL WHERE invited_user=$1",
        )
        .bind(&invited)
        .execute(&pool)
        .await
        .unwrap();
        after.drain_pending_rewards().await.unwrap();
        after.finalize_referral(invited.as_str()).await.unwrap();
        assert_eq!(rewards.load(Ordering::SeqCst), 1);
        let state: (Option<i32>, bool, bool) = sqlx::query_as("SELECT reward_tier,reward_requested_at IS NOT NULL,reward_response IS NULL FROM social.referral_progress WHERE invited_user=$1")
            .bind(&invited).fetch_one(&pool).await.unwrap();
        assert_eq!(state, (Some(5), true, true));
        server.abort();
        database.drop().await;
    }
}

/// The production shared client does not impose a total deadline. The reward
/// adapter must bound reading the response body and retain an ambiguous claim.
#[tokio::test]
async fn a_reward_body_timeout_is_quarantined_without_reposting() {
    use futures::StreamExt;
    use std::time::Duration;
    let Some(database) = reward_database().await else {
        return;
    };
    let pool = database.pool.clone();
    let (referrer, invited) = (wallet(), wallet());
    let rewards = Arc::new(AtomicUsize::new(0));
    let calls = rewards.clone();
    let (started, mut starts) = mpsc::unbounded_channel();
    let app = axum::Router::new().route(
        "/api/rewards",
        axum::routing::post(move || {
            let calls = calls.clone();
            let started = started.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let prefix = futures::stream::once(async move {
                    started.send(()).unwrap();
                    Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"{\"data\":["))
                });
                axum::body::Body::from_stream(prefix.chain(futures::stream::pending()))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let settings = Settings {
        reward_server_url: Some(url),
        ..Settings::for_tests()
    };
    sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted,reward_tier) VALUES($1,$2,'tier_granted',true,5)")
        .bind(&referrer).bind(&invited).execute(&pool).await.unwrap();
    // Remove the test client's own 30-second timeout, so this exercises the
    // adapter's deadline, not a default that serve deliberately does not use.
    let http = Outbound::for_tests().provider_profile(None, None).unwrap();
    let before = Referral::new(pool.clone(), http.clone(), settings.clone());
    let user = invited.clone();
    let task = tokio::spawn(async move { before.finalize_referral(user.as_str()).await });
    tokio::time::timeout(Duration::from_secs(5), starts.recv())
        .await
        .unwrap()
        .unwrap();
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    let finished = task.is_finished();
    tokio::time::resume();
    assert!(
        finished,
        "the adapter must bound reading an unfinished body"
    );
    assert!(task.await.unwrap().is_err());
    let after = Referral::new(pool.clone(), http, settings);
    sqlx::query("UPDATE social.referral_progress SET reward_retry_at=NULL WHERE invited_user=$1")
        .bind(&invited)
        .execute(&pool)
        .await
        .unwrap();
    after.drain_pending_rewards().await.unwrap();
    after.finalize_referral(invited.as_str()).await.unwrap();
    assert_eq!(rewards.load(Ordering::SeqCst), 1);
    let state: (Option<i32>, bool, bool) = sqlx::query_as("SELECT reward_tier,reward_requested_at IS NOT NULL,reward_response IS NULL FROM social.referral_progress WHERE invited_user=$1")
        .bind(&invited).fetch_one(&pool).await.unwrap();
    assert_eq!(state, (Some(5), true, true));
    server.abort();
    database.drop().await;
}

#[tokio::test]
async fn cancelled_inflight_reward_preserves_its_claim_for_reconciliation() {
    let Some(database) = reward_database().await else {
        return;
    };
    let pool = database.pool.clone();
    let (referrer, invited) = (wallet(), wallet());
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = entered.clone();
    let rewards = Arc::new(AtomicUsize::new(0));
    let observed = rewards.clone();
    let app = axum::Router::new().route(
        "/api/rewards",
        axum::routing::post(move || {
            let entered = gate.clone();
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                std::future::pending::<()>().await;
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let settings = Settings {
        reward_server_url: Some(url),
        ..Settings::for_tests()
    };
    sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted,reward_tier) VALUES($1,$2,'tier_granted',true,5)")
        .bind(&referrer).bind(&invited).execute(&pool).await.unwrap();
    let before = Referral::new(pool.clone(), Outbound::for_tests(), settings.clone());
    let user = invited.clone();
    let task = tokio::spawn(async move { before.finalize_referral(user.as_str()).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let after = Referral::new(pool.clone(), Outbound::for_tests(), settings);
    sqlx::query("UPDATE social.referral_progress SET reward_retry_at=NULL WHERE invited_user=$1")
        .bind(&invited)
        .execute(&pool)
        .await
        .unwrap();
    after.drain_pending_rewards().await.unwrap();
    after.finalize_referral(invited.as_str()).await.unwrap();
    let claimed: bool = sqlx::query_scalar("SELECT reward_requested_at IS NOT NULL AND reward_response IS NULL FROM social.referral_progress WHERE invited_user=$1")
        .bind(&invited).fetch_one(&pool).await.unwrap();
    assert!(
        claimed,
        "ordinary logins leave the cancelled request quarantined"
    );
    assert_eq!(rewards.load(Ordering::SeqCst), 1);
    server.abort();
    database.drop().await;
}

/// The first finalize fails as the source does (state and first effects), but
/// the grant keeps its reward tier pending. A request with an uncertain
/// provider outcome requires reconciliation before a retry.
/// A tier without a configured campaign key posts nothing; the reward stays
/// pending until the key is configured.
#[tokio::test]
async fn an_unset_campaign_key_leaves_the_reward_pending_without_a_request() {
    let Some(pool) = crate::db::scratch::pool(6).await else {
        return;
    };
    let (referrer, invited) = (wallet(), wallet());
    let rewards = Arc::new(AtomicUsize::new(0));
    let observed = rewards.clone();
    let app=axum::Router::new().route("/api/rewards",axum::routing::post(move || {let calls=observed.clone();async move {calls.fetch_add(1,Ordering::SeqCst);Json(json!({"data":[{"image":"https://reward.example.org/tier.png","rarity":"rare"}]}))}}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let settings = Settings {
        min_login_days: 0.0,
        reward_server_url: Some(url.clone()),
        reward_keys: Default::default(),
        suspicious_window_ms: 0.0,
        ..Settings::for_tests()
    };
    sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted) SELECT $1,'0x'||replace(gen_random_uuid()::text,'-','')||'00000000','tier_granted',true FROM generate_series(1,4)").bind(&referrer).execute(&pool).await.unwrap();
    let unset = Referral::new(pool.clone(), Outbound::for_tests(), settings.clone());
    unset
        .create(&referrer, &invited, "192.0.2.1")
        .await
        .unwrap();
    assert!(unset.finalize_referral(invited.as_str()).await.is_err());
    assert_eq!(
        rewards.load(Ordering::SeqCst),
        0,
        "no empty campaign_key request"
    );
    let claimed: bool = sqlx::query_scalar("SELECT reward_requested_at IS NOT NULL FROM social.referral_progress WHERE invited_user=$1")
        .bind(&invited).fetch_one(&pool).await.unwrap();
    assert!(
        !claimed,
        "missing configuration must not consume the provider claim"
    );
    let configured = Arc::new(Referral::new(
        pool.clone(),
        Outbound::for_tests(),
        Settings {
            reward_keys: Settings::for_tests().reward_keys,
            ..settings
        },
    ));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let referral = configured.clone();
        let invited = invited.clone();
        tasks.spawn(async move { referral.finalize_referral(invited.as_str()).await });
    }
    while let Some(task) = tasks.join_next().await {
        task.unwrap().unwrap();
    }
    assert_eq!(rewards.load(Ordering::SeqCst), 1);
    let pending: Option<i32> = sqlx::query_scalar(
        "SELECT reward_tier FROM social.referral_progress WHERE invited_user=$1",
    )
    .bind(&invited)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending, None);
    server.abort();
}

#[tokio::test]
async fn historical_ipv6_aliases_and_mapped_ipv4_keep_their_quota_and_new_writers_share_the_canonical_lock()
 {
    let Some(pool) = crate::db::scratch::pool(12).await else {
        return;
    };
    let referral = Arc::new(Referral::new(
        repeatable_read_writers(&pool).await,
        Outbound::for_tests(),
        Settings {
            max_ip_matches: 3.0,
            suspicious_window_ms: 0.0,
            ..Settings::for_tests()
        },
    ));
    for (historical, canonical, variants) in [
        (
            ["2001:0DB8:0:0:0:0:0:1", "2001:db8::1"],
            "2001:db8::1",
            ["2001:0db8::0001", "2001:db8:0:0::1"],
        ),
        (
            ["::ffff:c000:201", "::ffff:192.0.2.1"],
            "192.0.2.1",
            ["192.0.2.1", "::FFFF:192.0.2.1"],
        ),
    ] {
        let referrer = wallet();
        for ip in historical.into_iter().chain(["invalid historical IP"]) {
            sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,invited_user_ip,status) VALUES($1,$2,$3,'pending')")
                .bind(&referrer).bind(wallet()).bind(ip).execute(&pool).await.unwrap();
        }
        let key = advisory_key(&pool, &format!("social-referral-ip:{referrer}:{canonical}")).await;
        let mut held = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut *held)
            .await
            .unwrap();
        let invited = [wallet(), wallet()];
        let mut tasks = tokio::task::JoinSet::new();
        for (user, ip) in invited.iter().cloned().zip(variants) {
            let referral = referral.clone();
            let referrer = referrer.clone();
            tasks.spawn(async move { referral.create(&referrer, &user, ip).await });
        }
        wait_for_locks(&pool, &[key], 2).await;
        held.commit().await.unwrap();
        let mut accepted = 0;
        while let Some(result) = tasks.join_next().await {
            accepted += usize::from(result.unwrap().is_ok());
        }
        assert_eq!(
            accepted, 1,
            "two historical aliases already consume two of the three slots"
        );
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT invited_user_ip,status FROM social.referral_progress WHERE invited_user=ANY($1)")
            .bind(invited.iter().map(ToString::to_string).collect::<Vec<_>>()).fetch_all(&pool).await.unwrap();
        assert!(rows.iter().all(|(ip, _)| ip == canonical));
        assert_eq!(
            rows.iter()
                .filter(|(_, status)| status == "pending")
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|(_, status)| status == "rejected_ip_match")
                .count(),
            1
        );
        let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM social.referral_progress WHERE referrer=$1 AND invited_user_ip='invalid historical IP'")
            .bind(&referrer).fetch_one(&pool).await.unwrap();
        assert_eq!(retained, 1);
    }
    let referrer = wallet();
    assert!(referral.create(&referrer, &wallet(), "").await.is_err());
    assert!(
        referral
            .create(&referrer, &wallet(), "not-an-IP")
            .await
            .is_err()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM social.referral_progress WHERE referrer=$1")
            .bind(&referrer)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

/// Native reward completion is atomic even when either local table rejects a
/// write. The durable provider receipt resumes after restart without a new POST.
#[tokio::test]
async fn native_reward_sql_failures_resume_atomically_without_aws_or_duplicate_payment() {
    for failed_table in [
        "referral_reward_images",
        "notifications",
        "notification_deliveries",
    ] {
        let Some(database) = reward_database().await else {
            return;
        };
        let pool = database.pool.clone();
        let (referrer, invited) = (wallet(), wallet());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let app = axum::Router::new().route("/api/rewards", axum::routing::post(move || {
            let calls = observed.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({"data":[{"image":"https://reward.example.org/tier.png","rarity":"rare"}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let settings = Settings {
            reward_server_url: Some(url),
            ..Settings::for_tests()
        };
        sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted,reward_tier) VALUES($1,$2,'tier_granted',true,5)")
            .bind(&referrer).bind(&invited).execute(&pool).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE FUNCTION social.reject_native_reward() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'native SQL fixture failure'; END $$; CREATE TRIGGER reject_native_reward BEFORE INSERT ON social.{failed_table} FOR EACH ROW EXECUTE FUNCTION social.reject_native_reward()")))
            .execute(&pool).await.unwrap();
        let before = Referral::new(pool.clone(), Outbound::for_tests(), settings.clone());
        before.drain_pending_rewards().await.unwrap();
        let state: (Option<i32>, bool) = sqlx::query_as("SELECT reward_tier,reward_response IS NOT NULL FROM social.referral_progress WHERE invited_user=$1")
            .bind(&invited).fetch_one(&pool).await.unwrap();
        assert_eq!(
            state,
            (Some(5), true),
            "{failed_table}: confirmed receipt remains pending"
        );
        let effects: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM social.notifications),(SELECT count(*) FROM social.notification_deliveries),(SELECT count(*) FROM social.referral_reward_images)")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(
            effects,
            (0, 0, 0),
            "{failed_table}: all local effects roll back"
        );
        drop(before);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TRIGGER reject_native_reward ON social.{failed_table}; DROP FUNCTION social.reject_native_reward()")))
            .execute(&pool).await.unwrap();
        sqlx::query(
            "UPDATE social.referral_progress SET reward_retry_at=NULL WHERE invited_user=$1",
        )
        .bind(&invited)
        .execute(&pool)
        .await
        .unwrap();
        let after = Arc::new(Referral::new(pool.clone(), Outbound::for_tests(), settings));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let after = after.clone();
            tasks.spawn(async move { after.drain_pending_rewards().await });
        }
        while let Some(task) = tasks.join_next().await {
            task.unwrap().unwrap();
        }
        after.drain_pending_rewards().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let effects: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM social.notifications),(SELECT count(*) FROM social.notification_deliveries),(SELECT count(*) FROM social.referral_reward_images),(SELECT count(*) FROM social.referral_progress WHERE reward_tier IS NOT NULL)")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(effects, (1, 1, 1, 0));
        drop(after);
        server.abort();
        database.drop().await;
    }
}

#[tokio::test]
async fn native_login_grant_and_accepted_notification_roll_back_and_retry_together() {
    let Some(database) = reward_database().await else {
        return;
    };
    let pool = database.pool.clone();
    let (referrer, invited) = (wallet(), wallet());
    let referral = Referral::new(
        pool.clone(),
        Outbound::for_tests(),
        Settings {
            min_login_days: 0.0,
            ..Settings::for_tests()
        },
    );
    sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted) SELECT $1,'0x'||replace(gen_random_uuid()::text,'-','')||'00000000','tier_granted',true FROM generate_series(1,4)")
        .bind(&referrer).execute(&pool).await.unwrap();
    referral
        .create(&referrer, &invited, "192.0.2.1")
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION social.reject_login_notice() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'native login failure'; END $$; CREATE TRIGGER reject_login_notice BEFORE INSERT ON social.notification_deliveries FOR EACH ROW EXECUTE FUNCTION social.reject_login_notice()")
        .execute(&pool).await.unwrap();
    assert!(
        referral
            .record_authenticated_login(
                invited.as_str(),
                uuid::Uuid::new_v4(),
                chrono::Utc::now().timestamp_millis()
            )
            .await
            .is_err()
    );
    let row: (bool, Option<i32>) = sqlx::query_as(
        "SELECT tier_granted,reward_tier FROM social.referral_progress WHERE invited_user=$1",
    )
    .bind(&invited)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (false, None));
    sqlx::raw_sql("DROP TRIGGER reject_login_notice ON social.notification_deliveries; DROP FUNCTION social.reject_login_notice()")
        .execute(&pool).await.unwrap();
    referral
        .record_authenticated_login(
            invited.as_str(),
            uuid::Uuid::new_v4(),
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
    referral
        .record_authenticated_login(
            invited.as_str(),
            uuid::Uuid::new_v4(),
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
    let row: (bool, Option<i32>, bool) = sqlx::query_as("SELECT tier_granted,reward_tier,reward_requested_at IS NOT NULL FROM social.referral_progress WHERE invited_user=$1")
        .bind(&invited).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row,
        (true, Some(5), false),
        "login commits intent without requesting a provider"
    );
    let effects: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM social.notifications),(SELECT count(*) FROM social.notification_deliveries)")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(effects, (1, 1));
    drop(referral);
    database.drop().await;
}

#[tokio::test]
async fn native_login_receipt_upgrade_preserves_attribution_and_is_idempotent() {
    let Some(database) = reward_database().await else {
        return;
    };
    let pool = database.pool.clone();
    let (referrer, invited) = (wallet(), wallet());
    sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,login_days) VALUES($1,$2,ARRAY['2026-01-01'::date])")
        .bind(&referrer).bind(&invited).execute(&pool).await.unwrap();
    let columns = |pool: sqlx::PgPool| async move {
        sqlx::query_as::<_, (String, String, String)>("SELECT column_name,data_type,is_nullable FROM information_schema.columns WHERE table_schema='social' AND table_name='referral_login_receipts' ORDER BY ordinal_position")
            .fetch_all(&pool).await.unwrap()
    };
    let bootstrap = columns(pool.clone()).await;
    sqlx::query("DROP TABLE social.referral_login_receipts")
        .execute(&pool)
        .await
        .unwrap();
    let ledger_before: Vec<(String, String, bool, chrono::NaiveDateTime)> = sqlx::query_as(
        "SELECT step,sha256,created_fresh,applied_at FROM meta.schema_steps ORDER BY step",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let migrated =
        crate::db::schema_migrate::migrate(&pool, crate::db::SCHEMA_SQL, crate::db::MIGRATIONS_SQL)
            .await
            .unwrap();
    assert!(
        migrated.steps.is_empty(),
        "new objects need no existing-table step"
    );
    assert!(
        migrated
            .created
            .contains(&crate::db::schema_migrate::Object::Relation(
                "social.referral_login_receipts".into()
            ))
    );
    let replay =
        crate::db::schema_migrate::migrate(&pool, crate::db::SCHEMA_SQL, crate::db::MIGRATIONS_SQL)
            .await
            .unwrap();
    assert!(replay.steps.is_empty());
    assert!(replay.created.is_empty());
    let ledger_after: Vec<(String, String, bool, chrono::NaiveDateTime)> = sqlx::query_as(
        "SELECT step,sha256,created_fresh,applied_at FROM meta.schema_steps ORDER BY step",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        ledger_before, ledger_after,
        "historical migration receipts remain unchanged"
    );
    assert_eq!(columns(pool.clone()).await, bootstrap);
    let days: Vec<chrono::NaiveDate> =
        sqlx::query_scalar("SELECT login_days FROM social.referral_progress WHERE invited_user=$1")
            .bind(&invited)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(days, [chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()]);
    let login_id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO social.referral_login_receipts(invited_user,login_id,expires_at) VALUES($1,$2,timezone('UTC',now()))")
        .bind(&invited).bind(login_id).execute(&pool).await.unwrap();
    assert!(sqlx::query("INSERT INTO social.referral_login_receipts(invited_user,login_id,expires_at) VALUES($1,$2,timezone('UTC',now()))")
        .bind(&invited).bind(login_id).execute(&pool).await.is_err());
    assert!(sqlx::query("INSERT INTO social.referral_login_receipts(invited_user,login_id,expires_at) VALUES($1,$2,timezone('UTC',now()))")
        .bind(wallet()).bind(login_id).execute(&pool).await.is_err());
    sqlx::query("DELETE FROM social.referral_progress WHERE invited_user=$1")
        .bind(&invited)
        .execute(&pool)
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM social.referral_login_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    database.drop().await;
}

#[tokio::test]
async fn native_backdated_logins_share_a_server_timed_concurrent_quota_and_terminal_noop() {
    let Some(database) = reward_database().await else {
        return;
    };
    let pool = database.pool.clone();
    let (referrer, invited) = (wallet(), wallet());
    let referral = Arc::new(Referral::new(
        pool.clone(),
        Outbound::for_tests(),
        Settings::for_tests(),
    ));
    referral
        .create(&referrer, &invited, "192.0.2.1")
        .await
        .unwrap();
    sqlx::query("INSERT INTO social.referral_login_receipts(invited_user,login_id,expires_at) SELECT $1,gen_random_uuid(),timezone('UTC',now())+interval '24 hours' FROM generate_series(1,119)")
        .bind(&invited).execute(&pool).await.unwrap();
    let old = chrono::Utc::now().timestamp_millis() - 86_395_000;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let referral = referral.clone();
        let invited = invited.clone();
        tasks.spawn(async move {
            referral
                .record_authenticated_login(invited.as_str(), uuid::Uuid::new_v4(), old)
                .await
        });
    }
    let mut accepted = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(()) => accepted += 1,
            Err(CommError::MessageOnly(StatusCode::TOO_MANY_REQUESTS, _)) => {}
            other => panic!("unexpected admission: {other:?}"),
        }
    }
    assert_eq!(accepted, 1);
    let retention: bool = sqlx::query_scalar("SELECT bool_and(expires_at >= received_at+interval '23 hours 59 minutes') FROM social.referral_login_receipts WHERE invited_user=$1")
        .bind(&invited).fetch_one(&pool).await.unwrap();
    assert!(
        retention,
        "backdating cannot expire server quota receipts early"
    );
    for status in ["tier_granted", "rejected_ip_match"] {
        sqlx::query("UPDATE social.referral_progress SET status=$2,tier_granted=($2='tier_granted') WHERE invited_user=$1")
            .bind(&invited).bind(status).execute(&pool).await.unwrap();
        referral
            .record_authenticated_login(
                invited.as_str(),
                uuid::Uuid::new_v4(),
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            .unwrap();
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM social.referral_login_receipts WHERE invited_user=$1",
    )
    .bind(&invited)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 120,
        "terminal logins neither write nor fail the full quota"
    );
    sqlx::query("UPDATE social.referral_progress SET status='pending',tier_granted=false WHERE invited_user=$1")
        .bind(&invited).execute(&pool).await.unwrap();
    // A future-dated prior occurrence may outlive its server quota window.
    sqlx::query("UPDATE social.referral_login_receipts SET received_at=timezone('UTC',now())-interval '24 hours 1 second',expires_at=timezone('UTC',now())+interval '4 minutes' WHERE invited_user=$1")
        .bind(&invited).execute(&pool).await.unwrap();
    referral
        .record_authenticated_login(
            invited.as_str(),
            uuid::Uuid::new_v4(),
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM social.referral_login_receipts WHERE invited_user=$1",
    )
    .bind(&invited)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 121,
        "retained receipts outside the server quota window do not count"
    );
    drop(referral);
    database.drop().await;
}

#[tokio::test]
async fn native_receipt_cleanup_skips_locked_rows_and_failures_do_not_stop_rewards() {
    for failure in [false, true] {
        let Some(database) = reward_database().await else {
            return;
        };
        let pool = database.pool.clone();
        let (referrer, invited) = (wallet(), wallet());
        sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,status,tier_granted,reward_tier,reward_response) VALUES($1,$2,'tier_granted',true,5,$3)")
            .bind(&referrer).bind(&invited).bind(json!({"image":"https://reward.example.org/confirmed.png"})).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO social.referral_login_receipts(invited_user,login_id,expires_at,received_at) VALUES($1,gen_random_uuid(),timezone('UTC',now())-interval '1 second',timezone('UTC',now())-interval '25 hours')")
            .bind(&invited).execute(&pool).await.unwrap();
        let mut held = pool.begin().await.unwrap();
        if failure {
            sqlx::raw_sql("CREATE FUNCTION social.reject_receipt_cleanup() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'cleanup fixture'; END $$; CREATE TRIGGER reject_receipt_cleanup BEFORE DELETE ON social.referral_login_receipts FOR EACH ROW EXECUTE FUNCTION social.reject_receipt_cleanup()")
                .execute(&pool).await.unwrap();
        } else {
            sqlx::query(
                "SELECT * FROM social.referral_login_receipts WHERE invited_user=$1 FOR UPDATE",
            )
            .bind(&invited)
            .execute(&mut *held)
            .await
            .unwrap();
        }
        let referral = Referral::new(pool.clone(), Outbound::for_tests(), Settings::for_tests());
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            referral.drain_pending_rewards(),
        )
        .await
        .unwrap()
        .unwrap();
        let pending: Option<i32> = sqlx::query_scalar(
            "SELECT reward_tier FROM social.referral_progress WHERE invited_user=$1",
        )
        .bind(&invited)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            pending, None,
            "cleanup failure={failure}: confirmed reward completes"
        );
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM social.referral_login_receipts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 1);
        held.rollback().await.unwrap();
        if failure {
            sqlx::raw_sql("DROP TRIGGER reject_receipt_cleanup ON social.referral_login_receipts; DROP FUNCTION social.reject_receipt_cleanup()")
                .execute(&pool).await.unwrap();
        }
        referral.drain_pending_rewards().await.unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM social.referral_login_receipts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
        drop(referral);
        database.drop().await;
    }
}

#[tokio::test]
async fn earlier_native_receipts_upgrade_preserves_retries_and_server_quota() {
    let Some(database) = reward_database_with_legacy_receipts(true).await else {
        return;
    };
    let pool = database.pool.clone();
    let (referrer, invited) = (wallet(), wallet());
    sqlx::query("INSERT INTO social.referral_progress(referrer,invited_user,login_days) VALUES($1,$2,ARRAY['2026-01-01'::date])")
        .bind(&referrer).bind(&invited).execute(&pool).await.unwrap();
    // Old expiry came from the client's occurrence, so it may already be due
    // when the migration gives the retained facts a server quota timestamp.
    sqlx::query("INSERT INTO social.referral_login_receipts(invited_user,login_id,expires_at) SELECT $1,gen_random_uuid(),timezone('UTC',now())-interval '1 second' FROM generate_series(1,120)")
        .bind(&invited).execute(&pool).await.unwrap();
    let original: Vec<(uuid::Uuid, chrono::NaiveDateTime)> = sqlx::query_as(
        "SELECT login_id,expires_at FROM social.referral_login_receipts ORDER BY login_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let ledger = |pool: sqlx::PgPool| async move {
        sqlx::query_as::<_, (String, String, bool, chrono::NaiveDateTime)>(
            "SELECT step,sha256,created_fresh,applied_at FROM meta.schema_steps ORDER BY step",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    let before = ledger(pool.clone()).await;
    let started: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT timezone('UTC',clock_timestamp())")
            .fetch_one(&pool)
            .await
            .unwrap();
    let migrated =
        crate::db::schema_migrate::migrate(&pool, crate::db::SCHEMA_SQL, crate::db::MIGRATIONS_SQL)
            .await
            .unwrap();
    assert_eq!(migrated.steps, ["0025_social_referral_login_received_at"]);
    assert!(
        migrated.created.is_empty(),
        "the populated relation is altered in place"
    );
    let after = ledger(pool.clone()).await;
    assert_eq!(after.len(), before.len() + 1);
    assert!(before.iter().all(|row| after.contains(row)));
    let added = after
        .iter()
        .find(|row| row.0 == "0025_social_referral_login_received_at")
        .unwrap();
    assert!(!added.2, "the production executor records an applied step");
    let columns: (String, String, String) = sqlx::query_as("SELECT data_type,is_nullable,column_default FROM information_schema.columns WHERE table_schema='social' AND table_name='referral_login_receipts' AND column_name='received_at'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        (columns.0.as_str(), columns.1.as_str()),
        ("timestamp without time zone", "NO")
    );
    assert!(columns.2.contains("clock_timestamp()"));
    let receipts: Vec<(uuid::Uuid, chrono::NaiveDateTime, chrono::NaiveDateTime)> = sqlx::query_as(
        "SELECT login_id,expires_at,received_at FROM social.referral_login_receipts ORDER BY login_id",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(
        receipts
            .iter()
            .map(|row| (row.0, row.1))
            .collect::<Vec<_>>(),
        original
    );
    assert!(receipts.iter().all(|row| row.2 >= started));
    let finished: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT timezone('UTC',clock_timestamp())")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(receipts.iter().all(|row| row.2 <= finished));
    let replay =
        crate::db::schema_migrate::migrate(&pool, crate::db::SCHEMA_SQL, crate::db::MIGRATIONS_SQL)
            .await
            .unwrap();
    assert!(replay.steps.is_empty() && replay.created.is_empty());
    assert_eq!(ledger(pool.clone()).await, after);
    let reread: Vec<(uuid::Uuid, chrono::NaiveDateTime, chrono::NaiveDateTime)> = sqlx::query_as(
        "SELECT login_id,expires_at,received_at FROM social.referral_login_receipts ORDER BY login_id",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(reread, receipts, "replay never renews the quota window");
    let referral = Referral::new(pool.clone(), Outbound::for_tests(), Settings::for_tests());
    referral
        .record_authenticated_login(
            invited.as_str(),
            original[0].0,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
    let days: Vec<chrono::NaiveDate> =
        sqlx::query_scalar("SELECT login_days FROM social.referral_progress WHERE invited_user=$1")
            .bind(&invited)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        days,
        [chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()],
        "retained duplicate does not advance progress"
    );
    referral.drain_pending_rewards().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM social.referral_login_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 120,
        "worker cleanup respects the server quota backfill"
    );
    let event = uuid::Uuid::new_v4();
    assert!(
        matches!(
            referral
                .record_authenticated_login(
                    invited.as_str(),
                    event,
                    chrono::Utc::now().timestamp_millis()
                )
                .await,
            Err(CommError::MessageOnly(StatusCode::TOO_MANY_REQUESTS, _))
        ),
        "login cleanup cannot discard the quota backfill either"
    );
    // Advance the fixture's server receipt window, leaving original expiry and
    // identity intact. Both horizons must finish before cleanup frees the IDs.
    sqlx::query("UPDATE social.referral_login_receipts SET received_at=timezone('UTC',now())-interval '25 hours'")
        .execute(&pool).await.unwrap();
    referral.drain_pending_rewards().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM social.referral_login_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    referral
        .record_authenticated_login(
            invited.as_str(),
            event,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
    drop(referral);
    database.drop().await;
}
