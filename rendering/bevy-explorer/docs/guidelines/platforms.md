# Platforms

The engine builds three ways from one workspace. The architecture diagrams show where each
piece runs; this page is the list of things that differ and bite.

| | Native | Web | Headless |
|---|---|---|---|
| Binary | `decentra-bevy`, plus the `dcl_deno_ipc` sidecar and the `decentra-bevy-cef` helper | wasm in the engine worker, loaded by `deploy/web/engine/` | `headless`, behind the `headless` feature |
| Scenes run in | V8 isolates in the sidecar | one sandbox worker per scene | V8 isolates in the sidecar |
| HUD | react-web rendered by CEF into an engine texture | react-web as DOM over the canvas | none |
| Rendering | wgpu | WebGPU in a render worker, Chrome only | none |

## Keep the boundary clean

Make as much code platform-independent as possible, behind a clean boundary. Generic backend
functions go in the `platform` crate, which has one `native` and one `wasm` implementation of
the same signatures: `fetch`, `websocket`, config and scene-file IO, pointer lock, the async
lock, the default camera bundle. Code that needs one of those calls `platform::x` and never
sees a `cfg`. Backend functionality that only one caller needs lives next to that caller
(the livekit bridge in `comms`, the page-event dispatch), behind a `cfg` there, not in
`platform`. Either way the platform-specific part is a leaf; the logic that uses it is shared.

## Web

- **Chrome only.** Design against Chrome's feature set (multi-memory, JSPI, `Atomics.waitAsync`)
  and do not add cross-browser hedges.
- **No threads as you know them.** `TaskPool::spawn` runs on the JS event loop: no `Send`,
  no blocking, no `std::thread`, no busy-waiting. Use `web_time` for clocks. The engine worker
  runs the app world; the render world lives in its own worker.
- **The log level is compiled in.** `RUST_LOG` does nothing at runtime on wasm. A debug log you
  want to see needs a rebuild with the filter changed.
- **Engine switches are page params, not launch options.** A web-only knob is a `?param=` the
  page reads and hands to a wasm export, like `?guest=`. Adding it to `LaunchOptions` pollutes
  the native `--help` for something native can't use.
- **Derivatives in uniform control flow.** `dpdx`, `dpdy`, `fwidth` and `textureSample` inside
  a non-uniform `if` fail on web only. Hoist them above the branch.
- **Custom transparent materials must write the focus target.** The fork's transparent pass
  has a second output at `@location(1)` (`TRANSPARENT_FOCUS_OUTPUT`). A fragment shader that
  omits it fails pipeline creation on web and is silent on native.
