//! The service worker's asset cache (deploy/web/service_worker.js), as the engine reaches it.
//! The cache name and the keys here MUST match the worker's.
//!
//! The worker only answers from the cache; the engine fills it from what it downloads. An entry
//! under the shared key answers for every content server, so it is only written once the bytes
//! are checked against the hash (`content_hash`), and under that hash; a url's response goes
//! under the url.
//!
//! The cache is capped (`effective_cap`). The service worker trims it, oldest-written entries
//! first, when the engine asks through the `__contentCacheTrim` page function
//! (deploy/web/engine/content_cache.js): after every `trim_interval` bytes written, when the cap
//! changes, and before retrying a write that failed for quota.

// stand-in origin for shared entries: `.invalid` is reserved, so no server's own entries land here
const SHARED_ORIGIN: &str = "https://shared.invalid";

// The shared key, then the url's own. The shared key is the last path segment alone: the worker
// looks a request up by it, whatever the rest of the url says.
fn keys(url: &str) -> Option<[String; 2]> {
    let url = url::Url::parse(url).ok()?;
    let name = url.path().rsplit('/').next().unwrap_or_default();
    let query = url
        .query()
        .filter(|query| !query.is_empty())
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    Some([
        format!("{SHARED_ORIGIN}/{name}"),
        format!(
            "{}{}{query}",
            url.origin().ascii_serialization(),
            url.path()
        ),
    ])
}

// Whether a request for `url` is one the worker would answer from the shared entry for `hash`.
// A url is the server's to choose (its base url, then the hash), and the path of
// `…/contents/<other hash>#/contents/<hash>` ends at the other hash: sharing that response would
// file it under a hash nobody checked it against.
fn names(url: &str, hash: &str) -> bool {
    keys(url).is_some_and(|[shared, _]| shared == format!("{SHARED_ORIGIN}/{hash}"))
}

/// The most the content cache keeps on web, whatever `AppConfig::cache_bytes` says.
pub const WEB_CACHE_CAP_BYTES: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
// The worker leaves headroom below the cap, so a request every 1/16 of it is enough.
const TRIM_INTERVAL_MAX: u64 = 64 * MIB;
// A full origin is often full for reasons the cap does not see (OPFS, IndexedDB): forced trims
// are spaced so they cannot empty the cache a quarter at a time.
const FORCED_TRIM_SPACING_MS: u64 = 60_000;

/// The cap the worker trims to: a tester's session override, else the configured size, at most
/// [`WEB_CACHE_CAP_BYTES`].
pub fn effective_cap(configured: u64, override_bytes: Option<u64>) -> u64 {
    override_bytes.unwrap_or(configured.min(WEB_CACHE_CAP_BYTES))
}

/// Whether `cap` differs from the one last applied, which it then becomes.
pub fn cap_changed(applied: &mut Option<u64>, cap: u64) -> bool {
    applied.replace(cap) != Some(cap)
}

/// A session override given in MiB: none for 0, else at least 1 MiB, so a tiny value cannot
/// empty the cache.
pub fn override_bytes(mib: f64) -> Option<u64> {
    (mib > 0.0).then(|| ((mib * MIB as f64) as u64).max(MIB))
}

/// Bytes written between two trim requests.
fn trim_interval(cap: u64) -> u64 {
    (cap / 16).clamp(MIB, TRIM_INTERVAL_MAX)
}

/// [`trim_interval`] of the lower of the engine's cap and the one the worker last trimmed to: the
/// worker bounds the cap by the quota, which the engine does not see.
fn trim_interval_for(engine_cap: u64, worker_cap: u64) -> u64 {
    trim_interval(engine_cap.min(worker_cap))
}

/// Whether writing `len` bytes after `before` bytes crosses a multiple of `interval`.
fn crosses_interval(before: u64, len: u64, interval: u64) -> bool {
    interval > 0 && before / interval != before.saturating_add(len) / interval
}

/// Whether a forced trim may run at `now_ms`, the last one having run at `last_ms` (0 = never).
fn forced_trim_allowed(last_ms: u64, now_ms: u64) -> bool {
    last_ms == 0 || now_ms.saturating_sub(last_ms) >= FORCED_TRIM_SPACING_MS
}

