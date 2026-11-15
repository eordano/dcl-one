import { reactRouter } from "@react-router/dev/vite";
import { createReadStream, existsSync, readdirSync, statSync } from "node:fs";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin, type ProxyOptions } from "vite";
import { validateAliasObject } from "../ui3/vite.validate.js";

const ui = fileURLToPath(new URL("../ui3/src", import.meta.url));
const src = fileURLToPath(new URL("./src", import.meta.url));
const catalystApi = fileURLToPath(new URL("../../library/catalyst-api", import.meta.url));
const shopCore = fileURLToPath(new URL("../../library/shop", import.meta.url));
// The Explorer HUD UI /play compiles, and the behaviours /play supplies to it (hostContract.ts there).
const hud = fileURLToPath(new URL("../../library/ui/apps/hud", import.meta.url));
const explorerCore = fileURLToPath(new URL("../../library/explorer-core", import.meta.url));
const hudHost = fileURLToPath(new URL("../ui3/src/play/host.ts", import.meta.url));
// library/ui/apps/hud has no node_modules of its own: the @noble modules it imports resolve to this app's
// copies. Only these specifiers: a dependency of this app keeps an older @noble of its own. Keep this list
// to the @noble modules library/explorer-core imports; tsconfig.json maps all of @noble/* for the type check.
const noble = (path: string) => fileURLToPath(new URL(`./node_modules/@noble/${path}`, import.meta.url));

function watchUi3(ui3Src: string): Plugin {
  return {
    name: "watch-ui3",
    apply: "serve",
    configureServer(server) {
      server.watcher.add(ui3Src);
      server.watcher.on("change", (file) => {
        if (!file.startsWith(ui3Src)) return;
        const mods = server.moduleGraph.getModulesByFile(file);
        if (!mods || mods.size === 0) return;
        for (const mod of mods) void server.reloadModule(mod).catch(() => {});
      });
    },
  };
}
const monaco = fileURLToPath(new URL("./node_modules/monaco-editor", import.meta.url));
const three = fileURLToPath(new URL("./node_modules/three", import.meta.url));
const qrcode = fileURLToPath(new URL("./node_modules/qrcode-generator", import.meta.url));
const reactQuery = fileURLToPath(new URL("./node_modules/@tanstack/react-query", import.meta.url));
// `ws` is not installed (pnpm-workspace.yaml removes it): the packages that import it get the platform WebSocket.
const platformWebSocket = fileURLToPath(new URL("./src/core/lib/platform-websocket.ts", import.meta.url));

// The ONE base: the v2 node the SSR process reads. The dev proxy forwards the
// prefixes browsers call directly to the same node (nginx does this on a host).
const CATALYST_PROXY_TARGET =
  process.env.DCL_CATALYST_PROXY_TARGET || process.env.CATALYST_URL || "http://127.0.0.1:5210";

// Sites resource routes that share the /api prefix with the node's explore API,
// read from the route files (`api.<segment>.*` -> /api/<segment>) the way
// deploy/nix/sites.nix derives the nginx table, so dev and the host agree.
export function sitesApiRoutes(routesDir = fileURLToPath(new URL("./src/routes/routes", import.meta.url))): string[] {
  const segments = new Set<string>();
  for (const name of readdirSync(routesDir)) {
    const m = /^api\.([^.$_]+)/.exec(name);
    if (m && /\.(tsx?|ts)$/.test(name) && !/\.(test|spec)\.tsx?$/.test(name)) segments.add(m[1]);
  }
  return [...segments].sort().map((s) => `/api/${s}`);
}
const SITES_API_ROUTES = sitesApiRoutes();

