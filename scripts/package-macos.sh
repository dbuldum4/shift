#!/bin/sh
set -eu

version="${1:?version required}"
output_dir="${2:-dist}"
arch="$(uname -m)"
app="$output_dir/Shift.app"
contents="$app/Contents"
resources="$contents/Resources"
runtime="$resources/runtime"

rm -rf "$app"
mkdir -p "$contents/MacOS" "$resources/bin" "$runtime/bin" "$runtime/python" "$runtime/node"

cp target/release/shift "$contents/MacOS/shift"
cp target/release/shift-cli "$resources/bin/shift-cli"
cp LICENSE "$resources/LICENSE"

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
  <key>CFBundleVersion</key><string>${version#0.}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

uv pip install --python 3.11 --prerelease=allow --target "$runtime/python" \
  "markitdown[all]==0.1.6" "docling==2.115.0"
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
node="${SHIFT_NODE_BIN:-}"
if [ -z "$node" ]; then
  for candidate in /opt/homebrew/bin/node /usr/local/bin/node node; do
    if command -v "$candidate" >/dev/null 2>&1; then node="$candidate"; break; fi
  done
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
# is not available for 0.1.0.
codesign --force --deep --sign - "$app"

archive="$output_dir/shift-${version}-macos-${arch}.zip"
ditto -c -k --sequesterRsrc --keepParent "$app" "$archive"
shasum -a 256 "$archive" > "$archive.sha256"

dmg_root="$(mktemp -d "${TMPDIR:-/tmp}/shift-dmg.XXXXXX")"
trap 'rm -rf "$dmg_root"' EXIT
cp -R "$app" "$dmg_root/Shift.app"
ln -s /Applications "$dmg_root/Applications"
dmg="$output_dir/shift-${version}-macos-${arch}.dmg"
hdiutil create \
  -volname "Shift ${version}" \
  -srcfolder "$dmg_root" \
  -ov \
  -format UDZO \
  "$dmg"
shasum -a 256 "$dmg" > "$dmg.sha256"

printf '%s\n%s\n' "$archive" "$dmg"
