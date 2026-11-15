//! Operator moderation of a catalogue row: disable a place, highlight a place
//! or a world, set a place's or a world's content rating (the console's
//! `places/{place-disable,place-highlight,place-rating,world-highlight,
//! world-rating}`, `ops::admin::explore`, and the admin routes of
//! `places::admin`).
//!
//! The old service wrote the three columns of its `place` row, and the next
//! mirror pass wrote upstream's values back over them. Here the decision is
//! a fact of its own, `explore.place_moderation` (keyed by the catalogue id,
//! one nullable field per decision: NULL is no override), and the columns of
//! `explore.place` / `explore.place_world_local` are its projection: the
//! write updates both in one transaction, and the places mirror's upsert
//! re-applies the overlay to every row it writes (`mirror::UPSERT`), so a
//! pass never undoes an operator. The read path is unchanged (it reads the
//! projected columns through `place_indexed`, the partial like-score
//! indexes' `disabled IS FALSE` included).
//!
//! Three rules are the old routes' (places e660fbc2):
//! - a disable persists `disabled_at`, `disabled_reason` and `updated_at`
//!   into `raw` as `updateDisabled` did (`Place/model.ts:727-742`), with no
//!   predicate; the overlay keeps `disabled_at` and `reason` so the mirrors
//!   re-apply them;
//! - a highlight or a rating of a place lands only where `updatePlace`'s
//!   `WHERE disabled is false AND world is false` would (`Place/model.ts:
//!   916-935`): on a disabled place the decision is not recorded and the
//!   route still echoes the value, as upstream did; a world has no such
//!   predicate (`World/model.ts` `upsertWorld`, `updateHighlighted`);
//! - every rating decision appends an `explore.place_content_ratings` row
//!   (`updateRating.ts:60-68`, `updateWorldRating.ts:71-79`), written
//!   whether or not the row took it, and a disable or a rating upgrade
//!   captures a webhook notice (`Slack/utils.ts:103-140`, `:226-266`) on
//!   the `places` channel of the moderation outbox
//!   (`crate::moderation_hooks`), which delivers it.
//!
//! The pending rating has two spellings upstream: the route schema's `PR`
//! (`Place/schemas.ts:343`) and the code enum's `RP`
//! (`SceneContentRating.RATING_PENDING`), which ingestion and the worlds
//! default write. Both are accepted; `RP` is stored and served, and the
//! mirrors normalise an upstream `PR` the same way (`mirror::UPSERT`,
//! `derive::UPSERT*`).

use chrono::{DateTime, SubsecRound, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::rows::PlaceRow;
use crate::moderation_hooks::{Channel, enqueue};
use crate::shared::errors::ApiError;

/// The stored content-rating codes (gatsby `SceneContentRating`): `RP` is pending.
pub const RATINGS: &[&str] = &["RP", "E", "T", "A", "R"];

/// What a rating body may carry: the old route enum (`Place/schemas.ts:343`,
/// `World/schemas.ts:346`: `PR`, `E`, `T`, `A`, `R`) plus the stored pending
/// code `RP`.
pub const ACCEPTED_RATINGS: &[&str] = &["PR", "E", "T", "A", "R", "RP"];

/// The old `ratingScale` (`utils/rating/contentRating.ts:52-57`), least to
/// most restrictive; `E` is outside it, so a move from or to `E` is never
/// an upgrade there either.
const RATING_SCALE: &[&str] = &["RP", "T", "A", "R"];

/// The pending code as stored: `PR` (the route schema's spelling) reads as `RP`.
pub fn normalize_rating(code: &str) -> &str {
    if code == "PR" { "RP" } else { code }
}

/// `isUpgradingRating` (`utils/rating/contentRating.ts:69-81`): both codes
/// on the scale and the new one stricter.
pub fn is_upgrading_rating(rating: &str, original: &str) -> bool {
    let index = |code: &str| RATING_SCALE.iter().position(|c| *c == code);
    matches!((index(original), index(rating)), (Some(o), Some(n)) if o < n)
}

/// `to_char` format of the timestamps the projection writes into `raw`
/// (what `rows::place_columns` casts back).
const RAW_TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";

/// What the operator decided about one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Disabled {
        disabled: bool,
        reason: Option<String>,
    },
    Highlighted(bool),
    /// The stored code (`RP` for pending).
    Rating(String),
}

impl Decision {
    /// The rating body rule of the old route: one of [`ACCEPTED_RATINGS`],
    /// stored as its [`RATINGS`] code.
    pub fn rating(raw: &str) -> Result<Self, ApiError> {
        let r = raw.trim();
        if ACCEPTED_RATINGS.contains(&r) {
            Ok(Self::Rating(normalize_rating(r).to_string()))
        } else {
            Err(ApiError::bad_request("content rating body needed"))
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Disabled { .. } => "disabled",
            Self::Highlighted(_) => "highlighted",
            Self::Rating(_) => "content_rating",
        }
    }
}

