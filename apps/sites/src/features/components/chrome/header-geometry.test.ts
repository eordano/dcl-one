import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  findBrowser,
  openSession,
} from "../../../../../../library/ui/figma/review/scripts/capture-browser.mjs";

const root = new URL("../../../../../../", import.meta.url);
const css = [
  "apps/ui3/src/components/site-navigation.css",
  "apps/ui3/src/web/frames/sitelayout.css",
  "apps/ui3/src/shop/discovery/frames/shoptheme.css",
  "apps/ui3/src/social/social-page.css",
]
  .map((file) => readFileSync(fileURLToPath(new URL(file, root)), "utf8"))
  .join("\n");
// This file starts the browser while the other test files of the run use every core, so the browser can need
// longer than the helper's default start limit to publish its DevTools port.
const BROWSER_START_MS = 40_000;
// Longer than the start limit plus the helper's endpoint and socket limits (10 s each), so that a slow start
// that fails in one of those steps ends with the helper's diagnostics and not with a hook timeout.
const FIXTURE_MS = BROWSER_START_MS + 20_000;
let session: Awaited<ReturnType<typeof openSession>>;
let startup: ReturnType<typeof openSession> | undefined;
const startupController = new AbortController();
// Teardown also owns a startup that outlives this fixture's integration budget.
beforeAll(async () => {
  startup = (async () =>
    openSession(
      await findBrowser([
        process.env.BROWSER_PATH,
        process.env.CHROME_PATH,
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
      ]),
      { signal: startupController.signal, startTimeout: BROWSER_START_MS },
    ))();
  session = await startup;
}, FIXTURE_MS);
afterAll(async () => {
  startupController.abort(new Error("Header geometry fixture disposed"));
  const opened = await startup?.catch(() => undefined);
  await opened?.close();
}, 30_000);

describe("when native and legacy chrome hosts resolve their header offsets", () => {
  for (const width of [390, 992, 1024, 1440]) {
    for (const host of ["create", "social", "shop"] as const) {
      it(`should propagate the native header through ${host} token scopes at ${width}px`, async () => {
        await session.send("Emulation.setDeviceMetricsOverride", {
          width,
          height: 900,
          deviceScaleFactor: 1,
          mobile: false,
        });
        const child = host === "create" ? "ch" : "cs";
        const html = `<style>body{margin:0}${css}</style><div class="site-layout ui2" data-header-layout="native"><div class="site-layout__navigation"></div><div class="site-layout__content"><div class="${child} ui2 ${host === "shop" ? "shop-chrome" : ""} ${host === "social" ? "social-web" : ""}"><div class="soc"></div></div></div></div>`;
        await session.navigate(`data:text/html,${encodeURIComponent(html)}`);
        const response = await session.send("Runtime.evaluate", {
          expression: `(() => {const root=document.querySelector('.site-layout'), child=document.querySelector('.site-layout__content>div');return {root:getComputedStyle(root).getPropertyValue('--site-header-h').trim(), child:getComputedStyle(child).getPropertyValue('--site-header-h').trim(), shop:getComputedStyle(child).getPropertyValue('--shop-nav-h').trim(), height:child.getBoundingClientRect().height, social:document.querySelector('.soc').getBoundingClientRect().height}})()`,
          returnByValue: true,
        });
        const actual = (
          response as {
            result: {
              value: {
                root: string;
                child: string;
                shop: string;
                height: number;
                social: number;
              };
            };
          }
        ).result.value;
        const height = width < 1024 ? 64 : 80;
        expect(actual.root).toBe(`${height}px`);
        expect(actual.child).toBe(`${height}px`);
        if (host === "shop") expect(actual.shop).toBe(`${height}px`);
        expect(actual.height).toBe(900 - height);
        if (host === "social") expect(actual.social).toBe(900 - height);
      });
    }
  }
  it("should preserve the legacy standalone desktop header token", async () => {
    await session.send("Emulation.setDeviceMetricsOverride", {
      width: 1440,
      height: 900,
      deviceScaleFactor: 1,
      mobile: false,
    });
    await session.navigate(
      `data:text/html,${encodeURIComponent(`<style>${css}</style><div class="ui2" id="legacy"></div><div class="ui2 shop-chrome" id="shop"></div>`)}`,
    );
    const response = await session.send("Runtime.evaluate", {
      expression: `[...document.querySelectorAll('[id]')].map(el=>getComputedStyle(el).getPropertyValue('--site-header-h').trim())`,
      returnByValue: true,
    });
    expect((response as { result: { value: string[] } }).result.value).toEqual([
      "72px",
      "92px",
    ]);
  });
});
