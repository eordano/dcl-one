// /places: Live Now and the Explore grid on both sides. While recording, the reference page yields
// the position of the first Live Now scene that is not Genesis Plaza, which place-live compares.
import type { Page } from "playwright-core";
import { signedIn, type PageSpec } from "../../lib/spec.mts";
import { NAVIGATION_TIMEOUT_MS, READY_TIMEOUT_MS } from "../../lib/spec.mts";
import { holdLivePulse, mainText, SITES_UNRECORDED, sitesFrame, sitesFrameRules } from "./frame.mts";
import { LIVE_PLACE, REFERENCE_PLACE_TITLE } from "./place.mts";

const LIVE_NOW = mainText("Live Now").reference;
/** The Live Now rail's cards: the reference's LiveEventCard is a div with role=button that navigates to the scene. */
const LIVE_CARDS = `${LIVE_NOW}/..//*[@role='button']`;

/** Genesis Plaza covers many parcels (its page shows -3,-2 for 0,0): it is skipped by its title. */
const GENESIS = "Genesis Plaza";

/**
 * The scene page of the first Live Now card that is not Genesis Plaza, by opening the cards as a visitor does,
 * in a second page of the same context (so its reads are recorded under the same routes and clock):
 * the card carries no link, its position lives in the data its click navigates with.
 */
export async function discoverLivePlace(page: Page): Promise<string | null> {
  const other = await page.context().newPage();
  try {
    await other.goto(new URL("/places", page.url()).href, { waitUntil: "load" });
    const heading = other.locator(LIVE_NOW).first();
    if (!(await heading.waitFor({ state: "visible", timeout: READY_TIMEOUT_MS }).then(() => true, () => false))) return null;
    const cards = await other.locator(LIVE_CARDS).count();
    for (let i = 0; i < cards; i++) {
      // The card's top-left corner is its cover: away from the creator link and the JUMP IN button.
      await other.locator(LIVE_CARDS).nth(i).click({ position: { x: 8, y: 8 }, force: true });
      await other.waitForURL(/\/places\/(place|world)\//, { timeout: NAVIGATION_TIMEOUT_MS });
      const path = new URL(other.url()).pathname;
      if (/^\/places\/place\//.test(path)) {
        const title = other.locator(REFERENCE_PLACE_TITLE).first();
        await title.waitFor({ state: "visible", timeout: READY_TIMEOUT_MS });
        if ((await title.innerText()).trim() !== GENESIS) return path;
      }
      await other.goBack({ waitUntil: "load" });
      await heading.waitFor({ state: "visible", timeout: READY_TIMEOUT_MS });
    }
    return null;
  } catch {
    return null;
  } finally {
    await other.close().catch(() => {});
  }
}

/** The reference's cards (LiveEventCard and PlaceCard are both a div with role=button): the first four are Live Now's, the rest the Explore grid's. */
const REFERENCE_CARDS = "(//main//div[@role='button'])";
/** dcl-one's cards (library/ui/apps/sites/places/cards): the element that holds the title's covering link, an article in Live Now and the grid cell's div. */
const ONE_CARD = ":has(> div > h3 > a[href^='/places/'])";
const ONE_LIVE = `main section article${ONE_CARD}`;
const ONE_GRID = `main ul > li > div${ONE_CARD}`;
const SEARCH = "xpath=//main//input[@placeholder='Search places']";

const places: PageSpec = {
  id: "places",
  title: "Places (Live Now and Explore)",
  reference: "sites",
  path: "/places",
  // The grid loads its next page when its end nears the viewport, so the depth either side reaches depends on
  // timing: compare Live Now and the first grid page, above the first page's end on both viewports.
  maxHeight: 3500,
  ready: mainText("Explore all"),
  keys: [
    ...sitesFrame("dtb"),
    { name: "live.title", selector: mainText("Live Now") },
    { name: "live.card", selector: { reference: `xpath=${REFERENCE_CARDS}[position()<=4]`, "dcl-one": ONE_LIVE }, max: 4 },
    { name: "tab.explore-all", selector: mainText("Explore all") },
    { name: "tab.favourites", selector: mainText("Favourites") },
    { name: "tab.my-places", selector: mainText("My places") },
    { name: "search", selector: SEARCH },
    // A phone moves the category select into the Filters drawer (sites: DiscoverHomePage.tsx DesktopFilters, DrawerSelect).
    { name: "category", selector: mainText("Any Category"), absentOn: "mobile" },
    { name: "grid.card", selector: { reference: `xpath=${REFERENCE_CARDS}[position()>4]`, "dcl-one": ONE_GRID }, max: 5 },
  ],
  setup: holdLivePulse,
  ...sitesFrameRules(),
  unrecorded: SITES_UNRECORDED,
  discover: async (page) => ({ [LIVE_PLACE]: await discoverLivePlace(page) }),
};

export default [places, signedIn(places, "Places, signed in as the test wallet")] satisfies PageSpec[];
