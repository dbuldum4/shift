#!/bin/sh
# Small fixture test for both the happy path and a deliberately mismatched
# release surface. It is dependency-free and suitable for the normal CI job.
set -eu

root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
version="$(awk -F'"' '/^version = "/ { print $2; exit }' "$root/Cargo.toml")"
"$root/scripts/release-preflight.sh" "$version"

fixture="$(mktemp -d "${TMPDIR:-/tmp}/shift-release-preflight.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/release-notes" "$fixture/landing-page/src" "$fixture/.github/workflows" "$fixture/docs" "$fixture/scripts"
cp "$root/Cargo.toml" "$fixture/Cargo.toml"
cp "$root/Cargo.lock" "$fixture/Cargo.lock"
cp "$root/README.md" "$fixture/README.md"
cp "$root/release-notes/$version.md" "$fixture/release-notes/$version.md"
cp "$root/THIRD_PARTY_NOTICES.md" "$fixture/THIRD_PARTY_NOTICES.md"
cp "$root/docs/RELEASE.md" "$fixture/docs/RELEASE.md"
cp "$root/docs/DISTRIBUTION.md" "$fixture/docs/DISTRIBUTION.md"
cp "$root/landing-page/package.json" "$fixture/landing-page/package.json"
cp "$root/landing-page/src/App.jsx" "$fixture/landing-page/src/App.jsx"
cp "$root/.github/workflows/release.yml" "$fixture/.github/workflows/release.yml"
cp "$root/scripts/package-macos.sh" "$fixture/scripts/package-macos.sh"

SHIFT_RELEASE_PREFLIGHT_ROOT="$fixture" "$root/scripts/release-preflight.sh" "$version"

# The release tag must never be allowed to disagree with Cargo metadata.
rm "$fixture/release-notes/$version.md"
if SHIFT_RELEASE_PREFLIGHT_ROOT="$fixture" "$root/scripts/release-preflight.sh" "$version" >/dev/null 2>&1; then
  echo "release-preflight failure-path test unexpectedly passed" >&2
  exit 1
fi

if (cd "$root" && scripts/package-macos.sh 1.0 >/dev/null 2>&1); then
  echo "package-macos accepted a non-semver version" >&2
  exit 1
fi
if (cd "$root" && scripts/package-macos.sh 999.0.0 >/dev/null 2>&1); then
  echo "package-macos accepted a version that disagrees with Cargo.toml" >&2
  exit 1
fi
if "$root/scripts/verify-macos-package.sh" "$version" "$fixture/missing-dist" >/dev/null 2>&1; then
  echo "verify-macos-package accepted a missing package" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Finding 4 / 18: checksum sidecars must use basenames only (cd into dist).
# ---------------------------------------------------------------------------
checksum_fixture="$(mktemp -d "${TMPDIR:-/tmp}/shift-checksum.XXXXXX")"
trap 'rm -rf "$fixture" "$checksum_fixture"' EXIT
mkdir -p "$checksum_fixture/dist"
artifact_name="shift-${version}-macos-arm64.zip"
printf 'payload\n' > "$checksum_fixture/dist/$artifact_name"
# Mirror package-macos.sh: hash from inside dist so the sidecar is portable.
(
  cd "$checksum_fixture/dist"
  shasum -a 256 "$artifact_name" > "${artifact_name}.sha256"
)
checksum_line="$(tr -d '\r' < "$checksum_fixture/dist/${artifact_name}.sha256" | head -n 1)"
printf '%s\n' "$checksum_line" | grep -Eq "^[0-9a-fA-F]{64} [ *]${artifact_name}\$" \
  || {
    echo "basename checksum write produced non-portable line: $checksum_line" >&2
    exit 1
  }
# Path-prefixed sidecars must fail the verify allow-list even if the hash matches.
bad_line="$(printf '%s  %s/%s\n' "$(printf '%s' "$checksum_line" | awk '{print $1}')" "$checksum_fixture/dist" "$artifact_name")"
printf '%s\n' "$bad_line" > "$checksum_fixture/dist/${artifact_name}.sha256"
# Build a minimal fake bundle so verify reaches the checksum check.
mkdir -p "$checksum_fixture/dist/Shift.app/Contents/MacOS" \
  "$checksum_fixture/dist/Shift.app/Contents/Resources/bin"
printf '#!/bin/sh\necho shift-cli %s\n' "$version" \
  > "$checksum_fixture/dist/Shift.app/Contents/Resources/bin/shift-cli"
chmod +x "$checksum_fixture/dist/Shift.app/Contents/Resources/bin/shift-cli"
cp "$checksum_fixture/dist/Shift.app/Contents/Resources/bin/shift-cli" \
  "$checksum_fixture/dist/Shift.app/Contents/MacOS/shift"
chmod +x "$checksum_fixture/dist/Shift.app/Contents/MacOS/shift"
printf 'license\n' > "$checksum_fixture/dist/Shift.app/Contents/Resources/LICENSE"
printf 'notices\n' > "$checksum_fixture/dist/Shift.app/Contents/Resources/THIRD_PARTY_NOTICES.md"
# Minimal Info.plist with matching version (PlistBuddy-readable).
cat > "$checksum_fixture/dist/Shift.app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleShortVersionString</key><string>$version</string>
</dict></plist>
EOF
# DMG/ZIP integrity is checked after checksums; provide dummy siblings that
# will not be reached if basename validation fails first.
: > "$checksum_fixture/dist/shift-${version}-macos-arm64.dmg"
: > "$checksum_fixture/dist/shift-${version}-macos-arm64.dmg.sha256"
path_prefixed_out="$(
  "$root/scripts/verify-macos-package.sh" "$version" "$checksum_fixture/dist" arm64 2>&1 || true
)"
printf '%s\n' "$path_prefixed_out" | grep -Fq "basename only" \
  || {
    echo "verify-macos-package should reject path-prefixed checksums; got: $path_prefixed_out" >&2
    exit 1
  }

