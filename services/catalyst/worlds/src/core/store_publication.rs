//! Worlds publication persistence on the shared pool.
use super::*;

/// `$delete` (a `DELETE FROM worlds.world_scenes ...`) moving the rows it
/// removes into `worlds.retired_world_scenes` (a re-retired id restarts its TTL).
macro_rules! retiring {
    ($delete:literal) => {
        concat!(
            "WITH gone AS (",
            $delete,
            " RETURNING world_name, entity_id, entity, parcels, mirror_source) \
             INSERT INTO worlds.retired_world_scenes (world_name, entity_id, entity, parcels, mirror_source) \
             SELECT world_name, entity_id, entity, parcels, mirror_source FROM gone \
             ON CONFLICT (world_name, entity_id) DO UPDATE SET entity = EXCLUDED.entity, \
               parcels = EXCLUDED.parcels, mirror_source = EXCLUDED.mirror_source, \
               retired_at = timezone('UTC', now())"
        )
    };
}

/// The canonical parcel a stored scene's downstream identity uses:
/// [`ParcelSet::effective_base`] over the stored parcels.
pub fn effective_base_parcel(entity: &Value, parcels: &[String]) -> Option<String> {
    let set: ParcelSet = parcels
        .iter()
        .filter_map(|p| Parcel::parse(p).ok())
        .collect();
    let declared = entity
        .get("metadata")
        .and_then(|m| m.get("scene"))
        .and_then(|s| s.get("base"))
        .and_then(|b| b.as_str());
    set.effective_base(declared).map(|p| p.canonical())
}

pub async fn ensure_world(
    pool: &PgPool,
    world: &WorldName,
    owner: &Address,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO worlds.worlds (name, owner) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING",
    )
    .bind(world.key())
    .bind(owner)
    .execute(pool)
    .await?;
    Ok(())
}

/// One transaction: upsert the world (owner from a proven `Deploy::Owner`,
/// an operator disable kept, settings refreshed from the entity when the
/// scene ends up the sole occupant), refuse (400) when an overlapping scene
/// is newer (greater entity timestamp, ties by byte-wise greater entity id), delete
/// what the authorization allows replacing (a
/// leftover overlap under a scoped authorization is a 409 and rolls back),
/// upsert the scene row as a local publish and apply the storage identity
/// rule (`scene_identity`). A world this node only mirrored
/// becomes local here: every mirrored scene goes, overlapping or not, and
/// the world row's `mirror_source` is cleared in the same transaction, so
/// the row says what its scenes are (the mirror refuses to write a world
/// with a local scene from then on, `worlds::mirror`).
/// The refusal for the first key the scene names that the store no longer holds.
async fn missing_stored_file(
    storage: &Storage,
    entity_id: &str,
    entity: &Value,
) -> Result<Option<ApiError>, ApiError> {
    let mut named: Vec<(String, String)> = vec![(entity_id.to_string(), entity_id.to_string())];
    if let Some(Value::Array(items)) = entity.get("content") {
        for item in items {
            if let Some(hash) = item.get("hash").and_then(Value::as_str) {
                let file = item.get("file").and_then(Value::as_str).unwrap_or("");
                named.push((file.to_string(), hash.to_string()));
            }
        }
    }
    // Only a canonical CID can be a stored (and so collectable) key.
    named.retain(|(_, hash)| crate::auth::is_canonical_cid(hash));
    let hashes: Vec<&str> = named.iter().map(|(_, h)| h.as_str()).collect();
    let present = storage
        .exist_multiple(&hashes)
        .await
        .map_err(|e| ApiError::internal(format!("checking stored content: {e}")))?;
    Ok(named
        .iter()
        .find(|(_, hash)| !present.get(hash).copied().unwrap_or(false))
        .map(|(file, hash)| {
            // Validation saw every key, so a missing one was collected meanwhile: retryable.
            ApiError::service_unavailable(format!(
                "The file {file} ({hash}) was removed from storage while the deployment was in progress, please retry"
            ))
        }))
}

