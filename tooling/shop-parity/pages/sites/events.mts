// What's On: the reference's /events (Live Now, Upcoming, All Events with day tabs and the Host CTA),
// dcl-one's /whats-on. dcl-one renders no <main>, so its elements pair by the page's own classes.
import { signedIn, type KeyElement, type PageSpec, type PerSide } from "../../lib/spec.mts";
import type { Expectation } from "../../lib/structure.mts";
import { discoverUpcomingEvent, UPCOMING_EVENT } from "./event-detail.mts";
import { mainText, SITES_UNRECORDED, sitesFrame, sitesFrameRules, UP } from "./frame.mts";

const ALL_EVENTS = mainText("All Events").reference;

/** The reference's sections of HomePage (sites: pages/events/HomePage.tsx): the innermost <section> holding a heading. */
const refSection = (tag: string, text: string) => `xpath=//main//${tag}[normalize-space()='${text}']/ancestor::section[1]`;
const LIVE = refSection("h5", "Live Now");
const UPCOMING = refSection("h5", "Upcoming");
// The section carries aria-label "All Events" signed in and out (AllExperiences.tsx); its title is an h4 or a tab.
const ALL = "xpath=//main//section[@aria-label='All Events']";
const HOST = refSection("p", "Host an Event");

/** A key under a section: `ref` is an xpath tail below the reference's section, `one` a CSS selector. */
const under = (refSec: string, ref: string, one: string): PerSide<string> => ({ reference: `${refSec}${ref}`, "dcl-one": one });

const keys: KeyElement[] = [
  ...sitesFrame("dtb"),
  { name: "live", selector: { reference: LIVE, "dcl-one": "section.wo-ln" } },
  { name: "live.title", selector: { reference: "xpath=//main//h5[normalize-space()='Live Now']", "dcl-one": "h2.wo-ln__heading" } },
  { name: "live.card", selector: under(LIVE, "//button[contains(@class,'MuiCardActionArea')]", "article.wo-ln__card"), max: 2 },
  { name: "upcoming", selector: { reference: UPCOMING, "dcl-one": "section.wo-up" } },
  { name: "upcoming.title", selector: { reference: "xpath=//main//h5[normalize-space()='Upcoming']", "dcl-one": "h2.wo-up__title" } },
  { name: "upcoming.card", selector: under(UPCOMING, "//div[@role='button']", ".wo-up__grid > .wo-sm || .wo-up__page > .wo-sm"), max: 4 },
  { name: "all", selector: { reference: ALL, "dcl-one": "section.wo-all" } },
  { name: "all.title", selector: { reference: ALL_EVENTS, "dcl-one": ".wo-all__title" } },
  { name: "all.tabs", selector: under(ALL, "//div[@role='navigation']", "nav.wo-all__nav") },
  { name: "all.card", selector: under(ALL, "//*[(@role='listitem' or @role='button') and .//img and not(.//*[@role='listitem' or @role='button'])]", ".wo-col .wo-col__slot || .wo-all__list .wo-sm"), max: 4 },
  { name: "host", selector: { reference: HOST, "dcl-one": "section.wo-host" } },
  { name: "host.title", selector: { reference: "xpath=//main//p[normalize-space()='Host an Event']", "dcl-one": "h3.wo-host__title" } },
  { name: "host.create", selector: { reference: `${HOST}//button[${UP}='CREATE AN EVENT']`, "dcl-one": "section.wo-host a.wo-host__btn:first-of-type" } },
  { name: "host.learn", selector: { reference: `${HOST}//button[${UP}='LEARN MORE']`, "dcl-one": "section.wo-host a.wo-host__btn:last-of-type" } },
];

// On phones the reference's Live Now rail overflows, so it mounts its "next" chevron (sites:
// components/events/LiveNow/LiveNow.tsx, canScrollRight) and hides it below sm (LiveNow.styled.ts ChevronButton,
// display none): a boxless ChevronRightIcon#0 that numbers the visible Upcoming chevron #1. dcl-one renders that
// one chevron alone, unnumbered. Both ids name the same icon at the same box; the ordinal is the only gap. On
// desktop the two live cards fit, no rail chevron mounts, and the Upcoming chevrons pair as ChevronRightIcon.
const UPCOMING_CHEVRON = "the reference numbers its Upcoming chevron after a hidden Live Now rail chevron on phones";
const chevronOrdinal: Expectation[] = [
  { match: /^ChevronRightIcon#1$/, why: UPCOMING_CHEVRON, status: "only-reference" },
  { match: /^ChevronRightIcon$/, why: UPCOMING_CHEVRON, status: "only-dcl-one" },
];

const frame = sitesFrameRules();

const events: PageSpec = {
  id: "events",
  title: "What's On (events)",
  reference: "sites",
  path: { reference: "/events", "dcl-one": "/whats-on" },
  // The reference's AllExperiences heading renders on every visit (an <h4> "All Events" signed out, the
  // "All Events" tab beside "My Events" signed in); the Live Now and Upcoming ones only with data.
  // dcl-one's StWhatSOn marks the same element .wo-all__title: the <h2> signed out, the All Events tab signed in.
  ready: { reference: ALL_EVENTS, "dcl-one": ".wo-all__title" },
  keys,
  ...frame,
  expected: [...frame.expected, ...chevronOrdinal],
  // No masks: both sides replay one recording at one clock, so attendee counts and times do not tick.
  unrecorded: SITES_UNRECORDED,
  discover: async (page) => ({ [UPCOMING_EVENT]: await discoverUpcomingEvent(page) }),
};

export default [events, signedIn(events, "What's On, signed in as the test wallet")] satisfies PageSpec[];
