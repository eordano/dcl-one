# ADR: The Explorer's Bevy is vendored in this repository

- Status: Superseded by [2026-11-14](2026-11-14-explorer-sources-as-patches.md)
- Date: 2026-11-05

## Context

The Bevy Explorer built every Bevy crate over git from `robtfm/bevy`, branch `release-0.16-dcl`,
locked at `dac1ac9`: upstream `v0.16.1` plus 102 commits (+7,848/−1,336 lines in 180 files), all
of them explorer changes (`rendering/bevy-explorer/docs/guidelines/bevy-fork.md`). A Bevy change
shipped as a push to that branch, which only its owner could make, plus a lockfile bump PR here.

The next explorer changes are in Bevy itself: glTF instances that spawn a slice of their nodes per
frame need a resumable writer in `bevy_scene`'s spawner. Every such change would take two
repositories, two reviews and a lockfile bump, and the fork's history would stay apart from the
code that depends on it.

## Decision

Vendor the fork at `rendering/bevy-explorer/bevy`, one import of the tree at `dac1ac9`, and change
it in place from now on. This repository is the source of truth; the fork branch stays frozen.

- **What is imported:** the whole tree except `assets/` (358 files, 55 MB of example assets, which
  Bevy's own crates.io package leaves out too), the symlink `examples/wasm/assets` into it, and
  the fork's `.gitattributes`, whose `* text=auto` would override the root's `-text` rule for the
  subtree. Every other file is the fork's blob, byte for byte. Bevy's own workspace stays
  loadable, so its crates' tests can run in it.
- **Where:** beside `source/`, outside the Explorer's Cargo workspace. It is no workspace member,
  needs no `exclude`, and the Explorer's Clippy, Rustfmt and Taplo gates do not reach it.
  The native cache keeps its artifacts: `tooling/bevy-target-cache.py` removes only final binaries and reports.
- **Cargo:** `[patch.crates-io]` maps the 18 Bevy crates that ecosystem crates name by version to
  paths in the tree, and `[patch."https://github.com/robtfm/bevy"]` maps `bevy` for `bevy_dui`,
  `bevy_ecss` and `boimp`, which name the fork by git. `Cargo.lock` changes only by the 47 Bevy
  packages losing their git source.
- **Warnings:** a path dependency's rustc warnings are not capped, as a git dependency's are. The
  vendored root `Cargo.toml` allows `dead_code` (code our feature set leaves unused) and
  `mismatched_lifetime_syntaxes` (a lint newer than Bevy 0.16), with a comment saying why. They
  made up 810 of the 835 warnings Bevy's 47 crates printed in a native workspace check; the
  complete check afterwards prints 30, from six other lints. This is the only change to the fork's
  files.
- **Tooling:** the paths that keyed on `source/` also cover the tree: `.gitattributes`,
  `tooling/bevy-ci.py` (a Bevy change selects every lane, the Windows and macOS matrix included),
  `tooling/play-artifact.py` and `infra/packages/play.nix` (Bevy is a web engine input), and
  `tooling/ci-local.py`. `forks.toml` and `cargo-patches.toml` no longer list Bevy, since the lock
  has no Bevy git source.
- **Changes after the move:** an ordinary PR under `rendering/bevy-explorer/bevy`, updating
  `bevy-fork.md` in the same PR. Upstream fixes are backported with
  `git format-patch | git apply -3 --directory=rendering/bevy-explorer/bevy`, naming the upstream
  PR.

## Rejected options

- **Subtree with history:** imports Bevy's 8,842-commit history, and this repository merges by
  squash only, which flattens it anyway.
- **The fork's 102 commits replayed on a pristine 0.16.1 import:** the same squash problem.
- **Upstream 0.16.1 plus patch files** (the earlier dcl-one engine kept 27): the patches must be
  applied before Cargo can resolve, and a Bevy change becomes an edit to a patch file.
- **`rendering/bevy-explorer/source/vendor/bevy`:** the `source/` tooling would cover it
  unchanged, but it needs a workspace `exclude` and a Taplo exclusion, `play-artifact.py` rejects
  its symlink, and a Bevy change would count as platform-neutral source.
- **The whole tree with `assets/`:** 55 MB no build or test reads, two thirds of the repository's
  size.
- **The fork kept upstream and re-imported:** two sources of truth, and every change made here
  would conflict with the next import.
- **A `cargo vendor` directory source:** fingerprinted by version, so local edits would not
  rebuild.
- **For the CI cache, `sccache` on every native job** (Windows has it): a cache request per
  compiled crate, and every object in the repository's shared 10 GB cache.
- **For the CI cache, `-Zchecksum-freshness`:** still nightly-only in Cargo 1.99.1, so the native
  jobs cannot use it.
- **For the CI cache, file times pinned to the last commit date:** a cache built after an older
  commit would make a changed file look older than its stale artifact.
- **For the CI cache, file times pinned to one date with Bevy's tree hash in the cache key:**
  correct, but every Bevy change would lose the whole dependency cache.

## Verification

- `git diff --name-status dac1ac9^{tree} <import commit>:rendering/bevy-explorer/bevy` lists only
  the 360 omitted entries; `git ls-files --eol rendering/bevy-explorer/bevy` shows `-text`, with
  the same endings in the index and the working tree.
- The resolved graph (crates, versions, enabled features, edges and dependency kinds) is identical
  before and after, for every platform (1,332 packages) and for `wasm32-unknown-unknown` (1,030),
  with the same 44 workspace members; `cargo metadata --locked` accepts the lock.
- With the lock, a cargo home holding every other source but not the fork's resolves offline; the
  same home fails on a checkout that still uses the git fork.
- `cargo clippy --locked -- -D warnings` passes: a path dependency outside the workspace is built
  by plain rustc, so Clippy's lints never run on Bevy.
- On the full workspace, after every vendored file was set to the current time and restored by
  `tooling/bevy-mtimes.py`, `cargo check -v` reported all 56 Bevy units fresh. In a two-crate
  reproduction of a CI cache restore, the path dependency stayed fresh with unchanged content
  and rebuilt with changed content. `tooling/tests/test_bevy_mtimes.py` pins the behaviour.
- The tooling tests pin the new inputs: `test_vendored_bevy_selects_every_lane_and_the_native_matrix`,
  `test_vendored_bevy_rebuilds_the_images`, `test_vendored_bevy_edits_invalidate_the_artifact`,
  `test_vendored_bevy_is_an_engine_input_in_git_and_in_the_nix_contract`.

Binaries are not bit-identical: Bevy's absolute source path changes inside `file!()`, panic
locations and debug info. `embedded_asset!` paths do not change, since they are taken from the
nearest `src` directory.

## Consequences

- Bevy changes are reviewed and merged here, with the code that uses them.
- CI keeps a cached Bevy build. Cargo checks a path dependency's freshness by file times, and a
  fresh checkout makes every file new, so `tooling/bevy-mtimes.py` records each vendored file's
  blob id and time, floored to whole seconds, inside the cached target directory, and after the
  next checkout gives every unchanged file its recorded time back. To Cargo that is a checkout
  that never moved: only crates whose files changed rebuild. Every native job and the web target
  cache run it; the web target cache's key also hashes Bevy, so a Bevy change refreshes it. The
  web engine rebuilds exactly when Bevy or the Explorer changes.
- The fork branch must stay fetchable while `bevy_dui`, `bevy_ecss` and `boimp` name it: a
  resolve without `Cargo.lock` reads it once.
- The drift site no longer tracks Bevy against upstream; an upgrade is a planned project, and its
  delta is `git diff v0.16.1^{tree} HEAD:rendering/bevy-explorer/bevy`.
