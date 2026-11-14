//! The access step per entity type (scene LAND rights, profile/outfits
//! pointer ownership, names and items, store pointers, collection and
//! third-party items) over a [`BlockchainChecker`]. A checker `Err` means
//! the source could not answer: it is `Unavailable` with a fixed sentence
//! ([`undecided`]), never a rejection, and its cause is only logged.

use async_trait::async_trait;

use super::error::{PermissionResult, ValidationResponse, ValidatorError};
use super::types::*;

/// Upstream's answer when the scene access check cannot run
/// (`@dcl/content-validator` `access/on-chain/scenes.ts`, `b1893d37`).
pub(crate) const PARCEL_ACCESS_UNDECIDED: &str = "Cannot validate deployment";

/// A source that could not answer: `Unavailable` with `answer`. The cause
/// (RPC, subgraph or configuration detail) is logged, never answered.
/// Upstream folds these failures into an ownership refusal and has no
/// sentence of its own except for scenes ([`PARCEL_ACCESS_UNDECIDED`]).
fn undecided(check: &'static str, answer: &'static str, e: &ValidatorError) -> ValidationResponse {
    tracing::warn!(check, error = %e, "access check could not be answered");
    ValidationResponse::unavailable(answer)
}

/// What the access step reads: [`super::Sources`] is the live implementation
/// (`chain.rs`: the Checker contract and the item contracts at the entity
/// timestamp's block); the tests stub it.
#[async_trait]
pub trait BlockchainChecker: Send + Sync {
    async fn check_land_access(
        &self,
        eth_address: &str,
        parcels: &[(i32, i32)],
        timestamp: Timestamp,
    ) -> Result<Vec<bool>, ValidatorError>;

    async fn check_names_ownership(
        &self,
        eth_address: &str,
        names: &[String],
        timestamp: Timestamp,
    ) -> Result<PermissionResult, ValidatorError>;

    async fn check_items_ownership(
        &self,
        eth_address: &str,
        urns: &[String],
        timestamp: Timestamp,
    ) -> Result<PermissionResult, ValidatorError>;

    async fn check_collection_access(
        &self,
        eth_address: &str,
        contract_address: &str,
        item_id: &str,
        deployment: &Deployment<'_>,
        timestamp: Timestamp,
        layer: BlockchainLayer,
    ) -> Result<bool, ValidatorError>;

    async fn check_third_party_access(
        &self,
        asset_urn: &str,
        entity: &Entity,
        deployment: &Deployment<'_>,
        timestamp: Timestamp,
    ) -> Result<bool, ValidatorError>;

