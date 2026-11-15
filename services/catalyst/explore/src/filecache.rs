//! A bounded, keyed file cache beside the ONE hash-addressed blob store,
//! for objects addressed by someone else's key rather than by their own
//! hash: the imposter zips (`{level}/{x},{y}.{crc}.zip`, `explore::imposters`)
//! and the asset-bundle CDN objects (`v49/{cid}_mac`, `explore::api::ab_cdn`).
//! Every object is a file under `<root>/store/<rel>`, landed by rename from
//! `<root>/tmp`; a byte budget is kept by evicting the least recently
//! served files (mtime, refreshed on serve) once an eviction pass finds the
//! total over budget, files younger than an hour excepted; the counters
//! behind `usage_snapshot` follow landings and passes without filesystem
//! work. [`Supply`] adds what a read-through needs in front of it: a
//! memory LRU of served bytes, a per-key single flight so a burst of
//! requests for one missing object fetches it once, and a short negative
//! cache of upstream misses.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;

const EVICT_MIN_AGE: Duration = Duration::from_secs(3600);
const SWEEP_MAX_AGE: Duration = Duration::from_secs(86400);
const EVICT_MIN_INTERVAL: Duration = Duration::from_secs(60);
const MAX_DEPTH: usize = 8;

pub struct FileCache {
    root: PathBuf,
    max_bytes: u64,
    bytes: AtomicU64,
    entries: AtomicU64,
    touched: Mutex<HashSet<String>>,
    evicting: AtomicBool,
    last_evict: Mutex<Option<Instant>>,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Usage {
    pub bytes: u64,
    pub entries: u64,
}

/// A relative key is a `/`-separated path of plain segments: no empty, `.`
/// or `..` segment, no backslash, no NUL, at most `MAX_DEPTH` deep.
pub fn valid_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\\')
        && !rel.contains('\0')
        && rel.split('/').count() <= MAX_DEPTH
        && rel
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

impl FileCache {
    pub fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self {
            root,
            max_bytes,
            bytes: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            touched: Mutex::new(HashSet::new()),
            evicting: AtomicBool::new(false),
            last_evict: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Counters maintained by eviction passes and landings; no filesystem work.
    pub fn usage_snapshot(&self) -> Usage {
        Usage {
            bytes: self.bytes.load(Ordering::Relaxed),
            entries: self.entries.load(Ordering::Relaxed),
        }
    }

    pub fn store_dir(&self) -> PathBuf {
        self.root.join("store")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn evicted_dir(&self) -> PathBuf {
        self.root.join("evicted")
    }

    pub fn quarantined_dir(&self) -> PathBuf {
        self.root.join("quarantined")
    }

    /// The file of `rel`; `None` for a key that is not a valid relative path.
    pub fn path(&self, rel: &str) -> Option<PathBuf> {
        valid_rel(rel).then(|| self.store_dir().join(rel))
    }

    pub fn init(&self) -> Result<()> {
        for dir in [self.store_dir(), self.tmp_dir(), self.evicted_dir()] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn note_access(&self, rel: &str) {
        self.touched.lock().unwrap().insert(rel.to_string());
    }

    pub fn flush_touches(&self) {
        let keys: Vec<String> = std::mem::take(&mut *self.touched.lock().unwrap())
            .into_iter()
            .collect();
        for rel in keys {
            if let Some(path) = self.path(&rel) {
                touch(&path);
            }
        }
    }

    /// Moves `store/<rel>` to `quarantined/<rel>`; `Ok(false)` when absent.
    pub fn quarantine_entry(&self, rel: &str) -> std::io::Result<bool> {
        let Some(src) = self.path(rel) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid key",
            ));
        };
        let target = self.quarantined_dir().join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::rename(&src, target) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn read_hit(&self, rel: &str) -> Option<Vec<u8>> {
        let path = self.path(rel)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                self.note_access(rel);
                Some(bytes)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cache entry unreadable, evicting");
                let _ = self.evict_file(&path);
                None
            }
        }
    }

