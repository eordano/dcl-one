// The service worker's asset cache: the worker only answers from it. The engine fills it: an
// entry under the shared key (the hash alone, written once it has checked the bytes against that
// hash) answers for every content server, an entry under a url only that url.
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it, vi } from 'vitest'

const SHARED = 'https://shared.invalid'

// the worker's script, run against a stand-in global; `seeded` is what the cache already holds
function serviceWorker(seeded: Record<string, string> = {}) {
  const listeners: Record<string, (event: unknown) => void> = {}
  const store = new Map<string, Response>(Object.entries(seeded).map(([key, body]) => [key, new Response(body)]))
  const cache = {
    match: async (key: string) => store.get(key)?.clone(),
    put: async (key: string, res: Response) => void store.set(key, res),
    delete: async (key: string) => store.delete(key)
  }
  const network = vi.fn(async (req: Request) => new Response(new URL(req.url).origin))
  const self = {
    addEventListener: (type: string, fn: (event: unknown) => void) => (listeners[type] = fn),
    registration: { scope: 'https://play.example/' }
  }
  const source = readFileSync(resolve(__dirname, '../../../deploy/web/service_worker.js'), 'utf8')
  const load = new Function('self', 'caches', 'fetch', 'console', source) as (
    self: object,
    caches: object,
    fetch: object,
    console: object
  ) => void
  const open = vi.fn(async () => cache)
  load(self, { open }, network, { log: vi.fn(), warn: vi.fn() })

  const asset = async (url: string, method = 'GET', init?: RequestInit): Promise<{ body: string; cached: boolean }> => {
    let reply: Promise<Response> | undefined
    listeners.fetch({
      request: new Request(url, { ...init, method, headers: { 'X-IPFS': '1' } }),
      respondWith: (r: Promise<Response>) => (reply = r)
    })
    const response = await reply!
    return { body: await response.text(), cached: response.headers.has('X-IPFS-Cached') }
  }
  // a request without the header, as the engine sends content requests; whether the worker replied
  const untagged = (url: string): boolean => {
    let replied = false
    listeners.fetch({ request: new Request(url), respondWith: () => (replied = true) })
    return replied
  }
  return { asset, untagged, open, network, keys: () => [...store.keys()] }
}

describe('service worker asset cache', () => {
  const hash = 'bafkreiexample'
  const path = `/contents/${hash}`

  it('passes a miss to the network and stores nothing itself', async () => {
    const { asset, network, keys } = serviceWorker()
    expect(await asset(`https://peer.example${path}`)).toEqual({ body: 'https://peer.example', cached: false })
    expect(await asset(`https://peer.example${path}`)).toEqual({ body: 'https://peer.example', cached: false })
    expect(network).toHaveBeenCalledTimes(2)
    expect(keys()).toEqual([])
  })

  it('forwards a cache miss with its referrer policy and cancellation signal', async () => {
    const { asset, network } = serviceWorker()
    const controller = new AbortController()
    await asset(`https://peer.example${path}`, 'GET', {
      referrer: '', referrerPolicy: 'no-referrer', signal: controller.signal
    })
    const [forwarded] = network.mock.calls[0]
    expect(forwarded.headers.has('X-IPFS')).toBe(false)
    expect(forwarded.referrer).toBe('')
    expect(forwarded.referrerPolicy).toBe('no-referrer')
    controller.abort()
    expect(forwarded.signal.aborted).toBe(true)
  })

  it('answers a url from its own entry, and only that url', async () => {
    const { asset, network } = serviceWorker({ [`https://peer.example${path}`]: 'peer' })
    expect(await asset(`https://peer.example${path}`)).toEqual({ body: 'peer', cached: true })
    expect(await asset(`https://evil.example${path}`)).toEqual({ body: 'https://evil.example', cached: false })
    expect(network).toHaveBeenCalledTimes(1)
  })

  it('answers every origin from the shared entry, dropping the origin\'s own copy', async () => {
    const { asset, network, keys } = serviceWorker({
      [`${SHARED}/${hash}`]: 'checked',
      [`https://evil.example${path}`]: 'unchecked'
    })
    expect(await asset(`https://peer.example${path}`)).toEqual({ body: 'checked', cached: true })
    // whatever path the server keeps its content under
    expect(await asset(`https://worlds.example/world${path}`)).toEqual({ body: 'checked', cached: true })
    expect(await asset(`https://evil.example${path}`)).toEqual({ body: 'checked', cached: true })
    expect(network).not.toHaveBeenCalled()
    // and drops that origin's own copy
    expect(keys()).toEqual([`${SHARED}/${hash}`])
  })

  it('keys a request by where its path ends, not by what follows a `#`', async () => {
    const { asset } = serviceWorker({ [`${SHARED}/bafkreitarget`]: 'target' })
    expect(await asset(`https://evil.example/contents/bafkreitarget#${path}`)).toEqual({ body: 'target', cached: true })
  })

  it('answers nothing but a GET from an entry', async () => {
    const { asset, network } = serviceWorker({ [`https://peer.example${path}`]: 'peer' })
    expect(await asset(`https://peer.example${path}`, 'HEAD')).toEqual({ body: 'https://peer.example', cached: false })
    expect(await asset(`https://peer.example${path}`)).toEqual({ body: 'peer', cached: true })
    expect(network).toHaveBeenCalledTimes(1)
  })

  it('leaves a request without the header to the browser, without looking in the cache', async () => {
    const { asset, untagged, open, network } = serviceWorker({ [`${SHARED}/${hash}`]: 'checked' })
    expect(untagged(`https://peer.example${path}`)).toBe(false)
    expect(open).not.toHaveBeenCalled()
    expect(network).not.toHaveBeenCalled()
    // the same url with the header is answered from the cache
    expect(await asset(`https://peer.example${path}`)).toEqual({ body: 'checked', cached: true })
    expect(open).toHaveBeenCalledTimes(1)
  })
})
