# Registry API over local Catalyst data and native conversion state

- Status: Accepted
- Date: 2026-11-09

The Registry surface is composed under `/asset-bundle-registry` in the existing
Catalyst HTTP server. Profiles use Catalyst's shared entity lookup and profile
processing directly. The remaining Registry reads go through a gateway with
uncached source responses and a five-second cache of local denylist rows. No
Registry profile or entity response cache is added.

`POST /profiles` and `POST /profiles/metadata` share the marketplace state, content
denylist, ownership readers and profile transformations used by Lambdas. They
accept an empty `ids` array and, as the Registry, any number of ids; a request
body over 256 KiB is a 413 in the Registry's JSON error envelope. The Registry
reads precomputed rows, while each id here costs an entity read (on a read-through
node, an upstream `POST /entities/active` and a negative-cache entry) and an
ownership pass. So only ids that can name a profile, an address (`0x` and 40 hex
digits) or a `default` prefix followed by one to three ASCII digits, are looked up.
The body bounds the address population (fewer than 5,826 address strings fit),
while the accepted default namespace has 1,110 distinct spellings, including
leading-zero suffixes. Entity reads and read-through fills process at most
1,000 pointers at a time; every eligible
pointer is processed, with no total 1,000-pointer truncation.
They resolve case-insensitive pointers once and project the
processed results into Registry response shapes, in request order.
Non-profile entities and profiles without a usable avatar are omitted; an entity
missing its id, pointer or timestamp is logged and omitted. As the Registry, a
failed lookup layer is logged and answers 200 with what was found: a failed entity
read leaves out its 1000 pointers, and the ownership pass runs per 128 entities, so a
failed pass leaves out its chunk only. Unvalidated metadata is never served. Since
these answers are 200s, each failure also counts in
`profiles_lookup_failures_total{route,layer}` for alerting. Full profiles carry the entity
timestamp as a JSON number and both answers keep the Registry's key order, as the
2026-11-05 captures `AB-1-one-known.json` and `AB-2-one-known.json` show. Snapshots and compact thumbnails point to the configured local
profile-images service; each full-profile avatar carries exactly the `face256` and
`body` snapshots, as the Registry's sanitizer writes them. The Registry projection
does not use Lambdas' profile response cache, so a subsequent read sees changes in
the local entity view.

The Explore Registry gateway serves these reads from the backend named by
`EXPLORER_REGISTRY_SOURCE_URL`:

| Method | Paths |
| --- | --- |
| POST | `/entities/active`, `/entities/versions` |
| GET | `/entities/status`, `/entities/status/{id}`, `/worlds/{world_name}/manifest` |
| GET | `/denylist`, `/queues/status` |

The variable is the only selector; `ABGEN_ADMIN_URL` does not enable the gateway,
because an abgen build without native Registry state answers these routes
differently. The source is an `http(s)` base URL without credentials, query or
fragment, and startup refuses any other value, naming the variable.

