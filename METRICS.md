# Measured query workloads

## Seasonal photo progress

A disposable PostgreSQL 18 fixture with 1,000,000 photo-goal receipts and
500,000 retained Camera events measured the wallet/season/week `photosTaken`
query with `EXPLAIN (ANALYZE, BUFFERS)` after `ANALYZE`. Returning two counted
photos used a parallel sequential scan in 26.199 ms. The partial index on
`(address, season_id, week_id, ordinal) WHERE outcome = 'counted'` selected an
index scan in 0.030 ms with five shared-buffer reads. Its measured size was
67 MB. Including `photo_id` increased it to 82 MB without improving this
sample's execution time (0.031 ms), so the index has no included columns.

The global oldest-unprocessed-event diagnostic took 182.756 ms with one
pending event and 173.302 ms with none. Replacing the aggregate with an
ordered limit still selected a hash anti-join and did not reliably reduce
the work. The worker samples this diagnostic separately from photo
processing, at most every five minutes, with a query deadline; diagnostic
failure does not roll back completed work or prevent that pass's processing.

These are single local samples with synthetic distributions and uncontrolled
cache state. They establish the selected access path and the cost of the
diagnostic in that fixture, not production latency, throughput, index write
cost or capacity.

## Social notification polling and oversized-event expiry

The explicit `rpc::tests::fanout::notification_and_spill_queries_use_bounded_index_paths`
PostgreSQL test uses its own temporary database and inserts 5,000 community rows,
with 1,250 tracked communities and 2,500 independently cased notification keys,
and 5,000 temporary payload rows with 10 expired entries inside a rolled-back
transaction. The statements run `EXPLAIN (ANALYZE, BUFFERS)` after `ANALYZE`.

The plan selects `community_voice_notifications_pkey` directly
for an ordered raw-key page: 0.027 ms execution, 64 returned rows and four
shared-buffer hits. The ordered expiry selection uses
`event_payloads_expiry_idx`: 0.014 ms execution, 10 rows and 13 shared-buffer hits
including row locks. Both statements cap returned work at 64 rows. The test checks
that these indexes remain usable. Polling cost follows tracked raw keys, which can
outnumber canonical communities; the fixture measures two keys per tracked room.

These small warm-cache plans establish access paths, not production throughput or
capacity for the 512 MiB payload admission bound. Production active-room counts,
large-event frequency, cold-cache behavior, index write cost and maximum-payload
memory/latency remain unmeasured.

## Social mutation burst fanout

The real-NATS tests run an owned broker and two links, with 1,000 updates for one
local subscription. A targeted block burst delivered all 1,000 updates in order,
without origin loopback duplication, through awaited publisher admission and the
synchronous target callback. The community test uses PostgreSQL membership reads,
a pool of eight connections, and 100 interleaved messages on a control subject.

With the 1,000-entry community queue and four concurrent deliveries, the fixture
delivers all 1,000 updates; the 100 control messages arrived in
24.757 ms. A 256-entry serial queue in the same fixture delivered 331 of 1,000
updates, with controls in 16.145 ms. This is one small warm-cache burst fixture,
not a sustained throughput, multi-community fairness or maximum-payload benchmark.
SQL concurrency is capped by the configured pool size; ingress beyond the queue
bound can drop transient events and increments the delivery-drop counter.

The targeted burst harness records readiness, submission, broker observer counts
and both subscription queue lengths. Within the full Social suite it reaches
readiness in 5.007 seconds, finishes submission at 5.021 seconds and observes
all 1,000 frames at both broker links and both queues by 5.027 seconds; community
updates and the 100 controls arrive in 28.482 milliseconds. These are single
samples without controlled system load. The harness has a 10-second delivery
deadline and exact count, ordering and duplicate
assertions; that deadline is not a protocol latency guarantee.

## Ordinary Social RPC writer

