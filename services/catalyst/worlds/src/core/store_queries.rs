//! Worlds queries persistence on the shared pool.
use super::*;

fn world_from_row(r: &PgRow) -> WorldRecord {
    WorldRecord {
        name: r.get("name"),
        owner: r
            .get::<Option<String>, _>("owner")
            .and_then(|o| Address::parse(&o).ok()),
        access: AccessSetting::from_row(r),
        blocked_since: r.get("blocked_since"),
        spawn_coordinates: r.get("spawn_coordinates"),
        skybox_time: r.get("skybox_time"),
        single_player: r.get("single_player"),
        realm_name_override: r.get("realm_name_override"),
        preview_wearable_urns: r.get("preview_wearable_urns"),
    }
}

pub async fn get_world(pool: &PgPool, world: &WorldName) -> Result<Option<WorldRecord>, ApiError> {
    let row = sqlx::query(WORLD_SELECT)
        .bind(world.key())
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(world_from_row))
}

/// Whether upstream's `getRawWorldRecords` leaves `world` out, `name != ALL(getBannedNames())`:
/// a world named exactly like a `BLOCKED_WORLD_NAMES` entry (a whole name) or
/// like a list entry without its first .eth and .dcl (a bare name) is left out.
pub async fn raw_records_exclude(
    pool: &PgPool,
    names: &crate::core::names::NameDenyList,
    world: &WorldName,
) -> bool {
    let banned = names.snapshot(pool).await.banned_names();
    banned.iter().any(|n| n == world.key())
}

/// Upstream's `isWorldValid`: `checkNameDenyList` passes the name, the
/// `worlds` row exists, `getRawWorldRecords` keeps it
/// ([`raw_records_exclude`]), and it is neither disabled nor its owner
/// quota-blocked past the grace. A deployed scene is not required.
pub async fn world_is_valid(
    pool: &PgPool,
    policy: &crate::policy::Policy,
    names: &crate::core::names::NameDenyList,
    world: &WorldName,
) -> Result<bool, ApiError> {
    if !names.check_name_deny_list(pool, world.key()).await {
        return Ok(false);
    }
    let disabled: Option<Option<NaiveDateTime>> =
        sqlx::query_scalar("SELECT blocked_since FROM worlds.worlds WHERE name = $1")
            .bind(world.key())
            .fetch_optional(pool)
            .await?;
    let Some(disabled) = disabled else {
        return Ok(false);
    };
    if raw_records_exclude(pool, names, world).await {
        return Ok(false);
    }
    let quota = policy.quota_blocked_since(world).await?;
    let now = chrono::Utc::now().naive_utc();
    Ok(!crate::comms::access::world_blocked(disabled, quota, now))
}

pub async fn is_world_disabled(pool: &PgPool, world: &WorldName) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM worlds.worlds WHERE name = $1 AND blocked_since IS NOT NULL)",
    )
    .bind(world.key())
    .fetch_one(pool)
    .await?)
}

/// `(entity_id, parcels, effective base)` of every scene, by entity id.
pub async fn list_scenes(
    pool: &PgPool,
    world: &WorldName,
) -> Result<Vec<(String, Vec<String>, Option<String>)>, ApiError> {
    let rows = sqlx::query("SELECT entity_id, parcels, entity FROM worlds.world_scenes WHERE world_name = $1 ORDER BY entity_id")
        .bind(world.key())
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let parcels: Vec<String> = r.get("parcels");
            let entity: Value = r.get("entity");
            let base = effective_base_parcel(&entity, &parcels);
            (r.get("entity_id"), parcels, base)
        })
        .collect())
}

pub async fn scene_base_parcel(
    pool: &PgPool,
    world: &WorldName,
    scene_id: &str,
) -> Result<Option<String>, ApiError> {
    let row = sqlx::query(
        "SELECT entity, parcels FROM worlds.world_scenes WHERE world_name = $1 AND entity_id = $2",
    )
    .bind(world.key())
    .bind(scene_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| {
        let parcels: Vec<String> = r.get("parcels");
        let entity: Value = r.get("entity");
        effective_base_parcel(&entity, &parcels)
    }))
}

