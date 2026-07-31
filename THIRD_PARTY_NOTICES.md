# Third-party notices

Shift itself is distributed under the [MIT License](LICENSE).

Shift is a native application and CLI that combines in-process Rust crates with
optional or bundled converter runtimes. The authoritative Rust dependency graph
and exact versions are in [`Cargo.lock`](Cargo.lock). The macOS packaging script
also pins the following runtime packages:

- [Microsoft MarkItDown](https://github.com/microsoft/markitdown) `markitdown[all]==0.1.6`
- [Docling](https://github.com/docling-project/docling) `docling[asr]==2.115.0`
  on Apple Silicon; base `docling==2.115.0` on Intel
- [Docling Slim](https://github.com/docling-project/docling)
  `docling-slim[format-video]==2.115.0` on Apple Silicon; base
  `docling-slim==2.115.0` on Intel
- [Defuddle](https://github.com/kepano/defuddle) `defuddle@0.19.2`

Their licenses, notices, and transitive dependencies are governed by their
respective upstream distributions. The release process preserves this notice in
`Shift.app/Contents/Resources/THIRD_PARTY_NOTICES.md` beside Shift's MIT license.

System tools such as Python, Node.js, FFmpeg, Pandoc, Typst, qpdf, and macOS
`sips` are not relicensed by Shift; when installed or supplied by the user, they
remain subject to their own license terms. The same is true for optional Docling
model weights downloaded by Docling on first use.

For source redistribution or compliance review, use the exact revision's
`Cargo.lock`, the package pins in `scripts/package-macos.sh`, and the upstream
license files for the dependencies above. Update this document whenever either
set changes.
