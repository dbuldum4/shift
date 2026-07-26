# Distribution strategy

Shift uses a Homebrew cask as the primary install path.

The release ZIP contains `Shift.app`, `shift-cli`, the pinned Python packages
MarkItDown 0.1.6 and Docling 2.115.0, and the pinned Node package Defuddle
0.19.2. The cask installs the stable native runtimes and tools they require:
Python 3.11, Node, FFmpeg, Pandoc, Typst, and qpdf. This makes one Homebrew
command sufficient while avoiding first-launch package-manager mutations.

Docling model weights are intentionally not embedded because upstream selects
models for the active pipeline and downloads them on demand. They are cached by
Docling after first use.

The 0.1 release is ad-hoc signed and cannot be notarized without an Apple
Developer ID certificate. The release and website disclose this limitation.
