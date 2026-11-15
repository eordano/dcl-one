//! Real PostgreSQL coverage for multi-generation receipt upgrades. Every database
//! here is owned by this test and dropped after use; the configured DSN is admin only.
use catalyst_db::{
    MIGRATIONS_SQL, SCHEMA_SQL,
    schema_migrate::{self, MigrateError, Object},
};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

const RECEIPTS: &str = "social.island_takeover_receipts";
const STEP: &str = "0013_island_receipt_retirement";
const DISPATCH: &str = "11111111-1111-1111-1111-111111111111";

type Ledger = Vec<(String, String, bool)>;

struct OwnedDatabase {
    pool: PgPool,
    admin: PgPool,
    name: String,
}
impl OwnedDatabase {
    async fn new() -> Option<Self> {
        let url = match std::env::var("CATALYST_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) if std::env::var_os("GITHUB_ACTIONS").is_some() => {
                panic!("receipt upgrade tests require CATALYST_TEST_DATABASE_URL in CI")
            }
            Err(_) => return None,
        };
        let options: PgConnectOptions = url.parse().unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone().database("postgres"))
            .await
            .unwrap();
        let name = format!(
            "catalyst_test_lib_{}_receipt_{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        eprintln!("receipt upgrade test database: {name}");
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
        catalyst_db::apply_schema(&pool).await.unwrap();
        Some(Self { pool, admin, name })
    }
    async fn close(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}
