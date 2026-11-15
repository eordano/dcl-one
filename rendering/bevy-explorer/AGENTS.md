# bevy-explorer

- Read `docs/guidelines/README.md` first: the security model, how to contribute,
  scheduling, code style, platforms, the bevy fork, and the things that look like bugs but
  aren't.
- For changes to service routing, scene network capabilities or request instrumentation,
  apply the [security rules](docs/guidelines/security.md) and the
  [test requirements](docs/guidelines/contributing.md#tests).
- When adding or changing a runtime feature, expose the state and controls a tester needs
  in its owning plugin's [debug panel section](docs/guidelines/recipes.md#a-plugin-section-in-the-runtime-debug-panel).
  Verify the readings and actions through the running HUD.
- `source/react-web/`: the web explorer's HUD (React). Read `source/react-web/AGENTS.md`
  before changing it.
- `source/` is the engine's own Cargo workspace; build and run from there. Run
  `python3 ../../../tooling/patched-sources.py materialise` there before any `cargo` command:
  it writes the Bevy and forked-crate trees Cargo reads
  ([commands](docs/guidelines/bevy-fork.md#changing-bevy)). `README.md` here covers how the rest
  of dcl-one consumes the build.
- Validate a built Explorer through the [E2E driver](../../apps/bevy-explorer/e2e/README.md)
  without requiring user input. For desktop builds, use the authenticated loopback console
  to drive the client, query `/world_state`, and capture a rendered frame with `/screenshot`.
