#!/usr/bin/env bash
# Run the node test binaries concurrently; tests create their own scratch databases.
# The socket binaries (archipelago_*, pulse_*) keep wall-clock budgets, so they run with at
# most one test thread per core: after the others when the pool would oversubscribe the cores.
# CATALYST_TEST_SHARD=i/n runs every n-th binary from the i-th; without it every binary runs.
set -euo pipefail
source "$(dirname "$0")/catalyst-test-resources.sh"
prepare_catalyst_test_resources
cd "$(dirname "$0")/../services/catalyst"
logs="${CARGO_TARGET_DIR:-target}/test-logs"
mkdir -p "$logs"
cargo test --locked "$@" --no-run --message-format=json-render-diagnostics | python3 -c '
import json, sys
for line in sys.stdin:
    artifact = json.loads(line)
    if artifact.get("executable") and artifact.get("profile", {}).get("test"):
        print(artifact["executable"])
' > "$logs/binaries"
[ -s "$logs/binaries" ] || { echo "catalyst-test: no test binaries found" >&2; exit 1; }
shard="${CATALYST_TEST_SHARD:-1/1}"
index="${shard%/*}" total="${shard#*/}"
[[ "$shard" =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]] && [ "$index" -le "$total" ] || { echo "catalyst-test: CATALYST_TEST_SHARD must be i/n with i from 1 to n" >&2; exit 1; }
sort "$logs/binaries" | awk -v i="$index" -v n="$total" 'NR % n == i % n' > "$logs/shard"
cores=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN)
jobs="${CATALYST_TEST_JOBS:-$(( cores < 16 ? cores : 16 ))}"
[ "$jobs" -ge 1 ] || { echo "catalyst-test: CATALYST_TEST_JOBS must be at least 1" >&2; exit 1; }
# jobs * RUST_TEST_THREADS tests run at once, each with up to 10 of the cluster's 400 connections.
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-4}"
socket_jobs=$(( cores / RUST_TEST_THREADS ))
[ "$socket_jobs" -ge 1 ] || socket_jobs=1
[ "$socket_jobs" -le "$jobs" ] || socket_jobs="$jobs"
grep -E '/(archipelago|pulse)_[^/]*$' "$logs/shard" > "$logs/sockets" || [ $? -eq 1 ]
grep -v -E '/(archipelago|pulse)_[^/]*$' "$logs/shard" > "$logs/others" || [ $? -eq 1 ]
run() {
  xargs -P "$1" -I{} sh -c '
log="$2/$(basename "$1").log"
"$1" > "$log" 2>&1 || { cat "$log"; exit 1; }
sed -n "s|^test result|$(basename "$1")|p" "$log"
' _ {} "$logs"
}
if [ "$socket_jobs" -eq "$jobs" ]; then
  run "$jobs" < "$logs/shard"
else
  run "$jobs" < "$logs/others"
  run "$socket_jobs" < "$logs/sockets"
fi
cargo test --locked "$@" --doc
