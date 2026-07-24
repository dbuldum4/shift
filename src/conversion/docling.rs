use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, InvocationRecord,
    OutputFormat, command_argv_parts, format_argv_display, map_spawn_error, max_output_bytes,
    process_timeout, read_file_limited, resolve_tool_executable, run_command_cancellable,
    unique_temp_dir,
};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Document types Docling parses well without optional ASR extras.
/// See: https://docling-project.github.io/docling/usage/supported_formats/
const EXTENSIONS: &[&str] = &[
    // Office and publishing
    "pdf", "docx", "pptx", "xlsx", "odt", "ods", "odp", "epub", // Markup / text
    "md", "markdown", "adoc", "asciidoc", "tex", "latex", "txt", "html", "htm", "xhtml", "csv",
    // Images (layout / OCR pipeline)
    "png", "jpg", "jpeg", "tif", "tiff", "bmp", "webp",
];

/// Formats Docling can export that map cleanly onto Shift's `OutputFormat` catalog.
/// CLI `--to` values: md, html, text (see Docling CLI reference).
const OUTPUTS: &[OutputFormat] = &[
    OutputFormat::MARKDOWN,
    OutputFormat::HTML,
    OutputFormat("plain"),
];

/// How Docling places figures in Markdown/HTML exports.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoclingImageExportMode {
    /// Mark image positions only (small/fast desktop default).
    #[default]
    Placeholder,
    /// Embed images as base64 (larger artifacts).
    Embedded,
    /// Write PNGs beside the document and reference them.
    Referenced,
}

impl DoclingImageExportMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Placeholder => "placeholder",
            Self::Embedded => "embedded",
            Self::Referenced => "referenced",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Placeholder => "Placeholder",
            Self::Embedded => "Embedded",
            Self::Referenced => "Referenced",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Placeholder, Self::Embedded, Self::Referenced]
    }
}

impl std::str::FromStr for DoclingImageExportMode {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "placeholder" => Ok(Self::Placeholder),
            "embedded" | "embed" => Ok(Self::Embedded),
            "referenced" | "reference" | "refs" => Ok(Self::Referenced),
            other => Err(ConversionError::new(format!(
                "unknown Docling image export mode: {other} (try placeholder, embedded, referenced)"
            ))),
        }
    }
}

/// Table structure extraction mode for Docling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoclingTableMode {
    #[default]
    Fast,
    Accurate,
}

impl DoclingTableMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Accurate => "accurate",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Accurate => "Accurate",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Fast, Self::Accurate]
    }
}

impl std::str::FromStr for DoclingTableMode {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "accurate" | "hq" | "high" => Ok(Self::Accurate),
            other => Err(ConversionError::new(format!(
                "unknown Docling table mode: {other} (try fast, accurate)"
            ))),
        }
    }
}

/// Optional knobs for Docling. Defaults keep desktop conversions small/fast.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoclingOptions {
    pub image_export_mode: DoclingImageExportMode,
    /// Run OCR when the pipeline needs it (`--ocr` / `--no-ocr`).
    pub ocr: bool,
    /// OCR language codes when set (`--ocr-lang`), e.g. `eng` or `eng+deu`.
    pub ocr_lang: Option<String>,
    /// Extract table structure (`--tables` / `--no-tables`).
    pub tables: bool,
    pub table_mode: DoclingTableMode,
}

impl Default for DoclingOptions {
    fn default() -> Self {
        Self {
            // Shift prefers placeholder over Docling's upstream "embedded"
            // default so desktop artifacts stay small and conversions stay fast.
            image_export_mode: DoclingImageExportMode::Placeholder,
            ocr: true,
            ocr_lang: None,
            tables: true,
            table_mode: DoclingTableMode::Fast,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DoclingModule {
    executable: OsString,
}

impl Default for DoclingModule {
    fn default() -> Self {
        Self {
            executable: discover_executable(),
        }
    }
}

fn discover_executable() -> OsString {
    // Prefer a project-local venv when present (same convention as MarkItDown).
    // Absolute resolution matches diagnostics so GUI PATH quirks stay consistent.
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".venv/bin/docling");
    resolve_tool_executable("SHIFT_DOCLING_BIN", "docling", &[local])
}

impl DoclingModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn to_arg(output_format: OutputFormat) -> Option<&'static str> {
        match output_format.id() {
            "markdown" => Some("md"),
            "html" => Some("html"),
            "plain" => Some("text"),
            _ => None,
        }
    }