    fn is_address_owned_by_decentraland(&self, address: &str) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockchainLayer {
    L1,
    L2,
}

pub async fn validate_scene_access(
    deployment: &Deployment<'_>,
    checker: &dyn BlockchainChecker,
    deployer_address: &str,
) -> ValidationResponse {
    let entity = &deployment.entity;
    let pointers = &entity.pointers;
    let timestamp = entity.timestamp;

    let mut errors = Vec::new();

    let mut parcels: Vec<(i32, i32)> = Vec::new();
    for pointer in pointers {
        let parsed = crate::auth::parse_pointer(pointer)
            .and_then(|(x, y)| Some((i32::try_from(x).ok()?, i32::try_from(y).ok()?)));
        match parsed {
            Some(xy) => parcels.push(xy),
            None => {
                return ValidationResponse::fail(format!(
                    "Scene pointers should only contain two integers separated by a comma, \
                     for example (10,10) or (120,-45). Invalid pointer: {pointer}"
                ));
            }
        }
    }

    match checker
        .check_land_access(deployer_address, &parcels, timestamp)
        .await
    {
        Ok(results) => {
            for (i, has_access) in results.iter().enumerate() {
                if !*has_access {
                    let (x, y) = parcels[i];
                    errors.push(format!(
                        "The provided Eth Address does not have access to the \
                         following parcel: ({x},{y})"
                    ));
                }
            }
        }
        Err(e) => {
            return undecided("parcel access", PARCEL_ACCESS_UNDECIDED, &e);
        }
    }

    ValidationResponse::from_errors(errors)
}

pub async fn validate_profile_access(
    deployment: &Deployment<'_>,
    checker: &dyn BlockchainChecker,
    deployer_address: &str,
) -> ValidationResponse {
    let entity = &deployment.entity;
    let pointers = &entity.pointers;

    if pointers.len() != 1 {
        return ValidationResponse::fail(format!(
            "Only one pointer is allowed when you create a Profile. Received: {pointers:?}"
        ));
    }

    let pointer = pointers[0].to_lowercase();

    if pointer.starts_with("default") {
        if !checker.is_address_owned_by_decentraland(deployer_address) {
            return ValidationResponse::fail(
                "Only Decentraland can add or modify default profiles".to_string(),
            );
        }
        return ValidationResponse::Ok;
    }

    if !crate::auth::is_eth_address(&pointer) {
        return ValidationResponse::fail(
            "The given pointer is not a valid ethereum address.".to_string(),
        );
    }

    if pointer != deployer_address.to_lowercase() {
        return ValidationResponse::fail(format!(
            "You can only alter your own profile. The pointer address and the signer address \
             are different (pointer:{pointer} signer: {})",
            deployer_address.to_lowercase()
        ));
    }

    let identity =
        validate_profile_identity_fields(entity.metadata.as_ref(), entity.timestamp, &pointer);
    if !identity.is_ok() {
        return identity;
    }

    if entity.timestamp >= adr_timestamps::ADR_75
        && let Some(metadata) = &entity.metadata
    {
        let names = extract_claimed_names(metadata);
        if !names.is_empty() {
            match checker
                .check_names_ownership(deployer_address, &names, entity.timestamp)
                .await
            {
                Ok(result) => {
                    if !result.result {
                        let failing = result
                            .failing
                            .as_deref()
                            .map(|f| f.join(", "))
                            .unwrap_or_default();
                        return ValidationResponse::fail(format!(
                            "The following names ({failing}) are not owned by \
                             the address {}.",
                            deployer_address.to_lowercase()
                        ));
                    }
                }
                Err(e @ ValidatorError::TooManyItems(_)) => {
                    return ValidationResponse::fail(format!("Error checking name ownership: {e}"));
                }
                Err(e) => {
                    return undecided("name ownership", "Error checking name ownership.", &e);
                }
            }
        }
    }

    if let Some(metadata) = &entity.metadata {
        let item_urns = extract_profile_item_urns(metadata, entity.timestamp);
        if !item_urns.is_empty() {
            match checker
                .check_items_ownership(deployer_address, &item_urns, entity.timestamp)
                .await
            {
                Ok(result) => {
                    if !result.result {
                        let failing = result
                            .failing
                            .as_deref()
                            .map(|f| f.join(", "))
                            .unwrap_or_default();
                        return ValidationResponse::fail(format!(
                            "The following items ({failing}) are not owned by \
                             the address {}.",
                            deployer_address.to_lowercase()
                        ));
                    }
                }
                Err(e @ ValidatorError::TooManyItems(_)) => {
                    return ValidationResponse::fail(format!("Error checking item ownership: {e}"));
                }
                Err(e) => {
                    return undecided("item ownership", "Error checking item ownership.", &e);
                }
            }
        }
    }

    ValidationResponse::Ok
}

pub fn validate_local_pointer_ownership(
    deployment: &Deployment<'_>,
    deployer_address: &str,
) -> ValidationResponse {
    let entity = &deployment.entity;
    let deployer = deployer_address.to_lowercase();
    match entity.entity_type {
        EntityType::Profile => {
            if let Some(p) = entity.pointers.first() {
                let p = p.to_lowercase();
                if !p.starts_with("default") && crate::auth::is_eth_address(&p) && p != deployer {
                    return ValidationResponse::fail(format!(
                        "You can only alter your own profile. The pointer address and the \
                         signer address are different (pointer:{p} signer: {deployer})"
                    ));
                }
            }
            ValidationResponse::Ok
        }
        EntityType::Store => validate_store_access(deployment, deployer_address),
        EntityType::Outfits => {
            if let Some(p) = entity.pointers.first() {
                let p = p.to_lowercase();
                let addr = p.split(':').next().unwrap_or("");
                if crate::auth::is_eth_address(addr) && addr != deployer {
                    return ValidationResponse::fail(format!(
                        "You can only alter your own outfits. The address of the pointer and \
                         the signer address are different (pointer:{p} signer: {deployer})."
                    ));
                }
            }
            ValidationResponse::Ok
        }
        _ => ValidationResponse::Ok,
    }
}

pub fn validate_store_access(
    deployment: &Deployment<'_>,
    deployer_address: &str,
) -> ValidationResponse {
    let pointers = &deployment.entity.pointers;

    if pointers.len() != 1 {
        return ValidationResponse::fail(format!(
            "Only one pointer is allowed when you create a Store. Received: {pointers:?}"
        ));
    }

    let pointer = pointers[0].to_lowercase();

    if !pointer.starts_with("urn:decentraland:off-chain:marketplace-stores:") {
        return ValidationResponse::fail(format!(
            "Store pointers should be a urn, for example \
             (urn:decentraland:off-chain:marketplace-stores:{{address}}). \
             Invalid pointer: {pointer}"
        ));
    }

    let address_part = pointer
        .strip_prefix("urn:decentraland:off-chain:marketplace-stores:")
        .unwrap_or("");

    if address_part.to_lowercase() != deployer_address.to_lowercase() {
        return ValidationResponse::fail(format!(
            "You can only alter your own store. The pointer address and the signer address \
             are different (address:{} signer: {}).",
            address_part.to_lowercase(),
            deployer_address.to_lowercase()
        ));
    }

    ValidationResponse::Ok
}

pub async fn validate_outfits_access(
    deployment: &Deployment<'_>,
    checker: &dyn BlockchainChecker,
    deployer_address: &str,
) -> ValidationResponse {
    let entity = &deployment.entity;
    let pointers = &entity.pointers;

    if pointers.len() != 1 {
        return ValidationResponse::fail(format!(
            "Only one pointer is allowed when you create an Outfits. Received: {pointers:?}"
        ));
    }

    let pointer = pointers[0].to_lowercase();
    let parts: Vec<&str> = pointer.split(':').collect();

    if parts.len() != 2 {
        return ValidationResponse::fail(
            "The pointer is not valid. It should be in the format: <address>:outfits".to_string(),
        );
    }

    if parts[1] != "outfits" {
        return ValidationResponse::fail(
            "The pointer is not valid. It should be in the format: <address>:outfits".to_string(),
        );
    }

    let pointer_address = parts[0];
    if !crate::auth::is_eth_address(pointer_address) {
        return ValidationResponse::fail(
            "The address of the given pointer is not a valid ethereum address.".to_string(),
        );
    }

    if pointer_address != deployer_address.to_lowercase() {
        return ValidationResponse::fail(format!(
            "You can only alter your own outfits. The address of the pointer and the signer \
             address are different (pointer:{pointer} signer: {}).",
            deployer_address.to_lowercase()
        ));
    }

    if let Some(metadata) = &entity.metadata {
        let wearable_urns = extract_outfit_wearable_urns(metadata);
        if !wearable_urns.is_empty() {
            match checker
                .check_items_ownership(deployer_address, &wearable_urns, entity.timestamp)
                .await
            {
                Ok(result) => {
                    if !result.result {
                        let failing = result
                            .failing
                            .as_deref()
                            .map(|f| f.join(", "))
                            .unwrap_or_default();
                        return ValidationResponse::fail(format!(
                            "The following wearables ({failing}) are not owned by \
                             the address {}.",
                            deployer_address.to_lowercase()
                        ));
                    }
                }
                Err(e @ ValidatorError::TooManyItems(_)) => {
                    return ValidationResponse::fail(format!(
                        "Error checking wearable ownership: {e}"
                    ));
                }
                Err(e) => {
                    return undecided(
                        "wearable ownership",
                        "Error checking wearable ownership.",
                        &e,
                    );
                }
            }
        }

        let names = extract_outfit_names_for_extra_slots(metadata);
        if !names.is_empty() {
            match checker
                .check_names_ownership(deployer_address, &names, entity.timestamp)
                .await
            {
                Ok(result) => {
                    if !result.result {
                        let failing = result
                            .failing
                            .as_deref()
                            .map(|f| f.join(", "))
                            .unwrap_or_default();
                        return ValidationResponse::fail(format!(
                            "The following names ({failing}) are not owned by \
                             the address {}.",
                            deployer_address.to_lowercase()
                        ));
                    }
                }
                Err(e @ ValidatorError::TooManyItems(_)) => {
                    return ValidationResponse::fail(format!("Error checking name ownership: {e}"));
                }
                Err(e) => {
                    return undecided("name ownership", "Error checking name ownership.", &e);
                }
            }
        }
    }

    ValidationResponse::Ok
}

pub async fn validate_item_access(
    deployment: &Deployment<'_>,
    checker: &dyn BlockchainChecker,
    deployer_address: &str,
) -> ValidationResponse {
    let entity = &deployment.entity;
    let pointers = &entity.pointers;

    let valid_types: &[ItemAssetType] = if entity.entity_type == EntityType::Emote {
        EMOTE_ASSET_TYPES
    } else {
        WEARABLE_ASSET_TYPES
    };
    // Compare against the first key only, so the scan stays linear.
    let mut first: Option<(String, ItemAssetType)> = None;
    let mut multiple = false;
    for pointer in pointers {
        let Some(asset_type) = item_asset_urn_type(pointer) else {
            return ValidationResponse::fail(format!(
                "Item pointers should be a urn, for example \
                 (urn:decentraland:{{protocol}}:collections-v2:{{contract(0x[a-fA-F0-9]+)}}:{{id}}). \
                 Invalid pointer: ({pointer})"
            ));
        };
        let key = pointer.to_lowercase();
        match &first {
            None => first = Some((key, asset_type)),
            Some((k, _)) => multiple |= *k != key,
        }
    }
    let Some((_, asset_type)) = first.filter(|_| !multiple) else {
        return ValidationResponse::fail(format!(
            "Only one pointer is allowed when you create an item. Received: {}",
            pointers.join(",")
        ));
    };
    if !valid_types.contains(&asset_type) {
        let valid: Vec<&str> = valid_types.iter().map(|t| t.as_str()).collect();
        return ValidationResponse::fail(format!(
            "For the entity type: {}, the asset with urn type: {} is invalid. \
             Valid urn types for this entity: {}",
            entity.entity_type,
            asset_type.as_str(),
            valid.join(",")
        ));
    }

    let pointer = &pointers[0];

    match asset_type {
        ItemAssetType::OffChain => {
            if !checker.is_address_owned_by_decentraland(deployer_address) {
                return ValidationResponse::fail(format!(
                    "The provided Eth Address '{deployer_address}' does not have access \
                     to the following item: '{pointer}'"
                ));
            }
            ValidationResponse::Ok
        }
        ItemAssetType::CollectionV1 | ItemAssetType::CollectionV2 => {
            if let Some((contract, item_id, layer)) = parse_collection_urn(pointer) {
                let has_access = match checker
                    .check_collection_access(
                        deployer_address,
                        &contract,
                        &item_id,
                        deployment,
                        entity.timestamp,
                        layer,
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        return undecided(
                            "collection access",
                            "Error checking collection access.",
                            &e,
                        );
                    }
                };

                if !has_access {
                    if layer == BlockchainLayer::L1
                        && checker.is_address_owned_by_decentraland(deployer_address)
                        && pointer.contains("collections-v1")
                    {
                        return ValidationResponse::Ok;
                    }
                    return ValidationResponse::fail(format!(
                        "The provided Eth Address '{deployer_address}' does not have access \
                         to the following item: ({contract}, {item_id})"
                    ));
                }
                return ValidationResponse::Ok;
            }

            ValidationResponse::fail(format!("Could not parse collection URN: {pointer}"))
        }
        ItemAssetType::ThirdParty => {
            let verified = match checker
                .check_third_party_access(pointer, entity, deployment, entity.timestamp)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return undecided(
                        "third-party access",
                        "Error checking third-party access.",
                        &e,
                    );
                }
            };

