//! Worlds access persistence on the shared pool.
use super::*;

/// The deployment/streaming grants of a world with their scoping, in the
/// old `GET /permissions` order (the `access` grants render inside the
/// access setting, as the old document did).
pub async fn permission_records(
    pool: &PgPool,
    world: &WorldName,
) -> Result<Vec<PermissionRecord>, ApiError> {
    let rows = sqlx::query(
        r#"SELECT wp.permission, wp.address,
                  count(wpp.parcel) = 0 AS is_world_wide,
                  count(wpp.parcel) AS parcel_count
           FROM worlds.world_permissions wp
           LEFT JOIN worlds.world_permission_parcels wpp
             ON (wpp.world_name, wpp.permission, wpp.address) = (wp.world_name, wp.permission, wp.address)
           WHERE wp.world_name = $1 AND wp.permission IN ('deployment', 'streaming')
           GROUP BY wp.permission, wp.address
           ORDER BY wp.address, wp.permission"#,
    )
    .bind(world.key())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| PermissionRecord {
            permission: r.get("permission"),
            address: r.get("address"),
            is_world_wide: r.get("is_world_wide"),
            parcel_count: r.get("parcel_count"),
        })
        .collect())
}

/// `POST /permissions/{deployment|streaming}` in one statement: ensure the
/// worlds row (signer as owner), drop the addresses no longer listed, add the
/// new ones (existing rows keep their scoping). Returns `(added, removed)`.
pub async fn replace_world_wide_permission<'e, E>(
    executor: E,
    world: &WorldName,
    owner: &Address,
    permission: Permission,
    addresses: &[Address],
) -> Result<(Vec<String>, Vec<String>), ApiError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let rows: Vec<(bool, String)> = sqlx::query_as(
        r#"WITH ensured AS (
             INSERT INTO worlds.worlds (name, owner) VALUES ($1, $4) ON CONFLICT (name) DO NOTHING
           ), removed AS (
             DELETE FROM worlds.world_permissions
             WHERE world_name = $1 AND permission = $2 AND NOT (address = ANY($3::text[]))
             RETURNING address
           ), added AS (
             INSERT INTO worlds.world_permissions (world_name, permission, address)
             SELECT $1, $2, addr FROM unnest($3::text[]) AS addr
             ON CONFLICT (world_name, permission, address) DO NOTHING
             RETURNING address
           )
           SELECT true, address FROM added UNION ALL SELECT false, address FROM removed"#,
    )
    .bind(world.key())
    .bind(permission.as_str())
    .bind(addresses)
    .bind(owner)
    .fetch_all(executor)
    .await?;
    let (added, removed): (Vec<_>, Vec<_>) = rows.into_iter().partition(|(added, _)| *added);
    Ok((
        added.into_iter().map(|(_, a)| a).collect(),
        removed.into_iter().map(|(_, a)| a).collect(),
    ))
}

/// Grants world-wide `permission` to `addresses`: new rows are inserted and any
/// parcel scoping the addresses had is cleared. `ensure_owner` creates the
/// worlds row first. Returns the newly added addresses.
pub async fn grant_addresses_world_wide_permission<'e, E>(
    executor: E,
    world: &WorldName,
    permission: Permission,
    addresses: &[Address],
    ensure_owner: Option<&Address>,
) -> Result<Vec<String>, ApiError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    if addresses.is_empty() {
        return Ok(Vec::new());
    }
    let inserted = sqlx::query(
        r#"WITH ensured AS (
             INSERT INTO worlds.worlds (name, owner)
             SELECT $1, $4 WHERE $4::text IS NOT NULL
             ON CONFLICT (name) DO NOTHING
           ), added AS (
             INSERT INTO worlds.world_permissions (world_name, permission, address)
             SELECT $1, $2, addr FROM unnest($3::text[]) AS addr
             ON CONFLICT (world_name, permission, address) DO NOTHING
             RETURNING address
           ), cleared AS (
             DELETE FROM worlds.world_permission_parcels
             WHERE world_name = $1 AND permission = $2 AND address = ANY($3::text[])
           )
           SELECT address FROM added"#,
    )
    .bind(world.key())
    .bind(permission.as_str())
    .bind(addresses)
    .bind(ensure_owner)
    .fetch_all(executor)
    .await?;
    Ok(inserted.into_iter().map(|r| r.get("address")).collect())
}

