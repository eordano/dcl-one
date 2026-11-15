# Database design contract

Applies to content, Explorer, social and every domain added to `catalyst`: our
application of Richard Fabian's [*Data-Oriented Design*](https://www.dataorienteddesign.com/dodbook/)
to PostgreSQL. Fewer crates is an architecture target; table count follows
the data and its operations.

## Principles and decisions

- **Describe the workload first**: inputs, outputs, row counts, skew, fan-out,
  read/write rates, payload sizes, freshness. Keep expected and measured scale
  apart.
- **Store facts with explicit keys and dependencies.** Separate independently
  changing relationships; declare uniqueness, required values and ranges.
  Give every derived value a named source and update rule.
- **Process useful sets.** Select due work and needed rows in SQL, transform
  bounded batches, keep external validation out of the commit transaction.
- **Move fewer bytes.** Filter and deduplicate keys before fetching large
  payloads; choose indexes from predicates and ordering.
- **Confirm improvements** against an explicit latency, throughput or resource
  budget, write costs included.

## Schema and ownership

1. A schema names an authority and lifecycle. Shared facts have one
   authoritative relation; join across schemas in SQL. Match meanings and keys
   before merging duplicate schemas or same-named tables. Preserve source IDs;
   chain-derived identity includes network, contract and token.
2. Model memberships, friendships, favorites and permissions as keyed
   relations, with explicit acceptance/blocking and local-versus-mirrored
   semantics. No universal entity/attribute table; no JSON document holding
   unrelated, independently updated facts.
3. Keep opaque protocol metadata and ordered auth chains as documents when read
   as a unit; use typed columns or relations for fields used to join, filter or
   update independently. Do not split columns mechanically. Original
   hash-addressed bytes stay immutable in the blob store; never reserialize
   JSON to reconstruct signed or content-addressed input.
4. Start with `PRIMARY KEY`, `UNIQUE`, `NOT NULL`, row-local `CHECK` and
   suitable foreign keys. For every omitted relationship constraint, record its
   invariant, enforcing write path and deletion behavior. Use a cross-schema FK
   only with a shared restore/deletion lifecycle; otherwise keep FKs inside the
   owning schema and define reconciliation. `CHECK` is not a cross-table
   invariant mechanism.
5. Label projections. `active_pointers` and `deleter_deployment` derive from
   deployment ordering and must change atomically with accepted deployments; a
   rebuild must reproduce tie-breaking and deletion semantics. Peer cursors,
   failed retries and `local_entities` provenance are durable: a cache rebuild
   must not discard them or infer local authority from peer data.
6. Keep frequently scanned keys and state small; fetch auth chains, metadata
   and blobs only where needed. Split out a payload relation only after
   measuring row width, updates and joins (TOAST already stores large values
   out of line).
7. Land a DDL change twice once a database is stamped: in `schemas/*.sql` for
   new databases and, when it touches an existing table, as an additive step in
   `schemas/migrations/` (`catalyst schema migrate` applies each once and
   records it in `meta.schema_steps`; `services/catalyst/tests/`). A step
   that drops, renames, retypes or deletes is refused: that change is its own
   reviewed release (record the data-safe release procedure and evidence in the task report).
   A changed body of a hash-named function (`content.deploy_entity_<hash>`,
   `content.unpublish_scene_<hash>`, `social.world_authorization_started_<hash>`,
   `social.world_secret_refund_<hash>`, the `explore.*_<hash>` functions the
   generated columns call) is a new function; `migrate` creates it and
   leaves the old one for binaries still calling it. `catalyst schema status`
   lists the old ones as superseded after migrate (run before `migrate`, that
   includes the one the running binary still calls), never one a column
   default, a generated column or an index still calls (`pg_depend`; dropping
   it would take the column or the index with it); drop the listed ones by
   hand once no process runs an older binary.
   A step that adds a column calling such a function creates it first with
   `CREATE FUNCTION`, skipped when the database has it. A body that computes
   the same values at a lower cost is the one in-place change: the binary
   hashes it under a new name for fresh databases, and a step replaces the
   shipped name's body with it (`CREATE OR REPLACE FUNCTION`), so the
   generated columns of a migrated database take the new cost without a
   rewrite; a test pins the replacement to the binary's definition and a
   test shows both bodies agree on every input a generated column can pass
   them (`0033_explore_recurrence_one_pass`; `raw` is NOT NULL, and a SQL
   NULL `raw` is the one input where the `live_until` bodies differ).
   The `deleter_deployment` repair runs once per database and repair version
   (`content.system_properties` key `deleter_repair_version`, only ever
   raised), at a sync leader's bootstrap or a non-syncing process's startup.
   During a rolling deploy an older binary still writing can leave rows the
   repair's rules would change after the marker is set; the next sync leader
   bootstrap repairs them, or restart a sync leader once the rollout is done.
   The repair marks each snapshot window whose rows it superseded with
   `content.snapshots.outdated` (step `0026_content_snapshots_outdated`, a
   constant default without a rewrite; the table holds a few dozen rows, one
   per generation window). The generator's tick scans for the flag and
   regenerates those windows; the row keeps its real `generation_time`, which
   `/snapshots` publishes as upstream does.

## Worlds, persistent storage and scene hosting

One worlds subsystem owns deployment/permission lookup, world/player storage
and authoritative scene hosting, on the shared pool with explicit
realm/scene/namespace and player scope. Keep the storage namespace distinct
from a deployment's content hash. Each world scene has a stable identity
(`explore.world_scene_place`) following Places' overlap rule: overlapping
exactly one active scene retains it, including a moved base; overlapping none
or several creates a new identity. Retired identities keep their storage rows.

Partial (multi-request) world publishes stage in `worlds.pending_scenes` and
`worlds.pending_scene_files`, with no world row until the completing batch's
publish transaction consumes the upload and writes its
`worlds.completed_scene_uploads` receipt. Upload counts and staged bytes are
admitted under advisory locks (deployer, world, node budget); the per-world
count and byte sums read `pending_scenes (world_name, created_at)`. The blob
collector keeps every file a pending row names, and a leased job reclaims
expired uploads under the exclusive store lock, unlinking only files a
batch of that upload wrote (`pending_scene_files.written`). Local content
deploys (`POST /entities` and partial publication) and world scene deploys
take that lock's shared side in their write transaction and re-check every
file they name, so an unlink batch never removes a file such a deployment
references; a content deploy waits at most 5 s for it and answers 503 past
that. Synced content writes do not take it and rely on the collector's age
floor.

Genesis storage management requires LAND rights at the requested parcel, using
the published LAND manager index or its configured document source. Without a
document source, only ownership at that parcel grants authority; scene-admin
roles do not grant storage authority.

Storage collaborators are wallets in an active local world deployment's or
active Genesis deployment's `metadata.logsPermissions`; Genesis deployments
accepted through peer sync are eligible too. Per-key and list `/values*` and `/players*`
operations probe the resolved deployment by primary/unique key on every
request; redeployment without the wallet revokes that grant. Collaborator
access excludes clears, `/env*` and `/usage*`.
`GET /world-storage/collaborator` is a rare, user-initiated, self-scoped listing.
It derives membership at read time through `worlds.logs_permissions_decoded`, with no
stored projection. The partial GIN `world_scenes_logs_permissions_decoded_idx` covers
local world scenes; `deployments_scene_logs_permissions_decoded_idx` covers live
Genesis scenes, followed by an `active_pointers_entity_id_idx` probe per match.
Superseded Genesis rows leave the index. One statement counts and pages the
matches by `deployed_at DESC, scene_id` with `LIMIT/OFFSET`.
The function decodes JSON escapes as `JSON.parse` does and leaves out an entry
carrying `\u0000` or a surrogate escape (no address), so no stored JSON value
can fail an insert through an index. It reads the escapes as Unicode, so the database
must use the `UTF8` server encoding (the one `catalyst` supports): under another
encoding a non-ASCII escape would fail the insert or the index build. `schema apply`
and `schema migrate` check the server encoding before any schema changes. A database stamped before it keeps the
superseded `worlds.logs_permissions` and its two indexes, which `catalyst schema
status` does not report, until their release drops them. Run it once, with the
administrative role, after `catalyst schema migrate` created the decoded function
and every replica runs a binary that queries it (an older `serve` calls the old
function from its storage grant and `GET /collaborator`, which fail after the
drop); it refuses a database without the decoded function and both its indexes
built and valid, and drops the old indexes concurrently, outside a transaction
(rerun it if interrupted):

```sh
psql "$DATABASE_URL" -X -f schemas/releases/worlds-logs-permissions-retired.sql
```

The object pass builds `deployments_scene_logs_permissions_decoded_idx` on
`content.deployments` in the migrate transaction, not concurrently, so writes to
that table wait for the build (unmeasured on production). To build it outside
that window, run beforehand, with the same definition as `schemas/20_worlds.sql`
(the function first, then the index, which the object pass then skips):

```sql
CREATE FUNCTION worlds.logs_permissions_decoded(text) ... ; -- as in schemas/20_worlds.sql
CREATE INDEX CONCURRENTLY deployments_scene_logs_permissions_decoded_idx
    ON content.deployments
    USING gin (worlds.logs_permissions_decoded((entity_metadata -> 'logsPermissions')::text))
    WHERE entity_type = 'scene' AND deleter_deployment IS NULL;
```

A failed concurrent build leaves an invalid index of that name: drop it before
retrying.
Expected traffic is low; measured dataset sizes, query timings and index write
costs must be recorded in the task report.

Genesis collaborator grants also require the configured LAND index to be
complete and fresh. One shared SQL predicate checks the entire scene footprint
for both request authorization and listing, before counting and pagination.
For each parcel, indexed token-history probes select the latest LAND transfer,
estate membership change, and current estate's transfer by block/log position.
Missing ownership or required event timestamps fail closed. Approval and
operator changes do not change this ownership boundary. Transfer away and back
still revokes the old deployment's grant.

The predicate uses the fresh published fold while unrelated events are being
ingested. A relevant LAND transfer, estate membership event or current estate
transfer beyond that published block denies the affected footprint until
publication; approval churn and unrelated parcels do not cause a global
collaborator outage. Publication timestamps and ordinary LAND readiness use
the database clock; the status response observes the cursor and that clock
together. Both
request and listing SQL gate the history function behind a `CASE` membership
check, so a wallet absent from `logsPermissions` does not pay for that probe.

Timestamp repair runs after live ingest and ownership publication, consuming
only the remaining request budget within a 30-second deadline and applying
multiple batches of up to 100 distinct blocks. Cancellation or the deadline
retains committed repair progress. Rejected or partial batches fall back to
counted single-header calls, with periodic batch probes so a transient fault
does not disable batching until restart. An unresolved or hash-mismatched
block stops the live cursor at the preceding verified prefix. The repair due-set and updates are scoped to
the configured LAND/Estate registries. Unknown legacy timestamps keep the
collaborator readiness gate false until repair completes; ordinary ownership
publication remains independent. Verify upgrade lock behavior and backlog estimates before claiming readiness.

The immutable signed entity timestamp minus the protocol's 15-minute maximum
future skew must be strictly later than every relevant ownership event. No
request-time baseline, receipt timestamp, metadata refresh or peer replay
renews the grant. This conservative rule assumes source deployment validation
and ordinary agreement between source-node and chain clocks. A deployment
timestamped within 15 minutes after an ownership change cannot restore the
grant: another deployment with a later signed timestamp is required. Waiting
alone does not restore that old entity.
Unavailable LAND history affects this collaborator grant only; independent
owner, player and scene-server grants retain their own authorization rules.

Genesis scenes need a valid scope too. Use one permission core with explicit
owner/player/host operations. Guests get scoped storage capabilities, never SQL
access or other scenes' environment values. Mirroring cannot overwrite local
saves or grant host authority. Keep live CRDT/tick state in the scene runtime;
persist only what the restart contract requires. Test cross-scene/player
isolation, redeploy, revocation and restart.

