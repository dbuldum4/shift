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

printf '%s\n' 'release preflight failure-path test passed'
