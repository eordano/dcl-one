//! A peer's `/snapshots` list and the snapshot files it names: validated
//! metadata, the processed/replaced decision, the hash-verified download
//! into the snapshot tree, and the line-by-line hand-off of every entity to
//! the batch deployer.

use std::collections::HashSet;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures::StreamExt;
use reqwest::Client;
use sha2::Digest;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use super::pointer_changes::is_usable_timestamp;
use super::{SnapshotMetadata, SyncDeployment, SyncError, TimeRange, Timestamp};
use crate::auth::{HashV1Writer, is_canonical_cid};
use crate::batch_deployer::{BatchDeployer, DeploymentReport};
use crate::deploy_remote_entity::{BodyStats, DecodedSink, stream_body};
use crate::http::read_body_capped;
use crate::storage::{StagingGuard, Storage, create_staging_file, resolve_file_path, staging_path};

/// Cap on a snapshot body, wire and decoded, enforced as it streams to disk.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024 * 1024;
/// `GET /snapshots` lists a handful of snapshot descriptors per entity type.
const MAX_SNAPSHOT_LIST_BYTES: usize = 16 * 1024 * 1024;

const MAX_DECOMPRESSED_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Longest snapshot line read before it is dropped as unreadable and the reader skips to the
/// next newline; a real line is an entity's id, pointers and auth chain, kilobytes.
const MAX_SNAPSHOT_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Records parsed ahead of the deployer per batch: what one scheduling round holds in memory
/// (an entity's pointers and auth chain each) instead of a snapshot's every line.
const SNAPSHOT_BATCH_RECORDS: usize = 1000;

/// Upstream MAX_REPLACED_SNAPSHOT_HASHES. Every entry lands in the batched processed-snapshots
/// lookup, so a pathological list sizes that query; real snapshots replace tens.
const MAX_REPLACED_SNAPSHOT_HASHES: usize = 1000;

/// How many invalid /snapshots entries are logged in detail per response before the rest
/// collapse into one summary line -- the body is attacker-sized.
const MAX_INVALID_SNAPSHOT_LOGS: usize = 5;
/// Longest logged preview of an invalid snapshot entry, in bytes.
const SNAPSHOT_PREVIEW_BYTES: usize = 512;

/// The logged form of an invalid entry, cut on a character boundary at or
/// below [`SNAPSHOT_PREVIEW_BYTES`].
fn entry_preview(item: &serde_json::Value) -> String {
    let mut preview = item.to_string();
    preview.truncate(preview.floor_char_boundary(SNAPSHOT_PREVIEW_BYTES));
    preview
}

static SPOOL_SEQ: AtomicU64 = AtomicU64::new(0);

/// The stored snapshot as a byte stream. Snapshot files are gzip on the wire (peers serve them
/// stored) and are unwrapped on the way; a plain file passes through. The caller bounds the
/// read ([`MAX_DECOMPRESSED_SNAPSHOT_BYTES`]).
fn open_snapshot_reader<R: Read + 'static>(mut raw: R) -> std::io::Result<Box<dyn Read>> {
    let mut head = [0u8; 2];
    let mut got = 0;
    while got < head.len() {
        let n = raw.read(&mut head[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    let rejoined = std::io::Cursor::new(head[..got].to_vec()).chain(raw);
    Ok(if got == head.len() && head == [0x1f, 0x8b] {
        Box::new(flate2::read::MultiGzDecoder::new(rejoined))
    } else {
        Box::new(rejoined)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineRead {
    Line,
    Overlong,
    Eof,
}

/// The next line of `reader` into `buf` (newline included; the last line may lack one), or
/// `Overlong` once it passes `max_line`, in which case the rest of it is consumed and `buf`
/// comes back empty: the whole-file `lines()` this replaces had no bound on one line's size.
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_line: usize,
) -> std::io::Result<LineRead> {
    buf.clear();
    let mut overlong = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if overlong {
                LineRead::Overlong
            } else if buf.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line
            });
        }
        let (used, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if !overlong {
            if buf.len() + used > max_line {
                overlong = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&available[..used]);
            }
        }
        reader.consume(used);
        if done {
            return Ok(if overlong {
                LineRead::Overlong
            } else {
                LineRead::Line
            });
        }
    }
}

/// The content hash of a body that arrives in pieces, for either CID scheme a snapshot is named
/// by: `Qm` is the SHA-256 of the bytes as a CIDv0, `ba` the chunked CIDv1 [`HashV1Writer`]
/// computes. What `verify_hash` does over a whole buffer, without the buffer.
enum CidHasher {
    V0(sha2::Sha256),
    V1(HashV1Writer),
}

impl CidHasher {
    fn for_cid(expected: &str) -> Option<Self> {
        if !is_canonical_cid(expected) {
            return None;
        }
        if expected.starts_with("Qm") {
            Some(Self::V0(sha2::Sha256::new()))
        } else if expected.starts_with("ba") {
            Some(Self::V1(HashV1Writer::new()))
        } else {
            None
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::V0(sha) => sha.update(data),
            Self::V1(writer) => writer.update(data),
        }
    }

    fn finish(self) -> String {
        match self {
            Self::V0(sha) => {
                // `auth::cid::cid_v0_to_string` (private to `auth`): the SHA-256 multihash,
                // base58btc.
                let mut multihash = Vec::with_capacity(34);
                multihash.extend_from_slice(&[0x12, 0x20]);
                multihash.extend_from_slice(&sha.finalize());
                bs58::encode(&multihash).into_string()
            }
            Self::V1(writer) => writer.finish(),
        }
    }
}

/// Port of upstream 53e9c07's `SnapshotsFromServer`. Each snapshot stands for a whole time range, so
/// a discarded entry is a range nothing else covers, and treating the surviving subset as the
/// server's complete history would advance the frontier past those entities forever: callers that
/// record sync progress must check `discarded`, not just read `snapshots`.
#[derive(Debug, Clone)]
pub struct SnapshotsFromServer {
    pub snapshots: Vec<SnapshotMetadata>,
    pub discarded: usize,
}

