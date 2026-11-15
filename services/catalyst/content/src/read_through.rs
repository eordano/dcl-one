//! Read-through (`READ_THROUGH_ENABLED`): a read that misses locally asks the sync sources
//! (`SYNC_SOURCE`, in order) and keeps a verified copy. An entity goes through the sync write
//! path: its file must hash to its id and the entity, under its auth chain (the peer's
//! `/audit`), must pass the content validator sync runs before anything is stored; its content
//! files are hash-verified as sync downloads them, the validator runs again over them (a refusal
//! then removes the entity file; so does an undecided verdict, recorded for the retry worker, which runs with sync off too), and [`LiveSyncDeployer`] writes the row as a synced
//! deployment (the newest timestamp wins, nothing is marked local). What sync skips is skipped here too: entity types
//! outside `SYNC_ENTITY_TYPES` (only `SYNC_PROFILE_MAX_AGE_DAYS` is not applied, so an old
//! profile is still found). A content file is stored once it hashes to its name, a profile's
//! avatar renders included; a `/contents/{hash}` miss is filled the same way, so a file this
//! node lists but lost (or synced before it stored renders) is fetched on its first read.
//!
//! [`Fills`] bounds it: one fill per key at a time (a second reader waits on the first), a
//! fixed number of fills at once (a reader beyond that answers the miss), a reader waits at
//! most the guard (the fill keeps going and a later read finds the copy), and a key the sources
//! could not fill is not asked for again for [`NEGATIVE_TTL`]. [`Downloads`] bounds what every
//! fill fetches at once, a world's too: files in flight and the bytes their bodies hold. Whatever
//! goes wrong upstream, the reader answers what a miss answers.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::PgPool;
#[cfg(any(test, feature = "test-support"))]
use tokio::sync::Notify;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, watch};
use tokio::time::Instant;

use super::deploy_remote_entity::{
    BufferSink, ByteBudget, ByteReservation, ContentSource, RemoteDeployment,
    deploy_entity_streaming, fetch_verified, stream_body,
};
use super::failed_deployments_store::LiveFailedDeploymentsStore;
use super::queries::{self, EntityDoc};
use super::sync::{parse_entity_types, parse_sync_sources};
use super::sync_deployer::{LiveSyncDeployer, SyncValidation, is_buffered, is_entity_deployed};
use super::sync_types::{AuthChain, DeploymentContext, FailedDeployment, FailureReason, SyncError};
use super::validator::{Sources, SyncedContent};
use crate::auth::is_canonical_cid;
use crate::storage::{Denylist, Storage};

/// How long a key the sources could not fill stays unasked.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(60);
/// The longest a reader waits on a fill before it answers the miss.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(8);
/// Fills running at once.
pub const MAX_CONCURRENT_FILLS: usize = 16;
/// The hang guard of one fill, a scene's content files included.
const FILL_DEADLINE: Duration = Duration::from_secs(300);
const NEGATIVE_MAX_ENTRIES: usize = 100_000;
/// Keys longer than this enter the negative cache as their SHA-256, so its
/// memory stays bounded by entry count: upstream caps neither the length of
/// a pointer nor of an id, and a 10 MiB `/entities/active` body can name
/// 1000 unknown 10 KB pointers.
const NEGATIVE_KEY_MAX_BYTES: usize = 128;
/// Files every fill together fetches at once.
pub const MAX_CONCURRENT_DOWNLOADS: usize = 16;
const FLUSH_CONCURRENCY: usize = 2;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;

/// What [`Fills`] has done, for the status log and for tests to wait on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FillCounts {
    /// Fills spawned.
    pub started: u64,
    /// Readers that waited on a fill another reader started.
    pub joined: u64,
    /// Fills that ended, filled or not.
    pub finished: u64,
    /// Readers that answered the miss because every fill slot was taken.
    pub saturated: u64,
}

#[derive(Default)]
struct Counters {
    started: AtomicU64,
    joined: AtomicU64,
    finished: AtomicU64,
    saturated: AtomicU64,
}

struct FillsInner {
    negative: Mutex<HashMap<String, Instant>>,
    inflight: Mutex<HashMap<String, watch::Receiver<bool>>>,
    permits: Arc<Semaphore>,
    wait: Duration,
    negative_ttl: Duration,
    counters: Counters,
    #[cfg(any(test, feature = "test-support"))]
    changed: Notify,
}

