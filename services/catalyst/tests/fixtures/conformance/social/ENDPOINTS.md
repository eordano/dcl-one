# Social family: endpoint enumeration

The exhaustive `social-service-ea` registration and per-operation review backlog
is [PARITY.md](PARITY.md), backed by [parity.json](parity.json). It counts 72 API
HTTP registrations, five API auxiliary operations, five uWS HTTP operations,
two WebSocket upgrades, 33 RPC methods and six JSON subscriptions at
`b6b2cefefdf727b2ed47ddb887e77d226a41eea6`: 123 separately tracked operations.
HTTP and RPC twins, streams, conditional bearer routes, framework health/CORS,
metrics and fallbacks each have their own row. Production captures listed below
cover only their recorded requests and do not certify the other operations.

Baseline = the production `*.decentraland.org` services; candidate = a Catalyst node
(`serve`), which mounts each service under the configured gateway prefix (`/social-api`, `/comms-gatekeeper`,
`/notifications`, `/badges`, `/profile-images`, `/camera-reel-service`).
The rpc-social-service-ea host is mounted at `/social-rpc`.
Captured by `tests/conformance/capture/social.mjs`; the request log is
`MANIFEST.md` (rendered from `manifest.json`), the fixtures sit in one
directory per service with the raw response (`<name>.raw.json` +
`<name>.raw.body`) beside each replay-format `<name>.json`.

