# ADR: The Creator Hub desktop workspace lives in `apps/creator-hub`

- Status: Accepted
- Date: 2026-11-09

## Context

The Creator Hub desktop app, `@dcl/inspector` and `@dcl/asset-packs` lived in
`decentraland/creator-hub`, an npm-workspaces monorepo that publishes both packages to npm and
the app's installers to its own GitHub releases. The inspector reaches scenes through
`@dcl/sdk-commands` (now `library/js-sdk-toolchain`) and the asset packs through Sites, so
every change that crossed those lines waited for a publish. `apps/creator-hub` held a Tauri shell
that loaded the web Creator Hub and that nothing shipped.

## Decision

`apps/creator-hub` is a first-party snapshot of `decentraland/creator-hub` `main` (`7a68e6a`,
2026-11-11) and replaces the Tauri shell. It keeps its npm layout (one root `package-lock.json`,
`packages/asset-packs`, `packages/inspector`, `packages/creator-hub`, `file:` links between
them), its `Makefile` and its agent files, and is run from that folder: like
`library/js-sdk-toolchain`, it is its own npm project and dcl-one has no JavaScript workspace at
its root. Splitting the three packages across `apps/` and `library/` waits until a second
deliverable builds one of them from source.

Three changes were made in the snapshot:

- The repository-wide pre-commit hook (`simple-git-hooks` with `nano-staged`) is removed. Its
  install walks up to the nearest `.git`, which here is dcl-one's, and it ran ESLint and Prettier
  over the whole tree.
- The `devtools-frontend` submodule is replaced by `scripts/fetch-devtools.js`, which fetches the
  public repository at a pinned commit (git checks the objects against it) without its 130 MB of
  trace fixtures.
- Machine-local paths, PR working notes and an agent-state folder hidden only by a local
  `.git/info/exclude` are removed.

What else changes:

- **Media.** The asset packs' media (`packages/asset-packs/packs/**`: about 10,000 PNG, GLB,
  JPEG, WebP and MP3 files, 770 MB) is Git LFS (`apps/creator-hub/.gitattributes`). Only the jobs
  that build, validate or upload the packs fetch it, through
  `.github/actions/creator-hub-lfs` and a cache keyed on the LFS object ids. The app's own images
  and videos stay plain git.
- **CI.** The workflows are `.github/workflows/creator-hub-*.yml` (entry point
  `creator-hub-ci.yml`, triggered by `apps/creator-hub/**` and its own workflow and action files)
  and the composite actions `.github/actions/creator-hub-*`. Shell steps run in
  `apps/creator-hub`; paths given to actions carry the prefix. Every action is pinned to a commit.
- **npm releases.** `@dcl/asset-packs` and `@dcl/inspector` publish from here with npm trusted
  publishing (workflow `creator-hub-ci.yml`, environment `creator-hub-release`) and keep their
  tags, `@dcl/asset-packs@X.Y.Z` and `@dcl/inspector@X.Y.Z`. The tag history was not imported:
  the last released version of each (and `creator-hub@X.Y.Z` for the installers) is tagged on
  `main` before the import merges, since the merge itself starts the first release. A release on
  `main` fails when it finds no such tag, instead of publishing `0.1.0` as npm `latest`.
- **Installers.** They keep publishing to `decentraland/creator-hub` releases, which becomes a
  release-only repository: every installed app's updater reads them (`electron-builder.cjs`
  `publish`, `shared/urls.ts`), and GitHub has one "Latest" release per repository, which the
  updater follows. A GitHub App installed on that repository gives `creator-hub-desktop.yml` and
  `creator-hub-drop.yml` their token. The desktop workflow tags `creator-hub@X.Y.Z` here, from
  which `semantic-version` (`change_path: apps/creator-hub`) computes the next version. That
  repository is not archived: an archived repository takes no new releases.
