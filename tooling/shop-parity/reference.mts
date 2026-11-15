// Check out and build a pinned reference (reference.json: decentraland/shop; reference-sites.json:
// decentraland/sites) under state/shop-parity/reference/<revision>/, outside git. The build is the
// production bundle with base "/" and the production environment selected; every network read is
// answered by the replay at capture time, so nothing in the build is pointed anywhere else.
//
//   node reference.mts [--reference shop|sites] [--force]
//
// SHOP_PARITY_REFERENCE_REPO (shop) and SHOP_PARITY_REFERENCE_REPO_SITES (sites, a private
// repository) override the clone source with a local mirror holding the revision.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { join, relative } from "node:path";
import { pathToFileURL } from "node:url";
import { childEnv } from "./lib/env.mts";
import { isReferenceId, REFERENCE_IDS, REFERENCES, referenceDir, referenceDist, referencePin, type ReferenceId } from "./lib/paths.mts";

function run(cmd: string, args: string[], cwd: string, env: NodeJS.ProcessEnv = process.env): string {
  const r = spawnSync(cmd, args, { cwd, env, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], maxBuffer: 1 << 26 });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed in ${cwd} (exit ${r.status})`);
  return r.stdout.trim();
}

/** sha256 over every file of a tree, by relative path, so a changed byte changes the identity. */
export function treeHash(dir: string): string {
  const h = createHash("sha256");
  const walk = (d: string) => {
    for (const name of readdirSync(d).sort()) {
      const p = join(d, name);
      if (statSync(p).isDirectory()) walk(p);
      else h.update(relative(dir, p)).update("\0").update(readFileSync(p)).update("\0");
    }
  };
  walk(dir);
  return h.digest("hex");
}

export type ReferenceStamp = { repository: string; revision: string; date: string; buildEnv: Record<string, string>; node: string; builtAt: string; lockSha256: string; distSha256: string };

/**
 * The reference's dist, built now unless the one on disk is of this pin and intact. The build is
 * `vite build` alone. The sites repository's build script also runs the type check and four
 * post-build scripts, none of which changes a compared page (README, "Two references"):
 * prerender-hero.mjs puts a static hero shell and the English dictionary into dist/index.html, which
 * the app removes or loads itself on hydration, before anything is measured; prerender-legal.mjs,
 * build-route-manifest.mjs and build-llms-txt.mjs write dist/legal/, dist/routes.json and
 * dist/llms.txt, files the production edge reads and the app never requests.
 */
export function prepareReference(opts: { reference?: ReferenceId; force?: boolean } = {}): { dist: string; stamp: ReferenceStamp } {
  const reference = opts.reference ?? "shop";
  const force = opts.force ?? false;
  const pin = referencePin(reference);
  const dir = referenceDir(pin);
  const src = join(dir, "src");
  const dist = referenceDist(pin);
  const stampFile = join(dir, "build.json");
  if (!force && existsSync(stampFile) && existsSync(dist)) {
    const stamp = JSON.parse(readFileSync(stampFile, "utf8")) as ReferenceStamp;
    const same = stamp.revision === pin.revision && JSON.stringify(stamp.buildEnv) === JSON.stringify(pin.buildEnv) && stamp.node === process.version;
    if (same && stamp.distSha256 === treeHash(dist)) return { dist, stamp };
  }
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(src, { recursive: true });
  const repo = process.env[REFERENCES[reference].repoEnv] || pin.repository;
  run("git", ["init", "-q"], src);
  try {
    run("git", ["fetch", "-q", "--depth", "1", "--no-tags", "--", repo, pin.revision], src);
  } catch {
    run("git", ["fetch", "-q", "--no-tags", "--", repo, "+refs/heads/*:refs/remotes/origin/*"], src);
  }
  run("git", ["-c", "advice.detachedHead=false", "checkout", "-q", pin.revision], src);
  const head = run("git", ["rev-parse", "HEAD"], src);
  if (head !== pin.revision) throw new Error(`checked out ${head}, pinned ${pin.revision}`);

  const app = join(src, pin.app);
  // Only what npm and Vite need to run: a developer's own VITE_* or deploy variables must not reach the build.
  const env = childEnv();
  Object.assign(env, { PUPPETEER_SKIP_DOWNLOAD: "1", CI: "1" });
  // --ignore-scripts: the app's `prepare` installs git hooks and puppeteer/sentry-cli download
  // binaries; the build needs none of them.
  run("npm", ["ci", "--ignore-scripts", "--no-audit", "--no-fund"], app, env);
  const buildEnv: NodeJS.ProcessEnv = { ...env, ...pin.buildEnv };
  run(process.execPath, ["node_modules/vite/bin/vite.js", "build", "--logLevel", "warn"], app, buildEnv);

  const stamp: ReferenceStamp = {
    repository: pin.repository,
    revision: pin.revision,
    date: pin.date,
    buildEnv: pin.buildEnv,
    node: process.version,
    builtAt: new Date().toISOString(),
    lockSha256: createHash("sha256").update(readFileSync(join(app, "package-lock.json"))).digest("hex"),
    distSha256: treeHash(dist),
  };
  writeFileSync(stampFile, `${JSON.stringify(stamp, null, 2)}\n`);
  return { dist, stamp };
}

if (import.meta.url === pathToFileURL(process.argv[1]!).href) {
  const i = process.argv.indexOf("--reference");
  const reference = i >= 0 ? process.argv[i + 1] ?? "" : "shop";
  if (!isReferenceId(reference)) throw new Error(`--reference must be one of ${REFERENCE_IDS.join(", ")}, not ${JSON.stringify(reference)}`);
  const { dist, stamp } = prepareReference({ reference, force: process.argv.includes("--force") });
  console.log(`reference ${reference} ${stamp.revision} built at ${dist}`);
}