    fn output_file_name(stem: &std::ffi::OsStr, output_format: OutputFormat) -> PathBuf {
        // Docling writes `<stem>.md|html|txt` into `--output`.
        let extension = match output_format.id() {
            "markdown" => "md",
            "html" => "html",
            "plain" => "txt",
            other => other,
        };
        let mut output = PathBuf::from(stem);
        output.set_extension(extension);
        output
    }

    fn convert_with_cli(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        let to_arg = Self::to_arg(output_format).ok_or_else(|| {
            ConversionError::new(format!(
                "Docling does not produce {}",
                output_format.label()
            ))
        })?;

        let stem = input
            .file_stem()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsStr::new("converted"));

        let work_dir = unique_temp_dir("shift-docling")?;
        let cleanup = TempDirGuard(work_dir.clone());

        // Docling writes files into --output; it does not stream to stdout.
        // Explicit `convert` keeps the invocation stable if more subcommands are added.
        let knobs = &options.docling;
        let mut command = Command::new(&self.executable);
        command
            .arg("convert")
            .arg(input)
            .arg("--to")
            .arg(to_arg)
            .arg("--output")
            .arg(&work_dir)
            .arg("--image-export-mode")
            .arg(knobs.image_export_mode.id())
            .arg(if knobs.ocr { "--ocr" } else { "--no-ocr" })
            .arg(if knobs.tables {
                "--tables"
            } else {
                "--no-tables"
            })
            .arg("--table-mode")
            .arg(knobs.table_mode.id())
            .arg("--abort-on-error");
        if let Some(lang) = knobs
            .ocr_lang
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            command.arg("--ocr-lang").arg(lang);
        }

        let display_parts = command_argv_parts(&command);
        let invocation = InvocationRecord {
            module_id: self.id(),
            argv_display: format_argv_display(&display_parts),
        };

        let output = run_command_cancellable(
            command,
            process_timeout(),
            max_output_bytes(),
            options.cancel.clone(),
        )
        .map_err(|error| {
            map_spawn_error(
                error,
                "Docling is not installed. Install it with `pip install docling`, \
                 or set SHIFT_DOCLING_BIN.",
            )
        })?;

        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if detail.is_empty() {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if stdout.is_empty() {
                    format!("process exited with {}", output.status)
                } else {
                    stdout
                }
            } else {
                detail
            };
            return Err(ConversionError::new(format!(
                "Docling could not convert {}: {detail}",
                input.display()
            )));
        }

        let produced = work_dir.join(Self::output_file_name(stem, output_format));
        let bytes = read_file_limited(&produced, max_output_bytes()).map_err(|error| {
            ConversionError::new(format!(
                "Docling finished but did not write {}: {error}",
                produced.display()
            ))
        })?;

        // Drop temp dir after reading the artifact.
        drop(cleanup);

        Ok(ConversionArtifact {
            file_name: Self::output_file_name(stem, output_format)
                .to_string_lossy()
                .into_owned(),
            media_type: output_format.media_type(),
            bytes,
            format: output_format,
            module_id: self.id(),
            pipeline: vec![self.id()],
            invocations: vec![invocation],
        })
    }
}

impl ConversionModule for DoclingModule {
    fn id(&self) -> &'static str {
        "docling"
    }

    fn label(&self) -> &'static str {
        "Docling"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        EXTENSIONS
    }

    fn output_formats(&self) -> &'static [OutputFormat] {
        OUTPUTS
    }

    fn chainable_output_formats(&self) -> &'static [OutputFormat] {
        OUTPUTS
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !OUTPUTS.contains(&output_format) {
            return Err(ConversionError::new(format!(
                "Docling only produces Markdown, HTML, or plain text, not {}",
                output_format.label()
            )));
        }
        self.convert_with_cli(input, output_format, options)
    }
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    fn write_fake_docling(path: &Path) {
        // Mimic Docling CLI: honor `convert <input> --to <fmt> --output <dir>`
        // and write `<stem>.<ext>` into the output directory.
        let script = r#"#!/bin/sh
set -e
printf '%s\n' "$*" > "${0}.args"
to="md"
output="."
input=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    convert) shift; continue ;;
    --to) to="$2"; shift 2; continue ;;
    --output) output="$2"; shift 2; continue ;;
    --image-export-mode|--table-mode|--ocr-lang|--pdf-password) shift 2; continue ;;
    --ocr|--no-ocr|--tables|--no-tables|--abort-on-error) shift; continue ;;
    --*) shift; continue ;;
    *) input="$1"; shift; continue ;;
  esac