pub async fn remove_addresses_permission<'e, E>(
    executor: E,
    world: &WorldName,
    permission: Permission,
    addresses: &[Address],
) -> Result<Vec<String>, ApiError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    if addresses.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "DELETE FROM worlds.world_permissions WHERE world_name = $1 AND permission = $2 AND address = ANY($3::text[]) RETURNING address",
    )
    .bind(world.key())
    .bind(permission.as_str())
    .bind(addresses)
    .fetch_all(executor)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("address")).collect())
}

/// Adds canonical parcels under a grant, creating it when absent; `true` when
/// the grant was created by this call.
pub async fn add_parcels_to_permission<'e, E>(
    executor: E,
    world: &WorldName,
    permission: Permission,
    address: &Address,
    parcels: &ParcelSet,
) -> Result<bool, ApiError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let created: bool = sqlx::query_scalar(
        r#"WITH perm AS (
             INSERT INTO worlds.world_permissions (world_name, permission, address)
             VALUES ($1, $2, $3)
             ON CONFLICT (world_name, permission, address) DO UPDATE SET updated_at = timezone('UTC', now())
             RETURNING (xmax = 0) AS created
           ), added AS (
             INSERT INTO worlds.world_permission_parcels (world_name, permission, address, parcel)
             SELECT $1, $2, $3, parcel FROM unnest($4::text[]) AS parcel
             ON CONFLICT DO NOTHING
           )
           SELECT created FROM perm"#,
    )
    .bind(world.key())
    .bind(permission.as_str())
    .bind(address)
    .bind(parcels.canonical())
    .fetch_one(executor)
    .await?;
    Ok(created)
}

/// `None` when the address holds no such permission (the caller's 400),
/// else whether the grant was dropped. Removing the last parcel drops the
/// grant: zero parcels would read as world-wide (deliberate divergence from
/// upstream, which keeps the row).
pub async fn remove_parcels_from_permission<'c, A>(
    conn: A,
    world: &WorldName,
    permission: Permission,
    address: &Address,
    parcels: &ParcelSet,
) -> Result<Option<bool>, ApiError>
where
    A: sqlx::Acquire<'c, Database = Postgres>,
{
    let mut tx = conn.begin().await?;
    let found = sqlx::query(
        "SELECT 1 FROM worlds.world_permissions
         WHERE world_name = $1 AND permission = $2 AND address = $3 FOR UPDATE",
    )
    .bind(world.key())
    .bind(permission.as_str())
    .bind(address)
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    if !found {
        return Ok(None);
    }
    let removed = sqlx::query(
        "DELETE FROM worlds.world_permission_parcels
         WHERE world_name = $1 AND permission = $2 AND address = $3 AND parcel = ANY($4::text[])",
    )
    .bind(world.key())
    .bind(permission.as_str())
    .bind(address)
    .bind(parcels.canonical())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let dropped = removed > 0
        && sqlx::query(
            "DELETE FROM worlds.world_permissions wp
             WHERE world_name = $1 AND permission = $2 AND address = $3
               AND NOT EXISTS (SELECT 1 FROM worlds.world_permission_parcels wpp
                 WHERE (wpp.world_name, wpp.permission, wpp.address) = (wp.world_name, wp.permission, wp.address))",
        )
        .bind(world.key())
        .bind(permission.as_str())
        .bind(address)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
    if !dropped {
        sqlx::query(
            "UPDATE worlds.world_permissions SET updated_at = timezone('UTC', now())
             WHERE world_name = $1 AND permission = $2 AND address = $3",
        )
        .bind(world.key())
        .bind(permission.as_str())
        .bind(address)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(dropped))
}

