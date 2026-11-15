// dcl-scene-cli code-to-composite driver. Runs the scene's built bundle once
// under node against a mock renderer, keeps what the visual editor can edit
// and writes assets/scene/main.composite + main.crdt. Ported from upstream
// @dcl/sdk-commands commands/code-to-composite/{scene-executor,asset-migrator,
// name-generator,composite-generator}.ts. The bundle is this CLI's split
// loader, so it is loaded as the host runtime loads it (driver-common.mjs);
// the ~system answers below are this capture's own: offline, with empties.
//
//   node code-to-composite.mjs <sceneRoot>
//
// The scene's @dcl/inspector and @dcl/ecs are used (the vendored ones, or a
// real install): the editor engine is createEngineContext()'s, so the
// component set written is exactly what the editor reads back. The one
// stdout line is a JSON summary; scene console output goes to stderr. The
// process exits when the summary is written, so a timer or a socket that the
// scene left open does not keep it.
import { createRequire } from 'node:module'
import fs from 'node:fs'
import path from 'node:path'
import { inside, loadCjs, readSceneFile, realpath } from './driver-common.mjs'

const root = path.resolve(process.argv[2] ?? '.')
const sceneJson = JSON.parse(fs.readFileSync(path.join(root, 'scene.json'), 'utf8'))
const mainFile = sceneJson.main
const req = createRequire(path.join(root, 'package.json'))

const inspector = req('@dcl/inspector')
const ECS = req('@dcl/ecs/dist-cjs')
if (typeof inspector.createEngineContext !== 'function' || typeof inspector.dumpEngineToComposite !== 'function') {
  throw new Error(
    'the @dcl/inspector of the scene has no engine context: run dcl-scene-cli init --node-modules-only to restore the vendored node_modules'
  )
}
const { ReadWriteByteBuffer } = req('@dcl/ecs/dist-cjs/serialization/ByteBuffer')

// the editor engine: every component the inspector knows, plus react-ecs UI
// so UI entities can be recognised and dropped (upstream filter)
const { engine } = inspector.createEngineContext()
const uiComponentIds = new Set()
for (const name of ['UiTransform', 'UiText', 'UiBackground', 'UiInput', 'UiInputResult', 'UiDropdown', 'UiDropdownResult']) {
  const factory = ECS.components[name]
  if (typeof factory === 'function') uiComponentIds.add(factory(engine).componentId)
}
const transport = { filter: () => true, send: async () => {} }
engine.addTransport(transport)

const Transform = engine.getComponent('core::Transform')
const Name = engine.getComponent('core-schema::Name')

async function feed(bytes) {
  transport.onmessage(bytes)
  await engine.update(0)
}

// the renderer's initial state for the scene (upstream getInitialCrdtState):
// a player and camera Transform, the UI canvas and the camera mode
function putOperation(entity, component, value) {
  const data = new ReadWriteByteBuffer()
  component.schema.serialize(value, data)
  const out = new ReadWriteByteBuffer()
  ECS.PutComponentOperation.write(entity, 1, component.componentId, data.toBinary(), out)
  return out.toBinary()
}

function initialCrdtState() {
  const transform = {
    position: { x: 0, y: 0, z: 0 },
    rotation: { x: 0, y: 0, z: 0, w: 1 },
    scale: { x: 1, y: 1, z: 1 },
    parent: 0
  }
  const state = [putOperation(1, Transform, transform), putOperation(2, Transform, transform)]
  const probe = ECS.Engine()
  for (const [entity, name] of [
    [0, 'UiCanvasInformation'],
    [2, 'CameraMode']
  ]) {
    const factory = ECS.components[name]
    if (typeof factory !== 'function') continue
    const component = factory(probe)
    state.push(putOperation(entity, component, component.create(probe.RootEntity)))
  }
  return state
}

const mainCrdtPath = path.join(root, 'main.crdt')
const existingCrdt = fs.existsSync(mainCrdtPath) ? new Uint8Array(fs.readFileSync(mainCrdtPath)) : null

