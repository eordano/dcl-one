// Record the API answers both sides read, once, from the production services (read-only GETs and
// public POST lookups), into state/shop-parity/recordings/<set>/. A recording holds real answers, so it
// stays on the machine that made it, out of git; each developer records their own.
//
//   node record.mts --set <name> [--pages overview,items] [--viewports desktop,mobile]
//   node record.mts --set <name> --fill     # replay; read what the replay could not answer, once
import { runnable, testWalletSigner } from "./lib/session.mts";
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import type { Page } from "playwright-core";
import { join } from "node:path";
import { PAGES } from "./pages.mts";
import { offerSynthesizer } from "./lib/offers.mts";
import { arg } from "./lib/env.mts";
import { RECORDINGS, REFERENCE_IDS, referencePin, ROOT, SITES, STATE } from "./lib/paths.mts";
import { referenceOf, type PageSpec } from "./lib/spec.mts";
import { fillMisses, Recording, savingAfter } from "./lib/recording.mts";
import { captureSide } from "./lib/capture.mts";
import { runAll, SEED, selectViewports } from "./lib/run.mts";
import { prepareReference } from "./reference.mts";

const set = arg("--set");
if (!set || !/^[a-z0-9-]+$/.test(set)) throw new Error("--set <name> (lowercase, digits, dashes) is required");
const dir = join(RECORDINGS, set);
const wanted = arg("--pages")?.split(",");
const specs = runnable(wanted ? PAGES.filter((p) => wanted.includes(p.id)) : PAGES);

if (process.argv.includes("--fill")) {
  // Replay the recording, then read each request it could not answer once from production.
  const replay = new Recording(dir);
  if (!replay.manifest) throw new Error(`no recording ${set} under ${RECORDINGS}`);
  await runAll(replay, specs, selectViewports(arg("--viewports")), { now: Date.parse(replay.manifest.recordedAt), targets: replay.manifest.targets, out: join(STATE, "fill-run", set) });
  const { failed } = await fillMisses(replay, new Recording(dir, { live: true, signer: testWalletSigner() }));
  if (failed.length) {
    console.error(`could not fill ${failed.length} request(s); what was filled is saved:\n${failed.map((f) => `  ${f}`).join("\n")}`);
    process.exitCode = 1;
  }
} else {
  for (const reference of new Set(specs.map(referenceOf))) prepareReference({ reference });
  // Checked before the set is opened: a run that cannot start must not mark a shared set incomplete.
  if (!existsSync(join(SITES, "build", "server", "index.js"))) throw new Error("apps/sites is not built: run capture.mts --build (or `pnpm build` in apps/sites) first");
  const live = new Recording(dir, { live: true, signer: testWalletSigner() });
  live.synthesize = offerSynthesizer(live);
  const targets: Record<string, string> = { ...(live.manifest?.targets ?? {}) };
  const now = live.manifest?.recordedAt ? Date.parse(live.manifest.recordedAt) : Date.now();
  let current: PageSpec | null = null;
  /** The targets a discover found in this run, on any viewport. */
  const found = new Set<string>();
  // targets starts from the previous manifest. discover runs once per viewport (or once per page with discoverAt):
  // a target one of them found in this run stays, and one that a discover reports as not found, with none finding
  // it, is dropped rather than kept from the previous set.
  const discoverInto = async (spec: PageSpec, page: Page) => {
    for (const [key, value] of Object.entries(await spec.discover!(page, targets))) {
      if (value != null) {
        targets[key] = value;
        found.add(key);
      } else if (!found.has(key)) delete targets[key];
    }
  };
  const commit = spawnSync("git", ["rev-parse", "HEAD"], { cwd: ROOT, encoding: "utf8" }).stdout.trim();
  const run = () =>
    runAll(live, specs, selectViewports(arg("--viewports")), {
      now,
      targets,
      out: join(STATE, "record-run", set),
      beforePage: async (spec, browser, servers) => {
        current = spec;
        if (!spec.discover || !spec.discoverAt) return;
        // The page's path needs what discover finds: it reads the reference's discoverAt page first, in the
        // page's own context (session, init), through the same recording.
        const vp = selectViewports(arg("--viewports"))[0]!;
        const at: PageSpec = { ...spec, path: spec.discoverAt, ready: { reference: "body", "dcl-one": "body" }, keys: [], masks: [], regions: [], setup: undefined, maxHeight: 1 };
        await captureSide(browser, at, vp, "reference", { origin: servers.origin("reference", referenceOf(spec)), recording: live, now, seed: SEED, targets, onPage: (page) => discoverInto(spec, page) });
      },
      onPage: async (side, page) => {
        if (side === "reference" && current?.discover && !current.discoverAt) await discoverInto(current, page);
      },
      onResult: (r) => {
        console.log(`recorded ${r.page}@${r.viewport}: ${r.sides.reference.error ?? "reference ok"}, ${r.sides["dcl-one"].error ?? "dcl-one ok"}`);
        // A reference that failed before discover ran read no target here: what no viewport found keeps the previous set's.
        if (r.sides.reference.error && specs.some((s) => s.id === r.page && s.discover)) {
          console.warn(`  ${r.page}@${r.viewport} read no targets: a target no viewport found in this run is the previous set's`);
        }
      },
    });
  // Saved even if the run dies midway: running the same command again goes on from what was read.
  const { results } = await savingAfter(live, run, () => ({
    set,
    recordedAt: new Date(now).toISOString(),
    source: "production Decentraland services, read-only, through the pinned references (shop, sites) and the dcl-one UI",
    reference: referencePin("shop").revision,
    references: Object.fromEntries(REFERENCE_IDS.map((r) => [r, referencePin(r).revision])),
    dclOne: commit,
    targets,
  }));
  const missing = results.flatMap((r) => r.misses);
  if (missing.length) console.log(`unanswered while recording (dcl-one-only routes): ${[...new Set(missing.map((m) => `${m.method} ${m.service} ${m.path}`))].join(", ")}`);
  // A page may have rendered (or discovered its targets) from these errors: they were not kept.
  if (live.failures.length) {
    console.error(`production answered ${live.failures.length} request(s) with a server error; none was kept, so record.mts --set ${set} --fill can ask again, but check the pages that read them:\n${[...new Set(live.failures)].map((f) => `  ${f}`).join("\n")}`);
    process.exitCode = 1;
  }
}

// An error answer replays like any other: both sides render the same error state and nothing is reported.
const errors = [...new Recording(dir).entries.values()].filter((e) => e.status >= 400 && e.status !== 404);
if (errors.length) console.warn(`warning: ${errors.length} answer(s) were recorded as errors; record again before trusting the pages that read them:\n${errors.map((e) => `  ${e.status} ${e.method} ${e.service} ${e.path}`).join("\n")}`);
console.log(`recording ${set}: ${dir}`);
