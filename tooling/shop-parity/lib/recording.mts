// A recording: upstream answers keyed by (method, service, path, canonical query, request body),
// stored once and served to both sides. Replay never reaches the network; record mode reads the
// production service on a miss and keeps its answer. Recordings live under state/ and are not committed.
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, rmdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { ReferenceId } from "./paths.mts";
import { imageSize, placeholderPng } from "./png.mts";
import { canonicalQuery, cmsQuery, ORIGINS, type Service } from "./services.mts";

export type Entry = {
  key: string;
  method: string;
  service: Service;
  path: string;
  query: string;
  /** The canonical request body a POST was keyed on (JSON), when it had one. */
  request?: unknown;
  status: number;
  contentType: string;
  /** File under bodies/, named by content hash. */
  body: string;
};

export type Manifest = {
  set: string;
  recordedAt: string;
  source: string;
  /** The shop reference's revision (the first pin; `references` names every pin). */
  reference: string;
  /** The pinned revision of each reference when the set was recorded. */
  references?: Partial<Record<ReferenceId, string>>;
  dclOne: string;
  /** Page targets discovered while recording (item URLs and the like). */
  targets: Record<string, string>;
  entries: number;
  /** Set while the recording run that wrote it has not finished (it failed or was interrupted). */
  incomplete?: boolean;
};

export type Answer = { status: number; contentType: string; body: Buffer; headers?: Record<string, string> };

/** Sorted-key JSON, so two encodings of one request body hash alike. */
export function canonicalJson(v: unknown): string {
  if (Array.isArray(v)) return `[${v.map(canonicalJson).join(",")}]`;
  if (v && typeof v === "object") {
    return `{${Object.keys(v as object)
      .sort()
      .map((k) => `${JSON.stringify(k)}:${canonicalJson((v as Record<string, unknown>)[k])}`)
      .join(",")}}`;
  }
  return JSON.stringify(v);
}

const sha = (b: Buffer | string) => createHash("sha256").update(b).digest("hex");

/** Decentraland's own hosts: the only ones record mode reads a picture from, to learn its size, and the only ones a signed read goes to. */
export const learnable = (url: URL) => url.protocol === "https:" && (url.hostname === "decentraland.org" || url.hostname.endsWith(".decentraland.org"));
/** The hosts a live read may reach, a redirect included. */
const ORIGIN_HOSTS = new Set(Object.values(ORIGINS).map((o) => new URL(o).host));
/** The hops one live read follows. A bound on a redirect loop, not a measurement. */
const REDIRECT_HOPS = 4;
/** How long a save waits for another recorder's save, and how often it looks. A bound, not a measurement: a save writes three JSON files, and one that waits longer fails with the lock's path instead of hanging. */
const LOCK_WAIT_MS = 30_000;
const LOCK_STEP_MS = 50;

/** Runs `work` holding the set's lock, an exclusive directory beside its files, so two recorders' saves never interleave between the merge and the write. */
function locked<T>(dir: string, waitMs: number, work: () => T): T {
  const lock = join(dir, ".lock");
  for (let waited = 0; ; waited += LOCK_STEP_MS) {
    try {
      mkdirSync(lock);
      break;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== "EEXIST") throw err;
      if (waited >= waitMs) throw new Error(`${lock} is held: another recorder is saving this set, or one stopped while saving (then remove the directory)`);
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, LOCK_STEP_MS);
    }
  }
  try {
    return work();
  } finally {
    rmdirSync(lock);
  }
}

/** Write, then rename: a reader sees the file as it was or as it is, never part of it. */
function writeWhole(file: string, text: string) {
  const part = `${file}.${process.pid}.part`;
  writeFileSync(part, text);
  renameSync(part, file);
}

export function entryKey(method: string, service: Service, path: string, query: string, request?: unknown): string {
  const base = `${method} ${service} ${path}${query ? `?${query}` : ""}`;
  return request === undefined ? base : `${base} #${sha(canonicalJson(request)).slice(0, 16)}`;
}

type RpcCall = { jsonrpc?: string; id?: unknown; method: string; params?: unknown };

