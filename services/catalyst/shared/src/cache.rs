//! A bounded TTL memo with single-flight fetches: concurrent misses on one key
//! elect a leader and park the rest on a [`Notify`]; the leader always wakes
//! them, including when its task is cancelled mid-fetch (`LeaderGuard`), so a
//! dropped caller can never strand the others. Waiters park with a short
//! re-check bound as well, so a wakeup that lands before a waiter registers
//! costs one extra loop rather than the whole request.
//!
//! [`TtlCache`] is the simpler keyed memo behind a std mutex (no
//! single-flight), recomputed at most once per TTL.

use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Notify;
use tokio::time::Instant;

struct Entry<V> {
    at: Instant,
    value: V,
}

struct Slot<V> {
    cached: Option<Entry<V>>,
    notify: Option<Arc<Notify>>,
}

impl<V> Slot<V> {
    fn empty() -> Self {
        Self {
            cached: None,
            notify: None,
        }
    }
}

enum Action {
    Wait(Arc<Notify>),
    Lead(Arc<Notify>),
}

const WAIT_RECHECK: Duration = Duration::from_millis(250);

pub struct TtlMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    map: DashMap<K, Slot<V>>,
    ttl: Duration,
    max_entries: Option<usize>,
    #[cfg(test)]
    eviction_passes: std::sync::atomic::AtomicUsize,
}

struct LeaderGuard<'a, K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    cache: &'a TtlMap<K, V>,
    key: K,
    notify: Arc<Notify>,
    finished: bool,
}

impl<K, V> Drop for LeaderGuard<'_, K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(mut slot) = self.cache.map.get_mut(&self.key) {
            slot.notify = None;
        }
        self.notify.notify_waiters();
    }
}

