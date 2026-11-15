# ADR: Shared UI library

- Status: Accepted
- Date: 2026-10-30
- Updated: 2026-11-12
- Amended by: [Shared UI import name](2026-11-09-shared-ui-import-name.md) (how each consumer resolves `@dcl/one-ui`, and where Inter is declared)

## Context

The web applications and the Explorer's React HUD need shared UI foundations
and components. Keeping their design values and reusable controls together
supports consistent use across deliverables.

## Decision

Place shared UI foundations, components and their Figma tooling in `library/ui`.
Place UI used by a single app in `library/ui/apps/<app>/`; the app keeps its
pages, routes and data wiring. The shared component styling standard is CSS
Modules. The Figma tooling is separate from browser imports.

Public components are written by hand from the component template described in [`docs/ui-standards.md`](../ui-standards.md): the React component with its native semantics, its CSS Module on foundation tokens, behavior and accessibility tests, and a `Component.figma.ts` mapping that only the Figma tooling reads. Generated Figma views back the Review previews and are not a production rendering model. Hand-written public components are absent from `promotion.include` in `library/ui/figma/figma.config.json`.

### Layout

`library/ui` is used by several deliverables (the web apps and the Explorer
HUD), as the [layout ADR](2026-10-19-organizing-library-services-folders.md)
requires of `library/`. It is a TypeScript source library, not a Rust crate, so
the crate rules there (`dcl-*` name, `[workspace.dependencies]`) do not apply.

`library/ui/apps/<app>/` is an exception to that ADR's rule that `library/`
holds only code several deliverables use: UI used by a
single app lives there, beside the shared foundations and components it is
built from. The exception covers only `library/ui/apps/`; when a second
deliverable needs something from it, it moves to the shared `components/` or
`foundations/`.

### Foundation installation

The shared `figma install` command owns installation of public UI targets. Its foundations target regenerates colors, icons, typography, layout and spacing from the verified capture pinned in `library/ui/upstream.lock.json`. Live reads cannot write public output. Installation stages the complete target, refuses dirty or concurrently changed output and restores the previous directory when publication fails. Maintained CSS entrypoints, declarations and licensed Inter font assets are preserved. The public lock table records file digests and origins, and font validation compares materialized bytes with Git LFS object identities and their pinned digests. A port verification record binds the port inputs ([ADR](2026-11-10-port-verification-binds-port-inputs.md)) to a clean installation that reproduces every tracked foundation file. Browser entrypoints remain independent of installation code.

## Consequences

The Explorer HUD consumes shared tokens and icons through the public foundations entry while retaining its local design primitives. Applications can consume the public foundations and component entries independently. Integrations preserve existing behavior and applicable themes, account for independent builds and asset resolution, and bind artifacts to their actual source inputs.

### Component installation and integrity

The components target uses the same verified-cache source and atomic install transaction as foundations. A pure producer receives source data, configuration, a names-only visibility registry and the locked runtime; it cannot acquire data or publish files. The transaction owns and rechecks the complete input read-set. Public names receive mappings; internal names receive neither mappings nor package exports. Installation records source, configuration, dependency and generator-module identities alongside emitted output digests in `components/generation.json`; the module list is the install process's read-set ([ADR](2026-11-11-generation-binds-installer-import-closure.md)). The lock's public component table pins that record's digest, and the record's module list is validated against each module's digest in the source lock; output origins name source and transform and never repeat the list. The common output inspector verifies the index and materialized bytes for both targets; the component policy derives origin classes from the emitted plan and fixed runtime inputs. Components never admit LFS pointers. A hand-written component is a top-level directory of `components/` that the public component table does not name: it is plain tracked source, the lock never pins it, and the inspector refuses it when it is untracked or not TypeScript or CSS, or holds an LFS pointer, a Figma URL or key-shaped value, or a known generator marker. The component transaction owns files, not directories: it removes or overwrites only `generation.json`, `promotion.json` and an output the lock's public table or the installed generation record pins, while its bytes match that pin. It carries every other file in a component directory over byte for byte and refuses a plan that would write over such a file or into a component directory that holds one, and an unowned file at the top of the target. `lock-digests --write` regenerates both public tables from the staged manifests. The checked record establishes offline provenance consistency, not an authenticated claim of live Figma parity. No generated public component remains: NavBar, the last one, is hand-written in `components/NavBar/`, and the components target now installs an empty registry.

