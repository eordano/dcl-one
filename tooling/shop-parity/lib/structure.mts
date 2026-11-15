// Structural comparison of two collected pages: elements paired by testid path (or declared key
// name), visible text runs paired by their text. Geometry is compared against the nearest paired
// ancestor, so a shift is reported once, where it starts, not on everything below it.
import type { Box, Collected, Node, Style } from "./collect.mts";

export type StructDiff = {
  id: string;
  kind: "element" | "text" | "key";
  status: "changed" | "only-reference" | "only-dcl-one";
  changes: string[];
  /** Absolute boxes, for the report's overlays. */
  ref?: Box;
  one?: Box;
  /** A declared expected difference this one matches, with its justification. */
  expected?: string;
};

/**
 * A declared deliberate difference: diffs whose id matches. With `only`, a diff is covered only
 * when every one of its changes starts with one of those words (e.g. ["text"]), so a declared
 * text difference never hides a moved or restyled element. With `status`, only diffs of that status
 * are covered (e.g. "only-dcl-one" for an element the reference does not have at all).
 */
export type Expectation = { match: RegExp; why: string; only?: string[]; status?: StructDiff["status"] };

const STYLE_LABEL: Record<keyof Style, string> = {
  fontFamily: "font-family",
  fontSize: "font-size",
  fontWeight: "font-weight",
  lineHeight: "line-height",
  letterSpacing: "letter-spacing",
  color: "color",
  background: "background",
  borderRadius: "border-radius",
  textTransform: "text-transform",
};
const TEXT_STYLES: (keyof Style)[] = ["fontFamily", "fontSize", "fontWeight", "lineHeight", "letterSpacing", "color", "textTransform"];
const ELEMENT_STYLES: (keyof Style)[] = [...TEXT_STYLES, "background", "borderRadius"];

/** rgb(1, 2, 3) and rgba(1, 2, 3, 1) are one colour; a fully transparent colour is "transparent". */
export function normColor(c: string): string {
  const m = c.match(/^rgba?\(([^)]+)\)$/);
  if (!m) return c;
  const [r, g, b, a = "1"] = m[1]!.split(/[\s,/]+/).filter(Boolean);
  if (Number(a) === 0) return "transparent";
  return Number(a) === 1 ? `rgb(${r}, ${g}, ${b})` : `rgba(${r}, ${g}, ${b}, ${a})`;
}

const fmt = (n: number) => (Number.isInteger(n) ? String(n) : n.toFixed(1));

function geometry(r: Node, o: Node, rp: Node | undefined, op: Node | undefined, tol: number): string[] {
  const out: string[] = [];
  const rx = r.box.x - (rp?.box.x ?? 0), ry = r.box.y - (rp?.box.y ?? 0);
  const ox = o.box.x - (op?.box.x ?? 0), oy = o.box.y - (op?.box.y ?? 0);
  const rel = rp && op ? ` (in ${r.parent})` : "";
  if (Math.abs(ry - oy) > tol) out.push(`top${rel} ${fmt(ry)}→${fmt(oy)}`);
  if (Math.abs(rx - ox) > tol) out.push(`left${rel} ${fmt(rx)}→${fmt(ox)}`);
  if (Math.abs(r.box.w - o.box.w) > tol) out.push(`width ${fmt(r.box.w)}→${fmt(o.box.w)}`);
  if (Math.abs(r.box.h - o.box.h) > tol) out.push(`height ${fmt(r.box.h)}→${fmt(o.box.h)}`);
  return out;
}

function styles(r: Style, o: Style, props: (keyof Style)[]): string[] {
  if (!props.length) return [];
  const out: string[] = [];
  for (const p of props) {
    const a = p === "color" || p === "background" ? normColor(r[p]) : r[p];
    const b = p === "color" || p === "background" ? normColor(o[p]) : o[p];
    if (a !== b) out.push(`${STYLE_LABEL[p]} ${a}→${b}`);
  }
  return out;
}

