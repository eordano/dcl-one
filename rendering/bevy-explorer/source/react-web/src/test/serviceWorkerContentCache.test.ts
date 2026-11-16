// The service worker's upkeep of the content cache: on a `content-cache-trim` message it deletes
// the oldest-written entries of `ipfs-path-cache-v2` while the cache is over its cap, and touches
// no other cache.
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it, vi } from 'vitest'

const CONTENT_CACHE = 'ipfs-path-cache-v2'
const OTHER_CACHES = ['dcl-editor-preview-v1', 'dcl-editor-scene-v1', 'dcl-editor-storage-v1']

type Message = { type: string; capBytes?: unknown; quotaError?: boolean }
type Pure = {
  contentCacheTarget: (input: { capBytes: number; quota: number; cachesUsage: number; quotaError: boolean }) => {
    cap: number
    bytesToFree: number
  }
  pickEvictions: (sized: { key: string; size: number }[], bytesToFree: number) => { picked: string[]; freed: number }
}

const entryUrl = (n: number | string) => `https://shared.invalid/bafkrei${n}`

function entry(size: number, withLength = true) {
  const headers: Record<string, string> = withLength ? { 'Content-Length': String(size) } : {}
  return new Response('x'.repeat(size), { headers })
}

const keyOf = (key: string | Request) => (typeof key === 'string' ? key : key.url)

// the spec's cache: insertion ordered, a put removes the old item and appends the new one
function fakeCache(seed: [string, Response][] = []) {
  const map = new Map<string, Response>(seed)
  return {
    map,
    keys: async () => [...map.keys()].map((url) => new Request(url)),
    match: async (key: string | Request) => map.get(keyOf(key))?.clone(),
    delete: async (key: string | Request) => map.delete(keyOf(key)),
    put: async (key: string | Request, res: Response) => {
      map.delete(keyOf(key))
      map.set(keyOf(key), res)
    }
  }
}
type FakeCache = ReturnType<typeof fakeCache>

function serviceWorker(options: { quota?: number; usage?: number; usageDetails?: object | null; others?: boolean; otherSize?: number; persisted?: 'absent' | 'rejects' } = {}) {
  const listeners: Record<string, (event: unknown) => void> = {}
  const content = fakeCache()
  const fakes = new Map<string, FakeCache>([[CONTENT_CACHE, content]])
  if (options.others) {
    for (const name of OTHER_CACHES) fakes.set(name, fakeCache([[`https://editor.invalid/${name}`, entry(options.otherSize ?? 1000)]]))
  }
  const opened: string[] = []
  const caches = {
    open: vi.fn(async (name: string) => {
      opened.push(name)
      let cache = fakes.get(name)
      if (!cache) fakes.set(name, (cache = fakeCache()))
      return cache
    })
  }
  const cachesUsage = () => {
    let total = 0
    for (const cache of fakes.values()) {
      for (const response of cache.map.values()) total += Number(response.headers.get('Content-Length') ?? 0)
    }
    return total
  }
  const estimate = vi.fn(async () => ({
    usage: options.usage ?? cachesUsage(),
    quota: options.quota ?? 1e9,
    ...(options.usageDetails === null ? {} : { usageDetails: options.usageDetails ?? { caches: cachesUsage() } })
  }))
  const storage: Record<string, unknown> = { estimate }
  if (options.persisted === 'rejects') storage.persisted = async () => Promise.reject(new Error('not allowed'))
  else if (options.persisted !== 'absent') storage.persisted = async () => false
  const navigator = { storage }
  const self = {
    addEventListener: (type: string, fn: (event: unknown) => void) => (listeners[type] = fn),
    registration: { scope: 'https://play.example/' }
  }
  const source = readFileSync(resolve(__dirname, '../../../deploy/web/service_worker.js'), 'utf8')
  const load = new Function(
    'self',
    'caches',
    'fetch',
    'console',
    'navigator',
    source + '\nreturn { contentCacheTarget, pickEvictions };'
  ) as (...args: unknown[]) => Pure
  const pure = load(self, caches, vi.fn(), { log: vi.fn(), warn: vi.fn() }, navigator)

  const send = async (data: unknown) => {
    const port = { postMessage: vi.fn() }
    const waitUntil = vi.fn()
    listeners.message({ data, ports: [port], waitUntil })
    if (waitUntil.mock.calls.length) await waitUntil.mock.calls[0][0]
    return { port, waitUntil }
  }
  // like `send`, but does not wait: the replies come in `done`, in the order they were posted
  const sendAsync = (data: unknown) => {
    const port = { postMessage: vi.fn((message: unknown) => replies.push({ data, message })) }
    const waitUntil = vi.fn()
    listeners.message({ data, ports: [port], waitUntil })
    return { port, done: waitUntil.mock.calls.length ? (waitUntil.mock.calls[0][0] as Promise<unknown>) : Promise.resolve() }
  }
  const replies: { data: unknown; message: any }[] = []
  const seed = (n: number, size = 100, withLength = true) => content.map.set(entryUrl(n), entry(size, withLength))
  const trim = (message: Partial<Message> & { capBytes: unknown }) => send({ type: 'content-cache-trim', ...message })
  const remaining = () => [...content.map.keys()].map((url) => url.replace('https://shared.invalid/', ''))
  return { pure, send, sendAsync, replies, trim, seed, content, fakes, caches, opened, estimate, remaining }
}

