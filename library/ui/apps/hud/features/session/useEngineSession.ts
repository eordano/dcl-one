// Top-level session orchestration: login → entering (scene loading) → world.
// Owns the driver and exposes the login flow + scene-loading state + phase.

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react'
import { clearLogins, getLogin, redirectToAuth, returnedFromAuth, rootAddress, type StoredLogin } from '@explorer-core/features/auth/sso'
import { hoverKey, proximityKey } from '@explorer-core/engine/pointerKeys'
import type { LoginDriver, NewAccountPick } from '@explorer-core/engine/driver'
import type { LaunchHostOptions } from '@explorer-core/engine/engineRpc'
import { recordVisit } from '@explorer-core/features/lobby/lobbyApi'
import type { PortableInfo, PortableState, SatelliteView } from '@explorer-core/engine/generated'
import type { AvatarLook } from '@explorer-core/engine/avatarEquip'
import type { PhotoModeSnapshot, PhotoModeSource, PreviewFocus } from '@explorer-core/engine/protocol'
import { clickButton, screenViewed, sessionId, track } from '@explorer-core/lib/analytics'
import { randomDefaultLook } from '@explorer-core/lib/guestName'
import { fetchStoredProfile, type StoredProfile } from '@explorer-core/features/login/storedProfile'
import { useStoredProfile } from '../login/useStoredProfile'
import { inShell, postToShell } from '@explorer-core/lib/shell'
import { storedLoginType, type LoginType } from '@explorer-core/features/session/sessionAnalytics'
import type { InteractableArea } from '../../lib/hudInset'
import type { FatalError } from '@explorer-core/features/error/fatalError'
import { createLoadingProgress } from '@explorer-core/features/session/loadingProgress'
import { useConnectivity } from './useConnectivity'
import { DEFAULT_REALM } from '@hud-host'
import { provideHudLogin, provideHudLogout, type HudLoginReply, type HudLoginRequest } from '@explorer-core/lib/hudAutomation'
import { checkRealm, realmCheckMessage } from '@explorer-core/lib/realmCheck'
import { entryRealm } from '@hud-host'
import { color3ToHex, hexToColor3 } from '@explorer-core/lib/color'
import { closeTopPopup, hasOpenPopup, showToast, subscribePopups, topPopupOrder } from '../../design'
import { requestDebugPanel } from '../debug/useDebugPanel'
import { emitFriendEvent } from '@explorer-core/features/friends/friendEvents'
import { toastFriendOnline, toastFriendResult } from '../friends/friendToasts'
import { clearNotificationToasts, queueNotificationToasts } from '../notifications/NotificationToast'
import { clearRewardPopups, queueRewardPopups } from '../rewards/RewardPopup'
import { portableKey, toastPortableChanges } from '../portables/portableToasts'
import { bootMode, IN_WORLD_SHOP, unattendedRun } from '@hud-host'
import { notificationTimeMs } from '@explorer-core/lib/notificationTime'
import { isCancelKey, isEditableTarget, setBindingsSnapshot, useBindingsSnapshot } from '../../lib/bindingLabels'
import { dispatchCancelLayer } from '@explorer-core/lib/cancelLayers'
import { isInputLocked, subscribeInputLock } from '@explorer-core/lib/inputLock'
import { applyProfileEdit } from '@explorer-core/engine/profileEdit'
import { COMMUNITY_REQUEST_NOTIFICATIONS } from '@explorer-core/features/communities/notifications'
import { peekProfile, profileChanged, receiveProfile, seedProfiles, setOwnProfile, setProfileRequester, updateProfile } from './profileStore'
import { useWindowKeyDown } from '../../lib/useWindowKeyDown'
import { getCursor } from '@explorer-core/features/pointer/cursorStore'
import { openProfileCard } from '../profileCard/ProfileCard'
import { formatConsoleReply, parseChatCommand } from '@explorer-core/features/chat/chatCommands'
import { applyLanguageSetting, clearAutoTranslate } from '@explorer-core/features/chat/translation'
import { clearPingMode, shouldPing } from '../chat/ping'
import { risingIds, rowKeys } from '@explorer-core/features/chat/lineIds'
import { communityChannel, communityIdOf } from '@explorer-core/engine/communityChannel'
import { mentionName, mentionsName } from '@explorer-core/engine/mention'
import { PREF, getPref, setPref } from '@explorer-core/lib/prefs'
import { PARCEL_METERS, isGenesisSatelliteView } from '@explorer-core/features/map/atlas'
import { EDITOR_BUILD } from '@explorer-core/features/editorHost/config'
import { editorEntry } from '@explorer-core/features/editorHost/source'
import { hudActsOn, type EditorHudMode } from '@explorer-core/features/editorHost/hudMode'
import { streamHere } from '../voiceChat/streamHere'
import { eventAnalytics } from '../events/eventFormat'
import { useEventsData, type EventsData } from '../events/useEventsData'
import { donatePlaceOf, donateSceneKey } from '@explorer-core/features/donate/place'
import type { DonatePlace } from '@explorer-core/hostContract'
import type {
  AvatarColorTarget,
  AppNotification,
  BindingEntry,
  ChangeRealmRequest,
  ChatMessage,
  ChatReaction,
  MessageReaction,
  Community,
  DmUserState,
  ActiveCommunityVoiceChat,
  CommunityVoiceUpdateMessage,
  VoiceChatActionWire,
  VoiceChatState,
  CommunityAction,
  CommunityMemberAction,
  CommunityDetailMessage,
  CommunityInvitesMessage,
  CommunityMemberSection,
  CommunityMemberSectionMessage,
  InvitableCommunitiesMessage,
  CommunityChannel,
  CommunityMember,
  Emote,
  BlockedUser,
  Friend,
  FriendAction,
  FriendRequest,
  GalleryMessage,
  GalleryPhoto,
  GalleryPhotoMeta,
  GalleryWriteMessage,
  ExternalUrlOpenedMessage,
  HoverAction,
  MinimapRotation,
  MinimapStyle,
  NavAction,
  NearbyMember,
  OutfitSlot,
  OutfitsMetadata,
  PageToScene,
  PermissionLevelChoice,
  PermissionRequestMessage,
  PermissionRuleLevel,
  PermissionRulesMessage,
  PermissionRuleValue,
  PermissionUsedMessage,
  WorldPasswordRequestMessage,
  ProximityTip,
  Profile,
  ProfileEdit,
  CommunitySaveResultMessage,
  CommunityPlaceOptionsMessage,
  UpdateCommunityRequest,
  SceneLoadingState,
  Setting,
  TeleportRequest,
  Wearable
} from '@explorer-core/engine/protocol'

// Set by the wasm via boot.js __setEngineTextFocus (web) / the CEF relay (native): true while
// an engine-rendered text field (e.g. a scene textinput) holds keyboard focus. Those fields
// live on the canvas, so document.activeElement can't see them.
type EngineFocusWindow = Window & { __engineTextFocus?: boolean }

// Unity's wording (CommunityCardController *_ERROR_MESSAGE); the raw HTTP error only goes to the log.
const COMMUNITY_ERROR: Record<CommunityAction, string> = {
  join: 'There was an error joining the community. Please try again.',
  requestToJoin: 'There was an error requesting to join community. Please try again.',
  cancelJoinRequest: 'There was an error cancelling join request. Please try again.',
  leave: 'There was an error leaving the community. Please try again.',
  delete: 'There was an error deleting the community. Please try again.',
  removePlace: 'There was an error deleting the community place. Please try again.',
  // the member menu's (MembersListController *_ERROR_TEXT)
  promote: 'There was an error adding moderator to user. Please try again.',
  demote: 'There was an error removing moderator from user. Please try again.',
  transfer: 'There was an error transfering ownership. Please try again.',
  kick: 'There was an error kicking the user. Please try again.',
  ban: 'There was an error banning the user. Please try again.',
  unban: 'There was an error unbanning the user. Please try again.',
  fetch: 'There was an error loading the community. Please try again.',
  // answers to invites and requests
  acceptInvite: 'There was an error accepting community invitation. Please try again.',
  rejectInvite: 'There was an error rejecting community invitation. Please try again.',
  manageRequest: 'There was an error managing the user request. Please try again.'
}

// Answers to invites and requests, and an unban, are toasted rather than shown on a card: the
// Invites & Requests section has none.
const TOASTED_COMMUNITY_ACTIONS: ReadonlySet<CommunityAction> = new Set(['acceptInvite', 'rejectInvite', 'manageRequest', 'unban'])

// The texts for the Invites & Requests section's reads and the Invite to Community submenu.
export const INVITES_LOAD_ERROR: Record<CommunityInvitesMessage['failed'][number], string> = {
  invites: 'There was an error loading invites. Please try again.',
  requestsReceived: 'There was an error loading requests received. Please try again.',
  requestsSent: 'There was an error loading requests. Please try again.'
}
export const INVITE_SENT = 'Invite to Community Sent'
const INVITE_FAILED = 'Error sending invitation. Please try again.'
const INVITABLE_FAILED = "Error loading 'Invite to Community' menu option. Reopen menu to try again."
// A community member as the profile store keeps them: an unclaimed name with its #tag, as every
// explorer shows it (the card reads a bare name as claimed).
function memberProfiles(members: CommunityMember[]): Parameters<typeof seedProfiles>[0] {
  return members.map((m) => ({ address: m.address, name: m.hasClaimedName === true || m.name.includes('#') ? m.name : `${m.name}#${m.address.slice(-4)}`, picture: m.picture, claimed: m.hasClaimedName }))
}

// The engine's clock starts at 10:00 and runs at 12× (crates/visuals/src/day_night.rs start_clock).
// Fallbacks only: the menu reads the live clock with `/time` when it opens.
const SKYBOX_START_HOURS = 10
const SKYBOX_DAY_SPEED = 12

// The engine's `/time` reply (day_night.rs timeofday_console_command) ends with
// "speed <S> (elapsed: <seconds since midnight>)".
function parseTimeReply(reply: string): { hours: number; speed: number } | null {
  const m = /speed (-?[\d.]+) \(elapsed: ([\d.]+)\)/.exec(reply)
  return m ? { hours: Number(m[2]) / 3600, speed: Number(m[1]) } : null
}

// Unity's wording (BackpackEquipStatusController's in-world warning) when a profile deploy fails.
export const SAVE_FAILED_MESSAGE = 'There was an error updating your avatar profile. Please try again.'

/** A server-side catalog page request (backpack grid). Filters/sort are applied by the catalyst. */
export interface CatalogQuery {
  page: number
  pageSize: number
  category?: string
  search?: string
  orderBy?: 'date' | 'rarity' | 'name'
  direction?: 'asc' | 'desc'
  collectiblesOnly?: boolean
  smartOnly?: boolean
}

export interface BackpackState {
  /** The current catalog page (server-side paginated; drives the grid). */
  list: Wearable[]
  /** Total matching items for the active filters (drives the pager). */
  total: number
  /** True while a catalog page is in flight. */
  loading: boolean
  /** Request a catalog page (page + filters); the newest response wins (stale ones are dropped). */
  query: (q: CatalogQuery) => void
  equipped: Wearable[]
  open: boolean
  toggle: () => void
  /** Put a full equipped set on the Backpack's look (the explicit Equip action); deployed on close. */
  equip: (urns: string[]) => void
  /** The avatar's current body shape urn (items without a representation for it can't render). */
  bodyShape?: string
  /** Why deploying the look failed, or null (until Retry or Revert). */
  saveError: string | null
  /** Deploy the look again now. */
  retrySave: () => void
  /** Drop the look and go back to the last one that deployed. */
  revertSave: () => void
  /** The avatar's skin/hair/eye colors as hex; each present once the bridge has reported it. */
  colors?: { skin?: string; hair?: string; eyes?: string }
  /** Change one color: shown at once, deployed with the look when the Backpack closes. */
  setColor: (target: AvatarColorTarget, hex: string) => void
  /** Preview a set on the avatar without equipping it (selecting); null reverts to the look. */
  preview: (urns: string[] | null) => void
  /** Frame part of the preview avatar (follows the selected category). */
  focus: (focus: PreviewFocus) => void
  /** Where the Shop draws its stage image on screen, for the scene to draw behind the preview avatar;
   *  null gives the preview the Backpack's backdrop back. */
  stage: (rect: { x: number; y: number; width: number; height: number } | null) => void
  /** Play an emote on the preview avatar (again for every call); null stops it. */
  emote: (urn: string | null) => void
  /** Turn the preview avatar by `degrees` about its vertical axis (a drag over it). */
  rotate: (degrees: number) => void
  /** Zoom the preview avatar's camera: `zoom` × the framing in use, 1 to PREVIEW_MAX_ZOOM, keeping the height
   *  `anchor` (0 top, 1 bottom of the camera's view) where it is (the wheel over it). */
  zoom: (zoom: number, anchor: number) => void
  /** Categories shown even though an equipped item hides them. */
  forceRender: string[]
  /** Replace the force-render list (shown at once, deployed with the look on close). */
  setForceRender: (categories: string[]) => void
  /** Saved outfits (Outfits tab), by slot index. */
  outfits: OutfitSlot[]
  /** Number of outfit slots available (5 free + 1 per owned DCL name, capped at 10). */
  outfitSlots: number
  /** The saved outfits have arrived: until then every slot reads empty, saved or not. */
  outfitsLoaded: boolean
  /** Save the current look into a slot. */
  saveOutfit: (slot: number) => void
  /** Delete a saved outfit slot. */
  deleteOutfit: (slot: number) => void
  /** Equip a saved outfit (applies its body shape, colors and wearables). */
  equipOutfit: (slot: number) => void
  /** The randomize bar's state for this Backpack session. */
  outfitHistory: OutfitHistory
  /** Put a random look from the owned and base wearables on (keeps body shape and colors). */
  randomizeOutfit: () => void
  /** Go back to the look before the last randomize, undo or redo. */
  undoOutfit: () => void
  /** Go forward to the look the last undo left. */
  redoOutfit: () => void
}

export interface OutfitHistory {
  canUndo: boolean
  canRedo: boolean
  /** A roll is loading; the bar stays locked until it lands. */
  rolling: boolean
}

const NO_OUTFIT_HISTORY: OutfitHistory = { canUndo: false, canRedo: false, rolling: false }

/** What the create form sends. */
export interface CreateCommunityInput {
  name: string
  description: string
  privacy: 'public' | 'private'
  discoverable: boolean
  /** The picture as a data URL, when one was chosen. */
  thumbnail?: string
  /** The linked places, lands first, then worlds. */
  placeIds: string[]
}

/** What the edit form sends: the community and only the fields that changed. */
export type UpdateCommunityInput = Omit<UpdateCommunityRequest, 'kind'>

/** A community create or update in flight (`saving`) or its outcome (`done`). */
export type CommunitySave = { status: 'saving' } | { status: 'done'; result: CommunitySaveResultMessage }

/** The Communities page's browse grid: the pages read so far for one search. */
export interface CommunityBrowse {
  /** The search the grid shows; '' is every community. */
  search: string
  items: Community[]
  /** How many communities the search has in all (the service's total). */
  total: number
  /** The first page is being read. */
  loading: boolean
  /** A further page is being read. */
  loadingMore: boolean
  /** The first page could not be read: the grid shows no count and no empty state. */
  failed?: boolean
}

/** The toast when a browse page can't be read. */
export const BROWSE_ERROR = 'There was an error loading Communities. Please try again.'

const NO_BROWSE: CommunityBrowse = { search: '', items: [], total: 0, loading: false, loadingMore: false }

export interface CommunitiesState {
  /** My Communities: every community the user is a member of. */
  list: Community[]
  /** The browse grid, a page at a time. */
  browse: CommunityBrowse
  /** Read the grid's first page for a search ('' for every community), replacing the grid. */
  loadBrowse: (search: string) => void
  /** Read the grid's next page, when there is one and no read is running. */
  loadMoreBrowse: () => void
  open: boolean
  toggle: () => void
  /** Create a community (name, description, Public/Private, discoverable, picture as a data URL).
   *  Its outcome arrives in `save`. */
  create: (input: CreateCommunityInput) => void
  /** Delete a community the user owns. On success its id arrives in `deleted`. */
  remove: (id: string) => void
  /** The last community deleted, so its card closes. */
  deleted: string | null
  /** Save an edit (only the changed fields). Its outcome arrives in `save`. */
  update: (input: UpdateCommunityInput) => void
  /** The create or update in flight, or how the last one ended, for the form. */
  save: CommunitySave | null
  /** Forget the last outcome (the form closed or is being edited again). */
  clearSave: () => void
  /** The form's places: the user's own (the picker) and, when editing, the community's linked
   *  ones. Null until they arrive. */
  placeOptions: CommunityPlaceOptionsMessage | null
  /** Read the form's places; `communityId` when editing. */
  loadPlaceOptions: (communityId?: string) => void
  /** Forget them (the form closed). */
  clearPlaceOptions: () => void
  /** Unlink a place from a community (the card's delete). A failure arrives in `error`. */
  removePlace: (id: string, placeId: string) => void
  /** An owner's or moderator's action on a member, from the card's member menu. The card's
   *  members are read again after it; a failure arrives in `error`. */
  memberAction: (id: string, address: string, action: CommunityMemberAction) => void
  join: (id: string) => void
  /** Ask to join a private community. */
  requestToJoin: (id: string) => void
  cancelRequest: (id: string, requestId: string) => void
  leave: (id: string) => void
  /** The last community action the social-api rejected, for that community's modal. */
  error: { id: string; message: string } | null
  /** Per-community detail (members/posts/places/events) for the open modal. */
  detail: CommunityDetailMessage | null
  /** Request a community's detail (call when its modal opens). */
  loadDetail: (id: string) => void
  /** Communities fetched one at a time for a card opened from outside the browse list (the
   *  chat's View Community), by id; the browse list is left to the Communities page. */
  cards: Readonly<Record<string, Community>>
  /** Fetch one community into `cards`. */
  fetch: (id: string) => void
  /** The browser's Invites & Requests section: null until read. */
  invites: CommunityInvitesMessage | null
  /** Read the Invites & Requests section (and its counter) again. */
  loadInvites: () => void
  /** How many invites and requests received wait on the user, for the sidebar's counter; null
   *  until read. */
  pendingCount: number | null
  /** Read the counter alone (the Communities page opening). */
  loadInvitesCount: () => void
  /** Accept, reject or cancel an invite or a join request. `address` names the member when an
   *  owner or moderator answers their request or cancels their invite; without it the request is
   *  the user's own invite. A failure is toasted. */
  manageRequest: (id: string, requestId: string, intention: 'accepted' | 'rejected' | 'cancelled', address?: string) => void
  /** The open card's Members tab sections (Banned, Requests, Invites), as last read. */
  memberSections: Partial<Record<CommunityMemberSection, CommunityMemberSectionMessage>>
  /** The card for a community opened or closed (null), so a failure on it shows on the card. */
  cardOpen: (id: string | null) => void
  /** Read one of the open card's Members tab sections. */
  loadMemberSection: (id: string, section: CommunityMemberSection) => void
  /** The communities the user may invite the person last asked about to, or null until read. */
  invitable: InvitableCommunitiesMessage | null
  /** Read the communities the user may invite someone to (the Invite to Community submenu). */
  loadInvitable: (address: string) => void
  /** Invite someone to a community; its outcome is toasted. */
  invite: (id: string, address: string) => void
  /** Lift a member's ban (the card's Banned section); the section is read again after it. */
  unban: (id: string, address: string) => void
}

export interface MapState {
  /** Local player's current parcel. */
  x: number
  y: number
  open: boolean
  toggle: () => void
  teleport: (x: number, y: number) => void
  /** Teleport to a Genesis City place: from outside Genesis the parcel carries the Genesis realm,
   *  so the engine changes realm first. In Genesis already it is a plain teleport — a realm-carrying
   *  teleport is a full realm reconnect, like changeRealm, even to the realm the player is in. */
  teleportToPlace: (x: number, y: number) => void
  /** Travel to a world/realm by name (e.g. `boedo.dcl.eth`). */
  changeRealm: (realm: string) => void
}

/** Live player pose for the minimap: position in world metres, yaws in degrees. */
export interface PlayerPose {
  x: number
  z: number
  /** Avatar heading — drives the map arrow. */
  yaw: number
  /** Camera heading — drives the "rotate with camera" mode. */
  camYaw: number
}

export interface MinimapState {
  /** The pose stream, arriving ~20/s. Deliberately a ref rather than state: the minimap
   *  animates it from a RAF loop, and routing 20 updates/s through React state would
   *  re-render the whole HUD tree for a transform the DOM can apply directly. */
  pose: { current: PlayerPose }
  /** The satellite map the realm advertises, null when it has none. Only Genesis City
   *  advertises GENESIS_SATELLITE_VIEW; anywhere else (Worlds, local scenes) the minimap forces
   *  the engine-rendered Camera style and hides the style picker. */
  satelliteView: SatelliteView | null
  /** Title of the scene the player is standing in, for the header. Empty on an undeployed
   *  parcel (the header falls back to "Empty parcel"). Updates as the player crosses into
   *  another scene — unlike the entry overlay's title, which never does. */
  sceneTitle: string
  /** The place the player stands in, as Donate names it (a Genesis City parcel or a World); null in a preview,
   *  a local realm or between scenes. */
  donatePlace: DonatePlace | null
  /** Relay the current style/rotation/zoom to the scene. Only the Camera style needs the
   *  engine; on the DOM styles the scene disposes its TextureCamera. */
  setConfig: (config: { style: MinimapStyle; rotation: MinimapRotation; visibleMeters: number }) => void
}

// Places browses the places API over HTTP (no bridge data) — it only needs open/close.
export interface PlacesState {
  open: boolean
  toggle: () => void
}

/** What opened the Events page: its key, the menu's top bar, or the sidebar. */
export type EventsOpenSource = 'shortcut' | 'start_menu' | 'sidebar'

export interface EventCommunity {
  id: string
  name: string
  thumbnail: string
}

// Events reads the events service through the bridge, signed, so each event carries the user's
// reminder.
export interface EventsState {
  open: boolean
  toggle: (source?: EventsOpenSource) => void
  data: EventsData
  memberOf: EventCommunity[]
}

// The Shop browses the public marketplace catalog over HTTP, like Places and Events.
export interface ShopState {
  open: boolean
  toggle: () => void
}

// Unity's sidebar Skybox menu: time of day, driven through the engine's `/time <hours> <speed>`.
export interface SkyboxState {
  open: boolean
  toggle: () => void
  /** Hour of day shown on the slider, 0–24. */
  hours: number
  /** The sky follows the engine's day cycle; the slider is locked while on. */
  progressing: boolean
  setHours: (hours: number) => void
  setProgressing: (on: boolean) => void
}

/** Photo mode: the camera reel's in-world camera, owned by the engine (see the photoMode stream). */
export interface PhotoModeState {
  /** The engine's last report; `null` before the stream connects. */
  snapshot: PhotoModeSnapshot | null
  active: boolean
  /** The last photo taken, for the capture effect: the snapshot's `captured` pulse lasts one
   *  engine frame and the next report can overtake it before React renders. */
  capture: { seq: number; preview: string } | null
  /** Open or close photo mode (`/photo_mode`); `source` is for analytics only. */
  set: (enable: boolean, source: PhotoModeSource) => void
  /** Take a photo (`/take_photo`). */
  take: () => void
}

/** One page of a camera reel, newest first. `total` is how many photos the reel holds in all, `max`
 *  the owner's storage limit. `received` counts the images the service sent, url-less ones included
 *  (`photos.length` when absent). */
export interface ReelPage {
  ok: boolean
  photos: GalleryPhoto[]
  total: number
  max: number
  received?: number
}

/** The outcome of a camera-reel write; a delete also reports the storage usage that follows it. */
export interface ReelWrite {
  ok: boolean
  current?: number
  max?: number
}

export interface GalleryState {
  open: boolean
  toggle: () => void
  /** One page of the local player's own reel (signed, so private photos are included). */
  fetchPage: (offset: number, limit: number) => Promise<ReelPage>
  /** Per-photo metadata cache (place + people), filled lazily when a detail view opens. */
  metas: Record<string, GalleryPhotoMeta | null | undefined>
  /** Fetch one photo's full metadata (populates `metas`). */
  loadPhoto: (id: string) => void
  setPublic: (id: string, isPublic: boolean) => Promise<ReelWrite>
  remove: (id: string) => Promise<ReelWrite>
}

export interface EmotesState {
  list: Emote[]
  open: boolean
  toggle: () => void
  play: (urn: string) => void
  /** Assign an owned emote to a wheel slot (0–9); urn:'' clears the slot. */
  equip: (slot: number, urn: string) => void
}

export interface NotificationsState {
  list: AppNotification[]
  unread: number
  open: boolean
  toggle: () => void
  markAllRead: () => void
}

export interface SettingsState {
  list: Setting[]
  open: boolean
  toggle: () => void
  set: (name: string, value: number) => void
  /** Fetch the settings without opening the page (once per session). */
  load: () => void
}

export interface BindingsState {
  /** The engine's binding table (empty until the first `bindings` message lands). */
  list: BindingEntry[]
  /** Whole-table replace: pass the full edited table (see SetBindingsRequest). */
  set: (bindings: BindingEntry[]) => void
  /** Reset the whole table to engine defaults. */
  reset: () => void
  /** Press-a-key capture. `input` resolves with the InputIdentifier string; `cancel` abandons
   *  the capture (the engine still resolves it on the next input, but the result is dropped). */
  capture: () => { input: Promise<string>; cancel: () => void }
}

export interface ProfileState {
  data: Profile | null
  open: boolean
  toggle: () => void
  /** Claimed (NFT) names this account owns — the display-name picker's options. */
  ownedNames: string[]
  /** A save is in flight: the engine acks it only once the new profile version has deployed. */
  saving: boolean
  /** Why the last save failed, or null. */
  saveError: string | null
  /** Apply a partial edit to your own profile. Omitted fields are left alone. */
  save: (edit: ProfileEdit) => void
  /** Deploy a claimed name's colour without the edit form: a failure reverts it with no error. */
  saveNameColor: (color: NonNullable<ProfileEdit['nameColor']>) => void
  requestOwnedNames: () => void
  dismissSaveError: () => void
}

export interface FriendsState {
  /** false for guests / before the relationship snapshot is seeded. */
  available: boolean
  list: Friend[]
  received: FriendRequest[]
  sent: FriendRequest[]
  blocked: string[]
  /** The blocked accounts with their profiles (display); `blocked` stays the address set. */
  blockedUsers: BlockedUser[]
  /** Lowercased addresses of users who have blocked the local user. */
  blockedBy: string[]
  /** Signed in, social service still starting. */
  loading: boolean
  open: boolean
  toggle: () => void
  /* TODO: split domain data (queries) from commands — act/toggle don't belong in "State".
   Expose commands as an imperative service/context (like the popup service), not prop-drilled. (#18) */
  /** accept/reject/cancel/delete/block/unblock a user (guest-disabled in-engine). */
  act: (op: FriendAction, address: string, message?: string) => void
  /** In-flight actions as `op:address` (lowercase). */
  pending: ReadonlySet<string>
  /** Mutual friends by lowercase address, filled by loadMutuals. */
  mutuals: Record<string, MutualFriend[]>
  loadMutuals: (address: string) => void
}

export interface MutualFriend {
  address: string
  name: string
  picture?: string
}

/** The stored rules for the scene the player stands in: its hash, the realm url they were read
 *  under (what a rule row's subject carries), and the rules at each level. */
export type PermissionRules = Omit<PermissionRulesMessage, 'kind'>

/** A "This visit" answer the player gave for `ty` in `scene` (a prompt, or the review). */
export type VisitAnswer = Omit<PermissionUsedMessage, 'kind'>

/** The engine's latest outcome for a permission the scene used this visit. */
export interface TriggeredPermission {
  ty: string
  allowed: boolean
}

/** What the review dialog is about: a visit answer ('visit') or a stored rule at its level; `value`
 *  what the answer or the rule says. */
export interface PermissionSubject {
  ty: string
  sceneName: string
  scene: string
  realm: string
  scope: 'visit' | 'scene' | 'realm' | 'global'
  value: PermissionRuleValue
}

export interface PermissionsState {
  /** Outstanding scene permission prompts, oldest first (the HUD shows one at a time). A request
   *  the player already answered for 'visit' (same scene + type) is answered again without
   *  being queued, until the player leaves the scene or reloads it. */
  pending: PermissionRequestMessage[]
  /** The player's "This visit" answers for the scene they stand in, in order. A scene's answers
   *  are forgotten when the player leaves it or reloads it. */
  visit: VisitAnswer[]
  /** Permissions the scene used this visit, by type, with the engine's latest outcome. The list
   *  highlights the row that decided each: a visit answer, or the stored rule in force. */
  triggered: TriggeredPermission[]
  /** Refusals the player did not click themselves — the HUD answering from a visit deny, or a stored
   *  rule — by type, that the player has not opened the list for yet: the rail badge. */
  unseenDenied: number
  /** The list was opened: the badge clears until the next refused use. */
  markSeen: () => void
  /** The stored rules for the current scene, the realm and global, once `loadRules` answered. */
  rules: PermissionRules | null
  /** Ask the engine for the stored rules (the list is open). */
  loadRules: () => void
  /** Rewrite the rule for one permission and drop its uses from `used` — the next use reports the
   *  new outcome. 'visit' forgets the engine's stored rule and answers here from now on. */
  setRule: (subject: PermissionSubject, allow: boolean, level: PermissionRuleLevel) => void
  /** Clear one stored rule (the subject's level) and this visit's answer for the type. */
  clearRule: (subject: PermissionSubject) => void
  /** Forget every stored answer at every level, and this visit's: scenes ask again. */
  resetDefaults: () => void
  /** Allow/deny the request `id` at the chosen scope, then drop it from the queue. */
  resolve: (id: number, allow: boolean, level: PermissionLevelChoice) => void
}