function parseBody(body: Buffer | undefined): unknown {
  if (!body || body.length === 0) return undefined;
  try {
    return JSON.parse(body.toString("utf8"));
  } catch {
    return { raw: sha(body) };
  }
}

/**
 * Lookups the apps batch by id, each app its own way: answered row by row from every recorded batch
 * of the same route.
 */
type Batched = {
  service: Service;
  method: string;
  path: string;
  ids: (query: URLSearchParams, request: unknown) => string[];
  rows: (body: unknown) => unknown[];
  key: (row: unknown) => string | undefined;
  wrap: (rows: unknown[]) => unknown;
  /**
   * A row changes between the recorded batches (a profile redeployed while recording): the answer is composed from
   * the first recorded row per id even when the batch asked was recorded, so the batch a page composes (the
   * reference coalesces one tick's profile fetches) does not pick the snapshot.
   */
  oneRow?: true;
};

const field = (k: string) => (row: unknown) => {
  const v = (row as Record<string, unknown> | null)?.[k];
  return typeof v === "string" ? v.toLowerCase() : undefined;
};

const BATCHED: Batched[] = [
  {
    service: "marketplace",
    method: "GET",
    path: "/v3/catalog/items",
    ids: (q) => q.getAll("id").map((s) => s.toLowerCase()),
    rows: (b) => ((b as { data?: unknown[] }).data ?? []),
    key: field("id"),
    wrap: (rows) => ({ data: rows, total: rows.length }),
  },
  {
    service: "marketplace",
    method: "GET",
    path: "/v1/picks/stats",
    ids: (q) => q.getAll("itemId").map((s) => s.toLowerCase()),
    rows: (b) => ((b as { data?: unknown[] }).data ?? []),
    key: field("itemId"),
    wrap: (rows) => ({ ok: true, data: rows }),
  },
  {
    service: "peer",
    method: "POST",
    path: "/lambdas/profiles",
    ids: (_q, r) => (((r as { ids?: string[] } | undefined)?.ids ?? []) as string[]).map((s) => s.toLowerCase()),
    rows: (b) => (Array.isArray(b) ? b : b && typeof b === "object" && "avatars" in (b as object) ? [b] : []),
    key: (row) => {
      const a = (row as { avatars?: { ethAddress?: string; userId?: string }[] }).avatars?.[0];
      return (a?.ethAddress ?? a?.userId)?.toLowerCase();
    },
    wrap: (rows) => rows,
    oneRow: true,
  },
];

const oneRowBatch = (method: string, service: Service, path: string) =>
  BATCHED.some((b) => b.oneRow && b.service === service && b.method === method && b.path === path);

/** `signed`: the page sent it signed, with this `x-identity-metadata`, so a fill signs it again the same way. */
export type Miss = { key: string; service: Service; method: string; path: string; query: string; side: string; request?: unknown; signed?: string };

export class Recording {
  readonly dir: string;
  readonly entries = new Map<string, Entry>();
  manifest: Manifest | null = null;
  /** The manifest target keys this recorder loaded: a save keeps the other keys another recorder saved meanwhile. */
  private loadedTargets = new Set<string>();
  readonly misses: Miss[] = [];
  /** The bytes a missed POST carried, by entry key, so record.mts --fill can send them as they were. */
  readonly missBodies = new Map<string, { body: Buffer; contentType?: string }>();
  readonly hits = new Map<string, Set<string>>();
  /** Intrinsic size of each content picture, by imageSeed; the replay paints placeholders this size. */
  readonly images = new Map<string, { width: number; height: number }>();
  /** dcl-one-only routes with no upstream (purchase offers): answered by this hook when recording. */
  synthesize: ((req: { method: string; service: Service; path: string; query: string; request: unknown }) => Promise<Answer | null>) | null = null;
  readonly live: boolean;
  /** Record mode: requests production answered with a server error, which nothing kept (record.mts lists them). */
  readonly failures: string[] = [];
  /** The pauses before the second and third attempt when production fails. */
  private readonly retryDelays: number[];
  private readonly lockWaitMs: number;
  /**
   * Signs a request a page sent signed (an `x-identity-*` chain), as the test wallet, at the real
   * time: the page signed at the fixed clock, which production refuses once a minute has passed.
   * Without it a signed request goes out unsigned, as every other request does.
   */
  private readonly signer?: (method: string, path: string, metadata: string) => Promise<Record<string, string>>;

