import { describe, it, expect, vi, afterEach } from 'vitest'
import { render, screen } from '@testing-library/react'
import { anchoredMenuPosition } from '../design'
import { ReelGrid, buildReelRows, type ReelGridLayout } from '../features/gallery/ReelGrid'
import { reelFileName, shareToXUrl } from '../features/gallery/reelActions'
import { serviceUrl } from '@hud-host'
import type { GalleryPhoto } from '@explorer-core/engine/protocol'

const LAYOUT: ReelGridLayout = {
  columns: 6,
  tileWidth: 306,
  tileHeight: 230,
  groupByMonth: true,
  paddingTop: 30,
  paddingBottom: 30,
  paddingLeft: 30,
  monthSpacing: 20,
  optionButtonHeight: 41
}
const FEB = '1707000000'
const JAN = '1704600000'
const reel = (n: number, dateTime = FEB): GalleryPhoto[] => Array.from({ length: n }, (_, i) => ({ id: `r${i}`, url: `https://img/${i}.jpg`, dateTime }))

describe('AnchoredMenu placement', () => {
  const size = { width: 220, height: 207 }
  const screenSize = { width: 1920, height: 1080 }

  it('opens 11 right of the anchor with its top 18 above it', () => {
    expect(anchoredMenuPosition({ x: 500, y: 400 }, size, screenSize)).toEqual({ left: 511, top: 382 })
  })

  it('flips to the left near the right edge and above near the bottom', () => {
    expect(anchoredMenuPosition({ x: 1800, y: 1000 }, size, screenSize)).toEqual({ left: 1800 - 11 - 220, top: 1000 + 18 - 207 })
  })

  it('clamps to the screen when flipping is not enough', () => {
    expect(anchoredMenuPosition({ x: 100, y: 5 }, size, { width: 250, height: 1080 })).toEqual({ left: 0, top: 0 })
  })
})

describe('reel rows', () => {
  it('pads, heads each month, and spaces rows 4, months 20 and the last row 0', () => {
    const photos = [...reel(7, FEB), ...reel(2, JAN).map((p) => ({ ...p, id: `j${p.id}` }))]
    const { rows, height } = buildReelRows(photos, LAYOUT, 230)
    expect(rows.map((r) => [r.kind, r.height])).toEqual([
      ['padding', 30],
      ['header', 42],
      ['thumbs', 234],
      ['thumbs', 250],
      ['header', 42],
      ['thumbs', 230],
      ['padding', 30]
    ])
    expect(height).toBe(30 + 42 + 234 + 250 + 42 + 230 + 30)
  })

  it('without month grouping is one section with no header', () => {
    const { rows } = buildReelRows(reel(5), { ...LAYOUT, columns: 4, groupByMonth: false }, 201)
    expect(rows.filter((r) => r.kind === 'header')).toHaveLength(0)
    expect(rows.filter((r) => r.kind === 'thumbs').map((r) => r.height)).toEqual([205, 201])
  })
})

describe('ReelGrid virtualization', () => {
  afterEach(() => vi.restoreAllMocks())

  it('mounts only the rows near the viewport, and reports the last photo on screen', () => {
    vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockReturnValue(800)
    vi.spyOn(HTMLElement.prototype, 'clientWidth', 'get').mockReturnValue(1900)
    const onVisibleIndex = vi.fn()
    render(<ReelGrid photos={reel(600)} layout={LAYOUT} menu={null} onOpen={vi.fn()} onVisibleIndex={onVisibleIndex} />)
    const tiles = screen.getAllByRole('button', { name: 'Open photo' })
    // 800 px of viewport + 600 overscan: 6 rows of 234 at most, out of 100.
    expect(tiles.length).toBeLessThanOrEqual(6 * 7)
    expect(tiles.length).toBeGreaterThan(0)
    // Rows 1-4 start within 800 px (30 + 42 + 3 x 234 = 774): photos 0-23.
    expect(onVisibleIndex).toHaveBeenLastCalledWith(23)
  })
})

describe('reel actions', () => {
  it('names a download after the image url\'s last segment', () => {
    expect(reelFileName('https://cdn.example/images/abc%20def.jpg?x=1')).toBe('abc def.jpg')
  })

  it('shares to the X post intent with the reel page', () => {
    const url = new URL(shareToXUrl('r1'))
    expect(url.origin + url.pathname).toBe('https://x.com/intent/post')
    expect(url.searchParams.get('text')).toBe('Happening right now in @decentraland.\n\nCome hang out 👋\n\n')
    expect(url.searchParams.get('url')).toBe(`${serviceUrl('reels')}/r1`)
  })
})
