// The web shell (index.html): the top-level page that embeds the app (app.html: HUD + engine) in
// an iframe. app.html is served with Document-Isolation-Policy, which makes it cross-origin
// isolated on its own; this page is deliberately NOT isolated, so the browser keeps the two in
// separate processes. The signed-in identity's key stays here: the app asks for the public
// identity and for signatures, and never reads the origin's localStorage itself.
import { exitUrl } from '../host'
import { authLoginUrl, authReturnUrl } from '@explorer-core/features/auth/sso'
import { ANALYTICS_SESSION_PARAM, ANON_USER_ID_PARAM, dropAttributionParams } from '@explorer-core/lib/analytics'
import { AUTH_RETURN_PARAM } from '@explorer-core/features/auth/sso'
import { migratePrefs } from '@explorer-core/lib/prefs'
import { currentLogin, request } from './login'
import { onInWorld } from '@explorer-core/shell/googleTag'
import { PAGE_DIR } from '../lib/publicUrl'
import { loadOnce } from '../lib/loadOnce'
import { shellResponse } from '@explorer-core/lib/shellReply'
import type { ShellShop, ShellShopWallet } from './shop/purchase'
import type { ShellDonate } from './donate/donation'

// Runs the web shell on Sites' /play page: mounts the app document (/play/app.html) in an iframe of its
// own process, answers its identity/sign requests, and registers the service worker that gives the
// app document its isolation header. Upstream this was the module body of react-web/src/shell/main.ts
// (index.html); `app.html` is the /play/app.html route and the service worker lives at
// /play/service_worker.js, both resolved against the page directory.
// Once per document: React's StrictMode runs the mounting effect twice in development, and a second
// mount would start a second app, with a second engine, beside the first.
let shellMounted = false

export interface PlayShellOptions {
  /** Loads the buyer's wallet for the in-world Shop's purchases, as the host page resolves it, on the
   *  app's first shop request (with the purchase code, and like it loaded again after a failed load).
   *  Without one the Shop can't buy here (its dialog says so). */
  shopWallet?: () => Promise<ShellShopWallet | null>
}

