import { Fragment, lazy, Suspense, useEffect, useMemo, useRef, useState, type ComponentType, type ReactNode } from 'react'
import { EngineRpc } from '@explorer-core/engine/engineRpc'
import { EngineHost } from './features/engine/EngineHost'
import type { LoginDriver } from '@explorer-core/engine/driver'
import { mockPublicReelPage, startMockBridge } from '@explorer-core/engine/mockBridge'
import { setPublicReelFetcher } from './features/gallery/publicReels'
// Dev-only (?showcase=1) design gallery — lazy so it never ships in the prod HUD path.
const Showcase = lazy(() => import('./design/Showcase').then((m) => ({ default: m.Showcase })))
import { Chat } from './features/chat/Chat'
import { FriendsPanel } from './features/friends/FriendsPanel'
import { SettingsPanel } from './features/settings/SettingsPanel'
import { ProfilePanel } from './features/profile/ProfilePanel'
import { NotificationsPanel } from './features/notifications/NotificationsPanel'
import { NotificationToastHost } from './features/notifications/NotificationToast'
import { SkyboxMenu } from './features/skybox/SkyboxMenu'
import { EmotesWheel } from './features/emotes/EmotesWheel'
import { BackpackPage } from './features/backpack/BackpackPage'
import { CommunitiesPage } from './features/communities/CommunitiesPage'
import { MapPage } from './features/map/MapPage'
import { PlacesPage } from './features/places/PlacesPage'
import { EventsPage } from './features/events/EventsPage'
import { PlacesPicker } from './features/places/PlacesPicker'
import { LobbyHome } from './features/lobby/LobbyHome'
import { VoiceChatPanel } from './features/voiceChat/VoiceChatPanel'
import { SceneVoiceChatBanner } from './features/voiceChat/SceneVoiceChatBanner'
import { VoiceChatToasts } from './features/voiceChat/voiceChatToasts'
import { WelcomePage } from './features/welcome/WelcomePage'
import { GalleryPage } from './features/gallery/GalleryPage'
import { Sidebar } from './features/sidebar/Sidebar'
import { Minimap } from './features/minimap/Minimap'
import { Pointer } from './features/pointer/Pointer'
import { PhotoModeOverlay } from './features/photoMode/PhotoModeOverlay'
import { openPassport } from './features/profile/Passport'
import { openWorldVisit } from './components/WorldVisitModal'
import { openPermissionDialog } from './features/permissions/PermissionDialog'
import { openWorldPasswordDialog } from './features/worldAccess/WorldPasswordDialog'
import { PopupHost, ToastHost, showDialog } from './design'
import { SessionProvider } from './features/session/SessionContext'
import { bootMode, hostEntryParams, isNativeHud, unrecognisedEntryParams, untrustedLaunchParams } from '@hud-host'
import { FpsMeter } from './features/debug/FpsMeter'
import { BootProgress } from './features/engine/BootProgress'
import { LoadingAndLogin } from './features/login/LoadingAndLogin'
import { GuestStartPage } from './features/login/GuestStartPage'
import { SceneLoadingOverlay } from './features/session/SceneLoadingOverlay'
import { openTravelError } from './features/session/TravelError'
import { useEngineSession, type EngineSession } from './features/session/useEngineSession'
import { useWindowKeyDown } from './lib/useWindowKeyDown'
import { useHudInsetReport } from './lib/hudInset'
import { isMobile, isChromiumBased, hasBypassCookie } from '@explorer-core/lib/isMobile'
import { inShell } from '@explorer-core/lib/shell'
import { probeGpu, type GpuAdapterInfo } from '@explorer-core/lib/gpu'
import { integratedGpuOnWindows } from '@explorer-core/lib/integratedGpu'
import { getPref, PREF, setPref } from '@explorer-core/lib/prefs'
import { MobileGate, GateChecking } from './features/gate/MobileGate'
import { openIntegratedGpuGuide } from './features/gate/IntegratedGpuGuide'
import { UntrustedLaunchGate } from './features/gate/UntrustedLaunchGate'
import { ErrorBoundary } from './features/error/ErrorBoundary'
import { SurfaceBoundary } from './features/error/SurfaceBoundary'
import { CrashModal } from './features/error/CrashModal'
import { openRealmError } from './features/error/RealmErrorModal'
import { ConnectivityFeedback } from './features/error/ConnectivityFeedback'
import { DIALOG_TITLE, isDialogSource } from '@explorer-core/features/error/fatalError'
import { openEntryParamsDialog } from './features/gate/EntryParamsDialog'
import { EDITOR_BUILD, editorOffered, FEATURES_PARAM } from '@explorer-core/features/editorHost/config'
import { EditorOffered } from './features/editorHost/EditorOffered'
import { useTipEmote } from './features/donate/useDonate'
// The scene editor's host and Create page: loaded once Create is first opened. lazy() keeps a
// failed load, so a crash swaps in a fresh one for the next open.
const loadEditorHost = () => lazy(() => import('./features/editorHost/EditorHost'))
let EditorHost = /* @__PURE__ */ loadEditorHost()

