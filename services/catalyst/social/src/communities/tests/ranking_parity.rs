use super::super::ranking::{self, Metrics};
use super::*;

#[test]
fn ranking_scores_match_executed_pinned_source_goldens() {
    let fixture: Value = serde_json::from_str(include_str!(concat!(env!("DCL_ONE_ROOT"), "/services/catalyst/tests/fixtures/conformance/social/source-contracts/community-ranking.json"))).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let metrics: Metrics = serde_json::from_value(case["metrics"].clone()).unwrap();
        let expected = case["score"].as_f64().unwrap();
        assert!(
            (ranking::score(&metrics) - expected).abs() < 1e-14,
            "{}",
            case["name"]
        );
    }
}

async fn counters(rig: &Rig, id: CommunityId) -> (i32, i32, i32, i32, i32) {
    sqlx::query_as("SELECT ranking_events_count,ranking_photos_count,ranking_streams_count,ranking_events_total_attendees,ranking_streams_total_participants FROM social.communities WHERE id=$1")
        .bind(id).fetch_one(&rig.pool).await.unwrap()
}

#[tokio::test]
async fn ranking_event_handlers_preserve_clamps_receipts_fanout_and_atomic_retries() {
    use crate::comms::interfaces::SocialDirectory;
    let Some(rig) = Rig::new(Settings::for_tests()).await else {
        return;
    };
    let owner = wallet();
    let a = rig.community(&owner, false).await;
    let b = rig.community(&owner, false).await;
    let mirrored = rig.community(&owner, false).await;
    sqlx::query("UPDATE social.communities SET origin='upstream',ranking_score=0.85 WHERE id=$1")
        .bind(mirrored)
        .execute(&rig.pool)
        .await
        .unwrap();
    let place = Uuid::new_v4().to_string();
    for id in [a, b, mirrored] {
        sqlx::query(
            "INSERT INTO social.community_places(community_id,place_id,added_by) VALUES($1,$2,$3)",
        )
        .bind(id)
        .bind(&place)
        .bind(&owner.address)
        .execute(&rig.pool)
        .await
        .unwrap();
    }
    let now = chrono::Utc::now().naive_utc();
    let key = Uuid::new_v4().to_string();
    let mut rolled_back = rig.pool.begin().await.unwrap();
    ranking::record_photo(&mut rolled_back, &key, &place, now)
        .await
        .unwrap();
    rolled_back.rollback().await.unwrap();
    assert_eq!(counters(&rig, a).await.1, 0);
    for _ in 0..2 {
        let mut tx = rig.pool.begin().await.unwrap();
        ranking::record_photo(&mut tx, &key, &place, now)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(counters(&rig, a).await.1, 1);
    assert_eq!(counters(&rig, b).await.1, 1);
    assert_eq!(counters(&rig, mirrored).await.1, 0);
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM social.community_ranking_events WHERE kind='photo' AND event_key=$1",
    )
    .bind(&key)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    assert_eq!(
        receipts, 2,
        "photo fanout has one durable receipt per community"
    );
    let directory = crate::comms_directory::Directory::new(rig.pool.clone());
    let stream = json!({"metadata":{"communityId":a.as_uuid(),"totalParticipants":9999.8}});
    for _ in 0..2 {
        directory
            .record_community_voice_ended(a, &key, &stream, now)
            .await
            .unwrap();
    }
    assert_eq!(counters(&rig, a).await, (0, 1, 1, 0, 500));
    for (key, participants) in [("solo", 1.0), ("fractional", 1.9)] {
        directory
            .record_community_voice_ended(
                a,
                key,
                &json!({"metadata":{"totalParticipants":participants}}),
                now,
            )
            .await
            .unwrap();
    }
    assert_eq!(counters(&rig, a).await, (0, 1, 2, 0, 501));
    sqlx::query("UPDATE social.communities SET ranking_streams_count=2147483647,ranking_streams_total_participants=2147483640 WHERE id=$1").bind(a).execute(&rig.pool).await.unwrap();
    directory
        .record_community_voice_ended(a, "saturation", &stream, now)
        .await
        .unwrap();
    let values = counters(&rig, a).await;
    assert_eq!(
        (values.2, values.4),
        (i32::MAX, i32::MAX),
        "local additions saturate INT4"
    );
    assert_eq!(
        counters(&rig, mirrored).await,
        (0, 0, 0, 0, 0),
        "native contributions exclude upstream mirrors"
    );
    rig.finish();
}