The source transport fixture executes the pinned TypeScript transport with a
controlled socket and virtual timers. Its 17 cases cover queue capacity,
backoff, drain callbacks, accepted-once handling, closure and explicit unsupported
configuration edges. A real socket test preserves an 8 MiB profile
URL in a unary response, then stops reading and observes transport retirement
after bounded retries of subsequent responses, in 1.06 seconds. These checks do not measure native uWS buffer behavior, kernel
capacity, sustained throughput or maximum-memory consumption. Accepted bytes are
charged through the existing writer's physical flush; pending-message storage
grows on demand up to the configured finite threshold.

## Private-call RPC UUID boundary

The private identifier replay samples 128 sequential SQLx executions of the bound
scalar `SELECT $1::text::uuid` on the existing test pool. On local PostgreSQL 18.6,
over loopback with a warm statement cache and concurrent builds, one sample
measured p50 133 microseconds, p95 848 microseconds and maximum 5,001
microseconds. These timings include pool acquisition and the query round trip.
The sample adds no application-table reads or row locks. Hardware details, p99,
sustained throughput and production-load latency are not recorded. The RPC
performs this extra query; the measurement does not claim source performance parity.

## Community ranking and compliance cache

A disposable PostgreSQL 18.6 database uses the schema and query text in
`services/catalyst/social/src/communities/ranking.rs` and `compliance.rs`,
including the cache expiry recheck. The fixture contains 100 communities, 100,000 memberships, 10,000 posts,
1,000 community places, 100,000 photos, 100,000 ended local event occurrences and
100,000 compliance results. These are single `EXPLAIN (ANALYZE, BUFFERS)` samples
from a developer machine during a concurrent build, with uncontrolled cache state.

| Statement | Fixture result | Execution time |
| --- | --- | ---: |
| Ranking facts for 100 communities | 100 rows | 100.389 ms |
| Empty pending-photo page, partial index | 0 of 100,000 | 0.045 ms |
| Same empty page, index scans disabled | 0 of 100,000 | 30.039 ms |
| Pending-photo page, partial index | 50 of 100,000 | 0.166 ms |
| Photo receipt and counter fanout | 100 communities | 11.098 ms |
| Unbounded ended local occurrence scan | capped at 100 of 100,000 | 538.459 ms |
| Fresh compliance result by primary key | 1 of 100,000 | 0.057 ms |
| Expired compliance deletion | capped at 1,000 of 100,000 | 85.488 ms |

The partial index avoids a full photo-history scan when no repair is due. Cache
reads use the primary key; bounded expiry selection uses the expiry index.
Ranking aggregates depend on membership data. The event collector
bounds source reads and recurrence expansion as measured below. Repeated latency
percentiles, production distributions,
concurrent writer throughput, cold-cache performance, hardware characterization,
receipt growth and replica lag remain unmeasured. These samples establish query
paths and bounded output sizes, without establishing production capacity.

## Bounded local event collection

A disposable PostgreSQL 18.6 comparison uses actual query text, 100,000 already
receipted ordinary events and then 100 additional schedules of 1,000 recurrence
positions each. Three warm `EXPLAIN (ANALYZE, BUFFERS, WAL)` executions measured:

| Work | Execution times |
| --- | --- |
| Whole-catalogue scan, ordinary history | 1,084.812 / 1,006.240 / 1,131.275 ms |
| 100-key page, ordinary history | 0.342 / 0.353 / 0.374 ms |
| Ordinary occurrence window | 0.881 / 0.479 / 1.055 ms |
| Whole-catalogue scan, history plus long schedules | 2,874.111 / 1,851.613 / 2,495.208 ms |
| 100-key page, long schedules | 3.243 / 3.316 / 3.241 ms |
| 10,000-position window, long schedules | 89.958 / 59.554 / 179.075 ms |