impl FillsInner {
    fn bump(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::SeqCst);
        #[cfg(any(test, feature = "test-support"))]
        self.changed.notify_waiters();
    }
}

/// The negative cache's key for `key` ([`NEGATIVE_KEY_MAX_BYTES`]).
fn negative_key(key: &str) -> String {
    if key.len() <= NEGATIVE_KEY_MAX_BYTES {
        return key.to_string();
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Single-flight, bounded, negatively cached fills keyed by string.
#[derive(Clone)]
pub struct Fills {
    inner: Arc<FillsInner>,
}

/// Ends a fill even when its future panics: the keys leave the in-flight map and enter the
/// negative cache, and the waiters wake.
struct FillDone {
    inner: Arc<FillsInner>,
    keys: Vec<String>,
    done: watch::Sender<bool>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for FillDone {
    fn drop(&mut self) {
        let now = Instant::now();
        {
            let mut negative = self.inner.negative.lock();
            if negative.len() + self.keys.len() > NEGATIVE_MAX_ENTRIES {
                let ttl = self.inner.negative_ttl;
                negative.retain(|_, at| now.duration_since(*at) < ttl);
                if negative.len() + self.keys.len() > NEGATIVE_MAX_ENTRIES {
                    negative.clear();
                }
            }
            let mut inflight = self.inner.inflight.lock();
            for key in &self.keys {
                negative.insert(negative_key(key), now);
                inflight.remove(key);
            }
        }
        let _ = self.done.send(true);
        self.inner.bump(&self.inner.counters.finished);
    }
}

impl Fills {
    pub fn new(concurrency: usize, wait: Duration) -> Self {
        Self::with_negative_ttl(concurrency, wait, NEGATIVE_TTL)
    }

    /// Tests give the negative cache a lifetime longer than any run, so no assertion depends
    /// on how long the run took (the expiry itself is tested under paused time).
    pub fn with_negative_ttl(concurrency: usize, wait: Duration, negative_ttl: Duration) -> Self {
        Self {
            inner: Arc::new(FillsInner {
                negative: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                permits: Arc::new(Semaphore::new(concurrency.max(1))),
                wait,
                negative_ttl,
                counters: Counters::default(),
                #[cfg(any(test, feature = "test-support"))]
                changed: Notify::new(),
            }),
        }
    }

    pub fn counts(&self) -> FillCounts {
        let c = &self.inner.counters;
        FillCounts {
            started: c.started.load(Ordering::SeqCst),
            joined: c.joined.load(Ordering::SeqCst),
            finished: c.finished.load(Ordering::SeqCst),
            saturated: c.saturated.load(Ordering::SeqCst),
        }
    }

    /// Resolves once `done` holds for the counts; tests wait on fills with it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn until(&self, done: impl Fn(FillCounts) -> bool) {
        loop {
            let changed = self.inner.changed.notified();
            if done(self.counts()) {
                return;
            }
            changed.await;
        }
    }

    /// Joins the fills already running for some of `keys`, starts one `fill` for the rest
    /// (minus the negatively cached ones), and waits for all of them up to the guard. `fill`
    /// gets the keys it owns. `false` when nothing was started or joined, so the caller's
    /// miss stands without a second read.
    pub async fn fill<F, Fut>(&self, keys: Vec<String>, fill: F) -> bool
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.fill_within(keys, self.inner.wait, fill).await
    }

    /// [`fill`](Self::fill) with the reader's own guard instead of the default one.
    pub async fn fill_within<F, Fut>(&self, keys: Vec<String>, wait: Duration, fill: F) -> bool
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let inner = &self.inner;
        let mut waits: Vec<watch::Receiver<bool>> = Vec::new();
        let mut joined = false;
        let mut started = None;
        {
            let now = Instant::now();
            let negative = inner.negative.lock();
            let mut inflight = inner.inflight.lock();
            let mut own = Vec::new();
            let mut seen = HashSet::new();
            for key in keys {
                if !seen.insert(key.clone()) {
                    continue;
                }
                if let Some(running) = inflight.get(&key) {
                    waits.push(running.clone());
                    joined = true;
                } else if !negative
                    .get(&negative_key(&key))
                    .is_some_and(|at| now.duration_since(*at) < inner.negative_ttl)
                {
                    own.push(key);
                }
            }
            if !own.is_empty() {
                match inner.permits.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let (done, running) = watch::channel(false);
                        for key in &own {
                            inflight.insert(key.clone(), running.clone());
                        }
                        waits.push(running);
                        started = Some((
                            FillDone {
                                inner: inner.clone(),
                                keys: own.clone(),
                                done,
                                _permit: permit,
                            },
                            own,
                        ));
                    }
                    Err(_) => inner.bump(&inner.counters.saturated),
                }
            }
        }
        if joined {
            inner.bump(&inner.counters.joined);
        }
        if let Some((done, own)) = started {
            inner.bump(&inner.counters.started);
            let work = fill(own);
            tokio::spawn(async move {
                let _done = done;
                if tokio::time::timeout(FILL_DEADLINE, work).await.is_err() {
                    tracing::warn!("read-through fill exceeded its deadline; the miss stands");
                }
            });
        }
        if waits.is_empty() {
            return false;
        }
        // `wait_for`'s answer borrows the channel; only whether it ended is kept.
        let all = futures::future::join_all(
            waits
                .iter_mut()
                .map(|w| async move { w.wait_for(|done| *done).await.is_ok() }),
        );
        let _ = tokio::time::timeout(wait, all).await;
        true
    }
}

