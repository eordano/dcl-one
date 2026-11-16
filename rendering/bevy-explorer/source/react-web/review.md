# review.md — how we keep the HUD working

> **Suite availability:** this public tree includes the unit-test sources but omits
> the visual/engine gate specs, screenshot baselines and Playwright gate/visual
> configs described below. Those procedures apply only in an authorized test
> workspace that supplies them. Inspect the files at the target revision before
> running a tier; a package script alone does not prove its suite is present.
> Report unavailable tiers as **not run** and keep their evidence in the task report.

The single place to look before merging anything in `react-web/`. It defines the **test harness**
(what proves "everything still works"), the **per-domain expectations**, and the **pre-merge
checklist**. Keep it current: when you add a domain or a rule, add it here.

---

## 0. TL;DR — run this before you push

```bash
# from react-web/ — Node 24 (`.nvmrc`); on Node 20 vitest reports "no tests" and the SDK build fails
npm run typecheck          # tsc, the app and the shared HUD UI (library/ui/apps/hud)
npm test                   # tier 1 — the unit tests present at this revision, the shared HUD UI's included
# With the authorized visual suite and configuration present:
npm run test:visual        # tier 1.5 — visual regression (headless, no engine)
(cd bridge-scene && npm run build && npm run lint)   # the SDK's type check + eslint (must be 0 errors)
```

The bridge scene is type-checked by `sdk-commands build`, not bare `tsc` — its tsconfig trips
TypeScript's deprecation errors under the installed compiler.

`test:visual` reuses whatever is already serving `:5173` (and `npm run dev` reuses `:8100`), so in a
worktree make sure those aren't another checkout's servers, or run your own on other ports
(`E2E_URL=http://localhost:5174`).

If `test:visual` reports diffs, open the Playwright HTML report (`npx playwright show-report`) and
look at the **expected vs actual vs diff** images. Either it's a real regression (fix it) or an
intended change (refresh the baseline — see §2).

The real engine + the world-space UI it draws (3D nametags, crosshair) can't be screenshotted
deterministically — they have their own tiers (§3, §4).

---

## 1. The testing tiers (why three)

Connectivity feedback is engine-owned. `library/ui/apps/hud/test/hosted/connectivity.test.tsx` checks the shared
SystemApi stream/retry bridge, subscription cleanup, stable unchanged snapshots, one Connection Lost alert dialog,
pending Retry/Exit controls through bridge acknowledgement, reply timeout and session cleanup,
failed-retry recovery controls, automatic dismissal,
and scoped access messages. Runtime verification reads `/connectivity_state`, uses
`/connectivity_retry`, and locates `alertdialog` named `Connection Lost`, buttons `Retry` and
`Exit Application`, and status `Multiplayer status`. The engine's Connectivity debug section exposes the
same safe snapshot and Retry action. Preview, held-world and terminal access failures must
never become an offline alert. A reachable network with a failed or disconnected room shows its
room status. An initial `connecting` room stays quiet during ordinary scene crossings.

| Tier | Command | What it proves | Engine? | Deterministic? |
|------|---------|----------------|---------|----------------|
| **1 — contract** | `npm test` | Every page→scene API call posts the exact wire message, and every scene→page message updates state. Component clicks fire the right action. | no (FakeDriver) | yes |
| **1.5 — visual** | `npm run test:visual` | Every DOM **domain renders correctly** — layout, colour, tokens, spacing — diffed against a committed baseline PNG. | no (`?mock=1`) | yes |
| **2 — engine** | `npm run test:e2e` | The guest-reachable calls actually **round-trip through the real bevy engine + bridge scene**. | yes (headed, WebGPU) | partly |

