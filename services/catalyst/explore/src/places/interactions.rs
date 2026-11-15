//! A viewer's favorite and like of a place or a world: `PATCH
//! /api/{places,worlds}/{id}/{favorites,likes}` (upstream places
//! `UserFavorite/routes.ts`, `UserLikes/routes.ts`,
//! `World/routes/updateWorld{Favorites,Likes}.ts`) and `PUT`/`DELETE`
//! `/v1/destinations/{id}/{favorites,likes}`, signed fetch, behind
//! `DATA_WRITES_ENABLED`.
//!
//! The facts are `explore.user_favorites` and `explore.user_likes` (one row
//! per viewer and entity, local authority), each carrying the voter's
//! Snapshot voting power as `user_activity` ([`super::voting_power`],
//! upstream `fetchScore`). The counters the routes answer are upstream's
//! numbers (`explore.place.{favorites,likes,dislikes}`, the mirror's) plus
//! this node's own, kept in `explore.place_interaction_counts`: recounted
//! from the two fact tables in the fact's transaction under a lock on the
//! entity's counter row (so two writes on one entity serialise), as
//! upstream's `buildUpdate{Favorites,Likes}Query`
//! (`shared/entityInteractions.ts:111-160`) recount theirs, and rebuildable
//! from the facts ([`rebuild_counts`]).
//!
//! `like_rate` and `like_score` weigh only voters with `user_activity >=
//! 100` (`MIN_USER_ACTIVITY`), `like_score` by upstream's Wilson lower
//! bound. The mirror holds upstream's rates, not its voters, so the combined
//! rate is computed over upstream's qualified votes reconstructed from the
//! mirrored `likes`, `dislikes`, `like_rate` and `like_score`
//! ([`upstream_qualified_votes`]) plus this node's qualified votes; it is
//! stored on the counter row only once a local qualified vote exists (NULL
//! keeps the reads on the mirrored value). In that reconstruction a wallet
//! that voted upstream and here counts twice.
//!
//! The ground truth replaces the reconstruction: upstream's own `user_likes`
//! rows, imported from its database export ([`import_upstream_likes`],
//! `catalyst explore import-likes`) as facts with `source = 'upstream'`,
//! keyed like this node's by wallet and entity, so a wallet's later local
//! vote overrides its imported one (the vote's value, not its `source`,
//! which records where the row came from and never changes) and nobody
//! counts twice. The import settles each entity it covers: it records on
//! the counter row (`imported`, the rows the export held for the entity)
//! that the facts are the whole truth, and from then on the reads take the
//! entity's totals and rates from the fact table alone, even after every
//! imported voter withdraws; the mirrored aggregates no longer enter, and a
//! later import leaves a settled entity alone. The record is the import's,
//! outside the recounts and the rebuild.
//!
//! Lock order: a fact of entity E is written only under E's counter row
//! (`lock_counts`), and whoever takes several counter rows takes them in
//! entity id order, so the import and the votes wait on each other and
//! never deadlock.

use axum::Json;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
use std::sync::LazyLock;

use super::PlaceRow;
use super::voting_power::MIN_USER_ACTIVITY;
use crate::SharedState;
use crate::auth::normalize_eth_address;
use crate::fed;
use crate::shared::errors::{ApiData, ApiError};
use crate::validate::{self, Flavour, ObjectSchema, UPDATE_FAVORITES_BODY, UPDATE_LIKE_BODY};

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "places/"))]
pub struct FavoritesResult {
    pub favorites: i32,
    pub user_favorite: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "places/"))]
pub struct LikesResult {
    pub likes: i32,
    pub dislikes: i32,
    pub user_like: bool,
    pub user_dislike: bool,
}

/// The six counters `/v1/destinations/{id}/{favorites,likes}` answer.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "places/"))]
pub struct InteractionSummary {
    pub likes: i32,
    pub dislikes: i32,
    pub favorites: i32,
    pub user_like: bool,
    pub user_dislike: bool,
    pub user_favorite: bool,
}

impl From<&PlaceRow> for InteractionSummary {
    fn from(p: &PlaceRow) -> Self {
        Self {
            likes: p.likes,
            dislikes: p.dislikes,
            favorites: p.favorites,
            user_like: p.user_like,
            user_dislike: p.user_dislike,
            user_favorite: p.user_favorite,
        }
    }
}

/// places `World/schemas.ts:274-286`: the worlds route requires `like`
/// (`required: ["like"]`), unlike the places one (`UserLikes/schema.ts:14`,
/// `required: []`).
static UPDATE_WORLD_LIKE_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![("like", validate::boolean().nullable())])
        .required(&["like"])
        .closed()
});

/// A catalogue place id is a UUID; anything else names a world
/// (`shared/entityInteractions.ts:27-35`, `isPlaceId`).
pub fn is_place_id(s: &str) -> bool {
    s.len() == 36 && uuid::Uuid::parse_str(s).is_ok()
}

fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

// ── the rates ──────────────────────────────────────────────────────────────

/// Upstream's `like_score` (`shared/entityInteractions.ts:97-105`,
/// `calculateLikeScoreStatement`): the Wilson lower bound as that SQL
/// computes it, `(likes * dislikes) / (likes + dislikes)` included, which
/// divides two bigints and so truncates. `None` when nobody qualified voted.
pub fn like_score(likes: i64, dislikes: i64) -> Option<f64> {
    let n = likes + dislikes;
    if n <= 0 {
        return None;
    }
    let n_f = n as f64;
    let variance = ((likes * dislikes) / n) as f64 + 0.9604;
    Some(((likes as f64 + 1.9208) / n_f - 1.96 * variance.sqrt() / n_f) / (1.0 + 3.8416 / n_f))
}

/// Upstream's `like_rate` (`shared/entityInteractions.ts:153-155`).
pub fn like_rate(likes: i64, dislikes: i64) -> Option<f64> {
    let n = likes + dislikes;
    (n > 0).then(|| likes as f64 / n as f64)
}

/// What the mirror holds of upstream's votes on an entity.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MirroredVotes {
    pub likes: i64,
    pub dislikes: i64,
    pub like_rate: Option<f64>,
    pub like_score: Option<f64>,
}

/// Upstream's qualified (likes, dislikes): the unique pair whose count is
/// at most the mirrored total and whose rate and Wilson score are the
/// mirrored ones (a stored float8 round-trips through JSON exactly; the
/// score is matched to 1e-6, upstream's numeric arithmetic against f64).
/// No mirrored score means no qualified voter upstream. When the mirrored
/// values do not agree with any pair (a rounded or hand-edited row), the
/// total is split by the mirrored rate.
pub fn upstream_qualified_votes(m: &MirroredVotes) -> (i64, i64) {
    let (Some(rate), Some(score)) = (m.like_rate, m.like_score) else {
        return (0, 0);
    };
    let total = (m.likes + m.dislikes).max(0);
    for n in 1..=total {
        let likes = (rate * n as f64).round() as i64;
        if likes < 0 || likes > n {
            continue;
        }
        let dislikes = n - likes;
        let rate_matches = (likes as f64 / n as f64 - rate).abs() < 1e-9;
        let score_matches = like_score(likes, dislikes).is_some_and(|s| (s - score).abs() < 1e-6);
        if rate_matches && score_matches {
            return (likes, dislikes);
        }
    }
    let likes = (rate.clamp(0.0, 1.0) * total as f64).round() as i64;
    (likes, total - likes)
}

/// The votes on an entity in the fact tables, this node's own and the
/// imported ones together, and the import's record on the counter row
/// (`imported`, above zero once the import settled the entity).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalVotes {
    pub favorites: i64,
    pub likes: i64,
    pub dislikes: i64,
    pub qualified_likes: i64,
    pub qualified_dislikes: i64,
    pub imported: i64,
}

/// The rates the reads serve for an entity with votes in the fact table, by
/// upstream's formulas. For a settled entity the facts are the whole truth;
/// otherwise upstream's qualified votes reconstructed from the mirror plus
/// this node's, and `None` while no local vote qualifies, so the mirrored
/// value stays.
pub fn combined_rates(mirrored: &MirroredVotes, local: &LocalVotes) -> (Option<f64>, Option<f64>) {
    if local.imported > 0 {
        return (
            like_rate(local.qualified_likes, local.qualified_dislikes),
            like_score(local.qualified_likes, local.qualified_dislikes),
        );
    }
    if local.qualified_likes + local.qualified_dislikes == 0 {
        return (None, None);
    }
    let (up_likes, up_dislikes) = upstream_qualified_votes(mirrored);
    let likes = up_likes + local.qualified_likes;
    let dislikes = up_dislikes + local.qualified_dislikes;
    (like_rate(likes, dislikes), like_score(likes, dislikes))
}

async fn local_votes(tx: &mut PgConnection, entity_id: &str) -> Result<LocalVotes, ApiError> {
    let row = sqlx::query(
        r#"SELECT
             (SELECT count(*) FROM explore.user_favorites WHERE entity_id = $1) AS favorites,
             count(*) FILTER (WHERE "like") AS likes,
             count(*) FILTER (WHERE NOT "like") AS dislikes,
             count(*) FILTER (WHERE "like" AND user_activity >= $2) AS qualified_likes,
             count(*) FILTER (WHERE NOT "like" AND user_activity >= $2) AS qualified_dislikes,
             COALESCE((SELECT c.imported::bigint FROM explore.place_interaction_counts c WHERE c.entity_id = $1), 0) AS imported
           FROM explore.user_likes WHERE entity_id = $1"#,
    )
    .bind(entity_id)
    .bind(MIN_USER_ACTIVITY)
    .fetch_one(&mut *tx)
    .await?;
    Ok(LocalVotes {
        favorites: row.try_get("favorites")?,
        likes: row.try_get("likes")?,
        dislikes: row.try_get("dislikes")?,
        qualified_likes: row.try_get("qualified_likes")?,
        qualified_dislikes: row.try_get("qualified_dislikes")?,
        imported: row.try_get("imported")?,
    })
}

