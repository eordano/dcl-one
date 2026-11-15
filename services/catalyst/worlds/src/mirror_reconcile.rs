//! Worlds mirror reconcile.
use super::*;

/// A world with a local publish holds no mirrored scene: the mirror never
/// refreshes it again, so what it wrote there before is retired and the world
/// row says local, as a publish leaves it (core::store::deploy_scene).
async fn retire_beside_local(pool: &PgPool, name: &str) -> Result<u64> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT name FROM worlds.worlds WHERE name = lower($1) FOR UPDATE")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    let retired = sqlx::query(
        r#"DELETE FROM worlds.world_scenes
           WHERE world_name = lower($1) AND mirror_source IS NOT NULL
             AND EXISTS (SELECT 1 FROM worlds.world_scenes l
                         WHERE l.world_name = lower($1) AND l.mirror_source IS NULL)"#,
    )
    .bind(name)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    scene_identity::retire_orphans(&mut tx, &[name.to_lowercase()]).await?;
    sqlx::query(
        r#"UPDATE worlds.worlds SET mirror_source = NULL, updated_at = timezone('UTC', now())
           WHERE name = lower($1) AND mirror_source IS NOT NULL
             AND EXISTS (SELECT 1 FROM worlds.world_scenes l
                         WHERE l.world_name = lower($1) AND l.mirror_source IS NULL)"#,
    )
    .bind(name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if retired > 0 {
        tracing::info!(world = %name, retired, "retired the mirrored scenes beside a local publish");
    }
    Ok(retired)
}

/// Who holds `world`'s name as this node's own owner lookup says ([`Policy::current_owner`]),
/// asked only when an owner marked a setting of the row here: the source's `/permissions` owner
/// is the source's stored copy and trails a sale by up to its owner job's interval. `None`
/// (no policy, no answer, nobody) keeps every mark ([`mirror_keeps_local`]).
async fn name_holder(pool: &PgPool, owners: Option<&Policy>, world: &WorldName) -> Option<Address> {
    let owners = owners?;
    let marked: bool = sqlx::query_scalar(
        "SELECT access_set_here OR settings_set_here FROM worlds.worlds WHERE name = $1",
    )
    .bind(world.key())
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(false);
    if !marked {
        return None;
    }
    match owners.current_owner(world).await {
        Ok(holder) => holder,
        Err(e) => {
            tracing::warn!(%world, error = ?e, "name holder unknown; the marks set here are kept");
            None
        }
    }
}

/// Where a checked scene's entity file is when the world is stored.
enum EntityFile {
    /// Already in the store.
    Stored,
    /// Fetched (and on a read-through checked), stored from memory.
    Held(Bytes),
}

/// Whether this node keeps `name` itself, so a read-through must not write it: a local world
/// (no `mirror_source`, with scenes or not) or a blocked one.
async fn kept_here(pool: &PgPool, name: &str) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM worlds.worlds WHERE name = lower($1) \
           AND (mirror_source IS NULL OR blocked_since IS NOT NULL))",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .with_context(|| format!("checking whether {name} is kept here"))
}

pub(super) fn holds_every_scene(upstream: &[String], held: &[String]) -> bool {
    !upstream.is_empty()
        && upstream.len() == held.len()
        && upstream.iter().all(|cid| held.contains(cid))
}

/// One scene of a world as it is stored.
struct SceneRecord {
    cid: String,
    entity: Value,
    parcels: Vec<String>,
    size: i64,
    chain: Value,
    deployer: String,
    created_at: Option<chrono::NaiveDateTime>,
    updated_at: Option<chrono::NaiveDateTime>,
}

