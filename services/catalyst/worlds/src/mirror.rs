//! The worlds mirror: one pass is the catalogue (`GET {source}/index`, one
//! document listing every world and its scene ids) written to
//! `worlds.remote_worlds`, then the scenes/files walk over the catalogue in
//! name order: `GET {source}/world/{name}/about` (the world's spawn and content
//! `baseUrl`), its paged `GET {source}/world/{name}/scenes` (every deployed scene with its
//! deployment's times, size, chain and deployer: `/about` lists only the newest) and
//! `GET {source}/world/{name}/settings` (the stored world settings and their version) for each
//! world whose advertised scene set differs from what is held, the scene entities and
//! every file they reference into the ONE blob store (hash-verified, so a
//! lying upstream cannot plant bytes under a CID), and the `worlds.worlds` +
//! `worlds.world_scenes` rows in one transaction per world. Every world the walk
//! reaches, changed or not, also gets the access setting its
//! `GET {source}/world/{name}/permissions` reports (a shared secret stored as one no secret
//! matches), and the settings `/settings` reports. Either is kept while the owner that set it on
//! this node (`access_set_here`, `settings_set_here`) still holds the name, as this node's own
//! owner lookup says ([`name_holder`], [`mirror_keeps_local`]); the source's `/permissions` owner
//! trails a sale, so it never decides. An unreadable or unknown access setting
//! keeps the stored one: it fails the sync of a changed world, so a world restricted upstream is
//! never stored open, and a world whose scenes are all held skips only the access copy. The
//! deployment and streaming grants it lists are not copied. A world whose scenes are all held
//! is still read for its settings and its `/scenes` listing, since upstream edits settings and
//! access without a deployment and its held rows take the listed times, size, chain and
//! deployer. Each such world costs a pass `/permissions`, every `/scenes` page, `/settings` and
//! a thumbnail check against the store (a fetch only when it is not held), then one transaction
//! that writes only what changed.
//!
//! Resumable: the scenes walk persists its cursor (the last world name
//! completed) after every batch in `worlds.mirror_passes`, so a restart
//! continues there; a pass is complete when the cursor is cleared and
//! `pass_completed_at` stamped with the pass counts. Local authority: a world
//! holding a scene with `mirror_source IS NULL` (a local publish) is refused,
//! never overwritten (the write re-checks under the world row's lock, so a
//! publish landing mid-download wins too), and only scenes the mirror wrote
//! are ever retired: those of a refused world at once, since the mirror never
//! refreshes them again.
//!
//! A read-through fill ([`mirror_one`]) takes the same path with [`Verify`]: it takes each
//! scene's deployment auth chain from its row of that listing (the chain at most
//! [`MAX_CHAIN_BYTES`]) and stores only scenes signed (ECDSA links only, as the upstream worlds
//! server accepts) by an address the policy says may have published the world, or
//! (`WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS`, on the upstream's word) one its `/permissions`
//! lists as a deployer of a world whose owner it names as this node's NAME authority does; its
//! rows carry that chain and signer. The newest scene must pass, or nothing is stored. An older
//! scene this node definitely refuses (a malformed, empty or over-size chain, or a signer without
//! that right) is left out; an answer that could not be read, or a signature that does not match
//! its entity, fails the whole fill. Of the
//! files it stores only the scene host's (the main code file and `main.crdt`): the rest are read
//! through, hash-checked, from the same source when first asked for. It also leaves alone a world
//! this node keeps itself: a `worlds.worlds` row with no `mirror_source` (a local world, scenes or
//! not) or a `blocked_since`. The scheduled pass keeps the
//! catalogue's trust: a row carries the listed chain and deployer only when that chain's ECDSA
//! links sign the scene and its signer is the listed deployer, else no chain and the zero
//! address; no right here is granted from either (nor from the access setting, which only
//! narrows who may enter).
//! Deletion: a complete catalogue pass that no longer lists a world stamps
//! `unlisted_at`; the next scenes pass retires its mirrored scenes. A pass
//! that would unlist more than max(50, 20%) of the catalogue is refused, so
//! a truncated-but-well-formed index cannot empty the mirror.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Row};
use tokio::sync::Semaphore;

