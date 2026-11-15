// Which upstream service a request belongs to, for both sides of a parity capture.
//
// The reference shop and the sites SPA call the production hosts directly; ui3 calls the node's
// canonical `/shop-api` (and the compatibility mounts), which library/shop/api/endpoints.ts maps
// onto the same upstream routes, and apps/sites calls its own `/api/*`, the node's parity port of
// the places and events services. All resolve to one (service, path) so a recorded answer is
// served to both.
import { ENDPOINTS, SHOP_API_PREFIX, type Endpoint } from "../../../library/shop/api/endpoints.ts";

export type Service =
  | "marketplace"
  | "credits"
  | "shop-server"
  | "builder"
  | "peer"
  | "feature-flags"
  | "cms"
  /** The CMS's pictures (cms-server logic/urls: images.ctfassets.net served as cms-images.decentraland.org). */
  | "cms-images"
  | "places"
  | "camera-reel"
  | "rpc"
  | "notifications"
  | "transactions"
  | "signatures"
  | "fonts"
  | "fonts-static"
  | "wearable-preview"
  | "offers"
  | "node"
  | "events"
  /** decentraland.org's own origin: what the sites SPA reads from `/api/*` on it. */
  | "sites"
  | "social"
  | "realm-provider"
  | "worlds-content"
  | "profile"
  | "subgraph"
  | "cdn-data"
  | "docs"
  | "market"
  | "auth"
  | "gatekeeper"
  | "badges"
  | "storage"
  | "assets-cdn"
  /** Contentful's video CDN as decentraland.org reads it (sites: hooks/contentful.ts optimizeVideo): the invite page's clip. */
  | "cms-videos"
  /** GitHub's REST API: the sites /create reads the Creator Hub's latest release for its download links. */
  | "github-api"
  /** The twemoji flags the sites footer's language button loads from jsdelivr (sites: LandingFooter.tsx flagUrl). */
  | "twemoji";

/** The production origin of each upstream service; record mode reads these, replay never does. */
export const ORIGINS: Partial<Record<Service, string>> = {
  marketplace: "https://marketplace-api.decentraland.org",
  credits: "https://credits.decentraland.org",
  "shop-server": "https://shop-api.decentraland.org",
  builder: "https://builder-api.decentraland.org",
  peer: "https://peer.decentraland.org",
  "feature-flags": "https://feature-flags.decentraland.org",
  cms: "https://cms-api.decentraland.org",
  "cms-images": "https://cms-images.decentraland.org",
  places: "https://places.decentraland.org",
  "camera-reel": "https://camera-reel-service.decentraland.org",
  rpc: "https://rpc.decentraland.org",
  notifications: "https://notifications.decentraland.org",
  transactions: "https://transactions-api.decentraland.org",
  signatures: "https://signatures-api.decentraland.org",
  fonts: "https://fonts.googleapis.com",
  "fonts-static": "https://fonts.gstatic.com",
  events: "https://events.decentraland.org",
  sites: "https://decentraland.org",
  social: "https://social-api.decentraland.org",
  "realm-provider": "https://realm-provider-ea.decentraland.org",
  "worlds-content": "https://worlds-content-server.decentraland.org",
  profile: "https://profile.decentraland.org",
  subgraph: "https://subgraph.decentraland.org",
  "cdn-data": "https://cdn-data.decentraland.org",
  docs: "https://docs.decentraland.org",
  market: "https://market.decentraland.org",
  auth: "https://auth-api.decentraland.org",
  gatekeeper: "https://comms-gatekeeper.decentraland.org",
  badges: "https://badges.decentraland.org",
  storage: "https://storage.decentraland.org",
  "assets-cdn": "https://assets-cdn.decentraland.org",
  "github-api": "https://api.github.com",
  "cms-videos": "https://cms-videos.decentraland.org",
  twemoji: "https://cdn.jsdelivr.net",
};

const HOSTS = new Map<string, Service>(Object.entries(ORIGINS).map(([s, o]) => [new URL(o).host, s as Service]));
HOSTS.set("wearable-preview.decentraland.org", "wearable-preview");
// jsdelivr serves anything; only the twemoji flags are a known asset (TWEMOJI), the rest of the host stays unknown.
HOSTS.delete("cdn.jsdelivr.net");
/** A twemoji flag on jsdelivr: a static asset recorded as it is, not a content picture. */
const TWEMOJI = /^\/gh\/twitter\/twemoji@[^/]+\/assets\/svg\/[0-9a-f-]+\.svg$/;

