// Capture one page on one side: settle, run the page's interactions, read the structure, and take
// the page as a user scrolls it (viewport tiles stitched by their real scroll offset, so fixed
// backgrounds and sticky bars are painted as they are seen, not stretched by a full-page grab).
import { signIn, walletKey } from "./session.mts";
import type { Browser, BrowserContext, Page } from "playwright-core";
import { sideContext, type Viewport } from "./browser.mts";
import { collectInPage, type Box, type Collected } from "./collect.mts";
import { decodePng, encodePng, pasteTile, type Rgba } from "./png.mts";
import type { Recording } from "./recording.mts";
import { REDECODE_HEADER, routeSide, type SideLog } from "./route.mts";
import { pagePath, READY_TIMEOUT_MS, type PageSpec, type Side } from "./spec.mts";
import { referenceDist, referencePin } from "./paths.mts";

/** The sites reference's build: decentraland.org's own static files (public/) are answered from it. */
function referenceStatic(spec: PageSpec): string | undefined {
  return spec.reference === "sites" ? referenceDist(referencePin("sites")) : undefined;
}

export type SideCapture = {
  side: Side;
  url: string;
  png: Buffer;
  collected: Collected;
  masks: Box[];
  regions: Box[];
  /** The masks' boxes per declared mask, in spec order. */
  maskGroups: Box[][];
  /** The regions' boxes per declared region, in spec order. */
  regionGroups: Box[][];
  console: string[];
  /** Every response with an error status, as "<status> <method> <url>", so a miss on one side only shows. */
  failed: string[];
  log: SideLog;
  error?: string;
};

const IMAGE_READINESS_TIMEOUT_MS = 5000;

const BUSY = "[aria-busy=true], [data-testid*=skeleton], .skeleton, [class*=Skeleton], [class*=skeleton]";

/** Network idle, the page's ready element visible, no skeleton, fonts and visible images loaded, animations stopped. */
export async function settle(page: Page, ready?: string): Promise<void> {
  await page.waitForLoadState("networkidle", { timeout: 15_000 }).catch(() => {});
  if (ready) await page.locator(ready).first().waitFor({ state: "visible", timeout: READY_TIMEOUT_MS });
  await page
    .waitForFunction((busy: string) => ![...document.querySelectorAll(busy)].some((e) => e.getBoundingClientRect().height > 0), BUSY, { timeout: 15_000 })
    .catch(() => {});
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await visibleImages(page);
  await page.waitForLoadState("networkidle", { timeout: 10_000 }).catch(() => {});
  await frames(page);
  await still(page);
}

/**
 * Decode every loaded image and stop every animation, as the screenshot does (infinite ones go back to
 * their start, finite ones to their end), so boxes and pixels are not read at a random frame or before
 * an image is decoded. Both sides get the same treatment.
 */
async function still(page: Page): Promise<void> {
  await page.evaluate(async () => {
    await document.fonts.ready;
    await Promise.all([...document.images].filter((i) => i.complete && i.naturalWidth > 0).map((i) => i.decode().catch(() => {})));
    for (const a of document.getAnimations()) {
      if (a.effect?.getComputedTiming().endTime === Infinity) a.cancel();
      else a.finish();
    }
  });
}

/**
 * Draw every scaled image from one fresh decode. Chromium rasterizes a downscaled image from a mip of
 * the decode it holds or from the full decode, depending on what its cache holds when the tile is
 * drawn, and the two differ by a few pixels (What's On posters: 8 on desktop). Which one a capture got
 * depended on the loads before it, so the same bytes on the same layout compared unequal. Re-attaching
 * each loaded, scaled image from a blob of its own bytes (fetched through the replay) gives one decode
 * at the final size on both sides. Responsive images keep their source set.
 */
async function redecodeImages(page: Page): Promise<void> {
  await page.evaluate(async (header: string) => {
    const scaled = [...document.images].filter(
      (i) =>
        i.complete &&
        i.naturalWidth > 0 &&
        !i.srcset &&
        !(i.parentElement instanceof HTMLPictureElement) &&
        /^https?:/.test(i.currentSrc) &&
        (i.width !== i.naturalWidth || i.height !== i.naturalHeight),
    );
    await Promise.all(
      scaled.map(async (i) => {
        const res = await fetch(i.currentSrc, { headers: { [header]: "1" } }).catch(() => null);
        // An answer that is not an image to decode again: its body is dropped, not left unread.
        if (res && !res.ok) await res.body?.cancel().catch(() => {});
        const blob = res?.ok ? await res.blob() : null;
        if (!blob || !blob.type.startsWith("image/")) return;
        i.src = URL.createObjectURL(blob);
        await i.decode().catch(() => {});
      }),
    );
  }, REDECODE_HEADER);
}

