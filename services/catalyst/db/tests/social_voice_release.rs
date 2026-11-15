//! Exercise the dormant v3 release against genuine v2 layouts and receipts.
//! Current v3 apply/migrate/bootstrap execution is checked in social_voice_upgrade.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use sha2::{Digest, Sha256};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

const STEP: &str = "0020_social_voice_notifications";
const STEP_HASH: &str = "41c5172fe07ee74acad62a9e1fa3ae74e5eaeb51d2a13262146f20034faaef17";
const PREPARED_V2: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/services/catalyst/tests/fixtures/schema/v2-prepared-c1fab3991.sql"
));
const MAIN_V2: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/services/catalyst/tests/fixtures/schema/v2-main-0fb63aa3f.sql"
));
const UPGRADE_ROWS: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/services/catalyst/tests/fixtures/schema/v2-upgrade-rows-c1fab3991.sql"
));
const ORIGIN_ROWS: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/services/catalyst/tests/fixtures/schema/v2-origin-rows-c1fab3991.sql"
));
const RELEASE_V3: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/schemas/releases/v3-private-voice-pending.sql"
));
const VOICE_FACTS: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/schemas/releases/v3/41_social_voice_notifications.sql"
));
const PRIVATE_INDEXES: &str = include_str!(concat!(
    env!("DCL_ONE_ROOT"),
    "/schemas/releases/v3/42_social_private_voice_indexes.sql"
));
const LEDGER: &str =
    "SELECT step,sha256,created_fresh,applied_at FROM meta.schema_steps ORDER BY step";
type Receipt = (String, String, bool, chrono::NaiveDateTime);

#[derive(Clone, Copy, Debug)]
enum Source {
    Main,
    Prepared,
}
impl Source {
    fn schema(self) -> &'static str {
        match self {
            Self::Main => MAIN_V2,
            Self::Prepared => PREPARED_V2,
        }
    }
}
fn assert_capture_hash(text: &str, sha256: &str) {
    assert_eq!(hex::encode(Sha256::digest(text.as_bytes())), sha256);
}
fn release_v3() -> String {
    let stop_on_error = "\\set ON_ERROR_STOP on\n";
    assert_eq!(RELEASE_V3.matches(stop_on_error).count(), 1);
    let mut release = RELEASE_V3.replace(stop_on_error, "");
    for (directive, canonical) in [
        ("\\ir v3/41_social_voice_notifications.sql", VOICE_FACTS),
        (
            "\\ir v3/42_social_private_voice_indexes.sql",
            PRIVATE_INDEXES,
        ),
    ] {
        assert_eq!(release.matches(directive).count(), 1);
        release = release.replace(directive, canonical);
    }
    release
}

