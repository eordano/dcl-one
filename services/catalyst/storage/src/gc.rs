//! The ONE blob store's collector. Every domain writes into the same
//! hash-addressed store (content files and entities, world scenes and
//! thumbnails, posters, satellite regions, world manifests, community
//! thumbnails and federated post bodies, camera-reel photos, builder item contents, project files and asset packs,
//! player-report evidence, MLS commits and ciphertexts, snapshot files), so
//! a file may go only when no domain names it. The store is walked once;
//! its aged files (untouched for the age floor: at least an hour and the
//! upload + processing window, the widest gap between a writer storing a
//! blob and committing the row that names it) are checked in chunks
//! against every [`REFERENCE_SOURCES`] column (`= ANY`, index-served on the
//! large tables) and the JSON documents that embed keys (the world scenes'
//! and pending partial uploads' content lists, the asset-pack catalogue)
//! plus the global scenes the configuration names, so memory holds a chunk,
//! never the corpus. A scene
//! whose content list cannot be read exactly aborts the run, a candidate
//! is re-checked (references and age) right before it is unlinked, under
//! the exclusive store lock that local content deploys and world scene deploys
//! take shared (synced content writes and other domains rely on the age floor
//! alone), an empty reference set with
//! candidates is refused unless asked for, and the dry run reports without
//! deleting.
//!
//! `catalog_is_complete` (the test below) walks the schema: every column
//! whose name says hash, cid, entity id, thumbnail or image must be listed
//! as a reference or as [`NOT_STORE_KEYS`] with its reason, so a new table
//! cannot hold store keys the collector does not know about.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;

use super::{Storage, StorageError};

pub const MIN_ORPHAN_AGE: Duration = Duration::from_secs(3600);

/// Advisory lock key shared by publishes (shared) and unlink batches (exclusive).
pub const STORE_LOCK_KEY: i64 = 0x5354_4f52_455f_4743;

/// Advisory lock key held for the length of one collector run.
pub const GC_RUN_LOCK_KEY: i64 = 0x5354_4f52_455f_5255;

/// How long an unlink batch waits for in-flight publishes before deferring.
pub const STORE_LOCK_TIMEOUT: &str = "30s";

/// Taken by a local content deploy or a world scene deploy before it checks that the keys it names
/// exist: an unlink batch re-checks references and unlinks under the exclusive lock, so either the
/// deploy sees the file gone or the batch sees the committed reference. Synced content writes and
/// other domains do not take it.
pub async fn lock_store_shared(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(STORE_LOCK_KEY)
        .execute(conn)
        .await?;
    Ok(())
}

/// The exclusive side of [`lock_store_shared`], held by the returned transaction until it ends.
pub async fn lock_store_exclusive(
    pool: &PgPool,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, GcError> {
    let mut tx = pool.begin().await?;
    // The batch unlinks files with the transaction idle: exempt it from the pool's idle timeout.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT set_config('lock_timeout', '{STORE_LOCK_TIMEOUT}', true), \
         set_config('idle_in_transaction_session_timeout', '0', true)"
    )))
    .execute(&mut *tx)
    .await?;
    match sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(STORE_LOCK_KEY)
        .execute(&mut *tx)
        .await
    {
        Ok(_) => Ok(tx),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("55P03") => {
            Err(GcError::LockTimeout)
        }
        Err(e) => Err(e.into()),
    }
}

/// One run at a time across the fleet: `None` while another run holds it.
pub async fn try_lock_run(
    pool: &PgPool,
) -> Result<Option<sqlx::Transaction<'static, sqlx::Postgres>>, GcError> {
    let mut tx = pool.begin().await?;
    // Held idle for the whole walk: exempt it from the pool's idle timeout.
    sqlx::query("SET LOCAL idle_in_transaction_session_timeout = 0")
        .execute(&mut *tx)
        .await?;
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(GC_RUN_LOCK_KEY)
        .fetch_one(&mut *tx)
        .await?;
    Ok(held.then_some(tx))
}

/// Every plain column (`schema.table.column`) holding store keys. A chunk
/// of candidates is checked against each with `column = ANY($1)`.
pub const REFERENCE_SOURCES: &[&str] = &[
    "content.deployments.entity_id",
    "content.content_files.content_hash",
    "content.active_pointers.entity_id",
    "content.local_entities.entity_id",
    "content.snapshots.hash",
    "content.processed_snapshots.hash",
    "content.failed_deployments.snapshot_hash",
    "content.failed_deployments.entity_id",
    "content.pending_deployments.entity_id",
    "content.pending_deployment_files.hash",
    "explore.satellite_region.blob_cid",
    "explore.world_manifest.blob_cid",
    "explore.poster.blob_cid",
    "worlds.worlds.thumbnail_hash",
    "worlds.federation_worlds.thumbnail_hash",
    "worlds.world_scenes.entity_id",
    "worlds.retired_world_scenes.entity_id",
    "worlds.pending_scenes.entity_id",
    "worlds.pending_scene_files.hash",
    "social.community_thumbnails.thumbnail_hash",
    "social.community_contents.cid",
    "social.player_report_evidence.content_hash",
    "social.mls_commits.commit_cid",
    "social.mls_commits.welcome_cid",
    "social.mls_message_refs.ciphertext_cid",
    "social.profile_images.blob_cid",
    "builder.items.thumbnail",
    "builder.items.local_content_hash",
    "builder.items.content_hash",
    "builder.items.catalyst_content_hash",
    "builder.uploaded_contents.hash",
    "builder.item_contents.hash",
    "builder.projects.thumbnail",
    "builder.project_files.hash",
    "builder.asset_packs.thumbnail",
    "builder.assets.thumbnail",
    "builder.asset_contents.hash",
    "camera_reel.images.image_hash",
    "camera_reel.images.thumbnail_hash",
    "shop.outfit_thumbnails.blob_cid",
];