### HUD shared-source contract

Foundation-only consumers use the public `@dcl/one-ui/foundations` entry; the root package continues to export public components. The Explorer HUD computes its shared-library closure with its actual native TypeScript compiler and Vite web/native graphs, including CSS imports and assets through Vite's processor. A deterministic tracked manifest binds owned HUD source roots, resolver and font identity controls, reached shared files and their graph edges. The changed-file selector, artifact fingerprint, Vite access list, native stale check and pure Nix source contract consume the same validated data. Stale or unresolved head inputs are refused; historical revisions without their own manifest cannot reuse an artifact and select the web build conservatively. The manifest excludes installed npm dependency contents and generated engine bindings, whose locked package and engine producer contracts remain separate. Inter font substitution remains controlled by the installed locked package and the shared font metadata. This import contract does not change the Sites HUD adaptation review scope.

The HUD graph refuses CSS Modules composition (`composes` and `compose-with`) because Vite's current PostCSS Modules pipeline does not report those file dependencies. Supporting composition requires upstream dependency reporting; this contract does not attest to arbitrary CSS processor reads. Inline PostCSS plugins remain part of the actual pipeline; automatic or external PostCSS configuration is refused instead of silently replacing it. Filtered source archives without Git validate the checked manifest against regular physical files and the complete HUD root inventory before use.

Local Vite/Vitest configuration uses structural manifest inspection for its file allowlist so owned HUD and configuration edits can be tested before regeneration. Explicit graph checks, CI, artifact acceptance and Nix still validate the full identities. Validated body-only changes to existing owned HUD roots use the successful native bundle stamp and current modification times, so an unchanged second invocation reuses the local bundle. Structural, resolver and shared-input staleness still requires regeneration; malformed, unsafe or unreadable inputs abort. This local cache rule does not weaken strict CI or artifact identities. Configuration loading neither regenerates metadata nor updates review pins. Historical base metadata that cannot be validated selects a conservative web rebuild, while invalid head metadata remains an error.

### Resolution namespace witnesses

The manifest records `resolutionDirectories`: the exact set of ancestor directories of actually resolved shared files, bounded by the UI, Shop and Catalyst API library roots. Each witness records only sorted immediate names and regular-file or directory kinds. It never hashes unconsumed library bodies. The generator snapshots and rechecks these namespaces around actual compiler and Vite resolution, so a sibling file that would shadow a directory import cannot bypass source selection or artifact acceptance. Tracked additions require regeneration; unexpected physical candidates, symlinks and special entries are refused. Installed `node_modules`, Git metadata and named private output directories retain their existing separate contracts. Output-directory prefixes do not exclude regular source files with those prefixes.

Git-backed readers verify exact indexed and physical namespaces. Nix verifies the full original flake source before filtering. A filtered archive may omit unconsumed witnessed names but must contain every selected file and control with its verified identity; any new unlisted entry is refused. This subset rule trusts the checked original-source namespace contract and does not adopt a new resolver input. Ordering uses UTF-8 byte order across readers. Unsupported historical contracts select a conservative web rebuild. Current heads require a contract produced by the canonical generator before strict artifact acceptance.

The JS, Python and pure Nix validators share a malformed-manifest corpus. Metadata, domain, namespace and hash checks are compared; Python remains mandatory for binary payload lengths and artifact acceptance. Pure Nix never reads binary payloads as text. The image workflow evaluates the pure Nix corpus after installing Nix and before heavy builds. A local environment without Nix cannot claim that evaluation passed; the corresponding CI step remains a required gate.