- **16-bit textures lose precision** through the web sampler. Expect banding if you rely on it.
- **Build.** `just wasm` in `source` builds the wasm package and the sandbox worker bundle and
  starts the page dev server and the bridge. If a stale dev server is already on the port it
  gets reused; kill it first. The distributable is `make web` from repository-root
  `apps/bevy-explorer`, which builds the wasm package, sandbox worker and HUD into `dist/web`
  without serving them (see [Running](running.md#web)). `just wasm` also runs from Git Bash on
  Windows: a wasm build there gets `file!()` paths with backslashes, which the fork's
  `embedded_asset!` normalizes. Use Git Bash for every `just` recipe there: from PowerShell
  `just` finds no `sh`, and the `bash` on its `PATH` is usually WSL's. Windows has no `make`:
  `make web` is `python build-web.py` (`--no-opt` for `web-dev`), from any shell.
- **Exit work runs before `Last`.** On `AppExit`, `close_rooms_on_app_exit` blocks in `Last`
  (`comms`), so on web a system in `Last` may never run again. Anything that must reach the page
  at exit, such as the benchmark stream's final bytes, runs in `Update`.
- **Two GPUs: Chrome takes the low-power one.** On a laptop with integrated and discrete GPUs,
  Chrome's WebGPU renders on the integrated one while native wgpu takes the discrete one: on one
  laptop the same web build ran much slower (measurements (evidence supplied for the task)).
  Start Chrome with `--force-high-performance-gpu` to compare with native (the benchmark's browser
  leg does).
- **Rollout.** The web build ships to zone and org by a percentage walk; publishing any branch
  moves zone. Ask before publishing.

## Native

- Build and run with `just native-debug` or `just native-release`, with the client's own
  arguments after them (`--realm <url> --position x,y`). Both bundle the HUD when it is stale.
  A bare `cargo run` gives a login screen with no HUD because the bundle isn't built. Both
  binaries build: `cargo build --package dcl_deno_ipc` then `cargo build`. `just setup-cef`
  fetches the CEF framework once; only macOS falls back to it at runtime, so the `just` loop is
  macOS-first (see [Running](running.md#native) for Linux and Windows). Packaging follows the
  [native build steps](running.md#native).
- **Task pools.** Compute, which runs both worlds' parallel systems and `par_iter` work, gets
  half the cores (2 to 8; `--compute-threads` overrides it). IO is fixed at 8 because bevy runs
  every asset loader's decode there; async takes a quarter of the cores (2 to 8). The pools add
  up to more than the core count, so a heavy load shares cores with the frame's systems.
- **GPU uploads.** Throttled asset uploads are capped at 10 MB per frame, as on the web
  (`--gpu-bytes-per-frame`, 0 = unlimited). Assets with `RenderAssetTransferPriority::Immediate`,
  which includes every mesh, bypass the cap.
- **CEF `localStorage` is ephemeral.** Anything the native HUD must remember goes through
  `lib/prefs`, which the engine persists to disk. No native popups from the HUD: CEF's popup
  windows aren't composited into the texture.
- **Windows paths.** `Path::is_absolute` is not a containment check there; see the security
  page.
- **Windows builds.** v8's build script symlinks its source into the target dir when the two sit
  on different drives, and Windows refuses without Developer Mode: keep `CARGO_HOME` and
  `CARGO_TARGET_DIR` on one drive. `cef-dll-sys` needs Ninja on `PATH`; Visual Studio ships one
  under `Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja`. A cold build at a high job count can
  run LLVM out of memory: limit it with `CARGO_BUILD_JOBS`, and keep tens of gigabytes free for a
  debug target dir (measurements (evidence supplied for the task)). Keep `CARGO_TARGET_DIR` short
  and free of spaces: under a worktree's `.../source/target`, LiveKit's prebuilt libwebrtc headers
  pass the 260-character path limit and `webrtc-sys` stops with C1083 on an `absl/...` header, and
  GN-style build scripts break on a space. protoc is the `protoc-<version>-win64.zip` release that
  `apps/bevy-explorer/install-protoc.py` pins (that script only runs on CI): put its `bin` on
  `PATH` or set `PROTOC`. `CEF_PATH` takes a Windows path; in Git Bash,
  `export CEF_PATH="$(cygpath -w ~/.local/share/cef)"` after `just setup-cef`.
- **A PR runs the Windows and macOS tests only for platform inputs.** A Rust change under
  `rendering/bevy-explorer/source` runs clippy, the native test suite on Linux, the headless smoke
  test and the web build (`tooling/bevy-ci.py` at the dcl-one root selects them). A draft does not
  run the test suite and the headless smoke test: they start when the PR is marked ready for
  review, or on the next push with the `ci:engine` label. The suite adds
  Windows and macOS when the PR changes the root `Cargo.toml`, a `build*.rs`, `.cargo`, the
  toolchain, `local-harness/benchmark`, `local-harness/web-driver`, a file or folder named
  `windows` or `macos`, the locked package set in `Cargo.lock`, or a CI input outside the engine
  source (the workflow, the selector, `library/vendor/rusty_enet`, `apps/bevy-explorer` build
  scripts); when it is labelled `ci:macos` (then push or reopen); and on every main push and
  manual run that runs the suite. Label a PR whose change is platform-specific, or the break shows
  up on `main`: a `cfg(target_os)` block, or a crate `Cargo.toml` that adds a
  `[target.'cfg(...)'.dependencies]` entry on a crate already in `Cargo.lock`, runs on Linux only.

## Memory budgets

`MemoryStatus` samples engine-process RSS in native rendering and headless clients, and live
shared engine-heap allocations on Web, at most four times a second. `GpuMemoryBudget`
independently samples the renderer's selected native adapter or device
at most once a second. Each pool carries used bytes and a positive budget. Usage can exceed the budget.

| Renderer | GPU reading |
|---|---|
| Windows DX12 | Current process usage and the OS budget for the local segment on the renderer's node. [DXGI fields](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_4/ns-dxgi1_4-dxgi_query_video_memory_info) |
| Windows or Linux Vulkan | Estimated process usage for each device-local heap with a usable driver budget. Each heap has its own limit. Requires `VK_EXT_memory_budget`. [Vulkan memory budgets](https://docs.vulkan.org/refpages/latest/refpages/source/VkPhysicalDeviceMemoryBudgetPropertiesEXT.html) |
| macOS Metal | Allocated resource bytes and the device's recommended working set, including on unified-memory devices. If the renderer holds the device lock, the previous reading stays visible and sampling retries the next frame. [Metal working set](https://developer.apple.com/documentation/metal/mtldevice/recommendedmaxworkingsetsize) |
| Browser WebGPU or renderless headless | `NotApplicable`. WebGPU exposes no driver memory budget. [WebGPU adapter interface](https://www.w3.org/TR/webgpu/#gpuadapter) |
| Native query unsupported, failed or pending | `Unavailable`. A consumer has no current reading for this adapter. |

### Using the budgets in future asset loaders

Estimate the **additional peak bytes** of the operation in each domain, including concurrent
uploads and any old allocation still in use during replacement. Backing allocations determine
which budget applies. A scene, avatar or glTF can contain several of the following.

| Asset | RAM cost | GPU cost and possible response |
|---|---|---|
| Textures and video frames | Decode and upload staging, plus any retained pixels | Count blocks in the uploaded format at each mip, including layers or 3D depth and samples. A smaller texture or shorter buffered video queue can reduce the peak. |
| Meshes | CPU vertices, indices, colliders and staging | Charge growth of backing vertex/index buffers. Meshes sharing a slab reuse its allocation. A lower LOD can avoid growing it. |
| Audio and scene data | Decoded PCM, scene state and CPU caches | Consult RAM for CPU-only assets. A texture visualizing audio has its own GPU cost. |
| Render targets, shadow maps and imposters | Any separately retained CPU copy | Count each color, depth, multisample and resolve allocation once. A smaller target, shadow map or sample count lowers the GPU cost. |

Track shared allocations by backing identity through release. Views, handles and multiple users
of the same resource do not add another copy. Compare an estimate with its eligible native pool;
separate Vulkan heaps cannot lend each other space. On unified memory, RAM and GPU readings can
cover the same pages, so adding them would double-count. When the driver reading is absent, Web
can still track estimates against an explicit quality-profile allowance and handle allocation
failure by reducing quality. That allowance should remain distinguishable from measured headroom.

## GPU timestamps

The benchmark's GPU time (`gpu/frame_time`, see [GPU time](benchmark.md#gpu-time)) is the
difference of two timestamps that empty compute passes write with `timestamp_writes`, which needs
`wgpu::Features::TIMESTAMP_QUERY` on the render device. The engine sets no `WgpuSettings` of its
own (the web render worker uses the default too), so Bevy's default `Functionality` priority
requests every feature the adapter reports, and the feature is on wherever the backend exposes
it. Without it the engine logs one info line under `gpu_time` and has no GPU time. In the
Explorer's wgpu (upstream `v24.0.5` plus the series in `rendering/bevy-explorer/patches/wgpu`):

| Backend | `TIMESTAMP_QUERY` | Tick period | Checked on hardware |
|---|---|---|---|
| DX12 (Windows) | always | the queue's timestamp frequency | Windows, RTX 5080 Laptop GPU: the GPU tests, and the packaged release standalone with FXC and `--no_gltf` |
| Vulkan (Windows, Linux) | always | the device's `timestampPeriod` limit | Windows, the same GPU: the GPU tests and the packaged release standalone, which picks Vulkan by itself; Linux not |
| Metal (macOS) | when the device samples counters at stage boundaries (macOS 11 or newer) | fixed: 1 ns, 83.333 ns when the device name starts with "Intel" | not checked |
| WebGPU (Chrome) | when the browser's adapter lists `timestamp-query` | 1 ns (WebGPU reports nanoseconds) | Chrome on Windows (RTX 5080 Laptop GPU) |

- **DX12 and Vulkan** set the feature for every device: `wgpu-hal/src/dx12/adapter.rs` lists it
  among the features "always available on d3d12", and `wgpu-hal/src/vulkan/adapter.rs` sets it
  without reading the queue's timestamp bits.
- **DX12 in the packaged Windows folder** does not reach a frame, with the GPU time off too. The
  folder holds CEF's `dxcompiler.dll` and `dxil.dll`, and Bevy picks the DXC shader compiler
  whenever both sit in the working directory (`bevy_render/src/settings.rs`); wgpu's own
  indirect-validation pipeline then fails (`Validator version in metadata (1.9) is not supported;
  maximum: (1.7)`) and the device is lost at creation. `WGPU_DX12_COMPILER=fxc` starts it, and
  the full scene then runs out of descriptors (`Unable to allocate descriptors`); `--no_gltf`
  keeps the scene small enough to run. Vulkan, which wgpu picks by itself on the RTX 5080 laptop,
  is unaffected.
- **Metal.** A compute pass's `timestamp_writes` become the encoder's start and end samples
  (`wgpu-hal/src/metal/command.rs`), which stage-boundary sampling covers
  (`wgpu-hal/src/metal/adapter.rs`). Whether an empty pass writes its samples is not checked.
  wgpu does not convert Metal's GPU timestamps to CPU time; it hard-codes the period, and its own
  comment names Apple silicon (M1, M2) and AMD for the 1 ns. Not checked here.
- **WebGPU.** `wgpu/src/backend/webgpu.rs` maps the feature to `timestamp-query` and passes the
  pass `timestampWrites` through.
- **Chrome quantises timestamps** unless its "WebGPU Developer Features" flag is on
  (`chrome://flags/#enable-webgpu-developer-features`;
  [Chrome's WebGPU 121 notes](https://developer.chrome.com/blog/new-in-webgpu-121) give 100 µs).
  The benchmark runner does not set it. In the checked Chrome every web GPU value is a multiple of
  65.536 µs (measurements (evidence supplied for the task)). That is Chrome's behaviour, not wgpu's.
- **Headless** has no render app, so it has no GPU time.

## Headless

- The headless binary is the production authoritative scene server and the local preview
  server. It runs the same `ScenePlugin` and scene loop with a narrower plugin set and no
  render app. `source/docs/headless-sdk-preview.md` and `source/docs/PRESENCE_ISOLATION.md`
  cover the server-side contracts. Resolve scene-response isolation proposals
  and their review status through authorized task context; proposals require
  a user verdict before implementation.
- **It is feature-gated.** `--bin headless --no-default-features --features headless,livekit`. Running the wrong bin through a
  pipe exits 0 and tells you nothing.
- **Missing resources only show at runtime.** The headless plugin set skips `InputManagerPlugin`
  among others, so a system that takes a `Res<>` from a skipped plugin compiles and panics on
  first run. Run the headless binary against a scene before pushing anything that touches
  shared systems.
- Every scene-facing capability needs the headless question answered: see the security page.