- A loopback host (`127.0.0.0/8`, `::1`, `localhost`, the `ABGEN_ADMIN_URL` rule)
  is the local abgen runtime. It must implement the native Registry routes
  (decentraland/abgen#145 or a compatible release). The gateway forwards the
  signed-fetch `x-identity-*` headers and overwrites `x-original-path` with the
  path Catalyst received, without the query. The native verifier binds that path
  to the forwarded route and accepts only the canonical service path and its single
  Registry-prefix alias.
- Any other host is a public Registry, for example the mirror posture default
  `https://asset-bundle-registry.decentraland.org`. It passes the same SSRF and
  own-host checks as the other explorer sources, at startup and on each request,
  and receives no identity or path headers. Signed `GET /entities/status` calls
  therefore fail in this mode with the Registry's 400 `Invalid Auth Chain`;
  signed reads need the local runtime.
- Empty, the island posture, answers `/entities/active` and `/entities/versions`
  with `200 []`, `/denylist` with the node's own rows, and the other reads with 503.

## Local denylist

`GET /denylist` merges the source's list with the node's `content.denylist` rows,
newest first by `created_at`. Only an exact id shared by both lists keeps the
source's entry: CIDv0 ids are case-sensitive, so `QmCase` and `qmcase` both remain.
An operator row reads `{entity_id, reason: null, created_by, created_at, updated_at}`
with `updated_at` equal to `created_at`, in milliseconds as strings.

This is an anonymous public moderation feed. It intentionally includes every kind
of local denylist id (entity ids, content hashes and pointers), and `created_by`
is the operator wallet stored in `content.denylist.added_by`. The administrative
console's access control does not make those published fields private.

The existing shared Explore API state caches only its local rows for five seconds,
sharing one refresh among concurrent requests through the existing content pool.
The expected list is hundreds of rows; the existing hard bound is 100,000 rows,
with an overflow detected by reading at most 100,001. Database failures are cached
for the same five seconds and return 500; expired successful data is not served
on a failed refresh. Operator changes become visible after expiry on the next
read, and restart rebuilds the cache. No table, index or pool is added. The cache
bounds table reads; merging and JSON serialization still run per request.

The source is fetched and validated on every request, independently of that
cache. A non-200 source answer preserves its status and body; malformed JSON or
a 200 body that is not an array returns 502, and a refused source configuration
returns 503. The gateway never serves just the local half after a source failure:
a content server polling the list keeps its last good copy on an error, and would
lift the source's ids on a shorter 200. HEAD fetches GET from the source, performs
the same validation and merge, and Axum removes the response body. Cached rows
contain no source response, credentials, request headers or CORS headers, and are
shared by routers using that state, but not by independently configured states.

## Gateway forwarding

Whatever the source, `/entities/active` and `/entities/versions` first apply the
Registry's request rule: a body without a non-empty `pointers` (`ids` is not read)
answers `400 {"ok":false,"message":"No pointers provided"}` and a body that is not
JSON `500 {"error":"Internal Server Error"}`, with no cap on the pointers. A
successful `/entities/active` answer then drops every entity whose id is on the
node's content denylist projection, the one the content reads apply.

Except for HEAD on `/denylist`, which fetches GET, the gateway forwards the
method, raw query, body, content type and `accept`, and
never `authorization`; conditional headers are forwarded except on
`/entities/active` and `/denylist`, whose answers the gateway may rewrite. Registry writes (`/denylist/{id}`
POST/DELETE, `/queues/retry|pause|resume`, `/registry`, `/flush-cache`) are not
routed: the abgen admin bearer stays on the host and those operations remain with
the ops console, behind its sign-in, write switch and audit log.

Source responses are uncached, so every configured-source read, including each
explorer boot read, is a backend call. At most 64 backend requests run at once, bounding buffered
responses at 512 MiB; a read waits up to 2 seconds for a free slot and is then
refused with 503 and `Retry-After: 1`. A request times out after 30 seconds, does
not follow redirects and buffers at most 8 MiB; a failed request or a larger
response returns 502. A
path the URL parser would normalize, such as an encoded `..`, returns 400.
Responses preserve status, body, content type, cache directives, validators,
`Allow` and `Retry-After`. Local `/status`, `/health` and the status-check probes
(`/health/live`, `/health/ready`, `/health/startup`) describe the Catalyst
gateway; they do not establish native worker readiness. Every path under the
prefix, routed by either half or not, answers the Registry's CORS: a 204
preflight with origin `*`, its method list, the requested headers and a one-day
max-age. Behind `infra/gateway`, a response without
`Access-Control-Allow-Origin`, such as the answer to a request without
`Origin`, gets `Access-Control-Allow-Origin: *` from the gateway. The existing
gateway routes Registry traffic to Catalyst, so no additional Nginx upstream or new
service is introduced here, and this change does not select an abgen release.

The alternative of retaining an external Registry cache would duplicate the
entity/profile view and obscure native worker failures. Reimplementing conversion
state inside Catalyst would split the converter's state ownership. The chosen
boundary reuses Catalyst's existing implementations and keeps conversion state
with its existing service.

Compatibility expectations are scoped to Registry source
`63c9ef2bd7365ac7312d8bd23b6759515ca999e5` and explicit checked-in captures.
The profile input error tests use the 2026-11-05 captures at Registry
`f9578d4ab1ab9fdff4576ba6d052b966bd0fd5e9`: malformed JSON returns 500, while a
valid JSON value with invalid `ids` returns 400. The CORS preflight follows the
2026-11-05 capture `AB-12-preflight.json`. Full transport equivalence, including
all failure envelopes, is not implied by sharing the profile implementation.
