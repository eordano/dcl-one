#!/usr/bin/env bash
set -euo pipefail

protocol_dir="$(cd "$(dirname "$0")/.." && pwd)"
repo_root="$(git -C "$protocol_dir" rev-parse --show-toplevel)"
reference="${BUF_AGAINST_REF:-origin/main}"
buf="${BUF_BIN:-$protocol_dir/node_modules/.bin/buf}"

# Missing or incomplete baselines are errors, never a successful compatibility check.
commit="$(git -C "$repo_root" rev-parse --verify "$reference^{commit}")"
git -C "$repo_root" cat-file -e "$commit:library/protocol/proto"
workspace="$(mktemp -d "${TMPDIR:-/tmp}/dcl-protocol-buf.XXXXXX")"
trap 'rm -rf "$workspace"' EXIT
# Keep archive and extraction separate so either failure stops before Buf runs.
git -C "$repo_root" archive "$commit" library/protocol/proto > "$workspace/baseline.tar"
tar -xf "$workspace/baseline.tar" -C "$workspace" --strip-components=2
"$buf" breaking "$protocol_dir/proto" --against "$workspace/proto"
