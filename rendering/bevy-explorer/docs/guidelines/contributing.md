# Contributing

Smaller PRs are better. Everything below is in service of that and of a reviewer who can
understand the change in one sitting.

## Scope

- One problem per PR, fixed at the sentence that names it. If the fix wants new state or
  plumbing the problem didn't mention, that is a second PR and a conversation first. Give
  independently useful diagnostics their own PR and verification.
- Anything that changes an interface between areas, adds a scene-facing capability, or will
  take more than a couple of days gets a short written plan before a branch exists. The
  approach, not prose.
- For multi-step features, get the whole pipeline working as a proof of concept first, then
  split it into reviewable PRs. Intermediate states that build but cannot be exercised are not
  reviewable.
- We squash-merge, so stacked PRs conflict on the child after the base lands. Prefer
  sequential small PRs over stacks.

## The diff

- Nothing in the diff the change doesn't require. No reordering, restyling, renaming locals or
  tidying neighbours. Reviewer load beats marginal tidiness.
- Comments say what the code cannot: a current constraint, a non-obvious invariant, a platform
  quirk. Not what the code does, and no archaeological commentary: not what the old design was,
  not why the new one doesn't need X, not an argument addressed to the reviewer. Strip those
  before pushing. Rationale goes in the PR description.
- When simplifying a feature, remove leftover edits that no longer change behaviour. Keep
  comments that explain constraints still in force and rename tests that now prove a different
  contract.
