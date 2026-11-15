//! The content validator of the local write path: the port of
//! `catalyrst-validator` (itself the Rust port of `@dcl/content-validator`).
//! [`validate`] runs the old `ContentValidator` over one deployment in the
//! old order ([`rules`]): entity structure, IPFS hashing, the metadata
//! schema of the type ([`metadata_schema`]) and ADR-45, the auth
//! chain, the size cap of the entity type (skins 9 MB), wearable and emote
//! representations and spring bones, the profile rules (ADR-74/75/232/244/290,
//! emote slots), the item and face256 thumbnails ([`image_metadata`]), the
//! scene rules (parcels match pointers, ADR-173, ADR-236), the content
//! cross-check, the outfits rules, the third-party merkle proof content
//! ([`third_party`]) and the access step per type ([`checker`]), and answers
//! with the old error strings byte for byte.
//!
//! Where the old checker read the squid, [`Sources`] asks the chain the way
//! the reference catalyst's on-chain validator does ([`onchain`], `chain.rs`
//! has the table): the Checker contract and the item contracts over
//! `ETH_RPC_URL` (L1) and `RPC_ENDPOINT_POLYGON` (L2) at the block of the
//! entity timestamp, then five minutes before it; the escrow lease overlay
//! (`shop.usage_grants`) over the ONE pool, content probes against the ONE
//! blob store, every RPC call through the ONE `http::Outbound`.
//!
//! Each error has a kind ([`ValidationError`]): `Rejected` is the
//! deployment's fault (400); `Unavailable` means this node could not decide
//! (its store could not be read, a source is unset or failed, a
//! contract-wallet signature could not be asked about) and the write path
//! answers 503 with `Retry-After`. The old validator rejected on a checker
//! failure; it is now `Unavailable` with a fixed sentence, the cause logged.
//!
//! Not carried: `squid_checker.rs` (replaced as above), the squid root
//! source of third-party wearables and its refresh job, `land_rights.rs`
//! (the types and their tests live in `land_authz::resolve`),
//! `ExternalCalls::calculate_files_hashes`, the ts-rs bindings.

mod chain;
pub mod checker;
mod collections_v1;
pub mod entity_parser;
pub mod erc721;
pub mod error;
#[cfg(test)]
pub(crate) mod fake_chain;
mod helpers;
pub mod image_metadata;
mod item_hash;
pub(crate) mod jsjson;
pub mod merkle;
mod metadata_schema;
pub(crate) mod onchain;
pub mod rules;
mod scene_parcels;
mod spring_bones;
pub mod synced;
pub mod third_party;
pub mod types;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::bail;
use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{AuthChain, AuthError, Eip1654Validator};
use crate::http::Outbound;
use crate::storage::{Storage, StorageError};

pub use chain::DECENTRALAND_ADDRESS;
pub use error::{ValidationError, ValidationResponse, ValidatorError};
pub use metadata_schema::validate_scene_metadata;
pub use rules::{ContentValidator, ExternalCalls};
pub use spring_bones::validate_spring_bones_metadata;
pub use synced::{SyncedContent, validate_synced};
pub use types::{Deployment, Entity, EntityType};

/// The validator's configuration, under the old environment names.
#[derive(Clone, Default)]
pub struct Settings {
    /// `IGNORE_BLOCKCHAIN_ACCESS_CHECKS`: the access step runs only the
    /// local pointer-ownership rule (profiles, outfits, stores); for a node
    /// that must accept deploys without its chain sources. Off by default.
    pub ignore_blockchain_access: bool,
    /// `ADDITIONAL_DECENTRALAND_ADDRESS`: a second address with the
    /// Decentraland address's rights (default profiles, off-chain items,
    /// legacy content, v1 collection items).
    pub additional_dcl_address: Option<String>,
    /// `ETH_NETWORK`: mainnet (with polygon) or sepolia (with amoy).
    pub eth_network: String,
    /// `ETH_RPC_URL`: the L1 JSON-RPC.
    pub l1_rpc_url: Option<String>,
    /// `RPC_ENDPOINT_POLYGON`: the L2 JSON-RPC.
    pub l2_rpc_url: Option<String>,
}

/// No derived `Debug`: an RPC URL can carry a gateway key.
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("ignore_blockchain_access", &self.ignore_blockchain_access)
            .field("additional_dcl_address", &self.additional_dcl_address)
            .field("eth_network", &self.eth_network)
            .field("l1_rpc_url", &self.l1_rpc_url.is_some())
            .field("l2_rpc_url", &self.l2_rpc_url.is_some())
            .finish()
    }
}