/// Upstream `isValidSnapshotMetadata`. Snapshot metadata comes from untrusted servers, and
/// `time_range.end_timestamp` is what bootstrap installs as the server's last-snapshot timestamp and,
/// one poll boundary later, the durable GREATEST-monotonic frontier -- the same hazard the
/// pointer-changes timestamps are checked for.
///
/// `number_of_entities` and `generation_timestamp` are deliberately NOT validated (they default to 0
/// when missing or wrong-typed): nothing downstream reads them, and rejecting an entry over an unread
/// field would silently stop syncing from a server that reports, say, `numberOfEntities: "5"`.
pub(crate) fn parse_snapshot_metadata(
    value: &serde_json::Value,
    now_ms: Timestamp,
) -> Option<SnapshotMetadata> {
    let hash = value.get("hash")?.as_str()?;
    if !is_canonical_cid(hash) {
        return None;
    }

    let time_range = value.get("timeRange")?;
    let init_timestamp = time_range.get("initTimestamp")?.as_i64()?;
    let end_timestamp = time_range.get("endTimestamp")?.as_i64()?;
    if !is_usable_timestamp(init_timestamp, now_ms)
        || !is_usable_timestamp(end_timestamp, now_ms)
        || init_timestamp > end_timestamp
    {
        return None;
    }

    let replaced_snapshot_hashes = match value.get("replacedSnapshotHashes") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(hashes)) => {
            if hashes.len() > MAX_REPLACED_SNAPSHOT_HASHES {
                return None;
            }
            let mut replaced = Vec::with_capacity(hashes.len());
            for h in hashes {
                let h = h.as_str()?;
                if !is_canonical_cid(h) {
                    return None;
                }
                replaced.push(h.to_string());
            }
            Some(replaced)
        }
        Some(_) => return None,
    };

    Some(SnapshotMetadata {
        hash: hash.to_string(),
        time_range: TimeRange {
            init_timestamp,
            end_timestamp,
        },
        number_of_entities: value
            .get("numberOfEntities")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        replaced_snapshot_hashes,
        generation_timestamp: value
            .get("generationTimestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
    })
}

pub(crate) fn parse_snapshots_response(
    items: &[serde_json::Value],
    now_ms: Timestamp,
    server: &str,
) -> SnapshotsFromServer {
    let mut snapshots = Vec::with_capacity(items.len());
    let mut discarded = 0usize;
    for item in items {
        match parse_snapshot_metadata(item, now_ms) {
            Some(snapshot) => snapshots.push(snapshot),
            None => {
                discarded += 1;
                if discarded <= MAX_INVALID_SNAPSHOT_LOGS {
                    let preview = entry_preview(item);
                    warn!(server, entry = %preview, "Ignoring invalid snapshot metadata entry");
                }
            }
        }
    }
    if discarded > MAX_INVALID_SNAPSHOT_LOGS {
        warn!(
            server,
            total = discarded,
            "Ignored additional invalid snapshot metadata entries"
        );
    }
    snapshots.sort_by_key(|s| std::cmp::Reverse(s.time_range.end_timestamp));
    SnapshotsFromServer {
        snapshots,
        discarded,
    }
}

pub async fn fetch_snapshots(
    client: &Client,
    server: &str,
    max_retries: u32,
) -> Result<SnapshotsFromServer, SyncError> {
    let url = format!("{}/snapshots", server);

    let mut last_error = None;
    for attempt in 0..max_retries {
        match client
            .get(&url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    let body = read_body_capped(resp, MAX_SNAPSHOT_LIST_BYTES)
                        .await
                        .map_err(|e| {
                            SyncError::Other(format!("snapshot list from {server}: {e}"))
                        })?;
                    let items: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    return Ok(parse_snapshots_response(&items, now_ms, server));
                } else {
                    let status = resp.status();
                    warn!(url = %url, %status, attempt, "Snapshot fetch failed");
                    last_error = Some(SyncError::Other(format!(
                        "HTTP {} fetching snapshots from {}",
                        status, server
                    )));
                }
            }
            Err(e) => {
                warn!(url = %url, error = %e, attempt, "Snapshot fetch request failed");
                last_error = Some(SyncError::Http(e));
            }
        }

        if attempt + 1 < max_retries {
            // Doubles to four times the wait: a dead peer costs the bootstrap
            // seconds, not minutes (peers are fetched one at a time).
            tokio::time::sleep(std::time::Duration::from_millis(
                500u64.saturating_mul(1u64 << attempt.min(2)),
            ))
            .await;
        }
    }

    Err(last_error.unwrap_or_else(|| {
        SyncError::Other(format!(
            "Failed to fetch snapshots from {} after {} retries",
            server, max_retries
        ))
    }))
}

