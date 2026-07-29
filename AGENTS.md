# Shift contributor guide

## Product contract

Shift converts a selected local file into a downloadable artifact. The native
app and `shift-cli` must expose the same format support and conversion behavior.
Never implement a converter directly inside a UI event handler or CLI parser.

Multi-file work uses the shared batch queue in `src/conversion/batch.rs`
(`BatchQueue` + `run_batch`). Folder expansion uses `expand_input_paths` (shared
by app and CLI). The app and CLI must not reimplement queue ordering, destination
resolution, retry, cancellation, or recursive source discovery.

## Architecture

- Put conversion contracts and dispatch in `src/conversion/mod.rs`.
- Put each engine or format family in its own module under `src/conversion/`.
- Put multi-file orchestration in `src/conversion/batch.rs` and call `run_batch`
  from both surfaces.
- Implement `ConversionModule`, including explicit input/output capabilities,
  for new adapters and register them once in `ConversionRegistry::default`.
- Keep `src/main.rs` focused on GPUI state and presentation.
- Keep `src/bin/shift-cli.rs` focused on arguments, output selection, and exit
  behavior.
- Return `ConversionArtifact` values from modules (with `pipeline` + redacted
  `invocations` for provenance). Callers decide when and where to write them
  (single-file) or use the batch runner (multi-file).
- Optional `ConversionOptions.progress` (`ProgressSink`) reports phase/fraction;
  FFmpeg may emit determinate progress, other engines stay indeterminate.
- PDF page range is preprocessed with `qpdf` in shared dispatch
  (`pdf_slice.rs`) before non-qpdf modules run. The qpdf adapter handles
  PDF-native rewrites directly so extraction and toolkit options use one process.
- Session UI knobs (except secrets) persist in `session_settings.rs` under
  Application Support; CLI never loads that file. Module priority stays in
  `preferences.rs`.
- Artifact cache for Reveal/Copy of in-memory binaries: `artifact_cache.rs`.
- Conversion work can block and must run on GPUI's background executor.
- Preserve the selected source file; conversion and download must not mutate it.

## Format support

The MarkItDown adapter converts broad heterogeneous inputs to Markdown. Pandoc
handles publishing formats and overlaps on some document-to-Markdown paths.
Defuddle extracts clean article content from `http(s)` URLs and local HTML
(Markdown or HTML output). Docling reads PDFs and other documents with strong
layout awareness and exports Markdown, HTML, plain text, or JSON (enabling
PDF → HTML, which MarkItDown and Pandoc cannot do). With pinned
`docling[asr]==2.115.0` plus `docling-slim[format-video]==2.115.0`, it also
transcribes WAV/MP3/M4A/AAC/OGG/FLAC and MP4/AVI/MOV/MKV/WebM locally via the
dedicated `transcript` output (Markdown payload, timed-media-only). Docling
must not own video → Markdown or video → VTT; FFmpeg keeps subtitle-track
extraction (SRT/VTT) and MarkItDown keeps document Markdown routes.
FFmpeg converts audio and video
containers, still frames, subtitle tracks, and PNG sequence ZIPs (for example
MP4 → MP3, WAV → FLAC, video → PNG, MKV → SRT, video → `png-sequence-zip`).
The sips adapter (macOS only, `/usr/bin/sips`, no install step) owns still-image
conversion: it reads the families no other engine does — HEIC/HEIF, AVIF, SVG,
JPEG XL, and camera RAW — and writes PNG, JPG, TIFF, GIF, BMP, JP2, ICNS, and
PDF (image → PDF is reachable only here). HEIC, AVIF, WEBP, and ICO decode but
do not have reliable sips CLI encoders, so they are inputs only; WEBP output
stays with FFmpeg. sips is registered ahead of FFmpeg and therefore wins still → still
pairs, while FFmpeg keeps everything that starts from a container (frame
extraction, `png-sequence-zip`). Off macOS the module is not registered at all,
so its formats are absent from capability lists rather than failing at spawn.
The spreadsheet adapter (in-process: calamine + csv + rust_xlsxwriter) owns
tabular pairs — xlsx/xlsm/xlsb/xls/ods/csv/tsv → csv/tsv/xlsx — as cell values
only (no styles, charts, or formula evaluation). Dates export as ISO calendar
strings; XLSX writes preserve cell text (no boolean/number inference) except
re-emitting `YYYY-MM-DD` as Excel dates. It does not advertise Markdown or HTML,
so MarkItDown/Docling/Pandoc keep document → text routes. Sheet-native paths
default-suggest CSV; CSV/TSV are chainable for a second hop into document engines.
The qpdf adapter owns PDF → PDF and PDF → `pdf-pages-zip`, including page
selection, secure password input, rotation, compression, linearization, and
split grouping. It never mutates the source and records its invocation.
`preferences::DEFAULT_MODULES` must list every module id in registration order;
an id missing there is sorted last by `with_priority` and silently loses every
overlap.

Optional knobs live on `ConversionOptions`:

| Nest | Knobs |
|------|--------|
| `ffmpeg` | trim, streams, encode mode, quality, mono, sample rate, scale, fps, mute, normalize audio, burn embedded subtitles, frame interval (sequence ZIP) |
| `docling` | image export mode, OCR, tables, table mode, OCR language, ASR model, video sampling/diarization; password via `pdf.password` |
| `defuddle` | frontmatter, language |
| `pandoc` | standalone, TOC, PDF engine override, reference-doc, citations (off by default) |
| `markitdown` | keep data URIs |
| `sips` | max dimension, quality, rotate, flip, strip color profile |
| `spreadsheet` | sheet name, sheet index (1-based); values-only csv/tsv/xlsx |
| `pdf` | password (never persisted), page_from / page_to, rotate, compression, linearize, split pages |

App and CLI expose the same conversion flags; defaults match historical fixed
invocations. Batch items may `Inherit` or `Override` the session output format
(`BatchFormatSelection`). Keep capability lists explicit so unsupported pairs
fail before an external process is launched. If a new module overlaps an existing
conversion pair, document and test the registry precedence.

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

## Workflow notes

- Inspect `git diff` before editing and push finished work to `origin/main`.
