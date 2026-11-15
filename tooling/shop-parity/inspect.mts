// Evaluate an expression in one side's page, over the recording, to find selectors and causes.
//
//   node inspect.mts <page-id|/path> <desktop|mobile> <reference|dcl-one> '<js expression>'
//   node inspect.mts item desktop dcl-one 'document.querySelector("[data-testid=item-price]").outerHTML'
//
// A page id runs that page's interactions first (the cart adds its item) and signs in when the page does.
import { join } from "node:path";
import { PAGES } from "./pages.mts";
import { defaultSet } from "./capture.mts";
import { launch, sideContext, VIEWPORTS } from "./lib/browser.mts";
import { RECORDINGS } from "./lib/paths.mts";
import { Recording } from "./lib/recording.mts";
import { routeSide } from "./lib/route.mts";
import { SEED, startServers } from "./lib/run.mts";
import { pagePath, referenceOf } from "./lib/spec.mts";
import { signIn, walletKey } from "./lib/session.mts";

const [target, vpName, side, expr] = process.argv.slice(2);
if (!target || !vpName || (side !== "reference" && side !== "dcl-one") || !expr) {
  console.error("usage: node inspect.mts <page-id|/path> <desktop|mobile> <reference|dcl-one> '<js expression>'");
  process.exit(2);
}
const recording = new Recording(join(RECORDINGS, process.env.SHOP_PARITY_SET ?? defaultSet()));
const spec = PAGES.find((p) => p.id === target);
const targets = recording.manifest?.targets ?? {};
const now = Date.parse(recording.manifest!.recordedAt);
// A bare path is opened on the shop reference; a page id on the reference its spec names.
const reference = spec ? referenceOf(spec) : "shop";
const servers = await startServers(recording, now, [reference]);
let browser: Awaited<ReturnType<typeof launch>>["browser"] | undefined;
try {
  ({ browser } = await launch());
  const vp = VIEWPORTS.find((v) => v.name === vpName) ?? VIEWPORTS[0]!;
  const context = await sideContext(browser, vp, { now, seed: SEED });
  if (spec?.signedIn) await signIn(context, walletKey(), now);
  if (spec?.init) await spec.init(context, side);
  const origin = servers.origin(side, reference);
  await routeSide(context, recording, side, origin);
  const page = await context.newPage();
  await page.goto(new URL(spec ? pagePath(spec, targets, side) : target, origin).href, { waitUntil: "load" });
  if (spec?.setup) await spec.setup(page, side, targets);
  await page.waitForLoadState("networkidle", { timeout: 15_000 }).catch(() => {});
  if (spec) await page.locator(spec.ready[side]).first().waitFor({ timeout: 20_000 });
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  console.log(JSON.stringify(await page.evaluate(expr), null, 1));
} finally {
  await browser?.close();
  await servers.close();
}
