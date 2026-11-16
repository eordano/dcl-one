// Wire protocol between the React page and the super-user "bridge" scene.
//
// Transport is a same-origin BroadcastChannel (exposed to the --ui super-user
// scene only — see bevy-explorer deploy/web/sandbox_worker.js). Every message is
// wrapped in an addressed Envelope so each side ignores its own posts.
//
// Domain types mirror scene/src/bevy-api/interface.ts so the bridge scene can
// forward SystemApi results verbatim.

import type {
  Color3,
  CommunityVoiceStatus,
  ConnectivitySnapshot,
  HomeScene,
  PortableInfo,
  SatelliteView,
  SceneLoadingUi,
  VoiceChatAction,
  VoiceChatState,
  WelcomeState,
} from './generated'
import type { AvatarLook } from './avatarEquip'

export const BRIDGE_CHANNEL = 'bevy-ui-bridge'

/**
 * The channel name to actually open: BRIDGE_CHANNEL plus a per-boot session suffix.
 * BroadcastChannel is same-origin across ALL tabs, so the bare name lets one tab's HUD drive
 * every tab's bridge scene (issue #1089). The id lives on `window.__bridgeSession` — lazily
 * created by whichever page-side caller touches it first (this helper, or the e2e spy's init
 * script). engine.js only FORWARDS it to the scene sandbox, which appends the same suffix
 * (deploy/web/engine/sandbox_worker.js) — it never generates one, so embedders that share the
 * bus across documents (creator-hub's inspector) keep bare names. EngineHost calls this before
 * booting the engine, which is what opts this page in.
 */
export function bridgeChannelName(): string {
  // Structural globalThis, not window/crypto: this module is also compiled into the bridge
  // scene's program (type-only imports), whose tsconfig has no DOM lib. (The scene never calls
  // this at runtime — its suffix is applied by the sandbox's injected BroadcastChannel wrapper.)
  const g = globalThis as { __bridgeSession?: string; crypto?: { randomUUID?(): string } }
  // The id needs uniqueness, not unpredictability — Math.random covers insecure contexts
  // (plain-http LAN testing), where crypto.randomUUID is absent.
  g.__bridgeSession ??= g.crypto?.randomUUID?.().slice(0, 8) ?? Math.random().toString(36).slice(2, 10)
  return `${BRIDGE_CHANNEL}#${g.__bridgeSession}`
}

/** Mirrors SystemApi.getPreviousLogin(): userId is absent for a fresh user. */
export interface PreviousLogin {
  userId: string | null
}

/** Mirrors SystemApi.loginPrevious() result. */
export interface LoginPreviousResult {
  success: boolean
  error: string
}

/** Simple promise-returning SystemApi calls the login slice needs. */
export type RpcMethod =
  | 'getPreviousLogin'
  | 'loginPrevious'
  | 'loginNew'
  | 'loginGuest'
  | 'loginIdentity'
  | 'loginCancel'
  | 'logout'
  | 'guestLogin'
  | 'guestSign'
  | 'guestDiscard'

// ---- page -> scene ---------------------------------------------------------

export interface RpcRequest {
  kind: 'rpc:req'
  id: string
  method: RpcMethod
  params?: Record<string, string>
}

/** Send a chat message (page → engine via the scene's BevyApi.sendChat). The channel is 'Nearby',
 *  the DM partner's wallet, or a community's `community:<id>`. */
export interface SendChatRequest {
  kind: 'sendChat'
  message: string
  channel: string
}

/** Add or remove the local user's reaction to a chat message (page → engine via the scene's
 *  BevyApi.sendChatReaction). The engine echoes it back as a ChatReactionMessage. */
export interface SendChatReactionRequest {
  kind: 'sendChatReaction'
  /** 'Nearby', the DM partner's wallet, or `community:<id>`. */
  channel: string
  messageId: string
  emoji: string
  remove: boolean
}

/** Redraw a Nearby chat bubble with other words (its translation), if that message is still the
 *  sender's bubble; its timer starts again. */
export interface ChatBubbleTextRequest {
  kind: 'chatBubbleText'
  sender: string
  messageId: string
  message: string
}

/** Reload the current scene(s) — the `/reload` chat command (scene calls SystemApi.reload). */
export interface ReloadSceneRequest {
  kind: 'reloadScene'
}

/** Run an engine console command (chat `/commands` → `help`, or any passed-through `/command`);
 *  the scene answers with a `consoleReply`. */
export interface ConsoleCommandRequest {
  kind: 'consoleCommand'
  command: string
  args?: string[]
}

/** The console's reply to a `consoleCommand` (scene → page): its text output, or the failure
 *  text (`ok: false`, e.g. the engine's unknown-command rejection or a clap usage error). */
export interface ConsoleReplyMessage {
  kind: 'consoleReply'
  command: string
  args: string[]
  ok: boolean
  output: string
}

/** Sidebar nav actions the React sidebar triggers in the scene (open a menu/popup,
 *  toggle emote wheel / mic) until those panels are themselves migrated to React. */
export type NavAction =
  | 'map'
  | 'settings'
  | 'backpack'
  | 'communities'
  | 'friends'
  | 'profile'
  | 'notifications'
  | 'emotes'
  | 'mic'
  | 'support'

export interface NavActionRequest {
  kind: 'navAction'
  action: NavAction
}

/** Page→scene handshake ping, repeated until the scene answers `bridgeReady` (see BridgeChannel). */
export interface HelloRequest {
  kind: 'hello'
  /** The page's directory: the bridge scene keeps the first one it hears to recognise the page's own realms. */
  pageDir?: string
}

/** Scene→page: every domain is registered and the scene is listening. */
export interface BridgeReadyMessage {
  kind: 'bridgeReady'
}

/** Synthesised BY THE PAGE (never sent by the scene) when the bridge never answered. */
export interface BridgeUnavailableMessage {
  kind: 'bridgeUnavailable'
}

export type PageToScene =
  | HelloRequest
  | { kind: 'getConnectivity' }
  | { kind: 'retryConnectivity'; requestId: string }
  | VoiceChatActionRequest
  | RpcRequest
  | SendChatRequest
  | SendChatReactionRequest
  | ChatBubbleTextRequest
  | ReloadSceneRequest
  | ConsoleCommandRequest
  | NavActionRequest
  | FriendActionRequest
  | DmWatchRequest
  | DmHistoryRequest
  | DmDeleteRequest
  | GetMutualFriendsRequest
  | GetSettingsRequest
  | SetSettingRequest
  | GetBindingsRequest
  | SetBindingsRequest
  | ResetBindingsRequest
  | CaptureInputRequest
  | UiFocusMessage
  | GetProfileRequest
  | GetUserProfileRequest
  | SaveProfileRequest
  | GetOwnedNamesRequest
  | GetNotificationsRequest
  | MarkNotificationsReadRequest
  | GetEmotesRequest
  | TriggerEmoteRequest
  | EquipEmoteRequest
  | SetMicRequest
  | SetNearbyMuteRequest
  | GetWearablesRequest
  | CatalogQueryRequest
  | EquipRequest
  | CommitAvatarRequest
  | RevertAvatarRequest
  | PreviewAvatarRequest
  | PreviewFocusRequest
  | PreviewStageRequest
  | PreviewEmoteRequest
  | PreviewRotateRequest
  | PreviewZoomRequest
  | GetOutfitsRequest
  | SaveOutfitRequest
  | DeleteOutfitRequest
  | EquipOutfitRequest
  | RandomizeOutfitRequest
  | UndoOutfitRequest
  | RedoOutfitRequest
  | ResetOutfitHistoryRequest
  | SetAvatarColorRequest
  | SetForceRenderRequest
  | GetCommunitiesRequest
  | BrowseCommunitiesRequest
  | GetCommunityRequest
  | CreateCommunityRequest
  | UpdateCommunityRequest
  | GetCommunityPlaceOptionsRequest
  | RemoveCommunityPlaceRequest
  | CommunityMemberActionRequest
  | DeleteCommunityRequest
  | JoinCommunityRequest
  | LeaveCommunityRequest
  | RequestToJoinCommunityRequest
  | CancelJoinRequestRequest
  | ManageCommunityRequestRequest
  | UnbanCommunityMemberRequest
  | GetCommunityInvitesRequest
  | GetCommunityInvitesCountRequest
  | GetCommunityMemberSectionRequest
  | GetInvitableCommunitiesRequest
  | InviteToCommunityRequest
  | GetCommunityDetailRequest
  | GetCommunityChannelsRequest
  | SetCommunityChannelsRequest
  | GetCommunityOnlineMembersRequest
  | GetEventsRequest
  | SetEventAttendingRequest
  | GetEventPlacesRequest
  | PlayUiSoundRequest
  | GetMapRequest
  | GetHomeSceneRequest
  | GetWelcomeRequest
  | ExpectLookRequest
  | ShowLookRequest
  | PreloadLookRequest
  | AcceptWelcomeRequest
  | OpenLegalRequest
  | OpenExternalUrlRequest
  | TeleportRequest
  | ChangeRealmRequest
  | MinimapConfigRequest
  | PermissionResolveRequest
  | PermissionRuleRequest
  | ResetPermissionsRequest
  | GetPermissionRulesRequest
  | WorldPasswordResolveRequest
  | EditorSceneRequest
  | EngineViewportRequest
  | InteractableAreaMessage
  | GetGalleryRequest
  | SetPhotoModeRequest
  | TakePhotoRequest
  | GetGalleryPhotoRequest
  | DeleteGalleryPhotoRequest
  | SetGalleryPhotoPublicRequest

// ---- scene -> page ---------------------------------------------------------

export interface RpcResponse {
  kind: 'rpc:res'
  id: string
  ok: boolean
  value?: unknown
  error?: string
}

/** Fired once the local player has spawned in-world (getPlayer() non-null). */
export interface PlayerReadyEvent {
  kind: 'event'
  name: 'playerReady'
}

