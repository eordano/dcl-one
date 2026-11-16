// Profile: the local player's profile card + any user's passport (View Profile).
//   from: @dcl/sdk getPlayer() (address/name/isGuest)
//       + the ENGINE's profile cache: BevyApi.getUserProfile (the full deployed profile — face +
//         body snapshots, name, links, about-me) and BevyApi.getCompactProfiles (display
//         identity for a list)
//       + catalyst lambda  GET /lambdas/users/:id/names  (owned NFT names)
//       + badges service   GET <badges>/users/:id/badges
//       + camera-reel       GET <cameraReel>/api/users/:id/images
import { getPlayer } from '@dcl/sdk/players'
import { catalystBase, getJson, serviceBase } from '../http'
import { resolveEquippedSet, resolveWearables } from './wearables'
import { equippedSlots, resolveEquippedEmotes } from './emotes'
import { profileSaveSettled } from './avatarDraft'
import type { Badge, Profile, ProfileInfo, SaveProfileRequest } from '@explorer-core/engine/protocol'
import type { SetAvatarData } from '@explorer-core/engine/generated'
// Not via the generated barrel: it only re-exports the top-level files, not serde_json/.
import type { JsonValue } from '@explorer-core/engine/generated/serde_json/JsonValue'
import { BevyApi } from '../bevy-api'
import { relay } from '../system-helpers'
import { identity } from '../identity'
import type { Ctx } from '../bridge'

/** A deployed profile as the engine holds it (`common::profile::SerializedProfile`, serde JSON).
 *  Only the keys the passport reads are typed here; anything else rides along untyped. */
export type SerializedProfile = {
  version?: number
  name?: string
  hasClaimedName?: boolean
  /** Profile-set custom name colour (claimed names only), 0–1 floats. */
  nameColor?: { r: number; g: number; b: number }
  description?: string
  links?: Array<{ title: string; url: string }>
  // --- about-me fields. The renderer doesn't model these, so they ride the profile as free-form
  // extra keys (SerializedProfile::extra_fields) and reach us flattened onto the avatar. Names are
  // the ones unity-explorer and the old system scene write — matching them is what makes a profile
  // edited in either client read back correctly in the other.
  country?: string
  language?: string
  gender?: string
  pronouns?: string
  relationshipStatus?: string
  sexualOrientation?: string
  employmentStatus?: string
  profession?: string
  hobbies?: string
  realName?: string
  /** Epoch SECONDS, not the ISO string the passport shows. unity-explorer writes this key;
   *  @dcl/schemas declares the all-lowercase `birthdate` instead, and the entity allows additional
   *  properties, so both can exist — but unity is what actually writes profiles, so its spelling is
   *  the one that counts. `birthdate` is read too, to pick up anything an earlier build of this
   *  HUD wrote. */
  birthDate?: number
  birthdate?: number
  avatar?: {
    snapshots?: { face256?: string; body?: string }
    /** Deployed equipped-wearables urns — resolved into the passport's Equipped Wearables section. */
    wearables?: string[]
    /** Deployed equipped-emotes wheel slots — resolved into the passport's Equipped Emotes section. */
    emotes?: Array<{ slot: number; urn: string }>
  }
}

/** Addresses are always lowercased — the same wallet reaches us in either case, and the engine
 *  matches its cache on the lowercase form. */
export const profileKey = (address: string): string => address.toLowerCase()

/** A user's deployed profile as the ENGINE holds it — the same cache and fetch cascade (catalyst,
 *  then peers) that nametags read, so a guest resolves too (their profile only exists on the
 *  wire), and a save made through `setAvatar` is visible on the next read without a cache of our
 *  own to keep in step. `undefined` when the engine can't resolve the address. */
export async function fetchProfile(address: string): Promise<SerializedProfile | undefined> {
  return await BevyApi.getUserProfile(profileKey(address)).catch(() => undefined)
}

/** What a list row needs to show a person: their name, face, and claimed-name seal. */
export type ProfileIdentity = { name: string; picture?: string; hasClaimedName: boolean }