fn raw_float(v: Option<Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

async fn mirrored_votes(tx: &mut PgConnection, entity_id: &str) -> Result<MirroredVotes, ApiError> {
    let row = sqlx::query(
        "SELECT likes, dislikes, raw->'like_rate' AS like_rate, raw->'like_score' AS like_score \
         FROM explore.place WHERE id = $1",
    )
    .bind(entity_id)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(match row {
        None => MirroredVotes::default(),
        Some(r) => MirroredVotes {
            likes: i64::from(r.try_get::<i32, _>("likes")?),
            dislikes: i64::from(r.try_get::<i32, _>("dislikes")?),
            like_rate: raw_float(r.try_get::<Option<Value>, _>("like_rate")?),
            like_score: raw_float(r.try_get::<Option<Value>, _>("like_score")?),
        },
    })
}

/// Locks the entity's counter row for the rest of the transaction and
/// answers its `imported` (above zero: the import settled the entity).
async fn lock_counts(tx: &mut PgConnection, entity_id: &str) -> Result<i32, ApiError> {
    sqlx::query("INSERT INTO explore.place_interaction_counts (entity_id, updated_at) VALUES ($1, $2) ON CONFLICT (entity_id) DO NOTHING")
        .bind(entity_id)
        .bind(now())
        .execute(&mut *tx)
        .await?;
    Ok(sqlx::query_scalar(
        "SELECT imported FROM explore.place_interaction_counts WHERE entity_id = $1 FOR UPDATE",
    )
    .bind(entity_id)
    .fetch_one(&mut *tx)
    .await?)
}

/// Recounts the entity's counter row from the facts (upstream
/// `buildUpdateFavoritesQuery` / `buildUpdateLikesQuery`,
/// `shared/entityInteractions.ts:111-160`, over this node's facts), the
/// combined rates included; `imported` is the import's and stays. Under the
/// caller's lock.
async fn recount(tx: &mut PgConnection, entity_id: &str) -> Result<(), ApiError> {
    let local = local_votes(&mut *tx, entity_id).await?;
    let mirrored = mirrored_votes(&mut *tx, entity_id).await?;
    let (rate, score) = combined_rates(&mirrored, &local);
    sqlx::query(
        "UPDATE explore.place_interaction_counts \
         SET favorites = $2, likes = $3, dislikes = $4, like_rate = $5, like_score = $6, updated_at = $7 \
         WHERE entity_id = $1",
    )
    .bind(entity_id)
    .bind(local.favorites as i32)
    .bind(local.likes as i32)
    .bind(local.dislikes as i32)
    .bind(rate)
    .bind(score)
    .bind(now())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Rebuilds every counter row from the facts (the derived table's rebuild
/// rule): rows without facts go to zero and no rate, the rest are recounted,
/// in entity id order; `imported` is the import's record and stays.
pub async fn rebuild_counts(pool: &PgPool) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE explore.place_interaction_counts c \
         SET favorites = 0, likes = 0, dislikes = 0, like_rate = NULL, like_score = NULL, updated_at = $1 \
         WHERE NOT EXISTS (SELECT 1 FROM explore.user_favorites f WHERE f.entity_id = c.entity_id) \
           AND NOT EXISTS (SELECT 1 FROM explore.user_likes l WHERE l.entity_id = c.entity_id)",
    )
    .bind(now())
    .execute(&mut *tx)
    .await?;
    let entities: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id FROM explore.user_favorites UNION SELECT entity_id FROM explore.user_likes ORDER BY entity_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for entity_id in &entities {
        lock_counts(&mut tx, entity_id).await?;
        recount(&mut tx, entity_id).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// One row of upstream's `user_likes` export.
#[derive(Debug, serde::Deserialize)]
pub struct UpstreamLike {
    pub user: String,
    /// `place_id` in exports older than upstream's 2026-02 rename.
    #[serde(alias = "place_id")]
    pub entity_id: String,
    pub like: bool,
    #[serde(default)]
    pub user_activity: f64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// What [`import_upstream_likes`] did.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct LikesImport {
    pub read: u64,
    pub imported: u64,
    /// Rows a vote of this node's already covers (the wallet's local vote
    /// is the newer fact).
    pub kept_local: u64,
    /// Rows of an entity an earlier import settled; the import leaves them
    /// alone.
    pub settled: u64,
    /// Entities this import settled.
    pub entities: u64,
}

/// Imports upstream's `user_likes` rows (`psql "$OLD" -Atc "COPY (SELECT
/// row_to_json(l) FROM user_likes l) TO STDOUT"`, one JSON object a line)
/// as the ground truth of the like rates: each becomes a fact with `source
/// = 'upstream'` and its voting power at vote time, a wallet that already
/// voted here keeps its local fact, and every counter row is settled and
/// recounted. An entity an earlier import settled is left alone, so the
/// same export imports once, a vote a wallet withdrew here stays withdrawn,
/// and a fresher export adds nothing to it. One transaction an entity, in id
/// order, its counter row locked before its facts, so a vote on a settled
/// entity waits for that entity alone and a stopped import keeps what it
/// settled; a malformed line imports nothing and names itself.
pub async fn import_upstream_likes(
    pool: &PgPool,
    lines: impl Iterator<Item = std::io::Result<String>>,
) -> anyhow::Result<LikesImport> {
    use anyhow::Context;
    let mut out = LikesImport::default();
    let mut by_entity: std::collections::BTreeMap<String, Vec<UpstreamLike>> = Default::default();
    for (index, line) in lines.enumerate() {
        let line = line.with_context(|| format!("reading line {}", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let mut row: UpstreamLike = serde_json::from_str(&line)
            .with_context(|| format!("line {}: not a user_likes row", index + 1))?;
        if !is_place_id(&row.entity_id) && row.entity_id.trim().is_empty() {
            anyhow::bail!("line {}: empty entity_id", index + 1);
        }
        let Some(user) = normalize_eth_address(&row.user) else {
            anyhow::bail!("line {}: {:?} is not a wallet", index + 1, row.user);
        };
        out.read += 1;
        row.user = user;
        by_entity
            .entry(row.entity_id.clone())
            .or_default()
            .push(row);
    }
    for (entity_id, rows) in &by_entity {
        let mut tx = pool.begin().await?;
        if lock_counts(&mut tx, entity_id).await? > 0 {
            out.settled += rows.len() as u64;
            continue;
        }
        for row in rows {
            let inserted = sqlx::query(
                r#"INSERT INTO explore.user_likes ("user", entity_id, "like", user_activity, source, created_at, updated_at)
                   VALUES ($1, $2, $3, $4, 'upstream', $5, $6)
                   ON CONFLICT ("user", entity_id) DO NOTHING"#,
            )
            .bind(&row.user)
            .bind(entity_id)
            .bind(row.like)
            .bind(row.user_activity)
            .bind(row.created_at.naive_utc())
            .bind(row.updated_at.naive_utc())
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if inserted == 1 {
                out.imported += 1;
            } else {
                out.kept_local += 1;
            }
        }
        sqlx::query(
            "UPDATE explore.place_interaction_counts SET imported = GREATEST(imported, $2) WHERE entity_id = $1",
        )
        .bind(entity_id)
        .bind(rows.len() as i32)
        .execute(&mut *tx)
        .await?;
        recount(&mut tx, entity_id).await?;
        tx.commit().await?;
        out.entities += 1;
    }
    Ok(out)
}

// ── the facts ──────────────────────────────────────────────────────────────

/// Sets `user`'s favorite of `entity_id` (the catalogue row id), scored
/// `user_activity`. Idempotent.
pub async fn set_favorite(
    pool: &PgPool,
    entity_id: &str,
    user: &str,
    favorite: bool,
    user_activity: f64,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    set_favorite_in(&mut tx, entity_id, user, favorite, user_activity).await?;
    tx.commit().await?;
    Ok(())
}

/// [`set_favorite`] inside the caller's transaction.
pub async fn set_favorite_in(
    tx: &mut PgConnection,
    entity_id: &str,
    user: &str,
    favorite: bool,
    user_activity: f64,
) -> Result<(), ApiError> {
    let user = user.to_lowercase();
    lock_counts(&mut *tx, entity_id).await?;
    if favorite {
        // `UserFavoriteModel.create({entity_id, user, user_activity, created_at})`
        // (`UserFavorite/routes.ts:73-78`).
        sqlx::query(r#"INSERT INTO explore.user_favorites ("user", entity_id, user_activity, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT ("user", entity_id) DO NOTHING"#)
            .bind(&user)
            .bind(entity_id)
            .bind(user_activity)
            .bind(now())
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query(r#"DELETE FROM explore.user_favorites WHERE "user" = $1 AND entity_id = $2"#)
            .bind(&user)
            .bind(entity_id)
            .execute(&mut *tx)
            .await?;
    }
    recount(&mut *tx, entity_id).await
}

/// Sets `user`'s like (`Some(true)`), dislike (`Some(false)`) or neither
/// (`None`) of `entity_id`, scored `user_activity`. Idempotent.
pub async fn set_like(
    pool: &PgPool,
    entity_id: &str,
    user: &str,
    like: Option<bool>,
    user_activity: f64,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    set_like_in(&mut tx, entity_id, user, like, user_activity).await?;
    tx.commit().await?;
    Ok(())
}

/// [`set_like`] inside the caller's transaction.
pub async fn set_like_in(
    tx: &mut PgConnection,
    entity_id: &str,
    user: &str,
    like: Option<bool>,
    user_activity: f64,
) -> Result<(), ApiError> {
    let user = user.to_lowercase();
    lock_counts(&mut *tx, entity_id).await?;
    match like {
        // `UserLikesModel.like` (`UserLikes/model.ts:10-51`): a new row, or
        // the flipped one with the fresh score; an unchanged row stays. The
        // row's `source` records its origin and stays.
        Some(value) => {
            sqlx::query(
                r#"INSERT INTO explore.user_likes ("user", entity_id, "like", user_activity, source, created_at, updated_at)
                   VALUES ($1, $2, $3, $4, 'local', $5, $5)
                   ON CONFLICT ("user", entity_id) DO UPDATE
                   SET "like" = EXCLUDED."like", user_activity = EXCLUDED.user_activity, updated_at = EXCLUDED.updated_at
                   WHERE explore.user_likes."like" <> EXCLUDED."like""#,
            )
            .bind(&user)
            .bind(entity_id)
            .bind(value)
            .bind(user_activity)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        }
        // The counter row's `imported` stays, so a settled entity keeps
        // answering from the facts once this voter is gone from them.
        None => {
            sqlx::query(r#"DELETE FROM explore.user_likes WHERE "user" = $1 AND entity_id = $2"#)
                .bind(&user)
                .bind(entity_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    recount(&mut *tx, entity_id).await
}

// ── the lookups ────────────────────────────────────────────────────────────

/// `findEntityByIdWithAggregates` (`shared/entityInteractions.ts:43-52`): a
/// UUID is a place, anything else a world; the worlds routes look up a
/// world only.
pub async fn lookup_entity_for(
    state: &SharedState,
    entity_id: &str,
    is_world: bool,
    viewer: Option<&str>,
) -> Result<Option<PlaceRow>, ApiError> {
    if is_world || !is_place_id(entity_id) {
        // `World/model.ts:333` reads the worlds table: a world known only
        // through its scenes is a miss.
        return Ok(state
            .places
            .find_world_by_id_for(entity_id, viewer)
            .await?
            .filter(PlaceRow::is_world_catalogue_row));
    }
    state.places.find_by_id_for(entity_id, viewer).await
}

async fn resolve_entity(
    state: &SharedState,
    entity_id: &str,
    is_world: bool,
    viewer: &str,
) -> Result<PlaceRow, ApiError> {
    match lookup_entity_for(state, entity_id, is_world, Some(viewer)).await? {
        Some(entity) => Ok(entity),
        // `updateWorld*.ts:50-55` / `UserFavorite/routes.ts:48-53`.
        None if is_world => Err(ApiError::not_found(format!(
            "Not found world \"{entity_id}\""
        ))),
        None => Err(ApiError::not_found(format!(
            "Not found entity \"{entity_id}\""
        ))),
    }
}

/// Re-reads the entity after a write, so the answer is what the next read shows.
async fn reread(
    state: &SharedState,
    entity: &PlaceRow,
    viewer: &str,
) -> Result<PlaceRow, ApiError> {
    Ok(lookup_entity_for(state, &entity.id, false, Some(viewer))
        .await?
        .unwrap_or_else(|| entity.clone()))
}

fn current_like(row: &PlaceRow) -> Option<bool> {
    if row.user_like {
        Some(true)
    } else if row.user_dislike {
        Some(false)
    } else {
        None
    }
}

// ── the wkc routes ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Places,
    Worlds,
}

/// The body upstream validated: express's body-parser hands the route `{}`
/// for no body or a non-JSON one (`Route/middleware/withBody.js:9-14`).
fn body_object(body: Option<Json<Value>>) -> Value {
    body.map(|Json(v)| v).unwrap_or_else(|| json!({}))
}

/// `body.like` as the route saw it: absent (`undefined`), `null`, or a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LikeBody {
    Undefined,
    Null,
    Value(bool),
}

fn like_body(body: &Value) -> LikeBody {
    match body.get("like") {
        None => LikeBody::Undefined,
        Some(Value::Null) => LikeBody::Null,
        Some(v) => LikeBody::Value(v.as_bool().unwrap_or(true)),
    }
}

fn refuse_place_uuid_as_world(world_id: &str, route: &str) -> Result<(), ApiError> {
    if is_place_id(world_id) {
        // `updateWorldFavorites.ts:38-43` / `updateWorldLikes.ts:41-46`.
        return Err(ApiError::bad_request(format!(
            "Invalid world ID \"{world_id}\". Use /places/:entity_id/{route} for place entities."
        )));
    }
    Ok(())
}

/// `updateFavorites` (`UserFavorite/routes.ts:36-93`) and
/// `updateWorldFavorites` (`updateWorldFavorites.ts:31-91`): body, auth,
/// (the worlds route's UUID refusal,) entity, the signer's voting power, the
/// no-op short-circuit, the fact, the recount; answered as the entity read
/// before the write plus or minus one.
async fn patch_favorites(
    state: SharedState,
    method: Method,
    path: &str,
    headers: HeaderMap,
    entity_id: String,
    body: Option<Json<Value>>,
    route: Route,
) -> Result<Json<ApiData<FavoritesResult>>, ApiError> {
    let body = body_object(body);
    validate::validate(&UPDATE_FAVORITES_BODY, &body, Flavour::Wkc)?;
    let user = state.wkc_signer(&headers, method.as_str(), path).await?;
    state.user_writes()?;
    let is_world = route == Route::Worlds;
    if is_world {
        refuse_place_uuid_as_world(&entity_id, "favorites")?;
    }
    let entity = resolve_entity(&state, &entity_id, is_world, &user).await?;
    // `fetchScore` runs before the short-circuit (`routes.ts:55-65`).
    let user_activity = state.voting_power.fetch_score(&user).await;
    let wanted = body["favorites"].as_bool().unwrap_or(false);
    if wanted == entity.user_favorite {
        return Ok(Json(ApiData::ok(FavoritesResult {
            favorites: entity.favorites,
            user_favorite: entity.user_favorite,
        })));
    }
    set_favorite(&state.pool, &entity.id, &user, wanted, user_activity).await?;
    Ok(Json(ApiData::ok(FavoritesResult {
        favorites: if wanted {
            entity.favorites + 1
        } else {
            entity.favorites - 1
        },
        user_favorite: wanted,
    })))
}

/// `updateLike` (`UserLikes/routes.ts:38-102`) and `updateWorldLikes`
/// (`updateWorldLikes.ts:34-99`): body, auth, (UUID refusal,) entity, the
/// stored vote's short-circuit, delete on `null` else the signer's voting
/// power and the upsert (an absent `like` stores `true`,
/// `UserLikes/model.ts:16`), the recount, the re-read.
async fn patch_likes(
    state: SharedState,
    method: Method,
    path: &str,
    headers: HeaderMap,
    entity_id: String,
    body: Option<Json<Value>>,
    route: Route,
) -> Result<Json<ApiData<LikesResult>>, ApiError> {
    let body = body_object(body);
    let schema = match route {
        Route::Places => &*UPDATE_LIKE_BODY,
        Route::Worlds => &*UPDATE_WORLD_LIKE_BODY,
    };
    validate::validate(schema, &body, Flavour::Wkc)?;
    let user = state.wkc_signer(&headers, method.as_str(), path).await?;
    state.user_writes()?;
    let is_world = route == Route::Worlds;
    if is_world {
        refuse_place_uuid_as_world(&entity_id, "likes")?;
    }
    let entity = resolve_entity(&state, &entity_id, is_world, &user).await?;
    let wanted = like_body(&body);
    // `userLike && userLike.like === body.like` (`routes.ts:65-72`): only a
    // boolean can equal the stored vote.
    if let LikeBody::Value(value) = wanted
        && current_like(&entity) == Some(value)
    {
        return Ok(Json(ApiData::ok(LikesResult {
            likes: entity.likes,
            dislikes: entity.dislikes,
            user_like: entity.user_like,
            user_dislike: entity.user_dislike,
        })));
    }
    match wanted {
        LikeBody::Null => set_like(&state.pool, &entity.id, &user, None, 0.0).await?,
        LikeBody::Undefined | LikeBody::Value(_) => {
            let user_activity = state.voting_power.fetch_score(&user).await;
            let like = match wanted {
                LikeBody::Value(v) => v,
                _ => true,
            };
            set_like(&state.pool, &entity.id, &user, Some(like), user_activity).await?;
        }
    }
    let row = reread(&state, &entity, &user).await?;
    Ok(Json(ApiData::ok(LikesResult {
        likes: row.likes,
        dislikes: row.dislikes,
        user_like: row.user_like,
        user_dislike: row.user_dislike,
    })))
}

fn envelope(body: &Option<Json<Value>>) -> Option<Value> {
    body.as_ref()
        .map(|Json(v)| v)
        .filter(|v| catalyst_fed::is_envelope(v))
        .cloned()
}

/// A federation envelope's favorite (`PlaceFavorite`): the action logged
/// and applied in one transaction, answered with its digest.
async fn fed_favorites(
    state: &SharedState,
    headers: &HeaderMap,
    entity_id: &str,
    body: Value,
) -> Result<Response, ApiError> {
    state.user_writes()?;
    let (signed, signer) =
        fed::preflight::<fed::PlaceFavorite>(state, headers, body, fed::Scope::Places).await?;
    if signed.message.place_id != entity_id {
        return Err(ApiError::bad_request(
            "place_id in body does not match path",
        ));
    }
    let place = state
        .places
        .find_by_id_for(entity_id, Some(&signer))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Not found place \"{entity_id}\"")))?;
    let favorite = signed.message.action == fed::PlaceFavoriteAction::Add;
    let user_activity = state.voting_power.fetch_score(&signer).await;
    let hash = signed.signature_hash();
    let payload = json!({ "place_id": signed.message.place_id, "action": if favorite { "add" } else { "remove" }, "signed_at": signed.message.signed_at });
    let mut tx = state.pool.begin().await?;
    fed::record_action(
        &mut tx,
        &fed::Action {
            scope: fed::Scope::Places,
            signature_hash: &hash,
            signer: &signer,
            subject_id: Some(entity_id),
            action_type: if favorite { "favorite" } else { "unfavorite" },
            payload: &payload,
            signed_at: signed.signed_at,
        },
    )
    .await?;
    set_favorite_in(&mut tx, &place.id, &signer, favorite, user_activity).await?;
    tx.commit().await?;
    let row = reread(state, &place, &signer).await?;
    Ok(Json(fed::SignedApiData::ok(
        hash,
        FavoritesResult {
            favorites: row.favorites,
            user_favorite: row.user_favorite,
        },
    ))
    .into_response())
}

/// A federation envelope's vote (`PlaceVote`: a positive score likes, a
/// negative one dislikes, zero withdraws).
async fn fed_likes(
    state: &SharedState,
    headers: &HeaderMap,
    entity_id: &str,
    body: Value,
) -> Result<Response, ApiError> {
    state.user_writes()?;
    let (signed, signer) =
        fed::preflight::<fed::PlaceVote>(state, headers, body, fed::Scope::Places).await?;
    if signed.message.place_id != entity_id {
        return Err(ApiError::bad_request(
            "place_id in body does not match path",
        ));
    }
    let place = state
        .places
        .find_by_id_for(entity_id, Some(&signer))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Not found place \"{entity_id}\"")))?;
    let like = match signed.message.score {
        s if s > 0 => Some(true),
        s if s < 0 => Some(false),
        _ => None,
    };
    let action_type = match like {
        Some(true) => "vote_up",
        Some(false) => "vote_down",
        None => "vote_withdraw",
    };
    let user_activity = if like.is_some() {
        state.voting_power.fetch_score(&signer).await
    } else {
        0.0
    };
    let hash = signed.signature_hash();
    let payload = json!({ "place_id": signed.message.place_id, "score": signed.message.score, "signed_at": signed.message.signed_at });
    let mut tx = state.pool.begin().await?;
    fed::record_action(
        &mut tx,
        &fed::Action {
            scope: fed::Scope::Places,
            signature_hash: &hash,
            signer: &signer,
            subject_id: Some(entity_id),
            action_type,
            payload: &payload,
            signed_at: signed.signed_at,
        },
    )
    .await?;
    set_like_in(&mut tx, &place.id, &signer, like, user_activity).await?;
    tx.commit().await?;
    let row = reread(state, &place, &signer).await?;
    Ok(Json(fed::SignedApiData::ok(
        hash,
        LikesResult {
            likes: row.likes,
            dislikes: row.dislikes,
            user_like: row.user_like,
            user_dislike: row.user_dislike,
        },
    ))
    .into_response())
}

/// `PATCH /api/places/{entity_id}/favorites` `{ favorites: boolean }`, or a `PlaceFavorite` envelope.
pub async fn patch_place_favorites(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(entity_id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Response, ApiError> {
    if let Some(env) = envelope(&body) {
        return fed_favorites(&state, &headers, &entity_id, env).await;
    }
    Ok(patch_favorites(
        state,
        method,
        uri.path(),
        headers,
        entity_id,
        body,
        Route::Places,
    )
    .await?
    .into_response())
}

/// `PATCH /api/places/{entity_id}/likes` `{ like?: boolean|null }`, or a `PlaceVote` envelope.
pub async fn patch_place_likes(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(entity_id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Response, ApiError> {
    if let Some(env) = envelope(&body) {
        return fed_likes(&state, &headers, &entity_id, env).await;
    }
    Ok(patch_likes(
        state,
        method,
        uri.path(),
        headers,
        entity_id,
        body,
        Route::Places,
    )
    .await?
    .into_response())
}

/// `PATCH /api/worlds/{world_id}/favorites`: a place UUID is refused after
/// auth (envelopes pass).
pub async fn patch_world_favorites(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(world_id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Response, ApiError> {
    if let Some(env) = envelope(&body) {
        return fed_favorites(&state, &headers, &world_id, env).await;
    }
    Ok(patch_favorites(
        state,
        method,
        uri.path(),
        headers,
        world_id,
        body,
        Route::Worlds,
    )
    .await?
    .into_response())
}

/// `PATCH /api/worlds/{world_id}/likes` `{ like: boolean|null }`.
pub async fn patch_world_likes(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(world_id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Response, ApiError> {
    if let Some(env) = envelope(&body) {
        return fed_likes(&state, &headers, &world_id, env).await;
    }
    Ok(patch_likes(
        state,
        method,
        uri.path(),
        headers,
        world_id,
        body,
        Route::Worlds,
    )
    .await?
    .into_response())
}

// ── the v1 routes (dcl-one only) ───────────────────────────────────────────

async fn resolve_destination_entity(
    state: &SharedState,
    entity_id: &str,
    user: &str,
) -> Result<PlaceRow, ApiError> {
    let is_world = !is_place_id(entity_id);
    let lookup = if is_world {
        entity_id.to_string()
    } else {
        entity_id.to_lowercase()
    };
    resolve_entity(state, &lookup, is_world, user).await
}

async fn destination_favorite(
    state: &SharedState,
    entity_id: &str,
    user: &str,
    favorite: bool,
) -> Result<InteractionSummary, ApiError> {
    let entity = resolve_destination_entity(state, entity_id, user).await?;
    if favorite == entity.user_favorite {
        return Ok(InteractionSummary::from(&entity));
    }
    let user_activity = state.voting_power.fetch_score(user).await;
    set_favorite(&state.pool, &entity.id, user, favorite, user_activity).await?;
    Ok(InteractionSummary::from(
        &reread(state, &entity, user).await?,
    ))
}

async fn destination_like(
    state: &SharedState,
    entity_id: &str,
    user: &str,
    like: Option<bool>,
) -> Result<InteractionSummary, ApiError> {
    let entity = resolve_destination_entity(state, entity_id, user).await?;
    if current_like(&entity) == like {
        return Ok(InteractionSummary::from(&entity));
    }
    let user_activity = if like.is_some() {
        state.voting_power.fetch_score(user).await
    } else {
        0.0
    };
    set_like(&state.pool, &entity.id, user, like, user_activity).await?;
    Ok(InteractionSummary::from(
        &reread(state, &entity, user).await?,
    ))
}

/// `{ like: boolean }` and nothing else.
pub fn parse_like_body(body: Option<&Value>) -> Result<bool, ApiError> {
    let invalid = || ApiError::bad_request("Invalid likes body. Expected { like: boolean }.");
    let object = body.and_then(Value::as_object).ok_or_else(invalid)?;
    if object.len() != 1 {
        return Err(invalid());
    }
    object
        .get("like")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)
}

async fn v1_actor(
    state: &SharedState,
    headers: &HeaderMap,
    method: &Method,
    path: &str,
) -> Result<String, ApiError> {
    state.user_writes()?;
    state.signer(headers, method.as_str(), path).await
}

pub async fn put_v1_destination_favorites(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ApiData<InteractionSummary>>, ApiError> {
    let user = v1_actor(&state, &headers, &method, uri.path()).await?;
    Ok(Json(ApiData::ok(
        destination_favorite(&state, &id, &user, true).await?,
    )))
}

pub async fn delete_v1_destination_favorites(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ApiData<InteractionSummary>>, ApiError> {
    let user = v1_actor(&state, &headers, &method, uri.path()).await?;
    Ok(Json(ApiData::ok(
        destination_favorite(&state, &id, &user, false).await?,
    )))
}

pub async fn put_v1_destination_likes(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<ApiData<InteractionSummary>>, ApiError> {
    let user = v1_actor(&state, &headers, &method, uri.path()).await?;
    let like = parse_like_body(body.as_ref().map(|Json(v)| v))?;
    Ok(Json(ApiData::ok(
        destination_like(&state, &id, &user, Some(like)).await?,
    )))
}

pub async fn delete_v1_destination_likes(
    State(state): State<SharedState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ApiData<InteractionSummary>>, ApiError> {
    let user = v1_actor(&state, &headers, &method, uri.path()).await?;
    Ok(Json(ApiData::ok(
        destination_like(&state, &id, &user, None).await?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn like_body_accepts_exactly_a_boolean_like() {
        assert!(parse_like_body(Some(&json!({ "like": true }))).unwrap());
        assert!(!parse_like_body(Some(&json!({ "like": false }))).unwrap());
    }

    #[test]
    fn like_body_rejects_everything_else() {
        assert!(parse_like_body(None).is_err());
        assert!(parse_like_body(Some(&json!({}))).is_err());
        assert!(parse_like_body(Some(&json!({ "like": "yes" }))).is_err());
        assert!(parse_like_body(Some(&json!({ "like": null }))).is_err());
        assert!(parse_like_body(Some(&json!({ "like": true, "extra": 1 }))).is_err());
        assert!(parse_like_body(Some(&json!([true]))).is_err());
    }

    #[test]
    fn place_uuid_guard() {
        assert!(is_place_id("123e4567-e89b-12d3-a456-426614174000"));
        assert!(!is_place_id("my-world.dcl.eth"));
        assert!(!is_place_id("123e4567e89b12d3a456426614174000"));
        assert!(!is_place_id("123e4567-e89b-12d3-a456-42661417400g"));
        assert!(!is_place_id(""));
    }

    #[test]
    fn the_wkc_bodies_read_like_as_the_route_did() {
        assert_eq!(like_body(&json!({})), LikeBody::Undefined);
        assert_eq!(like_body(&json!({ "like": null })), LikeBody::Null);
        assert_eq!(like_body(&json!({ "like": false })), LikeBody::Value(false));
        assert_eq!(body_object(None), json!({}));
        assert_eq!(
            body_object(Some(Json(json!({ "like": true })))),
            json!({ "like": true })
        );
    }

    /// `calculateLikeScoreStatement` by hand: one like is
    /// `(2.9208 - 1.96 * sqrt(0.9604)) / 4.8416 = 1 / 4.8416`; the
    /// `(likes * dislikes) / (likes + dislikes)` term truncates as two
    /// bigints do (3 * 1 / 4 = 0, not 0.75).
    #[test]
    fn like_score_is_upstreams_wilson_statement() {
        assert_eq!(like_score(0, 0), None);
        assert_eq!(like_rate(0, 0), None);
        let one = like_score(1, 0).unwrap();
        assert!((one - 1.0 / 4.8416).abs() < 1e-12, "{one}");
        let three_one = like_score(3, 1).unwrap();
        let expected =
            ((3.0 + 1.9208) / 4.0 - 1.96 * (0.0f64 + 0.9604).sqrt() / 4.0) / (1.0 + 3.8416 / 4.0);
        assert!((three_one - expected).abs() < 1e-12, "{three_one}");
        let ten_ten = like_score(10, 10).unwrap();
        let expected = ((10.0 + 1.9208) / 20.0 - 1.96 * (5.0f64 + 0.9604).sqrt() / 20.0)
            / (1.0 + 3.8416 / 20.0);
        assert!((ten_ten - expected).abs() < 1e-12, "{ten_ten}");
        // dislikes only: `1.96 * sqrt(0.9604)` is 1.9208, so the statement lands on zero
        assert!(
            like_score(0, 5).unwrap().abs() < 1e-12,
            "{:?}",
            like_score(0, 5)
        );
        assert_eq!(like_rate(3, 1), Some(0.75));
    }

    /// The mirrored rate and score of (3 likes, 1 dislike) among 10 voters
    /// lead back to (3, 1); no score means no qualified voter; values
    /// matching no pair split the total by the rate.
    #[test]
    fn upstreams_qualified_votes_come_back_from_the_mirrored_rates() {
        let m = MirroredVotes {
            likes: 7,
            dislikes: 3,
            like_rate: like_rate(3, 1),
            like_score: like_score(3, 1),
        };
        assert_eq!(upstream_qualified_votes(&m), (3, 1));
        let all = MirroredVotes {
            likes: 40,
            dislikes: 2,
            like_rate: like_rate(40, 2),
            like_score: like_score(40, 2),
        };
        assert_eq!(upstream_qualified_votes(&all), (40, 2));
        let none = MirroredVotes {
            likes: 5,
            dislikes: 1,
            like_rate: None,
            like_score: None,
        };
        assert_eq!(upstream_qualified_votes(&none), (0, 0));
        let rounded = MirroredVotes {
            likes: 4,
            dislikes: 4,
            like_rate: Some(0.5),
            like_score: Some(0.123),
        };
        assert_eq!(upstream_qualified_votes(&rounded), (4, 4));
        // 1/1 and 2/2 share a rate; the score tells them apart.
        let two_two = MirroredVotes {
            likes: 9,
            dislikes: 9,
            like_rate: Some(0.5),
            like_score: like_score(2, 2),
        };
        assert_eq!(upstream_qualified_votes(&two_two), (2, 2));
    }

    #[test]
    fn combined_rates_need_a_local_qualified_vote_and_add_to_upstreams() {
        let m = MirroredVotes {
            likes: 4,
            dislikes: 1,
            like_rate: like_rate(3, 1),
            like_score: like_score(3, 1),
        };
        let unqualified = LocalVotes {
            likes: 1,
            ..LocalVotes::default()
        };
        assert_eq!(combined_rates(&m, &unqualified), (None, None));
        let qualified = LocalVotes {
            likes: 1,
            qualified_likes: 1,
            ..LocalVotes::default()
        };
        assert_eq!(
            combined_rates(&m, &qualified),
            (like_rate(4, 1), like_score(4, 1))
        );
        let fresh = combined_rates(&MirroredVotes::default(), &qualified);
        assert_eq!(fresh, (Some(1.0), like_score(1, 0)));
        // Imported rows are the whole truth: the mirror's reconstruction is out.
        let imported = LocalVotes {
            favorites: 0,
            likes: 2,
            dislikes: 1,
            qualified_likes: 2,
            qualified_dislikes: 1,
            imported: 3,
        };
        assert_eq!(
            combined_rates(&m, &imported),
            (like_rate(2, 1), like_score(2, 1))
        );
        let unqualified_import = LocalVotes {
            likes: 1,
            imported: 1,
            ..LocalVotes::default()
        };
        assert_eq!(
            combined_rates(&m, &unqualified_import),
            (None, None),
            "an import without a qualified voter leaves the reads on the mirrored value"
        );
    }

    #[tokio::test]
    async fn imported_likes_are_the_ground_truth_and_a_local_vote_overrides_by_wallet() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let entity = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let lone = uuid::Uuid::new_v4().to_string();
        let wallets: Vec<String> = (0..4).map(|i| format!("0x{i:0>40}")).collect();
        // The mirrored rows already count the voters the export holds: entity's
        // four (2 likes and 1 dislike qualified), lone's single like.
        seed_mirrored(&pool, &entity, (3, 1), (2, 1)).await;
        seed_mirrored(&pool, &lone, (1, 0), (1, 0)).await;
        let app = test_app(&pool);
        let export = export_line;
        let lines = vec![
            export(&wallets[0], &entity, true, 150.0),
            export(&wallets[1], &entity, true, 400.0),
            export(&wallets[2], &entity, false, 120.0),
            export(&wallets[3], &entity, true, 10.0),
            String::new(),
            export(&wallets[0], &other, false, 150.0),
            export(&wallets[0], &lone, true, 150.0),
        ];
        let out = import_upstream_likes(&pool, lines.into_iter().map(Ok))
            .await
            .unwrap();
        assert_eq!(
            out,
            LikesImport {
                read: 6,
                imported: 6,
                kept_local: 0,
                settled: 0,
                entities: 3
            }
        );
        assert_eq!(
            counts_row(&pool, &entity).await,
            (0, 3, 1, like_rate(2, 1), like_score(2, 1), 4),
            "the rates come from the imported facts, the unqualified voter aside"
        );
        // The mirrored total holds the same voters: the read answers the facts, not the sum.
        let (_, body) = call(&app, "GET", &format!("/api/places/{entity}"), None, false).await;
        assert_eq!(
            (
                body["data"]["likes"].clone(),
                body["data"]["dislikes"].clone()
            ),
            (json!(3), json!(1)),
            "{body}"
        );
        // The wallet that liked upstream dislikes here: one fact per wallet,
        // its value overridden and its origin kept.
        set_like(&pool, &entity, &wallets[1], Some(false), 500.0)
            .await
            .unwrap();
        assert_eq!(
            counts_row(&pool, &entity).await,
            (0, 2, 2, like_rate(1, 2), like_score(1, 2), 4)
        );
        let source: String = sqlx::query_scalar(
            r#"SELECT source FROM explore.user_likes WHERE "user" = $1 AND entity_id = $2"#,
        )
        .bind(&wallets[1])
        .bind(&entity)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(source, "upstream");
        // lone's only voter, imported, flips to a dislike: the facts alone,
        // never the mirrored like it was plus the dislike it is.
        set_like(&pool, &lone, &wallets[0], Some(false), 150.0)
            .await
            .unwrap();
        assert_eq!(
            counts_row(&pool, &lone).await,
            (0, 0, 1, like_rate(0, 1), like_score(0, 1), 1)
        );
        let (_, body) = call(&app, "GET", &format!("/api/places/{lone}"), None, false).await;
        assert_eq!(
            (
                body["data"]["likes"].clone(),
                body["data"]["dislikes"].clone()
            ),
            (json!(0), json!(1)),
            "{body}"
        );
        assert!(
            approx(body["data"]["like_rate"].as_f64(), Some(0.0)),
            "{body}"
        );
        // A second import of the same export changes nothing: the entity is settled.
        let again = import_upstream_likes(
            &pool,
            vec![export(&wallets[1], &entity, true, 400.0)]
                .into_iter()
                .map(Ok),
        )
        .await
        .unwrap();
        assert_eq!((again.imported, again.kept_local, again.settled), (0, 0, 1));
        assert_eq!(
            counts_row(&pool, &entity).await,
            (0, 2, 2, like_rate(1, 2), like_score(1, 2), 4)
        );
        let bad = import_upstream_likes(
            &pool,
            vec![export("not-a-wallet", &entity, true, 1.0)]
                .into_iter()
                .map(Ok),
        )
        .await;
        assert!(bad.unwrap_err().to_string().contains("line 1"));
        remove_entities(&pool, &[entity, other, lone]).await;
    }

    /// A withdrawn imported vote leaves the entity on the facts: the
    /// mirrored total, which still counts that voter, must not come back,
    /// and the wallet's next vote must count once.
    #[tokio::test]
    async fn withdrawing_the_only_imported_vote_keeps_the_entity_on_the_facts() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let entity = uuid::Uuid::new_v4().to_string();
        let wallet = format!("0x{:0>40}", "a1");
        seed_mirrored(&pool, &entity, (1, 0), (1, 0)).await;
        let app = test_app(&pool);
        let out = import_upstream_likes(
            &pool,
            vec![export_line(&wallet, &entity, true, 150.0)]
                .into_iter()
                .map(Ok),
        )
        .await
        .unwrap();
        assert_eq!((out.imported, out.entities), (1, 1));
        assert_eq!(served_votes(&app, &entity).await, (json!(1), json!(0)));
        set_like(&pool, &entity, &wallet, None, 0.0).await.unwrap();
        assert_eq!(
            served_votes(&app, &entity).await,
            (json!(0), json!(0)),
            "the withdrawn vote is gone from the facts and the mirror stays out"
        );
        let (_, likes, dislikes, _, _, imported) = counts_row(&pool, &entity).await;
        assert_eq!((likes, dislikes), (0, 0));
        assert!(imported > 0, "the entity stays settled");
        set_like(&pool, &entity, &wallet, Some(true), 150.0)
            .await
            .unwrap();
        assert_eq!(
            served_votes(&app, &entity).await,
            (json!(1), json!(0)),
            "the wallet's new vote counts once"
        );
        remove_entities(&pool, &[entity]).await;
    }

    /// Importing again leaves every entity the first import settled alone:
    /// a vote a wallet withdrew here does not come back.
    #[tokio::test]
    async fn a_second_import_leaves_a_settled_entity_alone() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let entity = uuid::Uuid::new_v4().to_string();
        let fresh = uuid::Uuid::new_v4().to_string();
        let wallets: Vec<String> = (0..2).map(|i| format!("0x{i:0>39}b")).collect();
        seed_mirrored(&pool, &entity, (2, 0), (2, 0)).await;
        let app = test_app(&pool);
        let export = vec![
            export_line(&wallets[0], &entity, true, 150.0),
            export_line(&wallets[1], &entity, true, 150.0),
        ];
        let out = import_upstream_likes(&pool, export.clone().into_iter().map(Ok))
            .await
            .unwrap();
        assert_eq!((out.imported, out.entities), (2, 1));
        set_like(&pool, &entity, &wallets[0], None, 0.0)
            .await
            .unwrap();
        assert_eq!(served_votes(&app, &entity).await, (json!(1), json!(0)));
        // The same export plus a row for an entity the import never saw.
        let mut again = export.clone();
        again.push(export_line(&wallets[0], &fresh, false, 150.0));
        let out = import_upstream_likes(&pool, again.into_iter().map(Ok))
            .await
            .unwrap();
        assert_eq!(
            out,
            LikesImport {
                read: 3,
                imported: 1,
                kept_local: 0,
                settled: 2,
                entities: 1
            }
        );
        assert_eq!(
            served_votes(&app, &entity).await,
            (json!(1), json!(0)),
            "the withdrawn vote stays withdrawn"
        );
        let voted: Option<bool> = sqlx::query_scalar(
            r#"SELECT "like" FROM explore.user_likes WHERE "user" = $1 AND entity_id = $2"#,
        )
        .bind(&wallets[0])
        .bind(&entity)
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(voted, None);
        assert_eq!(
            counts_row(&pool, &fresh).await.5,
            1,
            "the new entity is imported"
        );
        remove_entities(&pool, &[entity, fresh]).await;
    }

    /// A covered entity whose qualified voters all withdrew serves no rate
    /// and no score, as the old service does, instead of the mirror's stale
    /// ones.
    #[tokio::test]
    async fn a_covered_entity_without_qualified_voters_serves_no_rate() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let entity = uuid::Uuid::new_v4().to_string();
        let wallet = format!("0x{:0>40}", "d4");
        seed_mirrored(&pool, &entity, (1, 0), (1, 0)).await;
        let app = test_app(&pool);
        import_upstream_likes(
            &pool,
            vec![export_line(&wallet, &entity, true, 150.0)]
                .into_iter()
                .map(Ok),
        )
        .await
        .unwrap();
        let (_, body) = call(&app, "GET", &format!("/api/places/{entity}"), None, false).await;
        assert_eq!(body["data"]["like_rate"].as_f64(), Some(1.0));
        set_like(&pool, &entity, &wallet, None, 0.0).await.unwrap();
        let (_, body) = call(&app, "GET", &format!("/api/places/{entity}"), None, false).await;
        assert_eq!(
            (
                body["data"]["likes"].clone(),
                body["data"]["like_rate"].clone(),
                body["data"]["like_score"].clone()
            ),
            (json!(0), Value::Null, Value::Null),
            "the mirror's rate and score stay out of a covered entity"
        );
        remove_entities(&pool, &[entity]).await;
    }

    /// An entity whose export rows all met local votes settles too: the
    /// import keeps the local rows and records the rows the export held, so
    /// the mirror stops counting the voter beside the facts.
    #[tokio::test]
    async fn an_entity_whose_export_rows_all_met_local_votes_settles() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let entity = uuid::Uuid::new_v4().to_string();
        let wallet = format!("0x{:0>39}c", 7);
        seed_mirrored(&pool, &entity, (1, 0), (1, 0)).await;
        let app = test_app(&pool);
        set_like(&pool, &entity, &wallet, Some(true), 150.0)
            .await
            .unwrap();
        assert_eq!(
            served_votes(&app, &entity).await,
            (json!(2), json!(0)),
            "the mirror counts the voter until the import settles the entity"
        );
        let export = vec![export_line(&wallet, &entity, true, 150.0)];
        let out = import_upstream_likes(&pool, export.into_iter().map(Ok))
            .await
            .unwrap();
        assert_eq!((out.imported, out.kept_local), (0, 1));
        assert_eq!(
            counts_row(&pool, &entity).await.5,
            1,
            "the row the export held settles the entity"
        );
        assert_eq!(served_votes(&app, &entity).await, (json!(1), json!(0)));
        remove_entities(&pool, &[entity]).await;
    }

    /// The import takes an entity's counter lock before it writes that
    /// entity's facts, in the order `set_like_in` takes them, so a wallet
    /// that votes during the import waits instead of deadlocking it: the
    /// voter here holds the counter row as `set_like_in` does first, the
    /// import blocks on it before touching the fact table, the voter writes
    /// its fact and commits, and the import then keeps the local vote.
    #[tokio::test]
    async fn the_import_locks_the_counter_row_before_the_entitys_facts() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let entity = uuid::Uuid::new_v4().to_string();
        let voter = format!("0x{:0>40}", "c1");
        let other = format!("0x{:0>40}", "c2");
        let mut voting = pool.begin().await.unwrap();
        let voter_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *voting)
            .await
            .unwrap();
        lock_counts(&mut voting, &entity).await.unwrap();
        let lines = vec![
            export_line(&voter, &entity, false, 150.0),
            export_line(&other, &entity, true, 150.0),
        ];
        let import = tokio::spawn({
            let pool = pool.clone();
            async move { import_upstream_likes(&pool, lines.into_iter().map(Ok)).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let blocked: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
                )
                .bind(voter_pid)
                .fetch_one(&pool)
                .await
                .unwrap();
                if blocked > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the import waits on the held counter row");
        let facts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM explore.user_likes WHERE entity_id = $1")
                .bind(&entity)
                .fetch_one(&mut *voting)
                .await
                .unwrap();
        assert_eq!(facts, 0, "nothing of the entity is written before its lock");
        set_like_in(&mut voting, &entity, &voter, Some(true), 150.0)
            .await
            .unwrap();
        voting.commit().await.unwrap();
        let out = import.await.unwrap().unwrap();
        assert_eq!(
            out,
            LikesImport {
                read: 2,
                imported: 1,
                kept_local: 1,
                settled: 0,
                entities: 1
            }
        );
        assert_eq!(
            counts_row(&pool, &entity).await,
            (0, 2, 0, like_rate(2, 0), like_score(2, 0), 2)
        );
        remove_entities(&pool, &[entity]).await;
    }

    /// Each entity commits on its own: a vote on an entity the import
    /// settled waits for that entity alone, not for the rest of the export,
    /// and a stopped import keeps what it settled.
    #[tokio::test]
    async fn each_entity_commits_on_its_own() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let run = uuid::Uuid::new_v4();
        let first = format!("a-{run}");
        let second = format!("b-{run}");
        let wallet = format!("0x{:0>40}", "d1");
        let mut voting = pool.begin().await.unwrap();
        lock_counts(&mut voting, &second).await.unwrap();
        let lines = vec![
            export_line(&wallet, &first, true, 150.0),
            export_line(&wallet, &second, true, 150.0),
        ];
        let import = tokio::spawn({
            let pool = pool.clone();
            async move { import_upstream_likes(&pool, lines.into_iter().map(Ok)).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let settled: Option<i32> = sqlx::query_scalar(
                    "SELECT imported FROM explore.place_interaction_counts WHERE entity_id = $1",
                )
                .bind(&first)
                .fetch_optional(&pool)
                .await
                .unwrap();
                if settled.is_some_and(|imported| imported > 0) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first entity is committed while the second waits on its held counter row");
        voting.rollback().await.unwrap();
        let out = import.await.unwrap().unwrap();
        assert_eq!((out.imported, out.entities), (2, 2));
        remove_entities(&pool, &[first, second]).await;
    }

    fn test_app(pool: &PgPool) -> axum::Router {
        router(Arc::new(
            ExploreState::new(pool.clone(), Settings::for_tests(), None)
                .with_outbound(Outbound::for_tests()),
        ))
    }

    /// A catalogue row whose mirrored totals are `(likes, dislikes)`, with
    /// `qualified` of them behind the mirrored rate and score.
    async fn seed_mirrored(pool: &PgPool, id: &str, totals: (i32, i32), qualified: (i64, i64)) {
        sqlx::query("INSERT INTO explore.place (id, base_position, title, likes, dislikes, raw) VALUES ($1, '1,1', 'p', $2, $3, $4)")
            .bind(id)
            .bind(totals.0)
            .bind(totals.1)
            .bind(json!({
                "positions": ["1,1"],
                "like_rate": like_rate(qualified.0, qualified.1),
                "like_score": like_score(qualified.0, qualified.1)
            }))
            .execute(pool)
            .await
            .unwrap();
    }

    /// One line of the export, `row_to_json(user_likes)` as the old service
    /// wrote it: the entity under its older name, the wallet in mixed case.
    fn export_line(wallet: &str, entity: &str, like: bool, vp: f64) -> String {
        json!({
            "user": wallet.to_uppercase().replace("0X", "0x"),
            "place_id": entity,
            "like": like,
            "user_activity": vp,
            "created_at": "2025-03-01T10:00:00.000+00:00",
            "updated_at": "2025-03-02T10:00:00.000+00:00"
        })
        .to_string()
    }

    /// The `(likes, dislikes)` `GET /api/places/{id}` answers.
    async fn served_votes(app: &axum::Router, entity: &str) -> (Value, Value) {
        let (_, body) = call(app, "GET", &format!("/api/places/{entity}"), None, false).await;
        (
            body["data"]["likes"].clone(),
            body["data"]["dislikes"].clone(),
        )
    }

    async fn remove_entities(pool: &PgPool, ids: &[String]) {
        for sql in [
            "DELETE FROM explore.user_likes WHERE entity_id = ANY($1)",
            "DELETE FROM explore.place_interaction_counts WHERE entity_id = ANY($1)",
            "DELETE FROM explore.place WHERE id = ANY($1)",
        ] {
            sqlx::query(sql).bind(ids).execute(pool).await.unwrap();
        }
    }

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use alloy::signers::local::PrivateKeySigner;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::http::Outbound;
    use crate::shared::signed_fetch::test_support::{address, signed_headers_by, wallet};
    use crate::{ExploreState, Settings, router};

    /// A stand-in for `https://score.snapshot.org/`: one voting power per
    /// lowercased address, every request kept, and a switch to answer
    /// garbage.
    #[derive(Default)]
    struct ScoreStub {
        scores: HashMap<String, f64>,
        calls: Vec<Value>,
        broken: bool,
    }

    async fn score_api(stub: Arc<Mutex<ScoreStub>>) -> String {
        async fn handle(
            State(stub): State<Arc<Mutex<ScoreStub>>>,
            Json(body): Json<Value>,
        ) -> Response {
            let mut stub = stub.lock().unwrap();
            let address = body["params"]["address"].as_str().unwrap_or("").to_string();
            stub.calls.push(body);
            if stub.broken {
                return (StatusCode::INTERNAL_SERVER_ERROR, "not json").into_response();
            }
            let vp = stub.scores.get(&address).copied().unwrap_or(0.0);
            Json(json!({ "jsonrpc": "2.0", "result": { "vp": vp, "vp_by_strategy": [vp], "vp_state": "final" } }))
                .into_response()
        }
        let app = axum::Router::new()
            .route("/", axum::routing::post(handle))
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/")
    }

    async fn call_as(
        app: &axum::Router,
        signer: Option<&PrivateKeySigner>,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(signer) = signer {
            let timestamp = chrono::Utc::now().timestamp_millis().to_string();
            for (k, v) in
                signed_headers_by(signer, &method.to_lowercase(), path, "{}", true, &timestamp)
                    .await
                    .iter()
            {
                req = req.header(k, v);
            }
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
        signed: bool,
    ) -> (StatusCode, Value) {
        call_as(app, signed.then(wallet).as_ref(), method, path, body).await
    }

    fn lower(signer: &PrivateKeySigner) -> String {
        format!("{:#x}", signer.address()).to_lowercase()
    }

    async fn counts_row(
        pool: &PgPool,
        entity_id: &str,
    ) -> (i32, i32, i32, Option<f64>, Option<f64>, i32) {
        sqlx::query_as("SELECT favorites, likes, dislikes, like_rate, like_score, imported FROM explore.place_interaction_counts WHERE entity_id = $1")
            .bind(entity_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn like_activity(pool: &PgPool, user: &str, entity_id: &str) -> Option<f64> {
        sqlx::query_scalar(
            r#"SELECT user_activity FROM explore.user_likes WHERE "user" = $1 AND entity_id = $2"#,
        )
        .bind(user)
        .bind(entity_id)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    fn approx(a: Option<f64>, b: Option<f64>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => (a - b).abs() < 1e-9,
            _ => false,
        }
    }

    /// The wkc routes' contract (fixtures `places.patch_favorites.*`,
    /// `places.patch_likes.*`, `worlds.patch_favorites.*`, `worlds.patch_likes.*`):
    /// body before auth, auth before the worlds route's UUID refusal, the
    /// voting power fetched and stored, `{}` liking, the computed `± 1`, the
    /// no-op short-circuits, the mirrored counters plus this node's, the
    /// rates over qualified voters only, the rebuild from the facts.
    #[tokio::test]
    async fn favorites_and_likes_follow_the_wkc_routes_and_score_their_voters() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let stub = Arc::new(Mutex::new(ScoreStub::default()));
        let endpoint = score_api(stub.clone()).await;
        let whale = wallet();
        let minnow = PrivateKeySigner::random();
        stub.lock().unwrap().scores.insert(lower(&whale), 2500.7);
        stub.lock().unwrap().scores.insert(lower(&minnow), 42.0);

        // A mirrored place with upstream's votes: 4 likes and 1 dislike,
        // 3 likes and 1 dislike among them qualified (like_rate 0.75).
        let place = uuid::Uuid::new_v4().to_string();
        let world = format!(
            "fav{}.dcl.eth",
            &uuid::Uuid::new_v4().simple().to_string()[..10]
        );
        sqlx::query("INSERT INTO explore.place (id, base_position, title, favorites, likes, dislikes, raw) VALUES ($1, '1,1', 'p', 10, 4, 1, $2)")
            .bind(&place)
            .bind(json!({ "positions": ["1,1"], "like_rate": like_rate(3, 1), "like_score": like_score(3, 1) }))
            .execute(&pool)
            .await
            .unwrap();
        // A mirrored world (its id is its name, as upstream's) nobody rated.
        sqlx::query("INSERT INTO explore.place (id, base_position, title, favorites, raw) VALUES ($1, '0,0', 'w', 3, $2)")
            .bind(&world)
            .bind(json!({ "world": true, "world_name": world }))
            .execute(&pool)
            .await
            .unwrap();

        let settings = Settings {
            user_writes_enabled: true,
            snapshot_score_url: Some(endpoint.clone()),
            ..Settings::for_tests()
        };
        let closed = router(Arc::new(
            ExploreState::new(
                pool.clone(),
                Settings {
                    user_writes_enabled: false,
                    ..settings.clone()
                },
                None,
            )
            .with_outbound(Outbound::for_tests()),
        ));
        let app = router(Arc::new(
            ExploreState::new(pool.clone(), settings, None).with_outbound(Outbound::for_tests()),
        ));
        let fav_path = format!("/api/places/{place}/favorites");
        let likes_path = format!("/api/places/{place}/likes");

        // body before auth (`routes.ts:39-41`): fixtures missing_field, extra_key, wrong_type
        let (status, body) = call(&app, "PATCH", &fav_path, Some(json!({})), false).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            body["error"],
            json!(
                "Error validating input:\n- must have required property 'favorites' {\"missingProperty\":\"favorites\"}"
            )
        );
        assert_eq!(body["code"], json!("bad_request"));
        assert_eq!(body["data"]["body"], json!({}));
        assert_eq!(body["data"]["errors"][0]["keyword"], json!("required"));
        let (status, body) = call(
            &app,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": true, "foo": 1 })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            json!(
                "Error validating input:\n- must NOT have additional properties {\"additionalProperty\":\"foo\"}"
            )
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": "yes" })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            json!("Error validating input:\n- favorites must be boolean {\"type\":\"boolean\"}")
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &likes_path,
            Some(json!({ "like": true, "foo": 1 })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["data"]["messages"],
            json!(["must NOT have additional properties {\"additionalProperty\":\"foo\"}"])
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &likes_path,
            Some(json!({ "like": "yes" })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("Error validating input:\n- like must be boolean"),
            "{body}"
        );
        // no body is `{}` (body-parser): favorites refuses it, likes reads a like
        let (status, body) = call(&app, "PATCH", &fav_path, None, false).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // auth: fixtures no_auth (400 Invalid Auth Chain), the gate after it
        let (status, body) = call(
            &app,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": true })),
            false,
        )
        .await;
        assert_eq!(
            (status, &body["error"]),
            (StatusCode::BAD_REQUEST, &json!("Invalid Auth Chain"))
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &likes_path,
            Some(json!({ "like": true })),
            false,
        )
        .await;
        assert_eq!(
            (status, &body["error"]),
            (StatusCode::BAD_REQUEST, &json!("Invalid Auth Chain"))
        );
        let (status, body) = call(
            &closed,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["code"], json!("service_unavailable"));
        assert!(
            stub.lock().unwrap().calls.is_empty(),
            "no score asked before a refusal"
        );

        // not found: fixtures not_found; a UUID never falls through to the worlds
        let missing = uuid::Uuid::new_v4().to_string();
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/places/{missing}/favorites"),
            Some(json!({ "favorites": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body["error"],
            json!(format!("Not found entity \"{missing}\""))
        );
        let (status, body) = call(
            &app,
            "PATCH",
            "/api/places/nobody.dcl.eth/likes",
            Some(json!({ "like": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], json!("Not found entity \"nobody.dcl.eth\""));
        assert!(
            stub.lock().unwrap().calls.is_empty(),
            "no score asked for a missing entity"
        );

        // favorites: set answers entity.favorites + 1 computed, stores the VP, the repeat is a no-op
        let (status, body) = call(
            &app,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!({ "favorites": 11, "user_favorite": true })
        );
        let vp: f64 = sqlx::query_scalar(r#"SELECT user_activity FROM explore.user_favorites WHERE "user" = $1 AND entity_id = $2"#)
            .bind(address().to_lowercase())
            .bind(&place)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(vp, 2500.0, "`(vp || 0) | 0` truncates");
        {
            let calls = &stub.lock().unwrap().calls;
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0]["method"], json!("get_vp"));
            assert_eq!(calls[0]["params"]["address"], json!(lower(&whale)));
            assert_eq!(calls[0]["params"]["space"], json!("snapshot.dcl.eth"));
            assert_eq!(
                calls[0]["params"]["strategies"].as_array().unwrap().len(),
                5
            );
        }
        let (status, body) = call(
            &app,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["data"],
            json!({ "favorites": 11, "user_favorite": true }),
            "repeat: current counts"
        );
        assert_eq!(
            stub.lock().unwrap().calls.len(),
            2,
            "fetchScore runs before the favorites short-circuit (routes.ts:55)"
        );
        let (_, body) = call(
            &app,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": false })),
            true,
        )
        .await;
        assert_eq!(
            body["data"],
            json!({ "favorites": 10, "user_favorite": false })
        );
        let (_, body) = call(
            &app,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": false })),
            true,
        )
        .await;
        assert_eq!(
            body["data"],
            json!({ "favorites": 10, "user_favorite": false }),
            "unset when absent: no-op"
        );
        let (_, body) = call(
            &app,
            "PATCH",
            &fav_path,
            Some(json!({ "favorites": true })),
            true,
        )
        .await;
        assert_eq!(body["data"]["favorites"], json!(11));

        // likes: `{}` likes (model.ts:16) and stores the VP; the qualified vote moves the rates
        stub.lock().unwrap().calls.clear();
        let (status, body) = call(&app, "PATCH", &likes_path, Some(json!({})), true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!({ "likes": 5, "dislikes": 1, "user_like": true, "user_dislike": false })
        );
        assert_eq!(
            like_activity(&pool, &lower(&whale), &place).await,
            Some(2500.0)
        );
        assert_eq!(stub.lock().unwrap().calls.len(), 1);
        let (f, l, d, rate, score, _) = counts_row(&pool, &place).await;
        assert_eq!((f, l, d), (1, 1, 0));
        assert!(approx(rate, like_rate(4, 1)), "{rate:?}");
        assert!(approx(score, like_score(4, 1)), "{score:?}");
        let (_, body) = call(&app, "GET", &format!("/api/places/{place}"), None, false).await;
        assert!(
            approx(body["data"]["like_score"].as_f64(), like_score(4, 1)),
            "{}",
            body["data"]["like_score"]
        );
        assert!(
            approx(body["data"]["like_rate"].as_f64(), Some(0.8)),
            "{}",
            body["data"]["like_rate"]
        );
        // the same vote again: no write, no score call (routes.ts:65-72)
        let (_, body) = call(
            &app,
            "PATCH",
            &likes_path,
            Some(json!({ "like": true })),
            true,
        )
        .await;
        assert_eq!(
            body["data"],
            json!({ "likes": 5, "dislikes": 1, "user_like": true, "user_dislike": false })
        );
        assert_eq!(
            stub.lock().unwrap().calls.len(),
            1,
            "repeat_same_value skips fetchScore"
        );
        // `{}` over a stored like: fetchScore runs, nothing changes
        let (_, body) = call(&app, "PATCH", &likes_path, Some(json!({})), true).await;
        assert_eq!(body["data"]["likes"], json!(5));
        assert_eq!(stub.lock().unwrap().calls.len(), 2);

        // an unqualified voter counts in likes/dislikes, not in the rates (threshold)
        let (status, body) = call_as(
            &app,
            Some(&minnow),
            "PATCH",
            &likes_path,
            Some(json!({ "like": false })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!({ "likes": 5, "dislikes": 2, "user_like": false, "user_dislike": true })
        );
        assert_eq!(
            like_activity(&pool, &lower(&minnow), &place).await,
            Some(42.0)
        );
        let (_, _, _, rate, score, _) = counts_row(&pool, &place).await;
        assert!(approx(rate, like_rate(4, 1)), "{rate:?}");
        assert!(approx(score, like_score(4, 1)), "{score:?}");
        // the whale flips to a dislike: the row is updated in place, the rates follow
        let (_, body) = call(
            &app,
            "PATCH",
            &likes_path,
            Some(json!({ "like": false })),
            true,
        )
        .await;
        assert_eq!(
            body["data"],
            json!({ "likes": 4, "dislikes": 3, "user_like": false, "user_dislike": true })
        );
        let (_, _, _, rate, score, _) = counts_row(&pool, &place).await;
        assert!(approx(rate, like_rate(3, 2)), "{rate:?}");
        assert!(approx(score, like_score(3, 2)), "{score:?}");
        // null withdraws without a score call; no qualified local vote: the mirrored rates serve again
        let before = stub.lock().unwrap().calls.len();
        let (_, body) = call(
            &app,
            "PATCH",
            &likes_path,
            Some(json!({ "like": null })),
            true,
        )
        .await;
        assert_eq!(
            body["data"],
            json!({ "likes": 4, "dislikes": 2, "user_like": false, "user_dislike": false })
        );
        assert_eq!(
            stub.lock().unwrap().calls.len(),
            before,
            "null_withdraws: no Snapshot call"
        );
        let (_, _, _, rate, score, _) = counts_row(&pool, &place).await;
        assert_eq!((rate, score), (None, None));
        let (_, body) = call(&app, "GET", &format!("/api/places/{place}"), None, false).await;
        assert!(
            approx(body["data"]["like_score"].as_f64(), like_score(3, 1)),
            "{}",
            body["data"]["like_score"]
        );
        // a broken score API scores 0: the like lands, unqualified
        stub.lock().unwrap().broken = true;
        let (status, body) = call(
            &app,
            "PATCH",
            &likes_path,
            Some(json!({ "like": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["likes"], json!(5));
        assert_eq!(
            like_activity(&pool, &lower(&whale), &place).await,
            Some(0.0)
        );
        stub.lock().unwrap().broken = false;

        // the worlds routes: auth before the UUID refusal, `{}` refused (World/schemas.ts:278), the name verbatim
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/worlds/{place}/likes"),
            Some(json!({ "like": true })),
            false,
        )
        .await;
        assert_eq!(
            (status, &body["error"]),
            (StatusCode::BAD_REQUEST, &json!("Invalid Auth Chain"))
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/worlds/{place}/likes"),
            Some(json!({ "like": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            json!(format!(
                "Invalid world ID \"{place}\". Use /places/:entity_id/likes for place entities."
            ))
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/worlds/{place}/favorites"),
            Some(json!({ "favorites": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            json!(format!(
                "Invalid world ID \"{place}\". Use /places/:entity_id/favorites for place entities."
            ))
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/worlds/{world}/likes"),
            Some(json!({})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            body["error"],
            json!(
                "Error validating input:\n- must have required property 'like' {\"missingProperty\":\"like\"}"
            )
        );
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/worlds/{world}/favorites"),
            Some(json!({})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = call(
            &app,
            "PATCH",
            "/api/worlds/nobody.dcl.eth/favorites",
            Some(json!({ "favorites": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], json!("Not found world \"nobody.dcl.eth\""));
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/worlds/{world}/favorites"),
            Some(json!({ "favorites": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!({ "favorites": 4, "user_favorite": true })
        );
        // the retro-compatible world id on the places route (entityInteractions.ts:43-52)
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/places/{world}/likes"),
            Some(json!({ "like": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!({ "likes": 1, "dislikes": 0, "user_like": true, "user_dislike": false })
        );
        let (_, _, _, rate, score, _) = counts_row(&pool, &world).await;
        assert_eq!(rate, Some(1.0));
        assert!(approx(score, like_score(1, 0)), "{score:?}");
        let (status, body) = call(
            &app,
            "PATCH",
            &format!("/api/worlds/{world}/likes"),
            Some(json!({ "like": null })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!({ "likes": 0, "dislikes": 0, "user_like": false, "user_dislike": false })
        );

        // the v1 routes keep their summary over the same facts
        let (status, body) = call(
            &app,
            "PUT",
            &format!("/v1/destinations/{world}/likes"),
            Some(json!({ "like": true })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!({ "likes": 1, "dislikes": 0, "favorites": 4, "user_like": true, "user_dislike": false, "user_favorite": true })
        );
        let (_, body) = call(
            &app,
            "DELETE",
            &format!("/v1/destinations/{world}/favorites"),
            None,
            true,
        )
        .await;
        assert_eq!(body["data"]["favorites"], json!(3));
        let (status, _) = call(
            &app,
            "PUT",
            &format!("/v1/destinations/{world}/likes"),
            Some(json!({ "like": null })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &app,
            "PUT",
            &format!("/v1/destinations/{}/favorites", uuid::Uuid::new_v4()),
            None,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // the reads show the same numbers, the viewer's flags when signed
        let (_, body) = call(&app, "GET", &format!("/api/places/{place}"), None, true).await;
        assert_eq!(
            (
                body["data"]["favorites"].clone(),
                body["data"]["likes"].clone(),
                body["data"]["dislikes"].clone(),
                body["data"]["user_favorite"].clone()
            ),
            (json!(11), json!(5), json!(2), json!(true))
        );
        let (_, body) = call(
            &app,
            "GET",
            &format!("/v1/destinations/{world}"),
            None,
            false,
        )
        .await;
        assert_eq!(
            (
                body["data"]["favorites"].clone(),
                body["data"]["likes"].clone()
            ),
            (json!(3), json!(1))
        );

        // the counters equal their rebuild from the facts (`imported` is the
        // import's record, outside the rebuild)
        let expected_place = counts_row(&pool, &place).await;
        let expected_world = counts_row(&pool, &world).await;
        sqlx::query("UPDATE explore.place_interaction_counts SET favorites = 99, dislikes = 7, like_score = 0.5, like_rate = NULL WHERE entity_id = ANY($1)")
            .bind(vec![place.clone(), world.clone()])
            .execute(&pool)
            .await
            .unwrap();
        rebuild_counts(&pool).await.unwrap();
        assert_eq!(
            counts_row(&pool, &place).await,
            expected_place,
            "the rebuild restores the place's counters"
        );
        assert_eq!(
            counts_row(&pool, &world).await,
            expected_world,
            "the rebuild restores the world's counters"
        );
        assert_eq!(
            (expected_place.0, expected_place.1, expected_place.2),
            (1, 1, 1)
        );
        assert_eq!(
            (expected_world.0, expected_world.1, expected_world.2),
            (0, 1, 0)
        );
    }
}