These compare bounded page work with whole-catalogue work, rather than claiming
that one page completes the catalogue. Materializing the bounded schedules and
parsed occurrence rows avoids repeated JSON extraction within the page. Receipt
checks use the existing primary key and attendance counts run only for new
candidates. It needs no schema or index. A 100-receipt/counter batch takes 17.623 ms
of SQL execution and 114.364 ms wall time, writing 79,611 WAL bytes. Cache state,
concurrent builds and per-query timings do not establish production throughput;
full-cycle latency also depends on the three-second pass budget, the 1,024-page
cap, the 100-new-receipt cap, recurrence lengths and the minute cadence.

The actual Rust worker, using an isolated full-schema database with
100,000 completed ordinary events and receipts, finished one cycle in two passes
and 5,550.080 ms of active work without receipt/counter writes, without sleeping
between passes. Adding the normal 60-second interval gives a derived
65.550-second cycle for this sample, rather than a measured production freshness
bound. The page timings above exclude these transaction and driver round trips.

## Pending photo repair scheduling

A disposable PostgreSQL 18.6 fixture with 100,000 pending
photos across 200 owners uses the actual storage adapter with an absent valid
content hash. This sample excludes the one-second row budget measured below.
The two age cohorts read 200,000 keys through 2,002 bounded pages and perform
100,000 storage probes. At 32 pages per pass with one shared three-second budget,
the complete dual sweep takes 63 passes and 30.155 seconds of active work: median
364 ms, p95 983 ms and maximum 1.651 seconds per pass. No pass times out; there
are no storage errors or false publications. Adding the
existing 60-second sleep between passes gives a derived first-sweep duration of
62 minutes 30.155 seconds; the minute sleeps are not part of the measured time.
Cold retries add an hour after completion of their sweep; the ten-minute age
split and retry policy are candidates rather than an accepted freshness target.

The pending index returns 100 keys in 0.136 ms at the start and 0.126 ms midway,
with eight and nine shared buffer hits. It uses no additional index. A separate 1,000
actual publications measure median 1.671 ms and p95 7.968 ms per transaction;
markers, receipts and counter increments each totaled 1,000. These warm local
samples exclude production storage/network latency, sustained incoming writes,
cold caches and deployment interruption. They show bounded work and its backlog
cost; they do not establish production throughput or a 60-second repair bound.

The one-second row-budget regression uses an actual held PostgreSQL row lock. A
healthy later photo publishes in 1,029.286 ms while the blocked row stays pending
and its lock stays held; PostgreSQL activity shows the blocked UPDATE ends before
unlock. The shared pass deadline also keeps its tail with a shorter 500 ms
budget. The repair-only SQL timeout's setup cost and physically stalled storage
are not measured separately.

## Shared social registry profile cache

A disposable PostgreSQL 18.6 database populates a temporary cache relation
with 100,000 synthetic rows and the canonical primary/expiry indexes. Single
`EXPLAIN (ANALYZE, BUFFERS)` samples during a concurrent build measured:

| Statement | Fixture result | Execution time |
| --- | --- | ---: |
| Fresh profile key set | Primary-key index scan | 5.919 ms |
| Bulk conflicting profile write | 1,000 keys | 9.200 ms |
| Expired selection and deletion | capped at 1,000 rows | 2.465 ms |

Expiry selection uses the expiry index and deletion used tuple identifiers. The
reader always excludes expired rows independently of this sweep. These samples
show the selected access paths and bounded cleanup output; they do not establish
production latency, a sustained 1,000-key-per-second reclamation rate or storage
capacity. Production key distributions, concurrent writers, repeated percentiles,
cold-cache costs and the existing source-cache cutover remain unmeasured.

## Referral recent-invitation read

A PostgreSQL 18.6 TEMP fixture holds 100,000 referral rows with the
`(referrer,status)` index. The post-insert query orders one referrer's history
by creation time and returns at most two rows. Single local
`EXPLAIN (ANALYZE, BUFFERS)` samples use a bitmap index/heap scan and top-N heapsort:
90 matching rows take 0.086 ms; a skewed referrer with 10,000 matching rows take
0.870 ms. These are query execution observations from a developer machine during
a build. They do not include HTTP/Slack time, prove sustained throughput or bound
scan work as one referrer's history grows. No additional index is inferred from
these samples.

