use super::*;
use chrono::Duration;
use serde_json::json;

async fn community(pool: &PgPool, now: NaiveDateTime) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO social.communities(id,name,owner_address,ranking_tracking_since) VALUES($1,'Ranking scanner',$2,$3)")
        .bind(id).bind(format!("0x{}00000000", Uuid::new_v4().simple()))
        .bind(now-Duration::days(30)).execute(pool).await.unwrap();
    id
}

async fn event(
    pool: &PgPool,
    community: Uuid,
    id: &str,
    starts: &[NaiveDateTime],
    duration_ms: i64,
) {
    let dates: Vec<_> = starts
        .iter()
        .map(|start| start.and_utc().to_rfc3339())
        .collect();
    let raw = json!({"start_at":dates[0],"recurrent_dates":dates});
    sqlx::query("INSERT INTO explore.event(id,name,start_at,finish_at,duration_ms,approved,community_id,raw,origin) VALUES($1,'Ranking scanner',$2,$3,$4,true,$5,$6,'local')")
        .bind(id).bind(starts[0]).bind(*starts.iter().max().unwrap()+Duration::milliseconds(duration_ms))
        .bind(duration_ms).bind(community.to_string()).bind(raw).execute(pool).await.unwrap();
}

async fn wait_for_backend_block(pool: &PgPool, blocker: i32, waiter: i32) {
    tokio::time::timeout(crate::shared::test_wait::HANG_GUARD, async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $1=ANY(pg_blocking_pids($2))")
                .bind(blocker)
                .bind(waiter)
                .fetch_one(pool)
                .await
                .unwrap();
            if blocked {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn counts(pool: &PgPool, community: Uuid) -> (i32, i32, i64) {
    sqlx::query_as("SELECT ranking_events_count,ranking_events_total_attendees,(SELECT count(*) FROM social.community_ranking_events r WHERE r.community_id=c.id AND kind='event') FROM social.communities c WHERE id=$1")
        .bind(community).fetch_one(pool).await.unwrap()
}

async fn remove(pool: &PgPool, community: Uuid) {
    sqlx::query("DELETE FROM explore.event_attendance_local WHERE event_id IN (SELECT id FROM explore.event WHERE community_id=$1)")
        .bind(community.to_string()).execute(pool).await.unwrap();
    sqlx::query("DELETE FROM explore.event WHERE community_id=$1")
        .bind(community.to_string())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM social.communities WHERE id=$1")
        .bind(community)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn completed_catalog_pages_rotate_before_long_recurrence_windows_and_restart_safely() {
    let _serial = ENDED_SCAN_TESTS.lock().await;
    let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
        return;
    };
    let now = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
        .unwrap()
        .naive_utc();
    let community = community(&pool, now).await;
    let prefix = format!("r16-page-{}", Uuid::new_v4());
    let start = now - Duration::hours(2);
    sqlx::query("INSERT INTO explore.event(id,name,start_at,finish_at,duration_ms,approved,community_id,raw,origin) SELECT $1||'-'||lpad(n::text,3,'0'),'Completed page',$2,$2+interval '1 hour',3600000,true,$3,jsonb_build_object('start_at',$4::text),'local' FROM generate_series(0,204) n")
        .bind(&prefix).bind(start).bind(community.to_string()).bind(start.and_utc().to_rfc3339()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO social.community_ranking_events(kind,event_key,community_id,occurred_at) SELECT 'event',id||':'||start_at::text,$2,finish_at FROM explore.event WHERE community_id=$1 ORDER BY id LIMIT 200")
        .bind(community.to_string()).bind(community).execute(&pool).await.unwrap();
    sqlx::query("UPDATE social.communities SET ranking_events_count=200 WHERE id=$1")
        .bind(community)
        .execute(&pool)
        .await
        .unwrap();
    let long = format!("{prefix}-long");
    let starts: Vec<_> = (0..250)
        .map(|n| now - Duration::hours(1) - Duration::minutes(n))
        .collect();
    event(&pool, community, &long, &starts, 1000).await;
    sqlx::query("INSERT INTO explore.event_attendance_local(event_id,signer,action) SELECT $1,'0x'||replace(gen_random_uuid()::text,'-','')||'00000000',action FROM unnest(ARRAY['going','going','cancel']) action")
        .bind(&long).execute(&pool).await.unwrap();
    let mut cursor = EndedScan {
        after_event: prefix.clone(),
        ..Default::default()
    };
    assert_eq!(
        scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap(),
        0
    );
    assert_eq!(cursor.after_event, format!("{prefix}-099"));
    assert_eq!(
        scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap(),
        0
    );
    assert_eq!(cursor.after_event, format!("{prefix}-199"));
    assert!(scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap() <= 100);
    assert_eq!(counts(&pool, community).await.0, 300);
    // A lease/process restart loses only this scalar work position.
    cursor = EndedScan::default();
    let mut saw_later_window = false;
    for _ in 0..200 {
        assert!(scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap() <= 100);
        saw_later_window |= cursor.occurrence_offset >= 200;
        if counts(&pool, community).await.0 == 455 {
            break;
        }
    }
    assert!(saw_later_window);
    assert_eq!(counts(&pool, community).await, (455, 500, 455));
    remove(&pool, community).await;
}

#[tokio::test]
async fn cyclic_scan_reconsiders_duration_edits_new_backdated_rows_and_recurrence_changes() {
    let _serial = ENDED_SCAN_TESTS.lock().await;
    let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
        return;
    };
    let now = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
        .unwrap()
        .naive_utc();
    let community = community(&pool, now).await;
    sqlx::query("UPDATE social.communities SET ranking_tracking_since=$2 WHERE id=$1")
        .bind(community)
        .bind(now - Duration::hours(1))
        .execute(&pool)
        .await
        .unwrap();
    let prefix = format!("r16-edit-{}", Uuid::new_v4());
    let edited = format!("{prefix}-z");
    let start = now - Duration::minutes(30);
    event(&pool, community, &edited, &[start], 7_200_000).await;
    let mut cursor = EndedScan {
        after_event: prefix.clone(),
        ..Default::default()
    };
    scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap();
    assert_eq!(counts(&pool, community).await, (0, 0, 0));
    sqlx::query("UPDATE explore.event SET duration_ms=300000,finish_at=start_at+interval '5 minutes' WHERE id=$1")
        .bind(&edited).execute(&pool).await.unwrap();
    event(
        &pool,
        community,
        &format!("{prefix}-a"),
        &[now - Duration::minutes(20)],
        1000,
    )
    .await;
    event(
        &pool,
        community,
        &format!("{prefix}-old"),
        &[now - Duration::hours(2)],
        1000,
    )
    .await;
    for _ in 0..200 {
        scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap();
        if counts(&pool, community).await.0 == 2 {
            break;
        }
    }
    assert_eq!(counts(&pool, community).await, (2, 0, 2));
    sqlx::query("UPDATE explore.event SET raw=jsonb_set(raw,'{recurrent_dates}',$2),finish_at=$3 WHERE id=$1")
        .bind(&edited).bind(json!([start.and_utc().to_rfc3339(),(now-Duration::minutes(10)).and_utc().to_rfc3339()]))
        .bind(now-Duration::minutes(5)).execute(&pool).await.unwrap();
    for _ in 0..200 {
        scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap();
        if counts(&pool, community).await.0 == 3 {
            break;
        }
    }
    assert_eq!(counts(&pool, community).await, (3, 0, 3));
    // An earlier source key inserted after the first visit was not skipped,
    // activation history was not imported, and the unchanged start counted once.
    remove(&pool, community).await;
}

#[tokio::test]
async fn failed_and_cancelled_pages_retain_the_cursor_and_roll_back_receipts_with_counters() {
    let _serial = ENDED_SCAN_TESTS.lock().await;
    let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
        return;
    };
    let now = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
        .unwrap()
        .naive_utc();
    let community = community(&pool, now).await;
    let prefix = format!("r16-failure-{}", Uuid::new_v4());
    event(
        &pool,
        community,
        &format!("{prefix}-a"),
        &[now - Duration::hours(2)],
        1000,
    )
    .await;
    let function = format!("r16_failure_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE FUNCTION social.{function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'ranking scan fault'; END $$; CREATE TRIGGER {function} BEFORE UPDATE ON social.communities FOR EACH ROW WHEN (NEW.id='{community}') EXECUTE FUNCTION social.{function}()")))
        .execute(&pool).await.unwrap();
    let mut cursor = EndedScan {
        after_event: prefix,
        ..Default::default()
    };
    let before = cursor.clone();
    assert!(scan_ended(&pool, now, &mut cursor).await.is_err());
    assert_eq!(cursor, before);
    assert_eq!(counts(&pool, community).await, (0, 0, 0));
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {function} ON social.communities; DROP FUNCTION social.{function}()"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let cancellation_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let cancellation_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&cancellation_pool)
        .await
        .unwrap();
    let mut barrier = pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *barrier)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM social.communities WHERE id=$1 FOR UPDATE")
        .bind(community)
        .execute(&mut *barrier)
        .await
        .unwrap();
    tokio::time::pause();
    let clock = crate::shared::test_wait::hold_paused_clock();
    let mut pending = Box::pin(DISABLE_ENDED_STATEMENT_TIMEOUT.scope(
        (),
        scan_ended_page(&cancellation_pool, now, &mut cursor, 100),
    ));
    tokio::select! {
        result=&mut pending=>panic!("scan must reach its row barrier: {result:?}"),
        ()=wait_for_backend_block(&pool,blocker,cancellation_pid)=>{},
    }
    drop(pending);
    assert_eq!(cursor, before);
    barrier.commit().await.unwrap();
    sqlx::query("SELECT 1")
        .execute(&cancellation_pool)
        .await
        .unwrap();
    assert_eq!(counts(&pool, community).await, (0, 0, 0));
    tokio::time::resume();
    drop(clock);
    let mut barrier = pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *barrier)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM social.communities WHERE id=$1 FOR UPDATE")
        .bind(community)
        .execute(&mut *barrier)
        .await
        .unwrap();
    assert_eq!(scan_ended(&pool, now, &mut cursor).await.unwrap(), 0);
    assert_eq!(cursor, before);
    assert_eq!(counts(&pool, community).await, (0, 0, 0));
    let cancellation_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&cancellation_pool)
        .await
        .unwrap();
    tokio::time::pause();
    let clock = crate::shared::test_wait::hold_paused_clock();
    let mut pending = Box::pin(
        DISABLE_ENDED_STATEMENT_TIMEOUT.scope((), scan_ended(&cancellation_pool, now, &mut cursor)),
    );
    tokio::select! {
        result=&mut pending=>panic!("scan must reach its cancellation barrier: {result:?}"),
        ()=wait_for_backend_block(&pool,blocker,cancellation_pid)=>{},
    }
    let mut delayed_cursor = before.clone();
    tokio::select! {
        result=&mut pending=>panic!("scan must survive the observer's server deadline: {result:?}"),
        result=scan_ended_page(&pool,now,&mut delayed_cursor,100)=>{
            let error = result.unwrap_err();
            let error = error.downcast_ref::<sqlx::Error>().unwrap().as_database_error().unwrap();
            assert_eq!(error.code().as_deref(),Some("57014"));
            assert_eq!(error.message(),"canceling statement due to statement timeout");
        },
    }
    assert_eq!(delayed_cursor, before);
    assert_eq!(counts(&pool, community).await, (0, 0, 0));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT $1=ANY(pg_blocking_pids($2))")
            .bind(blocker)
            .bind(cancellation_pid)
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT pg_cancel_backend($1)")
            .bind(cancellation_pid)
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    let error = pending.await.unwrap_err();
    let error = error
        .downcast_ref::<sqlx::Error>()
        .unwrap()
        .as_database_error()
        .unwrap();
    assert_eq!(error.code().as_deref(), Some("57014"));
    assert_eq!(error.message(), "canceling statement due to user request");
    cancellation_pool.close().await;
    assert_eq!(cursor, before);
    assert_eq!(counts(&pool, community).await, (0, 0, 0));
    tokio::time::resume();
    drop(clock);
    barrier.commit().await.unwrap();
    scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap();
    assert_ne!(cursor, before);
    assert_eq!(counts(&pool, community).await, (1, 0, 1));
    cursor = before;
    scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap();
    assert_eq!(counts(&pool, community).await, (1, 0, 1));
    remove(&pool, community).await;
}

