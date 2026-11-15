//! Executed source: source-contracts/community-voice.json (social b6b2cef,
//! companion gatekeeper be6a4b9, Node 26). The companion pin describes this
//! composition; it does not establish every deployed gatekeeper's behavior.
use super::*;
use crate::comms::gatekeeper::voice::db::VoiceDbConfig;

struct ConfiguredVoice(VoiceDbConfig);

impl super::super::VoiceEviction for ConfiguredVoice {
    fn presence_config(&self) -> VoiceDbConfig {
        self.0
    }
    fn evict<'a>(
        &'a self,
        _: CommunityId,
        _: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn configured_comms_presence_drives_detail_lists_requests_and_active_handler() {
    let _voice = VOICE_ROWS.lock().await;
    let Some(rig) = Rig::new(Settings::for_tests()).await else {
        return;
    };
    rig.state
        .set_voice_eviction(Arc::new(ConfiguredVoice(VoiceDbConfig {
            connection_interrupted_ttl_ms: 60_000,
            initial_connection_ttl_ms: 90_000,
            ..Default::default()
        })));
    let (owner, viewer) = (wallet(), wallet());
    let live = rig.community(&owner, false).await;
    let stale = rig.community(&owner, false).await;
    let search = live.as_uuid().simple().to_string()[..12].to_owned();
    for (id, suffix) in [(live, "live"), (stale, "stale")] {
        sqlx::query("UPDATE social.communities SET name=$2 WHERE id=$1")
            .bind(id)
            .bind(format!("Voice {search} {suffix}"))
            .execute(&rig.pool)
            .await
            .unwrap();
    }
    rig.member(live, &viewer, "member").await;
    rig.request(live, &viewer, "invite").await;
    for (id, status, moderator, old) in [
        (live, "connection_interrupted", true, false),
        (live, "not_connected", false, false),
        (live, "connected", false, true),
        (live, "connection_interrupted", false, true),
        (live, "not_connected", false, true),
        (stale, "connection_interrupted", true, true),
        (stale, "connected", false, false),
    ] {
        let participant = wallet();
        rig.voice(id, &participant, status, moderator).await;
        sqlx::query("UPDATE social.community_voice_chat_users SET joined_at=timezone('UTC',now())-$3::text::interval,status_updated_at=timezone('UTC',now())-$3::text::interval WHERE community_id=$1 AND address=$2")
            .bind(id).bind(&participant.address).bind(if old {"120 seconds"} else {"1 second"}).execute(&rig.pool).await.unwrap();
    }
    let expected = json!({"isActive":true,"participantCount":3,"moderatorCount":1});
    for version in ["v1", "v2"] {
        let path = format!("/{version}/communities/{live}");
        let (status, body) = hit(&rig.app, Case::new("get", path).signed(&viewer).await).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["voiceChatStatus"], expected);
        for suffix in ["", "&onlyWithActiveVoiceChat=true"] {
            let (status, body) = hit(
                &rig.app,
                Case::new(
                    "get",
                    format!("/{version}/communities?search={search}{suffix}"),
                )
                .signed(&viewer)
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let rows = body["data"]["results"].as_array().unwrap();
            assert_eq!(
                rows.iter().find(|r| r["id"] == live.to_string()).unwrap()["voiceChatStatus"],
                expected
            );
            if !suffix.is_empty() {
                assert!(!rows.iter().any(|r| r["id"] == stale.to_string()));
            } else {
                assert_eq!(
                    rows.iter().find(|r| r["id"] == stale.to_string()).unwrap()["voiceChatStatus"],
                    json!({"isActive":false,"participantCount":1,"moderatorCount":0})
                );
            }
        }
        let (status, body) = hit(
            &rig.app,
            Case::new(
                "get",
                format!("/{version}/members/{}/requests?type=invite", viewer.address),
            )
            .signed(&viewer)
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["results"][0]["voiceChatStatus"], expected);
    }
    let (_, body) = hit(
        &rig.app,
        Case::new("get", format!("/v2/communities/{stale}")),
    )
    .await;
    assert_eq!(
        body["data"]["voiceChatStatus"],
        json!({"isActive":false,"participantCount":0,"moderatorCount":0})
    );
    let (status, body) = hit(
        &rig.app,
        Case::new("get", "/v1/community-voice-chats/active")
            .signed(&viewer)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let chats = body["data"]["activeChats"].as_array().unwrap();
    let chat = chats
        .iter()
        .find(|r| r["communityId"] == live.to_string())
        .unwrap();
    assert_eq!(chat["participantCount"], 3);
    assert_eq!(chat["moderatorCount"], 1);
    assert!(chat.get("communityImage").is_none());
    assert!(!chats.iter().any(|r| r["communityId"] == stale.to_string()));
    sqlx::query("DELETE FROM social.community_voice_chat_users WHERE community_id=ANY($1)")
        .bind(vec![live.as_uuid(), stale.as_uuid()])
        .execute(&rig.pool)
        .await
        .unwrap();
    rig.finish();
}

/// A separate real-schema database lets permission faults exercise handlers
/// without altering another test's tables or assuming the process has no rooms.
#[tokio::test]
async fn active_handler_preserves_empty_error_and_all_settled_source_contracts() {
    let Some(database) =
        crate::db::scratch::OwnedDatabase::new("voice", crate::db::scratch::Schema::Full, 3).await
    else {
        return;
    };
    let pool = database.pool.clone();
    let options: sqlx::postgres::PgConnectOptions = database.url.parse().unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let role = format!("voice_{suffix}");
    // The CI database uses SCRAM authentication; local trust must not hide
    // a fixture role without a usable credential. Generated for this test only.
    let password = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {role} LOGIN PASSWORD '{password}'"
    )))
    .execute(&pool)
    .await
    .unwrap();
    for statement in [
        format!("GRANT USAGE ON SCHEMA social TO {role}"),
        format!("GRANT SELECT ON social.community_voice_chat_users TO {role}"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&pool)
            .await
            .unwrap();
    }
    let limited = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.username(&role).password(&password))
        .await
        .unwrap();
    let storage_dir = std::env::temp_dir().join(format!("catalyst-voice-{suffix}"));
    let storage = Arc::new(
        crate::storage::Storage::from_config(&storage_dir)
            .await
            .unwrap(),
    );
    let state = Arc::new(State::new(
        limited.clone(),
        storage,
        Settings::for_tests(),
        None,
        &crate::http::Outbound::for_tests(),
        State::explore_for_tests(&limited),
    ));
    let app = router(state);
    let viewer = wallet();
    let request = || Case::new("get", "/v1/community-voice-chats/active");
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/services/catalyst/tests/fixtures/conformance/social/source-contracts/community-voice.json"
    )))
    .unwrap();
    let source = |name: &str| {
        fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .unwrap()["result"]
            .clone()
    };
    let (status, body) = hit(&app, request().signed(&viewer).await).await;
    assert_eq!(
        status.as_u16(),
        source("empty")["status"].as_u64().unwrap() as u16
    );
    assert_eq!(body, source("empty")["body"]);
    let member = CommunityId::parse("00000000-0000-4000-8000-000000000001").unwrap();
    let public = CommunityId::parse("00000000-0000-4000-8000-000000000002").unwrap();
    for (id, name, is_member) in [
        (member, "Member community", true),
        (public, "Public community", false),
    ] {
        sqlx::query("INSERT INTO social.communities(id,name,description,owner_address,private) VALUES($1,$2,'fixture',$3,false)")
            .bind(id).bind(name).bind(&viewer.address).execute(&pool).await.unwrap();
        if is_member {
            sqlx::query("INSERT INTO social.community_members(community_id,member_address,role) VALUES($1,$2,'member')").bind(id).bind(&viewer.address).execute(&pool).await.unwrap();
        }
        for i in 0..if is_member { 2 } else { 3 } {
            sqlx::query("INSERT INTO social.community_voice_chat_users(community_id,address,is_moderator,status) VALUES($1,$2,$3,'connected')").bind(id).bind(wallet().address).bind(i==0).execute(&pool).await.unwrap();
        }
    }
    let (status, body) = hit(&app, request().signed(&viewer).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        json!({"message":"permission denied for table communities"})
    );
    for version in ["v1", "v2"] {
        let (status, body) = hit(
            &app,
            Case::new("get", format!("/{version}/communities/{member}")),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            json!({"message":"permission denied for table communities"})
        );
    }
    for table in ["communities", "community_members", "community_bans"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT SELECT ON social.{table} TO {role}"
        )))
        .execute(&pool)
        .await
        .unwrap();
    }
    // Both enrichment tables are denied. allSettled retains the member;
    // the nonmember has no destinations and is omitted.
    let (status, body) = hit(&app, request().signed(&viewer).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, source("nonmember-places-error")["body"]);
    // A stored thumbnail whose blob is absent is also an omitted key.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "GRANT SELECT ON social.community_thumbnails TO {role}"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO social.community_thumbnails(community_id,thumbnail_hash) VALUES($1,$2)",
    )
    .bind(member)
    .bind("a".repeat(64))
    .execute(&pool)
    .await
    .unwrap();
    let (status, body) = hit(&app, request().signed(&viewer).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, source("nonmember-places-error")["body"]);
    limited.close().await;
    let (status, body) = hit(&app, request().signed(&viewer).await).await;
    // A local voice SQL failure follows the companion's catch-to-empty
    // contract. A direct HTTP500 from that provider is a separate source case.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, source("empty")["body"]);
    database.drop().await;
    // The role outlives its database; drop it from the shared scratch one.
    let shared = crate::db::scratch::pool(1).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
        .execute(&shared)
        .await
        .unwrap();
    shared.close().await;
    let _ = std::fs::remove_dir_all(storage_dir);
}