function pairSet(ref: Node[], one: Node[], kind: StructDiff["kind"], tol: number, compareText: boolean, parents?: { ref: Map<string, Node>; one: Map<string, Node> }): StructDiff[] {
  const R = new Map(ref.map((n) => [n.id, n]));
  const O = new Map(one.map((n) => [n.id, n]));
  const PR = parents?.ref ?? R, PO = parents?.one ?? O;
  const out: StructDiff[] = [];
  for (const r of ref) {
    const o = O.get(r.id);
    if (!o) {
      // An element whose collected parent is itself missing on the other side is implied by it.
      if (kind === "element" && r.parent && !O.has(r.parent) && R.has(r.parent)) continue;
      out.push({ id: r.id, kind, status: "only-reference", changes: [], ref: r.box });
      continue;
    }
    const rp = r.parent ? PR.get(r.parent) : undefined;
    const op = o.parent ? PO.get(o.parent) : undefined;
    const paired = rp && op && r.parent === o.parent;
    const props: (keyof Style)[] = kind === "text" ? TEXT_STYLES : r.ownText && o.ownText ? ELEMENT_STYLES : ["background", "borderRadius"];
    const changes = [...geometry(r, o, paired ? rp : undefined, paired ? op : undefined, tol), ...styles(r.style, o.style, props)];
    if (compareText && r.text !== o.text) changes.push(`text ${JSON.stringify(r.text.slice(0, 60))}→${JSON.stringify(o.text.slice(0, 60))}`);
    if (changes.length) out.push({ id: r.id, kind, status: "changed", changes, ref: r.box, one: o.box });
  }
  for (const o of one) {
    if (R.has(o.id)) continue;
    if (kind === "element" && o.parent && !R.has(o.parent) && O.has(o.parent)) continue;
    out.push({ id: o.id, kind, status: "only-dcl-one", changes: [], one: o.box });
  }
  return out;
}

