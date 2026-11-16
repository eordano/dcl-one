import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { GalleryPage } from '../../features/gallery/GalleryPage'
import { dispatchCancelLayer } from '@explorer-core/lib/cancelLayers'
import type { GalleryState, ProfileState, ReelPage } from '../../features/session/useEngineSession'
import { fakeProfileState } from '../harness'
import type { GalleryPhoto } from '@explorer-core/engine/protocol'
import { PopupHost } from '../../design'

const profile: ProfileState = fakeProfileState({
  data: { address: '0xme', name: 'Mojito', hasClaimedName: true, isGuest: false }
})

// Two photos in different months (unix seconds) → exercises month grouping.
const FEB = '1707000000' // ~Feb 2024
const JAN = '1704600000' // ~Jan 2024
const photos: GalleryPhoto[] = [
  { id: 'p-feb', url: 'https://img/feb.jpg', thumbnailUrl: 'https://img/feb_t.jpg', dateTime: FEB, isPublic: false },
  { id: 'p-jan', url: 'https://img/jan.jpg', thumbnailUrl: 'https://img/jan_t.jpg', dateTime: JAN, isPublic: true }
]

function gallery(reel: GalleryPhoto[], over: Partial<GalleryState> = {}): GalleryState {
  return {
    open: true,
    toggle: vi.fn(),
    fetchPage: vi.fn(
      async (offset: number, limit: number): Promise<ReelPage> => ({ ok: true, photos: reel.slice(offset, offset + limit), total: reel.length, max: 500 })
    ),
    metas: {},
    loadPhoto: vi.fn(),
    setPublic: vi.fn(async () => ({ ok: true })),
    remove: vi.fn(async () => ({ ok: true, current: reel.length - 1, max: 500 })),
    ...over
  }
}

function renderPage(g: GalleryState, props: Partial<Parameters<typeof GalleryPage>[0]> = {}) {
  return render(
    <>
      <GalleryPage
        gallery={g}
        profile={profile}
        onNavigate={vi.fn()}
        onTeleport={props.onTeleport ?? vi.fn()}
        onViewProfile={props.onViewProfile ?? vi.fn()}
      />
      <PopupHost />
    </>
  )
}

// A thumbnail answers clicks and hover once its image has loaded.
async function loadThumbnails(): Promise<HTMLElement[]> {
  const buttons = await screen.findAllByRole('button', { name: 'Open photo' })
  for (const b of buttons) {
    const img = b.querySelector('img')
    if (img != null) fireEvent.load(img)
  }
  return buttons
}

beforeEach(() => {
  Object.assign(navigator, { clipboard: { writeText: vi.fn(async () => undefined) } })
})

