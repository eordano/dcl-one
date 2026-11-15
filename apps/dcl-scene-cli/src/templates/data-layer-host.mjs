import crypto from 'node:crypto'
import { createRequire } from 'node:module'
import http from 'node:http'
import path from 'node:path'
import fs from 'node:fs'
import { exitWithParent, inside, realpath } from './driver-common.mjs'

const projectDir = path.resolve(process.argv[2])
const mode = process.argv[3] || 'serve'
process.chdir(projectDir)

const inspectorDir = process.env.DCL_ONE_INSPECTOR_DIR || ''
const sceneRequire = createRequire(path.join(projectDir, 'package.json'))
const inspectorRequire = inspectorDir
  ? createRequire(path.join(inspectorDir, 'package.json'))
  : null

function req(name) {
  if (inspectorRequire) {
    if (name === '@dcl/inspector') {
      return inspectorRequire(inspectorDir)
    }
    try {
      return inspectorRequire(name)
    } catch {}
  }
  return sceneRequire(name)
}

function pathToPosix(v) {
  return v.replace(/\\/g, '/')
}

// The file system the editor's data layer works on: every path it names is
// resolved against the scene folder and must stay inside it, symlinks
// followed, whatever the client sends. A call gets the path that was checked,
// not the name it was checked under. A removal (`entry`) gets the entry in its
// real folder, since `rm` removes a link and not what the link points to.
function makeFsInterface(workdir) {
  const root = fs.realpathSync(workdir)
  const resolve = (p, entry = false) => {
    const named = path.resolve(workdir, p)
    const real = realpath(named)
    const used = entry ? path.join(realpath(path.dirname(named)), path.basename(named)) : real
    if (!inside(root, real) || !inside(root, used)) throw new Error(`Path ${JSON.stringify(p)} is outside the project`)
    return used
  }
  return {
    dirname: (v) => pathToPosix(path.dirname(v)),
    basename: (v) => pathToPosix(path.basename(v)),
    join: (...paths) => path.join(...paths),
    async existFile(filePath) {
      try {
        return (await fs.promises.stat(resolve(filePath))).isFile()
      } catch {
        return false
      }
    },
    async readFile(filePath) {
      return fs.promises.readFile(resolve(filePath))
    },
    async writeFile(filePath, content) {
      const resolved = resolve(filePath)
      await fs.promises.mkdir(path.dirname(resolved), { recursive: true })
      await fs.promises.writeFile(resolved, content)
    },
    async rm(filePath) {
      await fs.promises.rm(resolve(filePath, true))
    },
    async rmdir(dirPath) {
      await fs.promises.rm(resolve(dirPath, true), { recursive: true })
    },
    async readdir(dirPath) {
      const resolved = resolve(dirPath)
      const entries = await fs.promises.readdir(resolved)
      return Promise.all(
        entries.map(async (name) => {
          let isDirectory = false
          try {
            isDirectory = (await fs.promises.stat(path.resolve(resolved, name))).isDirectory()
          } catch {}
          return { name: pathToPosix(name), isDirectory }
        })
      )
    },
    cwd: () => pathToPosix(workdir),
    async stat(filePath) {
      const stats = await fs.promises.stat(resolve(filePath))
      return { size: Number(stats.size) }
    }
  }
}

function findComposites(dir, base, out) {
  let entries
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true })
  } catch {
    return out
  }
  for (const entry of entries) {
    if (entry.name.startsWith('.')) continue
    const abs = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || entry.name === 'bin' || entry.name === 'dist') continue
      findComposites(abs, base, out)
    } else if (entry.name.endsWith('.composite')) {
      out.push(pathToPosix(path.relative(base, abs)))
    }
  }
  return out
}

async function dumpCrdtFallback() {
  const inspector = req('@dcl/inspector')
  const { Composite, Engine, EntityMappingMode } = req('@dcl/ecs/dist-cjs')
  const files = findComposites(projectDir, projectDir, []).sort()
  const composites = {}
  for (const file of files) {
    const raw = await fs.promises.readFile(path.join(projectDir, file), 'utf8')
    composites[file] = Composite.fromJson(JSON.parse(raw))
  }
  const provider = {
    getCompositeOrNull(src) {
      return src in composites ? { src, composite: composites[src] } : null
    }
  }
  const engine = Engine()
  // NOTE: the crdt this produces carries every component the composites
  // define, but not the editor's own composite::root marker — the
  // @dcl/sdk-commands path sets that when its data layer opens a composite.
  // Scene content is identical; only that bookkeeping entry is absent.
  for (const src of Object.keys(composites)) {
    Composite.instance(engine, provider.getCompositeOrNull(src), provider, {
      entityMapping: {
        type: EntityMappingMode.EMM_DIRECT_MAPPING,
        getCompositeEntity: (e) => e
      }
    })
  }
  const crdt = inspector.dumpEngineToCrdtCommands(engine)
  await fs.promises.writeFile(path.join(projectDir, 'main.crdt'), crdt)
  process.stdout.write(JSON.stringify({ ok: true, composites: files.length }) + '\n')
}