  /** Whether a request a page signed can go out signed (a live recording with the test wallet's signer). */
  get canSign(): boolean {
    return !!this.signer;
  }

  constructor(dir: string, opts: { live?: boolean; retryDelays?: number[]; lockWaitMs?: number; signer?: (method: string, path: string, metadata: string) => Promise<Record<string, string>> } = {}) {
    this.dir = dir;
    this.live = !!opts.live;
    this.retryDelays = opts.retryDelays ?? [1000, 3000];
    this.lockWaitMs = opts.lockWaitMs ?? LOCK_WAIT_MS;
    this.signer = opts.signer;
    const index = join(dir, "index.json");
    if (existsSync(index)) {
      for (const e of JSON.parse(readFileSync(index, "utf8")) as Entry[]) this.entries.set(e.key, e);
    }
    const manifest = join(dir, "manifest.json");
    if (existsSync(manifest)) this.manifest = JSON.parse(readFileSync(manifest, "utf8")) as Manifest;
    this.loadedTargets = new Set(Object.keys(this.manifest?.targets ?? {}));
    const images = join(dir, "images.json");
    if (existsSync(images)) for (const [k, v] of Object.entries(JSON.parse(readFileSync(images, "utf8")))) this.images.set(k, v as { width: number; height: number });
  }

  /**
   * Record mode: read the real picture once to learn its size (its bytes are never kept). Only from
   * Decentraland's own hosts, redirects included: a page renders pictures from any URL a creator or
   * profile names, and the harness has no reason to reach a LAN or link-local address for one.
   */
  async learnImage(seed: string, url: string): Promise<void> {
    if (!this.live || this.images.has(seed)) return;
    try {
      let next = new URL(url);
      for (let hop = 0; hop < REDIRECT_HOPS && learnable(next); hop++) {
        const res = await fetch(next, { redirect: "manual", signal: AbortSignal.timeout(20_000) });
        const location = res.status >= 300 && res.status < 400 ? res.headers.get("location") : null;
        if (location) {
          await res.body?.cancel();
          next = new URL(location, next);
          continue;
        }
        const size = imageSize(Buffer.from(await res.arrayBuffer()));
        if (size) this.images.set(seed, size);
        return;
      }
    } catch {
      // unknown size: the placeholder keeps its default
    }
  }

  body(e: Entry): Buffer {
    return readFileSync(join(this.dir, "bodies", e.body));
  }

  putBody(e: Omit<Entry, "body">, body: Buffer): Entry {
    return this.store(e, body);
  }

  private note(key: string, side: string) {
    if (!this.hits.has(key)) this.hits.set(key, new Set());
    this.hits.get(key)!.add(side);
  }