/// Columns whose names look like store keys but are not, with the reason.
pub const NOT_STORE_KEYS: &[(&str, &str)] = &[
    (
        "content.snapshots.replaced_hashes",
        "the snapshot files a newer snapshot replaced: garbage for this collector",
    ),
    (
        "camera_reel.withheld_blobs.hash",
        "the blobs of rejected or deleted photos: garbage for this collector",
    ),
    ("camera_reel.images.image_content_type", "a MIME type"),
    ("explore.place.image_url", "a URL"),
    ("explore.place_world_local.image_url", "a URL"),
    (
        "explore.world_scene_place.entity_id",
        "a non-owning deployment link: worlds.world_scenes holds the blob reference; \
         retiring a place clears the link, while its UUID and storage remain",
    ),
    (
        "explore.place_indexed.image_url",
        "a URL (the view over the place tables)",
    ),
    ("explore.schedules_local.image", "a URL"),
    (
        "content.pending_deployments.content_hashes",
        "a partial upload's manifest: content.pending_deployment_files names every key \
         it stored or found stored",
    ),
    (
        "content.pending_deployment_files.entity_id",
        "the upload a receipt belongs to: content.pending_deployments.entity_id holds the \
         reference to its entity file",
    ),
    (
        "content.forward_outbox.entity_id",
        "a forwarding record: content.deployments holds the reference, and a deployment gone \
         from it is superseded, never sent",
    ),
    (
        "worlds.forward_outbox.entity_id",
        "a forwarding record: worlds.world_scenes holds the reference, and a scene gone from \
         it is superseded, never sent",
    ),
    (
        "worlds.pending_scene_files.entity_id",
        "the upload a file belongs to: worlds.pending_scenes.entity_id holds the reference",
    ),
    (
        "worlds.completed_scene_uploads.entity_id",
        "a completion receipt: worlds.world_scenes holds the reference while the scene is live",
    ),
    ("explore.place_reports.entity_id", "a place id"),
    ("explore.user_favorites.entity_id", "a place id"),
    ("explore.user_likes.entity_id", "a place id"),
    ("explore.place_interaction_counts.entity_id", "a place id"),
    (
        "explore.place_content_ratings.entity_id",
        "a place id or a world name",
    ),
    (
        "explore.signed_actions.signature_hash",
        "an envelope digest",
    ),
    ("worlds.worlds.access_secret_hash", "a bcrypt hash"),
    ("shop.items.image", "a URL"),
    ("shop.trades.hashed_signature", "an order digest"),
    (
        "shop.trade_cancellations.target_hashed_signature",
        "an order digest",
    ),
    ("shop.moderation_flags.target_hash", "an order digest"),
    ("shop.disputes.trade_hash", "an order digest"),
    ("shop.admin_audit.target_hash", "an order digest"),
    ("shop.chain_cursor.last_block_hash", "a block hash"),
    ("shop.trade_event.hashed_signature", "an order digest"),
    ("shop.trade_event.tx_hash", "a transaction hash"),
    ("shop.sale_event.tx_hash", "a transaction hash"),
    ("shop.nft_event.tx_hash", "a transaction hash"),
    ("shop.mana_transfer.transaction_hash", "a transaction hash"),
    ("shop.tip_claim.transaction_hash", "a transaction hash"),
    ("shop.coupons.hashed_signature", "an order digest"),
    ("shop.fed_actions.signature_hash", "an envelope digest"),
    ("shop.fed_actions.target_hash", "an envelope digest"),
    ("shop.fed_actions.tx_hash", "a transaction hash"),
    ("economy.transactions.tx_hash", "a transaction hash"),
    ("shop.broker_purchases.tx_hash", "a transaction hash"),
    (
        "shop.broker_purchases.forward_tx_hash",
        "a transaction hash",
    ),
    (
        "shop.broker_purchases.trade_hashed_signature",
        "an order digest",
    ),
    ("shop.escrow_actions.tx_hash", "a transaction hash"),
    ("shop.name_transfers.tx_hash", "a transaction hash"),
    ("shop.rental_event.tx_hash", "a transaction hash"),
    ("shop.executions.tx_hash", "a transaction hash"),
    (
        "credits.goal_grants.credit_hash",
        "a CreditsManager credit digest",
    ),
    ("credits.season_goals.thumbnail", "an external image URL"),
    (
        "credits.goal_progress.thumbnail",
        "a snapshotted external image URL",
    ),
    (
        "credits.authorizations.credit_hash",
        "a CreditsManager credit digest",
    ),
    (
        "credits.authorizations.submitted_tx_hash",
        "a transaction hash",
    ),
    (
        "credits.revocations.credit_hash",
        "a CreditsManager credit digest",
    ),
    ("credits.revocations.tx_hash", "a transaction hash"),
    (
        "social.community_signed_actions.signature_hash",
        "an envelope digest",
    ),
    (
        "social.community_signed_actions.content_hash",
        "a post body's sha256; its blob is social.community_contents.cid",
    ),
    (
        "social.community_contents.content_hash",
        "a post body's sha256; its blob is the row's cid",
    ),
    (
        "social.community_content_uploads.content_hash",
        "a post body's sha256; its blob is social.community_contents.cid",
    ),
    (
        "social.mls_key_packages.ref_hash",
        "a key package digest; the bytes are in the row",
    ),
    (
        "social.mls_groups.last_commit_hash",
        "the wire sha256 of a commit (its bytes are commit_cid)",
    ),
    (
        "social.mls_commits.commit_hash",
        "the wire sha256 of a commit (its bytes are commit_cid)",
    ),
    (
        "social.mls_message_refs.signature_hash",
        "a message ref digest",
    ),
    (
        "social.mls_message_refs.ciphertext_hash",
        "the wire sha256 of a ciphertext (its bytes are ciphertext_cid)",
    ),
    (
        "builder.collection_publications.tx_hash",
        "a transaction hash",
    ),
    (
        "social.community_mirror.thumbnail_url",
        "a URL (the upstream CDN's thumbnail of a mirrored community)",
    ),
    (
        "social.referral_reward_images.reward_image_url",
        "a URL returned by the rewards API",
    ),
    ("credits.usages.tx_hash", "a transaction hash"),
    (
        "social.profile_images.entity_id",
        "the profile entity an image was drawn for; its blob is the row's blob_cid",
    ),
    (
        "shop.outfits.thumbnail_hash",
        "the sha256 wire id of an outfit thumbnail; its blob is shop.outfit_thumbnails.blob_cid",
    ),
    (
        "shop.outfit_thumbnails.hash",
        "the sha256 wire id of the bytes; the blob is the row's blob_cid",
    ),
    (
        "shop.treasury_pending_refill.swap_tx_hash",
        "a transaction hash",
    ),
    (
        "shop.treasury_pending_refill.transfer_tx_hash",
        "a transaction hash",
    ),
];

