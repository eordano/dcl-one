//! One remote entity, end to end: fetch the entity file from a peer, prove it
//! by its content hash, fetch every content file it declares (same proof),
//! then hand it to the batching [`LiveSyncDeployer`]. Nothing unproven is ever
//! stored, and a stored entity file that later fails its hash is evicted.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, OnceLock};

use futures::StreamExt;
use reqwest::Client;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use super::batch_deployer::DeploymentReport;
use super::sync_deployer::LiveSyncDeployer;
use super::sync_types::{AuthChain, DeploymentContext, SyncError};
use crate::auth::{is_canonical_cid, verify_hash};
use crate::entity::{is_ipfs_v1, is_ipfs_v2};
use crate::storage::Storage;

const MAX_DOWNLOAD_RETRIES: u32 = 3;
const RETRY_WAIT_MS: u64 = 500;

const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;

/// What a download without a Content-Length holds against the byte budget before its first
/// chunk; the hold grows with the decoded body from there.
const BODY_ESTIMATE_BYTES: u64 = 4 * 1024 * 1024;
/// Growth is asked for in steps at least this large, so a chunked body of a few MB does not
/// queue on the semaphore once per 16 KB chunk.
const BUDGET_GROWTH_STEP_BYTES: u64 = 4 * 1024 * 1024;
/// How long a download already holding part of the budget waits for more. Growth is asked
/// for after the bytes are buffered, so when every in-flight body needs more and none can
/// finish, the only way out is for one of them to give up and release its hold: the wait
/// turns that case into a failed attempt (retried later) instead of a hang.
const BUDGET_GROWTH_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Content addressing is over the DECODED file bytes, but some peers (peer.dclnodes.io) serve
/// content-addressed blobs with `Content-Encoding: gzip` regardless of Accept-Encoding, and
/// this package's reqwest has every automatic-decompression feature disabled -- the sync code
/// receives the wire bytes as sent. The transfer coding must therefore be undone here, before
/// hashing and storing, or verification fails on every such blob. It is undone chunk by chunk
/// ([`stream_body`]), so a body spooled to disk never has its wire bytes and its decoded bytes
/// in memory at once, and the caller caps the decoded size so a compression bomb cannot
/// outgrow its body cap.
pub(crate) enum TransferDecoder {
    Identity,
    Gzip(Box<flate2::write::MultiGzDecoder<Vec<u8>>>),
}

/// Compressed bytes inflated per step; deflate expands at most ~1032x, so one step yields
/// at most ~258 KiB.
const GZIP_INPUT_STEP_BYTES: usize = 256;
/// Decoded bytes [`stream_body`] holds before handing them to its sink.
const SINK_FLUSH_BYTES: usize = 1 << 20;

impl TransferDecoder {
    /// The wire bytes [`stream_body`] decodes at a time.
    fn input_step(&self) -> usize {
        match self {
            Self::Identity => usize::MAX,
            Self::Gzip(_) => GZIP_INPUT_STEP_BYTES,
        }
    }

