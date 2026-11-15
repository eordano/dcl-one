//! `GET /v1/contracts` and `GET /v1/collections` over the `collections`
//! mirror (`shop.collections`, the `collections-mirror` job; the old
//! routes read `squid_marketplace.collection`). Both answer the unavailable
//! envelope until the mirror has a complete pass for every configured
//! network. The L1 marketplace contracts (LAND, estates, names, the v1
//! wearable collections) are the static list the old crate carried.

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sqlx::Row;

use super::mirrors::{Mirror, require_ready};
use super::readers::{Ttl, cached};
use super::wire::{
    ApiError, ChainId, Contract, DataTotal, NETWORK_VALUES, NFT_CATEGORY_VALUES, Network,
    NftCategory, Params, clamp_first, clamp_skip, ethereum_chain_id, network_and_chain,
};
use super::{MarketplaceState, SharedState};

#[derive(Debug, Clone, Default)]
pub struct ContractFilters {
    pub first: Option<i64>,
    pub skip: Option<i64>,
    pub category: Option<NftCategory>,
    pub network: Option<Network>,
}

pub fn parse_contract_filters(pairs: &[(String, String)]) -> ContractFilters {
    let p = Params::new(pairs);
    ContractFilters {
        first: p.get_number("first", None).map(|f| f as i64),
        skip: p.get_number("skip", None).map(|f| f as i64),
        category: p
            .get_value("category", NFT_CATEGORY_VALUES, None)
            .and_then(|s| NftCategory::parse(&s)),
        network: p
            .get_value("network", NETWORK_VALUES, None)
            .and_then(|s| Network::parse(&s)),
    }
}