use super::ZERO_ADDRESS;
use crate::auth::{
    AuthChain, hash_bytes_v1, is_canonical_cid, raw_cid_v1_from_sha256_hex, verify_auth_chain,
    verify_hash,
};
use crate::comms::world_identity::WorldName;
use crate::content::read_through::{DownloadSlot, Downloads};
use crate::core::settings_policy::{
    MAX_CATEGORIES, VALID_RATINGS, detect_image_format, storable_skybox_time,
    storable_thumbnail_hash,
};
use crate::core::store::{AccessSetting, SetHere, mirror_keeps_local, replace_mirrored_access};
use crate::explore::places::scene_identity;
use crate::http::{is_safe_http_url, read_body_capped};
use crate::jobs::JobContext;
use crate::policy::Policy;
use crate::shared::identity::{Address, AuthorityNotEstablished, PermissionDecision};
use catalyst_storage::Storage;

#[path = "mirror_catalogue.rs"]
mod catalogue;
#[path = "mirror_config.rs"]
mod config;
#[path = "mirror_reconcile.rs"]
mod reconcile;
#[path = "mirror_upstream.rs"]
mod upstream;

use catalogue::*;
pub use config::{DEFAULT_BLOB_CONCURRENCY, DEFAULT_CONCURRENCY, DEFAULT_INTERVAL, MirrorSettings};
use reconcile::*;
use upstream::*;

/// The upstream index measured 4.3 MB for 1,667 worlds (2026-09-24); the
/// cap leaves an order of magnitude of growth before a pass is refused.
const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ABOUT_BYTES: usize = 1024 * 1024;
const MAX_BLOB_BYTES: usize = 256 * 1024 * 1024;
/// One `/scenes?entity_id=` answer: a single scene's row, its entity included.
const MAX_SCENE_ROW_BYTES: usize = 4 * 1024 * 1024;
/// A deployment auth chain as JSON; a real one is a few links of a few hundred bytes.
const MAX_CHAIN_BYTES: usize = 16 * 1024;
/// Scenes per page of a world's `/scenes` listing, and the cap of one page's body.
const SCENES_PAGE: usize = 20;
const MAX_SCENES_PAGE_BYTES: usize = 64 * 1024 * 1024;
/// A world listing more scenes than this is refused on every pass: none of it is retired, and
/// none of its updates land.
const MAX_LISTED_SCENES: usize = 2_000;
/// One `/world/{name}/permissions` answer: the owner and a few allow-lists of addresses.
const MAX_PERMISSIONS_BYTES: usize = 256 * 1024;
/// The stored secret hash of a mirrored shared-secret world: not a bcrypt hash, so no secret
/// verifies against it (the source never publishes its hash).
const NO_SECRET_MATCHES: &str = "mirrored: the source keeps the secret";
/// One `/world/{name}/settings` answer: a handful of short columns.
const MAX_SETTINGS_BYTES: usize = 256 * 1024;
/// A world thumbnail upstream accepts at most 1 MiB (`world-settings-handler.ts`).
const MAX_THUMBNAIL_BYTES: usize = 1024 * 1024;
const SWEEP_FUSE_MIN_CANDIDATES: i64 = 50;

#[derive(Debug, Default, Clone, Serialize)]
pub struct CatalogueOutcome {
    pub listed: usize,
    pub unlisted: usize,
    pub complete: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScenesOutcome {
    pub processed: usize,
    pub synced: usize,
    pub unchanged: usize,
    pub refused: usize,
    pub skipped: usize,
    pub failed: usize,
    pub retired_scenes: u64,
    pub scenes: usize,
    pub new_blobs: usize,
    pub resumed_from: Option<String>,
    pub interrupted: bool,
    pub complete: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct PassSummary {
    pub catalogue: CatalogueOutcome,
    pub scenes: ScenesOutcome,
}

/// One full pass: catalogue, then the scenes walk (resumed if a cursor is
/// persisted). A catalogue failure is the pass's error; the scenes walk
/// records per-world failures and completes regardless.
pub async fn run_pass(
    client: &crate::http::Outbound,
    pool: &PgPool,
    storage: &Arc<Storage>,
    settings: &MirrorSettings,
    owners: Option<Arc<Policy>>,
    ctx: &JobContext,
) -> Result<PassSummary> {
    let catalogue = match catalogue_pass(client, pool, settings, ctx).await {
        Ok(out) => out,
        Err(e) => {
            record_error(pool, "catalogue", &settings.source, &format!("{e:#}")).await?;
            return Err(e);
        }
    };
    if ctx.is_cancelled() {
        return Ok(PassSummary {
            catalogue,
            scenes: ScenesOutcome {
                interrupted: true,
                ..Default::default()
            },
        });
    }
    let scenes = match scenes_pass(client, pool, storage, settings, owners, ctx).await {
        Ok(out) => out,
        Err(e) => {
            record_error(pool, "scenes", &settings.source, &format!("{e:#}")).await?;
            return Err(e);
        }
    };
    Ok(PassSummary { catalogue, scenes })
}

async fn record_error(pool: &PgPool, mirror: &str, source: &str, error: &str) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO worlds.mirror_passes (mirror, source, last_attempt_at, last_error)
           VALUES ($1, $2, timezone('UTC', now()), $3)
           ON CONFLICT (mirror) DO UPDATE
             SET last_attempt_at = EXCLUDED.last_attempt_at, last_error = EXCLUDED.last_error"#,
    )
    .bind(mirror)
    .bind(source)
    .bind(error)
    .execute(pool)
    .await
    .context("recording the mirror error")?;
    Ok(())
}

