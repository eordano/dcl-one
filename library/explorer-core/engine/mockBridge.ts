// A fake bridge "scene" that answers the protocol on the BroadcastChannel, so the
// React HUD runs fully standalone (no engine, no wasm) under `?mock=1`. In the real
// app this is the super-user SDK7 bridge scene forwarding to SystemApi — the
// page-side BridgeClient does not change at all.

import {
  bridgeChannelName,
  type ActiveCommunityVoiceChat,
  type AppNotification,
  type Badge,
  type BadgeTier,
  type BindingEntry,
  type Community,
  type CommunityChannel,
  type CommunityMember,
  type CommunityMemberSection,
  type CommunityRequestMember,
  type DmHistoryEntry,
  type Emote,
  type Envelope,
  type GalleryPhoto,
  type Outfit,
  type OutfitsMetadata,
  type PageToScene,
  type PhotoModeSnapshot,
  type Profile,
  type ProfileEdit,
  type SceneToPage,
  type Setting,
  type VoiceChatActionWire,
  type VoiceChatParticipant,
  type VoiceChatRole,
  type VoiceChatState,
  type Wearable
} from './protocol'
import { applyProfileEdit } from './profileEdit'
import { mentionName } from './mention'
import { mockEventList, mockEventPlaces, setMockReminder } from './mockEvents'
import type { Color3, PortableInfo, PortableState } from './generated/index'
import { GENESIS_SATELLITE_VIEW } from '../features/map/atlas'
import { DEFAULT_REALM } from '@hud-host'

const badgeImg = (seed: string): string => `https://picsum.photos/seed/${seed}/96/96`
const tier = (badge: string, name: string, steps: string, unlockedAt?: number): BadgeTier => ({
  id: `${badge}-${name.toLowerCase()}`,
  name,
  description: `${steps} Genesis City scenes explored (for at least 1 min)`,
  image: badgeImg(`${badge}-${name}`),
  unlockedAt
})
// Newest first, then the locked ones — the order the bridge delivers.
function mockBadges(): Badge[] {
  return [
    { id: 'regally_rare', name: 'Regally Rare', description: 'Equipped at least 3 Rare Wearables at the same time', image: badgeImg('rare'), unlockedAt: Date.UTC(2026, 6, 15, 14, 16) },
    { id: 'festive_trail_2025', name: 'Festive Trail 2025', description: 'Followed the Festive Trail', image: badgeImg('festive'), unlockedAt: Date.UTC(2026, 0, 6, 20, 24) },
    { id: 'profile_pro', name: 'Profile Pro', description: 'Completed your profile', image: badgeImg('profile'), unlockedAt: Date.UTC(2025, 6, 1, 15, 27) },
    {
      id: 'traveler',
      name: 'Traveler',
      description: 'Genesis City scenes explored',
      tier: 'Starter',
      image: badgeImg('traveler-Starter'),
      unlockedAt: Date.UTC(2024, 11, 4, 14, 17),
      progress: { done: 5, target: 50 },
      tiers: [tier('traveler', 'Starter', '1', Date.UTC(2024, 11, 4, 14, 17)), tier('traveler', 'Bronze', '50'), tier('traveler', 'Silver', '250'), tier('traveler', 'Gold', '1,000'), tier('traveler', 'Platinum', '2,500'), tier('traveler', 'Diamond', '10,000')]
    },
    { id: 'dclcitizen', name: 'Decentraland Citizen', description: 'Landed in Decentraland', image: badgeImg('citizen'), unlockedAt: Date.UTC(2024, 11, 4, 14, 16) },
    {
      id: 'social_butterfly',
      name: 'Social Butterfly',
      description: 'Friends made',
      tier: 'Bronze',
      image: badgeImg('butterfly-Bronze'),
      unlockedAt: Date.UTC(2024, 2, 27, 15, 18),
      progress: { done: 12, target: 50 },
      tiers: [tier('butterfly', 'Starter', '1', Date.UTC(2024, 2, 20, 9, 0)), tier('butterfly', 'Bronze', '5', Date.UTC(2024, 2, 27, 15, 18)), tier('butterfly', 'Silver', '50'), tier('butterfly', 'Gold', '250')]
    },
    { id: 'moves_master', name: 'Moves Master', description: 'Emotes played', image: badgeImg('moves-Starter'), locked: true, progress: { done: 8, target: 100 }, tiers: [tier('moves', 'Starter', '100'), tier('moves', 'Bronze', '500'), tier('moves', 'Silver', '1,000')] },
    { id: 'vertical_voyager', name: 'Vertical Voyager', description: 'Reached 500m above ground', image: badgeImg('voyager'), locked: true },
    { id: 'open_for_business', name: 'Open for Business', description: 'Listed 2 items on the Marketplace', image: badgeImg('business'), locked: true, progress: { done: 0, target: 2 } }
  ]
}

// A fully-populated passport for the mock, so the React passport shows every section.
function richProfile(address: string, name: string, isGuest: boolean): Profile {
  return {
    address,
    name,
    picture: 'https://profile-images.decentraland.org/entities/bafkreid5btlh76opew65hxu6dtkdo6ybqhymdof6vrrmjy2p5a74oy4huq/face.png',
    bodyImage: `https://picsum.photos/seed/${encodeURIComponent(address)}/360/760`,
    hasClaimedName: !name.includes('#'),
    isGuest,
    description: 'Exploring Decentraland one plaza at a time. 🌅 DCL citizen since 2022.',
    links: [
      { title: 'Twitter', url: 'https://twitter.com' },
      { title: 'Discord', url: 'https://discord.com' }
    ],
    mutuals: 30,
    badges: mockBadges(),
    equippedWearables: equippedNow(),
    equippedEmotes: mockEquippedEmotes(),
    info: {
      gender: 'Male',
      birthdate: '1991-11-26',
      pronouns: 'He / Him',
      relationship: 'Single',
      sexualOrientation: 'Heterosexual',
      language: 'Persian',
      country: 'Argentina',
      profession: 'IT',
      employment: 'Chilling',
      hobby: 'games.movie.party',
      realName: 'mohammad'
    }
  }
}

// Nearby roster — shared by the `members` stream and getUserProfile (so a fetched
// passport resolves a real name from the clicked sender's address).
const MOCK_NEARBY = [
  { address: '0x5854cce95d5e25817b41f4c41f06b695a83bc495', name: 'Mojito', picture: 'https://profile-images.decentraland.org/entities/bafkreid5btlh76opew65hxu6dtkdo6ybqhymdof6vrrmjy2p5a74oy4huq/face.png' },
  { address: '0x6723dcb07f3ca735223cd1c0acfa62dd994a1bb4', name: 'Sharknado', picture: 'https://profile-images.decentraland.org/entities/bafkreie5bpho47gnh3jrfxoezwc4pxffup4cmmhmdxsmpf3oslopxb4enm/face.png' },
  { address: '0x1e105bb213754519903788022b962fe2b9c4b263', name: 'Pravus', picture: 'https://profile-images.decentraland.org/entities/bafkreig4xay5oxgbf75hwkjefx5hgdcdvm6a4tnpiisltslxf4jtajkbyq/face.png' },
  { address: '0x9f8c2a1b4d6e7f0a3b5c8d9e1f2a4b6c8d0e1f23', name: 'Johnny', claimed: false },
  { address: '0x3a1b2c4d5e6f7081920a3b4c5d6e7f8091a2b3c4', name: 'Clara' },
  { address: '0x77a0b1c2d3e4f5061728394a5b6c7d8e9f001122', name: 'SpottyGoat' },
  { address: '0xc0ffee254729296a45a3885639ac7e10f9d54979', name: '' }
]

// The places the mock user can link to a community, and what each community links.
const MOCK_OWNED_PLACES = [
  { id: 'place-land-1', isWorld: false, label: 'Mock Plaza (12,-34)', ownerName: 'You' },
  { id: 'place-world-1', isWorld: true, label: 'Mock Mansion (mock.dcl.eth)', ownerName: 'You' }
]
const mockLinkedPlaces = new Map<string, string[]>()
// The card places unlinked per community.
const mockRemovedPlaces = new Map<string, Set<string>>()
// The card's member actions by community: a member's new role, or null once kicked or banned.
const mockMemberChanges = new Map<string, Map<string, string | null>>()
// The Members tab's other sections, by community: join requests received, invites sent and bans.
const mockPerson = (n: number, name: string): CommunityRequestMember => ({ address: `0x${(0xa0 + n).toString(16).padStart(40, '0')}`, name, role: 'none', requestId: `rq-${n}` })
const mockSections: Record<CommunityMemberSection, Map<string, CommunityRequestMember[]>> = {
  requests: new Map([['c8', [mockPerson(1, 'Fenix#a1b2'), mockPerson(2, 'Moonwalker'), mockPerson(3, 'Gaia#77fe')]]]),
  invites: new Map([['c8', [mockPerson(4, 'Orbit'), mockPerson(5, 'Lumen#0c3d')]]]),
  banned: new Map([['c8', [{ ...mockPerson(6, 'Troll#dead'), requestId: '' }]]])
}
const mockSection = (section: CommunityMemberSection, id: string): CommunityRequestMember[] => mockSections[section].get(id) ?? []

const mockCommunities: Community[] = [
  { id: 'c1', name: 'Decentraland Foundation', description: 'The official Decentraland Foundation community. Stay up to date with events, releases and everything happening across the metaverse.', thumbnail: 'https://picsum.photos/seed/dcl/540/360', membersCount: 1242, role: 'member', ownerName: 'DCLOfficial', privacy: 'public' },
  { id: 'c2', name: 'Chiri Storytelling', description: "This space brings to life Chiri's adventures. Gathering cozy Social Gamers, Creators, Digi Fashion lovers, Virtual Citizens & more!", thumbnail: '', membersCount: 374, role: 'none', ownerName: 'Chiri', privacy: 'private' },
  { id: 'c3', name: 'VTATV', description: 'Virtual television and live shows broadcast from inside Decentraland.', thumbnail: '', membersCount: 186, role: 'none', ownerName: 'VTATV', pendingInviteId: 'inv-c3' },
  { id: 'c4', name: 'ABC Decentraland', description: 'Adventures and education for newcomers to the metaverse.', thumbnail: '', membersCount: 436, role: 'none', ownerName: 'BillyTeacoin' },
  { id: 'c5', name: 'Toxic Events', description: 'The wildest parties and events in DCL.', thumbnail: '', membersCount: 601, role: 'member', ownerName: 'ToxicWaifu' },
  { id: 'c6', name: 'Last Slice Creators', description: 'A community of pizza-loving 3D creators.', thumbnail: '', membersCount: 164, role: 'none', ownerName: 'Lastraum', privacy: 'private', pendingInviteId: 'inv-c6' },
  { id: 'c7', name: 'SheFi', description: 'Empowering women in web3 and DeFi.', thumbnail: '', membersCount: 310, role: 'none', ownerName: 'msjoedor' },
  { id: 'c8', name: 'Community Building DCL', description: 'Helping communities grow in Decentraland.', thumbnail: '', membersCount: 639, role: 'owner', ownerName: 'TheCryptKeeper', privacy: 'private' },
  // more than two browse pages (20 a page), every third private, some with friends in them
  ...Array.from({ length: 40 }, (_, i): Community => ({
    id: `b${i + 1}`,
    name: `${['Pixel', 'Neon', 'Genesis', 'Lunar', 'Voxel'][i % 5]} ${['Builders', 'Club', 'Guild', 'Collective', 'Lounge', 'Crew', 'Society', 'Hub'][i % 8]} ${i + 1}`,
    description: `Mock community ${i + 1}, here so the browse grid has pages to load.`,
    thumbnail: '',
    membersCount: 20 + ((i * 37) % 400),
    role: 'none',
    ownerName: `owner${i + 1}`,
    privacy: i % 3 === 2 ? 'private' : 'public'
  }))
]
// The user's friends in a community (the browse card's faces).
const mockFriend = (n: number): { address: string; name: string; picture: string; hasClaimedName: boolean } => ({
  address: `0x${(n + 100).toString(16).padStart(40, '0')}`,
  name: ['Mojito', 'KazeNoKai', 'laurenmae', 'Kimbo'][n % 4],
  picture: `https://picsum.photos/seed/friend${n}/64/64`,
  hasClaimedName: n % 2 === 0
})
mockCommunities[0].friends = [mockFriend(0), mockFriend(1), mockFriend(2)]
mockCommunities[2].friends = [mockFriend(3)]
mockCommunities[9].friends = [mockFriend(1), mockFriend(2)]
// My Communities: the ones the user is a member of
const mockMine = (): Community[] => mockCommunities.filter((c) => c.role === 'owner' || c.role === 'moderator' || c.role === 'member')