/// The transaction advisory lock key a scene publish of `world` holds.
pub(crate) fn publish_lock_key(world: &WorldName) -> String {
    format!("worlds-publish:{}", world.key())
}

/// Persists one scene deploy; `Ok(Some(previous))` when it replaced the
/// world's stored owner with a different one.
pub async fn deploy_scene(
    pool: &PgPool,
    storage: &Storage,
    deployment: SceneDeployment<'_>,
) -> Result<Option<Address>, ApiError> {
    deploy_scene_with(pool, storage, deployment, None)
        .await
        .map(|d| d.previous_owner)
}

/// [`deploy_scene`] that answers [`Deployed`] and, with `completion`, consumes
/// the entity's partial upload; any other publish drops the entity's staging state.
pub async fn deploy_scene_with(
    pool: &PgPool,
    storage: &Storage,
    deployment: SceneDeployment<'_>,
    completion: Option<&PartialCompletion<'_>>,
) -> Result<Deployed, ApiError> {
    let SceneDeployment {
        world,
        name_owner,
        entity_id,
        deployer,
        deployment_auth_chain,
        entity,
        parcels,
        size,
        replacement,
        forward_to,
        quota,
    } = deployment;
    let mut s = scene_settings_from_entity(entity);
    if let Some(hash) = s.thumbnail_hash.take() {
        s.thumbnail_hash = storable_thumbnail_hash(storage, &hash).await;
    }
    let parcels = parcels.canonical();

    let mut tx = pool.begin().await?;

    // The collector unlinks under the exclusive side: every key named here exists until commit.
    catalyst_storage::gc::lock_store_shared(&mut tx).await?;
    if let Some(error) = missing_stored_file(storage, entity_id, entity).await? {
        return Err(error);
    }

    // Serializes publishes of one world, so a first publish cannot race past the owner read.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(publish_lock_key(world))
        .execute(&mut *tx)
        .await?;
    // Locks an existing row so the owner read is the one this write replaces.
    let previous_owner: Option<String> =
        sqlx::query_scalar("SELECT owner FROM worlds.worlds WHERE name = $1 FOR UPDATE")
            .bind(world.key())
            .fetch_optional(&mut *tx)
            .await?
            .flatten();

    let (is_insert, disabled): (bool, bool) = sqlx::query_as(
        r#"INSERT INTO worlds.worlds AS w (
               name, owner, blocked_since, spawn_coordinates,
               title, description, content_rating, skybox_time, categories,
               single_player, show_in_places, thumbnail_hash, updated_at
           )
           VALUES ($1, COALESCE($2, $12), NULL, $3, $4, $5, $6, $7, $8::text[],
                   COALESCE($9, false), COALESCE($10, true), $11, timezone('UTC', now()))
           ON CONFLICT (name) DO UPDATE SET
             owner = COALESCE($2, w.owner),
             mirror_source = NULL,
             spawn_coordinates = COALESCE(w.spawn_coordinates, EXCLUDED.spawn_coordinates),
             updated_at = timezone('UTC', now())
           RETURNING (xmax = 0), blocked_since IS NOT NULL"#,
    )
    .bind(world.key())
    .bind(name_owner)
    .bind(&s.spawn_coordinates)
    .bind(&s.title)
    .bind(&s.description)
    .bind(&s.content_rating)
    .bind(s.skybox_time)
    .bind(&s.categories)
    .bind(s.single_player)
    .bind(s.show_in_places)
    .bind(&s.thumbnail_hash)
    .bind(deployer)
    .fetch_one(&mut *tx)
    .await?;

    // Under the world row's lock, so a disable racing this publish wins.
    if disabled {
        return Err(world_disabled(world));
    }

    // Under the world row's lock, so a concurrent publish cannot slip past.
    // A stored timestamp counts no later than the row's write (a future-dated
    // entity cannot lock the parcels); a non-numeric one never blocks.
    let newer_deployed = newer_scene_deployed(
        &mut tx,
        world,
        &parcels,
        entity_id,
        entity_timestamp_value(entity)?,
    )
    .await?;
    if newer_deployed {
        return Err(ApiError::bad_request(
            "Deployment failed: a newer scene is already deployed on one or more of these parcels.",
        ));
    }

    if !is_insert {
        let sole_occupant: bool = sqlx::query_scalar(
            "SELECT count(*) FILTER (WHERE NOT (parcels && $2::text[])) = 0 FROM worlds.world_scenes WHERE world_name = $1",
        )
        .bind(world.key())
        .bind(&parcels)
        .fetch_one(&mut *tx)
        .await?;

        if sole_occupant {
            sqlx::query(
                r#"UPDATE worlds.worlds SET
                     title = COALESCE($2, title),
                     description = COALESCE($3, description),
                     content_rating = COALESCE($4, content_rating),
                     skybox_time = COALESCE($5, skybox_time),
                     categories = COALESCE($6::text[], categories),
                     single_player = COALESCE($7, single_player),
                     show_in_places = COALESCE($8, show_in_places),
                     thumbnail_hash = COALESCE($9, thumbnail_hash),
                     settings_version = settings_version + 1,
                     updated_at = timezone('UTC', now())
                   WHERE name = $1
                     AND (title, description, content_rating, skybox_time, categories,
                          single_player, show_in_places, thumbnail_hash)
                         IS DISTINCT FROM
                         (COALESCE($2, title), COALESCE($3, description),
                          COALESCE($4, content_rating), COALESCE($5, skybox_time),
                          COALESCE($6::text[], categories), COALESCE($7, single_player),
                          COALESCE($8, show_in_places), COALESCE($9, thumbnail_hash))"#,
            )
            .bind(world.key())
            .bind(&s.title)
            .bind(&s.description)
            .bind(&s.content_rating)
            .bind(s.skybox_time)
            .bind(&s.categories)
            .bind(s.single_player)
            .bind(s.show_in_places)
            .bind(&s.thumbnail_hash)
            .execute(&mut *tx)
            .await?;
        }
    }

    // The scene's own row is never part of the overlap: a redeploy of the same
    // entity keeps its row (and its `created_at`) through the upsert below.
    match replacement {
        Deploy::Owner => {
            sqlx::query(retiring!(
                "DELETE FROM worlds.world_scenes WHERE world_name = $1 AND parcels && $2::text[] AND entity_id <> $3"
            ))
            .bind(world.key())
            .bind(&parcels)
            .bind(entity_id)
            .execute(&mut *tx)
            .await?;
        }
        Deploy::Scoped(ids) => {
            let ids: Vec<&str> = ids.iter().map(SceneId::as_str).collect();
            sqlx::query(retiring!("DELETE FROM worlds.world_scenes WHERE world_name = $1 AND parcels && $2::text[] AND entity_id = ANY($3::text[]) AND entity_id <> $4"))
                .bind(world.key())
                .bind(&parcels)
                .bind(&ids)
                .bind(entity_id)
                .execute(&mut *tx)
                .await?;
            let leftover: Option<String> =
                sqlx::query_scalar("SELECT entity_id FROM worlds.world_scenes WHERE world_name = $1 AND parcels && $2::text[] AND entity_id <> $3 LIMIT 1")
                    .bind(world.key())
                    .bind(&parcels)
                    .bind(entity_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if leftover.is_some() {
                return Err(ApiError::conflict(format!(
                    "Scene replacement authorization changed while deploying to world \"{}\". Please retry.",
                    world.display()
                )));
            }
        }
    }

    // The mirror never refreshes a world with a local scene, so a mirrored
    // scene left beside this one would stay stale for good.
    sqlx::query(retiring!(
        "DELETE FROM worlds.world_scenes WHERE world_name = $1 AND mirror_source IS NOT NULL"
    ))
    .bind(world.key())
    .execute(&mut *tx)
    .await?;

    let auth_chain_text = serde_json::to_string(deployment_auth_chain)
        .map_err(|e| ApiError::internal(format!("auth chain: {e}")))?;
    let created_ms: i64 = sqlx::query_scalar(
        r#"INSERT INTO worlds.world_scenes
             (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size, mirror_source)
           VALUES ($1, $2, $3::json, $4, $5, $6::text[], $7, NULL)
           ON CONFLICT (world_name, entity_id) DO UPDATE
             SET deployment_auth_chain = EXCLUDED.deployment_auth_chain,
                 entity = EXCLUDED.entity,
                 deployer = EXCLUDED.deployer,
                 parcels = EXCLUDED.parcels,
                 size = EXCLUDED.size,
                 mirror_source = NULL,
                 updated_at = timezone('UTC', now())
           RETURNING (extract(epoch FROM created_at) * 1000)::bigint"#,
    )
    .bind(world.key())
    .bind(entity_id)
    .bind(auth_chain_text)
    .bind(entity)
    .bind(deployer)
    .bind(&parcels)
    .bind(size)
    .fetch_one(&mut *tx)
    .await?;
    // Active again: a (world, entity) is active or retired, never both.
    sqlx::query("DELETE FROM worlds.retired_world_scenes WHERE world_name = $1 AND entity_id = $2")
        .bind(world.key())
        .bind(entity_id)
        .execute(&mut *tx)
        .await?;
    // The scene's storage identity, and the retired ones' (replaced, mirrored).
    crate::explore::places::scene_identity::assign(&mut tx, world.key(), entity_id, &parcels)
        .await?;
    crate::explore::places::scene_identity::retire_orphans(&mut tx, &[world.key().to_string()])
        .await?;

    if let Some(charge) = quota
        && let Some(max) = crate::core::quota::recheck_charge(&mut tx, charge, size).await?
    {
        return Err(ApiError::bad_request(format!(
            "Deployment failed: {}",
            crate::core::quota::too_big(max, size)
        )));
    }

    if let Some(target) = forward_to {
        // A re-publish of the same entity replaces its record: the new id
        // keeps a send still in flight for the old one from finishing it.
        // The world row's lock above serializes publishes of one world.
        sqlx::query("DELETE FROM worlds.forward_outbox WHERE entity_id = $1")
            .bind(entity_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO worlds.forward_outbox (entity_id, world_name, target) VALUES ($1, $2, $3)",
        )
        .bind(entity_id)
        .bind(world.key())
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }

    // Any publish of the entity supersedes its partial upload; only the completing one keeps a receipt.
    let pending_live: Option<bool> = sqlx::query_scalar(
        "DELETE FROM worlds.pending_scenes WHERE entity_id = $1 \
         RETURNING created_at > timezone('UTC', now()) - make_interval(secs => $2::float8 / 1000)",
    )
    .bind(entity_id)
    .bind(completion.map_or(0.0, |c| c.ttl_ms as f64))
    .fetch_optional(&mut *tx)
    .await?;
    match completion {
        Some(completion) => {
            if pending_live != Some(true) {
                return Err(ApiError::bad_request(PARTIAL_UPLOAD_EXPIRED));
            }
            sqlx::query(
                "INSERT INTO worlds.completed_scene_uploads \
                   (entity_id, deployer, world_name, parcels, creation_timestamp) \
                 VALUES ($1, $2, $3, $4::text[], $5) \
                 ON CONFLICT (entity_id) DO UPDATE SET deployer = EXCLUDED.deployer, \
                   world_name = EXCLUDED.world_name, parcels = EXCLUDED.parcels, \
                   creation_timestamp = EXCLUDED.creation_timestamp, \
                   completed_at = timezone('UTC', now())",
            )
            .bind(entity_id)
            .bind(deployer)
            .bind(completion.world_name)
            .bind(completion.scene_parcels)
            .bind(created_ms)
            .execute(&mut *tx)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM worlds.completed_scene_uploads WHERE entity_id = $1")
                .bind(entity_id)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(Deployed {
        created_at_ms: created_ms,
        previous_owner: previous_owner
            .and_then(|p| Address::parse(&p).ok())
            .filter(|p| name_owner.is_some_and(|owner| owner != p)),
    })
}

/// Whether a scene of `world` overlapping `parcels` is newer than this entity: a stored
/// timestamp counts no later than its row's write (a future-dated entity cannot lock the
/// parcels), ties break by byte-wise greater entity id, a non-numeric one never blocks. Both
/// timestamps are compared as doubles, as upstream compares JS numbers: a `numeric` cast of a
/// double keeps only 15 significant digits, two decimals of an epoch-ms value.
pub async fn newer_scene_deployed(
    conn: &mut sqlx::PgConnection,
    world: &WorldName,
    parcels: &[String],
    entity_id: &str,
    timestamp: f64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"WITH stored AS (
             SELECT entity_id,
                    CASE WHEN jsonb_typeof(entity->'timestamp') = 'number'
                         THEN LEAST((entity->>'timestamp')::float8,
                                    (extract(epoch FROM updated_at) * 1000)::float8)
                    END AS ts
             FROM worlds.world_scenes
             WHERE world_name = $1 AND parcels && $2::text[] AND entity_id <> $3)
           SELECT EXISTS (SELECT 1 FROM stored
             WHERE ts > $4::float8 OR (ts = $4::float8 AND entity_id COLLATE "C" > $3))"#,
    )
    .bind(world.key())
    .bind(parcels)
    .bind(entity_id)
    .bind(timestamp)
    .fetch_one(conn)
    .await
}

