use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, InvocationRecord,
    OutputFormat, command_argv_parts, find_executable, format_argv_display, map_spawn_error,
    max_output_bytes, process_timeout, resolve_tool_executable, run_command_cancellable,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const INPUTS: &[&str] = &[
    "adoc",
    "asciidoc",
    "bib",
    "biblatex",
    "commonmark",
    "creole",
    "csv",
    "djot",
    "docbook",
    "docx",
    "dokuwiki",
    "enw",
    "epub",
    "fb2",
    "gfm",
    "haddock",
    "htm",
    "html",
    "ipynb",
    "jats",
    "jira",
    "json",
    "latex",
    "man",
    "markdown",
    "md",
    "mdoc",
    "mediawiki",
    "muse",
    "native",
    "odt",
    "opml",
    "org",
    "pod",
    "pptx",
    "ris",
    "rst",
    "rtf",
    "t2t",
    "tex",
    "textile",
    "tikiwiki",
    "tsv",
    "twiki",
    "typ",
    "typst",
    "vimwiki",
    "wiki",
    "xlsx",
    "xml",
];

/// PDF engines Pandoc can invoke, ordered for new installations.
///
/// Typst and Tectonic are preferred because they are small single-binary
/// installs. Classic LaTeX engines remain available when present.
const PDF_ENGINE_CANDIDATES: &[&str] = &[
    "typst",
    "tectonic",
    "xelatex",
    "lualatex",
    "pdflatex",
    "weasyprint",
    "wkhtmltopdf",
    "prince",
    "context",
];

/// Optional knobs for Pandoc writers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PandocOptions {
    /// Per-conversion PDF engine override (`--pdf-engine`).
    ///
    /// When set, wins over `SHIFT_PDF_ENGINE` and auto-discovery.
    pub pdf_engine: Option<String>,
    /// Produce a standalone document (`-s` / `--standalone`).
    pub standalone: bool,
    /// Include a table of contents (`--toc`).
    pub toc: bool,
    /// Reference DOCX/ODT for styles when writing those formats (`--reference-doc`).
    pub reference_doc: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct PandocModule {
    executable: OsString,
}

impl Default for PandocModule {
    fn default() -> Self {
        Self {
            // Absolute path when found so GUI apps with a minimal PATH match
            // diagnostics readiness (PATH + common_bin_dirs).
            executable: resolve_tool_executable("SHIFT_PANDOC_BIN", "pandoc", &[]),
        }
    }
}

impl PandocModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
}

impl ConversionModule for PandocModule {
    fn id(&self) -> &'static str {
        "pandoc"
    }

    fn label(&self) -> &'static str {
        "Pandoc"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        INPUTS
    }

    fn output_formats(&self) -> &'static [OutputFormat] {
        OutputFormat::PANDOC
    }

    fn chainable_output_formats(&self) -> &'static [OutputFormat] {
        OutputFormat::PANDOC
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        let target = output_format.id();
        let input_format = input
            .extension()
            .and_then(|extension| extension.to_str())
            .map(pandoc_input_format)
            .unwrap_or("markdown");
        let mut command = Command::new(&self.executable);
        command
            .arg(input)
            .arg("--from")
            .arg(input_format)
            .arg("--to")
            .arg(target)
            .arg("--output")
            .arg("-");

        if options.pandoc.standalone {
            command.arg("--standalone");
        }
        if options.pandoc.toc {
            command.arg("--toc");
        }
        if let Some(reference) = options.pandoc.reference_doc.as_ref() {
            command.arg("--reference-doc").arg(reference);
        }

        // Pandoc's PDF writer always shells out to an external engine. Default
        // is pdflatex, which is rarely present on a fresh machine. Resolve a
        // lighter engine (Typst first) so DOCX → PDF works after a normal
        // `brew install pandoc typst` setup.
        if output_format == OutputFormat::PDF {
            let engine = resolve_pdf_engine(options.pandoc.pdf_engine.as_deref())?;
            command.arg("--pdf-engine").arg(&engine);
        }

        let invocation = InvocationRecord {
            module_id: self.id(),
            argv_display: format_argv_display(&command_argv_parts(&command)),
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
                "Pandoc is not installed. Install it with `brew install pandoc`, or set SHIFT_PANDOC_BIN.",
            )
        })?;

        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(ConversionError::new(format!(
                "Pandoc could not convert {} to {}: {}",
                input.display(),
                output_format.label(),
                if detail.is_empty() {
                    output.status.to_string()
                } else {
                    detail
                }
            )));
        }

        let stem = input
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("converted");
        Ok(ConversionArtifact {
            file_name: format!("{stem}.{}", output_format.extension()),
            media_type: output_format.media_type(),
            bytes: output.stdout,
            format: output_format,
            module_id: self.id(),
            pipeline: vec![self.id()],
            invocations: vec![invocation],
        })
    }
}

fn pandoc_input_format(extension: &str) -> &str {
    match extension.to_ascii_lowercase().as_str() {
        "adoc" => "asciidoc",
        "bib" => "bibtex",
        "enw" => "endnotexml",
        "htm" => "html",
        "md" | "markdown" | "txt" => "markdown",
        "tex" => "latex",
        "typ" => "typst",
        "wiki" => "mediawiki",
        _ => extension,
    }
}

/// PDF engines Pandoc may invoke (public for diagnostics).
pub fn pdf_engine_candidates() -> &'static [&'static str] {
    PDF_ENGINE_CANDIDATES
}