export interface WorldPasswordState {
  /** The engine's open password prompt for the world being travelled to, if any. */
  pending: WorldPasswordRequestMessage | null
  /** Answer the prompt: the password, or null to give up (the travel is refused). */
  resolve: (password: string | null) => void
}

export interface PortablesState {
  /** The portables the engine lists, tagged by kind: never the HUD's own scene or a startup portable. */
  list: PortableInfo[]
  /** Stop a listed portable by its scene hash (`/stop_portable`). */
  stop: (hash: string) => void
  /** Start a stopped smart wearable again (`/smart_wearable restart`). */
  restart: (urn: string) => void
}

export type ChatLine = ChatMessage & {
  id: number
  ts: number
  /** In the order each emoji was first used. */
  reactions?: MessageReaction[]
}

/** `reactions` with one wallet's reaction added or removed. */
export function applyReaction(reactions: MessageReaction[], reaction: Pick<ChatReaction, 'emoji' | 'from' | 'remove'>): MessageReaction[] {
  const from = reaction.from.toLowerCase()
  const i = reactions.findIndex((r) => r.emoji === reaction.emoji)
  if (reaction.remove) {
    if (i < 0 || !reactions[i].from.includes(from)) return reactions
    const rest = reactions[i].from.filter((f) => f !== from)
    return rest.length > 0 ? reactions.map((r, j) => (j === i ? { ...r, from: rest } : r)) : reactions.filter((_, j) => j !== i)
  }
  if (i < 0) return [...reactions, { emoji: reaction.emoji, from: [from] }]
  if (reactions[i].from.includes(from)) return reactions
  return reactions.map((r, j) => (j === i ? { ...r, from: [...r.from, from] } : r))
}

/** An open DM tab. Tabs are HUD state only: nothing opens on boot, a tab appears when a DM
 *  arrives or is sent, or from Message on a profile, and closing it is local. */
export interface Conversation {
  /** Partner wallet, lowercase. */
  address: string
  /** DMs received on this tab while it was not the one shown (or chat was closed). */
  unread: number
  /** From the engine's per-tab state stream; null until the first report. */
  state: DmUserState | null
  online: boolean
}

/** An open community chat: a community the local user is a member of whose chat they have not
 *  closed. Its lines are the engine's stored history, read like a DM's. */
export interface CommunityConversation {
  id: string
  /** The channel: `community:<id>`. */
  key: string
  name: string
  thumbnail: string
  /** 'owner' | 'moderator' | 'member' */
  role: string
  /** Lines received while the tab was not the one shown (or chat was closed). */
  unread: number
  /** One of the unread lines @-mentions the local user; cleared with `unread`. */
  mention: boolean
  /** The members online now, once asked for (when the tab is first shown); null until then. */
  members: CommunityMember[] | null
  /** How many of `members` count: not the local user, not anyone they blocked. */
  online: number
  /** Community chat's state from the engine while this tab is shown (the private chat room, and
   *  the server's message router in it); null until the first report. */
  state: DmUserState | null
}

/** A chat line's tab: 'Nearby', the partner wallet in lowercase, or `community:<id>`. */
export function channelKey(channel: string): string {
  return channel === 'Nearby' ? channel : channel.toLowerCase()
}

/** A DM rides a wallet-named channel; anything else (Nearby, System) belongs to the Nearby list. */
export function isDmChannel(channel: string): boolean {
  return /^0x[0-9a-f]{40}$/i.test(channel)
}

function communityConversation(c: CommunityChannel): CommunityConversation {
  return { id: c.id, key: communityChannel(c.id), name: c.name, thumbnail: c.thumbnail, role: c.role, unread: 0, mention: false, members: null, online: 0, state: null }
}

/** One row per member, the last word on each address winning. */
function dedupeMembers(members: CommunityMember[]): CommunityMember[] {
  const byAddress = new Map(members.map((m) => [m.address.toLowerCase(), m]))
  return [...byAddress.values()]
}

/** A list of community ids kept per account under one pref key for every account: the chats an
 *  account closed, and the communities it had chats for. */
function readAccountList(key: string, account: string): string[] {
  try {
    const parsed: unknown = JSON.parse(getPref(key) ?? '{}')
    const list = parsed != null && typeof parsed === 'object' ? (parsed as Record<string, unknown>)[account] : undefined
    return Array.isArray(list) ? list.filter((id): id is string => typeof id === 'string') : []
  } catch {
    return []
  }
}
function writeAccountList(key: string, account: string, ids: string[]): void {
  let all: Record<string, unknown> = {}
  try {
    const parsed: unknown = JSON.parse(getPref(key) ?? '{}')
    if (parsed != null && typeof parsed === 'object') all = parsed as Record<string, unknown>
  } catch {}
  setPref(key, JSON.stringify({ ...all, [account]: ids }))
}

export interface ChatState {
  /** The lines of the channel shown (`channel`). */
  messages: ChatLine[]
  /** Sends to the channel shown: Nearby, or the open DM. */
  send: (text: string) => void
  /** Adds or removes the local user's reaction to a message on the channel shown. */
  react: (messageId: string, emoji: string, remove: boolean) => void
  /** The Nearby lines, whichever channel is shown. */
  nearby: ChatLine[]
  /** Redraws the bubble over a Nearby sender's head with other words, while it is still that message's. */
  bubbleText: (sender: string, messageId: string, message: string) => void
  /** 'Nearby' or a partner wallet (lowercase). */
  channel: string
  select: (channel: string) => void
  /** Open DM tabs, in the order they appeared. */
  conversations: Conversation[]
  /** Show a DM tab for `address` (adding it if needed), select it, and open chat. */
  openConversation: (address: string) => void
  /** Drop a DM tab; its history stays stored. */
  closeConversation: (address: string) => void
  /** Delete the stored history of a conversation (a partner or `community:<id>`) and clear its lines. */
  deleteHistory: (address: string) => void
  /** The community chats open, most recently active first. */
  communities: CommunityConversation[]
  /** Show a community's chat (reopening it if closed), select it, and open chat. */
  openCommunity: (community: CommunityChannel) => void
  /** Hide a community's chat until it is opened again; its history stays stored. */
  closeCommunity: (id: string) => void
  /** Visibility, toggled by the React sidebar chat icon. */
  open: boolean
  toggle: () => void
  /** Nearby players (drives the "Nearby · N" header + members list). */
  members: NearbyMember[]
  /** Lowercased addresses talking in voice chat right now (engine voice stream). */
  speaking: ReadonlySet<string>
  /** Open chat and queue an @name mention into the draft (from a profile card's "Mention"). */
  mention: (name: string) => void
  /** A queued @name waiting to be dropped into the chat draft (consumed by Chat), or null. */
  pendingMention: string | null
  /** Clear the queued mention once Chat has inserted it. */
  consumeMention: () => void
  /** Lines that landed where they could not be seen: with the chat closed, or open but idle on
   *  another channel (the rail and its counts are hidden while idle). Reset when the chat opens or
   *  becomes active. Drives the sidebar badge. */
  unread: number
  /** The panel reports whether it is active (hovered or focused, chrome shown). */
  setActive: (active: boolean) => void
  /** Nearby lines that landed while Nearby was not on screen (chat closed or a DM tab shown);
   *  reset when it is. Drives the badge on the rail's Nearby tab. */
  nearbyUnread: number
  /** Bumped on every engine "focus chat" request (Enter, even while idle-open) — Chat watches
   *  this to (re)focus the input beyond the open-transition case. */
  focusTick: number
  /** Open + (re)focus chat. Driven by the bridge's focusChat relay of the engine's Chat
   *  action (Enter), which fires regardless of where DOM focus sits. */
  requestFocus: () => void
}

const MAX_CHAT_LINES = 200
/** The notifications list keeps at most this many, the size of the bridge's full fetch. */
export const NOTIFICATIONS_KEPT = 50
// The least time between two notification pings of one conversation and kind: a flood of lines
// is one sound, not one per line.
const PING_INTERVAL_MS = 400

// Render-settle: after a scene first reports loaded, hold the loader at least this long for the
// engine to render the first frame (so the world isn't revealed as black models), and at most this
// long so a stuck/absent render probe never traps the loader. The cap is deliberately short: the
// `renderBusy` probe is best-effort (the engine build may not even expose `#shader-compiling`), so a
// long cap turned a missed probe into a multi-second frozen-looking hold with the engine idle.
const MIN_REVEAL_MS = 300
const MAX_REVEAL_MS = 1500
// While loading, the engine streams scenes and `visible` can briefly drop between one finishing
// and the next starting. Revealing the world in that gap flashes the HUD (chat + sidebar) in and
// out, so we only drop the loader once loading has been stably clear for this long.
const REVEAL_DEBOUNCE_MS = 600
// how long, back from the auth site, the stored sign-in waits for the catalyst to have the profile
const AUTH_PROFILE_WAIT_MS = 30_000
// the look pending on the stage while RANDOMIZE's fetch is under way (no look number yet)
const EXPECTED_LOOK = -1
// how long that fetch is waited for before RANDOMIZE is let go again
const EXPECTED_LOOK_MS = 15_000
export const BRIDGE_CALL_TIMEOUT_MS = 30_000
// camera-reel-service deletes the photo and its thumbnail from S3 in turn, up to 15 s each.
export const GALLERY_DELETE_TIMEOUT_MS = 60_000

type BridgeReply = GalleryMessage | GalleryWriteMessage | ExternalUrlOpenedMessage

export type LoginStatus =
  | 'loading'
  | 'sign-in-or-guest'
  | 'reuse-login-or-new'

export type SessionPhase = 'login' | 'welcome' | 'lobby' | 'picking' | 'entering' | 'world'

// Where the user chose to spawn after login (the post-jump-in Places picker). `null` = skip → the
// engine's default spawn (Genesis Plaza). A world switches realm; a parcel teleports once spawned.
export type Destination =
  | { kind: 'parcel'; x: number; y: number }
  | { kind: 'world'; realm: string; position?: string }
  | null

/** What the guest start page picked: the new guest's name and curated default look (`default<look>`). */
export type GuestPick = NewAccountPick

export interface LoginFlow {
  status: LoginStatus
  /** Root wallet address of the stored SSO identity (shown on the "welcome back" screen). */
  account: string | null
  busy: boolean
  error: string | null
  /** Engine has booted and can accept the login command. The engine-driven CTAs (Jump in / Explore)
   *  stay disabled until this is true so a click never lands in a silent wait. Always true for mock. */
  engineReady: boolean
  /** The lobby stage's avatar has loaded. The boot bar holds short of done until then. Always true
   *  without an engine (mock), or for a link's destination (straight into the world, no stage). */
  avatarLoaded: boolean
  /** Real boot progress (0–100, weighted) from the engine loader, for the login footer bar. */
  loadProgress: number
  /** Active boot step id ('download'|'compile'|'init'|'workers'|'gpu') or null. */
  loadStep: string | null
  /** Fresh sign-in: same-domain auth redirect (web) or the engine's remote-wallet flow (native). */
  startWithAccount: () => void
  /** Native fresh sign-in in flight → show the verification panel (code null until it arrives). */
  authPending: boolean
  authCode: string | null
  /** Abort the in-flight native fresh sign-in. */
  cancelLogin: () => void
  /** `pick`: the guest start page's name and default look, accepted with the terms once signed in.
   *  Before the engine is ready the click is kept (`acceptQueued`) and runs once it is. */
  exploreAsGuest: (pick?: GuestPick) => void
  acceptQueued: boolean
  /** Reuse the stored SSO identity (hand it to the engine). */
  jumpIn: () => void
  /** The stored account's profile, for the screens before the engine draws it. */
  profile: StoredProfile
  /** The last login failed because the user's profile couldn't be fetched — offer
   *  resetProfileAndJumpIn as the explicit recovery. */
  profileFetchFailed: boolean
  /** Retry the login, continuing with (and deploying) a default profile if the fetch still
   *  fails. PERMANENTLY REPLACES the account's existing profile — only offered after a
   *  profile-fetch failure, and the UI must make the consequence clear. */
  resetProfileAndJumpIn: () => void
  /** Sign in with a different account → auth site. */
  useDifferentAccount: () => void
}

/** The welcome page, shown after sign-in while the terms aren't accepted on this install or the
 *  account has no profile yet. */
export interface WelcomeFlow {
  /** The terms still need accepting (the world is held until they are). */
  terms: boolean
  /** The account has no profile: the name and look are its new one's, deployed on accept. */
  newProfile: boolean
  /** Signed in to the lobby, and not yet known whether the page is needed: the stage shows alone. */
  pending: boolean
  saving: boolean
  error: string | null
  /** Another of the curated default looks on the avatar, fetched here (deployed on accept). */
  reroll: () => void
  /** The curated default look `default<look>`, as fetched, on the stage's avatar: the guest start
   *  page's pick. */
  showLook: (look: number, avatar: AvatarLook) => void
  /** The look the next RANDOMIZE shows, for the stage to load out of sight meanwhile. */
  preloadLook: (look: number, avatar: AvatarLook) => void
  /** A look is on its way to the stage (asked for, not yet shown there): RANDOMIZE waits. */
  lookPending: boolean
  /** The look last put on the stage, for a fresh pick never to repeat (it would never show). */
  lastLook: number | null
  /** A look is about to be asked for (its fetch is under way): holds RANDOMIZE until `showLook`. */
  expectLook: () => void
  /** Accept the terms, and for a new profile deploy it under `name` with the look. */
  accept: (name?: string) => void
  /** Open the terms of use or the privacy policy in the browser. */
  openLegal: (doc: 'terms' | 'privacy') => void
}

/** The scene editor's handle on the session (features/editorHost). */
export interface EditorHostState {
  /** 'edit' takes the HUD chrome and its hotkeys away, 'play' leaves a player's minimum. */
  mode: EditorHudMode
  setMode: (mode: EditorHudMode) => void
}

/** Private calls and community streams: the engine's state, the live streams, and the commands. */
export interface VoiceChatSession {
  /** Null until the engine's first snapshot. */
  state: VoiceChatState | null
  /** Every live community stream the local user may see. */
  active: ActiveCommunityVoiceChat[]
  /** `active` has been read for this account: until then it says nothing about what is live. */
  activeLoaded: boolean
  /** The live stream whose places include where the player stands. */
  scene: ActiveCommunityVoiceChat | null
  /** A guest account: voice chat is refused. */
  guest: boolean
  act: (action: VoiceChatActionWire) => void
}

type VoiceChatEventListener = (event: CommunityVoiceUpdateMessage) => void
const voiceChatListeners = new Set<VoiceChatEventListener>()
/** Subscribe to community stream started / ended broadcasts (toasts); returns the unsubscribe. */
export function onVoiceChatEvent(listener: VoiceChatEventListener): () => void {
  voiceChatListeners.add(listener)
  return () => voiceChatListeners.delete(listener)
}
function emitVoiceChatEvent(event: CommunityVoiceUpdateMessage): void {
  for (const listener of voiceChatListeners) listener(event)
}

export interface EngineSession {
  phase: SessionPhase
  playerReady: boolean
  /** The engine has drawn the lobby stage and avatar, so the page's stand-in can fade out. */
  lobbyStageReady: boolean
  /** The lobby reopened in-world from the menu. */
  lobbyPage: { open: boolean; toggle: () => void; travel: (dest: Destination) => void }
  /** The Backpack avatar preview's on-screen rect (CSS px), while it is open. */
  avatarPreviewRect: { x: number; y: number; width: number; height: number } | null
  /** The persisted home destination the lobby's landing card shows (null before the engine knows). */
  homeScene: () => { realm: string | null; parcel: string } | null
  /** Post-jump-in Places picker: choose where to spawn (or null to skip → Genesis Plaza). False
   *  when nothing was done: no driver, or the lobby still signing in. */
  pickDestination: (dest: Destination) => boolean
  login: LoginFlow
  welcome: WelcomeFlow
  /** Entry overlay state (the sceneLoading stream): what is loading while `phase` is
   *  'entering'. NOT the scene the player is in — that is `minimap.sceneTitle`, resolved by
   *  parcel; this title is whatever was last loading and goes stale as the player moves. */
  sceneLoading: SceneLoadingState | null
  connectivity: ReturnType<typeof useConnectivity>
  /** One 0–100 value for the whole loader, across every stage it waits on; it only rises. */
  loadingProgress: number
  /** Why the last in-world travel failed (the engine kept the player where they were), shown as a notice. */
  travelError: string | null
  dismissTravelError: () => void
  /** The realm a HUD-requested travel is heading to, until the engine reports the outcome. The
   *  loader shows meanwhile. */
  travellingTo: string | null
  /** Fatal engine error → full-screen error popup. 'launch' = boot panic (fatal), 'runtime' =
   *  post-launch crash bridged from the engine watchdog (dismissable). null when healthy. */
  fatalError: FatalError | null
  /** Reload the whole page (error-popup action). */
  reload: () => void
  /** Dismiss a non-fatal (runtime) error popup. */
  dismissFatal: () => void
  /** World-entity hover hints (empty = nothing hovered). */
  hover: HoverAction[]
  /** Engine has grabbed the mouse for camera-look (OS cursor hidden) → show the crosshair. */
  cursorLocked: boolean
  /** In-range world-entity tooltips, anchored at projected screen coords. */
  proximity: ProximityTip[]
  chat: ChatState
  friends: FriendsState
  settings: SettingsState
  bindings: BindingsState
  profile: ProfileState
  notifications: NotificationsState
  runDebugCommand: (line: string) => Promise<string>
  skybox: SkyboxState
  emotes: EmotesState
  backpack: BackpackState
  communities: CommunitiesState
  map: MapState
  minimap: MinimapState
  places: PlacesState
  events: EventsState
  shop: ShopState
  gallery: GalleryState
  photoMode: PhotoModeState
  /** The Create page (features/editorHost/CreatePage); `show` opens or closes it without toggling. */
  create: { open: boolean; toggle: () => void; show: (open: boolean) => void }
  /** Scene permission prompts (e.g. ChangeRealm) awaiting an Allow/Deny. */
  permissions: PermissionsState
  /** The password prompt for a password-protected world being travelled to. */
  worldPassword: WorldPasswordState
  /** The portable experiences running beside the scene (smart wearables, scene-spawned portables). */
  portables: PortablesState
  editor: EditorHostState
  mic: { enabled: boolean; available: boolean; toggle: () => void }
  /** Saved mutes for nearby voice: lowercased addresses the user hushed, kept per account by the engine. */
  nearbyMutes: { muted: ReadonlySet<string>; set: (address: string, muted: boolean) => void }
  /** Private calls and community streams. */
  voiceChat: VoiceChatSession
  /** Trigger a sidebar nav action in the scene (open menu/popup, emotes, mic). */
  nav: (action: NavAction) => void
  /** Open an `https:` url in the user's browser through the engine; resolves whether it opened. */
  openExternalUrl: (url: string) => Promise<boolean>
  /** Report (or clear) the screen rect where the scene should render an engine view. */
  setEngineViewport: (
    region: 'map' | 'avatarPreview' | 'lobby',
    rect: { x: number; y: number; width: number; height: number } | null,
    dpr?: number
  ) => void
  /** Report the screen area the persistent HUD occupies (see lib/hudInset.ts). */
  setInteractableArea: (area: InteractableArea) => void
  /** Sign out → back to the login screen. */
  logout: () => void
  /** A full scene menu page is open → the React HUD (sidebar + chat) hides. */
  menuOpen: boolean
  closeAllPanels: () => void
}

/** Parse a camera-reel `dateTime` (unix seconds, unix ms, or ISO) to epoch ms for sort/grouping. */
export function photoTime(dateTime: string): number {
  if (/^\d+$/.test(dateTime)) {
    const n = Number(dateTime)
    return n < 1e12 ? n * 1000 : n // sub-1e12 → seconds, else already ms
  }
  const t = Date.parse(dateTime)
  return Number.isNaN(t) ? 0 : t
}

/** A `/hud_login` destination as the lobby picks one: a world (with its parcel), a parcel on the
 *  main realm, or none, which stays in the lobby. */
function hudDestinationOf(destination: HudLoginRequest['destination']): Destination {
  if (destination == null) return null
  const [x, y] = (destination.position ?? '').split(',').map((n) => parseInt(n.trim(), 10))
  const hasPosition = Number.isFinite(x) && Number.isFinite(y)
  if (destination.realm != null) return { kind: 'world', realm: destination.realm, position: hasPosition ? `${x},${y}` : undefined }
  return hasPosition ? { kind: 'parcel', x, y } : null
}

function initialMessages(): ChatLine[] {
  return [{ sender: '', message: 'Type /help for available commands.', channel: 'Nearby', messageId: '', id: -1, ts: Date.now() }]
}