done
stem=$(basename "$input")
stem=${stem%.*}
case "$to" in
  md) ext=md; body='# From Docling' ;;
  html) ext=html; body='<p>From Docling</p>' ;;
  text) ext=txt; body='From Docling' ;;
  *) ext=out; body=unknown ;;
esac
printf '%s' "$body" > "$output/$stem.$ext"
"#;
        fs::write(path, script).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn converts_pdf_to_html_via_temp_output_dir() {
        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let artifact = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap();

        assert_eq!(
            artifact.file_name,
            format!("{}.html", input.file_stem().unwrap().to_string_lossy())
        );
        assert_eq!(artifact.media_type, "text/html");
        assert_eq!(artifact.bytes, b"<p>From Docling</p>");
        assert_eq!(artifact.module_id, "docling");
        assert_eq!(artifact.format, OutputFormat::HTML);

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("convert"), "args: {args}");
        assert!(args.contains("--to"), "args: {args}");
        assert!(args.contains("html"), "args: {args}");
        assert!(args.contains("--output"), "args: {args}");
        assert!(args.contains("--image-export-mode"), "args: {args}");
        assert!(args.contains("placeholder"), "args: {args}");
        assert!(args.contains("--ocr"), "args: {args}");
        assert!(args.contains("--tables"), "args: {args}");
        assert!(args.contains("--table-mode"), "args: {args}");
        assert!(args.contains("fast"), "args: {args}");
        assert!(args.contains("--abort-on-error"), "args: {args}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn honors_docling_options_in_cli_argv() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-opts", std::process::id());
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let options = ConversionOptions {
            docling: DoclingOptions {
                image_export_mode: DoclingImageExportMode::Embedded,
                ocr: false,
                ocr_lang: Some("eng+deu".into()),
                tables: false,
                table_mode: DoclingTableMode::Accurate,
            },
            ..ConversionOptions::default()
        };
        let artifact = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();
        assert_eq!(artifact.text(), Some("# From Docling"));

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("embedded"), "args: {args}");
        assert!(args.contains("--no-ocr"), "args: {args}");
        assert!(args.contains("--no-tables"), "args: {args}");
        assert!(args.contains("accurate"), "args: {args}");
        assert!(args.contains("--ocr-lang"), "args: {args}");
        assert!(args.contains("eng+deu"), "args: {args}");
        assert!(
            !args.contains("--pdf-password"),
            "PDF password should be handled by qpdf preprocess, not passed to docling, args: {args}"
        );
        assert!(
            !args.contains("s3cret"),
            "PDF password should not appear on the docling command line, args: {args}"
        );
        assert_eq!(artifact.invocations.len(), 1);

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn converts_pdf_to_markdown_and_plain_text() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-md", std::process::id());
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let module = DoclingModule::with_executable(&executable);

        let markdown = module
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert_eq!(markdown.text(), Some("# From Docling"));
        assert!(markdown.file_name.ends_with(".md"));

        let plain = module
            .convert(&input, OutputFormat("plain"), &ConversionOptions::default())
            .unwrap();
        assert_eq!(plain.text(), Some("From Docling"));
        assert!(plain.file_name.ends_with(".txt"));

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn preserves_non_utf8_stem_when_finding_docling_output() {
        let stem = OsString::from_vec(b"report-\xff".to_vec());
        let output = DoclingModule::output_file_name(&stem, OutputFormat::HTML);
        assert_eq!(output.into_os_string().into_vec(), b"report-\xff.html");
    }

    #[test]
    fn rejects_unsupported_output_formats() {
        let err = DoclingModule::with_executable("docling")
            .convert(
                Path::new("scan.pdf"),
                OutputFormat::DOCX,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("Word") || err.to_string().contains("DOCX"));
    }
}
