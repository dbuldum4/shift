//! Format conversion modules and capability-based dispatch.

mod batch;
mod defuddle;
mod diagnostics;
mod docling;
mod ffmpeg;
mod markitdown;
mod pandoc;
mod process;

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub use batch::{
    BatchEnqueueOptions, BatchEvent, BatchItem, BatchItemId, BatchItemState, BatchProgress,
    BatchQueue, BatchSource, BatchSummary, prepare_batch_destination, resolve_destination,
    run_batch, suggested_url_file_name, uniquify_destination,
};
pub use defuddle::{
    DefuddleModule, block_private_urls, looks_like_url, url_targets_non_public_host,
};
pub use diagnostics::{
    DiagnosticsReport, EngineDiagnostic, FormatAvailability, PdfEngineDiagnostic, Readiness,
    available_ready_outputs, available_ready_url_outputs, format_availability, supported_outputs,
};
pub use docling::DoclingModule;
pub use ffmpeg::{
    FfmpegEncodeMode, FfmpegModule, FfmpegOptions, FfmpegQuality, input_looks_like_media,
    is_audio_output, is_ffmpeg_output, is_image_output, is_subtitle_output, is_video_output,
};
pub use markitdown::MarkItDownModule;
pub use pandoc::{PandocModule, pdf_engine_candidates, resolve_pdf_engine};
pub use process::{
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_PROCESS_TIMEOUT, LimitedOutput, find_executable, is_runnable,
    max_output_bytes, process_timeout, read_file_limited, resolve_tool_executable,
    resolve_tool_path, run_command, run_command_cancellable,
};

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
            other => other,
        }
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

impl std::str::FromStr for OutputFormat {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let lowered = value.to_ascii_lowercase();
        let key = match lowered.as_str() {
            "jpeg" => "jpg",
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
#[derive(Clone, Debug, Default)]
pub struct ConversionOptions {
    pub ffmpeg: FfmpegOptions,
    /// When set and true, external converter processes should abort.
    ///
    /// Used by the shared batch runner for cooperative cancellation. Ignored by
    /// equality checks so option snapshots compare by engine knobs only.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl PartialEq for ConversionOptions {
    fn eq(&self, other: &Self) -> bool {
        self.ffmpeg == other.ffmpeg
    }
}

impl Eq for ConversionOptions {}

/// A completed conversion, independent of how it will be presented or saved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversionArtifact {
    pub file_name: String,
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
    pub format: OutputFormat,
    pub module_id: &'static str,
}

impl ConversionArtifact {
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<(), ConversionError> {
        std::fs::write(path.as_ref(), &self.bytes).map_err(|error| {
            ConversionError::new(format!(
                "could not write {}: {error}",
                path.as_ref().display()
            ))
        })
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
pub struct ConversionRegistry {
    modules: Vec<Box<dyn ConversionModule>>,
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
        // MarkItDown stays first for fast broad Markdown. Docling fills PDF →
        // HTML/plain (and higher-quality Markdown when prioritized above
        // MarkItDown). Pandoc owns publishing writers; Defuddle owns URLs.
        // FFmpeg owns audio/video container conversion (no document overlap).
        Self::new()
            .with_module(MarkItDownModule::default())
            .with_module(PandocModule::default())
            .with_module(DefuddleModule::default())
            .with_module(DoclingModule::default())
            .with_module(FfmpegModule::default())
    }
}

impl ConversionRegistry {
    pub fn new() -> Self {
        Self {
            modules: Vec::new(),
        }
    }

    pub fn with_module(mut self, module: impl ConversionModule + 'static) -> Self {
        self.modules.push(Box::new(module));
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
        self.modules.iter().map(Box::as_ref)
    }

    /// Whether a registered module uses this stable id.
    pub fn has_module(&self, id: &str) -> bool {
        self.modules.iter().any(|module| module.id() == id)
    }

    pub fn module_for(&self, input: &Path, output: OutputFormat) -> Option<&dyn ConversionModule> {
        self.modules
            .iter()
            .find(|module| module.supports(input, output))
            .map(Box::as_ref)
    }

    pub fn module_for_url(&self, output: OutputFormat) -> Option<&dyn ConversionModule> {
        self.modules
            .iter()
            .find(|module| module.supports_url(output))
            .map(Box::as_ref)
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
        match route {
            ConversionRoute::Direct(module) => module.convert(input, output, options),
            ConversionRoute::TwoStep {
                first,
                intermediate,
                second,
            } => {
                // First hop may be FFmpeg (trim/encode); the second hop drops
                // engine-specific knobs but keeps the cancel flag.
                let artifact = first.convert(input, intermediate, options)?;
                self.finish_chain(&artifact, output, second, options)
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
        let hop_options = ConversionOptions {
            cancel: options.cancel.clone(),
            ..ConversionOptions::default()
        };
        second.convert(&input, output, &hop_options)
    }

    pub fn available_outputs(&self, input: &Path) -> Vec<OutputFormat> {
        OutputFormat::ALL
            .iter()
            .copied()
            .filter(|output| self.route_for(input, *output).is_some())
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
            ConversionRoute::Direct(module) => module.convert_url(url, output, options),
            ConversionRoute::TwoStep {
                first,
                intermediate,
                second,
            } => {
                let artifact = first.convert_url(url, intermediate, options)?;
                self.finish_chain(&artifact, output, second, options)
            }
        }
    }
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

fn unique_temp_dir(prefix: &str) -> Result<PathBuf, ConversionError> {
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir(&path).map_err(|error| {
        ConversionError::new(format!(
            "could not create temporary conversion workspace {}: {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

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
            _options: &ConversionOptions,
        ) -> Result<ConversionArtifact, ConversionError> {
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
}
