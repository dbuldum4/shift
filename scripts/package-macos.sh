#!/bin/sh
set -eu

version="${1:?version required}"
output_dir="${2:-dist}"
arch="$(uname -m)"
app="$output_dir/Shift.app"
contents="$app/Contents"
resources="$contents/Resources"
runtime="$resources/runtime"

if ! printf '%s\n' "$version" | awk -F. '
  NF == 3 && $1 ~ /^[0-9]+$/ && $2 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ { ok = 1 }
  END { exit(ok ? 0 : 1) }
'; then
  echo "package-macos: version must be a numeric release version (for example 1.0.0)" >&2
  exit 2
fi

manifest_version="$(awk -F'\"' '/^version = \"/ { print $2; exit }' Cargo.toml)"
if [ "$version" != "$manifest_version" ]; then
  echo "package-macos: version $version does not match Cargo.toml ($manifest_version)" >&2
  exit 2
fi

if [ ! -x target/release/shift ] || [ ! -x target/release/shift-cli ]; then
  echo "package-macos: release binaries are missing; run cargo build --release --locked first" >&2
  exit 2
fi

mkdir -p "$output_dir"
rm -rf "$app"
mkdir -p "$contents/MacOS" "$resources/bin" "$runtime/bin" "$runtime/python" "$runtime/node"

cp target/release/shift "$contents/MacOS/shift"
cp target/release/shift-cli "$resources/bin/shift-cli"
cp LICENSE "$resources/LICENSE"
cp THIRD_PARTY_NOTICES.md "$resources/THIRD_PARTY_NOTICES.md"

