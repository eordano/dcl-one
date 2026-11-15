//! Worlds settings persistence on the shared pool.
use super::*;

/// What a deployed scene expresses about its world's settings; a value out
/// of policy is "not expressed" rather than a deployment failure.
pub(crate) fn scene_settings_from_entity(entity: &Value) -> DerivedSceneSettings {
    let meta = entity.get("metadata");
    let display = meta.and_then(|m| m.get("display"));
    let wc = meta.and_then(|m| m.get("worldConfiguration"));
    let scene = meta.and_then(|m| m.get("scene"));

    let title = display
        .and_then(|d| d.get("title"))
        .and_then(|v| v.as_str())
        .filter(|t| (TITLE_MIN_LENGTH..=TITLE_MAX_LENGTH).contains(&text_len(t)))
        .map(str::to_string);
    let description = display
        .and_then(|d| d.get("description"))
        .and_then(|v| v.as_str())
        .filter(|d| (DESCRIPTION_MIN_LENGTH..=DESCRIPTION_MAX_LENGTH).contains(&text_len(d)))
        .map(str::to_string);
    let content_rating = meta
        .and_then(|m| m.get("rating"))
        .and_then(|v| v.as_str())
        .filter(|r| VALID_RATINGS.contains(r))
        .map(str::to_string);
    let skybox_time = wc
        .and_then(|c| c.get("skyboxConfig"))
        .and_then(|s| s.get("fixedTime"))
        .and_then(|v| v.as_f64())
        .and_then(storable_skybox_time);
    let categories = meta
        .and_then(|m| m.get("tags"))
        .and_then(|t| t.as_array())
        .filter(|arr| !arr.is_empty() && arr.len() <= MAX_CATEGORIES)
        .and_then(|arr| {
            arr.iter()
                .map(|t| t.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        });
    let single_player = wc
        .and_then(|c| c.get("fixedAdapter"))
        .map(|v| v.as_str() == Some("offline:offline"));
    let show_in_places = wc
        .and_then(|c| c.get("placesConfig"))
        .and_then(|p| p.get("optOut"))
        .map(|v| !js_truthy(v));
    let spawn_coordinates = scene
        .and_then(|s| s.get("base"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            scene
                .and_then(|s| s.get("parcels"))
                .and_then(|p| p.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let thumbnail_hash = display
        .and_then(|d| d.get("navmapThumbnail"))
        .and_then(|v| v.as_str())
        .and_then(|file| {
            entity
                .get("content")
                .and_then(|c| c.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|c| c.get("file").and_then(|f| f.as_str()) == Some(file))
                        .and_then(|c| c.get("hash").and_then(|h| h.as_str()))
                        .map(str::to_string)
                })
        });

    DerivedSceneSettings {
        spawn_coordinates,
        title,
        description,
        content_rating,
        skybox_time,
        categories,
        single_player,
        show_in_places,
        thumbnail_hash,
    }
}

fn settings_from_row(r: &PgRow) -> WorldSettingsRow {
    WorldSettingsRow {
        title: r.get("title"),
        description: r.get("description"),
        content_rating: r.get("content_rating"),
        spawn_coordinates: r.get("spawn_coordinates"),
        skybox_time: r.get("skybox_time"),
        categories: r.get("categories"),
        single_player: r.get("single_player"),
        show_in_places: r.get("show_in_places"),
        thumbnail_hash: r.get("thumbnail_hash"),
        access_type: r.get("access_type"),
        realm_name_override: r.get("realm_name_override"),
        preview_wearable_urns: r.get("preview_wearable_urns"),
        settings_version: r.get("settings_version"),
    }
}

pub async fn get_world_settings(
    pool: &PgPool,
    world: &WorldName,
) -> Result<Option<WorldSettingsRow>, ApiError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SETTINGS_COLUMNS} FROM worlds.worlds WHERE name = $1"
    )))
    .bind(world.key())
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(settings_from_row))
}

