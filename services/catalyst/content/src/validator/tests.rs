//! The live [`Sources`] behind the rules: the public [`validate`] over a
//! signed entity and a real blob store, and every access check against a
//! chain on the loopback ([`super::fake_chain`]), with the old error strings
//! compared byte for byte and every missing or failing source answering
//! `Unavailable`. The escrow overlay runs on the process's scratch database
//! and skips without `CATALYST_TEST_DATABASE_URL`.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, U256};
use axum::Router;
use axum::routing::get;
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

use super::checker::{BlockchainChecker, validate_item_access, validate_profile_access};
use super::fake_chain::{self, FakeChain, Query, Reply};
use super::types::OwnedDeployment;
use super::*;
use crate::auth::hash_bytes_v1;
use crate::http::Outbound;
use crate::tests::support::require_test_db;

const OWNER: &str = "0xaaaa000000000000000000000000000000000001";
const OPERATOR: &str = "0xbbbb000000000000000000000000000000000002";
const STRANGER: &str = "0xcccc000000000000000000000000000000000003";
const STORED_HASH: &str = "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenosa7776";

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
        .unwrap()
}

async fn temp_storage(tag: &str) -> (Arc<Storage>, std::path::PathBuf) {
    let tmp = std::env::temp_dir().join(format!(
        "catalyst-validator-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Storage::from_config(&tmp).await.unwrap();
    storage
        .store(STORED_HASH, Bytes::from_static(b"content"))
        .await
        .unwrap();
    (Arc::new(storage), tmp)
}

/// Mainnet contracts, no RPC.
fn mainnet() -> Settings {
    Settings {
        eth_network: "mainnet".into(),
        ..Settings::default()
    }
}

/// Mainnet contracts over `chain` on both layers.
fn over(chain: &FakeChain) -> Settings {
    Settings {
        l1_rpc_url: Some(chain.url.clone()),
        l2_rpc_url: Some(chain.url.clone()),
        ..mainnet()
    }
}

fn sources(pool: PgPool, storage: Arc<Storage>, settings: Settings) -> Sources {
    Sources::new(pool, storage, Outbound::for_tests(), None, settings)
}

fn addr(s: &str) -> Address {
    s.parse().unwrap()
}

/// An entity file signed by a fixed key: the entity, its files (the entity
/// file under its id, `uploads` under their hashes) and the auth chain.
async fn signed(
    entity: Value,
    uploads: &[Vec<u8>],
) -> (
    Entity,
    HashMap<String, bytes::Bytes>,
    crate::auth::AuthChain,
    String,
) {
    use alloy::signers::{Signer, local::PrivateKeySigner};
    let bytes = serde_json::to_vec(&entity).unwrap();
    let id = hash_bytes_v1(&bytes);
    let key: PrivateKeySigner = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
        .parse()
        .unwrap();
    let signature = key.sign_message(id.as_bytes()).await.unwrap().to_string();
    let signer = format!("{:#x}", key.address());
    let chain = serde_json::from_value(json!([
        {"type": "SIGNER", "payload": signer},
        {"type": "ECDSA_SIGNED_ENTITY", "payload": id, "signature": signature}
    ]))
    .unwrap();
    let parsed = entity_parser::parse_entity_from_bytes(&bytes, &id).unwrap();
    let mut files = HashMap::from([(id.clone(), bytes.into())]);
    for upload in uploads {
        files.insert(hash_bytes_v1(upload), upload.clone().into());
    }
    (parsed, files, chain, signer)
}

fn scene(parcels: &[&str], model: &[u8]) -> Value {
    json!({
        "version": "v3",
        "type": "scene",
        "pointers": parcels,
        "timestamp": chrono::Utc::now().timestamp_millis(),
        "content": [{"file": "model.glb", "hash": hash_bytes_v1(model)}],
        "metadata": {"main": "bin/index.js", "scene": {"base": parcels[0], "parcels": parcels}}
    })
}

fn rejected(messages: &[&str]) -> Result<(), Vec<ValidationError>> {
    Err(messages
        .iter()
        .map(|m| ValidationError::Rejected(m.to_string()))
        .collect())
}

fn unavailable(messages: &[&str]) -> Result<(), Vec<ValidationError>> {
    Err(messages
        .iter()
        .map(|m| ValidationError::Unavailable(m.to_string()))
        .collect())
}

#[tokio::test]
async fn a_signed_scene_validates_end_to_end_and_its_land_leg_decides() {
    let (storage, tmp) = temp_storage("scene").await;
    let signer = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
    let chain = fake_chain::serve(fake_chain::land_owners(vec![
        (signer.to_string(), 5, 5),
        (STRANGER.to_string(), 5, 6),
    ]))
    .await;
    let with_land = sources(lazy_pool(), storage.clone(), over(&chain));
    let model = b"glb bytes".to_vec();

    let (entity, files, chain, deployer) =
        signed(scene(&["5,5"], &model), std::slice::from_ref(&model)).await;
    assert_eq!(deployer, signer);
    let ctx = Context {
        sources: &with_land,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        Ok(()),
        "the owner's signed scene with its file at hand"
    );

    for (parcel, message) in [
        (
            "5,6",
            "The provided Eth Address does not have access to the following parcel: (5,6)",
        ),
        (
            "9,9",
            "The provided Eth Address does not have access to the following parcel: (9,9)",
        ),
    ] {
        let (entity, files, chain, _) =
            signed(scene(&[parcel], &model), std::slice::from_ref(&model)).await;
        let ctx = Context {
            sources: &with_land,
            auth_chain: &chain,
        };
        assert_eq!(
            validate(&entity, &files, &ctx).await,
            rejected(&[message]),
            "{parcel}"
        );
    }

    let (entity, files, chain, _) = signed(scene(&["5,5"], &model), &[]).await;
    let ctx = Context {
        sources: &with_land,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        rejected(&[&format!(
            "Couldn't fetch content file with hash: {}",
            hash_bytes_v1(&model)
        )]),
        "a file neither uploaded nor stored fails the size step, which runs before the cross-check"
    );

    let referencing_stored = json!({
        "version": "v3", "type": "scene", "pointers": ["5,5"], "timestamp": chrono::Utc::now().timestamp_millis(),
        "content": [{"file": "stored.bin", "hash": STORED_HASH}],
        "metadata": {"main": "bin/index.js", "scene": {"base": "5,5", "parcels": ["5,5"]}}
    });
    let (entity, files, chain, _) = signed(referencing_stored.clone(), &[]).await;
    let ctx = Context {
        sources: &with_land,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        Ok(()),
        "a file this node already stores need not be uploaded"
    );
    let (entity, files, chain, _) = signed(referencing_stored, std::slice::from_ref(&model)).await;
    let ctx = Context {
        sources: &with_land,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        rejected(&[&format!(
            "This hash was uploaded but is not referenced in the entity: {}",
            hash_bytes_v1(&model)
        )])
    );

    let without_land = sources(lazy_pool(), storage, mainnet());
    let (entity, files, chain, _) =
        signed(scene(&["5,5"], &model), std::slice::from_ref(&model)).await;
    let ctx = Context {
        sources: &without_land,
        auth_chain: &chain,
    };
    let verdict = validate(&entity, &files, &ctx).await;
    assert_eq!(
        verdict,
        unavailable(&[super::checker::PARCEL_ACCESS_UNDECIDED])
    );
    assert!(matches!(
        into_deploy_failure(verdict.unwrap_err()),
        super::super::write_deployer::DeployFailure::Unavailable(_)
    ));

    let ignoring = sources(
        lazy_pool(),
        without_land.storage.clone(),
        Settings {
            ignore_blockchain_access: true,
            ..mainnet()
        },
    );
    let ctx = Context {
        sources: &ignoring,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        Ok(()),
        "IGNORE_BLOCKCHAIN_ACCESS_CHECKS skips the LAND leg"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn the_structure_signature_and_size_steps_answer_the_old_sentences() {
    let (storage, tmp) = temp_storage("steps").await;
    let s = sources(
        lazy_pool(),
        storage,
        Settings {
            ignore_blockchain_access: true,
            ..mainnet()
        },
    );
    let model = b"glb".to_vec();

    let (mut entity, files, chain, _) =
        signed(scene(&["1,1"], &model), std::slice::from_ref(&model)).await;
    let ctx = Context {
        sources: &s,
        auth_chain: &chain,
    };
    entity.pointers = vec!["1,1".into(), "1,1".into()];
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        rejected(&["There are repeated pointers in your request."])
    );
    entity.pointers.clear();
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        rejected(&["The entity needs to be pointed by one or more pointers."])
    );
    entity.pointers = vec!["1,1".into()];
    entity.version = "v2".into();
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        rejected(&[
            "Only entities v3 are allowed after the ADR-45. Check http://adr.decentraland.org/adr/ADR-45 for more information"
        ])
    );
    entity.version = "v3".into();

    let (other, _, other_chain, _) = signed(scene(&["2,2"], &model), &[]).await;
    let forged = Context {
        sources: &s,
        auth_chain: &other_chain,
    };
    let verdict = validate(&entity, &files, &forged).await.unwrap_err();
    assert!(
        matches!(&verdict[..], [ValidationError::Rejected(m)] if m.starts_with("The signature is invalid. ")),
        "a chain signing another entity ({}) is the depositor's: {verdict:?}",
        other.id
    );

    let big = vec![0u8; 16 * 1024 * 1024];
    let (entity, files, chain, _) = signed(scene(&["1,1"], &big), std::slice::from_ref(&big)).await;
    let ctx = Context {
        sources: &s,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        rejected(&[
            "The deployment is too big. The maximum allowed size per pointer is 15 MB for scene. You can upload up to 15728640 bytes but you tried to upload 16777216."
        ])
    );

    let (entity, files, chain, _) = signed(
        json!({
            "version": "v3", "type": "scene", "pointers": ["1,1"], "timestamp": chrono::Utc::now().timestamp_millis(),
            "content": [{"file": "model.glb", "hash": hash_bytes_v1(&model)}],
            "metadata": {"main": "bin/index.js", "scene": {"base": "1,1", "parcels": ["1,1"]}, "worldConfiguration": {"name": "x.dcl.eth"}}
        }),
        std::slice::from_ref(&model),
    )
    .await;
    let ctx = Context {
        sources: &s,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        rejected(&[
            "The scene.json contains a worldConfiguration section, which is not allowed for Genesis City scenes (see ADR-173: http://adr.decentraland.org/adr/ADR-173). Please remove it and try again."
        ])
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The metadata schema of the type runs before the signature: a scene
/// without its entry point is the depositor's fault, and so is an entity
/// without a version only when it names another one.
#[tokio::test]
async fn the_metadata_schema_and_version_steps_answer_the_reference_sentences() {
    let (storage, tmp) = temp_storage("metadata").await;
    let s = sources(
        lazy_pool(),
        storage,
        Settings {
            ignore_blockchain_access: true,
            ..Settings::default()
        },
    );
    let model = b"glb".to_vec();

    let mut no_main = scene(&["1,1"], &model);
    no_main["metadata"].as_object_mut().unwrap().remove("main");
    let (entity, files, chain, _) = signed(no_main, std::slice::from_ref(&model)).await;
    let ctx = Context {
        sources: &s,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        rejected(&[
            "The metadata for this entity type (scene) is not valid.",
            "metadata must have required property 'main'"
        ])
    );

    let mut unversioned = scene(&["1,1"], &model);
    unversioned.as_object_mut().unwrap().remove("version");
    let (_, files, chain, _) = signed(unversioned.clone(), std::slice::from_ref(&model)).await;
    let mut entity: Entity = serde_json::from_value(unversioned).unwrap();
    entity.id = chain[1].payload.clone();
    let ctx = Context {
        sources: &s,
        auth_chain: &chain,
    };
    assert_eq!(
        validate(&entity, &files, &ctx).await,
        Ok(()),
        "a missing version is v3"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A contract-wallet link this node cannot ask about is undecided: without
/// `ETH_RPC_URL` the verifier answers "not implemented", which is not the
/// depositor's fault.
#[tokio::test]
async fn a_contract_wallet_chain_without_an_rpc_validator_is_unavailable() {
    let (storage, tmp) = temp_storage("eip1654").await;
    let s = sources(
        lazy_pool(),
        storage,
        Settings {
            ignore_blockchain_access: true,
            ..mainnet()
        },
    );
    let model = b"glb".to_vec();
    let (entity, files, _, _) = signed(scene(&["1,1"], &model), std::slice::from_ref(&model)).await;
    let chain: crate::auth::AuthChain = serde_json::from_value(json!([
        {"type": "SIGNER", "payload": "0x1111111111111111111111111111111111111111"},
        {"type": "ECDSA_EIP_1654_SIGNED_ENTITY", "payload": entity.id, "signature": "0x00"}
    ]))
    .unwrap();
    let ctx = Context {
        sources: &s,
        auth_chain: &chain,
    };
    let verdict = validate(&entity, &files, &ctx).await.unwrap_err();
    assert!(
        matches!(&verdict[..], [ValidationError::Unverifiable(m)] if m.starts_with("The signature is invalid. EIP-1654")),
        "{verdict:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
fn seal_shard(storage: &Storage, hash: &str) -> std::path::PathBuf {
    let shard = storage.root().join(crate::storage::hex_prefix(hash));
    std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o000)).unwrap();
    shard
}

#[cfg(unix)]
fn unseal_shard(shard: &std::path::Path) {
    let _ = std::fs::set_permissions(shard, std::fs::Permissions::from_mode(0o755));
}

#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_shard_faults_the_size_probe_instead_of_answering_absent() {
    let (storage, tmp) = temp_storage("size").await;
    let s = sources(lazy_pool(), storage.clone(), mainnet());
    assert_eq!(
        s.fetch_content_file_size(STORED_HASH).await.unwrap(),
        Some(7)
    );

    let shard = seal_shard(&storage, STORED_HASH);
    let size = s.fetch_content_file_size(STORED_HASH).await;
    let exist = s
        .is_content_stored_already(&[STORED_HASH.to_string()])
        .await;
    unseal_shard(&shard);
    let _ = std::fs::remove_dir_all(&tmp);

    let err = size.expect_err("damage this node cannot read is never a depositor's miss");
    assert!(err.contains("I/O error"), "unexpected cause: {err}");
    assert!(
        exist
            .expect_err("the existence probe faults too")
            .contains("I/O error")
    );
}

#[tokio::test]
async fn an_id_that_cannot_name_content_is_absent_rather_than_a_fault() {
    let (storage, tmp) = temp_storage("invalid").await;
    let s = sources(lazy_pool(), storage, mainnet());
    let bogus = "../../etc/passwd".to_string();
    assert_eq!(s.fetch_content_file_size(&bogus).await.unwrap(), None);
    let stored = s
        .is_content_stored_already(std::slice::from_ref(&bogus))
        .await
        .unwrap();
    assert_eq!(stored.get(&bogus), Some(&false));
    let _ = std::fs::remove_dir_all(&tmp);
}

fn profile(owner: &str, name: &str, wearables: &[&str]) -> OwnedDeployment {
    OwnedDeployment::new(Entity {
        id: "bafkreiprofile".into(),
        entity_type: EntityType::Profile,
        pointers: vec![owner.to_string()],
        timestamp: 1_700_000_000_000,
        content: vec![],
        version: "v3".into(),
        metadata: Some(json!({ "avatars": [{
            "name": name, "hasClaimedName": true,
            "avatar": { "wearables": wearables, "emotes": [{ "slot": 0, "urn": "urn:decentraland:off-chain:base-emotes:wave" }] }
        }]})),
    })
}

/// The deploy's L1/L2 blocks for the fixed test timestamp.
const TS: i64 = 1_700_000_000_000;

#[tokio::test]
async fn claimed_names_are_asked_of_the_registrar() {
    let (storage, tmp) = temp_storage("names").await;
    let chain = fake_chain::serve(|q, _| match q {
        Query::Name { sender, name, .. } => Reply::Yes(*sender == addr(OWNER) && name == "alice"),
        other => panic!("{other:?}"),
    })
    .await;
    let s = sources(lazy_pool(), storage.clone(), over(&chain));

    let owned = profile(OWNER, "alice", &[]);
    assert!(
        validate_profile_access(&owned.view(), &s, OWNER)
            .await
            .is_ok()
    );
    let other = profile(OWNER, "bob", &[]);
    assert_eq!(
        validate_profile_access(&other.view(), &s, OWNER)
            .await
            .into_result(),
        rejected(&[&format!(
            "The following names (bob) are not owned by the address {OWNER}."
        )])
    );
    assert_eq!(
        validate_profile_access(
            &owned.view(),
            &s,
            "0xAAAA000000000000000000000000000000000009"
        )
        .await
        .into_result(),
        rejected(&[
            "You can only alter your own profile. The pointer address and the signer address are different (pointer:0xaaaa000000000000000000000000000000000001 signer: 0xaaaa000000000000000000000000000000000009)"
        ])
    );

    let no_rpc = sources(lazy_pool(), storage.clone(), mainnet());
    assert_eq!(
        validate_profile_access(&owned.view(), &no_rpc, OWNER)
            .await
            .into_result(),
        unavailable(&["Error checking name ownership."])
    );
    let failing = fake_chain::serve(|_, _| Reply::Yes(true)).await;
    failing.set_down(true);
    let down = sources(lazy_pool(), storage, over(&failing));
    let verdict = validate_profile_access(&owned.view(), &down, OWNER)
        .await
        .into_result()
        .unwrap_err();
    assert!(
        matches!(&verdict[..], [ValidationError::Unavailable(m)] if m == "Error checking name ownership."),
        "{verdict:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

fn random_address() -> String {
    let hex = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    format!("0x{}", &hex[..40])
}

/// Items with a token are asked by `ownerOf`, assets and off-chain items are
/// not asked, and an active escrow lease stands in for a token the wearer does
/// not hold (the overlay's SQL runs on the scratch database, so the whole
/// check skips without it).
#[tokio::test]
async fn owned_items_come_from_the_chain_and_the_escrow_overlay() {
    require_test_db!(Some(pool) = crate::db::scratch::pool(2).await);
    let (storage, tmp) = temp_storage("items").await;
    let owner = random_address();
    let mine = "0xaaa0000000000000000000000000000000000001";
    let theirs = "0xccc0000000000000000000000000000000000003";
    let escrowed = "0xddd0000000000000000000000000000000000004";
    let recent = "0xeee0000000000000000000000000000000000005";
    let holder = owner.clone();
    let chain = fake_chain::serve(move |q, block| match q {
        Query::OwnerOf { contract, .. } => Reply::Owner(
            if *contract == addr(mine)
                || (*contract == addr(recent) && block == fake_chain::at_deployment(TS))
            {
                addr(&holder)
            } else {
                addr(STRANGER)
            },
        ),
        other => panic!("{other:?}"),
    })
    .await;
    let s = sources(pool.clone(), storage.clone(), over(&chain));

    let unowned = format!("urn:decentraland:matic:collections-v2:{theirs}:0:3");
    let leased_token = format!("urn:decentraland:matic:collections-v2:{escrowed}:0:9");
    let wearables = [
        format!("urn:decentraland:matic:collections-v2:{mine}:1:42"),
        "urn:decentraland:mainnet:collections-v1:halloween_2019:hat".to_string(),
        format!("urn:decentraland:matic:collections-v2:{theirs}:0"),
        "urn:decentraland:off-chain:base-avatars:eyes_00".to_string(),
        unowned.clone(),
        leased_token.clone(),
    ];
    let refs: Vec<&str> = wearables.iter().map(String::as_str).collect();
    let mut d = profile(&owner, "", &refs);
    d.entity.metadata.as_mut().unwrap()["avatars"][0]["hasClaimedName"] = json!(false);
    assert_eq!(
        validate_profile_access(&d.view(), &s, &owner)
            .await
            .into_result(),
        rejected(&[&format!(
            "The following items ({unowned}, {leased_token}) are not owned by the address {owner}."
        )]),
        "a held token passes; assets and off-chain items are not asked"
    );

    sqlx::query(
        "INSERT INTO shop.usage_grants (grantee_address, urn, token_id, category, unlock_at) VALUES \
         ($1, $2, '9', 'wearable', timezone('UTC', now()) + interval '2 days')",
    )
    .bind(&owner)
    .bind(format!("urn:decentraland:matic:collections-v2:{escrowed}:0"))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        validate_profile_access(&d.view(), &s, &owner)
            .await
            .into_result(),
        rejected(&[&format!(
            "The following items ({unowned}) are not owned by the address {owner}."
        )]),
        "an active escrow lease is ownership of its token"
    );
    let bought_now = format!("urn:decentraland:matic:collections-v2:{recent}:0:1");
    let mut d2 = profile(&owner, "", &[&bought_now, &leased_token]);
    d2.entity.metadata.as_mut().unwrap()["avatars"][0]["hasClaimedName"] = json!(false);
    assert_eq!(
        validate_profile_access(&d2.view(), &s, &owner)
            .await
            .into_result(),
        Ok(()),
        "a leased item does not hold the others to one block"
    );

    sqlx::query("UPDATE shop.usage_grants SET status = 'revoked' WHERE grantee_address = $1")
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        validate_profile_access(&d.view(), &s, &owner)
            .await
            .into_result(),
        rejected(&[&format!(
            "The following items ({unowned}, {leased_token}) are not owned by the address {owner}."
        )]),
        "a revoked lease grants nothing"
    );

    let l2_unset = sources(
        pool.clone(),
        storage,
        Settings {
            l2_rpc_url: None,
            ..over(&chain)
        },
    );
    assert_eq!(
        validate_profile_access(&d.view(), &l2_unset, &owner)
            .await
            .into_result(),
        unavailable(&["Error checking item ownership."])
    );

    sqlx::query("DELETE FROM shop.usage_grants WHERE grantee_address = $1")
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    pool.close().await;
}

fn wearable(pointer: &str) -> OwnedDeployment {
    OwnedDeployment::new(Entity {
        id: "bafkreiwearable".into(),
        entity_type: EntityType::Wearable,
        pointers: vec![pointer.to_string()],
        timestamp: TS,
        content: vec![],
        version: "v3".into(),
        metadata: None,
    })
}

const COMMITTEE: &str = "0xdddd000000000000000000000000000000000004";
const MINTER: &str = "0xeeee000000000000000000000000000000000005";

const EDITABLE: &str = "0xc011000000000000000000000000000000000001";
const APPROVED: &str = "0xc011000000000000000000000000000000000002";
const APPROVED_LATELY: &str = "0xc011000000000000000000000000000000000003";
const PINNED: &str = "0xc011000000000000000000000000000000000004";

/// The ADR-32 hash of the pinned wearable below, written out by hand.
fn pinned_hash() -> String {
    hash_bytes_v1(
        format!(
            r#"{{"content":[{{"key":"model.glb","hash":"{STORED_HASH}"}}],"metadata":{{"name":"hat","id":"urn:decentraland:matic:collections-v2:{PINNED}:0","data":{{"z":1,"a":[1.5]}}}}}}"#
        )
        .as_bytes(),
    )
}

/// A wearable of the pinned collection whose metadata keys are not sorted.
fn pinned_wearable(name: &str) -> OwnedDeployment {
    let file = format!(
        r#"{{"version":"v3","type":"wearable","pointers":["urn:decentraland:matic:collections-v2:{PINNED}:0"],"timestamp":1700000000000,"content":[{{"file":"model.glb","hash":"{STORED_HASH}"}}],"metadata":{{"name":"{name}","id":"urn:decentraland:matic:collections-v2:{PINNED}:0","data":{{"z":1,"a":[1.5]}}}}}}"#
    );
    let id = hash_bytes_v1(file.as_bytes());
    let mut d =
        OwnedDeployment::new(entity_parser::parse_entity_from_bytes(file.as_bytes(), &id).unwrap());
    d.files.insert(id, file.into_bytes().into());
    d
}

/// The Checker's `validateWearables` for the collections above: the creator
/// and an item manager of an editable collection, nobody for an approved one
/// (one approved only after the block five minutes before the deploy), and a
/// committee member with the content the pinned item's hash names.
fn collection_rule(q: &Query, block: u64) -> Reply {
    let Query::Wearable {
        sender,
        collection,
        item,
        hash,
        committees,
        ..
    } = q
    else {
        panic!("{q:?}")
    };
    assert!(!committees.is_empty());
    let pinned = pinned_hash();
    let editor = *sender == addr(OWNER) || *sender == addr(OPERATOR);
    Reply::Yes(match collection.to_string().to_lowercase().as_str() {
        EDITABLE => editor && *item == U256::from(3),
        APPROVED => false,
        APPROVED_LATELY => editor && block == fake_chain::five_min_before(TS),
        PINNED => *sender == addr(COMMITTEE) && *hash == pinned,
        _ => false,
    })
}

#[tokio::test]
async fn collection_access_follows_the_checker_at_the_deploy_block() {
    let (storage, tmp) = temp_storage("collections").await;
    let chain = fake_chain::serve(collection_rule).await;
    let s = sources(lazy_pool(), storage.clone(), over(&chain));
    let item = |collection: &str| {
        wearable(&format!(
            "urn:decentraland:matic:collections-v2:{}:3",
            collection.to_uppercase().replace("0X", "0x")
        ))
    };
    let denied = |who: &str, collection: &str, id: &str| {
        rejected(&[&format!(
            "The provided Eth Address '{who}' does not have access to the following item: ({collection}, {id})"
        )])
    };

    for who in [OWNER, OPERATOR] {
        assert!(
            validate_item_access(&item(EDITABLE).view(), &s, who)
                .await
                .is_ok(),
            "an editable collection: {who}"
        );
    }
    for who in [MINTER, STRANGER] {
        assert_eq!(
            validate_item_access(&item(EDITABLE).view(), &s, who)
                .await
                .into_result(),
            denied(who, EDITABLE, "3"),
            "{who}"
        );
    }
    assert_eq!(
        validate_item_access(&item(APPROVED).view(), &s, OWNER)
            .await
            .into_result(),
        denied(OWNER, APPROVED, "3"),
        "an approved collection is frozen"
    );
    assert!(
        validate_item_access(&item(APPROVED_LATELY).view(), &s, OWNER)
            .await
            .is_ok(),
        "editable five minutes before the deploy"
    );

    assert!(
        validate_item_access(&pinned_wearable("hat").view(), &s, COMMITTEE)
            .await
            .is_ok(),
        "a committee member deploying the content the item pins"
    );
    assert_eq!(
        validate_item_access(&pinned_wearable("other").view(), &s, COMMITTEE)
            .await
            .into_result(),
        denied(COMMITTEE, PINNED, "0"),
        "content other than what the item pins"
    );
    assert_eq!(
        validate_item_access(&pinned_wearable("hat").view(), &s, OWNER)
            .await
            .into_result(),
        denied(OWNER, PINNED, "0"),
        "a pinned item is not the creator's to change"
    );

    let v1 = wearable(
        "urn:decentraland:ethereum:collections-v1:community_contest:cw_bell_attendant_hat",
    );
    assert!(
        validate_item_access(&v1.view(), &s, DECENTRALAND_ADDRESS)
            .await
            .is_ok(),
        "a v1 item by name: the Decentraland address"
    );
    assert_eq!(
        validate_item_access(&v1.view(), &s, STRANGER)
            .await
            .into_result(),
        rejected(&[&format!(
            "The provided Eth Address '{STRANGER}' does not have access to the following item: (community_contest, cw_bell_attendant_hat)"
        )])
    );
    assert_eq!(
        validate_item_access(
            &wearable("urn:decentraland:off-chain:base-avatars:hat").view(),
            &s,
            STRANGER
        )
        .await
        .into_result(),
        rejected(&[&format!(
            "The provided Eth Address '{STRANGER}' does not have access to the following item: 'urn:decentraland:off-chain:base-avatars:hat'"
        )])
    );
    assert_eq!(
        validate_item_access(
            &wearable(&format!(
                "urn:decentraland:ethereum:collections-v2:{EDITABLE}:3"
            ))
            .view(),
            &s,
            OWNER
        )
        .await
        .into_result(),
        denied(OWNER, EDITABLE, "3"),
        "an L1 collection is the Decentraland address's alone"
    );

    let no_rpc = sources(lazy_pool(), storage.clone(), mainnet());
    assert_eq!(
        validate_item_access(&item(EDITABLE).view(), &no_rpc, OWNER)
            .await
            .into_result(),
        unavailable(&["Error checking collection access."])
    );
    // The deploy block denies and the node cannot answer for the earlier one.
    let pruned = fake_chain::serve(|q, block| {
        if block == fake_chain::at_deployment(TS) {
            collection_rule(q, block)
        } else {
            Reply::Fail
        }
    })
    .await;
    let s = sources(lazy_pool(), storage, over(&pruned));
    let verdict = validate_item_access(&item(APPROVED).view(), &s, OWNER)
        .await
        .into_result()
        .unwrap_err();
    assert!(
        matches!(&verdict[..], [ValidationError::Unavailable(m)] if m == "Error checking collection access."),
        "{verdict:?}"
    );
    assert!(
        validate_item_access(&item(EDITABLE).view(), &s, OWNER)
            .await
            .is_ok(),
        "granted at the deploy block, the earlier one is not asked"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The production woodie wearable's own proof yields the root the registry
/// is asked to confirm at the deploy's L2 block.
#[tokio::test]
async fn third_party_items_are_proven_against_the_registry_at_the_deploy_block() {
    const WOODIE_0: &[u8] = include_bytes!("third_party_fixtures/woodie_0.entity.json");
    let (storage, tmp) = temp_storage("tp").await;
    let entity = entity_parser::parse_entity_from_bytes(
        WOODIE_0,
        "bafkreigxzm4duawatfhiylmawwdr4yezskltd3ygkup6nsl4yyif5t57i4",
    )
    .unwrap();
    let proof: third_party::MerkleProof =
        serde_json::from_value(entity.metadata.as_ref().unwrap()["merkleProof"].clone()).unwrap();
    let decoded: Vec<[u8; 32]> = proof
        .proof
        .iter()
        .map(|p| merkle::decode_hash32(p).unwrap())
        .collect();
    let root = merkle::generate_root(proof.index, &proof.entity_hash, &decoded);
    let timestamp = entity.timestamp;
    let mut d = OwnedDeployment::new(entity);
    d.files
        .insert(d.entity.id.clone(), WOODIE_0.to_vec().into());
    let pointer = d.entity.pointers[0].clone();

    let registry = |approved: Option<[u8; 32]>| {
        move |q: &Query, block: u64| match q {
            Query::ThirdParty { tp_id, root, .. } => {
                assert_eq!(
                    tp_id,
                    "urn:decentraland:matic:collections-thirdparty:woodies"
                );
                assert!(
                    block == fake_chain::at_deployment(timestamp)
                        || block == fake_chain::five_min_before(timestamp)
                );
                match approved {
                    Some(r) => Reply::Yes(root.0 == r),
                    None => Reply::Fail,
                }
            }
            other => panic!("{other:?}"),
        }
    };

    let good = fake_chain::serve(registry(Some(root))).await;
    let s = sources(lazy_pool(), storage.clone(), over(&good));
    assert!(
        validate_item_access(&d.view(), &s, STRANGER).await.is_ok(),
        "the proof, not the deployer, authorizes a third-party item ({pointer})"
    );

    let other = fake_chain::serve(registry(Some([0x11; 32]))).await;
    let s = sources(lazy_pool(), storage.clone(), over(&other));
    assert_eq!(
        validate_item_access(&d.view(), &s, STRANGER)
            .await
            .into_result(),
        rejected(&["Couldn't verify merkle proofed entity for third-party wearable"])
    );

    let failing = fake_chain::serve(move |q, block| match q {
        Query::ThirdParty { .. } if block == fake_chain::at_deployment(timestamp) => {
            Reply::Yes(false)
        }
        _ => Reply::Fail,
    })
    .await;
    let s = sources(lazy_pool(), storage.clone(), over(&failing));
    let verdict = validate_item_access(&d.view(), &s, STRANGER)
        .await
        .into_result()
        .unwrap_err();
    assert!(
        matches!(&verdict[..], [ValidationError::Unavailable(m)] if m == "Error checking third-party access."),
        "{verdict:?}"
    );
    let s = sources(lazy_pool(), storage, mainnet());
    assert_eq!(
        validate_item_access(&d.view(), &s, STRANGER)
            .await
            .into_result(),
        unavailable(&["Error checking third-party access."])
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn the_decentraland_addresses_are_the_constant_and_the_configured_one() {
    let (storage, tmp) = temp_storage("dcl").await;
    let s = sources(
        lazy_pool(),
        storage,
        Settings {
            additional_dcl_address: Some("0x00000000000000000000000000000000000000ee".into()),
            ..mainnet()
        },
    );
    assert!(s.is_address_owned_by_decentraland(DECENTRALAND_ADDRESS));
    assert!(s.is_address_owned_by_decentraland("0x1337E0507EB4AB47E08A179573ED4533D9E22A7B"));
    assert!(s.is_address_owned_by_decentraland("0x00000000000000000000000000000000000000EE"));
    assert!(!s.is_address_owned_by_decentraland("0xrandom"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn settings_keep_https_rpcs_only_and_refuse_bad_addresses() {
    let parse = |l2: Option<&str>, extra: Option<&str>| {
        Settings::parse(
            false,
            false,
            extra,
            " mainnet ",
            Some("https://l1.example/KEY".into()),
            l2,
        )
    };
    let ok = parse(
        Some(" https://l2.example/KEY "),
        Some("0x00000000000000000000000000000000000000EE"),
    )
    .unwrap();
    assert_eq!(ok.eth_network, "mainnet");
    assert_eq!(ok.l2_rpc_url.as_deref(), Some("https://l2.example/KEY"));
    assert_eq!(
        ok.additional_dcl_address.as_deref(),
        Some("0x00000000000000000000000000000000000000ee")
    );
    assert!(
        !format!("{ok:?}").contains("KEY"),
        "the Debug form never prints an RPC URL"
    );
    assert_eq!(
        parse(Some("http://127.0.0.1:8545"), None)
            .unwrap()
            .l2_rpc_url,
        None,
        "a plaintext L2 endpoint is not used for access checks"
    );
    assert!(
        parse(None, Some(""))
            .unwrap()
            .additional_dcl_address
            .is_none()
    );
    let err = format!("{:#}", parse(None, Some("not-an-address")).unwrap_err());
    assert!(err.contains("ADDITIONAL_DECENTRALAND_ADDRESS"), "{err}");
}

#[test]
fn a_node_taking_deploys_needs_both_rpcs_and_a_known_network() {
    let parse = |ignore: bool, writes: bool, network: &str, l1: Option<&str>, l2: Option<&str>| {
        Settings::parse(ignore, writes, None, network, l1.map(str::to_string), l2)
            .map_err(|e| format!("{e:#}"))
    };
    let (l1, l2) = (
        Some("https://l1.example/KEY"),
        Some("https://l2.example/KEY"),
    );
    assert!(parse(false, true, "mainnet", l1, l2).is_ok());
    assert!(parse(false, true, "sepolia", l1, l2).is_ok());
    for (network, l1, l2, needle) in [
        ("goerli", l1, l2, "ETH_NETWORK=goerli"),
        ("mainnet", None, l2, "ETH_RPC_URL"),
        ("mainnet", l1, None, "RPC_ENDPOINT_POLYGON"),
        (
            "mainnet",
            l1,
            Some("http://l2.example/KEY"),
            "RPC_ENDPOINT_POLYGON",
        ),
        ("mainnet", l1, Some("not a url"), "RPC_ENDPOINT_POLYGON"),
    ] {
        let err = parse(false, true, network, l1, l2).unwrap_err();
        assert!(err.contains(needle), "{err}");
        assert!(err.contains("IGNORE_BLOCKCHAIN_ACCESS_CHECKS"), "{err}");
        assert!(!err.contains("KEY"), "never the value: {err}");
        assert!(
            parse(true, true, network, l1, l2).is_ok(),
            "ignored checks need no chain"
        );
        assert!(
            parse(false, false, network, l1, l2).is_ok(),
            "a node without local deploys starts"
        );
    }
}

#[test]
fn a_node_judging_peer_entities_needs_both_rpcs_and_a_known_network() {
    let settings = |ignore: bool, network: &str, l1: Option<&str>, l2: Option<&str>| {
        Settings::parse(ignore, false, None, network, l1.map(str::to_string), l2)
            .unwrap()
            .require_chain_for_peer_entities()
            .map_err(|e| format!("{e:#}"))
    };
    let (l1, l2) = (
        Some("https://l1.example/KEY"),
        Some("https://l2.example/KEY"),
    );
    assert!(settings(false, "mainnet", l1, l2).is_ok());
    assert!(settings(false, "sepolia", l1, l2).is_ok());
    for (network, l1, l2, needle) in [
        ("goerli", l1, l2, "ETH_NETWORK=goerli"),
        ("mainnet", None, l2, "ETH_RPC_URL"),
        ("mainnet", l1, None, "RPC_ENDPOINT_POLYGON"),
        ("mainnet", l1, Some(" "), "RPC_ENDPOINT_POLYGON"),
        (
            "mainnet",
            l1,
            Some("http://l2.example/KEY"),
            "RPC_ENDPOINT_POLYGON",
        ),
    ] {
        let err = settings(false, network, l1, l2).unwrap_err();
        assert!(err.contains(needle), "{err}");
        assert!(err.contains("IGNORE_BLOCKCHAIN_ACCESS_CHECKS"), "{err}");
        assert!(!err.contains("KEY"), "never the value: {err}");
        assert!(
            settings(true, network, l1, l2).is_ok(),
            "ignored checks need no chain"
        );
    }
}

#[test]
fn a_refusal_keeps_its_kind_in_the_deploy_failure() {
    use super::super::write_deployer::DeployFailure;
    let rejected = into_deploy_failure(vec![
        ValidationError::Rejected("a".into()),
        ValidationError::Rejected("b".into()),
    ]);
    assert!(
        matches!(&rejected, DeployFailure::Rejected(_, e) if e == &["a".to_string(), "b".to_string()])
    );
    let unavailable = into_deploy_failure(vec![ValidationError::Unavailable("store".into())]);
    assert!(matches!(&unavailable, DeployFailure::Unavailable(e) if e == &["store".to_string()]));
    let unverifiable = into_deploy_failure(vec![ValidationError::Unverifiable("rpc".into())]);
    assert!(matches!(&unverifiable, DeployFailure::Unverifiable(e) if e == &["rpc".to_string()]));
}

/// `GET /queries/erc721/...` over the content database: the active entity
/// at the built URN, 400 for an unknown chain or a non-standard wearable,
/// 404 for none or a denylisted one.
#[tokio::test]
async fn the_erc721_route_formats_the_active_wearable() {
    require_test_db!(Some(pool) = crate::db::scratch::pool(2).await);
    let (storage, tmp) = temp_storage("erc721").await;
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let contract = format!("0x{}", &format!("{tag}{tag}")[..40]);
    let seed = |item: &str, metadata: Value| {
        let pool = pool.clone();
        let pointer = format!("urn:decentraland:matic:collections-v2:{contract}:{item}");
        let id = format!("bafkreierc721{tag}{item}");
        async move {
            let dep: i32 = sqlx::query_scalar(
                "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, \
                 entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
                 VALUES ('0xdeployer', 'v3', 'wearable', $1, $2::json, to_timestamp(1700000000), $3, now(), '[]'::json) RETURNING id",
            )
            .bind(&id)
            .bind(json!({ "v": metadata }))
            .bind(vec![pointer.clone()])
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO content.content_files (deployment, content_hash, key) VALUES ($1, 'bafkreithumb', 'thumbnail.png')")
                .bind(dep)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, 'wearable')")
                .bind(&pointer)
                .bind(&id)
                .execute(&pool)
                .await
                .unwrap();
            id
        }
    };
    let standard = seed(
        "0",
        json!({ "i18n": [{"code": "en", "text": "Hat"}], "rarity": "epic", "thumbnail": "thumbnail.png",
                "data": { "category": "hat", "tags": ["a"], "representations": [{ "bodyShapes": ["urn:decentraland:off-chain:base-avatars:BaseMale"] }] } }),
    )
    .await;
    let _plain = seed("1", json!({ "name": "no rarity" })).await;
    let _null_rarity = seed("3", json!({ "rarity": null })).await;
    let _empty_rarity = seed("4", json!({ "rarity": "" })).await;

    let state = Arc::new(crate::ContentState::new(
        pool.clone(),
        crate::Settings::for_tests(),
        storage,
    ));
    let app = Router::new()
        .route(
            "/queries/erc721/{chainId}/{contract}/{option}",
            get(erc721::get_erc721_entity),
        )
        .route(
            "/queries/erc721/{chainId}/{contract}/{option}/{emission}",
            get(erc721::get_erc721_entity),
        )
        .with_state(state.clone());
    let get_json = |path: String| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(
                    axum::http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status().as_u16();
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap();
            (status, serde_json::from_slice::<Value>(&body).unwrap())
        }
    };

    let (status, body) = get_json(format!("/queries/erc721/137/{contract}/0/12")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({
            "id": format!("urn:decentraland:matic:collections-v2:{contract}:0"),
            "name": "Hat",
            "description": "DCL Wearable 12/1000",
            "language": "en-US",
            "thumbnail": "http://localhost/content/contents/bafkreithumb",
            "attributes": [
                {"trait_type": "Rarity", "value": "epic"},
                {"trait_type": "Category", "value": "hat"},
                {"trait_type": "Tag", "value": "a"}
            ]
        })
    );
    assert_eq!(
        get_json(format!("/queries/erc721/137/{contract}/0"))
            .await
            .1["description"],
        ""
    );
    assert_eq!(
        get_json(format!("/queries/erc721/999/{contract}/0")).await,
        (400, json!({"error": "Invalid chainId '999'"}))
    );
    assert_eq!(
        get_json(format!("/queries/erc721/137/{contract}/1")).await,
        (400, json!({"error": "Wearable is not standard."}))
    );
    assert_eq!(
        get_json(format!("/queries/erc721/137/{contract}/2")).await,
        (404, json!({"error": "Entity does not exist"}))
    );
    // CS-17: a null or empty rarity is not standard, and the chain id is
    // upstream's `/^\d+$/` (no sign).
    for item in ["3", "4"] {
        assert_eq!(
            get_json(format!("/queries/erc721/137/{contract}/{item}")).await,
            (400, json!({"error": "Wearable is not standard."})),
            "{item}"
        );
    }
    assert_eq!(
        get_json(format!("/queries/erc721/+137/{contract}/0")).await,
        (400, json!({"error": "Invalid chainId '+137'"}))
    );
    state.denylist.add(&standard);
    assert_eq!(
        get_json(format!("/queries/erc721/137/{contract}/0")).await,
        (404, json!({"error": "Entity does not exist"}))
    );

    sqlx::query("DELETE FROM content.active_pointers WHERE entity_id LIKE $1")
        .bind(format!("bafkreierc721{tag}%"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.content_files WHERE deployment IN (SELECT id FROM content.deployments WHERE entity_id LIKE $1)")
        .bind(format!("bafkreierc721{tag}%"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM content.deployments WHERE entity_id LIKE $1")
        .bind(format!("bafkreierc721{tag}%"))
        .execute(&pool)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    pool.close().await;
}

#[tokio::test]
async fn staging_accepts_missing_content_that_full_validation_rejects() {
    let (storage, tmp) = temp_storage("staging-missing").await;
    let s = sources(
        lazy_pool(),
        storage,
        Settings {
            ignore_blockchain_access: true,
            ..Settings::default()
        },
    );
    let model = b"staged-model".to_vec();
    let (entity, mut files, chain, _) =
        signed(scene(&["1,1"], &model), std::slice::from_ref(&model)).await;
    files.remove(&hash_bytes_v1(&model));
    let ctx = Context {
        sources: &s,
        auth_chain: &chain,
    };
    assert_eq!(validate_staging(&entity, &files, &ctx, false).await, Ok(()));
    assert!(validate(&entity, &files, &ctx).await.is_err());
    let _ = std::fs::remove_dir_all(tmp);
}