struct Database {
    pool: PgPool,
    admin: PgPool,
    name: String,
    url: String,
}
impl Database {
    async fn empty() -> Option<Self> {
        let url = std::env::var("CATALYST_TEST_DATABASE_URL").ok()?;
        let options: PgConnectOptions = url.parse().unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone().database("postgres"))
            .await
            .unwrap();
        let name = format!(
            "catalyst_test_lib_{}_social_voice_{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(
                options
                    .database(&name)
                    .options([("TimeZone", "UTC"), ("search_path", "content,meta")]),
            )
            .await
            .unwrap();
        let mut psql_url = url::Url::parse(&url).unwrap();
        psql_url.set_path(&name);
        Some(Self {
            pool,
            admin,
            name,
            url: psql_url.to_string(),
        })
    }
    async fn new(source: Source) -> Option<Self> {
        // Hashes are the untouched output digests in the adjacent provenance
        // manifests; import their real ledger, never invent historical receipts.
        assert_capture_hash(
            MAIN_V2,
            "0a7e97ef603c04b9ee0f956d8242fd301aa523f94c6b1fec335264eaa9b60be3",
        );
        assert_capture_hash(
            PREPARED_V2,
            "3242308447fdc6294c7c100d1265eaba911f56dfaf1c03a463dcfc542c6328c6",
        );
        assert_capture_hash(
            UPGRADE_ROWS,
            "c8354451493799ce31e99c9369fc247021aa61045dedd9682709e2bbff0396bf",
        );
        assert_capture_hash(
            ORIGIN_ROWS,
            "869841157fcb18ddb32fd030f6920127c41142656f91046ae426be0dd1ab3925",
        );
        let db = Self::empty().await?;
        // One capture import at a time per process, like `apply_schema`: each
        // runs as one implicit transaction holding about 3,000 object locks
        // until commit, so overlapping imports exhaust a default cluster's
        // shared lock table ("out of shared memory"). The releases under test
        // take about 70 and still run in parallel.
        {
            static IMPORTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
            let _one_at_a_time = IMPORTING.lock().await;
            sqlx::raw_sql(sqlx::AssertSqlSafe(source.schema().to_owned()))
                .execute(&db.pool)
                .await
                .unwrap();
            if matches!(source, Source::Prepared) {
                for rows in [UPGRADE_ROWS, ORIGIN_ROWS] {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(rows.to_owned()))
                        .execute(&db.pool)
                        .await
                        .unwrap();
                }
            }
        }
        let receipts: Vec<Receipt> = sqlx::query_as(LEDGER).fetch_all(&db.pool).await.unwrap();
        match source {
            Source::Main => assert!(!receipts.iter().any(|row| row.0 == STEP)),
            Source::Prepared => assert!(
                receipts
                    .iter()
                    .any(|row| row.0 == STEP && row.1 == STEP_HASH && !row.2)
            ),
        }
        Some(db)
    }
    async fn close(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{}\" WITH(FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}
async fn release_result(pool: &PgPool, release: &str) -> Result<(), sqlx::Error> {
    let mut connection = pool.acquire().await?;
    let result = sqlx::raw_sql(sqlx::AssertSqlSafe(release.to_owned()))
        .execute(&mut *connection)
        .await
        .map(|_| ());
    if result.is_err() {
        sqlx::query("ROLLBACK").execute(&mut *connection).await?;
    }
    result
}
async fn assert_release_indexes(pool: &PgPool) {
    let indexes: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='social.private_voice_chats'::regclass AND c.relname IN ('private_voice_chats_caller_idx','private_voice_chats_callee_idx') AND NOT i.indisunique AND i.indisvalid").fetch_one(pool).await.unwrap();
    assert_eq!(
        indexes, 2,
        "both replacement indexes exist at release commit"
    );
}
async fn preserved_facts(pool: &PgPool) -> Vec<(String, String)> {
    let mut facts = Vec::new();
    // Every table populated by either genuine application-row capture, plus
    // seeded private intent/media facts. JSONB retains each value exactly.
    for table in [
        "credits.purchases",
        "worlds.worlds",
        "explore.event_profile_settings",
        "explore.event",
        "explore.event_moderation",
        "explore.events_local",
        "explore.place",
        "social.communities",
        "social.community_members",
        "social.private_voice_chats",
        "social.private_voice_chat_participants",
    ] {
        let projection = if table == "social.communities" {
            "to_jsonb(t)-'voice_notification_scope'-'voice_notification_generation'-'voice_notification_expires_at'"
        } else {
            "to_jsonb(t)"
        };
        let sql = format!(
            "SELECT coalesce(jsonb_agg({projection} ORDER BY {projection}), '[]'::jsonb)::text FROM {table} t"
        );
        facts.push((
            table.into(),
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .fetch_one(pool)
                .await
                .unwrap(),
        ));
    }
    facts
}
async fn seed_voice_facts(pool: &PgPool, source: Source) {
    // Synthetic edge facts supplement captured rows; they do not alter the
    // historical stamp or ledger and use the source layout's own constraints.
    // The accepted call shares a wallet across roles with the pending call. Its
    // participant rows exclude it from the pending-only normalization/overlap audit.
    sqlx::raw_sql("INSERT INTO social.communities(id,name,owner_address) VALUES ('a1111111-b111-4111-8111-e11111111111','Existing community','owner'); INSERT INTO social.community_members(community_id,member_address,role) VALUES ('a1111111-b111-4111-8111-e11111111111','owner','owner'); INSERT INTO social.private_voice_chats(id,caller_address,callee_address) VALUES ('11111111-1111-4111-8111-111111111111','pending-caller','pending-callee'),('22222222-2222-4222-8222-222222222222','media-caller','pending-caller'); INSERT INTO social.private_voice_chat_participants(call_id,address,status) VALUES ('22222222-2222-4222-8222-222222222222','media-caller','connection_interrupted'),('22222222-2222-4222-8222-222222222222','pending-caller','not_connected')").execute(pool).await.unwrap();
    if matches!(source, Source::Prepared) {
        sqlx::raw_sql("INSERT INTO social.communities(id,name,owner_address,voice_notification_scope,voice_notification_generation,voice_notification_expires_at) VALUES ('a2222222-b222-4222-8222-e22222222222','Live generation','owner','all','11111111-1111-4111-8111-111111111111','2100-01-02 03:04:05.123456'),('a3333333-b333-4333-8333-e33333333333','Expired generation','owner','members','22222222-2222-4222-8222-222222222222','2000-01-02 03:04:05.654321')").execute(pool).await.unwrap();
    }
}
async fn notification_facts(pool: &PgPool, source: Source) -> String {
    if matches!(source, Source::Main) {
        return "[]".into();
    }
    sqlx::query_scalar("SELECT coalesce(jsonb_agg(jsonb_build_array(id::text,id,voice_notification_scope,voice_notification_generation,voice_notification_expires_at) ORDER BY id),'[]'::jsonb)::text FROM social.communities WHERE voice_notification_generation IS NOT NULL").fetch_one(pool).await.unwrap()
}
#[tokio::test]
async fn both_genuine_v2_routes_roll_back_at_every_boundary_then_preserve_rows_and_real_ledger() {
    let release = release_v3();
    for source in [Source::Main, Source::Prepared] {
        for boundary in [
            "-- Cutover boundary: destination created.",
            "-- Cutover boundary: legacy facts verified.",
            "-- Cutover boundary: replacement indexes created.",
            "UPDATE meta.schema SET version = 3",
            "COMMIT;",
        ] {
            let Some(db) = Database::new(source).await else {
                return;
            };
            let pool = &db.pool;
            seed_voice_facts(pool, source).await;
            let facts = preserved_facts(pool).await;
            let notifications = notification_facts(pool, source).await;
            let ledger: Vec<Receipt> = sqlx::query_as(LEDGER).fetch_all(pool).await.unwrap();
            let mut interrupted = pool.acquire().await.unwrap();
            sqlx::raw_sql(sqlx::AssertSqlSafe(
                release[..release.find(boundary).unwrap()].to_owned(),
            ))
            .execute(&mut *interrupted)
            .await
            .unwrap();
            if boundary == "-- Cutover boundary: replacement indexes created." {
                let (version,uniques,indexes): (i32,i64,i64) = sqlx::query_as("SELECT (SELECT version FROM meta.schema WHERE id=1),(SELECT count(*) FROM pg_constraint WHERE conrelid='social.private_voice_chats'::regclass AND conname IN ('private_voice_chats_caller_key','private_voice_chats_callee_key')),(SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid='social.private_voice_chats'::regclass AND c.relname IN ('private_voice_chats_caller_idx','private_voice_chats_callee_idx') AND NOT i.indisunique AND i.indisvalid)").fetch_one(&mut *interrupted).await.unwrap();
                assert_eq!(
                    (version, uniques, indexes),
                    (2, 2, 2),
                    "replacement indexes precede UNIQUE removal and final stamp"
                );
            }
            interrupted.close().await.unwrap();
            let (version,target,legacy,uniques): (i32,bool,i64,i64) = sqlx::query_as("SELECT (SELECT version FROM meta.schema WHERE id=1),to_regclass('social.community_voice_notifications') IS NOT NULL,(SELECT count(*) FROM pg_attribute WHERE attrelid='social.communities'::regclass AND NOT attisdropped AND attname LIKE 'voice_notification_%'),(SELECT count(*) FROM pg_constraint WHERE conrelid='social.private_voice_chats'::regclass AND conname IN ('private_voice_chats_caller_key','private_voice_chats_callee_key'))").fetch_one(pool).await.unwrap();
            assert_eq!(
                (version, target, legacy, uniques),
                (
                    2,
                    false,
                    if matches!(source, Source::Prepared) {
                        3
                    } else {
                        0
                    },
                    2
                ),
                "{source:?}: {boundary}"
            );
            assert_eq!(preserved_facts(pool).await, facts);
            assert_eq!(notification_facts(pool, source).await, notifications);
            assert_eq!(
                sqlx::query_as::<_, Receipt>(LEDGER)
                    .fetch_all(pool)
                    .await
                    .unwrap(),
                ledger
            );
            release_result(pool, &release).await.unwrap();
            assert_release_indexes(pool).await;
            assert_eq!(
                sqlx::query_scalar::<_, i32>("SELECT version FROM meta.schema WHERE id=1")
                    .fetch_one(pool)
                    .await
                    .unwrap(),
                3
            );
            let copied: String = sqlx::query_scalar("SELECT coalesce(jsonb_agg(jsonb_build_array(raw_community_id,community_id,notification_scope,generation,expires_at) ORDER BY community_id),'[]'::jsonb)::text FROM social.community_voice_notifications").fetch_one(pool).await.unwrap();
            assert_eq!(
                copied, notifications,
                "scope, generation and exact TTL, including expired facts"
            );
            assert_server_message(
                release_result(pool, &release).await,
                "schema release v3 requires catalyst version 2",
            );
            assert_eq!(preserved_facts(pool).await, facts);
            assert_eq!(
                sqlx::query_as::<_, Receipt>(LEDGER)
                    .fetch_all(pool)
                    .await
                    .unwrap(),
                ledger,
                "no invented, retired or rewritten historical receipt"
            );
            db.close().await;
        }
    }
}

#[tokio::test]
async fn release_refuses_partial_or_unreceipted_legacy_layout_and_changed_hash() {
    for (corruption, expected) in [
        (
            "ALTER TABLE social.communities DROP COLUMN voice_notification_scope CASCADE",
            "schema release v3 requires the complete prepared v2 notification layout with its historical 0020 receipt; no data changed",
        ),
        (
            "DELETE FROM meta.schema_steps WHERE step='0020_social_voice_notifications'",
            "schema release v3 found legacy notification layout without its historical 0020 receipt; no data changed",
        ),
        (
            "ALTER TABLE social.communities DROP CONSTRAINT communities_voice_notification_scope_check; ALTER TABLE social.communities ADD CONSTRAINT communities_voice_notification_scope_check CHECK (true); INSERT INTO social.communities(id,name,owner_address,voice_notification_scope) VALUES ('a4444444-b444-4444-8444-e44444444444','incomplete fact','owner','all')",
            "schema release v3 found incomplete legacy notification facts; no data changed",
        ),
        (
            "ALTER TABLE social.communities DROP CONSTRAINT communities_voice_notification_scope_check; ALTER TABLE social.communities ADD CONSTRAINT communities_voice_notification_scope_check CHECK (true); INSERT INTO social.communities(id,name,owner_address,voice_notification_generation,voice_notification_expires_at) VALUES ('a4444444-b444-4444-8444-e44444444444','missing scope','owner','44444444-4444-4444-8444-444444444444','2100-01-02')",
            "schema release v3 found incomplete legacy notification facts; no data changed",
        ),
        (
            "ALTER TABLE social.communities DROP CONSTRAINT communities_voice_notification_scope_check; ALTER TABLE social.communities ADD CONSTRAINT communities_voice_notification_scope_check CHECK (true); INSERT INTO social.communities(id,name,owner_address,voice_notification_generation,voice_notification_scope) VALUES ('a4444444-b444-4444-8444-e44444444444','missing expiry','owner','44444444-4444-4444-8444-444444444444','all')",
            "schema release v3 found incomplete legacy notification facts; no data changed",
        ),
        (
            "UPDATE meta.schema_steps SET sha256=repeat('0',64) WHERE step='0020_social_voice_notifications'",
            "schema release v3 historical 0020 digest differs; no data changed",
        ),
    ] {
        let Some(db) = Database::new(Source::Prepared).await else {
            return;
        };
        sqlx::raw_sql(sqlx::AssertSqlSafe(corruption.to_owned()))
            .execute(&db.pool)
            .await
            .unwrap();
        let before = v2_state(&db.pool).await;
        assert_server_message(release_result(&db.pool, &release_v3()).await, expected);
        assert_eq!(v2_state(&db.pool).await, before);
        db.close().await;
    }
}

#[tokio::test]
async fn release_refuses_overlapping_or_non_normalized_pending_without_mutation() {
    for source in [Source::Main, Source::Prepared] {
        for (caller, callee, expected) in [
            (
                "caller",
                "caller",
                "schema release v3 found overlapping pending wallets; resolve retained state before cutover; no data changed",
            ),
            (
                "MixedCaller",
                "other-callee",
                "schema release v3 found non-normalized pending wallets; resolve retained state before cutover; no data changed",
            ),
            (
                "caller",
                "MixedCallee",
                "schema release v3 found non-normalized pending wallets; resolve retained state before cutover; no data changed",
            ),
        ] {
            let Some(db) = Database::new(source).await else {
                return;
            };
            sqlx::query("INSERT INTO social.private_voice_chats(id,caller_address,callee_address) VALUES($1,$2,'callee'),($3,'other-caller',$4)").bind(uuid::Uuid::new_v4()).bind(caller).bind(uuid::Uuid::new_v4()).bind(callee).execute(&db.pool).await.unwrap();
            let before = v2_state(&db.pool).await;
            assert_server_message(release_result(&db.pool, &release_v3()).await, expected);
            assert_eq!(v2_state(&db.pool).await, before);
            db.close().await;
        }
    }
}

fn assert_server_message(result: Result<(), sqlx::Error>, expected: &str) {
    let error = result.unwrap_err();
    let server = error
        .as_database_error()
        .expect("a server refusal, not a client or SQL composition failure");
    assert_eq!(server.code().as_deref(), Some("P0001"), "{error}");
    assert_eq!(server.message(), expected);
}

async fn v2_state(pool: &PgPool) -> Vec<(String, String)> {
    let mut facts = preserved_facts(pool).await;
    // Include full legacy values, the complete stamp and receipts, and all
    // affected constraints/index definitions. A disconnected psql transaction
    // must restore the entire v2 state, including the former UNIQUEs.
    for (name, sql) in [
        (
            "full communities",
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id),'[]'::jsonb)::text FROM social.communities t",
        ),
        (
            "schema stamp",
            "SELECT jsonb_agg(to_jsonb(t) ORDER BY id)::text FROM meta.schema t",
        ),
        (
            "real ledger",
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY step),'[]'::jsonb)::text FROM meta.schema_steps t",
        ),
        (
            "constraints",
            "SELECT coalesce(jsonb_agg(jsonb_build_array(conrelid::regclass::text,conname,contype,convalidated,pg_get_constraintdef(oid)) ORDER BY conrelid::regclass::text,conname),'[]'::jsonb)::text FROM pg_constraint WHERE connamespace='social'::regnamespace AND conrelid IN (SELECT oid FROM pg_class WHERE relnamespace='social'::regnamespace AND relname IN ('communities','private_voice_chats','private_voice_chat_participants','community_voice_notifications'))",
        ),
        (
            "indexes",
            "SELECT coalesce(jsonb_agg(jsonb_build_array(tablename,indexname,indexdef) ORDER BY tablename,indexname),'[]'::jsonb)::text FROM pg_indexes WHERE schemaname='social' AND tablename IN ('communities','private_voice_chats','private_voice_chat_participants','community_voice_notifications')",
        ),
        (
            "legacy columns",
            "SELECT coalesce(jsonb_agg(jsonb_build_array(attname,format_type(atttypid,atttypmod),attnotnull) ORDER BY attnum),'[]'::jsonb)::text FROM pg_attribute WHERE attrelid='social.communities'::regclass AND NOT attisdropped AND attname LIKE 'voice_notification_%'",
        ),
        (
            "destination",
            "SELECT (to_regclass('social.community_voice_notifications') IS NOT NULL)::text",
        ),
    ] {
        facts.push((
            name.into(),
            sqlx::query_scalar(sql).fetch_one(pool).await.unwrap(),
        ));
    }
    facts
}

