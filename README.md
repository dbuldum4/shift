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
which removes clutter and returns clean Markdown or HTML. The output menu ranks
mainstream formats first, followed by authoring, publishing, wiki, presentation,
and specialized formats.

## Supported input

- PDF (`.pdf`)
- PowerPoint (`.pptx`)
- Word (`.docx`)
- Excel (`.xlsx`, `.xls`)
- Images (`.bmp`, `.gif`, `.heic`, `.jpeg`, `.jpg`, `.png`, `.tif`, `.tiff`, `.webp`)
- Audio (`.aac`, `.flac`, `.m4a`, `.mp3`, `.ogg`, `.wav`)
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

- Pandoc for multi-format output:

```sh
brew install pandoc
```

Set `SHIFT_PANDOC_BIN=/absolute/path/to/pandoc` when it is not available on
`PATH`. PDF output may additionally require one of Pandoc's supported PDF
engines.

- [Defuddle](https://github.com/kepano/defuddle) for extracting clean article
  content from web pages (URLs) or local HTML:

```sh
npm install -g defuddle
# or use npx / a project-local node_modules/.bin/defuddle
```

Set `SHIFT_DEFUDDLE_BIN=/absolute/path/to/defuddle` when it is not available on
`PATH`.

Shift exposes every writer reported by Pandoc 3.10, including Markdown
variants, HTML, PDF, Word, PowerPoint, EPUB, ODT, RTF, LaTeX, Typst, AsciiDoc,
reStructuredText, Jupyter, Org, DocBook, JATS, bibliography formats, wiki
formats, web/Beamer slides, ICML, TEI, and Pandoc's specialized serializers.

## Native app

```sh
cargo dev
```

The first build downloads and compiles GPUI and may take a few minutes. Dropping
or choosing a supported file starts conversion automatically. Paste an `http(s)`
URL into the bar above the drop zone and press Enter or Convert to extract the
page with Defuddle. The source file is never modified. Use the output dropdown
on the right to choose a format. The settings button opens a module-priority
list; drag a module above another to make it the preferred engine for
overlapping conversions. Priority is saved in macOS Application Support and
shared with `shift-cli`.

## CLI

```sh
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
shift-cli ────┘                          └─ DefuddleModule  (URLs + HTML)
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