Tier 1 is the contract net (every call, including ones a guest can't reach). Tier 1.5 is the
*appearance* net (the thing humans notice). Tier 2 is the *it-actually-talks-to-the-engine* net.
They're complementary; none replaces another.

> **Why visual regression over an "agent drives Chrome and looks" approach?** A committed-baseline
> Playwright suite is repeatable, runs in CI, pins the exact pixels, and needs no human/agent in the
> loop to pass. The agent-driven flow is great for *exploring* a new bug, but it can't be a
> regression gate. So: Playwright for the gate, agent checklist (§4) only for what can't be mocked.

---

## 2. Tier 1.5 — the visual harness (`e2e/visual.spec.ts`)

Renders the HUD in **mock mode** (`?mock=1`: fake bridge, fixed data, no engine/GPU) and screenshots
each domain. Config: `playwright.visual.config.ts` (headless, 1600×900, `maxDiffPixelRatio: 0.01`).

**Determinism is engineered, not hoped for** (`prepare()` in the spec):
- **Frozen clock** (`page.clock.setFixedTime`) → relative times ("2h ago", "Yesterday") are stable.
- **External images stubbed** → every non-localhost avatar/thumbnail becomes a 1×1 transparent PNG,
  so the network can't make a screenshot flake (layout is preserved; only the bitmap is blanked).
- **Animations disabled** at capture time; **fixed viewport + `deviceScaleFactor: 1`**.

**Domains covered** (one baseline each, in `e2e/visual.spec.ts-snapshots/`):
`showcase` · `login-fresh` · `lobby` · `mobile-gate` · `browser-gate` · `gpu-gate` ·
`engine-error` · `realm-error` · `world-hud` · `profile-card` · `passport` · `hover-tooltips` ·
`permission-dialog` · `permissions-popover` · `world-password-dialog` · `community-modal` · `community-create-modal` ·
`panel-friends` · `panel-settings` · `panel-settings-keybindings` · `panel-profile` ·
`panel-notifications` · `panel-emote-wheel` · `panel-communities` · `panel-map` ·
`minimap-settings` · `backpack-wearables` · `backpack-emotes` · `passport-edit` · `name-edit`.

Not yet baselined: the Events page (`library/ui/apps/hud/features/events`: the calendar, a day's
grid, the event detail). Mock mode serves it a week of events around now
(`library/explorer-core/engine/mockEvents.ts`; `?mock=1&mockEvents=error` fails every read), so a
baseline needs the frozen clock. Its contract is tier-1, in `library/ui/apps/hud/test/hosted`:
`events.test.tsx` (the reads sent, the loading, error, empty and loaded states, the day
navigation, the reminder, the detail, share and jump), `eventsStore.test.ts` and
`eventFormat.test.ts`; the bridge's requests are in `src/test/bridgeEvents.test.ts`.

Not yet baselined: the integrated-GPU guide (`features/gate/IntegratedGpuGuide.tsx`, opened over the
lobby or world when the pre-boot probe sees an Intel adapter on Windows; `?mock=1&gpuGuide=1` forces
it). Its contract is tier-1: `library/ui/apps/hud/test/integratedGpuGuide.test.tsx`, `library/explorer-core/lib/integratedGpu.test.ts`.

**Updating baselines** (only when the change is intentional):
```bash
npm run test:visual:update      # regenerates the PNGs
git add e2e/visual.spec.ts-snapshots && git diff --cached --stat   # then EYEBALL the new PNGs
```
Never blind-update. A baseline refresh in a PR must be reviewed image-by-image.

**Caveats / known limits**
- **Baselines are platform-specific** (`*-chromium-darwin.png`). Font hinting differs across OSes, so
  generate/verify on the same platform CI uses (or add a Linux baseline set when CI is wired).
- **Mock data is the ceiling.** A panel can only be as rich as `library/explorer-core/engine/mockBridge.ts` makes it.
  - Enrich mock `getEmotes` with an OWNED collection (some with `slot`, varied `rarity`/`count`) so
    the new owned-grid + assign-to-slot are exercised, not just the 10 base emotes.
- **Captures are viewport-sized, hover-free.** A scrolling panel is pinned only above the fold: the
  `passport` baseline shows About Me + the Equipped Wearables grid; Equipped Emotes, Badges, the
  About → Equipped → Badges order and the SHOP-link rule are tier-1 (`profile.passport.test.tsx`).

---

## 3. Tier 2 — real engine (`e2e/engine.spec.ts` + `e2e/engine-clicks.spec.ts`)

See `e2e/README.md`. Needs a real GPU + a local engine build (`../deploy/web`). Run before a release
or when touching the bridge protocol.

- **`engine.spec.ts`** — every panel *opens* and *fetches* (the call crosses the bridge).
- **`engine-clicks.spec.ts`** — the controls *inside* panels do something the **engine** sees,
  verified **through the bevy console API** where the engine exposes state, bridge-spy otherwise:

  | Click | Verified by |
  |-------|-------------|
  | move / teleport | `/player_position` lands at the exact coords (`helpers.position`) |
  | equip a wearable | `/get_user_data` profile **`vN` bumps** (`helpers.profileVersion`) + `equip` on the bridge |
  | assign an emote to a slot | `equipEmote` on the bridge |
  | play a wheel emote | `triggerEmote` on the bridge |
  | change a setting (slider) | `setSetting` on the bridge |
  | (sanity) profile + roster reachable | `/get_user_data`, `/connected_players` |

  Console helpers live in `e2e/helpers.ts` (`position`, `getUserData`, `profileVersion`,
  `connectedPlayers`). Available bevy commands: `move_player_to`, `walk_player_to`, `teleport`,
  `player_position`, `get_user_data`, `connected_players`, `live_scenes`, `list_portables`, `emote`,
  plus `/login_*`, `/logout`, `/chat`.

**Still tier-1-only** (a fresh guest can't reach them, so they can't round-trip here): accepting a
friend request, leaving a community, marking notifications read, community chat (the tabs come
from the communities the account is a member of; `library/ui/apps/hud/test/hosted/communityChat*.test.tsx`, and
`apps/bevy-explorer/e2e/specs/community-chat.test.ts` with a member account), creating, editing
and deleting a community (it needs an account with a claimed NAME), linking places to it (an account
that owns a LAND or a World with a deployed scene; `library/ui/apps/hud/test/hosted/communities.clicks.test.tsx` covers the
form, the places picker, the card's ADD PLACE and place delete, the content-check screens, the picture checks, the card's Edit and Delete, the member row and menu, and the browse grid's search, paging and card buttons; `library/ui/apps/hud/test/hosted/communities.invites.test.tsx` covers the Invites & Requests section and its counter, accepting and rejecting invites, the card's Banned, Join Requests and Invited sections, the Invite to Community submenu and the notifications that open a card; acting on a member needs a second account in a community the first owns or moderates, an invite or a join request needs a second account too, and finding a community past the first browse page needs a service with more than 20) — these need seeded
data and are covered deterministically in tier 1. **TODO to deepen tier 2** (would need engine query commands or
fixtures): map *UI* teleport by clicking a parcel, a `/mic_state` query to confirm the mic toggle,
logout via the profile chip.

---

## 4. World-space UI — agent / manual checklist (can't be mocked)

The 3D nametags, the pointer-lock crosshair, and projected proximity tips are drawn **in the engine
scene**, not the DOM, so `?mock=1` can't show them and screenshots can't be deterministic. Verify
these by hand (or have an agent drive the Chrome extension against a live world) after any change to
`bridge-scene/src/domains/{nametags,pointer,proximity,avatarPointer}.*`:

- [ ] **Nametags** appear above every avatar's head, follow them while walking, and **face the camera**.
- [ ] Name **colour** = profile custom colour if set, else the address-hash palette; **claimed** names
      show the verified seal, **unclaimed** show `#abcd` tight to the name.
- [ ] A name colour picked on the passport reaches the tag once the passport closes, on your own
      avatar and, for other players, on theirs, without a rename or a reload.
- [ ] **Fixed world scale — never a giant.** A tag is a constant *world* size (it shrinks with distance,
      like the old SDK scene); it must NEVER balloon into a screen-filling pill when the camera pulls back
      or when its `AvatarAttach` fails to bind. (We dropped the old constant-on-screen scaling precisely
      because it turned a stranded tag into a giant.)
- [ ] **Exactly one tag per address — no duplicates / orphans / pile-up.** None accumulate over a long
      session, after alt-tabbing away and back, or across a comms/livekit blip's leave→enter churn (the
      classic regression). They're pooled — one reusable entity per wallet, never destroyed (engine entity
      removal is unreliable for attached entities; we show/hide instead).
