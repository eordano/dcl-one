// /create: the marketing landing (CreatePage: Hero, Why, CreateCards, LiveScenes, Learn, Faqs,
// FromTheBlog, Connect) on both sides. dcl-one's web /create is a port of that page (AGENTS.md:
// the Creator Hub home stays for the desktop shell and under /create/*).
import type { Page } from "playwright-core";

import { signedIn, type PageSpec } from "../../lib/spec.mts";
import { SITES_UNRECORDED, sitesFrame, sitesFrameRules } from "./frame.mts";

/**
 * The hero's background video comes from Contentful's video CDN, which the recording does not hold: on both sides
 * the <video> fails and shows its poster, a recorded picture. Each side requests one file per viewport (the
 * landscape clip on the desktop, the portrait one on the mobile): the reference picks it as it renders
 * (sites: components/Create/Hero/Hero.tsx), dcl-one mounts the video after hydration (CreateLanding.tsx Hero).
 */
const HERO_VIDEO = { match: /^GET videos\.ctfassets\.net\//, why: "the hero's background video (sites: components/Create/Hero/Hero.tsx; dcl-one: features/create-landing/CreateLanding.tsx) is a moving picture from Contentful's video CDN, not recorded; both sides show its recorded poster" };

/** The reference hero's title: "Create your own <word> in Decentraland" in one element (Create/Hero/Hero.tsx). */
const HERO_TITLE = "xpath=//main//*[starts-with(normalize-space(),'Create your own') and not(.//*[starts-with(normalize-space(),'Create your own')])]";
// dcl-one's landing renders the hero title as the <h1> of <main> (CreateLanding.tsx Hero).
const TITLE = { reference: HERO_TITLE, "dcl-one": "main h1" };

/** A section of the reference's CreatePage, as its innermost <section> holding its title (sites: components/Create/*). */
const section = (title: string) => `xpath=//main//section[not(.//section) and contains(normalize-space(.),'${title}')]`;

/** The hero's changing word that both sides stop on (sites: Create/data.ts heroData.changingWords). */
const FROZEN_WORD = "Worlds";

/**
 * Both sides run the same wall-clock loops: the hero types a word a letter per 100 ms (sites:
 * hooks/useTypingListEffect.ts) and the testimonials carousel steps every 5 s (components/Carousel).
 * Stop both in one state without touching the harness's own timers (the fixed clock drives them):
 * track the timers the page sets from now on, stop the carousels' autoplay through their
 * visibilitychange handler, wait for the hero to finish typing FROZEN_WORD and clear the tracked
 * timers (its next tick among them), then put the testimonials carousel on its first slide and
 * clear the autoplay that click restarts.
 */
async function freezeLoops(page: Page): Promise<void> {
  await page.evaluate(() => {
    const w = window as unknown as { __parityTimers: Set<number> };
    w.__parityTimers = new Set();
    const { setTimeout: st, setInterval: si } = window;
    window.setTimeout = ((...a: Parameters<typeof st>) => { const id = st(...a); w.__parityTimers.add(id); return id; }) as typeof st;
    window.setInterval = ((...a: Parameters<typeof si>) => { const id = si(...a); w.__parityTimers.add(id); return id; }) as typeof si;
    Object.defineProperty(document, "hidden", { configurable: true, get: () => true });
    document.dispatchEvent(new Event("visibilitychange"));
    delete (document as { hidden?: boolean }).hidden;
  });
  await page.waitForFunction(
    ({ title, word }) => {
      const w = window as unknown as { __parityTimers: Set<number>; __parityPrev?: string };
      const text = document.evaluate(`${title}/span`, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue?.textContent ?? "";
      const typed = text === word && w.__parityPrev === word.slice(0, -1);
      w.__parityPrev = text;
      if (!typed) return false;
      for (const id of w.__parityTimers) clearTimeout(id);
      w.__parityTimers.clear();
      return true;
    },
    { title: HERO_TITLE.slice("xpath=".length), word: FROZEN_WORD },
    { timeout: 60_000, polling: 30 },
  );
  await page.waitForTimeout(600);
  const firstDot = page.locator(`${section("What Decentraland creators say")}//button[@aria-label='Go to slide 1']`);
  await firstDot.scrollIntoViewIfNeeded();
  await firstDot.click();
  await page.waitForTimeout(600);
  // The reference reveals each section on its first view (Create/AnimatedSection.tsx: opacity 0 until it intersects;
  // dcl-one paints at once): show the blog, the last to mount (it waits for the CMS), before anything is measured.
  // Then park the mouse, which the click left over the page, where it hovers no card (a BlogCard scales on hover).
  const blog = page.locator(section("Fresh from the blog"));
  await blog.waitFor({ timeout: 30_000 });
  await blog.scrollIntoViewIfNeeded();
  await page.waitForTimeout(300);
  await page.mouse.move(0, 0);
  await page.evaluate(() => {
    const w = window as unknown as { __parityTimers: Set<number> };
    for (const id of w.__parityTimers) clearTimeout(id);
    w.__parityTimers.clear();
    window.scrollTo(0, 0);
  });
}

const create: PageSpec = {
  id: "create",
  title: "Create (the marketing landing)",
  reference: "sites",
  path: "/create",
  ready: TITLE,
  setup: freezeLoops,
  keys: [...sitesFrame("dtb"), { name: "hero.title", selector: TITLE },
    // On a phone both heroes link the creator docs instead (sites: Create/Hero/Hero.tsx isMobile, since /download/creator-hub
    // renders no actions there; dcl-one: CreateLanding.tsx Hero ctaMobile), so the download link is on neither side.
    { name: "hero.cta", selector: "xpath=//main//a[@href='/download/creator-hub' and contains(normalize-space(translate(.,'abcdefghijklmnopqrstuvwxyz','ABCDEFGHIJKLMNOPQRSTUVWXYZ')),'DOWNLOAD CREATOR HUB')][1]", absentOn: "mobile" },
    { name: "why", selector: section("Why creators choose Decentraland") },
    { name: "possibilities", selector: section("Explore the creative possibilities") },
    { name: "live-scenes", selector: section("Explore what creators are building") },
    { name: "live-scenes.all", selector: "xpath=//main//a[@href='/places' and contains(.,'See all live scenes')]" },
    { name: "testimonials", selector: section("What Decentraland creators say") },
    { name: "tutorials", selector: section("Learn from Community Tutorials") },
    { name: "blog", selector: section("Fresh from the blog") },
    { name: "faqs", selector: "xpath=//main//section[not(.//section) and .//h2[contains(normalize-space(.),'Frequently Asked')]]" },
    { name: "faqs.title", selector: "xpath=//main//h2[contains(normalize-space(.),'Frequently Asked')]" },
  ],
  ...sitesFrameRules(),
  unrecorded: [...SITES_UNRECORDED, HERO_VIDEO],
};

export default [create, signedIn(create, "Create, signed in as the test wallet")] satisfies PageSpec[];
