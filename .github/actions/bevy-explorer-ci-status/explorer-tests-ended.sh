#!/usr/bin/env bash
# Answers one question for Bevy Explorer / CI status: have the test jobs of this Bevy Explorer / CI
# run ended while the run itself still goes on, so the tests section can be written now?
#
# The run ends only when its slowest job ends, and the web and image jobs often outlast the tests.
# The `requested` caller asks this after it wrote the pending sections; on `true` it writes the
# tests section from the run's current job list, and the `completed` caller later writes both
# sections as before.
#
# The test jobs are *Headless Smoke* and every *Test Suite* job. They have ended when all of them
# are listed and completed. The matrix of *Test Suite* is listed in one step, so one listed
# platform means all are listed.
#
# Prints `true` (the test jobs ended, one of them ran, and the run has not ended) or `false` on
# stdout. `false` covers: every test job skipped (a draft, or unchanged test inputs), the run
# ended first (cancelled, or a failed guard), and the wait budget ran out. In each case the
# `completed` caller reports, so the pull request loses only the earlier result.
#
# Usage: RUN_ID=<id> REPO=owner/name [WAIT_SECONDS=0] [POLL_SECONDS=30] bash explorer-tests-ended.sh
#
# WAIT_SECONDS is the wait budget; 0 (the default) answers from the run's current state.
# POLL_SECONDS is the longest pause between two reads; the pauses start at 5 seconds and double.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/retry.sh"

: "${RUN_ID:?RUN_ID is required}"
: "${REPO:?REPO is required}"
WAIT_SECONDS="${WAIT_SECONDS:-0}"
POLL_SECONDS="${POLL_SECONDS:-30}"

# The same names as the tests section of explorer-ci-sections.sh.
HEADLESS_JOB='Headless Smoke'
TEST_JOB_PATTERN='^Test Suite( \(.*\))?$'

log() { echo "[explorer-tests-ended] $*" >&2; }

run_status() { gh api "repos/$REPO/actions/runs/$RUN_ID" --jq '.status'; }

deadline=$(( $(date +%s) + WAIT_SECONDS ))
attempt=1

while :; do
  jobs_json=$(gh api "repos/$REPO/actions/runs/$RUN_ID/jobs?per_page=100")

  # listed: Headless Smoke and one Test Suite job are in the list. open: test jobs not completed.
  # ran: test jobs that completed and were not skipped.
  IFS=$'\t' read -r listed open ran <<< "$(jq -r --arg smoke "$HEADLESS_JOB" --arg re "$TEST_JOB_PATTERN" '
    [.jobs[] | select(.name == $smoke or (.name | test($re)))] as $tests
    | [(($tests | any(.name == $smoke)) and ($tests | any(.name | test($re))) | tostring),
       ($tests | map(select(.status != "completed")) | length),
       ($tests | map(select(.status == "completed" and .conclusion != "skipped")) | length)] | @tsv' <<< "$jobs_json" | tr -d '\r')"
  log "test jobs: listed=$listed open=$open ran=$ran"

  if [ "$listed" = true ] && [ "$open" -eq 0 ]; then
    if [ "$ran" -eq 0 ]; then
      log "every test job is skipped: the completed run reports"
      echo false
      exit 0
    fi
    if [ "$(run_status)" = completed ]; then
      log "the run ended with its tests: the completed run reports"
      echo false
      exit 0
    fi
    log "the test jobs ended and the run goes on"
    echo true
    exit 0
  fi

  # A run that ended (cancelled, or a failed guard) starts no more jobs. While the test jobs are
  # listed and one is open the run cannot have ended, so this read is only for an incomplete list.
  if [ "$listed" != true ] && [ "$(run_status)" = completed ]; then
    log "the run ended before its test jobs were listed: the completed run reports"
    echo false
    exit 0
  fi

  now=$(date +%s)
  if [ "$now" -ge "$deadline" ]; then
    log "the test jobs have not ended after ${WAIT_SECONDS}s: the completed run reports"
    echo false
    exit 0
  fi

  remaining=$((deadline - now))
  delay=$(backoff_delay "$attempt" "$POLL_SECONDS" 5)
  sleep_for "$delay" "$remaining"
  attempt=$((attempt + 1))
done
