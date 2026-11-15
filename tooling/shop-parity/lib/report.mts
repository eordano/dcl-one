// The run report: a summary of which pages are identical, then per page and viewport the
// reference, dcl-one and diff images and the structural differences, unexpected first.
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { referencePin, type ReferenceId } from "./paths.mts";
import type { PageResult } from "./run.mts";

const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);

export type RunInfo = { recording: string; recordedAt: string; references: Partial<Record<ReferenceId, string>>; dclOne: string; chromium: string; startedAt: string };

/** `decentraland/shop`: the owner and name of a reference's pinned repository. */
const repository = (id: ReferenceId) => new URL(referencePin(id).repository).pathname.slice(1).replace(/\.git$/, "");

/** `decentraland/shop@3a322cf97490, decentraland/sites@17b3535a2690`: the references a run compared with. */
export function referencesLabel(references: RunInfo["references"]): string {
  return (Object.entries(references) as [ReferenceId, string][]).map(([id, rev]) => `${repository(id)}@${rev.slice(0, 12)}`).join(", ");
}

export function summaryText(results: PageResult[]): string {
  const ok = results.filter((r) => r.identical).length;
  const rows = results.map((r) => {
    const unexpected = r.structure.filter((d) => !d.expected).length;
    const err = r.sides.reference.error || r.sides["dcl-one"].error;
    return `${r.identical ? "IDENTICAL" : "different"}  ${`${r.page}@${r.viewport}`.padEnd(22)} pixels ${String(r.pixels.diff).padStart(8)} (${(r.pixels.ratio * 100).toFixed(2)}%)  structure ${String(unexpected).padStart(4)} unexpected${err ? `  ERROR ${err}` : ""}`;
  });
  return `${ok}/${results.length} page×viewport identical\n${rows.join("\n")}`;
}