    /// Writes `bytes` to a temporary file (fsynced) and renames it into place.
    pub async fn land(self: &Arc<Self>, rel: &str, bytes: &[u8]) -> Result<()> {
        let target = self
            .path(rel)
            .with_context(|| format!("invalid cache key {rel:?}"))?;
        let tmp = self.tmp_dir().join(uuid::Uuid::new_v4().to_string());
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .await
            .with_context(|| format!("syncing {}", tmp.display()))?;
        drop(f);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        tokio::fs::rename(&tmp, &target)
            .await
            .with_context(|| format!("landing {}", target.display()))?;
        self.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.entries.fetch_add(1, Ordering::Relaxed);
        self.spawn_evict_if_over_budget();
        Ok(())
    }

    pub fn spawn_evict_if_over_budget(self: &Arc<Self>) {
        if self.bytes.load(Ordering::Relaxed) <= self.max_bytes {
            return;
        }
        if self
            .last_evict
            .lock()
            .unwrap()
            .is_some_and(|at| at.elapsed() < EVICT_MIN_INTERVAL)
        {
            return;
        }
        if self
            .evicting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let _ = store.evict_pass();
            store.evicting.store(false, Ordering::Release);
        });
    }

    pub fn evict_file(&self, path: &Path) -> std::io::Result<()> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let target = self
            .evicted_dir()
            .join(format!("{}-{}", uuid::Uuid::new_v4(), name));
        std::fs::rename(path, &target)?;
        let _ = std::fs::remove_file(&target);
        Ok(())
    }

    /// A full scan of the store (for tests and the status route's cold start).
    pub fn usage(&self) -> Usage {
        let mut usage = Usage::default();
        for (_, _, len) in self.scan() {
            usage.bytes += len;
            usage.entries += 1;
        }
        usage
    }

    fn scan(&self) -> Vec<(PathBuf, SystemTime, u64)> {
        let mut entries = Vec::new();
        let mut stack = vec![(self.store_dir(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let Ok(md) = entry.metadata() else { continue };
                if md.is_dir() {
                    if depth < MAX_DEPTH {
                        stack.push((entry.path(), depth + 1));
                    }
                } else if md.is_file() {
                    entries.push((entry.path(), md.modified().unwrap_or(UNIX_EPOCH), md.len()));
                }
            }
        }
        entries
    }

    /// Oldest-first eviction down to the budget, entries younger than an
    /// hour protected; answers the total bytes left.
    pub fn evict_pass(&self) -> Result<u64> {
        self.flush_touches();
        let mut entries = self.scan();
        let mut total: u64 = entries.iter().map(|e| e.2).sum();
        let mut remaining = entries.len() as u64;
        if total > self.max_bytes {
            entries.sort_by_key(|e| e.1);
            let now = SystemTime::now();
            for (path, mtime, len) in entries {
                if total <= self.max_bytes {
                    break;
                }
                if now.duration_since(mtime).unwrap_or_default() < EVICT_MIN_AGE {
                    continue;
                }
                if self.evict_file(&path).is_ok() {
                    tracing::info!(path = %path.display(), len, "evicted");
                    total -= len;
                    remaining -= 1;
                }
            }
        }
        self.drain_evicted();
        self.bytes.store(total, Ordering::Relaxed);
        self.entries.store(remaining, Ordering::Relaxed);
        *self.last_evict.lock().unwrap() = Some(Instant::now());
        Ok(total)
    }

    fn drain_evicted(&self) {
        let Ok(rd) = std::fs::read_dir(self.evicted_dir()) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Drops leftovers of interrupted landings older than a day.
    pub fn sweep_transient(&self) {
        self.drain_evicted();
        let cutoff = SystemTime::now() - SWEEP_MAX_AGE;
        let Ok(rd) = std::fs::read_dir(self.tmp_dir()) else {
            return;
        };
        for entry in rd.flatten() {
            let Ok(md) = entry.metadata() else { continue };
            if md.modified().map(|m| m > cutoff).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            if md.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// The periodic pass: sweep, then evict if over budget (blocking work).
    pub fn maintenance_pass(&self) {
        self.sweep_transient();
        let _ = self.evict_pass();
    }
}

fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()));
    }
}

const MISS_TTL: Duration = Duration::from_secs(60);
const MISS_MAX_ENTRIES: usize = 65536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Store,
    Upstream,
}

pub enum Served {
    Hit(Bytes, Source),
    Miss,
}

pub struct ByteLru<K> {
    budget: usize,
    used: usize,
    seq: u64,
    map: HashMap<K, (Bytes, u64)>,
    order: BTreeMap<u64, K>,
}

