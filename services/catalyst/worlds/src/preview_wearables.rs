//! `GET /world/{world_name}/preview-wearables`: the smart wearables a realm is
//! previewing. The stock client fetches this on entry, takes `data[0]`,
//! resolves the `scene.json` inside its first representation and runs it as
//! a live scene, so the answer is never inferred: exactly the URNs an
//! operator selected for the world (`worlds.worlds.preview_wearable_urns`,
//! the settings route) or node-wide (`PREVIEW_WEARABLE_URNS`). A normally
//! deployed world selects none and answers an empty list. The old service
//! asked its content server over HTTP for the entities; the content server is
//! this process, so they are read from the active pointers directly.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Value, json};

use crate::content::queries::{active_entity_docs_by_pointers, retain_non_denylisted};
use crate::index::content_file_hash;
use crate::shared::cache::TtlMap;

use super::SharedState;
use crate::comms::world_identity::WorldName;

/// Resolved wearable entities per selected URN set; only successful lookups are kept.
pub const PREVIEW_MEMO_TTL: Duration = Duration::from_secs(120);

pub fn memo() -> TtlMap<String, Arc<Vec<Value>>> {
    TtlMap::bounded(PREVIEW_MEMO_TTL, 1024)
}

/// `PREVIEW_WEARABLE_URNS`: comma, space, tab or newline separated.
pub fn parse_urns(raw: &str) -> Vec<String> {
    raw.split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub async fn get_preview_wearables(
    State(state): State<SharedState>,
    Path(world_name): Path<String>,
) -> Json<Value> {
    let stored: Option<Vec<String>> = match WorldName::parse(&world_name) {
        Ok(world) => sqlx::query_scalar(
            "SELECT preview_wearable_urns FROM worlds.worlds WHERE name = $1",
        )
        .bind(world.key())
        .fetch_optional(&state.pool)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "preview-wearables: the world row could not be read");
            None
        })
        .flatten(),
        Err(_) => None,
    };
    let selected = stored
        .filter(|urns| !urns.is_empty())
        .unwrap_or_else(|| state.settings.preview_wearable_urns.clone());
    if selected.is_empty() {
        return Json(json!({ "ok": true, "data": [] }));
    }
    let mut sorted = selected.clone();
    sorted.sort();
    let key = sorted.join("\n");
    // A failure here is not the visitor's problem: the realm is still
    // enterable, and an empty list means "nothing previewed".
    let entities = state
        .preview_memo
        .get_or_fetch(key, || async {
            let pointers: Vec<String> = selected.iter().map(|u| u.to_lowercase()).collect();
            let mut docs = active_entity_docs_by_pointers(&state.pool, &pointers).await?;
            retain_non_denylisted(&mut docs, state.storage.denylist());
            Ok::<_, sqlx::Error>(Arc::new(
                docs.iter().filter_map(|d| d.to_value()).collect::<Vec<_>>(),
            ))
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "preview-wearables: the pointer lookup failed");
            Arc::new(Vec::new())
        });
    let content_base = state.settings.content_public_url.trim_end_matches('/');
    let data: Vec<Value> = entities
        .iter()
        .filter_map(|e| preview_wearable(e, content_base))
        .collect();
    Json(json!({ "ok": true, "data": data }))
}

