import { describe, it, expect, vi } from 'vitest'
import { act } from '@testing-library/react'
import type { GalleryPhoto } from '@explorer-core/engine/protocol'
import { renderSession, enterAsGuest } from '../harness'
import { BRIDGE_CALL_TIMEOUT_MS, GALLERY_DELETE_TIMEOUT_MS } from '../../features/session/useEngineSession'

// DOMAIN: gallery (camera reel) — paged reads, lazy per-photo metadata, visibility and delete writes,
// each answered by the requestId it was sent with.
describe('gallery domain', () => {
  const photo = (id: string, dateTime: string): GalleryPhoto => ({
    id,
    url: `https://img/${id}.jpg`,
    thumbnailUrl: `https://img/${id}_t.jpg`,
    dateTime
  })

  it('opening the gallery sends nothing by itself: the page asks for its pages', async () => {
    const h = renderSession()
    await enterAsGuest(h)
    act(() => h.session().gallery.toggle())
    expect(h.session().gallery.open).toBe(true)
    expect(h.driver.sentOf('getGallery')).toHaveLength(0)
  })

  it('fetchPage sends offset + limit and resolves with the matching reply, in service order', async () => {
    const h = renderSession()
    await enterAsGuest(h)
    let page: Awaited<ReturnType<ReturnType<typeof h.session>['gallery']['fetchPage']>> | undefined
    const pending = h.session().gallery.fetchPage(100, 100).then((p) => (page = p))
    const sent = h.driver.last('getGallery')
    expect(sent).toMatchObject({ kind: 'getGallery', offset: 100, limit: 100 })
    const requestId = (sent as { requestId: number }).requestId
    // Another request's reply doesn't settle this one.
    h.driver.emit({ kind: 'gallery', requestId: requestId + 99, ok: true, photos: [], received: 0, current: 0, max: 0 })
    h.driver.emit({ kind: 'gallery', requestId, ok: true, photos: [photo('old', '1000'), photo('new', '3000')], received: 2, current: 230, max: 500 })
    await act(async () => {
      await pending
    })
    expect(page).toEqual({ ok: true, photos: [photo('old', '1000'), photo('new', '3000')], received: 2, total: 230, max: 500 })
  })

  it('loadPhoto requests metadata and the stream fills the cache', async () => {
    const h = renderSession()
    await enterAsGuest(h)
    act(() => h.session().gallery.loadPhoto('g1'))
    expect(h.driver.last('getGalleryPhoto')).toEqual({ kind: 'getGalleryPhoto', id: 'g1' })
    h.driver.emit({ kind: 'galleryPhoto', id: 'g1', meta: { sceneName: 'Genesis Plaza', x: -9, y: 14 } })
    expect(h.session().gallery.metas['g1']?.sceneName).toBe('Genesis Plaza')
  })

  it('remove and setPublic send their writes and resolve with the outcome', async () => {
    const h = renderSession()
    await enterAsGuest(h)
    const removed = h.session().gallery.remove('a')
    const del = h.driver.last('deleteGalleryPhoto') as { requestId: number }
    expect(del).toMatchObject({ kind: 'deleteGalleryPhoto', id: 'a' })
    h.driver.emit({ kind: 'galleryWrite', requestId: del.requestId, ok: true, current: 4, max: 500 })
    await expect(removed).resolves.toEqual({ kind: 'galleryWrite', requestId: del.requestId, ok: true, current: 4, max: 500 })

    const updated = h.session().gallery.setPublic('b', true)
    const vis = h.driver.last('setGalleryPhotoPublic') as { requestId: number }
    expect(vis).toMatchObject({ kind: 'setGalleryPhotoPublic', id: 'b', isPublic: true })
    h.driver.emit({ kind: 'galleryWrite', requestId: vis.requestId, ok: false })
    await expect(updated).resolves.toMatchObject({ ok: false })
  })

  it('a request the bridge never answers fails after the timeout, and a late reply is ignored', async () => {
    const h = renderSession()
    await enterAsGuest(h)
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    try {
      const page = h.session().gallery.fetchPage(0, 100)
      const sent = h.driver.last('getGallery') as { requestId: number }
      vi.advanceTimersByTime(BRIDGE_CALL_TIMEOUT_MS)
      await expect(page).resolves.toEqual({ ok: false, photos: [], total: 0, max: 0 })
      h.driver.emit({ kind: 'gallery', requestId: sent.requestId, ok: true, photos: [photo('late', '1000')], received: 1, current: 1, max: 500 })
    } finally {
      vi.useRealTimers()
    }
  })

  it('a delete answered after the read timeout still succeeds', async () => {
    const h = renderSession()
    await enterAsGuest(h)
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    try {
      const removed = h.session().gallery.remove('a')
      const sent = h.driver.last('deleteGalleryPhoto') as { requestId: number }
      vi.advanceTimersByTime(BRIDGE_CALL_TIMEOUT_MS)
      h.driver.emit({ kind: 'galleryWrite', requestId: sent.requestId, ok: true, current: 1, max: 500 })
      await expect(removed).resolves.toMatchObject({ ok: true, current: 1, max: 500 })
    } finally {
      vi.useRealTimers()
    }
  })

  it('a delete the bridge never answers fails after the delete timeout', async () => {
    const h = renderSession()
    await enterAsGuest(h)
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    try {
      const removed = h.session().gallery.remove('a')
      vi.advanceTimersByTime(GALLERY_DELETE_TIMEOUT_MS)
      await expect(removed).resolves.toEqual({ ok: false })
    } finally {
      vi.useRealTimers()
    }
  })
})
