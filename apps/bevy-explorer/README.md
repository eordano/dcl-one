# Bevy Explorer app

Builds and publishes the Explorer in `../../rendering/bevy-explorer/source` (its own Cargo
workspace and `target/`); artifacts go to this app's ignored `dist/`.

## Build locally

Prerequisites: the [Explorer README](../../rendering/bevy-explorer/source/readme.md)'s platform
libraries, Rust, protoc, Node 24, npm. Web also needs `nightly-2026-04-15` with `rust-src` and
`wasm32-unknown-unknown`, and wasm-pack (CI takes 0.15.1 from the flake's `bevy-tools` package);
desktop needs FFmpeg and the pinned CEF.

CI uses `build-web.py --shared-build`: Cargo compiles the engine and scene runtime together,
then the lockfile-matched wasm-bindgen emits two separate modules. `install-web-tools.py`
checks the release archives' SHA-256 and the binding generator's version against Cargo.lock.
Optimized builds retain the release compilation settings and run wasm-opt. Unoptimized PR
builds use 16 codegen units; the larger modules trade download size for compilation speed.
Linux web CI takes protoc, clang 21, pkg-config and ALSA/udev development outputs from
the flake's cached `bevy-web-tools` package and installs no apt build dependencies. Its
pkg-config search path points to that package. Native Linux packaging retains distribution
libraries to preserve Ubuntu 22.04 tarball compatibility.

Package installation overlaps compilation, and each command reports its elapsed time. Native
build-tool thread pools default to at most 16 threads; `RAYON_NUM_THREADS` overrides that cap.
Portable bridge exports resolve their relative content URL against the served realm, including
when the app is mounted under a gateway path. CDN publication reuses the compiled scene.

The bridge denies dependency lifecycle scripts explicitly. The SDK hook launches another
package manager, esbuild uses its installed platform binary, and protobufjs's hook only
checks version conventions. The SDK build and static export validate these installations.
The production bridge bundle runs the pinned Oxlint correctness checks with warnings treated
as errors. The SDK's runtime packages stay pinned; its CLI uses the patched esbuild and the
maintained hashing 1.x implementation. Hash fixtures preserve the pinned SDK's byte and stream
content identifiers, while existing hashing 3.x consumers remain unchanged. The bridge's
`.dclrc` disables CLI analytics. Failure-path HUD tests assert their exact console diagnostics;
unexpected messages still fail those tests.

```sh
make web          # engine WASM, scene WASM, sandbox guard, HUD tests, bridge scene -> dist/web
make web-dev      # same, skipping wasm-opt
make headless     # headless engine + sidecar -> dist/headless/native
python3 build-headless.py --target x86_64-unknown-linux-gnu
```

Windows has no `make`, and `python3` there is a Microsoft Store stub: run `python build-web.py`
(`--no-opt` for `web-dev`) from PowerShell or Git Bash, or `make web PYTHON=python` where make is
installed. It needs Git for Windows: the bindings script runs in Git's bash, never WSL's.

Native dev loop: in the source directory, `just setup-cef`, then `just native-release` or
`just native-debug`. For packaging, follow the checked-in
[desktop workflow](../../.github/workflows/bevy-explorer-desktop.yml) and
[native build guide](../../rendering/bevy-explorer/docs/guidelines/running.md#native).
Importing into Sites (production requires the main CI artifact):
[Explorer integration](../../rendering/bevy-explorer/README.md#native-builds-and-hud-updates).

Windows CI caches the pinned sccache executable separately from compiler results. Cache setup
is optional; only a successful setup enables the compiler wrapper. The guarded stats step
records cache results, while the action's unconditional post-job annotations are disabled.
A download outage therefore falls back to direct compilation without skipping tests.

CI caches WASM outputs in `.cache/bevy-engine`; a restore that fails its identity, inventory or
checksum check fails the build. Native CI uses Rust 1.99.1 and `protoc` 36.2: Linux takes it from the
flake's `bevy-tools` package (`install-tools.sh` checks the major), Windows and macOS install the
release archive `install-protoc.py` pins. See the current measurement evidence in the task report
before claiming time savings.

Analytics: every environment reports to one Segment source, keyed by `BEVY_SEGMENT_WRITE_KEY` (a
repository variable; unset, the engine keeps the key it shared with the mobile client). Every event
carries `environment`, `prod` or `dev`, and the data stack filters on it. Only a run against
`decentraland.org` is `prod`: on the desktop a release build whose resolved `--base-domain` is
`decentraland.org` (a debug build or the zone staging domain is `dev`), on the web (always a release
build, one npm version on every site) a page whose host is exactly `decentraland.org` (any
subdomain, including the `cdn.decentraland.org` PR previews, `decentraland.zone` and localhost are
`dev`). The desktop workflow sets `BEVY_ANALYTICS_DEV=1` for a build of any ref but `main` or a tag,
so a QA build pointed at `decentraland.org` is `dev`. Setting the variable builds nothing by itself:
the key ships with the next web build on `main` and the next desktop `workflow_dispatch`. It is part
of the compiled engine's cache key, so that build recompiles the WASM.

## Actions and outputs

| Root workflow | Work | Outputs |
| --- | --- | --- |
| `bevy-explorer-ci.yml` | Selected engine checks and consumer build dependencies | Web app, tested Linux headless package, platform packages and APIs/Sites images, the `benchmark` runner for each *Test Suite* platform (Windows, macOS, Linux) |
| `bevy-explorer-benchmark.yml` | A pull request's web app against main's in Chrome on the laptop fleet: after each passed CI run of a PR that built the web app, or by hand ([Benchmark](../../rendering/bevy-explorer/docs/guidelines/benchmark.md)) | The *Performance* section of the PR's [CI status comment](#bevy-explorer-ci-status); the report in the job summary |
| `bevy-explorer-benchmark-leg.yml` | One laptop's leg, called by `bevy-explorer-benchmark.yml` | `benchmark-result-<label>` (run CSVs and logs) |
| `bevy-explorer-ci-status.yml` | After each `bevy-explorer-ci.yml` run of a PR that selected an explorer job | The *Build* and *Tests* sections of the PR's [CI status comment](#bevy-explorer-ci-status) |
| `bevy-explorer-scenes.yml` | Graphical scene tests (main pushes, nightly 03:17 UTC, manual) | Verdict, logs, screenshots |
| `bevy-explorer-desktop.yml` | Manual Linux/Windows AMD64 packaging | Desktop archives |
| `bevy-explorer-headless-build.yml` | Reusable platform compilation; Windows and macOS run beside Linux smoke | Engine + sidecar archives; platform npm tarballs |
| `bevy-explorer-headless.yml` | Publish the four validated platform packages after CI; explicit manual builds include Linux | Headless npm packages; manual prerelease |
| `apis-sites-images.yml` | Reusable image builds after the web producer; publish after complete CI | Tested AMD64/ARM64 APIs and Sites images |
| `bevy-explorer-images.yml` | Images from a successful main CI run | Web (Linux AMD64/ARM64), headless (Linux AMD64) |
| `bevy-explorer-rollout-manual.yml` | Roll out a CDN version at `decentraland.zone/play` or `decentraland.org/play` by hand | Rollout records |
| `bevy-explorer-upstream.yml` | Daily fork check and drift report | `upstream/forks` PR; drift artifact |

`tooling/bevy-ci.py` selects engine and image jobs per changed input. Unknown engine inputs and
manual Explorer dispatch select all engine checks. Image-only edits reuse a matching completed
web producer and do not select native compilation. On main, non-Linux headless packages build
alongside Headless Smoke; its stripped, tested Linux binaries also supply the Linux npm package.

Image builds use `needs` to start after the selected web producer completes. GitHub holds the
job without allocating a runner; imports and publication receipts use `--wait 0`. This moves
API compilation after the web producer too, so mixed backend/engine edits need elapsed-time
measurement as well as runner-minute measurement. Backend-only edits have no web dependency
work to wait for. Manual/tag image builds require an already available matching producer.

### Bevy Explorer CI status

A pull request that changes the explorer gets one `🚦 Bevy Explorer CI` comment, kept by the
[`bevy-explorer-ci-status`](../../.github/actions/bevy-explorer-ci-status/action.yml) action, with a section
per writer:

| Section | Written by | Holds |
| --- | --- | --- |
| *Build* | `bevy-explorer-ci-status.yml` | The web app and the headless engine with their artifacts, the web app's PR preview on the zone services (published only for a PR with the `ci:preview` label; label, then push or reopen), and the `benchmark` runner per platform |
| *Tests* | `bevy-explorer-ci-status.yml` | *Test Suite* per platform and *Headless Smoke*, with job times |
| *Performance* | `bevy-explorer-benchmark.yml` | After each passed CI run that built the web app: running, not run and why, cancelled, or the report with a badge from its *Overall* line |

`bevy-explorer-ci-status.yml` runs after each `bevy-explorer-ci.yml` run (`workflow_run`), in the
base context with write permission, and checks out only the action. It resolves the pull request
(`resolve-comment-pr.sh`: the run's own, else the open pull request whose head is the run's
commit) and whether the run's *Changes* job selected an explorer job (`explorer-run-selected.sh`:
*Test Suite*, *Headless Smoke*, *Clippy* or the web build not skipped). A run that selected none,
such as a backend-only change that CI runs for its image jobs, leaves the pull request without a
comment. When the run is requested, both sections go to pending; when it completes, they are
composed from its jobs and artifacts (`explorer-ci-sections.sh`). A cancelled run of a commit the
pull request moved past writes nothing. `automation` and `inworld` sections are allowed for suites
to come; they appear the first time one writes.

Each writer replaces only its section, strips marker-shaped lines from a body, truncates a section
over 12000 characters, re-reads to confirm the write and collapses duplicate comments
(`upsert-ci-status.sh`). The scripts' tests (`test-*.sh`, against a stubbed `gh`) run in CI's
*Integration guards*.

On main, a new push does not cancel a running CI run: it waits, replacing any run already waiting,
so each completed run tests every earlier commit. A manual dispatch or re-run on main also waits
and can replace a waiting push; to re-test main, push or dispatch rather than re-running an older
run. A pull request push replaces that pull request's running run. Image and headless publishers
run on successful main CI completion. They verify the source repository, workflow, SHA, attempt,
complete platform matrix and artifacts before publishing. The launcher takes its version from all
four platform packages, never the callback run ID. An APIs + Sites or headless release is
superseded only by a newer main run that changed that component and succeeded or was re-run (an
earlier attempt may have published); a newer first attempt still waiting or failed does not. Any
failed consumer build also fails the coordinated run and prevents automatic publication; graphical
acceptance is still a separate verdict. The standalone manual build/release entrypoints remain
available.

For timing queries, image jobs now appear under `Bevy Explorer / CI` as `APIs + Sites / build (...)`;
non-Linux package jobs use `Headless packages / build (...)`. Linux package compilation is the
`Headless Smoke` job; count it once rather than adding a second compilation series.
Graphical tests do not run automatically on PRs or block builds; check their verdict and
readiness (evidence supplied for the task) before claiming graphical acceptance. Manual dispatch accepts a
chosen ref; use `--ref main` for main validation. The default `full` suite runs all 14 scenes;
`snapshots` selects three diagnostic scenes and cannot establish acceptance. `scene-tests.txt`
lists every scene and the assertions it currently allows to fail (`tooling/bevy-ci.py scene-suite`
turns it into the runner's `--test_scenes` argument); a run passes only when every failure is
listed, so a new failure still fails main. The step summary (`scene-summary`) reports the counts,
unlisted failures and listed assertions that pass again, which should then be removed. Graphical
acceptance holds while the list is empty.

The graphical job has a 100-minute limit; scene execution has 20 minutes, leaving time to upload
logs and captures. Same-ref runs are serialized without cancelling the active run. Pushes compare
`before..head`; nightly dispatch retries all scenes even on unchanged main. Failed main assertions
still allow dependency-cache saves. Artifacts contain logs, screenshots and Cargo timings.

The harness retains player simulation but renders only 256×256 snapshot windows, skipping the
unused sky cubemap. Native UI is hidden and the background clock is frozen; scene lighting and
scene-authored time overrides remain active. Captures wait for completed render frames without
pending shader pipelines. Native video textures are held until GPU readback, then queued updates
resume. References keep their CPU pixels; invalid images fail comparison. The embedded references
(`crates/assets/src/assets/images/screenshots`) are this runner's own captures with the native UI
hidden; independent runs agree above 0.99999, well inside the scenes' 0.9995 threshold. After an
intended rendering change, regenerate a reference from the run's scene-test artifact: save
`<name>.png_output.png` as `<name>.png.png` (the runner writes a missing reference there itself).

## Naming and publication

Components: `bevy-explorer-web`, `bevy-explorer-desktop`, `bevy-explorer-headless`, `bevy-explorer-benchmark`. Artifacts:
`<component>-image-linux-<arch>-<sha>` and `<component>-<os>-<arch>-<sha>` (`linux|windows|macos`,
`amd64|arm64`, full dcl-one SHA).

Images publish only from a successful main CI run to `QUAY_BEVY_EXPLORER_WEB_REPOSITORY` and
`QUAY_BEVY_EXPLORER_HEADLESS_REPOSITORY` (secrets `QUAY_USERNAME`, `QUAY_TOKEN`), tagged
`sha-<sha>`, `sha-<sha>-<arch>`, `main`; only products that succeeded in that run publish. Global `v*`
tags do not trigger Bevy CI. Resolve each product's tag independently; never derive one product's tag
from the other's revision. Pin deployments to `quay.io/<org>/dcl-one-<component>@sha256:<digest>`:
replays can overwrite tags. Desktop/headless `publish` creates a prerelease tagged
`bevy-explorer-<component>-v0.1.0-<run-number>`.

## npm packages

| Package | Published by | Contents |
| --- | --- | --- |
| `@dcl-regenesislabs/bevy-explorer-web` | `bevy-explorer-ci.yml`, `publish-web` job | The web job's `deploy/web`, addressed to `https://cdn.decentraland.org/<package>/<version>` |
| `@dcl-regenesislabs/bevy-headless-server` + `-darwin-arm64`, `-darwin-x64`, `-linux-x64`, `-win32-x64` | `bevy-explorer-headless.yml`, `publish` job | Launcher, engine and `dcl_deno_ipc` sidecar |

`decentraland/oddish-action` stamps `0.1.0-<run id>.commit-<sha>`: `next` from main, `latest` from a
manual headless run on main, a chosen `publish-tag` from a branch. npm trusted publishing (OIDC): each
of the six names must list `decentraland/dcl-one` and its workflow on npmjs.com. Before the npm
publish, `decentraland/actions/cdn-deploy` uploads the web package over OIDC without a rollout;
once a main build's npm publish succeeds, the `rollout-web` job rolls that version out at
`decentraland.zone/play`. Retry with "re-run failed jobs": the upload skips a version already on the
CDN, and a failed rollout re-runs alone.

Same-repo PRs publish under the `pr-<n>` dist-tag. The CI status *Build* section links the exact
package version for the run after checking its CDN completion marker's commit against the web
checkout SHA. It uses the PR tag to look up the version if the run lacks a usable package artifact
or was dispatched manually.
A PR publish failure does not fail the build, and closing the PR removes the tag (the trusted
publisher needs "npm dist-tag"). A `pr-<n>` version is for testing only, never for a rollout;
while `BENCHMARK_ENABLED_RUNNERS` is `none`, a PR that compiles the engine also skips wasm-opt. The
automatic rollout takes only main builds, so `publish-tag` builds need "Manual CDN rollout".
Before archiving `decentraland/bevy-explorer`, verify one `next` of each package and re-adapt
`apps/ui3/src/play`.

## Container runtime

Web: port 8080, UID/GID 101; serves the app at `/` (a gateway must keep engine, bridge-scene and
service-worker paths same-origin). Headless: UID/GID 1000, writable `/data` and `/tmp`; supply realm,
scene, admission and deployment settings explicitly (no Catalyst or LiveKit included).

## Headless scene metrics

The native scene runtime samples counters and memory after completed updates, at
most once per five seconds. The headless emitter reports fresh observations about
every ten seconds as `@scene-stats` JSON, with `observed_at_ms`, `sample_seq`, an
execution identity and a monotonic `window_s`. Baselines belong to the scene
entity; restarted executions start fresh and stalled updates leave gaps.

`dcl_types` owns the shared counter payload. Native sampling and flushing run in
`dcl_deno`; web stats flush after the shared CRDT send returns, in the `dcl_wasm`
wrapper. The shared `dcl_js` operation updates counters but schedules no telemetry.

`cpu.run_ms` preserves fractional milliseconds and covers `onStart`/`onUpdate`
scene-thread CPU on Unix, including host-side fetch, websocket and renderer-response
work performed on that thread during those callbacks. It excludes other threads
and module loading. The first sample window includes module loading and the initial
CRDT send so startup counters have a matching interval; its CPU numerator still
covers only `onStart`/`onUpdate`. Non-Unix
builds omit CPU time and duration buckets because their runtime timer measures
wall time; update counts remain available. Heap usage,
V8 external memory and retained CRDT payload bytes are separate measurements,
not total scene RSS. Process RSS and cgroup memory are measured by the server.
Fetch status/duration/in-flight counts, open sockets, inbound comms and completed
update CPU buckets provide additional context.

Changes to `SceneResourceCounters` require publishing the headless engine and
`dcl_deno_ipc` sidecar from the same build, then updating the server's exact
`@dcl-regenesislabs/bevy-headless-server` dependency. Coordinate the server and telemetry schema rollout before updating that pin; resolve deployment-specific revisions and acceptance evidence from the current records supplied for the task. Review alerts on `scene_stats_reporting_scenes`: stalled
scenes stop producing samples and leave that count after 60 seconds. The server preserves source
windows through OTLP; receive-timed legacy observations cannot establish CPU
utilization. Metrics serialization, execution baselines and CPU bucket tests run with:

```sh
cd rendering/bevy-explorer/source # from the repository root
cargo test --locked --no-default-features --features headless --lib headless::stats
```