Sources: social-api and rpc-social-service-ea are read from a fresh clone of
`github.com/decentraland/social-service-ea` at the pinned revision below (committed
2026-10-30). The captured fixtures were recorded against production running
`fb3dcb80f8cac53485696a5f7c94b720b20ae6c9` (the rpc-host `/status` `commitHash` on
2026-10-26); the routes that revision lacks (the friends, blocks, settings and voice
HTTP routes and `/v1/stream`, upstream #503-#507) have no capture. The other services were read at the revision each checkout had on
2026-09-25 (not pinned by local checkout; the SHA is what
`git -c safe.directory='*' -C <repo> show HEAD:<path>` resolved to):

| service | source |
| --- | --- |
| social-api, rpc-social-service-ea | `github.com/decentraland/social-service-ea` @ `b6b2cefefdf727b2ed47ddb887e77d226a41eea6`: `src/controllers/routes/http.routes.ts` (social-api, the API http-server), `src/controllers/handlers/http/*` and `schemas.ts`, `src/controllers/middlewares/{friends,voice-chats}-errors.ts`, `src/controllers/routes/uws.routes.ts` and `src/controllers/handlers/uws/{status,privacy,ws,stream-ws}-handler.ts` (rpc host, the uWS server), `src/adapters/stream-sessions/`, `src/logic/ws-admission/`, `src/components.ts` (both servers and the http-server status checks), `docs/openapi.yaml`, `docs/websocket-stream-protocol.md` |
| comms-gatekeeper | `github.com-decentraland/comms-gatekeeper` @ `105df71d2ff434e6d9a0c3385494dd7367158abd`: `src/controllers/routes.ts`, `src/logic/utils.ts` (metadata keys), `docs/openapi.yaml` |
| notifications | `github.com-decentraland/notifications-workers` @ `0f113fba0bfc33856b8e184474b99e9c28046370`: `inbox/src/controllers/routes.ts`, `docs/openapi.yaml` |
| badges | no pinned upstream checkout recorded; `src/social/badges/mod.rs` is a local route inventory, not external evidence |
| profile-images | `github.com-decentraland/profile-images` @ `e75cc9a6bfa7d21831632a7fa432e369270d218e` (`src/controllers/routes.ts`: the processor's `/status` and the bearer-gated `/schedule-processing`; the image reads are the bucket behind the edge) |
| camera-reel | `github.com-decentraland/camera-reel-service` @ `69947bc697ee712c1b4cb4560ca5307773c14157`: `src/api.rs`, `src/api/{get,upload,delete,update,docs}.rs`, `src/lib.rs` (`/health`) |
| request shapes | `catalyrst/crates/catalyrst-conformance/fixtures/explorer-boot/corpus.json` (the explorer's boot traffic: `HEAD /comms-gatekeeper/status`, `POST /get-scene-adapter`, `GET /users/{address}/bans`, `GET /social-api/v2/communities?search=&onlyMemberOf=true&offset=-100&limit=100`, `GET /v1/community-voice-chats/active`, `GET /v1/mutes?limit=100&offset=0`, `GET /scene-admin`, `GET /scene-bans`, `GET /categories`, `GET /users/{address}/{badges,preview}`, `GET /entities/{cid}/face.png`, `GET /api/users/{address}`, `GET /api/places/{uuid}/images`), and `catalyrst/crates/catalyrst-conformance/fixtures/notifications/*.json` |

Capturable = an unauthenticated GET/HEAD/OPTIONS, or a POST whose body only
reads, or a signed read the run's throwaway key can issue. "signed" means
ADR-44 signed fetch; the fixture then records the throwaway wallet's auth
chain (timestamp-bound, so a replay must re-sign) and the baseline answer is
the authenticated shape for a wallet nothing is stored under. Everything
that writes (POST/PUT/PATCH/DELETE with effect), needs a service bearer
token or an operator role, is listed as not capturable.

## social-api.decentraland.org (Catalyst: `/social-api`)

| method | path | params | capturable | fixtures |
| --- | --- | --- | --- | --- |
| GET | `/v1/communities` | `limit` 1-100, `offset` <= 100000, `search`, `minimal` (needs auth + search >= 3), `onlyMemberOf`, `onlyWithActiveVoiceChat`, `roles[]`; optional signed fetch | yes | `communities-list`, `-page2`, `-search`, `-only-voice-chat`, `-limit-zero` (200: limit 0 falls back to the default), `-limit-not-a-number` (200), `-401-minimal-unauth`, `-minimal-signed`, `-origin` (with an `Origin`: `access-control-allow-origin: *`, `vary: accept-encoding`) |
| POST | `/v1/communities` | multipart create | no: write | |
| GET | `/v1/communities/{id}` | optional signed fetch | yes | `community-by-id`, `-404-unknown`, `-bad-id` (400 "Invalid identifier") |
| PUT/PATCH/DELETE | `/v1/communities/{id}` | | no: write | |
| GET | `/v1/communities/{id}/members` | `limit`, `offset`, `onlyOnline`; optional signed fetch | yes | `community-members`, `-only-online`, `-404-unknown` |
| POST | `/v1/communities/{id}/members` | join | no: write | |
| DELETE/PATCH | `/v1/communities/{id}/members/{memberAddress}` | leave, role | no: write | |
| GET | `/v1/communities/{id}/bans` | signed, moderator | signed shape only | `community-bans-unauth` (400 "Invalid Auth Chain"), `-signed-not-moderator` (401) |
| POST/DELETE | `/v1/communities/{id}/members/{memberAddress}/bans` | | no: write | |
| GET | `/v1/communities/{id}/places` | `limit`, `offset`; optional signed fetch | yes | `community-places` |
| POST | `/v1/communities/{id}/places`, DELETE `.../places/{placeId}` | | no: write | |
| GET | `/v1/communities/{id}/posts` | `limit`, `offset`; optional signed fetch | yes | `community-posts` |
| POST | `/v1/communities/{id}/posts`; DELETE `.../posts/{postId}`; POST/DELETE `.../posts/{postId}/like` | | no: write | |
| GET | `/v1/communities/{id}/requests` | signed, owner/moderator | signed shape only | `community-requests-unauth`, `-signed-not-owner` (401) |
| POST | `/v1/communities/{id}/requests`; PATCH `.../requests/{requestId}` | | no: write | |
| GET | `/v1/communities/{address}/managed` | bearer `API_ADMIN_TOKEN`; registered only when the token is configured | error shape only | `managed-communities-401-no-bearer` |
| GET | `/v1/members/{address}/communities` | `limit`, `offset`; optional signed fetch | yes | `member-communities`, `-bad-address` (200 empty page) |
| POST | `/v1/members/{address}/communities` | community ids in the body; bearer `API_ADMIN_TOKEN`, registered only when the token is configured | no: bearer | |
| GET | `/v1/members/{address}/requests` | signed, own address | yes (signed) | `member-requests-unauth`, `-signed` |
| GET | `/v1/members/{address}/invites` | signed | yes (signed) | `member-invites-unauth`, `-signed-self` (400 "Users cannot invite themselves"), `-signed-other` (200 `{"data":[]}`) |
| GET | `/v1/referral-progress` | signed | yes (signed) | `referral-progress-unauth`, `-signed` |
| POST/PATCH | `/v1/referral-progress` | signed | no: write | |
| POST | `/v1/referral-email` | signed | no: write | |
| GET | `/v1/mutes` | `limit` (default 10), `offset`, `address`, `addresses[]`; signed | yes (signed) | `mutes-400-unauth` (400 "Missing authentication" shape), `mutes-signed` (the corpus form `limit=100&offset=0`) |
| POST/DELETE | `/v1/mutes` | `{"muted_address"}` body; signed | unsigned error shape only | `mutes-post-unauth`, `mutes-delete-unauth` (body `{}`: 400 "Invalid Auth Chain", the signed-fetch gate answers before the body is read) |
| GET | `/v1/community-voice-chats/active` | signed | yes (signed) | `voice-chats-active-unauth`, `-signed` |
| GET | `/v1/moderation/communities` | signed, global moderator | shape only | `moderation-communities-unauth`, `-signed-not-moderator` (403) |
| GET | `/v2/communities` | as v1, search honoured | yes | `communities-v2-list-search`, `-no-search` (200: search is not required unauthenticated) |
| GET | `/v2/communities/{id}` | | yes | `community-v2-by-id`, `-404-unknown` |
| GET | `/v2/communities/{id}/members` | | yes | `community-v2-members` |
| GET | `/v2/communities/{id}/bans` | signed | shape only | `community-bans-v2-unauth` |
| GET | `/v2/communities/{id}/requests` | signed | shape only | `community-requests-v2-unauth` |
| GET | `/v2/members/{address}/requests` | signed | shape only | `member-requests-v2-unauth` |
| GET | `/v2/communities/{id}/posts` | | yes | `community-posts-v2` |
| OPTIONS | any | CORS preflight | yes | `preflight-communities` |
| GET | `/v2/friends` | `limit`, `offset`; signed | no capture (#504) | local `social/src/rpc/http.rs:42`, `:347` |
| DELETE | `/v1/friends/{address}` | signed | no: write | local `social/src/rpc/http.rs:43`, `:540` (the path parameter is named `{peer}`, as the local `/v1/friends/{peer}/messages`) |
| GET | `/v2/users/{address}/mutual-friends` | `limit`, `offset`; signed | no capture (#504) | local `social/src/rpc/http.rs:44`, `:371` |
| GET | `/v1/users/{address}/friendship-status` | signed | no capture (#504) | local `social/src/rpc/http.rs:48`, `:398` |
| GET | `/v2/friendship-requests` | `direction` `received`/`sent`, `limit`, `offset`; signed | no capture (#504) | local `social/src/rpc/http.rs:52`, `:413` |
| POST | `/v1/friendship-requests` | `{address, message?}`; signed | no: write | local `social/src/rpc/http.rs:53`, `:463` |
| PATCH | `/v1/friendship-requests/{address}` | `{intention: accepted\|rejected\|cancelled}`; signed | no: write | local `social/src/rpc/http.rs:54`, `:502` |
| GET | `/v2/blocks` | `limit`, `offset`; signed | no capture (#505) | local `social/src/rpc/http.rs:58`, `:562` |
| POST | `/v1/blocks`; DELETE `/v1/blocks/{address}` | `{address}`; signed | no: write | local `social/src/rpc/http.rs:59-60`, `:586`, `:605` |
| GET | `/v1/blocking-status` | signed | no capture (#505) | local `social/src/rpc/http.rs:61`, `:623` |
| GET, PATCH | `/v1/social-settings` | PATCH body: at least one of the three settings; signed | GET no capture, PATCH no: write (#506) | local `social/src/rpc/http.rs:62`, `:657`, `:674` |
| GET | `/v1/private-messages-settings` | `addresses` (repeated, at most 50); signed | no capture (#506) | local `social/src/rpc/http.rs:66`, `:712` |
| POST | `/v1/private-voice-chats`; PATCH/DELETE `/v1/private-voice-chats/{callId}` | `{calleeAddress}`, `{intention: accepted\|rejected}`; signed | no: write | local `social/src/rpc/http.rs:70`, `:75`, `:749`, `:795`, `:827` |
| GET | `/v1/private-voice-chats/incoming` | signed | no capture (#507) | local `social/src/rpc/http.rs:71`, `:775` |
| POST | `/v1/community-voice-chats`; DELETE `/v1/community-voice-chats/{communityId}`; POST `.../participants`, DELETE `.../participants/{address}`; PUT/PATCH/DELETE `.../speakers/{address}`; POST `.../speak-requests`, DELETE `.../speak-requests/{address}` | signed | no: write | local `social/src/rpc/http.rs:79-108`, `:845-1015` |
| GET | `/health/live`, `/health/ready`, `/health/startup` | `@dcl/http-server` status checks (`src/components.ts`) | yes | `health-live` (200 `alive`, `text/plain`), `health-ready`, `health-startup` (200 `{"details":{"httpServer":{"status":"pass"}},"status":"pass"}`, `application/health+json`) |
| GET | unknown path | | yes | `unknown-path-404` |

Local routes with no production counterpart on this host, not captured:
`PATCH /v1/communities/{id}/posts/{postId}`, `/v1/friends`,
`/v1/friends/{peer}/messages`, `/v1/admin/communities[...]`,
`/social/communities/{id}/raw-thumbnail.png`, `/federation/communities/*`.

### The social RPC's HTTP twins (#504-#507)

These routes answer from the same operations as their RPC procedures
(`social/src/rpc/service/mod.rs` `ops`), so they write the same rows and publish
the same updates; `social/src/rpc/tests/social_api.rs` covers each route, the
signed-fetch gate, the body and path checks and a route/procedure parity case.
Refusals render `{ error, message }`: 400 `Bad request` for the path checks
and the handlers' own body checks with upstream's messages (`Invalid address`,
`You cannot block yourself`,
`direction must be one of: received, sent`, `At most 50 addresses can be
requested`, `Invalid callId`, ...), 403 `Forbidden`, 404 `Not Found`, 409
`Conflict` and 429 `Too Many Requests` as upstream's `friends-errors.ts` and
`voice-chats-errors.ts` map them (local `social/src/rpc/http.rs:130`,
`social/src/communities/http.rs:203`). A body that fails the schema answers 400
as `@dcl/schema-validator-component` 1.0.1 does: `{ ok: false, message:
"Invalid JSON body", data: [<the first AJV error, in AJV's evaluation order>] }`,
and a body that is not JSON `{ ok: false, message }`; a community id is checked
against upstream's `UUID_PATTERN` (local `social/src/rpc/http.rs:194-240`,
`:317`; `a_body_failing_the_schema_answers_as_the_schema_validator`). The served
`/social-api/openapi.json` carries the twins' paths, the schemas they reference
and the `SignedFetch` scheme from upstream `docs/openapi.yaml`
(`library/catalyst-api/openapi/communities.openapi.json`;
`the_served_openapi_documents_every_route`). Open differences:

- A body that is not JSON carries V8's message only when it is empty
  (`Unexpected end of JSON input`); any other parse error carries serde's
  wording (local `social/src/rpc/http.rs:241-247`).
- A friendship request's `createdAt` is the action's time, as the procedure's
  `accepted.created_at` (upstream: the friendship row's `created_at`, upstream
  `src/logic/friends/component.ts:552`).
- `PATCH /v1/social-settings` writes the row only: local Comms Gatekeeper reads
  the same table, so there is no remote privacy sync to order.
- A non-`DELETE` method on `/v1/community-voice-chats/active` answers 405 from
  the listing route, where upstream would treat `active` as a community id.
- `/v1/stream` (`RPC.md` has its rows): the per-address channel falling
  behind (256 events of any stream) also closes `Subscription overflow`, and the
  `Slow consumer` trigger counts the writer task's queue, not uWS's buffered
  amount, so a very small `WS_MAX_BACKPRESSURE` can trip on scheduling delay
  (local `social/src/rpc/stream.rs:311-356`, `:846-850`).
- `WS_IDLE_TIMEOUT_IN_SECONDS` is not read, and one `AUTH_WINDOW_SECS` stands
  for `WS_AUTH_TIMEOUT_IN_MS` and `WS_AUTH_SIGNATURE_EXPIRATION_IN_MS`.

### Community writes and notifications (read at `social-service-ea` @ `b6b2cef`)

The write routes are not capturable; their behaviour was read from the
upstream source (paths below are in `decentraland/social-service-ea` at
`b6b2cef`, notification rows in `decentraland/notifications-workers` at
`0f113fb`) and is pinned by the tests named per row (in
`social/src/communities/tests/upstream_writes.rs` unless stated).

| behaviour | upstream | local | tests |
| --- | --- | --- | --- |
| `PUT /v1/communities/{id}` `placeIds`: a JSON array of strings replaces the places, `[]` clears, an empty field changes nothing, anything else 400 "placeIds must be a valid JSON array"; only ids the community does not hold are ownership-checked (401 "The user {address} doesn't own all the places"); the answer carries `message: "Community updated successfully"` | `src/logic/community/communities.ts` `updateCommunity`, `fields-validator.ts`, `places.ts` `updatePlaces` | `social/src/communities/communities.rs:754`, `:822`, `write_support.rs:94` | `place_ids_replace_the_places_checking_only_the_new_ones`, `write_support.rs` `multipart_hands_the_validator_the_raw_case` |
| private to public accepts every pending request to join of a wallet the community does not ban, in the update's transaction | `communities.ts` `updateCommunity`, `src/adapters/communities-db.ts` `acceptAllRequestsToJoin` | `communities.rs:845` | `going_public_accepts_the_pending_requests_to_join_of_unbanned_wallets` |
| `PATCH /v1/communities/{id}/members/{address}` `role: "owner"` transfers ownership: owner only (401 "...doesn't have permission to transfer ownership in community {id}"), target a member (401 "The target user {address} is not a member of community {id}") holding a NAME (400 "The user {address} doesn't have any names"); the old owner becomes moderator | `src/logic/community/members.ts` `transferOwnership`, `roles.ts` `validatePermissionToTransferOwnership`, `communities-db.ts` `transferCommunityOwnership` | `social/src/communities/members.rs:709`, `:748` | `the_owner_role_transfers_ownership_to_a_named_member`, `tests.rs` `every_route_answers_its_contract` |
| `onlyMemberOf=true` lists by role (owner, moderator, the rest), then name; every other listing by rank | `src/logic/queries.ts` `withSearchAndPagination` (`sortBy: 'role'`) | `social/src/communities/queries.rs:351` | `only_member_of_lists_by_role_then_name` |
| `COMMUNITIES_PLACES_OWNERSHIP_CHECK` defaults to true: upstream always asks the places API (`PLACES_API_URL` is required); here the in-process catalogue and LAND authority answer | `src/logic/community/places.ts` `validateOwnership`, `src/adapters/places-api.ts` | `config/src/social.rs:31`, `social/src/communities/mod.rs:456` | `place_ids_replace_the_places_checking_only_the_new_ones` |
| `GET /v1/community-voice-chats/active`: a non-member's room carries the positions and worlds of the community's places and is listed only when it has one; a member's carries none; a community that bans the caller is not listed | `src/logic/community-voice/community-voice.ts` `getActiveCommunityVoiceChatsForUser`, `src/utils/places.ts` | `social/src/communities/voice.rs:55`, `queries.rs:1225`, `places.rs:126` | `a_non_member_sees_a_room_only_through_its_places_and_never_when_banned`, `tests.rs` `voice_status_derives_from_the_presence_rows` |
| bans rows: `bannedAt` with milliseconds and `Z`; v1 and v2 carry the caller's `friendshipStatus` | `communities-db.ts` `getBannedMembers`, `src/logic/community/utils.ts` `mapMembersWithProfiles` | `social/src/communities/dto.rs:266`, `members.rs:318` | `ban_rows_carry_millisecond_banned_at_and_the_friendship_status` |
| `isHostingLiveEvent` on the signed `GET /v1/communities/{id}` and `/v2/communities/{id}` (absent from the public read), `false` on create; read from explore's events (`community_id`, live list) in process | `src/logic/community/events.ts` `isCurrentlyHostingEvents`, `communities.ts` `getCommunity` | `social/src/communities/communities.rs:369`, `:458`, `:467` | `a_signed_detail_says_whether_the_community_hosts_a_live_event` |
| notification rows (`social.notifications`, an opted-out recipient skipped, one row per type, recipient and event key): `community_request_to_join_received` (owner and moderators), `_request_to_join_accepted`, `_invite_received`, `_post_added` (members but the author), `_member_removed` (kick), `_member_banned` (a member only, keyed per wallet), `_renamed` (every member), `_deleted` (members but the owner), `_deleted_content_violation` (the owner, when a global moderator deletes), `_ownership_transferred` (the new owner), `_voice_chat_started` (online members but the opener) | `src/logic/community/broadcaster.ts`, `requests.ts`, `posts.ts`, `members.ts`, `bans.ts`, `communities.ts`, `src/logic/community-voice/community-voice.ts`; `processor/src/logic/event-parser.ts` | `social/src/notifications/community.rs:108`, `communities/mod.rs:407`, `requests.rs:634`, `posts.rs:260`, `members.rs:660`, `:911`, `communities.rs:894`, `:1043`, `rpc/service/community_voice.rs:168` | `community_events_write_the_notification_rows_upstream_publishes`, `a_global_moderators_deletion_tells_the_owner_of_the_violation`, `the_owner_role_transfers_ownership_to_a_named_member`, `rpc/tests.rs` `community_voice_start_join_and_ban_semantics`, `a_started_room_carries_the_places_and_the_thumbnail` |
| `POST /v1/referral-email`: signed; 400 "You must have at least 100 accepted invites to set an email", "Email is required", "Email is too long", "Email contains invalid characters", "Invalid email format", then once per 24 hours ("Email can only be updated once per day. Last update was less than 24 hours ago for user: {address}"); 204, the lower-cased email kept in `social.referral_emails` | `src/logic/referral/referral.ts` `setReferralEmail`, `src/controllers/handlers/http/add-referral-email-handler.ts` | `social/src/communities/referral.rs:249`, `:270`, `schemas/40_social.sql:746` | `referral_email_is_checked_and_kept_once_a_day`, `referral.rs` `referral_emails_are_checked_in_upstreams_order` |

Remaining differences, with their reason:

- Referral checks that need a service or deployment value this node does
  not have: the deny list (`assertReferrerNotBanned` and `setReferralEmail`
  fetch `config.decentraland.org/denylist.json`), the same-IP rejection
  (`REFERRAL_MAX_IP_MATCHES` and `invited_user_ip`, deployment configuration
  with no value in the source), the `REFERRAL_REGISTRATION_DISABLED` 503 flag,
  `finalizeReferral` (login-day count in Redis, tier rewards through the
  rewards API, Slack alerts, and with it the `referral_invited_users_accepted`
  and `referral_new_tier_reached` notifications) and the marketing email
  `setReferralEmail` sends; the submitted email stays in
  `social.referral_emails` for the operator.
- The ownership transfer's NAME check reads the target's profile
  `hasClaimedName` (as create does); upstream asks the catalyst for owned names.
- Bans rows keep the local `bannedBy` and `reason` keys and every row with
  placeholder profile fields; upstream's v1 rows also carry
  `lastFriendshipAction` and `actingUser` and drop a wallet without a profile.
- The update answer's `data` is the community detail (role, members count,
  voice status) where upstream returns the stored row (`id`, `name`,
  `description`, `ownerAddress`, `privacy`, `visibility`, `active`).
- A failed ownership lookup (catalogue or LAND authority) answers 502 "failed
  to validate place ownership"; upstream's failed places API call is a 500.
- Notification rows are written before the response, not through a queue;
  `MEMBER_LEFT` writes none, as the processor has no row for it.
- Nothing here sets a referral to `tier_granted` (that is `finalizeReferral`,
  above), so `GET /v1/referral-progress` counts no accepted invite and
  `POST /v1/referral-email` always answers the 100-invites refusal.
- `POST /v1/referral-email` checks the body after the invites count, with
  `setReferralEmail`'s messages; upstream's `AddReferralEmailSchema`
  (`schemas.ts:179`: `email` required, an email-format string, no other key)
  refuses a malformed body first, with the schema validator's message.

## rpc-social-service-ea.decentraland.org (Catalyst: `/social-rpc`)

The uWS server (`src/controllers/routes/uws.routes.ts`). Every HTTP answer
of a wrapped handler, `/health/live` and the catch-all carries
`Access-Control-Allow-Origin: *`.

| method | path | params | capturable | fixtures |
| --- | --- | --- | --- | --- |
| GET (WS upgrade) | `/` | dcl-rpc over WebSocket; the first frame is the signed auth chain (`ws-handler.ts`) | the eight reads only, by hand (`social/src/rpc/tests/upstream_reads.rs`) | `_socket/reads.json` (10 calls of a wallet created for the run; behaviour beyond it: `RPC.md`) |
| GET | `/status` | | yes | `status` (`{"version","currentTime","commitHash"}`, all three volatile) |
| ANY | `/health/live` | | yes | `health-live` (200 `alive`, no content-type) |
| GET | `/v1/users/{address}/privacy-settings` | address lower-cased | yes | `privacy-settings-unknown` (200 `{"private_messages_privacy":"all"}` for an address without a settings row), `-bad-address` (400 `{"error":"Invalid address"}`) |
| GET (WS upgrade) | `/v1/stream` | plain JSON stream (#503); the signed-fetch chain as upgrade headers or the first frame, signed over `/v1/stream` | no capture | local `social/src/rpc/mod.rs:263`, `social/src/rpc/stream.rs:136` |
| GET | `/metrics` | Prometheus | no: operator | |
| ANY | unknown path | | yes | `unknown-path-404` (404 `{"error":"Not Found"}`, JSON) |

Local routes with no production counterpart on this host, not captured:
`/social-rpc/info`, `/social-rpc/health`.

## comms-gatekeeper.decentraland.org (Catalyst: `/comms-gatekeeper`)

Metadata rules (`src/logic/utils.ts`): the comms handshake (`oldValidate`)
needs a top-level `realmName`; the scene routes need `signer`
`decentraland-kernel-scene` (or `dcl:authoritative-server` for
`/scene-admin`), `realm.{hostname,serverName}`, `sceneId`, `parcel`;
`/private-messages/token` needs `signer` `dcl:explorer`; the community voice
routes and `/users/{address}/voice-chat-status` take the social service's
bearer token; `/bans`, `/users/{address}/warnings` and the ban writes need
the moderator role.

| method | path | params | capturable | fixtures |
| --- | --- | --- | --- | --- |
| GET | `/ping` | | yes | `ping` |
| GET, HEAD | `/status` | | yes | `status`, `status-head` |
| POST | `/get-scene-adapter` | signed, explorer metadata | yes (signed read: mints a LiveKit token, redacted) | `get-scene-adapter-unauth`, `-signed-no-realm-name` (401), `-signed` (200 `{"adapter":"livekit:wss://...?access_token=<<REDACTED:jwt>>"}`) |
| POST | `/get-server-scene-adapter` | signed, authoritative server | error shape only | `get-server-scene-adapter-unauth` |
| GET | `/scene-participants` | `pointer`, `realm_name` (default `main` with a pointer) | yes | `scene-participants`, `-default-realm`, `-world`, `-400-no-params` |
| POST | `/mute` | | no: write | |
| GET | `/scene-admin` | `admin`; signed as scene | yes (signed) | `scene-admin-unauth` (400 `{"ok":false,"message":"Invalid Auth Chain"}`), `-signed-wrong-signer` (400 "Invalid metadata content"), `-signed-scene` (200: the place's admin list for the Genesis Plaza scene) |
| POST/DELETE | `/scene-admin` | | no: write | |
| GET | `/scene-bans` | `limit` (default 20), `offset`; signed as scene, admin | shape only | `scene-bans-unauth`, `-signed-wrong-signer`, `-signed-scene` (401 "User does not have permission to list scene bans") |
| GET | `/scene-bans/addresses` | signed as scene, admin | shape only | `scene-bans-addresses-unauth`, `-signed-scene` |
| POST/DELETE | `/scene-bans` | | no: write | |
| GET/POST/PUT/DELETE | `/scene-stream-access` | signed as scene, admin | GET shape only | `scene-stream-access-unauth` |
| POST | `/livekit-webhook` | LiveKit signature | no: write | |
| GET | `/private-messages/token` | signed as explorer | unauth shape only: the signed form mints a token for the wallet | `private-messages-token-unauth` |
| PATCH | `/users/{address}/private-messages-privacy` | bearer | no: write | |
| GET | `/users/{address}/voice-chat-status` | bearer | error shape only | `voice-chat-status-401-no-token` |
| POST | `/private-voice-chat`; DELETE `/private-voice-chat/{id}` | bearer | no: write | |
| GET | `/users/{userAddress}/community-voice-chat-status` | bearer | not captured (same 401 shape as voice-chat-status) | |
| POST | `/community-voice-chat`, `/community-voice-chat/status` | bearer | no: write / bearer | |
| GET | `/community-voice-chat/active` | bearer | error shape only | `community-voice-chat-active-401-no-token` |
| GET | `/community-voice-chat/{communityId}/status` | bearer | error shape only | `community-voice-chat-status-401-no-token` |
| POST/DELETE/PATCH | `/community-voice-chat/{communityId}[/users/{address}[/speak-request\|speaker\|mute]]` | bearer | no: write | |
| POST | `/cast/streamer-token`, `/cast/watcher-token`, `/cast/presentation-bot-token` | | no: write (mint) | |
| GET | `/cast/generate-stream-link` | signed as scene, admin | not captured: generates a key | |
| GET | `/cast/stream-info/{streamingKey}` | | yes | `cast-stream-info-invalid-key` (400) |
| GET | `/cast/presenters` | signed as scene | shape only | `cast-presenters-unauth` (400) |
| PUT/DELETE | `/cast/presenters/{participantIdentity}` | | no: write | |
| GET | `/users/{address}/bans` | | yes | `user-bans`, `-unknown-address`, `-bad-address` (200 `{"data":{"isBanned":false}}` for every input) |
| POST/DELETE | `/users/{address}/bans`; POST `/users/{address}/warnings` | signed, moderator | no: write | |
| GET | `/users/{address}/warnings` | signed, moderator | shape only | `user-warnings-401-unauth`, `-signed-not-moderator` |
| GET | `/bans` | signed, moderator | shape only | `bans-401-unauth` |
| GET | `/worlds/{worldName}/parcels/{baseParcel}/users/{address}/ban-status` | bearer | error shape only | `world-ban-status-no-token` (401 "Authorization header is missing") |
| OPTIONS | any | CORS preflight | yes | `preflight-get-scene-adapter` |
| GET | unknown path | | yes | `unknown-path-404` |

## notifications.decentraland.org (Catalyst: `/notifications`)

| method | path | params | capturable | fixtures |
| --- | --- | --- | --- | --- |
| GET | `/status` | | yes | `status` |
| GET | `/notifications` | `from` (ms), `limit` 1-50 (default 20), `onlyUnread` (presence); signed | yes (signed) | `notifications-unauth` (400, the body `auth-error-production.json` pins), `-signed`, `-signed-filters`, `-expired-timestamp` (401), `-bad-signature` (401) |
| POST | `/notifications` | bearer `NOTIFICATIONS_GOVERNANCE_TOKEN` (`governance_*` types) or `NOTIFICATIONS_REWARDS_TOKEN` (`reward_*` types), unset = producer disabled; `[{type, address?, eventKey, metadata, timestamp, optOutScope?}]` -> 204, body up to 16 MiB, at most 10,000 items, `metadata` up to 16 KiB serialized, `timestamp` at most 5 minutes ahead of the server clock; 400 in the upstream Joi words (another prefix's or an unknown type: `Invalid notification type: <type>`), 401 `Authorization header is missing` / `Invalid authorization header`, 413, 503 while social writes are off. Id = v5 (namespace `3f8a52d4-9c1e-4b07-a62d-71e05c9318bb`) over the JSON `["<type>","<lowercased address>","<eventKey>"]`: a repeat updates `metadata` and `timestamp`, keeps the read state and emails only on first insert. Only `governance_announcement` may omit `address` (else `"[i].address" is required`), and then without `optOutScope`: a broadcast stored once (`social.broadcast_notifications`, id over `["<type>",null,"<eventKey>"]`), listed by `GET /notifications` for every wallet with `address: null` and per-wallet read state while younger than `NOTIFICATIONS_BROADCAST_HORIZON_DAYS` (90; pruned after, unlike upstream), never emailed | no: write | |
| PUT | `/notifications/read` | signed; a broadcast id records this wallet's read (`social.broadcast_reads`), counted in `updated` | no: write | |
| POST | `/notifications/email` | bearer `NOTIFICATION_SERVICE_TOKEN`; `{email, subject, content, title?, titleHighlight?}` -> 204 | no: write | |
| GET | `/subscription` | signed | yes (signed) | `subscription-unauth`, `-signed` (the default document; `address` volatile) |
| PUT | `/subscription`, `/set-email`, `/confirm-email` | | no: write | |
| GET | `/subscription/opt-outs/{scope}/{scopeId}` | scope `community`; signed | yes (signed) | `opt-outs-community-unauth`, `-signed`, `opt-outs-bad-scope-signed` (400) |
| POST | `/subscription/opt-outs`; DELETE `/subscription/opt-outs/{scope}/{scopeId}` | | no: write | |
| GET | `/unsubscribe/{address}[/{notificationType}]` | signed URL from the e-mail (HMAC `SIGNING_KEY` over the `NOTIFICATIONS_SERVICE_BASE_URL` origin, path and query); 200 `text/html` | error shape only | `unsubscribe-address-no-signature` (401 "Invalid URL.") |
| OPTIONS | any | CORS preflight | yes | `preflight-notifications` |
| GET | unknown path | | yes | `unknown-path-404` |

`social/src/notifications/tests.rs`
`the_notifications_host_answers_as_its_captures` replays `status`, the
three `-unauth` reads and `unknown-path-404`, and re-signs the five signed
reads with a fresh wallet. `subscription-signed` differs only in
`details.message_type.friend_first_wear` and `.land_transfer_received`
(Catalyst's own types, absent from upstream's `@dcl/schemas` list) and
`.item_discounted` (added upstream after the capture). Not
replayed in process: `preflight-notifications` (200 with `*` for methods and
headers, no `max-age` or `vary`, against 204), `-expired-timestamp` and
`-bad-signature` (the recorded chains) and `unsubscribe-address-no-signature`.

## badges.decentraland.org (Catalyst: `/badges`)

| method | path | params | capturable | fixtures |
| --- | --- | --- | --- | --- |
| GET | `/categories` | | yes | `categories` (5 categories) |
| GET | `/users/{address}/badges` | | yes | `user-badges`, `-second-address`, `-unknown-address`, `-bad-address` (200 empty for a malformed address) |
| GET | `/users/{address}/preview` | | yes | `user-preview`, `-unknown-address` |
| GET | `/badges/{badgeId}/tiers` | | yes | `badge-tiers` (200 `{"data":{"tiers":[]}}` for a badge without tiers) |
| POST/DELETE | `/users/{address}/badges/{badgeId}` | admin token | no: write | |
| GET | badge art: `https://assets-cdn.decentraland.org/<badgeId>[/<tier>]/{2d,3d}/{normal,hrm,basecolor}.png` (the URLs the `assets` object carries) | | yes | `asset-2d`, `asset-3d`, `asset-404` |
| OPTIONS | any | CORS preflight | yes | `preflight-categories` |
| GET | unknown path | | yes | `unknown-path-404` |

## profile-images.decentraland.org (Catalyst: `/profile-images`)

Production answers every `/entities/{cid}/{face,body}.png` with a `301` to
`https://profile-images-bucket-43d0c58.s3.us-east-1.amazonaws.com/v1/entities/{cid}/{file}`
without checking the CID or the file name (the bucket then answers 200 or a
404 XML document); the explorer follows it. Both hops are captured.

| method | path | params | capturable | fixtures |
| --- | --- | --- | --- | --- |
| GET | `/entities/{cid}/face.png` | | yes | `face-profile-{0,1,2}` (301) + `-object` (200 image/png), `face-unknown-cid` + `-object` (404 XML), `face-bad-cid` (301) |
| GET | `/entities/{cid}/body.png` | | yes | `body-profile-{0,1,2}` + `-object`, `body-unknown-cid` |
| HEAD | `/entities/{cid}/face.png` | | yes | `face-head` |
| GET | `/entities/{cid}/other.png` | | yes | `unknown-file` (301 as well) |
| OPTIONS | `/entities/{cid}/face.png` | | yes | `preflight-face` (301, no CORS handling at the edge) |
| GET | `/` | | yes | `root-404` |
| GET | processor `/status`; POST `/schedule-processing` (bearer) | not on this host | no | |

The three profiles: the corpus CID
`bafkreickiq5m2aj4i4hi4emzcyaqvh2w2wdig325ienuqnkycz5xr5l66a` and the active
profile entities of two public community members (owner and moderator of
the largest public community), resolved through `POST
peer.decentraland.org/content/entities/active`.

## camera-reel-service.decentraland.org (Catalyst: `/camera-reel-service`)

| method | path | params | capturable | fixtures |
| --- | --- | --- | --- | --- |
| GET | `/health/live` | | yes | `health-live` |
| GET | `/api/users/{address}` | | yes | `user`, `user-unknown-address` |
| GET | `/api/users/{address}/images` | `offset`, `limit` (clamped to 100), `compact`; public photos only without the owner's signature | yes | `user-images`, `-compact-page2`, `-limit-over-max`, `-bad-limit` (400) |
| GET | `/api/images/{id}` | | yes | `image-redirect` (307 to the bucket), `image-file-redirect` (the `url` form `{id}-{ts}.jpg`) |
| GET | `/api/images/{id}/metadata` | | yes | `image-metadata`, `image-metadata-404` |
| POST | `/api/images`; DELETE `/api/images/{id}`; PATCH `/api/images/{id}/visibility` | signed | no: write | |
| GET | `/api/places/{placeId}/images` | `offset`, `limit` | yes | `place-images`, `-page2`, `-unknown-place` (200 empty) |
| POST | `/api/places/images` | `{placesIds:[...]}` + `offset`, `limit` | yes (read body) | `places-images-batch`, `-400-empty` |
| GET | `/api/wearables/{item}/images` | `<contract>-<itemId>`, `offset`, `limit` | yes | `wearable-images`, `-400-bad-item` |
| GET | `/api/docs/openapi.json` | | yes | `docs-openapi` |
| OPTIONS | any | CORS preflight | yes | `preflight-user` (200) |
| GET | unknown path | | yes | `unknown-path-404` (empty body) |

Legacy root paths (`/images`, `/users`, `/places/{id}/images`) are not
served on this host (README Coverage: "no Catalyst meaning").

## Counts

161 fixtures: social 53, social-rpc 5, gatekeeper 35, notifications 14,
badges 13, profile-images 20, camera-reel 21. 166 production requests plus 4
helper lookups on `peer.decentraland.org` across the six runs listed in
`MANIFEST.md`; the largest per-host total is social-api's 54.

Not captured and why: every write; token-minting reads under a real
identity (`/private-messages/token` signed, `/cast/generate-stream-link`);
bearer-token and moderator routes beyond their error shape (`POST
/v1/members/{address}/communities` included); the rpc host's WebSocket session beyond the reads in
`social-rpc/_socket/reads.json`; `GET /users/{userAddress}/community-voice-chat-status`
(same 401 shape as the captured voice-chat-status).

## Replay of social-api and social-rpc (2026-10-29 04:17 UTC)

`tests/conformance.rs` against revision `fa8f9acc` with the communities
mirror on production and `COMMUNITIES_ADMIN_TOKEN` set (setup and every row:
`tests/conformance/REPORT.md`, "Node setup" and "social"): social-api 32 of
53 identical and 21 different, none genuine (`communities-list-only-voice-chat`
and `community-v2-by-id` differ only in keys that are social-api extensions; the rest is the live listing, profiles not
synced, the mirror's empty member, post and place lists, and two messages that
name the replay's own signer); social-rpc 5 of 5 identical. The member, post
and place lists of the captured community, which the mirror does not copy, are
compared in process against seeded rows
(`social/src/communities/tests/fixture_shapes.rs`, where only the recorded
extension keys still differ).

`communities-list-limit-zero` cannot show a page count: `data.pages` and
`data.total` are volatile for the listing fixtures. The 2026-09-25 replay
against a Catalyst node recorded `pages` 0 vs 2 for it (limit 0 then paged by
zero). `limit=0` now falls back to the default 100, as the capture answers
(`limit` 100, `pages` 2): `social/src/communities/http.rs`, tests
`offset_is_capped_and_ordinary_values_pass_through` (the limit) and
`paginated_page_is_one_based` (the page count).

## Replay of the other services (2026-09-25 19:31-19:40 UTC)

`catalyrst-conformance-replay` (old harness) against the `v2-target` debug
binary built 2026-09-25 18:21 UTC, `serve` on database `catalyst_conf_social`
and port 5461 with the environment (bounded outfits/emote sync from
`peer.decentraland.org`, every mirror's first pass complete within 60 s,
`LIVEKIT_ALLOW_DEV_CREDS=true`), each service replayed with the candidate
`http://127.0.0.1:5461<prefix>`:

| service | passed | expected failures | divergences to look at |
| --- | --- | --- | --- |
| gatekeeper | 10/35 | 6 signed fixtures "Expired signature"; `scene-participants` 404 (no Pulse/archipelago room state) | unauthenticated scene routes answer `{"error":"Invalid Auth Chain"}` where production answers 400 `{"ok":false,"message":"Invalid Auth Chain"}`; `get-scene-adapter-unauth` 400 vs 401; bearer routes (`voice-chat-status`, `community-voice-chat/*`, `world ban-status`, `private-messages/token`) 503 vs 401/400 (Catalyst has no social-service token configured); `/cast/*` and `/scene-stream-access` 404 (G18 gap, unimplemented at the recorded revision); `ping` body 22 B vs `pong`; `user-bans-bad-address` 400 vs 200; preflight allow-origin reflected vs `*` |
| notifications | 1/14 | 4 signed fixtures 401 | `/status` 404 (route unimplemented at the recorded revision); `/unsubscribe/{address}` 404 (unimplemented at the recorded revision; now served, replayed in-process by `notifications::tests::the_unsigned_unsubscribe_capture_replays`); `?limit=5&onlyUnread&from=0` 400 (valueless `onlyUnread` rejected); opt-outs unknown scope 404 vs 400; every auth error body differs ("Unauthenticated: Expired signature" / "Invalid Auth Chain" vs the ADR-44 message pair); preflight 200 with `*` vs 204 with the requested headers |
| badges | 5/13 | user badges achieved 0 vs 3/5 (empty tables); `badge-tiers` 404 vs 200 empty | `/categories` lists 3 categories, production 5 (Explorer, Collector, Creator, Socializer, Builder); asset URLs: production `https://assets-cdn.decentraland.org/<id>/2d/normal.png`, Catalyst serves `/badges/assets/{path}` under `DEFAULT_ASSET_BASE_URL` `https://badges.decentraland.org`; preflight 200 `*` vs 204 |
| profile-images | 1/20 | the 19 image fixtures: production is a 301 to the bucket, Catalyst renders in process and answered 404 with no profile entities synced (`SYNC_ENTITY_TYPES=outfits,emote`) | structural: an edge redirect versus a direct `image/png`; `face-bad-cid` 400 vs 301; preflight 204 with every method vs the edge's 301 |
| camera-reel | 12/21 | `image-*` 404 (photo not in the local table) | the current answers are recorded in "In-process replay of camera-reel" below |

Logs: `replay-<service>.log` in the run's scratchpad; the node and its
database were stopped and dropped after the run.

## In-process replay of camera-reel

`camera-reel/src/tests/captures.rs`, test
`the_camera_reel_host_answers_as_its_captures`, mounts the router under
`/camera-reel-service` as `serve` does and replays all 21 captures through
`social::test_fixtures::replay` (status, recorded headers and body).
It seeds the captured photos from their recorded documents, place ids,
public flag, original filenames and `created_at` from `dateTime`. Filler
rows bring the corpus place to its 523 public photos and the item gallery
to its 12. The API and bucket origins come from the captures; requests stay
in process and never contact those origins.

The replay passed with PostgreSQL on 2026-11-13: 20 of 21 captures match.
Only `docs-openapi` differs from the 2026-09-25 capture: the `ScreenRect`
schema and nullable `User.screenRect` property were added in upstream
`3e8bd0b`. Both redirects match their captured status, headers and empty
body. The shared diff skips `id` and `url` leaves, so the test separately
checks exact photo ids, order, original URLs and thumbnail URLs for all five
photo answers.

`docs::tests::the_spec_is_the_captured_one_plus_the_later_upstream_screen_rect`
compares the whole pinned document with the capture under those two
additions. `GalleryImageWithPlace` remains undefined, as in the pinned
upstream document. The companion test checks the wire fields and every
path parameter. All three regressions passed in the 134-test Camera suite.