const docHeight = (page: Page, cap?: number) => page.evaluate((c: number) => Math.min(document.documentElement.scrollHeight, c), cap ?? Number.MAX_SAFE_INTEGER);

/** Scroll through once (to the page's depth cap) so lazy rails and images load, then return to the top. */
async function warm(page: Page, vh: number, cap?: number): Promise<void> {
  const height = await docHeight(page, cap);
  for (let y = 0; y < height; y += Math.round(vh * 0.8)) {
    await scroll(page, y);
    await visibleImages(page);
  }
  await scroll(page, Math.max(0, height - vh));
  await visibleImages(page);
  await scroll(page, 0);
}

export async function visibleImages(page: Page) {
  await page.waitForFunction(() => [...document.images].every((image) => {
    const box = image.getBoundingClientRect();
    return box.bottom <= 0 || box.top >= innerHeight || box.right <= 0 || box.left >= innerWidth || box.width === 0 || box.height === 0 || image.complete;
  }), undefined, { timeout: IMAGE_READINESS_TIMEOUT_MS }).catch((error: unknown) => {
    if (!(error instanceof Error) || error.name !== "TimeoutError") throw error;
  });
}

export async function scroll(page: Page, top: number) {
  const target = await page.evaluate((y: number) => {
    const target = Math.max(0, Math.min(y, document.documentElement.scrollHeight - innerHeight));
    window.scrollTo({ top: target, behavior: "instant" });
    return target;
  }, top);
  await page.waitForFunction((y: number) => Math.abs(scrollY - Math.max(0, Math.min(y, document.documentElement.scrollHeight - innerHeight))) <= 1, target, { timeout: 5000 });
  await frames(page);
}

async function frames(page: Page) {
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(null)))));
}

/**
 * Sticky elements are measured and painted once, where the layout puts them: relative with no offset
 * from measuring on, so a panel below the first screen is compared like any other content and its
 * boxes match its pixels. Both sides get the same treatment.
 */
async function unstick(page: Page): Promise<void> {
  await page.evaluate(() => {
    for (const el of document.body.querySelectorAll("*")) {
      if (getComputedStyle(el).position !== "sticky") continue;
      const style = (el as HTMLElement).style;
      style.setProperty("position", "relative", "important");
      for (const side of ["top", "right", "bottom", "left"]) style.setProperty(side, "auto", "important");
    }
  });
}

/**
 * Fixed bars (under half the viewport tall) are painted once, in the first tile; later tiles hide them
 * so the stitched page is not striped with copies. Full-height fixed layers (the page field) stay.
 * Both sides get the same treatment.
 */
async function hideBars(page: Page, vh: number, hide: boolean): Promise<void> {
  await page.evaluate(
    ([limit, on]) => {
      const KEY = "data-parity-hidden";
      if (!on) {
        for (const el of document.querySelectorAll(`[${KEY}]`)) {
          (el as HTMLElement).style.visibility = el.getAttribute(KEY)!;
          el.removeAttribute(KEY);
        }
        return;
      }
      for (const el of document.body.querySelectorAll("*")) {
        const cs = getComputedStyle(el);
        if (cs.position !== "fixed") continue;
        const r = el.getBoundingClientRect();
        if (r.height === 0 || r.height >= (limit as number) / 2) continue;
        el.setAttribute(KEY, (el as HTMLElement).style.visibility);
        (el as HTMLElement).style.visibility = "hidden";
      }
    },
    [vh, hide] as const,
  );
}

/**
 * A viewport shot the next shot repeats. Chromium draws a tile it has not rasterized yet as its layer's
 * background, so a frame captured right after a scroll can hold a box without its text (the reference
 * footer's legal bar, once in dozens of gate runs). A shot the next one does not repeat is taken again, a
 * few times at most: a tile with a playing video never repeats.
 */
async function steadyShot(page: Page): Promise<Rgba> {
  const options = { animations: "disabled", caret: "hide", scale: "css" } as const;
  let last = await page.screenshot(options);
  for (let i = 0; i < 3; i++) {
    const next = await page.screenshot(options);
    if (next.equals(last)) break;
    last = next;
  }
  return decodePng(last);
}