/// Refused publish to a world the operator disabled (`/admin/worlds/{w}/disable`).
pub fn world_disabled(world: &WorldName) -> ApiError {
    ApiError::forbidden(format!(
        "Deployment failed: World \"{}\" is disabled and does not accept deployments.",
        world.display()
    ))
}

/// The entity's `timestamp` as the number it is: any finite JSON number, as upstream reads it.
pub fn entity_timestamp_number(entity: &Value) -> Option<f64> {
    entity
        .get("timestamp")
        .and_then(Value::as_f64)
        .filter(|ts| ts.is_finite())
}

/// The entity's `timestamp` in whole milliseconds: an integer as it is, a fractional one taken
/// down to its millisecond.
pub fn entity_timestamp_ms(entity: &Value) -> Option<i64> {
    let value = entity.get("timestamp");
    value.and_then(Value::as_i64).or_else(|| {
        entity_timestamp_number(entity)
            .filter(|ts| ts.abs() < i64::MAX as f64)
            .map(|ts| ts.floor() as i64)
    })
}

/// [`entity_timestamp_ms`], refused as upstream's schema would when missing.
pub(crate) fn entity_timestamp(entity: &Value) -> Result<i64, ApiError> {
    entity_timestamp_ms(entity)
        .ok_or_else(|| ApiError::bad_request("The entity is missing a valid timestamp"))
}