    pub(crate) fn for_encoding(encoding: Option<&str>) -> Result<Self, String> {
        match encoding.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Ok(Self::Identity),
            "gzip" | "x-gzip" => Ok(Self::Gzip(Box::new(flate2::write::MultiGzDecoder::new(
                Vec::new(),
            )))),
            other => Err(format!("unsupported content-encoding {other:?}")),
        }
    }

    /// Appends what `chunk` decodes to (so far) onto `out`, failing once `out` passes
    /// `limit` bytes: gzip input is fed in small steps, so a compression bomb overshoots the
    /// limit by at most one step's output, never by a whole wire chunk's.
    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), String> {
        match self {
            Self::Identity => out.extend_from_slice(chunk),
            Self::Gzip(decoder) => {
                for step in chunk.chunks(GZIP_INPUT_STEP_BYTES) {
                    decoder
                        .write_all(step)
                        .map_err(|e| format!("gzip transfer decoding failed: {e}"))?;
                    out.append(decoder.get_mut());
                    if out.len() > limit {
                        return Err(format!("decoded body exceeds the {limit} bytes left"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Appends whatever the coding still holds onto `out`; a gzip stream cut short fails here.
    pub(crate) fn finish(self, out: &mut Vec<u8>) -> Result<(), String> {
        match self {
            Self::Identity => Ok(()),
            Self::Gzip(decoder) => {
                let mut rest = decoder
                    .finish()
                    .map_err(|e| format!("gzip transfer decoding failed: {e}"))?;
                out.append(&mut rest);
                Ok(())
            }
        }
    }
}

pub(crate) fn response_content_encoding(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Where [`stream_body`] hands each run of decoded bytes: a buffer for an entity or content
/// file, the spool file plus hasher for a snapshot.
pub(crate) trait DecodedSink {
    fn accept(
        &mut self,
        decoded: &[u8],
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BodyStats {
    /// Bytes as they came off the wire, what a Content-Length promises.
    pub wire_len: u64,
    /// Bytes after transfer decoding, what is hashed and stored.
    pub decoded_len: u64,
}

/// Streams `resp`'s body through its transfer decoding into `sink`, holding one wire chunk and
/// about [`SINK_FLUSH_BYTES`] of decoded bytes in memory at a time, with `cap` enforced on both
/// the wire and the decoded bytes as they pass. Whether the wire bytes fell short of the
/// Content-Length is the caller's to judge from the stats.
pub(crate) async fn stream_body<S: DecodedSink>(
    resp: reqwest::Response,
    cap: usize,
    sink: &mut S,
) -> Result<BodyStats, String> {
    if let Some(len) = resp.content_length()
        && len > cap as u64
    {
        return Err(format!(
            "response body advertises {len} bytes, over the {cap} byte cap"
        ));
    }
    let mut decoder = TransferDecoder::for_encoding(response_content_encoding(&resp).as_deref())?;
    let mut stats = BodyStats {
        wire_len: 0,
        decoded_len: 0,
    };
    let mut run: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        // A body error names the URL, which is stripped as `SendError` strips it from a
        // transport error.
        let chunk = chunk.map_err(|e| e.without_url().to_string())?;
        stats.wire_len += chunk.len() as u64;
        if stats.wire_len > cap as u64 {
            return Err(format!("response body exceeds the {cap} byte cap"));
        }
        for piece in chunk.chunks(decoder.input_step()) {
            // `run` holds decoded bytes not yet counted in `decoded_len`.
            let left = (cap as u64).saturating_sub(stats.decoded_len) as usize;
            decoder.push(piece, &mut run, left).map_err(|e| {
                if run.len() > left {
                    format!("decoded body exceeds {cap} byte cap")
                } else {
                    e
                }
            })?;
            if run.len() >= SINK_FLUSH_BYTES {
                stats.decoded_len += run.len() as u64;
                sink.accept(&run).await?;
                run.clear();
            }
        }
        if !run.is_empty() {
            stats.decoded_len += run.len() as u64;
            sink.accept(&run).await?;
            run.clear();
        }
    }
    decoder.finish(&mut run)?;
    stats.decoded_len += run.len() as u64;
    if stats.decoded_len > cap as u64 {
        return Err(format!("decoded body exceeds {cap} byte cap"));
    }
    if !run.is_empty() {
        sink.accept(&run).await?;
    }
    Ok(stats)
}

/// Decoded file bodies held in memory across every in-flight download, in bytes: what
/// `CONCURRENT_SYNC_DOWNLOADS` bounds by count, `SYNC_INFLIGHT_BYTES_MAX` bounds by size. One
/// [`LiveSyncDeployer`] owns one, so the snapshot workers, the pointer-changes stream and the
/// retry loop draw on the same pool. A hold is taken per download for its Content-Length (an
/// estimate without one, grown as the decoded bytes outrun it) and released once the file is
/// stored; a body larger than the whole budget holds the whole budget, so it proceeds alone.
pub struct ByteBudget {
    permits: Arc<Semaphore>,
    max: u32,
}

impl ByteBudget {
    /// `max_bytes` is clamped to what one hold can ask for (`u32::MAX`, the `acquire_many`
    /// bound), and 0 -- which would stall every download -- is raised to 1.
    pub fn new(max_bytes: u64) -> Self {
        let max = max_bytes
            .clamp(1, u32::MAX as u64)
            .min(Semaphore::MAX_PERMITS as u64) as u32;
        Self {
            permits: Arc::new(Semaphore::new(max as usize)),
            max,
        }
    }

    pub fn max_bytes(&self) -> u64 {
        self.max as u64
    }

    /// Bytes currently held by in-flight downloads.
    pub fn in_use(&self) -> u64 {
        self.max as u64 - self.permits.available_permits() as u64
    }

    pub(crate) async fn reserve(&self, bytes: u64) -> Result<ByteReservation, String> {
        let want = permits_for(bytes, self.max);
        let permit = self
            .permits
            .clone()
            .acquire_many_owned(want)
            .await
            .map_err(|_| "in-flight byte budget closed".to_string())?;
        Ok(ByteReservation {
            budget: self.permits.clone(),
            max: self.max,
            held: want,
            permit,
        })
    }
}

/// Permits a body of `bytes` takes from a budget of `max`: its size, or the whole budget when
/// it is larger.
pub(crate) fn permits_for(bytes: u64, max: u32) -> u32 {
    bytes.min(max as u64) as u32
}

/// Permits a hold of `held` must add to cover `needed` bytes: at least `step` (so a chunked
/// body grows in few asks), never past `max`, and 0 once covered or once the whole budget is
/// held.
pub(crate) fn growth_for(held: u32, needed: u64, max: u32, step: u64) -> u32 {
    if needed <= held as u64 || held >= max {
        return 0;
    }
    let room = (max - held) as u64;
    (needed - held as u64).max(step).min(room) as u32
}

/// One download's hold on a [`ByteBudget`], released on drop.
pub(crate) struct ByteReservation {
    budget: Arc<Semaphore>,
    max: u32,
    held: u32,
    permit: OwnedSemaphorePermit,
}

impl ByteReservation {
    #[cfg(test)]
    pub(crate) fn held(&self) -> u64 {
        self.held as u64
    }

    /// Grows the hold to cover `bytes`, waiting at most [`BUDGET_GROWTH_WAIT`] for the others
    /// to release theirs.
    pub(crate) async fn cover(&mut self, bytes: u64) -> Result<(), String> {
        let extra = growth_for(self.held, bytes, self.max, BUDGET_GROWTH_STEP_BYTES);
        if extra == 0 {
            return Ok(());
        }
        match tokio::time::timeout(
            BUDGET_GROWTH_WAIT,
            self.budget.clone().acquire_many_owned(extra),
        )
        .await
        {
            Ok(Ok(more)) => {
                self.permit.merge(more);
                self.held += extra;
                Ok(())
            }
            Ok(Err(_)) => Err("in-flight byte budget closed".to_string()),
            Err(_) => Err(format!(
                "in-flight byte budget exhausted: holding {} bytes, {} more not granted within {}s",
                self.held,
                extra,
                BUDGET_GROWTH_WAIT.as_secs()
            )),
        }
    }
}

/// The decoded body in memory, held against the budget as it arrives: the Content-Length is
/// reserved at the first decoded run (an estimate without one) and the hold grows as the
/// decoded bytes outrun it.
pub(crate) struct BufferSink<'a> {
    bytes: Vec<u8>,
    budget: Option<&'a ByteBudget>,
    expected_len: Option<u64>,
    reservation: Option<ByteReservation>,
}

impl<'a> BufferSink<'a> {
    pub(crate) fn new(budget: Option<&'a ByteBudget>, expected_len: Option<u64>) -> Self {
        Self {
            bytes: Vec::new(),
            budget,
            expected_len,
            reservation: None,
        }
    }

    /// The body and the hold that covers it, for the caller to release once the bytes are stored.
    pub(crate) fn into_parts(self) -> (Vec<u8>, Option<ByteReservation>) {
        (self.bytes, self.reservation)
    }
}

impl DecodedSink for BufferSink<'_> {
    async fn accept(&mut self, decoded: &[u8]) -> Result<(), String> {
        if let Some(budget) = self.budget
            && self.reservation.is_none()
        {
            let bytes = self.expected_len.unwrap_or(BODY_ESTIMATE_BYTES);
            self.reservation = Some(budget.reserve(bytes).await?);
        }
        self.bytes.extend_from_slice(decoded);
        if let Some(reservation) = self.reservation.as_mut() {
            reservation.cover(self.bytes.len() as u64).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CachedEntityVerification {
    Match,
    Mismatch,
    Unverifiable,
}

fn verify_cached_entity(bytes: &[u8], entity_id: &str) -> CachedEntityVerification {
    let ipfs_shape = is_ipfs_v1(entity_id) || is_ipfs_v2(entity_id);
    if !ipfs_shape || !is_canonical_cid(entity_id) {
        return CachedEntityVerification::Unverifiable;
    }
    if verify_hash(bytes, entity_id) {
        CachedEntityVerification::Match
    } else {
        CachedEntityVerification::Mismatch
    }
}

fn eviction_locks() -> &'static parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

async fn evict_corrupt_entity_file(storage: &Storage, entity_id: &str) -> &'static str {
    let lock = {
        let mut map = eviction_locks().lock();
        map.entry(entity_id.to_string()).or_default().clone()
    };

    let outcome = {
        let _guard = lock.lock().await;
        evict_corrupt_entity_file_locked(storage, entity_id).await
    };

    {
        let mut map = eviction_locks().lock();
        if let Some(existing) = map.get(entity_id)
            && Arc::ptr_eq(existing, &lock)
            && Arc::strong_count(existing) == 2
        {
            map.remove(entity_id);
        }
    }

    outcome
}

async fn evict_corrupt_entity_file_locked(storage: &Storage, entity_id: &str) -> &'static str {
    let current = match storage.retrieve(entity_id).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return "the corrupt local copy was already removed; \
                    a later retry will re-download it";
        }
        Err(_) => {
            return "the local copy could not be re-verified; \
                    it was kept and a later retry will re-check it";
        }
    };

    if verify_cached_entity(&current, entity_id) != CachedEntityVerification::Mismatch {
        return "the local copy has since been replaced by a hash-valid one, which was kept";
    }

    match storage.delete_strict(entity_id).await {
        Ok(()) => "removed the corrupt local copy so it can be re-downloaded",
        Err(_) => "could not remove the corrupt local copy; a later retry will attempt it again",
    }
}

pub struct ContentSource<'a> {
    pub client: &'a Client,
    pub storage: Arc<Storage>,
    pub servers: &'a [String],
    pub content_semaphore: Arc<Semaphore>,
}

pub struct RemoteDeployment<'a> {
    pub entity_id: &'a str,
    pub auth_chain: &'a AuthChain,
    pub context: DeploymentContext,
    pub report: Option<&'a Arc<DeploymentReport>>,
    pub retry_state: Option<(u32, u64)>,
}

pub async fn deploy_entity_streaming(
    source: ContentSource<'_>,
    deployer: &LiveSyncDeployer,
    target: RemoteDeployment<'_>,
) -> Result<(), SyncError> {
    let ContentSource {
        client,
        storage,
        servers,
        content_semaphore,
    } = source;
    let RemoteDeployment {
        entity_id,
        auth_chain,
        context,
        report,
        retry_state,
    } = target;
    // The read-through never removes a file held here until the flush takes the entity.
    let _in_flight = crate::sync_deployer::hold_buffered(entity_id);
    let budget = deployer.inflight_bytes();
    download_file_with_retries(client, storage.as_ref(), entity_id, servers, Some(budget)).await?;

    let entity_data =
        storage
            .retrieve(entity_id)
            .await?
            .ok_or_else(|| SyncError::EntityNotFound {
                entity_id: entity_id.to_string(),
            })?;

    let verification = verify_cached_entity(&entity_data, entity_id);
    if verification == CachedEntityVerification::Mismatch {
        warn!(
            entity_id,
            "Cached entity file failed content-hash verification"
        );
        let outcome = evict_corrupt_entity_file(storage.as_ref(), entity_id).await;
        return Err(SyncError::Other(format!(
            "The stored entity file for {entity_id} failed content-hash verification; {outcome}."
        )));
    }

    if entity_data.is_empty() {
        return Err(entity_parse_error(
            entity_id,
            verification,
            "the stored entity file was empty",
        ));
    }
    let hashes = extract_content_hashes(&entity_data)
        .map_err(|e| entity_parse_error(entity_id, verification, e))?;

    let mut tasks: tokio::task::JoinSet<Result<(), SyncError>> = tokio::task::JoinSet::new();

    let refs: Vec<&str> = hashes.iter().map(String::as_str).collect();
    let present = storage.exist_multiple(&refs).await?;
    for (hash, exists) in present {
        if exists {
            continue;
        }

        let client = client.clone();
        let storage = storage.clone();
        let servers = servers.to_vec();
        let sem = content_semaphore.clone();
        let budget = budget.clone();

        tasks.spawn(async move {
            let _permit = sem.acquire().await.map_err(|_| SyncError::Stopped)?;
            download_file_with_retries(&client, storage.as_ref(), &hash, &servers, Some(&budget))
                .await
        });
    }

    let mut first_error: Option<SyncError> = None;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(SyncError::Other(format!("task join error: {}", e)));
                }
            }
        }
    }

    if let Some(e) = first_error {
        return Err(e);
    }

    deployer
        .deploy_entity(
            &entity_data,
            entity_id,
            auth_chain,
            context,
            report,
            retry_state,
        )
        .await?;

    Ok(())
}

