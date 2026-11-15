#!/usr/bin/env sh
# Generate the TypeScript bindings for the ~system/BevyExplorerApi boundary from the Rust
# structs in crates/system_api_types, into library/explorer-core/engine/generated (+ a barrel index.ts),
# the one tracked copy both HUD hosts (react-web and Sites /play) compile.
# Also writes the web param TABLE (crates/system_api_types/src/web_params.rs → webParamTable.ts)
# and the language table (languages.rs → languageTable.ts): the `export_` filter catches both
# ts-rs's export_bindings_* tests and those table exports.
# Called by `just ts-bindings`, apps/bevy-explorer/build-web.py and bevy-explorer-desktop.yml
# before the react-web / bridge-scene builds; commit the output when the Rust structs change.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
# git-bash (windows CI): cargo is a native process, so hand it a windows-style path —
# the POSIX /d/... form would be misresolved.
case "$(uname -s)" in
MINGW* | MSYS*) root="$(cd "$root" && pwd -W)" ;;
esac
# Cargo reads Bevy and the forked crates from trees that are not tracked: write the ones that are
# missing or stale.
sh "$(dirname "$0")/patched-sources.sh"
out="$root/../../../library/explorer-core/engine/generated"
rm -rf "$out"
mkdir -p "$out"
TS_RS_EXPORT_DIR="$out" cargo test --manifest-path "$root/Cargo.toml" -p system_api_types export_
cd "$out"
: >index.ts
for f in $(ls ./*.ts 2>/dev/null | grep -v '^\./index\.ts$' | LC_ALL=C sort); do
    f="${f#./}"
    echo "export * from './${f%.ts}'" >>index.ts
done
