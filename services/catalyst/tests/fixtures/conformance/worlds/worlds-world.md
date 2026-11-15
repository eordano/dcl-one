# Worlds content server: world metadata reads and settings

Area: the routes that describe a world and edit its settings: `GET /world/:world_name/about`,
`GET`/`POST /world/:world_name/scenes`, `GET`/`PUT /world/:world_name/settings`,
`GET /world/:world_name/manifest`, `GET /world/:world_name/limits`, `GET /worlds`,
`GET /wallet/:wallet/stats`, `GET /wallet/contribute`.

Pinned revisions:

- Upstream: `decentraland/worlds-content-server` @ `ebbf8d41d07973e317892067c765107d3512938e` (2026-11-12).
- Packages read from their published tarballs, at the versions `yarn.lock` resolves:
  `@dcl/http-commons` 2.0.0 (`errorHandler`, `getPaginationParams`),
  `@dcl/schema-validator-component` 1.0.1, `@dcl/crypto-middleware` 6.3.0, `@dcl/http-server` 2.3.0 (`cors.js`).
  `@dcl/schemas` (`EthAddress`, `IPFSv2`, `SceneParcels`), `@dcl/catalyst-contracts` and
  `@dcl/catalyst-api-specs` were not read; rows that depend on them say so.
- dcl-one: origin main `2da8b8aea`, Rust workspace `services/catalyst`.

The upstream side is from reading its code and tests; nothing upstream was built, run or captured. The
dcl-one side names the test that pins each closed row.

Files read:

- Upstream:
  - `src/controllers/routes.ts`
  - `src/controllers/handlers/{world-about,scenes,world-settings,world-manifest,world-limits,worlds,wallet-stats,contributor}-handler.ts`
  - `src/controllers/schemas/scenes-query-schemas.ts`
  - `src/controllers/multipart/{middleware,limits,errors}.ts`, `src/controllers/upload-error-handler.ts`,
    `src/controllers/source-upload-admission.ts`
  - `src/adapters/worlds-manager.ts` (`getMetadataForWorld`, `getWorldScenes`, `getWorlds`,
    `getContributableDomains`, `getWorldSettings`, `updateWorldSettings`, `getOccupiedParcels`,
    `getWorldBoundingRectangle`)
  - `src/adapters/{limits-manager/component,limits-manager/errors,wallet-stats,name-deny-list-checker,status}.ts`,
    `src/adapters/content-locks/{component,errors}.ts`, `src/adapters/source-upload-limits/errors.ts`
  - `src/logic/{settings/component,settings/errors,world-settings-policy/component,world-settings-policy/types,coordinates/component,thumbnails/component,form-data,world-runtime-metadata-utils}.ts`,
    `src/logic/worlds/component.ts` (`isWorldBlocked`, `getWorldManifest`), `src/logic/validations/parcels.ts`
  - `src/types.ts`, `src/components.ts`, `src/migrations/{0017,0018}_*.ts`
  - Tests: `test/integration/{world-about-handler,scenes-handler,world-settings-handler,world-manifest-handler,world-limits-handler,worlds-handler,wallet-stats-handler,contributor-handler}.spec.ts`,
    the matching parts of `world-manager-adapter.spec.ts`, `limits-manager-adapter.spec.ts`,
    `wallet-stats-adapter.spec.ts`, and `test/unit/{settings-logic,settings-content-lock,world-limits-handler,world-limits-parity,name-deny-list-checker}.spec.ts`.
- dcl-one (`services/catalyst/`):
  - `worlds/src/lib.rs` (`router`, `app_router`, `Settings::from_config`, `fill_world`)
  - `worlds/src/about.rs`, `worlds/src/scenes.rs`, `worlds/src/queries.rs` (`world_about`, `push_scene_filters`, `list_scenes_full`)
  - `worlds/src/mirror_reconcile.rs` (`mirror_world`) and `worlds/src/mirror_upstream.rs` (`listed_scenes`, `upstream_settings`): the rows a mirrored world is served from
  - `worlds/src/core/mod.rs` (`router`, `root_router`, `upload_routes`, `timed_routes`)
  - `worlds/src/core/{settings,settings_policy,manifest,limits,wallet,names,quota,http,store}.rs`
  - `worlds/src/writes.rs`
  - `comms/src/worlds_envelope.rs`, `comms/src/signed_fetch.rs`, `comms/src/world_identity.rs` (`Parcel`)
  - `shared/src/signed_fetch.rs`, `shared/src/identity.rs` (`Address`), `shared/src/cors.rs`
  - `content/src/errors.rs`, `content/src/lib.rs` (`content_public_url`)
  - `schemas/20_worlds.sql` (repo root)
  - `docs/worlds-compatibility.md`

Common notes:

- **Mount points.** Upstream serves these routes at the root of its own host. dcl-one serves every route in
  this area at the root and again under `/worlds-content-server`: the reads from `worlds::router`
  (`worlds/src/lib.rs:342-357`) and the rest from `core::root_router` / `core::router`
  (`worlds/src/core/mod.rs:588-628`, `worlds/src/lib.rs:394-434`). The two mounts answer the same.
- **Error envelope.** Upstream's `errorHandler` (`@dcl/http-commons` 2.0.0) answers
  `400 {"error":"Bad request","message":…}`, `404 {"error":"Not Found","message":…}`,
  `401 {"error":"Not Authorized","message":…}` and `500 {"error":"Internal Server Error"}`. Handlers that build
  their own response (settings) answer `{"error":<message>}` with no `message`. dcl-one's worlds envelope
  (`comms/src/worlds_envelope.rs:123-140`) reproduces the http-commons labels and serves `/about` too (WW-02);
  the settings handlers answer their own `{"error":<message>}` (`core/settings.rs` `SettingsError`,
  WW-17, WW-19, WW-20).
- **Signed fetch.** Upstream's strict `signedFetchMiddleware` guards `PUT /world/:world_name/settings` and
  `GET /wallet/contribute` (`src/controllers/routes.ts:98,222,252`): current payload only,
  `rejectIfSigner('decentraland-kernel-scene')`, failures answer
  `{"error":<detail>,"message":"This endpoint requires a signed fetch request. See ADR-44."}` with the
  detail's status (400 `Invalid Auth Chain`, 401 `Invalid signature…` / `Expired signature…`).
  dcl-one calls `require_verified(…, &[], …)` (no legacy fallback, the scene gate included,
  `comms/src/signed_fetch.rs`), so acceptance matches, and failures answer the same body (WW-24). The signed-fetch verifier itself belongs to the auth area.
- **Pagination.** `getPaginationParams` 2.0.0: limit 1..100 else 100, offset `parseInt` ≥ 0 else 0, no
  offset cap (the 100,000 cap arrived in 2.0.1, not the locked version). dcl-one `pagination_of`
  (`worlds/src/core/http.rs:27-41`) is the same, except that an offset beyond `i64` saturates (upstream
  passes it to Postgres, which answers a 500).
- **CORS.** Upstream `@dcl/http-server` answers `Access-Control-Allow-Origin: *` with methods
  `GET, HEAD, OPTIONS, DELETE, POST, PUT` and max-age 86400 (`src/components.ts:101-104`). dcl-one echoes the
  origin with `Access-Control-Allow-Credentials: true`, reflects requested headers and adds `PATCH`
  (`shared/src/cors.rs:14-79`). Same cross-cutting row as the content-lambdas CORS theme (WW-37).
- **Writes switch.** `PUT /world/:world_name/settings` sits behind `writes::guard`; while
  `WORLDS_WRITES_ENABLED` is off (the default) it answers 503
  `{"ok":false,"error":"Service Unavailable","message":…}` with `Retry-After: 3600`
  (`worlds/src/writes.rs:29-75`). Every other route in this area is a read and stays open (WW-27).
- **Deny list.** Both trees read the DAO `banned-names` list (upstream `DCL_LISTS_URL`, one-hour cache;
  dcl-one `explore_api.lists`, one-hour cache, `worlds/src/core/names.rs:16-78`) and normalize the name the
  same way (`toLowerCase().replace('.eth','').replace('.dcl','')`, first occurrence only) for `/about`,
  the settings, manifest and limits routes. `GET /worlds` instead strips the entries without lowercasing
  them and compares them exactly with the stored name (upstream `getBannedNames`), so it hides only a bare
  name; a moderation gap both trees share (WW-31).
  Both trees also refuse every whole name in `BLOCKED_WORLD_NAMES` on these routes and hide it from
  `GET /worlds`, even while the list cannot be read (`worlds/src/core/names.rs`; WR-35).

## Endpoint inventory

| # | method | upstream path | dcl-one route(s) | status |
|---|---|---|---|---|
| 1 | GET | `/world/:world_name/about` | `worlds/src/about.rs` `get_about` (route `worlds/src/lib.rs:344`) | DIVERGENT (intended only) |
| 2 | GET | `/world/:world_name/scenes` | `worlds/src/scenes.rs:49` `list_scenes` (route `lib.rs:345-348`) | MATCH |
| 3 | POST | `/world/:world_name/scenes` | `worlds/src/scenes.rs` `post_list_scenes` (same route) | DIVERGENT (intended only) |
| 4 | GET | `/world/:world_name/settings` | `worlds/src/core/settings.rs` `get_world_settings` (route `core/mod.rs:669-672`) | DIVERGENT (intended only) |
| 5 | PUT | `/world/:world_name/settings` | `worlds/src/core/settings.rs` `update_world_settings` (route `core/mod.rs:604-612`) | DIVERGENT (intended only) |
| 6 | GET | `/world/:world_name/manifest` | `worlds/src/core/manifest.rs:19` (route `core/mod.rs:637-640`) | DIVERGENT (intended only) |
| 7 | GET | `/world/:world_name/limits` | `worlds/src/core/limits.rs:50` (route `core/mod.rs:673`) | DIVERGENT (intended only) |
| 8 | GET | `/worlds` | `worlds/src/core/manifest.rs` `get_worlds` (route `core/mod.rs:636`) | DIVERGENT (intended only) |
| 9 | GET | `/wallet/:wallet/stats` | `worlds/src/core/wallet.rs:119` (route `core/mod.rs:679`) | DIVERGENT (intended only) |
| 10 | GET | `/wallet/contribute` | `worlds/src/core/wallet.rs:42` (route `core/mod.rs:678`) | DIVERGENT (intended only) |