const params = new URLSearchParams(location.search)
// MOCK (?mock=1): UI only, no engine, fake bridge (?previousLogin=1 → returning user).
// ENGINE (default): real engine in a same-document canvas (EngineHost — no iframe) + super-user bridge scene.
// NATIVE (?native=1): HUD in a CEF offscreen webview over the native bevy engine — a JS shim
// bridges this app's BroadcastChannel to the engine's native relay (no iframe, no mock).
const MODE: 'mock' | 'engine' | 'native' =
  params.get('mock') === '1' ? 'mock' : isNativeHud() ? 'native' : 'engine'
const SHOWCASE = params.get('showcase') === '1'
// Embedded/debug mode (?hud=0 or ?systemScene= — see lib/bootMode.ts): render ONLY the
// engine — no React HUD at all (no sidebar, chat, pointer, panels, or the sign-in /
// loading overlays). Something else owns the UI: the sites `/discover` embed's own
// chrome, or the substituted ui scene in-engine.
const HIDE_HUD = bootMode().hideHud
// Gate: don't mount the HUD/engine where the engine can't run — mobile (no WebGPU/SharedArrayBuffer)
// or a non-Chromium desktop browser (the engine bundle renders its own "Browser Not Supported" page
// there — see deploy/web/index.html — which the HUD would otherwise cover, leaving login frozen at
// 0%). `?gate=1` forces the mobile variant, `?gate=browser` the browser variant, `?gate=gpu` the
// no-GPU variant (all for testing); `?nogate=1` bypasses. The mobile/browser checks are synchronous
// (UA); the real no-GPU detection is async (see useGpuProbe) — `?gate=gpu` short-circuits it here for a deterministic gate.
function gateReason(): 'mobile' | 'browser' | 'gpu' | null {
  // Precedence: a real device constraint wins over a test override — mobile is checked before
  // `?gate=browser`, so on an actual phone that param still (correctly) yields the mobile variant.
  // Native (CEF webview over native bevy): no browser/mobile gate — the host app IS the platform.
  if (MODE === 'native') return null
  if (params.get('nogate') === '1') return null
  if (isMobile() || params.get('gate') === '1') return 'mobile'
  if (params.get('gate') === 'browser') return 'browser'
  if (params.get('gate') === 'gpu') return 'gpu'
  if (!isChromiumBased()) return 'browser'
  // inside the shell the engine's SharedArrayBuffer needs Document-Isolation-Policy (Chrome 137+)
  if (inShell && !crossOriginIsolated) return 'browser'
  return null
}
const GATE_REASON = gateReason()
// `?gpuGuide=1` shows the integrated-GPU guide (features/gate/IntegratedGpuGuide.tsx) whatever the
// machine, for a look at it (mock mode included); otherwise the probe's adapter decides.
const GPU_GUIDE_FORCED = params.get('gpuGuide') === '1'
const SAMPLE_INTEGRATED_GPU: GpuAdapterInfo = { vendor: 'intel', architecture: 'gen-12lp', device: '', description: 'Intel(R) Iris(R) Xe Graphics' }

