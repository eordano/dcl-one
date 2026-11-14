# Pinned source captures

`tooling/social-captures.json` maps each of the 48 source capture entry points
to its exact fixture, argument order, required source checkouts, database
environment and optional commands. It includes the captures beside their JSON
in `source-contracts/` and those in `tooling/`; names need not match.
`MANIFEST.md` describes the separate dated server captures.

## Sources and runtime

| Source | Required commit |
| --- | --- |
| `decentraland/social-service-ea` | `b6b2cefefdf727b2ed47ddb887e77d226a41eea6` |
| `decentraland/comms-gatekeeper` | `be6a4b97a2189157884503485892ff46d0ac68da` |

Capture entry points require Node 26. The committed runtime receipts and CI
use Node 26.12.0; use that version for byte comparisons, including V8 error
messages. Install each source project's locked dependencies before capturing.
The shared `tooling/social-capture-common.cjs` verifies each required checkout
is the Git working-tree root, at its exact pin, with no staged, unstaged or
nonignored untracked changes. Both sources are checked before any upstream
module loads when a capture needs Comms. Ignored dependency installations do
not count as source changes.

These checks run before TypeScript registration, upstream imports, database
connections, listeners or output writes. The helper centralizes pins, hashing
and provenance without importing a capture script. The lazy voice fixture
factory is separate from that provenance helper.

## Regeneration

Read the catalog entry before running a script. Its ordered `args` tokens mean:

| Token | Value |
| --- | --- |
| `social` | Clean pinned Social checkout root |
| `gatekeeper` | Clean pinned Comms checkout root |
| `fixture` | Output JSON file, preferably in a temporary directory |
| `fixture-directory` | Output directory; the script chooses the catalogued basename |
| `postgres-url` | Explicit disposable PostgreSQL URL argument |
| `redis-server?` | Optional Redis executable override |

Entries with `CATALYST_TEST_DATABASE_URL` in `environment` read that explicit
variable instead of a positional database argument. Missing or blank database
values are refused; ambient `PG*` variables cannot silently select a database.
Use a disposable PostgreSQL instance for SQL captures. They create and remove
temporary schemas or databases; `member-request-pages` also creates and removes
a temporary role, requiring the corresponding cluster permission. PostgreSQL
18.6 matches the committed server-version receipts.

For example, from this repository root, with a clean pinned source checkout:

```sh
capture_dir="$(mktemp -d)"
node tooling/social-service-profile-key-cases.cjs \
  "$CAPTURE_SOCIAL_ROOT" "$capture_dir/profile-keys.json"
```

Compare the result with the catalogued fixture before replacing anything.
Review source pin, runtime, provider controls and fixture differences together;
a changed fixture also requires an explicitly reviewed catalog digest update.
Request-failure paths stop local listeners in `finally`, so failed captures
exit with an error. Manual HTTP requests reject interrupted responses and
enforce a five-second deadline across headers and body. No live provider
acceptance follows from a controlled source capture.

The analytics capture under `library/dcl-http/tests/fixtures/` records installed
analytics/fetch SDK transport behavior, including redirect and header semantics.
It stays with the shared HTTP conformance consumer rather than making a library
test read upward from a service-owned fixture. The catalog makes that placement
explicit.

## Checks

```sh
node --test tooling/tests/social-*.test.cjs
node tooling/social-capture-check.cjs
python3 tooling/owners.py --check
```

The `Social source captures` workflow runs these checks with Node 26.12.0 on
pull requests and main pushes that change a file the checks read; a test
fails when the workflow's path lists miss one. It validates the complete
script-to-fixture mapping, repository-contained paths, source/runtime
metadata and frozen JSON digests,
and syntax-checks capture, helper and test scripts. Negative tests exercise
checkout/runtime/database preflight and request-failure cleanup. This consumes
the catalog independently of service runtime activation.

CI does not regenerate the source fixtures or establish service equivalence.
Runtime conformance, deployed clients, provider routing and stateful cutover
require their own evidence. A recorded source difference is not approval.

## Compatibility boundary

The native stack preserves client HTTP/RPC, authorization and user-visible
notification/referral behavior. Internal SNS envelopes, SQS acknowledgement
policy and AWS SDK scheduling are source evidence, not required architecture.
The [native social events decision](../../../../../../docs/adr/2026-11-04-native-social-events.md)
uses local transactions, the email outbox and existing workers while retaining
NATS for cross-process updates. Keep frozen source captures intact; cite native
effect tests when an internal transport assertion no longer applies.