Places owns one active identity row per world scene, explicitly bound to its
current `(world_name, entity_id)` deployment. Worlds calls the Places identity
writer in the deployment transaction under the world row lock; storage and
scene discovery resolve that same binding. Catalogue refreshes never mint or
retire identities. Inactive rows retain the UUID but clear the deployment link,
so retained storage does not become reachable from a replacement scene. No
foreign key ties retained identities to replaceable deployment rows; the
transactional writer and partial unique deployment index enforce the binding.
The unique partial `(world_name, entity_id) WHERE active` index enforces one
active place per deployment and serves exact deployment lookups. Its leading
`world_name` column bounds overlap probes and retirement sweeps to the active
scenes in that world (expected up to a few hundred).
World quotas and usage exclude recorded retired identities, and exclude the
parked legacy `world` bucket once that world has an active identity; unknown
other legacy scopes still count. Storage rows remain retained and inaccessible
through new scene scopes. Verify migration and recovery limits and measure quota-query costs before acceptance.

The world discovery listing remains one aggregate per world. Scene listings and
place detail read the shared identities joined to active deployments; neither
adds scene rows to the aggregate catalogue. Upstream mirrored catalogue IDs
remain external IDs, never storage authority. The old
`worlds.world_scene_identity` table is a frozen upgrade source only: creating
the new Places relation copies its UUIDs and retained records during object
creation. This is once per creation, not once for the lifetime of a database:
never drop/recreate the authoritative relation to recover it, because that
would replay the stale legacy snapshot. Existing storage keys are not rewritten,
and migration `0010` stays byte-for-byte stable.

**SQL-level dependency:** Places discovery joins `worlds.world_scenes` and
`worlds.worlds`; Places identity retirement probes `worlds.world_scenes` for
the exact current deployment. Worlds owns those source rows and invokes the
Places identity writer within the same transaction. These cross-schema reads
are part of the shared database contract, independent of Rust crate imports.
The Rust module-DAG check does not inspect SQL and cannot prove these data
dependencies absent. Keep column/visibility and lifecycle changes coordinated
across both domains; no reverse Rust crate dependency or internal HTTP call is
introduced.

Preserve the reviewed method, path, scope and query authorization contract for scene-host storage requests. Handle security acceptance through the private review workflow.

Single world and player storage values (never env values or listings) are
read through a per-process LRU, as upstream: on unless `STORAGE_CACHE_ENABLED`
is exactly `false`, at most `STORAGE_CACHE_MAX` entries (8000, least recently
used evicted first), each fresh for `STORAGE_CACHE_TTL_SECONDS` (60) after it
was cached, values over `STORAGE_CACHE_MAX_VALUE_BYTES` (32768) never cached,
misses never cached. Invalidation is local to the process that wrote: with
several storage-serving processes, a read may return a value up to the TTL
old after another process's write or delete, as on upstream's replicas.
`FLEET_JOBS_ENABLED` does not make that cache coherent. A deployment needing
immediate cross-process freshness sets `STORAGE_CACHE_ENABLED=false`.

Permission-source outages deliberately return 503 instead of upstream's 401,
without granting access. Transient permission/RPC outages carry
`Retry-After: 1`. The header is a minimum retry delay, not a promise
that capacity has recovered; clients should use backoff for repeated failures.
Unsupported EIP-1654 validation requires configuration and carries no timed
retry instruction. `ENCRYPTION_KEY` is required: `serve` refuses to start
without it. This is an intentional wire divergence, not a claim of identical
error responses.

A storage export (`POST /world-storage/exports`) holds one pooled connection
while reading and serializing world rows and then player rows, never env. The
read-only transaction ends before the final gzip trailer is sent; cancellation,
the export deadline and the raw-size cap bound interrupted producers. `EXPORT_MAX_CONCURRENT_PER_POD` (5) bounds them per
`serve` process and must stay well under `PG_POOL_SIZE` (60): at most a
twelfth of the pool by default, so the request handlers keep the rest. The
caps are per process, as upstream's are per pod.

`export::produce` is rare and user-initiated. Its two row streams use the
namespace primary-key prefix in key order: `world_storage`, then
`player_storage` by `player_address, key`, in one read-only repeatable-read
transaction. As upstream, admission answers 200 and starts both the scene
cooldown and the `EXPORT_TIMEOUT_SECONDS` timer before any database work; the
producer then acquires the connection, owning it and its concurrency slot. A
connection, snapshot or query failure after the headers cuts the stream (an
HTTP body error, never a valid gzip trailer) and still consumes the cooldown.
The timer bounds acquisition, SQL and channel backpressure; dropping the body
cancels the producer. Completion uses a separate channel, so a failure or
panic cannot masquerade as successful HTTP EOF. Transaction-local
`statement_timeout` and `idle_in_transaction_session_timeout` are
`PG_COMPONENT_STREAM_QUERY_TIMEOUT` (120000 ms; 0 disables them) and reset
before pool reuse. The export settings take any value, as upstream's: a zero
concurrency cap answers every export 429, a zero timeout or size cap aborts
every stream, and a timeout too large to represent sets no deadline.
Optional selections are
`key = ANY` / `player_address = ANY` filters on that prefix. Not measured:
a scene at its quota (10 MB world plus 1 MB per player).

Scene-host storage requests sign the pathname, allowing `Storage.getValues`
listing queries. Verification still binds the method and declared storage mount;
the unsigned-query trade-off requires security acceptance.

Comms (`services/catalyst/comms`) uses the worlds/content identities and world
permission decisions through shared queries and local calls. World permissions
keep one authoritative representation; social keeps message/call membership
and privacy; comms enforces them and owns its connection/assignment lifecycle.
Persist ACLs, bans and assignment/fencing state needed for recovery under
their owning domains; live connections, presence and media follow the
transport's lifecycle. Presence is not proof of permission. Resolve room
identity from the same active deployment for both clients and authoritative
hosts. Permission revocation must also take effect in the running room, not
merely change a row. Preserve host identity and publish/subscribe grant
distinctions. Test shared-room joins, denied joins, revocation and reconnect
against the real transport.

World allow-lists admit explicit access wallets and members of listed active,
unsuspended communities who are not banned. Admission probes
`worlds.world_access_communities` with `social.community_members` and
`social.community_bans`; their primary keys cover the world/member lookups.
`world_access_communities_community_idx` finds affected worlds by community
for leave, kick, ban, suspension, deletion and mirror-deactivation cleanup.
Expected scale is thousands of grants and tens of room joins per second;
these are estimates, not measured throughput.

## Queries, ingest and derived state

The `/social-api` gateway reads `HTTP_MAX_REQUEST_BODY_BYTES` at startup: the
default is 1 MiB and the accepted range is an integer from 1 byte through 16 MiB.
A declared oversize receives a plain 413 before auth or routing, with the actual
response CORS headers and `Connection: close`. Social handlers defer body reads
until their existing auth gate and count streamed bytes against this cap. The
source schema boundary returns its own 400 for a failed read; multipart and raw
mute readers retain the source generic 500 for a global stream overflow.
The independent multipart total remains 1 MiB, even with a larger HTTP cap;
thumbnail bytes have a separate 1 KiB through 500 KiB range. A multipart limit
failure returns 400. The process's existing outer drain still handles bodies
dropped without a handler read, under its shared upload budget; per-social
unread-stream limiting and close timing remain outside these consumed-body
checks. Other domains keep their existing body and drain policy.

Community and friendship producers write native notification rows through the
existing SQL pool. Notification insertion also creates the email delivery row;
the existing delivery worker owns email attempts. Community recipients follow
membership and subscription opt-outs. These producers do not publish an AWS
event or depend on an external notifications worker. Failure behavior at each
producer remains explicit: best-effort community/friendship notifications do
not undo their preceding mutation; referral grant and accepted notification
are one transaction.

Native ended-event metrics live in Social. `social.event_ended_occurrences`
records one immutable fact per `(event_id, occurrence_start)`; its RSVP count
comes from explicit local attendance, and its community must resolve to an
active, unsuspended local community. The fact and community metric contribution
commit together. Removing a catalogue event does not erase facts; deleting a community removes its aggregate. The first scan starts at
activation and does not import historical endings. Each subsequent pass reconciles
currently ended occurrences against the immutable fact keys. Duration-only edits
that move an end behind an earlier scan are reconsidered. A DTSTART or recurrence
change records a private database-clock cutoff in the authoring transaction.
Unchanged eligible starts shared by the old and new materialized schedules
remain eligible, including live occurrences and deferred endings. A private
lookup stores only those pre-edit starts still in the current schedule, bounded
by that schedule rather than growing across edits. Newly introduced backdated
starts cannot contribute. Creation and each transition into approved state start
an eligibility window using the database clock: already-ended occurrences are
excluded, while an already eligible occurrence still in progress is retained.
Thus a late first submission counts its ongoing occurrence without importing
historical attendance, and later approval cannot backfill endings from pending
review or resurrect a newly backdated schedule identity. These keys
are excluded from the public event projection and notification snapshots.
Existing immutable facts remain intact. Ordinary
owners must obtain moderation again after changing dates or durations. Recovery
is limited to the latest 30 days: older unrecorded endings require a separate
backfill, and existing immutable facts remain untouched. The approved
`(finish_at,id)` index prefilters series before date expansion; attendance and
community resolution occur after the candidate LIMIT. Each pass handles
at most 100 occurrences with transaction-local statement and lock timeouts and
an independent scan budget, so metric contention cannot stop notification
delivery. Expected workload is one periodic scan per minute; throughput at
production catalogue size remains unmeasured.

The optional event moderation webhook uses an Explore-owned transactional
outbox with public labels and moderation reasons only. Delivery is at least
once, bounded to six attempts with five-second HTTP timeouts, and starts at its
first capture cutoff, written atomically with the intent using the database clock.
Exhausted delivery attempts remain distinct from successful completion. A creator
can enqueue at most two edited notices per rolling minute; unsent edits
coalesce per event. Every submission retains its own durable alert. Approvals,
rejections and deletions precede all submissions, and submissions precede edits;
submission volume therefore cannot delay operator moderation alerts. Deployment configuration supplies the secret endpoint;
operator writes and deployment posture gate outbound delivery. Unconfigured or
operator-write-disabled authoring captures no webhook intent. The same worker
performs bounded cleanup even when its endpoint is disabled: at most 1000 rows
per pass, with a one-second client budget and transaction-local statement/lock
timeouts. Completed and exhausted rows are retained for 30 days, as is legacy
history before activation; eligible unfinished retries and the rolling-minute
edit throttle window are preserved. `(created_at,id)` supports retention order. Migration `0018` adds missing creator
and edit-bucket columns on earlier shared development layouts without removing
intents, and the object pass recreates the changed due/creator indexes.

Event notification delivery is owned by Social in
`social.event_notification_intents`. Explore inserts an event transition snapshot
through qualified SQL in the same transaction as the event change; Social owns
recipient progress and completion. This shared-database edge has no reverse Rust
dependency or internal HTTP transport. No event foreign key is used: committed
host notices must survive removal of a mirrored catalogue row.

The lifecycle scanner considers live occurrences in a rolling one-hour catch-up
window with a local audience, including on first deployment. Separate scan
budgets prevent reminder work from starving starts. Stable occurrence keys and a unique queue index
deduplicate repeated scans, including late approvals and equal due times. A
recipient page commits its cursor with notification and email-outbox inserts.
Completed queue rows have 30-day retention, exceeding the catch-up window;
bounded cleanup retains unfinished work. Stale reminders and ended occurrences
are suppressed before delivery. Cleanup has a bounded opportunity before dispatch,
including when there is a backlog. Claims rotate queue order durably before page
work; transaction-local database timeouts bound slow queries and lock waits.
Recipient branches are bounded before merging the page. Notification timestamps
use the delivery clock so incremental polling sees catch-up deliveries; source
timing remains in metadata. The worker uses a five-second dispatch budget rather than a
fixed number of audience pages. Independent three-second scan budgets and
two-second scan statement timeouts allow more time for catalogue discovery than
for recipient pages. Scan timeouts and budget exhaustion emit error-level
diagnostics without activating job backoff; other scan failures reach job error
reporting after dispatch. Cleanup failures and budget exhaustion emit error-level
diagnostics without stopping delivery. Committed stale or
obsolete suppression logs the intent and reason, including expiry after lock
contention. Community opt-out UUIDs are canonicalized on write and lookup;
event and community delivery recognize existing non-canonical UUID scopes too.
Lookup binds optional UUID parameters parsed in Rust, preserving opaque legacy
scope IDs without relying on guarded casts of SQL parameters.
Catalogue-scale capacity is not established by
bounded result sizes alone. See the
[event notification ADR](docs/adr/2026-11-01-event-notification-delivery.md).