/// The download bound every fill shares, a world's included: a slot per file in flight and,
/// once its size is known, a hold on the byte budget of the read-through's deployer (the one a
/// scene's content files draw on), released when the slot drops.
#[derive(Clone)]
pub struct Downloads {
    files: Arc<Semaphore>,
    bytes: Arc<ByteBudget>,
    waited: Arc<AtomicU64>,
    #[cfg(any(test, feature = "test-support"))]
    changed: Arc<Notify>,
}

/// One file's place in [`Downloads`].
pub struct DownloadSlot {
    _file: OwnedSemaphorePermit,
    bytes: Arc<ByteBudget>,
    held: Option<ByteReservation>,
}

impl Downloads {
    pub fn new(files: usize, bytes: Arc<ByteBudget>) -> Self {
        Self {
            files: Arc::new(Semaphore::new(files.max(1))),
            bytes,
            waited: Arc::new(AtomicU64::new(0)),
            #[cfg(any(test, feature = "test-support"))]
            changed: Arc::new(Notify::new()),
        }
    }

    /// Waits for a file slot.
    pub async fn slot(&self) -> Result<DownloadSlot, String> {
        let file = match self.files.clone().try_acquire_owned() {
            Ok(file) => file,
            Err(TryAcquireError::NoPermits) => {
                self.waited.fetch_add(1, Ordering::SeqCst);
                #[cfg(any(test, feature = "test-support"))]
                self.changed.notify_waiters();
                self.files
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| "download slots closed".to_string())?
            }
            Err(TryAcquireError::Closed) => return Err("download slots closed".to_string()),
        };
        Ok(DownloadSlot {
            _file: file,
            bytes: self.bytes.clone(),
            held: None,
        })
    }

    /// Slots that had to wait for another download to end; tests wait on it.
    pub fn waited(&self) -> u64 {
        self.waited.load(Ordering::SeqCst)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn until_waited(&self, n: u64) {
        loop {
            let changed = self.changed.notified();
            if self.waited() >= n {
                return;
            }
            changed.await;
        }
    }
}

impl DownloadSlot {
    /// Holds `len` bytes of the budget (all of it for a larger body) until the slot drops.
    pub async fn hold(&mut self, len: u64) -> Result<(), String> {
        match &mut self.held {
            Some(held) => held.cover(len).await,
            None => {
                self.held = Some(self.bytes.reserve(len).await?);
                Ok(())
            }
        }
    }
}

/// The sources and the write path a fill uses.
struct Upstream {
    pool: PgPool,
    storage: Arc<Storage>,
    client: reqwest::Client,
    sources: Vec<String>,
    /// Asked for a content file only, after `sources`: the worlds content server the worlds
    /// read-through mirrors from (`WORLDS_CONTENT_MIRROR_SOURCE`), whose scene files a world's
    /// fill leaves to be read on demand.
    file_sources: Vec<String>,
    /// `SYNC_ENTITY_TYPES`: `None` copies every type.
    entity_types: Option<HashSet<String>>,
    deployer: LiveSyncDeployer,
    downloads: Downloads,
}