/// `POST /entities/active`: the newest scene of each named world as upstream's
/// `{...entity, id, metadata: {...metadata, owner}}` writes it (`worlds-manager.ts`
/// `getEntityForWorlds`): the stored entity's members in their jsonb order, `id`
/// and `metadata` replaced in place or appended, and `owner` (`null` when the row
/// has none) replaced in place in the metadata or appended last.
pub async fn entities_for_worlds(
    pool: &PgPool,
    worlds: &[WorldName],
) -> Result<Vec<Box<RawValue>>, ApiError> {
    if worlds.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<&str> = worlds.iter().map(WorldName::key).collect();
    let rows = sqlx::query(
        r#"SELECT DISTINCT ON (ws.world_name) ws.entity_id, ws.entity::text AS entity, w.owner
           FROM worlds.world_scenes ws
           JOIN worlds.worlds w ON w.name = ws.world_name
           WHERE ws.world_name = ANY($1::text[])
           ORDER BY ws.world_name, ws.created_at DESC"#,
    )
    .bind(&keys)
    .fetch_all(pool)
    .await?;
    Ok(active_entities(rows.into_iter().map(|r| {
        (
            r.get::<String, _>("entity_id"),
            r.get::<String, _>("entity"),
            r.get::<Option<String>, _>("owner"),
        )
    })))
}

/// `term` as an ILIKE substring pattern with its wildcards and the escape
/// character taken literally.
fn ilike_contains(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('%');
    for c in term.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Whether `pg_trgm` is installed, asked once per process like WCS's search
/// component; without it the search is full-text plus ILIKE only.
async fn trigram_available(pool: &PgPool) -> bool {
    static AVAILABLE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    // A failed lookup is not cached: the next search asks again.
    AVAILABLE
        .get_or_try_init(|| async {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
            )
            .fetch_one(pool)
            .await
        })
        .await
        .copied()
        .unwrap_or(false)
}

