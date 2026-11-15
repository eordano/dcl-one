// Navigation walk over a recording, on both sides: every gated sites page (signed out, and signed in where the
// spec says) on the pinned sites reference and on the dcl-one preview, the same-origin links each side shows,
// each reference link mapped to its dcl-one path (the specs' reference→path pairs, the detail pages,
// docs.decentraland.org/<p> ↔ /docs/<p>, decentraland.org/<p> ↔ /<p>, else the same path), and each pair
// opened as a full document on both sides and compared: the outcome (resolved, not found, error), the final
// path after redirects (mapped), and the first h1 (the title is listed, not compared: the reference SPA
// titles every page alike). A docs link opens on dcl-one only (docs is not a pinned reference) and must
// resolve there. A link into a production app the sites SPA never served (/marketplace, /builder, /create/<p>,
// /auth) opens on dcl-one only too; a link to a production path dcl-one does not host (/dao, /discord)
// counts as external. Links to another Decentraland host are listed as unmapped; links dcl-one shows that no
// reference link maps to are listed per page. Nothing is read from the network; with --fill, each
// request the replay could not answer is read once from production (as record.mts --fill does, signed reads
// signed again, a signed write never sent) and the documents that missed are opened again, until nothing misses.
//
//   node nav-walk.mts [--set <recording>] [--pages a,b] [--viewport desktop|mobile] [--max-links N] [--out DIR] [--fill]
//
// Writes <out>/report.md and visits.json; default out is state/shop-parity/nav/<timestamp>/ under the repository.
// Every link is opened unless --max-links cuts a page's list. Exits 1 when a pair differs, when links were cut,
// or when --fill leaves a document with a request the replay cannot answer.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { BrowserContext, Page } from "playwright-core";
import { defaultSet } from "./capture.mts";
import { launch, sideContext, VIEWPORTS } from "./lib/browser.mts";
import { settle } from "./lib/capture.mts";
import { arg } from "./lib/env.mts";
import { NAV, RECORDINGS, referenceDist, referencePin } from "./lib/paths.mts";
import { fillMisses, Recording } from "./lib/recording.mts";
import { routeSide } from "./lib/route.mts";
import { SEED, startServers } from "./lib/run.mts";
import { runnable, signIn, testWalletSigner, walletKey } from "./lib/session.mts";
import { pagePath, referenceOf, type PageSpec, type PerSide, type Side } from "./lib/spec.mts";
import { PAGES } from "./pages.mts";
import { interOnReference } from "./pages/sites/frame.mts";

const set = arg("--set") ?? defaultSet();
const only = arg("--pages")?.split(",");
const specs = runnable(PAGES.filter((p) => referenceOf(p) === "sites" && (!only || only.includes(p.id))));
const vp = VIEWPORTS.find((v) => v.name === (arg("--viewport") ?? "desktop"));
if (!vp) throw new Error(`--viewport must be one of ${VIEWPORTS.map((v) => v.name).join(", ")}`);
const maxLinks = Number(arg("--max-links") ?? Infinity);
if (Number.isNaN(maxLinks) || maxLinks < 1) throw new Error("--max-links must be a positive number");
/** A bound on the fill loop, not a measurement: a document whose requests carry a changing value would fill forever. */
const FILL_ROUNDS = 4;
// Bounds, not measurements. A replay answers from disk, so a document that loads for longer has failed. A page with
// no spec has no ready element: the walk gives its requests this long to stop, then reads the page as it is.
const LOAD_TIMEOUT_MS = 45_000;
const IDLE_TIMEOUT_MS = 8_000;
/** One answer of dcl-one's own server to an outside path: a local redirect, with no page to build. */
const OUTSIDE_TIMEOUT_MS = 10_000;
const fill = process.argv.includes("--fill");
const out = arg("--out") ?? join(NAV, new Date().toISOString().replace(/[:.]/g, "-"));
const recording = new Recording(join(RECORDINGS, set));
if (!recording.manifest) throw new Error(`no recording ${set} under ${RECORDINGS}`);
const now = Date.parse(recording.manifest.recordedAt);
const targets = recording.manifest.targets;

