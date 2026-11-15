# Decisions

Things chosen on purpose in a way that looks like an oversight, or that deliberately differ from
the Unity client. Check here before proposing a change in one of these areas. Each entry says
what was decided and why; the PR or issue has the detail. Add to this page whenever a review
ends in "we don't do that", so the next person doesn't have to rediscover it.

## Deliberate divergences from Unity

- **Toon MASK wearables.** Unity alpha-blends after the cutoff, so thin masked lines look
  fine there and fat and black here. Bevy stays glTF-spec-correct (MASK is binary). We don't
  match.
- **Day and night (#1467).** An explicit, stopped HUD time beats scene time; restarting the
  clock hands control back to the scene. No HUD lock while a scene controls the sky.
  `transitionMode` in scene.json is ignored. HUD and `/time` skips take the nearest
  direction; SDK `SkyboxTime` keeps its own mode. `/time` reports the effective time.
- **Scene environment components.** Skybox cloud speed accepts finite negative values for
  reverse rotation, as the public protocol specifies. Unity's environment profile at
  `c78b1f0c59545b0d3b1ff43e109fa63997267850` clamps negative speed to zero. Missing overrides
  retain this renderer's time-of-day atmosphere, lighting and quality settings; component
  compatibility does not imply identical default pixels across renderers. A sky image changes
  the visible sky while the authored color bands continue to drive procedural ambient lighting.
- **Emissive strength.** Unity never parses `KHR_materials_emissive_strength`; bevy honours
  it, spec-correctly. Content authored against Unity can glow (the Nekko cat at 65x). Dropping
  the extension is not the fix; the long-term answer is still open.
- **Sun shadows keep bevy's default bias.** Unity's `DepthBias` (1.75 or 1.56 texels, times
  URP's soft kernel radius 2.5) is a depth bias, not a normal bias, and carried over as bevy's
  normal bias it offset each lookup about half a metre on Low at 60 m, dropping thin and contact
  shadows. On the 1024 map, acne showed below a normal bias of 0.5 on Low and about 1 on High
  (Genesis Plaza, low sun), so both qualities keep bevy's 0.02 m depth and 1.8 texel normal
  bias.
- **Sun shadow map stays at 4096.** Unity's main shadow resolution is 1024. Low's single
  cascade spreads that over the whole shadow distance, and the bias is in texels, so past about
  75 m on 1024 the avatar's shadow disappeared, and on 2048 it was a smudge at the feet. 4096
  keeps it.
- **Sun shadows block 0.65 of the sun.** Unity's sun shadow strength is 1, and its Trilight
  ambient fills the shadow in. Our ambient is darker, so a full-strength shadow comes out far
  darker than Unity's. `SHADOW_OPACITY` in `shadow_softening.wgsl` stays at 0.65 until Unity's
  ambient lands, and the two get tuned together after that.
- **Sun shadows hold a floor past the shadow distance.** URP fades the shadow out completely at
  the shadow distance. We fade over the same squared-distance curve, but towards a partial
  shadow (`CASCADE_FAR_SHADOW` in `shadow_softening.wgsl`) that holds past the distance, so the
  worst-case change between near and far is smaller. Fading to no shadow looked worse about
  half the time.
- **Low shadows keep hardware 2x2 filtering.** Unity's soft shadows filter every quality. Low is
  the default preset and the filter applies per view, spot shadows included, so Low keeps
  bevy's `Hardware2x2` (one PCF sample) instead of `Gaussian` (nine) to stay cheap on weak GPUs
  and web. High uses `Gaussian`.
- **Jump marker reads the depth prepass.** Like Unity's projector, the blob (`BlobShadow.mat`)
  is projected onto the surface below, so it follows slopes, steps and ledges; a flat quad
  cut into rising ground. A depth bias does not fix that: it offsets the quad's depth test,
  so a bias that clears a metre of slope also draws it over anything that close, and the
  blob stays flat rather than lying on the ground. Bevy's own decals do not fit either:
  clustered decals are true box projectors but need bindless textures, so they do not run
  on WebGPU or macOS, and `ForwardDecal` is a flat quad with a depth fade, so it has the
  same slope limits in a softer form. The projection reads the primary camera's
  depth prepass, which the camera therefore keeps; without one the marker draws nothing.
  Keeping it costs nothing new: the camera already renders it on native and web, and it
  saves the main pass shading hidden opaque pixels. It also enables later work: decals,
  soft edges where transparent surfaces meet geometry, and GPU occlusion culling.
  The box runs from 0.1 m under the feet, so the blob never darkens the avatar, to 1 m
  below the ground under the player's centre, so ground falling away by up to 45 degrees
  across the footprint still catches it. The ramps follow the shadergraph, except that
  grounded means within 0.2 m of the ground, as for the animation, and the opacity rises
  over the next 0.2 m instead of the last metre: with the projector's 0.5 fade factor,
  Unity's ramp left the blob nearly invisible before the player landed.
- **Scene and download priority without distance buckets.** unity-explorer ranks scenes and
  asset loads by distance buckets and a bucket-indexed tick-rate table. We keep the aging
  scheduler and measure raw squared distance from the player to the nearest point of a scene's
  nearest parcel (not its corner); a scene behind the rendering camera counts four times
  farther in the tick queue (no once-a-second cap, no frozen far scenes). Request slots go to
  the head of the best-ranked queue (one per scene, wearable or emote hash, and one for requests
  with no hash): the player's scenes, then avatars' wearables and the emotes they play, then the
  other scenes, the last two by parcel ring, in front before behind, then raw distance; then
  requests no scene or avatar owns (no hash, or a hash nobody holds), oldest first; last, the
  downloads of a scene that has left.
  Scene roots and avatars register their hashes through component hooks; the publisher writes
  the values in place. See the
  [ADR](../../../../docs/adr/2026-11-02-explorer-nearest-first-priority.md).
- **Voice chat (private calls and community streams)** keeps the call statuses and guards, the
  3 × 2000 ms re-dial budget, the 1 s hang-up-before-join delay and the server's 60 s ring.
  - Don't add hold-to-talk to the call mic: `T` opens the chat. The `Microphone` key (`V`) toggles
    the call mic while a call is connected and the nearby mic otherwise. A community listener's
    `V` does nothing, since a listener can't speak.
  - Don't start a call's mic from a stored setting. It goes up on the call's first connect for
    whoever may speak (both sides of a private call, a stream's speakers). Don't turn it back on
    after a re-dial: what the user chose since then stands.
  - Don't keep a muted mic's track published. Muting unpublishes it, so no audio leaves a muted
    user. In a stream `MuteSpeakerFromCommunityVoiceChat` is still sent, so every client shows the
    same state.
  - Don't play calls at the nearby-voice volume. Calls and streams follow `Voice Chat Volume` and
    nearby players `Voice Volume`, both scaled by `Master`. Capture uses the system's default
    microphone; a device picker is next work.
  - Don't loop the call tune in the audio layer, which has no loop: the one-shot clip is
    re-triggered every 3.056 s (its length) while ringing.
  - Don't auto-subscribe the voice room. Like every room it joins with `auto_subscribe: false`,
    and the voice module subscribes to each microphone track itself.
  - Don't also play someone the user shares a voice room with (a call or a stream) through
    nearby voice: their spatial track is unsubscribed while they share it.
  - Don't skip the re-dial when the user is alone in the room. A stream's creator before anyone
    joins, or a caller whose partner is still connecting, would lose the call to a network blip.
    A call that has really ended has its room deleted, and that disconnect ends the call without
    a re-dial.
  - Don't count the user's own call room for the community chat's "Live Voice Stream" banner: it
    shows the backend's participant count for the stream (1 for a stream known only from its
    start update, until the next read). Don't let it catch clicks while the chat is unfocused: it
    is hidden, not just transparent.
  - Don't wait for an update about the user's own stream. Expected: once the user's stream starts
    it shows as live (rail icon, chat banner, sidebar badge, scene banner) with current counts.
    Actual: the social service leaves the starter out of the stream's STARTED update, and neither
    update carries counts; ENDED reaches everyone, the ender included. So the bridge re-reads the
    live list when the user's own stream is entered or left.
  - Don't let a ringing private call carry on next to a stream: joining or starting a stream
    while one rings refuses it first, and the stream starts a second later.
  - Don't leave a stream running with nobody in it: one whose start succeeds after its creator
    left (or moved to another stream) is ended with `EndCommunityVoiceChat`.
  - Don't hold a hung-up private call in an ending state. One whose id is known ends at once,
    and `EndPrivateVoiceChat` is sent behind it. With the social link down the request waits for
    the reconnect (up to 30 s of backoff), and an ending state would keep the nearby mic off and
    refuse new calls all that time. That request is not retried if it fails. A call whose start
    is still in flight at sign-out is not ended by the client either: its reply comes back after
    the account's link is gone, so the end goes out on the next account's link, where the server
    refuses it (only a party ends a call), or on none. The server ends both anyway: leaving the
    room makes the gatekeeper delete a private room, and a closed social socket ends that user's
    pending call (`social-service-ea` `logic/subscriptions/lifecycle.ts`, mirrored by the
    catalyst).
  - Don't ring a call again on reconnect. A call left ringing when the social link dropped is
    rejected on reconnect (one the user placed is ended), and the reconnect's own report of it is
    ignored: the server tells only the caller about a reject, so a second ring would never stop.
  - Don't show the "stream in this scene" banner to guests: they can't join.
- **Saved mutes for nearby voice** follow unity-explorer's: the profile menu's Hush and Hear, the
  list kept by the social service (`/v1/mutes`), a mute that holds for the session when saving it
  fails, a saved list that only adds to what the user did before it arrived, and a muted player
  silenced but still subscribed, with the hushed badge in place of the speaking bars. These differ:
  - The list belongs to the account. A change of account drops it and reads the new account's;
    unity-explorer reads it once at start and keeps it for whoever signs in next.
  - `nearby_voice_user_mute` is sent for the user's Hush or Hear only; unity-explorer also sends
    it for every mute read from the saved list.
  - Muting oneself is refused, as the service refuses it; the menu never offers it.
  - Changes are saved one at a time, in the order the user made them, and only once the saved
    list has been read: the service applies each request on its own, so a Hush and a Hear sent
    together could land in either order, and a row removed while the list is read page by page
    makes the next page skip one. unity-explorer sends each change as it is made, cancels the
    one in flight when the next is made, and does not wait for the list.
  - A saved list that cannot be read is read again from the first page after 2, 4 and 8 seconds;
    after that it stays unread until the account changes. unity-explorer repeats a single page
    request up to twice on a transient network error and does not read the list again.
  - Hush and Hear are on the profile card's default and friend menus, for a blocked user too, as
    unity-explorer's user menu has them; the request, blocked-list and community-member menus
    do not carry them.
  - Don't apply it to calls or community streams: as in unity-explorer it is nearby voice only.
- **Profiles from the centralized catalyst.** unity-explorer switches between the
  asset-bundle registry and a catalyst with `USE_CENTRALIZED_PROFILES` and reads a remote
  player's catalyst from their LiveKit metadata. We read the base-domain catalyst's lambdas:
  `POST /lambdas/profiles` for full profiles and `POST /lambdas/profiles/metadata` for the compact
  tier, which falls back to full where the route is missing, as unity-explorer forces full on a
  catalyst. No flag, and no catalyst named by another player. For now, when that catalyst is on
  `decentraland.org` or `decentraland.zone`, whose catalysts lack the compact route, compact reads
  go to that deployment's asset-bundle registry instead. This path is to be removed once those
  catalysts serve the route. See the
  [ADR](../../../../docs/adr/2026-11-04-explorer-profiles-from-the-catalyst.md).
- **Own profile versions never repeat.** unity-explorer's `SelfProfile` numbers an edit one above
  the profile it holds, so after a failed publish puts the old profile back, the next edit reuses
  the failed version. A failed deploy may still have reached the server, and a peer that read it
  skips any other profile under that number, so our next edit goes above every version this session
  gave the account (`CurrentUserProfile::bump_version`).
- **Community chat (#223).** A community packet is taken only from the server's message
  router, which checks membership and stamps the author; unity-explorer also accepts one
  straight from a peer under the peer's own identity. Community history is stored like a DM's
  and trimmed to 200 messages and 1000 records per community; unity-explorer keeps it in memory
  and persists DMs only. A community tab's input is blocked while the private chat room is not
  joined or the router is not in it, as a DM's is for an unreachable partner. Leaving a
  community (or being removed) deletes its stored history and settings, and the communities an
  account had chats for are remembered per account so a leave while logged out does the same.
  The notification ping has a default in the Chat settings tab and a per-conversation override
  with a "Default" choice; a flood of lines on one conversation is one sound (400 ms). The
  member-of list is read up to 10 pages of 100 (unity-explorer reads one), and one pair of clips
  serves focused and unfocused channels alike, played by the bridge scene from its own content.
- **Blocked users in chat history (#296).** Live chat and reactions from anyone blocked in
  either direction are dropped on every channel. Community history still stores them and
  leaves them out of each read while the block lasts, so an unblock brings them back;
  unity-explorer stores no community history. A new DM or DM reaction from them is not stored,
  since a DM history is never trimmed. A DM thread stored before the block is read back whole:
  hiding it would reveal that the other side blocked you, who is otherwise shown as offline.
- **Community creation and editing.** The form follows unity-explorer's (fields, limits, texts,
  the content check's two refusal screens, every HTTP 400 read as the check's answer), with
  these differences. CANCEL on the refusal screen asks "Discard this community?" first, as the
  form's own CANCEL does; unity-explorer closes at once. A refused edit says "Your Community
  Couldn't Be Updated"; unity-explorer says "Created" for edits too. A failed delete shows its
  message on the community card, like the card's other actions, instead of as a toast. A
  picture saved by an edit is kept by the HUD for the session, as unity-explorer's image cache
  does, since its URL does not change and the CDN (CloudFront on .org) ignores a version query.
  The places picker links the LANDs and Worlds the user owns or operates, as unity-explorer's does.
  An edit sends `placeIds` only when the linked places changed, and the service keeps them when the
  field is absent; unity-explorer sends them on every save. When the community's linked places
  could not be read, the picker and its remove buttons are disabled, so a save cannot replace them
  with a partial list; unity-explorer leaves them editable. Linked places the places service does
  not return (a disabled one, say) are kept on every save without being shown, and the community's
  places are read a page at a time; unity-explorer reads one page and would unlink the rest. As in
  unity-explorer, the picker also lists LANDs the user only operates, which the service refuses to
  link: the save fails with the generic error. That a moderator can unlink only the places they own
  is a HUD rule, as in unity-explorer; the service checks ownership only of places being added.
  On the card's Places tab, owners and moderators get unity-explorer's ADD PLACE tile (it opens the
  edit form) and delete on the places they own, asked first. A failed delete shows its message on
  the card, like the card's other actions, instead of as a toast. unity-explorer's variant of the
  PLACES tab button for owners and moderators is not ported.
  On the card's Members tab, a row opens the member's passport and carries unity-explorer's
  friendship buttons; its ⋮ opens unity-explorer's member menu: View Profile, Chat, Call, the
  owner's and moderators' actions (Demote or Promote Moderator, Make Community Owner, Remove
  Member, Ban) with unity-explorer's rules and confirmations, then Block and Report, then Invite
  to Community. Gift is left out until gifting exists (unity-explorer shows it only behind its
  gifting flag). Chat is always offered in-world: unity-explorer greys it
  only while its chat area is down. unity-explorer's row shows no presence, so neither does
  this one; "N Mutual Friends" is not shown because no members API returns the count. A failed
  action shows its message on the card instead of as a toast, and the tab lists the first 100
  members without unity-explorer's paging.
  The Communities page's browse grid is unity-explorer's: 20 communities a page, the next read as
  the grid's end comes into view, the name searched on the server a second after typing stops (at
  once on Enter), and each card's Public or Private, Join, Request to Join or Request Sent (Cancel
  Request on hover, on the card too), its description on hover, and the first three of the user's
  friends in it. A sent or cancelled request shows at once, before the community is read again,
  and a failed join or request from the grid is toasted. Whether a join request is
  pending comes from the user's own join requests, read when the grid's first page loads and when a
  closed community's card opens, and kept from the request's answer, as unity-explorer's. The
  card always shows its member count; unity-explorer shows it only behind its members-counter
  flag. Live streams show as a card's LIVE chip and the grid's Streaming filter, not as
  unity-explorer's separate Voice Streaming section (for an invited card, see Community invites
  below). A failed read shows unity-explorer's error toast
  and leaves the grid empty, with no count, where unity-explorer leaves its spinner running. A
  card shows no spinner while its join or request runs. My Communities is every membership, read
  apart from the grid; when that read fails the list stays as it was and the page asks again the
  next time it opens, without unity-explorer's "error loading My Communities" toast.