/// `None` when the address holds no such permission, otherwise the filtered
/// total and the page (bbox inclusive, corners in any order).
pub async fn parcels_for_permission(
    pool: &PgPool,
    world: &WorldName,
    permission: Permission,
    address: &Address,
    limit: i64,
    offset: i64,
    bbox: Option<(i32, i32, i32, i32)>,
) -> Result<Option<(i64, Vec<String>)>, ApiError> {
    let (has_bbox, min_x, max_x, min_y, max_y) = match bbox {
        Some((x1, y1, x2, y2)) => (true, x1.min(x2), x1.max(x2), y1.min(y2), y1.max(y2)),
        None => (false, 0, 0, 0, 0),
    };
    let rows = sqlx::query(
        r#"SELECT c.total, p.parcel
           FROM worlds.world_permissions wp
           CROSS JOIN LATERAL (
             SELECT count(*) AS total FROM worlds.world_permission_parcels wpp
             WHERE (wpp.world_name, wpp.permission, wpp.address) = (wp.world_name, wp.permission, wp.address)
               AND ($4::bool = false OR (
                  split_part(wpp.parcel, ',', 1)::int BETWEEN $5 AND $6
                  AND split_part(wpp.parcel, ',', 2)::int BETWEEN $7 AND $8))
           ) c
           LEFT JOIN LATERAL (
             SELECT wpp.parcel FROM worlds.world_permission_parcels wpp
             WHERE (wpp.world_name, wpp.permission, wpp.address) = (wp.world_name, wp.permission, wp.address)
               AND ($4::bool = false OR (
                  split_part(wpp.parcel, ',', 1)::int BETWEEN $5 AND $6
                  AND split_part(wpp.parcel, ',', 2)::int BETWEEN $7 AND $8))
             ORDER BY wpp.parcel
             LIMIT $9 OFFSET $10
           ) p ON true
           WHERE wp.world_name = $1 AND wp.permission = $2 AND wp.address = $3
           ORDER BY p.parcel"#,
    )
    .bind(world.key())
    .bind(permission.as_str())
    .bind(address)
    .bind(has_bbox)
    .bind(min_x)
    .bind(max_x)
    .bind(min_y)
    .bind(max_y)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let total: i64 = first.get("total");
    let parcels = rows
        .iter()
        .filter_map(|r| r.get::<Option<String>, _>("parcel"))
        .collect();
    Ok(Some((total, parcels)))
}

/// The grantees whose `permission` is world-wide or touches one of `parcels`.
pub async fn addresses_for_parcel_permission(
    pool: &PgPool,
    world: &WorldName,
    permission: Permission,
    parcels: &ParcelSet,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<String>), ApiError> {
    const WHERE: &str = r#"FROM worlds.world_permissions wp
           WHERE wp.world_name = $1 AND wp.permission = $2
             AND (NOT EXISTS (SELECT 1 FROM worlds.world_permission_parcels wpp
                              WHERE (wpp.world_name, wpp.permission, wpp.address) = (wp.world_name, wp.permission, wp.address))
                  OR EXISTS (SELECT 1 FROM worlds.world_permission_parcels wpp
                             WHERE (wpp.world_name, wpp.permission, wpp.address) = (wp.world_name, wp.permission, wp.address)
                               AND wpp.parcel = ANY($3::text[])))"#;
    let canon = parcels.canonical();
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT wp.address, count(*) OVER () AS total {WHERE} ORDER BY wp.address LIMIT $4 OFFSET $5"
    )))
    .bind(world.key())
    .bind(permission.as_str())
    .bind(&canon)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let total: i64 = match rows.first() {
        Some(r) => r.get("total"),
        None if offset > 0 => {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) {WHERE}")))
                .bind(world.key())
                .bind(permission.as_str())
                .bind(&canon)
                .fetch_one(pool)
                .await?
        }
        None => 0,
    };
    Ok((total, rows.into_iter().map(|r| r.get("address")).collect()))
}