const bare = (seg: string) => seg.replace(/#\d+$/, "");

/**
 * Name testid elements by the testids both sides have: a wrapper only one side adds (dcl-one's
 * page-level testid, a renamed bar) drops out of the path instead of unpairing everything below.
 */
function nameByShared(refNodes: Node[], oneNodes: Node[], refTexts: Node[], oneTexts: Node[]): void {
  const tids = (ns: Node[]) => new Set(ns.flatMap((n) => [...(n.chain ?? []), ...(n.parentChain ?? [])].map(bare)));
  const a = tids([...refNodes, ...refTexts]), b = tids([...oneNodes, ...oneTexts]);
  const shared = new Set([...a].filter((t) => b.has(t)));
  const name = (chain: string[]) => chain.filter((seg) => shared.has(bare(seg))).join("/");
  for (const ns of [refNodes, oneNodes]) {
    const used = new Map<string, number>();
    for (const n of ns) {
      if (!n.chain) continue;
      const own = n.chain.at(-1)!;
      const anc = name(n.chain.slice(0, -1));
      let id = shared.has(bare(own)) ? name(n.chain) : `${anc ? `${anc}/` : ""}${own}`;
      const k = used.get(id) ?? 0;
      used.set(id, k + 1);
      if (k) id = `${id}~${k + 1}`;
      n.id = id;
      n.parent = anc || null;
    }
  }
  for (const t of [...refTexts, ...oneTexts]) t.parent = t.parentChain ? name(t.parentChain) || null : null;
}

const near = (a?: Box, b?: Box) => !!a && !!b && Math.abs(a.x - b.x) <= 1 && Math.abs(a.y - b.y) <= 1;

export function compareStructure(ref: Collected, one: Collected, opts: { tolerance?: number; expected?: Expectation[]; keys?: string[] } = {}): StructDiff[] {
  const tol = opts.tolerance ?? 1;
  const keyRef = ref.nodes.filter((n) => n.id.startsWith("key:"));
  const keyOne = one.nodes.filter((n) => n.id.startsWith("key:"));
  const elRef = ref.nodes.filter((n) => !n.id.startsWith("key:")).map((n) => ({ ...n }));
  const elOne = one.nodes.filter((n) => !n.id.startsWith("key:")).map((n) => ({ ...n }));
  const txRef = ref.texts.map((n) => ({ ...n }));
  const txOne = one.texts.map((n) => ({ ...n }));
  nameByShared(elRef, elOne, txRef, txOne);
  const parents = { ref: new Map(elRef.map((n) => [n.id, n])), one: new Map(elOne.map((n) => [n.id, n])) };
  // A key element is measured inside the smallest paired testid element holding it on both sides.
  const inside = (outer: Box, inner: Box) => outer.x <= inner.x + 0.5 && outer.y <= inner.y + 0.5 && outer.x + outer.w >= inner.x + inner.w - 0.5 && outer.y + outer.h >= inner.y + inner.h - 0.5;
  const area = (b: Box) => b.w * b.h;
  const keysR = keyRef.map((n) => ({ ...n })), keysO = new Map(keyOne.map((n) => [n.id, { ...n }]));
  for (const k of keysR) {
    const o = keysO.get(k.id);
    if (!o) continue;
    let best: Node | undefined;
    for (const c of elRef) {
      const co = parents.one.get(c.id);
      if (!co || !inside(c.box, k.box) || !inside(co.box, o.box) || area(c.box) <= area(k.box) + 1) continue;
      if (!best || area(c.box) < area(best.box)) best = c;
    }
    if (best) k.parent = o.parent = best.id;
  }
  const structural = [...pairSet(keysR, [...keysO.values()], "key", tol, true, parents), ...pairSet(elRef, elOne, "element", tol, false)];
  // A text run on an element already reported (same boxes) adds nothing.
  const texts = pairSet(txRef, txOne, "text", tol, false, parents).filter((t) => !structural.some((d) => d.status === t.status && (t.status === "only-dcl-one" ? near(d.one, t.one) : near(d.ref, t.ref) && (t.status !== "changed" || near(d.one, t.one))) && d.changes.join() === t.changes.join()));
  const diffs = [...structural, ...texts];
  if (ref.height !== one.height) diffs.unshift({ id: "page", kind: "element", status: "changed", changes: [`height ${ref.height}→${one.height}`] });
  for (const d of diffs) {
    const hit = (opts.expected ?? []).find((e) => e.match.test(d.id) && (!e.status || e.status === d.status) && (!e.only || (d.changes.length > 0 && d.changes.every((c) => e.only!.some((w) => c === w || c.startsWith(`${w} `))))));
    if (hit) d.expected = hit.why;
  }
  // A declared key found on neither side compares nothing: report it, so a declaration cannot target nothing silently;
  // no expectation covers it.
  // A key below maxHeight was found, then left out of the comparison: not missing.
  const ids = [...keyRef.map((n) => n.id), ...keyOne.map((n) => n.id), ...(ref.clipped ?? []), ...(one.clipped ?? [])];
  const found = (name: string) => ids.some((id) => id === `key:${name}` || id.startsWith(`key:${name}#`));
  for (const name of opts.keys ?? []) if (!found(name)) diffs.push({ id: `key:${name}`, kind: "key", status: "changed", changes: ["found on neither side"] });
  return diffs.sort((a, b) => (a.ref?.y ?? a.one?.y ?? -1) - (b.ref?.y ?? b.one?.y ?? -1) || (a.ref?.x ?? a.one?.x ?? 0) - (b.ref?.x ?? b.one?.x ?? 0));
}

/**
 * A declared region is a deliberate difference's area: a structural difference lying wholly inside
 * one region (on every side it appears) is that region's difference, with its rule.
 */
export function coverByRegions(diffs: StructDiff[], regions: { why: string }[], ref: Box[][], one: Box[][]): void {
  const inside = (outer: Box, inner: Box) => outer.x <= inner.x + 1 && outer.y <= inner.y + 1 && outer.x + outer.w >= inner.x + inner.w - 1 && outer.y + outer.h >= inner.y + inner.h - 1;
  for (const d of diffs) {
    if (d.expected || (!d.ref && !d.one)) continue;
    const r = regions.findIndex((_, i) => (!d.ref || (ref[i] ?? []).some((b) => inside(b, d.ref!))) && (!d.one || (one[i] ?? []).some((b) => inside(b, d.one!))));
    if (r >= 0) d.expected = regions[r]!.why;
  }
}

/** One readable line per difference, e.g. `card.price: top 412→416, font-weight 600→500`. */
export function line(d: StructDiff): string {
  const where = d.status === "only-reference" ? "only in the reference" : d.status === "only-dcl-one" ? "only in dcl-one" : d.changes.join(", ");
  return `${d.id}: ${where}${d.expected ? `  [expected: ${d.expected}]` : ""}`;
}