pub struct ReadThrough {
    upstream: Arc<Upstream>,
    fills: Fills,
}

const ID_KEY: &str = "id:";
const POINTER_KEY: &str = "pointer:";
const CONTENT_KEY: &str = "content:";

impl ReadThrough {
    /// `sources` as `parse_sync_sources` returns them: peer content URLs, in order;
    /// `entity_types` as `parse_entity_types` does.
    pub fn new(
        pool: PgPool,
        storage: Arc<Storage>,
        client: reqwest::Client,
        sources: Vec<String>,
        entity_types: Option<HashSet<String>>,
        fills: Fills,
        validation: SyncValidation,
    ) -> Self {
        let deployer = LiveSyncDeployer::new(pool.clone(), FLUSH_CONCURRENCY, validation);
        let downloads = Downloads::new(MAX_CONCURRENT_DOWNLOADS, deployer.inflight_bytes().clone());
        Self {
            upstream: Arc::new(Upstream {
                pool,
                storage,
                client,
                sources,
                file_sources: Vec::new(),
                entity_types,
                deployer,
                downloads,
            }),
            fills,
        }
    }

    /// `None` while `READ_THROUGH_ENABLED` is off; on, `SYNC_SOURCE` must name at least one
    /// peer that is not this node (`own_urls`), under the rules sync applies, and
    /// `SYNC_ENTITY_TYPES` limits what is copied as it limits sync.
    pub fn from_config(
        config: &crate::config::Config,
        own_urls: &[String],
        pool: PgPool,
        storage: Arc<Storage>,
        client: reqwest::Client,
        validator: Arc<Sources>,
    ) -> anyhow::Result<Option<Self>> {
        if !config.content.read_through_enabled {
            return Ok(None);
        }
        let sources = parse_sync_sources(&config.content.sync_source, own_urls)
            .map_err(|e| anyhow::anyhow!("READ_THROUGH_ENABLED reads through SYNC_SOURCE: {e}"))?;
        let entity_types = parse_entity_types(&config.content.sync_entity_types)?;
        let worlds_source = config.worlds_content.worlds_content_mirror_source.trim();
        let file_sources = if worlds_source.is_empty() {
            Vec::new()
        } else {
            parse_sync_sources(worlds_source, own_urls).map_err(|e| {
                anyhow::anyhow!("READ_THROUGH_ENABLED reads world files through WORLDS_CONTENT_MIRROR_SOURCE: {e}")
            })?
        };
        Ok(Some(
            Self::new(
                pool,
                storage,
                client,
                sources,
                entity_types,
                Fills::new(MAX_CONCURRENT_FILLS, DEFAULT_WAIT),
                SyncValidation::Content(validator),
            )
            .with_file_sources(file_sources),
        ))
    }

    /// Servers asked for a content file (never an entity) once `sources` miss: a worlds
    /// content server answers `/contents/{hash}` as a peer does. Before the read-through is shared.
    pub fn with_file_sources(mut self, file_sources: Vec<String>) -> Self {
        Arc::get_mut(&mut self.upstream)
            .expect("file sources are set before the read-through is shared")
            .file_sources = file_sources;
        self
    }

    pub fn fills(&self) -> &Fills {
        &self.fills
    }

    pub fn downloads(&self) -> &Downloads {
        &self.upstream.downloads
    }

    /// The peers entities are read through, in order: the retry worker's peers with sync off.
    pub fn sources(&self) -> &[String] {
        &self.upstream.sources
    }

    /// The active entities for `values` (ids, or pointers), as the local lookup answers them
    /// once the misses have been filled, without the denylisted ones. Denylisted ids and
    /// pointers are never asked for, and a pointer whose active entity is listed counts as
    /// answered: hidden, not missing.
    pub async fn active_docs(
        &self,
        use_ids: bool,
        values: &[String],
    ) -> Result<Vec<EntityDoc>, sqlx::Error> {
        self.active_docs_within(use_ids, values, self.fills.inner.wait)
            .await
    }

