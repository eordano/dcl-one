import { spawn, spawnSync } from 'node:child_process'
import { createReadStream, existsSync, readFileSync, statSync } from 'node:fs'
import { createConnection } from 'node:net'
import { extname, isAbsolute, join, normalize, relative, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineConfig, searchForWorkspaceRoot, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'
import { INTER_FILES, SHARED_ALIASES, SHARED_DEDUPE, SHARED_TREES } from './vite.shared'

// Dev-only: run the bridge scene's live preview (`sdk-commands start` on :8100 — the engine's
// default systemScene in dev, with scene hot-reload) alongside vite, so `npm run dev` is the ONE
// command. If :8100 is already serving (your own terminal, or Playwright's webServer), leave it
// alone. The child outlives vite's restarts and is stopped when the dev server's process ends, with
// sdk-commands' own children (a detached group on POSIX, taskkill's tree on Windows), so none
// survive as orphans. BRIDGE_SCENE_PREVIEW=0 disables the auto-start entirely — Playwright
// sets it because it runs :8100 as its own webServer, and a child spawned from here would outlive
// its SIGKILL of the vite process group (own group, inherited stdio) and hang the test run's exit.
function bridgeScenePreview(): Plugin {
  return {
    name: 'bridge-scene-preview',
    apply: 'serve',
    configureServer() {
      if (process.env.BRIDGE_SCENE_PREVIEW === '0') return
      const probe = createConnection({ port: 8100, host: '127.0.0.1' })
      probe.once('connect', () => probe.destroy()) // already running — reuse it
      probe.once('error', () => {
        console.log('[bridge-scene] starting live preview on :8100')
        // Run the locally-installed @dcl/sdk-commands bin with this node, not through npx: npx would
        // fetch the UNRELATED registry package "sdk-commands" when bridge-scene/node_modules is stale,
        // and on Windows npx is npx.cmd, which Node starts only through a shell.
        const cwd = fileURLToPath(new URL('./bridge-scene', import.meta.url))
        // A throw here would stop vite: this runs in the probe's error handler.
        const pkgDir = join(cwd, 'node_modules/@dcl/sdk-commands')
        let script: string
        try {
          const { bin } = JSON.parse(readFileSync(join(pkgDir, 'package.json'), 'utf8')) as {
            bin?: string | Record<string, string>
          }
          const path = typeof bin === 'string' ? bin : bin?.['sdk-commands']
          if (!path) throw new Error('package.json names no sdk-commands bin')
          script = join(pkgDir, path)
        } catch (error) {
          console.warn(`[bridge-scene] cannot start @dcl/sdk-commands (run pnpm install in react-web/bridge-scene): ${error}`)
          return
        }
        // Windows has no process group to signal, so the tree is stopped with taskkill.
        const windows = process.platform === 'win32'
        const child = spawn(process.execPath, [script, 'start', '--no-client', '--port', '8100'], {
          cwd,
          stdio: ['ignore', 'inherit', 'inherit'],
          detached: !windows
        })
        // Runs at process exit, not at the server's close: a restart (an edit to this file, `r`)
        // builds the new server before it closes this one, so the new server's probe finds this
        // preview on :8100 and reuses it, and stopping it at close would leave the dev loop without
        // one. On Windows, once the child is gone its pid may belong to another process, which
        // taskkill /T /F would kill with its tree. A POSIX process group is not reused while a
        // member lives, so its survivors are still stopped.
        let stopped = false
        const stop = (): void => {
          if (stopped || child.pid == null) return
          if (windows && (child.exitCode !== null || child.signalCode !== null)) return
          stopped = true
          try {
            if (windows) spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' })
            else process.kill(-child.pid, 'SIGTERM')
          } catch {
            /* already gone */
          }
        }
        process.once('exit', stop)
        // Node runs no exit listener when SIGINT or SIGHUP ends it, and vite handles only SIGTERM.
        // Ctrl+C would then leave the POSIX child running: its own group is out of the terminal's reach.
        process.once('SIGINT', () => process.exit(130))
        process.once('SIGHUP', () => process.exit(129))
      })
    }
  }
}

// Cross-origin isolation is REQUIRED by the engine (SharedArrayBuffer + WebGPU).
const crossOriginIsolation = {
  'Cross-Origin-Opener-Policy': 'same-origin',
  // credentialless (not require-corp): the engine pulls scene assets from
  // cross-origin catalyst servers that don't send CORP. credentialless still
  // yields a cross-origin-isolated context (SharedArrayBuffer works) but loads
  // those no-CORP resources without credentials instead of blocking them. This
  // matches what the engine's own service worker sets.
  'Cross-Origin-Embedder-Policy': 'credentialless',
  'Cross-Origin-Resource-Policy': 'cross-origin'
}

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.wasm': 'application/wasm',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.svg': 'image/svg+xml',
  '.css': 'text/css; charset=utf-8',
  '.bin': 'application/octet-stream',
  '.data': 'application/octet-stream'
}