- **Promotion.** A merge publishes a pre-release, which no installed app sees, only when its pull
  request has the `release:creator-hub` label; a manual run of `creator-hub-ci.yml` on `main` with
  `release` set publishes one too. Every other run on `main` builds the unsigned test installers
  and publishes nothing. A person makes a pre-release Latest with `creator-hub-promote.yml` (`promote`), and `rollback` turns the current Latest
  back into a pre-release. Both run in the `creator-hub-promote` environment, which requires a
  reviewer. Promotion needs the `creator-hub@X.Y.Z` tag here, which the desktop workflow creates
  only after both installers built and passed the signature check: the installers upload before
  those checks end, so a pre-release alone may be half-built. A newer merge never cancels a
  release in progress on `main`, but GitHub keeps only one *queued* run per concurrency group: when
  several merges land during a release, only the newest of them runs, and it releases only if its
  own pull request has the label; the manual run covers a labelled merge that lost its place. Older
  pre-releases are pruned only after a new one is fully published, so the last finished one, and
  any rolled-back release, stays promotable.
- **Windows update signatures.** Installed Windows apps check each update's signature against
  `publisherName` (`verifyUpdateCodeSignature: true`). The release job fails if the signed
  installer's certificate carries a different name, so it never gets the tag promotion needs.
- **Secrets.** No AWS keys are stored: every upload assumes an IAM role through GitHub OIDC
  (`aws-actions/configure-aws-credentials` with `role-to-assume`, `id-token: write`). The role
  ARNs are repository variables: `CREATOR_HUB_SDK_BUCKET_ROLE_ARN` (PR packages, installers and
  inspector previews), `CREATOR_HUB_ASSETS_DEV_ROLE_ARN` (the `/upload-assets` comment) and
  `CREATOR_HUB_ASSETS_PRD_ROLE_ARN` (the production asset CDN, trusted only from the
  `creator-hub-release` environment). Jobs that run on pull requests read repository secrets: the
  bucket names, the end-to-end wallet and the app's build-time keys. Signing, notarization, the
  Sentry upload token and the release App key live in the `creator-hub-release` environment,
  limited to `main`. Jobs pick it with
  `github.ref == 'refs/heads/main' && 'creator-hub-release' || ''`.
- **Inspector previews.** These were GitHub Pages; they are now uploaded to the SDK bucket next to
  the branch's package tarballs.
- **UI standards.** The Electron renderer is built on `decentraland-ui2`/MUI with `styled`, and
  the inspector keeps one plain stylesheet per component, both against
  [the UI standards](../ui-standards.md). `tooling/ui-standards-imported.txt` lists the 238 files
  that broke the standard when they were imported. `tooling/ui_standards.py` passes them only as
  files the change adds, so every later change to them is checked as usual. The check reads the
  list from the base revision and fails a change that adds to it or deletes it, so the list never
  grows.
- **Ownership.** `apps/creator-hub` and the creator-hub workflows and actions belong to the
  `creator-ui` area.

## Consequences

- An asset-pack, inspector, SDK and app change can land in one pull request; until the SDK
  builds the inspector from `apps/creator-hub`, `@dcl/sdk-commands` still pins a published
  `@dcl/inspector`.
- A clone that runs `git lfs pull` downloads the packs' media; `lfs.fetchexclude
  'apps/creator-hub/packages/asset-packs/packs/**'` skips it for anyone who does not build them.
- History before the import stays in `decentraland/creator-hub`.

## Follow-ups

1. Move the renderer to `apps/ui3`, consumed as source like Sites does (`@ui/*`): React 19 first,
   then ui3's tokens, a translation seam in ui3 (the app ships English, Spanish and Chinese), a
   navigation seam, then one screen at a time. Remove `decentraland-ui2` and MUI at the end, and
   with them the renderer's lines in `tooling/ui-standards-imported.txt`.
2. Build `@dcl/sdk-commands` against `apps/creator-hub/packages/inspector` instead of a
   published version.
3. Delete what only the Tauri shell used in Sites: the native sign-in relay
   (`routes/auth.native.tsx`, `data/lib/auth/native-shell.ts`) and the IPC probe in
   `routes/create._index.tsx`.
4. Review `apps/creator-hub/docs/solutions` and `.plan-plus.json`: keep what describes current
   behaviour, drop the rest.