/** One of the engine's own analytics events as it queues it for Segment: its Segment name and
 *  properties (no common fields). The page's own events do not come back this way. */
export interface AnalyticsEventMessage {
  kind: 'analytics'
  event: string
  properties: unknown
}

/** Mid-flight `loginNew` verification code (SystemApi.loginNew's `code` promise): the engine
 *  has opened the auth site in the user's external browser; the page shows this code so the
 *  user can match it there. `null` = the auth server issued no code (nothing to match). */
export interface LoginCodeMessage {
  kind: 'loginCode'
  code: string | null
}

/** Mirrors SystemApi SceneLoadingWindow — the scene-asset loading state. */
export type SceneLoadingState = SceneLoadingUi

/** Streamed scene-asset loading updates (drives the React loading screen). */
export interface SceneLoadingMessage {
  kind: 'sceneLoading'
  state: SceneLoadingState
}

/** Mirrors SystemApi ChatMessageDefinition (sender_address → sender). */
export interface ChatMessage {
  sender: string
  message: string
  channel: string
  /** What reactions name this message by; empty for system messages. */
  messageId: string
}

/** A reaction added to or removed from a chat message, the local user's included. */
export interface ChatReaction {
  /** 'Nearby', the DM partner's wallet, or `community:<id>`. */
  channel: string
  messageId: string
  emoji: string
  from: string
  remove: boolean
}

/** Streamed chat reactions (scene → page). */
export interface ChatReactionMessage {
  kind: 'chatReaction'
  reaction: ChatReaction
}

/** Streamed incoming chat messages (scene → page). */
export interface ChatRelayMessage {
  kind: 'chat'
  chat: ChatMessage
}

/** Chat panel visibility, driven by the scene's sidebar chat icon (hud.chatOpen). */
export interface ChatVisibilityMessage {
  kind: 'chatVisibility'
  open: boolean
}

/** Enter was pressed (engine "Chat" system action) → open + focus the chat input, even while
 *  the engine holds keyboard focus (pointer-locked camera-look). */
export interface FocusChatMessage {
  kind: 'focusChat'
}

/** A nearby player (from the scene's PlayerIdentityData set). */
export interface NearbyMember {
  address: string
  name: string
  /** Avatar face snapshot URL (when the profile has loaded). */
  picture?: string
  /** Wears a claimed name (from the engine's profile, once it has loaded). */
  claimed?: boolean
}

/** A player started or stopped talking in voice chat (engine getVoiceStream, scene → page). */
export interface VoiceActivityMessage {
  kind: 'voiceActivity'
  address: string
  active: boolean
}

/** The wallets whose nearby voice the user muted (lowercased), whole on every change (scene → page). */
export interface NearbyMutesMessage {
  kind: 'nearbyMutes'
  addresses: string[]
}

/** Mute ("Hush") or unmute ("Hear") a wallet's nearby voice (page → scene). */
export interface SetNearbyMuteRequest {
  kind: 'setNearbyMute'
  address: string
  muted: boolean
}

// ---- voice chat: private calls and community streams ----------------------

export type { VoiceChatParticipant, VoiceChatRole, VoiceChatState } from './generated'

export interface VoiceChatStateMessage {
  kind: 'voiceChatState'
  state: VoiceChatState
}

/** A voice chat command (page → scene → engine), the engine's own shape; its outcome shows on
 *  `voiceChatState`. */
export type VoiceChatActionWire = VoiceChatAction

export interface VoiceChatActionRequest {
  kind: 'voiceChatAction'
  action: VoiceChatActionWire
}

/** A community stream started or ended, as the social service broadcasts it (scene → page). */
export interface CommunityVoiceUpdateMessage {
  kind: 'communityVoiceUpdate'
  communityId: string
  status: CommunityVoiceStatus
  createdAt: number
  endedAt: number | null
  /** Parcels ('x,y') of the community's places, sent to non-members. */
  positions: string[]
  /** World names of the community's places, sent to non-members. */
  worlds: string[]
  isMember: boolean
  communityName: string
  communityImage: string | null
}

/** A live community stream the local user may see (social-api `/v1/community-voice-chats/active`). */
export interface ActiveCommunityVoiceChat {
  communityId: string
  communityName: string
  communityImage?: string
  isMember: boolean
  positions: string[]
  worlds: string[]
  participantCount: number
  moderatorCount: number
}

/** Every live community stream, re-read on start and on each broadcast (scene → page). */
export interface ActiveCommunityVoiceChatsMessage {
  kind: 'activeCommunityVoiceChats'
  chats: ActiveCommunityVoiceChat[]
  /** False when the list was cleared (no account), not read: it says nothing about what is live. */
  loaded?: boolean
}

/** Nearby members + count, polled by the scene (chat header "Nearby · N"). */
export interface MembersMessage {
  kind: 'members'
  members: NearbyMember[]
}

/** A full menu page (map/settings/backpack/communities) is open in the scene — the
 *  React HUD (sidebar + chat) hides while it is, since the menu has its own nav. */
export interface MenuVisibilityMessage {
  kind: 'menuVisibility'
  open: boolean
}

export type FriendStatus = 'online' | 'offline' | 'away'

/** Mirrors the scene's FriendStatusData (profilePictureUrl → picture). */
export interface Friend {
  address: string
  name: string
  picture?: string
  status: FriendStatus
  /** From the social service: a claimed name shows the verified badge instead of a #tag. */
  claimed?: boolean
  /** The user's chosen name colour (0..1 channels). */
  nameColor?: { r: number; g: number; b: number }
}

/** A blocked account with the profile the social service resolved for it. */
export interface BlockedUser {
  address: string
  name: string
  picture?: string
  claimed?: boolean
  nameColor?: { r: number; g: number; b: number }
}

/** Mirrors the scene's FriendRequestData. */
export interface FriendRequest {
  address: string
  name: string
  picture?: string
  claimed?: boolean
  nameColor?: { r: number; g: number; b: number }
  message?: string
  id: string
  createdAt?: number
}

/** Friends snapshot relayed from the scene's social state (hud.friends + requests).
 *  `available` is false for guests / before the relationship snapshot is seeded. */
export interface FriendsMessage {
  kind: 'friends'
  available: boolean
  friends: Friend[]
  received: FriendRequest[]
  sent: FriendRequest[]
  /** Blocked addresses (names/avatars not resolved here). */
  blocked: string[]
  blockedUsers?: BlockedUser[]
  /** Lowercased addresses of users who have blocked the local user (absent from older bridges). */
  blockedBy?: string[]
  /** Signed in but the social service hasn't initialised yet (vs a guest, who never gets it). */
  loading?: boolean
}

/** Friends social action (page → scene → BevyApi.social.*). Guest-disabled. */
export type FriendAction = 'request' | 'accept' | 'reject' | 'cancel' | 'delete' | 'block' | 'unblock'

export interface FriendActionRequest {
  kind: 'friendAction'
  op: FriendAction
  address: string
  /** Intro text sent with a friend request (≤140 chars). */
  message?: string
}

/** The social service accepted a friend action (scene → page). */
export interface FriendActionDoneMessage {
  kind: 'friendActionDone'
  op: FriendAction
  address: string
}

/** Mutual friends with `address` (page → scene), answered by MutualFriendsMessage. */
export interface GetMutualFriendsRequest {
  kind: 'getMutualFriends'
  address: string
}

export interface MutualFriendsMessage {
  kind: 'mutualFriends'
  address: string
  friends: { address: string; name: string; picture?: string }[]
}

/** A friend came online after startup (scene → page), for the "is online" toast. */
export interface FriendOnlineMessage {
  kind: 'friendOnline'
  address: string
}

/** The social service rejected a friend action (scene → page). */
export interface FriendActionFailedMessage {
  kind: 'friendActionFailed'
  op: FriendAction
  address: string
  error: string
}

/** Whether the local user can DM someone, and why not. Mirrors the engine's DmUserStateData. For
 *  a community channel (`community:<id>`): whether a community message can go anywhere
 *  (`notConnected`: not in the private chat room; `noRouter`: no message router in it). */
export type DmUserState =
  | 'notConnected'
  | 'connected'
  | 'blockedByOwnUser'
  | 'privateMessagesBlockedByOwnUser'
  | 'privateMessagesBlocked'
  | 'disconnected'
  | 'otherClient'
  | 'noRouter'

/** Start or stop following a partner's DM state, or community chat's for a community channel
 *  (page → scene). The scene holds one engine stream per watched address; the page watches each
 *  open DM tab and the shown community tab. */
export interface DmWatchRequest {
  kind: 'dmWatch'
  address: string
  on: boolean
}

/** A watched partner's DM state, on watch and on change (scene → page). */
export interface DmUserStateMessage {
  kind: 'dmUserState'
  address: string
  state: DmUserState
  /** In the private chat room and not blocked by the local user. */
  online: boolean
}

/** The stored DMs with a partner (page → scene), answered by DmHistoryMessage. */
export interface DmHistoryRequest {
  kind: 'dmHistory'
  /** The partner's wallet, or a community's `community:<id>`. */
  address: string
}

/** One stored DM; `from` is the local user or the partner. */
export interface DmHistoryEntry {
  from: string
  message: string
  /** Unix ms when the local client stored it. */
  receivedAt: number
  /** What reactions name this DM by; empty for DMs stored before reactions. */
  messageId: string
  /** Its reactions, in the order each emoji was first used. */
  reactions: MessageReaction[]
}

/** One emoji's reactions to a chat message. */
export interface MessageReaction {
  emoji: string
  /** The wallets that reacted with it. */
  from: string[]
}

export interface DmHistoryMessage {
  kind: 'dmHistory'
  address: string
  /** Oldest first. */
  entries: DmHistoryEntry[]
}

/** Delete the stored DMs with a partner (page → scene). */
export interface DmDeleteRequest {
  kind: 'dmDelete'
  /** The partner's wallet, or a community's `community:<id>`. */
  address: string
}