/// JSON documents that embed store keys, read by [`reference_set`] beside
/// the plain sources: the world scene and pending upload entities'
/// `content[].hash` and the asset-pack catalogue's thumbnails and asset contents.
pub const JSON_SOURCES: &[&str] = &[
    "worlds.world_scenes.entity",
    "worlds.retired_world_scenes.entity",
    "worlds.pending_scenes.entity",
    "builder.asset_catalog.packs",
];

/// The age floor: at least an hour, and at least the widest store-then-row
/// gap (a multipart upload plus its processing).
pub fn orphan_age_floor(
    multipart_upload_timeout_ms: u64,
    deployment_processing_timeout_ms: u64,
) -> Duration {
    Duration::from_millis(
        multipart_upload_timeout_ms.saturating_add(deployment_processing_timeout_ms),
    )
    .max(MIN_ORPHAN_AGE)
}

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("reading the reference set: {0}")]
    Database(#[from] sqlx::Error),
    #[error("the reference set cannot be read exactly: {0}")]
    Unreadable(String),
    #[error("enumerating the store: {0}")]
    Store(#[from] StorageError),
    #[error("reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error(
        "refusing to collect: no domain names any stored key, which usually means the database is empty or misconfigured; re-run with allow_empty_reference_set to collect anyway"
    )]
    EmptyReferenceSet,
    #[error("publishes kept the store lock busy; retry later")]
    LockTimeout,
}

#[derive(Debug, Clone, Copy)]
pub struct GcOptions {
    pub dry_run: bool,
    pub min_age: Duration,
    pub allow_empty_reference_set: bool,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GcReport {
    pub dry_run: bool,
    pub scanned: usize,
    pub candidates: usize,
    pub removed: usize,
    pub removed_bytes: u64,
    pub failed: usize,
    /// Aged files kept because a domain names them.
    pub references: usize,
    /// Candidates kept because they were stored again during the run.
    pub refreshed: usize,
    pub min_age_seconds: u64,
}

fn content_hashes(
    owner: &str,
    content: Option<&Value>,
    out: &mut HashSet<String>,
) -> Result<(), GcError> {
    let items = match content {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(GcError::Unreadable(format!(
                "{owner} stores a non-array content list"
            )));
        }
    };
    for item in items {
        match item.get("hash").and_then(Value::as_str) {
            Some(hash) if !hash.is_empty() => {
                out.insert(hash.to_string());
            }
            _ => {
                return Err(GcError::Unreadable(format!(
                    "{owner} has a content entry without a usable hash"
                )));
            }
        }
    }
    Ok(())
}