impl Settings {
    /// The values as the environment spells them. `l1_rpc_url` is
    /// `Config::eth_rpc_url` (already https); a plaintext L2 endpoint is not
    /// used for access checks, since its answers decide who may deploy. With
    /// `content_writes` on and the access checks not ignored, a network
    /// without checker contracts or a missing or plaintext RPC refuses to
    /// start. An error names the variable, never the value.
    pub fn parse(
        ignore_blockchain_access: bool,
        content_writes: bool,
        additional_dcl_address: Option<&str>,
        eth_network: &str,
        l1_rpc_url: Option<String>,
        l2_rpc_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let clean = |v: Option<&str>| {
            v.map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let required = content_writes && !ignore_blockchain_access;
        let eth_network = eth_network.trim().to_string();
        let l2_raw = clean(l2_rpc_url);
        let l2_https = l2_raw
            .as_deref()
            .is_some_and(|url| url::Url::parse(url).is_ok_and(|u| u.scheme() == "https"));
        if required {
            let fix = "or set IGNORE_BLOCKCHAIN_ACCESS_CHECKS=true";
            if !matches!(eth_network.as_str(), "mainnet" | "sepolia") {
                bail!(
                    "ETH_NETWORK={eth_network} has no checker contracts (mainnet|sepolia): \
                     local deploys could not be checked; set it {fix}"
                );
            }
            if l1_rpc_url.is_none() {
                bail!("ETH_RPC_URL is unset: local deploys could not be checked; set it {fix}");
            }
            if !l2_https {
                bail!(
                    "RPC_ENDPOINT_POLYGON is unset or not an https URL: local deploys could \
                     not be checked; set it {fix}"
                );
            }
        } else if l2_raw.is_some() && !l2_https {
            tracing::warn!(
                "RPC_ENDPOINT_POLYGON is not https: deploy access checks on L2 stay undecidable"
            );
        }
        let l2_rpc_url = l2_raw.filter(|_| l2_https);
        let additional_dcl_address = clean(additional_dcl_address).map(|a| a.to_lowercase());
        if additional_dcl_address
            .as_deref()
            .is_some_and(|a| !crate::auth::is_eth_address(a))
        {
            bail!("ADDITIONAL_DECENTRALAND_ADDRESS is not an Ethereum address");
        }
        Ok(Self {
            ignore_blockchain_access,
            additional_dcl_address,
            eth_network,
            l1_rpc_url,
            l2_rpc_url,
        })
    }
}

impl Settings {
    /// A node that judges peers' entities (sync, read-through) needs the same
    /// chain sources as a local deploy: without them every entity with an
    /// access check would stay undecided. An error names the variable only.
    pub fn require_chain_for_peer_entities(&self) -> anyhow::Result<()> {
        if self.ignore_blockchain_access {
            return Ok(());
        }
        let fix = "or set IGNORE_BLOCKCHAIN_ACCESS_CHECKS=true";
        if !matches!(self.eth_network.as_str(), "mainnet" | "sepolia") {
            bail!(
                "ETH_NETWORK={} has no checker contracts (mainnet|sepolia): synced entities \
                 could not be checked; set it {fix}",
                self.eth_network
            );
        }
        if self.l1_rpc_url.is_none() {
            bail!("ETH_RPC_URL is unset: synced entities could not be checked; set it {fix}");
        }
        if self.l2_rpc_url.is_none() {
            bail!(
                "RPC_ENDPOINT_POLYGON is unset or not an https URL: synced entities could not \
                 be checked; set it {fix}"
            );
        }
        Ok(())
    }
}

/// What the validator reads, built once by `serve`: the ONE pool (the
/// `shop.usage_grants` overlay), the ONE blob store, the shared EIP-1654
/// validator (`None` without `ETH_RPC_URL`: a contract-wallet link is then
/// undecidable) and the chain access checks over the ONE client.
pub struct Sources {
    pool: PgPool,
    storage: Arc<Storage>,
    eip1654: Option<Arc<dyn Eip1654Validator>>,
    access: Arc<onchain::OnChainAccess>,
    ignore_blockchain_access: bool,
    additional_dcl_address: Option<String>,
}

impl Sources {
    pub fn new(
        pool: PgPool,
        storage: Arc<Storage>,
        http: Outbound,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
        settings: Settings,
    ) -> Self {
        let access = onchain::OnChainAccess::new(
            http,
            &settings.eth_network,
            settings.l1_rpc_url,
            settings.l2_rpc_url,
        );
        Self {
            pool,
            storage,
            eip1654,
            access: Arc::new(access),
            ignore_blockchain_access: settings.ignore_blockchain_access,
            additional_dcl_address: settings.additional_dcl_address,
        }
    }