/// `GET /worlds`. The search is WCS's: a full-text match over name, title and
/// description, OR an escaped ILIKE substring of any of them, OR (when
/// `pg_trgm` is installed) trigram similarity above 0.3. No index: the OR'd
/// predicate scans the worlds table (~1.7k rows) either way.
pub async fn list_worlds_public(
    pool: &PgPool,
    filters: &WorldsListFilters,
    options: &WorldsListOptions,
) -> Result<(Vec<WorldInfoRow>, i64), ApiError> {
    let similarity = if filters.search.is_some() && trigram_available(pool).await {
        "OR similarity(w.name, $3) > 0.3
                OR similarity(COALESCE(w.title, ''), $3) > 0.3
                OR similarity(COALESCE(w.description, ''), $3) > 0.3"
    } else {
        ""
    };
    let blockable = crate::core::quota::quota_blockable_world!();
    let base_from = format!(
        r#"
        FROM worlds.worlds w
        LEFT JOIN (
            SELECT ws.world_name,
                   count(DISTINCT ws.entity_id) AS deployed_scenes,
                   max(ws.created_at) AS last_deployed_at,
                   min(CASE WHEN p ~ '^-?[0-9]+,-?[0-9]+$' THEN split_part(p, ',', 1)::int END) AS min_x,
                   max(CASE WHEN p ~ '^-?[0-9]+,-?[0-9]+$' THEN split_part(p, ',', 1)::int END) AS max_x,
                   min(CASE WHEN p ~ '^-?[0-9]+,-?[0-9]+$' THEN split_part(p, ',', 2)::int END) AS min_y,
                   max(CASE WHEN p ~ '^-?[0-9]+,-?[0-9]+$' THEN split_part(p, ',', 2)::int END) AS max_y
            FROM worlds.world_scenes ws, unnest(ws.parcels) AS p
            GROUP BY ws.world_name
        ) ss ON ss.world_name = w.name
        LEFT JOIN worlds.quota_blocks b ON w.owner = b.wallet AND {blockable}
        WHERE ($1::text IS NULL
                OR w.owner = $1
                OR EXISTS (SELECT 1 FROM worlds.world_permissions wp
                            WHERE wp.world_name = w.name AND wp.address = $1 AND wp.permission = 'deployment'))
          AND ($2::bool IS NULL OR (COALESCE(ss.deployed_scenes, 0) > 0) = $2)
          AND ($3::text IS NULL
                OR to_tsvector('english', w.name || ' ' || COALESCE(w.title, '') || ' ' || COALESCE(w.description, ''))
                   @@ plainto_tsquery('english', $3)
                OR w.name ILIKE $4
                OR w.title ILIKE $4
                OR w.description ILIKE $4
                {similarity})
          AND w.name <> ALL($5::text[])
    "#
    );
    let dir = match options.order_direction {
        OrderDirection::Desc => "DESC",
        OrderDirection::Asc => "ASC",
    };
    let order_clause = match options.order_by {
        WorldsOrderBy::LastDeployedAt => format!(
            "ORDER BY ss.last_deployed_at IS NULL ASC, ss.last_deployed_at {dir}, w.name ASC"
        ),
        WorldsOrderBy::Name => format!("ORDER BY w.name {dir}"),
    };
    let main_sql = format!(
        r#"SELECT w.name, w.owner, w.title, w.description, w.content_rating,
                  w.spawn_coordinates, w.skybox_time, w.categories, w.single_player, w.show_in_places,
                  w.thumbnail_hash, ss.last_deployed_at, ss.min_x, ss.max_x, ss.min_y, ss.max_y,
                  b.created_at AS blocked_since,
                  COALESCE(ss.deployed_scenes, 0) AS deployed_scenes,
                  count(*) OVER () AS total
           {base_from}
           {order_clause}
           LIMIT $6 OFFSET $7"#
    );
    let deployer = filters
        .authorized_deployer
        .as_deref()
        .map(str::to_lowercase);
    let pattern = filters.search.as_deref().map(ilike_contains);
    let rows = sqlx::query(sqlx::AssertSqlSafe(main_sql))
        .bind(&deployer)
        .bind(filters.has_deployed_scenes)
        .bind(&filters.search)
        .bind(&pattern)
        .bind(&filters.banned_names)
        .bind(options.limit)
        .bind(options.offset)
        .fetch_all(pool)
        .await?;
    let total: i64 = match rows.first() {
        Some(r) => r.get("total"),
        None if options.offset > 0 => {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) AS total {base_from}"
            )))
            .bind(&deployer)
            .bind(filters.has_deployed_scenes)
            .bind(&filters.search)
            .bind(&pattern)
            .bind(&filters.banned_names)
            .fetch_one(pool)
            .await?
        }
        None => 0,
    };
    let worlds = rows
        .into_iter()
        .map(|r| WorldInfoRow {
            name: r.get("name"),
            owner: r.get("owner"),
            title: r.get("title"),
            description: r.get("description"),
            content_rating: r.get("content_rating"),
            spawn_coordinates: r.get("spawn_coordinates"),
            skybox_time: r.get("skybox_time"),
            categories: r.get("categories"),
            single_player: r.get("single_player"),
            show_in_places: r.get("show_in_places"),
            thumbnail_hash: r.get("thumbnail_hash"),
            last_deployed_at: r.get("last_deployed_at"),
            min_x: r.get("min_x"),
            max_x: r.get("max_x"),
            min_y: r.get("min_y"),
            max_y: r.get("max_y"),
            blocked_since: r.get("blocked_since"),
            deployed_scenes: r.get("deployed_scenes"),
        })
        .collect();
    Ok((worlds, total))
}

