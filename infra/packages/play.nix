{ lib, runCommand, python3 }:
let
  root = ../..;
  manifestPath = "rendering/bevy-explorer/source/react-web/shared-inputs.json";
  manifest = builtins.fromJSON (builtins.readFile (root + "/${manifestPath}"));
  # Validate the original flake source before filtering away unconsumed shared names.
  checked = assert import ./hud-shared-inputs.nix { inherit root manifest; }; manifest;
  sharedFiles = map (row: root + "/${row.path}") (checked.controls ++ checked.files);
  supplied = builtins.getEnv "PLAY_WEB_DIR";
  artifact = if supplied == "" then null else builtins.path {
    path = /. + supplied;
    name = "foundation-built-explorer";
  };
  # The flake's Git source contains tracked inputs, never Cargo targets or local
  # build output. Verify the first-party source directly without a snapshot pin.
  contract = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions (sharedFiles ++ (map (p: lib.fileset.maybeMissing (root + p)) [
      /rendering/bevy-explorer/source
      /rendering/bevy-explorer/patches /rendering/bevy-explorer/vendor
      /library/protocol/proto
      /library/hashing
      /apps/bevy-explorer/build-web.py /apps/bevy-explorer/install-linux-deps.py
      /apps/bevy-explorer/install-web-tools.py
      /apps/bevy-explorer/install-tools.sh /.github/actions/bevy-explorer-tools/action.yml
      /flake.lock /infra/packages/default.nix /infra/packages/tools.nix
      /tooling/play-artifact.py /tooling/hud_shared_inputs.py /tooling/patched-sources.py /tooling/check-local-hashing.mjs
      /.github/workflows/bevy-explorer-ci.yml
      /.cargo /rust-toolchain /rust-toolchain.toml
      /library/explorer-core/engine/generated
    ]));
  };
in runCommand "foundation-built-play-web" {
  src = artifact;
  nativeBuildInputs = [ python3 ];
  # Static files only. A WASM compiled inside a Nix shell embeds its toolchain's store
  # paths; imported from the store, those become references and Sites would ship the
  # 4 GiB Rust closure. Refuse that here instead of in a production image.
  allowedReferences = [ ];
} ''
  if [ -z "''${src:-}" ]; then
    echo 'Sites requires our Bevy artifact. Run tooling/fetch-play.py, then build with PLAY_WEB_DIR and --impure; see infra/containers/README.md.' >&2
    exit 1
  fi
  # A PR build (PLAY_ALLOW_UNOPTIMIZED=1) may carry main's engine when the PR changes no
  # engine input. --trust-inputs accepts only a build fetch-play.py recorded as validated
  # against its producer commit for a tree with exactly these inputs (producer-validated.json);
  # a build of the tree's own inputs, or any other, is still checked as before, and
  # integrity, HUD review and bindings are checked in every mode.
  python ${contract}/tooling/play-artifact.py verify "$src" --root ${contract} \
    ${if builtins.getEnv "PLAY_ALLOW_UNOPTIMIZED" == "1" then "--trust-inputs" else "--production"}
  mkdir -p "$out"
  for part in engine bridge-scene service_worker.js preview_realm.js sites-play.json; do
    cp -r "$src/$part" "$out/$part"
  done
''