Event RSVP state in `explore.event_attendance_local` is authoritative for that
wallet: cancellation markers override mirrored attendee previews in reads,
personal filters and attendee listings, and survive mirror refreshes. Counts
apply a signed local delta to the mirrored aggregate, deduplicating wallets
known in the mirrored preview. The preview is incomplete: an additional local
going wallet may already be included in the upstream aggregate. Without an
authoritative upstream membership list, the combined total remains an estimate;
it does not guarantee exact deduplication outside the known preview. Local-only
events do not have this ambiguity.
The read projection materializes the normalized preview once per event row and
excludes SQL-null preview entries. Local membership probes retain the
`event_attendance_local` primary-key lookup; cancellation and going deltas share
that preview instead of expanding its JSON separately for every local RSVP.

Event authoring distinguishes generated posters from explicit images and clears
inside the existing event document. Generated images may follow location
changes; an explicit clear must survive later edits. Missing profile or place
lookups must not erase stored facts on an unrelated edit. Location-derived facts
must be refreshed or cleared when their source location changes. This metadata
does not establish owner identity or grant permissions. Existing nonempty posters
without authoring provenance remain explicit: their source cannot be inferred
reliably from the URL alone. There is no historical provenance backfill.
The internal `_image_source` key can travel with raw document copies. Public API
and export paths must use an explicit field projection instead of publishing the
stored document unfiltered. `EventRecord` omits this key, and notification
snapshots use their own allowlist.

For Shop: keep explicit asset/chain/order identities, separate accounting
facts from rebuildable projections, bound reconciliation, use exact currency
units, reserve atomically with external work outside the transaction. The
MANA-only LAND/resale rule holds through authorization and settlement; a
stored client-supplied sale category never establishes eligibility.

- Share the pool and use qualified SQL joins. Return the fields the consumer
  needs; avoid per-result round trips. Preserve API ordering, collation,
  pagination and empty/error distinctions when moving Rust work into SQL.
- Keep the bounded `UNNEST` ingest and deterministic pointer locks. Bound bytes
  and transaction duration as well as entity count; measure pool and lock
  waits under reads. Advance durable progress only after data or its retry
  record is durable; keep duplicate/out-of-order/resume tests.
- Poll indexed, bounded due sets for retries, mirrors and notification
  delivery. Keep exhausted-row cleanup and no-auth behavior explicit. With
  multiple workers, use a short atomic claim with lease/recovery semantics; do
  network work outside the transaction; preserve idempotency on redelivery.
- Give every cached or materialized result a source, freshness budget,
  invalidation and rebuild path. Notifications may wake a reader; durable
  delivery needs a transactional record and a resumable consumer. Keep
  triggers small and count their cost in ingest measurements.
- Each index names its query or integrity constraint in the SQL comment beside
  it. Match filter, sort, expression and keyset tie-breaker; check the actual
  prepared plan. Measure storage/WAL/update cost before adding or removing an
  index; a tiny lab run with zero scans is not evidence to remove one.
  Partial-index predicates match the query; current time belongs in the
  due-work query, never in a moving `now()` index predicate.
- Prefer stable keyset pagination for new feeds and mirror scans; keep
  required offset APIs. Define tie-breaking, replay windows and deletion
  handling; a cursor alone does not give snapshot consistency.

The anonymous Registry `GET /denylist` feed reads `content.denylist` through the
existing content pool: `id`, `added_by` and `added_at`, ordered newest first.
Expected lists contain hundreds of rows; the read checks overflow at 100,001 rows
and refuses lists above 100,000. The shared Explore API state retains local
rows for five seconds and coalesces concurrent refreshes. Database failures are
retained for the same interval and return 500; a failed refresh never serves an
expired successful list. Operator edits become visible on the next read after
expiry, and process restart rebuilds on demand. Source moderation responses are
validated on every request and never cached here. Merge and serialization remain
per-request work; the cache bounds table-read frequency, not total endpoint CPU
or response bytes. No table, index or pool is added. The public feed intentionally
publishes every local id kind and `added_by` as the operator's `created_by` wallet.

The public What's On calendar caches completed, parsed catalogue responses
for 30 seconds in each Sites process. Its key is the catalogue base, search,
recurrence selector and exact date window; catalogue page offsets and accounts
are not inputs. At most eight entries of up to 500 records are retained, with
expired and least-recently-used entries evicted on reads. Parsed responses
retain malformed-row counts so a damaged record does not bypass the cache. Concurrent reads of one key share one upstream request with
an independent five-second budget; each request can cancel only its own wait.
At most eight shared reads run at once; a new key at capacity fails retryably.
Failed or timed-out reads are not retained, and process restart rebuilds from
the events API. Page views reuse the same calendar read within the freshness
budget while live/catalogue reads stay uncached.

## Measurement and acceptance

Use read-only PostgreSQL catalogue and statistics queries through psql for a
baseline (for example `pg_stat_user_tables` and `pg_stat_user_indexes`). Capture
a before/after pair over the same workload window; counters are cumulative and
row counts are estimates. For a
query/layout change record revision, PostgreSQL version, hardware, dataset
size/distribution, concurrency, cache state, query/parameters, repeated
p50/p95/p99, throughput and resource cost. Use `EXPLAIN (ANALYZE, BUFFERS,
WAL, SETTINGS)`; `ANALYZE` executes, so measure writes on a disposable
database. Buffer hits are not CPU cache hits. Before accepting a slice, prove
wire, authorization, concurrency and recovery behavior and state the
improvement and trade-off; smoke-scale bootstrap runs are not benchmarks.
Record missing measurements explicitly.

## Bootstrap, schema versions and local writes

`catalyst schema apply` bootstraps `catalyst` from the binary's rendering of
`schemas/`; it never replays third-party schema histories or dumps. Startup
verifies schema identity and version and fails clearly on a mismatch; it never
resets persistent data. Drop and re-apply disposable development databases. A
database holding local authority changes only through a data-safe release
(record the data-safe release procedure and evidence in the task report): additive steps under
`schemas/migrations/` applied once by `catalyst schema migrate`; anything that
drops, renames or retypes is its own reviewed release under `schemas/releases/`.
After the steps, the object pass creates every object of `schemas/` the
database lacks and re-creates each view it has in place, so a column a step
adds reaches the view; a view change that drops, renames or retypes a column
is a release, because the replacement refuses it.

The binary requires schema version 3. Stop all writers, pollers and administrative
write paths, and verify a backup restore before a release. A version-1 database
first takes `schemas/releases/v2-shop-schema.sql`, then a compatible version-2
binary runs `catalyst schema migrate` and `catalyst schema status`. Do not use the
version-3 binary to prepare version 2 or edit `meta.schema_steps` to bypass
preparation.

The Social voice release accepts shipped main version 2 without historical step
`0020_social_voice_notifications` or its retired objects, and genuinely prepared
version 2 with that step's matching real receipt and complete legacy layout.
`catalyst schema status` exits 5 on such a database. After verifying a backup
restore and with every writer stopped, the version-3 binary applies the release
and the additive steps in one transaction, with no statement timeout:

```sh
catalyst schema migrate --release
catalyst schema status
```

`--release` refuses while any other client session is connected to the database,
including sessions of roles whose activity the migrator cannot read, and reports
their count and application names; it changes nothing then. It is a best-effort
guard: the release locks its own tables, not every table. A connection pooler or
monitoring agent that holds sessions open also blocks it. Exit 5 reflects the
stamp only; the release's own checks (mixed layout, receipts, destination,
pending wallets) can still refuse it inside the transaction. A database already
at version 3 takes the plain migrate.
The binary embeds `schemas/releases/v3-private-voice-pending.sql` with its includes
(`db::RELEASE_SQL`). The same file still runs through psql, followed by
`catalyst schema migrate`:

```sh
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f schemas/releases/v3-private-voice-pending.sql
catalyst schema migrate
```

Keep every writer stopped through either sequence. The release refuses mixed
layouts, changed receipt hashes and a pre-existing destination. It audits pending
wallets across both roles and refuses overlapping or non-normalized pending state.
The explore parity layout (step `0029_explore_parity`) moves event moderation notices
into the shared outbox `explore.moderation_hooks`; the earlier tables
`explore.event_moderation_hooks`, `explore.event_moderation_hook_activation` and
`explore.event_upcoming_hooks` are retired by their own release. Run it once, with the
administrative role, after every replica runs a binary with the outbox and `catalyst
schema migrate` has recorded step `0029_explore_parity`; it refuses to run earlier:

```sh
psql "$DATABASE_URL" -X -f schemas/releases/explore-retired-hook-tables.sql
```

Until it runs, every pass of the `moderation-hook` job first moves the pending rows of
the old events table into the outbox, so the release finds few or none; it moves the
rest in the same transaction, keeps the earlier of the two activation instants and
drops delivered and dead-letter rows with the tables. The drop is not reversible: a
rollback restores the matching backup. Do not reset a database.

The canonical future definitions live once under `schemas/releases/v3/`; the
compatible version-3 build explicitly enrolls them in the single fresh bootstrap.
A version-2 build does not discover that nested directory. Merging dormant
release artifacts does not activate version 3 or establish cutover readiness.
In one transaction the release creates the canonical exact-key notification relation,
preserves prepared legacy generations and TTLs, verifies the backfill both ways,
retires the legacy layout when present, creates the canonical caller/callee
indexes, drops the former uniqueness constraints and stamps version 3 last. It
neither renews expired facts nor rewrites a call or participant. Legacy UUID
columns have already lost raw identifier spelling: their backfill uses `id::text`
and cannot reconstruct historical aliases.

Run the current `schema migrate` for the remaining additive layout, and verify a
clean schema status before traffic resumes. On shipped main version 2, step
`0021_social_referral_lifecycle` adds `referral_progress.id` with a volatile
default: PostgreSQL rewrites the table and builds its unique index under ACCESS
EXCLUSIVE and mints one id per existing row (see METRICS.md for a measured size).
Its other columns take constant defaults or are nullable, and step
`0023_social_referral_pending_reward` adds two nullable columns without a rewrite.
`0024_social_referral_reward_receipt` adds nullable claim, response and retry
columns, imports confirmed `reward_paid` receipts without losing notification
completion, and quarantines older uncertain pending rewards. The backfill touches
only paid or pending rewards. `0030_explore_read_columns` adds the stored read
columns of `explore.place`, `explore.place_world_local` and `explore.event` in
one `ALTER TABLE` per table, so each table is rewritten once: 0.96 s, 0.91 s and
0.14 s on 10,008, 10,008 and 3,011 rows, where one statement per column took
2.38 s, 2.07 s and 0.39 s. `0033_explore_recurrence_one_pass` replaces the
bodies of the two recurrence functions of `explore.event` in place with
one-pass bodies computing the same values: 19 ms on 7,686 rows, no row
rewritten; a write parses each `recurrent_dates` entry once per column
(5.7 µs per entry where the shipped bodies took 21 µs).