#[tokio::test]
async fn local_event_ends_use_occurrence_receipts_local_attendees_and_the_tracking_boundary() {
    let _serial = ranking::ENDED_SCAN_TESTS.lock().await;
    use chrono::Duration;
    let Some(rig) = Rig::new(Settings::for_tests()).await else {
        return;
    };
    let owner = wallet();
    let id = rig.community(&owner, false).await;
    let now = chrono::Utc::now().naive_utc();
    let start = now - Duration::hours(2);
    sqlx::query("UPDATE social.communities SET ranking_tracking_since=$2 WHERE id=$1")
        .bind(id)
        .bind(now - Duration::hours(3))
        .execute(&rig.pool)
        .await
        .unwrap();
    let event_id = format!("ranking-{}", Uuid::new_v4());
    let raw = json!({"id":event_id,"start_at":start.and_utc().to_rfc3339(),"duration":3600000,"total_attendees":999,"recurrent_dates":[(now-Duration::hours(10)).and_utc().to_rfc3339(),start.and_utc().to_rfc3339()],"approved":true,"community_id":id.as_uuid()});
    sqlx::query("INSERT INTO explore.event(id,name,start_at,finish_at,duration_ms,approved,community_id,raw,origin) VALUES($1,'Ranking event',$2,$3,3600000,true,$4,$5,'local')")
        .bind(&event_id).bind(start).bind(start+Duration::hours(1)).bind(id.to_string()).bind(&raw).execute(&rig.pool).await.unwrap();
    for i in 0..4 {
        let address = wallet().address;
        sqlx::query(
            "INSERT INTO explore.event_attendance_local(event_id,signer,action) VALUES($1,$2,$3)",
        )
        .bind(&event_id)
        .bind(address)
        .bind(if i == 3 { "cancel" } else { "going" })
        .execute(&rig.pool)
        .await
        .unwrap();
    }
    let mut cursor = ranking::EndedScan::default();
    for _ in 0..2 {
        ranking::scan_ended(&rig.pool, now, &mut cursor)
            .await
            .unwrap();
    }
    assert_eq!(
        counters(&rig, id).await,
        (1, 0, 0, 3, 0),
        "one eligible occurrence; local going rows supply attendees"
    );
    let future = now + Duration::hours(10);
    ranking::scan_ended(&rig.pool, future, &mut cursor)
        .await
        .unwrap();
    assert_eq!(
        counters(&rig, id).await.0,
        1,
        "retained receipts survive scanner catch-up"
    );
    let expected = ranking::score(&Metrics {
        events_count: 1,
        events_total_attendees: 3,
        new_members_count: 1,
        has_description: 1,
        ..Default::default()
    }) as f32;
    ranking::recalculate_ids(&rig.pool, &[id.as_uuid()], now)
        .await
        .unwrap();
    let actual: f32 =
        sqlx::query_scalar("SELECT ranking_score FROM social.communities WHERE id=$1")
            .bind(id)
            .fetch_one(&rig.pool)
            .await
            .unwrap();
    assert_eq!(actual, expected);
    sqlx::query("DELETE FROM explore.event_attendance_local WHERE event_id=$1")
        .bind(&event_id)
        .execute(&rig.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM explore.event WHERE id=$1")
        .bind(&event_id)
        .execute(&rig.pool)
        .await
        .unwrap();
    rig.finish();
}

#[tokio::test]
async fn ranking_upgrade_retains_nonempty_rows_and_starts_tracking_without_replaying_history() {
    let Some(database) = crate::db::scratch::OwnedDatabase::new(
        "ranking_upgrade",
        crate::db::scratch::Schema::Blank,
        2,
    )
    .await
    else {
        return;
    };
    let pool = database.pool.clone();
    sqlx::raw_sql("CREATE SCHEMA meta; CREATE TABLE meta.schema(id integer PRIMARY KEY,identity text NOT NULL,version integer NOT NULL,applied_at timestamp NOT NULL DEFAULT timezone('UTC',now()));
        CREATE SCHEMA social; CREATE SCHEMA camera_reel;
        CREATE TABLE social.communities(id uuid PRIMARY KEY, ranking_score real NOT NULL DEFAULT 0, retained jsonb NOT NULL);
        CREATE TABLE camera_reel.images(id uuid PRIMARY KEY, retained jsonb NOT NULL);
        INSERT INTO social.communities VALUES('00000000-0000-0000-0000-000000000001',0.85,'{\"source\":\"imported\"}');
        INSERT INTO camera_reel.images VALUES('00000000-0000-0000-0000-000000000002','{\"photo\":\"old\"}');")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO meta.schema(id,identity,version) VALUES(1,$1,$2)")
        .bind(crate::db::SCHEMA_IDENTITY)
        .bind(crate::db::SCHEMA_VERSION)
        .execute(&pool)
        .await
        .unwrap();
    let canonical = crate::db::schema_migrate::statements(crate::db::SCHEMA_SQL);
    let object = |prefix: &str| {
        canonical
            .iter()
            .find(|sql| sql.starts_with(prefix))
            .unwrap()
            .clone()
    };
    let target = format!("CREATE SCHEMA social; CREATE SCHEMA camera_reel;
        CREATE TABLE social.communities(id uuid PRIMARY KEY,ranking_score real NOT NULL DEFAULT 0,retained jsonb NOT NULL,
        ranking_events_count integer NOT NULL DEFAULT 0,ranking_photos_count integer NOT NULL DEFAULT 0,
        ranking_streams_count integer NOT NULL DEFAULT 0,ranking_events_total_attendees integer NOT NULL DEFAULT 0,
        ranking_streams_total_participants integer NOT NULL DEFAULT 0,ranking_tracking_since timestamp NOT NULL DEFAULT timezone('UTC',now()));
        CREATE TABLE camera_reel.images(id uuid PRIMARY KEY,retained jsonb NOT NULL,ranking_published boolean NOT NULL DEFAULT true);
        {}; {}; {}; {}; {};",object("CREATE TABLE meta.schema_steps"),object("CREATE TABLE social.community_ranking_events"),object("CREATE INDEX images_ranking_pending_idx"),object("CREATE TABLE social.community_compliance_cache"),object("CREATE INDEX community_compliance_cache_expiry_idx"));
    let migrations = concat!(
        "-- ════ step 0022_social_community_ranking ════\n",
        include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/schemas/migrations/0022_social_community_ranking.sql"
        ))
    );
    let before = chrono::Utc::now().naive_utc();
    let done = crate::db::schema_migrate::migrate(&pool, &target, migrations)
        .await
        .unwrap();
    let after = chrono::Utc::now().naive_utc();
    assert_eq!(done.steps, vec!["0022_social_community_ranking"]);
    let cached: i64 = sqlx::query_scalar("SELECT count(*) FROM social.community_compliance_cache")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        cached, 0,
        "new cache objects are created by the canonical object pass on a nonempty database"
    );
    assert!(
        crate::db::schema_migrate::migrate(&pool, &target, migrations)
            .await
            .unwrap()
            .is_empty(),
        "recorded migration never replays"
    );
    let row:(f32,Value,i32,i32,i32,i32,i32,chrono::NaiveDateTime)=sqlx::query_as("SELECT ranking_score,retained,ranking_events_count,ranking_photos_count,ranking_streams_count,ranking_events_total_attendees,ranking_streams_total_participants,ranking_tracking_since FROM social.communities").fetch_one(&pool).await.unwrap();
    assert_eq!(row.0, 0.85);
    assert_eq!(row.1, json!({"source":"imported"}));
    assert_eq!((row.2, row.3, row.4, row.5, row.6), (0, 0, 0, 0, 0));
    assert!(row.7 >= before && row.7 <= after);
    let old: (Value, bool) =
        sqlx::query_as("SELECT retained,ranking_published FROM camera_reel.images")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(old, (json!({"photo":"old"}), true));
    assert!(
        sqlx::query("UPDATE social.communities SET ranking_events_count=-1")
            .execute(&pool)
            .await
            .is_err()
    );
    let index:String=sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname='camera_reel' AND indexname='images_ranking_pending_idx'").fetch_one(&pool).await.unwrap();
    assert!(index.contains("WHERE (NOT ranking_published)"));
    database.drop().await;
}
