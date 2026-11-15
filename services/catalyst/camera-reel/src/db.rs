//! The statements over `camera_reel.images` (`schemas/50_create.sql`). Each
//! write is one statement so its outcome (owned /
//! not owned / absent, at the cap / under it) comes from one snapshot. The
//! wire `url`/`thumbnailUrl` use each upload's UUID and retained basename.
//! Older rows without a basename keep their hash URLs. Both render under
//! `api_url`, so a changed public base URL never strands a row.
//!
//! Pages: the old `offset` shape as is, plus a keyset shape (`Page::After`)
//! whose cursor is the last row's `(created_at, id)`, the order every page
//! statement uses and the order of `images_user_created_idx` and
//! `images_place_public_created_idx`. Both carry the filtered set's total
//! (the gallery's `currentImages` / `maxImages`), computed once per statement
//! as an uncorrelated subquery; only a page past the end (or `limit` 0) falls
//! back to a bare count.
//!
//! World galleries: the old service paged `PLACES_API_URL/api/places?names=`
//! with a 5-minute memo; here it is a read of `explore.place_indexed` with
//! the places listing's default predicates (`disabled IS FALSE` and the
//! `show_in_places` clause of `explore::places::query`), memoised the same.

use std::time::Duration;

use chrono::NaiveDateTime;
use moka::future::Cache;
use sqlx::types::Json;
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use super::dto::{Image, Metadata};
use crate::shared::cache::{TtlCache, cache_get, cache_put};

const SERVABLE_TTL: Duration = Duration::from_secs(60);

#[cfg(test)]
tokio::task_local! {
    pub(crate) static DISABLE_RANKING_STATEMENT_TIMEOUT: ();
}

pub const TABLE: &str = "camera_reel.images";

const COLUMNS: &str =
    "id, user_address, image_hash, thumbnail_hash, is_public, created_at, metadata, review_status";

#[derive(sqlx::FromRow, Debug)]
pub struct DbImage {
    pub id: Uuid,
    pub user_address: String,
    pub image_hash: String,
    pub thumbnail_hash: String,
    pub url: String,
    pub thumbnail_url: String,
    pub is_public: bool,
    pub created_at: NaiveDateTime,
    pub metadata: Json<Metadata>,
    pub review_status: String,
}

impl DbImage {
    fn with_public_urls(mut self, api_url: &str) -> Self {
        self.url = image_url(api_url, &self.url);
        self.thumbnail_url = image_url(api_url, &self.thumbnail_url);
        self
    }
}

/// What an upload inserts: the two blob hashes and the client's document.
#[derive(Debug, Clone)]
pub struct NewImage {
    pub id: Uuid,
    pub image_hash: String,
    pub thumbnail_hash: String,
    pub is_public: bool,
    pub metadata: Metadata,
}

impl NewImage {
    pub fn wire(&self, api_url: &str, original_filename: Option<&str>) -> Image {
        let (image_name, thumbnail_name) = match original_filename {
            Some(filename) => (
                format!("{}-{filename}", self.id),
                format!("{}-thumbnail-{filename}", self.id),
            ),
            None => (self.image_hash.clone(), self.thumbnail_hash.clone()),
        };
        Image {
            id: self.id.to_string(),
            url: image_url(api_url, &image_name),
            thumbnail_url: image_url(api_url, &thumbnail_name),
            is_public: self.is_public,
            metadata: self.metadata.clone(),
        }
    }
}

/// Upstream concatenates the configured base and decoded object name literally.
/// Reserved characters are part of the wire value, not escaped URL segments.
pub fn image_url(api_url: &str, name: &str) -> String {
    format!("{api_url}/api/images/{name}")
}

/// One exact photo alias selects its hash and original object MIME together.
pub struct ImageBlob {
    pub hash: String,
    pub content_type: Option<String>,
}