/** Mirrors the engine's ExplorerSetting (BevyApi.getSettings). A setting is a
 *  Select when it has namedVariants, otherwise a numeric Slider; a 2-variant or
 *  0..1 setting renders as a Toggle. */
import type { SettingInfo as Setting, NamedVariant as SettingVariant } from './generated/index'
export type { Setting, SettingVariant }

export interface SettingsMessage {
  kind: 'settings'
  settings: Setting[]
}

/** A passport achievement badge. */
export interface Badge {
  id: string
  name: string
  description?: string
  /** The latest reached tier's name, e.g. 'Bronze' — for the tier ring. */
  tier?: string
  image?: string
  /** Unix millis when the badge (or, for tiered badges, its latest tier) was unlocked. */
  unlockedAt?: number
  /** Not earned yet. Only your own passport lists these. */
  locked?: boolean
  /** Steps toward the next tier, or toward a multi-step badge's completion, while there is one. */
  progress?: { done: number; target: number }
  /** Every tier in order, for tiered badges. */
  tiers?: BadgeTier[]
}

export interface BadgeTier {
  id: string
  name: string
  description: string
  image?: string
  /** Unix millis when it was reached; absent while locked. */
  unlockedAt?: number
}

/** The about-me field grid on the passport (all optional). */
export interface ProfileInfo {
  country?: string
  sexualOrientation?: string
  gender?: string
  /** ISO `YYYY-MM-DD`. Stored in the profile as epoch seconds; the bridge converts. */
  birthdate?: string
  pronouns?: string
  relationship?: string
  language?: string
  profession?: string
  employment?: string
  hobby?: string
  realName?: string
}

/** The local player's profile (passport). */
export interface Profile {
  address: string
  name: string
  picture?: string
  hasClaimedName: boolean
  /** The chosen name colour (claimed names only), 0–1 channels. */
  nameColor?: { r: number; g: number; b: number }
  isGuest: boolean
  /** The deployed profile's version, when this came from the engine; compared against
   *  `profileChanged` to tell a stale copy from a current one. */
  version?: number
  description?: string
  links?: { title: string; url: string }[]
  // --- rich passport fields (optional; populated by the passport fetch) -----
  /** Full-body avatar snapshot (catalyst `avatar.snapshots.body`) — the passport hero image. */
  bodyImage?: string
  badges?: Badge[]
  info?: ProfileInfo
  /** Mutual-friends count shown under the name. */
  mutuals?: number
  /** Currently-equipped wearables (category slots), resolved from the deployed avatar. */
  equippedWearables?: Wearable[]
  /** Currently-equipped emotes (wheel slots), resolved from the deployed avatar. */
  equippedEmotes?: Emote[]
}

export interface ProfileMessage {
  kind: 'profile'
  profile: Profile | null
}

export interface GetProfileRequest {
  kind: 'getProfile'
}

/** Fetch a user's profile by address from the engine's cache. With `extras`, the passport's badges
 *  and equipped items come too (each a service call of its own, so only the passport asks). */
export interface GetUserProfileRequest {
  kind: 'getUserProfile'
  address: string
  extras?: boolean
}

/** A fetched user's passport (kept separate from the local `profile` message so it
 *  never clobbers the local player's profile state). */
export interface UserProfileMessage {
  kind: 'userProfile'
  address: string
  profile: Profile | null
}

/** The engine now holds this version of a profile (its own player's or anyone nearby). The page
 *  re-reads what it is still showing. */
export interface ProfileChangedMessage {
  kind: 'profileChanged'
  address: string
  version: number
}

/** Edit the local player's own profile (passport edit mode). Every field is optional and an
 *  omitted one is left as it is, so this carries one save, not the whole profile. `links` and
 *  `info`, when present, replace their whole section — that's how a link or a field is cleared. */
export interface SaveProfileRequest {
  kind: 'saveProfile'
  /** Display name. The bridge resolves whether it's a claimed (owned NFT) name. */
  name?: string
  description?: string
  links?: { title: string; url: string }[]
  info?: ProfileInfo
  /** A claimed name's colour, sRGB channels 0–1. The engine applies it only while the name is
   *  claimed; an unclaimed name keeps its derived colour. */
  nameColor?: { r: number; g: number; b: number }
}

/** The outcome of a `saveProfile` — the engine acks the profile deploy, so a failed save is
 *  reported rather than silently leaving the HUD showing something that was never stored. */
export interface ProfileSavedMessage {
  kind: 'profileSaved'
  ok: boolean
  error?: string
}

/** A `saveProfile` without the wire tag — what the UI hands the session. */
export type ProfileEdit = Omit<SaveProfileRequest, 'kind'>

/** The claimed (NFT) names this account owns — the display-name picker's options. */
export interface GetOwnedNamesRequest {
  kind: 'getOwnedNames'
}

export interface OwnedNamesMessage {
  kind: 'ownedNames'
  names: string[]
}

/** Mirrors the engine's BaseNotification (metadata varies by type). */
export interface AppNotification {
  id: string
  type: string
  timestamp: string
  read: boolean
  metadata: Record<string, unknown>
}

export interface NotificationsMessage {
  kind: 'notifications'
  notifications: AppNotification[]
}

/** Unread notifications newer than any seen before, from the scene's poll (newest first). */
export interface NotificationsNewMessage {
  kind: 'notificationsNew'
  notifications: AppNotification[]
}

export interface GetNotificationsRequest {
  kind: 'getNotifications'
}

/** Persist "mark as read" for the given notification ids (signed PUT in the relay). */
export interface MarkNotificationsReadRequest {
  kind: 'markNotificationsRead'
  ids: string[]
}

/** One owned emote. `slot` is its wheel slot (0–9) when currently equipped, undefined otherwise —
 *  so the backpack shows the whole collection while the wheel still finds the 10 equipped by slot. */
export interface Emote {
  slot?: number
  urn: string
  name: string
  thumbnail?: string
  rarity?: string
  /** Marketplace deep link (…/shop/item/<contract>/<itemId>) — set only for on-chain collectibles;
   *  base emotes have no listing. Resolved scene-side (see resolveShopUrls). */
  shopUrl?: string
}

export interface EmotesMessage {
  kind: 'emotes'
  emotes: Emote[]
}

/** Assign an owned emote to a wheel slot (0–9), or clear the slot with urn:''. */
export interface EquipEmoteRequest {
  kind: 'equipEmote'
  slot: number
  urn: string
}

export interface MicMessage {
  kind: 'mic'
  enabled: boolean
  available: boolean
}

export interface SetMicRequest {
  kind: 'setMic'
  enabled: boolean
}

/** The local player's current parcel (for the map marker / centering). */
export interface MapMessage {
  kind: 'mapState'
  x: number
  y: number
}

export interface GetMapRequest {
  kind: 'getMap'
}

export interface GetHomeSceneRequest {
  kind: 'getHomeScene'
}

export interface HomeSceneMessage extends HomeScene {
  kind: 'homeScene'
}

/** What the welcome page must show for the signed-in account (answered with `welcome`). */
export interface GetWelcomeRequest {
  kind: 'getWelcome'
  /** The page's tag for this ask, which its answer echoes. */
  ask?: number
}

export interface WelcomeMessage extends WelcomeState {
  kind: 'welcome'
  /** The tag of the ask this answers, as the page sent it. */
  ask?: number
}

/** Put the curated default look `default<look>` on the lobby stage's avatar, as the page fetched it
 *  (the scene's own fetches are signed, so it can't before sign-in): the guest start page's pick, or
 *  the welcome page's RANDOMIZE. Signed in it is the Backpack's unsaved look, deployed on
 *  `acceptWelcome`; before sign-in the stage wears it as a stand-in. */
export interface ShowLookRequest {
  kind: 'showLook'
  look: number
  avatar: AvatarLook
}

/** A `showLook` is on its way (RANDOMIZE was hit; the page is fetching the look): the lobby stage's
 *  avatar turns away now rather than when the look arrives. */
export interface ExpectLookRequest {
  kind: 'expectLook'
}

/** The look the next RANDOMIZE will show: the stage loads it out of sight once the one on show has
 *  loaded, so the swap is instant. */
export interface PreloadLookRequest {
  kind: 'preloadLook'
  look: number
  avatar: AvatarLook
}

/** The welcome page's accept: the terms, then for a new profile its `name` with the look in one
 *  setAvatar. Answered with `welcomeAccepted` once that lands. */
export interface AcceptWelcomeRequest {
  kind: 'acceptWelcome'
  name?: string
}

/** Open the terms of use or the privacy policy in the user's browser (the scene's openExternalUrl,
 *  which works from the native HUD too). */
export interface OpenLegalRequest {
  kind: 'openLegal'
  doc: 'terms' | 'privacy'
}

/** Open an `https:` url in the user's browser (the scene's openExternalUrl, which works from the
 *  native HUD too). Answered by an ExternalUrlOpenedMessage. */
export interface OpenExternalUrlRequest {
  kind: 'openExternalUrl'
  requestId: number
  url: string
}

export interface ExternalUrlOpenedMessage {
  kind: 'externalUrlOpened'
  requestId: number
  ok: boolean
}

export interface WelcomeAcceptedMessage {
  kind: 'welcomeAccepted'
  ok: boolean
  error?: string
}

/** The local player's live pose, streamed for the minimap. Position is in world metres
 *  (not parcels) so the map can scroll smoothly between parcels; both yaws are degrees.
 *  `yaw` is the avatar's heading (drives the arrow), `camYaw` the camera's (drives
 *  "rotate with camera"). Throttled and change-guarded by the scene. */
export interface PlayerPoseMessage {
  kind: 'playerPose'
  x: number
  z: number
  yaw: number
  camYaw: number
}