/// Writes a source's settings answer onto a mirrored row that is not disabled, unless the owner
/// that set its settings here still holds the name ([`mirror_keeps_local`], with [`name_holder`]). A thumbnail the pass could not fetch keeps the stored one. `settings_version` never goes back: it takes the source's counter, or this row's
/// plus one when a copied column changes and the source's is not ahead, so a consumer ordering
/// by it never sees an update out of order (a row bumped by earlier ingests can be ahead of its
/// source). No write when nothing changes.
async fn apply_settings(
    conn: &mut sqlx::PgConnection,
    world: &WorldName,
    copy: &WorldSettingsCopy,
    holder: Option<&Address>,
) -> Result<()> {
    let name = world.key();
    if mirror_keeps_local(conn, world, SetHere::Settings, holder)
        .await
        .with_context(|| format!("settings of {name}"))?
        != Some(false)
    {
        return Ok(());
    }
    sqlx::query(
        r#"WITH cur AS (
             SELECT name, title, description, content_rating, categories, thumbnail_hash,
                    skybox_time, single_player, show_in_places
             FROM worlds.worlds
             WHERE name = $1 AND mirror_source IS NOT NULL AND blocked_since IS NULL
           ), n AS (
             SELECT name,
                    $2::text AS title,
                    $3::text AS description,
                    $4::text AS content_rating,
                    $5::text[] AS categories,
                    CASE WHEN $12 THEN thumbnail_hash ELSE $6::text END AS thumbnail_hash,
                    CASE WHEN $8 THEN $7::integer ELSE skybox_time END AS skybox_time,
                    COALESCE($9, single_player) AS single_player,
                    COALESCE($10, show_in_places) AS show_in_places,
                    COALESCE($11::bigint, 0) AS source_version
             FROM cur
           ), d AS (
             SELECT n.*,
                    (cur.title, cur.description, cur.content_rating, cur.categories,
                     cur.thumbnail_hash, cur.skybox_time, cur.single_player, cur.show_in_places)
                      IS DISTINCT FROM
                    (n.title, n.description, n.content_rating, n.categories, n.thumbnail_hash,
                     n.skybox_time, n.single_player, n.show_in_places) AS changed
             FROM n JOIN cur USING (name)
           )
           UPDATE worlds.worlds w
           SET title = d.title, description = d.description, content_rating = d.content_rating,
               categories = d.categories, thumbnail_hash = d.thumbnail_hash,
               skybox_time = d.skybox_time, single_player = d.single_player,
               show_in_places = d.show_in_places,
               settings_version = GREATEST(d.source_version,
                                           w.settings_version + CASE WHEN d.changed THEN 1 ELSE 0 END),
               updated_at = timezone('UTC', now())
           FROM d
           WHERE w.name = d.name AND (d.changed OR w.settings_version < d.source_version)"#,
    )
    .bind(name)
    .bind(&copy.title)
    .bind(&copy.description)
    .bind(&copy.content_rating)
    .bind(&copy.categories)
    .bind(&copy.thumbnail_hash)
    .bind(copy.skybox_time.flatten())
    .bind(copy.skybox_time.is_some())
    .bind(copy.single_player)
    .bind(copy.show_in_places)
    .bind(copy.settings_version)
    .bind(copy.thumbnail_unread)
    .execute(&mut *conn)
    .await
    .with_context(|| format!("settings of {name}"))?;
    Ok(())
}