    /// The chain access checks, for entities this node did not validate
    /// itself (peer sync): every check takes the entity timestamp.
    pub(crate) fn access(&self) -> Arc<onchain::OnChainAccess> {
        self.access.clone()
    }

    /// Each configured RPC answers for the chain `ETH_NETWORK` names; an
    /// unreachable RPC is only logged.
    pub async fn verify_chain_ids(&self) -> anyhow::Result<()> {
        self.access.verify_chain_ids().await
    }

    fn eip1654(&self) -> Option<&dyn Eip1654Validator> {
        self.eip1654.as_deref()
    }
}

#[async_trait]
impl ExternalCalls for Sources {
    async fn is_content_stored_already(
        &self,
        hashes: &[String],
    ) -> Result<HashMap<String, bool>, String> {
        self.storage
            .exist_multiple(hashes)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_content_file_size(&self, hash: &str) -> Result<Option<usize>, String> {
        match self.storage.file_info(hash).await {
            Ok(info) => Ok(info.map(|i| i.size as usize)),
            Err(StorageError::InvalidId(_) | StorageError::PathTraversal(_)) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// The ephemeral links are checked against the entity timestamp, as
    /// upstream does: a key that expired after signing still signed a valid
    /// deployment.
    async fn validate_signature(
        &self,
        entity_id: &str,
        auth_chain: &AuthChain,
        timestamp: types::Timestamp,
    ) -> Result<(), AuthError> {
        crate::auth::verify_auth_chain_async(
            auth_chain,
            entity_id,
            Some(timestamp),
            self.eip1654.as_deref(),
        )
        .await
    }
}

/// One deployment's validation: the signer's chain and the node's sources.
pub struct Context<'a> {
    pub sources: &'a Sources,
    pub auth_chain: &'a AuthChain,
}

/// Validates a local deployment: `entity` as parsed from its file (id set),
/// `files` the uploaded bytes by content hash with the entity file under
/// the entity id. `Err` carries the old error strings, every one of the same
/// kind.
pub async fn validate(
    entity: &Entity,
    files: &HashMap<String, bytes::Bytes>,
    ctx: &Context<'_>,
) -> Result<(), Vec<ValidationError>> {
    let deployment = Deployment {
        entity,
        files,
        auth_chain: ctx.auth_chain,
    };
    ContentValidator {
        external_calls: ctx.sources,
        blockchain_checker: ctx.sources,
        ignore_blockchain_access: ctx.sources.ignore_blockchain_access,
        synced: false,
        access_passed: false,
    }
    .validate(&deployment)
    .await
    .into_result()
}

/// Validates one batch of a partial upload ([`ContentValidator::validate_staging`]):
/// `files` holds the entity file under its id and the batch's other keys
/// (their bytes are not read).
pub async fn validate_staging(
    entity: &Entity,
    files: &HashMap<String, bytes::Bytes>,
    ctx: &Context<'_>,
    include_access: bool,
) -> Result<(), Vec<ValidationError>> {
    let deployment = Deployment {
        entity,
        files,
        auth_chain: ctx.auth_chain,
    };
    ContentValidator {
        external_calls: ctx.sources,
        blockchain_checker: ctx.sources,
        ignore_blockchain_access: ctx.sources.ignore_blockchain_access,
        synced: false,
        access_passed: false,
    }
    .validate_staging(&deployment, include_access)
    .await
    .into_result()
}

/// The write deployer's failure for a refused validation: `Unavailable`
/// (503) when this node could not decide, `Unverifiable` (503) for a
/// contract-wallet signature not checked here, `Rejected` (400) otherwise.
pub fn into_deploy_failure(errors: Vec<ValidationError>) -> super::write_deployer::DeployFailure {
    use super::write_deployer::DeployFailure;
    let unavailable = errors.iter().any(ValidationError::is_unavailable);
    let unverifiable = errors.iter().any(ValidationError::is_unverifiable);
    let messages: Vec<String> = errors.iter().map(|e| e.message().to_string()).collect();
    if unavailable {
        DeployFailure::Unavailable(messages)
    } else if unverifiable {
        DeployFailure::Unverifiable(messages)
    } else {
        DeployFailure::Rejected(super::write_deployer::RejectionKind::Validation, messages)
    }
}
