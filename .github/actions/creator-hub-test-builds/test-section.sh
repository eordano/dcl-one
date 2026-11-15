#!/usr/bin/env bash
# Tests for section.sh: no network, no repo. Run from anywhere:
#   bash .github/actions/creator-hub-test-builds/test-section.sh
set -euo pipefail

SECTION_SH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/section.sh"
export RUN_ID=7 RUN_URL=https://github.com/example/repo/actions/runs/7 HEAD_SHA=abcdef1234567890
export PR_NUMBER=42 BASE_URL=https://bucket.example

FAILED=0
fail() { echo "FAIL: $1"; FAILED=1; }
pass() { echo "ok: $1"; }
has() { grep -qF -- "$2" <<< "$1"; }

RUN='<sub>Run summary: [7](https://github.com/example/repo/actions/runs/7)'
FILES=https://bucket.example/creator-hub/branch/feat/x/Decentraland%20Creator%20Hub-1.2.3-42-abcdef1

# --- desktop: both platforms in one section ---------------------------------------------------
OUT=$(SECTION=desktop VERSION=1.2.3 BRANCH=feat/x MACOS=uploaded WINDOWS=uploaded MACOS_SECONDS=278.412 WINDOWS_SECONDS=626.090 bash "$SECTION_SH")
[ "$(head -n 1 <<< "$OUT")" = '[![Desktop app](https://img.shields.io/badge/Desktop%20app-Success!-3fb950?logo=electron&logoColor=white&style=for-the-badge)](https://github.com/example/repo/actions/runs/7)' ] \
  || fail "desktop: badge line"
has "$OUT" "| macOS (Apple silicon) | ✅ Built in 278.412 seconds | [mac-arm64.zip]($FILES-mac-arm64.zip) |" || fail "desktop: Apple silicon row"
has "$OUT" "| macOS (Intel) | ✅ Built in 278.412 seconds | [mac-x64.zip]($FILES-mac-x64.zip) |" || fail "desktop: Intel row"
has "$OUT" "| Windows | ✅ Built in 626.090 seconds | [win-x64.exe]($FILES-win-x64.exe) |" || fail "desktop: Windows row"
has "$OUT" '<details><summary>How to open the macOS app</summary>' || fail "desktop: macOS steps are not collapsed"
has "$OUT" 'xattr -c "Decentraland Creator Hub.app"' || fail "desktop: macOS steps"
[ "$(tail -n 1 <<< "$OUT")" = "$RUN"' · Version: `1.2.3-42-abcdef1`</sub>' ] || fail "desktop: footer"
pass "desktop section joins the platforms"

# --- desktop: one platform failed -------------------------------------------------------------
OUT=$(SECTION=desktop VERSION=1.2.3 BRANCH=feat/x MACOS='' WINDOWS=uploaded bash "$SECTION_SH")
has "$OUT" "| Windows | ✅ Built | [win-x64.exe]($FILES-win-x64.exe) |" || fail "desktop failed: a build with no time"
has "$OUT" 'Desktop%20app-Failed!-ff0000' || fail "desktop failed: badge"
has "$OUT" '| macOS (Apple silicon) | ❌ Failed | — |' || fail "desktop failed: Apple silicon row"
has "$OUT" '| macOS (Intel) | ❌ Failed | — |' || fail "desktop failed: Intel row"
has "$OUT" "[win-x64.exe]($FILES-win-x64.exe)" || fail "desktop failed: the Windows link is lost"
has "$OUT" 'xattr' && fail "desktop failed: macOS steps without a macOS build"
OUT=$(SECTION=desktop VERSION=1.2.3 BRANCH=feat/x bash "$SECTION_SH")
[ "$(grep -c '❌ Failed' <<< "$OUT")" = 3 ] || fail "desktop failed: no result must read as failed"
pass "desktop section shows the platform that failed"

# --- inspector --------------------------------------------------------------------------------
OUT=$(SECTION=inspector VERSION=1.0.0 PACKAGE_KEY=creator-hub/branch/x/dcl-inspector-1.0.0.tgz PREVIEW_PATH=creator-hub/branch/x/inspector/index.html bash "$SECTION_SH")
has "$OUT" 'Inspector-Success!-3fb950?logo=npm' || fail "inspector: badge"
has "$OUT" '(https://bucket.example/creator-hub/branch/x/inspector/index.html)' || fail "inspector: preview link"
has "$OUT" '`npm install "https://bucket.example/creator-hub/branch/x/dcl-inspector-1.0.0.tgz"`' || fail "inspector: install line"
has "$OUT" "$RUN"' · Version: `1.0.0-42-abcdef1`</sub>' || fail "inspector: footer"
OUT=$(SECTION=inspector VERSION=1.0.0 PACKAGE_KEY=k.tgz bash "$SECTION_SH")
has "$OUT" '| Preview |' && fail "inspector: preview row without a preview"
pass "inspector section"

# --- asset packs ------------------------------------------------------------------------------
OUT=$(SECTION=asset-packs VERSION=2.0.0 PACKAGE_KEY=creator-hub/branch/x/dcl-asset-packs-2.0.0.tgz bash "$SECTION_SH")
has "$OUT" 'Asset%20packs-Success!-3fb950?logo=npm' || fail "asset packs: badge"
has "$OUT" '`npm install "https://bucket.example/creator-hub/branch/x/dcl-asset-packs-2.0.0.tgz"`' || fail "asset packs: install line"
has "$OUT" 'are not on the CDN until it merges' || fail "asset packs: CDN note"
has "$OUT" "$RUN"' · Version: `2.0.0-42-abcdef1`</sub>' || fail "asset packs: footer"
pass "asset packs section"

# --- bad input --------------------------------------------------------------------------------
for bad in "SECTION=build VERSION=1.2.3" "SECTION=desktop VERSION=1.2.3" "SECTION=inspector VERSION=1.2.3" "SECTION=asset-packs PACKAGE_KEY=k.tgz"; do
  set +e
  # shellcheck disable=SC2086 # one word per assignment
  env $bad bash "$SECTION_SH" >/dev/null 2>&1
  RC=$?
  set -e
  [ "$RC" != 0 ] || fail "bad input: '$bad' passed"
done
pass "an unknown section or a missing value stops"

[ "$FAILED" = 0 ] && echo "ALL PASS" || { echo "FAILURES PRESENT"; exit 1; }