  /**
   * One read of a production service. A redirect is followed by hand, and only to a host `ORIGINS` lists; the
   * test wallet's signature is made again for each hop and goes to Decentraland's hosts alone. A redirect to
   * any other host is not followed: it is listed, and the 502 it answers with is kept nowhere.
   */
  private async fetchLive(method: string, service: Service, path: string, query: string, body?: Buffer, headers?: Record<string, string>): Promise<Answer> {
    const origin = ORIGINS[service];
    if (!origin) return { status: 404, contentType: "application/json", body: Buffer.from(`{"error":"no upstream for ${service}"}`) };
    const url = `${origin}${path}${query ? `?${query}` : ""}`;
    const h: Record<string, string> = { accept: headers?.accept ?? "*/*" };
    if (headers?.["content-type"]) h["content-type"] = headers["content-type"];
    let lastError: unknown;
    let wait = 0;
    for (let attempt = 0; attempt < 3; attempt++) {
      if (attempt > 0) await new Promise((r) => setTimeout(r, Math.max(wait, this.retryDelays[attempt - 1] ?? 0)));
      wait = 0;
      try {
        let next = new URL(url);
        let verb = method;
        let res: Response;
        for (let hop = 0; ; hop++) {
          const sent = { ...h };
          const reads = verb === "GET" || verb === "HEAD";
          if (verb !== method) delete sent["content-type"];
          // The page's metadata is part of what it signed (a service may check who signs, e.g. the marketplace's `signer`).
          if (this.signer && headers?.["x-identity-auth-chain-0"] && learnable(next)) Object.assign(sent, await this.signer(verb, next.pathname, headers["x-identity-metadata"] ?? "{}"));
          res = await fetch(hop === 0 ? url : next.href, { method: verb, headers: sent, body: reads || !body ? undefined : new Uint8Array(body), redirect: "manual", signal: AbortSignal.timeout(30_000) });
          const location = res.status >= 300 && res.status < 400 ? res.headers.get("location") : null;
          if (!location) break;
          await res.body?.cancel();
          const target = new URL(location, next);
          const refused = target.protocol !== "https:" ? `redirects to ${target.origin}, which is not https` : !ORIGIN_HOSTS.has(target.host) ? `redirects to ${target.host}, a host lib/services.mts ORIGINS does not list` : hop + 1 === REDIRECT_HOPS ? `redirects more than ${REDIRECT_HOPS} times` : null;
          if (refused) {
            this.failures.push(`${method} ${service} ${path}: ${refused}; not followed`);
            return { status: 502, contentType: "application/json", body: Buffer.from(JSON.stringify({ error: refused })) };
          }
          // As a browser follows it: a 303, or a POST answered 301 or 302, is read again with GET.
          if (res.status === 303 ? verb !== "HEAD" : (res.status === 301 || res.status === 302) && verb === "POST") verb = "GET";
          next = target;
        }
        const answer = { status: res.status, contentType: res.headers.get("content-type") ?? "application/octet-stream", body: Buffer.from(await res.arrayBuffer()) };
        // A server error is the service's moment, not its answer: ask again, and the caller keeps none.
        if (answer.status < 500 || attempt === 2) return answer;
        // A Retry-After in seconds is honoured, up to ten.
        const after = Number(res.headers.get("retry-after"));
        if (after > 0) wait = Math.min(after, 10) * 1000;
      } catch (err) {
        lastError = err;
      }
    }
    throw lastError;
  }

  private store(e: Omit<Entry, "body">, body: Buffer): Entry {
    const ext = /json/.test(e.contentType) ? "json" : /svg/.test(e.contentType) ? "svg" : /css/.test(e.contentType) ? "css" : /font|woff/.test(e.contentType) ? "woff2" : /html/.test(e.contentType) ? "html" : /javascript/.test(e.contentType) ? "js" : "bin";
    const name = `${sha(body).slice(0, 24)}.${ext}`;
    mkdirSync(join(this.dir, "bodies"), { recursive: true });
    writeFileSync(join(this.dir, "bodies", name), body);
    const entry = { ...e, body: name };
    this.entries.set(e.key, entry);
    return entry;
  }

  /**
   * A set is shared: another recorder may have saved since this one loaded it. Keep the answers and image sizes it
   * saved that this one lacks, so a save does not drop them.
   */
  private mergeSaved() {
    const index = join(this.dir, "index.json");
    if (existsSync(index)) for (const e of JSON.parse(readFileSync(index, "utf8")) as Entry[]) if (!this.entries.has(e.key)) this.entries.set(e.key, e);
    const images = join(this.dir, "images.json");
    if (existsSync(images)) for (const [k, v] of Object.entries(JSON.parse(readFileSync(images, "utf8")))) if (!this.images.has(k)) this.images.set(k, v as { width: number; height: number });
  }

  /** The targets another recorder saved since this one loaded the set (keys this one did not start with). */
  private othersTargets(): Record<string, string> {
    const file = join(this.dir, "manifest.json");
    if (!existsSync(file)) return {};
    const saved = (JSON.parse(readFileSync(file, "utf8")) as Manifest).targets ?? {};
    return Object.fromEntries(Object.entries(saved).filter(([k]) => !this.loadedTargets.has(k)));
  }

