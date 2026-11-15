// Web Audio autoplay. A context created before the page's first user gesture is born `suspended`
// and plays nothing until resume(); Chrome also wakes it on a later source start() once the page
// has a sticky activation, Firefox and Safari never do. The engine creates its context
// (rendering/bevy-explorer, crates/av/src/audio_host_wasm.rs) as soon as EngineHost injects
// boot.js — while the sign-in screen is up, before any click — so a loop a scene starts during
// loading can stay silent for the whole session. This tracks every AudioContext the page creates
// (the engine's, livekit-client's) and resumes the suspended ones on the first gesture: the
// sign-in / jump-in click, or any pointer, key or touch before it. Engine builds from the commit
// "fix(web): resume the audio context on the first gesture" resume their own as well; this keeps
// the media libs and older play builds covered.

const GESTURES = ['pointerdown', 'keydown', 'touchend'] as const

const tracked = new Set<AudioContext>()
let installed = false

/** Replace `window.AudioContext` with a subclass that registers each instance, and resume the
 *  suspended ones on the page's first gesture. Call before the engine module (or anything else
 *  that creates a context) is injected; a second call is a no-op. */
export function trackAudioContexts(): void {
  if (installed) return
  const Native = window.AudioContext
  if (typeof Native !== 'function') return
  installed = true
  class TrackedAudioContext extends Native {
    constructor(options?: AudioContextOptions) {
      super(options)
      tracked.add(this)
    }
  }
  window.AudioContext = TrackedAudioContext
  for (const type of GESTURES) {
    document.addEventListener(type, unlockAudio, { capture: true, passive: true })
  }
}

/** Resume every tracked context the autoplay policy left suspended. Only effective inside a user
 *  gesture: the capture-phase listeners above, or a click handler. */
export function unlockAudio(): void {
  for (const context of tracked) {
    if (context.state === 'closed') tracked.delete(context)
    else if (context.state === 'suspended') context.resume().catch(() => undefined)
  }
}

export function trackedAudioContexts(): number {
  return tracked.size
}
