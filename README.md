# Shift

Shift is a native macOS file- and URL-to-Markdown converter. Drop a supported
file on the left, paste a URL into the input bar, inspect the result on the
right, and download it immediately. The `shift-cli` executable exposes the same
conversion modules for scripts and terminal workflows.

The ingestion module is powered by
[Microsoft MarkItDown](https://github.com/microsoft/markitdown), which preserves
useful document structure such as headings, lists, links, and tables. A second
module uses [Pandoc](https://pandoc.org/) for its complete reader/writer format
set. Web pages are extracted with [Defuddle](https://github.com/kepano/defuddle),
which removes clutter and returns clean Markdown or HTML.
[Docling](https://github.com/docling-project/docling) reads PDFs and other
documents with layout-aware parsing and exports Markdown, HTML, or plain text
(including PDF → HTML). [FFmpeg](https://ffmpeg.org/) converts audio and video containers, extracts
still frames and subtitles, and exposes trim/quality/stream options in the app
and CLI. The output menu ranks mainstream formats first, followed by authoring,
publishing, wiki, presentation, media, and specialized formats.

## Supported input

- PDF (`.pdf`)
- PowerPoint (`.pptx`)
- Word (`.docx`)
- Excel (`.xlsx`, `.xls`)
- Images (`.bmp`, `.gif`, `.heic`, `.jpeg`, `.jpg`, `.png`, `.tif`, `.tiff`, `.webp`)
- Audio (`.aac`, `.ac3`, `.flac`, `.m4a`, `.mp3`, `.ogg`, `.opus`, `.wav`, `.wma`, …)
- Video (`.mp4`, `.mkv`, `.mov`, `.webm`, `.avi`, `.gif`, `.ts`, `.3gp`, and related containers)
- Stills used as media sources (`.png`, `.jpg`, `.webp`, …)
- HTML (`.html`, `.htm`)
- Text-based data (`.csv`, `.json`, `.xml`, `.txt`, `.md`)
- ZIP archives (`.zip`, converted by iterating over supported contents)

Image OCR/descriptions and audio transcription depend on the optional model or
service configuration supported by MarkItDown. Base metadata extraction works
locally where the upstream converter supports it.

## Prerequisites

- macOS with Xcode and its command-line tools
- Rust stable (selected automatically by `rust-toolchain.toml`)
- Python 3.10 or newer
- MarkItDown with its format dependencies:

```sh
uv venv --python 3.11 .venv
uv pip install --python .venv/bin/python 'markitdown[all]'
```

Shift discovers this project-local environment automatically. If the executable
is installed somewhere else, set
`SHIFT_MARKITDOWN_BIN=/absolute/path/to/markitdown`.

- Pandoc for multi-format output (plus Typst for PDF):

```sh
brew install pandoc typst
```

Set `SHIFT_PANDOC_BIN=/absolute/path/to/pandoc` when it is not available on
`PATH`. PDF output needs an external engine; Shift auto-selects the first one
it finds, preferring **Typst** (lightweight, recommended for new installs),
then Tectonic, then classic LaTeX engines (`xelatex` / `lualatex` /
`pdflatex`). Override with `SHIFT_PDF_ENGINE=/path/to/engine` if needed.
Without any engine, DOCX → PDF fails with an install hint instead of Pandoc's
raw `pdflatex not found` message.

- [Defuddle](https://github.com/kepano/defuddle) for extracting clean article
  content from web pages (URLs) or local HTML. Packaged `Shift.app` embeds
  Defuddle but still needs a system [Node.js](https://nodejs.org/) binary:

```sh
brew install node
# development / non-bundled: npm install -g defuddle
```

Node is resolved from `PATH`, Homebrew, nvm, fnm, volta, asdf, and mise. Set
`SHIFT_NODE_BIN=/absolute/path/to/node` when the GUI cannot see your install, or
`SHIFT_DEFUDDLE_BIN=/absolute/path/to/defuddle` to override the Defuddle CLI.

- [Docling](https://github.com/docling-project/docling) for layout-aware PDF and
  office conversion to Markdown, HTML, or plain text:

```sh
# into the project venv used by MarkItDown, or any environment on PATH
uv pip install --python .venv/bin/python docling
# or: pip install docling
```

Set `SHIFT_DOCLING_BIN=/absolute/path/to/docling` when it is not available on
`PATH`. First runs may download model weights. Prefer Docling above MarkItDown
in Settings when you want higher-quality PDF → Markdown.

- [FFmpeg](https://ffmpeg.org/) for audio and video conversion:

```sh
brew install ffmpeg
```

Set `SHIFT_FFMPEG_BIN=/absolute/path/to/ffmpeg` when it is not available on
`PATH`. Media conversions write into a temporary workspace and return the
artifact without modifying the source file. Large outputs may need a higher
`SHIFT_CONVERSION_MAX_OUTPUT_BYTES` (default 64 MiB).

In the native app, selecting a source reveals a **Conversion options** panel
with sections for engines on the active route:

- **FFmpeg** — quality, encode mode, trim, frame time, mono/sample rate, scale,
  FPS, mute, loudness normalize, burn embedded subtitles, stream indices, frame
  interval for **PNG Sequence (ZIP)**
- **Docling** — image export mode, OCR, OCR language, tables, table mode
- **PDF input** — page range (requires [qpdf](https://qpdf.sourceforge.io/)),
  password (session only; never written to disk)
- **Defuddle** — frontmatter, language
- **Pandoc** — standalone, TOC, PDF engine, reference DOCX/PPTX
- **MarkItDown** — keep data URIs

Settings → Options holds core session knobs. Use **Apply** after editing text
fields; chips reconvert immediately. Session format and options (except secrets)
persist under Application Support (`session-settings.json`).

**Result actions:** Download, Copy (text to clipboard, or path for binary),
Reveal in Finder, Open, engine pipeline badge, Show command (redacted argv).
Binary results also show a local, bounded header inspection: image dimensions,
PDF version/page-object markers, media container/audio facts, or ZIP entry
counts where the format exposes them. Shift never decodes media or extracts
archive contents merely to render this inspection; use **Open** for a full
preview in your default app.
FFmpeg long encodes can show determinate progress; other engines stay
indeterminate. Failed conversions surface install hints when an engine is
missing.

**Batch:** drop or open multiple files or folders (confirm expansion caps);
per-item format can **Override** the session format; Overwrite, cancel, retry,
and Reveal on success. CLI uses `--recursive` for directories.

**Keyboard (main window):** ⌘S download, ⌘C copy, ⌘R reveal, ⌘⇧F format menu,
⌘, settings, ⌘/ shortcuts help, Esc cancel/close.

Shift exposes every writer reported by Pandoc 3.10, including Markdown
variants, HTML, PDF, Word, PowerPoint, EPUB, ODT, RTF, LaTeX, Typst, AsciiDoc,
reStructuredText, Jupyter, Org, DocBook, JATS, bibliography formats, wiki
formats, web/Beamer slides, ICML, TEI, and Pandoc's specialized serializers,
plus FFmpeg media outputs (audio, video, PNG/JPEG frames, PNG sequence ZIP,
SRT/VTT subtitles).

## Native app

```sh
cargo dev
```

To exercise the first-run onboarding without touching your real Shift history
or preferences, launch an isolated temporary profile:

```sh
cargo new-user
```

The command starts the app with empty Application Support and paste-staging
directories, then removes that temporary state when the app exits.

Use `cargo new-user -- --dry-run` to verify the isolated profile setup without
launching the app.

The first build downloads and compiles GPUI and may take a few minutes. Dropping
or choosing a supported file starts conversion automatically and may suggest an
output format (video → MP4, audio → MP3, documents → Markdown) until you pick
one yourself. Paste an `http(s)` URL into the bar above the drop zone and press
Enter or Convert to extract the page with Defuddle (this performs an outbound
fetch to the given host). URL fetches are **public internet only** by default
(no localhost/LAN); use the file picker for local files, or set
`SHIFT_ALLOW_PRIVATE_URLS=1` / CLI `--allow-private-urls` to opt in. The source
file is never modified; Download refuses to overwrite the selected source. Use the
output dropdown (with search) on the right to choose a format (formats whose
engines are missing are labeled). Multi-file queue supports **Overwrite** (CLI
`--force` parity). The settings button opens a full-screen settings view with a
left sidebar (Converters, General, Options, Paths, Diagnostics, About). On
Converters, drag a module above another to make it the preferred engine for
overlapping conversions; status badges show whether each engine is installed.
The Diagnostics page reports versions, install hints, and distinguishes
registered format support from conversions that are ready on this Mac. Module
priority, conversion history, and session options are saved under macOS
Application Support; priority is shared with `shift-cli`.

## CLI

```sh
# Probe external engines
# exit 0 = at least one conversion engine ready; 1 = none ready
# Optional engines / PDF backends do not fail the exit code.
# For a full install gate: doctor --script | grep -q 'complete=true'
cargo run --bin shift-cli -- doctor
cargo run --bin shift-cli -- doctor --script   # key=value for scripts
cargo run --bin shift-cli -- doctor --quiet    # exit code only

# Writes report.md beside report.docx
cargo run --bin shift-cli -- report.docx

# Choose the destination
cargo run --bin shift-cli -- report.pdf -o ~/Downloads/report.md

# Convert Word to HTML with Pandoc
cargo run --bin shift-cli -- report.docx --to html

# Prefer Pandoc where both modules can produce Markdown
cargo run --bin shift-cli -- report.docx --module pandoc

# Extract a web page with Defuddle (public hosts only; --yes skips TTY confirm
# and is required for non-interactive/non-TTY network fetches)
cargo run --bin shift-cli -- https://example.com/article --yes

# PDF → HTML via Docling (MarkItDown only emits Markdown)
cargo run --bin shift-cli -- scan.pdf --to html --module docling

# Prefer Docling for PDF → Markdown quality
cargo run --bin shift-cli -- scan.pdf --module docling

# Extract audio from a video with FFmpeg
cargo run --bin shift-cli -- clip.mp4 --to mp3

# Trim, re-encode, scale, mute, normalize
cargo run --bin shift-cli -- clip.mp4 --to mp4 --start 10 --duration 30 \
  --quality high --scale-width 1280 --fps 30 --mute --normalize-audio

# Still frame, subtitle extract, PNG sequence ZIP
cargo run --bin shift-cli -- clip.mkv --to png --frame 12.5
cargo run --bin shift-cli -- clip.mkv --to srt --subtitle-stream 0
cargo run --bin shift-cli -- clip.mp4 --to png-sequence-zip --frame-interval 1

# PDF pages (needs qpdf) and OCR language (Docling)
cargo run --bin shift-cli -- scan.pdf --module docling --pages 2-5 --ocr-lang eng

# Pandoc reference template
cargo run --bin shift-cli -- notes.md --to docx --reference-doc ~/Templates/ref.docx

# Recursive folder batch
cargo run --bin shift-cli -- ./inbox --recursive -O ./out -t markdown --force

# Verbose redacted command lines + progress on stderr
cargo run --bin shift-cli -- clip.mp4 --to mp3 --verbose --progress

# Convert audio containers
cargo run --bin shift-cli -- track.wav --to flac --module ffmpeg

# Pipe Markdown to another command
cargo run --bin shift-cli -- data.xlsx --stdout

# Batch-convert several files into one folder (shared queue with the app)
cargo run --bin shift-cli -- batch report.pdf notes.docx -t markdown -O ./out --force
# Multi-input without the `batch` subcommand also enters batch mode when -O is set:
cargo run --bin shift-cli -- a.pdf b.docx -O ./out -t html

# Inspect the registered formats
cargo run --bin shift-cli -- formats
```

After `cargo install --path . --bin shift-cli`, use `shift-cli` directly in the
same forms. `shift-cli convert <INPUT>` is also accepted for explicit scripts.
In the native app, multi-select or multi-drop opens the queue panel: choose an
output folder, press Start (or Cancel), and Retry failed items. Files are only
queued on drop — conversion does not auto-start. Progress, retry, and
cancellation use the same `run_batch` runner as the CLI (Ctrl-C cancels).

Overwrite policy matches single-file and batch: an existing destination fails
unless you pass `--force` (app batch does not force-overwrite). Within one
batch queue, colliding output names are uniquified (`report.md`, `report-1.md`).

## Architecture

```text
GPUI app ─────┐                          ┌─ MarkItDownModule
              ├── ConversionRegistry ───┼─ PandocModule
shift-cli ────┘                          ├─ DefuddleModule  (URLs + HTML)
         └── BatchQueue / run_batch      ├─ DoclingModule   (PDF/office → md/html/text)
                                         └─ FfmpegModule    (audio/video/stills/subs)
```

`src/conversion/` is the product boundary. A module advertises supported
input/output pairs and returns an in-memory `ConversionArtifact`; it does not
know whether the caller is the GUI or CLI. `ConversionRegistry` owns capability
filtering and ordered dispatch, so adding another engine does not require
duplicating workflow code in either surface. Multi-file conversion is owned by
`src/conversion/batch.rs` so queue state, destinations, retry, and cancel stay
identical across the app and CLI.

## Checks

```sh
cargo fmt --check
cargo lint
cargo test --all-targets
```

For optimized binaries, run `cargo build --release`. GPUI is pre-1.0 and pinned
to `0.2.2`; update it deliberately and review its release notes when upgrading.
