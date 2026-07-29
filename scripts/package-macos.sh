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
