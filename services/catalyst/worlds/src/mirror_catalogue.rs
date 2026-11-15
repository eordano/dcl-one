//! Worlds mirror catalogue.
use super::*;

pub(super) struct CatalogueEntry {
    pub(super) name: String,
    pub(super) scene_ids: Vec<String>,
    pub(super) newest_timestamp_ms: Option<i64>,
}

/// The worlds an upstream `/index` document lists with at least one scene,
/// lowercased and deduplicated (name order).
pub(super) fn parse_index(index: &Value) -> Vec<CatalogueEntry> {
    let mut seen = HashSet::new();
    let mut entries: Vec<CatalogueEntry> = index["data"]
        .as_array()
        .map(|worlds| {
            worlds
                .iter()
                .filter_map(|w| {
                    let name = w["name"].as_str()?.trim().to_ascii_lowercase();
                    let scenes: Vec<&Value> = w["scenes"]
                        .as_array()?
                        .iter()
                        .filter(|s| s["id"].as_str().is_some_and(is_canonical_cid))
                        .collect();
                    let scene_ids: Vec<String> = scenes
                        .iter()
                        .filter_map(|s| s["id"].as_str())
                        .map(str::to_string)
                        .collect();
                    if name.is_empty() || scene_ids.is_empty() || !seen.insert(name.clone()) {
                        return None;
                    }
                    let newest_timestamp_ms = scenes
                        .iter()
                        .filter_map(|s| crate::core::store::entity_timestamp_ms(s))
                        .max();
                    Some(CatalogueEntry {
                        name,
                        scene_ids,
                        newest_timestamp_ms,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

pub(super) async fn catalogue_pass(
    client: &crate::http::Outbound,
    pool: &PgPool,
    settings: &MirrorSettings,
    ctx: &JobContext,
) -> Result<CatalogueOutcome> {
    ctx.progress("catalogue: fetching /index");
    let url = format!("{}/index", settings.source);
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} answered {}", resp.status());
    }
    let body = read_body_capped(resp, MAX_INDEX_BYTES)
        .await
        .with_context(|| format!("reading {url}"))?;
    let index: Value = serde_json::from_slice(&body).with_context(|| format!("parsing {url}"))?;
    let entries = parse_index(&index);
    if entries.is_empty() {
        bail!("{url} lists no world with scenes; refusing to treat an empty catalogue as complete");
    }

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let scene_ids: Vec<Value> = entries
        .iter()
        .map(|e| Value::from(e.scene_ids.clone()))
        .collect();
    let newest: Vec<Option<i64>> = entries.iter().map(|e| e.newest_timestamp_ms).collect();

    let mut tx = pool.begin().await?;
    let previously_listed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worlds.remote_worlds WHERE unlisted_at IS NULL")
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query(
        r#"INSERT INTO worlds.remote_worlds (world_name, source, scene_ids, newest_timestamp_ms)
           SELECT t.name, $4, ARRAY(SELECT jsonb_array_elements_text(t.scene_ids)), t.newest
           FROM unnest($1::text[], $2::jsonb[], $3::bigint[]) AS t(name, scene_ids, newest)
           ON CONFLICT (world_name) DO UPDATE
             SET source = EXCLUDED.source,
                 scene_ids = EXCLUDED.scene_ids,
                 newest_timestamp_ms = EXCLUDED.newest_timestamp_ms,
                 listed_at = timezone('UTC', now()),
                 unlisted_at = NULL"#,
    )
    .bind(&names)
    .bind(&scene_ids)
    .bind(&newest)
    .bind(&settings.source)
    .execute(&mut *tx)
    .await
    .context("upserting the catalogue")?;

    let candidates: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worlds.remote_worlds WHERE unlisted_at IS NULL AND NOT (world_name = ANY($1))",
    )
    .bind(&names)
    .fetch_one(&mut *tx)
    .await?;
    let fuse = SWEEP_FUSE_MIN_CANDIDATES.max(previously_listed / 5);
    let (unlisted, complete, error) = if candidates > fuse {
        tracing::warn!(
            candidates,
            previously_listed,
            fuse,
            "catalogue pass would unlist more worlds than the fuse allows; keeping them listed"
        );
        (
            0,
            false,
            Some(format!(
                "refused to unlist {candidates} of {previously_listed} worlds (fuse {fuse})"
            )),
        )
    } else {
        let n = sqlx::query(
            r#"UPDATE worlds.remote_worlds SET unlisted_at = timezone('UTC', now())
               WHERE unlisted_at IS NULL AND NOT (world_name = ANY($1))"#,
        )
        .bind(&names)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        (n as usize, true, None)
    };
    let outcome = CatalogueOutcome {
        listed: entries.len(),
        unlisted,
        complete,
    };
    sqlx::query(
        r#"INSERT INTO worlds.mirror_passes
             (mirror, source, pass_number, pass_started_at, pass_completed_at, last_attempt_at, cursor, stats, last_error)
           VALUES ('catalogue', $1, 1, timezone('UTC', now()),
                   CASE WHEN $2 THEN timezone('UTC', now()) END,
                   timezone('UTC', now()), NULL, $3, $4)
           ON CONFLICT (mirror) DO UPDATE
             SET source = EXCLUDED.source,
                 pass_number = worlds.mirror_passes.pass_number + CASE WHEN $2 THEN 1 ELSE 0 END,
                 pass_started_at = EXCLUDED.pass_started_at,
                 pass_completed_at = COALESCE(EXCLUDED.pass_completed_at, worlds.mirror_passes.pass_completed_at),
                 last_attempt_at = EXCLUDED.last_attempt_at,
                 stats = EXCLUDED.stats,
                 last_error = EXCLUDED.last_error"#,
    )
    .bind(&settings.source)
    .bind(complete)
    .bind(serde_json::to_value(&outcome)?)
    .bind(&error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(
        listed = outcome.listed,
        unlisted,
        complete,
        "worlds catalogue mirrored"
    );
    Ok(outcome)
}

/// The scope of a pass: listed worlds, optionally only `names`, optionally
/// only the first `limit` in name order (a bounded, restart-stable run).
pub(super) async fn due_total(pool: &PgPool, settings: &MirrorSettings) -> Result<i64> {
    let n: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM (
             SELECT world_name FROM worlds.remote_worlds
             WHERE unlisted_at IS NULL
               AND (cardinality($1::text[]) = 0 OR world_name = ANY($1))
             ORDER BY world_name
             LIMIT $2) s"#,
    )
    .bind(&settings.names)
    .bind(settings.limit.map(|l| l as i64).unwrap_or(i64::MAX))
    .fetch_one(pool)
    .await?;
    Ok(n)
}

pub(super) async fn due_batch(
    pool: &PgPool,
    settings: &MirrorSettings,
    cursor: &str,
) -> Result<Vec<(String, Vec<String>)>> {
    let rows = sqlx::query(
        r#"SELECT world_name, scene_ids FROM (
             SELECT world_name, scene_ids FROM worlds.remote_worlds
             WHERE unlisted_at IS NULL
               AND (cardinality($1::text[]) = 0 OR world_name = ANY($1))
             ORDER BY world_name
             LIMIT $2) s
           WHERE world_name > $3
           ORDER BY world_name
           LIMIT $4"#,
    )
    .bind(&settings.names)
    .bind(settings.limit.map(|l| l as i64).unwrap_or(i64::MAX))
    .bind(cursor)
    .bind(settings.concurrency as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("world_name"), r.get("scene_ids")))
        .collect())
}

/// The error the last pass stored for a world, if any (`remote_worlds.last_error`).
pub(super) async fn stored_error(pool: &PgPool, name: &str) -> Result<Option<String>> {
    let stored: Option<Option<String>> =
        sqlx::query_scalar("SELECT last_error FROM worlds.remote_worlds WHERE world_name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(stored.flatten())
}

pub(super) async fn mark_world(pool: &PgPool, name: &str, error: Option<&str>) -> Result<()> {
    sqlx::query(
        r#"UPDATE worlds.remote_worlds
           SET mirrored_at = CASE WHEN $2::text IS NULL THEN timezone('UTC', now()) ELSE mirrored_at END,
               last_error = $2
           WHERE world_name = $1"#,
    )
    .bind(name)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Retires what the mirror wrote for worlds a complete catalogue pass no
/// longer lists: their mirrored scenes, their world row when nothing local
/// remains, and the catalogue row. Local scenes are never touched; blobs
/// stay in the store (content-addressed, possibly shared).
pub(super) async fn retire_unlisted(pool: &PgPool) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let candidates: Vec<String> = sqlx::query_scalar(
        "SELECT world_name FROM worlds.remote_worlds WHERE unlisted_at IS NOT NULL",
    )
    .fetch_all(&mut *tx)
    .await?;
    // Match publish/mirror lock order before touching scenes or shared places.
    // Freeze the candidate set so a later catalogue change cannot add an
    // unlocked world to any of the deletes below.
    let locked: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM worlds.worlds WHERE name = ANY($1) ORDER BY name FOR UPDATE",
    )
    .bind(&candidates)
    .fetch_all(&mut *tx)
    .await?;
    let retired: Vec<String> = sqlx::query_scalar(
        r#"DELETE FROM worlds.world_scenes s
           USING worlds.remote_worlds r
           WHERE r.world_name = ANY($1) AND r.unlisted_at IS NOT NULL AND lower(s.world_name) = r.world_name
             AND s.mirror_source IS NOT NULL
           RETURNING lower(s.world_name)"#,
    )
    .bind(&locked)
    .fetch_all(&mut *tx)
    .await?;
    let scenes = retired.len() as u64;
    scene_identity::retire_orphans(&mut tx, &retired).await?;
    sqlx::query(
        r#"DELETE FROM worlds.worlds w
           USING worlds.remote_worlds r
           WHERE r.world_name = ANY($1) AND r.unlisted_at IS NOT NULL AND lower(w.name) = r.world_name
             AND w.mirror_source IS NOT NULL
             AND NOT EXISTS (SELECT 1 FROM worlds.world_scenes s WHERE s.world_name = w.name)"#,
    )
    .bind(&locked)
    .execute(&mut *tx)
    .await?;
    let worlds = sqlx::query(
        "DELETE FROM worlds.remote_worlds r WHERE world_name = ANY($1) AND unlisted_at IS NOT NULL AND (world_name = ANY($2) OR NOT EXISTS (SELECT 1 FROM worlds.worlds w WHERE w.name = r.world_name))",
    )
    .bind(&candidates)
    .bind(&locked)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    if worlds > 0 {
        tracing::info!(
            worlds,
            scenes,
            "retired worlds the catalogue no longer lists"
        );
    }
    Ok(scenes)
}
