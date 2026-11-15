import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { it } from 'node:test'
import {
  hashV0, hashV1, prepareADR32Data, calculateMultipleHashesADR32,
  calculateMultipleHashesADR32LegacyQmHash, keccak256Hash
} from '../src/index.ts'
import { hashV1WithLayout } from '../src/layout.ts'
import { cidV1, cidV1Text, qmText, sha256Digest, varint } from '../src/cid.ts'

const seed = Number(process.env.HASH_FUZZ_SEED ?? 0x62ad32) >>> 0
const rounds = Number(process.env.HASH_FUZZ_ROUNDS ?? 1000)
assert.ok(Number.isSafeInteger(rounds) && rounds > 0)
let state = seed
function random(limit: number): number {
  state = (Math.imul(state, 1664525) + 1013904223) >>> 0
  return Math.floor(state / 0x100000000 * limit)
}
function bytes(size: number): Uint8Array {
  return Uint8Array.from({ length: size }, () => random(256))
}
function digest(data: Uint8Array): Uint8Array {
  return new Uint8Array(createHash('sha256').update(data).digest())
}
function base32(data: Uint8Array): string {
  const bits = [...data].map(byte => byte.toString(2).padStart(8, '0')).join('')
  return 'b' + (bits.match(/.{1,5}/g) ?? []).map(group =>
    'abcdefghijklmnopqrstuvwxyz234567'[parseInt(group.padEnd(5, '0'), 2)]).join('')
}
function base58(data: Uint8Array): string {
  let value = BigInt('0x' + Buffer.from(data).toString('hex'))
  let result = ''
  while (value) {
    result = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'[Number(value % 58n)] + result
    value /= 58n
  }
  return result
}
function referenceV0(data: Uint8Array): string {
  return base58(Uint8Array.from([18, 32, ...digest(data)]))
}
function integer(value: number): number[] {
  const result: number[] = []
  let rest = BigInt(value)
  do {
    result.push(Number(rest & 127n) | (rest > 127n ? 128 : 0))
    rest >>= 7n
  } while (rest)
  return result
}
function field(tag: number, data: number[]): number[] {
  return [tag, ...integer(data.length), ...data]
}
interface Node { cid: number[]; size: number; stored: number }
function referenceV1(data: Uint8Array, chunkSize: number, fanout: number): string {
  const make = (block: Uint8Array, codec: number, size: number, descendants = 0): Node => ({
    cid: [1, codec, 18, 32, ...digest(block)], size, stored: block.length + descendants
  })
  let level: Node[] = []
  for (let at = 0; at < data.length; at += chunkSize) {
    const block = data.slice(at, at + chunkSize)
    level.push(make(block, 85, block.length))
  }
  if (level.length === 0) level.push(make(data, 85, 0))
  while (level.length > 1) {
    const next: Node[] = []
    for (let at = 0; at < level.length; at += fanout) {
      const children = level.slice(at, at + fanout)
      const size = children.reduce((sum, child) => sum + child.size, 0)
      const links = children.flatMap(child => field(18, [
        ...field(10, child.cid), ...field(18, []), 24, ...integer(child.stored)
      ]))
      const unixfs = [8, 2, 24, ...integer(size), ...children.flatMap(child => [32, ...integer(child.size)])]
      next.push(make(Uint8Array.from([...links, ...field(10, unixfs)]), 112, size,
        children.reduce((sum, child) => sum + child.stored, 0)))
    }
    level = next
  }
  return base32(Uint8Array.from(level[0].cid))
}
async function* stream(data: Uint8Array, sizes: number[], reuse = false, shared = false): AsyncGenerator<Uint8Array> {
  const capacity = Math.max(...sizes)
  const scratch = shared ? new Uint8Array(new SharedArrayBuffer(capacity)) : new Uint8Array(capacity)
  let index = 0
  for (let at = 0; at < data.length;) {
    if (index % 3 === 0) yield new Uint8Array(0)
    const size = Math.min(sizes[index++ % sizes.length], data.length - at)
    const part = data.subarray(at, at + size)
    if (reuse) {
      scratch.set(part)
      yield scratch.subarray(0, size)
      scratch.fill(255)
    } else yield part
    at += size
  }
  yield new Uint8Array(0)
}

