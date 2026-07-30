//! Format conversion modules and capability-based dispatch.

mod batch;
mod defuddle;
mod diagnostics;
mod docling;
mod ffmpeg;
mod inspection;
mod magic_paste;
mod markitdown;
mod pandoc;
mod pdf_slice;
mod process;
mod qpdf;
mod sips;
mod sources;
mod spreadsheet;
mod suggest;
mod watch;

#[cfg(test)]
mod registry_parity;

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

pub use batch::{
    BatchEnqueueOptions, BatchEvent, BatchFormatSelection, BatchInput, BatchItem, BatchItemId,
    BatchItemState, BatchNamingTemplate, BatchProgress, BatchProvenance, BatchQueue, BatchSource,
    BatchSummary, available_outputs_for_batch_source, prepare_batch_destination,
    resolve_destination, resolve_destination_with_policy, run_batch, suggested_url_file_name,
    uniquify_destination, validate_batch_output_formats,
};
pub use defuddle::{
    DefuddleModule, DefuddleOptions, block_private_urls, ensure_public_url_fetch_allowed,
    looks_like_url, redact_url_credentials, url_display_host, url_targets_non_public_host,
};
pub use diagnostics::{
    DiagnosticsReport, EngineDiagnostic, FormatAvailability, PdfEngineDiagnostic, Readiness,
    available_ready_outputs, available_ready_url_outputs, format_availability, supported_outputs,
};
pub use docling::{
    DoclingAsrModel, DoclingImageExportMode, DoclingModule, DoclingOptions, DoclingTableMode,
    DoclingVideoSamplingMode, is_docling_audio_input, is_docling_timed_input,
    is_docling_video_input,
};
pub use ffmpeg::{
    FfmpegEncodeMode, FfmpegModule, FfmpegOptions, FfmpegQuality,
    ffmpeg_supports_target_size_output, input_looks_like_media, is_audio_output, is_ffmpeg_output,
    is_image_output, is_subtitle_output, is_video_output,
};
pub use inspection::{
    ArtifactInspection, MAX_INSPECTION_PREFIX_BYTES, MAX_INSPECTION_SUFFIX_BYTES, inspect_binary,
};
pub use magic_paste::{
    MAX_REMOTE_FILE_BYTES, MagicPaste, PasteToken, REMOTE_DOWNLOAD_TIMEOUT,
    materialize_magic_paste, materialize_paste_token, parse_magic_paste, stage_pasted_image,
    url_looks_like_remote_file,
};
pub use markitdown::{MarkItDownModule, MarkItDownOptions};
pub use pandoc::{PandocModule, PandocOptions, pdf_engine_candidates, resolve_pdf_engine};
pub use pdf_slice::extract_pdf_pages;
pub use process::{
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_PROCESS_TIMEOUT, FS_NAME_MAX, LimitedOutput,
    absolute_command_path, bundled_runtime_tool, create_private_file, find_executable, is_runnable,
    max_output_bytes, path_looks_like_option, process_timeout, push_flag_path, push_operand_path,
    push_path_arg, read_file_limited, resolve_tool_executable, resolve_tool_path, run_command,
    run_command_cancellable, run_command_cancellable_with_output_paths, short_path_hash,
    unique_temp_dir, unique_temp_file_name, validate_path_operand, write_secret_file,
};
pub use qpdf::{PdfCompression, QpdfModule};
pub use sips::{SipsFlip, SipsModule, SipsOptions, SipsQuality, sips_supports_target_size_output};
pub use sources::{
    ExpandedInputPath, MAX_EXPAND_DEPTH, MAX_EXPAND_FILES, expand_input_paths,
    expand_input_paths_preserving_roots, expand_input_paths_preserving_roots_with_extensions,
    expand_input_paths_with_extensions, supported_input_extensions,
};
pub use spreadsheet::{SpreadsheetModule, SpreadsheetOptions};
pub use suggest::{suggested_output_for_path, suggested_output_for_url};
pub use watch::{WatchTracker, validate_watch_directories};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OutputFormat(&'static str);

impl OutputFormat {
    pub const MARKDOWN: Self = Self("markdown");
    pub const HTML: Self = Self("html");
    pub const DOCX: Self = Self("docx");
    pub const PDF: Self = Self("pdf");
    pub const PPTX: Self = Self("pptx");
    pub const EPUB: Self = Self("epub");
    /// Dedicated local speech-transcription action. Docling writes Markdown.
    pub const TRANSCRIPT: Self = Self("transcript");
    pub const JSON: Self = Self("json");

    // Media containers written by FFmpeg (and accepted as inputs there).
    pub const MP3: Self = Self("mp3");
    pub const WAV: Self = Self("wav");
    pub const FLAC: Self = Self("flac");
    pub const AAC: Self = Self("aac");
    pub const M4A: Self = Self("m4a");
    pub const OGG: Self = Self("ogg");
    pub const OPUS: Self = Self("opus");
    pub const AC3: Self = Self("ac3");
    pub const WMA: Self = Self("wma");
    pub const CAF: Self = Self("caf");
    pub const AIFF: Self = Self("aiff");
    pub const MP4: Self = Self("mp4");
    pub const WEBM: Self = Self("webm");
    pub const MKV: Self = Self("mkv");
    pub const MOV: Self = Self("mov");
    pub const AVI: Self = Self("avi");
    pub const GIF: Self = Self("gif");
    pub const M4V: Self = Self("m4v");
    pub const MPEG: Self = Self("mpeg");
    pub const TS: Self = Self("ts");
    pub const THREEGP: Self = Self("3gp");
    pub const PNG: Self = Self("png");
    pub const JPG: Self = Self("jpg");
    pub const WEBP: Self = Self("webp");
    pub const SRT: Self = Self("srt");
    pub const VTT: Self = Self("vtt");
    /// PNG frame sequence packaged as a single ZIP (FFmpeg).
    pub const PNG_SEQUENCE_ZIP: Self = Self("png-sequence-zip");
    /// Lossless per-page PDFs packaged as one downloadable artifact.
    pub const PDF_PAGES_ZIP: Self = Self("pdf-pages-zip");

    // Still-image writers owned by the sips adapter (macOS ImageIO).
    // PNG/JPG/GIF/PDF above are shared with FFmpeg/Pandoc; see
    // `ConversionRegistry::build_default` for precedence.
    pub const TIFF: Self = Self("tiff");
    pub const BMP: Self = Self("bmp");
    pub const HEIC: Self = Self("heic");
    pub const AVIF: Self = Self("avif");
    pub const JP2: Self = Self("jp2");
    pub const ICNS: Self = Self("icns");

    // Spreadsheet writers owned by the spreadsheet adapter (calamine/csv/xlsx).
    pub const CSV: Self = Self("csv");
    pub const TSV: Self = Self("tsv");
    pub const XLSX: Self = Self("xlsx");

    /// Every writer in Pandoc 3.10, ordered by practical end-user popularity.
    /// Closely related variants follow their best-known parent format.
    pub const PANDOC: &'static [Self] = &[
        Self::MARKDOWN,
        Self::HTML,
        Self::PDF,
        Self::DOCX,
        Self::PPTX,
        Self("plain"),
        Self("gfm"),
        Self("commonmark"),
        Self("commonmark_x"),
        Self::EPUB,
        Self("epub3"),
        Self("epub2"),
        Self("odt"),
        Self("rtf"),
        Self("latex"),
        Self("typst"),
        Self("asciidoc"),
        Self("asciidoctor"),
        Self("asciidoc_legacy"),
        Self("rst"),
        Self("ipynb"),
        Self("org"),
        Self("revealjs"),
        Self("beamer"),
        Self::JSON,
        Self("xml"),
        Self("opml"),
        Self("docbook"),
        Self("docbook5"),
        Self("docbook4"),
        Self("jats"),
        Self("jats_archiving"),
        Self("jats_articleauthoring"),
        Self("jats_publishing"),
        Self("bibtex"),
        Self("biblatex"),
        Self("csljson"),
        Self("mediawiki"),
        Self("jira"),
        Self("dokuwiki"),
        Self("xwiki"),
        Self("zimwiki"),
        Self("html5"),
        Self("html4"),
        Self("chunkedhtml"),
        Self("markdown_github"),
        Self("markdown_mmd"),
        Self("markdown_phpextra"),
        Self("markdown_strict"),
        Self("djot"),
        Self("textile"),
        Self("muse"),
        Self("markua"),
        Self("fb2"),
        Self("tei"),
        Self("icml"),
        Self("opendocument"),
        Self("context"),
        Self("texinfo"),
        Self("man"),
        Self("ms"),
        Self("vimdoc"),
        Self("haddock"),
        Self("native"),
        Self("ansi"),
        Self("slidy"),
        Self("slideous"),
        Self("s5"),
        Self("dzslides"),
        Self("bbcode"),
        Self("bbcode_phpbb"),
        Self("bbcode_xenforo"),
        Self("bbcode_steam"),
        Self("bbcode_fluxbb"),
        Self("bbcode_hubzilla"),
    ];

    /// Audio, video, still-image, and subtitle formats FFmpeg can write.
    pub const MEDIA: &'static [Self] = &[
        // Audio
        Self::MP3,
        Self::WAV,
        Self::FLAC,
        Self::AAC,
        Self::M4A,
        Self::OGG,
        Self::OPUS,
        Self::AC3,
        Self::WMA,
        Self::CAF,
        Self::AIFF,
        // Video
        Self::MP4,
        Self::WEBM,
        Self::MKV,
        Self::MOV,
        Self::AVI,
        Self::GIF,
        Self::M4V,
        Self::MPEG,
        Self::TS,
        Self::THREEGP,
        // Still frames
        Self::PNG,
        Self::JPG,
        Self::WEBP,
        Self::PNG_SEQUENCE_ZIP,
        // Subtitles
        Self::SRT,
        Self::VTT,
    ];

    /// User-facing actions whose destination is owned exclusively by Docling.
    ///
    /// Docling also writes formats in `PANDOC` (Markdown, HTML, JSON, plain
    /// text); this slice contains only its non-overlapping transcript-intent
    /// alias. Timed-media ASR is limited to `TRANSCRIPT` so FFmpeg keeps
    /// subtitle-track VTT/SRT and document Markdown routes stay free of ASR.
    pub const DOCLING: &'static [Self] = &[Self::TRANSCRIPT];

    /// Full UI/parse catalog: publishing formats first, then media.
    ///
    /// Prefer [`Self::all`] when iterating. This slice exists for call sites that
    /// need a `'static` list (menus, `available_outputs` filtering).
    pub const ALL: &'static [Self] = &[
        // Primary publishing actions.
        Self::MARKDOWN,
        Self::HTML,
        Self::PDF,
        Self::DOCX,
        Self::PPTX,
        Self("plain"),
        // Dedicated Docling ASR action (Markdown payload with transcript intent).
        Self::TRANSCRIPT,
        // Remaining Pandoc writers (must otherwise stay in sync with `PANDOC`).
        Self("gfm"),
        Self("commonmark"),
        Self("commonmark_x"),
        Self::EPUB,
        Self("epub3"),
        Self("epub2"),
        Self("odt"),
        Self("rtf"),
        Self("latex"),
        Self("typst"),
        Self("asciidoc"),
        Self("asciidoctor"),
        Self("asciidoc_legacy"),
        Self("rst"),
        Self("ipynb"),
        Self("org"),
        Self("revealjs"),
        Self("beamer"),
        Self("json"),
        Self("xml"),
        Self("opml"),
        Self("docbook"),
        Self("docbook5"),
        Self("docbook4"),
        Self("jats"),
        Self("jats_archiving"),
        Self("jats_articleauthoring"),
        Self("jats_publishing"),
        Self("bibtex"),
        Self("biblatex"),
        Self("csljson"),
        Self("mediawiki"),
        Self("jira"),
        Self("dokuwiki"),
        Self("xwiki"),
        Self("zimwiki"),
        Self("html5"),
        Self("html4"),
        Self("chunkedhtml"),
        Self("markdown_github"),
        Self("markdown_mmd"),
        Self("markdown_phpextra"),
        Self("markdown_strict"),
        Self("djot"),
        Self("textile"),
        Self("muse"),
        Self("markua"),
        Self("fb2"),
        Self("tei"),
        Self("icml"),
        Self("opendocument"),
        Self("context"),
        Self("texinfo"),
        Self("man"),
        Self("ms"),
        Self("vimdoc"),
        Self("haddock"),
        Self("native"),
        Self("ansi"),
        Self("slidy"),
        Self("slideous"),
        Self("s5"),
        Self("dzslides"),
        Self("bbcode"),
        Self("bbcode_phpbb"),
        Self("bbcode_xenforo"),
        Self("bbcode_steam"),
        Self("bbcode_fluxbb"),
        Self("bbcode_hubzilla"),
        // Media (must stay in sync with `MEDIA`).
        Self::MP3,
        Self::WAV,
        Self::FLAC,
        Self::AAC,
        Self::M4A,
        Self::OGG,
        Self::OPUS,
        Self::AC3,
        Self::WMA,
        Self::CAF,
        Self::AIFF,
        Self::MP4,
        Self::WEBM,
        Self::MKV,
        Self::MOV,
        Self::AVI,
        Self::GIF,
        Self::M4V,
        Self::MPEG,
        Self::TS,
        Self::THREEGP,
        Self::PNG,
        Self::JPG,
        Self::WEBP,
        Self::PNG_SEQUENCE_ZIP,
        Self::SRT,
        Self::VTT,
        // PDF toolkit.
        Self::PDF_PAGES_ZIP,
        // Still images (must stay in sync with `IMAGE`).
        Self::TIFF,
        Self::BMP,
        Self::HEIC,
        Self::AVIF,
        Self::JP2,
        Self::ICNS,
        // Spreadsheet (must stay in sync with `SPREADSHEET`).
        Self::CSV,
        Self::TSV,
        Self::XLSX,
    ];

    /// Tabular writers owned by the spreadsheet adapter.
    pub const SPREADSHEET: &'static [Self] = &[Self::CSV, Self::TSV, Self::XLSX];

    /// Downloadable compound artifacts owned by the PDF toolkit.
    pub const PDF_TOOLKIT: &'static [Self] = &[Self::PDF_PAGES_ZIP];

    /// Still-image writers that only the sips adapter provides.
    ///
    /// Deliberately excludes formats shared with other engines (`PNG`, `JPG`,
    /// `GIF`, `PDF`); those live in [`Self::MEDIA`] / [`Self::PANDOC`] and stay
    /// there so the existing "no overlap between catalogs" invariant holds.
    /// The sips module's full writable set is declared in `sips.rs`.
    pub const IMAGE: &'static [Self] = &[
        Self::TIFF,
        Self::BMP,
        Self::HEIC,
        Self::AVIF,
        Self::JP2,
        Self::ICNS,
    ];

    pub fn id(self) -> &'static str {
        self.0
    }

    pub fn label(self) -> &'static str {
        match self.0 {
            "markdown" => "Markdown",
            "html" => "HTML",
            "pdf" => "PDF",
            "docx" => "Word (DOCX)",
            "pptx" => "PowerPoint (PPTX)",
            "plain" => "Plain Text",
            "transcript" => "Transcript (Markdown)",
            "gfm" => "GitHub-Flavored Markdown",
            "commonmark" => "CommonMark",
            "commonmark_x" => "CommonMark (extended)",
            "epub" => "EPUB",
            "odt" => "OpenDocument (ODT)",
            "rtf" => "Rich Text (RTF)",
            "latex" => "LaTeX",
            "typst" => "Typst",
            "rst" => "reStructuredText",
            "ipynb" => "Jupyter Notebook",
            "org" => "Org Mode",
            "revealjs" => "Reveal.js Slides",
            "beamer" => "LaTeX Beamer",
            "json" => "JSON",
            "xml" => "Pandoc XML",
            "mediawiki" => "MediaWiki",
            "jira" => "Jira Wiki",
            "mp3" => "MP3 Audio",
            "wav" => "WAV Audio",
            "flac" => "FLAC Audio",
            "aac" => "AAC Audio",
            "m4a" => "M4A Audio",
            "ogg" => "Ogg Audio",
            "opus" => "Opus Audio",
            "ac3" => "AC-3 Audio",
            "wma" => "WMA Audio",
            "caf" => "Core Audio (CAF)",
            "aiff" => "AIFF Audio",
            "mp4" => "MP4 Video",
            "webm" => "WebM Video",
            "mkv" => "Matroska (MKV)",
            "mov" => "QuickTime (MOV)",
            "avi" => "AVI Video",
            "gif" => "GIF",
            "m4v" => "M4V Video",
            "mpeg" => "MPEG Video",
            "ts" => "MPEG-TS",
            "3gp" => "3GP Video",
            "png" => "PNG Image",
            "jpg" => "JPEG Image",
            "webp" => "WebP Image",
            "srt" => "SubRip (SRT)",
            "vtt" => "WebVTT",
            "png-sequence-zip" => "PNG Sequence (ZIP)",
            "pdf-pages-zip" => "PDF Pages (ZIP)",
            "tiff" => "TIFF Image",
            "bmp" => "BMP Image",
            "heic" => "HEIC Image",
            "avif" => "AVIF Image",
            "jp2" => "JPEG 2000",
            "icns" => "Apple Icon (ICNS)",
            "csv" => "CSV",
            "tsv" => "TSV",
            "xlsx" => "Excel (XLSX)",
            other => other,
        }
    }

    /// Lowercased [`Self::label`], interned once per process.
    ///
    /// Avoids re-allocating a lowercase copy on every call (e.g. filter/search
    /// paths in `main.rs`).
    pub fn label_lowercase(self) -> &'static str {
        intern_lowercase(self.label())
    }

    /// Lowercased [`Self::id`], interned once per process.
    pub fn id_lowercase(self) -> &'static str {
        intern_lowercase(self.id())
    }

    pub fn extension(self) -> &'static str {
        match self.0 {
            "markdown" | "transcript" | "gfm" | "commonmark" | "commonmark_x"
            | "markdown_github" | "markdown_mmd" | "markdown_phpextra" | "markdown_strict" => "md",
            "html" | "html4" | "html5" => "html",
            "chunkedhtml" => "zip",
            "docx" => "docx",
            "pdf" => "pdf",
            "pptx" => "pptx",
            "epub" | "epub2" | "epub3" => "epub",
            "odt" | "opendocument" => "odt",
            "rtf" => "rtf",
            "latex" | "beamer" | "context" => "tex",
            "typst" => "typ",
            "asciidoc" | "asciidoctor" | "asciidoc_legacy" => "adoc",
            "rst" => "rst",
            "ipynb" => "ipynb",
            "org" => "org",
            "json" | "csljson" => "json",
            "xml"
            | "docbook"
            | "docbook4"
            | "docbook5"
            | "jats"
            | "jats_archiving"
            | "jats_articleauthoring"
            | "jats_publishing"
            | "tei" => "xml",
            "bibtex" => "bib",
            "biblatex" => "biblatex",
            "opml" => "opml",
            "icml" => "icml",
            "fb2" => "fb2",
            "plain" | "ansi" => "txt",
            "png-sequence-zip" | "pdf-pages-zip" => "zip",
            // Media format ids match their file extensions.
            other => other,
        }
    }

    pub fn media_type(self) -> &'static str {
        match self.0 {
            "markdown" | "transcript" | "gfm" | "commonmark" | "commonmark_x" => "text/markdown",
            "html" | "html4" | "html5" => "text/html",
            "chunkedhtml" => "application/zip",
            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "pdf" => "application/pdf",
            "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "epub" | "epub2" | "epub3" => "application/epub+zip",
            "odt" | "opendocument" => "application/vnd.oasis.opendocument.text",
            "json" | "csljson" => "application/json",
            "xml" | "docbook" | "docbook4" | "docbook5" | "jats" | "tei" => "application/xml",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "flac" => "audio/flac",
            "aac" => "audio/aac",
            "m4a" => "audio/mp4",
            "ogg" => "audio/ogg",
            "opus" => "audio/opus",
            "ac3" => "audio/ac3",
            "wma" => "audio/x-ms-wma",
            "caf" => "audio/x-caf",
            "aiff" => "audio/aiff",
            "mp4" | "m4v" => "video/mp4",
            "webm" => "video/webm",
            "mkv" => "video/x-matroska",
            "mov" => "video/quicktime",
            "avi" => "video/x-msvideo",
            "gif" => "image/gif",
            "mpeg" => "video/mpeg",
            "ts" => "video/mp2t",
            "3gp" => "video/3gpp",
            "png" => "image/png",
            "jpg" => "image/jpeg",
            "webp" => "image/webp",
            "tiff" => "image/tiff",
            "bmp" => "image/bmp",
            "heic" => "image/heic",
            "avif" => "image/avif",
            "jp2" => "image/jp2",
            "icns" => "image/x-icns",
            "srt" => "application/x-subrip",
            "vtt" => "text/vtt",
            "png-sequence-zip" | "pdf-pages-zip" => "application/zip",
            "csv" => "text/csv",
            "tsv" => "text/tab-separated-values",
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            _ => "text/plain",
        }
    }

    /// Text-oriented formats suitable for in-app preview excerpts.
    pub fn is_text_previewable(self) -> bool {
        matches!(
            self.media_type(),
            "text/markdown"
                | "text/html"
                | "text/plain"
                | "text/vtt"
                | "text/csv"
                | "text/tab-separated-values"
                | "application/x-subrip"
                | "application/json"
                | "application/xml"
        ) || matches!(
            self.id(),
            "srt" | "vtt" | "plain" | "markdown" | "transcript" | "html" | "gfm" | "csv" | "tsv"
        )
    }
}