// ~system/* for the bundle: Runtime serves the scene's own files (the loader
// reads its chunks through it), EngineApi pipes what the scene sends its
// renderer into the editor engine; everything else answers with empties
const STATIC = {
  '~system/Runtime': {
    readFile: async ({ fileName }) => ({
      content: new Uint8Array(readSceneFile(root, fileName)),
      hash: 'code-to-composite'
    }),
    getRealm: async () => ({
      realmInfo: {
        baseUrl: 'http://localhost',
        realmName: 'code-to-composite',
        networkId: 0,
        commsAdapter: 'offline:offline',
        isPreview: true
      }
    }),
    getWorldTime: async () => ({ seconds: 43200 }),
    getSceneInformation: async () => ({
      urn: 'urn:code-to-composite',
      content: [],
      metadataJson: JSON.stringify(sceneJson),
      baseUrl: 'http://localhost/'
    }),
    getExplorerInformation: async () => ({
      agent: 'dcl-scene-cli code-to-composite',
      platform: 'desktop',
      configurations: {}
    })
  },
  '~system/EngineApi': {
    crdtSendToRenderer: async ({ data }) => {
      if (data && data.length) await feed(new Uint8Array(data))
      return { data: [] }
    },
    crdtGetState: async () => ({
      hasEntities: false,
      data: existingCrdt ? [...initialCrdtState(), existingCrdt] : initialCrdtState()
    }),
    crdtGetMessageFromRenderer: async () => ({ data: [] }),
    sendBatch: async () => ({ events: [] }),
    subscribe: async () => ({}),
    unsubscribe: async () => ({}),
    isServer: async () => ({ isServer: false })
  },
  '~system/Scene': {
    getSceneInfo: async () => ({
      cid: '',
      metadata: JSON.stringify(sceneJson),
      baseUrl: 'http://localhost/',
      contents: []
    })
  },
  '~system/EnvironmentApi': {
    isPreviewMode: async () => ({ isPreview: true }),
    getPlatform: async () => ({ platform: 'desktop' }),
    areUnsafeRequestAllowed: async () => ({ status: false }),
    getCurrentRealm: async () => ({
      currentRealm: { domain: 'http://localhost', layer: '', serverName: 'code-to-composite', displayName: 'code-to-composite' }
    }),
    getExplorerConfiguration: async () => ({ clientUri: '', configurations: {} }),
    getDecentralandTime: async () => ({ seconds: 43200 })
  },
  '~system/UserIdentity': {
    getUserData: async () => ({
      data: {
        userId: '0x0000000000000000000000000000000000000000',
        displayName: 'code-to-composite',
        hasConnectedWeb3: false,
        version: 1,
        publicKey: null
      }
    }),
    getUserPublicKey: async () => ({ address: null })
  },
  '~system/Players': {
    getConnectedPlayers: async () => ({ players: [] }),
    getPlayersInScene: async () => ({ players: [] }),
    getPlayerData: async () => ({ data: undefined })
  },
  '~system/CommunicationsController': {
    send: async () => ({ data: [] }),
    sendBinary: async () => ({ data: [] })
  },
  '~system/CommsApi': { getActiveVideoStreams: async () => ({ streams: [] }) },
  '~system/SignedFetch': {
    signedFetch: async () => ({ ok: false, status: 0, statusText: 'offline', headers: {}, body: '' }),
    getHeaders: async () => ({ headers: {} })
  },
  '~system/Testing': { logTestResult: async () => ({}), plan: async () => ({}) }
}

// Anything not listed above, and any method not listed, resolves to an async
// no-op returning {} (upstream's auto-mock): scene code that touches it at
// start keeps running instead of aborting the capture.
function autoMock(base) {
  return new Proxy(base, {
    get(target, prop) {
      if (prop in target) return target[prop]
      if (prop === 'then' || typeof prop === 'symbol') return undefined
      return async () => ({})
    }
  })
}

const modules = new Map()
function hostRequire(spec) {
  if (typeof spec !== 'string' || !spec.startsWith('~system/')) {
    throw new Error('the scene requested a non-host module: ' + spec)
  }
  if (!modules.has(spec)) modules.set(spec, autoMock(STATIC[spec] ?? {}))
  return modules.get(spec)
}

// scene console output goes to stderr so the summary line owns stdout:
// loadCjs gives this object to the bundle as `console` and `globalThis.console`
const sceneConsole = {}
for (const k of ['log', 'info', 'debug', 'warn', 'error', 'trace']) {
  sceneConsole[k] = (...a) => console.error(...a)
}
const fakeGlobal = { require: hostRequire, console: sceneConsole }

// upstream filterInspectorCompatibleComponents: drop UI entities whole, and
// the player Transform the initial state planted
function filterInspectorCompatible() {
  const uiEntities = new Set()
  for (const component of engine.componentsIter()) {
    if (!uiComponentIds.has(component.componentId)) continue
    for (const [entity] of engine.getEntitiesWith(component)) uiEntities.add(entity)
  }
  for (const component of engine.componentsIter()) {
    if (!('deleteFrom' in component)) continue
    for (const [entity] of engine.getEntitiesWith(component)) {
      if (uiEntities.has(entity) || uiComponentIds.has(component.componentId)) {
        component.deleteFrom(entity)
      }
    }
  }
  if (Transform.has(engine.PlayerEntity)) Transform.deleteFrom(engine.PlayerEntity)
  return uiEntities.size
}

