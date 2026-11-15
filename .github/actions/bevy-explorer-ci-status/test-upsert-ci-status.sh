#!/usr/bin/env bash
# Functional tests for upsert-ci-status.sh against a stubbed gh whose comment store is a JSON
# file: no network, no repo. Run from anywhere:
#   bash .github/actions/bevy-explorer-ci-status/test-upsert-ci-status.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UPSERT="$SCRIPT_DIR/upsert-ci-status.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- gh stub ---------------------------------------------------------------------------------
# Supports exactly the calls the script makes; comments live in $STORE as a JSON array of
# {id, user:{login}, body}.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
exec python3 "$GH_STUB_PY" "$@"
STUB
chmod +x "$WORK/bin/gh"
cat > "$WORK/gh-stub.py" <<'PY'
import json, os, sys

store = os.environ['STORE']

def load():
    with open(store) as f:
        return json.load(f)

def save(comments):
    with open(store, 'w') as f:
        json.dump(comments, f)

args = sys.argv[1:]
if args[0] != 'api':
    sys.exit(f'gh stub: unsupported subcommand {args[0]}')
args = args[1:]

method = 'GET'
if '-X' in args:
    i = args.index('-X')
    method = args[i + 1]
    del args[i:i + 2]
path = next(a for a in args if a.startswith('/'))

comments = load()
if method == 'GET' and os.environ.get('GH_FAIL_READ_EXIT'):
    sys.exit(int(os.environ['GH_FAIL_READ_EXIT']))
if method in ('POST', 'PATCH') and os.environ.get('GH_FAIL_WRITES_LOG'):
    with open(os.environ['GH_FAIL_WRITES_LOG'], 'a') as f:
        f.write(method + '\n')
    sys.stdin.read()
    sys.exit(42)
if method == 'GET':
    # Optional eventual-consistency simulation: while the STALE_READS_FILE counter is positive
    # and a pre-PATCH snapshot exists, serve the snapshot instead of the live store and
    # decrement the counter.
    stale_file = os.environ.get('STALE_READS_FILE')
    snapshot = store + '.prev'
    if stale_file and os.path.exists(stale_file) and os.path.exists(snapshot):
        remaining = int(open(stale_file).read().strip() or 0)
        if remaining > 0:
            with open(snapshot) as f:
                comments = json.load(f)
            with open(stale_file, 'w') as f:
                f.write(str(remaining - 1))
    # --paginate --slurp shape: an array of pages.
    print(json.dumps([comments]))
elif method == 'POST':
    body = json.load(sys.stdin)['body']
    new_id = max([c['id'] for c in comments], default=0) + 1
    comments.append({'id': new_id, 'user': {'login': 'github-actions[bot]'}, 'body': body})
    save(comments)
    print(json.dumps({'id': new_id}))
elif method == 'PATCH':
    cid = int(path.rsplit('/', 1)[1])
    body = json.load(sys.stdin)['body']
    if os.environ.get('STALE_READS_FILE'):
        # Snapshot the pre-PATCH store so stale GETs can serve it.
        with open(store + '.prev', 'w') as f:
            json.dump(comments, f)
    for c in comments:
        if c['id'] == cid:
            c['body'] = body
    save(comments)
    print(json.dumps({'id': cid}))
elif method == 'DELETE':
    cid = int(path.rsplit('/', 1)[1])
    save([c for c in comments if c['id'] != cid])
PY

export PATH="$WORK/bin:$PATH"
export GH_STUB_PY="$WORK/gh-stub.py"
export STORE="$WORK/comments.json"
export REPO="example/repo" PR_NUMBER="1" GITHUB_TOKEN="stub"

FAILED=0
fail() { echo "FAIL: $1"; FAILED=1; }
pass() { echo "ok: $1"; }

reset_store() { echo "${1:-[]}" > "$STORE"; }

