# ADR: Hashing in `library/hashing`

- Status: Accepted
- Date: 2026-11-13

`library/hashing` owns `@dcl/hashing`. Its only runtime dependency is `@noble/hashes`; CID text, dag-pb and UnixFS encoding are implemented here. The [README](../../library/hashing/README.md) defines the public API, compatibility, input semantics, performance and tests. The exports map provides the root, `./node`, `./ADR32` and `./ADR62`, with declarations; ADR-32 and root exports include `sortKeys` and `compareStrings`.

SDK, bridge-scene and editor-scene pnpm workspaces include the local package and override every `@dcl/hashing` dependency with `workspace:*`. This covers transitive dependencies of `dcl-catalyst-client` and published SDK commands. Build-time resolution checks verify both callers load this implementation.

SDK installation builds protocol and hashing before compiling SDK commands. `make prepare` replaces local protocol/hashing links with caller-supplied `PROTOCOL_VERSION` and `HASHING_VERSION`; CI uses `0.0.0-unpublished.<sha>`. Missing versions fail packaging. The hashing package remains private until published from this repository. A release publishes `@dcl/hashing` from this repository first and gives that version to `make prepare`, which does not look the version up on npm.

CommonJS loading of ESM dependencies requires Node ≥22.12; SDK commands declares that minimum. Incremental compiler state stays in `node_modules/.cache`. `structure.yml` runs hash vectors, seeded properties and export-consumer tests.
