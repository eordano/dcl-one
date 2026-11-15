//! The local write path behind `POST /entities` ([`super::create_entity`]):
//! an uploaded entity file plus its content files become rows in
//! `content.deployments`, `content.content_files`, `content.active_pointers`
//! and (for a local scene) `content.local_entities`, all in ONE database
//! call, a plpgsql function named after the hash of its own body and defined
//! in `schemas/00_content.sql` ([`super::land_publish::content_functions_sql`]).
//!
//! Before that call, in upstream's order and with its texts: the entity
//! file and its parse (its `id` is the request's), the operator denylist
//! ([`super::denylist::refusal`]: a listed entity id, content hash or
//! pointer is refused, a dcl-one refusal), the signature at the entity's
//! timestamp, the pointers and this process's in-flight claim on them, the
//! newer-entity check, the per-pointer rate limits, the request TTLs
//! ([`DeployLimits`]), and the validation.
//! Production construction requires the full content validator: auth chains,
//! per-type rules and size caps, content existence, and the access step per
//! type. Test-support fixtures can explicitly select partial checks. Peer
//! sync does not use this deployer: it
//! batches through [`super::sync_deployer::LiveSyncDeployer`], which shares
//! [`lock_deployment_pointers`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{StreamExt, stream};
use parking_lot::Mutex;
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use super::Denylist;
use super::entity::{Entity, EntityType};
use crate::auth::verify::verify_auth_chain_async;
use crate::auth::{AuthChain, AuthError, Eip1654Validator, hash_bytes, hash_bytes_v1};
use crate::land_authz::LandAuthzState;
use crate::storage::Storage;

/// The local write decision, separate from its unchanged wire message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionKind {
    Validation,
    PointerContention,
    RateLimited,
    Superseded,
}

#[derive(Debug, Clone)]
pub enum DeployFailure {
    Rejected(RejectionKind, Vec<String>),
    /// This node could not decide, so the deposit is neither accepted nor at fault; answer 503 so
    /// the caller retries instead of being told their content does not exist.
    Unavailable(Vec<String>),
    /// A contract-wallet signature this node could not check: 503 like
    /// `Unavailable`, but it follows from the caller's auth chain.
    Unverifiable(Vec<String>),
}

impl DeployFailure {
    pub fn errors(&self) -> &[String] {
        match self {
            DeployFailure::Rejected(_, errors)
            | DeployFailure::Unavailable(errors)
            | DeployFailure::Unverifiable(errors) => errors,
        }
    }
}

/// A `local_timestamp` in ms as the history routes report it: their
/// `date_part('epoch', ..) * 1000`, truncated.
macro_rules! local_ms_sql {
    ($column:literal) => {
        concat!("floor(date_part('epoch', ", $column, ") * 1000)::bigint")
    };
}

/// Upstream's answer for a deploy that failed on this node's side; the
/// cause (a database, store or task error) is logged, never answered.
pub(crate) const DEPLOY_ERROR_MESSAGE: &str = "There was an error deploying the entity";

/// `Unavailable` with [`DEPLOY_ERROR_MESSAGE`], `cause` logged.
fn deploy_error(entity_id: &str, cause: &str) -> DeployFailure {
    tracing::error!(entity_id, cause, "deployment failed on this node");
    DeployFailure::Unavailable(vec![DEPLOY_ERROR_MESSAGE.to_string()])
}

impl From<Vec<String>> for DeployFailure {
    fn from(errors: Vec<String>) -> Self {
        DeployFailure::Rejected(RejectionKind::Validation, errors)
    }
}

struct HashedUpload {
    v0: String,
    v1: String,
    bytes: Bytes,
}

pub(crate) static UPLOAD_HASH_JOBS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(4)));

pub(crate) async fn bounded_upload_hash<T: Send + 'static>(
    limit: Arc<Semaphore>,
    hash: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let permit = limit
        .acquire_owned()
        .await
        .map_err(|_| "Upload hashing capacity is unavailable.".to_string())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash()
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "upload hashing task failed");
        "Upload hashing failed.".to_string()
    })
}

async fn hash_uploads(files: Vec<Bytes>) -> Result<Vec<HashedUpload>, String> {
    bounded_upload_hash(Arc::clone(&UPLOAD_HASH_JOBS), move || {
        files
            .into_iter()
            .map(|bytes| HashedUpload {
                v0: hash_bytes(&bytes),
                v1: hash_bytes_v1(&bytes),
                bytes,
            })
            .collect()
    })
    .await
}

fn deployment_files(
    entity: &Entity,
    entity_bytes: &Bytes,
    uploads: &[HashedUpload],
) -> HashMap<String, Bytes> {
    let by_hash: HashMap<&str, &Bytes> = uploads
        .iter()
        .flat_map(|upload| {
            [
                (upload.v0.as_str(), &upload.bytes),
                (upload.v1.as_str(), &upload.bytes),
            ]
        })
        .collect();
    let mut files = HashMap::from([(entity.id.clone(), entity_bytes.clone())]);
    for content in &entity.content {
        if let Some(bytes) = by_hash.get(content.hash.as_str()) {
            files.insert(content.hash.clone(), (*bytes).clone());
        }
    }
    files
}