- [ ] Tags **fade out** past ~20–40 m. Your **own** tag shows above your head in third person and is
      hidden in first person.
- [ ] **Crosshair** shows when the camera is locked (mouse hidden) and hides when the cursor is free.
- [ ] **Hover / proximity prompts** ("Press E…") show on interactables and sit on the right entity.
- [ ] **Community chat bubbles** (`bridge-scene/src/domains/{chat,nametags}.*`): a message said in a
      community chat by a player standing nearby shows as their bubble with the community's name on
      a smaller, dimmer, left-aligned row above their name, cut short with an ellipsis (and over
      your own head for your own); a Nearby message shows no such row, and a DM no bubble at all.
- [ ] **Hushed badge** (`bridge-scene/src/domains/{nearbyMutes,voice,nametags}.*`): while a player whose
      nearby voice you muted (profile menu, Hush) is talking, their tag shows the red hushed icon in
      place of the green speaking bars, and nothing when they are quiet; Hear brings the bars back.
      The same player talking in a private call or community stream with you, which the mute leaves
      audible, shows the green bars.
- [ ] **Lobby stage** (`bridge-scene/src/domains/avatarPreview.tsx`, lobby region): after sign-in the
      page's stand-in (backdrop + snapshot) crossfades to the engine stage with the live avatar centred;
      Customize opens the Backpack modal with the avatar through its hole and hands the stage back on
      close; Jump In loads the world (the hold is released) and a failed trip returns to the lobby.
      The `lobby` baseline only shows the stand-in (mock has no engine).

