import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import type { ReactElement } from 'react'
import { expectConsoleOutput } from '@explorer-core/test/console'

const { native, showToast } = vi.hoisted(() => ({ native: { value: false }, showToast: vi.fn() }))
vi.mock('@hud-host', async (original) => ({ ...(await original<object>()), isNativeHud: () => native.value }))
vi.mock('../design', async (original) => ({ ...(await original<object>()), showToast }))

const { REEL_MESSAGES, downloadReel, shareReelToX, shareToXUrl } = await import('../features/gallery/reelActions')

type Listener = (payload: unknown) => void
// The page subscribes once for its lifetime, so the listeners outlive each test.
const listeners = new Map<string, Listener>()
const hadCef = 'cef' in window

beforeEach(() => {
  native.value = false
  showToast.mockReset()
  Object.assign(navigator, { clipboard: { writeText: vi.fn(async () => undefined) } })
  vi.stubGlobal('fetch', vi.fn(async () => new Response(new Blob(['jpeg']), { status: 200 })))
  URL.createObjectURL = vi.fn(() => 'blob:cef://localhost/1')
  URL.revokeObjectURL = vi.fn()
  // jsdom cannot follow the download link
  vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
})

afterEach(() => {
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
  vi.useRealTimers()
  if (!hadCef) delete (window as { cef?: unknown }).cef
})

const toasts = (): string[] => showToast.mock.calls.map((c) => c[0] as string)

describe('shareReelToX', () => {
  it('opens a tab on web', () => {
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const external = vi.fn(async () => true)
    shareReelToX('r1', external)
    expect(open).toHaveBeenCalledWith(shareToXUrl('r1'), '_blank', 'noopener')
    expect(external).not.toHaveBeenCalled()
    open.mockRestore()
  })

  it('hands the post to the engine on native, where the HUD has no tabs', () => {
    native.value = true
    const open = vi.spyOn(window, 'open').mockReturnValue(null)
    const external = vi.fn(async () => true)
    shareReelToX('r1', external)
    expect(external).toHaveBeenCalledWith(shareToXUrl('r1'))
    expect(open).not.toHaveBeenCalled()
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith(shareToXUrl('r1'))
    open.mockRestore()
  })
})

describe('downloadReel', () => {
  it('toasts once the browser has the file on web', async () => {
    await downloadReel('https://img/feb.jpg')
    expect(toasts()).toEqual([REEL_MESSAGES.downloaded])
  })

  describe('on native', () => {
    const emit = vi.fn()
    beforeEach(() => {
      emit.mockReset()
      ;(window as { cef?: unknown }).cef = { emit, listen: (id: string, cb: Listener) => listeners.set(id, cb) }
    })

    it('waits for the engine to report the file saved', async () => {
      const done = downloadReel('https://img/feb.jpg')
      await vi.waitFor(() => expect(URL.createObjectURL).toHaveBeenCalled())
      expect(toasts()).toEqual([])
      listeners.get('download')?.(JSON.stringify({ url: 'blob:cef://localhost/1', ok: true }))
      await done
      expect(toasts()).toEqual([REEL_MESSAGES.downloaded])
      expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:cef://localhost/1')
    })

    it('the toast names the folder, which a click opens once', async () => {
      const done = downloadReel('https://img/feb.jpg')
      await vi.waitFor(() => expect(URL.createObjectURL).toHaveBeenCalled())
      listeners.get('download')?.(JSON.stringify({ url: 'blob:cef://localhost/1', ok: true, dir: '/home/ada/Downloads' }))
      await done
      expect(showToast.mock.calls[0][1]).toMatchObject({ duration: 3000 })
      render(showToast.mock.calls[0][0] as ReactElement)
      expect(screen.getByText(REEL_MESSAGES.downloaded)).toBeInTheDocument()
      const folder = screen.getByRole('button', { name: 'Open /home/ada/Downloads' })
      expect(folder).toHaveTextContent('/home/ada/Downloads')
      await userEvent.click(folder)
      await userEvent.click(folder)
      expect(emit.mock.calls).toEqual([[{ to: 'engine', kind: 'openDownloads' }]])
    })

    it('reports a save the engine refused or could not finish', async () => {
      expectConsoleOutput('warn', [['[gallery] download failed', new Error('the engine did not save the file')]])
      const done = downloadReel('https://img/feb.jpg')
      await vi.waitFor(() => expect(URL.createObjectURL).toHaveBeenCalled())
      listeners.get('download')?.(JSON.stringify({ url: 'blob:cef://localhost/1', ok: false }))
      await done
      expect(toasts()).toEqual([REEL_MESSAGES.failure])
    })

    it('ignores the report of another download, and fails when none comes', async () => {
      vi.useFakeTimers()
      expectConsoleOutput('warn', [['[gallery] download failed', new Error('the engine did not save the file')]])
      const done = downloadReel('https://img/feb.jpg')
      await vi.advanceTimersByTimeAsync(0)
      listeners.get('download')?.(JSON.stringify({ url: 'blob:cef://localhost/other', ok: true }))
      await vi.advanceTimersByTimeAsync(30_000)
      await done
      expect(toasts()).toEqual([REEL_MESSAGES.failure])
    })
  })
})