function nodeProxy(): Record<string, ProxyOptions> {
  const to = (extra: Partial<ProxyOptions> = {}): ProxyOptions => ({
    target: CATALYST_PROXY_TARGET,
    changeOrigin: true,
    ...extra,
  });
  return {
    "/about": to(),
    "/content/": to(),
    // A world's /about names its scene files at the node's root /contents/ (as the
    // single-domain gateway serves them), so a world /play loads reads them there.
    "/contents/": to(),
    "/lambdas": to(),
    "/api/": to({
      bypass: (req) =>
        SITES_API_ROUTES.some((p) => req.url?.startsWith(p)) ? req.url : undefined,
    }),
    "/places/api/": to(),
    "/events/api/": to(),
    "/shop-api": to(),
    "/marketplace-api": to(),
    "/social-api": to(),
    "/notifications/": to(),
    "/v1/": to(),
    "/presence": to(),
    "/world-storage": to(),
    "/worlds-content-server": to(),
    "/builder-api": to(),
    "/camera-reel-service": to(),
    "/transactions-api": to(),
    // The tip forward (/play's Donate), signed by the login: the node tells the creator `tip_received`.
    "/events-notifier/": to(),
    "/price/": to(),
    "/_play": to(),
    "/auth-api": to(),
    // The NAME banned list (library/shop/names/api.ts) and the image derivatives
    // (`/media/convert`) the cards request.
    "/dcl-lists": to(),
    "/media/": to(),
    // The operator plane (src/main.rs): credits, the relay and the imposter store.
    "^/credits(?:[/?]|$)": to(),
    "/rpc": to({ ws: true }),
    "/bvimposters": to(),
    // The web Explorer (/play) reaches the rest of the gateway's prefix table
    // (services/catalyst/tests/conformance/routes.tsv) from this origin: the realm
    // provider, comms, asset bundles, feature flags, badges and the social relay.
    "/realm-provider-ea": to(),
    "/archipelago-ea-stats": to(),
    "/asset-bundle-registry": to(),
    "/ab-cdn": to(),
    "/assets-cdn": to(),
    "/config": to(),
    "/feature-flags": to(),
    "/events-assets-099ac00": to(),
    "/opensea": to(),
    "/comms-gatekeeper": to(),
    "/badges": to(),
    "/profile-images": to(),
    "/signatures-api": to(),
    "/social-rpc": to({ ws: true }),
    "/metamorph-api": to(),
    "/autotranslate-server": to(),
    "/world-manifest.json": to(),
    "/WorldManifest.json": to(),
  };
}

// Our verified web Explorer build, imported by tooling/fetch-play.py. Development
// and the production Sites image serve the same engine/bridge/worker under /play/.
const PLAY_MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".wasm": "application/wasm",
  ".wat": "text/plain; charset=utf-8",
  ".ts": "text/plain; charset=utf-8",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".css": "text/css; charset=utf-8",
};
export function playWebDir(root = fileURLToPath(new URL("../..", import.meta.url))): string {
  if (process.env.PLAY_WEB_DIR) return process.env.PLAY_WEB_DIR;
  return join(root, ".cache/play/current");
}
function servePlayWeb(): Plugin {
  const served = new Set(["engine", "bridge-scene", "service_worker.js", "preview_realm.js", "sites-play.json"]);
  return {
    name: "serve-play-web",
    apply: "serve",
    configureServer(server) {
      const dir = playWebDir();
      server.middlewares.use((req, res, next) => {
        const url = new URL(req.url ?? "/", "http://localhost");
        if (!url.pathname.startsWith("/play/")) return next();
        const rel = decodeURIComponent(url.pathname.slice("/play/".length));
        const [head] = rel.split("/");
        if (!head || !served.has(head)) return next();
        const file = normalize(join(dir, rel));
        if (!file.startsWith(normalize(dir) + "/") || !existsSync(file) || !statSync(file).isFile()) {
          if (!existsSync(dir)) {
            res.statusCode = 503;
            res.setHeader("content-type", "text/plain; charset=utf-8");
            res.end(`web Explorer artifact not imported: run python3 tooling/fetch-play.py (expected ${dir})`);
            return;
          }
          return next();
        }
        res.setHeader("content-type", PLAY_MIME[extname(file)] ?? "application/octet-stream");
        // Documents under /play/ carry their isolation from their routes; the files only need to be
        // loadable from that isolated context. The service worker scopes itself to /play/.
        res.setHeader("cross-origin-resource-policy", "same-origin");
        if (rel === "engine/headless.html") {
          // The scene server's frame takes the app's policy (app.html), which only lets documents
          // with the same policy script it; the service worker sets the same once it controls /play/.
          res.setHeader("cross-origin-opener-policy", "same-origin");
          res.setHeader("document-isolation-policy", "isolate-and-credentialless");
        }
        res.setHeader("cache-control", "no-cache");
        createReadStream(file).pipe(res);
      });
    },
  };
}

function separateDevCaches(): Plugin {
  return {
    name: "separate-sites-dev-caches",
    config(config, { command }) {
      // The live dev stack and the journey kit's server run from the same checkout.
      // Sharing optimizer output lets either one remove the other's modules.
      if (command === "serve") return { cacheDir: `node_modules/.vite-sites/${config.server?.port ?? "default"}` };
    },
  };
}

