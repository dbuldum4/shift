#!/bin/sh
# Validate an already-created macOS release directory. Keep this separate from
# package-macos.sh so CI can test the release artifact after it is assembled.
set -eu

version="${1:?version required}"
output_dir="${2:-dist}"
arch="${3:-$(uname -m)}"
app="$output_dir/Shift.app"
archive="$output_dir/shift-${version}-macos-${arch}.zip"
dmg="$output_dir/shift-${version}-macos-${arch}.dmg"

fail() {
  printf '%s\n' "verify-macos-package: $*" >&2
  exit 1
}

[ -d "$app" ] || fail "missing app bundle: $app"
[ -x "$app/Contents/MacOS/shift" ] || fail "missing app executable"
[ -x "$app/Contents/Resources/bin/shift-cli" ] || fail "missing bundled CLI"
[ -f "$app/Contents/Resources/LICENSE" ] || fail "missing bundled LICENSE"
[ -f "$app/Contents/Resources/THIRD_PARTY_NOTICES.md" ] || fail "missing bundled third-party notices"
[ -f "$app/Contents/Info.plist" ] || fail "missing Info.plist"

bundle_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist" 2>/dev/null || true)"
[ "$bundle_version" = "$version" ] || fail "bundle version is $bundle_version, expected $version"

cli_version="$("$app/Contents/Resources/bin/shift-cli" --version)"
[ "$cli_version" = "shift-cli $version" ] || fail "bundled CLI reports '$cli_version', expected 'shift-cli $version'"

for artifact in "$archive" "$dmg"; do
  [ -f "$artifact" ] || fail "missing artifact: $artifact"
  [ -f "$artifact.sha256" ] || fail "missing checksum: $artifact.sha256"
  (cd "$(dirname "$artifact")" && shasum -a 256 -c "$(basename "$artifact").sha256") >/dev/null \
    || fail "checksum does not match: $artifact"
done

unzip -t "$archive" >/dev/null || fail "ZIP archive is corrupt: $archive"
unzip -Z1 "$archive" | grep -Fxq 'Shift.app/Contents/Info.plist' \
  || fail "ZIP archive does not contain Shift.app/Contents/Info.plist"
hdiutil verify "$dmg" >/dev/null || fail "DMG is corrupt: $dmg"

printf '%s\n' "macOS package validation passed for Shift $version ($arch)"