body_of() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[int(sys.argv[2])]["body"])' "$STORE" "${1:-0}"; }
count() { python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$STORE"; }

run_upsert() { (cd "$WORK" && SECTION="$1" SECTION_BODY="$2" bash "$UPSERT"); }

# --- 1. create from skeleton -----------------------------------------------------------------
reset_store
run_upsert build "BUILD-CONTENT" >/dev/null
[ "$(count)" = 1 ] || fail "create: expected 1 comment"
BODY="$(body_of)"
grep -q 'BUILD-CONTENT' <<< "$BODY" || fail "create: build content missing"
grep -q '<!-- ci:tests:start -->' <<< "$BODY" || fail "create: tests fence missing"
grep -q '<!-- ci:performance:start -->' <<< "$BODY" || fail "create: performance fence missing"
grep -q 'measures it against main' <<< "$BODY" || fail "create: performance default missing"
grep -q '<!-- ci:automation:start -->' <<< "$BODY" && fail "create: on-demand automation fence in skeleton"
grep -q '<!-- ci:inworld:start -->' <<< "$BODY" && fail "create: on-demand inworld fence in skeleton"
grep -q '<!-- ci:lint:start -->' <<< "$BODY" && fail "create: a lint fence in skeleton"
grep -q '🚦 Bevy Explorer CI' <<< "$BODY" || fail "create: header missing"
pass "create seeds skeleton with all always-present sections"

# --- 2. section update preserves the others --------------------------------------------------
run_upsert tests "TESTS-CONTENT" >/dev/null
BODY="$(body_of)"
grep -q 'BUILD-CONTENT' <<< "$BODY" || fail "update: build content lost"
grep -q 'TESTS-CONTENT' <<< "$BODY" || fail "update: tests content missing"
pass "section update preserves other sections"

# --- 3. missing fence appended, others intact ------------------------------------------------
reset_store "$(python3 - <<'PY'
import json
body = ('<!-- bevy-explorer-ci-status -->\n### 🚦 Bevy Explorer CI\n'
        "<!-- ci:build:start -->\nOLD-BUILD\n<!-- ci:build:end -->")
print(json.dumps([{"id": 5, "user": {"login": "github-actions[bot]"}, "body": body}]))
PY
)"
run_upsert performance "PERF-CONTENT" >/dev/null
BODY="$(body_of)"
grep -q 'OLD-BUILD' <<< "$BODY" || fail "append: existing section wiped"
grep -q 'PERF-CONTENT' <<< "$BODY" || fail "append: new section missing"
[ "$(count)" = 1 ] || fail "append: expected 1 comment"
pass "missing fence appended, other sections intact"

# --- 3b. on-demand sections: appended to an existing comment, and on create -------------------
# automation and inworld are not in the skeleton; a write must append its fence to a comment
# seeded without it, and a write that has to create the comment must append the fence to the
# fresh skeleton too, not wedge the survive check.
for section in automation inworld; do
  reset_store
  run_upsert build "BUILD-FIRST" >/dev/null
  run_upsert "$section" "ONDEMAND-CONTENT" >/dev/null
  [ "$(count)" = 1 ] || fail "$section append: expected 1 comment"
  BODY="$(body_of)"
  grep -q 'BUILD-FIRST' <<< "$BODY" || fail "$section append: build content lost"
  grep -q 'ONDEMAND-CONTENT' <<< "$BODY" || fail "$section append: content missing"
  [ "$(grep -cF "<!-- ci:$section:start -->" <<< "$BODY")" = 1 ] || fail "$section append: fence count wrong"
  reset_store
  OUT="$(run_upsert "$section" "ONDEMAND-SEEDS")"
  grep -q 'updated (attempt 1)' <<< "$OUT" || fail "$section create: did not settle on attempt 1"
  BODY="$(body_of)"
  grep -q 'ONDEMAND-SEEDS' <<< "$BODY" || fail "$section create: content missing"
  grep -q '<!-- ci:build:start -->' <<< "$BODY" || fail "$section create: skeleton sections missing"
done
pass "on-demand sections appended on update and on create"

