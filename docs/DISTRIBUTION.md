# Distribution strategy

Shift uses GitHub Releases as its distribution channel. Each macOS release
includes a drag-to-install DMG, a ZIP archive, and SHA-256 checksum files.

The release artifacts contain `Shift.app`, `shift-cli`, the pinned Python
packages MarkItDown 0.1.6 and Docling 2.115.0, and the pinned Node package
Defuddle 0.19.2. Apple Silicon artifacts include Docling's ASR/video extras
(`docling[asr]` and `docling-slim[format-video]`). Intel artifacts include base
Docling document support without transcription because PyTorch 2.8+ has no
macOS Intel wheels; Shift removes those unsupported pairs from the Intel
capability lists. The native runtimes and tools they require—Python 3.11, Node,
FFmpeg, Pandoc, Typst, and qpdf—remain explicit system requirements. Shift never
installs packages or mutates the host system at first launch.

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