/** A profile's compact tier as the engine returns it (`common::profile::CompactProfile`). */
export type CompactProfile = {
  pointer: string
  name: string
  hasClaimedName: boolean
  /** The claimed name's colour, 0–1 floats, when one is set. */
  nameColor: { r: number; g: number; b: number } | null
  /** The face snapshot's url; empty when the profile has none. */
  thumbnailUrl: string
}

/** The most addresses one `getCompactProfiles` call takes (the engine rejects more). */
const MAX_IDENTITIES = 200

/**
 * Resolve display identity for a list of addresses through the ENGINE's profile cache —
 * the one the nametags, chat and passport UI already read — in one call: the engine asks the
 * catalyst for the compact profiles of every address it doesn't already hold, in batches, and
 * anyone already on screen costs nothing. Returns a lookup that always answers: an address
 * with no (or an unresolvable) profile falls back to its shortened form, as does every address
 * past the first `MAX_IDENTITIES`.
 */
export async function fetchIdentities(addresses: string[]): Promise<(address: string) => ProfileIdentity> {
  const unique = [...new Set(addresses.filter((a) => a !== '').map(profileKey))]
  if (unique.length > MAX_IDENTITIES) console.error(`[profile] ${unique.length} addresses asked for, resolving the first ${MAX_IDENTITIES}`)
  const found = await BevyApi.getCompactProfiles(unique.slice(0, MAX_IDENTITIES)).catch((): CompactProfile[] => [])
  const resolved = new Map(found.map((profile) => [profileKey(profile.pointer), profile]))
  return (address) => identityOf(resolved.get(profileKey(address)), address)
}

function identityOf(data: CompactProfile | undefined, address: string): ProfileIdentity {
  return {
    name: data != null && data.name !== '' ? data.name : shortAddress(address),
    picture: httpOrUndef(data?.thumbnailUrl),
    hasClaimedName: data?.hasClaimedName ?? false
  }
}

const shortAddress = (a: string): string => (a.length > 12 ? `${a.slice(0, 6)}…${a.slice(-4)}` : a)

export const httpOrUndef = (s?: string | null): string | undefined => (typeof s === 'string' && s.startsWith('http') ? s : undefined)

/** The profile stores a non-claimed name bare; every explorer shows it with four hex digits of
 *  the address appended (the engine builds nametags the same way — `crates/avatar`), so the
 *  passport must too, or a name saved here would read back without the tag the world shows. */
const withAddressTag = (name: string, address: string, claimed: boolean): string =>
  claimed || name.includes('#') ? name : `${name}#${address.slice(-4)}`

function toProfile(av: SerializedProfile | undefined, address: string, isGuest: boolean, fallbackName: string): Profile {
  const snaps = av?.avatar?.snapshots
  const claimed = av?.hasClaimedName ?? !fallbackName.includes('#')
  return {
    address,
    name: withAddressTag(av?.name != null && av.name !== '' ? av.name : fallbackName, address, claimed),
    picture: httpOrUndef(snaps?.face256),
    bodyImage: httpOrUndef(snaps?.body),
    hasClaimedName: claimed,
    nameColor: claimed ? av?.nameColor : undefined,
    isGuest,
    version: av?.version,
    description: av?.description != null && av.description !== '' ? av.description : undefined,
    links: av?.links ?? undefined,
    info: toInfo(av)
  }
}

// --- about-me fields ------------------------------------------------------------
// The wire (`ProfileInfo`) names the fields as the passport labels them; the profile stores them
// under the names unity-explorer chose. One table drives both directions so a rename can't leave
// the read and the write disagreeing — which would look like an edit that silently didn't save.
const INFO_KEYS = {
  country: 'country',
  language: 'language',
  gender: 'gender',
  pronouns: 'pronouns',
  relationship: 'relationshipStatus',
  sexualOrientation: 'sexualOrientation',
  employment: 'employmentStatus',
  profession: 'profession',
  hobby: 'hobbies',
  realName: 'realName'
} as const satisfies Partial<Record<keyof ProfileInfo, keyof SerializedProfile>>

