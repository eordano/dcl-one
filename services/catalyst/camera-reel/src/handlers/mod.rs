//! The route handlers: `images` (upload, blob, metadata, visibility,
//! delete, the moderator's review and delete), `users` (the owner's gallery
//! and count), `places` (the public place and world galleries) and
//! `wearables` (the public photos of people wearing an item). The page
//! query is shared: the old `offset`/`limit`, plus `cursor` for the keyset
//! shape ([`super::db::Page`]); the wearables route is offset-only.

pub mod images;
pub mod places;
pub mod users;
pub mod wearables;

use serde::Deserialize;

use super::db::{Cursor, Page};
use super::http::ApiError;

pub fn default_offset() -> u64 {
    0
}
pub fn default_limit() -> u64 {
    20
}
pub const MAX_LIMIT: u64 = 100;
pub fn default_compact() -> bool {
    false
}

/// `?offset=&limit=` as before; `?cursor=` (empty for the first page) selects
/// the keyset shape, and `offset` is then ignored.
#[derive(Deserialize, Debug, Default)]
pub struct PageQuery {
    #[serde(default = "default_offset")]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
    #[serde(default)]
    pub cursor: Option<String>,
}

impl PageQuery {
    pub fn page(&self) -> Result<Page, ApiError> {
        match &self.cursor {
            None => Ok(Page::Offset(self.offset as i64)),
            Some(raw) if raw.is_empty() => Ok(Page::After(None)),
            Some(raw) => Cursor::decode(raw)
                .map(|c| Page::After(Some(c)))
                .ok_or_else(|| ApiError::BadRequest("invalid cursor".to_string())),
        }
    }

    pub fn capped_limit(&self) -> i64 {
        capped_limit(self.limit)
    }
}

pub fn capped_limit(limit: u64) -> i64 {
    limit.min(MAX_LIMIT) as i64
}

/// The wire form of a page's `next`: absent on an offset request.
pub fn next_cursor(next: Option<Option<Cursor>>) -> Option<Option<String>> {
    next.map(|n| n.map(|c| c.encode()))
}