pub async fn download_snapshot_files(
    client: &Client,
    storage: Arc<Storage>,
    snapshots: &[(String, HashSet<String>)],
    max_retries: u32,
    retry_wait_ms: u64,
) {
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    for (hash, servers) in snapshots {
        let client = client.clone();
        let storage = storage.clone();
        let hash = hash.clone();
        let servers = servers.clone();

        tasks.spawn(async move {
            if let Err(e) = download_snapshot_file(
                &client,
                storage.as_ref(),
                &hash,
                &servers,
                max_retries,
                retry_wait_ms,
            )
            .await
            {
                warn!(snapshot_hash = %hash, error = %e, "Failed to pre-download snapshot");
            } else {
                info!(snapshot_hash = %hash, "Snapshot pre-downloaded");
            }
        });
    }

    while let Some(result) = tasks.join_next().await {
        if let Err(e) = result {
            warn!(error = %e, "Snapshot download task panicked");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotDecision {
    /// Unless the node's own snapshot store already has it -- the one async check the caller still
    /// owns; its result never changes within a pass.
    Deploy,
    /// Some advertised replacement group is fully processed, so this snapshot's content is
    /// already covered: persist a processed mark for it and skip.
    MarkProcessed,
    /// Already processed, or entirely older than the genesis timestamp.
    Skip,
}

/// Pure core of upstream's `decideSnapshotDeploymentFromProcessedSet`, taking an already-fetched set
/// of processed hashes so a pass costs as few storage round trips as `filter_processed_in_chunks`
/// needs rather than one per snapshot.
///
/// `replaced_groups` carries one group per advertising server, and `.any()` over them matches
/// upstream's `.some()`: two peers advertising the same content hash with different replacement
/// histories must reach the same verdict regardless of which was fetched first.
///
/// On MarkProcessed the hash is added to `processed` -- mutating the caller's set is what makes the
/// snapshot that replaces THIS one skippable later in the same pass; see [`decide_snapshot_pass`].
pub(crate) fn decide_snapshot_deployment_from_processed_set(
    processed: &mut HashSet<String>,
    genesis_timestamp: Timestamp,
    snapshot_hash: &str,
    greatest_end_timestamp: Timestamp,
    replaced_groups: &[Vec<String>],
) -> SnapshotDecision {
    let snapshot_was_processed = processed.contains(snapshot_hash);
    let a_replaced_group_was_processed = replaced_groups
        .iter()
        .any(|group| !group.is_empty() && group.iter().all(|h| processed.contains(h)));

    if !snapshot_was_processed {
        if !a_replaced_group_was_processed {
            if greatest_end_timestamp > genesis_timestamp {
                return SnapshotDecision::Deploy;
            }
            return SnapshotDecision::Skip;
        }
        processed.insert(snapshot_hash.to_string());
        return SnapshotDecision::MarkProcessed;
    }
    SnapshotDecision::Skip
}

/// Fixed point of the deployment decision over one pass's candidates (upstream 53e9c07's pass loop in
/// `syncFromSnapshotsExclusively`). A mark is exactly what makes the snapshot replacing IT skippable
/// in turn, so a single pass over a replacement chain (h2 replaces h1 replaces a processed h0)
/// collapses only one link and deploys the tail for nothing. The loop terminates because "already
/// processed" and "older than genesis" drop out for good the first time, and every other pass either
/// marks at least one snapshot or ends it.
///
/// `candidates` is `(hash, greatest_end_timestamp_across_advertisers, replaced-group-per-advertiser)`.
/// The caller persists `newly_marked` and applies its own snapshot-store check to `to_deploy`.
/// Iteration order follows `candidates`, so callers pass a deterministic order.
pub(crate) fn decide_snapshot_pass(
    candidates: &[(String, Timestamp, Vec<Vec<String>>)],
    processed: &mut HashSet<String>,
    genesis_timestamp: Timestamp,
) -> (Vec<String>, Vec<String>) {
    let mut newly_marked: Vec<String> = Vec::new();
    let mut still_deployable: Vec<&(String, Timestamp, Vec<Vec<String>>)> =
        candidates.iter().collect();
    loop {
        let marked_before = processed.len();
        let mut next_round = Vec::with_capacity(still_deployable.len());
        for candidate in still_deployable {
            let (hash, greatest_end, groups) = candidate;
            match decide_snapshot_deployment_from_processed_set(
                processed,
                genesis_timestamp,
                hash,
                *greatest_end,
                groups,
            ) {
                SnapshotDecision::Deploy => next_round.push(candidate),
                SnapshotDecision::MarkProcessed => newly_marked.push(hash.clone()),
                SnapshotDecision::Skip => {}
            }
        }
        if processed.len() == marked_before {
            return (
                next_round.into_iter().map(|(h, _, _)| h.clone()).collect(),
                newly_marked,
            );
        }
        still_deployable = next_round;
    }
}

/// The same timestamp-plausibility gate /pointer-changes deltas get (upstream applies
/// `isUsableTimestamp` at both ingestion points plus /snapshots metadata).
fn usable_snapshot_line(deployment: &SyncDeployment, now_ms: Timestamp) -> bool {
    is_usable_timestamp(deployment.entity_timestamp, now_ms)
        && deployment
            .local_timestamp
            .is_none_or(|ts| is_usable_timestamp(ts, now_ms))
}

pub struct SnapshotSource<'a> {
    pub snapshot_hash: &'a str,
    pub servers: &'a HashSet<String>,
    pub max_retries: u32,
    pub retry_wait_ms: u64,
}

pub struct SnapshotFilter<'a> {
    pub genesis_timestamp: Timestamp,
    pub entity_type_filter: Option<&'a HashSet<String>>,
}

/// How many entities of one snapshot are handed to the deployer at once: the old
/// `SYNC_NONSCENE_CONCURRENCY` (64) and `SYNC_SCENE_CONCURRENCY` (1). Scenes are scheduled
/// after everything else and one at a time, because their content sets are the large ones.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotScheduling {
    pub nonscene_concurrency: usize,
    pub scene_concurrency: usize,
}