// The first SSR request imports every route module through Vite's module runner, about
// 5.5 s of transforms on this tree (METRICS.md, "Dev loop"). Pay that right after listen,
// in the background, with a path no route matches, so the first real page finds it warm.
function warmServerBuild(): Plugin {
  return {
    name: "warm-server-build",
    apply: "serve",
    configureServer(server) {
      server.httpServer?.once("listening", () => {
        const a = server.httpServer?.address();
        if (!a || typeof a === "string") return;
        const host = a.family === "IPv6" ? `[${a.address}]` : a.address;
        void fetch(`http://${host}:${a.port}/__dev-warmup`)
          .then((r) => r.body?.cancel())
          .catch(() => {});
      });
    },
  };
}

// Vite types .ico image/x-icon; the built server (@react-router/serve, express's mime db)
// answers image/vnd.microsoft.icon. The dev server follows the built one.
function icoTypeLikeServe(): Plugin {
  return {
    name: "ico-type-like-serve",
    apply: "serve",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url?.split("?")[0].endsWith(".ico")) {
          const setHeader = res.setHeader.bind(res);
          res.setHeader = (name, value) =>
            setHeader(name, name.toLowerCase() === "content-type" ? "image/vnd.microsoft.icon" : value);
        }
        next();
      });
    },
  };
}

export default defineConfig({
  plugins: [icoTypeLikeServe(), separateDevCaches(), servePlayWeb(), reactRouter(), watchUi3(ui), warmServerBuild()],
  server: {
    // A save is often truncate-then-write, and over the NFS home the two reach the
    // server's inotify as two events. Transforming on the first one served an EMPTY
    // stylesheet until the next touch. Wait for the size to settle.
    watch: { awaitWriteFinish: { stabilityThreshold: 150, pollInterval: 50 } },
    // DEV_ALLOWED_HOSTS: comma-separated extra hosts the TLS front serves the dev
    // server under (".example.org" allows the domain and every subdomain).
    allowedHosts: [
      ...(process.env.DEV_ALLOWED_HOSTS ?? "").split(",").map((h) => h.trim()).filter(Boolean),
      "localhost",
      "127.0.0.1",
    ],
    // HMR protocol and port come from the page: direct loopback loads and the TLS
    // front (which routes vite-hmr upgrades on / here) both connect.
    // PORT and HOST are the one listener contract (react-router-serve reads the
    // same names); the default is loopback v4, what the proxies and tours dial.
    host: process.env.HOST || "127.0.0.1",
    port: Number(process.env.PORT) || 5212,
    strictPort: true,
    fs: { allow: [".", "../ui3", "../../library/catalyst-api", "../../library/shop", "../../library/ui", "../../library/explorer-core"] },
    proxy: nodeProxy(),
  },
  resolve: {
    alias: {
      ...validateAliasObject(),
      "@ui": ui,
      "@dcl/one-ui/foundations.css": fileURLToPath(new URL("../../library/ui/foundations/foundations.css", import.meta.url)),
      "@dcl/one-ui": fileURLToPath(new URL("../../library/ui", import.meta.url)),
      "@catalyst-api": catalystApi,
      "@shop": shopCore,
      "@hud-host": hudHost,
      "@noble/curves/secp256k1.js": noble("curves/secp256k1.js"),
      "@noble/hashes/sha3.js": noble("hashes/sha3.js"),
      "@noble/hashes/utils.js": noble("hashes/utils.js"),
      "@hud": hud,
      "@explorer-core": explorerCore,
      "@": src,
      "monaco-editor": monaco,
      three,
      "qrcode-generator": qrcode,
      "@tanstack/react-query": reactQuery,
      ws: platformWebSocket,
    },
    dedupe: ["viem", "react", "react-dom", "zod"],
  },
  optimizeDeps: {
    include: ["monaco-editor", "@xstate/react", "xstate", "zod", "viem/accounts"],
    // The shared dev sandbox is also a public browsing surface. Compact vendor
    // code while keeping development React, named stacks and application HMR.
    rolldownOptions: { output: { minify: true, keepNames: true } },
    exclude: [
      "monaco-editor/esm/vs/editor/editor.worker?worker&url",
      "monaco-editor/esm/vs/language/typescript/ts.worker?worker&url",
    ],
  },
});