    /// [`active_docs`](Self::active_docs) waiting up to `wait` for the fill (a reader with a
    /// budget of its own, as `/profile-images`); the fill itself keeps its deadline.
    pub async fn active_docs_within(
        &self,
        use_ids: bool,
        values: &[String],
        wait: Duration,
    ) -> Result<Vec<EntityDoc>, sqlx::Error> {
        let pool = &self.upstream.pool;
        let denylist = self.upstream.storage.denylist();
        let mut docs = local_docs(pool, use_ids, values).await?;
        let missing = missing_values(use_ids, values, &docs, denylist);
        if missing.is_empty() {
            queries::retain_non_denylisted(&mut docs, denylist);
            return Ok(docs);
        }
        let prefix = if use_ids { ID_KEY } else { POINTER_KEY };
        let keys = missing.iter().map(|v| format!("{prefix}{v}")).collect();
        let upstream = self.upstream.clone();
        let filled = self
            .fills
            .fill_within(keys, wait, move |own| async move {
                let values = own
                    .iter()
                    .filter_map(|k| k.strip_prefix(prefix).map(str::to_string))
                    .collect();
                upstream.fill_entities(use_ids, values).await;
            })
            .await;
        if filled {
            docs = local_docs(pool, use_ids, values).await?;
        }
        queries::retain_non_denylisted(&mut docs, denylist);
        Ok(docs)
    }

    /// Fetches a content file this node does not hold; `true` when the caller should look
    /// again (a fill ran or was joined).
    pub async fn fill_content(&self, hash: &str) -> bool {
        if !is_canonical_cid(hash) || self.upstream.storage.denylist().contains(hash) {
            return false;
        }
        let upstream = self.upstream.clone();
        let hash = hash.to_string();
        self.fills
            .fill(vec![format!("{CONTENT_KEY}{hash}")], move |_| async move {
                upstream.fill_content(&hash).await;
            })
            .await
    }
}

async fn local_docs(
    pool: &PgPool,
    use_ids: bool,
    values: &[String],
) -> Result<Vec<EntityDoc>, sqlx::Error> {
    if use_ids {
        queries::active_entity_docs_by_ids(pool, values).await
    } else {
        queries::active_entity_docs_by_pointers(pool, values).await
    }
}

/// The requested ids (canonical CIDs) or pointers (lowercased) no local doc answers.
fn missing_values(
    use_ids: bool,
    values: &[String],
    docs: &[EntityDoc],
    denylist: &Denylist,
) -> Vec<String> {
    if use_ids {
        let mut seen = HashSet::new();
        let have: HashSet<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        values
            .iter()
            .filter(|id| !have.contains(id.as_str()) && is_canonical_cid(id))
            .filter(|id| !denylist.contains(id) && seen.insert(id.as_str()))
            .cloned()
            .collect()
    } else {
        let covered: HashSet<String> = docs
            .iter()
            .filter_map(EntityDoc::to_value)
            .flat_map(|doc| entity_pointers(&doc))
            .collect();
        let mut seen = HashSet::new();
        values
            .iter()
            .map(|p| p.to_lowercase())
            .filter(|p| !p.is_empty() && !covered.contains(p) && !denylist.denies_pointer(p))
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }
}

