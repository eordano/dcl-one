import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const TOOL = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const ROOT = resolve(TOOL, "../..");
export const STATE = join(ROOT, "state", "shop-parity");
/** Recordings hold real answers from the production services: they stay in state/, out of git. */
export const RECORDINGS = join(STATE, "recordings");
/** The pages found identical on this machine's recording. */
export const GATE_FILE = join(STATE, "gate.json");
/** The navigation walk's reports, one directory per run. */
export const NAV = join(STATE, "nav");
export const UI3 = join(ROOT, "apps", "ui3");
export const SITES = join(ROOT, "apps", "sites");

export type ReferencePin = {
  repository: string;
  revision: string;
  date: string;
  /** The app's directory in the repository; "." when the repository is the app. */
  app: string;
  buildEnv: Record<string, string>;
};

/** The pinned references: the Shop UI and the sites SPA (decentraland.org), each with its pin file and mirror override. */
export type ReferenceId = "shop" | "sites";
export const REFERENCES: Record<ReferenceId, { file: string; repoEnv: string }> = {
  shop: { file: "reference.json", repoEnv: "SHOP_PARITY_REFERENCE_REPO" },
  sites: { file: "reference-sites.json", repoEnv: "SHOP_PARITY_REFERENCE_REPO_SITES" },
};
export const REFERENCE_IDS = Object.keys(REFERENCES) as ReferenceId[];

export function isReferenceId(s: string): s is ReferenceId {
  return s in REFERENCES;
}

function pinFile(reference: ReferenceId): string {
  return join(TOOL, REFERENCES[reference].file);
}

/**
 * A pin file (reference.json, reference-sites.json), checked before anything uses it: its revision
 * names a directory that is wiped and rebuilt, and its repository and revision reach git.
 */
export function referencePin(file: string | ReferenceId = "shop"): ReferencePin {
  if (isReferenceId(file)) file = pinFile(file);
  const pin = JSON.parse(readFileSync(file, "utf8")) as ReferencePin;
  const bad = (what: string) => new Error(`${file}: ${what}`);
  if (!/^https:\/\/github\.com\/[\w.-]+\/[\w.-]+$/.test(pin.repository ?? "")) throw bad(`repository must be an https://github.com/<owner>/<repo> URL, not ${JSON.stringify(pin.repository)}`);
  if (!/^[0-9a-f]{40}$/.test(pin.revision ?? "")) throw bad(`revision must be a full 40-hex commit, not ${JSON.stringify(pin.revision)}`);
  if (typeof pin.app !== "string" || (pin.app !== "." && !pin.app.split("/").every((seg) => /^[\w.-]+$/.test(seg) && seg !== "." && seg !== ".."))) throw bad(`app must be "." or a relative path inside the repository, not ${JSON.stringify(pin.app)}`);
  const env = pin.buildEnv ?? {};
  for (const [k, v] of Object.entries(env)) if (!/^VITE_[A-Z0-9_]+$/.test(k) || typeof v !== "string") throw bad(`buildEnv may hold only VITE_* strings, not ${JSON.stringify(k)}`);
  return pin;
}

/** Where the pinned reference is checked out and built (gitignored). */
export function referenceDir(pin: ReferencePin = referencePin()): string {
  return join(STATE, "reference", pin.revision);
}

export function referenceDist(pin: ReferencePin = referencePin()): string {
  return join(referenceDir(pin), "src", pin.app, "dist");
}

/** Loopback ports of a capture; SHOP_PARITY_PORT_BASE moves the four together. */
const PORT_BASE = Number(process.env.SHOP_PARITY_PORT_BASE || 5176);
if (!Number.isInteger(PORT_BASE) || PORT_BASE < 1024 || PORT_BASE > 65532) throw new Error(`SHOP_PARITY_PORT_BASE must be a port from 1024 to 65532, not ${JSON.stringify(process.env.SHOP_PARITY_PORT_BASE)}`);

export const PORTS = {
  reference: () => PORT_BASE,
  replay: () => PORT_BASE + 1,
  sites: () => PORT_BASE + 2,
  /** The sites reference's static server (the shop reference is `reference`). */
  referenceSites: () => PORT_BASE + 3,
};

export function referencePort(reference: ReferenceId): number {
  return reference === "shop" ? PORTS.reference() : PORTS.referenceSites();
}