// Serve a locally-built static directory same-origin under a URL prefix, with
// the COOP/COEP headers WebGPU + wasm threads require. Used to host the engine
// bundle (../deploy/web) and the built bridge scene in a same-origin iframe —
// no npm download needed.
export function serveStatic(prefix: string, dirFromConfig: string): Plugin {
  // an absolute directory as is: `new URL` reads a Windows drive letter as a URL scheme
  const root = isAbsolute(dirFromConfig) ? dirFromConfig : fileURLToPath(new URL(dirFromConfig, import.meta.url))
  return {
    name: `serve-static:${prefix}`,
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (!req.url || !req.url.startsWith(prefix)) return next()
        const rel = decodeURIComponent(req.url.slice(prefix.length).split('?')[0])
        let file = normalize(join(root, rel))
        // relative(), not a '/' prefix check: on Windows the joined path uses '\'
        const inside = relative(root, file)
        if (inside === '..' || inside.startsWith(`..${sep}`) || isAbsolute(inside)) {
          res.statusCode = 403
          return res.end('forbidden')
        }
        try {
          if (statSync(file).isDirectory()) file = join(file, 'index.html')
        } catch {
          /* fall through */
        }
        let stat
        try {
          stat = statSync(file)
        } catch {
          res.statusCode = 404
          return res.end('not found')
        }
        for (const [k, v] of Object.entries(crossOriginIsolation)) res.setHeader(k, v)
        res.setHeader('Content-Type', MIME[extname(file).toLowerCase()] ?? 'application/octet-stream')
        res.setHeader('Content-Length', stat.size)
        createReadStream(file).pipe(res)
      })
    }
  }
}

// Dev-only: the scene editor package (a dcl-editor checkout's packages/web/dist, named by
// WEB_EDITOR_DIR) served same-origin under /editor/, where `?editor` loads it from on localhost
// (library/ui/apps/hud/features/editorHost). Without the variable nothing is mounted.
function editorPackage(): Plugin[] {
  const checkout = process.env.WEB_EDITOR_DIR
  if (!checkout) return []
  const dist = join(checkout, 'packages/web/dist')
  if (!existsSync(join(dist, 'editor.js'))) console.warn(`[editor] ${dist}/editor.js is missing: build the editor's web package`)
  return [serveStatic('/editor/', dist)]
}

// Apply the cross-origin-isolation headers the engine needs to every response EXCEPT the
// proxied auth dapp (/auth). The auth site is a normal web page that signs in with popups /
// OAuth redirects, which `Cross-Origin-Opener-Policy: same-origin` would break — and it does
// not need SharedArrayBuffer. Excluding it lets local sign-in work while the engine stays
// isolated. (In production the app and /auth are genuinely same-origin and the deploy host
// sets headers per-path; this only governs the dev server.)
//
// The two pages are the exception the other way: the app (app.html) isolates itself with
// Document-Isolation-Policy, which puts it in a process of its own, and the shell (index.html) that
// embeds it must NOT be isolated, or the app would share its process. The service worker does the
// same for prod (deploy/web/service_worker.js).
function coiHeadersExceptAuth(): Plugin {
  return {
    name: 'coi-headers-except-auth',
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const path = req.url?.split('?')[0]
        // the scene server's frame (engine/headless.html) takes the app's policy, as in the worker
        if (path === '/app.html' || path === '/engine/headless.html') {
          res.setHeader('Document-Isolation-Policy', 'isolate-and-credentialless')
        } else if (path !== '/' && path !== '/index.html' && !req.url?.startsWith('/auth')) {
          for (const [k, v] of Object.entries(crossOriginIsolation)) res.setHeader(k, v)
        }
        next()
      })
    }
  }
}

