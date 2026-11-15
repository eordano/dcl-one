# Camera service compatibility boundaries

- Status: Accepted
- Date: 2026-11-13
- Reference contract: [camera-reel-service at `3e8bd0b099400e7c15b95477c1f003c63042d461`](https://github.com/decentraland/camera-reel-service/tree/3e8bd0b099400e7c15b95477c1f003c63042d461).

## Decision

Camera preserves its external request parsing, authentication, response shapes,
filename spelling and image transformation contracts inside its domain crate.
Its image dependency is the separately named `image-camera` package, pinned to
`image` 0.24.9 with the codecs in the captured source corpus. The workspace's
general image dependency remains independent. Uploads accept only PNG and JPEG,
and their signatures must match the declared MIME; decoding is bounded to
8,192 pixels per dimension and 128 MiB, and runs on blocking workers behind a
two-permit semaphore. The broader codec set remains for source compatibility
fixtures and is not reachable from the upload path. Codec changes require
comparisons of accepted inputs, error classes, thumbnail dimensions, decoded
pixels and output media types;
enabling the same format names alone does not establish equivalent behavior.
The bounded world-place cache uses Moka with explicit capacity and expiry.

Storage remains the shared immutable blob store. A photo row retains its
filename and accepted MIME, so two aliases for identical bytes can retain
different declared media types. Public URLs concatenate the configured base
and filename literally. The byte handler resolves each alias through the photo
row and applies moderation. Reupload may release deletion withholding, while
rejection remains durable until an explicit moderation change. Camera handler
verdict transactions and their in-process denylist projection are serialized;
blob I/O runs outside that critical section.

Photo events and their consumers use the existing database and job registry.
The Credits photo worker commits its receipt, progress, locally signed MANA
grant and Social notification together. Receipts are the acknowledgement;
a bounded scan round is only a scheduling aid and must revisit failed or
late-committing events even while new events arrive. Seasonal MANA grants are
separate from the paid USD wallet and its settlement path. Credits reads feature
flags with its own configured audience rather than borrowing another domain's
remote answer. No active season, signing key or financial balance is seeded.

## Consequences

The compatibility codec adds a second image-library dependency graph, without
changing other image consumers. Its locked dependencies require the same
advisory review as other externally reachable parsers. Source comparisons are
bounded evidence tied to the reference revision and captured fixtures, not a
claim that an unknown production deployment has been replaced.

Historical imports must preserve object bytes and supplied object MIME metadata;
they must not regenerate thumbnails or emit new-photo rewards. Existing nullable
fields remain unknown until real source metadata is supplied. Public-origin
routing, source inventories, financial handover and production cutover remain
deployment responsibilities; repository tests do not perform those operations.
