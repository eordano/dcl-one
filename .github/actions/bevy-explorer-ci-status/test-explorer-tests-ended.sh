#!/usr/bin/env bash
# Functional tests for explorer-tests-ended.sh against a stubbed gh that serves canned /jobs and
# /runs responses from files: no network, no repo. Each scenario can hand out a different response
# per poll iteration (jobs.1.json, jobs.2.json, ...) to exercise the wait loop. Run from anywhere:
#   bash .github/actions/bevy-explorer-ci-status/test-explorer-tests-ended.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROBE="$SCRIPT_DIR/explorer-tests-ended.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- gh stub ---------------------------------------------------------------------------------
# Serves $SCENARIO/<kind>.<call-number>.json, falling back to the highest numbered file at or
# below the current call, so a scenario that wants one steady answer just writes <kind>.1.json.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [ -n "${GH_FAIL_EXIT:-}" ]; then exit "$GH_FAIL_EXIT"; fi
[ "$1" = api ] || { echo "gh stub: unsupported subcommand $1" >&2; exit 64; }
path="$2"
case "$path" in
  */jobs*) kind=jobs ;;
  *)       kind=run ;;
esac
counter="$SCENARIO/.calls.$kind"
n=1
[ -f "$counter" ] && n=$(( $(cat "$counter") + 1 ))
echo "$n" > "$counter"
file=""
for try in $(seq "$n" -1 1); do
  if [ -f "$SCENARIO/$kind.$try.json" ]; then file="$SCENARIO/$kind.$try.json"; break; fi
done
[ -n "$file" ] || { echo "gh stub: no canned $kind response for call $n" >&2; exit 65; }
if [ "${3:-}" = "--jq" ]; then jq -r "$4" < "$file"; else cat "$file"; fi
STUB
chmod +x "$WORK/bin/gh"
cat > "$WORK/bin/sleep" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$1" >> "$SCENARIO/.sleeps"
STUB
chmod +x "$WORK/bin/sleep"
export PATH="$WORK/bin:$PATH"

# --- helpers ---------------------------------------------------------------------------------
FAILURES=0
PASSES=0

jobs_json() { # jobs_json '<name>:<status>:<conclusion>' ...
  local out='{"jobs":[' first=1 spec name status conclusion
  for spec in "$@"; do
    IFS=: read -r name status conclusion <<< "$spec"
    [ $first -eq 1 ] || out+=','
    first=0
    if [ -z "$conclusion" ]; then
      out+="{\"name\":\"$name\",\"status\":\"$status\",\"conclusion\":null}"
    else
      out+="{\"name\":\"$name\",\"status\":\"$status\",\"conclusion\":\"$conclusion\"}"
    fi
  done
  echo "$out]}"
}

new_scenario() {
  SCENARIO="$WORK/$1"
  mkdir -p "$SCENARIO"
  export SCENARIO
}

# expect <name> <expected-stdout> <expected-exit>
expect() {
  local name="$1" want="$2" want_code="$3" got code=0
  got=$(RUN_ID=42 REPO=o/r WAIT_SECONDS="${WAIT:-0}" POLL_SECONDS=30 \
        bash "$PROBE" 2>"$SCENARIO/stderr") || code=$?
  if [ "$got" = "$want" ] && [ "$code" -eq "$want_code" ]; then
    echo "  ok   $name"
    PASSES=$((PASSES + 1))
  else
    echo "  FAIL $name: want stdout='$want' exit=$want_code, got stdout='$got' exit=$code"
    sed 's/^/         /' "$SCENARIO/stderr"
    FAILURES=$((FAILURES + 1))
  fi
}

# expect_calls <name> <kind> <count>: how many times the last scenario read <kind> (jobs | run).
expect_calls() {
  local name="$1" kind="$2" want="$3" got=0
  [ ! -f "$SCENARIO/.calls.$kind" ] || got=$(cat "$SCENARIO/.calls.$kind")
  if [ "$got" -eq "$want" ]; then
    echo "  ok   $name"
    PASSES=$((PASSES + 1))
  else
    echo "  FAIL $name: want $want $kind reads, got $got"
    FAILURES=$((FAILURES + 1))
  fi
}

echo "test-explorer-tests-ended.sh"

# 1. The tests ended and the web build still runs: the tests section can be written now.
new_scenario tests-ended-run-goes-on
jobs_json 'Changes:completed:success' 'Test Suite (macos-15):completed:success' \
  'Test Suite (ubuntu-24.04):completed:success' 'Test Suite (windows-2025):completed:success' \
  'Headless Smoke:completed:success' 'Web engine, HUD and bridge scene:in_progress:' > "$SCENARIO/jobs.1.json"
echo '{"status":"in_progress"}' > "$SCENARIO/run.1.json"
expect "tests ended, the run goes on => true" true 0

# 2. A failed test job is an ended test job.
new_scenario a-test-failed
jobs_json 'Test Suite (macos-15):completed:failure' 'Test Suite (ubuntu-24.04):completed:success' \
  'Headless Smoke:completed:cancelled' 'APIs + Sites / build (amd64, ubuntu-26.04):queued:' > "$SCENARIO/jobs.1.json"