export default defineConfig(({ command, mode }) => ({
  // ?native=1 (CEF HUD mode) is honoured only where it's legitimate: the bundle:native build
  // (--mode native) that CEF loads from disk, and the dev server (react_hud_cef.rs supports
  // REACT_HUD_URL at a vite dev server). Deployed web builds compile it FALSE so a crafted
  // link can't switch them into native mode and skip the launch gates (see src/App.tsx MODE).
  define: { __NATIVE_HUD__: JSON.stringify(command === 'serve' || mode === 'native') },
  // Production is served from a VERSIONED CDN subpath (cdn.decentraland.org/<pkg>/<version>/ —
  // see deploy/web/scripts/prebuild.js), so built asset URLs can't be origin-absolute. CI passes
  // PUBLIC_URL for an absolute CDN base; otherwise './' (relative → works from any path, e.g. a
  // local `serve deploy/web`). Dev keeps '/'.
  base: command === 'build' ? (process.env.PUBLIC_URL ? `${process.env.PUBLIC_URL}/` : './') : '/',
  // The editor host's signing script's own deps: it is outside the page's import graph, so they
  // would be found when it first loads, and a late find reloads the page.
  optimizeDeps: { include: ['@noble/curves/secp256k1.js', '@noble/hashes/sha3.js'] },
  resolve: { alias: SHARED_ALIASES, dedupe: SHARED_DEDUPE },
  plugins: [
    react(),
    bridgeScenePreview(),
    coiHeadersExceptAuth(),
    serveStatic('/engine/', '../deploy/web/engine'),
    // The IPFS-cache service worker, at the ROOT so its scope covers the page (like prod). The
    // engine tags cacheable asset fetches with an X-IPFS header for it; without a SW to strip
    // that header the requests need a CORS preflight, which the peer catalysts reject — no
    // scenes, no avatars. (serveStatic handles a single file fine: rel resolves to the root.)
    serveStatic('/service_worker.js', '../deploy/web/service_worker.js'),
    // importScripts'd by the service worker (the editor's preview realm), so it sits beside it.
    serveStatic('/preview_realm.js', '../deploy/web/preview_realm.js'),
    // Our headless super-user bridge scene (exported deployable). Pointed at by
    // the engine's systemScene so it loads as the trusted --system-scene scene.
    serveStatic('/bridge-scene/static/', './bridge-scene/static'),
    ...editorPackage()
  ],
  build: {
    // The app IS the production page: build straight into the npm-published deploy/web tree,
    // beside engine/ + bridge-scene/ (emptyOutDir would wipe them).
    outDir: '../deploy/web',
    emptyOutDir: false,
    // The only chunk over the default 500KB is the isolated emoji dataset (a cached data blob,
    // not executable HUD code). Raise the limit so the build stays clean; revisit if a CODE chunk
    // approaches it.
    chunkSizeWarningLimit: 600,
    rollupOptions: {
      // index.html is the web shell that embeds app.html; native (CEF) loads app.html directly.
      input: {
        app: fileURLToPath(new URL('./app.html', import.meta.url)),
        ...(mode === 'native' ? {} : { index: fileURLToPath(new URL('./index.html', import.meta.url)) })
      },
      output: {
        // Split the single ~880KB bundle so chunks download in parallel and cache
        // independently (vendor + design system rarely change). The heavy full-screen
        // menu pages are grouped together and kept out of the core HUD chunk.
        codeSplitting: {
          groups: [
            // The shell (index.html): its own code, and what it shares with the HUD. Ahead of the
            // HUD's groups, which would otherwise take these along with the modules that import them.
            { name: 'crypto', test: /node_modules\/@noble\//, priority: 1 },
            { name: 'shell', test: /\/(src|apps\/hud|explorer-core)\/(lib\/(shell|prefs)|features\/auth\/(sso|guest|thirdweb))\.ts/, priority: 1 },
            // The host module the shared HUD reads (`@hud-host`, src/host.ts) and what it reads, and the
            // signer the shell and the editor share: the shell page needs them, so they go with it rather
            // than with the first HUD chunk that imports them.
            { name: 'shell', test: /\/src\/(host|lib\/(baseDomain|bootMode|entryParams|loopbackHost|publicUrl))\.ts|\/(apps\/hud|explorer-core)\/(engine\/generated\/(serviceTable|webParamTable)|shell\/sign)\.ts/, priority: 1 },
            {
              name(id) {
                if (id.includes('node_modules')) return 'vendor'
                // The emoji dataset is ~716KB (78KB gz) — over half the JS. Pin it to its own chunk so
                // it caches independently and never bloats/busts the core HUD chunk. (Deferring it fully
                // behind the chat picker/autocomplete is a follow-up — see emojiData.ts.)
                if (id.includes('emojis_complete.json') || id.includes('/chat/emojiData')) return 'emoji'
                // Showcase is dev-only (?showcase=1) and lazy-loaded — leave it out of the design
                // chunk so its lazy boundary survives and it never ships in the prod HUD path.
                // The shared HUD (library/ui/apps/hud, library/explorer-core) is split the same way as this app's own sources.
                if (/\/(src|apps\/hud|explorer-core)\/design\//.test(id) && !id.includes('Showcase')) return 'design'
                if (/\/(src|apps\/hud|explorer-core)\/features\/(map|backpack|communities|gallery|places)\//.test(id)) return 'menus'
                return null
              }
            }
          ]
        }
      }
    }
  },
  server: {
    // Allow Cloudflare quick-tunnel hosts (random *.trycloudflare.com per run) so the dev server
    // can be reached through a tunnel. The leading dot matches any subdomain; scoping to this
    // domain keeps Vite's DNS-rebinding protection on for everything else.
    allowedHosts: ['.trycloudflare.com'],
    // The shared sources live outside this app (vite.shared.ts).
    fs: { allow: [searchForWorkspaceRoot(process.cwd()), ...SHARED_TREES, INTER_FILES] },
    // Proxy the auth dapp so it is served same-origin on localhost — sign in locally against
    // zone and the signed AuthIdentity lands in this origin's localStorage (the marketplace
    // does the same). Because it's same-origin, the auth site also accepts our localhost
    // `redirectTo`. Switch the target to decentraland.org/.today for other envs.
    proxy: {
      // Keep this env in sync with EngineHost REALM (currently mainnet/.org) so the account
      // you sign in as matches the realm the engine loads your profile/friends from. Switch
      // both to decentraland.zone together when testing against zone.
      '/auth': {
        target: 'https://decentraland.org',
        changeOrigin: true,
        secure: false,
        followRedirects: true,
        ws: true
      }
    }
  },
  preview: { headers: crossOriginIsolation }
}))
