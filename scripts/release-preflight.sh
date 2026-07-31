#!/bin/sh
# Validate the release facts that are intentionally duplicated across public
# surfaces. This runs before a release build, so a bad tag cannot produce an
# artifact that advertises a different version.
set -eu

root="${SHIFT_RELEASE_PREFLIGHT_ROOT:-$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)}"
version="${1:-}"

fail() {
  printf '%s\n' "release-preflight: $*" >&2
  exit 1
}

require_file() {
  [ -f "$1" ] || fail "missing required file: ${1#"$root"/}"
}

require_contains() {
  file="$1"
  needle="$2"
  grep -Fq "$needle" "$file" || fail "${file#"$root"/} must contain: $needle"
}

manifest="$root/Cargo.toml"
require_file "$manifest"
manifest_version="$(awk -F'"' '/^version = "/ { print $2; exit }' "$manifest")"
[ -n "$manifest_version" ] || fail "could not read package version from Cargo.toml"

if [ -z "$version" ]; then
  version="$manifest_version"
fi

if ! printf '%s\n' "$version" | awk -F. '
  NF == 3 && $1 ~ /^[0-9]+$/ && $2 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ { ok = 1 }
  END { exit(ok ? 0 : 1) }
'; then
  fail "version must be numeric semver (for example 1.0.0), got: $version"
fi

[ "$version" = "$manifest_version" ] || fail "requested $version but Cargo.toml is $manifest_version"

require_file "$root/Cargo.lock"
lock_version="$(awk '
  /^name = "shift"$/ { found=1; next }
  found && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "$root/Cargo.lock")"
[ "$lock_version" = "$version" ] || fail "Cargo.lock shift package is $lock_version, expected $version"

require_file "$root/release-notes/$version.md"
require_file "$root/THIRD_PARTY_NOTICES.md"
require_file "$root/docs/RELEASE.md"
require_file "$root/docs/DISTRIBUTION.md"
require_file "$root/landing-page/package.json"
require_file "$root/landing-page/src/App.jsx"
require_file "$root/.github/workflows/release.yml"

require_contains "$root/landing-page/package.json" "\"version\": \"$version\""
require_contains "$root/landing-page/src/App.jsx" "releases/tag/v$version"
require_contains "$root/landing-page/src/App.jsx" "download shift $version"
require_contains "$root/.github/workflows/release.yml" "default: v$version"
require_contains "$root/docs/RELEASE.md" "Signing and notarization"
require_contains "$root/docs/DISTRIBUTION.md" "not yet distributed with a Developer ID signature"
require_contains "$root/README.md" "shift-$version-macos-"
require_contains "$root/THIRD_PARTY_NOTICES.md" "docling[asr]=="
require_contains "$root/THIRD_PARTY_NOTICES.md" "docling-slim[format-video]=="
require_contains "$root/scripts/package-macos.sh" "docling[asr]=="
require_contains "$root/scripts/package-macos.sh" "docling-slim[format-video]=="
require_contains "$root/scripts/package-macos.sh" "docling==2.115.0"
require_contains "$root/scripts/package-macos.sh" "docling-slim==2.115.0"
require_contains "$root/scripts/package-macos.sh" "CFBundleDocumentTypes"

printf '%s\n' "release preflight passed for Shift $version"