export function mountPlayShell(options: PlayShellOptions = {}): void {
  if (shellMounted) return
  shellMounted = true
  // The service worker's scope is the package DIRECTORY, but the production entry URL has no
  // trailing slash, which puts this page outside it: the worker never sees the request, so the
  // host's COEP isolates this page, and a reload keeps it that way. Navigate to the directory form
  // instead, where the worker serves the page.
  const outOfScope = !location.pathname.endsWith('/') && !location.pathname.endsWith('.html')
  if (outOfScope) location.replace(location.pathname + '/' + location.search + location.hash)

  // Where the sign-in redirect returns to: this page as it was entered, before the attribution
  // params were taken off the address bar (and the engine's url sync rewrote the rest), so the
  // return lands with everything the entry had.
  const entryUrl = location.href

  let frame: HTMLIFrameElement | null = null
  let mounting = false
  function mount(): void {
    if (mounting) return
    mounting = true
    // the app can't read localStorage, so hand it whatever an earlier version kept there first
    void migratePrefs()
      .catch((e: unknown) => console.warn('[shell] could not move stored preferences:', e))
      .then(mountApp)
  }

  // Segment's anonymous id for this browser, the cookie decentraland.org's pages set: the app
  // stamps it on its analytics so the warehouse joins the session to the landing visit.
  function segmentAnonymousId(): string | null {
    const raw = document.cookie
      .split('; ')
      .find((c) => c.startsWith('ajs_anonymous_id='))
      ?.slice('ajs_anonymous_id='.length)
    if (!raw) return null
    const value = decodeURIComponent(raw)
    return value.startsWith('"') ? value.slice(1, -1) : value
  }

  const ANALYTICS_SESSION_KEY = 'dcl-analytics-session'

  function mountApp(): void {
    const f = document.createElement('iframe')
    const q = new URLSearchParams(location.search)
    // the return from the sign-in redirect continues the analytics session the app left with
    const resumed = sessionStorage.getItem(ANALYTICS_SESSION_KEY)
    sessionStorage.removeItem(ANALYTICS_SESSION_KEY)
    if (resumed) q.set(ANALYTICS_SESSION_PARAM, resumed)
    const anonymous = segmentAnonymousId()
    if (anonymous && !q.has(ANON_USER_ID_PARAM)) q.set(ANON_USER_ID_PARAM, anonymous)
    f.src = 'app.html' + (q.size > 0 ? '?' + q.toString() : '') + location.hash
    f.title = 'Decentraland'
    f.allow = 'fullscreen; microphone; clipboard-read; clipboard-write; autoplay; gamepad'
    f.addEventListener('load', () => f.focus())
    document.body.appendChild(f)
    frame = f
    dropAttributionParams([AUTH_RETURN_PARAM])
  }

  // The in-world Shop's purchases (shop/purchase), loaded on the app's first shop.* request: this page
  // holds the login and the wallet, shows the buyer the purchase dialog over the app and signs.
  // Shop and Donate share the one dialog over the app: each refuses as busy while the other's is up.
  let loadedShop: ShellShop | null = null
  let loadedDonate: ShellDonate | null = null
  const shopWallet = loadOnce(async () => (await options.shopWallet?.()) ?? null)
  const shellShop: () => Promise<ShellShop> = loadOnce(() =>
    Promise.all([import('./shop/purchase'), import('./shop/overlay'), shopWallet()]).then(([purchase, overlay, wallet]) => {
      loadedShop = purchase.createShellShop({
        login: currentLogin,
        wallet,
        ui: overlay.createShopOverlay(() => frame?.focus()),
        post: (event) => frame?.contentWindow?.postMessage(event, location.origin),
        openExternal: (url) => void window.open(url, '_blank', 'noopener'),
        signIn: () => location.assign(authLoginUrl(authReturnUrl(entryUrl))),
        otherDialogOpen: () => loadedDonate?.dialogOpen() === true
      })
      return loadedShop
    })
  )
  // Donate (shell/donate), loaded on the app's first donate.* request, with the same wallet as the Shop's.
  const shellDonate: () => Promise<ShellDonate> = loadOnce(() =>
    Promise.all([import('./donate/donation'), import('./donate/overlay'), shopWallet()]).then(([donation, overlay, wallet]) => {
      loadedDonate = donation.createShellDonate({
        login: currentLogin,
        wallet,
        ui: overlay.createDonateOverlay(() => frame?.focus()),
        post: (event) => frame?.contentWindow?.postMessage(event, location.origin),
        openExternal: (url) => void window.open(url, '_blank', 'noopener'),
        otherDialogOpen: () => loadedShop?.dialogOpen() === true
      })
      return loadedDonate
    })
  )

  // Shown instead of the app when the app would share this page's process.
  function refuse(): void {
    console.error('[shell] this page is cross-origin isolated, so the app would share its process')
    const p = document.createElement('p')
    p.textContent = "Decentraland couldn't start. Reload the page to try again."
    document.body.appendChild(p)
  }

  // Mounts the app, unless this page is cross-origin isolated (the host's COOP/COEP without the
  // service worker's rewrite, or an earlier version's worker): then the app shares its process.
  function start(): void {
    if (crossOriginIsolated) refuse()
    else mount()
  }

  // Messages from the app, for things only this page can do.
  addEventListener('message', async (e: MessageEvent) => {
    const f = frame
    if (!f || e.source !== f.contentWindow || e.origin !== location.origin) return
    const m = e.data as { type?: unknown; [key: string]: unknown } | null
    if (!m || typeof m !== 'object') return
    switch (m.type) {
      case 'bevy-shell:request': {
        const method = typeof m.method === 'string' ? m.method : ''
        const response = await shellResponse(m.id, async () =>
          method.startsWith('shop.')
            ? (await shellShop()).request(method, m.params)
            : method.startsWith('donate.')
              ? (await shellDonate()).request(method, m.params)
              : request(m.method, m.params)
        )
        f.contentWindow?.postMessage(response, location.origin)
        break
      }
      // the engine's URL sync (engine/boot.js set_url_params), mirrored into the address bar
      case 'bevy-shell:url':
        if (typeof m.search === 'string' && (m.search === '' || m.search[0] === '?')) {
          history.replaceState(null, '', location.pathname + m.search + location.hash)
        }
        break
      // sign-in: the same-origin auth site, returning here
      case 'bevy-shell:auth-login':
        if (typeof m.session === 'string') sessionStorage.setItem(ANALYTICS_SESSION_KEY, m.session)
        location.assign(authLoginUrl(authReturnUrl(entryUrl)))
        break
      // the session's first move in world: the Google Ads conversion (googleTag.ts), and GA4's ids
      // back to the app for Segment
      case 'bevy-shell:in-world':
        if (f.contentWindow) onInWorld(m, f.contentWindow)
        break
      // the untrusted-link gate's exit (library/ui/apps/hud/features/gate/UntrustedLaunchGate.tsx), to the
      // host's fixed address
      case 'bevy-shell:exit':
        window.close()
        location.replace(exitUrl())
        break
    }
  })

  // The service worker gives app.html its isolation header and strips the host's COEP from this
  // page (deploy/web/service_worker.js). A first visit or a hard reload bypasses it, and an earlier
  // version's worker isolates this page, so reload ONCE when the current one is active; the flag
  // stops a broken worker from reload-looping. Until then the app is not mounted.
  const FLAG = 'coi_sw_reloaded'
  if (outOfScope) {
    // leaving for the in-scope url
  } else if ((window as { credentialless?: boolean }).credentialless) {
    // no sign-in key to keep from the app here (see inShell in src/lib/shell.ts)
    location.replace('app.html' + location.search + location.hash)
  } else if (!('serviceWorker' in navigator)) {
    start()
  } else {
    // whether THIS load went through the current worker: one that claims the page mid-load
    // (clients.claim on a first visit) didn't rewrite its headers, so it still needs the reload
    const ready = navigator.serviceWorker.controller != null && !crossOriginIsolated
    if (ready) {
      sessionStorage.removeItem(FLAG)
      mount()
    }
    navigator.serviceWorker
      .register(new URL('service_worker.js', PAGE_DIR))
      .then(async (reg) => {
        if (ready) return
        // registering again doesn't check for a newer worker than an earlier version's
        await reg.update().catch(() => {})
        const reloadOnce = (): void => {
          if (sessionStorage.getItem(FLAG)) {
            sessionStorage.removeItem(FLAG)
            console.error('[shell] service worker failed to take control after reload')
            start()
          } else {
            sessionStorage.setItem(FLAG, 'true')
            location.reload()
          }
        }
        // a new worker (a first visit, or an update to an earlier version's) takes over once active
        const pending = reg.installing ?? reg.waiting
        if (pending) {
          pending.addEventListener('statechange', () => {
            if (pending.state === 'activated') reloadOnce()
            else if (pending.state === 'redundant') start()
          })
        } else if (reg.active) {
          reloadOnce()
        } else {
          start()
        }
      })
      .catch((e: unknown) => {
        console.log('[shell] service worker registration failed:', e)
        start()
      })
  }
}
