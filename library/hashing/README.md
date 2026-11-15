# `@dcl/hashing`

Decentraland content hashing ([package ADR](../../docs/adr/2026-11-13-hashing-in-dcl-one.md)):

| Export | Result |
|---|---|
| `hashV1(content)` | UnixFS CIDv1: 262144-byte raw leaves, 174 children per node; `bafkrei…` for at most one chunk, `bafybei…` otherwise |
| `hashV0(content)` | Legacy `Qm…` SHA-256 multihash, not IPFS CIDv0 |
| `prepareADR32Data(contents, metadata?)` | ADR-32 JSON bytes |
| `calculateMultipleHashesADR32(contents, metadata?)` | `{ data, hash }` using V1 |
| `calculateMultipleHashesADR32LegacyQmHash(contents, metadata?)` | `{ data, hash }` using V0 |
| `keccak256Hash(metadata, keys)` | ADR-62 keccak-256 hex digest of selected fields |
| `sortKeys(a, b)`, `compareStrings(a, b)` | Content-reference and UTF-16 string comparators |

All exports are available at the root. Subpaths: `@dcl/hashing/node` exports V0/V1; `@dcl/hashing/ADR32` exports ADR-32 functions and comparators; `@dcl/hashing/ADR62` exports keccak. Each has TypeScript declarations. The export map exposes no other path: `@dcl/hashing/package.json` and `@dcl/hashing/dist/*`, which 3.0.4 allowed, do not resolve.

`content` accepts `Uint8Array` or an async iterable of it, including Node read streams. Producers may reuse buffers after yielding. Other stream-part types reject with `TypeError`.

## Semantics

Results match published 1.1.3, 1.2.0 and 3.0.4 for declared input types. Other inputs follow 1.2.0, except for stricter stream-part validation:

- ADR-32 copies its input, sorts by hash then path using UTF-16 code units, and retains duplicate entries. It does not validate string fields. Versions 1.1.3 and 3.0.4 instead sort in place and reject null fields.
- ADR-62 reads only own metadata fields in key order. Versions 1.1.3 and 3.0.4 also read inherited fields, affecting `__proto__` and custom prototypes.
- Text encodings, case, path separators and Unicode forms remain distinct. Metadata follows `JSON.stringify`: undefined is omitted; null is retained.

## Performance

`@noble/hashes` is the only dependency. SHA-256 uses WebCrypto from 4096 bytes, with at most 16 V1 digests in flight, consumed in order. Small inputs, tree nodes, shared-memory views and platforms without WebCrypto use JavaScript. Complete stream chunks avoid copying; partial bytes use one growing buffer. V0 collects at most 16 MiB, then switches to incremental JavaScript hashing. Native digest failures reject instead of retrying producer-owned bytes.

Node 24 measurements: V1 on 64 MB is about 3× faster than 1.1.3 for buffers, 2× for 64 KiB stream parts, and over 10× faster than 1.2.0 for both. At 300 bytes, V1 is about 7× faster than 1.1.3. V0 is about 1.5× slower than 1.1.3 for buffers and 6× slower for streams beyond 16 MiB; that version uses Node's native incremental digest.

## Build and test

Node ≥22.12 supports the CommonJS build's `require` of ESM-only `@noble/hashes`. Builds and type checks cache results under `node_modules/.cache`.

```sh
pnpm install --frozen-lockfile --ignore-scripts
pnpm build
pnpm typecheck
pnpm test
HASH_FUZZ_SEED=42 HASH_FUZZ_ROUNDS=10000 pnpm test
```

Tests include published hash vectors, deep trees, edge inputs and seeded properties against independent digest, CID and tree encoders. They cover stream partitioning, reused/shared buffers, boundaries, ADR ordering, unchanged inputs, errors and concurrent digest ordering. Default: 1000 rounds. Export tests compile fresh CommonJS, ESM and TypeScript consumers.
