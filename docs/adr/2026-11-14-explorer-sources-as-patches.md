# ADR: The Explorer's Bevy and forked crates are an upstream commit plus patches

- Status: Accepted
- Date: 2026-11-14
- Supersedes: [The Explorer's Bevy is vendored in this repository](2026-11-05-explorer-vendored-bevy.md)

## Context

The Explorer builds 18 third-party sources that differ from their upstream. Bevy was a tracked
tree of 1,960 files. The 17 others were git dependencies on fork repositories outside the
organisation. The change over upstream was not visible in this repository, and a build needed
those repositories.

## Decision

Track each source as an upstream commit and an ordered patch series. A tool writes the rebuilt
trees, which git ignores.

- **Tracked:** `rendering/bevy-explorer/patches/<name>/source.toml` and
  `series/NNNN-<slug>.patch`. One patch is one commit of the former fork. `source.toml` records
  the upstream commit, the licence of the source (`license`) and the git tree id before and after
  the series.
- **Rebuilt:** `rendering/bevy-explorer/bevy` for `bevy`, `rendering/bevy-explorer/forks/<name>`
  for every other source.
- **Tool:** `tooling/patched-sources.py`, Python standard library and git only. `materialise`
  writes what is missing or stale, `verify` checks the tree ids, and `export NAME -m "subject"`
  turns edits in a rebuilt tree into the next patch
  ([commands](../../rendering/bevy-explorer/docs/guidelines/bevy-fork.md#changing-bevy)).
- **In-tree crates:** `bevy_dui`, `boimp` and `rpc-rust` (package `dcl-rpc`) are exact upstream
  bytes under `rendering/bevy-explorer/vendor/<directory>`. Each manifest records its licence.
  They sit outside `source/` because `rust-cache` drops the artifacts of path crates inside the
  workspace root.
- **Cargo:** every forked crate resolves to a path. The gltf pin is the only git source left in
  `Cargo.lock`. The package set and versions do not change.
- **Stale trees:** build scripts and CI run `materialise` first. The build script of the `common`
  crate fails with one line that names the command.
- **CI cache key:** `apps/bevy-explorer/prepare-native-cache.py` puts the git tree ids of the
  patch series and the in-tree crates into the exact key of `rust-cache`, not its restore key. A
  patch change saves a new cache, and a job without an exact match still restores the newest one.
- **Line endings:** `.gitattributes` sets `-text -whitespace` on the patch series and the in-tree
  crates.
- **Nix:** `infra/packages/bevy-explorer.nix` builds each patched source as one derivation with
  the same tool, then the Explorer in three layers: third-party crates, Bevy and the crates that
  use it, the Explorer.

## The superseded decision's reasons against patch files

- **"The patches must be applied before Cargo can resolve."** This stays true, and the cost is one
  command. `materialise` works offline after the first fetch, and the build scripts and CI run it.
  A build on a stale tree stops with one line that names the command, within the limits under
  Consequences.
- **"A Bevy change becomes an edit to a patch file."** Nobody edits a patch file. A change is an
  edit in the rebuilt tree, and `export` writes it as the next patch. When the series changes
  below an edited tree, `materialise` applies the edits again.

The step buys what a tracked tree does not give: the change over upstream of all 18 sources is in
this repository, and no build needs a fork repository.

## Rejected options

- **Bevy as a tracked tree, forks as git pins:** the forks' changes stay outside this repository.
- **Every fork as a tracked tree:** 55 MB and 7,000 files more, and the change over upstream is
  still not visible.
- **Git submodules:** the fork repositories stay.

## Verification

- `verify` rebuilds all 18 sources. Bevy's tree id is the id of the tree tracked before, and each
  fork's id is the id of its former revision.
- `Cargo.lock` has the same 1,341 packages and versions. A locked resolve fetches only gltf.
- `cargo check --locked --workspace` and `cargo clippy --locked -- -D warnings` pass for native
  Linux.
- A fresh checkout rebuilds the 18 trees offline from the cache, and git reports no change.

## Consequences

- A change to Bevy or a fork is an edit in the rebuilt tree, then `export`, then a PR that adds
  one patch file.
- A fresh checkout fetches the upstream commits once: 156 MB in a cache. After that a full
  rebuild takes 8 s.
- An upgrade is a new `commit` in `source.toml`, its `nix_hash` and a series that applies to it.
- The stale-tree check covers the packages that depend on `common`. A workspace member that does
  not is compiled from the trees on disk without it.
- With no rebuilt tree at all, Cargo fails when it loads the manifests, before any build script:
  ``patch location `...` does not contain packages matching `bevy` ``. The message does not name
  `materialise`.
- An evaluation of the Nix package builds the patched sources, because the first layer reads
  their manifests. `nix flake check` evaluates `packages` and `checks`, so the package is under
  `legacyPackages`: `nix build .#bevy-explorer` and `.#bevy-explorer.tests` find it there.
- CI does not build the Nix package. Nothing in CI proves `nix_hash` or the Nix layers.
- Four crate manifests still name three former forks by git. With `Cargo.lock` nothing reads
  those repositories.
- The build script of `client-sdk-rust` still downloads a prebuilt libwebrtc from a release of the
  former fork, as before.
- Cargo does not cap the warnings of a path dependency. The last patch of each forked source
  allows them, and the three in-tree crates print theirs.
