# Asset-bundle registry: content-related endpoints

- Area: every route of the asset-bundle registry (ABR), split into what a Catalyst replacement must
  answer from its own content (profiles, the profile metadata projection, world manifests, the
  denylist read, and the content fields of `entities/active` and `entities/status`) and what stays
  asset-bundle state or ABR administration (bundle and version state, build queues, re-ingest, cache
  flush, denylist writes).
- Upstream ABR: `decentraland/asset-bundle-registry` @ `f9578d4ab1ab9fdff4576ba6d052b966bd0fd5e9`
  (2026-10-21). Production served `/status` with that `commitHash` and `version` 2.5.3 on 2026-11-05
  ([`captures/abr/AB-15-status.json`](captures/abr/AB-15-status.json)); the zone registry served
  `d932a02eaa90345f846b068f5c8c2b9ab388deaa`, `version` `Unknown`
  ([`captures/abr/AB-15-zone-status.json`](captures/abr/AB-15-zone-status.json)).
- Upstream content server (it consumes the ABR denylist): `decentraland/catalyst` @
  `a416565db1e705cff9523540a2578af642598a29` (2026-10-21).
- Upstream lambdas (production owner of `/lambdas/profiles/:id`): `decentraland/lamb2` @
  `02a37f5d88fb895339c20e84c2470be68cdf3f57` (2026-10-21).
- dcl-one: `main` @ `534b862c` (2026-11-13), workspace `services/catalyst`.
- Method: static reading, plus the captures under [`captures/abr/`](captures/abr/) taken on 2026-11-05
  from `peer.decentraland.org`, `peer.decentraland.zone`, `asset-bundle-registry.decentraland.org` and
  `asset-bundle-registry.decentraland.zone` (format and sanitization in
  [`captures/README.md`](captures/README.md)). A row citing a capture states what that service
  answered; every other row was read. Nothing in this tree was built or run.
- ABR behaviour that lives in npm packages missing from the clone (`@dcl/http-server` routing, 404,
  405 and body limits; `@dcl/http-commons` `errorHandler` bodies; `@dcl/crypto-middleware`) is taken
  from the captures; rows that no capture covers say "not vendored".

Files read (ABR): `src/controllers/routes.ts`, `src/controllers/handlers/{get-service-status,get-active-entities,get-entity-versions,get-entity-status,get-world-manifest,get-profiles,get-profiles-metadata,get-denylist,post-denylist-entry}.ts`,
`src/controllers/schemas/{profiles,worlds}.ts`, `src/logic/profile-retriever.ts`, `src/logic/sync/{profile-sanitizer,ownership-validator-job}.ts`, `src/logic/registry-parser.ts`,
`src/logic/coordinates/component.ts`, `src/utils/{webgl-compat,pointers}.ts`, `src/adapters/{db,catalyst}.ts`, `src/types/types.ts`, `src/migrations/1763329911778_profiles.ts`, `.env.default`.
Upstream catalyst: `src/Environment.ts`, `src/adapters/denylist/component.ts`, `src/components.ts`, `src/controllers/handlers/get-content-handler.ts`, `src/controllers/utils.ts`.

Files read (dcl-one): `explore/src/api/{registry,mod,denylist}.rs`, `config/src/{posture,explore,ops}.rs`, `content/src/{denylist,queries,active_entities}.rs`,
`shop/src/market/{mod,profiles,lambdas_http}.rs`, `shared/src/http_util.rs`, `worlds/src/{lib,queries}.rs`, `src/serve.rs`, `../../infra/gateway/{root-aliases.nix,gateway.py}`.

## 1. Which production URLs reach ABR

None of the Catalyst URLs probed on 2026-11-05 is redirected (no 3xx, no `Location`) or proxied to
ABR, on the production peer or on the zone peer. Each answer came from lamb2, the content server or
the gateway in front of them, never with an ABR-shaped body:

