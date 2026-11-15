// The base domain every HUD backend host is composed from:
//   1. the ?baseDomain= entry param (set by hand on web, injected by the native shell from
//      --base-domain; non-decentraland values are stopped by the UntrustedLaunchGate)
//   2. else derived from the hosting origin — the staging deployment at decentraland.zone/bevy-web
//      keys to zone backends, prod to org
//   3. else decentraland.org (localhost dev, the native CEF page, everything else)
// Captured once from the entry URL. This module is the HUD's single source; the ENGINE takes the
// resolved value as an ordinary engine_run option (EngineHost puts `baseDomain` in the boot
// config, the same route as every other launch param — the wasm never reads it from the page),
// and echoes it back into the url like the rest (boot.js drops it when it is the derived default).
//
// On top of it, per-service urls (serviceUrl): a ?<service>= entry param — a FULL base url, the
// web form of the native --<service> flags — else composed from the base domain by the
// convention in the engine's service table (crates/system_api_types/src/services.rs, generated
// into engine/generated/serviceTable.ts). The explicit overrides go to the engine the same way
// (SERVICE_OVERRIDES, spread into the boot config); window.__serviceUrl(name) is for the engine
// loader (deploy/web/engine/boot.js), which composes the default realm / worlds prefix for its
// url sync exactly as the HUD does.

import { SERVICES, type ServiceDef } from '@explorer-core/engine/generated'

// Decentraland's own deployments — the hosting origins we derive from, and the only domains a
// LINK may point the session at without the UntrustedLaunchGate (lib/launchGate.ts).
const TRUSTED_BASE_DOMAINS = ['decentraland.org', 'decentraland.zone']

/** The apex deployment domain a hosting origin implies, or null for unrecognised hosts. */
export function hostBaseDomain(hostname: string): string | null {
  for (const apex of TRUSTED_BASE_DOMAINS) {
    if (hostname === apex || hostname.endsWith(`.${apex}`)) return apex
  }
  return null
}

/**
 * The ?baseDomain= value the engine will accept (crates/common/src/base_domain.rs `set`):
 * lowercased, bare ascii labels, at least one dot. Anything else is null so the HUD and the
 * engine fall back to the same derived default instead of splitting across two domains.
 */
export function normaliseBaseDomain(raw: string | null): string | null {
  const d = raw?.trim().toLowerCase() ?? ''
  return /^[a-z0-9-]+(\.[a-z0-9-]+)+$/.test(d) ? d : null
}

/**
 * dcl-one serves the HUD from the deployment itself: ONE host whose node answers every service
 * under a path prefix (services/catalyst/tests/conformance/routes.tsv). A page not served from a
 * decentraland.* host composes its service urls from that table on its own origin instead of
 * from subdomains of a base domain; the hosting host is the "base domain" such a page reports,
 * and `?baseDomain=decentraland.org` still selects the upstream deployments.
 */
export const ONE_GATEWAY_HOST = window.location.host
// Sites marks its documents with window.__DCL_PUBLIC__ (routes/root.tsx); a page without it (the
// upstream test harness, a standalone build) keeps the upstream base-domain convention.
const ON_SITES = typeof (window as { __DCL_PUBLIC__?: unknown }).__DCL_PUBLIC__ === 'object'
const GATEWAY_PREFIXES: Readonly<Record<string, string>> = {
  'realm-provider-ea': '/realm-provider-ea',
  peer: '',
  'worlds-content-server': '/worlds-content-server',
  places: '/places',
  'comms-gatekeeper': '/comms-gatekeeper',
  'comms-gatekeeper-local': '/comms-gatekeeper',
  'auth-api': '/auth-api',
  storage: '/world-storage',
  rpc: '/rpc',
  'rpc-social-service-ea': '/social-rpc',
  'social-api': '/social-api',
  opensea: '/opensea',
  // the site itself: Reels' share page
  '': '',
  api: '/api',
  'pulse-server': '',
  'marketplace-api': '/marketplace-api',
  events: '/events',
  'autotranslate-server': '/autotranslate-server',
  'camera-reel-service': '/camera-reel-service',
  badges: '/badges',
  notifications: '/notifications',
  'assets-cdn': '/assets-cdn'
}
// Services the gateway cannot serve yet, reached on Decentraland's public host instead: abgen's
// CDN (Bevy variants), whose `/ab-cdn` route serves the asset-bundle CDN and refuses `bevy/` paths
// (decentraland/dcl-one#631). Behind the gateway they compose from decentraland.org, never from
// the page's own host, as the engine does (`Gateway::segment` → its default base domain).
const NOT_THROUGH_GATEWAY: ReadonlySet<string> = new Set(['abgen-cdn'])

/** The domain a url without ?baseDomain= means here: derived from the hosting origin, else this host. */
export const DEFAULT_BASE_DOMAIN =
  hostBaseDomain(window.location.hostname) ?? (ON_SITES ? ONE_GATEWAY_HOST : 'decentraland.org')

/**
 * A page of the web site around the Explorer (shop, terms, help): the same origin on dcl-one,
 * the apex of the base domain upstream.
 */
export function siteUrl(path: string): string {
  return BASE_DOMAIN === ONE_GATEWAY_HOST ? `${window.location.origin}${path}` : `https://${BASE_DOMAIN}${path}`
}

export const BASE_DOMAIN =
  normaliseBaseDomain(new URLSearchParams(window.location.search).get('baseDomain')) ?? DEFAULT_BASE_DOMAIN

declare global {
  interface Window {
    __defaultBaseDomain?: () => string
    __defaultRealm?: () => string
    __serviceUrl?: (name: string) => string
  }
}
window.__defaultBaseDomain = () => DEFAULT_BASE_DOMAIN