export function writeReport(dir: string, results: PageResult[], info: RunInfo): void {
  const ok = results.filter((r) => r.identical).length;
  const section = (r: PageResult) => {
    const id = `${r.page}@${r.viewport}`;
    const unexpected = r.structure.filter((d) => !d.expected);
    const expected = r.structure.filter((d) => d.expected);
    const sideNotes = (["reference", "dcl-one"] as const)
      .map((s) => {
        const x = r.sides[s];
        const notes = [x.error ? `error: ${x.error}` : "", ...x.console.slice(0, 8), ...(x.unknown.length ? [`unanswered hosts: ${x.unknown.join(", ")}`] : [])].filter(Boolean);
        return notes.length ? `<h4>${s} console</h4><pre>${esc(notes.join("\n"))}</pre>` : "";
      })
      .join("");
    const failed = (["reference", "dcl-one"] as const).flatMap((s) => (r.sides[s].failed ?? []).map((f) => `${s}: ${f}`));
    const failedNotes = failed.length ? `<h4>Responses with an error status</h4><pre>${esc(failed.join("\n"))}</pre>` : "";
    const misses = r.misses.length ? `<h4>Requests not in the recording</h4><pre>${esc([...new Set(r.misses.map((m) => `${m.side}: ${m.method} ${m.service} ${m.path}${m.query ? `?${m.query}` : ""}`))].join("\n"))}</pre>` : "";
    const bands = r.pixels.bands.length
      ? `<p class="bands">Where the pixels differ (rows, widest first): ${r.pixels.bands.map((b) => `y ${b.y0}–${b.y1} × x ${b.x0}–${b.x1} (${b.pixels} px)`).join("; ")}</p>`
      : "";
    const declared = [...r.masks.map((m) => `mask: ${m.why}`), ...r.regions.map((m) => `deliberate: ${m.why}`)];
    return `<section id="${esc(id)}">
<h2><span class="${r.identical ? "ok" : "bad"}">${r.identical ? "identical" : "different"}</span> ${esc(r.title)} — ${esc(r.viewport)} <code>${esc(r.path)}</code></h2>
<p>${r.pixels.diff} differing pixels (${(r.pixels.ratio * 100).toFixed(3)}%), ${r.pixels.antialiased} tolerated as anti-aliasing, ${r.pixels.masked} masked; page height ${r.sides.reference.height} → ${r.sides["dcl-one"].height}; ${unexpected.length} unexpected structural difference(s), ${expected.length} expected.</p>
${bands}
${declared.length ? `<ul class="declared">${declared.map((d) => `<li>${esc(d)}</li>`).join("")}</ul>` : ""}
<div class="shots">
<figure><figcaption>reference</figcaption><a href="${esc(id)}/reference.png"><img loading="lazy" src="${esc(id)}/reference.png"></a></figure>
<figure><figcaption>dcl-one</figcaption><a href="${esc(id)}/dcl-one.png"><img loading="lazy" src="${esc(id)}/dcl-one.png"></a></figure>
<figure><figcaption>diff (red: differs, yellow: anti-aliasing, violet: masked)</figcaption><a href="${esc(id)}/diff.png"><img loading="lazy" src="${esc(id)}/diff.png"></a></figure>
</div>
<h3>Structural differences to fix</h3>
${unexpected.length ? `<pre class="lines">${esc(unexpected.map((d) => r.lines[r.structure.indexOf(d)]).join("\n"))}</pre>` : "<p>None.</p>"}
${expected.length ? `<details><summary>${expected.length} expected (declared deliberate differences)</summary><pre class="lines">${esc(expected.map((d) => r.lines[r.structure.indexOf(d)]).join("\n"))}</pre></details>` : ""}
${sideNotes}${misses}${failedNotes}
</section>`;
  };
  const rows = results
    .map((r) => `<tr><td><a href="#${esc(`${r.page}@${r.viewport}`)}">${esc(r.title)}</a></td><td>${esc(r.viewport)}</td><td class="${r.identical ? "ok" : "bad"}">${r.identical ? "identical" : "different"}</td><td>${r.pixels.diff}</td><td>${(r.pixels.ratio * 100).toFixed(2)}%</td><td>${r.structure.filter((d) => !d.expected).length}</td></tr>`)
    .join("");
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Shop parity</title>
<style>
:root{--bg:#fff;--fg:#1b1b1f;--muted:#5b5b66;--line:#ddd;--ok:#0a7a3c;--bad:#b3261e;--code:#f4f4f6}
@media (prefers-color-scheme:dark){:root{--bg:#141418;--fg:#ececf1;--muted:#a0a0ab;--line:#33333a;--ok:#5bd38e;--bad:#ff8a80;--code:#1f1f25}}
body{margin:0;padding:16px;background:var(--bg);color:var(--fg);font:14px/1.45 system-ui,sans-serif}
h1{font-size:20px;margin:0 0 4px}h2{font-size:16px;margin:28px 0 6px}h3{font-size:14px;margin:14px 0 4px}h4{font-size:13px;margin:10px 0 2px}
.meta,.bands{color:var(--muted)}table{border-collapse:collapse;margin:12px 0}td,th{border-bottom:1px solid var(--line);padding:4px 10px;text-align:left}
.ok{color:var(--ok);font-weight:600}.bad{color:var(--bad);font-weight:600}code,pre{background:var(--code);border-radius:4px}code{padding:1px 4px}
pre{padding:8px;overflow:auto;max-height:420px;font-size:12px;white-space:pre-wrap}
.shots{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:8px}.shots figure{margin:0;max-height:900px;overflow:auto;border:1px solid var(--line)}
.shots img{width:100%;display:block}figcaption{font-size:12px;color:var(--muted);padding:2px 6px;position:sticky;top:0;background:var(--bg)}
@media (max-width:700px){.shots{grid-template-columns:1fr}}
</style></head><body>
<h1>Shop parity: ${ok}/${results.length} page×viewport identical</h1>
<p class="meta">Reference <code>${esc(referencesLabel(info.references))}</code> vs dcl-one <code>${esc(info.dclOne)}</code>, recording <code>${esc(info.recording)}</code> (${esc(info.recordedAt)}), Chromium <code>${esc(info.chromium)}</code>, run ${esc(info.startedAt)}. Both sides read the same recorded answers; pictures are deterministic placeholders.</p>
<table><thead><tr><th>Page</th><th>Viewport</th><th>Verdict</th><th>Pixels</th><th>Ratio</th><th>Structural</th></tr></thead><tbody>${rows}</tbody></table>
${results.map(section).join("\n")}
</body></html>`;
  writeFileSync(join(dir, "index.html"), html);
  writeFileSync(join(dir, "run.json"), `${JSON.stringify({ info, results }, null, 1)}\n`);
  writeFileSync(join(dir, "summary.txt"), `${summaryText(results)}\n`);
}