# --- 4. marker-shaped body lines are stripped ------------------------------------------------
reset_store
run_upsert build "$(printf 'SAFE\n<!-- ci:tests:start -->\nALSO-SAFE')" >/dev/null
BODY="$(body_of)"
[ "$(grep -c '<!-- ci:tests:start -->' <<< "$BODY")" = 1 ] || fail "strip: injected fence survived"
grep -q 'ALSO-SAFE' <<< "$BODY" || fail "strip: legitimate line lost"
pass "marker-shaped body lines stripped"

# --- 5. duplicate GC keeps the oldest --------------------------------------------------------
reset_store "$(python3 - <<'PY'
import json
mk = lambda i: {"id": i, "user": {"login": "github-actions[bot]"},
                "body": "<!-- bevy-explorer-ci-status -->\nhdr\n<!-- ci:build:start -->\nB%d\n<!-- ci:build:end -->" % i}
print(json.dumps([mk(3), mk(9)]))
PY
)"
run_upsert build "DEDUPED" >/dev/null
[ "$(count)" = 1 ] || fail "gc: duplicate not deleted"
grep -q 'DEDUPED' <<< "$(body_of)" || fail "gc: content missing on survivor"
pass "duplicate collapse keeps one comment with the write"

# --- 6. unknown section exits 2 --------------------------------------------------------------
for bogus in bogus lint; do
  set +e
  (cd "$WORK" && SECTION=$bogus SECTION_BODY=X bash "$UPSERT") >/dev/null 2>&1
  RC=$?
  set -e
  [ "$RC" = 2 ] || fail "allowlist: expected exit 2 for '$bogus', got $RC"
done
pass "unknown section exits 2"

# --- 6b. the Creator Hub's comment is a second comment with its own sections ------------------
reset_store
run_upsert build "EXPLORER-BUILD" >/dev/null
EXPLORER_BODY="$(body_of 0)"
COMMENT=creator-hub run_upsert inspector "INSPECTOR-CONTENT" >/dev/null
[ "$(count)" = 2 ] || fail "creator-hub: expected its own comment beside the Explorer's"
[ "$(body_of 0)" = "$EXPLORER_BODY" ] || fail "creator-hub: the Explorer's comment changed"
BODY="$(body_of 1)"
[ "$(head -n 1 <<< "$BODY")" = '<!-- creator-hub-test-builds -->' ] || fail "creator-hub: marker missing"
grep -q 'Creator Hub test builds' <<< "$BODY" || fail "creator-hub: header missing"
grep -q 'ci:desktop' <<< "$BODY" || fail "creator-hub: desktop default missing"
grep -q 'INSPECTOR-CONTENT' <<< "$BODY" || fail "creator-hub: inspector content missing"
grep -q '<!-- ci:asset-packs:start -->' <<< "$BODY" && fail "creator-hub: asset-packs fence in skeleton"
grep -q '<!-- ci:build:start -->' <<< "$BODY" && fail "creator-hub: an Explorer fence in skeleton"
COMMENT=creator-hub run_upsert asset-packs "ASSET-PACKS-CONTENT" >/dev/null
COMMENT=creator-hub run_upsert desktop "DESKTOP-CONTENT" >/dev/null
run_upsert tests "EXPLORER-TESTS" >/dev/null
[ "$(count)" = 2 ] || fail "creator-hub: later writes added a comment"
BODY="$(body_of 1)"
for want in INSPECTOR-CONTENT ASSET-PACKS-CONTENT DESKTOP-CONTENT; do
  grep -q "$want" <<< "$BODY" || fail "creator-hub: $want lost"
done
grep -q 'To build the desktop app' <<< "$BODY" && fail "creator-hub: desktop default left after a write"
grep -q 'EXPLORER-TESTS' <<< "$BODY" && fail "creator-hub: an Explorer write landed in it"
grep -q 'EXPLORER-TESTS' <<< "$(body_of 0)" || fail "creator-hub: the Explorer write missed its comment"
[ "$(grep -c '^<!-- ci:desktop:start -->$' <<< "$BODY")" = 1 ] || fail "creator-hub: duplicate desktop fence"
for bad in "creator-hub build" "bevy-explorer desktop" "bogus build"; do
  set -- $bad
  set +e
  (cd "$WORK" && COMMENT=$1 SECTION=$2 SECTION_BODY=X bash "$UPSERT") >/dev/null 2>&1
  RC=$?
  set -e
  [ "$RC" = 2 ] || fail "creator-hub: expected exit 2 for comment '$1' section '$2', got $RC"
