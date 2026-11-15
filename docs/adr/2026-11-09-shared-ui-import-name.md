# ADR: Shared UI import name

- Status: Accepted
- Date: 2026-11-09
- Amends: [Shared UI library](2026-10-30-shared-ui-library.md)

## Context

The [shared UI library ADR](2026-10-30-shared-ui-library.md) puts the shared
foundations and components in `library/ui`, imported as source, and names the
`@dcl/one-ui` package with its `/foundations` entry. It does not say how a
consumer resolves the name or which stylesheet declares Inter. `apps/sites`
and `apps/ui3` consume both foundations and components.

## Decision

The import name is `@dcl/one-ui`, the library's package name.
`import { theme, Button, textClasses } from '@dcl/one-ui'` resolves to
`library/ui/index.ts` (the foundations, the shared components and the class of
each text style, `text-styles.ts`); `@dcl/one-ui/<path>` to `library/ui/<path>`.
The Explorer HUD imports `@dcl/one-ui/foundations` only.
Nothing is published or installed: each consumer resolves the names in
TypeScript (`paths`) and in its build (`resolve.alias`, plus `server.fs.allow`
for Vite's dev server) and bundles the source with its own single React and
React DOM. `apps/sites`, `apps/ui3` and the Explorer HUD
(`rendering/bevy-explorer/source/react-web`, foundations only) do.

`library/ui/foundations/` is the consumer entry for tokens and text styles.
`foundations.css` carries the tokens (`colors/colors.css`,
`spacing/spacing.css`, `layout/layout.css`) and the library's Inter faces
(`typography/inter.css`, variable Inter from Git LFS). The HUD loads it, and so
does Sites: its root and the routes that use the foundations import it.
`apps/ui3/src/atoms/inter.css` holds the Inter v20 faces decentraland.org's
Shop loads, which the parity harness measures against;
`apps/ui3/src/atoms/primitives.css` imports it, and Sites' root loads that
sheet too. Readable token names alias the generated ones
and never redefine a value. The app serves the Inter woff2 files at `/fonts/`
(`apps/ui3/public/fonts`), so a page preloads them by a stable URL and a
non-bundled consumer loads the stylesheet unchanged.

## Consequences

- Build inputs and CI selection include `library/ui/index.ts`, `foundations/`
  and `components/`: the Nix Sites package source set lists them; the
  workflows that build or test the apps select on `library/**`.
- A consumer never copies a token value. A value without a token stays a
  literal with a comment naming the reference value, which
  `tooling/ui_standards.py --tokens` counts as drift.
- When promotion replaces `foundations/`, consumers keep importing
  `@dcl/one-ui`; the readable aliases are re-pointed, not the pages.

## Open points

- A Sites document declares the `Inter` family twice: the Shop's static faces
  from `atoms/inter.css` and the library's variable face from
  `typography/inter.css`. `atoms/inter.css` states that a second set of Inter
  rules shifts text rasterization by a sub-pixel, and no measurement in the
  tree compares the Shop pages with one set and with two. The next work is one
  declaration per document: the web apps load the token sheets without
  `typography/inter.css`, or `foundations.css` leaves the faces to its
  consumer.