            if !verified {
                return ValidationResponse::fail(
                    "Couldn't verify merkle proofed entity for third-party wearable".to_string(),
                );
            }
            ValidationResponse::Ok
        }
    }
}

/// The `@dcl/urn-resolver` asset types an item pointer can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemAssetType {
    OffChain,
    CollectionV1,
    CollectionV2,
    ThirdParty,
}

impl ItemAssetType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OffChain => "off-chain",
            Self::CollectionV1 => "blockchain-collection-v1-asset",
            Self::CollectionV2 => "blockchain-collection-v2-asset",
            Self::ThirdParty => "blockchain-collection-third-party",
        }
    }
}

const WEARABLE_ASSET_TYPES: &[ItemAssetType] = &[
    ItemAssetType::OffChain,
    ItemAssetType::CollectionV1,
    ItemAssetType::CollectionV2,
    ItemAssetType::ThirdParty,
];
const EMOTE_ASSET_TYPES: &[ItemAssetType] = &[
    ItemAssetType::OffChain,
    ItemAssetType::CollectionV2,
    ItemAssetType::ThirdParty,
];

/// The networks `@dcl/urn-resolver` accepts in a collection URN.
const URN_NETWORKS: &[&str] = &[
    "ethereum", "goerli", "sepolia", "matic", "mumbai", "amoy", "arbitrum",
];
/// The networks with a third-party registry.
const URN_TPR_NETWORKS: &[&str] = &["matic", "mumbai", "amoy"];

