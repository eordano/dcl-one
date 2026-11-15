# dcl-scene-cli

One Rust binary that scaffolds, builds, previews and publishes Decentraland SDK7 scenes, replacing
`@dcl/sdk-commands` 7.27.0 ([js-sdk-toolchain](https://github.com/decentraland/js-sdk-toolchain)) with
the same commands and flags. rolldown, the TypeScript toolchain, abgen and `livekit-server` are in the
binary; nothing to `npm install`. The vendored runtime pins `@dcl/sdk` 7.29.1-34986384248.commit-bb45080
(src/vendor/README.md).

## Start a scene

```
dcl-scene-cli init [--dir D] [--project scene|smart-wearable|px-template|library] [-y|--yes]
                   [--skip-install] [--node-modules-only [--missing-only [--dry-run]]]
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
way the vendored `node_modules` is installed, and dependencies it lacks are named. A `node_modules`
folder at the top of a template is never unpacked, so a template supplies no package. The `/inspector/*` UI needs
`npm install --save-dev @dcl/inspector` or `DCL_ONE_INSPECTOR_DIR`. `get-context-files` writes the
`migrate-smart-items-to-code` skill and downloads `ai-sdk-context` into `dclcontext/`.

`init --node-modules-only` writes no project file. It installs the vendored `node_modules` into an
existing project. An install writes the SHA-256 of the vendored archive to
`node_modules/.dcl-one-archive`; the record is empty while the install runs. When `node_modules`
is there, the record decides what the command does:

- No record: another tool (npm, pnpm) installed the folder. The command changes nothing and prints
  that the folder is kept. One case is different: the installed `@dcl/inspector` has a version that
  ends with `-dcl-one-sdk-shim`. That is the vendored stand-in, which only this CLI installs, so a
  CLI that wrote no record installed the folder. The command then replaces the eleven vendored
  packages.
- The record names the archive of this CLI and the installed `@dcl/sdk` has the vendored version:
  the command does nothing.
- Each other record, or another `@dcl/sdk` with a record: the command replaces the eleven vendored
  packages with the vendored versions, whatever is there.

The eleven packages are `@dcl/ecs`, `@dcl/inspector`, `@dcl/js-runtime`, `@dcl/rpc`, `@dcl/sdk`,
`@protobufjs/utf8`, `long`, `mitt`, `protobufjs`, `typescript` and `ws`. The line that the command
prints names them. Each other entry of `node_modules` stays. The vendored `@dcl/inspector` has no
editor UI and the vendored `protobufjs` is a minimal build. Thus, if a package manager wrote the
full packages into a folder that has a record, install them again after a replacement. A package
that is a link is replaced, the link itself. A `@<scope>` folder that is a link stops the command
with exit code 1 before it writes a file, and the error names the link. So does a `node_modules`
or a `node_modules/.dcl-one-archive` that is a link: the command does not write the record through
a link. With `--json` the command
gives the document of `--missing-only` ([Reference](#reference)): `installed` has the packages it
wrote and `kept` is `[]`.

`init --node-modules-only --missing-only` installs only the vendored packages that `node_modules`
does not have, for a scene that has its own `@dcl/sdk`:

- A package is `node_modules/<name>` or `node_modules/@<scope>/<name>`. It is there when its
  `package.json` can be read and gives a `version` text. A package that is there stays as it is,
  whatever its version and its other content: the command writes nothing into it.
- An entry that is not such a package (an empty folder, a folder with no `package.json` or with
  one that gives no `version`, a file) is not kept. The command prints one line on stderr,
  `warning: node_modules/<name> is not a package (…); the vendored package replaces it`, and
  installs the package in its place as it installs an absent package. The entry is removed.
- The command does not replace a link and does not write through one. A link that does not lead
  to such a package, and a `@<scope>` folder that is a link above a package to install, stop the
  command with exit code 1 before it writes a file. The error names the link. A link that leads
  to a package is a package that is there.
- Each added package is complete when it comes into `node_modules`. A run that stops leaves no
  part of a package, and the next run adds the packages that are still absent. The command
  prepares the packages in `node_modules/.dcl-one-missing` and removes that folder at its end. A
  run also removes the folder that a stopped run left, with or without a package to install.
- For each kept package whose `package.json` version is not the vendored version, the command
  prints one line on stderr:
  `warning: node_modules keeps @dcl/sdk 7.0.0; the vendored SDK has @dcl/sdk <version>`.
- The command does not write, change or remove `node_modules/.dcl-one-archive`. Thus a build does
  not change the folder later.
- An empty `node_modules/.dcl-one-archive` shows an install of the vendored packages that
  stopped: a vendored package in the folder can lack files. The command then keeps no vendored
  package: it installs each one again, a package of another version included. It prints one
  warning on stderr and gives `stoppedExtraction: true` in the document. The record stays empty,
  so the next build or `init --node-modules-only` does the full install.
- A second run installs nothing. With `--json` the command gives the result as a document
  ([Reference](#reference)).
- With `--dry-run` the command reads the same and writes nothing: no package, no folder. The
  warnings, the errors, the exit code and the lists of the document are those of the run that
  writes.

A parent program gets "which SDK packages are outdated" and "update them" from these two commands,
with no npm:

1. `init --node-modules-only --missing-only --dry-run --json` changes no file. Each package of
   `kept` whose `version` is not its `archiveVersion` has another version than the one this CLI
   installs. The command compares the two texts and no more: the version in `node_modules` can
   be older or newer.
2. `init --node-modules-only` replaces the eleven vendored packages, each with the version of
   this CLI, and keeps all other packages. A second run of step 1 then gives `version` equal to
   `archiveVersion` for each. This step obeys the rules of the record above. A folder with no
   record gets the replacement only when its `@dcl/inspector` is the vendored stand-in, which a
   `--missing-only` run installs when the package is absent. The command keeps a folder with no
   record and another `@dcl/inspector`, or none: `installed` is then `[]`. To get the vendored
   packages there, remove `node_modules` first.

The CLI installs only the versions that it carries, all its packages together. It does not ask a
registry for a later version and it does not install one package alone: a later SDK comes with a
later `dcl-scene-cli`.

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
`@dcl/sdk`; `--skip-install` turns that off. An existing `node_modules` is never touched, except one
whose install of the vendored SDK stopped half way (an empty `node_modules/.dcl-one-archive`): the
build then replaces the eleven vendored packages, as `init --node-modules-only` does.

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

### Code to composite

```
dcl-scene-cli code-to-composite [--dir D] [-y|--yes]
```

Turns a code-driven scene into one the visual editor edits. The scene is built without its
composite and run once under node against a mock renderer; the entities its `main()` creates are
captured on an editor engine (react-ecs UI entities and components the editor does not know are
dropped). Referenced models, images, audio and video are copied under `assets/scene/Models/<name>/`,
`Images/`, `Audio/` and `Video/` and the component paths are rewritten. When two files have the same
name, the later one in path order gets its parent folders in front (`images/b/logo.png` to
`Images/b-logo.png`); two with the same content share one copy, and a file already there with other
content is kept. A destination that is a link, or is in a folder that links out of the scene, stops
the command. Unnamed entities get a `Name` from their GltfContainer, MeshRenderer, TextShape,
VideoPlayer, NftShape, LightSource, AudioSource or MeshCollider, and the result is written as
`assets/scene/main.composite` and `main.crdt`. Every `src/**/*.{ts,tsx,js,jsx}` is then commented
out line by line under upstream's marker (plus an `export {}` so it stays a module) and the scene is
rebuilt from the composite, which also writes `assets/scene/entity-names.ts`. An existing
`main.crdt` is part of the capture, so a converted scene round-trips. Only what the scene sends its
renderer is captured: `Name`, `Tags` and the sync components set in code do not cross that transport
(as upstream), which is why names are generated. The command asks for a confirmation on a terminal;
with no terminal, or under `--ci`, it stops unless `--yes` is given.

The scene's code runs with node's full privileges: it can read and write whatever your user can.
Convert a scene you did not write only if you would run its code. The run has no time limit: it
ends when `main()` and the first frame return, and a timer or a socket that the scene leaves open
does not keep the command. `--verbose` shows what the scene printed through `console`. If the
scene fails under node, `src/` is unchanged and files already copied to `assets/scene/` stay. The
command reads each file under `src/` before it changes one; if it cannot write a file, the files
that it changed get their content back. If the scene does not build from its composite, `src/` is
restored and the composite stays; a warning names each file that could not be restored. A
referenced file or glTF dependency that is not a file on disk is reported and its path left as it
is. A model whose glTF `images` or `buffers` is not a list of objects is copied alone: its
dependencies cannot be read. The run loads `@dcl/inspector` and `@dcl/ecs` from the scene's
`node_modules`; the vendored ones are enough.

## Preview it

```
dcl-scene-cli start [--dir D] [-p|--port N] [--skip-build] [--no-watch] [-m|--mobile]
                    [--explorer-alpha] [--web] [--dclenv org|zone] [--realm URL] [--position X,Y]
                    [--hub] [-n|--open-deeplink-in-new-instance] [--landscape-terrain-enabled]
                    [--skip-auth-screen] [--multi-instance]
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
upstream flags `--local-scene`, `--debug`, `--no-debug`/`-d` and `--web3` are accepted and do
nothing (`local-scene=true` is already on every deep link).

`--skip-auth-screen` and `--multi-instance` select which of `skip-auth-screen=true` and
`multi-instance=true` the desktop deep link has. With neither flag the link ends with
`&multi-instance=true&skip-auth-screen=true`. With one flag, or the two, the link has only the
parameters of the given flags: `start --hub --skip-auth-screen` prints a link that ends with
`&hub=true&skip-auth-screen=true`, and `start --hub --multi-instance` one that ends with
`&hub=true&multi-instance=true`. There is no flag form for a link with neither parameter. The first
visit of the launch page checks the same parameters. The `desktop (2nd instance)` row of
`--verbose` has `multi-instance=true` in all cases.

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
missing path), the step for it. Exit codes: 1 for a failed command, 2 for a usage error. The log
lines of `--verbose` and `RUST_LOG` go to stderr.

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
fragment. With `--data-layer`, the editor's host listens on a loopback port of its own and admits
only the relay of `/data-layer`, which carries a secret made for each launch of the host: an upgrade
without it, or with an `Origin`, gets 403. The host gets the secret in its environment
(`DCL_SCENE_CLI_DATA_LAYER_SECRET`), never in its arguments: a program of the same user that can
read the environment of the host process can read the secret. The file system that the host gives
the editor refuses each path that leaves the scene folder, symbolic links followed, and works on the
path that it checked. The check and the call are two steps: a second local program that puts a link
in that path between them is not stopped.

On **Scene**, use Left/Right, Home or End to switch editor tabs. In **Shape**, arrow keys
move between parcels; Space or Enter toggles a parcel. **Add spawn area** opens numeric
coordinates as an alternative to drawing. Save or Cancel finishes the spawn draft.
Errors stay visible until dismissed; successful saves are announced briefly.

### Driving a preview from a program

`start --json` is the run a parent program reads (the Creator Hub, a script): stdout carries one
JSON object on each line and nothing else, each with an `event` field. The text of the terminal
form (the join block, notes, warnings, the output of a custom `scripts.build`) goes to stderr.
Keys are camelCase and paths are absolute.

`ready`, once, when the preview serves and its links are known:

```json
{"event":"ready","port":8000,"url":"http://127.0.0.1:8000","editorUrl":"http://127.0.0.1:8000/inspector/","webUrl":"https://decentraland.org/bevy-web/?preview=true&realm=http://127.0.0.1:8000&position=0,0","deepLink":"decentraland://realm=http%3A%2F%2F127.0.0.1%3A8000&position=0%2C0&local-scene=true&dclenv=org&multi-instance=true&skip-auth-screen=true","dataLayerUrl":"ws://127.0.0.1:8000/data-layer","token":"…","clientsOpened":[]}
```

`deepLink` is the link of the `desktop:` row, with the flags of this run in it (`--skip-auth-screen`
and `--multi-instance` included), and `webUrl` is the
one `--web` opens. `dataLayerUrl` is `null` without `--data-layer`; `editorUrl` is `null` without
`--data-layer` or with no `/inspector/*` UI installed. `clientsOpened` lists the clients (`talk`,
`desktop`, `web`) for which the opener of the system (`xdg-open`, `open` on macOS, `explorer` on
Windows) started: the `desktop:` row prints on every run, so read this field, not the row, to know
whether a client was started. It is `[]` when the run opened nothing, as with `--explorer-alpha
--no-client`. A started opener does not say that the client is installed or came up.

`client-failed`, after `ready`, for each client whose opener did not start or ended with an exit
status that is not 0:

```json
{"event":"client-failed","client":"desktop","message":"xdg-open ended with exit status: 3"}
```

When the opener did not start (`xdg-open did not start: …`), the line comes directly after
`ready`, the client is not in `clientsOpened`, and the terminal note names the client (`could not
open the desktop client automatically — open the URL above manually`). When the opener started,
the client is in `clientsOpened` and the line comes when the opener ends, with no limit on the
wait. An opener that stays until its application exits gives the status of that application, so a
late `client-failed` can come from the application and not from the launch. An opener that a
signal ends gives no line. On Linux and macOS the opener runs in a process group of its own, so a
Ctrl-C in the terminal of the CLI does not go to it. On Windows the status of `explorer` is not
read.

`token` is the capability token of [Token](#token), the one `/data-layer?token=` and the project
writes ask for. It is on stdout for the parent process, which already owns the project folder; any
program that can read that pipe can write to the project through this preview while it runs. Do
not copy these lines into a log that others read. The token ends with the process.

With `--data-layer`, a parent opens `dataLayerUrl` with `?token=<token>` added (or the
`Authorization` header) and speaks `@dcl/rpc` to the `DataService` module; the host listens before
`ready` prints. The socket admits a request with no `Origin` (a program), a request from a page
of the preview, and a request with `Origin: null`, which a browser sends from a sandboxed frame
and from a `data:` document. A page on another origin that opens the socket for the parent, such
as the Creator Hub editor page, gets 403 unless the parent names its origin in
`DCL_SCENE_CLI_ALLOWED_ORIGINS` when it starts the CLI. The origin check does not stop a page
that sends `Origin: null`: the token is the gate for such a page, as it is for a program. The
host needs `node` on `PATH`.

`rebuild`, for each rebuild that wrote the scene's output while the preview runs:

```json
{"event":"rebuild","output":"/scenes/my-scene/bin/index.js","changed":["/scenes/my-scene/src/index.ts"],"durationMs":41}
```

`output` is the absolute path of the `main` file of scene.json, for the built-in bundler and for a
`scripts.build` alike; the chunks the built-in bundler writes beside it are not named. `changed` is
the saved files that started the rebuild, sorted, each once. A first build that failed and then
passes after a save is a `rebuild` too, with the same `output`. A save that changes a model alone
reloads that model and prints no event.

`build-error`, for each build that failed, the first build included (it can come before `ready`):

```json
{"event":"build-error","message":"package.json scripts.build failed (exit status: 1)","detail":"the SDK ran no build of its own, and a preview keeps serving the last good bundle","steps":["fix what the build script reported above; its output is the whole story","run it on its own to iterate faster: npm run build"]}
```

`message`, `detail` (or `null`) and `steps` are the what, why and `→ try:` steps of
[Problems](#problems); the same failure stays at `GET /api/problems` until a rebuild passes. Type
check failures print no event: read them there (`topic` `types`).

A first build that fails does not end a run that watches the project: the events are `build-error`,
then `ready` (the server runs), and a later save that builds gives `rebuild`. With `--no-watch` the
same failure ends the run with `error`.

`error`, once, when the run fails and exits with code 1:

```json
{"event":"error","message":"port 8000 is already in use","code":"port-in-use"}
```

`message` is the first line of the error text, without colour. The full text stays on stderr, as
without `--json`. `code` is a stable word for a cause the CLI knows, and `null` for all other
causes:

| `code` | Cause |
| --- | --- |
| `port-in-use` | A different process listens on the port of `--port`. |
| `no-scene` | The folder has no `scene.json`. |
| `no-directory` | The folder of `--dir` does not exist. |

A stop that you request (Ctrl-C, exit code 0) prints no event. A command line that the CLI cannot
parse prints no event: the exit code is 2 and the usage text is on stderr.

`deploy --json` and `deploy --programmatic` print the same `error` event when the run fails
([Driving a deploy from a program](#driving-a-deploy-from-a-program)).

The token is in the `ready` event only. With `--json`, the `Talk:` line of `--talk` on stderr shows
`http://127.0.0.1:<port>/talk`, which has no token: that address sends a browser on this machine to
the player with the token. Without `--json`, the `Talk:` line shows the player URL with the token.

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
                     [-p|--port N] [--host ADDRESS] [--language CODE]
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
own). `--skip-version-checks`, `--https`, `--multi-scene` and `--language CODE` are accepted for
upstream parity: there is no SDK version check, the signing page is served on loopback (a secure
context already), deploying beside a world's other scenes is the default (`--replace-world-scenes`
is the opt-out), and the text of this CLI and of its signing page is English only (the language
code changes nothing).
Also `unpublish` and `pack`.

The signing server of `deploy` listens on `127.0.0.1` only, so no other machine can connect to its
port. `--host ADDRESS` selects another address (an IP address; `DCL_SCENE_CLI_LINKER_HOST` is the
same setting from the environment, and the flag wins). A creator who opens the pages of this
server from another device passes `--host 0.0.0.0`.
`--host` does not change who can sign: the signing routes answer this machine only, through a
loopback address. Remote signing, where they answer other machines too, is on when
`DCL_SCENE_CLI_LINKER_HOST` is an address that is not loopback and the server does not listen on a
loopback address. The CLI reads the flag and the variable once, before the build: the listener, the
printed URLs and the signing routes follow that one result. While remote signing is off, `--host`
with one address that is not loopback (a LAN address) is a usage error with exit code 2, because
the server would refuse each request to that address: pass `--host 0.0.0.0`, or set the variable to
the address. The printed URLs name `localhost` for `127.0.0.1` and `0.0.0.0`, and the address
itself for another address. Under the signing URL the CLI prints how another device can sign: set
`DCL_SCENE_CLI_LINKER_HOST=0.0.0.0` and run the command again. A run with `--host` on a loopback
address does not print that note, because the flag wins and the variable does not open that run.
With remote signing on, a server on `0.0.0.0` prints the note to replace `localhost` with the
address of this machine. A server on one address prints no note: the URL names that address.
The signing page of the `world` commands reads `DCL_SCENE_CLI_LINKER_HOST` by the same rule: the
value is an IP address, and a value that is not one stops the command. Its URL and its note follow
the same rules. The name `localhost` is the
one exception: `deploy` and the `world` commands read it as `127.0.0.1`. These commands have no
`--host`.

### Driving a deploy from a program

`deploy --programmatic` is the run the Creator Hub (or any script) drives: no browser, no
narration, and stdout carries two JSON lines. The first, `{"event":"ready",…}`, arrives when the
signing server is up and names its `url` (the signing page, `#token=` appended), the `api`
resource, the `deployToken`, the `entityId` to sign and, for a world overwrite, the
`deleteScenesFromWorldPayload` to sign too. The last, `{"event":"state",…}`, is the final
deployment state, after which the process exits (non-zero unless `phase` is `deployed`).
`--deploy-token` fixes the token (at least 16 characters); by default it is random per run.
With a signing key the run never starts a server and prints only the final state line.

A run that fails prints one line more, the `error` event of
[`start --json`](#driving-a-preview-from-a-program), as the last line of stdout before the exit
with code 1:

```json
{"event":"error","message":"port 8000 is already in use","code":"port-in-use"}
```

`message` is the first line of the error text and `code` is `port-in-use` (the port of
`deploy --port`), `no-scene`, `no-directory` or `null`, with the same meaning as for `start`. The
failure can come before the server starts (no scene, a taken port, a refused token) or during the
deploy (the upload). The full text stays on stderr, and its `→ try:` step for a taken port names
`deploy --port`. The line has no token in it.

- `deploy --programmatic`: the line follows the final `state` line, which has `phase` `failed`
  and the full text in `error.message`. Read the final state from the last `state` line, not from
  the last line of stdout.
- `deploy --json` without `--programmatic`: the line is the only line of stdout. A deploy that
  passes prints the result document and no event.

There is no `error` event for a stop that the caller requests: a cancel through
`DELETE /api/deploy` (the `state` line says `cancelled`), or SIGINT and SIGTERM (the exit code is
then 1 or 130, and a programmatic run that waited for the signature prints its `state` line). A
command line that the CLI cannot parse prints no event and exits with code 2, and so does a
`--host` that needs remote signing. A run without `--json` and without `--programmatic` prints no
event.

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
- `GET /api/catalyst-pointers` answers `{"catalysts":[{"timestamp":…,"entityId":"…","url":"…"}]}`:
  the entity that each server has at the base parcel of the scene (`POST <content>/entities/active`
  with that parcel). The list is the content server of the run; when that server is a member of
  the catalyst rotation, the list is each member, `url` its catalyst address. A server with no
  entity there, or one that answers with a failure, has `"entityId":""` and no `timestamp`; a
  server that gives no readable answer is not in the list. The route does not end the run.
- The four `GET` routes answer this machine only: `403` to another peer, to the tunnel, and to a
  request whose `Host` is not a loopback name. Remote signing (`DCL_SCENE_CLI_LINKER_HOST`, as
  above, or `start --allow-remote-deploy`) opens them with the rest. `GET /api/catalyst-pointers`
  makes this machine ask other servers, so it also has the same-origin gate of the signing page:
  `403` to a page of another origin, with remote signing too.

Differences from upstream: `/api/catalyst-pointers` asks the servers of this run, not the catalyst
list of a chain, `chainId` is ignored, and the same routes serve the preview's own signing
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
nothing else there; the other commands have no result document. Errors, and the
`Continue? (y/N)` question of a deploy, go to stderr; the text that the command prints without
`--json` is not printed, and the output of a custom `scripts.build` goes to stderr. `start` prints
JSON lines instead ([Driving a preview from a program](#driving-a-preview-from-a-program)).

- `export-static` gives `{scenesUrn, destination, entities}` as upstream does, and three keys
  more: `entityFiles`, the path of each entity file (`<destination>/<entity id>`) in the order of
  `entities`; `contentFiles`, one list for each entity in the same order, with the content files
  that the entity names, in the order of its `content` (`file` is the path in the scene, `hash`
  is the content hash); and
  `aboutFile`, the realm file `--realmName` wrote, else `null`. Each content file is at
  `<destination>/<hash>`, beside the entity files. The list has each file of the scene, also
  a file that an earlier export wrote and this run kept. A path that is not UTF-8 is written as
  the CLI shows it, with `�` in the place of each part that is not.

  ```json
  { "scenesUrn": ["urn:decentraland:entity:bafkrei…?=&baseUrl=https://static.example/scenes/"],
    "destination": "/scenes/my-scene/out", "entities": ["bafkrei…"],
    "entityFiles": ["/scenes/my-scene/out/bafkrei…"],
    "contentFiles": [[{ "file": "scene.json", "hash": "bafkrei…" },
                      { "file": "bin/index.js", "hash": "bafkrei…" }]],
    "aboutFile": null }
  ```

- `deploy` gives `{entityId, message}`. A deploy that fails gives no document: stdout holds one
  `error` event ([Driving a deploy from a program](#driving-a-deploy-from-a-program)).
- `init --template` and `init --github-repo` give the project folder and the packages its
  `package.json` names (`dependencies` and `devDependencies`) that the vendored `node_modules`
  does not carry. `missingDependencies` has the names. `missingPackages` has, for each name, the
  `range` that `package.json` asks for: the text of the value (a version range, a tag, a URL or a
  path), or `null` when the value is not text. A package that the two sections name has the
  range of `dependencies`. The two lists are sorted by name; each is `[]` when the vendored
  `node_modules` carries all the packages, and `null` with `--skip-install`, which installs nothing:

  ```json
  { "dir": "/scenes/my-scene", "missingDependencies": ["@dcl-sdk/utils"],
    "missingPackages": [{ "name": "@dcl-sdk/utils", "range": "^1.2.0" }] }
  ```

  A package is missing when `node_modules` has no folder for it; the command does not compare the
  version of a package that is there with the range. The exit code is 0 when the project was
  written, with or without missing packages: installing them is the caller's step. A failed
  download or unpack exits 1 with no document.
- `init --node-modules-only` gives one document, with `--missing-only` and without. With
  `--missing-only` it has the project folder, the packages it installed and the packages it kept
  (the two lists sorted by name). `installed` has each package that `node_modules` did not have;
  an entry that was not a package (no `package.json` with a `version`) is in this list, not in
  `kept`. `version` is the version in the `package.json` of the kept package, always a text, and
  `archiveVersion` is the version that the command installs. `stoppedExtraction` is `true` when
  `node_modules/.dcl-one-archive` is empty: `installed` then has each vendored package, and
  `kept` is `[]`. With `--dry-run`, `dryRun` is `true`, nothing is written, and `installed` has
  the packages that a run without `--dry-run` installs. The warnings stay on stderr:

  ```json
  { "dir": "/scenes/my-scene", "dryRun": false, "installed": ["@dcl/ecs", "typescript"],
    "kept": [{ "name": "@dcl/sdk", "version": "7.0.0", "archiveVersion": "7.29.1" }],
    "stoppedExtraction": false }
  ```

  Without `--missing-only` the document has the same keys. `installed` has each package that the
  command wrote: the eleven vendored packages, or `[]` when the command wrote nothing
  (`node_modules` already had the vendored packages of this CLI, or another tool installed the
  folder and the command kept it). `kept` is `[]`: this mode does not list the packages of a
  folder. `dryRun` is `false`. `stoppedExtraction` is `true` when `node_modules/.dcl-one-archive`
  was empty at the start; the command then does the install again and writes the record.

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
