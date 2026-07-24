//! Format conversion modules and capability-based dispatch.

mod batch;
mod defuddle;
mod diagnostics;
mod docling;
mod ffmpeg;
mod magic_paste;
mod markitdown;
mod pandoc;
mod pdf_slice;
mod process;
mod sources;
mod suggest;

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub use batch::{
    BatchEnqueueOptions, BatchEvent, BatchFormatSelection, BatchItem, BatchItemId, BatchItemState,
    BatchProgress, BatchQueue, BatchSource, BatchSummary, prepare_batch_destination,
    resolve_destination, run_batch, suggested_url_file_name, uniquify_destination,
};
pub use defuddle::{
    DefuddleModule, DefuddleOptions, block_private_urls, ensure_public_url_fetch_allowed,
    looks_like_url, url_display_host, url_targets_non_public_host,
};
pub use diagnostics::{
    DiagnosticsReport, EngineDiagnostic, FormatAvailability, PdfEngineDiagnostic, Readiness,
    available_ready_outputs, available_ready_url_outputs, format_availability, supported_outputs,
};
pub use docling::{DoclingImageExportMode, DoclingModule, DoclingOptions, DoclingTableMode};
pub use ffmpeg::{
    FfmpegEncodeMode, FfmpegModule, FfmpegOptions, FfmpegQuality, input_looks_like_media,
    is_audio_output, is_ffmpeg_output, is_image_output, is_subtitle_output, is_video_output,
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
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_PROCESS_TIMEOUT, LimitedOutput, find_executable, is_runnable,
    max_output_bytes, process_timeout, read_file_limited, resolve_tool_executable,
    resolve_tool_path, run_command, run_command_cancellable, unique_temp_dir,
};
pub use sources::{
    MAX_EXPAND_DEPTH, MAX_EXPAND_FILES, expand_input_paths, supported_input_extensions,
};
pub use suggest::{suggested_output_for_path, suggested_output_for_url};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OutputFormat(&'static str);

impl OutputFormat {
    pub const MARKDOWN: Self = Self("markdown");
    pub const HTML: Self = Self("html");
    pub const DOCX: Self = Self("docx");
    pub const PDF: Self = Self("pdf");
    pub const PPTX: Self = Self("pptx");
    pub const EPUB: Self = Self("epub");

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
    pub const SRT: Self = Self("srt");
    pub const VTT: Self = Self("vtt");
    /// PNG frame sequence packaged as a single ZIP (FFmpeg).
    pub const PNG_SEQUENCE_ZIP: Self = Self("png-sequence-zip");

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
        Self::PNG_SEQUENCE_ZIP,
        // Subtitles
        Self::SRT,
        Self::VTT,
    ];

    /// Full UI/parse catalog: publishing formats first, then media.
    ///
    /// Prefer [`Self::all`] when iterating. This slice exists for call sites that
    /// need a `'static` list (menus, `available_outputs` filtering).
    pub const ALL: &'static [Self] = &[
        // Pandoc writers (must stay in sync with `PANDOC`).
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
        Self::PNG_SEQUENCE_ZIP,
        Self::SRT,
        Self::VTT,
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
            "json" => "Pandoc JSON",
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
            "srt" => "SubRip (SRT)",
            "vtt" => "WebVTT",
            "png-sequence-zip" => "PNG Sequence (ZIP)",
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
            "markdown" | "gfm" | "commonmark" | "commonmark_x" | "markdown_github"
            | "markdown_mmd" | "markdown_phpextra" | "markdown_strict" => "md",
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
            "png-sequence-zip" => "zip",
            // Media format ids match their file extensions.
            other => other,
        }
    }

    pub fn media_type(self) -> &'static str {
        match self.0 {
            "markdown" | "gfm" | "commonmark" | "commonmark_x" => "text/markdown",
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
            "srt" => "application/x-subrip",
            "vtt" => "text/vtt",
            "png-sequence-zip" => "application/zip",
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
                | "application/x-subrip"
                | "application/json"
                | "application/xml"
        ) || matches!(
            self.id(),
            "srt" | "vtt" | "plain" | "markdown" | "html" | "gfm"
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
            "png-zip" | "png_sequence" | "frames-zip" | "png-sequence-zip" => "png-sequence-zip",
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
    pub pdf: PdfInputOptions,
    /// When set and true, external converter processes should abort.
    ///
    /// Used by the shared batch runner for cooperative cancellation. Ignored by
    /// equality checks so option snapshots compare by engine knobs only.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Optional progress sink (not compared for equality).
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
            .field("pdf", &self.pdf)
            .field("cancel", &self.cancel.as_ref().map(|_| "<AtomicBool>"))
            .field(
                "progress",
                &self.progress.as_ref().map(|_| "<ProgressSink>"),
            )
            .finish()
    }
}