/// The row a decision is about: a place by its id, a world by its id or name.
#[derive(Debug, Clone)]
pub enum Target<'a> {
    Place(&'a str),
    World(&'a str),
}

impl Target<'_> {
    fn entity_type(&self) -> &'static str {
        match self {
            Self::Place(_) => "place",
            Self::World(_) => "world",
        }
    }
}

/// What goes with a decision beyond the row.
#[derive(Debug, Default, Clone)]
pub struct Context<'a> {
    /// The rating's audit comment (`comment` of the old rating body).
    pub comment: Option<&'a str>,
    /// `Some(places site base URL)`: capture a webhook notice for a disable
    /// or a rating upgrade, linking into that site; `None` captures none.
    pub notify: Option<&'a str>,
}

/// What [`apply_with`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub id: String,
    /// The decision's instant at millisecond precision: what a disable
    /// wrote as `disabled_at` / `updated_at`, so the route's answer equals
    /// the next read.
    pub at: DateTime<Utc>,
    /// Whether the catalogue row took the write (false on a disabled place
    /// for a highlight or a rating: the old `updatePlace` matched nothing).
    pub written: bool,
}

#[derive(Debug, sqlx::FromRow)]
pub(super) struct Resolved {
    pub(super) id: String,
    world: bool,
    disabled: bool,
    disabled_reason: Option<String>,
    title: Option<String>,
    base_position: String,
    world_name: Option<String>,
    content_rating: Option<String>,
}

const RESOLVE_COLUMNS: &str = "id, world, disabled, raw->>'disabled_reason' AS disabled_reason, \
     title, base_position, world_name, content_rating";

/// The catalogue row `target` names, from either leg; `None` when none matches.
/// A place target is any row by id, world-backed ones included
/// (`findByIdWithAggregates`, `Place/model.ts:513-553`); a world target is a
/// world catalogue row by id or folded name (`World/model.ts:333` reads the
/// worlds table alone, so a world known only through its scenes is a miss).
pub(super) async fn resolve(
    pool: &PgPool,
    target: &Target<'_>,
) -> Result<Option<Resolved>, ApiError> {
    let row = match target {
        Target::Place(id) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {RESOLVE_COLUMNS} FROM explore.place_indexed WHERE id = $1 LIMIT 1"
            )))
            .bind(id)
            .fetch_optional(pool)
            .await?
        }
        Target::World(id) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {RESOLVE_COLUMNS} FROM explore.place_indexed WHERE world IS TRUE AND id = lower(world_name) AND id = lower($1) LIMIT 1"
            )))
            .bind(id)
            .fetch_optional(pool)
            .await?
        }
    };
    Ok(row)
}

/// The overlay upsert: only the decided field moves, the others keep their
/// previous override (or none). `disabled_at` and `reason` follow the
/// disable decision.
const OVERLAY: &str = "INSERT INTO explore.place_moderation (place_id, disabled, highlighted, content_rating, reason, set_by, updated_at, disabled_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
     ON CONFLICT (place_id) DO UPDATE SET
         disabled = COALESCE(EXCLUDED.disabled, explore.place_moderation.disabled),
         highlighted = COALESCE(EXCLUDED.highlighted, explore.place_moderation.highlighted),
         content_rating = COALESCE(EXCLUDED.content_rating, explore.place_moderation.content_rating),
         reason = CASE WHEN EXCLUDED.disabled IS NOT NULL THEN EXCLUDED.reason ELSE explore.place_moderation.reason END,
         disabled_at = CASE WHEN EXCLUDED.disabled IS NOT NULL THEN EXCLUDED.disabled_at ELSE explore.place_moderation.disabled_at END,
         set_by = EXCLUDED.set_by,
         updated_at = EXCLUDED.updated_at";

/// The `raw` keys a disable decision writes, as `updateDisabled` did
/// (`Place/model.ts:727-742`): `disabled_at` and `disabled_reason` set or
/// cleared, `updated_at` moved.
fn disable_raw_patch(disabled: bool, reason: Option<&str>, at: DateTime<Utc>) -> Value {
    let at = at.format(RAW_TS_FORMAT).to_string();
    if disabled {
        json!({ "disabled_at": at, "disabled_reason": reason, "updated_at": at })
    } else {
        json!({ "disabled_at": null, "disabled_reason": null, "updated_at": at })
    }
}