---

## 5. Pre-merge review checklist

Run through this on every diff (it encodes `AGENTS.md`):

- [ ] **Design system** — no new hardcoded brand/status hex, `rgba()`, radius, or type size in a
      component (`grep` your diff for `#` and `rgba(`); each must be a `tokens.css` var or justified.
      No bespoke `<button>`/control where a `@hud/design` primitive exists.
- [ ] **No dead code** — no unused exports/CSS classes/imports; no commented-out blocks; comments are
      sparse and explain *why*, not *what*.
- [ ] **Structure** — one concern per file; reuse a primitive or *create* one rather than inlining a
      recurring pattern; bridge logic in one `domains/*` file.
- [ ] **Perf** — catalyst fetches cached (per address); per-frame engine systems are change-gated /
      throttled, not doing work every frame for free.
- [ ] **Tests** — tier 1 still green; **a new/changed domain has a tier-1.5 baseline**; world-space
      changes ran the §4 checklist.
- [ ] **Both projects typecheck** (`npm run typecheck` + `cd bridge-scene && npm run build`), and the
      bridge scene lints clean (`npm run lint` there).
- [ ] **No secrets / `.env` or working reports** staged; review the staged paths under the
      repository-root `AGENTS.md` rules. Use an imperative commit subject without AI attribution.

- [ ] **Input routing** — follows the §7 contracts (no hand-rolled Escape/hotkey handlers).
- [ ] **No native popups** — no `<select>`, no `<input type="date|time|color">`, no
      `alert`/`confirm`/`prompt`. On native the HUD is rendered offscreen and CEF paints popup
      widgets as a separate surface the engine never composites (`update_hud_texture` takes whatever
      paint arrives), so opening one replaces the whole HUD with the widget's bitmap. Use `Select`,
      `DateField` and `showConfirm`/`showDialog` from `@hud/design`.