describe('content cache target', () => {
  it('has nothing to free under the cap', () => {
    const { pure } = serviceWorker()
    expect(pure.contentCacheTarget({ capBytes: 1000, quota: 1e9, cachesUsage: 1000, quotaError: false })).toEqual({
      cap: 1000,
      bytesToFree: 0
    })
  })

  it('frees down to 90% of the cap when over it', () => {
    const { pure } = serviceWorker()
    expect(pure.contentCacheTarget({ capBytes: 1000, quota: 1e9, cachesUsage: 1200, quotaError: false }).bytesToFree).toBe(300)
  })

  it('caps at half the quota', () => {
    const { pure } = serviceWorker()
    expect(pure.contentCacheTarget({ capBytes: 1000, quota: 1000, cachesUsage: 600, quotaError: false })).toEqual({
      cap: 500,
      bytesToFree: 150
    })
  })
})

describe('eviction choice', () => {
  it('takes the leading entries until enough is freed', () => {
    const { pure } = serviceWorker()
    const sized = [
      { key: 'a', size: 50 },
      { key: 'b', size: 50 },
      { key: 'c', size: 50 }
    ]
    expect(pure.pickEvictions(sized, 60)).toEqual({ picked: ['a', 'b'], freed: 100 })
    expect(pure.pickEvictions(sized, 0)).toEqual({ picked: [], freed: 0 })
    expect(pure.pickEvictions(sized, 1000).picked).toEqual(['a', 'b', 'c'])
  })
})

