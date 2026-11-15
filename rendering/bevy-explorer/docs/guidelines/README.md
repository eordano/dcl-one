# bevy-explorer guidelines

Orientation and working rules for people and agents contributing to bevy-explorer. File paths
in these pages are relative to `source/`, the engine workspace.

- [Architecture](architecture.md): the two dataflow diagrams, native and web, and how to read them.
- [Security](security.md): the trust model and the rules that follow from it. Read first.
- [Contributing](contributing.md): scope, diffs, tests, PR descriptions, dependencies (never `cargo update`), review.
- [Scheduling](scheduling.md): the frame, the scene loop, PostUpdate, and where a new system goes.
- [Code style](code-style.md): lean into the ECS; state, cleanup, async, errors, logging.
- [Platforms](platforms.md): what differs between native, web and headless, and what bites.
- [The bevy fork](bevy-fork.md): how our bevy differs from stock 0.16, by area, and how to
  change it.
- [Decisions](decisions.md): deliberate client differences and choices that look like mistakes.
- [Recipes](recipes.md): adding an SDK component, a system-scene stream, an engine param, a console command, per-avatar data on a shared material, a constant shared with WGSL, a pipeline-compile test.
- [Porting from Unity](porting-from-unity.md): where a feature lives in the Unity explorer, the procedure, the Unity-to-engine and HLSL-to-WGSL mappings.
- [Oddities](oddities.md): things that look wrong and aren't; read before "fixing" something surprising.
- [Interaction](interaction.md): the pointer's distances, the scene raycast tiers and budget, and the `/hover_entity` and `/click_entity` commands with their replies and diagnoses.
- [Running and testing](running.md): entry points, debugging, testing gotchas, the test suites.
- [Benchmark](benchmark.md): the bare-metal benchmark of pull requests in Chrome on the laptop fleet: the files `--autopilot` writes, the browser stream, the `benchmark` runner and its report, the workflow and the laptops; read before changing the run, its CSV, log format, browser stream, report or workflows.
- [Glossary](glossary.md): the project's vocabulary.