The community SNS replay executes 18 source broadcaster cases,
two SNS adapter result cases and twelve source event creators. The local target
uses its real membership SQL and the official AWS SDK with a local HTTP stub.
For a rename of 1,101 members, both sides fetch twelve membership pages,
create twelve recipient messages and make two PublishBatch requests (ten and
two entries). A 205-member deletion excludes its owner and emits three
recipient messages. Partial result IDs and full failed chunks are compared;
failed broadcasts do not undo HTTP ban or voice START. This establishes the
sampled audience, batching and request shapes. SNS service latency, retry timing,
concurrent membership mutation across pages and sustained throughput are not measured.

## Social HTTP body boundary

The body replay uses social-service-ea at
`b6b2cefefdf727b2ed47ddb887e77d226a41eea6`, Node 26.12.0 and its installed
`@dcl/http-server` 2.3.0. It compares 18 startup inputs and 44 HTTP cases: 39 with
a 64-byte cap and five multipart cases with a 2 MiB cap. The target uses real
Hyper, the actual community and RPC HTTP routes, the shared unread-body drain
and the outer trailing-slash normalizer. The replay compares status, complete
body and every application header; date and transport framing/hop headers are excluded. Declared oversize and consumed-stream overflow explicitly checked the
close header. A declared oversized request received its response without an
upload body. Another domain accepted 65 bytes behind the same outer layers.

The larger-cap fixture is valid native multipart with 1,050,055 wire bytes,
including a 1,024-byte thumbnail and an unused padding field. Declared, chunked
and header-only variants all reached the independent multipart 400. Thumbnail
limits are checked separately while the total stays below 1 MiB. This replay
does not measure RSS, concurrent upload capacity, throughput, or unread chunked
connection-close timing. The latter remains a separate source/target audit.

## Private voice pending and retained media

An owned PostgreSQL fixture contains 10,512 media calls and 21,024
participants, including 512 historical calls for one wallet. Each media
participant is past its initial-connection TTL; no pending calls remain.
The target INSERT statement is extracted from the Rust write path. Fifty
local starts per wallet measured BEGIN, explicit READ COMMITTED, sorted
participant advisory locks, INSERT and COMMIT; cleanup is excluded.

| Wallet history | Median start | Local p95 | Final INSERT plan execution |
| --- | ---: | ---: | ---: |
| One retained call | 0.419 ms | 0.663 ms | 0.026 ms |
| 512 retained calls | 1.798 ms | 2.367 ms | 1.755 ms |

Both plans use the nonunique caller/callee indexes. The small history uses the
participant primary key; the skewed case uses a hash anti-join against the
participant set. Repeated insert/delete cleanup left dead index entries during
the sample. Rust compilation runs concurrently and cache state is uncontrolled.
These samples establish the access paths and local command cost; they do not
compare source performance or establish production capacity, contention latency,
cold-cache behavior or a service-level bound. Explicit isolation adds one SQL
command to pending creation so waiting writers observe the prior winner even
under a role whose default isolation is REPEATABLE READ.

## Community voice read separation

A PostgreSQL 18.6 TEMP fixture holds 100,000 synthetic voice-presence
rows across 10,000 communities with the `(community_id,address)` primary
key. Single local `EXPLAIN (ANALYZE, BUFFERS)` samples during a concurrent build
measured an all-community status aggregation at 21.030 ms (100,000 rows
scanned), the existing comms bulk query for 50 community IDs at 0.999 ms (500 rows
through bitmap primary-key/heap scans), and the all-active-room query at
17.344 ms (100,000 rows scanned). Ordinary community pages use the bulk
query after core SQL; active filters require the all-room query.

These are individual query observations on a temporary relation with matching
columns and key, not production latency or a sustained-throughput measurement.
They exclude HTTP, profile enrichment and queueing, and do not establish cold
cache behavior, repeated percentiles or capacity under concurrent presence
writes. It needs no index or schema change.