fn catalogue_hashes(packs: &Value, out: &mut HashSet<String>) -> Result<(), GcError> {
    let packs = match packs {
        Value::Array(packs) => packs,
        _ => {
            return Err(GcError::Unreadable(
                "builder.asset_catalog.packs is not an array".into(),
            ));
        }
    };
    for pack in packs {
        if let Some(thumbnail) = pack
            .get("thumbnail")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            out.insert(thumbnail.to_string());
        }
        for asset in pack
            .get("assets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for hash in asset
                .get("contents")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|c| c.values())
            {
                match hash.as_str() {
                    Some(hash) if !hash.is_empty() => {
                        out.insert(hash.to_string());
                    }
                    _ => {
                        return Err(GcError::Unreadable(
                            "an asset-pack content entry has no usable hash".into(),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The entity id of each `urn:decentraland:entity:<id>[?...]` the
/// configuration names (the global scenes every world loads).
pub fn global_scene_entity_ids(urns: &[String]) -> Vec<String> {
    urns.iter()
        .filter_map(|urn| {
            urn.split_once("urn:decentraland:entity:")
                .map(|(_, rest)| rest)
        })
        .map(|rest| rest.split('?').next().unwrap_or(rest).trim().to_string())
        .filter(|cid| !cid.is_empty())
        .collect()
}

/// `(table, column)` of a `schema.table.column` source name.
fn split_source(source: &str) -> (&str, &str) {
    let (table, column) = source.rsplit_once('.').expect("schema.table.column");
    (table, column)
}

/// Refuses a run over world scenes or pending uploads whose content lists
/// cannot be read exactly (a caller deletes what the references do not name).
async fn check_json_sources(pool: &PgPool) -> Result<(), GcError> {
    let bad: Option<String> = sqlx::query_scalar(
        "SELECT entity_id FROM (SELECT entity_id, entity FROM worlds.world_scenes \
           UNION ALL SELECT entity_id, entity FROM worlds.retired_world_scenes \
           UNION ALL SELECT entity_id, entity FROM worlds.pending_scenes) s \
         WHERE jsonb_typeof(s.entity) <> 'object' \
            OR jsonb_typeof(s.entity -> 'content') NOT IN ('array', 'null') \
            OR (jsonb_typeof(s.entity -> 'content') = 'array' AND EXISTS ( \
                SELECT 1 FROM jsonb_array_elements(s.entity -> 'content') e \
                WHERE jsonb_typeof(e -> 'hash') IS DISTINCT FROM 'string' OR e ->> 'hash' = '')) \
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    match bad {
        Some(entity_id) => Err(GcError::Unreadable(format!(
            "scene {entity_id} stores an entity whose content list is not an array of hashed files"
        ))),
        None => Ok(()),
    }
}

/// What every reference check keeps beyond the database rows: the global
/// scenes the configuration names, read from the store.
#[derive(Clone, Copy)]
pub struct Pins<'a> {
    pub storage: &'a Storage,
    pub global_scenes_urn: &'a [String],
}

impl<'a> Pins<'a> {
    pub fn new(storage: &'a Storage, global_scenes_urn: &'a [String]) -> Self {
        Self {
            storage,
            global_scenes_urn,
        }
    }
}

/// The small reference documents read whole: the asset-pack catalogue and
/// the global scenes the configuration names (their entity file and its
/// content list).
pub async fn fixed_references(
    conn: &mut sqlx::PgConnection,
    pins: Pins<'_>,
) -> Result<HashSet<String>, GcError> {
    let mut keys = catalogue_references(conn).await?;
    for entity_id in global_scene_entity_ids(pins.global_scenes_urn) {
        if let Some(bytes) = pins.storage.retrieve(&entity_id).await? {
            let entity: Value = serde_json::from_slice(&bytes).map_err(|e| {
                GcError::Unreadable(format!(
                    "global scene entity {entity_id} does not parse: {e}"
                ))
            })?;
            content_hashes(
                &format!("global scene {entity_id}"),
                entity.get("content"),
                &mut keys,
            )?;
        }
        keys.insert(entity_id);
    }
    Ok(keys)
}

/// The keys the asset-pack catalogue names (its one row).
pub async fn catalogue_references(
    conn: &mut sqlx::PgConnection,
) -> Result<HashSet<String>, GcError> {
    let mut keys = HashSet::new();
    let catalogue: Option<Value> =
        sqlx::query_scalar("SELECT packs FROM builder.asset_catalog WHERE id = 1")
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(packs) = catalogue {
        catalogue_hashes(&packs, &mut keys)?;
    }
    Ok(keys)
}

/// Whether any domain names any key at all (the empty-set refusal).
async fn any_reference(pool: &PgPool) -> Result<bool, GcError> {
    for source in REFERENCE_SOURCES {
        let (table, column) = split_source(source);
        let present: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {table} WHERE {column} IS NOT NULL AND {column} <> '')"
        )))
        .fetch_one(pool)
        .await?;
        if present {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The keys among `keys` some domain names: the fixed references, one
/// `= ANY` probe per source (index-served on the large tables) and the world
/// scenes' and pending uploads' content lists, retired ones included until
/// evicted. Every path that unlinks by reference runs this check.
pub async fn referenced_among_on(
    conn: &mut sqlx::PgConnection,
    pins: Pins<'_>,
    keys: &[String],
) -> Result<HashSet<String>, GcError> {
    references_among(conn, pins, keys, true).await
}

/// [`referenced_among_on`] without the fixed references, which a caller resolved beforehand.
pub async fn referenced_rows_among_on(
    conn: &mut sqlx::PgConnection,
    keys: &[String],
) -> Result<HashSet<String>, GcError> {
    rows_among(conn, keys, true, ALL_ROWS).await
}

/// The oldest transaction a scan started now might not see: rows it misses were written at or after it.
async fn scene_fence(pool: &PgPool) -> Result<i64, GcError> {
    Ok(
        sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text::bigint")
            .fetch_one(pool)
            .await?,
    )
}

/// [`referenced_among_on`], reading only scene and pending-upload rows written at or after `fence`.
async fn referenced_since(
    conn: &mut sqlx::PgConnection,
    pins: Pins<'_>,
    keys: &[String],
    fence: i64,
) -> Result<HashSet<String>, GcError> {
    if keys.is_empty() {
        return Ok(HashSet::new());
    }
    // Assigning this transaction an id pins `age()` to it.
    let current: i64 = sqlx::query_scalar("SELECT pg_current_xact_id()::text::bigint")
        .fetch_one(&mut *conn)
        .await?;
    let fixed = fixed_references(conn, pins).await?;
    let mut found = rows_among(conn, keys, true, (current - fence).max(0)).await?;
    found.extend(keys.iter().filter(|k| fixed.contains(*k)).cloned());
    Ok(found)
}

/// [`referenced_among_on`] on a pooled connection.
pub async fn referenced_among(
    pool: &PgPool,
    pins: Pins<'_>,
    keys: &[String],
) -> Result<HashSet<String>, GcError> {
    let mut conn = pool.acquire().await?;
    referenced_among_on(&mut conn, pins, keys).await
}

/// The keys among `keys` published content names: [`referenced_among`] without the pending
/// partial uploads, which charge for content only they keep.
pub async fn published_among(
    pool: &PgPool,
    pins: Pins<'_>,
    keys: &[String],
) -> Result<HashSet<String>, GcError> {
    let mut conn = pool.acquire().await?;
    references_among(&mut conn, pins, keys, false).await
}

async fn references_among(
    conn: &mut sqlx::PgConnection,
    pins: Pins<'_>,
    keys: &[String],
    pending: bool,
) -> Result<HashSet<String>, GcError> {
    if keys.is_empty() {
        return Ok(HashSet::new());
    }
    let fixed = fixed_references(conn, pins).await?;
    let mut found = rows_among(conn, keys, pending, ALL_ROWS).await?;
    found.extend(keys.iter().filter(|k| fixed.contains(*k)).cloned());
    Ok(found)
}

/// The database rows' part of [`references_among`], counting only scene and
/// pending-upload rows at most `max_age` transactions old.
async fn rows_among(
    conn: &mut sqlx::PgConnection,
    keys: &[String],
    pending: bool,
    max_age: i64,
) -> Result<HashSet<String>, GcError> {
    let mut found = HashSet::new();
    if keys.is_empty() {
        return Ok(found);
    }
    for source in REFERENCE_SOURCES {
        if !pending && PENDING_PREFIXES.iter().any(|p| source.starts_with(p)) {
            continue;
        }
        let (table, column) = split_source(source);
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT {column} FROM {table} WHERE {column} = ANY($1)"
        )))
        .bind(keys)
        .fetch_all(&mut *conn)
        .await?;
        found.extend(rows);
    }
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT e ->> 'hash' FROM (SELECT entity FROM worlds.world_scenes WHERE age(xmin) <= $3 \
           UNION ALL SELECT entity FROM worlds.retired_world_scenes WHERE age(xmin) <= $3 \
           UNION ALL SELECT entity FROM worlds.pending_scenes WHERE $2 AND age(xmin) <= $3) s \
         CROSS JOIN LATERAL jsonb_array_elements(CASE WHEN jsonb_typeof(s.entity -> 'content') = 'array' THEN s.entity -> 'content' ELSE '[]'::jsonb END) e \
         WHERE e ->> 'hash' = ANY($1)",
    )
    .bind(keys)
    .bind(pending)
    .bind(max_age)
    .fetch_all(&mut *conn)
    .await?;
    found.extend(rows);
    Ok(found)
}

