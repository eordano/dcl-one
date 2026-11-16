#!/usr/bin/env bash
# Functional tests for explorer-ci-sections.sh on canned job and artifact lists: no network, no
# repo. Run from anywhere:
#   bash .github/actions/bevy-explorer-ci-status/test-explorer-ci-sections.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSER="$SCRIPT_DIR/explorer-ci-sections.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
PASSES=0
SHA=1111111111111111111111111111111111111111

# job '<name>' '<conclusion>' [seconds]: one job object; an empty conclusion is null.
job() {
  local completed=""
  [ -n "${3:-}" ] && completed=$(date -u -d "@$((1793181600 + $3))" +%Y-%m-%dT%H:%M:%SZ)
  jq -nc --arg n "$1" --arg c "$2" --arg e "$completed" '
    {name:$n, html_url:("https://github.com/o/r/actions/runs/1/job/" + ($n | gsub("[^A-Za-z0-9]"; "-"))),
     conclusion:(if $c == "" then null else $c end)}
    + (if $e == "" then {} else {started_at:"2026-10-28T10:00:00Z", completed_at:$e} end)'
}
jobs() { printf '%s\n' "$@" | jq -sc '{jobs: .}' > "$WORK/jobs.json"; }
artifacts() { # artifacts '<name>:<id>[:expired]' ...
  local out='{"artifacts":[' first=1 spec name id expired
  for spec in "$@"; do
    IFS=: read -r name id expired <<< "$spec"
    [ $first -eq 1 ] || out+=','
    first=0
    out+="{\"name\":\"$name\",\"id\":$id,\"expired\":${expired:-false},\"created_at\":\"2026-10-28T10:00:00Z\"}"
  done
  echo "$out]}" > "$WORK/artifacts.json"
}
run_metadata() { # run_metadata <event> [referenced workflow SHA ...]
  local event="$1"
  shift
  jq -nc --arg event "$event" --arg head "$SHA" '
    {event:$event, head_sha:$head, referenced_workflows:[$ARGS.positional[] as $sha |
      {ref:"refs/pull/10/merge", path:("o/r/.github/workflows/child.yml@" + $sha), sha:$sha}]}
  ' --args "$@" > "$WORK/run.json"
}
compose() {
  (cd "$WORK" && REPO=o/r RUN_ID=1 RUN_URL=https://github.com/o/r/actions/runs/1 HEAD_SHA="$SHA" \
    RUN_FILE=run.json PR_NUMBER=10 JOBS_FILE=jobs.json ARTIFACTS_FILE=artifacts.json bash "$COMPOSER")
}
check() { # check <name> <file> <pattern>
  if grep -qF -- "$3" "$WORK/$2"; then
    echo "  ok   $1"
    PASSES=$((PASSES + 1))
  else
    echo "  FAIL $1: '$3' not in $2:"
    sed 's/^/         /' "$WORK/$2"
    FAILURES=$((FAILURES + 1))
  fi
}
check_absent() {
  if grep -qF -- "$3" "$WORK/$2"; then
    echo "  FAIL $1: '$3' found in $2:"
    sed 's/^/         /' "$WORK/$2"
    FAILURES=$((FAILURES + 1))
  else
    echo "  ok   $1"
    PASSES=$((PASSES + 1))
  fi
}

echo "test-explorer-ci-sections.sh"
run_metadata push

# 1. Everything ran and passed, with every product's artifact.
jobs "$(job 'Integration guards' success 30)" "$(job Changes success 40)" \
  "$(job 'Test Suite (windows-2025)' success 2700)" "$(job 'Test Suite (ubuntu-24.04)' success 1500)" \
  "$(job 'Test Suite (macos-15)' success 3700)" "$(job 'Headless Smoke' success 900)" \
  "$(job 'Web engine, HUD and bridge scene' success 1300)" "$(job 'Web package for the CDN' success 240)" \
  "$(job Rustfmt success 20)"
artifacts "bevy-explorer-web-$SHA:11" "bevy-headless-linux-x64-$SHA:12" \
  "bevy-explorer-headless-linux-amd64-$SHA:13" "bevy-explorer-benchmark-linux-amd64-$SHA:14" \
  "bevy-explorer-benchmark-windows-amd64-$SHA:15" "bevy-explorer-benchmark-macos-arm64-$SHA:16" \
  "bevy-explorer-native-timings-Windows-X64-$SHA:17"
compose
check "green build badge" build.md 'Build-Success!-3fb950'
check "web app row links the job, the result and the artifact" build.md \
  '| [Web app](https://github.com/o/r/actions/runs/1/job/Web-engine--HUD-and-bridge-scene) | ✅ Built | 21m 40s | [web app](https://github.com/o/r/actions/runs/1/artifacts/11) |'
