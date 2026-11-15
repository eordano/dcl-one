// Serve the built apps/sites tree in production mode on a loopback origin, with every API mount the
// browser reads proxied to one upstream (the capture's replay). The process connects to loopback only.
//
//   node sites-preview.mts --base http://127.0.0.1:<port> --upstream http://127.0.0.1:<port>
import { createRequire } from "node:module";
import { request } from "node:http";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { foreignRequest, guardLoopback } from "./lib/guard.mts";
import { SITES } from "./lib/paths.mts";
import { relaysToNode } from "./lib/services.mts";

const arg = (name: string): string => {
  const i = process.argv.indexOf(name);
  if (i < 0 || !process.argv[i + 1]) throw new Error(`${name} is required`);
  return process.argv[i + 1]!;
};
const base = new URL(arg("--base"));
const upstream = new URL(arg("--upstream"));
for (const url of [base, upstream]) if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port) throw new Error(`use an explicit HTTP loopback origin, not ${url.href}`);

// The browser's clock is fixed at the recording's time; so is the server-side render's, or anything it
// computes from the date drifts as the recording ages and between the two sides of one run.
const now = Number(process.env.SHOP_PARITY_NOW);
if (now) {
  const RealDate = Date;
  globalThis.Date = new Proxy(RealDate, {
    construct: (target, args, newTarget) => Reflect.construct(target, args.length ? args : [now], newTarget),
    apply: () => new RealDate(now).toString(),
    get: (target, key, receiver) => (key === "now" ? () => now : Reflect.get(target, key, receiver)),
  });
}
// The server-side render must not read the network: its loaders follow CATALYST_URL to the replay, and
// a module that names a production host of its own is refused here and listed in the run.
const blocked: string[] = [];
guardLoopback((to) => {
  blocked.push(to);
  console.error(`sites preview: blocked a server-side connection to ${to}`);
});

// The same seeded sequence the browser gets (lib/browser.mts), restarted for every render as the browser's
// is for every page, so a server-side shuffle does not depend on what rendered before it.
const seed = Number(process.env.SHOP_PARITY_SEED);
let reseed = () => {};
if (process.env.SHOP_PARITY_SEED && Number.isInteger(seed)) {
  let s = seed >>> 0;
  reseed = () => {
    s = seed >>> 0;
  };
  Math.random = () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

process.env.NODE_ENV = "production";
process.env.CATALYST_URL = upstream.origin;
process.chdir(SITES);
const serverRequire = createRequire(createRequire(resolve(SITES, "package.json")).resolve("@react-router/serve/package.json"));
const express = serverRequire("express");
const compression = serverRequire("compression");
const { createRequestHandler } = serverRequire("@react-router/express");
const build = await import(pathToFileURL(resolve(SITES, "build/server/index.js")).href);

const app = express();
app.disable("x-powered-by");
// Another site open in the developer's browser must not drive the preview: while recording, its renders
// and relays read production through the replay and write the recording. The harness's own pages are
// same-origin, and its navigations carry no site (lib/guard.mts foreignRequest).
app.use((incoming: any, outgoing: any, next: () => void) => {
  if (foreignRequest(incoming.headers, base)) return outgoing.status(403).end();
  next();
});
app.use(compression());
// One render at a time, each from a fresh seed: two renders sharing Math.random would interleave draws.
let rendering = Promise.resolve();
// The hosts refused since the last call, once every render queued before it has ended (lib/run.mts).
app.get("/__shop-parity/blocked", (_incoming: unknown, outgoing: any) => {
  void rendering.then(() => outgoing.json(blocked.splice(0)));
});
app.use((incoming: any, outgoing: any, next: () => void) => {
  if (!relaysToNode(incoming.path)) return next();
  // From the path only: an absolute-form request line must not make the preview relay to another host.
  const target = new URL(incoming.originalUrl.startsWith("/") ? incoming.originalUrl : "/", upstream);
  // The replay answers no browser (lib/servers.mts fromBrowser): what names the page's site stays here.
  const headers = Object.fromEntries(Object.entries(incoming.headers).filter(([k]) => !/^(origin|referer|cookie|sec-fetch-.*)$/.test(k)));
  const req = request(target, { method: incoming.method, headers: { ...headers, host: target.host, "accept-encoding": "identity" } }, (res) => {
    outgoing.status(res.statusCode ?? 502);
    for (const [key, value] of Object.entries(res.headers)) if (value && !["transfer-encoding", "connection"].includes(key)) outgoing.setHeader(key, value);
    res.on("error", () => outgoing.destroy());
    res.pipe(outgoing);
  });
  req.setTimeout(30_000, () => req.destroy(new Error("upstream timeout")));
  req.on("error", () => (outgoing.headersSent ? outgoing.destroy() : outgoing.status(502).end()));
  outgoing.on("close", () => req.destroy());
  incoming.pipe(req);
});
app.use("/assets", express.static(resolve(SITES, "build/client/assets"), { immutable: true, maxAge: "1y" }));
app.use(express.static(resolve(SITES, "build/client")));
app.use(express.static(resolve(SITES, "public"), { maxAge: "1h" }));
const render = createRequestHandler({ build, mode: "production" });
app.all("/{*splat}", (incoming: unknown, outgoing: unknown, next: (err?: unknown) => void) => {
  // The handler settles once the response is written, or at once for a client already gone; an aborted
  // render's loaders are done drawing by then too.
  rendering = rendering
    .then(async () => {
      reseed();
      await render(incoming, outgoing, next);
    })
    .catch(next);
});
const fail = (error: Error) => {
  console.error(`sites preview failed: ${error.message}`);
  process.exit(1);
};
const server = app.listen(Number(base.port), base.hostname, (error?: Error) => (error ? fail(error) : console.log(`sites preview: ${base.origin}`)));
server.on("error", fail);
// Each stop names its cause, so a capture that loses its preview mid-run can tell which one it was.
const stop = (why: string) => {
  console.error(`sites preview: stopped (${why})`);
  process.exit(0);
};
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) process.once(signal, () => stop(signal));
// The capture that started this preview owns it: stop when its pipe closes.
process.stdin.on("end", () => stop("the capture's pipe closed")).resume();