| request | production and zone answer | layer | capture |
|---|---|---|---|
| `POST /lambdas/profiles/metadata` | `405`, empty body, `Allow: HEAD, GET` (lamb2's `GET /profiles/:id`) | lamb2 | `AB-14-{peer,zone}-post-lambdas-profiles-metadata.json` |
| `GET /lambdas/profiles/metadata` | `404 {"error":"Not Found","message":"Profile not found"}` (a profile id `metadata`) | lamb2 | `AB-14-{peer,zone}-get-lambdas-profiles-metadata.json` |
| `OPTIONS /lambdas/profiles/metadata` | `204`, `access-control-allow-origin: *`, requested headers reflected, max-age 36000, no `allow-methods` (not ABR's 86400); no application trace header | gateway | `AB-14-{peer,zone}-preflight-lambdas-profiles-metadata.json` |
| `POST /profiles/metadata`, `POST /profiles`, `POST /entities/versions`, `GET /entities/status/0,0`, `GET /worlds/<name>/manifest`, `GET /denylist`, `GET /queues/status`, `GET /asset-bundle-registry/status` (root paths) | `404 {"ok":false,"error":"404 path not found"}`, `application/octet-stream` | gateway | `AB-14-{peer,zone}-*-root-*.json`, `AB-14-{peer,zone}-get-abr-prefix-status.json` |
| `POST /content/entities/versions` | `405`, `Allow: HEAD, GET` (content's `GET /entities/:type`) | content server | `AB-14-{peer,zone}-post-content-entities-versions.json` |
| `GET /content/entities/versions` | `400 {"error":"Unrecognized type: versions"}` | content server | `AB-14-{peer,zone}-get-content-entities-versions.json` |
| `GET /content/entities/status/0,0`, `GET /content/worlds/<name>/manifest`, `GET /content/denylist` | `404 Not found` (`text/plain`) | content server | `AB-14-{peer,zone}-get-content-*.json` |
| `POST /lambdas/entities/versions` | nginx `404` HTML | gateway | `AB-14-{peer,zone}-post-lambdas-entities-versions.json` |
| `POST /content/entities/active {"pointers":["0,0"]}` | the content server's entity (keys `id, type, pointers, content, version, timestamp, metadata` on the peer), no `deployer`, `status`, `bundles` or `versions` | content server | `AB-4-peer-content-entities-active.json`, `AB-4-zone-content-entities-active.json` |

The one production dependency of a Catalyst on ABR is the denylist: the content server reads
`https://asset-bundle-registry.decentraland.org/denylist` by default
(`decentraland/catalyst:src/Environment.ts:13`, `:513-514`) at boot and every 120 s
(`decentraland/catalyst:src/components.ts:134-137`), and drops every listed `entity_id` from its reads
(`decentraland/catalyst:src/adapters/denylist/component.ts:59-86`). On production, the first entry of
ABR's list answers `404` to `GET /content/contents/<id>` with `If-None-Match: *`, where a hash that is
not listed answers `304`; the denylist check runs before the `If-None-Match` check
(`decentraland/catalyst:src/controllers/handlers/get-content-handler.ts:22-24`,
`decentraland/catalyst:src/controllers/utils.ts:102-104`)
([`AB-10-peer-denylisted-hash.json`](captures/abr/AB-10-peer-denylisted-hash.json),
[`AB-10-peer-valid-hash.json`](captures/abr/AB-10-peer-valid-hash.json)), and
`POST /content/entities/active {"ids":[<that entity id>]}` answers `200 []`
([`AB-10-peer-active-by-denylisted-id.json`](captures/abr/AB-10-peer-active-by-denylisted-id.json)).
ABR still reports that entity as converted ([`AB-6-denylisted-id.json`](captures/abr/AB-6-denylisted-id.json)).
The zone peer answered `404` to both content requests, so the zone result does not show whether it
applies the list ([`AB-10-zone-*.json`](captures/abr/)). dcl-one reads it on a mirror (AB-10).

Clients call ABR at its own host (`asset-bundle-registry.<domain>`); in dcl-one that host and the
`/asset-bundle-registry` prefix reach two routers (`docs/adr/2026-11-09-registry-local-catalyst.md`):
the profiles, served from the node's own profiles (`services/catalyst/shop/src/market/profiles/registry.rs`,
mounted by `registry_profiles` in `services/catalyst/src/serve.rs`), and the Registry gateway
(`services/catalyst/explore/src/api/registry.rs`, nested by `router` in `services/catalyst/explore/src/api/mod.rs`).
The gateway's reads go, without a cache, to `EXPLORER_REGISTRY_SOURCE_URL`: a loopback URL is the
local abgen runtime with native Registry state, any other a public Registry (Decentraland's under
`NODE_POSTURE=mirror`: the `EXPLORER_REGISTRY_SOURCE_URL` row in `services/catalyst/config/src/posture.rs`), and an empty one (`island`)
answers the entity reads `[]`, `/denylist` with local rows, and the other reads `503`.

## 2. Endpoint inventory

| method | ABR path | class | production URL prefix | dcl-one route(s) | status |
|---|---|---|---|---|---|
| GET | `/status` | service status | `asset-bundle-registry.<domain>` | `services/catalyst/explore/src/api/registry.rs` | MATCH |
| POST | `/entities/active` | mixed: content fields + AB state | ABR host | `services/catalyst/explore/src/api/registry.rs`: ABR's pointer rule, then the backend's answer without the node's denylisted entities | GAP (AB-4) |
| POST | `/entities/versions` | AB state (only `pointers` is content) | ABR host | `services/catalyst/explore/src/api/registry.rs`: ABR's pointer rule, then the backend's answer | MATCH (body is the backend's) |
| GET | `/entities/status/:id` | mixed: `entityId`, `catalyst` + AB state | ABR host | `services/catalyst/explore/src/api/registry.rs`, forwarded | GAP (AB-6) |
| GET | `/entities/status` (signed fetch) | mixed: per deployer | ABR host | `services/catalyst/explore/src/api/registry.rs`, forwarded (identity headers to the local runtime only) | GAP (AB-7) |
| GET | `/queues/status` | AB only | ABR host | `services/catalyst/explore/src/api/registry.rs`, forwarded | out of parity scope |
| GET | `/worlds/:worldName/manifest` | content | ABR host | `services/catalyst/explore/src/api/registry.rs`, forwarded | GAP (AB-8) |
| POST | `/profiles` | content | ABR host | `services/catalyst/shop/src/market/profiles/registry.rs`, the node's profiles | DIVERGENT (AB-2) |
| POST | `/profiles/metadata` | content | ABR host | `services/catalyst/shop/src/market/profiles/registry.rs`, the node's profiles; `/lambdas/profiles/metadata` serves the same projection (the `/lambdas/profiles/metadata` route and its `DefaultBodyLimit` layer in `services/catalyst/shop/src/market/mod.rs`) | DIVERGENT (AB-1) |
| GET | `/denylist` | content (moderation list) | ABR host | `services/catalyst/explore/src/api/registry.rs`, the Registry source's list merged with the node's `content.denylist` rows; the gateway's root `/denylist` alias points there (the `^~ /denylist` alias in `infra/gateway/root-aliases.nix`) | MATCH (AB-9) |
| POST | `/denylist/:entityId` (signed fetch, moderators) | ABR admin | ABR host | none; the operator console reaches abgen (`denylist_add` in `services/catalyst/src/ops/admin/create.rs`) and edits `content.denylist` (`denylist_add` in `services/catalyst/src/ops/admin/content.rs`) | MISSING (out of parity scope) |
| DELETE | `/denylist/:entityId` (signed fetch, moderators) | ABR admin | ABR host | none; console `denylist_remove` in `services/catalyst/src/ops/admin/create.rs` | MISSING (out of parity scope) |
| POST | `/registry` (bearer) | ABR admin | ABR host | none; the console calls the external abgen runtime (`AdminConfig.abgen_admin_url` (`ABGEN_ADMIN_URL`) in `services/catalyst/config/src/ops.rs`) | MISSING (out of parity scope) |
| DELETE | `/flush-cache` (bearer) | ABR admin | ABR host | none; same console bridge | MISSING (out of parity scope) |

"forwarded": the gateway passes the method, raw query, body and content type to the Registry source
and answers its status and body (`services/catalyst/explore/src/api/registry.rs`). With a public Registry the answer is
Decentraland's; with the local runtime, its native state; without a source,
`503 {"message":"Registry service is not configured","ok":false}`.

Counts: 14 endpoints. MATCH 3, GAP 4, DIVERGENT 2, MISSING 4 (the ABR admin routes, outside catalyst
parity), and `/queues/status` forwarded, outside parity scope. Content-related: 7 (`entities/active`,
`entities/status/:id`, `entities/status`, `worlds/:worldName/manifest`, `profiles`,
`profiles/metadata`, `GET /denylist`).

Upstream routes: `decentraland/asset-bundle-registry:src/controllers/routes.ts:34-58`. `/profiles` and
`/profiles/metadata` are registered only when `DISABLE_PROFILES` is not `true`
(`decentraland/asset-bundle-registry:src/controllers/routes.ts:44-48`; `.env.default` sets `false`).
`/registry` and `/flush-cache` exist only with `API_ADMIN_TOKEN` (`:54-58`).

### Field classification of the mixed routes

| route | content fields (parity target) | asset-bundle fields (stay with ABR / the read-through) |
|---|---|---|
| `POST /entities/active` | `id`, `type`, `timestamp`, `pointers`, `content`, `metadata`, `deployer` (from the deployment's auth chain) | `status` (`complete`/`fallback` only are served), `bundles.{assets,lods}.{mac,windows,webgl}`, `versions.assets.{mac,windows,webgl}.{version,buildDate}` |
| `POST /entities/versions` | `pointers` | `versions`, `bundles`, `status` |
| `GET /entities/status/:id` | `entityId`; `catalyst` (always `"complete"` when a registry exists); the id/pointer resolution (current id, historical id, or newest entity on the pointer, `world_name` aware) | `complete`, `assetBundles.{mac,windows}`, `lods.{mac,windows}` |
| `GET /entities/status` | the deployer's entity ids (current and historical) | per-entity AB state as above |
| `GET /worlds/:worldName/manifest` | `occupied`, `spawn_coordinate`, `total` | only the filter: ABR counts parcels of registries in `complete`/`fallback` (converted) state |

### Cross-cutting behaviour of the ABR host

| aspect | ABR | dcl-one | verdict |
|---|---|---|---|
| 400 from `InvalidRequestError` | `{"error":"Bad request","message":<msg>}` ([`AB-3-missing-ids.json`](captures/abr/AB-3-missing-ids.json)) | the same on both profile routes (`services/catalyst/shop/src/market/profiles/registry.rs`) | MATCH |
| handler 400/404 | `{"ok":false,"message":<msg>}` (`decentraland/asset-bundle-registry:src/controllers/handlers/get-active-entities.ts:13-21`) | the boot reads' `No pointers provided` with ABR's bytes (`services/catalyst/explore/src/api/registry.rs`); a forwarded read's body is the backend's | MATCH |
| unhandled error (unparseable JSON, empty body; on the boot reads also a JSON `null` body) | `500 {"error":"Internal Server Error"}` ([`AB-3-not-json.json`](captures/abr/AB-3-not-json.json), [`AB-3-empty-body.json`](captures/abr/AB-3-empty-body.json)) | the same on the profile routes and the boot reads, which read raw bytes (`services/catalyst/explore/src/api/registry.rs`) | MATCH |
| `Content-Type` | ignored: `text/plain` with a JSON body answers `200` ([`AB-3-text-plain.json`](captures/abr/AB-3-text-plain.json)) | ignored by the profile routes and the boot reads; a forwarded read passes it to the backend | MATCH |
| unknown route | `404 Not found` `text/plain; charset=utf-8` ([`AB-12-unknown-route.json`](captures/abr/AB-12-unknown-route.json)) | the gateway half's fallback (`services/catalyst/explore/src/api/registry.rs`): `404` `text/plain; charset=utf-8` `Not found`, inside the Registry's CORS | MATCH |
| wrong method | `405`, empty body, `Allow: POST` ([`AB-1-get.json`](captures/abr/AB-1-get.json), [`AB-1-put.json`](captures/abr/AB-1-put.json)) | axum `405`, empty body, `Allow: POST` | MATCH |
| trailing slash | `POST /profiles/metadata/` answers as without it ([`AB-1-trailing-slash.json`](captures/abr/AB-1-trailing-slash.json)) | trimmed for the shared Registry routes (`NormalizePath::trim_trailing_slash` in `mount_services`, `services/catalyst/comms/src/stats/http.rs`, called by `serve`) | MATCH |
| CORS preflight | `204`, `allow-origin: *`, `allow-methods: GET,HEAD,OPTIONS,DELETE,POST,PUT`, `allow-headers` reflected, `max-age: 86400`, `vary: Access-Control-Request-Headers` ([`AB-12-preflight.json`](captures/abr/AB-12-preflight.json)) | `registry::cors` (`services/catalyst/explore/src/api/registry.rs`) on both halves of the prefix and on the gateway half's fallback (`registry.rs`, `services/catalyst/shop/src/market/profiles/registry.rs`; `router` in `services/catalyst/explore/src/api/mod.rs` nests the Registry outside the gateway's permissive CORS): every `OPTIONS` under the prefix, routed or not, is `204` with the captured headers; other responses get `allow-origin: *` only when the request has an `Origin`, and no exposed headers. axum also adds `Allow` to a preflight on a route without an `OPTIONS` handler | MATCH |
| backend failure | n/a | `502 {"message":"Registry service unavailable","ok":false}`; no free backend slot within 2 s: `503` with `Retry-After: 1` (`services/catalyst/explore/src/api/registry.rs`) | dcl-one only |
| no source configured | n/a | the boot reads answer `200 []` after the pointer rule, the other forwarded reads `503` (`services/catalyst/explore/src/api/registry.rs`); `EXPLORER_REGISTRY_SOURCE_URL` is set by `NODE_POSTURE=mirror` (the `EXPLORER_REGISTRY_SOURCE_URL` row in `services/catalyst/config/src/posture.rs`) or by the operator | dcl-one only |
| rate limits | none in ABR | gateway limit by IP (README "Rate limits") | as the other areas |
| request body size | no limit in the handlers | `entities/active` and `entities/versions` take at most 1 MiB (`services/catalyst/explore/src/api/registry.rs`); a larger body is axum's `413` before the pointer rule. unity-explorer's largest read, every occupied parcel of a world at the coordinate bound, is 865,088 bytes | dcl-one only |

---

## 3. GET /status

**Request:** none.

| trigger | status | body | headers | upstream | dcl-one | verdict |
|---|---|---|---|---|---|---|
| always | 200 | `{"data":{"version":"2.5.3","currentTime":1793883010167,"commitHash":"f9578d4a…"}}` ([`AB-15-status.json`](captures/abr/AB-15-status.json)); `version`/`commitHash` default to `""` | `content-type: application/json` | `decentraland/asset-bundle-registry:src/controllers/handlers/get-service-status.ts:11-30` | same keys and order, the node's `version` and `commit_hash` `services/catalyst/explore/src/api/registry.rs` | MATCH (values are the node's) |

**Data dependency:** none.

## 4. POST /entities/active

**Request:** body `{"pointers": string[]}`; `ids` is not read. Query `world_name` (case-insensitive):
with it only entities whose `metadata.worldConfiguration.name` matches; without it world entities are
excluded. No cap on the number of pointers, no auth. Pointers are lowercased
(`decentraland/asset-bundle-registry:src/adapters/db.ts:62-119`).

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| `pointers` missing, not truthy, an empty array, or with a `length` that is exactly `0` (`{"pointers":{"length":0}}`); a body that is not an object (also `{"ids":[...]}`, and a lone number beyond `f64` such as `1e400`, which `JSON.parse` reads as an infinity) | 400 | `{"ok":false,"message":"No pointers provided"}` ([`AB-4-empty-pointers.json`](captures/abr/AB-4-empty-pointers.json)) | `get-active-entities.ts:13-21` (`!pointers \|\| pointers?.length === 0`) | the same bytes before any backend call, with or without a source (`services/catalyst/explore/src/api/registry.rs`); a repeated key counts its last value, as `JSON.parse` | MATCH |
| body not JSON, empty, or JSON `null` | 500 | `{"error":"Internal Server Error"}` (`request.json()` throws; `null.pointers` throws) | `get-active-entities.ts:10-11` | the same (`registry.rs`) | MATCH |
| body over 1 MiB | no limit | — | — | `413` before the pointer rule (see the cross-cutting table) | dcl-one only |
| more than 1000 pointers | 200 (no cap) | entity array | `get-active-entities.ts:10-48` | no cap on the count within the 1 MiB body; the backend's answer | MATCH |
| `pointers` not an array (e.g. a string) | the string is passed to SQL as an array literal; not vendored | — | `get-active-entities.ts:11` | a truthy value passes the rule and is forwarded; the backend answers | needs capture |
| found | 200 | one element per distinct pointer set, `status` in `complete`/`fallback`, denylisted ids excluded; keys in order `id, type, timestamp, deployer, pointers, content, metadata, status, bundles, versions`; `timestamp` is a JSON **string** (Postgres `bigint`); `webgl` re-added to `bundles` and `versions` (`src/utils/webgl-compat.ts:46-81`). Example: `{"id":"bafkrei…","type":"scene","timestamp":"1791215990965","deployer":"0x…0b02","pointers":["-1,-4",…],"content":[{"file":"main.crdt","hash":"bafkrei…"},…],"metadata":{…},"status":"fallback","bundles":{"assets":{"mac":"complete","windows":"complete","webgl":"pending"},"lods":{"mac":"pending","windows":"pending","webgl":"pending"}},"versions":{"assets":{"mac":{"version":"v49","buildDate":"2026-10-05T16:08:08.914Z"},…}}}` ([`AB-4-pointer-0-0.json`](captures/abr/AB-4-pointer-0-0.json)) | `get-active-entities.ts:29-48`, `src/logic/registry-parser.ts:1-17` | the backend's bytes, uncached; unchanged unless the denylist row below drops an entity (`registry.rs`) | MATCH for entities the backend knows |
| order | 200 | grouped by the whole `pointers` array in database row order; the query has no `ORDER BY` for this route, so the element kept per group is the first row returned | `db.ts:113-115`, `registry-parser.ts:1-17` | the backend's order | MATCH |
| entity deployed to this node, not converted by the backend | 200 | absent (ABR has no registry for it) | — | absent: a public Registry has no registry for an entity only this node holds; the local abgen runtime converts the node's own deployments | GAP (AB-4, owner decision) |
| entity on this node's `content.denylist` (or on a `DENYLIST_URLS` list) | 200 | absent: ABR applies its own list | `db.ts:105-111` | absent: the content reads' projection and rule (`retain_non_denylisted` in `services/catalyst/content/src/queries.rs`) applied to each element's `id` (`services/catalyst/explore/src/api/registry.rs`) | MATCH |
| conditional request (`If-None-Match`, `If-Modified-Since`) | ABR's own caching | — | — | not forwarded, so the backend never answers `304` for an unfiltered body; the backend's `ETag` and `Last-Modified` are kept when the denylist drops nothing and removed when it drops an entity; `Cache-Control` is kept (`registry.rs`) | dcl-one only |

**Data dependency:** content fields: the node's content DB (active entities, `deployer_address`),
which dcl-one holds; the answer takes them from the Registry source with the AB fields. AB fields:
Decentraland's ABR (public source) or the local abgen runtime.

## 5. POST /entities/versions

Same request rules as `entities/active` (`pointers` only, no cap, `world_name`); no denylist filter
(`decentraland/asset-bundle-registry:src/controllers/handlers/get-entity-versions.ts:10-44`).

| trigger | status | body | dcl-one | verdict |
|---|---|---|---|---|
| no pointers (also `{"ids":[...]}`) | 400 | `{"ok":false,"message":"No pointers provided"}` ([`AB-11-missing-pointers.json`](captures/abr/AB-11-missing-pointers.json)) | the same bytes, no cap on the count, the same 1 MiB body limit (`services/catalyst/explore/src/api/registry.rs`) | MATCH |
| found | 200 | `[{"pointers":[…],"versions":{"assets":{"mac":{…},"windows":{…},"webgl":{"version":"","buildDate":""}}},"bundles":{"assets":{…},"lods":{…}},"status":"fallback"}]` ([`AB-11-pointer-0-0.json`](captures/abr/AB-11-pointer-0-0.json)) | the backend's bytes, uncached, the request body as received; no denylist filter, as ABR (the elements carry no entity id) | MATCH (body is the backend's) |

**Data dependency:** AB state only.

## 6. GET /entities/status/:id

**Request:** `:id` is an entity id (current registry, then historical registry, compared lowercased)
or a pointer; query `world_name`. No auth.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| found | 200 | `{"entityId":"bafkrei…","catalyst":"complete","complete":false,"assetBundles":{"mac":"pending","windows":"pending"},"lods":{"mac":"pending","windows":"pending"}}` ([`AB-6-by-pointer.json`](captures/abr/AB-6-by-pointer.json)); a pointer resolves to the newest registry of any status; missing AB values read `pending`; `lods` always present (defaults) | `get-entity-status.ts:5-54` | forwarded (`services/catalyst/explore/src/api/registry.rs`): the backend's answer; `503` without a source | MATCH for entities the backend knows |
| denylisted entity | 200 | answered as any other ([`AB-6-denylisted-id.json`](captures/abr/AB-6-denylisted-id.json)) | `db.ts:121-130` (no denylist filter) | forwarded | MATCH for entities the backend knows |
| not found | 404 | `{"ok":false,"message":"No active entity found for the provided id or pointer"}` ([`AB-6-unknown-id.json`](captures/abr/AB-6-unknown-id.json)) | `get-entity-status.ts:56-65` | the backend's `404`; `503` without a source | MATCH with a source |
| entity only this node holds | 200 | the status object | — | a public Registry's `404`; `503` without a source | GAP (AB-6) |

The body is built with `JSON.stringify` and sent as a string with `Content-Type: application/json`.

**Data dependency:** content DB (entity by id, overwritten entities, newest entity on a pointer, worlds
by name). dcl-one holds all of it; the AB fields come from the read-through or default to `pending`.

## 7. GET /entities/status (signed fetch)

**Request:** ADR-44 signed fetch, required; scene-signed requests (`signer: decentraland-kernel-scene`)
are refused (`decentraland/asset-bundle-registry:src/controllers/routes.ts:22-32`).

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| unsigned | 400 | `{"error":"Invalid Auth Chain","message":"This endpoint requires a signed fetch request. See ADR-44."}` ([`AB-7-unsigned.json`](captures/abr/AB-7-unsigned.json)) | `routes.ts:22-32` | forwarded (`services/catalyst/explore/src/api/registry.rs`): the backend's 400 | MATCH with a source |
| signer not an address | 400 | `{"ok":false,"message":"Invalid user address"}` | `get-entity-status.ts:78-84` | forwarded to the local runtime with the `x-identity-*` headers and the received path | MATCH with the local runtime |
| signed | 200 | array of the entity-status object above for every registry, then every historical registry, whose `deployer` is the signer (lowercased) | `get-entity-status.ts:86-100` | the local runtime's answer; a public Registry receives no identity headers and answers its unsigned 400 | GAP (AB-7) |

**Data dependency:** content DB by deployer (`deployer_address`), including overwritten deployments.

## 8. GET /worlds/:worldName/manifest

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| name not matching `^[a-zA-Z0-9-]+(\.dcl)?\.eth$` | 400 | `{"ok":false,"message":"A valid world name is required"}`, no `Content-Type` set by the handler (the captured answer is `application/json`) ([`AB-8-invalid-name.json`](captures/abr/AB-8-invalid-name.json)) | `get-world-manifest.ts:24-46`, `schemas/worlds.ts:1-3` | forwarded (`services/catalyst/explore/src/api/registry.rs`): the backend's 400; `503` without a source | MATCH with a source |
| unknown or empty world | 200 | `{"occupied":[],"spawn_coordinate":{"x":0,"y":0},"total":0}` ([`AB-8-unknown-world.json`](captures/abr/AB-8-unknown-world.json)) | `coordinates/component.ts:286-288` | forwarded | MATCH with a source |
| world with scenes | 200 | `occupied`: distinct `"x,y"` strings of the world's converted, non-denylisted scenes, unordered (`SELECT DISTINCT unnest(pointers)`); `spawn_coordinate`: the stored spawn (`world_spawn_coordinates`) if inside the bounding box, else the occupied parcel nearest the centroid; `x` and `y` are numbers; `total` = `occupied.length` | `coordinates/component.ts:277-310`, `:57-80`, `db.ts:887-901` | forwarded: the backend's worlds, not this node's worlds store; `503` without a source | GAP (AB-8) |
| database failure | 500 | `{"ok":false,"message":<error message or "Failed to get world manifest">}` | `get-world-manifest.ts:48-57` | the backend's answer; `502` when the backend cannot be read | dcl-one only on failure |

**Data dependency:** the worlds store: scenes and their parcels per world and the world's
`spawn_coordinates` (`services/catalyst/worlds/src/core/store.rs`, `world_manifest`), which dcl-one
holds. `worlds` sits above `explore` in `docs/module-layers.toml`, so a local answer is mounted
from `worlds` or the root crate, not from the gateway.

## 9. POST /profiles

**Request:** as `/profiles/metadata` (section 10): `ids` must be an array of non-empty strings, may be
empty, no cap.

| trigger | status | body | upstream | dcl-one (`services/catalyst/shop/src/market/profiles/registry.rs`) | verdict |
|---|---|---|---|---|---|
| found | 200 | `[{"timestamp":<number>,"avatars":[…]}]`; each avatar keeps the stored keys and order; `avatar.snapshots` is replaced by `{"face256":"<PROFILE_IMAGES_URL>/entities/<entityId>/face.png","body":"…/body.png"}` ([`AB-2-one-known.json`](captures/abr/AB-2-one-known.json)). `timestamp` is a number on a cache hit or a Catalyst fallback and the raw Postgres `bigint` (a string) when the profile was read from ABR's database in that request | `get-profiles.ts:11-22`, `profile-sanitizer.ts:118-152`, `profile-retriever.ts:32-54`, `src/migrations/1763329911778_profiles.ts:10` | the node's processed profiles in request order, keys `timestamp, avatars`, `timestamp` the entity timestamp as a number; every avatar with an `avatar` object gets exactly `{face256, body}` on the node's profile-images base, other deployed snapshot keys dropped (`services/catalyst/shop/src/market/profiles/registry.rs`); avatar keys are serialized in sorted order, not the stored order | MATCH except avatar key order |
| profile deployed to this node | 200 | ABR hydrates a miss from a Catalyst and persists it (`profile-retriever.ts:110-124`) | — | served from the node's entities, `island` included | MATCH |
| profile this node does not hold | 200 | ABR's own copy or its Catalyst fallback | — | the shared content lookup can fill misses from configured content read-through sources; without a source or a found profile, the id is omitted. There is no separate Registry upstream for profiles (`docs/adr/2026-11-09-registry-local-catalyst.md`) | depends on the configured content sources |
| errors | | as section 10 | | as section 10 | DIVERGENT only above 256 KiB |

**Data dependency:** the node's profiles, through the entity lookup and profile processing
`/lambdas/profiles` uses (`services/catalyst/shop/src/market/profiles.rs`).

## 10. POST /profiles/metadata

**Request** (`decentraland/asset-bundle-registry:src/controllers/schemas/profiles.ts:9-29`):
- Body read with `request.json()` regardless of `Content-Type`.
- `ids` must be an array whose every element is a non-empty string; an empty array passes. No upper
  bound ("deliberately not capped", `:5-7`). Body limit: `@dcl/http-server`, not vendored; 1001 ids
  (about 45 KB) answered `200`.
- No auth, no rate limit.
- Ids are lowercased and deduplicated before the lookup (`profile-retriever.ts:77`); not trimmed.

**Lookup** (`decentraland/asset-bundle-registry:src/logic/profile-retriever.ts:75-148`): L1 cache
(lowercased keys), then `profiles` table (`pointer = ANY(lowercased)`), then a Catalyst
`/lambdas/profiles` fallback whose results are persisted; profiles for pointers not asked for are
dropped. Database and Catalyst failures are logged and read as "not found": the answer stays `200`.
Each registry holds its own profiles: the zone registry answered `[]` for the address production
answered in `AB-1-one-known.json` ([`AB-1-zone-one.json`](captures/abr/AB-1-zone-one.json)).

**Projection** (`decentraland/asset-bundle-registry:src/logic/sync/profile-sanitizer.ts:107-116`,
type `src/types/types.ts:35-41`): from `avatars[0]`:

```json
{"pointer":"0x…0b00","hasClaimedName":false,"name":"profile-name-1","nameColor":{"r":1,"g":0.5,"b":0},"thumbnailUrl":"https://profile-images.decentraland.org/entities/<entity-id>/face.png"}
```

- `pointer`: the stored entity pointer (`pointers[0]`), lowercased at ingest.
- `hasClaimedName`, `name`, `nameColor`: copied from the stored avatar. A key whose value is
  `undefined` is dropped by `JSON.stringify`; a stored `null` is sent as `null`.
- `thumbnailUrl`: always `${PROFILE_IMAGES_URL}/entities/<entity id>/face.png`, built from the entity
  id, never from the deployed snapshot.
- Stored profiles are the lamb2-sanitized document (`hasClaimedName` checked against name ownership),
  refreshed by the ownership-validator job every `PROFILES_OWNERSHIP_VALIDATION_INTERVAL_MS` (default
  10 min) (`src/logic/sync/ownership-validator-job.ts:16-24`); `userId`/`ethAddress` are forced to the
  pointer (`profile-sanitizer.ts:48-65`); profiles without avatars are discarded at ingest (`:71-98`).

| trigger | status | body | headers | capture / upstream | dcl-one ABR host (`services/catalyst/shop/src/market/profiles/registry.rs`) | dcl-one `/lambdas/profiles/metadata` (`services/catalyst/shop/src/market/profiles.rs`) |
|---|---|---|---|---|---|---|
| one known id | 200 | one object, keys `pointer, hasClaimedName, name, thumbnailUrl` (`nameColor` absent when unset) | `content-type: application/json` | `AB-1-one-known.json` | own profile; keys in ABR's order (`ProfileMetadata`, `services/catalyst/shop/src/market/profiles.rs`, built at `services/catalyst/shop/src/market/profiles/registry.rs`) | own profile; keys in ABR's order |
| two known ids, warm cache | 200 | request order, both orders kept | | `AB-1-two-known.json`, `AB-1-two-known-reversed.json` | request order (`services/catalyst/shop/src/market/profiles/registry.rs`) | request order |
| order on a cold read | 200 | cache hits first in request order, then database rows in row order (no `ORDER BY`), then Catalyst fallbacks | | `profile-retriever.ts:81-121`, `db.ts:596-613` | request order | request order |
| same address twice, different case | 200 | one element | | `AB-1-case-duplicate.json` | one element (lowercased, deduplicated: `services/catalyst/shop/src/market/profiles/registry.rs`) | one element (lowercased, deduplicated: `services/catalyst/shop/src/market/profiles.rs`) |
| unknown id | 200 | `[]` | | `AB-1-unknown.json` | `[]` | `[]` |
| `ids: []` | 200 | `[]` | | `AB-3-empty-array.json` | `200 []`, no database read: MATCH | `200 []`: MATCH |
| `{}` (no `ids`) | 400 | `{"error":"Bad request","message":"The ids property must be an array of non-empty strings"}` | `application/json` | `AB-3-missing-ids.json` | same status, envelope and message (`services/catalyst/shop/src/market/profiles/registry.rs`): MATCH | same (`services/catalyst/shop/src/market/profiles.rs`): MATCH |
| `ids` not an array | 400 | same as above | | `AB-3-ids-not-array.json` | same 400: MATCH | same 400: MATCH |
| non-string element | 400 | same as above | | `AB-3-non-string-element.json` | same 400: MATCH | same 400: MATCH |
| empty-string element | 400 | same as above | | `AB-3-empty-string-element.json` | same 400, no trimming: MATCH | same 400 (`services/catalyst/shop/src/market/profiles.rs`): MATCH |
| more than 1000 ids | 200 | (1001 unknown ids) `[]` | | `AB-3-1001-ids.json` | no cap: `200 []` for 1001 unknown ids (test `registry_ids_are_not_capped`): MATCH | no cap: `200 []` (test `profiles_metadata_takes_any_number_of_ids_and_answers_bad_json_with_a_bare_500`): MATCH |
| body not JSON | 500 | `{"error":"Internal Server Error"}` | | `AB-3-not-json.json` | the same 500 (`services/catalyst/shop/src/market/profiles/registry.rs`): MATCH | the same 500 (`services/catalyst/shop/src/market/profiles.rs`): MATCH |
| empty body | 500 | `{"error":"Internal Server Error"}` | | `AB-3-empty-body.json` | same 500: MATCH | same 500: MATCH |
| `Content-Type: text/plain` | 200 | answered as JSON | | `AB-3-text-plain.json` | accepted (reads raw bytes): MATCH | accepted (reads raw bytes): MATCH |
| body over the limit | not vendored | | | | `413 {"error":"Payload Too Large","message":"The request body exceeds 262144 bytes"}` above 256 KiB (`services/catalyst/shop/src/market/profiles/registry.rs`) | `413` above 256 KiB (the `/lambdas/profiles/metadata` route and its `DefaultBodyLimit` layer in `services/catalyst/shop/src/market/mod.rs`) |
| `GET` / `PUT` | 405 | empty, `Allow: POST` | | `AB-1-get.json`, `AB-1-put.json` | axum 405, `Allow: POST` | axum 405 `Allow: POST` (production lamb2 answers `GET` with `404 Profile not found`: `AB-14-peer-get-lambdas-profiles-metadata.json`) |
| a lookup layer fails | 200 | what was found | | `profile-retriever.ts:50-53`, `:69-72` | `200` with what was found, logged and counted in `profiles_lookup_failures_total`: only address or `default<n>` ids are looked up, a failed entity read leaves out its 1000 pointers, a failed ownership pass its chunk of 128 entities, never served unvalidated (`services/catalyst/shop/src/market/profiles/registry.rs`): MATCH | `200` with what was found, logged and counted: a failed read leaves out its 1000 pointers (`services/catalyst/shop/src/market/profiles.rs`): MATCH |

**Data dependency:** the node's own profiles (content DB) and the name-ownership reader that sets
`hasClaimedName`; dcl-one holds both for `/lambdas/profiles`.

Local workload anchors: `is_profile_pointer`, `PROFILE_LOOKUP_CHUNK` and
`count_lookup_failure` in `services/catalyst/shop/src/market/profiles.rs`;
`respond` and `project_profiles` in `services/catalyst/shop/src/market/profiles/registry.rs`.
Regression coverage includes `registry_profile_routes_bound_actual_read_through_requests`,
`registry_short_ids_are_empty_reads_but_invalid_tail_is_rejected` and
`registry_failed_ownership_chunk_preserves_successful_neighbours`.

### dcl-one `/lambdas/profiles/metadata` against ABR, field by field

| aspect | ABR | `profile_metadata` (`services/catalyst/shop/src/market/profiles.rs`) | verdict |
|---|---|---|---|
| `pointer` | stored `pointers[0]` | the requested id, lowercased, under which the profile entity was found (`profiles.rs`) | MATCH for address pointers |
| `hasClaimedName` | stored value; key dropped if absent | the processed avatar's value; absent stays absent | MATCH |
| `name` | stored value; key dropped if absent | the processed avatar's value; absent stays absent | MATCH |
| `nameColor` | present when the key exists, `null` kept | present when the key exists, `null` kept | MATCH |
| `thumbnailUrl` | `${PROFILE_IMAGES_URL}/entities/<id>/face.png` | the processed `avatar.snapshots.face256`, which `rewrite_snapshot_urls` always sets to `<PROFILE_CDN_BASE_URL>/entities/<id>/face.png` (`profiles.rs`; a mirror defaults the base to `https://profile-images.decentraland.org`, island and journey to `<PUBLIC_URL>/profile-images`: the `PROFILE_CDN_BASE_URL` row in `services/catalyst/config/src/posture.rs`); an avatar without an `avatar` object gives `""` | MATCH except an avatar without `avatar` |
| key order | `pointer, hasClaimedName, name, nameColor, thumbnailUrl` | the same (struct field order) | MATCH |
| order | see the table above | request order | compatible |
| dedup | case-insensitive | case-insensitive | MATCH |
| validation and errors | see the table above | ABR's rules, envelope and message, no cap on the ids; a 256 KiB body limit | DIVERGENT only above 256 KiB |
| lookup failure | logged, `200` with what was found | logged, `200` with what was found | MATCH |
| GET | 405 | 405 | MATCH (ABR) |
| path | `asset-bundle-registry.<domain>/profiles/metadata` | `/lambdas/profiles/metadata`, and the ABR host from the same processed profiles (`services/catalyst/shop/src/market/profiles/registry.rs`), which builds `thumbnailUrl` from the entity id | MATCH |

---

## 11. GET /denylist

In `services/catalyst/explore/src/api/registry.rs`, `denylist` validates the source,
`LocalDenylist::rows` caches the local read in shared API state, and
`merge_denylists` preserves exact ids.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| always | 200 | array ordered by `created_at` descending; each entry `{"entity_id":string,"reason":string\|null,"created_by":string,"created_at":string,"updated_at":string}` (millisecond `bigint`s arrive as strings); `entity_id` and `created_by` lowercased at insert ([`AB-9-denylist.json`](captures/abr/AB-9-denylist.json): 16 entries) | `get-denylist.ts:3-13`, `db.ts:1456-1481`, `src/types/types.ts:140-148` | the Registry source's list (Decentraland's on a mirror, the local runtime's own) merged with the node's `content.denylist` rows, newest first (`services/catalyst/explore/src/api/registry.rs`; test `denylist_merges_the_source_and_operator_rows`). An operator row reads `{"entity_id":id,"reason":null,"created_by":added_by,"created_at":ms,"updated_at":ms}` (`content.denylist` has no `reason` or `updated_at`; `Row` and `rows` in `services/catalyst/content/src/denylist.rs`); a local row whose id the source names exactly is left out (CIDv0 is case-sensitive, and the Registry lowercases at insert). `created_by` publishes the console wallet, as the Registry publishes its moderators'. The shared API state caches local rows and database failures for 5 s, with one refresh at a time; a database failure returns 500. `HEAD` fetches GET upstream and returns no body. A non-200 source response passes through; malformed JSON or a non-array 200 returns 502; without a source, the local rows | MATCH |

The captured answer and the merged dcl-one answer use `application/json`. The
public local feed intentionally includes all id kinds and the operator wallet in
`created_by`; see the [local denylist contract](../../../../../../docs/adr/2026-11-09-registry-local-catalyst.md#local-denylist).

**Data dependency:** the node's `content.denylist`. Decentraland's moderation list itself is in ABR's
database; a dcl-one mirror reads it into the content reads' projection (AB-10) and serves it through
the Registry source on each `GET /denylist`, but does not store it. Local-row
caching bounds database reads; source fetches, merge and JSON serialization still
run per request.

## 12. Admin and build-queue routes (outside catalyst parity)

- `GET /queues/status`: `{"windowsPendingJobs":[ids],"macPendingJobs":[ids],"webglPendingJobs":[]}`
  (`get-entity-status.ts:103-128`; [`AB-16-queues-status.json`](captures/abr/AB-16-queues-status.json):
  1466 ids each).
- `POST`/`DELETE /denylist/:entityId`: signed fetch by a moderator in the `user_moderators` feature
  list; `201` with the stored entry, `403 {"ok":false,"message":"Forbidden: signer is not an authorized moderator"}`
  (`post-denylist-entry.ts:4-41`). Not probed (writes).
- `POST /registry`, `DELETE /flush-cache`: bearer `API_ADMIN_TOKEN`. Not probed.

In dcl-one the operator console reaches the external abgen runtime for the writes
(`AdminConfig.abgen_admin_url` (`ABGEN_ADMIN_URL`) in `services/catalyst/config/src/ops.rs`), and the gateway aliases the root paths `/registry`,
`/denylist`, `/queues/`, `/flush-cache` to `/asset-bundle-registry$uri`
(the `/registry`, `/denylist`, `/queues/` and `/flush-cache` aliases in `infra/gateway/root-aliases.nix`): `GET /queues/status` is forwarded to the Registry source
there, `GET /denylist` is the merged list of section 11, and the writes have no route.

---

## 13. Backlog

1. **AB-1** `POST /profiles/metadata` (ABR host and `/lambdas/profiles/metadata`). Both paths serve
   ABR's projection (section 10, `decentraland/asset-bundle-registry:src/controllers/handlers/get-profiles-metadata.ts:11-21`,
   `src/logic/sync/profile-sanitizer.ts:107-116`) from the node's own profiles: the ABR host through
   `services/catalyst/shop/src/market/profiles/registry.rs` (mounted by `registry_profiles` in `services/catalyst/src/serve.rs`), `/lambdas/profiles/metadata`
   through `services/catalyst/shop/src/market/profiles.rs`. Severity: P1. Item by item:
   1. Closed: `ids: []` answers `200 []` on both paths, as ABR ([`AB-3-empty-array.json`](captures/abr/AB-3-empty-array.json); `services/catalyst/shop/src/market/profiles/registry.rs` test `registry_empty_ids_does_not_require_a_database_or_upstream`).
   2. Closed: no cap on the ids on either path, as ABR (`schemas/profiles.ts:5-7`, [`AB-3-1001-ids.json`](captures/abr/AB-3-1001-ids.json); tests `registry_ids_are_not_capped`, `profiles_metadata_takes_any_number_of_ids_and_answers_bad_json_with_a_bare_500`). The 256 KiB body limit separately answers `413`; the address population is body-bounded (fewer than 5,826 strings), and `default` plus one to three ASCII digits gives 1,110 distinct accepted spellings, including leading zeros. Eligible ids are processed 1,000 per entity read, without total-count truncation (`docs/adr/2026-11-09-registry-local-catalyst.md`).
   3. Closed: `{}`, `ids` not an array and a non-string element answer ABR's `400 {"error":"Bad request","message":"The ids property must be an array of non-empty strings"}` on both paths (`services/catalyst/shop/src/market/profiles/registry.rs`; `services/catalyst/shop/src/market/profiles.rs`; test `registry_invalid_ids_preserve_captured_error_envelope` against [`AB-3-missing-ids.json`](captures/abr/AB-3-missing-ids.json)).
   4. Closed: an empty-string element gets the same 400, ids are not trimmed ([`AB-3-empty-string-element.json`](captures/abr/AB-3-empty-string-element.json)).
   5. Closed: a body that is not JSON, or empty, answers `500 {"error":"Internal Server Error"}` on both paths, as ABR (test `registry_invalid_json_matches_current_capture` against [`AB-3-not-json.json`](captures/abr/AB-3-not-json.json)).
   6. Closed: keys serialize in ABR's order `pointer, hasClaimedName, name, nameColor, thumbnailUrl` on both paths (`ProfileMetadata`, `services/catalyst/shop/src/market/profiles.rs`; `services/catalyst/shop/src/market/profiles/registry.rs`; test `registry_projection_uses_shared_ownership_and_entity_snapshot_identity`).
   7. Closed for the ABR host, open for `/lambdas/profiles/metadata`: the ABR host builds `thumbnailUrl` from the entity id (`services/catalyst/shop/src/market/profiles/registry.rs`); the `/lambdas` path reads the processed snapshot, so an avatar without an `avatar` object gives `""`.
   8. Closed: absent `hasClaimedName`/`name` stay absent and a stored `nameColor: null` is sent as `null`, as ABR (test `registry_projection_preserves_omitted_name_color`).
   9. Closed: a database or ownership-reader failure is logged and answers `200` with what was found, as ABR (`profile-retriever.ts:50-53`, `:69-72`). Only ids that can name a profile (an address or a `default<n>` look) are looked up, 1000 per entity read, with no total-count truncation; addresses are body-bounded and `default` plus one to three ASCII digits has 1,110 distinct accepted spellings; a failed entity read leaves out its 1000 pointers, and on the ABR host a failed ownership pass leaves out its chunk of 128 entities (`services/catalyst/shop/src/market/profiles/registry.rs`); each failure counts in `profiles_lookup_failures_total{route,layer}`, since these answers are `200`s (`services/catalyst/shop/src/market/profiles.rs`). Unvalidated metadata is never served (tests `registry_failed_lookup_answers_what_was_found`, `profiles_metadata_answers_a_failed_read_with_what_was_found_on_both_paths`).
   10. Closed: `asset-bundle-registry.<domain>/profiles/metadata` and `/asset-bundle-registry/profiles/metadata` serve the node's profiles; there is no Registry read-through for profiles (`docs/adr/2026-11-09-registry-local-catalyst.md`).
   11. Open: `GET /lambdas/profiles/metadata` answers `405` where production lamb2 answers `404 {"error":"Not Found","message":"Profile not found"}` ([`AB-14-peer-get-lambdas-profiles-metadata.json`](captures/abr/AB-14-peer-get-lambdas-profiles-metadata.json)); `POST` is a dcl-one extension at a lamb2 path (production answers `405 Allow: HEAD, GET`).
   12. Closed for this path: CORS under `/lambdas/` is lamb2's (`*`, requested headers reflected, `max-age: 36000`, no `allow-methods`: `LAMBDAS_CORS` and `cors_middleware` in `services/catalyst/shop/src/market/lambdas_http.rs`), which is what production's gateway answers there ([`AB-14-peer-preflight-lambdas-profiles-metadata.json`](captures/abr/AB-14-peer-preflight-lambdas-profiles-metadata.json)). ABR's own host answers `max-age: 86400` and an `allow-methods` list ([`AB-12-preflight.json`](captures/abr/AB-12-preflight.json)); that host is AB-12.
2. **AB-2** `POST /profiles` (ABR host). Status: MATCH for the document. The node's profiles are
   answered as `[{"timestamp":number,"avatars":[…]}]` in ABR's key order, with each avatar's snapshots
   replaced by exactly `{face256, body}` at `<PROFILE_IMAGES_URL>/entities/<id>/{face,body}.png`
   (`decentraland/asset-bundle-registry:src/logic/sync/profile-sanitizer.ts:118-152`;
   `services/catalyst/shop/src/market/profiles/registry.rs`; tests `registry_projection_uses_shared_ownership_and_entity_snapshot_identity`,
   `registry_routes_read_local_profiles_and_observe_changes_without_a_registry_cache`; the number
   follows [`AB-2-one-known.json`](captures/abr/AB-2-one-known.json)). Remaining differences: the
   keys inside each avatar are serialized in sorted order, ABR keeps the stored order; request
   validation and failures are AB-1 items 2 and 9.
3. **AB-3** profile request validation on the ABR host (both profile routes). Status: MATCH. The rules of `decentraland/asset-bundle-registry:src/controllers/schemas/profiles.ts:9-29`
   and the captured statuses (section 10) hold: empty array `200 []`, the http-commons 400 envelope
   with the one message, `500` for unparseable bodies, `Content-Type` ignored and no trimming, from
   raw bytes (`services/catalyst/shop/src/market/profiles/registry.rs`, tests in that file), and no cap on the ids (AB-1 item 2).
4. **AB-4** `POST /entities/active` content part. Severity: P1.
   1. Closed: the request follows `decentraland/asset-bundle-registry:src/controllers/handlers/get-active-entities.ts:10-21`
      with every Registry source and without one: `pointers` only (`ids` and an absent, falsy or empty
      `pointers` → `400 {"ok":false,"message":"No pointers provided"}` in ABR's key order), a body that
      is not JSON → `500 {"error":"Internal Server Error"}`, no cap, the `world_name` query forwarded
      (`services/catalyst/explore/src/api/registry.rs`; test `registry_boot_reads_take_the_registry_pointer_rule` in `services/catalyst/explore/src/api/tests.rs` against
      [`AB-4-empty-pointers.json`](captures/abr/AB-4-empty-pointers.json)).
   2. Closed: entities carry ABR's `timestamp` string and `deployer` because the body is the
      backend's, unchanged unless AB-5 drops an entity.
   3. Open (owner decision): an entity this node holds and the backend has not converted is absent,
      as ABR answers nothing for an entity without a registry. With the local abgen runtime the node's
      deployments are converted there; with a public Registry an entity only this node holds stays
      absent, and without a source the answer is `[]`. Whether to answer such an entity from the
      node's content with AB fields `pending` (and which `status`) is not decided.
5. **AB-5** `POST /entities/active` and `/entities/versions` against the node's denylist. Status:
   MATCH. An `/entities/active` answer drops every entity whose `id` is in the content reads'
   projection (`content.denylist` and the `DENYLIST_URLS` lists), with their rule
   (`retain_non_denylisted` in `services/catalyst/content/src/queries.rs`, failing closed on an entity without an id),
   and keeps the backend's bytes when nothing is dropped (`services/catalyst/explore/src/api/registry.rs`; test
   `registry_active_entities_leave_out_the_content_denylist` in `services/catalyst/explore/src/api/tests.rs`). `/entities/versions`
   answers carry no entity id and ABR applies no denylist there
   (`decentraland/asset-bundle-registry:src/controllers/handlers/get-entity-versions.ts:10-44`), so
   they pass as received.
6. **AB-6** `GET /entities/status/:id`. The route forwards to the Registry source
   (`services/catalyst/explore/src/api/registry.rs`, `docs/adr/2026-11-09-registry-local-catalyst.md`): with a public Registry it answers
   section 6 for the entities that Registry knows, the local abgen runtime answers from its native
   state, and without a source it answers `503`. Open (owner decision): serving `entityId` and
   `catalyst` from the node's content (current id, overwritten id, newest entity on a pointer,
   `world_name`) with the AB fields from the source or `pending`, and the 404 body
   `{"ok":false,"message":"No active entity found for the provided id or pointer"}` for ids the node
   does not hold, replaces the ADR's forwarding for this route. Severity: P1.