/// A world this run did not mirror is not automatically a failure: refusing
/// to overwrite a local publish is the mirror working, an unreachable
/// upstream is the mirror broken. A refusal counts the mirrored scenes it
/// retired from beside the local publish.
enum Outcome {
    Synced {
        scenes: usize,
        new_blobs: usize,
    },
    Unchanged,
    /// A local publish, or on a read-through a local or blocked world.
    RefusedLocalPublish {
        retired: u64,
    },
    NothingUpstream(&'static str),
}

async fn scenes_pass(
    client: &crate::http::Outbound,
    pool: &PgPool,
    storage: &Arc<Storage>,
    settings: &MirrorSettings,
    owners: Option<Arc<Policy>>,
    ctx: &JobContext,
) -> Result<ScenesOutcome> {
    let mut out = ScenesOutcome::default();

    let resume: Option<(Option<String>, Value)> =
        sqlx::query("SELECT cursor, stats FROM worlds.mirror_passes WHERE mirror = 'scenes'")
            .fetch_optional(pool)
            .await?
            .map(|r| (r.get("cursor"), r.get("stats")));
    let mut cursor: String = match resume {
        Some((Some(cursor), stats)) => {
            if let Ok(prev) = serde_json::from_value::<ScenesOutcome>(stats) {
                out = ScenesOutcome {
                    interrupted: false,
                    complete: false,
                    ..prev
                };
            }
            out.resumed_from = Some(cursor.clone());
            tracing::info!(
                cursor,
                "resuming the worlds scenes pass from its persisted cursor"
            );
            cursor
        }
        _ => {
            sqlx::query(
                r#"INSERT INTO worlds.mirror_passes (mirror, source, pass_started_at, last_attempt_at, cursor, stats)
                   VALUES ('scenes', $1, timezone('UTC', now()), timezone('UTC', now()), '', '{}'::jsonb)
                   ON CONFLICT (mirror) DO UPDATE
                     SET source = EXCLUDED.source, pass_started_at = EXCLUDED.pass_started_at,
                         last_attempt_at = EXCLUDED.last_attempt_at, cursor = '', stats = '{}'::jsonb,
                         last_error = NULL"#,
            )
            .bind(&settings.source)
            .execute(pool)
            .await?;
            out.retired_scenes = retire_unlisted(pool).await?;
            String::new()
        }
    };

    let total: i64 = due_total(pool, settings).await?;
    loop {
        if ctx.is_cancelled() {
            out.interrupted = true;
            break;
        }
        let batch = due_batch(pool, settings, &cursor).await?;
        if batch.is_empty() {
            break;
        }
        ctx.progress(format!(
            "scenes: {}/{total} worlds, at {:?}",
            out.processed, batch[0].0
        ));
        let mut set = tokio::task::JoinSet::new();
        for (name, scene_ids) in &batch {
            let (client, pool, storage, settings, owners) = (
                client.clone(),
                pool.clone(),
                storage.clone(),
                settings.clone(),
                owners.clone(),
            );
            let (name, scene_ids) = (name.clone(), scene_ids.clone());
            set.spawn(async move {
                let result = mirror_world(
                    &client,
                    &pool,
                    &storage,
                    &settings,
                    owners.as_deref(),
                    &name,
                    &scene_ids,
                    None,
                )
                .await;
                (name, result)
            });
        }
        let mut results = Vec::with_capacity(batch.len());
        let cancelled = loop {
            tokio::select! {
                joined = set.join_next() => match joined {
                    Some(Ok(r)) => results.push(r),
                    Some(Err(e)) => results.push((String::new(), Err(anyhow!("mirror task: {e}")))),
                    None => break false,
                },
                _ = ctx.cancel.cancelled() => break true,
            }
        };
        if cancelled {
            set.abort_all();
            out.interrupted = true;
            break;
        }
        for (name, result) in results {
            out.processed += 1;
            match result {
                Ok(Outcome::Synced { scenes, new_blobs }) => {
                    out.synced += 1;
                    out.scenes += scenes;
                    out.new_blobs += new_blobs;
                    mark_world(pool, &name, None).await?;
                }
                Ok(Outcome::Unchanged) => {
                    out.unchanged += 1;
                    mark_world(pool, &name, None).await?;
                }
                Ok(Outcome::RefusedLocalPublish { retired }) => {
                    out.refused += 1;
                    out.retired_scenes += retired;
                    tracing::warn!(world = %name, "refused: published to this node; the mirror never replaces a local publish");
                    mark_world(pool, &name, Some("published locally; not mirrored")).await?;
                }
                Ok(Outcome::NothingUpstream(why)) => {
                    out.skipped += 1;
                    tracing::info!(world = %name, why, "skipped");
                    mark_world(pool, &name, Some(why)).await?;
                }
                Err(e) => {
                    out.failed += 1;
                    let error = format!("{e:#}");
                    // The same error as the last pass (an upstream that keeps serving a
                    // body that does not hash, say) is one WARN, then INFO per pass while it
                    // lasts; the row keeps the error either way (`last_error`).
                    let repeated = !name.is_empty()
                        && stored_error(pool, &name).await?.as_deref() == Some(error.as_str());
                    if repeated {
                        tracing::info!(world = %name, error = %error, "world mirror still failing with the error of the last pass");
                    } else {
                        tracing::warn!(world = %name, error = %error, "world mirror failed");
                    }
                    if !name.is_empty() {
                        mark_world(pool, &name, Some(&error)).await?;
                    }
                }
            }
        }
        cursor = batch.last().map(|(n, _)| n.clone()).unwrap_or(cursor);
        sqlx::query(
            r#"UPDATE worlds.mirror_passes
               SET cursor = $1, stats = $2, last_attempt_at = timezone('UTC', now())
               WHERE mirror = 'scenes'"#,
        )
        .bind(&cursor)
        .bind(serde_json::to_value(&out)?)
        .execute(pool)
        .await
        .context("persisting the scenes cursor")?;
    }

    if out.synced + out.failed > 0 || out.retired_scenes > 0 {
        super::index::invalidate_index_memo();
    }
    if !out.interrupted {
        out.complete = true;
        sqlx::query(
            r#"UPDATE worlds.mirror_passes
               SET cursor = NULL, pass_number = pass_number + 1,
                   pass_completed_at = timezone('UTC', now()),
                   last_attempt_at = timezone('UTC', now()),
                   stats = $1, last_error = NULL
               WHERE mirror = 'scenes'"#,
        )
        .bind(serde_json::to_value(&out)?)
        .execute(pool)
        .await?;
        ctx.progress(format!(
            "scenes pass complete: {} worlds, {} synced, {} unchanged, {} refused, {} failed",
            out.processed, out.synced, out.unchanged, out.refused, out.failed
        ));
    } else {
        tracing::info!(
            cursor,
            processed = out.processed,
            "worlds scenes pass interrupted; cursor persisted"
        );
    }
    Ok(out)
}

/// What a read-through fill holds a mirrored world to beyond a pass's hash checks: the newest
/// scene, and every older scene it keeps, signed by an address [`Policy::may_have_published`]
/// allows (an older scene it definitely refuses is left out), every download under the bound the
/// content read-through shares.
pub struct Verify<'a> {
    pub policy: &'a Policy,
    pub downloads: &'a Downloads,
}