/// Records `decision` for `target` as `actor` and projects it onto the row.
/// `Ok(None)` when no catalogue row matches (the old routes' 404).
pub async fn apply(
    pool: &PgPool,
    target: Target<'_>,
    decision: &Decision,
    actor: &str,
) -> Result<Option<String>, ApiError> {
    Ok(
        apply_with(pool, target, decision, actor, Context::default())
            .await?
            .map(|applied| applied.id),
    )
}

/// [`apply`] with the rating's comment and the webhook capture; see the
/// module notes for the predicate, the audit row and the notices.
pub async fn apply_with(
    pool: &PgPool,
    target: Target<'_>,
    decision: &Decision,
    actor: &str,
    ctx: Context<'_>,
) -> Result<Option<Applied>, ApiError> {
    let Some(row) = resolve(pool, &target).await? else {
        return Ok(None);
    };
    let id = row.id.clone();
    let at = Utc::now().trunc_subsecs(3);
    // `updatePlace` (`Place/model.ts:928-935`): a genesis place takes a
    // highlight or rating while enabled, a world-backed place row while
    // enabled or disabled by `opt_out`; a disable has no predicate, nor do
    // the worlds routes (`World/model.ts`).
    let written = match (decision, &target) {
        (Decision::Disabled { .. }, _) | (_, Target::World(_)) => true,
        (Decision::Highlighted(_) | Decision::Rating(_), Target::Place(_)) => {
            !row.disabled || (row.world && row.disabled_reason.as_deref() == Some("opt_out"))
        }
    };
    let (disabled, highlighted, rating, reason) = match decision {
        Decision::Disabled { disabled, reason } => (Some(*disabled), None, None, reason.clone()),
        Decision::Highlighted(h) => (None, Some(*h), None, None),
        Decision::Rating(r) => (None, None, Some(r.clone()), None),
    };
    let mut tx = pool.begin().await?;
    if written {
        sqlx::query(OVERLAY)
            .bind(&id)
            .bind(disabled)
            .bind(highlighted)
            .bind(rating.as_deref())
            .bind(reason.as_deref())
            .bind(actor)
            .bind(at.naive_utc())
            .bind(disabled.filter(|d| *d).map(|_| at.naive_utc()))
            .execute(&mut *tx)
            .await?;
        let raw_patch = match disabled {
            Some(d) => disable_raw_patch(d, reason.as_deref(), at),
            None => json!({}),
        };
        for table in ["explore.place", "explore.place_world_local"] {
            let sql = format!(
                "UPDATE {table} SET disabled = COALESCE($2, disabled), highlighted = COALESCE($3, highlighted), \
                 content_rating = COALESCE($4, content_rating), raw = raw || $5::jsonb WHERE id = $1"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&id)
                .bind(disabled)
                .bind(highlighted)
                .bind(rating.as_deref())
                .bind(&raw_patch)
                .execute(&mut *tx)
                .await?;
        }
    }
    if let Decision::Rating(r) = decision {
        // `PlaceContentRatingModel.create` beside the update, whatever the
        // update matched (`updateRating.ts:58-68`, `updateWorldRating.ts:66-80`).
        sqlx::query(
            "INSERT INTO explore.place_content_ratings (id, entity_id, entity_type, original_rating, update_rating, moderator, comment, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::new_v4())
        .bind(&id)
        .bind(target.entity_type())
        .bind(row.content_rating.as_deref())
        .bind(r)
        .bind(actor)
        .bind(ctx.comment.map(str::trim).filter(|c| !c.is_empty()))
        .bind(at.naive_utc())
        .execute(&mut *tx)
        .await?;
    }
    if let Some(base) = ctx.notify {
        let notice = match decision {
            // `notifyDisablePlaces` on a disable (`updateDisabled.ts:69-71`).
            Decision::Disabled { disabled: true, .. } => Some(("disabled", disabled_message(&row))),
            // `notifyUpgradingRating` on a stricter rating of a rated row
            // (`updateRating.ts:71-79`, `updateWorldRating.ts:82-88`).
            Decision::Rating(r) => row
                .content_rating
                .as_deref()
                .filter(|original| is_upgrading_rating(r, original))
                .map(|original| {
                    (
                        "rating_upgraded",
                        rating_upgraded_message(&row, original, r, base),
                    )
                }),
            _ => None,
        };
        if let Some((kind, message)) = notice {
            capture(&mut tx, &id, kind, &message).await?;
        }
    }
    tx.commit().await?;
    tracing::info!(place = %id, field = decision.label(), actor, written, "place moderation recorded");
    Ok(Some(Applied { id, at, written }))
}

/// `where` of the old notices: the world's name, else the parcel.
fn where_label(row: &Resolved) -> &str {
    match (&row.world, row.world_name.as_deref()) {
        (true, Some(name)) => name,
        _ => row.base_position.as_str(),
    }
}

/// Slack mrkdwn's three reserved characters, for the fixed line that carries a link.
fn mrkdwn_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `notifyDisablePlaces([place])` (`Slack/utils.ts:103-140`), one place.
fn disabled_message(row: &Resolved) -> Value {
    let where_ = where_label(row);
    json!({ "blocks": [
        { "type": "section", "text": { "type": "mrkdwn", "text": format!(":x: 1 place disabled in: {}", mrkdwn_escape(where_)) } },
        { "type": "section", "text": { "type": "plain_text", "text": format!("{} ({where_})\nUUID: {}", row.title.as_deref().unwrap_or_default(), row.id) } }
    ] })
}

/// `notifyUpgradingRating(place, "Content Moderator", rating)` (`Slack/utils.ts:226-266`):
/// the link is `whatsOnPlaceUrl` / `whatsOnWorldUrl` (`Place/utils.ts:41-56`:
/// the base with pathname `/whats-on`, so the base's own path is replaced).
fn rating_upgraded_message(row: &Resolved, original: &str, rating: &str, base: &str) -> Value {
    let (kind, link) = if row.world {
        let name = row.world_name.as_deref().unwrap_or(&row.id);
        (
            "world",
            format!(
                "<{}|{}>",
                mrkdwn_escape(&super::social::site_url(base, "/whats-on", "world", name)),
                mrkdwn_escape(name)
            ),
        )
    } else {
        let position = super::social::canonical_position(&row.base_position, ".")
            .unwrap_or_else(|| row.base_position.clone());
        (
            "place",
            format!(
                "<{}|{}>",
                mrkdwn_escape(&super::social::site_url(
                    base,
                    "/whats-on",
                    "position",
                    &position
                )),
                mrkdwn_escape(&row.base_position)
            ),
        )
    };
    json!({ "blocks": [
        { "type": "section", "text": { "type": "mrkdwn", "text": format!(":white_check_mark: The {kind} upgrade rating: {link}") } },
        { "type": "section", "text": { "type": "plain_text", "text": "The rating was upgraded by Content Moderator" } },
        { "type": "section", "text": { "type": "plain_text", "text": format!("Previous rating: {original} - New rating: {rating}") } }
    ] })
}

/// Queues one notice in the decision's transaction.
async fn capture(
    tx: &mut Transaction<'_, Postgres>,
    place_id: &str,
    kind: &str,
    message: &Value,
) -> Result<(), sqlx::Error> {
    enqueue(tx, Channel::Places, place_id, kind, message, None).await
}

/// The row as the read routes serve it after the decision.
pub async fn answer(
    places: &super::PlacesComponent,
    target: &Target<'_>,
    id: &str,
) -> Result<Option<PlaceRow>, ApiError> {
    match target {
        Target::Place(_) => places.find_by_id_for(id, None).await,
        Target::World(_) => Ok(places
            .find_world_by_id_for(id, None)
            .await?
            .filter(PlaceRow::is_world_catalogue_row)),
    }
}

/// The override in force for `id`, as the mirror re-applies it (for the
/// console's detail and the tests).
pub async fn overlay(pool: &PgPool, id: &str) -> Result<Option<Value>, ApiError> {
    let row: Option<(Value,)> = sqlx::query_as(
        "SELECT jsonb_build_object('disabled', disabled, 'highlighted', highlighted, 'content_rating', content_rating, \
                'reason', reason, 'disabled_at', disabled_at, 'set_by', set_by, 'updated_at', updated_at) \
         FROM explore.place_moderation WHERE place_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(v,)| v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rating_rule_is_the_old_routes_with_both_pending_spellings() {
        assert_eq!(
            Decision::rating(" T ").unwrap(),
            Decision::Rating("T".into())
        );
        // `Place/schemas.ts:343` accepts `PR`; the stored code is `RP`
        // (gatsby `SceneContentRating.RATING_PENDING`); fixture
        // places.put_rating.invalid_enum shows upstream refusing `RP`, which
        // is accepted on purpose as the stored code.
        assert_eq!(
            Decision::rating("PR").unwrap(),
            Decision::Rating("RP".into())
        );
        assert_eq!(
            Decision::rating("RP").unwrap(),
            Decision::Rating("RP".into())
        );
        for code in ["E", "A", "R"] {
            assert_eq!(
                Decision::rating(code).unwrap(),
                Decision::Rating(code.into())
            );
        }
        for bad in ["", "X", "teen", "t", "pr", "M"] {
            assert!(Decision::rating(bad).is_err(), "{bad:?}");
        }
        assert_eq!(normalize_rating("PR"), "RP");
        assert_eq!(normalize_rating("E"), "E");
    }

    /// `utils/rating/contentRating.ts:69-81` over the scale `RP < T < A < R`.
    #[test]
    fn an_upgrade_is_a_stricter_code_on_the_scale() {
        assert!(is_upgrading_rating("T", "RP"));
        assert!(is_upgrading_rating("R", "A"));
        assert!(!is_upgrading_rating("RP", "T"));
        assert!(!is_upgrading_rating("T", "T"));
        assert!(!is_upgrading_rating("T", "E"), "E is outside the scale");
        assert!(!is_upgrading_rating("E", "RP"));
        assert!(
            !is_upgrading_rating("T", "PR"),
            "the unnormalised spelling never upgraded upstream"
        );
    }

    #[test]
    fn the_notices_carry_public_labels_and_the_site_links() {
        let row = Resolved {
            id: "3ca25728-5b41-48f6-8e24-5cb0e3f2bb5d".into(),
            world: false,
            disabled: false,
            disabled_reason: None,
            title: Some("Genesis <Plaza> & co".into()),
            base_position: "-3,-2".into(),
            world_name: None,
            content_rating: Some("RP".into()),
        };
        let disabled = disabled_message(&row);
        assert_eq!(
            disabled["blocks"][0]["text"]["text"],
            json!(":x: 1 place disabled in: -3,-2")
        );
        assert_eq!(disabled["blocks"][1]["text"]["type"], json!("plain_text"));
        assert_eq!(
            disabled["blocks"][1]["text"]["text"],
            json!("Genesis <Plaza> & co (-3,-2)\nUUID: 3ca25728-5b41-48f6-8e24-5cb0e3f2bb5d")
        );
        let upgraded = rating_upgraded_message(&row, "RP", "T", "https://decentraland.org/places");
        assert_eq!(
            upgraded["blocks"][0]["text"]["text"],
            json!(
                ":white_check_mark: The place upgrade rating: <https://decentraland.org/whats-on?position=-3.-2|-3,-2>"
            )
        );
        assert_eq!(
            upgraded["blocks"][1]["text"]["text"],
            json!("The rating was upgraded by Content Moderator")
        );
        assert_eq!(
            upgraded["blocks"][2]["text"]["text"],
            json!("Previous rating: RP - New rating: T")
        );
        let world = Resolved {
            id: "metadynelabs.dcl.eth".into(),
            world: true,
            disabled: false,
            disabled_reason: None,
            title: Some("Metadyne Labs".into()),
            base_position: "0,0".into(),
            world_name: Some("MetadyneLabs.dcl.eth".into()),
            content_rating: Some("T".into()),
        };
        assert_eq!(
            disabled_message(&world)["blocks"][0]["text"]["text"],
            json!(":x: 1 place disabled in: MetadyneLabs.dcl.eth")
        );
        assert_eq!(
            rating_upgraded_message(&world, "T", "A", "https://decentraland.org/places")["blocks"]
                [0]["text"]["text"],
            json!(
                ":white_check_mark: The world upgrade rating: <https://decentraland.org/whats-on?world=MetadyneLabs.dcl.eth|MetadyneLabs.dcl.eth>"
            )
        );
        let patch = disable_raw_patch(
            true,
            Some("moderation"),
            "2026-11-05T10:20:30.456Z".parse().unwrap(),
        );
        assert_eq!(
            patch,
            json!({ "disabled_at": "2026-11-05T10:20:30.456Z", "disabled_reason": "moderation", "updated_at": "2026-11-05T10:20:30.456Z" })
        );
        let cleared = disable_raw_patch(false, None, "2026-11-05T10:20:30.456Z".parse().unwrap());
        assert_eq!(
            cleared,
            json!({ "disabled_at": null, "disabled_reason": null, "updated_at": "2026-11-05T10:20:30.456Z" })
        );
    }

    /// `(entity_type, original_rating, update_rating, moderator, comment)`.
    type AuditRow = (String, Option<String>, String, String, Option<String>);

    fn mirror_row(id: &str, highlighted: bool, rating: &str) -> super::super::mirror::MirrorRow {
        super::super::mirror::MirrorRow {
            id: id.to_string(),
            base_position: "-7,7".into(),
            title: "moderated".into(),
            description: String::new(),
            creator_address: None,
            content_rating: Some(rating.into()),
            categories: Vec::new(),
            likes: 0,
            dislikes: 0,
            favorites: 0,
            deployed_at: None,
            disabled: false,
            highlighted,
            raw: serde_json::json!({ "id": id, "base_position": "-7,7", "content_rating": rating }),
        }
    }

    /// The mirror's upsert keeps an operator's decision over upstream's value
    /// and leaves every undecided field to upstream; a disable's timestamps
    /// and reason persist and survive the pass; an upstream `PR` is stored
    /// as `RP`.
    #[tokio::test]
    async fn a_mirror_pass_reapplies_the_overlay_and_the_projection_serves_it() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let id = format!("console-moderation-{}", uuid::Uuid::new_v4());
        let upsert = |rows: Vec<super::super::mirror::MirrorRow>| {
            let pool = pool.clone();
            async move {
                super::super::mirror::upsert_rows(&pool, &rows)
                    .await
                    .unwrap()
            }
        };
        upsert(vec![mirror_row(&id, false, "PR")]).await;

        let actor = "0x00000000000000000000000000000000000000c0";
        let cols = |pool: PgPool, id: String| async move {
            sqlx::query_as::<_, (bool, bool, Option<String>, Option<String>, Option<String>)>(
                "SELECT disabled, highlighted, content_rating, raw->>'disabled_reason', raw->>'disabled_at' FROM explore.place WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        assert_eq!(
            cols(pool.clone(), id.clone()).await.2.as_deref(),
            Some("RP"),
            "the mirror normalises the route schema's PR to the stored RP"
        );
        let raw_rating: Option<String> =
            sqlx::query_scalar("SELECT raw->>'content_rating' FROM explore.place WHERE id = $1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(raw_rating.as_deref(), Some("RP"));

        let disabled = apply_with(
            &pool,
            Target::Place(&id),
            &Decision::Disabled {
                disabled: true,
                reason: Some("spam".into()),
            },
            actor,
            Context::default(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(disabled.written);
        let (_, _, _, reason, at) = cols(pool.clone(), id.clone()).await;
        assert_eq!(reason.as_deref(), Some("spam"));
        assert_eq!(
            at.as_deref(),
            Some(disabled.at.format(RAW_TS_FORMAT).to_string().as_str()),
            "disabled_at is persisted at the decision's instant"
        );

        // `updatePlace` on a disabled place matches nothing: the rating is
        // not recorded, the audit row is (fixture places.put_rating.disabled_place_silent).
        let rated = apply_with(
            &pool,
            Target::Place(&id),
            &Decision::rating("A").unwrap(),
            actor,
            Context {
                comment: Some(" reviewed "),
                notify: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!rated.written);
        assert_eq!(
            cols(pool.clone(), id.clone()).await,
            (
                true,
                false,
                Some("RP".into()),
                Some("spam".into()),
                at.clone()
            ),
            "a disabled place keeps its rating"
        );
        let audit: Vec<AuditRow> = sqlx::query_as(
            "SELECT entity_type, original_rating, update_rating, moderator, comment FROM explore.place_content_ratings WHERE entity_id = $1 ORDER BY created_at",
        )
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            audit,
            vec![(
                "place".into(),
                Some("RP".into()),
                "A".into(),
                actor.into(),
                Some("reviewed".into())
            )]
        );

        // Re-enabled, the rating lands and the audit grows.
        apply(
            &pool,
            Target::Place(&id),
            &Decision::Disabled {
                disabled: false,
                reason: None,
            },
            actor,
        )
        .await
        .unwrap();
        let (_, _, _, reason, at) = cols(pool.clone(), id.clone()).await;
        assert_eq!((reason, at), (None, None), "a re-enable clears both keys");
        let rated = apply_with(
            &pool,
            Target::Place(&id),
            &Decision::rating("PR").unwrap(),
            actor,
            Context::default(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(rated.written);
        apply(
            &pool,
            Target::Place(&id),
            &Decision::rating("A").unwrap(),
            actor,
        )
        .await
        .unwrap();
        assert_eq!(
            apply(
                &pool,
                Target::World(&id),
                &Decision::Highlighted(true),
                actor
            )
            .await
            .unwrap(),
            None,
            "a place is not a world"
        );
        assert_eq!(
            apply(
                &pool,
                Target::Place("no-such-place"),
                &Decision::Highlighted(true),
                actor
            )
            .await
            .unwrap(),
            None
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.place_content_ratings WHERE entity_id = $1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 3, "append-only: one row per decision");

        apply(
            &pool,
            Target::Place(&id),
            &Decision::Disabled {
                disabled: true,
                reason: Some("moderation".into()),
            },
            actor,
        )
        .await
        .unwrap();
        let (disabled, _, rating, _, _) = cols(pool.clone(), id.clone()).await;
        assert_eq!(
            (disabled, rating.as_deref()),
            (true, Some("A")),
            "projected at once"
        );

        // Upstream says: enabled, highlighted, rated E. The pass keeps the
        // operator's disable (reason and instant included) and rating and
        // takes upstream's highlight.
        upsert(vec![mirror_row(&id, true, "E")]).await;
        let (disabled, highlighted, rating, reason, at) = cols(pool.clone(), id.clone()).await;
        assert_eq!(
            (disabled, highlighted, rating.as_deref(), reason.as_deref()),
            (true, true, Some("A"), Some("moderation"))
        );
        assert!(at.is_some(), "disabled_at survives the pass");

        let overlay = overlay(&pool, &id).await.unwrap().unwrap();
        assert_eq!(
            (
                overlay["disabled"].as_bool(),
                overlay["content_rating"].as_str(),
                overlay["reason"].as_str()
            ),
            (Some(true), Some("A"), Some("moderation"))
        );
        assert!(overlay["disabled_at"].is_string());
        assert!(overlay["highlighted"].is_null(), "undecided: no override");

        apply(
            &pool,
            Target::Place(&id),
            &Decision::Disabled {
                disabled: false,
                reason: None,
            },
            actor,
        )
        .await
        .unwrap();
        upsert(vec![mirror_row(&id, true, "E")]).await;
        let (disabled, highlighted, rating, reason, at) = cols(pool.clone(), id.clone()).await;
        assert_eq!(
            (disabled, highlighted, rating.as_deref(), reason, at),
            (false, true, Some("A"), None, None),
            "a re-enable is an override too, and the pass clears upstream's disable keys"
        );

        sqlx::query("DELETE FROM explore.place_moderation WHERE place_id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM explore.place_content_ratings WHERE entity_id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM explore.place WHERE id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
    }

    /// A disable and a rating upgrade capture notices in the decision's
    /// transaction; the worker posts them at least once, retries a failure
    /// with backoff, keeps an exhausted row apart from a delivered one and
    /// drops only aged terminal rows.
    #[tokio::test]
    async fn notices_are_captured_durably_and_delivered_by_the_worker() {
        use crate::moderation_hooks::{Endpoints, cleanup, run_once};
        use chrono::NaiveDateTime;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let to = |endpoint: Option<&str>| Endpoints {
            events: None,
            places: endpoint.map(str::to_owned),
        };
        let links = crate::events::hooks::Links {
            base: "https://example.org".into(),
            jump_in_site_url: "https://decentraland.org/jump".into(),
            asset_base: "https://example.org/events".into(),
        };
        // Rewrites the activation singleton: its own database.
        let Some(pool) = crate::db::scratch::isolated_pool(4).await else {
            return;
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let payloads = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let (captured_calls, captured_payloads) = (calls.clone(), payloads.clone());
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let (calls, payloads) = (captured_calls.clone(), captured_payloads.clone());
                async move {
                    payloads.lock().await.push(body);
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/hook", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let id = uuid::Uuid::new_v4().to_string();
        super::super::mirror::upsert_rows(&pool, &[mirror_row(&id, false, "RP")])
            .await
            .unwrap();
        let actor = "0x00000000000000000000000000000000000000c1";
        let notify = Context {
            comment: None,
            notify: Some("https://decentraland.org/places"),
        };
        // No notice without the webhook, none for a highlight, a re-enable or
        // a rating that is not an upgrade.
        apply_with(
            &pool,
            Target::Place(&id),
            &Decision::Highlighted(true),
            actor,
            notify.clone(),
        )
        .await
        .unwrap();
        apply_with(
            &pool,
            Target::Place(&id),
            &Decision::rating("RP").unwrap(),
            actor,
            notify.clone(),
        )
        .await
        .unwrap();
        apply_with(
            &pool,
            Target::Place(&id),
            &Decision::Disabled {
                disabled: false,
                reason: None,
            },
            actor,
            notify.clone(),
        )
        .await
        .unwrap();
        apply_with(
            &pool,
            Target::Place(&id),
            &Decision::rating("T").unwrap(),
            actor,
            Context::default(),
        )
        .await
        .unwrap();
        let queued = |pool: PgPool, id: String| async move {
            sqlx::query_as::<_, (String, Option<NaiveDateTime>, i32)>(
                "SELECT kind, completed_at, attempts FROM explore.moderation_hooks WHERE channel = 'places' AND subject = $1 ORDER BY created_at, kind",
            )
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        assert!(queued(pool.clone(), id.clone()).await.is_empty());

        // An upgrade (T -> A) and a disable.
        let upgraded = apply_with(
            &pool,
            Target::Place(&id),
            &Decision::rating("A").unwrap(),
            actor,
            notify.clone(),
        )
        .await
        .unwrap()
        .unwrap();
        apply_with(
            &pool,
            Target::Place(&id),
            &Decision::Disabled {
                disabled: true,
                reason: Some("moderation".into()),
            },
            actor,
            notify.clone(),
        )
        .await
        .unwrap();
        // A downgrade (A -> T) on the now disabled row: audit, no notice.
        apply_with(
            &pool,
            Target::Place(&id),
            &Decision::rating("T").unwrap(),
            actor,
            notify.clone(),
        )
        .await
        .unwrap();
        let rows = queued(pool.clone(), id.clone()).await;
        assert_eq!(
            rows.iter()
                .map(|(kind, _, _)| kind.as_str())
                .collect::<Vec<_>>(),
            vec!["rating_upgraded", "disabled"]
        );

        let client = crate::http::Outbound::for_tests();
        let now = upgraded.at.naive_utc() + chrono::Duration::seconds(1);
        // First pass: the upgrade's first attempt fails (503), the disable lands.
        assert_eq!(
            run_once(&pool, &client, &to(Some(&endpoint)), &links, now)
                .await
                .unwrap(),
            1
        );
        let rows = queued(pool.clone(), id.clone()).await;
        assert_eq!(
            rows.iter()
                .map(|(k, done, attempts)| (k.as_str(), done.is_some(), *attempts))
                .collect::<Vec<_>>(),
            vec![("rating_upgraded", false, 1), ("disabled", true, 1)]
        );
        // Not due yet: backoff holds it a minute.
        assert_eq!(
            run_once(
                &pool,
                &client,
                &to(Some(&endpoint)),
                &links,
                now + chrono::Duration::seconds(30)
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            run_once(
                &pool,
                &client,
                &to(Some(&endpoint)),
                &links,
                now + chrono::Duration::minutes(2)
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        for payload in payloads.lock().await.iter() {
            assert_eq!(payload["blocks"][0]["text"]["type"], json!("mrkdwn"));
            assert!(
                payload["blocks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .skip(1)
                    .all(|b| b["text"]["type"] == json!("plain_text")),
                "catalogue text is never mrkdwn: {payload}"
            );
        }
        assert!(payloads.lock().await.iter().any(|p| {
            p["blocks"][0]["text"]["text"]
                .as_str()
                .unwrap()
                .starts_with(":x: 1 place disabled in: -7,7")
        }));
        assert!(
            payloads
                .lock()
                .await
                .iter()
                .any(|p| p["blocks"][2]["text"]["text"]
                    == json!("Previous rating: T - New rating: A"))
        );

        // Exhaustion is durable and distinct from completion; no endpoint
        // posts nothing; cleanup drops only aged terminal rows.
        sqlx::query("UPDATE explore.moderation_hooks SET completed_at = NULL, attempts = 5, next_attempt_at = $2 WHERE channel = 'places' AND subject = $1 AND kind = 'rating_upgraded'")
            .bind(&id)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            run_once(
                &pool,
                &client,
                &to(Some("http://127.0.0.1:1/unreachable")),
                &links,
                now + chrono::Duration::minutes(3)
            )
            .await
            .unwrap(),
            0
        );
        let rows = queued(pool.clone(), id.clone()).await;
        assert_eq!(
            rows.iter()
                .map(|(k, done, attempts)| (k.as_str(), done.is_some(), *attempts))
                .collect::<Vec<_>>(),
            vec![("rating_upgraded", false, 6), ("disabled", true, 1)]
        );
        assert_eq!(
            run_once(
                &pool,
                &client,
                &to(None),
                &links,
                now + chrono::Duration::minutes(4)
            )
            .await
            .unwrap(),
            0
        );
        cleanup(&pool, now + chrono::Duration::days(29))
            .await
            .unwrap();
        assert_eq!(
            queued(pool.clone(), id.clone()).await.len(),
            2,
            "retention keeps rows under thirty days"
        );
        // An outbox live for fifty days: a forty-day-old retry is inside its boundary.
        sqlx::query(
            "UPDATE explore.moderation_hook_activation SET activated_at = $1 - interval '50 days'",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO explore.moderation_hooks (id, channel, subject, kind, payload, created_at, next_attempt_at, attempts) VALUES ($1, 'places', $2, 'disabled', '{}', $3 - interval '40 days', $3 - interval '40 days', 2)")
            .bind(Uuid::new_v4())
            .bind(&id)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        cleanup(&pool, now + chrono::Duration::days(31))
            .await
            .unwrap();
        let remaining: Vec<(String, i32)> = sqlx::query_as(
            "SELECT kind, attempts FROM explore.moderation_hooks WHERE channel = 'places' AND subject = $1 ORDER BY attempts",
        )
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            remaining,
            vec![("disabled".into(), 2)],
            "a deliverable retry outlives retention; completed and exhausted rows do not"
        );
        server.abort();
    }
}