/** Epoch seconds (how the profile stores a birthdate) → the `YYYY-MM-DD` the passport edits.
 *  UTC on both sides: a birthday is a date, and shifting it by the viewer's timezone would let a
 *  profile read back a day out from the one that was saved. */
const toIsoDate = (seconds: number | undefined): string | undefined => {
  if (typeof seconds !== 'number' || !Number.isFinite(seconds) || seconds === 0) return undefined
  const date = new Date(seconds * 1000)
  // A profile is user-supplied data: an out-of-range timestamp makes toISOString THROW rather than
  // return anything, so the date has to be checked before it's formatted.
  if (Number.isNaN(date.getTime())) return undefined
  return date.toISOString().slice(0, 10)
}

const fromIsoDate = (iso: string | undefined): number | undefined => {
  if (iso == null || iso === '') return undefined
  const ms = Date.parse(`${iso}T00:00:00Z`)
  return Number.isNaN(ms) ? undefined : Math.floor(ms / 1000)
}

/** `undefined` when the profile carries no about-me fields at all, so the passport can tell an
 *  empty section from a missing one. */
function toInfo(av: SerializedProfile | undefined): ProfileInfo | undefined {
  if (av == null) return undefined
  const info: ProfileInfo = {}
  for (const [wireKey, profileKey] of Object.entries(INFO_KEYS) as Array<[keyof ProfileInfo, keyof SerializedProfile]>) {
    const value = av[profileKey]
    if (typeof value === 'string' && value !== '') info[wireKey] = value
  }
  const birthdate = toIsoDate(av.birthDate ?? av.birthdate)
  if (birthdate != null) info.birthdate = birthdate
  return Object.keys(info).length > 0 ? info : undefined
}

// --- badges --------------------------------------------------------------------
type BadgeAssets = { '2d'?: Partial<Record<string, string>> }
type ApiBadge = {
  id: string
  name: string
  description?: string
  isTier?: boolean
  assets?: BadgeAssets
  /** Unix millis as a string; null until complete (the top tier, for tiered badges). */
  completedAt?: string | null
  progress?: {
    stepsDone?: number
    nextStepsTarget?: number | null
    totalStepsTarget?: number
    achievedTiers?: Array<{ tierId: string; completedAt?: number | string | null }>
    lastCompletedTierName?: string | null
    lastCompletedTierImage?: string | null
    /** Unix millis (a number, unlike `completedAt`). */
    lastCompletedTierAt?: number | string | null
  }
}
type BadgesResponse = { data?: { achieved?: ApiBadge[]; notAchieved?: ApiBadge[] } }
type ApiTier = { tierId: string; tierName: string; description?: string; assets?: BadgeAssets }
type TiersResponse = { data?: { tiers?: ApiTier[] } }

function millis(raw: number | string | null | undefined): number | undefined {
  const ms = raw == null ? NaN : Number(raw)
  return Number.isFinite(ms) && ms > 0 ? ms : undefined
}

// A badge's tier definitions never change, so one fetch per badge id serves every passport.
const tierCache = new Map<string, Promise<ApiTier[] | undefined>>()
async function fetchTiers(badgeId: string): Promise<ApiTier[] | undefined> {
  let tiers = tierCache.get(badgeId)
  if (tiers == null) {
    tiers = serviceBase('badges')
      .then(async (badges) => await getJson<TiersResponse>(`${badges}/badges/${badgeId}/tiers`))
      .then((r) => r?.data?.tiers)
    tiers.catch(() => tierCache.delete(badgeId))
    tierCache.set(badgeId, tiers)
  }
  return await tiers
}