// Gated entry-url params (lib/launchGate.ts: `?systemScene=` is a session takeover delivered as
// a link, `?baseDomain=` points every backend at another deployment) carrying a value this
// front-end doesn't recognise get an interstitial before anything boots — captured at module
// scope, from the ENTRY url, so a later history.replaceState can't retire the warning.
const UNTRUSTED_PARAMS = untrustedLaunchParams({ native: MODE === 'native' })
const EDITOR = EDITOR_BUILD && MODE === 'engine' && editorOffered(location.hostname, location.search)
// the ENTRY url: the engine's url sync rewrites it
const ENTRY_SEARCH = location.search
// Entry-url params nothing reads (lib/entryParams.ts) — told to the user once the HUD is up.
const UNRECOGNISED_PARAMS = unrecognisedEntryParams(params, [
  ...(EDITOR_BUILD && EDITOR ? ['editor', 'editor-projects', 'editor-worlds', 'editor-tag', FEATURES_PARAM] : []),
  // what the host's page acts on besides (react-web's benchmark switches, where it honours them)
  ...hostEntryParams()
])

/** The Shop page a host renders in the HUD's Shop surface. */
export interface HudShopProps {
  session: EngineSession
  onNavigate: (page: string) => void
  /** the mock bridge drives the HUD: no engine behind it */
  mock: boolean
}

/** What a host gives the HUD that its `@hud-host` module cannot: classes and components, which would
 *  load HUD code with the host module. */
export interface HudAppHost {
  /** The web engine's login driver. */
  createEngineDriver(rpc: EngineRpc): LoginDriver
  /** The bridge's login driver, for native and the mock. */
  createBridgeClient(): LoginDriver
  /** The host's Shop page. */
  ShopPage: ComponentType<HudShopProps>
  /** After the Shop surface crashed, before it opens again. */
  onShopCrash?(): void
  /** Providers around the whole HUD (/play's query client). */
  Providers?: ComponentType<{ children: ReactNode }>
}

/** The Explorer HUD: each host's App renders it with its own drivers and Shop page. */
export function HudApp(host: HudAppHost): React.JSX.Element {
  const Providers = host.Providers ?? Fragment
  const showFps = useFpsToggle()
  // Probe the GPU before booting the engine — but only on the real boot path (a sync gate already
  // decided, mock, native, or the design showcase all skip it). Native renders through the host's
  // bevy/wgpu, not WebGPU in this webview, so probing there would gate a HUD that runs fine.
  // 'checking' shows a brief spinner.
  const gpu = useGpuProbe(GATE_REASON != null || MODE === 'mock' || MODE === 'native' || SHOWCASE)
  // The one GPU finding the player can act on: a Windows browser on its integrated GPU (the
  // engine's probe path only; native renders through the host's own wgpu). Told once signed in.
  const integratedGpu = GPU_GUIDE_FORCED
    ? SAMPLE_INTEGRATED_GPU
    : MODE === 'engine' && integratedGpuOnWindows(gpu.adapter)
      ? gpu.adapter
      : null
  // Ahead of every other gate: returning here is what keeps EngineHost unmounted, and EngineHost is
  // what injects boot.js and starts the scene.
  const [trustUntrusted, setTrustUntrusted] = useState(false)
  if (UNTRUSTED_PARAMS.length > 0 && !trustUntrusted) {
    return <UntrustedLaunchGate params={UNTRUSTED_PARAMS} onProceed={() => setTrustUntrusted(true)} />
  }
  if (GATE_REASON) return <MobileGate reason={GATE_REASON} />
  if (gpu.state === 'checking') return <GateChecking />
  if (gpu.state === 'blocked') return <MobileGate reason="gpu" />
  return (
    <ErrorBoundary>
      {SHOWCASE ? (
        <Suspense fallback={null}>
          <Showcase />
        </Suspense>
      ) : (
        <Providers>
          <Hud integratedGpu={integratedGpu} host={host} />
        </Providers>
      )}
      {showFps && <FpsMeter />}
    </ErrorBoundary>
  )
}