async function dumpCrdt() {
  let getAllComposites
  try {
    ;({ getAllComposites } = req('@dcl/sdk-commands/dist/logic/composite.js'))
  } catch {}
  if (!getAllComposites) return dumpCrdtFallback()
  const log = (...args) => console.error(...args)
  const components = {
    fs: {
      readFile: (p) => fs.promises.readFile(p),
      writeFile: (p, data) => fs.promises.writeFile(p, data)
    },
    logger: { log, info: log, warn: log, error: log, debug: log }
  }
  const data = await getAllComposites(components, projectDir)
  process.stdout.write(
    JSON.stringify({ ok: !data.withErrors, composites: data.compositeLines.length }) + '\n'
  )
}

async function serve() {
  const admitUpgrade = admission()
  const inspector = req('@dcl/inspector')
  const { createRpcServer } = req('@dcl/rpc')
  const codegen = req('@dcl/rpc/dist/codegen')
  const { WebSocketTransport } = req('@dcl/rpc/dist/transports/WebSocket')
  const { WebSocketServer } = req('ws')

  const fsInterface = makeFsInterface(projectDir)
  const host = await inspector.createDataLayerHost(fsInterface)
  const logToStderr = (...args) => console.error(...args)
  const rpcServer = createRpcServer({
    logger: {
      log: logToStderr,
      info: logToStderr,
      warn: logToStderr,
      error: logToStderr,
      debug: logToStderr
    }
  })
  rpcServer.setHandler(async (serverPort) => {
    codegen.registerService(
      serverPort,
      inspector.DataServiceDefinition,
      async () => host.rpcMethods
    )
  })
  const wss = new WebSocketServer({
    host: '127.0.0.1',
    port: 0,
    verifyClient: (info, done) => done(admitUpgrade(info.req), 403)
  })
  wss.on('connection', (ws) => {
    const transport = WebSocketTransport(ws)
    rpcServer.attachTransport(transport, { fs: fsInterface, engine: host.engine })
    ws.on('error', () => ws.close())
  })
  // An error before the server listens fails the start; one after it ends
  // the host with exit code 1.
  const port = await new Promise((resolve, reject) => {
    wss.once('error', reject)
    wss.once('listening', () => {
      wss.off('error', reject)
      wss.on('error', (e) => {
        console.error('the data-layer server failed:', e && e.stack ? e.stack : String(e))
        process.exit(1)
      })
      resolve(wss.address().port)
    })
  })
  await refusesWithoutSecret(port)
  process.stdout.write(JSON.stringify({ ready: true, port }) + '\n')
  exitWithParent()
}

// The one client of this port is the CLI, which relays its own /data-layer
// socket here: it sends the secret of this launch in a header and no Origin.
// A browser always sends an Origin, and a program of another user does not
// have the secret, so an upgrade from either gets a 403. The secret comes in
// the environment, and leaves it before any other code is loaded.
const SECRET_ENV = 'DCL_SCENE_CLI_DATA_LAYER_SECRET'
const SECRET_HEADER = 'x-dcl-data-layer-secret'

function admission() {
  const secret = Buffer.from(process.env[SECRET_ENV] || '')
  delete process.env[SECRET_ENV]
  if (!secret.length) throw new Error(`${SECRET_ENV} is not set: only dcl-scene-cli start --data-layer starts this host`)
  return (request) => {
    const given = Buffer.from(String(request.headers[SECRET_HEADER] ?? ''))
    return (
      request.headers.origin === undefined && given.length === secret.length && crypto.timingSafeEqual(given, secret)
    )
  }
}

// The rule is applied by a package this script only calls (the `ws` beside
// the editor, or the scene's), and one that does not ask `verifyClient` admits
// every upgrade. So the host asks for an upgrade without the secret before it
// reports its port: any answer but the 403 stops it.
function refusesWithoutSecret(port) {
  const open = new Error('the data-layer server does not refuse an upgrade without the launch secret')
  const headers = {
    Connection: 'Upgrade',
    Upgrade: 'websocket',
    'Sec-WebSocket-Version': '13',
    'Sec-WebSocket-Key': crypto.randomBytes(16).toString('base64')
  }
  return new Promise((resolve, reject) => {
    const probe = http.get({ host: '127.0.0.1', port, headers })
    probe.on('response', (res) => {
      res.resume()
      res.statusCode === 403 ? resolve() : reject(open)
    })
    probe.on('upgrade', () => reject(open))
    probe.on('error', reject)
  })
}

const run = mode === 'dump-crdt' ? dumpCrdt : serve
run().catch((e) => {
  console.error(e && e.stack ? e.stack : String(e))
  process.exit(1)
})