async function tiles(page: Page, vp: Viewport, cap?: number): Promise<Rgba> {
  const height = await docHeight(page, cap);
  const out: Rgba = { width: vp.width, height, data: new Uint8Array(vp.width * height * 4) };
  let filled = 0;
  for (let y = 0; filled < height; y += vp.height) {
    await scroll(page, y);
    await visibleImages(page);
    const real = await page.evaluate(() => Math.round(window.scrollY));
    const shot = await steadyShot(page);
    const next = pasteTile(out, filled, real, shot);
    if (next <= filled) break;
    if (filled === 0) await hideBars(page, vp.height, true);
    filled = next;
  }
  await hideBars(page, vp.height, false);
  await page.evaluate(() => window.scrollTo(0, 0));
  return { width: vp.width, height: filled, data: out.data.subarray(0, vp.width * filled * 4) };
}

type CaptureOptions = { origin: string; recording: Recording; now: number; seed: number; targets: Record<string, string>; onPage?: (page: Page) => Promise<void> };

export async function captureSide(
  browser: Browser,
  spec: PageSpec,
  vp: Viewport,
  side: Side,
  opts: CaptureOptions,
): Promise<SideCapture> {
  const context = await sideContext(browser, vp, { now: opts.now, seed: opts.seed });
  try {
    if (spec.signedIn) await signIn(context, walletKey(), opts.now);
    if (spec.init) await spec.init(context, side);
    return await captureIn(context, spec, vp, side, opts);
  } finally {
    await context.close();
  }
}

async function captureIn(context: BrowserContext, spec: PageSpec, vp: Viewport, side: Side, opts: CaptureOptions): Promise<SideCapture> {
  const keys = spec.keys.map((k) => ({ name: k.name, selector: typeof k.selector === "string" ? k.selector : k.selector[side], max: k.max ?? 1 }));
  const masks = (spec.masks ?? []).flatMap((m) => m.selector[side]);
  const regions = (spec.regions ?? []).flatMap((r) => r.selector[side]);
  const log = await routeSide(context, opts.recording, side, opts.origin, side === "reference" ? referenceStatic(spec) : undefined);
  const page = await context.newPage();
  const consoleLines: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error" || m.type() === "warning") consoleLines.push(`${m.type()}: ${m.text().slice(0, 300)}`);
  });
  page.on("pageerror", (e) => consoleLines.push(`pageerror: ${String(e).slice(0, 300)}`));
  const failed: string[] = [];
  page.on("response", (r) => {
    if (r.status() >= 400) failed.push(`${r.status()} ${r.request().method()} ${r.url().slice(0, 300)}`);
  });
  try {
    await page.goto(new URL(pagePath(spec, opts.targets, side), opts.origin).href, { waitUntil: "load" });
    if (spec.setup) {
      await settle(page);
      await spec.setup(page, side, opts.targets);
    }
    await settle(page, spec.ready[side]);
    await warm(page, vp.height, spec.maxHeight);
    // A transition started by the scroll back to the top (a bar deepening on scroll) would be measured at
    // whatever point each app's handler happened to start it; end states are what is compared.
    await page.addStyleTag({ content: "*, *::before, *::after { transition-duration: 0s !important; transition-delay: 0s !important; }" });
    await redecodeImages(page);
    await settle(page);
    if (opts.onPage) await opts.onPage(page);
    await unstick(page);
    await frames(page);
    const collected = await page.evaluate(collectInPage, { keys, masks: [...masks, ...regions] });
    const maskGroups: Box[][] = [];
    for (const m of spec.masks ?? []) maskGroups.push((await page.evaluate(collectInPage, { keys: [], masks: m.selector[side] })).masks);
    const regionGroups: Box[][] = [];
    for (const r of spec.regions ?? []) regionGroups.push((await page.evaluate(collectInPage, { keys: [], masks: r.selector[side] })).masks);
    const png = encodePng(await tiles(page, vp, spec.maxHeight));
    if (spec.maxHeight) {
      const below = (b: { y: number }) => b.y < spec.maxHeight!;
      collected.clipped = collected.nodes.filter((n) => n.id.startsWith("key:") && !below(n.box)).map((n) => n.id);
      collected.nodes = collected.nodes.filter((n) => below(n.box));
      collected.texts = collected.texts.filter((n) => below(n.box));
      collected.height = Math.min(collected.height, spec.maxHeight);
    }
    return { side, url: page.url(), png, collected, masks: maskGroups.flat(), regions: regionGroups.flat(), maskGroups, regionGroups, console: consoleLines, failed, log };
  } catch (err) {
    const png = await page.screenshot({ fullPage: true }).catch(() => Buffer.alloc(0));
    return {
      side,
      url: page.url(),
      png,
      collected: { url: page.url(), width: vp.width, height: 0, nodes: [], texts: [], masks: [] },
      masks: [],
      regions: [],
      maskGroups: [],
      regionGroups: [],
      console: consoleLines,
      failed,
      log,
      error: String(err).split("\n")[0],
    };
  }
}