Counts: 10 endpoints. MATCH 1, GAP 0, DIVERGENT 9 (all on intended differences only), MISSING 0.
Every route exists at both dcl-one mounts.

---

## 1. GET /world/:world_name/about

The explorer's realm handshake for a world: it reads `configurations.scenesUrn` to load scenes and
`comms.adapter` to join comms.

### Request

Path parameter only. Upstream handler `src/controllers/handlers/world-about-handler.ts:11-125`; metadata
from `worldsManager.getMetadataForWorld` (`src/adapters/worlds-manager.ts:384-435`), runtime metadata from
`buildWorldRuntimeMetadata` (`src/logic/world-runtime-metadata-utils.ts:5-92`). dcl-one
`worlds/src/about.rs:121-301`, data from `queries::world_about` (`worlds/src/queries.rs:78-115`).

### Responses

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Deny-listed name | 404 | `{"error":"Not Found","message":"World \"<name>\" has no scene deployed."}` in both | `world-about-handler.ts:22-24` | `about.rs` `get_about` | MATCH (WW-02) |
| No world row, or no scenes | 404 | the same, message `World "<name>" has no scenes deployed.` | `:26-29` | `about.rs` `build_about` | MATCH (WW-02). dcl-one first tries a federation fill (`lib.rs:273`, dcl-one only) |
| Owner quota-blocked over 2 days | 401 | `{"error":"Not Authorized","message":"World \"<name>\" has been blocked since <Date.toString() in UTC> as it exceeded its allowed storage space."}` in both | `:31-35`, `logic/worlds/component.ts:52-60` | `about.rs` `build_about` | MATCH (WW-02, WW-03) for a UTC upstream. The grace matches; the live-holder rule and operator disable are intended (WW-13) |
| `ETH_NETWORK` not in `l1Contracts` | 500 | `{"error":"Internal Server Error"}` | `:46-50` | `lib.rs:122-127` maps anything but `sepolia` to 1 | DIVERGENT, config only, intended (WW-14) |
| Success | 200 | example below | `:121-124` | `about.rs:258-300` | DIVERGENT (fields below) |

Upstream success body for one scene (from `world-about-handler.spec.ts:106-145`, which uses `toEqual`):

```json
{
  "healthy": true,
  "acceptingUsers": true,
  "spawnCoordinates": "20,24",
  "configurations": {
    "networkId": 1,
    "globalScenesUrn": [],
    "scenesUrn": ["urn:decentraland:entity:<entityId>?=&baseUrl=http://0.0.0.0:3000/contents/"],
    "minimap": {"enabled": false, "dataImage": "http://0.0.0.0:3000/contents/<hash>"},
    "map": {"minimapEnabled": false, "sizes": []},
    "skybox": {"textures": ["http://0.0.0.0:3000/contents/<hash>"]},
    "realmName": "<worldName>"
  },
  "content": {"healthy": true, "publicUrl": "https://peer.com/content", "synchronizationStatus": "Syncing"},
  "lambdas": {"healthy": true, "publicUrl": "https://peer.com/lambdas"},
  "comms": {"healthy": true, "protocol": "v3", "adapter": "fixed-adapter:signed-login:http://0.0.0.0:3000/worlds/<worldname>/comms"}
}
```

**Fields** (upstream / dcl-one):

