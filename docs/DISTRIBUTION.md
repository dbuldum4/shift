# Distribution strategy

Shift uses GitHub Releases as its distribution channel. Each macOS release
includes a drag-to-install DMG, a ZIP archive, and SHA-256 checksum files.

The release artifacts contain `Shift.app`, `shift-cli`, the pinned Python packages
MarkItDown 0.1.6 and Docling 2.115.0, and the pinned Node package Defuddle
0.19.2. The native runtimes and tools they require—Python 3.11, Node, FFmpeg,
Pandoc, Typst, and qpdf—remain explicit system requirements. Shift never
installs packages or mutates the host system at first launch.

Docling model weights are intentionally not embedded because upstream selects
models for the active pipeline and downloads them on demand. They are cached by
Docling after first use.

The 0.1.x releases are ad-hoc signed and cannot be notarized without an Apple
Developer ID certificate. The release and website disclose this limitation.