#[tokio::test]
async fn voice_sql_failures_degrade_separately_from_core_community_failures() {
    let _permissions = DATABASE_PERMISSIONS.lock().await;
    let _voice = VOICE_ROWS.lock().await;
    let Some(rig) = Rig::new(Settings::for_tests()).await else {
        return;
    };
    let (owner, viewer) = (wallet(), wallet());
    let id = rig.community(&owner, false).await;
    let search = id.as_uuid().simple().to_string();
    sqlx::query("UPDATE social.communities SET name=$2 WHERE id=$1")
        .bind(id)
        .bind(&search)
        .execute(&rig.pool)
        .await
        .unwrap();
    rig.member(id, &viewer, "member").await;
    rig.request(id, &viewer, "request_to_join").await;
    rig.voice(id, &owner, "connected", true).await;
    rig.voice(id, &viewer, "connected", false).await;
    let (status, body) = hit(
        &rig.app,
        Case::new("get", format!("/v2/communities/{id}"))
            .signed(&viewer)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["voiceChatStatus"],
        json!({"isActive":true,"participantCount":2,"moderatorCount":1})
    );
    let role = format!("voice_errors_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE ROLE {role}")))
        .execute(&rig.pool)
        .await
        .unwrap();
    let schemas: Vec<String> = sqlx::query_scalar("SELECT nspname FROM pg_namespace WHERE nspname NOT LIKE 'pg_%' AND nspname<>'information_schema'").fetch_all(&rig.pool).await.unwrap();
    for schema in schemas {
        assert!(
            schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("GRANT USAGE ON SCHEMA {schema} TO {role}; GRANT SELECT,INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA {schema} TO {role}; GRANT USAGE,SELECT ON ALL SEQUENCES IN SCHEMA {schema} TO {role}"))).execute(&rig.pool).await.unwrap();
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "REVOKE SELECT ON social.community_voice_chat_users FROM {role}"
    )))
    .execute(&rig.pool)
    .await
    .unwrap();
    let connect_role = role.clone();
    // Original CI credentials authenticate the connection; the non-LOGIN role
    // supplies only the scoped permission fault, without relying on local trust.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |connection, _| {
            let role = connect_role.clone();
            Box::pin(async move {
                sqlx::query(sqlx::AssertSqlSafe(format!("SET ROLE {role}")))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&crate::db::scratch::url().await.unwrap())
        .await
        .unwrap();
    let state = Arc::new(State::new(
        pool.clone(),
        rig.state.storage.clone(),
        Settings::for_tests(),
        None,
        &crate::http::Outbound::for_tests(),
        rig.state.explore.clone(),
    ));
    state.pass_markers.insert(None, None);
    let app = Router::new().nest("/social-api", crate::social_api_gateway(router(state)));
    let fixture: Value=serde_json::from_str(include_str!(concat!(env!("DCL_ONE_ROOT"),"/services/catalyst/tests/fixtures/conformance/social/source-contracts/community-voice-errors.json"))).unwrap();
    let mut compared = 0;
    for phase in ["companion-sql-error", "community-sql-error"] {
        if phase == "community-sql-error" {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("GRANT SELECT ON social.community_voice_chat_users TO {role}; REVOKE SELECT ON social.communities FROM {role}"))).execute(&rig.pool).await.unwrap();
        }
        for case in fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| case["phase"] == phase)
        {
            compared += 1;
            let source_path = case["path"].as_str().unwrap();
            let path = source_path
                .replace(":id", &id.to_string())
                .replace(":address", viewer.address.as_str());
            let separator = if path.contains('?') { '&' } else { '?' };
            let path = format!("/social-api{path}{separator}search={search}");
            let (status, body) = hit(&app, Case::new("get", path).signed(&viewer).await).await;
            assert_eq!(
                status.as_u16(),
                case["response"]["status"].as_u64().unwrap() as u16,
                "{phase} {source_path}: {body}"
            );
            if status == StatusCode::INTERNAL_SERVER_ERROR
                || source_path.contains("onlyWithActiveVoiceChat")
                || source_path.ends_with("voice-chats/active")
            {
                assert_eq!(body, case["response"]["body"], "{phase} {source_path}");
            } else {
                let (row, source_row) = if source_path.ends_with("/:id") {
                    (&body["data"], &case["response"]["body"]["data"])
                } else {
                    (
                        &body["data"]["results"][0],
                        &case["response"]["body"]["data"]["results"][0],
                    )
                };
                assert!(
                    row.is_object(),
                    "healthy core SQL must retain the row: {body}"
                );
                assert_eq!(
                    row["voiceChatStatus"], source_row["voiceChatStatus"],
                    "{phase} {source_path}"
                );
            }
        }
    }
    assert_eq!(compared, 18);
    // With both tables denied, all-active still returns empty before core SQL.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "REVOKE SELECT ON social.community_voice_chat_users FROM {role}"
    )))
    .execute(&rig.pool)
    .await
    .unwrap();
    for source_path in [
        "/v1/community-voice-chats/active",
        "/v1/communities?onlyWithActiveVoiceChat=true",
        "/v2/communities?onlyWithActiveVoiceChat=true",
    ] {
        let (status, body) = hit(
            &app,
            Case::new("get", format!("/social-api{source_path}"))
                .signed(&viewer)
                .await,
        )
        .await;
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["phase"] == "companion-sql-error" && case["path"] == source_path)
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{source_path}: {body}");
        assert_eq!(body, case["response"]["body"], "{source_path}");
    }
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP OWNED BY {role}; DROP ROLE {role}"
    )))
    .execute(&rig.pool)
    .await
    .unwrap();
    rig.finish();
}