/// The partial-upload staging sources, content's and worlds' ([`published_among`] leaves them out).
const PENDING_PREFIXES: [&str; 2] = ["content.pending_", "worlds.pending_"];

/// A `max_age` that counts every row.
const ALL_ROWS: i64 = i32::MAX as i64;

const CHUNK: usize = 2_000;

/// Whether the stored file `id` was last written at least `min_age` before
/// `now`; `None` when it is not stored.
async fn aged(
    storage: &Storage,
    id: &str,
    now: SystemTime,
    min_age: Duration,
) -> Result<Option<bool>, GcError> {
    let Some(path) = storage.file_path(id).await? else {
        return Ok(None);
    };
    let modified = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta.modified().ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(GcError::Io(path.display().to_string(), e)),
    };
    Ok(Some(
        modified
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= min_age),
    ))
}

/// Unlinks those of `keys` a collection pass with `min_age` would: aged
/// files outside the fixed references (the asset-pack catalogue, the global
/// scenes) that no domain names, re-checked right before the unlink. For a
/// writer reclaiming the blobs it staged; younger ones are left to the
/// collector, which covers the same store-then-row gap. Answers how many
/// went.
pub async fn unlink_orphans(
    pool: &PgPool,
    storage: &Storage,
    global_scenes_urn: &[String],
    keys: &[String],
    min_age: Duration,
) -> Result<usize, GcError> {
    if keys.is_empty() {
        return Ok(0);
    }
    check_json_sources(pool).await?;
    let fixed = fixed_references(
        &mut *pool.acquire().await?,
        Pins::new(storage, global_scenes_urn),
    )
    .await?;
    let now = SystemTime::now();
    let mut candidates: Vec<String> = Vec::new();
    for key in keys {
        if !fixed.contains(key) && aged(storage, key, now, min_age).await? == Some(true) {
            candidates.push(key.clone());
        }
    }
    let mut unlinked = 0;
    for batch in candidates.chunks(CHUNK) {
        // The fixed references were filtered out above.
        let referenced = rows_among(&mut *pool.acquire().await?, batch, true, ALL_ROWS).await?;
        for key in batch.iter().filter(|k| !referenced.contains(*k)) {
            storage.delete_strict(key).await?;
            unlinked += 1;
        }
    }
    Ok(unlinked)
}

