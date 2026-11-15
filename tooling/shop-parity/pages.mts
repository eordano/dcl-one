// The pages a parity capture compares. Each runs on both sides with the same recording, path,
// viewport and interactions. Key elements name what matters on the page; most pair by testid
// (ui3 kept upstream's data-testid values), a few need a selector per side. Masks hide only what
// cannot be equal between two renders (3D previews, live timers) and say why; expected entries
// are deliberate differences, each with the rule (the repository's AGENTS.md) or code behind it.
//
// The Shop pages (reference.json) are declared here; the decentraland.org pages
// (reference-sites.json) each in their own file under pages/sites/, aggregated at the end.
import type { Page } from "playwright-core";
import { waitUntil } from "../../library/async/index.ts";
import { collectionOfItem, creatorOfProfile } from "./lib/discover.mts";
import { both, signedIn, type Expectation, type KeyElement, type Mask, type PageSpec, type PerSide, type Region } from "./lib/spec.mts";
import { NAV_SECTIONS, UP } from "./pages/sites/frame.mts";
import { SITES_PAGES } from "./pages/sites/index.mts";

const both1 = (s: string): PerSide<string[]> => ({ reference: [s], "dcl-one": [s] });

/** An element by its visible words, case-insensitively (the same words on both sides). */
const byText = (tag: string, text: string, scope = "//main") => `xpath=${scope}//${tag}[${UP}='${text.toUpperCase()}']`;
/**
 * The innermost element in the cart drawer (a dialog both sides portal to the body, outside <main>) whose
 * visible words start with `text`, case-insensitively.
 */
const inDrawer = (tag: string, text: string) =>
  `xpath=//*[@role='dialog']//${tag}[starts-with(${UP},'${text.toUpperCase()}') and not(.//${tag}[starts-with(${UP},'${text.toUpperCase()}')])]`;
/** Adds the detail page's item (or the second card's) through the page itself, as a buyer does. */
async function addToCart(page: Page, path?: string, expectedCount = 1) {
  if (path) await page.goto(new URL(path, page.url()).href, { waitUntil: "load" });
  const add = page.locator(byText("button", "Add to cart"));
  await add.first().waitFor({ state: "visible", timeout: 20_000 });
  if (expectedCount === 1) {
    const drawer = page.getByRole("dialog", { name: /shopping cart/i });
    await waitUntil(async (remainingMs) => {
      if (await drawer.isVisible()) return true;
      await page.getByTestId("subnav-cart").click({ timeout: remainingMs });
      return await drawer.isVisible() ? true : undefined;
    }, { timeoutMs: 20_000, description: "the cart responding before the first add" });
    await page.keyboard.press("Escape");
    await drawer.waitFor({ state: "hidden", timeout: 20_000 });
  }
  await page.waitForFunction(cartHasCount, expectedCount - 1, { timeout: 20_000 });
  await add.first().click();
  await page.waitForFunction(cartHasCount, expectedCount, { timeout: 20_000 });
}
/** The buy dialog's settled screens (upstream BuyModal's titles past its loading phase). */
const BUY_SCREENS = /Buy Credits and Item|Buy Item|Choose your payment method|Purchase complete!|Something went wrong/i;

/** The detail page's Buy now: upstream's button also carries its price (hidden), so its words only start with it. */
const BUY_NOW = `xpath=//main//button[starts-with(${UP},'BUY NOW')]`;

/** Opens the detail page's Buy now as a signed-in buyer does, and waits for the dialog to settle on a screen. */
async function buyNow(page: Page) {
  const buy = page.locator(BUY_NOW);
  await buy.first().waitFor({ state: "visible", timeout: 20_000 });
  await buy.first().click();
  const dialog = page.getByTestId("buy-modal");
  await dialog.waitFor({ state: "visible", timeout: 20_000 });
  await page.waitForFunction(
    (source) => new RegExp(source, "i").test(document.querySelector("[data-testid=buy-modal]")?.textContent ?? ""),
    BUY_SCREENS.source,
    { timeout: 20_000 },
  );
}

