# Worlds content server: world permissions

Area: the ten `/world/:world_name/permissions*` routes of the worlds content server, the
legacy-payload signed fetch on `POST .../permissions/:permission_name`, and the side effects a client can
see: participant kicks after an access change, permission notifications, the world-settings event, and
what `GET .../permissions` reveals.

Pinned revisions:

- Upstream: `decentraland/worlds-content-server` @ `ebbf8d41d07973e317892067c765107d3512938e` (2026-11-12).
- dcl-one: this tree, Rust workspace `services/catalyst`.

Everything below comes from reading the code. Nothing was built or run, and no line is "verified".
Some upstream behaviour comes from packages that are not in the clone. Their published tarballs were
read, not run:

- `@dcl/http-commons@2.0.0`: `errorHandler`, `getPaginationParams`, the error classes;
- `@dcl/schema-validator-component@1.0.1`: the 415/400 bodies;
- `@dcl/crypto-middleware@6.3.0`: `wellKnownComponents`, `verify`, `rejectIfSigner`,
  `assertLegacyMetadataKeys`.

A row that depends on one of them names the package.

Files read:

- Upstream:
  - `src/controllers/routes.ts`
  - `src/controllers/handlers/permissions-handlers.ts`
  - `src/controllers/schemas/permission-parcels-schema.ts`
  - `src/logic/permissions/{component,errors,types,constants}.ts`
  - `src/logic/access/{component,errors,types,constants}.ts`
  - `src/logic/access-checker/component.ts`
  - `src/logic/access-change-handler/{component,utils,types}.ts`
  - `src/logic/participant-kicker/{component,constants}.ts`
  - `src/logic/coordinates/component.ts` (`canonicalizeParcels`)
  - `src/logic/worlds/component.ts` (`isWorldValid`, `isWorldBlocked`)
  - `src/adapters/name-deny-list-checker.ts` (`BLOCKED_WORLD_NAMES`, `checkNameDenyList`, `getBannedNames`)
  - `src/adapters/permissions-manager.ts`
  - `src/adapters/dcl-name-checker.ts`
  - `src/adapters/worlds-manager.ts` (`getRawWorldRecords`, `modifyAccessAtomically`)
  - `src/components.ts`
  - `src/migrations/0020_create_world_permissions_table.ts`
- Upstream tests:
  - `test/integration/permissions/*.spec.ts` (all eleven files)
  - `test/integration/explorer-legacy-payload.spec.ts`
  - `test/integration/strict-scene-signer-gate.spec.ts`
  - `test/integration/deploy-parcel-permission.spec.ts`
  - `test/unit/access-component.spec.ts`
  - `test/unit/access-change-handler.spec.ts`
  - `test/unit/access-checker-component.spec.ts` (descriptions)
  - `test/unit/permissions-component.spec.ts` (descriptions)
  - `test/unit/worlds-component.spec.ts`, `test/unit/name-deny-list-checker.spec.ts`