describe('GalleryPage', () => {
  it('renders nothing when closed, and reads nothing', () => {
    const g = gallery(photos, { open: false })
    const { container } = renderPage(g)
    expect(container.querySelector('h1')).toBeNull()
    expect(g.fetchPage).not.toHaveBeenCalled()
  })

  it('shows the empty state when the reel is empty', async () => {
    renderPage(gallery([]))
    expect(await screen.findByText('There are no photos yet')).toBeInTheDocument()
  })

  it('reads the first page from offset 0, groups by month and shows storage usage', async () => {
    const g = gallery(photos)
    renderPage(g)
    expect(await screen.findByRole('heading', { name: 'February 2024' })).toBeInTheDocument()
    expect(screen.getByRole('heading', { name: 'January 2024' })).toBeInTheDocument()
    expect(screen.getAllByRole('button', { name: 'Open photo' })).toHaveLength(2)
    expect(screen.getByText('Storage 2/500 photos taken')).toBeInTheDocument()
    expect(g.fetchPage).toHaveBeenCalledWith(0, 100)
  })

  it('pages: the next 100 are read while the end is within 12 photos, until the total is loaded', async () => {
    const reel = Array.from({ length: 230 }, (_, i): GalleryPhoto => ({ id: `r${i}`, url: `https://img/${i}.jpg`, dateTime: FEB }))
    const g = gallery(reel)
    renderPage(g)
    // No layout in the test DOM, so every row counts as on screen: each page asks for the next.
    await waitFor(() => expect(g.fetchPage).toHaveBeenCalledTimes(3))
    expect(vi.mocked(g.fetchPage).mock.calls.map(([offset]) => offset)).toEqual([0, 100, 200])
    expect(await screen.findAllByRole('button', { name: 'Open photo' })).toHaveLength(230)
  })

  it('a first page that fails (even by rejecting) shows an error with Retry, not the empty reel', async () => {
    let fail = true
    const fetchPage = vi.fn(async (offset: number, limit: number): Promise<ReelPage> => {
      if (fail) throw new Error('bridge gone')
      return { ok: true, photos: photos.slice(offset, offset + limit), total: photos.length, max: 500 }
    })
    renderPage(gallery(photos, { fetchPage }))
    expect(await screen.findByText("Your photos couldn't be loaded")).toBeInTheDocument()
    expect(screen.queryByText('There are no photos yet')).toBeNull()
    fail = false
    await userEvent.click(screen.getByRole('button', { name: 'RETRY' }))
    expect(await screen.findAllByRole('button', { name: 'Open photo' })).toHaveLength(2)
    expect(fetchPage.mock.calls.map(([offset]) => offset)).toEqual([0, 0])
  })

  it('a later page that fails ends the grid with Retry, and paging waits for it', async () => {
    const reel = Array.from({ length: 150 }, (_, i): GalleryPhoto => ({ id: `r${i}`, url: `https://img/${i}.jpg`, dateTime: FEB }))
    let fail = true
    const fetchPage = vi.fn(async (offset: number, limit: number): Promise<ReelPage> => {
      if (offset > 0 && fail) return { ok: false, photos: [], total: 0, max: 0 }
      return { ok: true, photos: reel.slice(offset, offset + limit), total: 150, max: 500 }
    })
    renderPage(gallery(reel, { fetchPage }))
    expect(await screen.findByText("Couldn't load more photos.")).toBeInTheDocument()
    expect(screen.getAllByRole('button', { name: 'Open photo' })).toHaveLength(100)
    expect(fetchPage).toHaveBeenCalledTimes(2)
    fail = false
    await userEvent.click(screen.getByRole('button', { name: 'RETRY' }))
    await waitFor(() => expect(screen.getAllByRole('button', { name: 'Open photo' })).toHaveLength(150))
    expect(screen.queryByText("Couldn't load more photos.")).toBeNull()
    expect(fetchPage.mock.calls.map(([offset]) => offset)).toEqual([0, 100, 100])
  })

  it('a full page with photos dropped for lacking a url is not taken for the last one', async () => {
    const reel = Array.from({ length: 150 }, (_, i): GalleryPhoto => ({ id: `r${i}`, url: `https://img/${i}.jpg`, dateTime: FEB }))
    const fetchPage = vi.fn(
      async (offset: number, limit: number): Promise<ReelPage> =>
        offset === 0
          ? { ok: true, photos: reel.slice(0, 98), received: 100, total: 150, max: 500 }
          : { ok: true, photos: reel.slice(offset, offset + limit), total: 150, max: 500 }
    )
    renderPage(gallery(reel, { fetchPage }))
    await waitFor(() => expect(fetchPage).toHaveBeenCalledTimes(2))
    expect(fetchPage.mock.calls[1][0]).toBe(98)
  })

  it('a page that repeats a photo already held (the reel shifted) shows it once', async () => {
    const reel = Array.from({ length: 150 }, (_, i): GalleryPhoto => ({ id: `r${i}`, url: `https://img/${i}.jpg`, dateTime: FEB }))
    const fetchPage = vi.fn(
      async (offset: number): Promise<ReelPage> =>
        offset === 0 ? { ok: true, photos: reel.slice(0, 100), total: 150, max: 500 } : { ok: true, photos: reel.slice(99, 150), total: 150, max: 500 }
    )
    renderPage(gallery(reel, { fetchPage }))
    await waitFor(() => expect(fetchPage).toHaveBeenCalledTimes(2))
    await waitFor(() => expect(screen.getAllByRole('button', { name: 'Open photo' })).toHaveLength(150))
  })

  it('a delete while a page is on its way asks for that page again from the corrected offset', async () => {
    const reel = Array.from({ length: 230 }, (_, i): GalleryPhoto => ({ id: `r${i}`, url: `https://img/${i}.jpg`, dateTime: FEB }))
    const held: Array<(page: ReelPage) => void> = []
    const fetchPage = vi.fn((offset: number, limit: number): Promise<ReelPage> => {
      if (offset === 0) return Promise.resolve({ ok: true, photos: reel.slice(0, limit), total: 230, max: 500 })
      return new Promise((resolve) => held.push(resolve))
    })
    renderPage(gallery(reel, { fetchPage }))
    await waitFor(() => expect(fetchPage).toHaveBeenCalledTimes(2))
    await loadThumbnails()
    await userEvent.click(screen.getAllByRole('button', { name: 'Photo options' })[0])
    await userEvent.click(within(screen.getByRole('menu', { name: 'Photo options' })).getByRole('menuitem', { name: 'Delete' }))
    await userEvent.click(screen.getByRole('button', { name: 'DELETE' }))
    await waitFor(() => expect(fetchPage).toHaveBeenCalledTimes(3))
    expect(fetchPage.mock.calls.map(([offset]) => offset)).toEqual([0, 100, 99])
    // The stale page is dropped; the re-asked one lands.
    act(() => held[0]({ ok: true, photos: reel.slice(101, 201), total: 229, max: 500 }))
    act(() => held[1]({ ok: true, photos: reel.slice(100, 200), total: 229, max: 500 }))
    await waitFor(() => expect(screen.getAllByRole('button', { name: 'Open photo' })).toHaveLength(199))
    expect(screen.getAllByRole('button', { name: 'Open photo' })[99].querySelector('img')).toHaveAttribute('src', 'https://img/100.jpg')
  })

  it('closing and reopening the gallery does not reopen the photo viewer', async () => {
    const g = gallery(photos)
    const view = renderPage(g)
    const [first] = await loadThumbnails()
    await userEvent.click(first)
    expect(screen.getByRole('dialog', { name: 'Photo' })).toBeInTheDocument()
    const rerender = (open: boolean): void =>
      view.rerender(
        <>
          <GalleryPage gallery={{ ...g, open }} profile={profile} onNavigate={vi.fn()} onTeleport={vi.fn()} onViewProfile={vi.fn()} />
          <PopupHost />
        </>
      )
    rerender(false)
    rerender(true)
    await loadThumbnails()
    expect(screen.queryByRole('dialog', { name: 'Photo' })).toBeNull()
  })

  describe('with a viewport', () => {
    afterEach(() => vi.restoreAllMocks())

    it('the viewer reads the next page as it nears the loaded end', async () => {
      vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockReturnValue(800)
      vi.spyOn(HTMLElement.prototype, 'clientWidth', 'get').mockReturnValue(1900)
      const reel = Array.from({ length: 230 }, (_, i): GalleryPhoto => ({ id: `r${i}`, url: `https://img/${i}.jpg`, dateTime: FEB }))
      const g = gallery(reel)
      renderPage(g)
      const [first] = await loadThumbnails()
      expect(g.fetchPage).toHaveBeenCalledTimes(1)
      await userEvent.click(first)
      // Photo 89 leaves 11 loaded past it, under the 12-photo threshold.
      for (let i = 0; i < 89; i++) fireEvent.keyDown(window, { key: 'ArrowRight' })
      await waitFor(() => expect(g.fetchPage).toHaveBeenCalledWith(100, 100))
    })
  })

  it('opening a photo shows the detail and requests its metadata', async () => {
    const g = gallery(photos)
    renderPage(g)
    const [first] = await loadThumbnails()
    await userEvent.click(first)
    expect(screen.getByRole('dialog', { name: 'Photo' })).toBeInTheDocument()
    expect(g.loadPhoto).toHaveBeenCalledWith('p-feb')
  })

  it('a thumbnail whose image has not loaded does not open', async () => {
    renderPage(gallery(photos))
    const [first] = await screen.findAllByRole('button', { name: 'Open photo' })
    await userEvent.click(first)
    expect(screen.queryByRole('dialog', { name: 'Photo' })).toBeNull()
  })

  it('the open lightbox is a cancel layer: one Cancel dispatch closes it, none registered after', async () => {
    renderPage(gallery(photos))
    const [first] = await loadThumbnails()
    await userEvent.click(first)
    act(() => {
      expect(dispatchCancelLayer()).toBe(true)
    })
    expect(screen.queryByRole('dialog', { name: 'Photo' })).toBeNull()
    expect(dispatchCancelLayer()).toBe(false)
  })

  it('Jump In teleports to the photo parcel from its metadata', async () => {
    const onTeleport = vi.fn()
    renderPage(gallery(photos, { metas: { 'p-feb': { sceneName: 'Genesis Plaza', x: -9, y: 14 } } }), { onTeleport })
    const [first] = await loadThumbnails()
    await userEvent.click(first)
    await userEvent.click(screen.getByRole('button', { name: /Jump In/i }))
    expect(onTeleport).toHaveBeenCalledWith(-9, 14)
  })

  describe('photo options menu', () => {
    async function openMenu(at = 0): Promise<HTMLElement> {
      await loadThumbnails()
      const options = screen.getAllByRole('button', { name: 'Photo options' })
      await userEvent.click(options[at])
      return screen.getByRole('menu', { name: 'Photo options' })
    }

    it('lists the options in order, the toggle showing the photo\'s visibility', async () => {
      renderPage(gallery(photos))
      const menu = await openMenu()
      expect(within(menu).getByRole('menuitemcheckbox', { name: 'Set as Public' })).toHaveAttribute('aria-checked', 'false')
      expect(within(menu).getAllByRole('menuitem').map((b) => b.textContent)).toEqual(['Share on X', 'Copy Link', 'Download', 'Delete'])
      expect(within(menu).getByRole('separator')).toBeInTheDocument()
    })

    it('Set as Public writes the visibility, keeps the menu open and toasts', async () => {
      const g = gallery(photos)
      renderPage(g)
      const menu = await openMenu()
      await userEvent.click(within(menu).getByRole('menuitemcheckbox', { name: 'Set as Public' }))
      expect(g.setPublic).toHaveBeenCalledWith('p-feb', true)
      expect(screen.getByRole('menu', { name: 'Photo options' })).toBeInTheDocument()
      expect(within(menu).getByRole('menuitemcheckbox', { name: 'Set as Public' })).toHaveAttribute('aria-checked', 'true')
    })

    it('Copy Link copies the reel page and closes the menu', async () => {
      renderPage(gallery(photos))
      const menu = await openMenu()
      await userEvent.click(within(menu).getByRole('menuitem', { name: 'Copy Link' }))
      expect(navigator.clipboard.writeText).toHaveBeenCalledWith(expect.stringMatching(/\/reels\/p-feb$/))
      expect(screen.queryByRole('menu', { name: 'Photo options' })).toBeNull()
    })

    it('Share on X opens the post intent with its text and copies it', async () => {
      const open = vi.spyOn(window, 'open').mockReturnValue(null)
      renderPage(gallery(photos))
      const menu = await openMenu()
      await userEvent.click(within(menu).getByRole('menuitem', { name: 'Share on X' }))
      const url = open.mock.calls[0][0] as string
      expect(url.startsWith('https://x.com/intent/post?text=')).toBe(true)
      expect(decodeURIComponent(url)).toContain('Happening right now in @decentraland.\n\nCome hang out 👋')
      expect(navigator.clipboard.writeText).toHaveBeenCalledWith(url)
      open.mockRestore()
    })

    it('Delete asks first, then deletes and drops the photo', async () => {
      const g = gallery(photos)
      renderPage(g)
      const menu = await openMenu()
      await userEvent.click(within(menu).getByRole('menuitem', { name: 'Delete' }))
      expect(screen.getByText('Are you sure you want to permanently delete this photo?')).toBeInTheDocument()
      expect(g.remove).not.toHaveBeenCalled()
      await userEvent.click(screen.getByRole('button', { name: 'DELETE' }))
      expect(g.remove).toHaveBeenCalledWith('p-feb')
      await waitFor(() => expect(screen.getAllByRole('button', { name: 'Open photo' })).toHaveLength(1))
      expect(screen.getByText('Storage 1/500 photos taken')).toBeInTheDocument()
    })

    it('a failed Set as Public flips the switch back', async () => {
      renderPage(gallery(photos, { setPublic: vi.fn(async () => ({ ok: false })) }))
      const menu = await openMenu()
      const toggle = within(menu).getByRole('menuitemcheckbox', { name: 'Set as Public' })
      await userEvent.click(toggle)
      await waitFor(() => expect(toggle).toHaveAttribute('aria-checked', 'false'))
    })

    it('opening another photo\'s menu without a press outside shows that photo\'s visibility', async () => {
      renderPage(gallery(photos))
      await openMenu(0)
      expect(screen.getByRole('menuitemcheckbox', { name: 'Set as Public' })).toHaveAttribute('aria-checked', 'false')
      // A keyboard activation is a click with no mousedown, so the open menu isn't closed first.
      fireEvent.click(screen.getAllByRole('button', { name: 'Photo options' })[1])
      expect(screen.getByRole('menuitemcheckbox', { name: 'Set as Public' })).toHaveAttribute('aria-checked', 'true')
    })

    it('scrolling the grid closes the menu', async () => {
      renderPage(gallery(photos))
      await openMenu()
      fireEvent.scroll(screen.getByLabelText('Photos'))
      expect(screen.queryByRole('menu', { name: 'Photo options' })).toBeNull()
    })

    it('closing the menu puts focus back on its ⋮', async () => {
      renderPage(gallery(photos))
      await openMenu()
      const trigger = screen.getAllByRole('button', { name: 'Photo options' })[0]
      expect(trigger).not.toHaveFocus()
      act(() => {
        dispatchCancelLayer()
      })
      expect(trigger).toHaveFocus()
    })

    it('Cancel closes the menu', async () => {
      renderPage(gallery(photos))
      await openMenu()
      act(() => {
        expect(dispatchCancelLayer()).toBe(true)
      })
      expect(screen.queryByRole('menu', { name: 'Photo options' })).toBeNull()
    })
  })
})
