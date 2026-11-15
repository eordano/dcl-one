# Shop parity

Compares `apps/ui3/src/shop` with the Shop UI at the `decentraland/shop` revision pinned in
[`reference.json`](reference.json), and `apps/sites`' landing, What's On, Create and Places pages
with decentraland.org (the `decentraland/sites` SPA) at the revision pinned in
[`reference-sites.json`](reference-sites.json), page by page, on the same recorded data. A pin is the
revision production serves on the date it names; moving it means recording a new set against it.

The tool is committed; the data it compares is not. Recordings, the gate and every run stay under
`state/shop-parity/` (gitignored) on the machine that made them, so numbers quoted elsewhere name the
recording they were measured on.

```sh
pnpm install                           # playwright-core; then `pnpm exec playwright-core install chromium`
node reference.mts                     # check out and build the pinned Shop UI under state/
node reference.mts --reference sites   # the same for the pinned sites SPA (capture builds what its pages need)
node record.mts --set <date>           # record the answers both sides read (see Recording)
node capture.mts --build               # build apps/sites, compare every page and viewport, print the summary
node gate.mts                          # fail if a page marked identical is not
node gate.mts --promote                # mark the pages that are identical now
node inspect.mts item desktop dcl-one 'document.title'   # evaluate in one side's page
node nav-walk.mts --set <date>         # open every link each gated sites page shows, on both sides (see Navigation walk)
pnpm test && pnpm typecheck
```

Node 24+, with `apps/ui3` and `apps/sites` installed (`pnpm install` in each). `--build` builds
`apps/sites` with an allowlisted environment, so none of the developer's `VITE_*` or deploy variables
is baked into the bundle. Vite also reads `apps/sites`' `.env*` files from disk, so `--build`
refuses to run while one that is not committed exists, gitignored or not (`.env.production.local` is
neither), and names it. It stamps the build with what it was built
from: the commit, a hash of every uncommitted change that can reach the build (edits and untracked
files anywhere in the repository but this tool), and the Node version. Without `--build`, a run
refuses a build whose stamp is not the current sources. `gate.mts --promote` runs only on a fully
committed tree, this tool included. `--pages overview,items` and `--viewports desktop` narrow a run
(a capture, or the pages a promotion reads).
`CHROMIUM_BIN` picks the browser; both sides always use the same one. `pnpm test` needs no install,
and the repository's structure check runs it.

The loopback servers listen on 5176–5179 (`SHOP_PARITY_PORT_BASE` moves them): the shop reference,
the replay, the sites preview and the sites reference. The replay answers the sites preview only, server to server, and refuses any request a browser makes; the
preview refuses any request another site makes. Both also refuse a `Host` other than their own
loopback address, which is what a page on a name rebound to 127.0.0.1 would send. So a website open
elsewhere cannot make a recording read production. A page opened on the preview or the reference in
another browser calls the real services and is not what is compared.

## Two references

`reference-sites.json` pins `decentraland/sites` (private: `SHOP_PARITY_REFERENCE_REPO_SITES` names a
local mirror holding the revision, as `SHOP_PARITY_REFERENCE_REPO` does for the shop), built from the
repository root (`app: "."`) with `VITE_REACT_APP_DCL_DEFAULT_ENV=prod`, so the bundle names the
production services the recording answers. Each reference is checked out and built under
`state/shop-parity/reference/<revision>/` and served by its own static server; a page's `reference`
("shop", the default, or "sites") picks the one it is compared with, and its `path` may differ per
side (`/events` on the reference, `/whats-on` on dcl-one). The manifest and the report name both
revisions.

The sites build is `vite build` alone. The repository's build script also runs the type check
(`tsc -b`) and four post-build scripts, none of which changes what is compared. Three write files
the production edge reads and the app never requests: `scripts/prerender-legal.mjs` (`dist/legal/`,
the legal pages as static HTML for a client without JavaScript), `scripts/build-route-manifest.mjs`
(`dist/routes.json`, the paths the edge answers 404 for; the reference server here answers every
path that is not a file with the app, which renders its own not-found page) and
`scripts/build-llms-txt.mjs` (`dist/llms.txt`). The fourth is `scripts/prerender-hero.mjs`,
which puts a static hero shell, a font preload and the English dictionary (`window.__dclEn`) before
`#root` in `dist/index.html`; the app removes the shell on hydration (the landing's Hero effect, an
inline script on every other path) and loads the dictionary itself when it is not inlined
(`src/intl/LocaleContext.tsx`), and a capture measures after both, so the step changes nothing that is
compared.

decentraland.org's stylesheet names `Inter, Helvetica, Arial, sans-serif` but declares no Inter face, so
the reference renders the fallback; dcl-one declares Inter v20 on every page (`apps/ui3/src/atoms/inter.css`)
and keeps it. Every sites page is therefore measured under one frame rule (`pages/sites/frame.mts` INTER,
installed by its `init`): the reference's context gets inter.css's `@font-face` rules before it navigates,
each url pointing at the same woff2 files, which the sites reference's static server serves from
`apps/ui3/public/fonts` under `/__parity/fonts/` (`lib/fonts.mts`), and both sides await
`document.fonts.ready` before measuring. The shop reference loads its own fonts and is untouched.

