//! A retried deployment keeps its failure record until the flush that writes it commits: the
//! retry worker deleted the record as soon as the entity was buffered, so a crash before the
//! flush, or a flush that failed and could not record it again, lost the entity for good. A retry cycle walks every due record in its database, so this runs
//! in a binary of its own over a scratch database of its own.

use std::sync::Arc;

use alloy::signers::{Signer, local::PrivateKeySigner};
use catalyst_content::failed_deployments_store::LiveFailedDeploymentsStore;
use catalyst_content::sync::retry_failed::{RetryFailedConfig, RetryFailedDeployments};
use catalyst_content::sync::{AuthChain, AuthLink, AuthLinkType, FailedDeployment, FailureReason};
use catalyst_content::sync_deployer::LiveSyncDeployer;

const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

async fn signed_chain(entity_id: &str) -> AuthChain {
    let key: PrivateKeySigner = TEST_KEY.parse().unwrap();
    let signature = key.sign_message(entity_id.as_bytes()).await.unwrap();
    vec![
        AuthLink {
            link_type: AuthLinkType::SIGNER,
            payload: format!("{:#x}", key.address()),
            signature: None,
        },
        AuthLink {
            link_type: AuthLinkType::EcdsaSignedEntity,
            payload: entity_id.to_string(),
            signature: Some(signature.to_string()),
        },
    ]
}

#[tokio::test]
async fn a_retried_deployment_keeps_its_record_until_its_write_commits() {
    let Some(pool) = catalyst_db::scratch::pool(4).await else {
        return;
    };
    let body = br#"{"version":"v3","type":"scene","pointers":["retry:0,0"],"timestamp":1000,"content":[],"metadata":{"z":9007199254740993,"a":{"z":1,"a":2}}}"#.to_vec();
    let original = body.clone();
    let entity_id = dcl_auth::hash_bytes_v1(&body);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/contents/{hash}",
        axum::routing::get(move || {
            let body = body.clone();
            async move { body }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let store = Arc::new(LiveFailedDeploymentsStore::new(pool.clone()));
    store
        .report_failure(FailedDeployment {
            entity_type: "scene".into(),
            entity_id: entity_id.clone(),
            reason: FailureReason::DeploymentError,
            auth_chain: signed_chain(&entity_id).await,
            error_description: "content server unreachable".into(),
            failure_timestamp: 1_700_000_000_000,
            snapshot_hash: None,
            retry_count: 0,
            next_retry_at: 0,
        })
        .await
        .unwrap();

    let tmp = std::env::temp_dir().join(format!(
        "catalyst-retry-record-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(catalyst_storage::Storage::from_config(&tmp).await.unwrap());
    let deployer = Arc::new(LiveSyncDeployer::new(
        pool.clone(),
        1,
        catalyst_content::sync_deployer::SyncValidation::AuthChainOnly,
    ));
    let retry = RetryFailedDeployments::new(
        RetryFailedConfig::default(),
        reqwest::Client::new(),
        storage.clone(),
        deployer.clone(),
        store.clone(),
        Arc::new(tokio::sync::RwLock::new(vec![peer.clone()])),
    );
    let count = |sql: &'static str| {
        let pool = pool.clone();
        let entity_id = entity_id.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(entity_id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let recorded = "SELECT count(*) FROM content.failed_deployments WHERE entity_id = $1";
    let deployed = "SELECT count(*) FROM content.deployments WHERE entity_id = $1";

    let stats = retry.execute_retry_cycle().await.unwrap();
    assert_eq!(stats.succeeded, 1);
    assert_eq!(count(deployed).await, 0, "the retry only buffered it");
    assert_eq!(
        count(recorded).await,
        1,
        "a buffered deployment is not durable: its record stays"
    );

    // Lose the in-memory batch, then rebuild from the durable record and bytes.
    drop(retry);
    drop(deployer);
    assert_eq!(
        storage
            .retrieve(&entity_id)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        original
    );
    let deployer = Arc::new(LiveSyncDeployer::new(
        pool.clone(),
        1,
        catalyst_content::sync_deployer::SyncValidation::AuthChainOnly,
    ));
    let retry = RetryFailedDeployments::new(
        RetryFailedConfig::default(),
        reqwest::Client::new(),
        storage,
        deployer.clone(),
        store,
        Arc::new(tokio::sync::RwLock::new(vec![peer])),
    );
    sqlx::query(
        "UPDATE content.failed_deployments SET next_retry_at=to_timestamp(0) WHERE entity_id=$1",
    )
    .bind(&entity_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(retry.execute_retry_cycle().await.unwrap().succeeded, 1);
    assert_eq!(count(deployed).await, 0);
    assert_eq!(count(recorded).await, 1);
    deployer.flush().await.unwrap();
    assert_eq!(count(deployed).await, 1);
    assert_eq!(
        count(recorded).await,
        0,
        "the transaction that wrote it cleared the record"
    );

    let metadata: String = sqlx::query_scalar(
        "SELECT entity_metadata::text FROM content.deployments WHERE entity_id=$1",
    )
    .bind(&entity_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        metadata,
        r#"{"v":{"z":9007199254740993,"a":{"z":1,"a":2}}}"#
    );
    server.abort();
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    pool.close().await;
}
