//! Collection and item drafts: the wire shapes the Creator Hub saves, their
//! validation (the owner in the body must be the signer, no draft write can
//! claim publication or approval), the owner-scoped listings and the guarded
//! upserts. A draft write is refused once its collection is publishing or
//! published (`publication_pending` / `is_published`, the parent rows taken
//! `FOR UPDATE` first so a waiting writer re-reads the committed lock), and
//! an item's owner always equals its collection's. Uploaded files go to the
//! ONE blob store under their CIDv1 and `builder.uploaded_contents` keeps the
//! acceptance record; a draft can only name a hash this node accepted (the
//! `item_contents.hash` FK, the old "upload before saving" rule).

use std::collections::BTreeMap;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::storage::OpenedContent;
use dcl_auth::{hash_bytes_v1, is_canonical_cid, is_eth_address};

use super::http::ApiError;
use super::items::{CollectionMetaOut, CollectionMetaRow, ItemsComponent};

#[derive(Debug, sqlx::FromRow)]
struct CollectionListingRow {
    #[sqlx(flatten)]
    collection: CollectionMetaRow,
    item_count: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "builder/"))]
pub struct CollectionListingOut {
    #[serde(flatten)]
    pub collection: CollectionMetaOut,
    pub item_count: i64,
}

#[derive(Debug, Deserialize)]
pub struct CollectionDraft {
    pub id: Uuid,
    pub name: String,
    pub eth_address: Option<String>,
    pub salt: Option<String>,
    pub contract_address: Option<String>,
    pub urn: Option<String>,
    #[serde(default)]
    pub is_published: bool,
    #[serde(default)]
    pub is_approved: bool,
}

#[derive(Debug, Deserialize)]
pub struct ItemDraft {
    pub id: Uuid,
    pub name: String,
    pub collection_id: Option<Uuid>,
    pub eth_address: Option<String>,
    #[serde(default)]
    pub description: String,
    pub thumbnail: Option<String>,
    #[serde(rename = "type")]
    pub item_type: String,
    pub rarity: Option<String>,
    pub price: Option<String>,
    pub beneficiary: Option<String>,
    #[serde(default)]
    pub data: Value,
    pub metrics: Option<Value>,
    #[serde(default)]
    pub contents: BTreeMap<String, String>,
    #[serde(default)]
    pub is_published: bool,
    #[serde(default)]
    pub is_approved: bool,
}

fn validate_owner(owner: Option<&str>, signer: &str) -> Result<(), ApiError> {
    if owner.is_some_and(|owner| !owner.eq_ignore_ascii_case(signer)) {
        return Err(ApiError::forbidden(
            "The draft owner must match the signing wallet",
        ));
    }
    Ok(())
}

fn validate_name(name: &str, max: usize) -> Result<(), ApiError> {
    if name.trim().is_empty() || name.encode_utf16().count() > max {
        return Err(ApiError::bad_request(format!(
            "Name must contain 1 to {max} characters"
        )));
    }
    Ok(())
}

impl CollectionDraft {
    pub fn validate(&self, id: Uuid, signer: &str) -> Result<(), ApiError> {
        if self.id != id {
            return Err(ApiError::bad_request(
                "The body and URL collection ids do not match",
            ));
        }
        validate_name(&self.name, 42)?;
        validate_owner(self.eth_address.as_deref(), signer)?;
        if let Some(urn) = &self.urn {
            let parts: Vec<_> = urn.split(':').collect();
            if urn.len() > 512
                || parts.len() != 6
                || parts[..4] != ["urn", "decentraland", "matic", "collections-thirdparty"]
                || parts[4..].iter().any(|part| {
                    part.is_empty() || part.chars().any(|c| c.is_whitespace() || c == '|')
                })
            {
                return Err(ApiError::bad_request("Invalid linked collection URN"));
            }
        }
        if self.is_published || self.is_approved || self.contract_address.is_some() {
            return Err(ApiError::bad_request(
                "Publication requires a verified blockchain receipt",
            ));
        }
        if self.salt.as_ref().is_some_and(|salt| salt.len() > 128) {
            return Err(ApiError::bad_request("Invalid collection salt"));
        }
        Ok(())
    }
}