/** Which realm we're in. `satelliteView` is the satellite map the realm advertises in its
 *  `/about` — only Genesis City does, so anywhere else (Worlds, local scenes) it is null and
 *  the minimap forces the engine-rendered Camera style. Pushed on change. */
export interface RealmInfoMessage {
  kind: 'realmInfo'
  realm: string
  satelliteView: SatelliteView | null
}

/** Title of the scene the player is standing in, for the minimap header. Resolved by parcel
 *  from the live scene list and pushed when the parcel changes — NOT the same as the
 *  `sceneLoading` title, which describes the entry overlay and goes stale the moment you
 *  walk into the next scene. Empty when the parcel has no deployed scene. */
/** The lobby stage and its avatar have been drawn (scene → page), so the page's stand-in can go. */
export interface LobbyStageReadyMessage {
  kind: 'lobbyStageReady'
}

/** The look `default<look>` (a `showLook`) is on the lobby stage, loaded and facing front (scene →
 *  page): the next RANDOMIZE can go. */
export interface LookShownMessage {
  kind: 'lookShown'
  look: number
}

export interface SceneInfoMessage {
  kind: 'sceneInfo'
  title: string
  /** The parcel the title was resolved for, and the realm it is in. */
  parcel?: { x: number; y: number }
  realm?: string
  /** Hash of the scene on that parcel; absent when it has none. */
  scene?: string
  /** Genesis City (it advertises the satellite map); anything else is a World or a local realm. */
  genesis?: boolean
  preview?: boolean
}

/** Minimap style/zoom/rotation (page → scene). The scene only runs the Camera-style
 *  TextureCamera while `style` is 'imposters'; the DOM styles render in React and the
 *  camera is disposed. `visibleMeters` maps to the camera's orthographic vertical range. */
export interface MinimapConfigRequest {
  kind: 'minimapConfig'
  style: MinimapStyle
  rotation: MinimapRotation
  visibleMeters: number
}

/** 'parcel' and 'satellite' are rendered in the DOM from map tiles; 'imposters' (labelled
 *  "Camera" in the UI) is a live top-down render of the world by the engine. */
export type MinimapStyle = 'parcel' | 'satellite' | 'imposters'

/** 'camera' rotates the map with the camera; 'north' keeps north up. */
export type MinimapRotation = 'camera' | 'north'

/** Teleport to a parcel (page → scene → teleportTo). */
export interface TeleportRequest {
  kind: 'teleport'
  x: number
  y: number
  /** Realm the parcel belongs to (a world name or realm url). The engine changes realm first —
   *  a full reconnect, as for changeRealm, even to the realm the player is in. Omitted: a parcel of
   *  the realm the player is in. */
  realm?: string
  /** Set with `realm`: the scene answers with a `travelResult` carrying this id. */
  travelId?: number
}

/** Change to a world/realm (page → scene → changeRealm). `realm` is a world name
 *  (e.g. `boedo.dcl.eth`) or realm URL. */
export interface ChangeRealmRequest {
  kind: 'changeRealm'
  realm: string
  /** The scene answers with a `travelResult` carrying this id. */
  travelId?: number
}

/** How a realm change the page asked for ended (scene → page). On failure the engine kept the
 *  player in the realm they were in; `message` is the engine's reason. */
export interface TravelResultMessage {
  kind: 'travelResult'
  travelId: number
  realm: string
  ok: boolean
  message?: string
}

/** Spawn or kill the scene editor's super-user scene, served at `<PAGE_DIR>editor-scene/<hash>`. */
export interface EditorSceneRequest {
  kind: 'editorScene'
  id: number
  action: 'spawn' | 'kill'
  source: string
  hash: string
}

/** How an `editorScene` request ended (scene → page). A spawn answers once the scene is live. */
export interface EditorSceneResultMessage {
  kind: 'editorSceneResult'
  id: number
  ok: boolean
  error?: string
}

/** A scene's pending permission prompt relayed from the engine (e.g. it wants to move you
 *  to a new realm). Shown as the React permission dialog; resolved with permissionResolve. */
export interface PermissionRequestMessage {
  kind: 'permissionRequest'
  /** Engine-assigned request id (echoed back to resolve a single request). */
  id: number
  /** PermissionType serde name, e.g. 'ChangeRealm' — React maps it to the human prompt. */
  ty: string
  /** Scene title (e.g. 'Genesis Plaza') for the dialog text. */
  sceneName: string
  /** Scene hash — the value for a Scene-level "Always" grant. */
  scene: string
  /** Realm url the request was made under — the value for a Realm-level "Always" grant. */
  realm: string
  /** Extra context line (e.g. 'Jump to DCL Kickoff Challenge?'). */
  additional?: string
}

/** A queued permission prompt the engine has already resolved (a permanent rule set for an earlier
 *  prompt now covers it) — the HUD drops it without asking. */
export interface PermissionWithdrawnMessage {
  kind: 'permissionWithdrawn'
  id: number
}

/** A scene used a permission, allowed or refused (scene → page, from the engine's permission-used
 *  stream). The HUD keeps the latest outcome per type for the scene the player stands in, so the
 *  rail can highlight the answer or rule that decided it and count the refusals the player did
 *  not click themselves. */
export interface PermissionUsedMessage {
  kind: 'permissionUsed'
  ty: string
  sceneName: string
  scene: string
  realm: string
  allowed: boolean
}

/** The portable experiences the player can see and stop (scene → page, from getPortablesStream):
 *  every portable but the HUD's own scene and the startup ones, plus smart wearables that are
 *  stopped or still loading. The engine sends the whole list on subscribe and on every change. */
export interface PortablesMessage {
  kind: 'portables'
  portables: PortableInfo[]
}

/** The engine needs a password to enter the world the player is travelling to (scene → page).
 *  Shown as the password dialog; answered with worldPasswordResolve. Re-sent with an `error` when
 *  an answer was refused (wrong password, too many attempts). There is only ever one open prompt:
 *  a new one replaces it. */
export interface WorldPasswordRequestMessage {
  kind: 'worldPasswordRequest'
  /** The world's name, e.g. 'my-world.dcl.eth'. */
  world: string
  /** Why the previous answer was refused; absent on the first ask. */
  error?: string
}

/** The engine no longer needs the password prompt answered (its realm change is over). */
export interface WorldPasswordWithdrawnMessage {
  kind: 'worldPasswordWithdrawn'
}

/** The player's answer to the world password prompt (page → scene → SystemApi): the password, or
 *  null when they gave up. */
export interface WorldPasswordResolveRequest {
  kind: 'worldPasswordResolve'
  password: string | null
}

/** Which scope an Allow/Deny applies to. `once` = just this request (dismissing the prompt);
 *  `visit` = this request, and the page answers the same scene's later requests of the type the
 *  same way until the player leaves the scene or reloads it; the rest persist a rule in the engine. */
export type PermissionLevelChoice = 'once' | 'visit' | 'scene' | 'realm' | 'global'

/** Scope of a rule written from the permissions review (page → scene): the "Always for …" levels
 *  store Allow/Deny in the engine; 'visit' forgets the stored rule at scene and realm level and
 *  stores Ask at global, so the scene asks again and the page answers from its visit memory. */
export type PermissionRuleLevel = Exclude<PermissionLevelChoice, 'once'>

/** Rewrite the rule for one permission (page → scene → setPermanentPermission). Only the stored
 *  defaults change: a request already granted or refused is not revisited. */
export interface PermissionRuleRequest {
  kind: 'permissionRule'
  ty: string
  level: PermissionRuleLevel
  /** Ignored for 'visit' (the page keeps that answer); null clears the stored rule at `level`
   *  (at 'global', stores Ask: the built-in default allows some types outright). */
  allow: boolean | null
  scene: string
  realm: string
}

/** Forget every stored permission answer, at every level, back to the built-in defaults
 *  (page → scene → /reset_permissions). */
export interface ResetPermissionsRequest {
  kind: 'resetPermissions'
}

export type PermissionRuleValue = 'Allow' | 'Deny' | 'Ask'

/** One stored rule (PermanentPermissionItem). */
export interface PermissionRuleItem {
  ty: string
  allow: PermissionRuleValue
}

/** Fetch the stored rules for a scene, the current realm and global (page → scene →
 *  getPermanentPermissions ×3); answered with `permissionRules`. */
export interface GetPermissionRulesRequest {
  kind: 'getPermissionRules'
  scene: string
}

export interface PermissionRulesMessage {
  kind: 'permissionRules'
  scene: string
  realm: string
  rules: { scene: PermissionRuleItem[]; realm: PermissionRuleItem[]; global: PermissionRuleItem[] }
}

/** The user's decision on a permission prompt (page → scene → SystemApi). */
export interface PermissionResolveRequest {
  kind: 'permissionResolve'
  id: number
  ty: string
  allow: boolean
  level: PermissionLevelChoice
  /** Scene hash + realm carried back so the scene can target a permanent grant. */
  scene: string
  realm: string
}

/**
 * Tells the scene to render an engine-backed view (the rich map, or the avatar
 * preview) into a screen rectangle that a React page has carved out as transparent.
 * `rect` is in CSS pixels relative to the viewport; null hides the view (on close).
 */
export interface EngineViewportRequest {
  kind: 'engineViewport'
  region: 'map' | 'avatarPreview' | 'lobby'
  rect: { x: number; y: number; width: number; height: number } | null
  /** `devicePixelRatio` at the time the rect was measured. The rect is in CSS pixels, so this is
   *  what turns it into the physical pixels the scene should actually render — without it a
   *  render target has to assume the worst display and oversample everywhere else. */
  dpr?: number
}

/**
 * The screen area the persistent HUD (nav rail, minimap, chat or friends panel) occupies, as insets from each
 * window edge in CSS pixels. The engine publishes it to scenes as
 * UiCanvasInformation.interactable_area. Sent whenever the measured layout changes.
 */
export interface InteractableAreaMessage {
  kind: 'interactableArea'
  left: number
  top: number
  right: number
  bottom: number
}