/// The Ethereum mainnet marketplace contracts the old `marketplace_contracts.rs` listed.
pub fn marketplace_contracts(chain_id: ChainId) -> Vec<Contract> {
    if chain_id != ChainId::EthereumMainnet {
        return Vec::new();
    }
    let mk = |name: &str, address: &str, category: NftCategory| Contract {
        name: name.to_string(),
        address: address.to_string(),
        category,
        network: Network::Ethereum,
        chain_id: ChainId::EthereumMainnet,
    };
    use NftCategory::*;
    vec![
        mk("LAND", crate::land_authz::LAND_REGISTRY_MAINNET, Parcel),
        mk(
            "Estates",
            crate::land_authz::ESTATE_REGISTRY_MAINNET,
            Estate,
        ),
        mk("Names", "0x2a187453064356c898cae034eaed119e1663acb8", Ens),
        mk(
            "Atari Launch",
            "0x4c290f486bae507719c562b6b524bdb71a2570c9",
            Wearable,
        ),
        mk(
            "Binance Us",
            "0xa8ee490e4c4da48cc1653502c1a77479d4d818de",
            Wearable,
        ),
        mk(
            "China Flying",
            "0x90958d4531258ca11d18396d4174a007edbc2b42",
            Wearable,
        ),
        mk(
            "Community Contest",
            "0x32b7495895264ac9d0b12d32afd435453458b1c6",
            Wearable,
        ),
        mk(
            "Cybermike CyberSoldier Set",
            "0x24d538a6265b006d4b53c45ba91af5ef60dca6cb",
            Wearable,
        ),
        mk(
            "CZ Mercenary MTZ",
            "0xc3ca6c364b854fd0a653a43f8344f8c22ddfdd26",
            Wearable,
        ),
        mk(
            "Dappcraft Moonminer",
            "0x1e1d4e6262787c8a8783a37fee698bd42aa42bec",
            Wearable,
        ),
        mk(
            "DCG",
            "0x3163d2cfee3183f9874e2869942cc62649eeb004",
            Wearable,
        ),
        mk(
            "DCL Launch",
            "0xd35147be6401dcb20811f2104c33de8e97ed6818",
            Wearable,
        ),
        mk(
            "DC Meta",
            "0xe7a64f6a239ed7f5bf18caa1ce2920d0c1278129",
            Wearable,
        ),
        mk(
            "DC Niftyblocksmith",
            "0x102daabd1e9d294d4436ec4c521dce7b1f15499e",
            Wearable,
        ),
        mk(
            "DG Fall 2020",
            "0x7038e9d2c6f5f84469a84cf9bc5f4909bb6ac5e0",
            Wearable,
        ),
        mk(
            "DG Summer",
            "0xbf53c33235cbfc22cef5a61a83484b86342679c5",
            Wearable,
        ),
        mk(
            "Dgtble Headspace",
            "0x574f64ac2e7215cba9752b85fc73030f35166bc0",
            Wearable,
        ),
        mk(
            "Digital Alchemy",
            "0x5cf39e64392c615fd8086838883958752a11b486",
            Wearable,
        ),
        mk(
            "Ethermon Wearables",
            "0x54266bcf2ffa841af934f003d144957d5934f3ab",
            Wearable,
        ),
        mk(
            "Exclusive Masks",
            "0xc04528c14c8ffd84c7c1fb6719b4a89853035cdd",
            Wearable,
        ),
        mk(
            "Dillon Francis Atari",
            "0x51e0b1afe5da0c038fc93a3fc8e11cf7a238b40b",
            Wearable,
        ),
        mk(
            "Halloween 2019",
            "0xc1f4b0eea2bd6690930e6c66efd3e197d620b9c2",
            Wearable,
        ),
        mk(
            "Halloween 2020",
            "0xfeb52cbf71b9adac957c6f948a6cf9980ac8c907",
            Wearable,
        ),
        mk(
            "MCH",
            "0xf64dc33a192e056bb5f0e5049356a0498b502d50",
            Wearable,
        ),
        mk(
            "Meme don't buy this",
            "0x1a57f6afc902d25792c53b8f19b7e17ef84222d5",
            Wearable,
        ),
        mk(
            "MF Sammichgamer",
            "0x30d3387ff3de2a21bef7032f82d00ff7739e403c",
            Wearable,
        ),
        mk(
            "ML Liondance",
            "0x0b1c6c75d511fae05e7dc696f4cf14129a9c43c9",
            Wearable,
        ),
        mk(
            "ML Pekingopera",
            "0x60d8271c501501c4b8cd9ed5343ac59d1b79d993",
            Wearable,
        ),
        mk(
            "Moonshot",
            "0x6a99abebb48819d2abe92c5e4dc4f48dc09a3ee8",
            Wearable,
        ),
        mk(
            "PM Dreamverse Eminence",
            "0x09305998a531fade369ebe30adf868c96a34e813",
            Wearable,
        ),
        mk(
            "PM Outtathisworld",
            "0x75a3752579dc2d63ca229eebbe3537fbabf85a12",
            Wearable,
        ),
        mk(
            "RAC Basics",
            "0x68e139552c4077ce5c9ab929c7e18ca721ffff00",
            Wearable,
        ),
        mk(
            "Release the Kraken",
            "0xffc5043d9a00865d089d5eefa5b3d1625aec6763",
            Wearable,
        ),
        mk(
            "RTFKT X Atari",
            "0x6b47e7066c7db71aa04a1d5872496fe05c4c331f",
            Wearable,
        ),
        mk(
            "Stay Safe",
            "0x201c3af8c471e5842428b74d1e7c0249adda2a92",
            Wearable,
        ),
        mk(
            "Sugarclub Yumi",
            "0xb5d14052d1e2bce2a2d7459d0379256e632b855d",
            Wearable,
        ),
        mk(
            "Tech Tribal Marc0matic",
            "0x480a0f4e360e8964e68858dd231c2922f1df45ef",
            Wearable,
        ),
        mk(
            "3LAUBasics",
            "0xe1ecb4e5130f493551c7d6df96ad19e5b431a0a9",
            Wearable,
        ),
        mk(
            "Winklevoss Capital",
            "0xc82a864a94db3550bc71fcb4ce07228bcec21f1a",
            Wearable,
        ),
        mk(
            "Wonderzone Meteorcharser",
            "0x34ed0aa248f60f54dd32fbc9883d6137a491f4f3",
            Wearable,
        ),
        mk(
            "Wonderzone Steampunk",
            "0xb96697fa4a3361ba35b774a42c58daccaad1b8e1",
            Wearable,
        ),
        mk(
            "WZ Wonderbot",
            "0x5df4602e7f38a91ea7724fc167f0c67f61604b1e",
            Wearable,
        ),
        mk(
            "Xmas2019",
            "0xc3af02c0fd486c8e9da5788b915d6fff3f049866",
            Wearable,
        ),
        mk(
            "Xmas2020",
            "0xecf073f91101ce5628669c487aee8f5822a101b1",
            Wearable,
        ),
        mk(
            "XmashUp",
            "0xdd9c7bc159dacb19c9f6b9d7e23948c87aa2397f",
            Wearable,
        ),
    ]
}