- Comments explain current invariants and constraints. Keep implementation provenance and
  decision history out of them; apply the [public-prose rule](#public-prose).
- Existing comments survive a refactor unless they became wrong.
- Limit each `Drop` implementation to one blocking operation. Release a lock before taking
  another or doing file I/O. Put multi-step cleanup in an explicit completion path.
- Reuse what exists: protocol and SDK types, the existing scheduling, picking and loading
  paths, helpers such as `scene_runner::parcel_to_vec3` and `vec3_to_parcel`. Hand-rolled
  parallels are the main source of drift.
- You should not need to add a schedule set or ordering step. If you think you do, raise it
  first; the [scheduling guide](scheduling.md) explains the current setup.
- If something works better with a change in a dependency, change the dependency. Upstream it
  where possible; where it is bespoke to us, it is a patch in this repository: see
  [the bevy fork](bevy-fork.md#changing-bevy).

## Tests

- Test behaviour at a boundary, not the shape of the implementation. A test that has to change
  when the code is refactored without a behaviour change is a cost, not an asset.
- For route and service contracts, exercise the actual producers and consumers, such as the
  engine resolver, the HUD resolver and generated proxy configuration. Assert which endpoints
  they share and which intentionally use separate destinations.
- Pair a network access denial with a permitted case through the same transport. Check what
  the server actually received so a client-side error cannot hide an unwanted request.
- For each required regression, identify the CI job and command that execute it. Verify that
  selectors include the test, including any explicit opt-in, and that the job has the tools
  and files the test needs. Check path filters against changes to the test's inputs, including
  routing sources outside the engine workspace.
- Parse structured output; never string-compare it. Serialisation order is not stable across
  feature unification.
- For anything animated or staged, test what lands mid-way, not only the happy path. Enumerate
  the events that can arrive while the transition runs: the thing it replaces is rebuilt,
  despawned, hidden, or toggled by its setting, or it loads late or fails. Decide the behaviour
  for each and give each a test.

## The PR description is the record

- Squash-merge discards commit bodies. The description carries the why, the alternatives
  rejected, and how it was verified, naming the platforms covered: native, web, headless.
  For network capabilities, describe the caller, transport, platform and mode that grant
  access. For instrumentation, identify the request paths that emit events and which
  callers bypass them, then verify the documented event lifecycle.
- A diagram for anything with dataflow, ordering or lifetime changes. A draw.io SVG or a
  mermaid block both render on GitHub. A reviewer reads a diagram in a minute and a 400-line
  diff in an hour.
- Screenshots before and after for rendering changes. There is no visual regression suite for
  the 3D side.
- Measurements in the description or an ADR describe the merged code: re-run them at the head
  after the last change to the measured path, merged with main.
- Explain the chosen behavior, constants, alternatives and verification in this engine's own
  terms. Follow [public prose](#public-prose); for ports, use [Porting from Unity](porting-from-unity.md).
- Follow the repository PR template, including its test instructions and checks run; omit a chronological work narrative.

## Public prose

Document current behavior, ownership, constraints and reasons directly. Except for the
porting workflow below, omit Unity implementation provenance from code comments, tracked documentation
(including ADRs and agent instructions), and PR titles and descriptions: source-file or
revision inventories, copied-constant attribution, and comparisons offered as the reason for
this engine's design. Keep that research in ignored task/review reports. This supersedes older
instructions to put implementation provenance in PRs outside that workflow.

The [Porting from Unity guide](porting-from-unity.md) is an explicit exception: preserve
its source inventory, mappings and integration instructions. Its workflow may name Unity
source files, carried constants and intentional divergences in porting PR descriptions.
Code comments still explain this engine's constraints, as that guide requires.

Unity remains an existing Decentraland client. Name it accurately in client lists, protocol
consumer requirements, compatibility and rollout limitations, tooling instructions, and
benchmark operations. Preserve the evidence and source pins supporting those facts. Do not
remove a client, obscure its identity as a generic "external client", or imply that its
implementation is complete merely to remove its name.
Compatibility comparisons in [decisions](decisions.md) and [oddities](oddities.md) are
permitted; classify each claim in context rather than treating a page's client names as violations.

Preserve exact tool and skill identifiers, endpoint paths, process names, wire values, and
repository or download URLs needed for the documented task. Keep required legal attribution
in licenses and dependency manifests.

Removing provenance is a documentation edit, not a behavior or process change. Preserve
technical values, rejected alternatives and their reasons, verification requirements, merge
gates and useful source pins outside this prohibition. Do not turn a report about another
implementation into a requirement or claimed behavior of this engine; retain such evidence
in the ignored review report when it cannot stand alone here.

For generated documentation, update its source or generation transform and regenerate derived
views, including search indexes. Run the generator's consistency check; editing only the rendered
document leaves the policy unenforced on the next generation.

Before committing or updating a PR, search every changed file and the complete PR text with
`rg -ni '\bunity(?:\b|[-_])'`. This is a minimum check: also inspect the diff for source-file
and type names that do not contain that word. Apply the porting-workflow exception above;
otherwise remove implementation provenance. Preserve client facts and operational references,
and check the diff for lost compatibility information. Record the classification in the ignored review report. Explain
this engine's design using its requirements and tests.

## Before pushing

- Merge main first. CI tests the merge, not the branch.
- `python3 ../../../tooling/patched-sources.py materialise` after the merge and before the cargo
  commands: Cargo reads the [rebuilt trees](bevy-fork.md#changing-bevy).
- `cargo fmt --all`, `cargo clippy --locked -- -D warnings`, `cargo test --all`. Clippy warnings
  are errors in CI.
- The wasm build, only if the change has platform-sensitive code: `#[cfg]` branches,
  threading, atomics, blocking calls, timers, filesystem or network access, wasm-specific deps.
- A change that limits per-frame work (a budget, slicing) is tried in the web build by hand,
  where frames are slowest and a fixed cost takes the largest share of one: that run is part of
  the case for the change, and CI only shows that it compiles. Say in the description what you
  saw and the worst frames. Other changes to per-frame work need no manual web run.
- Web-only engine switches are a page param plus a wasm export, not a launch option.

## Dependencies

- **Never run `cargo update`. Edit `Cargo.lock` by hand.** Cargo's re-resolve is not
  deterministic for this workspace: any `cargo update -p <crate>`, and any dependabot rebase,
  also flips unrelated lines (`syn` 2 to 1 under `data-encoding-macro-internal`, `heck` 0.5 to
  0.4 under `prost-build`, and others). Those flips are rejected in review even when CI is
  green. To bump a dependency, change its `version`, `source` or git hash, `checksum` and
  dependency list in the lock directly, then validate with `cargo metadata --locked` (exit 0
  means the lock was accepted unchanged). For a dependabot PR, take main's lock and hand-apply
  only the intended crate's change.
- Hoist every third-party dependency to `[workspace.dependencies]` and use
  `{ workspace = true }` at the call site, even for a crate used in one place. Convert existing
  inline uses of the same crate in the same change.
- A change to bevy or to a forked crate is one more patch file under
  `rendering/bevy-explorer/patches/<name>/series`: see [the bevy fork](bevy-fork.md#changing-bevy).

## Review

- First response within one working day. If it hasn't come, ping the reviewer on Slack rather
  than wait.

### Reviewing content request changes

From the repository root, run `python tooling/bevy-http-checks.py` with the native build
prerequisites. It runs the IPFS and platform libraries' tests in parallel and compiles
`scene_inspector`, `imposters` and `scene_runner`. Those consumers exercise the editor
import, imposter and font-loading interfaces. A missing or unexecuted required regression
fails the check. For a targeted run, `--filter TEXT` selects matching tests and
`--tests-only` skips consumer compilation. [Build prerequisites](running.md)

| Check | Regression |
|---|---|
| One network attempt consumes one ticket and holds the permit through the body read | `a_content_fetch_uses_exactly_one_ticket_on_its_own_gate`, `engine_request_keeps_its_slot_until_the_body_finishes` |
| Other downloads can run during backoff and the retry queues again under the current priorities | `retry_uses_live_priorities_after_another_download_completes_in_backoff`, `immediate_timeout_retry_yields_to_an_older_unowned_request` |
| Equal-priority arrivals preserve their order even when the executor polls them in reverse | `shared_downloads_keep_arrival_order_when_workers_poll_in_reverse` |
| Dropping a reader preserves other claims and dropping the last one cancels queue waits and retry timers | `dropping_shared_readers_withdraws_only_the_last_claim_of_each_owner`, `last_shared_reader_cancels_a_queued_attempt_and_a_retry_timer` |
| Concurrent writers publish complete cache files, preserve occupied staging files, and remove their own staging file after a failed publish | `concurrent_writes_to_one_cache_key_publish_complete_files`, `cache_writer_skips_a_stale_staging_file`, `failed_cache_publish_removes_its_staging_file` |
| Readiness polls obey server delays and their deadline | `readiness_poll_respects_retry_after_and_remaining_deadline` |
| Keys are published while a slot is in use, since its release can choose between newcomers | `keys_are_read_while_a_slot_is_in_use` |
| A departed hash ranks last while any request of it is alive, including a retry after its queue drained | `a_departed_hash_ranks_last_for_a_retry_after_its_queue_drained` |
| The gate keeps granting after a poisoned lock, and a release skips a queue entry without its ticket instead of panicking in a destructor | `a_poisoned_gate_keeps_granting_and_releasing`, `a_queue_entry_without_its_ticket_is_skipped_inside_a_release` |

Give each test its own gate and counters so it can run alongside other tests. Trace destructor
paths for nested locks or file I/O under a lock. When changing diagnostics, check the units
and width of each counter against partial transfers and overlapping requests.

Before adding a shared budget, identify the workload that needs it and measure the impact
on unrelated callers. Give changes with independent consumers or failure modes their own
PR and test evidence.

For browser forwarding changes, run
`npx vitest run src/test/serviceWorkerCache.test.ts src/test/previewRealm.test.ts` from
`rendering/bevy-explorer/source/react-web`. Check abort propagation, streamed requests,
referrer policy and disposal of unread response bodies.