impl ItemDraft {
    pub fn validate(&self, id: Uuid, signer: &str) -> Result<(), ApiError> {
        if self.id != id {
            return Err(ApiError::bad_request(
                "The body and URL item ids do not match",
            ));
        }
        validate_name(&self.name, 64)?;
        validate_owner(self.eth_address.as_deref(), signer)?;
        if !matches!(self.item_type.as_str(), "wearable" | "emote") || !self.data.is_object() {
            return Err(ApiError::bad_request("Invalid item type or data"));
        }
        if self.is_published || self.is_approved {
            return Err(ApiError::bad_request(
                "Draft writes cannot change publication or approval",
            ));
        }
        if self.metrics.as_ref().is_some_and(|metrics| {
            !metrics.is_object()
                || metrics.to_string().len() > 1024
                || metrics.as_object().is_some_and(|fields| {
                    fields
                        .values()
                        .any(|v| v.as_f64().is_none_or(|n| n < 0.0 || !n.is_finite()))
                })
        }) {
            return Err(ApiError::bad_request("Invalid item metrics"));
        }
        if self.description.len() > 4096
            || self.data.to_string().len() > 128 * 1024
            || self.contents.len() > 100
        {
            return Err(ApiError::bad_request("Item metadata is too large"));
        }
        if self.price.as_ref().is_some_and(|price| {
            price.is_empty() || price.len() > 78 || !price.bytes().all(|b| b.is_ascii_digit())
        }) {
            return Err(ApiError::bad_request(
                "Price must be an integer amount in MANA wei",
            ));
        }
        if self
            .beneficiary
            .as_ref()
            .is_some_and(|address| !is_eth_address(address))
        {
            return Err(ApiError::bad_request("Invalid beneficiary address"));
        }
        if self.rarity.as_deref().is_some_and(|rarity| {
            !matches!(
                rarity,
                "unique"
                    | "mythic"
                    | "exotic"
                    | "legendary"
                    | "epic"
                    | "rare"
                    | "uncommon"
                    | "common"
            )
        }) {
            return Err(ApiError::bad_request("Invalid rarity"));
        }
        for (file, hash) in &self.contents {
            if !valid_filename(file) || !is_canonical_cid(hash) {
                return Err(ApiError::bad_request("Invalid content filename or CID"));
            }
        }
        Ok(())
    }
}