/// One collection pass: the store is walked in chunks of aged files, each
/// checked against every domain's references; a candidate is re-checked
/// right before it is unlinked (a deployment may reuse a stored blob).
pub async fn collect(
    pool: &PgPool,
    storage: &Storage,
    global_scenes_urn: &[String],
    options: GcOptions,
) -> Result<GcReport, GcError> {
    check_json_sources(pool).await?;
    let pins = Pins::new(storage, global_scenes_urn);
    let fixed = fixed_references(&mut *pool.acquire().await?, pins).await?;
    let refuse_empty =
        !options.allow_empty_reference_set && fixed.is_empty() && !any_reference(pool).await?;
    let now = SystemTime::now();
    let mut report = GcReport {
        dry_run: options.dry_run,
        min_age_seconds: options.min_age.as_secs(),
        ..GcReport::default()
    };
    let mut orphans: Vec<String> = Vec::new();
    let mut chunk: Vec<String> = Vec::with_capacity(CHUNK);
    let mut ids = storage.all_file_ids(None);
    loop {
        let next = ids.next().await?;
        if let Some(id) = &next {
            report.scanned += 1;
            if let Some(aged) = aged(storage, id, now, options.min_age).await? {
                if aged && !fixed.contains(id) {
                    chunk.push(id.clone());
                } else if fixed.contains(id) {
                    report.references += 1;
                }
            }
        }
        if chunk.len() >= CHUNK || (next.is_none() && !chunk.is_empty()) {
            chunk.sort();
            chunk.dedup();
            // The fixed references never enter a chunk.
            let referenced =
                rows_among(&mut *pool.acquire().await?, &chunk, true, ALL_ROWS).await?;
            report.references += referenced.len();
            orphans.extend(chunk.drain(..).filter(|id| !referenced.contains(id)));
        }
        if next.is_none() {
            break;
        }
    }
    report.candidates = orphans.len();
    if refuse_empty && !orphans.is_empty() {
        return Err(GcError::EmptyReferenceSet);
    }
    if !options.dry_run {
        unlink_collected(pool, storage, pins, &orphans, options.min_age, &mut report).await?;
    }
    tracing::info!(?report, "blob store collection finished");
    Ok(report)
}