// A live stream on one community for the Communities page's chip and the scene banner.
mockCommunities[0].voiceChatStatus = { isActive: true, participantCount: 12, moderatorCount: 1 }
const MOCK_ACTIVE_CHATS: ActiveCommunityVoiceChat[] = [
  { communityId: 'c1', communityName: 'Decentraland Foundation', communityImage: 'https://picsum.photos/seed/dcl/540/360', isMember: true, positions: ['0,0', '0,1'], worlds: [], participantCount: 12, moderatorCount: 1 }
]

/** Voice chat: a scripted call so every panel state shows in ?mock=1. `?mockcall=incoming` rings
 *  on start; the Call buttons and the community Start / Join flows drive the rest. */
const MOCK_ME = '0x0000000000000000000000000000000000000001'
function mockParticipant(address: string, name: string, picture: string | undefined, role: VoiceChatRole, isSpeaker: boolean, extra: Partial<VoiceChatParticipant> = {}): VoiceChatParticipant {
  return { address, name, hasClaimedName: !/\d/.test(name), profilePictureUrl: picture ?? '', role, isSpeaker, isRequestingToSpeak: false, isMuted: false, isSpeaking: false, ...extra }
}
const idleVoice: VoiceChatState = { status: 'disconnected', kind: 'none', callId: null, communityId: null, peer: null, connection: 'none', participants: [], micEnabled: false, localSpeaker: false, localRole: 'none', localRequestingToSpeak: false, error: null, errorIsNotice: false, errorSeq: 0 }
let voice: VoiceChatState = { ...idleVoice }
let mockVoiceAction: ((action: VoiceChatActionWire) => void) | null = null
const voiceTimers: ReturnType<typeof setTimeout>[] = []
const clearVoiceTimers = (): void => {
  while (voiceTimers.length > 0) clearTimeout(voiceTimers.pop())
}

const RARITIES = ['base', 'common', 'uncommon', 'rare', 'epic', 'legendary', 'mythic', 'unique', 'exotic']
const thumb = (urn: string): string => `https://peer.decentraland.org/lambdas/collections/contents/${urn}/thumbnail`
// Real base-avatar wearables so the mock grid shows actual thumbnails (catalyst).
const BASE: { name: string; category: string; label: string }[] = [
  { name: 'casual_hair_01', category: 'hair', label: 'Casual Hair' },
  { name: 'cornrows', category: 'hair', label: 'Cornrows' },
  { name: 'hair_anime_01', category: 'hair', label: 'Anime Hair' },
  { name: 'eyebrows_00', category: 'eyebrows', label: 'Eyebrows' },
  { name: 'mouth_00', category: 'mouth', label: 'Mouth' },
  { name: 'beard', category: 'facial_hair', label: 'Beard' },
  { name: 'granpa_beard', category: 'facial_hair', label: "Grandpa Beard" },
  { name: 'f_sweater', category: 'upper_body', label: 'Sweater' },
  { name: 'm_greenhoodie', category: 'upper_body', label: 'Green Hoodie' },
  { name: 'blue_tshirt', category: 'upper_body', label: 'Blue T-Shirt' },
  { name: 'sport_jacket', category: 'upper_body', label: 'Sport Jacket' },
  { name: 'f_jeans', category: 'lower_body', label: 'Jeans' },
  { name: 'f_brown_trousers', category: 'lower_body', label: 'Brown Trousers' },
  { name: 'basketball_shorts', category: 'lower_body', label: 'Basketball Shorts' },
  { name: 'sneakers', category: 'feet', label: 'Sneakers' },
  { name: 'bun_shoes', category: 'feet', label: 'Bun Shoes' },
  { name: 'm_greenflipflops', category: 'feet', label: 'Flip Flops' },
  { name: 'hat', category: 'hat', label: 'Hat' },
  { name: 'bandana', category: 'hat', label: 'Bandana' },
  { name: 'black_sun_glasses', category: 'eyewear', label: 'Sunglasses' },
  { name: 'piratepatch', category: 'eyewear', label: 'Pirate Patch' },
  { name: 'blue_bandana', category: 'mask', label: 'Blue Bandana' },
  { name: 'pink_gem_earring', category: 'earring', label: 'Pink Gem Earring' },
  { name: 'Thunder_earring', category: 'earring', label: 'Thunder Earring' }
]
// The mock avatar's colors (preset values: skin #ddb18f, hair #5b310f, eyes #20b3f6).
const mockColors: { skin?: Color3; hair?: Color3; eyes?: Color3 } = {
  skin: { r: 221 / 255, g: 177 / 255, b: 143 / 255 },
  hair: { r: 91 / 255, g: 49 / 255, b: 15 / 255 },
  eyes: { r: 32 / 255, g: 179 / 255, b: 246 / 255 }
}

const mockWearables: Wearable[] = BASE.map((b, i) => {
  const urn = `urn:decentraland:off-chain:base-avatars:${b.name}`
  return {
    urn,
    name: b.label,
    rarity: RARITIES[i % RARITIES.length],
    category: b.category,
    thumbnail: thumb(urn),
    // f_* base items are female-only in the catalyst (the mock avatar is BaseMale).
    bodyShapes: b.name.startsWith('f_') ? ['urn:decentraland:off-chain:base-avatars:BaseFemale'] : undefined,
    equipped: i % 6 === 0,
    isSmart: b.name === 'sport_jacket' || b.name === 'Thunder_earring' ? true : undefined,
    hides: b.name === 'bandana' ? ['hair'] : undefined
  }
})

// A saved outfit from a set of wearable urns (fixed colors/body shape — no real avatar in the mock).
const BASE_MALE = 'urn:decentraland:off-chain:base-avatars:BaseMale'
const mockOutfit = (urns: string[]): Outfit => ({
  bodyShape: BASE_MALE,
  eyes: { color: { r: 0.37, g: 0.22, b: 0.19 } },
  hair: { color: { r: 0.6, g: 0.36, b: 0.2 } },
  skin: { color: { r: 0.8, g: 0.6, b: 0.47 } },
  wearables: urns,
  forceRender: []
})

// An equipped wearable that is NOT in the catalog grid (simulates an item beyond the fetched page).
// It must still drive its category slot via the decoupled `equipped` list — the bug this exercises.
const MOCK_OFF_CATALOG_EQUIPPED: Wearable[] = [
  { urn: 'urn:decentraland:off-chain:base-avatars:thug_life', name: 'Off-catalog Eyewear', rarity: 'epic', category: 'eyewear', thumbnail: thumb('urn:decentraland:off-chain:base-avatars:black_sun_glasses'), equipped: true }
]
// Equipped collectibles, both carrying the shopUrl the scene resolves (→ the passport shows the SHOP
// hover button): a collections-v2 item, whose urn already ends in the numeric item id, and a legacy
// collections-v1 item, whose slug urn needs the marketplace lookup to reach item 3. Base/off-chain
// wearables have no listing and so never get one.
const MOCK_COLLECTIBLE_EQUIPPED: Wearable[] = [
  { urn: 'urn:decentraland:matic:collections-v2:0xa42e166edac870aa5351b098ae6458d39ca0fca6:0', name: 'Neon Tiara', rarity: 'legendary', category: 'tiara', thumbnail: thumb('urn:decentraland:off-chain:base-avatars:hat'), equipped: true, shopUrl: 'https://decentraland.org/shop/item/0xa42e166edac870aa5351b098ae6458d39ca0fca6/0' },
  { urn: 'urn:decentraland:ethereum:collections-v1:mf_sammichgamer:mf_animehair', name: 'Anime warrior hair', rarity: 'legendary', category: 'hair', thumbnail: thumb('urn:decentraland:off-chain:base-avatars:hair_anime_01'), equipped: true, shopUrl: 'https://decentraland.org/shop/item/0x30d3387ff3de2a21bef7032f82d00ff7739e403c/3' }
]
let mockForceRender: string[] = []
const equippedNow = (): Wearable[] => [...mockWearables.filter((w) => w.equipped), ...MOCK_OFF_CATALOG_EQUIPPED, ...MOCK_COLLECTIBLE_EQUIPPED]

// The 10 wheel-slot base emotes — shared by getEmotes (the wheel) and the passport's Equipped
// Emotes section, so both mocks agree.
const MOCK_EMOTE_NAMES = ['Hands Air', 'Wave', 'Fist Pump', 'Dance', 'Raise Hand', 'Clap', 'Money', 'Kiss', 'Head Explode', 'Shrug']
const mockEquippedEmotes = (): Emote[] =>
  // No `thumbnail` field — mirrors the scene relay so we validate URN-derived thumbnails too.
  MOCK_EMOTE_NAMES.map((name, slot) => ({
    slot,
    urn: `urn:decentraland:off-chain:base-emotes:${name.toLowerCase().replace(/ /g, '')}`,
    name,
    rarity: 'base'
  }))

const v = (name: string): { name: string; description: string } => ({ name, description: '' })

// Stateful so the mock's controls actually move when changed (setSetting → re-emit).
const mockSettings: Setting[] = [
  { name: 'fullscreen', category: 'general', description: 'Run in fullscreen mode.', minValue: 0, maxValue: 1, namedVariants: [v('Off'), v('On')], value: 1, default: 1, stepSize: 1 },
  { name: 'resolution', category: 'general', description: '', minValue: 0, maxValue: 2, namedVariants: [v('1280 × 720'), v('1920 × 1080'), v('2560 × 1440')], value: 1, default: 1, stepSize: 1 },
  { name: 'graphics_quality', category: 'graphics', description: 'Overall visual quality.', minValue: 0, maxValue: 3, namedVariants: [v('Low'), v('Medium'), v('High'), v('Ultra')], value: 2, default: 1, stepSize: 1 },
  { name: 'fps_limit', category: 'graphics', description: 'Cap the frame rate.', minValue: 30, maxValue: 144, namedVariants: [], value: 60, default: 60, stepSize: 1 },
  { name: 'Voice Volume', category: 'audio', description: "The volume of nearby players' voices.", minValue: 0, maxValue: 100, namedVariants: [], value: 100, default: 100, stepSize: 1 },
  { name: 'Voice Chat Volume', category: 'audio', description: 'The volume of private calls and community voice streams.', minValue: 0, maxValue: 100, namedVariants: [], value: 100, default: 100, stepSize: 1 },
  { name: 'master_volume', category: 'audio', description: '', minValue: 0, maxValue: 100, namedVariants: [], value: 80, default: 100, stepSize: 1 },
  { name: 'voice_chat', category: 'audio', description: 'Enable voice chat.', minValue: 0, maxValue: 1, namedVariants: [v('Off'), v('On')], value: 1, default: 1, stepSize: 1 }
]

