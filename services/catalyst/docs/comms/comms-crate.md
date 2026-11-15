# catalyst-comms: as-built boundaries

`services/catalyst/comms` (`catalyst-comms`, layer 8) owns comms and multiplayer.
The ownership boundary is the [root AGENTS.md "Ownership → comms" bullet](../../../../AGENTS.md#ownership);
this file adds modules, interfaces and channel contracts. Decisions, findings and acceptance evidence live in the task report; use the [record workflow](README.md).

For the C# responsibility map and Rust ownership/threading migration instructions, read the
[Pulse to Rust migration guide](pulse-rust-migration.md). Its proposed worker structure is
separate from the current runtime described here.

## Architectural constraints

- One crate, not four, evolved in place from the moved dcl-one modules.
- Client-facing contracts (HTTP paths, room names, client wire protocol) stay compatible; NATS subjects are internal and change with both ends in one commit.
- Services talk over core NATS, at most once; it was kept over gRPC, Zenoh and Postgres NOTIFY because it alone covers per-key events, snapshots, fan-out and request/reply.
- The gatekeeper runs inside the primary `catalyst serve`; only the primary serves `/ws`. Pulse runs as `catalyst pulse`.
- The shared vocabulary lives in comms; ban status has one answer, comms moderation; the MLS delivery service stays in social.
- Friends' online status comes from Pulse's islands snapshot. Room occupancy and capacity stay on LiveKit as an interim; the target is Pulse.
- Pulse retains unfinished takeover work in memory; gatekeeper alone applies LiveKit cleanup, confirms completion and mints under the authoritative token floor.
- Pulse and the native Bevy client use one pinned, patched ENet source in `library/vendor/rusty_enet`. Each keeps its own transport driver; channel 1 explicitly bypasses local sender throttling while retaining sequenced-unreliable semantics. Pulse implements the opt-in 8192 payload/65536 charged queued bytes/256 command native receive boundary, with typed terminal early reason 16 and exact release; shared Explorer defaults/outgoing limits remain. Ordinary/connected event adapters preserve session ownership. No service/process/channel is added. Check current acceptance evidence in the task report before making runtime claims.
- Public Pulse V4 and Archipelago native/negotiated V4 authentication are separate proposed capabilities, deferred pending a separate user verdict. Ordinary signed `/ws` and its configured ownership remain supported. Profile announcements require nonnegative, strictly increasing versions but do not consume the teleport/emote budget; emote IDs follow the baseline without a separate 512-character cap.
- Scene hosting splits into a comms orchestrator and the engine runtime; Hammurabi is deprecated and never a source. See § Scene host.

## Target crate

| Module | Runs in | Owns |
|---|---|---|
| `pulse` | `catalyst pulse` | avatar sync over ENet UDP (default `:9000`) and WebTransport (`:7743`); islands (`cluster`); the dormant [application relay implementation](../../comms/src/pulse/application_relay/README.md); ban snapshots (`bans`, ETag poll of `GET /bans`); remote flags and per-IP admission (`flags`) |
| `archipelago` | primary `serve` | `/ws`, authenticated observations, socket registrations and enforcement room assignments |
| `stats` | every `serve` | complete read-only Pulse statistics snapshots, HTTP projections, `CommsCore` and archive views; uncached enrichment through the existing content pool |
| `gatekeeper::admission` | `serve` | scene, server-scene, world (`world_mint`) and island tokens; the Pulse room authority (`relay_authority`) |
| `gatekeeper::moderation`, `removal` | `serve` | bans, warnings, scene admins, durable ban kicks, disabled-place cleanup, room metadata repair |
| `gatekeeper::voice`, `streaming`, `streaming_jobs` | `serve` | private and community voice; stream access, LiveKit Ingress, streaming expiry; `/cast/*` answers 501 |
| `gatekeeper::reports` | `serve` | the player report intake at `/report-user` (`/api/reports/players/presign`, the evidence `PUT`s, `/api/reports/players`); the moderator token reads `/api/reports` |
| `gatekeeper::webhook`, `events` | `serve` | the one LiveKit webhook; a typed nine-field `RoomEvent` to `RoomEventSink` and `RoomEventStore` |
| `gatekeeper::bus` | `serve` | `serve`'s one NATS link |
| `presence`, `topology` | every `serve` | `/live-data`, `/wallet/{w}/connected-world`, capacity counts from LiveKit occupancy; social online from Pulse's topology |
| `protocol` | all | Pulse proto, legacy archipelago, `bus`, `room_authority`: the only module the services share |
| `rooms`, `world_identity`, `access`, `limits`, `worlds_envelope`, `writes`, `signed_fetch`, `deny_list` | library | the vocabulary worlds, social and shop use |

Outside: access, room resolution and storage (worlds); privacy, friendships, roles, notification
records and `/mls/*` (social); the scene runtime; LiveKit. Dropped: the relay to Decentraland's
Pulse (a mirror advertises Decentraland's Pulse in `/about`),
the player report routes under the gatekeeper's own mount (the intake is the
`/report-user` area), `POST /auth/challenge`, `/auth/livekit-token` and
`/heartbeat`. Room recovery uses `cluster_change`, `cluster_lookup` and
`takeover_completed`; hints do not own unfinished work.

## Interfaces

| Comms needs | Implemented by | For |
|---|---|---|
| `interfaces::AccessPolicy` | `worlds::policy::Policy` | may this wallet enter this world, scene or parcel |
| `interfaces::SceneIdentity` | `worlds::rooms::SceneResolver` | a world's active scene and the resolved room; room names are `comms::rooms` |
| `interfaces::SocialDirectory` | `social::comms_directory::Directory` | privacy, friendships, community roles; notices inserted on the caller's transaction |
| `interfaces::NameOwnerReader` | shop `MarketplaceState` | name ownership for Admin Toolkit targets |
| `gatekeeper::events::RoomEventSink` | `worlds::core::CoreState` | worlds' join/leave access log |
| `gatekeeper::events::RoomEventStore` | `social::badges::presence::EventSink` | every verified join/leave, stored before the webhook answers (503 on failure) |
| `stats::http::MetricsMatcher` | root `metrics_matcher` | fallible configured route evaluation on the pinned embedded engine; one async permit retained inside each submitted blocking operation |

| Comms offers | Consumers |
|---|---|
| `CommsState::revoke(RoomRevocation)`, the one revocation entry point | worlds permission edits |
| `explore::places::live::Participants` for `CommsState` | explore live counts, the presence archive |
| ban status, presence, voice rooms | worlds policy and publish; social, `/worlds/status`; social call signalling |
| signed `POST /get-server-scene-adapter`, `GET /scene-participants` | scene hosts |

## Placement

World-scene place identity belongs to `explore::places::scene_identity`
. Worlds invokes it in each deployment transaction under the world
row lock. The shared `explore.world_scene_place` row binds a stable UUID to the
current deployment; storage resolution and scene discovery read that binding.
World aggregate discovery, upstream catalogue IDs, Genesis storage scopes and
comms room-name contracts remain unchanged. The periodic catalogue job refreshes
metadata; it does not own identity lifecycle.

`comms = 8` in `docs/module-layers.toml` (above explore, below worlds, social and shop), enforced by
the repository structure checks. Higher crates implement comms' traits, keeping V8 and worlds out of comms builds.

## Services and how they talk

| Link | Channel |
|---|---|
| connector ↔ gatekeeper | calls inside the primary `serve` (`CommsState::announce_session`, `IslandSockets`); `peer.*.connect`, `peer.*.disconnect`, `engine.peer.*.island_changed` are retired |
| Pulse ↔ `serve` | core NATS, § Bus messages |
| Pulse → gatekeeper | `GET /bans` (moderator bearer, ETag); `POST /internal/pulse/room-authority/v1` with an HMAC key |
| gatekeeper → `multiplayer-hosting` | `RoomEvent` on `comms.room_events.v1` (defined, not yet published) |
| `multiplayer-hosting` → gatekeeper / engine | public routes / stdin-stdout JSON lines |

`/ws` limits : `WS_MAX_SOCKETS` (default 3,000, sized by the 10 ms-per-socket
re-announce sweep's 30 s cadence), beyond it 503 with `Retry-After: 5`; `WS_MAX_SOCKETS_PER_IP`
(default 64, 0 unlimited) per client source (IPv4 address or IPv6 /64) attributable through
`HTTP_TRUSTED_PROXY_CIDRS`, beyond it 429; an unattributable upgrade (a gateway peer outside that
list, or no `X-Real-IP`) is not counted, logs one warning and increments
`comms_ws_unattributed_upgrades_total` (registered at 0, exported with the other `comms_` series on `SERVE_METRICS_BIND`); positions coalesce
for ownership control to one per 500 ms; statistical observations retain every decoded position; a flooding socket closes with 1008; a 64-frame send queue in which a newer
`island_changed` replaces the queued one and `kicked` is never dropped, full for 10 s closes
with 1013; undecodable frames 1007; restart closes every socket with 1012.

**Deployment.**
- One primary is a release gate. Every API replica sets `FLEET_JOBS_ENABLED=false` (default true). Only the primary serves WebSockets; every serve supplies read-only statistics, about and realms from its retained complete snapshot. Stop the primary before its successor starts.
- The gateway's subdomain-prefix mapping sends `archipelago-ea-stats.<domain>` to `/archipelago-ea-stats/*` and `archipelago-ea-ws-connector.<domain>` to `/archipelago-ea-ws-connector/*`. Route both connector hosts and WebSockets to the primary. Additional host aliases require explicit proxy mappings. The complete standalone mounts sit outside shared path normalization and CORS; root content `/status` remains unchanged. Existing `/ws` and `/archipelago-ea-stats/ws` entrypoints remain, with `/archipelago-ea-ws-connector/ws` available at the connector mount. No public Pulse listener is added. Shared-root `/stats/*` aliases, including `/stats/health`, remain available at the root and are excluded from the standalone Stats mount. Optional WebSocket subprotocol names are echoed in offered order; they never select authentication or enable extended control fields.
- Key separation: only the gatekeeper holds the LiveKit secret; the dormant relay implementation requires a room-authority answer and has no production activation; `multiplayer-hosting` will hold the authoritative key; engines run as a separate user with a scrubbed environment.
- Configured legacy ownership uses `ARCHIPELAGO_CONTROL_V4_AUDIENCE`. Pulse accepts that audience or `PULSE_V4_AUDIENCE` for its shared NATS control-position bridge; conflicting values fail startup. Neither setting enables public V4 authentication.
- `DENY_LIST_URL` (or upstream's name `DENYLIST_JSON_URL`) is required in production; `NODE_POSTURE=mirror` defaults it to `https://config.decentraland.org/denylist.json`. Unset, the node warns at start and runs with the list off.
- `PULSE_FLAGS_URL` (HTTPS or numeric-loopback HTTP, polled every 60 s) may set only the four `PULSE_IP_LIMITER_*` values (`ENABLED` false, `MAX_CONCURRENCY` 10, `SCENE_LISTENER_MAX_CONCURRENCY` 2, `WHITELIST` empty by default).

## Statistics ownership and runtime

Pulse owns a separate legacy statistics population. Authenticated decoded positions enter a
bounded 1,024-event connector queue before ownership, realm and finite-position filtering.
A serialized publisher assigns a process UUID and increasing sequence; each socket disconnect
emits a removal, including duplicate devices and replaced sockets. Publish rejection or timeout
is counted and logged without retry. These observations cannot authorize room ownership.
Connector assignments exist only for moderation and participant removal; fenced delivery records
the issued room under the existing local ownership guard after Pulse and SQL authorization.

Every serve subscribes to ordered statistical changes and complete recovery frames.
Contiguous changes become visible at subscription callback completion; no periodic
timer delays the healthy change path. A gap retains the previous view until a
watermarked complete recovery arrives. HTTP, about/realms and each archive pass
retain one immutable snapshot across projection and content I/O. Cold state remains an empty
successful view, and an outage retains the last complete snapshot without public peer expiry.
Social keeps its existing topology subscription and LiveKit keeps occupancy authority.
Hot-scene HTTP and archive enrichment use serve's existing content pool and shared
content denylist, preserving selected local document order before projection,
stable count sorting and top-100 selection. Thumbnails name `CONTENT_URL`, or
this node's own `/content` when it is unset; local SQL determines the corpus and
dependency failures. The
[local content source ADR](../../../../docs/adr/2026-11-12-stats-local-content-source.md)
records the accepted source semantics and preserved rendering and snapshot obligations.
The discovery subscription and original frame validation remain for transport compatibility;
they no longer maintain a connector statistics cache.

Recovery deadlines are checked between transfers. One captured checkpoint follows
its finite queued revision prefix before later changes; ongoing observations cannot
postpone it. The five-second recovery cadence excludes the current transfer, prefix
drain, encoding and checkpoint send time. Each send has a 15-second deadline;
encoding is outside it. Overflow or failed publication discards the pending prefix
and checkpoint in favor of a new complete recovery. Sequential encoding awaits do
not prevent an already-started blocking encode from outliving publisher cancellation.

Stats and connector hosts have separate real request registries, bearer-protected scrapes,
optional operating-system collectors, and reset after the first successful scrape strictly
past the previous reset's local midnight plus 24 hours. Histogram maintenance runs
at each recording boundary, independent of successful scrapes. Readiness reports
the original `server` check from the actual listener lifecycle;
NATS and content add no probes. Rust exposes actual available process measurements rather than
invented Node/V8 series. Node-specific default runtime metrics remain a compatibility gap.

## Bus messages

Types, subjects and caps: `comms::protocol::bus`. Record reviewed channel contracts in the architecture model.

| Message | From → to | Subject | Cap, cadence | Recovery | NATS permission |
|---|---|---|---|---|---|
| Cluster change | Pulse → primary island subscriber (one unqueued subscription) | `peer.{wallet}.cluster_change` | 64 KiB; coalescing outbox | fresh lookup; rotating retained-pending hints | Pulse pub, `serve` sub |
| Cluster lookup | `serve` → Pulse, reply on private `_INBOX.` | `peer.{wallet}.cluster_lookup` | exact session request; complete reply ≤64 KiB; 2 s deadline | active re-announcement and retained pending cleanup | `serve` pub, Pulse sub; both pub/sub `_INBOX.>` |
| Takeover completion | gatekeeper → Pulse | `peer.{wallet}.takeover_completed` | 1–4096 bytes; ID 1–64 bytes | resend identical ID/cutoff until authoritative acknowledgment | `serve` pub, Pulse sub |
| Islands snapshot | Pulse → social topology in every `serve` | `engine.islands` | 1 MiB, 1 s | next snapshot; online expires 5 s after the last valid one | Pulse pub, `serve` sub |
| Discovery beacon | Pulse → retained connector discovery subscription | `engine.discovery` | 10 s | next beacon; no statistics cache | Pulse pub, `serve` sub |
| Statistics producer control | connector ↔ Pulse | `archipelago.statistics.producer.v2`, private `_INBOX` replies | ≤64 KiB and broker limit; existing bounded request queue/deadline | lazy incarnation probe, candidate grant, exact confirmation; idempotent registration only | `serve` request/private reply sub, Pulse sub/private reply pub |
| Statistics observation | connector → Pulse | `archipelago.statistics.observations.v2` | ≤64 KiB and broker limit; queue 1,024; 2 s including registration | Pulse incarnation/generation and increasing cursor; at most once; no event retry | `serve` pub, Pulse sub |
| Statistics changes and recovery | Pulse → every `serve` | `archipelago.statistics.snapshots.v1` | each frame ≤min(64 KiB, broker limit); transfer ≤64 MiB; changes on observation; recovery requested every 5 s at transfer boundaries | finite queued prefix, then complete checkpoint; retain last complete view on gaps | Pulse pub, `serve` sub |
| Control position | connector → Pulse | `archipelago.control.position.v1` | 640 B | next heartbeat | `serve` pub, Pulse sub |
| Room event | webhook → `multiplayer-hosting` | `comms.room_events.v1` | 4 KiB | reconcile via `GET /scene-participants` | `serve` pub, `multiplayer-hosting` sub only |

A publish returns after bounded local SDK acceptance: `Submitted` proves neither broker authorization nor delivery. Require per-process NATS credentials and subject permissions matching the producer/consumer table. Verify deployment enforcement through the private release review.

Social login/referral work and notification persistence stay inside `serve`:
authenticated login ingress commits local SQL and the existing worker handles
reward providers. They use no SQS/SNS transport and add no NATS subject. See the
[native social events ADR](../../../../docs/adr/2026-11-04-native-social-events.md).

## Social subscription fanout

Each `serve` attaches Social to its existing `CommsState::bus` link and subscribes
without a queue group to `comms.social.events.v1`. There is no second connection,
process or pool. Comms owns the subject constant; Social owns the codec, because
its protobuf types belong above Comms in the module layers.

The version-1 envelope contains a process UUID and a tagged event: a normalized
recipient plus friendship, block or private-call protobuf; community voice plus
scope and optional excluded creator; community membership status; community
deletion; or an immutable payload reference. The decoder checks version, kind,
UUIDs, normalized wallet identities, status enums and the permitted protobuf arm.
Private REQUESTED includes only the caller, ACCEPTED alone may carry credentials,
and neither private parties nor closure notices can be added by another arm.
Private acceptance reads the caller's credential before publishing ACCEPTED, then
reads the callee's credential for the reply. An incomplete credential map returns
the source missing-property error in RPC and a generic HTTP 500. The accepted
intent remains consumed; if only the callee's grant is missing, ACCEPTED has
already been published with the caller's own grant.
The NATS subject and payload together are limited to 64 KiB. The
outbound application queue contains at most 256 messages; mutation publishers
await admission for up to five seconds. The community inbound queue contains at
most 1,000 messages and runs at most four deliveries, capped by the shared SQL
pool size. Address-targeted events need no SQL and enter subscription queues
directly in the broker callback. Queue admission timeout, community ingress
saturation or failed publish drops that event and increments
`social_event_delivery_dropped`. These operational bounds are separate from
client subscription queues; delivery beyond them is unmeasured.

Each client subscription has its own typed queue of 1,000 pending events. Unrelated
event types do not consume that queue. RPC discards its oldest pending update at
the bound; JSON closes its socket with code 1008 on the next update. Dropping a
subscription clears its queue and unregisters it. The raw 256-slot broadcast
observer API is used by domain observers and tests, not by socket subscriptions.

Social connectivity RPC permits a single large encoded profile when the guarded
writer lane is empty. Its actual bytes remain charged until the physical write
finishes; additional frames wait for the configured socket backpressure budget.
A stalled write keeps the existing five-second deadline and retires the socket.
The producer waits for each acknowledgement and rechecks topology and privacy
before writing. Other domains retain their default 16 KiB guarded-frame limit.

Ordinary Social RPC frames use an opt-in pending-message queue in the same
writer. Its source adaptive capacity is 128 messages with the defaults. The seven
`WS_TRANSPORT_*` queue, size-estimate and retry settings use numeric-prefix
parsing. Fractional queue thresholds and finite large capacities are preserved;
the queue allocates as messages arrive. Overflow or retry exhaustion closes with
1011 `RPC transport closed`. A pending head keeps its scheduled exponential
backoff across new sends; a writer drain advances the retry. Accepted frames are
removed once and retain their byte charge through physical flush. Admission
checks bytes already pending with a strict `>` threshold, so one frame can cross
the configured budget. Ordinary unary responses have no guarded-frame size cap.
After authentication, binary and text frames both feed their bytes into dRPC,
matching the source handler's ignored `isBinary` callback argument. Responses
remain binary. The source handler/transport/server fixture and a real socket test
cover the same binary and text CreatePort exchange. The Social reader observes
the shared last-activity expiry independently of a stalled physical write,
including a single accepted response with no pending retry head. This passive
observer leaves Ping emission to the writer and remains disabled at idle timeout
zero. A real stopped-reader test covers this separate expiry path.

Tokio writer admission and flush supply these acceptance and drain signals;
they do not measure uWS's kernel buffer. Source callback traces cover the queue
policy, and socket tests cover large response preservation and stopped-reader
closure. Native buffer timing and throughput equivalence remain unmeasured.
Startup refuses zero `WS_MAX_BACKPRESSURE` and nonfinite computed ordinary queue
capacities to retain bounded admission; the pinned
[native send guard](https://github.com/uNetworking/uWebSockets/blob/7fd2be6fe2afe5af55e4c25b696801beeca1da52/src/WebSocket.h#L120)
disables its limit with zero, and the source transport can compute an infinite
queue capacity. `WS_MAX_BACKPRESSURE`
still uses an unsigned integer parser, so the source's numeric-prefix,
fractional, negative and wrapping native conversion forms remain configuration
differences to check before cutover. Other users of the shared transport keep
their existing ordinary writer unless they select this queue.

Both Social socket protocols read `WS_AUTH_TIMEOUT_IN_MS` and
`WS_AUTH_SIGNATURE_EXPIRATION_IN_MS` separately using the source environment
provider's numeric-prefix parser. The authentication timer follows Node's
millisecond timer conversion: values outside 1..=2,147,483,647 schedule one
millisecond, and valid fractions are truncated. Signature age checks retain the
configured numeric value and the original signed timestamp text; timestamp
conversion accepts the source's finite JavaScript Number forms. Idle timeout
uses the pinned native binding's unsigned-short conversion, accepting zero or
8..=960 seconds after conversion. The source-executed numeric fixtures exercise
these conversions and native option admission; they do not measure native idle
timer scheduling or kernel buffering.

Social signatures bind the source protocol paths: `/` for RPC, and `/v1/stream`
for both JSON handshakes. Connecting through `/social-rpc` or
`/social-rpc/v1/stream` keeps those signed paths; `x-original-path` does not
expand them. Actual source verifier captures and real socket replays cover
81 combinations. The Bevy Social client normalizes only its signed URI to `/`
while keeping its configured connection URL. Clients signing a mounted alias
must sign the source path; deployed web artifacts need the corresponding client
build at cutover. Other shared handshake callers keep their chosen alias policy.

When the shared bus is enabled, Social registers its event subscription before
checking the existing NATS subscription readiness fence. Until the first ready
transition, valid RPC and JSON WebSocket upgrade requests receive an empty HTTP
503 before authentication, socket admission, or a 101 response. Other HTTP routes,
ordinary fallback requests, and local mode remain available. The retained Social
subscription and readiness poll run in the existing fanout job. Admission stays
open through later outages; publication and delivery retain transient, at-most-once
behavior, with no replay guarantee.

The pinned source starts its standalone RPC listener only after Redis connects
and all nine subscription registrations finish. Executed component/lifecycle
captures cover delayed connections, delayed subscriptions, subscription failure,
and an outage after startup. The monolith's pre-upgrade 503 is an operational
difference from that closed listener; it does not wait within an HTTP request or
hold authentication age and socket slots while startup is pending. The controlled
startup tests exercise both protocols and delivery after the fence; the separate
real-broker tests cover live cross-replica delivery.

Oversized community voice metadata is stored as one immutable binary protobuf
snapshot in `social.event_payloads`, with a UUID and a 24-hour expiry, before a
small reference is published. A subscriber reads that exact snapshot through its
existing SQL pool, validates it, and delivers it locally. Stored snapshots cannot
contain references or private-call credentials. Separate START snapshots remain
separate across END, privacy edits and a later START; metadata is never truncated.
A payload row is a transport cache, not an outbox, replay log or delivery receipt.

A mutation is delivered locally before its bus hint is queued. The origin drops
its own loopback; address-targeted deliveries preserve the order observed from
that publisher. Community handlers can complete out of order, as the source's
async handlers can. Concurrent publishers have no total order. Community workers
allow five seconds per delivery. Community fanout reads subscriber membership in
pages of 256 and re-reads current privacy for START. Member removal and deletion
status reach the removed member without requiring it to remain a member. Pulse
connectivity is already derived locally by every replica and never crosses this
subject, preventing duplicate global announcements.

Authentication is the configured internal NATS link; the event envelope does not
supply client authority. Routing checks still run in Social, and the existing
connectivity delivery guards re-check privacy and shared membership before handing
an item to a socket. Public client sockets cannot publish this internal subject.

Delivery remains transient and at most once, as Redis pub/sub in
`decentraland/social-service-ea` at `b6b2cefefdf727b2ed47ddb887e77d226a41eea6`.
A disconnect may lose hints; the existing broker reconnects and restores its
subscriptions. There is no per-event retry, forced client closure, sequence ledger,
or promise to replay missed updates. A new friend-connectivity subscription starts
with online friends only; community connectivity starts with live events.

Community START facts are Social-owned rows in
`social.community_voice_notifications`: the exact case-sensitive raw key, canonical
community UUID, scope, generation and expiry. Raw UUID spellings retain independent
audiences and TTLs; only membership and media use the canonical UUID. The polling
job runs every `COMMUNITY_VOICE_CHAT_POLLING_JOB_INTERVAL` milliseconds (45,000 by
default), reads raw PK pages of 64 and asks the existing Comms liveness query. Active
entries refresh their 24-hour expiry without changing scope or generation; inactive
fresh entries are claimed by raw key plus generation and announce END once. Expired
entries are cleared without an event. Explicit END claims only its supplied raw key
before media teardown and restores it on failure only if that key remains absent.
Other aliases await the poll; a canonical media closure does not clear their facts.
Generation checks fence notification metadata without making external media END
atomic with START. The same job removes at most 64 expired spill rows per pass.


## Room recovery

The canonical contract is `library/protocol/proto/decentraland/pulse/pulse_clusters.proto`.
Pulse commits each completed assignment pass and its recovery state before producing hints.
A process-lifetime source incarnation and checked assignment version distinguish replacement
and expiry; unchanged passes preserve identity. Live registration and control-owner checks
invalidate stale active lookups between passes. Ready snapshots record potential credential
exposure, so an intermediate assignment blocked by cleanup does not invent another predecessor.

Snapshots and persisted admission stamps retain selected origin and exact identity. Native
registration reserves precedence even without usable coordinates and invalidates earlier legacy
evidence until a completed pass installs a fresh version. Unknown and cleanup-only evidence
cannot mint or refresh credentials. Legacy v1/v2 readers share the wallet capacity; mixed
authority is withheld without a grounded common allocator-domain proof. The ordinary connector
continues emitting v1, so reader support does not establish final native SQL ownership or a
producer cutover ([selected evidence ADR](../../../../docs/adr/2026-11-12-pulse-selected-room-evidence.md)).

Pulse budgets 24,573 simultaneous distinct transport/control wallets and one equally sized
churn cohort, for 49,146 retained wallet records, with at most 32 pending operations per
wallet. Complete plans are bounded to 64 KiB; overflow never becomes a truncated ready reply.
Unfinished or blocked records never expire. Other records require both 300-pass retention
and the confirmed floor's 15-second clock grace. Exact retained inactive sessions can finish
pending cleanup without a socket. Their last completion makes lookup unavailable, preventing
inactive admission. Further churn can exhaust the finite reserve; it is not a production
throughput measurement or an unlimited availability guarantee.

A completion removes only its matching retained operation. Its positive whole-second cutoff
must exceed that operation's prior floor and remain within the JavaScript safe-integer range;
the aggregate floor takes the maximum. Duplicate or unknown IDs cannot advance it. Gatekeeper
separately bounds arithmetic, waits for the floor plus its one-second clock allowance and
refuses waits of 60 seconds or more. Signing chooses the greater of the Pulse/durable wallet
floor and current Unix time less that allowance. A positively settled Cloud removal must
finish before the cutoff minus the allowance; otherwise only that known-settled operation
may obtain fresh authority and a higher cutoff. Unknown outcomes never gain a retry.

Gatekeeper owns three typed shared PostgreSQL relations: permanent wallet admission
fences/generations/floors, immutable per-RPC cleanup attempts and logical Pulse takeover
receipts. The key spans all Pulse sources and configured/unconfigured ownership in the
shared database. The unresolved dispatch marker commits before RPC; remote calls start only after that
SQL transaction ends. Short transactions lock wallet before assignment/intent rows;
there is no NATS or SFU await while SQL locks are held. Only primary dispatches cleanup.
Every island removal, including connector and moderation enforcement, uses this same
wallet safety with real operation/intent provenance; enforcement never fabricates a Pulse
operation or completion. Lift/delete/claim expiry and room closure do not delete hazards.

The cap of 10,000 counts outstanding logical obligations, not permanent fence rows or
all historical attempts. Unknown timeout, cancellation or disconnect stays admission-closed
across gatekeeper restart and never retries or expires. Confirmed takeover work replays its
original completion/cutoff until fresh authoritative acknowledgment, without repeating
removal; its confirmed durable floor survives acknowledgment and compaction. Maintenance
visits at most 16 wallets/pass and deletes at most 64 eligible receipts plus 64 attempts per
wallet while preserving floors, unknown and unacknowledged work. These bounds limit
mutations, not candidate scans or latency. A strict definite rejection can prepare a new
immutable attempt under current authority. Typed room-missing can permit ensuring the room
and rechecking; creation is never completion. Positively settled self-hosted quarantine
requires a full maximum 60-second issuer lifetime plus 60-second verifier leeway and
2-second safety allowance, including a fresh process's monotonic observation, before a
separately recorded final removal. Unknown final work stays closed.

Internal admission stamps carry exact wallet/session/realm/cluster/source/version, Pulse
floor, durable generation/floor and chosen credential `nbf` through existing assignment JSON
and socket queues. Configured queued credentials also retain their exact committed
TokenFence. These fields are private; client packets are unchanged. Every signing,
publication, refresh and dequeued-consumption boundary checks shared durable safety and
fresh Pulse authority. Configured final SQL authorization checks owner session/epoch/revision/token,
policy and current database-clock freshness after potentially blocking owner acquisition;
refresh includes the presented device. Queued output then checks current local owner/permit.
An old candidate never adopts a newer stamp or fence. All serves connect their existing
NATS link for nondestructive verification and signed refresh; replicas start no destructive
subscriber/job. Database or authority failure denies. These finite snapshots do not make
remote cleanup or physical network output atomic with later ownership/policy changes.

Lost hints and lost confirmed acknowledgments can recover while Pulse retains its ledger;
positively confirmed fenced recovery and replica refresh use the same durable safety.
Gatekeeper restart preserves hazards; it does not turn unknown outcomes into completion.
Recovery procedures must establish safe admission, participant cleanup and token lifecycle
boundaries before reopening traffic. Verify those requirements, deployment routing and
rollout readiness through the private acceptance review before release.

## Scene host

As built, hosting is still `worlds::scene_state`: the old own-socket contract (`/ws/{scene}`,
one in-process V8 isolate per scene), which cannot run current SDK multiplayer scenes. The
decided target uses the room-participant contract (the server joins the scene's LiveKit room
as `authoritative-server`):
- `multiplayer-hosting`, a comms service run as one instance, starts a server on the first joined event for a scene with `authoritativeMultiplayer`, stops it 60 s after the room empties, reconciles against `GET /scene-participants`, keys servers by canonical room and entity hash (a redeploy drains the old one), allows 6 starts per 60 s, and pushes room tokens and 60-minute storage delegations to the engine. Metadata comes from `/content/entities/active` and `/content/contents/{hash}` (5 s timeout, verdict cached per entity hash). `/debug/*` answers only signed fetches from configured admin wallets, otherwise 403; local preview is deferred (503).
- The runtime is upstream bevy-explorer `headless --orchestrated`, unpatched and keyless, one engine per world. The one engine change left, as an upstream PR: report a room joined only once LiveKit confirms it.
- Presence, world capacity and the room-empty rule exclude `authoritative-server`.
- Pulse scene listeners must chain to the authoritative address, scoped to their parcels. Pulse admits a listener handshake only from `AUTHORITATIVE_SERVER_ADDRESS` and any `PULSE_SCENE_LISTENER_WALLETS` (`catalyst secrets init` copies the address into Pulse's file), and refuses every listener with neither; the parcel scope is not yet enforced. Verify this requirement through the private acceptance review.

## Acceptance evidence

Read the architecture decisions, divergence findings and readiness evidence supplied for the task before planning or claiming deployment. Record missing implementation, measured capacity, live-client verification and required private security review there. Public ownership guidance does not itself establish readiness.

Default process metrics use actual OS data: fractional user/system/total CPU, process start time and resident bytes on Windows and Linux; Linux additionally exposes virtual/data-segment bytes and open/max file descriptors. The Linux CPU source has kernel clock-tick resolution. Unavailable measurements are omitted; Node/V8 runtime measurements are not synthesized. `WKC_METRICS_COLLECT_DEFAULT=false` disables these collectors.
