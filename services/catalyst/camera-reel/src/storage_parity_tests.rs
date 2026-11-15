use super::*;
use crate::db;

#[tokio::test]
async fn image_urls_redirect_without_a_database_or_existing_object() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/camera_redirect_test")
        .unwrap();
    pool.close().await;
    let directory = std::env::temp_dir().join(format!("camera-redirect-{}", Uuid::new_v4()));
    let storage = Arc::new(Storage::from_config(&directory).await.unwrap());
    for bucket in [
        None,
        Some("https://bucket.example"),
        Some("https://bucket.example/"),
    ] {
        let mut settings = Settings::new(API_URL);
        settings.bucket_url = bucket.map(str::to_owned);
        let state = Arc::new(State::new(pool.clone(), storage.clone(), settings, None));
        let app = Router::new().nest(PREFIX, router(state));
        for (name, decoded) in [
            ("absent", "absent"),
            ("absent%23part.png", "absent#part.png"),
            ("absent%3Fsize.png", "absent?size.png"),
            ("absent%252Fname.png", "absent%2Fname.png"),
            ("absent%2Fname.png", "absent/name.png"),
            ("absent%2523name.png", "absent%23name.png"),
            ("absent%20name.png", "absent name.png"),
            ("caf%C3%A9.png", "café.png"),
            ("quoted%22name.png", "quoted\"name.png"),
        ] {
            let (status, headers, body) = send(
                app.clone(),
                "GET",
                &format!("{PREFIX}/api/images/{name}"),
                vec![],
                None,
            )
            .await;
            assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
            assert!(body.is_empty());
            let target = match bucket {
                Some(bucket) => format!("{}/{decoded}", bucket.trim_end_matches('/')),
                None => format!("{API_URL}/api/files/{decoded}"),
            };
            assert_eq!(headers[header::LOCATION].as_bytes(), target.as_bytes());
        }
    }
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

async fn follow_photo(app: Router, url: &str) -> (StatusCode, Vec<u8>) {
    let path = url.strip_prefix("http://127.0.0.1:5149").unwrap();
    let (status, headers, body) = send(app.clone(), "GET", path, vec![], None).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert!(body.is_empty());
    let target = headers[header::LOCATION]
        .to_str()
        .unwrap()
        .strip_prefix("http://127.0.0.1:5149")
        .unwrap();
    let (status, _, body) = send(app, "GET", target, vec![], None).await;
    (status, body)
}