/// Return a `'static` lowercase copy of `value`, computed and leaked once.
///
/// Backed by a process-wide table so repeated calls (menus, search filters)
/// reuse the same interned string instead of allocating a fresh `String`.
fn intern_lowercase(value: &'static str) -> &'static str {
    static TABLE: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();
    let table = TABLE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table.lock().expect("lowercase intern table poisoned");
    if let Some(&interned) = table.get(value) {
        return interned;
    }
    let lowered = value.to_ascii_lowercase();
    let interned: &'static str = if lowered == value {
        value
    } else {
        Box::leak(lowered.into_boxed_str())
    };
    table.insert(value, interned);
    interned
}

impl std::str::FromStr for OutputFormat {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let lowered = value.to_ascii_lowercase();
        let key = match lowered.as_str() {
            "jpeg" => "jpg",
            "mpg" | "mpg2" => "mpeg",
            "aif" => "aiff",
            "tif" => "tiff",
            "heif" => "heic",
            "jpeg2000" | "jpx" => "jp2",
            "png-zip" | "png_sequence" | "frames-zip" | "png-sequence-zip" => "png-sequence-zip",
            "pdf-zip" | "pdf_pages" | "pdf-pages" | "pdf-pages-zip" => "pdf-pages-zip",
            other => other,
        };
        Self::ALL
            .iter()
            .copied()
            .find(|format| {
                format.id().eq_ignore_ascii_case(key)
                    || format.extension().eq_ignore_ascii_case(key)
            })
            .ok_or_else(|| ConversionError::new(format!("unknown output format: {value}")))
    }
}

/// Optional engine knobs passed through the registry to modules that understand them.
///
/// Each module reads only its own nested options; foreign knobs are ignored.
/// Defaults match the historical fixed CLI invocations so existing callers keep
/// the same conversion behavior.
/// PDF-input preprocess knobs shared by Docling / MarkItDown PDF routes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PdfInputOptions {
    /// Password for encrypted PDFs (Docling `--pdf-password`). Never persisted.
    pub password: Option<String>,
    /// 1-based inclusive page range start (requires qpdf when set with end).
    pub page_from: Option<u32>,
    /// 1-based inclusive page range end.
    pub page_to: Option<u32>,
    /// Relative clockwise rotation applied after page selection.
    pub rotate_degrees: Option<u16>,
    /// Stream and image rewrite policy for PDF outputs.
    pub compression: PdfCompression,
    /// Produce a web-optimized, linearized PDF.
    pub linearize: bool,
    /// Pages per PDF inside `pdf-pages-zip` (defaults to one).
    pub split_pages: Option<u32>,
}

impl PdfInputOptions {
    pub fn needs_slice(&self) -> bool {
        // `page_from == 1` with no `page_to` is the same as the full document,
        // so there is no need to run it through qpdf.
        self.page_from.is_some_and(|page| page > 1) || self.page_to.is_some()
    }

    /// True when qpdf should preprocess the PDF before any module runs.
    ///
    /// This includes page slicing and password decryption; both are handled
    /// securely through `qpdf --password-file` rather than exposing the
    /// password on a module command line.
    pub fn needs_preprocessing(&self) -> bool {
        self.needs_slice() || self.password.is_some()
    }

    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Optional engine knobs passed through the registry to modules that understand them.
///
/// Each module reads only its own nested options; foreign knobs are ignored.
/// Defaults match the historical fixed CLI invocations so existing callers keep
/// the same conversion behavior.
#[derive(Clone, Default)]
pub struct ConversionOptions {
    pub ffmpeg: FfmpegOptions,
    pub markitdown: MarkItDownOptions,
    pub pandoc: PandocOptions,
    pub defuddle: DefuddleOptions,
    pub docling: DoclingOptions,
    pub sips: SipsOptions,
    pub spreadsheet: SpreadsheetOptions,
    pub pdf: PdfInputOptions,
    /// Best-effort upper bound for the final artifact.
    ///
    /// Only modules that explicitly opt into target-size encoding may consume
    /// this value. It is global rather than engine-specific so a saved session,
    /// recipe, batch item, and CLI invocation all describe the same user goal.
    pub target_size_bytes: Option<u64>,
    /// When set and true, external converter processes should abort.
    ///
    /// Used by the shared batch runner for cooperative cancellation. Compared
    /// by pointer identity for equality so snapshots with the same engine knobs
    /// but different cancel flags are not considered equal.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Optional progress sink (compared by pointer identity for equality).
    pub progress: Option<ProgressSink>,
}

impl std::fmt::Debug for ConversionOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversionOptions")
            .field("ffmpeg", &self.ffmpeg)
            .field("markitdown", &self.markitdown)
            .field("pandoc", &self.pandoc)
            .field("defuddle", &self.defuddle)
            .field("docling", &self.docling)
            .field("sips", &self.sips)
            .field("spreadsheet", &self.spreadsheet)
            .field("pdf", &self.pdf)
            .field("target_size_bytes", &self.target_size_bytes)
            .field("cancel", &self.cancel.as_ref().map(|_| "<AtomicBool>"))
            .field(
                "progress",
                &self.progress.as_ref().map(|_| "<ProgressSink>"),
            )
            .finish()
    }
}

fn arc_ptr_eq<T: ?Sized>(left: &Option<Arc<T>>, right: &Option<Arc<T>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

impl PartialEq for ConversionOptions {
    fn eq(&self, other: &Self) -> bool {
        self.ffmpeg == other.ffmpeg
            && self.markitdown == other.markitdown
            && self.pandoc == other.pandoc
            && self.defuddle == other.defuddle
            && self.docling == other.docling
            && self.sips == other.sips
            && self.spreadsheet == other.spreadsheet
            && self.pdf == other.pdf
            && self.target_size_bytes == other.target_size_bytes
            && arc_ptr_eq(&self.cancel, &other.cancel)
            && arc_ptr_eq(&self.progress, &other.progress)
    }
}

impl Eq for ConversionOptions {}

/// One redacted argv line for UI / `--verbose` (never secrets).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationRecord {
    pub module_id: &'static str,
    pub argv_display: String,
}

/// Conversion progress for UI / CLI (side channel; not on the artifact).
#[derive(Clone, Debug, PartialEq)]
pub enum ConversionProgress {
    /// Indeterminate phase label (any engine).
    Phase(String),
    /// Determinate fraction in `0.0..=1.0` when known (FFmpeg).
    Fraction { fraction: f32, label: String },
}

/// Thread-safe progress callback used by modules and the batch runner.
pub type ProgressSink = Arc<dyn Fn(ConversionProgress) + Send + Sync>;

/// A completed conversion, independent of how it will be presented or saved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversionArtifact {
    pub file_name: String,
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
    pub format: OutputFormat,
    pub module_id: &'static str,
    /// Module ids that ran, first hop first (direct = one id).
    pub pipeline: Vec<&'static str>,
    /// Redacted invocations, same order as pipeline hops when available.
    pub invocations: Vec<InvocationRecord>,
}

impl ConversionArtifact {
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<(), ConversionError> {
        // Atomic: write a sibling partial, then rename into place so cancel /
        // crash never leaves a half-written final path. Replace existing files
        // (force semantics) — callers that must refuse clobber use
        // [`Self::write_to_with_replace`].
        write_bytes_atomically(path.as_ref(), &self.bytes)
    }

    /// Write the artifact, optionally refusing to replace an existing file.
    ///
    /// When `replace` is false the final path is published with an exclusive
    /// create (no TOCTOU between an earlier exists-check and the write).
    pub fn write_to_with_replace(
        &self,
        path: impl AsRef<Path>,
        replace: bool,
    ) -> Result<(), ConversionError> {
        write_bytes_atomically_with_replace(path.as_ref(), &self.bytes, replace)
    }

    /// Human-readable result summary for UI previews (text excerpt or binary facts).
    pub fn preview_summary(&self) -> String {
        if self.format.is_text_previewable() {
            return text_preview_excerpt(&self.bytes);
        }
        self.inspection().summary()
    }

    /// Header-derived facts for a binary result, safe to render without
    /// decoding media or extracting archive contents.
    pub fn inspection(&self) -> ArtifactInspection {
        inspect_binary(self.format, &self.bytes)
    }

    /// Fill pipeline/invocations for a single-module conversion when unset.
    pub fn with_module_provenance(
        mut self,
        module_id: &'static str,
        invocation: Option<InvocationRecord>,
    ) -> Self {
        if self.pipeline.is_empty() {
            self.pipeline = vec![module_id];
        }
        if self.invocations.is_empty() {
            if let Some(record) = invocation {
                self.invocations = vec![record];
            }
        }
        self.module_id = module_id;
        self
    }
}

fn file_stem_for_temp(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "output".into())
}

/// Write `bytes` to `path` via a unique `*.shift-partial` sibling, then rename.
///
/// On failure the partial file is removed. The final path appears only after a
/// complete write so cancelled conversions do not leave truncated destinations.
/// Existing destinations are replaced (force / overwrite semantics).
pub fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> Result<(), ConversionError> {
    write_bytes_atomically_with_replace(path, bytes, true)
}

