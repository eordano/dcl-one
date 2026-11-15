# ADR: Image cache reuse and retention

- Status: Proposed
- Date: 2026-11-08

## Context

Image builds need dependency fallback after lockfile changes without restoring
unbounded Cargo target snapshots. Cache uploads must replace stale identities,
avoid duplicate main/PR copies, and retain a bounded number of generations.

## Decision

Only PR Catalyst and Sites dependency restores may fall back from the exact main
identity to the group's main prefix. Main, tag and other normal branch builds
require exact main identities because imports are unsigned. PRs cannot publish.
Exact keys do not authenticate cache writers; publishing refs must run trusted code
because their GitHub cache scope is shared across workflows.
Save when the matched derivation digest differs from the primary digest, including
a miss. An exact main identity restored by a PR needs no duplicate save. A stale
fallback must not prevent saving the replacement.

Catalyst exports retain both compiled dependencies and vendored sources. Sites
exports retain dependencies and the toolchain. API and Sites app exports contain the
runtime closure and use exact identities, without a stale prefix fallback. The Sites
app excludes the Explorer build and the docs media, which are assembled beside it, so
its identity changes only with Sites sources and dependencies. Its entry holds the
compiled app of every Sites image the build produces, so an exact hit leaves no Sites
compile. A runtime closure
restored under its exact identity makes that package's dependency restore redundant;
the build skips it and saves no dependency entry it never restored.
Stale Catalyst archives retain vendored sources but do not avoid recompiling a
changed dependency derivation. The whole archive is transferred; this is a source
availability fallback, not a claim of incremental compiled-artifact reuse.
Before API export, both cache and image references must evaluate successfully to
the same Catalyst derivation, with the image reference evaluated using `--impure`.

The hourly trusted sweep retains two image entries per group, architecture and
main/open-PR ref, ordered by creation time and cache ID. It requires the key's
scope to match the ref. PR closure removes all entries on its merge ref. Only IDs
in the completed listing are deleted, so a concurrent upload absent from that
listing survives until a later sweep.

Image uploads require a first attempt and a current-head SHA check after
smoke tests. Main compares its ref with the built commit. PRs compare
`refs/pull/N/head` with the event's PR head SHA, allowing a save when only the base
advanced and regenerated the merge ref. Cache keys still identify the actual
built derivations, so a changed base cannot turn a stale cache into an exact hit.
Reruns and builds whose head was superseded at this check skip uploads;
failed ref lookups also skip uploads without failing image validation. Ref changes
during export/upload can still reorder entries. Keeping two generations limits
that race's impact, without guaranteeing a hit or a byte-size bound.

## Consequences

Main/tag dependency changes can incur cold rebuilds without a stale fallback.
Tags from older main commits can also miss when their exact cache generations have
been pruned. Fully cold ARM64 dependency builds have not been timed; exceeding the
100-minute limit saves no replacement and can leave subsequent runs cold.
Performance measurements and run evidence belong in working
reports and the PR; the cache rules do not imply a particular speedup.
