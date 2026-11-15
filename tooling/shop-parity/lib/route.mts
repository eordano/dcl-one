// Browser-side routing of a capture: every request that leaves the side's own origin is answered
// from the recording (pictures by placeholder, telemetry blocked); nothing reaches the network.
import { existsSync, statSync } from "node:fs";
import { join, normalize } from "node:path";
import type { BrowserContext, Route } from "playwright-core";
import type { Recording } from "./recording.mts";
import { imageSeed, placeholderFor } from "./servers.mts";
import { fromNodePath, ORIGINS, resolve } from "./services.mts";
import type { Side } from "./spec.mts";

export type SideLog = { blocked: string[]; unknown: string[]; images: number };

const CORS = { "access-control-allow-origin": "*", "access-control-allow-headers": "*", "access-control-allow-methods": "*", "cache-control": "no-store" };

/** The wearable preview iframe: a blank document; its box is a declared 3D mask on both sides. */
const BLANK_PREVIEW = "<!doctype html><html><body style=\"margin:0;background:transparent\"></body></html>";

/** A file of `dir` at the path, when `dir` ships one (a reference's own static asset on its production host). */
function staticFile(dir: string | undefined, path: string | null): string | null {
  if (!dir || path === null) return null;
  const rel = normalize(decodeURIComponent(path)).replace(/^[/\\]+/, "");
  if (!rel || rel.startsWith("..")) return null;
  const file = join(dir, rel);
  return existsSync(file) && statSync(file).isFile() ? file : null;
}

/**
 * The path of a sites asset a request names: on the sites origin, or wrapped in the peer's content path by
 * decentraland-ui2's AvatarFace, which prefixes `https://peer.decentraland.org/content/contents/` to a face256
 * that is not https (ui2 src/components/AvatarFace/AvatarFace.tsx). The reference built without VITE_BASE_URL
 * names its own `/dcl-logo.svg` there (sites WhatsOnCard DCL_LOGO_URL); production's CDN URL is https, so the
 * logo itself is what the page shows. A content hash never starts with a slash, so the double slash is the mark.
 */
export function shippedPath(url: URL): string | null {
  if (url.origin === ORIGINS.sites) return url.pathname;
  if (url.origin === ORIGINS.peer) return /^\/content\/contents\/(\/.*)$/.exec(url.pathname)?.[1] ?? null;
  return null;
}

/** Set by `redecodeImages` (capture.mts) on its fetch of an image's bytes. */
export const REDECODE_HEADER = "x-shop-parity-redecode";

/**
 * `staticDir`: the build of the app that serves ORIGINS.sites (decentraland.org); a GET for a file the build
 * ships (shippedPath) is answered with that file, as production serves it, not with a picture placeholder.
 */
export async function routeSide(context: BrowserContext, recording: Recording, side: Side, origin: string, staticDir?: string): Promise<SideLog> {
  const log: SideLog = { blocked: [], unknown: [], images: 0 };
  // `route` does not see WebSockets: close every one before it connects, and list it as blocked.
  await context.routeWebSocket(/.*/, (ws) => {
    log.blocked.push(`ws:${new URL(ws.url()).host}`);
    ws.close().catch(() => {});
  });
  await context.route("**/*", async (route: Route) => {
    const request = route.request();
    const url = new URL(request.url());
    const method = request.method().toUpperCase();
    // A capture's image re-decode fetches an image's own URL, so it is routed as the image was.
    const resourceType = request.headers()[REDECODE_HEADER] ? "image" : request.resourceType();
    const r = resolve(url, method, origin, resourceType, side);
    try {
      if (r.kind === "own") return await route.continue();
      const file = method === "GET" ? staticFile(staticDir, shippedPath(url)) : null;
      if (file) return await route.fulfill({ status: 200, headers: CORS, path: file });
      if (method === "OPTIONS") return await route.fulfill({ status: 204, headers: CORS });
      if (r.kind === "blocked") {
        log.blocked.push(url.host);
        return await route.fulfill({ status: 204, headers: CORS, body: "" });
      }
      if (r.kind === "image") {
        log.images++;
        if (recording.live) {
          const own = url.origin === origin ? fromNodePath("GET", url.pathname) : null;
          const real = own ? `${ORIGINS[own.service] ?? ORIGINS.peer}${own.path}` : url.href;
          await recording.learnImage(imageSeed(url), real);
        }
        return await route.fulfill({ status: 200, headers: { ...CORS, "content-type": "image/png" }, body: placeholderFor(recording, url) });
      }
      if (r.kind === "unknown") {
        log.unknown.push(`${method} ${url.host}${url.pathname}`);
        return await route.fulfill({ status: 404, headers: CORS, body: "" });
      }
      if (r.service === "wearable-preview") return await route.fulfill({ status: 200, headers: { ...CORS, "content-type": "text/html" }, body: BLANK_PREVIEW });
      const answer = await recording.answer({
        method,
        service: r.service,
        path: r.path,
        search: url.search,
        body: request.postDataBuffer() ?? undefined,
        headers: await request.allHeaders(),
        side,
      });
      return await route.fulfill({ status: answer.status, headers: { ...CORS, "content-type": answer.contentType }, body: answer.body });
    } catch (err) {
      if (/closed|disposed/i.test(String(err))) return;
      try {
        await route.fulfill({ status: 502, headers: CORS, body: String(err) });
      } catch {
        // the page went away
      }
    }
  });
  return log;
}