fn is_hex_address_like(s: &str) -> bool {
    s.strip_prefix("0x")
        .is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_decimal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The `@dcl/urn-resolver` type of an item pointer when it names an asset
/// (no token id); `None` for anything else, an item URN included.
pub fn item_asset_urn_type(urn: &str) -> Option<ItemAssetType> {
    let lower = urn.to_lowercase();
    let path = lower.strip_prefix("urn:")?;
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let parts: Vec<&str> = path.split(':').collect();
    if parts.iter().any(|p| p.is_empty()) || parts.first() != Some(&"decentraland") {
        return None;
    }
    match parts.as_slice() {
        ["decentraland", "off-chain", _, _] => Some(ItemAssetType::OffChain),
        ["decentraland", network, "collections-v1", collection, _] => {
            let by_contract = is_hex_address_like(collection) && URN_NETWORKS.contains(network);
            (by_contract || *network == "ethereum").then_some(ItemAssetType::CollectionV1)
        }
        ["decentraland", network, "collections-v2", contract, id] => (URN_NETWORKS
            .contains(network)
            && is_hex_address_like(contract)
            && (is_hex_address_like(id) || is_decimal(id)))
        .then_some(ItemAssetType::CollectionV2),
        ["decentraland", network, "collections-thirdparty", _, _, _] => URN_TPR_NETWORKS
            .contains(network)
            .then_some(ItemAssetType::ThirdParty),
        _ => None,
    }
}

fn is_off_chain_urn(urn: &str) -> bool {
    urn.to_lowercase()
        .starts_with("urn:decentraland:off-chain:")
}

pub(super) fn parse_collection_urn(urn: &str) -> Option<(String, String, BlockchainLayer)> {
    let lower = urn.to_lowercase();
    let parts: Vec<&str> = lower.split(':').collect();

    if parts.len() < 6 {
        return None;
    }

    let network = parts[2];
    let contract = parts[4].to_string();
    let item_id = parts[5..].join(":");

    let layer = if is_l1_network(network) || network == "ethereum" {
        BlockchainLayer::L1
    } else if is_l2_network(network) {
        BlockchainLayer::L2
    } else {
        return None;
    };

    Some((contract, item_id, layer))
}

fn is_old_emote(s: &str) -> bool {
    s.len() <= 20 && s.chars().all(|c| c.is_ascii_alphabetic())
}

fn validate_profile_identity_fields(
    metadata: Option<&serde_json::Value>,
    timestamp: Timestamp,
    pointer: &str,
) -> ValidationResponse {
    if timestamp < adr_timestamps::PROFILE_IDENTITY {
        return ValidationResponse::Ok;
    }

    let avatars = metadata
        .and_then(|m| m.get("avatars"))
        .and_then(|a| a.as_array());
    if let Some(avatars) = avatars {
        for avatar in avatars {
            for field in ["ethAddress", "userId"] {
                if let Some(value) = avatar.get(field).and_then(|v| v.as_str()) {
                    if value.is_empty() {
                        continue;
                    }
                    let value = value.to_lowercase();
                    if value != pointer {
                        return ValidationResponse::fail(format!(
                            "The avatar {field} must match the profile pointer \
                             (pointer:{pointer} {field}:{value})."
                        ));
                    }
                }
            }
        }
    }

    ValidationResponse::Ok
}

fn extract_claimed_names(metadata: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(avatars) = metadata.get("avatars").and_then(|v| v.as_array()) {
        for avatar in avatars {
            if avatar.get("hasClaimedName").and_then(|v| v.as_bool()) == Some(true)
                && let Some(name) = avatar.get("name").and_then(|v| v.as_str())
            {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    names.push(trimmed.to_string());
                }
            }
        }
    }
    names
}