// The engine's default binding table (mirrors InputMap::default() in crates/common/src/inputs.rs)
// — drives the mock keydown→systemAction relay and the Key Bindings tab under ?mock=1.
const defaultBindings = (): BindingEntry[] => [
  [{ Scene: 'IaPointer' }, ['Mouse Left', 'Gamepad LeftTrigger2']],
  [{ Scene: 'IaPrimary' }, ['KeyE', 'Gamepad South']],
  [{ Scene: 'IaSecondary' }, ['KeyF', 'Gamepad East']],
  [{ Scene: 'IaForward' }, ['KeyW', 'GamepadLeft Up']],
  [{ Scene: 'IaBackward' }, ['KeyS', 'GamepadLeft Down']],
  [{ Scene: 'IaRight' }, ['KeyD', 'GamepadLeft Right']],
  [{ Scene: 'IaLeft' }, ['KeyA', 'GamepadLeft Left']],
  [{ Scene: 'IaJump' }, ['Space', 'Gamepad North']],
  [{ Scene: 'IaWalk' }, ['ControlLeft']],
  [{ Scene: 'IaModifier' }, ['ShiftLeft']],
  [{ Scene: 'IaAction3' }, ['Digit1', 'Gamepad DPadUp']],
  [{ Scene: 'IaAction4' }, ['Digit2', 'Gamepad DPadRight']],
  [{ Scene: 'IaAction5' }, ['Digit3', 'Gamepad DPadDown']],
  [{ Scene: 'IaAction6' }, ['Digit4', 'Gamepad DPadLeft']],
  [{ System: 'CameraLock' }, ['Mouse Right', 'Gamepad RightTrigger2']],
  [{ System: 'Emote' }, ['KeyB', 'Gamepad West']],
  [{ System: 'Cancel' }, ['Escape', 'Gamepad Select']],
  [{ System: 'HideUi' }, ['KeyU']],
  [{ System: 'ShowDebugUi' }, ['PageUp']],
  [{ System: 'HideNames' }, ['KeyN']],
  [{ System: 'RollLeft' }, []],
  [{ System: 'RollRight' }, []],
  [{ System: 'PhotoMode' }, ['KeyC']],
  [{ System: 'TakePhoto' }, ['Space', 'Gamepad South']],
  [{ System: 'PhotoCameraUp' }, ['KeyR', 'Gamepad DPadUp']],
  [{ System: 'PhotoCameraDown' }, ['KeyF', 'Gamepad DPadDown']],
  [{ System: 'PhotoTiltLeft' }, ['KeyQ', 'Gamepad DPadLeft']],
  [{ System: 'PhotoTiltRight' }, ['KeyE', 'Gamepad DPadRight']],
  [{ System: 'PointAt' }, ['Mouse Middle', 'KeyQ']],
  [{ System: 'Microphone' }, ['KeyV']],
  [{ System: 'Map' }, ['KeyM', 'Tab']],
  [{ System: 'Chat' }, ['Enter', 'NumpadEnter']],
  [{ System: 'CameraUp' }, ['ArrowUp']],
  [{ System: 'CameraDown' }, ['ArrowDown']],
  [{ System: 'CameraLeft' }, ['ArrowLeft']],
  [{ System: 'CameraRight' }, ['ArrowRight']],
  [{ System: 'CameraZoomIn' }, ['MouseWheel Up', 'Gamepad LeftTrigger']],
  [{ System: 'CameraZoomOut' }, ['MouseWheel Down', 'Gamepad RightTrigger']],
  [{ System: 'ScrollUp' }, ['MouseWheel Up', 'GamepadLeft Up']],
  [{ System: 'ScrollDown' }, ['MouseWheel Down', 'GamepadLeft Down']],
  [{ System: 'ScrollLeft' }, ['MouseWheel Left', 'GamepadLeft Left']],
  [{ System: 'ScrollRight' }, ['MouseWheel Right', 'GamepadLeft Right']],
  [{ System: 'PointerUp' }, ['GamepadRight Up']],
  [{ System: 'PointerDown' }, ['GamepadRight Down']],
  [{ System: 'PointerLeft' }, ['GamepadRight Left']],
  [{ System: 'PointerRight' }, ['GamepadRight Right']],
  ...Array.from({ length: 10 }, (_, slot): BindingEntry => [{ System: `QuickEmote${slot}` }, [`Numpad${slot}`]]),
  [{ System: 'Places' }, ['KeyZ']],
  [{ System: 'Events' }, ['KeyX']],
  [{ System: 'Communities' }, ['KeyO']],
  [{ System: 'Backpack' }, ['KeyI']],
  [{ System: 'Gallery' }, ['KeyG']],
  [{ System: 'Settings' }, ['KeyP']],
  [{ System: 'Friends' }, ['KeyL']],
  [{ System: 'ChatPanel' }, ['KeyT']]
]

// Saved profile edits, in order. Applied over the mock's stock passport so an edit made in ?mock=1
// sticks for the rest of the session — the real bridge gets the same effect by folding the save
// into its catalyst cache.
const ownEdits: ProfileEdit[] = []
/** The returning mock user holds two NAMEs; a guest holds none. */
const ownedNamesOf = (o: MockOptions): string[] => (o.hasPreviousLogin ? ['Mojito', 'MojitoDCL'] : [])
const ownProfile = (o: MockOptions): Profile =>
  ownEdits.reduce<Profile>(
    (p, edit) => applyProfileEdit(p, edit, ownedNamesOf(o)),
    richProfile(o.userId, o.hasPreviousLogin ? 'Mojito' : 'Guest#beef', !o.hasPreviousLogin)
  )

interface MockOptions {
  /** Simulate a returning user (reuse-login flow) vs a fresh user. */
  hasPreviousLogin: boolean
  /** Simulated user id for the returning-user case. */
  userId: string
  /** Latency for simulated calls, ms. */
  latency: number
  /** Show the welcome page after sign-in, as for a new account on an install without the terms. */
  welcome: boolean
}

const DEFAULTS: MockOptions = {
  hasPreviousLogin:
    new URLSearchParams(location.search).get('previousLogin') === '1',
  userId: '0xmock00000000000000000000000000000000beef',
  latency: 600,
  welcome: new URLSearchParams(location.search).get('welcome') === '1'
}

let running: (() => void) | null = null

let mockPublicReels: ((address: string) => GalleryPhoto[]) | null = null

export async function mockPublicReelPage(
  address: string,
  offset: number,
  limit: number
): Promise<{ ok: boolean; photos: GalleryPhoto[]; total: number; max: number }> {
  const all = mockPublicReels?.(address) ?? []
  await new Promise((resolve) => setTimeout(resolve, 150))
  return { ok: true, photos: all.slice(offset, offset + limit), total: all.length, max: 500 }
}

