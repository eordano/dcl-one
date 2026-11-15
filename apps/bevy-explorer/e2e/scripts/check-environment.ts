// Run the SDK environment fixture through a built native Explorer's authenticated console.
import assert from 'node:assert/strict'
import { mkdir, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { Explorer } from '../driver/explorer.ts'
import { spawnNative } from '../driver/native.ts'
import { buildScene, serveRealm } from '../driver/realm.ts'
import { REPO_ROOT, REPORT_DIR, SCENES_DIR } from '../driver/paths.ts'
import { until } from '../driver/wait.ts'

const bin = process.env.DCL_E2E_NATIVE_BIN
if (!bin) throw new Error('DCL_E2E_NATIVE_BIN must name a built native Explorer')
const scene = await buildScene(path.join(SCENES_DIR, 'environment'))
const realm = await serveRealm({ scenes: [scene], port: 0, realmName: 'environment-contract' })
const source = path.join(REPO_ROOT, 'rendering/bevy-explorer/source')
const output = path.join(REPORT_DIR, 'environment')
await mkdir(output, { recursive: true })
const checks: string[] = []
let explorer: Explorer | undefined
try {
  explorer = new Explorer(await spawnNative({
    bin: path.resolve(bin), cwd: source, env: { BEVY_ASSET_ROOT: source },
    args: ['--realm', realm.realm, '--position', '0,24', '--preview', '--guest',
      '--system-scene', 'none', '--portables', '', '--fps', '30', '--log-file', path.join(output, 'client.log')]
  }))
  const client = explorer
  await client.loginGuest()
  const state = () => client.json<{
    scene: string | null; sunVisible: boolean; sunIntensity: number; fogDensity: number | null;
    zenith: number[] | null; nadir: number[] | null; cloudOpacity: number | null;
    cloudSpeed: number | null; starsBrightness: number | null; textureState: string
  }>('/scene_environment')
  const env = await until(state, s => s.sunVisible === false && s.sunIntensity === 0 && s.fogDensity === 0,
    { timeoutMs: 90_000, what: 'SDK root environment components' })
  assert.deepEqual(env.zenith, [0, 0, 1])
  assert.deepEqual(env.nadir, [1, 0, 0])
  assert.equal(env.cloudOpacity, 0)
  assert.ok(env.cloudSpeed !== null && env.cloudSpeed < 0)
  assert.equal(env.starsBrightness, 0)
  checks.push('SDK-generated root components preserve explicit zero/false and negative cloud speed')
  await writeFile(path.join(output, 'procedural.png'), await client.screenshot())
  await client.run('/set_component root Skybox {"skyboxTexture":"assets/sky.png","skyColors":{"zenith":{"keys":[{"time":0,"color":{"r":0,"g":0,"b":1,"a":1}}]}}}')
  await until(state, s => s.textureState === 'ready', { timeoutMs: 30_000, what: 'scene sky image' })
  await writeFile(path.join(output, 'texture.png'), await client.screenshot())
  checks.push('scene content texture loads')
  for (const name of ['Skybox', 'Sun', 'EnvironmentFog']) await client.run(`/delete_component root ${name}`)
  const reset = await until(state, s => s.scene === null, { timeoutMs: 10_000, what: 'component removal reset' })
  assert.equal(reset.sunVisible, true)
  assert.equal(reset.sunIntensity, 1)
  assert.equal(reset.fogDensity, null)
  await writeFile(path.join(output, 'default.png'), await client.screenshot())
  checks.push('component removal restores defaults')
  await client.run('/set_component root Sun {"visible":false,"intensity":0}')
  await until(state, s => s.sunVisible === false, { timeoutMs: 10_000, what: 'sun override' })
  await client.run('/teleport 1 24')
  await until(state, s => s.scene === null, { timeoutMs: 30_000, what: 'scene departure reset' })
  checks.push('scene departure clears overrides')
  await writeFile(path.join(output, 'world-state.json'), JSON.stringify(await client.worldState(), null, 2))
} finally {
  await writeFile(path.join(output, 'checks.json'), JSON.stringify(checks, null, 2))
  await explorer?.close()
  await realm.close()
}
console.log(checks.join('\n'))