The sites pages live one per file under `pages/sites/<id>.mts`, each exporting its signed-out and signed-in specs,
aggregated into `PAGES` after the shop pages; `pages/sites/frame.mts` holds the shared nav and footer
keys (the reference's LandingNavbar and LandingFooter against dcl-one's `header.dtb` or the explorer's
`header.xc__nav`). While recording, the places page discovers the first Live Now scene that is not
Genesis Plaza; place-live compares its page. The node's `/api/places`, `/api/destinations`, `/api/map`
are the places service and `/api/events`, `/api/schedules` the events service, so one recorded answer
feeds both sides; the reference's own-origin `/api/*` is the `sites` service (decentraland.org).
`report` compares the player report form at `/report`; `report-success` opens that form and makes the
client navigation a submission makes (`{ submitted: true }` in the history entry), since the success
card shows to no other visit. The invite page's clip is the `cms-videos` service (Contentful's video
CDN as decentraland.org reads it), so the recording holds it; dcl-one serves the same bytes as its own
asset. `page-not-found` opens `/this-page-does-not-exist` on both sides, the fullscreen page a dead link lands on
(no nav, no footer): the reference's static server answers it with the SPA shell (200) and dcl-one with a 404, which
is listed with the capture and not compared. The footer's language flag is the `twemoji` service (jsdelivr's twemoji SVGs only), a fixed
interface asset, so the recording keeps its bytes, unlike a content picture. A key both sides leave out
on one viewport by design names it in `absentOn`; every other declared key found on neither side is
reported, and no expectation covers it.

## What a run does

- **Same data on both sides.** `state/shop-parity/recordings/<set>/` holds the API answers both UIs read, keyed by
  service, path, canonical query and request body. The reference shop is served from its build and
  every request it makes off its origin is answered from the recording; dcl-one runs as the built
  `apps/sites` tree (`sites-preview.mts`) whose node is a replay of the same recording, so SSR and
  the browser read it too. `/shop-api` paths map to upstream routes through
  `library/shop/api/endpoints.ts`. Content pictures are deterministic placeholders of the
  recorded shape (at most 1024 px a side), telemetry is answered empty, WebSockets are closed, and
  nothing reaches the network. The sites preview may connect to loopback only: a server-side module
  that names a production host of its own is refused, and the host is listed with the page as one
  the harness does not know. Clock, time zone, locale, `Math.random` and scale are fixed per context,
  the clock and `Math.random` of the server-side render too (one render at a time, each from the
  seed), and transitions are turned off before anything is measured. The reference build, the sites
  build and the sites preview get an allowlisted environment, so a developer's own variables reach
  none of them.
- **Structure.** Key elements per page (`pages.mts`), every `data-testid` element the two sides share
  and every visible text run are paired and compared: box (±1px, measured inside the nearest paired
  ancestor so a shift is reported where it starts), font family, size, weight, line height, letter
  spacing, colour, background, radius and text. One line each:
  `trending.card.price#0: font-weight 600→700, text "20"→"20 Credits"`.
- **Pixels.** Each page is taken as a user scrolls it (viewport tiles stitched at their scroll
  offset, fixed bars painted once, sticky elements at their place in the layout) and diffed perceptually (YIQ threshold 0.1, anti-aliasing
  tolerated). Masks cover only what cannot match between two renders (3D previews), inside a box both
  sides have at the same place: a pixel only one side masks counts as differing unless the other
  side's box comes within 1px of it (the structural tolerance), so a masked box that moves or grows
  is caught. Regions cover a declared deliberate difference, whose extent differs
  between the sides, so a region hides its area on both; a structural difference inside a region
  takes its rule only when it lies inside that region on every side it appears. Both are declared
  per page with their reason.
- **Report.** `state/shop-parity/runs/<time>/index.html`: per page and viewport the reference,
  dcl-one and diff images, the bands where pixels differ, the unexpected structural lines, the
  expected ones with their rule, console errors and any request the recording could not answer.

A page×viewport is identical when no pixel differs outside its masks and regions, every
structural difference is a declared expectation, and every request the recording could not answer
or that went to a host the harness does not know is declared by the page (`unrecorded`).
`state/shop-parity/gate.json` lists the identical ones and the recording they were found on; the gate
fails when one of them stops being identical, and drops a page×viewport whose page the tool no longer
declares. Its `ceilings` hold, per differing page×viewport, the
differing pixels and unexpected structural differences of the last promotion (`gate.mts --promote`
only lowers them); the gate fails when a capture rises above one. Before measuring, a capture decodes
every image and stops every animation on both sides (infinite ones at their start, finite ones at
their end, as the screenshot does), so a count depends on neither a decode nor an animation frame.
Every scaled image is re-attached from a blob of its own bytes first, so both sides draw it from one
decode at its final size and not from whichever mip Chromium's decode cache held.
An expectation records a difference, it does not approve one: each names the rule or code behind it.

