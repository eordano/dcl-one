//! Each handler catches a real SQL server exception at its own HTTP boundary.
//! The five reference envelopes were executed from the pinned source handlers.
use super::*;

#[tokio::test]
async fn referral_and_privacy_errors_match_executed_source_handler_boundaries() {
    let Some(database) =
        crate::db::scratch::OwnedDatabase::new("errors", crate::db::scratch::Schema::Blank, 2)
            .await
    else {
        return;
    };
    let pool = database.pool.clone();
    // An owned database avoids changing shared relations or cache facts. The
    // views raise the same server error for actual SQLx route dependency reads.
    sqlx::raw_sql("CREATE SCHEMA social;
        CREATE FUNCTION social.handler_failure() RETURNS text LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'source dependency failure'; END $$;
        CREATE VIEW social.referral_progress AS SELECT social.handler_failure() AS referrer,
          social.handler_failure() AS invited_user, ''::text AS invited_user_ip,
          ''::text AS status, true AS tier_granted, now() AS created_at;
        CREATE VIEW social.user_settings AS SELECT social.handler_failure() AS address,
          'all'::text AS private_messages_privacy, 'show_messages'::text AS blocked_users_messages_visibility,
          'show'::text AS show_situation_reactions;")
        .execute(&pool).await.unwrap();
    let storage_dir =
        std::env::temp_dir().join(format!("catalyst-handler-errors-{}", Uuid::new_v4()));
    let storage = Arc::new(
        crate::storage::Storage::from_config(&storage_dir)
            .await
            .unwrap(),
    );
    let state = Arc::new(State::new(
        pool.clone(),
        storage,
        Settings::for_tests(),
        None,
        &crate::http::Outbound::for_tests(),
        State::explore_for_tests(&pool),
    ));
    let api = router(state);
    let rpc = crate::rpc::router(Arc::new(crate::rpc::State::new(
        pool.clone(),
        Default::default(),
        None,
        Arc::new(crate::comms::gatekeeper::voice::rooms::NoVoiceRuntime),
    )));
    let fixture: Value = serde_json::from_str(include_str!(concat!(env!("DCL_ONE_ROOT"),
        "/services/catalyst/tests/fixtures/conformance/social/source-contracts/referral-privacy-errors.json"))).unwrap();
    let (invited, referrer) = (wallet(), wallet());
    for case in fixture["cases"].as_array().unwrap() {
        let method = match case["method"].as_str().unwrap() {
            "GET" => "GET",
            "POST" => "POST",
            "PATCH" => "PATCH",
            other => panic!("unknown source method {other}"),
        };
        let path = case["path"].as_str().unwrap();
        let privacy = path.contains("privacy-settings");
        let request = if privacy {
            Case::new(
                "get",
                format!("/social-rpc/v1/users/{}/privacy-settings", invited.address),
            )
        } else {
            let mut request = Case::new(method, path).signed(&invited).await;
            if method == "POST" {
                // Each source route's schema disallows additional properties.
                request = if path.ends_with("referral-email") {
                    request.json(&json!({"email":"person@example.org"}))
                } else {
                    request.json(&json!({"referrer":referrer.address.as_str()}))
                };
            }
            request.peer("192.0.2.1")
        };
        let (status, body) = hit(if privacy { &rpc } else { &api }, request).await;
        assert_eq!(
            status.as_u16(),
            case["response"]["status"].as_u64().unwrap() as u16,
            "{method} {path}: {body}"
        );
        assert_eq!(body, case["response"]["body"], "{method} {path}");
    }
    database.drop().await;
    let _ = std::fs::remove_dir_all(storage_dir);
}
