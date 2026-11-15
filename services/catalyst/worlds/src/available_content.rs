//! `GET /worlds-content-server/available-content`: worlds-content-server's
//! `availableContentHandler` (`content-file-handler.ts`) over the shared blob
//! store: its query checks and 400 envelope, the literal `cid` pairs
//! (`URLSearchParams.getAll('cid')`), and every requested cid listed once.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::content::ContentState;
use crate::content::entity::is_ipfs_v2;
use crate::content::errors::AppError;
use crate::content::wire_types::AvailableContentItem;

const MAX_AVAILABLE_CONTENT_CIDS: usize = 500;

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "Bad request", "message": message })),
    )
        .into_response()
}

/// The values of the pairs named exactly `cid`, as `URLSearchParams.getAll`
/// reads them: `cid[]` and `cid[0]` are other names.
fn cid_values(query: &str) -> Vec<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .filter(|(k, _)| k == "cid")
        .map(|(_, v)| v.into_owned())
        .collect()
}

pub async fn get(State(blobs): State<Arc<ContentState>>, request: Request) -> Response {
    let cids = cid_values(request.uri().query().unwrap_or(""));
    if cids.is_empty() {
        return bad_request("At least one cid query parameter is required.");
    }
    if cids.len() > MAX_AVAILABLE_CONTENT_CIDS {
        return bad_request(&format!(
            "Too many cid query parameters. Maximum allowed is {MAX_AVAILABLE_CONTENT_CIDS}."
        ));
    }
    if !cids.iter().all(|cid| is_ipfs_v2(cid)) {
        return bad_request("Invalid cid format.");
    }
    // Upstream answers `storage.existMultiple`'s map: a repeated cid once, where
    // it first appears. A denylisted cid is held for no one, so it is
    // `available: false` (a deploy that uploads it is refused as before).
    let mut seen = HashSet::new();
    let cids: Vec<String> = cids
        .into_iter()
        .filter(|c| seen.insert(c.clone()))
        .collect();
    let lookup: Vec<&String> = cids
        .iter()
        .filter(|cid| !blobs.denylist.contains(cid))
        .collect();
    let existence = match blobs.storage.exist_multiple(&lookup).await {
        Ok(existence) => existence,
        Err(e) => return AppError::from(e).into_response(),
    };
    let body: Vec<AvailableContentItem> = cids
        .into_iter()
        .map(|cid| AvailableContentItem {
            available: existence.get(&cid).copied().unwrap_or(false),
            cid,
        })
        .collect();
    Json(body).into_response()
}

#[cfg(test)]
mod tests {
    use super::cid_values;

    #[test]
    fn only_the_literal_cid_name_counts() {
        assert_eq!(
            cid_values("cid=a&cid%5B%5D=b&cid[0]=c&cids=d&cid=e+f%21"),
            vec!["a".to_string(), "e f!".to_string()]
        );
        assert!(cid_values("cid[]=a").is_empty());
    }
}