// Perf overlay visibility: on via ?fps=1, toggle anytime with Ctrl/Cmd+Shift+F
// (works even when the engine holds keyboard focus — see useWindowKeyDown).
function useFpsToggle(): boolean {
  const [on, setOn] = useState(params.get('fps') === '1')
  useWindowKeyDown((e) => {
    if ((e.ctrlKey || e.metaKey) && e.shiftKey && (e.key === 'F' || e.key === 'f')) {
      e.preventDefault()
      setOn((v) => !v)
    }
  })
  return on
}

// Async pre-boot GPU probe. 'checking' → 'ok' (boot) or 'blocked' (→ the no-GPU gate), with what the
// adapter reports about itself once known. `skip` (a sync gate already decided, mock, native, or
// showcase) resolves immediately to 'ok'. `?nogate=1` and the shared bypass cookie skip the gate too;
// `?gate=gpu` is handled synchronously in gateReason(), so this only runs the real detection path.
type GpuProbeState = { state: 'checking' | 'ok' | 'blocked'; adapter: GpuAdapterInfo | null }
function useGpuProbe(skip: boolean): GpuProbeState {
  const [probe, setProbe] = useState<GpuProbeState>({ state: skip ? 'ok' : 'checking', adapter: null })
  useEffect(() => {
    if (skip) return
    if (params.get('nogate') === '1' || hasBypassCookie()) {
      setProbe({ state: 'ok', adapter: null })
      return
    }
    let cancelled = false
    void probeGpu().then((result) => {
      if (!cancelled) setProbe({ state: result.usable ? 'ok' : 'blocked', adapter: result.adapter })
    })
    return () => {
      cancelled = true
    }
  }, [skip])
  return probe
}