fn extract_profile_item_urns(metadata: &serde_json::Value, timestamp: Timestamp) -> Vec<String> {
    let mut urns = Vec::new();
    if let Some(avatars) = metadata.get("avatars").and_then(|v| v.as_array()) {
        for avatar in avatars {
            if timestamp >= adr_timestamps::ADR_75
                && let Some(wearables) = avatar
                    .get("avatar")
                    .and_then(|a| a.get("wearables"))
                    .and_then(|w| w.as_array())
            {
                for w in wearables {
                    if let Some(urn) = w.as_str()
                        && !is_off_chain_urn(urn)
                        && !is_old_emote(urn)
                    {
                        urns.push(urn.to_string());
                    }
                }
            }

            if timestamp >= adr_timestamps::ADR_74
                && let Some(emotes) = avatar
                    .get("avatar")
                    .and_then(|a| a.get("emotes"))
                    .and_then(|e| e.as_array())
            {
                for emote in emotes {
                    if let Some(urn) = emote.get("urn").and_then(|u| u.as_str())
                        && !is_off_chain_urn(urn)
                        && !is_old_emote(urn)
                    {
                        urns.push(urn.to_string());
                    }
                }
            }
        }
    }
    urns
}

fn extract_outfit_wearable_urns(metadata: &serde_json::Value) -> Vec<String> {
    let mut urns = Vec::new();
    if let Some(outfits) = metadata.get("outfits").and_then(|v| v.as_array()) {
        for outfit in outfits {
            if let Some(wearables) = outfit
                .get("outfit")
                .and_then(|o| o.get("wearables"))
                .and_then(|w| w.as_array())
            {
                for w in wearables {
                    if let Some(urn) = w.as_str()
                        && !is_off_chain_urn(urn)
                        && !is_old_emote(urn)
                    {
                        urns.push(urn.to_string());
                    }
                }
            }
        }
    }
    urns
}

