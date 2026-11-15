#!/usr/bin/env bash
# Prints one section of the pull request's "Creator Hub test builds" comment, in the form of the
# Explorer's CI status sections: a badge that links the run, a table of what to get, and a footer
# with the run and the version.
#
# Environment: SECTION (desktop | inspector | asset-packs), RUN_ID, RUN_URL, HEAD_SHA, PR_NUMBER,
# BASE_URL (the bucket's base URL), VERSION, and
#   desktop:      BRANCH, MACOS and WINDOWS (`uploaded` when that build is in the bucket;
#                 anything else reads as a failed build), MACOS_SECONDS and WINDOWS_SECONDS
#   inspector:    PACKAGE_KEY, PREVIEW_PATH
#   asset-packs:  PACKAGE_KEY
set -euo pipefail

: "${SECTION:?SECTION is required}"
: "${RUN_ID:?RUN_ID is required}"
: "${RUN_URL:?RUN_URL is required}"
: "${HEAD_SHA:?HEAD_SHA is required}"
: "${PR_NUMBER:?PR_NUMBER is required}"
: "${BASE_URL:?BASE_URL is required}"
: "${VERSION:?VERSION is required}"

BADGE_STYLE='logoColor=white&style=for-the-badge'
# The version of a test build: the desktop workflow puts it in the installers' file names.
BUILD="$VERSION-$PR_NUMBER-${HEAD_SHA:0:7}"
FOOTER="<sub>Run summary: [$RUN_ID]($RUN_URL) · Version: \`$BUILD\`</sub>"

# badge LABEL STATE LOGO: STATE is passed or failed.
badge() {
  local label="${1// /%20}" text='Success!-3fb950'
  [ "$2" = passed ] || text='Failed!-ff0000'
  printf '[![%s](https://img.shields.io/badge/%s-%s?logo=%s&%s)](%s)\n\n' "$1" "$label" "$text" "$3" "$BADGE_STYLE" "$RUN_URL"
}

# installer_row PLATFORM RESULT SECONDS SUFFIX: one table row for the installer whose file name
# ends in SUFFIX.
installer_row() {
  if [ "$2" = uploaded ]; then
    printf '| %s | ✅ Built%s | [%s](%s/creator-hub/branch/%s/Decentraland%%20Creator%%20Hub-%s-%s) |\n' \
      "$1" "${3:+ in $3 seconds}" "$4" "$BASE_URL" "$BRANCH" "$BUILD" "$4"
  else
    printf '| %s | ❌ Failed | — |\n' "$1"
  fi
}

case "$SECTION" in
  desktop)
    : "${BRANCH:?BRANCH is required}"
    state=passed
    [ "${MACOS:-}" = uploaded ] && [ "${WINDOWS:-}" = uploaded ] || state=failed
    badge 'Desktop app' "$state" electron
    printf '| Platform | Result | Download |\n|---|---|---|\n'
    installer_row 'macOS (Apple silicon)' "${MACOS:-}" "${MACOS_SECONDS:-}" mac-arm64.zip
    installer_row 'macOS (Intel)' "${MACOS:-}" "${MACOS_SECONDS:-}" mac-x64.zip
    installer_row 'Windows' "${WINDOWS:-}" "${WINDOWS_SECONDS:-}" win-x64.exe
    if [ "${MACOS:-}" = uploaded ]; then
      cat <<'MD'

<details><summary>How to open the macOS app</summary>

The app is not signed.

1. Unzip the download.
2. Open a terminal in the folder that has the app.
3. Run `xattr -c "Decentraland Creator Hub.app"`.
4. Double-click the app.

To know which Mac you have, see [Mac computers with Apple silicon](https://support.apple.com/en-us/116943).

</details>
MD
    fi
    printf '\n%s\n' "$FOOTER"
    ;;
  inspector)
    : "${PACKAGE_KEY:?PACKAGE_KEY is required}"
    badge 'Inspector' passed npm
    printf '| Product | How to use it |\n|---|---|\n'
    if [ -n "${PREVIEW_PATH:-}" ]; then
      printf '| Preview | [Open in the browser](%s/%s) |\n' "$BASE_URL" "$PREVIEW_PATH"
    fi
    printf '| `@dcl/inspector` | `npm install "%s/%s"` |\n' "$BASE_URL" "$PACKAGE_KEY"
    printf '\n%s\n' "$FOOTER"
    ;;
  asset-packs)
    : "${PACKAGE_KEY:?PACKAGE_KEY is required}"
    badge 'Asset packs' passed npm
    printf '| Product | How to use it |\n|---|---|\n'
    printf '| `@dcl/asset-packs` | `npm install "%s/%s"` |\n' "$BASE_URL" "$PACKAGE_KEY"
    printf '\nAssets that this pull request adds are not on the CDN until it merges.\n'
    printf '\n%s\n' "$FOOTER"
    ;;
  *) echo "::error::Unknown section '$SECTION'." >&2; exit 2 ;;
esac
