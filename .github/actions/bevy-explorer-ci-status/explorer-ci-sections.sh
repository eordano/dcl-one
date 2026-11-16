#!/usr/bin/env bash
# Writes the build and tests sections of the CI status comment for a completed Bevy Explorer / CI
# run, from the run's job and artifact lists (the trusted Actions API; the run's own code reaches
# this only as job names and artifact names, which render inside table cells).
#
# Environment: REPO (owner/name), PR_NUMBER, RUN_ID, RUN_URL, HEAD_SHA (the PR head), RUN_FILE
# (the run attempt's metadata), JOBS_FILE (the run's /jobs response), ARTIFACTS_FILE (its
# /artifacts response) and OUT_DIR (where build.md and tests.md are written). Optional:
# HEAD_RUNS_FILE (the runs of the PR head, an /actions/runs response; the caller gives it only for
# an open pull request).
#
# The build section covers the products: the web app (*Web engine, HUD and bridge scene*), its
# package (*Web package for the CDN*, a row only when the job ran) and the headless engine
# (*Headless Smoke*), with their artifacts' download links, and the `benchmark` runners the
# *Test Suite* uploads. The tests section covers *Test Suite* on each platform and
# *Headless Smoke*. A job *Changes* did not select (`skipped`) is reported as not selected; a
# section none of whose jobs ran says its inputs are unchanged; the tests section adds that a draft
# holds its tests, because the job list does not tell the two apart. A run with no test job reads
# the newer runs of the same workflow for the same head (the run that ready_for_review starts
# waits for this one): while one has not ended, the tests section says so and links it; when one
# has ended, that run wrote the tests section and this run writes none.
#
# The early report of the tests (the test jobs ended and the run goes on) uses only tests.md: its
# caller gives an empty RUN_FILE (`{}`) and an ARTIFACTS_FILE without artifacts, so the build
# section it also writes makes no lookup.
#
# Usage: REPO=o/r PR_NUMBER=10 RUN_ID=1 RUN_URL=... HEAD_SHA=... RUN_FILE=run.json \
#          JOBS_FILE=jobs.json ARTIFACTS_FILE=artifacts.json bash explorer-ci-sections.sh
set -euo pipefail

: "${REPO:?REPO is required}"
: "${PR_NUMBER:?PR_NUMBER is required}"
: "${RUN_ID:?RUN_ID is required}"
: "${RUN_URL:?RUN_URL is required}"
: "${HEAD_SHA:?HEAD_SHA is required}"
: "${RUN_FILE:?RUN_FILE is required}"
: "${JOBS_FILE:?JOBS_FILE is required}"
: "${ARTIFACTS_FILE:?ARTIFACTS_FILE is required}"
OUT_DIR="${OUT_DIR:-.}"

WEB_JOB='Web engine, HUD and bridge scene'
WEB_PACKAGE_JOB='Web package for the CDN'
HEADLESS_JOB='Headless Smoke'
TEST_JOB_PREFIX='Test Suite'
BADGE_STYLE='logoColor=white&style=for-the-badge'

# Same h/m/s convention as the benchmark report's durations.
fmt_secs() {
  awk -v s="$1" 'BEGIN { s=int(s+0.5); h=int(s/3600); m=int(s%3600/60); r=s%60;
    if (h>0) printf("%dh %dm", h, m); else if (m>0) printf("%dm %ds", m, r); else printf("%ds", r) }'
}

# A job's fields by exact name: conclusion, html_url and wall time in seconds, tab-separated;
# empty when the run has no such job. jq's output loses any carriage return, so a Windows jq
# works too.
job_fields() {
  local conclusion url started completed secs=""
  IFS=$'	' read -r conclusion url started completed <<< "$(jq -r --arg n "$1" '
    first(.jobs[] | select(.name == $n)) // empty
    | [(.conclusion // ""), (.html_url // ""), (.started_at // ""), (.completed_at // "")] | @tsv' "$JOBS_FILE" | tr -d '\r')" || true
  [ -n "${url:-}${conclusion:-}" ] || return 0
  if [ -n "$started" ] && [ -n "$completed" ]; then
    secs=$(( $(date -u -d "$completed" +%s) - $(date -u -d "$started" +%s) ))
  fi
  printf '%s	%s	%s' "$conclusion" "$url" "$secs"
}

# The names of the jobs starting with $1, sorted.
job_names_with_prefix() {
  jq -r --arg p "$1" '[.jobs[] | select(.name | startswith($p)) | .name] | sort | .[]' "$JOBS_FILE" | tr -d '\r'
}

# The result cell of a job conclusion. $2 names the success wording.
result_cell() {
  case "$1" in
    success) printf '✅ %s' "$2" ;;
    failure) printf '❌ Failed' ;;
    cancelled) printf '⚠️ Cancelled' ;;
    timed_out) printf '⚠️ Timed out' ;;
    skipped) printf '— Not selected' ;;
    '') printf '🕓 In progress' ;;
    *) printf '⚠️ `%s`' "$1" ;;
  esac
}