fn entity_parse_error(
    entity_id: &str,
    verification: CachedEntityVerification,
    cause: impl std::fmt::Display,
) -> SyncError {
    let kept = match verification {
        CachedEntityVerification::Match => {
            "the stored bytes match the content hash, so the local copy was kept"
        }
        _ => {
            "the stored bytes could not be proven corrupt (unverifiable hash scheme), \
             so the local copy was kept"
        }
    };
    SyncError::Other(format!(
        "Failed to parse the downloaded entity file for {entity_id}; {kept}. Cause: {cause}"
    ))
}

/// The hashes sync downloads for an entity: every content file, a profile's avatar renders
/// (`face.png`, `face128.png`, `face256.png`, `body.png`) included, as the reference catalyst
/// stores them, so every hash this node lists in `/deployments`, `/pointer-changes` and its
/// snapshots is one `/contents/{hash}` serves. An entity other than a profile that carries
/// avatars (`metadata.avatars` or a top-level `avatars`) also has their snapshot hashes fetched.
fn extract_content_hashes(entity_data: &[u8]) -> Result<Vec<String>, serde_json::Error> {
    let entity: serde_json::Value = serde_json::from_slice(entity_data)?;
    let is_profile = entity.get("type").and_then(|t| t.as_str()) == Some("profile");
    let mut seen = HashSet::new();
    let mut hashes = Vec::new();

    if let Some(content) = entity.get("content").and_then(|c| c.as_array()) {
        for entry in content {
            if let Some(hash) = entry.get("hash").and_then(|h| h.as_str())
                && seen.insert(hash.to_string())
            {
                hashes.push(hash.to_string());
            }
        }
    }

    let avatars = entity
        .pointer("/metadata/avatars")
        .or_else(|| entity.get("avatars"))
        .and_then(|a| a.as_array());

    if let Some(avatar_list) = avatars.filter(|_| !is_profile) {
        for avatar_entry in avatar_list {
            let snapshots = avatar_entry.pointer("/avatar/snapshots");
            if let Some(obj) = snapshots.and_then(|s| s.as_object()) {
                for (_key, val) in obj {
                    if let Some(raw) = val.as_str() {
                        let hash = extract_hash_from_snapshot_value(raw);
                        if !hash.is_empty() && seen.insert(hash.to_string()) {
                            hashes.push(hash.to_string());
                        }
                    }
                }
            }
        }
    }

    Ok(hashes)
}

