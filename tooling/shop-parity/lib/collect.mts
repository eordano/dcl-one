// What a capture reads from a rendered page for the structural comparison. `collectInPage` runs
// inside the page (serialized by Playwright), so it uses only DOM APIs.

export type Box = { x: number; y: number; w: number; h: number };
export type Style = {
  fontFamily: string;
  fontSize: string;
  fontWeight: string;
  lineHeight: string;
  letterSpacing: string;
  color: string;
  background: string;
  borderRadius: string;
  textTransform: string;
};
export type Node = {
  /** Paired across sides: a testid chain (see structure.mts), "key:<name>#i", or a quoted text. */
  id: string;
  /** testid chain from the outermost testid ancestor to this element: "tid" or "tid#n" each. */
  chain?: string[];
  /** For a text run: the chain of its nearest testid ancestor. */
  parentChain?: string[];
  parent: string | null;
  box: Box;
  style: Style;
  text: string;
  /** Whether the element has visible text of its own (its colour and font then matter). */
  ownText: boolean;
};
/** `clipped`: the key nodes found below the spec's maxHeight, which the comparison leaves out. */
export type Collected = { url: string; width: number; height: number; nodes: Node[]; texts: Node[]; masks: Box[]; clipped?: string[] };

export type CollectOptions = { keys: { name: string; selector: string; max: number }[]; masks: string[] };

