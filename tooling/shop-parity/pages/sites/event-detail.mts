// An event's page: the reference's /events/event?id=<id> (a legacy deep link that redirects to /events?id=<id>, the
// events page with the EventDetailModal open), dcl-one's /whats-on/<id> (the What's On page with the same dialog).
// The event is the first card of the reference's Upcoming section, found while recording the events page (events.mts).
import type { Page } from "playwright-core";
import { both, signedIn, type KeyElement, type PageSpec } from "../../lib/spec.mts";
import { READY_TIMEOUT_MS } from "../../lib/spec.mts";
import { SITES_UNRECORDED, sitesFrameRules, UP } from "./frame.mts";

/** The recording's target holding the event's id. */
export const UPCOMING_EVENT = "upcomingEvent";

/** The reference's Upcoming section on /events (sites: components/events/Upcoming), its cards a div with role=button. */
const UPCOMING_CARD = "xpath=(//main//h5[normalize-space()='Upcoming']/ancestor::section[1]//div[@role='button'])[1]";

type ListedEvent = { id: string; name: string };

/**
 * The id of the first Upcoming card on the reference's /events: the card carries no link, so its title is matched
 * against the upcoming list the page read (events?list=upcoming), in a second page of the same context, so its
 * reads are the recording's.
 */
export async function discoverUpcomingEvent(page: Page): Promise<string | null> {
  const other = await page.context().newPage();
  const lists: Promise<ListedEvent[]>[] = [];
  other.on("response", (r) => {
    const url = new URL(r.url());
    if (/\/api\/events$/.test(url.pathname) && url.searchParams.get("list") === "upcoming") {
      lists.push(r.json().then((b: { data?: ListedEvent[] }) => b.data ?? [], () => []));
    }
  });
  try {
    await other.goto(new URL("/events", page.url()).href, { waitUntil: "load" });
    const card = other.locator(UPCOMING_CARD);
    if (!(await card.waitFor({ state: "visible", timeout: READY_TIMEOUT_MS }).then(() => true, () => false))) return null;
    const text = (await card.innerText()).replace(/\s+/g, " ");
    const events = (await Promise.all(lists)).flat();
    return events.find((e) => e.name && text.includes(e.name.replace(/\s+/g, " ").trim()))?.id ?? null;
  } catch {
    return null;
  } finally {
    await other.close().catch(() => {});
  }
}

/** The dialog: MUI's Dialog paper on the reference (role=dialog, aria-labelledby the title); dcl-one ports it. */
const DIALOG = "xpath=//*[@role='dialog' and @aria-labelledby='event-detail-title']";
const inDialog = (tail: string) => `${DIALOG}${tail}`;
const label = (text: string) => inDialog(`//*[self::button or self::a][@aria-label='${text}']`);

const keys: KeyElement[] = [
  { name: "dialog", selector: DIALOG },
  { name: "title", selector: "#event-detail-title" },
  { name: "hero.close", selector: label("Close") },
  { name: "hero.creator", selector: inDialog("//*[starts-with(normalize-space(),'By ') and not(.//*[starts-with(normalize-space(),'By ')])]") },
  { name: "hero.location", selector: inDialog("//button[starts-with(@aria-label,'Jump In –')]") },
  { name: "hero.calendar", selector: label("Add to calendar"), max: 2 },
  { name: "hero.remind", selector: label("Remind me") },
  { name: "hero.copy", selector: label("Copy link") },
  { name: "content.expect", selector: inDialog(`//*[${UP}='WHAT TO EXPECT' and not(./*)]`) },
  { name: "content.schedule", selector: inDialog(`//*[${UP}='SCHEDULE' and not(./*)]`) },
  { name: "content.jump-in", selector: inDialog(`//*[self::a or self::button][${UP}='JUMP IN']`) },
];

/**
 * The page behind the dialog is the events page (reference /events, dcl-one /whats-on): it is not compared
 * here, because its cards show live counts. Both sides remove everything beside the dialog's fixed root, wherever that root
 * is mounted (a body portal on the reference, inside the What's On page on dcl-one): each sibling of the root and of
 * its ancestors, so the root's ancestors stay visible and the harness still sees the dialog. The hidden page's testids
 * go too: the harness numbers a testid among all its copies, hidden or not, so the events page's dozens of MUI icons
 * would renumber the dialog's own.
 *
 * The dialog locks the page's scroll and scrolls its paper, so its lower half (Schedule, the bottom Jump In) lies
 * below the first screen. Both sides lay the dialog out in full, from the top of the page, so the whole dialog is
 * measured: the fixed root becomes absolute at the top with its content's height, the centring container starts at
 * the top, and the paper and every scroll container inside the root take their content's height (their overflow
 * stays, so the paper's rounded corners still clip the hero). The hidden page also leaves the layout (display: none):
 * its height differs by its live counts (the events spec reports that), and it would set this page's height.
 */
async function hidePageBehind(page: Page): Promise<void> {
  await page.locator(DIALOG).waitFor({ state: "visible", timeout: READY_TIMEOUT_MS });
  await page.evaluate((selector) => {
    const paper = document.evaluate(selector, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue as HTMLElement | null;
    let root = paper;
    while (root && getComputedStyle(root).position !== "fixed") root = root.parentElement;
    for (let el = root; el && el !== document.body; el = el.parentElement) {
      for (const sibling of [...(el.parentElement?.children ?? [])]) {
        if (sibling === el || !(sibling instanceof HTMLElement)) continue;
        sibling.style.setProperty("display", "none", "important");
        for (const t of [sibling, ...sibling.querySelectorAll("[data-testid]")]) t.removeAttribute("data-testid");
      }
    }
    if (!root) return;
    for (const el of [document.documentElement, document.body]) el.style.setProperty("overflow", "visible", "important");
    Object.assign(root.style, { position: "absolute", top: "0", bottom: "auto", height: "auto" });
    for (const el of root.querySelectorAll<HTMLElement>("*")) {
      const cs = getComputedStyle(el);
      if (cs.position === "fixed") continue;
      if (el !== paper && el.contains(paper)) Object.assign(el.style, { alignItems: "flex-start", height: "auto" });
      if (el === paper || /auto|scroll/.test(cs.overflowY)) Object.assign(el.style, { maxHeight: "none", height: "auto" });
    }
  }, DIALOG.replace(/^xpath=/, ""));
  await page.evaluate(() => document.fonts.ready);
}

const eventDetail: PageSpec = {
  id: "event-detail",
  title: "Event detail: the first Upcoming event of the events page",
  reference: "sites",
  path: (t) => {
    const id = t[UPCOMING_EVENT];
    if (!id) throw new Error(`the recording has no ${UPCOMING_EVENT} target (record the events page: its discovery reads it)`);
    return { reference: `/events/event?id=${encodeURIComponent(id)}`, "dcl-one": `/whats-on/${encodeURIComponent(id)}` };
  },
  ready: both("#event-detail-title"),
  setup: hidePageBehind,
  keys,
  ...sitesFrameRules(),
  unrecorded: SITES_UNRECORDED,
};

export default [eventDetail, signedIn(eventDetail, "Event detail, signed in as the test wallet")] satisfies PageSpec[];