  save(manifest?: Partial<Manifest>) {
    mkdirSync(this.dir, { recursive: true });
    locked(this.dir, this.lockWaitMs, () => {
      this.mergeSaved();
      const list = [...this.entries.values()].sort((a, b) => (a.key < b.key ? -1 : 1));
      writeWhole(join(this.dir, "index.json"), `${JSON.stringify(list, null, 1)}\n`);
      const images = Object.fromEntries([...this.images.entries()].sort(([a], [b]) => (a < b ? -1 : 1)));
      writeWhole(join(this.dir, "images.json"), `${JSON.stringify(images, null, 1)}\n`);
      if (manifest) {
        const targets = manifest.targets && { ...this.othersTargets(), ...manifest.targets };
        this.manifest = { ...(this.manifest ?? ({} as Manifest)), ...manifest, ...(targets && { targets }), entries: list.length } as Manifest;
        writeWhole(join(this.dir, "manifest.json"), `${JSON.stringify(this.manifest, null, 2)}\n`);
      }
    });
  }

  private async rpc(path: string, body: Buffer | undefined, side: string): Promise<Answer> {
    const parsed = parseBody(body);
    const calls = (Array.isArray(parsed) ? parsed : [parsed]) as (RpcCall | null | undefined)[];
    const out: unknown[] = [];
    for (const call of calls) {
      if (typeof call?.method !== "string") {
        // Not a JSON-RPC call (an empty body, say): there is nothing to look up, but the page asked.
        this.misses.push({ key: entryKey("POST", "rpc", path, "", call ?? null), service: "rpc", method: "POST", path, query: "", side });
        out.push({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "not a JSON-RPC call" } });
        continue;
      }
      const request = { method: call.method, params: call.params ?? [] };
      const key = entryKey("POST", "rpc", path, "", request);
      let entry = this.entries.get(key);
      if (!entry && this.live) {
        const res = await this.fetchLive("POST", "rpc", path, "", Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: 1, ...request })), { "content-type": "application/json", accept: "application/json" });
        if (res.status >= 500) {
          // Kept nowhere: listed, and a replay misses it, so --fill asks again.
          this.failures.push(`POST rpc ${path} ${call.method}: answered ${res.status}`);
          out.push({ jsonrpc: "2.0", id: call.id ?? null, error: { code: -32000, message: `upstream answered ${res.status}` } });
          continue;
        }
        const answer = JSON.parse(res.body.toString("utf8")) as Record<string, unknown>;
        const result = Buffer.from(JSON.stringify("error" in answer ? { error: answer.error } : { result: answer.result }));
        entry = this.store({ key, method: "POST", service: "rpc", path, query: "", request, status: 200, contentType: "application/json" }, result);
      }
      if (!entry) {
        this.misses.push({ key, service: "rpc", method: "POST", path, query: "", side, request });
        out.push({ jsonrpc: "2.0", id: call.id ?? null, error: { code: -32000, message: "not recorded" } });
        continue;
      }
      this.note(key, side);
      out.push({ jsonrpc: "2.0", id: call.id ?? null, ...(JSON.parse(this.body(entry).toString("utf8")) as object) });
    }
    return { status: 200, contentType: "application/json", body: Buffer.from(JSON.stringify(Array.isArray(parsed) ? out : out[0])) };
  }

  private fromBatches(method: string, service: Service, path: string, query: string, request: unknown): Answer | null {
    const rule = BATCHED.find((b) => b.service === service && b.method === method && b.path === path);
    if (!rule) return null;
    const ids = rule.ids(new URLSearchParams(query), request);
    if (!ids.length) return null;
    const rest = [...new URLSearchParams(query)].filter(([k]) => k !== "id" && k !== "itemId" && k !== "first");
    const byId = new Map<string, unknown>();
    const asked = new Set<string>();
    for (const e of this.entries.values()) {
      if (e.service !== service || e.path !== path || e.method !== method || e.status !== 200) continue;
      // Other filters must agree; only the id list and its page size may differ.
      const eRest = [...new URLSearchParams(e.query)].filter(([k]) => k !== "id" && k !== "itemId" && k !== "first");
      if (JSON.stringify(eRest) !== JSON.stringify(rest)) continue;
      for (const id of rule.ids(new URLSearchParams(e.query), e.request)) asked.add(id);
      try {
        for (const row of rule.rows(JSON.parse(this.body(e).toString("utf8")))) {
          const k = rule.key(row);
          if (k && !byId.has(k)) byId.set(k, row);
        }
      } catch {
        // not JSON
      }
    }
    // Every id must have been asked while recording: answered, or known to be absent.
    if (ids.some((id) => !asked.has(id))) return null;
    const found = ids.map((id) => byId.get(id)).filter((r) => r !== undefined);
    return { status: 200, contentType: "application/json", body: Buffer.from(JSON.stringify(rule.wrap(found))) };
  }

  /** Answer one upstream request from the recording (or, in record mode, from the live service). */
  async answer(req: { method: string; service: Service; path: string; search: string; body?: Buffer; headers?: Record<string, string>; side: string }): Promise<Answer> {
    const method = req.method.toUpperCase();
    if (req.service === "rpc" && method === "POST") return this.rpc(req.path, req.body, req.side);
    const query = req.service === "cms" ? cmsQuery(req.search) : canonicalQuery(req.search);
    const request = method === "GET" || method === "HEAD" ? undefined : parseBody(req.body);
    const key = entryKey(method, req.service, req.path, query, request);
    let entry = this.entries.get(key);
    // A server error a recording made before such errors were refused kept: record mode asks again.
    if (entry && this.live && entry.status >= 500) entry = undefined;
    if (!entry && this.live && req.service === "offers" && this.synthesize) {
      const res = await this.synthesize({ method, service: req.service, path: req.path, query, request });
      if (res) entry = this.store({ key, method, service: req.service, path: req.path, query, ...(request === undefined ? {} : { request }), status: res.status, contentType: res.contentType }, res.body);
    }
    // Recording reads production: a request a page signed goes out (signed again) only when it reads. A
    // signed write would change the test wallet's state, so it is never sent; it is listed and missed.
    if (!entry && this.live && req.headers?.["x-identity-auth-chain-0"] && method !== "GET" && method !== "HEAD") {
      this.failures.push(`${method} ${req.service} ${req.path}: a signed write, never sent while recording`);
      return { status: 403, contentType: "application/json", body: Buffer.from(JSON.stringify({ ok: false, error: "a signed write is never sent while recording" })) };
    }
    if (!entry && this.live && req.service !== "offers" && req.service !== "node") {
      const res = await this.fetchLive(method, req.service, req.path, query, req.body, req.headers);
      // A twemoji flag is a fixed interface asset (an SVG of a few hundred bytes), not a content picture: kept as it is.
      if (/^image\//.test(res.contentType) && req.service !== "twemoji") {
        // A picture answered by an API route: keep its size only, never its bytes.
        const size = imageSize(res.body);
        if (size) this.images.set(req.path, size);
        return { status: 200, contentType: "image/png", body: placeholderPng(req.path, size?.width, size?.height) };
      }
      // Not kept: listed, and a replay misses it, so --fill asks again.
      if (res.status >= 500) {
        this.failures.push(`${method} ${req.service} ${req.path}: answered ${res.status}`);
        return res;
      }
      entry = this.store({ key, method, service: req.service, path: req.path, query, ...(request === undefined ? {} : { request }), status: res.status, contentType: res.contentType }, res.body);
    }
    if (!entry || oneRowBatch(method, req.service, req.path)) {
      const batched = this.fromBatches(method, req.service, req.path, query, request);
      if (batched) {
        this.note(`${method} ${req.service} ${req.path} (by id)`, req.side);
        return batched;
      }
    }
    if (!entry && this.images.has(req.path)) {
      const size = this.images.get(req.path)!;
      return { status: 200, contentType: "image/png", body: placeholderPng(req.path, size.width, size.height) };
    }
    if (!entry) {
      const signed = req.headers?.["x-identity-auth-chain-0"] ? (req.headers["x-identity-metadata"] ?? "{}") : undefined;
      this.misses.push({ key, service: req.service, method, path: req.path, query, side: req.side, ...(request === undefined ? {} : { request }), ...(signed === undefined ? {} : { signed }) });
      if (request !== undefined && req.body?.length) this.missBodies.set(key, { body: req.body, contentType: req.headers?.["content-type"] });
      return { status: 404, contentType: "application/json", body: Buffer.from(JSON.stringify({ ok: false, error: "not in the parity recording", key })) };
    }
    this.note(key, req.side);
    return { status: entry.status, contentType: entry.contentType, body: this.body(entry) };
  }
}