---

## 6. Adding a domain

1. Build it (token-driven, reusing `@hud/design` primitives).
2. Add the mock data in `library/explorer-core/engine/mockBridge.ts` so `?mock=1` renders it richly.
3. Add a tier-1 test for its API contract + clicks beside the shared code: `library/explorer-core/test/`
   for core code, `library/ui/apps/hud/test/hosted/` for UI and session code (`src/test/` only for this
   host's own files).
4. Add a screenshot to `e2e/visual.spec.ts` and run `npm run test:visual:update`.
5. If it's world-space (engine-drawn), add a line to the §4 checklist instead of a screenshot.
6. List its expectations here.

---

## 7. Input routing contracts

The ENGINE resolves input to actions; the HUD reacts to the action stream and declares its
focus back (`uiFocus`). Keep it that way — the AZERTY bug and the CEF double-fire both came
from HUD-side key matching. The rules:

- **Never add a DOM hotkey or Escape handler.** Panel/menu keys arrive as `systemAction`
  messages (dispatcher in `useEngineSession`); add a `case` there. Cancel/close behaviour
  for leaf UI (a lightbox, an open dropdown) = register on the **cancel-layer stack**
  (`library/explorer-core/lib/cancelLayers.ts`) while open — the dispatcher peels popup → layer → panels, one
  per press, keyboard and gamepad alike.
- **The only DOM key handling allowed is inside a focused text field** (chat, map search),
  where the engine deliberately resolves nothing (`UiText`). There: match with
  `isCancelKey`, and `stopPropagation()` anything you consumed so the blur rule doesn't
  also fire on the same press.
- **A new panel/popup/overlay must be visible to `uiFocus`** — panels via the session's
  `anyPanelOpen`, dialogs via the popup stack (`openPopup`), full-screen input freezes via
  `inputLock`. If the engine can't see that your surface is active, scenes keep receiving
  input behind it.
- **The scene editor stands the HUD down through `session.editor.setMode`** (Create or `?editor`,
  `library/ui/apps/hud/features/editorHost`): in `edit` the dispatcher acts on `Cancel` only and App unmounts the
  sidebar, minimap, chat and pointer; `play` adds the reticle and the emote wheel. Not through
  `inputLock`: that declares `ui` focus, which would take the viewport's input from the editor's
  own scene. A new hotkey or persistent surface must respect the mode (`hudActsOn`).
- **Scrollable HUD content** just needs real `overflow: auto` — hover detection, the
  engine-side reservation and gamepad scrolling all key off that.
- Key **labels** come from `bindingLabels` (`keyHintFor`/`labelForInput`), never hardcoded
  letters — they follow the user's rebinds and keyboard layout.

## Portables

A portable experience runs beside the scene the player stands in: a smart wearable's scene
while it is worn, or a portable another scene spawned. The engine decides and lists them; the HUD
shows them and stops or restarts them (`bridge-scene/src/domains/portables.ts`). Expectations:

- **List**: the engine's `portables` message (`getPortablesStream`), sent on subscribe and on every
  change, never the HUD's own scene or a startup portable. No polling and no prompt of its own: a
  portable's permissions are asked on use through the scene permission dialog.
- **Rail**: the `Portable experiences` button shows while the list is non-empty; its tooltip reads
  `N running`, plus `, M paused in this scene` while the scene the player stands in disables
  portable experiences. Its popover lists one row per portable: name, kind (`Smart wearable` /
  `Portable experience`) and state (`Loading`, `Running`, `Failed: <reason>`, `Paused in this
  scene`, `Stopped`). `Stop <name>` on every row with a scene hash runs the console's
  `/stop_portable <hash>`; a stopped smart wearable (still worn) shows `Restart <name>` instead,
  `/smart_wearable restart <urn>`. The HUD's own command replies stay out of chat; a failed one is
  toasted.
- **Toasts**: entering running, paused, stopped or failed is toasted, from the list the player
  arrived with (no burst at login).
- Mock: `?mock=1&mockportables=1`. Tests: `library/ui/apps/hud/test/portables.test.tsx`.

## Analytics

The page reports the screens and clicks mobile does, with mobile's names and property shapes
(`lib/analytics.ts`). Nothing is sent from the page: each event is stamped when it happens, waits
in prefs (`PREF.analyticsQueue`, newest 200) and goes to the engine's `/track` console command once
the engine is up (`EngineDriver.launch` attaches the console); a send the console rejected is kept
for the next one. The engine batches the Segment posts, with its own events, and adds the common fields
as it does for those (`crates/analytics/README.md`), including `campaign_anon_user_id`, the
landing page's Segment anonymous id that the shell reads from the `ajs_anonymous_id` cookie and
hands the app as `anon_user_id` (`shell/main.ts`). The session id goes to the engine at launch (`EngineDriver.launch`), and the
return from the sign-in redirect continues it (the shell keeps it in sessionStorage and hands it
back as `analyticsSession`). Expectations:

- **Boot** (`bootAnalytics`, `main.tsx`): `App Opened` then `Screen Viewed` START, except on the
  return from the sign-in redirect. Between them, when the link carries a campaign (`utm_*`,
  `gad_*`, ad click ids; GA's cross-domain `_gl` alone does not count), `Install Attribution` in
  mobile's install-referrer shape, told apart from mobile's by `platform` `web` and
  `attribution_source` `url` (one per page load with a campaign, not per install): `referrer` the
  raw campaign params, the `utm_*`, `gclid`, `gbraid` and `wbraid` columns, and `page_session_id`,
  this load's session (mobile's column). Every page event carries `session_id`, the session of the
  load that created it: the queue outlives a load that bounces before the engine is up, and the
  engine keeps the page's value over its own, and drops an event held over 36 h by its own time
  (`crates/analytics/README.md`).
- **Sign-in link** (`authLoginUrl`): the entry's `utm_*` params ride along, so the auth site's
  own Segment calls carry the campaign and tie it to the address it signs in. Once read, the
  shell and the app take the attribution params (`utm_*`, `gad_*`, ad click ids such as `gclid`,
  the landing page's anonymous id) off the address bar (`dropAttributionParams`), so a copied link
  or a reload does not attribute again; the Google tag gets them from what the shell kept at load.
- **Sign-in** (`useEngineSession`): with no session the guest start page (`GuestStartPage`) is
  AVATAR_NAMING, its ACCEPT AND PLAY a LETS_GO click and CONTINUE WITH AN ACCOUNT (sign in or
  sign up) a SIGN_IN click; the guest account is created only on that click, and the welcome page
  it then needs is answered with the picked name instead of shown. COMEBACK (after a failed stored
  sign-in), ACCOUNT_GUEST_CREATE, AUTH_BROWSER_OPEN, AUTH_SUCCESS {login_type}, AUTH_ERROR {error};
  clicks lets_go, use_another_account, try_again. `Guest Wallet Creation` wraps the
  thirdweb guest step (`features/auth/guestWalletEvent.ts`).
- **Welcome / lobby**: WELCOME or AVATAR_NAMING, AUTH_DEPLOY_SUCCESS; clicks LETS_GO,
  NAME_RANDOMIZE; LOBBY, PLACES_PICKER; clicks JUMP_IN {kind, place_id|event_id}, CUSTOMIZE,
  NOTIFICATIONS, CLOSE (`LobbyHome.tsx`).
- **Loading**: the engine's, not the page's. LOADING_START {position, realm, when}, LOADING_END
  {position, status} and LOADING_TIMEOUT after 90 s follow its own loading screen
  (`crates/analytics/src/loading.rs`), the one `sceneLoading` mirrors here.
- **Social**: Friend Request, Friend Accept, Unfriend, Block User on the action.
- **Nearby voice**: `nearby_voice_user_mute` {wallet_id, is_muted} on the profile menu's Hush or
  Hear; not for the mutes read from the saved list, nor for a Hush or Hear the list already has.
- **In world**: the engine's First Move In World (an `analytics` message over the bridge, which
  relays the engine's own Segment events by name; a metre walked since logging in), once per page load, with the terms accepted (the welcome
  flow's answer, or the engine's persisted config), posts `bevy-shell:in-world` {login_type,
  session_id} to the shell, the one message that leaves the page for anyone but the engine. On decentraland.org
  only, never for a `preview=true` page, once per tab session, the shell then loads the Google
  tag for the Explorer GA4 stream and sends `bevy_first_move_in_world`, the Google Ads conversion,
  with the user property `login_type` `guest` or `web3`, the event param `dcl_session_id` (the
  Segment session), and as its page the url the tab arrived
  with, keeping only its `utm_*` and ad click ids (`shell/googleTag.ts`). Nothing of Google's loads before that moment.
  Once gtag.js is up the shell posts GA4's ids back (`bevy-shell:google-tag` {client_id, session_id}),
  and the app sends them as `Google Tag Linked` {ga_client_id, ga_session_id (null when GA4 gives
  none)}: the GA4 export's `user_pseudo_id` / `ga_session_id` on the Segment session, taken at the
  conversion (`shell/googleTag.ts` `onInWorld`, `listenForGoogleTag` over `lib/shell.ts`
  `onShellMessage`).
- Tests: `library/explorer-core/test/analytics.test.ts` (stamping, the handoff, the rejected send, `Google Tag Linked`),
  `library/explorer-core/test/analytics.boot.test.ts` (the boot events, `Install Attribution`),
  `library/ui/apps/hud/test/hosted/analytics.session.test.tsx` (the sequences above), `engineDriver.test.ts` (the session
  id at launch), `entryParams.test.ts` (campaign params).

## Photo mode

The camera reel's in-world camera (`features/photoMode`, bridge `domains/photoMode.ts`). The engine
owns the camera, the capture and the upload and reports them on the `photoMode` stream; the HUD
owns the mode as a surface. Expectations:

- **Opening and closing** go through the session: the `PhotoMode` key (a `systemAction`, so the
  dispatcher's popup, editor and lobby gates apply), the rail's Camera button and the overlay's
  Close and Gallery send `setPhotoMode` with its source (`shortcut` or `button`, the engine's
  analytics). The gallery key and button close photo mode on the way to the gallery; Escape is the
  overlay's cancel layer. Opening closes every page and panel.
- **Focus**: photo mode is part of `uiFocus`, so scenes get no input while it is on and the engine
  reads only the camera's own keys above that. A popup opening on top of it closes it; a scene
  permission prompt waits until it ends (`App.tsx`).
- **The overlay** replaces the chrome (no rail, minimap or chat): the frame at 0.87 of the tighter
  screen dimension in 16:9, the thirds grid, Gallery / Shoot / camera controls under it, the camera
  controls panel whose keys follow the bindings (scene movement actions included), and the
  storage-full notice in Shoot's place. Shoot is disabled while a photo is in flight, the storage
  is unknown or the capture effect plays; a capture plays the flash once per event seq and the
  photo flies into the Gallery button.
- Tests: `library/ui/apps/hud/test/hosted/photoMode.test.tsx` (the session contract above), `library/ui/apps/hud/test/hosted/photoModeOverlay.test.tsx`
  (the overlay's buttons, bindings, notice and effect); the e2e camera scenarios
  (`apps/bevy-explorer/e2e/specs/camera.test.ts`) drive the real engine.

## Gallery (camera reel)

The Gallery page and the passport's Photos tab (`library/ui/apps/hud/features/gallery`, bridge `domains/gallery.ts`).
Expectations:

- **Paging**: a reel is read 100 photos a page, the next page when fewer than 12 loaded photos remain
  past the last one on screen, and paging stops once the reel's total is loaded or the service sends
  a short page (counted before url-less images are dropped) (`useReelList`). Every Gallery open reads
  again from the top. Your own reel goes through the bridge, signed (private photos included), its
  replies matched by `requestId` (`bridgeCall`). The passport reads the PUBLIC reel from the HUD
  itself, unsigned (`publicReels.ts`), since every bridge request is signed; `?mock=1` swaps in the
  mock bridge's reels.
- **Failures**: a failed first page shows an error with RETRY, never the empty reel; a failed later
  page ends the grid with a RETRY row, and paging waits for it instead of asking on every scroll.
- **The grid** (`ReelGrid`) mounts only the rows near the viewport; it reports what is on screen only
  once it has measured it. Gallery: 6 x 306x230 with month headers; passport: 4 columns at 272:201,
  no headers. A thumbnail is disabled until its image has loaded.
- **The options menu** (`design/AnchoredMenu` around `ContextMenu`): Set as Public, Share on X, Copy
  Link, Download, Delete, in that order (`reelActions.ts`, `useOwnReelActions.ts`). The passport offers
  it only on your own passport, and making a photo private there drops it at once (back in place if
  the write fails). It is portaled with `--ui-scale`, opens below-right of the ⋮ and flips or clamps
  at the screen edges: check it visually near the edges. On native, Download needs the engine's CEF
  download handler (`crates/cef_offscreen`): it saves to `Downloads` under the home folder and
  reports how each save ended, and the toast waits for that report. That toast shows the folder,
  which opens in the file manager on a click (once per toast); web has no such link. Share on X on native asks the
  bridge scene to open the post in the user's browser (`openExternalUrl`, `https:` only); on web
  it opens a tab.
- Tests: in `library/ui/apps/hud/test/`, `hosted/gallery.test.tsx` (the session calls),
  `hosted/gallery.page.test.tsx` (paging, failures, the menu and its actions),
  `profile.passport.test.tsx` (passport photos) and `reelGrid.test.tsx` (rows, virtualization, menu
  placement); here, `src/test/bridgeGallery.test.ts` (the bridge's requests and replies).

## Donate

Tipping a place's creator in MANA, where the host can sign a tip (`@hud-host`'s `donations`: the Sites `/play`
web shell; null here, so this HUD shows no Donate). The shared HUD names a place and nothing else
(`features/donate/useDonate.ts`); the shell resolves the creator, draws the panel and signs (`apps/ui3/src/play/shell/donate`,
[ADR](../../../../docs/adr/2026-11-12-play-shell-signs-tips.md)). Expectations:

- **Where it shows:** the minimap's header for the scene the player stands in (the session's
  `minimap.donatePlace`: a Genesis City parcel or a World, none in a preview or a local realm), and the
  map's place panel. Only when the host says the session may tip and the place has a creator; hidden while
  the first answer is pending. Whether the session may tip is asked once per sign-in. The minimap's place
  changes with the scene, not with each parcel crossed (none on a parcel with no scene), and its last answer
  stays while the next scene is asked about; on the map a newly picked place hides it until answered.
- **After a tip** the host's `onSent` plays the `money` base emote; a `tip_received` notification opens the
  tipper's passport (`notifications/summary.ts` `tipTarget`).
- Tests: `library/ui/apps/hud/test/donate.test.tsx` (gating, open, emote, notification),
  `library/explorer-core/test/donatePlace.test.ts`; the shell's side in `apps/ui3/src/play/test/donateShell.test.ts`
  and `donateGuard.test.ts`. No visual baseline: the panel is drawn by the `/play` shell, not by `?mock=1`.
