//! Source profile metadata replayed through RPC mutations and the native inbox/outbox.
use super::*;
use serde_json::Value;

#[tokio::test]
async fn friendship_native_inbox_metadata_and_delivery_failures_preserve_committed_actions() {
    let Some(database) =
        super::fanout::OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let pool = database.pool.clone();
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/services/catalyst/tests/fixtures/conformance/social/notification-cases.json"
    )))
    .unwrap();
    let sender = fixture["context"]["senderAddress"].as_str().unwrap();
    let receiver = fixture["context"]["receiverAddress"].as_str().unwrap();
    let db = Db::new(pool.clone());
    let mut deployed = Vec::new();
    for (address, key) in [(sender, "senderProfile"), (receiver, "receiverProfile")] {
        let entity = format!("bafkreinativefixture{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO content.deployments(deployer_address,version,entity_type,entity_id,entity_metadata,entity_timestamp,entity_pointers,local_timestamp,auth_chain) VALUES($1,'v3','profile',$2,$3,now(),ARRAY[$1],now(),'[]'::json)")
            .bind(address).bind(&entity).bind(serde_json::json!({"v":fixture["context"][key]})).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO content.active_pointers(pointer,entity_id,entity_type) VALUES($1,$2,'profile')")
            .bind(address).bind(&entity).execute(&pool).await.unwrap();
        deployed.push((address, entity));
    }
    for case in fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["failures"] == 0)
    {
        for fail_delivery in [false, true] {
            cleanup_pair(&db, sender, receiver).await;
            if fail_delivery {
                sqlx::raw_sql("CREATE FUNCTION social.reject_native_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'native delivery unavailable'; END $$; CREATE TRIGGER reject_native_delivery BEFORE INSERT ON social.notification_deliveries FOR EACH ROW EXECUTE FUNCTION social.reject_native_delivery()")
                .execute(&pool).await.unwrap();
            }
            let ctx = test_ctx(db.clone(), Arc::new(FakeVoiceRooms::new(pool.clone())));
            let action = if case["action"] == "accept" {
                db.apply_friendship_action(receiver, sender, "request", false, None, None)
                    .await
                    .unwrap();
                upsert_friendship_payload::Action::Accept(
                    upsert_friendship_payload::AcceptPayload {
                        user: Some(User {
                            address: receiver.into(),
                        }),
                    },
                )
            } else {
                upsert_friendship_payload::Action::Request(
                    upsert_friendship_payload::RequestPayload {
                        user: Some(User {
                            address: receiver.into(),
                        }),
                        message: case["message"].as_str().map(str::to_string),
                    },
                )
            };
            let started = chrono::Utc::now().timestamp_millis();
            let response = SocialServiceImpl
                .upsert_friendship(
                    UpsertFriendshipPayload {
                        action: Some(action),
                    },
                    procedure_ctx(&ctx, sender, 5801),
                )
                .await
                .unwrap();
            assert!(
                matches!(
                    response.response,
                    Some(upsert_friendship_response::Response::Accepted(_))
                ),
                "native delivery failure cannot undo the committed mutation"
            );
            let friendship_id = db
                .last_friendship_action(sender, receiver)
                .await
                .unwrap()
                .unwrap()
                .friendship_id;
            let action_id = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM social.friendship_actions WHERE friendship_id=$1 AND action=$2",
            )
            .bind(friendship_id)
            .bind(case["action"].as_str().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
            let row: Option<(String,String,Value,i64)> = sqlx::query_as("SELECT address,type,metadata,(EXTRACT(EPOCH FROM occurred_at)*1000)::bigint FROM social.notifications WHERE id=$1").bind(action_id).fetch_optional(&pool).await.unwrap();
            let deliveries: Vec<(String,String,i32,Option<String>)> = sqlx::query_as("SELECT channel,state,attempts,last_error FROM social.notification_deliveries WHERE notification_id=$1").bind(action_id).fetch_all(&pool).await.unwrap();
            if fail_delivery {
                assert!(
                    row.is_none(),
                    "failed outbox write rolls back its inbox row"
                );
                assert!(deliveries.is_empty());
                sqlx::raw_sql("DROP TRIGGER reject_native_delivery ON social.notification_deliveries; DROP FUNCTION social.reject_native_delivery()")
                .execute(&pool).await.unwrap();
            } else {
                let (address, kind, mut metadata, timestamp) = row.expect("native inbox row");
                assert_eq!(address, receiver);
                assert_eq!(
                    kind,
                    if case["action"] == "accept" {
                        "social_service_friendship_accepted"
                    } else {
                        "social_service_friendship_request"
                    }
                );
                assert!((started..=chrono::Utc::now().timestamp_millis()).contains(&timestamp));
                assert_eq!(metadata["requestId"], action_id.to_string());
                let expected = &case["events"][0]["metadata"];
                metadata["requestId"] = expected["requestId"].clone();
                assert_eq!(
                    metadata, *expected,
                    "{} source profile metadata",
                    case["id"]
                );
                assert_eq!(
                    deliveries,
                    vec![(
                        "email".into(),
                        "skipped".into(),
                        0,
                        Some("no email template".into())
                    )]
                );
            }
        }
    }
    cleanup_pair(&db, sender, receiver).await;
    for (address, entity) in deployed {
        drop_profile(&pool, address, &entity).await;
    }
    database.drop().await;
}