impl<K: Clone + Hash + Eq> ByteLru<K> {
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            seq: 0,
            map: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    pub fn get(&mut self, key: &K) -> Option<Bytes> {
        let (bytes, seq) = self.map.get_mut(key)?;
        self.order.remove(seq);
        self.seq += 1;
        *seq = self.seq;
        self.order.insert(self.seq, key.clone());
        Some(bytes.clone())
    }

    pub fn insert(&mut self, key: K, bytes: Bytes) {
        if bytes.len() > self.budget {
            return;
        }
        self.remove(&key);
        self.seq += 1;
        self.used += bytes.len();
        self.map.insert(key.clone(), (bytes, self.seq));
        self.order.insert(self.seq, key);
        while self.used > self.budget {
            let Some((_, victim)) = self.order.pop_first() else {
                break;
            };
            if let Some((old, _)) = self.map.remove(&victim) {
                self.used -= old.len();
            }
        }
    }

    pub fn remove(&mut self, key: &K) {
        if let Some((old, seq)) = self.map.remove(key) {
            self.order.remove(&seq);
            self.used -= old.len();
        }
    }

    pub fn used(&self) -> usize {
        self.used
    }
}

/// Memory LRU + single flight + negative cache over a [`FileCache`]; `K` is
/// the caller's key, `rel` its file under the cache.
pub struct Supply<K> {
    store: Arc<FileCache>,
    locks: crate::keyed_locks::KeyedLocks<K>,
    bytes: Mutex<ByteLru<K>>,
    misses: Mutex<HashMap<K, Instant>>,
}