function toBadge(b: ApiBadge, locked: boolean): Badge {
  const p = b.progress
  const done = p?.stepsDone ?? 0
  const progress =
    b.completedAt != null
      ? undefined
      : b.isTier && p?.nextStepsTarget != null
        ? { done, target: p.nextStepsTarget }
        : !b.isTier && p?.totalStepsTarget != null && p.totalStepsTarget > 1
          ? { done, target: p.totalStepsTarget }
          : undefined
  return {
    id: b.id,
    name: b.name,
    description: b.description,
    tier: p?.lastCompletedTierName ?? undefined,
    image: httpOrUndef(p?.lastCompletedTierImage) ?? httpOrUndef(b.assets?.['2d']?.normal),
    // Unity shows the latest tier's date where there is one, else the badge's own.
    unlockedAt: millis(p?.lastCompletedTierAt ?? b.completedAt),
    locked: locked || undefined,
    progress
  }
}

async function withTiers(badge: Badge, b: ApiBadge): Promise<Badge> {
  const tierDefs = b.isTier ? await fetchTiers(b.id).catch(() => undefined) : undefined
  if (tierDefs == null) return badge
  const reached = new Map((b.progress?.achievedTiers ?? []).map((t) => [t.tierId, millis(t.completedAt)]))
  const tiers = tierDefs.map((t) => ({
    id: t.tierId,
    name: t.tierName,
    description: t.description ?? '',
    image: httpOrUndef(t.assets?.['2d']?.normal),
    unlockedAt: reached.get(t.tierId)
  }))
  return { ...badge, tiers }
}

type BadgesFetch = {
  /** The list, from one request. */
  badges: Badge[]
  /** Whether any of them is tiered, i.e. whether `withTiers` has anything to fetch. */
  tiered: boolean
  /** The same list with each tiered badge's tiers filled in: one more request per tiered badge
   *  (cached per badge id), so it is awaited separately and never holds the passport up. */
  withTiers: () => Promise<Badge[]>
}

// `includeLocked` (your own passport) adds the badges not earned yet; the service only lists them
// on request, and unity-explorer only asks for its own user.
async function fetchBadges(address: string, includeLocked: boolean): Promise<BadgesFetch | undefined> {
  const r = await serviceBase('badges')
    .then(async (badges) => await getJson<BadgesResponse>(`${badges}/users/${address}/badges?includeNotAchieved=${includeLocked}`))
    .catch(() => undefined)
  const achieved = r?.data?.achieved
  if (achieved == null) return undefined
  // Newest first: the service's order is its badge catalogue, which means nothing to a reader.
  // Locked badges follow in catalogue order, as in unity-explorer.
  const unlocked = achieved.map((b): [Badge, ApiBadge] => [toBadge(b, false), b])
  unlocked.sort(([a], [b]) => (b.unlockedAt ?? 0) - (a.unlockedAt ?? 0))
  const all = [...unlocked, ...(r?.data?.notAchieved ?? []).map((b): [Badge, ApiBadge] => [toBadge(b, true), b])]
  return {
    badges: all.map(([badge]) => badge),
    tiered: all.some(([, b]) => b.isTier === true),
    withTiers: async () => await Promise.all(all.map(async ([badge, b]) => await withTiers(badge, b)))
  }
}

// --- claimed names -------------------------------------------------------------
// The NFT names this wallet owns. Only one of them can be worn without the `#1234` suffix, so the
// picker offers exactly these; anything else is an unclaimed name.
type NamesResponse = { elements?: Array<{ name?: string }> }

let ownedNames: string[] | undefined
identity.onChange(() => {
  ownedNames = undefined
})
async function fetchOwnedNames(address: string): Promise<string[]> {
  if (ownedNames != null) return ownedNames
  const base = await catalystBase()
  const r = await getJson<NamesResponse>(`${base}/lambdas/users/${address}/names`).catch(() => undefined)
  // Cached only on success: a failed lookup must not pin an empty picker for the whole session.
  if (r == null) return []
  ownedNames = (r.elements ?? []).map((e) => e.name).filter((n): n is string => typeof n === 'string' && n !== '')
  return ownedNames
}

/**
 * A save turns the passport's edits into the profile keys the deployed profile actually holds.
 * Everything here is a PARTIAL update — the engine merges `profileExtras` per key and treats an
 * empty `bodyShapeUrn`/absent colors as "unchanged" — so we send only what the user edited and
 * never have to restate (and risk clobbering) the rest of their profile. Clearing a field sends
 * `null`, which removes the key outright rather than leaving an empty string behind.
 */
