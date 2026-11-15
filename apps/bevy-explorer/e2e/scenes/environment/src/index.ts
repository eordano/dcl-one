import { engine, EnvironmentFog, Skybox, Sun } from '@dcl/sdk/ecs'

export function main() {
  const color = (r: number, g: number, b: number) => ({ keys: [{ time: 0, color: { r, g, b, a: 1 } }] })
  Skybox.create(engine.RootEntity, {
    skyColors: { zenith: color(0, 0, 1), horizon: color(0, 1, 0), nadir: color(1, 0, 0) },
    clouds: { opacity: 0, speed: -0.01 },
    stars: { brightness: 0 }
  })
  Sun.create(engine.RootEntity, { visible: false, intensity: 0, direction: { x: 0, y: -1, z: 0 } })
  EnvironmentFog.create(engine.RootEntity, { density: 0, color: color(0.1, 0.2, 0.3) })
  console.log('environment fixture ready')
}