echo '{"status":"in_progress"}' > "$SCENARIO/run.1.json"
expect "a failed test job and the run goes on => true" true 0

# 3. One platform still runs: without a wait budget the answer is no.
new_scenario one-platform-open
jobs_json 'Test Suite (macos-15):completed:success' 'Test Suite (windows-2025):in_progress:' \
  'Headless Smoke:completed:success' > "$SCENARIO/jobs.1.json"
expect "a test job still runs, no budget => false" false 0
expect_calls "an open test job needs no read of the run" run 0

# 4. Headless Smoke is not listed yet (it waits for its needs): the tests have not ended.
new_scenario smoke-not-listed
jobs_json 'Changes:completed:success' 'Test Suite (macos-15):completed:success' \
  'Test Suite (ubuntu-24.04):completed:success' > "$SCENARIO/jobs.1.json"
echo '{"status":"in_progress"}' > "$SCENARIO/run.1.json"
expect "Headless Smoke not listed, no budget => false" false 0

# 5. Only Headless Smoke is listed: the Test Suite jobs are not listed yet.
new_scenario suite-not-listed
jobs_json 'Changes:completed:success' 'Headless Smoke:completed:success' > "$SCENARIO/jobs.1.json"
echo '{"status":"in_progress"}' > "$SCENARIO/run.1.json"
expect "Test Suite not listed, no budget => false" false 0

# 6. A draft, or unchanged test inputs: every test job is skipped and the completed run reports.
new_scenario tests-skipped
jobs_json 'Changes:completed:success' 'Test Suite:completed:skipped' 'Headless Smoke:completed:skipped' \
  'Web engine, HUD and bridge scene:in_progress:' > "$SCENARIO/jobs.1.json"
WAIT=600 expect "every test job skipped => false, at once" false 0
expect_calls "skipped tests end the wait on the first read" jobs 1

# 7. The tests ended in the same moment as the run: the completed run reports both sections.
new_scenario run-ended-with-tests
jobs_json 'Test Suite (macos-15):completed:success' 'Headless Smoke:completed:success' \
  'Web engine, HUD and bridge scene:completed:success' > "$SCENARIO/jobs.1.json"
echo '{"status":"completed"}' > "$SCENARIO/run.1.json"
expect "tests ended and the run ended => false" false 0

# 8. Cancelled before the test jobs were listed (a new push, or a failed guard).
new_scenario run-ended-early
jobs_json 'Integration guards:completed:failure' 'Changes:completed:cancelled' > "$SCENARIO/jobs.1.json"
echo '{"status":"completed"}' > "$SCENARIO/run.1.json"
WAIT=600 expect "the run ended before its tests were listed => false, at once" false 0
expect_calls "an ended run ends the wait on the first read" jobs 1

# 9. The wait: nothing listed, then running, then ended while the image build goes on.
new_scenario waits-for-the-tests
jobs_json 'Changes:completed:success' > "$SCENARIO/jobs.1.json"
jobs_json 'Test Suite (macos-15):in_progress:' 'Test Suite (ubuntu-24.04):queued:' \
  'Headless Smoke:queued:' > "$SCENARIO/jobs.2.json"
jobs_json 'Test Suite (macos-15):completed:success' 'Test Suite (ubuntu-24.04):in_progress:' \
  'Headless Smoke:in_progress:' > "$SCENARIO/jobs.3.json"
jobs_json 'Test Suite (macos-15):completed:success' 'Test Suite (ubuntu-24.04):completed:success' \
  'Headless Smoke:in_progress:' > "$SCENARIO/jobs.5.json"
jobs_json 'Test Suite (macos-15):completed:success' 'Test Suite (ubuntu-24.04):completed:success' \
  'Headless Smoke:completed:success' 'APIs + Sites / build (amd64, ubuntu-26.04):in_progress:' > "$SCENARIO/jobs.6.json"
echo '{"status":"in_progress"}' > "$SCENARIO/run.1.json"
WAIT=2400 expect "waits until the last test job ends => true" true 0
if [ "$(tr '\n' ' ' < "$SCENARIO/.sleeps")" != '5 10 20 30 30 ' ]; then
  echo "FAIL: the pauses between reads changed: $(tr '\n' ' ' < "$SCENARIO/.sleeps")"
  exit 1
fi
expect_calls "one read of the job list for each pause, and the first" jobs 6
expect_calls "the run is read for the incomplete list and before the answer" run 2

# 10. Out of wait budget with a test job still running: the completed run reports.
new_scenario open-past-deadline
jobs_json 'Test Suite (macos-15):in_progress:' 'Headless Smoke:queued:' > "$SCENARIO/jobs.1.json"
expect "a test job open past the deadline => false" false 0

new_scenario github-read-failed
export GH_FAIL_EXIT=41
expect "unexpected GitHub read failure remains a hard error" "" 41
unset GH_FAIL_EXIT

echo
if [ "$FAILURES" -gt 0 ]; then
  echo "$FAILURES failed, $PASSES passed"
  exit 1
fi
echo "all $PASSES passed"
