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
  content from web pages (URLs) or local HTML:

```sh
npm install -g defuddle
# or use npx / a project-local node_modules/.bin/defuddle
```

Set `SHIFT_DEFUDDLE_BIN=/absolute/path/to/defuddle` when it is not available on
`PATH`.

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

In the native app, selecting a media file (or a media output format) reveals a
**Media options** panel: quality, encode mode, trim start/duration, frame time
for stills, mono/sample rate, scale width, and audio/subtitle stream indices.
Use **Apply** after editing text fields; chips reconvert immediately.

Shift exposes every writer reported by Pandoc 3.10, including Markdown
variants, HTML, PDF, Word, PowerPoint, EPUB, ODT, RTF, LaTeX, Typst, AsciiDoc,
reStructuredText, Jupyter, Org, DocBook, JATS, bibliography formats, wiki
formats, web/Beamer slides, ICML, TEI, and Pandoc's specialized serializers,
plus FFmpeg media outputs (audio, video, PNG/JPEG frames, SRT/VTT
subtitles).

## Native app

```sh
cargo dev
```

The first build downloads and compiles GPUI and may take a few minutes. Dropping
or choosing a supported file starts conversion automatically. Paste an `http(s)`
URL into the bar above the drop zone and press Enter or Convert to extract the
page with Defuddle. The source file is never modified. Use the output dropdown
on the right to choose a format. The settings button opens a full-screen
settings view with a left sidebar (Converters, General, Media, Paths,
Diagnostics, About). On Converters, drag a module above another to make it the
preferred engine for overlapping conversions; status badges show whether each
engine is installed. The Diagnostics page reports versions, install hints, and
distinguishes registered format support from conversions that are ready on this
Mac. Priority is saved in macOS Application Support and shared with `shift-cli`.

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

# Extract a web page with Defuddle
cargo run --bin shift-cli -- https://example.com/article

# PDF → HTML via Docling (MarkItDown only emits Markdown)
cargo run --bin shift-cli -- scan.pdf --to html --module docling

# Prefer Docling for PDF → Markdown quality
cargo run --bin shift-cli -- scan.pdf --module docling

# Extract audio from a video with FFmpeg
cargo run --bin shift-cli -- clip.mp4 --to mp3

# Trim, re-encode, and scale
cargo run --bin shift-cli -- clip.mp4 --to mp4 --start 10 --duration 30 \
  --quality high --scale-width 1280

# Still frame and subtitles
cargo run --bin shift-cli -- clip.mkv --to png --frame 12.5
cargo run --bin shift-cli -- clip.mkv --to srt --subtitle-stream 0

# Convert audio containers
cargo run --bin shift-cli -- track.wav --to flac --module ffmpeg

# Pipe Markdown to another command
cargo run --bin shift-cli -- data.xlsx --stdout

# Inspect the registered formats
cargo run --bin shift-cli -- formats
```

After `cargo install --path . --bin shift-cli`, use `shift-cli` directly in the
same forms. `shift-cli convert <INPUT>` is also accepted for explicit scripts.

## Architecture

```text
GPUI app ─────┐                          ┌─ MarkItDownModule
              ├── ConversionRegistry ───┼─ PandocModule
shift-cli ────┘                          ├─ DefuddleModule  (URLs + HTML)
                                         ├─ DoclingModule   (PDF/office → md/html/text)
                                         └─ FfmpegModule    (audio/video/stills/subs)
```

`src/conversion/` is the product boundary. A module advertises supported
input/output pairs and returns an in-memory `ConversionArtifact`; it does not
know whether the caller is the GUI or CLI. `ConversionRegistry` owns capability
filtering and ordered dispatch, so adding another engine does not require
duplicating workflow code in either surface.

## Checks

```sh
cargo fmt --check
cargo lint
cargo test --all-targets
```

For optimized binaries, run `cargo build --release`. GPUI is pre-1.0 and pinned
to `0.2.2`; update it deliberately and review its release notes when upgrading.
