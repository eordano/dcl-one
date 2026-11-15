// Compare apps/ui3/src/shop with the pinned reference shop over a local recording (record.mts).
//
//   node capture.mts [--set <recording>] [--pages overview,items] [--viewports desktop,mobile] [--out <dir>] [--build]
//
// Writes <out>/index.html (the report), run.json and summary.txt; default out is
// state/shop-parity/runs/<timestamp>/. Nothing is read from the network.
import { existsSync, mkdirSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { runnable } from "./lib/session.mts";
import { PAGES } from "./pages.mts";
import { assertFreshBuild, buildSites, dclOneIdentity } from "./lib/build.mts";
import { arg } from "./lib/env.mts";
import { GATE_FILE, RECORDINGS, STATE, type ReferenceId } from "./lib/paths.mts";
import { referenceOf } from "./lib/spec.mts";
import { Recording } from "./lib/recording.mts";
import { summaryText, writeReport } from "./lib/report.mts";
import { runAll, selectViewports, type PageResult } from "./lib/run.mts";
import { prepareReference } from "./reference.mts";

export function defaultSet(): string {
  if (existsSync(GATE_FILE)) return (JSON.parse(readFileSync(GATE_FILE, "utf8")) as { recording: string }).recording;
  const sets = existsSync(RECORDINGS) ? readdirSync(RECORDINGS).sort() : [];
  if (!sets.length) throw new Error(`no recording under ${RECORDINGS}: run record.mts --set <name>`);
  return sets.at(-1)!;
}

export async function capture(opts: { set: string; pages?: string[]; viewports?: string; out: string; build?: boolean; quiet?: boolean }): Promise<PageResult[]> {
  const recording = new Recording(join(RECORDINGS, opts.set));
  if (!recording.manifest) throw new Error(`no recording ${opts.set} under ${RECORDINGS}: run record.mts --set ${opts.set}`);
  if (recording.manifest.incomplete) throw new Error(`recording ${opts.set} did not finish: run record.mts --set ${opts.set} again, which goes on from what it read`);
  const specs = runnable(opts.pages ? PAGES.filter((p) => opts.pages!.includes(p.id)) : PAGES);
  // Only the references the selected pages compare with are built; the run is labelled with their revisions.
  const references: Partial<Record<ReferenceId, string>> = {};
  for (const reference of new Set(specs.map(referenceOf))) {
    const revision = prepareReference({ reference }).stamp.revision;
    references[reference] = revision;
    const recorded = recording.manifest.references?.[reference] ?? (reference === "shop" ? recording.manifest.reference : undefined);
    if (recorded !== revision) console.warn(`warning: recording ${opts.set} was made with ${reference} reference ${recorded?.slice(0, 12) ?? "none"}, the pinned one is ${revision.slice(0, 12)}; what it reads now may be missing from the recording`);
  }
  if (opts.build) buildSites();
  // Comparing a build of other sources would label its result with these.
  const identity = dclOneIdentity();
  assertFreshBuild(identity);
  mkdirSync(opts.out, { recursive: true });
  const startedAt = new Date().toISOString();
  const { results, executable } = await runAll(recording, specs, selectViewports(opts.viewports), {
    now: Date.parse(recording.manifest.recordedAt),
    targets: recording.manifest.targets,
    out: opts.out,
    onResult: (r) => {
      if (!opts.quiet) console.log(`${r.identical ? "identical" : "different"} ${r.page}@${r.viewport}: ${r.pixels.diff} px, ${r.structure.filter((d) => !d.expected).length} structural`);
    },
  });
  writeReport(opts.out, results, {
    recording: opts.set,
    recordedAt: recording.manifest.recordedAt,
    references,
    dclOne: identity,
    chromium: executable,
    startedAt,
  });
  return results;
}

if (import.meta.url === pathToFileURL(process.argv[1]!).href) {
  const out = arg("--out") ?? join(STATE, "runs", new Date().toISOString().replace(/[:.]/g, "-"));
  const results = await capture({ set: arg("--set") ?? defaultSet(), pages: arg("--pages")?.split(","), viewports: arg("--viewports"), out, build: process.argv.includes("--build") });
  console.log(`\n${summaryText(results)}\nreport: ${join(out, "index.html")}`);
}