cat > "$contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDisplayName</key><string>Shift</string>
  <key>CFBundleExecutable</key><string>shift</string>
  <key>CFBundleIdentifier</key><string>org.denizbuldum.shift</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Shift</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <!-- Bundle build number. For historical 0.x tags ${version#0.} strips a
       leading "0."; for 1.0.0+ the full semver is used as-is. -->
  <key>CFBundleVersion</key><string>${version#0.}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <!-- Finder advertises Shift as an alternate viewer, never the default. The
       executable also accepts one or many local paths for `open -a Shift …`.
       Keep this list in parity with conversion module input extensions
       (MarkItDown / Pandoc / Docling / FFmpeg / sips / spreadsheet / qpdf /
       Defuddle). scripts/test-release-preflight.sh asserts core extensions
       remain present; expand here when a module gains a mainstream input. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Shift convertible files</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>CFBundleTypeExtensions</key>
      <array>
        <string>pdf</string><string>txt</string><string>md</string><string>markdown</string>
        <string>html</string><string>htm</string><string>xhtml</string>
        <string>doc</string><string>docx</string><string>dot</string><string>dotx</string>
        <string>ppt</string><string>pptx</string><string>odt</string><string>ods</string>
        <string>odp</string><string>rtf</string><string>epub</string><string>tex</string>
        <string>latex</string><string>ipynb</string>
        <string>csv</string><string>tsv</string><string>json</string><string>xml</string>
        <string>zip</string>
        <string>xls</string><string>xlsx</string><string>xlsm</string><string>xlsb</string>
        <string>mp3</string><string>wav</string><string>flac</string><string>m4a</string>
        <string>aac</string><string>ogg</string><string>opus</string><string>ac3</string>
        <string>wma</string><string>aiff</string><string>aif</string><string>caf</string>
        <string>mp4</string><string>mov</string><string>mkv</string><string>webm</string>
        <string>avi</string><string>m4v</string><string>mpeg</string><string>mpg</string>
        <string>ts</string><string>m2ts</string><string>3gp</string>
        <string>png</string><string>jpg</string><string>jpeg</string>
        <string>gif</string><string>webp</string><string>heic</string><string>heif</string>
        <string>avif</string><string>svg</string><string>tiff</string><string>tif</string>
        <string>bmp</string><string>jxl</string><string>ico</string><string>jp2</string>
        <string>psd</string>
        <string>dng</string><string>cr2</string><string>cr3</string><string>nef</string>
        <string>arw</string><string>orf</string><string>raf</string><string>rw2</string>
        <string>srt</string><string>vtt</string>
      </array>
    </dict>
  </array>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

uv pip install --python 3.11 --prerelease=allow --target "$runtime/python" \
  "markitdown[all]==0.1.6" "docling[asr]==2.115.0" \
  "docling-slim[format-video]==2.115.0"
npm install --prefix "$runtime/node" --omit=dev --no-package-lock "defuddle@0.19.2"

cat > "$runtime/bin/markitdown" <<'EOF'
#!/bin/sh
set -eu
root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
python="${SHIFT_PYTHON_BIN:-}"
if [ -z "$python" ]; then
  for candidate in /opt/homebrew/opt/python@3.11/bin/python3.11 /usr/local/opt/python@3.11/bin/python3.11 python3.11; do
    if command -v "$candidate" >/dev/null 2>&1; then python="$candidate"; break; fi
  done
fi
PYTHONPATH="$root/python" exec "$python" -m markitdown "$@"
EOF

cat > "$runtime/bin/docling" <<'EOF'
#!/bin/sh
set -eu
root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
python="${SHIFT_PYTHON_BIN:-}"
if [ -z "$python" ]; then
  for candidate in /opt/homebrew/opt/python@3.11/bin/python3.11 /usr/local/opt/python@3.11/bin/python3.11 python3.11; do
    if command -v "$candidate" >/dev/null 2>&1; then python="$candidate"; break; fi
  done
fi
PYTHONPATH="$root/python" exec "$python" -c 'from docling.cli.main import app; app()' "$@"
EOF

cat > "$runtime/bin/defuddle" <<'EOF'
#!/bin/sh
set -eu
root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"

# Resolve Node for GUI apps with a minimal PATH (Homebrew, nvm, fnm, volta, asdf, mise).
resolve_node() {
  if [ -n "${SHIFT_NODE_BIN:-}" ] && [ -x "${SHIFT_NODE_BIN}" ]; then
    printf '%s\n' "${SHIFT_NODE_BIN}"
    return 0
  fi

  for candidate in /opt/homebrew/bin/node /usr/local/bin/node; do
    if [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  if command -v node >/dev/null 2>&1; then
    command -v node
    return 0
  fi

  home="${HOME:-}"
  if [ -n "$home" ]; then
    nvm_dir="${NVM_DIR:-$home/.nvm}"
    if [ -d "$nvm_dir/versions/node" ]; then
      latest="$(ls -1d "$nvm_dir"/versions/node/v* 2>/dev/null | sort -V | tail -n 1 || true)"
      if [ -n "$latest" ] && [ -x "$latest/bin/node" ]; then
        printf '%s\n' "$latest/bin/node"
        return 0
      fi
    fi

    for base in "${FNM_DIR:-}" "$home/.local/share/fnm" "$home/.fnm"; do
      [ -n "$base" ] || continue
      if [ -d "$base/node-versions" ]; then
        latest="$(ls -1d "$base"/node-versions/v* 2>/dev/null | sort -V | tail -n 1 || true)"
        if [ -n "$latest" ] && [ -x "$latest/installation/bin/node" ]; then
          printf '%s\n' "$latest/installation/bin/node"
          return 0
        fi
      fi
    done

    for candidate in \
      "$home/.volta/bin/node" \
      "$home/.asdf/shims/node" \
      "$home/.local/share/mise/shims/node" \
      "$home/.mise/shims/node" \
      "$home/.local/bin/node"; do
      if [ -x "$candidate" ]; then
        printf '%s\n' "$candidate"
        return 0
      fi
    done
  fi

  return 1
}

node="$(resolve_node)" || node=""
if [ -z "$node" ] || [ ! -x "$node" ]; then
  printf '%s\n' \
    "defuddle: Node.js not found. Install Node (for example: brew install node) or set SHIFT_NODE_BIN to an absolute path to node." >&2
  exit 127
fi
exec "$node" "$root/node/node_modules/defuddle/dist/cli.js" "$@"
EOF

chmod +x "$runtime/bin/"* "$contents/MacOS/shift" "$resources/bin/shift-cli"

# Verify every bundled launcher before publishing a large release artifact.
SHIFT_PYTHON_BIN="$(command -v python3.11)" "$runtime/bin/markitdown" --help >/dev/null
SHIFT_PYTHON_BIN="$(command -v python3.11)" "$runtime/bin/docling" --help >/dev/null
SHIFT_NODE_BIN="$(command -v node)" "$runtime/bin/defuddle" --help >/dev/null

# No Developer ID is available yet. Ad-hoc signing keeps the bundle internally
# consistent, but the release notes must disclose that Gatekeeper notarization
# is not available for 0.1.x.
codesign --force --deep --sign - "$app"

archive_name="shift-${version}-macos-${arch}.zip"
archive="$output_dir/$archive_name"
ditto -c -k --sequesterRsrc --keepParent "$app" "$archive"
# Checksums must record basenames only so `shasum -c` works after `cd dist`
# (and after users download just the artifact + sidecar into one folder).
# Writing `shasum path/to/file` embeds the path prefix and breaks verify.
(
  cd "$output_dir"
  shasum -a 256 "$archive_name" > "${archive_name}.sha256"
)

dmg_root="$(mktemp -d "${TMPDIR:-/tmp}/shift-dmg.XXXXXX")"
trap 'rm -rf "$dmg_root"' EXIT
cp -R "$app" "$dmg_root/Shift.app"
ln -s /Applications "$dmg_root/Applications"
dmg_name="shift-${version}-macos-${arch}.dmg"
dmg="$output_dir/$dmg_name"
hdiutil create \
  -volname "Shift ${version}" \
  -srcfolder "$dmg_root" \
  -ov \
  -format UDZO \
  "$dmg"
(
  cd "$output_dir"
  shasum -a 256 "$dmg_name" > "${dmg_name}.sha256"
)

printf '%s\n%s\n' "$archive" "$dmg"
