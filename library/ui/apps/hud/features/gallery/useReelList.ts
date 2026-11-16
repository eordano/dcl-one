// The next page starts at the number of photos held, not the last offset plus the page size: a
// delete shifts the reel, and the fixed step would skip a photo.

import { useCallback, useEffect, useRef, useState } from 'react'
import type { GalleryPhoto } from '@explorer-core/engine/protocol'
import type { ReelPage } from '../session/useEngineSession'

const REEL_PAGE_SIZE = 100
const REEL_LOAD_MORE_THRESHOLD = 12

export interface ReelList {
  photos: GalleryPhoto[]
  /** Photos in the reel in all, and the owner's storage limit, as the last page reported them. */
  total: number
  max: number
  loaded: boolean
  /** The last page asked for failed. Paging waits for `retry` rather than ask again on every scroll. */
  error: boolean
  retry: () => void
  loadMore: (lastVisibleIndex: number) => void
  /** Drop a photo (deleted, or made private on a public reel); returns where it was, or -1. */
  remove: (id: string) => number
  /** Put a dropped photo back at `index` (a write that dropped it first failed). */
  restore: (photo: GalleryPhoto, index: number) => void
  setPublic: (id: string, isPublic: boolean) => void
  setStorage: (total: number, max: number) => void
}

const FAILED_PAGE: ReelPage = { ok: false, photos: [], total: 0, max: 0 }

export function useReelList(key: string | null, fetchPage: (offset: number, limit: number) => Promise<ReelPage>): ReelList {
  const [photos, setPhotos] = useState<GalleryPhoto[]>([])
  const [storage, setStorageState] = useState({ total: 0, max: 0 })
  const [loaded, setLoaded] = useState(false)
  const [error, setError] = useState(false)
  // Read synchronously by loadMore, which the grid calls on every scroll. `ids` are the photos held:
  // a page that repeats one (the reel shifted under it) adds it once, and only new photos advance
  // the offset.
  const state = useRef({ generation: 0, loading: false, done: false, failed: false, count: 0, ids: new Set<string>() })
  const fetchRef = useRef(fetchPage)
  fetchRef.current = fetchPage

  const load = useCallback(() => {
    const s = state.current
    if (s.loading || s.done) return
    s.loading = true
    s.failed = false
    setError(false)
    // A retried first page shows the spinner again, not the empty reel.
    if (s.count === 0) setLoaded(false)
    const generation = s.generation
    const settle = (page: ReelPage): void => {
      if (generation !== s.generation) return
      s.loading = false
      if (page.ok) {
        const fresh = page.photos.filter((p) => !s.ids.has(p.id))
        for (const p of fresh) s.ids.add(p.id)
        s.count += fresh.length
        // A short page is judged by what the service sent: photos it sent without a url are dropped,
        // and would make a full page look like the last one.
        s.done = s.count >= page.total || (page.received ?? page.photos.length) < REEL_PAGE_SIZE
        setPhotos((list) => [...list, ...fresh])
        setStorageState({ total: page.total, max: page.max })
      } else {
        s.failed = true
        setError(true)
      }
      setLoaded(true)
    }
    void fetchRef.current(s.count, REEL_PAGE_SIZE).then(settle, (e: unknown) => {
      console.warn('[gallery] reel page failed', e)
      settle(FAILED_PAGE)
    })
  }, [])

  useEffect(() => {
    const s = state.current
    s.generation += 1
    s.loading = false
    s.done = false
    s.failed = false
    s.count = 0
    s.ids = new Set()
    setPhotos([])
    setStorageState({ total: 0, max: 0 })
    setLoaded(false)
    setError(false)
    if (key != null) load()
  }, [key, load])

  const loadMore = useCallback(
    (lastVisibleIndex: number) => {
      const s = state.current
      if (s.failed || s.count - lastVisibleIndex >= REEL_LOAD_MORE_THRESHOLD) return
      load()
    },
    [load]
  )

  const photosRef = useRef(photos)
  photosRef.current = photos
  const remove = useCallback(
    (id: string): number => {
      const s = state.current
      if (!s.ids.has(id)) return -1
      const index = photosRef.current.findIndex((p) => p.id === id)
      s.ids.delete(id)
      s.count -= 1
      setPhotos((list) => list.filter((p) => p.id !== id))
      setStorageState((st) => ({ ...st, total: Math.max(0, st.total - 1) }))
      // A page already on its way asked from the old offset; if the service applied this removal
      // first, that page starts one photo late and skips one. Ask again from the corrected offset.
      if (s.loading) {
        s.generation += 1
        s.loading = false
        load()
      }
      return index
    },
    [load]
  )

  const restore = useCallback((photo: GalleryPhoto, index: number) => {
    const s = state.current
    if (s.ids.has(photo.id)) return
    s.ids.add(photo.id)
    s.count += 1
    setPhotos((list) => {
      const at = index < 0 ? list.length : Math.min(index, list.length)
      return [...list.slice(0, at), photo, ...list.slice(at)]
    })
    setStorageState((st) => ({ ...st, total: st.total + 1 }))
  }, [])

  const setPublic = useCallback((id: string, isPublic: boolean) => {
    setPhotos((list) => list.map((p) => (p.id === id ? { ...p, isPublic } : p)))
  }, [])

  const setStorage = useCallback((total: number, max: number) => setStorageState({ total, max }), [])

  return { photos, total: storage.total, max: storage.max, loaded, error, retry: load, loadMore, remove, restore, setPublic, setStorage }
}