done
COMMENT=creator-hub run_upsert desktop $'kept\n<!-- bevy-explorer-ci-status -->\n<!-- creator-hub-test-builds -->' >/dev/null
[ "$(count)" = 2 ] || fail "creator-hub: a marker in a body changed the comment count"
[ "$(grep -c 'creator-hub-test-builds\|bevy-explorer-ci-status' <<< "$(body_of 1)")" = 1 ] || fail "creator-hub: a marker line in a body was kept"
pass "the Creator Hub comment is separate, with its own sections"

# --- 7. oversized body from a file truncates and re-closes constructs ------------------------
reset_store
BIG="$WORK/big-body.md"
{
  echo '<details><summary>big</summary>'
  echo '```'
  for i in $(seq 1 3000); do echo "line $i of filler to overflow the cap"; done
} > "$BIG"
(cd "$WORK" && SECTION=tests SECTION_BODY= SECTION_BODY_FILE="$BIG" bash "$UPSERT") >/dev/null
BODY="$(body_of)"
grep -q 'truncated' <<< "$BODY" || fail "truncate: no truncation note"
[ "$(( $(grep -c '^```' <<< "$BODY") % 2 ))" = 0 ] || fail "truncate: unbalanced code fence"
SECTION_CONTENT="$(awk '/^<!-- ci:tests:start -->$/{grab=1;next} /^<!-- ci:tests:end -->$/{grab=0} grab' <<< "$BODY")"
[ "${#SECTION_CONTENT}" -le 12000 ] || fail "truncate: section is ${#SECTION_CONTENT} chars, closers re-inflated past the cap"
pass "oversized body truncated with constructs closed"

# --- 7b. a body file under the cap lands whole -----------------------------------------------
reset_store
REPORT="$WORK/report.md"
printf '## Benchmark of #1\n\n| Metric | Baseline |\n| --- | ---: |\n| CPU average | 1.0 ms |\n' > "$REPORT"
(cd "$WORK" && SECTION=performance SECTION_BODY= SECTION_BODY_FILE="$REPORT" bash "$UPSERT") >/dev/null
BODY="$(body_of)"
grep -q '| CPU average | 1.0 ms |' <<< "$BODY" || fail "body file: content missing"
grep -q 'truncated' <<< "$BODY" && fail "body file: truncated under the cap"
pass "a body file under the cap lands whole"

# --- 8. embedded own-section markers must not scramble the comment ---------------------------
# The wedge shape: the body smuggles in this section's own end marker (which would truncate the
# fence) and the top-level comment marker. The script strips such lines, so the write must settle
# on the first attempt with the structure intact (one marker, one end fence) and later writers
# must still land.
reset_store
OUT="$(run_upsert build "$(printf 'BEFORE\n<!-- ci:build:end -->\n  <!-- bevy-explorer-ci-status -->\nAFTER')")"
grep -q 'updated (attempt 1)' <<< "$OUT" || fail "wedge: write did not settle on attempt 1"
[ "$(count)" = 1 ] || fail "wedge: expected 1 comment"
BODY="$(body_of)"
grep -q 'BEFORE' <<< "$BODY" || fail "wedge: content before marker lost"
grep -q 'AFTER' <<< "$BODY" || fail "wedge: content after marker lost"
[ "$(grep -c '<!-- ci:build:end -->' <<< "$BODY")" = 1 ] || fail "wedge: embedded end marker survived"
[ "$(grep -c '<!-- bevy-explorer-ci-status -->' <<< "$BODY")" = 1 ] || fail "wedge: embedded comment marker survived"
run_upsert tests "TESTS-AFTER-WEDGE" >/dev/null
BODY="$(body_of)"
grep -q 'TESTS-AFTER-WEDGE' <<< "$BODY" || fail "wedge: later section write lost"
grep -q 'BEFORE' <<< "$BODY" || fail "wedge: later write wiped earlier section"
pass "embedded section markers stripped, comment structure intact"