describe('content cache trim message', () => {
  it('evicts the oldest-written entries until the cache is under the cap', async () => {
    const sw = serviceWorker()
    for (let n = 0; n < 10; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 500 })
    expect(sw.remaining()).toEqual(['bafkrei6', 'bafkrei7', 'bafkrei8', 'bafkrei9'])
    // the exact report: crates/ipfs/src/web_cache.rs `parse_trim_reply` reads these keys
    expect(port.postMessage).toHaveBeenCalledWith({
      capBytes: 500,
      usage: 1000,
      quota: 1e9,
      cachesUsage: 1000,
      evictedEntries: 6,
      evictedBytes: 600,
      persisted: false,
      contentBytes: 1000,
      preempted: false
    })
  })

  it('keeps an entry written again while the trim was sizing', async () => {
    const sw = serviceWorker()
    for (let n = 0; n < 10; n++) sw.seed(n)
    const match = sw.content.match
    let rewritten = false
    sw.content.match = async (key: string | Request) => {
      // the engine writes entry 0 again after the trim listed the keys
      if (!rewritten) {
        rewritten = true
        await sw.content.put(entryUrl(0), entry(100))
      }
      return match(key)
    }
    const { port } = await sw.trim({ capBytes: 500 })
    expect(sw.remaining()).toContain('bafkrei0')
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ evictedEntries: 5, evictedBytes: 500 }))
  })

  it('keeps the newest entry when it was written again with other bytes', async () => {
    const sw = serviceWorker()
    sw.seed(0, 100)
    sw.seed(1, 900)
    const match = sw.content.match
    sw.content.match = async (key: string | Request) => {
      const response = await match(key)
      // once sized, a processed texture is written back: the order shows nothing, the size does
      if (keyOf(key) === entryUrl(1) && response?.headers.get('Content-Length') === '900') {
        await sw.content.put(entryUrl(1), entry(800))
      }
      return response
    }
    const { port } = await sw.trim({ capBytes: 500 })
    expect(sw.remaining()).toEqual(['bafkrei1'])
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ evictedEntries: 1, evictedBytes: 100 }))
  })

  it('keeps an entry written again while the trim was sizing when the pick takes the whole cache', async () => {
    const sw = serviceWorker()
    sw.seed(0, 100)
    sw.seed(1, 900)
    const match = sw.content.match
    let rewritten = false
    sw.content.match = async (key: string | Request) => {
      if (!rewritten) {
        rewritten = true
        await sw.content.put(entryUrl(0), entry(100))
      }
      return match(key)
    }
    const { port } = await sw.trim({ capBytes: 500 })
    expect(sw.remaining()).toEqual(['bafkrei0'])
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ evictedEntries: 1, evictedBytes: 900 }))
  })

  it('moves an entry written again to the newest end', async () => {
    const sw = serviceWorker()
    for (let n = 0; n < 3; n++) sw.seed(n)
    await sw.content.put(entryUrl(0), entry(100))
    await sw.trim({ capBytes: 250 })
    expect(sw.remaining()).toEqual(['bafkrei2', 'bafkrei0'])
  })

  it('does not open any cache while usage with other storage is at or under the cap', async () => {
    const sw = serviceWorker({ others: true, otherSize: 100 })
    for (let n = 0; n < 5; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 1000 })
    expect(sw.caches.open).not.toHaveBeenCalled()
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ cachesUsage: 800, contentBytes: null }))
  })

  it('does not open the cache when under the cap', async () => {
    const sw = serviceWorker()
    sw.seed(0)
    const { port } = await sw.trim({ capBytes: 1000 })
    expect(sw.caches.open).not.toHaveBeenCalled()
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ evictedEntries: 0 }))
  })

  it('never opens or changes another cache', async () => {
    const sw = serviceWorker({ others: true })
    for (let n = 0; n < 5; n++) sw.seed(n)
    await sw.trim({ capBytes: 10 })
    expect(sw.remaining()).toEqual([])
    expect(new Set(sw.opened)).toEqual(new Set([CONTENT_CACHE]))
    for (const name of OTHER_CACHES) expect(sw.fakes.get(name)!.map.size).toBe(1)
  })

  it('sizes an entry without Content-Length by its body', async () => {
    const sw = serviceWorker({ usage: 1000, usageDetails: { caches: 1000 } })
    sw.seed(0, 300, false)
    sw.seed(1)
    sw.seed(2)
    // the worker is told the cache holds 1000 bytes against a cap of 100
    const { port } = await sw.trim({ capBytes: 100 })
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ evictedEntries: 3, evictedBytes: 500 }))
  })

  it('answers a bad cap with an error and keeps serving later trims', async () => {
    const sw = serviceWorker()
    for (let n = 0; n < 4; n++) sw.seed(n)
    const bad = await sw.trim({ capBytes: 'big' })
    expect(bad.port.postMessage).toHaveBeenCalledWith({ error: expect.any(String) })
    const good = await sw.trim({ capBytes: 200 })
    expect(good.port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ evictedEntries: 3, evictedBytes: 300 }))
  })

  it('ignores other messages', async () => {
    const sw = serviceWorker()
    const { waitUntil } = await sw.send({ type: 'something-else' })
    expect(sw.estimate).not.toHaveBeenCalled()
    expect(waitUntil).not.toHaveBeenCalled()
  })

  it('frees the oldest content worth a quarter of the cap after a quota error, even under the cap', async () => {
    const sw = serviceWorker()
    for (let n = 0; n < 6; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 1000, quotaError: true })
    expect(sw.remaining()).toEqual(['bafkrei3', 'bafkrei4', 'bafkrei5'])
    expect(port.postMessage).toHaveBeenCalledWith(
      expect.objectContaining({ evictedEntries: 3, evictedBytes: 300, contentBytes: null })
    )
  })

  it('sizes only what a quota-error trim frees, not the whole cache', async () => {
    const sw = serviceWorker()
    for (let n = 0; n < 200; n++) sw.seed(n)
    const match = sw.content.match
    let matches = 0
    sw.content.match = async (key: string | Request) => {
      matches++
      return match(key)
    }
    const { port } = await sw.trim({ capBytes: 4000, quotaError: true })
    expect(matches).toBeLessThanOrEqual(64)
    expect(sw.remaining()).toHaveLength(190)
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ evictedEntries: 10, evictedBytes: 1000 }))
  })

  it('reads Cache Storage usage from usageDetails, and 0 when it is left out', async () => {
    const sw = serviceWorker({ usage: 5000, usageDetails: {} })
    for (let n = 0; n < 3; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 1000 })
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ cachesUsage: 0, evictedEntries: 0 }))
  })
  it('falls back to usage when usageDetails is missing altogether', async () => {
    const sw = serviceWorker({ usage: 1000, usageDetails: null })
    for (let n = 0; n < 10; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 500 })
    expect(port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ cachesUsage: 1000, evictedEntries: 6 }))
  })

  it('still trims when navigator.storage.persisted is missing', async () => {
    const sw = serviceWorker({ persisted: 'absent' })
    for (let n = 0; n < 10; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 500 })
    expect(port.postMessage).toHaveBeenCalledWith(
      expect.objectContaining({ evictedEntries: 6, persisted: false })
    )
  })

  it('still trims when navigator.storage.persisted rejects', async () => {
    const sw = serviceWorker({ persisted: 'rejects' })
    for (let n = 0; n < 10; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 500 })
    expect(port.postMessage).toHaveBeenCalledWith(
      expect.objectContaining({ evictedEntries: 6, persisted: false })
    )
  })

  it('does not evict content when other storage alone keeps usage over the cap', async () => {
    const sw = serviceWorker({ others: true })
    for (let n = 0; n < 10; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 2000 })
    expect(sw.remaining()).toHaveLength(10)
    expect(port.postMessage).toHaveBeenCalledWith(
      expect.objectContaining({ cachesUsage: 4000, evictedEntries: 0, evictedBytes: 0, contentBytes: 1000 })
    )
  })

  it('does not evict content under the cap when other storage takes part of it', async () => {
    // cap 2000, other storage 1500, content 1000: usage is over the cap, the content is not
    const sw = serviceWorker({ others: true, otherSize: 500 })
    for (let n = 0; n < 10; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 2000 })
    expect(sw.remaining()).toHaveLength(10)
    expect(port.postMessage).toHaveBeenCalledWith(
      expect.objectContaining({ cachesUsage: 2500, evictedEntries: 0, contentBytes: 1000 })
    )
  })

  it('trims content over the cap to 90% of it, whatever other storage holds', async () => {
    for (const otherSize of [500, 1000]) {
      const sw = serviceWorker({ others: true, otherSize })
      for (let n = 0; n < 10; n++) sw.seed(n)
      await sw.trim({ capBytes: 500 })
      expect(sw.remaining()).toEqual(['bafkrei6', 'bafkrei7', 'bafkrei8', 'bafkrei9'])
    }
  })

  it('frees a quarter of the cap after a quota error whatever other storage holds', async () => {
    const sw = serviceWorker({ others: true })
    for (let n = 0; n < 10; n++) sw.seed(n)
    const { port } = await sw.trim({ capBytes: 2000, quotaError: true })
    expect(sw.remaining()).toHaveLength(5)
    expect(port.postMessage).toHaveBeenCalledWith(
      expect.objectContaining({ evictedEntries: 5, evictedBytes: 500, contentBytes: null })
    )
  })

  it('cancels the body of an entry sized by its Content-Length', async () => {
    const sw = serviceWorker()
    const cancel = vi.fn()
    for (let n = 0; n < 5; n++) {
      const body = new ReadableStream({ cancel })
      sw.content.map.set(entryUrl(n), new Response(body, { headers: { 'Content-Length': '100' } }))
    }
    // hand out the stored response itself, so the spy sees the cancel
    sw.content.match = async (key: string | Request) => sw.content.map.get(keyOf(key))
    await sw.trim({ capBytes: 100 })
    expect(cancel).toHaveBeenCalledTimes(5)
  })

  it('lets a quota-error request run before pending regular trims and stops a running regular one', async () => {
    const sw = serviceWorker()
    for (let n = 0; n < 300; n++) sw.seed(n)
    const first = sw.sendAsync({ type: 'content-cache-trim', capBytes: 1000 })
    const second = sw.sendAsync({ type: 'content-cache-trim', capBytes: 1000 })
    const forced = sw.sendAsync({ type: 'content-cache-trim', capBytes: 1000, quotaError: true })
    await Promise.all([first.done, second.done, forced.done])
    expect(first.port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ preempted: true, evictedEntries: 0 }))
    expect(sw.replies.map((r) => (r.data as Message).quotaError === true)).toEqual([false, true, false])
    expect(forced.port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ preempted: false }))
    expect(sw.remaining().length * 100).toBeLessThanOrEqual(1000)
  })
})
