# Distribution strategy

Shift uses GitHub Releases as its distribution channel. Each macOS release
includes a drag-to-install DMG, a ZIP archive, and SHA-256 checksum files.

The release artifacts contain `Shift.app` and `shift-cli`; MarkItDown, Docling,
and Defuddle are not embedded in the application bundle. Onboarding offers an
optional verified dependency installation that keeps converter components under
`~/Library/Application Support/Shift/dependencies`. It neither changes the
user's shell nor invokes Homebrew, pip, or npm. Each release pins and verifies
its dependency archives before they are activated. Apple Silicon packs include
Docling ASR/video support; Intel packs retain document conversion and omit the
unsupported transcription routes.

Docling model weights are intentionally not embedded because upstream selects
models for the active pipeline and downloads them on demand. They are cached by
Docling after first use.

## Installing and upgrading

Release assets are architecture-specific macOS ZIP and DMG files plus matching
SHA-256 checksum files. Users should verify the checksum before installing, drag
`Shift.app` to Applications, and may add
`/Applications/Shift.app/Contents/Resources/bin` to `PATH` for `shift-cli`.

Upgrades replace the application bundle only. The app's preferences, history,
artifact cache, and non-secret session settings remain in
`~/Library/Application Support/Shift`; a release must preserve compatible
settings migrations.

## Finder open-with

The bundle declares Shift as an alternate Finder viewer for its supported input
extensions. It does not claim default ownership of those files. Finder's **Open
With → Shift** supports one or many files; the latter appears as the app's
normal batch queue. This metadata is local-only and does not require signing or
notarization to test during development.

## Security status

Shift 1.1.1 is not yet distributed with a Developer ID signature or Apple
notarization. This release process deliberately does not add signing work.
Users may need to use macOS's Control-click → Open flow. Signing and
notarization are explicit follow-up release work, not an implied guarantee of
this release.

## Removal

Moving `Shift.app` to the Trash removes the application. Deleting
`~/Library/Application Support/Shift` additionally removes local preferences,
history, session settings, and cached artifacts; it does not remove converted
files written outside that directory.
