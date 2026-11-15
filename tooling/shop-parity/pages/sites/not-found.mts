// A dead link: the fullscreen 404 an unknown path lands on (sites: pages/not-found/NotFoundPage.tsx, the `*` route,
// outside Layout: no nav, no footer, the logo linking home, the CTA to the events page) and dcl-one's root error
// boundary (apps/sites routes/root.tsx, features/not-found/NotFoundPage). The reference's static server answers the
// path with the SPA shell (200); dcl-one answers 404 with the page in it. The status is not compared, the page is.
import type { Page } from "playwright-core";

import { both, signedIn, type KeyElement, type PageSpec } from "../../lib/spec.mts";
import { hideCanvas, interOnReference, mainText, UP } from "./frame.mts";

const HOME = "xpath=//main//a[@aria-label='Decentraland Home']";
const TITLE = both("xpath=//main//h1");

const keys: KeyElement[] = [
  { name: "page", selector: "main" },
  { name: "home", selector: HOME },
  // One svg on both sides: MUI's SvgIcon takes the Logo's own <svg> as its root (sites: decentraland-ui2 Logo).
  // An SVG element is in its own namespace, which a bare `svg` step does not match in an HTML document.
  { name: "home.logo", selector: `${HOME}//*[local-name()='svg']` },
  { name: "watermark", selector: mainText("404") },
  { name: "title", selector: TITLE },
  { name: "title.rest", selector: "xpath=//main//h1/span" },
  { name: "description", selector: "xpath=//main//h1/following-sibling::p[1]" },
  { name: "cta", selector: `xpath=//main//a[${UP}='SEE EVENTS']` },
  { name: "avatar", selector: "xpath=//main//img" },
];

/**
 * The AnimatedBackground behind the page is a WebGL <canvas> animated over a static picture (sites: decentraland-ui2
 * AnimatedBackground variant "absolute"; dcl-one: shop/discovery/components/animated-background), as on the report
 * page: both sides hide the canvas, so the static picture and the page over it are compared.
 */
async function holdBackground(page: Page): Promise<void> {
  await hideCanvas(page);
}

const notFound: PageSpec = {
  id: "page-not-found",
  title: "Not found (a dead link)",
  reference: "sites",
  path: "/this-page-does-not-exist",
  ready: TITLE,
  init: interOnReference,
  setup: holdBackground,
  keys,
};

export default [notFound, signedIn(notFound, "Not found (a dead link), signed in as the test wallet")] satisfies PageSpec[];