/// Reshapes a wearable entity into what the client parses. The `hash` alongside each
/// `url` is load-bearing: the client turns it into the previewed scene's content
/// mapping, so without it the scene loads with no files.
fn preview_wearable(entity: &Value, content_base: &str) -> Option<Value> {
    let id = entity
        .get("pointers")
        .and_then(|p| p.as_array())
        .and_then(|p| p.first())
        .and_then(|p| p.as_str())
        .or_else(|| entity.get("id").and_then(|i| i.as_str()))?;

    let hash_of = |file: &str| content_file_hash(entity, file);

    let representations: Vec<Value> = entity
        .get("metadata")
        .and_then(|m| m.get("data"))
        .and_then(|d| d.get("representations"))
        .and_then(|r| r.as_array())
        .map(|reps| {
            reps.iter()
                .map(|rep| {
                    let contents: Vec<Value> = rep
                        .get("contents")
                        .and_then(|c| c.as_array())
                        .map(|files| {
                            files
                                .iter()
                                .filter_map(|f| f.as_str())
                                .filter_map(|file| {
                                    let hash = hash_of(file)?;
                                    Some(json!({
                                        "key": file,
                                        "url": format!("{content_base}/contents/{hash}"),
                                        "hash": hash,
                                    }))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({ "contents": contents })
                })
                .collect()
        })
        .unwrap_or_default();

    let runnable = representations.iter().any(|rep| {
        rep["contents"]
            .as_array()
            .map(|c| {
                c.iter().any(|item| {
                    item["key"]
                        .as_str()
                        .map(|k| k.to_ascii_lowercase().ends_with("scene.json"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
    if !runnable {
        return None;
    }

    Some(json!({
        "id": id,
        "data": { "representations": representations },
    }))
}

#[cfg(test)]
mod tests {
    use super::preview_wearable;
    use serde_json::json;

    fn smart_wearable() -> serde_json::Value {
        json!({
            "id": "bafyEntity",
            "pointers": ["urn:decentraland:off-chain:base-avatars:smart-hat"],
            "content": [
                { "file": "male/scene.json", "hash": "QmScene" },
                { "file": "male/hat.glb", "hash": "QmHat" }
            ],
            "metadata": {
                "data": {
                    "representations": [
                        { "contents": ["male/scene.json", "male/hat.glb"] }
                    ]
                }
            }
        })
    }

    #[test]
    fn the_node_wide_list_splits_on_every_separator() {
        assert_eq!(
            super::parse_urns(" urn:a,urn:b\turn:c\n\nurn:d  "),
            vec!["urn:a", "urn:b", "urn:c", "urn:d"]
        );
        assert!(super::parse_urns("").is_empty());
    }

    #[test]
    fn maps_contents_to_key_url_and_hash() {
        let out = preview_wearable(&smart_wearable(), "https://node/content").expect("smart");
        assert_eq!(
            out["id"],
            json!("urn:decentraland:off-chain:base-avatars:smart-hat")
        );
        let contents = &out["data"]["representations"][0]["contents"];
        assert_eq!(contents[0]["key"], json!("male/scene.json"));
        assert_eq!(
            contents[0]["url"],
            json!("https://node/content/contents/QmScene")
        );
        assert_eq!(contents[0]["hash"], json!("QmScene"));
        assert_eq!(contents[1]["hash"], json!("QmHat"));
    }

    #[test]
    fn drops_a_wearable_with_no_scene_json() {
        let mut plain = smart_wearable();
        plain["content"] = json!([{ "file": "male/hat.glb", "hash": "QmHat" }]);
        plain["metadata"]["data"]["representations"] = json!([{ "contents": ["male/hat.glb"] }]);
        assert!(preview_wearable(&plain, "https://node/content").is_none());
    }

    #[test]
    fn drops_a_content_entry_the_entity_does_not_carry() {
        let mut missing = smart_wearable();
        missing["content"] = json!([{ "file": "male/scene.json", "hash": "QmScene" }]);
        let out = preview_wearable(&missing, "https://node/content").expect("smart");
        let contents = out["data"]["representations"][0]["contents"]
            .as_array()
            .expect("array")
            .len();
        assert_eq!(contents, 1);
    }

    /// A world's own selection wins over the node-wide list; an unknown world
    /// takes the node-wide list; the active entity is read from the pointers.
    #[tokio::test]
    async fn the_world_selection_resolves_through_the_active_pointers() {
        use std::sync::Arc;
        use tower::ServiceExt;
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let smart = format!("urn:decentraland:off-chain:preview-{tag}:smart-hat");
        let plain = format!("urn:decentraland:off-chain:preview-{tag}:plain-hat");
        for (pointer, files) in [
            (&smart, vec!["male/scene.json", "male/hat.glb"]),
            (&plain, vec!["male/hat.glb"]),
        ] {
            let entity_id = format!("bafypreview{}{tag}", files.len());
            let meta =
                json!({ "id": pointer, "data": { "representations": [{ "contents": files }] } });
            let id: i32 = sqlx::query_scalar(
                "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
                 VALUES ('0x1', 'v3', 'wearable', $1, $2, to_timestamp(1700000000), ARRAY[$3], timezone('UTC', now()), '[]'::json) RETURNING id",
            )
            .bind(&entity_id)
            .bind(json!({ "v": meta }))
            .bind(pointer)
            .fetch_one(&pool)
            .await
            .unwrap();
            for file in &files {
                sqlx::query("INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, $2, $3)")
                    .bind(id)
                    .bind(format!("Qm{}", file.replace(['/', '.'], "")))
                    .bind(*file)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, 'wearable')")
                .bind(pointer)
                .bind(&entity_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let world = format!("preview{}.dcl.eth", &tag[..12]);
        sqlx::query("INSERT INTO worlds.worlds (name, preview_wearable_urns) VALUES ($1, $2)")
            .bind(&world)
            .bind(vec![smart.clone(), plain.clone()])
            .execute(&pool)
            .await
            .unwrap();

        let dir = std::env::temp_dir().join(format!(
            "catalyst-worlds-preview-{}-{tag}",
            std::process::id()
        ));
        let storage = Arc::new(catalyst_storage::Storage::from_config(&dir).await.unwrap());
        let mut settings = crate::Settings::for_tests("https://worlds.example.org");
        settings.preview_wearable_urns = vec![plain.clone()];
        let state = Arc::new(crate::WorldsState::new(
            pool.clone(),
            storage,
            settings,
            None,
            crate::http::Outbound::for_tests(),
        ));
        let get = |uri: String| {
            let app = crate::router(state.clone());
            async move {
                let resp = app
                    .oneshot(
                        axum::http::Request::get(uri)
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), 200);
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
            }
        };

        let body = get(format!("/world/{}/preview-wearables", world.to_uppercase())).await;
        assert_eq!(body["ok"], json!(true));
        let data = body["data"].as_array().unwrap();
        assert_eq!(
            data.len(),
            1,
            "the wearable without scene.json is dropped: {body}"
        );
        assert_eq!(data[0]["id"], json!(smart));
        assert_eq!(
            data[0]["data"]["representations"][0]["contents"][0]["url"],
            json!("https://peer.example.org/content/contents/Qmmalescenejson")
        );

        let fallback = get(format!(
            "/world/unknown{}.dcl.eth/preview-wearables",
            &tag[..8]
        ))
        .await;
        assert_eq!(
            fallback,
            json!({ "ok": true, "data": [] }),
            "the node-wide plain wearable is not a smart one"
        );

        sqlx::query("DELETE FROM worlds.worlds WHERE name = $1")
            .bind(&world)
            .execute(&pool)
            .await
            .unwrap();
    }
}