- **Community invites and join requests.** As unity-explorer: the browser's Invites & Requests
  opens Invites, Requests Received (only for a user who owns or moderates a community; one group
  per community, each request with ACCEPT, REJECT and the member menu without owner actions) and
  Requests Sent, each titled with its count, and "No pending invites or requests." when all are
  empty. Its counter is the invites plus the requests received, and Requests Received's title
  counts the same totals. Unlike unity-explorer, opening the page reads only the counter (each
  read's total, one row asked for); the section is read in full when it opens, and again after
  any answer or cancel and when an invite or request notification arrives while the page is open
  (the counter alone until the section has been opened). A section that cannot be read says so,
  with RETRY. Only private communities are read for join requests, a
  community's first 100 (the service's page cap); one whose read fails is left out, and the list
  fails only when none could be read. A card the user is invited to offers Accept and Reject
  (asked first) instead of Join in the grid, and ACCEPT INVITE and REJECT INVITE on the community
  card. The card's Members tab gives owners and moderators MEMBERS, BANNED (with UNBAN), JOIN
  REQUESTS (private communities, with its counter) and INVITED (with CANCEL). Invite to Community,
  in the user menus, the member menus and the passport but not the voice panel's, lists the
  communities the user may invite the person to, is hidden when there are none, and is read again
  after an invite. It opens beside its row, on the left or upward where there is no room, and from
  the keyboard (a keyboard click on the row focuses its first community, Tab moves through them);
  Cancel closes it before the menu around it. Clicking an invite, a request received or a request accepted notification opens the
  community's card, and every opening of a card reads its community again, so an invite or an
  accepted request that arrived since shows. An accepted request also reads My Communities, the
  grid and that open card again. Call in the member menu closes the card first, as Chat does.
  Answers and an unban are toasted when they fail. A request takes one answer at a time, released
  when that answer settles, and a request answered here never comes back from an older read.
  Differences: a Members section that cannot be read says so, with RETRY (unity-explorer keeps
  what it showed and tries again on the next read, so a first read that fails looks empty); each section reads its first 100 rows without paging, as the Members list does;
  the section shows a spinner, not unity-explorer's skeleton cards, while it loads; and a row's
  buttons are disabled, with no spinner, while its answer runs.