## Community child deletion indexes

An owned PostgreSQL 18.6 scratch database holds 10,000 communities and 100,000
rows in each of `community_voice_notifications` and `community_ranking_events`,
with ten children per selected community. Five samples per operation use
`EXPLAIN (ANALYZE, BUFFERS, WAL, FORMAT JSON)` and rolled back each mutation.

| Operation | Without community-id indexes, median | With indexes, median |
| --- | ---: | ---: |
| Delete ten notification rows | 4.384 ms | 0.128 ms |
| Delete ten ranking receipts | 4.499 ms | 0.127 ms |
| Delete parent with both cascades | 10.142 ms | 1.603 ms |
| Insert 10,000 notification rows | 39.582 ms | 48.747 ms |
| Insert 10,000 ranking receipts | 47.042 ms | 86.380 ms |

Without the indexes, child deletion uses sequential scans discarding 99,990
rows; with them, bitmap scans selecting ten rows. Each index occupies 2,072,576 bytes.
The insertion medians increase by 23.2% and 83.6%, respectively. These warm,
synthetic samples establish the tradeoff for this fixture; they do not establish
production latency, sustained throughput or a service-level bound.

## Community mutation coordination

A PostgreSQL 18.6 synthetic fixture used eight workers, 2,000 operations per sample
and three warm samples. Each operation runs two independently committed SQL
statements. With no guard on one community, median/p95 are 0.182/0.331ms;
with its session advisory guard, 1.029/1.189ms; guards over eight communities
measured 0.269/0.427ms. These isolate SQL coordination cost and exclude HTTP
signatures, Rust/SQLx prepare costs and external awaits. They establish no
production capacity or service-level bound.


## Referral lifecycle step on shipped main

A disposable PostgreSQL 18.6 database holds a shipped-main-shaped
`social.referral_progress` with 1,000,000 rows (205 MB with its primary key),
a quarter of them `tier_granted`. Step `0021_social_referral_lifecycle` runs in one
transaction under `psql` timing on a developer machine during a concurrent build:

| Statement | Execution time |
| --- | ---: |
| Add `id` with `gen_random_uuid()` default (table rewrite) | 3,214.949 ms |
| Add the five other columns (constant defaults or nullable) | 0.867 ms total |
| Mark 250,000 granted rows | 5,436.449 ms |
| Build the unique `id` index | 494.525 ms |
| Step `0023` nullable `reward_tier` | 0.181 ms |

The table holds 301 MB afterwards. The rewrite and index build hold ACCESS
EXCLUSIVE on the table; the cutover already stops every writer. One warm sample
scales with row count and storage; it does not establish production duration.

## Referral canonical IP quota comparison

PostgreSQL 18.6 TEMP data holds 100,000 four-column referral
probe rows with the existing `(referrer,status)` index shape. Seven warm
`EXPLAIN (ANALYZE, BUFFERS)` samples after warmup compare text equality with
guarded inet equality including mapped IPv4 aliases. Median execution times:

| Matching referrer prefix | Text equality | Guarded inet equality | Local buffer hits, both |
| --- | --- | --- | --- |
| 10 rows | 0.004 ms | 0.006 ms | 3 |
| 1,000 rows | 0.053 ms | 0.183 ms | 16 |
| 10,000 rows | 0.520 ms | 1.819 ms | 135 |

Both comparisons use the same index access path and buffer counts; canonical
historical comparison adds conversion work within the selected prefix. This synthetic
probe does not measure full attribution writes, transaction contention,
providers, cold data, production distributions or a service-level bound.
No index or historical-row rewrite is inferred from these observations.

HTTP method labels cover nine standard methods in each source transport casing
(uppercase for ordinary HTTP, lowercase for uWS), plus `other`; WebSocket close
labels use a finite standard/source allowlist plus `other`. Per-protocol open
counts update under the existing socket-map mutex without scanning that map.
These are bounded label/state invariants, not measured throughput improvements.
The community coordination benchmark above excludes the bounded SQLx
acquisition setup and does not measure it.

