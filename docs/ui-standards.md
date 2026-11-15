# UI standards

Normative standards for web UI code in this repository. Each section records a
decision that has been made; areas without a decision say so instead of
guessing. The first standard defined here is styling.

## Contents

- [Relation to the contributor guides](#relation-to-the-contributor-guides)
- [Styling: CSS Modules](#styling-css-modules)
  - [Scope](#scope)
  - [Files and imports](#files-and-imports)
  - [Design tokens](#design-tokens)
  - [Global CSS boundary](#global-css-boundary)
  - [Variants and state](#variants-and-state)
  - [Runtime values](#runtime-values)
  - [Composition](#composition)
  - [Responsive, themes and portals](#responsive-themes-and-portals)
  - [Accessibility](#accessibility)
  - [Migrating existing styles](#migrating-existing-styles)
  - [Verification](#verification)
- [App-specific UI](#app-specific-ui)
- [Shared UI: `library/ui`](#shared-ui-libraryui)
  - [Structure](#structure)
  - [Foundations](#foundations)
  - [Explorer HUD transition](#explorer-hud-transition)
  - [Integration](#integration)

## Relation to the contributor guides

The current contributor guides reflect the standards in production. This document defines the new standards for One. State, data fetching and structure are in [web app architecture standards](web-app-standards.md), and tests in [frontend testing standards](frontend-testing.md).

## Styling: CSS Modules

**Decision:** component styles are written as CSS Modules.

### Scope

- Applies to every current and future web app under `apps/` and to
  `rendering/bevy-explorer/source/react-web`. The Explorer HUD keeps its own
  design primitives and tokens, which both of its hosts (`react-web` and the
  Sites adaptation `apps/ui3/src/play`) import from `library/ui/apps/hud`;
  see [Explorer HUD transition](#explorer-hud-transition).
- Required for all new component styles and for existing styles when they are
  migrated. Existing global stylesheets do not have to be migrated all at once.

### Files and imports

- Colocate the module with its component: `Component.tsx` next to
  `Component.module.css`.
- Import the class map and apply classes from it:
  `import styles from './Component.module.css'`, then
  `className={styles.root}`.
- Write ordinary CSS. No preprocessor or CSS-in-JS syntax.
- Do not introduce Emotion, styled-components, MUI `styled`, or another
  CSS-in-JS layer for new styling.

Start a shared component from the reusable template at
`library/ui/templates/component/`. Copy and rename the files into the new
component's directory under `library/ui/components/`:

```text
Component.tsx          React component and its props and types
Component.module.css   component styles
Component.test.tsx     behavior and accessibility tests
Component.figma.ts     Figma-to-component mapping for the tooling
```

Define component-owned props and types in the component file by default. Use a
separate types file for a substantial contract shared across components.
Export the component and its public prop types from `library/ui/index.ts`.
The mapping is used by the Figma tooling and is not imported by browser code.
The template is not a production component. Replace the Figma mapping
placeholder with a mapping in the contract the tooling reads
(`library/ui/figma/src/contract/mapping-file.ts`, described in
`library/ui/figma/MAINTENANCE.md`): header comments `url`, `source`,
`component` and `contract=1`, then the component's Figma properties.
`components/NavBar/NavBar.figma.ts` is a working example. The sample test is a
smoke test; React Testing Library, jest-dom matchers and CSS Module handling
are already configured in `library/ui` (`vitest.config.ts`, `test/setup.ts`),
so add the component-specific interaction and accessibility tests before use.

### Design tokens

Use shared design tokens through CSS custom properties, for example
`color: var(--text)`. Use semantic tokens appropriate to the component's
context and theme rather than hardcoding values or copying them from another
surface. CSS Modules scope class names, not custom properties: ensure the
required token scope is available wherever the component renders, including
portals.

### Global CSS boundary

Shared foundations and components belong in `library/ui`; a surface keeps
its existing ones until they are migrated there (see
[Shared UI](#shared-ui-libraryui)). Global stylesheets
contain only token definitions, font faces, resets, and application-wide base
and accessibility rules. Each app keeps its own global entry point and loads
the shared foundations it needs.

When migrating a shared style or component to `library/ui`, update its
consumers and remove the replaced implementation from the old location.

Component selectors belong in modules. Avoid:

- routine `:global(...)` selectors;
- ancestor overrides such as `.page .button { ... }`;
- selectors that reach into another component's internals, including a
  shared component from `library/ui` styled from an app.

When a legacy global class has to keep working, keep it deliberately, write
down which consumer depends on it, and verify that consumer after the change.

### Variants and state

Control component variants and states through props and component CSS, not
global overrides. Disabled controls must actually be disabled, and existing
states such as focus and hover must be preserved.

### Runtime values

Inline styles are allowed for values that are only known at runtime, such as
progress, position or a computed size. Pass them as a narrowly scoped custom
property and let the module consume it; fixed appearance stays in the module.

```tsx
import type { CSSProperties } from "react";

<div
  className={styles.track}
  style={{ "--progress": `${pct}%` } as CSSProperties}
/>;
```

Shared components expose runtime geometry through typed props mapped to custom
properties, while their appearance stays in the module. Do not use runtime
values to pass arbitrary colours or palette values from props.

### Composition

- Build components from custom browser primitives (native elements, ARIA attributes and DOM events). Do not introduce Base UI or another component kit, headless or styled, as the base of a component.
- Once `library/ui` is implemented and adopted by a surface, reuse its shared
  components before creating a new one, and put components shared by multiple
  apps or the Bevy HUD there. Until then, reuse the surface's existing
  components.
- Once `library/ui` is implemented, put components and styles used by one app
  only in `library/ui/apps/<app>/`; until then they stay in the app. See
  [App-specific UI](#app-specific-ui).

### Responsive, themes and portals

- Use the breakpoint definitions in `library/ui` foundations once adopted; retain each surface's breakpoints until then. CSS custom properties cannot be used directly in media conditions.
- Select themes through the shared semantic token mechanism. The supported
  themes and the selection mechanism's details remain to be defined.
- Carry the required theme scope into portal content rendered outside its
  original subtree.
- HUD overlays must account for runtime viewport scaling.

### Accessibility

Components need visible focus, keyboard interaction, labels, sufficient
contrast and reduced-motion support. Do not override reduced-motion rules
from a component module.

### Migrating existing styles

A migration covers one bounded component and its consumers, and is scheduled
on purpose. Before changing anything, inventory:

- every reference to the component's global classes, including other
  stylesheets and tests;
- specificity and import order the current look depends on;
- theme scopes and portal contexts the component renders in.

Keep the public props and any legacy class hook until its callers are
migrated. Renaming `component.css` to `component.module.css` is not a
migration. Do not rewrite unrelated components or tokens in the same change.

### Verification

- Run the affected interaction tests and verify each consuming surface.
- Check each applicable theme and representative width in a browser, including
  portals and server-rendered layouts where applicable.
- Compare visual behavior before and after a change; unit tests alone do not
  prove visual parity.
- `make check` and CI run `tooling/ui_standards.py` on changed files. It flags
  new CSS-in-JS imports (including an Emotion `@jsxImportSource` pragma), and a
  new plain `.css` file with the same name as a component next to it. An app's
  entry stylesheet (`App.css`, `index.css` or `main.css` directly in its
  `src/`) and moved existing stylesheets are not flagged. It skips the scene
  SDK toolchain's
  `apps/dcl-scene-cli/scripts/`, `src/templates/` and `testdata/`. It is a
  best-effort check: a pass does not prove the change follows this standard.
- `make check` and CI also run `tooling/ui_standards.py --token-floor`. For
  each stylesheet of the surfaces that use the shared foundations (the
  `FLOOR_PATHS` of the script) it fails when the `var(--dcl-` uses drop
  below, or the uses of an `apps/ui3` `atoms/primitives.css` property rise
  above, the counts in `tooling/ui-token-floor.json`, and when that file
  records a stylesheet that is not there. After a deliberate change (a moved
  or removed stylesheet, a count that must change), run
  `python3 tooling/ui_standards.py --write-token-floor` and commit the file
  with the change.
- Report checks that could not run or failed. Documentation-only changes need
  link, structure and content checks, not an application build.

## App-specific UI

App-specific UI is used by one app only: its layouts, app chrome and feature
components, with their styles. It lives in `library/ui/apps/<app>/`, apart from
the shared foundations and components. The app itself, under `apps/<app>`,
keeps its pages, routes and data wiring and imports its UI from there. Until
`library/ui` is implemented, app-specific UI stays where it is in the app.

Until `library/ui` is implemented, Sites imports some UI source through the
`@ui/*` alias (`apps/sites/tsconfig.json`, resolving to `apps/ui3/src`). Verify the
actual importing surfaces of a component before assuming it has more than one
consumer, and verify a change to it in every app that imports it.

- Follow the same CSS Modules, accessibility, migration and verification rules
  as shared UI.
- Compose it from shared components and foundations: those in `library/ui`
  once the app has adopted it, the app's existing ones until then. Do not
  restyle a shared component's internals from the app; when the app needs a
  different look, add or extend a variant on the shared component.
- Use shared tokens rather than hardcoded values or values copied from another
  app.
- Each app keeps its own global entry point for its resets and base rules.
- When a second app or the Bevy HUD needs the same component or foundation,
  move it from `library/ui/apps/<app>/` (or from the app) to the shared `components/` or `foundations/`, update its
  consumers and remove the old copy.

## Shared UI: `library/ui`

The shared foundations are implemented in `library/ui/foundations`, with bundled fonts and typed browser exports. Shared components live in `library/ui/components`, written from the component template above, and are exported from `library/ui/index.ts`. A surface keeps its existing tokens and components until its migration to the shared ones has been reviewed. The decision is recorded in the [shared UI library ADR](adr/2026-10-30-shared-ui-library.md).

`library/ui` is the home of the shared UI foundation:

- design foundations and tokens shared across surfaces;
- React components used by more than one app, or by an app and the Bevy HUD
  (for example Button);
- the Figma MCP integration.

The Figma MCP integration is Node-only tooling and is never imported by browser
code; the shared runtime is foundations and components, with React as a peer
dependency. "Bevy HUD" here means the Explorer's React DOM HUD: its source in
`rendering/bevy-explorer/source/react-web` and its Sites adaptation in
`apps/ui3/src/play` (see [Explorer HUD transition](#explorer-hud-transition)).
Sharing components with it does not extend to Rust Bevy widgets or SDK scene
UI.

Use the public `@dcl/one-ui/foundations` entry for foundation-only consumers. The Explorer HUD imports tokens and icons from this entry and keeps generated components outside its compiled source and build fingerprint. The root `@dcl/one-ui` entry remains the public foundation and component surface. Shared foundation CSS continues to use the existing CSS subpaths.

The Explorer HUD records its computed shared-library import closure in `rendering/bevy-explorer/source/react-web/shared-inputs.json`. The owned root inventory contains every tracked file under `rendering/bevy-explorer/source/react-web/src` plus `app.html` and `index.html`. Any edit, addition, deletion or rename of those roots, including comment-only edits, requires regenerating this manifest before CI planning or strict artifact acceptance. Other owned files, including bridge-scene and web-engine sources, become controls only when the actual compiler or Vite graph reaches them; changes to those recorded controls, shared imports or resolver configuration also require regeneration. Unreached owned files remain covered by the separate Explorer source fingerprint. From the repository root, run `npm --prefix rendering/bevy-explorer/source/react-web run gen:shared-inputs` with locked dependencies and canonical Rust-generated bindings available. Local Vite/Vitest configuration still supports ordinary owned-source edits, and native staleness still invokes the existing rebuild without updating the manifest. `npm run check:shared-inputs` recomputes the compiler, web/native bundle and CSS-processor graphs in CI. The manifest binds all tracked HUD roots and resolver/font identity controls separately from the actually reached UI, Shop and Catalyst API source files. Selection and artifact checks validate each revision's own hashes, reject stale head metadata and conservatively rebuild when a historical revision has no contract. Vite, native stale checks and the pure Nix fileset read the same checked graph. The engine-generated bindings (`library/explorer-core/engine/generated`) are not in it: the engine build produces them and the Sites import compares the tracked copy with its own.

The manifest also witnesses the immediate names and kinds in the finite ancestor directories of actually reached shared files. Adding, removing or renaming a candidate can change import resolution and requires regeneration even when no recorded body changed; editing an unconsumed existing body does not. Git readers compare exact tracked and physical namespaces. Nix checks the original source before filtering; filtered archives allow only an expected namespace subset while requiring every selected file/control and refusing new names. Installed and named private output directories are separate; similarly prefixed regular files remain witnessed. Unsupported historical contracts rebuild conservatively; current heads require a contract produced by the canonical command.

### Structure

```text
library/ui/
  foundations/
    colors/             primitive and semantic colors; gradients
    typography/         font families, sizes and text styles
    layout/             responsive layout values
    spacing/            spacing and corner-radius scales
    icons/              shared icon assets and exports
  components/
    Button/
      Button.tsx
      Button.module.css
      Button.test.tsx
      Button.figma.ts   Figma mapping, read by the tooling only
  apps/
    <app>/              app-specific UI: components and styles used by one app
  index.ts              public foundation and component exports
  figma/                Node-only Figma MCP server and CLI, config, verified cache
```

- **Foundations:** expose shared tokens as CSS custom properties and typed
  values. Each app loads shared tokens through its global entry point.
- **Components:** use React and colocated CSS Modules, export public components
  from `index.ts`, and declare React and React DOM as peer dependencies. Each
  consumer bundles the shared source it imports.
- **App-specific UI:** keep components used by a single app in
  `apps/<app>/`, built from the shared foundations and components; only that
  app imports them. See [App-specific UI](#app-specific-ui).
- **Figma tooling:** keep the MCP server and CLI in `figma/` and outside browser
  exports. Component mappings connect Figma nodes to shared components.

### Foundations

`library/ui/foundations` owns the design values and assets shared by the UI.
Components consume these foundations rather than defining their own palette,
typography, spacing, or icon set. Keep the following foundation families in
this shared library:

- **Color:** primitive colors and semantic roles for backgrounds, text, actions,
  borders and feedback. Semantic roles reference primitives instead of
  duplicating their values.
- **Spacing and shape:** spacing and corner-radius scales used by components
  and layouts.
- **Layout:** responsive margins, section spacing and typography sizes for
  desktop, large desktop, tablet and mobile contexts.
- **Typography:** font families, weights, sizes, line heights, letter spacing
  and text styles. Supply the font assets and their licenses with the library.
- **Gradients:** named gradient styles and their stops and directions.
- **Icons:** shared icon assets and their variants, with their size, color and
  accessible-use contracts.

Figma supplies the design source for these foundations. The Figma tooling
captures and validates the source before producing the shared library's
foundation outputs. Preserve semantic aliases, modes and units in that
process; a missing or unsupported value must be reported for review instead of
silently substituting a value from another mode. Generated output is reviewed
before it becomes a shared foundation.

Review theme and surface mappings before replacing existing values. Preserve
runtime state such as HUD viewport scaling in the consumer.

### Explorer HUD transition

The Explorer HUD has two hosts:

- `rendering/bevy-explorer/source/react-web` is the host the Explorer
  workflows build for the CDN and the native CEF app;
- `apps/ui3/src/play` is the host for Sites, served under `/play`, with its
  own compiler profile (`apps/ui3/tsconfig.play.json`).

What both hosts share lives once, in two layers
([ADR](adr/2026-11-10-shared-explorer-hud-ui.md)), both keeping the hosts'
folder layout:

- `library/ui/apps/hud` (`@hud/*`): the HUD UI. Components, styles, assets
  (`design/`, `styles/tokens.css`, `assets/`, `features/`) and the React state
  around the core, the session (`features/session/useEngineSession.ts`)
  included.
- `library/explorer-core` (`@explorer-core/*`): the Explorer page's core, with
  no React. The page-engine protocol and the tracked bindings generated from
  the Explorer's Rust structs (`engine/`, written by
  `rendering/bevy-explorer/source/scripts/gen-ts-bindings.sh`), the drivers'
  contract, sign-in, shell messaging, analytics, server reads and the host
  contract. The UI is built on it, never the other way round
  (`library/explorer-core/boundary.test.ts`).

Each host keeps only what is its own:

- its entry points (`main.tsx`, `/play`'s `mount.tsx`) and page shell (`shell/`,
  apart from the shared signer `shell/sign.ts`);
- its module `host.ts` and the libraries behind it (`lib/baseDomain.ts`,
  `lib/bootMode.ts`, `lib/entryParams.ts`, `lib/launchGate.ts`,
  `lib/publicUrl.ts`, `lib/systemScene.ts`);
- `App.tsx`, which renders the shared `HudApp` (`library/ui/apps/hud/App.tsx`)
  with the host's drivers and Shop page, and `lib/engineLaunch.ts`, the engine's
  launch for the shared `EngineHost`;
- its global stylesheet (`styles/global.css`, which also gives the shared
  stylesheets the host's public files as CSS variables such as `--hud-logo`);
- what only one host has: react-web's benchmark autopilot
  (`engine/autopilot.ts`, `shell/localGuest.ts`) and Shop query client, and
  `/play`'s in-world Shop and Creator Hub editor embed.

Where the two copies had drifted, the shared copy kept the fix or feature
either host had, for both.

Shared HUD code imports nothing from a host. Where the hosts behave
differently, it imports the behavior from `@hud-host`, which each host maps
to its own module implementing `library/explorer-core/hostContract.ts`
(`react-web/src/host.ts`, `apps/ui3/src/play/host.ts`). The mapping is a
build-time alias, so code one host leaves out stays out of its bundle.

Both layers keep the conventions of `react-web`, under
`rendering/bevy-explorer/source/react-web/AGENTS.md`: `react-web` type-checks
them, tests included, under its compiler profile, and `library/ui`'s lint and
formatting skip `apps/hud`. Their tests live beside them: `react-web`'s Vitest
runs all of them against its host, `apps/ui3`'s `play-shared` project runs them
again against `/play`'s host (`apps/ui3/src/play/host.ts`), and `library/ui`'s `hud` project runs the UI tests that need
no package only the hosts install (all but `apps/hud/test/hosted/`) against a
web host (`apps/hud/test/host.ts`). Tests of a host's own files stay in that host. The Sites host does not load the shared
foundations yet.

### Integration

Each consumer keeps its own independent build and imports the shared code as
source:

```tsx
import { theme } from "@dcl/one-ui/foundations";
import "@dcl/one-ui/foundations.css";
```

`@dcl/one-ui` exposes the shared source and foundation styles ([ADR](adr/2026-11-09-shared-ui-import-name.md)). Before using these imports, each application configures the package name and CSS subpaths in both its TypeScript paths and bundler aliases, allows access to shared sources and deduplicates React and React DOM. Assets must work in every supported target, including native CEF.

Build inputs, dependency resolution, HUD provenance and CI selection must
include the shared sources so that changes invalidate the affected artifacts.

`rendering/bevy-explorer/source/react-web` is set up this way:

- `vite.shared.ts` holds the aliases and the React deduplication, and
  `tsconfig.json` maps the same entries;
- `src/main.tsx` loads `@dcl/one-ui/foundations.css` once;
- `vite.shared.ts` also maps `@hud` to `library/ui/apps/hud`, `@explorer-core`
  to `library/explorer-core` and `@hud-host` to `src/host.ts`; the bridge scene
  (`react-web/bridge-scene/tsconfig.json`) maps `@explorer-core/*` too, for the
  wire protocol;
- `shared-inputs.json` records the actual compiler/Vite shared dependency closure, across `library/ui`, `library/shop`, `library/catalyst-api` and `library/explorer-core`. `tooling/play-artifact.py` and the CI selector validate its identities, while Vite, native and Nix use the same contract; the HUD imports the public foundation entry, so generated components stay outside that closure.
- Its builds don't fetch Git LFS. The foundations' Inter faces therefore use
  `react-web`'s own `@fontsource-variable/inter`, whose bytes match the digests
  `foundations/typography/font-source.json` pins
  (`src/test/sharedUi.test.tsx` checks them).

The Sites host maps `@hud`, `@explorer-core` and `@hud-host` the same way,
with React deduplicated and both folders allowed to its dev servers:
`apps/ui3/tsconfig.json` and `apps/ui3/vitest.config.ts` for the `/play`
type check and tests, `apps/sites/tsconfig.json` and
`apps/sites/vite.config.ts` for the Sites build.
