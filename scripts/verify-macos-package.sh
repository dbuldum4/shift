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
[ -f "$app/Contents/Resources/dependency-manifest.json" ] || fail "missing dependency manifest"
[ ! -e "$app/Contents/Resources/runtime" ] || fail "converter runtime must not be bundled in Shift.app"

bundle_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist" 2>/dev/null || true)"
[ "$bundle_version" = "$version" ] || fail "bundle version is $bundle_version, expected $version"

cli_version="$("$app/Contents/Resources/bin/shift-cli" --version)"
[ "$cli_version" = "shift-cli $version" ] || fail "bundled CLI reports '$cli_version', expected 'shift-cli $version'"

for artifact in "$archive" "$dmg"; do
  [ -f "$artifact" ] || fail "missing artifact: $artifact"
  [ -f "$artifact.sha256" ] || fail "missing checksum: $artifact.sha256"
  # Sidecars must name basenames only (no directory prefix). package-macos.sh
  # writes them with `(cd dist && shasum file > file.sha256)` for this reason.
  # Accept "HASH  basename" or "HASH *basename" (binary mode); reject path prefixes.
  base="$(basename "$artifact")"
  checksum_line="$(tr -d '\r' < "$artifact.sha256" | head -n 1)"
  printf '%s\n' "$checksum_line" | grep -Eq "^[0-9a-fA-F]{64} [ *]${base}\$" \
    || fail "checksum must be 'HASH  $base' with basename only (got: $checksum_line)"
  (cd "$(dirname "$artifact")" && shasum -a 256 -c "$base.sha256") >/dev/null \
    || fail "checksum does not match: $artifact"
done

for component in documents-markdown web-extraction; do
  dependency="$output_dir/shift-dependencies-${version}-macos-${arch}-${component}.zip"
  [ -f "$dependency" ] || fail "missing dependency component: $dependency"
  [ -f "$dependency.sha256" ] || fail "missing dependency checksum: $dependency.sha256"
  (cd "$output_dir" && shasum -a 256 -c "$(basename "$dependency").sha256") >/dev/null \
    || fail "dependency checksum does not match: $dependency"
  grep -Fq "$(basename "$dependency")" "$app/Contents/Resources/dependency-manifest.json" \
    || fail "dependency manifest does not reference: $dependency"
done

# Check the runtime actually shipped in the dependency archive, not the host.
verification_dir="$(mktemp -d "${TMPDIR:-/tmp}/shift-node-verification.XXXXXX")"
trap 'rm -rf "$verification_dir"' EXIT
web_archive="$output_dir/shift-dependencies-${version}-macos-${arch}-web-extraction.zip"
unzip -q "$web_archive" -d "$verification_dir"
"$verification_dir/node-runtime/bin/node" "$(dirname "$0")/check-node-runtime.cjs" \
  || fail "web-extraction archive contains an unpatched or unusable Node runtime"
"$verification_dir/bin/defuddle" --help >/dev/null \
  || fail "packaged Defuddle launcher failed"
printf '<html><head><title>Runtime check</title></head><body><article><h1>Runtime check</h1><p>Local conversion remains available.</p></article></body></html>\n' > "$verification_dir/control.html"
"$verification_dir/bin/defuddle" parse "$verification_dir/control.html" --markdown > "$verification_dir/control.md"
grep -Fq 'Local conversion remains available.' "$verification_dir/control.md" \
  || fail "packaged Defuddle failed the local HTML conversion control"

unzip -t "$archive" >/dev/null || fail "ZIP archive is corrupt: $archive"
unzip -Z1 "$archive" | grep -Fxq 'Shift.app/Contents/Info.plist' \
  || fail "ZIP archive does not contain Shift.app/Contents/Info.plist"
hdiutil verify "$dmg" >/dev/null || fail "DMG is corrupt: $dmg"

printf '%s\n' "macOS package validation passed for Shift $version ($arch)"
