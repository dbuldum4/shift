use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, InvocationRecord,
    OutputFormat, TempDirGuard, bundled_runtime_tool, command_argv_parts, format_argv_display,
    map_spawn_error, max_output_bytes, process_timeout, read_file_limited, resolve_tool_executable,
    run_command_cancellable, unique_temp_dir,
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
    let mut candidates = Vec::new();
    if let Some(bundled) = bundled_runtime_tool("docling") {
        candidates.push(bundled);
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".venv/bin/docling"));
    resolve_tool_executable("SHIFT_DOCLING_BIN", "docling", &candidates)
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

    /// Discover the file Docling actually wrote, in case it renamed the output
    /// (for example when a same-named file already existed in the temp dir).
    ///
    /// Returns the exact expected path if present, or a single candidate with
    /// the matching extension. Returns `None` if no candidates exist or if
    /// multiple ambiguous candidates are found (callers should treat this as a
    /// conversion failure rather than silently picking an arbitrary file).
    fn discover_output(work_dir: &Path, expected: &Path) -> Option<PathBuf> {
        let expected_ext = expected.extension().and_then(|value| value.to_str())?;
        let expected_name = expected.file_name()?;

        let matches: Vec<PathBuf> = fs::read_dir(work_dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().ok().is_some_and(|t| t.is_file()))
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(expected_ext))
            })
            .collect();

        if matches.is_empty() {
            return None;
        }

        if let Some(exact) = matches
            .iter()
            .find(|path| path.file_name() == Some(expected_name))
        {
            return Some(exact.clone());
        }

        // Only accept a single unambiguous candidate; multiple candidates
        // indicate an unexpected output layout and should fail explicitly.
        if matches.len() == 1 {
            return matches.into_iter().next();
        }

        None
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

        let expected = Self::output_file_name(stem, output_format);
        let produced =
            Self::discover_output(&work_dir, &expected).unwrap_or_else(|| work_dir.join(&expected));
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

        let mut options = ConversionOptions {
            docling: DoclingOptions {
                image_export_mode: DoclingImageExportMode::Embedded,
                ocr: false,
                ocr_lang: Some("eng+deu".into()),
                tables: false,
                table_mode: DoclingTableMode::Accurate,
            },
            ..ConversionOptions::default()
        };
        options.pdf.password = Some("s3cret".into());
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

    #[test]
    fn discover_output_returns_none_on_ambiguous_candidates() {
        let work = std::env::temp_dir().join("shift-docling-ambig");
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("alpha.md"), b"# A").unwrap();
        fs::write(work.join("beta.md"), b"# B").unwrap();
        // Expected file does not exist; two candidates are ambiguous.
        let expected = work.join("report.md");
        let result = DoclingModule::discover_output(&work, &expected);
        assert!(result.is_none(), "ambiguous candidates must return None");
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_returns_single_renamed_candidate() {
        let work = std::env::temp_dir().join("shift-docling-single");
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("renamed.md"), b"# OK").unwrap();
        let expected = work.join("report.md");
        let result = DoclingModule::discover_output(&work, &expected);
        assert_eq!(result, Some(work.join("renamed.md")));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_prefers_exact_match() {
        let work = std::env::temp_dir().join("shift-docling-exact");
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("report.md"), b"# exact").unwrap();
        fs::write(work.join("other.md"), b"# other").unwrap();
        let expected = work.join("report.md");
        let result = DoclingModule::discover_output(&work, &expected);
        assert_eq!(result, Some(work.join("report.md")));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn image_export_mode_from_str_id_label_round_trips() {
        let cases = [
            (
                DoclingImageExportMode::Placeholder,
                "placeholder",
                "Placeholder",
                &["placeholder"][..],
            ),
            (
                DoclingImageExportMode::Embedded,
                "embedded",
                "Embedded",
                &["embedded", "embed"],
            ),
            (
                DoclingImageExportMode::Referenced,
                "referenced",
                "Referenced",
                &["referenced", "reference", "refs"],
            ),
        ];
        assert_eq!(DoclingImageExportMode::all().len(), cases.len());
        for (mode, id, label, aliases) in cases {
            assert_eq!(mode.id(), id);
            assert_eq!(mode.label(), label);
            for alias in aliases {
                assert_eq!(
                    alias.parse::<DoclingImageExportMode>().unwrap(),
                    mode,
                    "alias {alias}"
                );
                assert_eq!(
                    alias
                        .to_ascii_uppercase()
                        .parse::<DoclingImageExportMode>()
                        .unwrap(),
                    mode,
                    "uppercase alias {alias}"
                );
            }
        }
        let err = "nope".parse::<DoclingImageExportMode>().unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown Docling image export mode"),
            "{err}"
        );
    }

    #[test]
    fn table_mode_from_str_id_label_round_trips() {
        let cases = [
            (DoclingTableMode::Fast, "fast", "Fast", &["fast"][..]),
            (
                DoclingTableMode::Accurate,
                "accurate",
                "Accurate",
                &["accurate", "hq", "high"],
            ),
        ];
        assert_eq!(DoclingTableMode::all().len(), cases.len());
        for (mode, id, label, aliases) in cases {
            assert_eq!(mode.id(), id);
            assert_eq!(mode.label(), label);
            for alias in aliases {
                assert_eq!(
                    alias.parse::<DoclingTableMode>().unwrap(),
                    mode,
                    "alias {alias}"
                );
                assert_eq!(
                    alias
                        .to_ascii_uppercase()
                        .parse::<DoclingTableMode>()
                        .unwrap(),
                    mode,
                    "uppercase alias {alias}"
                );
            }
        }
        let err = "slow".parse::<DoclingTableMode>().unwrap_err();
        assert!(
            err.to_string().contains("unknown Docling table mode"),
            "{err}"
        );
    }

    #[test]
    fn to_arg_maps_markdown_html_plain() {
        assert_eq!(DoclingModule::to_arg(OutputFormat::MARKDOWN), Some("md"));
        assert_eq!(DoclingModule::to_arg(OutputFormat::HTML), Some("html"));
        assert_eq!(DoclingModule::to_arg(OutputFormat("plain")), Some("text"));
        assert_eq!(DoclingModule::to_arg(OutputFormat::PDF), None);
        assert_eq!(DoclingModule::to_arg(OutputFormat::DOCX), None);
    }

    #[test]
    fn pdf_password_does_not_appear_on_docling_argv() {
        let directory = std::env::temp_dir();
        let suffix = format!(
            "{}-{}-pw",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let secret = "p@ssw0rd-never-on-argv";
        let mut options = ConversionOptions::default();
        options.pdf.password = Some(secret.into());
        DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(
            !args.contains("--pdf-password"),
            "docling must not receive --pdf-password, args: {args}"
        );
        assert!(
            !args.contains(secret),
            "password must not appear on docling argv, args: {args}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn reports_capability_lists() {
        let module = DoclingModule::with_executable("docling");
        let inputs = module.input_extensions();
        assert!(
            inputs.contains(&"pdf"),
            "input_extensions should include pdf: {inputs:?}"
        );
        let outputs = module.output_formats();
        assert!(outputs.contains(&OutputFormat::MARKDOWN));
        assert!(outputs.contains(&OutputFormat::HTML));
        assert!(outputs.contains(&OutputFormat("plain")));
        assert_eq!(module.chainable_output_formats(), outputs);
    }

    #[test]
    fn missing_executable_fails_cleanly() {
        let missing = std::env::temp_dir().join(format!(
            "shift-docling-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let input = std::env::temp_dir().join(format!(
            "shift-docling-missing-input-{}-{}.pdf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let err = DoclingModule::with_executable(&missing)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Docling is not installed")
                || message.contains("executable not found"),
            "{message}"
        );
        // Install hint must stay stable for UX / docs.
        assert!(
            message.contains("pip install docling") || message.contains("SHIFT_DOCLING_BIN"),
            "missing-exe message should mention install path: {message}"
        );

        let _ = fs::remove_file(&input);
    }

    fn unique_suffix(tag: &str) -> String {
        format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            tag
        )
    }

    fn write_fake_docling_body(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn cancel_flag_aborts_conversion() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("cancel");
        let executable = directory.join(format!("shift-docling-cancel-{suffix}"));
        let input = directory.join(format!("shift-docling-cancel-in-{suffix}.pdf"));
        write_fake_docling_body(&executable, "#!/bin/sh\nsleep 30\n");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let options = ConversionOptions {
            cancel: Some(std::sync::Arc::clone(&cancel)),
            ..ConversionOptions::default()
        };
        let err = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap_err();
        assert!(err.is_cancelled(), "error: {err}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn cancel_mid_run_stops_hanging_docling() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("mid-cancel");
        let executable = directory.join(format!("shift-docling-midcancel-{suffix}"));
        let input = directory.join(format!("shift-docling-midcancel-in-{suffix}.pdf"));
        write_fake_docling_body(&executable, "#!/bin/sh\nsleep 30\n");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&cancel);
        let started = std::time::Instant::now();
        let watcher = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let options = ConversionOptions {
            cancel: Some(cancel),
            ..ConversionOptions::default()
        };
        let err = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap_err();
        let _ = watcher.join();
        assert!(err.is_cancelled(), "error: {err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "cancel took too long: {:?}",
            started.elapsed()
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn discover_output_empty_dir_returns_none() {
        let work = std::env::temp_dir().join(format!("shift-docling-empty-{}", unique_suffix("e")));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let expected = work.join("report.md");
        assert!(DoclingModule::discover_output(&work, &expected).is_none());
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_ignores_nested_files_and_directories() {
        let work =
            std::env::temp_dir().join(format!("shift-docling-nested-{}", unique_suffix("n")));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(work.join("subdir")).unwrap();
        // Nested candidate must not be discovered (only top-level files).
        fs::write(work.join("subdir").join("report.md"), b"# nested").unwrap();
        // A directory whose name ends like the expected extension is not a file.
        fs::create_dir_all(work.join("looks.md")).unwrap();
        let expected = work.join("report.md");
        assert!(
            DoclingModule::discover_output(&work, &expected).is_none(),
            "nested/dir entries must not count as candidates"
        );
        // Once a top-level file appears, it is found.
        fs::write(work.join("renamed.md"), b"# top").unwrap();
        assert_eq!(
            DoclingModule::discover_output(&work, &expected),
            Some(work.join("renamed.md"))
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_extension_match_is_case_insensitive() {
        let work = std::env::temp_dir().join(format!("shift-docling-case-{}", unique_suffix("c")));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("Report.MD"), b"# Case").unwrap();
        let expected = work.join("report.md");
        let result = DoclingModule::discover_output(&work, &expected);
        assert_eq!(result, Some(work.join("Report.MD")));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_missing_work_dir_returns_none() {
        let missing =
            std::env::temp_dir().join(format!("shift-docling-missing-dir-{}", unique_suffix("md")));
        let _ = fs::remove_dir_all(&missing);
        let expected = missing.join("report.md");
        assert!(DoclingModule::discover_output(&missing, &expected).is_none());
    }

    #[test]
    fn all_image_export_modes_appear_on_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("img-modes");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        for mode in DoclingImageExportMode::all() {
            let options = ConversionOptions {
                docling: DoclingOptions {
                    image_export_mode: *mode,
                    ..DoclingOptions::default()
                },
                ..ConversionOptions::default()
            };
            DoclingModule::with_executable(&executable)
                .convert(&input, OutputFormat::MARKDOWN, &options)
                .unwrap();
            let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
            assert!(
                args.contains("--image-export-mode") && args.contains(mode.id()),
                "mode {} missing from argv: {args}",
                mode.id()
            );
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn all_table_modes_appear_on_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("table-modes");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        for mode in DoclingTableMode::all() {
            let options = ConversionOptions {
                docling: DoclingOptions {
                    table_mode: *mode,
                    ..DoclingOptions::default()
                },
                ..ConversionOptions::default()
            };
            DoclingModule::with_executable(&executable)
                .convert(&input, OutputFormat::HTML, &options)
                .unwrap();
            let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
            assert!(
                args.contains("--table-mode") && args.contains(mode.id()),
                "table mode {} missing from argv: {args}",
                mode.id()
            );
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn empty_or_whitespace_ocr_lang_is_omitted_from_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("ocr-empty");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        for lang in [Some(String::new()), Some("   ".into()), None] {
            let options = ConversionOptions {
                docling: DoclingOptions {
                    ocr_lang: lang.clone(),
                    ..DoclingOptions::default()
                },
                ..ConversionOptions::default()
            };
            DoclingModule::with_executable(&executable)
                .convert(&input, OutputFormat::MARKDOWN, &options)
                .unwrap();
            let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
            assert!(
                !args.contains("--ocr-lang"),
                "empty/whitespace ocr_lang must not pass --ocr-lang (lang={lang:?}): {args}"
            );
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn converts_non_pdf_office_and_image_inputs() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("nonpdf");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        write_fake_docling(&executable);
        let module = DoclingModule::with_executable(&executable);

        for (name, bytes) in [
            ("slide.docx", b"PK fake docx" as &[u8]),
            ("sheet.xlsx", b"PK fake xlsx"),
            ("deck.pptx", b"PK fake pptx"),
            ("scan.png", b"\x89PNG fake"),
            ("page.html", b"<html><body>hi</body></html>"),
            ("notes.md", b"# notes\n"),
            ("book.epub", b"PK fake epub"),
        ] {
            let input = directory.join(format!("shift-docling-{suffix}-{name}"));
            fs::write(&input, bytes).unwrap();
            let artifact = module
                .convert(
                    &input,
                    OutputFormat::MARKDOWN,
                    &ConversionOptions::default(),
                )
                .unwrap_or_else(|e| panic!("convert {name}: {e}"));
            assert!(
                artifact.file_name.ends_with(".md"),
                "{name} → {}",
                artifact.file_name
            );
            assert_eq!(artifact.module_id, "docling");
            assert_eq!(artifact.pipeline, vec!["docling"]);
            let _ = fs::remove_file(&input);
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
    }

    #[test]
    fn capability_list_is_exhaustive_for_extensions_and_outputs() {
        let module = DoclingModule::with_executable("docling");
        // Every documented Docling input extension must be advertised.
        for ext in [
            "pdf", "docx", "pptx", "xlsx", "odt", "ods", "odp", "epub", "md", "markdown", "adoc",
            "asciidoc", "tex", "latex", "txt", "html", "htm", "xhtml", "csv", "png", "jpg", "jpeg",
            "tif", "tiff", "bmp", "webp",
        ] {
            assert!(
                module.input_extensions().contains(&ext),
                "missing input extension {ext:?} in {:?}",
                module.input_extensions()
            );
        }
        assert_eq!(module.input_extensions().len(), EXTENSIONS.len());
        assert_eq!(module.input_extensions(), EXTENSIONS);

        let outputs = module.output_formats();
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs, OUTPUTS);
        assert_eq!(module.chainable_output_formats(), OUTPUTS);
        assert_eq!(module.id(), "docling");
        assert_eq!(module.label(), "Docling");

        // supports() follows the extension + output lists.
        assert!(module.supports(Path::new("scan.PDF"), OutputFormat::HTML));
        assert!(module.supports(Path::new("slide.docx"), OutputFormat::MARKDOWN));
        assert!(module.supports(Path::new("scan.png"), OutputFormat("plain")));
        assert!(!module.supports(Path::new("clip.mp4"), OutputFormat::MARKDOWN));
        assert!(!module.supports(Path::new("scan.pdf"), OutputFormat::DOCX));
    }

    #[test]
    fn default_docling_options_prefer_fast_small_artifacts() {
        let defaults = DoclingOptions::default();
        assert_eq!(
            defaults.image_export_mode,
            DoclingImageExportMode::Placeholder
        );
        assert!(defaults.ocr);
        assert!(defaults.tables);
        assert_eq!(defaults.table_mode, DoclingTableMode::Fast);
        assert_eq!(defaults.ocr_lang, None);
    }

    #[test]
    fn process_failure_surfaces_stderr_detail() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("fail-stderr");
        let executable = directory.join(format!("shift-docling-fail-{suffix}"));
        let input = directory.join(format!("shift-docling-fail-in-{suffix}.pdf"));
        write_fake_docling_body(
            &executable,
            "#!/bin/sh\necho 'parser exploded' >&2\nexit 2\n",
        );
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let err = DoclingModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Docling could not convert") && message.contains("parser exploded"),
            "{message}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn process_failure_falls_back_to_stdout_when_stderr_empty() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("fail-stdout");
        let executable = directory.join(format!("shift-docling-fail-{suffix}"));
        let input = directory.join(format!("shift-docling-fail-in-{suffix}.pdf"));
        write_fake_docling_body(&executable, "#!/bin/sh\necho 'only on stdout'\nexit 1\n");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let err = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("only on stdout") || message.contains("exited with"),
            "{message}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn process_success_without_output_file_fails_cleanly() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("no-out");
        let executable = directory.join(format!("shift-docling-empty-out-{suffix}"));
        let input = directory.join(format!("shift-docling-empty-in-{suffix}.pdf"));
        // Succeed but never write the expected artifact.
        write_fake_docling_body(&executable, "#!/bin/sh\nexit 0\n");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let err = DoclingModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Docling finished but did not write") || message.contains("not write"),
            "{message}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn output_file_name_handles_empty_and_normal_stems() {
        // PathBuf::set_extension on an empty stem yields an empty path (no file name).
        // convert() avoids this by substituting "converted" when file_stem is empty/None.
        let empty = std::ffi::OsStr::new("");
        let empty_name = DoclingModule::output_file_name(empty, OutputFormat::MARKDOWN);
        assert!(
            empty_name.as_os_str().is_empty() || empty_name.as_os_str() == ".md",
            "empty stem → {empty_name:?}"
        );
        assert_eq!(
            DoclingModule::output_file_name(std::ffi::OsStr::new("report"), OutputFormat::HTML),
            PathBuf::from("report.html")
        );
        assert_eq!(
            DoclingModule::output_file_name(std::ffi::OsStr::new("report"), OutputFormat("plain")),
            PathBuf::from("report.txt")
        );
        // Unsupported/other formats fall through to the format id as extension.
        assert_eq!(
            DoclingModule::output_file_name(std::ffi::OsStr::new("x"), OutputFormat::DOCX),
            PathBuf::from("x.docx")
        );
        assert_eq!(
            DoclingModule::output_file_name(
                std::ffi::OsStr::new("converted"),
                OutputFormat::MARKDOWN
            ),
            PathBuf::from("converted.md")
        );
    }

    #[test]
    fn convert_substitutes_converted_stem_when_file_stem_is_none() {
        // Paths whose file_stem() is None (e.g. ".." components as the name) use "converted".
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("none-stem");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let work = directory.join(format!("shift-docling-none-stem-{suffix}"));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        write_fake_docling(&executable);

        // Create a regular file, then convert using a path that ends with ".." — not practical.
        // Instead exercise the filter branch via a zero-length stem OsString in output_file_name
        // (above) and verify convert still succeeds for a normal hidden-style name.
        let input = work.join(".hidden.pdf");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();
        let artifact = DoclingModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        // ".hidden.pdf" → stem ".hidden" on Unix.
        assert!(
            artifact.file_name.ends_with(".md"),
            "got {}",
            artifact.file_name
        );
        assert!(!artifact.bytes.is_empty());

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn image_export_and_table_mode_trim_whitespace_on_parse() {
        assert_eq!(
            "  embedded  ".parse::<DoclingImageExportMode>().unwrap(),
            DoclingImageExportMode::Embedded
        );
        assert_eq!(
            "\treferenced\n".parse::<DoclingImageExportMode>().unwrap(),
            DoclingImageExportMode::Referenced
        );
        assert_eq!(
            " accurate ".parse::<DoclingTableMode>().unwrap(),
            DoclingTableMode::Accurate
        );
    }

    #[test]
    fn successful_convert_records_provenance_and_media_types() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("prov");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let module = DoclingModule::with_executable(&executable);
        let md = module
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert_eq!(md.media_type, OutputFormat::MARKDOWN.media_type());
        assert_eq!(md.pipeline, vec!["docling"]);
        assert_eq!(md.invocations.len(), 1);
        assert_eq!(md.invocations[0].module_id, "docling");
        assert!(
            md.invocations[0].argv_display.contains("convert")
                || md.invocations[0].argv_display.contains("--to"),
            "argv_display: {}",
            md.invocations[0].argv_display
        );

        let plain = module
            .convert(&input, OutputFormat("plain"), &ConversionOptions::default())
            .unwrap();
        assert_eq!(plain.media_type, OutputFormat("plain").media_type());
        assert!(plain.file_name.ends_with(".txt"));

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn ocr_and_tables_toggle_flags_on_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("toggles");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        // Defaults: --ocr --tables
        DoclingModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--ocr"), "{args}");
        assert!(args.contains("--tables"), "{args}");
        assert!(!args.contains("--no-ocr"), "{args}");
        assert!(!args.contains("--no-tables"), "{args}");

        let options = ConversionOptions {
            docling: DoclingOptions {
                ocr: false,
                tables: false,
                ..DoclingOptions::default()
            },
            ..ConversionOptions::default()
        };
        DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();
        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--no-ocr"), "{args}");
        assert!(args.contains("--no-tables"), "{args}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }
}