## Signed-in pages

A page with `signedIn` runs signed in as a test wallet on both sides: the shared
`single-sign-on-<address>` session both apps read, and, for the reference's decentraland-connect, its
stored injected connection and a `window.ethereum` that answers with the account and forwards every
other call over the network (where the recording answers it). The wallet's key comes from
`SHOP_PARITY_WALLET_KEY` (a test wallet's 0x-prefixed key, never committed). Without it a run leaves
the signed-in pages out, says so once, and compares every other page. The test wallet declines any
signature or transaction a page asks for (EIP-1193 `4001`), and answers only read calls. The ephemeral key is derived from it, so the same wallet and clock give the same
session and the same captures.

Pages sign their requests at the fixed clock, which production refuses a minute later. While
recording, a read (GET or HEAD) a page sent signed is signed again as the same wallet, at the real
time and with the page's own metadata, before it goes out; every other request still goes out
unsigned. A live read follows a redirect only to a host `lib/services.mts` lists, signs each hop
again, and signs only for a `decentraland.org` host; a redirect to any other host is listed and
not followed. A write a page signed is never sent: it would change the test wallet's state, so it is
listed and missed. A fill without the key fails a miss the page signed rather than sending it
unsigned. A replay answers by method, path, query and body, never by headers, so the signature is
not part of what is compared, and a URL both a signed-in and a signed-out page read keeps one
answer: personal fields in it (a `pickedByUser`) read the same on both sides, as everything does.

The session's tests sign with viem, so they live in `test/` and run with `pnpm test` after
`pnpm install`; the structure check runs `lib/*.test.mts`, which needs nothing installed.

## Navigation walk

`nav-walk.mts` checks that every link a sites page shows resolves on dcl-one as it does on the reference,
over one recording. It opens each sites page on both sides (signed out, and signed in where the spec
says), collects the same-origin links each side shows, maps each reference link to its dcl-one path (the
specs' reference→path pairs, `/events/event?id=<id>` → `/whats-on/<id>`, `/places/place/<x,y>` and
`/places/world/<name>` as they are, `https://docs.decentraland.org/<p>` → `/docs/<p>`,
`https://decentraland.org/<p>` → `/<p>`, any other path as it is) and opens each pair as a full document
on both sides. A pair is equal when both sides reach the same outcome (resolved, not found, error), the
reference's final path maps to dcl-one's (or dcl-one's own redirect to `/places/<id>` reaches it), and the
first `h1` reads the same where both have one; titles are listed, not compared. A docs link opens on
dcl-one only (docs is not a pinned reference) and must resolve there. A link into a production app the
sites SPA never served (`/marketplace`, `/builder`, `/auth`) opens on dcl-one only too. A production path
dcl-one does not host (`/dao`, `/discord`) counts as external; where the walker names the public
target dcl-one redirects such a path to, it asks dcl-one's own server for the path without following the
redirect and reports the answer. Links to another Decentraland host are listed as unmapped, and links
dcl-one shows that no reference link maps to are listed per page. The report (`report.md`, `visits.json`,
under `state/shop-parity/nav/<time>/` or `--out`) lists the mapping, the unmapped links, the outside paths,
every page and every pair; the exit code is 1 when a pair differs, an outside path answers something
else, `--max-links` left a link unopened or `--fill` left a document with an unanswered request. `--fill` reads each
request the replay could not answer once from production, as `record.mts --fill` does, and opens the
documents that missed again until nothing misses, so the detail pages the links open get their answers
recorded; run it on a copy of the set when the gate's recording must not change.

## Recording

```sh
node record.mts --set <date>           # read the answers once from production
node record.mts --set <date> --fill    # replay, read what the replay could not answer, once
```

`--fill` sends each missed request as the page sent it, body included, and saves what it read even
when one read fails; the failures are listed and the exit code is 1. A server error (5xx) is asked
again twice, after 1 s and 3 s (longer if `Retry-After` says so, up to 10 s), and never kept: it
stays a miss for the next `--fill`, recording lists it at the end and exits 1, and an older
recording's kept 5xx is asked again. A recording that dies midway, Ctrl+C included, still saves what
it read, marked incomplete; `capture.mts` refuses an incomplete recording, and running the same
`record.mts` command again goes on from what was read. To learn a picture's size, recording reads it
once, from Decentraland's own hosts only.

Recording drives both UIs against the production services (public reads only) and keeps their
answers in `state/shop-parity/recordings/<set>/`. They are real public data (profiles, listings,
wallets), which is why they are never committed. An answer recorded as an error is listed when the
recording ends: it would replay as the same error on both sides and hide the page it feeds. dcl-one's
`POST /shop-api/v1/purchase-offers` has no upstream; while recording it is answered from the recorded
listing the way `services/catalyst/shop/src/mana/quote.rs` answers a signed-out primary trade.
