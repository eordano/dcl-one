// Each sandbox worker shares five sends between fetch and signedFetch, and sends what scenes
// ask of third-party services through the page's own node (createSceneRoutes).

const ROUTED_METHODS = new Set(["GET", "HEAD", "POST"]);
// A node that fails or cannot be reached is asked once a minute, not at each spawn of a worker: the
// workers spawned meanwhile send direct.
const ROUTES_RETRY_MS = 60_000;

// The node's routing map, `GET /_scene` (services/catalyst/src/scene_proxy.rs): host to a path
// prefix on this origin. The page reads it once and hands it to every worker it spawns
// (sandbox_host.js). A node that answers without a map has none: the page keeps an empty one and
// does not ask again.
export function createSceneRoutes(nativeFetch, origin = self.location.origin) {
  let routes = null;
  let pending = null;
  let failedAt = 0;
  return function sceneRoutes() {
    if (routes) return Promise.resolve(routes);
    if (pending) return pending;
    if (failedAt && Date.now() - failedAt < ROUTES_RETRY_MS) return Promise.resolve(null);
    pending = nativeFetch(`${origin}/_scene`, { credentials: "omit" })
      .then(async (response) => {
        // An answer that is not read as a map: its body is dropped, so it holds no connection.
        if (!response.ok) await response.body?.cancel().catch(() => {});
        if (response.status >= 500) throw new Error(`routing map: ${response.status}`);
        const body = response.ok ? await response.json().catch(() => null) : null;
        routes = new Map(Object.entries(body?.routes ?? {}));
        return routes;
      })
      .catch(() => { failedAt = Date.now(); return null; })
      .finally(() => { pending = null; });
    return pending;
  };
}

// `https://<host>/<path>` becomes `<origin><prefix>/<path>` when the map names the host, for the
// methods the proxy forwards; anything else stays as it is (null). A GET or HEAD is re-addressed
// at once; a POST once its body is read (a promise), as a stream body cannot be re-addressed.
// Credentials are omitted, so nothing a scene sends carries the page's session.
export function routeSceneRequest(request, routes, origin = self.location.origin) {
  if (!routes || !ROUTED_METHODS.has(request.method)) return null;
  let url;
  try {
    url = new URL(request.url);
  } catch {
    return null;
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") return null;
  const prefix = routes.get(url.host);
  if (prefix === undefined) return null;
  const readdress = (body) => new Request(`${origin}${prefix}${url.pathname}${url.search}`, {
    method: request.method,
    headers: request.headers,
    body,
    credentials: "omit",
    referrerPolicy: "no-referrer",
    signal: request.signal,
  });
  return request.method === "POST" ? request.arrayBuffer().then(readdress) : readdress(undefined);
}

// `sceneRoutes` returns the current map, or null while there is none.
export function createSceneFetch(nativeFetch, sceneRoutes = () => null) {
  const queue = new Set();
  let active = 0;

  return function sceneFetch(input, init) {
    return new Promise((resolve, reject) => {
      // Snapshot caller-owned headers, body and options before waiting for a slot.
      const request = new Request(input, init);
      const { signal } = request;
      // Chromium only exposes its local-network annotation on RequestInit. Passing
      // a new init to fetch also resets the referrer unless we supply it again.
      const addressSpace = init?.targetAddressSpace;
      const options = addressSpace === undefined ? undefined : {
        targetAddressSpace: String(addressSpace),
        referrer: request.referrer,
        referrerPolicy: request.referrerPolicy,
      };

      function cancelQueued() {
        queue.delete(start);
        // Notify an unsent upload's producer without delaying the rejection.
        request.body?.cancel(signal.reason).catch(() => {});
        reject(signal.reason);
      }

      async function start() {
        queue.delete(start);
        signal.removeEventListener("abort", cancelQueued);
        active++;
        try {
          // A send that waits for nothing leaves the moment its slot starts.
          const routed = routeSceneRequest(request, sceneRoutes());
          resolve(await (routed instanceof Promise
            ? routed.then((readdressed) => nativeFetch(readdressed))
            : nativeFetch(routed ?? request, routed ? undefined : options)));
        } catch (error) {
          reject(error);
        } finally {
          // Browser fetch settles at headers. The caller owns the response stream.
          active--;
          queue.values().next().value?.();
        }
      }

      if (signal.aborted) cancelQueued();
      else if (active < 5) start();
      else {
        queue.add(start);
        signal.addEventListener("abort", cancelQueued, { once: true });
      }
    });
  };
}