/// The typed columns and the `access` lists of `access`: the secret hash,
/// the NFT urn, and an allow-list's wallets and community ids, each parsed
/// and deduplicated (an unparsable entry is dropped).
fn access_columns(access: &AccessSetting) -> (Option<&str>, Option<&str>, Vec<Address>, Vec<Uuid>) {
    match access {
        AccessSetting::Unrestricted => (None, None, Vec::new(), Vec::new()),
        AccessSetting::SharedSecret { secret_hash } => {
            (Some(secret_hash), None, Vec::new(), Vec::new())
        }
        AccessSetting::NftOwnership { nft } => (None, Some(nft), Vec::new(), Vec::new()),
        AccessSetting::AllowList {
            wallets,
            communities,
        } => {
            let mut seen = HashSet::new();
            let wallets = wallets
                .iter()
                .filter_map(|w| Address::parse(w).ok())
                .filter(|w| seen.insert(w.clone()))
                .collect();
            let mut seen = HashSet::new();
            let communities = communities
                .iter()
                .filter_map(|c| Uuid::parse_str(c.trim()).ok())
                .filter(|c| seen.insert(*c))
                .collect();
            (None, None, wallets, communities)
        }
    }
}

/// Ensures the worlds row and replaces the access setting in one
/// transaction: the typed columns, then the `access` grants and the
/// communities (deleted on every write, whatever the previous type, so a
/// list never resurfaces after a round trip through another type). A fresh
/// row lands at settings_version 1. The row is marked `access_set_here` by
/// `owner`, so the worlds mirror keeps the setting while `owner` holds the
/// name ([`mirror_keeps_local`]).
pub async fn store_access_for_owner(
    pool: &PgPool,
    world: &WorldName,
    owner: &Address,
    access: &AccessSetting,
) -> Result<(), ApiError> {
    let (secret_hash, nft, wallets, communities) = access_columns(access);
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"INSERT INTO worlds.worlds AS w (name, owner, access_type, access_secret_hash, access_nft, access_set_here, access_set_by, settings_version)
           VALUES ($1, $2, $3, $4, $5, true, $2, 1)
           ON CONFLICT (name) DO UPDATE SET
             access_type = EXCLUDED.access_type,
             access_secret_hash = EXCLUDED.access_secret_hash,
             access_nft = EXCLUDED.access_nft,
             access_set_here = true,
             access_set_by = EXCLUDED.access_set_by,
             settings_version = w.settings_version + 1,
             updated_at = timezone('UTC', now())"#,
    )
    .bind(world.key())
    .bind(owner)
    .bind(access.access_type().as_str())
    .bind(secret_hash)
    .bind(nft)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM worlds.world_permissions WHERE world_name = $1 AND permission = 'access'",
    )
    .bind(world.key())
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM worlds.world_access_communities WHERE world_name = $1")
        .bind(world.key())
        .execute(&mut *tx)
        .await?;
    if !wallets.is_empty() {
        sqlx::query(
            "INSERT INTO worlds.world_permissions (world_name, permission, address) SELECT $1, 'access', addr FROM unnest($2::text[]) AS addr",
        )
        .bind(world.key())
        .bind(&wallets)
        .execute(&mut *tx)
        .await?;
    }
    if !communities.is_empty() {
        sqlx::query("INSERT INTO worlds.world_access_communities (world_name, community_id) SELECT $1, c FROM unnest($2::uuid[]) AS c")
            .bind(world.key())
            .bind(&communities)
            .execute(&mut *tx)
            .await?;
    }
    queue_access_recheck(&mut tx, world).await?;
    tx.commit().await?;
    Ok(())
}