impl<K: Clone + Hash + Eq> Supply<K> {
    pub fn new(store: Arc<FileCache>, mem_budget: usize) -> Self {
        Self {
            store,
            locks: crate::keyed_locks::KeyedLocks::default(),
            bytes: Mutex::new(ByteLru::new(mem_budget)),
            misses: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &Arc<FileCache> {
        &self.store
    }

    /// Memory, then the file, then `fetch` once per key at a time; a fetched
    /// body passes `verify` before it lands, an upstream miss is remembered
    /// for a minute, a failed fetch is not.
    pub async fn get<F, Fut, V>(&self, key: &K, rel: &str, fetch: F, verify: V) -> Result<Served>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<Vec<u8>>>>,
        V: FnOnce(&[u8]) -> Result<()>,
    {
        if let Some(bytes) = self.cached(key, rel) {
            return Ok(Served::Hit(bytes, Source::Store));
        }
        if self.is_known_miss(key) {
            return Ok(Served::Miss);
        }
        let lock = self.locks.claim(key);
        let result = {
            let _guard = lock.lock().await;
            if let Some(bytes) = self.cached(key, rel) {
                Ok(Served::Hit(bytes, Source::Store))
            } else if let Some(bytes) = self.read_store(key, rel).await {
                Ok(Served::Hit(bytes, Source::Store))
            } else {
                match fetch().await {
                    Ok(Some(bytes)) => {
                        verify(&bytes)?;
                        let bytes = Bytes::from(bytes);
                        self.store.land(rel, &bytes).await?;
                        self.remember(key, bytes.clone());
                        Ok(Served::Hit(bytes, Source::Upstream))
                    }
                    Ok(None) => {
                        self.remember_miss(key);
                        Ok(Served::Miss)
                    }
                    Err(e) => Err(e),
                }
            }
        };
        result
    }

    /// The file alone, bypassing the memory caches.
    pub async fn get_stored(&self, rel: &str) -> Served {
        match self.store.read_hit(rel).await {
            Some(bytes) => Served::Hit(Bytes::from(bytes), Source::Store),
            None => Served::Miss,
        }
    }

    /// Memory or file, no fetch.
    pub async fn get_local(&self, key: &K, rel: &str) -> Option<Bytes> {
        match self.cached(key, rel) {
            Some(b) => Some(b),
            None => self.read_store(key, rel).await,
        }
    }

    fn cached(&self, key: &K, rel: &str) -> Option<Bytes> {
        let bytes = self.bytes.lock().unwrap().get(key)?;
        self.store.note_access(rel);
        Some(bytes)
    }

    async fn read_store(&self, key: &K, rel: &str) -> Option<Bytes> {
        let bytes = Bytes::from(self.store.read_hit(rel).await?);
        self.remember(key, bytes.clone());
        Some(bytes)
    }

    fn remember(&self, key: &K, bytes: Bytes) {
        self.misses.lock().unwrap().remove(key);
        self.bytes.lock().unwrap().insert(key.clone(), bytes);
    }

    fn remember_miss(&self, key: &K) {
        let mut misses = self.misses.lock().unwrap();
        let now = Instant::now();
        if misses.len() >= MISS_MAX_ENTRIES {
            misses.retain(|_, until| *until > now);
            if misses.len() >= MISS_MAX_ENTRIES {
                misses.clear();
            }
        }
        misses.insert(key.clone(), now + MISS_TTL);
    }

    pub fn is_known_miss(&self, key: &K) -> bool {
        let mut misses = self.misses.lock().unwrap();
        match misses.get(key) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                misses.remove(key);
                false
            }
            None => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn locks_len(&self) -> usize {
        self.locks.len()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    pub(crate) struct TempDir(pub PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn tempdir(label: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!(
            "catalyst-filecache-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn set_mtime(path: &Path, ago: Duration) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now() - ago))
            .unwrap();
    }

    fn put(store: &FileCache, rel: &str, len: usize, ago: Duration) -> PathBuf {
        let path = store.path(rel).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; len]).unwrap();
        set_mtime(&path, ago);
        path
    }

    fn cache(dir: &TempDir, budget: u64) -> Arc<FileCache> {
        let c = Arc::new(FileCache::new(dir.0.join("root"), budget));
        c.init().unwrap();
        c
    }

    #[test]
    fn keys_are_plain_relative_paths() {
        for ok in [
            "0/0,0.1.zip",
            "v49/bafy/scene.json",
            "manifest/x_mac.json",
            "LOD/0/x_0_mac",
        ] {
            assert!(valid_rel(ok), "{ok}");
        }
        for bad in [
            "",
            "/abs",
            "a//b",
            "../x",
            "a/../b",
            ".",
            "a/./b",
            "a\\b",
            "a\0b",
            "1/2/3/4/5/6/7/8/9",
        ] {
            assert!(!valid_rel(bad), "{bad:?}");
        }
        let dir = tempdir("keys");
        let c = cache(&dir, u64::MAX);
        assert!(c.path("../etc/passwd").is_none());
        assert_eq!(c.path("a/b").unwrap(), dir.0.join("root/store/a/b"));
    }

    #[test]
    fn evicts_oldest_first_until_under_budget() {
        let dir = tempdir("evict");
        let store = cache(&dir, 250);
        let oldest = put(&store, "0/a", 100, Duration::from_secs(4 * 3600));
        let middle = put(&store, "0/b", 100, Duration::from_secs(3 * 3600));
        let newest = put(&store, "1/c", 100, Duration::from_secs(2 * 3600));
        let total = store.evict_pass().unwrap();
        assert!(total <= 250);
        assert!(!oldest.exists());
        assert!(middle.exists());
        assert!(newest.exists());
        assert_eq!(std::fs::read_dir(store.evicted_dir()).unwrap().count(), 0);
    }

    #[test]
    fn young_entries_are_protected() {
        let dir = tempdir("young");
        let store = cache(&dir, 50);
        let young = put(&store, "0/y", 100, Duration::ZERO);
        let old = put(&store, "0/o", 100, Duration::from_secs(2 * 3600));
        store.evict_pass().unwrap();
        assert!(young.exists());
        assert!(!old.exists());
    }

    #[tokio::test]
    async fn touch_on_serve_reorders_eviction() {
        let dir = tempdir("touch");
        let store = cache(&dir, 150);
        let path_a = put(&store, "0/a", 100, Duration::from_secs(4 * 3600));
        let path_b = put(&store, "0/b", 100, Duration::from_secs(2 * 3600));
        assert!(store.read_hit("0/a").await.is_some());
        store.evict_pass().unwrap();
        assert!(path_a.exists());
        assert!(!path_b.exists());
    }

    #[tokio::test]
    async fn usage_snapshot_follows_scans_and_landings() {
        let dir = tempdir("usage");
        let store = cache(&dir, u64::MAX);
        assert_eq!(store.usage_snapshot().entries, 0);
        put(&store, "0/a", 10, Duration::ZERO);
        put(&store, "3/deep/b", 20, Duration::ZERO);
        store.evict_pass().unwrap();
        let usage = store.usage_snapshot();
        assert_eq!((usage.entries, usage.bytes), (2, 30));
        store.land("1/c", &[0u8; 20]).await.unwrap();
        let usage = store.usage_snapshot();
        assert_eq!((usage.entries, usage.bytes), (3, 50));
        assert_eq!(store.usage().bytes, 50);
        assert!(store.land("../x", b"no").await.is_err());
        assert!(store.read_hit("0/missing").await.is_none());
    }

    #[test]
    fn sweep_removes_stale_transients_and_quarantine_moves_entries() {
        let dir = tempdir("sweep");
        let store = cache(&dir, u64::MAX);
        let stale = store.tmp_dir().join("stale");
        std::fs::write(&stale, b"x").unwrap();
        set_mtime(&stale, Duration::from_secs(2 * 86400));
        let fresh = store.tmp_dir().join("fresh");
        std::fs::write(&fresh, b"x").unwrap();
        store.sweep_transient();
        assert!(!stale.exists());
        assert!(fresh.exists());
        let present = put(&store, "0/p", 8, Duration::ZERO);
        assert!(store.quarantine_entry("0/p").unwrap());
        assert!(!present.exists());
        assert!(store.quarantined_dir().join("0/p").exists());
        assert!(!store.quarantine_entry("0/p").unwrap());
        assert!(store.quarantine_entry("../p").is_err());
    }

    const MEM: usize = 64 << 20;

    fn no_verify(_: &[u8]) -> Result<()> {
        Ok(())
    }

    #[tokio::test]
    async fn verification_failure_releases_key() {
        let dir = tempdir("verify-release");
        let supply = Supply::<String>::new(cache(&dir, u64::MAX), MEM);
        let key = "k".to_string();
        let result = supply
            .get(
                &key,
                "0/k",
                || async { Ok(Some(vec![1])) },
                |_| anyhow::bail!("rejected body"),
            )
            .await;
        assert!(result.err().unwrap().to_string().contains("rejected body"));
        assert_eq!(supply.locks_len(), 0);
    }

    #[tokio::test]
    async fn landing_failure_releases_key() {
        let dir = tempdir("land-release");
        let store = cache(&dir, u64::MAX);
        // A file where the target's parent directory belongs makes landing fail.
        std::fs::write(store.path("blocked").unwrap(), b"file").unwrap();
        let supply = Supply::<String>::new(store, MEM);
        let result = supply
            .get(
                &"k".to_string(),
                "blocked/k",
                || async { Ok(Some(vec![1])) },
                no_verify,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(supply.locks_len(), 0);
    }

    #[tokio::test]
    async fn cancellation_releases_key() {
        let dir = tempdir("cancel-release");
        let supply = Supply::<String>::new(cache(&dir, u64::MAX), MEM);
        let key = "k".to_string();
        let entered = tokio::sync::Notify::new();
        let mut get = Box::pin(supply.get(
            &key,
            "0/k",
            || async {
                entered.notify_one();
                std::future::pending().await
            },
            no_verify,
        ));
        tokio::select! {
            _ = &mut get => panic!("fetch finished"),
            _ = entered.notified() => {}
        }
        // Keep the same mutex observable across cancellation.
        let lock = supply.locks.claim(&key);
        drop(get);
        // Cancellation releases the mutex even when registry cleanup is broken.
        let mut acquired = Box::pin(lock.lock());
        assert!(futures::poll!(&mut acquired).is_ready());
        drop(acquired);
        drop(lock);
        assert_eq!(supply.locks_len(), 0);
        assert!(matches!(
            supply
                .get(&key, "0/k", || async { Ok(None) }, no_verify)
                .await
                .unwrap(),
            Served::Miss
        ));
    }

    #[tokio::test]
    async fn coalesces_concurrent_fetches() {
        let dir = tempdir("coalesce");
        let store = cache(&dir, u64::MAX);
        let supply = Supply::<String>::new(store.clone(), MEM);
        let count = AtomicUsize::new(0);
        let (entered, release) = (tokio::sync::Notify::new(), tokio::sync::Notify::new());
        let fetch = {
            let (count, entered, release) = (&count, &entered, &release);
            move || async move {
                count.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                Ok(Some(b"payload".to_vec()))
            }
        };
        let mut gets = std::pin::pin!(futures::future::join_all((0..8).map(|_| async {
            match supply
                .get(&"k".to_string(), "0/k", fetch, no_verify)
                .await
                .unwrap()
            {
                Served::Hit(body, _) => body,
                Served::Miss => panic!("miss"),
            }
        })));
        // One poll queues all eight gets; the first then fetches until released.
        assert!(futures::poll!(&mut gets).is_pending());
        tokio::select! {
            _ = &mut gets => panic!("the gets finished while the fetch was held"),
            _ = entered.notified() => {}
        }
        release.notify_one();
        for body in gets.await {
            assert_eq!(body, Bytes::from_static(b"payload"));
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(supply.locks_len(), 0);
        assert!(store.path("0/k").unwrap().exists());
        assert_eq!(store.usage_snapshot().entries, 1);
    }

    #[tokio::test]
    async fn miss_is_negatively_cached_and_a_bad_body_is_an_error() {
        let dir = tempdir("miss");
        let store = cache(&dir, u64::MAX);
        let supply = Supply::<String>::new(store.clone(), MEM);
        let key = "k".to_string();
        assert!(matches!(
            supply
                .get(&key, "0/k", || async { Ok(None) }, no_verify)
                .await
                .unwrap(),
            Served::Miss
        ));
        assert_eq!(store.usage().entries, 0);
        let fetched = Arc::new(AtomicUsize::new(0));
        let counter = fetched.clone();
        let again = supply
            .get(
                &key,
                "0/k",
                move || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(b"late".to_vec()))
                },
                no_verify,
            )
            .await
            .unwrap();
        assert!(matches!(again, Served::Miss));
        assert_eq!(fetched.load(Ordering::SeqCst), 0);

        let bad = "bad".to_string();
        let result = supply
            .get(
                &bad,
                "0/bad",
                || async { Ok(Some(b"garbage".to_vec())) },
                |_| anyhow::bail!("not a zip"),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(store.usage().entries, 0);
        assert!(!supply.is_known_miss(&bad));
    }

    #[tokio::test]
    async fn second_get_serves_from_store_and_memory_hit_skips_the_filesystem() {
        let dir = tempdir("second");
        let store = cache(&dir, u64::MAX);
        let supply = Supply::<String>::new(store.clone(), MEM);
        let key = "k".to_string();
        let first = supply
            .get(
                &key,
                "0/k",
                || async { Ok(Some(b"body".to_vec())) },
                no_verify,
            )
            .await
            .unwrap();
        assert!(matches!(first, Served::Hit(_, Source::Upstream)));
        let fetched = Arc::new(AtomicUsize::new(0));
        let counter = fetched.clone();
        match supply
            .get(
                &key,
                "0/k",
                move || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                },
                no_verify,
            )
            .await
            .unwrap()
        {
            Served::Hit(body, Source::Store) => assert_eq!(body, Bytes::from_static(b"body")),
            _ => panic!("expected store hit"),
        }
        assert_eq!(fetched.load(Ordering::SeqCst), 0);
        std::fs::remove_file(store.path("0/k").unwrap()).unwrap();
        assert!(matches!(
            supply
                .get(&key, "0/k", || async { Ok(None) }, no_verify)
                .await
                .unwrap(),
            Served::Hit(_, Source::Store)
        ));
        assert!(matches!(supply.get_stored("0/k").await, Served::Miss));
    }

    #[test]
    fn byte_lru_evicts_least_recently_used_within_budget() {
        let mut lru = ByteLru::<u8>::new(10);
        lru.insert(1, Bytes::from_static(&[0; 4]));
        lru.insert(2, Bytes::from_static(&[0; 4]));
        assert!(lru.get(&1).is_some());
        lru.insert(3, Bytes::from_static(&[0; 4]));
        assert!(lru.get(&2).is_none());
        assert!(lru.get(&1).is_some());
        assert!(lru.get(&3).is_some());
        assert_eq!(lru.used(), 8);
        lru.insert(1, Bytes::from_static(&[0; 11]));
        assert!(lru.get(&1).is_some());
        assert_eq!(lru.used(), 8);
    }
}
