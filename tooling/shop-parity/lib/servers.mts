// The three loopback servers of a capture: the reference build (static, SPA fallback), the replay
// that stands in for the node (sites SSR and its /shop-api proxy read it), and the built sites tree
// served by sites-preview.mts with that replay as its upstream.
import { waitUntil } from "../../../library/async/index.ts";
import { fetchGeneratesNoServerError, HTTP_PROBE_TIMEOUT_MS } from "../../../library/async/http.ts";
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, readFileSync, statSync } from "node:fs";
import { createServer, type IncomingMessage, type Server } from "node:http";
import { extname, join, normalize, sep } from "node:path";
import { childEnv } from "./env.mts";
import { placeholderPng } from "./png.mts";
import { SITES, TOOL } from "./paths.mts";
import type { Recording } from "./recording.mts";
import { CMS_SPACE_PATH, fromNodePath, isContentImage, NODE_TELEMETRY, OWN_IMAGE_MOUNTS, serviceForHost } from "./services.mts";

const PROCESS_STARTUP_TIMEOUT_MS = 60_000;

const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".webmanifest": "application/manifest+json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".webp": "image/webp",
  ".gif": "image/gif",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".woff": "font/woff",
  ".txt": "text/plain; charset=utf-8",
  ".xml": "application/xml",
  ".mp4": "video/mp4",
  ".json5": "application/json",
  ".wasm": "application/wasm",
  ".glb": "model/gltf-binary",
};

function listen(server: Server, port: number): Promise<Server> {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => resolve(server));
  });
}

/** A file under `dir` at `path` (its leading prefix removed), or null when there is none or the path leaves `dir`. */
function fileUnder(dir: string, path: string, prefix = "/"): string | null {
  const file = normalize(join(dir, path.slice(prefix.length)));
  return file.startsWith(dir + sep) && existsSync(file) && !statSync(file).isDirectory() ? file : null;
}

/**
 * The reference build: files as built, every other path the SPA's index.html (vercel.json's rewrite).
 * `mounts` serves other directories by URL prefix as the server's own static files (dcl-one's Inter
 * files for the sites reference, lib/fonts.mts): a file missing there is 404, never the index.
 */
export function startStatic(dist: string, port: number, mounts: Record<string, string> = {}): Promise<Server> {
  const server = createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    let path: string;
    try {
      path = decodeURIComponent(url.pathname);
    } catch {
      path = "/";
    }
    const mount = Object.entries(mounts).find(([prefix]) => path.startsWith(prefix));
    const file = mount ? fileUnder(mount[1], path, mount[0]) : fileUnder(dist, path) ?? join(dist, "index.html");
    if (!file) {
      res.writeHead(404, { "content-type": "text/plain", "cache-control": "no-store" });
      return res.end(`no ${path} under the ${mount![0]} mount`);
    }
    res.writeHead(200, { "content-type": TYPES[extname(file)] ?? "application/octet-stream", "cache-control": "no-store" });
    res.end(readFileSync(file));
  });
  return listen(server, port);
}

function readBody(req: IncomingMessage): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

/** The picture a content URL stands for, independent of which host or mount serves it. */
const CMS_IMAGE_HOSTS = new Set(["cms-images.decentraland.org", "images.ctfassets.net"]);