/// Every approved collection as a `Contract` (category wearable), name order
/// (`COLLATE "C"`, the old sort), memoized 15 min: the old route cached the
/// whole list for an hour and the mirror moves every 15 min anyway.
pub async fn all_collection_contracts(state: &MarketplaceState) -> Result<Vec<Contract>, ApiError> {
    let value = cached(state, Ttl::M15, "collections:contracts".into(), || async {
        let rows = sqlx::query(
            "SELECT contract_address, name, chain_id, network FROM shop.collections \
             WHERE is_approved ORDER BY name COLLATE \"C\" ASC, contract_address ASC",
        )
        .fetch_all(&state.pool)
        .await
        .map_err(|e| e.to_string())?;
        let out: Vec<Contract> = rows
            .iter()
            .map(|r| {
                let (network, chain_id) = network_and_chain(&r.get::<String, _>("network"));
                Contract {
                    name: r.get("name"),
                    address: r.get("contract_address"),
                    category: NftCategory::Wearable,
                    network,
                    chain_id,
                }
            })
            .collect();
        serde_json::to_value(out).map_err(|e| e.to_string())
    })
    .await
    .map_err(ApiError::internal)?;
    serde_json::from_value((*value).clone()).map_err(|e| ApiError::internal(e.to_string()))
}