pub fn valid_filename(file: &str) -> bool {
    !file.is_empty()
        && file.len() <= 256
        && !file.contains(['\\', '\0'])
        && file
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The item's `contents` document with the timestamps rendered as the old
/// timestamptz columns were (`+00:00`), the item's own row otherwise.
pub(super) const ITEM_DRAFT_PROJECTION: &str = "to_jsonb(i) || jsonb_build_object('urn', i.urn_suffix, \
    'created_at', i.created_at AT TIME ZONE 'UTC', 'updated_at', i.updated_at AT TIME ZONE 'UTC', 'contents', \
    COALESCE((SELECT jsonb_object_agg(file, hash) FROM builder.item_contents WHERE item_id = i.id), '{}'::jsonb))";

const FK_VIOLATION: &str = "23503";

/// The bound of the committee-wide and the public listings: the per-owner
/// ones are a wallet's own handful, these span every wallet.
pub const LISTING_LIMIT: i64 = 1000;

const COLLECTION_LISTING: &str = "SELECT c.id, c.name, c.eth_address, c.contract_address, c.urn_suffix, c.third_party_id, \
    c.is_published, c.is_approved, c.created_at, c.updated_at, \
    (SELECT count(*) FROM builder.items i WHERE i.collection_id=c.id) AS item_count FROM builder.collections c";
const DRAFT_PREDICATE: &str = "(NOT c.is_published OR c.third_party_id IS NOT NULL)";
const ITEM_DRAFT_LISTING: &str = "SELECT jsonb_build_object('id', id, 'name', name, 'type', type, 'collection_id', collection_id) FROM builder.items";
const ITEM_DRAFT_PREDICATE: &str = "NOT EXISTS (SELECT 1 FROM builder.collections c WHERE c.id=items.collection_id \
    AND (c.is_published OR c.publication_pending))";

fn collection_listing(rows: &[CollectionListingRow]) -> Vec<CollectionListingOut> {
    rows.iter()
        .map(|row| CollectionListingOut {
            collection: CollectionMetaOut::from(&row.collection),
            item_count: row.item_count,
        })
        .collect()
}

impl ItemsComponent {
    pub async fn save_collection_draft(
        &self,
        signer: &str,
        draft: &CollectionDraft,
    ) -> Result<CollectionMetaOut, ApiError> {
        draft.validate(draft.id, signer)?;
        let changed = sqlx::query(
            "INSERT INTO builder.collections (id, name, eth_address, salt, urn_suffix, third_party_id) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, salt = EXCLUDED.salt, updated_at = timezone('UTC', now()) \
             WHERE collections.eth_address = $3 AND NOT collections.is_published AND NOT collections.publication_pending \
             AND collections.urn_suffix IS NOT DISTINCT FROM $5 AND collections.third_party_id IS NOT DISTINCT FROM $6 \
             RETURNING id",
        ).bind(draft.id).bind(draft.name.trim()).bind(signer).bind(&draft.salt)
            .bind(&draft.urn).bind(draft.urn.as_deref().and_then(|urn| urn.rsplit_once(':').map(|(provider, _)| provider)))
            .fetch_optional(&self.pool).await?;
        if changed.is_none() {
            return Err(ApiError::forbidden(
                "Collection is publishing, published, or belongs to another wallet",
            ));
        }
        let row = self
            .collection_by_id(&draft.id)
            .await?
            .ok_or_else(|| ApiError::not_found("Collection not found"))?;
        Ok(CollectionMetaOut::from(&row))
    }

    /// "My collections": the owner's unpublished collections and published
    /// linked ones (`collections_owner_idx`).
    pub async fn collection_drafts(
        &self,
        signer: &str,
    ) -> Result<Vec<CollectionListingOut>, ApiError> {
        let rows = sqlx::query_as::<_, CollectionListingRow>(sqlx::AssertSqlSafe(format!(
            "{COLLECTION_LISTING} WHERE c.eth_address = $1 AND {DRAFT_PREDICATE} ORDER BY c.updated_at DESC"
        )))
        .bind(signer)
        .fetch_all(&self.pool)
        .await?;
        Ok(collection_listing(&rows))
    }

    /// Every wallet's drafts, newest first: the committee's `GET
    /// /v1/collections` (builder-server's `getCollections` without the
    /// published search), at most [`LISTING_LIMIT`] rows.
    pub async fn collection_drafts_of_everyone(
        &self,
    ) -> Result<Vec<CollectionListingOut>, ApiError> {
        let rows = sqlx::query_as::<_, CollectionListingRow>(sqlx::AssertSqlSafe(format!(
            "{COLLECTION_LISTING} WHERE {DRAFT_PREDICATE} ORDER BY c.updated_at DESC LIMIT $1"
        )))
        .bind(LISTING_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        Ok(collection_listing(&rows))
    }

    /// The published collections whose name contains `q` (case-insensitive,
    /// the pattern characters of `q` literal), newest first: the public
    /// search `GET /v1/collections?q=...&is_published=true`, at most
    /// [`LISTING_LIMIT`] rows.
    pub async fn published_collections_named(
        &self,
        q: &str,
    ) -> Result<Vec<CollectionListingOut>, ApiError> {
        let pattern = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let rows = sqlx::query_as::<_, CollectionListingRow>(sqlx::AssertSqlSafe(format!(
            "{COLLECTION_LISTING} WHERE c.is_published AND c.name ILIKE '%' || $1 || '%' ESCAPE '\\' \
             ORDER BY c.updated_at DESC LIMIT $2"
        )))
        .bind(pattern)
        .bind(LISTING_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        Ok(collection_listing(&rows))
    }

    /// "My item drafts" (`items_owner_updated_idx`).
    pub async fn item_drafts(&self, signer: &str) -> Result<Vec<Value>, ApiError> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "{ITEM_DRAFT_LISTING} WHERE eth_address = $1 AND NOT is_published AND {ITEM_DRAFT_PREDICATE} ORDER BY updated_at DESC"
        )))
        .bind(signer)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every wallet's item drafts, newest first: the committee's `GET
    /// /v1/items` (builder-server's `getItems`), at most [`LISTING_LIMIT`].
    pub async fn item_drafts_of_everyone(&self) -> Result<Vec<Value>, ApiError> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "{ITEM_DRAFT_LISTING} WHERE NOT is_published AND {ITEM_DRAFT_PREDICATE} ORDER BY updated_at DESC LIMIT $1"
        )))
        .bind(LISTING_LIMIT)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn item_draft(&self, signer: &str, id: Uuid) -> Result<Value, ApiError> {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {ITEM_DRAFT_PROJECTION} FROM builder.items i WHERE i.id = $1 AND i.eth_address = $2"
        )))
        .bind(id)
        .bind(signer)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("Item not found"))
    }

    /// One owned, unpublished item by primary key. Use publication's parent-then-item
    /// lock order; cascading content links go away, shared uploaded bytes remain.
    pub async fn delete_item_draft(&self, signer: &str, id: Uuid) -> Result<(), ApiError> {
        let mut tx = self.pool.begin().await?;
        lock_item_draft(&mut tx, signer, id, None).await?;
        let deleted = sqlx::query(
            "DELETE FROM builder.items WHERE id=$1 AND eth_address=$2 AND NOT is_published",
        )
        .bind(id)
        .bind(signer)
        .execute(&mut *tx)
        .await?;
        if deleted.rows_affected() == 0 {
            return Err(ApiError::not_found("Item not found"));
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn save_item_draft(
        &self,
        signer: &str,
        draft: &ItemDraft,
    ) -> Result<Value, ApiError> {
        draft.validate(draft.id, signer)?;
        let mut tx = self.pool.begin().await?;
        lock_item_draft(&mut tx, signer, draft.id, draft.collection_id).await?;
        let changed = sqlx::query(
            "INSERT INTO builder.items (id, name, eth_address, collection_id, description, thumbnail, type, rarity, price, beneficiary, data, metrics) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
             ON CONFLICT (id) DO UPDATE SET name=EXCLUDED.name, collection_id=EXCLUDED.collection_id, \
             description=EXCLUDED.description, thumbnail=EXCLUDED.thumbnail, type=EXCLUDED.type, \
             rarity=EXCLUDED.rarity, price=EXCLUDED.price, beneficiary=EXCLUDED.beneficiary, data=EXCLUDED.data, metrics=COALESCE(EXCLUDED.metrics,items.metrics), updated_at=timezone('UTC', now()) \
             WHERE items.eth_address=$3 AND NOT items.is_published \
             AND NOT EXISTS (SELECT 1 FROM builder.collections c WHERE c.id=items.collection_id AND (c.is_published OR c.publication_pending)) RETURNING id",
        ).bind(draft.id).bind(draft.name.trim()).bind(signer).bind(draft.collection_id)
            .bind(&draft.description).bind(&draft.thumbnail).bind(&draft.item_type).bind(&draft.rarity)
            .bind(&draft.price).bind(&draft.beneficiary).bind(&draft.data).bind(&draft.metrics).fetch_optional(&mut *tx).await?;
        if changed.is_none() {
            return Err(ApiError::forbidden(
                "Item is publishing, published, or belongs to another wallet",
            ));
        }
        sqlx::query("DELETE FROM builder.item_contents WHERE item_id = $1")
            .bind(draft.id)
            .execute(&mut *tx)
            .await?;
        if !draft.contents.is_empty() {
            let files: Vec<&str> = draft.contents.keys().map(String::as_str).collect();
            let hashes: Vec<&str> = draft.contents.values().map(String::as_str).collect();
            let inserted = sqlx::query(
                "INSERT INTO builder.item_contents (item_id, file, hash) \
                 SELECT $1, f, h FROM unnest($2::text[], $3::text[]) AS t(f, h)",
            )
            .bind(draft.id)
            .bind(&files)
            .bind(&hashes)
            .execute(&mut *tx)
            .await;
            match inserted {
                Ok(_) => {}
                Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some(FK_VIOLATION) => {
                    return Err(ApiError::bad_request(
                        "Upload item content before saving its metadata",
                    ));
                }
                Err(e) => return Err(e.into()),
            }
        }
        tx.commit().await?;
        self.item_draft(signer, draft.id).await
    }
}

impl ItemsComponent {
    /// The multipart upload: 1..100 files, 20 MB together. Each file's bytes
    /// are stored under their CIDv1 in the ONE blob store before the row
    /// that records the acceptance (the store is immutable and idempotent, so
    /// a rolled-back transaction leaves nothing wrong behind).
    pub async fn save_item_files(
        &self,
        signer: &str,
        id: Uuid,
        files: Vec<(String, Vec<u8>)>,
    ) -> Result<BTreeMap<String, String>, ApiError> {
        if files.is_empty()
            || files.len() > 100
            || files.iter().map(|(_, bytes)| bytes.len()).sum::<usize>() > 20 * 1024 * 1024
        {
            return Err(ApiError::bad_request(
                "Upload 1 to 100 files totaling at most 20 MB",
            ));
        }
        let mut tx = self.pool.begin().await?;
        lock_item_draft(&mut tx, signer, id, None).await?;
        let allowed: Option<Uuid> = sqlx::query_scalar(
            "SELECT i.id FROM builder.items i WHERE i.id=$1 AND i.eth_address=$2 AND NOT i.is_published \
             AND NOT EXISTS (SELECT 1 FROM builder.collections c WHERE c.id=i.collection_id AND (c.is_published OR c.publication_pending)) FOR UPDATE OF i",
        ).bind(id).bind(signer).fetch_optional(&mut *tx).await?;
        if allowed.is_none() {
            return Err(ApiError::forbidden(
                "Item is publishing, published, or belongs to another wallet",
            ));
        }
        let mut contents = BTreeMap::new();
        for (file, bytes) in files {
            if !valid_filename(&file) {
                return Err(ApiError::bad_request("Invalid filename"));
            }
            let hash = hash_bytes_v1(&bytes);
            let size = bytes.len() as i64;
            if !self
                .blobs
                .exist(&hash)
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
            {
                self.blobs
                    .store(&hash, Bytes::from(bytes))
                    .await
                    .map_err(|e| ApiError::internal(format!("storing {hash}: {e}")))?;
            }
            sqlx::query("INSERT INTO builder.uploaded_contents (hash, size, uploaded_by) VALUES ($1,$2,$3) ON CONFLICT (hash) DO NOTHING")
                .bind(&hash).bind(size).bind(signer).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO builder.item_contents (item_id,file,hash) VALUES ($1,$2,$3) ON CONFLICT (item_id,file) DO UPDATE SET hash=EXCLUDED.hash")
                .bind(id).bind(&file).bind(&hash).execute(&mut *tx).await?;
            contents.insert(file, hash);
        }
        sqlx::query("UPDATE builder.items SET updated_at=timezone('UTC', now()) WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(contents)
    }

    pub async fn uploaded_content_exists(&self, hash: &str) -> Result<bool, ApiError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM builder.uploaded_contents WHERE hash=$1)",
        )
        .bind(hash)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The accepted upload's bytes, whole: for tests and small files; the
    /// serve route streams through [`Self::open_uploaded_content`].
    pub async fn uploaded_content(&self, hash: &str) -> Result<Option<Bytes>, ApiError> {
        if !self.uploaded_content_exists(hash).await? {
            return Ok(None);
        }
        self.blobs
            .retrieve(hash)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))
    }

    /// `None` unless `builder.uploaded_contents` names the hash (a blob the
    /// store holds for content or worlds is not a builder upload).
    pub async fn open_uploaded_content(
        &self,
        hash: &str,
    ) -> Result<Option<OpenedContent>, ApiError> {
        if !self.uploaded_content_exists(hash).await? {
            return Ok(None);
        }
        self.blobs
            .open(hash)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))
    }
}