/// What the service worker reported for its last trim (deploy/web/service_worker.js
/// `trimContentCache`), in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TrimReport {
    pub cap_bytes: f64,
    pub usage: f64,
    pub quota: f64,
    pub caches_usage: f64,
    pub evicted_entries: f64,
    pub evicted_bytes: f64,
    pub persisted: bool,
    /// The content cache's measured bytes; `None` when the usage bound was under the cap and
    /// nothing was sized.
    pub content_bytes: Option<f64>,
}

/// The service worker's answer to a trim request.
#[derive(Clone, Debug, PartialEq)]
pub enum TrimReply {
    /// No worker answered: none controls the page, or it has no listener.
    Missing,
    /// The worker failed, or its reply lacked a reading.
    Failed(String),
    /// A regular trim that stopped for a quota-error request: nothing ran but the cap it bounded.
    Preempted {
        cap_bytes: f64,
    },
    Done(TrimReport),
}

/// Reads the worker's reply (`trimContentCache`), as JSON: `JSON.stringify` writes an infinite
/// quota as null.
pub fn parse_trim_reply(value: &serde_json::Value) -> TrimReply {
    if value.is_null() {
        return TrimReply::Missing;
    }
    if let Some(error) = value.get("error") {
        return TrimReply::Failed(
            error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned),
        );
    }
    let number = |name: &str| value.get(name).and_then(serde_json::Value::as_f64);
    let flag = |name: &str| value.get(name).and_then(serde_json::Value::as_bool) == Some(true);
    let Some(cap_bytes) = number("capBytes") else {
        return TrimReply::Failed(format!("a reply without capBytes: {value}"));
    };
    if flag("preempted") {
        return TrimReply::Preempted { cap_bytes };
    }
    let report = (|| {
        Some(TrimReport {
            cap_bytes,
            usage: number("usage")?,
            quota: match value.get("quota")? {
                serde_json::Value::Null => f64::INFINITY,
                quota => quota.as_f64()?,
            },
            caches_usage: number("cachesUsage")?,
            evicted_entries: number("evictedEntries")?,
            evicted_bytes: number("evictedBytes")?,
            persisted: flag("persisted"),
            content_bytes: number("contentBytes"),
        })
    })();
    report.map_or_else(
        || TrimReply::Failed(format!("a reply missing a reading: {value}")),
        TrimReply::Done,
    )
}

/// The engine side's readings, for the debug panel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WebCacheState {
    pub cap_bytes: u64,
    pub override_bytes: Option<u64>,
    pub written_bytes: u64,
    pub quota_retries: u64,
    pub trims: u64,
    /// Requests no service worker answered: none controls the page (after a hard reload, say), or
    /// the reply timed out (5 s after a quota error, 30 s otherwise).
    pub unanswered: u64,
    /// Summed over every trim reported this session.
    pub evicted_entries_total: u64,
    pub evicted_bytes_total: u64,
    pub last: Option<TrimReport>,
}

fn mib(bytes: f64) -> f64 {
    (bytes / MIB as f64 * 10.0).round() / 10.0
}

/// The `/web_content_cache` reply.
pub fn state_json(state: &WebCacheState) -> serde_json::Value {
    let last = state.last;
    let reason = "The service worker has not answered a trim yet: it does not control this page, or none ran.";
    // a quota the browser leaves out reaches the page as infinity, which JSON writes as null
    let quota_reason = if last.is_some() {
        "The browser reported no quota."
    } else {
        reason
    };
    serde_json::json!({
        "capMiB": mib(state.cap_bytes as f64),
        "overrideMiB": state.override_bytes.map(|b| mib(b as f64)),
        "writtenMiB": mib(state.written_bytes as f64),
        "quotaRetries": state.quota_retries,
        "trims": state.trims,
        "unansweredTrims": state.unanswered,
        "evictedEntriesTotal": state.evicted_entries_total,
        "evictedTotalMiB": mib(state.evicted_bytes_total as f64),
        "workerCapMiB": last.map(|r| mib(r.cap_bytes)),
        "usageMiB": last.map(|r| mib(r.usage)),
        "quotaMiB": last.and_then(|r| r.quota.is_finite().then(|| mib(r.quota))),
        "cachesMiB": last.map(|r| mib(r.caches_usage)),
        "evictedEntries": last.map(|r| r.evicted_entries),
        "evictedMiB": last.map(|r| mib(r.evicted_bytes)),
        "persisted": last.map(|r| r.persisted),
        "contentMiB": last.and_then(|r| r.content_bytes.map(mib)),
        "unavailable": {
            "overrideMiB": "No override: the configured cap applies.",
            "workerCapMiB": reason, "usageMiB": reason, "quotaMiB": quota_reason, "cachesMiB": reason,
            "evictedEntries": reason, "evictedMiB": reason, "persisted": reason,
            "contentMiB": if last.is_some() {
                "Cache Storage usage was at or under the cap: the content cache was not sized."
            } else {
                reason
            },
        },
    })
}