#[tokio::test]
async fn release_refuses_missing_changed_or_unexpected_role_uniqueness_before_mutation() {
    let named = "schema release v3 requires the named v2 caller/callee UNIQUE constraints with their expected keys; no data changed";
    let unexpected = "schema release v3 found unexpected private voice UNIQUE indexes; inspect retained state before cutover; no data changed";
    for source in [Source::Main, Source::Prepared] {
        for (corruption, expected) in [
            (
                "ALTER TABLE social.private_voice_chats RENAME CONSTRAINT private_voice_chats_caller_key TO renamed_caller_key",
                named,
            ),
            (
                "ALTER TABLE social.private_voice_chats DROP CONSTRAINT private_voice_chats_callee_key",
                named,
            ),
            (
                "ALTER TABLE social.private_voice_chats DROP CONSTRAINT private_voice_chats_caller_key; ALTER TABLE social.private_voice_chats ADD CONSTRAINT private_voice_chats_caller_key UNIQUE(callee_address)",
                named,
            ),
            (
                "ALTER TABLE social.private_voice_chats DROP CONSTRAINT private_voice_chats_caller_key; ALTER TABLE social.private_voice_chats ADD CONSTRAINT private_voice_chats_caller_key CHECK(caller_address <> '')",
                named,
            ),
            (
                "ALTER TABLE social.private_voice_chats DROP CONSTRAINT private_voice_chats_caller_key; CREATE UNIQUE INDEX private_voice_chats_caller_key ON social.private_voice_chats(caller_address)",
                named,
            ),
            (
                "CREATE UNIQUE INDEX unexpected_role_unique ON social.private_voice_chats(caller_address)",
                unexpected,
            ),
            (
                "ALTER TABLE social.private_voice_chats ADD CONSTRAINT unexpected_callee_key UNIQUE(callee_address)",
                unexpected,
            ),
        ] {
            let Some(db) = Database::new(source).await else {
                return;
            };
            seed_voice_facts(&db.pool, source).await;
            sqlx::raw_sql(sqlx::AssertSqlSafe(corruption.to_owned()))
                .execute(&db.pool)
                .await
                .unwrap();
            let before = v2_state(&db.pool).await;
            assert_server_message(release_result(&db.pool, &release_v3()).await, expected);
            assert_eq!(v2_state(&db.pool).await, before);
            db.close().await;
        }
    }
}

