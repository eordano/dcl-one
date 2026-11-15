// DOMAIN: lib/audioUnlock.ts — the AudioContexts the page creates before its first gesture (the
// engine's, during the sign-in screen) are resumed by that gesture, so the sign-in / jump-in
// click is what turns sound on.

import { afterEach, describe, expect, it, vi } from 'vitest'

type FakeState = 'suspended' | 'running' | 'closed'
class FakeAudioContext {
  state: FakeState = 'suspended'
  resumed = 0
  constructor(public readonly options?: AudioContextOptions) {}
  resume(): Promise<void> {
    this.resumed += 1
    this.state = 'running'
    return Promise.resolve()
  }
}
type W = { AudioContext?: unknown }

// `null` = a browser without AudioContext (jsdom keeps a deleted window property, so it is set to
// undefined instead; and an undefined argument would just pick the default).
const load = async (native: unknown = FakeAudioContext) => {
  vi.resetModules()
  ;(window as W).AudioContext = native ?? undefined
  return import('../lib/audioUnlock')
}
const gesture = (type: string) => document.body.dispatchEvent(new Event(type, { bubbles: true }))
const context = (options?: AudioContextOptions) => new window.AudioContext(options) as unknown as FakeAudioContext
afterEach(() => {
  ;(window as W).AudioContext = undefined
})

describe('audio unlock', () => {
  it('resumes the contexts created before the first gesture, on that gesture', async () => {
    const m = await load()
    m.trackAudioContexts()
    const engine = context({ latencyHint: 'interactive' })
    expect(engine).toBeInstanceOf(FakeAudioContext)
    expect(engine.options).toEqual({ latencyHint: 'interactive' })
    expect(m.trackedAudioContexts()).toBe(1)
    expect(engine.resumed).toBe(0)
    gesture('pointerdown')
    expect(engine.resumed).toBe(1)
    expect(engine.state).toBe('running')
    gesture('keydown')
    expect(engine.resumed).toBe(1)
  })

  it('leaves running contexts alone and forgets closed ones', async () => {
    const m = await load()
    m.trackAudioContexts()
    const running = context()
    running.state = 'running'
    const closed = context()
    closed.state = 'closed'
    const suspended = context()
    expect(m.trackedAudioContexts()).toBe(3)
    gesture('touchend')
    expect(running.resumed).toBe(0)
    expect(closed.resumed).toBe(0)
    expect(suspended.resumed).toBe(1)
    expect(m.trackedAudioContexts()).toBe(2)
  })

  it('installs once, and does nothing where there is no AudioContext', async () => {
    const m = await load()
    m.trackAudioContexts()
    m.trackAudioContexts()
    expect(Object.getPrototypeOf(window.AudioContext)).toBe(FakeAudioContext)
    const none = await load(null)
    none.trackAudioContexts()
    expect((window as W).AudioContext).toBeUndefined()
    expect(() => none.unlockAudio()).not.toThrow()
  })
})
