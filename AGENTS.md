# Shift contributor guide

## Product contract

Shift converts a selected local file into a downloadable artifact. The native
app and `shift-cli` must expose the same format support and conversion behavior.
Never implement a converter directly inside a UI event handler or CLI parser.

## Architecture

- Put conversion contracts and dispatch in `src/conversion/mod.rs`.
- Put each engine or format family in its own module under `src/conversion/`.
- Implement `ConversionModule`, including explicit input/output capabilities,
  for new adapters and register them once in `ConversionRegistry::default`.
- Keep `src/main.rs` focused on GPUI state and presentation.
- Keep `src/bin/shift-cli.rs` focused on arguments, output selection, and exit
  behavior.
- Return `ConversionArtifact` values from modules. Callers decide when and where
  to write them.
- Conversion work can block and must run on GPUI's background executor.
- Preserve the selected source file; conversion and download must not mutate it.

## Format support

The MarkItDown adapter converts broad heterogeneous inputs to Markdown. Pandoc
handles publishing formats and overlaps on some document-to-Markdown paths.
Defuddle extracts clean article content from `http(s)` URLs and local HTML
(Markdown or HTML output). Docling reads PDFs and other documents with strong
layout awareness and exports Markdown, HTML, or plain text (enabling PDF →
HTML, which MarkItDown and Pandoc cannot do). Keep capability lists explicit so
unsupported pairs fail before an external process is launched. If a new module
overlaps an existing conversion pair, document and test the registry
precedence.

## Verification

Before handing off a change, run:

```sh
cargo fmt --check
cargo lint
cargo test --all-targets
```

Add unit tests for dispatch, output naming, and module-specific behavior. For
external converters, prefer deterministic fake executables in tests instead of
depending on network access or a developer's global installation.

## Working tree safety

This repository may contain in-progress visual work. Inspect `git diff` before
editing, preserve unrelated changes, and do not overwrite or revert user edits.
