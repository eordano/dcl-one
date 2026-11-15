// decentraland.org's landing page at /: the Hero (its <h1>) and the sections below it (Jump Into What's
// Happening, Catch the Vibe, Your Weekly Rituals, Come Hang Out), on both sides, paired by heading text and
// structure inside <main> (sites: pages/index.tsx, components/Home/*, whose lower sections mount once the page
// is scrolled; dcl-one: apps/sites/src/routes/routes/_index.tsx with library/ui/apps/sites/home/*, which render
// both layouts and show one).
import type { Page } from "playwright-core";

import { signedIn, type KeyElement, type PageSpec } from "../../lib/spec.mts";
import { hideCanvas, holdLivePulse, SITES_UNRECORDED, sitesFrame, sitesFrameRules } from "./frame.mts";

/** The reference's section headings: Typography h3 (Come Hang Out is an h2), matched by their words. */
const heading = (text: string) => `xpath=//main//*[self::h2 or self::h3][normalize-space()="${text}"]`;
/** The reference element right after a heading's own box: its section's content (cards live in that sibling). */
const after = (text: string, inner: string) => `${heading(text)}/following-sibling::div[1]${inner}`;
const HERO_H1 = "main h1";

const keys: KeyElement[] = [
  ...sitesFrame("dtb"),
  { name: "hero.title", selector: HERO_H1 },
  // dcl-one renders both layouts: the harness keeps the visible matches, so the first "Download for <OS>" link
  // (download/DownloadActions.tsx) or App Store badge is the Hero's, and the last of each Come Hang Out's.
  { name: "hero.cta", selector: { reference: "xpath=(//main//a[contains(@href,'landing-hero') or contains(@href,'apps.apple.com')])[1]", "dcl-one": "xpath=//main//a[starts-with(normalize-space(),'Download for')] || xpath=//main//a[@aria-label='Download on the App Store']" } },
  // The Epic button is the desktop Hero's only (sites Hero.tsx isDesktop; dcl-one .heroDesktop, hidden under 991px).
  { name: "hero.cta.epic", selector: { reference: "xpath=//main//a[contains(@href,'store.epicgames.com')][1]", "dcl-one": 'main a[href*="store.epicgames.com"]' }, absentOn: "mobile" },
  { name: "whatson.title", selector: heading("Jump Into What's Happening") },
  { name: "whatson.card", selector: { reference: "xpath=//main//button[.//a[contains(@href,'/profile/accounts/')]]", "dcl-one": "main [data-card]" }, max: 3 },
  { name: "whatson.card.title", selector: { reference: "xpath=//main//button[.//a[contains(@href,'/profile/accounts/')]]/div[3]/div/div[1]/div", "dcl-one": 'main [data-card] [class*="cardTitle"]' }, max: 3 },
  { name: "whatson.all", selector: { reference: "xpath=//main//a[@href='/events']", "dcl-one": "xpath=//main//a[@href='/whats-on']" } },
  { name: "vibe.title", selector: heading("Catch the Vibe") },
  // The visible clip cards (the row on desktop, the carousel's slides on mobile): each poster's card.
  { name: "vibe.card", selector: { reference: `${heading("Catch the Vibe")}/following-sibling::div//img[contains(@class,'catch-vibe-image')]/../..`, "dcl-one": `${heading("Catch the Vibe")}/following-sibling::*//img[contains(@class,'catch-vibe-image')]/../..` }, max: 4 },
  // dcl-one's label is cased by CSS (JumpInCTA uppercase), so its words are compared case-blind.
  { name: "vibe.cta", selector: { reference: "xpath=//main//button[normalize-space()='HANG OUT NOW']", "dcl-one": "xpath=//main//a[translate(normalize-space(),'abcdefghijklmnopqrstuvwxyz','ABCDEFGHIJKLMNOPQRSTUVWXYZ')='HANG OUT NOW']" } },
  { name: "rituals.title", selector: heading("Your Weekly Rituals") },
  { name: "rituals.card", selector: { reference: `${after("Your Weekly Rituals", "")}//button[string-length(normalize-space())=0 and not(@aria-label) and .//img]`, "dcl-one": `${after("Your Weekly Rituals", "")}//a[.//img]` }, max: 5 },
  { name: "banner.title", selector: heading("Come Hang Out") },
  { name: "banner.cta", selector: { reference: "xpath=(//main//a[contains(@href,'place=come-hang-out') or contains(@href,'apps.apple.com')])[last()]", "dcl-one": "xpath=(//main//a[starts-with(normalize-space(),'Download for')])[last()] || xpath=(//main//a[@aria-label='Download on the App Store'])[last()]" } },
];