/// Whether the mirror leaves `which` of `world` as it is, in the transaction that syncs it, under
/// the row lock (taken here): `None` when the row is not the mirror's (`mirror_source` unset, or
/// disabled), so nothing is copied; `Some(true)` while the owner that set it on this node still
/// holds the name, as `holder` (this node's own owner lookup, never the source's `/permissions`
/// owner, which trails a sale) says. A row marked before its setter was recorded takes `holder`
/// as it. Once `holder` is another address (a NAME transfer), the mark is cleared and
/// `Some(false)` lets the source's setting in. Without a `holder` a marked setting is kept.
pub async fn mirror_keeps_local(
    conn: &mut sqlx::PgConnection,
    world: &WorldName,
    which: SetHere,
    holder: Option<&Address>,
) -> Result<Option<bool>, sqlx::Error> {
    let (here, by) = match which {
        SetHere::Access => ("access_set_here", "access_set_by"),
        SetHere::Settings => ("settings_set_here", "settings_set_by"),
    };
    let held: Option<(bool, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {here}, {by} FROM worlds.worlds \
         WHERE name = $1 AND mirror_source IS NOT NULL AND blocked_since IS NULL FOR UPDATE"
    )))
    .bind(world.key())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((set_here, set_by)) = held else {
        return Ok(None);
    };
    if !set_here {
        return Ok(Some(false));
    }
    let Some(holder) = holder else {
        return Ok(Some(true));
    };
    let holder = holder.as_str();
    match set_by.as_deref() {
        Some(setter) if setter.eq_ignore_ascii_case(holder) => Ok(Some(true)),
        None => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE worlds.worlds SET {by} = $2 WHERE name = $1"
            )))
            .bind(world.key())
            .bind(holder)
            .execute(&mut *conn)
            .await?;
            Ok(Some(true))
        }
        Some(setter) => {
            tracing::info!(%world, setter, holder, ?which,
                "the owner that set this here no longer holds the name; the source's setting applies");
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE worlds.worlds SET {here} = false, {by} = NULL WHERE name = $1"
            )))
            .bind(world.key())
            .execute(&mut *conn)
            .await?;
            Ok(Some(false))
        }
    }
}

/// The mirror's copy of the source's access setting, in the transaction that
/// syncs `world`: written only while the row is the mirror's and the setting
/// is not one its owner made here ([`mirror_keeps_local`], with the name's
/// `holder` as this node looks it up), and only where it differs, so an unchanged pass
/// writes nothing. A change bumps `settings_version` (the settings copy then
/// keeps it from going back) and queues the access recheck, as an owner's
/// edit does, so a connected wallet the new setting refuses leaves; no
/// notice. Returns whether anything changed.
pub async fn replace_mirrored_access(
    conn: &mut sqlx::PgConnection,
    world: &WorldName,
    access: &AccessSetting,
    holder: Option<&Address>,
) -> Result<bool, sqlx::Error> {
    if mirror_keeps_local(conn, world, SetHere::Access, holder).await? != Some(false) {
        return Ok(false);
    }
    let (secret_hash, nft, wallets, communities) = access_columns(access);
    let mut changed = sqlx::query(
        r#"UPDATE worlds.worlds
           SET access_type = $2, access_secret_hash = $3, access_nft = $4
           WHERE name = $1
             AND (access_type, access_secret_hash, access_nft)
                 IS DISTINCT FROM ($2::text, $3::text, $4::text)"#,
    )
    .bind(world.key())
    .bind(access.access_type().as_str())
    .bind(secret_hash)
    .bind(nft)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    changed += sqlx::query(
        "DELETE FROM worlds.world_permissions \
         WHERE world_name = $1 AND permission = 'access' AND address <> ALL($2::text[])",
    )
    .bind(world.key())
    .bind(&wallets)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    changed += sqlx::query(
        "INSERT INTO worlds.world_permissions (world_name, permission, address) \
         SELECT $1, 'access', addr FROM unnest($2::text[]) AS addr ON CONFLICT DO NOTHING",
    )
    .bind(world.key())
    .bind(&wallets)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    changed += sqlx::query(
        "DELETE FROM worlds.world_access_communities \
         WHERE world_name = $1 AND community_id <> ALL($2::uuid[])",
    )
    .bind(world.key())
    .bind(&communities)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    changed += sqlx::query(
        "INSERT INTO worlds.world_access_communities (world_name, community_id) \
         SELECT $1, c FROM unnest($2::uuid[]) AS c ON CONFLICT DO NOTHING",
    )
    .bind(world.key())
    .bind(&communities)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if changed > 0 {
        sqlx::query(
            "UPDATE worlds.worlds SET settings_version = settings_version + 1, \
             updated_at = timezone('UTC', now()) WHERE name = $1",
        )
        .bind(world.key())
        .execute(&mut *conn)
        .await?;
        queue_access_recheck(conn, world).await?;
    }
    Ok(changed > 0)
}