/// Brings the held rows of a world whose scene set is unchanged to what its listing says: the
/// deploy times and size, and on a row without a chain the listed chain and deployer once they
/// check ([`listed_signature`]). Rows mirrored before these were kept, and a listing that gave
/// none then, are filled here; a row the listing does not name is left as it is.
async fn refresh_listed_scenes(
    conn: &mut sqlx::PgConnection,
    name: &str,
    listed: &[ListedScene],
) -> Result<()> {
    let held: Vec<(String, Value, bool)> = sqlx::query_as(
        "SELECT entity_id, entity, CASE WHEN json_typeof(deployment_auth_chain) = 'array' \
           THEN json_array_length(deployment_auth_chain) = 0 ELSE true END \
         FROM worlds.world_scenes WHERE world_name = lower($1) AND mirror_source IS NOT NULL",
    )
    .bind(name)
    .fetch_all(&mut *conn)
    .await
    .with_context(|| format!("reading the scenes held for {name}"))?;
    let mut cids = Vec::new();
    let mut sizes = Vec::new();
    let mut chains = Vec::new();
    let mut deployers = Vec::new();
    let mut created = Vec::new();
    let mut updated = Vec::new();
    for (cid, entity, unsigned) in held {
        let Some(scene) = listed.iter().find(|scene| scene.id == cid) else {
            continue;
        };
        let (chain, deployer) = match unsigned {
            true => listed_signature(scene, &entity).unzip(),
            false => (None, None),
        };
        cids.push(cid);
        sizes.push(scene.size);
        chains.push(chain);
        deployers.push(deployer);
        created.push(scene.created_at);
        updated.push(scene.updated_at);
    }
    sqlx::query(
        r#"UPDATE worlds.world_scenes s
           SET size = COALESCE(t.size, s.size),
               deployment_auth_chain = COALESCE(t.chain::json, s.deployment_auth_chain),
               deployer = COALESCE(t.deployer, s.deployer),
               created_at = COALESCE(t.created, s.created_at),
               updated_at = COALESCE(t.updated, t.created, s.updated_at)
           FROM unnest($2::text[], $3::bigint[], $4::jsonb[], $5::text[], $6::timestamp[],
                       $7::timestamp[]) AS t(cid, size, chain, deployer, created, updated)
           WHERE s.world_name = lower($1) AND s.entity_id = t.cid AND s.mirror_source IS NOT NULL
             AND (t.chain IS NOT NULL
                  OR (s.size, s.created_at, s.updated_at) IS DISTINCT FROM
                     (COALESCE(t.size, s.size), COALESCE(t.created, s.created_at),
                      COALESCE(t.updated, t.created, s.updated_at)))"#,
    )
    .bind(name)
    .bind(&cids)
    .bind(&sizes)
    .bind(&chains)
    .bind(&deployers)
    .bind(&created)
    .bind(&updated)
    .execute(&mut *conn)
    .await
    .with_context(|| format!("refreshing the scenes of {name}"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn mirror_world(
    client: &crate::http::Outbound,
    pool: &PgPool,
    storage: &Arc<Storage>,
    settings: &MirrorSettings,
    owners: Option<&Policy>,
    name: &str,
    catalogue_scene_ids: &[String],
    verify: Option<&Verify<'_>>,
) -> Result<Outcome> {
    let source = settings.source.as_str();
    let locally_published: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM worlds.world_scenes WHERE world_name = lower($1) AND mirror_source IS NULL)",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .with_context(|| format!("checking local publish for {name}"))?;
    if locally_published {
        let retired = retire_beside_local(pool, name).await?;
        return Ok(Outcome::RefusedLocalPublish { retired });
    }
    if verify.is_some() && kept_here(pool, name).await? {
        return Ok(Outcome::RefusedLocalPublish { retired: 0 });
    }

    let held: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id FROM worlds.world_scenes WHERE world_name = lower($1) AND mirror_source IS NOT NULL",
    )
    .bind(name)
    .fetch_all(pool)
    .await
    .with_context(|| format!("reading the scenes held for {name}"))?;
    if holds_every_scene(catalogue_scene_ids, &held) {
        // Every file, not only the entities: a read-through fill leaves all but the scene
        // host's to be read through, and the pass fetches them.
        let files: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT c->>'hash' FROM worlds.world_scenes s, \
               jsonb_array_elements(CASE WHEN jsonb_typeof(s.entity->'content') = 'array' \
                                         THEN s.entity->'content' ELSE '[]'::jsonb END) c \
             WHERE s.world_name = lower($1) AND s.mirror_source IS NOT NULL AND c->>'hash' IS NOT NULL",
        )
        .bind(name)
        .fetch_all(pool)
        .await
        .with_context(|| format!("reading the files of the scenes held for {name}"))?;
        let wanted: Vec<&str> = catalogue_scene_ids
            .iter()
            .chain(&files)
            .map(String::as_str)
            .collect();
        let present = storage.exist_multiple(&wanted).await?;
        if wanted
            .iter()
            .all(|hash| present.get(*hash).copied().unwrap_or(false))
        {
            // The scenes are current. The access setting and the world settings change upstream
            // without a deployment, and the listing carries what rows written before it was kept
            // (or while it gave none) lack: the deploy times, size, chain and deployer.
            // An unreadable access setting keeps the stored one and the rest of the sync.
            let access = upstream_access(client, source, name)
                .await
                .inspect_err(|e| {
                    tracing::warn!(world = %name, error = %e, "keeping the stored access setting");
                })
                .ok();
            let listed = listed_scenes(client, source, name).await?;
            let world_settings = upstream_settings(client, pool, storage, source, name).await;
            let world = WorldName::parse(name)?;
            let holder = name_holder(pool, owners, &world).await;
            let mut tx = pool.begin().await.context("begin tx")?;
            if let Some(access) = &access {
                replace_mirrored_access(&mut tx, &world, access, holder.as_ref())
                    .await
                    .with_context(|| format!("access setting of {name}"))?;
            }
            if let Some(copy) = &world_settings {
                apply_settings(&mut tx, &world, copy, holder.as_ref()).await?;
            }
            refresh_listed_scenes(&mut tx, name, &listed).await?;
            tx.commit().await.context("commit tx")?;
            return Ok(Outcome::Unchanged);
        }
    }

    let url = world_url(source, name, "about")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Ok(Outcome::NothingUpstream(
            "upstream /about did not answer 2xx",
        ));
    }
    let about: Value = serde_json::from_slice(&read_body_capped(resp, MAX_ABOUT_BYTES).await?)
        .with_context(|| format!("parsing {url}"))?;
    let Some((_, base)) = scene_refs(&about).first().cloned() else {
        return Ok(Outcome::NothingUpstream("upstream world has no scenes"));
    };
    // `/about` lists the newest scene only; `/scenes` lists them all, under the same `baseUrl`.
    // The listing decides what is retired below, so any doubt about it stops this world's sync.
    let listed = listed_scenes(client, source, name).await?;
    if listed.is_empty() {
        return Ok(Outcome::NothingUpstream("upstream world has no scenes"));
    }
    // A scene held here and missing from the listing is retired, with its storage identity. One
    // the source still lists by id means the listing moved under the pages (a redeploy shifts
    // every later row), so the world waits for the next pass instead.
    for cid in held
        .iter()
        .filter(|cid| !listed.iter().any(|scene| &scene.id == *cid))
    {
        if source_lists_scene(client, source, name, cid).await? {
            bail!("the scene listing of {name} changed while it was read ({cid} is still listed)");
        }
    }
    // The world's settings as the source stores them, not as its `/about` derives them from the
    // newest scene: a fixed hour or offline adapter there may be the scene's own.
    let world_settings = upstream_settings(client, pool, storage, source, name).await;
    // Without that answer a new row takes the fixed hour and offline adapter the source's
    // `/about` serves (its stored value whenever it has one, `worlds-manager.ts` `getWorldInfo`),
    // so this node's `/about` answers the same until a later pass reads the settings.
    let about_skybox_time = about["configurations"]["skybox"]["fixedHour"]
        .as_f64()
        .and_then(storable_skybox_time);
    let about_single_player =
        about["comms"]["adapter"].as_str() == Some("fixed-adapter:offline:offline");
    let upstream_spawn = about["spawnCoordinates"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(String::from);

    let signatures = match verify {
        Some(verify) => Some(Signatures::new(verify, settings, name)?),
        None => None,
    };
    let downloads = verify.map(|v| v.downloads);

    // Every scene entity fetched and, on a read-through, its signature and signer checked
    // before any file of the world is stored; the bytes checked are the bytes stored.
    let mut scenes = Vec::with_capacity(listed.len());
    for (index, listed) in listed.into_iter().enumerate() {
        let cid = listed.id.clone();
        if !settings.base_url_allowed(&base) {
            bail!("scene {cid} of {name} names a baseUrl this node will not fetch: {base}");
        }
        let signed = match &signatures {
            Some(signatures) => match signatures.signer(client, &listed).await {
                Ok(signed) => Some(signed),
                // The newest scene decides whether the world is filled. An older one this node
                // will not accept (a former owner's, a collaborator's) is left out of the fill,
                // and nothing of it is fetched. An answer that could not be read still fails it.
                Err(error) if index > 0 && error.downcast_ref::<Unacceptable>().is_some() => {
                    tracing::warn!(world = name, scene = %cid, error = %format!("{error:#}"),
                        "read-through leaves out an older scene whose chain it will not accept");
                    continue;
                }
                Err(error) => return Err(error),
            },
            None => None,
        };
        // The slot ends with this scene's checks: holding it to the store could wait on itself
        // across the scenes of one world.
        let (entity_bytes, fetched, _slot) =
            fetch_checked(client, storage, &base, &cid, downloads).await?;
        let entity: Value =
            serde_json::from_slice(&entity_bytes).with_context(|| format!("parse entity {cid}"))?;
        if let (Some(signatures), Some(signed)) = (&signatures, &signed) {
            signatures.check(&cid, &entity, signed).await?;
        }
        let file = if fetched {
            EntityFile::Held(entity_bytes)
        } else {
            EntityFile::Stored
        };
        scenes.push((listed, file, entity, signed));
    }
    // Asked once the scenes check, so a refused read-through asks nothing more.
    let access = upstream_access(client, source, name).await?;

    let mut new_blobs = 0usize;
    let mut scene_base_spawn: Option<String> = None;
    let mut records: Vec<SceneRecord> = Vec::new();
    let blob_sem = Arc::new(Semaphore::new(settings.blob_concurrency));
    for (listed, file, entity, signed) in scenes {
        let cid = listed.id.clone();
        match file {
            EntityFile::Stored => {}
            EntityFile::Held(bytes) => {
                storage.store(&cid, bytes).await?;
                new_blobs += 1;
            }
        }

        // A read-through stores only what the scene host reads from the blob store (the main
        // code file and main.crdt); every other file is read through, hash-checked, from the
        // source on its first `/contents/{hash}` (`content::read_through`), so a large world
        // answers its /about in seconds. A pass fetches every file of the worlds it syncs,
        // those a fill left to be read through included.
        let main = entity["metadata"]["main"].as_str().unwrap_or_default();
        let hashes: Vec<String> = entity["content"]
            .as_array()
            .map(|content| {
                content
                    .iter()
                    .filter(|c| {
                        signed.is_none()
                            || c["file"]
                                .as_str()
                                .is_some_and(|f| f == main || f == "main.crdt")
                    })
                    .filter_map(|c| c["hash"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut set = tokio::task::JoinSet::new();
        for hash in hashes {
            let permit = blob_sem.clone().acquire_owned().await?;
            let (client, base, storage) = (client.clone(), base.clone(), storage.clone());
            let downloads = downloads.cloned();
            set.spawn(async move {
                let _permit = permit;
                let (bytes, fetched) =
                    fetch_blob(&client, &storage, &base, &hash, downloads.as_ref()).await?;
                Ok::<_, anyhow::Error>((bytes.len() as i64, fetched))
            });
        }
        let mut size: i64 = 0;
        while let Some(joined) = set.join_next().await {
            let (len, fetched) = joined.context("blob fetch task")??;
            size += len;
            new_blobs += fetched as usize;
        }

        let parcels: Vec<String> = entity["pointers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if scene_base_spawn.is_none() {
            scene_base_spawn = entity["metadata"]["scene"]["base"]
                .as_str()
                .map(String::from);
        }
        // The deployment's own size, chain and signer as the source lists them; a pass keeps the
        // chain only when it signs this scene, a read-through the one it checked.
        let (chain, deployer) = match signed {
            Some(signed) => {
                size = signed.size;
                (signed.json, signed.signer.as_str().to_string())
            }
            None => {
                size = listed.size.unwrap_or(size);
                listed_signature(&listed, &entity)
                    .unwrap_or_else(|| (Value::Array(Vec::new()), ZERO_ADDRESS.to_string()))
            }
        };
        records.push(SceneRecord {
            cid,
            entity,
            parcels,
            size,
            chain,
            deployer,
            created_at: listed.created_at,
            updated_at: listed.updated_at,
        });
    }

    let world = WorldName::parse(name)?;
    let holder = name_holder(pool, owners, &world).await;
    let mut tx = pool.begin().await.context("begin tx")?;
    // A new row takes the source's settings (the `/about` fallback without them); a held one
    // keeps its settings here and takes the answer through `apply_settings` below.
    let copied = world_settings.unwrap_or_default();
    let (skybox_time, single_player) = match copied.present {
        true => (copied.skybox_time.flatten(), copied.single_player),
        false => (about_skybox_time, Some(about_single_player)),
    };
    let upserted = sqlx::query(
        r#"INSERT INTO worlds.worlds AS w
             (name, spawn_coordinates, title, description, content_rating, categories,
              thumbnail_hash, skybox_time, single_player, show_in_places, settings_version,
              blocked_since, mirror_source, updated_at)
           VALUES ($1, COALESCE($2, $3), $6, $7, $8, $9::text[], $10, $11, COALESCE($12, false),
                   COALESCE($13, true), COALESCE($14, 0), NULL, $4, timezone('UTC', now()))
           ON CONFLICT (name) DO UPDATE
             SET spawn_coordinates = COALESCE($2, w.spawn_coordinates, $3),
                 mirror_source = EXCLUDED.mirror_source,
                 updated_at = timezone('UTC', now())
             WHERE NOT $5
                OR (w.mirror_source IS NOT NULL AND w.blocked_since IS NULL)"#,
    )
    .bind(name)
    .bind(&upstream_spawn)
    .bind(&scene_base_spawn)
    .bind(source)
    .bind(verify.is_some())
    .bind(&copied.title)
    .bind(&copied.description)
    .bind(&copied.content_rating)
    .bind(&copied.categories)
    .bind(&copied.thumbnail_hash)
    .bind(skybox_time)
    .bind(single_player)
    .bind(copied.show_in_places)
    .bind(copied.settings_version)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("upsert world {name}"))?;
    if upserted.rows_affected() == 0 {
        // Kept here since the check above: the transaction rolls back on drop.
        return Ok(Outcome::RefusedLocalPublish { retired: 0 });
    }
    // The upsert holds the world row, which a local publish takes first
    // (core::store::deploy_scene), so this fresh read sees any publish that
    // landed during the download and none can land before the commit.
    let published_meanwhile: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM worlds.world_scenes WHERE world_name = lower($1) AND mirror_source IS NULL)",
    )
    .bind(name)
    .fetch_one(&mut *tx)
    .await
    .with_context(|| format!("re-checking local publish for {name}"))?;
    if published_meanwhile {
        tx.rollback().await.context("rollback tx")?;
        let retired = retire_beside_local(pool, name).await?;
        return Ok(Outcome::RefusedLocalPublish { retired });
    }
    replace_mirrored_access(&mut tx, &world, &access, holder.as_ref())
        .await
        .with_context(|| format!("access setting of {name}"))?;
    // A held row keeps its settings without an answer: the `/about` values mix the newest
    // scene's own with the stored ones, so writing them would flap with each failed read.
    if copied.present {
        apply_settings(&mut tx, &world, &copied, holder.as_ref()).await?;
    }

    let cids: Vec<&str> = records.iter().map(|r| r.cid.as_str()).collect();
    let entities: Vec<Value> = records.iter().map(|r| r.entity.clone()).collect();
    let parcels: Vec<Value> = records
        .iter()
        .map(|r| Value::from(r.parcels.clone()))
        .collect();
    let sizes: Vec<i64> = records.iter().map(|r| r.size).collect();
    let chains: Vec<Value> = records.iter().map(|r| r.chain.clone()).collect();
    let deployers: Vec<&str> = records.iter().map(|r| r.deployer.as_str()).collect();
    let created: Vec<Option<chrono::NaiveDateTime>> =
        records.iter().map(|r| r.created_at).collect();
    let updated: Vec<Option<chrono::NaiveDateTime>> =
        records.iter().map(|r| r.updated_at).collect();
    // A pass's unsigned row never replaces the chain and signer a read-through stored.
    sqlx::query(
        r#"INSERT INTO worlds.world_scenes
             (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size, mirror_source)
           SELECT $1, t.cid, t.chain::json, t.entity, t.deployer,
                  ARRAY(SELECT jsonb_array_elements_text(t.parcels)), t.size, $8
           FROM unnest($2::text[], $3::jsonb[], $4::jsonb[], $5::bigint[], $6::jsonb[], $7::text[])
                AS t(cid, entity, parcels, size, chain, deployer)
           ON CONFLICT (world_name, entity_id) DO UPDATE
             SET entity = EXCLUDED.entity, parcels = EXCLUDED.parcels, size = EXCLUDED.size,
                 deployment_auth_chain = CASE
                   WHEN json_array_length(EXCLUDED.deployment_auth_chain) > 0
                   THEN EXCLUDED.deployment_auth_chain
                   ELSE worlds.world_scenes.deployment_auth_chain END,
                 deployer = CASE
                   WHEN json_array_length(EXCLUDED.deployment_auth_chain) > 0
                   THEN EXCLUDED.deployer
                   ELSE worlds.world_scenes.deployer END,
                 mirror_source = EXCLUDED.mirror_source, updated_at = timezone('UTC', now())"#,
    )
    .bind(name)
    .bind(&cids)
    .bind(&entities)
    .bind(&parcels)
    .bind(&sizes)
    .bind(&chains)
    .bind(&deployers)
    .bind(source)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("upsert {} scenes of {name}", records.len()))?;
    // Each row keeps its deployment's times upstream (`/about` takes the newest, `/scenes` pages
    // by them, `/worlds` sorts by them); a row the listing gave none keeps this node's.
    sqlx::query(
        r#"UPDATE worlds.world_scenes s
           SET created_at = COALESCE(t.created, s.created_at),
               updated_at = COALESCE(t.updated, t.created, s.updated_at)
           FROM unnest($2::text[], $3::timestamp[], $4::timestamp[]) AS t(cid, created, updated)
           WHERE s.world_name = lower($1) AND s.entity_id = t.cid
             AND (t.created IS NOT NULL OR t.updated IS NOT NULL)"#,
    )
    .bind(name)
    .bind(&cids)
    .bind(&created)
    .bind(&updated)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("deploy times of the scenes of {name}"))?;

    sqlx::query(
        r#"DELETE FROM worlds.world_scenes
           WHERE world_name = lower($1) AND entity_id <> ALL($2::text[])
             AND mirror_source IS NOT NULL"#,
    )
    .bind(name)
    .bind(&cids)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("retire the scenes {name} no longer lists upstream"))?;
    let world = name.to_lowercase();
    // Apply the overlap rule in deployment order:
    // a replacement must inherit its predecessor before a later scene takes
    // a parcel that predecessor vacated. CID breaks timestamp ties stably.
    let mut chronological: Vec<_> = records.iter().collect();
    chronological.sort_by(|a, b| {
        let ts =
            |entity: &Value| crate::core::store::entity_timestamp_number(entity).unwrap_or(0.0);
        ts(&a.entity)
            .total_cmp(&ts(&b.entity))
            .then_with(|| a.cid.cmp(&b.cid))
    });
    for record in chronological {
        scene_identity::assign(&mut tx, &world, &record.cid, &record.parcels)
            .await
            .with_context(|| format!("storage identity of {} in {name}", record.cid))?;
    }
    scene_identity::retire_orphans(&mut tx, std::slice::from_ref(&world))
        .await
        .with_context(|| format!("retire the storage identities of {name}"))?;
    tx.commit().await.context("commit tx")?;

    Ok(Outcome::Synced {
        scenes: records.len(),
        new_blobs,
    })
}