it(`seeded layout and stream properties (seed=${seed}, rounds=${rounds})`, async () => {
  for (let run = 0; run < rounds; run++) {
    const chunkSize = 1 + random(128)
    const fanout = 2 + random(8)
    const boundary = fanout ** (1 + random(4)) * chunkSize
    const size = run % 2 ? random(4096) : Math.max(0, boundary + random(3) - 1)
    const data = bytes(size)
    const original = data.slice()
    const options = { chunkSize, maxChildrenPerNode: fanout }
    const expected = referenceV1(data, chunkSize, fanout)
    const context = `seed=${seed}, run=${run}, size=${size}, chunk=${chunkSize}, fanout=${fanout}`
    assert.equal(await hashV1WithLayout(data, options), expected, context)
    const sizes = [1 + random(256), 1 + random(256), 1 + random(256)]
    assert.equal(await hashV1WithLayout(stream(data, sizes), options), expected, context)
    assert.equal(await hashV1WithLayout(stream(data, sizes, true), options), expected, context + ', reused buffer')
    assert.equal(await hashV0(stream(data, sizes, true)), referenceV0(data), context)
    assert.deepEqual(data, original, context + ', input mutation')
  }
})

it('public layouts agree with independent SHA-256 and DAG encodings at boundaries', async () => {
  for (const size of [0, 1, 4095, 4096, 4097, 262143, 262144, 262145, 524287, 524288, 524289, 17 * 262144 + 7]) {
    const storage = bytes(size + 23)
    const data = storage.subarray(11, 11 + size)
    const expected = referenceV1(data, 262144, 174)
    assert.equal(await hashV1(data), expected, `size=${size}`)
    for (const reuse of [false, true]) {
      assert.equal(await hashV1(stream(data, [4093, 262145, 700001], reuse)), expected, `size=${size}, reuse=${reuse}`)
      assert.equal(await hashV0(stream(data, [4093, 262145, 700001], reuse)), referenceV0(data))
    }
    assert.equal(await hashV0(data), referenceV0(data))
  }
})

it('V0 streaming crosses the collection limit without losing bytes', async () => {
  for (const delta of [-1, 0, 1, 65537]) {
    const data = bytes(16 * 1024 * 1024 + delta)
    const expected = referenceV0(data)
    for (const sizes of [[data.length], [65537, 1048573]]) {
      assert.equal(await hashV0(stream(data, sizes, true)), expected, `delta=${delta}, sizes=${sizes}`)
    }
  }
})

it('V0 uses one platform digest up to the collection limit and none above it', { skip: !globalThis.crypto?.subtle }, async () => {
  const subtle = globalThis.crypto.subtle
  const nativeDigest = subtle.digest
  const digested: number[] = []
  subtle.digest = (algorithm, data) => {
    digested.push((data as Uint8Array).byteLength)
    return nativeDigest.call(subtle, algorithm, data)
  }
  try {
    const limit = 16 * 1024 * 1024
    const data = bytes(limit + 1)
    const atLimit = data.subarray(0, limit)
    assert.equal(await hashV0(stream(atLimit, [1048576])), referenceV0(atLimit))
    assert.deepEqual(digested, [limit])
    assert.equal(await hashV0(stream(data, [1048576])), referenceV0(data))
    assert.deepEqual(digested, [limit])
  } finally { subtle.digest = nativeDigest }
})

it('hashes reused shared-memory stream chunks before advancing the producer', async () => {
  const data = bytes(3 * 262144 + 1)
  for (const sizes of [[262144], [300001], [4093, 262145]]) {
    assert.equal(await hashV1(stream(data, sizes, true, true)), referenceV1(data, 262144, 174))
    assert.equal(await hashV0(stream(data, sizes, true, true)), referenceV0(data))
  }
})

it('propagates native digest failures and closes the stream without retrying reused bytes', { skip: !globalThis.crypto?.subtle }, async () => {
  const subtle = globalThis.crypto.subtle
  const nativeDigest = subtle.digest
  const failure = new Error('native digest failed')
  let closed = false
  let parts = 0
  subtle.digest = () => Promise.reject(failure)
  async function* reused() {
    const buffer = new Uint8Array(262144)
    try {
      for (let i = 0; i < 32; i++) {
        buffer.fill(i)
        parts++
        yield buffer
      }
    } finally { closed = true }
  }
  try {
    await assert.rejects(hashV1(reused()), error => error === failure)
    assert.equal(closed, true)
    assert.ok(parts <= 16)
    await assert.rejects(hashV0(new Uint8Array(4096)), error => error === failure)
  } finally { subtle.digest = nativeDigest }
})