/// What one delete reports back: ownership outcome, the hashes the row
/// named (the servable memo drops them; the blobs stay in the shared store),
/// the ones now withheld node-wide and the owner's remaining image count.
#[derive(Debug)]
pub struct DeletedImage {
    pub user_address: String,
    pub image_hash: String,
    pub thumbnail_hash: String,
    pub deleted: bool,
    pub remaining: u64,
    pub withheld: Vec<String>,
}

/// What a review write did to the row's blobs in `camera_reel.withheld_blobs`:
/// the hashes now withheld on every route (a rejection) or released again
/// (a row set back to ok/flagged). The caller mirrors both into the shared
/// in-memory `content::Denylist`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BlobVerdict {
    pub withheld: Vec<String>,
    pub released: Vec<String>,
}

pub const WITHHELD_REJECTED: &str = "rejected";
pub const WITHHELD_DELETED: &str = "deleted";

/// The two hashes of a row, once when the thumbnail is the image.
fn hashes_of(image_hash: &str, thumbnail_hash: &str) -> Vec<String> {
    let mut hashes = vec![image_hash.to_string()];
    if thumbnail_hash != image_hash {
        hashes.push(thumbnail_hash.to_string());
    }
    hashes
}

/// Inside the write's transaction, after the row is rejected or gone:
/// records `hashes` as withheld unless a non-rejected row, a content file
/// or a builder upload still names them.
/// Returns the hashes it recorded. A qualifying rejection promotes an older
/// deletion record; deletion can never downgrade a durable rejection.
async fn withhold(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    image_id: Uuid,
    hashes: &[String],
    reason: &str,
    now: NaiveDateTime,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "INSERT INTO camera_reel.withheld_blobs (hash, image_id, reason, withheld_at) \
         SELECT h.hash, $2, $3, $4 FROM unnest($1::text[]) AS h(hash) \
         WHERE NOT EXISTS (SELECT 1 FROM camera_reel.images i \
                           WHERE (i.image_hash = h.hash OR i.thumbnail_hash = h.hash) AND i.review_status <> 'rejected') \
           AND NOT EXISTS (SELECT 1 FROM content.content_files f WHERE f.content_hash = h.hash) \
           AND NOT EXISTS (SELECT 1 FROM builder.uploaded_contents u WHERE u.hash = h.hash) \
         ON CONFLICT (hash) DO UPDATE \
             SET reason=EXCLUDED.reason, image_id=EXCLUDED.image_id, withheld_at=EXCLUDED.withheld_at \
             WHERE camera_reel.withheld_blobs.reason = 'deleted' AND EXCLUDED.reason = 'rejected' \
         RETURNING hash",
    )
    .bind(hashes)
    .bind(image_id)
    .bind(reason)
    .bind(now)
    .fetch_all(&mut **tx)
    .await
}

/// Inside the write's transaction, after a row is set back to ok/flagged: a
/// non-rejected row names `hashes` again, so nothing withholds them.
async fn release(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    hashes: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "DELETE FROM camera_reel.withheld_blobs WHERE hash = ANY($1) RETURNING hash",
    )
    .bind(hashes)
    .fetch_all(&mut **tx)
    .await
}

