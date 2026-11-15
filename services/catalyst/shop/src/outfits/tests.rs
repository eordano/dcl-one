//! The outfits routes over the scratch database and a temporary blob store
//! (shop-server@dc1a8be96 test/unit/controllers/handlers/{post-outfit,
//! get-outfits,...}.spec.ts and test/unit/logic/outfits.spec.ts, re-expressed
//! through the router), plus the pure checks.

use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::testkit::{scratch_wallet, send, send_raw, signed_headers, signed_headers_with};
use super::*;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 1, 2, 3,
];

fn item(contract: &str, id: &str) -> OutfitItemRef {
    OutfitItemRef {
        contract_address: contract.to_string(),
        item_id: id.to_string(),
    }
}

fn request(id: &str) -> OutfitRequest {
    OutfitRequest {
        id: id.to_string(),
        name: "Summer look".to_string(),
        thumbnail_hash: String::new(),
        items: vec![item("0xAbCdEf0000000000000000000000000000000001", "7")],
        body_shape: "unisex".to_string(),
        gradient_from: String::new(),
        gradient_to: String::new(),
        published: false,
    }
}

#[test]
fn the_creator_allowlist_parses_like_the_flag_variant_and_fails_closed() {
    let creators = Settings::parse_creators(
        " 0xABCDEF0000000000000000000000000000000001,\n0xabcdef0000000000000000000000000000000001\nnot-an-address, 0x00000000000000000000000000000000000000aa ",
    );
    assert_eq!(
        creators,
        vec![
            "0x00000000000000000000000000000000000000aa".to_string(),
            "0xabcdef0000000000000000000000000000000001".to_string()
        ]
    );
    let settings = Settings {
        creators,
        writes_enabled: true,
    };
    assert!(settings.is_creator("0xABCDEF0000000000000000000000000000000001"));
    assert!(!settings.is_creator("0xabcdef0000000000000000000000000000000002"));
    assert!(Settings::parse_creators("garbage").is_empty());
    assert!(!Settings::default().is_creator("0xabcdef0000000000000000000000000000000001"));
}

#[test]
fn the_request_schema_is_the_upstream_ajv_schema() {
    let ok = request("2f1c8d1e-9a6b-4c0d-8e2f-1a2b3c4d5e6f");
    assert!(validate_request(&ok).is_ok());
    let mut bad = ok.clone();
    bad.id = "2F1C8D1E-9A6B-4C0D-8E2F-1A2B3C4D5E6F".into();
    assert!(
        validate_request(&bad).unwrap_err().contains("uuid"),
        "the pattern is lowercase hex"
    );
    let mut bad = ok.clone();
    bad.name = "a\u{1}b".into();
    assert!(validate_request(&bad).is_err());
    let mut bad = ok.clone();
    bad.name = "x".repeat(61);
    assert!(validate_request(&bad).is_err());
    let mut bad = ok.clone();
    bad.thumbnail_hash = "ABC".into();
    assert!(validate_request(&bad).is_err());
    let mut bad = ok.clone();
    bad.items = (0..21)
        .map(|i| item("0xabcdef0000000000000000000000000000000001", &i.to_string()))
        .collect();
    assert!(validate_request(&bad).is_err());
    let mut bad = ok.clone();
    bad.items = vec![item("0xabcdef0000000000000000000000000000000001", "12a")];
    assert!(validate_request(&bad).is_err());
    let mut bad = ok.clone();
    bad.body_shape = "robot".into();
    assert!(validate_request(&bad).is_err());
    let mut bad = ok.clone();
    bad.gradient_from = "#12345".into();
    assert!(validate_request(&bad).is_err());
    let mut fine = ok.clone();
    fine.gradient_from = "#ABCDEF".into();
    fine.gradient_to = "#000000".into();
    assert!(validate_request(&fine).is_ok());
    let normalized = normalize(fine);
    assert_eq!(normalized.gradient_from, "#abcdef");
    assert_eq!(
        normalized.items[0].contract_address,
        "0xabcdef0000000000000000000000000000000001"
    );
    let unknown: Result<OutfitRequest, _> = serde_json::from_str(
        r#"{"id":"x","name":"n","thumbnailHash":"","items":[],"bodyShape":"male","gradientFrom":"","gradientTo":"","published":false,"extra":1}"#,
    );
    assert!(unknown.is_err(), "additionalProperties: false");
}