export function collectInPage(opts: CollectOptions): Collected {
  const vw = document.documentElement.clientWidth;
  const sx = window.scrollX, sy = window.scrollY;
  const hiddenCache = new Map<Element, boolean>();
  /** display:none, visibility, zero opacity, or the screen-reader-only clip. */
  const hidden = (el: Element): boolean => {
    if (hiddenCache.has(el)) return hiddenCache.get(el)!;
    const cs = getComputedStyle(el);
    const r = el.getBoundingClientRect();
    // The screen-reader-only pattern: a 1px box that clips its content. A zero-size wrapper that does
    // not clip (a fixed bar's anchor) hides nothing.
    const srOnly = (r.width <= 1 && r.height <= 1 && cs.overflow !== "visible") || /rect\(0(px)?,? 0(px)?,? 0(px)?,? 0(px)?\)/.test(cs.clip) || /inset\(50%\)/.test(cs.clipPath);
    const own = cs.display === "none" || cs.visibility === "hidden" || Number(cs.opacity) === 0 || srOnly;
    const result = own || (el.parentElement ? hidden(el.parentElement) : false);
    hiddenCache.set(el, result);
    return result;
  };
  const clipped = (r: DOMRect, el: Element): boolean => {
    // A fixed element escapes its ancestors' overflow clip.
    for (let p = el.parentElement, c: Element = el; p && p !== document.body; c = p, p = p.parentElement) {
      if (getComputedStyle(c).position === "fixed") return false;
      const ps = getComputedStyle(p);
      if (ps.overflowX !== "visible" || ps.overflowY !== "visible") {
        const pr = p.getBoundingClientRect();
        if (r.right <= pr.left + 1 || r.left >= pr.right - 1 || r.bottom <= pr.top + 1 || r.top >= pr.bottom - 1) return true;
      }
    }
    return false;
  };
  const visible = (el: Element): DOMRect | null => {
    const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) return null;
    if (r.right + sx <= 0 || r.left + sx >= vw) return null;
    if (hidden(el) || clipped(r, el)) return null;
    return r;
  };
  const box = (r: { left: number; top: number; width: number; height: number }): Box => ({
    x: Math.round((r.left + sx) * 10) / 10,
    y: Math.round((r.top + sy) * 10) / 10,
    w: Math.round(r.width * 10) / 10,
    h: Math.round(r.height * 10) / 10,
  });
  const style = (el: Element): Style => {
    const cs = getComputedStyle(el);
    return {
      fontFamily: cs.fontFamily.split(",")[0]!.trim().replace(/^["']|["']$/g, ""),
      fontSize: cs.fontSize,
      fontWeight: cs.fontWeight,
      lineHeight: cs.lineHeight,
      letterSpacing: cs.letterSpacing,
      color: cs.color,
      background: cs.backgroundColor,
      borderRadius: cs.borderRadius,
      textTransform: cs.textTransform,
    };
  };
  const shown = (t: Text): string => {
    const el = t.parentElement!;
    const s = (t.textContent ?? "").replace(/\s+/g, " ");
    const tt = getComputedStyle(el).textTransform;
    return tt === "uppercase" ? s.toUpperCase() : tt === "lowercase" ? s.toLowerCase() : s;
  };
  /** The text a sighted user reads inside `el`: visible text nodes only. */
  const textOf = (el: Element): string => {
    const w = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    const parts: string[] = [];
    for (let t = w.nextNode() as Text | null; t; t = w.nextNode() as Text | null) {
      if (!t.textContent?.trim() || !t.parentElement || hidden(t.parentElement)) continue;
      parts.push(shown(t));
    }
    return parts.join(" ").replace(/\s+/g, " ").trim().slice(0, 120);
  };
  const ownTextNodes = (el: Element): Text[] => [...el.childNodes].filter((c): c is Text => c.nodeType === 3 && !!c.textContent?.trim());

  // testid chains: each testid indexed among same-testid elements under its nearest testid ancestor.
  const chains = new Map<Element, string[]>();
  const nodes: Node[] = [];
  const all = [...document.querySelectorAll("[data-testid]")];
  const testidParent = (el: Element): Element | null => {
    let anc: Element | null = el.parentElement;
    while (anc && !anc.hasAttribute("data-testid")) anc = anc.parentElement;
    return anc;
  };
  const totals = new Map<Element | null, Map<string, number>>();
  for (const el of all) {
    const scope = testidParent(el);
    const tid = el.getAttribute("data-testid")!;
    const m = totals.get(scope) ?? new Map<string, number>();
    m.set(tid, (m.get(tid) ?? 0) + 1);
    totals.set(scope, m);
  }
  const seenIn = new Map<Element | null, Map<string, number>>();
  for (const el of all) {
    const anc = testidParent(el);
    const tid = el.getAttribute("data-testid")!;
    const m = seenIn.get(anc) ?? new Map<string, number>();
    const n = m.get(tid) ?? 0;
    m.set(tid, n + 1);
    seenIn.set(anc, m);
    const many = (totals.get(anc)?.get(tid) ?? 0) > 1;
    const chain = [...(anc ? chains.get(anc) ?? [] : []), many ? `${tid}#${n}` : tid];
    chains.set(el, chain);
    const r = visible(el);
    if (!r) continue;
    nodes.push({ id: chain.join("/"), chain, parent: null, box: box(r), style: style(el), text: textOf(el), ownText: ownTextNodes(el).length > 0 });
  }
  /** CSS, or `xpath=` for what CSS cannot say (a section by its heading's text). */
  const select = (selector: string): Element[] => {
    if (!selector.startsWith("xpath=")) return [...document.querySelectorAll(selector)];
    const res = document.evaluate(selector.slice(6), document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
    return Array.from({ length: res.snapshotLength }, (_, i) => res.snapshotItem(i)).filter((n): n is Element => n instanceof Element);
  };
  for (const k of opts.keys) {
    const found = k.selector.split(" || ").flatMap(select).filter((e, i, a) => a.indexOf(e) === i && visible(e)).slice(0, k.max);
    found.forEach((el, i) =>
      nodes.push({ id: `key:${k.name}${k.max > 1 ? `#${i}` : ""}`, parent: null, box: box(el.getBoundingClientRect()), style: style(el), text: textOf(el), ownText: ownTextNodes(el).length > 0 }),
    );
  }

  // Visible text runs: an element's own text, boxed by its glyphs (a Range), keyed by the text.
  const texts: Node[] = [];
  const seen = new Map<string, number>();
  const owners = new Set<Element>();
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let t = walker.nextNode() as Text | null; t; t = walker.nextNode() as Text | null) {
    const el = t.parentElement;
    if (!el || !t.textContent?.trim() || owners.has(el) || /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|OPTION)$/.test(el.tagName)) continue;
    owners.add(el);
    if (!visible(el)) continue;
    const own = ownTextNodes(el);
    const range = document.createRange();
    let l = Infinity, tp = Infinity, rr = -Infinity, b = -Infinity;
    for (const n of own) {
      range.selectNodeContents(n);
      for (const rect of range.getClientRects()) {
        if (rect.width < 0.5) continue;
        l = Math.min(l, rect.left); tp = Math.min(tp, rect.top); rr = Math.max(rr, rect.right); b = Math.max(b, rect.bottom);
      }
    }
    if (!Number.isFinite(l)) continue;
    const text = own.map(shown).join(" ").replace(/\s+/g, " ").trim();
    // Numbered by the label the id shows: two texts sharing their first 60 characters stay apart.
    const label = text.slice(0, 60);
    const n = seen.get(label) ?? 0;
    seen.set(label, n + 1);
    let anc: Element | null = el;
    while (anc && !chains.has(anc)) anc = anc.parentElement;
    texts.push({
      id: `"${label}"${n ? `#${n}` : ""}`,
      parentChain: anc ? chains.get(anc) : undefined,
      parent: null,
      box: box({ left: l, top: tp, width: rr - l, height: b - tp }),
      style: style(el),
      text,
      ownText: true,
    });
  }

  const masks = opts.masks.flatMap((sel) => select(sel).map((e) => e.getBoundingClientRect()).filter((r) => r.width > 0 && r.height > 0).map(box));
  return { url: location.href, width: vw, height: document.documentElement.scrollHeight, nodes, texts, masks };
}