/**
 * The Jump Into What's Happening (mobile) and Your Weekly Rituals carousels advance every 5 s and 4 s (sites:
 * components/Carousel autoplayDelay; dcl-one: library/ui/apps/sites/shared/Carousel.tsx) from when each side starts them: sites
 * mounts its lower sections on the first scroll, dcl-one's Carousel starts once half of it is on screen; either way after this
 * setup, so the slide a capture lands on is not a race: both sides hold their first slide.
 * The section backgrounds of Jump Into What's Happening and Come Hang Out are a WebGL <canvas> animated over a
 * static picture (sites: decentraland-ui2 AnimatedBackground; dcl-one: shop/discovery/components/animated-background): both sides hide
 * the canvas, so the static fallback and the headings, cards and CTAs over it are compared. The Catch the Vibe
 * clips are preload=metadata posters that play only on hover: they are compared as they are.
 * The lower sections' slides are lazy images (sites: WeeklyRituals.tsx loading="lazy", mounted on the first
 * scroll), which the capture's own pass can outrun: one pass first waits for the images in view at each step.
 * The desktop Hero plays the same looping clip on both sides (sites: Hero.tsx, video_landing.webm; dcl-one:
 * assets/home/hero.webm, the same bytes) once the page is idle, so its frame depends on playback time. Both
 * sides hold it paused at 0 s, the frame each shows the moment it becomes ready.
 */
async function holdMotion(page: Page): Promise<void> {
  await page.evaluate(() => {
    const every = window.setInterval;
    window.setInterval = ((fn: TimerHandler, ms?: number, ...args: unknown[]) => (ms === 4000 || ms === 5000 ? 0 : every(fn, ms, ...args))) as typeof window.setInterval;
  });
  await holdLivePulse(page);
  await hideCanvas(page);
  await page.evaluate(async () => {
    const pause = (ms: number) => new Promise((r) => setTimeout(r, ms));
    for (let y = 0; y < document.documentElement.scrollHeight; y += Math.round(innerHeight * 0.8)) {
      window.scrollTo(0, y);
      await pause(200);
      const inView = [...document.querySelectorAll<HTMLImageElement>("main img")].filter((i) => {
        const b = i.getBoundingClientRect();
        return !i.complete && b.width > 0 && b.bottom > 0 && b.top < innerHeight && b.right > 0 && b.left < innerWidth;
      });
      await Promise.all(inView.map((i) => Promise.race([new Promise((r) => { i.addEventListener("load", r, { once: true }); i.addEventListener("error", r, { once: true }); }), pause(5000)])));
    }
    window.scrollTo(0, 0);
  });
  if ((page.viewportSize()?.width ?? 0) < 991) return;
  const ready = await page.waitForSelector('video.hero-video-overlay[data-ready="true"]', { timeout: 10_000 }).catch(() => null);
  if (!ready) return;
  const held = await page.evaluate(
    () =>
      new Promise<boolean>((resolve) => {
        const v = document.querySelector<HTMLVideoElement>("video.hero-video-overlay")!;
        v.pause();
        v.addEventListener("seeked", () => resolve(true), { once: true });
        v.currentTime = 0;
        setTimeout(() => resolve(false), 2000);
      }),
  );
  if (!held) console.warn(`landing: the hero video's seek to 0 s did not end (${page.url()})`);
}

const landing: PageSpec = {
  id: "landing",
  title: "Landing page",
  reference: "sites",
  path: "/",
  // The Hero h1 (words differ by viewport: Hero.tsx); dcl-one renders both and shows one.
  ready: { reference: HERO_H1, "dcl-one": `${HERO_H1} >> visible=true` },
  setup: holdMotion,
  keys,
  ...sitesFrameRules(),
  unrecorded: SITES_UNRECORDED,
};

export default [landing, signedIn(landing, "Landing page, signed in as the test wallet")] satisfies PageSpec[];