/// Unlinks each candidate still unreferenced and still aged, under the exclusive store lock.
async fn unlink_collected(
    pool: &PgPool,
    storage: &Storage,
    pins: Pins<'_>,
    orphans: &[String],
    min_age: Duration,
    report: &mut GcReport,
) -> Result<(), GcError> {
    for batch in orphans.chunks(CHUNK) {
        // The full scan runs unlocked; the lock re-checks only what was written since.
        let fence = scene_fence(pool).await?;
        let unlocked = referenced_among(pool, pins, batch).await?;
        let survivors: Vec<String> = batch
            .iter()
            .filter(|id| !unlocked.contains(*id))
            .cloned()
            .collect();
        if survivors.is_empty() {
            continue;
        }
        let mut lock = lock_store_exclusive(pool).await?;
        let referenced_now = referenced_since(&mut lock, pins, &survivors, fence).await?;
        for id in survivors.iter().filter(|id| !referenced_now.contains(*id)) {
            // A blob stored again since the walk is young again: keep it.
            let meta = match super::resolve_file_path(storage.root(), id) {
                Ok(path) => tokio::fs::metadata(path).await.ok(),
                Err(_) => None,
            };
            let Some(meta) = meta else { continue };
            if !meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .is_some_and(|age| age >= min_age)
            {
                report.refreshed += 1;
                continue;
            }
            let size = meta.len();
            match storage.delete_strict(id).await {
                Ok(()) => {
                    report.removed += 1;
                    report.removed_bytes += size;
                }
                Err(e) => {
                    report.failed += 1;
                    tracing::warn!(key = %id, error = %e, "blob store collector: could not remove an unreferenced file");
                }
            }
        }
        lock.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use serde_json::json;

    #[test]
    fn the_floor_is_at_least_an_hour_and_the_upload_window() {
        assert_eq!(orphan_age_floor(1_000, 1_000), MIN_ORPHAN_AGE);
        assert_eq!(
            orphan_age_floor(3_000_000, 1_000_000),
            Duration::from_millis(4_000_000)
        );
    }

    #[test]
    fn a_content_list_without_a_usable_hash_aborts_the_set() {
        let mut out = HashSet::new();
        content_hashes("s", Some(&json!([{ "file": "a", "hash": "h1" }])), &mut out).unwrap();
        assert!(out.contains("h1"));
        assert!(content_hashes("s", Some(&json!([{ "file": "a" }])), &mut out).is_err());
        assert!(content_hashes("s", Some(&json!({ "a": "h" })), &mut out).is_err());
        let mut packs = HashSet::new();
        catalogue_hashes(
            &json!([{ "thumbnail": "t1", "assets": [{ "contents": { "a.glb": "g1" } }] }]),
            &mut packs,
        )
        .unwrap();
        assert_eq!(packs, HashSet::from(["t1".to_string(), "g1".to_string()]));
        assert!(catalogue_hashes(&json!({}), &mut packs).is_err());
        assert_eq!(
            global_scene_entity_ids(&[
                "urn:decentraland:entity:bafyx?baseUrl=https://x/".into(),
                "nonsense".into()
            ]),
            vec!["bafyx"]
        );
    }

    /// Every hash/cid/entity/thumbnail/image column of the schema is either a
    /// reference source or a declared non-key, and every declared source
    /// statement runs against the real schema.
    #[tokio::test]
    async fn catalog_is_complete() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let columns: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT table_schema, table_name, column_name FROM information_schema.columns \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
               AND data_type IN ('text', 'character varying', 'ARRAY', 'jsonb', 'json') \
               AND column_name ~ '(hash|cid|entity_id|thumbnail|image)'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let known: HashSet<&str> = REFERENCE_SOURCES
            .iter()
            .copied()
            .chain(NOT_STORE_KEYS.iter().map(|(c, _)| *c))
            .chain(JSON_SOURCES.iter().copied())
            .collect();
        let unclassified: Vec<String> = columns
            .iter()
            .map(|(s, t, c)| format!("{s}.{t}.{c}"))
            .filter(|name| !known.contains(name.as_str()))
            .collect();
        assert!(
            unclassified.is_empty(),
            "classify these columns in storage::gc (REFERENCE_SOURCES or NOT_STORE_KEYS): {unclassified:?}"
        );
        let dir =
            std::env::temp_dir().join(format!("catalyst-gc-{}", uuid::Uuid::new_v4().simple()));
        let storage = Storage::from_config(&dir).await.unwrap();
        referenced_among(&pool, Pins::new(&storage, &[]), &["none".to_string()])
            .await
            .expect("every source statement runs against the schema");
    }

    /// An unlink batch waits for a publish holding the shared lock, and only one run holds the run lock.
    #[tokio::test]
    async fn unlink_batches_wait_for_publishes_and_runs_are_exclusive() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let mut publish = pool.begin().await.unwrap();
        lock_store_shared(&mut publish).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), lock_store_exclusive(&pool))
                .await
                .is_err(),
            "the batch waits while a publish holds the shared lock"
        );
        publish.commit().await.unwrap();
        let batch = tokio::time::timeout(Duration::from_secs(5), lock_store_exclusive(&pool))
            .await
            .expect("the batch proceeds once the publish ends")
            .unwrap();
        batch.commit().await.unwrap();

        let run = try_lock_run(&pool).await.unwrap().expect("first run");
        assert!(try_lock_run(&pool).await.unwrap().is_none());
        run.commit().await.unwrap();
        assert!(try_lock_run(&pool).await.unwrap().is_some());
    }

    /// Under the lock only scenes and pending uploads written since the fence are read; the full check sees all.
    #[tokio::test]
    async fn the_locked_recheck_reads_only_scenes_written_since_the_fence() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let world = format!("gc{}.dcl.eth", uuid::Uuid::new_v4().simple());
        let (old, new) = (format!("old-{world}"), format!("new-{world}"));
        let (old_pending, new_pending) = (format!("oldp-{world}"), format!("newp-{world}"));
        sqlx::query("INSERT INTO worlds.worlds (name) VALUES ($1)")
            .bind(&world)
            .execute(&pool)
            .await
            .unwrap();
        let insert_scene = |entity_id: &'static str, hash: String| {
            let (pool, world) = (pool.clone(), world.clone());
            async move {
                sqlx::query_scalar::<_, i64>(
                    "INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size) \
                     VALUES ($1, $2, '[]'::json, $3, '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', '{\"0,0\"}', 1) \
                     RETURNING xmin::text::bigint",
                )
                .bind(&world)
                .bind(entity_id)
                .bind(json!({ "content": [{ "file": "a", "hash": hash }] }))
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let insert_pending = |entity_id: String, hash: String| {
            let (pool, world) = (pool.clone(), world.clone());
            async move {
                sqlx::query(
                    "INSERT INTO worlds.pending_scenes (entity_id, world_name, parcels, entity, deployer, created_at) \
                     VALUES ($1, $2, '{\"0,0\"}', $3, '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', timezone('UTC', now()))",
                )
                .bind(entity_id)
                .bind(&world)
                .bind(json!({ "content": [{ "file": "a", "hash": hash }] }))
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        insert_pending(format!("bafyoldp{world}"), old_pending.clone()).await;
        let written = insert_scene("bafyold", old.clone()).await;
        assert!(scene_fence(&pool).await.unwrap() > 0);
        let fence = written + 1;
        insert_scene("bafynew", new.clone()).await;
        insert_pending(format!("bafynewp{world}"), new_pending.clone()).await;
        let keys = vec![
            old.clone(),
            new.clone(),
            old_pending.clone(),
            new_pending.clone(),
        ];
        let dir =
            std::env::temp_dir().join(format!("catalyst-gc-{}", uuid::Uuid::new_v4().simple()));
        let storage = Storage::from_config(&dir).await.unwrap();
        let pins = Pins::new(&storage, &[]);

        let mut lock = lock_store_exclusive(&pool).await.unwrap();
        assert_eq!(
            referenced_since(&mut lock, pins, &keys, fence)
                .await
                .unwrap(),
            HashSet::from([new.clone(), new_pending.clone()])
        );
        assert_eq!(
            referenced_among_on(&mut lock, pins, &keys).await.unwrap(),
            HashSet::from([old, new, old_pending, new_pending])
        );
        lock.rollback().await.unwrap();
        for table in ["worlds.world_scenes", "worlds.pending_scenes"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE world_name = $1"
            )))
            .bind(&world)
            .execute(&pool)
            .await
            .unwrap();
        }
    }

    /// A candidate stored again after the walk is young again under the lock and stays.
    #[tokio::test]
    async fn a_candidate_stored_again_since_the_walk_stays() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let dir =
            std::env::temp_dir().join(format!("catalyst-gc-{}", uuid::Uuid::new_v4().simple()));
        let storage = Storage::from_config(&dir).await.unwrap();
        let mut keys = Vec::new();
        for bytes in [&b"aged orphan"[..], b"stored again"] {
            let key = crate::auth::hash::hash_bytes_v1(bytes);
            storage
                .store(&key, Bytes::from(bytes.to_vec()))
                .await
                .unwrap();
            keys.push(key);
        }
        let aged = storage.file_path(&keys[0]).await.unwrap().unwrap();
        std::fs::File::open(aged)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(7200)),
            )
            .unwrap();
        let mut report = GcReport::default();
        let pins = Pins::new(&storage, &[]);
        unlink_collected(&pool, &storage, pins, &keys, MIN_ORPHAN_AGE, &mut report)
            .await
            .unwrap();
        assert_eq!((report.removed, report.refreshed), (1, 1));
        assert!(!storage.exist(&keys[0]).await.unwrap());
        assert!(storage.exist(&keys[1]).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On a database of its own: an aged unreferenced file goes, a
    /// referenced one (a content file, a poster, a scene's content, a global
    /// scene) and a fresh one stay; the dry run deletes nothing; an empty
    /// reference set is refused unless allowed.
    #[tokio::test]
    async fn only_aged_unreferenced_files_are_collected() {
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
        let Ok(base_url) = std::env::var("CATALYST_TEST_DATABASE_URL") else {
            return;
        };
        let opts: PgConnectOptions = base_url.parse().unwrap();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(opts.database("postgres"))
            .await
            .unwrap();
        let name = format!(
            "catalyst_test_gc_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&base_url).unwrap();
        url.set_path(&format!("/{name}"));
        let pool = crate::db::connect_pool(
            url.as_str(),
            &crate::db::PoolSettings {
                max_connections: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        crate::db::apply_schema(&pool).await.unwrap();
        let dir =
            std::env::temp_dir().join(format!("catalyst-gc-{}", uuid::Uuid::new_v4().simple()));
        let storage = Storage::from_config(&dir).await.unwrap();
        let put = |bytes: Vec<u8>| {
            let storage = &storage;
            async move {
                let key = crate::auth::hash::hash_bytes_v1(&bytes);
                storage.store(&key, Bytes::from(bytes)).await.unwrap();
                key
            }
        };
        let orphan = put(b"orphan".to_vec()).await;
        let poster = put(b"poster".to_vec()).await;
        let scene_file = put(b"scene file".to_vec()).await;
        let retired_file = put(b"retired scene file".to_vec()).await;
        let global_file = put(b"global file".to_vec()).await;
        let fresh = put(b"fresh".to_vec()).await;
        let global = put(json!({ "content": [{ "file": "a", "hash": global_file }] })
            .to_string()
            .into_bytes())
        .await;
        let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
        for key in [
            &orphan,
            &poster,
            &scene_file,
            &retired_file,
            &global,
            &global_file,
        ] {
            let path = storage.file_path(key).await.unwrap().unwrap();
            std::fs::File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
                .unwrap();
        }
        let options = GcOptions {
            dry_run: true,
            min_age: MIN_ORPHAN_AGE,
            allow_empty_reference_set: false,
        };
        let globals = vec![format!(
            "urn:decentraland:entity:{global}?baseUrl=https://x/contents/"
        )];
        assert!(matches!(
            collect(&pool, &storage, &[], options).await,
            Err(GcError::EmptyReferenceSet)
        ));

        sqlx::query("INSERT INTO explore.poster (kind, file, blob_cid, content_type, byte_size, origin, fetched_at) VALUES ('poster', 'p.png', $1, 'image/png', 1, 'local', timezone('UTC', now()))")
            .bind(&poster)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO worlds.worlds (name) VALUES ('gc.dcl.eth')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size) \
             VALUES ('gc.dcl.eth', 'bafkreiscene', '[]'::json, $1, $2, '{\"0,0\"}', 1)",
        )
        .bind(json!({ "content": [{ "file": "a.glb", "hash": scene_file }] }))
        .bind(crate::shared::identity::ZERO_ADDRESS)
        .execute(&pool)
        .await
        .unwrap();
        // A replaced scene's content stays referenced until it is evicted.
        sqlx::query(
            "INSERT INTO worlds.retired_world_scenes (world_name, entity_id, entity, parcels) \
             VALUES ('gc.dcl.eth', 'bafkreiretired', $1, '{\"0,0\"}')",
        )
        .bind(json!({ "content": [{ "file": "a.glb", "hash": retired_file }] }))
        .execute(&pool)
        .await
        .unwrap();

        let dry = collect(&pool, &storage, &globals, options).await.unwrap();
        assert_eq!((dry.scanned, dry.candidates, dry.removed), (7, 1, 0));
        assert!(
            storage.exist(&orphan).await.unwrap(),
            "the dry run deletes nothing"
        );
        let wet = collect(
            &pool,
            &storage,
            &globals,
            GcOptions {
                dry_run: false,
                ..options
            },
        )
        .await
        .unwrap();
        assert_eq!((wet.candidates, wet.removed, wet.failed), (1, 1, 0));
        assert!(!storage.exist(&orphan).await.unwrap());
        for key in [
            &poster,
            &scene_file,
            &retired_file,
            &global,
            &global_file,
            &fresh,
        ] {
            assert!(storage.exist(key).await.unwrap(), "{key} stays");
        }
        // The key-level check other unlink paths run counts the fixed references too.
        let pack_file = put(b"asset pack file".to_vec()).await;
        sqlx::query("INSERT INTO builder.asset_catalog (id, packs, source, built_at) VALUES (1, $1, 'test', timezone('UTC', now()))")
            .bind(json!([{ "assets": [{ "contents": { "a.glb": pack_file } }] }]))
            .execute(&pool)
            .await
            .unwrap();
        let keys = vec![global.clone(), global_file.clone(), pack_file.clone()];
        let pins = Pins::new(&storage, &globals);
        assert_eq!(referenced_among(&pool, pins, &keys).await.unwrap().len(), 3);
        assert_eq!(published_among(&pool, pins, &keys).await.unwrap().len(), 3);
        pool.close().await;
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
