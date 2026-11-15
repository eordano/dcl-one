# dcl-one

`README.md` covers what this is and how to build, run and test it.

## Agent work and commits

- Keep accepted architecture in `docs/adr/` and reusable instructions in this tree.
- Keep agent instructions and skills usable across LLM providers and models; omit provider-specific metadata files.
- Keep plans, briefs, handoffs, task reports and review evidence in ignored `.agent-work/`; follow `services/catalyst/docs/comms/README.md` for specialist reporting. `services/catalyst/docs/comms/plans/` and `.superpowers/` stay local too.
- Before committing, capture accepted public architectural choices in ADRs and check the staged paths for working documents and sensitive data.
- Never commit working documents unless the user asks.
- Tracked documentation must stand alone without local plans or private ledger exports.
- For tracked documentation, ADRs, agent instructions, comments and PR prose, apply the [public-prose rule](rendering/bevy-explorer/docs/guidelines/contributing.md#public-prose), including its pre-publication search.
- Make focused edits; preserve other agents' work.

For formal verification of LiveKit or networking decisions, or maintenance of
their proof evidence, use
[verify-dcl-networking](.claude/skills/verify-dcl-networking/SKILL.md).

## Agentic roles documentation

- README `Agentic roles` lists every agent role: name, goal, main functions, relative link to its tracked definition or workflow.
- Adding, renaming, changing or removing a role updates that table in the same change, verified against the definitions.
- Describe local-only roles only from tracked workflows; never link ignored files.

## Scope

Code that serves none of these deliverables is removed, not kept for reference:

1. landing page;
2. What's On and discovery (places, events, map);
3. `/play`: Explorer (`rendering/bevy-explorer`), HUD (`apps/ui3/src/play`), desktop packaging (`apps/bevy-explorer`);
4. `/shop`: Marketplace and Store merged;
5. `/create`: Creator Hub, web and the desktop workspace `apps/creator-hub` (Electron app, `@dcl/inspector`, `@dcl/asset-packs`);
6. `apps/dcl-scene-cli`: the `@dcl/sdk-commands` rewrite;
7. `/docs`: the docs.decentraland.org rewrite;
8. page-coherence scripts (`apps/sites/scripts`);
9. one backend schema (`schemas/`);
10. the `catalyst` binary: every backend area behind one base domain;
11. the protocol (`library/protocol`): the `.proto` contract and the `@dcl/protocol` package;
12. the creator SDK (`library/js-sdk-toolchain`): the `@dcl/sdk` packages, built from the protocol beside it.

- Docs state current behavior, verified capabilities and next work; no transition histories or internal generation labels.
- Compare compatibility only against pinned `github.com/decentraland` repositories or dated Decentraland server captures.
- A recorded difference is not approval; identify unresolved gaps and their evidence in the review report.
- Check the current readiness evidence before claiming deployment or compatibility; missing evidence means the claim remains unverified.
- Write PR descriptions per `.github/PULL_REQUEST_TEMPLATE.md`.
- For schema, SQL, ingestion, mirrors or caches, apply `DATABASE.md`; state workload and invariants before choosing tables or indexes.
- Record measured performance changes in the task report; follow-ups are candidates until measured.
- Preserve wire, authorization and crash/resume contracts.
- Never reset an existing database to fit a changed schema.

## Code structure

- Read the [layout ADR](docs/adr/2026-10-19-organizing-library-services-folders.md) before adding, moving or renaming a folder or crate; record layout changes as a new ADR.
- `library/` depends only on library code.
- `library/protocol/proto` is the only `.proto` source ([ADR](docs/adr/2026-10-25-protocol-in-dcl-one.md)); `tooling/proto-source-check.py` fails on any other copy.
- Reach other files through `env!("DCL_ONE_ROOT")`, never `../` chains.
- Domain crates follow `docs/module-layers.toml`; no upward dependency.
- Keep the fresh-schema bootstrap single.
- Put helpers other crates' tests use behind a `test-support` feature.
- ops, presence, serve and main stay in the root crate.

## Ownership

- `explore`: discovery and explorer-facing APIs (places, events, map, realms, flags, client config), including durable world-scene place identities maintained inside deployment transactions; `places::RemovedPlaces` captures catalogue removals, which comms consumes for moderation and ingress cleanup.
  - Explore reaches social by shared SQL on exactly two edges, the current contract: it reads `social.communities` and `social.community_members` (one `EXISTS` per community-event write: the community active and not suspended, the signer its owner or an owner/moderator member; explore never writes those rows), and it inserts `social.event_notification_intents` rows (one per event notice, in the event's own transaction, carrying the event snapshot; social owns the table, processes the rows and deletes processed ones after its retention window; explore never updates or deletes them). Changing a column either edge reads or writes updates this paragraph in the same change.
- `social` owns badges; its badge processor reads, never writes, other areas' tables by shared SQL: `content.deployments` and `content.active_pointers` (deployments by `id`, scene titles, wearable rarity), and on Polygon `shop.items`, `shop.collections`, `shop.sale_event`, `shop.trade_event`, `shop.nft_event`, `shop.chain_cursor` (an index's published block) and `shop.mirror_state` (a mirror's first complete pass). Changing a column it reads updates this paragraph and `DATABASE.md` "Badge processor" in the same change.
- `worlds`: world catalogues, deployments, permissions, world/player storage, encrypted env values, SDK7 scene hosting with per-scene isolation, limits and cancellation.
- Guest code reaches storage through a scoped host API, never the shared pool.
- `comms` (layer 8): Pulse, the archipelago connector (`/ws`), the gatekeeper (admission, moderation, voice, streaming, LiveKit webhook, room events) and presence; all but Pulse run in `catalyst serve`.
  - Worlds supplies access and scene identity via `comms::interfaces`; social supplies membership, privacy and notification persistence via `SocialDirectory` and keeps MLS.
  - Comms owns streaming lifecycle and cleanup intent; social owns notification delivery records.
  - Comms enforces access through local calls and its own SQL: owner/player/host scopes, one room resolver, one LiveKit/WebSocket transport; LiveKit media stays external.
  - Pulse keeps assignments and unfinished takeovers in process memory, persists nothing, admits only from its room-authority response and imports nothing from the gatekeeper.
  - Only the gatekeeper holds the LiveKit key, mints/verifies the relay admission proof and applies live room policy; it owns cleanup receipts and acknowledges retained takeovers.
  - Pulse runs in its own process unless a load measurement shows the loop is unaffected in-process.
  - The connector is part of the gatekeeper (local calls); Pulse and `serve` share the NATS subjects in `comms::protocol::bus`.
  - Pulse alone owns the legacy Archipelago statistics population, independently of native ownership and without peer expiry. The connector forwards authenticated heartbeat/disconnect observations; its retained room assignments serve enforcement only. `comms::stats` supplies read-only complete snapshots to HTTP, content/about/realms and the presence archive; one archive pass retains one snapshot across content I/O. Hot scenes use serve's existing content pool and shared content denylist before projection and top-100 selection; thumbnails name `CONTENT_URL`, or this node's `/content` when it is unset. Preserve the accepted [local source semantics](docs/adr/2026-11-12-stats-local-content-source.md) when changing enrichment.
  - The root owns Stats configured route compilation on the pinned embedded V8; comms owns the fallible async matcher interface. Preserve the pre-worker initialization and lifetime-wide unsafe preparation contract, and retain one owned admission permit inside submitted blocking work through native teardown ([ADR](docs/adr/2026-11-12-stats-configured-matcher.md)).
  - Only the primary `serve` (`FLEET_JOBS_ENABLED`) serves `/ws` and the island subscriber.
  - Social replicas derive online status from Pulse's wallet topology over NATS; every `serve` recovers room occupancy through the gatekeeper's LiveKit adapter; presence owns observations, source age and public/admission projections.
- `storage`: the shared hash-addressed blob store.
- `shop::credits::goals` owns seasonal MANA rewards, separately from the paid USD ledger. Its photo worker reads `camera_reel.photo_events` (`id`, `kind`, `photo_id`, `user_address`, `payload`, `created_at`) and `social.subscriptions` (`address`, `email`) through shared SQL; its policy reads `explore_api.feature_flags_local` (`app`, `name`, `value`). It never changes those rows. It commits its receipt, progress, signed grant and notification through Social's transaction-aware notification helper in one transaction. Changing a shared column updates this paragraph and `DATABASE.md` in the same change.
- Domain ownership is not a separate deployable; keep the shared SQL and process contracts.

- Prefer the smallest readable implementation: no forwarding layers, internal HTTP hops or per-service pools; direct SQL and a passed transaction for atomic writes.
- A new crate needs an independent consumer or a concrete build requirement.
- Preserve external API paths and room names when code moves.
- Shop absorbs every Marketplace feature (resales, bids, LAND, NAME resale, listing, owners and sales, lists), shown
  only to a web3 wallet session while the `dapps-shop-marketplace-features` rollout is on
  (`apps/ui3/src/shop/market/access.tsx`); web2 sessions never see them. Web3 means a web3 sign-in sites recorded
  (`apps/sites/src/data/lib/auth/signin-kind.ts`) with a wallet here to sign, never an installed extension alone. One
  exception: the in-world HUD outside the web shell (desktop, native) cannot buy or tell the wallet, so a signed-in
  player who is not a guest may browse resales there, and the web Shop decides again. New and migrated Shop code
  follows `docs/web-app-standards.md` and `docs/ui-standards.md`.
- Credits are the main payment option; logged-in web3 wallets also see MANA.
- LAND and every secondary sale are MANA-only.
- Enforce eligibility in backend and settlement, including legacy routes; hiding a button is insufficient.

## Self-contained, public tree

- Never add a dependency on a sibling checkout.
- `rendering/bevy-explorer/source` is its own workspace; integration lives in `apps/bevy-explorer`.
- Keep component licenses and pins (`upstream.lock.json`, `abgen-release.lock`, `livekit-release.lock`, `tooling/uuav/uuav-binaries.lock.json`).
- Regenerate `.github/CODEOWNERS` from `ownership.json` with `tooling/owners.py`.
- No machine names, private addresses, home paths, personal emails or the operator's domain; deployments supply them via gitignored config.
- Never delete the gitignored `credentials` and `data` directories.
- `/docs` data is `apps/sites/content/docs` (`pnpm gen:docs-content`); media is Git LFS.

## UI standards

Before changing web UI in `apps/` (including the Play HUD in
`apps/ui3/src/play/`), `library/ui/` (including the Explorer HUD UI in
`library/ui/apps/hud/`, shared by both HUD hosts) or
`rendering/bevy-explorer/source/react-web/`, read and follow
[`docs/ui-standards.md`](docs/ui-standards.md). It defines component styling,
shared foundations and components, accessibility, migration and verification.
To implement a Figma design, follow the `implement-figma-design` skill in
`.claude/skills/`; it resolves each component to the library instead of
recreating it. To port a capture into `library/ui`, follow
`maintain-figma-library`.

State, data fetching and structure follow [`docs/web-app-standards.md`](docs/web-app-standards.md), and tests follow [`docs/frontend-testing.md`](docs/frontend-testing.md).

## Explorer automation

The end-to-end suite in `apps/bevy-explorer/e2e` and the Explorer's console surface it drives
(`rendering/bevy-explorer/source`: `src/automation.rs`, `src/world_state.rs`,
`library/explorer-core/lib/hudAutomation.ts`, `local-harness/web-driver`, documented in the readme's
"Agents and tests") read the code they test, so a change to that code updates them in the same
change. That is any of: a HUD control's role, accessible name, placeholder, `aria-*` state or
visibility; a page's heading or tablist; a key binding; a console command's name, arguments or
reply; a launch option or the trusted-input contract; a visual scene, its parcels or the capture
window; a move of a path under `apps/bevy-explorer`, `tooling/bevy-e2e-*.py` or the workflow
`bevy-explorer-e2e.yml`; a rename of the CI workflow, its jobs or the web artifact.
`apps/bevy-explorer/e2e/README.md`, "Keeping the suite true", maps each change to what it touches
and the checks to run.