export function imageSeed(url: URL): string {
  // A resized copy (the node's /media/convert, Vercel's /_vercel/image) is the picture it wraps.
  const wrapped = /^\/(media\/convert|_vercel\/image)\/?$/.test(url.pathname) ? url.searchParams.get("url") : null;
  if (wrapped) return imageSeed(new URL(wrapped, url));
  const p = decodeURIComponent(url.pathname);
  // A CMS picture on the CMS's image host (/<space>/<asset>/<token>/<file>).
  const cms = CMS_IMAGE_HOSTS.has(url.host) ? p.match(/^\/[^/]+\/([A-Za-z0-9_-]+)\//) : null;
  if (cms) return `cms-asset:${cms[1]}`;
  const urn = p.match(/(urn:decentraland:[^/]+)\/(thumbnail|image)$/i);
  if (urn) return `${urn[1]!.toLowerCase()}/${urn[2]}`;
  const content = p.match(/\/contents\/([A-Za-z0-9]{32,})$/);
  if (content) return content[1]!;
  // A profile picture, whether read from the profile-images service or through the node's mount.
  const face = p.match(/\/entities\/([A-Za-z0-9]{32,})\/(face256|face|body)(\.png)?$/);
  if (face) return `profile:${face[1]}/${face[2]}`;
  const loopback = url.hostname === "127.0.0.1" || url.hostname === "localhost";
  const service = loopback ? fromNodePath("GET", p).service : serviceForHost(url.host) ?? url.host;
  return `${service}:${loopback ? fromNodePath("GET", p).path : p}`;
}

export function placeholderFor(recording: Recording, url: URL): Buffer {
  const seed = imageSeed(url);
  const size = recording.images.get(seed);
  return placeholderPng(seed, size?.width, size?.height);
}

/**
 * A request a browser made: every current browser names the requesting site, and the sites preview
 * strips these headers when it relays.
 */
export function fromBrowser(headers: IncomingMessage["headers"]): boolean {
  return headers["sec-fetch-site"] !== undefined || headers.origin !== undefined;
}

/**
 * The node as sites sees it: every request answered from the recording, pictures by placeholder. Only
 * the sites preview reads it, server to server, so it answers no browser: while recording, a website
 * open in the developer's browser could otherwise make it read production and write the recording.
 * The preview names the replay by its loopback address, so any other Host is refused too: a page on
 * a name rebound to 127.0.0.1 already sends Sec-Fetch-Site, so this is a second lock, not the only one.
 */
export function startReplay(recording: Recording, port: number): Promise<Server> {
  const server = createServer(async (req, res) => {
    try {
      const url = new URL(req.url ?? "/", "http://127.0.0.1");
      const method = (req.method ?? "GET").toUpperCase();
      if (req.headers.host !== `127.0.0.1:${port}` || fromBrowser(req.headers) || !["GET", "HEAD", "POST"].includes(method)) {
        res.writeHead(403, { "content-type": "text/plain" });
        return res.end("the parity replay answers the sites preview only");
      }
      if (isContentImage(url) && OWN_IMAGE_MOUNTS.test(url.pathname)) {
        res.writeHead(200, { "content-type": "image/png" });
        return res.end(placeholderFor(recording, url));
      }
      if (NODE_TELEMETRY.test(url.pathname)) {
        res.writeHead(204);
        return res.end();
      }
      const { service, path } = fromNodePath(method, url.pathname);
      const body = await readBody(req);
      const answer = await recording.answer({ method, service, path, search: url.search, body, headers: req.headers as Record<string, string>, side: "dcl-one" });
      res.writeHead(answer.status, { "content-type": answer.contentType });
      res.end(answer.body);
    } catch (err) {
      res.writeHead(502, { "content-type": "text/plain" });
      res.end(String(err));
    }
  });
  return listen(server, port);
}

/** The production preview of the built sites tree (sites-preview.mts), with the replay as its node. */
export async function startSites(port: number, upstream: string, env: Record<string, string> = {}): Promise<ChildProcess> {
  if (!existsSync(join(SITES, "build", "server", "index.js"))) throw new Error("apps/sites is not built: run `pnpm build` in apps/sites (or capture.mts --build)");
  const previewUrl = `http://127.0.0.1:${port}`;
  const child = spawn(process.execPath, [join(TOOL, "sites-preview.mts"), "--base", previewUrl, "--upstream", upstream], {
    // The blog reads the CMS space from the replay (`/cms/*` is the recorded cms-api), so both sides read one answer.
    env: { ...childEnv(), CATALYST_URL: upstream, PUBLIC_CATALYST_URL: "", BLOG_CMS_URL: `${upstream}/cms${CMS_SPACE_PATH}`, ...env },
    stdio: ["pipe", "pipe", "pipe"],
  });
  // Exiting without stopping the preview would leave its port taken for the next run.
  process.once("exit", () => child.kill());
  let log = "";
  child.stdout!.on("data", (c) => (log += c));
  child.stderr!.on("data", (c) => (log += c));
  try {
    await waitUntil(async (remainingMs) => {
      const childRunning = child.exitCode === null && child.signalCode === null;
      if (!childRunning) throw new Error(`sites preview exited (${child.exitCode ?? child.signalCode}):\n${log}`);
      const loggedPreviewUrl = log.includes(`sites preview: ${previewUrl}`);
      if (!loggedPreviewUrl) return;
      try {
        const ready = await fetchGeneratesNoServerError(`${previewUrl}/robots.txt`, Math.min(HTTP_PROBE_TIMEOUT_MS, remainingMs));
        return ready ? true : undefined;
      } catch {
        return;
      }
    }, { timeoutMs: PROCESS_STARTUP_TIMEOUT_MS, description: `sites preview on ${port}` });
    child.once("exit", (code, signal) => {
      if (!child.killed) console.error(`sites preview exited (${code ?? signal}) during the run:\n${log.slice(-4000)}`);
    });
    return child;
  } catch (error) {
    child.kill();
    throw new Error(`sites preview startup failed on ${port}: ${error instanceof Error ? error.message : String(error)}\n${log}`, { cause: error });
  }
}
