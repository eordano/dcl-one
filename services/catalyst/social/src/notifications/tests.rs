//! Route-level contracts over the router as `serve` mounts it (nested under
//! `/notifications`), signed with the crate's test wallet. DB-backed tests
//! use the process's scratch database (`CATALYST_TEST_DATABASE_URL`) and
//! rows scoped by fresh addresses or timestamps; without the variable they
//! skip.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::email::INVALID_EMAIL;
use super::store::test_support::{fresh_address, seed_notification};
use super::types::default_message_type;
use super::{Settings, SharedState, State, router};
use crate::identity::Address;
use crate::shared::signed_fetch::test_support::{
    address as signer_address, signed_headers, signed_headers_by,
};

async fn state(settings: Settings) -> Option<SharedState> {
    let pool = crate::db::scratch::pool(4).await?;
    Some(Arc::new(State::new(
        pool,
        settings,
        crate::http::Outbound::for_tests(),
        None,
        None,
    )))
}

fn app(state: SharedState) -> Router {
    Router::new().nest("/notifications", router(state))
}

async fn send(
    app: Router,
    method: &str,
    path: &str,
    headers: HeaderMap,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    for (k, v) in headers.iter() {
        req = req.header(k, v);
    }
    let req = match body {
        Some(b) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn signed(method: &str, path: &str) -> HeaderMap {
    signed_headers(method, path, r#"{"signer":"dcl:explorer"}"#, true).await
}

fn signer() -> Address {
    Address::parse(&signer_address()).unwrap()
}

#[tokio::test]
async fn unsigned_scene_signed_and_missigned_requests_are_refused() {
    let Some(state) = state(Settings::default()).await else {
        return;
    };
    let (status, body) = send(
        app(state.clone()),
        "GET",
        "/notifications/notifications",
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body,
        json!({
            "error": "Invalid Auth Chain",
            "message": "This endpoint requires a signed fetch request. See ADR-44."
        })
    );

    for metadata in [
        r#"{"signer":"decentraland-kernel-scene"}"#,
        r#"{"Signer":"decentraland-kernel-scene"}"#,
    ] {
        for v6 in [true, false] {
            let headers = signed_headers("get", "/notifications/notifications", metadata, v6).await;
            let (status, body) = send(
                app(state.clone()),
                "GET",
                "/notifications/notifications",
                headers,
                None,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "v6={v6} {metadata}: {body}"
            );
        }
    }

    let headers = signed("get", "/notifications/subscription").await;
    let (status, body) = send(
        app(state.clone()),
        "GET",
        "/notifications/notifications",
        headers,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], json!("Unauthenticated: Invalid signature"));

    // notifications.<domain>/notifications and gateway.<domain>/notifications/notifications
    // are signed over the route alone; the front moves both under /notifications and
    // forwards the signed path.
    let mut headers = signed("get", "/notifications").await;
    headers.insert("x-original-path", "/notifications".parse().unwrap());
    let (status, body) = send(
        app(state.clone()),
        "GET",
        "/notifications/notifications",
        headers,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A proxy that strips the prefix forwards the public path.
    let mut headers = signed("get", "/notifications/notifications").await;
    headers.insert(
        "x-original-path",
        "/notifications/notifications".parse().unwrap(),
    );
    let (status, _) = send(
        Router::new().merge(router(state)),
        "GET",
        "/notifications",
        headers,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn signed_list_read_subscription_and_opt_outs_round_trip() {
    let Some(state) = state(Settings::default()).await else {
        return;
    };
    let pool = state.store.pool().clone();
    let me = signer();
    // The test wallet is shared by this process's tests: scope by a window
    // far in the future, unique to this test.
    let base = chrono::Utc::now().timestamp_millis()
        + (Uuid::new_v4().as_u128() % 1_000_000_000) as i64
        + 1_000_000_000_000;
    let older = seed_notification(&pool, &me, base + 1, false).await;
    let newer = seed_notification(&pool, &me, base + 2, false).await;
    seed_notification(&pool, &fresh_address(), base + 3, false).await;

    let list_path = format!("/notifications/notifications?from={base}&limit=10");
    let (status, body) = send(
        app(state.clone()),
        "GET",
        &list_path,
        signed("get", "/notifications/notifications").await,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["notifications"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        vec![newer.to_string(), older.to_string()]
    );
    assert_eq!(items[0]["timestamp"], json!((base + 2).to_string()));
    assert_eq!(items[0]["type"], json!("bid_received"));
    assert_eq!(items[0]["address"], json!(me.as_str()));
    assert_eq!(items[0]["read"], json!(false));
    assert!(items[0]["created_at"].as_str().unwrap().ends_with('Z'));

    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/notifications/notifications/read",
        signed("put", "/notifications/notifications/read").await,
        Some(json!({ "notificationIds": [newer.to_string(), older.to_string(), Uuid::new_v4().to_string()] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "updated": 2 }));
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/notifications/notifications/read",
        signed("put", "/notifications/notifications/read").await,
        Some(json!({ "notificationIds": ["not-a-uuid"] })),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid notification id"))
    );

    let unread_path = format!("/notifications/notifications?from={base}&onlyUnread=false");
    let (_, body) = send(
        app(state.clone()),
        "GET",
        &unread_path,
        signed("get", "/notifications/notifications").await,
        None,
    )
    .await;
    assert_eq!(body["notifications"].as_array().unwrap().len(), 0);

    sqlx::query("DELETE FROM social.subscriptions WHERE address = $1")
        .bind(&me)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = send(
        app(state.clone()),
        "GET",
        "/notifications/subscription",
        signed("get", "/notifications/subscription").await,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["address"], json!(me.as_str()));
    assert!(body.get("email").is_none() && body.get("unconfirmedEmail").is_none());
    assert_eq!(body["details"]["ignore_all_email"], json!(false));
    assert_eq!(
        body["details"]["message_type"]["bid_received"],
        json!({"email": true, "in_app": true})
    );

    let mut details = json!({ "ignore_all_email": true, "ignore_all_in_app": false, "message_type": default_message_type() });
    details["message_type"]["item_sold"] = json!({"email": false, "in_app": true});
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/notifications/subscription",
        signed("put", "/notifications/subscription").await,
        Some(details),
    )
    .await;
    assert_eq!((status, body), (StatusCode::NO_CONTENT, Value::Null));
    // An older client's partial list keeps every type it does not send.
    let partial = json!({ "ignore_all_email": false, "ignore_all_in_app": false,
        "message_type": { "bid_received": { "email": false, "in_app": false } } });
    let (status, _) = send(
        app(state.clone()),
        "PUT",
        "/notifications/subscription",
        signed("put", "/notifications/subscription").await,
        Some(partial),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = send(
        app(state.clone()),
        "GET",
        "/notifications/subscription",
        signed("get", "/notifications/subscription").await,
        None,
    )
    .await;
    assert_eq!(
        body["email"],
        Value::Null,
        "a row without an email answers null"
    );
    assert!(body.as_object().unwrap().contains_key("email"), "{body}");
    assert_eq!(body["details"]["ignore_all_email"], json!(false));
    assert_eq!(
        body["details"]["message_type"]["item_sold"],
        json!({"email": false, "in_app": true})
    );
    assert_eq!(
        body["details"]["message_type"]["bid_received"],
        json!({"email": false, "in_app": false})
    );
    for (bad, message) in [
        (
            json!({ "ignore_all_email": "yes", "ignore_all_in_app": false, "message_type": {} }),
            "Invalid subscription. ignore_all_email must be boolean",
        ),
        (
            json!({ "ignore_all_email": false, "ignore_all_in_app": false, "message_type": { "made_up": {} } }),
            "Invalid subscription. message_type must NOT have additional properties",
        ),
    ] {
        let (status, body) = send(
            app(state.clone()),
            "PUT",
            "/notifications/subscription",
            signed("put", "/notifications/subscription").await,
            Some(bad),
        )
        .await;
        assert_eq!(
            body,
            json!({ "error": "Bad request", "message": message }),
            "{status}"
        );
    }

    let community = Uuid::new_v4().to_string();
    let scoped = format!("/notifications/subscription/opt-outs/community/{community}");
    let (status, body) = send(
        app(state.clone()),
        "POST",
        "/notifications/subscription/opt-outs",
        signed("post", "/notifications/subscription/opt-outs").await,
        Some(json!({ "scope": "community", "scopeId": community })),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::CREATED,
            json!({ "scope": "community", "scopeId": community, "optedOut": true })
        )
    );
    for (bad, keyword) in [
        (json!({ "scope": "world", "scopeId": "x" }), "enum"),
        (
            json!({ "scope": "community", "scopeId": "x", "extra": 1 }),
            "additionalProperties",
        ),
        (json!({ "scope": "community" }), "required"),
        (json!({ "scope": "community", "scopeId": "" }), "minLength"),
    ] {
        let (status, body) = send(
            app(state.clone()),
            "POST",
            "/notifications/subscription/opt-outs",
            signed("post", "/notifications/subscription/opt-outs").await,
            Some(bad.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(body["ok"], json!(false), "{body}");
        assert_eq!(body["message"], json!("Invalid JSON body"), "{body}");
        assert_eq!(body["data"][0]["keyword"], json!(keyword), "{body}");
    }
    let planet = format!("/notifications/subscription/opt-outs/planet/{community}");
    let (status, body) = send(
        app(state.clone()),
        "GET",
        &planet,
        signed("get", &planet).await,
        None,
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": "Bad request", "message": "Invalid scope" })
        )
    );
    let (status, body) = send(
        app(state.clone()),
        "GET",
        &scoped,
        signed("get", &scoped).await,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "scope": "community", "scopeId": community, "optedOut": true })
    );
    let (status, _) = send(
        app(state.clone()),
        "DELETE",
        &scoped,
        signed("delete", &scoped).await,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = send(
        app(state.clone()),
        "GET",
        &scoped,
        signed("get", &scoped).await,
        None,
    )
    .await;
    assert_eq!(body["optedOut"], json!(false));
}

#[tokio::test]
async fn moderation_notices_reach_signed_recipient_reader_and_marking_stays_wallet_scoped() {
    use crate::comms::interfaces::{
        ModerationNotification, ModerationNotificationKind as Kind, SocialDirectory,
    };
    let Some(state) = state(Settings::default()).await else {
        eprintln!("GUARDED moderation signed-reader assertions: CATALYST_TEST_DATABASE_URL unset");
        return;
    };
    let wallet = alloy::signers::local::PrivateKeySigner::random();
    let recipient = Address::parse(&format!("{:#x}", wallet.address())).unwrap();
    let pool = state.store.pool().clone();
    let directory = crate::comms_directory::Directory::new(pool.clone());
    let time = chrono::DateTime::from_timestamp_millis(1_700_000_000_123)
        .unwrap()
        .naive_utc();
    let mut txn = pool.begin().await.unwrap();
    for event in [
        Kind::Banned {
            reason: "abuse".into(),
            banned_at: 123,
            expires_at: None,
            custom_message: None,
        },
        Kind::BanLifted { lifted_at: 124 },
        Kind::BanWarning {
            reason: "warning".into(),
            warned_at: 125,
        },
        Kind::UserBannedFromScene {
            place_title: "Exact Scene".into(),
        },
        Kind::UserUnbannedFromScene {
            place_title: "Exact Scene".into(),
        },
    ] {
        directory
            .notify_moderation_notification(
                &mut txn,
                &ModerationNotification {
                    source_id: Uuid::new_v4().to_string(),
                    recipient: recipient.clone(),
                    occurred_at: time,
                    event,
                },
            )
            .await
            .unwrap();
    }
    let stranger = fresh_address();
    directory
        .notify_moderation_notification(
            &mut txn,
            &ModerationNotification {
                source_id: Uuid::new_v4().to_string(),
                recipient: stranger.clone(),
                occurred_at: time,
                event: Kind::BanWarning {
                    reason: "private stranger warning".into(),
                    warned_at: 126,
                },
            },
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let signed = async |method: &str, path: &str| {
        signed_headers_by(
            &wallet,
            method,
            path,
            r#"{"signer":"dcl:explorer"}"#,
            true,
            &chrono::Utc::now().timestamp_millis().to_string(),
        )
        .await
    };
    let path = "/notifications/notifications";
    let (status, response) = send(
        app(state.clone()),
        "GET",
        path,
        signed("get", path).await,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let items = response["notifications"].as_array().unwrap();
    let mut kinds: Vec<&str> = items
        .iter()
        .map(|item| {
            assert_eq!(item["address"], recipient.as_str());
            assert_eq!(item["timestamp"], "1700000000123");
            item["type"].as_str().unwrap()
        })
        .collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "ban_lifted",
            "ban_warning",
            "banned",
            "user_banned_from_scene",
            "user_unbanned_from_scene"
        ]
    );
    let stranger_id: Uuid =
        sqlx::query_scalar("SELECT id FROM social.notifications WHERE address=$1")
            .bind(&stranger)
            .fetch_one(&pool)
            .await
            .unwrap();
    let path = "/notifications/notifications/read";
    let (status, response) = send(
        app(state),
        "PUT",
        path,
        signed("put", path).await,
        Some(json!({"notificationIds":[items[0]["id"],stranger_id.to_string()]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["updated"], 1);
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT read FROM social.notifications WHERE id=$1")
            .bind(stranger_id)
            .fetch_one(&pool)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn moderator_authorization_precedes_notice_and_warning_mutation() {
    use crate::comms::gatekeeper::{CommsState, Settings as CommsSettings};
    use crate::comms::interfaces::test_support::{StubPolicy, StubScenes};
    let Some(pool) = crate::db::scratch::pool(4).await else {
        eprintln!(
            "GUARDED moderation authorization/notification assertions: CATALYST_TEST_DATABASE_URL unset"
        );
        return;
    };
    let recipient = fresh_address();
    let mut settings = CommsSettings::for_tests();
    settings.moderator_token = Some("moderation-test-token".into());
    settings.livekit.configured = false;
    let state = CommsState::new(
        pool.clone(),
        crate::http::Outbound::for_tests(),
        settings,
        None,
        None,
        Arc::new(StubPolicy::default()),
        Arc::new(StubScenes::default()),
        Arc::new(crate::comms::presence::Presence::new()),
        Arc::new(crate::comms_directory::Directory::new(pool.clone())),
    );
    let app = crate::comms::gatekeeper::router(Arc::new(state));
    let path = format!("/users/{}/warnings?moderator=Test", recipient.as_str());
    let (status, _) = send(
        app.clone(),
        "POST",
        &path,
        HeaderMap::new(),
        Some(json!({"reason":"warning"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let counts: (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM social.user_warnings WHERE warned_address=$1),(SELECT count(*) FROM social.notifications WHERE address=$1)").bind(&recipient).fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (0, 0));
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer moderation-test-token".parse().unwrap(),
    );
    let (status, response) = send(
        app,
        "POST",
        &path,
        headers,
        Some(json!({"reason":"warning"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{response}");
    let row: (String, serde_json::Value) =
        sqlx::query_as("SELECT type,metadata FROM social.notifications WHERE address=$1")
            .bind(&recipient)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        row,
        (
            "ban_warning".into(),
            json!({"reason":"warning","warnedAt":chrono::DateTime::parse_from_rfc3339(response["data"]["warnedAt"].as_str().unwrap()).unwrap().timestamp_millis()})
        )
    );
}

#[tokio::test]
async fn broadcast_is_bearer_gated_and_validated() {
    let Some(unset) = state(Settings::default()).await else {
        return;
    };
    let bearer = |token: &str| {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    };
    let body = json!({ "type": "bid_received", "metadata": {"title": "hi"}, "addresses": [fresh_address().as_str()] });
    let (status, resp) = send(
        app(unset),
        "POST",
        "/notifications/notifications/broadcast",
        bearer("s3cret"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        resp["error"],
        json!("admin broadcast disabled: CATALYRST_NOTIFICATIONS_ADMIN_TOKEN is not set")
    );

    let state = state(Settings {
        admin_token: Some("s3cret".into()),
        ..Settings::default()
    })
    .await
    .unwrap();
    for wrong in [HeaderMap::new(), bearer("nope")] {
        let (status, resp) = send(
            app(state.clone()),
            "POST",
            "/notifications/notifications/broadcast",
            wrong,
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            resp["error"],
            json!("You are not authorized to access this resource")
        );
    }
    let (status, resp) = send(
        app(state.clone()),
        "POST",
        "/notifications/notifications/broadcast",
        bearer("s3cret"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(
        (
            resp["ok"].as_bool(),
            resp["type"].as_str(),
            resp["recipients"].as_u64()
        ),
        (Some(true), Some("bid_received"), Some(1))
    );
    assert!(Uuid::parse_str(resp["broadcastId"].as_str().unwrap()).is_ok());

    // A client-supplied broadcastId is the idempotency key: the retried POST
    // reaches nobody twice and echoes the same id.
    let target = fresh_address();
    let key = format!("ops-{}", Uuid::new_v4());
    let keyed = json!({ "type": "bid_received", "metadata": {"title": "hi"}, "addresses": [target.as_str()], "broadcastId": key });
    let (status, first) = send(
        app(state.clone()),
        "POST",
        "/notifications/notifications/broadcast",
        bearer("s3cret"),
        Some(keyed.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = send(
        app(state.clone()),
        "POST",
        "/notifications/notifications/broadcast",
        bearer("s3cret"),
        Some(keyed),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(
        (first["recipients"].as_u64(), second["recipients"].as_u64()),
        (Some(1), Some(0))
    );
    assert_eq!(
        (
            first["broadcastId"].as_str(),
            second["broadcastId"].as_str()
        ),
        (Some(key.as_str()), Some(key.as_str()))
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE address = $1")
            .bind(&target)
            .fetch_one(state.store.pool())
            .await
            .unwrap();
    assert_eq!(rows, 1, "the retry inserted no second row");

    let cases = [
        (
            json!({ "type": "made_up" }),
            "unknown notification type: made_up",
        ),
        (
            json!({ "type": "bid_received", "metadata": [1] }),
            "metadata must be an object",
        ),
        (
            json!({ "type": "bid_received", "addresses": [] }),
            "addresses must be non-empty when provided (omit it to broadcast to all)",
        ),
        (
            json!({ "type": "bid_received", "addresses": ["0x12"] }),
            "invalid address: 0x12",
        ),
        (
            json!({ "type": "bid_received", "broadcastId": "  " }),
            "broadcastId must be 1 to 128 characters",
        ),
        (
            json!({ "type": "bid_received", "broadcastId": "x".repeat(129) }),
            "broadcastId must be 1 to 128 characters",
        ),
    ];
    for (body, message) in cases {
        let (status, resp) = send(
            app(state.clone()),
            "POST",
            "/notifications/notifications/broadcast",
            bearer("s3cret"),
            Some(body),
        )
        .await;
        assert_eq!(
            (status, resp["message"].as_str()),
            (StatusCode::BAD_REQUEST, Some(message))
        );
    }
}

#[tokio::test]
async fn set_email_and_confirm_email_validate_before_touching_the_mailer() {
    let Some(state) = state(Settings {
        email: super::EmailConfig {
            domain_blacklist: vec!["spam.com".into()],
            ..Settings::default().email
        },
        ..Settings::default()
    })
    .await
    else {
        return;
    };
    // Its own wallet: the failed send below leaves a pending email on the signer, which
    // the shared test wallet's subscription round trip must never see.
    let wallet = alloy::signers::local::PrivateKeySigner::random();
    let signed = async |method: &str, path: &str| {
        let now = chrono::Utc::now().timestamp_millis().to_string();
        signed_headers_by(
            &wallet,
            method,
            path,
            r#"{"signer":"dcl:explorer"}"#,
            true,
            &now,
        )
        .await
    };
    for email in [
        json!("nope"),
        json!("@x.com"),
        json!("x@"),
        json!("a+b@x.com"),
        json!("me@Spam.com"),
        json!("a.b.c@x.com"),
        json!(" a@x.com"),
        json!("a@x.com "),
        Value::Null,
    ] {
        let (status, body) = send(
            app(state.clone()),
            "PUT",
            "/notifications/set-email",
            signed("put", "/notifications/set-email").await,
            Some(json!({ "email": email })),
        )
        .await;
        assert_eq!(
            (status, body["message"].as_str()),
            (StatusCode::BAD_REQUEST, Some(INVALID_EMAIL)),
            "{email}"
        );
    }
    let (status, _) = send(
        app(state.clone()),
        "PUT",
        "/notifications/set-email",
        signed("put", "/notifications/set-email").await,
        Some(json!({ "email": "" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "clearing the email sends nothing"
    );
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/notifications/set-email",
        signed("put", "/notifications/set-email").await,
        Some(json!({ "email": format!("new-{}@example.com", Uuid::new_v4().simple()) })),
    )
    .await;
    assert_eq!(
        (status, body["error"].as_str(), body.get("message")),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Some("Internal Server Error"),
            None
        ),
        "an unconfigured mailer fails loudly"
    );

    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/notifications/confirm-email",
        HeaderMap::new(),
        Some(json!({ "code": "short" })),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (StatusCode::BAD_REQUEST, Some("Missing address"))
    );
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/notifications/confirm-email",
        HeaderMap::new(),
        Some(json!({ "address": format!("{:#x}", wallet.address()), "code": "short" })),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (StatusCode::BAD_REQUEST, Some("Missing code"))
    );
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        "/notifications/confirm-email",
        HeaderMap::new(),
        Some(json!({ "address": fresh_address().as_str(), "code": "a".repeat(32) })),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (
            StatusCode::BAD_REQUEST,
            Some("No unconfirmed email for this address")
        ),
        "no pending row"
    );
}

#[tokio::test]
async fn confirm_email_answers_the_old_messages_and_rechecks_the_blacklist() {
    let Some(state) = state(Settings {
        email: super::EmailConfig {
            domain_blacklist: vec!["spam.com".into()],
            ..Settings::default().email
        },
        ..Settings::default()
    })
    .await
    else {
        return;
    };
    let pool = state.store.pool().clone();
    let confirm = async |address: &Address, code: &str| {
        send(
            app(state.clone()),
            "PUT",
            "/notifications/confirm-email",
            HeaderMap::new(),
            Some(json!({ "address": address.as_str(), "code": code })),
        )
        .await
    };
    let code = "c".repeat(32);
    let (ok, blocked) = (fresh_address(), fresh_address());
    for (address, email) in [(&ok, "me@example.com"), (&blocked, "me@spam.com")] {
        sqlx::query("INSERT INTO social.unconfirmed_emails (address, email, code, source) VALUES ($1, $2, $3, 'account')")
            .bind(address)
            .bind(email)
            .bind(&code)
            .execute(&pool)
            .await
            .unwrap();
    }
    let (status, body) = confirm(&ok, &"d".repeat(32)).await;
    assert_eq!(
        (status, body["message"].as_str()),
        (StatusCode::BAD_REQUEST, Some("Invalid code"))
    );
    let (status, body) = confirm(&blocked, &code).await;
    assert_eq!(
        (status, body["message"].as_str()),
        (StatusCode::BAD_REQUEST, Some(INVALID_EMAIL))
    );
    let (status, _) = confirm(&ok, &code).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let sub = state.store.get_subscription(&ok).await.unwrap();
    assert_eq!(sub.email, Some(Some("me@example.com".into())));

    // A disposable domain is refused at confirm time once the list holds it.
    let disposable = fresh_address();
    sqlx::query("INSERT INTO social.unconfirmed_emails (address, email, code, source) VALUES ($1, 'me@Mailinator.com', $2, 'credits')")
        .bind(&disposable)
        .bind(&code)
        .execute(&pool)
        .await
        .unwrap();
    let list = axum::Router::new().route(
        "/list.conf",
        axum::routing::get(|| async { "# disposable\nmailinator.com\n" }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/list.conf", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, list).await });
    state
        .email
        .refresh_disposable_domains(&url, 1)
        .await
        .unwrap();
    let (status, body) = confirm(&disposable, &code).await;
    assert_eq!(
        (status, body["message"].as_str()),
        (StatusCode::BAD_REQUEST, Some(INVALID_EMAIL))
    );
    assert!(
        state
            .store
            .get_subscription(&disposable)
            .await
            .unwrap()
            .email
            .is_none(),
        "nothing confirmed"
    );
}

const SIGNING_KEY: &str = "test-signing-key";

fn bearer(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    h
}

/// The old processor's `signUrl(key, new URL(path, SERVICE_BASE_URL))`, computed independently.
fn old_signed_path(path: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let base = Settings::default().service_base_url;
    let url = url::Url::parse(&base).unwrap().join(path).unwrap();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(SIGNING_KEY.as_bytes()).unwrap();
    mac.update(url.as_str().as_bytes());
    format!(
        "{path}?signature={}",
        hex::encode(mac.finalize().into_bytes())
    )
}

async fn get_html(app: Router, path: &str) -> (StatusCode, String, String) {
    let req = Request::builder().uri(path).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, ct, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn unsubscribe_links_verify_the_old_signature_and_update_the_subscription() {
    let Some(state) = state(Settings {
        signing_key: Some(SIGNING_KEY.into()),
        ..Settings::default()
    })
    .await
    else {
        return;
    };
    let address = fresh_address();
    let one = format!("/unsubscribe/{}/bid_received", address.as_str());
    let all = format!("/unsubscribe/{}", address.as_str());

    for path in [
        all.clone(),
        format!("{all}?signature=00"),
        format!("{}x", old_signed_path(&all)),
        old_signed_path(&one).replace("bid_received", "bid_accepted"),
    ] {
        let (status, body) = send(
            app(state.clone()),
            "GET",
            &format!("/notifications{path}"),
            HeaderMap::new(),
            None,
        )
        .await;
        assert_eq!(
            (status, body),
            (
                StatusCode::UNAUTHORIZED,
                json!({ "error": "Not Authorized", "message": "Invalid URL." })
            ),
            "{path}"
        );
    }

    let (status, ct, page) = get_html(
        app(state.clone()),
        &format!("/notifications{}", old_signed_path(&one)),
    )
    .await;
    assert_eq!((status, ct.as_str()), (StatusCode::OK, "text/html"));
    assert!(
        page.contains("email notifications of type bid_received"),
        "{page}"
    );
    assert!(page.contains("href=\"https://account.decentraland.org\""));
    let sub = state.store.get_subscription(&address).await.unwrap();
    assert_eq!(
        sub.details["message_type"]["bid_received"]["email"],
        json!(false)
    );
    assert_eq!(
        sub.details["message_type"]["bid_accepted"]["email"],
        json!(true)
    );
    assert_eq!(sub.details["ignore_all_email"], json!(false));

    let (status, ct, page) = get_html(
        app(state.clone()),
        &format!("/notifications{}", old_signed_path(&all)),
    )
    .await;
    assert_eq!((status, ct.as_str()), (StatusCode::OK, "text/html"));
    assert!(page.contains("unsubscribed from all email notifications"));
    assert!(!page.contains("{{"), "every placeholder is filled");
    let sub = state.store.get_subscription(&address).await.unwrap();
    assert_eq!(sub.details["ignore_all_email"], json!(true));
    assert_eq!(
        sub.details["message_type"]["bid_received"]["email"],
        json!(false)
    );

    let bad = format!("/unsubscribe/{}/not_a_type", address.as_str());
    let (status, body) = send(
        app(state.clone()),
        "GET",
        &format!("/notifications{}", old_signed_path(&bad)),
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (
            StatusCode::BAD_REQUEST,
            Some("Invalid notification type: not_a_type")
        )
    );
}

#[tokio::test]
async fn unsubscribe_without_a_signing_key_refuses_every_link() {
    let Some(state) = state(Settings::default()).await else {
        return;
    };
    let path = old_signed_path(&format!("/unsubscribe/{}", fresh_address().as_str()));
    let (status, _) = send(
        app(state),
        "GET",
        &format!("/notifications{path}"),
        HeaderMap::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Served as `serve` mounts it (behind the social writes guard): a valid
/// link on a node with social writes off answers the 503 and writes nothing.
#[tokio::test]
async fn unsubscribe_links_are_refused_while_social_writes_are_disabled() {
    let Some(state) = state(Settings {
        signing_key: Some(SIGNING_KEY.into()),
        ..Settings::default()
    })
    .await
    else {
        return;
    };
    let address = fresh_address();
    let guarded = Router::new().nest(
        "/notifications",
        crate::writes::guard(router(state.clone()), false),
    );
    for path in [
        format!("/unsubscribe/{}", address.as_str()),
        format!("/unsubscribe/{}/bid_received", address.as_str()),
    ] {
        let (status, body) = send(
            guarded.clone(),
            "GET",
            &format!("/notifications{}", old_signed_path(&path)),
            HeaderMap::new(),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(body["error"], "Service Unavailable", "{path}");
    }
    assert!(
        state
            .store
            .get_subscription(&address)
            .await
            .unwrap()
            .email
            .is_none(),
        "nothing was written"
    );
}

/// The opt-out is one upsert over the stored document, so a preference
/// written in between (another type, `ignore_all_in_app`) survives it.
#[tokio::test]
async fn unsubscribe_keeps_preferences_written_concurrently() {
    let Some(state) = state(Settings::default()).await else {
        return;
    };
    let address = fresh_address();
    sqlx::query("INSERT INTO social.subscriptions (address, details) VALUES ($1, $2)")
        .bind(&address)
        .bind(json!({"ignore_all_in_app": true, "message_type": {"item_sold": {"email": false, "in_app": false}}}))
        .execute(state.store.pool())
        .await
        .unwrap();
    state
        .store
        .unsubscribe_email(&address, Some("bid_received"))
        .await
        .unwrap();
    state.store.unsubscribe_email(&address, None).await.unwrap();
    let sub = state.store.get_subscription(&address).await.unwrap();
    assert_eq!(sub.details["ignore_all_email"], json!(true));
    assert_eq!(sub.details["ignore_all_in_app"], json!(true));
    assert_eq!(
        sub.details["message_type"]["item_sold"],
        json!({"email": false, "in_app": false})
    );
    assert_eq!(
        sub.details["message_type"]["bid_received"],
        json!({"email": false, "in_app": true})
    );
}

#[tokio::test]
async fn common_email_is_bearer_gated_and_validated() {
    let Some(state) = state(Settings {
        notification_service_token: Some("svc".into()),
        ..Settings::default()
    })
    .await
    else {
        return;
    };
    let full = json!({ "email": "a@example.com", "subject": "s", "content": "c" });
    for (headers, message) in [
        (HeaderMap::new(), "Authorization header is missing"),
        (bearer("nope"), "Invalid authorization header"),
    ] {
        let (status, body) = send(
            app(state.clone()),
            "POST",
            "/notifications/notifications/email",
            headers,
            Some(full.clone()),
        )
        .await;
        assert_eq!(
            (status, body),
            (
                StatusCode::UNAUTHORIZED,
                json!({ "error": "Not Authorized", "message": message })
            )
        );
    }
    // The bearer is checked before the body is parsed.
    let (status, body) = send_raw(
        app(state.clone()),
        "POST",
        "/notifications/notifications/email",
        HeaderMap::new(),
        "{not json",
        Some("application/json"),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (
            StatusCode::UNAUTHORIZED,
            Some("Authorization header is missing")
        )
    );
    let (status, body) = send_raw(
        app(state.clone()),
        "POST",
        "/notifications/notifications/email",
        bearer("svc"),
        "{not json",
        Some("application/json"),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (StatusCode::BAD_REQUEST, Some("Invalid body"))
    );
    for (body, message) in [
        (json!({ "subject": "s", "content": "c" }), "Missing email"),
        (
            json!({ "email": "a@example.com", "subject": "s" }),
            "Missing content",
        ),
        (
            json!({ "email": "a@example.com", "content": "c", "subject": "" }),
            "Missing subject",
        ),
    ] {
        let (status, resp) = send(
            app(state.clone()),
            "POST",
            "/notifications/notifications/email",
            bearer("svc"),
            Some(body),
        )
        .await;
        assert_eq!(
            (status, resp["message"].as_str()),
            (StatusCode::BAD_REQUEST, Some(message))
        );
    }
    let (status, _) = send(
        app(state.clone()),
        "POST",
        "/notifications/notifications/email",
        bearer("svc"),
        Some(full),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unconfigured mailer fails loudly"
    );
}

#[tokio::test]
async fn opaque_and_uuid_opt_out_ids_work_with_custom_and_generic_plans_on_one_connection() {
    let Some(pool) = crate::db::scratch::pool(1).await else {
        return;
    };
    let store = super::NotificationsStore::new(pool.clone());
    let address = fresh_address();
    let scope_id = format!("legacy-{}", Uuid::new_v4());
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    for setting in [
        "SET plan_cache_mode=force_custom_plan",
        "SET plan_cache_mode=force_generic_plan",
    ] {
        sqlx::query(setting).execute(&pool).await.unwrap();
        for _ in 0..8 {
            store
                .create_opt_out(&address, "community", &scope_id)
                .await
                .unwrap();
            assert!(
                store
                    .is_opted_out(&address, "community", &scope_id)
                    .await
                    .unwrap()
            );
            assert!(
                store
                    .delete_opt_out(&address, "community", &scope_id)
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .is_opted_out(&address, "community", &scope_id)
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .delete_opt_out(&address, "community", &scope_id)
                    .await
                    .unwrap()
            );
        }
        let community = Uuid::new_v4();
        let upper = community.to_string().to_uppercase();
        let canonical = community.to_string();
        let other = fresh_address();
        store
            .create_opt_out(&other, "community", &canonical)
            .await
            .unwrap();
        for _ in 0..8 {
            store
                .create_opt_out(&address, "community", &upper)
                .await
                .unwrap();
            assert!(
                store
                    .is_opted_out(&address, "community", &canonical)
                    .await
                    .unwrap()
            );
            assert!(
                store
                    .is_opted_out(&address, "community", &upper)
                    .await
                    .unwrap()
            );
            assert!(
                store
                    .delete_opt_out(&address, "community", &upper)
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .is_opted_out(&address, "community", &canonical)
                    .await
                    .unwrap()
            );
            assert!(
                store
                    .is_opted_out(&other, "community", &canonical)
                    .await
                    .unwrap()
            );
        }
        let current: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            current, pid,
            "all executions use the same PostgreSQL prepared-statement cache"
        );
    }
    sqlx::query("RESET plan_cache_mode")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn direct_community_notices_honor_legacy_uppercase_opt_outs() {
    let Some(pool) = crate::db::scratch::pool(1).await else {
        return;
    };
    let address = fresh_address();
    let community = crate::CommunityId::new();
    sqlx::query("INSERT INTO social.subscription_opt_outs(address,scope,scope_id) VALUES($1,'community',$2)")
        .bind(&address).bind(community.to_string().to_uppercase()).execute(&pool).await.unwrap();
    let event = super::community::CommunityEvent {
        kind: "community_renamed",
        key: Uuid::new_v4().to_string(),
        community_id: community,
        metadata: json!({"communityId":community.to_string()}),
    };
    assert_eq!(
        super::community::publish(&pool, &event, super::community::Audience::One(&address))
            .await
            .unwrap(),
        0
    );
    super::NotificationsStore::new(pool.clone())
        .delete_opt_out(&address, "community", &community.to_string())
        .await
        .unwrap();
    assert_eq!(
        super::community::publish(&pool, &event, super::community::Audience::One(&address))
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn the_unsigned_unsubscribe_capture_replays() {
    let Some(state) = state(Settings {
        signing_key: Some(SIGNING_KEY.into()),
        ..Settings::default()
    })
    .await
    else {
        return;
    };
    let name = "unsubscribe-address-no-signature";
    let fx = crate::test_fixtures::fixture("notifications", name);
    let notes = crate::test_fixtures::replay(&app(state), "/notifications", name, &fx).await;
    assert!(notes.is_empty(), "{notes:#?}");
}

const CAPTURES: &[&str] = &[
    "status",
    "notifications-unauth",
    "subscription-unauth",
    "opt-outs-community-unauth",
    "unknown-path-404",
];

/// Captures replayed with a fresh signature by a fresh wallet (empty inbox,
/// default subscription), as the throwaway wallet of the capture run.
const SIGNED_CAPTURES: &[&str] = &[
    "notifications-signed",
    "notifications-signed-filters",
    "subscription-signed",
    "opt-outs-community-signed",
    "opt-outs-bad-scope-signed",
];

#[tokio::test]
async fn the_notifications_host_answers_as_its_captures() {
    let Some(state) = state(Settings {
        current_version: Some("3.12.2".into()),
        commit_hash: Some("0f113fba0bfc33856b8e184474b99e9c28046370".into()),
        ..Settings::default()
    })
    .await
    else {
        return;
    };
    let app = app(state);
    for name in CAPTURES {
        let mut fx = crate::test_fixtures::fixture("notifications", name);
        fx.volatile_paths.push("currentTime".into());
        let notes = crate::test_fixtures::replay(&app, "/notifications", name, &fx).await;
        assert!(notes.is_empty(), "{name}: {notes:#?}");
    }
    for name in SIGNED_CAPTURES {
        let mut fx = crate::test_fixtures::fixture("notifications", name);
        fx.volatile_paths.push("address".into());
        let wallet = alloy::signers::local::PrivateKeySigner::random();
        let path = format!("/notifications{}", fx.request.path);
        let timestamp = chrono::Utc::now().timestamp_millis().to_string();
        let metadata = r#"{"signer":"dcl:explorer"}"#;
        let auth = signed_headers_by(&wallet, "get", &path, metadata, true, &timestamp).await;
        let notes =
            crate::test_fixtures::replay_signed(&app, "/notifications", name, &fx, &auth).await;
        // friend_first_wear and land_transfer_received are this service's
        // own types; item_discounted postdates the capture.
        let expected: &[&str] = if *name == "subscription-signed" {
            &[
                "details.message_type.friend_first_wear extra",
                "details.message_type.item_discounted extra",
                "details.message_type.land_transfer_received extra",
            ]
        } else {
            &[]
        };
        assert_eq!(notes, expected, "{name}");
    }
}

/// A request with a raw body and an optional Content-Type.
async fn send_raw(
    app: Router,
    method: &str,
    path: &str,
    headers: HeaderMap,
    body: &str,
    content_type: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    for (k, v) in headers.iter() {
        req = req.header(k, v);
    }
    if let Some(content_type) = content_type {
        req = req.header(header::CONTENT_TYPE, content_type);
    }
    let resp = app
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn bodies_are_json_whatever_the_content_type_and_refusals_keep_the_envelope() {
    let Some(state) = state(Settings::default()).await else {
        return;
    };
    let me = signer();
    let id = seed_notification(state.store.pool(), &me, 1_000, false).await;
    let read = "/notifications/notifications/read";
    let ids = format!(r#"{{"notificationIds":["{id}"]}}"#);
    for content_type in [None, Some("text/plain;charset=UTF-8")] {
        let (status, body) = send_raw(
            app(state.clone()),
            "PUT",
            read,
            signed("put", read).await,
            &ids,
            content_type,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{content_type:?}: {body}");
    }
    for (raw, message) in [
        ("{", "Invalid body"),
        ("{}", "Missing notificationIds"),
        (r#"{"notificationIds":[]}"#, "Missing notificationIds"),
        (r#"{"notificationIds":"x"}"#, "Missing notificationIds"),
    ] {
        let (status, body) = send_raw(
            app(state.clone()),
            "PUT",
            read,
            signed("put", read).await,
            raw,
            None,
        )
        .await;
        assert_eq!(
            (status, body),
            (
                StatusCode::BAD_REQUEST,
                json!({ "error": "Bad request", "message": message })
            ),
            "{raw}"
        );
    }

    // A handler on axum's `Json`: the header is supplied, the rejection enveloped.
    let confirm = "/notifications/confirm-email";
    let (status, body) = send_raw(
        app(state.clone()),
        "PUT",
        confirm,
        HeaderMap::new(),
        "{",
        None,
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": "Bad request", "message": "Invalid body" })
        )
    );
    let (status, body) = send_raw(
        app(state.clone()),
        "PUT",
        confirm,
        HeaderMap::new(),
        r#"{"code":"x"}"#,
        Some("text/plain"),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (StatusCode::BAD_REQUEST, Some("Missing address"))
    );

    let (status, body) = send(
        app(state.clone()),
        "POST",
        "/notifications/subscription/opt-outs",
        signed("post", "/notifications/subscription/opt-outs").await,
        None,
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "message": "Invalid JSON body" })
        )
    );
}

#[tokio::test]
async fn only_mark_read_accepts_the_pre_6_signed_payload() {
    let Some(state) = state(Settings::default()).await else {
        return;
    };
    // Uppercase metadata is where the folded legacy payload and 6.x differ.
    let metadata = r#"{"signer":"dcl:navbar","intent":"dcl:navbar:SeeNotifications"}"#;
    let legacy = |method: &'static str, path: &'static str| async move {
        signed_headers(method, path, metadata, false).await
    };
    let read = "/notifications/notifications/read";
    let (status, body) = send(
        app(state.clone()),
        "PUT",
        read,
        legacy("put", read).await,
        Some(json!({ "notificationIds": [Uuid::new_v4().to_string()] })),
    )
    .await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "updated": 0 })));
    for path in [
        "/notifications/notifications",
        "/notifications/subscription",
    ] {
        let headers = legacy("get", path).await;
        let (status, body) = send(app(state.clone()), "GET", path, headers, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: {body}");
    }
}

mod ingest {
    use super::*;
    use crate::notifications::ingest::{
        MAX_BODY_BYTES, MAX_METADATA_BYTES, Producer, dedup, validate,
    };
    use crate::notifications::store::test_support::delivery_state;

    const PATH: &str = "/notifications/notifications";
    /// The clock `validate` is given in unit tests.
    const NOW: i64 = 1_750_000_000_000;

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    async fn producers() -> Option<SharedState> {
        state(Settings {
            governance_token: Some("gov".into()),
            rewards_token: Some("rew".into()),
            ..Settings::default()
        })
        .await
    }

    fn entry(kind: &str, address: &Address, key: &str, ts: i64) -> Value {
        json!({ "type": kind, "address": address.as_str(), "eventKey": key,
                "metadata": { "title": "t", "link": "https://example.org" }, "timestamp": ts })
    }

    fn id_of(kind: &str, address: &Address, key: &str) -> Uuid {
        let body = json!([entry(kind, address, key, 1_700_000_000_000)]);
        validate(&body, Producer::Governance, NOW)
            .or_else(|_| validate(&body, Producer::Rewards, NOW))
            .unwrap()[0]
            .id()
    }

    async fn row(state: &SharedState, id: Uuid) -> Option<(Value, i64, bool)> {
        sqlx::query_as(
            "SELECT metadata, (extract(epoch FROM occurred_at) * 1000)::bigint, read FROM social.notifications WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(state.store.pool())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn each_token_publishes_only_its_own_prefix() {
        let Some(state) = producers().await else {
            return;
        };
        let to = fresh_address();
        for (token, kind, ok) in [
            ("gov", "governance_proposal_enacted", true),
            ("gov", "reward_assignment", false),
            ("rew", "reward_assignment", true),
            ("rew", "governance_proposal_enacted", false),
            ("gov", "governance_not_a_type", false),
            ("rew", "bid_received", false),
        ] {
            let (status, body) = send(
                app(state.clone()),
                "POST",
                PATH,
                bearer(token),
                Some(json!([entry(kind, &to, "k", 1_700_000_000_000)])),
            )
            .await;
            if ok {
                assert_eq!(status, StatusCode::NO_CONTENT, "{token} {kind}: {body}");
                assert!(row(&state, id_of(kind, &to, "k")).await.is_some());
            } else {
                assert_eq!(
                    (status, body),
                    (
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "Bad request", "message": format!("Invalid notification type: {kind}") })
                    ),
                    "{token} {kind}"
                );
            }
        }
        for (headers, message) in [
            (HeaderMap::new(), "Authorization header is missing"),
            (bearer("nope"), "Invalid authorization header"),
            (bearer(""), "Invalid authorization header"),
        ] {
            let (status, body) =
                send(app(state.clone()), "POST", PATH, headers, Some(json!([]))).await;
            assert_eq!(
                (status, body),
                (
                    StatusCode::UNAUTHORIZED,
                    json!({ "error": "Not Authorized", "message": message })
                )
            );
        }
    }

    #[tokio::test]
    async fn an_unset_token_disables_its_producer() {
        let Some(state) = state(Settings {
            governance_token: Some("gov".into()),
            ..Settings::default()
        })
        .await
        else {
            return;
        };
        let (status, _) = send(app(state), "POST", PATH, bearer(""), Some(json!([]))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn validation_answers_in_the_joi_words() {
        let a = "0x00000000000000000000000000000000000000aa";
        let ok = json!({ "type": "governance_announcement", "address": a, "eventKey": "k", "metadata": {}, "timestamp": 1_700_000_000_000i64 });
        let with = |key: &str, value: Value| {
            let mut v = ok.clone();
            v[key] = value;
            json!([v])
        };
        let without = |key: &str| {
            let mut v = ok.clone();
            v.as_object_mut().unwrap().remove(key);
            json!([v])
        };
        for (body, message) in [
            (json!({}), "\"value\" must be an array"),
            (json!([1]), "\"[0]\" must be of type object"),
            (without("type"), "\"[0].type\" is required"),
            (
                with("address", json!("0xZZ")),
                "\"[0].address\" with value \"0xZZ\" fails to match the required pattern: /^0x[a-fA-F0-9]{40}$/",
            ),
            (without("eventKey"), "\"[0].eventKey\" is required"),
            (
                with("eventKey", json!("")),
                "\"[0].eventKey\" is not allowed to be empty",
            ),
            (
                with("metadata", json!([])),
                "\"[0].metadata\" must be of type object",
            ),
            (without("timestamp"), "\"[0].timestamp\" is required"),
            (
                with("timestamp", json!(1_672_531_200_000i64)),
                "\"[0].timestamp\" must be greater than 1672531200000",
            ),
            (
                with("timestamp", json!(1.5e12)),
                "\"[0].timestamp\" must be an integer",
            ),
            (
                with("optOutScope", json!({ "scope": "world", "scopeId": "x" })),
                "Invalid notification scope: world",
            ),
            (with("extra", json!(1)), "\"[0].extra\" is not allowed"),
            (
                with("timestamp", json!(NOW + 5 * 60 * 1000 + 1)),
                "\"[0].timestamp\" must be less than or equal to 1750000300000",
            ),
            (
                with("metadata", json!({ "pad": "x".repeat(MAX_METADATA_BYTES) })),
                "\"[0].metadata\" must be less than or equal to 16384 bytes",
            ),
        ] {
            assert_eq!(
                validate(&body, Producer::Governance, NOW).unwrap_err(),
                message,
                "{body}"
            );
        }
        let parsed = validate(
            &with("address", json!(a.to_uppercase().replace("0X", "0x"))),
            Producer::Governance,
            NOW,
        )
        .unwrap();
        assert_eq!(parsed[0].address.as_ref().unwrap().as_str(), a);
        assert!(
            validate(&without("address"), Producer::Governance, NOW).unwrap()[0]
                .address
                .is_none()
        );
        let at_skew = with("timestamp", json!(NOW + 5 * 60 * 1000));
        assert!(validate(&at_skew, Producer::Governance, NOW).is_ok());
        let mut big = ok.clone();
        big["metadata"] = json!({ "pad": "x".repeat(MAX_METADATA_BYTES - 10) });
        assert!(validate(&json!([big]), Producer::Governance, NOW).is_ok());
        // No item cap: more than the old 10,000 small items pass.
        assert_eq!(
            validate(&json!(vec![ok.clone(); 20_000]), Producer::Governance, NOW)
                .unwrap()
                .len(),
            20_000
        );
    }

    #[test]
    fn only_announcements_may_omit_the_address_and_never_with_an_opt_out_scope() {
        let bare =
            |kind: &str| json!({ "type": kind, "eventKey": "k", "metadata": {}, "timestamp": NOW });
        let mut scoped = bare("governance_announcement");
        scoped["optOutScope"] = json!({ "scope": "community", "scopeId": "c1" });
        for (producer, body, message) in [
            (
                Producer::Governance,
                bare("governance_proposal_enacted"),
                "\"[0].address\" is required",
            ),
            (
                Producer::Rewards,
                bare("reward_assignment"),
                "\"[0].address\" is required",
            ),
            (
                Producer::Governance,
                scoped,
                "\"[0].optOutScope\" is not allowed",
            ),
        ] {
            assert_eq!(
                validate(&json!([body]), producer, NOW).unwrap_err(),
                message
            );
        }
        let ok = validate(
            &json!([bare("governance_announcement")]),
            Producer::Governance,
            NOW,
        )
        .unwrap();
        assert!(ok[0].address.is_none());
    }

    #[test]
    fn ids_are_the_documented_v5_and_a_batch_is_written_in_id_order() {
        let ns: Uuid = "3f8a52d4-9c1e-4b07-a62d-71e05c9318bb".parse().unwrap();
        let a = "0x00000000000000000000000000000000000000aa";
        let body = json!([
            { "type": "governance_announcement", "eventKey": "k", "metadata": {}, "timestamp": NOW },
            { "type": "governance_announcement", "address": a.to_uppercase().replace("0X", "0x"),
              "eventKey": "k", "metadata": {}, "timestamp": NOW },
        ]);
        let parsed = validate(&body, Producer::Governance, NOW).unwrap();
        assert_eq!(
            parsed[0].id(),
            Uuid::new_v5(&ns, br#"["governance_announcement",null,"k"]"#)
        );
        assert_eq!(
            parsed[1].id(),
            Uuid::new_v5(
                &ns,
                format!(r#"["governance_announcement","{a}","k"]"#).as_bytes()
            )
        );

        let to = fresh_address();
        let batch: Vec<Value> = (0..50)
            .map(|i| entry("governance_whale_vote", &to, &format!("k{i}"), NOW))
            .chain([entry("governance_whale_vote", &to, "k3", NOW + 1)])
            .collect();
        let written = dedup(validate(&json!(batch), Producer::Governance, NOW).unwrap());
        let ids: Vec<Uuid> = written.iter().map(|n| n.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!((ids.len(), ids), (50, sorted));
        let k3 = written.iter().find(|n| n.event_key == "k3").unwrap();
        assert_eq!(
            chrono_ms(k3.occurred_at),
            NOW + 1,
            "the last duplicate wins"
        );
    }

    fn chrono_ms(t: chrono::NaiveDateTime) -> i64 {
        t.and_utc().timestamp_millis()
    }

    #[tokio::test]
    async fn a_future_timestamp_is_refused_over_http() {
        let Some(state) = producers().await else {
            return;
        };
        let to = fresh_address();
        let kind = "governance_proposal_enacted";
        let (status, body) = send(
            app(state.clone()),
            "POST",
            PATH,
            bearer("gov"),
            Some(json!([entry(kind, &to, "f", now_ms() + 3_600_000)])),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(row(&state, id_of(kind, &to, "f")).await.is_none());
    }

    #[tokio::test]
    async fn a_repeated_key_updates_the_row_keeps_read_state_and_never_emails_again() {
        let Some(state) = producers().await else {
            return;
        };
        let to = fresh_address();
        sqlx::query(
            "INSERT INTO social.subscriptions (address, email) VALUES ($1, 'r@example.org')",
        )
        .bind(&to)
        .execute(state.store.pool())
        .await
        .unwrap();
        let kind = "governance_proposal_enacted";
        let id = id_of(kind, &to, "p1");
        let first = json!([entry(kind, &to, "p1", 1_700_000_000_000)]);
        for _ in 0..2 {
            let (status, _) = send(
                app(state.clone()),
                "POST",
                PATH,
                bearer("gov"),
                Some(first.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE address = $1")
                .bind(&to)
                .fetch_one(state.store.pool())
                .await
                .unwrap();
        assert_eq!(count, 1, "an idempotent replay");
        assert_eq!(
            delivery_state(state.store.pool(), id).await.unwrap().0,
            "pending"
        );

        state.store.mark_read(&to, &[id]).await.unwrap();
        // The outbox prunes terminal rows; a later update must not re-queue an email.
        sqlx::query("DELETE FROM social.notification_deliveries WHERE notification_id = $1")
            .bind(id)
            .execute(state.store.pool())
            .await
            .unwrap();
        let mut updated = entry(kind, &to, "p1", 1_700_000_500_000);
        updated["metadata"] = json!({ "title": "new" });
        let (status, _) = send(
            app(state.clone()),
            "POST",
            PATH,
            bearer("gov"),
            Some(json!([updated])),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            row(&state, id).await,
            Some((json!({ "title": "new" }), 1_700_000_500_000, true))
        );
        assert_eq!(delivery_state(state.store.pool(), id).await, None);
    }

    #[tokio::test]
    async fn a_failing_chunk_leaves_the_earlier_chunks_committed() {
        use crate::notifications::ingest::upsert;
        let Some(pool) = crate::notifications::store::test_support::private_pool("ing-chunk").await
        else {
            return;
        };
        let to = fresh_address();
        let kind = "governance_proposal_enacted";
        let body: Vec<Value> = (0..1_500)
            .map(|i| entry(kind, &to, &format!("c{i}"), 1_700_000_000_000))
            .collect();
        let mut batch = validate(&json!(body), Producer::Governance, NOW).unwrap();
        // Chunks follow id order, so the largest id is in the second chunk.
        let last = batch.iter_mut().max_by_key(|n| n.id()).unwrap();
        last.metadata = json!({ "title": "boom" });
        sqlx::query("ALTER TABLE social.notifications ADD CONSTRAINT ingest_chunk_test CHECK (metadata->>'title' IS DISTINCT FROM 'boom')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(upsert(&pool, batch).await.is_err());
        let (rows, deliveries): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(d.notification_id) FROM social.notifications n LEFT JOIN social.notification_deliveries d ON d.notification_id = n.id WHERE n.address = $1",
        )
        .bind(&to)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (rows, deliveries),
            (1_000, 1_000),
            "the first chunk committed alone"
        );
    }

    #[tokio::test]
    async fn upsert_counts_new_and_updated_notifications() {
        use crate::notifications::ingest::{Upserted, upsert};
        let Some(state) = producers().await else {
            return;
        };
        let (a, b) = (fresh_address(), fresh_address());
        let kind = "governance_proposal_enacted";
        let key = format!("count-{}", Uuid::new_v4());
        let batch = |ts: i64| {
            validate(
                &json!([entry(kind, &a, &key, ts), entry(kind, &b, &key, ts)]),
                Producer::Governance,
                NOW,
            )
            .unwrap()
        };
        let pool = state.store.pool();
        let first = upsert(pool, batch(1_700_000_000_000)).await.unwrap();
        assert_eq!(
            first,
            Upserted {
                inserted: 2,
                updated: 0
            }
        );
        let again = upsert(pool, batch(1_700_000_100_000)).await.unwrap();
        assert_eq!(
            again,
            Upserted {
                inserted: 0,
                updated: 2
            }
        );
    }

    #[tokio::test]
    async fn duplicates_in_one_batch_keep_the_last_and_opt_outs_drop_the_row() {
        let Some(state) = producers().await else {
            return;
        };
        let to = fresh_address();
        let kind = "reward_assignment";
        let mut last = entry(kind, &to, "r1", 1_700_000_100_000);
        last["metadata"] = json!({ "n": 2 });
        let (status, _) = send(
            app(state.clone()),
            "POST",
            PATH,
            bearer("rew"),
            Some(json!([entry(kind, &to, "r1", 1_700_000_000_000), last])),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            row(&state, id_of(kind, &to, "r1")).await.unwrap().0,
            json!({ "n": 2 })
        );

        state
            .store
            .create_opt_out(&to, "community", "c1")
            .await
            .unwrap();
        let mut scoped = entry(kind, &to, "r2", 1_700_000_000_000);
        scoped["optOutScope"] = json!({ "scope": "community", "scopeId": "c1" });
        let (status, _) = send(
            app(state.clone()),
            "POST",
            PATH,
            bearer("rew"),
            Some(json!([scoped])),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(row(&state, id_of(kind, &to, "r2")).await.is_none());
    }

    #[tokio::test]
    async fn a_community_opt_out_drops_the_row_however_the_producer_spells_the_uuid() {
        let Some(state) = producers().await else {
            return;
        };
        let to = fresh_address();
        let kind = "reward_assignment";
        let community = Uuid::new_v4();
        // Stored in the canonical spelling, whatever the wallet sent.
        state
            .store
            .create_opt_out(&to, "community", &community.simple().to_string())
            .await
            .unwrap();
        let other = Uuid::new_v4().to_string();
        let cases = [
            ("canonical", community.to_string(), false),
            ("upper", community.to_string().to_uppercase(), false),
            ("simple", community.simple().to_string(), false),
            ("braced", community.braced().to_string(), false),
            ("other", other, true),
        ];
        for (key, scope_id, kept) in cases {
            let mut scoped = entry(kind, &to, key, 1_700_000_000_000);
            scoped["optOutScope"] = json!({ "scope": "community", "scopeId": scope_id });
            let (status, _) = send(
                app(state.clone()),
                "POST",
                PATH,
                bearer("rew"),
                Some(json!([scoped])),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(
                row(&state, id_of(kind, &to, key)).await.is_some(),
                kept,
                "{key}: {scope_id}"
            );
        }
    }

    #[tokio::test]
    async fn a_legacy_community_opt_out_in_another_spelling_drops_the_row() {
        let Some(state) = producers().await else {
            return;
        };
        let kind = "reward_assignment";
        for key in ["upper", "simple"] {
            let to = fresh_address();
            let community = Uuid::new_v4();
            let legacy = match key {
                "upper" => community.to_string().to_uppercase(),
                _ => community.simple().to_string(),
            };
            // Rows written before `create_opt_out` canonicalised the id.
            sqlx::query(
                "INSERT INTO social.subscription_opt_outs (address, scope, scope_id, created_at) \
                 VALUES ($1, 'community', $2, now())",
            )
            .bind(&to)
            .bind(&legacy)
            .execute(state.store.pool())
            .await
            .unwrap();
            let mut scoped = entry(kind, &to, key, 1_700_000_000_000);
            scoped["optOutScope"] =
                json!({ "scope": "community", "scopeId": community.to_string() });
            let (status, _) = send(
                app(state.clone()),
                "POST",
                PATH,
                bearer("rew"),
                Some(json!([scoped])),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert!(row(&state, id_of(kind, &to, key)).await.is_none(), "{key}");

            // The settings API reads and removes the same legacy row.
            let canonical = community.to_string();
            assert!(
                state
                    .store
                    .is_opted_out(&to, "community", &canonical)
                    .await
                    .unwrap(),
                "{key}"
            );
            assert!(
                state
                    .store
                    .delete_opt_out(&to, "community", &canonical)
                    .await
                    .unwrap(),
                "{key}"
            );
            assert!(
                !state
                    .store
                    .is_opted_out(&to, "community", &canonical)
                    .await
                    .unwrap(),
                "{key}"
            );
        }
    }

    /// Broadcasts show in every inbox, so these tests use a private database.
    async fn private_state(tag: &str) -> Option<SharedState> {
        let pool = crate::notifications::store::test_support::private_pool(tag).await?;
        Some(Arc::new(State::new(
            pool,
            Settings {
                governance_token: Some("gov".into()),
                ..Settings::default()
            },
            crate::http::Outbound::for_tests(),
            None,
            None,
        )))
    }

    fn announcement(key: &str, ts: i64, title: &str) -> Value {
        json!({ "type": "governance_announcement", "eventKey": key,
                "metadata": { "title": title }, "timestamp": ts })
    }

    async fn post(state: &SharedState, body: Value) {
        let (status, resp) =
            send(app(state.clone()), "POST", PATH, bearer("gov"), Some(body)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{resp}");
    }

    #[tokio::test]
    async fn a_broadcast_reaches_every_wallet_with_its_own_read_state_and_no_email() {
        let Some(state) = private_state("ingest_bc").await else {
            return;
        };
        let pool = state.store.pool().clone();
        let (stranger, subscriber) = (fresh_address(), fresh_address());
        sqlx::query(
            "INSERT INTO social.subscriptions (address, email) VALUES ($1, 's@example.org')",
        )
        .bind(&subscriber)
        .execute(&pool)
        .await
        .unwrap();
        let t0 = now_ms() - 3_600_000;
        for _ in 0..2 {
            post(&state, json!([announcement("ann-1", t0, "t")])).await;
        }
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM social.broadcast_notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
        let fanned: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM social.notifications) + (SELECT count(*) FROM social.notification_deliveries)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (rows, fanned),
            (1, 0),
            "stored once, no per-wallet rows, no email"
        );

        // A wallet with no subscription row sees it, on the old wire (`address: null`).
        let (status, body) = send(
            app(state.clone()),
            "GET",
            "/notifications/notifications",
            signed("get", "/notifications/notifications").await,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let item = &body["notifications"][0];
        assert_eq!(
            (
                &item["type"],
                &item["address"],
                &item["read"],
                &item["timestamp"]
            ),
            (
                &json!("governance_announcement"),
                &Value::Null,
                &json!(false),
                &json!(t0.to_string())
            )
        );
        let id: Uuid = item["id"].as_str().unwrap().parse().unwrap();

        assert_eq!(state.store.mark_read(&stranger, &[id]).await.unwrap(), 1);
        assert_eq!(state.store.mark_read(&stranger, &[id]).await.unwrap(), 0);
        let read = |who: Address| {
            let store = state.store.clone();
            async move { store.list(&who, 10, None, false).await.unwrap()[0].read }
        };
        assert!(read(stranger.clone()).await);
        assert!(!read(subscriber.clone()).await, "reads are per wallet");

        // A repeat updates the broadcast and keeps the reads.
        post(&state, json!([announcement("ann-1", t0 + 500_000, "new")])).await;
        let listed = state.store.list(&stranger, 10, None, false).await.unwrap();
        assert_eq!(
            (
                listed.len(),
                listed[0].id,
                listed[0].read,
                listed[0].timestamp
            ),
            (1, id, true, t0 + 500_000)
        );
        assert_eq!(listed[0].metadata, json!({ "title": "new" }));
        assert!(
            state
                .store
                .list(&stranger, 10, None, true)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_inbox_merges_addressed_rows_and_broadcasts_by_time() {
        let Some(state) = private_state("ingest_mix").await else {
            return;
        };
        let me = fresh_address();
        let t = now_ms() - 3_600_000;
        let kind = "governance_proposal_enacted";
        post(
            &state,
            json!([
                entry(kind, &me, "a1", t + 1),
                announcement("b2", t + 2, "b2"),
                entry(kind, &me, "a3", t + 3),
                announcement("b4", t + 4, "b4"),
            ]),
        )
        .await;
        let stamps = |items: Vec<super::super::types::NotificationItem>| {
            items.iter().map(|i| i.timestamp - t).collect::<Vec<_>>()
        };
        let store = &state.store;
        assert_eq!(
            stamps(store.list(&me, 3, None, false).await.unwrap()),
            vec![4, 3, 2]
        );
        assert_eq!(
            stamps(store.list(&me, 10, Some(t + 2), false).await.unwrap()),
            vec![4, 3, 2]
        );
        let newest = store.list(&me, 1, None, false).await.unwrap()[0].id;
        let own = store.list(&me, 2, None, false).await.unwrap()[1].id;
        assert_eq!(store.mark_read(&me, &[newest, own]).await.unwrap(), 2);
        assert_eq!(
            stamps(store.list(&me, 10, None, true).await.unwrap()),
            vec![2, 1]
        );
        assert_eq!(
            stamps(store.list(&me, 1, Some(t + 2), true).await.unwrap()),
            vec![2]
        );
        // Another wallet sees only the broadcasts, both unread.
        assert_eq!(
            stamps(store.list(&fresh_address(), 10, None, true).await.unwrap()),
            vec![4, 2]
        );
    }

    #[tokio::test]
    async fn broadcasts_are_kept_by_default_and_past_a_set_horizon_hidden_then_pruned() {
        let Some(state) = private_state("ingest_horizon").await else {
            return;
        };
        let pool = state.store.pool().clone();
        let me = fresh_address();
        let day = 86_400_000i64;
        let now = now_ms();
        post(
            &state,
            json!([
                announcement("old", now - 91 * day, "old"),
                announcement("new", now - 89 * day, "new"),
            ]),
        )
        .await;
        let titles = |items: Vec<super::super::types::NotificationItem>| {
            items
                .iter()
                .map(|i| i.metadata["title"].clone())
                .collect::<Vec<_>>()
        };
        // The default keeps every broadcast, as upstream.
        assert_eq!(
            titles(state.store.list(&me, 10, None, false).await.unwrap()),
            vec![json!("new"), json!("old")]
        );
        assert_eq!(
            state
                .store
                .prune_broadcasts(crate::shared::now_utc(), 100)
                .await
                .unwrap(),
            0
        );
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM social.broadcast_notifications")
            .fetch_all(&pool)
            .await
            .unwrap();
        state.store.mark_read(&me, &ids).await.unwrap();

        let ninety = crate::notifications::store::NotificationsStore::new(pool.clone())
            .with_broadcast_horizon(Some(std::time::Duration::from_secs(90 * 86_400)));
        assert_eq!(
            titles(ninety.list(&me, 10, None, false).await.unwrap()),
            vec![json!("new")]
        );
        let pruned = ninety
            .prune_broadcasts(crate::shared::now_utc(), 100)
            .await
            .unwrap();
        let left: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM social.broadcast_notifications), (SELECT count(*) FROM social.broadcast_reads)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((pruned, left), (1, (1, 1)));

        // A shorter horizon hides and prunes the rest.
        let short = crate::notifications::store::NotificationsStore::new(pool.clone())
            .with_broadcast_horizon(Some(std::time::Duration::from_secs(30 * 86_400)));
        assert!(short.list(&me, 10, None, false).await.unwrap().is_empty());
        assert_eq!(
            short
                .prune_broadcasts(crate::shared::now_utc(), 100)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn writes_off_answers_503_and_writes_nothing() {
        let Some(state) = producers().await else {
            return;
        };
        let to = fresh_address();
        let gated = crate::writes::guard(app(state.clone()), false);
        let (status, _) = send(
            gated,
            "POST",
            PATH,
            bearer("gov"),
            Some(json!([entry(
                "governance_announcement",
                &to,
                "w",
                1_700_000_000_000
            )])),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            row(&state, id_of("governance_announcement", &to, "w"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_body_limit_admits_large_batches_and_refuses_beyond_it() {
        let Some(state) = producers().await else {
            return;
        };
        let to = fresh_address();
        // ~1 MiB: four times the router's default limit.
        let batch: Vec<Value> = (0..2_000)
            .map(|i| {
                let mut e = entry(
                    "governance_voting_ended_voter",
                    &to,
                    &format!("v{i}"),
                    1_700_000_000_000,
                );
                e["metadata"]["pad"] = json!("x".repeat(400));
                e
            })
            .collect();
        let (status, body) = send(
            app(state.clone()),
            "POST",
            PATH,
            bearer("gov"),
            Some(json!(batch)),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        let written: i64 =
            sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE address = $1")
                .bind(&to)
                .fetch_one(state.store.pool())
                .await
                .unwrap();
        assert_eq!(written, 2_000, "every chunk of the batch is written");
        let delivered: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.notification_deliveries d JOIN social.notifications n ON n.id = d.notification_id WHERE n.address = $1",
        )
        .bind(&to)
        .fetch_one(state.store.pool())
        .await
        .unwrap();
        assert_eq!(
            delivered, 2_000,
            "each chunk commits its rows' delivery rows"
        );
        let req = Request::builder()
            .method("POST")
            .uri(PATH)
            .header(header::AUTHORIZATION, "Bearer gov")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(vec![b' '; MAX_BODY_BYTES + 1]))
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