The object pass also builds `deployments_active_entity_timestamp_idx` on
`content.deployments` (the snapshot generator's inactive-entities count) in the
migrate transaction, not concurrently, so writes to that table wait for the
build: 7.8 s on 4M narrow synthetic rows (METRICS.md, "Snapshot inactive-entities
count"), unmeasured on production. Writers are stopped for a release anyway. To
build it outside that window, run beforehand, with the same definition as
`schemas/00_content.sql`:

```sql
CREATE INDEX CONCURRENTLY deployments_active_entity_timestamp_idx
    ON content.deployments USING btree (entity_timestamp) INCLUDE (entity_id)
    WHERE deleter_deployment IS NULL;
```

The object pass creates only objects the database lacks, so it then skips the
index. A failed concurrent build leaves an invalid index of that name: drop it
before retrying.

**Migration 0024 does not support a rolling deployment from a pre-0024 binary,
even when both binaries require schema version 3.** A pre-0024 binary treats any
non-null `reward_tier` as permission to POST and ignores the new claim and lock.
It could issue a reward already claimed or quarantined by the new code. Before
running `catalyst schema migrate` for 0024:

1. Pause referral/login ingress and stop every login consumer, retry worker and
   administrative writer. Drain or stop in-flight reward work. Pending native
   login observations retain their client event IDs and may retry within their
   acceptance window. Legacy SQS messages cannot be replayed into the signed
   native endpoint: they lack the user signature, and no queue importer exists.
   Any undrained legacy messages will not advance referral progress after cutover.
2. Stop every pre-0024 binary sharing this database and disable its automatic
   restart or rollback. A fleet lease or `FLEET_JOBS_ENABLED=false` alone is not
   this gate: older login consumers do not honor the new reward lock.
3. With all writers stopped and a verified backup/restore procedure, run migrate
   and schema status using the compatible binary. Inspect imported paid receipts
   and quarantined pending claims before resuming only compatible processes.

This gate applies even when the database is already version 3; the version stamp
cannot distinguish the two reward implementations. Schema migration does not
automatically enforce the process shutdown. Do not roll an old binary back onto
the migrated database; use a reviewed forward fix or a coordinated restore and
provider reconciliation, since restoring SQL cannot undo an external reward.
No production deployment history is assumed to waive this gate.

The owned upgrade test
`populated_main_v2_referrals_survive_the_upgrade_with_minted_ids` checks a
populated shipped-main table keeps every value, gains unique ids and marks its
granted rows. A database prepared at the captured version 2 also holds
`referral_progress_ip_idx`, which the bootstrap does not define and no query uses;
schema status does not report it. From version 3 onward, the exact
immutable historical 0020 is retired: apply/migrate never execute it or invent a
receipt. Status/bootstrap verify any genuine receipt and reject the five known
retired columns, constraint and index if present. A failed release transaction
preserves the version-2 stamp, facts and constraints. Both older and newer
binaries refuse a mismatched stamp.

Retained media and independent raw notification aliases can coexist in version 3.
Reinstalling the former uniqueness or single-row layout is not a general rollback.
Quiesce and use a reviewed forward fix or restore; never delete or deduplicate
media calls or notification facts to force old constraints back. Owned upgrade
tests exercise the composed release and real migrator against captured, genuinely
prepared and shipped-main version-2 state; deployment backup, restore, historical
raw-alias loss and cutover validation remain separate operating gates.

### Retired telemetry sink

The node is not a Sentry or Segment sink: the `telemetry` schema (the sink
tables, their console state and the Shop analytics rows) left `schemas/`, no
binary reads or writes it, and client telemetry goes to Segment from the sites
and the explorer. A database stamped before the removal keeps the schema until
the release script drops it; the bootstrap and the object pass neither create
nor drop it, so the old tables are inert, not an error. Rollout, in order: set
`SEGMENT_WRITE_KEY` for the sites (`track()` sends nothing without it) and
deploy the sites build without the sink's reader (`TELEMETRY_DATABASE_URL`
leaves with it); run `catalyst schema migrate` with the binary without the
sink (steps `0032_shop_sales_index` to `0035_explore_mirror_adopted_at`; its
`serve` refuses to start before them); run that binary on every replica and
confirm no `serve` of the old build is left (its ingest writer and
`telemetry-purge` job write the schema, and the database cannot tell a quiet
one apart: ingest was off by default and both builds connect alike); then
once, with the administrative role:

```sh
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f schemas/releases/telemetry-retired.sql
```

The release refuses a database without step `0035_explore_mirror_adopted_at`
or whose `telemetry.events` took a row in the last ten minutes (an old `serve`
still ingesting; the fleet check above is the gate for a quiet one), waits at
most 10 s for its locks, then drops the schema with its
twelve tables and nine indexes in one transaction and keeps `ops.admin_audit`:
its rows with domain `telemetry` and the CHECK value that admits them stay. The
deployment then drops the read-only role it created for the console's SQL
reader and the `TELEMETRY_*` settings. An old `serve` started after the drop
fails its bootstrap on the missing relations and stays down.
The drop is not reversible without the matching backup. Do not reset a
database.

Peer sync is not a backup of local state. Keep local write paths disabled until a backup restore of the node's database, blobs and credentials has been demonstrated. Record the restore and rollback evidence in the task report before enabling writes. Development and commit builds sharing a database must agree on its schema; do not reset or silently migrate shared state to fit a working tree.


## Social realtime notification workload

Social keeps each notification scope, generation UUID and 24-hour expiry in
`social.community_voice_notifications`. Its case-sensitive `raw_community_id`
text primary key preserves the source cache key, while `community_id` is the
canonical foreign key used for membership and media. Several spellings of the
same UUID may retain independent facts. Non-null fields and identity/scope checks
couple their lifecycle. START upserts one exact key with a new generation; a
45-second default poll reads raw keys in ordered PK pages of 64. It refreshes an
active key's expiry, or clears an inactive/expired key, by exact key plus generation.
A media closure does not identify a raw key; explicit END handles only its supplied
spelling, and other aliases await polling. No SQL lock spans a media or NATS await.
Generation CAS protects metadata from stale cleanup, without making external
media teardown atomic with a competing START. The raw primary key supports all
runtime reads/writes; expiry is checked in the bounded pages, so it needs no
separate expiry index.

`social.event_payloads` is an optional immutable transport cache for community
voice metadata whose event exceeds the existing 64 KiB NATS admission limit.
The writer inserts opaque binary protobuf bytes under a random UUID, capped at
512 MiB and expiring after 24 hours. Normal events require no insert. Receivers
perform one fresh-only PK lookup for an oversized event; separate snapshots cannot
overwrite each other. Private-call credentials never use this relation. The
`event_payloads_expiry_idx(expires_at,id)` supports ordered deletion of at most 64
expired rows with `FOR UPDATE SKIP LOCKED` per polling pass. This relation does not
record delivery, retry, or replay work.

The expected hot set is tracked active raw voice keys plus oversized events
published during the preceding day, rather than all communities or subscribers.
Membership fanout uses set probes of at most 256 subscriber addresses per SQL
statement. A community voice update snapshots eligible subscribers and completes
all membership reads before emitting. Any failed page suppresses the entire
member-scoped update; a public start instead clears all membership annotations and
announces to the whole snapshot with `isMember=false`. A public end skips
membership reads. The snapshot and membership set each hold at most one entry per
eligible subscriber. Production active-room counts, oversized-event frequency and
a 512 MiB payload's memory/latency cost have not been measured; the byte limit is
an admission bound, not a demonstrated capacity. Operators can observe transport
drops through `social_event_delivery_dropped` and retain the existing SQL/job instrumentation.

Private-call accept, reject and end RPCs first run the parameterized scalar query
`SELECT $1::text::uuid` through the existing pool. This adds one SQL round trip per
RPC so PostgreSQL controls accepted UUID spellings and syntax-error messages before
the stricter media identifier check. It reads no application row, takes no row
lock and needs no index. HTTP keeps its own canonical UUID validation and calls
the typed operation directly. Query-only observations are recorded in `METRICS.md`;
they do not establish equal latency with the source's combined lookup.

## Social community ranking and compliance workload

Community ranking reads current membership, posts, places and thumbnail facts in
pages of 100 communities. Five lifetime counters on `social.communities` retain
photos, ended events, event attendees, ended streams and ended voice chats. The
ranking job updates local authoritative communities only. Mirrored ranking scores
remain upstream facts. `ranking_tracking_since` bounds local event occurrences
eligible for collection; zero counter defaults do not reconstruct history. An
authoritative cutover must import the lifetime counters, collection watermark and
local producer receipts together before enabling local ranking ownership.

The existing leased event collector keeps a scalar source-key cursor and a
recurrence ordinal window. It selects 100 event keys through the existing primary
key before reading schedules, expanding at most 100 recurrence positions per
event or probing receipts. One pass shares a three-second budget across at most
1,024 pages and commits at most 100 new occurrences. Page receipts and lifetime
counter changes commit before cursor advancement. Cancellation, SQL failure and
process restart retain durable deduplication; cursor state is only local work
progress. Full cyclic revisits reconsider backdated inserts, duration edits and
appended recurrences. No end-time watermark hides these changes. A long catalogue
or recurrence list can take multiple minute passes; these bounds do not establish
ranking freshness in production.

`social.community_ranking_events` retains local producer receipts with primary key
`(kind,event_key,community_id)`. One photo may contribute to several communities;
receipt insertion and counter changes share a transaction. Receipts have no expiry.
External decoded events preserve the upstream consumer's repeated increments,
including its finally-acknowledge behavior on handler failure. The receipt relation
does not turn that transport into an exactly-once consumer.

Community deletion probes `community_voice_notifications.community_id` and
`community_ranking_events.community_id` through their cascading foreign keys.
Both have a leading community-id index. The raw notification key and the ranking
receipt's `(kind,event_key,community_id)` key serve different access patterns.
Synthetic warm measurements and insertion costs are recorded in `METRICS.md`;
they are not production capacity measurements.

Camera uploads retain their multipart basename in `camera_reel.images.original_filename`.
Gallery URLs concatenate the configured API base, row UUID and basename literally;
thumbnail names add `thumbnail-`. Reserved filename characters remain raw in JSON
and redirect Location values, as in the pinned source. To request a complete
filename containing `?`, `#` or `%`, clients must percent-encode its path segment.
Alias downloads extract the UUID and use the existing primary key to resolve the
shared image or thumbnail hash. The nullable column adds no index or backfill:
existing rows retain their hash URLs. Migration `0045_camera_reel_original_filename`
adds the column without replacing rows or blob bytes. Local `/api/files` downloads
apply the camera row's review state and the shared blob denylist. Image redirects
preserve the requested filename without querying the database. After storing both
blobs, a new upload releases previous deletion withholding for those hashes only
when a live non-rejected photo references them and no rejected photo does. It
retains moderation rejections and operator or remote denials. Configure
`CAMERA_REEL_BUCKET_URL` as the public API base plus `/api/files` to use this virtual
origin, or route the existing image origin to that handler. A hash-only origin
needs an ingress/configuration adjustment before cutover; it cannot serve the
filename aliases directly. An external public origin is responsible for restricting
its own copies. The local byte route serves complete GET responses, without Range
or ETag support; HEAD returns 404. It does not implement the S3 object API.

Camera upload MIME is retained per photo in nullable
`camera_reel.images.image_content_type`, constrained to the source's accepted
`image/png` and `image/jpeg` values. Both object aliases return that claimed MIME,
including when their detected encoding differs. An upload adds this short string
to its existing row insert; alias downloads select it with the existing UUID
primary-key lookup. The column adds no index, table, pool or extra read query.
Migration `0044_camera_reel_image_content_type` preserves existing rows without
inventing an object MIME backfill. Legacy NULL values and raw hash URLs retain byte-format sniffing. An import
can populate the per-photo value from object metadata; a raw hash alone does not
identify which photo's claimed MIME to use.

New camera uploads start with `ranking_published=false`. After both blobs are
written, one transaction marks the row and applies local photo contributions.
Existing photos receive `true` during migration, preserving the history boundary.
The existing leased minute job keeps two scalar UUID cursors over the partial
`images_ranking_pending_idx`. Each query fetches 100 pending keys before age
classification. Recent and cold sweeps alternate while due, with a frozen
ten-minute age boundary per sweep. A pass visits at most 32 pages under one
three-second budget, including storage and SQL. Cold retries wait an hour after
the full cold sweep; recent sweeps wait a minute. These are bounded candidate
policies, not a production freshness guarantee: a large backlog adds whole-sweep
latency, as measured in `METRICS.md`.

Both blobs must exist before the publication transaction. A candidate one-second
row budget covers both existence checks and publication; repair transactions set
a local SQL statement timeout from the remaining row budget. The direct upload
finalizer keeps its existing timeout behavior. A row error or row-budget timeout
logs and advances that pending row so later rows can proceed and a later sweep
can retry it. Job cancellation or the shared pass deadline retains the in-flight
row and unvisited tail. Restart resets only the two local cursors and delays. Durable markers and receipts prevent repeat
contributions. Missing blobs retain their pending rows for later completion;
nothing is expired or marked published merely to shrink the backlog. Deletion
does not decrement lifetime counters. A failed best-effort publication followed by
photo deletion before repair can lose the contribution.

Community create and edit compliance share `social.community_compliance_cache`
through the existing pool. Its primary key is `(model,input_hash)`, where the hash
is 32 bytes of SHA-256 over the source's name, description and image-digest
serialization. Rows store the whole validated provider result, including rejected
content, for two hours; they do not store the input text or image. Reads require a
fresh expiry. The expiry index supports removal of at most 1,000 expired rows per
hour, rechecking expiry to preserve concurrent refreshes. Provider failures are not
cached. Cache-read SQL failures remain handler failures; a cache-write failure is
reported by the source's compliance-unavailable branch.

The compliance adapter retains one lazy Outbound provider profile per community
State. Its 600-second SDK budget ends at response headers; a progressing body may
continue beyond that budget. The native connect and read inactivity budgets are
10 and 300 seconds. Provider bodies have no separate 4 MiB cap. Source-paired
factory/SDK and native-fetch probes cover these boundaries, missing versus empty
credentials and compliance metric outcomes. Delayed DNS, combined network phases
and deployed providers remain unverified; the probes do not establish exhaustive
provider equivalence.

## Shared social registry profile facts

RPC and community profile readers share `social.registry_profile_cache` through
one existing pool. The primary key is the source registry cache key. A single
read writes the requested wallet key; a batch writes the public `avatar.userId`
key. Public profile identity never replaces the actor used for authorization or
relationship storage. Batch reads retain cached-first order and newly fetched
aliases, including duplicates admitted by the pinned source registry.

Each successful write retains the minimal public profile for 600 seconds, measured
by the database clock. Reads ignore expired rows even if cleanup is delayed.
Missing or invalid profiles are not cached. Accepted content updates do not
invalidate these facts; the source retains its previous value until expiry. The
six V1 profile-enriched community reads (community listing, members, bans,
community requests, member requests and posts) propagate cache-read failures to
their handler error boundary. Consumers that intentionally use the best-effort
profile wrapper retain that behavior. V1 members, bans and community requests
omit unresolved profile rows after database pagination, preserving the database
total; their V2 responses retain those rows without profiles. Post-author and
strict single-owner behavior remain separate. Cache writes are best effort: a
batch first uses one set write and retries individual keys if it fails, so one
failed key does not prevent successful keys from being retained.

The workload consists of indexed key/`ANY` reads, existing active-pointer/profile
lookups on misses and bounded page writes. No external await occurs inside an SQL
transaction. `registry_profile_cache_expiry_idx(expires_at,profile_key)` supports
the existing one-second leased expiry job, selecting and deleting at most 1,000
expired rows per pass. Delayed cleanup affects storage growth, not freshness.
Production new-key rates and sustained reclamation capacity remain unmeasured.
An authoritative cutover must import cached values with their remaining TTL or
drain the source cache before switching readers; this runtime procedure is not
established by the local reader and migration tests.

A newly inserted referral performs one recent-invitation query by referrer,
ordered by `created_at DESC` with a result limit of two. The existing
`referral_progress_referrer_idx(referrer,status)` supports its equality filter;
the database sorts matching history to select the two newest rows. The result is
bounded, while the scanned history grows with that referrer. A query failure after
the insert retains attribution and reaches the source handler's error boundary.
Configured timing/IP Slack calls occur after insertion and the hundred-invite
alert after the grant and accepted notification; no external await holds an SQL
transaction. Each complete Slack effect has a ten-second absolute deadline, at
most three attempts and a two-second Retry-After cap, including shared provider
permit and pause waits. A failure retains the committed state and is swallowed
at the existing boundary. Same-referrer retries exit before alert delivery.
These timing limits deliberately differ from the pinned source SDK defaults.

Registry cache keys retain the requested spelling, including case; batch key
uniqueness is case-sensitive. Only the physical content-pointer lookup normalizes
case and resolves one document per normalized pointer. Batch cache-hit matching
compares the normalized public user ID to each requested key lowercased, while
single-read errors retain the original requested key. This keeps mixed-case cache
facts separate from authenticated wallet normalization and from the public ID.

Community analytics reuses the configured Social analytics adapter and Outbound
client. It adds no SQL storage, delivery receipt, retry queue or transaction that
waits for HTTP. Join and opposite-request acceptance report the removed request
ID when one exists; no ID property is sent when none exists. Making a community
public reports the IDs of removed non-banned `request_to_join` rows, including
historical status values, as the pinned source adapter's acceptance query does.
That write can return one ID per matching request, so its returned data and
analytics payload grow with that community's matching requests. The regression
covers two accepted rows and one retained banned row; sustained large-community
load is unmeasured. Analytics fetch failures do not change the mutation response.

Client community writes retain the pinned Social source's commit boundaries.
BAN commits membership removal, then removal of that wallet's likes, emits
analytics, commits the ban, and removes its requests. A later failure preserves
completed stages; voice eviction, member-status publication and the ban
notification follow successful request removal. These are separate statements
on the existing pool, with no transaction around external delivery. Updating a
community commits its fields before thumbnail and place changes. Place removal
and insertion commit separately. Changing privacy to public then accepts
non-banned join requests in its own transaction: membership insertion and request
deletion roll back together on failure, while the earlier privacy change stays.
The atomic join-and-remove-requests operation remains separate from these
workflows. Trigger-injected regressions cover one affected member, like and
request, plus place replacement; they do not measure concurrent cutover or
large-community workloads.

## Private voice pending and media workload

`social.private_voice_chats` records call identity and both parties. Absence of
participant rows means a pending intent; accepting inserts both participants
atomically. Only pending creation commits a call without participants. It
normalizes and sorts both wallets, acquires their transaction advisory locks,
and probes pending rows in a fresh statement under explicit `READ COMMITTED`,
including when the role default is stronger. Those locks enforce one pending
per wallet across both roles. Pending expiry, rejection and disconnect delete
whole pending calls; media cleanup deletes whole media calls and cascades their
participants. No path removes the last participant while retaining its call.

Live media presence still refuses a new Social call. Historical or expired media
can coexist with a new pending intent until its own lifecycle removes it. The two
nonunique address indexes support the caller/callee membership probes without
constraining that history. Pending creation does not delete media. Expected
traffic is low-rate two-wallet writes and indexed existence probes, with at most
one pending per normalized wallet; retained media depends on its connection and
cleanup lifecycle. Capacity and production latency are not established by the
functional tests.

Community detail, listing and member-request reads fetch voice status separately
through the existing comms `VoiceDb` on the same pool and presence configuration.
An ordinary page uses one primary-key-bounded bulk read for its community IDs;
core community query errors remain fallible. Voice-query errors become idle
statuses, matching the pinned companion's SQL catch. Active-room filters and the
active-room endpoint first read all active rooms; an empty or failed voice read
returns an empty response before community SQL. This all-room query still grows
with stored voice presence. These local SQL catches do not describe direct
HTTP-provider errors in the separate Social deployment.

Signed community listings, members, bans and member-request summaries propagate
relationship-query failures through their handler boundary on the existing pool.
Members and bans read relationship facts before V1 profiles, including empty
pages; anonymous listing and member reads omit those relationship queries.
Community-request rows carry `friendshipStatus: NONE` without a relationship
read. Ordinary signed community pages use the existing accepted-friend read and
at most three matching member addresses per community; no index or storage was
added. The legacy best-effort friendship-status wrapper remains available to
its existing consumers outside these community reads.

Member-request summaries read the pending request page and its total before any
community lookup. An empty raw page returns immediately. A nonempty page adds
one community-summary query bounded to its community IDs, followed by the
existing relationship read even if every community is inactive or bans the
caller. Voice and profile enrichment operate on surviving summaries. Results
follow the saved raw request order, and total/page metadata still describe all
pending requests; visibility filtering can leave a page empty. These statements
use the same pool without a transaction or a repeated read of request rows.

Signed V1 member and ban rows retain `lastFriendshipAction` and `actingUser`
from the existing latest-action query, including SQL null when no action exists.
Their numeric status uses those same facts; this adds no SQL statement, table,
index or pool. Anonymous members omit the aliases and relationship read, and V2
returns only the numeric status. The pinned source's complete response bodies
are compared for accepted, sent/received requests, absent friendship and empty
history, with V2 and anonymous controls; no performance change was measured.

## Native referral login workload

`POST /social-api/v1/client-login` derives the invited wallet from ADR-44 signed
fetch. Its signed metadata contains `loginId` and `occurredAt`; the timestamp
limits admission to the preceding 24 hours with five minutes of future tolerance,
matching the signed-fetch clock window.
The client preserves these values across retries and signs each request afresh.
The occurrence timestamp bounds replay acceptance, while login-day accounting
continues to use the database's UTC processing date.

Most logins have no referral attribution and require only the indexed invited
wallet lookup. For an attributed wallet, `social.referral_login_receipts` stores
one receipt keyed by `(invited_user, login_id)` in the same transaction as the
login-day update or grant and its accepted notification/email delivery row.
Duplicate receipts do not evaluate the threshold again. The FK to referral
progress shares its deletion/restore lifecycle and cascades on deletion.

Under the invited-user row lock, admission caps distinct new IDs at 120 per
wallet in a rolling 24-hour window of server `received_at` values. A duplicate
does not consume another slot. Finished or rejected referrals return success
without adding receipts; banned referrers are a no-op on this native ingress,
without exposing their identity or stored IP. Those acknowledged login
observations are not retained or replayed if the ban is later lifted; a new
login observation is required. The referrer-wide advisory lock
is acquired only when the login can grant a tier, after receipt checks and
login-day processing. Lock or statement timeouts return a retryable 503.

Expiry is the later of server reception and signed occurrence, plus 24 hours.
Backdating cannot shorten quota retention. Future-dated events can retain a
receipt for up to 24 hours and five minutes, so retained receipts are not the
same count as the rolling admission limit. The primary key's invited-user
prefix serves this bounded per-wallet count; no separate receipt-time index
is needed. The existing minute worker locks and deletes at most 1,000 expired
receipts with `FOR UPDATE SKIP LOCKED`, using
`referral_login_receipts_expiry_idx(expires_at, invited_user, login_id)`.
Cleanup failure is logged and does not skip the reward pass. Delayed cleanup
does not extend accepted event age or the admission window. These are
implementation bounds, not measured production rates or throughput.

The bootstrap defines this new relation and index. Run `catalyst schema
migrate` before rolling out the new server. On an existing database, the
production migrator creates missing objects from that same definition. Step
`0025_social_referral_login_received_at` also upgrades databases that already
have the earlier receipt table without `received_at`, preserving its rows,
keys and original expiry timestamps. Each backfilled timestamp uses the UTC
server clock after the DDL lock is acquired; future inserts use the same
server-clock default. Existing `received_at` values remain unchanged. The step
is recorded once and a second migrate does not change the timestamps. The
volatile default can rewrite the table under the DDL lock; no production-sized
migration duration has been measured.

Both receipt cleanup paths require `expires_at` to have passed and
`received_at` to be at least 24 hours old. This preserves the backfilled quota
window even when a legacy expiry is already in the past. The effective
retention for such upgraded rows is at least 24 hours from each row’s
backfilled timestamp. The
workload remains the existing per-wallet receipts; no new table or index is
added, and no receipt or referral progress is reset.

Referral attribution serializes quota decisions with a transaction-scoped
referrer/IP advisory key; count and insertion share that transaction. The IP is
the actual TCP peer unless an explicitly trusted backend proxy supplies one
valid overwritten X-Real-IP. New facts use canonical addresses; the count uses
guarded inet equality over historical text, including mapped IPv4 aliases.
Malformed historical values remain stored and do not abort the count. The
existing referrer-leading index bounds the prefix; alias conversion is still
per matching row and has a measured CPU cost (see METRICS.md). No index on the
stored IP text serves this inet comparison, so the schema defines none. Finalizing
a referral serializes its referrer and counts the milestone before committing
the grant. A grant that reaches a reward tier records it in the nullable
`referral_progress.reward_tier` in the same transaction. Optional milestone Slack alerts occur after commit. At most eight such tasks
per adapter are admitted before spawning; saturation drops and logs the alert.
Admitted effects retain the ten-second deadline. Reward requests run in the existing
leased worker after login recording commits; they do not delay login acknowledgement. One nullable receipt per milestone stores a
pre-POST `reward_requested_at`, confirmed provider `reward_response` and
`reward_retry_at`. The expected workload is at most one receipt per milestone,
with a 64-row due batch every minute; this bound is not a throughput measurement.
The partial due index excludes completed and ambiguous requests. A session
advisory guard serializes attempts without an open transaction across HTTP; its
connection closes on cancellation.

Configuration failures leave the request unclaimed. After the POST claim commits,
transport errors, non-success statuses, malformed replies and a crash before
response persistence leave an ambiguous receipt that is never automatically
reissued. Operators must reconcile the outcome before restoring it to pending or
supplying a confirmed response. Additive step `0024_social_referral_reward_receipt`
imports previously confirmed object-valued `reward_paid` responses, including
receipts with missing or invalid image metadata, and quarantines uncertain pending
markers. A confirmed receipt with unusable image metadata remains confirmed and
needs metadata repair for image delivery; it must not authorize another POST. This avoids automatic duplicate issuance but cannot guarantee
exactly-once external delivery.

`reward_paid` retains the historical `{tier, reward, notified}` receipt while
outstanding effects remain. A confirmed response permits the worker to retry
local completion without issuing another reward. Image persistence, the native
notification and email delivery row, and clearing `reward_tier`/`reward_paid`
share one transaction. A SQL failure rolls back those effects together. An
imported receipt already marked notified does not send another notification.
Images remain unique per referrer and tier. External reward issuance still
cannot be made exactly once without a provider idempotency contract.

Community membership mutations use one PostgreSQL session advisory guard per
community on the shared pool. BAN, direct join, request acceptance, bulk public
acceptance, the SQL phase of ownership transfer and every federation role action
(join, leave, role, ban, unban) serialize under that guard. A federation action
that grants membership re-checks the active ban inside its guarded transaction,
so a ban committed after its pre-check refuses it.
BAN retains separate committed kick, unlike, analytics, ban and request-removal
phases. Ownership NAME validation precedes its guard; external notification,
voice and provider awaits follow release. SQLx 0.9 native acquire already owns
its cancellation cleanup. Acquisition has one absolute one-second budget
covering pool checkout, temporary BEGIN, SET LOCAL, native acquire and COMMIT.
The local server lock timeout is 750ms; the connection is marked close-on-drop
until COMMIT is confirmed, then disarmed synchronously. This bounds waiting
backend retention while preserving the same connection, autocommit and previous
GUC values after success, including a pool of size one. Normal release remains
native guard Drop. These limits strengthen coordination and bound contention;
the separate source commit phases remain independently committed.

Private media credentials serialize same-call writers before a plain party read,
then take sorted wallet transaction locks before locking/upserting the call.
A conflict preserves its existing parties, creation time and prior participants.
Pending expiry remains effective while a media writer waits for a wallet.

### Durable world admission edits and stable bans

World scene bans use `<world>:<base parcel>` keys. Genesis bans keep their entity
keys. The release script `schemas/releases/world-scene-ban-keys.sql` consolidates active and retained
world scene keys, keeping the newest ban when aliases overlap. It removes mapped
legacy aliases so an unban cannot leave a hidden effective ban. Unmapped rows are
retained. The existing retired-scene table supplies mappings during its configured
retention period; a live world scene whose deployment is positively absent is
removed during maintenance. Authority errors remain undecided.

Access edits commit a unique revision and database-clock cutoff to
`social.world_access_rechecks` in the same transaction. Community closures persist
retry intent before their advisory wakeups. Maintenance shares one room inventory
between 16 due edits and a rotating batch of 16 live worlds. Cutoffs stay after
success so an old token cannot regain access by reconnecting. World tokens carry
additive signed authorization attributes bound to world, wallet, room and mint
start time. The short `FOR SHARE` stamp orders against existing access writers.
The `dcl.worldAuthorization` participant attribute survives normal client metadata
replacement. Previously issued signed metadata proofs remain readable when the
attribute is absent; an invalid present attribute fails closed. Legacy unsigned
tokens and key rotation may require a fresh mint after an access edit. Existing main ownership, ENS and scoped grant policy
still decides admission.

The caller secret budget keeps the existing `attempts/window_start` table and
configured limit. Mint, adapter and scene-host paths share it. Attempts charge
before verification; cancellation keeps the charge until expiry. Non-invalid-secret
answers refund only their exact charged window. Expiry cleanup deletes in batches
of 1,000 rows and continues while a batch is full. The 10-second sweep budget is
checked between batches, so an in-flight batch can overrun it; it is not a hard
deadline. This does not add a cross-caller world ceiling.

For rollout, stop all old API and scene-host replicas before running `catalyst schema migrate` and then applying
`schemas/releases/world-scene-ban-keys.sql` with the administrative database role, then start the updated binaries. `catalyst serve` refuses startup while active or retained
world scenes still have mapped entity-id bans awaiting this conversion; fresh databases and
unmapped or Genesis bans do not require it. Old binaries cannot enforce the
new stable ban keys. A binary-only rollback is not supported after rekeying;
restore the matching pre-migration database backup. Do not reset a database.

### World admission SQL and completed access checks

World admission stamps and shared-secret refunds each use one versioned SQL
function. The function names include their signature, settings and body hash.
The schema object pass installs the functions on existing databases. Run
`catalyst schema migrate` before starting the new binary; `catalyst schema functions`
also prints their administrative installation SQL. Existing function versions remain
available to old binaries during a rollout. Admission returns unavailable if its
required functions are missing.

The stamp retains `FOR SHARE`. It must wait for non-key access updates from older
binaries as well as newer access writers. A function-local 500 ms lock timeout
restores the caller's setting after success or failure. A one-second client deadline
bounds the stamp and refund wait, including pool acquisition. The server retains
the pool's configured statement timeout; a client timeout does not prove that a
dispatched server statement stopped. Stamp timeouts return unavailable, and a
refund timeout leaves the reservation to expire unless its dispatched refund completes.
Refunds lock and modify
only the exact caller window charged by the reservation; the positive-attempt
constraint and the existing table layout remain unchanged.

A completed world access check retains its revision and authorization cutoff, but
sets `next_attempt_at` to PostgreSQL infinity. Completion compares both the revision
and the exact claimed schedule, and checks that the lease is still unexpired after
acquiring the row lock. A newer edit, later claim or same-revision community wakeup
cannot be consumed by an older pass. Completion-write failures are logged and leave
retry intent intact; they do not stop the remaining batch or the live-world sweep.
Community closure advances the schedule with `LEAST`, including completed rows. The live-world sweep still reads retained
cutoffs. After completion, stale-token re-enforcement relies on that rotating
sweep rather than repeated due-queue work: visiting all live worlds takes
`ceil(live_worlds / 16)` maintenance ticks (approximately that many minutes at
the normal 60-second cadence). Outages and slow calls can extend the wall-clock
latency; this is not a guaranteed deadline. The application decodes only finite
claimed schedules and never decodes infinity as a Rust timestamp. This change adds
no migration column and does not relax the stable-ban migration's coordinated
rollout requirements.

## Segment webhook events workload

`POST /events-notifier/forward`, `/forward-web` and
`/forward-onboarding-checkpoint` are the Segment destination webhooks the
events-notifier served. The first two store the event they accept as one row of
`social.client_events`, inside the request, after the signature, the context
checks and (client events) the auth chain verified. Writers are those two
handlers at the destination's delivery rate: the explorer reports a
`move_to_parcel` per parcel crossing and the distance and height events on
thresholds, so the expected order is one row per active player per minute; no
production rate is measured. Readers are consumers scanning forward in
insertion order (`seq`) from a cursor of their own (the badge processor, below)
and the purge;
the trigger `client_events_notify_trigger` announces each stored row's identity
columns (never the metadata) on the `social.client_events` channel for
listeners outside the request path.
That channel is a data-exposure surface: Postgres grants `LISTEN` to every
role that can connect, so each such role receives the wallet, the IP, the
anonymous and session ids, the realm and the event names of every stored
event; the database's roles are the node's own and the flagging service's, and
a role granted to anyone else reads this feed. The identity columns are
bounded at 512 bytes each and free of control characters by the parsers (a
longer or control-bearing optional value is dropped, such a `messageId`
refuses the event), so the escaped payload stays under `pg_notify`'s 8000
bytes and the key under the index's limit.
A row is about 0.3-0.6 KB: `metadata` holds the sub-type part only (parcel,
emote, passport, distance and step count, height, runtime; the web identify's
agent, page and locale). The auth chain is verified at the ingress and not
stored: no consumer reads it.

Invariants: `key` is Segment's `messageId`, so an at-least-once redelivery
inserts nothing (`INSERT ... SELECT ... WHERE NOT EXISTS` on the key, then
`ON CONFLICT (key) DO NOTHING` for a concurrent duplicate; the check comes
first so a redelivery draws no `seq`); `address` is the
owner of the auth chain the event carried (client: the chain proves that the
wallet once delegated to a key, not that it sent this event, so a consumer
reads the row as a claim about the wallet, not by it, and never flags a
wallet on `source = 'client'` rows alone) or `traits.ethAddress`
(web), lower-cased and checked by the table; rows are immutable; `reported_at` and
`received_at` are the client's and Segment's clocks and `created_at` the
node's. Nothing references an event and an event references nothing: no FK.
Deletion is the `client-events-purge` job only: hourly, leased, it deletes rows
whose `created_at` is older than `CLIENT_EVENTS_RETENTION_DAYS` (default 30)
in batches of 5,000 keys through `client_events_created_idx (created_at,
key)`, at most 200 batches a pass (a million rows an hour, some 280 inserts a
second sustained). The badge processor's cursor stays above the purge horizon
while it runs: it passes rows seconds after their insert, so the purge only
reaches rows a stopped processor never read (their badges are then not
counted, which is the accepted cost of the retention).

The bootstrap (`schemas/40_social.sql`) defines the table, the indexes and the
trigger; an existing database gains them from the object pass. Step
`0038_social_badge_processor` adds `seq` to a table that predates it as a
nullable column, then its default (`nextval` of `social.client_events_seq`,
named as text so the step precedes the object pass that creates the
sequence): both change the catalog only, no row is rewritten, and the kept
rows keep a NULL `seq` (they precede every cursor, which starts at the head).
Both take ACCESS EXCLUSIVE on the table, and `migrate` runs the steps and the
object pass in one transaction, so the lock is held until it commits: reads,
the webhook inserts and the purge all wait for the rest of the migration.
The object pass then builds `client_events_seq_idx` (`seq`) and
`client_events_session_moves_idx` (`address, session_id, seq` over
`move-to-parcel` rows), both partial on `seq IS NOT NULL`, in that
transaction, one heap scan each with nothing to sort. On 2M
synthetic rows (822 MB, about two months at the expected rate) the step took
22 ms and the two builds 0.24 s and 0.35 s with a warm cache; the identity
column it replaces rewrote the same table in 27.7 s under ACCESS EXCLUSIVE.
To take even that off the migrate window, add the column by hand first
(`ALTER TABLE social.client_events ADD COLUMN IF NOT EXISTS seq bigint`, the
step's first statement, without the default: the sequence does not exist
yet), then build both indexes with the definitions of `schemas/40_social.sql`
and `CREATE INDEX CONCURRENTLY`; `migrate` then sets the default, creates the
sequence and skips the indexes (a failed concurrent build leaves an invalid
index: drop it before retrying).
`client_events_seq_idx` serves the cursor scan and
`client_events_session_moves_idx` traveler's read-back of one session.

## Credits wallet flags workload

`credits.wallet_flags` (`schemas/60_ops.sql`) is the multi-accounting flag
notified on the `credits.wallet_flags` channel, `{action: "flag", addresses,
source, ip}` (`shop/src/credits/wallet_flags.rs`), or posted through the
admin, `POST /credits/admin/flags {addresses, source, ip}` (the shape the
events-notifier posted to the credits server's `/flag`): one row per flagged
wallet, keyed by address, with the source that raised it, the IP it reported
and the wallets it was linked with (`linked_addresses`, the body's list). The
node runs no detector: it keeps the flags it is told of.

Writers are the leased `credits-wallet-flags` listener (where
`OPS_WRITES_ENABLED` is on) and the admin's `POST /credits/admin/flags`, the
same upsert per address with one `ops.admin_audit` row per wallet in the same
transaction (the listener's actor is `pg:credits.wallet_flags`), the notified
or admin unflag (`DELETE /credits/admin/flags/{address}`) and the credits
operator's unblock
(`admin_store.rs` `admin_set_blocked`), which deletes the wallet's flag in the
transaction that clears its block. Readers are the credits claim (`EXISTS` by
address, folded into the captcha claim and the balance reports as
`is_blocked_for_claiming`; the spend and the reservation read the operator's
block alone, so a flag never holds credits a wallet has) and the admin list
(`GET /credits/admin/flags`, by address or paged by `flagged_at`). Rows are
small and few: a flag per wallet reported.

The channel's writers are whoever can connect: Postgres grants no privilege on
`NOTIFY`, so every role of the database can raise or release a flag, and the
listener applies it as an audited admin action. The roles are the node's own
and the flagging service's; a role granted to anyone else can flag wallets, and
a flag raised while no listener is connected is lost. A queue table under `GRANT
INSERT` to one role, or a `SECURITY DEFINER` function, would name the writer
and keep the flags raised while no listener is up: the next step if the
database gains other roles.

Invariants: a flag is a signal, not a verdict: it holds the credits claim
alone, and the admin's unflag or the operator's unblock deletes it; nothing
else does, and a flag never expires. A wallet has at most one flag (the
primary key); flagging it again keeps `flagged_at` and takes the new source,
IP and links. Addresses are lower-cased and checked by the table; the handler
normalises the body's. The bootstrap defines the table; an existing database
gains it from the object pass, so no step exists.

## Camera reel photo events workload

`camera_reel.photo_events` (`schemas/50_create.sql`) is the append-only outbox
of the photo events camera-reel-service published to SNS: `photo-taken`, one
row per finalised upload, written in the transaction that sets
`camera_reel.images.ranking_published` (public and private photos alike), and
`photo-privacy-changed`, one row per visibility change, written in the
statement that changes it. `payload` is the upstream body `{type: "camera",
subType, key, timestamp, metadata}`. Writes follow the upload rate.

The Credits photo-goal worker reads committed `photo-taken` rows lacking a
receipt in `credits.photo_goal_receipts`. Receipt and effects commit together;
the outbox holds no consumer state. This anti-join also sees transactions
that commit after a higher event ID has already been processed. A sequence
ID alone is not a safe consumer watermark. The community photo counts are
applied directly in the upload's finalising transaction and do not read the
outbox.

Invariants: rows are never updated; the leased
`camera-photo-events-retention` job deletes rows older than 30 days, 1000 per
statement by `created_at` (its index), at most 100 statements an hourly pass,
so the table holds about 30 days of events and a consumer further behind
misses events. `user_address` is the photo's owner, lowercase. The bootstrap
defines the table; an existing database gains it from the object pass, so no
step exists.

## Seasonal photo rewards workload

`shop::credits::goals` reads Camera photo facts and Social subscription facts
using the existing database pool. `credits.seasons` and `credits.season_goals`
hold season windows, MANA limits and the `take-photo` assignment. These tables
have no active monetary seed. `credits.program_opt_outs` records enrollment
withdrawal separately from the paid wallet's identity.

The worker writes one durable `credits.photo_goal_receipts` row per processed
photo, including terminal ineligibility decisions. Eligible photos update
`credits.goal_progress`, keyed by wallet, season, week and goal. Completion
creates one `credits.goal_grants` row with immutable signature terms and one
keyed Social notification with its delivery records. All four effects share
one transaction. A failed signature or notification rolls back progress and
the receipt so the event can be retried. No network call runs inside this
transaction. Per-wallet advisory locks serialize progress and enrollment;
claim serialization enforces the weekly and season limits.

MANA amounts use integer wei in `numeric(78,0)` and exact integer arithmetic.
The signed grant binds wallet, chain, manager, amount, expiry and salt.
`credits.usages` supplies consumption by the same chain, manager and credit
hash. Goal grants never credit or settle the paid USD ledger. Wallet-scoped
reads use the progress and grant primary-key prefixes. The partial receipt
index on `(address, season_id, week_id, ordinal)` for counted photos serves
the ordered `photosTaken` lookup; the pending-photo query uses the unique
receipt identity. `METRICS.md` records the synthetic access-path measurement.
Production volume and latency remain unmeasured.

The worker runs through the existing leased job registry under the Ops write
gate. Receipts survive photo deletion and event retention; changing visibility
does not undo a reward. The Camera event retention window still bounds crash
recovery: a worker more than 30 days behind cannot recover deleted facts.
These new relations are created by the schema object pass on existing
databases, without resetting wallet, photo or notification data.

Credits keeps its own in-memory feature-flag snapshots, fetched with the
configured Credits audience outside SQL transactions. Social's persisted
`dapps` answer and Explorer's site answer cannot authorize these rewards.
Local flag overrides remain in `explore_api.feature_flags_local`; missing
remote policy is retryable rather than a completed photo decision.

`credits.captcha_attempts` holds a wallet's failure count and most recent
failure time. Three failures block claims for 24 hours from the last failure;
a successful solve clears the row. The existing `credits.captcha_challenges`
table records `generated_at` for the minimum solve-time check and keeps a
two-hour challenge lifetime. Both challenge and failure updates take the
same wallet lock. The additive `0043_credits_captcha_generated_at` step
preserves existing challenges and establishes their solve-time baseline.

## Badges catalog and progress workload

The catalog (`social.badge_definitions`, `badge_tiers`, `presence_badges`;
`schemas/40_social.sql`) is reference data: upstream's 34 badges and 84 tiers,
seeded by the bootstrap, a few dozen rows that change through the admin catalog
routes (below) or the TS database import. Every read route reads the published
rows in `created_at, id` order per request, as upstream does: one statement of
about 0.3 ms and 30 KB, so an import or a catalog edit shows at once
(`social/src/badges/store.rs`). Per
wallet, `user_badge_progress` holds one row per badge touched (its
`steps_done` plus upstream's `progress` document verbatim: the evaluators'
dedup and resume state, a few KB at most) and `user_achieved_tiers` one row per
earned tier; reads load both for one address in one statement.

Writers of progress go through `badges::progress::apply_progress` in their own
transaction: the badge row `FOR SHARE` only when published and not archived
(upstream's awardable rule, `published_at IS NOT NULL AND archived_at IS
NULL`), a transaction advisory lock per (address, badge) that also serializes
the console revoke, then the document, the counter, the newly reached tiers,
completion and, unless suppressed, one idempotent `badge_granted` notification
per new tier. The console grant raises the steps to the requested tier's
target through it. The backfill (`POST /badges/badges/{id}/backfill`,
`social/src/badges/backfill.rs`) runs one such transaction per wallet (a
statement error fails that wallet; a lost connection fails it and reports
every later wallet as not attempted, at once): it
takes the same badge lock and advisory lock first, merges upstream's way over
the progress it reads under them, writes through `apply_progress` without
notifications, then sets the merger's instants on the earned tiers and the
completion.

Invariants: `steps_done` equals the document's `steps`; earned tiers and
completion are never taken back by a write (only the console revoke deletes
them); a tiered badge completes at its last tier, a single badge at
`criteria_steps`; an archived badge stays listed and kept by its earners and is
never awarded; an unpublished one is invisible. A database stamped before step
`0037_social_badges_catalog_parity` keeps its four placeholder rows: the step
gives `open_for_business` and `emotionista` upstream's content and leaves
`decentraland_citizen` and `walkabout` unpublished. It also fits the old
progress rows to the new targets: a tiered row keeps completion only when the
wallet holds the last tier and its steps stop at its highest held tier's
target (no unearned tier on the next write), and a completed single badge
reads its target as reached. The rest of upstream's
catalog reaches it through the TS database import.

### Badge processor

The writers of evaluated progress are four leased jobs
(`social/src/badges/jobs/`, on where `BADGES_PROCESSOR_ENABLED`, off by
default, and social writes are), each a reader of tables other areas own, with one row of
`social.badge_cursors` per source:

- `social.client_events` by `seq` (citizen, traveler, walkabout, moves
  master, social butterfly, vertical voyager), traveler resolving a parcel's
  scene title through `content.active_pointers` and `content.deployments`.
  The values are the client's claim, as upstream took them, within bounds:
  a `walked-distance` event counts at most 10,000 steps (the explorer
  reports on thresholds far below it), a `vertical-height-reached` height
  above 5,000 m is passed (no scene is that tall) and a `passport-opened`
  receiver must be a wallet address;
- `content.deployments` by `id` (rarity badges, profile pro, land architect,
  the store half of open for business), a wearable's rarity from its active
  entity or `shop.items`;
- `shop.sale_event` (`mint`, `bid_accepted`), `shop.trade_event` (`traded`)
  and `shop.nft_event` (`order_successful`) on Polygon by (block, log index),
  each up to the block its index published in `shop.chain_cursor`
  (emotionista, fashionista; one step per transaction hash, a legacy order
  indexed before `nft_event.tx_hash` was recorded counted by its chain
  position until a re-index fills the hash). An off-chain good counts for
  its `beneficiary` in `trade_event.assets`; a legacy order bought by a fiat
  or cross-chain relay counts for the token's last `transfer` recipient after
  the order in the same block, and for nobody while a relay holds it (one
  `nft_event_token_idx` probe per such order). Step
  `0040_shop_nft_event_tx_hash` adds `tx_hash` as a nullable column without
  a default (catalog only, no row rewritten); it takes ACCESS EXCLUSIVE on
  `shop.nft_event`, held until `migrate` commits, so the nft index's writes
  and the reads of the table wait for the rest of the migration. A sale of
  an item `shop.items` lacks holds its log's cursor until the item is
  mirrored or the items mirror completes a pass an hour after the sale (a
  pass walks every item, minutes at the defaults); then it is passed without
  effect. Without
  `MARKETPLACE_MIRRORS_ENABLED` no pass ever completes, so the sales job is
  not started (logged at startup);
- recounts of `shop.items` and `shop.collections` on Polygon (once
  `shop.mirror_state` records a complete pass) and of accepted
  `social.referral_progress` rows (creators, the collection half of open for
  business, referral), for the wallets whose stored steps are behind. A pass
  walks those wallets in address order and records the last one recounted
  (`badge_cursors.resume_after`), so a wallet the evaluator cannot move (an
  imported document whose flag is truthy but not `true`) never starves the
  ones after it; `position` counts completed passes. A batch reads its
  creators' items in one statement through `items_creator_lower_idx`
  (`lower(creator)`: the mirror lowercases, no constraint says so): 20 ms
  for 200 creators on 1M synthetic items (781 MB), where the unindexed
  per-creator read scanned the table once per creator. The candidate scan
  itself reads the table once per batch (150 ms on the same rows). The
  object pass builds the index non-concurrently in the migrate transaction,
  SHARE on `shop.items` until it commits (the mirror's upserts wait): 1.7 s
  on those rows; it can be
  built beforehand with `CREATE INDEX CONCURRENTLY` and the definition of
  `schemas/30_shop.sql`.

One transaction holds a batch (`BADGES_PROCESSOR_BATCH_SIZE`, default 200):
the cursor row `FOR UPDATE`, the batch's progress writes through
`apply_progress` and the cursor advance, so a database error rolls the batch
back and the next tick reads the same rows; a row an evaluator cannot read is
logged and passed. Invariants: a cursor never moves past a row it did not
pass; its first pass records the source's head and replays no history; a
recount's first pass writes no notification.

The id cursors (`client_events` by `seq`, `deployments` by `id`) never skip a
committed row. A sequence value is drawn before its transaction commits, so
ids become visible out of order: the sync flushes insert concurrent batches,
and a younger transaction can hold a lower id than rows an older one already
committed. A batch therefore passes every visible row past the cursor and
records each run of ids it passed without seeing in
`social.badge_cursor_gaps` (`lo..=hi`, the `horizon` and `seen_at` of the scan
that found it). The invariant: every id at or below a cursor's position is
processed or inside a gap of that source. Each batch first re-reads the gaps
and processes the rows that appeared (out of id order, as upstream's queue
delivered them), then drops a gap only when both hold:

- its `horizon` is at or below the `xmin` of a snapshot taken before the
  re-read that still misses it. The horizon is the transaction id of the
  batch that found the gap, which reads the cursor without a lock, scans,
  and only then locks the cursor row, which assigns that id: every
  transaction that had an id at the scan, so every one that could hold an id
  of the gap, has a lower one. (A snapshot's `xmax` is one past the highest
  completed id; a transaction still running can be above it.) Once `xmin`
  reaches the horizon all of them have ended, and a committed row would have
  been visible to the re-read. A batch that finds the cursor row changed when
  it locks it (a second runner after a lease expiry) writes nothing;
- it is older than the grace (600 s) and was then re-stamped. A writer draws
  the value inside its INSERT before it takes its transaction id (the row's
  default is evaluated before the heap insert assigns the id), so a writer
  between the two at the scan is not under the horizon. Usually that window
  is one row's projection, but an `ON CONFLICT` pre-check can wait in it on
  another open transaction inserting the same key. So the first batch past
  the grace replaces the gap's `horizon` with its own transaction id
  (`restamped`), and the gap is dropped only once `xmin` reaches that one
  too: every transaction such a writer could be waiting on has then ended.

Values that never commit (a rolled-back flush, a value an `ON CONFLICT DO
NOTHING` burned, as a concurrent duplicate Segment delivery does) are what
expire. A long
write transaction anywhere on the cluster delays expiry, never a row: rows
are processed as soon as they are visible. A source holds at most 5,000 gap
ranges; at the cap its cursor passes no row that would open another until
gaps resolve (it holds, it does not skip): alert on
`badges_processor_cursor_gaps` well before 5,000. The first pass starts at the
head, so a row still uncommitted below the head when a cursor is created is
history and not read. Each tick publishes, per source,
`badges_processor_cursor_lag` (ids behind the head; blocks behind the
published block for the sales logs), and for the id cursors
`badges_processor_cursor_gaps` and `badges_processor_cursor_oldest_gap_seconds`:
a cursor that stops advancing shows as a growing lag, and a horizon held by a
transaction left open on the cluster as gaps that keep ageing. A deployment stored
more than `BADGES_DEPLOYMENT_MAX_LAG_SECS` (default off, as upstream had no
such filter) after its entity timestamp is history a syncing node caught up
on and is passed without effect; `BADGES_EVENT_MAX_AGE_SECS` (default off) is upstream's
`EVENT_PROCESSING_MAX_AGE_IN_SECONDS`. Step `0038_social_badge_processor`
adds `seq`; the object pass creates `social.badge_cursors`,
`social.badge_cursor_gaps` and both indexes.

### Presence badges flow

Input is every verified LiveKit join and leave (the gatekeeper's
`RoomEventStore`), including one the gatekeeper's own session ordering
supersedes, such as a reconnect's old leave that follows the new join: wallet
identities only, scene and world rooms only, with LiveKit's participant sid as
the session. The webhook inserts each into `social.presence_events` before it
answers, keyed by LiveKit's event id, so a redelivered webhook is one row; a
failed insert answers 503 and LiveKit delivers the event again. Only events in
a place a badge could count now are kept: a 5 s snapshot of the published badges' worlds, parcels and the
scene entities deployed on them (one GIN-indexed `entity_pointers &&` read),
with the engine's lookahead and lookback. A scene entity the snapshot does not
know (a redeploy since) is looked up by `entity_id` in the webhook that carries
it, under the webhook's lock stripes, and dropped
only when it is deployed off every badge parcel; one the node has not synced
is kept for the engine. Every event is kept when the snapshot cannot be read.
A badge published or moved earlier is seen within 5 s; joins before that are
not counted, and the player's next join is. Expected volume is every
join and leave of the scene and world rooms of active presence badges while
`BADGES_PRESENCE_ENABLED` is on (unmeasured); rows live only
until the next pass of the leased `badges-presence` job (5 s), which applies
up to 500 in `seq` order, one transaction per event that deletes it, so the
queue is its own cursor and a row committed behind a later `seq` is still
there on the next pass. The clock (bookkeeping times and the 5-minute future
guard) is read per event, so a long drain stays current. Only a deterministic
failure counts toward dropping an event (after 3 passes, logged and counted);
a connection, pool, timeout, conflict or failover error is retried on the next
pass without counting until the event has been queued for an hour, then counts
too, so one event cannot hold the queue. `badges_presence_queue_oldest_seconds`
reports the age of the oldest queued event each pass.

Every presence transaction first takes one advisory lock: the engine and the
`badges-presence-sweep` job are a single writer fleet-wide, so the runtime
tables need no per-session locks. The runtime state is upstream's
(`presence_visits` keyed by badge, wallet and room, cascading from
`presence_badges`; `presence_applied_events`, `presence_pending_leaves` and
`presence_absence_facts` keyed by their facts), in epoch milliseconds where a
value meets a LiveKit event time or the `intervals` document. Indexes follow
the statements: `(wallet, realm_name)` for a leave's open visits,
`opened_at` and `(last_checked_at, opened_at)` for the sweeper's due visits,
and each fact table's retention column for the bounded prune (batches of
`BADGES_PRESENCE_PRUNE_BATCH_SIZE` rows, 1,000 by default, per table until one
comes back short, at most 20 per sweep, after the redelivery window,
`BADGES_PRESENCE_APPLIED_EVENTS_TTL_SECONDS`, 7 days by default). Completed badges are left out in one statement per event,
so a wallet with nothing left to earn in a place records no visit or pending
leave there. A scene badge resolves
the event's entity to its parcels through `content.deployments` (`entity_id`
is unique); a world badge compares names. Progress is written through
`apply_progress` with the progress document `{ steps, intervals }` kept as
upstream writes it, so an imported document resumes. As upstream's catalog
requires, a badge is counted only when its row is single (not tiered), its
`criteria_steps` is its rule's (`required_seconds` for a stay, 1 for a visit)
and no evaluator or import of its own (other than a one-time import) writes
it; another row is skipped and counted as `badge_invalid` (logged once per
badge and reason), so the
flow never overwrites another writer's progress. Invariants: a session
counts only from its own join to its own leave, never past the first
occupancy observation that missed the wallet; an ended session never reopens
for that badge; overlapping connections count once; a stay completes at the
instant the union reaches `required_seconds`. A grant writes its
`badge_granted` notification unless `BADGES_PRESENCE_GRANT_NOTIFICATIONS` is
false. The sweeper reads this process's LiveKit occupancy observation
(`comms::presence`) and only closes visits; no observation, or an empty one,
changes nothing. `BADGES_PRESENCE_SWEEPER_ENABLED=false` stops only these
occupancy checks; the maximum age and the prune still run.

Upstream's tuning is environment settings, in seconds and with upstream's
defaults (shared with `presence::Settings::default()`; each duration at most
30 days, each batch at most 100,000):
`BADGES_PRESENCE_EARLY_JOIN_LOOKAHEAD_SECONDS` (21,600; 0 keeps no early join),
`BADGES_PRESENCE_OPEN_VISIT_CHECK_AFTER_SECONDS` (900),
`BADGES_PRESENCE_OPEN_VISIT_MAX_AGE_SECONDS` (86,400),
`BADGES_PRESENCE_APPLIED_EVENTS_TTL_SECONDS` (604,800),
`BADGES_PRESENCE_ABSENT_EXPIRE_AFTER_SECONDS` (3,600),
`BADGES_PRESENCE_SWEEP_INTERVAL_SECONDS` (60), `BADGES_PRESENCE_SWEEP_BATCH_SIZE`
(100), `BADGES_PRESENCE_PRUNE_BATCH_SIZE` (1,000),
`BADGES_PRESENCE_GRANT_NOTIFICATIONS` and `BADGES_PRESENCE_SWEEPER_ENABLED`
(both true). Upstream names them without the `BADGES_` prefix and gives the
sweep interval in milliseconds.
### Adding a badge

A running node takes a new badge through the bearer-gated catalog routes
(`social/src/badges/catalog.rs`, bearer `CATALYRST_BADGES_ADMIN_TOKEN`, writes
refused while `SOCIAL_WRITES_ENABLED` is off). A schema step cannot insert
catalog rows, and the bootstrap seed reaches only new databases.

1. `PUT /badges/badges/{id}` with the definition in the shape `GET
   /badges/{id}` reads: `name`, `description`, `category` (upstream's enum),
   and either `criteria: {steps}` (a single badge) or `tiers: [{tierId,
   tierName, description, criteria: {steps}}]` in tier order (a tiered one,
   steps strictly increasing). `assets` (`{"2d": {...}, "3d": {...}}`, paths
   relative to `BADGES_PUBLIC_ASSET_BASE_URL`) defaults to upstream's CDN
   layout: `<id>/{2d,3d}/...` for a single badge, `<id>/<slot>/{2d,3d}/...`
   for a tier whose id is `<id with _ as ->-<slot>`; any other tier id names
   its assets. Every problem is one 400. A new badge is created unpublished
   and invisible (201); a repeated `PUT` replaces the content in place (200),
   keeping publication, archive and `created_at`. A `PUT` that would remove a
   tier a wallet holds, or switch a badge with player rows between single and
   tiered, is a 409 and changes nothing; a removed tier nobody holds is
   deleted from `badge_tiers`. Player rows are never written: a lowered
   target awards on the wallet's next progress write (for a processor badge
   other than traveler, the next one that gains steps, one tier per step or
   event), not retroactively, and a tier added above a completed badge
   leaves it complete until such a write.
   A badge takes at most 32 tiers, each `2d` or `3d` at most 8 files, and
   the badges routes refuse bodies over 1 MiB (413), the backfill's over
   64 MiB.
2. Make the art reachable: upload it to the CDN that
   `BADGES_PUBLIC_ASSET_BASE_URL` names (upstream's by default), or mirror it
   into `BADGES_ASSETS_DIR` under the same relative paths when the base is
   `<node>/badges/assets`.
3. `PATCH /badges/badges/{id}` `{"published": true}`. The badge is listed and
   awardable. An active badge with no evaluator, backfill import or presence
   configuration (`social.presence_badges`) is named in a warning at startup,
   since only console grants award it.
4. Grant it per wallet with the console grant (`POST
   /badges/users/{address}/badges/{id}`), which awards it through
   `apply_progress`.

Retire a badge with `PATCH {"archived": true}`: it stays listed and kept by
its earners, and `apply_progress` answers `NotAwardable`. `{"archived":
false}` reopens it. `{"published": false}` hides a badge and is a 409 once
any wallet has a row of it. Each upsert (with the definition it wrote) and
each publication or archive transition appends a row to
`social.badge_catalog_audit`; `badge_admin_audit` keeps per-wallet grants and
revokes, whose `address` a catalog write does not have.