type Lane = "signed-out" | "signed-in";
const SIDES = ["reference", "dcl-one"] as const;
/** The reference's production hosts, whose absolute links are its own pages. */
const SITES_HOSTS = new Set(["decentraland.org", "www.decentraland.org"]);
const DOCS_HOST = "docs.decentraland.org";
const NOISE = [/favicon/i, /wearable-preview/i, /net::ERR_(ABORTED|FAILED|BLOCKED_BY_CLIENT)/, /Failed to load resource/i, /the parity replay answers/];
const NOT_FOUND = /\b(not found|page not found|404)\b/i;

/** `oneSided`: why the reference side is not opened (the harness serves no reference for the link's host). */
type Mapped = { path: string; rule: string; oneSided?: string };
const DOCS_ONE_SIDED = `${DOCS_HOST} is not a pinned reference: dcl-one only`;
type Rule = { name: string; why: string; map: (path: string, query: URLSearchParams) => string | undefined };

/** The specs' reference→dcl-one path pairs that differ, exact. */
const SPEC_PAIRS = new Map<string, { path: string; spec: string }>();
for (const spec of specs) {
  const reference = pagePath(spec, targets, "reference");
  const dclOne = pagePath(spec, targets, "dcl-one");
  if (reference !== dclOne && !SPEC_PAIRS.has(reference)) SPEC_PAIRS.set(reference, { path: dclOne, spec: spec.id });
}

/** Production paths dcl-one does not host: a reference link to one is external, as dcl-one's own chrome links them. With a `target`, dcl-one answers the path with a redirect there, which the walk checks over its own server. */
const OUTSIDE: { path: string; why: string; target?: string }[] = [
  { path: "/dao", why: "decentraland.org/dao is the DAO app, outside dcl-one's scope; dcl-one's chrome does not link it" },
  { path: "/discord", target: "https://dcl.gg/discord", why: "the reference's /discord is a captcha gate before the Discord invite (pages/discord/DiscordPage); dcl-one redirects to the invite itself (routes/discord.tsx), as its create page links it (features/create-landing/content.ts)" },
];
/** Production apps under decentraland.org that the pinned sites SPA never served (it answers its 404 page): dcl-one absorbs them, so the link opens on dcl-one only and must resolve there. */
const PRODUCTION_APPS: { prefix: RegExp; why: string }[] = [
  { prefix: /^\/marketplace(\/|$)/, why: "the Marketplace app; dcl-one's Shop (apps/sites marketplace.$.tsx redirects into /shop)" },
  { prefix: /^\/builder(\/|$)/, why: "the Builder app; dcl-one's /create" },
  { prefix: /^\/create\/./, why: "the Builder app's pages under /create (the sites SPA serves /create alone); dcl-one's /create/<section>" },
  { prefix: /^\/auth(\/|$)/, why: "the auth app; dcl-one's own /auth routes (apps/sites auth.login.tsx)" },
];
const productionApp = (path: string) => PRODUCTION_APPS.find((a) => a.prefix.test(path));