function toProfileExtras(msg: SaveProfileRequest): Record<string, JsonValue> {
  const extras: Record<string, JsonValue> = {}
  const set = (key: string, value: string | number | undefined): void => {
    extras[key] = value == null || value === '' ? null : value
  }
  // `description` is REQUIRED by the profile schema (@dcl/schemas Avatar), so clearing it means
  // sending an empty string: a null here removes the key, and the catalyst rejects the deploy with
  // "failed to deploy to server." Every other field below is nullable/optional, so for those a
  // null — which the engine applies by removing the key — is the right way to clear.
  if (msg.description !== undefined) extras.description = msg.description.trim()
  if (msg.links !== undefined) {
    const links = msg.links.filter((l) => l.url !== '')
    extras.links = links.length > 0 ? links : null
  }
  if (msg.info !== undefined) {
    for (const [wireKey, profileKey] of Object.entries(INFO_KEYS) as Array<[keyof ProfileInfo, string]>) {
      set(profileKey, msg.info[wireKey]?.trim())
    }
    set('birthDate', fromIsoDate(msg.info.birthdate))
    // Drop the all-lowercase key an earlier build of this HUD may have written, so a stale value
    // can't sit alongside the one unity-explorer reads.
    extras.birthdate = null
  }
  return extras
}

