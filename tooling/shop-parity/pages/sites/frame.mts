// The frame decentraland.org's pages share, as each side renders it. The reference's Layout wraps
// every route in the LandingNavbar (a fixed <nav> whose logo links home), one <main> and the
// LandingFooter (sites: components/Layout/Layout.tsx); dcl-one's web pages render the DclTopBar (the Sites
// host's header[aria-label='Decentraland'] around the shared NavBar) inside SitesChrome, and /places the
// ExploreChrome (header.xc__nav) instead.
import type { BrowserContext, Page } from "playwright-core";

import { installInter } from "../../lib/fonts.mts";
import type { Expectation, KeyElement, PageSpec, PerSide, Region, Side } from "../../lib/spec.mts";

/** An element's text in capitals: a match by text, whatever the case of the markup. */
export const UP = "translate(normalize-space(),'abcdefghijklmnopqrstuvwxyz','ABCDEFGHIJKLMNOPQRSTUVWXYZ')";
/** A link or button of <main> by its text in capitals. */
export const control = (text: string) => `xpath=//main//*[self::a or self::button][${UP}='${text}']`;

/** Hides the WebGL canvas both sides animate behind <main>'s content, so the static picture under it is compared. */
export async function hideCanvas(page: Page): Promise<void> {
  await page.addStyleTag({ content: "main canvas { visibility: hidden !important; }" });
}

/** The reference's site nav: the LandingNavbar, the <nav> holding the "Decentraland Home" logo link. */
export const REFERENCE_NAV = "xpath=//nav[.//a[@aria-label='Decentraland Home']]";

/** Which chrome dcl-one renders the page in: the site's top bar or the explorer's. */
export type DclOneChrome = "dtb" | "xc";

const DCL_ONE_NAV: Record<DclOneChrome, string> = { dtb: "header[aria-label='Decentraland']", xc: "header.xc__nav" };
const DCL_ONE_SIGNIN: Record<DclOneChrome, string> = {
  dtb: `xpath=//header[@aria-label='Decentraland']//*[self::a or self::button][${UP}='SIGN IN']`,
  xc: "header.xc__nav .xc__user",
};

export function sitesFrame(chrome: DclOneChrome): KeyElement[] {
  return [
    { name: "site-nav", selector: { reference: REFERENCE_NAV, "dcl-one": DCL_ONE_NAV[chrome] } },
    { name: "site-nav.signin", selector: { reference: `${REFERENCE_NAV}//*[self::a or self::button][${UP}='SIGN IN']`, "dcl-one": DCL_ONE_SIGNIN[chrome] } },
    { name: "footer", selector: "footer" },
  ];
}

/** The innermost element inside <main> whose visible words are `text` (case-insensitively), on both sides. */
export function mainText(text: string): PerSide<string> {
  const t = text.toUpperCase().replace(/'/g, "\\'");
  const s = `xpath=//main//*[${UP}='${t}' and not(.//*[${UP}='${t}'])]`;
  return { reference: s, "dcl-one": s };
}

/** The reference's LandingFooter embeds the beehiiv newsletter iframe, a third-party page the harness does not record. */
export const SITES_UNRECORDED: NonNullable<PageSpec["unrecorded"]> = [
  { match: /^GET embeds\.beehiiv\.com\//, why: "the reference's footer newsletter is a third-party beehiiv iframe (sites: components/LandingFooter/LandingFooter.tsx); its document is not recorded" },
];

export const NAV_SECTIONS = "the site nav links dcl-one's own sections: the landing page, What's On, /play, /shop, /create and /docs are one site (AGENTS.md, Scope)";
export const NEWSLETTER = "the third-party beehiiv newsletter iframe is replaced by a first-party form of the same box (posting to /v1/newsletter)";

/** The font rule of every sites page (lib/fonts.mts): the reference's context gets dcl-one's Inter faces before it navigates. */
export async function interOnReference(context: BrowserContext, side: Side): Promise<void> {
  if (side === "reference") await installInter(context);
}

/**
 * The deliberate frame differences every SitesChrome page shares: the nav's section links, the newsletter
 * form, and the Inter faces (lib/fonts.mts, installed by `init`). The footer flag is compared:
 * the harness serves the reference's twemoji picture from the recording (lib/services.mts TWEMOJI). The regions cover only those boxes: on the reference
 * each DesktopDropdownWrapper of the DesktopTabList (sites: LandingNavbar.tsx) and the footer's beehiiv
 * iframe; on dcl-one each section tab of the NavBar's list and the .st__newsform, which takes the iframe's box. The mobile navs
 * and the signed-out desktop landing nav show no section links, so the nav region matches nothing there.
 */
/** The newsletter form's box on each side, and the differences it explains. */
export const NEWSLETTER_REGION: Region = { selector: { reference: ["footer iframe"], "dcl-one": [".st__news .st__newsform"] }, why: NEWSLETTER };
export const NEWSLETTER_EXPECTED: Expectation[] = [
  { match: /newsletter|^"Enter your email"$|^"SUBSCRIBE"$/i, why: NEWSLETTER },
  { match: /^key:footer$/, why: NEWSLETTER, only: ["text"] },
];

export function sitesFrameRules(): { regions: Region[]; expected: Expectation[]; init: NonNullable<PageSpec["init"]> } {
  return {
    init: interOnReference,
    regions: [
      { selector: { reference: [`${REFERENCE_NAV}//*[normalize-space(text())='Discover']/ancestor::div[count(*)>=4][1]/*`], "dcl-one": ["header[aria-label='Decentraland'] nav > div > ul > li > :is(a, button)"] }, why: NAV_SECTIONS },
      NEWSLETTER_REGION,
    ],
    expected: [
      { match: /^"Discover"$/, why: NAV_SECTIONS, status: "only-reference" },
      { match: /^"(Play|Social)"$/, why: NAV_SECTIONS, status: "only-dcl-one" },
      { match: /^"(Create|Learn)"$/, why: NAV_SECTIONS, only: ["left", "width"] },
      { match: /^"Shop"$/, why: NAV_SECTIONS, only: ["left", "width"] },
      { match: /^key:site-nav$/, why: NAV_SECTIONS, only: ["text"] },
      ...NEWSLETTER_EXPECTED,
    ],
  };
}

/**
 * The LIVE badge's icon pulses on both sides (decentraland-ui2 Badges livePulse; dcl-one web/places
 * PlaceBadges.module.css, Landing.module.css) on a loop that starts when each side mounts its card, so its
 * scale depends on time: both sides hold it at its first frame.
 */
export async function holdLivePulse(page: Page): Promise<void> {
  await page.addStyleTag({ content: "main [data-testid=SensorsRoundedIcon] { animation: none !important; }" });
}