const RULES: Rule[] = [
  { name: "spec pair", why: "the page's spec names its path on each side (lib/spec.mts pagePath)", map: (path, q) => SPEC_PAIRS.get(withQuery(path, q))?.path },
  { name: "content policy", why: "the node's content server owns /content/* (lib/services.mts PEER_PREFIXES, as `catalyst serve` mounts it), so dcl-one serves the Content Policy at /content-policy (apps/sites $slug.tsx LEGAL_ALIAS)", map: (path, q) => (path === "/content" ? withQuery("/content-policy", q) : undefined) },
  { name: "event detail", why: "the reference's /events/event?id=<id> (a legacy deep link that it redirects to /events?id=<id>) is dcl-one's /whats-on/<id> (pages/sites/event-detail.mts)", map: (path, q) => ((path === "/events/event" || path === "/events") && q.get("id") ? `/whats-on/${encodeURIComponent(q.get("id")!)}` : undefined) },
  { name: "events", why: "the reference's /events is dcl-one's /whats-on (pages/sites/events.mts)", map: (path, q) => (path === "/events" ? withQuery("/whats-on", q) : undefined) },
  { name: "place detail", why: "/places/place/<x,y> and /places/world/<name> are the same path on both sides; dcl-one redirects each to /places/<id> (pages/sites/place-live.mts, place-world.mts)", map: (path, q) => (/^\/places\/(place|world)\/[^/]+\/?$/.test(path) ? withQuery(path, q) : undefined) },
];
/** What `mapPath` answers when no rule maps the path. */
const SAME_PATH = { name: "same path", why: "a path with no rule is the same on both sides" };
/** The rules by host: `mapReference` applies `docs` and `sites`, and the reference's servers apply `shop`. */
const HOST_RULES = {
  shop: { name: "shop reference", why: "a /shop path on the reference is the pinned Shop UI (reference.json), served by its own static server, as production serves decentraland.org/shop" },
  docs: { name: "docs host", why: `https://${DOCS_HOST}/<p> is dcl-one's /docs/<p> (apps/sites docs.$.tsx); ${DOCS_ONE_SIDED}` },
  sites: { name: "sites host", why: "https://decentraland.org/<p> is the reference's own /<p>, mapped by the path rules" },
};
/** A final path dcl-one reaches by its own redirect from the mapped reference path. */
const REDIRECTS = [
  { reference: /^\/places\/(place|world)\/[^/]+$/, "dcl-one": /^\/places\/[^/]+$/, why: "dcl-one redirects a position or world path to the place's /places/<id>" },
  { reference: /^\/shop\/items\?category=names$/, "dcl-one": /^\/shop\/names$/, why: "dcl-one's Shop browses NAMES on its own page and sends the items category there (apps/ui3/src/shop/discovery/pages/Items.tsx)" },
];

function withQuery(path: string, q: URLSearchParams): string {
  const s = q.toString();
  return s ? `${path}?${s}` : path;
}

/** A URL's path and query relative to its origin, without a hash, one trailing slash removed. */
function relative(url: URL): string {
  const path = url.pathname.length > 1 ? url.pathname.replace(/\/+$/, "") : url.pathname;
  return withQuery(path, url.searchParams);
}

function mapPath(path: string, query: URLSearchParams): Mapped {
  for (const rule of RULES) {
    const mapped = rule.map(path, query);
    if (mapped !== undefined) return { path: mapped, rule: rule.name };
  }
  return { path: withQuery(path, query), rule: SAME_PATH.name };
}

/** A reference link's dcl-one path, or why it has none (`host` names another Decentraland host; external links are counted). */
function mapReference(href: string, origin: string): Mapped | { host: string } | { external: true; outside?: string } | null {
  let url: URL;
  try {
    url = new URL(href);
  } catch {
    return null;
  }
  if (!/^https?:$/.test(url.protocol)) return null;
  const path = url.pathname.length > 1 ? url.pathname.replace(/\/+$/, "") : url.pathname;
  if (url.hostname === DOCS_HOST) return { path: withQuery(`/docs${path === "/" ? "" : path}`, url.searchParams), rule: HOST_RULES.docs.name, oneSided: DOCS_ONE_SIDED };
  if (url.origin === origin || SITES_HOSTS.has(url.hostname)) {
    if (OUTSIDE.some((o) => o.path === path)) return { external: true, outside: path };
    const host = url.origin === origin ? "" : `${HOST_RULES.sites.name}, `;
    const m = mapPath(path, url.searchParams);
    const app = productionApp(path);
    if (app) return { path: m.path, rule: `${host}production app`, oneSided: `${app.why}: the sites reference never served it, dcl-one only` };
    return host ? { path: m.path, rule: `${host}${m.rule}` } : m;
  }
  if (url.hostname.endsWith(".decentraland.org")) return { host: url.hostname };
  return { external: true };
}