/** A community (from the scene's fetchCommunities). */
export interface Community {
  id: string
  name: string
  description: string
  thumbnail?: string
  membersCount: number
  /** 'owner' | 'moderator' | 'member' | 'none' — membership of the local user. */
  role: string
  ownerName: string
  /** 'public' | 'private' — gates the join flow (public = join, private = request). */
  privacy?: string
  /** 'all' (listed in browse and search) | 'unlisted' (members and invitees only). */
  visibility?: string
  /** Id of the local user's pending request to join (private communities). */
  pendingRequestId?: string
  /** Id of the local user's pending invite to the community, to accept or reject. */
  pendingInviteId?: string
  /** The community's voice stream, as the social-api reports it. */
  voiceChatStatus?: { isActive: boolean; participantCount: number; moderatorCount: number }
  /** The first three of the user's friends who are members, for the browse card. Only the paged
   *  browse read carries them. */
  friends?: CommunityFriend[]
}

/** A friend who is a member of a community, for the browse card. */
export interface CommunityFriend {
  address: string
  name: string
  picture?: string
  hasClaimedName: boolean
}

/** A page request's handler threw, so its reply never came (scene → page). */
export interface RequestFailedMessage {
  kind: 'requestFailed'
  request: PageToScene['kind']
  error: string
}

/** Deploying the Backpack's look was rejected (scene → page). */
export interface AvatarSaveFailedMessage {
  kind: 'avatarSaveFailed'
  message: string
}

/** The communities the user is a member of: My Communities (scene → page). */
export interface CommunitiesMessage {
  kind: 'communities'
  communities: Community[]
}

export interface GetCommunitiesRequest {
  kind: 'getCommunities'
}

/** One page of the browse grid: 20 communities a page, by name on the
 *  server when `search` is set (page → scene → social-api GET /v2/communities). */
export interface BrowseCommunitiesRequest {
  kind: 'browseCommunities'
  /** '' lists every community. */
  search: string
  /** From 1. */
  page: number
}

/** The answer to BrowseCommunitiesRequest, naming the search and page it is for, so the page drops
 *  an answer to an older search (scene → page). */
export interface CommunityBrowseMessage {
  kind: 'communityBrowse'
  search: string
  page: number
  communities: Community[]
  /** How many communities the search has in all. */
  total: number
  /** The read failed; `communities` is empty. */
  failed?: boolean
}

/** One community by id (page → scene → social-api GET /v2/communities/{id}), for a card opened
 *  outside the browse list, e.g. from its chat. Answered by CommunityMessage. */
export interface GetCommunityRequest {
  kind: 'getCommunity'
  id: string
}
export interface CommunityMessage {
  kind: 'community'
  community: Community
}

/** Create a community (page → scene → signed multipart POST to the social-api). */
export interface CreateCommunityRequest {
  kind: 'createCommunity'
  name: string
  description: string
  privacy: 'public' | 'private'
  /** Discoverable in the directory → visibility 'all', otherwise 'unlisted'. */
  discoverable: boolean
  /** The picture as a data URL (PNG or JPEG, at most 500 KB and 512×512 px), sent as the form's
   *  `thumbnail` file part. */
  thumbnail?: string
  /** The linked places (lands first, then worlds), sent as the form's `placeIds` JSON array. */
  placeIds: string[]
}

/** Edit a community (page → scene → signed multipart PUT to the social-api). As unity-explorer,
 *  only what changed is sent; a field left out keeps its value. */
export interface UpdateCommunityRequest {
  kind: 'updateCommunity'
  id: string
  name?: string
  description?: string
  privacy?: 'public' | 'private'
  /** Listed in browse and search → visibility 'all', otherwise 'unlisted'. */
  discoverable?: boolean
  /** A new picture as a data URL (same limits as on create). */
  thumbnail?: string
  /** The whole new set of linked places (lands first, then worlds), when it changed; absent keeps
   *  the current set. */
  placeIds?: string[]
}

/** A LAND or World the form can link, or one already linked (unity-explorer's CommunityPlace). */
export interface CommunityPlaceOption {
  id: string
  isWorld: boolean
  /** "title (world name)" or "title (x,y)". */
  label: string
  /** The place owner's name, or empty when it could not be read. */
  ownerName: string
}

/** The form opened (page → scene): read the places the user owns or operates, and, when editing,
 *  the community's linked places. */
export interface GetCommunityPlaceOptionsRequest {
  kind: 'getCommunityPlaceOptions'
  /** The community being edited; absent on create. */
  communityId?: string
}
/** The places for the form (scene → page). */
export interface CommunityPlaceOptionsMessage {
  kind: 'communityPlaceOptions'
  communityId?: string
  /** The places the user owns or operates: the picker's options. */
  owned: CommunityPlaceOption[]
  /** The community's linked places (edit only). */
  linked: CommunityPlaceOption[]
  /** Linked places the places service did not return (a disabled one, say): never shown, but kept
   *  on every save so the form cannot unlink them. */
  hiddenLinkedIds: string[]
  /** The linked places could not be read (edit only): the form must not replace them. */
  linkedFailed: boolean
  /** unity-explorer's messages for the reads that failed, each shown as a toast. */
  errors: string[]
}

/** Delete a community the user owns (page → scene → signed DELETE to the social-api). */
export interface DeleteCommunityRequest {
  kind: 'deleteCommunity'
  id: string
}
/** A community was deleted (scene → page): its card closes. */
export interface CommunityDeletedMessage {
  kind: 'communityDeleted'
  id: string
}

/** The content check's findings per field: short tags, at most three each. */
export interface CommunityModerationIssues {
  name: string[]
  description: string[]
  image: string[]
}

/** How a community create or update ended (scene → page). Exactly one of `id`, `moderation`, `error`. */
export interface CommunitySaveResultMessage {
  kind: 'communitySaveResult'
  mode: 'create' | 'update'
  /** The community created or updated, on success. */
  id?: string
  /** The social-api refused it (HTTP 400): the content check's findings, which may be empty, or
   *  `unavailable` when the check itself could not run. */
  moderation?: { issues: CommunityModerationIssues; unavailable: boolean }
  /** Any other failure. */
  error?: string
}

export interface JoinCommunityRequest {
  kind: 'joinCommunity'
  id: string
}

export interface LeaveCommunityRequest {
  kind: 'leaveCommunity'
  id: string
}

/** Ask to join a private community (social-api POST /communities/{id}/requests). */
export interface RequestToJoinCommunityRequest {
  kind: 'requestToJoinCommunity'
  id: string
}

/** The user's join request to a community was sent (with its id) or cancelled (no id), told as
 *  soon as the write succeeds, before the community is read again (scene → page). */
export interface CommunityJoinRequestMessage {
  kind: 'communityJoinRequest'
  id: string
  requestId?: string
}

export interface CancelJoinRequestRequest {
  kind: 'cancelJoinRequest'
  id: string
  requestId: string
}

/** What an owner or moderator can do to a member from the card's member menu (unity-explorer's
 *  MembersListView): promote to moderator, demote to member, hand over ownership, remove, ban. */
export type CommunityMemberAction = 'promote' | 'demote' | 'transfer' | 'kick' | 'ban'

/** A member action from the card's member menu (page → scene → signed social-api write). */
export interface CommunityMemberActionRequest {
  kind: 'communityMemberAction'
  id: string
  /** The member acted on. */
  address: string
  action: CommunityMemberAction
}

export type CommunityAction =
  | 'join'
  | 'requestToJoin'
  | 'cancelJoinRequest'
  | 'leave'
  | 'fetch'
  | 'delete'
  | 'removePlace'
  | CommunityMemberAction
  | 'unban'
  | CommunityRequestAction

/** Lift a member's ban, from the card's Banned section (page → scene → signed DELETE
 *  /v1/communities/{id}/members/{address}/bans). */
export interface UnbanCommunityMemberRequest {
  kind: 'unbanCommunityMember'
  id: string
  address: string
}

/** The answers to an invite or a request, each with its own error text: accepting or rejecting the user's own invite, and an owner's or
 *  moderator's accept, reject or cancel on a member's request or invite. */
export type CommunityRequestAction = 'acceptInvite' | 'rejectInvite' | 'manageRequest'

/** Accept, reject or cancel a pending invite or join request (page → scene → signed PATCH
 *  /v1/communities/{id}/requests/{requestId}). `address` names the member when an owner or
 *  moderator answers another member's request or cancels an invite they sent; without it the
 *  request is the user's own invite. */
export interface ManageCommunityRequestRequest {
  kind: 'manageCommunityRequest'
  id: string
  requestId: string
  intention: 'accepted' | 'rejected' | 'cancelled'
  address?: string
}

/** An answer to a request ended, well or not (scene → page): that request's buttons are released. */
export interface CommunityRequestSettledMessage {
  kind: 'communityRequestSettled'
  id: string
  requestId: string
}

/** A pending join request or invite as a member row: who it is for, and the request's id. */
export interface CommunityRequestMember extends CommunityMember {
  requestId: string
}

/** The join requests one of the user's communities received, for its owners and moderators. */
export interface CommunityRequestsGroup {
  community: Community
  requests: CommunityRequestMember[]
  total: number
}

/** The browser's Invites & Requests section (page → scene): the user's invites, the join requests
 *  their owned or moderated communities received, and their own join requests. */
export interface GetCommunityInvitesRequest {
  kind: 'getCommunityInvites'
}
/** The sidebar's counter alone (page → scene), read when the Communities page opens: the full
 *  section is read only when it opens. */
export interface GetCommunityInvitesCountRequest {
  kind: 'getCommunityInvitesCount'
}
/** How many invites and requests received wait on the user (scene → page). */
export interface CommunityInvitesCountMessage {
  kind: 'communityInvitesCount'
  count: number
}
/** The answer to GetCommunityInvitesRequest (scene → page). `failed` names the lists that could not
 *  be read; those are empty. */
