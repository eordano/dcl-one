# Local development

Needs [Nix with flakes](https://nix.dev/install-nix) on Apple Silicon macOS,
Linux or WSL2 (clone outside `/mnt/c`).

```sh
git clone https://github.com/decentraland/dcl-one.git foundation  # GIT_LFS_SKIP_SMUDGE=1: skip docs media
cd foundation
nix develop                        # FOUNDATION_DEV_AUTOSTART=0: no services
nix develop --command make check   # non-interactive: never starts services
```

`flake.nix` pins nixpkgs to a nixos-unstable commit. The Update nixpkgs workflow
proposes the current tip on the first of each month; `python3
tooling/nixpkgs-update.py` does the same locally.

`nix develop` starts a per-checkout PostgreSQL 18, API, Sites, LiveKit, NATS and
Pulse that outlive the shell and reload on edits.

Ports, from base 5210 (`FOUNDATION_DEV_PORT` on first use; saved in
`.cache/dev/settings.json`): API and `/ws` +0, its metrics +1, Sites and Pulse WebTransport +2,
PostgreSQL +3, LiveKit +4..+6, Pulse ENet and `/metrics` +7, NATS +8, its
monitor +9. An occupied saved port fails startup.

| `foundation-dev …` | Action |
| --- | --- |
| `status` | URLs, health, Pulse SPKI hash |
| `wait --timeout 1800` | Wait for readiness |
| `logs`, `restart <service>` | After lockfile changes restart `web-install`, then `sites` |
| `up`, `down` | Start or attach, stop (`--postgres-only`) |
| `profile mirror\|testnet\|offline` | Switch profile while stopped |
| `service install\|uninstall` | Login service (WSL: `systemd=true`) |
| `trades-snapshot <file>` | Snapshot for `FOUNDATION_DEV_TRADES_SNAPSHOT` |
| `env` | App URLs, `FOUNDATION_DEV_TEST_DATABASE_URL`, `FOUNDATION_DEV_STACK_PROFILE` |

Without this Chrome profile, `/play` logs `pulse: webtransport connect failed`
and shows no other avatars:

```sh
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --user-data-dir="$(mktemp -d)" \
  --webtransport-developer-mode --ignore-certificate-errors-spki-list=<hash from status>
```

`/about` answers 503 while Pulse is down.

## Data profiles

- `mirror` (default) mirrors catalogues hourly and reads content and worlds through on a miss; places, worlds and events answer 503 until their first pass.
- `WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS` trusts the upstream's deployer list; it is not verification.
- Deployments answer 503 except profiles (`SERVE_UNSYNCED_DEPLOY_TYPES=profile`); nothing is written upstream.
- `offline` (`FOUNDATION_DEV_PROFILE=offline`) starts empty for local deployments.
- `testnet` (`FOUNDATION_DEV_PROFILE=testnet`) mirrors the Shop's catalogue and order book from Decentraland's testnet (.zone, Amoy and Sepolia) and settles on Amoy, for [`tooling/shop-trade-journey`](../tooling/shop-trade-journey/README.md). Start it in its own worktree: a stack holding data of another profile refuses to switch to it, and a testnet stack refuses `trades-snapshot`. Credits settle through dcl-one's own CreditsManager on Amoy with mock card top-ups (the Shop refuses the shared upstream one); the stack signs them only when `.cache/dev/credits-signer.key` holds a key with that contract's signer role. Until the relay admits that manager, a credits purchase is a transaction the wallet pays gas for, so in-app wallets can't buy with Credits on this profile.
- Never delete `.cache/dev` to fix a migration failure.
- Keep `settings.json` with the database; without it startup refuses to create credentials.
- Use Linux for exact `en_US.utf8` collation comparisons.

## Production APIs in use

dcl-one still reads these Decentraland-operated services. Each row is temporary until its exit criteria are met. Hosts are `*.decentraland.org` unless a row says otherwise, and the repo records no upstream owner for any of them.

How the host is chosen:
- **Catalyst:** `NODE_POSTURE=mirror` expands every row of `services/catalyst/config/src/posture.rs` (`ROWS`) to its Decentraland host. `island` and `journey` leave them empty. The repo does not record which posture each deployment runs, so a "mirror" row is production traffic only where a deployment runs that posture. The `mirror` dev profile sets the same values through `tooling/dev.py` (`runtime_env()`). The `offline` profile (used by CI's `dev-smoke.sh`) sets none.
- **Sites and ui3:** they reach the node through `CATALYST_URL` or the page origin, and the gateway serves the node's prefixes (`infra/gateway/routes.nix`, `root-aliases.nix`). Only the rows below leave that path.

Status values:
- **production:** deployed code calls the host by default;
- **mirror:** production under the mirror posture, and the dev `mirror` profile;
- **inactive by default:** production code that runs only when the named variable is set;
- **dev-only** and **CI-only:** what the names say.

| Entry | Where | Owner | Exit criteria |
| --- | --- | --- | --- |
| Content peers `peer`, `peer-eu1` (`/content`: `/snapshots`, `/pointer-changes`, `/entities/active`, `/contents/{hash}`). `SYNC_SOURCE` (`DCL_PEERS` in `posture.rs`), `READ_THROUGH_ENABLED`, `SYNC_FROM_DAO`. Status: mirror | `services/catalyst/content/src/sync/`, `content/src/read_through.rs` | `@decentraland/apis` | No target yet: this is how the local content area is filled |
| Chain RPC `rpc` (`/mainnet`, `/sepolia`, `/polygon`, `/amoy`, JSON-RPC). `ETH_RPC_URL`, `RPC_ENDPOINT_POLYGON`. When unset, defaults to `rpc.decentraland.org/<chain>?project=catalyst-content` (`config/src/config.rs`, `default_rpc_urls`) under mirror or no posture. Status: production | `content/src/sync/worker.rs`, `content/src/validator/`, `land-authz/src/`, `shop/src/{evm,mana,market,coupons}/`, `builder/src/handlers/publication.rs`, `src/index.rs` | `@decentraland/apis` | No target yet |
| RPC relay upstreams `rpc` (`RPC_UPSTREAM_{MAINNET,ETHEREUM,SEPOLIA,POLYGON,MATIC,AMOY}`); the OpenSea resolver's RPC legs reuse them. Status: mirror | `src/ops/rpc/routes.rs`, `explore/src/opensea.rs` | `@decentraland/apis` | No target yet |
| Shop checkout RPC `rpc` (`/polygon`, `/mainnet`, `/amoy`, `/sepolia`). `DEFAULT_RPC`, overridden by `window.__SHOP_CHAIN__.rpc` or `VITE_SHOP_RPC_*`; no deployment sets either. The market reader uses `rpc/amoy` only on Amoy. Status: production (browser) | `apps/ui3/src/shop/checkout/chain/config.ts`, `chain/rpc.ts`, `checkout/lib/authorizations.ts`; `apps/ui3/src/shop/market/lib/rpc.ts` | `@decentraland/web-applications` | Reads go through the node's relay (`${catalystBase()}/rpc/<network>`, as `apps/ui3/src/shop/land/evm.ts` already does), served by `services/catalyst/src/ops/rpc/` |
| Feature flags `feature-flags` (`/dapps.json`, Referer `FEATURE_FLAGS_ORIGIN`). Sites: `FEATURE_FLAGS_URL`, default `https://feature-flags.decentraland.org`. Shop browser fallback: `FLAGS_SERVICE`. Catalyst: `FF_URL`, `SHOP_FEATURE_FLAGS_URL` (mirror). Status: production | `apps/sites/src/data/lib/feature-flags.server.ts`, `auth/sign-in-flags.server.ts`; `library/shop/discovery/lib/rolloutFlags.ts`; `services/catalyst/social/src/communities/features.rs`, `shop/src/market/features.rs` | `@decentraland/web-applications`, `@decentraland/apis` | Sites and Shop read the node's `GET /feature-flags/<app>.json` (`services/catalyst/explore/src/api/feature_flags.rs`), which mirrors the document and applies local overrides |
| Explorer download counts `cdn-data` (`/public/current/explorer-alpha-downloads.json`). Hardcoded `COUNTS_URL`. Falls back to "+400K". Status: production | `apps/sites/src/data/lib/catalyst/landings/download-counts.server.ts` | `@decentraland/web-applications` | No target yet |
| Editor-scene realm `peer` (`/content`, `/lambdas`, `/bff`). The static realm JSON advertises them as `publicUrl`. Status: production (`/editor-scene/about`) | `apps/ui3/src/editor/editor-scene-realm.json`, `apps/sites/src/routes/routes/editor-scene.EditorScene.about.ts` | `@decentraland/web-applications` | No target yet. The node serves `/content` and `/lambdas` itself |
| Places `places` (`/api/places`, `/api/worlds`). `PLACES_UPSTREAM_URL`, `WORLDS_UPSTREAM_URL`. Status: mirror | `services/catalyst/explore/src/places/mirror.rs` | `@decentraland/apis` | No target yet |
| Events `events` (`/api/events`, `/api/events/{id}`). `EVENTS_UPSTREAM_URL`. Status: mirror | `explore/src/events/mirror.rs` | `@decentraland/apis` | `EVENTS_MIRROR_ADOPT` adopts the mirrored rows as local once the old service stops ([explore API parity ADR](adr/2026-11-05-explore-api-parity-contract.md)) |
| Event posters `events-assets-099ac00` (`/poster/{file}`, `/poster-vertical/{file}`). `EVENTS_POSTER_UPSTREAM_URL`. Status: mirror | `explore/src/api/posters.rs` | `@decentraland/apis` | No target yet |
| Worlds content server `worlds-content-server` (`/index`, `/world/{name}/{about,scenes,permissions}`, content). `WORLDS_CONTENT_MIRROR_SOURCE`. Read-through only in dev. Status: mirror | `services/catalyst/worlds/src/mirror.rs`, `content/src/read_through.rs` | `@decentraland/apis` | No target yet |
| Communities `social-api` (`/v1/communities`), thumbnails from `assets-cdn`. `COMMUNITIES_UPSTREAM_URL`, `COMMUNITIES_MIRROR_THUMBNAIL_URL`, and the `catalyst communities mirror` CLI defaults (`src/main.rs`). Off in dev. Status: mirror | `services/catalyst/social/src/communities/mirror.rs` | `@decentraland/apis` | No target yet |
| Denylists `asset-bundle-registry` (`/denylist`, every 120 s) and `config` (`/denylist.json`). `DENYLIST_URLS`, `EXPLORER_DENYLIST_SOURCE_URL`, `DENY_LIST_URL` (comms; not set in dev). Status: mirror | `content/src/denylist.rs`, `explore/src/api/denylist.rs`, `comms/src/archipelago/mod.rs` | `@decentraland/apis` | No target yet |
| Explorer lists, flags and map: `dcl-lists` (`/pois`, `/banned-names`), `feature-flags` (`/explorer.json`), `api` (`/v2/tiles`, `/v2/tiles/info`). `EXPLORER_LISTS_SOURCE_URL`, `EXPLORER_FEATURE_FLAGS_SOURCE_URL`, `EXPLORER_MAP_SOURCE_URL`. Status: mirror | `explore/src/api/lists.rs`, `explore/src/api/feature_flags.rs`, `explore/src/api/map/mod.rs` | `@decentraland/apis` | No target yet |
| Asset-bundle registry `asset-bundle-registry` (`/entities/{active,versions,status}`, `/worlds/{name}/manifest`, `/queues/status`). `EXPLORER_REGISTRY_SOURCE_URL`. Status: mirror | `explore/src/api/registry.rs` | `@decentraland/apis` | The local abgen runtime with native Registry state ([registry ADR](adr/2026-11-09-registry-local-catalyst.md)) |
| Asset-bundle CDN `ab-cdn` (`/{key}`). `ABGEN_UPSTREAM_AB_CDN`. Status: mirror | `explore/src/api/ab_cdn.rs` | `@decentraland/apis` | Empty: every object comes from this node's cache and abgen (`config/src/explore.rs`) |
| Marketplace trades `marketplace-api` (`/v1/trades`, `/v1/trades/{id}`). `TRADES_SYNC_UPSTREAM_URL`. Status: mirror | `services/catalyst/shop/src/market/trades.rs` | `@decentraland/apis` | No target yet |
| Subgraphs `subgraph` (`collections-ethereum-mainnet`, `collections-matic-mainnet`, `marketplace`, `land-manager`, `tpr-matic-mainnet`). `ETH_COLLECTIONS_SUBGRAPH_URL`, `MATIC_COLLECTIONS_SUBGRAPH_URL`, `MARKETPLACE_SUBGRAPH_URL`, `LAND_SUBGRAPH_URL`, `THIRD_PARTY_REGISTRY_L2_SUBGRAPH_URL` (not set in dev). Status: mirror | `chain-index/src/graph.rs`, `chain-index/src/land_graph.rs`, `builder/src/onchain.rs`, `shop/src/market/third_party.rs` | `@decentraland/apis` | LAND and permissions read the local land-authz and marketplace indexes once they are published (`config/src/shop.rs`); no target yet for collections and third-party registries |
| NFT worker `nfts` (`/wallets/{owner}/networks/{network}/nfts`). `NFT_WORKER_BASE_URL`; mirror refuses to start without it. Status: mirror | `shop/src/market/third_party.rs` | `@decentraland/apis` | No target yet. When it is unset, holdings are read over JSON-RPC |
| OpenSea fallback `opensea` (`/api/v2/chain/{chain}/contract/{contract}/nfts/{id}`). `OPENSEA_FALLBACK_URL`. Status: mirror | `explore/src/opensea.rs` | `@decentraland/apis` | No target yet |
| Profile images `profile-images` (`/entities/{entity}/{face,body}.png`). `PROFILE_IMAGES_ORIGIN_URL`, the last resort behind the local renderer. Status: mirror | `social/src/profile_images/origin.rs` | `@decentraland/apis` | No target yet |
| Media convert remote sources: mirrored place and event images on the peers' `/content/contents/...`. `MEDIA_CONVERT_REMOTE_SOURCES`. Status: mirror | `social/src/media/` | `@decentraland/apis` | No target yet |
| Foundation Builder `builder-api` (`/v1/collections`, `/v1/items`, `/v1/itemCurations`, `/v1/thirdParties`, publication writes). Catalyst verify: `BUILDER_FOUNDATION_URL` (mirror). Sites relay: `/api/builder-foundation/*` relays only when `NODE_POSTURE=mirror`, and answers 503 otherwise. Status: mirror | `services/catalyst/builder/src/linked_publication_review.rs`; `apps/sites/src/data/lib/catalyst/builder/foundation-relay.server.ts`, `foundation-fetch.ts`, `linked-providers.ts` | `@decentraland/apis`, `@decentraland/web-applications` | No target yet. The node's `/builder-api` does not replace Foundation curation |
| Builder storage `builder-api` (`/v1/storage/contents/{hash}`) and `builder-items` (`/{hash}`). `BUILDER_ASSET_PACKS_BUCKET_URL`, `BUILDER_CONTENT_BUCKET_URL`. Status: dev-only | `services/catalyst/builder/src/catalog.rs`, `tooling/dev.py` | `@decentraland/apis` | No target yet |
| Testnet upstreams: `contracts` (`/addresses.json`), `transactions-api.decentraland.zone` (`/v1/transactions`), and the `.zone` peer, marketplace and subgraphs of the `testnet` profile. Status: dev-only | `tooling/dev.py` (`testnet_env()`); `shop/src/evm/mod.rs`, `shop/src/evm/upstream.rs` | `@decentraland/infra`, `@decentraland/apis` | No target yet |
| Shop CMS images `cms-images`. Active only when `VITE_SHOP_CMS_URL`, `_SPACE_ID`, `_ENVIRONMENT` and `_ADMIN_ENTRY` (or `__SHOP_CMS__`) are all set; nothing sets them. Status: inactive by default | `apps/ui3/src/shop/discovery/lib/contentful.ts` | `@decentraland/web-applications` | No target yet |
| `/play` HUD services (realm provider, peer, places, events, map, marketplace, social, notifications and the rest). The HUD defaults to this deployment's gateway (`GATEWAY_PREFIXES`). It reaches `.org` or `.zone` only on a Decentraland origin, with the trusted `?baseDomain=`, or without `__DCL_PUBLIC__` (standalone). Status: inactive by default | `apps/ui3/src/play/lib/baseDomain.ts` | `@decentraland/web-applications` | Already served by the gateway prefixes; the exit is removing the `.org`/`.zone` fallback |
| Scene editor host: `worlds-content-server`, comms gatekeeper, storage and multiplayer on `.org` or `.zone`, and `creators-data` (`/v2`). Offered only on loopback, or on Decentraland hosts with `editor-features=experimental`. Status: dev-only on dcl-one deployments | `apps/ui3/src/play/features/editorHost/source.ts`, `editorHost/config.ts` | `@decentraland/web-applications` | No target yet. The node has `/worlds-content-server`, `/comms-gatekeeper` and `/world-storage`, but no document moves the editor host to them |
| `@dcl/sdk-commands`: `peer` and `peer.decentraland.zone` (`/lambdas/contracts/servers`, `/content/entities/active`, `POST /content/entities`), `quests`, `feature-flags.decentraland.zone`, `rpc/mainnet`. `DCL_CATALYST` (default `https://peer.decentraland.org`), others hardcoded. Status: production (creator CLI) | `library/js-sdk-toolchain/packages/@dcl/sdk-commands/src/` | `@decentraland/infra` | `apps/dcl-scene-cli` replaces the package; no per-endpoint target |
| Bevy Explorer E2E against `decentraland.zone` and `peer.decentraland.zone`. `vars.DCL_E2E_BASE_DOMAIN`, `vars.DCL_E2E_REALM`. Runs on the `ci:e2e` label, after main CI, nightly or by hand. Status: CI-only | `.github/workflows/bevy-explorer-e2e.yml`, `apps/bevy-explorer/e2e/driver/target.ts` | `@decentraland/client` | No target yet |
| Shop parity and trade journey harnesses: they record live traffic to the Shop's services and use `rpc` (`/mainnet`, `/amoy`, `/sepolia`). Run by hand; CI runs only their offline tests. Status: dev-only | `tooling/shop-parity/lib/services.mts`, `lib/session.mts`, `tooling/shop-trade-journey/lib/wallet.mts` | `@decentraland/web-applications` | No target yet |

Not listed: human links, canonical and sitemap URLs, URLs written into responses that the node does not fetch, mocks and fixtures, and third-party services (CoinGecko, publicnode, thirdweb, Segment and others).

## The web Explorer (`/play`)

`web-install` imports our verified Bevy web build (`/play/sites-play.json`):

```sh
gh auth login   # import CI artifacts, or build locally:
make bevy-web && python3 tooling/fetch-play.py --from-build apps/bevy-explorer/dist/web
gh workflow run bevy-explorer-ci.yml --ref main   # no matching or expired artifact
```

See [production rules](../infra/containers/README.md#sites-explorer-build-input).
`/play/app.html?mock=1` runs the HUD without the WebGPU engine.

## Validation

```sh
nix develop --command bash tooling/dev-smoke.sh
nix develop --command bash tooling/dev-test.sh [rust|e2e]   # rust: PostgreSQL only
```

CI runs the Rust suites in `nix develop .#ci`: the toolchain, the services the tests
start and the build tools, without the web and media tools.

Run both in a fresh checkout. `python3 tooling/ci-local.py install` adds hooks
that run the matching workflows' checks (images need Linux or
`CI_LOCAL_IMAGES=1`); `ci-local.py run [REV]` runs them without pushing.
The Rust tests run in three jobs that each compile the tests and run a third of
the test executables; on PRs they run on Linux (`CATALYST_TEST_SHARD=i/n` for
`tooling/catalyst-test.sh`). A PR that changes a build or dev-stack input gets a
notice to add the `ci:macos` label: Cargo manifests and lockfiles, build
scripts, toolchain files, `.cargo`, `library/vendor/rusty_enet`, Nix,
`infra/dev`, the Makefile, the dev tooling and
`.github/workflows/development.yml` (`tooling/dev-ci.py`). On `main`,
the Rust tests and smoke run on macOS, and a new push does not cancel a running
Development run: it waits, replacing any run already waiting, so each completed
run tests every earlier commit and a macOS-only failure shows up
there. A PR labelled `ci:macos` runs both on macOS (label, then push or reopen;
re-running a job does not pick up a new label).
