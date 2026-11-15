//! The camera-reel signed-fetch profile at the routes: a signature at most
//! 60 s old and never too new, `x-identity-metadata` required but unread,
//! every chain link carrying a signature string, and the scene signer served
//! on the owner reads and refused on every write with the source's 401.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::{
    API_URL, PREFIX, Wallet, image, json_of, metadata_json, multipart, png, scratch, send, tag,
    wallet,
};
use crate::Settings;
use crate::shared::signed_fetch::test_support::signed_headers_by;
use crate::shared::signed_fetch::{AUTH_CHAIN_HEADER_PREFIX, AUTH_METADATA_HEADER};

const SCENE: &str = r#"{"signer":"decentraland-kernel-scene","sceneId":"bafkscene"}"#;

/// Headers `w` signs for `method path` `offset_ms` from now, in the 6.x
/// (`v6`) or the legacy payload, carrying `metadata` (`None`: no metadata
/// header, signed as `{}`).
async fn signed_as(
    w: &Wallet,
    method: &str,
    path: &str,
    metadata: Option<&str>,
    offset_ms: i64,
    v6: bool,
) -> Vec<(String, String)> {
    let timestamp = (chrono::Utc::now().timestamp_millis() + offset_ms).to_string();
    let mut headers = signed_headers_by(
        &w.signer,
        method,
        path,
        metadata.unwrap_or("{}"),
        v6,
        &timestamp,
    )
    .await;
    if metadata.is_none() {
        headers.remove(AUTH_METADATA_HEADER);
    }
    headers
        .iter()
        .map(|(name, value)| {
            let mut value = value.to_str().unwrap().to_string();
            // The source's chain parser wants a signature string on every
            // link, the SIGNER's empty one included.
            if name.as_str() == format!("{AUTH_CHAIN_HEADER_PREFIX}0") {
                let mut link: Value = serde_json::from_str(&value).unwrap();
                link["signature"] = json!("");
                value = link.to_string();
            }
            (name.to_string(), value)
        })
        .collect()
}

/// [`signed_as`] in the 6.x payload.
async fn signed(
    w: &Wallet,
    method: &str,
    path: &str,
    metadata: Option<&str>,
    offset_ms: i64,
) -> Vec<(String, String)> {
    signed_as(w, method, path, metadata, offset_ms, true).await
}

/// `headers` with the metadata header replaced, as a replay would send it.
fn with_metadata(mut headers: Vec<(String, String)>, metadata: &str) -> Vec<(String, String)> {
    for (name, value) in &mut headers {
        if name == AUTH_METADATA_HEADER {
            *value = metadata.to_string();
        }
    }
    headers
}

/// The source's refusal: 401 with its text/plain `Unathorized` body.
fn unauthorized() -> (StatusCode, Vec<u8>) {
    (StatusCode::UNAUTHORIZED, b"Unathorized".to_vec())
}

#[tokio::test]
async fn a_scene_signed_read_lists_the_owners_private_photos() {
    let Some(mut s) = scratch(Settings::new(API_URL)).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let t = tag();
    let private = image(&owner, &format!("h-{t}"), &format!("t-{t}"), false, "");
    s.db().insert_image(&private).await.unwrap();

    let images = format!("{PREFIX}/api/users/{owner}/images");
    let user = format!("{PREFIX}/api/users/{owner}");
    let (_, _, resp) = send(s.app(), "GET", &images, vec![], None).await;
    assert_eq!(json_of(&resp)["currentImages"], 0, "anonymous");
    for metadata in [SCENE, "not json"] {
        let headers = signed(&w, "get", &images, Some(metadata), 0).await;
        let (status, _, resp) = send(s.app(), "GET", &images, headers, None).await;
        assert_eq!(status, StatusCode::OK);
        let page = json_of(&resp);
        assert_eq!(page["currentImages"], 1, "{metadata}");
        assert_eq!(
            page["images"][0]["id"],
            private.id.to_string(),
            "{metadata}"
        );
        let headers = signed(&w, "get", &user, Some(metadata), 0).await;
        let (_, _, resp) = send(s.app(), "GET", &user, headers, None).await;
        assert_eq!(json_of(&resp)["currentImages"], 1, "{metadata}");
    }
    s.done().await;
}