- **One answer to "banned or blocked?".** The `ban_block` crate is a facade: it reads the
  social client's block sets and comms' scene admission and scene-room lists through one
  `BanBlock` system parameter and owns only the platform-ban check, where unity-explorer keeps
  `IUserBlockingCache`, `ApplicationBlocklistGuard` and `RoomMetadataCurrentScene` apart. A
  blocked pair is blocked in either direction, as there. The last block sets stand across a
  social reconnect until the next snapshot (unity-explorer also resets only on the next
  `GetBlockingStatus`). The platform ban is read from the gatekeeper's public, unsigned
  `GET /users/{address}/bans` (unity-explorer signs the same request, which the endpoint does
  not need), on sign-in, on a realm change, on `/ban_block_refresh` and, as a periodic chance to
  notice a ban while the client idles, every 300 s (unity-explorer checks at start-up only; no
  push reaches an idle client here, and the period promises no freshness, as a failed check
  fails open, as there, and the state says so instead of guessing); a ban whose record the
  gatekeeper withholds (a device match) is still a ban. The endpoint matches the address only,
  so a device ban shows only as a platform-scoped refusal of the scene adapter: that refusal
  marks the platform ban, and the endpoint's "not banned" does not lift it while the refusal
  stands. The local user's scene ban is the refusal of the
  scene adapter, read from its status and body (the gatekeeper's 403 for a platform or scene
  ban, the worlds server's 401 for a platform or world ban), where unity-explorer treats any 401
  or 403 as forbidden access. The player is not moved out of a scene that refused them, as
  there. Other players' scene bans and
  the admins come from the room's metadata and count only while the room is connected, as there.
- **Pulse only.** unity-explorer falls back to LiveKit-only after five failed start-up attempts,
  through dual-transport proxies and LiveKit broadcast targeting. Here Pulse is the only
  avatar-state transport and none of that exists, so a retryable failure is retried without limit.
- **Pulse refusals set a floor under the next wait.** Server full (disconnect code 6) waits at least
  60 s, the pre-auth limits (7, 8) and the per-IP connection cap (17) at least 10 s, the per-IP
  scene-listener cap (18) 10 s doubling to 5 minutes, and all five are retried, as the Pulse
  server documents. unity-explorer applies its 10 s and 60 s waits only to a session that was
  already accepted, and ends Pulse on codes 17 and 18, which it does not know (`reconnect.rs`).
- **A rejected Pulse handshake is retried, and a realm change ends a terminal stop.** A rejection,
  or no answer within 10 s, closes the connection and the next attempt is a new one on the
  backoff; a ban, an eviction, misbehaviour or an unknown code stops Pulse until the realm changes,
  or for a scene listener, which follows no realm, until restart. unity-explorer stops Pulse on a
  rejection and on those codes until a new sign-in, which has no counterpart here: the account is
  fixed once in the world. It has no response timeout: it waits, and the server's pre-auth
  deadline ends the attempt with AUTH_TIMEOUT, which it retries.
- **Teleports and emotes are paced.** The client lets out 10 a second with a burst of 8, half of
  what both servers allow for them, and keeps only the latest teleport and the latest emote command
  while that is spent. Overrunning the servers' budget ends the session for good, and a scene that
  repositions the player every frame would do it; unity-explorer has no limiter (`pacing.rs`).
- **IPv6 when no IPv4 address resolves.** The native driver connects to the first IPv4 address and
  falls back to the first IPv6 one, on an IPv6 socket, only when there is none. unity-explorer
  fails without an IPv4 address, and the servers bind IPv4 by default.
- **One Pulse subject per wallet.** A newer join for a wallet replaces the subject it held: the
  avatar snaps to the newcomer, or leaves the older subject's realm when the newcomer joined in
  another. unity-explorer keeps the older subject and promotes it if the newer one leaves. Both
  servers evict a wallet's older session once the newer one authenticates, so the older subject
  is always a session already gone, and promoting it would show its last pose and ask for a
  resync the server never answers. One gap is accepted: the .NET server keeps the older session
  visible for up to 5 s after the eviction, so an observer that first sees both then can hear the
  older join last. The avatar then follows the dead session and leaves with it, and the live one
  shows again only once it re-enters that observer's view. dcl-one's server removes the older
  session before the newer one is visible, so it never happens there.
- **Pulse reconnect waits are jittered.** Every wait, from the 3 s after a drop to each backoff
  step and refusal floor, is scaled by a random factor between 0.8 and 1.2; unity-explorer's waits
  are exact. Clients that fail together, every client of a restarting server or many behind one
  address, would otherwise come back at the same instant. The servers hold at most 512 connections
  waiting to authenticate, 32 per address, so the excess is refused (codes 8 and 7), and the 10 s
  floor would send every refused client back together again. The Pulse server's documentation
  calls jitter mandatory for those codes.
- **Scene raycasts are capped only outside the scenes the player is in.** The system scene, every
  portable and the scenes the player stands in run all their raycasts every frame, with no cap;
  every other scene shares a tenth of the target frame (1.67 ms at 60 fps, 0.69 ms at 144),
  nearest scene first, and what does not fit waits for a later frame. unity-explorer instead caps
  the current scene at 3 ms and runs no other scene's raycasts. We differ because scenes drive
  frame-sensitive logic from ray results: `movement-scene`, the default avatar movement driver,
  runs its ground and wall checks on continuous `RS_AVATAR` rays, and for it a slower frame is
  better than a late answer. A share of the target frame, not a fixed time, keeps the cap the same
  part of the main thread at any frame rate. See [Interaction](interaction.md#scene-raycasts).

- **Photo mode follows the engine's cursor convention.** unity-explorer's in-world camera has its
  own Lock/Unlock cursor actions. Here right-click toggles the lock as everywhere else, left-drag
  looks while the cursor is free (Unity's drag), and Escape closes photo mode. The gallery key (G,
  Unity's K) closes photo mode and opens the gallery, where Unity's K only closes it.
- **Photo mode has no gamepad toggle, and its pan is on the D-pad.** unity-explorer opens the
  in-world camera with the pad's East button and pans with the bumpers (and `dpad/y`). Here East
  is the scenes' secondary action and nothing is reserved outside photo mode, so a pad binding
  would open the camera from inside a scene's own action; the toggle is C, the rail's Camera
  button and `/photo_mode`, like the other HUD hotkeys. Inside photo mode the bumpers already zoom
  (the player camera's binding), so pan is D-pad up/down.
- **Photo mode is not feature-flagged.** unity-explorer gates the camera reel behind
  `alfa-camera-reel`; the Gallery already ships ungated here and photo mode does the same.
- **A photo's `screenRect` is sent and dropped.** Each visible person carries the rectangle they
  cover in the saved image, as unity-explorer computes it; the catalyst's camera-reel service parses
  the metadata into a struct without that field, so it is not stored. Kept for parity with the
  upstream service's clients.
- **The photo camera stops at scene geometry instead of sliding.** unity-explorer moves a capsule
  character controller, which slides along walls; here the move is swept and refused whole when
  the camera's sphere would cross a scene collider. A collider the camera starts inside (the player
  camera has no wall avoidance, so photo mode can open in one) does not block it, so it can leave.
- **No camera-to-player dither in photo mode.** unity-explorer keeps fading what stands between the
  camera and the avatar; here the fade showed in the photos, so the dither depth is zero while the
  mode is on and the player camera's depth comes back when it closes.
- **The photo camera looks through the player camera's input path.** unity-explorer turns it by a
  rotation speed times the frame time, which depends on the frame rate and ignores the sensitivity
  setting; here the pointer delta goes through the player camera's sensitivity and smoothing
  settings, scaled by 0.17, which matches the Unity feel at 60 fps.
- **The photo camera's wheel zoom is per line, and a held zoom button zooms per second.**
  unity-explorer scales the scroll delta by 50 everywhere but macOS, per frame; here a wheel line
  (16 pixels of a trackpad's scroll, on every platform including web) zooms as Unity's 60 fps frame
  would, and a held bumper zooms at Unity's per-second rate, so neither depends on the frame rate.
- **Smart wearable permissions are asked on use, not at equip.** unity-explorer asks once, when a
  smart wearable is equipped (or at login for one already worn), to allow the permissions its
  `requiredPermissions` declares, and refuses what it did not declare. Here a smart wearable is a
  portable like any other: each restricted action goes through the stored rules and the per-use
  prompt, which shows what is being done (the URL to open, the realm to change to). So
  `requiredPermissions` is ignored, and nothing is asked before the scene starts. On native,
  `fetch` and WebSocket are asked per use like the rest (`dcl_deno`); on web they are not
  permission-gated for any scene (scene code can reach the page's raw `fetch`), so a
  declared-permission check on them would be bypassable.
- **`portableExperiences: disabled` pauses every listed portable.** unity-explorer unloads smart
  wearables while the player stands in a scene that disables portable experiences and loads them
  again on leaving. Here every portable the player can see (not startup ones, not the HUD's scene)
  is paused through `RendererSceneContext.blocked` and its UI is hidden, so it keeps its state
  across the crossing and nothing has to remember a portable that is not running.

- **Donate is on `/play` only, and for web3 sign-ins only.** unity-explorer offers it to any
  signed-in player through its wallet. Here only the Sites `/play` web shell holds a wallet that can
  sign a transfer, so the Explorer's own HUD shows no Donate. MANA is shown to web3 wallet
  sign-ins only (AGENTS.md), so an in-app (managed) sign-in sees no Donate either, and the shell
  refuses one that asks. See [the ADR](../../../../docs/adr/2026-11-12-play-shell-signs-tips.md).
- **The Donate panel is drawn by the web shell, over the app.** The amount is confirmed where the
  wallet is, and the shell resolves the creator itself; the HUD names the place only.
- **An unclear tip outcome is shown as processing, with no Try again.** unity-explorer's error
  view always offers Try again. Here a failure that may have reached the chain (the relay's answer
  lost or a 5xx after signing, a transaction not yet mined) shows the tip as processing and watches
  it; Try again appears only after a clear failure (the wallet declined, nothing was signed, the
  transaction reverted), so a player never tips twice. A tip still unconfirmed after 30 minutes
  stays "still being processed" with no retry, and opening Donate again shows it until it settles.
  There is no fallback to a transaction from the wallet: the relay's refusal fails the tip, and
  trying again signs the same nonce (pinned), so at most one can land; once that nonce is used, the
  next send signs nothing and watches the failed attempts until one is seen mined.
- **The MANA price comes from the deployment.** unity-explorer reads the MANA/USD rate from
  CoinGecko; the panel reads the catalyst's price service (`/price/api/v3/simple/price`), which
  answers in the same shape.
- **The creator is the scene's deployer.** The places service's `creator_address`, which the tip
  goes to, is the deployer of the place's scene here.
- **Donate's buttons.** The minimap's uses unity-explorer's tip icon, beside the collapse button;
  the map's place panel has a DONATE text button under JUMP IN, whose art is not ported. The fourth
  amount choice reads OTHER, and the minimum reads "Tips must be ≥ 1 MANA".
- **Events page: JUMP IN closes the page and the event detail.** unity-explorer leaves both open
  while the player travels. Here a jump reconnects the realm under the loading screen, and every
  other page closes on a jump, so Events does too.
- **Events page: its links carry the HUD's `utm_source`.** unity-explorer tags them
  `utm_source=explorer`; here they carry `bevy-web-client` or `bevy-desktop-client` like every
  other link out, and keep `utm_campaign=discover`. Event names and descriptions are URL-encoded
  in the share and calendar links, and so are the world, the referrer and the event id in the
  jump and event links; unity-explorer leaves all of them raw.
- **Events page: JUMP IN travels to a world only by its name.** The events service takes any
  text as an event's `server`, and the engine reads anything but a world name as a realm's
  address. unity-explorer builds the worlds server's address from it; here a `server` that is
  not a `.eth` name does not travel, and a toast says the world cannot be reached.
- **Events page: the calendar never steps before today.** unity-explorer steps back by the days
  shown and locks the arrow only on today itself, so after the highlighted events come or go,
  or past midnight, it reaches earlier days. Here a step back stops at today.
- **Events page: a recurrent date's offset keeps its minutes.** unity-explorer writes whole
  hours, `(UTC+5)` in a `+5:30` zone; here it reads `(UTC+5:30)`.
- **Events page: a failed read shows an error with Retry.** unity-explorer shows a toast and
  leaves the loading placeholders up. A read the bridge has not answered in 10 seconds counts as
  failed (the limit `library/explorer-core/lib/shell.ts` gives a request); an answer that arrives later still shows,
  and so does a reminder the service confirms late.
- **Events page: one copy of an event.** unity-explorer changes the reminder on the card's own
  copy, so a card scrolled out and back shows the old state; its day view's card buttons are not
  wired, and its banner is drawn before the friends list arrives. Here the calendar, the day
  view, the banner and the detail read one event, every card's buttons act, and friends show
  when they arrive.
- **Events page: analytics.** unity-explorer reports `event_card_clicked` from calendar cards
  only and nothing for an open from the sidebar; here a day view's cards report it too, and the
  sidebar reports `events_section_opened` with `source: sidebar`.
- **Events page: no "error loading friends" or "My Communities" toast.** The friends list and
  the member-of list reach the HUD as snapshots with no failure signal, so there is nothing to
  report; cards show friends and community marks once the lists are there.
- **Events page: shapes are CSS, icons are unity-explorer's.** Its rounded containers, outlines
  and gradients are nine-slice sprites there; drawn as masks at the HUD's fractional scale they
  show seams between the slices, so here they are radii, borders and gradients with the sprites'
  colours. The canvas scales by the smaller of width and height, as every menu page does, where
  unity-explorer's scales by width.

## Deliberate choices that look like mistakes

- **`UnapprovedPathMode::Allow`** in the asset server: bevy's check only admits paths under
  the executable's assets directory and the IPFS cache lives elsewhere. Containment is done
  in `ipfs_path.rs` instead. See the security page.
- **Web-only engine switches are page params**, not `LaunchOptions` fields.
- **`CommsApi.consumeMessages` resolves to a bare array** of `{ sender, data }`, not the
  protocol's `{ messages }`, because unity-explorer, the client scenes are tested on, returns
  the array. Changing it to the protocol's shape would break those scenes here.
- **A scene's messages use the scene room that carries the scene.** The message bus and topic
  messages, sent and received, and the `isConnectedSceneRoom` flag in `RealmInfo` all ask
  `comms::scene_rooms::SceneRooms`. A message sent while that room is connecting or reconnecting
  waits in the room's channel and goes out once it is connected; with no room that carries the
  scene it is dropped. The flag is true only while that room is connected. A client's room carries
  the scene the player is in. In a local preview or a world with one scene it carries every scene
  of the realm.
- **Native HUD preferences are persisted by the engine** (`hud-prefs.json`), because CEF's
  `localStorage` does not survive.
- **Web is Chrome only.**
- **The end-to-end tests use no Playwright.** They drive the client through its console on both
  shells, so one spec runs on the web and the standalone, and what they need from a browser (real
  input, pages outside the client such as sign-in, screenshots) the in-repo web driver does
  natively over the DevTools pipe; no Puppeteer, WebDriver or Synpress either. react-web's own
  Playwright tiers and the scene-kill harness are outside this decision and are not supplied
  in this public checkout. See the
  [ADR](../../../../docs/adr/2026-10-29-explorer-tests-through-the-console.md).
- **The automation surface ships in every build.** The console commands, the HUD and input
  commands natively, and the loopback `dcl/console` server behind `--automation-token` are the
  client's agent surface, so people's own agents drive the deployed clients instead of rebuilding
  the plumbing. The preview-only commands stay preview-only. Same ADR.
- **Collectible cache retention must be generic** across wearables and emotes, and an emote's
  ownership is its membership of the avatar's animation graph. A pruning PR that special-cases
  emotes (#1323) is parked for that reason.
- **Camera layers are identified by entity**, not by an explicit layer number; the cleanup in
  #1320 waits on the protocol change.
- **`-logFile` with one dash is accepted** (rewritten to `--log-file` before clap, `main.rs`):
  the benchmark harness spells it that way. See the benchmark page.
- **The benchmark log carries markup a Rust log wouldn't**: `<color=#…>` tags and a
  `DebugLogReportHandler:LogExceptionInternal` line in every exception block, the shapes the
  harness's parser counts exceptions by. Every error-level event counts as an exception.
- **The benchmark warm-up arrives in world.** A launch without a CSV destination signs in,
  loads and only then ends, so the stored guest and the caches exist before the measured runs.
- **The benchmark guest is local.** Under `--autopilot` the guest key is made on the machine
  (or by the shell on web) and kept, instead of a thirdweb guest wallet: thirdweb refuses
  `localhost` origins, and a kept root key lets an expired identity be signed again with the same
  address. Only that guest signs in under `--autopilot`: an account stored on the machine is
  never used by a benchmark run, where scenes may fetch and sign requests without a prompt. The
  engine ends a run signed in as anything else with exit code 1, and a kept key that cannot be
  read fails the run instead of becoming a new account.
- **The benchmark's GPU time leaves out queued uploads and standalone submits.** Two empty
  compute passes bracket the frame as the first and last nodes of the render graph, so both
  timestamps reach the GPU in the frame's own submission. A bracket submitted on its own ahead of
  the frame also counts the render world's CPU time up to the submission, which on a CPU-bound
  frame is most of the sample (measurements (evidence supplied for the task)).
  See [GPU time](benchmark.md#gpu-time).
- **Bevy stays on the 0.16 fork for now.** Security-relevant dependencies (V8 and deno,
  libwebrtc, ffmpeg, wgpu) get bumped inside it. Whether to ever move the bevy version is an
  open discussion, not a decision.
