# Benchmark

The bare-metal A/B benchmark of the web explorer. On self-hosted laptops, Chrome runs a pull
request's web app against main's, interleaved, and the report goes into the *Performance* section
of the pull request's CI status comment: samples, CPU and GPU frame time (average, 1% and 0.1%
worst) and exceptions per run, with a verdict for each. It has three parts:

- the explorer's unattended run (`--autopilot`, `?autopilot` on the web), which writes a CSV and a
  log ([Launch options](#launch-options), [The run](#the-run), [Files](#files));
- `benchmark` (`local-harness/benchmark`), which serves the web app, runs Chrome, stores what the
  page streams back ([Browser stream](#browser-stream)) and writes the report
  ([The report](#the-report), [The runner](#the-runner));
- the workflow *Bevy Explorer / Benchmark*, which finds the builds in CI's artifacts, runs a leg on
  each laptop and posts the report ([How a run starts](#how-a-run-starts),
  [The workflow](#the-workflow), [The fleet](#the-fleet)).

For comparisons with unity-explorer, obtain its benchmark source and dated results through
authorized task context; record the revisions, workload and hardware with the comparison.

Read this before changing `--autopilot`, the benchmark CSV, the benchmark log format, the browser
stream, the report or the workflows: the report parses the files byte for byte, and one runner
serves web apps built from different commits.

## How a run starts

Only open pull requests of this repository are measured, and only from a passed *Bevy Explorer /
CI* run, whose artifacts hold the builds ([The workflow](#the-workflow)).

- **Every explorer change.** A pull request is measured each time a CI run of its head passes
  with a web app built (`workflow_run`). CI builds the web app when the pull request changes a web
  input (the engine, the HUD, the page), so a run that built none, such as a backend-only change,
  starts nothing.
- **By hand.** *Run workflow* on *Bevy Explorer / Benchmark* (`workflow_dispatch`) with `pr` (its
  number or its URL) and optionally `change_sha` (a commit of the pull request; its head by
  default), `base_sha` (a commit on main; by default the newest main build that has each
  artifact) and `runs` (1 to 3). Starting one takes write access.
- **Optimized builds only.** CI builds every web app with wasm-opt, on pull requests as on main,
  unless `BENCHMARK_ENABLED_RUNNERS` is `none`: then a pull request that compiles the engine skips
  it, while one that reuses main's compiled engine stays optimized. Setup reads `engine.optimized`
  from both web apps' `build-info.json` and refuses a run whose change or base build is
  unoptimized, such as one built while the fleet was off. A manual *Bevy Explorer / CI* run on the
  branch builds optimized; its completion starts no benchmark, so start one by hand after it.
- **Nothing starts, silently,** for a CI run that failed, built no web app or belongs to no pull
  request, a pull request that is closed or comes from a fork, and a CI run of a commit that is no
  longer the head (the head's own run starts the benchmark). The reason is in the *Inputs* job's
  summary.
- **A note in the performance section instead of legs** when the builds are unfit: an unoptimized
  change or base build, no passed main run that still has a web app, no Linux runner to write the
  report, or no laptop enabled; by hand, also no passed CI run of the commit yet, or no web app in
  it (CI built none, or the artifact expired).
- **One run per pull request.** The next run for the same pull request cancels one in progress:
  any passed CI run of it that a `pull_request` event started (a re-run of an older commit's
  included, which then skips), or a manual run naming it by number (one naming it by URL is a
  group of its own). A manual CI run on its branch cancels nothing.

## What a leg runs

Each enabled laptop of [the fleet](#the-fleet) runs one leg, a job on its self-hosted runner
(*Chrome*, in `bevy-explorer-benchmark-leg.yml`), labelled `<runner label>-chrome`, for example
`intel-i5-chrome`. The job checks nothing out:

1. It removes an earlier session's files: the runner, both web app folders, `runs_on.txt` and every
   run file (`runner`, `runs_on.txt`, `change_build*`, `base_build*`). A file it cannot remove,
   one that a leftover process holds, fails the job.
2. It downloads the `benchmark` runner for the laptop's operating system and the change and base
   web apps from their CI runs, and puts the runner on the PATH.
3. It warms each build up (`benchmark warmup`): the run signs in, arrives in world and ends,
   measuring nothing. The first warm-up on a laptop creates the guest that every later run reuses.
4. `benchmark measure` runs change, base, change, base ..., `RUNS` times each (3 by default), with
   a 15 s settle after each run. Each run's files become `change_build_<i>.csv/.log` and
   `base_build_<i>.csv/.log`; `runs_on.txt`, holding the label, comes last.
5. It uploads `benchmark-result-<label>`: `runs_on.txt` and every run's `.csv`, `.csv.partial`
   and `.log`, kept 7 days. It does so after a failed or cut-off `measure` too, so the runs that
   finished can be looked at in the artifact, but not when `measure` never started. The report
   reads a leg only once `measure` wrote its `runs_on.txt`, at the end: after a failed or cut-off
   `measure` it says the leg "produced no results" ([The report](#the-report)). A re-run of the
   job replaces the artifact.

Every run is the explorer's unattended session ([The run](#the-run)) in the live
`supportteam.dcl.eth`: sign in as the local guest, wait until in world, stand at the spawn point
for 90 s recording each frame's CPU and GPU time, then quit. The character does not move: the
smallest, most repeatable workload. Both builds are measured fresh in one session, interleaved,
so they see the same world content, thermal state and background load, and a drift of the
machine or the world between sessions cannot read as a regression.

`supportteam.dcl.eth` is live content: editing it changes the workload of every later run. Both
builds of a leg see the same content, so an edit gives no false verdict, but it shifts every
absolute number: do not edit it casually, and never during a run.

Limits: a measured run 480 s (`RUN_TIMEOUT_SECONDS`), a warm-up 300 s (`WARMUP_TIMEOUT_SECONDS`,
a backstop), each warm-up step 10 minutes, the `measure` step 60 minutes, the job 90 minutes.
Closing Chrome takes up to 30 s and the settle 15 s, so `runs` × 2 × (480 + 45) must stay under
3600: setup refuses more than 3 runs.

## The report

`benchmark report` (`local-harness/benchmark/src/report/`) turns the legs' results into one
Markdown section. It reads files only; the workflow writes it into the pull request's CI status
comment ([The workflow](#the-workflow)).

- **Sections.** Every folder under its `DIR` that holds a `runs_on.txt` is a section, named from the
  label: a fleet leg's `<runner label>-chrome` is `<machine> (Chrome)`, for example
  `intel-i5-chrome` is *Intel Core i5 (Chrome)* ([The fleet](#the-fleet) has the names); any other
  label, such as the by-hand default `local-chrome`, is shown as it is. A section reads each
  `change_build_<i>.csv` and `base_build_<i>.csv` with the `.log` of the same name; with no base
  runs its table has one column and a note says so.
- **Layout.** `## <title>`, a line naming the pull request, the change and base builds and the run,
  the **Overall** headline when a section has data, a collapsed *How to read this table*, the
  sections in path order, and a line for every expected label that left no results:
  "*name* produced no results (its job failed, timed out or was skipped)".
- **Exit code.** 0 when a section has data; 2 when none has, after writing the file anyway; 64 for
  a `--pr` or `--expected-runs` that is not a whole number, with no file written.

### Reading the results

Each section is a table, **Metric | Baseline | Change | Δ | Result**: *Samples* (the median rows
per run, × the runs), CPU and GPU *average*, *1% worst* and *0.1% worst* frame time (the mean of
the worst 1% or 0.1% of a run's frames, the
[GamersNexus](https://www.youtube.com/watch?v=WcTxrzFqdyw#t=34m17s) method), and *Exceptions per
run*.

- A value is the **median** of the build's runs, followed by the **(min–max)** range when the runs
  differ. A wide range means a noisy metric, where small differences mean little.
- **Δ** is Change minus Baseline: negative means Change is faster.
- **Result:**
  - **🟢 _N_% faster / 🔴 _N_% slower:** the two builds' runs are fully separated (every run of one
    build faster than every run of the other; with 3 runs a build, the most extreme outcome of a
    permutation test, one-sided p = 0.05), and the medians differ by at least 3% and at least
    0.5 ms. Only then is a percentage shown, so a noisy number never reads as a win.
  - **⚪ within noise:** the runs overlap or the difference is small. Treat it as no change.
  - **— informational:** the 0.1% worst rows. They average the two or three worst frames of a run,
    so one OS hiccup moves them by tens of percent; they never get a verdict.
  - **⚠️ no verdict — _reason_:** the sessions cannot be compared. *too few runs*: a build has
    fewer than 2 usable runs. *sample counts differ*: the builds' median row counts are more than
    1.5× apart, so one did not run the whole session. *GPU timing not working* (GPU rows only): a
    build's GPU average is under 1 ms, so its GPU column is mostly zeros ([GPU time](#gpu-time)).
    *implausibly small values*: a median under 1 ms.
- **Exceptions per run** is the average number of exceptions in a run's log, counted up to the exit
  marker, so teardown errors do not count ([The run](#the-run)). It is 🔴 *more errors* or 🟢 *fewer
  errors* only when the builds differ by at least 2 per run and by 1.5× the other build (taken as
  at least 1), so one transient network error does not flip it; otherwise ⚪ *no significant
  change*. Kinds the baseline never threw are listed under the table, and the *Exception
  breakdown* totals every kind across the runs as `[CATEGORY] Type`: the explorer's report category
  and the exception type.
- A run that logged **unusually many exceptions**, at least 10 and 5× the median of its build's
  runs (a service was down during it), is left out of every number and named under the table.
- "Only *n* of *m* … runs produced data": the missing runs left no CSV (the app crashed or never
  reached the world) or only a `.csv.partial` ([The runner](#the-runner)).
- "No Baseline run produced data": every base run of the section crashed, or none was measured
  (`leg` without `--base`). The table shows the change alone, with no verdicts.
- "Could not gather the results of …": a file of the section could not be read, for example a CSV
  with a header and no rows, and the whole section is lost. `measure` sets such CSVs aside as
  `.csv.partial`, so a leg's own files do not cause it.
- The **Overall** headline is what to act on: the regressions if there are any, else the
  improvements, else "✅ no significant changes"; when no section had base runs, "⚠️ nothing was
  compared — no Baseline run produced data". It names a metric only when it moved on two or
  more laptops, or by 10% or more on one: with about five verdict rows on each of up to five
  laptops, one modest 🟢 or 🔴 cell turns up by chance now and then, so a single cell is never a
  conclusion. An exceptions verdict needs two laptops.

Treat small GPU differences with caution: in this scene the GPU average is a few milliseconds
(measurements (evidence supplied for the task)), near the noise floor, so most GPU changes rightly stay ⚪.

## Launch options

| Meaning | Native | Web |
| --- | --- | --- |
| Run the benchmark session | `--autopilot` | `?autopilot` (launch param) |
| Write the CSV | `--csv <path>` | `?perfCsv` (page param): the stream carries the CSV |
| Write the log | `--log-file <path>`, or `-logFile <path>` | `?perfStream=<same-origin path>` (page param): the log streams there |
| World | `--realm supportteam.dcl.eth` | `?realm=supportteam.dcl.eth` |
| Walk in while measuring (by hand only) | `--autopilot-walk-to <x,y>` | — |

The page hands `perfStream` and `perfCsv` to `engine_set_perf_stream(path, csv)` before
`engine_run` (`src/web.rs`). A launch with no CSV destination is the warm-up.

The web switches act only on a loopback host (`localhost`, `*.localhost`, `127.0.0.1`, `[::1]`),
where the runner serves the web app. Anywhere else the page ignores `autopilot`, `perfStream`
and `perfCsv` outright: it does not boot in autopilot mode, leaves the `autopilot` key out of the
engine's launch options (absent, not `false`), starts no stream, and offers no warning to click
through. A link cannot start an unattended session or send the log anywhere. Natively
`--autopilot` is the user's own flag. The Sites `/play` page never runs the benchmark: it ignores
the switches on every host. `perfStream` is a path on the page's own origin: a leading slash, no
query or fragment.

The native client is measured by hand; the fleet runs only the web app. Per build, one warm-up
launch without `--csv`, then the measured launches interleaved with the other build's, 15 s apart:
`<exe> --log-file <dir>/<build>_<i>.log --autopilot --csv <dir>/<build>_<i>.csv --realm
supportteam.dcl.eth`, where `<build>` is `change_build` or `base_build`. Then write a label into
`<dir>/runs_on.txt` and run `benchmark report <dir>` ([The runner](#the-runner)). A run that
leaves no CSV counts as crashed.

A walk-in measures content that loads during play instead of behind the loading backdrop:
- **How it moves:** with `--autopilot-walk-to <x,y>` (native only), once measuring starts the
  player moves in a straight line from where it arrived to the centre of that parcel at 6 m/s,
  facing it, then stands there for the rest of the 90 s.
- **Where to start:** arrive with `--position` more than the load distance from the target scene,
  so it loads on the way. Walking never raises the backdrop; only a teleport or a realm change
  does.
- **Start on an empty parcel or an SDK7 scene.** The explorer never starts an SDK6 scene (no
  `runtimeVersion` 7 in its metadata), so a run that arrives inside one never leaves the backdrop
  and fails at the 300 s arrive timeout.
- **How the walk is done:** it sets the position on the ground plane each frame
  (`src/autopilot/walk.rs`), so no wall stops it and every run takes the same path; the physics
  keeps the height.
- **Example, into Genesis Plaza** (parcels −3…3, −4…9): `--realm
  https://realm-provider-ea.decentraland.org/main --position -12,2 --distance 100
  --autopilot-walk-to 0,2` arrives on an empty parcel 136 m west of it and walks 192 m in,
  through the SDK7 "Genesis Plaza - Roads".

## The run

`crates/common/src/autopilot.rs` holds what the run and the output share: `Autopilot { measure }`
and `AutopilotPhase`.

- `Arriving` while signing in and loading, nothing recorded; `Measuring` from the first frame in
  world; `Ending` after 90 s of real time (exit code 0), or on failure (exit code 1: no usable
  identity, sign-in failed, signed in as anything but a guest account, not in world within
  300 s, out of world while measuring, a CSV that cannot be created, written or flushed). A
  `--log-file` that cannot be created, or a `--csv` whose folder does not exist, ends a desktop
  run with exit code 1 before the app starts, so no run goes uncounted; the reason is also
  written to the session log at once. The warm-up goes from arriving straight to `Ending`.
- Then one info line under target `autopilot`, `[ExitUtils] Exit requested at <RFC 3339 UTC>`:
  the exit marker. The report counts no exception after it, so teardown errors don't count.
  Desktop exits with the code; the web page sends `end`. After the marker a native run still
  closes its rooms and CEF before the process ends, and that can take a while or hang, so
  whatever launches a native run needs a kill timeout of its own; the runner has one for Chrome.
- In world means signed in, no `WorldHold`, the realm connected and the player's `OutOfWorld`
  seen and then cleared (the spawn scene is ready). Leaving it while measuring ends the run
  with exit code 1 and one error under `autopilot`: the rows of an emptied world would read as
  a faster build. Each phase change is an info line under `autopilot` (`src/autopilot.rs`).
- Unattended: the run signs in only with the local guest, never with an account stored on the
  machine or in the Chrome profile. The guest is made once, with no network call, and its root
  key is kept; every run signs a fresh 30-day identity with that key, so the address never
  changes. Natively the engine keeps the root key in its config (`/login_local_guest`); on web
  the shell keeps it in the profile (`react-web/src/shell/localGuest.ts`) and holds the signed
  identity for the page without storing it, so a stored account is neither used nor replaced,
  and signing in or out of an account leaves the key alone. The shell makes the guest on
  loopback hosts only. Natively and on the web, a kept key that is not a key fails the run and
  is left as it is, instead of becoming a new account. The engine also ends a run signed in as
  anything but a guest account (exit code 1), a throwaway guest included. The app page opened
  on its own, outside the shell, has nowhere to keep the key: the sign-in fails, and with it the
  run. No thirdweb call: thirdweb refuses `localhost` origins.
- No lobby, destination picker, welcome page or sign-in screen. Scene permission prompts are
  answered without UI by a fixed table (`common::autopilot::permission`): Fetch and Websocket
  allowed, since scenes need them for their workload; Teleport, ChangeRealm, portables, Web3,
  clipboard and OpenUrl denied, so the player stays at spawn. What the normal rules would ask a
  person for a scene the player is not in is denied; Fetch, Websocket and HideAvatarsNametags,
  which any scene may use, follow the table. Stored rules are ignored, so every machine runs the
  same policy.
- On web, when the engine ends its own run (`AppExit`), it tells the page (`__engineExit`, with or
  without a stream), and the crash watchdog and the bridge-readiness banner stand down
  (`__engineExited`).

## Files

- **CSV**: UTF-8 without BOM, CRLF line ends, the header
  `"Frame","CPU Time","GPU Time"`, then one row per measured frame, `frame,cpu_ms,gpu_ms`, with `.`
  decimals and no grouping or quotes. Created when `Measuring` starts, so a run that never arrives
  leaves none. A CSV that cannot be created, written or flushed ends the run with exit code 1
  (`common::autopilot::CsvFailed`).
  - CPU is the frame's wall-clock time (`Time<Real>` delta): the main thread's whole frame, waits
    included, so the CPU average ≈ 90 s ÷ samples. `Time<Real>` takes the render world's
    end-of-render instant, so a row is offset from the frame it names by two to three frames; the
    sum and the percentiles are exact.
  - GPU is the latest `gpu/frame_time` measurement ([GPU time](#gpu-time)), 0 while there is
    none. A measurement is recorded after the GPU finishes the frame and its readback has been
    read: three frames after that frame when the GPU finishes each frame before the next one is
    submitted, later when it does not. So a row's value is that of the frame three frames before
    it (measured in world on desktop), up to one frame behind the row's CPU value. A row whose
    frame gave no measurement carries the previous value.
- **Log**: every event as `HH:MM:SS.mmm - <color=#RRGGBB>[target]</color>: message`, the time in
  UTC, the colour carrying the level, LF line ends; further lines of a message are indented four
  spaces, so none can pass for an entry. Every error-level event and every panic is instead
  a three-line exception block: an entry with an empty message, then
  `Type: message` (`Error` for an event, `PanicException` for a panic), then a line containing
  `DebugLogReportHandler:LogExceptionInternal` and, for an event, its `file:line`. The colour
  tags and the handler name are format markers the report counts by. Two strings are markers
  wherever they stand, so no message or category holds either: the exit marker and the handler
  name are broken with a zero-width space after their first character, except in the run's own
  exit line and each block's last line. A scene's error text cannot end the count or add to it.
- Both formats live once in `crates/perf_output/src/format.rs`; the desktop sinks and the page's
  stream take its bytes unchanged. The log layer first replaces every credential in a message or
  a category with `REDACTED` (`crates/perf_output/src/scrub.rs`): `access_token=` values, string
  fields named `access_token`, `accessToken`, `privateKey`, `private_key` or `signature`, and
  base64 blobs that decode to text naming a private key. The log is the run's artifact, so what
  logs it cannot leak a room token or an identity. On web a panic's block comes from the engine's
  panic hook through the same scrub (`perf_output::layer::panic_block`); the page's own blocks
  hold fixed messages only ([Browser stream](#browser-stream)). `--log-file` adds a sink: the
  session log and the crash report's touch file stay as they are.

## GPU time

The CSV's GPU column carries the diagnostic `gpu/frame_time` (`common::gpu_time::GPU_FRAME_TIME`,
in milliseconds). `crates/perf_output/src/gpu.rs` measures it in every rendering client build,
native and web, with no flag; the headless build has none. The CSV writer
(`crates/perf_output/src/csv.rs`) and the engine's system info panel
(`crates/system_ui/src/sysinfo.rs`) read it from the diagnostics store.

- **What it is.** The time between two GPU timestamps that bracket the frame's render-graph work.
  Two empty compute passes write them, one recorded before every node of the main render graph
  and one after every node, so both go to the GPU in the frame's own submission. Not counted:
  the render world's CPU work to prepare the frame, uploads queued ahead of the submission and
  standalone submits outside the graph. The brackets enclose the nodes that exist when the
  plugin finishes; on the first render frame it checks that every node of the graph runs
  between them and names any that does not in one warning under `gpu_time`.
- **How it is read.** The two timestamps are resolved into one of four readback slots, which is
  mapped without waiting; a later frame reads it and converts the ticks with the queue's timestamp
  period. A frame is not measured while every slot is in use: nothing blocks, nothing grows, and
  the CSV row carries the previous value. A readback that fails retires its slot for good, with
  one warning under `gpu_time`; with every slot retired nothing more is measured.
- **Lag.** A measurement is recorded three frames after the frame it measures when the GPU
  finishes each frame before the next one is submitted, later when it does not (measured in
  world on desktop). So a row's GPU value is the GPU time of the frame three frames before it,
  or of an older one. The row's CPU value is two to three frames behind its frame, so GPU trails
  CPU by up to one frame.
- **No timestamp queries.** A device without `wgpu::Features::TIMESTAMP_QUERY` logs one info line
  under target `gpu_time` (`GPU frame time unavailable: the adapter has no timestamp queries`)
  and registers nothing: the CSV's GPU is 0 and the panel shows a dash. The report then gives no
  GPU verdict.
- **Chrome.** It quantises timestamps unless it starts with WebGPU developer features, which the
  runner does not do: a web GPU value is a multiple of Chrome's step, 65.536 µs in the checked
  Chrome (measurements (evidence supplied for the task)), well under the verdict's 0.5 ms threshold. A
  frame whose two timestamps quantise to the same value gives no sample, and the CSV repeats the
  previous value.
- **Seeing it.** The diagnostics log line prints when `--log-fps` is on (default: the
  `graphics.log_fps` setting, on for desktop, off on the web), also with the native CEF HUD up;
  the system info panel's `GPU :` row (`--sysinfo`) shows only while the `graphics.log_fps`
  setting is on, as the smoothed value or a dash. The panel belongs to the engine's own UI, which
  the native HUD hides: look for the row in a run without the HUD (for example
  `--system-scene none`).
- **Cost.** Its cost is in measurements (evidence supplied for the task).
- Which backends expose timestamp queries is in the [platform guide](platforms.md#gpu-timestamps).

**Tests.** The unit tests cover the sample arithmetic, the slot ring and the publisher. Two test
files under `crates/perf_output/tests/` need a GPU, so their tests are `#[ignore]`d and CI
skips them: `gpu_timestamps.rs` (an empty pass writes its timestamp) and
`gpu_frame_time.rs` (a known compute workload
reads its own time, a 50 ms stall in the render world is not counted, and a device without
timestamp queries logs its one line and registers nothing). Run them per backend, from
`rendering/bevy-explorer/source`:

```bash
WGPU_BACKEND=dx12   cargo test -p perf_output --test gpu_frame_time -- --ignored --nocapture
WGPU_BACKEND=vulkan cargo test -p perf_output --test gpu_frame_time -- --ignored --nocapture
WGPU_BACKEND=dx12   cargo test -p perf_output --test gpu_timestamps -- --ignored --nocapture
WGPU_BACKEND=vulkan cargo test -p perf_output --test gpu_timestamps -- --ignored --nocapture
```

## Browser stream

The page sends the run's bytes to the serving origin under the `perfStream` path `P`. The runner
hands it `P = /__perf/<token>`, a token it makes for each run, and answers only these routes and
the reset page's two (`reset`, `reset-done`) under `/__perf/`:

- `POST P/log` and, with `perfCsv`, `POST P/csv`: the next bytes of the file, with
  `X-Perf-Offset` set to the offset of the body's first byte. The server appends only at the
  file's length; bytes it already has get 204 and are not written twice; a gap or a differing
  overlap gets 409. One request is in flight per stream, carrying at most 1 MiB of what was
  written meanwhile; the rest goes in the next.
- `POST P/end`, once every byte is acknowledged, body
  `{"exitCode": n, "csvBytes": b, "logBytes": b, "failed": f}`: `csvBytes` and `logBytes` are the
  bytes the server acknowledged on each stream (0 for the CSV without `perfCsv`), and `failed` is
  true when either stream was given up, so the receiver can tell a truncated file from a whole
  one.
- An attempt the server leaves unanswered for 15 s is dropped and made again with the same
  offset and bytes, as is one that fails, gets 408, 429 or a 5xx; after 40 attempts the stream is
  given up. Any other 4xx gives the stream up at once (the same request would get the same
  answer), and `end` is not sent again after one.
- A panic: the engine's panic hook, on the panicking thread before it traps, hands the page what
  the engine still held (`__perfAppend`), then the panic's `PanicException` block, scrubbed and
  formatted as the desktop log's (`perf_output::layer::panic_block`, `__perfPanic`), and the page
  ends with 101.
- When the engine stops without handing over a block (a panic whose block did not arrive, a lost
  GPU device, one fatal error every frame, a hang), the page takes what the engine still held
  (`engine_perf_take`: the log it had not handed over and the CSV rows it had not flushed),
  appends a `PanicException` block of its own with a fixed message for what stopped the engine,
  holding none of the engine's or the page's text, and ends with 101, so a blank-screen run is
  never reported as a success. A stall the page shows on an error's word, about 4 s in, ends the
  run only once it has lasted as long as a hang (about 16 s), since it may still recover.
- `P` is resolved against the page's origin, and the page sends nothing when it lands anywhere
  else (`streamBase` in `deploy/web/engine/perf_stream.js`: the engine is told of no stream).
  Every request is made with `credentials: 'omit'`, `redirect: 'error'` and
  `mode: 'same-origin'`, so no cookie goes with it and a redirect fails the request like a
  dropped connection.
- The page is the only sender (`deploy/web/engine/perf_stream.js`): the engine queues its bytes
  and a system in `Update` hands them over (`__perfAppend`, `__perfEnd` on `AppExit`). A panic
  traps the engine, and the page can still finish the run; the page never calls into the engine
  to scrub after a trap.
- The bytes equal what the desktop writes to `--csv` and `--log-file`.
- One runner serves both builds of a leg, and their web apps come from different commits: a
  change to this protocol keeps the receiver accepting the baseline's sender.
- Serve with `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy:
  require-corp` and `Cross-Origin-Resource-Policy: cross-origin`, as set by the benchmark
  runner in `local-harness/benchmark/src/server.rs`, in a visible Chrome window: a hidden
  tab pauses the engine.

## The runner

`benchmark` (`local-harness/benchmark`, a binary of the engine workspace) is the receiver of the
browser stream, a leg's runner and the report, so the page's sender (`perf_stream.js`) and its
receiver change together. CI's *Test Suite* builds it on each platform it runs and uploads it as
`bevy-explorer-benchmark-<os>-<arch>-<sha>` (`windows-amd64`, `macos-arm64`, `linux-amd64`; kept
90 days on main, 7 otherwise). Windows and macOS run on every main push that runs the Test Suite,
and on a PR only for a [platform input](platforms.md) or `ci:macos`. A draft PR does not run the
Test Suite, so it uploads no runner until it is marked ready for review. Other PRs that run the Test
Suite, including one confined to `perf_stream.js` or `sso.ts` (which the runner's tests read),
build only the Linux runner; one confined to other web files builds none. The workflow takes the
missing runners from main. A draft that changes the sender or the runner is then measured with the
runner of main, which is not the receiver of its sender: read its benchmark only after the PR is
ready for review.

| Command | Does |
| --- | --- |
| `run` | One run of a web app (a folder holding `index.html`, a `.zip` or the URL of one): serves it from `http://localhost:18742`, opens Chrome, stores `<build>.csv` and `<build>.log` in `--out`, closes Chrome on `end` or at the timeout (480 s): it asks Chrome to exit and kills it after 20 s, or after 3 s when the run is interrupted, so a cancelled job leaves none. |
| `warmup` | `run` for a warm-up: no `perfCsv`, a 300 s backstop, exit 0 whatever the run's outcome (130 when interrupted, 64 and 70 as for `run`). |
| `fetch` | Downloads or unpacks a web app (a folder, a `.zip` or the URL of one) into `--dir`. Under Actions only a folder or a `.zip` on the machine: an address or a network path (`\\host\share`, `//host/share`) is refused. |
| `measure` | Change, base, change, base ... (`--runs` 3, `--settle` 15 s); each run's files become `change_build_<i>.csv/.log` and `base_build_<i>.csv/.log`, then `runs_on.txt` holds `--label`. Before its first run, `measure` removes `runs_on.txt` and every `change_build_<n>` and `base_build_<n>` `.csv`, `.csv.partial` and `.log` an earlier session left in `--out` (the warm-ups' `<build>.log` and the app folders stay), and before each run its own three files, so a run that crashes is never reported with an earlier session's files. |
| `leg` | `fetch` both builds, warm both up, `measure`: the whole leg by hand. Its label is `local-chrome` unless `--label` gives one. |
| `report [DIR]` | The report ([The report](#the-report)) of every folder under `DIR` (the working directory by default) that holds a `runs_on.txt`, written to `--out` (`summary.md` by default). The header takes `--title` (`Benchmark` by default), `--pr`, `--change` and `--base` (how it names the two builds) and `--run-url`; `--expected-runs` (3 by default: fewer CSVs mean crashed runs) and `--expected-labels` (comma-separated: one without results gets a line) shape the notes. |

Every option but four has an environment variable (`benchmark <command> --help` lists them), so a
workflow step needs no shell-specific syntax: `--keep-service-worker` (by hand only), `leg`'s
`--change` and `--base` and `fetch`'s `--source` have none. `--chrome-arg` takes its flags from
`CHROME_EXTRA_ARGS`, space-separated, which `--help` does not list. `report` reads
`GITHUB_STEP_SUMMARY`, `RUN_NAME`, `PR_NUMBER`, `CHANGE_DESCRIPTION`, `BASE_DESCRIPTION`,
`RUN_URL`, `EXPECTED_RUNS` and `EXPECTED_LABELS`; its `DIR` is an argument.

`run` exits 0 when the page ended the run with exit code 0, 1 when it ended with another code or
an incomplete stream, 2 at the timeout, 3 when Chrome exited first, 130 when interrupted; 64 is a
usage error, 70 a machine that cannot run the leg (no Chrome, port taken, download failed).

By hand, one command per OS, from `rendering/bevy-explorer/source` (or with the CI artifact's
binary), then the report of the output folder:

```powershell
# Windows (PowerShell)
cargo build -p benchmark --release
.\target\release\benchmark.exe leg --change .\builds\change-web --base .\builds\base-web --label intel-i5-chrome --out .\perf\browser
.\target\release\benchmark.exe report .\perf\browser --out .\perf\browser\summary.md
```

```bash
# macOS
cargo build -p benchmark --release
./target/release/benchmark leg --change ~/builds/change-web --base ~/builds/base-web --label apple-m1-chrome --out ~/perf/browser
./target/release/benchmark report ~/perf/browser --out ~/perf/browser/summary.md
```

`--base` is optional: without it the section shows the change build alone, with the "No Baseline
run produced data" note.

**The server** listens on `127.0.0.1` and `::1` only. The port is fixed (`--port`, `PERF_PORT`):
the Chrome profile keeps the local guest per origin. Every response carries the isolation headers
above, `Cache-Control: no-store`, `Content-Security-Policy: frame-ancestors 'self'` and
`X-Frame-Options: SAMEORIGIN` (the shell page frames the app page from its own origin).

- The stream is under `/__perf/<token>`, a random 128-bit token made for each run and part of the
  address Chrome is opened on (the job log shows `<token>`). Any path under `/__perf/` other than
  the run's `csv`, `log`, `end`, `reset` and `reset-done` is 404. `Host` must be `localhost`,
  `127.0.0.1` or `[::1]` with the bound port (421): a page whose name was re-pointed at loopback
  sends its own. `end` takes only `application/json` (415) and no preflight is answered, so a
  cross-site page cannot end a run.
- A `csv` or `log` request carries a `Content-Length` of at most 16 MiB (411, 413); `end` and the
  reset routes at most 4 KiB; a file holds at most 256 MiB (413). Bodies go to disk as they
  arrive. Every reply to a `csv` or `log` request that reaches the stream (204, 400 for a body that
  did not arrive whole, 408, 409, 413 for a file at its cap, 500) carries `X-Perf-Length`, the
  stored length; the 400 (offset), the 411 and the 413 (a request over 16 MiB), answered before
  the stream is reached, do not.
- A path the web app does not have is a 404; the first five are logged by path, the rest only
  counted, and the total is logged when the run ends.
- When `end` says a stream was given up, or its totals differ from the stored lengths, the page
  ended the run but its files are incomplete: `measure` sets the CSV aside as
  `<build>_<i>.csv.partial` (the report counts the run as crashed) and keeps the log. So is any
  CSV the report cannot read, which would lose the laptop's whole section: one with a header and
  no rows, a row with fewer than three fields or a CPU or GPU field that is not a number (a row
  cut by a timeout or a closed Chrome) and a blank row. Any other CSV stays as the run's, whatever
  the exit code.
- A download is limited to 1 GiB, an archive to 20,000 entries and 2 GiB unpacked; an entry whose
  name could leave the folder, a link and a device name fail the fetch. `--build` is letters,
  digits, `_`, `-` and `.`, and not only dots: its folder is emptied before a build is unpacked.

**Chrome and its profile.** Chrome is found in its usual place on Windows and macOS, or at
`CHROME_PATH`. It is opened visible and maximized (never headless or minimized: a hidden tab
pauses the engine), with the flags in `src/chrome.rs`: no first-run, sign-in or crash-restore
prompts, no throttling of a covered window, and `--force-high-performance-gpu`, because Chrome's
WebGPU picks the low-power GPU of a laptop that has two while the desktop client renders on the
other.

- One profile per laptop (`--profile`, `PERF_CHROME_PROFILE`; by default
  `decentraland-benchmark/chrome-profile` in the user's local application data folder), because
  it holds the local guest's root key: the address never changes between runs and builds.
  Deleting the folder makes the next warm-up create a new guest. Keep the path short: on Windows
  Chrome's Cache Storage fails past 260 characters. Do not open the profile in another Chrome: the
  next run closes it.
- **Every web run starts with a cold content cache.** Before each run a Chrome still running on
  the profile (found by `--user-data-dir`; no other Chrome is touched) is closed, the folders where
  a page leaves code or data are deleted from disk (`Service Worker` with its Cache Storage,
  `Cache`, `Code Cache`, `IndexedDB`, `File System`, `WebStorage` (storage buckets),
  `Shared Dictionary`, `blob_storage`, `Session Storage`, the cookie files), and the reset page
  (`/__perf/<token>/reset?to=<app>`) clears local storage but for the root key, session storage
  and cookies, then continues to a path on its own origin. Nothing the web app keeps in those
  places, a service worker least of all, reaches the next build served on the same origin, at the
  price of a cold start for every run, the same for both. `Shared Storage` is not on the list: the
  checked profile has no such folder.
  The root key, Chrome's settings and the GPU shader caches stay. `--keep-service-worker` (by hand
  only) skips the scrub and the reset page.

**Tests.** `cargo test -p benchmark` runs the stream protocol and the security rules against the
server in the test's process, a run played by a stand-in page, closing a Chrome, the report on
fixtures (`tests/fixtures/report`: a whole report, a crashed run, a `.csv.partial`, a header-only
CSV, an all-zero GPU column, an outlier run, teardown exceptions, a missing label and a section
without base runs; the log markers are read from `crates/common` and `crates/perf_output`, so they
cannot drift), and, under node (skipped with the reason printed where it is missing), the page's
own sender and the reset page. `local-harness/benchmark/fake-app` is a stand-in page that speaks
the stream, to check a leg
without a Bevy build: `benchmark run --app <path to it>`; `--query fake=<mode>` selects a failing
page (the modes are listed at the top of its script), and `build-info.json` beside the page sets
the fake CPU time and error count, so two copies of the folder with different values make a fake
A/B for `leg`. The workflow's resolver has its own tests, with the GitHub API faked
([`tooling/tests/test_benchmark_inputs.py`](../../../../tooling/tests/test_benchmark_inputs.py)).

## The workflow

[`bevy-explorer-benchmark.yml`](../../../../.github/workflows/bevy-explorer-benchmark.yml)
(*Bevy Explorer / Benchmark*) and the leg it calls once per laptop,
[`bevy-explorer-benchmark-leg.yml`](../../../../.github/workflows/bevy-explorer-benchmark-leg.yml).
They use `GITHUB_TOKEN` only, with no permission at the top and each job's own:

| Job | Runs on | Permissions | Does |
| --- | --- | --- | --- |
| *Inputs* (`setup`) | GitHub-hosted | read contents, actions, pull requests | Runs [`tooling/benchmark-inputs.py`](../../../../tooling/benchmark-inputs.py) on the event: decides whether the benchmark runs and finds the builds, the runners and the legs. |
| *Notice* | GitHub-hosted | write pull requests | When nothing runs and setup left notes, writes them into the performance section (*Not run*). |
| *Running* | GitHub-hosted | write pull requests | When the benchmark runs, sets the performance section to *Running* with the run, the builds and the legs. |
| *Queue watchdog* | GitHub-hosted | write actions, pull requests | Cancels the run when a leg never starts, saying so in the performance section ([The fleet](#the-fleet)). |
| `<label>` / *Chrome* (`leg`) | the laptop | read actions | [What a leg runs](#what-a-leg-runs); a failed leg does not stop the others. |
| *Report* (`gather`) | GitHub-hosted | read actions | Unless the run was cancelled: downloads every `benchmark-result-*` and the Linux runner, writes the report and uploads it as `benchmark-report`. |
| *Post* | GitHub-hosted | write pull requests | Unless the run was cancelled, after a report: downloads `benchmark-report`, puts a badge read from its *Overall* line above it and writes it into the performance section. |

The jobs that write to the pull request use the repository's
[`bevy-explorer-ci-status`](../../../../.github/actions/bevy-explorer-ci-status/action.yml) action, which keeps
the one CI status comment per pull request and replaces only the *performance* section
([CI status comment](../../../../apps/bevy-explorer/README.md#bevy-explorer-ci-status)); they check out
only the action, at the workflow's own commit.

**The artifacts.** In each name `<sha>` is the commit the CI run built: for a pull request's run,
its merge commit.

| Artifact | Holds | Uploaded by |
| --- | --- | --- |
| `bevy-explorer-web-<sha>` | the web app, `apps/bevy-explorer/dist/web/`, with its `build-info.json` | CI's web job; 90 days on main, 7 otherwise |
| `bevy-explorer-benchmark-<os>-<arch>-<sha>` | the `benchmark` binary | CI's *Test Suite* ([The runner](#the-runner)) |
| `benchmark-result-<label>` | a leg's `runs_on.txt` and run files | the leg; 7 days |
| `benchmark-report` | the report, `report.md` | the report job; 7 days |

**What setup finds**, after the checks in [How a run starts](#how-a-run-starts):

- **The change:** the newest passed CI run of the change commit (a pull request's run, or a manual
  CI run of its branch) and its web app: the run's one artifact named `bevy-explorer-web-` and a
  40-character commit.
- **The base:** the newest passed `push` run on main that still has a web app, among the latest
  100; with `base_sha`, a passed main run of that commit.
- **The runners,** for each operating system the legs need and for Linux, which writes the report:
  from the change run when it has one, so a pull request that changes the runner is measured with
  its own, else from main's runs, searched as for the base. A laptop whose operating system has no
  runner is left out, with a note in the report; without a Linux runner nothing runs.
- **The legs:** the fleet's laptops that `BENCHMARK_ENABLED_RUNNERS` enables, each with its runner
  and both web apps; their labels, which the report expects; and the runs per build.

**The report job** runs `benchmark report results` with `PR_NUMBER`, `CHANGE_DESCRIPTION`
(`#<pr> at <commit>`), `BASE_DESCRIPTION` (`main at <commit>`), `RUN_URL`, `EXPECTED_RUNS` and
`EXPECTED_LABELS`, appends setup's notes, copies the result to the job summary and uploads it, with
the report's exit code as a job output. The post job then puts a badge above it, read from the
**Overall** line (*Regression*, *Improved*, *No change*, *Not compared*; *No data* on exit 2), and
writes it into the performance section. The action truncates a section over 12000 characters with
a note, so the full report stays in the report job's summary and the `benchmark-report` artifact;
a write it cannot confirm after five attempts warns and does not fail the job. A report without
data (exit 2) is written, then fails the post job. A cancelled run, by the watchdog or by a newer
run of the pull request, writes no report, so the section keeps what the watchdog or the newer
run wrote.

**Trust.** This repository is public. Benchmark legs execute the selected revision's app and may execute its runner. Before enabling self-hosted runners, verify the current workflow event filters, ref selection, token permissions and runner-group access. Keep untrusted PR code off persistent machines that hold credentials or can reach private resources. Fork filtering alone does not establish trust in a same-repository branch. Use an explicitly reviewed revision and an isolated runner environment. Jobs that consume untrusted artifacts must not gain write permissions; a comment-only job must treat the report as data. Repository settings and runner authorization require an administrator's verification, not an assumption from this guide.

## The fleet

| Runner label | OS | Report section |
| --- | --- | --- |
| `intel-i5` | Windows | Intel Core i5 (Chrome) |
| `apple-m1` | macOS | Apple M1 (Chrome) |
| `amd-ryzen-3` | Windows | AMD Ryzen 3 (Chrome) |
| `intel-i3` | Windows | Intel Core i3 (Chrome) |
| `framework-13-i7` | Windows | Framework 13 i7 (Chrome) |

A laptop's leg is labelled `<runner label>-chrome`. A new laptop needs its label and operating
system in `FLEET` (`tooling/benchmark-inputs.py`) and its name in `MACHINES`
(`local-harness/benchmark/src/report/render.rs`).

**Repository variables** (Settings → Secrets and variables → Actions → Variables):

- `BENCHMARK_ENABLED_RUNNERS`: the labels of the laptops that are online, for example
  `intel-i5,apple-m1`. Unset or empty means every laptop, so it can never silently turn the
  benchmark off. The match is a substring check, so any separator works. Clear it when the whole
  fleet is online. `none`, in any letter case, turns the benchmark off, and pull requests that
  compile the engine then skip wasm-opt; any other value that names no laptop turns it off and
  keeps them optimized.
- `BENCHMARK_RUNS`: measured runs per build, 1 to 3; 3 when unset. A manual run's `runs` overrides
  it.
- `BENCHMARK_QUEUE_TIMEOUT_MINUTES`: the watchdog's limit, 60 when unset.

**The watchdog.** A leg sent to an offline, stuck or busy laptop waits in the queue, and a job's
timeout counts only once it starts. The *Queue watchdog* job reads the run's jobs every minute;
when one is still queued after `BENCHMARK_QUEUE_TIMEOUT_MINUTES`, it sets the performance section
to *Cancelled* with a message naming it and cancels the run. The message says the laptop is
offline, stuck or busy with another job, and suggests *Re-run
failed jobs* once it is free, which keeps the legs that finished and their results, raising
`BENCHMARK_QUEUE_TIMEOUT_MINUTES`, or leaving the laptop out of `BENCHMARK_ENABLED_RUNNERS`. It
ends when every job has started. The limit is generous because a laptop busy with another job, of
either repository that uses the fleet, also keeps a leg queued. A leg that starts and hangs is
stopped by its own timeouts ([What a leg runs](#what-a-leg-runs)).

**Admin steps**, once:

1. Register one runner per laptop, under its runner label, in an organization runner group whose
   repository access is limited to the repositories that use the fleet: this one and the one that
   holds unity-explorer's benchmark ([Setting up a Windows laptop](#setting-up-a-windows-laptop),
   [Setting up a macOS laptop](#setting-up-a-macos-laptop)). One runner takes one job at a time,
   so jobs from both repositories queue behind each other and never measure at once.
2. Verify the public repository's Actions approval policy and restrict self-hosted runner access to reviewed workflows and revisions. Do not enable untrusted fork jobs on the persistent fleet. Record the administrator's verification privately before enabling the benchmark.
3. Set `BENCHMARK_ENABLED_RUNNERS`, and `BENCHMARK_RUNS` or `BENCHMARK_QUEUE_TIMEOUT_MINUTES` when
   the defaults do not fit.

**Laptop prerequisites:** Google Chrome; a logged-in desktop session, unlocked, with the screen
on, since Chrome's window must stay visible; the runner's job-started hook that empties the
workspace before each job (the leg also empties its own folders and run files); and port 18742
free on loopback (`PERF_PORT` in the runner's environment moves it, on a Windows that reserves the
port). The Chrome profile is the runner account's ([The runner](#the-runner)).

### Setting up a Windows laptop

1. **Install Windows cleanly**, with local accounts only, so no bundled or cloud-account software
   runs during a measurement. Put the laptop on a guest network if you can, so a job cannot reach
   other computers. Give the administrator account a strong random password: a job could try to
   crack it.
2. **Tune it for stable measurements:** never sleep, never turn the screen off, keep the lid open;
   mute the microphone and speakers and set the brightness to its minimum. Make the firewall
   block apps without asking, so no prompt opens over Chrome's window: the runner and Chrome need
   only outgoing connections and loopback.
3. **Create the runner account:** a separate account without administrator rights, with a strong
   random password, that logs in automatically
   ([Microsoft's instructions](https://learn.microsoft.com/en-us/troubleshoot/windows-server/user-profiles-and-logon/turn-on-automatic-logon)).
   Logged in as it, allow PowerShell scripts (Settings, in the developer settings).
4. **Install Google Chrome.**
5. **Install the GitHub runner** as the runner account, following GitHub's instructions for an
   organization runner, and configure it in the fleet's runner group with
   `config.cmd --runnergroup <group> --no-default-labels --labels <runner label>`, so it takes
   only the jobs meant for it. One runner per laptop.
6. **Empty the workspace before each job.** As the administrator, make a folder the runner account
   can read and execute but not write, for example `<runner-root>/scripts`, holding
   `job_started.ps1`:

   The hook must resolve the assigned job workspace to an absolute path, verify it
   is beneath the dedicated runner work directory and contains no shared state,
   then remove only that job workspace's contents. Fail if validation fails;
   never rely on the hook's current directory for a recursive cleanup.

7. **Start the runner at login, with the hook:** a start script, for example on the desktop,

   ```bat
   cd /d <runner-root>
   set ACTIONS_RUNNER_HOOK_JOB_STARTED=%CD%\scripts\job_started.ps1
   run.cmd
   ```

   and a shortcut to it in the Start menu's Startup folder.

### Setting up a macOS laptop

1. **Configure macOS:** a guest network if you can; a strong random password on the administrator
   account; never sleep, never turn the display off, keep the lid open; microphone and speakers
   muted, brightness at its minimum.
2. **Create the runner account:** a separate standard account with a strong random password,
   which logs in automatically (verify it under [Validation and acceptance](#validation-and-acceptance)).
3. **Install Google Chrome** in `/Applications`.
4. **Install the GitHub runner** as the runner account, following GitHub's instructions for an
   organization runner, and configure it in the fleet's runner group with
   `./config.sh --runnergroup <group> --no-default-labels --labels apple-m1`. One runner per
   laptop.
5. **Empty the workspace before each job:** a job-started hook as on Windows, a shell script the
   runner account can read and execute but not write, named in `ACTIONS_RUNNER_HOOK_JOB_STARTED`.
6. **Start the runner at login, with the hook** (verify it under [Validation and acceptance](#validation-and-acceptance)).

## Validation and acceptance

Record current run evidence and open work in the task report. Before claiming a platform
or fleet setup works, verify it at the selected revision:

- Exercise fake-app success, shell, storage, panic and give-up paths; compare
  stored output checksums and verify profile cleanup without losing intended
  guest identity.
- Run a complete baseline/change leg and generate its report. Account for every
  measured run, warm-up, timeout, cancellation and exception.
- Verify workflow triggers, artifact selection, runner labels and permissions,
  shell setup, watchdog behavior, report publication and cancellation on the
  actual configured runners.
- Test runner login and workspace-cleanup hooks on each supported OS.
- For GPU time, verify a known workload, unavailable-query handling and heavier
  workloads on each graphics backend. Keep these results separate from CPU and
  frame-time measurements; a pass on one backend does not establish another.

List unavailable fixtures or omitted private suites as not run. Historical
results and hardware-specific observations are not current acceptance evidence.
