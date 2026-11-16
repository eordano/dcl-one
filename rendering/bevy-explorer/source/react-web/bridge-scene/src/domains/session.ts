// Session: login RPC, scene-loading stream, player-ready, sidebar nav.
//   from: BevyApi (login + getSceneLoadingUIStream), @dcl/sdk getPlayer (player-ready).
import { getPlayer } from '@dcl/sdk/players'
import { openExternalUrl } from '~system/RestrictedActions'
import { bugReportUrl } from '@explorer-core/lib/bugReport'
import { BevyApi } from '../bevy-api'
import { relay } from '../system-helpers'
import { identity } from '../identity'
import type { Ctx } from '../bridge'
import { GUEST_SESSION_KEY, guestLogin, guestSign } from '@explorer-core/features/auth/thirdweb'

// The Bevy engine injects a persistent localStorage into system scenes.
declare const localStorage: {
  setItem: (key: string, value: string) => void
  removeItem: (key: string) => void
}

// thirdweb only serves origins on the project's allowlist, and the native page's cef:// origin is
// not one. The scene's fetch runs in the engine, so it can name one that is (as Godot does).
const THIRDWEB_HEADERS = { Origin: 'https://decentraland.org' }

const SIGN_INS = new Set(['loginPrevious', 'loginNew', 'loginIdentity'])

/** Only `https:`: the page asks for web pages, never a file or another app's scheme. */
async function openHttps(url: string): Promise<boolean> {
  if (!/^https:\/\//i.test(url)) {
    console.error('[session] refused to open', url)
    return false
  }
  try {
    await openExternalUrl({ url })
    return true
  } catch (e) {
    console.error('[session] open', url, e)
    return false
  }
}

export function registerSession(ctx: Ctx): void {
  // Player-spawned signal: one-shot per page, not per scene. The page gates its world-entry
  // fetches on it and never retries, so a page that arrives late is re-told on `hello`.
  let ready = false

  // Login surface (request/response by method). Most clients log in via the engine's
  // `/login_identity` console command now; this stays for channel-based callers.
  ctx.on('rpc:req', async (msg) => {
    try {
      let value: unknown
      switch (msg.method) {
        case 'getPreviousLogin': value = await BevyApi.getPreviousLogin(); break
        case 'loginPrevious': value = await BevyApi.loginPrevious(); break
        // Fresh sign-in (remote wallet): forward the mid-flight verification code as its own
        // message; the rpc itself resolves on approval (rejects on failure/cancel via catch).
        case 'loginNew': {
          const login = BevyApi.loginNew()
          void login.code
            .then((code) => { ctx.send({ kind: 'loginCode', code: code ?? null }); })
            .catch(() => undefined) // errors surface through `success` below
          await login.success
          value = { success: true, error: '' }
          break
        }
        // The engine has no "log in with a raw identity" surface; reuse the saved login instead.
        case 'loginIdentity': value = await BevyApi.loginPrevious(); break
        case 'loginGuest': BevyApi.loginGuest(); break
        case 'loginCancel': BevyApi.loginCancel(); break
        // Persistent guest (native): the page builds the identity, the thirdweb calls run here.
        case 'guestLogin': {
          const sessionId = msg.params?.sessionId ?? ''
          localStorage.setItem(GUEST_SESSION_KEY, sessionId)
          value = await guestLogin(sessionId, THIRDWEB_HEADERS)
          break
        }
        case 'guestSign': value = await guestSign(msg.params?.token ?? '', msg.params?.message ?? '', THIRDWEB_HEADERS); break
        case 'guestDiscard': localStorage.removeItem(GUEST_SESSION_KEY); break
        case 'logout': BevyApi.logout(); break
        default: throw new Error(`unsupported method ${String(msg.method)}`)
      }
      // A completed sign-in says the (possibly same) account is back and must be announced again,
      // even if no frame saw the player gone in between.
      if (SIGN_INS.has(msg.method) && (value as { success?: boolean } | undefined)?.success !== false) ready = false
      ctx.send({ kind: 'rpc:res', id: msg.id, ok: true, value })
    } catch (err) {
      ctx.send({ kind: 'rpc:res', id: msg.id, ok: false, error: String(err) })
    }
  })

  // Panels are rendered by React. The engine owns the microphone and native system browser.
  ctx.on('navAction', async (msg) => {
    if (msg.action === 'mic') {
      void BevyApi.getMicState().then((m) => {
        BevyApi.setMicEnabled(!m.enabled)
      })
    } else if (msg.action === 'support') {
      await openExternalUrl({ url: bugReportUrl() })
    }
  })

  ctx.on('openExternalUrl', async (msg) => {
    const ok = await openHttps(msg.url)
    ctx.send({ kind: 'externalUrlOpened', requestId: msg.requestId, ok })
  })

  // Scene-asset loading stream → React loading screen.
  relay('sceneLoading', async () => await BevyApi.getSceneLoadingUIStream(), (s) => {
    ctx.send({
      kind: 'sceneLoading',
      state: {
        visible: s.visible,
        realmConnected: s.realmConnected,
        title: s.title ?? '',
        pendingAssets: s.pendingAssets ?? null
      }
    })
  })

  identity.onChange(() => {
    ready = false
  })
  ctx.push(() => {
    const player = getPlayer()
    identity.observe(player?.userId ?? null)
    // the engine clears the player on logout; the next one is announced afresh
    if (player == null) ready = false
    if (ready) return
    if (player != null) {
      ready = true
      ctx.send({ kind: 'event', name: 'playerReady' })
    }
  })
  ctx.on('hello', () => {
    if (ready) ctx.send({ kind: 'event', name: 'playerReady' })
  })
}
