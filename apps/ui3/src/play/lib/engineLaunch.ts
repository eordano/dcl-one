// How /play launches the web engine (the shared EngineHost reads it through `@hud-host`): every
// launch param the engine's web param table lists, read from the entry url, the services this HUD
// resolved for itself (lib/baseDomain.ts), the ui scene (our bundled bridge scene unless a link
// overrode it, or the Creator Hub editor's), and the pinned engine artifact Sites serves.
import type { EngineLaunch } from '@explorer-core/hostContract'
import { launchOptionsFromUrl } from '@explorer-core/lib/webParams'
import { trackAudioContexts } from './audioUnlock'
import { engineServiceOptions } from './baseDomain'
import { bootMode } from './bootMode'
import { EDITOR_SCENE, editorSession, isEditorScene } from './editorEmbed'
import { PAGE_DIR } from './publicUrl'
import { SYSTEM_SCENE } from './systemScene'

/** In the scene editor's viewport the session is the editor's, so the editor scene's bus channel
 *  (suffixed by the sandbox) is the one the editor page opens. */
export function seedBridgeSession(): void {
  // Before any module that creates an AudioContext loads: the engine's is born while the sign-in
  // screen is up, and the first gesture, the sign-in or jump-in click, resumes it (audioUnlock.ts).
  trackAudioContexts()
  const editor = editorSession(bootMode().systemScene, location.hash)
  if (editor != null) (window as { __bridgeSession?: string }).__bridgeSession = editor
}

export function engineLaunch(params: URLSearchParams): EngineLaunch {
  const editor = editorSession(bootMode().systemScene, location.hash)
  return {
    // `autopilot` is never handed over, on any host: /play has no benchmark run, and the key stays
    // out of the options altogether (not `false`), so an engine that predates the field still launches.
    options: {
      ...launchOptionsFromUrl(params, ['autopilot']),
      ...engineServiceOptions(),
      // the engine takes a bare value for a world name, so the editor scene goes as its full url
      systemScene: (() => {
        const scene = bootMode().systemScene
        return scene == null ? SYSTEM_SCENE : isEditorScene(scene) ? EDITOR_SCENE : scene
      })(),
      // a host option: the engine freezes each scene after its first frame until the editor plays it
      ...(editor != null ? { editor: true } : {})
    },
    // pkg/ fetch base: the pinned engine artifact Sites serves next to this page (/play/engine).
    engineDir: new URL('engine', PAGE_DIR).href
  }
}