async fn lock_item_draft(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    signer: &str,
    id: Uuid,
    next_collection: Option<Uuid>,
) -> Result<(), ApiError> {
    let previous: Option<(Option<Uuid>, String)> =
        sqlx::query_as("SELECT collection_id, eth_address FROM builder.items WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if previous
        .as_ref()
        .is_some_and(|(_, owner)| !owner.eq_ignore_ascii_case(signer))
    {
        return Err(ApiError::forbidden("Item belongs to another wallet"));
    }
    let previous_collection = previous.and_then(|(collection, _)| collection);
    let mut parents: Vec<_> = previous_collection
        .into_iter()
        .chain(next_collection)
        .collect();
    parents.sort();
    parents.dedup();
    for parent in parents {
        let allowed: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM builder.collections WHERE id=$1 AND eth_address=$2 \
            AND NOT is_published AND NOT publication_pending FOR UPDATE",
        )
        .bind(parent)
        .bind(signer)
        .fetch_optional(&mut **tx)
        .await?;
        if allowed.is_none() {
            return Err(ApiError::forbidden(
                "Collection is publishing, published, or belongs to another wallet",
            ));
        }
    }
    let current: Option<(Option<Uuid>, String, bool)> = sqlx::query_as(
        "SELECT collection_id, eth_address, is_published FROM builder.items WHERE id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((parent, owner, published)) = current {
        if !owner.eq_ignore_ascii_case(signer) || published {
            return Err(ApiError::forbidden(
                "Item is published or belongs to another wallet",
            ));
        }
        if parent != previous_collection {
            return Err(ApiError::http(
                409,
                "Item moved to another collection. Reload it before saving.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn draft_write_cannot_claim_another_owner_or_publication() {
        let id = Uuid::nil();
        let mut draft: CollectionDraft =
            serde_json::from_value(json!({"id": id, "name": "My collection"})).unwrap();
        assert!(draft.validate(id, "0xowner").is_ok());
        draft.eth_address = Some("0xother".into());
        assert!(draft.validate(id, "0xowner").is_err());
        draft.eth_address = None;
        draft.is_published = true;
        assert!(draft.validate(id, "0xowner").is_err());
    }

    #[test]
    fn content_paths_allow_subdirectories_without_escape() {
        assert!(valid_filename("textures/skin.png"));
        for bad in [
            "",
            "../skin.png",
            "/skin.png",
            "a/../b",
            "a\\b",
            "a//b",
            "a\0b",
        ] {
            assert!(!valid_filename(bad), "{bad}");
        }
    }
}