it('preserves chunk order and bounds concurrent native digests', { skip: !globalThis.crypto?.subtle }, async () => {
  const subtle = globalThis.crypto.subtle
  const nativeDigest = subtle.digest
  let active = 0
  let peak = 0
  let calls = 0
  subtle.digest = async (algorithm, data) => {
    active++
    peak = Math.max(peak, active)
    const index = calls++
    try {
      const result = await nativeDigest.call(subtle, algorithm, data)
      await new Promise(resolve => setTimeout(resolve, index % 2 ? 0 : 5))
      return result
    } finally { active-- }
  }
  try {
    const data = bytes(40 * 4096 + 7)
    const options = { chunkSize: 4096, maxChildrenPerNode: 3 }
    assert.equal(await hashV1WithLayout(stream(data, [4096], true), options), referenceV1(data, 4096, 3))
    assert.equal(calls, 40)
    assert.ok(peak > 1 && peak <= 16, `peak=${peak}`)
    assert.equal(active, 0)
  } finally { subtle.digest = nativeDigest }
})

it('CID encodings, varints and both digest paths agree with independent encoders', async () => {
  for (const value of [0, 1, 127, 128, 255, 16383, 16384, 2 ** 31, 2 ** 32, Number.MAX_SAFE_INTEGER]) {
    assert.deepEqual(varint(value), integer(value))
  }
  for (let run = 0; run < rounds; run++) {
    const hash = digest(bytes(random(128)))
    assert.equal(qmText(hash), base58(Uint8Array.from([18, 32, ...hash])))
    for (const codec of [85, 112]) {
      const expected = Uint8Array.from([1, codec, 18, 32, ...hash])
      assert.deepEqual(cidV1(codec, hash), expected)
      assert.equal(cidV1Text(expected), base32(expected))
    }
    const value = random(Number.MAX_SAFE_INTEGER)
    assert.deepEqual(varint(value), integer(value))
  }
  for (const size of [0, 1, 4095, 4096, 4097, 262144]) {
    const data = bytes(size)
    assert.deepEqual(await sha256Digest(data), digest(data))
    const shared = new Uint8Array(new SharedArrayBuffer(size))
    shared.set(data)
    assert.deepEqual(await sha256Digest(shared), digest(data))
  }
})

it(`ADR canonicalization properties (seed=${seed})`, async () => {
  const names = ['', 'a', 'b', 'é', '😀', '\u0000', '__proto__', 'constructor', '10', '2']
  for (let run = 0; run < rounds; run++) {
    const content = Array.from({ length: random(20) }, () => ({
      file: names[random(names.length)], hash: names[random(names.length)]
    }))
    const metadata = { title: names[random(names.length)], run, nested: [null, true, { value: run }] }
    const original = structuredClone(content)
    const sorted = content.slice().sort((a, b) => a.hash < b.hash ? -1 : a.hash > b.hash ? 1 : a.file < b.file ? -1 : a.file > b.file ? 1 : 0)
    const expected = new TextEncoder().encode(JSON.stringify({
      content: sorted.map(entry => ({ key: entry.file, hash: entry.hash })), metadata
    }))
    assert.deepEqual(prepareADR32Data(content, metadata), expected)
    for (let i = content.length - 1; i > 0; i--) {
      const j = random(i + 1)
      ;[content[i], content[j]] = [content[j], content[i]]
    }
    const before = structuredClone(content)
    const current = await calculateMultipleHashesADR32(content, metadata)
    const legacy = await calculateMultipleHashesADR32LegacyQmHash(content, metadata)
    assert.deepEqual(current.data, expected)
    assert.deepEqual(legacy.data, expected)
    assert.equal(current.hash, referenceV1(expected, 262144, 174))
    assert.equal(legacy.hash, referenceV0(expected))
    assert.deepEqual(content, before)
    assert.deepEqual(prepareADR32Data(original, metadata), expected)
    const keys = ['title', 'nested', 'run']
    const picked = { title: metadata.title, nested: metadata.nested, run }
    assert.equal(keccak256Hash({ ...metadata, ignored: random(100) }, keys), keccak256Hash(picked, keys))
    assert.equal(keccak256Hash(metadata, [...keys, 'missing', 'title']), keccak256Hash(metadata, keys))
  }
})

it('propagates stream errors and closes invalid producers', async () => {
  for (const hash of [hashV0, hashV1]) {
    for (const size of [0, 1, 4096, 262145]) {
      const failure = new Error('producer failure')
      let closed = false
      async function* broken() {
        try { yield new Uint8Array(size); throw failure } finally { closed = true }
      }
      await assert.rejects(hash(broken()), error => error === failure)
      assert.equal(closed, true)
    }
    for (const value of [null, undefined, 42, {}, [], new Uint16Array(2), { [Symbol.asyncIterator]: 1 }]) {
      await assert.rejects(hash(value as never))
    }
    for (const invalid of ['text', null, undefined, new Uint16Array(8)]) {
      let closed = false
      async function* broken() {
        try { yield new Uint8Array(4096); yield invalid as never } finally { closed = true }
      }
      await assert.rejects(hash(broken()))
      assert.equal(closed, true)
    }
  }
})