/**
 * Run `work` against a live recording and save what it read, with its manifest, even when `work`
 * throws or the run is interrupted (Ctrl+C): a crash midway must not cost the answers already on
 * disk. Until `work` finishes the manifest says `incomplete`, so a capture does not take it as done.
 */
export async function savingAfter<T>(live: Recording, work: () => Promise<T>, manifest: () => Partial<Manifest>): Promise<T> {
  // Registered before the browser starts, so it runs before playwright's own handler exits the process.
  const interrupted = () => {
    live.save({ ...manifest(), incomplete: true });
    process.exit(130);
  };
  process.once("SIGINT", interrupted);
  try {
    const result = await work();
    live.save({ ...manifest(), incomplete: undefined });
    return result;
  } catch (err) {
    live.save({ ...manifest(), incomplete: true });
    throw err;
  } finally {
    process.off("SIGINT", interrupted);
  }
}

/**
 * Read each request a replay could not answer once from production into `live`, sending a POST's
 * body as it was. One failed read does not stop the rest, and what was read is saved either way.
 */
export async function fillMisses(replay: Recording, live: Recording, log: (line: string) => void = console.log): Promise<{ filled: number; failed: string[] }> {
  const seen = new Set<string>();
  const failed: string[] = [];
  let filled = 0;
  try {
    for (const m of replay.misses) {
      if (m.service === "node" || m.service === "offers" || seen.has(m.key)) continue;
      seen.add(m.key);
      const sent = m.service === "rpc" ? { body: Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: 1, ...(m.request as object) })), contentType: "application/json" } : replay.missBodies.get(m.key);
      if (m.request !== undefined && !sent) {
        failed.push(`${m.method} ${m.service} ${m.path}: its body was not kept`);
        continue;
      }
      if (m.service === "rpc" && m.request === undefined) {
        failed.push(`${m.method} ${m.service} ${m.path}: not a JSON-RPC call`);
        continue;
      }
      try {
        const before = live.failures.length;
        // A request the page signed goes out signed again (the live recording's signer, at the real time),
        // or not at all: sent unsigned, its refusal would be kept as the answer.
        if (m.signed !== undefined && !live.canSign) {
          failed.push(`${m.method} ${m.service} ${m.path}: signed by the page, and no test wallet key to sign it again`);
          continue;
        }
        const signedBy: Record<string, string> = m.signed === undefined ? {} : { "x-identity-auth-chain-0": "page", "x-identity-metadata": m.signed };
        const answer = await live.answer({ method: m.method, service: m.service, path: m.path, search: m.query, body: sent?.body, headers: { "content-type": sent?.contentType ?? "application/json", accept: "application/json", ...signedBy }, side: "fill" });
        // A server error (an rpc one is answered 200 with a JSON-RPC error) was not kept.
        if (live.failures.length > before) {
          failed.push(...live.failures.slice(before));
          continue;
        }
        log(`filled ${m.method} ${m.service} ${m.path} -> ${answer.status}`);
        filled++;
      } catch (err) {
        failed.push(`${m.method} ${m.service} ${m.path}: ${String(err)}`);
      }
    }
  } finally {
    live.save();
  }
  return { filled, failed };
}