| field | upstream | dcl-one | verdict |
|---|---|---|---|
| `scenesUrn` | the most recently deployed scene only: `getWorldScenes(…, {limit: 1, orderBy: created_at, desc})` (`worlds-manager.ts:403-407`); the spec pins length 1 (`world-about-handler.spec.ts:541-554`) | the newest scene only (`about.rs` `build_about`, `.take(1)` of the newest-first rows) | MATCH (WW-01) |
| `baseUrl` in URNs, file URLs, adapter | `HTTP_BASE_URL`, else the request's host | URNs and file URLs: `Settings::content_server_url`, `<PUBLIC_URL>/worlds-content-server` (WR-12); adapter: `Settings.public_url` | MATCH when `HTTP_BASE_URL` is the worlds host |
| `spawnCoordinates` | `worlds.spawn_coordinates`, `null` when unset | the same | MATCH (WW-07) |
| `realmName` | `worldConfiguration.name` (after the `dclName` migration) `|| worldName` | the same (`about.rs` `RuntimeMeta::from_scene`), then the ENS strip and override (`resolve_realm_name`) | MATCH (WW-05, WW-06); strip intended (WW-12) |
| `minimap.enabled` | `miniMapConfig.visible ?? false`, where a truthy legacy `minimapVisible` replaces the whole `miniMapConfig` | the same, read as a boolean (JS truthiness) | MATCH (WW-05) |
| `minimap.dataImage` / `estateImage` | present when enabled or set; the file URL, else `https://api.decentraland.org/v1/minimap.png` / `estatemap.png` | same rule; defaults `map_parcel_view_url` (this node's `/api/v1/minimap.png` unless configured, `config/src/config.rs:570`) and `map_estate_view_url` | DIVERGENT: default URLs, intended (WW-09) |
| `skybox.fixedHour` | `worlds.skybox_time` when not NULL, else `skyboxConfig.fixedTime` (or legacy `skybox`); absent when neither | the same; the number as stored or deployed | MATCH (WW-04, WW-05) |
| `skybox.textures` | each texture resolved to `<baseUrl>/contents/<hash>`; an unresolved file becomes `""`; `[]` when unset | the same | MATCH (WW-06) |
| `globalScenesUrn` | `GLOBAL_SCENES_URN.split(' ')` (two spaces give an empty element) | `split_whitespace` (`lib.rs:166-173`) | DIVERGENT, config only, intended (WW-14) |
| `networkId` | `l1Contracts[ETH_NETWORK].chainId` | `network_id_for` (`lib.rs:122-127`) | MATCH for mainnet and sepolia |
| `map` | `{minimapEnabled:false, sizes:[]}` | same | MATCH |
| `healthy`, `acceptingUsers`, `content.healthy`, `lambdas.healthy` | `GET <CONTENT_URL>/status` reports `Syncing`, and `GET <LAMBDAS_URL>/status` answers, cached 5 min (`adapters/status.ts`) | false only when this node's content sync failed; lambdas always true (`about.rs:93-118,262-276`) | DIVERGENT, intended (WW-10) |
| `content.publicUrl`, `lambdas.publicUrl` | `new URL(CONTENT_URL).toString()` (`https://peer.com/content`, no trailing slash added) | `CONTENT_URL`/`LAMBDAS_URL` as set, else `<public_url>/content` and `/lambdas` (`lib.rs` `Settings::from_config`) | MATCH (WW-08) |
| `comms.adapter` | `fixed-adapter:offline:offline` when `fixedAdapter` is `offline:offline` or `single_player`; otherwise `fixed-adapter:signed-login:<baseUrl>/worlds/<lowercase name>/comms` (`world-about-handler.ts:127-134`) | the same, and offline too while LiveKit has no credentials or the SFU probe fails (`about.rs:320-333`) | DIVERGENT, intended (WW-11) |
| `comms.healthy`, `protocol` | `true`, `"v3"` | same | MATCH |
| `content.synchronizationStatus` | `"Syncing"` | same | MATCH |

### Data dependency

The about route reads `worlds.worlds` and `worlds.world_scenes` (both trees), the quota block
(dcl-one: `worlds.quota_blocks` for the live NAME holder, `about.rs:136-145`), the deny list, and the comms
state (dcl-one: `cfg.comms_online && cfg.sfu.is_alive()`).

**Dependency on the comms area.** The adapter string points the explorer at
`POST <public_url>/worlds/<name>/comms`, which dcl-one serves from the comms gatekeeper, not the worlds crate.
Whether that URL answers at both mounts, and with the upstream token shape, is the comms area's row. The
offline fallback (WW-11) reads the shared SFU liveness that comms maintains.

---

## 2. GET /world/:world_name/scenes

### Request

| param | upstream (`scenes-handler.ts:18-85`) | dcl-one (`scenes.rs:49-223`) | verdict |
|---|---|---|---|
| `limit`, `offset` | `getPaginationParams` | `pagination` | MATCH (common notes) |
| `x1, x2, y1, y2` | present, non-empty, `Number()`-valid values; one to three of them is 400 `Bounding box requires all of x1, x2, y1, y2 to be provided.`; the box is min/max of each pair | `bounding_box` and `js_number` (`scenes.rs:101-148`) | MATCH |
| `authorized_deployer` | `EthAddress.validate` or 400 `Invalid authorized_deployer address: <v>. Must be a valid Ethereum address.`; owner or world `deployment` grant | `Address::parse_untrimmed` behind a `0x` check (`scenes.rs:157-170`); same SQL (`queries.rs:165-177`) | MATCH (assuming the `@dcl/schemas` regex `^0x[a-fA-F0-9]{40}$`) |
| `entity_id` | present (even empty) and not `IPFSv2` is 400 `Invalid entity_id: <v>. Must be a valid IPFS CIDv1.` | `is_cid_v1` (`scenes.rs:151-153,171-178`) | MATCH (depends on `IPFSv2` being `^ba[a-zA-Z0-9]{57}$`) |
| repeated keys | `searchParams.get` reads the first | `query_param` reads the first | MATCH |

### Responses

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Success | 200 | `{"scenes":[…],"total":<n>}` | `scenes-handler.ts:73-84` | `scenes.rs:187-223` | MATCH |
| Unknown world | 200 | `{"scenes":[],"total":0}` | `:73` | same (after a federation fill attempt, dcl-one only) | MATCH |
| Filter errors | 400 | `{"error":"Bad request","message":…}` | above | above | MATCH |
| Database error | 500 | `{"error":"Internal Server Error"}` | `errorHandler` | `ApiError::Internal` | MATCH |

Scene objects: upstream spreads `WorldScene` (`worlds-manager.ts:60-73`) with `size` as a string:
`worldName, deployer, entityId, deploymentAuthChain, entity, parcels, size, status, createdAt, updatedAt`.
dcl-one has the same keys and values (`scenes.rs:22-35`), with `status` always `"DEPLOYED"` (retired scenes
live in `worlds.retired_world_scenes`, so `world_scenes` holds deployed rows only), and timestamps as
RFC 3339 with milliseconds and `Z`, as `Date.toJSON()` writes them. Key order differs (`entityId` after
`entity`); no client reads key order. Order: upstream `ORDER BY created_at ASC` with no tie-break;
dcl-one adds `entity_id` (`queries.rs:198`), a deterministic subset of the upstream order. A mirrored
world's rows carry the times, size, chain and deployer its source lists (WW-40, WW-41). No deny-list
check in either tree.

---

## 3. POST /world/:world_name/scenes

### Request

Upstream: `schemaValidator.withSchemaValidatorMiddleware(getWorldScenesSchema)` then the GET handler with
`coordinates` from the body (`routes.ts:204-208`). Schema (`schemas/scenes-query-schemas.ts:10-26`):
object, `additionalProperties:false`, required `coordinates`: array of 1..500 unique strings matching
`^-?\d+,-?\d+$`. Coordinates are canonicalized before the `parcels && $1` match.
dcl-one: `post_list_scenes` (`scenes.rs:67-108`) checks the Content-Type before it reads the body, then
`schema_coordinates` (`scenes.rs:150`) checks the same rules and canonicalizes with
`Parcel::parse_strict` (`comms/src/world_identity.rs:348-360`). Upstream reads a body of any size
(`@dcl/http-server` is created without `maxBodySize`); dcl-one reads at most
`MAX_SCENES_QUERY_BODY_BYTES` (2 MiB).

### Responses

| trigger | status | upstream body | dcl-one body | verdict |
|---|---|---|---|---|
| `Content-Type` not JSON (`application/json` or `type/sub+json`) | 415 | `{"ok":false,"message":"Content-Type must be application/json"}` | the same (`content::validation::is_json_content_type`), for a body of any size | MATCH (WW-15) |
| Body is not JSON | 400 | `{"ok":false,"message":"<JSON.parse message>"}` | the same, V8's text for the captured cases (`content::validation::json_parse_message`), else serde's | MATCH (WW-15) |
| Schema failure | 400 | `{"ok":false,"message":"Invalid JSON body","data":[<AJV errors>]}` | the same, the first AJV 8.17.1 error (`scenes.rs` `schema_coordinates`) | MATCH (WW-15); the spec asserts only `message` (`scenes-handler.spec.ts:153-156`) |
| A coordinate past the i32 range (`"99999999999,0"`) | 200 (matches the pattern; no scene) | the same | | MATCH (WW-16) |
| JSON body over 2 MiB | no cap: answered as any other body | 413, `@dcl/http-server`'s own `Payload Too Large` text with `Connection: close` (`scenes.rs` `payload_too_large`; `core/tests.rs` `world_scenes_page_filter_and_count_like_the_old_listing`) | | DIVERGENT, intended (worlds-compatibility.md, "Scene queries") |
| Success, query filters, errors | as GET | as GET | | MATCH |

---

## 4. GET /world/:world_name/settings

### Responses

| trigger | status | upstream body | dcl-one body | verdict |
|---|---|---|---|---|
| Name that is not a valid world key | upstream 404 or 200 | dcl-one 400 `{"error":"Bad request","message":"Invalid world name: …"}` (`permissions::parse_world`) | | DIVERGENT, intended (WW-29) |
| No `worlds` row | 404 | `{"error":"World \"<name>\" not found or has no settings configured."}` (`world-settings-handler.ts:168-174`, `logic/settings/errors.ts:8-13`) | the same (`settings.rs` `get_world_settings`) | MATCH (WW-17); the spec asserts `error` (`world-settings-handler.spec.ts:233-242`) |
| Success | 200 | `toSnakeCaseSettings` (`world-settings-handler.ts:24-38`) | `settings_json` | MATCH plus two keys, intended (WW-18) |

Success fields:

| key | upstream (`worlds-manager.ts:1236-1262`) | dcl-one | verdict |
|---|---|---|---|
| `title`, `description`, `content_rating`, `spawn_coordinates`, `thumbnail_hash` | the value, or **absent** when NULL or empty | the same | MATCH (WW-18) |
| `categories` | the array (`[]` kept), or absent when NULL | the same | MATCH (WW-18) |
| `skybox_time` | number or `null` | same | MATCH |
| `single_player`, `show_in_places` | booleans, NULL read as `false` / `true` | NOT NULL columns with those defaults (`schemas/20_worlds.sql:85-86`) | MATCH |
| `access_type` | `access.type` | `access_type` column | MATCH |
| `settings_version` | number; bumped by every settings write that sets a column and by every access change, not by a spawn-only write (`world-settings-handler.spec.ts:96-199`) | same (`store_settings.rs` `update_world_settings`, `core/tests.rs:2308-2390`) | MATCH |
| `realm_name_override`, `preview_wearable_urns` | not present | dcl-one only, always present | DIVERGENT, superset, intended (WW-18) |

A mirrored world's settings are the ones its source answers on this route (WW-42). No deny-list check in
either tree.

---

## 5. PUT /world/:world_name/settings

### Request

Upstream chain (`routes.ts:214-233`): `stampRequestArrival`, per-source upload admission (2 MiB), strict signed
fetch, multipart parser (2 MiB payload cap, wire cap 2 MiB + 10 MiB, `repeatableFields: ['categories']`,
busboy limits 100 fields of 1 MiB), then the handler under the shared content lock.

dcl-one order (`settings.rs` `update_world_settings`): world-name parse, signed fetch, `Content-Length` check
against the 12 MiB wire cap, per-source admission, a process-wide concurrent-upload slot, then the body: the
content-type check and the multipart parse under the upload timeout, then authorization, thumbnail store and
the update under the processing deadline. The body is not read before the signed fetch (WW-24).

Fields (upstream `parseMultipartInput`, `world-settings-handler.ts:40-153`; dcl-one `parse_multipart`,
`settings.rs:222-470`):

| field | upstream | dcl-one | verdict |
|---|---|---|---|
| `title` | first value; 3..100 UTF-16 units; empty or out of range is 400 `Invalid title: <v or "title cannot be empty">. Expected between 3 and 100 characters.` | same (`text_len` counts UTF-16 units) | MATCH |
| `description` | same with 3..1000 | same | MATCH |
| `content_rating` | defined (every value non-empty) and in `RP, E, T, A, R`, else 400 `Invalid content rating: <v>. Expected one of: RP, E, T, A, R` | same | MATCH |
| `spawn_coordinates` | defined; `parseCoordinate` (`split(',')` into exactly two, `parseInt`, bounds ±150), else 400 `Invalid spawnCoordinates format: "<v>".`; then inside the row lock: no deployed scene is 400 `Invalid spawnCoordinates "<v>". The world has no deployed scenes.`, outside the shape is 400 `…It must be within the world shape rectangle: (x,y) to (x,y).` | `Parcel::parse` plus bounds (`settings.rs` `parse_coordinate`), same messages (`store_settings.rs` `update_world_settings`), same lock | DIVERGENT: `parseInt` leniency, intended (WW-25) |
| `skybox_time` | `"null"` clears; else `Number(v)` must be an integer in the i32 range, else 400 `Invalid skybox_time: <v>. Expected an integer between -2147483648 and 2147483647, or null.` | same (`settings.rs:390-400`) | MATCH |
| `categories` | repeatable; `["null"]` clears to `[]`; more than 20 is 400 `Invalid categories: <n> items. Expected at most 20` | same (`settings.rs:418-431`) | MATCH |
| `single_player`, `show_in_places` | defined; `=== 'true'` | same | MATCH |
| `thumbnail` file | over 1 MiB is 400 `Invalid thumbnail: size <n> bytes exceeds maximum of 1048576 bytes (1MB).`; PNG/JPEG/GIF87a/GIF89a/WebP magic, else 400 `Invalid thumbnail: expected a PNG, JPEG, GIF or WebP image.`; stored as SHA-256 hex | same checks (`settings.rs:293-301,455-467`, `settings_policy.rs`); stored as CIDv1 after authorization | MATCH on checks; hash intended (WW-28) |
| a non-repeatable field sent twice | 400 `Duplicate form field '<name>'` (`multipart/middleware.ts:233-240`) | the same (`categories` and the dcl-one `preview_wearable_urns` repeat) | MATCH (WW-22) |
| `realm_name_override`, `preview_wearable_urns` | ignored | dcl-one only fields | dcl-one superset |

Authorization: upstream `namePermissionChecker.checkPermission` (the NAME owner), else a world-wide
`deployment` grant (`logic/settings/component.ts:49-57`); parcel-scoped grants are refused. dcl-one
`policy.is_owner`, else `has_world_wide_permission(Deployment)` (`settings.rs` `update_world_settings`); an owner lookup
that cannot be decided answers 503 (`ApiError::from(undetermined)`), where upstream's checker throws a 500.
MATCH on the decision.

### Responses

| trigger | status | upstream body | dcl-one body | verdict |
|---|---|---|---|---|
| Unsigned / malformed chain | 400 | `{"error":"Invalid Auth Chain","message":"This endpoint requires a signed fetch request. See ADR-44."}` (`world-settings-handler.spec.ts:849-876`) | the same; the body is read only after the signed fetch | MATCH (WW-24) |
| Validation failure (fields above, and the two spawn refusals from `store::update_world_settings`) | 400 | `{"error":"<message>"}` | the same (`SettingsError::Handler`; `core/tests.rs` `settings_empty_values_follow_the_upstream_handler`) | MATCH (WW-19) |
| Not owner, no world-wide grant | 403 | `{"error":"Unauthorized. You do not have permission to update settings for this world."}` | the same | MATCH (WW-20) |
| Body too large (declared `Content-Length` over the wire cap, payload over 2 MiB, a file over 2 MiB, a field over 1 MiB, too many parts or fields) | 413 | `{"error":"Payload Too Large","message":…}` (`multipart/middleware.ts:159-200`, `upload-error-handler.ts:49-51`); the spec pins 413 `An uploaded file exceeds the maximum allowed size.` (`world-settings-handler.spec.ts:1045-1080`) | the same statuses and texts (`upload_limits::too_large`, `upload_limits::too_many_fields_message`; `core/tests.rs` `a_settings_put_declaring_too_many_bytes_answers_413`) | MATCH (WW-21) |
| Not `multipart/form-data` (after the signed fetch) | 400 | busboy 1.6.0: `{"error":"Bad request","message":"Missing Content-Type"` / `"Unsupported content type: <ct>"` / `"Multipart: Boundary not found"}`; busboy also reads `application/x-www-form-urlencoded` | the same three texts; a urlencoded form is `Unsupported content type` | MATCH; urlencoded intended (worlds-compatibility.md, "Settings uploads") |
| Invalid `Content-Length` | 400 | `{"error":"Bad request","message":"Invalid Content-Length header."}` | same | MATCH |
| Source over its share | 429 + `Retry-After: 5` | `{"error":"Too Many Requests","message":"Too many uploads in progress from this client, please retry shortly."}` | same (`settings.rs:120-126`; keyed on `X-Real-IP`, intended in worlds-compatibility.md) | MATCH |
| In-flight byte or file budget exhausted | 503 + `Retry-After: 5` | `{"error":"Service Unavailable","message":"Server is buffering too many uploads…"}` / `…upload files…` | same messages | MATCH |
| Process-wide concurrent uploads exceeded | dcl-one 503 + `Retry-After: 5` `Server is handling too many concurrent uploads, please retry shortly.` | upstream has no such cap on this route | | DIVERGENT, intended (WW-23) |
| Body not received in time | 408 | `{"error":"Request Timeout","message":"The upload did not finish within <s> s: received …"}` (`describeUploadTimeout`) | the same text over the bytes received (`settings.rs` `describe_upload_timeout`; `core/tests.rs` `a_stalled_settings_upload_answers_the_upstream_timeout_text`) | MATCH (WW-23) |
| Processing over the deadline | dcl-one 408 `Deployment processing exceeded the <n>ms deadline.` | upstream has no processing deadline here | | DIVERGENT, intended (WW-23) |
| Writes switch off | dcl-one 503 `{"ok":false,"error":"Service Unavailable","message":…}`, `Retry-After: 3600` | not upstream | | DIVERGENT, intended (WW-27) |
| Success | 200 | `{"message":"World settings updated successfully","settings":<as GET>}` | the same, with the two extra keys (WW-18) | MATCH plus two keys, intended (WW-18) |

Side effects: upstream publishes `WorldSettingsChangedEvent` and, when the spawn changed,
`WorldSpawnCoordinateSetEvent` to SNS (`logic/settings/component.ts:102-134`); dcl-one publishes neither
(WW-26, intended). Upstream holds the shared content lock so a running GC cannot delete a just-stored thumbnail
before its row lands (`routes.ts:223`, `unit/settings-content-lock.spec.ts`); dcl-one's collector has an age
floor (`core/admin.rs:7,66-108`), which closes the same race (WW-30).

---

## 6. GET /world/:world_name/manifest

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Invalid world key | dcl-one 400 `Invalid world name: …` | | | `manifest.rs:23` | DIVERGENT, intended (WW-29) |
| Deny-listed | 404 | `{"error":"Not Found","message":"World \"<name>\" not found."}` | `world-manifest-handler.ts:32-35` | `manifest.rs:24-32` | MATCH |
| No occupied parcel | 404 | `…"message":"World \"<name>\" has no scenes deployed."` | `:38-42` | `:33-37` | MATCH |
| Success | 200 | `{"occupied":[…],"spawn_coordinate":{"x":"<x>","y":"<y>"},"total":<n>}` | `:44-58`, `logic/worlds/component.ts:122-141`, `worlds-manager.ts:1552-1600` | `manifest.rs:38-46`, `store_settings.rs` `world_manifest` | MATCH: distinct parcels sorted by integer x then y, first 500, `total` the distinct count, spawn `"0,0"` when unset |

---

## 7. GET /world/:world_name/limits

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Invalid world key | dcl-one 400 | | | `limits.rs:55` | DIVERGENT, intended (WW-29) |
| `parcels` present but not unique canonical pointers (`?parcels=` included) | 400 | `{"error":"Bad request","message":"Parcels must be unique canonical \"x,y\" coordinates, as in a scene's parcels."}` | `world-limits-handler.ts:38-42` (`SceneParcels.validate`, `@dcl/schemas`) | `limits.rs:28-48`, `deploy_validate.rs:45-61` | MATCH (depends on `SceneParcels`) |
| A parcel out of ±150 | 400 | `…"message":"Coordinate X value <x> is out of bounds. Must be between -150 and 150."` | `:43-49` | `deploy_validate.rs:63-79` | MATCH |
| Deny-listed (checked after the parcels) | 404 | `…"message":"World \"<lowercased>\" not found."` | `:54-56` | `limits.rs:60-69` | MATCH |
| Owner cannot be determined | 404 | `…"message":"Could not determine owner for world <lowercased>"` | `:69-71`, `limits-manager/errors.ts` | `limits.rs:72-91` | MATCH |
| Allowance unavailable | dcl-one 503 `The world's storage allowance could not be read right now` | upstream counts a failed holdings fetch as an error (500) after retries | | `limits.rs:91-96` | DIVERGENT, intended (WW-35) |
| Query over the header limit | upstream 431 from Node's 16 KB header limit (`world-limits-handler.spec.ts:49-62`) | dcl-one: whatever the gateway and hyper allow | | | DIVERGENT, edge (WW-35) |
| Success | 200 | `{"maxSceneSizeInBytes":"<decimal>","maxParcels":<n>}` | `:58-67`, `limits-manager/component.ts:58-87` | `limits.rs:98-101` | MATCH in shape; the size comes from the intended allowance rules (WW-35) |

---

## 8. GET /worlds

### Request

| param | upstream (`worlds-handler.ts:49-104`) | dcl-one (`manifest.rs:51-126`) | verdict |
|---|---|---|---|
| `limit`, `offset` | `getPaginationParams` | `pagination_of` | MATCH |
| `authorized_deployer` | `EthAddress.validate` (no trim, `0x` only) | `core::http::authorized_deployer` (`0x` check, then `Address::parse_untrimmed`), shared with the scene listings | MATCH (WW-32) |
| `search` | trimmed; empty is absent; over 64 UTF-16 units is 400 `Invalid search parameter: must be at most 64 characters.` | the same (`text_len`) | MATCH (WW-32) |
| `has_deployed_scenes` | present: `=== 'true'` | same | MATCH |
| `sort` | `name` (default) or `last_deployed_at`, else 400 `Invalid sort parameter: <v>. Valid values are: name, last_deployed_at` | same | MATCH |
| `order` | `asc` (default) or `desc`, else 400 `Invalid order parameter: <v>. Valid values are: asc, desc` | same | MATCH |
| a repeated key | first value | the same (`QueryPairs`, `query_param`) | MATCH (WW-32) |

### Responses

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Success | 200 | `{"worlds":[…],"total":<n>}` | `worlds-handler.ts:97-103`, `worlds-manager.ts:1366-1550` | `manifest.rs:123-125`, `store_queries.rs` `list_worlds_public` | DIVERGENT (fields below) |
| Validation errors | 400 | `{"error":"Bad request","message":…}` | above | above | MATCH |

World objects, key order `name, owner, title, description, shape, content_rating, spawn_coordinates,
skybox_time, categories, single_player, show_in_places, thumbnail_hash, last_deployed_at, blocked_since,
deployed_scenes` in both (`worlds-handler.ts:29-47`, `manifest.rs:133-157`).

| field / behaviour | upstream | dcl-one | verdict |
|---|---|---|---|
| Deny-listed names | `w.name != ALL(getBannedNames())` (`worlds-manager.ts:1371,1409-1413`): the `BLOCKED_WORLD_NAMES` entries as configured, then the list entries with .eth/.dcl stripped; since a stored name keeps its suffix, a list entry hides only bare-name worlds, case-sensitively | the same filter on the page and the count (`store_queries.rs` `list_worlds_public`, `BannedNames::banned_names`) | MATCH; the list gap is shared (WW-31) |
| `shape`, `deployed_scenes`, `last_deployed_at` | denormalized columns kept by `recalculateWorldSceneStats`; `null` / `0` / `null` with no scene | computed from `world_scenes` (`store_queries.rs` `list_worlds_public`): the same values, a mirrored world's from its source's deploy times (WW-40) | MATCH |
| `blocked_since` | any `blocked` row of the owner, no grace (`worlds-manager.ts:1404`) | `quota_blocks` row only for quota-blockable local DCL NAME and ENS worlds (`store_queries.rs` `list_worlds_public`, `quota.rs:39-45`) | DIVERGENT, intended (WW-33) |
| `owner` | NOT NULL string | `null` for worlds with no owner (bare names, mirrors) | DIVERGENT, intended (WW-33) |
| `single_player`, `show_in_places` | `COALESCE(…, false/true)` | NOT NULL columns | MATCH |
| `categories` | the column (`null` when unset) | same | MATCH |
| timestamps | `Date.toISOString()` | RFC 3339 ms `Z` | MATCH |
| Search | `search_vector` (weighted per column) `@@ plainto_tsquery`, OR `ILIKE '%term%'` unescaped, OR trigram > 0.3 | on-the-fly `to_tsvector(name || title || description)`, escaped ILIKE, trigram only with `pg_trgm` (`store_queries.rs` `list_worlds_public`) | DIVERGENT, intended (worlds-compatibility.md, "`GET /worlds` search"); match sets agree except a literal `%`/`_` |
| Ordering | `ORDER BY w.name` / `last_deployed_at IS NULL, last_deployed_at <dir>, name` | same (`store_queries.rs` `list_worlds_public`) | MATCH (collation of the two databases not compared) |

---

## 9. GET /wallet/:wallet/stats

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Not `^0x[a-fA-F0-9]{40}$` | 400 | `{"error":"Bad request","message":"Invalid request. Missing or invalid wallet in request url param."}` | `wallet-stats-handler.ts:11-13` | `wallet.rs:114-130` | MATCH |
| Success | 200 | `{"wallet":<as sent>,"dclNames":[{"name","size"}],"ensNames":[…],"usedSpace":"<n>","maxAllowedSpace":"<n>","blockedSince"?:<ISO>}` | `:15-33`, `adapters/wallet-stats.ts:77-106` | `wallet.rs:119-185`, `quota.rs:119-164` | DIVERGENT, intended (WW-34) |

Shape, ordering (`ORDER BY w.name`), the `wallet` echo and `blockedSince` (absent when not blocked) match.
Which worlds are listed and `maxAllowedSpace` follow the intended allowance rules: upstream lists every world
whose stored owner is the wallet, splitting on the `.dcl.eth` suffix (bare names fall into `ensNames`), and
reads `spaceAllowance` from `DCL_NAME_STATS_URL` (`Number.MAX_SAFE_INTEGER` MiB without it). dcl-one lists the
NAMEs the wallet holds now, drops mirrored, personal and bare-name worlds, and computes the allowance in process
(WW-34).

---

## 10. GET /wallet/contribute

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Signed-fetch failure | 400/401 | crypto-middleware shape | `routes.ts:252` | `wallet.rs:48-56` | MATCH (WW-24) |
| Success | 200 | `{"domains":[{"name","user_permissions":[…],"size":"<n>","owner","parcelCount":<n>}],"count":<n>}` | `contributor-handler.ts:5-16`, `worlds-manager.ts:893-939` | `wallet.rs:42-91` | MATCH except `owner` (WW-36) |

Both select the worlds where the signer holds a grant, with `user_permissions` sorted distinct,
`size` the deployed size as text, and `parcelCount` 0 when any grant is world-wide, else the smallest grant.
dcl-one restricts `permission IN ('deployment','streaming')`, the only two types the table holds in either
tree, and orders by name (upstream: group order). `owner` is `null` for worlds with no stored owner.

---

## Backlog (WW-)

1. **WW-01 · `/about` · `scenesUrn` names the newest scene.** Status: MATCH, no work.
   - Upstream: the newest scene only (`decentraland/worlds-content-server:src/adapters/worlds-manager.ts:403-407`,
     `src/logic/world-runtime-metadata-utils.ts:76-84`); the spec pins one URN for a two-scene world
     (`test/integration/world-about-handler.spec.ts:500-555`). Runtime metadata (realm name, minimap, skybox,
     `fixedAdapter`) comes from that same scene.
   - dcl-one: `build_about` takes the first of the newest-first rows (`worlds/src/about.rs:192-195`,
     `worlds/src/queries.rs:79-93`, a tie going to the greater entity id); runtime metadata reads the same
     scene; `is_locally_published` reads every row. A mirrored world's rows carry its source's deploy times (WW-40).
     Pinned by `about.rs` `scenes_urn_names_the_newest_scene_only` and `tests/worlds_mirror.rs`.
   - Severity: none (closed).

2. **WW-02 · `/about` · error envelope.** Status: MATCH, no work.
   - Upstream: `NotFoundError`/`NotAuthorizedError` through http-commons: `{"error":"Not Found"|"Not Authorized","message":…}`;
     the specs assert `message` (`world-about-handler.spec.ts:30-37,571-578`) and `error: 'Not Authorized'` (`:604-614`).
   - dcl-one: `get_about` and `build_about` answer `core::http::ApiError` (`not_found`, `unauthorized`;
     `about.rs:126-195`). Pinned by `about.rs` `a_deny_listed_world_is_404_before_any_read` and
     `a_world_without_scenes_is_404_and_a_blocked_world_401`.
   - Severity: none (closed).

3. **WW-03 · `/about` · blocked message date.** Status: MATCH, no work.
   - Upstream: `${blockedSince}` is `Date.prototype.toString()`, e.g. `Tue Oct 06 2026 10:00:00 GMT+0000 (Coordinated Universal Time)`.
   - dcl-one: both block messages format the date as `%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)`
     (`about.rs:187`); an operator disable says `by an operator` (WW-13). Upstream's text follows its process time
     zone; production runs in UTC. Pinned by `about.rs` `a_world_without_scenes_is_404_and_a_blocked_world_401`.
   - Severity: none (closed).

4. **WW-04 · `/about` · `skybox.fixedHour` absent or a number.** Status: MATCH, no work.
   - Upstream: the key is absent when no fixed time is set (the spec's `toEqual` body has no `fixedHour`); a set value is a JSON integer.
   - dcl-one: `SkyboxConfig.fixed_hour` is a `serde_json::Number` left out when unset (`about.rs:29-36`);
     `library/catalyst-api/worlds/SkyboxConfig.ts`, the sites zod schema and
     `library/catalyst-api/openapi/worlds.openapi.json` type it as an optional number. Pinned by `about.rs`
     `runtime_metadata_follows_upstream_migrate_configuration`.
   - Severity: none (closed).

5. **WW-05 · `/about` · legacy `worldConfiguration` keys.** Status: MATCH, no work.
   - Upstream `migrateConfiguration` (`world-runtime-metadata-utils.ts:5-33`): a truthy `dclName` replaces `name`; a
     truthy `minimapVisible` replaces the whole `miniMapConfig` with `{visible}` (dropping `dataImage`/`estateImage`);
     a truthy `skybox` replaces `skyboxConfig` with `{fixedTime: skybox}`. A falsy legacy key is skipped.
   - dcl-one: `RuntimeMeta::from_scene` applies the three rules with JS truthiness (`about.rs:358-430`). Pinned by
     `about.rs` `runtime_metadata_follows_upstream_migrate_configuration` (truthy and falsy legacy keys).
   - Severity: none (closed).

6. **WW-06 · `/about` · unresolved textures and an empty name.** Status: MATCH, no work.
   - Upstream: an unresolved texture is `""` in `textures`; `name || worldName` treats `""` as missing.
   - dcl-one: each texture is resolved to `Option<hash>` in order and an unresolved (non-string, empty or unknown)
     one links as `""` (`about.rs` `build_about`, `RuntimeMeta::from_scene`); an empty name falls back to the
     world name. Same test as WW-05.
   - Severity: none (closed).

7. **WW-07 · `/about` · `spawnCoordinates`.** Status: MATCH, no work.
   - Upstream: `worlds.spawn_coordinates`, `null` when unset (`worlds-manager.ts:420-427`).
   - dcl-one: the column, `null` when unset (`about.rs:222`).
   - Severity: none (closed).

8. **WW-08 · `/about` · `publicUrl` without a trailing slash.** Status: MATCH, no work.
   - Upstream: `new URL(CONTENT_URL).toString()`: `https://peer.com/content` (spec `world-about-handler.spec.ts:136-139`).
   - dcl-one: the worlds `Settings::from_config` takes a set `CONTENT_URL`/`LAMBDAS_URL` as configured and this
     node's own URLs without the trailing slash (`worlds/src/lib.rs:136-148`); the content `/about` keeps its own.
     Pinned by `lib.rs` `settings_derive_the_worlds_urls_from_the_node_config`.
   - Severity: none (closed).

9. **WW-09 · `/about` · default minimap images.** Status: intended (worlds-compatibility.md).
   - Upstream: `https://api.decentraland.org/v1/minimap.png` and `/v1/estatemap.png`.
   - dcl-one: this node's `/api/v1/minimap.png` and `/api/v1/estatemap.png` unless `MAP_PARCEL_VIEW_URL` /
     `MAP_ESTATE_VIEW_URL` name others (`services/catalyst/config/src/config.rs:570-572`); worlds-compatibility.md,
     "`/about` configuration". Setting both to the upstream URLs gives a byte-equal answer.
   - Severity: none (closed).

10. **WW-10 · `/about` · health fields.** Status: intended (worlds-compatibility.md).
    - Upstream polls `CONTENT_URL/status` and `LAMBDAS_URL/status`; dcl-one reads its own sync state and reports
      lambdas healthy (worlds-compatibility.md, "`/about` health").
    - Drop-in: a stock explorer refuses a realm with `acceptingUsers:false`; dcl-one reports false less often, never more. No break.
    - Severity: none (closed).

11. **WW-11 · `/about` · offline adapter while the SFU is down.** Status: intended (worlds-compatibility.md).
    - dcl-one: `comms_online && sfu.is_alive()` (`about.rs:239`, `resolve_fixed_adapter` `:328`);
      worlds-compatibility.md, "Offline comms when the SFU is down". Pinned by `about.rs`
      `an_unreachable_sfu_serves_the_offline_adapter`.
    - Drop-in: a client gets an offline realm instead of a handshake that fails. No break.
    - Severity: none (closed).

12. **WW-12 · `/about` · `realmName` strip and override.** Status: intended (worlds-compatibility.md).
    - dcl-one strips `.dcl.eth` for locally published worlds (`WORLDS_REALM_NAME_STRIP_ENS`, default true) and applies
      `realm_name_override` (`about.rs:209-210,303-326`); worlds-compatibility.md, "Realm name".
    - Drop-in: a client that compares `realmName` with the world name gets a different string;
      `WORLDS_REALM_NAME_STRIP_ENS=false` gives a byte-equal answer.
    - Severity: none (closed).

13. **WW-13 · `/about` · operator disable and live-holder quota block.** Status: intended (worlds-compatibility.md).
    - dcl-one: worlds-compatibility.md, "Operator disables apply at once".
    - Severity: none (closed).

14. **WW-14 · `/about` · configuration edges.** Status: intended (worlds-compatibility.md).
    - `GLOBAL_SCENES_URN` with repeated spaces: upstream keeps empty elements (`split(' ')`), dcl-one splits on runs of whitespace.
    - An `ETH_NETWORK` other than mainnet/sepolia: upstream 500, dcl-one `networkId: 1` (`lib.rs:122`).
    - Both under worlds-compatibility.md, "`/about` configuration"; production values answer the same.
    - Severity: none (closed).

15. **WW-15 · `POST /scenes` · schema-validator bodies.** Status: MATCH, no work.
    - Upstream (`@dcl/schema-validator-component` 1.0.1): 415 `{"ok":false,"message":"Content-Type must be application/json"}`;
      400 `{"ok":false,"message":<JSON.parse error>}`; 400 `{"ok":false,"message":"Invalid JSON body","data":[<AJV errors>]}`.
    - dcl-one: `post_list_scenes` (`worlds/src/scenes.rs:67-108`) checks the Content-Type with
      `content::validation::is_json_content_type` before reading the body, then answers the validator's three
      bodies, keys in its order (`content::validation::ordered_refusal_json`), the parse message from
      `content::validation::json_parse_message` and the first AJV error from `schema_coordinates`
      (`scenes.rs:150`), checked against AJV 8.17.1. Pinned by `scenes.rs` `schema_errors_are_the_first_ajv_error`,
      `content/src/validation.rs` `json_content_types_follow_the_schema_validator` and `core/tests.rs`
      `world_scenes_page_filter_and_count_like_the_old_listing`. A body over 2 MiB is the intended
      difference under worlds-compatibility.md, "Scene queries".
    - Severity: none (closed).

16. **WW-16 · `POST /scenes` · coordinates past i32.** Status: MATCH, no work.
    - Upstream: the pattern accepts any digit count; such a coordinate matches no scene (200).
    - dcl-one: the pattern alone decides (`scenes.rs` `matches_coordinate_pattern`); a coordinate past `i32` is
      kept as sent and matches nothing. Pinned by `core/tests.rs` `world_scenes_page_filter_and_count_like_the_old_listing`.
    - Severity: none (closed).

17. **WW-17 · `GET /settings` · 404 body.** Status: MATCH, no work.
    - Upstream: `404 {"error":"World \"<name>\" not found or has no settings configured."}`
      (`src/logic/settings/errors.ts:8-13`, `world-settings-handler.ts:168-174`).
    - dcl-one: `SettingsError::Handler` answers `{error}` only (`worlds/src/core/settings.rs:65-92,105-119`).
      Pinned by `core/tests.rs` `every_route_answers_its_contract` (the `nope.dcl.eth` read).
    - Severity: none (closed).

18. **WW-18 · `GET`/`PUT /settings` · absent keys, and two extra keys.** Status: MATCH, no work; the extra keys intended (worlds-compatibility.md).
    - Upstream omits `title`, `description`, `content_rating`, `spawn_coordinates`, `thumbnail_hash` when NULL or
      empty, and `categories` when NULL (`worlds-manager.ts:1236-1262`).
    - dcl-one: `settings_json` leaves the same keys out (`settings.rs:122-150`) and adds `realm_name_override` and
      `preview_wearable_urns` (worlds-compatibility.md, "Settings superset"). The sites settings reader
      (`apps/sites/src/data/lib/catalyst/creator-hub/world-settings.ts`) accepts absent keys. Pinned by
      `settings.rs` `settings_json_keeps_set_values_and_drops_empty_text` and `world-settings.test.ts`.
    - Severity: none (closed).

19. **WW-19 · `PUT /settings` · 400 body.** Status: MATCH, no work.
    - Upstream: every validation failure is `400 {"error":"<message>"}` (`world-settings-handler.ts:208-213`); the spec asserts `error`
      for each message (`world-settings-handler.spec.ts:332-1160`).
    - dcl-one: the field validations and the two spawn refusals of `store::update_world_settings` answer
      `400 {error: <message>}` (`SettingsError::invalid`, `settings.rs:252-258`); the parser's refusals
      (Content-Length, duplicates, content type) keep the http-commons shape, as upstream. Pinned by
      `core/tests.rs` `settings_empty_values_follow_the_upstream_handler`.
    - Severity: none (closed).

20. **WW-20 · `PUT /settings` · 403 text.** Status: MATCH, no work.
    - Upstream: `Unauthorized. You do not have permission to update settings for this world.` in `error` only.
    - dcl-one: `NOT_ALLOWED_MESSAGE` in `error` only (`settings.rs:97-98,242`). Pinned by `core/tests.rs`
      `every_route_answers_its_contract`.
    - Severity: none (closed).

21. **WW-21 · `PUT /settings` · size and count limits.** Status: MATCH, no work.
    - Upstream: a declared length over the wire cap, a payload, file or field over its limit, too many parts or fields:
      `413 {"error":"Payload Too Large","message":…}`; a field over 1 MiB says
      `Field '<name>' is too large. The maximum allowed size per field is 1048576 bytes.`
      (`src/controllers/multipart/middleware.ts:159-200,330-336`).
    - dcl-one: every size and count branch answers `upload_limits::too_large` with upstream's text; the fields
      cap is `upload_limits::too_many_fields_message` (`settings.rs:178-183,384-495`,
      `worlds/src/core/upload_limits.rs`). Pinned by `settings.rs` `parse_multipart_caps_each_field_value_at_one_megabyte`,
      `parse_multipart_rejects_more_than_one_hundred_fields`, `parse_multipart_rejects_more_than_the_parts_limit`,
      and `core/tests.rs` `a_settings_put_declaring_too_many_bytes_answers_413` and `every_route_answers_its_contract`
      (a file over 2 MiB).
    - Severity: none (closed).

22. **WW-22 · `PUT /settings` · duplicate fields.** Status: MATCH, no work.
    - Upstream: a second `title` (any field but `categories`, or any file name seen twice) is
      `400 {"error":"Bad request","message":"Duplicate form field 'title'"}`.
    - dcl-one: `parse_multipart` refuses a name seen before unless both parts are fields of `categories` or the
      dcl-one `preview_wearable_urns` (`settings.rs:102,398-403`). Pinned by `settings.rs` `a_repeated_form_field_is_refused`.
    - Severity: none (closed).

23. **WW-23 · `PUT /settings` · timeouts and the concurrency slot.** Status: MATCH, no work, for the timeout text; the slot and the processing deadline intended (worlds-compatibility.md).
    - Upstream: 408 with `describeUploadTimeout`'s text (`multipart/errors.ts:32-50`); no processing deadline and no
      process-wide upload count on this route.
    - dcl-one: 408 with `describe_upload_timeout`'s text over the body bytes received and the declared length
      (`settings.rs:206-228,318-329`); the `MAX_CONCURRENT_UPLOADS` slot and the
      `DEPLOYMENT_PROCESSING_TIMEOUT_MS` deadline are under worlds-compatibility.md, "Settings uploads". Pinned by
      `settings.rs` `the_timeout_message_is_upstreams` and `core/tests.rs`
      `a_stalled_settings_upload_answers_the_upstream_timeout_text`.
    - Severity: none (closed).

24. **WW-24 · `PUT /settings`, `/wallet/contribute` · signed-fetch failures.** Status: MATCH, no work.
    - Upstream: `{"error":<detail>,"message":"This endpoint requires a signed fetch request. See ADR-44."}`, detail
      e.g. `Invalid Auth Chain` (`src/controllers/routes.ts:89-92`, spec `world-settings-handler.spec.ts:849-876`),
      before the body is parsed.
    - dcl-one: `map_auth_error_api` answers the crypto-middleware body (worlds-comms.md WC-01); the settings handler
      takes the raw request and reads the body only after the signed fetch (`settings.rs:153-169`), so an unsigned
      request of any content type gets the signed-fetch refusal. Pinned by `core/tests.rs`
      `signed_routes_refuse_with_the_crypto_middleware_body` and
      `an_unsigned_settings_put_is_refused_by_the_signed_fetch_before_the_body`.
    - Severity: none (closed).

25. **WW-25 · `PUT /settings` · `spawn_coordinates` parsing.** Status: intended (worlds-compatibility.md).
    - Upstream `parseCoordinate` splits on `,` and uses `parseInt`, so `5abc,3`, `5.9,3` and ` 5,3` pass and are stored as sent.
    - dcl-one: `Parcel::parse` refuses trailing garbage and fractions (`settings.rs:659`); worlds-compatibility.md,
      "Settings uploads".
    - Severity: none (closed).

26. **WW-26 · `PUT /settings` · no settings events.** Status: intended (worlds-compatibility.md).
    - Upstream publishes `WorldSettingsChangedEvent` and `WorldSpawnCoordinateSetEvent` to SNS (`src/logic/settings/component.ts:102-134`).
    - dcl-one publishes nothing; its explore crate reads the worlds tables directly (worlds-compatibility.md,
      Notifications, "Settings events").
    - Severity: none (closed).

27. **WW-27 · `PUT /settings` · the writes switch is off by default.** Status: intended (worlds-compatibility.md).
    - dcl-one: 503 `{"ok":false,"error":"Service Unavailable","message":…}` with `Retry-After: 3600` until
      `WORLDS_WRITES_ENABLED` is on (`services/catalyst/worlds/src/writes.rs:29-75`); worlds-compatibility.md,
      "Writes switch", once for every worlds write route (WD-03 too).
    - Drop-in: a node started with defaults refuses every settings edit until the operator sets the switch.
    - Severity: none (closed).

28. **WW-28 · `PUT /settings` · thumbnail hash is CIDv1.** Status: intended (worlds-compatibility.md).
    - Upstream stores SHA-256 hex (spec `world-settings-handler.spec.ts:424-447`); dcl-one CIDv1 (`settings.rs:245`),
      worlds-compatibility.md, "Thumbnails". Both serve it from `/contents/<hash>`.
    - Severity: none (closed).

29. **WW-29 · settings, manifest, limits · invalid world name is 400.** Status: intended (worlds-compatibility.md).
    - dcl-one: worlds-compatibility.md, "Invalid world names".
    - Severity: none (closed).

30. **WW-30 · `PUT /settings` · content lock.** Status: MATCH, no work.
    - Upstream holds the shared content lock across the thumbnail write and the row update.
    - dcl-one's collector skips blobs younger than its age floor (`services/catalyst/worlds/src/core/admin.rs:7,66-108`), so a just-stored thumbnail cannot be collected before its row lands.
    - Severity: none (closed).

31. **WW-31 · `/about`, `/manifest`, `/limits`, `/worlds` · deny-listed and blocked names.** Status: MATCH, no work.
    - Upstream: `BLOCKED_WORLD_NAMES` is split on commas, trimmed, lowercased and stripped of empty entries
      (`src/adapters/name-deny-list-checker.ts:8-13`). `checkNameDenyList` refuses an exact lowercased match before
      it reads the remote list, so the block holds while that list is unset or failing (`:45-49`). `/about` then
      answers 404 `World "<n>" has no scene deployed.` (`world-about-handler.ts:22-24`), `/manifest` 404
      `World "<n>" not found.` (`world-manifest-handler.ts:33-35`) and `/limits` 404 with the lowercased name
      (`world-limits-handler.ts:54-56`). `GET /worlds` adds `w.name != ALL(getBannedNames())`
      (`worlds-manager.ts:1371,1409-1413`); `getBannedNames` is the configured names followed by each list entry with
      its first `.eth` and `.dcl` stripped and not lowercased, deduplicated (`name-deny-list-checker.ts:61-65`).
      Postgres compares case-sensitively, so a list entry hides only a bare name equal to it.
    - dcl-one: `NameDenyList::new` parses `BLOCKED_WORLD_NAMES` (`config/src/worlds.rs:449-454`,
      `worlds/src/core/names.rs:31-39,92-97`); `BannedNames::allows` refuses a configured name first, also when the
      list read fails (`names.rs:51-54,107-120`), on `/about` (`about.rs:129-137`), `/manifest`
      (`core/manifest.rs:22-30`) and `/limits` (`core/limits.rs:60-69`). `list_worlds_public` adds
      `w.name <> ALL($5)` to the page and the count (`worlds/src/core/store_queries.rs` `list_worlds_public`) with
      `BannedNames::banned_names()` (`names.rs:67-76`), the same order, stripping and no lowercasing. Pinned by
      `names.rs` `configured_blocks_refuse_exact_names_and_lead_the_banned_names` and
      `configured_blocks_hold_while_the_list_cannot_be_read`, `core/tests.rs:6366`
      `configured_world_names_are_hidden_from_the_world_reads` and `core/tests.rs`
      `worlds_listing_reads_the_query_like_url_search_params_and_hides_banned_bare_names`.
    - Both trees list `<banned>.dcl.eth` from the remote list while `/about` and the manifest answer 404 for it;
      recorded as a shared moderation gap in worlds-compatibility.md, "Deny list in world listings".
    - Severity: none (closed).

32. **WW-32 · `/worlds` · query parsing.** Status: MATCH, no work.
    - Upstream reads each parameter with `URLSearchParams.get` (the first of a repeated key), validates
      `authorized_deployer` with `EthAddress` (no trim, `0x` only) and counts the 64-character `search` limit in UTF-16 units.
    - dcl-one: `get_worlds` reads `QueryPairs` with `query_param` (`core/manifest.rs:51-105`), validates the deployer
      with `core::http::authorized_deployer` (`core/http.rs:19-35`, shared with the scene listings) and counts with
      `settings_policy::text_len`. Same test as WW-31.
    - Severity: none (closed).

33. **WW-33 · `/worlds` · `blocked_since` and `owner`.** Status: intended (worlds-compatibility.md).
    - Upstream: `blocked_since` is any block of the stored owner; `owner` is never null.
    - dcl-one: only quota-blockable local DCL NAME and ENS worlds show a block (`store_queries.rs` `list_worlds_public`, `quota.rs:39-45`);
      `owner` is `null` for ownerless worlds; worlds-compatibility.md, "World listings".
    - Severity: none (closed).

34. **WW-34 · `/wallet/:wallet/stats` · allowance and listed worlds.** Status: intended (worlds-compatibility.md).
    - Bare-name worlds, which upstream lists under `ensNames`, are not in the page (`quota.rs:156-161`);
      worlds-compatibility.md, "Wallet storage allowance".
    - Severity: none (closed).

35. **WW-35 · `/limits` · allowance sources and the header limit.** Status: intended (worlds-compatibility.md).
    - dcl-one answers 503 while the allowance or owner cannot be read; upstream retries the stats service and then fails
      (worlds-compatibility.md, "Wallet storage allowance", "Per-world limits", "Unowned names").
    - The 431 upstream gets from Node's 16 KB header limit for very long `parcels` lists depends on the gateway and hyper here; not compared.
    - Severity: none (closed).

36. **WW-36 · `/wallet/contribute` · `owner` null.** Status: intended (worlds-compatibility.md).
    - dcl-one returns `owner: null` for worlds with no stored owner (`wallet.rs:23-31`); upstream's column is NOT NULL.
      Recorded with WW-33 under "World listings".
    - Severity: none (closed).

37. **WW-37 · all routes · CORS.** Status: intended (worlds-compatibility.md).
    - Upstream `Access-Control-Allow-Origin: *`; dcl-one echoes the origin with credentials (`services/catalyst/shared/src/cors.rs:72-79`),
      the CORS layer shared with the content server (content-lambdas theme "CORS"); worlds-compatibility.md, "CORS".
    - Severity: none (closed).

38. **WW-38 · paged routes · offset past i64.** Status: Kept by decision.
    - Upstream (http-commons 2.0.0) passes it to Postgres, which answers 500; dcl-one saturates and answers an empty page (`core/http.rs:49-60`).
    - Severity: none (closed).

39. **WW-39 · mirror · a multi-scene world's scene set.** Status: MATCH, no work.
    - Upstream `/about` lists the newest scene only (WW-01); `GET /world/:n/scenes` lists every deployed
      scene, paged oldest first (`scenes-handler.ts` `getScenesHandler`, `worlds-manager.ts` `getWorldScenes`).
    - dcl-one: the mirror reads a world's scene ids from the source's paged `/world/{name}/scenes`
      (`listed_scenes`, `worlds/src/mirror_upstream.rs`) and keeps `/about` for the world's spawn and the
      content `baseUrl`. The listing decides what is retired, so any doubt stops that world's sync until the
      next pass and retires nothing: a refusal (404 and 405 included), a page without `scenes`, a repeated id,
      a count other than `total`, or a held scene missing from the pages that `?entity_id=` still lists (the
      listing moved while it was read). A world over 2,000 scenes is refused on every pass. The read-through
      fill takes the same path and verifies each scene by the `deploymentAuthChain` of its listing row. The
      newest scene must verify, or nothing is stored. An older scene whose chain this node will not accept
      (a signer its NAME authority refuses and no trusted upstream grant covers, as a former owner's or a
      collaborator's, or an over-size, malformed or empty chain) is left out with a warning and nothing of it
      is fetched. A signature that does not match its entity, or an authority that cannot answer, still fails
      the whole fill. Pinned by `tests/worlds_mirror.rs:1208`
      `a_world_of_many_scenes_is_mirrored_whole_from_the_paged_listing`, `tests/worlds_read_through.rs:1047`
      `a_world_of_many_scenes_is_filled_whole_on_a_miss_from_its_listing_chains`, `:1100`
      `an_older_scene_signed_by_one_who_may_not_publish_is_left_out_of_the_fill`, `:1145`
      `older_scenes_with_an_over_size_malformed_or_empty_chain_are_left_out_of_the_fill`, `:1187`
      `an_older_scene_whose_grant_cannot_be_read_fails_the_whole_fill` and `:1215`
      `an_older_scene_whose_chain_does_not_sign_it_fails_the_whole_fill`; the fakes
      in `tests/worlds_mirror.rs` and `tests/worlds_read_through.rs` answer `/about` with the newest URN only.
    - Severity: none (closed).

40. **WW-40 · mirror · `/about`, `/scenes`, `/worlds` · each scene's deploy times.** Status: MATCH, no work.
    - Upstream: each row keeps its deployment's `created_at`. `/about` takes the newest scene
      (`getWorldScenes({limit: 1, orderBy: created_at, desc})`, `worlds-manager.ts:403-407`), `/scenes` pages by
      `created_at` ascending (`:1015-1019`) and serves `createdAt`/`updatedAt` from the row (`toWorldScene`,
      `:60-73`), and `/worlds` reads `last_deployed_at` as the newest deploy time (`:1366-1404`).
    - dcl-one: `listed_scenes` keeps each listed row's `createdAt` and `updatedAt`, read as RFC 3339 into UTC
      (`ListedScene::from_row`, `worlds/src/mirror_upstream.rs`), and the world's transaction writes them onto its rows; a row the
      listing gives no time keeps this node's (`mirror_world`, `mirror_reconcile.rs`). `/about` breaks a remaining tie by the
      greater entity id (`queries.rs:93`). Pinned by `tests/worlds_mirror.rs:1291`
      `a_mirrored_world_keeps_the_upstream_deploy_times`: three scenes whose newest is the smallest id; `/about`
      names it, `/scenes` lists the source's order and times, and `/worlds` its `last_deployed_at`.
    - A world the pass finds unchanged (every listed scene held, `holds_every_scene`, `mirror_reconcile.rs`) is still read for its
      `/scenes` listing, and its held rows take the listed times, size, chain and deployer
      (`refresh_listed_scenes`, `mirror_reconcile.rs`), so rows written before these were kept catch up without a
      redeploy. Pinned by `tests/worlds_mirror.rs:1730`
      `an_unchanged_mirrored_world_takes_settings_and_listing_updates`.
    - Severity: none (closed).

41. **WW-41 · mirror · `/scenes` · `deployer`, `deploymentAuthChain`, `size`.** Status: MATCH, no work.
    - Upstream: each row's `deployer`, `deployment_auth_chain` and `size` are what its deployment stored
      (`toWorldScene`, `worlds-manager.ts:60-73`); `scenes-handler.ts:73-84` serves them, the size as a string.
    - dcl-one: a pass keeps the listed size (`mirror_world`, `mirror_reconcile.rs`) and stores the listed chain and deployer when the
      chain's ECDSA links sign the scene id and its first link names the listed deployer, checked with
      `verify_auth_chain` (`listed_signature`, `mirror_upstream.rs`); otherwise the row carries `[]` and the zero
      address. A read-through stores the chain and signer it checked against the world's publishers. A pass over
      an unchanged world fills a held row that has no chain the same way (WW-40). No grant here reads
      `worlds.world_scenes.deployer`. Pinned by `tests/worlds_mirror.rs:1392`
      `a_mirror_pass_keeps_the_upstream_deployer_and_chain` (a signed scene; a chain over another id; a chain whose
      signer is not the listed deployer) and `tests/worlds_mirror.rs:1730`.
    - Severity: none (closed).

42. **WW-42 · mirror · `/settings`, `/worlds`, `/about` · a mirrored world's settings.** Status: MATCH, no work.
    - Upstream: `GET /world/:world_name/settings` is public (`routes.ts:213`) and answers the stored row
      (`world-settings-handler.ts:155-178`, `worlds-manager.ts:1222-1262`); its comments (`:1242-1243,1254-1255`)
      say `skybox_time: null` and the version are exposed so that a mirror can copy them. `/worlds` serves the same
      columns (`:1381-1404`). Settings change without a deployment: a settings write bumps `settings_version`
      (`:143-164`), and so does an access change, "because mirrors read the access type through
      getWorldSettings" (`:768-776`); `/index` carries neither. `/about` serves the row's `skybox_time` when it is
      not null, else the scene's, and `offline:offline` when `single_player` (`:409-419`).
    - dcl-one: every pass that reaches a mirrored world, changed or not, reads
      `GET {source}/world/{name}/settings` (one body of at most 256 KiB, `upstream_settings`) and, on a row that is not disabled,
      writes `title`, `description`, `content_rating` (one of `RP`, `E`, `T`, `A`, `R`), `categories` (the first
      20), `show_in_places`, `single_player`, `skybox_time` (`null` clears) and `thumbnail_hash`
      (`WorldSettingsCopy` in `mirror_upstream.rs`, `apply_settings` in `mirror_reconcile.rs`). An owner's `PUT /world/{name}/settings` here that
      changes one of these marks the row `settings_set_here` for the name's holder (a collaborator's edit too), and
      no pass replaces those settings while that holder holds the name as this node's own owner lookup says. The
      mark covers the whole group: once the owner edits a setting here, this node keeps all of them as they are
      and the source's later edits to the others are not copied, so one owner's settings never mix with the
      source's; after a NAME transfer the source's
      settings apply again (`store::mirror_keeps_local`, worlds-permissions.md WP-21). `settings_version` takes the
      source's counter and never goes back: a row ahead of its source (bumped by earlier ingests) moves one past
      its own value when a copied column changes. No answer, a refusal or a body that is not an object leaves
      the stored settings as they are and is logged as a warning; a new row then takes the fixed hour and offline adapter the source's
      `/about` serves, which is the source's stored value whenever it has one, until a later
      pass reads the settings. `access_type` is copied from `/permissions` (worlds-permissions.md, WP-21).
      Pinned by `mirror.rs` `upstream_settings_are_copied_as_the_columns_accept_them`,
      `tests/worlds_mirror.rs:1468` `a_mirror_pass_copies_the_upstream_world_settings`, `:1730`
      `an_unchanged_mirrored_world_takes_settings_and_listing_updates` (an edit with no redeploy, a first pass
      without an answer, a version ahead of the source), `an_owner_edit_here_is_kept_while_that_owner_holds_the_name`
      and `:608` (the `/about` fallback).
    - The thumbnail is held here (`mirrored_thumbnail`): a CID key is fetched from the source's
      `/contents` when not held; a SHA-256 hex key (an upload upstream, `logic/settings/component.ts:65`) is
      fetched, checked against its digest and kept under the CIDv1 of its bytes, so `/settings` names that CID and
      Places' `image` and `/contents` serve it; worlds-compatibility.md, "Thumbnails". A key the source does not
      serve, or whose bytes are not an image, leaves a new world without a thumbnail; a held row keeps its stored
      thumbnail when the fetch fails, and drops it when the bytes are not an image. Pinned by
      `tests/worlds_mirror.rs` `a_mirrored_world_holds_the_thumbnail_its_settings_name`.
    - Severity: none (closed).

## Endpoints dcl-one serves in this area that upstream does not

| method | path | handler | note |
|---|---|---|---|
| GET | `/world/{world_name}/preview-wearables` | `worlds/src/preview_wearables.rs` (route `worlds/src/lib.rs:349-352`) | reads the `preview_wearable_urns` setting |
| GET | `/worlds/status` | `worlds/src/status.rs` (route `lib.rs:354`) | worlds status under the root mount |
| GET | `/admin/worlds`, `/admin/worlds/{world_name}`, `…/ban-status`; POST `…/enable`, `…/disable` | `worlds/src/core/admin.rs` (routes `core/mod.rs:680-693`) | operator world reads and the disable that WW-13 refers to |
| GET | `/openapi.json` (under `/worlds-content-server`) | `worlds/src/lib.rs:419-422` | the worlds OpenAPI document |
| PUT fields | `realm_name_override`, `preview_wearable_urns` on `/world/{world_name}/settings` | `core/settings.rs:402-446` | dcl-one-only settings (WW-18) |
