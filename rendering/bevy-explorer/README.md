# Explorer integration

`source/` is the official Decentraland Explorer (engine, React HUD, desktop, headless server),
Apache-2.0, building in `source/target`. App builds, workflows and the
`@dcl-regenesislabs` npm packages live in [`apps/bevy-explorer`](../../apps/bevy-explorer/README.md)
([npm packages](../../apps/bevy-explorer/README.md#npm-packages)). Support links point at
[public Decentraland support](https://decentraland.org/help/).

Pins: the engine artifact is identified by its producer commit in `/play/sites-play.json` (no npm
engine pin). Sites runs the same shared HUD as the Explorer, so it has no separate HUD review pin.

## What runs where

- `/play`, `/play/app`: Sites routes (`apps/sites/src/routes/routes/play*.tsx`) mounting
  `apps/ui3/src/play`. `/play/app` sends COOP `same-origin` and COEP `credentialless`; the service
  worker keeps the `/play` shell unisolated so it alone holds the sign-in key.
- `/play/engine/`, `/play/bridge-scene/`, `/play/service_worker.js`, `/play/preview_realm.js`: the
  web artifact imported by `tooling/fetch-play.py` into `.cache/play/<manifest-hash>` (production:
  `build/client/play`, described by `/play/sites-play.json`).
- `library/explorer-core/engine/generated`: bindings from `crates/system_api_types`, which both HUD
  hosts compile; regenerate with `source/scripts/gen-ts-bindings.sh` and commit the result.

## The HUD

The HUD both hosts share lives in `library/ui/apps/hud` (`@hud/*`, the UI) and `library/explorer-core`
(`@explorer-core/*`, the page's core with no React); where they differ, it reads
the behaviour from `@hud-host`, each host's `host.ts` (`docs/ui-standards.md`, "Explorer HUD
transition"). Each host's `App.tsx` renders the shared `HudApp` with its own drivers and Shop page.
The rest of `apps/ui3/src/play` adapts `source/react-web/src` with these changes:

- `main.tsx` → `mount.tsx` (`mountPlayApp`), `shell/main.ts` → `shell/mount.ts` (`mountPlayShell`).
- Same-origin paths resolve against `/play/`: `lib/engineLaunch.ts` (the engine artifact under
  `/play/engine`), `lib/systemScene.ts` (no `sdk-commands` live preview), `lib/publicUrl.ts`
  (`apps/ui3/public/play`).
- Off `decentraland.org`, service URLs come from the gateway's prefix table (`lib/baseDomain.ts`);
  `?baseDomain=decentraland.org` selects the Foundation deployments.
- `lib/editorEmbed.ts`: the Creator Hub editor frames `/play/` with its trusted scene, the session
  fragment becomes the sandbox channel suffix, the engine runs with `editor`, and no bridge is
  reported. The shell mounts its app once per document.
- The native CEF HUD and its SDK bridge receive gateway service URLs from the desktop engine.
  CEF routes eligible snapshot images when displaying profiles and notifications (`@hud-host`'s
  `nativeProfileImage`, `react-web/src/host.ts`). Sites uses its web service configuration and the
  snapshot URLs returned by its backend (`apps/ui3/src/play/host.ts` returns them as given).
- The shared UI foundations: `react-web` loads `@dcl/one-ui/foundations.css` in `main.tsx`; `/play`
  does not load them yet (`docs/ui-standards.md`, "Explorer HUD transition").
- The Shop page and its query cache, each host's `App.tsx`: `react-web` renders the catalogue inside
  `features/shop/ShopQueries.tsx` (one client per account, remounted and emptied when another account
  or a sign-out replaces it); `/play` renders its in-world Shop, with the app-wide
  `QueryClientProvider` it uses, until that Shop moves to `react-web`.
- Tests of the shared HUD live once, in `library/ui/apps/hud` and `library/explorer-core`, and
  `react-web`'s `npm test` runs them; `/play` keeps the tests of its own files, run with `pnpm test` in `apps/ui3` under
  `tsconfig.play.json`. `?native=1` is not built.
- The benchmark's unattended run is not carried. The shared session only knows that a host may
  have an unattended run (`@hud-host`'s `bootMode` and `unattendedRun`); react-web's `host.ts`
  supplies the autopilot's (`?autopilot` read by `lib/bootMode.ts`, the local guest's sign-in, the
  run's failure report and its Genesis landing), `/play`'s supplies none. Nor are the shell's local
  guest (`shell/localGuest.ts`) and the stream params (`perfStream`, `perfCsv`). `/play` never runs the
  benchmark: its `lib/engineLaunch.ts` keeps `autopilot` out of the engine's launch options
  on every host (the key is absent, not `false`, so an engine that predates the field still
  launches), and `perfStream` and `perfCsv` are never read. Neither HUD's `lib/launchGate.ts` has
  an entry for these switches: the Explorer's HUD ignores them except on loopback hosts and in
  native, `/play` ignores them everywhere, and `lib/entryParams.ts` does not list `autopilot`
  among the parameters a link can set. Nothing else of the run is carried either, because nothing in
  `/play` calls it: react-web's autopilot drivers (`engine/autopilot.ts`, extending the shared ones
  with `loginLocalGuest` and `autopilotFailed`), the shell's `createLocalGuest` with the root key it
  keeps (`shell/login.ts`, `shell/localGuest.ts`), `lib/loopbackHost.ts`, the `__bevyPerfStream` page
  switch (react-web's `lib/engineLaunch.ts`) and the bridge channel's `__engineExited` exemption
  (`@hud-host`'s `engineExited`; `boot.js` sets it only when the engine ends its own run under the
  autopilot, which `/play` never starts, so `/play`'s host answers false).

Open: `library/explorer-core/features/auth/sso.ts` expects the Foundation auth site's `/auth/login`, which Sites does not
serve; guest sessions work.

## Forks and drift

Bevy and 17 forked sources are tracked as an upstream commit and a patch series under
`patches/<name>/` ([ADR](../../docs/adr/2026-11-14-explorer-sources-as-patches.md)).
`python3 tooling/patched-sources.py materialise` writes the rebuilt trees at `bevy/` and
`forks/<name>/`, which git ignores. Run it before any `cargo` command;
[the bevy fork](docs/guidelines/bevy-fork.md#changing-bevy) has `export`, the options and the
stale-tree check. `bevy_dui`, `boimp` and `rpc-rust` (package `dcl-rpc`) are exact upstream bytes
under `vendor/`.

Licences: each `patches/<name>/source.toml` records the licence of its source in `license`, and
each crate under `vendor/` records its own in its `Cargo.toml`.

The Nix build (`infra/packages/bevy-explorer.nix`, `nix build .#bevy-explorer`) makes one
derivation per source: it fetches `commit` with the `nix_hash` of `source.toml`, then runs the
same tool offline. `nix_hash` is the hash of a checkout of `commit` without submodules, and each
`[[submodule]]` has its own. CI does not build the Nix package, so nothing there proves
`nix_hash`: after a change of `commit`, run `nix build .#bevy-explorer.sources`, which fails with
the hash to record.

`python3 tooling/upstream-drift.py check` compares each source's pinned commit with its upstream
ref. The daily `bevy-explorer-upstream.yml` runs it and uploads the drift page
(`tooling/upstream-drift.py collect`, `tooling/upstream-drift-site.py`).

## Native builds and HUD updates

Build from `rendering/bevy-explorer/source` per its README (`just wasm` for the web engine), or
`make web` in `apps/bevy-explorer` (repository root). For native builds, follow the [native build guide](docs/guidelines/running.md#native). Import with `python3 tooling/fetch-play.py --from-build apps/bevy-explorer/dist/web`
or `python3 tooling/fetch-play.py` (main CI artifact); see the
[container build contract](../../infra/containers/README.md#sites-explorer-build-input).

When the HUD changes: regenerate the bindings and the shared inputs
(`npm --prefix rendering/bevy-explorer/source/react-web run gen:shared-inputs`), then run both hosts'
tests and typechecks (`npm test` in react-web, `pnpm test` in `apps/ui3`, whose `play-shared`
project runs the shared HUD against `/play`'s host). Record browser and multiplayer acceptance evidence for the target
revision in the task report; a matching fingerprint alone does not establish acceptance.

## Benchmark

A pull request that changes the explorer's web app is measured against main in Chrome on the laptop fleet:
`.github/workflows/bevy-explorer-benchmark.yml` takes both web apps (`bevy-explorer-web-<sha>`) and
the `benchmark` runner (`source/local-harness/benchmark`, `bevy-explorer-benchmark-<os>-<arch>-<sha>`)
from CI's artifacts and writes the report into the pull request's CI status comment. How a run
starts, how to read the report and how to set up a laptop: [Benchmark](docs/guidelines/benchmark.md).

## Authenticated login events

The Explorer queues a client-login event at the common successful identity completion for new
sign-in, previous-login restoration and browser-imported identities. It does not need an opened
social panel or social RPC connection. Unsigned guest mode does not emit authenticated events.
Each completion has a new UUID and epoch-millisecond occurrence time. Delivery sends an empty
`POST` to the configured `SocialApi` base plus `/v1/client-login`, with both values in the
ADR-44 signed metadata alongside `signer: "dcl:explorer"`. The server derives the wallet from
the auth chain.

The queue is separate from settings: `client-logins.json` in the native configuration directory,
or the browser's `dcl-client-logins` IndexedDB database. Each instance persists only its new
IDs and explicit removals. Browser transactions and native file locks merge those changes with
the latest stored state; a busy native lock defers persistence instead of blocking the worker.
Removal records remain for 24 hours and five minutes so an uncertain earlier save cannot restore
an acknowledged or discarded occurrence. The engine awaits the queue save before
sending. Empty/corrupt stores are repaired, with one `.corrupt` file or IndexedDB `corrupt`
record retaining the previous contents when quarantine succeeds. Native quarantine failure is logged
and does not block repair. Malformed records and additions are dropped individually while valid
records and acknowledgments survive. Three consecutive storage failures (ten-second timeout per asynchronous operation,
five seconds between retries) enable visibly degraded memory-only delivery without delaying login.
Unsaved changes remain in memory and storage recovery retries every minute. Normal persistence
gating resumes after recovery; events accepted in memory mode are removed during reconciliation.
Memory-only records can be lost on process/page shutdown. Each retry signs a fresh
request with the original event ID and occurrence time. A signing snapshot binds the event to
its wallet even if the current account changes. Logout cancels active delivery; pending records
resume when that wallet logs in to the same service again.

One request runs at a time, with a ten-second deadline covering signing and HTTP. Network
failures, HTTP 408, 429 and server errors use randomized exponential backoff bounded between
one second and five minutes. HTTP 204
acknowledges the event. HTTP 404/405 retain it with a randomized 150–300 second backoff so client
and server deployments can arrive in either order. Other client-error statuses stop its delivery
and log only the status code. Native response bodies are drained up to 16 KiB or one second for
connection reuse; browser fetch manages its connections. Response contents are never logged. Records expire after 24 hours. At most 32 pending records are retained per runtime and in shared storage;
adding one beyond the limit discards the oldest. The debug panel's **Client login events**
section and `/client_login_state` expose pending count, persistence, memory-only mode, consecutive storage failures, attempts, result and
session discard count. Concurrent processes and browser tabs share the durable queue without
replacing each other’s pending records; the shared capacity policy discards the oldest occurrence.

This producer covers the owned Bevy Explorer, including its imported web identity path.
Website-only sign-ins and external Explorer clients do not emit this event through this code.