/// A keyset position: the last row of the previous page in `(created_at
/// DESC, id DESC)` order. Encoded as `<micros since epoch>.<uuid>`;
/// `timestamp without time zone` is microsecond-precise, so it round-trips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub created_at: NaiveDateTime,
    pub id: Uuid,
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!(
            "{}.{}",
            self.created_at.and_utc().timestamp_micros(),
            self.id.simple()
        )
    }

    pub fn decode(raw: &str) -> Option<Self> {
        let (micros, id) = raw.split_once('.')?;
        let created_at = chrono::DateTime::from_timestamp_micros(micros.parse().ok()?)?.naive_utc();
        Some(Self {
            created_at,
            id: Uuid::parse_str(id).ok()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    /// The old API: skip `offset` rows.
    Offset(i64),
    /// The keyset API: rows after the cursor (`None` for the first page).
    After(Option<Cursor>),
}

#[derive(Debug)]
pub struct ImagePage {
    pub images: Vec<DbImage>,
    pub total: u64,
    /// `None` on an offset request; `Some(None)` when a keyset page is the
    /// last one, `Some(Some(cursor))` otherwise.
    pub next: Option<Option<Cursor>>,
}

#[derive(Clone)]
pub struct Database {
    pool: PgPool,
    api_url: String,
    servable: std::sync::Arc<TtlCache<bool>>,
    world_places: Cache<String, Vec<String>>,
}

fn parse_uuid(uuid: &str) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(uuid).map_err(|_| sqlx::Error::Protocol("Invalid UUID".to_string()))
}

enum Filter<'a> {
    UserAddress(&'a str),
    PlaceId(&'a str),
    PlacesIds(&'a [String]),
    /// A `<contract>-<itemId>` the caller has validated; matched on the
    /// expression `images_wearable_items_idx` is built on.
    WearableItem(&'a str),
}

impl Database {
    pub fn new(
        pool: PgPool,
        api_url: impl Into<String>,
        places_cache_ttl_seconds: u64,
        places_cache_max_size: u64,
    ) -> Self {
        Self {
            pool,
            api_url: api_url.into(),
            servable: Default::default(),
            world_places: Cache::builder()
                .max_capacity(places_cache_max_size)
                .time_to_live(Duration::from_secs(places_cache_ttl_seconds))
                .build(),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    fn select(&self) -> QueryBuilder<Postgres> {
        let mut qb = QueryBuilder::new(format!("SELECT {COLUMNS}, "));
        qb.push("CASE WHEN original_filename IS NULL THEN image_hash ELSE id::text || '-' || original_filename END AS url, ")
            .push("CASE WHEN original_filename IS NULL THEN thumbnail_hash ELSE id::text || '-thumbnail-' || original_filename END AS thumbnail_url");
        qb
    }

    pub async fn get_image(&self, id: &str) -> Result<DbImage, sqlx::Error> {
        let mut qb = self.select();
        qb.push(format!(" FROM {TABLE} WHERE id = "))
            .push_bind(parse_uuid(id)?);
        qb.build_query_as::<DbImage>()
            .fetch_one(&self.pool)
            .await
            .map(|image| image.with_public_urls(&self.api_url))
    }

    fn push_filter(
        qb: &mut QueryBuilder<Postgres>,
        filter: &Filter<'_>,
        public_only: bool,
    ) -> Result<(), sqlx::Error> {
        qb.push(" WHERE ");
        match filter {
            Filter::UserAddress(addr) => {
                qb.push("user_address = ");
                qb.push_bind(addr.to_lowercase());
            }
            Filter::PlaceId(place_id) => {
                qb.push("place_id = ");
                qb.push_bind(parse_uuid(place_id)?.to_string());
            }
            Filter::PlacesIds(ids) => {
                qb.push("place_id = ANY(");
                qb.push_bind(ids.to_vec());
                qb.push(")");
            }
            Filter::WearableItem(item) => {
                qb.push("camera_reel.image_wearable_items(metadata) && ARRAY[");
                qb.push_bind(item.to_lowercase());
                qb.push("]::text[]");
            }
        }
        if public_only {
            qb.push(" AND is_public = true AND review_status <> 'rejected'");
        }
        Ok(())
    }

    async fn get_images_count(
        &self,
        filter: &Filter<'_>,
        public_only: bool,
    ) -> Result<u64, sqlx::Error> {
        let mut qb = QueryBuilder::new(format!("SELECT COUNT(*) FROM {TABLE}"));
        Self::push_filter(&mut qb, filter, public_only)?;
        let count = qb.build_query_scalar::<i64>().fetch_one(&self.pool).await?;
        Ok(count as u64)
    }

    /// Page plus total in one statement; only a page past the end (or `limit`
    /// 0) falls back to a bare COUNT.
    async fn get_images_page(
        &self,
        filter: Filter<'_>,
        page: &Page,
        limit: i64,
        public_only: bool,
    ) -> Result<ImagePage, sqlx::Error> {
        let mut qb = self.select();
        qb.push(format!(", (SELECT COUNT(*) FROM {TABLE}"));
        Self::push_filter(&mut qb, &filter, public_only)?;
        qb.push(format!(") AS total FROM {TABLE}"));
        Self::push_filter(&mut qb, &filter, public_only)?;
        if let Page::After(Some(cursor)) = page {
            qb.push(" AND (created_at, id) < (")
                .push_bind(cursor.created_at)
                .push(", ")
                .push_bind(cursor.id)
                .push(")");
        }
        qb.push(" ORDER BY created_at DESC, id DESC LIMIT ")
            .push_bind(limit);
        if let Page::Offset(offset) = page {
            qb.push(" OFFSET ").push_bind(*offset);
        }
        let rows = qb.build().fetch_all(&self.pool).await?;
        let total = match rows.first() {
            Some(first) => first.try_get::<i64, _>("total")? as u64,
            None => self.get_images_count(&filter, public_only).await?,
        };
        let images = rows
            .iter()
            .map(|row| DbImage::from_row(row).map(|image| image.with_public_urls(&self.api_url)))
            .collect::<Result<Vec<_>, _>>()?;
        let next = match page {
            Page::Offset(_) => None,
            Page::After(_) => Some(
                images
                    .last()
                    .filter(|_| limit > 0 && images.len() as i64 >= limit)
                    .map(|last| Cursor {
                        created_at: last.created_at,
                        id: last.id,
                    }),
            ),
        };
        Ok(ImagePage {
            images,
            total,
            next,
        })
    }

    pub async fn get_user_images_page(
        &self,
        user: &str,
        page: &Page,
        limit: i64,
        public_only: bool,
    ) -> Result<ImagePage, sqlx::Error> {
        self.get_images_page(Filter::UserAddress(user), page, limit, public_only)
            .await
    }

    pub async fn get_user_images_count(
        &self,
        user: &str,
        public_only: bool,
    ) -> Result<u64, sqlx::Error> {
        self.get_images_count(&Filter::UserAddress(user), public_only)
            .await
    }

    pub async fn get_place_images_page(
        &self,
        place_id: &str,
        page: &Page,
        limit: i64,
    ) -> Result<ImagePage, sqlx::Error> {
        self.get_images_page(Filter::PlaceId(place_id), page, limit, true)
            .await
    }

    pub async fn get_multiple_places_images_page(
        &self,
        places_ids: &[String],
        page: &Page,
        limit: i64,
    ) -> Result<ImagePage, sqlx::Error> {
        self.get_images_page(Filter::PlacesIds(places_ids), page, limit, true)
            .await
    }

    pub async fn get_wearable_images_count(&self, item: &str) -> Result<u64, sqlx::Error> {
        self.get_images_count(&Filter::WearableItem(item), true)
            .await
    }

    /// Public, non-rejected photos in which somebody wears `item`
    /// (`<contract>-<itemId>`), newest first; public whoever asks, since
    /// these are photos of other people offered for an item.
    pub async fn get_wearable_images_page(
        &self,
        item: &str,
        offset: i64,
        limit: i64,
    ) -> Result<ImagePage, sqlx::Error> {
        self.get_images_page(
            Filter::WearableItem(item),
            &Page::Offset(offset),
            limit,
            true,
        )
        .await
    }

    pub async fn insert_image(&self, image: &NewImage) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO camera_reel.images (id, user_address, image_hash, thumbnail_hash, is_public, metadata) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(image.id)
        .bind(image.metadata.user_address.to_lowercase())
        .bind(&image.image_hash)
        .bind(&image.thumbnail_hash)
        .bind(image.is_public)
        .bind(Json(&image.metadata))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Inserts only while the owner is under `max_images`; `None` means the
    /// limit held. The returned count already includes the new row. The
    /// owner's advisory lock serialises two uploads racing at the cap (the
    /// count and the insert see one snapshot each; without the lock both
    /// could pass the count).
    pub async fn insert_image_within_limit(
        &self,
        image: &NewImage,
        max_images: u64,
    ) -> Result<Option<u64>, sqlx::Error> {
        self.insert_image_within_limit_named(image, max_images, None, None)
            .await
    }

    pub async fn insert_image_within_limit_named(
        &self,
        image: &NewImage,
        max_images: u64,
        original_filename: Option<&str>,
        image_content_type: Option<&str>,
    ) -> Result<Option<u64>, sqlx::Error> {
        let user = image.metadata.user_address.to_lowercase();
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(&user)
            .execute(&mut *tx)
            .await?;
        let count = sqlx::query_scalar::<_, i64>(
            "WITH ins AS ( \
                 INSERT INTO camera_reel.images (id, user_address, image_hash, thumbnail_hash, is_public, metadata, ranking_published, original_filename, image_content_type) \
                 SELECT $1, $2, $3, $4, $5, $6, false, $8, $9 \
                 WHERE (SELECT COUNT(*) FROM camera_reel.images WHERE user_address = $2) < $7 \
                 RETURNING id \
             ) \
             SELECT (SELECT COUNT(*) FROM camera_reel.images WHERE user_address = $2) + 1 FROM ins",
        )
        .bind(image.id)
        .bind(&user)
        .bind(&image.image_hash)
        .bind(&image.thumbnail_hash)
        .bind(image.is_public)
        .bind(Json(&image.metadata))
        .bind(max_images.min(i64::MAX as u64) as i64)
        .bind(original_filename)
        .bind(image_content_type)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(count.map(|c| c as u64))
    }

    /// A new upload may reuse bytes whose last photo was deleted. Only lift
    /// deletion withholding after both blobs exist; a rejection remains durable.
    /// Lock this live row so its own concurrent deletion/review cannot invalidate
    /// the references checked by this transaction.
    pub(crate) async fn release_deleted_upload(
        &self,
        id: Uuid,
    ) -> Result<BlobVerdict, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT image_hash, thumbnail_hash FROM camera_reel.images \
             WHERE id=$1 AND review_status <> 'rejected' FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((image_hash, thumbnail_hash)) = row else {
            return Ok(BlobVerdict {
                withheld: Vec::new(),
                released: Vec::new(),
            });
        };
        let hashes = hashes_of(&image_hash, &thumbnail_hash);
        let released = sqlx::query_scalar::<_, String>(
            "DELETE FROM camera_reel.withheld_blobs w \
             WHERE w.hash = ANY($1) AND w.reason = 'deleted' \
               AND EXISTS (SELECT 1 FROM camera_reel.images i \
                           WHERE (i.image_hash=w.hash OR i.thumbnail_hash=w.hash) AND i.review_status <> 'rejected') \
               AND NOT EXISTS (SELECT 1 FROM camera_reel.images i \
                               WHERE (i.image_hash=w.hash OR i.thumbnail_hash=w.hash) AND i.review_status = 'rejected') \
             RETURNING w.hash",
        ).bind(&hashes).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        self.forget_servable(&image_hash);
        self.forget_servable(&thumbnail_hash);
        Ok(BlobVerdict {
            withheld: Vec::new(),
            released,
        })
    }

    /// Blob I/O finishes before this transaction. The marker, local receipt,
    /// all attached communities' counters and the `photo-taken` event
    /// ([`crate::events`]) either commit together or remain due.
    pub async fn publish_ranking(&self, id: Uuid) -> Result<(), sqlx::Error> {
        self.publish_ranking_before(id, None).await
    }

    pub(crate) async fn publish_ranking_before(
        &self,
        id: Uuid,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        if let Some(deadline) = deadline {
            // Cancel the actual server statement too: dropping a SQLx future
            // alone can leave a locked query occupying its pooled connection.
            let milliseconds = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_millis()
                .max(1);
            #[cfg(test)]
            let milliseconds = if DISABLE_RANKING_STATEMENT_TIMEOUT.try_with(|()| ()).is_ok() {
                0
            } else {
                milliseconds
            };
            sqlx::query("SELECT set_config('statement_timeout',$1,true)")
                .bind(format!("{milliseconds}ms"))
                .execute(&mut *tx)
                .await?;
        }
        let row: Option<(String, NaiveDateTime, String, bool, Json<Metadata>)> = sqlx::query_as(
            "UPDATE camera_reel.images SET ranking_published=true WHERE id=$1 AND NOT ranking_published RETURNING COALESCE(place_id,''),created_at,user_address,is_public,metadata")
            .bind(id).fetch_optional(&mut *tx).await?;
        if let Some((place, occurred_at, owner, is_public, Json(metadata))) = row {
            if !place.is_empty() {
                crate::social::communities::ranking::record_photo(
                    &mut tx,
                    &id.to_string(),
                    &place,
                    occurred_at,
                )
                .await?;
            }
            let payload = crate::events::photo_taken(
                id,
                is_public,
                &metadata,
                occurred_at.and_utc().timestamp(),
            );
            crate::events::record(&mut *tx, crate::events::PHOTO_TAKEN, id, &owner, &payload)
                .await?;
        }
        tx.commit().await
    }

    pub async fn pending_rankings(
        &self,
        after: Uuid,
    ) -> Result<Vec<(Uuid, String, String, NaiveDateTime)>, sqlx::Error> {
        // Bound the pending-key read before the caller applies its frozen age cohort.
        sqlx::query_as("SELECT id,image_hash,thumbnail_hash,created_at FROM camera_reel.images WHERE NOT ranking_published AND id>$1 ORDER BY id LIMIT 100")
            .bind(after).fetch_all(&self.pool).await
    }

    pub async fn delete_image(&self, id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM camera_reel.images WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// One transaction: deletes the row (only the owner's when `owner` is
    /// given) and reports, from the same snapshot, the hashes it named and
    /// how many images the owner has left, then records the hashes nothing
    /// else names in `withheld_blobs` (`WITHHELD_DELETED`). `None` is "no
    /// such image"; `deleted == false` is an ownership mismatch, with nothing
    /// removed. The blobs are never unlinked: the store is shared (module
    /// docs); the withheld record is what keeps them off every route.
    pub async fn delete_image_owned(
        &self,
        id: &str,
        owner: Option<&str>,
        now: NaiveDateTime,
    ) -> Result<Option<DeletedImage>, sqlx::Error> {
        let Ok(id) = Uuid::parse_str(id) else {
            return Ok(None);
        };
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (String, String, String, bool, i64)>(
            "WITH target AS ( \
                 SELECT id, user_address, image_hash, thumbnail_hash FROM camera_reel.images WHERE id = $1 \
             ), del AS ( \
                 DELETE FROM camera_reel.images \
                 WHERE id = $1 AND ($2::text IS NULL OR user_address = $2) \
                 RETURNING id \
             ) \
             SELECT t.user_address, t.image_hash, t.thumbnail_hash, \
                    EXISTS (SELECT 1 FROM del) AS deleted, \
                    (SELECT COUNT(*) FROM camera_reel.images o WHERE o.user_address = t.user_address) - 1 AS remaining \
             FROM target t",
        )
        .bind(id)
        .bind(owner.map(str::to_lowercase))
        .fetch_optional(&mut *tx)
        .await?;
        let Some((user_address, image_hash, thumbnail_hash, deleted, remaining)) = row else {
            return Ok(None);
        };
        let withheld = if deleted {
            withhold(
                &mut tx,
                id,
                &hashes_of(&image_hash, &thumbnail_hash),
                WITHHELD_DELETED,
                now,
            )
            .await?
        } else {
            Vec::new()
        };
        tx.commit().await?;
        if deleted {
            self.forget_servable(&image_hash);
            self.forget_servable(&thumbnail_hash);
        }
        Ok(Some(DeletedImage {
            user_address,
            image_hash,
            thumbnail_hash,
            deleted,
            remaining: remaining.max(0) as u64,
            withheld,
        }))
    }

    /// A hash is served only while a non-rejected row names it as image or
    /// thumbnail. Deviation from the old service, which served a hash no row
    /// referenced: over the shared store that rule would expose any content
    /// file. Memoised for `SERVABLE_TTL`; review and delete writes drop the
    /// entry.
    pub async fn hash_is_servable(&self, hash: &str) -> Result<bool, sqlx::Error> {
        if let Some(servable) = cache_get(&self.servable, hash) {
            return Ok(servable);
        }
        let servable = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM camera_reel.images \
                            WHERE (image_hash = $1 OR thumbnail_hash = $1) AND review_status <> 'rejected')",
        )
        .bind(hash)
        .fetch_one(&self.pool)
        .await?;
        cache_put(&self.servable, hash.to_string(), servable, SERVABLE_TTL);
        Ok(servable)
    }

    /// Resolve only exact stored photo aliases, or a legacy hash a photo still
    /// references. The UUID prefix uses the existing primary key; arbitrary
    /// shared-storage hashes never become readable through this route.
    pub async fn image_blob_for_name(&self, name: &str) -> Result<Option<ImageBlob>, sqlx::Error> {
        if let Some((id, suffix)) = name
            .get(..36)
            .and_then(|id| Uuid::parse_str(id).ok())
            .zip(name.get(36..).and_then(|suffix| suffix.strip_prefix('-')))
        {
            if !name.starts_with(&id.to_string()) {
                return Ok(None);
            }
            let row: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT image_hash, thumbnail_hash, original_filename, image_content_type FROM camera_reel.images \
                 WHERE id=$1 AND review_status <> 'rejected'",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
            if let Some((image_hash, thumbnail_hash, Some(filename), content_type)) = row {
                if suffix == filename {
                    return Ok(Some(ImageBlob {
                        hash: image_hash,
                        content_type,
                    }));
                }
                if suffix == format!("thumbnail-{filename}") {
                    return Ok(Some(ImageBlob {
                        hash: thumbnail_hash,
                        content_type,
                    }));
                }
            }
            return Ok(None);
        }
        Ok(self.hash_is_servable(name).await?.then(|| ImageBlob {
            hash: name.to_string(),
            content_type: None,
        }))
    }

    fn forget_servable(&self, hash: &str) {
        self.servable.lock().unwrap().remove(hash);
    }

    /// `None`: no such image. `Some(false)`: not the owner's, unchanged.
    /// `Some(true)`: owned, and now at `is_public` (a no-op when it already
    /// was). A change records `photo-privacy-changed` ([`crate::events`]) in
    /// the same statement; a no-op records nothing.
    pub async fn set_image_visibility(
        &self,
        id: &str,
        owner: &str,
        is_public: bool,
    ) -> Result<Option<bool>, sqlx::Error> {
        let Ok(id) = Uuid::parse_str(id) else {
            return Ok(None);
        };
        let owner = owner.to_lowercase();
        let payload = crate::events::photo_privacy_changed(
            id,
            &owner,
            is_public,
            chrono::Utc::now().timestamp(),
        );
        sqlx::query_scalar::<_, bool>(
            "WITH target AS ( \
                 SELECT user_address FROM camera_reel.images WHERE id = $1 \
             ), upd AS ( \
                 UPDATE camera_reel.images SET is_public = $3 \
                 WHERE id = $1 AND user_address = $2 AND is_public <> $3 \
                 RETURNING id \
             ), ev AS ( \
                 INSERT INTO camera_reel.photo_events (kind, photo_id, user_address, payload) \
                 SELECT $4, id, $2, $5 FROM upd \
             ) \
             SELECT user_address = $2 FROM target",
        )
        .bind(id)
        .bind(&owner)
        .bind(is_public)
        .bind(crate::events::PHOTO_PRIVACY_CHANGED)
        .bind(Json(&payload))
        .fetch_optional(&self.pool)
        .await
    }

    /// One transaction: the moderator's verdict on the row, then its blobs'
    /// (`withheld_blobs`): `rejected` withholds the hashes nothing else
    /// names, any other status releases them. `None` is "no such image".
    pub async fn update_image_review_status(
        &self,
        id: &str,
        review_status: &str,
        now: NaiveDateTime,
    ) -> Result<Option<BlobVerdict>, sqlx::Error> {
        let id = parse_uuid(id)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (String, String)>(
            "UPDATE camera_reel.images SET review_status = $1 WHERE id = $2 RETURNING image_hash, thumbnail_hash",
        )
        .bind(review_status)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((image_hash, thumbnail_hash)) = row else {
            return Ok(None);
        };
        let hashes = hashes_of(&image_hash, &thumbnail_hash);
        let verdict = if review_status == WITHHELD_REJECTED {
            BlobVerdict {
                withheld: withhold(&mut tx, id, &hashes, WITHHELD_REJECTED, now).await?,
                released: Vec::new(),
            }
        } else {
            BlobVerdict {
                withheld: Vec::new(),
                released: release(&mut tx, &hashes).await?,
            }
        };
        tx.commit().await?;
        self.forget_servable(&image_hash);
        self.forget_servable(&thumbnail_hash);
        Ok(Some(verdict))
    }

    /// Every withheld hash, for the boot load of the shared denylist: the
    /// whole relation, sized by moderation and deletes, read once per
    /// process.
    pub async fn withheld_hashes(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>("SELECT hash FROM camera_reel.withheld_blobs ORDER BY hash")
            .fetch_all(&self.pool)
            .await
    }

    /// The place ids of a world name (`name.dcl.eth`), the places listing's
    /// `?names=` answer as a local read, memoised with the configured TTL:
    /// the world's scenes, not its catalogue row (`id = lower(world_name)`),
    /// which the places API leaves out of that answer too.
    pub async fn world_place_ids(&self, world_name: &str) -> Result<Vec<String>, sqlx::Error> {
        // Cache identity follows the exact request, independently of the
        // case-insensitive catalogue lookup.
        let key = world_name.to_owned();
        if let Some(ids) = self.world_places.get(&key).await {
            return Ok(ids);
        }
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM explore.place_indexed \
             WHERE disabled IS FALSE AND world IS TRUE AND lower(world_name) = $1 \
               AND id <> lower(world_name) \
               AND (world IS FALSE OR COALESCE(NULLIF(raw->'show_in_places', 'null'::jsonb) = 'true'::jsonb, true) IS TRUE) \
             ORDER BY id",
        )
        .bind(world_name.to_lowercase())
        .fetch_all(&self.pool)
        .await?;
        self.world_places.insert(key, ids.clone()).await;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn world_lookup_cache_obeys_configured_capacity_after_maintenance() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/camera_cache_test")
            .unwrap();
        let db = Database::new(pool, "https://camera.example", 300, 2);
        for n in 0..20 {
            db.world_places.insert(format!("{n}.eth"), vec![]).await;
        }
        db.world_places.run_pending_tasks().await;
        assert!(db.world_places.entry_count() <= 2);
        assert!(db.world_places.entry_count() > 0);
    }

    #[test]
    fn cursor_round_trips_microseconds_and_id() {
        let created_at = chrono::DateTime::from_timestamp_micros(1_700_000_000_123_456)
            .unwrap()
            .naive_utc();
        let cursor = Cursor {
            created_at,
            id: Uuid::new_v4(),
        };
        let encoded = cursor.encode();
        assert_eq!(Cursor::decode(&encoded), Some(cursor));
        assert_eq!(Cursor::decode("garbage"), None);
        assert_eq!(Cursor::decode("12.not-a-uuid"), None);
        assert_eq!(Cursor::decode(&format!("x.{}", Uuid::new_v4())), None);
    }

    #[test]
    fn urls_render_from_the_hash_under_the_api_url() {
        assert_eq!(
            image_url("https://x.example/camera-reel-service/", "bafk"),
            "https://x.example/camera-reel-service//api/images/bafk"
        );
    }
}
