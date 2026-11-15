# bevy-explorer

A forward-looking implementation of the Decentraland protocol, written in [rust](https://www.rust-lang.org/) on the [Bevy](https://bevy.org) engine.

![screenshots](montage.png)

One engine, three products:

| Target | What it is | Where it ships |
| --- | --- | --- |
| **Web client** | The engine compiled to WebAssembly (WebGPU), with a React DOM HUD, running in the browser | `@dcl-regenesislabs/bevy-explorer-web` → served at `decentraland.zone/bevy-web` |
| **Desktop client** | Native binary for macOS / Linux / Windows, same React HUD rendered through an offscreen CEF webview | [Releases](https://github.com/decentraland/bevy-explorer/releases/latest) |
| **Headless server** | Authoritative scene server, no rendering — drop-in replacement for `@dcl/hammurabi-server` | `@dcl-regenesislabs/bevy-headless-server` (npm) |

This project's goals are to:
- document current and future protocol standards
- experiment with changes to the protocol
- increase the field of alternative Explorers
- prioritize solid fundamentals, extensibility, and the use of modern open-source frameworks

## Repository layout

| Path | What |
| --- | --- |
| `src/` | binaries: `decentra-bevy` (client), `decentra-bevy-cef` (CEF render-process helper), `headless` (scene server) |
| `crates/` | the engine, split by domain (`scene_runner`, `comms`, `avatar`, `ipfs`, `dcl_deno`, `system_bridge`, …) |
| `react-web/` | the React DOM HUD — one codebase for both web and desktop; see [`react-web/README.md`](react-web/README.md) |
| `react-web/bridge-scene/` | headless SDK7 "super-user" scene that relays engine ↔ React over a `BroadcastChannel` |
| `deploy/web/` | the published web tree: engine boot module + workers + wasm, bridge scene, service worker |
| `deploy/headless/` | npm launcher + per-platform packaging for the headless server |
| `deploy/macos`, `deploy/linux` | desktop packaging (installer, AppImage) |
| `docs/`, `react-web/docs/` | design notes and backlog |

## Prerequisites

Common to every target:

- [rust](https://www.rust-lang.org/tools/install) (stable; the wasm build needs nightly — see below)
- [protoc](https://github.com/protocolbuffers/protobuf/releases) — `brew install protobuf`; on windows, the `protoc-<version>-win64.zip` that `apps/bevy-explorer/install-protoc.py` pins, with its `bin` on `PATH` (or `PROTOC` set)
- node 20+ (24 in CI) for the React HUD and bridge scene
- optionally [just](https://github.com/casey/just) — `just --list` for the dev entry points used below; on windows its recipes need Git for Windows: run `just` from Git Bash (from PowerShell it finds no `sh`)

Platform libraries (needed by the native and headless builds):

- **linux**: `sudo apt-get install --no-install-recommends libasound2-dev libudev-dev ninja-build clang cmake pkg-config libssl-dev libx11-dev libgl1-mesa-dev libxext-dev` plus ffmpeg dev packages (`libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev libavdevice-dev`)
- **macos**: `brew install ffmpeg@6 pkg-config ninja` and `export PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg@6/lib/pkgconfig`
- **windows**: install [clang/LLVM](https://github.com/llvm/llvm-project/releases/) and set `LIBCLANG_PATH`; unzip [ffmpeg 6.0 shared](https://github.com/GyanD/codexffmpeg/releases/download/6.0/ffmpeg-6.0-full_build-shared.7z), set `FFMPEG_DIR` to its root and add `ffmpeg\bin` to `PATH` (ninja and cmake ship with visual studio); keep `CARGO_TARGET_DIR` short, free of spaces and on the drive of `CARGO_HOME` ([why](../docs/guidelines/platforms.md#native))

## Web client

The engine is compiled to wasm and boots **in the React page's own document** (no iframe): the canvas sits behind the HUD, and the bridge scene relays between them.

Hidden tabs and an editor viewport covered by another preview run at up to 5 FPS. Returning
to the viewport restores the configured frame rate without changing saved graphics settings.

```bash
rustup toolchain install nightly-2026-04-15 --target wasm32-unknown-unknown --component rust-src
cargo install wasm-pack
just wasm     # builds the wasm into deploy/web/engine/pkg, then serves react-web and opens a browser
```

With Nix, `nix develop .#wasm` from the repository root replaces the two installs: the shell carries the pinned nightly with the wasm target, `just`, `wasm-pack`, `wasm-bindgen` at the `Cargo.lock` version, `wasm-opt`, node, pnpm and protoc.

`just wasm` is the whole loop: `just ts-bindings` (the HUD imports the generated types), `wasm-pack build` on the pinned nightly for the engine and for the scene runtime (`crates/dcl_scene_wasm`, into `pkg-scene`), re-bundle the sandbox worker (it inlines the wasm glue, so it must be rebuilt with the wasm), `npm install` in `react-web`, a frozen pnpm install in `react-web/bridge-scene`, then `npm run dev`.

Useful URLs once the dev server is up:

- `http://localhost:5173/` — real engine + live bridge-scene preview on :8100
- `http://localhost:5173/?mock=1` — full HUD on a fake bridge, **no engine build needed**
- `http://localhost:5173/?bundled=1` — engine loads the exported static bridge scene, i.e. exactly what ships

Requires a WebGPU-capable browser. Deployment topology (versioned CDN base, same-origin rules, COEP service worker) is documented in [`react-web/README.md`](react-web/README.md).

## Desktop client

The desktop build renders the same React HUD through an offscreen CEF webview (`react-hud-cef`, a default feature). Without the HUD bundle the app runs with **no UI at all**.

```bash
just setup-cef                 # once per machine: exports the CEF distribution to ~/.local/share/cef
export CEF_PATH=$HOME/.local/share/cef
# linux only, to run from the target dir:
export LD_LIBRARY_PATH=$CEF_PATH:$LD_LIBRARY_PATH
# nixos only, when running without just: lets CEF's bundled vulkan loader find the drivers
export XDG_DATA_DIRS=/run/opengl-driver/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}

just native-release            # bundles the HUD if stale, then builds + runs everything
```

`just native-debug` is the same in debug. Both pass extra arguments through:

```bash
just native-release --realm https://realm-provider-ea.decentraland.org/main --position 52,-52
```

`--base-domain` retargets the backend hosts (auth, comms, places, worlds, social, ...) at another deployment's domain. `renderer-artifacts` (sdk6 adaption layer) and `builder-items` (inspector asset catalog) stay on decentraland.org — they have no other deployment:

```bash
just native-release --base-domain interconnected.online --position 0,0
```

On web the same thing is the `?baseDomain=` query param, e.g. `?baseDomain=interconnected.online`. Without the param, the hosting origin decides: a page served under decentraland.zone keys to zone backends, anything else to org.

One service at a time: each has its own flag taking a full base url that replaces that service's composition while the rest keep following the domain (`--help`, "Service endpoints"). The same names are web query params, e.g. `--catalyst http://localhost:3000` / `?catalyst=http://localhost:3000`:

```bash
just native-release --places http://localhost:5000 --realm-provider http://localhost:8000
```

Doing it by hand instead of via `just`:

```bash
python3 ../../../tooling/patched-sources.py materialise   # Bevy and the forked crates cargo reads
./scripts/gen-ts-bindings.sh                     # TS types for the system API (library/ui/apps/hud, tracked)
cd react-web && npm ci
(cd bridge-scene && corepack pnpm@11.27.0 install --frozen-lockfile --ignore-scripts && corepack pnpm@11.27.0 --filter @dcl/hashing build)
npm run bundle:native && cd ..
cargo build --release --package dcl_deno_ipc     # scene runtime sidecar
cargo build --release                            # decentra-bevy + decentra-bevy-cef
cargo run --release
```

To build without CEF at all (the engine's own bevy-ui HUD instead of React), after the same `materialise`:

```bash
cargo run --release --no-default-features --features "livekit,ffmpeg,inspect,social"
```

## Headless server

An authoritative scene server with no renderer — the SDK spawns it for scenes with `authoritativeMultiplayer` enabled.

```bash
npx @dcl-regenesislabs/bevy-headless-server --realm http://localhost:8000
```

From source (the binary is feature-gated out of default builds, and execs the sidecar from its own directory, so both land in `target/release`):

```bash
python3 ../../../tooling/patched-sources.py materialise   # Bevy and the forked crates cargo reads
cargo build --release -p dcl_deno_ipc
cargo build --release --bin headless --no-default-features --features headless,livekit
./target/release/headless --realm <url> --location 0,0 --server-mode
```

`--orchestrated` runs it as a multi-scene worker driven over stdin/stdout instead. See [`deploy/headless/launcher/README.md`](deploy/headless/launcher/README.md) for the CLI contract and [`docs/headless-sdk-preview.md`](docs/headless-sdk-preview.md) for how it replaces hammurabi in the SDK preview.

## Arguments

`cargo run --release --bin decentra-bevy -- --help` lists every flag with its description.

The launch parameters shared with the web build — realm, spawn parcel, ui scene, portables, preview, editor, content server, pulse server, imposter source, base domain, fps logging, gpu bytes per frame — are declared once, in `crates/system_api_types/src/launch_options.rs`: each field is the native `--flag`, the `engine_run` options key and the entry-url query param, and the react page's parameter table is generated from it. Which of those a link may set without a warning is the page's own policy (`react-web/src/lib/launchGate.ts`). Native-only flags (rendering, imposters, debug, the `--builtin-*` ui pieces) live on `DecentralandArguments` in `src/lib.rs`.

### Bevy variants (prototype)

abgen can publish, per scene, wearable and emote entity, GPU-ready variants of its textures: KTX2 files holding BC7, BC1 or BC5 blocks, level 0 alone or with its mip chain, each level zstd-supercompressed. The explorer reads them from `--bevy-variants-url <url>` natively, `?bevyVariants=<url>` on web, or the `bevy_variants_url` field of the config file, and otherwise from abgen's CDN for the base domain (`https://abgen-cdn.decentraland.org`, or the `abgen_cdn` service's override). `off` in any of them turns variants off.

With a base url:

- For each scene, wearable and emote entity it loads, the explorer fetches `{base}/bevy/manifest/{entityId}.json` once per session; a scene's is requested with its entity file. Manifests and variants are fetched in one attempt with short timeouts, since the original is the retry. A 404 means no variants for that entity. The entity's files wait for the manifest for at most two seconds from the request; after that they load their originals, and a manifest that arrives later serves only the files read after it. The manifest (schema `v: 3`) maps each original content CID to the paths of its variants, `bevy/{prefix}/{name}`; a prefix's first letter is the variant's class (`t` a BC KTX2 texture, `m` a `glb-br` model, `g` a GLB). It names no hash for a variant: abgen keeps a path's bytes fixed for the life of its generation, so the explorer trusts a variant by its path from the configured service and rejects only what fails to decode.
- Nothing in preview mode asks for a manifest, including wearables and emotes, nor do `b64-` entity ids or entities served from a loopback or `file://` content server.
- When a content file `.png`, `.jpg`, `.jpeg`, `.webp` or `.ktx2`, or one without an extension, has a texture variant and the GPU supports BC textures, the file is read from `{base}/{path}` instead. The bytes are cached under a digest of that url, never under the original's hash.
- The reader hands the KTX2 to the image loaders as it is, after checking that its header names a BC format (BC1, BC5 or BC7). The loaders for `.png`, `.jpg`, `.jpeg`, `.webp`, `.ktx2` and paths without an extension unwrap it into a DDS with a DX10 header, inside the asset load: each level is zstd-decoded, and its size must be exactly the block bytes its dimensions need. Dimensions above 8192, a level 0 that is not whole 4×4 blocks, more levels than the mip chain, sRGB or other formats, and any other supercompression are rejected. The asset keeps its original path, and the texture is sRGB or linear as it is requested.
- On web, a `.glb` file with a `glb-br` variant is read from `{base}/{path}.br`, the Brotli copy of the original file, which the browser decodes. The bytes must hash to the original file's own content id (they are the original byte for byte), then go to the glTF loader of the original path; anything else falls back to the original. The native explorer loads original models for now.
- `glb` variants are not used. They embed their textures as KTX2 under `KHR_texture_basisu`, and the glTF loader rejects a file that requires that extension.
- Any failure falls back to the original for that file: no variant of a supported class, a failed fetch, a KTX2 that does not unwrap, or an image that fails to load from the variant. A failed variant is not tried again that session, and its cached copy is dropped. Five consecutive failed requests to the service turn variants off for the session.
- Image processing skips assets loaded from a variant, so their bytes are never re-encoded or written over the original's cache entry.
- The runtime debug panel's "Optimized variants" section, and the `/bevy_variants` console command, show the base url, the formats taken, each scene's manifest state, how many variants were served in all and per format, and recent fallbacks with their reasons.

On web, a link that sets `?bevyVariants=` to a host outside Decentraland's own deployments stops on the launch gate, as `?contentServer=` does. The variants service decides what those scenes look like. On web the service must also send CORS headers for the explorer's origin.

To try it against a local abgen:

```bash
just native-release --realm https://realm-provider-ea.decentraland.org/main --position 0,0 --bevy-variants-url http://localhost:5147
```

## Smart wearables and portables

A smart wearable is a wearable whose content carries an SDK7 scene (`scene.json`, packed under the
body-shape folders `male/` and `female/`, or bare). The engine runs that scene as a portable
experience while the wearable is equipped, and stops it when it is unequipped. On entering a realm
it also fetches `{realm}/preview-wearables`, the creator preview path, and runs each wearable
listed there the same way.

The scene loads like any other portable: a wearable's collection carries a `{shape}.smart_wearable`
alias for its body shape's `scene.json`, and loading that alias as the scene's entity
(`crates/collectibles/src/smart_wearables.rs`) roots the shape's folder, so a `scene.json` that is
not SDK7 or lacks its `main` fails the scene's load. Its permission requests go through the stored
rules and the per-use prompt like any portable's; `requiredPermissions` is not read
([decisions](../docs/guidelines/decisions.md)).

The parcel scene the player stands in decides what portables do there:
`featureToggles.portableExperiences` set to `disabled` pauses every listed portable (every portable
but the startup ones and the HUD's own scene) and hides its UI while the player is inside it;
`hideUi` keeps them running with their UI hidden. The HUD follows the listed portables through
`getPortablesStream` (`PortableInfo`: each one's kind, `loading`, `running`, `failed`, `blocked` or
`stopped`) and shows them in the sidebar's `Portable experiences` popover, each with `Stop`; a
stopped smart wearable stays equipped and offers `Restart`. A smart wearable is listed once its
wearable data says it is one (its scene resolved); while an equipped wearable's data is still
loading, or when it has no scene or could not be resolved, it is not a portable and does not
appear (`/smart_wearables` shows those phases).

Console commands:

- `/portables`: `<name> [<hash or ->, <scene|smartWearable>, <state>]` per listed portable, or
  `no portables`. `/stop_portable <hash>` stops one (replies `stopped <hash>`).
- `/smart_wearables`: `current scene allows: true|false`, then `<urn> <state> [<reason>]` per
  wearable (`loading`, `running`, `blocked`, `stopped`, `notSmart`, `failed`), or
  `no smart wearables`.
- `/smart_wearable run <urn>` runs a wearable's scene without equipping it (ownership is not
  checked); `/smart_wearable kill <urn>` stops one; `/smart_wearable restart <urn>` clears a stop or
  retries a failure.
- `/live_scenes` marks their scenes `, portable, smart wearable`; `/list_portables` prints every
  entry of `PortableScenes` with its kind (`scene`, `startup` or `smart wearable`), startup
  portables included: a debug listing, where `/portables` and the scenes' own
  `getPortableExperiencesLoaded` hide them.

## Testing

```bash
python3 ../../../tooling/patched-sources.py materialise   # Bevy and the forked crates cargo reads
cargo test --all                      # engine
npm test --prefix react-web           # HUD, deterministic (vitest, no engine)
npm run test:e2e --prefix react-web   # HUD against a real engine (playwright, needs a GPU)
```

CI's Test Suite builds the engine tests with the `ci-test` profile. The `dcl_deno_ipc` integration
test makes Cargo build the runnable sidecar in the same profile directory, where the scene tests
look for it.

```bash
cargo test --all --profile ci-test --locked -- --test-threads=1
```

`react-web/review.md` is the pre-merge checklist for anything under `react-web/`.

## Agents and tests

The console (`/help` lists it; `window.engine_console_command` in the app frame on the web) is the
client's agent surface, the same in every build: end-to-end tests and people's own agents drive the
deployed clients through it, the HUD included, so nothing outside the client rebuilds the plumbing.
Replies are JSON where the command says so; a failed command rejects (web) or answers `ok: false`
(loopback), with no marker in the text. Arguments split on whitespace.

- State: `/world_state` (`inWorld`, `arrivals`, which grows on every return to the world, so a wait
  after a teleport reads it first; `keyboardClaimed`, true while a text field holds the keys),
  `/avatar_state [all|<address>]`, `/chat_history [count]`
  (Nearby only, as received: blocks and scene bans are not applied), `/dm_history <address|community:<id>> [count]` (a DM partner's or a community's
  stored lines), `/community_state` (community chat's router, channels and counters),
  `/voice_state` (the nearby voice rooms and, as `call`, the private call or community stream the
  player is in: `status`, `kind`, `peer`, `communityId`, `connection`, `participants`,
  `micEnabled`, `localSpeaker`, `localRole`, `error`, `errorIsNotice`),
  `/connectivity_state` (safe room state, offline warning and reachability probe; room states
  match voice diagnostics, with `connecting` for first admission and `failed` for refused dials),
  `/connectivity_retry` (immediate recovery without overriding denied access),
  `/ban_block_state [address]` (platform ban, current scene admission, social blocks and
  scene moderation lists; Nearby chat and reactions from an address the scene room bans are
  dropped; `/ban_block_refresh` re-checks the platform ban), `/entity_screen_position <entity>
  [--bounds]`, `/photo_mode [on|off]` (the camera reel's in-world camera: its state, or open or
  close it) and `/take_photo` (a photo, captured and uploaded; the reply is the state, a refusal
  names why), plus the older `/player_position`, `/get_user_data`, `/connected_players`, `/time`
  and the scene commands.
- Social: `/block_user <address>` and `/unblock_user <address>` block and unblock through the
  social service, as the passport does.
- Chat: `/chat <channel> <word>`, `/dm <address> <message>`, `/community_chat <id> <message>`
  and `/react <channel> <message id> <emoji>` send as the HUD does; `/community_channels [add
  <id>|remove <id>|clear]` lists or edits the community channels the engine listens to, which
  the HUD sets from the user's communities (the engine drops community messages on any other).
- Voice chat, as the HUD's buttons do it and through the same refusals: `/voice call <address>`
  rings a user, `/voice accept` and `/voice reject` answer a ringing call, `/voice end` hangs up
  or leaves a stream, `/voice start|join <community id>` opens or joins a stream, `/voice close`
  ends it for everyone, `/voice hand up|down`, `/voice mic on|off` (the call microphone; the
  nearby one is the `Microphone` key) and `/voice promote|demote|kick|deny <address>`. Each only
  queues the request; read `/voice_state` `call` for the outcome. The debug panel's "Voice call"
  section shows the call and binds these actions.
- Saved mutes for nearby voice: `/nearby_mute on|off <address>` mutes or unmutes a wallet, as the
  profile menu's Hush and Hear do, and `/nearby_mutes` reports the account's list. The debug
  panel's "Nearby voice mutes" section shows it and binds both.
- In development builds the lower-right bug button toggles the debug panel. In all builds,
  open **Sidebar settings (•••) → Debug panel** or type `/debug` in chat. Search by
  section or control, or enter `/debug Performance` directly. Expanded sections refresh
  about once per second. `/debug help` lists the available sections. Engine plugins
  advertise their queries, fields and controls through `/debug_panel`. The HUD offers session actions
  and checks `/debug_panel --dev-build` for the engine's compile-time development flag.
  Use the panel to inspect a scene, compare frame and memory measurements, diagnose
  room and media connections, and exercise supported controls. Availability depends
  on the build, platform and preview mode. `/performance_state` reports the effective
  frame-rate target and session override independently of the panel.
- The HUD, by role and accessible name (`library/explorer-core/lib/hudAutomation.ts`): `/hud_list [role]`,
  `/hud_read`, `/hud_click`, `/hud_fill` (`{"name", "text"}`), `/hud_wait` (`"state"`: `visible`,
  `hidden`, `enabled`, `actionable`, `ready`; `"timeout"` ms). A selector is the name or JSON
  `{"name", "role", "index", "exact"}`; several matches need an `index`. A control still moving
  (a CSS animation or transition), or under a pointer lock that takes every click, is not
  `actionable`; one inside an `aria-busy` region reads
  `"busy"`, and `ready` waits it out.
- Signing in through the HUD (web, under a test runner driving the browser): `/hud_login
  <identity> [--name <name> --look <n>] [--guest] [--default-on-error] [--realm <realm>]
  [--position <x,y>]`, with an AuthIdentity encoded as `/login_identity` takes it, lands in the
  lobby or on the way to the destination; `/login_identity` alone signs the engine in under a HUD
  that keeps its start page. `/hud_logout` signs the HUD out, the engine with it, so `/hud_login`
  can sign in again; the engine's `/logout` leaves the HUD signed in.
- Input: `/press_key <code> [--state down|up]` (`KeyboardEvent.code` names) and
  `/click_at <x> <y> [--button left|right|middle]`. Natively these and the HUD commands go through
  CEF and the window's own input, always real. On the web the page needs a DevTools client to make
  them real (below); without one they fall back to DOM events and say `"input": "synthetic"`.
- Portables and smart wearables: `/portables`, `/stop_portable <hash>`, `/smart_wearables`,
  `/smart_wearable run|kill|restart <urn>` and the `, smart wearable` mark in `/live_scenes`
  ([Smart wearables and portables](#smart-wearables-and-portables)).
- Scene pointer: `/hover_entity <entity>` and `/click_entity <entity>` hover and click a scene
  entity by id through the pointer pipeline, on native and web alike, and answer with the hit, the
  hover entries, the scene's pointer events and a diagnosis (`docs/guidelines/running.md`). A hover
  lasts at most 1800 frames (`--frames`) and is never cut short by `--timeout-frames`.
- Raycasts: `/raycast_stats` (the capped tier's `budgetMs`, `ran`, `deferred`, `spentMs` and the
  uncapped tier's `uncappedRan`, `uncappedSpentMs`) and `/raycast_budget [ms|auto]` (override the
  capped budget, a tenth of the target frame, from 0 to 10 ms). On the web, these and the two pointer commands go
  to the engine through `engine_console_command` and write Bevy input events, unlike `/press_key`
  and `/click_at`, which need a DevTools client for real input. `docs/guidelines/interaction.md`
  has the options, replies and diagnoses.
- The preview-only commands (`/jump`, `/speed`, `/move_player_to`, the debug dumps) stay preview-only.

**The loopback console.** Every native build, the headless client mode included, carries a JSON-RPC
server on `127.0.0.1` that runs console commands from outside the process, off unless started with
`--automation-token <token>` (`--automation-port <port>`, 15702 by default; `DCL_AUTOMATION_TOKEN`
and `DCL_AUTOMATION_PORT` stand in for the flags). Two methods, none of Bevy's own: `dcl/console`
(`{"command": "/teleport 10 20", "token"}` answers `{"id"}`) and `dcl/console_result` (`{"id",
"token"}` answers `{"done": false}` until the command has answered, then `done`, `ok`, `output`).
Both need the token. The headless client registers `/world_state`, `/chat_history` and
`/voice_state`, and runs the startup portables (`--portables`, the movement scene by default), so
`/walk_player_to` walks it through the movement scene, as on the desktop client; the HUD, input
and screen-position commands do not apply there.

**Trusted input on the web: a stable contract.** A DevTools client that drives the page (the runner
in `local-harness/web-driver`, or a Playwright or Puppeteer script through `exposeBinding` and its
own mouse and keyboard) installs `window.__dclTrustedInput(json)` in every frame of the page's
origin. The page keeps the finding and calls the hook with one JSON string, `{"id", "op", ...}`:

| `op` | fields |
| --- | --- |
| `click` | `x`, `y` (CSS pixels in the calling frame's viewport), `button`: `left`, `right` or `middle` |
| `key` | `code`, `key`, `keyCode` (Windows virtual key code), `text` (optional), `location` (0, 1 left, 2 right), `state` (optional, `down` or `up`; absent presses and releases) |
| `insertText` | `text` |

The client performs the input on the top-level page, offsetting `x` and `y` by where the calling
frame sits (the app frame is out of process), then evaluates `window.__dclTrustedInputDone(id,
error)` in the frame that asked, `error` a string or `null`. A key with `state` `down` stays held,
its modifier on every event until its `up`. The page waits 10 s for the answer, then fails the
command. Nothing else about the runner is part of the contract; `local-harness/web-driver/README.md`
documents that runner (Node 24 and Chrome, no dependency), and the end-to-end suite in
`apps/bevy-explorer/e2e` builds on it.

## CI and release channels

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) is the most accurate source of build information — it covers fmt, clippy, the test matrix, a headless smoke test, and the web build/deploy.

- **Web** — every push to `main` publishes `@dcl-regenesislabs/bevy-explorer-web` (npm + CDN) from `deploy/web`.
- **Headless** — `publish-headless.yml` publishes a snapshot on `next` per main push; releasing to SDK previews means dispatching it with `dist_tag=latest`.
- **Desktop** — `package.yml`, dispatched manually, cuts a GitHub prerelease (linux + windows; the macOS leg is disabled pending notarization secrets).

Built by DCL Regenesis Labs — dclregenesislabs.xyz

Powered by the Decentraland DAO
![Decentraland DAO logo](https://bafkreibci6gg3wbjvxzlqpuh353upzrssalqqoddb6c4rez33bcagqsc2a.ipfs.nftstorage.link/)