export function registerProfile(ctx: Ctx): void {
  ctx.on('getOwnedNames', async () => {
    const player = getPlayer()
    ctx.send({ kind: 'ownedNames', names: player == null ? [] : await fetchOwnedNames(player.userId).catch(() => []) })
  })

  ctx.on('saveProfile', async (msg) => {
    const player = getPlayer()
    if (player == null) {
      ctx.send({ kind: 'profileSaved', ok: false, error: 'Not signed in yet.' })
      return
    }

    // the account can change while this waits; the edit and its answers belong to this one only
    const signedOut = (): boolean => getPlayer()?.userId !== player.userId
    const data: SetAvatarData = {}
    if (msg.name !== undefined) {
      const owned = await fetchOwnedNames(player.userId).catch(() => [])
      if (signedOut()) return
      // An empty bodyShapeUrn and null colors mean "leave the avatar alone" — the name is the only
      // thing this edit touches, and the Backpack owns the rest.
      data.base = { skinColor: null, eyesColor: null, hairColor: null, bodyShapeUrn: '', name: msg.name, talking: null }
      data.hasClaimedName = owned.some((n) => n.toLowerCase() === msg.name?.toLowerCase())
    }
    const extras = toProfileExtras(msg)
    if (Object.keys(extras).length > 0) data.profileExtras = extras
    if (msg.nameColor !== undefined) data.nameColor = { ...msg.nameColor, clear: false }
    if (data.base == null && data.profileExtras == null && data.nameColor == null) {
      ctx.send({ kind: 'profileSaved', ok: true })
      return
    }

    try {
      // setAvatar resolves only once the engine has deployed the new profile version (guests
      // resolve immediately — they have no catalyst presence to deploy to), so a rejection here
      // is a genuinely failed save and the HUD must not keep showing the edit as if it stuck.
      await BevyApi.setAvatar(data)
    } catch (e) {
      if (signedOut()) return
      // a save that a later one replaced settles nothing: the later one carries it
      if (!String(e).includes('cancelled')) profileSaveSettled(false)
      console.error('[profile] save failed', e)
      ctx.send({ kind: 'profileSaved', ok: false, error: e instanceof Error ? e.message : String(e) })
      return
    }

    if (signedOut()) return
    profileSaveSettled(true)
    ctx.send({ kind: 'profileSaved', ok: true })
    // setAvatar amends the engine's own copy of the profile before it resolves, so re-reading it
    // is the post-save state — no need to fold the edit in by hand.
    const av = await fetchProfile(player.userId)
    if (signedOut()) return
    ctx.send({ kind: 'profile', profile: toProfile(av, player.userId, player.isGuest, player.name) })
  })

  // The page marks its world-entry profile fetch done as soon as it ASKS, so an answer of `null`
  // costs it the profile for the whole session. `getPlayer()` is the scene's view of the player
  // CRDT, which lags world entry by a good few hundred frames, so a request that arrives in that
  // window is held rather than answered — the page only ever hears a profile it can use.
  let wanted = false
  let inFlight = false
  const answerProfile = async (): Promise<void> => {
    const player = getPlayer()
    if (player == null || inFlight) return
    wanted = false
    inFlight = true
    try {
      const av = await fetchProfile(player.userId)
      ctx.send({ kind: 'profile', profile: toProfile(av, player.userId, player.isGuest, player.name) })
    } finally {
      inFlight = false
    }
  }
  ctx.on('getProfile', () => {
    wanted = true
    void answerProfile()
  })
  ctx.push(() => {
    if (wanted) void answerProfile()
  })

  // The engine tells us when any profile it holds moves to a new version; the page decides what
  // it is still showing and re-reads only that.
  relay('profile', async () => await BevyApi.getProfileChangedStream(), (ev) => {
    ctx.send({ kind: 'profileChanged', address: ev.address, version: ev.version })
  })

  // A user's profile by address: the engine's copy alone for a name and a face (chat lines, the
  // profile card), plus badges + equipped items when the passport asks (`extras`).
  ctx.on('getUserProfile', async (msg) => {
    if (msg.extras !== true) {
      const av = await fetchProfile(msg.address)
      ctx.send({
        kind: 'userProfile',
        address: msg.address,
        profile: av == null ? null : toProfile(av, msg.address, false, av.name ?? msg.address)
      })
      return
    }
    const me = getPlayer()
    const isSelf = me != null && me.userId.toLowerCase() === msg.address.toLowerCase()
    const [av, badges] = await Promise.all([
      fetchProfile(msg.address),
      fetchBadges(msg.address, isSelf).catch(() => undefined)
    ])
    if (av == null && badges == null) {
      ctx.send({ kind: 'userProfile', address: msg.address, profile: null })
      return
    }
    // Your OWN passport: read the live avatar (getPlayer()) so it matches the Backpack exactly.
    // Other users have no live source (getPlayer(userId) only resolves nearby avatars), so they
    // read the equipment their deployed profile lists.
    const wearableUrns = isSelf ? (me.wearables ?? []).map(String) : (av?.avatar?.wearables ?? [])
    // Through equippedSlots, not me.emotes: the bevy runtime leaves a fresh profile's wheel empty
    // and the emote wheel fills it with the 10 base emotes, so reading the raw array would show an
    // empty Equipped Emotes section in your own passport while the wheel shows ten.
    const emoteEntries = isSelf
      ? equippedSlots(me.emotes).map((urn, slot) => ({ slot, urn }))
      : (av?.avatar?.emotes ?? [])
    const [equippedWearables, equippedEmotes] = await Promise.all([
      // Only our OWN urns go through resolveEquippedSet (it indexes tokens for the equip handler);
      // shopUrls because the passport is the only surface that renders a SHOP action.
      (isSelf ? resolveEquippedSet : resolveWearables)(wearableUrns, { shopUrls: true }).catch(() => undefined),
      resolveEquippedEmotes(emoteEntries).catch(() => undefined)
    ])
    const profile: Profile = {
      ...toProfile(av, msg.address, false, av?.name ?? msg.address),
      badges: badges?.badges,
      equippedWearables,
      equippedEmotes
    }
    ctx.send({ kind: 'userProfile', address: msg.address, profile })
    // The tiers arrive in a second copy of the same profile, so the passport shows its badges at
    // once and the tier rows (and NEXT TIER names) fill in when they land.
    if (badges?.tiered === true) {
      ctx.send({ kind: 'userProfile', address: msg.address, profile: { ...profile, badges: await badges.withTiers() } })
    }
  })
}