// Idempotent: StrictMode runs the HUD's useMemo twice, and a second bridge would answer every message again.
export function startMockBridge(opts: Partial<MockOptions> = {}): () => void {
  if (running) return running
  const o = { ...DEFAULTS, ...opts }
  const mockNearbyMuted = new Set<string>([MOCK_NEARBY[1].address.toLowerCase()])
  // Stateful so markNotificationsRead persists across reopens (like the real service).
  const mockNow = 1_700_000_000_000
  // Shapes mirror the real notifications service: friendship notifications carry the other user
  // under metadata.sender (name + avatar), with no metadata.title.
  let mockPolled = false
  const mockNotifications: AppNotification[] = [
    { id: 'n1', type: 'social_service_friendship_accepted', read: false, timestamp: new Date(mockNow).toISOString(), metadata: { sender: { address: '0x5854cce95d5e25817b41f4c41f06b695a83bc495', name: 'Mojito', profileImageUrl: 'https://profile-images.decentraland.org/entities/bafkreid5btlh76opew65hxu6dtkdo6ybqhymdof6vrrmjy2p5a74oy4huq/face.png' } } },
    { id: 'n2', type: 'social_service_friendship_request', read: false, timestamp: new Date(mockNow - 1800_000).toISOString(), metadata: { sender: { address: '0x6723dcb07f3ca735223cd1c0acfa62dd994a1bb4', name: 'Sharknado', profileImageUrl: 'https://profile-images.decentraland.org/entities/bafkreie5bpho47gnh3jrfxoezwc4pxffup4cmmhmdxsmpf3oslopxb4enm/face.png' } } },
    { id: 'n3', type: 'item_sold', read: false, timestamp: new Date(mockNow - 3600_000).toISOString(), metadata: { title: 'Item sold', description: 'Your “Sunset Hoodie” sold for 120 MANA.', image: 'https://profile-images.decentraland.org/entities/bafkreie5bpho47gnh3jrfxoezwc4pxffup4cmmhmdxsmpf3oslopxb4enm/face.png' } },
    { id: 'n4', type: 'event_started', read: true, timestamp: new Date(mockNow - 86_400_000).toISOString(), metadata: { title: 'Event live', description: 'Music Festival is happening now in Genesis Plaza.' } },
    // Title-less types (server sends only structured fields) — formatted client-side in the panel.
    { id: 'n5', type: 'community_post_added', read: false, timestamp: new Date(mockNow - 2400_000).toISOString(), metadata: { communityName: 'Toxic Events', communityId: 'c5', thumbnailUrl: 'https://picsum.photos/seed/dcl/80/80' } },
    { id: 'n10', type: 'community_voice_chat_started', read: false, timestamp: new Date(mockNow - 600_000).toISOString(), metadata: { communityId: 'c1', communityName: 'Decentraland Foundation', thumbnailUrl: 'https://picsum.photos/seed/dcl/80/80' } },
    { id: 'n6', type: 'credits_reminder_claim_credits', read: false, timestamp: new Date(mockNow - 5400_000).toISOString(), metadata: {} }
  ]
  // 230 photos a day apart, so the grid pages across several month headers. dateTime is a
  // unix-seconds string, like the real compact endpoint.
  const DAY = 86_400_000
  const mockGallery = Array.from({ length: 230 }, (_, i) => ({
    id: `g${i}`,
    url: `https://picsum.photos/seed/reel${i}/1200/800`,
    thumbnailUrl: `https://picsum.photos/seed/reel${i}/400/300`,
    isPublic: i % 3 !== 0,
    dateTime: String(Math.floor((mockNow - i * DAY) / 1000))
  }))
  mockPublicReels = (address) =>
    address.toLowerCase() === o.userId.toLowerCase()
      ? mockGallery.filter((p) => p.isPublic)
      : Array.from({ length: 40 }, (_, i) => ({
          id: `${address.slice(2, 8)}-${i}`,
          url: `https://picsum.photos/seed/${address}${i}/1200/800`,
          thumbnailUrl: `https://picsum.photos/seed/${address}${i}/400/300`,
          isPublic: true,
          dateTime: String(Math.floor((mockNow - i * 2 * DAY) / 1000))
        }))
  let mockPhotoMode: PhotoModeSnapshot = {
    active: false,
    hudHidden: false,
    cursorLocked: true,
    phase: 'idle',
    storageCurrent: 0,
    storageMax: 500,
    storageLoaded: true,
    event: 'none',
    eventSeq: 0,
    previewJpegBase64: '',
    message: ''
  }
  // Stateful saved outfits so save/delete persist across reopens (mirrors the real store).
  const mockOutfits: OutfitsMetadata = {
    outfits: [
      { slot: 0, outfit: mockOutfit(mockWearables.slice(0, 5).map((w) => w.urn)) },
      { slot: 2, outfit: mockOutfit(mockWearables.slice(5, 10).map((w) => w.urn)) }
    ],
    namesForExtraSlots: []
  }
  // The randomize bar's looks, as equipped catalog urns.
  const mockUndo: string[][] = []
  const mockRedo: string[][] = []
  const mockLook = (): string[] => mockWearables.filter((w) => w.equipped).map((w) => w.urn)
  const wearMock = (urns: string[]): void => {
    const set = new Set(urns)
    for (const w of mockWearables) w.equipped = set.has(w.urn)
  }
  // Message ids as the engine makes them: the sender and a timestamp.
  let mockClock = 46000
  const messageId = (sender: string): string => `${sender.toLowerCase()}:${(mockClock += 0.001).toFixed(3)}`
  // DM history per partner, as the engine's local store would hold it; one conversation is seeded,
  // and one community chat (stored the same way, under `community:<id>`).
  const mojito = '0x5854cce95d5e25817b41f4c41f06b695a83bc495'
  const dmStores = new Map<string, DmHistoryEntry[]>([
    [
      mojito,
      [
        { from: mojito, message: 'you around later?', receivedAt: Date.now() - 86_400_000, messageId: messageId(mojito), reactions: [] },
        {
          from: o.userId,
          message: 'yeah, after 8',
          receivedAt: Date.now() - 86_000_000,
          messageId: messageId(o.userId),
          reactions: [{ emoji: '\u{1f44d}', from: [mojito] }]
        }
      ]
    ],
    [
      'community:c1',
      [
        { from: mojito, message: 'welcome everyone to the Foundation chat', receivedAt: Date.now() - 3_600_000, messageId: messageId(mojito), reactions: [{ emoji: '\u{1f44b}', from: [o.userId] }] },
        { from: o.userId, message: 'hi all!', receivedAt: Date.now() - 3_500_000, messageId: messageId(o.userId), reactions: [] }
      ]
    ]
  ])
  // The communities the mock user is in, as chat channels; who is online in one.
  const memberNames = ['DCLOfficial', 'KazeNoKai', 'METAWOLF', 'laurenmae', 'Eax', 'Thund', 'aixa', 'Kimbo', 'BayBackner', 'Rochyou', 'Soultasium', 'franfranfran', 'olavra', 'Meshroom']
  const memberAddr = (i: number): string => `0x${(i + 1).toString(16).padStart(40, '0')}`
  const communityChannels = (): CommunityChannel[] =>
    mockCommunities.filter((c) => c.role === 'owner' || c.role === 'moderator' || c.role === 'member').map((c) => ({ id: c.id, name: c.name, thumbnail: c.thumbnail ?? '', role: c.role }))
  const onlineMembers = (): CommunityMember[] => [
    // the mock user is in the server's list too: the HUD must not count them
    { address: o.userId, name: 'You', role: 'member' },
    { address: mojito, name: 'Mojito', role: 'moderator', picture: 'https://picsum.photos/seed/mojito/64/64' },
    ...memberNames.slice(0, 5).map((name, i) => ({ address: memberAddr(i), name, role: i === 0 ? 'owner' : 'member', picture: `https://picsum.photos/seed/${name}/64/64` }))
  ]
  const dmStore = (address: string): DmHistoryEntry[] => {
    const key = address.toLowerCase()
    let store = dmStores.get(key)
    if (store == null) {
      store = []
      dmStores.set(key, store)
    }
    return store
  }
  const ch = new BroadcastChannel(bridgeChannelName())
  const wait = (ms: number): Promise<void> =>
    new Promise((r) => setTimeout(r, ms))

  const reply = (msg: SceneToPage): void => {
    ch.postMessage({ to: 'page', msg } satisfies Envelope)
  }

  // ?mockportables=1: the portables the mock engine lists. The jetpack runs until the player stops it
  // (then it can be restarted); the pet is paused by the scene; the scene's portable failed to load.
  const portables: PortableInfo[] = [
    { hash: 'bafkmockjetpack-sw-male', pid: 'urn:decentraland:matic:collections-v2:0xmock:0', name: 'Jetpack', kind: 'smartWearable', state: 'running', reason: null, source: 'equipped' },
    { hash: 'bafkmockpet-sw-male', pid: 'urn:decentraland:matic:collections-v2:0xmock:1', name: 'Pocket Pet', kind: 'smartWearable', state: 'blocked', reason: null, source: 'equipped' },
    { hash: 'bafkmockminigame', pid: 'minigame.dcl.eth', name: 'Minigame', kind: 'scene', state: 'failed', reason: 'scene.json could not be loaded', source: null }
  ]
  const sendPortables = (): void => reply({ kind: 'portables', portables: portables.map((p) => ({ ...p })) })
  // A stopped smart wearable is not loaded, so it has no scene hash until it runs again.
  const loadedHash = new Map(portables.map((p) => [p.pid, p.hash]))
  const setPortableState = (find: (p: PortableInfo) => boolean, state: PortableState): boolean => {
    const p = portables.find(find)
    if (p == null) return false
    p.state = state
    p.hash = state === 'stopped' ? null : (loadedHash.get(p.pid) ?? null)
    sendPortables()
    return true
  }

  // An in-flight loginNew approval (so loginCancel/logout can abort it).
  let pendingAuth: { id: string; timer: ReturnType<typeof setTimeout> } | null = null
  // Drives the mock pose stream (see spawnPlayer) so the minimap is developable without an engine.
  let poseTimer: ReturnType<typeof setInterval> | null = null

  // No engine in mock mode, so stand in for the engine's input stream: relay DOM key edges as
  // the systemAction messages the real bridge sends from getSystemActionStream, looked up from
  // the (rebindable) mock binding table by PHYSICAL code — e.code, matching engine KeyCodes.
  // The page applies its own guards (focused input, popups, phase), same as with the engine.
  let mockBindings = defaultBindings()
  // The pending press-a-key capture (null = none). Captured before the systemAction relay,
  // mirroring the engine's BindInput priority which suppresses the action stream.
  let captureId: string | null = null
  // Declared HUD text focus (the 'uiFocus' message): while a HUD text field is focused the
  // real engine resolves no actions from keys (InputPriority::UiText), so the mock relay
  // goes silent too. `ui` needs no emulation — the stream keeps flowing at that level.
  let textFocus = false
  const systemActionFor = (code: string): string | null => {
    const hit = mockBindings.find(([action, inputs]) => 'System' in action && inputs.includes(code))
    return hit != null && 'System' in hit[0] ? hit[0].System : null
  }
  const onKeyEdge = (e: KeyboardEvent, pressed: boolean): void => {
    if (e.repeat) return
    if (captureId != null) {
      if (!pressed) return
      reply({ kind: 'inputCaptured', id: captureId, input: e.code })
      captureId = null
      return
    }
    if (textFocus) return
    const action = systemActionFor(e.code)
    if (action == null) return
    reply({ kind: 'systemAction', action, pressed })
    // Mirror the bridge's chat domain: the Chat action becomes a dedicated focusChat message.
    if (action === 'Chat' && pressed) reply({ kind: 'focusChat' })
  }
  const onKeyDown = (e: KeyboardEvent): void => onKeyEdge(e, true)
  const onKeyUp = (e: KeyboardEvent): void => onKeyEdge(e, false)
  // Pointerdown + capture-phase: registered before the capture modal's suppression handler
  // (which cancels pointerdown, so derived mouse events never fire) and therefore runs first
  // (mirrors the engine capturing raw mouse input).
  const MOUSE_WIRE = ['Mouse Left', 'Mouse Middle', 'Mouse Right', 'Mouse Back', 'Mouse Forward']
  const onPointerDown = (e: PointerEvent): void => {
    if (captureId == null) return
    reply({ kind: 'inputCaptured', id: captureId, input: MOUSE_WIRE[e.button] ?? 'Mouse Left' })
    captureId = null
  }
  window.addEventListener('keydown', onKeyDown)
  window.addEventListener('keyup', onKeyUp)
  window.addEventListener('pointerdown', onPointerDown, true)

  // Simulate the engine spawning the player + loading the spawn scene after a
  // successful login: player-ready, then a scene-asset countdown, then "done".
  // Mirrors the real session domain's latch, so a page that says hello after the player spawned
  // (a reload) is re-told, as the bridge scene does.
  let playerSpawned = false
  const spawnPlayer = (): void => {
    setTimeout(() => {
      playerSpawned = true
      reply({ kind: 'event', name: 'playerReady' })
      reply({ kind: 'chatVisibility', open: true })
      // Two-step countdown then done — kept short so a throttled/backgrounded tab
      // (which clamps setTimeout) still reaches the world in ~2s instead of crawling.
      const steps = [564, 60, 0]
      const tick = (i: number): void => {
        const pending = steps[i]
        reply({
          kind: 'sceneLoading',
          state: {
            visible: pending > 0,
            realmConnected: true,
            title: 'Genesis Plaza',
            pendingAssets: pending > 0 ? pending : null
          }
        })
        if (pending > 0) setTimeout(() => tick(i + 1), 250)
      }
      tick(0)
    }, 500)

    // No engine in mock mode, so no real pose stream: walk a slow circle near Genesis
    // Plaza at the same ~20/s cadence the real bridge uses, so the minimap has something
    // to follow, centre on, and rotate with. Genesis City, so the DOM tile styles apply.
    reply({ kind: 'realmInfo', realm: 'https://realm-provider.decentraland.org/main', satelliteView: GENESIS_SATELLITE_VIEW })
    // The real bridge resolves this per parcel from the live scene list; the mock circles
    // inside one scene, so a single push is the whole story.
    reply({ kind: 'sceneInfo', title: 'Genesis Plaza', scene: 'bafkreigenesisplazahash', parcel: { x: 0, y: 0 }, realm: 'main' })
    let t = 0
    poseTimer = setInterval(() => {
      t += 0.05
      const a = t * 0.08
      const x = Math.cos(a) * 40
      const z = Math.sin(a) * 40
      // Heading = the tangent of the circle. SDK yaw is degrees about Y with 0 facing +Z.
      const yaw = (Math.atan2(-Math.sin(a), Math.cos(a)) * 180) / Math.PI
      reply({ kind: 'playerPose', x, z, yaw, camYaw: yaw })
    }, 50)

    // Fake nearby roster → drives the "Nearby · N" count + members list, and lets
    // chat bubbles resolve sender addresses to names.
    setTimeout(
      () =>
        reply({
          kind: 'members',
          members: MOCK_NEARBY
        }),
      1400
    )
    // One nearby player is talking in voice chat, so the speaking indicator is visible in ?mock=1.
    setTimeout(() => reply({ kind: 'voiceActivity', address: MOCK_NEARBY[0].address, active: true }), 1500)
    // One nearby player is already hushed, so both Hush and Hear show in ?mock=1.
    setTimeout(() => reply({ kind: 'nearbyMutes', addresses: [...mockNearbyMuted] }), 1500)

    // Voice chat: the engine's first snapshot and the live streams (one is at Genesis Plaza's parcel).
    const pushVoice = (next: Partial<VoiceChatState>): void => {
      voice = { ...voice, ...next }
      reply({ kind: 'voiceChatState', state: voice })
    }
    const voiceLater = (ms: number, next: () => void): void => {
      voiceTimers.push(setTimeout(next, ms))
    }
    setTimeout(() => {
      pushVoice({})
      reply({ kind: 'activeCommunityVoiceChats', chats: [...MOCK_ACTIVE_CHATS] })
    }, 1600)
    const setLive = (id: string, live: boolean): void => {
      const c = mockCommunities.find((m) => m.id === id)
      if (c == null) return
      const at = MOCK_ACTIVE_CHATS.findIndex((a) => a.communityId === id)
      if (live && at < 0) MOCK_ACTIVE_CHATS.push({ communityId: id, communityName: c.name, isMember: c.role !== 'none', positions: [], worlds: [], participantCount: 1, moderatorCount: 1 })
      if (!live && at >= 0) MOCK_ACTIVE_CHATS.splice(at, 1)
      reply({ kind: 'activeCommunityVoiceChats', chats: [...MOCK_ACTIVE_CHATS] })
    }
    // ?mockstream=start: Toxic Events goes live a few seconds in, with its broadcast and notification.
    if (new URLSearchParams(location.search).get('mockstream') === 'start') {
      setTimeout(() => {
        reply({ kind: 'communityVoiceUpdate', communityId: 'c5', status: 'started', createdAt: Date.now(), endedAt: null, positions: [], worlds: [], isMember: true, communityName: 'Toxic Events', communityImage: null })
        setLive('c5', true)
        const started: AppNotification = { id: 'n-stream-c5', type: 'community_voice_chat_started', read: false, timestamp: new Date().toISOString(), metadata: { communityId: 'c5', communityName: 'Toxic Events', thumbnailUrl: 'https://picsum.photos/seed/dcl/80/80' } }
        mockNotifications.unshift(started)
        reply({ kind: 'notificationsNew', notifications: [started] })
      }, 5000)
    }
    const mockCall = new URLSearchParams(location.search).get('mockcall')
    if (mockCall === 'incoming') {
      setTimeout(() => pushVoice({ status: 'receivedCall', kind: 'private', callId: 'mock-call', peer: MOCK_NEARBY[0].address }), 2000)
    }
    mockVoiceAction = (action) => {
      const community = (id: string): VoiceChatParticipant[] => [
        mockParticipant(MOCK_ME, 'You', undefined, id === 'c8' ? 'owner' : 'user', id === 'c8'),
        mockParticipant(MOCK_NEARBY[0].address, MOCK_NEARBY[0].name, MOCK_NEARBY[0].picture, 'moderator', true, { isSpeaking: true }),
        mockParticipant(MOCK_NEARBY[1].address, MOCK_NEARBY[1].name, MOCK_NEARBY[1].picture, 'user', true, { isMuted: true }),
        mockParticipant(MOCK_NEARBY[2].address, MOCK_NEARBY[2].name, MOCK_NEARBY[2].picture, 'user', false, { isRequestingToSpeak: true }),
        mockParticipant(MOCK_NEARBY[3].address, MOCK_NEARBY[3].name, undefined, 'user', false),
        mockParticipant(MOCK_NEARBY[4].address, MOCK_NEARBY[4].name, undefined, 'user', false),
        mockParticipant(MOCK_NEARBY[5].address, MOCK_NEARBY[5].name, undefined, 'user', false)
      ]
      const peerOf = (address: string): VoiceChatParticipant => {
        const known = MOCK_NEARBY.find((p) => p.address.toLowerCase() === address.toLowerCase())
        return mockParticipant(address, known?.name ?? 'Caller', known?.picture, 'none', true)
      }
      switch (action.op) {
        case 'startPrivateCall': {
          clearVoiceTimers()
          const busy = MOCK_NEARBY[2].address.toLowerCase() === action.address.toLowerCase()
          pushVoice({ ...idleVoice, status: 'startingCall', kind: 'private', peer: action.address })
          voiceLater(600, () => {
            if (busy) pushVoice({ ...idleVoice, status: 'busy', error: 'One of the users is busy calling someone else', errorSeq: voice.errorSeq + 1 })
            else pushVoice({ status: 'startedCall', callId: 'mock-call' })
          })
          if (!busy) {
            voiceLater(4000, () => pushVoice({ status: 'inCall', connection: 'connecting' }))
            voiceLater(5000, () => pushVoice({ connection: 'connected', micEnabled: true, localSpeaker: true, participants: [peerOf(action.address)] }))
          }
          break
        }
        case 'acceptCall':
          clearVoiceTimers()
          pushVoice({ status: 'startingCall' })
          voiceLater(500, () => pushVoice({ status: 'inCall', connection: 'connecting' }))
          voiceLater(1200, () => pushVoice({ connection: 'connected', micEnabled: true, localSpeaker: true, participants: [peerOf(voice.peer ?? MOCK_NEARBY[0].address)] }))
          break
        case 'rejectCall':
          clearVoiceTimers()
          pushVoice({ status: 'rejectingCall' })
          voiceLater(300, () => pushVoice({ ...idleVoice }))
          break
        case 'endCall':
        case 'leaveCall':
          clearVoiceTimers()
          pushVoice({ ...idleVoice, status: voice.kind === 'private' ? 'endingCall' : 'disconnected' })
          voiceLater(300, () => pushVoice({ ...idleVoice }))
          break
        case 'startCommunityCall':
        case 'joinCommunityCall': {
          clearVoiceTimers()
          const id = action.communityId
          const owner = id === 'c8'
          pushVoice({ ...idleVoice, status: 'startingCall', kind: 'community', communityId: id })
          voiceLater(600, () => {
            pushVoice({ status: 'inCall', connection: 'connecting', localSpeaker: owner, localRole: owner ? 'owner' : 'user' })
            // the bridge re-reads the live list on entering the stream
            if (action.op === 'startCommunityCall') setLive(id, true)
          })
          voiceLater(1400, () => pushVoice({ connection: 'connected', micEnabled: owner, participants: community(id) }))
          break
        }
        case 'endCommunityCall': {
          clearVoiceTimers()
          const id = voice.communityId
          pushVoice({ ...idleVoice })
          if (id != null) setLive(id, false)
          break
        }
        case 'requestToSpeak':
          pushVoice({ localRequestingToSpeak: action.raise })
          break
        case 'promoteSpeaker':
          pushVoice({ participants: voice.participants.map((p) => (p.address === action.address ? { ...p, isSpeaker: true, isRequestingToSpeak: false } : p)) })
          break
        case 'demoteSpeaker':
          if (action.address.toLowerCase() === MOCK_ME) pushVoice({ localSpeaker: false, micEnabled: false })
          pushVoice({ participants: voice.participants.map((p) => (p.address === action.address ? { ...p, isSpeaker: false } : p)) })
          break
        case 'rejectSpeakRequest':
          pushVoice({ participants: voice.participants.map((p) => (p.address === action.address ? { ...p, isRequestingToSpeak: false } : p)) })
          break
        case 'kickPlayer':
          pushVoice({ participants: voice.participants.filter((p) => p.address !== action.address) })
          break
        case 'setMic':
          pushVoice({ micEnabled: action.enabled && voice.localSpeaker })
          break
        case 'dismissError':
          pushVoice({ ...idleVoice })
          break
      }
    }

    // ?simhover=N seeds N world-hover prompts so the radial cursor tooltips are visible/verifiable in
    // ?mock=1 (the real engine hover stream isn't mocked). React positions them at the live DOM cursor,
    // so we only send the action list — no coordinates. One is disabled (camera-distance gated →
    // shows the camera glyph; see pointer.test.tsx for the player-distance / walking-glyph variant).
    const simHover = Number(new URLSearchParams(location.search).get('simhover') ?? 0)
    if (simHover > 0) {
      const sample = [
        { button: 0, text: 'Show Profile', enabled: true },
        { button: 1, text: 'Open', enabled: true },
        { button: 2, text: 'Inspect', enabled: true },
        { button: 8, text: 'Jump', enabled: true },
        { button: 4, text: 'Grab', enabled: false, tooFarReason: 'camera' as const },
        { button: 10, text: 'Use', enabled: true },
        { button: 11, text: 'Activate', enabled: true }
      ].slice(0, Math.min(simHover, 7))
      setTimeout(() => reply({ kind: 'hover', actions: sample }), 1500)
    }

    // Fake friends + requests for the React friends panel.
    setTimeout(
      () =>
        reply({
          kind: 'friends',
          available: true,
          friends: [
            { address: '0x5854cce95d5e25817b41f4c41f06b695a83bc495', name: 'Mojito', status: 'online', picture: 'https://profile-images.decentraland.org/entities/bafkreid5btlh76opew65hxu6dtkdo6ybqhymdof6vrrmjy2p5a74oy4huq/face.png' },
            { address: '0x6723dcb07f3ca735223cd1c0acfa62dd994a1bb4', name: 'Sharknado', status: 'online' },
            { address: '0x1e105bb213754519903788022b962fe2b9c4b263', name: 'Pravus', status: 'away' },
            { address: '0x3a1b2c4d5e6f7081920a3b4c5d6e7f8091a2b3c4', name: 'Clara#1l0u', status: 'offline' }
          ],
          received: [
            { id: 'r1', address: '0x77a0b1c2d3e4f5061728394a5b6c7d8e9f001122', name: 'SpottyGoat', message: 'Met you at the plaza!' }
          ],
          sent: [{ id: 's1', address: '0x9f8c2a1b4d6e7f0a3b5c8d9e1f2a4b6c8d0e1f23', name: 'Johnny' }],
          blocked: [],
          // The unnamed nearby player has blocked you: their passport offers no CHAT or friend button.
          blockedBy: ['0xc0ffee254729296a45a3885639ac7e10f9d54979']
        }),
      1600
    )

    // Settings snapshot for the React settings panel.
    setTimeout(() => reply({ kind: 'settings', settings: mockSettings }), 1700)

    // Drip a couple of fake nearby messages so the chat isn't empty.
    const samples = [
      ['0x5854cce95d5e25817b41f4c41f06b695a83bc495', 'gm everyone 👋'],
      ['0x6723dcb07f3ca735223cd1c0acfa62dd994a1bb4', 'anyone going to the event?'],
      ['0x1e105bb213754519903788022b962fe2b9c4b263', 'this plaza looks great'],
      ['0x6723dcb07f3ca735223cd1c0acfa62dd994a1bb4', '@Mojito meet me at 10,10']
    ]
    samples.forEach(([sender, message], i) => {
      const id = messageId(sender)
      setTimeout(() => reply({ kind: 'chat', chat: { sender, message, channel: 'Nearby', messageId: id } }), 2500 + i * 2200)
      // someone reacts to the first one
      if (i === 0) {
        const reaction = { channel: 'Nearby', messageId: id, emoji: '\u{1f44b}', from: samples[1][0], remove: false }
        setTimeout(() => reply({ kind: 'chatReaction', reaction }), 3500)
      }
    })

    // ?worldpw=1 → fire a sample world password prompt so the dialog is exercisable in the mock; a
    // wrong answer (anything but "open-sesame") re-asks with the reason, as the engine does.
    if (new URLSearchParams(location.search).get('worldpw') === '1') {
      setTimeout(() => reply({ kind: 'worldPasswordRequest', world: 'secret-garden.dcl.eth' }), 3000)
    }
    // ?mockportables=1 → the portables list, so the rail button and its popover are exercisable.
    if (new URLSearchParams(location.search).get('mockportables') === '1') sendPortables()
    // ?perm=1 → fire a sample scene permission prompt so the dialog is exercisable in the mock, and
    // a few used permissions so the rail's permissions list has something to review.
    if (new URLSearchParams(location.search).get('perm') === '1') {
      const scene = 'bafkreigenesisplazahash'
      const realm = 'https://realm-provider.decentraland.org/main'
      for (const [ty, allowed] of [['Fetch', true], ['OpenUrl', false], ['MovePlayer', true]] as const) {
        reply({ kind: 'permissionUsed', ty, sceneName: 'Genesis Plaza', scene, realm, allowed })
      }
      setTimeout(
        () =>
          reply({
            kind: 'permissionRequest',
            id: 1,
            ty: 'ChangeRealm',
            sceneName: 'Genesis Plaza',
            scene: 'bafkreigenesisplazahash',
            realm: 'https://realm-provider.decentraland.org/main',
            additional: 'Jump to DCL Kickoff Challenge?'
          }),
        3000
      )
    }
  }

  ch.onmessage = async (e: MessageEvent<Envelope>) => {
    const env = e.data
    if (env?.to !== 'scene') return
    const msg: PageToScene = env.msg

    // Answered before the simulated latency: the page holds everything else until this lands.
    if (msg.kind === 'hello') {
      reply({ kind: 'bridgeReady' })
      if (playerSpawned) reply({ kind: 'event', name: 'playerReady' })
      return
    }

    await wait(o.latency)

    if (msg.kind === 'getConnectivity' || msg.kind === 'retryConnectivity') {
      reply({ kind: 'connectivity', state: { connectionLost: false, probing: false, everConnected: playerSpawned, suppressed: !playerSpawned, rooms: [] } })
      if (msg.kind === 'retryConnectivity') reply({ kind: 'connectivityRetry', requestId: msg.requestId })
      return
    }

    if (msg.kind === 'getHomeScene') {
      reply({ kind: 'homeScene', realm: DEFAULT_REALM, parcel: { x: 0, y: 0 } })
      return
    }

    if (msg.kind === 'getWelcome') {
      reply({ kind: 'welcome', terms: o.welcome, newProfile: o.welcome, ask: msg.ask })
      return
    }
    if (msg.kind === 'acceptWelcome') {
      reply({ kind: 'welcomeAccepted', ok: true })
      return
    }
    // no catalyst looks or browser to open in the mock
    if (msg.kind === 'showLook') {
      reply({ kind: 'lookShown', look: msg.look })
      return
    }
    if (msg.kind === 'expectLook' || msg.kind === 'preloadLook' || msg.kind === 'openLegal') return
    if (msg.kind === 'openExternalUrl') {
      reply({ kind: 'externalUrlOpened', requestId: msg.requestId, ok: true })
      return
    }

    if (msg.kind === 'sendChat') {
      // Echo the local player's message back (the engine would broadcast it); a DM is stored first.
      const id = messageId(o.userId)
      if (msg.channel !== 'Nearby') dmStore(msg.channel).push({ from: o.userId, message: msg.message, receivedAt: Date.now(), messageId: id, reactions: [] })
      reply({ kind: 'chat', chat: { sender: o.userId, message: msg.message, channel: msg.channel, messageId: id } })
      return
    }

    if (msg.kind === 'sendChatReaction') {
      // Echo the local player's reaction back; a DM's is stored with it first.
      const from = o.userId.toLowerCase()
      const dm = msg.channel !== 'Nearby' ? dmStore(msg.channel).find((e) => e.messageId === msg.messageId) : undefined
      if (dm != null) {
        const group = dm.reactions.find((r) => r.emoji === msg.emoji)
        if (msg.remove) {
          if (group != null) group.from = group.from.filter((f) => f !== from)
          dm.reactions = dm.reactions.filter((r) => r.from.length > 0)
        } else if (group == null) dm.reactions.push({ emoji: msg.emoji, from: [from] })
        else if (!group.from.includes(from)) group.from.push(from)
      }
      reply({ kind: 'chatReaction', reaction: { channel: msg.channel, messageId: msg.messageId, emoji: msg.emoji, from, remove: msg.remove } })
      return
    }

    if (msg.kind === 'chatBubbleText') return // no avatars' bubbles in the mock
    if (msg.kind === 'navAction') return // no scene menus in the mock
    if (msg.kind === 'voiceChatAction') {
      mockVoiceAction?.(msg.action)
      return
    }
    if (msg.kind === 'friendAction') {
      // No social service in the mock: acknowledge so the page's flows (toasts, popups) run.
      reply({ kind: 'friendActionDone', op: msg.op, address: msg.address })
      return
    }
    // DMs: every partner is reachable, and one has a stored conversation.
    if (msg.kind === 'dmWatch') {
      if (msg.on) reply({ kind: 'dmUserState', address: msg.address, state: 'connected', online: true })
      return
    }
    if (msg.kind === 'dmHistory') {
      reply({ kind: 'dmHistory', address: msg.address, entries: dmStore(msg.address).map((e) => ({ ...e, reactions: e.reactions.map((r) => ({ ...r, from: [...r.from] })) })) })
      return
    }
    if (msg.kind === 'dmDelete') {
      dmStore(msg.address).length = 0
      return
    }
    if (msg.kind === 'getMutualFriends') {
      reply({
        kind: 'mutualFriends',
        address: msg.address,
        friends: [
          { address: '0x5854cce95d5e25817b41f4c41f06b695a83bc495', name: 'Mojito' },
          { address: '0x6723dcb07f3ca735223cd1c0acfa62dd994a1bb4', name: 'Sharknado' }
        ]
      })
      return
    }
    if (msg.kind === 'getSettings') {
      reply({ kind: 'settings', settings: mockSettings })
      return
    }
    if (msg.kind === 'setSetting') {
      const s = mockSettings.find((x) => x.name === msg.name)
      if (s) s.value = msg.value
      reply({ kind: 'settings', settings: mockSettings })
      return
    }
    if (msg.kind === 'getBindings') {
      reply({ kind: 'bindings', bindings: mockBindings })
      return
    }
    if (msg.kind === 'setBindings') {
      mockBindings = msg.bindings
      reply({ kind: 'bindings', bindings: mockBindings })
      return
    }
    if (msg.kind === 'resetBindings') {
      mockBindings = defaultBindings()
      reply({ kind: 'bindings', bindings: mockBindings })
      return
    }
    if (msg.kind === 'captureInput') {
      captureId = msg.id // resolved by the next keydown (see onKeyEdge)
      return
    }
    if (msg.kind === 'uiFocus') {
      textFocus = msg.text // see the relay above; `ui` has no observable mock effect
      return
    }
    if (msg.kind === 'getEmotes') {
      reply({ kind: 'emotes', emotes: mockEquippedEmotes() })
      return
    }
    if (msg.kind === 'triggerEmote') return // no-op in the mock
    if (msg.kind === 'equipEmote') return // no-op in the mock
    if (msg.kind === 'commitAvatar' || msg.kind === 'revertAvatar') return // no-op in the mock
    if (msg.kind === 'getWearables') {
      reply({ kind: 'wearables', equipped: equippedNow(), bodyShape: BASE_MALE, colors: mockColors, forceRender: mockForceRender })
      return
    }
    if (msg.kind === 'setForceRender') {
      mockForceRender = msg.categories
      reply({ kind: 'wearables', equipped: equippedNow(), bodyShape: BASE_MALE, colors: mockColors, forceRender: mockForceRender })
      return
    }
    if (msg.kind === 'catalogQuery') {
      if (msg.catalog !== 'wearables') {
        reply({ kind: 'catalogPage', catalog: msg.catalog, items: [], total: 0, requestId: msg.requestId })
        return
      }
      let items = mockWearables.slice()
      if (msg.category != null && msg.category !== 'all') items = items.filter((w) => w.category === msg.category)
      if (msg.search != null && msg.search !== '') items = items.filter((w) => (w.name ?? '').toLowerCase().includes(msg.search!.toLowerCase()))
      if (msg.collectiblesOnly === true) items = items.filter((w) => (w.rarity ?? 'base') !== 'base')
      if (msg.smartOnly === true) items = items.filter((w) => w.isSmart === true)
      const dir = msg.direction === 'asc' ? 1 : -1
      if (msg.orderBy === 'name') items.sort((a, b) => dir * (a.name ?? '').localeCompare(b.name ?? ''))
      else if (msg.orderBy === 'rarity') items.sort((a, b) => dir * (RARITIES.indexOf(a.rarity) - RARITIES.indexOf(b.rarity)))
      else if (msg.orderBy === 'date' && dir > 0) items.reverse()
      const total = items.length
      const start = msg.page * msg.pageSize
      reply({ kind: 'catalogPage', catalog: 'wearables', items: items.slice(start, start + msg.pageSize), total, requestId: msg.requestId })
      return
    }
    if (msg.kind === 'setAvatarColor') {
      mockColors[msg.target] = msg.color
      reply({ kind: 'wearables', equipped: equippedNow(), colors: mockColors })
      return
    }
    if (msg.kind === 'equip') {
      const set = new Set(msg.urns)
      for (const w of mockWearables) w.equipped = set.has(w.urn)
      reply({ kind: 'wearables', equipped: equippedNow(), bodyShape: BASE_MALE, colors: mockColors })
      return
    }
    if (msg.kind === 'getOutfits') {
      reply({ kind: 'outfits', metadata: mockOutfits })
      return
    }
    // One random catalog item per category; no conflicts or odds in mock mode.
    if (msg.kind === 'randomizeOutfit' || msg.kind === 'undoOutfit' || msg.kind === 'redoOutfit') {
      const [from, to] = msg.kind === 'redoOutfit' ? [mockRedo, mockUndo] : [mockUndo, mockRedo]
      if (msg.kind === 'randomizeOutfit') {
        const byCategory = new Map<string, Wearable[]>()
        for (const w of mockWearables) byCategory.set(w.category, [...(byCategory.get(w.category) ?? []), w])
        mockUndo.push(mockLook())
        mockRedo.length = 0
        wearMock([...byCategory.values()].map((ws) => ws[Math.floor(Math.random() * ws.length)].urn))
      } else {
        const target = from.pop()
        if (target == null) return
        to.push(mockLook())
        wearMock(target)
      }
      reply({ kind: 'wearables', equipped: equippedNow(), bodyShape: BASE_MALE, colors: mockColors })
      reply({ kind: 'outfitHistory', canUndo: mockUndo.length > 0, canRedo: mockRedo.length > 0, rolling: false })
      return
    }
    if (msg.kind === 'resetOutfitHistory') {
      mockUndo.length = 0
      mockRedo.length = 0
      reply({ kind: 'outfitHistory', canUndo: false, canRedo: false, rolling: false })
      return
    }
    if (msg.kind === 'saveOutfit') {
      // Capture the full current look (equippedNow includes the off-catalog item), so a saved slot
      // equals the equipped set and shows the "matches current look" marker.
      mockOutfits.outfits = mockOutfits.outfits.filter((o) => o.slot !== msg.slot)
      mockOutfits.outfits.push({ slot: msg.slot, outfit: mockOutfit(equippedNow().map((w) => w.urn)) })
      mockOutfits.outfits.sort((a, b) => a.slot - b.slot)
      reply({ kind: 'outfits', metadata: mockOutfits })
      return
    }
    if (msg.kind === 'deleteOutfit') {
      mockOutfits.outfits = mockOutfits.outfits.filter((o) => o.slot !== msg.slot)
      reply({ kind: 'outfits', metadata: mockOutfits })
      return
    }
    if (msg.kind === 'equipOutfit') {
      // Reflect the outfit's wearables as equipped so the Wearables tab updates too.
      const found = mockOutfits.outfits.find((o) => o.slot === msg.slot)
      if (found) {
        const set = new Set(found.outfit.wearables)
        for (const w of mockWearables) w.equipped = set.has(w.urn)
        reply({ kind: 'wearables', equipped: equippedNow(), bodyShape: BASE_MALE, colors: mockColors })
      }
      return
    }
    if (msg.kind === 'setNearbyMute') {
      if (msg.muted) mockNearbyMuted.add(msg.address)
      else mockNearbyMuted.delete(msg.address)
      reply({ kind: 'nearbyMutes', addresses: [...mockNearbyMuted] })
      return
    }
    if (msg.kind === 'setMic') {
      reply({ kind: 'mic', enabled: msg.enabled, available: true })
      return
    }
    if (msg.kind === 'getMap') {
      reply({ kind: 'mapState', x: 0, y: 0 })
      return
    }
    if (msg.kind === 'engineViewport') {
      // No engine in mock mode — nothing to render into the cutout.
      return
    }
    if (msg.kind === 'interactableArea') return
    if (msg.kind === 'minimapConfig') {
      // No engine in mock mode — the Camera style has nothing to render. The DOM styles
      // are unaffected: React draws those from map tiles without the scene's help.
      return
    }
    if (msg.kind === 'previewFocus') return
    // No engine in mock mode — the Shop's stage, emote try-on, turning and zoom have no avatar to show on.
    if (msg.kind === 'previewStage' || msg.kind === 'previewEmote' || msg.kind === 'previewRotate' || msg.kind === 'previewZoom') return
    if (msg.kind === 'previewAvatar') {
      // No engine in mock mode — avatar preview has nothing to render.
      return
    }
    // No realm switching in the mock: a realm change just reports its outcome, failing for a
    // `noexiste…` destination so the travel notice can be tried.
    const travelResult = (realm: string, travelId: number | undefined): void => {
      if (travelId == null) return
      if (realm.startsWith('noexiste')) reply({ kind: 'travelResult', travelId, realm, ok: false, message: 'status: 404 Not Found' })
      else reply({ kind: 'travelResult', travelId, realm, ok: true })
    }
    if (msg.kind === 'teleport') {
      reply({ kind: 'mapState', x: msg.x, y: msg.y })
      if (msg.realm != null) travelResult(msg.realm, msg.travelId)
      return
    }
    if (msg.kind === 'changeRealm') {
      travelResult(msg.realm, msg.travelId)
      return
    }
    if (msg.kind === 'worldPasswordResolve') {
      if (msg.password != null && msg.password !== 'open-sesame') {
        reply({ kind: 'worldPasswordRequest', world: 'secret-garden.dcl.eth', error: 'Incorrect password' })
      }
      return
    }
    // no engine to apply the decision in, and no editor in mock mode
    if (msg.kind === 'permissionResolve' || msg.kind === 'permissionRule' || msg.kind === 'resetPermissions' || msg.kind === 'editorScene') return
    if (msg.kind === 'getPermissionRules') {
      reply({
        kind: 'permissionRules',
        scene: msg.scene,
        realm: 'https://realm-provider.decentraland.org/main',
        rules: { scene: [{ ty: 'MovePlayer', allow: 'Allow' }], realm: [], global: [{ ty: 'OpenUrl', allow: 'Deny' }] }
      })
      return
    }
    if (msg.kind === 'getEvents') {
      // `?mockEvents=error` fails every read, to show the error state and its Retry
      const failed = new URLSearchParams(location.search).get('mockEvents') === 'error'
      setTimeout(() => {
        if (failed) reply({ kind: 'events', request: msg.request, events: [], failed: true })
        else reply({ kind: 'events', request: msg.request, events: mockEventList(msg) })
      }, 600)
      return
    }
    if (msg.kind === 'setEventAttending') {
      setMockReminder(msg.eventId, msg.attending)
      setTimeout(() => reply({ kind: 'eventAttending', eventId: msg.eventId, attending: msg.attending }), 300)
      return
    }
    if (msg.kind === 'getEventPlaces') {
      reply({ kind: 'eventPlaces', request: msg.request, places: mockEventPlaces(msg.placeIds) })
      return
    }
    if (msg.kind === 'getCommunities') {
      reply({ kind: 'communities', communities: mockMine() })
      return
    }
    if (msg.kind === 'browseCommunities') {
      // a name search on the "server", 20 a page; a search for "error" fails, to show the toast
      const term = msg.search.trim().toLowerCase()
      const found = mockCommunities.filter((c) => c.visibility !== 'unlisted' && c.name.toLowerCase().includes(term))
      const from = (msg.page - 1) * 20
      setTimeout(() => {
        if (term === 'error') reply({ kind: 'communityBrowse', search: msg.search, page: msg.page, communities: [], total: 0, failed: true })
        else reply({ kind: 'communityBrowse', search: msg.search, page: msg.page, communities: found.slice(from, from + 20).map((c) => ({ ...c })), total: found.length })
      }, 500)
      return
    }
    if (msg.kind === 'getCommunity') {
      const c = mockCommunities.find((x) => x.id === msg.id)
      if (c) reply({ kind: 'community', community: c })
      return
    }
    if (msg.kind === 'getCommunityChannels') {
      reply({ kind: 'communityChannels', communities: communityChannels() })
      // a line that mentions the mock user, and a member arriving, a while after they are in
      setTimeout(() => {
        // stored before it is relayed, as the engine does: the shown tab re-reads the store
        const line = { sender: mojito, message: `@${mentionName(o.hasPreviousLogin ? 'Mojito' : 'Guest', o.userId, o.hasPreviousLogin)} the Foundation town hall starts in 10 minutes`, channel: 'community:c1', messageId: messageId(mojito) }
        dmStore('community:c1').push({ from: line.sender, message: line.message, receivedAt: Date.now(), messageId: line.messageId, reactions: [] })
        reply({ kind: 'chat', chat: line })
      }, 6000)
      setTimeout(() => reply({ kind: 'communityPresence', communityId: 'c1', member: { address: memberAddr(7), name: memberNames[7], role: 'member' }, online: true }), 9000)
      return
    }
    if (msg.kind === 'setCommunityChannels' || msg.kind === 'playUiSound') return
    if (msg.kind === 'getCommunityOnlineMembers') {
      reply({ kind: 'communityOnlineMembers', id: msg.id, members: msg.id === 'c1' ? onlineMembers() : onlineMembers().slice(0, 3) })
      return
    }
    if (msg.kind === 'removeCommunityPlace') {
      // unlinked from this community; the card reads its places again
      const removed = mockRemovedPlaces.get(msg.id) ?? new Set<string>()
      removed.add(msg.placeId)
      mockRemovedPlaces.set(msg.id, removed)
      const reread: Envelope = { to: 'scene', msg: { kind: 'getCommunityDetail', id: msg.id } }
      setTimeout(() => void ch.onmessage?.(new MessageEvent('message', { data: reread })), 300)
      return
    }
    if (msg.kind === 'communityMemberAction') {
      // the member's new role (null: removed); a transfer also makes the user a moderator
      const changes = mockMemberChanges.get(msg.id) ?? new Map<string, string | null>()
      const role = { promote: 'moderator', demote: 'member', transfer: 'owner', kick: null, ban: null }[msg.action]
      changes.set(msg.address, role)
      // as the bridge, a ban adds the member to the Banned section and sends it again
      if (msg.action === 'ban') {
        mockSections.banned.set(msg.id, [...mockSection('banned', msg.id), { address: msg.address, name: msg.address.slice(0, 10), role: 'none', requestId: '' }])
        reply({ kind: 'communityMemberSection', id: msg.id, section: 'banned', members: mockSection('banned', msg.id), total: mockSection('banned', msg.id).length })
      }
      mockMemberChanges.set(msg.id, changes)
      const c = mockCommunities.find((x) => x.id === msg.id)
      if (msg.action === 'transfer' && c != null) {
        c.role = 'moderator'
        reply({ kind: 'community', community: { ...c } })
        reply({ kind: 'communities', communities: mockMine() })
      }
      const reread: Envelope = { to: 'scene', msg: { kind: 'getCommunityDetail', id: msg.id } }
      setTimeout(() => void ch.onmessage?.(new MessageEvent('message', { data: reread })), 300)
      return
    }
    // The Invites & Requests section: the invites, the requests the user's owned or moderated
    // communities received, and the user's own requests.
    const sendInvites = (): void => {
      // as the bridge: only private communities take join requests
      const managed = mockCommunities.filter((c) => (c.role === 'owner' || c.role === 'moderator') && c.privacy === 'private' && mockSection('requests', c.id).length > 0)
      reply({
        kind: 'communityInvites',
        invites: mockCommunities.filter((c) => c.pendingInviteId != null).map((c) => ({ ...c })),
        requestsReceived: managed.map((c) => ({ community: { ...c }, requests: mockSection('requests', c.id), total: mockSection('requests', c.id).length })),
        requestsSent: mockCommunities.filter((c) => c.pendingRequestId != null).map((c) => ({ ...c })),
        failed: []
      })
    }
    if (msg.kind === 'unbanCommunityMember') {
      mockSections.banned.set(msg.id, mockSection('banned', msg.id).filter((m) => m.address !== msg.address))
      reply({ kind: 'communityMemberSection', id: msg.id, section: 'banned', members: mockSection('banned', msg.id), total: mockSection('banned', msg.id).length })
      return
    }
    if (msg.kind === 'getCommunityInvitesCount') {
      const invites = mockCommunities.filter((c) => c.pendingInviteId != null).length
      const received = mockCommunities.filter((c) => (c.role === 'owner' || c.role === 'moderator') && c.privacy === 'private').reduce((n, c) => n + mockSection('requests', c.id).length, 0)
      setTimeout(() => reply({ kind: 'communityInvitesCount', count: invites + received }), 300)
      return
    }
    if (msg.kind === 'getCommunityInvites') {
      setTimeout(sendInvites, 400)
      return
    }
    if (msg.kind === 'getCommunityMemberSection') {
      const members = mockSection(msg.section, msg.id)
      setTimeout(() => reply({ kind: 'communityMemberSection', id: msg.id, section: msg.section, members, total: members.length }), 300)
      return
    }
    if (msg.kind === 'manageCommunityRequest') {
      const c = mockCommunities.find((x) => x.id === msg.id)
      if (msg.address == null) {
        // the user's own invite: accepting it joins
        if (c != null) {
          delete c.pendingInviteId
          if (msg.intention === 'accepted') {
            c.role = 'member'
            c.membersCount += 1
            reply({ kind: 'communityChannelAdded', community: { id: c.id, name: c.name, thumbnail: c.thumbnail ?? '', role: c.role } })
          }
          reply({ kind: 'community', community: { ...c } })
          reply({ kind: 'communities', communities: mockMine() })
        }
      } else {
        for (const section of ['requests', 'invites'] as const) {
          mockSections[section].set(msg.id, mockSection(section, msg.id).filter((m) => m.requestId !== msg.requestId))
          reply({ kind: 'communityMemberSection', id: msg.id, section, members: mockSection(section, msg.id), total: mockSection(section, msg.id).length })
        }
        // as the bridge, an accepted request reads the card's members and the community again
        if (msg.intention === 'accepted' && c != null) {
          c.membersCount += 1
          reply({ kind: 'community', community: { ...c } })
          const reread: Envelope = { to: 'scene', msg: { kind: 'getCommunityDetail', id: msg.id } }
          setTimeout(() => void ch.onmessage?.(new MessageEvent('message', { data: reread })), 300)
        }
      }
      sendInvites()
      reply({ kind: 'communityRequestSettled', id: msg.id, requestId: msg.requestId })
      return
    }
    if (msg.kind === 'getInvitableCommunities') {
      const communities = mockCommunities.filter((c) => c.role === 'owner' || c.role === 'moderator').map((c) => ({ id: c.id, name: c.name }))
      setTimeout(() => reply({ kind: 'invitableCommunities', address: msg.address, communities }), 300)
      return
    }
    if (msg.kind === 'inviteToCommunity') {
      mockSections.invites.set(msg.id, [...mockSection('invites', msg.id), { address: msg.address, name: msg.address.slice(0, 8), role: 'none', requestId: `rq-${Date.now()}` }])
      setTimeout(() => reply({ kind: 'communityInviteSent', id: msg.id, address: msg.address, ok: true }), 300)
      return
    }
    if (msg.kind === 'getCommunityPlaceOptions') {
      // one LAND and one World the user owns, and what each community links
      const linked = msg.communityId != null ? (mockLinkedPlaces.get(msg.communityId) ?? []) : []
      const options = { kind: 'communityPlaceOptions' as const, communityId: msg.communityId, owned: MOCK_OWNED_PLACES, linked: MOCK_OWNED_PLACES.filter((p) => linked.includes(p.id)), hiddenLinkedIds: [], linkedFailed: false, errors: [] }
      setTimeout(() => reply(options), 300)
      return
    }
    if (msg.kind === 'createCommunity') {
      // A name with "flagged" or "unavailable" in it gets the content check's two refusals, so the
      // form's moderation views can be seen without a backend.
      const lower = msg.name.toLowerCase()
      if (lower.includes('flagged') || lower.includes('unavailable')) {
        const unavailable = lower.includes('unavailable')
        const issues = { name: unavailable ? [] : ['Offensive language'], description: [], image: [] }
        setTimeout(() => reply({ kind: 'communitySaveResult', mode: 'create', moderation: { issues, unavailable } }), 600)
        return
      }
      const id = `new-${mockCommunities.length}`
      mockLinkedPlaces.set(id, msg.placeIds)
      mockCommunities.unshift({
        id,
        name: msg.name,
        description: msg.description,
        thumbnail: msg.thumbnail ?? '',
        membersCount: 1,
        role: 'owner',
        ownerName: 'You',
        privacy: msg.privacy
      })
      setTimeout(() => {
        reply({ kind: 'communitySaveResult', mode: 'create', id })
        reply({ kind: 'communities', communities: mockMine() })
      }, 600)
      return
    }
    if (msg.kind === 'deleteCommunity') {
      const at = mockCommunities.findIndex((m) => m.id === msg.id)
      if (at >= 0) mockCommunities.splice(at, 1)
      setTimeout(() => {
        reply({ kind: 'communityDeleted', id: msg.id })
        reply({ kind: 'communities', communities: mockMine() })
      }, 300)
      return
    }
    if (msg.kind === 'updateCommunity') {
      // as createCommunity: a name with "flagged" or "unavailable" gets the content check's refusals
      const lower = (msg.name ?? '').toLowerCase()
      if (lower.includes('flagged') || lower.includes('unavailable')) {
        const unavailable = lower.includes('unavailable')
        const issues = { name: unavailable ? [] : ['Offensive language'], description: [], image: [] }
        setTimeout(() => reply({ kind: 'communitySaveResult', mode: 'update', moderation: { issues, unavailable } }), 600)
        return
      }
      const c = mockCommunities.find((m) => m.id === msg.id)
      if (c != null) {
        if (msg.name != null) c.name = msg.name
        if (msg.description != null) c.description = msg.description
        if (msg.privacy != null) c.privacy = msg.privacy
        if (msg.discoverable != null) c.visibility = msg.discoverable ? 'all' : 'unlisted'
        if (msg.thumbnail != null) c.thumbnail = msg.thumbnail
        if (msg.placeIds != null) mockLinkedPlaces.set(c.id, msg.placeIds)
      }
      setTimeout(() => {
        reply({ kind: 'communitySaveResult', mode: 'update', id: msg.id })
        if (c != null) reply({ kind: 'community', community: { ...c } })
        reply({ kind: 'communities', communities: mockMine() })
      }, 600)
      return
    }
    if (msg.kind === 'joinCommunity') {
      const c = mockCommunities.find((x) => x.id === msg.id)
      if (c) {
        c.role = 'member'
        c.membersCount += 1
        reply({ kind: 'communityChannelAdded', community: { id: c.id, name: c.name, thumbnail: c.thumbnail ?? '', role: c.role } })
        reply({ kind: 'community', community: { ...c } })
      }
      reply({ kind: 'communities', communities: mockMine() })
      return
    }
    if (msg.kind === 'requestToJoinCommunity') {
      const c = mockCommunities.find((x) => x.id === msg.id)
      if (c) c.pendingRequestId = `req-${c.id}`
      reply({ kind: 'communityJoinRequest', id: msg.id, requestId: `req-${msg.id}` })
      if (c) reply({ kind: 'community', community: { ...c } })
      reply({ kind: 'communities', communities: mockMine() })
      return
    }
    if (msg.kind === 'cancelJoinRequest') {
      const c = mockCommunities.find((x) => x.id === msg.id)
      if (c) delete c.pendingRequestId
      reply({ kind: 'communityJoinRequest', id: msg.id })
      if (c) reply({ kind: 'community', community: { ...c } })
      reply({ kind: 'communities', communities: mockMine() })
      // as the bridge, the Invites & Requests section loses the cancelled request
      sendInvites()
      return
    }
    if (msg.kind === 'leaveCommunity') {
      const c = mockCommunities.find((x) => x.id === msg.id)
      if (c) {
        c.role = 'none'
        c.membersCount = Math.max(0, c.membersCount - 1)
        reply({ kind: 'communityChannelRemoved', id: c.id })
        reply({ kind: 'community', community: { ...c } })
      }
      reply({ kind: 'communities', communities: mockMine() })
      return
    }
    if (msg.kind === 'getCommunityDetail') {
      const addr = memberAddr
      const day = 24 * 60 * 60 * 1000
      reply({
        kind: 'communityDetail',
        id: msg.id,
        members: memberNames
          .map((name, i) => {
            const changed = mockMemberChanges.get(msg.id)
            const base = i === 0 ? 'owner' : i < 3 ? 'moderator' : 'member'
            // a transfer hands the owner's role over: the old owner (the first row) becomes a moderator
            const handedOver = i === 0 && [...(changed?.values() ?? [])].includes('owner')
            const role = changed?.has(addr(i)) === true ? changed.get(addr(i)) : handedOver ? 'moderator' : base
            // every third name unclaimed, so the rows show the #tag
            return role == null ? null : { address: addr(i), name, role, picture: `https://i.pravatar.cc/80?u=${name}`, hasClaimedName: i % 3 !== 2 }
          })
          .filter((m) => m != null),
        posts: [
          { id: 'p1', author: 'Kimbo', authorAddress: addr(7), authorPicture: 'https://i.pravatar.cc/80?u=Kimbo', text: 'It was TIME for an update. See you all in the new Plaza. Enjoy!', timestamp: Date.now() - 2 * day, likes: 10 },
          { id: 'p2', author: 'Kimbo', authorAddress: addr(7), authorPicture: 'https://i.pravatar.cc/80?u=Kimbo', text: 'EPIC Launch Party happening now in the theatre!', timestamp: Date.now() - 16 * day, likes: 4 },
          { id: 'p3', author: 'Kimbo', authorAddress: addr(7), authorPicture: 'https://i.pravatar.cc/80?u=Kimbo', text: 'Hey everyone! Happy Epic and Mobile Launch Day!!! Please help us spread the word and get ready to celebrate together on April 2nd! It will be EPIC.', timestamp: Date.now() - 18 * day, likes: 1 }
        ],
        // the mock user owns this one, so an owner or moderator can delete it from the card
        places: [
          { id: 'pl1', title: 'Music Festival Main Stage', thumbnail: 'https://picsum.photos/seed/mfest/420/260', positions: '-66,56', likeRate: 1, isMine: true }
        ].filter((pl) => !(mockRemovedPlaces.get(msg.id)?.has(pl.id) ?? false)),
        photos: Array.from({ length: 9 }, (_, i) => ({
          id: `ph${i}`,
          url: `https://picsum.photos/seed/dclph${i}/600/600`,
          thumbnail: `https://picsum.photos/seed/dclph${i}/300/300`
        })),
        events: [
          { id: 'e1', name: 'Watch scary movies with Cult Horror Club', thumbnail: 'https://picsum.photos/seed/horror1/200/120', startsAt: Date.now() + 1 * day },
          { id: 'e2', name: 'Pride Sound Set with Artist X', thumbnail: 'https://picsum.photos/seed/pride/200/120', startsAt: Date.now() + 1 * day + 6 * 60 * 60 * 1000 },
          { id: 'e3', name: 'Join DEEJAY FAMILY at Takeover Tuesdays', thumbnail: 'https://picsum.photos/seed/takeover/200/120', startsAt: Date.now() + 4 * day },
          { id: 'e4', name: 'PRIDE: Join Watch Party Wednesdays', thumbnail: 'https://picsum.photos/seed/watch/200/120', startsAt: Date.now() + 5 * day }
        ]
      })
      return
    }
    if (msg.kind === 'getNotifications') {
      reply({ kind: 'notifications', notifications: mockNotifications })
      // Once per session, a poll result a few seconds later, so ?mock=1 shows the new-notification
      // toasts (one at a time) and the list growing.
      if (!mockPolled) {
        mockPolled = true
        setTimeout(() => {
          const fresh: AppNotification[] = [
            { id: 'n7', type: 'community_invite_received', read: false, timestamp: new Date().toISOString(), metadata: { communityName: 'Toxic Events', communityId: 'c5', thumbnailUrl: 'https://picsum.photos/seed/dcl/80/80' } },
            { id: 'n8', type: 'social_service_friendship_request', read: false, timestamp: new Date(Date.now() - 1000).toISOString(), metadata: { sender: { address: '0x6723dcb07f3ca735223cd1c0acfa62dd994a1bb4', name: 'Sharknado' } } },
            // Opens the reward popup as well as its toast.
            { id: 'n9', type: 'reward_in_progress', read: false, timestamp: new Date(Date.now() - 2000).toISOString(), metadata: { title: 'New item unlocked', description: 'Your reward is being processed', tokenName: 'Mock Reward Hat', tokenImage: 'https://picsum.photos/seed/reward/256/256', tokenRarity: 'epic', tokenCategory: 'hat' } }
          ]
          mockNotifications.unshift(...fresh)
          reply({ kind: 'notificationsNew', notifications: fresh })
        }, 4000)
      }
      return
    }
    if (msg.kind === 'markNotificationsRead') {
      // Persist read state in the mock so reopening reflects it (mirrors the real service).
      for (const n of mockNotifications) if (msg.ids.includes(n.id)) n.read = true
      return
    }
    if (msg.kind === 'setPhotoMode') {
      mockPhotoMode = { ...mockPhotoMode, active: msg.enable, event: 'none', previewJpegBase64: '', message: '' }
      reply({ kind: 'photoMode', state: mockPhotoMode })
      return
    }
    if (msg.kind === 'takePhoto') {
      if (!mockPhotoMode.active) return
      mockPhotoMode = { ...mockPhotoMode, event: 'captured', eventSeq: mockPhotoMode.eventSeq + 1, previewJpegBase64: '', message: '' }
      reply({ kind: 'photoMode', state: mockPhotoMode })
      setTimeout(() => {
        const id = `mock-photo-${mockPhotoMode.eventSeq}`
        mockGallery.unshift({ id, url: mockGallery[0]?.url ?? '', thumbnailUrl: mockGallery[0]?.thumbnailUrl, dateTime: String(Math.floor(Date.now() / 1000)), isPublic: false })
        mockPhotoMode = {
          ...mockPhotoMode,
          event: 'uploaded',
          eventSeq: mockPhotoMode.eventSeq + 1,
          storageCurrent: mockGallery.length,
          message: id
        }
        reply({ kind: 'photoMode', state: mockPhotoMode })
      }, 800)
      return
    }
    if (msg.kind === 'getGallery') {
      const page = mockGallery.slice(msg.offset, msg.offset + msg.limit)
      setTimeout(() => reply({ kind: 'gallery', requestId: msg.requestId, ok: true, photos: page, received: page.length, current: mockGallery.length, max: 500 }), o.latency)
      return
    }
    if (msg.kind === 'getGalleryPhoto') {
      reply({
        kind: 'galleryPhoto',
        id: msg.id,
        meta: {
          userName: o.hasPreviousLogin ? 'Mojito' : 'Guest',
          userAddress: o.userId,
          sceneName: 'Genesis Plaza',
          x: -9,
          y: 14,
          realm: 'main',
          people: MOCK_NEARBY.slice(0, 3).map((m) => ({ address: m.address, name: m.name || 'Guest', isGuest: false }))
        }
      })
      return
    }
    if (msg.kind === 'deleteGalleryPhoto') {
      const i = mockGallery.findIndex((p) => p.id === msg.id)
      if (i >= 0) mockGallery.splice(i, 1)
      reply({ kind: 'galleryWrite', requestId: msg.requestId, ok: i >= 0, current: mockGallery.length, max: 500 })
      return
    }
    if (msg.kind === 'setGalleryPhotoPublic') {
      const photo = mockGallery.find((p) => p.id === msg.id)
      if (photo != null) photo.isPublic = msg.isPublic
      reply({ kind: 'galleryWrite', requestId: msg.requestId, ok: photo != null })
      return
    }
    if (msg.kind === 'getProfile') {
      reply({ kind: 'profile', profile: ownProfile(o) })
      return
    }
    if (msg.kind === 'getOwnedNames') {
      reply({ kind: 'ownedNames', names: ownedNamesOf(o) })
      return
    }
    if (msg.kind === 'saveProfile') {
      const { kind: _kind, ...edit } = msg
      // `robtfmfail` is the mock's failure switch: renaming to it exercises the error path (the
      // engine rejects a deploy the same way) without needing a broken catalyst.
      if (edit.name === 'robtfmfail') {
        reply({ kind: 'profileSaved', ok: false, error: 'failed to deploy to server.' })
        return
      }
      ownEdits.push(edit)
      reply({ kind: 'profileSaved', ok: true })
      reply({ kind: 'profile', profile: ownProfile(o) })
      return
    }
    if (msg.kind === 'getUserProfile') {
      if (msg.address.toLowerCase() === o.userId.toLowerCase()) {
        reply({ kind: 'userProfile', address: msg.address, profile: ownProfile(o) })
        return
      }
      // Resolve a real name from the nearby roster (real engine gets it from the catalyst).
      const member = MOCK_NEARBY.find((m) => m.address.toLowerCase() === msg.address.toLowerCase())
      const name = member?.name || `${msg.address.slice(0, 6)}…${msg.address.slice(-4)}`
      if (msg.extras !== true) {
        // Like the engine, hold nothing for someone never seen: a friend's seeded name stays.
        const profile = member == null ? null : { address: msg.address, name, picture: member.picture, hasClaimedName: !name.includes('#'), isGuest: false }
        reply({ kind: 'userProfile', address: msg.address, profile })
        return
      }
      reply({ kind: 'userProfile', address: msg.address, profile: richProfile(msg.address, name, false) })
      return
    }
    // Slash-command effects (no engine in mock): echo a system chat line so the flow is testable in ?mock=1.
    if (msg.kind === 'reloadScene') {
      reply({ kind: 'chat', chat: { sender: '', message: 'Reloading the current scene…', channel: 'Nearby', messageId: '' } })
      return
    }
    if (msg.kind === 'consoleCommand') {
      const args = msg.args ?? []
      // A stopped smart wearable stays listed (still equipped); any other portable leaves the list.
      if (msg.command === 'stop_portable' && args[0] != null) {
        const hash = args[0]
        const p = portables.find((e) => e.hash === hash)
        if (p == null) {
          reply({ kind: 'consoleReply', command: msg.command, args, ok: false, output: `not a listed portable: ${hash}` })
          return
        }
        if (p.kind === 'smartWearable') setPortableState((e) => e === p, 'stopped')
        else {
          portables.splice(portables.indexOf(p), 1)
          sendPortables()
        }
        reply({ kind: 'consoleReply', command: msg.command, args, ok: true, output: `stopped ${hash}` })
        return
      }
      if (msg.command === 'smart_wearable' && args[0] === 'restart' && args[1] != null) {
        const urn = args[1]
        const ok = setPortableState((e) => e.kind === 'smartWearable' && e.pid === urn && e.state === 'stopped', 'running')
        reply({ kind: 'consoleReply', command: msg.command, args, ok, output: ok ? `restarted ${urn}` : `unknown smart wearable: ${urn}` })
        return
      }
      const output =
        msg.command === 'help' && args.length === 0
          ? 'Available commands:\n  /help     - List available commands\n  /teleport - set location\n  /crdt_snapshot - (hidden by the HUD)'
          : `(mock) ran /${msg.command} ${args.join(' ')}`.trim()
      reply({ kind: 'consoleReply', command: msg.command, args, ok: true, output })
      return
    }

    switch (msg.method) {
      case 'getPreviousLogin':
        reply({
          kind: 'rpc:res',
          id: msg.id,
          ok: true,
          value: { userId: o.hasPreviousLogin ? o.userId : null }
        })
        return
      case 'loginPrevious':
        reply({
          kind: 'rpc:res',
          id: msg.id,
          ok: true,
          value: { success: true, error: '' }
        })
        spawnPlayer()
        return
      case 'loginGuest':
        reply({ kind: 'rpc:res', id: msg.id, ok: true })
        spawnPlayer()
        return
      case 'loginIdentity':
        // Same-domain SSO hand-off: the engine would finalize the wallet from the identity.
        reply({ kind: 'rpc:res', id: msg.id, ok: true })
        spawnPlayer()
        return
      case 'loginNew':
        // Remote-wallet flow: code first, then approval after a beat (long enough to see the
        // verification panel; the real flow waits on the user's external browser).
        reply({ kind: 'loginCode', code: '42' })
        pendingAuth = {
          id: msg.id,
          timer: setTimeout(() => {
            pendingAuth = null
            reply({ kind: 'rpc:res', id: msg.id, ok: true, value: { success: true, error: '' } })
            spawnPlayer()
          }, 2500)
        }
        return
      case 'loginCancel':
      case 'logout':
        // Mid-loginNew this stops the pending approval, mirroring the real flow (the engine
        // drops its login task and the relay resolves the rpc as cancelled).
        if (pendingAuth != null) {
          clearTimeout(pendingAuth.timer)
          reply({
            kind: 'rpc:res',
            id: pendingAuth.id,
            ok: true,
            value: { success: false, error: 'cancelled' }
          })
          pendingAuth = null
        }
        reply({ kind: 'rpc:res', id: msg.id, ok: true })
        return
    }
  }

  running = () => {
    running = null
    window.removeEventListener('keydown', onKeyDown)
    window.removeEventListener('keyup', onKeyUp)
    window.removeEventListener('pointerdown', onPointerDown, true)
    if (poseTimer != null) clearInterval(poseTimer)
    ch.close()
  }
  return running
}