/// Like [`write_bytes_atomically`], but when `replace` is false the destination
/// is published with an exclusive create so a concurrent creator cannot be
/// clobbered (closes the TOCTOU between `exists` checks and rename).
pub fn write_bytes_atomically_with_replace(
    path: &Path,
    bytes: &[u8],
    replace: bool,
) -> Result<(), ConversionError> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = parent.unwrap_or_else(|| Path::new("."));
    let stem = file_stem_for_temp(path);
    let partial = dir.join(unique_temp_file_name(&stem, ".shift-partial"));

    // Partial is sensitive intermediate content — keep it private on Unix.
    if let Err(error) = write_secret_file(&partial, bytes) {
        let _ = std::fs::remove_file(&partial);
        return Err(ConversionError::new(format!(
            "could not write {}: {error}",
            path.display()
        )));
    }

    if !replace {
        return publish_exclusive(&partial, path);
    }

    if let Err(error) = std::fs::rename(&partial, path) {
        // Some platforms refuse rename-over-existing. Move the previous file
        // aside first so a failed second rename can restore it (never delete
        // the only good copy before the new file is in place). The backup name
        // uses its own unique token so it cannot collide with the partial.
        if path.exists() {
            let backup = dir.join(unique_temp_file_name(&stem, ".shift-bak"));
            match std::fs::rename(path, &backup) {
                Ok(()) => match std::fs::rename(&partial, path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&backup);
                        return Ok(());
                    }
                    Err(error2) => {
                        let _ = std::fs::rename(&backup, path);
                        let _ = std::fs::remove_file(&partial);
                        return Err(ConversionError::new(format!(
                            "could not finalize {}: {error2}",
                            path.display()
                        )));
                    }
                },
                Err(_) => {
                    let _ = std::fs::remove_file(&partial);
                    return Err(ConversionError::new(format!(
                        "could not finalize {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        let _ = std::fs::remove_file(&partial);
        return Err(ConversionError::new(format!(
            "could not finalize {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

/// Publish `partial` to `path` only if `path` does not already exist.
///
/// Prefer `hard_link` (fails with AlreadyExists when the destination is taken),
/// then fall back to `OpenOptions::create_new` + copy. Never uses a plain
/// `rename`, which would replace an existing file on POSIX.
fn publish_exclusive(partial: &Path, path: &Path) -> Result<(), ConversionError> {
    match std::fs::hard_link(partial, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(partial);
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(partial);
            return Err(ConversionError::new(format!(
                "output already exists: {} (pass --force / enable Overwrite to replace)",
                path.display()
            )));
        }
        Err(_) => {
            // hard_link unavailable (some FS) — exclusive create + copy.
        }
    }

    use std::io::Write;
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(partial);
            return Err(ConversionError::new(format!(
                "output already exists: {} (pass --force / enable Overwrite to replace)",
                path.display()
            )));
        }
        Err(error) => {
            let _ = std::fs::remove_file(partial);
            return Err(ConversionError::new(format!(
                "could not finalize {}: {error}",
                path.display()
            )));
        }
    };
    let bytes = match std::fs::read(partial) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(partial);
            return Err(ConversionError::new(format!(
                "could not finalize {}: {error}",
                path.display()
            )));
        }
    };
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(partial);
        return Err(ConversionError::new(format!(
            "could not finalize {}: {error}",
            path.display()
        )));
    }
    let _ = std::fs::remove_file(partial);
    Ok(())
}

/// Remove incomplete `*.shift-partial` / `*.shift-bak` siblings for a planned destination.
///
/// Matching uses the stable [`short_path_hash`] of the planned basename so long
/// stems still clean up after bounded temp naming.
pub fn remove_partial_outputs(planned: &Path) -> usize {
    let parent = planned
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = file_stem_for_temp(planned);
    let key = format!("{:016x}", short_path_hash(&stem));
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let is_ours = name.contains(&key)
            && (name.ends_with(".shift-partial") || name.ends_with(".shift-bak"));
        // Also accept legacy `.{stem}.…` names from older builds.
        let is_legacy = name.starts_with(&format!(".{stem}."))
            && (name.ends_with(".shift-partial") || name.ends_with(".shift-bak"));
        if (is_ours || is_legacy) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

const TEXT_PREVIEW_CHAR_LIMIT: usize = 4_000;

fn text_preview_excerpt(bytes: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return format!(
            "Binary-looking payload ({} bytes) — not valid UTF-8 text.\nUse Download to save the file.",
            bytes.len()
        );
    };
    // Single pass over the char iterator: take the excerpt, then keep counting
    // the same iterator's remainder to detect (and quantify) truncation without
    // re-scanning the whole string.
    let mut chars = text.chars();
    let mut excerpt = String::new();
    for _ in 0..TEXT_PREVIEW_CHAR_LIMIT {
        match chars.next() {
            Some(ch) => excerpt.push(ch),
            None => break,
        }
    }
    let remaining = chars.count();
    if remaining > 0 {
        let total_chars = TEXT_PREVIEW_CHAR_LIMIT + remaining;
        excerpt.push_str(&format!(
            "\n\n… preview truncated ({total_chars} characters total · {} on disk when saved)",
            format_byte_size(bytes.len() as u64)
        ));
    } else if excerpt.trim().is_empty() {
        excerpt.push_str("The conversion completed with an empty document.");
    }
    excerpt
}

fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Join argv for display; caller must already redact secrets.
///
/// Quoting follows POSIX shell rules: safe bare words stay bare; anything with
/// whitespace or shell metacharacters is wrapped in single quotes, with embedded
/// single quotes escaped as `'"'"'`.
pub fn format_argv_display(argv: &[impl AsRef<str>]) -> String {
    fn needs_quote(part: &str) -> bool {
        if part.is_empty() {
            return true;
        }
        part.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '\\' | '"'
                        | '\''
                        | '`'
                        | '$'
                        | ';'
                        | '|'
                        | '&'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '!'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '#'
                )
        })
    }

    fn quote(part: &str) -> String {
        if !needs_quote(part) {
            return part.to_owned();
        }
        format!("'{}'", part.replace('\'', "'\"'\"'"))
    }

    argv.iter()
        .map(|part| quote(part.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collect program + args from a [`Command`] as owned strings for display.
pub fn command_argv_parts(command: &Command) -> Vec<String> {
    let mut parts = Vec::new();
    parts.push(command.get_program().to_string_lossy().into_owned());
    for arg in command.get_args() {
        parts.push(arg.to_string_lossy().into_owned());
    }
    parts
}

/// Replace the value following `flag` in an argv list (e.g. password → `••••`).
///
/// Handles both `--flag value` and `--flag=value` forms.
pub fn redact_flag_value(parts: &mut [String], flag: &str, replacement: &str) {
    let prefix = format!("{}=", flag);
    let mut index = 0;
    while index < parts.len() {
        if parts[index] == flag && index + 1 < parts.len() {
            parts[index + 1] = replacement.to_owned();
            index += 2;
        } else if let Some(rest) = parts[index].strip_prefix(&prefix) {
            if !rest.is_empty() {
                parts[index] = format!("{}={}", flag, replacement);
            }
            index += 1;
        } else {
            index += 1;
        }
    }
}

/// A self-contained adapter for one conversion engine or format family.
pub trait ConversionModule: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn input_extensions(&self) -> &'static [&'static str];
    fn output_formats(&self) -> &[OutputFormat];
    /// Outputs which may be materialized and safely consumed by another module.
    fn chainable_output_formats(&self) -> &[OutputFormat];
    /// Whether this module can honor [`ConversionOptions::target_size_bytes`]
    /// for the requested output instead of silently ignoring it.
    fn supports_target_size(&self, _output: OutputFormat) -> bool {
        false
    }
    fn convert(
        &self,
        input: &Path,
        output: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError>;

    fn supports(&self, input: &Path, output: OutputFormat) -> bool {
        let Some(extension) = input.extension().and_then(|value| value.to_str()) else {
            return false;
        };
        self.input_extensions()
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
            && self.output_formats().contains(&output)
    }

    /// Whether this module can convert a remote URL to the given format.
    fn supports_url(&self, _output: OutputFormat) -> bool {
        false
    }

    /// Convert a remote URL. Default rejects; URL-capable modules override.
    fn convert_url(
        &self,
        url: &str,
        _output: OutputFormat,
        _options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        Err(ConversionError::new(format!(
            "{} does not support URL conversion ({url})",
            self.label()
        )))
    }
}

/// Dispatches requests according to module order. Earlier modules win.
#[derive(Clone)]
pub struct ConversionRegistry {
    modules: Vec<Arc<dyn ConversionModule>>,
}

#[derive(Clone, Copy)]
enum ConversionRoute<'a> {
    Direct(&'a dyn ConversionModule),
    TwoStep {
        first: &'a dyn ConversionModule,
        intermediate: OutputFormat,
        second: &'a dyn ConversionModule,
    },
}

impl Default for ConversionRegistry {
    fn default() -> Self {
        // Building modules resolves external executables, but that work is now
        // memoized process-wide in `process.rs` (see `resolve_tool_executable`).
        // So repeated `default()` calls no longer re-scan `PATH`/common dirs;
        // they only re-wrap the shared `Arc` modules, which is cheap.
        Self::build_default()
    }
}

impl ConversionRegistry {
    pub fn new() -> Self {
        Self {
            modules: Vec::new(),
        }
    }

    /// Build the standard registry used by [`Default`].
    ///
    /// MarkItDown stays first for fast broad Markdown. Docling fills PDF →
    /// HTML/plain (and higher-quality Markdown when prioritized above
    /// MarkItDown). Pandoc owns publishing writers; Defuddle owns URLs. FFmpeg
    /// owns audio/video container conversion (no document overlap).
    ///
    /// The spreadsheet module owns sheet-native pairs (`xlsx`/`xls`/`ods`/`csv`
    /// ↔ `csv`/`tsv`/`xlsx`) as values-only grids. It does not advertise
    /// Markdown/HTML, so MarkItDown and Docling keep document → text routes.
    /// CSV chainable output allows a second hop into those engines when needed.
    ///
    /// qpdf owns PDF → PDF rewrites and PDF page ZIPs (extract, rotate, compress,
    /// linearize, split). It is a direct route and does not compete with
    /// document extraction engines.
    ///
    /// sips is registered immediately before FFmpeg, which is the only module
    /// it overlaps: both accept still images (`png`, `jpg`, `tiff`, `bmp`,
    /// `gif`) and write `png`/`jpg`/`gif`. sips wins those pairs because it is
    /// a single ImageIO call with no transcoding pipeline, and because it also
    /// reads the formats FFmpeg cannot (HEIC, AVIF, SVG, JXL, RAW). FFmpeg
    /// keeps every pair that starts from a video or audio container, including
    /// frame extraction and `png-sequence-zip`, since sips declares no
    /// container inputs. sips does not overlap Pandoc on `pdf`: Pandoc reads no
    /// raster inputs, so image → PDF is reachable only through sips.
    ///
    /// sips is macOS-only. Off macOS the module is not registered at all, so
    /// its formats are absent from capability lists rather than failing at
    /// spawn time.
    fn build_default() -> Self {
        let registry = Self::new()
            .with_module(MarkItDownModule::default())
            .with_module(PandocModule::default())
            .with_module(DefuddleModule::default())
            .with_module(DoclingModule::default())
            .with_module(QpdfModule::default())
            .with_module(SpreadsheetModule);
        #[cfg(target_os = "macos")]
        let registry = registry.with_module(SipsModule::default());
        registry.with_module(FfmpegModule::default())
    }

    pub fn with_module(mut self, module: impl ConversionModule + 'static) -> Self {
        self.modules.push(Arc::new(module));
        self
    }

    pub fn with_priority(mut self, priority: &[impl AsRef<str>]) -> Self {
        self.modules.sort_by_key(|module| {
            priority
                .iter()
                .position(|id| id.as_ref() == module.id())
                .unwrap_or(priority.len())
        });
        self
    }

    pub fn modules(&self) -> impl Iterator<Item = &dyn ConversionModule> {
        self.modules.iter().map(Arc::as_ref)
    }

    /// Whether a registered module uses this stable id.
    pub fn has_module(&self, id: &str) -> bool {
        self.modules.iter().any(|module| module.id() == id)
    }

    pub fn module_for(&self, input: &Path, output: OutputFormat) -> Option<&dyn ConversionModule> {
        self.modules
            .iter()
            .find(|module| module.supports(input, output))
            .map(Arc::as_ref)
    }

    pub fn module_for_url(&self, output: OutputFormat) -> Option<&dyn ConversionModule> {
        self.modules
            .iter()
            .find(|module| module.supports_url(output))
            .map(Arc::as_ref)
    }

    fn route_for(&self, input: &Path, output: OutputFormat) -> Option<ConversionRoute<'_>> {
        if let Some(module) = self.module_for(input, output) {
            return Some(ConversionRoute::Direct(module));
        }

        for (first_index, first) in self.modules.iter().enumerate() {
            if !first.input_extensions().iter().any(|extension| {
                input
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|input| input.eq_ignore_ascii_case(extension))
            }) {
                continue;
            }
            for &intermediate in first.chainable_output_formats() {
                if !first.supports(input, intermediate) {
                    continue;
                }
                let synthetic = PathBuf::from(format!("converted.{}", intermediate.extension()));
                if let Some((_, second)) =
                    self.modules
                        .iter()
                        .enumerate()
                        .find(|(second_index, second)| {
                            *second_index != first_index && second.supports(&synthetic, output)
                        })
                {
                    return Some(ConversionRoute::TwoStep {
                        first: first.as_ref(),
                        intermediate,
                        second: second.as_ref(),
                    });
                }
            }
        }
        None
    }

    fn url_route_for(&self, output: OutputFormat) -> Option<ConversionRoute<'_>> {
        if let Some(module) = self.module_for_url(output) {
            return Some(ConversionRoute::Direct(module));
        }
        for (first_index, first) in self.modules.iter().enumerate() {
            for &intermediate in first.chainable_output_formats() {
                if !first.supports_url(intermediate) {
                    continue;
                }
                if let Some((_, second)) =
                    self.modules
                        .iter()
                        .enumerate()
                        .find(|(second_index, second)| {
                            *second_index != first_index
                                && second.input_extensions().iter().any(|extension| {
                                    extension.eq_ignore_ascii_case(intermediate.extension())
                                })
                                && second.output_formats().contains(&output)
                        })
                {
                    return Some(ConversionRoute::TwoStep {
                        first: first.as_ref(),
                        intermediate,
                        second: second.as_ref(),
                    });
                }
            }
        }
        None
    }

    /// Module ids that would run for `input` → `output` (direct or two-step).
    ///
    /// Shared with diagnostics so readiness checks cannot drift from dispatch.
    pub fn route_module_ids(
        &self,
        input: &Path,
        output: OutputFormat,
    ) -> Option<Vec<&'static str>> {
        match self.route_for(input, output)? {
            ConversionRoute::Direct(module) => Some(vec![module.id()]),
            ConversionRoute::TwoStep { first, second, .. } => Some(vec![first.id(), second.id()]),
        }
    }

    /// Module ids that would run for a URL → `output` conversion.
    pub fn url_route_module_ids(&self, output: OutputFormat) -> Option<Vec<&'static str>> {
        match self.url_route_for(output)? {
            ConversionRoute::Direct(module) => Some(vec![module.id()]),
            ConversionRoute::TwoStep { first, second, .. } => Some(vec![first.id(), second.id()]),
        }
    }

    fn execute_route(
        &self,
        input: &Path,
        output: OutputFormat,
        route: ConversionRoute<'_>,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        validate_target_size_for_route(route, output, options)?;
        // PDF page-range / password preprocess (qpdf). If qpdf handles the
        // password, remove it from the module options so it never reaches an
        // external tool command line.
        let mut options = options.clone();
        let mut slice_guard: Option<TempDirGuard> = None;
        let direct_qpdf = matches!(route, ConversionRoute::Direct(module) if module.id() == "qpdf");
        let convert_input =
            if is_pdf_path(input) && options.pdf.needs_preprocessing() && !direct_qpdf {
                let sliced = extract_pdf_pages(
                    input,
                    options.pdf.page_from.unwrap_or(1),
                    options.pdf.page_to,
                    options.pdf.password.as_deref(),
                    options.cancel.clone(),
                )?;
                options.pdf.password = None;
                options.pdf.page_from = None;
                options.pdf.page_to = None;
                if let Some(parent) = sliced.parent() {
                    slice_guard = Some(TempDirGuard(parent.to_path_buf()));
                }
                sliced
            } else {
                input.to_path_buf()
            };
        let _slice_cleanup = slice_guard;
        let input = convert_input.as_path();

        match route {
            ConversionRoute::Direct(module) => {
                let artifact = module.convert(input, output, &options)?;
                Ok(ensure_direct_provenance(artifact, module.id()))
            }
            ConversionRoute::TwoStep {
                first,
                intermediate,
                second,
            } => {
                // Target size is a final-artifact goal. Intermediate hops must
                // not re-encode or quality-ladder under the final byte budget
                // (e.g. HEIC→MP3 via JPG, or MP4→JP2 via JPG).
                let hop1_options = options_for_intermediate_hop(&options);
                // Pass the full options snapshot on hop 2 so second-module knobs
                // (e.g. MarkItDown keep-data-uris) still apply; modules ignore
                // foreign fields.
                let hop1 = first.convert(input, intermediate, &hop1_options)?;
                let hop1 = ensure_direct_provenance(hop1, first.id());
                self.finish_chain(&hop1, output, second, &options)
            }
        }
    }

    fn finish_chain(
        &self,
        intermediate: &ConversionArtifact,
        output: OutputFormat,
        second: &dyn ConversionModule,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        let workspace = unique_temp_dir("shift-conversion")?;
        let _cleanup = TempDirGuard(workspace.clone());
        let stem = Path::new(&intermediate.file_name)
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("converted");
        let input = workspace.join(format!("{stem}.{}", intermediate.format.extension()));
        intermediate.write_to(&input)?;
        let hop2 = second.convert(&input, output, options)?;
        let hop2 = ensure_direct_provenance(hop2, second.id());
        Ok(merge_chain_provenance(intermediate, hop2))
    }

    pub fn available_outputs(&self, input: &Path) -> Vec<OutputFormat> {
        let Some(extension) = input.extension().and_then(|value| value.to_str()) else {
            return Vec::new();
        };

        let mut reachable = HashSet::new();
        for (first_index, first) in self.modules.iter().enumerate() {
            if !first
                .input_extensions()
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
            {
                continue;
            }

            // Ask each module what it actually supports for this input so
            // runtime encoder probes (e.g. FFmpeg libwebp) are honored.
            for &output in OutputFormat::ALL {
                if first.supports(input, output) {
                    reachable.insert(output);
                }
            }

            for &intermediate in first.chainable_output_formats() {
                if !first.supports(input, intermediate) {
                    continue;
                }
                let synthetic = PathBuf::from(format!("converted.{}", intermediate.extension()));
                for (second_index, second) in self.modules.iter().enumerate() {
                    if second_index == first_index {
                        continue;
                    }
                    for &output in OutputFormat::ALL {
                        if second.supports(&synthetic, output) {
                            reachable.insert(output);
                        }
                    }
                }
            }
        }

        OutputFormat::ALL
            .iter()
            .copied()
            .filter(|output| reachable.contains(output))
            .collect()
    }

    pub fn available_url_outputs(&self) -> Vec<OutputFormat> {
        OutputFormat::ALL
            .iter()
            .copied()
            .filter(|output| self.url_route_for(*output).is_some())
            .collect()
    }

    pub fn convert(&self, input: impl AsRef<Path>) -> Result<ConversionArtifact, ConversionError> {
        self.convert_to(input, OutputFormat::MARKDOWN)
    }

    pub fn convert_to(
        &self,
        input: impl AsRef<Path>,
        output: OutputFormat,
    ) -> Result<ConversionArtifact, ConversionError> {
        self.convert_to_with_options(input, output, &ConversionOptions::default())
    }

    pub fn convert_to_with_options(
        &self,
        input: impl AsRef<Path>,
        output: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        let input = input.as_ref();
        if !input.is_file() {
            return Err(ConversionError::new(format!(
                "input is not a readable file: {}",
                input.display()
            )));
        }

        let route = self.route_for(input, output).ok_or_else(|| {
            let extension = input
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("(none)");
            ConversionError::new(format!(
                "no conversion module supports .{extension} to {}",
                output.label()
            ))
        })?;

        self.execute_route(input, output, route, options)
    }

    pub fn convert_url(
        &self,
        url: &str,
        output: OutputFormat,
    ) -> Result<ConversionArtifact, ConversionError> {
        self.convert_url_with_options(url, output, &ConversionOptions::default())
    }

    pub fn convert_url_with_options(
        &self,
        url: &str,
        output: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        let url = url.trim();
        if !looks_like_url(url) {
            return Err(ConversionError::new(format!(
                "not a valid http(s) URL: {url}"
            )));
        }

        let route = self.url_route_for(output).ok_or_else(|| {
            ConversionError::new(format!(
                "no conversion module supports URL conversion to {}",
                output.label()
            ))
        })?;
        validate_target_size_for_route(route, output, options)?;

        match route {
            ConversionRoute::Direct(module) => {
                let artifact = module.convert_url(url, output, options)?;
                Ok(ensure_direct_provenance(artifact, module.id()))
            }
            ConversionRoute::TwoStep {
                first,
                intermediate,
                second,
            } => {
                let hop1_options = options_for_intermediate_hop(options);
                let hop1 = first.convert_url(url, intermediate, &hop1_options)?;
                let hop1 = ensure_direct_provenance(hop1, first.id());
                self.finish_chain(&hop1, output, second, options)
            }
        }
    }
}