/// `GET /admin/worlds`: page and total in one statement.
pub async fn admin_list_worlds_page(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<WorldAdminRow>, i64), ApiError> {
    let rows = sqlx::query(
        r#"SELECT w.name, w.owner, w.access_type, w.blocked_since, w.spawn_coordinates,
                  COALESCE(sc.scene_count, 0)::bigint AS scene_count,
                  count(*) OVER () AS total
           FROM worlds.worlds w
           LEFT JOIN (SELECT world_name, count(*) AS scene_count FROM worlds.world_scenes GROUP BY world_name) sc
             ON sc.world_name = w.name
           ORDER BY w.name
           LIMIT $1 OFFSET $2"#,
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let total: i64 = match rows.first() {
        Some(r) => r.get("total"),
        None if offset > 0 => {
            sqlx::query_scalar("SELECT count(*) FROM worlds.worlds")
                .fetch_one(pool)
                .await?
        }
        None => 0,
    };
    let worlds = rows
        .into_iter()
        .map(|r| WorldAdminRow {
            name: r.get("name"),
            owner: r.get("owner"),
            access_type: r.get("access_type"),
            blocked_since: r.get("blocked_since"),
            spawn_coordinates: r.get("spawn_coordinates"),
            scene_count: r.get("scene_count"),
        })
        .collect();
    Ok((worlds, total))
}

/// `(entity_id, parcels)` of a world's scenes, newest first.
pub async fn admin_scenes(
    pool: &PgPool,
    world: &WorldName,
) -> Result<Vec<(String, Vec<String>)>, ApiError> {
    let rows = sqlx::query("SELECT entity_id, parcels FROM worlds.world_scenes WHERE world_name = $1 ORDER BY created_at DESC")
        .bind(world.key())
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("entity_id"), r.get("parcels")))
        .collect())
}

pub async fn admin_set_world_blocked(
    pool: &PgPool,
    world: &WorldName,
    blocked: bool,
) -> Result<bool, ApiError> {
    let sql = if blocked {
        "UPDATE worlds.worlds SET blocked_since = timezone('UTC', now()), updated_at = timezone('UTC', now()) WHERE name = $1"
    } else {
        "UPDATE worlds.worlds SET blocked_since = NULL, updated_at = timezone('UTC', now()) WHERE name = $1"
    };
    let res = sqlx::query(sql).bind(world.key()).execute(pool).await?;
    Ok(res.rows_affected() > 0)
}

pub async fn admin_list_blocked(pool: &PgPool) -> Result<Vec<BlockedRow>, ApiError> {
    let rows = sqlx::query("SELECT wallet, created_at, updated_at FROM worlds.blocked_wallets ORDER BY created_at DESC").fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| BlockedRow {
            wallet: r.get("wallet"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        })
        .collect())
}