// upstream migrateAssets: referenced files are copied under assets/scene/<Type>/
// (models into their own folder with their gltf dependencies) and every
// component path is rewritten
const ASSET_TYPES = {
  Models: ['.gltf', '.glb'],
  Images: ['.png', '.jpg', '.jpeg', '.bmp'],
  Audio: ['.mp3', '.wav', '.ogg'],
  Video: ['.mp4']
}
function assetType(file) {
  const ext = path.extname(file).toLowerCase()
  return Object.keys(ASSET_TYPES).find((type) => ASSET_TYPES[type].includes(ext)) ?? null
}

function collectAssetPaths(data, into) {
  if (data === null || data === undefined) return
  if (typeof data === 'string') {
    if (data.length && assetType(data) && !/^[a-z][a-z0-9+.-]*:/i.test(data)) into.add(data)
    return
  }
  if (Array.isArray(data)) {
    for (const item of data) collectAssetPaths(item, into)
    return
  }
  if (typeof data === 'object') {
    for (const value of Object.values(data)) collectAssetPaths(value, into)
  }
}

function replaceAssetPaths(data, mapping) {
  if (data === null || data === undefined) return { data, changed: false }
  if (typeof data === 'string') {
    const to = mapping.get(data)
    return to === undefined ? { data, changed: false } : { data: to, changed: true }
  }
  if (Array.isArray(data)) {
    let changed = false
    const out = data.map((item) => {
      const r = replaceAssetPaths(item, mapping)
      changed ||= r.changed
      return r.data
    })
    return { data: out, changed }
  }
  if (typeof data === 'object') {
    let changed = false
    const out = {}
    for (const [k, v] of Object.entries(data)) {
      const r = replaceAssetPaths(v, mapping)
      changed ||= r.changed
      out[k] = r.data
    }
    return { data: out, changed }
  }
  return { data, changed: false }
}

// A glTF `uri` that names a file: a relative path, decoded, with no `..`
// segment. A URL or `data:` URI is not a file to copy; an absolute path or a
// climb out of the model's folder is not a dependency a model may have, and
// copying it would read or write outside the project.
function fileDependency(uri) {
  if (typeof uri !== 'string' || !uri || /^[a-z][a-z0-9+.-]*:/i.test(uri)) return null
  let decoded
  try {
    decoded = decodeURIComponent(uri)
  } catch {
    return null
  }
  if (path.posix.isAbsolute(decoded) || path.win32.isAbsolute(decoded)) return null
  const segments = decoded.split(/[\\/]+/)
  if (segments.some((s) => s === '' || s === '.' || s === '..')) return null
  return segments.join('/')
}

// External images and buffers a .gltf (JSON) or .glb (JSON chunk) names,
// relative to the model file; data: URIs are embedded and need no copy.
// `rejected` holds the uris that are not file dependencies (see
// fileDependency), which are left as they are.
function gltfDependencies(file) {
  const none = { deps: [], rejected: [] }
  const deps = []
  const rejected = []
  try {
    const bytes = fs.readFileSync(file)
    let json
    if (bytes.length >= 20 && bytes.readUInt32LE(0) === 0x46546c67) {
      const len = bytes.readUInt32LE(12)
      if (bytes.readUInt32LE(16) !== 0x4e4f534a) return none
      json = JSON.parse(bytes.subarray(20, 20 + len).toString('utf8'))
    } else {
      json = JSON.parse(bytes.toString('utf8'))
    }
    const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value)
    if (!isObject(json)) throw new Error('the glTF is not an object')
    for (const key of ['images', 'buffers']) {
      const list = json[key] ?? []
      if (!Array.isArray(list) || !list.every(isObject)) throw new Error(`${key} is not a list of objects`)
      for (const item of list) {
        if (typeof item.uri !== 'string' || !item.uri || item.uri.startsWith('data:')) continue
        const dep = fileDependency(item.uri)
        if (dep === null) rejected.push(item.uri)
        else deps.push(dep)
      }
    }
  } catch (e) {
    console.error(`  could not read the dependencies of ${path.relative(root, file)}: ${e.message}`)
    return none
  }
  return { deps, rejected }
}