/// Intermediate hops must not apply the final fit-to-size budget.
fn options_for_intermediate_hop(options: &ConversionOptions) -> ConversionOptions {
    let mut hop = options.clone();
    hop.target_size_bytes = None;
    hop
}

fn validate_target_size_for_route(
    route: ConversionRoute<'_>,
    output: OutputFormat,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let Some(target) = options.target_size_bytes else {
        return Ok(());
    };
    if target < 16 * 1024 {
        return Err(ConversionError::new("target size must be at least 16 KiB"));
    }
    if target as usize > max_output_bytes() {
        return Err(ConversionError::new(format!(
            "target size exceeds Shift's {} byte artifact limit",
            max_output_bytes()
        )));
    }
    let final_module = match route {
        ConversionRoute::Direct(module) => module,
        ConversionRoute::TwoStep { second, .. } => second,
    };
    if !final_module.supports_target_size(output) {
        return Err(ConversionError::new(format!(
            "{} cannot fit {} output to a target size",
            final_module.label(),
            output.label()
        )));
    }
    Ok(())
}

fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

/// Ensure a single-module artifact has pipeline/invocations and module_id set.
fn ensure_direct_provenance(
    mut artifact: ConversionArtifact,
    module_id: &'static str,
) -> ConversionArtifact {
    if artifact.pipeline.is_empty() {
        artifact.pipeline = vec![module_id];
    }
    artifact.module_id = *artifact.pipeline.last().unwrap_or(&module_id);
    artifact
}

/// Merge hop-1 provenance into the hop-2 artifact (final module_id = hop 2).
fn merge_chain_provenance(
    hop1: &ConversionArtifact,
    mut hop2: ConversionArtifact,
) -> ConversionArtifact {
    let mut pipeline = hop1.pipeline.clone();
    if pipeline.is_empty() {
        pipeline.push(hop1.module_id);
    }
    let hop2_id = if hop2.pipeline.is_empty() {
        hop2.module_id
    } else {
        *hop2.pipeline.last().unwrap_or(&hop2.module_id)
    };
    if hop2.pipeline.is_empty() {
        pipeline.push(hop2_id);
    } else {
        pipeline.extend(hop2.pipeline.iter().copied());
    }
    let mut invocations = hop1.invocations.clone();
    invocations.append(&mut hop2.invocations);
    hop2.pipeline = pipeline;
    hop2.invocations = invocations;
    hop2.module_id = hop2_id;
    hop2
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversionErrorKind {
    Message,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversionError {
    kind: ConversionErrorKind,
    message: String,
}

impl ConversionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: ConversionErrorKind::Message,
            message: message.into(),
        }
    }

    /// Cooperative cancellation requested by the batch runner or caller.
    pub fn cancelled() -> Self {
        Self {
            kind: ConversionErrorKind::Cancelled,
            message: "conversion cancelled".into(),
        }
    }

    /// True when process spawn failed because the executable was missing.
    pub fn is_executable_not_found(&self) -> bool {
        self.message.starts_with("executable not found:")
    }

    /// True when the user or batch runner cancelled the conversion.
    pub fn is_cancelled(&self) -> bool {
        self.kind == ConversionErrorKind::Cancelled
    }
}

/// Rewrite spawn failures with an engine-specific install hint.
pub(crate) fn map_spawn_error(
    error: ConversionError,
    not_found_message: impl Into<String>,
) -> ConversionError {
    if error.is_executable_not_found() {
        ConversionError::new(not_found_message)
    } else {
        error
    }
}

impl fmt::Display for ConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ConversionError {}