#[tokio::test]
async fn release_postconditions_refuse_skipped_or_wrong_replacement_objects() {
    for source in [Source::Main, Source::Prepared] {
        for indexes in [
            String::new(),
            PRIVATE_INDEXES.replace("CREATE INDEX", "CREATE UNIQUE INDEX"),
            PRIVATE_INDEXES.replace("(caller_address)", "(callee_address)"),
            PRIVATE_INDEXES.replace(
                "social.private_voice_chats (caller_address)",
                "social.private_voice_chat_participants (address)",
            ),
        ] {
            let Some(db) = Database::new(source).await else {
                return;
            };
            seed_voice_facts(&db.pool, source).await;
            let before = v2_state(&db.pool).await;
            let release = release_v3().replace(PRIVATE_INDEXES, &indexes);
            assert_server_message(
                release_result(&db.pool, &release).await,
                "schema release v3 requires both valid replacement private voice indexes before stamp; no data changed",
            );
            assert_eq!(v2_state(&db.pool).await, before);
            db.close().await;
        }
    }
    let Some(db) = Database::new(Source::Main).await else {
        return;
    };
    let before = v2_state(&db.pool).await;
    assert_server_message(
        release_result(&db.pool, &release_v3().replace(VOICE_FACTS, "")).await,
        "schema release v3 requires the notification destination table before stamp; no data changed",
    );
    assert_eq!(v2_state(&db.pool).await, before);
    db.close().await;
}