# Folds a job conclusion into a section state: failed beats cancelled beats passed; skipped and
# absent jobs leave it as it is.
fold_state() {
  local state="$1" conclusion="$2"
  case "$conclusion" in
    success) [ "$state" = none ] && state=passed ;;
    skipped|'') ;;
    cancelled) [ "$state" != failed ] && state=cancelled ;;
    *) state=failed ;;
  esac
  printf '%s' "$state"
}

# The download link of the newest unexpired artifact whose name starts with $1, labelled $2;
# empty when the run has none.
artifact_link() {
  local id
  id=$(jq -r --arg p "$1" '[.artifacts[] | select(.expired == false and (.name | startswith($p)))]
    | max_by(.created_at) | .id // empty' "$ARTIFACTS_FILE")
  [ -n "$id" ] && printf '[%s](https://github.com/%s/actions/runs/%s/artifacts/%s)' "$2" "$REPO" "$RUN_ID" "$id"
  return 0
}

job_time() { if [[ "$1" =~ ^[0-9]+$ ]]; then fmt_secs "$1"; else printf '—'; fi; }
job_cell() { if [ -n "$2" ]; then printf '[%s](%s)' "$1" "$2"; else printf '%s' "$1"; fi; }

npm_preview_field() {
  local value
  if value=$(npm view "@dcl-regenesislabs/bevy-explorer-web@$1" "$2" --prefer-online \
      --registry=https://registry.npmjs.org --fetch-retries=0 --fetch-timeout=15000 2>/dev/null); then
    printf '%s' "$value" | tr -d '\r'
  fi
}

# The upload's completion marker: written last, only through the CDN broker, and readable before
# npm serves the version.
preview_matches_checkout() {
  local version="$1" revision="$2" marker
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+-$RUN_ID\.commit-([0-9a-f]{7,40})$ &&
     "$revision" == "${BASH_REMATCH[1]}"* ]] || return 1
  marker=$(curl -fsS --max-time 15 --retry 2 --retry-connrefused \
    "https://cdn.decentraland.org/@dcl-regenesislabs/bevy-explorer-web/$version/.deploy-complete.json" \
    2>/dev/null) || return 1
  jq -e --arg v "$version" --arg c "$revision" \
    '.package == "@dcl-regenesislabs/bevy-explorer-web" and .version == $v and .commit == $c' \
    <<< "$marker" >/dev/null 2>&1
}

FOOTER="[Run summary]($RUN_URL) · PR head \`${HEAD_SHA:0:7}\`"
web_revision=""