pub async fn get_contracts(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let filters = parse_contract_filters(&pairs);
    let mut all = marketplace_contracts(ethereum_chain_id());
    if let Some(c) = filters.category {
        all.retain(|cn| cn.category == c);
    }
    if let Some(n) = filters.network {
        all.retain(|cn| cn.network == n);
    }
    let should_fetch_all = filters.category.is_none_or(|c| c == NftCategory::Wearable)
        && filters.network.is_none_or(|n| n == Network::Matic);
    if should_fetch_all {
        if let Err(envelope) = require_ready(&state, Mirror::Collections).await {
            return envelope;
        }
        match all_collection_contracts(&state).await {
            Ok(collections) => all.extend(collections),
            Err(e) => return e.into_response(),
        }
    }
    let total = all.len() as i64;
    let first = filters.first.unwrap_or(0);
    let skip = filters.skip.unwrap_or(0).max(0) as usize;
    let data: Vec<Contract> = if first <= 0 {
        all.into_iter().skip(skip).collect()
    } else {
        all.into_iter().skip(skip).take(first as usize).collect()
    };
    Json(DataTotal { data, total }).into_response()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionSortBy {
    Newest,
    RecentlyReviewed,
    Name,
    Size,
    RecentlyListed,
}

#[derive(Debug, Clone, Default)]
pub struct CollectionFilters {
    pub first: Option<i64>,
    pub skip: Option<i64>,
    pub sort_by: Option<CollectionSortBy>,
    pub name: Option<String>,
    pub search: Option<String>,
    pub creator: Option<String>,
    pub urn: Option<String>,
    pub contract_address: Option<String>,
    pub is_on_sale: bool,
    pub network: Option<Network>,
}

pub fn parse_collection_filters(pairs: &[(String, String)]) -> CollectionFilters {
    let p = Params::new(pairs);
    let sort_by = p
        .get_value(
            "sortBy",
            &[
                "newest",
                "recently_reviewed",
                "name",
                "size",
                "recently_listed",
            ],
            None,
        )
        .map(|s| match s.as_str() {
            "newest" => CollectionSortBy::Newest,
            "recently_reviewed" => CollectionSortBy::RecentlyReviewed,
            "size" => CollectionSortBy::Size,
            "recently_listed" => CollectionSortBy::RecentlyListed,
            _ => CollectionSortBy::Name,
        });
    CollectionFilters {
        first: p.get_number("first", None).map(|f| f as i64),
        skip: p.get_number("skip", None).map(|f| f as i64),
        sort_by,
        name: p.get_string("name", None),
        search: p.get_string("search", None),
        creator: p.get_address("creator", false, None),
        urn: p.get_string("urn", None),
        contract_address: p.get_address("contractAddress", false, None),
        is_on_sale: p.get_boolean("isOnSale"),
        network: p
            .get_value("network", NETWORK_VALUES, None)
            .and_then(|s| Network::parse(&s)),
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct Collection {
    pub urn: String,
    pub creator: String,
    pub name: String,
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    #[serde(rename = "createdAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub updated_at: i64,
    #[serde(rename = "reviewedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub reviewed_at: i64,
    #[serde(rename = "isOnSale")]
    pub is_on_sale: bool,
    pub size: i32,
    pub network: Network,
    #[serde(rename = "chainId")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
    #[serde(rename = "firstListedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub first_listed_at: Option<i64>,
}

/// The old mapper's rule: a value of ten digits or fewer is seconds and is scaled to milliseconds.
pub fn from_seconds_to_milliseconds(time: i64) -> i64 {
    if time.abs().to_string().len() <= 10 {
        time.saturating_mul(1000)
    } else {
        time
    }
}

pub async fn list_collections(
    state: &MarketplaceState,
    f: &CollectionFilters,
) -> Result<(Vec<Collection>, i64), ApiError> {
    let limit = clamp_first(f.first, 1000);
    let offset = clamp_skip(f.skip);
    let mut wheres: Vec<String> = vec!["is_approved = true".to_string()];
    let mut binds: Vec<String> = Vec::new();
    let next = |binds: &mut Vec<String>, v: String| {
        binds.push(v);
        format!("${}", binds.len())
    };
    if let Some(c) = &f.contract_address {
        let p = next(&mut binds, c.to_lowercase());
        wheres.push(format!("contract_address = {p}"));
    }
    if let Some(c) = &f.creator {
        let p = next(&mut binds, c.to_lowercase());
        wheres.push(format!("creator = {p}"));
    }
    if let Some(u) = &f.urn {
        let p = next(&mut binds, u.clone());
        wheres.push(format!("urn = {p}"));
    }
    if f.is_on_sale {
        wheres.push("search_is_store_minter = true".to_string());
    }
    if let Some(n) = &f.name {
        let p = next(&mut binds, n.clone());
        wheres.push(format!("name = {p}"));
    }
    if let Some(s) = &f.search {
        let p = next(&mut binds, format!("%{}%", s.trim().to_lowercase()));
        wheres.push(format!("COALESCE(search_text, lower(name)) LIKE {p}"));
    }
    if let Some(n) = f.network {
        let p = next(&mut binds, n.db_name().to_string());
        wheres.push(format!("network = {p}"));
    }
    if matches!(f.sort_by, Some(CollectionSortBy::RecentlyListed)) {
        wheres.push("first_listed_at IS NOT NULL".to_string());
    }
    let order = match f.sort_by {
        Some(CollectionSortBy::Newest) => "created_at DESC",
        Some(CollectionSortBy::RecentlyReviewed) => "reviewed_at DESC",
        Some(CollectionSortBy::Size) => "items_count DESC",
        Some(CollectionSortBy::RecentlyListed) => "first_listed_at DESC",
        Some(CollectionSortBy::Name) | None => "name COLLATE \"C\" ASC",
    };
    let where_sql = format!("WHERE {}", wheres.join(" AND "));
    let limit_p = binds.len() + 1;
    let offset_p = binds.len() + 2;
    let sql = format!(
        "SELECT contract_address, creator, name, urn, items_count, created_at, updated_at, reviewed_at, first_listed_at, \
                search_is_store_minter, network, COUNT(*) OVER() AS total \
         FROM shop.collections {where_sql} ORDER BY {order}, contract_address ASC LIMIT ${limit_p} OFFSET ${offset_p}"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.bind(limit).bind(offset).fetch_all(&state.pool).await?;
    let total = match rows.first() {
        Some(r) => r.get::<i64, _>("total"),
        None if offset > 0 => {
            let mut cq = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM shop.collections {where_sql}"
            )));
            for b in &binds {
                cq = cq.bind(b);
            }
            cq.fetch_one(&state.pool).await.unwrap_or(0)
        }
        None => 0,
    };
    let data = rows
        .iter()
        .map(|r| {
            let (network, chain_id) = network_and_chain(&r.get::<String, _>("network"));
            Collection {
                urn: r.get("urn"),
                creator: r.get("creator"),
                name: r.get("name"),
                contract_address: r.get("contract_address"),
                created_at: from_seconds_to_milliseconds(r.get("created_at")),
                updated_at: from_seconds_to_milliseconds(r.get("updated_at")),
                reviewed_at: from_seconds_to_milliseconds(r.get("reviewed_at")),
                is_on_sale: r.get("search_is_store_minter"),
                size: r.get("items_count"),
                network,
                chain_id,
                first_listed_at: r
                    .get::<Option<i64>, _>("first_listed_at")
                    .map(from_seconds_to_milliseconds),
            }
        })
        .collect();
    Ok((data, total))
}

pub async fn get_collections(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    if let Err(envelope) = require_ready(&state, Mirror::Collections).await {
        return envelope;
    }
    let filters = parse_collection_filters(&pairs);
    match list_collections(&state, &filters).await {
        Ok((data, total)) => Json(DataTotal { data, total }).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Whether `address` is a mirrored collection on any network: the economy
/// relay's contract allowlist probe (the old `squid.collection` read).
pub async fn is_collection_address(
    pool: &sqlx::PgPool,
    address: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shop.collections WHERE contract_address = $1)")
        .bind(address.to_lowercase())
        .fetch_one(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_list_and_the_filters_parse_as_before() {
        let all = marketplace_contracts(ChainId::EthereumMainnet);
        assert_eq!(all.len(), 45);
        assert_eq!(all[0].name, "LAND");
        assert_eq!(all[2].category, NftCategory::Ens);
        assert!(marketplace_contracts(ChainId::EthereumSepolia).is_empty());
        let f = parse_contract_filters(&[
            ("category".into(), "ens".into()),
            ("network".into(), "ETHEREUM".into()),
            ("first".into(), "2".into()),
        ]);
        assert_eq!(
            (f.category, f.network, f.first),
            (Some(NftCategory::Ens), Some(Network::Ethereum), Some(2))
        );
        let f = parse_collection_filters(&[
            ("sortBy".into(), "size".into()),
            (
                "creator".into(),
                "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            ),
            ("isOnSale".into(), "".into()),
        ]);
        assert_eq!(f.sort_by, Some(CollectionSortBy::Size));
        assert!(f.is_on_sale);
        assert_eq!(
            f.creator.as_deref(),
            Some("0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            "the collections filter keeps the caller's casing, the query lowercases"
        );
        assert_eq!(
            from_seconds_to_milliseconds(1_700_000_000),
            1_700_000_000_000
        );
        assert_eq!(
            from_seconds_to_milliseconds(1_700_000_000_000),
            1_700_000_000_000
        );
    }

    #[test]
    fn a_collection_serializes_camel_case_with_a_nullable_first_listed_at() {
        let c = Collection {
            urn: "urn:1".into(),
            creator: "0xa".into(),
            name: "Alpha".into(),
            contract_address: "0x1".into(),
            created_at: 100_000,
            updated_at: 100_000,
            reviewed_at: 100_000,
            is_on_sale: true,
            size: 3,
            network: Network::Matic,
            chain_id: ChainId::MaticMainnet,
            first_listed_at: None,
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["contractAddress"], "0x1");
        assert_eq!(v["isOnSale"], true);
        assert_eq!(v["network"], "MATIC");
        assert_eq!(v["chainId"], 137);
        assert_eq!(v["firstListedAt"], serde_json::Value::Null);
    }
}