export function serviceForHost(host: string): Service | undefined {
  return HOSTS.get(host);
}

/** Third-party telemetry and payment scripts: answered empty, never recorded. */
const BLOCKED = [
  /(^|\.)segment\.(com|io)$/,
  /(^|\.)sentry\.io$/,
  /(^|\.)google-analytics\.com$/,
  /(^|\.)googletagmanager\.com$/,
  /(^|\.)doubleclick\.net$/,
  /(^|\.)hotjar\.(com|io)$/,
  /(^|\.)intercom(cdn)?\.(io|com)$/,
  /(^|\.)stripe\.(com|network)$/,
  /(^|\.)walletconnect\.(com|org)$/,
  /(^|\.)reown\.com$/,
  /(^|\.)thirdweb\.com$/,
  /(^|\.)growthbook\.io$/,
  // Decentraland's own Segment CDN and API (the sites SPA's SEGMENT_CDN_URL and SEGMENT_API_HOST).
  /^(evs|api)\.e\.decentraland\.org$/,
  // The sites SPA's Contentsquare session analytics.
  /(^|\.)contentsquare\.net$/,
  // The Explorer installers /download_success starts (decentraland-ui2 cdnReleases, the sites download gateway):
  // a capture never saves a download, and an empty answer keeps the page where a 404 would navigate away.
  /^explorer-artifacts\.decentraland\.org$/,
  /^download-gateway\.decentraland\.(org|zone)$/,
];

export type Resolved =
  | { kind: "api"; service: Service; path: string }
  | { kind: "image" }
  | { kind: "blocked" }
  | { kind: "unknown"; host: string };

/** The node mounts whose pictures are content (thumbnails, avatars, outfit renders), never app assets. */
export const OWN_IMAGE_MOUNTS = /^\/(lambdas|content|media|profile-images|shop-api\/v1\/outfits\/thumbnails)\//;

const IMAGE_EXT = /\.(png|jpe?g|webp|gif|avif|svg|ico)$/i;