export interface CommunityInvitesMessage {
  kind: 'communityInvites'
  /** Communities the user is invited to, each with its `pendingInviteId`. */
  invites: Community[]
  requestsReceived: CommunityRequestsGroup[]
  /** Communities the user asked to join, each with its `pendingRequestId`. */
  requestsSent: Community[]
  failed: Array<'invites' | 'requestsReceived' | 'requestsSent'>
}

/** The card's Members tab sections an owner or moderator sees beside Members: banned members,
 *  join requests (private communities) and invites sent. */
export type CommunityMemberSection = 'banned' | 'requests' | 'invites'

/** Read one Members tab section (page → scene → signed social-api GET). */
export interface GetCommunityMemberSectionRequest {
  kind: 'getCommunityMemberSection'
  id: string
  section: CommunityMemberSection
}
/** One Members tab section (scene → page). A banned row's `requestId` is ''. */
export interface CommunityMemberSectionMessage {
  kind: 'communityMemberSection'
  id: string
  section: CommunityMemberSection
  members: CommunityRequestMember[]
  total: number
  failed?: boolean
}

/** The communities the user may invite someone to (page → scene → social-api GET
 *  /v1/members/{address}/invites): those they own or moderate that the person is not in. */
export interface GetInvitableCommunitiesRequest {
  kind: 'getInvitableCommunities'
  address: string
}
export interface InvitableCommunitiesMessage {
  kind: 'invitableCommunities'
  address: string
  communities: Array<{ id: string; name: string }>
  failed?: boolean
}

/** Invite someone to a community (page → scene → signed POST /v1/communities/{id}/requests). */
export interface InviteToCommunityRequest {
  kind: 'inviteToCommunity'
  id: string
  address: string
}
/** How an invite ended (scene → page), for its toast. */
export interface CommunityInviteSentMessage {
  kind: 'communityInviteSent'
  id: string
  address: string
  ok: boolean
}

/** A community write, or the read of one community, the social-api rejected (scene → page), so the UI can say so. */
export interface CommunityActionFailedMessage {
  kind: 'communityActionFailed'
  id: string
  action: CommunityAction
  message: string
}

/** A member of a community (Members tab). */
export interface CommunityMember {
  address: string
  name: string
  /** 'owner' | 'moderator' | 'member' */
  role: string
  picture?: string
  hasClaimedName?: boolean
}

/** A place shared by a community (Places tab). */
export interface CommunityPlace {
  id: string
  title: string
  thumbnail?: string
  /** e.g. "-66,56", or the world's name for a World */
  positions?: string
  /** 0..1 like rate. */
  likeRate?: number
  /** The signed-in user owns the place: an owner or moderator may unlink it from the card. */
  isMine?: boolean
}

/** Unlink a place from a community (page → scene → signed DELETE to the social-api). */
export interface RemoveCommunityPlaceRequest {
  kind: 'removeCommunityPlace'
  id: string
  placeId: string
}

/** An announcement post (Announcements tab). */
export interface CommunityPost {
  id: string
  author: string
  authorAddress: string
  authorPicture?: string
  text: string
  timestamp: number
  likes: number
}

/** An upcoming event (right-hand sidebar). */
export interface CommunityEvent {
  id: string
  name: string
  thumbnail?: string
  startsAt: number
}

/** A camera-reel photo shared in the community (Photos tab). */
export interface CommunityPhoto {
  id: string
  url: string
  thumbnail?: string
}

/** Request the per-community detail when the modal opens. */
export interface GetCommunityDetailRequest {
  kind: 'getCommunityDetail'
  id: string
}
export interface CommunityDetailMessage {
  kind: 'communityDetail'
  id: string
  members: CommunityMember[]
  posts: CommunityPost[]
  places: CommunityPlace[]
  events: CommunityEvent[]
  photos: CommunityPhoto[]
}

// ---- community chat --------------------------------------------------------

/** A community the local user is a member of, as a chat channel. */
export interface CommunityChannel {
  id: string
  name: string
  /** The community's thumbnail URL (may 404: the UI falls back to the initial). */
  thumbnail: string
  /** 'owner' | 'moderator' | 'member' */
  role: string
}

/** The communities the local user is a member of (page → scene → social-api onlyMemberOf). */
export interface GetCommunityChannelsRequest {
  kind: 'getCommunityChannels'
}
export interface CommunityChannelsMessage {
  kind: 'communityChannels'
  communities: CommunityChannel[]
}
/** The local user joined (or was told they are in) a community (scene → page). */
export interface CommunityChannelAddedMessage {
  kind: 'communityChannelAdded'
  community: CommunityChannel
}
/** The local user left, was removed from, or lost a community (scene → page). */
export interface CommunityChannelRemovedMessage {
  kind: 'communityChannelRemoved'
  id: string
}

/** The community channels the HUD listens to: member of, chat not closed (page → scene → engine).
 *  The engine drops messages on any other community channel before storing them. */
export interface SetCommunityChannelsRequest {
  kind: 'setCommunityChannels'
  ids: string[]
}

/** The members of a community who are online now (page → scene → social-api onlyOnline). */
export interface GetCommunityOnlineMembersRequest {
  kind: 'getCommunityOnlineMembers'
  id: string
}
export interface CommunityOnlineMembersMessage {
  kind: 'communityOnlineMembers'
  id: string
  members: CommunityMember[]
}

/** A member of one of the local user's communities came online or went offline (scene → page). */
export interface CommunityPresenceMessage {
  kind: 'communityPresence'
  communityId: string
  member: CommunityMember
  online: boolean
}

/** Play a UI sound (page → scene → engine): a chat line landing, or one that mentions the user. */
export type UiSound = 'chatMessage' | 'chatMention'
export interface PlayUiSoundRequest {
  kind: 'playUiSound'
  sound: UiSound
}

// ---- gallery (camera reel) -------------------------------------------------

/** One camera-reel photo (compact list item). `dateTime` is the service's raw string
 *  (a unix timestamp in seconds or ms, or ISO) — parsed for month-grouping on the page. */
export interface GalleryPhoto {
  id: string
  url: string
  thumbnailUrl?: string
  dateTime: string
  /** Whether the photo is publicly shareable (only the owner can flip it). */
  isPublic?: boolean
}

/** A person captured in a photo (the detail-view "people in this photo" list). */
export interface GalleryPerson {
  address: string
  name: string
  isGuest?: boolean
}

/** Full metadata for one photo (fetched lazily when its detail view opens). */
export interface GalleryPhotoMeta {
  /** Who took the photo. */
  userName?: string
  userAddress?: string
  /** Scene name + parcel where it was taken (for "Jump In"). */
  sceneName?: string
  x?: number
  y?: number
  realm?: string
  people?: GalleryPerson[]
}

/** One page of the local player's camera reel (signed, so private photos are included), newest
 *  first as the service orders them. `requestId` is echoed in the GalleryMessage that answers it. */
export interface GetGalleryRequest {
  kind: 'getGallery'
  requestId: number
  offset: number
  limit: number
}

/** A page of the local player's camera-reel photos + storage usage: `current` is the total number of
 *  photos the player has, `max` the storage limit. `received` counts the images the service sent,
 *  url-less ones included, which `photos` drops. */
export interface GalleryMessage {
  kind: 'gallery'
  requestId: number
  ok: boolean
  photos: GalleryPhoto[]
  received: number
  current: number
  max: number
}

/** Fetch one photo's full metadata (place + people) for its detail view. */
export interface GetGalleryPhotoRequest {
  kind: 'getGalleryPhoto'
  id: string
}

export interface GalleryPhotoMessage {
  kind: 'galleryPhoto'
  id: string
  meta: GalleryPhotoMeta | null
}

/** Delete one of the local player's photos (signed DELETE). Answered by a GalleryWriteMessage. */
export interface DeleteGalleryPhotoRequest {
  kind: 'deleteGalleryPhoto'
  requestId: number
  id: string
}

/** Make one of the local player's photos public or private (signed PATCH). Answered by a
 *  GalleryWriteMessage. */
export interface SetGalleryPhotoPublicRequest {
  kind: 'setGalleryPhotoPublic'
  requestId: number
  id: string
  isPublic: boolean
}

/** The outcome of a gallery write. A delete also reports the storage usage that follows it. */
export interface GalleryWriteMessage {
  kind: 'galleryWrite'
  requestId: number
  ok: boolean
  current?: number
  max?: number
}

/** An owned wearable (backpack catalog item). */
export interface Wearable {
  urn: string
  name: string
  rarity: string
  category: string
  thumbnail?: string
  equipped: boolean
  /** Marketplace deep link (…/shop/item/<contract>/<itemId>) — set only for on-chain collectibles;
   *  base/off-chain items have no listing. Resolved scene-side (see resolveShopUrls). */
  shopUrl?: string
  /** Body shapes the item has a representation for (from the catalog entity). */
  bodyShapes?: string[]
  /** Ships a scene script (runs while worn). */
  isSmart?: boolean
  /** Categories it hides when worn on the current body shape. */
  hides?: string[]
}

/** Currently-equipped wearables, resolved by urn independently of the (paginated) grid so every
 *  equipped item drives its per-category slot even when it isn't on the current catalog page. */
export type AvatarColorTarget = 'skin' | 'hair' | 'eyes'

export interface WearablesMessage {
  kind: 'wearables'
  equipped: Wearable[]
  /** The avatar's current body shape urn. */
  bodyShape?: string
  /** The avatar's colors (engine Color3, 0–1). */
  colors?: { skin?: Color3; hair?: Color3; eyes?: Color3 }
  /** Categories shown even though an equipped item hides them. */
  forceRender?: string[]
}

/** Replace the Backpack look's force-render list (deployed when it closes). */
export interface SetForceRenderRequest {
  kind: 'setForceRender'
  categories: string[]
}