- dcl-one (`services/catalyst/`):
  - `worlds/src/core/permissions.rs`
  - `worlds/src/core/auth.rs`
  - `worlds/src/core/http.rs`
  - `worlds/src/core/mod.rs` (routes, `Revocation`, `revoked`, `revoke_unless_admitted`)
  - `worlds/src/core/store.rs` (`AccessSetting` and the persistence facade)
  - `worlds/src/core/store_queries.rs` (`get_world`, `world_is_valid`, `raw_records_exclude`)
  - `worlds/src/core/store_access.rs` (`permission_records`, grant, parcel and allow-list writers)
  - `worlds/src/core/notices.rs` (`permission_changed`)
  - `worlds/src/core/names.rs` (`BlockedNames`, `BannedNames::allows`, `banned_names`)
  - `worlds/src/mirror_upstream.rs` (`upstream_access`, `parse_upstream_access`; the mirrored world's access setting)
  - `worlds/src/policy.rs` (`member_communities`, the access rules table)
  - `worlds/src/writes.rs`
  - `worlds/src/lib.rs` (`app_router`)
  - `worlds/src/core/tests.rs` (the permission tests named below)
  - `comms/src/signed_fetch.rs`
  - `comms/src/worlds_envelope.rs`
  - `comms/src/world_identity.rs` (`Parcel::parse_strict`)
  - `comms/src/gatekeeper/revocation.rs`
  - `shared/src/identity.rs` (`Address::parse`)
  - `../../library/dcl-auth/src/signed_fetch.rs` (`is_refused_signer`, `field_is_canonical`)
  - `../../schemas/20_worlds.sql` (`world_permissions`, `world_permission_parcels`, `world_access_communities`)
  - `docs/worlds-compatibility.md`

Common notes:

- **Mount points.** Upstream serves the routes at its root. dcl-one serves the ten routes at the root
  (`core::root_router`) and again under `/worlds-content-server` (`core::router`), both from
  `timed_routes()` (`services/catalyst/worlds/src/core/mod.rs:640-668`, nested in
  `services/catalyst/worlds/src/lib.rs:428-432`). The signed path is the bare route path on both mounts
  (`SignedPath::from_uris`; test `owner_gated_permission_routes_verify_the_bare_path_through_the_gateway_nest`,
  `worlds/src/core/tests.rs:4620`).
- **Error envelope.** Upstream `@dcl/http-commons` `errorHandler` answers `InvalidRequestError` as
  `400 {"error":"Bad request","message":…}`, `NotFoundError` as `404 {"error":"Not Found",…}`,
  `NotAuthorizedError` as `401 {"error":"Not Authorized",…}`, and anything else as
  `500 {"error":"Internal Server Error"}`. dcl-one's `envelope` uses the same labels
  (`services/catalyst/comms/src/worlds_envelope.rs:124-139`), and adds a 503 with `Retry-After: 1`
  (`worlds_envelope.rs:197-203`). The signed-fetch and schema-validator refusals do not go through
  `errorHandler` upstream, so they have their own shapes (WP-01, WP-02).
- **Signed fetch.** Upstream builds every signed route from `createSignedFetchMiddleware`
  (`decentraland/worlds-content-server:src/controllers/routes.ts:74-96`):
  - `optional:false`;
  - the scene gate `rejectIfSigner('decentraland-kernel-scene')`;
  - `onError → {error: err.message, message: 'This endpoint requires a signed fetch request. See ADR-44.'}`.

  `POST /world/:world_name/permissions/:permission_name` is the only route on the instance that accepts the
  folded pre-6.0.0 payload. Its canonical keys are `signer,type,secret,wallets,communities,nft`
  (`routes.ts:158-165`). The other six signed routes here use the strict instance (`routes.ts:98`).

  dcl-one's `require_verified` (`services/catalyst/comms/src/signed_fetch.rs:131-185`) runs the same
  stages in the same order:
  1. the chain;
  2. the timestamp, with a 60 s window;
  3. the metadata parse;
  4. the scene gate, which refuses a non-canonical or re-cased `signer` (`library/dcl-auth/src/signed_fetch.rs:185-208`);
  5. the 6.x signature;
  6. on a 401, and only when keys are declared: the declared-key guard, then the folded signature.

  `post_permissions` passes `PERMISSIONS_METADATA_KEYS`, which holds the same six keys
  (`services/catalyst/worlds/src/core/auth.rs:23-24`). Every other permission route passes `&[]`
  (`verify_signer`, `services/catalyst/worlds/src/core/permissions.rs:175-186`). The statuses and bodies
  match (WP-01).
- **Owner check.** Upstream `checkOwnership` asks `namePermissionChecker.checkPermission(signer, world)`, which is
  `nameOwnership.findOwners` (`src/adapters/dcl-name-checker.ts:9-17`). A refusal is
  `401 Your wallet does not own "<world>", you can not set access control lists for it.`
  (`permissions-handlers.ts:41-48`). dcl-one's `owner_decision` asks `Policy::is_owner` and answers the same
  401 text (`permissions.rs:152-173`). An authority that cannot answer is a 503 here, where upstream answers
  500 (WP-12).
- **Writes switch (dcl-one only).** With `WORLDS_WRITES_ENABLED` off (the default), every `POST`/`PUT`/`DELETE`
  on these routes answers `503 {"ok":false,"error":"Service Unavailable","message":…}` with `Retry-After: 3600`.
  The one exception is `POST .../permissions/:permission_name/parcels`, which is a read
  (`services/catalyst/worlds/src/writes.rs:33-53`). It is a deployment switch; see "dcl-one-only routes and
  behaviour".
- **CORS, NUL guard, 30 s timeout.** dcl-one wraps these routes in `cors_middleware`, `nul_guard_middleware`
  and a 30 s `request_timeout` (408 `request timed out`) (`core/mod.rs:588-605,710-718`). CORS belongs
  to the shared-HTTP area, and none of these is specific to permissions.
- **Invalid world names** answer `400 Invalid world name: …` on every route here
  (`permissions.rs:68-70`), after the signed fetch on the signed routes. Upstream has no such check. This
  is intended (worlds-compatibility.md "Invalid world names").

## Endpoint inventory

| # | method | upstream path (upstream `routes.ts`) | dcl-one handler (`worlds/src/core/permissions.rs`) | status |
|---|---|---|---|---|
| 1 | GET | `/world/:world_name/permissions` (`:255`) | `get_permissions` | DIVERGENT |
| 2 | POST | `/world/:world_name/permissions/:permission_name` (`:256-260`, legacy-payload instance) | `post_permissions` `:242` | DIVERGENT |
| 3 | GET | `/world/:world_name/permissions/:permission_name/address/:address/parcels` (`:264-267`) | `get_allowed_parcels_for_permission` | MATCH |
| 4 | POST | `/world/:world_name/permissions/:permission_name/address/:address/parcels` (`:269-274`) | `post_permission_parcels` `:676` | DIVERGENT |
| 5 | DELETE | `/world/:world_name/permissions/:permission_name/address/:address/parcels` (`:276-281`) | `delete_permission_parcels` `:702` | DIVERGENT |
| 6 | POST | `/world/:world_name/permissions/:permission_name/parcels` (`:284-288`, unsigned) | `get_addresses_for_parcel_permission` `:791` | DIVERGENT |
| 7 | PUT | `/world/:world_name/permissions/access/communities/:communityId` (`:292-296`) | `put_permissions_access_community` `:848` | DIVERGENT |
| 8 | DELETE | `/world/:world_name/permissions/access/communities/:communityId` (`:297-301`) | `delete_permissions_access_community` `:876` | DIVERGENT |
| 9 | PUT | `/world/:world_name/permissions/:permission_name/:address` (`:304-308`) | `put_permissions_address` `:475` | DIVERGENT |
| 10 | DELETE | `/world/:world_name/permissions/:permission_name/:address` (`:311-315`) | `delete_permissions_address` `:520` | DIVERGENT |

Counts: 10 endpoints. MATCH 1, GAP 0, DIVERGENT 9, MISSING 0. Route 1 differs for mirrored worlds
(WP-21, intended: their access and grants). Every route exists in dcl-one at the same method
and path, on both mounts. On the success path, each route answers the upstream status and body shape.
Of the three unsigned routes (1, 3 and 6), route 3 differs only where `docs/worlds-compatibility.md` says
so; route 1 differs there and, for a mirrored world, as WP-21 records. The seven signed routes (six strict, one legacy) share the
signed-fetch error bodies (WP-01), and the three parcel routes the schema-validator bodies (WP-02). Each DIVERGENT route keeps at least one intended difference.

---

## 1. GET /world/:world_name/permissions

Upstream: `permissions-handlers.ts:50-107`. It reads three things in parallel:

- `access.getAccessForWorld`: the stored JSONB access, or `{type:"unrestricted"}` when the world has no row or
  the name is deny-listed (`access/component.ts:34-40`, `worlds-manager.ts:322-356`);
- `permissionsManager.getOwner`: the stored `worlds.owner` first, then `nameOwnership`; a lookup failure gives
  `undefined` (`permissions-manager.ts:18-32`);
- `permissions.getPermissionsSummary` (`permissions/component.ts:422-440`).

dcl-one: `permissions.rs:75-127`. It reads `store::get_world`, `store::permission_records`
(`store_queries.rs` `get_world`, `store_access.rs` `permission_records`) and `Policy::current_owner`, which is the live holder.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| World with grants | 200 | `{"permissions":{"deployment":{"type":"allow-list","wallets":[…]},"streaming":{"type":"allow-list","wallets":[…]},"access":{…}},"owner":"0x…","summary":{"0x…":[{"permission":"deployment","world_wide":true}]}}` | `:89-106` | `:112-126` | MATCH on the shape. `streaming.type` is always `allow-list` in both, even when streaming is unrestricted (no rows). A parcel-scoped deployer is listed in `deployment.wallets` in both (test "both world-wide and parcel-based deployers"). |
| `summary` entry | | `parcel_count` only when not world-wide | `permissions/component.ts:432-436`, handler `:82-86` | `:101-109` (`skip_serializing_if`) | MATCH. Key order: upstream follows the row order of an unordered `GROUP BY`; dcl-one sorts by address (`BTreeMap`, `ORDER BY wp.address, wp.permission`). Upstream defines no order. |
| Access `shared-secret` | 200 | `access: {"type":"shared-secret"}`, no `secret` | `removeSecrets` `:33-39` | `AccessSetting::to_public_json` `store.rs` `AccessSetting::to_public_json` | MATCH (tests "access settings without the secret") |
| Access `nft-ownership` | 200 | `{"type":"nft-ownership","nft":"…"}` | stored JSONB | `store.rs` `AccessSetting::to_public_json` | MATCH |
| Access `allow-list` | 200 | `{"type":"allow-list","wallets":[…],"communities":[…]}` | the JSONB as posted: case, order and duplicates kept (`access/component.ts:84-88`); PUT adds lowercased | wallets lowercased, deduplicated and ordered by `created_at, address`; communities as lowercase UUIDs | intended (worlds-compatibility.md "Allow-list documents", WP-05) |
| World unknown, NAME has a holder | 200 | defaults, `owner` the holder | `getOwner` → `nameOwnership` | `current_owner` | MATCH (test `permissions_owner_falls_back_to_the_name_holder`, `tests.rs:5666`) |
| World unknown, no holder | 200 | `owner` absent | `owner: undefined` is dropped by `JSON.stringify` | `PermissionsResponse::owner` `skip_serializing_if` | MATCH (WP-03; test `permissions_owner_falls_back_to_the_name_holder`) |
| Owner lookup fails | upstream 200 with `owner` absent; dcl-one 503 | | `permissions-manager.ts:25-31` | `current_owner` `?` (`permissions.rs:87`; test `permissions_answer_503_while_the_owner_is_undetermined`, `tests.rs:5687`) | DIVERGENT: intended (worlds-compatibility.md "Owner admission") |
| World with a stored owner whose NAME was sold | 200 | upstream: the **stored** owner; dcl-one: the live holder | `getOwner` reads `metadata.owner` first | `current_owner` | DIVERGENT: intended (same entry) |
| Invalid world name | upstream 200 defaults; dcl-one 400 | | none | `parse_world` | intended (worlds-compatibility.md "Invalid world names") |
| Deny-listed name | 200; access read as `unrestricted` when `getRawWorldRecords` filters the record out, summary still listed | | `access/component.ts:34-40`; `worlds-manager.ts:327,345-349`: `name != ALL(getBannedNames())`, the `BLOCKED_WORLD_NAMES` entries whole plus the list entries without their first `.eth`/`.dcl`, so a list entry leaves out only a bare name equal to it, a plain `<label>` entry nothing | `store::raw_records_exclude` (`store_queries.rs` `raw_records_exclude`) over `BannedNames::banned_names` (`names.rs:67-77`) | MATCH (WP-11, WP-20; tests `deleting_an_address_grant_requires_a_valid_world`, `configured_world_names_are_invalid_on_the_permission_routes`) |
| Mirrored world (`mirror_source` set) | 200; upstream reads the world's own rows. dcl-one: the access setting the source's `/permissions` reports, copied by every mirror pass and read-through fill unless the owner set the access here (`upstream_access` in `mirror_upstream.rs`, `mirror_world` in `mirror_reconcile.rs`; a shared secret stored as one no secret matches); `deployment.wallets`, `streaming.wallets` and `summary` list only grants made on this node, so they are empty | | `permissions-handlers.ts:50-107` | `get_permissions` over the stored rows | DIVERGENT (WP-21, intended; tests `tests/worlds_mirror.rs` `a_mirror_pass_copies_the_upstream_access_setting`, `an_access_setting_the_owner_made_here_is_kept_by_the_mirror`, `tests/worlds_read_through.rs` `a_fill_stores_the_upstream_access_setting`) |

## 2. POST /world/:world_name/permissions/:permission_name

Upstream: `permissions-handlers.ts:111-170`, behind the legacy-payload instance. Stage order:

1. signed fetch;
2. `checkOwnership`;
3. a switch on `permission_name`.

The logic is in `permissions/component.ts:224-313` (deployment, streaming) and `access/component.ts:52-133` (access).

dcl-one: `permissions.rs:242-326`. Stage order:

1. signed fetch with the six keys;
2. world name;
3. `owner_decision`;
4. the switch.

### Request

The metadata is the signed `x-identity-metadata` header; there is no body.

| field | upstream | dcl-one |
|---|---|---|
| `type` | Read as is. A missing `type` reads as `undefined`. | `meta.get("type").as_str()`, else `""` (`:236,256,319`). |
| `wallets` (deployment, streaming) | `authMetadata.wallets \|\| []`, lowercased, **not validated**. A non-string element throws a `TypeError`, which becomes a 500 (`permissions/component.ts:246,299`). | Only the string elements are read, and each must be an address, else `400 Invalid address: <w>.` (`:300-316`). |
| `wallets`, `communities` (access allow-list) | Stored as given. Only the lengths are checked against `ACCESS_MAX_WALLETS` (1000) and `ACCESS_MAX_COMMUNITIES` (50), which come from env (`access/component.ts:24-25,61-71`). | Each wallet must be an address and each community a UUID. The lengths are checked against `ACCESS_MAX_COMMUNITIES` then `ACCESS_MAX_WALLETS` (`access_from_metadata`, `:399-412`; `config/src/worlds.rs:134-150`, defaults 50 and 1000) (WP-09). |
| `nft` | Any truthy value. | Must be a string (`:323-325`). |
| `secret` | Any truthy value, hashed with bcrypt cost 10. A non-string truthy value makes `hashSync` throw, which becomes a 500. | Must be a non-empty string, hashed with bcrypt `DEFAULT_COST` (12) (`:330-341`). |

### Responses

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Success (any of the three) | 204 | empty | `:167-169` | `:297` | MATCH |
| Not signed | 400 | `{"error":"Invalid Auth Chain","message":"This endpoint requires a signed fetch request. See ADR-44."}` in both | crypto-middleware `extractAuthChain`, `onError` | `map_auth_error_api` (`comms/src/signed_fetch.rs`) | MATCH (WP-01) |
| Folded payload, canonical keys (creator-hub: `{type:"shared-secret",secret:"MyPassWord123"}`, `{type:"allow-list",wallets:[],communities:["B7B1E0D2-…"]}`) | 204 | | `routes.ts:158-165`; legacy spec | `PERMISSIONS_METADATA_KEYS` | MATCH on status. The upper-case community id is stored and returned in lower case here (WP-05). |
| Folded payload, re-cased declared key (`Secret`, `Type`, `Wallets`, `Communities`) | 400 | `error` `Invalid chain metadata: expected "<k>", got "<K>"` in both | `assertLegacyMetadataKeys` | `first_respelled_key` (`comms/src/signed_fetch.rs`) | MATCH (WP-01) |
| Scene signer, or `Signer` re-cased (folded or 6.x) | 400 | `error` `Invalid metadata content: <metadata JSON>` in both | `rejectIfSigner` | `is_refused_signer` | MATCH (WP-01) |
| Folded payload on `PUT .../:permission_name/:address` | 401 | `error` matching `/^Invalid signature:/` in both | strict instance | keys `&[]` | MATCH (WP-01) |
| Not the owner | 401 | `{"error":"Not Authorized","message":"Your wallet does not own \"<w>\", you can not set access control lists for it."}` | `:41-48,123` | `:135-154,231` | MATCH |
| Unknown permission name | 400 | `Invalid permission name: <p>.` (after the owner check) | `:151-153` | `:291-295` | MATCH |
| `deployment`, type not `allow-list` | 400 | `Invalid payload received. Deployment permission needs to be 'allow-list'.` | `permissions/component.ts:230-234` | `:237-241` | MATCH |
| `streaming`, type not `unrestricted`/`allow-list` | 400 | `Invalid payload received. Streaming permission needs to be either 'unrestricted' or 'allow-list'.` | `:276-280` | `:257-261` | MATCH |
| `access`, `type` missing | 400 | `Invalid access type: undefined.` | `access/component.ts:112-114` (template of `undefined`) | `js_string` | MATCH (WP-04; unit `an_invalid_access_type_prints_as_javascript_does`) |
| `access`, `type` invalid | 400 | `Invalid access type: invalid.`; a non-string prints as JS `String()` does (`7`, `true`, `[object Object]`) | | | MATCH (WP-04) |
| `shared-secret` without secret | 400 | `For shared secret there needs to be a valid secret.` | `:102-105` | `:330-337` | MATCH |
| `nft-ownership` without nft | 400 | `For nft ownership there needs to be a valid nft.` | `:95-98` | `:322-325` | MATCH |
| `allow-list`, more than the cap | 400 | `Too many communities. Maximum allowed is 50, but N were provided.` / `Too many wallets in allow-list. Maximum allowed is 1000, but N were provided.` | `:61-71` (communities checked first) | `access_from_metadata` (communities first; caps from `ACCESS_MAX_COMMUNITIES` / `ACCESS_MAX_WALLETS`) | MATCH (WP-09; unit `allow_list_caps_come_from_settings_communities_first`, test `access_allow_list_caps_from_settings_bound_every_route`) |
| `allow-list`, signer not a member | 400 | `You are not a member of the following communities: a, b.` | `access/errors.ts:15-19` | `require_community_member` `:378-404` | MATCH. Membership comes from upstream's social-service HTTP and from this node's `social.community_members` (`policy.rs:918-931`; social area). |
| `allow-list`, a wallet that is not an address, or a community that is not a UUID | upstream 204 (stored); dcl-one 400 `Invalid address: <w>.` / `Invalid community id.` | | none | `access_from_metadata` | intended (worlds-compatibility.md "Community ids", "Permission payloads"; WP-10) |
| World row missing | 204 and the row is created with the signer as owner | | `createBasicWorldIfNotExists` | `replace_world_wide_permission` / `store_access_for_owner` upsert (`store_access.rs` `replace_world_wide_permission` / `store_access_for_owner`) | MATCH (upstream tests "should create the world") |
| `access` on a mirrored world, or on a world the mirror later takes over | 204; the setting is stored and kept | | `storeAccess` (`access/component.ts:118`) | `store_access_for_owner` marks the row `access_set_here` by the signer; the mirror then keeps it while that owner holds the name as this node's `Policy::current_owner` says, never the source's `/permissions` owner, which trails a sale (`mirror_keeps_local`) | MATCH (WP-21; tests `an_access_setting_the_owner_made_here_is_kept_by_the_mirror`, `an_owner_edit_here_is_kept_while_that_owner_holds_the_name`). The allow-list edits of routes 7-10 mark it the same way (`modify_allow_list_access`). |
| Name authority unavailable | upstream 500 `{"error":"Internal Server Error"}`; dcl-one 503 + `Retry-After: 1` | | `dcl-name-checker.ts:14` throws | `owner_decision` | intended (worlds-compatibility.md "Owner admission", WP-12) |

### Side effects

| effect | upstream | dcl-one | verdict |
|---|---|---|---|
| Deployment/streaming replace | Two passes, not atomic: revoke the wallets no longer listed, grant the new ones. Rows already listed keep their parcel scoping. Unrestricted streaming deletes every streaming row (`permissions/component.ts:224-313`). | One statement (`store_access.rs` `replace_world_wide_permission`) with the same set semantics. | MATCH |
| Grant/revoke notifications | SNS `WORLDS_PERMISSION_GRANTED`/`REVOKED`, one per added or removed address. The description names the world; the URL is `${BUILDER_URL}/worlds?tab=dcl` (`permissions/component.ts:23-85`). | Inbox and email rows in the same transaction, at most 50 per direction. The description leaves the world out; the URL comes from the notices config (`permissions.rs:184-211`, `notices.rs:78-125`; test `permission_edits_notify_each_address_actually_changed`, `tests.rs:6593`). | DIVERGENT: intended (worlds-compatibility.md "Notifications") |
| `WORLD_SETTINGS_CHANGED` event on `access` | SNS `{type:"world", subType:"world-settings-changed", key:"<world>-<ts>", metadata:{worldName, accessType}}` after every successful `setAccess` (`access/component.ts:121-132`) | none | intended (worlds-compatibility.md "No settings events", WP-13) |
| Kicks after an access change | `handleAccessChange(prev, next)` (`access-change-handler/component.ts:118-187`). The first matching rule wins: (1) next is `unrestricted`: no kick. (2) The type changed, or the secret hash changed (a re-posted password always gets a new hash): kick every participant except the owner and the deployment-grant holders. (3) The allow-list wallets or communities changed: kick the non-privileged participants that `checkAccess` now refuses; one whose check fails is kept. (4) Otherwise no kick. Kicks run in batches of `ACCESS_KICK_BATCH_SIZE` (20), and failures are logged. | `Revocation::AccessSetting` re-decides every admission in the world's live rooms on a spawned task (`permissions.rs:285-289`, `core/mod.rs:361-367`, `comms/src/gatekeeper/revocation.rs:59-74`). The owner and every deployment **or streaming** grant holder stay. A switch to an allow-list keeps participants already on it. A shared-secret admission is void until the new secret is presented. | DIVERGENT: intended (worlds-compatibility.md "Kicks on access change"). Depends on the comms area (WP-14). |

## 3. GET /world/:world_name/permissions/:permission_name/address/:address/parcels

Upstream: `permissions-handlers.ts:294-366`, unsigned. Stage order:

1. `EthAddress.validate`;
2. `deployment`/`streaming` only;
3. `getPaginationParams` (limit 1..100, else 100; offset ≥ 0, else 0);
4. the bounding box (all four or none; `parseInt`);
5. `getParcelsForPermission`: `ORDER BY parcel`, inclusive box, corners in any order (`permissions-manager.ts:159-227`).

dcl-one: `permissions.rs:737-788`, `pagination` (`core/http.rs:27-41`), `store_access.rs` `parcels_for_permission`.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Parcel-scoped grant | 200 | `{"total":N,"parcels":["0,0",…]}` | `:353-359` | `:692` | MATCH (pagination and box: tests `parcel_listings_paginate_like_http_commons`, `tests.rs:5138`, and the unit `bounding_box_parses_like_parse_int`) |
| World-wide grant | 200 | `{"total":0,"parcels":[]}` | | `store_access.rs` `parcels_for_permission` | MATCH |
| Partial box | 400 | `Bounding box requires all four parameters: x1, y1, x2, y2.` | `:325-328` | `:647-651` | MATCH |
| Box value not an integer | 400 | `Bounding box parameters must be valid integers.` | `:335-337` | `:652-656` | MATCH |
| Invalid address | 400 | `Invalid address: <raw>.` | `:303-305` | `parse_path_address` (`^0x[a-fA-F0-9]{40}$`) | MATCH (WP-10: a padded or `0X` address is refused as upstream) |
| Permission not `deployment`/`streaming` | 400 | `Permission '<p>' does not support allow-list. Only 'deployment' and 'streaming' do.` | `:307-311` | `:556-567` | MATCH |
| No such grant | 404 | `Address <raw address> does not have <p> permission for world <world>.` | `PermissionNotFoundError` (`permissions/errors.ts:6-15`) | `get_allowed_parcels_for_permission` | MATCH (WP-06; test `a_missing_parcel_grant_names_the_address_as_sent`) |
| A grant whose last parcel was removed | upstream 200 `{"total":0,"parcels":[]}`, and the grant is now world-wide; dcl-one 404 | | `removeParcelsFromPermission` keeps the row | the row is deleted (`store_access.rs` `remove_parcels_from_permission`) | intended (worlds-compatibility.md "Removing a grant's last parcel") |

## 4. POST /world/:world_name/permissions/:permission_name/address/:address/parcels

Upstream: `permissions-handlers.ts:221-248`. Stage order:

1. strict signed fetch;
2. the `permissionParcelsSchema` validator: exactly `{parcels}`, 1..500 strings matching `^-?\d+,-?\d+$`
   (`schemas/permission-parcels-schema.ts`);
3. address;
4. permission;
5. owner;
6. `addParcelsToPermission`: the grant is created if it is missing, the parcels are canonicalized
   (`00,01` → `0,1`), and the grantee is notified only on creation (`permissions/component.ts:324-336`,
   `permissions-manager.ts:309-371`).

dcl-one: `post_permission_parcels`. Stage order:

1. signed fetch (`verify_signer`);
2. the body (`parcels_body`: content type, JSON, then `parcels_schema_error`, AJV's first error, built by
   `content::validation::array_property_error`);
3. address;
4. permission;
5. world;
6. owner;
7. `store::add_parcels_to_permission`.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Valid | 204 | empty | `:247` | `:598` | MATCH (canonical storage: unit `parcels_body_matches_the_upstream_schema`) |
| Grant already world-wide | 204; the grant becomes parcel-scoped | | | | MATCH |
| Body fails the schema (empty, more than 500, extra key, bad pattern) | 400 | `{"ok":false,"message":"Invalid JSON body","data":[<ajv error>]}`, keys in the validator's order | schema-validator-component `withSchemaValidatorMiddleware` | `parcels_body`; unit `parcels_body_matches_the_upstream_schema` (AJV 8 answers, run against the schema); test `parcel_bodies_are_schema_validated_on_every_parcel_route` | MATCH (WP-02) |
| Content-Type not JSON (`application/json` or `*/*+json` accepted) | 415 | `{"ok":false,"message":"Content-Type must be application/json"}` | | `content::validation::is_json_content_type` | MATCH (WP-02) |
| Body not JSON | 400 | `{"ok":false,"message":"<JSON.parse message>"}`: V8's text for an empty body, a `{` not followed by a key, and a short source with a bad first token; serde_json's for any other | | `content::validation::json_parse_message`; unit `parcels_body_matches_the_upstream_schema` | MATCH (WP-02) |
| A coordinate outside i32 (`99999999999,0`) | upstream 204 (stored); dcl-one 400 `Invalid parcels: …` | | pattern only | `ParcelError::Malformed` | intended (worlds-compatibility.md "Permission payloads", WP-10) |
| Unsigned request with a bad body or path | 400, the signed-fetch refusal (the signature is checked first) | | middleware order (`routes.ts:269-274`) | `verify_signer` first | MATCH (WP-07, WP-01; test `owner_gated_permission_routes_check_the_signature_first`) |
| Invalid address / permission / not owner | 400 / 400 / 401 with the upstream texts | | `:231-241` | `:556-567,581-588` | MATCH |
| World row missing | 500 in both (`world_permissions` has an FK to `worlds`) | | `0020_create_world_permissions_table.ts:20-23` | `schemas/20_worlds.sql:188-189` | MATCH |

## 5. DELETE /world/:world_name/permissions/:permission_name/address/:address/parcels

Upstream: `permissions-handlers.ts:254-288`. It has the same middleware and stage order as route 4, then
`removeParcelsFromPermission` (`permissions/component.ts:347-362`, `permissions-manager.ts:376-397`).

dcl-one: `permissions.rs:702-735`, `store_access.rs` `remove_parcels_from_permission`.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Valid | 204 | | `:287` | `:639` | MATCH |
| Every parcel removed | 204. Upstream keeps the row, which now reads as **world-wide** ("keep the permission record with zero parcels"). dcl-one deletes the grant and notifies `worlds_permission_revoked`. | | `permissions-manager.ts:376-397` | `store_access.rs` `remove_parcels_from_permission`, `core/mod.rs` `revoked` | DIVERGENT: intended (worlds-compatibility.md "Removing a grant's last parcel", "Last parcel removed"). Keeping this is right: upstream's behaviour widens a scoped grant. |
| No such grant | 400 | `Permission not found. Address <raw address> does not have <p> permission for world <world>.` | `permissions/component.ts:355-359` | `delete_permission_parcels` | MATCH (WP-06; test `a_missing_parcel_grant_names_the_address_as_sent`) |
| Schema / content type / ordering | as route 4 | | | | as route 4 |
| Invalid address / permission / not owner | as route 4 | | | | MATCH |

## 6. POST /world/:world_name/permissions/:permission_name/parcels

Upstream: `permissions-handlers.ts:372-396`, **unsigned**. Stage order:

1. the schema validator;
2. permission;
3. pagination;
4. `getAddressesForParcelPermission` (`permissions-manager.ts:404-453`), which matches world-wide holders or a
   parcel in `ANY($parcels)`, ordered by address. The **raw** parcels are used, not canonicalized.

dcl-one: `permissions.rs:791-820`, `store_access.rs` `addresses_for_parcel_permission`. It also stays open while writes are off
(`writes.rs:45-52`).

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Valid | 200 | `{"total":N,"addresses":[…]}` | `:389-395` | `:722` | MATCH (pagination: `tests.rs:5138`) |
| Nothing matches | 200 | `{"total":0,"addresses":[]}` | | `store_access.rs` `addresses_for_parcel_permission` | MATCH |
| Non-canonical query parcels (`00,00`) | upstream: no match against the stored `0,0`; dcl-one: a match | | raw `parcels` | `parcels.canonical()` | intended (worlds-compatibility.md "Parcel holder lookups", WP-08) |
| Permission `access` or unknown | 400 | `Permission '<p>' does not support allow-list. Only 'deployment' and 'streaming' do.` | `:378-382` | `get_addresses_for_parcel_permission` | MATCH, `access/parcels` included (WP-15) |
| Schema / content type | 400 / 415 | as route 4 | | | as route 4: MATCH (WP-02) |

## 7. PUT /world/:world_name/permissions/access/communities/:communityId

Upstream: `permissions-handlers.ts:457-489`. Stage order:

1. strict signed fetch;
2. a blank id is `400 Invalid community id.`;
3. owner;
4. `addCommunityToAccessAllowList`: membership first, then the row lock and the allow-list check, then the cap
   (`access/component.ts:202-231`).

No kick follows, since adding cannot revoke. Upstream registers the route before
`:permission_name/:address` (`routes.ts:290-301`).

dcl-one: `put_permissions_access_community`. Stage order:

1. signed fetch;
2. `parse_community` (UUID, trimmed);
3. world;
4. owner;
5. membership;
6. `modify_allow_list_access` (`store_access.rs` `modify_allow_list_access`).

axum's matcher puts the static `access` segment ahead of `{permission_name}`.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Added, or already listed | 204 | | `:488` | `:748` | MATCH |
| Signer not a member | 400 | `You are not a member of the following communities: <id>.` | | `:743` | MATCH |
| World not `allow-list` (or no row) | 400 | `World "<w>" does not have allow-list access type. Cannot add or remove wallets.` | `access/errors.ts:22-27` | `:792-795` | MATCH |
| At 50 communities | 400 | `Too many communities. Maximum allowed is 50, cannot add more.` | `:223-227` | `:744-746` | MATCH |
| Blank id (`%20`) | 400 | `Invalid community id.` | `:469-471` | `:408-410` | MATCH (test `tests.rs:4162`) |
| Id that is not a UUID (`test-community-id`, as upstream's own tests use) | upstream 204; dcl-one 400 `Invalid community id.` | | any string | `Uuid::parse_str` | intended (worlds-compatibility.md "Community ids"). It only breaks a client whose community ids are not UUIDs; Decentraland's social service issues UUIDs. |
| Unsigned request with a bad id | 400, the signed-fetch refusal | | | `verify_signer` first | MATCH (WP-07, WP-01) |

## 8. DELETE /world/:world_name/permissions/access/communities/:communityId

Upstream: `permissions-handlers.ts:495-523`. Stage order:

1. signed fetch;
2. blank id;
3. owner;
4. `removeCommunityFromAccessAllowList`;
5. `handleAccessChange`: rule (3), kick the participants without access (`access/component.ts:241-251`).

dcl-one: `permissions.rs:876-900`. It applies the edit, then `Revocation::AccessSetting`.

| trigger | status | upstream | dcl-one | verdict |
|---|---|---|---|---|
| Removed, or not listed | 204 | `:522` | `:781` | MATCH |
| World not `allow-list` | 400, the same text | | `:792-795` | MATCH |
| Blank id / non-UUID id / unsigned with a bad id | as route 7 | | | as route 7 |
| Kick | Upstream kicks the non-privileged participants that `checkAccess` now refuses, and only if the list changed. dcl-one re-decides the world's admissions after every applied edit. | | `:776-780` | intended; depends on comms (WP-14) |

## 9. PUT /world/:world_name/permissions/:permission_name/:address

Upstream: `permissions-handlers.ts:177-215`. Stage order:

1. strict signed fetch;
2. address;
3. `deployment`/`streaming`/`access`;
4. owner;
5. deployment/streaming: `grantWorldWidePermission(…, owner)`, which creates the world row, inserts the grant,
   **clears any parcel scoping** and notifies only a new grantee;
6. access: `addWalletToAccessAllowList`, which creates the world row, then refuses a world that is not an
   allow-list, adds the wallet lowercased and checks the cap.

dcl-one: `put_permissions_address`. Stage order:

1. signed fetch;
2. address (`parse_path_address`);
3. permission;
4. world;
5. owner;
6. `grant_addresses_world_wide_permission` (`store_access.rs` `grant_addresses_world_wide_permission`) or `ensure_world` + `AllowListEdit::AddWallet`.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| New or existing grantee | 204, empty | | `:214` | `:461` | MATCH (parcels cleared: `store_access.rs` `grant_addresses_world_wide_permission`) |
| `access`, world is an allow-list | 204; the wallet stored lowercased | | `access/component.ts:145-169` | `store_access.rs` `modify_allow_list_access` | MATCH |
| `access`, world not an allow-list (the row is created first in both) | 400 `World "<w>" does not have allow-list access type. Cannot add or remove wallets.` | | | `:456-459,792-795` | MATCH |
| `access`, at 1000 wallets | 400 `Cannot add wallet: allow-list would exceed the maximum of 1000 wallets.` | | `:160-165` | `:456-458` | MATCH |
| Invalid address | 400 `Invalid address: anything.` | | `:190-192` | `parse_path_address` | MATCH (padded or `0X` addresses refused, WP-10) |
| Invalid permission name | 400 `Invalid permission name: <p>.` | | `:194-196` | `:425-429` | MATCH |
| Not owner | 401, the upstream text | | | | MATCH |
| Folded payload | 401 | | strict instance | keys `&[]` | MATCH (WP-01) |
| Unsigned request with a bad address or permission | 400, the signed-fetch refusal | | | `verify_signer` first | MATCH (WP-07, WP-01) |
| `PUT .../deployment/parcels`, `PUT .../access/communities` | 400 `Invalid address: parcels.` / `… communities.` | upstream's router falls through to this route | `{permission_name}/parcels` hands PUT and DELETE to it (`put_permissions_parcels_address`); `access/communities` backtracks | MATCH (WP-15; test `permission_paths_between_the_static_and_named_routes_route_like_upstream`) |
| Notification | Upstream sends `WORLDS_PERMISSION_GRANTED` only to an address that was actually inserted, via SNS. dcl-one sends the same notice through its inbox. | | | `:451` | intended (Notifications) |

## 10. DELETE /world/:world_name/permissions/:permission_name/:address

Upstream: `permissions-handlers.ts:403-450`. Stage order:

1. strict signed fetch;
2. address;
3. permission, with the message naming three permissions;
4. owner;
5. `worlds.isWorldValid`, else `404 World "<w>" not found.`;
6. deployment/streaming: `revokePermission`, which deletes the grant (cascading to its parcels) and notifies a
   real removal;
7. access: `removeWalletFromAccessAllowList` + `handleAccessChange`.

`isWorldValid` first asks `checkNameDenyList`, then passes when the `worlds` row exists, `getRawWorldRecords`'s
deny-list filter keeps it, and the world is not blocked past the two-day grace (`logic/worlds/component.ts:71-87`,
`worlds-manager.ts:322-356`). It does **not** require a deployed scene, even though the comment above it says it
does.

dcl-one: `delete_permissions_address` (`permissions.rs:520-565`). `store::world_is_valid` (`store_queries.rs` `world_is_valid`)
asks `NameDenyList::check_name_deny_list` first, then requires the row, that `getRawWorldRecords`'s deny-list
filter keeps it (`store::raw_records_exclude`), and no operator disable or quota block past the grace.

| trigger | status | body | upstream | dcl-one | verdict |
|---|---|---|---|---|---|
| Grant or wallet removed, or absent | 204, empty | | `:449` | `:515` | MATCH (parcel-scoped grants are dropped whole in both) |
| World row missing | 404 `World "<w>" not found.` | | `:428-432` | `:487-492` | MATCH |
| World row exists with no deployed scene (permissions set before the first deploy, or every scene undeployed) | 204 | | `isWorldValid` | `world_is_valid` | MATCH (WP-11; test `deleting_an_address_grant_requires_a_valid_world`, for `deployment` and `access`) |
| Deny-listed name | 404 when `checkNameDenyList` refuses the name (a `BLOCKED_WORLD_NAMES` entry equal to it, case-insensitively, or a list entry equal to it lowercased without its first `.eth` and first `.dcl`, so a plain `<label>` entry in any case refuses `<label>.dcl.eth`), or when `getRawWorldRecords` filters it out (the stored name equals a list entry with its first `.eth` and first `.dcl` removed). A bare world name has no owner, so its DELETE answers 401 first | | `component.ts:72-74`, `name-deny-list-checker.ts:45-59`; `getRawWorldRecords` filter | `check_name_deny_list` (`names.rs:123-125`, `BannedNames::allows` `:51-54`), then `raw_records_exclude` | MATCH (WP-11, WP-20; tests `deleting_an_address_grant_requires_a_valid_world`, `configured_world_names_are_invalid_on_the_permission_routes`) |
| Invalid permission name | 400 `Permission '<p>' does not support allow-list. Only 'deployment', 'streaming', and 'access' do.` | | `:420-424` | `:473-477` | MATCH |
| `access`, world not an allow-list | 400, the same text | | `access/component.ts:180-183` | `:505-511,792-795` | MATCH |
| `access` kick | Upstream: `handleAccessChange`, rule (3), over every participant. dcl-one: `revoke_unless_admitted` for the removed wallet only (`core/mod.rs:371-389`). The result is the same: only the removed wallet can lose admission. | | | `:513` | intended (WP-14) |
| Invalid address / unsigned ordering | as route 9 | | | | MATCH (WP-07, WP-10) |

---

## Explorer and client expectations

- **creator-hub** (decentraland-crypto-fetch 2.0.1) signs the folded payload on route 2 with
  `{type, secret}` or `{type, wallets, communities}` and no `signer`. The fallback and the declared keys match,
  so its password and allow-list dialogs get 204 here. Its other permission calls send no metadata and sign the
  6.x payload. An upper-case community id comes back lower-cased on the next `GET` (WP-05); its membership check
  passes when the community exists in this node's `social` schema.
- **Builder / CLI** use the 6.x payload on every route. Their error toasts read `message`, which on the
  signed-fetch refusals carries the ADR-44 line in both trees (WP-01).
- **Explorers** do not call these routes. They see the kicks (comms area).

## Backlog (WP-)

1. **WP-01 · all seven signed routes · signed-fetch refusal bodies.** Status: MATCH, no work.
   - Upstream: `{"error": <middleware message>, "message": "This endpoint requires a signed fetch request. See ADR-44."}`.
     The middleware messages are `Invalid Auth Chain`, `Invalid signature: <detail>`,
     `Expired signature: signature timestamp: …`, `Invalid metadata content: {…}` and
     `Invalid chain metadata: expected "<k>", got "<K>"`. A 5xx keeps `err.message`
     (`decentraland/worlds-content-server:src/controllers/routes.ts:89-92`; `@dcl/crypto-middleware@6.3.0`
     `dist/verify.js:69-93,161-205,285-312,331-333`).
     Pinned by `post-permissions.spec.ts:355-365,393-402`, `post-permissions-legacy-payload.spec.ts` and
     `explorer-legacy-payload.spec.ts:116-171`.
   - dcl-one: `map_auth_error_api` (`services/catalyst/comms/src/signed_fetch.rs:342`) answers
     `{error: <crypto-middleware 6.3.0 text>, message: <ADR-44 line>}` at the upstream status (worlds-comms.md
     WC-01). Pinned by the `worlds/src/core/auth.rs` tests (the key guard's `expected "<k>", got "<K>"`, the scene
     gate) and `worlds/src/core/tests.rs` `signed_routes_refuse_with_the_crypto_middleware_body`. The reason after
     `Invalid signature: ` is this node's verifier's (worlds-compatibility.md "Signed-fetch refusals").
   - Severity: none (closed).

2. **WP-02 · routes 4, 5, 6 · schema-validator bodies.** Status: MATCH, no work.
   - Upstream: `@dcl/schema-validator-component@1.0.1`:
     - a wrong content type: `415 {"ok":false,"message":"Content-Type must be application/json"}`;
     - unparsable JSON: `400 {"ok":false,"message":<JSON.parse message>}`;
     - a schema failure: `400 {"ok":false,"message":"Invalid JSON body","data":[ajv errors]}`.
   - dcl-one: `parcels_body` (`services/catalyst/worlds/src/core/permissions.rs:609-640`) answers the 415, the
     parse error through `content::validation::json_parse_message` (V8's text for the captured failures, serde's
     otherwise; `content/src/validation.rs:98-100`), and AJV 8's first error for `permissionParcelsSchema`
     (`parcels_schema_error`, `permissions.rs:645-660`, over `content::validation::array_property_error` and
     `object_keys_in_order`), written in the validator's key order by `ordered_refusal_json`. Pinned by the unit
     `parcels_body_matches_the_upstream_schema` (expected answers produced by running AJV 8 on the schema) and the
     test `parcel_bodies_are_schema_validated_on_every_parcel_route`. `Malformed` parcels (past i32) answer
     `Invalid parcels: …` (WP-10).
   - Severity: none (closed).

3. **WP-03 · route 1 · `owner` with no holder.** Status: MATCH, no work.
   - Upstream: the key is absent (`permissions-handlers.ts:103`; `get-permissions.spec.ts:82-85,97-100` expect
     `toBeUndefined()`).
   - dcl-one: `PermissionsResponse::owner` is `skip_serializing_if = "Option::is_none"` (`permissions.rs:62`);
     the ts export is `owner?: string` and the sites schema `.nullish()`. Pinned by the test
     `permissions_owner_falls_back_to_the_name_holder`.
   - Severity: none (closed).

4. **WP-04 · route 2 · access `type` missing or not a string.** Status: MATCH, no work.
   - Upstream: `Invalid access type: undefined.`, or `String(type)` for a number or boolean
     (`access/component.ts:112-114`; pinned by `post-permissions.spec.ts:124-138`).
   - dcl-one: `js_string` (`permissions.rs:348`) prints the value as JS does: missing is `undefined`, an array
     joins with `,`, an object is `[object Object]`. Pinned by the unit
     `an_invalid_access_type_prints_as_javascript_does`.
   - Severity: none (closed).

5. **WP-05 · routes 1, 2, 7, 9 · the allow-list document is normalized.** Status: intended (worlds-compatibility.md).
   - Upstream: `POST .../access` stores `wallets` and `communities` as sent (case, order, duplicates).
     `GET .../permissions` returns them as stored (`access/component.ts:84-88`). An upper-case community id
     survives (`post-permissions-legacy-payload.spec.ts:108-113`).
   - dcl-one: the wallets are parsed to lowercase addresses and deduplicated, the communities stored as `uuid`
     (the schema CHECKs need both). GET returns lowercase wallets ordered by `created_at, address` and lowercase
     UUIDs (`store::AccessSetting::to_public_json`, `store.rs` `AccessSetting::to_public_json`). worlds-compatibility.md "Allow-list
     documents".
   - Severity: none (closed).

6. **WP-06 · routes 3, 5 · grant-not-found messages.** Status: MATCH, no work.
   - Upstream: route 3: 404 `Address <address as sent> does not have <p> permission for world <world>.`.
     Route 5: 400 `Permission not found. Address <address as sent> does not have <p> permission for world <world>.`
     (`permissions/errors.ts:12`, `permissions/component.ts:355-359`; pinned by `get-parcels.spec.ts:150-158` and
     `delete-parcels.spec.ts:137-151`).
   - dcl-one: the same texts, echoing the path address as sent (`get_allowed_parcels_for_permission`,
     `delete_permission_parcels`). Pinned by the test `a_missing_parcel_grant_names_the_address_as_sent`.
   - Severity: none (closed).

7. **WP-07 · routes 2, 4, 5, 7, 8, 9, 10 · the signature is checked first.** Status: MATCH, no work.
   - Upstream: the signed-fetch middleware, then the schema validator, run before the handler, so an unsigned or
     badly signed request with a bad body, address, permission or community id answers the signature error first
     (`routes.ts:256-314`). Upstream does not validate the world name.
   - dcl-one: each handler calls the signed fetch first (`verify_signer`, `permissions.rs:175-186`; route 2 calls
     `require_verified` with `PERMISSIONS_METADATA_KEYS`, `permissions.rs:249-258`), then the body, path, world
     name and owner. Pinned by the tests `owner_gated_permission_routes_check_the_signature_first` and
     `posting_a_permission_checks_the_signature_before_the_world_name`.
   - Severity: none (closed).

8. **WP-08 · route 6 · non-canonical query parcels.** Status: intended (worlds-compatibility.md).
   - Upstream: the query parcels go to SQL raw, so `00,00` never matches the canonical stored `0,0`
     (`permissions-manager.ts:404-426`).
   - dcl-one: canonicalized, so they match (`store::addresses_for_parcel_permission`, `store_access.rs` `addresses_for_parcel_permission`).
     worlds-compatibility.md "Parcel holder lookups".
   - Severity: none (closed).

9. **WP-09 · route 2 and the allow-list edits · caps and check order.** Status: MATCH, no work.
   - Upstream: `ACCESS_MAX_WALLETS` and `ACCESS_MAX_COMMUNITIES` come from config (defaults 1000 and 50).
     `setAccess` checks communities before wallets (`access/component.ts:24-25,61-71`).
   - dcl-one: the same names in `catalyst-config` (`services/catalyst/config/src/worlds.rs:134-150`, defaults 1000
     and 50, ranges 1..=10,000 and 1..=1,000) reach `Settings::with_access_caps` (`src/serve_states.rs` `worlds_states`,
     `worlds/src/core/mod.rs:169`). `access_from_metadata` checks communities first (`permissions.rs:399-412`);
     the single-item PUTs check `count >= cap` under the row lock (`store::modify_allow_list_access`,
     `store_access.rs` `modify_allow_list_access`). Pinned by the unit `allow_list_caps_come_from_settings_communities_first`, the test
     `access_allow_list_caps_from_settings_bound_every_route` (caps of 1 and 1 on POST and both PUTs) and the
     config test `access_allow_list_caps_keep_the_upstream_defaults_within_a_range`. The ranges are this node's
     (worlds-compatibility.md "Allow-list caps").
   - Severity: none (closed).

10. **WP-10 · routes 2, 3–10 · input checks.** Status: intended (worlds-compatibility.md).
    - Upstream:
      - deployment, streaming and access wallets are not validated (stored lowercased or as sent);
      - `nft` is any truthy value;
      - a non-string `secret` is a 500;
      - parcels are pattern-only, so `99999999999,0` is stored;
      - a path `:address` must match `^0x[a-fA-F0-9]{40}$` exactly.
    - dcl-one:
      - wallets must be addresses (`400 Invalid address: <w>.`); `nft` must be a string; a non-string `secret` is
        a 400; a coordinate outside i32 is `400 Invalid parcels: …` (worlds-compatibility.md "Permission
        payloads");
      - the path address is matched as upstream (`parse_path_address`, `permissions.rs:78-85`), so a padded or
        `0X` address is refused.
    - Severity: none (closed).

11. **WP-11 · route 10 (and route 1 for the deny list) · `isWorldValid` semantics.** Status: MATCH, no work.
    - Upstream: `checkNameDenyList` first (`logic/worlds/component.ts:71-74`; the name lowercased without its
      first `.eth` and first `.dcl` against each list entry lowercased, `name-deny-list-checker.ts:50-58`), then
      valid when the `worlds` row exists, `getRawWorldRecords`'s deny-list filter keeps it
      (`name != ALL(getBannedNames())`), and the world is not blocked past the two-day grace. A scene is **not**
      required (`logic/worlds/component.ts:71-87`, `adapters/worlds-manager.ts:322-356`). So a list entry `foo`
      or `FOO` makes the owner's DELETE on `foo.dcl.eth` answer 404 (unit `worlds-component.spec.ts:169`).
    - dcl-one: `store::world_is_valid` (`store_queries.rs` `world_is_valid`; its only caller is route 10, after the owner check,
      `permissions.rs:536`) asks `NameDenyList::check_name_deny_list` first (`names.rs:123-125`), then requires
      the row, applies the same filter (`store::raw_records_exclude`, `store_queries.rs` `raw_records_exclude`) and the operator
      disable and quota grace. Route 1 reads a filtered-out world's access as the default; a world only
      `checkNameDenyList` refuses keeps its stored access there, as upstream. Pinned by the test
      `deleting_an_address_grant_requires_a_valid_world`: a world with no scene answers 204 on the `deployment`
      and `access` branches (the latter revokes the removed wallet), a deny-listed world's DELETE answers 404
      (an upper-cased plain label entry included, while route 1 still reads the stored `nft-ownership`), a bare
      name 401.
    - Severity: none (closed).

12. **WP-12 · all owner-gated routes · name authority unavailable.** Status: intended (worlds-compatibility.md).
    - Upstream: `findOwners` throws, so `errorHandler` answers `500 {"error":"Internal Server Error"}`
      (`adapters/dcl-name-checker.ts:14`).
    - dcl-one: `503 {"error":"Service Unavailable","message":…}` with `Retry-After: 1` (`owner_decision`,
      `permissions.rs:159-173`). worlds-compatibility.md "Owner admission".
    - Severity: none (closed).

13. **WP-13 · route 2 (`access`) · no `WORLD_SETTINGS_CHANGED` event.** Status: intended (worlds-compatibility.md).
    - Upstream: after every successful `setAccess`, SNS publishes
      `{type:"world", subType:"world-settings-changed", key:"<world>-<ts>", timestamp, metadata:{worldName, accessType}}`
      (`access/component.ts:121-132`).
    - dcl-one: no event. In-tree readers read `worlds.worlds.access_type`; `queue_access_recheck` covers comms.
      worlds-compatibility.md "No settings events".
    - Severity: none (closed).

14. **WP-14 · routes 2, 8, 10 · who is kicked after an access change.** Status: intended (worlds-compatibility.md).
    - Upstream: the first matching rule decides (`access-change-handler/component.ts:118-187`):
      - next `unrestricted`: no kick;
      - the type changed, or the secret hash changed: kick everyone but the owner and the deployment holders;
      - the allow-list changed: kick whoever `checkAccess` refuses, keeping anyone whose check failed.

      The kicks go through `participantKicker` in batches of 20, and errors are logged.
    - dcl-one: `Revocation::AccessSetting` / `Address` (`worlds/src/core/mod.rs:211`, `revoke_unless_admitted`
      `:383`) hands the edit to the gatekeeper, which re-decides each live admission
      (`comms/src/gatekeeper/revocation.rs`). The owner and every deployment **or streaming** holder stay; a switch
      to an allow-list keeps anyone already listed; an undecidable participant stays. A same-type re-post also
      re-decides. worlds-compatibility.md "Kicks on access change"; the room-level behaviour is in the comms area.
    - Severity: none (closed).

15. **WP-15 · route tree · paths between the static `access` and `{permission_name}` routes.** Status: MATCH, no work.
    - Upstream: `POST /world/w/permissions/access/parcels` reaches route 6 (400 `does not support allow-list`,
      pinned by `get-addresses-for-parcel-permission.spec.ts:51-62`). `PUT /world/w/permissions/deployment/parcels`
      and `PUT /world/w/permissions/access/communities` reach route 9 (400 `Invalid address: parcels.` /
      `… communities.`).
    - dcl-one: matchit backtracks from `access/communities/{id}` for `access/parcels` and `access/communities`;
      `{permission_name}/parcels` hands PUT and DELETE to routes 9 and 10 with the address `parcels`
      (`put_permissions_parcels_address`, `permissions.rs:825-845`; routes `worlds/src/core/mod.rs:673-679`).
      Pinned by the test `permission_paths_between_the_static_and_named_routes_route_like_upstream`.
    - Severity: none (closed).

16. **WP-16 · route 2 · bcrypt cost.** Status: intended (worlds-compatibility.md).
    - Upstream: cost 10 (`access/constants.ts:15`; `post-permissions.spec.ts:84` reads `$2b$10$` from the stored
      value, which is not on the wire).
    - dcl-one: `bcrypt::DEFAULT_COST` (12) (`permissions.rs:392`). Hashes from either tree verify on the other.
      worlds-compatibility.md "Shared-secret hashes".
    - Severity: none (closed).

17. **WP-17 · routes 2, 4, 5, 9, 10 · notification transport and text.** Status: intended (worlds-compatibility.md).
    - Upstream: SNS `WORLDS_PERMISSION_GRANTED`/`REVOKED`, one per changed address; the description names the world;
      `url: ${BUILDER_URL}/worlds?tab=dcl` (`permissions/component.ts:23-85`).
    - dcl-one: inbox and email rows in the edit transaction, at most 50 per direction, with no world in the
      description; removing a grant's last parcel sends a revoke (`commit_with_notices`, `permissions.rs:212`;
      `notices.rs:78`). worlds-compatibility.md "Delivery", "Last parcel removed".
    - Severity: none (closed).

18. **WP-18 · route 5 · removing the last parcel.** Status: intended (worlds-compatibility.md).
    - Upstream: keeps the row, which becomes world-wide (`delete-parcels.spec.ts:86-126`).
    - dcl-one: deletes the grant (`store::remove_parcels_from_permission`, `store_access.rs` `remove_parcels_from_permission`), so a scoped deployer is
      never widened to the whole world; route 3 then answers 404 where upstream answers
      `{"total":0,"parcels":[]}`. worlds-compatibility.md "Removing a grant's last parcel".
    - Severity: none (closed).

19. **WP-19 · routes 1–10 · community ids must be UUIDs; owner is the live holder; invalid world names 400.**
    Status: intended (worlds-compatibility.md).
    - These are the worlds-compatibility.md entries "Community ids", "Owner admission" and "Invalid world names".
      None breaks a Decentraland client: the social service issues UUID community ids, and world names come from
      NAMEs.
    - Severity: none (closed).

20. **WP-20 · routes 1, 10 · `BLOCKED_WORLD_NAMES`.** Status: MATCH, no work.
    - Upstream: `BLOCKED_WORLD_NAMES` (`.env.default`, empty) is split on commas, trimmed, lowercased and
      stripped of empty entries (`name-deny-list-checker.ts:8-13`). `checkNameDenyList` refuses an exact match
      before it reads the list, also while the list cannot be read (`:45-49`), and `getBannedNames` returns the
      entries whole ahead of the stripped list entries (`:61-65`). So route 1 reads a blocked world's access as
      `{"type":"unrestricted"}` (`worlds-manager.ts:327,345-349`) and route 10 answers 404 through
      `isWorldValid` (`component.ts:72-74`).
    - dcl-one: the one `NameDenyList` every area shares holds the parsed entries (`BlockedNames::parse`,
      `names.rs:31-39`); `BannedNames::allows` and `banned_names` apply them as upstream (`names.rs:51-77`),
      so routes 1 and 10 follow through `raw_records_exclude` and `world_is_valid`. The setting itself is WR-35
      (worlds-reads.md). Pinned by the test `configured_world_names_are_invalid_on_the_permission_routes`
      (route 1 `unrestricted` and route 10 404 on a node blocking the upper-cased name; the stored
      `nft-ownership` and a 204 on a node that does not) and the unit tests in `names.rs`.
    - Severity: none (closed).

21. **WP-21 · routes 1, 2, 7-10 and comms admission · a mirrored world's access and grants.** Status:
    DIVERGENT (intended).
    - Upstream: the source reads the world's own rows: the access JSONB (`access/component.ts:34-40`), and the
      deployment and streaming grants with their summary (`permissions-handlers.ts:50-107`,
      `permissions/component.ts:422-440`). A shared-secret world admits a caller who presents the matching
      secret (`access-checker/component.ts:48-55`, `bcrypt.compare`); an allow-list admits a listed wallet or,
      for its communities, a member by the Decentraland social service's `getMemberCommunities`
      (`access-checker/component.ts:16-29`). The owner's `setAccess` and allow-list edits are stored and kept
      (`access/component.ts:52,118,148-242`). A replay of `globalpx.dcl.eth` on 2026-11-12 listed three
      deployment and three streaming wallets.
    - dcl-one, access: every mirror pass that reaches a world, changed scenes or not, and every read-through
      fill reads `{source}/world/{name}/permissions` and copies `permissions.access` onto the world while the
      row is the mirror's (`mirror_source` set, not disabled) and the setting is not one its owner made on
      this node: `upstream_access`, `parse_upstream_access` (`mirror_upstream.rs`) and
      `store::replace_mirrored_access` (`store_access.rs` `replace_mirrored_access`), inside the world's sync transaction or, for
      unchanged scenes, its own. Only a difference is written; it bumps `settings_version`, queues the
      access recheck and sends no notice. An unreadable answer or an unknown type keeps the stored setting:
      it fails the sync of a world with changed scenes and skips only the access copy for unchanged scenes.
      The owner's access writes here (route 2 `access`, the allow-list edits of routes 7-10) mark the row
      `access_set_here` with the signer as `access_set_by`, and no pass replaces a marked setting while that
      signer holds the name as this node's own owner lookup says (`Policy::current_owner`, asked by
      `mirror::name_holder` only for a marked row; `store::mirror_keeps_local`), also when a scheduled pass
      later takes over a row the owner created before any scene. The source's `/permissions` `owner` never
      decides: it is upstream's stored owner, refreshed by its 12-hourly job, so it trails a sale. Once this
      node names another holder, the mark is cleared and the source's setting is copied; an unknown holder
      keeps every mark. A world published here keeps its own
      setting. Pinned by `tests/worlds_mirror.rs`
      `a_mirror_pass_copies_the_upstream_access_setting` (allow-list copied, `Policy::can_access` admits the
      listed wallet and refuses another, a later shared-secret copied on an unchanged pass and refusing any
      secret, failures keeping the setting, a local publish kept) and
      `an_access_setting_the_owner_made_here_is_kept_by_the_mirror`, `tests/worlds_read_through.rs`
      `a_fill_stores_the_upstream_access_setting` (a fill stores an allow-list; an unknown type stores
      nothing), and the unit `upstream_access_is_copied_or_refused`.
    - What differs from upstream. The mirror applies this node's permission model and settings to the
      source's access; a world's own grants, secrets and allow-list members come with a data migration,
      not through the mirror:
      - a shared-secret world: the source does not publish its hash, so the copy is stored with one no secret
        matches (`NO_SECRET_MATCHES`, `mirror.rs`) and every secret is refused here, while the source
        admits the matching one;
      - an allow-list's communities are matched against this node's `social.community_members`
        (`Policy::is_listed_community_member`, `policy.rs:914`), not the Decentraland social service, so a member there
        who is not one here is refused;
      - which side wins: the mirror follows the source until the owner edits the access here, and from then
        on keeps the owner's setting and no longer follows the source;
      - grants: not copied, so `deployment.wallets`, `streaming.wallets` and `summary` are empty for a
        mirrored world. A copied grant would let its holder deploy or stream on this node on the source's
        word, which the mirror's trust rule refuses (`mirror.rs` module doc; `Policy::may_have_published`;
        `WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS` is the only opt-in, and only for a read-through signer).
    - Listed in `docs/worlds-compatibility.md` ("Mirrored worlds").
    - Severity: none (closed).

## Endpoints dcl-one serves in this area that upstream does not

No extra routes. Behaviour on the same routes that upstream lacks:

- **`WORLDS_WRITES_ENABLED` off (the default):** every permission edit answers
  `503 {"ok":false,"error":"Service Unavailable","message":"worlds writes are disabled on this node …"}` with
  `Retry-After: 3600`. `GET .../permissions`, `GET .../address/:a/parcels` and `POST .../:p/parcels` stay open
  (`services/catalyst/worlds/src/writes.rs:33-53`). A node meant as a drop-in replacement must turn it on.
- **408 `request timed out`** after 30 s on every permission route (`core/mod.rs:710-718`).
- **The persisted access recheck:** every access write queues `social.world_access_rechecks`
  (`store_access.rs` `queue_access_recheck`), so a kick that a LiveKit outage blocked is retried. Upstream's kick is fire-and-forget.
- **The operator disable** (`/admin/worlds/{name}/disable`) makes route 10 answer 404 (`store::world_is_valid`).
