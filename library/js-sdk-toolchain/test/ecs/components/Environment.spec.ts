import { components, Engine, PBSkybox, PBSun, PBEnvironmentFog } from '../../../packages/@dcl/ecs/src'
import { testComponentSerialization } from './assertion'

describe('Scene-root environment contracts', () => {
  it('exports the three canonical IDs and preserves nested HDR gradients', () => {
    const engine = Engine()
    const skybox = components.Skybox(engine)
    const sun = components.Sun(engine)
    const fog = components.EnvironmentFog(engine)
    expect([skybox.componentId, sun.componentId, fog.componentId]).toEqual([1223, 1224, 1225])
    const gradient = {
      keys: [
        { time: 0, color: { r: 2, g: 0, b: 0.5, a: 1 } },
        { time: 1, color: { r: 0, g: 1, b: 3, a: 0 } }
      ]
    }
    testComponentSerialization(skybox, {
      skyboxTexture: 'images/sky.png',
      skyColors: { zenith: gradient, horizon: gradient, nadir: gradient, rim: gradient },
      clouds: { color: gradient, opacity: 0, speed: -0.01 },
      stars: { brightness: 0 }
    })
    testComponentSerialization(sun, { color: gradient, visible: false, intensity: 0, direction: { x: 0, y: -1, z: 0 } })
    testComponentSerialization(fog, { color: gradient, density: 0 })
  })

  it('preserves explicit zero and false on the protobuf wire, independently of absence', () => {
    // Literal wire tags pinned to the public .proto field numbers; not codec round trips alone.
    expect(Array.from(PBSun.encode({ visible: false, intensity: 0 }).finish())).toEqual([16, 0, 29, 0, 0, 0, 0])
    expect(Array.from(PBEnvironmentFog.encode({ density: 0 }).finish())).toEqual([21, 0, 0, 0, 0])
    expect(
      Array.from(PBSkybox.encode({ clouds: { opacity: 0, speed: 0 }, stars: { brightness: 0 } }).finish())
    ).toEqual([26, 10, 13, 0, 0, 0, 0, 21, 0, 0, 0, 0, 34, 5, 13, 0, 0, 0, 0])
    expect(PBSun.decode(new Uint8Array())).toMatchObject({ visible: undefined, intensity: undefined })
    expect(PBEnvironmentFog.decode(new Uint8Array())).toMatchObject({ density: undefined })
  })
})