/// One allow-list edit under the world row's lock: refused unless the row
/// exists and is an allow-list, capped at `caps` wallets / communities; the
/// settings version bumps on every applied edit, as the old upsert did, and
/// the row is marked `access_set_here` by `owner` (see [`store_access_for_owner`]).
pub async fn modify_allow_list_access(
    pool: &PgPool,
    world: &WorldName,
    owner: &Address,
    caps: AccessCaps,
    edit: AllowListEdit<'_>,
) -> Result<AllowListEditOutcome, ApiError> {
    let mut tx = pool.begin().await?;
    let access_type: Option<String> =
        sqlx::query_scalar("SELECT access_type FROM worlds.worlds WHERE name = $1 FOR UPDATE")
            .bind(world.key())
            .fetch_optional(&mut *tx)
            .await?;
    if access_type.as_deref() != Some(AccessType::AllowList.as_str()) {
        return Ok(AllowListEditOutcome::NotAllowList);
    }
    match edit {
        AllowListEdit::AddWallet(wallet) => {
            let (present, count): (bool, i64) = sqlx::query_as(
                r#"SELECT EXISTS (SELECT 1 FROM worlds.world_permissions WHERE world_name = $1 AND permission = 'access' AND address = $2),
                          (SELECT count(*) FROM worlds.world_permissions WHERE world_name = $1 AND permission = 'access')"#,
            )
            .bind(world.key())
            .bind(wallet)
            .fetch_one(&mut *tx)
            .await?;
            if !present {
                if count >= caps.wallets {
                    return Ok(AllowListEditOutcome::CapExceeded);
                }
                sqlx::query("INSERT INTO worlds.world_permissions (world_name, permission, address) VALUES ($1, 'access', $2)")
                    .bind(world.key())
                    .bind(wallet)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        AllowListEdit::RemoveWallet(wallet) => {
            sqlx::query("DELETE FROM worlds.world_permissions WHERE world_name = $1 AND permission = 'access' AND address = $2")
                .bind(world.key())
                .bind(wallet)
                .execute(&mut *tx)
                .await?;
        }
        AllowListEdit::AddCommunity(id) => {
            let (present, count): (bool, i64) = sqlx::query_as(
                r#"SELECT EXISTS (SELECT 1 FROM worlds.world_access_communities WHERE world_name = $1 AND community_id = $2),
                          (SELECT count(*) FROM worlds.world_access_communities WHERE world_name = $1)"#,
            )
            .bind(world.key())
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if !present {
                if count >= caps.communities {
                    return Ok(AllowListEditOutcome::CapExceeded);
                }
                sqlx::query("INSERT INTO worlds.world_access_communities (world_name, community_id) VALUES ($1, $2)")
                    .bind(world.key())
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        AllowListEdit::RemoveCommunity(id) => {
            sqlx::query("DELETE FROM worlds.world_access_communities WHERE world_name = $1 AND community_id = $2")
                .bind(world.key())
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::query("UPDATE worlds.worlds SET settings_version = settings_version + 1, access_set_here = true, access_set_by = $2, updated_at = timezone('UTC', now()) WHERE name = $1")
        .bind(world.key())
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    queue_access_recheck(&mut tx, world).await?;
    tx.commit().await?;
    Ok(AllowListEditOutcome::Applied)
}

async fn queue_access_recheck(
    conn: &mut sqlx::PgConnection,
    world: &WorldName,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO social.world_access_rechecks(world_name,changed_at) VALUES($1,timezone('UTC',clock_timestamp())) ON CONFLICT(world_name) DO UPDATE SET revision=gen_random_uuid(),changed_at=timezone('UTC',clock_timestamp()),next_attempt_at=timezone('UTC',now())")
        .bind(world.key()).execute(conn).await?;
    Ok(())
}