function Hud({ integratedGpu, host }: { integratedGpu: GpuAdapterInfo | null; host: HudAppHost }): React.JSX.Element {
  const { rpc, createDriver } = useMemo((): {
    rpc: EngineRpc | null
    createDriver: () => LoginDriver
  } => {
    if (MODE === 'mock') {
      startMockBridge()
      setPublicReelFetcher(mockPublicReelPage)
      return { rpc: null, createDriver: () => host.createBridgeClient() }
    }
    if (MODE === 'native') {
      // The CEF shim wires this BroadcastChannel to the native engine relay (see
      // src/lib/cefNativeBridge.ts and src/react_hud_cef.rs).
      return { rpc: null, createDriver: () => host.createBridgeClient() }
    }
    const engineRpc = new EngineRpc()
    return { rpc: engineRpc, createDriver: () => host.createEngineDriver(engineRpc) }
    // the host's drivers are fixed for the page
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const session = useEngineSession(createDriver)
  // The tip emote once the host says a tip was sent (Donate, where the host can sign one).
  useTipEmote(session.emotes.play)
  // The signed-in account the places discovery reads for its favourites, once the profile names it.
  const discoveryAccount = session.profile.data?.address
  // mounted for good once wanted: `?editor`, or Create opened
  const [editorWanted, setEditorWanted] = useState(params.has('editor'))
  if (EDITOR_BUILD && EDITOR && !editorWanted && session.create.open) setEditorWanted(true)

  // A link with params the Explorer doesn't know gets an ordinary dialog listing what was ignored
  // and what it accepts — informational, nothing is frozen behind it.
  useEffect(() => {
    if (UNRECOGNISED_PARAMS.length === 0) return
    return openEntryParamsDialog(UNRECOGNISED_PARAMS)
  }, [])

  // A Windows browser on its integrated GPU: how to move it to the dedicated one, as an ordinary
  // dialog once the player is signed in and in the lobby or the world (never over the sign-in
  // screen), once per device ("Don't show again"; "Later" asks again next launch) and once per
  // launch. Only the player closes it (or the HUD unmounting): travel that leaves the lobby or the
  // world keeps it open, which is why the close handle lives in a ref rather than the effect's
  // cleanup. Not where something else owns the UI (the embed) and not on an unattended or
  // auto-signed-in run (the benchmark, which nobody would close it on; `?guest=1` previews); always
  // when forced for a look.
  const gpuGuideDue = integratedGpu != null && (GPU_GUIDE_FORCED || session.phase === 'lobby' || session.phase === 'world')
  const gpuGuideClose = useRef<(() => void) | null>(null)
  const gpuGuideOffered = useRef(false)
  useEffect(() => {
    if (integratedGpu == null || !gpuGuideDue || gpuGuideOffered.current || HIDE_HUD) return
    if (!GPU_GUIDE_FORCED && (bootMode().autoLogin != null || getPref(PREF.gpuGuideSeen) === '1')) return
    gpuGuideOffered.current = true
    gpuGuideClose.current = openIntegratedGpuGuide({ adapter: integratedGpu, onDontShowAgain: () => setPref(PREF.gpuGuideSeen, '1') })
  }, [gpuGuideDue, integratedGpu])
  // Resetting the refs lets StrictMode's simulated unmount in dev remount with a clean slate, or
  // `?gpuGuide=1` would open on the first pass, close, and never come back.
  useEffect(
    () => () => {
      gpuGuideClose.current?.()
      gpuGuideClose.current = null
      gpuGuideOffered.current = false
    },
    []
  )

  // A world that doesn't exist, or a HUD bridge that never answered, isn't a crash — it's an ordinary
  // dialog on the popup layer, so it gets Escape/scrim-click for free and freezes nothing behind it
  // (unlike CrashModal, see inputLock). Any close path clears the session's error, so a keyboard
  // dismiss can't strand it.
  const dialogError =
    session.fatalError != null && isDialogSource(session.fatalError.source)
      ? { title: DIALOG_TITLE[session.fatalError.source], message: session.fatalError.message }
      : null
  useEffect(() => {
    if (dialogError == null) return
    return openRealmError({ ...dialogError, onDismiss: session.dismissFatal })
    // eslint-disable-next-line react-hooks/exhaustive-deps -- re-open only when the message changes
  }, [dialogError?.message])
  // Everything else IS a crash → the full-screen CrashModal (narrowed so it never sees a dialog source).
  const crash =
    session.fatalError != null && !isDialogSource(session.fatalError.source)
      ? { message: session.fatalError.message, source: session.fatalError.source }
      : null

  // Scene permission prompts (e.g. ChangeRealm), one at a time, opened through the popup layer. Escape
  // / scrim-click deny once. Re-opens whenever the front of the pending queue changes. Photo mode
  // holds them: a popup would close it, and the prompt opens when it ends.
  const permFrontId = session.phase === 'world' && !session.photoMode.active ? session.permissions.pending[0]?.id : undefined
  useEffect(() => {
    const front = session.permissions.pending[0]
    if (permFrontId == null || !front) return
    return openPermissionDialog(front, session.permissions.resolve)
    // eslint-disable-next-line react-hooks/exhaustive-deps -- re-open only when the front request changes
  }, [permFrontId])

  // A password-protected world being travelled to: the engine holds the travel until the prompt is
  // answered. Escape / scrim-click gives up (the travel is refused). Re-opens when the engine re-asks
  // after a refused answer (now with the reason).
  const worldPasswordPrompt = session.worldPassword.pending
  useEffect(() => {
    if (worldPasswordPrompt == null) return
    return openWorldPasswordDialog(worldPasswordPrompt, session.worldPassword.resolve)
    // eslint-disable-next-line react-hooks/exhaustive-deps -- re-open only when the prompt changes
  }, [worldPasswordPrompt])

  // A refused in-world travel (destination missing/unreachable): the player stays put and is told why.
  const travelError = session.travelError
  useEffect(() => {
    if (travelError == null) return
    return openTravelError(travelError, session.dismissTravelError)
    // eslint-disable-next-line react-hooks/exhaustive-deps -- dismissTravelError is stable
  }, [travelError])

  // Deploying the Backpack's look failed: try it again, or go back to the last look that deployed.
  // A deliberate choice only: Escape reverts, a stray click outside does nothing.
  const avatarSaveError = session.backpack.saveError
  useEffect(() => {
    if (avatarSaveError == null) return
    const { retrySave, revertSave } = session.backpack
    void showDialog({
      title: "Couldn't save your look",
      body: <p>{avatarSaveError}</p>,
      dismissible: false,
      actions: [
        { id: 'revert', label: 'Revert', variant: 'secondary' },
        { id: 'retry', label: 'Retry' }
      ]
    }).then((choice) => (choice === 'retry' ? retrySave() : revertSave()))
    // eslint-disable-next-line react-hooks/exhaustive-deps -- the backpack actions are stable
  }, [avatarSaveError])

  // Which tab the Backpack opens on. The emote wheel's "Customise [E]" opens it on Emotes; it resets
  // to Wearables once the Backpack closes so a normal (sidebar/topbar) open lands on Wearables.
  const [backpackTab, setBackpackTab] = useState<'wearables' | 'emotes'>('wearables')
  useEffect(() => {
    if (!session.backpack.open) setBackpackTab('wearables')
  }, [session.backpack.open])
  // Open MY OWN passport (Sidebar profile icon + the menu's "View Profile") — same popup the profile
  // card opens for others.
  const viewMyProfile = (): void => {
    const me = session.profile.data
    if (me) openPassport(me.address)
  }

  // Top-nav navigation between the full-screen menu pages (Settings/Backpack/Map)
  // and the Communities panel. Each toggle is mutually exclusive.
  const goToMenuPage = (page: string): void => {
    // the startup lobby mounts only the Backpack; other pages wait for the world
    if (session.phase === 'lobby' && page !== 'backpack') return
    if (page === 'lobby') session.lobbyPage.toggle()
    else if (page === 'settings') session.settings.toggle()
    else if (page === 'backpack') session.backpack.toggle()
    else if (page === 'communities') session.communities.toggle()
    else if (page === 'map') session.map.toggle()
    else if (page === 'places') session.places.toggle()
    else if (page === 'events') session.events.toggle(session.events.open ? undefined : 'start_menu')
    else if (page === 'shop') session.shop.toggle()
    else if (page === 'gallery') session.gallery.toggle()
    else if (page === 'create') session.create.toggle()
    // Profile-chip actions (forwarded from MainMenuShell's ProfileChip): View Profile
    // opens the full passport (same as for other users), not the small profile card.
    else if (page === 'profile') viewMyProfile()
  }

  // A full-screen MainMenuShell page is open (covers the whole HUD).
  const pageOpen =
    session.settings.open || session.backpack.open || session.communities.open || session.map.open || session.places.open || session.events.open || session.shop.open || session.gallery.open || session.lobbyPage.open || session.create.open
  // The scene editor owns the screen: no chrome in 'edit', only the reticle and prompts in 'play',
  // and no loading overlay in either (a scene reloads on every edit).
  const editorMode = session.editor.mode
  // photo mode shows only its own overlay (and the reticle) over the world
  const chrome = !pageOpen && editorMode === 'off' && !session.photoMode.active

  // Tell the engine how much of the screen the persistent HUD occupies. A full-screen page is
  // transient, so the last in-world value stands while one is open.
  useHudInsetReport(session.setInteractableArea, session.phase === 'world' && !session.menuOpen && !pageOpen)

  // Embedded mode: mount only the engine (+ the error surfaces so a crash isn't silently blank).
  // No sidebar / chat / pointer / panels / sign-in UI. PopupHost renders nothing while the stack is
  // empty, and it's what a world-not-found dialog opens into — without it that error would be invisible.
  if (HIDE_HUD) {
    return (
      <SessionProvider value={session}>
        {rpc && <EngineHost rpc={rpc} />}
        <PopupHost />
        <ToastHost />
        {/* After PopupHost: the crash surface sits above the popup layer. */}
        {crash && <CrashModal error={crash} onReload={session.reload} onDismiss={crash.source === 'runtime' ? session.dismissFatal : undefined} />}
      </SessionProvider>
    )
  }

  return (
    <SessionProvider value={session}>
      {rpc && <EngineHost rpc={rpc} />}
      {MODE === 'engine' && <BootProgress flow={session.login} />}
      {session.phase === 'login' &&
        (session.login.status === 'sign-in-or-guest' && !session.login.authPending ? (
          <GuestStartPage flow={session.login} />
        ) : (
          <LoadingAndLogin flow={session.login} />
        ))}
      {session.phase === 'welcome' && <WelcomePage setEngineViewport={session.setEngineViewport} />}
      {session.phase === 'picking' && <PlacesPicker account={discoveryAccount} onPick={session.pickDestination} />}
      {session.phase === 'lobby' && (
        <>
          <LobbyHome onPick={session.pickDestination} setEngineViewport={session.setEngineViewport} />
          <SurfaceBoundary name="Notifications" open={session.notifications.open} onCrash={session.closeAllPanels}>
            <NotificationsPanel notifications={session.notifications} friends={session.friends} voiceChat={session.voiceChat} anchor="lobby" />
          </SurfaceBoundary>
          <SurfaceBoundary name="Backpack" open={session.backpack.open} onCrash={session.closeAllPanels}>
            <BackpackPage backpack={session.backpack} emotes={session.emotes} profile={session.profile} onNavigate={goToMenuPage} setEngineViewport={session.setEngineViewport} initialTab={backpackTab} modal />
          </SurfaceBoundary>
        </>
      )}
      {session.phase === 'entering' && editorMode === 'off' && !session.create.open && (
        <SceneLoadingOverlay scene={session.sceneLoading} progress={session.loadingProgress} travellingTo={session.travellingTo} />
      )}
      {session.phase === 'world' && !session.menuOpen && (
        <EditorOffered.Provider value={EDITOR}>
          {session.lobbyPage.open && (
            <LobbyHome onPick={session.lobbyPage.travel} onClose={session.lobbyPage.toggle} setEngineViewport={session.setEngineViewport} />
          )}
          {/* The full-screen menu pages own the whole screen; hide the rail + chat so
              they don't show through (the map page's body is transparent). */}
          {session.photoMode.active && !pageOpen && (
            <PhotoModeOverlay photoMode={session.photoMode} onOpenGallery={session.gallery.toggle} />
          )}
          {chrome && <Sidebar session={session} onViewProfile={viewMyProfile} />}
          {chrome && <VoiceChatPanel />}
          {chrome && <SceneVoiceChatBanner />}
          {chrome && (
            <Minimap
              minimap={session.minimap}
              map={session.map}
              sceneTitle={session.minimap.sceneTitle}
              setEngineViewport={session.setEngineViewport}
            />
          )}
          {/* Reticle (when pointer-locked) + world-hover prompt — hidden under a full-screen page. */}
          {!pageOpen && editorMode !== 'edit' && (
            <Pointer
              hover={session.hover}
              locked={session.cursorLocked}
              proximity={session.proximity}
            />
          )}
          <Chat
            chat={session.chat}
            hidden={session.friends.open || !chrome}
            me={session.profile.data}
            onTeleport={(x, y) => session.map.teleport(x, y)}
            onVisitWorld={(name) => openWorldVisit({ worldName: name, onConfirm: () => session.map.changeRealm(name) })}
            onOpenSettings={session.settings.toggle}
          />
          <SurfaceBoundary name="Friends" open={session.friends.open} onCrash={session.closeAllPanels}>
            <FriendsPanel friends={session.friends} onMessage={session.chat.openConversation} />
          </SurfaceBoundary>
          <SurfaceBoundary name="Settings" open={session.settings.open} onCrash={session.closeAllPanels}>
            <SettingsPanel settings={session.settings} bindings={session.bindings} profile={session.profile} onNavigate={goToMenuPage} />
          </SurfaceBoundary>
          <SurfaceBoundary name="Profile" open={session.profile.open} onCrash={session.closeAllPanels}>
            <ProfilePanel profile={session.profile} />
          </SurfaceBoundary>
          <SurfaceBoundary name="Notifications" open={session.notifications.open} onCrash={session.closeAllPanels}>
            <NotificationsPanel notifications={session.notifications} friends={session.friends} voiceChat={session.voiceChat} chat={session.chat} anchor={session.lobbyPage.open ? 'lobbyInWorld' : 'rail'} />
          </SurfaceBoundary>
          <SurfaceBoundary name="Skybox" open={session.skybox.open} onCrash={session.closeAllPanels}>
            <SkyboxMenu skybox={session.skybox} />
          </SurfaceBoundary>
          <SurfaceBoundary name="Emotes" open={session.emotes.open} onCrash={session.closeAllPanels}>
            <EmotesWheel
              emotes={session.emotes}
              onCustomise={() => {
                setBackpackTab('emotes')
                session.backpack.toggle() // exclusive → closes the wheel, opens the backpack
              }}
            />
          </SurfaceBoundary>
          <SurfaceBoundary name="Backpack" open={session.backpack.open} onCrash={session.closeAllPanels}>
            <BackpackPage backpack={session.backpack} emotes={session.emotes} profile={session.profile} onNavigate={goToMenuPage} setEngineViewport={session.setEngineViewport} initialTab={backpackTab} modal={session.lobbyPage.open} />
          </SurfaceBoundary>
          <SurfaceBoundary name="Communities" open={session.communities.open} onCrash={session.closeAllPanels}>
            <CommunitiesPage
              communities={session.communities}
              profile={session.profile}
              voiceChat={session.voiceChat}
              onNavigate={goToMenuPage}
            />
          </SurfaceBoundary>
          <SurfaceBoundary name="Map" open={session.map.open} onCrash={session.closeAllPanels}>
            <MapPage map={session.map} profile={session.profile} onNavigate={goToMenuPage} />
          </SurfaceBoundary>
          <SurfaceBoundary name="Places" open={session.places.open} onCrash={session.closeAllPanels}>
            <PlacesPage
              account={discoveryAccount}
              places={session.places}
              profile={session.profile}
              onNavigate={goToMenuPage}
              onTeleport={(x, y) => session.map.teleportToPlace(x, y)}
              onVisitWorld={(realm) => session.map.changeRealm(realm)}
            />
          </SurfaceBoundary>
          <SurfaceBoundary name="Events" open={session.events.open} onCrash={session.closeAllPanels}>
            <EventsPage events={session.events} profile={session.profile} friends={session.friends} onNavigate={goToMenuPage} />
          </SurfaceBoundary>
          <SurfaceBoundary
            name="Shop"
            open={session.shop.open}
            onCrash={() => {
              host.onShopCrash?.()
              session.closeAllPanels()
            }}
          >
            <host.ShopPage session={session} onNavigate={goToMenuPage} mock={MODE === 'mock'} />
          </SurfaceBoundary>
          <SurfaceBoundary name="Gallery" open={session.gallery.open} onCrash={session.closeAllPanels}>
            <GalleryPage
              gallery={session.gallery}
              profile={session.profile}
              onNavigate={goToMenuPage}
              onTeleport={(x, y) => session.map.teleport(x, y)}
              onViewProfile={(u) => openPassport(u.address)}
            />
          </SurfaceBoundary>
        </EditorOffered.Provider>
      )}
      {EDITOR_BUILD && EDITOR && editorWanted && (
        <SurfaceBoundary
          name="Create"
          open={session.create.open}
          onCrash={() => {
            EditorHost = loadEditorHost()
            session.closeAllPanels()
          }}
        >
          <Suspense fallback={null}>
            <EditorHost session={session} entrySearch={ENTRY_SEARCH} onNavigate={goToMenuPage} />
          </Suspense>
        </SurfaceBoundary>
      )}
      {/* Popups (imperative overlay stack) live inside the session provider so popup-mounted surfaces
          — the world <ProfileCard> — can read useSession(). */}
      <PopupHost />
      <ToastHost />
      <ConnectivityFeedback state={session.connectivity.state} retry={session.connectivity.retry} exit={session.logout} />
      <NotificationToastHost notifications={session.notifications} friends={session.friends} voiceChat={session.voiceChat} chat={session.chat} />
      <VoiceChatToasts />
      {/* Crash surface (boot panic / runtime crash / react render crash) — last, so it renders above
          the popup layer. A world-not-found is not a crash and opens as a popup instead (see above). */}
      {crash && <CrashModal error={crash} onReload={session.reload} onDismiss={crash.source === 'runtime' ? session.dismissFatal : undefined} />}
    </SessionProvider>
  )
}