# --- build -----------------------------------------------------------------------------------
state=none
rows=""
IFS=$'\t' read -r conclusion url secs <<< "$(job_fields "$WEB_JOB")" || true
if [ -n "${url:-}${conclusion:-}" ]; then
  state=$(fold_state "$state" "$conclusion")
  # GitHub records the exact SHA when it resolves a local reusable workflow in
  # this run attempt. For a PR this is the test merge; head_sha is the branch tip.
  # Only accept a single SHA from this repository's references to this PR merge.
  IFS=$'\t' read -r event checkout_sha <<< "$(jq -r --arg head "$HEAD_SHA" \
    --arg ref "refs/pull/$PR_NUMBER/merge" --arg prefix "$REPO/.github/workflows/" '
    select(.head_sha == $head)
    | [.event, (if .event == "pull_request" then
        [.referenced_workflows[]?
          | select(.ref == $ref and (.sha // "" | test("^[0-9a-f]{40}$")))
          | .sha as $sha | select(.path | startswith($prefix) and endswith("@" + $sha)) | $sha]
        | unique | if length == 1 then .[0] else "" end
      else .head_sha end)] | @tsv' "$RUN_FILE" | tr -d '\r')"
  downloads=""
  if [ "$conclusion" != skipped ] && [[ "$checkout_sha" =~ ^[0-9a-f]{40}$ ]]; then
    web_id=$(jq -r --arg name "bevy-explorer-web-$checkout_sha" '
      [.artifacts[] | select(.name == $name and .expired == false)]
      | if length == 1 then .[0].id else empty end' "$ARTIFACTS_FILE" | tr -d '\r')
    if [ -n "$web_id" ]; then
      downloads="[web app](https://github.com/$REPO/actions/runs/$RUN_ID/artifacts/$web_id)"
    fi
    if [ "$conclusion" = success ] || [ -n "$downloads" ]; then
      web_revision="$checkout_sha"
    fi
  fi
  if [ "$conclusion" = success ] && [ -n "$web_revision" ] && [[ "$event" = pull_request || "$event" = workflow_dispatch ]]; then
    version=""
    if [ "$event" = pull_request ]; then
      # Use Oddish's versioned tarball so moving the PR tag does not break the link.
      # Try the three newest candidates to bound registry lookups after reruns.
      candidates=$(jq -r --arg run "$RUN_ID" --arg sha "$web_revision" '
        [.artifacts[]
          | (.name | capture("^dcl-regenesislabs-bevy-explorer-web-(?<version>[0-9]+\\.[0-9]+\\.[0-9]+-"
              + $run + "\\.commit-(?<short>[0-9a-f]{7,40}))\\.tgz$")) as $package
          | select($sha | startswith($package.short)) | {version:$package.version, created_at}]
        | sort_by(.created_at) | reverse | .[:3][] | .version' "$ARTIFACTS_FILE" | tr -d '\r')
      while IFS= read -r candidate; do
        if preview_matches_checkout "$candidate" "$web_revision"; then
          version="$candidate"
          break
        fi
      done <<< "$candidates"
    fi
    if [ -z "$version" ]; then
      # A dispatch must target the PR tag. The tag also covers PR runs without
      # a usable package artifact.
      candidate=$(npm_preview_field "pr-$PR_NUMBER" version)
      if preview_matches_checkout "$candidate" "$web_revision"; then
        version="$candidate"
      fi
    fi
    if [ -n "$version" ]; then
      preview="[preview](https://cdn.decentraland.org/@dcl-regenesislabs/bevy-explorer-web/$version/index.html?position=0%2C-2&baseDomain=decentraland.zone)"
      downloads="${downloads:+$downloads · }$preview"
    fi
  fi
  rows="$rows| $(job_cell 'Web app' "$url") | $(result_cell "$conclusion" Built) | $(job_time "$secs") | ${downloads:-—} |"$'\n'
fi
# The package job follows the web job: it is skipped when the web app was not built.
IFS=$'\t' read -r conclusion url secs <<< "$(job_fields "$WEB_PACKAGE_JOB")" || true
if [ -n "${url:-}${conclusion:-}" ] && [ "$conclusion" != skipped ]; then
  state=$(fold_state "$state" "$conclusion")
  rows="$rows| $(job_cell 'Web package' "$url") | $(result_cell "$conclusion" Built) | $(job_time "$secs") | — |"$'\n'
fi
IFS=$'\t' read -r conclusion url secs <<< "$(job_fields "$HEADLESS_JOB")" || true
if [ -n "${url:-}${conclusion:-}" ]; then
  state=$(fold_state "$state" "$conclusion")
  downloads=""
  link=$(artifact_link 'bevy-headless-linux-x64-' 'npm package'); [ -n "$link" ] && downloads="$downloads · $link"
  link=$(artifact_link 'bevy-explorer-headless-linux-amd64-' 'engine'); [ -n "$link" ] && downloads="$downloads · $link"
  downloads="${downloads# · }"
  rows="$rows| $(job_cell 'Headless engine (Linux)' "$url") | $(result_cell "$conclusion" Built) | $(job_time "$secs") | ${downloads:-—} |"$'\n'
fi

runners=""
while IFS=$'\t' read -r name id; do
  [ -n "$name" ] || continue
  label=$(sed -E 's/^bevy-explorer-benchmark-([a-z]+-[a-z0-9]+)-[0-9a-f]{40}$/\1/' <<< "$name")
  runners="$runners · [$label](https://github.com/$REPO/actions/runs/$RUN_ID/artifacts/$id)"
done <<< "$(jq -r '[.artifacts[] | select(.expired == false and (.name | startswith("bevy-explorer-benchmark-")))]
  | sort_by(.name) | .[] | [.name, (.id | tostring)] | @tsv' "$ARTIFACTS_FILE" | tr -d '\r')"
runners="${runners# · }"

case "$state" in
  passed) badge="Build-Success!-3fb950"; headline="The explorer built." ;;
  failed) badge="Build-Failed!-ff0000"; headline="A build failed. Check the [run]($RUN_URL) to see what went wrong." ;;
  cancelled) badge="Build-Cancelled-lightgrey"; headline="The run was cancelled. Push a new commit or re-run the workflow to refresh this section." ;;
  *) badge="Build-Not%20selected-lightgrey"; headline="No explorer build was selected: the web and headless inputs are unchanged, so the consumers reuse main's builds." ;;