/// Read-through: one world this node holds no scene of, mirrored now through the single-world
/// path a pass uses (every file hash-verified, a local publish never overwritten) with each
/// kept scene's signature and signer checked before anything is stored. `true` when scenes
/// landed.
pub async fn mirror_one(
    client: &crate::http::Outbound,
    pool: &PgPool,
    storage: &Arc<Storage>,
    settings: &MirrorSettings,
    name: &str,
    verify: &Verify<'_>,
) -> Result<bool> {
    let synced = matches!(
        mirror_world(
            client,
            pool,
            storage,
            settings,
            Some(verify.policy),
            name,
            &[],
            Some(verify)
        )
        .await?,
        Outcome::Synced { .. }
    );
    if synced {
        super::index::invalidate_index_memo();
    }
    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `permissions.access` as upstream's `removeSecrets` answers it, stored as this node can:
    /// a shared secret no secret matches, an allow-list as listed, anything else refused.
    #[test]
    fn upstream_access_is_copied_or_refused() {
        let access =
            |value: Value| parse_upstream_access(&json!({"permissions": {"access": value}}));
        assert_eq!(
            access(json!({"type": "unrestricted"})).unwrap(),
            AccessSetting::Unrestricted
        );
        let secret = access(json!({"type": "shared-secret"})).unwrap();
        let AccessSetting::SharedSecret { secret_hash } = &secret else {
            panic!("{secret:?}");
        };
        assert!(!bcrypt::verify("", secret_hash).unwrap_or(false));
        assert_eq!(
            access(json!({"type": "nft-ownership", "nft": "urn:x"})).unwrap(),
            AccessSetting::NftOwnership {
                nft: "urn:x".into()
            }
        );
        assert_eq!(
            access(json!({"type": "allow-list", "wallets": ["0xA", 1], "communities": ["c"]}))
                .unwrap(),
            AccessSetting::AllowList {
                wallets: vec!["0xA".into()],
                communities: vec!["c".into()],
            }
        );
        for refused in [
            json!({"type": "nft-ownership"}),
            json!({"type": "members-only"}),
            json!(null),
        ] {
            assert!(access(refused.clone()).is_err(), "{refused}");
        }
    }

    /// `toSnakeCaseSettings` leaves out empty text and NULL categories and answers
    /// `skybox_time: null`; only what the columns accept is copied.
    #[test]
    fn upstream_settings_are_copied_as_the_columns_accept_them() {
        let copy = WorldSettingsCopy::parse(&json!({
            "title": "Globe", "description": "", "content_rating": "X",
            "categories": ["art", 1, "games"], "skybox_time": null, "single_player": true,
            "thumbnail_hash": "bafkrei/../x", "access_type": "unrestricted", "settings_version": 7
        }))
        .unwrap();
        assert_eq!(
            copy,
            WorldSettingsCopy {
                present: true,
                title: Some("Globe".into()),
                categories: Some(vec!["art".into(), "games".into()]),
                skybox_time: Some(None),
                single_player: Some(true),
                settings_version: Some(7),
                ..WorldSettingsCopy::default()
            }
        );
        let copy = WorldSettingsCopy::parse(&json!({
            "content_rating": "T", "skybox_time": 43200.0, "settings_version": -1,
            "categories": vec!["c"; 25], "thumbnail_hash": "a".repeat(64)
        }))
        .unwrap();
        assert_eq!(copy.content_rating.as_deref(), Some("T"));
        assert_eq!(copy.skybox_time, Some(Some(43200)));
        assert_eq!(copy.settings_version, None);
        assert_eq!(copy.categories.unwrap().len(), MAX_CATEGORIES);
        assert_eq!(copy.thumbnail_hash, Some("a".repeat(64)));
        // A thumbnail key is a canonical CID or a lowercase SHA-256 hex, nothing else.
        let thumbnail = |key: &str| {
            WorldSettingsCopy::parse(&json!({"thumbnail_hash": key}))
                .unwrap()
                .thumbnail_hash
        };
        let cid = crate::auth::hash_bytes_v1(b"thumbnail");
        assert_eq!(thumbnail(&cid), Some(cid.clone()));
        for refused in ["abc", &"A".repeat(64), &"g".repeat(64), &"a".repeat(65)] {
            assert_eq!(thumbnail(refused), None, "{refused}");
        }
        assert_eq!(WorldSettingsCopy::parse(&json!([])), None);
    }

    #[test]
    fn a_world_is_unchanged_only_when_it_holds_exactly_the_upstream_scenes() {
        let cids = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert!(holds_every_scene(&cids(&["a", "b"]), &cids(&["b", "a"])));
        assert!(!holds_every_scene(&cids(&["a", "b"]), &cids(&["a"])));
        assert!(!holds_every_scene(&cids(&["a"]), &cids(&["a", "stale"])));
        assert!(!holds_every_scene(&cids(&["new"]), &cids(&["old"])));
        assert!(!holds_every_scene(&[], &[]));
    }

    #[test]
    fn an_upstream_grant_is_its_owner_and_the_deployment_allow_list_only() {
        let owner = "0x00000000000000000000000000000000000000aa";
        let deployer = "0x00000000000000000000000000000000000000BB";
        let grant = parse_upstream_grant(&json!({
            "owner": owner,
            "permissions": {
                "deployment": {"type": "allow-list", "wallets": [deployer, "not an address"]},
                "streaming": {"type": "allow-list", "wallets": ["0x00000000000000000000000000000000000000cc"]}
            }
        }))
        .unwrap();
        assert_eq!(grant.owner, Address::parse(owner).unwrap());
        assert_eq!(grant.deployers, vec![Address::parse(deployer).unwrap()]);
        let unrestricted = parse_upstream_grant(&json!({
            "owner": owner, "permissions": {"deployment": {"type": "unrestricted"}}
        }))
        .unwrap();
        assert!(unrestricted.deployers.is_empty());
        assert!(parse_upstream_grant(&json!({"permissions": {}})).is_err());
    }

    #[test]
    fn the_read_through_alone_mirrors_worlds_as_read_and_runs_no_pass() {
        let mirror = |args: &[&str]| {
            let mut argv = vec!["x", "--database-url", "postgres:///c"];
            argv.extend_from_slice(args);
            let config = crate::config::Config::parse_for_tests(&argv).unwrap();
            MirrorSettings::from_config(&config, &[]).unwrap()
        };
        let source = "https://worlds.example.org";
        assert!(mirror(&[]).is_none());
        assert!(mirror(&["--worlds-content-mirror-source", source]).is_none());
        assert!(mirror(&["--read-through-enabled", "true"]).is_none());
        let read_through = mirror(&[
            "--read-through-enabled",
            "true",
            "--worlds-content-mirror-source",
            source,
        ])
        .unwrap();
        assert!(!read_through.scheduled);
        assert!(!read_through.trust_upstream_grants);
        assert_eq!(read_through.source, source);
        let trusting = mirror(&[
            "--read-through-enabled",
            "true",
            "--worlds-content-mirror-source",
            source,
            "--worlds-read-through-trust-upstream-grants",
            "true",
        ])
        .unwrap();
        assert!(trusting.trust_upstream_grants);
        let scheduled = mirror(&[
            "--worlds-content-mirror-enabled",
            "true",
            "--worlds-content-mirror-source",
            source,
        ])
        .unwrap();
        assert!(scheduled.scheduled);
        let config = crate::config::Config::parse_for_tests(&[
            "x",
            "--database-url",
            "postgres:///c",
            "--read-through-enabled",
            "true",
            "--worlds-content-mirror-source",
            "http://10.0.0.1",
        ])
        .unwrap();
        assert!(MirrorSettings::from_config(&config, &[]).is_err());
    }

    #[test]
    fn a_world_name_is_one_encoded_segment_of_the_upstream_url() {
        assert_eq!(
            world_url("http://up.example/base/", "rt.dcl.eth", "about").unwrap(),
            "http://up.example/base/world/rt.dcl.eth/about"
        );
        assert_eq!(
            world_url("https://up.example", "rt.dcl.eth\\about?0#f/x", "about").unwrap(),
            "https://up.example/world/rt.dcl.eth%5Cabout%3F0%23f%2Fx/about"
        );
        for refused in ["", ".", ".."] {
            assert!(world_url("https://up.example", refused, "about").is_err());
        }
    }

    #[test]
    fn scene_refs_split_the_urn_into_id_and_base_url() {
        let about = json!({"configurations": {"scenesUrn": [
            "urn:decentraland:entity:bafkreicid?=&baseUrl=https://up.example/contents/",
            "urn:decentraland:entity:bafkreiother",
            42
        ]}});
        assert_eq!(
            scene_refs(&about),
            vec![
                (
                    "bafkreicid".to_string(),
                    "https://up.example/contents/".to_string()
                ),
                ("bafkreiother".to_string(), String::new()),
            ]
        );
        assert!(scene_refs(&json!({})).is_empty());
    }

    #[test]
    fn the_index_keeps_worlds_with_canonical_scene_ids_lowercased_and_sorted() {
        let index = json!({"data": [
            {"name": "Zed.dcl.eth", "scenes": [
                {"id": "bafkreie4eisvkzyjuqrcendydk6vikqs2vco5lmib4nlzsxtjzofiqy2pa", "timestamp": 5},
                {"id": "not-a-cid", "timestamp": 9}
            ]},
            {"name": "empty.dcl.eth", "scenes": []},
            {"name": "zed.dcl.eth", "scenes": [{"id": "bafkreie4eisvkzyjuqrcendydk6vikqs2vco5lmib4nlzsxtjzofiqy2pa"}]},
            {"name": "alpha", "scenes": [{"id": "QmcoQSrVoi8CKSwiRyJ3MPYyN1AUiLjHiAtYCUGoBr8JM4", "timestamp": 7}]},
            {"scenes": [{"id": "bafkreie4eisvkzyjuqrcendydk6vikqs2vco5lmib4nlzsxtjzofiqy2pa"}]}
        ]});
        let entries = parse_index(&index);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zed.dcl.eth"]);
        assert_eq!(
            entries[1].scene_ids,
            vec!["bafkreie4eisvkzyjuqrcendydk6vikqs2vco5lmib4nlzsxtjzofiqy2pa"]
        );
        assert_eq!(
            entries[1].newest_timestamp_ms,
            Some(5),
            "the dropped non-CID scene does not vote"
        );
        assert_eq!(entries[0].newest_timestamp_ms, Some(7));
        assert!(parse_index(&json!({"data": "nope"})).is_empty());
    }

    fn parse(source: &str, own: &[&str]) -> Result<MirrorSettings> {
        let own: Vec<String> = own.iter().map(|s| s.to_string()).collect();
        MirrorSettings::parse(source, &own, Duration::from_secs(60), 4, 8, 0, "")
    }

    #[test]
    fn the_source_must_be_public_https_and_never_this_node() {
        let ok = parse(
            "https://worlds-content-server.decentraland.org/",
            &["https://v2.example.org"],
        )
        .unwrap();
        assert_eq!(ok.source, "https://worlds-content-server.decentraland.org");
        assert!(!ok.allows_private_source());
        for private in [
            "http://127.0.0.1:9/",
            "https://10.0.0.5",
            "https://localhost:9",
            "https://[::1]/",
            "https://router/",
        ] {
            let err = parse(private, &[]).unwrap_err().to_string();
            assert!(
                err.contains("SSRF guard") || err.contains("plaintext"),
                "{private}: {err}"
            );
        }
        assert!(
            parse("http://worlds.example.org", &[])
                .unwrap_err()
                .to_string()
                .contains("plaintext")
        );
        assert!(parse("ftp://x", &[]).is_err());
        assert!(parse("   ", &[]).unwrap_err().to_string().contains("empty"));
        let err = parse(
            "https://v2.example.org/",
            &["https://v2.example.org:443/content/"],
        )
        .unwrap_err();
        assert!(err.to_string().contains("this node itself"), "{err}");
        assert!(
            MirrorSettings::parse(
                "https://up.example",
                &[],
                Duration::from_secs(60),
                0,
                8,
                0,
                ""
            )
            .is_err()
        );
        // The loopback is admitted only through the test constructor.
        let dev = MirrorSettings::for_tests("http://127.0.0.1:9/");
        assert!(dev.allows_private_source());
        assert_eq!(dev.source, "http://127.0.0.1:9");
    }

    #[test]
    fn names_and_limit_bound_a_run() {
        let s = MirrorSettings::parse(
            "https://up.example",
            &[],
            Duration::from_secs(60),
            4,
            8,
            25,
            " Foo.dcl.eth, bar ,,",
        )
        .unwrap();
        assert_eq!(s.limit, Some(25));
        assert_eq!(s.names, vec!["foo.dcl.eth", "bar"]);
        assert_eq!(MirrorSettings::for_tests("https://up.example").limit, None);
    }

    #[test]
    fn scene_base_urls_are_the_source_or_public_and_never_this_node() {
        let s = parse("https://up.example", &["https://v2.example.org"]).unwrap();
        assert!(s.base_url_allowed("https://up.example/contents/"));
        assert!(s.base_url_allowed("https://peer.decentraland.org/content/contents/"));
        assert!(
            !s.base_url_allowed("https://v2.example.org/contents/"),
            "a mirror loop"
        );
        assert!(
            !s.base_url_allowed("http://169.254.169.254/latest/"),
            "the SSRF guard"
        );
        assert!(!s.base_url_allowed("http://10.0.0.1/contents/"));
        assert!(!s.base_url_allowed(""));
        assert!(
            !s.base_url_allowed("https://up.example.internal/contents/"),
            "the source's own host is not exempt"
        );
        let dev = MirrorSettings::for_tests("http://127.0.0.1:9")
            .with_own_urls(&["http://127.0.0.1:5210".to_string()]);
        assert!(
            dev.base_url_allowed("http://127.0.0.1:9/contents/"),
            "the test stub itself"
        );
        assert!(!dev.base_url_allowed("http://127.0.0.1:5210/contents/"));
        assert!(
            !dev.base_url_allowed("http://127.0.0.1:10/contents/"),
            "another loopback port is not the stub"
        );
    }
    #[tokio::test]
    async fn unlisting_locks_the_world_before_scenes_and_cannot_retire_a_concurrent_publish() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let world = format!("unlist-{}.dcl.eth", uuid::Uuid::new_v4().simple());
        let mut seed = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO worlds.worlds (name, mirror_source) VALUES ($1,'https://up.example')",
        )
        .bind(&world)
        .execute(&mut *seed)
        .await
        .unwrap();
        sqlx::query("INSERT INTO worlds.world_scenes (world_name,entity_id,deployment_auth_chain,entity,deployer,parcels,size,mirror_source) VALUES ($1,'old','[]','{}','owner',ARRAY['0,0'],1,'https://up.example')").bind(&world).execute(&mut *seed).await.unwrap();
        let place = scene_identity::assign(&mut seed, &world, "old", &["0,0".into()])
            .await
            .unwrap()
            .unwrap();
        sqlx::query("INSERT INTO worlds.remote_worlds (world_name,source,scene_ids,unlisted_at) VALUES ($1,'https://up.example',ARRAY['old'],now())").bind(&world).execute(&mut *seed).await.unwrap();
        seed.commit().await.unwrap();

        let mut publish = pool.begin().await.unwrap();
        sqlx::query("SELECT name FROM worlds.worlds WHERE name=$1 FOR UPDATE")
            .bind(&world)
            .execute(&mut *publish)
            .await
            .unwrap();
        let publisher: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *publish)
            .await
            .unwrap();
        let retiring = tokio::spawn({
            let pool = pool.clone();
            async move { retire_unlisted(&pool).await }
        });
        // Wait for actual lock contention, not a scheduler sleep that could
        // let either implementation win the race accidentally.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))")
                    .bind(publisher).fetch_one(&pool).await.unwrap();
                if blocked { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("the unlisting transaction waits for the publisher's world lock");
        sqlx::query("SET LOCAL lock_timeout='500ms'")
            .execute(&mut *publish)
            .await
            .unwrap();
        sqlx::query("DELETE FROM worlds.world_scenes WHERE world_name=$1")
            .bind(&world)
            .execute(&mut *publish)
            .await
            .expect("retirement cannot hold the scene while waiting for its world");
        sqlx::query("INSERT INTO worlds.world_scenes (world_name,entity_id,deployment_auth_chain,entity,deployer,parcels,size) VALUES ($1,'new','[]','{}','owner',ARRAY['0,0'],1)").bind(&world).execute(&mut *publish).await.unwrap();
        assert_eq!(
            scene_identity::assign(&mut publish, &world, "new", &["0,0".into()])
                .await
                .unwrap(),
            Some(place)
        );
        sqlx::query("UPDATE worlds.worlds SET mirror_source=NULL WHERE name=$1")
            .bind(&world)
            .execute(&mut *publish)
            .await
            .unwrap();
        publish.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), retiring)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let binding: (String, bool) = sqlx::query_as(
            "SELECT entity_id,active FROM explore.world_scene_place WHERE identity=$1",
        )
        .bind(place)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(binding, ("new".into(), true));
        sqlx::query("DELETE FROM worlds.worlds WHERE name=$1")
            .bind(&world)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM explore.world_scene_place WHERE identity=$1")
            .bind(place)
            .execute(&pool)
            .await
            .unwrap();
    }
}