impl Default for SnapshotScheduling {
    fn default() -> Self {
        Self {
            nonscene_concurrency: 64,
            scene_concurrency: 1,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ParseStats {
    total: u64,
    parse_errors: u64,
    skipped_by_filter: u64,
}

/// The per-line decision, one snapshot's worth of state, run on the blocking thread that reads
/// the file: the JSON shape, the timestamp-plausibility gate, the genesis cutoff and the
/// entity-type allowlist. The first few skips of a snapshot are logged, the rest counted.
struct SnapshotLineFilter {
    snapshot_hash: String,
    genesis_timestamp: Timestamp,
    entity_type_filter: Option<HashSet<String>>,
    now_ms: Timestamp,
    stats: ParseStats,
}

impl SnapshotLineFilter {
    fn skip(&mut self, message: &str, error: Option<String>) {
        self.stats.parse_errors += 1;
        if self.stats.parse_errors <= 5 {
            warn!(
                snapshot_hash = %self.snapshot_hash,
                error = error.as_deref().unwrap_or(""),
                "{message}"
            );
        }
    }

    fn parse(&mut self, line: &[u8]) -> Option<SyncDeployment> {
        let text = String::from_utf8_lossy(line);
        let trimmed = text.trim();
        if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
            if !(trimmed.is_empty() || trimmed.starts_with("###")) {
                self.skip("Skipping unreadable snapshot line", None);
            }
            return None;
        }
        self.stats.total += 1;
        let deployment: SyncDeployment = match serde_json::from_str(trimmed) {
            Ok(d) => d,
            Err(e) => {
                self.skip("Skipping unparseable snapshot entry", Some(e.to_string()));
                return None;
            }
        };
        if !usable_snapshot_line(&deployment, self.now_ms) {
            self.skip("Skipping snapshot line with implausible timestamp", None);
            return None;
        }
        if deployment.entity_timestamp < self.genesis_timestamp {
            return None;
        }
        if let Some(filter) = &self.entity_type_filter
            && !filter.contains(&deployment.entity_type)
        {
            self.stats.skipped_by_filter += 1;
            return None;
        }
        Some(deployment)
    }
}

/// Reads the stored snapshot line by line on the blocking pool and hands the deployable records
/// to `tx` in batches of [`SNAPSHOT_BATCH_RECORDS`]. Returns once the file is read, or as soon
/// as the receiver is gone (the consumer stopped). A file that inflates past
/// [`MAX_DECOMPRESSED_SNAPSHOT_BYTES`] or cannot be read is an error, not a shorter snapshot.
fn read_snapshot_records(
    path: PathBuf,
    mut filter: SnapshotLineFilter,
    tx: tokio::sync::mpsc::Sender<Vec<SyncDeployment>>,
) -> Result<ParseStats, String> {
    let file =
        std::fs::File::open(&path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    let reader =
        open_snapshot_reader(file).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut reader = std::io::BufReader::with_capacity(
        256 * 1024,
        reader.take(MAX_DECOMPRESSED_SNAPSHOT_BYTES + 1),
    );
    let mut line = Vec::with_capacity(16 * 1024);
    let mut batch = Vec::with_capacity(SNAPSHOT_BATCH_RECORDS);
    loop {
        match read_line_capped(&mut reader, &mut line, MAX_SNAPSHOT_LINE_BYTES)
            .map_err(|e| format!("reading {}: {e}", path.display()))?
        {
            LineRead::Eof => break,
            LineRead::Overlong => filter.skip("Skipping overlong snapshot line", None),
            LineRead::Line => {
                if let Some(record) = filter.parse(&line) {
                    batch.push(record);
                    if batch.len() >= SNAPSHOT_BATCH_RECORDS {
                        let full = std::mem::replace(
                            &mut batch,
                            Vec::with_capacity(SNAPSHOT_BATCH_RECORDS),
                        );
                        if tx.blocking_send(full).is_err() {
                            return Ok(filter.stats);
                        }
                    }
                }
            }
        }
    }
    if reader.get_ref().limit() == 0 {
        return Err(format!(
            "decompressed to more than {MAX_DECOMPRESSED_SNAPSHOT_BYTES} bytes"
        ));
    }
    if !batch.is_empty() {
        let _ = tx.blocking_send(batch);
    }
    Ok(filter.stats)
}

#[derive(Debug, Default, Clone, Copy)]
struct ScheduleOutcome {
    scheduled: u64,
    errors: u64,
    stopped: bool,
}

impl ScheduleOutcome {
    fn absorb(&mut self, other: Self) {
        self.scheduled += other.scheduled;
        self.errors += other.errors;
        self.stopped |= other.stopped;
    }
}

async fn schedule_records(
    deployer: &BatchDeployer,
    records: Vec<SyncDeployment>,
    concurrency: usize,
    servers: &[String],
    report: &Arc<DeploymentReport>,
    snapshot_hash: &str,
    should_stop: &impl Fn() -> bool,
) -> ScheduleOutcome {
    let mut records = records;
    let offered = records.len() as u64;
    let probed = match deployer.retain_undeployed(&mut records).await {
        Ok(()) => true,
        Err(e) => {
            warn!(snapshot_hash, error = %e, "Batched already-deployed probe failed; probing per record");
            false
        }
    };
    let scheduled = AtomicU64::new(offered - records.len() as u64);
    let errors = AtomicU64::new(0);
    let stopped = AtomicBool::new(false);
    let (scheduled_ref, errors_ref, stopped_ref) = (&scheduled, &errors, &stopped);
    futures::stream::iter(records)
        .for_each_concurrent(concurrency, |deployment| async move {
            if stopped_ref.load(Ordering::Relaxed) {
                return;
            }
            if should_stop() {
                stopped_ref.store(true, Ordering::Relaxed);
                return;
            }
            let outcome = if probed {
                deployer
                    .schedule_undeployed(deployment, servers, Some(report))
                    .await
            } else {
                deployer
                    .schedule_entity_deployment(deployment, servers, Some(report))
                    .await
            };
            match outcome {
                Ok(()) => {
                    scheduled_ref.fetch_add(1, Ordering::Relaxed);
                }
                Err(SyncError::Stopped) => stopped_ref.store(true, Ordering::Relaxed),
                Err(e) => {
                    errors_ref.fetch_add(1, Ordering::Relaxed);
                    warn!(snapshot_hash, error = %e, "Failed to schedule entity deployment");
                }
            }
        })
        .await;
    ScheduleOutcome {
        scheduled: scheduled.into_inner(),
        errors: errors.into_inner(),
        stopped: stopped.into_inner(),
    }
}

/// Err when the snapshot could not be fully handed off -- download failure, unreadable lines, or
/// scheduling errors -- so the caller keeps the advertising servers in snapshot bootstrap with their
/// timestamps held back instead of advancing past entities that were never deployed. Readable
/// entities are still scheduled first.
///
/// The snapshot is read from disk as a stream ([`read_snapshot_records`]) and scheduled a batch
/// at a time: at most one batch is parsed ahead of the one being scheduled, whatever the
/// snapshot's size. Within a batch, scenes go after the other entities and one at a time
/// (`scene_concurrency`), as the whole-snapshot pass this replaces ordered them; the order
/// across batches is the file's.
///
/// `report` accumulates scheduled-vs-acknowledged counts; the caller re-checks it after the deployer
/// drains, since an Ok return only proves everything was SCHEDULED.
#[allow(
    clippy::too_many_arguments,
    reason = "source, filter and scheduling are already grouped; the rest are the caller's shared handles"
)]
pub async fn deploy_entities_from_snapshot(
    client: &Client,
    storage: &Storage,
    deployer: &BatchDeployer,
    source: SnapshotSource<'_>,
    filter: SnapshotFilter<'_>,
    scheduling: SnapshotScheduling,
    report: &Arc<DeploymentReport>,
    should_stop: impl Fn() -> bool,
) -> Result<(), SyncError> {
    let SnapshotSource {
        snapshot_hash,
        servers,
        max_retries,
        retry_wait_ms,
    } = source;
    let SnapshotFilter {
        genesis_timestamp,
        entity_type_filter,
    } = filter;
    let server_list: Vec<String> = servers.iter().cloned().collect();

    download_snapshot_file(
        client,
        storage,
        snapshot_hash,
        servers,
        max_retries,
        retry_wait_ms,
    )
    .await?;

    let path =
        storage
            .file_path(snapshot_hash)
            .await?
            .ok_or_else(|| SyncError::EntityNotFound {
                entity_id: snapshot_hash.to_string(),
            })?;

    let nonscene_concurrency = scheduling.nonscene_concurrency.max(1);
    let scene_concurrency = scheduling.scene_concurrency.max(1);

    let line_filter = SnapshotLineFilter {
        snapshot_hash: snapshot_hash.to_string(),
        genesis_timestamp,
        entity_type_filter: entity_type_filter.cloned(),
        now_ms: chrono::Utc::now().timestamp_millis(),
        stats: ParseStats::default(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<SyncDeployment>>(1);
    let reader = tokio::task::spawn_blocking(move || read_snapshot_records(path, line_filter, tx));

    let mut outcome = ScheduleOutcome::default();
    while let Some(batch) = rx.recv().await {
        if should_stop() {
            outcome.stopped = true;
            break;
        }
        let (scenes, others): (Vec<_>, Vec<_>) = batch
            .into_iter()
            .partition(|deployment| deployment.entity_type == "scene");
        for (records, concurrency) in [(others, nonscene_concurrency), (scenes, scene_concurrency)]
        {
            outcome.absorb(
                schedule_records(
                    deployer,
                    records,
                    concurrency,
                    &server_list,
                    report,
                    snapshot_hash,
                    &should_stop,
                )
                .await,
            );
            if outcome.stopped {
                break;
            }
        }
        if outcome.stopped {
            break;
        }
    }
    // A reader still going returns at its next hand-off once the receiver is gone.
    drop(rx);
    let stats = match reader.await {
        Ok(Ok(stats)) => stats,
        Ok(Err(reason)) => {
            if outcome.stopped {
                return Err(SyncError::Stopped);
            }
            return Err(SyncError::Other(format!(
                "snapshot {snapshot_hash} could not be read: {reason}"
            )));
        }
        Err(join) => {
            return Err(SyncError::Other(format!(
                "snapshot {snapshot_hash} reader failed: {join}"
            )));
        }
    };

    if outcome.stopped {
        return Err(SyncError::Stopped);
    }

    let ParseStats {
        total,
        parse_errors: num_parse_errors,
        skipped_by_filter: num_skipped_by_filter,
    } = stats;
    let num_scheduled = outcome.scheduled;
    let num_schedule_errors = outcome.errors;

    info!(
        snapshot_hash,
        total,
        num_scheduled,
        num_skipped_by_filter,
        num_parse_errors,
        num_schedule_errors,
        nonscene_concurrency,
        scene_concurrency,
        "Snapshot scheduled"
    );

    if num_parse_errors > 0 || num_schedule_errors > 0 {
        return Err(SyncError::Other(format!(
            "snapshot {} was not fully deployable: {} unreadable lines, {} schedule errors \
             (scheduled {})",
            snapshot_hash, num_parse_errors, num_schedule_errors, num_scheduled
        )));
    }

    Ok(())
}

/// The snapshot's staging file and hasher while its body streams in.
struct SnapshotSpool {
    file: tokio::fs::File,
    hasher: CidHasher,
}

impl DecodedSink for SnapshotSpool {
    async fn accept(&mut self, decoded: &[u8]) -> Result<(), String> {
        self.hasher.update(decoded);
        self.file
            .write_all(decoded)
            .await
            .map_err(|e| format!("spooling snapshot to disk: {e}"))
    }
}

/// A snapshot body streamed into the store's own staging file for its hash (the path
/// `Storage::store` writes through, `<id>.<pid>.<seq>.tmp` beside the final one, which the
/// startup sweep reclaims after a crash), transfer decoding undone and the CID computed on the
/// way, so a file of hundreds of megabytes never sits in memory. Dropped uncommitted, the
/// staging file goes with it ([`StagingGuard`]).
struct SpooledSnapshot {
    guard: StagingGuard,
    final_path: PathBuf,
    stats: BodyStats,
    digest: String,
}

impl SpooledSnapshot {
    /// The store's commit step: rename into place, then the directory entry to disk.
    async fn commit(mut self) -> Result<(), SyncError> {
        tokio::fs::rename(self.guard.path(), &self.final_path)
            .await
            .map_err(|e| SyncError::Storage(format!("committing snapshot: {e}")))?;
        self.guard.disarm();
        if let Some(parent) = self.final_path.parent()
            && let Ok(dir) = tokio::fs::File::open(parent).await
        {
            let _ = dir.sync_all().await;
        }
        Ok(())
    }
}

async fn spool_snapshot(
    storage: &Storage,
    snapshot_hash: &str,
    resp: reqwest::Response,
) -> Result<SpooledSnapshot, String> {
    let hasher = CidHasher::for_cid(snapshot_hash)
        .ok_or_else(|| format!("{snapshot_hash} is not a canonical CID"))?;
    let final_path = resolve_file_path(storage.root(), snapshot_hash).map_err(|e| e.to_string())?;
    if let Some(dir) = final_path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let seq = SPOOL_SEQ.fetch_add(1, Ordering::Relaxed);
    let (file, guard) = create_staging_file(staging_path(&final_path, "snapshot", seq))
        .await
        .map_err(|e| e.to_string())?;
    let mut sink = SnapshotSpool { file, hasher };
    let stats = stream_body(resp, MAX_BODY_BYTES, &mut sink).await?;
    let SnapshotSpool { file, hasher } = sink;
    file.sync_all()
        .await
        .map_err(|e| format!("syncing snapshot to disk: {e}"))?;
    drop(file);
    Ok(SpooledSnapshot {
        guard,
        final_path,
        stats,
        digest: hasher.finish(),
    })
}

async fn download_snapshot_file(
    client: &Client,
    storage: &Storage,
    snapshot_hash: &str,
    servers: &HashSet<String>,
    max_retries: u32,
    retry_wait_ms: u64,
) -> Result<(), SyncError> {
    if storage.exist(snapshot_hash).await? {
        debug!(snapshot_hash, "Snapshot already in storage");
        return Ok(());
    }

    let server_list: Vec<&String> = servers.iter().collect();
    if server_list.is_empty() {
        return Err(SyncError::NoServers);
    }

    let mut last_error = None;
    for retry in 0..max_retries {
        let server = server_list[retry as usize % server_list.len()];
        let url = format!("{}/contents/{}", server, snapshot_hash);

        match client.get(&url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    let expected_len = resp.content_length();
                    match spool_snapshot(storage, snapshot_hash, resp).await {
                        Ok(spooled) => {
                            if let Some(len) = expected_len
                                && spooled.stats.wire_len < len
                            {
                                warn!(
                                    snapshot_hash,
                                    %server,
                                    retry,
                                    got = spooled.stats.wire_len,
                                    expected = len,
                                    "Snapshot download TRUNCATED (short read), trying next server"
                                );
                                last_error = Some(SyncError::Other(format!(
                                    "snapshot {} from {} truncated: got {} of {} bytes",
                                    snapshot_hash, server, spooled.stats.wire_len, len
                                )));
                            } else if spooled.digest != snapshot_hash {
                                warn!(
                                    snapshot_hash,
                                    %server,
                                    retry,
                                    bytes = spooled.stats.decoded_len,
                                    "Snapshot content failed hash verification, trying next server"
                                );
                                last_error = Some(SyncError::Other(format!(
                                    "snapshot hash mismatch for {} from {}",
                                    snapshot_hash, server
                                )));
                            } else {
                                let bytes = spooled.stats.decoded_len;
                                spooled.commit().await?;
                                info!(snapshot_hash, bytes, "Snapshot downloaded");
                                return Ok(());
                            }
                        }
                        Err(reason) => {
                            warn!(
                                snapshot_hash,
                                %server,
                                retry,
                                %reason,
                                "Snapshot body could not be spooled to disk, trying next server"
                            );
                            last_error = Some(SyncError::Other(format!(
                                "snapshot {} from {}: {}",
                                snapshot_hash, server, reason
                            )));
                        }
                    }
                } else {
                    let status = resp.status();
                    warn!(snapshot_hash, %server, %status, retry, "Snapshot download failed");
                    last_error = Some(SyncError::Other(format!(
                        "HTTP {} downloading snapshot {} from {}",
                        status, snapshot_hash, server
                    )));
                }
            }
            Err(e) => {
                warn!(snapshot_hash, %server, error = %e, retry, "Snapshot download request failed");
                last_error = Some(SyncError::Http(e));
            }
        }

        if retry + 1 < max_retries {
            // Doubles to four times the wait, as the list fetch above.
            tokio::time::sleep(std::time::Duration::from_millis(
                retry_wait_ms.saturating_mul(1u64 << retry.min(2)),
            ))
            .await;
        }
    }

    Err(last_error.unwrap_or_else(|| {
        SyncError::Other(format!(
            "Failed to download snapshot {} after {} retries",
            snapshot_hash, max_retries
        ))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{hash_bytes, hash_bytes_v1};
    use crate::deploy_remote_entity::gzip;
    use crate::sync_test_support::{spawn_content_server, temp_storage, unique};

    const NOW_MS: Timestamp = 1_750_000_000_000;
    const GOOD_HASH: &str = "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
    const GOOD_HASH_2: &str = "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdH";
    const GOOD_HASH_3: &str = "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdJ";

    #[test]
    fn a_long_invalid_entry_preview_is_cut_on_a_character_boundary() {
        // `"` takes byte 0, so every `é` starts on an odd byte and byte 512 falls inside one.
        let item = serde_json::Value::String("é".repeat(400));
        let preview = entry_preview(&item);
        assert_eq!(preview, format!("\"{}", "é".repeat(255)));
        assert_eq!(preview.len(), 511);
    }

    fn entry(hash: &str, init: i64, end: i64) -> serde_json::Value {
        serde_json::json!({
            "hash": hash,
            "timeRange": { "initTimestamp": init, "endTimestamp": end },
            "numberOfEntities": 10,
            "generationTimestamp": end,
        })
    }

    #[test]
    fn far_future_and_inverted_entries_are_discarded_and_counted() {
        let items = vec![
            entry(GOOD_HASH, 0, 1_700_000_000_000),
            entry(GOOD_HASH_2, 0, 253_402_300_799_000),
            entry(GOOD_HASH_3, 1_700_000_000_000, 1_600_000_000_000),
        ];
        let result = parse_snapshots_response(&items, NOW_MS, "https://peer.test");
        assert_eq!(result.discarded, 2);
        assert_eq!(result.snapshots.len(), 1);
        assert_eq!(result.snapshots[0].hash, GOOD_HASH);
    }

    #[test]
    fn valid_entry_parses_with_replaced_hashes_and_sorts_newest_first() {
        let mut older = entry(GOOD_HASH, 0, 1_600_000_000_000);
        older["replacedSnapshotHashes"] = serde_json::json!([GOOD_HASH_2]);
        let newer = entry(GOOD_HASH_3, 0, 1_700_000_000_000);
        let result = parse_snapshots_response(&[older, newer], NOW_MS, "s");
        assert_eq!(result.discarded, 0);
        assert_eq!(result.snapshots[0].hash, GOOD_HASH_3, "newest first");
        assert_eq!(
            result.snapshots[1].replaced_snapshot_hashes.as_deref(),
            Some(&[GOOD_HASH_2.to_string()][..])
        );
    }

    #[test]
    fn unread_fields_never_reject_an_entry() {
        let mut e = entry(GOOD_HASH, 0, 1_700_000_000_000);
        e["numberOfEntities"] = serde_json::json!("5");
        e["generationTimestamp"] = serde_json::json!("not a number");
        let parsed = parse_snapshot_metadata(&e, NOW_MS).expect("entry must survive");
        assert_eq!(parsed.number_of_entities, 0);
        assert_eq!(parsed.generation_timestamp, 0);
    }

    #[test]
    fn malformed_hash_or_replaced_hashes_reject_the_entry() {
        let traversal = entry("../../etc/passwd", 0, 1_700_000_000_000);
        assert!(parse_snapshot_metadata(&traversal, NOW_MS).is_none());

        let mut bad_replaced = entry(GOOD_HASH, 0, 1_700_000_000_000);
        bad_replaced["replacedSnapshotHashes"] = serde_json::json!(["not-a-cid"]);
        assert!(parse_snapshot_metadata(&bad_replaced, NOW_MS).is_none());

        let mut wrong_type = entry(GOOD_HASH, 0, 1_700_000_000_000);
        wrong_type["replacedSnapshotHashes"] = serde_json::json!("QmSingle");
        assert!(parse_snapshot_metadata(&wrong_type, NOW_MS).is_none());

        let mut fractional = entry(GOOD_HASH, 0, 1_700_000_000_000);
        fractional["timeRange"]["endTimestamp"] = serde_json::json!(1.7e12 + 0.5);
        assert!(
            parse_snapshot_metadata(&fractional, NOW_MS).is_none(),
            "epoch milliseconds are integers; a fraction only ever indicates a malformed server"
        );
    }

    #[test]
    fn snapshot_line_timestamps_get_the_plausibility_gate() {
        let line = |entity_ts: &str, local_ts: Option<&str>| {
            let local = local_ts.map_or(String::new(), |ts| format!(",\"localTimestamp\":{ts}"));
            format!(
                "{{\"entityId\":\"QmLine\",\"entityType\":\"scene\",\"pointers\":[\"0,0\"],\"authChain\":[],\"entityTimestamp\":{entity_ts}{local}}}"
            )
        };
        let parse = |s: &str| serde_json::from_str::<SyncDeployment>(s).unwrap();
        let now_ms = 1_700_000_000_000;

        assert!(usable_snapshot_line(
            &parse(&line("1600000000000", None)),
            now_ms
        ));
        assert!(
            !usable_snapshot_line(&parse(&line("253402300800000", None)), now_ms),
            "year-9999 entityTimestamp would win overwrite ordering forever"
        );
        assert!(
            !usable_snapshot_line(
                &parse(&line("1600000000000", Some("253402300800000"))),
                now_ms
            ),
            "an implausible localTimestamp is rejected even with a sane entityTimestamp"
        );
    }

    #[test]
    fn replacement_chain_collapses_in_a_single_call() {
        let h0 = "h0".to_string();
        let h1 = "h1".to_string();
        let h2 = "h2".to_string();
        let genesis = 0;
        let end_ts = 1_700_000_000_000;
        let candidates = vec![
            (h2.clone(), end_ts, vec![vec![h1.clone()]]),
            (h1.clone(), end_ts, vec![vec![h0.clone()]]),
        ];
        let mut processed: HashSet<String> = [h0].into_iter().collect();
        let (to_deploy, newly_marked) = decide_snapshot_pass(&candidates, &mut processed, genesis);
        assert!(
            to_deploy.is_empty(),
            "no link of a fully-replaced chain may be deployed, got {to_deploy:?}"
        );
        let mut sorted = newly_marked;
        sorted.sort();
        assert_eq!(sorted, vec![h1, h2], "both chain links must end up marked");
        assert!(processed.contains("h1") && processed.contains("h2"));
    }

    #[test]
    fn unreplaced_snapshot_deploys_and_genesis_cutoff_skips() {
        let candidates = vec![
            ("new".to_string(), 1_700_000_000_000, vec![]),
            ("ancient".to_string(), 5, vec![]),
        ];
        let mut processed = HashSet::new();
        let (to_deploy, marked) =
            decide_snapshot_pass(&candidates, &mut processed, 1_000_000_000_000);
        assert_eq!(to_deploy, vec!["new".to_string()]);
        assert!(marked.is_empty());
    }

    #[tokio::test]
    async fn gzip_labeled_snapshot_decodes_then_hashes_and_stores_decoded() {
        const PAYLOAD: &[u8] = b"### Decentraland json snapshot\n{\"entityId\":\"Qm1\"}\n";
        let cid = hash_bytes_v1(PAYLOAD);
        let wire = gzip(PAYLOAD);
        let (base, server) = spawn_content_server(wire, Some("gzip")).await;
        let (storage, tmp) = temp_storage("snap-gzip").await;
        let servers: HashSet<String> = [base].into_iter().collect();

        download_snapshot_file(&Client::new(), &storage, &cid, &servers, 1, 0)
            .await
            .expect("a gzip-labeled snapshot whose decoded bytes match the CID must be accepted");

        let stored = storage.retrieve(&cid).await.unwrap().unwrap();
        assert_eq!(
            &stored[..],
            PAYLOAD,
            "the stored bytes must be the decoded representation"
        );

        server.abort();
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn corrupt_snapshot_body_still_fails_hash_verification() {
        const PAYLOAD: &[u8] = b"### Decentraland json snapshot\n{\"entityId\":\"Qm1\"}\n";
        const OTHER: &[u8] = b"### Decentraland json snapshot\n{\"entityId\":\"Qm2\"}\n";
        let cid = hash_bytes_v1(PAYLOAD);
        let wire = gzip(OTHER);
        let (base, server) = spawn_content_server(wire, Some("gzip")).await;
        let (storage, tmp) = temp_storage("snap-corrupt").await;
        let servers: HashSet<String> = [base].into_iter().collect();

        let err = download_snapshot_file(&Client::new(), &storage, &cid, &servers, 1, 0)
            .await
            .expect_err("decoded bytes that mismatch the CID must still be rejected");
        assert!(err.to_string().contains("snapshot hash mismatch"), "{err}");
        assert!(!storage.exist(&cid).await.unwrap());

        server.abort();
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    fn read_all(raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        open_snapshot_reader(std::io::Cursor::new(raw.to_vec()))
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn a_stored_gzip_snapshot_decompresses_and_a_plain_one_passes_through() {
        const PAYLOAD: &[u8] = b"### Decentraland json snapshot\n{\"entityId\":\"Qm1\"}\n";
        assert_eq!(read_all(&gzip(PAYLOAD)), PAYLOAD);
        assert_eq!(read_all(PAYLOAD), PAYLOAD);
        assert_eq!(
            read_all(b"{"),
            b"{",
            "a file shorter than the gzip magic passes through"
        );
        assert!(read_all(b"").is_empty());
    }

    #[test]
    fn a_streamed_cid_matches_the_whole_buffer_hash_under_both_schemes() {
        for size in [0usize, 10, 262_144, 262_145, 700_000] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let v1 = hash_bytes_v1(&data);
            let v0 = hash_bytes(&data);
            for piece in [1usize << 10, 65_536, 1 << 20] {
                let mut h1 = CidHasher::for_cid(&v1).unwrap();
                let mut h0 = CidHasher::for_cid(&v0).unwrap();
                for part in data.chunks(piece) {
                    h1.update(part);
                    h0.update(part);
                }
                assert_eq!(h1.finish(), v1, "v1 size {size} piece {piece}");
                assert_eq!(h0.finish(), v0, "v0 size {size} piece {piece}");
            }
        }
        assert!(CidHasher::for_cid("../../etc/passwd").is_none());
    }

    #[test]
    fn the_capped_line_reader_skips_overlong_lines_and_keeps_an_unterminated_last_one() {
        let text = b"short\nxxxxxxxxxxxxxxxxxxxxxxxx\n\nlast";
        let mut reader = std::io::BufReader::with_capacity(4, &text[..]);
        let mut buf = Vec::new();
        let mut seen = Vec::new();
        loop {
            match read_line_capped(&mut reader, &mut buf, 8).unwrap() {
                LineRead::Eof => break,
                LineRead::Overlong => seen.push("<overlong>".to_string()),
                LineRead::Line => seen.push(String::from_utf8(buf.clone()).unwrap()),
            }
        }
        assert_eq!(seen, vec!["short\n", "<overlong>", "\n", "last"]);
    }

    /// 2,503 deployable lines in a gzip snapshot with a header, blank lines, junk, an unparseable
    /// entry, an implausible timestamp, a pre-genesis line and a filtered-out type reach the
    /// receiver as batches of at most [`SNAPSHOT_BATCH_RECORDS`], with the counts the old
    /// whole-file pass reported.
    #[test]
    fn snapshot_records_arrive_in_bounded_batches_with_the_old_counts() {
        let line = |i: usize, entity_type: &str, ts: i64| {
            format!(
                "{{\"entityId\":\"Qm{i}\",\"entityType\":\"{entity_type}\",\"pointers\":[\"{i},0\"],\"authChain\":[],\"entityTimestamp\":{ts}}}\n"
            )
        };
        let mut text = String::from("### Decentraland json snapshot\n\n");
        for i in 0..2_503 {
            text.push_str(&line(
                i,
                if i % 5 == 0 { "scene" } else { "profile" },
                1_600_000_000_000,
            ));
        }
        text.push_str("this is not json\n");
        text.push_str("{\"entityId\":1}\n");
        text.push_str(&line(9_001, "scene", 253_402_300_800_000));
        text.push_str(&line(9_002, "scene", 5));
        text.push_str(&line(9_003, "wearable", 1_600_000_000_000));
        let dir = std::env::temp_dir().join(format!("catalyst-snapshot-lines-{}", unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.gz");
        std::fs::write(&path, gzip(text.as_bytes())).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let stats = read_snapshot_records(
            path,
            SnapshotLineFilter {
                snapshot_hash: "test".into(),
                genesis_timestamp: 1_000,
                entity_type_filter: Some(
                    ["scene".to_string(), "profile".to_string()]
                        .into_iter()
                        .collect(),
                ),
                now_ms: 1_700_000_000_000,
                stats: ParseStats::default(),
            },
            tx,
        )
        .unwrap();

        let mut sizes = Vec::new();
        let mut scenes = 0;
        while let Ok(batch) = rx.try_recv() {
            sizes.push(batch.len());
            scenes += batch.iter().filter(|d| d.entity_type == "scene").count();
        }
        assert_eq!(sizes, vec![1000, 1000, 503]);
        assert_eq!(scenes, 501);
        assert_eq!(
            stats,
            ParseStats {
                total: 2_503 + 4,
                parse_errors: 3,
                skipped_by_filter: 1
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn any_advertisers_processed_group_suffices() {
        let mut processed: HashSet<String> =
            ["a".to_string(), "b".to_string()].into_iter().collect();
        let decision = decide_snapshot_deployment_from_processed_set(
            &mut processed,
            0,
            "snap",
            1_700_000_000_000,
            &[
                vec!["a".to_string(), "x".to_string()],
                vec!["a".to_string(), "b".to_string()],
            ],
        );
        assert_eq!(decision, SnapshotDecision::MarkProcessed);
        assert!(
            processed.contains("snap"),
            "mark must mutate the caller's set"
        );

        let mut processed = HashSet::new();
        let decision = decide_snapshot_deployment_from_processed_set(
            &mut processed,
            0,
            "snap2",
            1_700_000_000_000,
            &[vec![]],
        );
        assert_eq!(decision, SnapshotDecision::Deploy);
    }
}