#[test]
fn item_uniqueness_publish_bar_and_visibility_follow_logic_outfits_ts() {
    let a = item("0xABCDEF0000000000000000000000000000000001", "1");
    let b = item("0xabcdef0000000000000000000000000000000001", "1");
    assert!(
        !has_unique_items(&[a.clone(), b]),
        "case-folded duplicates are duplicates"
    );
    assert!(has_unique_items(&[
        a,
        item("0xabcdef0000000000000000000000000000000001", "2")
    ]));

    let hash = "a".repeat(64);
    assert!(is_publishable(&hash, "#000000", "#ffffff", 1));
    assert!(is_publishable(&hash, "#000000", "#ffffff", 20));
    assert!(!is_publishable("", "#000000", "#ffffff", 1));
    assert!(!is_publishable(&hash, "", "#ffffff", 1));
    assert!(!is_publishable(&hash, "#000000", "", 1));
    assert!(!is_publishable(&hash, "#000000", "#ffffff", 0));
    assert!(!is_publishable(&hash, "#000000", "#ffffff", 21));

    let settings = Settings {
        creators: vec!["0x00000000000000000000000000000000000000aa".into()],
        writes_enabled: true,
    };
    let draft = Outfit {
        id: "id".into(),
        name: "n".into(),
        thumbnail_hash: String::new(),
        items: vec![],
        body_shape: "male".into(),
        gradient_from: String::new(),
        gradient_to: String::new(),
        author_address: "0x00000000000000000000000000000000000000aa".into(),
        published: false,
        created_at: 1,
        updated_at: 1,
    };
    assert!(!can_read(&draft, None, &settings));
    assert!(!can_read(
        &draft,
        Some("0x00000000000000000000000000000000000000bb"),
        &settings
    ));
    assert!(can_read(
        &draft,
        Some("0x00000000000000000000000000000000000000AA"),
        &settings
    ));
    let published = Outfit {
        published: true,
        ..draft
    };
    assert!(can_read(&published, None, &settings));
}

#[test]
fn image_sniffing_and_js_number_parsing() {
    assert_eq!(sniff_image_type(PNG), Some("image/png"));
    assert_eq!(
        sniff_image_type(&[0xff, 0xd8, 0xff, 0xe0]),
        Some("image/jpeg")
    );
    assert_eq!(
        sniff_image_type(b"RIFF\0\0\0\0WEBPVP8 "),
        Some("image/webp")
    );
    assert_eq!(sniff_image_type(b"GIF89a"), None);
    assert_eq!(sniff_image_type(b""), None);
    assert_eq!(sniff_image_type(&[0x89, 0x50]), None);

    assert_eq!(js_number_or(None, 50), 50);
    assert_eq!(js_number_or(Some(&"".to_string()), 50), 50);
    assert_eq!(js_number_or(Some(&"abc".to_string()), 50), 50);
    assert_eq!(
        js_number_or(Some(&"0".to_string()), 50),
        50,
        "Number('0') || 50"
    );
    assert_eq!(js_number_or(Some(&"7".to_string()), 50), 7);
    assert_eq!(js_number_or(Some(&"7.9".to_string()), 50), 7);
    assert_eq!(js_number_or(Some(&"-3".to_string()), 0), -3);
    assert_eq!(
        js_number_or(Some(&"0x10".to_string()), 50),
        16,
        "Number('0x10')"
    );
    assert_eq!(
        js_number_or(Some(&"inf".to_string()), 50),
        50,
        "Number('inf') is NaN"
    );
    assert_eq!(js_number_or(Some(&"1_0".to_string()), 50), 50);
}

struct Rig {
    app: Router,
    creator: super::testkit::Wallet,
    stranger: super::testkit::Wallet,
    _dir: std::path::PathBuf,
}