/// [`entity_timestamp_number`], refused as upstream's schema would when missing.
pub(crate) fn entity_timestamp_value(entity: &Value) -> Result<f64, ApiError> {
    entity_timestamp_number(entity)
        .ok_or_else(|| ApiError::bad_request("The entity is missing a valid timestamp"))
}

async fn delete_scenes_locked(
    pool: &PgPool,
    world: &WorldName,
    sql: &'static str,
    bind: impl FnOnce(
        sqlx::query::Query<'static, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'static, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<u64, ApiError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT name FROM worlds.worlds WHERE name = $1 FOR UPDATE")
        .bind(world.key())
        .execute(&mut *tx)
        .await?;
    let affected = bind(sqlx::query(sql))
        .execute(&mut *tx)
        .await?
        .rows_affected();
    crate::explore::places::scene_identity::retire_orphans(&mut tx, &[world.key().to_string()])
        .await?;
    recompute_spawn(&mut tx, world).await?;
    tx.commit().await?;
    Ok(affected)
}

/// The spawn after scenes left the world (WCS `undeployScene`/`undeployWorld`):
/// kept while inside the remaining scenes' bounding rectangle, else its
/// centre (floored); NULL once no scene remains. Bumps the settings version
/// when it changes. Runs under the world row lock.
async fn recompute_spawn(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world: &WorldName,
) -> Result<(), ApiError> {
    let Some(row) = sqlx::query(
        r#"SELECT w.spawn_coordinates, br.min_x, br.max_x, br.min_y, br.max_y
           FROM worlds.worlds w
           LEFT JOIN LATERAL (
             SELECT min(split_part(p, ',', 1)::int) AS min_x,
                    max(split_part(p, ',', 1)::int) AS max_x,
                    min(split_part(p, ',', 2)::int) AS min_y,
                    max(split_part(p, ',', 2)::int) AS max_y
             FROM worlds.world_scenes ws, unnest(ws.parcels) AS p
             WHERE ws.world_name = w.name
           ) br ON true
           WHERE w.name = $1"#,
    )
    .bind(world.key())
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(());
    };
    let current: Option<String> = row.get("spawn_coordinates");
    let col = |c: &str| row.get::<Option<i32>, _>(c);
    let rect = (|| Some((col("min_x")?, col("max_x")?, col("min_y")?, col("max_y")?)))();
    let next = spawn_within(current.as_deref(), rect);
    if next != current {
        sqlx::query(
            "UPDATE worlds.worlds SET spawn_coordinates = $2, settings_version = settings_version + 1, \
             updated_at = timezone('UTC', now()) WHERE name = $1",
        )
        .bind(world.key())
        .bind(&next)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// `rect` is `(min_x, max_x, min_y, max_y)`; see [`recompute_spawn`].
fn spawn_within(current: Option<&str>, rect: Option<(i32, i32, i32, i32)>) -> Option<String> {
    let (min_x, max_x, min_y, max_y) = rect?;
    let current = current?;
    let inside = Parcel::parse(current)
        .map(|p| (min_x..=max_x).contains(&p.x) && (min_y..=max_y).contains(&p.y))
        .unwrap_or(false);
    if inside {
        return Some(current.to_string());
    }
    let mid = |a: i32, b: i32| (i64::from(a) + i64::from(b)).div_euclid(2);
    Some(format!("{},{}", mid(min_x, max_x), mid(min_y, max_y)))
}

/// Deletes the retired scenes older than `ttl` (`SCENE_EVICTION_TTL_MS`);
/// their content becomes collectable. Returns the rows evicted.
pub async fn evict_retired_scenes(
    pool: &PgPool,
    ttl: std::time::Duration,
) -> Result<u64, ApiError> {
    let ttl = chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::MAX);
    let cutoff = chrono::Utc::now()
        .naive_utc()
        .checked_sub_signed(ttl)
        .unwrap_or(NaiveDateTime::MIN);
    Ok(
        sqlx::query("DELETE FROM worlds.retired_world_scenes WHERE retired_at < $1")
            .bind(cutoff)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

/// Undeploy every scene standing on `parcel`. `authorized` scopes the delete
/// to those exact identities (a parcel-scoped deployer cannot remove a scene
/// reaching into parcels it was never granted); `None` (the owner) is
/// unrestricted.
pub async fn undeploy_scene(
    pool: &PgPool,
    world: &WorldName,
    parcel: Parcel,
    authorized: Option<&[SceneId]>,
) -> Result<u64, ApiError> {
    let key = world.key().to_string();
    let parcel = parcel.canonical();
    match authorized {
        Some(ids) => {
            let ids: Vec<String> = ids.iter().map(|s| s.as_str().to_string()).collect();
            delete_scenes_locked(
                pool,
                world,
                retiring!("DELETE FROM worlds.world_scenes WHERE world_name = $1 AND $2 = ANY(parcels) AND entity_id = ANY($3::text[])"),
                move |q| q.bind(key).bind(parcel).bind(ids),
            )
            .await
        }
        None => {
            delete_scenes_locked(
                pool,
                world,
                retiring!(
                    "DELETE FROM worlds.world_scenes WHERE world_name = $1 AND $2 = ANY(parcels)"
                ),
                move |q| q.bind(key).bind(parcel),
            )
            .await
        }
    }
}

pub async fn undeploy_world(pool: &PgPool, world: &WorldName) -> Result<u64, ApiError> {
    let key = world.key().to_string();
    delete_scenes_locked(
        pool,
        world,
        retiring!("DELETE FROM worlds.world_scenes WHERE world_name = $1"),
        move |q| q.bind(key),
    )
    .await
}