#[tokio::test]
async fn uploaded_filename_aliases_follow_to_immutable_bytes_and_obey_moderation() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.bucket_url = Some(format!("{API_URL}/api/files"));
    settings.admin_token = Some("moderator-secret".to_string());
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let owner_wallet = wallet();
    let owner = s.owner(&owner_wallet.address);
    let bytes = png(1280, 720, 19);
    let (mut body, content_type) =
        multipart(&bytes, "image/png", &metadata_json(&owner, ""), Some(false));
    let start = body
        .windows(b"shot.png".len())
        .position(|bytes| bytes == b"shot.png")
        .unwrap();
    body.splice(
        start..start + b"shot.png".len(),
        b"photo-123.png".iter().copied(),
    );
    let path = format!("{PREFIX}/api/images");
    let (status, _, body) = send(
        s.app(),
        "POST",
        &path,
        signed_headers(&owner_wallet, "post", &path, true).await,
        Some((body, &content_type)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let uploaded = json_of(&body);
    let id = uploaded["image"]["id"].as_str().unwrap();
    let image_url = uploaded["image"]["url"].as_str().unwrap();
    let thumb_url = uploaded["image"]["thumbnailUrl"].as_str().unwrap();
    assert_eq!(
        image_url,
        format!("{API_URL}/api/images/{id}-photo-123.png")
    );
    assert_eq!(
        thumb_url,
        format!("{API_URL}/api/images/{id}-thumbnail-photo-123.png")
    );
    let stored = s.db().get_image(id).await.unwrap();
    s.hash(&stored.image_hash);
    s.hash(&stored.thumbnail_hash);
    assert_eq!(stored.url, image_url);
    assert_eq!(stored.thumbnail_url, thumb_url);
    let filename: Option<String> =
        sqlx::query_scalar("SELECT original_filename FROM camera_reel.images WHERE id=$1")
            .bind(stored.id)
            .fetch_one(s.db().pool())
            .await
            .unwrap();
    assert_eq!(filename.as_deref(), Some("photo-123.png"));
    assert_eq!(
        follow_photo(s.app(), image_url).await,
        (StatusCode::OK, bytes.clone())
    );
    // The local virtual origin currently serves complete GET responses, not the
    // S3 object protocol. Keep these cutover limits explicit in the contract test.
    let bytes_path = format!("{PREFIX}/api/files/{id}-photo-123.png");
    let (status, headers, ranged) = send(
        s.app(),
        "GET",
        &bytes_path,
        vec![("range".to_string(), "bytes=0-3".to_string())],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ranged, bytes);
    assert!(headers.get(header::CONTENT_RANGE).is_none());
    assert!(headers.get(header::ETAG).is_none());
    let (status, _, body) = send(s.app(), "HEAD", &bytes_path, vec![], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty());

    let (status, thumbnail) = follow_photo(s.app(), thumb_url).await;
    assert_eq!(status, StatusCode::OK);
    let decoded = image::load_from_memory(&thumbnail).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (640, 360));
    let legacy_url = format!("{API_URL}/api/images/{}", stored.image_hash);
    assert_eq!(
        follow_photo(s.app(), &legacy_url).await,
        (StatusCode::OK, bytes.clone())
    );

    // An altered basename is not an alias, and an unrelated blob in shared
    // storage cannot be reached by inventing a camera URL for its hash.
    let forged = format!("{API_URL}/api/images/{id}-other.png");
    assert_eq!(
        follow_photo(s.app(), &forged).await.0,
        StatusCode::NOT_FOUND
    );
    let unrelated = hash_bytes_v1(b"unrelated content");
    s.state
        .storage
        .store(&unrelated, bytes::Bytes::from_static(b"unrelated content"))
        .await
        .unwrap();
    assert_eq!(
        follow_photo(s.app(), &format!("{API_URL}/api/images/{unrelated}"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let review_path = format!("{PREFIX}/admin/images/{id}/review");
    let admin = vec![(
        "authorization".to_string(),
        "Bearer moderator-secret".to_string(),
    )];
    for verdict in ["rejected", "ok"] {
        let review = json!({"reviewStatus":verdict}).to_string().into_bytes();
        let (status, _, _) = send(
            s.app(),
            "PATCH",
            &review_path,
            admin.clone(),
            Some((review, "application/json")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let expected = if verdict == "rejected" {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
        assert_eq!(follow_photo(s.app(), image_url).await.0, expected);
        assert_eq!(follow_photo(s.app(), thumb_url).await.0, expected);
    }
    // An operator denylist entry applies after resolving a filename to its hash.
    s.state.denylist.operator_insert(&stored.image_hash);
    assert_eq!(
        follow_photo(s.app(), image_url).await.0,
        StatusCode::NOT_FOUND
    );
    s.state.denylist.operator_remove(&stored.image_hash);
    let delete = format!("{PREFIX}/api/images/{id}");
    let (status, _, _) = send(
        s.app(),
        "DELETE",
        &delete,
        signed_headers(&owner_wallet, "delete", &delete, true).await,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        follow_photo(s.app(), image_url).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        follow_photo(s.app(), thumb_url).await.0,
        StatusCode::NOT_FOUND
    );
    assert!(s.state.storage.exist(&stored.image_hash).await.unwrap());
    s.done().await;
}

#[tokio::test]
async fn filename_migration_preserves_existing_hash_urls_and_can_be_reapplied() {
    let Some(pool) = catalyst_db::scratch::isolated_pool(4).await else {
        return;
    };
    // This database belongs only to this test. Model the pre-migration table
    // before inserting the photo, so the upgrade must preserve an existing row.
    sqlx::query("ALTER TABLE camera_reel.images DROP COLUMN original_filename")
        .execute(&pool)
        .await
        .unwrap();
    let directory = std::env::temp_dir().join(format!("camera-migration-{}", Uuid::new_v4()));
    let storage = Arc::new(Storage::from_config(&directory).await.unwrap());
    let state = Arc::new(State::new(pool, storage, Settings::new(API_URL), None));
    let mut s = Scratch {
        state,
        owners: Vec::new(),
        hashes: Vec::new(),
        dir: directory,
    };
    let owner = s.owner(&random_address());
    let bytes = png(32, 32, 29);
    let hash = s.hash(&hash_bytes_v1(&bytes));
    s.state
        .storage
        .store(&hash, bytes.clone().into())
        .await
        .unwrap();
    let old = image(&owner, &hash, &hash, true, "");
    s.db().insert_image(&old).await.unwrap();
    const MIGRATION: &str = include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/schemas/migrations/0045_camera_reel_original_filename.sql"
    ));
    sqlx::raw_sql(MIGRATION)
        .execute(s.db().pool())
        .await
        .unwrap();
    let upgraded = s.db().get_image(&old.id.to_string()).await.unwrap();
    let old_url = format!("{API_URL}/api/images/{hash}");
    assert_eq!(upgraded.url, old_url);
    assert_eq!(upgraded.thumbnail_url, old_url);
    assert_eq!(
        follow_photo(s.app(), &old_url).await,
        (StatusCode::OK, bytes.clone())
    );
    let filename: Option<String> =
        sqlx::query_scalar("SELECT original_filename FROM camera_reel.images WHERE id=$1")
            .bind(old.id)
            .fetch_one(s.db().pool())
            .await
            .unwrap();
    assert!(filename.is_none());

    let named = image(&owner, &hash, &hash, true, "");
    assert_eq!(
        s.db()
            .insert_image_within_limit_named(&named, 500, Some("new.png"), None)
            .await
            .unwrap(),
        Some(2)
    );
    sqlx::raw_sql(MIGRATION)
        .execute(s.db().pool())
        .await
        .unwrap();
    assert_eq!(
        s.db().get_image(&old.id.to_string()).await.unwrap().url,
        old_url
    );
    let new_url = format!("{API_URL}/api/images/{}-new.png", named.id);
    assert_eq!(
        s.db().get_image(&named.id.to_string()).await.unwrap().url,
        new_url
    );
    assert_eq!(
        follow_photo(s.app(), &new_url).await,
        (StatusCode::OK, bytes)
    );
    assert_eq!(s.count(&owner).await, 2);
    s.done().await;
}

#[tokio::test]
async fn filename_urls_preserve_raw_names_in_upload_gallery_and_redirects() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.bucket_url = Some(format!("{API_URL}/api/files"));
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let owner_wallet = wallet();
    let owner = s.owner(&owner_wallet.address);
    let bytes = png(12, 8, 117);
    for (filename, encoded) in [
        ("shot#part.png", "shot%23part.png"),
        ("shot?size.png", "shot%3Fsize.png"),
        ("shot%2Fname.png", "shot%252Fname.png"),
        ("shot name.png", "shot%20name.png"),
        ("café.png", "caf%C3%A9.png"),
        ("quoted\"name.png", "quoted%22name.png"),
        ("percent%name.png", "percent%25name.png"),
    ] {
        let (mut body, content_type) =
            multipart(&bytes, "image/png", &metadata_json(&owner, ""), Some(true));
        let start = body
            .windows(b"shot.png".len())
            .position(|bytes| bytes == b"shot.png")
            .unwrap();
        let escaped = filename.replace('"', "\\\"");
        body.splice(
            start..start + b"shot.png".len(),
            escaped.as_bytes().iter().copied(),
        );
        let path = format!("{PREFIX}/api/images");
        let (status, _, body) = send(
            s.app(),
            "POST",
            &path,
            signed_headers(&owner_wallet, "post", &path, true).await,
            Some((body, &content_type)),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{filename}: {}",
            String::from_utf8_lossy(&body)
        );
        let uploaded = json_of(&body);
        let id = uploaded["image"]["id"].as_str().unwrap();
        let url = uploaded["image"]["url"].as_str().unwrap();
        assert_eq!(url, format!("{API_URL}/api/images/{id}-{filename}"));
        let stored = s.db().get_image(id).await.unwrap();
        s.hash(&stored.image_hash);
        s.hash(&stored.thumbnail_hash);
        assert_eq!(stored.url, url);
        let page = s
            .db()
            .get_user_images_page(&owner, &Page::Offset(0), 100, false)
            .await
            .unwrap();
        assert_eq!(
            page.images
                .iter()
                .find(|image| image.id.to_string() == id)
                .unwrap()
                .url,
            url
        );
        // The source wire URL is literal even for '?' and '#'. Construct a
        // client URI explicitly to request the whole object name, then inspect
        // Location without parsing away those filename characters.
        let (status, headers, _) = send(
            s.app(),
            "GET",
            &format!("{PREFIX}/api/images/{id}-{encoded}"),
            vec![],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            headers[header::LOCATION].as_bytes(),
            format!("{API_URL}/api/files/{id}-{filename}").as_bytes()
        );
        let (status, _, body) = send(
            s.app(),
            "GET",
            &format!("{PREFIX}/api/files/{id}-{encoded}"),
            vec![],
            None,
        )
        .await;
        assert_eq!((status, body), (StatusCode::OK, bytes.clone()));
    }
    s.done().await;
}

#[tokio::test]
async fn configured_bucket_preserves_known_and_unknown_object_names() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.bucket_url = Some("https://bucket.example/api/files".into());
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let owner_wallet = wallet();
    let owner = s.owner(&owner_wallet.address);
    let bytes = png(12, 8, 118);
    let (body, content_type) =
        multipart(&bytes, "image/png", &metadata_json(&owner, ""), Some(false));
    let path = format!("{PREFIX}/api/images");
    let (status, _, body) = send(
        s.app(),
        "POST",
        &path,
        signed_headers(&owner_wallet, "post", &path, true).await,
        Some((body, &content_type)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let uploaded = json_of(&body);
    let id = uploaded["image"]["id"].as_str().unwrap();
    let stored = s.db().get_image(id).await.unwrap();
    s.hash(&stored.image_hash);
    s.hash(&stored.thumbnail_hash);
    for (url, hash) in [
        (&stored.url, &stored.image_hash),
        (&stored.thumbnail_url, &stored.thumbnail_hash),
    ] {
        let path = url.strip_prefix("http://127.0.0.1:5149").unwrap();
        let (status, headers, _) = send(s.app(), "GET", path, vec![], None).await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            headers[header::LOCATION],
            format!(
                "https://bucket.example/api/files/{}",
                url.rsplit('/').next().unwrap()
            )
        );
        assert!(s.state.storage.exist(hash).await.unwrap());
    }
    let (status, headers, _) = send(
        s.app(),
        "GET",
        &format!("{PREFIX}/api/images/absent%23file.png"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        headers[header::LOCATION],
        "https://bucket.example/api/files/absent#file.png"
    );
    s.done().await;
}

#[tokio::test]
async fn reupload_releases_deleted_bytes_but_retains_other_denials() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    settings.admin_token = Some("moderator-secret".into());
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let signer = wallet();
    let owner = s.owner(&signer.address);
    for (index, scenario) in [
        "deleted",
        "rejected",
        "operator",
        "remote",
        "deleted-operator",
        "deleted-remote",
        "deleted-rejected-reference",
        "deleted-rejected-owner-delete",
        "deleted-rejected-admin-delete",
    ]
    .into_iter()
    .enumerate()
    {
        let bytes = png(12, 8, 160 + index as u8);
        let upload_path = format!("{PREFIX}/api/images");
        let mut previous_id = None;
        for attempt in 0..2 {
            let (form, mime) =
                multipart(&bytes, "image/png", &metadata_json(&owner, ""), Some(true));
            let (status, _, body) = send(
                s.app(),
                "POST",
                &upload_path,
                signed_headers(&signer, "post", &upload_path, true).await,
                Some((form, &mime)),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{scenario}: {}",
                String::from_utf8_lossy(&body)
            );
            let uploaded = json_of(&body);
            let id = uploaded["image"]["id"].as_str().unwrap();
            let stored = s.db().get_image(id).await.unwrap();
            s.hash(&stored.image_hash);
            s.hash(&stored.thumbnail_hash);
            if attempt == 0 {
                previous_id = Some(id.to_owned());
                if scenario.contains("operator") {
                    s.state.denylist.operator_insert(&stored.image_hash);
                    s.state.denylist.operator_insert(&stored.thumbnail_hash);
                }
                if scenario.contains("remote") {
                    s.state.denylist.set_remote(
                        "https://moderation.example/list",
                        [stored.image_hash.clone(), stored.thumbnail_hash.clone()],
                    );
                }
                if scenario == "rejected" {
                    let path = format!("{PREFIX}/admin/images/{id}/review");
                    let (status, _, _) = send(
                        s.app(),
                        "PATCH",
                        &path,
                        vec![("authorization".into(), "Bearer moderator-secret".into())],
                        Some((
                            br#"{"reviewStatus":"rejected"}"#.to_vec(),
                            "application/json",
                        )),
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK);
                }
                if scenario.starts_with("deleted") {
                    let path = format!("{PREFIX}/api/images/{id}");
                    let (status, _, _) = send(
                        s.app(),
                        "DELETE",
                        &path,
                        signed_headers(&signer, "delete", &path, true).await,
                        None,
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK);
                    assert!(!s.db().hash_is_servable(&stored.image_hash).await.unwrap());
                    assert!(
                        !s.db()
                            .hash_is_servable(&stored.thumbnail_hash)
                            .await
                            .unwrap()
                    );
                    if scenario.starts_with("deleted-rejected") {
                        // Model rejection after row insertion but before upload
                        // finalization releases the earlier deletion record.
                        let rejected = NewImage {
                            id: Uuid::new_v4(),
                            image_hash: stored.image_hash.clone(),
                            thumbnail_hash: stored.thumbnail_hash.clone(),
                            is_public: false,
                            metadata: stored.metadata.0.clone(),
                        };
                        s.db().insert_image(&rejected).await.unwrap();
                        let verdict = s
                            .db()
                            .update_image_review_status(
                                &rejected.id.to_string(),
                                "rejected",
                                chrono::Utc::now().naive_utc(),
                            )
                            .await
                            .unwrap()
                            .unwrap();
                        s.state.apply_verdict(&verdict);
                        for hash in [&stored.image_hash, &stored.thumbnail_hash] {
                            assert_eq!(s.withheld(hash).await.as_deref(), Some(WITHHELD_REJECTED));
                        }
                        if scenario.ends_with("-delete") {
                            let (path, headers) = if scenario == "deleted-rejected-admin-delete" {
                                (
                                    format!("{PREFIX}/admin/images/{}", rejected.id),
                                    vec![(
                                        "authorization".into(),
                                        "Bearer moderator-secret".into(),
                                    )],
                                )
                            } else {
                                let path = format!("{PREFIX}/api/images/{}", rejected.id);
                                let headers = signed_headers(&signer, "delete", &path, true).await;
                                (path, headers)
                            };
                            let (status, _, _) =
                                send(s.app(), "DELETE", &path, headers, None).await;
                            assert_eq!(status, StatusCode::OK, "{scenario}");
                            for hash in [&stored.image_hash, &stored.thumbnail_hash] {
                                assert_eq!(
                                    s.withheld(hash).await.as_deref(),
                                    Some(WITHHELD_REJECTED),
                                    "{scenario}: deleting the rejected row must not downgrade its denial"
                                );
                            }
                        }
                    }
                }
            } else {
                assert_ne!(Some(id), previous_id.as_deref());
                let expected = if scenario == "deleted" {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                };
                for field in ["url", "thumbnailUrl"] {
                    assert_eq!(
                        follow_photo(s.app(), uploaded["image"][field].as_str().unwrap())
                            .await
                            .0,
                        expected,
                        "{scenario}: {field}"
                    );
                }
                for hash in [&stored.image_hash, &stored.thumbnail_hash] {
                    assert_eq!(
                        s.state.storage.withholds(hash),
                        scenario != "deleted",
                        "{scenario}"
                    );
                    if scenario == "deleted" {
                        assert_eq!(s.withheld(hash).await, None);
                        assert!(s.db().hash_is_servable(hash).await.unwrap());
                    }
                    s.state.denylist.operator_remove(hash);
                }
                s.state
                    .denylist
                    .set_remote("https://moderation.example/list", Vec::new());
                if scenario.starts_with("deleted-rejected") {
                    // A new projection loads the rejection even when its original
                    // photo was deleted; the newly uploaded aliases stay blocked.
                    let storage = Arc::new(Storage::from_config(&s.dir).await.unwrap());
                    let restarted = Arc::new(State::new(
                        s.db().pool().clone(),
                        storage,
                        Settings::new(API_URL),
                        None,
                    ));
                    restarted.load_withheld().await.unwrap();
                    for hash in [&stored.image_hash, &stored.thumbnail_hash] {
                        assert_eq!(s.withheld(hash).await.as_deref(), Some(WITHHELD_REJECTED));
                        assert!(restarted.storage.withholds(hash));
                    }
                    let app = Router::new().nest(PREFIX, router(restarted));
                    for field in ["url", "thumbnailUrl"] {
                        assert_eq!(
                            follow_photo(app.clone(), uploaded["image"][field].as_str().unwrap())
                                .await
                                .0,
                            StatusCode::NOT_FOUND,
                            "{scenario}: restart {field}"
                        );
                    }
                    // An explicit moderator restore keeps its existing authority
                    // to release the camera rejection, unlike a fresh upload.
                    let path = format!("{PREFIX}/admin/images/{id}/review");
                    let (status, _, _) = send(
                        s.app(),
                        "PATCH",
                        &path,
                        vec![("authorization".into(), "Bearer moderator-secret".into())],
                        Some((br#"{"reviewStatus":"ok"}"#.to_vec(), "application/json")),
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK);
                    for hash in [&stored.image_hash, &stored.thumbnail_hash] {
                        assert_eq!(s.withheld(hash).await, None);
                    }
                    for field in ["url", "thumbnailUrl"] {
                        assert_eq!(
                            follow_photo(s.app(), uploaded["image"][field].as_str().unwrap())
                                .await
                                .0,
                            StatusCode::OK,
                            "{scenario}: explicit restore {field}"
                        );
                    }
                }
            }
        }
    }
    s.done().await;
}

#[test]
fn configured_api_base_and_filename_are_literal_wire_values() {
    for base in [
        "https://camera.example.test",
        "https://camera.example.test/",
    ] {
        let settings = Settings::new(base);
        assert_eq!(settings.api_url, base);
        for filename in ["café 'x$()?#%2F.png", "literal%252F.png"] {
            assert_eq!(
                db::image_url(&settings.api_url, filename),
                format!("{base}/api/images/{filename}")
            );
        }
    }
}

#[tokio::test]
async fn a_photo_alias_retains_its_mime_and_bytes_of_another_format_are_refused() {
    let mut settings = Settings::new(API_URL);
    settings.writes_enabled = true;
    let Some(mut s) = scratch(settings).await else {
        return;
    };
    let signer = wallet();
    let owner = s.owner(&signer.address);
    let bytes = png(12, 8, 191);
    let mut photos = Vec::new();
    for claimed in ["image/png", "image/jpeg"] {
        let (form, mime) = multipart(&bytes, claimed, &metadata_json(&owner, ""), Some(true));
        let path = format!("{PREFIX}/api/images");
        let (status, _, body) = send(
            s.app(),
            "POST",
            &path,
            signed_headers(&signer, "post", &path, true).await,
            Some((form, &mime)),
        )
        .await;
        if claimed == "image/jpeg" {
            // The signature must match the declared MIME (ADR camera service compatibility).
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(json_of(&body), json!({ "message": "invalid image format" }));
            continue;
        }
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let uploaded = json_of(&body);
        let id = uploaded["image"]["id"].as_str().unwrap().to_owned();
        let row = s.db().get_image(&id).await.unwrap();
        s.hash(&row.image_hash);
        s.hash(&row.thumbnail_hash);
        photos.push((id, row.image_hash, row.thumbnail_hash, claimed));
    }
    assert_eq!(photos.len(), 1);
    assert_eq!(s.count(&owner).await, 1);
    for (id, _, _, claimed) in &photos {
        let retained: Option<String> =
            sqlx::query_scalar("SELECT image_content_type FROM camera_reel.images WHERE id=$1")
                .bind(Uuid::parse_str(id).unwrap())
                .fetch_one(s.db().pool())
                .await
                .unwrap();
        assert_eq!(retained.as_deref(), Some(*claimed));
        for variant in ["", "thumbnail-"] {
            let (status, headers, body) = send(
                s.app(),
                "GET",
                &format!("{PREFIX}/api/files/{id}-{variant}shot.png"),
                vec![],
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], *claimed);
            assert_eq!(image::guess_format(&body).unwrap(), image::ImageFormat::Png);
            if variant.is_empty() {
                assert_eq!(body, bytes);
            }
        }
    }
    s.done().await;
}

#[tokio::test]
async fn content_type_migration_preserves_legacy_rows_and_retained_metadata() {
    let Some(pool) = catalyst_db::scratch::isolated_pool(4).await else {
        return;
    };
    // This fresh isolated database models a pre-step row; never reset a shared DB.
    sqlx::query("ALTER TABLE camera_reel.images DROP COLUMN image_content_type")
        .execute(&pool)
        .await
        .unwrap();
    let directory = std::env::temp_dir().join(format!("camera-mime-migration-{}", Uuid::new_v4()));
    let storage = Arc::new(Storage::from_config(&directory).await.unwrap());
    let state = Arc::new(State::new(pool, storage, Settings::new(API_URL), None));
    let mut s = Scratch {
        state,
        owners: Vec::new(),
        hashes: Vec::new(),
        dir: directory,
    };
    let owner = s.owner(&random_address());
    let bytes = png(12, 8, 192);
    let hash = s.hash(&hash_bytes_v1(&bytes));
    s.state
        .storage
        .store(&hash, bytes.clone().into())
        .await
        .unwrap();
    let old = image(&owner, &hash, &hash, true, "");
    s.db().insert_image(&old).await.unwrap();
    sqlx::query("UPDATE camera_reel.images SET original_filename='legacy.png' WHERE id=$1")
        .bind(old.id)
        .execute(s.db().pool())
        .await
        .unwrap();
    const MIGRATION: &str = include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/schemas/migrations/0044_camera_reel_image_content_type.sql"
    ));
    sqlx::raw_sql(MIGRATION)
        .execute(s.db().pool())
        .await
        .unwrap();
    let retained: Option<String> =
        sqlx::query_scalar("SELECT image_content_type FROM camera_reel.images WHERE id=$1")
            .bind(old.id)
            .fetch_one(s.db().pool())
            .await
            .unwrap();
    assert_eq!(retained, None);
    let path = format!("{PREFIX}/api/files/{}-legacy.png", old.id);
    let (status, headers, body) = send(s.app(), "GET", &path, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(body, bytes);
    sqlx::query("UPDATE camera_reel.images SET image_content_type='image/jpeg' WHERE id=$1")
        .bind(old.id)
        .execute(s.db().pool())
        .await
        .unwrap();
    // Reapplied as `catalyst schema migrate` does: a constraint that exists
    // is skipped, the rest of the step is idempotent as written.
    let reapplied: String = MIGRATION
        .lines()
        .filter(|line| !line.contains("ADD CONSTRAINT"))
        .collect::<Vec<_>>()
        .join("\n");
    sqlx::raw_sql(sqlx::AssertSqlSafe(reapplied))
        .execute(s.db().pool())
        .await
        .unwrap();
    let (status, headers, body) = send(s.app(), "GET", &path, vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(body, bytes);
    assert_eq!(
        s.db().get_image(&old.id.to_string()).await.unwrap().url,
        format!("{API_URL}/api/images/{}-legacy.png", old.id)
    );
    assert_eq!(s.count(&owner).await, 1);
    let invalid =
        sqlx::query("UPDATE camera_reel.images SET image_content_type='image/webp' WHERE id=$1")
            .bind(old.id)
            .execute(s.db().pool())
            .await
            .unwrap_err();
    assert_eq!(
        invalid.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    s.done().await;
}