# --- 9. stale re-read after PATCH retries and converges --------------------------------------
# Create-race shape: the PATCH lands, but the confirming re-read returns a stale body whose
# section content differs from WANT. The script must treat that as unsettled, retry, and
# converge once reads are fresh again.
reset_store "$(python3 - <<'PY'
import json
body = ("<!-- bevy-explorer-ci-status -->\n### 🚦 Bevy Explorer CI\n"
        "<!-- ci:build:start -->\nPRE-RACE\n<!-- ci:build:end -->")
print(json.dumps([{"id": 7, "user": {"login": "github-actions[bot]"}, "body": body}]))
PY
)"
echo 1 > "$WORK/stale-reads"
OUT="$( (cd "$WORK" && SECTION=build SECTION_BODY="RACE-CONVERGED" STALE_READS_FILE="$WORK/stale-reads" bash "$UPSERT") )"
grep -q "not settled (attempt 1)" <<< "$OUT" || fail "race: stale re-read did not trigger a retry"
grep -q 'updated (attempt 2)' <<< "$OUT" || fail "race: did not converge on attempt 2"
grep -q 'RACE-CONVERGED' <<< "$(body_of)" || fail "race: final body missing converged content"
[ "$(cat "$WORK/stale-reads")" = 0 ] || fail "race: stale read was not consumed"
pass "stale re-read retried until convergence"

# --- 10. CRLF body normalized in place, fences not duplicated --------------------------------
reset_store "$(python3 - <<'PY'
import json
body = ("<!-- bevy-explorer-ci-status -->\r\n### 🚦 Bevy Explorer CI\r\n"
        "<!-- ci:build:start -->\r\nOLD-BUILD\r\n<!-- ci:build:end -->")
print(json.dumps([{"id": 9, "user": {"login": "github-actions[bot]"}, "body": body}]))
PY
)"
run_upsert build "CRLF-BUILD" >/dev/null
[ "$(count)" = 1 ] || fail "crlf: expected 1 comment"
BODY="$(body_of)"
[ "$(grep -cF '<!-- ci:build:start -->' <<< "$BODY")" = 1 ] || fail "crlf: build fence duplicated"
grep -q 'CRLF-BUILD' <<< "$BODY" || fail "crlf: new content missing"
grep -q 'OLD-BUILD' <<< "$BODY" && fail "crlf: stale content still rendered"
grep -q $'\r' <<< "$BODY" && fail "crlf: body still carries CR"
pass "CRLF body replaced in place, no duplicate fences"

reset_store
code=0
(cd "$WORK" && SECTION=build SECTION_BODY=READ-FAILURE GH_FAIL_READ_EXIT=41 bash "$UPSERT") > "$WORK/read-failure.log" 2>&1 || code=$?
[ "$code" = 41 ] || fail "GitHub read failure was hidden as $code"
[ "$(count)" = 0 ] || fail 'read failure unexpectedly wrote a comment'
pass "unexpected GitHub read failures stop immediately"

cat > "$WORK/bin/sleep" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$1" >> "$GH_RETRY_SLEEP_LOG"
STUB
chmod +x "$WORK/bin/sleep"
export GH_RETRY_SLEEP_LOG="$WORK/retry-sleeps"
reset_store
GH_FAIL_WRITES_LOG="$WORK/failed-writes" run_upsert build WRITE-FAILURE > "$WORK/write-failure.log"
[ "$(wc -l < "$WORK/failed-writes" | tr -d ' ')" = 5 ] || fail 'write retry attempt budget changed'
[ "$(cat "$GH_RETRY_SLEEP_LOG")" = $'1\n2\n4\n8' ] || fail 'write retries changed or slept after the final attempt'
grep -q 'after 5 attempts' "$WORK/write-failure.log" || fail 'write exhaustion did not retain its warning'
pass "failed writes retain five attempts and omit the final wait"

[ "$FAILED" = 0 ] && echo "ALL PASS" || { echo "FAILURES PRESENT"; exit 1; }