check "web package row links the job and the result" build.md \
  '| [Web package](https://github.com/o/r/actions/runs/1/job/Web-package-for-the-CDN) | ✅ Built | 4m 0s | — |'
check "headless row lists both artifacts" build.md \
  '| ✅ Built | 15m 0s | [npm package](https://github.com/o/r/actions/runs/1/artifacts/12) · [engine](https://github.com/o/r/actions/runs/1/artifacts/13) |'
check "benchmark runners by platform, sorted" build.md \
  'Benchmark runner: [linux-amd64](https://github.com/o/r/actions/runs/1/artifacts/14) · [macos-arm64](https://github.com/o/r/actions/runs/1/artifacts/16) · [windows-amd64](https://github.com/o/r/actions/runs/1/artifacts/15)'
check_absent "timings artifacts are not products" build.md 'native-timings'
check "green tests badge" tests.md 'Tests-Passed!-3fb950'
check "test rows by platform with job time" tests.md '| [Test Suite (Windows)](https://github.com/o/r/actions/runs/1/job/Test-Suite--windows-2025-) | ✅ Passed | 45m 0s |'
check "macOS row" tests.md '| [Test Suite (macOS)]('
check "an hour-long job" tests.md '| ✅ Passed | 1h 1m |'
check "headless smoke is a test row" tests.md '| [Headless Smoke](https://github.com/o/r/actions/runs/1/job/Headless-Smoke) | ✅ Passed | 15m 0s |'
check_absent "rustfmt is not a test row" tests.md 'Rustfmt'
check "footer names the run and the commit" tests.md '<sub>[Run summary](https://github.com/o/r/actions/runs/1) · PR head `1111111`</sub>'

# 2. A failed Linux suite, a cancelled Windows one, a web build that failed with no artifact.
jobs "$(job Changes success 40)" "$(job 'Test Suite (windows-2025)' cancelled 100)" \
  "$(job 'Test Suite (ubuntu-24.04)' failure 1500)" "$(job 'Web engine, HUD and bridge scene' failure 300)" \
  "$(job 'Web package for the CDN' skipped)" "$(job 'Headless Smoke' skipped)"
artifacts "bevy-explorer-benchmark-linux-amd64-$SHA:14"
compose
check "red build badge" build.md 'Build-Failed!-ff0000'
check "a failed web build has no download" build.md '| ❌ Failed | 5m 0s | — |'
check "a skipped product is not selected" build.md '| — Not selected | — | — |'
check_absent "a skipped web package has no row" build.md 'Web package'
check "red tests badge" tests.md 'Tests-Failed!-ff0000'
check "a cancelled suite" tests.md '| ⚠️ Cancelled | 1m 40s |'
check "a failed suite" tests.md '| ❌ Failed | 25m 0s |'
check "a skipped suite" tests.md '| [Headless Smoke](https://github.com/o/r/actions/runs/1/job/Headless-Smoke) | — Not selected | — |'

# 3. Only the web build was selected: the tests section says so.
jobs "$(job Changes success 40)" "$(job 'Test Suite' skipped)" "$(job 'Headless Smoke' skipped)" \
  "$(job 'Web engine, HUD and bridge scene' success 1300)"
artifacts "bevy-explorer-web-$SHA:11"
compose
check "green build badge with only the web app" build.md 'Build-Success!-3fb950'
check "tests not selected badge" tests.md 'Tests-Not%20selected-lightgrey'
check "tests headline" tests.md 'No test job was selected: the test inputs are unchanged, or the pull request is a draft and the tests wait until it is ready for review.'