type Visit = { side: Side; url: string; status: number | null; final: string; title: string; h1: string; notFound: boolean; outcome: "ok" | "not-found" | "error"; errors: string[]; unrecorded: string[]; ms: number };
/** `visits.reference` is absent for a one-sided pair (`oneSided` says why); dcl-one is then expected to resolve the link. */
type Pair = { lane: Lane; from: string; reference: string; dclOne: string; rule: string; oneSided?: string; shownOnDclOne: boolean; visits: { reference?: Visit; "dcl-one": Visit }; differences: string[] };
type PageWalk = { page: string; lane: Lane; paths: PerSide<string>; start: PerSide<Visit>; links: PerSide<number>; external: number; dclOneOnly: string[]; pairs: string[] };
/** dcl-one's answer to an outside path a reference page linked: the redirect its `target` names, or what came back instead. */
type OutsideCheck = { path: string; target: string; status: number | null; location: string | null; ok: boolean };
const outsideSeen = new Set<string>();
/** Ask dcl-one's own server for each outside path a reference page linked, without following the redirect. */
async function checkOutside(): Promise<OutsideCheck[]> {
  const checks: OutsideCheck[] = [];
  for (const o of OUTSIDE) {
    if (!o.target || !outsideSeen.has(o.path)) continue;
    const res = await fetch(`${origins["dcl-one"]}${o.path}`, { redirect: "manual", signal: AbortSignal.timeout(OUTSIDE_TIMEOUT_MS) }).catch(() => null);
    const location = res?.headers.get("location") ?? null;
    // Only the status and the Location are read: the body is dropped, so its connection is free.
    await res?.body?.cancel().catch(() => {});
    checks.push({ path: o.path, target: o.target, status: res?.status ?? null, location, ok: res !== null && res.status >= 300 && res.status < 400 && location === o.target });
  }
  return checks;
}

const pairs = new Map<string, Pair>();
const pages: PageWalk[] = [];
/** Links --max-links left unopened, over every page. */
let linksCut = 0;
const unmapped = new Map<string, { count: number; pages: Set<string>; example: string }>();
const sitesStatic = referenceDist(referencePin("sites"));

const servers = await startServers(recording, now, ["sites", "shop"]);
/** The reference's /shop paths are the Shop UI's static server; every other reference path the sites SPA's. */
const origins = { reference: servers.origin("reference", "sites"), shop: servers.origin("reference", "shop"), "dcl-one": servers.origin("dcl-one", "sites") };
const isShop = (path: string) => /^\/shop(\/|\?|$)/.test(path);
const originOf = (side: Side, path: string) => (side === "reference" && isShop(path) ? origins.shop : origins[side]);
const { browser } = await launch();
const contexts = new Map<string, BrowserContext>();

/** One context per lane, side and origin for the whole walk: the session, the recording's routes, the frame's fonts. */
async function contextFor(lane: Lane, side: Side, origin: string): Promise<BrowserContext> {
  const key = `${lane} ${side} ${origin}`;
  let context = contexts.get(key);
  if (context) return context;
  context = await sideContext(browser, vp!, { now, seed: SEED });
  if (lane === "signed-in") await signIn(context, walletKey(), now);
  await interOnReference(context, side);
  await routeSide(context, recording, side, origin, origin === origins.reference ? sitesStatic : undefined);
  contexts.set(key, context);
  return context;
}