#[tokio::test]
async fn pass_shares_one_deadline_and_receipt_budget_across_pages_without_losing_progress() {
    let _serial = ENDED_SCAN_TESTS.lock().await;
    let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
        return;
    };
    let now = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
        .unwrap()
        .naive_utc();
    let community = community(&pool, now).await;
    let prefix = format!("r16-pass-{}", Uuid::new_v4());
    let start = now - Duration::hours(2);
    sqlx::query("INSERT INTO explore.event(id,name,start_at,finish_at,duration_ms,approved,community_id,raw,origin) SELECT $1||'-'||lpad(n::text,3,'0'),'Due pages',$2,$2+interval '1 second',1000,true,$3,jsonb_build_object('start_at',$4::text),'local' FROM generate_series(0,204) n")
        .bind(&prefix).bind(start).bind(community.to_string()).bind(start.and_utc().to_rfc3339()).execute(&pool).await.unwrap();
    let mut cursor = EndedScan {
        after_event: prefix.clone(),
        ..Default::default()
    };
    let before = cursor.clone();
    let mut connections = Vec::new();
    for _ in 0..4 {
        connections.push(pool.acquire().await.unwrap());
    }
    // An occupied real pool leaves the pass waiting at its first acquire.
    // Advance only its clock, rather than sleeping through the three seconds.
    tokio::time::pause();
    let mut pending = Box::pin(scan_ended(&pool, now, &mut cursor));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    assert_eq!(pending.await.unwrap(), 0);
    tokio::time::resume();
    drop(connections);
    assert_eq!(cursor, before);
    assert_eq!(scan_ended(&pool, now, &mut cursor).await.unwrap(), 100);
    assert_eq!(cursor.after_event, format!("{prefix}-099"));
    assert_eq!(counts(&pool, community).await, (100, 0, 100));
    assert_eq!(scan_ended(&pool, now, &mut cursor).await.unwrap(), 100);
    assert_eq!(cursor.after_event, format!("{prefix}-199"));
    assert_eq!(scan_ended(&pool, now, &mut cursor).await.unwrap(), 5);
    assert_eq!(counts(&pool, community).await, (205, 0, 205));
    assert!(cursor.after_event.is_empty());
    assert_eq!(scan_ended(&pool, now, &mut cursor).await.unwrap(), 0);
    assert_eq!(counts(&pool, community).await, (205, 0, 205));
    remove(&pool, community).await;
}