function destinationFor(rel, prefix = '') {
  const type = assetType(rel)
  const file = prefix + path.basename(rel)
  if (type === 'Models') {
    return path.join('assets', 'scene', 'Models', path.basename(file, path.extname(file)), file)
  }
  return path.join('assets', 'scene', type, file)
}

// What a copy of `fromAbs` to `toRel` would find there: nothing ('free'), a
// file with the same bytes ('same') or anything else ('other'). The files are
// read only when their sizes are equal.
function found(fromAbs, toRel) {
  const toAbs = path.join(root, toRel)
  if (path.resolve(fromAbs) === path.resolve(toAbs)) return 'same'
  const to = fs.statSync(toAbs, { throwIfNoEntry: false })
  if (!to) return 'free'
  if (!to.isFile() || to.size !== fs.statSync(fromAbs).size) return 'other'
  return fs.readFileSync(fromAbs).equals(fs.readFileSync(toAbs)) ? 'same' : 'other'
}

// Two sources with different bytes never share a destination, and a file with
// other bytes is never replaced. The first source (migrateAssets goes in path
// order) takes the plain name; a later one gets its parent folders in front
// (images/b/logo.png -> b-logo.png), then a number. A source whose bytes are
// all at a taken destination already shares it. A model folder belongs to one
// source folder, so the files beside the model (`beside`) keep their relative
// paths. `owners` maps each taken destination to its source and its spelling;
// the key is lower case because macOS and Windows ignore case.
function claimDestination({ rel, abs, from, model, beside }, owners) {
  const owner = path.resolve(model ? from : abs)
  const parents = path.relative(root, from).split(path.sep).filter(Boolean)
  for (let n = 0; ; n++) {
    const number = n > parents.length ? `${n - parents.length + 1}-` : ''
    const folders = parents.slice(Math.max(parents.length - n, 0)).map((p) => `${p}-`)
    const to = destinationFor(rel, number + folders.join('')).split(path.sep).join('/')
    const dir = path.posix.dirname(to)
    const place = model ? dir : to
    const taken = owners.get(place.toLowerCase())
    const shared = taken && taken.owner !== owner
    if (shared && taken.place !== place) continue
    const files = [[abs, to], ...beside.map((dep) => [path.join(from, dep), path.posix.join(dir, dep)])]
    const there = files.map(([file, dest]) => found(file, dest))
    if (there.includes('other') || (shared && there.includes('free'))) continue
    if (!taken) owners.set(place.toLowerCase(), { owner, place })
    return to
  }
}

// Where a write to `toRel` lands. A link is never written through: the bytes
// would land where it points.
function destination(toRel) {
  const toAbs = path.join(root, toRel)
  if (!inside(root, toAbs)) throw new Error(`${toAbs} is outside the project`)
  if (fs.lstatSync(toAbs, { throwIfNoEntry: false })?.isSymbolicLink()) {
    throw new Error(`${toRel} is a link: remove it and run again`)
  }
  if (!inside(fs.realpathSync(root), realpath(path.dirname(toAbs)))) {
    throw new Error(`${toRel} is in a folder that links out of the scene: remove the link and run again`)
  }
  return toAbs
}

function copyInto(fromAbs, toRel) {
  if (!inside(root, fromAbs)) throw new Error(`${fromAbs} is outside the project`)
  const toAbs = destination(toRel)
  // A destination that exists holds these bytes: claimDestination compared them.
  if (fs.existsSync(toAbs)) return
  fs.mkdirSync(path.dirname(toAbs), { recursive: true })
  fs.copyFileSync(fromAbs, toAbs)
}

