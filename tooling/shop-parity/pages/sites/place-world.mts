// A world's page: the first world card of the reference /places grid, found while recording (discoverAt), at
// /places/world/<name> on both sides (dcl-one redirects it to /places/<id>). With people in the world a desktop
// browser shows the live view; a phone, or a world nobody is in, shows the Jump in card, on both sides.
import type { Page } from "playwright-core";
import { signedIn, type PageSpec } from "../../lib/spec.mts";
import { NAVIGATION_TIMEOUT_MS, READY_TIMEOUT_MS } from "../../lib/spec.mts";
import { control, holdLivePulse, mainText, sitesFrame, sitesFrameRules, UP } from "./frame.mts";
import { PLACE_UNRECORDED, REFERENCE_PLACE_TITLE } from "./place.mts";

/** The recording's target holding the world's path (/places/world/<name>). */
export const WORLD_PLACE = "worldPlace";

/** The reference's /places cards (LiveEventCard and PlaceCard, each a div with role=button): the first four are Live Now's. */
const GRID_CARDS = "xpath=(//main//div[@role='button'])[position()>4]";

/**
 * The first card of the /places grid that opens a world, by opening the cards as a visitor does: a card carries no
 * link, it navigates to /places/world/<name> (sites: features/discover/discover.helpers.ts) on click.
 */
export async function discoverWorld(page: Page): Promise<string | null> {
  try {
    const places = page.url();
    await page.locator(GRID_CARDS).first().waitFor({ state: "visible", timeout: READY_TIMEOUT_MS });
    const cards = await page.locator(GRID_CARDS).count();
    for (let i = 0; i < cards; i++) {
      // The card's top-left corner is its cover: away from the creator link and the JUMP IN button.
      await page.locator(GRID_CARDS).nth(i).click({ position: { x: 8, y: 8 }, force: true });
      await page.waitForURL(/\/places\/(place|world)\//, { timeout: NAVIGATION_TIMEOUT_MS });
      const path = new URL(page.url()).pathname;
      if (/^\/places\/world\/[^/]+$/.test(path)) return path;
      await page.goto(places, { waitUntil: "load" });
      await page.locator(GRID_CARDS).first().waitFor({ state: "visible", timeout: READY_TIMEOUT_MS });
    }
    return null;
  } catch {
    return null;
  }
}

/** The scene title: a paragraph on the reference (SceneTitle), the h1 on dcl-one (PlaceDetail). */
const TITLE = { reference: REFERENCE_PLACE_TITLE, "dcl-one": "xpath=//main//h1" };

const placeWorld: PageSpec = {
  id: "place-world",
  title: "World detail: the first world of the /places grid",
  reference: "sites",
  path: (t) => {
    if (!t[WORLD_PLACE]) throw new Error(`the recording has no ${WORLD_PLACE} target (record place-world: its discovery reads /places)`);
    return t[WORLD_PLACE];
  },
  ready: TITLE,
  keys: [
    ...sitesFrame("dtb"),
    { name: "title", selector: TITLE },
    { name: "creator", selector: "xpath=//main//p[starts-with(normalize-space(),'By ')]" },
    // A phone always gets the Jump in card, never the live view (sites: DiscoverScenePage.tsx, isMobile || livePlayers === 0).
    { name: "cta.explore", selector: control("EXPLORE THE SCENE"), absentOn: "mobile" },
    { name: "cta.jump-in", selector: control("JUMP IN") },
    { name: "chat", selector: `xpath=//main//*[self::h2 or self::h6][${UP}='IN-WORLD CHAT']`, absentOn: "mobile" },
    { name: "what-to-expect", selector: mainText("What to expect") },
    { name: "description", selector: "xpath=//main//*[normalize-space()='What to expect']/following-sibling::*[1]" },
  ],
  setup: holdLivePulse,
  ...sitesFrameRules(),
  unrecorded: PLACE_UNRECORDED,
  discoverAt: "/places",
  discover: async (page) => ({ [WORLD_PLACE]: await discoverWorld(page) }),
};

export default [placeWorld, signedIn(placeWorld, "A world, signed in as the test wallet")] satisfies PageSpec[];
