#!/usr/bin/env sh
# Cargo reads Bevy and the forked crates from ../bevy and ../forks, which are not tracked: write the
# ones that are missing or stale from ../patches.
# Called by the justfile's `_sources` and scripts/gen-ts-bindings.sh before they run Cargo.
set -eu
# A stock Windows has only a Microsoft Store stub for python3; its installed Python is `python`.
python=python3
"$python" -c '' >/dev/null 2>&1 || python=python
exec "$python" "$(dirname "$0")/../../../../tooling/patched-sources.py" materialise --quiet