/// One locked transaction: the row (`FOR UPDATE`) plus, when a spawn is being
/// set, the world's bounding rectangle; the upsert, bumping the version only
/// when a versioned settings column is in the patch. A patch to a setting the
/// worlds mirror copies marks the row `settings_set_here` by `set_by`, the
/// name's holder (a collaborator's edit is the holder's too; `None` when the
/// holder is unknown, and the mirror then takes the holder of its next pass),
/// so the mirror keeps those settings while that holder holds the name
/// ([`mirror_keeps_local`]). Returns the row and the previous spawn.
pub async fn update_world_settings(
    pool: &PgPool,
    world: &WorldName,
    owner: &Address,
    set_by: Option<&Address>,
    input: &WorldSettingsUpdate,
) -> Result<(WorldSettingsRow, Option<String>), ApiError> {
    let mut tx = pool.begin().await?;
    let locked = sqlx::query(
        r#"SELECT w.spawn_coordinates, br.min_x, br.max_x, br.min_y, br.max_y
           FROM worlds.worlds w
           LEFT JOIN LATERAL (
             SELECT min(split_part(p, ',', 1)::int) AS min_x,
                    max(split_part(p, ',', 1)::int) AS max_x,
                    min(split_part(p, ',', 2)::int) AS min_y,
                    max(split_part(p, ',', 2)::int) AS max_y
             FROM worlds.world_scenes ws, unnest(ws.parcels) AS p
             WHERE $2::bool AND ws.world_name = w.name
           ) br ON true
           WHERE w.name = $1
           FOR UPDATE OF w"#,
    )
    .bind(world.key())
    .bind(input.spawn_coordinates.is_some())
    .fetch_optional(&mut *tx)
    .await?;
    let old_spawn: Option<String> = locked
        .as_ref()
        .and_then(|r| r.get::<Option<String>, _>("spawn_coordinates"));

    if let Some(spawn) = input.spawn_coordinates.as_deref() {
        let rect = locked.as_ref().and_then(|r| {
            let col = |c: &str| r.get::<Option<i32>, _>(c);
            Some((col("min_x")?, col("max_x")?, col("min_y")?, col("max_y")?))
        });
        let (min_x, max_x, min_y, max_y) = rect.ok_or_else(|| {
            ApiError::bad_request(format!(
                "Invalid spawnCoordinates \"{spawn}\". The world has no deployed scenes."
            ))
        })?;
        let within = Parcel::parse(spawn)
            .map(|p| (min_x..=max_x).contains(&p.x) && (min_y..=max_y).contains(&p.y))
            .unwrap_or(false);
        if !within {
            return Err(ApiError::bad_request(format!(
                "Invalid spawnCoordinates \"{spawn}\". It must be within the world shape rectangle: ({min_x},{min_y}) to ({max_x},{max_y})."
            )));
        }
    }

    let categories: Option<Vec<String>> = input
        .categories_provided
        .then(|| input.categories.clone().unwrap_or_default());
    let has_settings_patch = input.title.is_some()
        || input.description.is_some()
        || input.content_rating.is_some()
        || input.skybox_time_provided
        || input.categories_provided
        || input.single_player.is_some()
        || input.show_in_places.is_some()
        || input.thumbnail_hash.is_some()
        || input.realm_name_override_provided
        || input.preview_wearable_urns_provided;
    let mirrored_setting_patch = input.title.is_some()
        || input.description.is_some()
        || input.content_rating.is_some()
        || input.skybox_time_provided
        || input.categories_provided
        || input.single_player.is_some()
        || input.show_in_places.is_some()
        || input.thumbnail_hash.is_some();

    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"INSERT INTO worlds.worlds AS w (
               name, owner, title, description, content_rating, spawn_coordinates,
               skybox_time, categories, single_player, show_in_places, thumbnail_hash,
               realm_name_override, preview_wearable_urns, settings_set_here, settings_set_by
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text[], COALESCE($9, false), COALESCE($10, true), $11, $14, $16::text[],
                   $18, CASE WHEN $18 THEN $19 END)
           ON CONFLICT (name) DO UPDATE SET
             title = COALESCE(EXCLUDED.title, w.title),
             description = COALESCE(EXCLUDED.description, w.description),
             content_rating = COALESCE(EXCLUDED.content_rating, w.content_rating),
             spawn_coordinates = COALESCE(EXCLUDED.spawn_coordinates, w.spawn_coordinates),
             skybox_time = CASE WHEN $12::boolean THEN EXCLUDED.skybox_time
                                ELSE COALESCE(EXCLUDED.skybox_time, w.skybox_time) END,
             categories = COALESCE(EXCLUDED.categories, w.categories),
             single_player = COALESCE($9, w.single_player),
             show_in_places = COALESCE($10, w.show_in_places),
             thumbnail_hash = COALESCE(EXCLUDED.thumbnail_hash, w.thumbnail_hash),
             realm_name_override = CASE WHEN $15::boolean THEN EXCLUDED.realm_name_override
                                        ELSE w.realm_name_override END,
             preview_wearable_urns = CASE WHEN $17::boolean THEN EXCLUDED.preview_wearable_urns
                                          ELSE w.preview_wearable_urns END,
             settings_version = CASE WHEN $13::boolean THEN w.settings_version + 1
                                     ELSE w.settings_version END,
             settings_set_here = w.settings_set_here OR $18,
             settings_set_by = CASE WHEN $18 THEN EXCLUDED.settings_set_by ELSE w.settings_set_by END,
             updated_at = timezone('UTC', now())
           RETURNING {SETTINGS_COLUMNS}"#
    )))
    .bind(world.key())
    .bind(owner)
    .bind(&input.title)
    .bind(&input.description)
    .bind(&input.content_rating)
    .bind(&input.spawn_coordinates)
    .bind(input.skybox_time)
    .bind(&categories)
    .bind(input.single_player)
    .bind(input.show_in_places)
    .bind(&input.thumbnail_hash)
    .bind(input.skybox_time_provided)
    .bind(has_settings_patch)
    .bind(&input.realm_name_override)
    .bind(input.realm_name_override_provided)
    .bind(&input.preview_wearable_urns)
    .bind(input.preview_wearable_urns_provided)
    .bind(mirrored_setting_patch)
    .bind(set_by)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((settings_from_row(&row), old_spawn))
}

pub async fn world_manifest(
    pool: &PgPool,
    world: &WorldName,
) -> Result<Option<WorldManifest>, ApiError> {
    const PARCELS_LIMIT: i64 = 500;
    let (total, parcels, spawn_coordinates): (i64, Vec<String>, Option<String>) = sqlx::query_as(
        r#"WITH parcels AS MATERIALIZED (
               SELECT DISTINCT p AS parcel
               FROM worlds.world_scenes ws, unnest(ws.parcels) AS p
               WHERE ws.world_name = $1 AND p IS NOT NULL
           )
           SELECT (SELECT count(*) FROM parcels),
                  ARRAY(SELECT parcel FROM parcels
                        ORDER BY split_part(parcel, ',', 1)::int,
                                 split_part(parcel, ',', 2)::int LIMIT $2),
                  (SELECT spawn_coordinates FROM worlds.worlds WHERE name = $1 LIMIT 1)"#,
    )
    .bind(world.key())
    .bind(PARCELS_LIMIT)
    .fetch_one(pool)
    .await?;
    if total == 0 {
        return Ok(None);
    }
    Ok(Some(WorldManifest {
        parcels,
        spawn_coordinates,
        total,
    }))
}