esac
{
  echo "[![Build](https://img.shields.io/badge/$badge?logo=webassembly&$BADGE_STYLE)]($RUN_URL)"
  echo
  echo "$headline"
  if [ -n "$rows" ]; then
    echo
    echo "| Product | Result | Job time | Download |"
    echo "| --- | --- | ---: | --- |"
    printf '%s' "$rows"
  fi
  echo
  if [ -n "$runners" ]; then
    echo "<sub>Benchmark runner: $runners</sub>"
    echo
  fi
  if [ -n "$web_revision" ]; then
    echo "<sub>$FOOTER · Web build \`${web_revision:0:7}\`</sub>"
  else
    echo "<sub>$FOOTER</sub>"
  fi
} > "$OUT_DIR/build.md"

# --- tests -----------------------------------------------------------------------------------
state=none
rows=""
while IFS= read -r name; do
  [ -n "$name" ] || continue
  IFS=$'\t' read -r conclusion url secs <<< "$(job_fields "$name")" || true
  state=$(fold_state "$state" "$conclusion")
  case "$name" in
    *windows*) display='Test Suite (Windows)' ;;
    *ubuntu*) display='Test Suite (Linux)' ;;
    *macos*) display='Test Suite (macOS)' ;;
    *) display="$name" ;;
  esac
  # A job name comes from the workflow file, but a pull request's own copy of it writes the name,
  # so it renders as text: no pipes, no backticks.
  display=$(tr -d '|`' <<< "$display")
  rows="$rows| $(job_cell "$display" "$url") | $(result_cell "$conclusion" Passed) | $(job_time "$secs") |"$'\n'
done <<< "$(job_names_with_prefix "$TEST_JOB_PREFIX")"
IFS=$'\t' read -r conclusion url secs <<< "$(job_fields "$HEADLESS_JOB")" || true
if [ -n "${url:-}${conclusion:-}" ]; then
  state=$(fold_state "$state" "$conclusion")
  rows="$rows| $(job_cell "$HEADLESS_JOB" "$url") | $(result_cell "$conclusion" Passed) | $(job_time "$secs") |"$'\n'
fi

# The newer runs of this workflow for the same head. One that has not ended reports the tests
# later; one that has ended reported them already, and this run must not replace its section.
waiting_url="" newer_ended=""
if [ "$state" = none ] && [ -s "${HEAD_RUNS_FILE:-}" ]; then
  IFS=$'\t' read -r waiting_url newer_ended <<< "$(jq -r --arg run "$RUN_ID" --arg head "$HEAD_SHA" --slurpfile this "$RUN_FILE" '
    ($this[0].workflow_id // null) as $workflow
    | [.workflow_runs[]? | select(.workflow_id == $workflow and .event == "pull_request"
        and .head_sha == $head and .id > ($run | tonumber))] as $newer
    | [($newer | map(select(.status != "completed")) | sort_by(.id) | last | .html_url // "-"),
       ($newer | any(.status == "completed") | tostring)] | @tsv' "$HEAD_RUNS_FILE" | tr -d '\r')" || true
  [ "$waiting_url" != - ] || waiting_url=""
fi
if [ -z "$waiting_url" ] && [ "$newer_ended" = true ]; then
  echo "A newer run of ${HEAD_SHA:0:7} has ended and reported the tests: run $RUN_ID, which ran no test job, writes no tests section."
  rm -f "$OUT_DIR/tests.md"
  exit 0
fi

case "$state" in
  passed) badge="Tests-Passed!-3fb950"; headline="All explorer tests passed ✅" ;;
  failed) badge="Tests-Failed!-ff0000"; headline="Some explorer tests failed ❌ Check the [run]($RUN_URL) for the failing tests." ;;
  cancelled) badge="Tests-Cancelled-lightgrey"; headline="The run was cancelled. Push a new commit or re-run the workflow to refresh this section." ;;
  *) if [ -n "$waiting_url" ]; then
       badge="Tests-Pending!-yellow"; headline="A newer [run]($waiting_url) of this commit has not ended. This section updates when that run reports."
     else
       badge="Tests-Not%20selected-lightgrey"; headline="No test job was selected: the test inputs are unchanged, or the pull request is a draft and the tests wait until it is ready for review."
     fi ;;
esac
{
  echo "[![Tests](https://img.shields.io/badge/$badge?logo=rust&$BADGE_STYLE)]($RUN_URL)"
  echo
  echo "$headline"
  if [ -n "$rows" ]; then
    echo
    echo "| Suite | Result | Job time |"
    echo "| --- | --- | ---: |"
    printf '%s' "$rows"
  fi
  echo
  echo "<sub>$FOOTER</sub>"
} > "$OUT_DIR/tests.md"
