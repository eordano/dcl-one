import assert from 'node:assert/strict'
import { test, after } from 'node:test'
import { execFileSync } from 'node:child_process'
import { createRequire } from 'node:module'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import vm from 'node:vm'
const directory = mkdtempSync(path.join(tmpdir(), 'dcl-environment-contract-'))
after(() => rmSync(directory, { recursive: true, force: true }))
const archive = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../src/vendor/node_modules.zip')
execFileSync('python3', ['-c', 'import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])', archive, directory])
const require = createRequire(path.join(directory, 'test.js'))
const source = readFileSync(path.join(directory, 'node_modules/@dcl/sdk/prebuilt/core.js'), 'utf8')
const fixtures = [
  ['Skybox', 1223, { clouds: { opacity: 0, speed: 0 }, stars: { brightness: 0 } }, [26, 10, 13, 0, 0, 0, 0, 21, 0, 0, 0, 0, 34, 5, 13, 0, 0, 0, 0]],
  ['Sun', 1224, { visible: false, intensity: 0 }, [16, 0, 29, 0, 0, 0, 0]],
  ['EnvironmentFog', 1225, { density: 0 }, [21, 0, 0, 0, 0]]
]
function contract(ecs, components, ByteBuffer) {
  const otherEngine = ecs.Engine()
  for (const [name, id, value, wire] of fixtures) {
    const component = ecs[name]
    assert.equal(component.componentId, id)
    assert.equal(components[name](otherEngine).componentId, id)
    assert.equal(components.componentDefinitionByName['core::' + name](otherEngine).componentId, id)
    const buffer = new ByteBuffer()
    component.schema.serialize(value, buffer)
    assert.deepEqual(Array.from(buffer.toBinary()), wire)
    assert.deepEqual(Array.from(ecs['PB' + name].encode(value).finish()), wire)
    const decoded = component.schema.deserialize(new ByteBuffer(new Uint8Array(wire)))
    for (const key of Object.keys(value)) assert.deepEqual(JSON.parse(JSON.stringify(decoded[key])), value[key])
    assert.equal(component.schema.create()[name === 'Sun' ? 'visible' : name === 'EnvironmentFog' ? 'density' : 'clouds'], undefined)
  }
  const gradient = { keys: [{ time: 0, color: { r: 2, g: 0, b: 0.5, a: 1 } }, { time: 1, color: { r: 0, g: 1, b: 3, a: 0 } }] }
  const sky = { skyboxTexture: 'assets/sky.png', skyColors: { zenith: gradient, nadir: gradient }, clouds: { opacity: 0, speed: -0.01 } }
  const encoded = ecs.PBSkybox.encode(sky).finish()
  const decoded = ecs.PBSkybox.decode(encoded)
  assert.deepEqual(JSON.parse(JSON.stringify(decoded.skyColors.zenith)), gradient)
  assert.equal(decoded.skyboxTexture, sky.skyboxTexture)
  assert.ok(decoded.clouds.speed < 0)
  assert.ok(ecs.ColorGradient.encode(gradient).finish().length > 0)
}
test('embedded CJS bindings keep canonical IDs and literal wire contracts', () => {
  const ecs = require('@dcl/ecs/dist-cjs')
  contract(ecs, ecs.components, require('@dcl/ecs/dist-cjs/serialization/ByteBuffer').ReadWriteByteBuffer)
})
for (const first of ['@dcl/sdk/ecs', '@dcl/ecs', '@dcl/ecs/dist/components']) {
  test(`prebuilt registry preserves component identity when ${first} loads first`, () => {
    const module = { exports: {} }
    vm.runInNewContext(source, { module, console, require: () => ({}), Uint8Array, ArrayBuffer, DataView, TextEncoder, TextDecoder, setTimeout, clearTimeout })
    const registry = module.exports
    void registry[first]
    const ecs = registry['@dcl/sdk/ecs']
    contract(ecs, registry['@dcl/ecs/dist/components'], registry['@dcl/ecs/dist/serialization/ByteBuffer'].ReadWriteByteBuffer)
    assert.equal(ecs.engine, registry['@dcl/ecs'].engine)
    for (const [name] of fixtures) {
      assert.equal(ecs[name], registry['@dcl/ecs'][name])
      assert.equal(ecs[name], registry['@dcl/sdk/ecs'][name])
    }
    assert.equal(registry['@dcl/ecs/dist/components/component-number'].componentNumberFromName('core::Sun'), 1224)
  })
}

test('embedded ambient declarations typecheck the public scene and component map', () => {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..')
  const sdkRequire = createRequire(path.join(root, 'library/js-sdk-toolchain/package.json'))
  const tsc = sdkRequire.resolve('typescript/lib/tsc.js')
  const fixture = readFileSync(path.join(root, 'apps/bevy-explorer/e2e/scenes/environment/src/index.ts'), 'utf8')
  writeFileSync(path.join(directory, 'index.ts'), fixture + `
import { componentDefinitionByName, ColorGradient } from '@dcl/sdk/ecs'
const sun = componentDefinitionByName['core::Sun'](engine)
const color: ColorGradient = { keys: [{ time: 0, color: { r: 2, g: 0, b: 0, a: 1 } }] }
sun.createOrReplace(engine.RootEntity, { visible: false, intensity: 0, color })
`)
  writeFileSync(path.join(directory, 'tsconfig.json'), JSON.stringify({
    extends:'./node_modules/@dcl/sdk/types/tsconfig.ecs7.json',
    compilerOptions:{noEmit:true,skipLibCheck:false},include:['index.ts']
  }))
  try {
    execFileSync(process.execPath, [tsc, '-p', path.join(directory, 'tsconfig.json'), '--pretty', 'false'], {stdio:'pipe'})
  } catch (error) {
    throw new Error(error.stdout?.toString() || String(error))
  }
})