export function useEngineSession(createDriver: () => LoginDriver): EngineSession {
  const driverRef = useRef<LoginDriver | null>(null)
  const [status, setStatus] = useState<LoginStatus>('loading')
  // Same-domain SSO identity read from localStorage (null = no stored account).
  const [stored, setStored] = useState<StoredLogin | null>(null)
  // How the pending login is reported once it resolves: 'stored' reads the stored identity's kind
  // then (it loads alongside the status), null is a scene-owned login the page never ran.
  const pendingLoginType = useRef<LoginType | 'stored' | null>(null)
  const lastLoginType = useRef<LoginType | null>(null)
  // the terms accepted on this install, as the welcome flow reported it (the engine knows otherwise)
  const termsOk = useRef(false)
  // The Google Ads conversion, once per page load (shell/googleTag.ts dedupes per tab session).
  const inWorldReported = useRef(false)
  const storedRef = useRef<StoredLogin | null>(null)
  storedRef.current = stored
  const welcomeRef = useRef<{ terms: boolean; newProfile: boolean } | null>(null)
  // The guest start page's pick went in with the sign-in, and its ACCEPT AND PLAY already accepted
  // the terms, so the welcome page is answered instead of shown.
  const guestPick = useRef<GuestPick | null>(null)
  // ACCEPT AND PLAY clicked before the engine was ready: runs once it is
  const [queuedGuest, setQueuedGuest] = useState<{ pick: GuestPick | null } | null>(null)
  // the look sent to the stage and not yet shown there (null = none)
  const [pendingLook, setPendingLook] = useState<number | null>(null)
  // The address of the engine's actual reusable previous login (drives "Welcome back").
  const [prevUserId, setPrevUserId] = useState<string | null>(null)
  const account = prevUserId ?? (stored ? rootAddress(stored.identity) : null)
  // The stored account's profile, for the screens before the engine draws it. The stored sign-in
  // waits for the answer: back from the auth site, the profile it just made can still be syncing
  // to the catalyst, which the engine's own fetch then asks (and waits for too, login_profile).
  const accountAddress = status === 'reuse-login-or-new' && account != null ? account : undefined
  const accountProfile = useStoredProfile(accountAddress, returnedFromAuth ? AUTH_PROFILE_WAIT_MS : 0)
  const accountChecking = accountAddress != null && !accountProfile.settled
  const [busy, setBusy] = useState(false)
  const busyRef = useRef(false)
  busyRef.current = busy
  const [error, setError] = useState<string | null>(null)
  // Engine boots (autostart) while the login screen is up; this flips true once it can take commands.
  const [engineReady, setEngineReady] = useState(false)
  const [avatarLoaded, setAvatarLoaded] = useState(false)
  // Real WASM-download/boot progress surfaced from the engine (0–100) + the active step id,
  // for the login footer bar. The engine's own loader is hidden (hideLoader=1).
  const [loadProgress, setLoadProgress] = useState(0)
  const [loadStep, setLoadStep] = useState<string | null>(null)
  // Fatal engine error → full-screen popup. 'launch' = boot panic (fatal, no dismiss); 'runtime' =
  // post-launch crash bridged from the engine watchdog (can be a false positive → dismissable).
  // ?simerror=1 (or =launch, =realm) seeds a sample so each surface can be iterated without a real
  // panic — 'realm' matters in mock, where the driver has no launch and the ?realm= pre-flight
  // (below) is skipped entirely.
  const [fatalError, setFatalError] = useState<FatalError | null>(() => {
    const sim = new URLSearchParams(location.search).get('simerror')
    if (sim == null) return null
    if (sim === 'realm') return { message: 'The world "noexiste.dcl.eth" doesn\'t exist. (simulated)', source: 'realm' }
    return {
      message: "panicked at crates/dcl_wasm/src/inner/mod.rs:41:9:\ncan't init wasm queue\n\n(simulated)",
      source: sim === 'launch' ? 'launch' : 'runtime'
    }
  })

  // Past login → waiting for the world.
  const [submitted, setSubmitted] = useState(false)
  // for a /hud_login, which may come between a render and the effect that hands it over
  const submittedRef = useRef(false)
  submittedRef.current = submitted
  const fatalErrorRef = useRef<FatalError | null>(null)
  fatalErrorRef.current = fatalError
  // The post-jump-in Places picker: stay in 'picking' until the user chooses a destination (or skips).
  const [destinationPicked, setDestinationPicked] = useState(false)
  // Signed in and shown the lobby, with the engine launched holding the world back (web) or already
  // running (native, mock). Ends once a destination lands; a failed one returns here.
  const [lobby, setLobby] = useState(false)
  const lobbyRef = useRef(false)
  const [lobbyStageReady, setLobbyStageReady] = useState(false)
  lobbyRef.current = lobby
  // The welcome page: asked of the engine after each sign-in, null until it answers (or once accepted).
  const [welcome, setWelcome] = useState<{ terms: boolean; newProfile: boolean } | null>(null)
  // the lobby waits for the answer, so it never shows only to be replaced by the page
  const [welcomePending, setWelcomePending] = useState(false)
  const [welcomeSaving, setWelcomeSaving] = useState(false)
  const [welcomeError, setWelcomeError] = useState<string | null>(null)
  // A ?position/?realm link the sign-in entered, and the destination whose world was launched held
  // for the terms (undefined = none): it goes there once the welcome page is done.
  const welcomeLink = useRef<Destination>(null)
  const heldDest = useRef<Destination | undefined>(undefined)
  // The engine launches once per page (boot.js), so after that a destination is a runtime travel.
  const launchedRef = useRef(false)
  // The last place recorded as visited (realm|title), so walking inside one scene records it once.
  const visitedTitle = useRef('')
  // The login captured on Jump in, run once the engine is launched (for the lobby, or straight at a
  // ?position/?realm destination).
  const pendingLogin = useRef<((driver: LoginDriver) => Promise<unknown>) | null>(null)
  // Counts sign-outs, so a sign-in the engine answers after one (runPendingLogin) is known stale.
  const signOuts = useRef(0)
  // A welcome page ask, tagged with the sign-out count it goes out under, which its answer echoes:
  // an answer to one from before a sign-out is the signed-out account's.
  const askWelcome = useCallback((driver: LoginDriver): void => {
    driver.send({ kind: 'getWelcome', ask: signOuts.current })
  }, [])
  // The sign-in `/hud_login` runs (lib/hudAutomation.ts, a test runner's), answered once it has
  // landed (`landed`: the engine took it) and the welcome page is done, or failed. The destination
  // it then goes to from the lobby, as a link's would.
  const hudLoginPending = useRef<{ resolve: (reply: HudLoginReply) => void; reject: (error: Error) => void; login: StoredLogin; landed: boolean; welcomeAwaited: boolean } | null>(null)
  const hudDestination = useRef<Destination>(null)
  // While the page's stored sign-in is a /hud_login's: the one it replaced, put back
  // (`restoreHudSignIn`) if the engine refuses the sign-in or never runs it, until the engine has
  // taken it (`landed`). It outlives the command's reply, which an error the engine may survive
  // gives at once, and the startup read of the stored sign-in, should it come late, lands in it
  // rather than over the runner's.
  const hudStored = useRef<{ replaced: StoredLogin | null; landed: boolean } | null>(null)
  const restoreHudSignIn = useCallback((): void => {
    const owned = hudStored.current
    if (owned == null || owned.landed) return
    hudStored.current = null
    setStored(owned.replaced)
    storedRef.current = owned.replaced
  }, [])
  // ticks as the pending sign-in moves on (landed, the welcome page in), so the effect that
  // answers it runs again where no state of its own changes (a link's page sets none)
  const [hudLoginTick, setHudLoginTick] = useState(0)
  // Answers the pending /hud_login, if any: with where it landed, or the error that ended it.
  const settleHudLogin = useCallback((outcome: HudLoginReply | Error): void => {
    const pending = hudLoginPending.current
    hudLoginPending.current = null
    hudDestination.current = null
    if (pending == null) return
    if (outcome instanceof Error) pending.reject(outcome)
    else pending.resolve(outcome)
  }, [])
  // Native fresh sign-in (driver.loginNew) in flight: non-null shows the verification-code panel;
  // `code` fills in when the engine's 'loginCode' message lands. The attempt counter invalidates
  // a cancelled attempt's eventual resolution (the promise settles after loginCancel).
  const [auth, setAuth] = useState<{ code: string | null } | null>(null)
  const authAttempt = useRef(0)
  // Stops the post-launch boot-panic poll once the world is reached (so it can't mislabel a benign
  // post-boot panic as a launch failure). The timer id is kept so it's cancelled on unmount.
  const bootPollStop = useRef(false)
  const pollTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  // Native parcel pick: the engine can only teleport an existing player (there's no boot-at-position
  // when the engine is already running), so the pick is held until playerReady — or sent at once if
  // the player already spawned (see the no-launch pick path).
  const pendingParcel = useRef<{ x: number; y: number } | null>(null)
  // A realm change the HUD asked for, until the bridge reports how it ended ('travelResult'). Only
  // the latest counts: an earlier one is answered as superseded when a newer one replaces it.
  const travelSeq = useRef(0)
  const [travellingTo, setTravellingTo] = useState<string | null>(null)
  const [travelError, setTravelError] = useState<string | null>(null)
  const [editorMode, setEditorModeState] = useState<EditorHudMode>('off')
  const [playerReady, setPlayerReady] = useState(false)
  // Ref twin of playerReady: the destination pick runs in a callback that would close over a
  // stale value of the state.
  const playerReadyRef = useRef(false)
  const [sceneLoading, setSceneLoading] = useState<SceneLoadingState | null>(null)
  const [hover, setHover] = useState<HoverAction[]>([])
  const [proximity, setProximity] = useState<ProximityTip[]>([])
  const [cursorLocked, setCursorLocked] = useState(false)
  const [messages, setMessages] = useState<ChatLine[]>(initialMessages)
  const [channel, setChannel] = useState('Nearby')
  const channelRef = useRef(channel)
  channelRef.current = channel
  const [conversations, setConversations] = useState<Conversation[]>([])
  // Which tabs are open, for the message handler and for de-duplicating opens within a batch;
  // `conversations` carries what the tabs show.
  const openTabsRef = useRef<Set<string>>(new Set())
  // A DM tab shows the engine's stored history and nothing else: `messages` holds Nearby only.
  // The store is re-read when the tab is shown and whenever a line lands on the shown tab.
  const [dmLines, setDmLines] = useState<Record<string, ChatLine[]>>({})
  // Reactions to Nearby lines, by message id; a DM tab's come with its stored history.
  const [nearbyReactions, setNearbyReactions] = useState<Record<string, MessageReaction[]>>({})
  const nearbyIdsRef = useRef<Set<string>>(new Set())
  // Every community the local user is a member of, as chat tabs (closed ones included: `closed`
  // says which are hidden), most recently active first. The handler reads the refs.
  const [communityConvos, setCommunityConvos] = useState<CommunityConversation[]>([])
  const communityConvosRef = useRef<CommunityConversation[]>([])
  communityConvosRef.current = communityConvos
  const [closedCommunities, setClosedCommunities] = useState<ReadonlySet<string>>(() => new Set())
  const closedCommunitiesRef = useRef<ReadonlySet<string>>(closedCommunities)
  closedCommunitiesRef.current = closedCommunities
  const [members, setMembers] = useState<NearbyMember[]>([])
  const [speaking, setSpeaking] = useState<ReadonlySet<string>>(() => new Set())
  const [nearbyMuted, setNearbyMuted] = useState<ReadonlySet<string>>(() => new Set())
  // `nearbyMuted`, readable before the next render
  const nearbyMutedRef = useRef<ReadonlySet<string>>(nearbyMuted)
  const applyNearbyMuted = useCallback((next: ReadonlySet<string>) => {
    nearbyMutedRef.current = next
    setNearbyMuted(next)
  }, [])
  // Mirror cursor-lock into a ref so the run-once message handler reads it without a stale closure —
  // avatarClick uses it to centre the card while the camera has the pointer locked.
  const cursorLockedRef = useRef(false)
  const [chatOpen, setChatOpen] = useState(true)
  const [chatUnread, setChatUnread] = useState(0)
  const [nearbyUnread, setNearbyUnread] = useState(0)
  const [chatFocusTick, setChatFocusTick] = useState(0)
  // Read inside the message-subscription closure (mounted once), not via a stale `chatOpen` capture.
  const chatOpenRef = useRef(chatOpen)
  chatOpenRef.current = chatOpen
  const chatActiveRef = useRef(false)
  // True while something is covering the chat (see the assignment below for what counts). Read by
  // requestFocusChat from a callback that would otherwise close over a stale value. Popups aren't in
  // here — they live in their own module store, so requestFocusChat asks it directly.
  const chatCoveredRef = useRef(false)
  // Open + (re)focus chat — the engine's "Chat" system action (Enter, relayed by the bridge as
  // focusChat) funnels here, wherever keyboard focus sat when it fired.
  const requestFocusChat = useCallback(() => {
    // Enter is only a chat key when nothing is covering the chat. While the main menu, a popup or a
    // modal is up it owns the screen, so Enter neither dismisses it nor focuses the chat behind it.
    // hasOpenPopup() is read here, not during render: the popup stack is a module store that changes
    // without re-rendering this hook.
    if (chatCoveredRef.current || hasOpenPopup()) return
    // Release the browser pointer lock so the mouse stops driving the camera and you can type. On web
    // camera-look IS the pointer lock; exiting it here (the always-firing focus path) reliably releases
    // it, and the engine self-heals — update_pointer_lock drops camera-look on
    // `!document.pointerLockElement`. No-op on native (no DOM pointer lock; the engine frees its own OS
    // cursor grab from the Chat action).
    document.exitPointerLock?.()
    // Friends is the only panel Enter closes: it shares the chat's bottom-left dock, so Chat renders
    // null behind it (App's `hidden`) and focusing without closing it would do nothing on screen.
    // The rest (profile, notifications, emotes) sit clear of the chat, so leave them open.
    setFriendsOpen(false)
    setChatOpen(true)
    setChatFocusTick((t) => t + 1)
  }, [])
  const [pendingMention, setPendingMention] = useState<string | null>(null)
  const [menuOpen, setMenuOpen] = useState(false)
  const [friendsData, setFriendsData] = useState<{
    available: boolean
    friends: Friend[]
    received: FriendRequest[]
    sent: FriendRequest[]
    blocked: string[]
    blockedUsers: BlockedUser[]
    blockedBy: string[]
    loading: boolean
  }>({ available: false, friends: [], received: [], sent: [], blocked: [], blockedUsers: [], blockedBy: [], loading: false })
  const [friendsOpen, setFriendsOpen] = useState(false)
  // In-flight friend actions as `op:address`, so their buttons disable until the service answers.
  const [friendPending, setFriendPending] = useState<ReadonlySet<string>>(new Set())
  const [mutualFriends, setMutualFriends] = useState<Record<string, MutualFriend[]>>({})
  const [settings, setSettings] = useState<Setting[]>([])
  const [settingsOpen, setSettingsOpen] = useState(false)
  // The binding table lives in the bindingLabels external store (leaf components — menu bar,
  // hover keycaps — read it without the session); this hook is its single writer and re-reads
  // it here to expose session.bindings.list.
  const { bindings } = useBindingsSnapshot()
  // Engine hotkey edges arrive on the run-once driver subscription; dispatch through a ref
  // assigned every render so the handler sees current toggles/state without stale closures
  // (same idiom as chatOpenRef).
  const systemActionRef = useRef<(action: string, pressed: boolean) => void>(() => {})
  // The active press-a-key capture. Only the newest matters: the engine has no capture
  // cancel (a stale request resolves on the NEXT input), so anything but the active id is
  // dropped when an inputCaptured message lands.
  const captureRef = useRef<{ id: string; resolve: (input: string) => void } | null>(null)
  const captureSeq = useRef(0)
  const [profile, setProfile] = useState<Profile | null>(null)
  // The handler's view of the local user, for @-mention detection on lines that land.
  const meRef = useRef<Profile | null>(null)
  meRef.current = profile
  const [profileOpen, setProfileOpen] = useState(false)
  // Own-profile edit: the claimed names the picker offers, and the state of the save in flight.
  const [ownedNames, setOwnedNames] = useState<string[]>([])
  const [profileSaving, setProfileSaving] = useState(false)
  const [profileSaveError, setProfileSaveError] = useState<string | null>(null)
  // What the profile looked like before the in-flight save, to put back if the engine rejects it.
  // `quiet` marks a save the player didn't press SAVE for (the name colour, saved as the passport
  // closes): its failure reverts without an error, as unity-explorer drops a failed colour deploy.
  const profileRevertRef = useRef<{ address: string; profile: Profile | null; stored: Profile | undefined; quiet: boolean } | null>(null)
  // One profile save at a time: the engine cancels an in-flight deploy when another arrives, which
  // would report the first as failed although the second carries it. A save asked for meanwhile
  // waits here, merged with any other waiting one, until the engine answers.
  const profileInFlightRef = useRef(false)
  const queuedProfileEditRef = useRef<{ edit: ProfileEdit; quiet: boolean } | null>(null)
  const startProfileSaveRef = useRef<(edit: ProfileEdit, quiet: boolean) => void>(() => undefined)
  // The latest profile and claimed names, for a save started from the message handler.
  const profileRef = useRef(profile)
  profileRef.current = profile
  const ownedNamesRef = useRef(ownedNames)
  ownedNamesRef.current = ownedNames
  const [notifications, setNotifications] = useState<AppNotification[]>([])
  // The list as of the last render, so a poll result can tell new ids from known ones without a
  // side effect inside a state updater.
  const notificationsRef = useRef(notifications)
  notificationsRef.current = notifications
  const [notificationsOpen, setNotificationsOpen] = useState(false)
  const [skyboxOpen, setSkyboxOpen] = useState(false)
  const [skyboxHours, setSkyboxHoursState] = useState(SKYBOX_START_HOURS)
  const [skyboxProgressing, setSkyboxProgressingState] = useState(true)
  const [emotes, setEmotes] = useState<Emote[]>([])
  const [emotesOpen, setEmotesOpen] = useState(false)
  const [mic, setMic] = useState({ enabled: false, available: false })
  const [voiceChatState, setVoiceChatState] = useState<VoiceChatState | null>(null)
  const [activeVoiceChats, setActiveVoiceChats] = useState<ActiveCommunityVoiceChat[]>([])
  const [activeVoiceChatsLoaded, setActiveVoiceChatsLoaded] = useState(false)
  const [scenePlace, setScenePlace] = useState<{ parcel: string; realm: string } | null>(null)
  const [catalogItems, setCatalogItems] = useState<Wearable[]>([])
  const [catalogTotal, setCatalogTotal] = useState(0)
  const [catalogLoading, setCatalogLoading] = useState(false)
  const catalogReqId = useRef(0)
  const [equippedWearables, setEquippedWearables] = useState<Wearable[]>([])
  const [bodyShape, setBodyShape] = useState<string | undefined>(undefined)
  const [forceRender, setForceRenderState] = useState<string[]>([])
  const [saveError, setSaveError] = useState<string | null>(null)
  const [avatarColors, setAvatarColors] = useState<{ skin?: string; hair?: string; eyes?: string } | undefined>(undefined)
  // Mirror of catalogItems for equipWearables' optimistic equipped-set rebuild (avoids stale closure).
  const catalogItemsRef = useRef<Wearable[]>([])
  useEffect(() => { catalogItemsRef.current = catalogItems }, [catalogItems])
  const [outfits, setOutfits] = useState<OutfitsMetadata>({ outfits: [], namesForExtraSlots: [] })
  const [outfitsLoaded, setOutfitsLoaded] = useState(false)
  const [outfitHistory, setOutfitHistory] = useState<OutfitHistory>(NO_OUTFIT_HISTORY)
  const [backpackOpen, setBackpackOpen] = useState(false)
  const [communities, setCommunities] = useState<Community[]>([])
  const [communityCards, setCommunityCards] = useState<Record<string, Community>>({})
  const [communityBrowse, setCommunityBrowse] = useState<CommunityBrowse>(NO_BROWSE)
  // The grid as last set, and the page read last, for the next page's request.
  const communityBrowseRef = useRef<CommunityBrowse>(NO_BROWSE)
  const communityBrowsePage = useRef(0)
  // The search and page the grid waits for: an answer for any other (an older search) is dropped.
  const communityBrowseWanted = useRef<{ search: string; page: number } | null>(null)
  const updateCommunityBrowse = useCallback((next: (prev: CommunityBrowse) => CommunityBrowse) => {
    communityBrowseRef.current = next(communityBrowseRef.current)
    setCommunityBrowse(communityBrowseRef.current)
  }, [])
  const [communitiesOpen, setCommunitiesOpen] = useState(false)
  // read by the bridge handler, which outlives each render
  const communitiesOpenRef = useRef(false)
  communitiesOpenRef.current = communitiesOpen
  const [communityDetail, setCommunityDetail] = useState<CommunityDetailMessage | null>(null)
  const [communityError, setCommunityError] = useState<{ id: string; message: string } | null>(null)
  const [communitySave, setCommunitySave] = useState<CommunitySave | null>(null)
  const [deletedCommunity, setDeletedCommunity] = useState<string | null>(null)
  const [communityPlaceOptions, setCommunityPlaceOptions] = useState<CommunityPlaceOptionsMessage | null>(null)
  // Whose places the open form asked for ('' on create), or null with no form open: a late answer
  // for another form is dropped.
  const placeOptionsWanted = useRef<string | null>(null)
  // The community whose card asked for its detail last: details for any other are dropped.
  const detailWanted = useRef<string | null>(null)
  const [communityInvites, setCommunityInvites] = useState<CommunityInvitesMessage | null>(null)
  const communityInvitesRef = useRef<CommunityInvitesMessage | null>(null)
  communityInvitesRef.current = communityInvites
  const [pendingCount, setPendingCount] = useState<number | null>(null)
  const [memberSections, setMemberSections] = useState<Partial<Record<CommunityMemberSection, CommunityMemberSectionMessage>>>({})
  const [invitable, setInvitable] = useState<InvitableCommunitiesMessage | null>(null)
  const invitableWanted = useRef<string | null>(null)
  // the requests being answered, each released when the bridge says it settled
  const answering = useRef(new Set<string>())
  // the community whose card is open, if any
  const openCard = useRef<string | null>(null)
  // A create or update is waiting for its answer. Cleared when the form closes, so an answer that
  // comes after that is dropped instead of driving the next form.
  const communitySaveInFlight = useRef(false)
  // Pictures the user saved by editing, by community id. The picture's URL does not change on
  // update, so the browser and the CDN keep showing the old one; as unity-explorer replaces its
  // image cache, the HUD shows the saved picture for the rest of the session.
  const savedCommunityPictures = useRef<Record<string, string>>({})
  const pendingCommunityPicture = useRef<{ id: string; picture: string } | null>(null)
  const withSavedPicture = useCallback(<T extends { id: string; thumbnail?: string }>(c: T): T => {
    const picture = savedCommunityPictures.current[c.id]
    return picture != null ? { ...c, thumbnail: picture } : c
  }, [])
  // a saved picture shows at once wherever the community is already on screen
  const showSavedPicture = useCallback((id: string, picture: string) => {
    savedCommunityPictures.current = { ...savedCommunityPictures.current, [id]: picture }
    const swap = <T extends { id: string; thumbnail?: string }>(c: T): T => (c.id === id ? { ...c, thumbnail: picture } : c)
    setCommunities((prev) => prev.map(swap))
    setCommunityCards((prev) => (prev[id] != null ? { ...prev, [id]: swap(prev[id]) } : prev))
    updateCommunityBrowse((prev) => ({ ...prev, items: prev.items.map(swap) }))
    setCommunityConvos((prev) => prev.map(swap))
  }, [updateCommunityBrowse])
  const [mapParcel, setMapParcel] = useState({ x: 0, y: 0 })
  const [mapOpen, setMapOpen] = useState(false)
  // Minimap pose: a ref, not state — see MinimapState.pose for why.
  const poseRef = useRef<PlayerPose>({ x: 0, z: 0, yaw: 0, camYaw: 0 })
  const [satelliteView, setSatelliteView] = useState<SatelliteView | null>(null)
  const [sceneTitle, setSceneTitle] = useState('')
  // Kept as its key, so a scene report for the same place leaves the minimap's state as it was.
  const [donatePlaceKey, setDonatePlaceKey] = useState<string | null>(null)
  /** The scene (realm and hash) Donate's place was last taken from. */
  const donateSceneRef = useRef<string | null>(null)
  const [placesOpen, setPlacesOpen] = useState(false)
  // The lobby reopened in-world: over the running world, under the panels it opens.
  const [lobbyOpen, setLobbyOpen] = useState(false)
  const [home, setHome] = useState<{ realm: string | null; parcel: string } | null>(null)
  const [eventsOpen, setEventsOpen] = useState(false)
  const eventsOpenSource = useRef<EventsOpenSource | null>(null)
  const eventsData = useEventsData(driverRef, (event) => track('event_set_as_interested', eventAnalytics(event)))
  const [shopOpen, setShopOpen] = useState(false)
  const bridgeRequestRef = useRef(0)
  const bridgePendingRef = useRef(new Map<number, (msg: BridgeReply) => void>())
  const [galleryMetas, setGalleryMetas] = useState<Record<string, GalleryPhotoMeta | null>>({})
  const [galleryOpen, setGalleryOpen] = useState(false)
  const [photoMode, setPhotoMode] = useState<PhotoModeSnapshot | null>(null)
  const [photoCapture, setPhotoCapture] = useState<{ seq: number; preview: string } | null>(null)
  const photoModeActiveRef = useRef(false)
  const photoModeSeqRef = useRef(0)
  const [createOpen, setCreateOpen] = useState(false)
  const [permissionQueue, setPermissionQueue] = useState<PermissionRequestMessage[]>([])
  const [visitAnswers, setVisitAnswers] = useState<VisitAnswer[]>([])
  // the engine's latest outcome per permission the scene used this visit (scene hash + type)
  const [triggeredPermissions, setTriggeredPermissions] = useState<(TriggeredPermission & { scene: string })[]>([])
  // types the scene was refused without the player clicking Deny — by a visit deny or a stored rule
  // — and those the player has seen. The engine reports the refusal of the request the player did
  // click Deny (or Escape) on too: that one report is skipped.
  const [refusedTypes, setRefusedTypes] = useState<string[]>([])
  const [seenDenied, setSeenDenied] = useState<string[]>([])
  const skipRefusal = useRef(new Set<string>())
  const [permissionRules, setPermissionRules] = useState<PermissionRules | null>(null)
  // hash of the scene the player stands in (sceneInfo), for the rules lookup
  const currentScene = useRef<string | undefined>(undefined)
  const [sceneHash, setSceneHash] = useState<string | undefined>(undefined)
  // The HUD's own answers this visit, by scene hash + type, until the player leaves the scene or
  // reloads it: a boolean answers the scene's later requests too ('visit'); null does not (a
  // dismissed prompt asks again; an "Always …" answer is the engine's stored rule from here on).
  const answeredPermissions = useRef(new Map<string, boolean | null>())
  const [worldPassword, setWorldPassword] = useState<WorldPasswordRequestMessage | null>(null)
  const [portables, setPortables] = useState<PortableInfo[]>([])
  // Each portable's last reported state, for the change toasts (null: no list yet). Toasts start
  // once the player is in world from a known baseline: the list the player arrived with, or the
  // first list after arriving, so logging in wearing several smart wearables does not burst toasts.
  const portableStates = useRef<Map<string, PortableState> | null>(null)
  const portableToasts = useRef(false)
  // Stop/Restart lines the HUD ran itself: their console reply is not echoed into the chat (the
  // list's toast says what happened); a typed /stop_portable or /smart_wearable command still is.
  const ownConsoleCommands = useRef<string[]>([])
  const chatId = useRef(0)
  // when each conversation last pinged, per kind of ping
  const pingAtRef = useRef<Map<string, number>>(new Map())
  // the lines appended to a shown tab since its history was last read, by conversation: a read
  // queued before such a line was stored answers without it
  const appendedRef = useRef<Map<string, Set<string>>>(new Map())
  // the next id for a line of each DM or community tab: ids are the HUD's own, never a row's
  // place in the store (the engine trims a community's store, which moves every row)
  const dmNextIdRef = useRef<Map<string, number>>(new Map())
  const nextDmId = (key: string): number => {
    const id = dmNextIdRef.current.get(key) ?? 0
    dmNextIdRef.current.set(key, id + 1)
    return id
  }
  const chatAccountRef = useRef<string | null>(null)
  // The speed to restore when Time progression is turned back on (the last running speed seen).
  const skyboxDaySpeed = useRef(SKYBOX_DAY_SPEED)
  // Catalog fetches done once per session (cache; relays re-emit on change).
  const fetchedRef = useRef<Set<string>>(new Set())

  useEffect(() => {
    const driver = createDriver()
    driverRef.current = driver
    // The profile store asks the engine itself for any address it's shown and doesn't hold.
    setProfileRequester((address, extras) => driver.send({ kind: 'getUserProfile', address, extras }))

    // The engine's answer to the profile save in flight, or the bridge failing to handle it (then no
    // answer ever comes, so it counts as a failure rather than leaving every later save queued).
    const settleProfileSave = (ok: boolean, error?: string): void => {
      profileInFlightRef.current = false
      // A failed save must not leave the optimistic edit on screen claiming to be saved, so the
      // pre-save profile goes back — see saveProfile. When the failed edit is the one the engine
      // shows, it puts the profile from before it back too and announces it; the store re-reads
      // that, and drops a read of the edit still on its way instead of showing the edit again.
      const revert = profileRevertRef.current
      profileRevertRef.current = null
      if (!ok) {
        if (revert?.quiet !== true) setProfileSaveError(error ?? 'Could not save your profile.')
        if (revert != null) {
          // Also the base a queued save starts from below, before any render catches up.
          profileRef.current = revert.profile
          setProfile(revert.profile)
          updateProfile(revert.address, () => revert.stored)
        }
      }
      const queued = queuedProfileEditRef.current
      queuedProfileEditRef.current = null
      // A quiet save left waiting runs on its own; the form's SAVE is done once its deploy is.
      if (queued == null || queued.quiet) setProfileSaving(false)
      if (queued != null) startProfileSaveRef.current(queued.edit, queued.quiet)
    }

    // A community the user is no longer in (left, removed, or the community deleted): its tab
    // goes, and with it its stored history (the delete option went with the tab), its ping and
    // translate settings and its closed entry.
    const forgetCommunity = (id: string): void => {
      const key = communityChannel(id)
      driver.send({ kind: 'dmDelete', address: key })
      clearPingMode(key)
      clearAutoTranslate(key)
      setCommunityConvos((prev) => prev.filter((c) => c.id !== id))
      const account = chatAccountRef.current
      if (account != null) writeAccountList(PREF.chatKnownCommunities, account, readAccountList(PREF.chatKnownCommunities, account).filter((k) => k !== id))
      setClosedCommunities((prev) => {
        if (!prev.has(id)) return prev
        const next = new Set(prev)
        next.delete(id)
        if (chatAccountRef.current != null) writeAccountList(PREF.chatClosedCommunities, chatAccountRef.current, [...next])
        return next
      })
      setDmLines((prev) => {
        if (!(key in prev)) return prev
        const next = { ...prev }
        delete next[key]
        return next
      })
      if (channelRef.current === key) setChannel('Nearby')
    }
    // One generic subscription; switch on kind (mirrors dcl-editor's onSceneMessage).
    const off = driver.on((msg) => {
      switch (msg.kind) {
        case 'event':
          if (msg.name === 'playerReady') {
            playerReadyRef.current = true
            setPlayerReady(true)
            portableToasts.current = portableStates.current != null
            bootPollStop.current = true // world reached → stop watching for a boot panic
            // The bridge pushes bindings at registration, but that can race the page's channel
            // subscription — re-request now that both ends are up (shortcut hints need them).
            driver.send({ kind: 'getBindings' })
            if (pendingParcel.current != null) {
              driver.send({ kind: 'teleport', ...pendingParcel.current })
              pendingParcel.current = null
            }
          }
          break
        case 'analytics':
          // the engine's "First Move In World" (its Segment event, a metre walked since logging in):
          // the shell's Google Ads conversion (shell/googleTag.ts), the same moment, so the two
          // counts agree. Only with the terms accepted, like every event the engine sends; a move
          // before that is not the one reported, a later login's first move can be. The session id
          // goes to Google with it, so the GA4 export joins the Segment session.
          if (msg.event === 'First Move In World' && !inWorldReported.current) {
            const terms = termsOk.current || driver.termsAccepted?.() === true
            if (inShell && terms) {
              inWorldReported.current = true
              postToShell({ type: 'bevy-shell:in-world', login_type: lastLoginType.current, session_id: sessionId() })
            }
          }
          break
        case 'loginCode':
          // Only meaningful while a fresh sign-in is in flight; a stray late code must not
          // resurrect the panel.
          setAuth((a) => (a == null ? a : { code: msg.code }))
          break
        case 'sceneLoading':
          setSceneLoading(msg.state)
          break
        case 'hover': {
          const next = msg.actions
          setHover((prev) => (hoverKey(prev) === hoverKey(next) ? prev : next))
          break
        }
        case 'cursorLock':
          cursorLockedRef.current = msg.locked
          setCursorLocked(msg.locked)
          break
        case 'proximity': {
          const next = msg.tips
          setProximity((prev) => (proximityKey(prev) === proximityKey(next) ? prev : next))
          break
        }
        case 'photoMode': {
          const state = msg.state
          // photo mode owns the screen: every page and panel closes as it opens
          if (state.active && !photoModeActiveRef.current) panelSetters.forEach((set) => set(false))
          photoModeActiveRef.current = state.active
          if (state.event === 'uploadFailed' && state.eventSeq !== photoModeSeqRef.current) {
            showToast('There was an error uploading your screenshot.', { tone: 'error' })
          }
          if (state.event === 'captured' && state.eventSeq !== photoModeSeqRef.current) {
            setPhotoCapture({ seq: state.eventSeq, preview: state.previewJpegBase64 })
          }
          if (!state.active) setPhotoCapture(null)
          if (state.event !== 'none') photoModeSeqRef.current = state.eventSeq
          setPhotoMode(state)
          break
        }
        case 'avatarClick': {
          // The card's scrim swallows mouse input, so the engine's raycast freezes and never sends
          // the hover-exit — clear the hover here or its tooltip stays painted beside the card.
          setHover([])
          // Open the profile card as a popup, anchored at the live DOM cursor (centre while the camera
          // has the pointer locked). The card resolves the name/avatar by address from the roster.
          const p = cursorLockedRef.current ? { x: window.innerWidth / 2, y: window.innerHeight / 2 } : getCursor()
          openProfileCard(msg.address, p.x, p.y)
          break
        }
        case 'chat': {
          const me = meRef.current
          const own = me != null && msg.chat.sender.toLowerCase() === me.address.toLowerCase()
          const system = msg.chat.sender === '' || msg.chat.sender.toLowerCase() === 'system'
          const mentioned = !own && !system && me != null && mentionsName(msg.chat.message, mentionName(me.name, me.address, me.hasClaimedName))
          const ping = (channel: string): void => {
            if (own || system || !shouldPing(channel, mentioned)) return
            const slot = `${channel}|${mentioned ? 'mention' : 'message'}`
            const now = Date.now()
            if (now - (pingAtRef.current.get(slot) ?? -Infinity) < PING_INTERVAL_MS) return
            pingAtRef.current.set(slot, now)
            driver.send({ kind: 'playUiSound', sound: mentioned ? 'chatMention' : 'chatMessage' })
          }
          const communityId = communityIdOf(msg.chat.channel)
          if (communityId != null) {
            // Only a community whose chat is open is listened to; the engine drops the rest, and so
            // does this (a line can land between a close and the engine learning of it).
            const key = communityChannel(communityId)
            if (!communityConvosRef.current.some((c) => c.id === communityId) || closedCommunitiesRef.current.has(communityId)) break
            const shown = chatOpenRef.current && channelRef.current === key
            if (!shown && !(chatOpenRef.current && chatActiveRef.current)) setChatUnread((n) => n + 1)
            // the shown tab appends the line (the store has it too; a later re-read matches it by
            // messageId and keeps its id) rather than reading the whole store back for every line a
            // busy community sends; any other tab counts it; either way the community moves to the
            // top, as every non-Nearby line does
            setCommunityConvos((prev) => {
              const i = prev.findIndex((c) => c.id === communityId)
              if (i < 0) return prev
              const c = shown ? prev[i] : { ...prev[i], unread: prev[i].unread + 1, mention: prev[i].mention || mentioned }
              return [c, ...prev.slice(0, i), ...prev.slice(i + 1)]
            })
            if (shown) {
              if (msg.chat.messageId !== '') {
                const appended = appendedRef.current.get(key) ?? new Set<string>()
                appended.add(msg.chat.messageId)
                appendedRef.current.set(key, appended)
              }
              const line: ChatLine = { sender: msg.chat.sender, message: msg.chat.message, channel: key, messageId: msg.chat.messageId, id: nextDmId(key), ts: Date.now(), reactions: [] }
              setDmLines((prev) => ({ ...prev, [key]: [...(prev[key] ?? []), line].slice(-MAX_CHAT_LINES) }))
            }
            ping(key)
            break
          }
          const key = isDmChannel(msg.chat.channel) ? channelKey(msg.chat.channel) : 'Nearby'
          const shown = chatOpenRef.current && channelRef.current === key
          if (!shown && !(chatOpenRef.current && chatActiveRef.current)) setChatUnread((n) => n + 1)
          if (key !== 'Nearby' || msg.chat.channel === 'Nearby') ping(key)
          if (key === 'Nearby') {
            if (!shown) setNearbyUnread((n) => n + 1)
            // known at once, so a reaction relayed right behind its line isn't dropped
            if (msg.chat.messageId !== '') nearbyIdsRef.current.add(msg.chat.messageId)
            setMessages((prev) =>
              [...prev, { ...msg.chat, id: chatId.current++, ts: Date.now() }].slice(
                -MAX_CHAT_LINES
              )
            )
            break
          }
          // A DM (theirs, or the echo of ours) is already in the store by the time it is relayed:
          // the shown tab re-reads it, any other tab counts it.
          ensureConversation(key)
          if (shown) {
            driverRef.current?.send({ kind: 'dmHistory', address: key })
          } else {
            setConversations((prev) => prev.map((c) => (c.address === key ? { ...c, unread: c.unread + 1 } : c)))
          }
          break
        }
        case 'dmHistory': {
          const key = channelKey(msg.address)
          // a late answer for a tab closed meanwhile
          const communityId = communityIdOf(key)
          const communityOpen = communityId != null && communityConvosRef.current.some((c) => c.id === communityId) && !closedCommunitiesRef.current.has(communityId)
          if (!openTabsRef.current.has(key) && !communityOpen) break
          // A line keeps the id it was shown with, so a re-read after the engine trimmed the store
          // leaves keys, the NEW separator and the open reaction bar on the same lines.
          const entries = msg.entries.slice(Math.max(0, msg.entries.length - MAX_CHAT_LINES))
          // a line appended while this read was pending, and stored after the read was queued, is
          // not in the answer: it stays behind the stored rows until a later read returns it
          const appended = appendedRef.current.get(key)
          setDmLines((prev) => {
            const shown = prev[key] ?? []
            const known = new Map(rowKeys(shown.map((l) => l.messageId)).map((k, i) => [k, shown[i].id]))
            const keys = rowKeys(entries.map((e) => e.messageId))
            const lines: ChatLine[] = entries.map((e, i) => ({
              sender: e.from,
              message: e.message,
              channel: key,
              messageId: e.messageId,
              id: known.get(keys[i]) ?? nextDmId(key),
              ts: e.receivedAt,
              reactions: e.reactions
            }))
            const nextId = (): number => nextDmId(key)
            if (appended == null || appended.size === 0) return { ...prev, [key]: risingIds(lines, nextId) }
            const stored = new Set(lines.map((l) => l.messageId))
            const pending = shown.filter((l) => appended.has(l.messageId) && !stored.has(l.messageId))
            appendedRef.current.set(key, new Set(pending.map((l) => l.messageId)))
            return { ...prev, [key]: risingIds([...lines, ...pending], nextId).slice(-MAX_CHAT_LINES) }
          })
          break
        }
        case 'chatReaction': {
          const r = msg.reaction
          if (r.channel === 'Nearby') {
            // reactions to lines we don't have (from before we arrived, or scrolled away) are dropped
            if (!nearbyIdsRef.current.has(r.messageId)) break
            setNearbyReactions((prev) => {
              const next = applyReaction(prev[r.messageId] ?? [], r)
              return next === prev[r.messageId] ? prev : { ...prev, [r.messageId]: next }
            })
            break
          }
          // a DM's or a community's reaction is stored with its line; a tab that has the line
          // applies it there (a later read folds the store the same way), nothing is read back
          const key = channelKey(r.channel)
          setDmLines((prev) => {
            const lines = prev[key]
            const i = lines?.findIndex((l) => l.messageId === r.messageId) ?? -1
            if (lines == null || i < 0) return prev
            const reactions = applyReaction(lines[i].reactions ?? [], r)
            if (reactions === lines[i].reactions) return prev
            return { ...prev, [key]: lines.map((l, j) => (j === i ? { ...l, reactions } : l)) }
          })
          break
        }
        case 'dmUserState': {
          const key = channelKey(msg.address)
          const communityId = communityIdOf(key)
          if (communityId != null) {
            setCommunityConvos((prev) => prev.map((c) => (c.id === communityId ? { ...c, state: msg.state } : c)))
            break
          }
          setConversations((prev) => prev.map((c) => (c.address === key ? { ...c, state: msg.state, online: msg.online } : c)))
          break
        }
        case 'focusChat':
          requestFocusChat()
          break
        case 'chatVisibility':
          setChatOpen(msg.open)
          break
        case 'members':
          setMembers(msg.members)
          seedProfiles(msg.members)
          break
        case 'voiceActivity': {
          const address = msg.address.toLowerCase()
          setSpeaking((prev) => {
            if (prev.has(address) === msg.active) return prev
            const next = new Set(prev)
            if (msg.active) next.add(address)
            else next.delete(address)
            return next
          })
          break
        }
        case 'voiceChatState':
          setVoiceChatState(msg.state)
          break
        case 'communityVoiceUpdate':
          // the live list follows (the scene re-reads it); the broadcast itself feeds the toasts
          emitVoiceChatEvent(msg)
          break
        case 'activeCommunityVoiceChats':
          setActiveVoiceChats(msg.chats)
          setActiveVoiceChatsLoaded(msg.loaded !== false)
          break
        case 'menuVisibility':
          setMenuOpen(msg.open)
          break
        case 'friendActionDone':
        case 'friendActionFailed': {
          const ok = msg.kind === 'friendActionDone'
          setFriendPending((prev) => {
            const next = new Set(prev)
            next.delete(`${msg.op}:${msg.address.toLowerCase()}`)
            return next
          })
          emitFriendEvent({ op: msg.op, address: msg.address, ok })
          toastFriendResult(msg.op, msg.address, ok)
          break
        }
        case 'mutualFriends':
          setMutualFriends((prev) => ({ ...prev, [msg.address.toLowerCase()]: msg.friends }))
          break
        case 'friendOnline':
          toastFriendOnline(msg.address)
          break
        case 'friends':
          setFriendsData({
            available: msg.available,
            friends: msg.friends,
            received: msg.received,
            sent: msg.sent,
            blocked: msg.blocked,
            blockedUsers: msg.blockedUsers ?? [],
            blockedBy: msg.blockedBy ?? [],
            loading: msg.loading === true
          })
          seedProfiles([...msg.friends, ...msg.received, ...msg.sent])
          break
        case 'settings':
          setSettings(msg.settings)
          applyLanguageSetting(msg.settings)
          break
        case 'bindings':
          setBindingsSnapshot(msg.bindings)
          break
        case 'inputCaptured':
          if (captureRef.current?.id === msg.id) {
            const { resolve } = captureRef.current
            captureRef.current = null
            resolve(msg.input)
          }
          break
        case 'systemAction':
          systemActionRef.current(msg.action, msg.pressed)
          break
        case 'bridgeUnavailable':
          // Every panel behind the bridge would stay empty with nothing to explain why. Not a crash:
          // the engine and the world itself are fine, so it opens as an ordinary dialog (issue #1233).
          // With the HUD hidden (?hud=0, or a substituted ui scene) there are no panels to explain.
          if (bootMode().hideHud) break
          setFatalError((prev) => prev ?? {
            message: 'The HUD could not reach the explorer bridge, so panels may stay empty. Restarting usually fixes it.',
            source: 'bridge'
          })
          break
        case 'profile':
          setProfile(msg.profile)
          setOwnProfile(msg.profile?.address ?? null)
          // The passport reads the store, so an authoritative own profile has to reach it too or a
          // just-saved name would keep its stale claimed-name seal until reopened.
          if (msg.profile != null) receiveProfile(msg.profile.address, msg.profile)
          break
        case 'userProfile':
          receiveProfile(msg.address, msg.profile)
          break
        case 'profileChanged':
          profileChanged(msg.address, msg.version)
          break
        case 'ownedNames':
          setOwnedNames(msg.names)
          break
        case 'profileSaved':
          settleProfileSave(msg.ok, msg.error)
          break
        case 'notifications':
          setNotifications(msg.notifications)
          notificationsRef.current = msg.notifications
          break
        case 'notificationsNew': {
          const known = new Set(notificationsRef.current.map((n) => n.id))
          const at = (n: AppNotification): number => notificationTimeMs(n.timestamp) ?? 0
          const fresh = msg.notifications.filter((n) => !known.has(n.id)).sort((a, b) => at(b) - at(a))
          if (fresh.length === 0) break
          // Kept to the size of the full fetch: older ones come back when the panel refetches.
          notificationsRef.current = [...fresh, ...notificationsRef.current].slice(0, NOTIFICATIONS_KEPT)
          setNotifications((prev) => {
            const have = new Set(prev.map((n) => n.id))
            return [...fresh.filter((n) => !have.has(n.id)), ...prev].slice(0, NOTIFICATIONS_KEPT)
          })
          // an invite or a request arriving while the Communities page is open reads its Invites &
          // Requests section again
          // (the whole section once it has been read, else its counter alone)
          if (communitiesOpenRef.current && fresh.some((n) => COMMUNITY_REQUEST_NOTIFICATIONS.has(n.type))) {
            driverRef.current?.send({ kind: communityInvitesRef.current != null ? 'getCommunityInvites' : 'getCommunityInvitesCount' })
          }
          // a request of the user's accepted: they are a member now. My Communities, the grid and
          // that community's open card are read again
          const joined = fresh.filter((n) => n.type === 'community_request_to_join_accepted').map((n) => n.metadata?.communityId).filter((id): id is string => typeof id === 'string')
          if (joined.length > 0) {
            if (communitiesOpenRef.current) {
              driverRef.current?.send({ kind: 'getCommunities' })
              if (communityBrowsePage.current > 0) loadCommunityBrowse(communityBrowseRef.current.search)
            }
            const open = detailWanted.current
            if (open != null && joined.includes(open)) {
              driverRef.current?.send({ kind: 'getCommunity', id: open })
              driverRef.current?.send({ kind: 'getCommunityDetail', id: open })
            }
          }
          // Newest first in the list; toasts and reward popups play in arrival order.
          const arrived = [...fresh].reverse()
          queueNotificationToasts(arrived)
          // A popup, unlike the toasts, would still render with the HUD hidden (?hud=0).
          if (!bootMode().hideHud) queueRewardPopups(arrived)
          break
        }
        case 'emotes':
          setEmotes(msg.emotes)
          break
        case 'mic':
          setMic({ enabled: msg.enabled, available: msg.available })
          break
        case 'nearbyMutes':
          applyNearbyMuted(new Set(msg.addresses.map((a) => a.toLowerCase())))
          break
        case 'requestFailed':
          fetchedRef.current.delete(msg.request)
          console.error(`[bridge] ${msg.request} failed:`, msg.error)
          if (msg.request === 'saveProfile' && profileInFlightRef.current) settleProfileSave(false)
          break
        case 'avatarSaveFailed':
          setSaveError(SAVE_FAILED_MESSAGE)
          console.error('[backpack] deploy failed:', msg.message)
          break
        case 'wearables':
          setEquippedWearables(msg.equipped)
          if (msg.bodyShape) setBodyShape(msg.bodyShape)
          if (msg.forceRender) setForceRenderState(msg.forceRender)
          if (msg.colors) {
            const c = msg.colors
            setAvatarColors((prev) => ({
              skin: c.skin ? color3ToHex(c.skin) : prev?.skin,
              hair: c.hair ? color3ToHex(c.hair) : prev?.hair,
              eyes: c.eyes ? color3ToHex(c.eyes) : prev?.eyes
            }))
          }
          // The grid page carries its own per-item equipped flags (stamped at fetch, flipped by the
          // single-equip optimistic rebuild) — an authoritative emit (e.g. after equipOutfit) must
          // reconcile them too, or stale page flags shadow the new set in the category slots
          // (equippedByCat prefers page items) and the card markers.
          setCatalogItems((list) =>
            list.map((w) => ({
              ...w,
              equipped: msg.equipped.some((e) => e.urn === w.urn || e.urn.startsWith(`${w.urn}:`))
            }))
          )
          break
        case 'catalogPage':
          // Only the newest request's page wins — drop stale responses (fast page/filter changes).
          if (msg.catalog === 'wearables' && msg.requestId === catalogReqId.current) {
            setCatalogItems(msg.items)
            setCatalogTotal(msg.total)
            setCatalogLoading(false)
          }
          break
        case 'outfits':
          setOutfits(msg.metadata)
          setOutfitsLoaded(true)
          break
        case 'outfitHistory':
          setOutfitHistory({ canUndo: msg.canUndo, canRedo: msg.canRedo, rolling: msg.rolling })
          break
        case 'communities':
          setCommunities(msg.communities.map(withSavedPicture))
          break
        case 'community': {
          // one community for a card, kept apart from the browse list (replaced if already there);
          // after a write it is the community that changed, so its browse card shows the change too
          // (keeping the friends only the browse read carries)
          const fresh = withSavedPicture(msg.community)
          setCommunityCards((prev) => ({ ...prev, [fresh.id]: fresh }))
          // My Communities shows it too, when listed there (a card prefers that entry)
          setCommunities((prev) => (prev.some((c) => c.id === fresh.id) ? prev.map((c) => (c.id === fresh.id ? { ...fresh, friends: fresh.friends ?? c.friends } : c)) : prev))
          if (communityBrowseRef.current.items.some((c) => c.id === fresh.id)) {
            updateCommunityBrowse((prev) => ({
              ...prev,
              items: prev.items.map((c) => (c.id === fresh.id ? { ...fresh, friends: fresh.friends ?? c.friends } : c))
            }))
          }
          break
        }
        case 'communityBrowse': {
          const wanted = communityBrowseWanted.current
          if (wanted == null || wanted.search !== msg.search || wanted.page !== msg.page) break
          communityBrowseWanted.current = null
          if (msg.failed === true) {
            showToast(BROWSE_ERROR, { tone: 'error' })
            updateCommunityBrowse((prev) => ({ ...prev, loading: false, loadingMore: false, failed: msg.page === 1 }))
            break
          }
          communityBrowsePage.current = msg.page
          const page = msg.communities.map(withSavedPicture)
          updateCommunityBrowse((prev) => {
            // a community that moved up the order while paging is listed once
            const items = msg.page === 1 ? page : [...prev.items, ...page.filter((c) => !prev.items.some((p) => p.id === c.id))]
            return { search: msg.search, items, total: msg.total, loading: false, loadingMore: false }
          })
          break
        }
        case 'communityActionFailed':
          if (TOASTED_COMMUNITY_ACTIONS.has(msg.action)) showToast(COMMUNITY_ERROR[msg.action], { tone: 'error' })
          else {
            setCommunityError({ id: msg.id, message: COMMUNITY_ERROR[msg.action] })
            // a join or request from a grid card: no card is open to show it, so it is toasted
            if (msg.id !== openCard.current && (msg.action === 'join' || msg.action === 'requestToJoin' || msg.action === 'cancelJoinRequest')) {
              showToast(COMMUNITY_ERROR[msg.action], { tone: 'error' })
            }
          }
          break
        case 'communityRequestSettled':
          answering.current.delete(msg.requestId)
          break
        case 'communityInvites':
          for (const list of msg.failed) showToast(INVITES_LOAD_ERROR[list], { tone: 'error' })
          // the requesters' names and faces, for their menu and the passport they open
          seedProfiles(memberProfiles(msg.requestsReceived.flatMap((g) => g.requests)))
          setCommunityInvites({ ...msg, invites: msg.invites.map(withSavedPicture), requestsSent: msg.requestsSent.map(withSavedPicture) })
          setPendingCount(msg.invites.length + msg.requestsReceived.reduce((n, g) => n + g.total, 0))
          break
        case 'communityJoinRequest': {
          // every copy of the community shows the request (or its cancel) at once
          const patch = (c: Community): Community => (c.id === msg.id ? { ...c, pendingRequestId: msg.requestId } : c)
          setCommunityCards((prev) => (prev[msg.id] != null ? { ...prev, [msg.id]: patch(prev[msg.id]) } : prev))
          setCommunities((prev) => (prev.some((c) => c.id === msg.id) ? prev.map(patch) : prev))
          if (communityBrowseRef.current.items.some((c) => c.id === msg.id)) updateCommunityBrowse((prev) => ({ ...prev, items: prev.items.map(patch) }))
          break
        }
        case 'communityInvitesCount':
          setPendingCount(msg.count)
          break
        case 'communityMemberSection':
          // only the card open now
          if (msg.id !== detailWanted.current) break
          seedProfiles(memberProfiles(msg.members))
          setMemberSections((prev) => ({ ...prev, [msg.section]: msg }))
          break
        case 'invitableCommunities':
          if (msg.address !== invitableWanted.current) break
          if (msg.failed === true) showToast(INVITABLE_FAILED, { tone: 'error' })
          setInvitable(msg)
          break
        case 'communityInviteSent':
          showToast(msg.ok ? INVITE_SENT : INVITE_FAILED, msg.ok ? {} : { tone: 'error' })
          break
        case 'communityPlaceOptions':
          if (placeOptionsWanted.current !== (msg.communityId ?? '')) break
          setCommunityPlaceOptions(msg)
          break
        case 'communitySaveResult': {
          if (!communitySaveInFlight.current) break
          communitySaveInFlight.current = false
          setCommunitySave({ status: 'done', result: msg })
          const pending = pendingCommunityPicture.current
          pendingCommunityPicture.current = null
          if (msg.mode === 'update' && msg.id != null && pending?.id === msg.id) showSavedPicture(pending.id, pending.picture)
          // a save reloads the grid: a new or changed community may
          // join, leave or move in the grid
          if (msg.id != null && communityBrowsePage.current > 0) loadCommunityBrowse(communityBrowseRef.current.search)
          break
        }
        case 'communityDeleted':
          setDeletedCommunity(msg.id)
          if (communityBrowseRef.current.items.some((c) => c.id === msg.id)) {
            updateCommunityBrowse((prev) => ({ ...prev, items: prev.items.filter((c) => c.id !== msg.id), total: Math.max(0, prev.total - 1) }))
          }
          setCommunityCards((prev) => {
            const { [msg.id]: _gone, ...rest } = prev
            return rest
          })
          break
        case 'communityDetail':
          // only the card open now: a re-read for a card already closed must not replace its detail
          if (msg.id !== detailWanted.current) break
          // the members' names and faces, for the member menu and the passport they open
          seedProfiles(memberProfiles(msg.members))
          setCommunityDetail(msg)
          break
        case 'events':
        case 'eventAttending':
        case 'eventPlaces':
          eventsData.onMessage(msg)
          break
        case 'communityChannels': {
          // the user's communities, in the order they already have (new ones last)
          setCommunityConvos((prev) => {
            const next = msg.communities.map(withSavedPicture).map((c) => {
              const old = prev.find((p) => p.id === c.id)
              return old != null ? { ...old, name: c.name, thumbnail: c.thumbnail, role: c.role } : communityConversation(c)
            })
            const order = new Map(prev.map((c, i) => [c.id, i]))
            return next.sort((a, b) => (order.get(a.id) ?? Infinity) - (order.get(b.id) ?? Infinity))
          })
          // the communities the account had chats for are remembered: one gone from this list was
          // left (or the user removed) while no client of theirs was looking, and the store cannot
          // be listed, so this is where its history and settings go
          if (chatAccountRef.current != null) {
            const account = chatAccountRef.current
            const ids = msg.communities.map((c) => c.id)
            for (const id of readAccountList(PREF.chatKnownCommunities, account)) if (!ids.includes(id)) forgetCommunity(id)
            writeAccountList(PREF.chatKnownCommunities, account, ids)
          }
          // the shown community gone from the list (left from elsewhere): back to Nearby, as when
          // its removal arrives alone, or the Nearby header would front sends to the community
          const shownId = communityIdOf(channelRef.current)
          if (shownId != null && !msg.communities.some((c) => c.id === shownId)) {
            const key = communityChannel(shownId)
            setDmLines((prev) => {
              if (!(key in prev)) return prev
              const next = { ...prev }
              delete next[key]
              return next
            })
            setChannel('Nearby')
          }
          break
        }
        case 'communityChannelAdded': {
          const added = withSavedPicture(msg.community)
          setCommunityConvos((prev) => (prev.some((c) => c.id === added.id) ? prev.map((c) => (c.id === added.id ? { ...c, name: added.name, thumbnail: added.thumbnail, role: added.role } : c)) : [...prev, communityConversation(added)]))
          const account = chatAccountRef.current
          if (account != null) {
            const known = readAccountList(PREF.chatKnownCommunities, account)
            if (!known.includes(msg.community.id)) writeAccountList(PREF.chatKnownCommunities, account, [...known, msg.community.id])
          }
          break
        }
        case 'communityChannelRemoved':
          forgetCommunity(msg.id)
          break
        case 'communityOnlineMembers':
          setCommunityConvos((prev) => prev.map((c) => (c.id === msg.id ? { ...c, members: dedupeMembers(msg.members) } : c)))
          break
        case 'communityPresence':
          // only a list already asked for is kept up to date; any other update changes nothing
          setCommunityConvos((prev) => {
            const i = prev.findIndex((c) => c.id === msg.communityId)
            const c = i < 0 ? undefined : prev[i]
            if (c?.members == null) return prev
            const same = (m: CommunityMember): boolean => m.address.toLowerCase() === msg.member.address.toLowerCase()
            // the update names the member only: the role stays the listed one, if they were listed
            const listed = c.members.find(same)
            const rest = c.members.filter((m) => !same(m))
            const members = msg.online ? [...rest, listed != null ? { ...msg.member, role: listed.role } : msg.member] : rest
            return prev.map((p, j) => (j === i ? { ...c, members } : p))
          })
          break
        case 'mapState':
          setMapParcel({ x: msg.x, y: msg.y })
          break
        case 'playerPose': {
          // Mutate the ref instead of setState: this arrives ~20/s and the minimap reads
          // it from a RAF loop, so it must not drive React renders.
          poseRef.current = { x: msg.x, z: msg.z, yaw: msg.yaw, camYaw: msg.camYaw }
          // The parcel is state, but only changes on a boundary crossing: returning `prev`
          // unchanged makes React skip the render.
          const px = Math.floor(msg.x / PARCEL_METERS)
          const py = Math.floor(msg.z / PARCEL_METERS)
          setMapParcel((prev) => (prev.x === px && prev.y === py ? prev : { x: px, y: py }))
          break
        }
        case 'consoleReply': {
          const own = ownConsoleCommands.current.indexOf([msg.command, ...msg.args].join(' '))
          if (own >= 0) {
            ownConsoleCommands.current.splice(own, 1)
            if (!msg.ok) showToast(formatConsoleReply(msg.command, msg.args, msg.output), { tone: 'error' })
            break
          }
          // Command feedback: a local "DCL System" line, never broadcast (same shape as pushSystemMessage).
          setMessages((prev) =>
            [
              ...prev,
              { sender: '', message: formatConsoleReply(msg.command, msg.args, msg.output), channel: 'Nearby', messageId: '', id: chatId.current++, ts: Date.now() }
            ].slice(-MAX_CHAT_LINES)
          )
          break
        }
        case 'realmInfo':
          setSatelliteView(msg.satelliteView)
          break
        case 'homeScene':
          setHome({ realm: msg.realm === DEFAULT_REALM ? null : msg.realm, parcel: `${msg.parcel.x},${msg.parcel.y}` })
          break
        case 'travelResult':
          if (msg.travelId !== travelSeq.current) break
          setTravellingTo(null)
          if (!msg.ok) {
            setTravelError(`Couldn't travel to "${msg.realm}": ${msg.message ?? 'unknown error'}`)
          }
          if (lobbyRef.current) {
            if (msg.ok) setLobby(false)
            else setDestinationPicked(false)
          }
          break
        case 'lobbyStageReady':
          setLobbyStageReady(true)
          setAvatarLoaded(true)
          break
        case 'lookShown':
          setPendingLook((pending) => (pending === msg.look ? null : pending))
          break
        case 'welcome': {
          // an answer to an ask from before a sign-out is the signed-out account's: no page of a
          // sign-in since, even one already submitted, and none of its pick. One from a bridge
          // that echoes no tag counts while a sign-in is submitted.
          if (msg.ask !== undefined ? msg.ask !== signOuts.current : !submittedRef.current) break
          // ACCEPT AND PLAY's pick accepted the terms already, and a /hud_login's sign-in stands
          // for them too (lib/hudAutomation.ts): the page is answered, with the pick's name. A
          // first login deploys its profile under the pick's name, so without one a /hud_login
          // fails here and the page shows, as it would to a person.
          const hudLogin = hudLoginPending.current
          if (hudLogin != null) {
            hudLogin.welcomeAwaited = false
            setHudLoginTick((n) => n + 1)
            if (msg.newProfile && guestPick.current == null) settleHudLogin(new Error('the account has no profile yet: a first login needs --name and --look (/hud_logout, then sign in again with them)'))
          }
          const chosen: { name?: string } | null = guestPick.current ?? (hudLoginPending.current != null ? {} : null)
          guestPick.current = null
          if (chosen == null || (!msg.terms && !msg.newProfile)) setWelcomePending(false)
          // native, which launches itself, holds a link's world for the terms (src/lib.rs)
          if (msg.terms && driver.launch == null && welcomeLink.current != null) heldDest.current = welcomeLink.current
          if (!msg.terms) termsOk.current = true
          if (!msg.terms && !msg.newProfile) {
            releaseLinkRef.current()
            break
          }
          // a new profile's name shows on the page; the profile is otherwise first pulled on world entry
          if (msg.newProfile && !fetchedRef.current.has('getProfile')) {
            fetchedRef.current.add('getProfile')
            driver.send({ kind: 'getProfile' })
          }
          welcomeRef.current = { terms: msg.terms, newProfile: msg.newProfile }
          if (!msg.terms) termsOk.current = true
          // with the name, so the engine deploys the new profile it holds for the welcome page
          if (chosen != null) {
            setWelcomeSaving(true)
            setWelcomeError(null)
            // the name is a new profile's (the page's own accept passes one only then): an
            // existing account's profile keeps its name and is not deployed again
            driver.send({ kind: 'acceptWelcome', name: msg.newProfile ? chosen.name : undefined })
            break
          }
          screenViewed(msg.newProfile ? 'AVATAR_NAMING' : 'WELCOME')
          setWelcome({ terms: msg.terms, newProfile: msg.newProfile })
          break
        }
        case 'welcomeAccepted': {
          setWelcomeSaving(false)
          setWelcomePending(false)
          if (!msg.ok) {
            setWelcomeError(msg.error ?? 'Saving failed')
            // a /hud_login ends here, on the page that shows the error
            settleHudLogin(new Error(`accepting the welcome page failed: ${msg.error ?? 'Saving failed'}`))
            // the guest start page's accept failed: the welcome page shows, with the error
            if (welcomeRef.current != null) setWelcome((w) => w ?? welcomeRef.current)
            break
          }
          releaseLinkRef.current()
          termsOk.current = true
          setWelcome(null)
          if (welcomeRef.current?.newProfile) screenViewed('AUTH_DEPLOY_SUCCESS')
          welcomeRef.current = null
          break
        }
        case 'sceneInfo':
          setSceneTitle(msg.title)
          setScenePlace(msg.parcel == null ? null : { parcel: `${msg.parcel.x},${msg.parcel.y}`, realm: msg.realm ?? '' })
          {
            // Donate's place changes with the scene, not with each parcel crossed inside it (the bridge reports
            // every parcel), so the host is asked once per scene; a parcel with no scene names none.
            const sceneKey = donateSceneKey(msg)
            if (sceneKey !== donateSceneRef.current) {
              donateSceneRef.current = sceneKey
              const place = sceneKey ? donatePlaceOf(msg) : null
              setDonatePlaceKey(place ? JSON.stringify(place) : null)
            }
          }
          // Leaving a scene forgets its visit answers (a neighbour's are kept until the player leaves
          // that one); the used permissions and the badge are the current scene's only.
          if (msg.scene !== currentScene.current) {
            const left = currentScene.current
            if (left != null) {
              setVisitAnswers((u) => u.filter((e) => e.scene !== left))
              for (const key of answeredPermissions.current.keys()) {
                if (key.startsWith(`${left}|`)) answeredPermissions.current.delete(key)
              }
            }
            currentScene.current = msg.scene
            setSceneHash(msg.scene)
            setTriggeredPermissions([])
            setRefusedTypes([])
            setSeenDenied([])
            setPermissionRules(null)
            skipRefusal.current.clear()
          }
          if (msg.title !== '' && msg.parcel != null && msg.realm && !msg.preview && (msg.genesis || msg.realm.includes('.'))) {
            const key = `${msg.realm}|${msg.title}`
            if (key !== visitedTitle.current) {
              visitedTitle.current = key
              recordVisit({ realm: msg.realm, genesis: msg.genesis === true, parcel: msg.parcel }).catch(() => undefined)
            }
          }
          break
        case 'gallery':
        case 'galleryWrite':
        case 'externalUrlOpened': {
          const resolve = bridgePendingRef.current.get(msg.requestId)
          bridgePendingRef.current.delete(msg.requestId)
          resolve?.(msg)
          break
        }
        case 'galleryPhoto':
          setGalleryMetas((prev) => ({ ...prev, [msg.id]: msg.meta }))
          break
        case 'permissionRequest': {
          const remembered = answeredPermissions.current.get(`${msg.scene}|${msg.ty}`)
          if (typeof remembered === 'boolean') {
            driverRef.current?.send({ kind: 'permissionResolve', id: msg.id, ty: msg.ty, allow: remembered, level: 'visit', scene: msg.scene, realm: msg.realm })
            break
          }
          setPermissionQueue((q) => (q.some((r) => r.id === msg.id) ? q : [...q, msg]))
          break
        }
        case 'permissionWithdrawn':
          setPermissionQueue((q) => q.filter((r) => r.id !== msg.id))
          break
        case 'permissionUsed':
          // a neighbouring scene's or a portable's use is not this scene's
          if (msg.scene !== currentScene.current) break
          setTriggeredPermissions((t) => {
            const same = t.find((e) => e.scene === msg.scene && e.ty === msg.ty)
            if (same?.allowed === msg.allowed) return t
            return [...t.filter((e) => e !== same), { scene: msg.scene, ty: msg.ty, allowed: msg.allowed }]
          })
          if (!msg.allowed && !skipRefusal.current.delete(`${msg.scene}|${msg.ty}`)) {
            setRefusedTypes((r) => (r.includes(msg.ty) ? r : [...r, msg.ty]))
          }
          break
        case 'permissionRules': {
          const { kind: _kind, ...rules } = msg
          if (msg.scene === currentScene.current) setPermissionRules(rules)
          break
        }
        case 'worldPasswordRequest':
          setWorldPassword(msg)
          break
        case 'worldPasswordWithdrawn':
          setWorldPassword(null)
          break
        case 'portables': {
          const before = portableStates.current
          portableStates.current = new Map(msg.portables.map((p) => [portableKey(p), p.state]))
          if (playerReadyRef.current) {
            if (portableToasts.current && before != null) toastPortableChanges(before, msg.portables)
            portableToasts.current = true
          }
          setPortables(msg.portables)
          break
        }
      }
    })

    // Same-domain SSO: an identity in this origin's localStorage (written by the auth site)
    // means the user is already signed in — no engine query, no polling. Returning from the
    // auth-site redirect lands here too, with the identity already present.
    // An unattended run (the host's `unattendedRun`) never looks at what is stored: it signs in with
    // its own identity (the auto-login effect below), so no stored account is read, shown or used.
    if (boot.current.autoLogin !== 'unattended') {
      // Show "Jump in" only when the ENGINE has a usable previous login. Gating on the stored SSO
      // identity alone showed a Jump-in button that couldn't actually log in (the engine reuses its
      // own saved login via loginPrevious; there is no log-in-with-raw-identity surface), so a stale
      // localStorage entry would strand the user on a button that throws. The driver folds both
      // signals together (engine saved login + SSO) into getPreviousLogin().
      const login = getLogin().catch(() => null)
      const signOut = signOuts.current
      void login.then((read) => {
        // a sign-out since cleared what the read would bring back
        if (signOuts.current !== signOut) return
        // a /hud_login that came first holds the stored sign-in: the read is what goes back if
        // the engine refuses that sign-in, never a replacement for it
        const owned = hudStored.current
        if (owned == null) setStored(read)
        else owned.replaced = read
      })
      // an auto-login boot (?guest=1) goes straight in: nothing to pick, the splash stays up
      const screen = (stored: boolean): void => {
        if (boot.current.autoLogin == null) setStatus(stored ? 'reuse-login-or-new' : 'sign-in-or-guest')
      }
      // nor does the engine's answer bring a signed-out account back to the start page
      driver
        .getPreviousLogin()
        .then((r) => {
          if (signOuts.current !== signOut) return
          setPrevUserId(r.userId)
          screen(Boolean(r.userId))
        })
        .catch(() => login.then((l) => signOuts.current === signOut && screen(l != null)))
    }

    return () => {
      off()
      driver.dispose()
      driverRef.current = null
    }
    // createDriver is stable; run-once.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // Watch engine boot: the engine autostarts on mount, so poll until it can take commands, then stop.
  // Drivers without an engine (mock/tests) report ready immediately.
  useEffect(() => {
    const driver = driverRef.current
    if (driver == null || typeof driver.engineReady !== 'function') {
      setEngineReady(true)
      setAvatarLoaded(true)
      setLoadProgress(100)
      return
    }
    if (driver.engineReady()) {
      setEngineReady(true)
      setLoadProgress(100)
      return
    }
    const id = setInterval(() => {
      const d = driverRef.current
      setLoadProgress(d?.loadProgress?.() ?? 0)
      setLoadStep(d?.loadStep?.() ?? null)
      if (d?.engineReady?.() === true) {
        setEngineReady(true)
        setLoadProgress(100)
        setLoadStep(null)
        clearInterval(id)
      }
    }, 200)
    return () => clearInterval(id)
  }, [])

  // Runtime engine crashes (heartbeat stall) are detected by the bundle's watchdog and surfaced to us
  // directly (the engine's own crash overlay is hidden behind react-web's HUD).
  useEffect(() => {
    // Same-document engine (no iframe): the crash watchdog in engine/boot.js calls this directly
    // instead of rendering any overlay — React owns the error UI.
    const w = window as Window & { __onEngineCrash?: (message: string, source: string) => void }
    w.__onEngineCrash = (message) => {
      setFatalError((prev) => prev ?? { message: message || 'The engine stopped responding.', source: 'runtime' })
    }
    return () => {
      delete w.__onEngineCrash
    }
  }, [])

  // Cancel the boot-panic poll on unmount — the one self-scheduling timer in this hook.
  useEffect(() => () => { if (pollTimer.current) clearTimeout(pollTimer.current) }, [])

  // On world-entry, pull the profile (top-bar chip) and notifications (so the unread badge
  // shows immediately, not only after the panel is first opened). Marked fetched once.
  useEffect(() => {
    if (!playerReady) return
    if (!fetchedRef.current.has('getProfile')) {
      fetchedRef.current.add('getProfile')
      driverRef.current?.send({ kind: 'getProfile' })
    }
    if (!fetchedRef.current.has('getNotifications')) {
      fetchedRef.current.add('getNotifications')
      driverRef.current?.send({ kind: 'getNotifications' })
    }
  }, [playerReady])

  // The closed community chats are the account's: read when it is known, written on each change.
  const chatAccount = profile?.address.toLowerCase() ?? null
  chatAccountRef.current = chatAccount
  useEffect(() => {
    setClosedCommunities(new Set(chatAccount == null ? [] : readAccountList(PREF.chatClosedCommunities, chatAccount)))
  }, [chatAccount])
  // The community chats open from the start (the Communities page lists more). Asked
  // for once the account is known, so its closed chats are closed from the first list (the engine
  // never listens to them) and a close made before the profile is not lost.
  useEffect(() => {
    if (!playerReady || chatAccount == null) return
    if (!fetchedRef.current.has('getCommunityChannels')) {
      fetchedRef.current.add('getCommunityChannels')
      driverRef.current?.send({ kind: 'getCommunityChannels' })
    }
  }, [playerReady, chatAccount])
  const setCommunityClosed = useCallback((id: string, closed: boolean) => {
    setClosedCommunities((prev) => {
      if (prev.has(id) === closed) return prev
      const next = new Set(prev)
      if (closed) next.add(id)
      else next.delete(id)
      if (chatAccount != null) writeAccountList(PREF.chatClosedCommunities, chatAccount, [...next])
      return next
    })
  }, [chatAccount])
  // What the engine listens to: the open community chats. Sent whenever that set changes.
  const openCommunityIds = useMemo(() => communityConvos.filter((c) => !closedCommunities.has(c.id)).map((c) => c.id).sort().join(','), [communityConvos, closedCommunities])
  useEffect(() => {
    if (!playerReady) return
    driverRef.current?.send({ kind: 'setCommunityChannels', ids: openCommunityIds === '' ? [] : openCommunityIds.split(',') })
  }, [openCommunityIds, playerReady])
  // The shown community tab follows community chat's state (the private chat room, the server's
  // message router in it), as a DM tab follows its partner's: one watch, while it is shown.
  useEffect(() => {
    const key = channel
    if (!chatOpen || communityIdOf(key) == null) return
    driverRef.current?.send({ kind: 'dmWatch', address: key, on: true })
    return () => driverRef.current?.send({ kind: 'dmWatch', address: key, on: false })
  }, [chatOpen, channel])

  // Inject a local "DCL System" line (empty sender → rendered as the system member) for command
  // feedback (/help, /goto usage, /commands output) that must NOT be broadcast to other players.
  const pushSystemMessage = useCallback((message: string) => {
    setMessages((prev) =>
      [...prev, { sender: '', message, channel: 'Nearby', messageId: '', id: chatId.current++, ts: Date.now() }].slice(-MAX_CHAT_LINES)
    )
  }, [])

  // A realm change from the HUD: the loader shows from the request until the engine reports the
  // outcome (it validates the destination before leaving the current realm).
  const travel = useCallback((msg: ChangeRealmRequest | (TeleportRequest & { realm: string })) => {
    setLobbyOpen(false)
    const travelId = ++travelSeq.current
    setTravellingTo(msg.realm)
    driverRef.current?.send({ ...msg, travelId })
  }, [])
  const changeRealm = useCallback((realm: string) => travel({ kind: 'changeRealm', realm }), [travel])
  const dismissTravelError = useCallback(() => setTravelError(null), [])

  // Chat send doubles as the slash-command interceptor (parity with bevy-ui-scene's `sendChatMessage`):
  // a recognized `/command` never reaches other players — it teleports, reloads, runs an engine console
  // command, or echoes a system message. Anything else is sent as a normal Nearby message.
  const sendChat = useCallback(
    (text: string) => {
      const cmd = parseChatCommand(text)
      switch (cmd.kind) {
        case 'send':
          if (cmd.text) driverRef.current?.send({ kind: 'sendChat', message: cmd.text, channel: channelRef.current })
          break
        case 'goto':
          driverRef.current?.send({ kind: 'teleport', x: cmd.x, y: cmd.y })
          break
        case 'genesis':
          changeRealm(DEFAULT_REALM)
          break
        case 'world':
          changeRealm(cmd.realm)
          break
        case 'reload':
          driverRef.current?.send({ kind: 'reloadScene' })
          // the reloaded scene starts afresh, so this visit's answers and uses go too
          answeredPermissions.current.clear()
          setVisitAnswers([])
          setTriggeredPermissions([])
          setRefusedTypes([])
          setSeenDenied([])
          skipRefusal.current.clear()
          break
        case 'debug':
          requestDebugPanel(cmd.target, pushSystemMessage)
          break
        case 'commands':
          driverRef.current?.send({ kind: 'consoleCommand', command: 'help' })
          break
        case 'console':
          driverRef.current?.send({ kind: 'consoleCommand', command: cmd.command, args: cmd.args })
          break
        case 'system':
          pushSystemMessage(cmd.message)
          break
      }
    },
    [pushSystemMessage, changeRealm]
  )

  // Toggle one exclusive panel (closing chat + all others); optionally run onOpen.
  // All exclusive (one-at-a-time) panel setters. Toggling one closes chat + the rest.
  const panelSetters = [setFriendsOpen, setSettingsOpen, setProfileOpen, setNotificationsOpen, setEmotesOpen, setBackpackOpen, setCommunitiesOpen, setMapOpen, setPlacesOpen, setEventsOpen, setShopOpen, setGalleryOpen, setCreateOpen, setSkyboxOpen]
  const exclusive = useCallback(
    (setSelf: React.Dispatch<React.SetStateAction<boolean>>, onOpen?: () => void) => {
      setChatOpen(false)
      panelSetters.forEach((set) => {
        if (set !== setSelf) set(false)
      })
      setSelf((o) => {
        if (!o && onOpen) onOpen()
        return !o
      })
    },
    // panelSetters are stable useState setters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    []
  )
  const send = useCallback(
    (kind: 'getSettings' | 'getProfile' | 'getNotifications' | 'getEmotes' | 'getWearables' | 'getCommunities' | 'getMap') => {
      driverRef.current?.send({ kind })
    },
    []
  )
  // Cache catalog-style fetches: only request once per session. Equip/join/setSetting
  // re-emit fresh data through the relay, so we never need to re-pull on reopen. Avoids
  // hammering the catalyst every time a menu is reopened.
  const ensure = useCallback(
    (kind: 'getSettings' | 'getProfile' | 'getEmotes' | 'getWearables' | 'getOutfits' | 'getCommunities') => {
      if (fetchedRef.current.has(kind)) return
      fetchedRef.current.add(kind)
      driverRef.current?.send({ kind })
    },
    []
  )
  const closeAllPanels = useCallback(() => {
    setChatOpen(false)
    setLobbyOpen(false)
    panelSetters.forEach((set) => set(false))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const toggleChat = useCallback(() => {
    panelSetters.forEach((set) => set(false))
    setChatOpen((o) => !o)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  // Opening chat (any path — sidebar, Enter, a queued mention) clears the unread badge, and the
  // shown DM tab's: its lines are on screen again, re-read in case any landed while it was closed.
  useEffect(() => {
    if (!chatOpen) return
    setChatUnread(0)
    const key = channelRef.current
    if (key === 'Nearby') {
      setNearbyUnread(0)
    } else {
      setConversations((prev) => prev.map((c) => (c.address === key ? { ...c, unread: 0 } : c)))
      showCommunity(key)
      driverRef.current?.send({ kind: 'dmHistory', address: key })
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [chatOpen])
  // A community tab shown: its counts are cleared, and who is online is asked for again (presence
  // while it was not shown is not relayed, and a social reconnect can miss some).
  const showCommunity = useCallback((key: string) => {
    const id = communityIdOf(key)
    if (id == null) return
    setCommunityConvos((prev) => prev.map((c) => (c.id === id ? { ...c, unread: 0, mention: false } : c)))
    driverRef.current?.send({ kind: 'getCommunityOnlineMembers', id })
  }, [])
  // Active, the panel shows its rail and the per-channel counts, so the sidebar's can go.
  const setChatActive = useCallback((active: boolean) => {
    chatActiveRef.current = active
    if (active) setChatUnread(0)
  }, [])
  // "Mention" from a profile card opens chat and queues the @name; Chat consumes it into its draft.
  const mentionInChat = useCallback((name: string) => {
    panelSetters.forEach((set) => set(false))
    setChatOpen(true)
    setPendingMention(name)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  const consumeMention = useCallback(() => setPendingMention(null), [])
  // A DM tab for `key` (lowercase wallet), added on first sight: the engine follows the partner's
  // state while the tab is open. Its lines load when it is shown.
  const ensureConversation = useCallback((key: string) => {
    if (openTabsRef.current.has(key)) return
    openTabsRef.current.add(key)
    setConversations((prev) => [...prev, { address: key, unread: 0, state: null, online: false }])
    driverRef.current?.send({ kind: 'dmWatch', address: key, on: true })
  }, [])
  const selectChannel = useCallback((next: string) => {
    const key = channelKey(next)
    setChannel(key)
    if (key === 'Nearby') {
      setNearbyUnread(0)
    } else {
      setConversations((prev) => prev.map((c) => (c.address === key ? { ...c, unread: 0 } : c)))
      if (chatOpenRef.current) showCommunity(key)
      // with chat closed, opening it reads the tab
      if (chatOpenRef.current) driverRef.current?.send({ kind: 'dmHistory', address: key })
    }
  }, [showCommunity])
  const openCommunity = useCallback((community: CommunityChannel) => {
    const key = communityChannel(community.id)
    setCommunityClosed(community.id, false)
    // right after joining, before the list has been re-read
    setCommunityConvos((prev) => (prev.some((c) => c.id === community.id) ? prev : [communityConversation(community), ...prev]))
    panelSetters.forEach((set) => set(false))
    setChatOpen(true)
    selectChannel(key)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [setCommunityClosed, selectChannel])
  const closeCommunity = useCallback((id: string) => {
    const key = communityChannel(id)
    setCommunityClosed(id, true)
    setCommunityConvos((prev) => prev.map((c) => (c.id === id ? { ...c, unread: 0, mention: false, members: null, state: null } : c)))
    setDmLines((prev) => {
      if (!(key in prev)) return prev
      const next = { ...prev }
      delete next[key]
      return next
    })
    if (channelRef.current === key) setChannel('Nearby')
  }, [setCommunityClosed])
  // The tabs shown, with the online count: not the local user, not anyone they blocked.
  const communityConversations = useMemo(() => {
    const blocked = new Set(friendsData.blocked.map((a) => a.toLowerCase()))
    return communityConvos
      .filter((c) => !closedCommunities.has(c.id))
      .map((c) => {
        const members = c.members?.filter((m) => m.address.toLowerCase() !== chatAccount && !blocked.has(m.address.toLowerCase())) ?? null
        return { ...c, members, online: members?.length ?? 0 }
      })
  }, [communityConvos, closedCommunities, chatAccount, friendsData.blocked])
  const openConversation = useCallback((address: string) => {
    const key = channelKey(address)
    panelSetters.forEach((set) => set(false))
    setChatOpen(true)
    ensureConversation(key)
    selectChannel(key)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  const closeConversation = useCallback((address: string) => {
    const key = channelKey(address)
    openTabsRef.current.delete(key)
    setConversations((prev) => prev.filter((c) => c.address !== key))
    setDmLines((prev) => {
      const next = { ...prev }
      delete next[key]
      return next
    })
    driverRef.current?.send({ kind: 'dmWatch', address: key, on: false })
    if (channelRef.current === key) setChannel('Nearby')
  }, [])
  const deleteHistory = useCallback((address: string) => {
    const key = channelKey(address)
    driverRef.current?.send({ kind: 'dmDelete', address: key })
    setDmLines((prev) => ({ ...prev, [key]: [] }))
    // a read queued before the delete would answer with the old rows: this one answers after it
    driverRef.current?.send({ kind: 'dmHistory', address: key })
  }, [])
  // Nearby reactions follow the lines kept: those for lines trimmed off the log go with them.
  useEffect(() => {
    const ids = new Set(messages.map((l) => l.messageId).filter((id) => id !== ''))
    nearbyIdsRef.current = ids
    setNearbyReactions((prev) => {
      const kept = Object.entries(prev).filter(([id]) => ids.has(id))
      return kept.length === Object.keys(prev).length ? prev : Object.fromEntries(kept)
    })
  }, [messages])
  const channelMessages = useMemo(
    () =>
      channel === 'Nearby'
        ? messages.map((l) => (nearbyReactions[l.messageId] != null ? { ...l, reactions: nearbyReactions[l.messageId] } : l))
        : (dmLines[channel] ?? []),
    [messages, nearbyReactions, dmLines, channel]
  )
  const reactInChat = useCallback((messageId: string, emoji: string, remove: boolean) => {
    driverRef.current?.send({ kind: 'sendChatReaction', channel: channelRef.current, messageId, emoji, remove })
  }, [])
  const bubbleText = useCallback((sender: string, messageId: string, message: string) => {
    driverRef.current?.send({ kind: 'chatBubbleText', sender, messageId, message })
  }, [])

  // The full-screen main menu — mirrors App's `pageOpen`.
  const menuPageOpen =
    settingsOpen || backpackOpen || communitiesOpen || mapOpen || placesOpen || eventsOpen || shopOpen || galleryOpen || lobbyOpen || createOpen
  // What takes the screen from the chat, for requestFocusChat: the main menu, the emote wheel, and
  // the two modals App renders above everything (permission prompt, fatal error).
  chatCoveredRef.current =
    menuPageOpen || emotesOpen || permissionQueue.length > 0 || fatalError != null || (EDITOR_BUILD && editorMode !== 'off')

  // In-world, cancel is an ENGINE action: the cancel key flows to the engine like any other
  // input, resolves to 'Cancel', and comes back on the action stream — the dispatcher below
  // performs the one layered close (topmost popup, else open panels) for keyboard and
  // gamepad alike. No DOM cancel handling here; see the pre-world fallback further down.
  const anyPanelOpen =
    menuPageOpen || friendsOpen || profileOpen || notificationsOpen || emotesOpen || skyboxOpen
  // Everything that can sit over the in-world lobby: Cancel closes these before the lobby.
  const anyPanelOpenExceptLobby =
    settingsOpen || backpackOpen || communitiesOpen || mapOpen || placesOpen || eventsOpen || shopOpen || galleryOpen ||
    friendsOpen || profileOpen || notificationsOpen || emotesOpen || skyboxOpen
  const toggleFriends = useCallback(() => exclusive(setFriendsOpen), [exclusive])
  const loadSettings = useCallback(() => ensure('getSettings'), [ensure])
  // Re-read on every open: the engine changes settings itself (the DM privacy slot follows the
  // social service), and settings are local, so there is nothing to spare by caching.
  const toggleSettings = useCallback(() => exclusive(setSettingsOpen, () => send('getSettings')), [exclusive, send])
  const toggleProfile = useCallback(() => exclusive(setProfileOpen, () => ensure('getProfile')), [exclusive, ensure])
  const toggleNotifications = useCallback(() => exclusive(setNotificationsOpen, () => send('getNotifications')), [exclusive, send])
  const toggleEmotes = useCallback(() => exclusive(setEmotesOpen, () => ensure('getEmotes')), [exclusive, ensure])
  const toggleBackpack = useCallback(
    () => {
      if (!playerReadyRef.current) return
      exclusive(setBackpackOpen, () => {
        ensure('getWearables')
        ensure('getEmotes') // Backpack's Emotes tab reuses the emotes list.
        ensure('getOutfits') // Backpack's Outfits tab.
        driverRef.current?.send({ kind: 'getOwnedNames' })
      })
    },
    [exclusive, ensure]
  )
  const toggleCommunities = useCallback(() => exclusive(setCommunitiesOpen, () => ensure('getCommunities')), [exclusive, ensure])
  const loadCommunityBrowse = useCallback((search: string) => {
    communityBrowseWanted.current = { search, page: 1 }
    communityBrowsePage.current = 0
    updateCommunityBrowse(() => ({ search, items: [], total: 0, loading: true, loadingMore: false }))
    driverRef.current?.send({ kind: 'browseCommunities', search, page: 1 })
  }, [updateCommunityBrowse])
  const loadMoreCommunityBrowse = useCallback(() => {
    const current = communityBrowseRef.current
    if (communityBrowseWanted.current != null || current.loading || current.loadingMore) return
    if (communityBrowsePage.current === 0 || current.items.length >= current.total) return
    const page = communityBrowsePage.current + 1
    communityBrowseWanted.current = { search: current.search, page }
    updateCommunityBrowse((prev) => ({ ...prev, loadingMore: true }))
    driverRef.current?.send({ kind: 'browseCommunities', search: current.search, page })
  }, [updateCommunityBrowse])
  const fetchCommunity = useCallback((id: string) => {
    // a failed fetch's message would otherwise outlive a later success
    setCommunityError((prev) => (prev?.id === id ? null : prev))
    driverRef.current?.send({ kind: 'getCommunity', id })
  }, [])
  const toggleMap = useCallback(() => exclusive(setMapOpen, () => send('getMap')), [exclusive, send])
  // Places fetches its own HTTP data (no bridge), so opening needs no engine request.
  const togglePlaces = useCallback(() => exclusive(setPlacesOpen), [exclusive])
  const toggleEvents = useCallback(
    (source?: EventsOpenSource) => {
      // a click handler may pass its event here
      eventsOpenSource.current = typeof source === 'string' ? source : null
      exclusive(setEventsOpen)
    },
    [exclusive]
  )
  const clearEvents = eventsData.clear
  useEffect(() => {
    if (!eventsOpen) {
      // nothing is kept between visits: the page reads again each time it opens
      clearEvents()
      return
    }
    if (eventsOpenSource.current != null) track('events_section_opened', { source: eventsOpenSource.current })
    eventsOpenSource.current = null
  }, [eventsOpen, clearEvents])
  const eventCommunities = useMemo(() => communityConvos.map((c) => ({ id: c.id, name: c.name, thumbnail: c.thumbnail })), [communityConvos])
  // The in-world Shop's try-on dresses the current look, so opening it asks for the look the way the
  // Backpack does (where this host runs that Shop).
  const toggleShop = useCallback(
    () =>
      exclusive(setShopOpen, () => {
        if (IN_WORLD_SHOP && playerReadyRef.current) ensure('getWearables')
      }),
    [exclusive, ensure]
  )
  const toggleCreate = useCallback(() => exclusive(setCreateOpen), [exclusive])
  const showCreate = useCallback((open: boolean) => {
    setChatOpen(false)
    panelSetters.forEach((set) => set(open && set === setCreateOpen))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  const runDebugCommand = useCallback((line: string) =>
    driverRef.current?.command?.(line) ?? Promise.reject(new Error('Engine console unavailable')), [])

  // The engine clock as `/time` (no args) reports it; null when there is no engine console.
  const readClock = useCallback(async (): Promise<{ hours: number; speed: number } | null> => {
    const reply = await driverRef.current?.command?.('/time').catch(() => undefined)
    const clock = reply != null ? parseTimeReply(reply) : null
    if (clock != null && clock.speed !== 0) skyboxDaySpeed.current = clock.speed
    return clock
  }, [])
  const setClock = useCallback((hours: number, speed: number) => {
    driverRef.current
      ?.command?.(`/time ${hours.toFixed(2)} ${speed}`)
      .catch((e: unknown) => console.warn('[hud] /time failed:', e))
  }, [])
  const toggleSkybox = useCallback(
    () =>
      exclusive(setSkyboxOpen, () => {
        void readClock().then((clock) => {
          if (clock == null) return
          setSkyboxHoursState(clock.hours)
          setSkyboxProgressingState(clock.speed !== 0)
        })
      }),
    [exclusive, readClock]
  )
  const setSkyboxHours = useCallback(
    (hours: number) => {
      setSkyboxHoursState(hours)
      if (!skyboxProgressing) setClock(hours, 0)
    },
    [skyboxProgressing, setClock]
  )
  const setSkyboxProgressing = useCallback(
    (on: boolean) => {
      setSkyboxProgressingState(on)
      if (on) {
        setClock(skyboxHours, skyboxDaySpeed.current)
        return
      }
      // Freeze where the running clock is now, not where the slider last was.
      void readClock().then((clock) => {
        const hours = clock?.hours ?? skyboxHours
        setSkyboxHoursState(hours)
        setClock(hours, 0)
      })
    },
    [skyboxHours, readClock, setClock]
  )
  // While the day cycle runs, the open menu follows the engine clock (Unity's slider tracks it too).
  useEffect(() => {
    if (!skyboxOpen || !skyboxProgressing) return
    let live = true
    const id = setInterval(() => {
      void readClock().then((clock) => {
        if (live && clock != null && clock.speed !== 0) setSkyboxHoursState(clock.hours)
      })
    }, 1000)
    return () => {
      live = false
      clearInterval(id)
    }
  }, [skyboxOpen, skyboxProgressing, readClock])
  const toggleGallery = useCallback(() => exclusive(setGalleryOpen), [exclusive])
  const setPhotoModeEnabled = useCallback((enable: boolean, source: PhotoModeSource) => {
    driverRef.current?.send({ kind: 'setPhotoMode', enable, source })
  }, [])
  const takePhoto = useCallback(() => {
    driverRef.current?.send({ kind: 'takePhoto' })
  }, [])
  const loadGalleryPhoto = useCallback((id: string) => {
    driverRef.current?.send({ kind: 'getGalleryPhoto', id })
  }, [])
  // A reply that never comes (the bridge scene reloaded mid-request) resolves null after
  // `timeoutMs`, so a page never waits for good.
  const bridgeCall = useCallback(
    <K extends BridgeReply['kind']>(
      kind: K,
      build: (requestId: number) => PageToScene,
      timeoutMs = BRIDGE_CALL_TIMEOUT_MS
    ): Promise<Extract<BridgeReply, { kind: K }> | null> => {
      const driver = driverRef.current
      if (driver == null) return Promise.resolve(null)
      const requestId = ++bridgeRequestRef.current
      return new Promise((resolve) => {
        const timer = setTimeout(() => {
          bridgePendingRef.current.delete(requestId)
          resolve(null)
        }, timeoutMs)
        bridgePendingRef.current.set(requestId, (msg) => {
          clearTimeout(timer)
          resolve(msg.kind === kind ? (msg as Extract<BridgeReply, { kind: K }>) : null)
        })
        try {
          driver.send(build(requestId))
        } catch (e) {
          console.error(`[bridge] ${kind} request could not be sent`, e)
          clearTimeout(timer)
          bridgePendingRef.current.delete(requestId)
          resolve(null)
        }
      })
    },
    []
  )
  const fetchGalleryPage = useCallback(
    async (offset: number, limit: number): Promise<ReelPage> => {
      const msg = await bridgeCall('gallery', (requestId) => ({ kind: 'getGallery', requestId, offset, limit }))
      return msg == null
        ? { ok: false, photos: [], total: 0, max: 0 }
        : { ok: msg.ok, photos: msg.photos, total: msg.current, max: msg.max, received: msg.received }
    },
    [bridgeCall]
  )
  const openExternalUrl = useCallback(
    async (url: string): Promise<boolean> => (await bridgeCall('externalUrlOpened', (requestId) => ({ kind: 'openExternalUrl', requestId, url })))?.ok === true,
    [bridgeCall]
  )
  const setGalleryPhotoPublic = useCallback(
    async (id: string, isPublic: boolean): Promise<ReelWrite> =>
      (await bridgeCall('galleryWrite', (requestId) => ({ kind: 'setGalleryPhotoPublic', requestId, id, isPublic }))) ?? { ok: false },
    [bridgeCall]
  )
  const removeGalleryPhoto = useCallback(
    async (id: string): Promise<ReelWrite> =>
      (await bridgeCall('galleryWrite', (requestId) => ({ kind: 'deleteGalleryPhoto', requestId, id }), GALLERY_DELETE_TIMEOUT_MS)) ?? { ok: false },
    [bridgeCall]
  )
  const teleport = useCallback((x: number, y: number) => {
    setLobbyOpen(false)
    driverRef.current?.send({ kind: 'teleport', x, y })
  }, [])
  const teleportToPlace = useCallback(
    (x: number, y: number) => {
      if (!isGenesisSatelliteView(satelliteView)) travel({ kind: 'teleport', realm: DEFAULT_REALM, x, y })
      else teleport(x, y)
    },
    [satelliteView, teleport, travel]
  )
  // The in-world lobby: open it over the world (closing other pages), and travel from it the
  // way any in-world pick does — a runtime realm change or teleport, then close.
  const toggleLobby = useCallback(() => {
    // a page over the lobby (or anything else) gives way to it; only the lobby alone closes
    const closing = lobbyOpen && !anyPanelOpenExceptLobby
    setChatOpen(false)
    panelSetters.forEach((set) => set(false))
    setLobbyOpen(!closing)
    if (!closing) {
      setLobbyStageReady(false)
      screenViewed('LOBBY')
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [lobbyOpen, anyPanelOpenExceptLobby])
  const homeScene = useCallback(() => driverRef.current?.homeScene?.() ?? home, [home])
  useEffect(() => {
    if ((!lobby && !lobbyOpen) || driverRef.current?.homeScene != null) return
    setHome(null)
    driverRef.current?.send({ kind: 'getHomeScene' })
  }, [lobby, lobbyOpen])
  // Opening the Backpack or the lobby rebuilds the stage, so the page's stand-in shows again.
  useEffect(() => {
    if (backpackOpen) setLobbyStageReady(false)
  }, [backpackOpen])
  const travelFromLobbyInWorld = useCallback(
    (dest: Destination) => {
      if (dest == null) {
        const home = homeScene()
        if (home == null) return
        const [x, y] = home.parcel.split(',').map(Number)
        travel({ kind: 'teleport', realm: home.realm ?? DEFAULT_REALM, x, y })
      } else if (dest.kind === 'parcel') teleportToPlace(dest.x, dest.y)
      else {
        const [x, y] = (dest.position ?? '').split(',').map(Number)
        if (Number.isFinite(x) && Number.isFinite(y)) travel({ kind: 'teleport', realm: dest.realm, x, y })
        else travel({ kind: 'changeRealm', realm: dest.realm })
      }
      setLobbyOpen(false)
    },
    [homeScene, teleportToPlace, travel]
  )
  const setMinimapConfig = useCallback(
    (config: { style: MinimapStyle; rotation: MinimapRotation; visibleMeters: number }) => {
      driverRef.current?.send({ kind: 'minimapConfig', ...config })
    },
    []
  )
  const loadPermissionRules = useCallback(() => {
    const scene = currentScene.current
    if (scene != null) driverRef.current?.send({ kind: 'getPermissionRules', scene })
  }, [])

  // The HUD answered for `scene` + `ty` (a prompt or the review): remember it; a 'visit' answer is
  // listed, an "Always …" answer shows as the stored rule once refetched. What the scene did under
  // the previous state no longer points at a row.
  const answerPermission = (subject: Omit<VisitAnswer, 'allowed'>, allow: boolean, level: PermissionRuleLevel): void => {
    const key = `${subject.scene}|${subject.ty}`
    answeredPermissions.current.set(key, level === 'visit' ? allow : null)
    const others = (u: VisitAnswer[]): VisitAnswer[] => u.filter((e) => !(e.scene === subject.scene && e.ty === subject.ty))
    const answer: VisitAnswer = { ty: subject.ty, sceneName: subject.sceneName, scene: subject.scene, realm: subject.realm, allowed: allow }
    setVisitAnswers((u) => (level === 'visit' ? [...others(u), answer] : others(u)))
    setTriggeredPermissions((t) => t.filter((e) => !(e.scene === subject.scene && e.ty === subject.ty)))
    setRefusedTypes((r) => r.filter((ty) => ty !== subject.ty))
    setSeenDenied((s) => s.filter((ty) => ty !== subject.ty))
  }

  const setPermissionRule = useCallback(
    (subject: PermissionSubject, allow: boolean, level: PermissionRuleLevel) => {
      driverRef.current?.send({ kind: 'permissionRule', ty: subject.ty, level, allow, scene: subject.scene, realm: subject.realm })
      answerPermission(subject, allow, level)
      loadPermissionRules()
    },
    [loadPermissionRules]
  )

  // the rule (or visit answer) is gone: the scene asks next time
  const clearPermissionRule = useCallback(
    (subject: PermissionSubject) => {
      if (subject.scope !== 'visit') {
        driverRef.current?.send({ kind: 'permissionRule', ty: subject.ty, level: subject.scope, allow: null, scene: subject.scene, realm: subject.realm })
      }
      answeredPermissions.current.delete(`${subject.scene}|${subject.ty}`)
      setVisitAnswers((u) => u.filter((e) => !(e.scene === subject.scene && e.ty === subject.ty)))
      setTriggeredPermissions((t) => t.filter((e) => !(e.scene === subject.scene && e.ty === subject.ty)))
      setRefusedTypes((r) => r.filter((ty) => ty !== subject.ty))
      setSeenDenied((s) => s.filter((ty) => ty !== subject.ty))
      loadPermissionRules()
    },
    [loadPermissionRules]
  )

  const resetPermissions = useCallback(() => {
    driverRef.current?.send({ kind: 'resetPermissions' })
    answeredPermissions.current.clear()
    setVisitAnswers([])
    setTriggeredPermissions([])
    setRefusedTypes([])
    setSeenDenied([])
    skipRefusal.current.clear()
    loadPermissionRules()
  }, [loadPermissionRules])

  const markPermissionsSeen = useCallback(() => setSeenDenied(refusedTypes), [refusedTypes])
  const unseenDenied = refusedTypes.filter((ty) => !seenDenied.includes(ty)).length

  const resolvePermission = useCallback(
    (id: number, allow: boolean, level: PermissionLevelChoice) => {
      setPermissionQueue((q) => {
        const req = q.find((r) => r.id === id)
        if (req) {
          // a dismissed prompt ('once') is no answer: the scene asks again, its refusal has no row
          if (level === 'once') answeredPermissions.current.set(`${req.scene}|${req.ty}`, null)
          else answerPermission({ ty: req.ty, sceneName: req.sceneName, scene: req.scene, realm: req.realm }, allow, level)
          // the player refused this one themselves: its report is not a silent refusal
          if (!allow) skipRefusal.current.add(`${req.scene}|${req.ty}`)
          driverRef.current?.send({ kind: 'permissionResolve', id, ty: req.ty, allow, level, scene: req.scene, realm: req.realm })
          // a visit answer settles the scene's other queued requests for the type as well
          if (level === 'visit') {
            const same = q.filter((r) => r.id !== id && r.scene === req.scene && r.ty === req.ty)
            for (const r of same) driverRef.current?.send({ kind: 'permissionResolve', id: r.id, ty: r.ty, allow, level, scene: r.scene, realm: r.realm })
            return q.filter((r) => r.id !== id && !same.includes(r))
          }
        }
        return q.filter((r) => r.id !== id)
      })
    },
    []
  )
  const ownConsoleCommand = useCallback((command: string, args: string[]) => {
    ownConsoleCommands.current.push([command, ...args].join(' '))
    driverRef.current?.send({ kind: 'consoleCommand', command, args })
  }, [])
  const stopPortable = useCallback((hash: string) => ownConsoleCommand('stop_portable', [hash]), [ownConsoleCommand])
  const restartSmartWearable = useCallback((urn: string) => ownConsoleCommand('smart_wearable', ['restart', urn]), [ownConsoleCommand])
  const resolveWorldPassword = useCallback((password: string | null) => {
    setWorldPassword((p) => {
      if (p == null) return p
      driverRef.current?.send({ kind: 'worldPasswordResolve', password })
      return null
    })
  }, [])
  // Runs the login captured at Jump In, once the engine is up.
  const runPendingLogin = useCallback((driver: NonNullable<typeof driverRef.current>): void => {
    const login = pendingLogin.current
    pendingLogin.current = null
    const type = pendingLoginType.current
    pendingLoginType.current = null
    const signOut = signOuts.current
    // A sign-out came while the engine held the sign-in: the HUD is back on its start page, so the
    // answer only ends the wait (`busy`), and a sign-in that landed is signed out again.
    const stale = (): boolean => {
      if (signOuts.current === signOut) return false
      setBusy(false)
      return true
    }
    Promise.resolve(login?.(driver))
      .then(() => {
        if (stale()) {
          if (login != null) driver.logout().catch((e: Error) => console.error('[session] logout failed', e))
          return
        }
        if (type != null) {
          lastLoginType.current = type === 'stored' ? storedLoginType(storedRef.current) : type
          screenViewed('AUTH_SUCCESS', { login_type: lastLoginType.current })
        }
        welcomeLink.current = urlDestination.current
        urlDestination.current = null
        // only a sign-in that ran lands it: a run with nothing pending answers no /hud_login, and
        // one whose command was already answered keeps the stored sign-in it set
        if (login != null && hudStored.current != null) hudStored.current.landed = true
        if (login != null && hudLoginPending.current != null) {
          hudLoginPending.current.landed = true
          hudLoginPending.current.welcomeAwaited = welcomeShown()
          setHudLoginTick((n) => n + 1)
        }
        setBusy(false)
        if (welcomeShown()) askWelcome(driver)
      })
      .catch((e: unknown) => {
        console.error('[login] post-launch login failed:', e)
        // an unattended run's host still hears of the failure, which ends the run
        if (boot.current.autoLogin !== 'unattended' && stale()) return
        restoreHudSignIn()
        // The engine driver rejects with a RAW STRING (wasm-bindgen JsValue), not an Error —
        // e.message would be undefined and the login screen would show no error at all.
        const msg = e instanceof Error ? e.message : String(e)
        screenViewed('AUTH_ERROR', { error: msg })
        settleHudLogin(new Error(msg !== '' ? msg : 'Login failed'))
        // No person to retry or pick another way: the unattended run ends (its host reports the
        // failure), and nothing here returns to a sign-in screen or tries again.
        if (boot.current.autoLogin === 'unattended') {
          const run = unattendedRun()
          if (run != null && driverRef.current != null) run.loginFailed(driverRef.current, `sign-in failed: ${msg}`)
          return
        }
        setError(msg !== '' ? msg : 'Login failed')
        setBusy(false)
        // Back to the LOGIN screen, not the picker or lobby: neither renders an error, and the
        // login screen is where the retry / profile-reset actions live. The engine stays
        // launched (start()'s __bevyStarted guard makes the next launch a no-op).
        setSubmitted(false)
        setDestinationPicked(false)
        setLobby(false)
        setWelcomePending(false)
        guestPick.current = null
      })
  }, [])

  // Boots the engine (once per page) and watches the boot for a panic. False when launch threw.
  const launchEngine = useCallback(
    (driver: NonNullable<typeof driverRef.current>, realm: string, position?: string, host?: LaunchHostOptions): boolean => {
      bootPollStop.current = false
      driver.clearEnginePanic?.() // start clean so the boot poll only sees THIS launch's panic
      try {
        driver.launch?.(realm, position, host)
      } catch (e) {
        // A boot-time engine panic throws synchronously out of launch() (a generic "unreachable"
        // wasm trap). The readable message is captured on the engine via enginePanic().
        const panic = driverRef.current?.enginePanic?.()?.message
        setFatalError({ message: panic ?? (e as Error)?.message ?? 'The engine failed to start.', source: 'launch' })
        driverRef.current?.clearEnginePanic?.()
        setBusy(false)
        return false
      }
      launchedRef.current = true
      // A boot panic can also surface a frame or two AFTER launch() returns (async wasm init / OnceCell)
      // — launch() returns normally, so poll the panic hook during boot and raise it as a FATAL 'launch'
      // error, not the dismissable 'runtime' crash the heartbeat watchdog would otherwise mislabel it as.
      // Stops on world-entry (bootPollStop) or after the window elapses.
      let polls = 0
      const pollPanic = (): void => {
        if (bootPollStop.current) return
        const panic = driverRef.current?.enginePanic?.()?.message
        if (panic != null) {
          setFatalError((prev) => prev ?? { message: panic, source: 'launch' })
          driverRef.current?.clearEnginePanic?.()
          return
        }
        if (++polls < 24) pollTimer.current = setTimeout(pollPanic, 250) // ~6s boot window
      }
      pollTimer.current = setTimeout(pollPanic, 250)
      return true
    },
    []
  )

  // A link's destination goes straight into the world, with no stage before it: the boot bar has
  // no avatar to wait for (the lobby's stage reports its own).
  useEffect(() => {
    if (engineReady && urlDestination.current != null) setAvatarLoaded(true)
  }, [engineReady])

  // A visitor with no session gets the engine the moment it is ready, holding the world, with no
  // sign-in: the guest start page's stage then shows the live avatar instead of a snapshot, and
  // ACCEPT AND PLAY only has the sign-in left to run (submitLogin sees launchedRef). A link's
  // destination and the auto-login boots launch for themselves.
  useEffect(() => {
    const driver = driverRef.current
    if (!engineReady || submitted || status !== 'sign-in-or-guest' || driver?.launch == null || launchedRef.current) return
    if (urlDestination.current != null || boot.current.autoLogin != null) return
    const home = driver.homeScene?.()
    launchEngine(driver, home?.realm ?? DEFAULT_REALM, home?.parcel ?? '0,0', { holdWorld: true })
  }, [engineReady, submitted, status, launchEngine])

  // A destination for an engine already running at its own start realm (native, mock): runtime
  // directives instead of boot parameters. A world switches realm now (works pre-login); a parcel
  // teleport needs a spawned player, so it's held until playerReady. A parcel pick sends no realm
  // change: if the picker was reachable at all the engine omitted ?realm=, which it only does when
  // it booted on the HUD's own DEFAULT_REALM — a changeRealm to the same realm is NOT a no-op (full
  // scene purge + reconnect). Skip likewise keeps the engine's own start realm.
  const travelInPlace = useCallback(
    (driver: NonNullable<typeof driverRef.current>, dest: Destination): void => {
      // Deferred to the playerReady flush only if the player hasn't spawned yet. Fresh sign-in
      // completes login BEFORE the picker, so playerReady has usually fired by pick time and the
      // flush would never run again — send immediately then. An immediate teleport still lands
      // after a just-sent changeRealm: the engine applies teleports after realm changes and
      // overrides the spawn position.
      const sendParcel = (x: number, y: number): void => {
        if (playerReadyRef.current) driver.send({ kind: 'teleport', x, y })
        else pendingParcel.current = { x, y }
      }
      if (dest?.kind === 'world') {
        travel({ kind: 'changeRealm', realm: dest.realm })
        const [x, y] = (dest.position ?? '').split(',').map(Number)
        if (Number.isFinite(x) && Number.isFinite(y)) sendParcel(x, y)
      } else if (dest?.kind === 'parcel') {
        sendParcel(dest.x, dest.y)
      }
    },
    [travel]
  )

  // Leaving the lobby. The engine is holding the world on its boot realm (native holds whenever
  // the lobby shows, see src/lib.rs), and only a realm change releases it, so every destination
  // names its realm (Skip goes home).
  const travelFromLobby = useCallback(
    (dest: Destination): void => {
      if (dest == null) {
        const home = homeScene()
        const [x, y] = (home?.parcel ?? '0,0').split(',').map(Number)
        travel({ kind: 'teleport', realm: home?.realm ?? DEFAULT_REALM, x, y })
      } else if (dest.kind === 'parcel') {
        travel({ kind: 'teleport', realm: DEFAULT_REALM, x: dest.x, y: dest.y })
      } else {
        const [x, y] = (dest.position ?? '').split(',').map(Number)
        if (Number.isFinite(x) && Number.isFinite(y)) travel({ kind: 'teleport', realm: dest.realm, x, y })
        else travel({ kind: 'changeRealm', realm: dest.realm })
      }
    },
    [homeScene, travel]
  )
  // A link held for the terms is released by travelling there; the lobby keeps its hold until a
  // destination is picked.
  const releaseLink = useCallback(() => {
    const dest = heldDest.current
    heldDest.current = undefined
    welcomeLink.current = null
    if (dest !== undefined && !lobbyRef.current) travelFromLobby(dest)
  }, [travelFromLobby])
  const releaseLinkRef = useRef(releaseLink)
  releaseLinkRef.current = releaseLink

  // Post-jump-in Places picker (or the lobby): choose a destination (or null to skip → home),
  // then leave the picker. A world switches realm now; a parcel is teleported once the avatar spawns.
  const pickDestination = useCallback((dest: Destination): boolean => {
    const driver = driverRef.current
    if (driver == null) return false
    // from the lobby, a pick waits for sign-in: it releases the hold, so a failed sign-in
    // afterwards would leave the world loaded under the sign-in screen
    if (lobbyRef.current && busyRef.current) return false
    setDestinationPicked(true) // flip to the loading overlay first
    // and the boot bar stops waiting on the lobby's stage: a pick before the stage reported (a
    // stored session's stage is only built after its sign-in) leaves it unreported. It stays set if
    // the travel fails back to the lobby: the bar has gone by then.
    setAvatarLoaded(true)
    if (lobbyRef.current) {
      travelFromLobby(dest)
      return true
    }
    setBusy(true)

    // Boot the engine straight at the chosen destination, then run the deferred login. `engine_run`
    // is heavy and runs on the shared main thread, so defer it a paint (rAF, setTimeout fallback) so
    // the loading overlay is on screen before the freeze — same trick as the login loader. Run once.
    const signOut = signOuts.current
    let ran = false
    const run = (): void => {
      // Bail if the session unmounted during the deferred kick — cleanup nulls driverRef, so this
      // guards against launching on a disposed driver (the rAF/timeout aren't otherwise cancellable).
      if (ran || driverRef.current == null) return
      ran = true
      // signed out before the deferred run: neither the launch nor the sign-in runs
      if (signOuts.current !== signOut) return setBusy(false)
      if (driver.launch == null) {
        travelInPlace(driver, dest)
        runPendingLogin(driver)
        return
      }
      // World by realm, parcel by spawn position, skip at 0,0 (Genesis). Nothing loaded before this,
      // so only the chosen scene streams in. (No-op on the mock, which has no engine to launch.)
      // Parcels pass the MAIN realm explicitly — the engine's initialRealm may carry a ?realm
      // override (possibly an invalid world after a failed validation), and inheriting it would
      // strand a Genesis pick "Reconnecting to the realm" forever.
      let launched: boolean
      if (dest == null) {
        // Skip goes HOME — the engine's persisted home scene (0,0 on the default realm unless
        // the user pinned one; the engine gives no realm before launch, so the default is ours).
        const home = driver.homeScene?.()
        launched = launchEngine(driver, home?.realm ?? DEFAULT_REALM, home?.parcel ?? '0,0', holdForTerms(driver, dest))
      } else if (dest.kind === 'world') launched = launchEngine(driver, dest.realm, dest.position, holdForTerms(driver, dest))
      else launched = launchEngine(driver, DEFAULT_REALM, `${dest.x},${dest.y}`, holdForTerms(driver, dest))
      if (launched) runPendingLogin(driver)
      else {
        // a /hud_login's sign-in never ran: answered with that, its stored sign-in put back
        restoreHudSignIn()
        settleHudLogin(new Error('the engine failed to launch'))
      }
    }
    requestAnimationFrame(() => requestAnimationFrame(run))
    setTimeout(run, 60)
    return true
  }, [launchEngine, runPendingLogin, travelFromLobby, travelInPlace, settleHudLogin, restoreHudSignIn])
  // Boot-mode flags (?hud=0 / ?guest=1 / ?systemScene= — see lib/bootMode.ts), captured once
  // per session mount so tests can vary location.search between mounts.
  const boot = useRef(bootMode())
  // The welcome page needs a HUD to show it, the sites embed's auto guest skips it, and an
  // unattended run has no one to accept it.
  const welcomeShown = (): boolean =>
    !boot.current.hideHud && boot.current.autoLogin !== 'guest' && boot.current.autoLogin !== 'unattended'
  // A destination launched straight in (a link) holds the world while the terms wait for the page.
  // Not in preview, which refuses the realm change that would release it.
  const holdForTerms = (driver: LoginDriver, dest: Destination): LaunchHostOptions | undefined => {
    if (!welcomeShown() || new URLSearchParams(location.search).has('preview') || driver.termsAccepted?.() !== false) return undefined
    heldDest.current = dest
    return { holdWorld: true }
  }
  // ?position=x,y / ?realm= (parity with the plain engine page): skip the Places picker and launch
  // straight there. realm wins when both are given, carrying the position along — letting
  // ?position shadow ?realm made a reload in a custom realm respawn in Genesis at the same
  // coordinates (a parcel launch passes DEFAULT_REALM explicitly). The engine's URL sync only
  // writes ?position when the realm honours one; realms with fixed scene urns (worlds) spawn at
  // their base scene and ignore it anyway. Consumed by the first successful login — a failed one
  // retries to the same destination, the engine having already launched there unheld, where the
  // lobby can't work; after a sign-out the picker shows normally.
  const urlDestination = useRef<Destination>(
    (() => {
      const q = new URLSearchParams(location.search)
      const raw = q.get('position')
      const [x, y] = raw?.split(',').map((n) => parseInt(n.trim(), 10)) ?? []
      const hasPosition = Number.isFinite(x) && Number.isFinite(y)
      const realm = entryRealm(q)
      if (realm != null) return { kind: 'world', realm, position: hasPosition ? `${x},${y}` : undefined }
      if (hasPosition) return { kind: 'parcel', x, y }
      // A hidden HUD renders no picker — land at Genesis 0,0 so the boot doesn't strand on an
      // invisible screen (parity with the picker's "skip").
      if (boot.current.hideHud) return { kind: 'parcel', x: 0, y: 0 }
      // Likewise for an unattended run: nobody picks a destination, so it goes where the run goes
      const run = boot.current.autoLogin === 'unattended' ? unattendedRun() : null
      if (run != null) return run.destination
      // An editor link (?editor[=<projectId>]) opens the editor once in-world: land rather than wait in the lobby.
      if (editorEntry(location.search).open) return { kind: 'parcel', x: 0, y: 0 }
      return null
    })()
  )
  // The sign-out count the realm check in flight started under (null = none): a sign-in after a
  // sign-out starts its own check rather than wait for one whose answer is dropped.
  const validatingRealm = useRef<number | null>(null)
  useEffect(() => {
    if (!submitted || destinationPicked || urlDestination.current == null) return
    const dest = urlDestination.current
    if (dest.kind === 'world' && driverRef.current?.launch == null) {
      // Native: ?realm= is injected by the engine from its own --realm, so the engine is
      // already there — skip the picker and keep the realm (the no-launch pickDestination(null)
      // path). No validation fetch either: the engine booted on this realm, and preview/file
      // realms wouldn't pass the worlds-server about probe anyway.
      pickDestination(null)
      return
    }
    // An unattended run skips the pre-flight: the run is the engine's, and an unreachable realm fails
    // there, where the run reports it.
    if (dest.kind === 'world' && boot.current.autoLogin === 'unattended') {
      pickDestination(dest)
      return
    }
    if (dest != null && dest.kind === 'world') {
      if (validatingRealm.current === signOuts.current) return
      const signOut = signOuts.current
      validatingRealm.current = signOut
      // Launching against an unreachable realm strands the engine in a cryptic login failure, so
      // block up front: 404 → not found, no/failed answer (incl. timeout) → unreachable.
      checkRealm(dest.realm)
        .then((result) => {
          // signed out meanwhile: the next sign-in checks the realm again
          if (signOuts.current !== signOut) return
          if (result === 'ok') pickDestination(dest)
          else {
            urlDestination.current = null
            setFatalError({ message: realmCheckMessage(dest.realm, result), source: 'realm' })
            // a /hud_login's sign-in never ran: answered with that, its stored sign-in put back,
            // and it never runs later from the picker the dismissed dialog leads to (a person's
            // own link sign-in does), so the HUD goes back to its start page
            const runners = hudStored.current != null && !hudStored.current.landed
            restoreHudSignIn()
            settleHudLogin(new Error(realmCheckMessage(dest.realm, result)))
            if (runners) {
              guestPick.current = null
              pendingLogin.current = null
              pendingLoginType.current = null
              setSubmitted(false)
              submittedRef.current = false
            }
          }
        })
        .finally(() => {
          if (validatingRealm.current === signOut) validatingRealm.current = null
        })
      return
    }
    pickDestination(dest)
  }, [submitted, destinationPicked, pickDestination, settleHudLogin, restoreHudSignIn])
  // Optimistically reflect a new equipped set so the button flips to "Unequip" immediately AND the
  // equipped set stays authoritative for the next equip — the engine deploy + wearables re-emit lags,
  // and a failed deploy never re-emits at all. `urns` is the full next set (see BackpackPage), so
  // rebuild `equippedWearables` from it (resolving each urn against the catalog ∪ current equipped),
  // mirroring bevy-ui-scene's local-store-is-source-of-truth model. Otherwise a stale equipped set
  // makes each subsequent equip drop the previously-equipped items from the deploy.
  const applyEquippedOptimistic = useCallback((urns: string[]) => {
    const matches = (u: string, urn: string): boolean => u === urn || u.startsWith(`${urn}:`)
    setCatalogItems((list) => list.map((w) => ({ ...w, equipped: urns.some((u) => matches(u, w.urn)) })))
    setEquippedWearables((prev) => {
      const pool = new Map<string, Wearable>([...catalogItemsRef.current, ...prev].map((w) => [w.urn, w]))
      return urns
        .map((u): Wearable | null => {
          const w = pool.get(u) ?? [...pool.values()].find((x) => u.startsWith(`${x.urn}:`))
          return w != null ? { ...w, equipped: true } : null
        })
        .filter((w): w is Wearable => w != null)
    })
  }, [])
  const equipWearables = useCallback((urns: string[]) => {
    driverRef.current?.send({ kind: 'equip', urns })
    applyEquippedOptimistic(urns)
  }, [applyEquippedOptimistic])
  const retrySave = useCallback(() => {
    setSaveError(null)
    driverRef.current?.send({ kind: 'commitAvatar' })
  }, [])
  // The bridge swaps its look back straight away, so re-reading the equipped sets picks it up.
  const revertSave = useCallback(() => {
    setSaveError(null)
    driverRef.current?.send({ kind: 'revertAvatar' })
    send('getWearables')
    send('getEmotes')
  }, [send])
  // Closing the Backpack (its own toggle, or another page opening) deploys its look in one go, as
  // Unity does; the bridge skips it when nothing changed.
  // The randomize history only covers one Backpack session. It resets first, so a roll still loading
  // drops its result instead of editing the look after the commit captured it.
  const backpackWasOpen = useRef(false)
  useEffect(() => {
    if (backpackWasOpen.current && !backpackOpen) {
      driverRef.current?.send({ kind: 'resetOutfitHistory' })
      driverRef.current?.send({ kind: 'commitAvatar' })
      setOutfitHistory(NO_OUTFIT_HISTORY)
    }
    backpackWasOpen.current = backpackOpen
  }, [backpackOpen])
  const setAvatarColor = useCallback((target: AvatarColorTarget, hex: string) => {
    setAvatarColors((prev) => ({ ...prev, [target]: hex }))
    driverRef.current?.send({ kind: 'setAvatarColor', target, color: hexToColor3(hex) })
  }, [])
  const previewWearables = useCallback((urns: string[] | null) => {
    driverRef.current?.send({ kind: 'previewAvatar', urns })
  }, [])
  const focusPreview = useCallback((focus: PreviewFocus) => {
    driverRef.current?.send({ kind: 'previewFocus', focus })
  }, [])
  const stagePreview = useCallback((rect: { x: number; y: number; width: number; height: number } | null) => {
    driverRef.current?.send({ kind: 'previewStage', rect })
  }, [])
  const emotePreview = useCallback((urn: string | null) => {
    driverRef.current?.send({ kind: 'previewEmote', urn })
  }, [])
  const rotatePreview = useCallback((degrees: number) => {
    driverRef.current?.send({ kind: 'previewRotate', degrees })
  }, [])
  const zoomPreview = useCallback((zoom: number, anchor: number) => {
    driverRef.current?.send({ kind: 'previewZoom', zoom, anchor })
  }, [])
  const setForceRender = useCallback((categories: string[]) => {
    setForceRenderState(categories)
    driverRef.current?.send({ kind: 'setForceRender', categories })
  }, [])
  const queryCatalog = useCallback((q: CatalogQuery) => {
    catalogReqId.current += 1
    setCatalogLoading(true)
    driverRef.current?.send({ kind: 'catalogQuery', catalog: 'wearables', requestId: catalogReqId.current, ...q })
  }, [])
  const saveOutfit = useCallback((slot: number) => {
    driverRef.current?.send({ kind: 'saveOutfit', slot })
  }, [])
  const deleteOutfit = useCallback((slot: number) => {
    driverRef.current?.send({ kind: 'deleteOutfit', slot })
  }, [])
  const equipOutfit = useCallback((slot: number) => {
    // The bridge resolves the outfit's wearables by urn and re-emits the full `wearables` set (incl.
    // items off the loaded catalog page), which becomes the authoritative equipped set — so no
    // client-side rebuild here (that dropped off-page items and later un-equipped them).
    driverRef.current?.send({ kind: 'equipOutfit', slot })
  }, [])
  // The bridge re-emits the full `wearables` set after each step, like equipOutfit.
  const randomizeOutfit = useCallback(() => {
    driverRef.current?.send({ kind: 'randomizeOutfit' })
  }, [])
  const undoOutfit = useCallback(() => {
    driverRef.current?.send({ kind: 'undoOutfit' })
  }, [])
  const redoOutfit = useCallback(() => {
    driverRef.current?.send({ kind: 'redoOutfit' })
  }, [])
  const createCommunity = useCallback((input: CreateCommunityInput) => {
    communitySaveInFlight.current = true
    pendingCommunityPicture.current = null
    setCommunitySave({ status: 'saving' })
    driverRef.current?.send({ kind: 'createCommunity', ...input })
  }, [])
  const updateCommunity = useCallback((input: UpdateCommunityInput) => {
    communitySaveInFlight.current = true
    pendingCommunityPicture.current = input.thumbnail != null ? { id: input.id, picture: input.thumbnail } : null
    setCommunitySave({ status: 'saving' })
    driverRef.current?.send({ kind: 'updateCommunity', ...input })
  }, [])
  const clearCommunitySave = useCallback(() => {
    communitySaveInFlight.current = false
    pendingCommunityPicture.current = null
    setCommunitySave(null)
  }, [])
  const loadCommunityPlaceOptions = useCallback((communityId?: string) => {
    placeOptionsWanted.current = communityId ?? ''
    setCommunityPlaceOptions(null)
    driverRef.current?.send({ kind: 'getCommunityPlaceOptions', ...(communityId != null ? { communityId } : {}) })
  }, [])
  const clearCommunityPlaceOptions = useCallback(() => {
    placeOptionsWanted.current = null
    setCommunityPlaceOptions(null)
  }, [])
  const deleteCommunity = useCallback((id: string) => {
    setCommunityError(null)
    driverRef.current?.send({ kind: 'deleteCommunity', id })
  }, [])
  const removeCommunityPlace = useCallback((id: string, placeId: string) => {
    setCommunityError(null)
    driverRef.current?.send({ kind: 'removeCommunityPlace', id, placeId })
  }, [])
  const communityMemberAction = useCallback((id: string, address: string, action: CommunityMemberAction) => {
    setCommunityError(null)
    driverRef.current?.send({ kind: 'communityMemberAction', id, address, action })
  }, [])
  const joinCommunity = useCallback((id: string) => {
    setCommunityError(null)
    driverRef.current?.send({ kind: 'joinCommunity', id })
  }, [])
  const requestToJoinCommunity = useCallback((id: string) => {
    setCommunityError(null)
    driverRef.current?.send({ kind: 'requestToJoinCommunity', id })
  }, [])
  const cancelJoinRequest = useCallback((id: string, requestId: string) => {
    setCommunityError(null)
    driverRef.current?.send({ kind: 'cancelJoinRequest', id, requestId })
  }, [])
  const leaveCommunity = useCallback((id: string) => {
    setCommunityError(null)
    driverRef.current?.send({ kind: 'leaveCommunity', id })
  }, [])
  const loadCommunityInvites = useCallback(() => {
    driverRef.current?.send({ kind: 'getCommunityInvites' })
  }, [])
  const loadCommunityInvitesCount = useCallback(() => {
    driverRef.current?.send({ kind: 'getCommunityInvitesCount' })
  }, [])
  const manageCommunityRequest = useCallback((id: string, requestId: string, intention: 'accepted' | 'rejected' | 'cancelled', address?: string) => {
    // one answer per request: a second click while the first runs would answer a request the
    // first already settled, and toast an error after the success. Released when the bridge says
    // this request settled.
    if (answering.current.has(requestId)) return
    answering.current.add(requestId)
    driverRef.current?.send({ kind: 'manageCommunityRequest', id, requestId, intention, ...(address != null ? { address } : {}) })
  }, [])
  const setOpenCard = useCallback((id: string | null) => {
    openCard.current = id
  }, [])
  const loadMemberSection = useCallback((id: string, section: CommunityMemberSection) => {
    driverRef.current?.send({ kind: 'getCommunityMemberSection', id, section })
  }, [])
  const loadInvitable = useCallback((address: string) => {
    invitableWanted.current = address
    setInvitable(null)
    driverRef.current?.send({ kind: 'getInvitableCommunities', address })
  }, [])
  const unbanCommunityMember = useCallback((id: string, address: string) => {
    driverRef.current?.send({ kind: 'unbanCommunityMember', id, address })
  }, [])
  const inviteToCommunity = useCallback((id: string, address: string) => {
    driverRef.current?.send({ kind: 'inviteToCommunity', id, address })
  }, [])
  const loadCommunityDetail = useCallback((id: string) => {
    if (detailWanted.current !== id) setMemberSections({})
    detailWanted.current = id
    setCommunityDetail(null) // clear stale detail while the new one loads
    driverRef.current?.send({ kind: 'getCommunityDetail', id })
  }, [])
  const playEmote = useCallback((urn: string) => {
    driverRef.current?.send({ kind: 'triggerEmote', urn })
    setEmotesOpen(false)
  }, [])
  // Assign an owned emote to a wheel slot (urn:'' clears it) on the Backpack's look, reflecting the
  // slot move here straight away (nothing deploys until the Backpack closes).
  const equipEmote = useCallback((slot: number, urn: string) => {
    driverRef.current?.send({ kind: 'equipEmote', slot, urn })
    setEmotes((list) =>
      list.map((e) => {
        if (e.urn === urn) return { ...e, slot } // the newly assigned one
        if (e.slot === slot) return { ...e, slot: undefined } // whatever was in that slot
        return e
      })
    )
  }, [])
  const voiceChatAct = useCallback((action: VoiceChatActionWire) => {
    driverRef.current?.send({ kind: 'voiceChatAction', action })
  }, [])
  const sceneVoiceChat = useMemo(
    () => (scenePlace == null ? null : streamHere(activeVoiceChats, scenePlace.parcel, scenePlace.realm)),
    [activeVoiceChats, scenePlace]
  )
  const toggleMic = useCallback(() => {
    setMic((m) => {
      driverRef.current?.send({ kind: 'setMic', enabled: !m.enabled })
      return { ...m, enabled: !m.enabled } // optimistic; relay confirms
    })
  }, [])
  const setNearbyMute = useCallback((address: string, muted: boolean) => {
    const wallet_id = address.toLowerCase()
    if (nearbyMutedRef.current.has(wallet_id) === muted) return
    // optimistic; the engine's list confirms or puts it back
    const next = new Set(nearbyMutedRef.current)
    if (muted) next.add(wallet_id)
    else next.delete(wallet_id)
    applyNearbyMuted(next)
    track('nearby_voice_user_mute', { wallet_id, is_muted: muted })
    driverRef.current?.send({ kind: 'setNearbyMute', address: wallet_id, muted })
  }, [])
  const markNotificationsRead = useCallback(() => {
    const unreadIds = notifications.filter((n) => !n.read).map((n) => n.id)
    if (unreadIds.length === 0) return
    // Persist to the notifications service so it survives a re-fetch on reopen.
    driverRef.current?.send({ kind: 'markNotificationsRead', ids: unreadIds })
    setNotifications((prev) => prev.map((n) => (n.read ? n : { ...n, read: true })))
  }, [notifications])
  const requestOwnedNames = useCallback(() => {
    driverRef.current?.send({ kind: 'getOwnedNames' })
  }, [])
  // Save an edit to YOUR OWN profile. Applied locally straight away — the engine only acks once it
  // has deployed the new version, and the catalyst reindexes later still, so waiting for the round
  // trip would leave the passport showing the old profile for seconds after a save. `profileSaved`
  // either confirms it or hands back the pre-save state (see the message handler).
  startProfileSaveRef.current = (edit: ProfileEdit, quiet: boolean): void => {
    const current = profileRef.current
    const address = current?.address.toLowerCase() ?? ''
    profileRevertRef.current = { address, profile: current, stored: peekProfile(address), quiet }
    profileInFlightRef.current = true
    if (!quiet) {
      setProfileSaveError(null)
      setProfileSaving(true)
    }
    driverRef.current?.send({ kind: 'saveProfile', ...edit })
    const next = current != null ? applyProfileEdit(current, edit, ownedNamesRef.current) : current
    // A save chained straight after this one must snapshot the edited profile, not the stale render.
    profileRef.current = next
    setProfile(next)
    updateProfile(address, (mine) => (mine != null ? applyProfileEdit(mine, edit, ownedNamesRef.current) : mine))
  }
  const queueOrStartProfileSave = useCallback((edit: ProfileEdit, quiet: boolean) => {
    if (!profileInFlightRef.current) {
      startProfileSaveRef.current(edit, quiet)
      return
    }
    const waiting = queuedProfileEditRef.current
    queuedProfileEditRef.current = { edit: { ...waiting?.edit, ...edit }, quiet: (waiting?.quiet ?? true) && quiet }
    if (!quiet) {
      setProfileSaveError(null)
      setProfileSaving(true)
    }
  }, [])
  const saveProfile = useCallback((edit: ProfileEdit) => queueOrStartProfileSave(edit, false), [queueOrStartProfileSave])
  // The passport's name colour, deployed as it closes. Quiet: a failure only puts the old colour back.
  const saveNameColor = useCallback(
    (nameColor: NonNullable<ProfileEdit['nameColor']>) => queueOrStartProfileSave({ nameColor }, true),
    [queueOrStartProfileSave]
  )
  const dismissProfileSaveError = useCallback(() => setProfileSaveError(null), [])
  const friendAct = useCallback((op: FriendAction, address: string, message?: string) => {
    setFriendPending((prev) => new Set(prev).add(`${op}:${address.toLowerCase()}`))
    // the social events mobile sends on the click, whatever the outcome
    const receiver_id = address.toLowerCase()
    if (op === 'request') track('Friend Request', { receiver_id })
    else if (op === 'accept') track('Friend Accept', { receiver_id })
    else if (op === 'delete') track('Unfriend', { receiver_id })
    else if (op === 'block') track('Block User', { receiver_id, is_friend: friendsData.friends.some((f) => f.address.toLowerCase() === receiver_id) })
    driverRef.current?.send({ kind: 'friendAction', op, address, message })
  }, [friendsData.friends])
  const loadMutualFriends = useCallback((address: string) => {
    driverRef.current?.send({ kind: 'getMutualFriends', address })
  }, [])
  const settingSet = useCallback((name: string, value: number) => {
    driverRef.current?.send({ kind: 'setSetting', name, value })
  }, [])
  const bindingsSet = useCallback((entries: BindingEntry[]) => {
    driverRef.current?.send({ kind: 'setBindings', bindings: entries })
  }, [])
  const bindingsReset = useCallback(() => {
    driverRef.current?.send({ kind: 'resetBindings' })
  }, [])
  const captureBinding = useCallback(() => {
    const id = `capture-${++captureSeq.current}`
    const input = new Promise<string>((resolve) => {
      captureRef.current = { id, resolve }
    })
    driverRef.current?.send({ kind: 'captureInput', id })
    return {
      input,
      cancel: () => {
        if (captureRef.current?.id === id) captureRef.current = null
      }
    }
  }, [])

  const nav = useCallback((action: NavAction) => {
    // Opening another panel closes the React panels (single active panel).
    closeAllPanels()
    driverRef.current?.send({ kind: 'navAction', action })
  }, [closeAllPanels])

  const setEditorMode = useCallback((mode: EditorHudMode) => {
    if (mode !== 'off') panelSetters.forEach((set) => set(false))
    setEditorModeState(mode)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const logout = useCallback(() => {
    signOuts.current += 1
    driverRef.current?.logout().catch((e: Error) => console.error('[session] logout failed', e))
    clearLogins() // drop the same-domain SSO identity for this origin
    setStored(null)
    // the refs at once too: a /hud_login in the same task sees the sign-out
    storedRef.current = null
    pendingLoginType.current = null
    lastLoginType.current = null
    welcomeRef.current = null
    guestPick.current = null
    setQueuedGuest(null)
    setPendingLook(null)
    setStatus('sign-in-or-guest')
    closeAllPanels()
    setPlayerReady(false)
    setSubmitted(false) // back to the login screen
    submittedRef.current = false
    setDestinationPicked(false) // re-show the picker on the next jump-in
    setLobby(false)
    setLobbyOpen(false)
    setLobbyStageReady(false)
    setWelcome(null)
    setWelcomePending(false)
    setWelcomeSaving(false)
    setWelcomeError(null)
    welcomeLink.current = null
    heldDest.current = undefined
    visitedTitle.current = ''
    pendingLogin.current = null
    // nothing to put back: a /hud_login's sign-in the engine still holds goes with the rest
    hudStored.current = null
    settleHudLogin(new Error('signed out'))
    // The next account starts clean: it fetches its own data and hasn't spawned yet.
    fetchedRef.current.clear()
    playerReadyRef.current = false
    pendingParcel.current = null
    // Nothing of this account's may show under the next one, or be saved onto it: an unsaved
    // Backpack look is dropped rather than committed after the engine has let the account go.
    backpackWasOpen.current = false
    profileRevertRef.current = null
    profileInFlightRef.current = false
    queuedProfileEditRef.current = null
    setOwnProfile(null)
    setProfile(null)
    setPrevUserId(null)
    setOwnedNames([])
    setProfileSaving(false)
    setProfileSaveError(null)
    setSaveError(null)
    setEquippedWearables([])
    setBodyShape(undefined)
    setForceRenderState([])
    setAvatarColors(undefined)
    setNotifications([])
    clearNotificationToasts()
    clearRewardPopups()
    // the engine sends its state again after an account change
    setVoiceChatState(null)
    // the live streams are per account (`isMember`)
    setActiveVoiceChats([])
    setActiveVoiceChatsLoaded(false)
    setCommunities([])
    setCommunityCards({})
    setCommunityInvites(null)
    setPendingCount(null)
    answering.current.clear()
    setMemberSections({})
    setInvitable(null)
    communityBrowseWanted.current = null
    communityBrowsePage.current = 0
    updateCommunityBrowse(() => NO_BROWSE)
    clearCommunitySave()
    clearCommunityPlaceOptions()
    setDeletedCommunity(null)
    savedCommunityPictures.current = {}
    setFriendPending(new Set())
    setMutualFriends({})
    setMessages(initialMessages)
    setChannel('Nearby')
    openTabsRef.current.clear()
    setConversations([])
    setCommunityConvos([])
    setClosedCommunities(new Set())
    setDmLines({})
    setChatUnread(0)
    setNearbyUnread(0)
    setPortables([])
    portableStates.current = null
    portableToasts.current = false
  }, [closeAllPanels])

  // Where the Backpack's avatar shows through, so the lobby under its modal can open the same hole.
  const [avatarPreviewRect, setAvatarPreviewRect] = useState<{ x: number; y: number; width: number; height: number } | null>(null)
  const setEngineViewport = useCallback(
    (region: 'map' | 'avatarPreview' | 'lobby', rect: { x: number; y: number; width: number; height: number } | null, dpr?: number) => {
      if (region === 'avatarPreview') setAvatarPreviewRect(rect)
      driverRef.current?.send({ kind: 'engineViewport', region, rect, dpr })
    },
    []
  )

  const setInteractableArea = useCallback((area: InteractableArea) => {
    driverRef.current?.send({ kind: 'interactableArea', ...area })
  }, [])

  // engine_run briefly blocks the main thread, so the lobby (or the loader) paints first and the
  // launch runs behind it (the guest start page's early launch, above, is the one that runs under
  // an interactive screen). On failure, back to sign-in.
  const submitLogin = useCallback(
    (loginCall: (driver: LoginDriver) => Promise<unknown>, loginType: LoginType | 'stored' | null): boolean => {
      const driver = driverRef.current
      if (driver == null || busy || !engineReady) return false
      setError(null)
      pendingLogin.current = loginCall
      pendingLoginType.current = loginType
      setSubmitted(true)
      // at once, not on the next render: a /hud_login in the same task is refused as signed in
      submittedRef.current = true
      // A ?position/?realm link launches straight there (pickDestination, from the url effect).
      if (urlDestination.current != null) return true
      // Otherwise the lobby: launch now holding the world back, so signing in, the avatar and
      // friends work but no scene loads until a destination is picked (which releases it).
      setLobby(true)
      setWelcomePending(welcomeShown())
      // an engine launched for the guest start page keeps its stage, avatar and all, into the lobby
      if (!launchedRef.current) setLobbyStageReady(false)
      setBusy(true)
      busyRef.current = true
      const signOut = signOuts.current
      let ran = false
      const run = (): void => {
        if (ran || driverRef.current == null) return
        ran = true
        // signed out before the deferred run: the sign-in never runs
        if (signOuts.current !== signOut) return setBusy(false)
        if (driver.launch != null && !launchedRef.current) {
          const home = driver.homeScene?.()
          if (!launchEngine(driver, home?.realm ?? DEFAULT_REALM, home?.parcel ?? '0,0', { holdWorld: true })) {
            setWelcomePending(false) // no sign-in to answer it
            // a /hud_login's sign-in never ran: answered with that, its stored sign-in put back
            restoreHudSignIn()
            settleHudLogin(new Error('the engine failed to launch'))
            return
          }
        }
        runPendingLogin(driver)
      }
      // Defer a paint so the lobby is on screen before engine_run's main-thread work.
      requestAnimationFrame(() => requestAnimationFrame(run))
      setTimeout(run, 60)
      return true
    },
    [busy, engineReady, launchEngine, runPendingLogin, settleHudLogin, restoreHudSignIn]
  )

  // A test runner's sign-in (/hud_login, lib/hudAutomation.ts): the identity becomes this page's
  // stored sign-in, as the auth site's return leaves one, and goes in the way Jump In takes a stored
  // one, so the HUD leaves its start page for the lobby. The sign-in stands for the terms the
  // welcome page asks for, and the pick names and dresses a first login's profile; the destination
  // is picked from the lobby once it takes picks (the effect after the phase). Answered once the
  // sign-in has landed and the page is done, or failed; the stored sign-in goes back to what it
  // was if the engine refuses the sign-in or never runs it.
  const hudLogin = useCallback(
    (request: HudLoginRequest): Promise<HudLoginReply> =>
      new Promise((resolve, reject) => {
        // the refs, as the command may come between a render and the effect that hands it over
        if (hudLoginPending.current != null) return reject(new Error('a /hud_login is already under way: /hud_logout ends it'))
        if (fatalErrorRef.current != null) return reject(new Error(fatalErrorRef.current.message))
        if (submittedRef.current) return reject(new Error('already signed in: /hud_logout first'))
        // a sign-out under a sign-in the engine has not answered yet: busy until it does
        if (busyRef.current) return reject(new Error('a sign-in is still finishing'))
        if (!engineReady) return reject(new Error('the engine is not ready to take a sign-in yet'))
        const { identity, guest, defaultOnError, pick, destination } = request
        const login: StoredLogin = { address: rootAddress(identity).toLowerCase(), identity, guest }
        hudStored.current = { replaced: storedRef.current, landed: false }
        setStored(login)
        storedRef.current = login
        guestPick.current = pick ?? null
        hudDestination.current = hudDestinationOf(destination)
        // the runner's sign-in stands in for a stored account's own: no Jump In with that one
        // once its profile check ends, should this sign-in fail
        autoEntered.current = true
        hudLoginPending.current = { resolve, reject, login, landed: false, welcomeAwaited: false }
        if (!submitLogin((d) => d.loginWithIdentity(identity, defaultOnError, guest, pick), 'stored')) {
          guestPick.current = null
          restoreHudSignIn()
          settleHudLogin(new Error('the sign-in could not start: another is under way'))
        }
      }),
    [engineReady, submitLogin, settleHudLogin, restoreHudSignIn]
  )
  // handed over once, through a ref to the latest callback (and so the latest `submitLogin`): a
  // command between a render and an effect that would hand a new one over sees the current state
  const hudLoginRef = useRef(hudLogin)
  hudLoginRef.current = hudLogin
  // and the sign-out a runner's `/hud_logout` takes, the HUD's own, which the engine's `/logout`
  // never reaches
  const logoutRef = useRef(logout)
  logoutRef.current = logout
  useEffect(() => {
    provideHudLogin((request) => hudLoginRef.current(request))
    provideHudLogout(() => logoutRef.current())
    return () => {
      provideHudLogin(null)
      provideHudLogout(null)
    }
  }, [])
  // a sign-in the HUD went away under is answered with that, not left to the runner's timeout
  useEffect(() => () => settleHudLogin(new Error('the HUD unmounted')), [settleHudLogin])

  // the curated default look on the stage, so RANDOMIZE never repeats it
  const shownLook = useRef(0)
  const showLook = useCallback((look: number, avatar: AvatarLook) => {
    shownLook.current = look
    setPendingLook(look)
    driverRef.current?.send({ kind: 'showLook', look, avatar })
  }, [])
  // A look is pending from the moment it is asked for, so RANDOMIZE can't be hit again while its
  // profile is fetched; a fetch that never comes back lets go after a while (timed from the last
  // ask: an earlier ask's timer must not let a later one go).
  const expectTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const expectLook = useCallback(() => {
    setPendingLook(EXPECTED_LOOK)
    driverRef.current?.send({ kind: 'expectLook' })
    if (expectTimer.current != null) clearTimeout(expectTimer.current)
    expectTimer.current = setTimeout(() => setPendingLook((pending) => (pending === EXPECTED_LOOK ? null : pending)), EXPECTED_LOOK_MS)
  }, [])
  const preloadLook = useCallback((look: number, avatar: AvatarLook) => driverRef.current?.send({ kind: 'preloadLook', look, avatar }), [])
  // RANDOMIZE: the page picks and fetches the look (the scene's fetches are signed, and it has no
  // say in the pick), and the stage wears it; a later pick outruns a slow fetch
  const rerollLook = useCallback(() => {
    clickButton('NAME_RANDOMIZE', 'AVATAR_NAMING')
    expectLook()
    const look = randomDefaultLook(shownLook.current || undefined)
    shownLook.current = look
    void fetchStoredProfile(`default${look}`).then((profile) => {
      if (profile?.look != null && shownLook.current === look) showLook(look, profile.look)
    })
  }, [showLook, expectLook])
  const openLegal = useCallback((doc: 'terms' | 'privacy') => driverRef.current?.send({ kind: 'openLegal', doc }), [])
  const acceptWelcome = useCallback((name?: string) => {
    clickButton('LETS_GO', welcomeRef.current?.newProfile ? 'AVATAR_NAMING' : 'WELCOME')
    setWelcomeSaving(true)
    setWelcomeError(null)
    driverRef.current?.send({ kind: 'acceptWelcome', name })
  }, [])

  // The button creates a guest account that persists; ?guest=1 stays a throwaway guest.
  const startGuest = useCallback(
    (pick?: GuestPick) => {
      const persistent = driverRef.current?.loginPersistentGuest != null
      guestPick.current = pick ?? null
      if (!submitLogin((d) => d.loginPersistentGuest?.(pick) ?? d.loginGuest(), 'guest')) guestPick.current = null
      else if (persistent) screenViewed('ACCOUNT_GUEST_CREATE')
    },
    [submitLogin]
  )
  const exploreAsGuest = useCallback(
    (pick?: GuestPick) => {
      if (pick != null) clickButton('LETS_GO', 'AVATAR_NAMING')
      else clickButton('PLAY_GUEST', 'ACCOUNT_HOME')
      // before the engine is ready the click is kept, and runs once it is (the effect below)
      if (!engineReady && driverRef.current != null) {
        setQueuedGuest({ pick: pick ?? null })
        return
      }
      startGuest(pick)
    },
    [engineReady, startGuest]
  )
  useEffect(() => {
    if (queuedGuest == null || !engineReady) return
    setQueuedGuest(null)
    startGuest(queuedGuest.pick ?? undefined)
  }, [queuedGuest, engineReady, startGuest])
  const autoGuest = useCallback(() => submitLogin((d) => d.loginGuest(), 'guest'), [submitLogin])
  // An unattended run signs in with its own identity (the host's `unattendedRun`), never a stored account
  const unattendedLogin = useCallback(
    () => submitLogin((d) => unattendedRun()?.login(d) ?? d.loginGuest(), 'guest'),
    [submitLogin]
  )
  // Reuse the existing login. The driver picks the path its backend supports (console
  // `/login_identity` for the engine, `loginPrevious` over the bridge).
  const enterStored = useCallback((defaultOnError?: boolean) => submitLogin((d) => d.jumpIn(defaultOnError), 'stored'), [submitLogin])
  const jumpIn = useCallback(() => {
    clickButton('lets_go', 'COMEBACK')
    enterStored()
  }, [enterStored])
  // A stored session goes straight to the lobby, once per page: the lobby's profile menu is where
  // to sign out or switch accounts.
  const autoEntered = useRef(false)
  useEffect(() => {
    if (status !== 'reuse-login-or-new' || submitted || !engineReady || autoEntered.current) return
    if (accountChecking) return
    autoEntered.current = true
    enterStored()
  }, [status, submitted, engineReady, enterStored, accountChecking])
  // The engine tags profile-fetch login failures (vs bad credentials etc.) with this marker —
  // mirrors PROFILE_FETCH_FAILED in system_bridge. Only then do we offer the destructive reset.
  const profileFetchFailed = error != null && error.includes('profile fetch failed')
  // Explicit recovery from an unfetchable/corrupt profile: retry, continuing with a default
  // profile if it still fails. The engine deploys that default, permanently replacing the
  // account's server-side profile — the button copy must carry that warning.
  const resetProfileAndJumpIn = useCallback(() => {
    clickButton('try_again', 'AUTH_ERROR')
    enterStored(true)
  }, [enterStored])

  // Fresh sign-in (or signing in with a different account). Web: bounce to the same-domain
  // auth site, which writes the identity back to this origin's localStorage and redirects
  // here. Native (the driver has loginNew): that redirect would resolve against cef:// and
  // 404 into the asset server — instead run the engine's remote-wallet flow, which opens the
  // auth site in the user's EXTERNAL browser and streams back a verification code to show.
  // The auth runs now (not deferred like Jump in — it needs the user at the code panel);
  // approval means the engine is already logged in, so the destination pick has no deferred
  // login left to run.
  const startWithAccount = useCallback(() => {
    if (busy) return
    clickButton('SIGN_IN', 'AVATAR_NAMING')
    guestPick.current = null
    const driver = driverRef.current
    if (driver?.loginNew == null) {
      screenViewed('AUTH_BROWSER_OPEN', { method: 'sso' })
      redirectToAuth()
      return
    }
    const attempt = ++authAttempt.current
    setError(null)
    setBusy(true)
    setAuth({ code: null })
    driver
      .loginNew()
      .then(() => {
        if (attempt !== authAttempt.current) return // cancelled meanwhile
        setAuth(null)
        setBusy(false)
        pendingLogin.current = null
        setSubmitted(true)
        // at once: the welcome page's answer may come before the next render
        submittedRef.current = true
        welcomeLink.current = urlDestination.current
        if (welcomeShown() && driverRef.current != null) askWelcome(driverRef.current)
        // the engine booted holding the world, as for Jump in: the pick is the lobby's
        if (urlDestination.current == null) {
          setLobby(true)
          setLobbyStageReady(false)
          setWelcomePending(welcomeShown())
        }
      })
      .catch((e: unknown) => {
        if (attempt !== authAttempt.current) return // cancelled: the rejection is expected
        setAuth(null)
        setBusy(false)
        const msg = e instanceof Error ? e.message : String(e)
        setError(msg !== '' ? msg : 'Sign-in failed')
      })
  }, [busy])

  // Abort an in-flight fresh sign-in: drop the engine's login task and invalidate the pending
  // loginNew promise (it settles late — as cancelled from the relay or rejected — and is ignored).
  const cancelLogin = useCallback(() => {
    authAttempt.current++
    setAuth(null)
    setBusy(false)
    driverRef.current?.loginCancel().catch(() => {})
  }, [])

  // "Use a different account" shows the sign-in/guest screen (Start with account + Explore as
  // guest) rather than jumping straight to auth — matching the reference scene, and the only way a
  // returning user can reach Explore as Guest.
  const useDifferentAccount = useCallback(() => {
    if (busy) return
    clickButton('use_another_account', 'COMEBACK')
    setStatus('sign-in-or-guest')
  }, [busy])

  // Auto-login (see lib/bootMode.ts): skip the sign-in screen the moment the engine is warm —
  // as a guest (?guest=1, the sites `/discover` embed), or with no React login at all
  // (?systemScene=: the substituted ui scene owns login in-engine, pre-react behavior), or, for a
  // host's unattended run, with the run's own identity and nothing else; a sign-in that fails ends
  // the run (runPendingLogin). The destination is auto-picked by the effect above
  // (?position / ?realm, else the hidden-HUD Genesis fallback), so this goes straight into the
  // scene. Fires once — `submitted` flips true on the first call and gates re-entry.
  useEffect(() => {
    if (boot.current.autoLogin == null || !engineReady || submitted) return
    if (boot.current.autoLogin === 'guest') autoGuest()
    // scene-owned: no React login at all — just boot the engine (a no-op deferred login).
    else if (boot.current.autoLogin === 'scene-owned') submitLogin(() => Promise.resolve(), null)
    else unattendedLogin()
  }, [engineReady, submitted, autoGuest, unattendedLogin, submitLogin])

  // Render-settle. When the scene flips from loading → loaded (visible true→false), hold the loader
  // a beat longer while the engine actually renders the world (compiling shaders / uploading
  // textures) — otherwise it's revealed as black models. Gated on the engine's `renderBusy` probe
  // and capped by MAX_REVEAL_MS so it never hangs. Mock has no engine to wait for (no renderBusy) →
  // no delay. Applies to every scene stream, not just the first, so crossings don't flash black.
  const [revealing, setRevealing] = useState(false)
  const prevSceneVisible = useRef<boolean | undefined>(undefined)
  useEffect(() => {
    const visible = sceneLoading?.visible
    const justLoaded = prevSceneVisible.current === true && visible === false
    prevSceneVisible.current = visible
    if (!justLoaded || typeof driverRef.current?.renderBusy !== 'function') return
    setRevealing(true)
    const startedAt = performance.now()
    let timer: ReturnType<typeof setTimeout>
    const tick = (): void => {
      const elapsed = performance.now() - startedAt
      const stillRendering = driverRef.current?.renderBusy?.() === true
      if ((elapsed >= MIN_REVEAL_MS && !stillRendering) || elapsed >= MAX_REVEAL_MS) {
        setRevealing(false)
        return
      }
      timer = setTimeout(tick, 120)
    }
    timer = setTimeout(tick, MIN_REVEAL_MS)
    return () => clearTimeout(timer)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sceneLoading?.visible])

  // Mirror the engine's native loading screen (SDK7 SceneLoadingWindow: `if (!visible) return null`).
  // The engine keeps its loader visible until the player's scene is rendered, and flips it back on
  // for each scene streamed into Genesis Plaza. We debounce the *reveal* (loading→world) so a brief
  // `visible` gap between scenes doesn't flash the HUD; the loader still appears INSTANTLY whenever
  // loading re-asserts. Loading = a HUD travel pending, or scene visible, or not spawned, or the
  // render-settle still holding.
  // No state received yet (sceneLoading == null) counts as loading: the loading stream is the
  // bridge-scene's domain, and until it's running and reports otherwise the world isn't ready
  // (on native the engine relay itself sends no loading state at all).
  const loadingNow = travellingTo != null || sceneLoading?.visible !== false || !playerReady || revealing
  const [loaderActive, setLoaderActive] = useState(true)
  useEffect(() => {
    if (loadingNow) {
      setLoaderActive(true)
      return
    }
    // No engine to stream scenes (mock / tests) → nothing to bridge, reveal immediately.
    if (typeof driverRef.current?.renderBusy !== 'function') {
      setLoaderActive(false)
      return
    }
    const t = setTimeout(() => setLoaderActive(false), REVEAL_DEBOUNCE_MS)
    return () => clearTimeout(t)
  }, [loadingNow])

  const loadingTracker = useRef(createLoadingProgress())
  if (!loaderActive) loadingTracker.current.reset()
  const loadingProgress = loaderActive
    ? loadingTracker.current.next({ scene: sceneLoading, playerReady, revealing, travelling: travellingTo != null })
    : 100

  const phase: SessionPhase = !submitted
    ? 'login'
    : welcome != null || (welcomePending && lobby)
      ? 'welcome'
      : !destinationPicked
      ? urlDestination.current != null
        ? 'entering' // a ?position/?realm launch is about to fire — never flash the picker
        : lobby
          ? 'lobby'
          : 'picking'
      : loaderActive
        ? 'entering'
        : 'world'

  const connectivity = useConnectivity(driverRef.current, phase === 'world' || phase === 'entering')
  // A /hud_login is answered once the engine took the sign-in and the HUD is past the welcome page
  // (still to come, shown, or its accept still saving): in the lobby, where its destination is
  // picked as a card's would be (or from the picker), or already entering a link's destination,
  // which wins over its own. On a link the phase is `entering` from the submit on and no lobby
  // waits for the page, so the sign-in's landing and the page are waited for apart.
  useEffect(() => {
    const pending = hudLoginPending.current
    if (pending == null || !pending.landed || pending.welcomeAwaited || phase === 'login' || phase === 'welcome' || welcomeSaving) return
    if (phase === 'lobby' && busy) return
    const destination = hudDestination.current
    // `entering` only when the pick went out: a pick that did nothing leaves the HUD where it is
    const picked = destination != null && (phase === 'lobby' || phase === 'picking') && pickDestination(destination)
    // the identity the command sent, not the page's stored sign-in as it may have moved on
    settleHudLogin({ address: pending.login.address, guest: pending.login.guest === true, phase: picked ? 'entering' : phase })
  }, [phase, busy, welcomeSaving, hudLoginTick, pickDestination, settleHudLogin])

  // A pending /hud_login is answered at once with the error (the launch and realm-check failures
  // have answered theirs by the time this runs). Only a failed launch leaves no engine to answer
  // the sign-in, so only then does its stored sign-in go back here: a boot panic the poll reports
  // after launch() returned (a launch that throws has put it back already). The bridge's dialog
  // leaves the world running, and the watchdog's stall may pass, so the engine can still land or
  // refuse the sign-in, and its answer decides (runPendingLogin).
  useEffect(() => {
    if (fatalError == null) return
    if (fatalError.source === 'launch') restoreHudSignIn()
    settleHudLogin(new Error(fatalError.message))
  }, [fatalError, settleHudLogin, restoreHudSignIn])

  // The screens a phase shows (the loading screen's own are the engine's: LOADING_START / END /
  // TIMEOUT come from `crates/analytics/src/loading.rs`).
  const prevPhase = useRef<SessionPhase | null>(null)
  useEffect(() => {
    const prev = prevPhase.current
    prevPhase.current = phase
    if (prev === phase) return
    if (phase === 'lobby') screenViewed('LOBBY')
    else if (phase === 'picking') screenViewed('PLACES_PICKER')
  }, [phase])
  // The sign-in screens as mobile names them, once per showing (an error flip on the same screen
  // is not a new view). The "welcome back" panel only shows after a failed sign-in: a stored login
  // heads for the lobby under the splash.
  const loginScreen = useRef<string | null>(null)
  useEffect(() => {
    const screen =
      phase !== 'login' || boot.current.hideHud
        ? null
        : status === 'sign-in-or-guest'
          ? 'AVATAR_NAMING'
          : status === 'reuse-login-or-new' && error != null
            ? 'COMEBACK'
            : null
    if (screen != null && screen !== loginScreen.current) screenViewed(screen)
    loginScreen.current = screen
  }, [phase, status, error])

  // Once launched the bridge scene must exist, so from here on its absence is a fault. Not
  // 'picking': nothing is launched behind the picker.
  useEffect(() => {
    if (phase === 'welcome' || phase === 'lobby' || phase === 'entering' || phase === 'world') driverRef.current?.expectBridge?.()
  }, [phase])

  // HUD focus, declared to the engine (fire-and-forget; latest wins). `ui` reserves all
  // input above scenes — the avatar stops walking when a menu/popup opens — while the
  // system-action stream keeps flowing so Cancel/hotkeys still arrive here. `text` reserves
  // the keyboard above the stream too: keys are typing, no actions resolve at all. Chat
  // alone is deliberately NOT ui — you can walk with the chat panel open.
  const popupOpen = useSyncExternalStore(subscribePopups, hasOpenPopup)
  const locked = useSyncExternalStore(subscribeInputLock, isInputLocked)
  const [textFocused, setTextFocused] = useState(false)
  useEffect(() => {
    const update = (): void => setTextFocused(isEditableTarget(document.activeElement))
    const onFocusOut = (): void => void setTimeout(update, 0) // activeElement settles next tick
    // The cancel key blurs a focused text field (fields are otherwise sticky: while one
    // holds focus no actions resolve, so cancel could never escape it). Bubble phase, so a
    // widget that owns the key — chat's layered Escape — can stopPropagation first.
    const onKey = (e: KeyboardEvent): void => {
      if (isCancelKey(e) && isEditableTarget(e.target)) (e.target as HTMLElement).blur()
    }
    document.addEventListener('focusin', update, true)
    document.addEventListener('focusout', onFocusOut, true)
    window.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('focusin', update, true)
      document.removeEventListener('focusout', onFocusOut, true)
      window.removeEventListener('keydown', onKey)
    }
  }, [])
  // `scroll` focus: the cursor sits over a scrollable HUD element, so the SCROLL gesture
  // belongs to that panel — the engine reserves the Scroll actions (every input bound to
  // them stands down for world consumers: camera zoom on a shared wheel, movement on a
  // shared key) while their edges keep streaming for the driver below. Tracked on
  // pointer crossings (pointerover, and pointermove for shadow roots), walking up for scrollable overflow.
  const [scrollHover, setScrollHover] = useState(false)
  const scrollElRef = useRef<Element | null>(null)
  const lastWheelAtRef = useRef(0)
  useEffect(() => {
    const scrollable = (el: Element): boolean => {
      const s = getComputedStyle(el)
      return (
        (/(auto|scroll)/.test(s.overflowY) && el.scrollHeight > el.clientHeight) ||
        (/(auto|scroll)/.test(s.overflowX) && el.scrollWidth > el.clientWidth)
      )
    }
    // from the composed path: inside a shadow root (the scene editor's UI) e.target is its host
    const findScrollable = (t: EventTarget | undefined): Element | null => {
      let el = t instanceof Element ? t : null
      while (el != null && el !== document.body) {
        if (scrollable(el)) return el
        const root = el.parentElement == null ? el.getRootNode() : null
        el = root instanceof ShadowRoot ? root.host : el.parentElement
      }
      return null
    }
    // a crossing inside a shadow root never reaches the window as pointerover
    let over: EventTarget | undefined
    const onOver = (e: PointerEvent): void => {
      const t = e.composedPath()[0]
      if (t === over) return
      over = t
      scrollElRef.current = findScrollable(t)
      setScrollHover(scrollElRef.current != null)
    }
    // Every wheel event is stamped so the scroll driver can tell a wheel-sourced Scroll
    // edge (the DOM already scrolled natively — the wheel directions are FIXED Scroll
    // bindings, engine-enforced) from a key/gamepad one.
    const onWheel = (): void => {
      lastWheelAtRef.current = performance.now()
    }
    window.addEventListener('pointerover', onOver)
    window.addEventListener('pointermove', onOver)
    window.addEventListener('wheel', onWheel, true)
    return () => {
      window.removeEventListener('pointerover', onOver)
      window.removeEventListener('pointermove', onOver)
      window.removeEventListener('wheel', onWheel, true)
    }
  }, [])
  // The scroll driver: Scroll press/release edges from the action stream scroll the hovered
  // panel at a steady rate while held — this is what makes panels scrollable from ANY
  // Scroll binding (gamepad shoulder buttons, keys), not just the wheel.
  const scrollDirsRef = useRef(new Set<string>())
  const scrollAnimRef = useRef<number | null>(null)
  const scrollTickAtRef = useRef(0)
  const HUD_SCROLL_SPEED = 800 // px/s
  const ensureScrollLoop = (): void => {
    if (scrollAnimRef.current != null || scrollDirsRef.current.size === 0) return
    scrollTickAtRef.current = performance.now()
    const tick = (): void => {
      const dirs = scrollDirsRef.current
      if (dirs.size === 0) {
        scrollAnimRef.current = null
        return
      }
      const now = performance.now()
      const d = (HUD_SCROLL_SPEED * (now - scrollTickAtRef.current)) / 1000
      scrollTickAtRef.current = now
      scrollElRef.current?.scrollBy({
        left: (dirs.has('Right') ? d : 0) - (dirs.has('Left') ? d : 0),
        top: (dirs.has('Down') ? d : 0) - (dirs.has('Up') ? d : 0)
      })
      scrollAnimRef.current = requestAnimationFrame(tick)
    }
    scrollAnimRef.current = requestAnimationFrame(tick)
  }
  useEffect(
    () => () => {
      if (scrollAnimRef.current != null) cancelAnimationFrame(scrollAnimRef.current)
    },
    []
  )
  // photo mode reserves all input like a page; the engine reads the camera's keys above that
  const photoModeActive = photoMode?.active ?? false
  const uiFocus = anyPanelOpen || popupOpen || locked || photoModeActive
  // a popup opening on top of photo mode ends it
  useEffect(() => {
    if (popupOpen && photoModeActive) setPhotoModeEnabled(false, 'button')
  }, [popupOpen, photoModeActive, setPhotoModeEnabled])
  // `covered` also spans the loading overlay: it outlives the engine's own out-of-world state
  // (player spawn, render-settle, reveal debounce), so the engine can't see that tail itself.
  const covered = menuPageOpen || phase === 'welcome' || phase === 'lobby' || phase === 'entering'
  // The open menu page, by the SystemAction that toggles it (the pages are exclusive, so at most
  // one is open). The engine answers a scene's openExplorerUi from this, and writes the page's
  // opened/closed events to the scene whose request opened it.
  const menu = settingsOpen ? 'Settings'
    : backpackOpen ? 'Backpack'
    : communitiesOpen ? 'Communities'
    : mapOpen ? 'Map'
    : placesOpen ? 'Places'
    : galleryOpen ? 'Gallery'
    : eventsOpen ? 'Events'
    : null
  useEffect(() => {
    if (phase !== 'world' && phase !== 'entering' && phase !== 'lobby' && phase !== 'welcome') return
    // The lobby and the welcome page hold input like a menu page: the player can't walk under them.
    const ui = uiFocus || phase === 'lobby' || phase === 'welcome'
    driverRef.current?.send({ kind: 'uiFocus', ui, text: textFocused, scroll: scrollHover, covered, menu })
  }, [phase, uiFocus, textFocused, scrollHover, covered, menu])

  // Pre-world the bridge stream doesn't exist, so popups opened during login/entering
  // (realm errors, world-visit prompts) need a DOM cancel fallback; in-world the engine's
  // 'Cancel' action owns closing (see the dispatcher below).
  useWindowKeyDown(
    (e) => {
      if (!isCancelKey(e) || isInputLocked() || !hasOpenPopup()) return
      e.preventDefault()
      e.stopPropagation()
      closeTopPopup()
    },
    { capture: true, enabled: phase !== 'world' && phase !== 'lobby' }
  )

  // Engine-authoritative hotkeys (the [M]/[Z]/… hints in the nav + sidebar): SystemAction
  // edges relayed from the engine's input stream via the bridge, so user rebinds apply and
  // keys fire regardless of where keyboard focus sits. The engine already resolves nothing
  // while `text` focus is declared; the DOM-focus guards stay as defence for the declaration
  // race (a keystroke landing before the uiFocus relay does).
  systemActionRef.current = (action, pressed) => {
    // Scroll edges (both edges — held scrolls continuously) drive the hovered panel via
    // the driver above. Wheel-sourced edges are skipped: the DOM scrolled natively on the
    // wheel event itself, and acting on its echo would double-scroll.
    const scrollDir = /^Scroll(Up|Down|Left|Right)$/.exec(action)
    if (scrollDir != null) {
      if (pressed && performance.now() - lastWheelAtRef.current < 250) return
      if (pressed) scrollDirsRef.current.add(scrollDir[1])
      else scrollDirsRef.current.delete(scrollDir[1])
      ensureScrollLoop()
      return
    }
    if (!pressed || (phase !== 'world' && phase !== 'lobby') || isInputLocked()) return
    if (isEditableTarget(document.activeElement)) return
    if ((window as EngineFocusWindow).__engineTextFocus) return
    if (EDITOR_BUILD && !hudActsOn(editorMode, action)) return
    // 'Cancel' is the one action handled even with a popup open: the engine resolved the
    // cancel key or gamepad button, and this is the single layered close — the most recent of
    // the topmost popup and the topmost registered leaf layer (lightbox, open dropdown, a menu
    // inside a popup — see cancelLayers) first, else any open panels. One layer per press.
    if (action === 'Cancel') {
      if (dispatchCancelLayer(topPopupOrder())) return
      if (hasOpenPopup()) closeTopPopup()
      // the lobby sits under the panels it opens: close those first, then the lobby
      else if (anyPanelOpenExceptLobby) panelSetters.forEach((set) => set(false))
      else setLobbyOpen(false)
      return
    }
    // the startup lobby mounts only what it opens itself; the world's hotkeys wait for the world
    if (phase === 'lobby') return
    if (hasOpenPopup()) return
    // in photo mode only its own key and the gallery key act, and both close it
    if (photoModeActiveRef.current) {
      if (action === 'PhotoMode' || action === 'Gallery') setPhotoModeEnabled(false, 'shortcut')
      if (action === 'Gallery') toggleGallery()
      return
    }
    // Quick emotes: while the wheel is open, QuickEmoteN plays that slot's emote (which also
    // closes the wheel). With the wheel closed a number does nothing — this covers both
    // "hold B, tap a number" and "tap B, then a number", since either way B opened the wheel.
    const quickEmote = /^QuickEmote(\d)$/.exec(action)
    if (quickEmote != null) {
      if (!emotesOpen) return
      const emote = emotes.find((em) => em.slot === Number(quickEmote[1]))
      if (emote) playEmote(emote.urn)
      return
    }
    switch (action) {
      case 'Map': toggleMap(); break
      case 'Places': togglePlaces(); break
      case 'Events': toggleEvents('shortcut'); break
      case 'Communities': toggleCommunities(); break
      case 'Backpack': toggleBackpack(); break
      case 'Gallery': toggleGallery(); break
      case 'PhotoMode': setPhotoModeEnabled(true, 'shortcut'); break
      case 'Settings': toggleSettings(); break
      case 'Friends': toggleFriends(); break
      case 'ChatPanel': toggleChat(); break
      case 'Emote': toggleEmotes(); break
      // 'Microphone' is NOT handled here: MicUiPlugin isn't NativeUi-gated, so the engine
      // itself still toggles the mic on that action (the mic relay updates the HUD icon).
      // 'Chat' (Enter) is NOT handled here: the bridge's chat domain turns it into the
      // dedicated 'focusChat' message (releasing the engine pointer lock scene-side too).
    }
  }

  // Stable slices let memoized surfaces (the minimap) skip HUD re-renders they don't depend on.
  const mapSlice = useMemo(
    () => ({ x: mapParcel.x, y: mapParcel.y, open: mapOpen, toggle: toggleMap, teleport, changeRealm, teleportToPlace }),
    [mapParcel, mapOpen, toggleMap, teleport, changeRealm, teleportToPlace]
  )
  const minimapSlice = useMemo(
    () => ({ pose: poseRef, satelliteView, sceneTitle, donatePlace: donatePlaceKey ? (JSON.parse(donatePlaceKey) as DonatePlace) : null, setConfig: setMinimapConfig }),
    [satelliteView, sceneTitle, donatePlaceKey, setMinimapConfig]
  )

  return {
    phase,
    playerReady,
    lobbyStageReady,
    lobbyPage: { open: lobbyOpen, toggle: toggleLobby, travel: travelFromLobbyInWorld },
    homeScene,
    avatarPreviewRect,
    pickDestination,
    welcome: {
      terms: welcome?.terms ?? false,
      newProfile: welcome?.newProfile ?? false,
      pending: welcome == null,
      saving: welcomeSaving,
      error: welcomeError,
      reroll: rerollLook,
      showLook,
      preloadLook,
      lookPending: pendingLook != null,
      lastLook: shownLook.current || null,
      expectLook,
      accept: acceptWelcome,
      openLegal
    },
    sceneLoading,
    connectivity,
    loadingProgress,
    travelError,
    dismissTravelError,
    travellingTo,
    fatalError,
    reload: () => location.reload(),
    dismissFatal: () => {
      // Re-arm the engine watchdog (it left its `shown` flag set when it bridged the crash) so a later
      // genuine crash still surfaces, and clear the stashed panic so it isn't re-read stale.
      driverRef.current?.rearmCrashWatchdog?.()
      driverRef.current?.clearEnginePanic?.()
      setFatalError(null)
    },
    hover,
    cursorLocked,
    proximity,
    chat: {
      messages: channelMessages,
      send: sendChat,
      react: reactInChat,
      nearby: messages,
      bubbleText,
      channel,
      select: selectChannel,
      conversations,
      openConversation,
      closeConversation,
      deleteHistory,
      communities: communityConversations,
      openCommunity,
      closeCommunity,
      open: chatOpen,
      toggle: toggleChat,
      members,
      speaking,
      mention: mentionInChat,
      pendingMention,
      consumeMention,
      unread: chatUnread,
      setActive: setChatActive,
      nearbyUnread,
      focusTick: chatFocusTick,
      requestFocus: requestFocusChat
    },
    friends: {
      available: friendsData.available,
      list: friendsData.friends,
      received: friendsData.received,
      sent: friendsData.sent,
      blocked: friendsData.blocked,
      blockedUsers: friendsData.blockedUsers,
      blockedBy: friendsData.blockedBy,
      loading: friendsData.loading,
      open: friendsOpen,
      toggle: toggleFriends,
      act: friendAct,
      pending: friendPending,
      mutuals: mutualFriends,
      loadMutuals: loadMutualFriends
    },
    settings: { list: settings, open: settingsOpen, toggle: toggleSettings, set: settingSet, load: loadSettings },
    bindings: { list: bindings, set: bindingsSet, reset: bindingsReset, capture: captureBinding },
    profile: {
      data: profile,
      open: profileOpen,
      toggle: toggleProfile,
      ownedNames,
      saving: profileSaving,
      saveError: profileSaveError,
      save: saveProfile,
      saveNameColor,
      requestOwnedNames,
      dismissSaveError: dismissProfileSaveError
    },
    runDebugCommand,
    skybox: {
      open: skyboxOpen,
      toggle: toggleSkybox,
      hours: skyboxHours,
      progressing: skyboxProgressing,
      setHours: setSkyboxHours,
      setProgressing: setSkyboxProgressing
    },
    notifications: {
      list: notifications,
      unread: notifications.reduce((n, x) => n + (x.read ? 0 : 1), 0),
      open: notificationsOpen,
      toggle: toggleNotifications,
      markAllRead: markNotificationsRead
    },
    emotes: { list: emotes, open: emotesOpen, toggle: toggleEmotes, play: playEmote, equip: equipEmote },
    backpack: {
      list: catalogItems, total: catalogTotal, loading: catalogLoading, query: queryCatalog,
      equipped: equippedWearables, open: backpackOpen, toggle: toggleBackpack, bodyShape, colors: avatarColors, setColor: setAvatarColor, equip: equipWearables, saveError, retrySave, revertSave, preview: previewWearables, focus: focusPreview, stage: stagePreview, emote: emotePreview, rotate: rotatePreview, zoom: zoomPreview, forceRender, setForceRender,
      outfits: outfits.outfits, outfitSlots: ownedNames.length > 0 ? 10 : 5, outfitsLoaded,
      saveOutfit, deleteOutfit, equipOutfit,
      outfitHistory, randomizeOutfit, undoOutfit, redoOutfit
    },
    communities: { list: communities, browse: communityBrowse, loadBrowse: loadCommunityBrowse, loadMoreBrowse: loadMoreCommunityBrowse, open: communitiesOpen, toggle: toggleCommunities, create: createCommunity, update: updateCommunity, remove: deleteCommunity, deleted: deletedCommunity, save: communitySave, clearSave: clearCommunitySave, placeOptions: communityPlaceOptions, loadPlaceOptions: loadCommunityPlaceOptions, clearPlaceOptions: clearCommunityPlaceOptions, removePlace: removeCommunityPlace, memberAction: communityMemberAction, join: joinCommunity, requestToJoin: requestToJoinCommunity, cancelRequest: cancelJoinRequest, leave: leaveCommunity, error: communityError, detail: communityDetail, loadDetail: loadCommunityDetail, cards: communityCards, fetch: fetchCommunity, invites: communityInvites, loadInvites: loadCommunityInvites, pendingCount, loadInvitesCount: loadCommunityInvitesCount, manageRequest: manageCommunityRequest, memberSections, cardOpen: setOpenCard, loadMemberSection, invitable, loadInvitable, invite: inviteToCommunity, unban: unbanCommunityMember },
    map: mapSlice,
    minimap: minimapSlice,
    places: { open: placesOpen, toggle: togglePlaces },
    events: { open: eventsOpen, toggle: toggleEvents, data: eventsData, memberOf: eventCommunities },
    shop: { open: shopOpen, toggle: toggleShop },
    create: { open: createOpen, toggle: toggleCreate, show: showCreate },
    photoMode: {
      snapshot: photoMode,
      active: photoMode?.active ?? false,
      capture: photoCapture,
      set: setPhotoModeEnabled,
      take: takePhoto
    },
    gallery: {
      open: galleryOpen,
      toggle: toggleGallery,
      fetchPage: fetchGalleryPage,
      metas: galleryMetas,
      loadPhoto: loadGalleryPhoto,
      setPublic: setGalleryPhotoPublic,
      remove: removeGalleryPhoto
    },
    permissions: {
      pending: permissionQueue,
      visit: visitAnswers.filter((e) => e.scene === sceneHash),
      triggered: triggeredPermissions,
      unseenDenied,
      markSeen: markPermissionsSeen,
      rules: permissionRules,
      loadRules: loadPermissionRules,
      setRule: setPermissionRule,
      clearRule: clearPermissionRule,
      resetDefaults: resetPermissions,
      resolve: resolvePermission
    },
    worldPassword: { pending: worldPassword, resolve: resolveWorldPassword },
    portables: { list: portables, stop: stopPortable, restart: restartSmartWearable },
    editor: { mode: editorMode, setMode: setEditorMode },
    mic: { enabled: mic.enabled, available: mic.available, toggle: toggleMic },
    nearbyMutes: { muted: nearbyMuted, set: setNearbyMute },
    voiceChat: { state: voiceChatState, active: activeVoiceChats, activeLoaded: activeVoiceChatsLoaded, scene: sceneVoiceChat, guest: profile?.isGuest === true, act: voiceChatAct },
    nav,
    openExternalUrl,
    setEngineViewport,
    setInteractableArea,
    logout,
    menuOpen,
    closeAllPanels,
    login: {
      status,
      account,
      busy,
      error,
      engineReady,
      avatarLoaded,
      loadProgress,
      loadStep,
      startWithAccount,
      authPending: auth != null,
      authCode: auth?.code ?? null,
      cancelLogin,
      exploreAsGuest,
      acceptQueued: queuedGuest != null,
      jumpIn,
      profile: accountProfile,
      profileFetchFailed,
      resetProfileAndJumpIn,
      useDifferentAccount
    }
  }
}