7. **AB-7** `GET /entities/status` (signed fetch). Forwarded with the signed-fetch headers to the local
   abgen runtime only (`services/catalyst/explore/src/api/registry.rs`); a public Registry answers the captured unsigned 400. Expected:
   section 7 (`get-entity-status.ts:68-101`, `routes.ts:22-38`) for the signer's current and
   overwritten entities on this node. Change: `services/catalyst/explore/src/api/registry.rs` with the node's signed-fetch verifier.
   Severity: P2 (creator tooling).
8. **AB-8** `GET /worlds/:worldName/manifest`. The route forwards to the Registry source
   (`services/catalyst/explore/src/api/registry.rs`), so with a public Registry it answers that Registry's worlds and without a source
   `503`. Expected: section 8 (`decentraland/asset-bundle-registry:src/controllers/handlers/get-world-manifest.ts:13-59`,
   `src/logic/coordinates/component.ts:277-310`) from the node's worlds store: occupied parcels of the
   world's non-denylisted scenes, stored spawn when inside the bounds else the parcel nearest the
   centroid, numeric `x`/`y`, `total`; unknown world `200 {"occupied":[],"spawn_coordinate":{"x":0,"y":0},"total":0}`;
   invalid name 400. Open (owner decision): this replaces the ADR's forwarding for the route, and ABR
   counts only converted scenes, which the worlds store does not know; the answer must be mounted from
   `worlds` or the root crate (`docs/module-layers.toml`), reading
   `services/catalyst/worlds/src/core/store.rs` (`world_manifest`). Severity: P1.