impl PartialEq for ConversionOptions {
    fn eq(&self, other: &Self) -> bool {
        self.ffmpeg == other.ffmpeg
            && self.markitdown == other.markitdown
            && self.pandoc == other.pandoc
            && self.defuddle == other.defuddle
            && self.docling == other.docling
            && self.pdf == other.pdf
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
        // crash never leaves a half-written final path.
        write_bytes_atomically(path.as_ref(), &self.bytes)
    }

    /// Human-readable result summary for UI previews (text excerpt or binary facts).
    pub fn preview_summary(&self) -> String {
        if self.format.is_text_previewable() {
            return text_preview_excerpt(&self.bytes);
        }
        binary_preview_summary(self)
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

static WRITE_TOKEN: AtomicU64 = AtomicU64::new(0);

fn file_stem_for_temp(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "output".into())
}

fn unique_temp_file_name(stem: &str, suffix: &str) -> String {
    let pid = std::process::id();
    let count = WRITE_TOKEN.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".{stem}.{pid}-{count}-{nanos}{suffix}")
}

/// Write `bytes` to `path` via a unique `*.shift-partial` sibling, then rename.
///
/// On failure the partial file is removed. The final path appears only after a
/// complete write so cancelled conversions do not leave truncated destinations.
pub fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> Result<(), ConversionError> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = parent.unwrap_or_else(|| Path::new("."));
    let stem = file_stem_for_temp(path);
    let partial = dir.join(unique_temp_file_name(&stem, ".shift-partial"));

    if let Err(error) = std::fs::write(&partial, bytes) {
        let _ = std::fs::remove_file(&partial);
        return Err(ConversionError::new(format!(
            "could not write {}: {error}",
            path.display()
        )));
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

/// Remove incomplete `*.shift-partial` siblings next to a planned destination.
pub fn remove_partial_outputs(planned: &Path) -> usize {
    let parent = planned
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = file_stem_for_temp(planned);
    let prefix = format!(".{stem}.");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix)
            && (name.ends_with(".shift-partial") || name.ends_with(".shift-bak"))
            && std::fs::remove_file(entry.path()).is_ok()
        {
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

fn binary_preview_summary(artifact: &ConversionArtifact) -> String {
    let size = format_byte_size(artifact.bytes.len() as u64);
    let pipeline = if artifact.pipeline.is_empty() {
        artifact.module_id.to_owned()
    } else {
        artifact.pipeline.join(" → ")
    };
    let kind = if ffmpeg::is_video_output(artifact.format) {
        "Video"
    } else if ffmpeg::is_audio_output(artifact.format) {
        "Audio"
    } else if ffmpeg::is_image_output(artifact.format) {
        "Image"
    } else if ffmpeg::is_subtitle_output(artifact.format) {
        "Subtitles"
    } else {
        "Binary"
    };
    format!(
        "{kind} · {} · {size}\nFile: {}\nEngine: {pipeline}\n\nNot shown inline — Download, drag, or Reveal after save.\nNo media player in Shift; open with your default app after saving.",
        artifact.format.label(),
        artifact.file_name,
    )
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
pub fn format_argv_display(argv: &[impl AsRef<str>]) -> String {
    argv.iter()
        .map(|part| {
            let part = part.as_ref();
            if part.is_empty()
                || part
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '"' | '\''))
            {
                format!("\"{}\"", part.replace('"', "\\\""))
            } else {
                part.to_owned()
            }
        })
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
pub fn redact_flag_value(parts: &mut [String], flag: &str, replacement: &str) {
    let mut index = 0;
    while index + 1 < parts.len() {
        if parts[index] == flag {
            parts[index + 1] = replacement.to_owned();
            index += 2;
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
    fn output_formats(&self) -> &'static [OutputFormat];
    /// Outputs which may be materialized and safely consumed by another module.
    fn chainable_output_formats(&self) -> &'static [OutputFormat];
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
    fn build_default() -> Self {
        Self::new()
            .with_module(MarkItDownModule::default())
            .with_module(PandocModule::default())
            .with_module(DefuddleModule::default())
            .with_module(DoclingModule::default())
            .with_module(FfmpegModule::default())
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
                if !first.output_formats().contains(&intermediate) {
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
        // PDF page-range / password preprocess (qpdf). If qpdf handles the
        // password, remove it from the module options so it never reaches an
        // external tool command line.
        let mut options = options.clone();
        let mut slice_guard: Option<TempDirGuard> = None;
        let convert_input = if is_pdf_path(input) && options.pdf.needs_preprocessing() {
            let sliced = extract_pdf_pages(
                input,
                options.pdf.page_from.unwrap_or(1),
                options.pdf.page_to,
                options.pdf.password.as_deref(),
                options.cancel.clone(),
            )?;
            options.pdf.password = None;
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
                // First hop may be FFmpeg (trim/encode). Pass the full options
                // snapshot on hop 2 so second-module knobs (e.g. MarkItDown
                // keep-data-uris) still apply; modules ignore foreign fields.
                let hop1 = first.convert(input, intermediate, &options)?;
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

            reachable.extend(first.output_formats().iter().copied());

            for &intermediate in first.chainable_output_formats() {
                if !first.output_formats().contains(&intermediate) {
                    continue;
                }
                for (second_index, second) in self.modules.iter().enumerate() {
                    if second_index == first_index {
                        continue;
                    }
                    if second
                        .input_extensions()
                        .iter()
                        .any(|candidate| intermediate.extension().eq_ignore_ascii_case(candidate))
                    {
                        reachable.extend(second.output_formats().iter().copied());
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
                let hop1 = first.convert_url(url, intermediate, options)?;
                let hop1 = ensure_direct_provenance(hop1, first.id());
                self.finish_chain(&hop1, output, second, options)
            }
        }
    }
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

struct TempDirGuard(PathBuf);

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

/// True when `left` and `right` name the same filesystem object.
///
/// Used to refuse writing conversion output over the selected source. When the
/// destination does not exist yet, compares the source's canonical path with
/// the destination's parent (canonicalized when possible) plus file name.
pub fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    if let (Ok(left), Ok(right)) = (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        return left == right;
    }

    let Ok(left_canonical) = std::fs::canonicalize(left) else {
        return false;
    };

    let right_parent = right
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let right_parent = match right_parent {
        Some(parent) => std::fs::canonicalize(parent).ok(),
        None => std::env::current_dir().ok(),
    };
    let Some(right_parent) = right_parent else {
        return false;
    };
    let Some(file_name) = right.file_name() else {
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
        fn output_formats(&self) -> &'static [OutputFormat] {
            self.outputs
        }
        fn chainable_output_formats(&self) -> &'static [OutputFormat] {
            self.chainable
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
        let partial = dir.join(".report.md.123.shift-partial");
        std::fs::write(&partial, b"half").unwrap();
        assert_eq!(remove_partial_outputs(&planned), 1);
        assert!(!partial.exists());
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
        assert!(summary.contains("Not shown inline"), "{summary}");
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
    fn all_catalog_matches_pandoc_plus_media() {
        assert_eq!(
            OutputFormat::ALL.len(),
            OutputFormat::PANDOC.len() + OutputFormat::MEDIA.len()
        );
        for format in OutputFormat::PANDOC {
            assert!(OutputFormat::ALL.contains(format));
        }
        for format in OutputFormat::MEDIA {
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
        // Video → audio (FFmpeg) → Markdown (MarkItDown) is a valid two-step route.
        assert!(video_outputs.contains(&OutputFormat::MARKDOWN));
        assert!(
            registry
                .module_for(Path::new("clip.mov"), OutputFormat::MARKDOWN)
                .is_none(),
            "Markdown is only available via the FFmpeg → MarkItDown chain"
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
            assert!(binary_preview.contains("Not shown inline"));
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

        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

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

        // SAFETY: serialized behind ENV_LOCK.
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
}