fn extract_hash_from_snapshot_value(s: &str) -> &str {
    if let Some(idx) = s.rfind("/contents/") {
        &s[idx + "/contents/".len()..]
    } else {
        s
    }
}

/// Tries up to [`MAX_DOWNLOAD_RETRIES`] peers, starting at a random one, and stores the file
/// only once its decoded bytes hash to `hash`. With a `budget`, the decoded body is held
/// against it from its first chunk until the file is stored.
pub async fn download_file_with_retries(
    client: &Client,
    storage: &Storage,
    hash: &str,
    servers: &[String],
    budget: Option<&ByteBudget>,
) -> Result<(), SyncError> {
    if storage.exist(hash).await? {
        return Ok(());
    }

    if servers.is_empty() {
        return Err(SyncError::Other("No servers available".into()));
    }

    let mut last_error = None;
    let start = (uuid::Uuid::new_v4().as_u128() % servers.len() as u128) as usize;
    for attempt in 0..MAX_DOWNLOAD_RETRIES {
        let server = &servers[(start + attempt as usize) % servers.len()];
        match fetch_verified(client, hash, server, attempt, budget).await {
            Ok((bytes, reservation)) => {
                storage.store(hash, bytes.into()).await?;
                drop(reservation);
                debug!(hash, "Downloaded content file");
                return Ok(());
            }
            Err(e) => last_error = Some(e),
        }

        if attempt + 1 < MAX_DOWNLOAD_RETRIES {
            tokio::time::sleep(std::time::Duration::from_millis(
                RETRY_WAIT_MS
                    .saturating_mul(1u64 << attempt.min(6))
                    .min(30_000),
            ))
            .await;
        }
    }

    Err(last_error
        .unwrap_or_else(|| SyncError::Other(format!("Failed to download {} after retries", hash))))
}