9. **AB-9** `GET /denylist`. Status: MATCH. The answer is the Registry source's list (on a mirror
   Decentraland's moderation list, the one the mirror's content reads already hide, AB-10; with the
   local runtime its own list) merged with the node's `content.denylist` rows, newest first by
   `created_at` (`services/catalyst/explore/src/api/registry.rs`, `docs/adr/2026-11-09-registry-local-catalyst.md`).
   An operator row reads `reason: null` and `updated_at` equal to `created_at`; a local row whose id
   the source names exactly is left out (ids compare exactly: CIDv0 is case-sensitive). The shared
   API state caches local rows and failures for five seconds, with one refresh at a time;
   a failed local read returns 500. Source responses remain uncached: non-200 replies pass
   through, while malformed JSON or a non-array 200 returns 502. A source failure never
   serves a partial local-only success; without a source the answer is the local rows. The gateway's root `/denylist` alias points at this route
   (the `^~ /denylist` alias in `infra/gateway/root-aliases.nix`).
10. **AB-10** Decentraland's moderation denylist on the content server. Status: MATCH, no work.
    `DENYLIST_URLS` lists remote denylists, `NODE_POSTURE=mirror` sets it to
    `https://asset-bundle-registry.decentraland.org/denylist` (the `DENYLIST_URLS` row in `services/catalyst/config/src/posture.rs`,
    the `DENYLIST_URLS` argument in `services/catalyst/config/src/content.rs`), and the lists are fetched at boot and every
    120 s into the content reads' denylist projection, in memory only, keeping a URL's last good list
    when a fetch fails (`REMOTE_RELOAD_INTERVAL`, `reload_remote` and `spawn_remote_denylist` in `services/catalyst/content/src/denylist.rs`), as upstream
    (`decentraland/catalyst:src/Environment.ts:13`, `src/adapters/denylist/component.ts:59-86`,
    `src/components.ts:134-137`). Tests: `services/catalyst/content/src/denylist.rs`
    (`a_remote_listed_hash_is_a_404_on_contents`, `a_failed_fetch_keeps_the_last_list_and_the_operators`),
    `services/catalyst/content/src/tests.rs` (`a_remote_listed_entity_is_hidden_from_the_reads`).