# 3b. The same run and the newer runs of the same workflow and head. Run 1 is this run.
head_runs() { # head_runs '<id>:<workflow id>:<status>:<event>[:<head sha>]' ...
  jq -nc --arg head "$SHA" '{workflow_runs: $ARGS.positional | map(split(":") | {id:(.[0] | tonumber),
    workflow_id:(.[1] | tonumber), status:.[2], event:.[3], head_sha:(.[4] // $head),
    html_url:("https://github.com/o/r/actions/runs/" + .[0])})}' --args "$@" > "$WORK/head-runs.json"
}
compose_with_head_runs() { # compose_with_head_runs [run id]
  rm -f "$WORK/tests.md"
  (cd "$WORK" && REPO=o/r RUN_ID="${1:-1}" RUN_URL=https://github.com/o/r/actions/runs/1 HEAD_SHA="$SHA" \
    RUN_FILE=run.json PR_NUMBER=10 JOBS_FILE=jobs.json ARTIFACTS_FILE=artifacts.json \
    HEAD_RUNS_FILE=head-runs.json bash "$COMPOSER")
}
not_selected='No test job was selected'
run_metadata pull_request
jq -c '. + {workflow_id: 7}' "$WORK/run.json" > "$WORK/run.tmp" && mv "$WORK/run.tmp" "$WORK/run.json"
head_runs 1:7:completed:pull_request 2:7:pending:pull_request 3:8:queued:pull_request 4:7:queued:push
compose_with_head_runs
check "tests pending badge while a newer run has not ended" tests.md 'Tests-Pending!-yellow'
check "the headline links the newer run" tests.md \
  'A newer [run](https://github.com/o/r/actions/runs/2) of this commit has not ended. This section updates when that run reports.'
check_absent "no not selected headline while a newer run has not ended" tests.md "$not_selected"
head_runs 2:7:queued:pull_request 6:7:in_progress:pull_request
compose_with_head_runs
check "the newest of two runs that have not ended is the link" tests.md 'actions/runs/6)'
head_runs 1:7:in_progress:pull_request
compose_with_head_runs
check "this run in the list is no newer run" tests.md "$not_selected"
head_runs 3:8:queued:pull_request 4:7:queued:push
compose_with_head_runs
check "a run of another workflow or another event is no newer run" tests.md "$not_selected"
head_runs 2:7:queued:pull_request:2222222222222222222222222222222222222222
compose_with_head_runs
check "a run of another commit is no newer run" tests.md "$not_selected"
head_runs 2:7:queued:pull_request 3:7:completed:pull_request
compose_with_head_runs 5
check "an older run that has not ended is no newer run" tests.md "$not_selected"
head_runs 5:7:completed:pull_request
compose_with_head_runs
if [ -e "$WORK/tests.md" ]; then
  echo "  FAIL a newer run that ended keeps its tests section: tests.md was written"
  FAILURES=$((FAILURES + 1))
else
  echo "  ok   a newer run that ended keeps its tests section"
  PASSES=$((PASSES + 1))
fi
check "the build section is still written" build.md 'Build-Success!-3fb950'
head_runs 2:7:queued:pull_request 5:7:completed:pull_request
compose_with_head_runs
check "a newer run that has not ended is the headline beside one that ended" tests.md 'actions/runs/2)'
run_metadata pull_request
head_runs 2:7:queued:pull_request
compose_with_head_runs
check "a run with no workflow id names no newer run" tests.md "$not_selected"
echo '{}' > "$WORK/head-runs.json"
compose_with_head_runs
check "an empty run list, as for a closed pull request, names no newer run" tests.md "$not_selected"
run_metadata push

# 4. Only the tests were selected, and an expired artifact is no download.
jobs "$(job Changes success 40)" "$(job 'Test Suite (ubuntu-24.04)' success 1500)" \
  "$(job 'Web engine, HUD and bridge scene' skipped)" "$(job 'Headless Smoke' skipped)"
artifacts "bevy-explorer-web-$SHA:11:true" "bevy-explorer-benchmark-linux-amd64-$SHA:14"
compose
check "build not selected badge" build.md 'Build-Not%20selected-lightgrey'
check_absent "an expired artifact is no download" build.md 'artifacts/11'
check "green tests badge with one suite" tests.md 'Tests-Passed!-3fb950'

# 5. The run was cancelled: both sections say to refresh.
jobs "$(job Changes success 40)" "$(job 'Test Suite (ubuntu-24.04)' cancelled 10)" \
  "$(job 'Web engine, HUD and bridge scene' cancelled 10)"
artifacts
compose
check "grey build badge" build.md 'Build-Cancelled-lightgrey'
check "grey tests badge" tests.md 'Tests-Cancelled-lightgrey'
check "the cancelled headline" tests.md 'Push a new commit or re-run the workflow to refresh this section.'

# 6. A PR preview uses the actual web checkout and the package from this run.
WEB_SHA=abc1234222222222222222222222222222222222
VERSION=0.1.1-1.commit-abc1234
SPOOF_SHA=abc1234333333333333333333333333333333333
run_metadata pull_request "$WEB_SHA" "$WEB_SHA"
jobs "$(job Changes success 40)" "$(job 'Web engine, HUD and bridge scene' success 1300)"
artifacts "bevy-explorer-web-$WEB_SHA:11" "dcl-regenesislabs-bevy-explorer-web-$VERSION.tgz:12" \
  "bevy-explorer-web-$SPOOF_SHA:13"