async fn rig(writes_enabled: bool) -> Option<Rig> {
    let pool = crate::db::scratch::pool(4).await?;
    let dir = std::env::temp_dir().join(format!(
        "catalyst-outfits-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(Storage::from_config(&dir).await.unwrap());
    let creator = scratch_wallet();
    let stranger = scratch_wallet();
    let settings = Settings {
        creators: vec![creator.address.clone()],
        writes_enabled,
    };
    let state = Arc::new(State::new(pool, storage, settings, None));
    let app = Router::new().nest("/shop-api", shop_router(state));
    Some(Rig {
        app,
        creator,
        stranger,
        _dir: dir,
    })
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn body(req: &OutfitRequest) -> Value {
    json!({
        "id": req.id, "name": req.name, "thumbnailHash": req.thumbnail_hash,
        "items": req.items.iter().map(|i| json!({"contractAddress": i.contract_address, "itemId": i.item_id})).collect::<Vec<_>>(),
        "bodyShape": req.body_shape, "gradientFrom": req.gradient_from, "gradientTo": req.gradient_to, "published": req.published,
    })
}

#[tokio::test]
async fn authoring_is_signed_and_allowlisted_and_drafts_never_leak() {
    let Some(r) = rig(true).await else { return };
    let id = new_id();
    let req = request(&id);

    // Unsigned: the static 401 body, no verifier detail.
    let (status, _, v) = send(
        &r.app,
        "POST",
        "/shop-api/v1/outfits",
        HeaderMap::new(),
        Some(body(&req)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        v,
        json!({ "error": "unauthorized", "message": "This endpoint requires a signed-fetch request (ADR-44)." })
    );

    // Signed by a wallet outside the allowlist: 403 not_allowed.
    let h = signed_headers(&r.stranger, "post", "/shop-api/v1/outfits").await;
    let (status, _, v) = send(&r.app, "POST", "/shop-api/v1/outfits", h, Some(body(&req))).await;
    assert_eq!(
        (status, v),
        (
            StatusCode::FORBIDDEN,
            json!({ "ok": false, "error": "not_allowed" })
        )
    );

    // A scene-signed chain is refused before verification (400, as rejectIfSigner).
    let h = signed_headers_with(
        &r.creator,
        "post",
        "/shop-api/v1/outfits",
        r#"{"signer":"decentraland-kernel-scene"}"#,
    )
    .await;
    let (status, _, v) = send(&r.app, "POST", "/shop-api/v1/outfits", h, Some(body(&req))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "invalid_signer");

    // The creator: 201 with the row, the author from the signature and the addresses lowercased.
    let h = signed_headers(&r.creator, "post", "/shop-api/v1/outfits").await;
    let (status, _, v) = send(&r.app, "POST", "/shop-api/v1/outfits", h, Some(body(&req))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let outfit = &v["outfit"];
    assert_eq!(outfit["id"], id);
    assert_eq!(outfit["authorAddress"], r.creator.address);
    assert_eq!(
        outfit["items"][0]["contractAddress"],
        "0xabcdef0000000000000000000000000000000001"
    );
    assert_eq!(outfit["published"], false);
    assert!(outfit["createdAt"].as_i64().unwrap() > 1_700_000_000_000);
    let keys: Vec<&String> = outfit.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "authorAddress",
            "bodyShape",
            "createdAt",
            "gradientFrom",
            "gradientTo",
            "id",
            "items",
            "name",
            "published",
            "thumbnailHash",
            "updatedAt"
        ]
    );

    // A replay of the same client id: 200 with the stored row (whatever the body says now).
    let mut replay = req.clone();
    replay.name = "Renamed in the replay".into();
    let h = signed_headers(&r.creator, "post", "/shop-api/v1/outfits").await;
    let (status, _, v) = send(
        &r.app,
        "POST",
        "/shop-api/v1/outfits",
        h,
        Some(body(&replay)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["outfit"]["name"], "Summer look");

    // A draft: 404 to the public and to a signed stranger, 200 to the creator; the public list omits it.
    let path = format!("/shop-api/v1/outfits/{id}");
    let (status, _, v) = send(&r.app, "GET", &path, HeaderMap::new(), None).await;
    assert_eq!(
        (status, v),
        (
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": "not_found" })
        )
    );
    let h = signed_headers(&r.stranger, "get", &path).await;
    let (status, _, _) = send(&r.app, "GET", &path, h, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let h = signed_headers(&r.creator, "get", &path).await;
    let (status, headers, v) = send(&r.app, "GET", &path, h, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert_eq!(v["outfit"]["id"], id);
    let (_, _, v) = send(
        &r.app,
        "GET",
        "/shop-api/v1/outfits?limit=50",
        HeaderMap::new(),
        None,
    )
    .await;
    assert!(
        v["outfits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["id"] != id),
        "a draft is not listed publicly"
    );
    // The authoring view lists it, for the creator only.
    let (status, _, _) = send(
        &r.app,
        "GET",
        "/shop-api/v1/outfits/all",
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let h = signed_headers(&r.creator, "get", "/shop-api/v1/outfits/all").await;
    let (status, _, v) = send(
        &r.app,
        "GET",
        "/shop-api/v1/outfits/all?limit=1000",
        h,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        v["outfits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == id)
    );

    // Publishing needs the bar: thumbnail + both stops + items.
    let mut publish = req.clone();
    publish.published = true;
    let h = signed_headers(&r.creator, "put", &path).await;
    let (status, _, v) = send(&r.app, "PUT", &path, h, Some(body(&publish))).await;
    assert_eq!(
        (status, v),
        (
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "not_publishable" })
        )
    );
    let mut dup = req.clone();
    dup.items
        .push(item("0xABCDEF0000000000000000000000000000000001", "7"));
    let h = signed_headers(&r.creator, "put", &path).await;
    let (status, _, v) = send(&r.app, "PUT", &path, h, Some(body(&dup))).await;
    assert_eq!(
        (status, v),
        (
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "invalid_items" })
        )
    );
    let mut mismatch = req.clone();
    mismatch.id = new_id();
    let h = signed_headers(&r.creator, "put", &path).await;
    let (status, _, v) = send(&r.app, "PUT", &path, h, Some(body(&mismatch))).await;
    assert_eq!(
        (status, v),
        (
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "id_mismatch" })
        )
    );

    // The thumbnail: length required, the cap, the sniffed type, the sha256 id, the immutable bytes.
    let h = signed_headers(&r.creator, "post", "/shop-api/v1/outfits/thumbnails").await;
    let (status, _, _) = send_raw(&r.app, "POST", "/shop-api/v1/outfits/thumbnails", h, None).await;
    assert_eq!(status, StatusCode::LENGTH_REQUIRED);
    let mut h = signed_headers(&r.creator, "post", "/shop-api/v1/outfits/thumbnails").await;
    h.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(MAX_THUMBNAIL_BYTES as u64 + 1),
    );
    let (status, _, _) = send_raw(&r.app, "POST", "/shop-api/v1/outfits/thumbnails", h, None).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let h = signed_headers(&r.creator, "post", "/shop-api/v1/outfits/thumbnails").await;
    let (status, _, out) = send_raw(
        &r.app,
        "POST",
        "/shop-api/v1/outfits/thumbnails",
        h,
        Some((b"<svg/>".to_vec(), "image/svg+xml")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        String::from_utf8_lossy(&out)
    );
    let h = signed_headers(&r.creator, "post", "/shop-api/v1/outfits/thumbnails").await;
    let (status, _, out) = send_raw(
        &r.app,
        "POST",
        "/shop-api/v1/outfits/thumbnails",
        h,
        Some((PNG.to_vec(), "text/plain")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&out)
    );
    let hash = serde_json::from_slice::<Value>(&out).unwrap()["hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(hash, hex::encode(Sha256::digest(PNG)));
    let (status, headers, bytes) = send_raw(
        &r.app,
        "GET",
        &format!("/shop-api/v1/outfits/thumbnails/{}", hash.to_uppercase()),
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, PNG);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "image/png");
    assert_eq!(
        headers.get(header::CONTENT_LENGTH).unwrap(),
        &PNG.len().to_string()
    );
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    let (status, _, v) = send(
        &r.app,
        "GET",
        "/shop-api/v1/outfits/thumbnails/nothex",
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(
        (status, v),
        (
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "invalid_hash" })
        )
    );
    let (status, _, _) = send(
        &r.app,
        "GET",
        &format!("/shop-api/v1/outfits/thumbnails/{}", "0".repeat(64)),
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Published: the public read and the listing serve it, newest first.
    publish.thumbnail_hash = hash.clone();
    publish.gradient_from = "#FF00AA".into();
    publish.gradient_to = "#000000".into();
    let h = signed_headers(&r.creator, "put", &path).await;
    let (status, _, v) = send(&r.app, "PUT", &path, h, Some(body(&publish))).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["outfit"]["published"], true);
    assert_eq!(v["outfit"]["gradientFrom"], "#ff00aa");
    assert_eq!(
        v["outfit"]["authorAddress"], r.creator.address,
        "the author is retained on replace"
    );
    let (status, _, v) = send(&r.app, "GET", &path, HeaderMap::new(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["outfit"]["thumbnailHash"], hash);
    let (_, headers, v) = send(
        &r.app,
        "GET",
        "/shop-api/v1/outfits?limit=abc&offset=",
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert_eq!(v["outfits"][0]["id"], id, "newest first");

    // Delete is idempotent; the thumbnail stays (shared, content-addressed).
    let h = signed_headers(&r.creator, "delete", &path).await;
    let (status, _, v) = send(&r.app, "DELETE", &path, h, None).await;
    assert_eq!((status, v), (StatusCode::OK, json!({ "ok": true })));
    let h = signed_headers(&r.creator, "delete", &path).await;
    let (status, _, _) = send(&r.app, "DELETE", &path, h, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&r.app, "GET", &path, HeaderMap::new(), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send_raw(
        &r.app,
        "GET",
        &format!("/shop-api/v1/outfits/thumbnails/{hash}"),
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A signature over another path does not open this route.
    let other = new_id();
    let h = signed_headers(&r.creator, "post", "/shop-api/v1/outfits/thumbnails").await;
    let (status, _, _) = send(
        &r.app,
        "POST",
        "/shop-api/v1/outfits",
        h,
        Some(body(&request(&other))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_write_gate_holds_the_writes_and_leaves_the_reads_open() {
    let Some(r) = rig(false).await else { return };
    let id = new_id();
    let h = signed_headers(&r.creator, "post", "/shop-api/v1/outfits").await;
    let (status, headers, v) = send(
        &r.app,
        "POST",
        "/shop-api/v1/outfits",
        h,
        Some(body(&request(&id))),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("DATA_WRITES_ENABLED")
    );
    let (status, _, v) = send(
        &r.app,
        "GET",
        "/shop-api/v1/outfits",
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["outfits"].is_array());
}