#[cfg(target_arch = "wasm32")]
#[bevy::web_worker::page_functions]
#[wasm_bindgen::prelude::wasm_bindgen(js_namespace = self)]
extern "C" {
    /// deploy/web/engine/content_cache.js: asks the service worker to trim the cache to
    /// `cap_bytes`, freeing the oldest content worth a quarter of it when `quota_error`. Resolves with the worker's
    /// report, or null when no worker answered.
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = "__contentCacheTrim")]
    fn page_content_cache_trim(cap_bytes: f64, quota_error: bool) -> js_sys::Promise;
}

#[cfg(target_arch = "wasm32")]
pub use io::*;

#[cfg(target_arch = "wasm32")]
mod io {
    use super::{
        crosses_interval, forced_trim_allowed, keys, names, page_content_cache_trim,
        parse_trim_reply, trim_interval_for, TrimReply, TrimReport, WebCacheState,
        WEB_CACHE_CAP_BYTES,
    };
    use js_sys::Uint8Array;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, PoisonError,
    };
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Cache, CacheStorage, Headers, Response, ResponseInit};

    const CACHE_NAME: &str = "ipfs-path-cache-v2";

    pub async fn open() -> Result<Cache, JsValue> {
        // a window or a worker
        let caches: CacheStorage =
            js_sys::Reflect::get(&js_sys::global(), &"caches".into())?.dyn_into()?;
        JsFuture::from(caches.open(CACHE_NAME)).await?.dyn_into()
    }

    /// The entry under `key`.
    pub async fn get(cache: &Cache, key: &str) -> Result<Option<Response>, JsValue> {
        let response = JsFuture::from(cache.match_with_str(key)).await?;
        if response.is_undefined() {
            return Ok(None);
        }
        Ok(Some(response.dyn_into()?))
    }

    /// The cached entry for `url`, and the key it is under.
    pub async fn find(cache: &Cache, url: &str) -> Result<(String, Response), JsValue> {
        for key in keys(url).ok_or("not a url")? {
            if let Some(response) = get(cache, &key).await? {
                return Ok((key, response));
            }
        }
        Err("no previous cache item".into())
    }

    /// The cached bytes for `url`, under either key; `None` when there are none (or no cache).
    pub async fn read(url: &str) -> Option<Vec<u8>> {
        async fn read(url: &str) -> Result<Vec<u8>, JsValue> {
            let cache = open().await?;
            let (_, response) = find(&cache, url).await?;
            bytes(&response).await
        }
        read(url).await.ok()
    }

    /// The body of a cached response.
    pub async fn bytes(response: &Response) -> Result<Vec<u8>, JsValue> {
        let buffer: js_sys::ArrayBuffer =
            JsFuture::from(response.array_buffer()?).await?.dyn_into()?;
        let array = Uint8Array::new(&buffer);
        let mut data = vec![0u8; array.length() as usize];
        array.copy_to(&mut data);
        Ok(data)
    }

    async fn put_once(
        cache: &Cache,
        key: &str,
        old: &Response,
        data: &[u8],
    ) -> Result<(), JsValue> {
        let headers = old.headers();
        headers.set("Content-Length", &data.len().to_string())?;

        let init = ResponseInit::new();
        init.set_status(old.status());
        init.set_status_text(&old.status_text());
        init.set_headers(&headers);

        let body = Uint8Array::from(data);
        let response = Response::new_with_opt_buffer_source_and_init(Some(&body), &init)?;
        JsFuture::from(cache.put_with_str(key, &response)).await?;
        Ok(())
    }

    async fn store_once(cache: &Cache, key: &str, data: &[u8]) -> Result<(), JsValue> {
        let headers = Headers::new()?;
        headers.set("Content-Type", "application/octet-stream")?;
        headers.set("Content-Length", &data.len().to_string())?;

        let init = ResponseInit::new();
        init.set_status(200);
        init.set_headers(&headers);

        let body = Uint8Array::from(data);
        let response = Response::new_with_opt_buffer_source_and_init(Some(&body), &init)?;
        JsFuture::from(cache.put_with_str(key, &response)).await?;
        Ok(())
    }

    /// Stores `data`, the bytes `url` answered with, under the url's own key.
    pub async fn keep(url: &str, data: &[u8]) -> Result<(), JsValue> {
        let [_, own] = keys(url).ok_or("not a url")?;
        let cache = open().await?;
        store(&cache, &own, data).await
    }

    /// Drops the entry under the url's own key, if there is one.
    pub async fn forget(url: &str) -> Result<(), JsValue> {
        let [_, own] = keys(url).ok_or("not a url")?;
        let cache = open().await?;
        JsFuture::from(cache.delete_with_str(&own)).await?;
        Ok(())
    }

    /// Stores `data`, the bytes `url` answered with, under the shared key for `hash`, if
    /// `matches_hash`: whether they are what `hash` names. The url's own entry, if an older
    /// worker left one, goes: it is dead weight beside the shared one.
    pub async fn share(
        url: &str,
        hash: &str,
        data: &[u8],
        matches_hash: impl FnOnce() -> bool,
    ) -> Result<(), JsValue> {
        if !names(url, hash) || !matches_hash() {
            return Ok(());
        }
        let [shared, own] = keys(url).ok_or("not a url")?;
        let cache = open().await?;
        store(&cache, &shared, data).await?;
        JsFuture::from(cache.delete_with_str(&own)).await?;
        Ok(())
    }

    // Written from the asset loader and processor workers, which have no `World`; one wasm memory
    // is shared by every worker of an engine instance.
    static CAP_BYTES: AtomicU64 = AtomicU64::new(WEB_CACHE_CAP_BYTES);
    // the cap of the worker's last report (u64::MAX = none yet)
    static WORKER_CAP_BYTES: AtomicU64 = AtomicU64::new(u64::MAX);
    static WRITTEN_BYTES: AtomicU64 = AtomicU64::new(0);
    static LAST_FORCED_MS: AtomicU64 = AtomicU64::new(0);
    static QUOTA_RETRIES: AtomicU64 = AtomicU64::new(0);
    static TRIMS: AtomicU64 = AtomicU64::new(0);
    static UNANSWERED: AtomicU64 = AtomicU64::new(0);
    static EVICTED_ENTRIES: AtomicU64 = AtomicU64::new(0);
    static EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);
    static LAST_REPORT: Mutex<Option<TrimReport>> = Mutex::new(None);

    /// Replaces the body of the entry under `key`.
    pub async fn put(cache: &Cache, key: &str, old: &Response, data: &[u8]) -> Result<(), JsValue> {
        write_with_quota_retry(key, data.len(), || put_once(cache, key, old, data)).await
    }

    /// Stores `data` under `key` as a fresh response.
    async fn store(cache: &Cache, key: &str, data: &[u8]) -> Result<(), JsValue> {
        write_with_quota_retry(key, data.len(), || store_once(cache, key, data)).await
    }

    /// The cap the worker trims to.
    pub fn set_cap(cap_bytes: u64) {
        CAP_BYTES.store(cap_bytes, Ordering::Relaxed);
    }

    /// Asks the service worker for a trim to the current cap and keeps its report.
    pub async fn request_trim(quota_error: bool) -> Option<TrimReport> {
        let cap = CAP_BYTES.load(Ordering::Relaxed) as f64;
        let reply = JsFuture::from(page_content_cache_trim(cap, quota_error))
            .await
            .map_err(|e| bevy::log::warn!("content cache trim failed: {e:?}"))
            .ok()?;
        // only no reply is `Missing`: one that does not read as JSON is a failure to log
        let parsed = if reply.is_null() || reply.is_undefined() {
            TrimReply::Missing
        } else {
            js_sys::JSON::stringify(&reply)
                .ok()
                .and_then(|text| text.as_string())
                .and_then(|text| serde_json::from_str(&text).ok())
                .map_or_else(
                    || TrimReply::Failed(format!("an unreadable reply: {reply:?}")),
                    |json| parse_trim_reply(&json),
                )
        };
        let report = match parsed {
            TrimReply::Done(report) => report,
            TrimReply::Preempted { cap_bytes } => {
                WORKER_CAP_BYTES.store(cap_bytes as u64, Ordering::Relaxed);
                return None;
            }
            TrimReply::Failed(error) => {
                bevy::log::warn!("content cache trim failed in the service worker: {error}");
                return None;
            }
            TrimReply::Missing => {
                UNANSWERED.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        TRIMS.fetch_add(1, Ordering::Relaxed);
        WORKER_CAP_BYTES.store(report.cap_bytes as u64, Ordering::Relaxed);
        EVICTED_ENTRIES.fetch_add(report.evicted_entries as u64, Ordering::Relaxed);
        EVICTED_BYTES.fetch_add(report.evicted_bytes as u64, Ordering::Relaxed);
        *LAST_REPORT.lock().unwrap_or_else(PoisonError::into_inner) = Some(report);
        Some(report)
    }

    /// The engine side's readings, for the debug panel.
    pub fn state(override_bytes: Option<u64>) -> WebCacheState {
        WebCacheState {
            cap_bytes: CAP_BYTES.load(Ordering::Relaxed),
            override_bytes,
            written_bytes: WRITTEN_BYTES.load(Ordering::Relaxed),
            quota_retries: QUOTA_RETRIES.load(Ordering::Relaxed),
            trims: TRIMS.load(Ordering::Relaxed),
            unanswered: UNANSWERED.load(Ordering::Relaxed),
            evicted_entries_total: EVICTED_ENTRIES.load(Ordering::Relaxed),
            evicted_bytes_total: EVICTED_BYTES.load(Ordering::Relaxed),
            last: *LAST_REPORT.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }

    // A failed `put` may have consumed its response's body: `write` builds a fresh one each call.
    async fn write_with_quota_retry<F, Fut>(key: &str, len: usize, write: F) -> Result<(), JsValue>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(), JsValue>>,
    {
        let result = match write().await {
            Err(e) if is_quota_error(&e) && claim_forced_trim() => {
                QUOTA_RETRIES.fetch_add(1, Ordering::Relaxed);
                bevy::log::debug!("content cache full writing `{key}`: trimming and retrying");
                request_trim(true).await;
                write().await
            }
            other => other,
        };
        if result.is_ok() {
            note_written(len as u64);
        }
        result
    }

    fn is_quota_error(error: &JsValue) -> bool {
        js_sys::Reflect::get(error, &"name".into())
            .ok()
            .and_then(|name| name.as_string())
            .is_some_and(|name| name == "QuotaExceededError")
    }

    fn claim_forced_trim() -> bool {
        let now = js_sys::Date::now() as u64;
        let last = LAST_FORCED_MS.load(Ordering::Relaxed);
        forced_trim_allowed(last, now)
            && LAST_FORCED_MS
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }

    fn note_written(len: u64) {
        let interval = trim_interval_for(
            CAP_BYTES.load(Ordering::Relaxed),
            WORKER_CAP_BYTES.load(Ordering::Relaxed),
        );
        let before = WRITTEN_BYTES.fetch_add(len, Ordering::Relaxed);
        if crosses_interval(before, len, interval) {
            wasm_bindgen_futures::spawn_local(async {
                request_trim(false).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_key_is_the_hash_whichever_server_and_path() {
        let [catalyst, _] = keys("https://peer.example/content/contents/bafkreihash").unwrap();
        let [worlds, own] = keys("https://worlds.example/contents/bafkreihash?v=1").unwrap();
        assert_eq!(catalyst, "https://shared.invalid/bafkreihash");
        assert_eq!(worlds, catalyst);
        assert_eq!(own, "https://worlds.example/contents/bafkreihash?v=1");
    }

    #[test]
    fn a_url_only_names_the_hash_its_path_ends_with() {
        assert!(names(
            "https://peer.example/contents/bafkreihash",
            "bafkreihash"
        ));
        // the worker keys this request by `bafkreitarget`
        let url = "https://evil.example/contents/bafkreitarget#/contents/bafkreihash";
        assert!(!names(url, "bafkreihash"));
        assert_eq!(
            keys(url).unwrap(),
            [
                "https://shared.invalid/bafkreitarget".to_owned(),
                "https://evil.example/contents/bafkreitarget".to_owned()
            ]
        );
        assert!(!names(
            "https://evil.example/contents/bafkreitarget?/bafkreihash",
            "bafkreihash"
        ));
        assert!(!names(
            "https://peer.example/contents/bafkreihash/",
            "bafkreihash"
        ));
    }

    #[test]
    fn web_cap_is_at_most_a_gibibyte_whatever_the_config() {
        assert_eq!(
            effective_cap(common::structs::AppConfig::default().cache_bytes, None),
            WEB_CACHE_CAP_BYTES
        );
        assert_eq!(effective_cap(200 << 20, None), 200 << 20);
    }

    #[test]
    fn a_session_override_replaces_the_configured_cap() {
        assert_eq!(effective_cap(10 << 30, Some(50 << 20)), 50 << 20);
        assert_eq!(effective_cap(10 << 30, Some(4 << 30)), 4 << 30);
    }

    #[test]
    fn an_override_is_none_for_zero_and_at_least_a_mebibyte() {
        assert_eq!(override_bytes(0.0), None);
        assert_eq!(override_bytes(0.0000001), Some(1 << 20));
        assert_eq!(override_bytes(0.5), Some(1 << 20));
        assert_eq!(override_bytes(50.0), Some(50 << 20));
    }

    #[test]
    fn a_reply_under_the_cap_has_no_content_total() {
        let reply = serde_json::json!({
            "capBytes": 1000.0, "usage": 900.0, "quota": 1e9, "cachesUsage": 800.0,
            "evictedEntries": 0.0, "evictedBytes": 0.0, "contentBytes": null, "preempted": false,
        });
        let TrimReply::Done(report) = parse_trim_reply(&reply) else {
            panic!("not a report");
        };
        assert_eq!(report.content_bytes, None);
        let state = WebCacheState {
            unanswered: 2,
            last: Some(report),
            ..Default::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&state_json(&state).to_string()).unwrap();
        assert!(json["contentMiB"].is_null());
        assert!(json["unavailable"]["contentMiB"].is_string());
        assert_eq!(json["unansweredTrims"], 2);
    }

    #[test]
    fn only_a_new_cap_counts_as_a_change() {
        let mut applied = None;
        // the first run, at start
        assert!(cap_changed(&mut applied, 1 << 30));
        assert!(!cap_changed(&mut applied, 1 << 30));
        assert!(cap_changed(&mut applied, 50 << 20));
        assert!(cap_changed(&mut applied, 1 << 30));
    }

    #[test]
    fn trims_are_requested_every_sixteenth_of_the_cap() {
        assert_eq!(trim_interval(1 << 30), 64 << 20);
        assert!(crosses_interval(63 << 20, 1 << 20, 64 << 20));
        assert!(!crosses_interval(0, (64 << 20) - 1, 64 << 20));
        assert!(!crosses_interval(10, 0, 64 << 20));
    }

    #[test]
    fn small_caps_trim_at_least_every_mebibyte_and_large_ones_every_64() {
        assert_eq!(trim_interval(4 << 20), 1 << 20);
        assert_eq!(trim_interval(16 << 30), 64 << 20);
        assert_eq!(trim_interval(50 << 20), (50 << 20) / 16);
    }

    #[test]
    fn trims_follow_the_cap_the_worker_bounded_by_the_quota() {
        assert_eq!(trim_interval_for(1 << 30, 100 << 20), (100 << 20) / 16);
        assert_eq!(trim_interval_for(1 << 30, u64::MAX), 64 << 20);
        assert_eq!(trim_interval_for(50 << 20, 1 << 30), (50 << 20) / 16);
    }

    #[test]
    fn forced_trims_are_a_minute_apart() {
        assert!(forced_trim_allowed(0, 5));
        assert!(!forced_trim_allowed(1_000, 60_999));
        assert!(forced_trim_allowed(1_000, 61_000));
        // the clock went back
        assert!(!forced_trim_allowed(70_000, 1_000));
    }

    #[test]
    fn state_without_a_report_names_why_the_worker_readings_are_missing() {
        let state = WebCacheState {
            cap_bytes: 1 << 30,
            ..Default::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&state_json(&state).to_string()).unwrap();
        assert_eq!(json["capMiB"], 1024.0);
        assert!(json["usageMiB"].is_null());
        assert!(json["unavailable"]["usageMiB"].is_string());
    }

    #[test]
    fn state_reports_the_last_trim_in_mebibytes() {
        let state = WebCacheState {
            last: Some(TrimReport {
                evicted_bytes: 3.0 * 1048576.0 + 52429.0,
                persisted: true,
                content_bytes: Some(2.0 * 1048576.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&state_json(&state).to_string()).unwrap();
        assert_eq!(json["evictedMiB"], 3.1);
        assert_eq!(json["persisted"], true);
        assert_eq!(json["contentMiB"], 2.0);
    }

    #[test]
    fn a_trim_reply_is_read_with_an_infinite_quota_as_null() {
        let reply = serde_json::json!({
            "capBytes": 1000.0, "usage": 900.0, "quota": null, "cachesUsage": 800.0,
            "evictedEntries": 3.0, "evictedBytes": 300.0, "persisted": true,
            "contentBytes": 750.0, "preempted": false,
        });
        assert_eq!(
            parse_trim_reply(&reply),
            TrimReply::Done(TrimReport {
                cap_bytes: 1000.0,
                usage: 900.0,
                quota: f64::INFINITY,
                caches_usage: 800.0,
                evicted_entries: 3.0,
                evicted_bytes: 300.0,
                persisted: true,
                content_bytes: Some(750.0),
            })
        );
    }

    #[test]
    fn a_preempted_trim_keeps_only_its_cap() {
        let reply = serde_json::json!({
            "capBytes": 500.0, "usage": 900.0, "quota": 1e9, "cachesUsage": 800.0,
            "evictedEntries": 0.0, "evictedBytes": 0.0, "preempted": true,
        });
        assert_eq!(
            parse_trim_reply(&reply),
            TrimReply::Preempted { cap_bytes: 500.0 }
        );
    }

    #[test]
    fn no_reply_an_error_and_a_short_reply_are_not_reports() {
        assert_eq!(
            parse_trim_reply(&serde_json::Value::Null),
            TrimReply::Missing
        );
        assert_eq!(
            parse_trim_reply(
                &serde_json::json!({ "error": "capBytes must be a non-negative number" })
            ),
            TrimReply::Failed("capBytes must be a non-negative number".into())
        );
        let short = serde_json::json!({ "capBytes": 500.0, "usage": 900.0 });
        assert!(matches!(parse_trim_reply(&short), TrimReply::Failed(_)));
    }

    #[test]
    fn state_sums_the_evictions_of_every_trim() {
        let state = WebCacheState {
            evicted_entries_total: 160,
            evicted_bytes_total: 10 << 20,
            ..Default::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&state_json(&state).to_string()).unwrap();
        assert_eq!(json["evictedEntriesTotal"], 160);
        assert_eq!(json["evictedTotalMiB"], 10.0);
    }

    #[test]
    fn a_report_without_a_quota_says_the_browser_gave_none() {
        let state = WebCacheState {
            last: Some(TrimReport {
                usage: 1048576.0,
                quota: f64::INFINITY,
                ..Default::default()
            }),
            ..Default::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&state_json(&state).to_string()).unwrap();
        assert!(json["quotaMiB"].is_null());
        assert_eq!(
            json["unavailable"]["quotaMiB"],
            "The browser reported no quota."
        );
        // the other readings are the report's, not missing
        assert_eq!(json["usageMiB"], 1.0);
    }

    #[test]
    fn a_report_with_a_quota_shows_it() {
        let state = WebCacheState {
            last: Some(TrimReport {
                quota: 2.0 * 1048576.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&state_json(&state).to_string()).unwrap();
        assert_eq!(json["quotaMiB"], 2.0);
    }
}