jq '.artifacts[-1].created_at = "2026-10-28T11:00:00Z"' "$WORK/artifacts.json" > "$WORK/new.json"
mv "$WORK/new.json" "$WORK/artifacts.json"
cat > "$WORK/npm" <<'EOF'
#!/usr/bin/env bash
set -eu
[[ "$1" = view ]]
case "$2 $3" in
  '@dcl-regenesislabs/bevy-explorer-web@pr-10 version') printf '%s\n' "${TAG_VERSION:-}" ;;
  *) exit 64 ;;
esac
EOF
cat > "$WORK/curl" <<'EOF'
#!/usr/bin/env bash
set -eu
url="${*: -1}"
[ "$url" = "https://cdn.decentraland.org/@dcl-regenesislabs/bevy-explorer-web/$PUBLISHED_VERSION/.deploy-complete.json" ] || exit 22
if [ -n "${MARKER_BODY+set}" ]; then printf '%s' "$MARKER_BODY"; exit 0; fi
jq -nc --arg v "$PUBLISHED_VERSION" --arg c "$PUBLISHED_SHA" \
  '{package:"@dcl-regenesislabs/bevy-explorer-web", version:$v, commit:$c, objectCount:1, kind:"upload"}'
EOF
chmod +x "$WORK/npm" "$WORK/curl"
preview_compose() {
  PATH="$WORK:$PATH" PUBLISHED_VERSION="$VERSION" PUBLISHED_SHA="$1" compose
}
preview_compose "$WEB_SHA"
check "the preview is linked after the web app" build.md \
  "[web app](https://github.com/o/r/actions/runs/1/artifacts/11) · [preview](https://cdn.decentraland.org/@dcl-regenesislabs/bevy-explorer-web/$VERSION/index.html?position=0%2C-2&baseDomain=decentraland.zone) |"
check "the build report distinguishes the PR head from the web build" build.md \
  'PR head `1111111` · Web build `abc1234`'
check_absent "a newer artifact with another SHA is ignored" build.md 'artifacts/13'
TAG_VERSION=0.1.1-2.commit-abc1234 preview_compose "$WEB_SHA"
check "a newer PR tag does not change the versioned link" build.md "$VERSION/index.html"
preview_compose "$SPOOF_SHA"
check_absent "an upload with a different full SHA is not linked" build.md '[preview]'
MARKER_BODY='' preview_compose "$WEB_SHA"
check_absent "an empty marker is not linked" build.md '[preview]'
MARKER_BODY='<html>error</html>' preview_compose "$WEB_SHA"
check_absent "a non-JSON marker is not linked" build.md '[preview]'
artifacts "bevy-explorer-web-$WEB_SHA:11" "dcl-regenesislabs-bevy-explorer-web-0.1.1-2.commit-abc1234.tgz:12"
VERSION=0.1.1-2.commit-abc1234 preview_compose "$WEB_SHA"
check_absent "a package for a different run is not linked" build.md '[preview]'
artifacts "bevy-explorer-web-$WEB_SHA:11" "dcl-regenesislabs-bevy-explorer-web-0.1.1-1.commit-7654321.tgz:12"
VERSION=0.1.1-1.commit-7654321 preview_compose "$WEB_SHA"
check_absent "a version for a different checkout is not linked" build.md '[preview]'
artifacts "bevy-explorer-web-$WEB_SHA:11:true" "dcl-regenesislabs-bevy-explorer-web-$VERSION.tgz:12"
preview_compose "$WEB_SHA"
check "an expired Actions download can still have a CDN preview" build.md '[preview]'
check_absent "the expired Actions download is hidden" build.md 'artifacts/11'
artifacts "bevy-explorer-web-$WEB_SHA:11"
TAG_VERSION="$VERSION" preview_compose "$WEB_SHA"
check "a published preview can outlive its package artifact" build.md '[preview]'
TAG_VERSION=0.1.1-2.commit-abc1234 preview_compose "$WEB_SHA"
check_absent "the fallback tag must still point to this run" build.md '[preview]'
BOGUS_VERSION=999.0.0-1.commit-abc1234
artifacts "bevy-explorer-web-$WEB_SHA:11" "dcl-regenesislabs-bevy-explorer-web-$BOGUS_VERSION.tgz:13"
TAG_VERSION="$VERSION" preview_compose "$WEB_SHA"
check "an unpublished artifact candidate does not block the PR tag fallback" build.md "$VERSION/index.html"
artifacts "bevy-explorer-web-$WEB_SHA:11" "dcl-regenesislabs-bevy-explorer-web-$VERSION.tgz:12" \
  "dcl-regenesislabs-bevy-explorer-web-$BOGUS_VERSION.tgz:13"