function migrateAssets() {
  const refs = new Set()
  for (const component of engine.componentsIter()) {
    for (const [, value] of engine.getEntitiesWith(component)) collectAssetPaths(value, refs)
  }
  const mapping = new Map()
  const owners = new Map()
  const missing = []
  const skipped = []
  for (const rel of [...refs].sort()) {
    const abs = path.isAbsolute(rel) ? rel : path.join(root, rel)
    const from = path.dirname(abs)
    if (!inside(root, abs) || !fs.existsSync(abs) || !fs.statSync(abs).isFile()) {
      missing.push(rel)
      continue
    }
    const posixRel = rel.split(path.sep).join('/')
    const model = assetType(rel) === 'Models'
    const { deps, rejected } = model ? gltfDependencies(abs) : { deps: [], rejected: [] }
    for (const uri of rejected) skipped.push(`${posixRel}: ${uri}`)
    const beside = deps.filter((dep) => fs.statSync(path.join(from, dep), { throwIfNoEntry: false })?.isFile())
    for (const dep of deps) {
      if (!beside.includes(dep)) missing.push(path.posix.join(path.posix.dirname(posixRel), dep))
    }
    const to = claimDestination({ rel, abs, from, model, beside }, owners)
    if (to !== posixRel) {
      copyInto(abs, to)
      mapping.set(rel, to)
    }
    for (const dep of beside) {
      const depFrom = path.posix.join(path.posix.dirname(posixRel), dep)
      const depTo = path.posix.join(path.posix.dirname(to), dep)
      if (depTo === depFrom) continue
      copyInto(path.join(from, dep), depTo)
      mapping.set(depFrom, depTo)
    }
  }
  for (const component of engine.componentsIter()) {
    if (!('createOrReplace' in component)) continue
    for (const [entity, value] of Array.from(engine.getEntitiesWith(component))) {
      const r = replaceAssetPaths(value, mapping)
      if (!r.changed) continue
      component.createOrReplace(entity, r.data)
    }
  }
  return { moved: Array.from(mapping, ([from, to]) => ({ from, to })), missing, skipped }
}

// upstream generateEntityNames: first matching component names the entity
function stem(p) {
  return path.parse(path.basename(p)).name
}

const NAME_RULES = [
  ['core::GltfContainer', (d) => (d?.src ? stem(d.src) : null)],
  ['core::MeshRenderer', (d) => d?.mesh?.$case ?? 'Mesh'],
  [
    'core::TextShape',
    (d) => {
      if (!d?.text) return 'Text'
      const text = String(d.text).trim()
      return `Text: ${text.length > 10 ? text.substring(0, 10) + '...' : text}`
    }
  ],
  ['core::VideoPlayer', (d) => (d?.src ? stem(d.src) : 'Video')],
  ['core::NftShape', () => 'NFT'],
  ['core::LightSource', () => 'Light'],
  ['core::AudioSource', (d) => (d?.audioClipUrl ? stem(d.audioClipUrl) : 'Audio')],
  ['core::MeshCollider', (d) => (d?.mesh?.$case ? `Mesh Collider: ${d.mesh.$case}` : 'Mesh Collider')]
]

function nameEntities() {
  const reserved = new Set([engine.RootEntity, engine.PlayerEntity, engine.CameraEntity])
  for (const [componentName, rule] of NAME_RULES) {
    const component = engine.getComponentOrNull(componentName)
    if (!component) continue
    for (const [entity, value] of Array.from(engine.getEntitiesWith(component))) {
      if (reserved.has(entity) || Name.has(entity)) continue
      const name = rule(value)
      if (!name) continue
      Name.create(entity, { value: name.charAt(0).toUpperCase() + name.slice(1) })
    }
  }
}

process.on('unhandledRejection', (e) => {
  console.error('unhandled rejection in the scene:', e)
})

if (existingCrdt) await feed(existingCrdt)

const loader = loadCjs(root, mainFile, fakeGlobal)
if (typeof loader.onStart !== 'function' || typeof loader.onUpdate !== 'function') {
  console.error(`${mainFile} exports no onStart/onUpdate -- not a built scene?`)
  process.exit(1)
}
try {
  await loader.onStart()
  await loader.onUpdate(0)
  await engine.update(0)
} catch (e) {
  console.error('the scene threw while starting:', e && e.stack ? e.stack : e)
  process.exit(1)
}

const uiEntities = filterInspectorCompatible()
const assets = migrateAssets()
nameEntities()
await engine.update(0)

const composite = inspector.dumpEngineToComposite(engine, 'json')
const [compositePath, crdtPath] = ['assets/scene/main.composite', 'main.crdt'].map(destination)
fs.mkdirSync(path.dirname(compositePath), { recursive: true })
fs.writeFileSync(compositePath, JSON.stringify(ECS.Composite.toJson(composite), null, 2))
fs.writeFileSync(crdtPath, inspector.dumpEngineToCrdtCommands(engine))

const entities = new Set()
for (const component of engine.componentsIter()) {
  for (const [entity] of engine.getEntitiesWith(component)) entities.add(entity)
}
const summary = JSON.stringify({
  ok: true,
  entities: entities.size,
  components: composite.components.length,
  uiEntities,
  assets: assets.moved,
  missingAssets: assets.missing,
  skippedDependencies: assets.skipped
})
// The run ends here, whatever the scene left running: a timer, a socket.
process.stdout.write(summary + '\n', () => process.exit(0))