async fn ledger(pool: &PgPool) -> Ledger {
    sqlx::query_as("SELECT step,sha256,created_fresh FROM meta.schema_steps ORDER BY step")
        .fetch_all(pool)
        .await
        .unwrap()
}
async fn pending(pool: &PgPool) {
    sqlx::query("DELETE FROM meta.schema_steps WHERE step=$1")
        .bind(STEP)
        .execute(pool)
        .await
        .unwrap();
}
async fn remove_dependencies(pool: &PgPool, retain: usize) {
    sqlx::raw_sql("DROP TABLE social.island_takeover_receipts")
        .execute(pool)
        .await
        .unwrap();
    if retain < 2 {
        sqlx::raw_sql("DROP TABLE social.island_cleanup_dispatches")
            .execute(pool)
            .await
            .unwrap();
    }
    if retain < 1 {
        sqlx::raw_sql("DROP TABLE social.island_admission_fences")
            .execute(pool)
            .await
            .unwrap();
    }
}
async fn seed_fence(pool: &PgPool) {
    sqlx::query("INSERT INTO social.island_admission_fences(wallet,generation,token_floor) VALUES('wallet',7,91)").execute(pool).await.unwrap();
}
async fn seed_dispatch(pool: &PgPool) {
    sqlx::query("INSERT INTO social.island_cleanup_dispatches(dispatch_id,wallet,purpose,pulse_source,pulse_takeover_id,room_name,participant_identity,cutoff,fence_generation,phase,state,settled_at) VALUES($1::text::uuid,'wallet','pulse_takeover','source','takeover','room','participant',91,7,'initial','confirmed',timezone('UTC',now()))").bind(DISPATCH).execute(pool).await.unwrap();
}
async fn seed_receipt(pool: &PgPool) {
    sqlx::query("INSERT INTO social.island_takeover_receipts(wallet,source_incarnation,takeover_id,predecessor_session,predecessor_cluster,predecessor_realm,latest_dispatch,last_successor_session) VALUES('wallet','source','takeover','predecessor','cluster','realm',$1::text::uuid,'successor')").bind(DISPATCH).execute(pool).await.unwrap();
}
async fn facts(pool: &PgPool) -> Vec<String> {
    let mut rows = Vec::new();
    for table in [
        "social.island_admission_fences",
        "social.island_cleanup_dispatches",
        RECEIPTS,
    ] {
        let sql =
            format!("SELECT (to_jsonb(t)-'retired_by_source')::text FROM {table} t ORDER BY 1");
        rows.extend(
            sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql))
                .fetch_all(pool)
                .await
                .unwrap(),
        );
    }
    rows
}
async fn upgrade(pool: &PgPool) -> schema_migrate::Migrated {
    schema_migrate::migrate(pool, SCHEMA_SQL, MIGRATIONS_SQL)
        .await
        .unwrap()
}
async fn assert_recorded(pool: &PgPool, before: Ledger) {
    let current = ledger(pool).await;
    let previous: Ledger = current
        .iter()
        .filter(|(name, _, _)| name != STEP)
        .cloned()
        .collect();
    assert_eq!(previous, before);
    let step = schema_migrate::parse_steps(MIGRATIONS_SQL)
        .into_iter()
        .find(|s| s.name == STEP)
        .unwrap();
    assert_eq!(
        current.into_iter().find(|(name, _, _)| name == STEP),
        Some((STEP.into(), step.sha256, false))
    );
}
async fn assert_no_dependencies(pool: &PgPool) {
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname='social' AND tablename IN ('island_admission_fences','island_cleanup_dispatches','island_takeover_receipts')").fetch_one(pool).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn absent_and_partial_receipt_prerequisites_use_canonical_order_and_preserve_facts() {
    for retain in 0..=2 {
        let Some(db) = OwnedDatabase::new().await else {
            return;
        };
        let pool = &db.pool;
        pending(pool).await;
        remove_dependencies(pool, retain).await;
        if retain > 0 {
            seed_fence(pool).await;
        }
        if retain > 1 {
            seed_dispatch(pool).await;
        }
        let before = ledger(pool).await;
        let done = upgrade(pool).await;
        assert_eq!(done.steps, vec![STEP]);
        let relations: Vec<_> = done
            .created
            .iter()
            .filter_map(|o| {
                if let Object::Relation(name) = o {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect();
        let expected = [
            "social.island_admission_fences",
            "social.island_cleanup_dispatches",
            RECEIPTS,
        ];
        assert_eq!(relations, &expected[retain..]);
        let unique: std::collections::HashSet<_> = done.created.iter().collect();
        assert_eq!(unique.len(), done.created.len(), "creations reported once");
        let indexes:i64=sqlx::query_scalar("SELECT count(*) FROM pg_indexes WHERE schemaname='social' AND tablename='island_takeover_receipts'").fetch_one(pool).await.unwrap();
        assert_eq!(indexes, 2, "primary and final-pass due index");
        if retain > 0 {
            let floor:(i64,i64)=sqlx::query_as("SELECT generation,token_floor FROM social.island_admission_fences WHERE wallet='wallet'").fetch_one(pool).await.unwrap();
            assert_eq!(floor, (7, 91));
        }
        if retain > 1 {
            let id:String=sqlx::query_scalar("SELECT dispatch_id::text FROM social.island_cleanup_dispatches WHERE wallet='wallet'").fetch_one(pool).await.unwrap();
            assert_eq!(id, DISPATCH);
        }
        assert_recorded(pool, before).await;
        catalyst_db::bootstrap(pool).await.unwrap();
        assert!(upgrade(pool).await.is_empty());
        db.close().await;
    }
}

#[tokio::test]
async fn predecessor_receipt_alter_preserves_linked_facts_and_ledger() {
    let Some(db) = OwnedDatabase::new().await else {
        return;
    };
    let pool = &db.pool;
    seed_fence(pool).await;
    seed_dispatch(pool).await;
    seed_receipt(pool).await;
    pending(pool).await;
    sqlx::raw_sql("ALTER TABLE social.island_takeover_receipts DROP CONSTRAINT island_takeover_receipts_retirement_check; ALTER TABLE social.island_takeover_receipts DROP COLUMN retired_by_source").execute(pool).await.unwrap();
    let old_facts = facts(pool).await;
    let before = ledger(pool).await;
    assert!(upgrade(pool).await.created.is_empty());
    assert_eq!(facts(pool).await, old_facts);
    let retirement: Option<String> =
        sqlx::query_scalar("SELECT retired_by_source FROM social.island_takeover_receipts")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(retirement, None);
    for invalid in ["", "source"] {
        let error = sqlx::query("UPDATE social.island_takeover_receipts SET retired_by_source=$1")
            .bind(invalid)
            .execute(pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }
    assert_recorded(pool, before).await;
    let after = ledger(pool).await;
    assert!(upgrade(pool).await.is_empty());
    assert_eq!(ledger(pool).await, after);
    db.close().await;
}

#[tokio::test]
async fn recorded_receipt_step_preserves_fresh_and_migrated_ledgers_and_recovers_missing_object() {
    for fresh in [true, false] {
        let Some(db) = OwnedDatabase::new().await else {
            return;
        };
        let pool = &db.pool;
        if !fresh {
            pending(pool).await;
            upgrade(pool).await;
        }
        seed_fence(pool).await;
        seed_dispatch(pool).await;
        seed_receipt(pool).await;
        let before = ledger(pool).await;
        assert_eq!(
            before.iter().find(|(name, _, _)| name == STEP).unwrap().2,
            fresh
        );
        let old_facts = facts(pool).await;
        assert!(upgrade(pool).await.is_empty());
        assert_eq!(facts(pool).await, old_facts);
        assert_eq!(ledger(pool).await, before);
        // A recorded step must not invoke early prerequisite handling: an absent
        // canonical entry would be a clear error on that path. The final pass
        // may still recreate the missing receipt using its ordinary CREATE.
        sqlx::raw_sql("DROP TABLE social.island_takeover_receipts")
            .execute(pool)
            .await
            .unwrap();
        let schema_without_early_prerequisite = SCHEMA_SQL.replace("CREATE SCHEMA social;", "");
        assert_ne!(schema_without_early_prerequisite, SCHEMA_SQL);
        let done =
            schema_migrate::migrate(pool, &schema_without_early_prerequisite, MIGRATIONS_SQL)
                .await
                .unwrap();
        assert!(done.steps.is_empty());
        assert_eq!(
            done.created,
            vec![
                Object::Relation(RECEIPTS.into()),
                Object::Index("social.island_takeover_receipts_due_idx".into())
            ]
        );
        assert_eq!(ledger(pool).await, before);
        assert!(upgrade(pool).await.is_empty());
        db.close().await;
    }
}

#[tokio::test]
async fn receipt_prerequisites_and_step_records_roll_back_on_drift_or_later_failure() {
    for drift in [true, false] {
        let Some(db) = OwnedDatabase::new().await else {
            return;
        };
        let pool = &db.pool;
        pending(pool).await;
        remove_dependencies(pool, 0).await;
        sqlx::raw_sql("CREATE TABLE meta.receipt_upgrade_probe(id integer); INSERT INTO meta.receipt_upgrade_probe VALUES(1)").execute(pool).await.unwrap();
        let before = ledger(pool).await;
        let schema = if drift {
            format!(
                "{SCHEMA_SQL}\nCREATE TABLE meta.receipt_upgrade_probe(id integer, missing integer);"
            )
        } else {
            SCHEMA_SQL.into()
        };
        let steps = if drift {
            MIGRATIONS_SQL.into()
        } else {
            // Keep the injected SQL failure after every real migration as new
            // steps are added, so validation does not short-circuit the test.
            let last = schema_migrate::parse_steps(MIGRATIONS_SQL).pop().unwrap();
            let next = last.name.split_once('_').unwrap().0.parse::<u16>().unwrap() + 1;
            assert!(next <= 9999, "migration step numbers must fit four digits");
            format!(
                "{MIGRATIONS_SQL}\n-- ════ step {next:04}_receipt_test_failure ════\nUPDATE meta.receipt_upgrade_probe SET id=2;\nUPDATE meta.receipt_upgrade_probe SET missing=3;\n"
            )
        };
        let error = schema_migrate::migrate(pool, &schema, &steps)
            .await
            .unwrap_err();
        if drift {
            assert!(matches!(error, MigrateError::Drift(_)), "{error:?}");
        } else {
            let MigrateError::Sqlx(error) = error else {
                panic!("{error:?}")
            };
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some("42703"),
                "the deliberate later missing-column failure must occur after 0013"
            );
        }
        assert_no_dependencies(pool).await;
        assert_eq!(ledger(pool).await, before);
        let value: i32 = sqlx::query_scalar("SELECT id FROM meta.receipt_upgrade_probe")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(value, 1);
        db.close().await;
    }
}

#[tokio::test]
async fn receipt_checksum_mismatch_refuses_before_bootstrap_and_preserves_records() {
    let Some(db) = OwnedDatabase::new().await else {
        return;
    };
    let pool = &db.pool;
    remove_dependencies(pool, 0).await;
    sqlx::query("UPDATE meta.schema_steps SET sha256=repeat('0',64) WHERE step=$1")
        .bind(STEP)
        .execute(pool)
        .await
        .unwrap();
    let before = ledger(pool).await;
    let error = schema_migrate::migrate(pool, SCHEMA_SQL, MIGRATIONS_SQL)
        .await
        .unwrap_err();
    assert!(matches!(error, MigrateError::StepDrift { .. }));
    assert_no_dependencies(pool).await;
    assert_eq!(ledger(pool).await, before);
    db.close().await;
}