pub async fn admin_block_wallet(pool: &PgPool, wallet: &Address) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO worlds.blocked_wallets (wallet) VALUES ($1) ON CONFLICT (wallet) DO UPDATE SET updated_at = timezone('UTC', now())",
    )
    .bind(wallet)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn admin_unblock_wallet(pool: &PgPool, wallet: &Address) -> Result<bool, ApiError> {
    let res = sqlx::query("DELETE FROM worlds.blocked_wallets WHERE wallet = $1")
        .bind(wallet)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// The admin listing, one statement per filter arm so each walks its own
/// index (`world_access_log_{world,address,created}_idx`; a generic
/// `$1 IS NULL OR` plan cannot pick the arm).
pub async fn admin_query_access_log(
    pool: &PgPool,
    world: Option<&WorldName>,
    address: Option<&Address>,
    limit: i64,
    offset: i64,
) -> Result<Vec<AccessLogRow>, ApiError> {
    const COLUMNS: &str =
        "SELECT id, world_name, address, action, room, created_at FROM worlds.world_access_log";
    const ORDER: &str = "ORDER BY created_at DESC, id DESC LIMIT $1 OFFSET $2";
    let rows = match (world, address) {
        (Some(w), Some(a)) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "{COLUMNS} WHERE world_name = $3 AND address = $4 {ORDER}"
            )))
            .bind(limit)
            .bind(offset)
            .bind(w.key())
            .bind(a)
            .fetch_all(pool)
            .await?
        }
        (Some(w), None) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "{COLUMNS} WHERE world_name = $3 {ORDER}"
            )))
            .bind(limit)
            .bind(offset)
            .bind(w.key())
            .fetch_all(pool)
            .await?
        }
        (None, Some(a)) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "{COLUMNS} WHERE address = $3 {ORDER}"
            )))
            .bind(limit)
            .bind(offset)
            .bind(a)
            .fetch_all(pool)
            .await?
        }
        (None, None) => {
            sqlx::query(sqlx::AssertSqlSafe(format!("{COLUMNS} {ORDER}")))
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?
        }
    };
    Ok(rows
        .into_iter()
        .map(|r| AccessLogRow {
            id: r.get("id"),
            world_name: r.get("world_name"),
            address: r.get("address"),
            action: r.get("action"),
            room: r.get("room"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// One multi-row insert of queued access-log rows.
pub async fn insert_access_log(pool: &PgPool, rows: &[AccessLogEntry]) -> Result<(), sqlx::Error> {
    if rows.is_empty() {
        return Ok(());
    }
    let (mut worlds, mut addresses, mut actions, mut rooms) = (
        Vec::with_capacity(rows.len()),
        Vec::with_capacity(rows.len()),
        Vec::with_capacity(rows.len()),
        Vec::with_capacity(rows.len()),
    );
    for e in rows {
        worlds.push(e.world_name.as_str());
        addresses.push(e.address.as_str());
        actions.push(e.action);
        rooms.push(e.room.as_str());
    }
    sqlx::query(
        "INSERT INTO worlds.world_access_log (world_name, address, action, room) SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::text[])",
    )
    .bind(&worlds)
    .bind(&addresses)
    .bind(&actions)
    .bind(&rooms)
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) fn collect_content_hashes(
    entity_id: &str,
    content: Option<&Value>,
    out: &mut HashSet<String>,
) -> Result<(), String> {
    let items = match content {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(format!(
                "scene {entity_id} stores a non-array entity.content"
            ));
        }
    };
    for item in items {
        match item.get("hash").and_then(Value::as_str) {
            Some(hash) if !hash.is_empty() => {
                out.insert(hash.to_string());
            }
            _ => {
                return Err(format!(
                    "scene {entity_id} has a content entry without a usable hash"
                ));
            }
        }
    }
    Ok(())
}

/// Every blob key `worlds.*` references: each scene entity id (retired ones until evicted) and its content
/// hashes, and each world's thumbnail. A row this cannot read exactly aborts
/// the whole set (a caller deletes everything the set does not name), read in
/// one repeatable-read snapshot. The auth chain lives in the row, so there is
/// no `<entity>.auth` key any more.
pub async fn active_content_keys(pool: &PgPool) -> Result<HashSet<String>, ApiError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    // Retired scenes stay referenced until evicted.
    let scenes = sqlx::query(
        "SELECT entity_id, jsonb_typeof(entity) AS entity_type, entity -> 'content' AS content FROM worlds.world_scenes \
         UNION ALL SELECT entity_id, jsonb_typeof(entity), entity -> 'content' FROM worlds.retired_world_scenes",
    )
        .fetch_all(&mut *tx)
        .await?;
    let mut keys: HashSet<String> = HashSet::with_capacity(scenes.len() * 4);
    for row in &scenes {
        let entity_id: String = row.try_get("entity_id")?;
        let entity_type: Option<String> = row.try_get("entity_type")?;
        if entity_type.as_deref() != Some("object") {
            return Err(ApiError::internal(format!(
                "scene {entity_id} stores an entity that is not a JSON object"
            )));
        }
        let content: Option<Value> = row.try_get("content")?;
        collect_content_hashes(&entity_id, content.as_ref(), &mut keys)
            .map_err(ApiError::internal)?;
        keys.insert(entity_id);
    }
    let thumbnails =
        sqlx::query("SELECT thumbnail_hash FROM worlds.worlds WHERE thumbnail_hash IS NOT NULL")
            .fetch_all(&mut *tx)
            .await?;
    for row in &thumbnails {
        let hash: String = row.try_get("thumbnail_hash")?;
        if !hash.is_empty() {
            keys.insert(hash);
        }
    }
    tx.commit().await?;
    Ok(keys)
}
