// The page side of the content cache's upkeep (crates/ipfs/src/web_cache.rs): the engine's workers
// ask through the page functions below, and the service worker, which owns the cache, does the work
// on its own thread. Defined on the app page (engine.js) and on the scene server's frame
// (headless.js): each is the page of its own engine's workers.

export const CONTENT_CACHE_TRIM = 'content-cache-trim';
const REPLY_TIMEOUT_MS = 30_000;
// A write that hit the quota waits for its trim: do not hold it for long on a worker that never answers.
const QUOTA_ERROR_REPLY_TIMEOUT_MS = 5_000;

/**
 * Posts `message` to the controlling service worker and resolves with its reply, or null when
 * the page has no controller or no reply comes within `timeoutMs` (a worker without the listener).
 * @param {object} message
 * @param {ServiceWorkerContainer | undefined} container
 * @param {number} timeoutMs
 * @returns {Promise<object | null>}
 */
export function askServiceWorker(message, container = navigator.serviceWorker, timeoutMs = REPLY_TIMEOUT_MS) {
  const worker = container?.controller;
  if (!worker) return Promise.resolve(null);
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    const timer = setTimeout(() => {
      channel.port1.close();
      resolve(null);
    }, timeoutMs);
    channel.port1.onmessage = ({ data }) => {
      clearTimeout(timer);
      channel.port1.close();
      resolve(data ?? null);
    };
    worker.postMessage(message, [channel.port2]);
  });
}

/** `__contentCacheTrim`: trim to `capBytes`; after a quota error, free a quarter of it. */
export function contentCacheTrim(capBytes, quotaError, container, timeoutMs) {
  return askServiceWorker(
    { type: CONTENT_CACHE_TRIM, capBytes, quotaError: quotaError === true },
    container,
    timeoutMs ?? (quotaError === true ? QUOTA_ERROR_REPLY_TIMEOUT_MS : undefined)
  );
}

/**
 * Asks once for persistent storage, so the browser does not clear this origin under storage
 * pressure. Chromium decides without prompting; other browsers may ask the player.
 * @param {StorageManager} storage
 * @returns {Promise<boolean>}
 */
export async function requestPersistentStorage(storage = navigator.storage) {
  if (await storage.persisted()) return true;
  return storage.persist();
}

/**
 * Defines the page functions on `target` (a window). Without a controlling worker (after a hard
 * reload, say) nothing trims the cache: that is logged once, and the last cap asked for is sent
 * again when a worker takes control.
 */
export function installContentCachePageFunctions(target = window, container = navigator.serviceWorker) {
  let warned = false;
  let unanswered = null;
  container?.addEventListener?.('controllerchange', () => {
    if (unanswered === null) return;
    const capBytes = unanswered;
    unanswered = null;
    // only runs the trim: its report does not reach the engine, whose next request reads the result
    contentCacheTrim(capBytes, false, container).catch(() => {});
  });
  target.__contentCacheTrim = (capBytes, quotaError) => {
    if (!container?.controller) {
      unanswered = capBytes;
      if (!warned) {
        warned = true;
        console.warn('[content cache] no service worker controls this page: the content cache is not trimmed until one does');
      }
    }
    return contentCacheTrim(capBytes, quotaError, container);
  };
}