## Snapshot inactive-entities count

The snapshot generator's port of upstream's inactive-entities check counts, at
each pass (every `SNAPSHOT_GENERATION_INTERVAL_HOURS`, default 6), the active
deployments of every window whose row was generated over four weeks earlier
(`content/src/snapshot_gen.rs` `has_inactive_entities`). On a settled node that
is nearly every window, the year windows included, so a pass counts close to
every active row of `content.deployments`.

Dataset: a synthetic `content.deployments` of 4,042,824 rows (2,730 MB heap)
on PostgreSQL 18.6, Apple M3 Pro, 18 GiB, `shared_buffers` 64 MB, during a
concurrent build. Active rows follow the 14 windows and `numberOfEntities` a
public upstream node listed on 2026-11-05 (1,470,117 active; 707,202 in one
month window, 396,161 in the newest year window), with 1.75 overwritten rows
per active row in each window, uniform `entity_timestamp` inside the window
and random physical order. Rows carry a three-link auth chain and no
metadata (about 675 heap bytes per row), so production rows are wider. 809 `local_entities`
rows, 270 tombstoned. The indexes on the measured columns were those of
`schemas/00_content.sql` (`deployments_entity_timestamp_entity_id_idx`,
`deployments_full_snapshots_ix`). Query: the count with the tombstone
anti-join. The table is `VACUUM ANALYZE`d. Execution times from
`EXPLAIN (ANALYZE, BUFFERS)`: the first run, then five more as
min / median / max.

| Window (active rows) | Existing indexes: plan, buffers | first; min / median / max | With `deployments_active_entity_timestamp_idx`: buffers | first; min / median / max |
| --- | --- | --- | --- | --- |
| year 2025-07 .. 2026-06 (396,161) | parallel seq scan, 349,370 | 7,399; 4,162 / 4,567 / 5,335 ms | index-only, 4,684 | 116; 48 / 51 / 62 ms |
| month 2026-08 .. 09 (707,202) | parallel seq scan, 349,370 | 4,279; 3,280 / 4,078 / 6,386 ms | index-only, 8,259 | 369; 103 / 133 / 148 ms |
| year 2020-01 .. 12 (16,162) | bitmap AND of both indexes, 45,277 | 4,019; 3,709 / 3,816 / 7,371 ms | index-only, 215 | 7.6; 3.2 / 4.0 / 5.4 ms |
| month 2026-06 .. 07 (2,941) | bitmap heap scan, 8,097 | 608; 13 / 20 / 26 ms | index-only, 64 | 2.1; 0.6 / 0.9 / 1.5 ms |

The existing indexes do not serve the count: `deleter_deployment` lives only in
the heap or in `deployments_full_snapshots_ix`, which has no `entity_timestamp`,
so each large window reads the whole heap (2.7 GB here) and a pass several
times that. The partial index `deployments_active_entity_timestamp_idx`
(`entity_timestamp` INCLUDE `entity_id` WHERE `deleter_deployment IS NULL`)
makes each count an index-only scan with no heap fetches after a vacuum. It
holds 133 MB for 1,470,117 active rows and built in 7.8 s.

Write cost, `EXPLAIN (ANALYZE, WAL)` in a rolled-back transaction on the same
table: inserting 10,000 active rows wrote 24,408,070 WAL bytes in 50,261
records with the index and 23,035,141 bytes in 42,820 records without it
(+6 %, about 137 bytes per row). Setting `deleter_deployment` on those rows
wrote 23,941,555 bytes with it and 23,893,327 without: the new row version
leaves the partial index. `catalyst schema migrate` builds the index in its
transaction (not concurrently), holding writes on `content.deployments` for
the build; DATABASE.md gives the concurrent build to run beforehand.

These are single warm-cache samples on synthetic data during other load. They
do not measure production row width, cold reads, the build time or lock wait
on the production table, or a whole pass's total.