jq '.artifacts[-1].created_at = "2026-10-28T11:00:00Z"' "$WORK/artifacts.json" > "$WORK/new.json"
mv "$WORK/new.json" "$WORK/artifacts.json"
TAG_VERSION=0.1.1-2.commit-abc1234 preview_compose "$WEB_SHA"
check "a newer unpublished candidate does not hide an earlier published artifact" build.md "$VERSION/index.html"
run_metadata pull_request "$WEB_SHA" "$SPOOF_SHA"
TAG_VERSION="$VERSION" preview_compose "$WEB_SHA"
check_absent "ambiguous run metadata produces no web download" build.md 'artifacts/11'
check_absent "ambiguous run metadata produces no web build label" build.md 'Web build'
check_absent "ambiguous run metadata produces no preview" build.md '[preview]'
run_metadata pull_request
TAG_VERSION="$VERSION" preview_compose "$WEB_SHA"
check_absent "missing merge metadata produces no preview" build.md '[preview]'
run_metadata pull_request "$WEB_SHA"
jq '.referenced_workflows[0].path = "other/r/.github/workflows/child.yml@refs/pull/10/merge"' \
  "$WORK/run.json" > "$WORK/foreign.json"
mv "$WORK/foreign.json" "$WORK/run.json"
TAG_VERSION="$VERSION" preview_compose "$WEB_SHA"
check_absent "an external reusable workflow does not establish the checkout" build.md 'Web build'

run_metadata workflow_dispatch
VERSION=0.1.0-1.commit-1111111
artifacts "bevy-explorer-web-$SHA:11" "dcl-regenesislabs-bevy-explorer-web-$VERSION.tgz:12"
TAG_VERSION="$VERSION" preview_compose "$SHA"
check "a branch dispatch published to the PR tag has a preview" build.md '[preview]'
preview_compose "$SHA"
check_absent "a branch dispatch published to another tag has no preview" build.md '[preview]'
jobs "$(job Changes success 40)" "$(job 'Web engine, HUD and bridge scene' failure 300)"
TAG_VERSION="$VERSION" preview_compose "$SHA"
check_absent "a failed web build has no preview" build.md '[preview]'
jobs "$(job Changes success 40)" "$(job 'Web engine, HUD and bridge scene' skipped)"
TAG_VERSION="$VERSION" preview_compose "$SHA"
check_absent "a skipped web build does not link an artifact" build.md 'artifacts/11'

# 7. The web app built and its package failed: the build failed.
jobs "$(job Changes success 40)" "$(job 'Web engine, HUD and bridge scene' success 1300)" \
  "$(job 'Web package for the CDN' failure 200)"
artifacts "bevy-explorer-web-$SHA:11"
compose
check "a failed web package gives the red build badge" build.md 'Build-Failed!-ff0000'
check "the web app row still passes" build.md '| ✅ Built | 21m 40s | [web app](https://github.com/o/r/actions/runs/1/artifacts/11) |'
check "the failed web package row" build.md \
  '| [Web package](https://github.com/o/r/actions/runs/1/job/Web-package-for-the-CDN) | ❌ Failed | 3m 20s | — |'

# 8. A job name with table furniture renders as text.
jobs "$(job Changes success 40)" "$(job 'Test Suite (ubuntu|24.04)' success 10)"
artifacts
compose
check "pipes are stripped from a suite name" tests.md '| [Test Suite (Linux)]('

# 9. The early report: the tests ended and the web and image jobs still run. The caller gives an
# empty run file and artifact list, so the build section, which it does not use, makes no lookup.
jobs "$(job Changes success 40)" "$(job 'Test Suite (ubuntu-24.04)' success 500)" \
  "$(job 'Test Suite (macos-15)' failure 300)" "$(job 'Headless Smoke' success 480)" \
  "$(job 'Web engine, HUD and bridge scene' '')" "$(job 'APIs + Sites / build (amd64, ubuntu-26.04)' '')"
artifacts
echo '{}' > "$WORK/run.json"
compose
check "the early tests section has the verdict" tests.md 'Tests-Failed!-ff0000'
check "the early tests section has the row of each ended job" tests.md '| ✅ Passed | 8m 20s |'
check_absent "the build section of an early report has no download" build.md 'artifacts/'

echo
if [ "$FAILURES" -gt 0 ]; then
  echo "$FAILURES failed, $PASSES passed"
  exit 1
fi
echo "all $PASSES passed"