#[tokio::test]
async fn statement_budget_keeps_prior_page_progress_and_resumes_the_rolled_back_page() {
    let _serial = ENDED_SCAN_TESTS.lock().await;
    let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
        return;
    };
    let now = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
        .unwrap()
        .naive_utc();
    let committed = community(&pool, now).await;
    let blocked = community(&pool, now).await;
    let prefix = format!("r17-budget-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO explore.event(id,name,start_at,finish_at,duration_ms,approved,community_id,raw,origin) SELECT $1||'-'||lpad(n::text,3,'0'),'Statement budget',$2,$2+interval '1 second',1000,true,CASE WHEN n=101 THEN $4 ELSE $3 END,jsonb_build_object('start_at',$5::text),'local' FROM generate_series(1,101) n")
        .bind(&prefix).bind(now-Duration::hours(2)).bind(committed.to_string())
        .bind(blocked.to_string()).bind((now-Duration::hours(2)).and_utc().to_rfc3339())
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO social.community_ranking_events(kind,event_key,community_id,occurred_at) SELECT 'event',id||':'||start_at::text,$1,finish_at FROM explore.event WHERE community_id=$2 AND id<$3")
        .bind(committed).bind(committed.to_string()).bind(format!("{prefix}-100"))
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE social.communities SET ranking_events_count=99 WHERE id=$1")
        .bind(committed)
        .execute(&pool)
        .await
        .unwrap();
    let mut barrier = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM social.communities WHERE id=$1 FOR UPDATE")
        .bind(blocked)
        .execute(&mut *barrier)
        .await
        .unwrap();
    let mut cursor = EndedScan {
        after_event: prefix,
        ..Default::default()
    };
    assert_eq!(scan_ended(&pool, now, &mut cursor).await.unwrap(), 1);
    assert!(cursor.after_event.ends_with("-100"));
    assert_eq!(counts(&pool, committed).await, (100, 0, 100));
    assert_eq!(counts(&pool, blocked).await, (0, 0, 0));
    barrier.commit().await.unwrap();
    assert_eq!(
        scan_ended_page(&pool, now, &mut cursor, 100).await.unwrap(),
        1
    );
    assert_eq!(counts(&pool, committed).await, (100, 0, 100));
    assert_eq!(counts(&pool, blocked).await, (1, 0, 1));
    remove(&pool, committed).await;
    remove(&pool, blocked).await;
}