/** Change one avatar color in the Backpack's look (deployed when it closes). */
export interface SetAvatarColorRequest {
  kind: 'setAvatarColor'
  target: AvatarColorTarget
  color: Color3
}

/** Load the equipped-wearables set (category slots). The owned catalog itself is paged via
 *  catalogQuery — this only carries the decoupled equipped items. */
export interface GetWearablesRequest {
  kind: 'getWearables'
}

/** Which owned-items catalog a paged query targets. Emotes reuse the same query/response. */
export type CatalogKind = 'wearables' | 'emotes'

/** A generic server-side-paginated request for an owned-items catalog page (backpack grid). The
 *  scene fetches exactly this page from the catalyst so multi-thousand inventories never load at
 *  once. Filters/sort are applied server-side; `requestId` lets the page drop stale responses. */
export interface CatalogQueryRequest {
  kind: 'catalogQuery'
  catalog: CatalogKind
  /** 0-based page index. */
  page: number
  pageSize: number
  /** Wearables body-part category filter ('all' → omit). */
  category?: string
  /** Free-text name filter (server-side). */
  search?: string
  orderBy?: 'date' | 'rarity' | 'name'
  direction?: 'asc' | 'desc'
  /** Exclude base (off-chain) items → collectibles only. */
  collectiblesOnly?: boolean
  smartOnly?: boolean
  /** Monotonic per-catalog id echoed in the response; the page ignores out-of-order replies. */
  requestId: number
}

/** One catalog page (response to CatalogQueryRequest). `total` drives the pager. */
export interface CatalogPageMessage {
  kind: 'catalogPage'
  catalog: CatalogKind
  items: Wearable[]
  total: number
  requestId: number
}

/** Equip a new full wearable set on the Backpack's look (page → scene); deployed on commitAvatar. */
export interface EquipRequest {
  kind: 'equip'
  urns: string[]
}

/** The Backpack closed: deploy its look if it changed (page → scene → BevyApi.setAvatar). */
export interface CommitAvatarRequest {
  kind: 'commitAvatar'
}

/** Drop the Backpack's look after a failed deploy and put back the last one that deployed. */
export interface RevertAvatarRequest {
  kind: 'revertAvatar'
}

/** Preview a wearable set on the Backpack avatar WITHOUT adding it to the Backpack's look
 *  (selecting an item, not equipping). `urns: null` clears the preview (back to the look). */
export interface PreviewAvatarRequest {
  kind: 'previewAvatar'
  urns: string[] | null
}

/** Which part of the preview avatar the camera frames (the Backpack's selected category). */
export type PreviewFocus = 'body' | 'head' | 'top' | 'bottom' | 'shoes'
export interface PreviewFocusRequest {
  kind: 'previewFocus'
  focus: PreviewFocus
}

/** The Shop's stage around the preview avatar: where the page draws its stage image on screen, in CSS
 *  pixels like engineViewport's rect. The scene draws the same image (its images/shop-stage.jpg, the HUD's
 *  assets/shop/stage.jpg) there behind the avatar, so the avatar's window shows no seam, and drops the
 *  Backpack's podium: the stage's floor is in the image. `null` gives the preview its own backdrop back. */
export interface PreviewStageRequest {
  kind: 'previewStage'
  rect: { x: number; y: number; width: number; height: number } | null
}

/** Play an emote on the preview avatar once (the Shop trying one on); asked again, it plays again.
 *  `null` stops it. */
export interface PreviewEmoteRequest {
  kind: 'previewEmote'
  urn: string | null
}

/** Turn the preview avatar about its vertical axis by `degrees` (the page's drag over it: the engine's
 *  drag-lock doesn't reach the preview in every host, so the page drives the turn itself). */
export interface PreviewRotateRequest {
  kind: 'previewRotate'
  degrees: number
}

/** Zoom the preview avatar's camera: `zoom` × the framing in use (1 is the framing as it is, up to
 *  PREVIEW_MAX_ZOOM), keeping what is at `anchor` where it is: a height in the camera's view, 0 its top and 1
 *  its bottom (the pointer's, as the page's wheel over the avatar asks). The view stays inside the framing. */
export interface PreviewZoomRequest {
  kind: 'previewZoom'
  zoom: number
  anchor: number
}

/** The furthest the preview avatar's camera zooms in: the head framing is 2.5× the whole body. */
export const PREVIEW_MAX_ZOOM = 2.5

/** RGB color, components 0..1 (matches PBAvatarBase / the deployed avatar entity). */
export interface RGBColor {
  r: number
  g: number
  b: number
}

/** A saved avatar look (backpack Outfits tab). The shape mirrors the catalyst `outfits` entity's
 *  per-slot outfit so a saved look can later be deployed unchanged (Phase 2); Phase 1 persists
 *  these locally (localStorage in the bridge scene). */
export interface Outfit {
  bodyShape: string
  eyes: { color: RGBColor }
  hair: { color: RGBColor }
  skin: { color: RGBColor }
  wearables: string[]
  forceRender: string[]
}

/** One saved outfit at a fixed slot index (0-based). */
export interface OutfitSlot {
  slot: number
  outfit: Outfit
  /** A picture of the outfit worn (PNG data URL), kept on this device only. */
  thumbnail?: string
}

/** The player's saved outfits + owned DCL names that unlock extra slots (beyond the 5 free). */
export interface OutfitsMetadata {
  outfits: OutfitSlot[]
  namesForExtraSlots: string[]
}

export interface OutfitsMessage {
  kind: 'outfits'
  metadata: OutfitsMetadata
}

export interface GetOutfitsRequest {
  kind: 'getOutfits'
}

/** Save the player's CURRENT look into a slot (scene captures getPlayer(); re-emits outfits). */
export interface SaveOutfitRequest {
  kind: 'saveOutfit'
  slot: number
}

/** Remove a saved outfit slot (re-emits outfits). */
export interface DeleteOutfitRequest {
  kind: 'deleteOutfit'
  slot: number
}

/** Equip a saved outfit: apply its body shape, colors and wearables to the profile. */
export interface EquipOutfitRequest {
  kind: 'equipOutfit'
  slot: number
}

/** Put a random look from the owned and base wearables on the Backpack's look, keeping the body
 *  shape and colors (re-emits wearables and outfitHistory). */
export interface RandomizeOutfitRequest {
  kind: 'randomizeOutfit'
}

/** Go back to the look before the last randomize, undo or redo of the Backpack session. */
export interface UndoOutfitRequest {
  kind: 'undoOutfit'
}

/** Go forward to the look the last undo left. */
export interface RedoOutfitRequest {
  kind: 'redoOutfit'
}

/** Forget the randomize history and the cached catalog: the Backpack closed. */
export interface ResetOutfitHistoryRequest {
  kind: 'resetOutfitHistory'
}

/** Whether the randomize bar's undo and redo have a step, and whether a roll is loading. */
export interface OutfitHistoryMessage {
  kind: 'outfitHistory'
  canUndo: boolean
  canRedo: boolean
  rolling: boolean
}

export interface GetEmotesRequest {
  kind: 'getEmotes'
}

export interface TriggerEmoteRequest {
  kind: 'triggerEmote'
  urn: string
}

export interface GetSettingsRequest {
  kind: 'getSettings'
}

export interface SetSettingRequest {
  kind: 'setSetting'
  name: string
  value: number
}

// ---- input bindings --------------------------------------------------------
// Wire types pinned to crates/common/src/inputs.rs (hand-written: the Rust types are
// bevy-dependent so ts-rs generation can't cover them).

/** Externally-tagged serde `Action`: `{System:'Map'}` | `{Scene:'IaJump'}`. */
export type ActionWire = { System: string } | { Scene: string }
/** `InputIdentifier` string form: 'KeyZ' | 'Mouse Left' | 'Gamepad South' | 'MouseWheel Down' … */
export type InputIdentifierWire = string
/** One row of the engine's binding table. */
export type BindingEntry = [ActionWire, InputIdentifierWire[]]

export interface GetBindingsRequest {
  kind: 'getBindings'
}

/** Whole-table replace: must round-trip every entry from the last BindingsMessage
 *  (gamepad/mouse/analog rows included), with just the intended edits applied. */
export interface SetBindingsRequest {
  kind: 'setBindings'
  bindings: BindingEntry[]
}

/** Reset the whole table to engine defaults (runs /reset_controls engine-side). */
export interface ResetBindingsRequest {
  kind: 'resetBindings'
}

/** Ask the engine for the next physical input pressed (press-a-key capture). The engine has
 *  no cancel: a dismissed capture resolves on the next input, so responses carry the request
 *  id and stale ids are dropped on both sides. */
export interface CaptureInputRequest {
  kind: 'captureInput'
  id: string
}

/** HUD focus state (fire-and-forget, latest wins). `ui`: a HUD surface (menu/popup) is
 *  active — the engine reserves all input above scenes, but keeps resolving system actions
 *  so the HUD still receives Cancel/hotkeys on the action stream. `text`: a HUD text field
 *  holds keyboard focus — keys are typing; the engine resolves no actions from them.
 *  `scroll`: the cursor is over a scrollable HUD element — the engine reserves the Scroll
 *  ACTIONS, so every input bound to them (wheel, key, gamepad button) stands down for
 *  world consumers (camera zoom on a shared wheel) while the action stream still resolves
 *  Scroll for the HUD to drive the hovered panel. `covered`: a full-screen HUD surface (a
 *  menu page, the loading overlay) hides the world — the engine tells scenes they are hidden
 *  (EngineInfo.scene_hidden). `menu`: the open full-screen menu page, named by the engine
 *  SystemAction that toggles it ('Map', 'Backpack', ...), else null — the engine answers a
 *  scene's openExplorerUi from it and writes the page's opened/closed events. */
export interface UiFocusMessage {
  kind: 'uiFocus'
  ui: boolean
  text: boolean
  scroll: boolean
  covered: boolean
  menu: string | null
}

