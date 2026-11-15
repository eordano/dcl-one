# The bevy fork

bevy-explorer builds bevy from upstream bevy 0.16.1 plus our changes. The repository tracks the
upstream commit and one patch file per change in `rendering/bevy-explorer/patches/bevy`, and
`tooling/patched-sources.py` writes the rebuilt tree at `rendering/bevy-explorer/bevy` (see the
[ADR](../../../../docs/adr/2026-11-14-explorer-sources-as-patches.md)). Seventeen other sources
are tracked the same way and rebuilt under `rendering/bevy-explorer/forks`; this page is only
about bevy, because that is where behaviour differs from stock and where a reader who knows bevy
will otherwise be wrong.

If a change belongs in bevy, make it a patch (see [Changing bevy](#changing-bevy)). Changes marked **throwaway** are 0.16
adaptations of work that is already upstream in a later bevy and are dropped, not ported,
on any migration.

## Web workers and threading (`web-worker` feature)

Stock bevy on wasm is single-threaded on the page's main thread. The fork runs the engine
across workers:

- `bevy::web_worker::start` takes the page's canvas and spawns every worker from one page
  call: the render worker (OffscreenCanvas, wgpu device, render world), the compute workers
  (`ComputeTaskPool`), the async-compute workers (`AsyncComputeTaskPool`) and the engine worker
  (app world, winit event loop). One script serves every role; each worker instantiates the
  module over the page's shared memory and calls bevy's export for its role. `start_render`
  brings the render worker up first so a pipeline cache can warm while the launch screen shows.
- Page functions: JS hooks the engine looks up on `self` are declared in an extern block with
  `#[page_functions]` (`bevy_derive`) and relayed from every worker to the page. Worker console
  output is mirrored to the page and printed only there.
- The render world runs on its own worker (`bevy_render::web_worker`,
  `RenderCreation::WebWorker`, `App::set_finisher`, `PrecreatedSurface`). The app is paced from
  the render worker's animation frames, rendering happens inside the rAF callback, and at most
  two frames are in flight, fenced with mappable buffers.
- `RenderCapabilities`: a main-world resource for limits, features and adapter info. With the
  feature on, no wgpu handle is ever inserted into the main world.
- `WgpuWrapper` on wasm records its creating thread; a value dropped off-thread is parked and
  released on the render thread at `RenderSet::PostCleanup` instead of panicking.
- `bevy_tasks`: the multi-threaded `TaskPool` on the web. Workers join through `run_worker`;
  `Task::split` separates handle from future. `TaskPool::spawn` keeps the single-threaded
  contract (static, non-`Send`, driven by the JS event loop). `AsyncComputeTaskPool::spawn`
  requires `Send` on the web and runs on the async-compute workers. With WASM atomics,
  `wasm-bindgen-futures` can re-poll a self-waking task inline. A CPU loop that awaits
  `yield_now()` can therefore occupy the engine worker for the whole loop. Run CPU-heavy
  work on an async-compute worker or in the scene worker that owns the input.
- `bevy_ecs`: parallel iteration and the multi-threaded executor on the web under the feature.
  The render world's Extract and Render schedules stay single-threaded, since WebGPU objects
  are thread-bound.
- `PipelinedRenderingDiagnosticsPlugin`: per-stage timings of a pipelined frame under
  `pipelined/`, including GPU backlog and animation-frame waits on the web.
- winit is upstream `v0.30.12` plus the series in `rendering/bevy-explorer/patches/winit`, for
  OffscreenCanvas worker windows.
- `wasm_threaded_loader` feature (`bevy_asset`): asset loading on a worker; in-flight loads
  cancel when the last handle drops; `futures-channel` mpsc instead of `async-channel`, whose
  lock lowers to `memory.atomic.wait` and traps on the main thread.

## Depth of field and transparency

- **Transparent focus target.** When a view has depth of field, the main transparent pass gets
  a second `Rg16Float` attachment. Fragments blend `(view_depth, 1, 0, alpha)` into it, so it
  accumulates alpha-weighted depth and coverage, and the DoF shader derives focus depth from
  it. `FragmentOutput` gains the output behind `TRANSPARENT_FOCUS_OUTPUT`. A custom material
  shader that omits it fails pipeline creation on web. Gizmos declare the target with an empty
  write mask. Transmission-reading blend pipelines are included.
- `DepthOfFieldMode::Disabled` keeps the component active with no blur, so the focus target is
  still rendered for passes that read it (the explorer's stars).
- The DoF gather weights each sample by the ratio of its circle of confusion to the gathering
  pixel's, so sharp neighbours don't smear into a blurred pixel. Earlier: distant-only blur and
  alpha blending in the DoF shader.
- Per-asset write ticks in `RenderAssets` so entities respecialize when a prepared asset is
  swapped render-world-only (fallback material to real, deferred mesh re-upload).
- `Transparent3d` sorts by euclidean distance from the camera rather than view-space z.
- Superseded: an "alpha depth write" change (blend materials wrote depth in the main pass,
  `MeshPipelineKey::IN_PREPASS`) that the focus target replaced. The key flag remains.

## GPU upload and render assets

- Byte-budgeted, prioritised per-frame uploads: `RenderAssetTransferPriority`,
  `RenderAssetBytesPerFrame::new_with_priorities`, `request_bytes`. `RenderAssets` keeps the
  previous GPU copy while a re-upload is pending (`get_latest`, `set_stale`), and GPU textures
  are reused across re-uploads.
- Render assets are not removed when the main-world asset changes:
  `Assets::get_mut_bypass_change_detection`, `Mesh::take_gpu_copy`.
- `immediate_upload` on `Image`, `Mesh` and font atlases, and as a glTF loader setting, to
  bypass the byte budget.
- **Fallback materials**: `ExtendedMaterial` renders its base while the extension's assets
  aren't ready; `ForwardDecalMaterial<B>` is generic over the base; a material extension's
  `alpha_mode` receives the base as input.
- Pipeline cache: `PipelineCompilationMode::async_with_handler` with a
  `PipelineCompilationHandler` that is notified as pipelines compile.
- **Throwaway**: `RenderMeshInstances` and `RenderMaterialInstances` mirrored into render-entity
  components with component-first lookups (`MeshInstanceLookup`, `MaterialInstanceLookup`,
  `validate_instance_mirror` feature); upstream #22966 removes the need.
- **Throwaway**: `ViewVisibility` carries previous-frame visibility instead of per-frame hash
  sets (`SetViewVisibility::set_visible`); upstream #22226.

## Shader and uniform hooks

- `UserGlobal(f32)` resource surfaces as `globals.user_global`; `ViewUserValue(f32)` on a
  camera surfaces as `view.user_value`.
- Light textures (upstream #18031): `PointLightTexture`, `SpotLightTexture`,
  `DirectionalLightTexture`, extended to multicolour cookies.
- Unclipped-depth emulation is forced on WebGPU, where the feature may be declared but not work.
- `MAX_MORPH_WEIGHTS` raised from 64 to 256.
- `specular_multiscatter` matches the Filament reference (upstream #23514); inverse tonemapping
  fix.

## glTF and images

- `GltfLoader::uri_resolver` (`GltfPlugin::with_uri_resolver`): the host resolves every URI a
  glTF references. This is how content paths are contained; see the security page.
- `KHR_texture_basisu`, `EXT_texture_webp` and `MSFT_texture_dds` are supported: `gltf`'s
  `allow_empty_texture` feature lets a file that requires them (and textures with no `source`)
  through. A texture with no `source` samples its extension image, KTX2 first when it has
  several; one with a PNG/JPEG fallback keeps the fallback. A KTX2, by path or embedded,
  decodes through `bevy_image`'s KTX2 route below; WebP and DDS through bevy's own decoders.
- KTX2 transcoder hook (`bevy_image::set_ktx2_transcoder`, `Ktx2Transcoder`): one process-wide
  host function, set once, that every KTX2 decode asks first. It gets the bytes and the asset
  path: the one `ImageLoader` loaded it from, or the labelled path the glTF loader decodes an
  embedded image at through `Image::from_buffer_at` (made public; `model.glb#Texture0`).
  `Image::from_buffer` passes `None` (bevy's LUTs). It returns `None` (bevy's own KTX2
  decode), bytes plus the format to decode them as instead (with the caller's `is_srgb`,
  sampler and asset usage), or an error. A global because `Image::from_buffer` has no plugin
  or loader to carry it. `image_processing` sets it to transcode UASTC to BC7, fails a KTX2
  it can't transcode whenever a path is given (glTF content never reaches bevy's own KTX2
  decode, which trusts the header), and uses the path to cache the result. Alongside it: KTX2 bytes are taken by their magic at any path,
  whatever type was declared (also under the `Guess` format setting, which the `image` crate
  can't guess for KTX2), so a KTX2 without a file extension or under another one loads too.
  DDS bytes decode as DDS whatever was declared, so a transcode the host cached over the KTX2
  bytes reads back from the same path.
- Every compressed decode (basis, dds, ktx2) gets the caller's asset usage, which upstream
  leaves at the default: a cached dds loads with `RENDER_WORLD` as its first load did.
- The patched crates' tests (`bevy_image`, `bevy_gltf`) run in the explorer's CI ("Test the
  patched Bevy crates"): they are path dependencies, so `cargo test --all` never
  compiles them. From `source`, seed the fork's workspace with this lock file and run them there:
  `cp Cargo.lock ../bevy/Cargo.lock`, then `cargo test --manifest-path ../bevy/Cargo.toml -p
  bevy_image -p bevy_gltf --lib --features bevy_gltf/dds,bevy_image/ktx2,bevy_image/zstd`.
- Accessors with no `bufferView` are accepted (zero-filled per spec; Blender morph targets).
- Texture decode is skipped when `load_materials` is empty, with a 1x1 placeholder under the
  normal label: headless servers keep geometry without pixels.
- `Rgba16Unorm` textures are converted to 8-bit, preserving sampler and sRGB (the web can't
  sample 16-bit).
- `reduce_image_sizes` feature: textures over a per-dimension cap are resized at load
  (1024 on wasm, 8192 on native).
- Smaller: glTF textures load sequentially rather than in parallel; non-embedded textures honour
  `render_asset_usages`; samplers survive downsizing; unique mesh names; default image format
  guessing.

## ECS, app and scene

- `HierarchyPropagatePlugin`, `Propagate<C>`, `PropagateStop<C>`, `PropagateOver<C>` (upstream
  #17575): propagate a component down the hierarchy.
- `children!` accepts more than twelve children (upstream #18865, #21099).
- Ignored command errors build no `BevyError` (upstream #19718: `HandleError::ignore_error`,
  `queue_silenced`): the `try_*` entity commands and the relationship hooks drop their error
  instead of handing it to `ignore`. Building a `BevyError` captures a backtrace whenever
  `RUST_BACKTRACE` is set, and despawning a hierarchy queues a failing `try_remove::<ChildOf>`
  per child plus every on-remove observer's `try_remove`, nested as deep as the hierarchy; a
  Genesis Plaza unload spent about a second capturing them.
- `SceneSpawner` forgets child instances when their parent is gone, and `unregister_instance`
  cancels a spawn that hasn't happened yet.
- `SceneSpawner::set_spawn_deadline`: queued `Scene` instances are written an entity at a time,
  parents before children, one instance after another in request order, until the deadline. The
  rest waits for the next run, and each run writes at least one entity. `None`, the default,
  spawns them whole with stock bevy's `write_to_world_with`; dynamic scenes and `spawn_sync`
  always do.
  - The progress is on the entities: each one not written yet carries a sparse-set marker, and a
    second, *ready* marker once its parent is written, the top-level ones first. Bevy shows a
    child whose parent has no visibility yet, so a child is never written before its parent.
  - A part-written instance's top-level entities are attached to the parent and kept
    `Visibility::Hidden` until it is complete; a `HiddenSceneRoot` component holds their own
    visibility, restored then unless something changed it meanwhile (none becomes `Inherited`).
    A descendant with an explicit `Visibility::Visible` still shows; glTF nodes are `Inherited`.
  - Its parent despawned, `despawn_instance` or `unregister_instance` despawn what was written,
    and drop the instance if it is still queued.
  - A scene replaced or removed mid-spawn starts the instance over. When none is ready although
    some are unwritten, those whose parent was despawned go too and the instance completes with
    what is left (its ready event fires); those the hierarchy does not reach are written in
    slices. An instance that fails to write, sliced or whole, is despawned and forgotten.
  - A part-written instance is finished in the world it started in: continuing it, despawning
    it or running `scene_spawner_system` on another world meanwhile panics before anything
    changes.
  - `spawn_progress` reports the queue and the instance being written, for diagnostics.
  - `spawn_as_child_hidden` keeps an instance hidden after it has spawned, until
    `reveal_instance_sync`.
  - The explorer sets the deadline each frame (`scene_runner::scene_spawn_slice`) and spawns glTF
    containers hidden until `update_ready_gltfs` has processed them, a node at a time within the
    same slice.
- `mark_dirty_trees` keys its early-out on this run's tick, so transform propagation can run
  twice per frame (the explorer's early pass plus bevy's own).
- `GlobalTransform::from(Transform)` and `mul_transform` sanitise the affine: a zero scale
  component becomes `f32::EPSILON` and non-finite parts fall back to identity. A singular
  affine decomposes to a NaN rotation, which reached colliders as NaN AABBs and crashed parry's
  BVH build on zero-thickness glTF walls. Every derived `GlobalTransform` is safe by
  construction; the local `Transform` keeps its exact values, so scene read-back of what a scene
  wrote comes from locals (`TransformHelperPub::compute_relative_transform`), never from the
  global.
- `bevy_animation`: newly added graphs are threaded the same frame (no one-frame T-pose);
  evaluator hash maps replaced with dense index lists; the evaluator stack is cleared.
- `bevy_text`: faces of removed `Font` assets are evicted from the cosmic font system;
  `TextFont` is `PartialEq`.
- `bevy_winit`: modifier transitions reported only via `ModifiersChanged` are synthesised as key
  events, so a modifier swallowed by an OS chord doesn't stick; `UpdateMode::Reactive` timing
  fix.
- `WorldId::new` takes `try_update` behind `target_has_atomic = "ptr"` with the `fetch_update`
  fallback (upstream #26020, **throwaway**); `try_update` needs Rust 1.95, so `bevy`, `bevy_ecs`
  and `bevy_input_focus` declare `rust-version = "1.95.0"`. The native builds pin 1.99.1 and the
  web build pins `nightly-2026-04-15` (`apps/bevy-explorer/build-web.py`, `source/justfile`); both
  are above that floor.

## UI

- `BorderColor { left, top, right, bottom }` with `BorderColor::all` (upstream #18682).
- `CameraCursorPosition` on a camera gives UI picking a per-camera cursor, for render-to-texture
  UI; cursor positions are in physical coordinates.
- `Interaction::Pressed` is dropped when the pointer leaves the node.

## Visibility and small changes

- `ComputedCameraValues` fields, `RenderMaterialInstance` fields public; `Projection` is
  `PartialEq`.
- Web screenshots blit through the sRGB swapchain view (upstream #22090).
- Fixes with no API surface: unicode asset paths, skin buffer growth, missing cascades panic,
  bad vertices for a frame on second mesh load, sparse index warning, `embedded_asset!` paths
  from a Windows build host (`file!()` backslashes on a non-Windows target such as wasm).

## Changing bevy

The same steps change any source under `rendering/bevy-explorer/patches`; use its name in place
of `bevy`. The commands are `python3 tooling/patched-sources.py <command>`, with the path from the
repository root.

- **Files.** `patches/<name>/source.toml` records the upstream repository and commit, the licence
  of the source (`license`), the git tree ids before and after the series, and `nix_hash`.
  `patches/<name>/series` holds the patches in order. The rebuilt tree is
  `rendering/bevy-explorer/bevy` for `bevy` and `rendering/bevy-explorer/forks/<name>` for every
  other source. Git ignores it.
- **`materialise`** writes each rebuilt tree that is missing or stale, also one that lost all its
  files. Run it after a checkout, a pull or a rebase, before any `cargo` command. The `just`
  recipes, the build scripts under `apps/bevy-explorer` and CI run it.
  - Edits in a rebuilt tree stay. When the series changed below them, `materialise` rebuilds the
    tree, applies the edits again and prints each step. On a conflict it stops, names the files
    and changes nothing in the tree. In both cases it saves the edits as a patch file under
    `edits/` in its cache (`~/.cache/dcl-one/patched-sources`) and prints the path.
  - `--strict` fails on an edited tree.
  - `--force` drops the edits.
- **`export bevy -m "subject"`** writes the edits in the rebuilt tree as the next patch file and
  updates the tree id in `source.toml`. Edit the rebuilt tree in place and build as usual first.
  Commit both files, and update this page in the same PR.
- **`status`** says which trees are missing, current, stale or edited. **`verify`** rebuilds each
  source and checks the tree ids.
- **Stale trees.** The build script of the `common` crate fails with one line that names
  `materialise` when a rebuilt tree is not the one the tracked files describe. Only packages that
  depend on `common` have this check. With no rebuilt tree at all Cargo fails earlier, when it
  loads the manifests: ``patch location `...` does not contain packages matching `bevy` ``.
- **Nix.** `infra/packages/bevy-explorer.nix` fetches `commit` with `nix_hash` and runs the same
  tool. CI does not build the Nix package, so nothing there proves `nix_hash`: after a change of
  `commit`, run `nix build .#bevy-explorer.sources`, which fails with the hash to record.
- `.gitattributes` sets `-text -whitespace` on `patches/**`, so patch bytes stay exact on every
  platform. Never reformat a patch file.
- `source/Cargo.toml` points its patch tables at the rebuilt trees. `[patch.crates-io]` lists
  each bevy subcrate an ecosystem crate names by crates.io version (18 today) and every forked
  crate. `[patch."https://github.com/robtfm/bevy"]` catches `bevy_dui`, `bevy_ecss` and `boimp`,
  whose manifests name a former fork by git. With `Cargo.lock` nothing fetches that repository.
  A resolve without it reads the branch once.
- The rebuilt trees are not workspace members, so the explorer's Clippy and Rustfmt gates never
  reach them. Cargo doesn't cap a path dependency's rustc warnings: bevy's root `Cargo.toml`
  allows `dead_code` (code our feature set leaves unused) and `mismatched_lifetime_syntaxes` (a
  lint newer than bevy 0.16); other warnings stay visible. Don't run Clippy inside bevy's own
  workspace: its code predates this toolchain's lints.
- Backport an upstream change: `git fetch https://github.com/bevyengine/bevy <sha>`, then
  `git format-patch -1 --stdout <sha> | git apply --directory=rendering/bevy-explorer/bevy`, then
  `export`. Name the upstream PR on this page, as the entries above do.
- Our delta over upstream, for a migration: `python3 tooling/patched-sources.py divergence bevy` lists
  files and lines changed by crate, and the series is the delta itself.
- CI reuses cached Bevy and engine builds until their inputs change: Cargo would otherwise rebuild a path
  dependency after every checkout, since a checkout makes every file new.
  `tooling/bevy-mtimes.py` records the Bevy, fork, engine and protocol files' blob ids and times with
  the cache and gives
  unchanged files, and directories that hold the same names, their times back; every job that caches the explorer's target runs it.
- Never share a `target/` directory with a checkout that builds a different bevy.
- The lockfile is edited by hand, never with `cargo update`: see the dependencies section of
  [contributing](contributing.md).