impl<K, V> TtlMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new(ttl: Duration) -> Self {
        Self {
            map: DashMap::new(),
            ttl,
            max_entries: None,
            #[cfg(test)]
            eviction_passes: Default::default(),
        }
    }

    pub fn bounded(ttl: Duration, max_entries: usize) -> Self {
        Self {
            max_entries: Some(max_entries),
            ..Self::new(ttl)
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&self) {
        self.map.clear();
    }

    pub fn get_fresh(&self, key: &K) -> Option<V> {
        let slot = self.map.get(key)?;
        let entry = slot.cached.as_ref()?;
        (entry.at.elapsed() < self.ttl).then(|| entry.value.clone())
    }

    /// The last value stored under `key`, however old: a failed refresh
    /// keeps it, so a caller can serve it stale.
    pub fn get_stale(&self, key: &K) -> Option<V> {
        let slot = self.map.get(key)?;
        slot.cached.as_ref().map(|entry| entry.value.clone())
    }

    pub fn insert(&self, key: K, value: V) {
        if !self.map.contains_key(&key) {
            self.make_room(1);
        }
        let mut slot = self.map.entry(key).or_insert_with(Slot::empty);
        slot.cached = Some(Entry {
            at: Instant::now(),
            value,
        });
    }

    /// Makes room for `incoming` new keys in a bounded map: once full, expired
    /// entries go, then the oldest down to ~90% of `max_entries` in one pass,
    /// so a full map pays one scan per tenth of its size. In-flight fetches
    /// stay, and concurrent inserts are not serialized: the bound is soft by
    /// at most the in-flight and racing keys.
    fn make_room(&self, incoming: usize) {
        let Some(max) = self.max_entries else { return };
        if self.map.len() + incoming <= max {
            return;
        }
        #[cfg(test)]
        self.eviction_passes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ttl = self.ttl;
        self.map.retain(|_, slot| {
            slot.notify.is_some() || slot.cached.as_ref().is_some_and(|e| e.at.elapsed() < ttl)
        });
        let low_water = max.saturating_sub(max / 10 + 1);
        let len = self.map.len() + incoming;
        if len <= max {
            return;
        }
        let excess = len.saturating_sub(low_water.max(incoming)).max(1);
        let mut ages: Vec<(Option<Instant>, K)> = self
            .map
            .iter()
            .filter(|slot| slot.notify.is_none())
            .map(|slot| (slot.cached.as_ref().map(|e| e.at), slot.key().clone()))
            .collect();
        if ages.len() > excess {
            ages.select_nth_unstable_by_key(excess - 1, |(at, _)| *at);
            ages.truncate(excess);
        }
        for (_, key) in ages {
            self.map.remove(&key);
        }
    }

    /// The cached value, expired or not, while it is younger than `max_age`:
    /// a stale-on-error read for a caller that may answer an old value when
    /// its fetch fails. A failed fetch leaves the expired entry in place.
    pub fn get_within(&self, key: &K, max_age: Duration) -> Option<V> {
        let slot = self.map.get(key)?;
        let entry = slot.cached.as_ref()?;
        (entry.at.elapsed() < max_age).then(|| entry.value.clone())
    }

    /// Ages `key`'s entry by `by`, as if it had been stored that much earlier.
    #[cfg(any(test, feature = "test-support"))]
    pub fn backdate(&self, key: &K, by: Duration) {
        if let Some(mut slot) = self.map.get_mut(key)
            && let Some(entry) = slot.cached.as_mut()
        {
            entry.at = entry
                .at
                .checked_sub(by)
                .expect("a backdate within the clock");
        }
    }

    pub async fn get_or_fetch<F, Fut, E>(&self, key: K, fetch: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let mut fetch = Some(fetch);
        loop {
            let action = {
                let mut entry = self.map.entry(key.clone()).or_insert_with(Slot::empty);
                if let Some(cached) = entry.cached.as_ref()
                    && cached.at.elapsed() < self.ttl
                {
                    return Ok(cached.value.clone());
                }
                if let Some(notify) = entry.notify.as_ref() {
                    Action::Wait(notify.clone())
                } else {
                    let notify = Arc::new(Notify::new());
                    entry.notify = Some(notify.clone());
                    Action::Lead(notify)
                }
            };

            match action {
                Action::Wait(notify) => {
                    let _ = tokio::time::timeout(WAIT_RECHECK, notify.notified()).await;
                    continue;
                }
                Action::Lead(notify) => {
                    let mut guard = LeaderGuard {
                        cache: self,
                        key: key.clone(),
                        notify: notify.clone(),
                        finished: false,
                    };

                    // This leader's own slot is in flight, so it survives.
                    self.make_room(0);

                    let fetch = fetch.take().expect("a leader fetches once");
                    let result = fetch().await;
                    match &result {
                        Ok(value) => {
                            let entry = Entry {
                                at: Instant::now(),
                                value: value.clone(),
                            };
                            match self.map.get_mut(&key) {
                                Some(mut slot) => {
                                    slot.cached = Some(entry);
                                    slot.notify = None;
                                }
                                None => {
                                    self.map.insert(
                                        key.clone(),
                                        Slot {
                                            cached: Some(entry),
                                            notify: None,
                                        },
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            if let Some(mut slot) = self.map.get_mut(&key) {
                                slot.notify = None;
                            }
                        }
                    }
                    guard.finished = true;
                    notify.notify_waiters();
                    return result;
                }
            }
        }
    }
}

/// A keyed TTL memo behind a mutex: the category counts, the parcel aliases and
/// the moderator set are recomputed at most once per TTL.
pub struct Cached<T> {
    pub value: T,
    pub expires_at: std::time::Instant,
}

pub type TtlCache<T> = std::sync::Mutex<std::collections::HashMap<String, Cached<T>>>;

pub fn cache_get<T: Clone>(cache: &TtlCache<T>, key: &str) -> Option<T> {
    let cache = cache.lock().unwrap();
    cache
        .get(key)
        .filter(|c| c.expires_at > std::time::Instant::now())
        .map(|c| c.value.clone())
}

pub fn cache_put<T>(cache: &TtlCache<T>, key: String, value: T, ttl: Duration) {
    let mut cache = cache.lock().unwrap();
    let now = std::time::Instant::now();
    cache.retain(|_, c| c.expires_at > now);
    cache.insert(
        key,
        Cached {
            value,
            expires_at: now + ttl,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn fresh_hits_skip_the_fetch_and_clear_forces_one() {
        let map: TtlMap<String, u32> = TtlMap::bounded(Duration::from_secs(5), 10);
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(7)
        };
        assert_eq!(map.get_or_fetch("k".into(), fetch).await, Ok(7));
        assert_eq!(map.get_or_fetch("k".into(), fetch).await, Ok(7));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        map.clear();
        assert_eq!(map.get_or_fetch("k".into(), fetch).await, Ok(7));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_stale_value() {
        let map: TtlMap<String, u32> = TtlMap::new(Duration::ZERO);
        assert_eq!(map.get_stale(&"k".into()), None);
        map.insert("k".into(), 3);
        assert_eq!(
            map.get_fresh(&"k".into()),
            None,
            "a zero TTL is never fresh"
        );
        let failed = map
            .get_or_fetch("k".into(), || async { Err::<u32, &str>("boom") })
            .await;
        assert_eq!(failed, Err("boom"));
        assert_eq!(map.get_stale(&"k".into()), Some(3));
    }

    #[tokio::test]
    async fn a_failed_fetch_is_not_cached() {
        let map: TtlMap<String, u32> = TtlMap::new(Duration::from_secs(5));
        let failed = map
            .get_or_fetch("k".into(), || async { Err::<u32, &str>("boom") })
            .await;
        assert_eq!(failed, Err("boom"));
        assert_eq!(
            map.get_or_fetch("k".into(), || async { Ok::<_, &str>(1) })
                .await,
            Ok(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_the_ttl() {
        let map: TtlMap<&'static str, u32> = TtlMap::new(Duration::from_millis(20));
        map.insert("k", 1);
        tokio::time::advance(Duration::from_millis(19)).await;
        assert_eq!(map.get_fresh(&"k"), Some(1));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(map.get_fresh(&"k"), None);
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_fetch() {
        let map: TtlMap<&'static str, u32> = TtlMap::new(Duration::from_secs(5));
        let calls = AtomicUsize::new(0);
        let release = Notify::new();
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            release.notified().await;
            Ok::<_, ()>(3)
        };
        let mut misses = std::pin::pin!(futures::future::join_all(
            (0..8).map(|_| map.get_or_fetch("k", fetch))
        ));
        assert!(
            futures::poll!(&mut misses).is_pending(),
            "the leader's fetch holds every miss"
        );
        release.notify_one();
        assert_eq!(misses.await, vec![Ok(3); 8]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn insert_on_a_full_bounded_map_evicts_the_oldest() {
        let map: TtlMap<u32, u32> = TtlMap::bounded(Duration::from_secs(60), 3);
        for k in 0..3 {
            map.insert(k, k);
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        map.insert(1, 10);
        assert_eq!(map.len(), 3, "replacing a key needs no room");
        map.insert(3, 3);
        assert!(map.len() <= 3, "{}", map.len());
        assert_eq!(map.get_fresh(&0), None, "the oldest entry is evicted");
        assert_eq!(map.get_fresh(&1), Some(10));
        assert_eq!(map.get_fresh(&3), Some(3));
    }

    #[tokio::test(start_paused = true)]
    async fn insert_on_a_full_bounded_map_drops_expired_entries_first() {
        let map: TtlMap<u32, u32> = TtlMap::bounded(Duration::from_secs(1), 3);
        map.insert(0, 0);
        map.insert(1, 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        map.insert(2, 2);
        map.insert(3, 3);
        assert_eq!(map.len(), 2, "both expired entries are swept");
        assert_eq!(map.get_fresh(&2), Some(2));
        assert_eq!(map.get_fresh(&3), Some(3));
    }

    #[tokio::test]
    async fn an_unbounded_map_keeps_every_insert() {
        let map: TtlMap<u32, u32> = TtlMap::new(Duration::from_secs(60));
        for k in 0..100 {
            map.insert(k, k);
        }
        assert_eq!(map.len(), 100);
    }

    #[tokio::test(start_paused = true)]
    async fn a_map_of_one_keeps_only_the_newest_entry() {
        let map: TtlMap<u32, u32> = TtlMap::bounded(Duration::from_secs(60), 1);
        for k in 0..5 {
            map.insert(k, k);
            tokio::time::advance(Duration::from_millis(1)).await;
            assert_eq!(map.len(), 1);
            assert_eq!(map.get_fresh(&k), Some(k));
        }
        assert_eq!(
            map.get_or_fetch(9, || async { Ok::<_, ()>(9) }).await,
            Ok(9)
        );
        assert_eq!(map.len(), 1);
        assert_eq!(map.get_fresh(&9), Some(9));
    }

    #[tokio::test]
    async fn a_full_map_evicts_in_batches_not_per_insert() {
        let map: TtlMap<u32, u32> = TtlMap::bounded(Duration::from_secs(60), 100);
        for k in 0..1_000 {
            map.insert(k, k);
        }
        assert!(map.len() <= 100, "{}", map.len());
        let passes = map.eviction_passes.load(Ordering::Relaxed);
        assert!(
            passes <= 1_000 / 10 + 1,
            "one scan per tenth of the map, not per insert: {passes}"
        );
        assert_eq!(map.get_fresh(&999), Some(999), "the newest entry stays");
    }

    #[tokio::test]
    async fn an_in_flight_fetch_survives_eviction_and_lands() {
        let map: TtlMap<u32, u32> = TtlMap::bounded(Duration::from_secs(60), 10);
        let release = Notify::new();
        let fetch = || async {
            release.notified().await;
            Ok::<_, ()>(42)
        };
        let mut pending = std::pin::pin!(map.get_or_fetch(1_000, fetch));
        assert!(futures::poll!(&mut pending).is_pending());
        for k in 0..100 {
            map.insert(k, k);
        }
        assert!(map.len() <= 11, "{}", map.len());
        release.notify_one();
        assert_eq!(pending.await, Ok(42));
        assert_eq!(map.get_fresh(&1_000), Some(42));
    }

    #[tokio::test]
    async fn a_leader_on_a_full_map_evicts_instead_of_clearing() {
        let map: TtlMap<u32, u32> = TtlMap::bounded(Duration::from_secs(60), 10);
        for k in 0..10 {
            map.insert(k, k);
        }
        assert_eq!(
            map.get_or_fetch(99, || async { Ok::<_, ()>(99) }).await,
            Ok(99)
        );
        assert!(map.len() <= 10, "{}", map.len());
        assert_eq!(map.get_fresh(&9), Some(9), "recent entries are kept");
    }
}
