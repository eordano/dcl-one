# Environment contract fixture

SDK7 scene at `0,24`, fixed at noon. Its root authors blue zenith, green horizon,
red nadir, no clouds or stars, an invisible sun with zero light intensity, and
zero fog density. These explicit values exercise optional protobuf fields.

From `apps/bevy-explorer/e2e`, build with the repository's scene CLI:

```sh
DCL_SCENE_CLI=/path/to/dcl-scene-cli npm run build-scenes -- environment
DCL_E2E_NATIVE_BIN=/path/to/decentra-bevy npm run check:environment
```

The native check serves this scene on a temporary loopback realm, checks its SDK
values through the renderer, switches to `assets/sky.png`, then removes the root
components and leaves the scene. It captures default/procedural/texture frames in
the E2E package's `report/environment` directory.
The image is a 4×2 equirectangular fixture: red centre faces +Z and its green seam
faces -Z. It is tracked with Git LFS, like the other visual fixtures.

For manual preview, open this scene with the scene CLI and use the **Scene environment**
debug section to inspect the overrides and change the clock. Removing a component
restores that renderer's time-of-day defaults. The GPU test
`environment::tests::scene_environment_pipelines_compile` additionally reads raw
HDR cubemaps with nonzero daylight, including the exact nadir and both ambient-map
quality settings; a normal screen capture cannot prove those raw texels are finite.