/** A service table row by web param name. Throws so a renamed service fails at module load. */
function service(name: string): ServiceDef {
  const s = SERVICES.find((s) => s.name === name)
  if (s == null) throw new Error(`serviceUrl: '${name}' is not in the engine's service table`)
  return s
}

/**
 * The ?<service>= value the engine will accept (crates/common/src/base_domain.rs
 * `set_services`), in the service's shape: a full http(s) — ws(s) for the websocket services —
 * base url with no query or fragment, trailing slash dropped; or, for an authority service
 * (pulse), `host` or `host:port` and nothing else. Anything else is null so the HUD and the
 * engine fall back to the same composed default rather than splitting. Recomposed from the
 * parsed parts rather than serialised: a bare trailing `?` or `#` is empty to `search`/`hash`
 * but a query/fragment to the engine's parser, which would refuse it and fail the launch.
 */
export function normaliseServiceUrl(s: ServiceDef, raw: string | null): string | null {
  if (raw == null) return null
  try {
    if (s.value === 'authority') {
      const a = raw.trim()
      if (a === '' || /[/?#@]/.test(a)) return null
      // behind a non-special scheme, so a default-looking port is kept as given
      const u = new URL(`dummy://${a}`)
      return u.hostname === '' ? null : u.host.toLowerCase()
    }
    const u = new URL(raw.trim())
    const ok = s.value === 'websocket' ? /^wss?:$/ : /^https?:$/
    if (!ok.test(u.protocol) || u.search !== '' || u.hash !== '') return null
    return `${u.protocol}//${u.host}${u.pathname}`.replace(/\/+$/, '')
  } catch {
    return null
  }
}

/** The explicit overrides in the ENTRY url, by service name — also the engine's, via the boot config. */
export const SERVICE_OVERRIDES: Readonly<Record<string, string>> = {}
{
  const q = new URLSearchParams(window.location.search)
  for (const s of SERVICES) {
    const url = normaliseServiceUrl(s, q.get(s.name))
    if (url != null) (SERVICE_OVERRIDES as Record<string, string>)[s.name] = url
  }
}

/**
 * A service's base url (an authority service: its host) by its table name: the entry url's
 * override, else composed from the base domain, e.g. serviceUrl('places') →
 * "https://places.decentraland.org".
 */
export function serviceUrl(name: string): string {
  return SERVICE_OVERRIDES[name] ?? derivedServiceUrl(name)
}

/**
 * The url this deployment composes for a service on its own, ignoring the entry url's override:
 * what the Explorer uses when the link says nothing. The engine writes its effective options back
 * into the address bar (deploy/web/engine/boot.js set_url_params), so a reloaded or copied link
 * carries every service explicitly; one that merely repeats this value changes nothing.
 */
export function derivedServiceUrl(name: string): string {
  const s = service(name)
  if (BASE_DOMAIN === ONE_GATEWAY_HOST && !NOT_THROUGH_GATEWAY.has(s.sub)) {
    const prefix = GATEWAY_PREFIXES[s.sub]
    if (prefix == null) throw new Error(`serviceUrl: '${name}' (${s.sub}) has no gateway prefix`)
    if (s.value === 'authority') return ONE_GATEWAY_HOST
    const secure = window.location.protocol === 'https:'
    const scheme = s.value === 'websocket' ? (secure ? 'wss' : 'ws') : secure ? 'https' : 'http'
    return `${scheme}://${ONE_GATEWAY_HOST}${prefix}${s.path}`
  }
  // on the gateway host no ?baseDomain= was given, so an ungated service takes the upstream default
  const domain =
    BASE_DOMAIN === ONE_GATEWAY_HOST && NOT_THROUGH_GATEWAY.has(s.sub) ? TRUSTED_BASE_DOMAINS[0] : BASE_DOMAIN
  const host = s.sub === '' ? domain : `${s.sub}.${domain}`
  return s.value === 'authority' ? host : `${s.scheme}://${host}${s.path}`
}
window.__serviceUrl = serviceUrl

/**
 * The service options the ENGINE boots with (EngineHost's boot config). Upstream the engine composes
 * every service from a bare base domain; behind the dcl-one gateway the services live under path
 * prefixes of this one origin, so the engine gets each one's gateway url explicitly instead, and a
 * base domain only when it is one the engine accepts (a dev host like 127.0.0.1:5222 is not). The
 * entry url's own overrides win either way.
 */
export function engineServiceOptions(): Record<string, string> {
  const options: Record<string, string> = {}
  if (BASE_DOMAIN === ONE_GATEWAY_HOST) {
    for (const s of SERVICES) options[s.name] = serviceUrl(s.name)
  }
  Object.assign(options, SERVICE_OVERRIDES)
  const bare = normaliseBaseDomain(BASE_DOMAIN)
  if (bare != null) options.baseDomain = bare
  return options
}

/**
 * The main (Genesis City) realm: the realm provider's `/main`, composed exactly as the engine's
 * common::structs::default_home_realm. Parcel launches pass it EXPLICITLY so a ?realm override —
 * possibly an invalid world — never leaks into a Places pick (always a Genesis coordinate).
 */
export const DEFAULT_REALM = `${serviceUrl('realmProvider')}/main`
window.__defaultRealm = () => DEFAULT_REALM

// Any other domain arriving by LINK points the whole session's backends — sign-in, content,
// comms — at someone else's servers, so App.tsx stops on it with the UntrustedLaunchGate before
// anything boots (lib/launchGate.ts, which also exempts native: there the shell injects the
// user's own --base-domain flag).
export function isTrustedBaseDomain(domain: string): boolean {
  return domain === DEFAULT_BASE_DOMAIN || TRUSTED_BASE_DOMAINS.includes(domain)
}