/** One interaction hint (a single key binding) for a world entity: the button to press + its label.
 *  Shared by the reticle hover and the proximity tooltips. React maps `button` (an `InputAction` enum)
 *  to a glyph (E / 🖱 / 1…). */
export interface HoverAction {
  button: number
  text: string
  /** false → out of range (shown greyed with a "get closer" hint instead of the key glyph). */
  enabled: boolean
  /** Only meaningful when `enabled` is false — which distance rule gates the action: 'camera' (only
   *  `maxCameraDistance` set) → camera glyph + "Get camera closer"; 'player' (`maxDistance` / its
   *  deprecated alias `maxPlayerDistance`, incl. the implicit 10m default) → walking glyph +
   *  "Get closer". Defaults to 'player'. */
  tooFarReason?: 'camera' | 'player'
}
/** Interaction hints for the entity under the reticle. Empty array = nothing hovered. The tooltip's
 *  screen position is derived in React from the DOM cursor (free) or the centre (pointer-locked), so
 *  no coordinates travel on the wire. */
export interface HoverMessage {
  kind: 'hover'
  actions: HoverAction[]
}

/** Whether the engine has grabbed the mouse for camera-look (OS cursor hidden) → draw the
 *  center crosshair. Derived from PrimaryPointerInfo.screenCoordinates being absent. */
export interface CursorLockMessage {
  kind: 'cursorLock'
  locked: boolean
}

/** A HUD-relevant engine system action edge. `action` is the engine's SystemAction variant name
 *  (e.g. 'Places', 'Map', 'Cancel'); both press and release edges are relayed. Relayed
 *  authoritatively from the engine's input stream so it works even while the engine holds
 *  keyboard focus, and respects user rebinds. */
export interface SystemActionMessage {
  kind: 'systemAction'
  action: string
  pressed: boolean
}

/** The engine's full binding table (sent on request and after every set/reset). */
export interface BindingsMessage {
  kind: 'bindings'
  bindings: BindingEntry[]
}

/** Result of a CaptureInputRequest: the input that was pressed, tagged with the request id. */
export interface InputCapturedMessage {
  kind: 'inputCaptured'
  id: string
  input: InputIdentifierWire
}

/** A proximity tooltip for an in-range world entity, anchored at its projected screen position
 *  (the bridge does the world→screen projection each frame). */
export interface ProximityTip {
  id: number
  /** Screen pixel coords of the entity (tooltip is centered on this). */
  x: number
  y: number
  actions: HoverAction[]
}
/** All in-range entity tooltips this frame (empty = none). */
export interface ProximityMessage {
  kind: 'proximity'
  tips: ProximityTip[]
}

// ---- photo mode (camera reel capture) --------------------------------------

/** Photo mode as the engine reports it (the `PhotoModeState` stream): whether the in-world camera
 *  is open, what the last photo is doing, the player's storage, and a one-shot `event` stamped
 *  with `eventSeq` (a capture carries its preview JPEG, a failed upload its message). */
export interface PhotoModeSnapshot {
  active: boolean
  hudHidden: boolean
  cursorLocked: boolean
  phase: 'idle' | 'capturing' | 'encoding' | 'uploading' | string
  storageCurrent: number
  storageMax: number
  storageLoaded: boolean
  event: 'none' | 'captured' | 'uploaded' | 'uploadFailed' | string
  eventSeq: number
  previewJpegBase64: string
  message: string
}

export interface PhotoModeMessage {
  kind: 'photoMode'
  state: PhotoModeSnapshot
}

/** Open or close photo mode (the engine's `/photo_mode`); `source` names what asked, for analytics. */
export interface SetPhotoModeRequest {
  kind: 'setPhotoMode'
  enable: boolean
  source: PhotoModeSource
}

export type PhotoModeSource = 'button' | 'shortcut'

/** Take a photo in photo mode (the engine's `/take_photo`). */
export interface TakePhotoRequest {
  kind: 'takePhoto'
}

/** A nearby avatar was clicked in the world → open their profile card. Only the address travels;
 *  React resolves the display name (nearby roster) and anchors the card at the live DOM cursor. */
export interface AvatarClickMessage {
  kind: 'avatarClick'
  address: string
}

export type SceneToPage =
  | BridgeReadyMessage
  | { kind: 'connectivity'; state: ConnectivitySnapshot }
  | { kind: 'connectivityRetry'; requestId: string; error?: string }
  | BridgeUnavailableMessage
  | VoiceChatStateMessage
  | CommunityVoiceUpdateMessage
  | ActiveCommunityVoiceChatsMessage
  | RpcResponse
  | HoverMessage
  | CursorLockMessage
  | SystemActionMessage
  | ProximityMessage
  | PhotoModeMessage
  | AvatarClickMessage
  | PlayerReadyEvent
  | AnalyticsEventMessage
  | LoginCodeMessage
  | SceneLoadingMessage
  | ChatRelayMessage
  | ChatReactionMessage
  | ConsoleReplyMessage
  | ChatVisibilityMessage
  | FocusChatMessage
  | MembersMessage
  | MenuVisibilityMessage
  | FriendsMessage
  | FriendActionFailedMessage
  | FriendActionDoneMessage
  | MutualFriendsMessage
  | FriendOnlineMessage
  | DmUserStateMessage
  | DmHistoryMessage
  | SettingsMessage
  | BindingsMessage
  | InputCapturedMessage
  | ProfileMessage
  | UserProfileMessage
  | ProfileChangedMessage
  | ProfileSavedMessage
  | OwnedNamesMessage
  | NotificationsMessage
  | NotificationsNewMessage
  | EmotesMessage
  | MicMessage
  | WearablesMessage
  | CatalogPageMessage
  | OutfitsMessage
  | OutfitHistoryMessage
  | CommunitiesMessage
  | CommunityBrowseMessage
  | CommunityMessage
  | VoiceActivityMessage
  | NearbyMutesMessage
  | CommunityActionFailedMessage
  | CommunitySaveResultMessage
  | CommunityPlaceOptionsMessage
  | CommunityDeletedMessage
  | TravelResultMessage
  | AvatarSaveFailedMessage
  | RequestFailedMessage
  | CommunityDetailMessage
  | CommunityInvitesMessage
  | CommunityInvitesCountMessage
  | CommunityJoinRequestMessage
  | CommunityRequestSettledMessage
  | CommunityMemberSectionMessage
  | InvitableCommunitiesMessage
  | CommunityInviteSentMessage
  | CommunityChannelsMessage
  | CommunityChannelAddedMessage
  | CommunityChannelRemovedMessage
  | CommunityOnlineMembersMessage
  | CommunityPresenceMessage
  | EventsMessage
  | EventAttendingMessage
  | EventPlacesMessage
  | MapMessage
  | HomeSceneMessage
  | WelcomeMessage
  | WelcomeAcceptedMessage
  | ExternalUrlOpenedMessage
  | PlayerPoseMessage
  | RealmInfoMessage
  | SceneInfoMessage
  | LobbyStageReadyMessage
  | LookShownMessage
  | GalleryMessage
  | GalleryPhotoMessage
  | GalleryWriteMessage
  | PermissionRequestMessage
  | PermissionWithdrawnMessage
  | PermissionUsedMessage
  | PermissionRulesMessage
  | WorldPasswordRequestMessage
  | WorldPasswordWithdrawnMessage
  | EditorSceneResultMessage
  | PortablesMessage

// ---- events ----------------------------------------------------------------

/** An event as the events service lists it for the signed-in user. Dates are ISO strings. */
export interface EventData {
  id: string
  name: string
  description: string | null
  image: string | null
  image_vertical: string | null
  start_at: string | null
  finish_at: string | null
  next_start_at: string | null
  next_finish_at: string | null
  /** Milliseconds. */
  duration: number | null
  x: number
  y: number
  /** The world's name when `world` is set. */
  server: string | null
  world: boolean
  user_name: string | null
  scene_name: string | null
  highlighted: boolean
  live: boolean
  /** The signed-in user asked to be reminded. */
  attending: boolean
  total_attendees: number
  recurrent: boolean
  recurrent_dates: string[]
  /** Who is at the event now, by address. */
  connected_addresses: string[]
  community_id: string | null
  place_id: string | null
}

/** A list of events (page → scene → events service, signed so each event carries `attending`).
 *  With `from` and `to` it is the events starting in that range; with `list` it is that list. */
export interface GetEventsRequest {
  kind: 'getEvents'
  /** Echoed in the answer, so the page drops an answer to a request it no longer waits for. */
  request: number
  /** UTC, `yyyy-MM-ddTHH:mm:ssZ`. */
  from?: string
  to?: string
  list?: 'live' | 'highlight'
  limit?: number
  offset?: number
}
export interface EventsMessage {
  kind: 'events'
  request: number
  events: EventData[]
  /** The read failed; `events` is empty. */
  failed?: boolean
}

/** Set or clear the signed-in user's reminder on an event (POST / DELETE …/attendees). */
export interface SetEventAttendingRequest {
  kind: 'setEventAttending'
  eventId: string
  attending: boolean
}
/** The reminder as it is after the request; `failed` when the service refused, with `attending`
 *  the value that was asked for. */
export interface EventAttendingMessage {
  kind: 'eventAttending'
  eventId: string
  attending: boolean
  failed?: boolean
}

/** A place an event is held at, for the event detail's location line. */
export interface EventPlace {
  id: string
  title: string
  world_name: string | null
  /** "x,y". */
  base_position: string | null
}
export interface GetEventPlacesRequest {
  kind: 'getEventPlaces'
  request: number
  placeIds: string[]
}
export interface EventPlacesMessage {
  kind: 'eventPlaces'
  request: number
  places: EventPlace[]
  failed?: boolean
}

// ---- envelope --------------------------------------------------------------

export type Envelope =
  | { to: 'scene'; msg: PageToScene }
  | { to: 'page'; msg: SceneToPage }