function cartHasCount(expected: number): boolean {
  const cart = document.querySelector("[data-testid=subnav-cart]");
  if (!cart) return false;
  const text = `${cart.getAttribute("aria-label") ?? ""} ${cart.textContent ?? ""}`;
  const count = Number(text.match(/\d+/)?.[0] ?? 0);
  return count === expected;
}

/** The nearest ancestor box of a text that is at least `minChildren` wide, e.g. a panel by its heading. */
const panelOf = (text: string) => `xpath=//main//*[${UP}='${text.toUpperCase()}' and not(*)]/ancestor::*[count(*)>=2][2]`;

/** The frame every Shop page shares: site nav, Shop subnav, footer. */
const FRAME_KEYS: KeyElement[] = [
  { name: "site-nav", selector: { reference: "xpath=//nav[.//*[normalize-space(text())='Discover']]", "dcl-one": "header[aria-label='Decentraland']" } },
  { name: "site-nav.signin", selector: { reference: "xpath=//nav[.//*[normalize-space(text())='Discover']]//*[self::a or self::button][normalize-space()='SIGN IN' or normalize-space()='Sign In' or normalize-space()='Sign in']", "dcl-one": "xpath=//header[@aria-label='Decentraland']//*[self::a or self::button][normalize-space()='SIGN IN' or normalize-space()='Sign In' or normalize-space()='Sign in']" } },
  { name: "subnav", selector: both("[data-testid=subnav]") },
  { name: "subnav.tabs", selector: both("[data-testid=subnav-tabs]") },
  { name: "subnav.tab", selector: both("[data-testid=subnav-tabs] a"), max: 4 },
  { name: "subnav.search", selector: { reference: "input[aria-label='Search the shop']", "dcl-one": "[data-testid=search-input]" } },
  { name: "subnav.credits", selector: both("[data-testid=subnav] a[href*=credits]") },
  { name: "subnav.favorites", selector: both("[data-testid=subnav] a[aria-label='My Favorites']") },
  { name: "subnav.cart", selector: both("[data-testid=subnav-cart]") },
  { name: "footer", selector: both("footer") },
];

/** Every page: WebGL canvases are 3D renders (product previews, the promos' live preview). */
const WEBGL: Mask = {
  selector: { reference: ["canvas"], "dcl-one": ["canvas"] },
  why: "WebGL canvas (3D preview): its pixels differ between frames and renderers; the box it sits in is still compared structurally",
};


const NO_MODELS =
  "3D models are not recorded (pictures are placeholders), so ui3's renderer shows its fallback notice inside the masked preview; the reference's preview iframe renders nothing there";

/** The base avatar ui3's 3D preview dresses: its wearables' entities are not recorded and the preview is masked. */
const PREVIEW_AVATAR = { match: /^POST peer \/content\/entities\/active .*urn:decentraland:off-chain:base-avatars:/, why: NO_MODELS };

/** The overview's only entity lookups: the promos' live preview, which starts once a promo is on screen (or not at all). */
const PROMO_PREVIEW = {
  match: /^POST peer \/content\/entities\/active /,
  why: "the promos' live 3D preview (discovery/components/LivePromo.tsx, masked) looks up its avatar and wearables only when it starts, which depends on timing; their entities are not recorded",
};

const OUTFIT_ORDER =
  "the reference shuffles Buy the Look on every visit within recency buckets (components/OutfitsRow/OutfitsRow.tsx, lib/outfits.ts orderOutfitsByRecency), so its order is not a property to match; the row's box and cards are still compared";

const PLAY_MODE =
  "the play-mode chip shows only when the item carries data.emote.loop (detail/ItemDetail.tsx, as upstream's own comment states); the reference shows PLAY ONCE on this wearable, whose recorded rows carry no emote data";

/**
 * The reference cart's first line card, whatever item the detail page put in it: the first innermost element
 * holding three or more buttons (the stepper, favourite and remove; the rail's cards below match too, later).
 * It holds for the recording's single credits mint; a resale (no stepper) or a shown chooser would need more.
 * dcl-one's side uses its own line classes.
 */