/// One `GET {server}/contents/{hash}`: the decoded body, only when it hashes to `hash`, and
/// the budget hold that covers it until the caller has stored it.
pub(crate) async fn fetch_verified(
    client: &Client,
    hash: &str,
    server: &str,
    attempt: u32,
    budget: Option<&ByteBudget>,
) -> Result<(Vec<u8>, Option<ByteReservation>), SyncError> {
    let url = format!("{}/contents/{}", server, hash);
    let resp = client.get(&url).send().await.map_err(|e| {
        warn!(hash, %server, error = %e, attempt, "Download request failed");
        SyncError::Http(e)
    })?;
    if resp.status().as_u16() == 404 {
        return Err(SyncError::Other(format!(
            "404 fetching {} from {}",
            hash, server
        )));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        warn!(hash, %server, %status, attempt, "Download failed");
        return Err(SyncError::Other(format!(
            "HTTP {} fetching {}",
            status, url
        )));
    }
    let expected_len = resp.content_length();
    let mut sink = BufferSink::new(budget, expected_len);
    let stats = stream_body(resp, MAX_BODY_BYTES, &mut sink)
        .await
        .map_err(|reason| {
            warn!(
                hash,
                %server,
                attempt,
                %reason,
                "Content body could not be read, trying next server"
            );
            SyncError::Other(format!("content {} from {}: {}", hash, server, reason))
        })?;
    if let Some(len) = expected_len
        && stats.wire_len < len
    {
        warn!(
            hash,
            %server,
            attempt,
            got = stats.wire_len,
            expected = len,
            "Content download TRUNCATED (short read), trying next server"
        );
        return Err(SyncError::Other(format!(
            "content {} from {} truncated: got {} of {} bytes",
            hash, server, stats.wire_len, len
        )));
    }
    let (bytes, reservation) = sink.into_parts();
    if !verify_hash(&bytes, hash) {
        warn!(
            hash,
            %server,
            attempt,
            "Downloaded content failed hash verification"
        );
        return Err(SyncError::Other(format!(
            "content hash mismatch for {} from {}",
            hash, server
        )));
    }
    Ok((bytes, reservation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{hash_bytes, hash_bytes_v1};
    #[cfg(unix)]
    use crate::storage::hex_prefix;
    use crate::sync_test_support::{spawn_content_server, temp_storage};

    #[test]
    fn test_extract_content_hashes() {
        let entity = serde_json::json!({
            "content": [
                {"file": "scene.json", "hash": "bafabc"},
                {"file": "model.glb", "hash": "bafdef"},
            ]
        });
        let data = serde_json::to_vec(&entity).unwrap();
        let hashes = extract_content_hashes(&data).unwrap();
        assert_eq!(hashes, vec!["bafabc", "bafdef"]);
    }

    #[test]
    fn test_extract_content_hashes_deduplicates() {
        let entity = serde_json::json!({
            "content": [
                {"file": "body.png", "hash": "bafbody"},
            ],
            "metadata": {
                "avatars": [{
                    "avatar": {
                        "snapshots": {
                            "face256": "bafface",
                            "body": "bafbody",
                        }
                    }
                }]
            }
        });
        let data = serde_json::to_vec(&entity).unwrap();
        let hashes = extract_content_hashes(&data).unwrap();
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&"bafbody".to_string()));
        assert!(hashes.contains(&"bafface".to_string()));
    }

    #[test]
    fn test_extract_content_hashes_multi_avatar() {
        let entity = serde_json::json!({
            "content": [],
            "metadata": {
                "avatars": [
                    {"avatar": {"snapshots": {"face": "baf1"}}},
                    {"avatar": {"snapshots": {"face": "baf2"}}},
                ]
            }
        });
        let data = serde_json::to_vec(&entity).unwrap();
        let hashes = extract_content_hashes(&data).unwrap();
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&"baf1".to_string()));
        assert!(hashes.contains(&"baf2".to_string()));
    }

    #[test]
    fn test_extract_content_hashes_url_snapshots() {
        let entity = serde_json::json!({
            "content": [],
            "metadata": {
                "avatars": [{
                    "avatar": {
                        "snapshots": {
                            "face": "https://peer.decentraland.org/content/contents/bafurlhash"
                        }
                    }
                }]
            }
        });
        let data = serde_json::to_vec(&entity).unwrap();
        let hashes = extract_content_hashes(&data).unwrap();
        assert_eq!(hashes, vec!["bafurlhash"]);
    }

    #[test]
    fn profiles_fetch_every_avatar_render() {
        let entity = serde_json::json!({
            "type": "profile",
            "content": [
                {"file": "body.png", "hash": "bafbody"},
                {"file": "face256.png", "hash": "bafface256"},
                {"file": "face.png", "hash": "bafface512"},
                {"file": "./face128.png", "hash": "bafface128"},
                {"file": "extra.json", "hash": "bafextra"},
            ],
            "metadata": {
                "avatars": [{
                    "avatar": {
                        "snapshots": {
                            "face": "bafface512",
                            "face128": "https://peer.decentraland.org/content/contents/bafface128",
                            "face256": "bafface256",
                            "body": "bafbody",
                        }
                    }
                }]
            }
        });
        let data = serde_json::to_vec(&entity).unwrap();
        assert_eq!(
            extract_content_hashes(&data).unwrap(),
            vec![
                "bafbody",
                "bafface256",
                "bafface512",
                "bafface128",
                "bafextra"
            ]
        );
    }

    const ENTITY_JSON: &[u8] = br#"{"type":"scene","content":[]}"#;
    const CORRUPT_BYTES: &[u8] = br#"{"type":"scene","#;

    #[test]
    fn test_verify_cached_entity_tri_state() {
        let v1 = hash_bytes_v1(ENTITY_JSON);
        let v0 = hash_bytes(ENTITY_JSON);

        assert_eq!(
            verify_cached_entity(ENTITY_JSON, &v1),
            CachedEntityVerification::Match
        );
        assert_eq!(
            verify_cached_entity(ENTITY_JSON, &v0),
            CachedEntityVerification::Match
        );
        assert_eq!(
            verify_cached_entity(CORRUPT_BYTES, &v1),
            CachedEntityVerification::Mismatch
        );
        assert_eq!(
            verify_cached_entity(CORRUPT_BYTES, &v0),
            CachedEntityVerification::Mismatch
        );

        assert_eq!(
            verify_cached_entity(CORRUPT_BYTES, "Qmnotavalidcid"),
            CachedEntityVerification::Unverifiable
        );
        let over_long_v1 = format!("ba{}", "a".repeat(70));
        assert_eq!(
            verify_cached_entity(CORRUPT_BYTES, &over_long_v1),
            CachedEntityVerification::Unverifiable
        );
        assert_eq!(
            verify_cached_entity(CORRUPT_BYTES, "zUnknownScheme"),
            CachedEntityVerification::Unverifiable
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_entity_is_held_from_its_download_until_the_attempt_ends() {
        let asked = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (on_ask, on_release) = (asked.clone(), release.clone());
        let app = axum::Router::new().route(
            "/contents/{hash}",
            axum::routing::get(move || {
                let (asked, release) = (on_ask.clone(), on_release.clone());
                async move {
                    asked.notify_one();
                    let _open = release.acquire().await;
                    axum::http::StatusCode::NOT_FOUND
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (storage, tmp) = temp_storage("in-flight").await;
        let deployer = LiveSyncDeployer::new(
            sqlx::PgPool::connect_lazy("postgres://nobody@127.0.0.1:1/none").unwrap(),
            1,
            crate::sync_deployer::SyncValidation::AuthChainOnly,
        );
        let id = hash_bytes_v1(format!("in-flight-{}", uuid::Uuid::new_v4()).as_bytes());
        let servers = vec![url];
        let (client, chain) = (reqwest::Client::new(), Vec::new());
        let attempt = deploy_entity_streaming(
            ContentSource {
                client: &client,
                storage,
                servers: &servers,
                content_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            },
            &deployer,
            RemoteDeployment {
                entity_id: &id,
                auth_chain: &chain,
                context: DeploymentContext::Synced,
                report: None,
                retry_state: None,
            },
        );
        let observe = async {
            asked.notified().await;
            let during = crate::sync_deployer::is_buffered(&id);
            release.add_permits(1);
            during
        };
        let (result, held_during_download) = tokio::join!(attempt, observe);
        assert!(held_during_download, "held while its file downloads");
        assert!(result.is_err());
        assert!(
            !crate::sync_deployer::is_buffered(&id),
            "released after the attempt"
        );
        server.abort();
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn evict_removes_a_proven_corrupt_copy() {
        const MISMATCH_JSON: &[u8] = br#"{"type":"scene","content":[],"t":"mismatch"}"#;

        let (storage, tmp) = temp_storage("mismatch").await;
        let entity_id = hash_bytes_v1(MISMATCH_JSON);
        storage
            .store(&entity_id, bytes::Bytes::from_static(CORRUPT_BYTES))
            .await
            .unwrap();

        let outcome = evict_corrupt_entity_file(&storage, &entity_id).await;
        assert_eq!(
            outcome,
            "removed the corrupt local copy so it can be re-downloaded"
        );
        assert!(!storage.exist(&entity_id).await.unwrap());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn evict_keeps_a_healed_copy() {
        const HEALED_JSON: &[u8] = br#"{"type":"scene","content":[],"t":"healed"}"#;

        let (storage, tmp) = temp_storage("healed").await;
        let entity_id = hash_bytes_v1(HEALED_JSON);
        storage
            .store(&entity_id, bytes::Bytes::from_static(HEALED_JSON))
            .await
            .unwrap();

        let outcome = evict_corrupt_entity_file(&storage, &entity_id).await;
        assert_eq!(
            outcome,
            "the local copy has since been replaced by a hash-valid one, which was kept"
        );
        assert!(storage.exist(&entity_id).await.unwrap());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn evict_reports_an_already_removed_copy() {
        const GONE_JSON: &[u8] = br#"{"type":"scene","content":[],"t":"gone"}"#;

        let (storage, tmp) = temp_storage("gone").await;
        let entity_id = hash_bytes_v1(GONE_JSON);

        let outcome = evict_corrupt_entity_file(&storage, &entity_id).await;
        assert_eq!(
            outcome,
            "the corrupt local copy was already removed; \
             a later retry will re-download it"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn concurrent_evictions_of_the_same_entity_serialize() {
        const RACE_JSON: &[u8] = br#"{"type":"scene","content":[],"t":"race"}"#;

        let (storage, tmp) = temp_storage("race").await;
        let entity_id = hash_bytes_v1(RACE_JSON);
        storage
            .store(&entity_id, bytes::Bytes::from_static(CORRUPT_BYTES))
            .await
            .unwrap();

        let mut join = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let storage = storage.clone();
            let entity_id = entity_id.clone();
            join.spawn(async move { evict_corrupt_entity_file(&storage, &entity_id).await });
        }
        let mut outcomes: Vec<&'static str> = Vec::new();
        while let Some(res) = join.join_next().await {
            outcomes.push(res.unwrap());
        }
        outcomes.sort();

        assert!(outcomes.contains(&"removed the corrupt local copy so it can be re-downloaded"));
        assert!(!storage.exist(&entity_id).await.unwrap());
        assert!(!eviction_locks().lock().contains_key(&entity_id));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(unix)]
    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn evict_reports_failure_when_delete_fails() {
        if running_as_root() {
            eprintln!("skipping: permissions do not bind when running as root");
            return;
        }
        const DELFAIL_JSON: &[u8] = br#"{"type":"scene","content":[],"t":"delfail"}"#;

        let (storage, tmp) = temp_storage("delfail").await;
        let entity_id = hash_bytes_v1(DELFAIL_JSON);
        storage
            .store(&entity_id, bytes::Bytes::from_static(CORRUPT_BYTES))
            .await
            .unwrap();

        let shard_dir = storage.root().join(hex_prefix(&entity_id));
        set_mode(&shard_dir, 0o555);

        let outcome = evict_corrupt_entity_file(&storage, &entity_id).await;
        assert_eq!(
            outcome,
            "could not remove the corrupt local copy; \
             a later retry will attempt it again"
        );
        set_mode(&shard_dir, 0o755);
        assert!(
            storage.exist(&entity_id).await.unwrap(),
            "the copy must still be on disk"
        );

        let outcome = evict_corrupt_entity_file(&storage, &entity_id).await;
        assert_eq!(
            outcome,
            "removed the corrupt local copy so it can be re-downloaded"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn evict_reports_fault_not_removal_when_reverify_cannot_read() {
        if running_as_root() {
            eprintln!("skipping: permissions do not bind when running as root");
            return;
        }

        const REVERIFY_FAULT_JSON: &[u8] = br#"{"type":"scene","content":[],"t":"reverify"}"#;

        let (storage, tmp) = temp_storage("reverify-fault").await;
        let entity_id = hash_bytes_v1(REVERIFY_FAULT_JSON);
        storage
            .store(&entity_id, bytes::Bytes::from_static(CORRUPT_BYTES))
            .await
            .unwrap();

        let shard_dir = storage.root().join(hex_prefix(&entity_id));
        set_mode(&shard_dir, 0o000);

        let outcome = evict_corrupt_entity_file(&storage, &entity_id).await;
        assert_eq!(
            outcome,
            "the local copy could not be re-verified; \
             it was kept and a later retry will re-check it"
        );

        set_mode(&shard_dir, 0o755);
        assert!(storage.exist(&entity_id).await.unwrap());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn gzip_labeled_content_decodes_then_hashes_and_stores_decoded() {
        const PAYLOAD: &[u8] = br#"{"type":"scene","content":[],"t":"gzip-wire"}"#;
        let cid = hash_bytes_v1(PAYLOAD);
        let wire = gzip(PAYLOAD);
        let (base, server) = spawn_content_server(wire, Some("gzip")).await;
        let (storage, tmp) = temp_storage("gzip-wire").await;

        download_file_with_retries(&reqwest::Client::new(), &storage, &cid, &[base], None)
            .await
            .expect("a gzip-labeled body whose decoded bytes match the CID must be accepted");

        let stored = storage.retrieve(&cid).await.unwrap().unwrap();
        assert_eq!(
            &stored[..],
            PAYLOAD,
            "the stored bytes must be the decoded representation"
        );
        assert!(verify_hash(&stored, &cid));

        server.abort();
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn gzip_labeled_body_whose_raw_bytes_match_the_cid_is_rejected() {
        const PAYLOAD: &[u8] = br#"{"type":"scene","content":[],"t":"raw-labeled"}"#;
        let cid = hash_bytes_v1(PAYLOAD);
        let (base, server) = spawn_content_server(PAYLOAD.to_vec(), Some("gzip")).await;
        let (storage, tmp) = temp_storage("raw-labeled").await;

        let err =
            download_file_with_retries(&reqwest::Client::new(), &storage, &cid, &[base], None)
                .await
                .expect_err("a non-gzip body labeled gzip must fail transfer decoding");
        assert!(
            err.to_string().contains("gzip transfer decoding failed"),
            "{err}"
        );
        assert!(!storage.exist(&cid).await.unwrap());

        server.abort();
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn corrupt_gzip_content_still_fails_hash_verification() {
        const PAYLOAD: &[u8] = br#"{"type":"scene","content":[],"t":"expected"}"#;
        const OTHER: &[u8] = br#"{"type":"scene","content":[],"t":"corrupted"}"#;
        let cid = hash_bytes_v1(PAYLOAD);
        let wire = gzip(OTHER);
        let (base, server) = spawn_content_server(wire, Some("gzip")).await;
        let (storage, tmp) = temp_storage("gzip-corrupt").await;

        let err =
            download_file_with_retries(&reqwest::Client::new(), &storage, &cid, &[base], None)
                .await
                .expect_err("decoded bytes that mismatch the CID must still be rejected");
        assert!(err.to_string().contains("content hash mismatch"), "{err}");
        assert!(!storage.exist(&cid).await.unwrap());

        server.abort();
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    fn decode(encoding: Option<&str>, wire: &[u8], piece: usize) -> Result<Vec<u8>, String> {
        let mut decoder = TransferDecoder::for_encoding(encoding)?;
        let mut out = Vec::new();
        for chunk in wire.chunks(piece.max(1)) {
            decoder.push(chunk, &mut out, usize::MAX)?;
        }
        decoder.finish(&mut out)?;
        Ok(out)
    }

    #[test]
    fn a_gzip_bomb_stops_near_the_limit_within_one_wire_chunk() {
        let wire = gzip(&vec![0u8; 64 << 20]);
        assert!(wire.len() < 128 << 10, "{}", wire.len());
        let mut decoder = TransferDecoder::for_encoding(Some("gzip")).unwrap();
        let mut out = Vec::new();
        let err = decoder.push(&wire, &mut out, 1 << 20).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        assert!(out.len() < 2 << 20, "inflated {} bytes", out.len());

        let mut decoder = TransferDecoder::for_encoding(Some("gzip")).unwrap();
        let mut out = Vec::new();
        decoder.push(&wire, &mut out, 64 << 20).unwrap();
        decoder.finish(&mut out).unwrap();
        assert_eq!(out.len(), 64 << 20);
    }

    #[test]
    fn identity_and_missing_encodings_pass_through() {
        let body = b"raw bytes".to_vec();
        assert_eq!(decode(None, &body, 3).unwrap(), body);
        assert_eq!(decode(Some("identity"), &body, 3).unwrap(), body);
        assert_eq!(decode(Some(" Identity "), &body, 3).unwrap(), body);
    }

    #[test]
    fn gzip_and_x_gzip_decode_to_the_original_bytes_in_any_chunking() {
        let original = b"### Decentraland json snapshot\n{\"entityId\":\"x\"}\n".to_vec();
        let wire = gzip(&original);
        for piece in [1, 7, wire.len()] {
            assert_eq!(decode(Some("gzip"), &wire, piece).unwrap(), original);
            assert_eq!(decode(Some("X-Gzip"), &wire, piece).unwrap(), original);
        }
    }

    #[test]
    fn non_gzip_bytes_labeled_gzip_fail_decode() {
        let err = decode(Some("gzip"), b"not gzip at all", 64)
            .expect_err("mislabeled body must be rejected, never hashed as-is");
        assert!(err.contains("gzip transfer decoding failed"), "{err}");
    }

    #[test]
    fn unsupported_encoding_is_rejected() {
        for enc in ["br", "zstd", "deflate", "gzip, br"] {
            let err = TransferDecoder::for_encoding(Some(enc))
                .err()
                .expect("unsupported coding must be rejected, never hashed as-is");
            assert!(err.contains("unsupported content-encoding"), "{err}");
        }
    }

    #[tokio::test]
    async fn decoded_size_cap_is_enforced_while_streaming() {
        let wire = gzip(&vec![0u8; 4096]);
        let (base, server) = spawn_content_server(wire, Some("gzip")).await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/contents/bomb"))
            .send()
            .await
            .unwrap();
        let mut sink = BufferSink::new(None, None);
        let err = stream_body(resp, 100, &mut sink)
            .await
            .expect_err("a compression bomb must not outgrow the caller's cap");
        assert!(err.contains("exceeds 100 byte cap"), "{err}");
        server.abort();
    }

    /// Records the largest run handed over, keeping no bytes.
    struct LargestRun(usize, u64);

    impl DecodedSink for LargestRun {
        async fn accept(&mut self, decoded: &[u8]) -> Result<(), String> {
            self.0 = self.0.max(decoded.len());
            self.1 += decoded.len() as u64;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_highly_compressed_body_reaches_the_sink_in_bounded_runs() {
        let wire = gzip(&vec![0u8; 32 << 20]);
        let (base, server) = spawn_content_server(wire, Some("gzip")).await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/contents/zeros"))
            .send()
            .await
            .unwrap();
        let mut sink = LargestRun(0, 0);
        let stats = stream_body(resp, 64 << 20, &mut sink).await.unwrap();
        assert_eq!((stats.decoded_len, sink.1), (32 << 20, 32 << 20));
        assert!(
            sink.0 < SINK_FLUSH_BYTES + (300 << 10),
            "largest run {}",
            sink.0
        );
        server.abort();
    }

    #[test]
    fn a_hold_is_the_body_size_or_the_whole_budget() {
        assert_eq!(permits_for(0, 100), 0);
        assert_eq!(permits_for(60, 100), 60);
        assert_eq!(permits_for(100, 100), 100);
        assert_eq!(
            permits_for(u64::MAX, 100),
            100,
            "a larger body proceeds alone"
        );
    }

    #[test]
    fn growth_is_stepped_bounded_and_idle_once_covered_or_whole() {
        assert_eq!(growth_for(10, 5, 100, 8), 0, "already covered");
        assert_eq!(growth_for(10, 10, 100, 8), 0, "exactly covered");
        assert_eq!(
            growth_for(10, 11, 100, 8),
            8,
            "one byte over grows by a whole step"
        );
        assert_eq!(
            growth_for(10, 50, 100, 8),
            40,
            "a big jump grows to what is needed"
        );
        assert_eq!(growth_for(10, 500, 100, 8), 90, "never past the budget");
        assert_eq!(
            growth_for(100, 500, 100, 8),
            0,
            "the whole budget held: nothing to ask"
        );
        assert_eq!(
            growth_for(98, 99, 100, 8),
            2,
            "a step is clipped to the room left"
        );
    }

    #[test]
    fn a_budget_is_clamped_to_one_hold_and_never_zero() {
        assert_eq!(ByteBudget::new(0).max_bytes(), 1);
        assert_eq!(ByteBudget::new(1 << 30).max_bytes(), 1 << 30);
        assert_eq!(ByteBudget::new(u64::MAX).max_bytes(), u32::MAX as u64);
    }

    #[tokio::test]
    async fn a_reservation_holds_grows_and_releases() {
        let budget = ByteBudget::new(100);
        let mut first = budget.reserve(60).await.unwrap();
        assert_eq!((first.held(), budget.in_use()), (60, 60));
        first.cover(61).await.unwrap();
        assert_eq!(
            (first.held(), budget.in_use()),
            (100, 100),
            "a growth step past the room left takes the room left"
        );
        first.cover(10_000).await.unwrap();
        assert_eq!(
            first.held(),
            100,
            "holding the whole budget, there is nothing to wait for"
        );
        drop(first);
        assert_eq!(budget.in_use(), 0);
    }

    #[tokio::test]
    async fn a_body_larger_than_the_budget_waits_for_everyone_then_proceeds_alone() {
        let budget = ByteBudget::new(100);
        let holder = budget.reserve(40).await.unwrap();
        let mut big = std::pin::pin!(async { budget.reserve(1_000).await.unwrap().held() });
        assert!(
            futures::poll!(&mut big).is_pending(),
            "the large body waits for the 40 bytes in flight"
        );
        drop(holder);
        assert_eq!(big.await, 100, "then it holds the whole budget");
        assert_eq!(budget.in_use(), 0, "and releases it on drop");
    }
}