fn entity_pointers(entity: &Value) -> Vec<String> {
    entity["pointers"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

async fn read_json(resp: reqwest::Response) -> Result<Value, SyncError> {
    let mut sink = BufferSink::new(None, None);
    stream_body(resp, MAX_JSON_BYTES, &mut sink)
        .await
        .map_err(SyncError::Other)?;
    Ok(serde_json::from_slice(&sink.into_parts().0)?)
}

impl Upstream {
    /// The sources with `first` moved to the front: the peer that named an entity serves it.
    fn servers_from(&self, first: usize) -> Vec<String> {
        let mut servers = vec![self.sources[first].clone()];
        servers.extend(
            self.sources
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != first)
                .map(|(_, s)| s.clone()),
        );
        servers
    }

    fn copies_type(&self, entity_type: &str) -> bool {
        self.entity_types
            .as_ref()
            .is_none_or(|types| types.contains(entity_type))
    }

    async fn fill_entities(&self, use_ids: bool, values: Vec<String>) {
        let mut wanted = match self.not_held(use_ids, values).await {
            Ok(wanted) => wanted,
            Err(e) => {
                tracing::warn!(error = %e, "read-through: local lookup failed; the miss stands");
                return;
            }
        };
        let mut queued = false;
        for (index, source) in self.sources.iter().enumerate() {
            if wanted.is_empty() {
                break;
            }
            let docs = match self.active_upstream(source, use_ids, &wanted).await {
                Ok(docs) => docs,
                Err(e) => {
                    tracing::warn!(%source, error = %e, "read-through: source did not answer");
                    continue;
                }
            };
            let mut seen = HashSet::new();
            for doc in &docs {
                let (Some(id), Some(entity_type)) = (doc["id"].as_str(), doc["type"].as_str())
                else {
                    continue;
                };
                if !seen.insert(id)
                    || self.storage.denylist().contains(id)
                    || !self.copies_type(entity_type)
                {
                    continue;
                }
                match self.copy_entity(index, id, entity_type).await {
                    Ok(pointers) => {
                        queued = true;
                        wanted.retain(|v| {
                            if use_ids {
                                v != id
                            } else {
                                !pointers.contains(v)
                            }
                        });
                    }
                    Err(e) => tracing::warn!(
                        %source,
                        entity_id = id,
                        error = %e,
                        "read-through: entity refused, nothing stored"
                    ),
                }
            }
        }
        if queued && let Err(e) = self.deployer.flush().await {
            tracing::warn!(error = %e, "read-through: flush failed");
        }
    }

    /// `values` minus what a fill that finished first (or sync) has stored meanwhile: ids
    /// with any deployment row, pointers with an active entity.
    async fn not_held(
        &self,
        use_ids: bool,
        values: Vec<String>,
    ) -> Result<Vec<String>, sqlx::Error> {
        let held: Vec<String> = if use_ids {
            sqlx::query_scalar(
                "SELECT entity_id FROM content.deployments WHERE entity_id = ANY($1)",
            )
            .bind(&values)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT pointer FROM content.active_pointers WHERE pointer = ANY($1)",
            )
            .bind(&values)
            .fetch_all(&self.pool)
            .await?
        };
        let held: HashSet<String> = held.into_iter().collect();
        Ok(values.into_iter().filter(|v| !held.contains(v)).collect())
    }

    async fn active_upstream(
        &self,
        source: &str,
        use_ids: bool,
        values: &[String],
    ) -> Result<Vec<Value>, SyncError> {
        let body = if use_ids {
            json!({ "ids": values })
        } else {
            json!({ "pointers": values })
        };
        let resp = self
            .client
            .post(format!("{source}/entities/active"))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        match read_json(resp).await? {
            Value::Array(docs) => Ok(docs),
            _ => Err(SyncError::Other(
                "/entities/active did not answer an array".into(),
            )),
        }
    }

    async fn audit_chain(
        &self,
        servers: &[String],
        entity_type: &str,
        id: &str,
    ) -> Result<AuthChain, SyncError> {
        let mut last = SyncError::NoServers;
        for server in servers {
            let resp = match self
                .client
                .get(format!("{server}/audit/{entity_type}/{id}"))
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
            {
                Ok(resp) => resp,
                Err(e) => {
                    last = e.into();
                    continue;
                }
            };
            match read_json(resp).await.and_then(|audit| {
                serde_json::from_value::<AuthChain>(audit["authChain"].clone()).map_err(Into::into)
            }) {
                Ok(chain) => return Ok(chain),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// One entity, verified before anything is stored: the file hashes to `id` and the entity
    /// passes the content validator; then sync's own path downloads its content files, validates
    /// again with them and queues the row. Answers the pointers the verified file claims.
    async fn copy_entity(
        &self,
        source: usize,
        id: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, SyncError> {
        if !is_canonical_cid(id) {
            return Err(SyncError::Other(format!("{id} is not a content id")));
        }
        let deployed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM content.deployments WHERE entity_id = $1)",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?;
        if deployed {
            return Err(SyncError::Other(format!(
                "{id} is already deployed here; a later one supersedes it"
            )));
        }
        let servers = self.servers_from(source);
        let chain = self.audit_chain(&servers, entity_type, id).await?;

        let slot = self.downloads.slot().await.map_err(SyncError::Other)?;
        let budget = self.deployer.inflight_bytes().as_ref();
        let mut fetched = Err(SyncError::NoServers);
        for server in &servers {
            fetched = fetch_verified(&self.client, id, server, 0, Some(budget)).await;
            if fetched.is_ok() {
                break;
            }
        }
        let (bytes, hold) = fetched?;
        self.deployer
            .verify_entity(&bytes, id, &chain, SyncedContent::Pending)
            .await?;
        let doc: Value = serde_json::from_slice(&bytes)?;
        let pointers = entity_pointers(&doc);
        let entity_type = doc["type"].as_str().unwrap_or(entity_type).to_string();
        self.storage.store(id, bytes.into()).await?;
        drop((hold, slot));

        let deployed = deploy_entity_streaming(
            ContentSource {
                client: &self.client,
                storage: self.storage.clone(),
                servers: &servers,
                content_semaphore: self.downloads.files.clone(),
            },
            &self.deployer,
            RemoteDeployment {
                entity_id: id,
                auth_chain: &chain,
                context: DeploymentContext::Synced,
                report: None,
                retry_state: None,
            },
        )
        .await;
        if let Err(e) = deployed {
            self.settle_unvalidated(id, &entity_type, &chain, &e).await;
            return Err(e);
        }
        Ok(pointers)
    }

    /// After a copy failed once its files were in: a refused or undecided entity's file goes,
    /// unless sync holds it, and an undecided one is recorded for the retry worker.
    async fn settle_unvalidated(
        &self,
        id: &str,
        entity_type: &str,
        chain: &AuthChain,
        error: &SyncError,
    ) {
        match error {
            SyncError::DeploymentRejected(_) => {}
            SyncError::ValidationUndecided(_) => {
                let failure = FailedDeployment {
                    entity_type: entity_type.to_string(),
                    entity_id: id.to_string(),
                    reason: FailureReason::ValidationUndecided,
                    auth_chain: chain.clone(),
                    error_description: error.to_string(),
                    failure_timestamp: chrono::Utc::now().timestamp_millis(),
                    snapshot_hash: None,
                    retry_count: 0,
                    next_retry_at: 0,
                };
                if let Err(e) = LiveFailedDeploymentsStore::new(self.pool.clone())
                    .report_failure(failure)
                    .await
                {
                    tracing::warn!(entity_id = id, error = %e, "read-through: undecided entity not recorded");
                }
            }
            _ => return,
        }
        self.evict_refused(id).await;
    }

    /// Removes an unvalidated entity's file unless a deployment row or a sync buffer refers to it.
    async fn evict_refused(&self, id: &str) {
        if is_buffered(id) {
            return;
        }
        match is_entity_deployed(&self.pool, id, 0).await {
            Ok(false) => {}
            Ok(true) => return,
            Err(e) => {
                tracing::warn!(entity_id = id, error = %e, "read-through: refused entity file kept");
                return;
            }
        }
        // Sync may have taken its hold during the lookup.
        if is_buffered(id) {
            return;
        }
        if let Err(evict) = self.storage.delete(id).await {
            tracing::warn!(entity_id = id, error = %evict, "read-through: refused entity file not removed");
        }
    }

    async fn fill_content(&self, hash: &str) {
        match self.storage.exist(hash).await {
            Ok(false) => {}
            Ok(true) => return,
            Err(e) => {
                tracing::warn!(hash, error = %e, "read-through: storage check failed");
                return;
            }
        }
        let slot = match self.downloads.slot().await {
            Ok(slot) => slot,
            Err(e) => {
                tracing::warn!(hash, error = %e, "read-through: no download slot");
                return;
            }
        };
        let budget = self.deployer.inflight_bytes().as_ref();
        for server in self.sources.iter().chain(&self.file_sources) {
            match fetch_verified(&self.client, hash, server, 0, Some(budget)).await {
                Ok((bytes, hold)) => {
                    if let Err(e) = self.storage.store(hash, bytes.into()).await {
                        tracing::warn!(hash, error = %e, "read-through: storing a content file failed");
                    }
                    drop((hold, slot));
                    return;
                }
                Err(e) => tracing::debug!(hash, %server, error = %e, "read-through: content miss"),
            }
        }
    }
}

#[cfg(test)]
mod tests;