#[tokio::test]
async fn hundred_thousand_completed_events_use_bounded_pages_and_finish_a_cycle_without_writes() {
    // An owned database keeps this history workload and its cursor independent
    // of the other scanner fixtures and their deliberate failure barriers.
    let Some(database) =
        crate::db::scratch::OwnedDatabase::new("ranking", crate::db::scratch::Schema::Full, 2)
            .await
    else {
        return;
    };
    let pool = database.pool.clone();
    let now = chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
        .unwrap()
        .naive_utc();
    let community = community(&pool, now).await;
    let start = now - Duration::hours(2);
    sqlx::query("INSERT INTO explore.event(id,name,start_at,finish_at,duration_ms,approved,community_id,raw,origin) SELECT 'history-'||lpad(n::text,6,'0'),'Completed history',$1,$1+interval '1 second',1000,true,$2,jsonb_build_object('start_at',$3::text,'description',repeat('x',128)),'local' FROM generate_series(1,100000) n")
        .bind(start).bind(community.to_string()).bind(start.and_utc().to_rfc3339()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO social.community_ranking_events(kind,event_key,community_id,occurred_at) SELECT 'event',id||':'||start_at::text,$1,finish_at FROM explore.event")
        .bind(community).execute(&pool).await.unwrap();
    sqlx::query("UPDATE social.communities SET ranking_events_count=100000 WHERE id=$1")
        .bind(community)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("ANALYZE explore.event; ANALYZE social.communities; ANALYZE social.community_ranking_events; ANALYZE explore.event_attendance_local")
        .execute(&pool).await.unwrap();
    let mut cursor = EndedScan {
        after_event: "history-000000".to_owned(),
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let mut passes = 0;
    loop {
        passes += 1;
        assert_eq!(scan_ended(&pool, now, &mut cursor).await.unwrap(), 0);
        if cursor.after_event.is_empty() {
            break;
        }
        assert!(
            passes < 10,
            "each budgeted pass must retain useful progress"
        );
    }
    assert_eq!(cursor, EndedScan::default(), "the full cycle must wrap");
    let elapsed = started.elapsed();
    assert_eq!(counts(&pool, community).await, (100000, 0, 100000));
    eprintln!(
        "ranking 100k completed history: {passes} passes, {:.3}ms active scan, zero receipt/counter writes",
        elapsed.as_secs_f64() * 1000.0
    );
    database.drop().await;
}
