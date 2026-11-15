# Recipes

How to add the things that get added most often. Each one names the template to copy; copy it
whole rather than assembling from pieces.

## A new SDK component

Register it in `update_world` with `add_crdt_lww_component::<PbFoo, Foo>` (or
`add_crdt_go_component` for grow-only sets). The registration adds the `UpdateWorld` system
that converts each delta into your component via `TryFrom<PbFoo>`; you don't add a system to
the scene loop by hand. Systems that react to the component go in `SceneSets::PostLoop`. The
proto type comes from `dcl_component`; never redeclare it.

## A new engine-to-system-scene stream

State the engine owns reaches the HUD this way, sent on subscribe and on change; the HUD shows
what the stream last said rather than polling a request.

Template: the proximity stream, `GetProximityStream` in
`crates/scene_runner/src/update_scene/pointer_results.rs` and
`react-web/bridge-scene/src/domains/proximity.ts`, added whole in #764. Not the chat stream,
which drags in player, wallet and marker filtering you don't want. Smallest plumbing-only
reference: `getBlockUpdateStream` (#844). The file set:

1. `crates/system_api_types/src/lib.rs`: the payload struct, `#[serde(rename_all =
   "camelCase")]`, `#[derive(ts_rs::TS)] #[ts(export)]`, `#[ts(type = "number")]` on 64-bit
   fields. It must be a **new, distinct struct**: the receiver is stored in scene state keyed
   by Rust type, so two streams sharing a payload type fight over one slot.
2. `crates/system_bridge/src/lib.rs`: the `SystemApi::GetFooStream(RpcStreamSender<Foo>)`
   variant.
3. The producing system: `EventReader<SystemApi>` collects senders, retains the open ones,
   early-outs when empty, diffs against previous state, and sends fire-and-forget.
   `pipe_connectivity_events_to_scene` in `crates/social/src/lib.rs` is the cleanest example.
4. `crates/dcl_js/src/system_api.rs`: `op_get_foo_stream` and `op_read_foo_stream`.
5. `crates/dcl_deno/src/js/op_wrappers/system_api.rs`: the two native op wrappers, added to
   the super-user ops list.
6. `crates/dcl_wasm/src/inner/op_wrappers/system_api.rs`: the two web wrappers. No list to
   edit; the sandbox worker scans every `op_*` export. Only a wasm build compiles these.
7. `crates/dcl_js/src/modules/SystemApi.js`: `getFooStream` as an async generator over the
   two ops.
8. `react-web/bridge-scene/src/bevy-api.ts`: the method on `BevyApiInterface`, importing the
   generated type; a relay domain under `bridge-scene/src/domains/`; a message kind in
   `library/explorer-core/engine/protocol.ts`; a case in `useEngineSession.ts`; the mock bridge.

Run `just ts-bindings` before touching the TypeScript: it regenerates the types under
`library/explorer-core/engine/generated/`, which both HUD hosts compile. Commit them with the
Rust struct change. CI regenerates them before every TS build, so a Rust struct change that
breaks the page fails `tsc` there, and the Sites import fails while the committed copy differs.

## Pausing a scene

To hold a scene for a while, insert a named reason into its `RendererSceneContext.blocked` and
remove it when the cause ends: the scene loop skips a blocked scene (a super-user scene runs
regardless), which keeps its entities and
JS state and resumes where it stopped. References: imposter baking (`"imposter_baking"`), the
inspector's freeze (`FROZEN_BLOCK`), `portableExperiences: disabled`
(`PORTABLE_EXPERIENCES_BLOCK` in `crates/scene_runner/src/portables.rs`). Compare before writing
so the context is not marked changed every frame, and hide its UI through the `ui_scenes` filter
in `scene_ui` if it should not show. `LiveSceneInfo.is_blocked` reports it.

## The ts-rs boundary

`crates/system_api_types` holds the plain serde structs that cross to the HUD and the system
scene; `scripts/gen-ts-bindings.sh` is the single source of truth for generating their TS.
Rules that follow from the transport:

- `skip_serializing_if` is unsafe on these types. Natively they cross the scene-process IPC
  as positional msgpack, so a skipped field shortens the array and decode fails.
- 64-bit integers get `#[ts(type = "number")]`; `Option` becomes `| null`.
- `serde_repr` enums get no ts derive (the wire is numeric, ts-rs would emit a string union);
  use a field-level `ts(type = "number")` instead.

## A new engine parameter

The shared clap `LaunchOptions` in `crates/system_api_types/src/launch_options.rs` is the one
definition: native flags are the kebab-case web names, with no aliases. The web-param table in
`web_params.rs` is derived from the clap metadata and exported to the page with the ts
bindings, so a new option appears in both places from one edit. On web the page reads it with
`webParam(name)` and the engine receives it through `engine_run(options)`, which rejects
unknown fields. Whether a param is trusted is the front end's concern (`lib/launchGate.ts`),
not the engine's. A **web-only** switch is not a `LaunchOptions` field at all: it is a page
param handed to a wasm export, like `?guest=`.

## A new console command

`app.add_console_command::<FooCommand, _>(foo_system)` with a `bevy_console::Command` struct.
`add_preview_console_command` registers it only in preview mode; outside preview it is absent
from `/help` and rejected. Console commands land in `ConsoleSet::Commands`, just before
`SceneSets::Init`, so their effects are visible to the scene loop in the same frame.

## Per-avatar data on a shared material

Wearables and masks come from `crates/avatar/src/material_cache.rs`, shared across avatars so
they batch. A per-avatar value is never a material property; it goes in the entity's `MeshTag`
(a `u32`, read in WGSL with `mesh_functions::get_tag(instance_index)`). `scene_material` owns
the layout: the top byte is flags (`SCENE_MATERIAL_*_MESH_TAG`); the rest is free for values.

- **Budget the bits to the effect.** Work out the step the bits give against what the eye can
  see in one frame, and write the arithmetic in the PR. A value that moves centimetres per
  frame does not need millimetre steps.
- **No flag bit beside a value field.** For a range, 0 means none and `1..=255` spans the
  range. Encode both ends of the range, so a "hidden" value sits past the mesh rather than at
  a boundary where a strict comparison in the shader leaves a sliver.
- **Modes live in the unique material, not the tag.** A material that is already one per
  entity carries its own switches; the shared tag carries only the value.
- Rust owns encode and decode, in `scene_material`, with a round-trip test that mirrors the
  WGSL decode: monotonic across the range and within half a step.
- After writing a tag, call `set_changed` on the `Mesh3d`: bevy 0.16 ignores
  `Changed<MeshTag>` ([oddities](oddities.md#rendering)).

## A constant shared between Rust and WGSL

A WGSL literal with a comment "keep in sync with" a Rust constant is rejected in review. Push
the constant as a shader def from the material's `specialize` and read it as `#{NAME}`:

```rust
// scene_material: the one definition, for every pipeline whose shader imports the function
pub fn foo_shader_defs() -> [ShaderDefVal; 2] {
    [
        ShaderDefVal::UInt("FOO_MESH_TAG_MASK".to_owned(), SCENE_MATERIAL_FOO_MESH_TAG_MASK),
        ShaderDefVal::Int("FOO_MILLIMETRES".to_owned(), SCENE_MATERIAL_FOO_MILLIMETRES),
    ]
}
// in each Material's specialize whose shader calls the function:
fragment.shader_defs.extend(foo_shader_defs());
```

```wgsl
// defs are integers: metres travel as millimetres
#ifdef FOO_MESH_TAG_MASK
fn foo(mesh_tag: u32) -> f32 {
    return f32(mesh_tag & #{FOO_MESH_TAG_MASK}) * f32(#{FOO_MILLIMETRES}) * 0.001;
}
#endif
```

The `#ifdef` matters: naga_oil preprocesses the whole imported file, so a shader that imports
an unrelated function from the same file, from a material that does not push the defs, fails
without it. Every `Material` whose shader calls the function needs a `specialize` that pushes
the defs; a material extension's defs do not reach a separate material.

## A pipeline-compile test for a shader

Every shader change gets a test that compiles every pipeline variant off-screen, `#[ignore]`d
because CI has no GPU. The `pipeline_test` crate holds the setup these tests share:

1. `headless_app()`: a `DefaultPlugins` app with `WinitPlugin`, `LogPlugin` and
   `PipelinedRenderingPlugin` disabled, no primary window and the embedded assets.
2. A camera rendering to `render_target(&mut app)`, with `DepthPrepass` and `NormalPrepass`,
   and a shadowing `DirectionalLight`, so the prepass and shadow variants compile too.
3. One entity per material and tag variant the change touches.
4. `compile_pipelines(&mut app)` updates until the `RenderApp`'s `PipelineCache` has nothing
   pending and panics on a pipeline in an error state. `fragment_pipelines(&app, path)` counts
   the compiled pipelines whose fragment stage is one shader.

A crate with such a test takes `pipeline_test` and `naga_oil` as dev-dependencies, the latter
only to enable its `override_any` feature for the crate. Run with
`cargo test -p <crate> <test> -- --ignored --nocapture`. The native pass compiles the same
variants as web, but the web-only rules on the [platforms](platforms.md#web) page still need
the web build once.

## A plugin section in the runtime debug panel

In the plugin that registers the commands, call `app.add_debug_section(DebugSection { ... })`
from `console::{AddDebugSection, DebugSection}`. Give it a unique title, a read-only JSON
query and labeled fields using dot-separated keys or array indices. Actions pair a button
label with a console command line. Use commands available in the same builds as the section.
The HUD discovers sections through `/debug_panel` and polls only expanded sections.
Return numeric groups as arrays. For a tuple with named components, end the field label
with their ordered names, for example `Scene definitions per batch (min / mean / max)`
or `Position (X / Y / Z)`. The HUD puts each caption beside its value.
If a measurement needs a peer, scene or sample, an object reply can include an
`unavailable` map from field paths to reasons. The HUD uses the closest matching path
prefix (or the empty key as a default) only when the value is missing or null.
Fields waiting for the same prerequisite share one explanation.
When developing a runtime feature, include the diagnostics that answer a concrete testing
question. Show the effective state, units, scope and reason for an unavailable reading.
For an adjustable feature, bind its existing validated action or setting. Register controls
only where the feature and its permission level are available. Keep keys, tokens, private
messages and signed URLs out of the panel.

Keep section labels, console bindings and JSON formatting in a diagnostics module beside
the feature. The feature owns its state, transitions and permission checks. Read existing
components on demand. For rates and histories, retain the smallest counters or bounded
window needed to answer the first query after activity. When the panel reopens, show
activity that occurred while polling was stopped. Build charts and format snapshots
only when queried, after releasing any producer lock. Verify the first read with the
panel closed throughout the activity being measured.

Use `DebugInput` for a toggle, choice, number or text argument. A field path binds the
control to live state. A command template substitutes `{value}` into its arguments.
For opposite actions such as Connect and Disconnect, register one `ActionButton`.
Bind it to the boolean the command changes and give each direction its caption and
argument. When neither action is available, return null and the reason.
Validate user input in the owning handler. For HUD-owned features, use the existing
session handler and apply the same lifecycle rules. Verify a real value change or
side effect through an open panel in a built client. See `WorldStatePlugin` for a
readout with navigation controls and `AvatarPlugin` for an action.
