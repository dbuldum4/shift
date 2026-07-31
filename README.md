# Shift

Shift is a native macOS file- and URL-to-Markdown converter. Drop a supported
file on the left, paste a URL into the input bar, inspect the result on the
right, and download it immediately. The `shift-cli` executable exposes the same
conversion modules for scripts and terminal workflows.

## Install, upgrade, and uninstall

Shift 1.1 supports macOS 13 (Ventura) and later on Apple Silicon and Intel.
Download the archive that matches your Mac from [GitHub
Releases](https://github.com/dbuldum4/shift/releases), then verify its checksum
before opening it:

```sh
shasum -a 256 -c shift-1.1.1-macos-<arch>.zip.sha256
```

Open the DMG and drag `Shift.app` into Applications. The command-line tool is
bundled at `/Applications/Shift.app/Contents/Resources/bin/shift-cli`; add that
directory to `PATH` if you want to invoke it directly:

```sh
echo 'export PATH="/Applications/Shift.app/Contents/Resources/bin:$PATH"' >> ~/.zshrc
```

To upgrade, quit Shift, replace `Shift.app` with the newer release, and rerun
`shift-cli --version`. Your preferences, history, and non-secret session options
are retained under `~/Library/Application Support/Shift`.

To uninstall the app, move `Shift.app` from Applications to the Trash. To also
remove local history, conversion cache, preferences, and session settings, open
Finder, choose **Go → Go to Folder…**, enter
`~/Library/Application Support/Shift`, and move that folder to the Trash. This
does not remove files you converted or downloaded elsewhere. See [the release
guide](docs/RELEASE.md) for the maintainer checklist and [third-party
notices](THIRD_PARTY_NOTICES.md) for bundled and source dependencies.

The ingestion module is powered by
[Microsoft MarkItDown](https://github.com/microsoft/markitdown), which preserves
useful document structure such as headings, lists, links, and tables. A second
module uses [Pandoc](https://pandoc.org/) for its complete reader/writer format
set. Web pages are extracted with [Defuddle](https://github.com/kepano/defuddle),
which removes clutter and returns clean Markdown or HTML.
[Docling](https://github.com/docling-project/docling) reads PDFs and other
documents with layout-aware parsing and exports Markdown, HTML, plain text, or
JSON (including PDF → HTML). On Apple Silicon, its local ASR pipeline turns
audio and video into a dedicated `transcript` output (FFmpeg keeps
subtitle-track SRT/VTT). The pinned ASR stack is unavailable on macOS Intel
because PyTorch 2.8+ does not publish Intel Mac wheels; Intel builds retain
Docling's document conversion routes.
[qpdf](https://qpdf.sourceforge.io/)
provides PDF page extraction, rotation, compression (lossless Flate or smaller
lossy image recompress), linearization, and page splitting.
[FFmpeg](https://ffmpeg.org/) converts audio and video containers, extracts
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
- Python 3.11 (packaged launchers and release packaging resolve `python3.11`;
  Homebrew `python@3.11` is the documented path)
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
  office conversion to Markdown, HTML, plain text, or JSON. Install its pinned
  ASR/video extras to transcribe audio and video to the dedicated `transcript`
  output:

```sh
# into the project venv used by MarkItDown, or any environment on PATH
uv pip install --python .venv/bin/python \
  'docling[asr]==2.115.0' 'docling-slim[format-video]==2.115.0'
# or: pip install 'docling[asr]==2.115.0' 'docling-slim[format-video]==2.115.0'
```

Set `SHIFT_DOCLING_BIN=/absolute/path/to/docling` when it is not available on
`PATH`. Audio/video transcription is available on Apple Silicon and also needs
FFmpeg; the chosen Whisper model downloads on first use. On macOS Intel, install
base `docling==2.115.0` for document conversion; Shift does not advertise its
unsupported transcript route. Prefer Docling above MarkItDown in Settings when
you want higher-quality PDF → Markdown.

- [qpdf](https://qpdf.sourceforge.io/) for PDF toolkit operations:

```sh
brew install qpdf
```

Set `SHIFT_QPDF_BIN=/absolute/path/to/qpdf` when it is not available on `PATH`.
Page extract/rotate and Flate recompress are lossless; **Smaller** compression
re-encodes suitable images with JPEG (lossy) for size. PDF passwords are passed
through restrictive temporary files and never appear in converter command lines
or persisted session settings.

- [FFmpeg](https://ffmpeg.org/) for audio and video conversion:

```sh
brew install ffmpeg
```

Set `SHIFT_FFMPEG_BIN=/absolute/path/to/ffmpeg` when it is not available on
`PATH`. Fit-to-size conversions also use the `ffprobe` installed with FFmpeg;
set `SHIFT_FFPROBE_BIN=/absolute/path/to/ffprobe` to override it. Media
conversions write into a temporary workspace and return the artifact without
modifying the source file. Large outputs may need a higher
`SHIFT_CONVERSION_MAX_OUTPUT_BYTES` (default 64 MiB).

In the native app, selecting a source reveals a **Conversion options** panel
with sections for engines on the active route:

- **FFmpeg** — quality, encode mode, trim, frame time, mono/sample rate, scale,
  FPS, mute, loudness normalize, burn embedded subtitles, stream indices, frame
  interval for **PNG Sequence (ZIP)**
- **Fit to size** — supported lossy media and still-image routes can target a
  maximum artifact size. Shift calculates an initial bitrate or image quality,
  checks the produced file, and retries when container or encoder overhead puts
  it above the requested cap. Video planning floors (~80 kbps video / ~32 kbps
  audio) limit how small short clips can go; large stills may also need a max
  dimension so quality alone can fit.
- **Docling** — image export mode, OCR, OCR language, tables, table mode;
  audio/video ASR model, video frame sampling, and optional speaker diarization
- **PDF toolkit** — page range, password (session only), 90° rotation,
  preserve/lossless/smaller compression, web linearization, and split-page ZIP
  output
- **Defuddle** — frontmatter, language
- **Pandoc** — standalone, TOC, PDF engine, reference DOCX/PPTX
- **MarkItDown** — keep data URIs

Settings → Options holds core session knobs. Use **Apply** after editing text
fields; chips reconvert immediately. Session format and options (except secrets)
persist under Application Support (`session-settings.json`).

**Recipes:** Settings → Recipes saves the current output format, preferred
converter, every non-secret conversion option, optional output folder,
overwrite policy, and a file-name template. Applying a recipe updates the
current conversion and snapshots the resolved setup into still-queued batch
items; running and finished items are unchanged. The active recipe is shown
beside the output selector, with a `modified` marker after local edits. PDF
passwords, cancellation flags, and progress callbacks are never written.
Recipes are shared with `shift-cli` through the versioned, atomically-written
`conversion-recipes.json` file under Application Support.

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
or choosing a supported file selects the source and may suggest an output format
(video → MP4, audio → MP3, documents → Markdown) until you pick one yourself —
conversion does **not** auto-start on drop; press Convert (or Enter in the paste
bar) when you are ready. Multi-file drops only queue work until you press Start.
Paste an `http(s)` URL into the bar above the drop zone and press Enter or
Convert to extract the page with Defuddle (this performs an outbound fetch to
the given host). URL fetches are **public internet only** by default
(no localhost/LAN); use the file picker for local files, or set
`SHIFT_ALLOW_PRIVATE_URLS=1` / CLI `--allow-private-urls` to opt in. The source
file is never modified; Download refuses to overwrite the selected source. Use the
output dropdown (with search) on the right to choose a format (formats whose
engines are missing are labeled). Multi-file queue supports **Overwrite** (CLI
`--force` parity). The settings button opens a full-screen settings view with a
left sidebar (Converters, General, Recipes, Options, Paths, Diagnostics, About). On
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

# Local audio/video transcription (dedicated transcript action)
cargo run --bin shift-cli -- interview.m4a --to transcript --module docling \
  --docling-asr-model turbo
cargo run --bin shift-cli -- recording.mp4 --to transcript --module docling \
  --docling-video-sampling scene --docling-video-diarization

# Extract embedded captions with FFmpeg (track demux, not ASR)
cargo run --bin shift-cli -- clip.mkv --to vtt

# Extract audio from a video with FFmpeg
cargo run --bin shift-cli -- clip.mp4 --to mp3

# Trim, re-encode, scale, mute, normalize
cargo run --bin shift-cli -- clip.mp4 --to mp4 --start 10 --duration 30 \
  --quality high --scale-width 1280 --fps 30 --mute --normalize-audio

# Fit compressed media under an attachment limit (bare values mean decimal MB)
cargo run --bin shift-cli -- clip.mp4 --to mp4 --target-size 10MB
cargo run --bin shift-cli -- interview.wav --to mp3 --target-size 25

# Still frame, subtitle extract, PNG sequence ZIP
cargo run --bin shift-cli -- clip.mkv --to png --frame 12.5
cargo run --bin shift-cli -- clip.mkv --to srt --subtitle-stream 0
cargo run --bin shift-cli -- clip.mp4 --to png-sequence-zip --frame-interval 1

# PDF pages and OCR language (Docling)
cargo run --bin shift-cli -- scan.pdf --module docling --pages 2-5 --ocr-lang eng

# Lossless PDF rewrite, rotation, web optimization, and split-page ZIP
cargo run --bin shift-cli -- scan.pdf --to pdf --pages 2-5 --pdf-rotate 90 \
  --pdf-compression lossless --pdf-linearize
cargo run --bin shift-cli -- scan.pdf --to pdf-pages-zip --pdf-split-pages 1

# Pandoc reference template
cargo run --bin shift-cli -- notes.md --to docx --reference-doc ~/Templates/ref.docx

# Recursive folder batch
cargo run --bin shift-cli -- ./inbox --recursive -O ./out -t markdown --force
# Recreates nested folders below ./out.

# One input → several outputs (repeat --also-to as needed)
cargo run --bin shift-cli -- report.docx -t markdown \
  --also-to html --also-to pdf -O ./out

# Safe shared naming template
cargo run --bin shift-cli -- ./inbox --recursive -O ./out \
  --name-template '{parent}-{stem}-{format}.{ext}'

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

# Save, inspect, apply, and delete named recipes
cargo run --bin shift-cli -- recipes save web-video --to mp4 --module ffmpeg \
  --quality high --scale-width 1280 -O ./exports \
  --name-template '{stem}-web.{ext}'
cargo run --bin shift-cli -- recipes list
cargo run --bin shift-cli -- recipes show web-video
cargo run --bin shift-cli -- clip.mov --recipe web-video
cargo run --bin shift-cli -- clip.mov --recipe web-video --quality small --no-force
cargo run --bin shift-cli -- recipes delete web-video

# Inspect the registered formats
cargo run --bin shift-cli -- formats
```

After `cargo install --path . --bin shift-cli`, use `shift-cli` directly in the
same forms. `shift-cli convert <INPUT>` is also accepted for explicit scripts.
When `--recipe NAME` is present, saved values are loaded first and every
explicit CLI flag wins, regardless of whether it appears before or after the
input. Recipe commands are `recipes list`, `show`, `save`, and `delete`.
In the native app, multi-select or multi-drop opens the queue panel. Each queued
output has a capability-filtered **Format** picker, **+ output** adds
another output for the same source, and **Remove** drops a queued output. Choose
an output folder, optionally apply a naming template or recipe, then press Start
(or Cancel) and Retry failed items. Files are only queued on drop — conversion
does not auto-start. Progress, retry, and cancellation use the same `run_batch`
runner as the CLI (Ctrl-C cancels).

Overwrite policy matches single-file and batch: an existing destination fails
unless you pass `--force` (app batch does not force-overwrite). Within one
batch queue, colliding output names are uniquified (`report.md`, `report-1.md`).
Recursive folder batches preserve the path below each selected source folder,
so `inbox/team/drafts/report.pdf` writes to
`out/team/drafts/report.md`.

Batch naming templates are resolved in the shared queue and accept:

| Placeholder | Value |
|---|---|
| `{stem}` | Source file or URL stem |
| `{parent}` | Immediate source parent directory (`root` when unavailable) |
| `{format}` | Canonical output format id, such as `markdown` |
| `{ext}` | Output extension, such as `md` |

Templates create a file name, not a path. Shift rejects traversal, directory
separators, unknown placeholders, control characters, and platform-reserved
file-name characters. If a template omits the correct extension, Shift appends
it. The default is `{stem}.{ext}`.

## macOS workflows and automation

The packaged app registers as an **alternate Finder viewer** for Shift's
convertible document, spreadsheet, image, audio, video, and subtitle formats.
Select one or more files in Finder and choose **Open With → Shift** (or run
`open -a Shift file1 file2`). A single file opens its normal preview; multiple
files enter the existing batch queue. Shift never makes itself the default app
for these formats.

For Shortcuts or Automator, use **Run Shell Script** and pass the Shortcut's
input files as arguments. `shift-cli` emits only completed artifact paths to
standard output; progress and diagnostics remain on standard error:

```sh
shift-cli batch "$@" --to markdown --output-dir "$HOME/Desktop/Shift output" --yes
```

For a drop folder, keep output **outside** the watched tree so Shift cannot
consume its own artifacts. The watcher waits for an unchanged file before it
uses the shared `BatchQueue` / `run_batch` path; Ctrl-C stops the current item
and the remaining queue safely.

```sh
# Test the current inbox once (useful in a Shortcut).
shift-cli watch "$HOME/Inbox" -O "$HOME/Shift output" -t pdf --once --yes

# Keep monitoring: check every second and require two seconds of stability.
shift-cli watch "$HOME/Inbox" -O "$HOME/Shift output" -t markdown \
  --poll 1 --debounce 2 --yes
```

Watch mode accepts the same converter flags as `batch` (`--module`, OCR,
media, PDF range, and so on). It deliberately does not save watched folders or
secrets: automation should be explicit, inspectable, and easy to stop.

## Architecture

```text
GPUI app ─────┐                          ┌─ MarkItDownModule
              ├── ConversionRegistry ───┼─ PandocModule
shift-cli ────┘                          ├─ DefuddleModule  (URLs + HTML)
         └── BatchQueue / run_batch      ├─ DoclingModule   (docs → text formats; ARM ASR → transcript)
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