fn extract_outfit_names_for_extra_slots(metadata: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(arr) = metadata
        .get("namesForExtraSlots")
        .and_then(|v| v.as_array())
    {
        for name_val in arr {
            if let Some(name) = name_val.as_str() {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    names.push(trimmed.to_string());
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ownership_deployment(etype: EntityType, pointer: &str) -> OwnedDeployment {
        OwnedDeployment::new(Entity {
            id: "bafkreitest".into(),
            entity_type: etype,
            pointers: vec![pointer.to_string()],
            timestamp: 1_700_000_000_000,
            content: vec![],
            version: "v3".into(),
            metadata: None,
        })
    }

    #[test]
    fn local_ownership_rejects_foreign_pointer() {
        let me = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let other = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        for d in [
            ownership_deployment(EntityType::Profile, other),
            ownership_deployment(EntityType::Outfits, &format!("{other}:outfits")),
            ownership_deployment(
                EntityType::Store,
                &format!("urn:decentraland:off-chain:marketplace-stores:{other}"),
            ),
        ] {
            assert!(
                !matches!(
                    validate_local_pointer_ownership(&d.view(), me),
                    ValidationResponse::Ok
                ),
                "foreign {:?} pointer must be rejected even with chain checks off",
                d.entity.entity_type
            );
        }
    }

    #[test]
    fn local_ownership_allows_own_pointer() {
        let me = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for d in [
            ownership_deployment(EntityType::Profile, me),
            ownership_deployment(EntityType::Outfits, &format!("{me}:outfits")),
            ownership_deployment(
                EntityType::Store,
                &format!("urn:decentraland:off-chain:marketplace-stores:{me}"),
            ),
            ownership_deployment(EntityType::Scene, "10,10"),
        ] {
            assert!(
                matches!(
                    validate_local_pointer_ownership(&d.view(), me),
                    ValidationResponse::Ok
                ),
                "own/non-local {:?} pointer must pass the local gate",
                d.entity.entity_type
            );
        }
    }

    #[test]
    fn item_pointers_resolve_to_asset_urn_types() {
        let cases = [
            (
                "urn:decentraland:off-chain:base-avatars:f_hat",
                Some(ItemAssetType::OffChain),
            ),
            (
                "urn:decentraland:ethereum:collections-v1:community_contest:hat",
                Some(ItemAssetType::CollectionV1),
            ),
            (
                "urn:decentraland:matic:collections-v1:community_contest:hat",
                None,
            ),
            (
                "urn:decentraland:sepolia:collections-v1:0xabc123:hat",
                Some(ItemAssetType::CollectionV1),
            ),
            (
                "urn:decentraland:matic:collections-v2:0xabc123:0",
                Some(ItemAssetType::CollectionV2),
            ),
            (
                "urn:decentraland:matic:collections-v2:0xabc123:0x1f",
                Some(ItemAssetType::CollectionV2),
            ),
            ("urn:decentraland:matic:collections-v2:0xabc123:0:12", None),
            ("urn:decentraland:matic:collections-v2:0xabc123:abc", None),
            ("urn:decentraland:mainnet:collections-v2:0xabc123:0", None),
            ("urn:decentraland:matic:collections-v2:0xabc123", None),
            (
                "urn:decentraland:amoy:collections-thirdparty:tp:col:item",
                Some(ItemAssetType::ThirdParty),
            ),
            (
                "urn:decentraland:sepolia:collections-thirdparty:tp:col:item",
                None,
            ),
            (
                "urn:decentraland:amoy:collections-thirdparty:tp:col:item:amoy:0xabc:1",
                None,
            ),
            ("urn:decentraland:amoy:collections-thirdparty:tp:col", None),
            ("urn:decentraland:sepolia:parcel:0,0", None),
            ("not-a-urn", None),
        ];
        for (urn, expected) in cases {
            assert_eq!(item_asset_urn_type(urn), expected, "{urn}");
        }
    }

    /// Item access without a chain: refuses nothing it is asked when it
    /// `grants`, and records what it was asked.
    #[derive(Default)]
    struct ItemChain {
        grants: bool,
        asked: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl BlockchainChecker for ItemChain {
        async fn check_land_access(
            &self,
            _: &str,
            _: &[(i32, i32)],
            _: Timestamp,
        ) -> Result<Vec<bool>, ValidatorError> {
            unreachable!()
        }
        async fn check_names_ownership(
            &self,
            _: &str,
            _: &[String],
            _: Timestamp,
        ) -> Result<PermissionResult, ValidatorError> {
            unreachable!()
        }
        async fn check_items_ownership(
            &self,
            _: &str,
            _: &[String],
            _: Timestamp,
        ) -> Result<PermissionResult, ValidatorError> {
            unreachable!()
        }
        async fn check_collection_access(
            &self,
            _: &str,
            contract: &str,
            item: &str,
            _: &Deployment<'_>,
            _: Timestamp,
            layer: BlockchainLayer,
        ) -> Result<bool, ValidatorError> {
            assert!(self.grants);
            let asked = format!("collection {contract} {item} {layer:?}");
            self.asked.lock().unwrap().push(asked);
            Ok(true)
        }
        async fn check_third_party_access(
            &self,
            pointer: &str,
            _: &Entity,
            _: &Deployment<'_>,
            _: Timestamp,
        ) -> Result<bool, ValidatorError> {
            assert!(self.grants);
            let asked = format!("third-party {pointer}");
            self.asked.lock().unwrap().push(asked);
            Ok(true)
        }
        fn is_address_owned_by_decentraland(&self, _: &str) -> bool {
            self.grants
        }
    }

    async fn item_access_errors(etype: EntityType, pointers: &[&str]) -> Vec<String> {
        let mut deployment = ownership_deployment(etype, pointers[0]);
        deployment.entity.pointers = pointers.iter().map(|p| p.to_string()).collect();
        match validate_item_access(&deployment.view(), &ItemChain::default(), "0xabc").await {
            ValidationResponse::Failed { errors } => errors,
            other => panic!("expected a rejection, got {other}"),
        }
    }

    /// A pointer of an accepted asset type reaches the access check of its type.
    #[tokio::test]
    async fn accepted_item_pointers_reach_their_access_check() {
        let cases = [
            (
                EntityType::Wearable,
                "urn:decentraland:matic:collections-v2:0xAbC123:0",
                Some("collection 0xabc123 0 L2"),
            ),
            (
                EntityType::Emote,
                "urn:decentraland:matic:collections-v2:0xabc123:7",
                Some("collection 0xabc123 7 L2"),
            ),
            (
                EntityType::Wearable,
                "urn:decentraland:ethereum:collections-v1:community_contest:hat",
                Some("collection community_contest hat L1"),
            ),
            (
                EntityType::Emote,
                "urn:decentraland:amoy:collections-thirdparty:tp:col:item",
                Some("third-party urn:decentraland:amoy:collections-thirdparty:tp:col:item"),
            ),
            (
                EntityType::Wearable,
                "urn:decentraland:amoy:collections-thirdparty:tp:col:item",
                Some("third-party urn:decentraland:amoy:collections-thirdparty:tp:col:item"),
            ),
            (
                EntityType::Emote,
                "urn:decentraland:off-chain:base-emotes:wave",
                None,
            ),
        ];
        for (etype, pointer, asked) in cases {
            let chain = ItemChain {
                grants: true,
                ..ItemChain::default()
            };
            let deployment = ownership_deployment(etype, pointer);
            let result = validate_item_access(&deployment.view(), &chain, "0xabc").await;
            assert!(result.is_ok(), "{etype} {pointer}: {result}");
            assert_eq!(
                *chain.asked.lock().unwrap(),
                asked.into_iter().map(String::from).collect::<Vec<_>>(),
                "{pointer}"
            );
        }
    }

    #[tokio::test]
    async fn an_emote_cannot_point_at_a_collections_v1_asset() {
        let errors = item_access_errors(
            EntityType::Emote,
            &["urn:decentraland:ethereum:collections-v1:community_contest:hat"],
        )
        .await;
        assert_eq!(
            errors,
            vec![
                "For the entity type: emote, the asset with urn type: blockchain-collection-v1-asset \
                 is invalid. Valid urn types for this entity: \
                 off-chain,blockchain-collection-v2-asset,blockchain-collection-third-party"
                    .to_string()
            ]
        );
    }

    #[tokio::test]
    async fn an_item_urn_with_a_token_id_is_not_an_item_pointer() {
        let pointer = "urn:decentraland:matic:collections-v2:0xabc123:0:12";
        let errors = item_access_errors(EntityType::Wearable, &[pointer]).await;
        assert_eq!(
            errors,
            vec![format!(
                "Item pointers should be a urn, for example \
                 (urn:decentraland:{{protocol}}:collections-v2:{{contract(0x[a-fA-F0-9]+)}}:{{id}}). \
                 Invalid pointer: ({pointer})"
            )]
        );
    }

    #[tokio::test]
    async fn two_item_pointers_are_refused_in_the_reference_format() {
        let errors = item_access_errors(
            EntityType::Wearable,
            &[
                "urn:decentraland:matic:collections-v2:0xabc123:0",
                "urn:decentraland:matic:collections-v2:0xabc123:1",
            ],
        )
        .await;
        assert_eq!(
            errors,
            vec![
                "Only one pointer is allowed when you create an item. Received: \
                 urn:decentraland:matic:collections-v2:0xabc123:0,\
                 urn:decentraland:matic:collections-v2:0xabc123:1"
                    .to_string()
            ]
        );
    }

    #[tokio::test]
    async fn an_invalid_pointer_is_reported_before_the_pointer_count() {
        let errors = item_access_errors(
            EntityType::Wearable,
            &[
                "urn:decentraland:matic:collections-v2:0xabc123:0",
                "urn:decentraland:matic:collections-v2:0xabc123:1",
                "not-an-urn",
            ],
        )
        .await;
        assert_eq!(
            errors,
            vec![
                "Item pointers should be a urn, for example \
                 (urn:decentraland:{protocol}:collections-v2:{contract(0x[a-fA-F0-9]+)}:{id}). \
                 Invalid pointer: (not-an-urn)"
                    .to_string()
            ]
        );
    }

    /// The CPU time the calling thread has used. Unlike a wall clock, it
    /// leaves out the time the thread waits for a core.
    #[cfg(unix)]
    fn thread_cpu_time() -> std::time::Duration {
        // SAFETY: timespec holds only integers, so all zeroes is a value of it.
        let mut spec: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: clock_gettime writes one timespec into `spec`.
        let status = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut spec) };
        assert_eq!(status, 0, "{}", std::io::Error::last_os_error());
        std::time::Duration::new(spec.tv_sec as u64, spec.tv_nsec as u32)
    }

    // One thread runs the whole check, so its CPU time is the cost of the check.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_huge_pointer_list_is_refused_in_linear_time() {
        let pointers: Vec<String> = (0..200_000)
            .map(|i| format!("urn:decentraland:off-chain:base-avatars:item_{i}"))
            .collect();
        let refs: Vec<&str> = pointers.iter().map(String::as_str).collect();
        let started = thread_cpu_time();
        let errors = item_access_errors(EntityType::Wearable, &refs).await;
        let used = thread_cpu_time() - started;
        assert!(
            used < std::time::Duration::from_secs(10),
            "used {used:?} of CPU time"
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with(
            "Only one pointer is allowed when you create an item. Received: \
             urn:decentraland:off-chain:base-avatars:item_0,\
             urn:decentraland:off-chain:base-avatars:item_1,"
        ));
    }

    #[test]
    fn off_chain_urns_are_recognised_in_any_case() {
        assert!(is_off_chain_urn(
            "urn:decentraland:off-chain:base-avatars:f_hat"
        ));
        assert!(is_off_chain_urn(
            "URN:Decentraland:Off-Chain:base-avatars:f_hat"
        ));
        assert!(!is_off_chain_urn(
            "urn:decentraland:matic:collections-v2:0xabc123:0"
        ));
    }

    #[test]
    fn old_emote_detection() {
        assert!(is_old_emote("dance"));
        assert!(is_old_emote("wave"));
        assert!(!is_old_emote("urn:decentraland:something"));
        assert!(!is_old_emote("has spaces"));
        assert!(!is_old_emote("aVeryLongEmoteNameThatExceedsTwentyChars"));
    }

    #[test]
    fn extract_names_from_profile() {
        let metadata = serde_json::json!({
            "avatars": [
                {
                    "name": "TestName",
                    "hasClaimedName": true,
                    "avatar": { "wearables": [] }
                },
                {
                    "name": "Unclaimed",
                    "hasClaimedName": false,
                    "avatar": { "wearables": [] }
                }
            ]
        });
        let names = extract_claimed_names(&metadata);
        assert_eq!(names, vec!["TestName"]);
    }

    const POINTER: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn identity_metadata(eth_address: Option<&str>, user_id: Option<&str>) -> serde_json::Value {
        let mut avatar = serde_json::json!({ "name": "x", "avatar": { "wearables": [] } });
        if let Some(v) = eth_address {
            avatar["ethAddress"] = serde_json::json!(v);
        }
        if let Some(v) = user_id {
            avatar["userId"] = serde_json::json!(v);
        }
        serde_json::json!({ "avatars": [avatar] })
    }

    #[test]
    fn profile_identity_matching_or_absent_fields_pass() {
        let mixed_case = POINTER.to_uppercase().replace("0X", "0x");
        for metadata in [
            identity_metadata(Some(POINTER), Some(POINTER)),
            identity_metadata(Some(mixed_case.as_str()), None),
            identity_metadata(None, None),
            identity_metadata(Some(""), Some("")),
        ] {
            assert!(
                validate_profile_identity_fields(
                    Some(&metadata),
                    adr_timestamps::PROFILE_IDENTITY,
                    POINTER
                )
                .is_ok()
            );
        }
        assert!(
            validate_profile_identity_fields(None, adr_timestamps::PROFILE_IDENTITY, POINTER)
                .is_ok()
        );
    }

    #[test]
    fn profile_identity_mismatch_is_rejected_per_field() {
        for (metadata, field) in [
            (identity_metadata(Some(OTHER), Some(POINTER)), "ethAddress"),
            (identity_metadata(Some(POINTER), Some(OTHER)), "userId"),
        ] {
            match validate_profile_identity_fields(
                Some(&metadata),
                adr_timestamps::PROFILE_IDENTITY,
                POINTER,
            ) {
                ValidationResponse::Failed { errors } => {
                    assert_eq!(
                        errors,
                        vec![format!(
                            "The avatar {field} must match the profile pointer \
                             (pointer:{POINTER} {field}:{OTHER})."
                        )]
                    );
                }
                other => panic!("mismatched {field} must be rejected, got {other}"),
            }
        }
    }

    #[test]
    fn profile_identity_gate_exempts_older_deployments() {
        let metadata = identity_metadata(Some(OTHER), Some(OTHER));
        assert!(
            validate_profile_identity_fields(
                Some(&metadata),
                adr_timestamps::PROFILE_IDENTITY - 1,
                POINTER
            )
            .is_ok()
        );
    }

    #[test]
    fn outfits_require_ownership_only_for_collection_items() {
        let metadata = serde_json::json!({"outfits": [{"slot": 0, "outfit": {"wearables": [
            "urn:decentraland:off-chain:base-avatars:f_glasses_city",
            "urn:decentraland:off-chain:base-avatars:eyes_04",
            "urn:decentraland:matic:collections-v2:0xabc123:0",
            "invalid-urn"
        ]}}]});
        assert_eq!(
            extract_outfit_wearable_urns(&metadata),
            vec![
                "urn:decentraland:matic:collections-v2:0xabc123:0",
                "invalid-urn"
            ]
        );
    }

    #[test]
    fn extract_profile_item_urns_excludes_default_off_chain_emotes_and_wearables() {
        let metadata = serde_json::json!({
            "avatars": [
                {
                    "name": "TestAvatar",
                    "avatar": {
                        "wearables": [
                            "urn:decentraland:off-chain:base-avatars:BaseMale",
                            "urn:decentraland:off-chain:base-avatars:eyes_00"
                        ],
                        "emotes": [
                            { "slot": 0, "urn": "urn:decentraland:off-chain:base-emotes:wave" },
                            { "slot": 1, "urn": "urn:decentraland:off-chain:base-emotes:fistpump" },
                            { "slot": 2, "urn": "urn:decentraland:off-chain:base-emotes:robot" },
                            { "slot": 3, "urn": "urn:decentraland:off-chain:base-emotes:raiseHand" },
                            { "slot": 4, "urn": "urn:decentraland:off-chain:base-emotes:clap" },
                            { "slot": 5, "urn": "urn:decentraland:off-chain:base-emotes:money" },
                            { "slot": 6, "urn": "urn:decentraland:off-chain:base-emotes:kiss" },
                            { "slot": 7, "urn": "urn:decentraland:off-chain:base-emotes:headexplode" },
                            { "slot": 8, "urn": "urn:decentraland:off-chain:base-emotes:shrug" },
                            { "slot": 9, "urn": "urn:decentraland:off-chain:base-scene-emotes:handsair" }
                        ]
                    }
                }
            ]
        });

        let urns = extract_profile_item_urns(&metadata, 1_700_000_000_000);
        assert!(
            urns.is_empty(),
            "off-chain default wearables/emotes must never reach ownership checks, got {urns:?}"
        );
    }

    #[test]
    fn extract_profile_item_urns_keeps_owned_collection_items() {
        let metadata = serde_json::json!({
            "avatars": [
                {
                    "name": "TestAvatar",
                    "avatar": {
                        "wearables": [
                            "urn:decentraland:off-chain:base-avatars:BaseMale",
                            "urn:decentraland:matic:collections-v2:0xabc123:0"
                        ],
                        "emotes": [
                            { "slot": 0, "urn": "urn:decentraland:off-chain:base-emotes:wave" },
                            {
                                "slot": 1,
                                "urn": "urn:decentraland:matic:collections-v2:0xdef456:3"
                            }
                        ]
                    }
                }
            ]
        });

        let urns = extract_profile_item_urns(&metadata, 1_700_000_000_000);
        assert_eq!(
            urns,
            vec![
                "urn:decentraland:matic:collections-v2:0xabc123:0".to_string(),
                "urn:decentraland:matic:collections-v2:0xdef456:3".to_string(),
            ]
        );
    }
}