11. **AB-11** `POST /entities/versions` error bodies and selector. Status: MATCH, no work:
    `{"ok":false,"message":"No pointers provided"}`, `pointers` only, no cap, the same rule as AB-4
    item 1 (`services/catalyst/explore/src/api/registry.rs`; test `registry_boot_reads_take_the_registry_pointer_rule` against
    [`AB-11-missing-pointers.json`](captures/abr/AB-11-missing-pointers.json)).
12. **AB-12** CORS on the ABR host. Status: MATCH. Both halves of `/asset-bundle-registry` answer the
    captured preflight (`allow-origin: *`, `allow-methods: GET,HEAD,OPTIONS,DELETE,POST,PUT`, reflected
    `allow-headers`, `vary: Access-Control-Request-Headers`, `max-age: 86400`) and add `allow-origin: *`
    to other responses only when the request has an `Origin` (`@dcl/http-server` `cors.js` with
    `decentraland/asset-bundle-registry@3608576f1ff8f76ffa43cfa47f3b1fe0449ac528:src/components.ts:54-62`;
    `services/catalyst/explore/src/api/registry.rs`), on paths neither half routes too: the gateway
    half holds the prefix's fallback (`registry.rs`), as ABR's CORS middleware runs before its router. Remaining difference: axum adds an `Allow`
    header to a preflight on a route without an `OPTIONS` handler.
