import { sha256 } from '@noble/hashes/sha2.js'
import { abytes } from '@noble/hashes/utils.js'
import { DAG_PB_CODEC, RAW_CODEC, cidV1, cidV1Text, sha256Digest, varint } from './cid.ts'
import type { HashableContent as Content } from './index.ts'

export const CHUNK_SIZE_BYTES = 262_144
export const MAX_CHILDREN_PER_NODE = 174

const MAX_CHUNKS_IN_FLIGHT = 16

interface DagNode {
  cid: Uint8Array
  // bytes of file content below this node
  fileSize: number
  // bytes of the subtree as stored, which a link carries as Tsize
  storedSize: number
}

export function isAsyncIterable(content: unknown): content is AsyncIterable<Uint8Array> {
  return (
    (typeof content === 'object' || typeof content === 'function') &&
    content !== null &&
    Symbol.asyncIterator in content
  )
}

function rawLeaf(digest: Uint8Array, size: number): DagNode {
  return { cid: cidV1(RAW_CODEC, digest), fileSize: size, storedSize: size }
}

// A dag-pb node: one link per child (hash, empty name, Tsize), then the UnixFS file
// header (type 2, file size, one block size per child).
function buildParent(children: DagNode[]): DagNode {
  const links: number[] = []
  const blockSizes: number[] = []
  let fileSize = 0
  let storedSize = 0

  for (const child of children) {
    const link = [0x0a, child.cid.length, ...child.cid, 0x12, 0x00, 0x18, ...varint(child.storedSize)]
    links.push(0x12, ...varint(link.length), ...link)
    blockSizes.push(0x20, ...varint(child.fileSize))
    fileSize += child.fileSize
    storedSize += child.storedSize
  }

  const data = [0x08, 0x02, 0x18, ...varint(fileSize), ...blockSizes]
  const block = Uint8Array.from([...links, 0x0a, ...varint(data.length), ...data])
  // A node block is a few kilobytes, so the synchronous digest is the fast one here.
  return { cid: cidV1(DAG_PB_CODEC, sha256(block)), fileSize, storedSize: storedSize + block.length }
}

function* chunksFromBuffer(content: Uint8Array, chunkSize: number): Generator<Uint8Array> {
  for (let offset = 0; offset < content.length; offset += chunkSize) {
    yield content.subarray(offset, Math.min(offset + chunkSize, content.length))
  }
}

// The stream's bytes as chunks. Bytes that do not fill a chunk are copied into `partial`, so the
// stream can use its buffer again after each part. A whole chunk inside one part is not copied:
// the digest reads it before the stream gives the next part.
async function* chunksFromStream(content: AsyncIterable<Uint8Array>, chunkSize: number): AsyncGenerator<Uint8Array> {
  let partial: Uint8Array | undefined
  let filled = 0
  // Most streams are shorter than a chunk, so the first buffer starts small and grows.
  let firstSize = Math.min(chunkSize, 4096)

  function room(needed: number): Uint8Array {
    if (partial !== undefined && needed <= partial.length) return partial
    const grown = new Uint8Array(Math.min(chunkSize, Math.max(needed, partial === undefined ? firstSize : partial.length * 2)))
    if (partial !== undefined) grown.set(partial.subarray(0, filled))
    return grown
  }

  for await (const incoming of content) {
    abytes(incoming)
    if (incoming.length === 0) continue
    let offset = 0
    if (partial !== undefined) {
      const size = Math.min(chunkSize - filled, incoming.length)
      partial = room(filled + size)
      partial.set(size === incoming.length ? incoming : incoming.subarray(0, size), filled)
      filled += size
      offset = size
      if (filled === chunkSize) {
        yield partial
        partial = undefined
        filled = 0
        firstSize = chunkSize
      }
    }
    while (incoming.length - offset >= chunkSize) {
      yield incoming.subarray(offset, offset + chunkSize)
      offset += chunkSize
      firstSize = chunkSize
    }
    // Here no partial chunk is open: the bytes left start a new one.
    if (offset < incoming.length) {
      filled = incoming.length - offset
      partial = room(filled)
      partial.set(offset === 0 ? incoming : incoming.subarray(offset))
    }
  }

  if (partial !== undefined) yield partial.subarray(0, filled)
}

// The balanced layout of ipfs-unixfs-importer, built as the leaves arrive: a full level
// folds into a parent at once, so memory is bounded by tree depth times the fan-out.
class BalancedTreeBuilder {
  private readonly levels: DagNode[][] = [[]]
  private readonly maxChildrenPerNode: number
  private leafCount = 0

  constructor(maxChildrenPerNode: number) {
    this.maxChildrenPerNode = maxChildrenPerNode
  }

  addLeaf(digest: Uint8Array, size: number): void {
    this.leafCount++
    this.pushAt(0, rawLeaf(digest, size))
  }

  finalize(): DagNode {
    if (this.leafCount === 0) return rawLeaf(sha256(new Uint8Array(0)), 0)
    if (this.leafCount === 1) return this.levels[0][0]

    let carry: DagNode | undefined
    for (let i = 0; i < this.levels.length; i++) {
      const level = this.levels[i]
      if (carry !== undefined) level.push(carry)
      if (level.length === 0) {
        carry = undefined
        continue
      }
      // A single node on the top level is the root. A single node on a lower level still
      // gets a parent, as the importer wraps a lone straggler on its way up.
      const isTopmost = i === this.levels.length - 1
      carry = isTopmost && level.length === 1 ? level[0] : buildParent(level)
    }

    if (carry === undefined) throw new Error('hashV1: the tree produced no root')
    return carry
  }

  private pushAt(level: number, node: DagNode): void {
    if (level === this.levels.length) this.levels.push([])
    const bucket = this.levels[level]
    bucket.push(node)

    if (bucket.length === this.maxChildrenPerNode) {
      const parent = buildParent(bucket)
      bucket.length = 0
      this.pushAt(level + 1, parent)
    }
  }
}

async function hashChunks(
  chunks: Iterable<Uint8Array> | AsyncIterable<Uint8Array>,
  maxChildrenPerNode: number
): Promise<string> {
  const builder = new BalancedTreeBuilder(maxChildrenPerNode)
  const inFlight: Array<{ digest: Uint8Array | Promise<Uint8Array>; size: number }> = []

  for await (const chunk of chunks) {
    inFlight.push({ digest: sha256Digest(chunk), size: chunk.length })
    if (inFlight.length === MAX_CHUNKS_IN_FLIGHT) {
      const leaf = inFlight.shift()!
      builder.addLeaf(await leaf.digest, leaf.size)
    }
  }
  for (const leaf of inFlight) builder.addLeaf(await leaf.digest, leaf.size)

  return cidV1Text(builder.finalize().cid)
}

export async function hashV1WithLayout(
  content: Content,
  options: { chunkSize: number; maxChildrenPerNode: number }
): Promise<string> {
  const { chunkSize, maxChildrenPerNode } = options

  if (content instanceof Uint8Array) {
    if (content.length <= chunkSize) return cidV1Text(cidV1(RAW_CODEC, await sha256Digest(content)))
    return hashChunks(chunksFromBuffer(content, chunkSize), maxChildrenPerNode)
  }
  if (isAsyncIterable(content)) return hashChunks(chunksFromStream(content, chunkSize), maxChildrenPerNode)

  throw new Error(
    'Invalid value provided to hashV1. Expected AsyncGenerator<Uint8Array> | AsyncIterable<Uint8Array> | Uint8Array'
  )
}
