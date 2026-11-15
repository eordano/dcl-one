import { sha256 } from '@noble/hashes/sha2.js'
import { keccak_256 } from '@noble/hashes/sha3.js'
import { abytes } from '@noble/hashes/utils.js'
import { qmText, sha256Digest } from './cid.ts'
import { CHUNK_SIZE_BYTES, MAX_CHILDREN_PER_NODE, hashV1WithLayout, isAsyncIterable } from './layout.ts'

export type HashableContent = AsyncGenerator<Uint8Array> | AsyncIterable<Uint8Array> | Uint8Array

export interface EntityContentItemReference {
  file: string
  hash: string
}

export interface CalculateMultipleHashesResult {
  data: Uint8Array
  hash: string
}

const textEncoder = new TextEncoder()

// hashV0 copies a stream up to this size and hashes it in one call, which can use the platform's
// digest. A longer stream is hashed as it arrives, so memory stays bounded.
const V0_COLLECT_LIMIT_BYTES = 16 * 1024 * 1024

/**
 * The Qm-prefixed hash of the content: its sha2-256 digest as a multihash. It is not an IPFS CIDv0.
 * @deprecated use hashV1; this exists for entities deployed with the old hash.
 */
export async function hashV0(content: HashableContent): Promise<string> {
  if (content instanceof Uint8Array) return qmText(await sha256Digest(content))

  if (!isAsyncIterable(content)) {
    throw new Error(
      'Invalid value provided to hashV0. Expected AsyncGenerator<Uint8Array> | AsyncIterable<Uint8Array> | Uint8Array'
    )
  }

  let collected = new Uint8Array(65536)
  let size = 0
  let incremental: ReturnType<typeof sha256.create> | undefined

  for await (const part of content) {
    abytes(part)
    if (incremental !== undefined) {
      incremental.update(part)
    } else if (size + part.length > V0_COLLECT_LIMIT_BYTES) {
      incremental = sha256.create().update(collected.subarray(0, size)).update(part)
    } else {
      if (size + part.length > collected.length) {
        const grown = new Uint8Array(Math.min(V0_COLLECT_LIMIT_BYTES, Math.max(collected.length * 2, size + part.length)))
        grown.set(collected.subarray(0, size))
        collected = grown
      }
      collected.set(part, size)
      size += part.length
    }
  }

  return qmText(incremental !== undefined ? incremental.digest() : await sha256Digest(collected.subarray(0, size)))
}

export async function hashV1(content: HashableContent): Promise<string> {
  return hashV1WithLayout(content, { chunkSize: CHUNK_SIZE_BYTES, maxChildrenPerNode: MAX_CHILDREN_PER_NODE })
}

export function compareStrings(a: string, b: string): number {
  if (a < b) return -1
  if (a > b) return 1
  return 0
}

export function sortKeys(a: EntityContentItemReference, b: EntityContentItemReference): number {
  return compareStrings(a.hash, b.hash) || compareStrings(a.file, b.file)
}

export function prepareADR32Data(contents: EntityContentItemReference[], metadata?: unknown): Uint8Array {
  // slice, not a spread: a value that is not an array throws here
  const sorted = contents.slice().sort(sortKeys)
  return textEncoder.encode(
    JSON.stringify({ content: sorted.map((entry) => ({ key: entry.file, hash: entry.hash })), metadata })
  )
}

export async function calculateMultipleHashesADR32(
  contents: EntityContentItemReference[],
  metadata?: unknown
): Promise<CalculateMultipleHashesResult> {
  const data = prepareADR32Data(contents, metadata)
  return { data, hash: await hashV1(data) }
}

/** @deprecated the ADR-32 hash with the old Qm-prefixed hash. */
export async function calculateMultipleHashesADR32LegacyQmHash(
  contents: EntityContentItemReference[],
  metadata?: unknown
): Promise<CalculateMultipleHashesResult> {
  const data = prepareADR32Data(contents, metadata)
  return { data, hash: await hashV0(data) }
}

export function keccak256Hash(metadata: object, keys: string[]): string {
  const picked = Object.fromEntries(
    keys.filter((key) => Object.hasOwn(metadata, key)).map((key) => [key, (metadata as Record<string, unknown>)[key]])
  )
  const digest = keccak_256(textEncoder.encode(JSON.stringify(picked)))
  return Array.from(digest, (byte) => byte.toString(16).padStart(2, '0')).join('')
}
