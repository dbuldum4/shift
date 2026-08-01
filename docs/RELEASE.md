# Release checklist

This checklist is for a tagged, public Shift release. It deliberately excludes
Developer ID signing and notarization; those are separate follow-up work and
must not be represented as complete until credentials and verification are in
place.

## Before tagging

1. Choose the release version and update the authoritative Cargo package
   version. Run `cargo check --locked` so `Cargo.lock` records the same version.
2. Update the landing-page package version, release URL, and download label.
3. Add `release-notes/<version>.md`, including runtime requirements and the
   current macOS security status.
4. Run `sh scripts/release-preflight.sh <version>`; it rejects version drift
   between the manifest, lockfile, landing page, workflow default, and release
   documentation.
5. Run the full project checks:

   ```sh
   cargo fmt --check
   cargo lint
   cargo test --all-targets
   sh scripts/test-release-preflight.sh
   ```

6. Review `THIRD_PARTY_NOTICES.md` whenever dependencies or release-managed
   runtime packages change. Confirm the package versions and their upstream
   licenses remain accurate.

## Release run

1. Create and push an annotated `v<version>` tag from the reviewed commit.
2. The macOS release workflow builds with `--locked`, runs the metadata gate,
   packages the application and architecture-specific managed dependency
   archives, and validates the app bundle, embedded CLI version, ZIP/DMG
   integrity, and SHA-256 sidecars before publishing.
3. Verify the GitHub Release contains the application ZIP, DMG, both managed
   dependency ZIPs, and a matching `.sha256` sidecar for every artifact; use
   the release notes as the public changelog.
4. On a clean macOS 13+ machine (Apple Silicon and Intel where available),
   verify the checksum, drag the app to Applications, run
   `shift-cli --version`, run `shift-cli doctor`, convert a representative local
   file, and confirm source preservation and output placement.

## Signing and notarization (not part of 1.1.1)

Signing and notarization require a Developer ID certificate, a secure credential
handling plan, CI keychain setup, `codesign` verification, and `spctl`/notary
acceptance checks. Do not claim the release is Developer ID signed or notarized
until those checks are in the release workflow and have passed.
