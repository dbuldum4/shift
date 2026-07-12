//! Format conversion modules and capability-based dispatch.

mod defuddle;
mod docling;
mod markitdown;
mod pandoc;

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

pub use defuddle::{DefuddleModule, looks_like_url};
pub use docling::DoclingModule;
pub use markitdown::MarkItDownModule;
pub use pandoc::PandocModule;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OutputFormat(&'static str);

impl OutputFormat {
    pub const MARKDOWN: Self = Self("markdown");
    pub const HTML: Self = Self("html");
    pub const DOCX: Self = Self("docx");
    pub const PDF: Self = Self("pdf");
    pub const PPTX: Self = Self("pptx");
    pub const EPUB: Self = Self("epub");

    /// Every writer in Pandoc 3.10, ordered by practical end-user popularity.
    /// Closely related variants follow their best-known parent format.
    pub const ALL: &'static [Self] = &[
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
            _ => "text/plain",
        }
    }
}

impl std::str::FromStr for OutputFormat {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|format| {
                format.id().eq_ignore_ascii_case(value)
                    || format.extension().eq_ignore_ascii_case(value)
            })
            .ok_or_else(|| ConversionError::new(format!("unknown output format: {value}")))
    }
}

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
    fn convert(
        &self,
        input: &Path,
        output: OutputFormat,
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

impl Default for ConversionRegistry {
    fn default() -> Self {
        // MarkItDown stays first for fast broad Markdown. Docling fills PDF →
        // HTML/plain (and higher-quality Markdown when prioritized above
        // MarkItDown). Pandoc owns publishing writers; Defuddle owns URLs.
        Self::new()
            .with_module(MarkItDownModule::default())
            .with_module(PandocModule::default())
            .with_module(DefuddleModule::default())
            .with_module(DoclingModule::default())
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

    pub fn available_outputs(&self, input: &Path) -> Vec<OutputFormat> {
        OutputFormat::ALL
            .iter()
            .copied()
            .filter(|output| self.module_for(input, *output).is_some())
            .collect()
    }

    pub fn available_url_outputs(&self) -> Vec<OutputFormat> {
        OutputFormat::ALL
            .iter()
            .copied()
            .filter(|output| self.module_for_url(*output).is_some())
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
        let input = input.as_ref();
        if !input.is_file() {
            return Err(ConversionError::new(format!(
                "input is not a readable file: {}",
                input.display()
            )));
        }

        let module = self.module_for(input, output).ok_or_else(|| {
            let extension = input
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("(none)");
            ConversionError::new(format!(
                "no conversion module supports .{extension} to {}",
                output.label()
            ))
        })?;

        module.convert(input, output)
    }

    pub fn convert_url(
        &self,
        url: &str,
        output: OutputFormat,
    ) -> Result<ConversionArtifact, ConversionError> {
        let url = url.trim();
        if !looks_like_url(url) {
            return Err(ConversionError::new(format!(
                "not a valid http(s) URL: {url}"
            )));
        }

        let module = self.module_for_url(output).ok_or_else(|| {
            ConversionError::new(format!(
                "no conversion module supports URL conversion to {}",
                output.label()
            ))
        })?;

        module.convert_url(url, output)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversionError {
    message: String,
}

impl ConversionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ConversionError {}

pub fn default_output_path(input: &Path, output: OutputFormat) -> PathBuf {
    input.with_extension(output.extension())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!pdf_outputs.contains(&OutputFormat::DOCX));
        assert!(!pdf_outputs.contains(&OutputFormat::PDF));
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
    fn pandoc_output_catalog_has_no_duplicates() {
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
    fn url_conversion_routes_to_defuddle() {
        let registry = ConversionRegistry::default();
        assert_eq!(
            registry
                .module_for_url(OutputFormat::MARKDOWN)
                .unwrap()
                .id(),
            "defuddle"
        );
        assert_eq!(
            registry.available_url_outputs(),
            vec![OutputFormat::MARKDOWN, OutputFormat::HTML]
        );
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
}
