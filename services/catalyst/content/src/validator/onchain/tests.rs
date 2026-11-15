//! The checks against a chain on the loopback ([`fake_chain`]): each
//! granted and denied, the five-minute fallback, an RPC that cannot decide,
//! the block search; the encodings against the vendored ABI; and, ignored by
//! default, one LAND parcel and one approved item against a public RPC.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_primitives::{Address, U256, keccak256};
use alloy_sol_types::SolCall;

use super::*;
use crate::validator::fake_chain::{self, Query, Reply, at_deployment, five_min_before};

const OWNER: &str = "0xaaaa000000000000000000000000000000000001";
const STRANGER: &str = "0xcccc000000000000000000000000000000000003";
const TS: i64 = 1_700_000_000_123;

fn addr(s: &str) -> Address {
    s.parse().unwrap()
}

fn access(l1: Option<&str>, l2: Option<&str>) -> OnChainAccess {
    OnChainAccess::new(
        Outbound::for_tests(),
        "mainnet",
        l1.map(str::to_string),
        l2.map(str::to_string),
    )
}

#[test]
fn the_encodings_match_the_vendored_abi() {
    let abi: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("contracts/checker.abi.json")).unwrap();
    let signature = |name: &str| {
        let f = abi.iter().find(|f| f["name"] == name).unwrap();
        let inputs: Vec<&str> = f["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect();
        format!("{name}({})", inputs.join(","))
    };
    for (name, selector, ours) in [
        (
            "checkLAND",
            Checker::checkLANDCall::SELECTOR,
            Checker::checkLANDCall::SIGNATURE,
        ),
        (
            "checkName",
            Checker::checkNameCall::SELECTOR,
            Checker::checkNameCall::SIGNATURE,
        ),
        (
            "multicall",
            Checker::multicallCall::SELECTOR,
            Checker::multicallCall::SIGNATURE,
        ),
        (
            "validateThirdParty",
            Checker::validateThirdPartyCall::SELECTOR,
            Checker::validateThirdPartyCall::SIGNATURE,
        ),
        (
            "validateWearables",
            Checker::validateWearablesCall::SELECTOR,
            Checker::validateWearablesCall::SIGNATURE,
        ),
    ] {
        assert_eq!(signature(name), ours);
        assert_eq!(keccak256(ours.as_bytes())[..4], selector, "{name}");
    }
    let outputs: Vec<&str> = abi.iter().find(|f| f["name"] == "multicall").unwrap()["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["type"].as_str().unwrap())
        .collect();
    assert_eq!(outputs, ["bool[]"]);
    assert!(checker_bytecode().starts_with("0x6080"));
    for network in ["mainnet", "sepolia"] {
        assert!(CONTRACTS.l1.contains_key(network));
        assert!(CONTRACTS.l2.contains_key(l2_network(network).unwrap()));
    }
}

#[test]
fn the_bounds_reach_8_s_after_and_5_min_7_s_before() {
    assert_eq!(
        timestamp_bounds(1_700_000_000_000),
        (1_700_000_008, 1_699_999_701)
    );
    assert_eq!(
        timestamp_bounds(1_700_000_000_001),
        (1_700_000_009, 1_699_999_702)
    );
    assert_eq!(timestamp_bounds(0), (8, 0));
}

#[tokio::test]
async fn land_is_granted_per_parcel_at_the_deploy_block_or_five_minutes_before() {
    let at = at_deployment(TS);
    let before = five_min_before(TS);
    let chain = fake_chain::serve(move |q, block| match q {
        Query::Land {
            sender,
            land,
            estate,
            x,
            y,
        } => {
            assert_eq!(*land, CONTRACTS.l1["mainnet"].land.0);
            assert_eq!(*estate, CONTRACTS.l1["mainnet"].estate.0);
            assert!(block == at || block == before, "{block}");
            // (1,1) is the owner's now; (2,2) was until shortly before the deploy.
            Reply::Yes(
                *sender == addr(OWNER)
                    && ((*x, *y) == (1, 1) || ((*x, *y) == (2, 2) && block == before)),
            )
        }
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), None);
    assert_eq!(
        a.land(OWNER, &[(1, 1), (2, 2), (3, 3)], TS).await.unwrap(),
        vec![true, true, false]
    );
    let calls = chain.calls();
    assert!(calls.contains(&format!("eth_call@{at}")), "{calls:?}");
    assert!(calls.contains(&format!("eth_call@{before}")), "{calls:?}");
    chain.clear();
    assert_eq!(a.land(OWNER, &[(1, 1)], TS).await.unwrap(), vec![true]);
    assert_eq!(
        chain.calls(),
        vec![format!("eth_call@{at}")],
        "granted at the deploy block: no fallback, and the blocks are known"
    );
    assert_eq!(a.land(STRANGER, &[(1, 1)], TS).await.unwrap(), vec![false]);
    assert_eq!(
        a.land("not-an-address", &[(1, 1)], TS).await.unwrap(),
        vec![false]
    );
}

/// A large parcel list goes out as several Checker multicalls of at most
/// `CALLS_PER_BATCH` parcels, answered in order.
#[tokio::test]
async fn land_parcels_are_asked_in_bounded_multicalls() {
    let at = at_deployment(TS);
    let chain = fake_chain::serve(move |q, _| match q {
        Query::Land { sender, .. } => Reply::Yes(*sender == addr(OWNER)),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), None);
    a.l1().unwrap().chain.blocks_for(TS).await.unwrap();
    chain.clear();
    let parcels: Vec<(i32, i32)> = (0..250).map(|x| (x, 0)).collect();
    let granted = a.land(OWNER, &parcels, TS).await.unwrap();
    assert_eq!(granted, vec![true; 250], "every parcel answered");
    assert_eq!(
        chain.calls(),
        vec![format!("eth_call@{at}"); 3],
        "250 parcels in three multicalls"
    );
}

/// Repeated names are asked once; more distinct names than the cap refuse the
/// check without asking, and a synced check past its own cap is undecided.
#[tokio::test]
async fn names_are_deduplicated_and_capped() {
    let chain = fake_chain::serve(move |q, _| match q {
        Query::Name { .. } => Reply::Yes(true),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), None);
    a.l1().unwrap().chain.blocks_for(TS).await.unwrap();
    chain.clear();
    let repeated = vec!["alice".to_string(); 500];
    assert_eq!(
        a.names(OWNER, &repeated, TS).await.unwrap(),
        vec![true; 500]
    );
    assert_eq!(chain.calls().len(), 1, "one distinct name, one multicall");

    chain.clear();
    let many: Vec<String> = (0..=MAX_NAMES_ASKED).map(|i| format!("n{i}")).collect();
    let err = a.names(OWNER, &many, TS).await.unwrap_err();
    assert!(matches!(err, ValidatorError::TooManyItems(_)), "{err}");
    let err = a
        .names_capped(
            OWNER,
            &many,
            TS,
            ItemCaps {
                names: 10,
                ..SYNCED_ITEM_CAPS
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ValidatorError::BlockchainQuery(_)), "{err}");
    assert!(chain.calls().is_empty(), "nothing asked past a cap");

    let long = "x".repeat(MAX_NAME_BYTES + 1);
    assert_eq!(
        a.names(OWNER, &[long.clone(), long], TS).await.unwrap(),
        vec![false, false]
    );
    assert!(chain.calls().is_empty(), "an overlong name is not asked");

    let mixed = vec!["alice".to_string(), "y".repeat(MAX_NAME_BYTES + 1)];
    assert_eq!(a.names(OWNER, &mixed, TS).await.unwrap(), vec![true, false]);
    assert_eq!(chain.calls().len(), 1, "only the short name is asked");
}

#[tokio::test]
async fn names_are_asked_of_the_registrar() {
    let chain = fake_chain::serve(|q, _| match q {
        Query::Name {
            sender,
            registrar,
            name,
        } => {
            assert_eq!(*registrar, CONTRACTS.l1["mainnet"].registrar.0);
            Reply::Yes(*sender == addr(OWNER) && name == "alice")
        }
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), None);
    assert_eq!(
        a.names(OWNER, &["alice".into(), "bob".into()], TS)
            .await
            .unwrap(),
        vec![true, false]
    );
    assert!(a.names(OWNER, &[], TS).await.unwrap().is_empty());
}

#[tokio::test]
async fn collection_items_pass_the_hashes_and_the_contracts_active_at_the_block() {
    let at = at_deployment(TS);
    let collection = "0xc011000000000000000000000000000000000001";
    let chain = fake_chain::serve(move |q, block| match q {
        Query::Wearable {
            sender,
            factories,
            collection: c,
            item,
            hash,
            committees,
        } => {
            let l2 = &CONTRACTS.l2["polygon"];
            let active = |list: &[SinceBlock]| -> Vec<Address> {
                list.iter()
                    .filter(|s| block >= s.since_block)
                    .map(|s| s.address.0)
                    .collect()
            };
            assert_eq!(*factories, active(&l2.factories));
            assert_eq!(*committees, active(&l2.committees));
            assert_eq!(factories.len(), 2, "both factories are active by then");
            assert_eq!(*c, addr(collection));
            assert_eq!(*item, U256::from(3));
            // Only the CIDv1 and only at the deploy block.
            Reply::Yes(*sender == addr(OWNER) && hash == "bafkreinew" && block == at)
        }
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(None, Some(&chain.url));
    let hashes = ["QmOld".to_string(), "bafkreinew".to_string()];
    assert!(
        a.collection_item(OWNER, collection, "3", &hashes, TS)
            .await
            .unwrap()
    );
    assert!(
        !a.collection_item(STRANGER, collection, "3", &hashes, TS)
            .await
            .unwrap()
    );
    assert!(
        !a.collection_item(OWNER, collection, "3", &hashes[..1], TS)
            .await
            .unwrap()
    );
    assert!(
        !a.collection_item(OWNER, "community_contest", "3", &hashes, TS)
            .await
            .unwrap(),
        "a collection named without its address"
    );
}

#[tokio::test]
async fn a_third_party_root_is_asked_of_the_registry() {
    let root = [7u8; 32];
    let chain = fake_chain::serve(move |q, _| match q {
        Query::ThirdParty {
            registry,
            tp_id,
            root: r,
        } => {
            assert_eq!(*registry, CONTRACTS.l2["polygon"].third_party.0);
            Reply::Yes(
                tp_id == "urn:decentraland:matic:collections-thirdparty:woodies" && r.0 == root,
            )
        }
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(None, Some(&chain.url));
    let tp = "urn:decentraland:matic:collections-thirdparty:woodies";
    assert!(a.third_party_root(tp, root, TS).await.unwrap());
    assert!(!a.third_party_root(tp, [8u8; 32], TS).await.unwrap());
}

#[tokio::test]
async fn items_are_owned_by_token_and_third_party_nfts_by_their_standard() {
    let erc721 = "0x7210000000000000000000000000000000000001";
    let erc1155 = "0x1155000000000000000000000000000000000001";
    let not_nft = "0x0000000000000000000000000000000000000bad";
    let v2 = "0x0002000000000000000000000000000000000001";
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = probes.clone();
    let chain = fake_chain::serve(move |q, _| match q {
        Query::OwnerOf { contract, token }
            if *contract == addr(v2) || *contract == addr(erc721) =>
        {
            if token.is_zero() {
                seen.fetch_add(1, Ordering::SeqCst);
                return Reply::Revert;
            }
            Reply::Owner(if *token == U256::from(42) {
                addr(OWNER)
            } else {
                addr(STRANGER)
            })
        }
        Query::OwnerOf { .. } => Reply::Empty,
        Query::BalanceOf {
            contract,
            account,
            id,
        } => {
            if account == contract {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            if *contract == addr(erc1155) {
                Reply::Balance(U256::from(u8::from(
                    *account == addr(OWNER) && *id == U256::from(5),
                )))
            } else if *contract == addr(not_nft) {
                Reply::Empty
            } else {
                Reply::Revert
            }
        }
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), Some(&chain.url));
    let urns: Vec<String> = [
        format!("urn:decentraland:matic:collections-v2:{v2}:0:42"),
        format!("urn:decentraland:matic:collections-v2:{v2}:0:43"),
        format!("urn:decentraland:matic:collections-v2:{v2}:0"),
        format!("urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:{erc721}:42"),
        format!("urn:decentraland:matic:collections-thirdparty:tp:c:i:mainnet:{erc1155}:5"),
        format!("urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:{erc1155}:6"),
        "urn:decentraland:off-chain:base-avatars:eyes_00".to_string(),
    ]
    .to_vec();
    assert_eq!(
        a.items(OWNER, &urns, TS).await.unwrap(),
        vec![true, false, true, true, true, false, true]
    );
    let first = probes.load(Ordering::SeqCst);
    assert_eq!(first, 4, "one probe per contract and chain");
    a.items(OWNER, &urns, TS).await.unwrap();
    assert_eq!(
        probes.load(Ordering::SeqCst),
        first,
        "the standard is kept once known"
    );
    // A contract that is neither: its items are not owned.
    let unknown = vec![format!(
        "urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:{not_nft}:1"
    )];
    assert_eq!(a.items(OWNER, &unknown, TS).await.unwrap(), vec![false]);
}

#[tokio::test]
async fn case_variants_are_asked_and_malformed_collection_urns_are_never_owned() {
    let v2 = "0x0002000000000000000000000000000000000001";
    let chain = fake_chain::serve(move |q, _| match q {
        Query::OwnerOf { contract, token } if *contract == addr(v2) => {
            Reply::Owner(if *token == U256::from(42) {
                addr(OWNER)
            } else {
                addr(STRANGER)
            })
        }
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(None, Some(&chain.url));
    let mixed = vec![format!(
        "urn:decentraland:MATIC:Collections-V2:0X{}:0:42",
        &v2[2..]
    )];
    assert_eq!(a.items(OWNER, &mixed, TS).await.unwrap(), vec![true]);
    assert_eq!(a.items(STRANGER, &mixed, TS).await.unwrap(), vec![false]);
    chain.clear();
    let malformed = vec![
        format!("urn:decentraland:matic:collections-v2:{v2}:0:42:extra"),
        format!("urn:decentraland:foo:collections-v2:{v2}:0:42"),
    ];
    assert_eq!(
        a.items(OWNER, &malformed, TS).await.unwrap(),
        vec![false, false]
    );
    assert!(chain.calls().is_empty(), "{:?}", chain.calls());
}

#[tokio::test]
async fn names_must_all_pass_at_one_block() {
    let at = at_deployment(TS);
    let chain = fake_chain::serve(move |q, block| match q {
        // alice is held only at the deploy block, bob only five minutes before.
        Query::Name { name, .. } => Reply::Yes(match name.as_str() {
            "alice" => block == at,
            "bob" => block != at,
            _ => true,
        }),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), None);
    let owned = a
        .names(OWNER, &["alice".into(), "bob".into()], TS)
        .await
        .unwrap();
    assert!(!owned.iter().all(|o| *o), "{owned:?}");
    assert_eq!(
        a.names(OWNER, &["bob".into(), "carol".into()], TS)
            .await
            .unwrap(),
        vec![true, true],
        "all at the earlier block"
    );
}

#[tokio::test]
async fn items_must_all_pass_at_one_block_per_chain() {
    let at = at_deployment(TS);
    let l2_now = "0x0002000000000000000000000000000000000001";
    let l2_before = "0x0002000000000000000000000000000000000002";
    let l1_before = "0x0001000000000000000000000000000000000001";
    let chain = fake_chain::serve(move |q, block| match q {
        Query::OwnerOf { contract, .. } => {
            let held = if *contract == addr(l2_now) {
                block == at
            } else {
                block != at
            };
            Reply::Owner(if held { addr(OWNER) } else { addr(STRANGER) })
        }
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), Some(&chain.url));
    let urn =
        |network: &str, kind: &str, c: &str| format!("urn:decentraland:{network}:{kind}:{c}:0:1");
    let split = vec![
        urn("matic", "collections-v2", l2_now),
        urn("matic", "collections-v2", l2_before),
    ];
    let owned = a.items(OWNER, &split, TS).await.unwrap();
    assert!(!owned.iter().all(|o| *o), "{owned:?}");
    // Each chain at its own block: L2 at the deploy block, L1 five minutes before.
    let per_chain = vec![
        urn("matic", "collections-v2", l2_now),
        urn("sepolia", "collections-v2", l1_before),
    ];
    assert_eq!(
        a.items(OWNER, &per_chain, TS).await.unwrap(),
        vec![true, true]
    );
}

#[tokio::test]
async fn a_timestamp_ahead_of_the_chain_is_asked_at_the_head() {
    let chain = fake_chain::serve(|q, _| match q {
        Query::Name { sender, .. } => Reply::Yes(*sender == addr(OWNER)),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), None);
    let ahead = (chrono::Utc::now().timestamp() + 10 * 60) * 1000;
    let blocks = a.l1().unwrap().chain.blocks_for(ahead).await.unwrap();
    assert_eq!(blocks.at_deployment, blocks.five_min_before, "{blocks:?}");
    assert_eq!(
        a.names(STRANGER, &["alice".into()], ahead).await.unwrap(),
        vec![false],
        "a refusal, not an undecidable check"
    );
    assert_eq!(
        a.names(OWNER, &["alice".into()], ahead).await.unwrap(),
        vec![true]
    );
}

#[tokio::test]
async fn item_calls_go_out_in_one_batch_per_block() {
    let at = at_deployment(TS);
    let v2 = "0x0002000000000000000000000000000000000001";
    let chain = fake_chain::serve(move |q, block| match q {
        Query::OwnerOf { token, .. } => Reply::Owner(if *token < U256::from(10) || block != at {
            addr(OWNER)
        } else {
            addr(STRANGER)
        }),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(None, Some(&chain.url));
    let urns: Vec<String> = (0..20)
        .map(|t| format!("urn:decentraland:matic:collections-v2:{v2}:0:{t}"))
        .collect();
    a.l2().unwrap().chain.blocks_for(TS).await.unwrap();
    chain.clear();
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![true; 20]);
    assert_eq!(chain.requests(), 2, "one batch at each block");
    assert_eq!(chain.calls().len(), 40);

    chain.set_no_batch(true);
    chain.clear();
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![true; 20]);
    assert_eq!(
        chain.calls().len(),
        40,
        "an endpoint without batches: one by one"
    );
}

#[tokio::test]
async fn items_past_the_cap_refuse_the_check_without_asking() {
    let v2 = "0x0002000000000000000000000000000000000001";
    let chain = fake_chain::serve(|q, _| match q {
        Query::OwnerOf { .. } => Reply::Owner(addr(OWNER)),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(None, Some(&chain.url));
    let mut urns: Vec<String> = (0..MAX_ITEMS_ASKED)
        .map(|t| format!("urn:decentraland:matic:collections-v2:{v2}:0:{t}"))
        .collect();
    // A repeated URN counts once.
    urns.insert(1, urns[0].clone());
    let owned = a.items(OWNER, &urns, TS).await.unwrap();
    assert_eq!(owned, vec![true; MAX_ITEMS_ASKED + 1]);
    chain.clear();
    urns.push(format!(
        "urn:decentraland:matic:collections-v2:{v2}:0:{MAX_ITEMS_ASKED}"
    ));
    let err = a.items(OWNER, &urns, TS).await.unwrap_err();
    assert!(matches!(err, ValidatorError::TooManyItems(_)), "{err}");
    assert_eq!(chain.requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_past_its_deadline_is_undecided() {
    let chain = fake_chain::serve(|_, _| {
        std::thread::sleep(std::time::Duration::from_millis(500));
        Reply::Yes(true)
    })
    .await;
    let a = access(Some(&chain.url), None).with_deadline(std::time::Duration::from_millis(100));
    let err = a.land(OWNER, &[(1, 1)], TS).await.unwrap_err();
    assert!(
        matches!(&err, ValidatorError::BlockchainQuery(m) if m.contains("did not finish")),
        "{err}"
    );
}

#[tokio::test]
async fn a_contract_of_no_known_standard_is_probed_again_only_after_a_while() {
    let not_nft = "0x0000000000000000000000000000000000000bad";
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = probes.clone();
    let chain = fake_chain::serve(move |q, _| {
        seen.fetch_add(1, Ordering::SeqCst);
        match q {
            Query::OwnerOf { .. } | Query::BalanceOf { .. } => Reply::Empty,
            other => panic!("{other:?}"),
        }
    })
    .await;
    let a = access(None, Some(&chain.url));
    let urns = vec![format!(
        "urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:{not_nft}:1"
    )];
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![false]);
    let first = probes.load(Ordering::SeqCst);
    assert_eq!(first, 2, "balanceOf, then ownerOf");
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![false]);
    assert_eq!(probes.load(Ordering::SeqCst), first, "kept for a while");
    for (_, at) in a.nft_kinds.lock().values_mut() {
        *at = Instant::now() - UNKNOWN_KIND_TTL;
    }
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![false]);
    assert_eq!(probes.load(Ordering::SeqCst), 2 * first);
}

#[tokio::test]
async fn the_standards_are_probed_in_one_batch_each() {
    let chain = fake_chain::serve(|q, _| match q {
        Query::BalanceOf { contract, .. } if contract.as_slice()[19] % 2 == 0 => {
            Reply::Balance(U256::ZERO)
        }
        Query::BalanceOf { .. } => Reply::Revert,
        Query::OwnerOf { token, .. } if token.is_zero() => Reply::Revert,
        Query::OwnerOf { .. } => Reply::Owner(addr(OWNER)),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(None, Some(&chain.url));
    let urns: Vec<String> = (1..=10u8)
        .map(|c| format!("urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:0x{c:040x}:1"))
        .collect();
    a.l2().unwrap().chain.blocks_for(TS).await.unwrap();
    chain.clear();
    let owned = a.items(OWNER, &urns, TS).await.unwrap();
    // The ERC-721s answer ownerOf; the ERC-1155s hold a zero balance.
    assert_eq!(owned, (1..=10u8).map(|c| c % 2 == 1).collect::<Vec<_>>());
    // balanceOf probes, ownerOf probes of the rest, then the items at each block.
    assert_eq!(chain.requests(), 2 + 2);
}

#[tokio::test]
async fn items_of_contracts_past_the_cap_refuse_the_check_without_probing() {
    let chain = fake_chain::serve(|q, _| match q {
        Query::BalanceOf { .. } => Reply::Revert,
        Query::OwnerOf { .. } => Reply::Owner(addr(OWNER)),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(None, Some(&chain.url));
    let tp = |c: usize| {
        format!("urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:0x{c:040x}:1")
    };
    let mut urns: Vec<String> = (1..=MAX_NFT_CONTRACTS_ASKED).map(tp).collect();
    assert_eq!(
        a.items(OWNER, &urns, TS).await.unwrap(),
        vec![true; MAX_NFT_CONTRACTS_ASKED]
    );
    chain.clear();
    urns.push(tp(MAX_NFT_CONTRACTS_ASKED + 1));
    let err = a.items(OWNER, &urns, TS).await.unwrap_err();
    assert!(matches!(err, ValidatorError::TooManyItems(_)), "{err}");
    assert_eq!(chain.requests(), 0);
}

#[tokio::test]
async fn a_contract_without_code_is_not_remembered_as_of_no_standard() {
    let not_yet = "0x0000000000000000000000000000000000000bad";
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = probes.clone();
    let chain = fake_chain::serve(move |q, _| {
        seen.fetch_add(1, Ordering::SeqCst);
        match q {
            Query::OwnerOf { .. } | Query::BalanceOf { .. } => Reply::Empty,
            other => panic!("{other:?}"),
        }
    })
    .await;
    chain.set_no_code(true);
    let a = access(None, Some(&chain.url));
    let urns = vec![format!(
        "urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:{not_yet}:1"
    )];
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![false]);
    assert_eq!(probes.load(Ordering::SeqCst), 2);
    assert!(chain.calls().iter().any(|c| c == "eth_getCode"));
    // Probed again on the next check: nothing was kept.
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![false]);
    assert_eq!(probes.load(Ordering::SeqCst), 4);
}

/// An endpoint without batches still answers `eth_getCode` one address at a
/// time, so a deployed contract of no known standard is remembered.
#[tokio::test]
async fn code_is_asked_address_by_address_when_batches_are_refused() {
    let not_nft = "0x0000000000000000000000000000000000000bad";
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = probes.clone();
    let chain = fake_chain::serve(move |q, _| {
        seen.fetch_add(1, Ordering::SeqCst);
        match q {
            Query::OwnerOf { .. } | Query::BalanceOf { .. } => Reply::Empty,
            other => panic!("{other:?}"),
        }
    })
    .await;
    chain.set_no_batch(true);
    let a = access(None, Some(&chain.url));
    let urns = vec![format!(
        "urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:{not_nft}:1"
    )];
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![false]);
    assert_eq!(probes.load(Ordering::SeqCst), 2);
    assert!(chain.calls().iter().any(|c| c == "eth_getCode"));
    assert_eq!(a.items(OWNER, &urns, TS).await.unwrap(), vec![false]);
    assert_eq!(probes.load(Ordering::SeqCst), 2, "the code was seen: kept");
}

#[tokio::test]
async fn synced_caps_ask_past_the_local_ones_and_beyond_their_own_are_undecided() {
    let v2 = "0x0002000000000000000000000000000000000001";
    let chain = fake_chain::serve(|q, _| match q {
        Query::BalanceOf { .. } => Reply::Revert,
        Query::OwnerOf { .. } => Reply::Owner(addr(OWNER)),
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(None, Some(&chain.url));
    let items = |n: usize| -> Vec<String> {
        (0..n)
            .map(|t| format!("urn:decentraland:matic:collections-v2:{v2}:0:{t}"))
            .collect()
    };
    let third_party = |n: usize| -> Vec<String> {
        (1..=n)
            .map(|c| {
                format!("urn:decentraland:matic:collections-thirdparty:tp:c:i:matic:0x{c:040x}:1")
            })
            .collect()
    };

    let urns = items(MAX_ITEMS_ASKED + 3);
    let owned = a
        .items_capped(OWNER, &urns, TS, SYNCED_ITEM_CAPS)
        .await
        .unwrap();
    assert_eq!(owned, vec![true; MAX_ITEMS_ASKED + 3]);
    let urns = third_party(MAX_NFT_CONTRACTS_ASKED + 2);
    let owned = a
        .items_capped(OWNER, &urns, TS, SYNCED_ITEM_CAPS)
        .await
        .unwrap();
    assert_eq!(owned, vec![true; MAX_NFT_CONTRACTS_ASKED + 2]);

    chain.clear();
    for urns in [
        items(SYNCED_ITEM_CAPS.items + 1),
        third_party(SYNCED_ITEM_CAPS.nft_contracts + 1),
    ] {
        let err = a
            .items_capped(OWNER, &urns, TS, SYNCED_ITEM_CAPS)
            .await
            .unwrap_err();
        assert!(matches!(err, ValidatorError::BlockchainQuery(_)), "{err}");
    }
    assert!(chain.calls().is_empty(), "nothing asked past the cap");
}

#[tokio::test]
async fn the_rpcs_must_answer_for_the_configured_network() {
    let chain = fake_chain::serve(|_, _| Reply::Yes(true)).await;
    assert!(
        access(Some(&chain.url), None)
            .verify_chain_ids()
            .await
            .is_ok()
    );
    let err = access(None, Some(&chain.url))
        .verify_chain_ids()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("L2") && err.contains("137"), "{err}");
    chain.set_chain_id(137);
    assert!(
        access(None, Some(&chain.url))
            .verify_chain_ids()
            .await
            .is_ok()
    );
    chain.set_down(true);
    assert!(
        access(Some(&chain.url), None)
            .verify_chain_ids()
            .await
            .is_ok(),
        "an unreachable RPC is logged, not refused"
    );
}

#[tokio::test]
async fn a_check_the_rpc_cannot_answer_is_an_error_never_a_grant() {
    let at = at_deployment(TS);
    let chain = fake_chain::serve(move |q, block| match q {
        // The deploy block grants nothing and the earlier one is pruned.
        Query::Land { .. } if block == at => Reply::Yes(false),
        Query::Land { .. } => Reply::Fail,
        Query::Name { .. } => Reply::Revert,
        other => panic!("{other:?}"),
    })
    .await;
    let a = access(Some(&chain.url), None);
    let err = a.land(OWNER, &[(1, 1)], TS).await.unwrap_err().to_string();
    assert!(err.contains("missing trie node"), "{err}");
    assert_eq!(
        a.names(OWNER, &["alice".into()], TS).await.unwrap(),
        vec![false],
        "a revert is the chain's no"
    );

    chain.set_down(true);
    let fresh = access(Some(&chain.url), None);
    let err = fresh
        .land(OWNER, &[(1, 1)], TS)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("block lookup"), "{err}");
    assert!(
        !err.contains("127.0.0.1"),
        "the endpoint is never named: {err}"
    );

    for (a, needle) in [
        (access(None, None), "ETH_RPC_URL"),
        (
            OnChainAccess::new(
                Outbound::for_tests(),
                "goerli",
                Some(chain.url.clone()),
                None,
            ),
            "ETH_NETWORK goerli",
        ),
    ] {
        let err = a.land(OWNER, &[(1, 1)], TS).await.unwrap_err().to_string();
        assert!(err.contains(needle), "{err}");
    }
    let err = access(Some(&chain.url), None)
        .collection_item(OWNER, OWNER, "0", &[], TS)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("RPC_ENDPOINT_POLYGON"), "{err}");
}

#[tokio::test]
async fn the_deploy_blocks_come_from_a_search_that_remembers() {
    let chain = fake_chain::serve(|_, _| Reply::Yes(true)).await;
    let a = access(Some(&chain.url), None);
    let l1 = a.l1().unwrap();
    let blocks = l1.chain.blocks_for(TS).await.unwrap();
    assert_eq!(
        blocks,
        Blocks {
            at_deployment: Some(at_deployment(TS)),
            five_min_before: Some(five_min_before(TS)),
        }
    );
    let cold = chain.calls().len();
    assert!(cold < 70, "two logarithmic searches: {cold}");
    chain.clear();
    assert_eq!(l1.chain.blocks_for(TS).await.unwrap(), blocks);
    assert!(chain.calls().is_empty(), "{:?}", chain.calls());
    assert_eq!(
        l1.chain.blocks_for(GENESIS_MS_BEFORE).await.unwrap(),
        Blocks {
            at_deployment: None,
            five_min_before: None
        },
        "before the first block"
    );
}

const GENESIS_MS_BEFORE: i64 = (fake_chain::GENESIS - 3600) * 1000;

/// Read-only calls to a public archive RPC (`CHECKER_LIVE_L1_RPC` and
/// `CHECKER_LIVE_L2_RPC` override the defaults), at fixed past timestamps so
/// the answers do not move: a parcel owned directly, one in an estate, and
/// an item of an approved collection deployed by a committee member with the
/// content its on-chain hash pins.
#[tokio::test]
#[ignore = "reads a public RPC"]
async fn live_land_and_approved_item_on_mainnet() {
    let l1 = std::env::var("CHECKER_LIVE_L1_RPC")
        .unwrap_or_else(|_| "https://rpc.decentraland.org/mainnet".into());
    let l2 = std::env::var("CHECKER_LIVE_L2_RPC")
        .unwrap_or_else(|_| "https://rpc.decentraland.org/polygon".into());
    let http = Outbound::new(Default::default()).unwrap();
    let a = OnChainAccess::new(http, "mainnet", Some(l1), Some(l2));

    // Mainnet block 26336000.
    let at = 1_793_729_663_000;
    let direct = "0x5127bc7594839253bd204246b17a5a6ef60da540";
    let estate = "0x5188e308fee25ac49c10f9fd9270d953c4822ce5";
    assert_eq!(
        a.land(direct, &[(-30, -120), (0, 0)], at).await.unwrap(),
        vec![true, false]
    );
    assert_eq!(
        a.land(estate, &[(-30, -120), (0, 0)], at).await.unwrap(),
        vec![false, true],
        "the owner of the estate holding (0,0)"
    );
    assert_eq!(a.land(STRANGER, &[(0, 0)], at).await.unwrap(), vec![false]);

    let collection = "0xf18f33c4db6689595f25ea4af6dd8df3e3abf18b";
    let committee = "0x1dec5f50cb1467f505bb3ddfd408805114406b10";
    let creator = "0xfe2d424af0df49bb3316cb2e9f574b0d09cf98ad";
    let pinned = ["bafkreiewhubsoukrvwic5kn7o3ga3wdqxpd7neh7jyz3w3tgdmor6djr7a".to_string()];
    let deployed = 1_793_717_744_232;
    assert!(
        a.collection_item(committee, collection, "0", &pinned, deployed)
            .await
            .unwrap(),
        "the committee member deploying the pinned content"
    );
    assert!(
        !a.collection_item(
            committee,
            collection,
            "0",
            &["bafkreiother".to_string()],
            deployed
        )
        .await
        .unwrap(),
        "other content"
    );
    assert!(
        !a.collection_item(creator, collection, "0", &pinned, deployed)
            .await
            .unwrap(),
        "the creator of an approved collection"
    );
}
