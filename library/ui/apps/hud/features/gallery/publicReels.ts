// Every bridge kernelFetch is signed, so the HUD makes this unsigned read itself.

import { serviceUrl } from '@hud-host'
import type { GalleryPhoto } from '@explorer-core/engine/protocol'
import type { ReelPage } from '../session/useEngineSession'

type CompactImage = { id: string; url?: string; thumbnailUrl?: string; isPublic?: boolean; dateTime?: string }
type CompactResponse = { images?: CompactImage[]; currentImages?: number; maxImages?: number }

// The address goes into the request path, and it can come from a photo's people, which the uploader
// writes: only a wallet address is read.
const ADDRESS = /^0x[0-9a-f]{40}$/i

async function fetchFromService(address: string, offset: number, limit: number): Promise<ReelPage> {
  if (!ADDRESS.test(address)) {
    console.warn('[passport] camera reel not read: not a wallet address', address)
    return { ok: false, photos: [], total: 0, max: 0 }
  }
  const params = new URLSearchParams({ limit: String(limit), offset: String(offset), compact: 'true' })
  try {
    const res = await fetch(`${serviceUrl('cameraReel')}/api/users/${address.toLowerCase()}/images?${params}`)
    if (!res.ok) {
      // Release the unread error body rather than leave the connection holding it.
      await res.body?.cancel()
      throw new Error(`camera reel returned ${res.status}`)
    }
    const body = (await res.json()) as CompactResponse
    const images = body.images ?? []
    const photos: GalleryPhoto[] = images
      .filter((i) => typeof i.url === 'string')
      .map((i) => ({ id: i.id, url: i.url as string, thumbnailUrl: i.thumbnailUrl, dateTime: i.dateTime ?? '', isPublic: i.isPublic }))
    return { ok: true, photos, received: images.length, total: body.currentImages ?? photos.length, max: body.maxImages ?? 0 }
  } catch (e) {
    console.warn('[passport] camera reel read failed', e)
    return { ok: false, photos: [], total: 0, max: 0 }
  }
}

type PublicReelFetcher = (address: string, offset: number, limit: number) => Promise<ReelPage>
let fetcher: PublicReelFetcher = fetchFromService

export function fetchPublicReelPage(address: string, offset: number, limit: number): Promise<ReelPage> {
  return fetcher(address, offset, limit)
}

/** Swap the source (the ?mock=1 HUD has no service to read); null restores the service. */
export function setPublicReelFetcher(next: PublicReelFetcher | null): void {
  fetcher = next ?? fetchFromService
}