struct ReleaseDirectory(PathBuf);
impl ReleaseDirectory {
    fn new(missing: Option<&str>) -> Self {
        let root = std::env::temp_dir().join(format!(
            "social-voice-release-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("v3")).unwrap();
        fs::write(root.join("release.sql"), RELEASE_V3).unwrap();
        for (name, bytes) in [
            ("41_social_voice_notifications.sql", VOICE_FACTS),
            ("42_social_private_voice_indexes.sql", PRIVATE_INDEXES),
        ] {
            if missing != Some(name) {
                fs::write(root.join("v3").join(name), bytes).unwrap();
            }
        }
        Self(root)
    }
}
impl Drop for ReleaseDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn psql_release(db: &Database, directory: &Path, stdin: bool, ambient_off: bool) -> Output {
    let mut command = Command::new(std::env::var_os("PSQL").unwrap_or_else(|| "psql".into()));
    command
        .args(["-X", "--no-password", "--dbname", &db.url])
        .current_dir(directory);
    // Default cases pass no ON_ERROR_STOP flag at all. Separate cases prove the
    // script's own setting overrides an explicitly disabled client variable.
    if ambient_off {
        command.args(["-v", "ON_ERROR_STOP=off"]);
    }
    if !stdin {
        command.args(["-f", "release.sql"]);
    }
    command
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .expect("psql is required for the release client proof");
    if stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(RELEASE_V3.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

#[tokio::test]
async fn psql_missing_includes_and_stdin_wrong_directory_disconnect_without_changing_v2() {
    for source in [Source::Main, Source::Prepared] {
        for ambient_off in [false, true] {
            for missing in [
                Some("41_social_voice_notifications.sql"),
                Some("42_social_private_voice_indexes.sql"),
                None,
            ] {
                let Some(db) = Database::new(source).await else {
                    return;
                };
                seed_voice_facts(&db.pool, source).await;
                let before = v2_state(&db.pool).await;
                let directory = ReleaseDirectory::new(missing);
                // Stdin has no script filename: \ir resolves against this
                // unrelated working directory, not against the release path.
                let wrong_cwd = directory.0.join("unrelated");
                fs::create_dir(&wrong_cwd).unwrap();
                let stdin = missing.is_none();
                let output = psql_release(
                    &db,
                    if stdin { &wrong_cwd } else { &directory.0 },
                    stdin,
                    ambient_off,
                );
                assert_eq!(
                    output.status.code(),
                    Some(3),
                    "{source:?}, {missing:?}, ambient off={ambient_off}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains(missing.unwrap_or("41_social_voice_notifications.sql"))
                );
                assert_eq!(
                    v2_state(&db.pool).await,
                    before,
                    "after psql client disconnected"
                );
                db.close().await;
            }
        }
    }
}

#[tokio::test]
async fn psql_runs_both_genuine_origins_and_excludes_accepted_calls_from_pending_audit() {
    for source in [Source::Main, Source::Prepared] {
        let Some(db) = Database::new(source).await else {
            return;
        };
        seed_voice_facts(&db.pool, source).await;
        // An unrelated ordinary access path is permitted alongside the PK and
        // reviewed role UNIQUEs; only unexpected uniqueness is refused.
        sqlx::query("CREATE INDEX private_voice_extra_access ON social.private_voice_chats(callee_address,created_at)").execute(&db.pool).await.unwrap();
        let facts = preserved_facts(&db.pool).await;
        let notifications = notification_facts(&db.pool, source).await;
        let ledger: Vec<Receipt> = sqlx::query_as(LEDGER).fetch_all(&db.pool).await.unwrap();
        let directory = ReleaseDirectory::new(None);
        let output = psql_release(&db, &directory.0, false, true);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_release_indexes(&db.pool).await;
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT version FROM meta.schema WHERE id=1")
                .fetch_one(&db.pool)
                .await
                .unwrap(),
            3
        );
        let copied: String = sqlx::query_scalar("SELECT coalesce(jsonb_agg(jsonb_build_array(raw_community_id,community_id,notification_scope,generation,expires_at) ORDER BY community_id),'[]'::jsonb)::text FROM social.community_voice_notifications").fetch_one(&db.pool).await.unwrap();
        assert_eq!(copied, notifications, "live and expired TTLs remain exact");
        assert_eq!(preserved_facts(&db.pool).await, facts);
        assert_eq!(
            sqlx::query_as::<_, Receipt>(LEDGER)
                .fetch_all(&db.pool)
                .await
                .unwrap(),
            ledger
        );
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_index WHERE indrelid='social.private_voice_chats'::regclass AND indisunique AND NOT indisprimary").fetch_one(&db.pool).await.unwrap();
        assert_eq!(remaining, 0);
        db.close().await;
    }
}