/// Choose a PDF engine for Pandoc.
///
/// Order of preference:
/// 1. Per-conversion override (`PandocOptions::pdf_engine`) when non-empty
/// 2. `SHIFT_PDF_ENGINE` when set (name or absolute path)
/// 3. First candidate found on `PATH` / common install locations
pub fn resolve_pdf_engine(override_engine: Option<&str>) -> Result<OsString, ConversionError> {
    if let Some(engine) = override_engine
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(OsString::from(engine));
    }

    if let Some(engine) = std::env::var_os("SHIFT_PDF_ENGINE") {
        if !engine.is_empty() {
            return Ok(engine);
        }
    }

    for name in PDF_ENGINE_CANDIDATES {
        if let Some(path) = find_executable(name) {
            return Ok(path.into_os_string());
        }
    }

    Err(ConversionError::new(
        "No PDF engine found for Pandoc. For new installations, install Typst \
         (lightweight, recommended) with `brew install typst`, or a TeX \
         distribution such as `brew install --cask basictex`. You can also set \
         SHIFT_PDF_ENGINE to a specific engine binary.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // Serializes env-mutating tests so parallel cargo test doesn't race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn advertises_publishing_outputs_for_docx() {
        let module = PandocModule::default();
        assert!(module.supports(Path::new("report.docx"), OutputFormat::HTML));
        assert!(!module.supports(Path::new("scan.pdf"), OutputFormat::HTML));
    }

    #[test]
    fn catalog_matches_the_installed_pandoc_writers() {
        let Ok(output) = Command::new("pandoc").arg("--list-output-formats").output() else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let installed = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        let registered = OutputFormat::PANDOC
            .iter()
            .map(|format| format.id().to_owned())
            .collect::<HashSet<_>>();
        assert_eq!(registered, installed);
    }

    #[cfg(unix)]
    #[test]
    fn captures_binary_output_as_the_requested_artifact() {
        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-pandoc-test-{suffix}"));
        let input = directory.join(format!("shift-pandoc-input-{suffix}.docx"));
        std::fs::write(&executable, "#!/bin/sh\nprintf 'fake-docx'").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "source").unwrap();

        let artifact = PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::DOCX, &ConversionOptions::default())
            .unwrap();
        assert_eq!(artifact.bytes, b"fake-docx");
        assert_eq!(artifact.format, OutputFormat::DOCX);
        assert_eq!(artifact.module_id, "pandoc");

        std::fs::remove_file(executable).unwrap();
        std::fs::remove_file(input).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn passes_standalone_and_toc_options() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-opts", std::process::id());
        let executable = directory.join(format!("shift-pandoc-opts-{suffix}"));
        let input = directory.join(format!("shift-pandoc-input-{suffix}.md"));
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '<p>ok</p>'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "# hi").unwrap();

        let reference = directory.join(format!("shift-pandoc-ref-{suffix}.docx"));
        std::fs::write(&reference, "ref").unwrap();
        let options = ConversionOptions {
            pandoc: PandocOptions {
                standalone: true,
                toc: true,
                pdf_engine: None,
                reference_doc: Some(reference.clone()),
            },
            ..ConversionOptions::default()
        };
        PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &options)
            .unwrap();

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--standalone"), "args: {args}");
        assert!(args.contains("--toc"), "args: {args}");
        assert!(args.contains("--reference-doc"), "args: {args}");
        assert!(
            args.contains(reference.to_string_lossy().as_ref()),
            "args: {args}"
        );

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&reference);
    }

    #[test]
    fn pdf_engine_env_override_wins() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: serialized behind ENV_LOCK for the duration of this test.
        unsafe {
            std::env::set_var("SHIFT_PDF_ENGINE", "/custom/bin/typst");
        }
        let engine = resolve_pdf_engine(None).unwrap();
        assert_eq!(engine, OsString::from("/custom/bin/typst"));
        // Per-conversion override wins over env.
        let engine = resolve_pdf_engine(Some("xelatex")).unwrap();
        assert_eq!(engine, OsString::from("xelatex"));
        unsafe {
            std::env::remove_var("SHIFT_PDF_ENGINE");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pdf_conversion_passes_resolved_pdf_engine() {
        let _guard = ENV_LOCK.lock().unwrap();
        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-pandoc-pdf-test-{suffix}"));
        let input = directory.join(format!("shift-pandoc-pdf-input-{suffix}.docx"));
        // Echo argv so we can assert --pdf-engine was supplied.
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s' \"$*\" > /dev/null\nprintf '%%PDF-1.4 fake'\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_PDF_ENGINE", "typst");
        }

        // Capture argv by wrapping with a logger script.
        let wrapper = directory.join(format!("shift-pandoc-pdf-wrapper-{suffix}"));
        let log = directory.join(format!("shift-pandoc-pdf-args-{suffix}.txt"));
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexec '{}' \"$@\"\n",
                log.display(),
                executable.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions).unwrap();

        let artifact = PandocModule::with_executable(&wrapper)
            .convert(&input, OutputFormat::PDF, &ConversionOptions::default())
            .unwrap();
        assert!(artifact.bytes.starts_with(b"%PDF"));
        assert_eq!(artifact.format, OutputFormat::PDF);

        let args = std::fs::read_to_string(&log).unwrap();
        assert!(
            args.contains("--pdf-engine"),
            "expected --pdf-engine in argv, got:\n{args}"
        );
        assert!(
            args.lines().any(|line| line == "typst")
                || args.contains("--pdf-engine\ntypst")
                || args.contains("typst"),
            "expected typst engine in argv, got:\n{args}"
        );

        unsafe {
            std::env::remove_var("SHIFT_PDF_ENGINE");
        }
        let _ = std::fs::remove_file(executable);
        let _ = std::fs::remove_file(wrapper);
        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(log);
    }

    #[test]
    fn pdf_engine_candidates_prefer_typst() {
        assert_eq!(PDF_ENGINE_CANDIDATES[0], "typst");
        assert!(PDF_ENGINE_CANDIDATES.contains(&"pdflatex"));
    }
}