async function visit(lane: Lane, side: Side, path: string, spec?: PageSpec): Promise<{ visit: Visit; page: Page }> {
  const origin = originOf(side, path);
  const context = await contextFor(lane, side, origin);
  const page = await context.newPage();
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error" && !NOISE.some((n) => n.test(m.text()))) errors.push(m.text().slice(0, 200));
  });
  page.on("pageerror", (e) => errors.push(`pageerror: ${String(e.message).slice(0, 200)}`));
  const t0 = Date.now();
  const missesBefore = recording.misses.length;
  let status: number | null = null;
  try {
    const res = await page.goto(origin + path, { waitUntil: "load", timeout: LOAD_TIMEOUT_MS });
    status = res?.status() ?? null;
    if (spec) {
      // As a capture does (lib/capture.mts captureIn): the page's interactions first, then its ready element.
      if (spec.setup) {
        await settle(page);
        await spec.setup(page, side, targets);
      }
      await settle(page, spec.ready[side]);
    } else await page.waitForLoadState("networkidle", { timeout: IDLE_TIMEOUT_MS }).catch(() => {});
  } catch (e) {
    errors.push(`navigation: ${String((e as Error).message).split("\n")[0].slice(0, 200)}`);
  }
  const title = await page.title().catch(() => "");
  const [h1, notFound] = await page
    .evaluate((pattern: string) => {
      const h1 = [...document.querySelectorAll("h1")].map((h) => h.innerText.replace(/\s+/g, " ").trim()).find((t) => t) ?? "";
      return [h1, new RegExp(pattern, "i").test(document.body?.innerText.slice(0, 4000) ?? "")] as const;
    }, NOT_FOUND.source)
    .catch(() => ["", false] as const);
  const unrecorded = [...new Set(recording.misses.slice(missesBefore).map((m) => `${m.method} ${m.service}${m.path}${m.query ? `?${m.query}` : ""}`))];
  const outcome: Visit["outcome"] = status === null || status >= 500 || errors.some((e) => e.startsWith("navigation:")) ? "error" : status >= 400 || notFound ? "not-found" : "ok";
  let final = path;
  try {
    final = relative(new URL(page.url()));
  } catch {
    // about:blank after a failed navigation: the path stays
  }
  return { visit: { side, url: path, status, final, title, h1, notFound: notFound || NOT_FOUND.test(title), outcome, errors, unrecorded, ms: Date.now() - t0 }, page };
}

const normal = (s: string) => s.toLowerCase().replace(/\s+/g, " ").trim();

function compare(pair: Pair): string[] {
  const r = pair.visits.reference;
  const d = pair.visits["dcl-one"];
  const out: string[] = [];
  if (!r) return d.outcome === "ok" ? [] : [`dcl-one ${d.outcome} (${d.status ?? "—"}) for a link the reference shows (${pair.oneSided})`];
  if (r.outcome !== d.outcome) out.push(`outcome ${r.outcome} (${r.status ?? "—"}) → ${d.outcome} (${d.status ?? "—"})`);
  const finalRef = mapReference(origins.reference + r.final, origins.reference);
  const expected = finalRef && "path" in finalRef ? finalRef.path : r.final;
  const redirected = REDIRECTS.some((x) => x.reference.test(expected) && x["dcl-one"].test(d.final));
  if (expected !== d.final && !redirected) out.push(`final ${expected} → ${d.final}`);
  if (r.h1 && d.h1 && normal(r.h1) !== normal(d.h1)) out.push(`h1 "${r.h1}" → "${d.h1}"`);
  return out;
}

async function openPair(pair: Pair): Promise<void> {
  for (const side of SIDES) {
    if (side === "reference" && pair.oneSided) continue;
    const { visit: v, page } = await visit(pair.lane, side, side === "reference" ? pair.reference : pair.dclOne);
    pair.visits[side] = v;
    await page.close().catch(() => {});
  }
  pair.differences = compare(pair);
}

const pairKey = (lane: Lane, reference: string, dclOne: string) => `${lane} ${reference} -> ${dclOne}`;