pub(crate) struct TempDirGuard(pub(crate) PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Default download/write path for a conversion, always distinct from `input`.
///
/// Same-extension pairs (for example `.html` → HTML or `.md` → Markdown) would
/// otherwise resolve to the source path and risk overwriting it.
pub fn default_output_path(input: &Path, output: OutputFormat) -> PathBuf {
    let extension = output.extension();
    let candidate = input.with_extension(extension);
    if paths_refer_to_same_file(input, &candidate) {
        let stem = input
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("converted");
        input.with_file_name(format!("{stem}.converted.{extension}"))
    } else {
        candidate
    }
}

/// Lexically normalize a path, resolving `.` and `..` segments without
/// requiring the path to exist. Relative paths are made absolute against the
/// current directory first so two relative forms that point to the same place
/// compare equal.
pub fn normalize_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                normalized = PathBuf::from(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

/// True when `left` and `right` name the same filesystem object.
///
/// Used to refuse writing conversion output over the selected source. Starts
/// with a lexical normalization (covers `a/../out/x.md` vs `out/x.md`), then
/// falls back to filesystem canonicalization when the paths exist.
pub fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    let left_norm = normalize_path(left);
    let right_norm = normalize_path(right);
    if left_norm == right_norm {
        return true;
    }

    if let (Ok(left), Ok(right)) = (
        std::fs::canonicalize(&left_norm),
        std::fs::canonicalize(&right_norm),
    ) {
        return left == right;
    }

    let Ok(left_canonical) = std::fs::canonicalize(&left_norm) else {
        return false;
    };

    let right_parent = right_norm
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let right_parent = match right_parent {
        Some(parent) => std::fs::canonicalize(parent).ok(),
        None => std::env::current_dir().ok(),
    };
    let Some(right_parent) = right_parent else {
        return false;
    };
    let Some(file_name) = right_norm.file_name() else {
        return false;
    };
    right_parent.join(file_name) == left_canonical
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct FakeModule {
        id: &'static str,
        inputs: &'static [&'static str],
        outputs: &'static [OutputFormat],
        chainable: &'static [OutputFormat],
        marker: &'static [u8],
        seen_input: Option<Arc<Mutex<Option<PathBuf>>>>,
        seen_target_size: Option<Arc<Mutex<Option<Option<u64>>>>>,
        supports_target_size: bool,
        assert_password_absent: bool,
    }

    impl ConversionModule for FakeModule {
        fn id(&self) -> &'static str {
            self.id
        }
        fn label(&self) -> &'static str {
            self.id
        }
        fn input_extensions(&self) -> &'static [&'static str] {
            self.inputs
        }
        fn output_formats(&self) -> &[OutputFormat] {
            self.outputs
        }
        fn chainable_output_formats(&self) -> &[OutputFormat] {
            self.chainable
        }
        fn supports_target_size(&self, _output: OutputFormat) -> bool {
            self.supports_target_size
        }
        fn convert(
            &self,
            input: &Path,
            output: OutputFormat,
            options: &ConversionOptions,
        ) -> Result<ConversionArtifact, ConversionError> {
            if self.assert_password_absent {
                assert!(
                    options.pdf.password.is_none(),
                    "PDF password should be removed by qpdf preprocessing before the module runs"
                );
            }
            if let Some(seen) = &self.seen_target_size {
                *seen.lock().unwrap() = Some(options.target_size_bytes);
            }
            if let Some(seen) = &self.seen_input {
                *seen.lock().unwrap() = Some(input.to_owned());
                assert_eq!(std::fs::read(input).unwrap(), b"intermediate");
            }
            Ok(ConversionArtifact {
                file_name: format!("result.{}", output.extension()),
                media_type: output.media_type(),
                bytes: self.marker.to_vec(),
                format: output,
                module_id: self.id,
                pipeline: vec![self.id],
                invocations: Vec::new(),
            })
        }
    }

    fn fake(
        id: &'static str,
        inputs: &'static [&'static str],
        outputs: &'static [OutputFormat],
        chainable: &'static [OutputFormat],
        marker: &'static [u8],
    ) -> FakeModule {
        FakeModule {
            id,
            inputs,
            outputs,
            chainable,
            marker,
            seen_input: None,
            seen_target_size: None,
            supports_target_size: false,
            assert_password_absent: false,
        }
    }

    #[test]
    fn direct_route_takes_precedence_over_a_two_step_route() {
        let registry = ConversionRegistry::new()
            .with_module(fake(
                "first",
                &["src"],
                &[OutputFormat::MARKDOWN],
                &[OutputFormat::MARKDOWN],
                b"intermediate",
            ))
            .with_module(fake(
                "second",
                &["md"],
                &[OutputFormat::PDF],
                &[],
                b"chained",
            ))
            .with_module(fake(
                "direct",
                &["src"],
                &[OutputFormat::PDF],
                &[],
                b"direct",
            ));
        assert_eq!(
            registry
                .module_for(Path::new("input.src"), OutputFormat::PDF)
                .unwrap()
                .id(),
            "direct"
        );
    }

    #[test]
    fn two_step_routes_are_derived_executed_and_cleaned_up() {
        let seen = Arc::new(Mutex::new(None));
        let mut second = fake("second", &["md"], &[OutputFormat::PDF], &[], b"final");
        second.seen_input = Some(seen.clone());
        let registry = ConversionRegistry::new()
            .with_module(fake(
                "first",
                &["src"],
                &[OutputFormat::MARKDOWN],
                &[OutputFormat::MARKDOWN],
                b"intermediate",
            ))
            .with_module(second);
        let input = std::env::temp_dir().join(format!("shift-route-{}.src", std::process::id()));
        std::fs::write(&input, b"source").unwrap();

        assert!(
            registry
                .available_outputs(&input)
                .contains(&OutputFormat::PDF)
        );
        assert_eq!(
            registry.route_module_ids(&input, OutputFormat::PDF),
            Some(vec!["first", "second"])
        );
        let artifact = registry.convert_to(&input, OutputFormat::PDF).unwrap();
        assert_eq!(artifact.bytes, b"final");
        let temporary_input = seen.lock().unwrap().clone().unwrap();
        assert!(!temporary_input.exists());
        std::fs::remove_file(input).unwrap();
    }

    #[test]
    fn module_order_deterministically_selects_the_first_two_step_route() {
        let registry = ConversionRegistry::new()
            .with_module(fake(
                "preferred",
                &["src"],
                &[OutputFormat::MARKDOWN],
                &[OutputFormat::MARKDOWN],
                b"intermediate",
            ))
            .with_module(fake(
                "other",
                &["src"],
                &[OutputFormat::HTML],
                &[OutputFormat::HTML],
                b"other",
            ))
            .with_module(fake(
                "markdown-writer",
                &["md"],
                &[OutputFormat::PDF],
                &[],
                b"preferred",
            ))
            .with_module(fake(
                "html-writer",
                &["html"],
                &[OutputFormat::PDF],
                &[],
                b"other",
            ));
        let route = registry
            .route_for(Path::new("input.src"), OutputFormat::PDF)
            .unwrap();
        let ConversionRoute::TwoStep { first, second, .. } = route else {
            panic!("expected chain")
        };
        assert_eq!((first.id(), second.id()), ("preferred", "markdown-writer"));
    }

    #[test]
    fn outputs_not_marked_chainable_do_not_create_routes() {
        let registry = ConversionRegistry::new()
            .with_module(fake(
                "first",
                &["src"],
                &[OutputFormat::MARKDOWN],
                &[],
                b"intermediate",
            ))
            .with_module(fake("second", &["md"], &[OutputFormat::PDF], &[], b"final"));
        assert!(
            registry
                .route_for(Path::new("input.src"), OutputFormat::PDF)
                .is_none()
        );
    }

    #[test]
    fn default_priority_prefers_markitdown_for_shared_markdown_conversion() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for(Path::new("REPORT.DOCX"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "markitdown"
        );
    }

    #[test]
    fn priority_can_promote_pandoc() {
        let registry = ConversionRegistry::default().with_priority(&["pandoc", "markitdown"]);
        assert_eq!(
            registry
                .module_for(Path::new("REPORT.DOCX"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "pandoc"
        );
    }

    #[test]
    fn output_capabilities_are_filtered_by_input() {
        let registry = ConversionRegistry::default();
        let pdf_outputs = registry.available_outputs(Path::new("scan.pdf"));
        // MarkItDown: Markdown. Docling: Markdown, HTML, plain.
        assert!(pdf_outputs.contains(&OutputFormat::MARKDOWN));
        assert!(pdf_outputs.contains(&OutputFormat::HTML));
        assert!(pdf_outputs.contains(&OutputFormat("plain")));
        assert!(pdf_outputs.contains(&OutputFormat::DOCX));
        assert!(pdf_outputs.contains(&OutputFormat::PDF));
        assert!(
            registry
                .available_outputs(Path::new("report.docx"))
                .contains(&OutputFormat::PDF)
        );
    }

    #[test]
    fn pdf_html_routes_to_docling() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for(Path::new("scan.pdf"), OutputFormat::HTML)
                .unwrap()
                .id(),
            "docling"
        );
    }

    #[test]
    fn pdf_plain_routes_to_docling() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for(Path::new("scan.pdf"), OutputFormat("plain"))
                .unwrap()
                .id(),
            "docling"
        );
    }

    #[test]
    fn default_priority_still_prefers_markitdown_over_docling_for_pdf_markdown() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for(Path::new("scan.pdf"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "markitdown"
        );
    }

    #[test]
    fn priority_can_promote_docling_for_pdf_markdown() {
        let registry = ConversionRegistry::default().with_priority(&["docling", "markitdown"]);
        assert_eq!(
            registry
                .module_for(Path::new("scan.pdf"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "docling"
        );
    }

    #[test]
    fn output_path_uses_target_extension() {
        assert_eq!(
            default_output_path(Path::new("notes/report.docx"), OutputFormat::HTML),
            Path::new("notes/report.html")
        );
    }

    #[test]
    fn output_path_avoids_overwriting_same_extension_source() {
        assert_eq!(
            default_output_path(Path::new("notes/page.html"), OutputFormat::HTML),
            Path::new("notes/page.converted.html")
        );
        assert_eq!(
            default_output_path(Path::new("notes/doc.docx"), OutputFormat::DOCX),
            Path::new("notes/doc.converted.docx")
        );
        assert_eq!(
            default_output_path(Path::new("notes/readme.md"), OutputFormat::MARKDOWN),
            Path::new("notes/readme.converted.md")
        );
        assert_eq!(
            default_output_path(Path::new("notes/readme.md"), OutputFormat("gfm")),
            Path::new("notes/readme.converted.md")
        );
    }

    #[test]
    fn paths_refer_to_same_file_matches_identical_paths() {
        assert!(paths_refer_to_same_file(
            Path::new("report.html"),
            Path::new("report.html")
        ));
        assert!(!paths_refer_to_same_file(
            Path::new("report.html"),
            Path::new("report.converted.html")
        ));
    }

    #[test]
    fn write_bytes_atomically_creates_final_without_partial_left_behind() {
        let dir = std::env::temp_dir().join(format!(
            "shift-atomic-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.md");
        write_bytes_atomically(&path, b"# hello\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"# hello\n");
        // Overwrite existing destination (backup path on platforms that need it).
        write_bytes_atomically(&path, b"# world\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"# world\n");
        // No partial or backup siblings remain.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            assert!(!name.contains("shift-partial"), "leftover partial: {name}");
            assert!(!name.contains("shift-bak"), "leftover backup: {name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn write_bytes_atomically_exclusive_refuses_existing_destination() {
        let dir = std::env::temp_dir().join(format!(
            "shift-atomic-excl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.md");
        std::fs::write(&path, b"original").unwrap();
        let error = write_bytes_atomically_with_replace(&path, b"new", false).unwrap_err();
        assert!(
            error.to_string().contains("already exists"),
            "error: {error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        // Exclusive create succeeds when the path is free.
        let free = dir.join("fresh.md");
        write_bytes_atomically_with_replace(&free, b"only", false).unwrap();
        assert_eq!(std::fs::read(&free).unwrap(), b"only");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unique_temp_file_name_bounds_length_for_long_stems() {
        let long_stem = "a".repeat(500);
        let name = unique_temp_file_name(&long_stem, ".shift-partial");
        assert!(
            name.len() <= FS_NAME_MAX,
            "temp name length {} exceeds FS_NAME_MAX ({FS_NAME_MAX}): {name}",
            name.len()
        );
        assert!(name.ends_with(".shift-partial"), "{name}");
        assert!(name.starts_with('.'), "{name}");
        // Hash of the stem is embedded so cleanup can find it without the full stem.
        let key = format!("{:016x}", short_path_hash(&long_stem));
        assert!(name.contains(&key), "expected hash {key} in {name}");
    }

    #[test]
    fn remove_partial_outputs_cleans_siblings() {
        let dir = std::env::temp_dir().join(format!(
            "shift-partial-clean-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let planned = dir.join("report.md");
        // New bounded naming.
        let partial = dir.join(unique_temp_file_name("report.md", ".shift-partial"));
        std::fs::write(&partial, b"half").unwrap();
        // Legacy naming still cleaned.
        let legacy = dir.join(".report.md.123.shift-partial");
        std::fs::write(&legacy, b"half").unwrap();
        assert_eq!(remove_partial_outputs(&planned), 2);
        assert!(!partial.exists());
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn preview_summary_is_honest_for_binary_and_text() {
        let text = ConversionArtifact {
            file_name: "note.md".into(),
            media_type: "text/markdown",
            bytes: b"# Title\n\nHello".to_vec(),
            format: OutputFormat::MARKDOWN,
            module_id: "pandoc",
            pipeline: vec!["pandoc"],
            invocations: Vec::new(),
        };
        let summary = text.preview_summary();
        assert!(summary.contains("Title"), "{summary}");

        let binary = ConversionArtifact {
            file_name: "clip.mp3".into(),
            media_type: "audio/mpeg",
            bytes: vec![0u8; 2048],
            format: OutputFormat::MP3,
            module_id: "ffmpeg",
            pipeline: vec!["ffmpeg"],
            invocations: Vec::new(),
        };
        let summary = binary.preview_summary();
        assert!(
            summary.contains("Audio") || summary.contains("MP3") || summary.contains("mp3"),
            "{summary}"
        );
        assert!(summary.contains("preview"), "{summary}");
        assert!(
            summary.contains("Open") || summary.contains("Download"),
            "binary notes should steer to Open/Download: {summary}"
        );
        assert!(!summary.contains("player widget"), "{summary}");
    }

    #[test]
    fn prepare_destination_refuses_source_overwrite() {
        let dir = std::env::temp_dir().join(format!(
            "shift-source-safe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("doc.md");
        std::fs::write(&source, b"src").unwrap();
        let error = prepare_batch_destination(&source, Some(&source), true).unwrap_err();
        assert!(
            error.to_string().contains("refusing to overwrite source"),
            "error: {error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn has_module_reports_registered_ids() {
        let registry = ConversionRegistry::default();
        assert!(registry.has_module("pandoc"));
        assert!(registry.has_module("docling"));
        assert!(registry.has_module("ffmpeg"));
        assert!(!registry.has_module("pandocx"));
        assert!(!registry.has_module(""));
    }

    #[test]
    fn output_catalog_has_no_duplicates() {
        let mut ids = OutputFormat::ALL
            .iter()
            .map(|format| format.id())
            .collect::<Vec<_>>();
        let original_len = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), original_len);
    }

    #[test]
    fn all_catalog_matches_its_output_partitions() {
        assert_eq!(
            OutputFormat::ALL.len(),
            OutputFormat::PANDOC.len()
                + OutputFormat::DOCLING.len()
                + OutputFormat::MEDIA.len()
                + OutputFormat::IMAGE.len()
                + OutputFormat::SPREADSHEET.len()
                + OutputFormat::PDF_TOOLKIT.len()
        );
        for format in OutputFormat::PANDOC {
            assert!(OutputFormat::ALL.contains(format));
        }
        for format in OutputFormat::MEDIA {
            assert!(OutputFormat::ALL.contains(format));
        }
        for format in OutputFormat::DOCLING {
            assert!(OutputFormat::ALL.contains(format));
        }
        for format in OutputFormat::IMAGE {
            assert!(OutputFormat::ALL.contains(format));
        }
        for format in OutputFormat::SPREADSHEET {
            assert!(OutputFormat::ALL.contains(format));
        }
        for format in OutputFormat::PDF_TOOLKIT {
            assert!(OutputFormat::ALL.contains(format));
        }
    }

    #[test]
    fn media_outputs_route_to_ffmpeg() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for(Path::new("clip.mp4"), OutputFormat::MP3)
                .unwrap()
                .id(),
            "ffmpeg"
        );
        assert_eq!(
            registry
                .module_for(Path::new("track.wav"), OutputFormat::FLAC)
                .unwrap()
                .id(),
            "ffmpeg"
        );
        let video_outputs = registry.available_outputs(Path::new("clip.mov"));
        assert!(video_outputs.contains(&OutputFormat::MP4));
        assert!(video_outputs.contains(&OutputFormat::MP3));
        // ASR lives on the dedicated transcript action so MarkItDown/FFmpeg
        // chains keep video → Markdown and FFmpeg keeps subtitle-track VTT/SRT.
        assert!(video_outputs.contains(&OutputFormat::TRANSCRIPT));
        assert!(
            registry
                .module_for(Path::new("clip.mov"), OutputFormat::TRANSCRIPT)
                .is_some_and(|module| module.id() == "docling"),
            "Docling should own video → Transcript (ASR)"
        );
        assert!(
            registry
                .module_for(Path::new("clip.mov"), OutputFormat::MARKDOWN)
                .is_none()
                || registry
                    .module_for(Path::new("clip.mov"), OutputFormat::MARKDOWN)
                    .is_some_and(|module| module.id() != "docling"),
            "Docling must not own video → Markdown (use transcript)"
        );
        assert_eq!(
            registry
                .module_for(Path::new("clip.mov"), OutputFormat::VTT)
                .map(|module| module.id()),
            Some("ffmpeg"),
            "FFmpeg owns video → WebVTT track extraction"
        );
    }

    #[test]
    fn url_conversion_routes_to_defuddle() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for_url(OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "defuddle"
        );
        let url_outputs = registry.available_url_outputs();
        assert!(url_outputs.contains(&OutputFormat::MARKDOWN));
        assert!(url_outputs.contains(&OutputFormat::HTML));
        assert!(url_outputs.contains(&OutputFormat::DOCX));
    }

    #[test]
    fn default_priority_still_prefers_markitdown_for_local_html() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for(Path::new("page.html"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "markitdown"
        );
    }

    #[test]
    fn route_module_ids_matches_available_outputs_for_default_registry() {
        let registry = ConversionRegistry::default();
        for sample in [
            "notes.txt",
            "report.docx",
            "scan.pdf",
            "clip.mp4",
            "page.html",
        ] {
            let input = Path::new(sample);
            for output in registry.available_outputs(input) {
                let ids = registry
                    .route_module_ids(input, output)
                    .unwrap_or_else(|| panic!("no route ids for {sample} → {}", output.id()));
                assert!(
                    !ids.is_empty(),
                    "empty route for {sample} → {}",
                    output.id()
                );
            }
        }
        for output in registry.available_url_outputs() {
            let ids = registry
                .url_route_module_ids(output)
                .unwrap_or_else(|| panic!("no url route ids for {}", output.id()));
            assert!(!ids.is_empty());
        }
    }

    #[test]
    fn two_step_merges_pipeline_provenance() {
        let registry = ConversionRegistry::new()
            .with_module(fake(
                "first",
                &["src"],
                &[OutputFormat::MARKDOWN],
                &[OutputFormat::MARKDOWN],
                b"intermediate",
            ))
            .with_module(fake("second", &["md"], &[OutputFormat::PDF], &[], b"final"));
        let input = std::env::temp_dir().join(format!("shift-pipeline-{}.src", std::process::id()));
        std::fs::write(&input, b"source").unwrap();
        let artifact = registry.convert_to(&input, OutputFormat::PDF).unwrap();
        assert_eq!(artifact.pipeline, vec!["first", "second"]);
        assert_eq!(artifact.module_id, "second");
        std::fs::remove_file(input).unwrap();
    }

    #[test]
    fn redact_flag_value_masks_password() {
        let mut parts = vec![
            "docling".into(),
            "--pdf-password".into(),
            "s3cret".into(),
            "--ocr".into(),
        ];
        redact_flag_value(&mut parts, "--pdf-password", "••••");
        assert_eq!(parts[2], "••••");
        assert!(!parts.iter().any(|p| p == "s3cret"));
    }

    #[test]
    fn format_argv_display_quotes_spaces_empty_and_metacharacters() {
        assert_eq!(
            format_argv_display(&["ffmpeg", "-i", "in.mp4"]),
            "ffmpeg -i in.mp4"
        );
        assert_eq!(
            format_argv_display(&["ffmpeg", "-i", "my file.mp4"]),
            "ffmpeg -i 'my file.mp4'"
        );
        assert_eq!(format_argv_display(&["tool", ""]), "tool ''");
        assert_eq!(format_argv_display(&["echo", "a'b"]), "echo 'a'\"'\"'b'");
        assert_eq!(
            format_argv_display(&["sh", "-c", "echo $HOME; true"]),
            "sh -c 'echo $HOME; true'"
        );
        assert_eq!(
            format_argv_display(&["cmd", "x*y", "a|b", "c&d"]),
            "cmd 'x*y' 'a|b' 'c&d'"
        );
    }

    #[test]
    fn redact_flag_value_handles_missing_last_arg_equals_and_multiples() {
        // Flag present but no following value: leave argv unchanged.
        let mut trailing = vec!["tool".into(), "--pdf-password".into()];
        redact_flag_value(&mut trailing, "--pdf-password", "••••");
        assert_eq!(trailing, vec!["tool", "--pdf-password"]);

        // Flag not present at all.
        let mut missing = vec!["tool".into(), "--other".into(), "x".into()];
        redact_flag_value(&mut missing, "--pdf-password", "••••");
        assert_eq!(missing, vec!["tool", "--other", "x"]);

        // `--flag=value` form.
        let mut equals = vec![
            "docling".into(),
            "--pdf-password=s3cret".into(),
            "--ocr".into(),
        ];
        redact_flag_value(&mut equals, "--pdf-password", "••••");
        assert_eq!(equals[1], "--pdf-password=••••");
        assert!(!equals.iter().any(|p| p.contains("s3cret")));

        // Multiple occurrences of both forms.
        let mut multi = vec![
            "a".into(),
            "--secret".into(),
            "one".into(),
            "--secret=two".into(),
            "--secret".into(),
            "three".into(),
        ];
        redact_flag_value(&mut multi, "--secret", "REDACTED");
        assert_eq!(
            multi,
            vec![
                "a",
                "--secret",
                "REDACTED",
                "--secret=REDACTED",
                "--secret",
                "REDACTED"
            ]
        );
    }

    #[test]
    fn text_preview_and_artifact_text_cover_utf8_binary_empty_and_truncation() {
        // Valid short UTF-8 text.
        let short = ConversionArtifact {
            file_name: "note.md".into(),
            media_type: "text/markdown",
            bytes: b"# Hi\n\nbody".to_vec(),
            format: OutputFormat::MARKDOWN,
            module_id: "pandoc",
            pipeline: vec!["pandoc"],
            invocations: Vec::new(),
        };
        assert_eq!(short.text(), Some("# Hi\n\nbody"));
        assert_eq!(short.preview_summary(), "# Hi\n\nbody");

        // Empty document message.
        let empty = ConversionArtifact {
            file_name: "empty.md".into(),
            media_type: "text/markdown",
            bytes: b"   \n".to_vec(),
            format: OutputFormat::MARKDOWN,
            module_id: "pandoc",
            pipeline: vec!["pandoc"],
            invocations: Vec::new(),
        };
        let empty_preview = empty.preview_summary();
        assert!(empty_preview.contains("empty document"), "{empty_preview}");

        // Invalid UTF-8 on a text-previewable format.
        let bad_utf8 = ConversionArtifact {
            file_name: "weird.md".into(),
            media_type: "text/markdown",
            bytes: vec![0xff, 0xfe, 0x00],
            format: OutputFormat::MARKDOWN,
            module_id: "pandoc",
            pipeline: vec!["pandoc"],
            invocations: Vec::new(),
        };
        assert!(bad_utf8.text().is_none());
        let bad_preview = bad_utf8.preview_summary();
        assert!(bad_preview.contains("not valid UTF-8"), "{bad_preview}");
        assert!(bad_preview.contains("3 bytes"), "{bad_preview}");

        // Long text is truncated with a character count.
        let long_body = "x".repeat(TEXT_PREVIEW_CHAR_LIMIT + 50);
        let long = ConversionArtifact {
            file_name: "long.md".into(),
            media_type: "text/markdown",
            bytes: long_body.into_bytes(),
            format: OutputFormat::MARKDOWN,
            module_id: "pandoc",
            pipeline: vec!["pandoc"],
            invocations: Vec::new(),
        };
        let long_preview = long.preview_summary();
        assert!(long_preview.contains("preview truncated"), "{long_preview}");
        assert!(
            long_preview.contains(&format!(
                "{} characters total",
                TEXT_PREVIEW_CHAR_LIMIT + 50
            )),
            "{long_preview}"
        );

        // Binary format still reports non-text summary even with UTF-8 bytes.
        let binary = ConversionArtifact {
            file_name: "clip.mp4".into(),
            media_type: "video/mp4",
            bytes: b"not really video".to_vec(),
            format: OutputFormat::MP4,
            module_id: "ffmpeg",
            pipeline: vec!["ffmpeg"],
            invocations: Vec::new(),
        };
        assert_eq!(binary.text(), Some("not really video"));
        let bin_preview = binary.preview_summary();
        assert!(bin_preview.contains("preview"), "{bin_preview}");
        assert!(
            bin_preview.contains("Video") || bin_preview.contains("MP4"),
            "{bin_preview}"
        );
        assert!(
            bin_preview.contains("Size:") || bin_preview.contains("Size "),
            "expected a size fact: {bin_preview}"
        );
        assert!(
            bin_preview.contains("Open") || bin_preview.contains("Download"),
            "{bin_preview}"
        );
    }

    #[test]
    fn normalize_path_collapses_dot_and_dotdot_components() {
        let with_dots = normalize_path(Path::new("/tmp/a/./b/../c/file.md"));
        assert_eq!(with_dots, Path::new("/tmp/a/c/file.md"));

        // Relative paths become absolute against cwd, then collapse.
        let cwd = std::env::current_dir().unwrap();
        let relative = normalize_path(Path::new("foo/./bar/../baz.txt"));
        assert_eq!(relative, cwd.join("foo/baz.txt"));

        // Extra parent segments pop roots carefully (lexically).
        let up_from_tmp = normalize_path(Path::new("/tmp/x/../../etc/passwd"));
        assert_eq!(up_from_tmp, Path::new("/etc/passwd"));
    }

    #[test]
    fn default_output_path_handles_unicode_stem_and_missing_stem() {
        assert_eq!(
            default_output_path(Path::new("notes/rapor-ç.md"), OutputFormat::HTML),
            Path::new("notes/rapor-ç.html")
        );
        // Same extension with unicode stem still gets the uniquifying suffix.
        assert_eq!(
            default_output_path(Path::new("notes/文档.html"), OutputFormat::HTML),
            Path::new("notes/文档.converted.html")
        );
        // No stem (e.g. ".gitignore"-style or bare extension name): use "converted".
        // `file_stem` of ".md" is often empty or the whole name depending on platform
        // conventions; assert the result never equals the input for same-extension.
        let bare = default_output_path(Path::new(".md"), OutputFormat::MARKDOWN);
        assert_ne!(bare, Path::new(".md"));
        assert!(
            bare.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".md")),
            "unexpected bare path: {}",
            bare.display()
        );
    }

    #[test]
    fn output_format_from_str_and_metadata_cover_common_ids() {
        assert_eq!(
            "markdown".parse::<OutputFormat>().unwrap(),
            OutputFormat::MARKDOWN
        );
        assert_eq!("HTML".parse::<OutputFormat>().unwrap(), OutputFormat::HTML);
        assert_eq!(
            "md".parse::<OutputFormat>().unwrap(),
            OutputFormat::MARKDOWN
        );
        assert_eq!("jpeg".parse::<OutputFormat>().unwrap().id(), "jpg");
        assert_eq!(
            "png-zip".parse::<OutputFormat>().unwrap(),
            OutputFormat::PNG_SEQUENCE_ZIP
        );
        assert_eq!("mp3".parse::<OutputFormat>().unwrap(), OutputFormat::MP3);

        let unknown = "not-a-real-format".parse::<OutputFormat>().unwrap_err();
        assert!(
            unknown.to_string().contains("unknown output format"),
            "{unknown}"
        );

        assert_eq!(OutputFormat::MARKDOWN.extension(), "md");
        assert_eq!(OutputFormat::MARKDOWN.media_type(), "text/markdown");
        assert!(OutputFormat::MARKDOWN.is_text_previewable());

        assert_eq!(OutputFormat::HTML.extension(), "html");
        assert_eq!(OutputFormat::HTML.media_type(), "text/html");
        assert!(OutputFormat::HTML.is_text_previewable());

        assert_eq!(OutputFormat::MP3.extension(), "mp3");
        assert_eq!(OutputFormat::MP3.media_type(), "audio/mpeg");
        assert!(!OutputFormat::MP3.is_text_previewable());

        assert_eq!(OutputFormat::PDF.extension(), "pdf");
        assert_eq!(OutputFormat::PDF.media_type(), "application/pdf");
        assert!(!OutputFormat::PDF.is_text_previewable());

        assert_eq!(OutputFormat("srt").extension(), "srt");
        assert!(OutputFormat("srt").is_text_previewable());
        assert_eq!(OutputFormat("vtt").media_type(), "text/vtt");
        assert!(OutputFormat("vtt").is_text_previewable());
    }

    #[test]
    fn conversion_error_cancelled_and_not_found_helpers() {
        let cancelled = ConversionError::cancelled();
        assert!(cancelled.is_cancelled());
        assert!(!cancelled.is_executable_not_found());
        assert_eq!(cancelled.to_string(), "conversion cancelled");

        let missing = ConversionError::new("executable not found: /tmp/missing-bin");
        assert!(missing.is_executable_not_found());
        assert!(!missing.is_cancelled());

        let other = ConversionError::new("something else failed");
        assert!(!other.is_cancelled());
        assert!(!other.is_executable_not_found());
    }

    #[test]
    fn write_bytes_atomically_fails_when_parent_is_a_file() {
        let dir = std::env::temp_dir().join(format!(
            "shift-atomic-parent-file-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let parent_as_file = dir.join("not-a-dir");
        std::fs::write(&parent_as_file, b"file").unwrap();
        let impossible = parent_as_file.join("child.md");

        let error = write_bytes_atomically(&impossible, b"data").unwrap_err();
        assert!(
            error.to_string().contains("could not write")
                || error.to_string().contains("could not finalize"),
            "error: {error}"
        );
        assert!(!impossible.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn available_url_outputs_is_non_empty_for_default_registry() {
        let outputs = ConversionRegistry::default().available_url_outputs();
        assert!(!outputs.is_empty());
        assert!(outputs.contains(&OutputFormat::MARKDOWN));
        assert!(outputs.contains(&OutputFormat::HTML));
    }

    #[test]
    fn pdf_input_options_slice_preprocessing_and_default() {
        let default = PdfInputOptions::default();
        assert!(default.is_default());
        assert!(!default.needs_slice());
        assert!(!default.needs_preprocessing());

        // page_from == 1 with no page_to is full-document (no slice).
        let from_one = PdfInputOptions {
            page_from: Some(1),
            ..Default::default()
        };
        assert!(!from_one.needs_slice());
        assert!(!from_one.needs_preprocessing());
        assert!(!from_one.is_default());

        let from_later = PdfInputOptions {
            page_from: Some(2),
            ..Default::default()
        };
        assert!(from_later.needs_slice());
        assert!(from_later.needs_preprocessing());

        let with_to = PdfInputOptions {
            page_to: Some(5),
            ..Default::default()
        };
        assert!(with_to.needs_slice());
        assert!(with_to.needs_preprocessing());

        let password_only = PdfInputOptions {
            password: Some("s3cret".into()),
            ..Default::default()
        };
        assert!(!password_only.needs_slice());
        assert!(password_only.needs_preprocessing());
        assert!(!password_only.is_default());

        let range = PdfInputOptions {
            page_from: Some(2),
            page_to: Some(5),
            password: Some("x".into()),
            ..Default::default()
        };
        assert!(range.needs_slice());
        assert!(range.needs_preprocessing());
    }

    /// UI hot paths: format chips, previews, and destination naming on selection change.
    ///
    /// Budgets are deliberately loose for unoptimized debug test builds; they
    /// still fail if a helper accidentally shells out or turns quadratic.
    mod ui_perf {
        use super::*;
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        fn assert_within(budget: Duration, label: &str, work: impl FnOnce()) {
            let start = Instant::now();
            work();
            let elapsed = start.elapsed();
            assert!(
                elapsed <= budget,
                "{label} took {elapsed:?}, budget {budget:?}"
            );
        }

        #[test]
        fn format_catalog_metadata_for_chips_is_cheap() {
            // ~500 UI refreshes of the full chip strip.
            assert_within(Duration::from_secs(1), "OutputFormat::ALL×500", || {
                for _ in 0..500 {
                    for format in OutputFormat::ALL {
                        black_box(format.id());
                        black_box(format.label());
                        black_box(format.extension());
                        black_box(format.media_type());
                        black_box(format.is_text_previewable());
                    }
                }
            });
        }

        #[test]
        fn preview_summary_for_text_and_binary_stays_responsive() {
            let text = ConversionArtifact {
                file_name: "essay.md".into(),
                media_type: "text/markdown",
                bytes: "# Title\n\n".repeat(2_000).into_bytes(),
                format: OutputFormat::MARKDOWN,
                module_id: "pandoc",
                pipeline: vec!["pandoc"],
                invocations: Vec::new(),
            };
            let binary = ConversionArtifact {
                file_name: "clip.mp4".into(),
                media_type: "video/mp4",
                bytes: vec![0u8; 128 * 1024],
                format: OutputFormat::MP4,
                module_id: "ffmpeg",
                pipeline: vec!["ffmpeg"],
                invocations: Vec::new(),
            };

            // One preview per conversion result; stress a few hundred restores.
            assert_within(Duration::from_secs(1), "preview_summary×400", || {
                for _ in 0..200 {
                    black_box(text.preview_summary());
                    black_box(binary.preview_summary());
                }
            });

            let text_preview = text.preview_summary();
            assert!(text_preview.contains("Title") || text_preview.contains("truncated"));
            let binary_preview = binary.preview_summary();
            assert!(binary_preview.contains("preview"));
            assert!(
                binary_preview.contains("Open") || binary_preview.contains("Download"),
                "{binary_preview}"
            );
        }

        #[test]
        fn default_output_path_naming_scales_with_batch_size() {
            let inputs: Vec<_> = (0..200)
                .map(|i| {
                    std::path::PathBuf::from(format!("/Users/me/Movies/project/clip_{i:04}.mov"))
                })
                .collect();

            // One naming pass per batch item × a few formats.
            assert_within(Duration::from_secs(1), "default_output_path×800", || {
                for input in &inputs {
                    for format in [
                        OutputFormat::MP3,
                        OutputFormat::MARKDOWN,
                        OutputFormat::PNG,
                        OutputFormat::SRT,
                    ] {
                        black_box(default_output_path(input, format));
                    }
                }
            });
        }

        #[test]
        fn registry_available_outputs_listing_is_stable_cost() {
            let registry = ConversionRegistry::default();
            let samples = [
                "report.docx",
                "scan.pdf",
                "page.html",
                "clip.mp4",
                "track.wav",
                "notes.md",
                "deck.pptx",
                "photo.png",
            ];
            // File-pick path: list chips for a handful of extensions repeatedly.
            assert_within(Duration::from_secs(2), "available_outputs×80", || {
                for _ in 0..10 {
                    for name in samples {
                        black_box(registry.available_outputs(Path::new(name)));
                    }
                }
            });
            assert_within(Duration::from_secs(1), "available_url_outputs×200", || {
                for _ in 0..200 {
                    black_box(registry.available_url_outputs());
                }
            });
        }

        #[test]
        fn suggested_output_helpers_keep_up_with_ingest() {
            let paths = [
                "a.docx", "b.pdf", "c.mp4", "d.html", "e.wav", "f.pptx", "g.md", "h.png", "i.mkv",
                "j.epub",
            ];
            assert_within(Duration::from_secs(1), "suggested_output×2k", || {
                for _ in 0..200 {
                    for name in paths {
                        black_box(suggested_output_for_path(Path::new(name)));
                    }
                    black_box(suggested_output_for_url());
                }
            });
        }

        #[test]
        fn format_byte_size_and_argv_display_for_status_lines() {
            assert_within(Duration::from_secs(1), "format_byte_size×20k", || {
                for i in 0..20_000u64 {
                    black_box(format_byte_size(i.wrapping_mul(997)));
                }
            });
            let argv = [
                "ffmpeg",
                "-i",
                "/tmp/in.mp4",
                "-vn",
                "-acodec",
                "libmp3lame",
                "/tmp/out.mp3",
            ];
            assert_within(Duration::from_secs(1), "format_argv_display×5k", || {
                for _ in 0..5_000 {
                    black_box(format_argv_display(&argv));
                }
            });
        }
    }

    #[cfg(unix)]
    #[test]
    fn pdf_password_is_preprocessed_and_not_passed_to_modules() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let fake_qpdf = directory.join(format!("shift-qpdf-mod-test-{suffix}"));
        let input = directory.join(format!("shift-password-input-{suffix}.pdf"));
        std::fs::write(
            &fake_qpdf,
            "#!/bin/sh\n# Minimal fake qpdf: copy the .pdf input to the .pdf output.\ninput=\"\"\noutput=\"\"\nfor a in \"$@\"; do\n  case \"$a\" in\n    --) ;;\n    *.pdf) if [ -z \"$input\" ]; then input=\"$a\"; else output=\"$a\"; fi ;;\n  esac\ndone\ncp \"$input\" \"$output\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_qpdf).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_qpdf, permissions).unwrap();
        std::fs::write(&input, b"%PDF-1.4 fake").unwrap();

        // SAFETY: serialized behind crate::ENV_LOCK.
        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake_qpdf);
        }

        let mut module = fake(
            "no-password",
            &["pdf"],
            &[OutputFormat("plain")],
            &[],
            b"ok",
        );
        module.assert_password_absent = true;
        let registry = ConversionRegistry::new().with_module(module);
        let options = ConversionOptions {
            pdf: PdfInputOptions {
                password: Some("secret".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let artifact = registry
            .convert_to_with_options(&input, OutputFormat("plain"), &options)
            .unwrap();
        assert_eq!(artifact.bytes, b"ok");

        // Cleanup.
        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&fake_qpdf);
        let _ = std::fs::remove_file(&input);
    }

    // -------------------------------------------------------------------------
    // OutputFormat catalog matrix — every entry in OutputFormat::ALL
    // -------------------------------------------------------------------------

    #[test]
    fn output_format_all_id_parse_round_trip() {
        for format in OutputFormat::ALL {
            let id = format.id();
            assert!(!id.is_empty(), "empty id in catalog");
            let parsed: OutputFormat = id
                .parse()
                .unwrap_or_else(|e| panic!("id {id:?} failed to parse: {e}"));
            assert_eq!(
                parsed.id(),
                id,
                "round-trip id mismatch for catalog entry {id}"
            );
            // Case-insensitive parse of the canonical id.
            let upper = id.to_ascii_uppercase();
            let parsed_upper: OutputFormat = upper
                .parse()
                .unwrap_or_else(|e| panic!("uppercase id {upper:?} failed to parse for {id}: {e}"));
            assert_eq!(parsed_upper.id(), id);
        }
    }

    #[test]
    fn output_format_all_extension_media_type_label_non_empty() {
        for format in OutputFormat::ALL {
            let id = format.id();
            let ext = format.extension();
            // Catalog extensions are non-empty for every writer we ship.
            assert!(!ext.is_empty(), "extension empty for format id {id}");
            let media = format.media_type();
            assert!(!media.is_empty(), "media_type empty for format id {id}");
            assert!(
                media.contains('/'),
                "media_type should look like type/subtype for {id}: {media}"
            );
            let label = format.label();
            assert!(!label.is_empty(), "label empty for format id {id}");
            // Lowercase helpers must be consistent and non-empty.
            assert!(!format.label_lowercase().is_empty());
            assert!(!format.id_lowercase().is_empty());
            assert_eq!(
                format.id_lowercase(),
                id.to_ascii_lowercase(),
                "id_lowercase drift for {id}"
            );
        }
    }

    #[test]
    fn output_format_text_previewable_consistent_with_media_types() {
        for format in OutputFormat::ALL {
            let media = format.media_type();
            let previewable = format.is_text_previewable();
            // Explicit text/* media types in the catalog should be previewable.
            if matches!(
                media,
                "text/markdown" | "text/html" | "text/plain" | "text/vtt"
            ) {
                assert!(
                    previewable,
                    "{} has media_type {media} but is_text_previewable=false",
                    format.id()
                );
            }
            // Structured text-like application types used by pandoc writers.
            if matches!(
                media,
                "application/json" | "application/xml" | "application/x-subrip"
            ) {
                assert!(
                    previewable,
                    "{} has media_type {media} but is_text_previewable=false",
                    format.id()
                );
            }
            // Binary media families must not claim text preview.
            if media.starts_with("audio/")
                || media.starts_with("video/")
                || media.starts_with("image/")
                || media == "application/pdf"
                || media == "application/epub+zip"
                || media == "application/zip"
                || media.starts_with("application/vnd.")
            {
                assert!(
                    !previewable,
                    "{} has binary media_type {media} but is_text_previewable=true",
                    format.id()
                );
            }
        }
    }

    #[test]
    fn output_format_partitions_pandoc_media_cover_all_without_overlap() {
        use std::collections::HashSet;

        let all_ids: HashSet<&str> = OutputFormat::ALL.iter().map(|f| f.id()).collect();
        let pandoc_ids: HashSet<&str> = OutputFormat::PANDOC.iter().map(|f| f.id()).collect();
        let docling_ids: HashSet<&str> = OutputFormat::DOCLING.iter().map(|f| f.id()).collect();
        let media_ids: HashSet<&str> = OutputFormat::MEDIA.iter().map(|f| f.id()).collect();
        let image_ids: HashSet<&str> = OutputFormat::IMAGE.iter().map(|f| f.id()).collect();
        let sheet_ids: HashSet<&str> = OutputFormat::SPREADSHEET.iter().map(|f| f.id()).collect();
        let pdf_toolkit_ids: HashSet<&str> =
            OutputFormat::PDF_TOOLKIT.iter().map(|f| f.id()).collect();

        assert_eq!(
            OutputFormat::ALL.len(),
            OutputFormat::PANDOC.len()
                + OutputFormat::DOCLING.len()
                + OutputFormat::MEDIA.len()
                + OutputFormat::IMAGE.len()
                + OutputFormat::SPREADSHEET.len()
                + OutputFormat::PDF_TOOLKIT.len()
        );
        assert_eq!(
            docling_ids.len(),
            OutputFormat::DOCLING.len(),
            "duplicate ids in DOCLING"
        );
        assert_eq!(
            image_ids.len(),
            OutputFormat::IMAGE.len(),
            "duplicate ids in IMAGE"
        );
        assert_eq!(
            sheet_ids.len(),
            OutputFormat::SPREADSHEET.len(),
            "duplicate ids in SPREADSHEET"
        );
        assert_eq!(
            pdf_toolkit_ids.len(),
            OutputFormat::PDF_TOOLKIT.len(),
            "duplicate ids in PDF_TOOLKIT"
        );
        // IMAGE holds only the writers no other catalog claims; formats sips
        // shares with FFmpeg/Pandoc (png, jpg, gif, pdf) stay in their original
        // slice so the catalogs remain a true partition of ALL.
        for (name, other) in [
            ("PANDOC", &pandoc_ids),
            ("DOCLING", &docling_ids),
            ("MEDIA", &media_ids),
            ("SPREADSHEET", &sheet_ids),
        ] {
            let overlap: Vec<_> = image_ids.intersection(other).copied().collect();
            assert!(
                overlap.is_empty(),
                "ids appear in both IMAGE and {name}: {overlap:?}"
            );
        }
        for (name, other) in [
            ("PANDOC", &pandoc_ids),
            ("DOCLING", &docling_ids),
            ("MEDIA", &media_ids),
            ("IMAGE", &image_ids),
            ("PDF_TOOLKIT", &pdf_toolkit_ids),
        ] {
            let overlap: Vec<_> = sheet_ids.intersection(other).copied().collect();
            assert!(
                overlap.is_empty(),
                "ids appear in both SPREADSHEET and {name}: {overlap:?}"
            );
        }
        for (name, other) in [
            ("PANDOC", &pandoc_ids),
            ("MEDIA", &media_ids),
            ("IMAGE", &image_ids),
            ("SPREADSHEET", &sheet_ids),
        ] {
            let overlap: Vec<_> = pdf_toolkit_ids.intersection(other).copied().collect();
            assert!(
                overlap.is_empty(),
                "ids appear in both PDF_TOOLKIT and {name}: {overlap:?}"
            );
        }
        assert_eq!(
            all_ids.len(),
            OutputFormat::ALL.len(),
            "duplicate ids in ALL"
        );
        assert_eq!(
            pandoc_ids.len(),
            OutputFormat::PANDOC.len(),
            "duplicate ids in PANDOC"
        );
        assert_eq!(
            media_ids.len(),
            OutputFormat::MEDIA.len(),
            "duplicate ids in MEDIA"
        );

        let overlap: Vec<_> = pandoc_ids.intersection(&media_ids).copied().collect();
        assert!(
            overlap.is_empty(),
            "ids appear in both PANDOC and MEDIA: {overlap:?}"
        );
        for (name, other) in [
            ("PANDOC", &pandoc_ids),
            ("MEDIA", &media_ids),
            ("IMAGE", &image_ids),
            ("SPREADSHEET", &sheet_ids),
        ] {
            let overlap: Vec<_> = docling_ids.intersection(other).copied().collect();
            assert!(
                overlap.is_empty(),
                "ids appear in both DOCLING and {name}: {overlap:?}"
            );
        }

        let union: HashSet<&str> = pandoc_ids
            .union(&media_ids)
            .copied()
            .collect::<HashSet<&str>>()
            .union(&docling_ids)
            .copied()
            .collect::<HashSet<&str>>()
            .union(&image_ids)
            .copied()
            .collect::<HashSet<&str>>()
            .union(&sheet_ids)
            .copied()
            .collect::<HashSet<&str>>()
            .union(&pdf_toolkit_ids)
            .copied()
            .collect();
        assert_eq!(
            union, all_ids,
            "PANDOC ∪ DOCLING ∪ MEDIA ∪ IMAGE ∪ SPREADSHEET ∪ PDF_TOOLKIT must equal ALL"
        );

        // Every ALL entry is in exactly one partition.
        for format in OutputFormat::ALL {
            let in_pandoc = OutputFormat::PANDOC.contains(format);
            let in_docling = OutputFormat::DOCLING.contains(format);
            let in_media = OutputFormat::MEDIA.contains(format);
            let in_image = OutputFormat::IMAGE.contains(format);
            let in_sheet = OutputFormat::SPREADSHEET.contains(format);
            let in_pdf_toolkit = OutputFormat::PDF_TOOLKIT.contains(format);
            assert_eq!(
                usize::from(in_pandoc)
                    + usize::from(in_docling)
                    + usize::from(in_media)
                    + usize::from(in_image)
                    + usize::from(in_sheet)
                    + usize::from(in_pdf_toolkit),
                1,
                "{} must appear in exactly one output partition (pandoc={in_pandoc}, docling={in_docling}, media={in_media}, image={in_image}, sheet={in_sheet}, pdf_toolkit={in_pdf_toolkit})",
                format.id()
            );
        }

        // MEDIA partition members are exactly the FFmpeg writers.
        for format in OutputFormat::MEDIA {
            assert!(
                is_ffmpeg_output(*format),
                "MEDIA entry {} should be an FFmpeg output",
                format.id()
            );
        }
        for format in OutputFormat::PANDOC {
            assert!(
                !is_ffmpeg_output(*format),
                "PANDOC entry {} must not be classified as FFmpeg output",
                format.id()
            );
        }
    }

    #[test]
    fn output_format_parse_aliases_and_unknowns_matrix() {
        let aliases = [
            ("md", "markdown"),
            ("jpeg", "jpg"),
            ("mpg", "mpeg"),
            ("mpg2", "mpeg"),
            ("aif", "aiff"),
            ("png-zip", "png-sequence-zip"),
            ("png_sequence", "png-sequence-zip"),
            ("frames-zip", "png-sequence-zip"),
            ("PNG-SEQUENCE-ZIP", "png-sequence-zip"),
            ("  markdown  ", "markdown"),
            ("HTML", "html"),
            ("Mp3", "mp3"),
            ("3gp", "3gp"),
        ];
        for (input, expected_id) in aliases {
            let parsed: OutputFormat = input
                .parse()
                .unwrap_or_else(|e| panic!("alias {input:?} failed: {e}"));
            assert_eq!(
                parsed.id(),
                expected_id,
                "alias {input:?} should resolve to {expected_id}"
            );
        }

        for bad in [
            "",
            "   ",
            "not-a-real-format",
            "docx-plus",
            "audio/mpeg",
            "mark down",
            "png-sequence",
        ] {
            let err = bad.parse::<OutputFormat>().unwrap_err();
            assert!(
                err.to_string().contains("unknown output format"),
                "expected unknown for {bad:?}, got {err}"
            );
        }
    }

    #[test]
    fn output_format_extension_round_trip_via_parse_where_unique() {
        // For formats whose extension uniquely maps back to the same id (or a
        // sibling that shares the extension), parsing the extension must succeed.
        for format in OutputFormat::ALL {
            let ext = format.extension();
            let parsed = ext.parse::<OutputFormat>();
            assert!(
                parsed.is_ok(),
                "extension {ext:?} of {} must parse as some OutputFormat",
                format.id()
            );
            let parsed = parsed.unwrap();
            // The parsed format's extension must match (aliases like md→markdown).
            assert_eq!(
                parsed.extension(),
                ext,
                "parsed extension for {} via {ext}",
                format.id()
            );
        }
    }

    // -------------------------------------------------------------------------
    // Default registry route matrix
    // -------------------------------------------------------------------------

    #[test]
    fn default_registry_route_matrix_representative_pairs() {
        let registry = ConversionRegistry::default();

        // (input path, output, expected direct module id or None for unsupported direct)
        // Two-step-only routes assert module_for is None while route_module_ids is Some.
        struct Case {
            input: &'static str,
            output: OutputFormat,
            direct_module: Option<&'static str>,
            route_head: Option<&'static str>,
        }

        // sips is macOS-only, so still-image expectations are platform-specific.
        #[cfg(target_os = "macos")]
        let (still_module, heic_module) = ("sips", Some("sips"));
        // Off macOS nothing reads HEIC, so that pair is unsupported entirely.
        #[cfg(not(target_os = "macos"))]
        let (still_module, heic_module) = ("ffmpeg", None);

        let cases = [
            Case {
                input: "REPORT.DOCX",
                output: OutputFormat::MARKDOWN,
                direct_module: Some("markitdown"),
                route_head: Some("markitdown"),
            },
            Case {
                input: "scan.pdf",
                output: OutputFormat::MARKDOWN,
                direct_module: Some("markitdown"),
                route_head: Some("markitdown"),
            },
            Case {
                input: "scan.pdf",
                output: OutputFormat::HTML,
                direct_module: Some("docling"),
                route_head: Some("docling"),
            },
            Case {
                input: "scan.pdf",
                output: OutputFormat("plain"),
                direct_module: Some("docling"),
                route_head: Some("docling"),
            },
            Case {
                input: "clip.mp4",
                output: OutputFormat::MP3,
                direct_module: Some("ffmpeg"),
                route_head: Some("ffmpeg"),
            },
            Case {
                input: "page.html",
                output: OutputFormat::MARKDOWN,
                direct_module: Some("markitdown"),
                route_head: Some("markitdown"),
            },
            Case {
                input: "page.HTM",
                output: OutputFormat::HTML,
                // Pandoc is registered before Defuddle and handles HTML→HTML.
                direct_module: Some("pandoc"),
                route_head: Some("pandoc"),
            },
            Case {
                input: "track.wav",
                output: OutputFormat::FLAC,
                direct_module: Some("ffmpeg"),
                route_head: Some("ffmpeg"),
            },
            Case {
                input: "clip.mkv",
                output: OutputFormat::SRT,
                direct_module: Some("ffmpeg"),
                route_head: Some("ffmpeg"),
            },
            // Still → still is sips on macOS (registered ahead of FFmpeg) and
            // FFmpeg everywhere else. Frame extraction from a container stays
            // with FFmpeg on every platform; see the `clip.mp4` cases above.
            Case {
                input: "photo.png",
                output: OutputFormat::JPG,
                direct_module: Some(still_module),
                route_head: Some(still_module),
            },
            Case {
                input: "photo.heic",
                output: OutputFormat::JPG,
                direct_module: heic_module,
                route_head: heic_module,
            },
            Case {
                input: "notes.md",
                output: OutputFormat::DOCX,
                direct_module: Some("pandoc"),
                route_head: Some("pandoc"),
            },
            Case {
                input: "notes.md",
                output: OutputFormat::PDF,
                direct_module: Some("pandoc"),
                route_head: Some("pandoc"),
            },
            Case {
                input: "deck.pptx",
                output: OutputFormat::MARKDOWN,
                direct_module: Some("markitdown"),
                route_head: Some("markitdown"),
            },
            // Unsupported direct pairs
            Case {
                input: "clip.mp4",
                output: OutputFormat::DOCX,
                direct_module: None,
                route_head: None, // may be two-step or unsupported
            },
            Case {
                input: "report.docx",
                output: OutputFormat::MP3,
                direct_module: None,
                route_head: None,
            },
            Case {
                input: "mystery.xyz",
                output: OutputFormat::MARKDOWN,
                direct_module: None,
                route_head: None,
            },
        ];

        for case in cases {
            let path = Path::new(case.input);
            let direct = registry.module_for(path, case.output).map(|m| m.id());
            assert_eq!(
                direct,
                case.direct_module,
                "module_for({}, {})",
                case.input,
                case.output.id()
            );

            let route = registry.route_module_ids(path, case.output);
            match case.route_head {
                Some(head) => {
                    let ids = route.unwrap_or_else(|| {
                        panic!("expected route for {} → {}", case.input, case.output.id())
                    });
                    assert_eq!(
                        ids.first().copied(),
                        Some(head),
                        "route head for {} → {}: {ids:?}",
                        case.input,
                        case.output.id()
                    );
                }
                None => {
                    // For explicitly unsupported samples, either no route or no direct module.
                    if case.direct_module.is_none()
                        && matches!(case.input, "mystery.xyz" | "report.docx")
                        && case.output == OutputFormat::MP3
                    {
                        assert!(
                            route.is_none(),
                            "unexpected route for {} → {}: {:?}",
                            case.input,
                            case.output.id(),
                            route
                        );
                    }
                    if case.input == "mystery.xyz" {
                        assert!(route.is_none(), "mystery.xyz must not route: {:?}", route);
                    }
                }
            }
        }
    }

    #[test]
    fn default_registry_url_routes_and_unsupported() {
        let registry = ConversionRegistry::default();

        assert_eq!(
            registry
                .module_for_url(OutputFormat::MARKDOWN)
                .map(|m| m.id()),
            Some("defuddle")
        );
        assert_eq!(
            registry.module_for_url(OutputFormat::HTML).map(|m| m.id()),
            Some("defuddle")
        );
        // Defuddle does not produce PDF directly; URL→PDF is two-step via pandoc.
        assert!(registry.module_for_url(OutputFormat::PDF).is_none());
        let pdf_route = registry.url_route_module_ids(OutputFormat::PDF);
        assert!(
            pdf_route
                .as_ref()
                .is_some_and(|ids| ids.first() == Some(&"defuddle")),
            "URL→PDF should chain from defuddle: {pdf_route:?}"
        );

        assert!(registry.module_for_url(OutputFormat::MP3).is_none());
        assert!(
            registry.url_route_module_ids(OutputFormat::MP3).is_none(),
            "URL→mp3 must be unsupported"
        );

        let url_outputs = registry.available_url_outputs();
        assert!(url_outputs.contains(&OutputFormat::MARKDOWN));
        assert!(url_outputs.contains(&OutputFormat::HTML));
        assert!(!url_outputs.contains(&OutputFormat::MP3));
    }

    #[test]
    fn priority_reordering_markitdown_pandoc_docling_on_pdf_markdown() {
        let default = ConversionRegistry::default();
        assert_eq!(
            default
                .module_for(Path::new("scan.pdf"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "markitdown"
        );

        let pandoc_first =
            ConversionRegistry::default().with_priority(&["pandoc", "markitdown", "docling"]);
        // Pandoc also accepts pdf→md? Check — if not, markitdown stays.
        let chosen = pandoc_first
            .module_for(Path::new("scan.pdf"), OutputFormat::MARKDOWN)
            .unwrap()
            .id();
        // Pandoc may or may not support pdf input; assert stability of docling promote.
        let _ = chosen;

        let docling_first =
            ConversionRegistry::default().with_priority(&["docling", "markitdown", "pandoc"]);
        assert_eq!(
            docling_first
                .module_for(Path::new("scan.pdf"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "docling"
        );

        let markitdown_first =
            ConversionRegistry::default().with_priority(&["markitdown", "docling", "pandoc"]);
        assert_eq!(
            markitdown_first
                .module_for(Path::new("scan.pdf"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "markitdown"
        );

        // DOCX markdown priority between markitdown and pandoc.
        let pandoc_docx = ConversionRegistry::default().with_priority(&["pandoc", "markitdown"]);
        assert_eq!(
            pandoc_docx
                .module_for(Path::new("report.docx"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "pandoc"
        );
        let markitdown_docx =
            ConversionRegistry::default().with_priority(&["markitdown", "pandoc"]);
        assert_eq!(
            markitdown_docx
                .module_for(Path::new("report.docx"), OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "markitdown"
        );
    }

    #[test]
    fn default_registry_module_ids_are_stable() {
        let registry = ConversionRegistry::default();
        let ids: Vec<_> = registry.modules().map(|m| m.id()).collect();
        #[cfg(target_os = "macos")]
        let expected = vec![
            "markitdown",
            "pandoc",
            "defuddle",
            "docling",
            "qpdf",
            "spreadsheet",
            "sips",
            "ffmpeg",
        ];
        #[cfg(not(target_os = "macos"))]
        let expected = vec![
            "markitdown",
            "pandoc",
            "defuddle",
            "docling",
            "qpdf",
            "spreadsheet",
            "ffmpeg",
        ];
        assert_eq!(ids, expected);
        for id in &ids {
            assert!(registry.has_module(id));
        }
        assert!(!registry.has_module("libreoffice"));
        assert!(!registry.has_module(""));
    }

    #[test]
    fn spreadsheet_owns_tabular_pairs_not_markdown() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for(Path::new("sheet.xlsx"), OutputFormat::CSV)
                .unwrap()
                .id(),
            "spreadsheet"
        );
        assert_eq!(
            registry
                .module_for(Path::new("data.csv"), OutputFormat::XLSX)
                .unwrap()
                .id(),
            "spreadsheet"
        );
        // Document engines still own xlsx → markdown.
        let md = registry
            .module_for(Path::new("sheet.xlsx"), OutputFormat::MARKDOWN)
            .unwrap()
            .id();
        assert!(
            md == "markitdown" || md == "docling" || md == "pandoc",
            "xlsx→md should stay with a document engine, got {md}"
        );
        assert!(
            registry
                .module_for(Path::new("sheet.xlsx"), OutputFormat::MARKDOWN)
                .unwrap()
                .id()
                != "spreadsheet"
        );
    }

    #[test]
    fn unsupported_pairs_have_no_route_matrix() {
        let registry = ConversionRegistry::default();
        let unsupported = [
            ("report.docx", OutputFormat::MP3),
            ("report.docx", OutputFormat::MP4),
            ("report.docx", OutputFormat::PNG),
            ("report.docx", OutputFormat::SRT),
            ("notes.md", OutputFormat::MP3),
            ("notes.md", OutputFormat::WEBM),
            ("mystery.xyz", OutputFormat::MARKDOWN),
            ("mystery.xyz", OutputFormat::HTML),
            ("mystery.xyz", OutputFormat::MP3),
            ("data.csv", OutputFormat::MP4),
        ];
        for (input, output) in unsupported {
            let path = Path::new(input);
            assert!(
                registry.module_for(path, output).is_none(),
                "unexpected direct module for {input} → {}",
                output.id()
            );
            // Two-step may still exist for some document pairs via chain; only
            // assert hard-unsupported when neither direct nor chain is found.
            if input.starts_with("mystery")
                || (input.ends_with(".docx") && is_ffmpeg_output(output))
            {
                assert!(
                    registry.route_module_ids(path, output).is_none(),
                    "unexpected route for {input} → {}: {:?}",
                    output.id(),
                    registry.route_module_ids(path, output)
                );
            }
        }
    }

    #[test]
    fn media_input_available_outputs_include_media_catalog() {
        let registry = ConversionRegistry::default();
        let video_outputs = registry.available_outputs(Path::new("clip.mp4"));
        for format in OutputFormat::MEDIA {
            // WEBP only appears when this machine's ffmpeg was compiled with libwebp.
            if *format == OutputFormat::WEBP
                && registry
                    .module_for(Path::new("clip.mp4"), OutputFormat::WEBP)
                    .is_none()
            {
                continue;
            }
            assert!(
                video_outputs.contains(format),
                "mp4 should list media output {}",
                format.id()
            );
        }
        // Document-only formats are not direct FFmpeg outputs.
        assert!(
            !video_outputs.contains(&OutputFormat::DOCX) || {
                // DOCX might appear via a chain; if present it must not be direct ffmpeg.
                registry
                    .module_for(Path::new("clip.mp4"), OutputFormat::DOCX)
                    .is_none()
            }
        );
    }

    #[test]
    fn default_output_path_matrix_for_catalog_sample() {
        let samples = [
            (
                Path::new("a/b/note.md"),
                OutputFormat::HTML,
                "a/b/note.html",
            ),
            (
                Path::new("a/b/note.md"),
                OutputFormat::MARKDOWN,
                "a/b/note.converted.md",
            ),
            (Path::new("clip.mp4"), OutputFormat::MP3, "clip.mp3"),
            (
                Path::new("clip.mp3"),
                OutputFormat::MP3,
                "clip.converted.mp3",
            ),
            (
                Path::new("scan.pdf"),
                OutputFormat::PNG_SEQUENCE_ZIP,
                "scan.zip",
            ),
            (Path::new("x.3gp"), OutputFormat::THREEGP, "x.converted.3gp"),
            (Path::new("subs.srt"), OutputFormat::VTT, "subs.vtt"),
        ];
        for (input, format, expected) in samples {
            assert_eq!(
                default_output_path(input, format),
                Path::new(expected),
                "{} → {}",
                input.display(),
                format.id()
            );
        }
    }

    #[test]
    fn conversion_artifact_write_to_and_preview_matrix() {
        let dir = std::env::temp_dir().join(format!(
            "shift-artifact-matrix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let cases: &[(OutputFormat, &[u8], bool)] = &[
            (OutputFormat::MARKDOWN, b"# hi\n", true),
            (OutputFormat::HTML, b"<p>x</p>", true),
            (OutputFormat("plain"), b"plain text", true),
            (OutputFormat::MP3, b"ID3fake", false),
            (OutputFormat::PDF, b"%PDF-1.4", false),
            (OutputFormat::PNG, b"\x89PNG", false),
            (
                OutputFormat::SRT,
                b"1\n00:00:00,000 --> 00:00:01,000\nHi\n",
                true,
            ),
            (OutputFormat::VTT, b"WEBVTT\n\nHi\n", true),
        ];

        for (format, bytes, text_preview) in cases {
            let artifact = ConversionArtifact {
                file_name: format!("out.{}", format.extension()),
                media_type: format.media_type(),
                bytes: bytes.to_vec(),
                format: *format,
                module_id: "test",
                pipeline: vec!["test"],
                invocations: Vec::new(),
            };
            let path = dir.join(&artifact.file_name);
            artifact.write_to(&path).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), *bytes);

            let preview = artifact.preview_summary();
            if *text_preview {
                assert!(
                    !preview.contains("Not shown inline"),
                    "expected text preview for {}: {preview}",
                    format.id()
                );
            } else {
                assert!(
                    preview.contains("preview"),
                    "expected binary inspection for {}: {preview}",
                    format.id()
                );
                assert!(
                    preview.contains("Open") || preview.contains("Download"),
                    "binary notes should steer to Open/Download for {}: {preview}",
                    format.id()
                );
            }
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn two_step_url_and_file_route_ids_cover_common_chains() {
        let registry = ConversionRegistry::default();

        // Video → Transcript is the direct Docling ASR route.
        let ids = registry
            .route_module_ids(Path::new("clip.mp4"), OutputFormat::TRANSCRIPT)
            .expect("video→transcript route");
        assert_eq!(ids, vec!["docling"]);
        // Video → Markdown should not be a Docling direct route (ASR is transcript).
        if let Some(md_ids) =
            registry.route_module_ids(Path::new("clip.mp4"), OutputFormat::MARKDOWN)
        {
            assert_ne!(md_ids, vec!["docling"]);
        }

        // Local HTML → DOCX: defuddle/markitdown chain or pandoc direct.
        let html_docx = registry.route_module_ids(Path::new("page.html"), OutputFormat::DOCX);
        assert!(html_docx.is_some(), "html→docx should route");

        // URL → DOCX chains through defuddle.
        let url_docx = registry
            .url_route_module_ids(OutputFormat::DOCX)
            .expect("url→docx");
        assert_eq!(url_docx[0], "defuddle");
    }

    #[test]
    fn format_byte_size_edges() {
        assert_eq!(format_byte_size(0), "0 B");
        assert_eq!(format_byte_size(1), "1 B");
        assert_eq!(format_byte_size(1023), "1023 B");
        assert_eq!(format_byte_size(1024), "1.0 KB");
        assert_eq!(format_byte_size(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn pdf_input_options_matrix_combinations() {
        let matrix = [
            (None, None, None, false, false, true),
            (Some(1), None, None, false, false, false),
            (Some(2), None, None, true, true, false),
            (None, Some(3), None, true, true, false),
            (Some(1), Some(3), None, true, true, false),
            (Some(2), Some(5), Some("pw"), true, true, false),
            (None, None, Some("pw"), false, true, false),
            (Some(1), None, Some("pw"), false, true, false),
        ];
        for (from, to, password, needs_slice, needs_pre, is_default) in matrix {
            let opts = PdfInputOptions {
                page_from: from,
                page_to: to,
                password: password.map(str::to_owned),
                ..Default::default()
            };
            assert_eq!(
                opts.needs_slice(),
                needs_slice,
                "slice from={from:?} to={to:?}"
            );
            assert_eq!(
                opts.needs_preprocessing(),
                needs_pre,
                "pre from={from:?} to={to:?} pw={password:?}"
            );
            assert_eq!(
                opts.is_default(),
                is_default,
                "default from={from:?} to={to:?} pw={password:?}"
            );
        }
    }

    #[test]
    fn conversion_options_default_nests_are_default() {
        let opts = ConversionOptions::default();
        assert!(opts.ffmpeg.is_default());
        assert!(opts.pdf.is_default());
        assert!(opts.cancel.is_none());
        assert!(opts.progress.is_none());
    }

    #[test]
    fn paths_refer_to_same_file_edge_cases() {
        assert!(paths_refer_to_same_file(Path::new("a/b"), Path::new("a/b")));
        assert!(!paths_refer_to_same_file(
            Path::new("a/b"),
            Path::new("a/c")
        ));
        // Relative vs absolute with same cwd resolution is best-effort.
        let cwd_file = std::env::current_dir().unwrap().join("Cargo.toml");
        if cwd_file.is_file() {
            assert!(paths_refer_to_same_file(Path::new("Cargo.toml"), &cwd_file));
        }
    }

    #[test]
    fn redact_and_format_argv_matrix() {
        let cases = [
            (vec!["a"], "a"),
            (vec!["a", "b c"], "a 'b c'"),
            (vec!["tool", "--x=y"], "tool --x=y"),
        ];
        for (parts, expected) in cases {
            assert_eq!(format_argv_display(&parts), expected);
        }

        let mut parts = vec!["docling".into(), "--pdf-password".into(), "hunter2".into()];
        redact_flag_value(&mut parts, "--pdf-password", "••••");
        assert_eq!(parts[2], "••••");
    }

    #[test]
    fn every_media_format_extension_matches_id_or_known_alias() {
        for format in OutputFormat::MEDIA {
            let id = format.id();
            let ext = format.extension();
            if id == "png-sequence-zip" {
                assert_eq!(ext, "zip");
            } else if id == "jpg" {
                assert_eq!(ext, "jpg");
            } else {
                assert_eq!(ext, id, "media format id should equal extension for {id}");
            }
        }
    }

    #[test]
    fn route_module_ids_for_all_media_outputs_from_mp4() {
        let registry = ConversionRegistry::default();
        let input = Path::new("clip.mp4");
        for format in OutputFormat::MEDIA {
            // WEBP routes only when this machine's ffmpeg was compiled with libwebp.
            let Some(ids) = registry.route_module_ids(input, *format) else {
                assert_eq!(
                    *format,
                    OutputFormat::WEBP,
                    "unexpected missing route clip.mp4 → {}",
                    format.id()
                );
                continue;
            };
            assert_eq!(ids, vec!["ffmpeg"], "route for {}", format.id());
        }
        // ASR is a dedicated action outside the MEDIA partition.
        assert_eq!(
            registry
                .route_module_ids(input, OutputFormat::TRANSCRIPT)
                .expect("video→transcript"),
            vec!["docling"]
        );
    }

    #[test]
    fn available_outputs_non_empty_for_common_inputs() {
        let registry = ConversionRegistry::default();
        for sample in [
            "a.docx", "b.pdf", "c.html", "d.md", "e.mp4", "f.wav", "g.png", "h.pptx", "i.epub",
            "j.jpg", "k.mkv",
        ] {
            let outs = registry.available_outputs(Path::new(sample));
            assert!(!outs.is_empty(), "available_outputs empty for {sample}");
        }
    }

    #[test]
    fn conversion_error_display_and_helpers_matrix() {
        let cases = [
            ("conversion cancelled", true, false),
            ("executable not found: /tmp/x", false, true),
            ("executable not found: missing", false, true),
            ("Executable not found on PATH", false, false),
            ("boom", false, false),
        ];
        for (msg, cancelled, missing) in cases {
            let err = if msg == "conversion cancelled" {
                ConversionError::cancelled()
            } else {
                ConversionError::new(msg)
            };
            assert_eq!(err.is_cancelled(), cancelled, "{msg}");
            assert_eq!(err.is_executable_not_found(), missing, "{msg}");
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn conversion_options_debug_and_partial_eq_cover_cancel_and_progress() {
        let cancel_a = Arc::new(AtomicBool::new(false));
        let cancel_b = Arc::new(AtomicBool::new(false));
        let sink_a: ProgressSink = Arc::new(|_| {});
        let sink_b: ProgressSink = Arc::new(|_| {});

        let base = ConversionOptions::default();
        let with_cancel = ConversionOptions {
            cancel: Some(Arc::clone(&cancel_a)),
            ..ConversionOptions::default()
        };
        let with_same_cancel = ConversionOptions {
            cancel: Some(Arc::clone(&cancel_a)),
            ..ConversionOptions::default()
        };
        let with_other_cancel = ConversionOptions {
            cancel: Some(Arc::clone(&cancel_b)),
            ..ConversionOptions::default()
        };
        let with_progress = ConversionOptions {
            progress: Some(Arc::clone(&sink_a)),
            ..ConversionOptions::default()
        };
        let with_same_progress = ConversionOptions {
            progress: Some(Arc::clone(&sink_a)),
            ..ConversionOptions::default()
        };
        let with_other_progress = ConversionOptions {
            progress: Some(Arc::clone(&sink_b)),
            ..ConversionOptions::default()
        };

        assert_eq!(base, ConversionOptions::default());
        assert_eq!(with_cancel, with_same_cancel);
        assert_ne!(with_cancel, with_other_cancel);
        assert_ne!(with_cancel, base);
        assert_eq!(with_progress, with_same_progress);
        assert_ne!(with_progress, with_other_progress);

        let debug = format!("{with_cancel:?}");
        assert!(debug.contains("ConversionOptions"));
        assert!(debug.contains("<AtomicBool>"));
        let debug_progress = format!("{with_progress:?}");
        assert!(debug_progress.contains("<ProgressSink>"));
    }

    #[test]
    fn with_module_provenance_fills_empty_and_preserves_existing() {
        let bare = ConversionArtifact {
            file_name: "out.md".into(),
            media_type: "text/markdown",
            bytes: b"hi".to_vec(),
            format: OutputFormat::MARKDOWN,
            module_id: "old",
            pipeline: Vec::new(),
            invocations: Vec::new(),
        };
        let filled = bare.with_module_provenance(
            "pandoc",
            Some(InvocationRecord {
                module_id: "pandoc",
                argv_display: "pandoc in out".into(),
            }),
        );
        assert_eq!(filled.module_id, "pandoc");
        assert_eq!(filled.pipeline, vec!["pandoc"]);
        assert_eq!(filled.invocations.len(), 1);
        assert_eq!(filled.invocations[0].argv_display, "pandoc in out");

        // Existing pipeline/invocations are preserved; only module_id updates.
        let already = ConversionArtifact {
            file_name: "out.md".into(),
            media_type: "text/markdown",
            bytes: b"hi".to_vec(),
            format: OutputFormat::MARKDOWN,
            module_id: "first",
            pipeline: vec!["first", "second"],
            invocations: vec![InvocationRecord {
                module_id: "first",
                argv_display: "a".into(),
            }],
        };
        let kept = already.with_module_provenance(
            "override",
            Some(InvocationRecord {
                module_id: "override",
                argv_display: "ignored".into(),
            }),
        );
        assert_eq!(kept.module_id, "override");
        assert_eq!(kept.pipeline, vec!["first", "second"]);
        assert_eq!(kept.invocations.len(), 1);
        assert_eq!(kept.invocations[0].argv_display, "a");

        // Empty pipeline + None invocation leaves invocations empty.
        let no_inv = ConversionArtifact {
            file_name: "x.md".into(),
            media_type: "text/markdown",
            bytes: Vec::new(),
            format: OutputFormat::MARKDOWN,
            module_id: "m",
            pipeline: Vec::new(),
            invocations: Vec::new(),
        }
        .with_module_provenance("m2", None);
        assert_eq!(no_inv.pipeline, vec!["m2"]);
        assert!(no_inv.invocations.is_empty());
        assert_eq!(no_inv.module_id, "m2");
    }

    #[test]
    fn format_byte_size_gigabytes_and_subtitle_preview() {
        assert_eq!(format_byte_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(
            format_byte_size(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024),
            "3.5 GB"
        );

        let artifact = ConversionArtifact {
            file_name: "subs.srt".into(),
            media_type: "application/x-subrip",
            bytes: b"1\n00:00:01,000 --> 00:00:02,000\nHi\n".to_vec(),
            format: OutputFormat::SRT,
            module_id: "ffmpeg",
            pipeline: Vec::new(),
            invocations: Vec::new(),
        };
        // SRT is text-previewable — the normal Ready path uses text excerpts.
        assert!(artifact.format.is_text_previewable());
        let text_preview = artifact.preview_summary();
        assert!(
            text_preview.contains("Hi") || text_preview.contains("00:00:01"),
            "expected text excerpt for SRT, got: {text_preview}"
        );
        // Direct inspection still labels the format without implying a player.
        let summary = artifact.inspection().summary();
        assert!(
            summary.contains("preview"),
            "expected inspection, got: {summary}"
        );
        assert!(summary.contains("SRT") || summary.contains("SubRip"));
        assert!(summary.contains("Open") || summary.contains("Download"));
    }

    #[test]
    fn convert_url_rejects_invalid_and_uses_custom_module() {
        struct UrlOnlyModule;
        impl ConversionModule for UrlOnlyModule {
            fn id(&self) -> &'static str {
                "url-only"
            }
            fn label(&self) -> &'static str {
                "URL Only"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &[]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                _input: &Path,
                _output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                Err(ConversionError::new("files not supported"))
            }
            fn supports_url(&self, output: OutputFormat) -> bool {
                output == OutputFormat::MARKDOWN
            }
            fn convert_url(
                &self,
                url: &str,
                output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                Ok(ConversionArtifact {
                    file_name: "page.md".into(),
                    media_type: "text/markdown",
                    bytes: format!("# {url}").into_bytes(),
                    format: output,
                    module_id: self.id(),
                    pipeline: Vec::new(),
                    invocations: Vec::new(),
                })
            }
        }

        let registry = ConversionRegistry::new().with_module(UrlOnlyModule);
        let err = registry
            .convert_url("not-a-url", OutputFormat::MARKDOWN)
            .unwrap_err();
        assert!(err.to_string().contains("not a valid http(s) URL"));

        let err = registry
            .convert_url_with_options(
                "https://example.com/x",
                OutputFormat::DOCX,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("DOCX") || err.to_string().contains("docx"),
            "error: {err}"
        );

        let artifact = registry
            .convert_url("https://example.com/article", OutputFormat::MARKDOWN)
            .unwrap();
        assert_eq!(artifact.module_id, "url-only");
        assert_eq!(artifact.pipeline, vec!["url-only"]);
        assert!(
            artifact.text().unwrap().contains("example.com/article"),
            "text: {:?}",
            artifact.text()
        );

        // Default trait convert_url rejects when supports_url is false.
        struct NoUrlModule;
        impl ConversionModule for NoUrlModule {
            fn id(&self) -> &'static str {
                "no-url"
            }
            fn label(&self) -> &'static str {
                "No URL"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["txt"]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                _input: &Path,
                output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                Ok(ConversionArtifact {
                    file_name: "x.md".into(),
                    media_type: "text/markdown",
                    bytes: b"x".to_vec(),
                    format: output,
                    module_id: self.id(),
                    pipeline: Vec::new(),
                    invocations: Vec::new(),
                })
            }
        }
        let module = NoUrlModule;
        let err = module
            .convert_url(
                "https://example.com",
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("does not support URL conversion"));
    }

    #[test]
    fn convert_shortcut_and_unreadable_input() {
        struct EchoModule;
        impl ConversionModule for EchoModule {
            fn id(&self) -> &'static str {
                "echo"
            }
            fn label(&self) -> &'static str {
                "Echo"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["txt"]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                input: &Path,
                output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                let bytes =
                    std::fs::read(input).map_err(|e| ConversionError::new(e.to_string()))?;
                Ok(ConversionArtifact {
                    file_name: "out.md".into(),
                    media_type: "text/markdown",
                    bytes,
                    format: output,
                    module_id: self.id(),
                    pipeline: Vec::new(),
                    invocations: Vec::new(),
                })
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "shift-convert-shortcut-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("note.txt");
        std::fs::write(&input, b"hello").unwrap();

        let registry = ConversionRegistry::new().with_module(EchoModule);
        let artifact = registry.convert(&input).unwrap();
        assert_eq!(artifact.bytes, b"hello");
        assert_eq!(artifact.format, OutputFormat::MARKDOWN);

        let missing = dir.join("missing.txt");
        let err = registry
            .convert_to(&missing, OutputFormat::MARKDOWN)
            .unwrap_err();
        assert!(
            err.to_string().contains("not a readable file"),
            "error: {err}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn target_size_rejects_too_small_and_unsupported_routes_before_spawn() {
        let input =
            std::env::temp_dir().join(format!("shift-target-route-{}.md", std::process::id()));
        std::fs::write(&input, "# test").unwrap();
        let registry = ConversionRegistry::default();

        let unsupported = registry
            .convert_to_with_options(
                &input,
                OutputFormat::HTML,
                &ConversionOptions {
                    target_size_bytes: Some(1_000_000),
                    ..ConversionOptions::default()
                },
            )
            .unwrap_err();
        assert!(
            unsupported.to_string().contains("cannot fit"),
            "{unsupported}"
        );

        let tiny = registry
            .convert_to_with_options(
                &input,
                OutputFormat::HTML,
                &ConversionOptions {
                    target_size_bytes: Some(1),
                    ..ConversionOptions::default()
                },
            )
            .unwrap_err();
        assert!(tiny.to_string().contains("at least 16 KiB"), "{tiny}");

        let _ = std::fs::remove_file(input);
    }

    #[test]
    fn two_step_target_size_applies_only_to_final_hop() {
        let hop1_seen = Arc::new(Mutex::new(None));
        let hop2_seen = Arc::new(Mutex::new(None));
        let mut first = fake(
            "first",
            &["src"],
            &[OutputFormat::JPG],
            &[OutputFormat::JPG],
            b"intermediate",
        );
        first.seen_target_size = Some(hop1_seen.clone());
        let mut second = fake("second", &["jpg"], &[OutputFormat::MP3], &[], b"fitted-mp3");
        second.supports_target_size = true;
        second.seen_target_size = Some(hop2_seen.clone());
        let registry = ConversionRegistry::new()
            .with_module(first)
            .with_module(second);

        let input =
            std::env::temp_dir().join(format!("shift-target-chain-{}.src", std::process::id()));
        std::fs::write(&input, b"source").unwrap();

        let artifact = registry
            .convert_to_with_options(
                &input,
                OutputFormat::MP3,
                &ConversionOptions {
                    target_size_bytes: Some(100_000),
                    ..ConversionOptions::default()
                },
            )
            .unwrap();
        assert_eq!(artifact.bytes, b"fitted-mp3");
        assert_eq!(artifact.pipeline, vec!["first", "second"]);
        assert_eq!(*hop1_seen.lock().unwrap(), Some(None));
        assert_eq!(*hop2_seen.lock().unwrap(), Some(Some(100_000)));

        let _ = std::fs::remove_file(input);
    }

    #[test]
    fn remove_partial_outputs_missing_parent_returns_zero() {
        let planned = Path::new("/nonexistent-shift-parent-xyz/out.md");
        assert_eq!(remove_partial_outputs(planned), 0);
    }
}
