import { describe, it, expect, vi, beforeEach } from 'vitest'
import type { Ctx } from '../../bridge-scene/src/bridge'
import type { PageToScene, SceneToPage } from '@explorer-core/engine/protocol'

const http = vi.hoisted(() => ({
  serviceBase: vi.fn(async () => 'https://camera-reel.test'),
  signed: vi.fn(async (): Promise<unknown> => undefined)
}))
const player = vi.hoisted(() => ({ current: { userId: '0xme' } as { userId: string } | null }))
vi.mock('../../bridge-scene/src/http', () => http)
vi.mock('../../bridge-scene/node_modules/@dcl/sdk/players/index.js', () => ({ getPlayer: () => player.current }))

const { registerGallery } = await import('../../bridge-scene/src/domains/gallery')

type Handler = (msg: PageToScene) => Promise<void>

function start(): { sent: SceneToPage[]; ask: (msg: PageToScene) => Promise<void> } {
  const sent: SceneToPage[] = []
  const handlers = new Map<string, Handler>()
  const ctx = { send: (m: SceneToPage) => sent.push(m), on: (kind: string, h: Handler) => handlers.set(kind, h), push: () => {} } as unknown as Ctx
  registerGallery(ctx)
  return { sent, ask: async (msg) => handlers.get(msg.kind)?.(msg) }
}

beforeEach(() => {
  vi.clearAllMocks()
  player.current = { userId: '0xme' }
  http.serviceBase.mockImplementation(async () => 'https://camera-reel.test')
})

describe('bridge gallery', () => {
  it('reads one page by offset and limit, and counts the images the service sent', async () => {
    http.signed.mockResolvedValueOnce({
      images: [
        { id: 'a', url: 'https://img/a.jpg', dateTime: '1', isPublic: true },
        { id: 'b', dateTime: '2' }
      ],
      currentImages: 230,
      maxImages: 500
    })
    const { sent, ask } = start()
    await ask({ kind: 'getGallery', requestId: 7, offset: 100, limit: 100 })
    expect(http.signed).toHaveBeenCalledWith('https://camera-reel.test/api/users/0xme/images?limit=100&offset=100&compact=true')
    expect(sent).toEqual([
      {
        kind: 'gallery',
        requestId: 7,
        ok: true,
        photos: [{ id: 'a', url: 'https://img/a.jpg', thumbnailUrl: undefined, dateTime: '1', isPublic: true }],
        received: 2,
        current: 230,
        max: 500
      }
    ])
  })

  it('answers a failed read, the service base included, with ok false under its requestId', async () => {
    http.serviceBase.mockRejectedValueOnce(new Error('no services'))
    const { sent, ask } = start()
    await ask({ kind: 'getGallery', requestId: 3, offset: 0, limit: 100 })
    expect(sent).toEqual([{ kind: 'gallery', requestId: 3, ok: false, photos: [], received: 0, current: 0, max: 0 }])
  })

  it('answers with ok false when there is no player yet', async () => {
    player.current = null
    const { sent, ask } = start()
    await ask({ kind: 'getGallery', requestId: 4, offset: 0, limit: 100 })
    expect(http.signed).not.toHaveBeenCalled()
    expect(sent).toEqual([{ kind: 'gallery', requestId: 4, ok: false, photos: [], received: 0, current: 0, max: 0 }])
  })

  it('deletes with a signed DELETE and reports the storage that follows', async () => {
    http.signed.mockResolvedValueOnce({ currentImages: 229, maxImages: 500 })
    const { sent, ask } = start()
    await ask({ kind: 'deleteGalleryPhoto', requestId: 5, id: 'a/b' })
    expect(http.signed).toHaveBeenCalledWith('https://camera-reel.test/api/images/a%2Fb', 'DELETE')
    expect(sent).toEqual([{ kind: 'galleryWrite', requestId: 5, ok: true, current: 229, max: 500 }])
  })

  it('makes a photo public with the snake-case body, and reports a failed write', async () => {
    const { sent, ask } = start()
    await ask({ kind: 'setGalleryPhotoPublic', requestId: 6, id: 'a', isPublic: true })
    expect(http.signed).toHaveBeenCalledWith('https://camera-reel.test/api/images/a/visibility', 'PATCH', { is_public: true })
    http.signed.mockRejectedValueOnce(new Error('403'))
    await ask({ kind: 'setGalleryPhotoPublic', requestId: 8, id: 'a', isPublic: false })
    expect(sent).toEqual([
      { kind: 'galleryWrite', requestId: 6, ok: true },
      { kind: 'galleryWrite', requestId: 8, ok: false }
    ])
  })
})