async fn store_deployment_files(
    storage: &Storage,
    files: HashMap<String, Bytes>,
) -> Result<(), String> {
    let mut writes = stream::iter(files.into_iter().map(|(hash, bytes)| async move {
        storage
            .store(&hash, bytes)
            .await
            .map_err(|e| format!("failed to store deployment file {hash}: {e}"))
    }))
    .buffer_unordered(8);
    let mut first_error = None;
    while let Some(result) = writes.next().await {
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
fn happened_before_cmp(a_ts: i64, a_id: &str, b_ts: i64, b_id: &str) -> std::cmp::Ordering {
    a_ts.cmp(&b_ts)
        .then_with(|| a_id.to_lowercase().cmp(&b_id.to_lowercase()))
}

#[cfg(test)]
fn stored_is_newer_or_equal(stored_ts: i64, stored_id: &str, in_ts: i64, in_id: &str) -> bool {
    happened_before_cmp(in_ts, in_id, stored_ts, stored_id) == std::cmp::Ordering::Less
}

#[cfg(test)]
fn stored_is_overwritten(stored_ts: i64, stored_id: &str, in_ts: i64, in_id: &str) -> bool {
    happened_before_cmp(stored_ts, stored_id, in_ts, in_id) == std::cmp::Ordering::Less
}

fn metadata_unchanged(stored_wrapped: Option<&Value>, incoming: Option<&Value>) -> bool {
    let active = match stored_wrapped {
        Some(meta) => meta.get("v").cloned().unwrap_or(Value::Null),
        None => return false,
    };
    let new_meta = incoming.cloned().unwrap_or(Value::Null);
    active == new_meta
}

/// Measured on tokio's clock, which is the system's outside a paused test runtime.
struct TtlBucket {
    ttl: Duration,
    max_size: usize,
    entries: HashMap<String, tokio::time::Instant>,
}

impl TtlBucket {
    fn new(ttl: Duration, max_size: usize) -> Self {
        Self {
            ttl,
            max_size,
            entries: HashMap::new(),
        }
    }

    fn prune(&mut self, now: tokio::time::Instant) {
        let ttl = self.ttl;
        self.entries.retain(|_, t| now.duration_since(*t) < ttl);
    }

    fn is_limited(&mut self, pointers: &[String]) -> bool {
        let now = tokio::time::Instant::now();
        self.prune(now);
        let ttl_hit = pointers.iter().any(|p| self.entries.contains_key(p));
        let size_hit = self.entries.len() > self.max_size;
        ttl_hit || size_hit
    }

    /// Until the pointers clear: the latest of their windows, or a whole
    /// TTL when the bucket is over its size; `None` when not limited.
    fn retry_after(&mut self, pointers: &[String]) -> Option<Duration> {
        if !self.is_limited(pointers) {
            return None;
        }
        let now = tokio::time::Instant::now();
        let window = pointers
            .iter()
            .filter_map(|p| self.entries.get(p))
            .map(|t| self.ttl.saturating_sub(now.duration_since(*t)))
            .max();
        Some(window.unwrap_or(self.ttl))
    }

    #[cfg(test)]
    fn record(&mut self, pointers: &[String]) {
        let now = tokio::time::Instant::now();
        for p in pointers {
            self.entries.insert(p.clone(), now);
        }
    }

    /// The check and the record in one step: `None` when limited.
    fn try_reserve(&mut self, pointers: &[String]) -> Option<Held> {
        if self.is_limited(pointers) {
            return None;
        }
        let at = tokio::time::Instant::now();
        let mut prev = HashMap::with_capacity(pointers.len());
        for p in pointers {
            let old = self.entries.insert(p.clone(), at);
            prev.entry(p.clone()).or_insert(old);
        }
        Some(Held { at, prev })
    }

    /// Restores each pointer's earlier entry, unless something re-recorded it since.
    fn release(&mut self, held: Held) {
        for (p, old) in held.prev {
            if self.entries.get(&p) != Some(&held.at) {
                continue;
            }
            match old {
                Some(t) => self.entries.insert(p, t),
                None => self.entries.remove(&p),
            };
        }
    }
}

struct Held {
    at: tokio::time::Instant,
    prev: HashMap<String, Option<tokio::time::Instant>>,
}

/// Pointers held from a passed check; [`Reservation::release`] frees them when the deploy is
/// known not to have been stored. Dropping it keeps them, since a cancelled write may have committed.
struct Reservation<'a> {
    limiter: &'a DeployRateLimiter,
    etype: String,
    held: Option<Held>,
    unchanged: Option<Held>,
}

impl Reservation<'_> {
    fn release(self) {
        if let Some(held) = self.held
            && let Some(bk) = self.limiter.buckets.lock().get_mut(self.etype.as_str())
        {
            bk.release(held);
        }
        if let Some(held) = self.unchanged {
            self.limiter.unchanged_profile.lock().release(held);
        }
    }
}

struct DeployRateLimiter {
    buckets: Mutex<HashMap<&'static str, TtlBucket>>,
    unchanged_profile: Mutex<TtlBucket>,
}

/// The built-in per-pointer windows and key caps by entity type, upstream's
/// (`getCacheConfigPerEntityMap` in `rate-limiter.ts`), used where no
/// `DEPLOYMENT_RATE_LIMIT_*_<TYPE>` setting names the type.
const REFERENCE_RATE_LIMITS: [(&str, Duration, usize); 6] = [
    ("profile", Duration::from_secs(3), 500),
    ("scene", Duration::from_secs(20), 1000),
    ("wearable", Duration::from_secs(20), 1000),
    ("store", Duration::from_secs(3), 300),
    ("emote", Duration::from_secs(20), 1000),
    ("outfits", Duration::from_secs(3), 2000),
];

/// The deployer's request bounds: `REQUEST_TTL_BACKWARDS`, and per entity
/// type the per-pointer rate limit's window and key cap. A type takes its
/// own `DEPLOYMENT_RATE_LIMIT_TTL_<TYPE>` / `_MAX_<TYPE>`, else its
/// [`REFERENCE_RATE_LIMITS`] entry. As upstream's `rate-limiter.ts`,
/// `DEPLOYMENTS_DEFAULT_RATE_LIMIT_TTL` / `_MAX` apply only to a type
/// outside that table, and every type this node deploys is in it.
#[derive(Clone, Debug)]
pub struct DeployLimits {
    pub request_ttl_backwards_ms: i64,
    pub rate_limits: Vec<(&'static str, Duration, usize)>,
}

impl Default for DeployLimits {
    fn default() -> Self {
        Self {
            request_ttl_backwards_ms: REQUEST_TTL_BACKWARDS_MS,
            rate_limits: REFERENCE_RATE_LIMITS.to_vec(),
        }
    }
}

impl DeployLimits {
    pub fn from_config(c: &crate::config::ContentConfig) -> Self {
        let per_type = |etype: &str| match etype {
            "scene" => (
                c.deployment_rate_limit_ttl_scene_ms,
                c.deployment_rate_limit_max_scene,
            ),
            "profile" => (
                c.deployment_rate_limit_ttl_profile_ms,
                c.deployment_rate_limit_max_profile,
            ),
            "wearable" => (
                c.deployment_rate_limit_ttl_wearable_ms,
                c.deployment_rate_limit_max_wearable,
            ),
            "store" => (
                c.deployment_rate_limit_ttl_store_ms,
                c.deployment_rate_limit_max_store,
            ),
            "emote" => (
                c.deployment_rate_limit_ttl_emote_ms,
                c.deployment_rate_limit_max_emote,
            ),
            _ => (
                c.deployment_rate_limit_ttl_outfits_ms,
                c.deployment_rate_limit_max_outfits,
            ),
        };
        let rate_limits = REFERENCE_RATE_LIMITS
            .iter()
            .map(|&(etype, ttl, max)| {
                let (type_ttl, type_max) = per_type(etype);
                let ttl = type_ttl.map_or(ttl, Duration::from_millis);
                let max = type_max.map_or(max, |m| usize::try_from(m).unwrap_or(usize::MAX));
                (etype, ttl, max)
            })
            .collect();
        Self {
            request_ttl_backwards_ms: i64::try_from(c.request_ttl_backwards_ms).unwrap_or(i64::MAX),
            rate_limits,
        }
    }
}

impl DeployRateLimiter {
    fn with_reference_defaults() -> Self {
        Self {
            buckets: Mutex::new(Self::buckets(&REFERENCE_RATE_LIMITS)),
            unchanged_profile: Mutex::new(TtlBucket::new(Duration::from_secs(300), usize::MAX)),
        }
    }