13. **AB-13** error envelopes on the ABR host. The profile routes and the boot reads answer ABR's
    envelopes (`{"error":"Bad request","message":…}`, `{"ok":false,"message":…}` in ABR's key order,
    `500 {"error":"Internal Server Error"}`) and ignore `Content-Type` (section 2, cross-cutting
    table). Open: the gateway's own answers (`503` without a source, `502` on a backend failure, `400`
    for a normalized path) serialize `{"message":…,"ok":false}` with sorted keys
    (`services/catalyst/explore/src/api/registry.rs`). Severity: P2.
14. **AB-14** production routing. No production or zone Catalyst URL redirects or proxies to ABR
    (section 1). Status: MATCH, no work: a Catalyst replacement needs no ABR path under `/content`,
    `/lambdas` or the root to match production; `POST /lambdas/profiles/metadata` is a dcl-one
    extension (AB-1 item 11).
15. **AB-15** `GET /status`. Status: MATCH, no work.
16. **AB-16** `GET /queues/status`, denylist writes, `/registry`, `/flush-cache`: ABR administration and
    build state, outside catalyst parity. Status: out of scope, no work.

## 14. Routes dcl-one serves here that ABR does not

- `GET /asset-bundle-registry/health` → `{"status":"ok"}` (`services/catalyst/explore/src/api/registry.rs`, `health`).

The status-check probes of ABR's `createStatusCheckComponent`
(`decentraland/asset-bundle-registry@3608576f1ff8f76ffa43cfa47f3b1fe0449ac528:src/components.ts:64`) are
served under the prefix as ABR answers them: `GET /asset-bundle-registry/health/live` → `200 alive`,
`/health/ready` and `/health/startup` → the `application/health+json` pass document
`{"details":{"server":{"status":"pass"}},"status":"pass"}` (`services/catalyst/explore/src/api/registry.rs`,
`services/catalyst/shared/src/health.rs`). `@dcl/http-server`'s `status-checks.js` keys `details` by the
component's name in the service's component map, and ABR names its http server `server`
(`src/components.ts:54,226`; `@dcl/http-server` 2.2.1, `yarn.lock:1247-1249`), its only component with a
probe. They describe the Catalyst gateway, not the abgen runtime.
- Every route again under the `/asset-bundle-registry` prefix on the main host; production answers that
  prefix with the gateway 404 ([`AB-14-peer-get-abr-prefix-status.json`](captures/abr/AB-14-peer-get-abr-prefix-status.json)).