const LINE_CARD = "(//main//*[count(.//button)>=3 and not(.//*[count(.//button)>=3])])[1]";

const NEWSLETTER = "the third-party beehiiv newsletter iframe is replaced by a first-party form of the same box (discovery/frames/ShopFooter.tsx, posting to /v1/newsletter)";

/** The reference's footer loads that iframe from beehiiv, a host the harness does not record. */
const NEWSLETTER_EMBED = { match: /^GET embeds\.beehiiv\.com\//, why: NEWSLETTER };

/** Only the section links: the bar's height, brand, sign-in button and colours are still compared. */
const FRAME_REGIONS: Region[] = [
  { selector: { reference: ["xpath=//nav[.//*[normalize-space(text())='Discover']]//*[normalize-space(text())='Discover']/ancestor::div[count(*)>=4][1]"], "dcl-one": ["header[aria-label='Decentraland'] nav > div > ul"] }, why: NAV_SECTIONS },
  { selector: { reference: ["footer iframe"], "dcl-one": ["[data-testid=shop-footer-newsletter]"] }, why: NEWSLETTER },
];

const TEST_HOOKS = "ui3's unit tests find these elements by test ids the reference does not carry (ShopChrome, SearchDropdown, Items and CartPage tests); the elements themselves are compared by their key, testid-free path or text";

const FRAME_EXPECTED: Expectation[] = [
  { match: /^"(Discover|Play|Social|Create|Learn|Marketplace|Builder|Events|Places)"(#\d+)?$/, why: NAV_SECTIONS },
  { match: /^"Shop"$/, why: NAV_SECTIONS, only: ["left", "width"] },
  { match: /^key:site-nav$/, why: NAV_SECTIONS, only: ["text"] },
  { match: /(^|\/)(search|search-input|sort-select|total-credits|confirm-checkout)$/, status: "only-dcl-one", why: TEST_HOOKS },
  { match: /newsletter|^"Enter your email"$|^"SUBSCRIBE"$/i, why: NEWSLETTER },
  { match: /^key:footer$/, why: NEWSLETTER, only: ["text"] },
];

/** A rail's section, by testid where both have it, else by its heading's text. */
const rail = (testid: string, heading: string): PerSide<string> => ({
  reference: `[data-testid=${testid}] || xpath=//section[.//h2[normalize-space()='${heading}']]`,
  "dcl-one": `[data-testid=${testid}] || xpath=//section[.//h2[normalize-space()='${heading}']]`,
});
const within = (scope: PerSide<string>, inner: string): PerSide<string> => ({
  reference: scope.reference.split(" || ").map((s) => (s.startsWith("xpath=") ? `${s}//*[${inner}]` : `${s} [${inner.replace(/^@/, "").replace(/=/, "=")}]`)).join(" || "),
  "dcl-one": scope["dcl-one"].split(" || ").map((s) => (s.startsWith("xpath=") ? `${s}//*[${inner}]` : `${s} [${inner.replace(/^@/, "").replace(/=/, "=")}]`)).join(" || "),
});

const CARD_KEYS = (name: string, scope: PerSide<string>): KeyElement[] => [
  { name, selector: scope },
  { name: `${name}.title`, selector: { reference: scope.reference.split(" || ").map((s) => (s.startsWith("xpath=") ? `${s}//h2` : `${s} h2`)).join(" || "), "dcl-one": scope["dcl-one"].split(" || ").map((s) => (s.startsWith("xpath=") ? `${s}//h2` : `${s} h2`)).join(" || ") } },
  { name: `${name}.card`, selector: within(scope, "@data-testid='card'"), max: 5 },
  { name: `${name}.card.name`, selector: within(scope, "@data-testid='card-name'"), max: 5 },
  { name: `${name}.card.price`, selector: within(scope, "@data-testid='card-price'"), max: 5 },
  { name: `${name}.card.author`, selector: within(scope, "@data-testid='card-author'"), max: 5 },
];

export const PAGES: PageSpec[] = [
  {
    id: "overview",
    title: "Shop home",
    path: "/shop/overview",
    ready: both("[data-testid=hero-title]"),
    keys: [
      ...FRAME_KEYS,
      { name: "hero", selector: both("xpath=//section[.//*[@data-testid='hero-title']]") },
      { name: "hero.title", selector: both("[data-testid=hero-title]") },
      { name: "hero.cta", selector: both("[data-testid=hero-credits-cta]") },
      ...CARD_KEYS("trending", rail("trending-rail", "Trending Products")),
      { name: "outfits", selector: both("[data-testid=outfits-row]") },
      { name: "outfits.title", selector: both("[data-testid=outfits-row] h2") },
      { name: "outfits.card", selector: both("[data-testid=outfit-card]"), max: 6 },
      ...CARD_KEYS("new-creations", rail("new-creations-rail", "New Creations")),
      { name: "promos", selector: { reference: "xpath=//section[.//a[contains(@href,'items?category=emote')]][not(.//h2)]", "dcl-one": ".shop-overview__promos" } },
      { name: "top-creators", selector: both("[data-testid=top-creators]") },
      { name: "top-creators.card", selector: both("[data-testid=top-creator-card]"), max: 4 },
    ],
    masks: [WEBGL],
    regions: [...FRAME_REGIONS, { selector: both1("[data-testid=outfits-row-track]"), why: OUTFIT_ORDER }],
    expected: [...FRAME_EXPECTED, { match: /^key:outfits$/, why: OUTFIT_ORDER, only: ["text"] }],
    unrecorded: [PROMO_PREVIEW, NEWSLETTER_EMBED],
    discover: async (page) => {
      const href = await page.locator("xpath=(//*[@data-testid='outfit-card']//a[@href] | //a[@data-testid='outfit-card'])[1]").getAttribute("href").catch(() => null);
      return { outfit: href ? new URL(href, page.url()).pathname.replace(/^(?!\/shop)/, "/shop") : null };
    },
  },
  {
    id: "items",
    title: "Items list, filtered (wearables, epic)",
    path: "/shop/items?category=wearable&rarities=epic",
    ready: { reference: "[data-testid=grid] [data-testid=card]", "dcl-one": "[data-testid=grid] > *" },
    keys: [
      ...FRAME_KEYS,
      { name: "filters", selector: { reference: "aside, [data-testid=filters]", "dcl-one": "aside, [data-testid=filters]" } },
      { name: "filters.category", selector: { reference: "xpath=(//aside//button[@data-cat])[1]/../..", "dcl-one": "[data-testid=category-filter]" } },
      { name: "filters.rarity", selector: both("[data-testid=rarity-filter]") },
      { name: "toolbar", selector: { reference: "[data-testid=browse-toolbar], [data-testid=toolbar]", "dcl-one": "[data-testid=browse-toolbar]" } },
      { name: "grid", selector: both("[data-testid=grid]") },
      { name: "grid.card", selector: { reference: "[data-testid=grid] [data-testid=card]", "dcl-one": "[data-testid=grid] > *" }, max: 8 },
      { name: "grid.card.name", selector: both("[data-testid=grid] [data-testid=card-name]"), max: 8 },
      { name: "grid.card.price", selector: both("[data-testid=grid] [data-testid=card-price]"), max: 8 },
    ],
    discover: async (page) => {
      const href = await page.locator("[data-testid=grid] [data-testid=card] a[data-testid=card-link]").first().getAttribute("href");
      if (!href) throw new Error("no card in the reference items grid to take as the detail page");
      const second = await page.locator("[data-testid=grid] [data-testid=card] a[data-testid=card-link]").nth(1).getAttribute("href", { timeout: 5_000 }).catch(() => null);
      const shop = (h: string) => new URL(h, page.url()).pathname.replace(/^(?!\/shop)/, "/shop");
      return { item: shop(href), item2: second ? shop(second) : null };
    },
    maxHeight: 3000,
    masks: [WEBGL],
    regions: FRAME_REGIONS,
    expected: FRAME_EXPECTED,
    unrecorded: [NEWSLETTER_EMBED],
  },
  {
    id: "item",
    title: "Item detail (first card of the filtered list)",
    path: (t) => t.item ?? "/shop/items",
    ready: both("[data-testid=item-price]"),
    keys: [
      ...FRAME_KEYS,
      { name: "breadcrumb", selector: both("[aria-label=Breadcrumb]") },
      { name: "preview", selector: both("[data-testid=item-preview]") },
      { name: "info", selector: both("[data-testid=item-info]") },
      { name: "title", selector: both("[data-testid=item-info] h1") },
      { name: "favorite", selector: both("[data-testid=fav-count]") },
      { name: "tag.rarity", selector: both("[data-testid=detail-rarity-link]") },
      { name: "tag.category", selector: both("[data-testid=detail-category-link]") },
      { name: "tag.play-mode", selector: both("[data-testid=detail-play-mode]") },
      { name: "tag.export", selector: both("[data-testid=detail-export-blocked]") },
      { name: "creator", selector: both("[data-testid=creator]") },
      { name: "price", selector: both("[data-testid=item-price]") },
      { name: "stock", selector: { reference: "xpath=//main//*[" + UP + "='STOCK' and not(*)]/..", "dcl-one": "[data-testid=detail-stock]" } },
      { name: "buy-now", selector: both(BUY_NOW) },
      { name: "add-to-cart", selector: both(byText("button", "Add to cart")) },
      { name: "rail", selector: { reference: "[data-testid=carousel]", "dcl-one": "[data-testid=detail-rail]" } },
      { name: "rail.title", selector: { reference: "[data-testid=carousel] h2", "dcl-one": "[data-testid=detail-rail] h2" } },
      { name: "rail.card", selector: { reference: "[data-testid=carousel] [data-testid=card]", "dcl-one": "[data-testid=detail-rail] [data-testid=card]" }, max: 5 },
      { name: "rail.card.price", selector: { reference: "[data-testid=carousel] [data-testid=card-price]", "dcl-one": "[data-testid=detail-rail] [data-testid=card-price]" }, max: 5 },
    ],
    masks: [
      {
        selector: { reference: ["[data-testid=item-preview]"], "dcl-one": ["[data-testid=item-preview]"] },
        why: "3D preview: a WebGL render (the reference's wearable-preview iframe, ui3's own renderer) that differs between frames and renderers; its box is still compared structurally",
      },
      WEBGL,
    ],
    regions: FRAME_REGIONS,
    expected: [
      ...FRAME_EXPECTED,
      { match: /^"Preview unavailable for this item"$/, status: "only-dcl-one", why: NO_MODELS },
      { match: /^key:preview$/, why: NO_MODELS, only: ["text"] },
      { match: /(^|\/)detail-play-mode$|^key:tag\.play-mode$|^"PLAY ONCE"$/, status: "only-reference", why: PLAY_MODE },
    ],
    discover: async (page, targets) => {
      const shop = (h: string | null) => (h ? new URL(h, page.url()).pathname.replace(/^(?!\/shop)/, "/shop") : null);
      const link = (contains: string) => page.locator(`xpath=(//main//a[contains(@href,'${contains}')])[1]`).getAttribute("href", { timeout: 2_000 }).catch(() => null);
      // The pinned reference names the item's collection and creator as text, not links. Its collection is the
      // item's contract (/shop/collection/<contract>, a page the reference serves), and its creator is the account
      // that collection page links to as its profile (/shop/items/creator/<address>).
      const collection = shop(await link("/collection/")) ?? collectionOfItem(shop(page.url()) ?? "");
      let creator = shop(await link("/creator/"));
      if (!creator && collection) {
        // Opened in the item capture's context, so its answers are recorded with the same routes and clock. That
        // context's request log is shared too: a host only the collection page asks for is listed under item@.
        // Best-effort: a collection page that fails keeps the collection found above, and the creator already known
        // for that same collection (from the previous set or another viewport), which is still its creator.
        let other: Page | undefined;
        try {
          other = await page.context().newPage();
          await other.goto(new URL(collection, page.url()).href, { waitUntil: "load" });
          // The first profile link in <main> is the collection header's creator, which renders before the cards. A
          // layout that put another profile link first would record that account; the creator page would then
          // show its items, not this collection's.
          const profile = await other.locator("xpath=(//main//a[contains(@href,'/profile/0x')])[1]").getAttribute("href", { timeout: 15_000 });
          creator = profile ? creatorOfProfile(profile) : null;
        } catch {
          creator = targets.collection === collection ? (targets.creator ?? null) : null;
        } finally {
          await other?.close().catch(() => {});
        }
      }
      return { collection, creator };
    },
    unrecorded: [PREVIEW_AVATAR, NEWSLETTER_EMBED],
  },
  {
    id: "cart",
    title: "Cart with the detail page's item, signed out",
    path: "/shop/cart",
    ready: both(byText("*", "Purchase Summary")),
    setup: async (page, _side, targets) => {
      // Add the detail page's item through the page itself, then open the cart.
      await addToCart(page, targets.item ?? "/shop/items");
      await page.goto(new URL("/shop/cart", page.url()).href, { waitUntil: "load" });
    },
    keys: [
      ...FRAME_KEYS,
      { name: "lines", selector: both(panelOf("Fitting room")) },
      { name: "lines.count", selector: both("xpath=//main//*[starts-with(normalize-space(),'Cart:') and not(*)]") },
      { name: "lines.fitting-room", selector: both(byText("button", "Fitting room")) },
      { name: "line.name", selector: { reference: `xpath=(${LINE_CARD}//*[@title and not(*)])[1]`, "dcl-one": "xpath=(//article[contains(@class,'shop-cartline')])[1]//*[contains(@class,'shop-cartline__name')]" } },
      { name: "line.price", selector: { reference: `xpath=(${LINE_CARD}//span[normalize-space(text())!='' and translate(normalize-space(text()),'0123456789,.K','')=''])[last()]/..`, "dcl-one": "xpath=(//article[contains(@class,'shop-cartline')])[1]//*[contains(@class,'shop-cartline__price')]" } },
      { name: "lines.clear", selector: both(byText("*", "Clear cart")) },
      { name: "summary", selector: both(panelOf("Purchase Summary")) },
      { name: "summary.title", selector: both(byText("*", "Purchase Summary")) },
      { name: "summary.total", selector: both("xpath=//main//*[starts-with(normalize-space(),'Total:') and not(*)]/..") },
      { name: "summary.checkout", selector: both(byText("button", "Checkout")) },
      { name: "rail", selector: both("[data-testid=carousel]") },
      { name: "rail.card", selector: both("[data-testid=carousel] [data-testid=card]"), max: 5 },
    ],
    masks: [WEBGL],
    regions: FRAME_REGIONS,
    expected: FRAME_EXPECTED,
    unrecorded: [PREVIEW_AVATAR, NEWSLETTER_EMBED],
  },
  {
    id: "item-added",
    title: "Item detail right after Add to cart (the reference opens its cart drawer)",
    path: (t) => t.item ?? "/shop/items",
    // After the click: the reference stays on the item with its drawer open; dcl-one may have moved on.
    ready: both("[data-testid=subnav]"),
    // The page the path opened: click its Add to cart where a buyer would.
    setup: async (page) => {
      await addToCart(page);
    },
    // The drawer is fixed: one screen holds it, and a scrolled capture would stitch it once per tile.
    maxHeight: 800,
    keys: [
      ...FRAME_KEYS,
      { name: "drawer.title", selector: both(inDrawer("*", "Shopping Cart")) },
      { name: "drawer.added", selector: both(inDrawer("*", "1 Item successfully added")) },
      { name: "drawer.total", selector: both(inDrawer("*", "Total:")) },
      { name: "drawer.continue", selector: both(inDrawer("button", "Continue shopping")) },
      { name: "drawer.go-to-cart", selector: both(inDrawer("button", "Go to cart")) },
      { name: "subnav.cart", selector: both("[data-testid=subnav-cart]") },
    ],
    masks: [
      { selector: both1("[data-testid=item-preview]"), why: "3D preview: a WebGL render that differs between frames and renderers; its box is still compared structurally" },
      WEBGL,
    ],
    regions: FRAME_REGIONS,
    expected: FRAME_EXPECTED,
    unrecorded: [PREVIEW_AVATAR, NEWSLETTER_EMBED],
  },
  {
    id: "cart-two",
    title: "Cart with two different items, signed out",
    path: "/shop/cart",
    ready: both(byText("*", "Purchase Summary")),
    setup: async (page, _side, targets) => {
      if (!targets.item2) throw new Error("the recording has no second item (record a new set: discover reads it)");
      await addToCart(page, targets.item ?? "/shop/items");
      await addToCart(page, targets.item2, 2);
      await page.goto(new URL("/shop/cart", page.url()).href, { waitUntil: "load" });
    },
    keys: [
      ...FRAME_KEYS,
      { name: "lines.count", selector: both("xpath=//main//*[starts-with(normalize-space(),'Cart:') and not(*)]") },
      { name: "summary", selector: both(panelOf("Purchase Summary")) },
      { name: "summary.total", selector: both("xpath=//main//*[starts-with(normalize-space(),'Total:') and not(*)]/..") },
      { name: "summary.checkout", selector: both(byText("button", "Checkout")) },
      { name: "lines.clear", selector: both(byText("*", "Clear cart")) },
    ],
    masks: [WEBGL],
    regions: FRAME_REGIONS,
    expected: FRAME_EXPECTED,
    unrecorded: [PREVIEW_AVATAR, NEWSLETTER_EMBED],
  },
  {
    id: "cart-empty",
    title: "Empty cart, signed out",
    path: "/shop/cart",
    ready: both("[data-testid=subnav]"),
    keys: [...FRAME_KEYS],
    masks: [WEBGL],
    regions: FRAME_REGIONS,
    expected: FRAME_EXPECTED,
    unrecorded: [NEWSLETTER_EMBED],
  },
];

const byId = (id: string) => {
  const spec = PAGES.find((p) => p.id === id);
  if (!spec) throw new Error(`no page ${id}`);
  return spec;
};

/** A whole page compared on its frame, its pixels and every text run it shows. */
function plain(id: string, title: string, path: PageSpec["path"], extra: Partial<PageSpec> = {}): PageSpec {
  return {
    id,
    title,
    path,
    ready: both("[data-testid=subnav]"),
    keys: [...FRAME_KEYS],
    maxHeight: 3000,
    masks: [WEBGL],
    regions: FRAME_REGIONS,
    expected: FRAME_EXPECTED,
    unrecorded: [PREVIEW_AVATAR, NEWSLETTER_EMBED],
    ...extra,
  };
}

/** A page discovered while recording: none recorded is a refusal, never another page under this id. */
const target = (key: string) => (t: Record<string, string>) => {
  if (!t[key]) throw new Error(`the recording has no ${key} target (record a new set: discovery reads it)`);
  return t[key];
};

PAGES.push(
  plain("collection", "A collection (the detail page's)", target("collection")),
  plain("creator", "A creator's items (the detail page's)", target("creator")),
  plain("outfit", "An outfit (Buy the Look, the home's first)", target("outfit"), {
    masks: [
      {
        selector: { reference: ["[data-testid=outfit-detail-preview]"], "dcl-one": ["[data-testid=outfit-detail-preview]"] },
        why: "the outfit's 3D preview: the reference's wearable-preview iframe and ui3's own renderer draw different frames (its models are not recorded); its box is still compared structurally",
      },
      WEBGL,
    ],
  }),
  plain("credits", "Buy credits", "/shop/credits"),
  plain("not-found", "A path the Shop does not have", "/shop/no-such-page"),
);

/** The cart's checkout dialog (upstream's CartCheckoutModal, labelled for its whole flow). */
const CART_CHECKOUT = "[role=dialog][aria-label='Check out your cart']";

const SIGNED_IN_ONLY: PageSpec[] = [
  plain("my-items", "My Items", "/shop/my-items"),
  plain("my-favorites", "My Favorites", "/shop/my-favorites"),
  plain("activity", "Activity", "/shop/activity"),
  plain("my-store", "My Store", "/shop/my-store"),
  {
    id: "cart-checkout",
    title: "Checkout from the cart, short on Credits (the reference opens its CartCheckoutModal on the packs)",
    path: "/shop/cart",
    // The test wallet holds no Credits, so Checkout opens the packs that finish the purchase.
    ready: both(CART_CHECKOUT),
    setup: async (page, _side, targets) => {
      await addToCart(page, targets.item ?? "/shop/items");
      await page.goto(new URL("/shop/cart", page.url()).href, { waitUntil: "load" });
      const checkout = page.locator(byText("button", "Checkout"));
      await checkout.first().waitFor({ state: "visible", timeout: 20_000 });
      await waitUntil(async () => (await checkout.first().isEnabled()) || undefined, { timeoutMs: 20_000, description: "Checkout enabled once the cart is priced" });
      await checkout.first().click();
    },
    // The dialog is fixed over the page: one screen holds it.
    maxHeight: 800,
    keys: [
      ...FRAME_KEYS,
      { name: "dialog", selector: both(CART_CHECKOUT) },
      { name: "dialog.title", selector: both(`xpath=//*[@role='dialog']//h2`) },
      { name: "dialog.packs", selector: both(`${CART_CHECKOUT} [data-testid=credit-pack]`), max: 4 },
    ],
    masks: [WEBGL],
    regions: FRAME_REGIONS,
    expected: FRAME_EXPECTED,
    unrecorded: [PREVIEW_AVATAR, NEWSLETTER_EMBED],
  },
  {
    id: "item-buy",
    title: "Buy now on the item detail (the reference opens its BuyModal over the page)",
    path: (t) => t.item ?? "/shop/items",
    ready: both("[data-testid=item-price]"),
    setup: async (page) => {
      await buyNow(page);
    },
    // The dialog is fixed over the page: one screen holds it.
    maxHeight: 800,
    keys: [
      ...FRAME_KEYS,
      { name: "dialog", selector: both("[data-testid=buy-modal]") },
      { name: "dialog.balance", selector: both(inDrawer("*", "My Credits Balance:")) },
      { name: "dialog.insufficient", selector: both(inDrawer("*", "Insufficient Funds.")) },
      { name: "dialog.packs", selector: both("[data-testid=buy-modal] [data-testid=credit-packs]") },
      { name: "dialog.pack", selector: both("[data-testid=buy-modal] [data-testid=credit-pack]"), max: 4 },
      { name: "dialog.total", selector: both("[data-testid=buy-modal] [data-testid=topup-total-credits]") },
      { name: "dialog.cancel", selector: both(inDrawer("button", "Cancel")) },
      { name: "dialog.buy", selector: both(inDrawer("button", "Buy")) },
    ],
    masks: [
      { selector: both1("[data-testid=item-preview]"), why: "3D preview: a WebGL render that differs between frames and renderers; its box is still compared structurally" },
      WEBGL,
    ],
    regions: FRAME_REGIONS,
    expected: FRAME_EXPECTED,
    unrecorded: [PREVIEW_AVATAR, NEWSLETTER_EMBED],
  },
];

PAGES.push(
  ...SIGNED_IN_ONLY.map((spec) => signedIn(spec, `${spec.title}, signed in as the test wallet`)),
  signedIn(byId("credits"), "Buy credits, signed in as the test wallet"),
  signedIn(byId("overview"), "Overview, signed in as the test wallet"),
  signedIn(byId("item"), "Item detail, signed in as the test wallet"),
  signedIn(byId("cart"), "Cart with the detail page's item, signed in as the test wallet"),
  signedIn(byId("cart-two"), "Cart with two different items, signed in as the test wallet"),
);

PAGES.push(...SITES_PAGES);
