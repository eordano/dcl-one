// The parity gate: every page×viewport listed in state/shop-parity/gate.json must still be identical
// to the reference (no differing pixel outside its masks and deliberate regions, no unexpected
// structural difference) on the recording the gate names, and every page×viewport in its `ceilings`
// may not differ by more pixels or structural differences than the ceiling records. The gate is
// local, like the recording. A page×viewport whose page pages.mts no longer declares leaves the file. Exit 1
// names what changed.
//
//   node gate.mts [--build]      # check the gated pages and the ceilings
//   node gate.mts --promote      # capture every page, add the identical ones, lower the others' ceilings
//   node gate.mts --promote --pages landing,places [--viewports mobile]   # promote from these pages only
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { capture, defaultSet } from "./capture.mts";
import { isCommitted } from "./lib/build.mts";
import { arg } from "./lib/env.mts";
import { GATE_FILE, STATE } from "./lib/paths.mts";
import { PAGES } from "./pages.mts";
import { ceilingFailures, gateFailures, isIdentical, lowerCeiling, type Ceiling } from "./lib/verdict.mts";

type Gate = { recording: string; pixelTolerance: number; pages: Record<string, string>; ceilings?: Record<string, Ceiling> };

const gate: Gate = existsSync(GATE_FILE) ? (JSON.parse(readFileSync(GATE_FILE, "utf8")) as Gate) : { recording: defaultSet(), pixelTolerance: 0, pages: {} };
const promote = process.argv.includes("--promote");
// A page is marked identical against a commit: the build's sources and this tool's declarations both.
if (promote && !isCommitted()) {
  console.error("gate: commit or stash every change (and move apps/sites' uncommitted .env files aside) before promoting");
  process.exit(1);
}
const declared = new Set(PAGES.map((p) => p.id));
const undeclared = [...Object.keys(gate.pages), ...Object.keys(gate.ceilings ?? {})].filter((id) => !declared.has(id.split("@")[0]!));
if (undeclared.length) {
  for (const id of undeclared) {
    delete gate.pages[id];
    if (gate.ceilings) delete gate.ceilings[id];
  }
  writeFileSync(GATE_FILE, `${JSON.stringify(gate, null, 2)}\n`);
  console.log(`gate: dropped ${undeclared.length} page×viewport whose page is no longer declared: ${undeclared.join(", ")}`);
}
const gated = Object.keys(gate.pages);
const ceilings = gate.ceilings ?? {};
const checked = [...gated, ...Object.keys(ceilings)];
const out = join(STATE, "gate", new Date().toISOString().replace(/[:.]/g, "-"));

if (!promote && checked.length === 0) {
  console.log(`gate: no page is marked identical yet (${GATE_FILE}); nothing to check`);
  process.exit(0);
}

const pages = promote ? arg("--pages")?.split(",") : [...new Set(checked.map((g) => g.split("@")[0]!))];
const viewports = promote ? arg("--viewports") : [...new Set(checked.map((g) => g.split("@")[1]!))].join(",");
const results = await capture({ set: gate.recording, pages, viewports, out, build: process.argv.includes("--build"), quiet: true });

if (promote) {
  const today = new Date().toISOString().slice(0, 10);
  for (const r of results) {
    const id = `${r.page}@${r.viewport}`;
    if (isIdentical(r, gate.pixelTolerance)) {
      if (!gate.pages[id]) gate.pages[id] = today;
      delete ceilings[id];
    } else if (!gate.pages[id]) ceilings[id] = lowerCeiling(ceilings[id], r);
  }
  gate.ceilings = Object.fromEntries(Object.entries(ceilings).sort(([a], [b]) => a.localeCompare(b)));
  writeFileSync(GATE_FILE, `${JSON.stringify(gate, null, 2)}\n`);
  console.log(`gate: ${Object.keys(gate.pages).length} page×viewport marked identical; report ${join(out, "index.html")}`);
  process.exit(0);
}

const failures = [...gateFailures(gated, results, gate.pixelTolerance), ...ceilingFailures(ceilings, results)];
for (const id of gated) {
  const f = failures.find((x) => x.id === id);
  if (!f) {
    console.log(`gate: ${id} identical`);
    continue;
  }
  console.error(`gate: ${id} is no longer identical:`);
  for (const reason of f.reasons) console.error(`  ${reason}`);
}
for (const f of failures.filter((x) => ceilings[x.id])) console.error(`gate: ${f.id} rose above its ceiling: ${f.reasons.join("; ")}`);
console.log(`report: ${join(out, "index.html")}`);
process.exit(failures.length ? 1 : 0);