/** Requests that are pictures of content (thumbnails, avatars, covers): answered with a placeholder. */
export function isContentImage(url: URL, resourceType?: string): boolean {
  if (resourceType === "image") return true;
  const p = url.pathname;
  if (IMAGE_EXT.test(p)) return true;
  if (/\/(thumbnail|image|face256|face|body)$/.test(p)) return true;
  if (/\/v1\/outfits\/thumbnails\/[^/]+$/.test(p)) return true;
  if (/^\/(lambdas\/collections\/contents|content\/contents|media|profile-images)\//.test(p) && !/\/entities?\//.test(p)) return true;
  return false;
}

function matchTemplate(template: string, path: string): boolean {
  const re = new RegExp(`^${template.replace(/[.*+?^$()|[\]\\]/g, "\\$&").replace(/\\?\{[A-Za-z]+\\?\}/g, "[^/]+")}$`);
  return re.test(path);
}

function endpointFor(method: string, path: string): Endpoint | null {
  let best: Endpoint | null = null;
  for (const e of Object.values(ENDPOINTS) as Endpoint[]) {
    if (e.method !== method || !matchTemplate(e.path, path)) continue;
    // A literal segment beats a template one (/v1/trades-mirror over /v1/trades/{id}).
    if (!best || e.path.split("{").length < best.path.split("{").length) best = e;
  }
  return best;
}

/** A canonical `/shop-api/<path>` request as the upstream route it stands for. */
export function fromShopApi(method: string, path: string): { service: Service; path: string } | null {
  const e = endpointFor(method, path);
  if (!e) return null;
  if (e.path === ENDPOINTS.purchaseOffers.path) return { service: "offers", path };
  const fill = (legacy: string) => {
    const names = [...e.path.matchAll(/\{([A-Za-z]+)\}/g)].map((m) => m[1]!);
    const values = path.match(new RegExp(`^${e.path.replace(/\{[A-Za-z]+\}/g, "([^/]+)")}$`))?.slice(1) ?? [];
    let out = legacy;
    names.forEach((n, i) => (out = out.replace(`{${n}}`, values[i] ?? "")));
    return out;
  };
  if (e.legacy === null) return { service: "shop-server", path };
  const legacy = fill(e.legacy);
  for (const [prefix, service] of [
    ["/marketplace-api", "marketplace"],
    ["/credits", "credits"],
    ["/transactions-api", "transactions"],
    ["/signatures-api", "signatures"],
  ] as const) {
    if (legacy.startsWith(`${prefix}/`)) return { service, path: legacy.slice(prefix.length) };
  }
  return null;
}

/**
 * Every path production routes off the sites app, so a page's read reaches the replay (and is listed as a
 * miss when the recording lacks it) instead of rendering the app's 404 page. It covers each same-origin
 * service path the production sites build reads (apps/sites/.env.production), and the gateway's /camera-reel
 * alias the profile gallery requests directly (apps/sites/src/data/lib/catalyst/social/photos.ts).
 */
const NODE_MOUNTS: readonly string[] = ["cms", "lambdas", "content", "builder-items", "auth-api", "builder-api", "shop-api", "marketplace-api", "transactions-api", "signatures-api", "rpc", "camera-reel", "camera-reel-service", "report-user", "media", "profile-images", "credits", "api/events", "api/places", "api/destinations", "api/schedules", "api/map", "telemetry", "places/api", "events/api", "social-api", "badges", "worlds-content-server", "comms-gatekeeper", "cdn-data", "feature-flags"];

/** Whether the sites preview relays this path to the replay rather than serving it from the sites app. */
export function relaysToNode(path: string): boolean {
  return NODE_MOUNTS.some((m) => path === `/${m}` || path.startsWith(`/${m}/`));
}

/** The node's prefixes for a recorded upstream, which answers the path without the prefix. */
const UPSTREAM_PREFIXES: readonly (readonly [string, Service])[] = [
  ["/marketplace-api", "marketplace"],
  ["/credits", "credits"],
  ["/transactions-api", "transactions"],
  ["/signatures-api", "signatures"],
  ["/builder-api", "builder"],
  // The node serves the camera reel's API under /camera-reel-service, and the gateway aliases /camera-reel to it.
  ["/camera-reel-service", "camera-reel"],
  ["/camera-reel", "camera-reel"],
  ["/rpc", "rpc"],
  // The social bundle's mounts (services/catalyst/social/src/lib.rs): the social-api and badges hosts' roots.
  ["/social-api", "social"],
  ["/badges", "badges"],
  // The node's worlds content server (apps/sites WORLDS_PREFIX), at the upstream paths under that mount.
  ["/worlds-content-server", "worlds-content"],
  // The node's comms gatekeeper (services/catalyst/src/serve.rs nests it at /comms-gatekeeper): the place page's scene chat.
  ["/comms-gatekeeper", "gatekeeper"],
  // The Explorer's download counts the sites server reads (DOWNLOAD_COUNTS_URL, lib/run.mts), from the recorded CDN.
  ["/cdn-data", "cdn-data"],
  // The dapps rollout flags the sites server reads (FEATURE_FLAGS_URL, lib/run.mts), from the recorded service.
  ["/feature-flags", "feature-flags"],
  // The blog's CMS the sites server reads (BLOG_CMS_URL, servers.mts startSites): the same recorded cms-api answers the reference reads.
  ["/cms", "cms"],
];

/** The blog's CMS space under cms-api, as decentraland.org reads it (sites: features/cms getCmsBaseUrl). */
export const CMS_SPACE_PATH = "/spaces/ea2ybdmmn1kv/environments/master";

/** The node's prefixes the recorded peer answers with the prefix kept. */
const PEER_PREFIXES: readonly string[] = ["/lambdas", "/content"];

/** Every prefix `fromNodePath` answers from a recorded service: anything else is the node's own, never fetched. */
export const RECORDED_NODE_PREFIXES: readonly string[] = [SHOP_API_PREFIX, ...UPSTREAM_PREFIXES.map(([prefix]) => prefix), ...PEER_PREFIXES];

/** The node's discovery API: its parity port of the places and events services, under the same paths. */
const DISCOVERY_API: [RegExp, Service][] = [
  [/^\/api\/(places|destinations|map)(\/|$)/, "places"],
  [/^\/api\/(events|schedules)(\/|$)/, "events"],
];

/**
 * The same services under the node's mounts, as the sites server-side loaders read them
 * (services/catalyst/src/serve.rs nests explore at /places and /events): `/places/api/*` and
 * `/events/api/*` are the upstream `/api/*`. `/events/v1/events` is the node's list with the
 * envelope of the upstream `/api/events` (apps/sites/src/data/lib/catalyst/places/events.ts),
 * which production serves under that path only.
 */
const DISCOVERY_MOUNTS: [RegExp, Service, string][] = [
  [/^\/places\/api\//, "places", "/api/"],
  [/^\/events\/api\//, "events", "/api/"],
  [/^\/events\/v1\/events(?=\/|$)/, "events", "/api/events"],
];

/** The node's own telemetry (`/telemetry/*`): answered empty like the reference's Segment, never recorded. */
export const NODE_TELEMETRY = /^\/telemetry(\/|$)/;

/** A request the dcl-one side makes to its own origin (or that sites makes to the node). */
export function fromNodePath(method: string, path: string): { service: Service; path: string } {
  if (path.startsWith(`${SHOP_API_PREFIX}/`)) {
    const r = fromShopApi(method, path.slice(SHOP_API_PREFIX.length));
    if (r) return r;
  }
  for (const [re, service] of DISCOVERY_API) if (re.test(path)) return { service, path };
  for (const [re, service, to] of DISCOVERY_MOUNTS) if (re.test(path)) return { service, path: path.replace(re, to) };
  for (const [prefix, service] of UPSTREAM_PREFIXES) {
    if (path.startsWith(`${prefix}/`)) return { service, path: path.slice(prefix.length) };
  }
  // The node's archipelago answers hot-scenes in the shape the realm provider serves it.
  if (path === "/archipelago-ea-stats/hot-scenes") return { service: "realm-provider", path: "/hot-scenes" };
  // The node's realm description, which the catalyst serves at the same path.
  if (PEER_PREFIXES.some((prefix) => path.startsWith(`${prefix}/`)) || path === "/about") return { service: "peer", path };
  // The node's live scenes, which the catalyst serves at the realm provider's path and shape (comms archipelago).
  if (path === "/hot-scenes") return { service: "realm-provider", path };
  return { service: "node", path };
}

/**
 * Classify a browser request of either side; `origin` is that side's own server. The reference's
 * own `/api/*` is what the sites SPA reads from decentraland.org (Vercel functions its static
 * build has none of), so it is the `sites` service; the dcl-one side's own `/api/*` goes on to the
 * sites preview, which relays it to the replay (fromNodePath).
 */
export function resolve(url: URL, method: string, origin: string, resourceType?: string, side?: "reference" | "dcl-one"): Resolved | { kind: "own" } {
  if (url.origin === origin) {
    if (isContentImage(url, resourceType) && OWN_IMAGE_MOUNTS.test(url.pathname)) return { kind: "image" };
    if (side === "reference" && /^\/api\//.test(url.pathname)) return { kind: "api", service: "sites", path: url.pathname };
    return { kind: "own" };
  }
  if (url.protocol === "data:" || url.protocol === "blob:") return { kind: "own" };
  if (BLOCKED.some((re) => re.test(url.hostname))) return { kind: "blocked" };
  if (url.host === "cdn.jsdelivr.net" && TWEMOJI.test(url.pathname)) return { kind: "api", service: "twemoji", path: url.pathname };
  const service = HOSTS.get(url.host);
  if (service === "fonts" || service === "fonts-static") return { kind: "api", service, path: url.pathname };
  if (isContentImage(url, resourceType)) return { kind: "image" };
  if (service) return { kind: "api", service, path: url.pathname };
  return { kind: "unknown", host: url.host };
}

/** The canonical query: parsed and re-serialized in key/value order, so encodings agree. */
export function canonicalQuery(search: string): string {
  const entries = [...new URLSearchParams(search)];
  entries.sort(([a, av], [b, bv]) => (a < b ? -1 : a > b ? 1 : av < bv ? -1 : av > bv ? 1 : 0));
  return new URLSearchParams(entries).toString();
}

/**
 * A cms-api query: cms-server answers in en-US when a read names no locale, and decentraland.org's reads name
 * none, so a read that names `locale=en-US` (apps/sites' blog reader) keys as the read that names none.
 */
export function cmsQuery(search: string): string {
  const params = new URLSearchParams(search);
  if (params.get("locale") === "en-US") params.delete("locale");
  return canonicalQuery(params.toString());
}
