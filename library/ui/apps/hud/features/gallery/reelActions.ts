import { createElement, type ReactNode } from 'react'
import { showConfirm, showToast } from '../../design'
import { DownloadedToast } from './DownloadedToast'
import { isNativeHud, serviceUrl } from '@hud-host'
import { cefDownloadOutcome } from '@explorer-core/lib/cefNativeBridge'

export const REEL_MESSAGES = {
  shareToX: 'Happening right now in @decentraland.\n\nCome hang out 👋\n\n',
  deleted: 'Photo successfully deleted',
  updated: 'Photo successfully updated',
  downloaded: 'Photo successfully downloaded',
  linkCopied: 'Link copied!',
  failure: 'There was an error while trying to process your request. Please try again!',
  confirmDelete: 'Are you sure you want to permanently delete this photo?'
} as const

const TOAST_MS = 3000
const TOAST_KEY = 'camera-reel'
const NATIVE_DOWNLOAD_TIMEOUT_MS = 30_000

export type OpenExternalUrl = (url: string) => Promise<boolean>

export function reelToast(content: ReactNode, tone: 'default' | 'error' = 'default'): void {
  showToast(content, { duration: TOAST_MS, tone, key: TOAST_KEY })
}

function reelLink(id: string): string {
  return `${serviceUrl('reels')}/${id}`
}

export function shareToXUrl(id: string): string {
  return `https://x.com/intent/post?text=${encodeURIComponent(REEL_MESSAGES.shareToX)}&url=${encodeURIComponent(reelLink(id))}`
}

/** The intent url is also copied, so the post can be pasted where the browser can't open it; no
 *  toast. The native HUD has no browser tabs of its own, so there the engine opens the url. */
export function shareReelToX(id: string, openExternal?: OpenExternalUrl): void {
  const url = shareToXUrl(id)
  void navigator.clipboard?.writeText(url).catch(() => undefined)
  if (isNativeHud() && openExternal != null) void openExternal(url)
  else window.open(url, '_blank', 'noopener')
}

export function copyReelLink(id: string): void {
  const write = navigator.clipboard?.writeText(reelLink(id))
  if (write == null) {
    reelToast(REEL_MESSAGES.failure, 'error')
    return
  }
  write.then(
    () => reelToast(REEL_MESSAGES.linkCopied),
    () => reelToast(REEL_MESSAGES.failure, 'error')
  )
}

export function reelFileName(url: string): string {
  try {
    const name = new URL(url).pathname.split('/').pop()
    return name != null && name !== '' ? decodeURIComponent(name) : 'photo.jpg'
  } catch {
    return 'photo.jpg'
  }
}

/** Fetched first: a cross-origin link's `download` is ignored, so the browser would open the image
 *  instead of saving it. On native the engine saves the file, so the toast waits for its report. */
export async function downloadReel(url: string): Promise<void> {
  try {
    const res = await fetch(url)
    if (!res.ok) {
      // Release the unread error body rather than leave the connection holding it.
      await res.body?.cancel()
      throw new Error(`HTTP ${res.status}`)
    }
    const href = URL.createObjectURL(await res.blob())
    const saved = cefDownloadOutcome(href, NATIVE_DOWNLOAD_TIMEOUT_MS)
    const a = document.createElement('a')
    a.href = href
    a.download = reelFileName(url)
    a.rel = 'noopener'
    document.body.appendChild(a)
    a.click()
    a.remove()
    if (saved == null) {
      setTimeout(() => URL.revokeObjectURL(href), 1000)
      reelToast(REEL_MESSAGES.downloaded)
      return
    }
    const { ok, dir } = await saved
    URL.revokeObjectURL(href)
    if (!ok) throw new Error('the engine did not save the file')
    reelToast(dir == null ? REEL_MESSAGES.downloaded : createElement(DownloadedToast, { message: REEL_MESSAGES.downloaded, dir }))
  } catch (e) {
    console.warn('[gallery] download failed', e)
    reelToast(REEL_MESSAGES.failure, 'error')
  }
}

export function confirmReelDelete(): Promise<boolean> {
  return showConfirm({ title: REEL_MESSAGES.confirmDelete, cancelLabel: 'CANCEL', confirmLabel: 'DELETE' })
}
