// What a page of a parity capture declares: shared by pages.mts, the page files under pages/ and the
// library, so a page file never imports the list it is part of.
import type { BrowserContext, Page } from "playwright-core";
import type { ReferenceId } from "./paths.mts";
import type { Expectation } from "./structure.mts";

export type { Expectation };

/**
 * How long a wait for an element a page shows, and for a navigation a page starts, may take. Bounds, not
 * measurements: a replay answers from disk, so a wait that reaches its bound has failed.
 */
export const READY_TIMEOUT_MS = 20_000;
export const NAVIGATION_TIMEOUT_MS = 15_000;

export type Side = "reference" | "dcl-one";
export type PerSide<T> = { reference: T; "dcl-one": T };

/**
 * `absentOn`: the viewport where both sides leave the element out by design (the rule goes in a comment beside it), so
 * finding it on neither side there is not reported; finding it on one side still is.
 */
export type KeyElement = { name: string; selector: string | PerSide<string>; max?: number; absentOn?: "mobile" | "desktop" };
/** What cannot be equal between two renders, in a box both sides have at the same place: a pixel only one side masks still counts. */
export type Mask = { selector: PerSide<string[]>; why: string };
/** A deliberate difference's area: excluded from the pixel count, listed with its rule. */
export type Region = { selector: PerSide<string[]>; why: string };

/** A page's path: one for both sides, or one per side; a function reads targets discovered while recording. */
export type PagePath = string | PerSide<string> | ((targets: Record<string, string>) => string | PerSide<string>);

export type PageSpec = {
  id: string;
  title: string;
  path: PagePath;
  /** Which pinned reference the page is compared with (lib/paths.mts REFERENCES); shop when absent. */
  reference?: ReferenceId;
  ready: PerSide<string>;
  /** Signed in as the test wallet ([`signIn`](session.mts), SHOP_PARITY_WALLET_KEY) on both sides. */
  signedIn?: boolean;
  /** Installed in a side's context before its page loads, as a session is (the frame's Inter faces on the sites reference, [`fonts.mts`](fonts.mts)). */
  init?: (context: BrowserContext, side: Side) => Promise<void>;
  /** Interactions after the first render (both sides do the same thing). */
  setup?: (page: Page, side: Side, targets: Record<string, string>) => Promise<void>;
  /** While recording: read targets later pages need (from the reference). A null target was not found: the
   *  recording drops it, so a page reading it refuses rather than compare the previous set's. */
  discover?: (page: Page, targets: Readonly<Record<string, string>>) => Promise<Record<string, string | null>>;
  /** While recording: the reference path `discover` reads, once, before the page opens, when the targets it
   *  finds are the page's own path (a world from the /places grid); without it, discover reads the page itself. */
  discoverAt?: string;
  keys: KeyElement[];
  masks?: Mask[];
  regions?: Region[];
  /**
   * Compare only the top of the page, in CSS px: an infinitely scrolling list is captured to a depth
   * that never reaches its next-page trigger, so both sides show the same first page.
   */
  maxHeight?: number;
  expected?: Expectation[];
  /** Requests a page makes that the recording cannot hold, matched on `METHOD service path body`, with why. */
  unrecorded?: { match: RegExp; why: string }[];
};

export const both = (s: string): PerSide<string> => ({ reference: s, "dcl-one": s });

export const referenceOf = (spec: Pick<PageSpec, "reference">): ReferenceId => spec.reference ?? "shop";

export function pagePath(spec: Pick<PageSpec, "path">, targets: Record<string, string>, side: Side = "reference"): string {
  const p = typeof spec.path === "function" ? spec.path(targets) : spec.path;
  return typeof p === "string" ? p : p[side];
}

export function pathLabel(spec: Pick<PageSpec, "path">, targets: Record<string, string>): string {
  const reference = pagePath(spec, targets, "reference");
  const dclOne = pagePath(spec, targets, "dcl-one");
  return reference === dclOne ? reference : `${reference} → ${dclOne}`;
}

/**
 * The same page signed in as the test wallet: the frame shows the account instead of the sign-in
 * button. A signed-in page is its signed-out page's path, interactions, masks and rules; only the
 * sign-in key, which a session replaces, is dropped, and discovery stays with the signed-out page.
 */
export function signedIn(spec: PageSpec, title: string): PageSpec {
  const { discover: _discover, discoverAt: _discoverAt, ...rest } = spec;
  return {
    ...rest,
    id: `${spec.id}-signed-in`,
    title,
    signedIn: true,
    keys: spec.keys.filter((k) => k.name !== "site-nav.signin"),
  };
}
