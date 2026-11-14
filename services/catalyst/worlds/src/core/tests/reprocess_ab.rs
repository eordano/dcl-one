//! `POST /reprocess-ab` against the real schema, with a stand-in abgen on the
//! loopback: upstream's bearer, validator and `No scenes found for
//! reprocessing` answers, the 500 without a converter, and the
//! `{baseUrl, batch, successful, failed}` answer of the scenes handed to the
//! converter (`reprocess-ab-handler.spec.ts`).

use std::sync::Mutex;

use axum::extract::Path as AxumPath;

use super::super::reprocess::{Converter, NO_SCENES_MESSAGE};
use super::*;

/// The tag of an entity the stand-in abgen refuses. It holds letters outside
/// the hex alphabet, so the random part of an entity id never matches it.
const REFUSED: &str = "refused";

/// A stand-in abgen: records each manifest path it is asked for and answers
/// 500 for an entity id that contains [`REFUSED`], 200 otherwise.
struct FakeAbgen {
    base: String,
    asked: Arc<Mutex<Vec<String>>>,
}

async fn fake_abgen() -> FakeAbgen {
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = asked.clone();
    let app = Router::new().route(
        "/manifest/{file}",
        axum::routing::get(move |AxumPath(file): AxumPath<String>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(file.clone());
                if file.contains(REFUSED) {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FakeAbgen {
        base: format!("http://{addr}"),
        asked,
    }
}

/// The fixture's routes with `converter` handed the scenes.
fn app_with(f: &Fixture, converter: Option<Converter>) -> Router {
    router(Arc::new(
        CoreState::new(
            f.pool.clone(),
            f.storage.clone(),
            f.state.policy.clone(),
            test_settings(),
            None,
        )
        .with_converter(converter),
    ))
}

async fn seed_scene(pool: &PgPool, world: &WorldName, entity_id: &str, chain: &str) {
    sqlx::query("INSERT INTO worlds.worlds (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
        .bind(world.key())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO worlds.world_scenes (world_name, entity_id, deployment_auth_chain, entity, deployer, parcels, size) \
         VALUES ($1, $2, $3::json, '{}', 'owner', ARRAY['0,0'], 0)",
    )
    .bind(world.key())
    .bind(entity_id)
    .bind(chain)
    .execute(pool)
    .await
    .unwrap();
}

fn entity(tag: &str) -> String {
    format!("bafy{tag}{}", &Uuid::new_v4().simple().to_string()[..12])
}

fn reprocess(body: Value) -> Case {
    Case::new("post", "/reprocess-ab")
        .bearer(ADMIN_TOKEN)
        .json(&body)
}

#[tokio::test]
async fn the_bearer_and_the_validator_answer_before_the_converter() {
    let Some(f) = fixture().await else { return };
    let app = app_with(&f, None);
    let missing = expect(&app, Case::new("post", "/reprocess-ab"), 401).await;
    assert_eq!(missing["message"], "Authorization header is missing");
    let wrong = expect(&app, Case::new("post", "/reprocess-ab").bearer("nope"), 401).await;
    assert_eq!(wrong["message"], "Invalid authorization header");
    let not_json = expect(
        &app,
        Case::new("post", "/reprocess-ab")
            .bearer(ADMIN_TOKEN)
            .body(b"{}".to_vec(), "text/plain"),
        415,
    )
    .await;
    assert_eq!(
        not_json,
        json!({ "ok": false, "message": "Content-Type must be application/json" })
    );
    let invalid = expect(&app, reprocess(json!({ "worlds": [] })), 400).await;
    assert_eq!(invalid["message"], "Invalid JSON body");
    assert_eq!(invalid["data"][0]["keyword"], "minItems");
}

#[tokio::test]
async fn without_a_converter_a_valid_request_answers_500() {
    let Some(f) = fixture().await else { return };
    let world = world_name();
    seed_scene(&f.pool, &world, &entity("a"), "[]").await;
    let app = app_with(&f, None);
    let body = expect(
        &app,
        reprocess(json!({ "worlds": [{ "worldName": world.key() }] })),
        500,
    )
    .await;
    assert_eq!(body, json!({ "error": "Internal Server Error" }));
}

#[tokio::test]
async fn every_scene_of_the_named_worlds_goes_to_the_converter() {
    let Some(f) = fixture().await else { return };
    let abgen = fake_abgen().await;
    let app = app_with(
        &f,
        Some(Converter::new(&abgen.base, reqwest::Client::new())),
    );
    let world = world_name();
    let other = world_name();
    let (good, bad, elsewhere) = (entity("good"), entity(REFUSED), entity("other"));
    let chain = r#"[{"type":"SIGNER","payload":"0xowner","signature":""}]"#;
    seed_scene(&f.pool, &world, &good, chain).await;
    seed_scene(&f.pool, &world, &bad, "[]").await;
    seed_scene(&f.pool, &other, &elsewhere, "[]").await;

    let upper = world.key().to_uppercase().replace(".DCL.ETH", ".dcl.eth");
    let answer = expect(
        &app,
        reprocess(json!({ "worlds": [{ "worldName": upper }] })),
        200,
    )
    .await;
    assert_eq!(
        answer["baseUrl"],
        "https://worlds.example/worlds-content-server"
    );
    assert_eq!(
        (answer["successful"].as_u64(), answer["failed"].as_u64()),
        (Some(1), Some(1))
    );
    let batch = answer["batch"].as_array().unwrap();
    let ids: Vec<&str> = batch
        .iter()
        .map(|b| b["entity"]["entityId"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(
        ids.contains(&good.as_str()) && ids.contains(&bad.as_str()),
        "{answer}"
    );
    let item = batch
        .iter()
        .find(|b| b["entity"]["entityId"] == good.as_str())
        .unwrap();
    assert_eq!(
        item["entity"]["authChain"],
        serde_json::from_str::<Value>(chain).unwrap()
    );
    assert_eq!(
        item["contentServerUrls"],
        json!(["https://worlds.example/worlds-content-server"])
    );
    let mut asked = abgen.asked.lock().unwrap().clone();
    asked.sort();
    let mut expected = vec![
        format!("{bad}_mac.json"),
        format!("{bad}_windows.json"),
        format!("{good}_mac.json"),
        format!("{good}_windows.json"),
    ];
    expected.sort();
    assert_eq!(asked, expected, "the other world is not reprocessed");

    // `entityIds` keeps only the listed scenes of that world.
    abgen.asked.lock().unwrap().clear();
    let filtered = expect(
        &app,
        reprocess(json!({ "worlds": [
            { "worldName": world.key(), "entityIds": [good] },
            { "worldName": other.key(), "entityIds": [] },
        ] })),
        200,
    )
    .await;
    let ids: Vec<&str> = filtered["batch"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["entity"]["entityId"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{filtered}");
    assert!(ids.contains(&good.as_str()) && ids.contains(&elsewhere.as_str()));
    assert_eq!(filtered["successful"], 2);

    let none = expect(
        &app,
        reprocess(json!({ "worlds": [{ "worldName": world_name().key() }] })),
        400,
    )
    .await;
    assert_eq!(
        none,
        json!({ "error": "Bad request", "message": NO_SCENES_MESSAGE })
    );
}

#[tokio::test]
async fn an_unreachable_converter_fails_the_scenes() {
    let Some(f) = fixture().await else { return };
    // A port nothing listens on: bind one, then release it.
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let app = app_with(
        &f,
        Some(Converter::new(
            &format!("http://{addr}"),
            reqwest::Client::new(),
        )),
    );
    let world = world_name();
    seed_scene(&f.pool, &world, &entity("a"), "[]").await;
    let answer = expect(
        &app,
        reprocess(json!({ "worlds": [{ "worldName": world.key() }] })),
        200,
    )
    .await;
    assert_eq!(
        (answer["successful"].as_u64(), answer["failed"].as_u64()),
        (Some(0), Some(1))
    );
}

#[tokio::test]
async fn a_deny_listed_world_is_not_reprocessed() {
    let world = world_name();
    let Some(f) = fixture_blocking(world.key()).await else {
        return;
    };
    let abgen = fake_abgen().await;
    let app = app_with(
        &f,
        Some(Converter::new(&abgen.base, reqwest::Client::new())),
    );
    seed_scene(&f.pool, &world, &entity("a"), "[]").await;
    let none = expect(
        &app,
        reprocess(json!({ "worlds": [{ "worldName": world.key() }] })),
        400,
    )
    .await;
    assert_eq!(none["message"], NO_SCENES_MESSAGE);
    assert!(abgen.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_route_answers_while_worlds_writes_are_disabled() {
    let Some(f) = fixture().await else { return };
    let app = crate::writes::guard(app_with(&f, None), false);
    let body = expect(
        &app,
        reprocess(json!({ "worlds": [{ "worldName": world_name().key() }] })),
        500,
    )
    .await;
    assert_eq!(body, json!({ "error": "Internal Server Error" }));
}