#[tokio::test]
async fn a_scene_signed_write_is_refused_on_every_write_route() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let t = tag();
    let private = image(&owner, &format!("h-{t}"), &format!("t-{t}"), false, "");
    s.db().insert_image(&private).await.unwrap();

    let upload = format!("{PREFIX}/api/images");
    let (body, ct) = multipart(
        &png(16, 9, 1),
        "image/png",
        &metadata_json(&owner, ""),
        None,
    );
    let headers = signed(&w, "post", &upload, Some(SCENE), 0).await;
    let (status, _, resp) = send(s.app(), "POST", &upload, headers, Some((body, &ct))).await;
    assert_eq!((status, resp), unauthorized(), "multipart upload");

    let upload_json = format!("{PREFIX}/api/images-json");
    let body = {
        use base64::Engine as _;
        json!({
            "image": base64::engine::general_purpose::STANDARD.encode(png(16, 9, 2)),
            "content_type": "image/png",
            "metadata": metadata_json(&owner, ""),
        })
        .to_string()
        .into_bytes()
    };
    let headers = signed(&w, "post", &upload_json, Some(SCENE), 0).await;
    let (status, _, resp) = send(
        s.app(),
        "POST",
        &upload_json,
        headers,
        Some((body, "application/json")),
    )
    .await;
    assert_eq!((status, resp), unauthorized(), "json upload");
    assert_eq!(s.count(&owner).await, 1, "no upload landed");

    let vis = format!("{PREFIX}/api/images/{}/visibility", private.id);
    let flip = json!({ "is_public": true }).to_string().into_bytes();
    for metadata in [SCENE, r#"{"Signer":" Decentraland-Kernel-Scene"}"#] {
        let headers = signed(&w, "patch", &vis, Some(metadata), 0).await;
        let (status, _, resp) = send(
            s.app(),
            "PATCH",
            &vis,
            headers,
            Some((flip.clone(), "application/json")),
        )
        .await;
        assert_eq!((status, resp), unauthorized(), "{metadata}");
    }
    assert_eq!(s.is_public(private.id).await, Some(false));

    let del = format!("{PREFIX}/api/images/{}", private.id);
    let headers = signed(&w, "delete", &del, Some(SCENE), 0).await;
    let (status, _, resp) = send(s.app(), "DELETE", &del, headers, None).await;
    assert_eq!((status, resp), unauthorized(), "delete");
    assert_eq!(s.count(&owner).await, 1, "the row stays");

    // A legacy-signed scene request replayed with a literal uppercased: the
    // metadata no longer parses but the folded payload still verifies.
    let replayed = with_metadata(
        signed_as(
            &w,
            "patch",
            &vis,
            Some(r#"{"signer":"decentraland-kernel-scene","isGuest":false}"#),
            0,
            false,
        )
        .await,
        r#"{"signer":"decentraland-kernel-scene","isGuest":FALSE}"#,
    );
    let (status, _, resp) = send(
        s.app(),
        "PATCH",
        &vis,
        replayed,
        Some((flip.clone(), "application/json")),
    )
    .await;
    assert_eq!((status, resp), unauthorized(), "replayed legacy");
    assert_eq!(s.is_public(private.id).await, Some(false));

    // Metadata that does not parse and names no scene is the user's.
    let headers = signed(&w, "patch", &vis, Some("not json"), 0).await;
    let (status, _, _) = send(
        s.app(),
        "PATCH",
        &vis,
        headers,
        Some((flip, "application/json")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(s.is_public(private.id).await, Some(true));
    s.done().await;
}

#[tokio::test]
async fn the_timestamp_window_and_the_metadata_header_are_upstreams() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let t = tag();
    let private = image(&owner, &format!("h-{t}"), &format!("t-{t}"), false, "");
    s.db().insert_image(&private).await.unwrap();
    let vis = format!("{PREFIX}/api/images/{}/visibility", private.id);
    let images = format!("{PREFIX}/api/users/{owner}/images");
    let patch = |headers, public: bool| {
        let app = s.app();
        let vis = vis.clone();
        async move {
            let body = json!({ "is_public": public }).to_string().into_bytes();
            let (status, _, resp) = send(
                app,
                "PATCH",
                &vis,
                headers,
                Some((body, "application/json")),
            )
            .await;
            (status, resp)
        }
    };
    let listed = |headers| {
        let app = s.app();
        let images = images.clone();
        async move {
            let (_, _, resp) = send(app, "GET", &images, headers, None).await;
            json_of(&resp)["currentImages"].as_u64()
        }
    };

    let two_minutes_ago = -2 * 60 * 1000;
    assert_eq!(
        patch(
            signed(&w, "patch", &vis, Some("{}"), two_minutes_ago).await,
            true
        )
        .await,
        unauthorized(),
        "a 2-minute-old signature"
    );
    assert_eq!(
        listed(signed(&w, "get", &images, Some("{}"), two_minutes_ago).await).await,
        Some(0),
        "a stale read is anonymous"
    );

    assert_eq!(
        patch(signed(&w, "patch", &vis, None, 0).await, true).await,
        unauthorized(),
        "x-identity-metadata missing"
    );
    assert_eq!(
        listed(signed(&w, "get", &images, None, 0).await).await,
        Some(0),
        "a read without metadata is anonymous"
    );
    assert_eq!(s.is_public(private.id).await, Some(false));

    let an_hour_ahead = 60 * 60 * 1000;
    assert_eq!(
        listed(signed(&w, "get", &images, Some("{}"), an_hour_ahead).await).await,
        Some(1),
        "a future signature reads as the owner"
    );
    let (status, _) = patch(
        signed(&w, "patch", &vis, Some("{}"), an_hour_ahead).await,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a future signature writes");
    assert_eq!(s.is_public(private.id).await, Some(true));
    s.done().await;
}

#[tokio::test]
async fn a_legacy_signature_over_mixed_case_metadata_and_an_empty_metadata_header_verify() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let w = wallet();
    let owner = s.owner(&w.address);
    let t = tag();
    let private = image(&owner, &format!("h-{t}"), &format!("t-{t}"), false, "");
    s.db().insert_image(&private).await.unwrap();
    let vis = format!("{PREFIX}/api/images/{}/visibility", private.id);
    let images = format!("{PREFIX}/api/users/{owner}/images");

    for (metadata, v6, public) in [
        (r#"{"realmName":"Main"}"#, false, true),
        ("", true, false),
        ("", false, true),
    ] {
        let headers = signed_as(&w, "get", &images, Some(metadata), 0, v6).await;
        let (_, _, resp) = send(s.app(), "GET", &images, headers, None).await;
        assert_eq!(
            json_of(&resp)["currentImages"],
            1,
            "{metadata:?} v6={v6} reads as the owner"
        );
        let headers = signed_as(&w, "patch", &vis, Some(metadata), 0, v6).await;
        let body = json!({ "is_public": public }).to_string().into_bytes();
        let (status, _, _) = send(
            s.app(),
            "PATCH",
            &vis,
            headers,
            Some((body, "application/json")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{metadata:?} v6={v6}");
        assert_eq!(s.is_public(private.id).await, Some(public));
    }
    s.done().await;
}