# ---------------------------------------------------------------------------
# Finding 3 / 20: workflow_dispatch tag allow-list (env only, no shell inject).
# Must match .github/workflows/release.yml resolve-tag step.
# ---------------------------------------------------------------------------
tag_re='^v?[0-9]+\.[0-9]+\.[0-9]+$'
for good in v1.0.0 1.0.0 v0.3.0; do
  printf '%s' "$good" | grep -Eq "$tag_re" \
    || {
      echo "release tag allow-list unexpectedly rejected: $good" >&2
      exit 1
    }
done
for bad in '' 'v1' '1.0' '../main' 'v1.0.0;curl evil' 'v1.0.0$(id)' 'main' 'v1.0.0/../../../etc/passwd' 'v2.0.0_beta.1' 'v1.2.3-rc.1' '1.2.3+build.7'; do
  if printf '%s' "$bad" | grep -Eq "$tag_re"; then
    echo "release tag allow-list unexpectedly accepted: $bad" >&2
    exit 1
  fi
done
# Confirm the release workflow never interpolates inputs.tag into a run script
# body (must only appear in env: mappings / job outputs).
if awk '
  /^[[:space:]]*run:[[:space:]]*\|/ { in_run=1; next }
  in_run && /^[[:space:]]*[^#[:space:]]/ && $0 !~ /^[[:space:]]{2,}/ { in_run=0 }
  in_run && /\$\{\{[^}]*inputs\.tag/ { found=1 }
  END { exit found ? 0 : 1 }
' "$root/.github/workflows/release.yml"; then
  echo "release.yml interpolates inputs.tag into a run script; use RELEASE_TAG env instead" >&2
  exit 1
fi
# Multi-arch matrix must cover the latest GA arm64 and x86_64 images.
require_workflow_contains() {
  grep -Fq "$1" "$root/.github/workflows/release.yml" \
    || {
      echo "release.yml missing required multi-arch/hardening content: $1" >&2
      exit 1
    }
}
require_workflow_contains "macos-26"
require_workflow_contains "macos-26-intel"
require_workflow_contains "arch: arm64"
require_workflow_contains "arch: x86_64"
require_workflow_contains 'MACOSX_DEPLOYMENT_TARGET: "13.0"'
require_workflow_contains "RELEASE_TAG"
require_workflow_contains "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"
require_workflow_contains "actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020"
require_workflow_contains "astral-sh/setup-uv@d0cc045d04ccac9d8b7881df0226f9e82c39688e"

# CI must pin checkout and avoid floating bun latest.
grep -Fq "actions/checkout@11d5960a326750d5838078e36cf38b85af677262" "$root/.github/workflows/ci.yml" \
  || {
    echo "ci.yml must pin actions/checkout to a full commit SHA" >&2
    exit 1
  }
if grep -E 'bun-version:\s*latest' "$root/.github/workflows/ci.yml"; then
  echo "ci.yml must not float bun-version on latest" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Finding 57: Finder document associations cover core conversion extensions.
# ---------------------------------------------------------------------------
for ext in pdf docx pptx xlsx html md csv mp4 mp3 png heic heif avif svg jxl \
  json xml zip odt epub opus srt vtt xlsm xlsb ods; do
  grep -Fq "<string>$ext</string>" "$root/scripts/package-macos.sh" \
    || {
      echo "package-macos Info.plist missing Finder association for .$ext" >&2
      exit 1
    }
done
grep -Fq "CFBundleDocumentTypes" "$root/scripts/package-macos.sh" \
  || {
    echo "package-macos must declare CFBundleDocumentTypes" >&2
    exit 1
  }

# package-macos checksum write must cd into output_dir (basename-only).
grep -Fq 'shasum -a 256 "$archive_name"' "$root/scripts/package-macos.sh" \
  || {
    echo "package-macos must hash archive basenames after cd into output_dir" >&2
    exit 1
  }
grep -Fq 'shasum -a 256 "$dmg_name"' "$root/scripts/package-macos.sh" \
  || {
    echo "package-macos must hash dmg basenames after cd into output_dir" >&2
    exit 1
  }
# Reject the old path-prefixed pattern if it reappears.
if grep -nE 'shasum -a 256 "\$archive"' "$root/scripts/package-macos.sh" \
  || grep -nE 'shasum -a 256 "\$dmg"' "$root/scripts/package-macos.sh"; then
  echo "package-macos still writes path-prefixed checksums" >&2
  exit 1
fi

# The PyPI typing backport must not shadow Python 3.11's standard-library
# typing module inside the bundled target directory.
grep -Fq 'typing_backport="$runtime/python/typing.py"' "$root/scripts/package-macos.sh" \
  || {
    echo "package-macos must remove the Python typing backport" >&2
    exit 1
  }
grep -Fq 'typing-*.dist-info' "$root/scripts/package-macos.sh" \
  || {
    echo "package-macos must remove typing backport metadata" >&2
    exit 1
  }

printf '%s\n' 'release preflight failure-path test passed'
