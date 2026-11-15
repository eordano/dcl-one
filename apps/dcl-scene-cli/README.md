# dcl-scene-cli

One Rust binary that scaffolds, builds, previews and publishes Decentraland SDK7 scenes, replacing
`@dcl/sdk-commands` 7.27.0 ([js-sdk-toolchain](https://github.com/decentraland/js-sdk-toolchain)) with
the same commands and flags. rolldown, the TypeScript toolchain, abgen and `livekit-server` are in the
binary; nothing to `npm install`. The vendored runtime pins `@dcl/sdk` 7.29.1-34986384248.commit-bb45080
(src/vendor/README.md).

## Start a scene

```
dcl-scene-cli init [--dir D] [--project scene|smart-wearable|px-template|library] [-y|--yes]
                   [--skip-install] [--node-modules-only]
dcl-scene-cli init [--dir D] --template ZIP_URL_OR_PATH | --github-repo https://github.com/O/R[/tree/B/PATH]
dcl-scene-cli get-context-files [--dir D] [--offline]
```

`init` writes embedded templates and `node_modules` offline (`--skip-install` leaves them out).
`smart-wearable` ships a placeholder `model.glb` and `thumbnail.png` beside `wearable.json`, so it
builds, packs and deploys before the real assets land; `px-template` is a scene manifest flagged
`isPortableExperience`; `library` is an SDK library that
exports `initLibrary(engine)` and previews as a scene (`npm run build:lib` = `build --single
src/index.ts --emitDeclaration`). `--template` unpacks a zip with one top-level folder (a GitHub
branch archive, say); `--github-repo` fetches the branch archive (`main`, else `master`, or the
`/tree/<branch>` named) and keeps the `/tree/<branch>/<path>` folder alone. A download from an
`https://` URL follows no redirect to `http://`, ends after 30 s of silence, and is refused past
256 MB or when it unpacks to more than 1 GB; a plain `http://` URL is fetched with a warning. Either
way the vendored `node_modules` is installed, and dependencies it lacks are named. The `/inspector/*` UI needs
`npm install --save-dev @dcl/inspector` or `DCL_ONE_INSPECTOR_DIR`. `get-context-files` writes the
`migrate-smart-items-to-code` skill and downloads `ai-sdk-context` into `dclcontext/`.

## Build it

```
dcl-scene-cli build [--dir D] [-p|--production] [-w|--watch] [--ignoreComposite]
                    [--customEntryPoint] [--skip-install] [--skip-type-check] [--built-in]
                    [--single GLOB] [--emitDeclaration]
```

`--single GLOB` bundles each matching `.ts`/`.tsx` under the project to a sibling `.js` instead of the
scene, after one type check; the SDK stays a `require`, as in the scene chunk, so a consuming scene
bundles one SDK. Two matches that would write one `.js` (`a.ts` and `a.tsx`) are refused.
`--emitDeclaration` has the type check also write `.d.ts` files
(`--emitDeclarationOnly`, into tsconfig's `declarationDir` or `outDir`; TypeScript 6 wants `rootDir`
set beside them); with `--watch`, `--skip-type-check` or a custom `scripts.build` it writes none and
says so. `--watch` applies the `--ignoreComposite` and `--customEntryPoint` of an SDK `scripts.build`,
as `start` does. The check runs the scene's own `typescript`: 5.x, 6.x or 7.x (the Go port); the
vendored one is 6.x. A tsconfig TypeScript 6 only
deprecates (`baseUrl`, say) is checked with `--ignoreDeprecations 6.0`, and said so: at once when
the tsconfig shows it (`baseUrl` alone) or an earlier check found it (`.dcl-cache/`), after one
failing run otherwise. TypeScript 7 has removed those options, and the error's steps name the edit.

### Custom build scripts

A `package.json` `scripts.build` is run by `build`, `start`, `deploy` and `pack`: once, from the scene
root, via `sh -c` (Windows `cmd.exe /d /s /c`) with `node_modules/.bin` on `PATH`; no npm hooks,
install or SDK fallback. A script that is the SDK build itself (`sdk-commands build` or
`dcl-scene-cli build`, flags included, with or without `npx`/`pnpm exec` in front) never spawns: it
runs as the built-in pipeline, in production mode under `deploy`. Any other script must terminate,
exit zero and leave a nonempty bundle at `scene.json.main`. `DCL_SCENE_CLI_PRODUCTION` is `1` for deploy, `0` for preview;
`DCL_SCENE_CLI_MAIN` names the bundle. Keep output in its own directory (normally `bin/`) and
serialize build and deploy of one scene. `build --built-in` forces the SDK pipeline (also from inside
a script); `--skip-build` skips the initial run.

A scene with no `node_modules` folder (a fresh clone) gets the vendored SDK restored before the
built-in pipeline runs, with a line saying so and a second one when `package.json` pins another
`@dcl/sdk`; `--skip-install` turns that off, and an existing `node_modules` is never touched.

The embedded SDK retains its pinned 7.29.1 runtime and local fixes. Its Skybox, Sun and
EnvironmentFog bindings (1223–1225) are generated from the monorepo protocol/SDK sources.
After generating the SDK components, `node apps/dcl-scene-cli/scripts/refresh-environment-sdk.mjs`
refreshes only those bindings and their prebuilt registry exports. It requires the SDK TypeScript
and Explorer HUD esbuild dependencies to be installed. The refresh checks the pinned base files,
keeps unrelated ZIP entries byte-for-byte, and records source/output hashes inside
`@dcl/sdk/prebuilt/environment-bindings.json`. Run
`node --test apps/dcl-scene-cli/scripts/environment-sdk.test.mjs` for the embedded CJS and
prebuilt-registry wire contracts and strict declaration check; this development check requires
Node, Python 3 and the installed SDK TypeScript dependency. SDK CI runs it after installation.
The normal scene build uses these bindings without npm.


The built-in pipeline bundles to `bin/index.js`, type-checks beside the build with incremental tsc
(`.dcl-cache/`), and encodes `main.crdt` in Rust (`DCL_ONE_CRDT_VERIFY=1` compares with node).
A watcher (`start`, `build --watch`) keeps one node process with tsc 5.x or 6.x loaded and the last
program in memory, so the check after a save covers the edit alone; it reports what a spawned tsc
does, ends with the watcher, and with TypeScript 7 each check spawns tsc.

## Preview it

```
dcl-scene-cli start [--dir D] [-p|--port N] [--skip-build] [--no-watch] [-m|--mobile]
                    [--explorer-alpha] [--web] [--dclenv org|zone] [--realm URL] [--position X,Y]
                    [--hub] [-n|--open-deeplink-in-new-instance] [--landscape-terrain-enabled]
                    [--data-layer] [--offline-comms]
                    [--no-livekit | --livekit-url WS_URL --livekit-api-key KEY
                     --livekit-api-secret SECRET|--livekit-api-secret-file F] [--livekit-room NAME]
                    [--no-asset-bundles] [--no-mcp] [--mcp-port N] [--error-source-lines-context N]
                    [--no-host] [--tunnel WSS_URL] [--public-url URL] [--talk [--player URL]]
                    [--skip-install] [-- EXPLORER_PARAMS...]
```

Serves a local realm on 8000 and reloads the client on save. `http://127.0.0.1:8000` is the launch
page (deep-link knobs, web **Player** via `?where=web&player=<url>`). abgen on 5147 serves
`{realm}/optimized-assets`. Scene errors print via the client's MCP port (default 8123, range
1024-65535). `/content/contents/{hash}` resolves by path: a stale digest gets the current bytes.
`--data-layer` serves the Creator Hub protocol at `/data-layer` and `/inspector/`.
`/preview-wearables` lists the smart wearables this preview serves and `/preview-wearables/{id}`
answers one by its scene id or wearable id (empty `data` when none matches, as upstream), the
routes the wearable preview page polls.

The join block always prints the desktop deep link and the web URL; nothing launches unless asked.
`--mobile` adds a scan-to-join QR code for the web URL (also served at `/mobile-preview`).
`--explorer-alpha` opens the deep link in the desktop Explorer and `--web` opens the hosted web
explorer in a browser (both off under `--ci` or `--no-client`, `--web` also under `--no-browser`).
`--dclenv zone` points the deep link's `dclenv` and the web explorer at decentraland.zone; `--realm`
names another realm in the desktop deep links; `--position X,Y` picks the parcel the join links land
on; `--hub`, `-n` and `--landscape-terrain-enabled` add the matching `=true` deep-link knobs. The
upstream flags `--skip-auth-screen`, `--local-scene`, `--debug`, `--no-debug`/`-d` and `--web3` are
accepted and do nothing (the first two are already on every deep link).

### Addresses

The preview answers on every interface and describes itself (`/about`, content and comms URLs, join
links, QR code) as the address each request arrived at, so one running `start` serves all of these:

| Reached as | What to do | Notes |
| --- | --- | --- |
| `http://127.0.0.1:8000`, `http://localhost:8000` | nothing | the only addresses that get the write token by page load, and a secure context for the web explorer |
| `http://<LAN address>:8000` | nothing; `--mobile` prints the QR code | desktop and mobile clients join; the hosted web explorer cannot (below) |
| `https://<your domain>` behind a reverse proxy | `--public-url https://<your domain>` (or `DCL_SCENE_CLI_PUBLIC_URL`) | prints that address's join links; voice needs `--livekit-url wss://…` |
| a public URL with no proxy of your own | `--tunnel` ([Share it](#share-it)) | voice needs `--livekit-url` |

A proxy that sends `X-Forwarded-Proto`, `X-Forwarded-Host` and, when it serves the preview under a
path, `X-Forwarded-Prefix` (the prefix stripped before forwarding) needs no flag. `--public-url`
covers the proxy that sends none of them: a request whose `Host` is that address is answered as that
origin, scheme and path prefix included. Websockets (`/`, `/mini-comms/*`, `/data-layer`) must be
proxied as upgrades.

```
preview.example.com {
    reverse_proxy 127.0.0.1:8000
}
```

```
dcl-scene-cli start --public-url https://preview.example.com
```

A browser lets a page served over https fetch plain http from loopback only. The hosted web explorer
is https, so it plays a preview at `127.0.0.1`/`localhost` or one served over https, and is blocked
on `http://<LAN address>`: the launch page says so in place of the hint and `/play` answers 409 with
the same steps. Editing routes still need the token off loopback: open a link `start` printed, which
carries `#token=`.

### Problems

Every failure is printed as what failed, why, and one or more `→ try:` steps; `--verbose` adds the
full cause chain. An error with no steps of its own still names its root cause and, for the common
ones (refused or dropped connections, DNS, timeouts, TLS, a taken port, permissions, a full disk, a
missing path), the step for it. Exit codes: 1 for a failed command, 2 for a usage error.

A running `start` survives a failed rebuild, type errors and errors the scene throws: each prints as
a `warning:` and stays open until it stops being true (the next clean rebuild, the next passing type
check, the reload that replaces the throwing code). The open ones are on every preview page, above
the content, and at `GET /api/problems`:

```json
{ "revision": 3,
  "problems": [ { "id": 2, "topic": "types", "at": 1794563175854,
                  "what": "type check failed — 1 error",
                  "why": "src/index.ts(1,32): error TS2322: …",
                  "steps": ["fix the type errors above …"] } ] }
```

`topic` is `build`, `types`, `scene` or `preview`; `revision` moves when the list does. Scene errors
need a client with its MCP port on (the default deep link).

### Play

A build with `PLAY_EMBED_DIR` serves the web explorer at `http://127.0.0.1:8000/play/`
(cross-origin isolated, CSP from `engine/web/csp-policy.mts`; secure context only); otherwise `/play/`
redirects to the hosted web explorer (`DCL_SCENE_CLI_WEB_EXPLORER`) with this preview as its realm.
`--talk` opens it (else `DCL_SCENE_CLI_SITE`, `--player` or `DCL_SCENE_CLI_WEB_EXPLORER`) with
the Scene assistant and prints a `Talk:` URL.

- Cargo: `scripts/compose-play-embed.sh`, then
  `PLAY_EMBED_DIR=$ONE/engine/deploy/web cargo build -p dcl-scene-cli`.
- Nix: `dcl-scene-cli.override { playEmbed = self.packages.x86_64-linux.engine-web-embedded; }`.

### Token

Each `start` mints a capability token. Writes (`/api/project/{file,asset}`, assistant, debug
commands, `/scene-json`, `/scene-thumbnail`, `/data-layer?token=`) need `Authorization: Bearer
<token>`, else 401. Pages get it only by `<meta>` on a top-level loopback navigation or a `#token=`
fragment.

On **Scene**, use Left/Right, Home or End to switch editor tabs. In **Shape**, arrow keys
move between parcels; Space or Enter toggles a parcel. **Add spawn area** opens numeric
coordinates as an alternative to drawing. Save or Cancel finishes the spawn draft.
Errors stay visible until dismissed; successful saves are announced briefly.

### Voice and multiplayer

The embedded LiveKit uses 7880-7882; `--no-livekit` and `--tunnel` use the ws-room (no audio). An
external server takes the `--livekit-*` flags or `LIVEKIT_URL`, `LIVEKIT_API_KEY`,
`LIVEKIT_API_SECRET`. Unity needs `--accept-untrusted-realm` and
`--gatekeeper-url http://127.0.0.1:8000/get-scene-adapter`. macOS cargo builds embed no server.

```
dcl-scene-cli host [--dir D] [--preview http://127.0.0.1:8000] [--room room-1]
```

With `"authoritativeMultiplayer": true`, `start` also runs the bundle headless as the server (voice
off); `--no-host` skips it; `host` runs only the server.

### Storage

```
dcl-scene-cli storage scene|player|env get|set|delete|list|clear [KEY]
                    [--value V] [--address 0x…] [--prefix P] [--limit N] [--offset N]
                    [--confirm] [--json] [--dir D] [--target local|preview|org|zone|URL]
                    [--preview http://127.0.0.1:8000] [--sign-key FILE]
dcl-scene-cli storage target [local|org|zone|URL] [--dir D]
dcl-scene-cli storage export [--out FILE] | import FILE [--merge]
```

The preview serves the storage service's routes from `.dcl-one/storage.sqlite` or forwards them,
signed, upstream; a bare command acts where the preview's **Storage** tab points.

Browser imports merge by default. Unchecking **Merge with existing values** requires
confirmation before replacing every local scope. JSON values are parsed; quote a string
such as `"true"` to keep it as text. Environment values are always text. Use Previous/Next
to browse larger listings.

## Share it

```
HTTP_SERVER_HOST=0.0.0.0 HTTP_SERVER_PORT=9000 TUNNEL_TOKENS=SECRET dcl-preview-tunnel  # public host
dcl-scene-cli start --tunnel wss://tunnel.example:9000 --tunnel-token SECRET            # your machine
dcl-scene-cli sync [--dir D] [--node URL] [--project ID] [--sign-key FILE] [--prefer local|remote] [--dry-run]
```

The relay is configured by environment (`dcl-preview-tunnel --help`). It refuses to start without `TUNNEL_TOKENS` unless `TUNNEL_ALLOW_ANONYMOUS=true` (a local relay only). A trunk closed with 4401 had a wrong or missing token; 4409 means no free tunnel id or the relay is at `TUNNEL_MAX_AGENTS`.

Prefer `--tunnel-token-file` or `DCL_SCENE_CLI_TUNNEL_TOKEN`. `sync` is a two-way content-hash sync
with a catalyrst builder project (`.dcl-one/sync.json`).

## Publish it

```
dcl-scene-cli deploy [--dir D] [-t|--target-server CATALYST-DOMAIN-OR-CONTENT-URL]
                     [--sign-key FILE] [--skip-build] [--skip-validations] [--force-upload]
                     [--dry-run] [--timestamp MS] [-y|--yes] [-b|--no-browser] [--ci]
                     [--programmatic] [--deploy-token TOKEN]
dcl-scene-cli export-static [--dir D] [--destination DIR] [--timestamp MS|RFC3339] [--skip-build]
                            [--realmName NAME --baseUrl URL [--commsAdapter URL]]
dcl-scene-cli world access show|edit <name>
dcl-scene-cli world access set    <name> --public | --password S | --user U... --community C...
dcl-scene-cli world access add|remove <name> --user U | --community C
```

`deploy` signs in a browser on the preview server, or headlessly with `--sign-key FILE` or
`DCL_PRIVATE_KEY` (the flag wins; every signing command, `unpublish`, `sync`, `storage` and the
`world` writes included, takes both). Worlds go to the worlds server, parcels to the first healthy
public catalyst (`--target-server` / `DCL_SCENE_CLI_TARGET_SERVER` override); only content missing
from `/available-content` is uploaded (`--force-upload` or `DCL_SCENE_CLI_UPLOAD_ALL=1` sends all).
Before signing, scene.json's media permissions and the 50 MB per-file limit are checked;
`--skip-validations` skips both, as a custom `--target-server` does implicitly (its limits are its
own). `--skip-version-checks`, `--https` and `--multi-scene` are accepted for upstream parity: there
is no SDK version check, the signing page is served on loopback (a secure context already), and
deploying beside a world's other scenes is the default (`--replace-world-scenes` is the opt-out).
Also `unpublish` and `pack`.

### Driving a deploy from a program

`deploy --programmatic` is the run the Creator Hub (or any script) drives: no browser, no
narration, and stdout carries two JSON lines. The first, `{"event":"ready",…}`, arrives when the
signing server is up and names its `url` (the signing page, `#token=` appended), the `api`
resource, the `deployToken`, the `entityId` to sign and, for a world overwrite, the
`deleteScenesFromWorldPayload` to sign too. The last, `{"event":"state",…}`, is the final
deployment state, after which the process exits (non-zero unless `phase` is `deployed`).
`--deploy-token` fixes the token (at least 16 characters); by default it is random per run.
With a signing key the run never starts a server and prints only the final state line.

The resource is upstream's `/api/deploy` contract:

- `GET /api/deploy` answers the state: `{phase, upload, result, error, cancelRequested}` with
  `phase` one of `awaiting-signature`, `undeploying`, `uploading`, `deployed`, `failed`,
  `cancelled`; `upload` is `{stage, sentBytes, totalBytes, completedBatches, totalBatches}` while
  bytes move (`stage` `checking`, `uploading` or `processing`), `result` is `{entityId, message}`
  once deployed, `error` is `{code, message}` with `DEPLOY_FAILED` or
  `DEPLOY_CANCELLED_AFTER_UNDEPLOY` (a plain cancel has no `error`). With
  `Accept: text/event-stream` every change is a `state` event (a `: ping` comment every 15 s) and
  the stream ends after the final state, so close the EventSource then.
- `POST /api/deploy` takes the linker's `{address, authChain, chainId?, deleteSignature?}` (the
  entity id is the last link's payload) and signs the run: with `Prefer: respond-async` it answers
  `202` at once (`Location: /api/deploy`, `Preference-Applied: respond-async`, the state as body)
  and the run is followed through `GET`; without it the answer is the final state, or a `400`
  `{message}` when the upload fails. A body that is not JSON, misses fields or signs a stale entity
  id is `400` before anything runs; a run that already started or ended is `409` with its state,
  so of two posts at once one gets the `202`. The run is not tied to the request: when the client
  goes away the deploy still runs to its end, and `GET` tells the result.
  Only this machine's browser may post (the same-origin gate of the signing page).
- `DELETE /api/deploy` cancels with the token in `X-Deploy-Token` (`403` without it): `202` with the
  state when the run is cancelled (before the signature) or cancelling (the upload is dropped unless
  every byte is already out, when the server's answer is the truth); `409` with the state once the
  run ended or while a world's previous scenes are being removed, after which the result is
  `DEPLOY_CANCELLED_AFTER_UNDEPLOY`.
- `GET /api/info` is what the signing page shows (`baseParcel`, `parcels`, `rootCID`, `title`,
  `description`, `isWorld`, `world`, `deleteScenesFromWorldPayload`, `targetContent`, `multiScene`,
  `skipValidations`, `isPortableExperience`; `landRegistry`, `estateRegistry` and `debug` are null
  and false, there is no chain-side check here); `GET /api/files` lists `[{name, size}]`.
- The three `GET` routes answer this machine only: `403` to another peer, to the tunnel, and to a
  request whose `Host` is not a loopback name. `DCL_SCENE_CLI_LINKER_HOST` other than `127.0.0.1`
  (or `start --allow-remote-deploy`) opens them with the rest.

Differences from upstream: there is no `/api/catalyst-pointers` (the target is already resolved
when the server starts), `chainId` is ignored, and the same routes serve the preview's own signing
page too, so a run started by hand can be cancelled from that page's button when its URL carries
the token.

`export-static` writes the scene (or every workspace member) as a content server stores it: each
publishable file under its hash, the entity file under its id (no pointers), and prints each scene's
`urn:decentraland:entity:<id>` (`?=&baseUrl=<URL>/` appended with `--baseUrl`). `--realmName` also
writes `<destination>/<NAME>/about`, a realm whose `scenesUrn` is the export and whose comms adapter
is `--commsAdapter` (default `offline:offline`); serve the folder at `--baseUrl` and an explorer
joins it by `realm=<baseUrl>/<NAME>`. `--destination` defaults to the current folder and is refused
when it is the project folder or a folder above it; a folder inside the project (`--destination out`)
is fine, and its files stay out of the entity.

## Reference

Ports: 8000 preview, 5147 abgen, 7880-7882 LiveKit. Scene files (never deployed): `bin/`,
`.dcl-one/`, `.dcl-cache/`, `.dcl-optimized-assets/`.

Environment: `DCL_SCENE_CLI_CATALYST` (default `https://interconnected.online`),
`DCL_SCENE_CLI_CATALYST_ROTATION`, `DCL_SCENE_CLI_WORLD_BASE`, `DCL_SCENE_CLI_SOCIAL_API`,
`DCL_SCENE_CLI_FEATURE_FLAGS`, `DCL_SCENE_CLI_CONTENT_CACHE_MAX`, `DCL_SCENE_CLI_WEB_EXPLORER`,
`DCL_SCENE_CLI_ALLOWED_ORIGINS`, `DCL_SCENE_CLI_PUBLIC_URL`, `DCL_SCENE_CLI_LINKER_HOST`,
`DCL_PRIVATE_KEY`, `ABGEN_BIN`; build time `PLAY_EMBED_DIR` and `DCL_SCENE_CLI_SITE` (the
operator's site origin, no default).

`--json` anywhere on the line prints the command's result as one JSON document on stdout and
nothing else there: `export-static` gives `{scenesUrn, destination, entities}` as upstream does,
`deploy` gives `{entityId, message}`; the other commands have no result document. Errors, and the
`Continue? (y/N)` question of a deploy, go to stderr.

`--ci` on `start`, `deploy` and the `world` writes, or `CI` set in the environment, makes the run
non-interactive: no colours, spinners, re-floated banners, QR codes, prompts or browser windows
(`init` takes the default project kind, `deploy` refuses an implicit public target without `--yes`).

Tests: `cargo test -p dcl-scene-cli` needs a toolchain and `node`; gated suites are in
docs/testing.md.

### Creator Hub integration

```sh
DCL_SCENE_CLI_ALLOWED_ORIGINS=https://example.org dcl-scene-cli start --dir ./my-scene
```

**Edit in Creator Hub** opens `<site>/create?projectUrl=…#token=…` (`DCL_SCENE_CLI_CREATOR_HUB_URL`
overrides). Loopback routes: `/api/project`, `/api/project/{files,file,assets,asset}`
(revision-checked writes), `/api/project/assistant/{providers,turn,conversations,bridge}`,
`/api/project/debug[/sessions|/events|/command]`, `/scene-inspector`.

The Scene assistant runs a local `codex`, `claude`, `cursor-agent`, `gemini`, `opencode` or `hermes`;
extra setups `{id, label, engine, command, model?, args?}` go in
`$XDG_CONFIG_HOME/dcl-scene-cli/assistants.json` (or `DCL_SCENE_CLI_ASSISTANTS`). Scene tools need a
separately started `dcl-scene-mcp --http` and `DCL_SCENE_CLI_SCENE_MCP_URL`, `DCL_SCENE_MCP_BEARER`,
`DCL_SCENE_MCP_TOKEN`.

UI Designer: install `@dcl/inspector` (verified 7.46.1), run
`bash scripts/install-ui-designer-runtime.sh /path/to/@dcl/inspector`, set `DCL_ONE_INSPECTOR_DIR` and
start with `--data-layer`.