    fn buckets(limits: &[(&'static str, Duration, usize)]) -> HashMap<&'static str, TtlBucket> {
        limits
            .iter()
            .map(|&(etype, ttl, max)| (etype, TtlBucket::new(ttl, max)))
            .collect()
    }

    /// Replaces the windows, forgetting what the old ones held.
    fn configure(&self, limits: &[(&'static str, Duration, usize)]) {
        *self.buckets.lock() = Self::buckets(limits);
    }

    fn is_rate_limited(&self, etype: &str, pointers: &[String]) -> bool {
        let mut b = self.buckets.lock();
        b.get_mut(etype)
            .map(|bk| bk.is_limited(pointers))
            .unwrap_or(false)
    }

    #[cfg(test)]
    fn record(&self, etype: &str, pointers: &[String]) {
        if let Some(bk) = self.buckets.lock().get_mut(etype) {
            bk.record(pointers);
        }
    }

    /// Checks and records the pointers atomically (and in the unchanged-profile bucket when
    /// `unchanged`); `None` when either bucket is limited.
    fn reserve(
        &self,
        etype: &str,
        pointers: &[String],
        unchanged: bool,
    ) -> Option<Reservation<'_>> {
        let held = match self.buckets.lock().get_mut(etype) {
            Some(bk) => Some(bk.try_reserve(pointers)?),
            None => None,
        };
        let mut reservation = Reservation {
            limiter: self,
            etype: etype.to_string(),
            held,
            unchanged: None,
        };
        if unchanged {
            let held = self.unchanged_profile.lock().try_reserve(pointers);
            match held {
                Some(held) => reservation.unchanged = Some(held),
                None => {
                    reservation.release();
                    return None;
                }
            }
        }
        Some(reservation)
    }

    fn retry_after(&self, etype: &str, pointers: &[String]) -> Option<Duration> {
        self.buckets.lock().get_mut(etype)?.retry_after(pointers)
    }

    fn is_unchanged_limited(&self, pointers: &[String]) -> bool {
        self.unchanged_profile.lock().is_limited(pointers)
    }

    #[cfg(test)]
    fn clear(&self) {
        for bucket in self.buckets.lock().values_mut() {
            bucket.entries.clear();
        }
        self.unchanged_profile.lock().entries.clear();
    }
}

/// One deploy at a time per entity id in this process. A copy of an
/// in-flight deploy (a client's retry after a timeout) waits for it and
/// answers the timestamp it stored, instead of meeting the pointers the first
/// one holds in the rate limiter. An entry lives while a deploy holds or
/// awaits it.
#[derive(Default)]
struct EntityLocks(Mutex<HashMap<String, std::sync::Weak<EntityLock>>>);

/// The stored timestamp once a holder deployed the entity.
type EntityLock = tokio::sync::Mutex<Option<i64>>;

impl EntityLocks {
    async fn lock(&self, entity_id: &str) -> tokio::sync::OwnedMutexGuard<Option<i64>> {
        let lock = {
            let mut locks = self.0.lock();
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(entity_id).and_then(std::sync::Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(EntityLock::new(None));
                    locks.insert(entity_id.to_string(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }
}

enum LocalValidation {
    Content(Arc<super::validator::Sources>),
    #[cfg(any(test, feature = "test-support"))]
    PartialChecks,
}

pub struct WriteDeployer {
    pool: PgPool,
    storage: Arc<Storage>,
    /// The shared EIP-1654 validator (`crate::auth::cached_rpc_validator`);
    /// `None` without `ETH_RPC_URL`: a contract-wallet link is undecidable.
    eip1654: Option<Arc<dyn Eip1654Validator>>,
    /// The LAND rights index (`land_authz::can_deploy_batch`): the partial
    /// checks' LAND leg (`None` runs none) and the unpublish route's parcel
    /// check.
    land_authz: Option<Arc<LandAuthzState>>,
    validator: LocalValidation,
    /// The shared denylist (`ContentState::denylist`): a deploy naming a
    /// listed id is refused.
    denylist: Option<Arc<Denylist>>,
    rate_limiter: DeployRateLimiter,
    /// `REQUEST_TTL_BACKWARDS`, ms ([`WriteDeployer::configure`]).
    request_ttl_backwards_ms: AtomicI64,
    /// The pointers a deploy of this process holds, by entity type, from its
    /// checks to its write: a second deploy of any of them is refused.
    pointers_in_flight: Mutex<HashMap<String, HashSet<String>>>,
    deploying: EntityLocks,
    /// The deploy function per provenance variant, once its existence was checked.
    deploy_fns: [tokio::sync::OnceCell<String>; 2],
    /// Upstream forwarding (`CONTENT_FORWARD_UPSTREAM_URL`): a local deploy
    /// queues its `content.forward_outbox` row in the deploy statement
    /// ([`forward_call`]) and wakes the worker.
    forwarder: Option<Arc<super::forward::Forwarder>>,
}

impl WriteDeployer {
    /// Full local validation; a missing EIP-1654 source remains undecidable.
    pub fn new(
        pool: PgPool,
        storage: Arc<Storage>,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
        sources: Arc<super::validator::Sources>,
    ) -> Self {
        Self::with_validation(pool, storage, eip1654, LocalValidation::Content(sources))
    }

    /// Partial chain, LAND and content checks for fixtures without validator sources.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_partial_validation_for_tests(
        pool: PgPool,
        storage: Arc<Storage>,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
    ) -> Self {
        Self::with_validation(pool, storage, eip1654, LocalValidation::PartialChecks)
    }

    fn with_validation(
        pool: PgPool,
        storage: Arc<Storage>,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
        validator: LocalValidation,
    ) -> Self {
        Self {
            pool,
            storage,
            eip1654,
            land_authz: None,
            validator,
            denylist: None,
            rate_limiter: DeployRateLimiter::with_reference_defaults(),
            request_ttl_backwards_ms: AtomicI64::new(REQUEST_TTL_BACKWARDS_MS),
            pointers_in_flight: Mutex::new(HashMap::new()),
            deploying: EntityLocks::default(),
            deploy_fns: [tokio::sync::OnceCell::new(), tokio::sync::OnceCell::new()],
            forwarder: None,
        }
    }

    /// Applies the configured request TTL and rate limits.
    pub fn configure(&self, limits: &DeployLimits) {
        self.request_ttl_backwards_ms
            .store(limits.request_ttl_backwards_ms, Ordering::Relaxed);
        self.rate_limiter.configure(&limits.rate_limits);
    }

    /// `REQUEST_TTL_BACKWARDS`, ms.
    pub(crate) fn request_ttl_backwards_ms(&self) -> i64 {
        self.request_ttl_backwards_ms.load(Ordering::Relaxed)
    }

    /// Whether a deploy of this process holds `pointer` of `etype`.
    #[cfg(test)]
    pub(crate) fn holds_pointer(&self, etype: &str, pointer: &str) -> bool {
        self.pointers_in_flight
            .lock()
            .get(etype)
            .is_some_and(|held| held.contains(pointer))
    }

    /// The ones of `pointers` of `etype` another deploy of this process holds,
    /// in the entity's order; takes nothing.
    fn busy_pointers(&self, etype: &str, pointers: &[String]) -> Vec<String> {
        self.pointers_in_flight
            .lock()
            .get(etype)
            .map_or_else(Vec::new, |held| held_of(held, pointers))
    }

    #[cfg(test)]
    pub(crate) fn hold_pointers_for_tests(
        &self,
        etype: &str,
        pointers: &[String],
    ) -> impl Drop + '_ {
        self.claim_pointers(etype, pointers)
            .expect("fixture pointers are free")
    }

    /// Claims `pointers` of `etype` for one deploy; `Err` names the ones
    /// another deploy of this process holds, in the entity's order.
    fn claim_pointers<'a>(
        &'a self,
        etype: &str,
        pointers: &[String],
    ) -> Result<PointerClaim<'a>, Vec<String>> {
        let mut in_flight = self.pointers_in_flight.lock();
        let held = in_flight.entry(etype.to_string()).or_default();
        let busy = held_of(held, pointers);
        if !busy.is_empty() {
            return Err(busy);
        }
        held.extend(pointers.iter().cloned());
        Ok(PointerClaim {
            deployer: self,
            etype: etype.to_string(),
            pointers: pointers.to_vec(),
        })
    }

    pub fn with_forwarder(mut self, forwarder: Option<Arc<super::forward::Forwarder>>) -> Self {
        self.forwarder = forwarder;
        self
    }

    pub fn forwarder(&self) -> Option<&Arc<super::forward::Forwarder>> {
        self.forwarder.as_ref()
    }

    pub fn with_land_authz(mut self, land: Option<Arc<LandAuthzState>>) -> Self {
        self.land_authz = land;
        self
    }

    pub fn with_denylist(mut self, denylist: Arc<Denylist>) -> Self {
        self.denylist = Some(denylist);
        self
    }

    pub fn land_authz(&self) -> Option<&Arc<LandAuthzState>> {
        self.land_authz.as_ref()
    }

    pub fn eip1654(&self) -> Option<&dyn Eip1654Validator> {
        self.eip1654.as_deref()
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn storage(&self) -> &Arc<Storage> {
        &self.storage
    }

    /// The stored `local_timestamp` (ms) of a deployed entity, as the
    /// history routes report it.
    pub(crate) async fn deployed_timestamp(&self, entity_id: &str) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar(concat!(
            "SELECT ",
            local_ms_sql!("local_timestamp"),
            " FROM content.deployments WHERE entity_id = $1"
        ))
        .bind(entity_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// The deploy statement's newer-entity test, ahead of it: a deployment
    /// of the same type on any of the pointers that happened after `entity`.
    pub(crate) async fn has_newer_deployment(&self, entity: &Entity) -> sqlx::Result<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM content.deployments
                WHERE entity_type = $1 AND entity_pointers && $2
                  AND (entity_timestamp > to_timestamp($3 / 1000.0)
                       OR (entity_timestamp = to_timestamp($3 / 1000.0)
                           AND lower(entity_id) > lower($4))))",
        )
        .bind(entity.entity_type.as_str())
        .bind(&entity.pointers)
        .bind(entity.timestamp as f64)
        .bind(&entity.id)
        .fetch_one(&self.pool)
        .await
    }

    /// Seconds until the per-pointer deploy limit lets `pointers` through
    /// (at least 1); `None` when it does now.
    pub(crate) fn rate_limit_retry_after(&self, etype: &str, pointers: &[String]) -> Option<u64> {
        self.rate_limiter
            .retry_after(etype, pointers)
            .map(|d| d.as_secs_f64().ceil().max(1.0) as u64)
    }

    /// The chain checked against `entity_id` at `at_ms`, as the deploy
    /// validator checks it: `Unverifiable` for a contract wallet this node
    /// cannot ask about, `Rejected` with upstream's text otherwise.
    pub(crate) async fn check_signature(
        &self,
        entity_id: &str,
        auth_chain: &AuthChain,
        at_ms: i64,
    ) -> Result<(), DeployFailure> {
        match self.validate_signature(entity_id, auth_chain, at_ms).await {
            Ok(()) => Ok(()),
            Err(e @ (AuthError::Eip1654ValidationFailed(_) | AuthError::Eip1654NotImplemented)) => {
                Err(DeployFailure::Unverifiable(vec![format!(
                    "The signature is invalid. {e}"
                )]))
            }
            Err(e) => Err(DeployFailure::Rejected(
                RejectionKind::Validation,
                vec![format!("The signature is invalid. {e}")],
            )),
        }
    }

    /// The validation of one partial-upload batch: the content validator's
    /// staging subset (the signature is checked before). The access step is
    /// skipped when `include_access` is off.
    pub(crate) async fn validate_staging(
        &self,
        entity: &Entity,
        files: &HashMap<String, Bytes>,
        auth_chain: &AuthChain,
        include_access: bool,
    ) -> Result<(), DeployFailure> {
        match &self.validator {
            LocalValidation::Content(sources) => {
                let ctx = super::validator::Context {
                    sources,
                    auth_chain,
                };
                super::validator::validate_staging(entity, files, &ctx, include_access)
                    .await
                    .map_err(super::validator::into_deploy_failure)
            }
            #[cfg(any(test, feature = "test-support"))]
            LocalValidation::PartialChecks => {
                if include_access
                    && let Some(signer) = auth_chain.first().map(|link| link.payload.to_lowercase())
                {
                    self.validate_land_rights(entity, &signer).await?;
                }
                Ok(())
            }
        }
    }

    /// Forgets every per-pointer TTL: a route test deploys to one pointer
    /// more than once inside the 3-20 s windows.
    #[cfg(test)]
    pub(crate) fn clear_rate_limits(&self) {
        self.rate_limiter.clear();
    }

    #[cfg(test)]
    pub(crate) fn is_rate_limited(&self, etype: &str, pointer: &str) -> bool {
        self.rate_limiter
            .is_rate_limited(etype, &[pointer.to_lowercase()])
    }

    /// The old validator's LAND leg for a local scene: every pointer must be
    /// a parcel the signer may deploy to. A source that cannot decide
    /// (`land_authz::Unavailable`) is `Unavailable`, never a grant and never
    /// a refusal blamed on the depositor.
    #[cfg(any(test, feature = "test-support"))]
    async fn validate_land_rights(
        &self,
        entity: &Entity,
        signer: &str,
    ) -> Result<(), DeployFailure> {
        let Some(land) = self.land_authz.as_ref() else {
            return Ok(());
        };
        if entity.entity_type != EntityType::Scene {
            return Ok(());
        }
        let mut coords = Vec::with_capacity(entity.pointers.len());
        for pointer in &entity.pointers {
            let parsed = pointer.split_once(',').and_then(|(x, y)| {
                Some((x.trim().parse::<i32>().ok()?, y.trim().parse::<i32>().ok()?))
            });
            match parsed {
                Some(c) => coords.push(c),
                None => {
                    return Err(DeployFailure::Rejected(
                        RejectionKind::Validation,
                        vec![format!(
                            "Scene pointers must be x,y parcel coordinates. Invalid pointer: ({pointer})"
                        )],
                    ));
                }
            }
        }
        let decisions = crate::land_authz::can_deploy_batch(land, &coords, signer)
            .await
            .map_err(|e| {
                warn!(entity_id = %entity.id, error = %e, "LAND rights could not be checked");
                DeployFailure::Unavailable(vec![
                    super::validator::checker::PARCEL_ACCESS_UNDECIDED.to_string(),
                ])
            })?;
        let errors: Vec<String> = coords
            .iter()
            .zip(decisions.iter())
            .filter(|(_, d)| !d.verdict.allows())
            .map(|((x, y), _)| format!("The provided Eth Address does not have access to the following parcel: ({x},{y})"))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DeployFailure::Rejected(RejectionKind::Validation, errors))
        }
    }

    /// `Ok(None)` is provably absent, `Err` a storage fault: damage this node cannot read is
    /// never reported as a depositor's miss.
    #[cfg(any(test, feature = "test-support"))]
    async fn is_content_stored_already(
        &self,
        hashes: &[String],
    ) -> Result<HashMap<String, bool>, String> {
        let refs: Vec<&str> = hashes.iter().map(|h| h.as_str()).collect();
        self.storage
            .exist_multiple(&refs)
            .await
            .map_err(|e| e.to_string())
    }

    /// The ephemeral links are checked against the entity timestamp, as upstream does: a key
    /// that expired after signing still signed a valid deployment.
    async fn validate_signature(
        &self,
        entity_id: &str,
        auth_chain: &AuthChain,
        timestamp: i64,
    ) -> Result<(), AuthError> {
        verify_auth_chain_async(
            auth_chain,
            entity_id,
            Some(timestamp),
            self.eip1654.as_deref(),
        )
        .await
    }

    async fn validate(
        &self,
        entity: &Entity,
        files: &HashMap<String, Bytes>,
        auth_chain: &AuthChain,
        _context: &str,
    ) -> Result<(), DeployFailure> {
        match &self.validator {
            LocalValidation::Content(sources) => {
                let ctx = super::validator::Context {
                    sources,
                    auth_chain,
                };
                super::validator::validate(entity, files, &ctx)
                    .await
                    .map_err(super::validator::into_deploy_failure)
            }
            #[cfg(any(test, feature = "test-support"))]
            LocalValidation::PartialChecks => {
                // The chain was checked before the server checks.
                if _context == "LOCAL"
                    && let Some(signer) = auth_chain.first().map(|link| link.payload.to_lowercase())
                {
                    self.validate_land_rights(entity, &signer).await?;
                }

                let not_uploaded: Vec<String> = entity
                    .content
                    .iter()
                    .map(|c| c.hash.clone())
                    .filter(|hash| !files.contains_key(hash))
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                if !not_uploaded.is_empty() {
                    let stored = self
                        .is_content_stored_already(&not_uploaded)
                        .await
                        .map_err(|e| DeployFailure::Unavailable(vec![e]))?;
                    let mut missing: Vec<String> = not_uploaded
                        .into_iter()
                        .filter(|hash| !stored.get(hash).copied().unwrap_or(false))
                        .collect();
                    if !missing.is_empty() {
                        missing.sort();
                        return Err(DeployFailure::Rejected(
                            RejectionKind::Validation,
                            vec![format!(
                                "These content files are neither uploaded nor already stored: {}",
                                missing.join(", ")
                            )],
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    async fn is_content_unchanged(&self, entity: &Entity) -> bool {
        let Some(pointer) = entity.pointers.first() else {
            return false;
        };
        let row: Option<Option<Value>> = sqlx::query_scalar(
            r#"
            SELECT d.entity_metadata
            FROM content.active_pointers ap
            JOIN content.deployments d ON d.entity_id = ap.entity_id
            WHERE ap.pointer = $1
            "#,
        )
        .bind(pointer.to_lowercase())
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();
        let stored = row.flatten();
        metadata_unchanged(stored.as_ref(), entity.metadata.as_ref())
    }
}

/// How long a deploy's write waits for an unlink batch's exclusive store lock.
const STORE_LOCK_WAIT: &str = "5s";

/// The most parcels a scene may name: every parcel of the -150..=150 map.
pub(crate) const MAX_SCENE_POINTERS: usize = 301 * 301;
/// An early bound for the other types, which the validator holds to one pointer.
pub(crate) const MAX_OTHER_POINTERS: usize = 100;

/// The ones of `pointers` in `held`, in the entity's order.
fn held_of(held: &HashSet<String>, pointers: &[String]) -> Vec<String> {
    pointers
        .iter()
        .filter(|p| held.contains(*p))
        .cloned()
        .collect()
}

/// `REQUEST_TTL_BACKWARDS`'s default.
pub(crate) const REQUEST_TTL_BACKWARDS_MS: i64 = 20 * 60 * 1000;

/// One deploy's hold on its pointers ([`WriteDeployer::claim_pointers`]),
/// released on drop.
struct PointerClaim<'a> {
    deployer: &'a WriteDeployer,
    etype: String,
    pointers: Vec<String>,
}

impl Drop for PointerClaim<'_> {
    fn drop(&mut self) {
        let mut in_flight = self.deployer.pointers_in_flight.lock();
        if let Some(held) = in_flight.get_mut(&self.etype) {
            for p in &self.pointers {
                held.remove(p);
            }
        }
    }
}

/// ` (entityId=<id> pointers=<p,..>)`, the tail upstream's server checks
/// put before their final period.
fn entity_suffix(entity: &Entity) -> String {
    format!(
        " (entityId={} pointers={})",
        entity.id,
        entity.pointers.join(",")
    )
}

/// Upstream's refusal of an entity a newer one on its pointers overtook.
pub(crate) fn newer_entity_message(entity: &Entity) -> String {
    format!(
        "There is a newer entity pointed by one or more of the pointers you provided{}.",
        entity_suffix(entity)
    )
}

pub(crate) const REQUEST_TTL_FORWARDS_MS: i64 = 15 * 60 * 1000;

impl WriteDeployer {
    /// `context` is `"LOCAL"` for a deploy against this node's write API (a local scene then
    /// gets its `local_entities` provenance row); anything else is a mirrored write.
    pub async fn deploy_entity(
        &self,
        files: Vec<Bytes>,
        entity_id: &str,
        auth_chain: Value,
        context: &str,
    ) -> Result<i64, DeployFailure> {
        self.deploy_entity_anchored(files, entity_id, auth_chain, context, None)
            .await
    }

    /// [`Self::deploy_entity`] with the request TTLs measured from
    /// `ttl_anchor_ms` instead of now: a partial upload's publication
    /// measures them from the upload's first batch.
    pub(crate) async fn deploy_entity_anchored(
        &self,
        files: Vec<Bytes>,
        entity_id: &str,
        auth_chain: Value,
        context: &str,
        ttl_anchor_ms: Option<i64>,
    ) -> Result<i64, DeployFailure> {
        let mut deployed = self.deploying.lock(entity_id).await;
        if let Some(creation_ts) = *deployed {
            return Ok(creation_ts);
        }
        let result = self
            .deploy_entity_unlocked(files, entity_id, auth_chain, context, ttl_anchor_ms)
            .await;
        if let Ok(creation_ts) = result {
            *deployed = Some(creation_ts);
        }
        result
    }

    async fn deploy_entity_unlocked(
        &self,
        files: Vec<Bytes>,
        entity_id: &str,
        auth_chain: Value,
        context: &str,
        ttl_anchor_ms: Option<i64>,
    ) -> Result<i64, DeployFailure> {
        let started = Instant::now();
        let uploads = hash_uploads(files)
            .await
            .map_err(|e| deploy_error(entity_id, &e))?;
        let hash_us = started.elapsed().as_micros() as u64;
        let entity_bytes = uploads
            .iter()
            .find(|upload| upload.v1 == entity_id || upload.v0 == entity_id)
            .map(|upload| upload.bytes.clone())
            .ok_or_else(|| vec!["Failed to find the entity file.".to_string()])?;

        // Upstream answers the bare sentence; the parser's text is logged.
        let mut entity: Entity = serde_json::from_slice(&entity_bytes).map_err(|e| {
            info!(entity_id, error = %e, "deployment refused: unparsable entity file");
            vec!["There was a problem parsing the entity".to_string()]
        })?;
        for pointer in &mut entity.pointers {
            *pointer = pointer.to_lowercase();
        }
        // The id is the request's, as upstream's parser takes it: the file
        // hashes to it, whatever `id` field it carries.
        entity.id = entity_id.to_string();

        // A dcl-one refusal upstream does not make: the operator denylist
        // keeps a listed id, hash or pointer off this node.
        if let Some(errors) = self
            .denylist
            .as_deref()
            .and_then(|d| super::denylist::refusal(&entity, d))
        {
            warn!(entity_id, ?errors, "deployment refused: denylisted");
            return Err(DeployFailure::Rejected(RejectionKind::Validation, errors));
        }

        let auth_chain: AuthChain = serde_json::from_value(auth_chain)
            .map_err(|e| vec![format!("invalid auth chain: {e}")])?;
        // The signature at the entity's timestamp, the pointers and the
        // server checks (newer entity, rate limit, request TTLs), then the
        // validator; the in-flight claim follows validation.
        self.check_signature(entity_id, &auth_chain, entity.timestamp)
            .await?;
        if entity.pointers.is_empty() {
            return Err(vec!["The entity does not have any pointer.".to_string()].into());
        }
        let in_flight = |busy: Vec<String>| {
            DeployFailure::Rejected(
                RejectionKind::PointerContention,
                vec![format!(
                    "The following pointers are currently being deployed: '{}'. Please try again in a few seconds.",
                    busy.join(",")
                )],
            )
        };
        let max_pointers = if entity.entity_type == EntityType::Scene {
            MAX_SCENE_POINTERS
        } else {
            MAX_OTHER_POINTERS
        };
        if entity.pointers.len() > max_pointers {
            return Err(vec![format!(
                "A {} may name at most {max_pointers} pointers.",
                entity.entity_type.as_str()
            )]
            .into());
        }
        // Upstream's early refusal, without taking a claim.
        let busy = self.busy_pointers(entity.entity_type.as_str(), &entity.pointers);
        if !busy.is_empty() {
            return Err(in_flight(busy));
        }

        // An early refusal, as upstream's; the write's own check is binding,
        // so a failed lookup goes on to it.
        match self.has_newer_deployment(&entity).await {
            Ok(true) => {
                return Err(DeployFailure::Rejected(
                    RejectionKind::Superseded,
                    vec![newer_entity_message(&entity)],
                ));
            }
            Ok(false) => {}
            Err(e) => warn!(entity_id, error = %e, "newer-entity lookup failed"),
        }
        let pointers_lc = &entity.pointers;
        let rate_limited_msg = || {
            vec![format!(
                "Entity rate limited (entityId={} pointers={}).",
                entity.id,
                entity.pointers.join(",")
            )]
        };
        // An early refusal; the reservation after validation is the binding check.
        if self
            .rate_limiter
            .is_rate_limited(entity.entity_type.as_str(), pointers_lc)
        {
            return Err(DeployFailure::Rejected(
                RejectionKind::RateLimited,
                rate_limited_msg(),
            ));
        }
        let content_unchanged =
            entity.entity_type == EntityType::Profile && self.is_content_unchanged(&entity).await;
        if content_unchanged && self.rate_limiter.is_unchanged_limited(pointers_lc) {
            return Err(DeployFailure::Rejected(
                RejectionKind::RateLimited,
                rate_limited_msg(),
            ));
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        if ttl_anchor_ms.unwrap_or(now_ms) - entity.timestamp > self.request_ttl_backwards_ms() {
            return Err(vec![format!(
                "The request is not recent enough, please submit it again with a new timestamp{}.",
                entity_suffix(&entity)
            )]
            .into());
        }
        if now_ms - entity.timestamp < -REQUEST_TTL_FORWARDS_MS {
            return Err(vec![format!(
                "The request is too far in the future, please submit it again with a new timestamp{}.",
                entity_suffix(&entity)
            )]
            .into());
        }

        let declared: HashSet<&str> = entity
            .content
            .iter()
            .map(|c| c.hash.as_str())
            .chain(std::iter::once(entity.id.as_str()))
            .collect();
        let mut vfiles: HashMap<String, Bytes> = HashMap::new();
        for upload in &uploads {
            let key = if declared.contains(upload.v1.as_str()) {
                &upload.v1
            } else if declared.contains(upload.v0.as_str()) {
                &upload.v0
            } else {
                &upload.v1
            };

            if let Some(prev) = vfiles.insert(key.clone(), upload.bytes.clone())
                && prev != upload.bytes
            {
                return Err(vec![format!(
                    "two different uploaded files map to the same content hash {key}"
                )]
                .into());
            }
        }

        let validation_started = Instant::now();
        match self.validate(&entity, &vfiles, &auth_chain, context).await {
            Ok(()) => {}
            Err(DeployFailure::Rejected(kind, errors)) => {
                warn!(entity_id, ?errors, "deployment rejected by validation");
                return Err(DeployFailure::Rejected(kind, errors));
            }
            Err(DeployFailure::Unavailable(errors)) => {
                warn!(
                    entity_id,
                    ?errors,
                    "deployment undecidable; this node is damaged"
                );
                return Err(DeployFailure::Unavailable(errors));
            }
            Err(DeployFailure::Unverifiable(errors)) => {
                warn!(
                    entity_id,
                    ?errors,
                    "deployment signature could not be checked"
                );
                return Err(DeployFailure::Unverifiable(errors));
            }
        }

        let validation_us = validation_started.elapsed().as_micros() as u64;
        // Claimed once validation passed, so a deploy without rights never holds a pointer;
        // the deploy function's per-pointer locks stay the binding serializer.
        let _claim = self
            .claim_pointers(entity.entity_type.as_str(), &entity.pointers)
            .map_err(in_flight)?;
        let persist_started = Instant::now();
        // Released on a failed or no-op write, so only an accepted deploy holds its pointers.
        let Some(reservation) =
            self.rate_limiter
                .reserve(entity.entity_type.as_str(), pointers_lc, content_unchanged)
        else {
            return Err(DeployFailure::Rejected(
                RejectionKind::RateLimited,
                rate_limited_msg(),
            ));
        };
        let persisted = match self
            .persist(&entity, &entity_bytes, &uploads, &auth_chain, context)
            .await
        {
            Ok(persisted) => persisted,
            Err(e) => {
                reservation.release();
                return Err(e);
            }
        };
        if !persisted.inserted {
            reservation.release();
        }

        let persist_us = persist_started.elapsed().as_micros() as u64;
        let cleanup_started = Instant::now();
        drop(vfiles);
        drop(uploads);
        tracing::debug!(target: "catalyst_perf", phase="deploy", entity_id, hash_us, validation_us, persist_us, cleanup_us=cleanup_started.elapsed().as_micros() as u64, elapsed_us=started.elapsed().as_micros() as u64);
        Ok(persisted.creation_ts)
    }
}

struct Persisted {
    /// The stored `local_timestamp` (ms), as the history routes report it.
    creation_ts: i64,
    /// `false` when the entity was already deployed.
    inserted: bool,
}

#[derive(sqlx::FromRow)]
struct PersistOutcome {
    dep_id: Option<i32>,
    newer: bool,
    overwrote: i32,
    cleared: i32,
    /// The statement's `now()`, the `local_timestamp` an insert stores, in
    /// ms ([`LOCAL_MS_SQL`]); absent from the bare CTE the tests run.
    #[sqlx(default)]
    local_ms: i64,
}

/// The deploy call's `now()`: one statement is one transaction, so it is the
/// `now()` the function stored as the deployment's `local_timestamp`.
const LOCAL_MS_SQL: &str = local_ms_sql!("now()");

/// The whole write as one statement off the pointer lock: the newer-entity check and the
/// overwrite calculation read the locked snapshot, the deployment insert is gated on the former,
/// and every dependent write (provenance, content files, pointer upsert, pointer clears,
/// overwritten marks) hangs off the inserted row, so a conflict or a newer entity makes them all
/// no-ops.
fn persist_statement(local_provenance: bool) -> String {
    let provenance = if local_provenance {
        "
        provenance AS (
            INSERT INTO content.local_entities (entity_id, signer)
            SELECT $4, $11 FROM ins
            ON CONFLICT (entity_id) DO NOTHING
        ),"
    } else {
        ""
    };
    format!(
        r#"
        WITH newer AS (
            SELECT 1 AS present FROM content.deployments
            WHERE entity_type = $3
              AND entity_pointers && $7
              AND (entity_timestamp > to_timestamp($6 / 1000.0)
                   OR (entity_timestamp = to_timestamp($6 / 1000.0) AND lower(entity_id) > lower($4)))
            LIMIT 1
        ),
        overwrote AS (
            SELECT dep1.id, dep1.entity_pointers
            FROM content.deployments AS dep1
            LEFT JOIN content.deployments AS dep2 ON dep1.deleter_deployment = dep2.id
            WHERE dep1.entity_type = $3
              AND dep1.entity_pointers && $7
              AND (dep1.entity_timestamp < to_timestamp($6 / 1000.0)
                   OR (dep1.entity_timestamp = to_timestamp($6 / 1000.0) AND lower(dep1.entity_id) < lower($4)))
              AND (dep2.id IS NULL
                   OR dep2.entity_timestamp > to_timestamp($6 / 1000.0)
                   OR (dep2.entity_timestamp = to_timestamp($6 / 1000.0) AND lower(dep2.entity_id) > lower($4)))
        ),
        ins AS (
            INSERT INTO content.deployments
                (deployer_address, version, entity_type, entity_id, entity_metadata,
                 entity_timestamp, entity_pointers, local_timestamp, auth_chain)
            SELECT $1, $2, $3, $4, $5, to_timestamp($6 / 1000.0), $7, now(), $8
            WHERE NOT EXISTS (SELECT 1 FROM newer)
            ON CONFLICT (entity_id) DO NOTHING
            RETURNING id
        ),{provenance}
        files AS (
            INSERT INTO content.content_files (deployment, content_hash, key)
            SELECT ins.id, c.hash, c.key
            FROM ins, unnest($9::text[], $10::text[]) AS c(hash, key)
        ),
        pointed AS (
            INSERT INTO content.active_pointers (pointer, entity_id, entity_type)
            SELECT p.pointer, $4, $3 FROM ins, unnest($7::text[]) AS p(pointer)
            ON CONFLICT (pointer) DO UPDATE
                SET entity_id = EXCLUDED.entity_id, entity_type = EXCLUDED.entity_type
                WHERE NOT EXISTS (
                    SELECT 1 FROM content.deployments cur
                    WHERE cur.entity_id = active_pointers.entity_id
                      AND (cur.entity_timestamp > to_timestamp($6 / 1000.0)
                           OR (cur.entity_timestamp = to_timestamp($6 / 1000.0)
                               AND lower(cur.entity_id) > lower(EXCLUDED.entity_id)))
                )
        ),
        cleared AS (
            DELETE FROM content.active_pointers
            WHERE EXISTS (SELECT 1 FROM ins)
              AND pointer IN (SELECT unnest(entity_pointers) FROM overwrote)
              AND NOT (pointer = ANY($7))
            RETURNING pointer
        ),
        overwritten AS (
            UPDATE content.deployments SET deleter_deployment = ins.id
            FROM ins
            WHERE deployments.id IN (SELECT id FROM overwrote)
            RETURNING deployments.id
        )
        SELECT (SELECT id FROM ins) AS dep_id,
               EXISTS (SELECT 1 FROM newer) AS newer,
               (SELECT count(*) FROM overwritten)::int AS overwrote,
               (SELECT count(*) FROM cleared)::int AS cleared
        "#
    )
}

/// The same write as one function call. The pointer lock is the first statement of the body
/// and the CTE the second, so the CTE's snapshot follows the lock; a single client statement
/// takes its snapshot before it can block on the lock, which is why the transaction shape
/// needed BEGIN, lock, CTE, COMMIT. The name is a hash of the body: a binary always calls the
/// function that matches its own SQL, and two binaries mid-rollout coexist in one database.
/// Positional `$n` inside plpgsql are the function's parameters, so the CTE is embedded as is.
/// The function lives in schema `content` next to the tables it writes.
pub(crate) fn deploy_function_name(local_provenance: bool) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(deploy_function_body(local_provenance));
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("deploy_entity_{}", &hex[..12])
}

/// The locks in byte order (`COLLATE "C"`), the order `lock_deployment_pointers`
/// sorts in Rust: under the database's own collation (`en_US.UTF-8` on the
/// lab and the deploy tree) `0,10` sorts before `0,-2` and `-1,0` after
/// `0,1`, and two writers taking overlapping locks in two orders can
/// deadlock.
fn deploy_function_body(local_provenance: bool) -> String {
    format!(
        "#variable_conflict use_column\nBEGIN\n\
         PERFORM pg_advisory_xact_lock(hashtext(k)) FROM (SELECT k FROM (SELECT DISTINCT unnest($7) AS k) d ORDER BY k COLLATE \"C\") locks;\n\
         RETURN QUERY{};\nEND",
        persist_statement(local_provenance)
    )
}

const DEPLOY_FUNCTION_ARGS: &str =
    "text, text, text, text, json, double precision, text[], json, text[], text[], text";

/// The `to_regprocedure` spelling the deployer checks before its first call.
pub(crate) fn deploy_function_signature(local_provenance: bool) -> String {
    format!(
        "content.{}({DEPLOY_FUNCTION_ARGS})",
        deploy_function_name(local_provenance)
    )
}

/// `schemas/00_content.sql` carries the `or_replace = false` form
/// (`land_publish::content_functions_sql`).
pub(crate) fn deploy_function_sql(local_provenance: bool, or_replace: bool) -> String {
    format!(
        "CREATE {}FUNCTION content.{}({DEPLOY_FUNCTION_ARGS}) \
         RETURNS TABLE (dep_id integer, newer boolean, overwrote integer, cleared integer) \
         LANGUAGE plpgsql AS $deploy$ {} $deploy$",
        if or_replace { "OR REPLACE " } else { "" },
        deploy_function_name(local_provenance),
        deploy_function_body(local_provenance)
    )
}

pub(crate) fn deploy_function_call(name: &str) -> String {
    // Keep the shared JSONB validity boundary without normalizing stored property order.
    format!(
        "SELECT dep_id, newer, overwrote, cleared, {LOCAL_MS_SQL} AS local_ms FROM content.{name}($1, $2, $3, $4, CASE WHEN $5::text::jsonb IS NOT NULL THEN $5::text::json ELSE NULL END, $6, $7, $8::json, $9, $10, $11)"
    )
}

/// [`deploy_function_call`] with the forwarding outbox row (`$12` the target)
/// in the same statement: the row exists exactly when the call inserted the
/// deployment, commits or rolls back with it, and the function, its locks and
/// its hash-derived name are unchanged.
pub(crate) fn forward_call(name: &str) -> String {
    format!(
        "WITH d AS MATERIALIZED ({}), \
              queued AS (INSERT INTO content.forward_outbox (entity_id, target) \
                         SELECT $4, $12 FROM d WHERE d.dep_id IS NOT NULL AND NOT d.newer \
                         ON CONFLICT (entity_id) DO NOTHING) \
         SELECT dep_id, newer, overwrote, cleared, local_ms FROM d",
        deploy_function_call(name)
    )
}

impl WriteDeployer {
    async fn persist(
        &self,
        entity: &Entity,
        entity_bytes: &Bytes,
        uploads: &[HashedUpload],
        auth_chain: &AuthChain,
        context: &str,
    ) -> Result<Persisted, DeployFailure> {
        let unavailable = |e: String| deploy_error(&entity.id, &e);
        let metadata = crate::entity::deployment_metadata(
            entity_bytes,
            crate::entity::MetadataInput::LocalEntity,
        )
        .and_then(|metadata| serde_json::to_string(&metadata))
        .map_err(|e| unavailable(e.to_string()))?;
        let started = Instant::now();
        store_deployment_files(
            &self.storage,
            deployment_files(entity, entity_bytes, uploads),
        )
        .await
        .map_err(unavailable)?;

        let storage_us = started.elapsed().as_micros() as u64;
        let db_started = Instant::now();
        let deployer_address = auth_chain
            .first()
            .map(|l| l.payload.clone())
            .unwrap_or_default();
        let pointers: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            entity
                .pointers
                .iter()
                .map(|p| p.to_lowercase())
                .filter(|p| seen.insert(p.clone()))
                .collect()
        };
        let auth_chain_json =
            serde_json::to_value(auth_chain).map_err(|e| unavailable(e.to_string()))?;
        let hashes: Vec<String> = entity.content.iter().map(|c| c.hash.clone()).collect();
        let keys: Vec<String> = entity.content.iter().map(|c| c.file.clone()).collect();

        let local_provenance = context == "LOCAL" && entity.entity_type == EntityType::Scene;
        // The schema installs the function (the runtime role cannot); the
        // first call checks the name this binary computes is there.
        let deploy_fn = self.deploy_fns[local_provenance as usize]
            .get_or_try_init(|| async {
                super::land_publish::require_function(
                    &self.pool,
                    &deploy_function_signature(local_provenance),
                )
                .await?;
                Ok::<_, String>(deploy_function_name(local_provenance))
            })
            .await
            .map_err(unavailable)?;

        // Only a deploy against this node's own write API is forwarded.
        let forward = self.forwarder.as_ref().filter(|_| context == "LOCAL");
        let call = match forward {
            Some(_) => forward_call(deploy_fn),
            None => deploy_function_call(deploy_fn),
        };
        let mut query = sqlx::query_as::<_, PersistOutcome>(sqlx::AssertSqlSafe(call))
            .bind(&deployer_address)
            .bind(&entity.version)
            .bind(entity.entity_type.as_str())
            .bind(&entity.id)
            .bind(&metadata)
            .bind(entity.timestamp as f64)
            .bind(&pointers)
            .bind(&auth_chain_json)
            .bind(&hashes)
            .bind(&keys)
            .bind(deployer_address.to_lowercase());
        if let Some(forwarder) = forward {
            query = query.bind(forwarder.target());
        }
        let write_failed = |e: sqlx::Error| unavailable(format!("deployment write failed: {e}"));
        let mut tx = self.pool.begin().await.map_err(write_failed)?;
        // The collector unlinks under the exclusive side: every named file stays until commit.
        // Bounded, so an unlink batch delays deploys briefly instead of queueing them.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SET LOCAL lock_timeout = '{STORE_LOCK_WAIT}'"
        )))
        .execute(&mut *tx)
        .await
        .map_err(write_failed)?;
        crate::storage::gc::lock_store_shared(&mut tx)
            .await
            .map_err(|e| unavailable(format!("the content store is being collected: {e}")))?;
        sqlx::query("SET LOCAL lock_timeout TO DEFAULT")
            .execute(&mut *tx)
            .await
            .map_err(write_failed)?;
        let named: Vec<&str> = hashes
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let stored = self
            .storage
            .exist_multiple(&named)
            .await
            .map_err(|e| unavailable(format!("checking stored content: {e}")))?;
        let mut gone: Vec<&str> = named
            .into_iter()
            .filter(|h| !stored.get(*h).copied().unwrap_or(false))
            .collect();
        if !gone.is_empty() {
            gone.sort_unstable();
            // Validation saw them: a collection raced the deploy, so a retry re-uploads them.
            return Err(DeployFailure::Unavailable(vec![format!(
                "Content files were removed from the store during the deployment, please retry: {}",
                gone.join(", ")
            )]));
        }
        let outcome = query.fetch_one(&mut *tx).await.map_err(write_failed)?;
        tx.commit().await.map_err(write_failed)?;

        tracing::debug!(target: "catalyst_perf", phase="persist", entity_id=%entity.id, storage_us, db_us=db_started.elapsed().as_micros() as u64);

        if outcome.newer {
            return Err(DeployFailure::Rejected(
                RejectionKind::Superseded,
                vec![newer_entity_message(entity)],
            ));
        }
        let Some(dep_id) = outcome.dep_id else {
            info!(entity_id = %entity.id, "entity already deployed; treating as success");
            let stored = self
                .deployed_timestamp(&entity.id)
                .await
                .map_err(|e| unavailable(format!("reading the stored deployment: {e}")))?;
            return Ok(Persisted {
                creation_ts: stored.unwrap_or(outcome.local_ms),
                inserted: false,
            });
        };
        info!(
            entity_id = %entity.id,
            dep_id,
            overwrote = outcome.overwrote,
            cleared = outcome.cleared,
            forwarded = forward.is_some(),
            "deployment committed"
        );
        if let Some(forwarder) = forward {
            forwarder.kick();
        }
        Ok(Persisted {
            creation_ts: outcome.local_ms,
            inserted: true,
        })
    }
}

/// Transaction-scoped advisory locks on every pointer a write touches, taken in one sorted
/// order so two writers over overlapping pointer sets cannot deadlock. The sync batch flush
/// takes these before its inserts; the deploy function takes the same locks in its body.
pub async fn lock_deployment_pointers(
    conn: &mut sqlx::PgConnection,
    pointers: &[String],
) -> Result<(), sqlx::Error> {
    let mut keys = pointers.to_vec();
    keys.sort_unstable();
    keys.dedup();
    sqlx::query(
        r#"SELECT pg_advisory_xact_lock(hashtext(pointer))
           FROM (SELECT pointer FROM unnest($1::text[]) WITH ORDINALITY AS p(pointer, ordinal)
                 ORDER BY ordinal) ordered"#,
    )
    .bind(&keys)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
