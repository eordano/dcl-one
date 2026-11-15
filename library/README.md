# `library/`

Shared code ([rules](../docs/adr/2026-10-19-organizing-library-services-folders.md)).
The one exception is `ui/apps/<app>/`, which holds UI used by a single app
([shared UI library ADR](../docs/adr/2026-10-30-shared-ui-library.md)).

| Path | What | Used by |
| --- | --- | --- |
| `dcl-auth/` | Hashes, signatures, auth chains, signed fetch, wallets | catalyst, scene CLI |
| `dcl-http/` | API errors, capped bodies, JSON-RPC, outbound client | catalyst, dcl-auth |
| `dcl-livekit-token/` | Offline LiveKit tokens | comms, SDK preview |
| `dcl-scene-authoring/` | Composites, CRDT encoding | scene CLI, scene build |
| `dcl-scene-build/` | SDK7 scene build, native and WASI | sites, scene CLI tests |
| `dcl-preview-tunnel{,-protocol}/` | Preview relay and wire types | scene CLI |
| `async/` | Capped exponential backoff, bounded condition waits and HTTP probes | Shop parity/trade tooling, Redis conformance tooling |
| `catalyst-api/` | TypeScript DTOs, OpenAPI documents | sites, ui3, catalyst |
| `shop/` | The Shop's portable core: model, money, prices, sale and payment rules, cart partitioning, contract ABIs and endpoint map; plain TypeScript with no host access ([ADR](../docs/adr/2026-11-09-shop-portable-core.md)) | ui3 (web Shop, `/play` HUD), sites; planned: the Explorer React HUD |
| `protocol/` | The `.proto` contract, the npm package `@dcl/protocol` | Explorer (`dcl_component`), comms, social, scene CLI, scene authoring, the SDK |
| `hashing/` | The npm package `@dcl/hashing`: `hashV0`, `hashV1`, the ADR-32 and ADR-62 hashes; `@noble/hashes` is its only dependency ([ADR](../docs/adr/2026-11-13-hashing-in-dcl-one.md)) | the SDK (`sdk-commands`) |
| `js-sdk-toolchain/` | The creator SDK: `@dcl/sdk`, `ecs`, `react-ecs`, `js-runtime`, `sdk-commands`, `playground-assets` (npm, own lockfiles) | scenes; reads `protocol/` and `hashing/` |
| `explorer-core/` | The Explorer page's core both HUD hosts share: page-engine protocol and generated bindings, drivers, sign-in, shell messaging, analytics, server reads and the host contract; no React ([ADR](../docs/adr/2026-11-10-shared-explorer-hud-ui.md)) | react-web (the Explorer HUD and its bridge scene), ui3 `/play` HUD, sites |
| `ui/` | Shared UI foundations, components, app-specific UI (`apps/<app>/`) and Figma tooling ([standard](../docs/ui-standards.md#shared-ui-libraryui)); the Explorer HUD UI its two hosts share is `apps/hud/`, built on `explorer-core/` ([ADR](../docs/adr/2026-11-10-shared-explorer-hud-ui.md)) | Explorer React HUD (foundations, HUD UI), ui3 `/play` HUD and sites (HUD UI); planned: web apps under `apps/` |
| `vendor/rusty_enet/` | Pinned ENet-CSharp transport source, MIT license and reviewed local patches | Catalyst Pulse, native Bevy Pulse client |
| `uuav/` | Native video player: the client crate the Explorer links, the sandboxed `uuav-helper` (FFmpeg 8.1, hardware decode with software fallback) and the locked runtime set under `dist/` ([ADR](../docs/adr/2026-11-03-uuav-native-video.md)) | Explorer (integration to follow) |