async function walkPage(spec: PageSpec): Promise<void> {
  const lane: Lane = spec.signedIn ? "signed-in" : "signed-out";
  const paths: PerSide<string> = { reference: pagePath(spec, targets, "reference"), "dcl-one": pagePath(spec, targets, "dcl-one") };
  const start = {} as PerSide<Visit>;
  const hrefs = {} as PerSide<string[]>;
  for (const side of SIDES) {
    const { visit: v, page } = await visit(lane, side, paths[side], spec);
    start[side] = v;
    hrefs[side] = v.outcome === "error" ? [] : await page.evaluate(() => [...document.querySelectorAll("a[href]")].map((a) => (a as HTMLAnchorElement).href)).catch(() => []);
    await page.close().catch(() => {});
  }
  const dclOneShown = new Set(hrefs["dcl-one"].filter((h) => h.startsWith(origins["dcl-one"])).map((h) => relative(new URL(h))).filter((p) => p !== paths["dcl-one"]));
  const mapped = new Map<string, Mapped>();
  let external = 0;
  for (const href of new Set(hrefs.reference.map((h) => h.replace(/#.*$/, "")))) {
    const m = mapReference(href, origins.reference);
    if (!m) continue;
    if ("external" in m) {
      external++;
      if (m.outside) outsideSeen.add(m.outside);
    } else if ("host" in m) {
      const u = unmapped.get(m.host) ?? { count: 0, pages: new Set<string>(), example: href };
      u.count++;
      u.pages.add(spec.id);
      unmapped.set(m.host, u);
    } else {
      const reference = relative(new URL(href));
      if (reference !== paths.reference && !mapped.has(reference)) mapped.set(reference, m);
    }
  }
  const own: string[] = [];
  linksCut += Math.max(0, mapped.size - maxLinks);
  for (const [reference, m] of [...mapped].slice(0, maxLinks)) {
    const key = pairKey(lane, reference, m.path);
    own.push(key);
    if (pairs.has(key)) continue;
    const pair: Pair = { lane, from: spec.id, reference, dclOne: m.path, rule: m.rule, ...(m.oneSided && { oneSided: m.oneSided }), shownOnDclOne: dclOneShown.has(m.path), visits: {} as Pair["visits"], differences: [] };
    pairs.set(key, pair);
    await openPair(pair);
  }
  const mappedTo = new Set([...mapped.values()].map((m) => m.path));
  const walk: PageWalk = { page: spec.id, lane, paths, start, links: { reference: mapped.size, "dcl-one": dclOneShown.size }, external, dclOneOnly: [...dclOneShown].filter((p) => !mappedTo.has(p)).sort(), pairs: own };
  pages.push(walk);
  const differing = own.filter((k) => pairs.get(k)!.differences.length).length;
  console.log(`${spec.id} [${lane}] ${paths.reference} | ${paths["dcl-one"]}: ${start.reference.outcome} | ${start["dcl-one"].outcome}; ${mapped.size} reference links (${mapped.size > maxLinks ? `${maxLinks} opened, ` : ""}${differing} differ), ${walk.dclOneOnly.length} dcl-one only, ${external} external`);
}

/** The pages and pairs with a request the replay could not answer. */
const missing = (): string[] => [...pages.filter((p) => SIDES.some((s) => p.start[s].unrecorded.length)).map((p) => p.page), ...[...pairs.values()].filter((p) => SIDES.some((s) => p.visits[s]?.unrecorded.length)).map((p) => pairKey(p.lane, p.reference, p.dclOne))];

/** With --fill: read what the replay could not answer, once, and open the documents that missed again. */
async function fillRound(round: number): Promise<boolean> {
  const missed = missing();
  if (!missed.length) return false;
  const live = new Recording(recording.dir, { live: true, signer: testWalletSigner() });
  const { filled, failed } = await fillMisses(recording, live);
  for (const f of failed) console.error(`  could not fill: ${f}`);
  if (failed.length) process.exitCode = 1;
  for (const [k, e] of live.entries) recording.entries.set(k, e);
  for (const [k, v] of live.images) recording.images.set(k, v);
  recording.misses.splice(0);
  console.log(`fill round ${round}: ${filled} filled, ${failed.length} failed; opening ${missed.length} document(s) again`);
  if (!filled) return false;
  for (const p of pages) {
    if (!missed.includes(p.page)) continue;
    const spec = specs.find((s) => s.id === p.page)!;
    for (const side of SIDES) {
      const { visit: v, page } = await visit(p.lane, side, p.paths[side], spec);
      p.start[side] = v;
      await page.close().catch(() => {});
    }
  }
  for (const pair of pairs.values()) if (missed.includes(pairKey(pair.lane, pair.reference, pair.dclOne))) await openPair(pair);
  return true;
}

let outsideChecks: OutsideCheck[] = [];
try {
  for (const spec of specs) await walkPage(spec);
  if (fill) for (let round = 1; round <= FILL_ROUNDS && (await fillRound(round)); round++);
  outsideChecks = await checkOutside();
} finally {
  for (const c of contexts.values()) await c.close().catch(() => {});
  await browser.close();
  await servers.close();
}

mkdirSync(out, { recursive: true });
const all = [...pairs.values()];
const differing = all.filter((p) => p.differences.length);
const lanes = (["signed-out", "signed-in"] as Lane[]).map((lane) => {
  const own = all.filter((p) => p.lane === lane);
  return { lane, pages: pages.filter((p) => p.lane === lane).length, pairs: own.length, equal: own.filter((p) => !p.differences.length).length, differing: own.filter((p) => p.differences.length).length };
});
const unmappedList = [...unmapped].map(([host, u]) => ({ host, count: u.count, pages: [...u.pages], example: u.example })).sort((a, b) => b.count - a.count);
writeFileSync(join(out, "visits.json"), `${JSON.stringify({ recording: set, viewport: vp.name, origins, lanes, rules: [...Object.values(HOST_RULES), ...[...RULES, SAME_PATH].map((r) => ({ name: r.name, why: r.why }))], outside: OUTSIDE, outsideChecks, productionApps: PRODUCTION_APPS.map((a) => ({ prefix: a.prefix.source, why: a.why })), specPairs: [...SPEC_PAIRS].map(([reference, p]) => ({ reference, dclOne: p.path, spec: p.spec })), redirects: REDIRECTS.map((r) => ({ reference: r.reference.source, dclOne: r["dcl-one"].source, why: r.why })), unmapped: unmappedList, pages, pairs: all }, null, 1)}\n`);

const cell = (s: string) => s.replace(/\|/g, "/").replace(/\n/g, " ");
const v = (x: Visit) => `${x.outcome} (${x.status ?? "—"})${x.final !== x.url ? ` → ${x.final}` : ""}`;
const flags = (x: Visit) => [...x.errors, ...x.unrecorded.slice(0, 3).map((u) => `unrecorded ${u}`), ...(x.unrecorded.length > 3 ? [`+${x.unrecorded.length - 3} unrecorded`] : [])].join("; ");
const heading = (x: Visit) => cell(x.h1 || `title: ${x.title}`);
const row = (p: Pair) => {
  const r = p.visits.reference;
  const d = p.visits["dcl-one"];
  return `| ${p.from} | ${p.lane} | ${cell(p.reference)}${p.oneSided ? ` (${cell(p.oneSided)})` : ""} | ${cell(p.dclOne)}${p.shownOnDclOne ? "" : " (not shown)"} | ${r ? v(r) : "—"} | ${v(d)} | ${r ? heading(r) : "—"} | ${heading(d)} | ${cell(p.differences.join("; "))} | ${cell([r ? flags(r) : "", flags(d)].filter(Boolean).join(" ‖ "))} |`;
};
const HEAD = ["| from | lane | reference | dcl-one | reference outcome | dcl-one outcome | reference h1 | dcl-one h1 | differences | flags |", "|---|---|---|---|---|---|---|---|---|---|"];
const unfilled = fill ? missing().length : 0;
const incomplete = [...(linksCut ? [`--max-links ${maxLinks} left ${linksCut} link(s) unopened`] : []), ...(unfilled ? [`${unfilled} document(s) still miss a request after the fill`] : [])];
const lines = [
  `# Navigation walk (${vp.name}, recording ${set})`,
  "",
  `Reference ${origins.reference} (sites ${referencePin("sites").revision.slice(0, 12)}; /shop on ${origins.shop}, shop ${referencePin("shop").revision.slice(0, 12)}), dcl-one ${origins["dcl-one"]}. ${pages.length} pages, ${all.length} pairs opened on both sides: ${all.length - differing.length} equal, ${differing.length} differing; ${unmappedList.reduce((n, u) => n + u.count, 0)} reference links to another Decentraland host unmapped.`,
  "",
  ...(incomplete.length ? [`**Incomplete:** ${incomplete.join("; ")}.`, ""] : []),
  "| lane | pages | pairs | equal | differing |",
  "|---|---|---|---|---|",
  ...lanes.map((l) => `| ${l.lane} | ${l.pages} | ${l.pairs} | ${l.equal} | ${l.differing} |`),
  "",
  "A pair is equal when both sides reach the same outcome (resolved, not found, error), the reference's final path maps to dcl-one's final path (or a declared redirect reaches it), and the first h1 reads the same where both have one. Titles are listed, not compared: the reference SPA titles every page alike.",
  "",
  "## Mapping",
  "",
  "| rule | why |",
  "|---|---|",
  ...[...Object.values(HOST_RULES), ...RULES, SAME_PATH].map((r) => `| ${r.name} | ${cell(r.why)} |`),
  ...REDIRECTS.map((r) => `| redirect \`${r.reference.source}\` → \`${r["dcl-one"].source}\` | ${cell(r.why)} |`),
  ...OUTSIDE.map((o) => `| outside \`${o.path}\` (external${o.target ? `, dcl-one redirects to ${o.target}` : ""}) | ${cell(o.why)} |`),
  ...PRODUCTION_APPS.map((a) => `| production app \`${a.prefix.source}\` (dcl-one only) | ${cell(a.why)} |`),
  "",
  "Spec pairs (reference → dcl-one):",
  "",
  ...[...SPEC_PAIRS].map(([reference, p]) => `- \`${reference}\` → \`${p.path}\` (${p.spec})`),
  "",
  "## Unmapped",
  "",
  unmappedList.length ? "| host | links | pages | example |\n|---|---|---|---|" : "No reference link to another Decentraland host.",
  ...unmappedList.map((u) => `| ${u.host} | ${u.count} | ${u.pages.join(", ")} | ${cell(u.example)} |`),
  "",
  "## Differing pairs",
  "",
  ...(differing.length ? [...HEAD, ...differing.map(row)] : ["None."]),
  "",
  "## Pages",
  "",
  ...pages.flatMap((p) => [
    `### ${p.page} [${p.lane}]`,
    "",
    `Reference \`${p.paths.reference}\`: ${v(p.start.reference)}, h1 "${cell(p.start.reference.h1)}", title "${cell(p.start.reference.title)}"${flags(p.start.reference) ? `; ${cell(flags(p.start.reference))}` : ""}.`,
    `dcl-one \`${p.paths["dcl-one"]}\`: ${v(p.start["dcl-one"])}, h1 "${cell(p.start["dcl-one"].h1)}", title "${cell(p.start["dcl-one"].title)}"${flags(p.start["dcl-one"]) ? `; ${cell(flags(p.start["dcl-one"]))}` : ""}.`,
    `${p.links.reference} mapped reference links (${p.pairs.length} opened, ${p.pairs.filter((k) => pairs.get(k)!.differences.length).length} differ), ${p.links["dcl-one"]} dcl-one links, ${p.external} external.`,
    ...(p.dclOneOnly.length ? ["", `dcl-one only: ${p.dclOneOnly.map((l) => `\`${cell(l)}\``).join(", ")}`] : []),
    "",
  ]),
  ...(outsideChecks.length
    ? [
        "## Outside paths",
        "",
        "dcl-one's own answer to each outside path a reference page linked, asked without following the redirect.",
        "",
        "| path | expected | dcl-one | ok |",
        "|---|---|---|---|",
        ...outsideChecks.map((c) => `| \`${c.path}\` | redirect to ${c.target} | ${c.status ?? "no answer"}${c.location ? ` → ${c.location}` : ""} | ${c.ok ? "yes" : "NO"} |`),
        "",
      ]
    : []),
  "## All pairs",
  "",
  ...HEAD,
  ...all.map(row),
];
writeFileSync(join(out, "report.md"), `${lines.join("\n")}\n`);
const outsideFailed = outsideChecks.filter((c) => !c.ok);
console.log(`\n${all.length} pairs, ${differing.length} differing, ${outsideChecks.length} outside path(s) checked (${outsideFailed.length} wrong), ${unmappedList.length} unmapped host(s) -> ${join(out, "report.md")}`);
for (const why of incomplete) console.error(`incomplete: ${why}`);
if (differing.length || outsideFailed.length || incomplete.length) process.exitCode = 1;
