// Whether a captured page counts as identical, and which unanswered requests its spec declares. Kept
// apart from run.mts so the tests need neither a browser nor playwright-core.
import type { PageSpec, Side } from "./spec.mts";
import type { Miss } from "./recording.mts";
import type { PageResult } from "./run.mts";

/**
 * Pixel-identical (outside masks and deliberate regions), no unexpected structural difference, and no
 * request left unanswered (by the recording, or to a host the harness does not know) unless the page
 * declares it: two sides missing the same data render the same empty state, which compares nothing.
 */
export function isIdentical(r: Pick<PageResult, "pixels" | "structure" | "sides" | "misses" | "unknown">, tolerance = 0): boolean {
  if (r.sides.reference.error || r.sides["dcl-one"].error) return false;
  return r.pixels.diff <= tolerance && r.structure.every((d) => d.expected) && (r.misses ?? []).every((m) => m.expected) && (r.unknown ?? []).every((u) => u.expected);
}

type Gated = Pick<PageResult, "page" | "viewport" | "pixels" | "structure" | "lines" | "sides" | "misses" | "unknown">;

/**
 * Each gated page×viewport (`page@viewport`) that is no longer identical, with every reason: an error,
 * its differing pixels, its unexpected structural differences, and the requests it left unanswered or
 * sent to an unknown host without declaring them. Empty when every gated page still is identical.
 */
export function gateFailures(gated: string[], results: Gated[], tolerance = 0): { id: string; reasons: string[] }[] {
  const failures: { id: string; reasons: string[] }[] = [];
  for (const id of gated) {
    const r = results.find((x) => `${x.page}@${x.viewport}` === id);
    if (!r) {
      failures.push({ id, reasons: ["it was not captured"] });
      continue;
    }
    if (isIdentical(r, tolerance)) continue;
    const reasons: string[] = [];
    for (const side of ["reference", "dcl-one"] as const) if (r.sides[side].error) reasons.push(`${side} error: ${r.sides[side].error}`);
    if (r.pixels.diff > tolerance) reasons.push(`${r.pixels.diff} differing pixels`);
    reasons.push(...r.lines.filter((_, i) => !r.structure[i]!.expected).slice(0, 40));
    for (const m of (r.misses ?? []).filter((x) => !x.expected)) reasons.push(`unanswered (${m.side}): ${m.method} ${m.service} ${m.path}${m.query ? `?${m.query}` : ""}`);
    for (const u of (r.unknown ?? []).filter((x) => !x.expected)) reasons.push(`unknown host (${u.side}): ${u.request}`);
    failures.push({ id, reasons });
  }
  return failures;
}

/** A miss with the page's rule attached when one of its `unrecorded` entries covers it. */
export function classifyMiss(m: Miss, spec: Pick<PageSpec, "unrecorded">): Miss & { expected?: string } {
  const text = `${m.method} ${m.service} ${m.path} ${m.request === undefined ? "" : JSON.stringify(m.request)}`;
  const rule = spec.unrecorded?.find((u) => u.match.test(text));
  return rule ? { ...m, expected: rule.why } : m;
}

/** A request to an unknown host (`GET host/path`), with the page's rule attached when one covers it. */
export function classifyUnknown(side: Side, request: string, spec: Pick<PageSpec, "unrecorded">): PageResult["unknown"][number] {
  const rule = spec.unrecorded?.find((u) => u.match.test(request));
  return rule ? { side, request, expected: rule.why } : { side, request };
}

/** A differing page×viewport's counts on the commit the gate last promoted: later captures may not exceed them. */
export type Ceiling = { pixels: number; structural: number };

const unexpected = (r: Pick<PageResult, "structure">) => r.structure.filter((d) => !d.expected).length;

/** The lower of the recorded ceiling and this capture's counts, so a promotion never raises a ceiling. */
export function lowerCeiling(old: Ceiling | undefined, r: Pick<PageResult, "pixels" | "structure">): Ceiling {
  const now = { pixels: r.pixels.diff, structural: unexpected(r) };
  return old ? { pixels: Math.min(old.pixels, now.pixels), structural: Math.min(old.structural, now.structural) } : now;
}

export function ceilingFailures(ceilings: Record<string, Ceiling>, results: Gated[]): { id: string; reasons: string[] }[] {
  const failures: { id: string; reasons: string[] }[] = [];
  for (const [id, c] of Object.entries(ceilings)) {
    const r = results.find((x) => `${x.page}@${x.viewport}` === id);
    if (!r) {
      failures.push({ id, reasons: ["it was not captured"] });
      continue;
    }
    const reasons: string[] = [];
    if (r.pixels.diff > c.pixels) reasons.push(`${r.pixels.diff} differing pixels, ceiling ${c.pixels}`);
    if (unexpected(r) > c.structural) reasons.push(`${unexpected(r)} structural differences, ceiling ${c.structural}`);
    if (reasons.length) failures.push({ id, reasons });
  }
  return failures;
}
