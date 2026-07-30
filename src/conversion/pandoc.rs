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
    "txt",
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
    /// Parse Pandoc citation syntax (`@key`) in Markdown inputs.
    ///
    /// Off by default: bare `@` is far more often a package name, handle, or
    /// mention than an academic cite, and Typst PDF fails hard without a
    /// bibliography. When true, uses the default `markdown` reader (with
    /// citations enabled).
    pub citations: bool,
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

/// Outputs from Pandoc that downstream modules (MarkItDown, Defuddle, Docling)
/// can consume as inputs for a second conversion hop.
const CHAINABLE: &[OutputFormat] = &[
    OutputFormat::MARKDOWN,
    OutputFormat::HTML,
    OutputFormat("plain"),
];

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

    fn output_formats(&self) -> &[OutputFormat] {
        OutputFormat::PANDOC
    }

    fn chainable_output_formats(&self) -> &[OutputFormat] {
        CHAINABLE
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !OutputFormat::PANDOC.contains(&output_format) {
            return Err(ConversionError::new(format!(
                "Pandoc cannot produce {}",
                output_format.label()
            )));
        }
        let target = output_format.id();
        let input_format = input
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| pandoc_input_format(extension, options.pandoc.citations))
            .unwrap_or_else(|| pandoc_markdown_from(options.pandoc.citations));
        let mut command = Command::new(&self.executable);
        command
            .arg(input)
            .arg("--from")
            .arg(&input_format)
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
            let reference = validate_reference_doc(reference)?;
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

/// Pandoc `--from` for Markdown-family inputs, with optional citations.
fn pandoc_markdown_from(citations: bool) -> String {
    if citations {
        "markdown".into()
    } else {
        // Disable the citations extension so `@pkg` stays literal text.
        "markdown-citations".into()
    }
}

fn pandoc_input_format(extension: &str, citations: bool) -> String {
    let lower = extension.to_ascii_lowercase();
    match lower.as_str() {
        "adoc" => "asciidoc".into(),
        "bib" => "bibtex".into(),
        "enw" => "endnotexml".into(),
        "htm" => "html".into(),
        "md" | "markdown" | "txt" => pandoc_markdown_from(citations),
        "tex" => "latex".into(),
        "typ" => "typst".into(),
        "wiki" => "mediawiki".into(),
        _ => extension.to_owned(),
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

/// Validate a Pandoc `--reference-doc` path before it reaches the command line.
///
/// Rejects missing files, directories, and paths that escape the filesystem
/// root. Relative paths are resolved against the current directory.
fn validate_reference_doc(path: &Path) -> Result<PathBuf, ConversionError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                ConversionError::new(format!(
                    "could not resolve reference document path: {error}"
                ))
            })?
            .join(path)
    };

    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ConversionError::new(format!(
            "reference document path cannot contain parent-directory references: {}",
            path.display()
        )));
    }

    if !path.is_file() {
        return Err(ConversionError::new(format!(
            "reference document is not a readable file: {}",
            path.display()
        )));
    }

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // Serializes env-mutating tests so parallel cargo test doesn't race.

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
                citations: false,
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
            args.contains("markdown-citations"),
            "default Markdown should disable citations, args: {args}"
        );
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
    fn markdown_input_format_disables_citations_by_default() {
        assert_eq!(pandoc_input_format("md", false), "markdown-citations");
        assert_eq!(pandoc_input_format("MD", false), "markdown-citations");
        assert_eq!(pandoc_input_format("markdown", false), "markdown-citations");
        assert_eq!(pandoc_input_format("txt", false), "markdown-citations");
        assert_eq!(pandoc_input_format("md", true), "markdown");
        assert_eq!(pandoc_input_format("docx", false), "docx");
        assert_eq!(pandoc_markdown_from(false), "markdown-citations");
        assert_eq!(pandoc_markdown_from(true), "markdown");
    }

    #[cfg(unix)]
    #[test]
    fn citations_option_enables_markdown_cite_reader() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-cite", std::process::id());
        let executable = directory.join(format!("shift-pandoc-cite-{suffix}"));
        let input = directory.join(format!("shift-pandoc-input-{suffix}.md"));
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '<p>ok</p>'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "See @smith2020.").unwrap();

        let options = ConversionOptions {
            pandoc: PandocOptions {
                citations: true,
                ..PandocOptions::default()
            },
            ..ConversionOptions::default()
        };
        PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &options)
            .unwrap();

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        // Reader is exactly `markdown` (citations on), not `markdown-citations`.
        assert!(
            args.contains("--from markdown"),
            "expected --from markdown with citations on, args: {args}"
        );
        assert!(
            !args.contains("markdown-citations"),
            "citations on must not use markdown-citations, args: {args}"
        );

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn pdf_engine_env_override_wins() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized behind crate::ENV_LOCK for the duration of this test.
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    #[test]
    fn reference_doc_validation_rejects_missing_and_parent_dir() {
        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let valid = directory.join(format!("shift-pandoc-ref-valid-{suffix}.docx"));
        std::fs::write(&valid, "ref").unwrap();
        assert!(validate_reference_doc(&valid).is_ok());

        let missing = directory.join(format!("shift-pandoc-ref-missing-{suffix}.docx"));
        assert!(validate_reference_doc(&missing).is_err());

        let parent_dir = PathBuf::from(format!("../shift-pandoc-ref-traversal-{suffix}.docx"));
        assert!(validate_reference_doc(&parent_dir).is_err());

        let _ = std::fs::remove_file(&valid);
    }

    fn unique_suffix(tag: &str) -> String {
        format!(
            "{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[cfg(unix)]
    fn write_fake_pandoc(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn capability_lists_cover_inputs_outputs_and_chainable() {
        let module = PandocModule::with_executable("pandoc");
        assert_eq!(module.id(), "pandoc");
        assert_eq!(module.label(), "Pandoc");

        let inputs = module.input_extensions();
        for required in [
            "docx", "md", "html", "epub", "rst", "odt", "tex", "org", "rtf", "ipynb", "pptx",
            "xlsx", "csv", "bib", "typ", "wiki",
        ] {
            assert!(
                inputs.contains(&required),
                "INPUTS missing {required}: {inputs:?}"
            );
        }
        // No duplicates in capability lists.
        let mut seen = HashSet::new();
        for ext in inputs {
            assert!(seen.insert(*ext), "duplicate input extension: {ext}");
        }

        let outputs = module.output_formats();
        assert!(outputs.contains(&OutputFormat::MARKDOWN));
        assert!(outputs.contains(&OutputFormat::HTML));
        assert!(outputs.contains(&OutputFormat::PDF));
        assert!(outputs.contains(&OutputFormat::DOCX));
        assert!(outputs.contains(&OutputFormat::EPUB));
        assert!(outputs.contains(&OutputFormat("plain")));
        assert_eq!(outputs, OutputFormat::PANDOC);

        let chainable = module.chainable_output_formats();
        assert!(chainable.contains(&OutputFormat::MARKDOWN));
        assert!(chainable.contains(&OutputFormat::HTML));
        assert!(chainable.contains(&OutputFormat("plain")));
        // Chainable is a subset of outputs.
        for format in chainable {
            assert!(
                outputs.contains(format),
                "chainable {format:?} not in outputs"
            );
        }
        // PDF/DOCX are not chainable intermediate hops for other modules.
        assert!(!chainable.contains(&OutputFormat::PDF));
        assert!(!chainable.contains(&OutputFormat::DOCX));
    }

    #[test]
    fn supports_accepts_known_inputs_and_rejects_unknown() {
        let module = PandocModule::with_executable("pandoc");
        assert!(module.supports(Path::new("paper.odt"), OutputFormat::PDF));
        assert!(module.supports(Path::new("notes.RST"), OutputFormat::HTML));
        assert!(module.supports(Path::new("book.epub"), OutputFormat::MARKDOWN));
        assert!(module.supports(Path::new("sheet.xlsx"), OutputFormat("plain")));
        assert!(!module.supports(Path::new("clip.mp4"), OutputFormat::HTML));
        assert!(!module.supports(Path::new("track.wav"), OutputFormat::MARKDOWN));
        assert!(!module.supports(Path::new("noext"), OutputFormat::HTML));
    }

    #[test]
    fn rejects_unsupported_output_format_before_spawn() {
        let module = PandocModule::with_executable("/definitely/missing/pandoc-binary");
        let err = module
            .convert(
                Path::new("doc.docx"),
                OutputFormat::MP3,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("Pandoc cannot produce"),
            "expected unsupported format error, got: {err}"
        );
        // Media zip is also out of scope.
        let err = module
            .convert(
                Path::new("doc.docx"),
                OutputFormat::PNG_SEQUENCE_ZIP,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("Pandoc cannot produce"));
    }

    #[cfg(unix)]
    #[test]
    fn missing_executable_maps_to_install_hint() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("missing");
        let missing = directory.join(format!("shift-pandoc-missing-{suffix}"));
        let input = directory.join(format!("shift-pandoc-missing-in-{suffix}.md"));
        std::fs::write(&input, "# hi").unwrap();
        let _ = std::fs::remove_file(&missing);

        let err = PandocModule::with_executable(&missing)
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Pandoc is not installed") || message.contains("executable not found"),
            "{message}"
        );
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn reference_doc_rejects_directory_and_nested_parent_components() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("ref-dir");
        let as_dir = directory.join(format!("shift-pandoc-ref-dir-{suffix}"));
        std::fs::create_dir_all(&as_dir).unwrap();
        let err = validate_reference_doc(&as_dir).unwrap_err();
        assert!(
            err.to_string().contains("not a readable file"),
            "directory must be rejected: {err}"
        );

        // Absolute path that still contains a ParentDir component after join.
        let with_parent = directory
            .join("nested")
            .join("..")
            .join(format!("shift-pandoc-ref-parent-{suffix}.docx"));
        // Even if the file exists, parent-dir components are rejected.
        std::fs::write(
            directory.join(format!("shift-pandoc-ref-parent-{suffix}.docx")),
            "ref",
        )
        .unwrap();
        // Path may normalize; construct a path that retains ParentDir.
        let mut raw = directory.clone();
        raw.push("sub");
        raw.push("..");
        raw.push(format!("shift-pandoc-ref-parent-{suffix}.docx"));
        // std::path keeps ParentDir components until canonicalize.
        assert!(
            raw.components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "test setup should retain ParentDir: {}",
            raw.display()
        );
        let err = validate_reference_doc(&raw).unwrap_err();
        assert!(
            err.to_string().contains("parent-directory"),
            "parent-dir path must be rejected: {err}"
        );

        let _ = std::fs::remove_dir(&as_dir);
        let _ =
            std::fs::remove_file(directory.join(format!("shift-pandoc-ref-parent-{suffix}.docx")));
        let _ = with_parent; // silence unused
    }

    #[cfg(unix)]
    #[test]
    fn reference_doc_relative_path_resolves_against_cwd() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("ref-rel");
        let work = directory.join(format!("shift-pandoc-ref-work-{suffix}"));
        std::fs::create_dir_all(&work).unwrap();
        let ref_name = "styles.docx";
        std::fs::write(work.join(ref_name), "ref-bytes").unwrap();

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&work).unwrap();
        let resolved = validate_reference_doc(Path::new(ref_name)).unwrap();
        assert!(resolved.is_file());
        assert!(resolved.ends_with(ref_name));
        std::env::set_current_dir(previous).unwrap();

        let _ = std::fs::remove_dir_all(&work);
    }

    #[cfg(unix)]
    #[test]
    fn citations_standalone_toc_combinations_on_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("combo");
        let executable = directory.join(format!("shift-pandoc-combo-{suffix}"));
        let input = directory.join(format!("shift-pandoc-combo-in-{suffix}.md"));
        write_fake_pandoc(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf 'ok'",
        );
        std::fs::write(&input, "# Title\n\nSee @key.").unwrap();

        // standalone only
        let options = ConversionOptions {
            pandoc: PandocOptions {
                standalone: true,
                toc: false,
                citations: false,
                ..PandocOptions::default()
            },
            ..ConversionOptions::default()
        };
        PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &options)
            .unwrap();
        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--standalone"), "args: {args}");
        assert!(!args.contains("--toc"), "args: {args}");
        assert!(args.contains("markdown-citations"), "args: {args}");

        // toc only + citations on
        let options = ConversionOptions {
            pandoc: PandocOptions {
                standalone: false,
                toc: true,
                citations: true,
                ..PandocOptions::default()
            },
            ..ConversionOptions::default()
        };
        PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &options)
            .unwrap();
        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(!args.contains("--standalone"), "args: {args}");
        assert!(args.contains("--toc"), "args: {args}");
        assert!(
            args.contains("--from markdown") && !args.contains("markdown-citations"),
            "args: {args}"
        );

        // all flags together
        let options = ConversionOptions {
            pandoc: PandocOptions {
                standalone: true,
                toc: true,
                citations: true,
                ..PandocOptions::default()
            },
            ..ConversionOptions::default()
        };
        PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &options)
            .unwrap();
        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--standalone"), "args: {args}");
        assert!(args.contains("--toc"), "args: {args}");

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn plain_html_epub_docx_outputs_with_fake_binary() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("formats");
        let executable = directory.join(format!("shift-pandoc-formats-{suffix}"));
        let input = directory.join(format!("shift-pandoc-formats-in-{suffix}.md"));
        write_fake_pandoc(
            &executable,
            "#!/bin/sh\n# Echo a format-specific payload based on --to value.\nto=\"\"\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--to\" ]; then to=\"$a\"; fi\n  prev=\"$a\"\ndone\ncase \"$to\" in\n  plain) printf 'plain-text-body' ;;\n  html) printf '<p>html</p>' ;;\n  epub) printf 'PK-epub-fake' ;;\n  docx) printf 'PK-docx-fake' ;;\n  *) printf 'other' ;;\nesac\n",
        );
        std::fs::write(&input, "# src").unwrap();
        let module = PandocModule::with_executable(&executable);

        let plain = module
            .convert(&input, OutputFormat("plain"), &ConversionOptions::default())
            .unwrap();
        assert_eq!(plain.bytes, b"plain-text-body");
        assert_eq!(plain.format, OutputFormat("plain"));
        assert!(
            plain
                .file_name
                .ends_with(&format!(".{}", OutputFormat("plain").extension()))
        );

        let html = module
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap();
        assert_eq!(html.bytes, b"<p>html</p>");
        assert_eq!(html.format, OutputFormat::HTML);
        assert!(html.file_name.ends_with(".html"));

        let epub = module
            .convert(&input, OutputFormat::EPUB, &ConversionOptions::default())
            .unwrap();
        assert_eq!(epub.bytes, b"PK-epub-fake");
        assert_eq!(epub.format, OutputFormat::EPUB);
        assert!(epub.file_name.ends_with(".epub"));

        let docx = module
            .convert(&input, OutputFormat::DOCX, &ConversionOptions::default())
            .unwrap();
        assert_eq!(docx.bytes, b"PK-docx-fake");
        assert_eq!(docx.format, OutputFormat::DOCX);
        assert!(docx.file_name.ends_with(".docx"));

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn pdf_engine_candidates_list_properties() {
        let candidates = pdf_engine_candidates();
        assert_eq!(candidates, PDF_ENGINE_CANDIDATES);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0], "typst");
        assert_eq!(candidates[1], "tectonic");
        // Classic LaTeX engines remain available.
        for engine in ["xelatex", "lualatex", "pdflatex"] {
            assert!(
                candidates.contains(&engine),
                "missing LaTeX engine {engine} in {candidates:?}"
            );
        }
        for engine in ["weasyprint", "wkhtmltopdf", "prince", "context"] {
            assert!(
                candidates.contains(&engine),
                "missing alternate engine {engine} in {candidates:?}"
            );
        }
        // No duplicates.
        let mut seen = HashSet::new();
        for name in candidates {
            assert!(seen.insert(*name), "duplicate pdf engine: {name}");
        }
        // All entries are non-empty bare names (no path separators).
        for name in candidates {
            assert!(!name.is_empty());
            assert!(!name.contains('/'));
            assert!(!name.contains('\\'));
        }
    }

    #[test]
    fn resolve_pdf_engine_override_paths_and_whitespace() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("SHIFT_PDF_ENGINE");
        // SAFETY: serialized behind crate::ENV_LOCK.
        unsafe {
            std::env::remove_var("SHIFT_PDF_ENGINE");
        }

        // Empty / whitespace-only override falls through (may error if no engine).
        let empty = resolve_pdf_engine(Some(""));
        let spaces = resolve_pdf_engine(Some("   "));
        // Both should behave the same (no override).
        assert_eq!(empty.is_ok(), spaces.is_ok());
        if let (Ok(a), Ok(b)) = (&empty, &spaces) {
            assert_eq!(a, b);
        }

        // Bare name override is returned as-is without PATH resolution.
        assert_eq!(
            resolve_pdf_engine(Some("custom-engine")).unwrap(),
            OsString::from("custom-engine")
        );
        // Absolute path override.
        assert_eq!(
            resolve_pdf_engine(Some("/opt/local/bin/typst")).unwrap(),
            OsString::from("/opt/local/bin/typst")
        );
        // Trimmed override.
        assert_eq!(
            resolve_pdf_engine(Some("  xelatex  ")).unwrap(),
            OsString::from("xelatex")
        );
        // Per-conversion override wins over env even when env is set.
        unsafe {
            std::env::set_var("SHIFT_PDF_ENGINE", "/from/env");
        }
        assert_eq!(
            resolve_pdf_engine(Some("from-option")).unwrap(),
            OsString::from("from-option")
        );
        // Env used when override is None.
        assert_eq!(
            resolve_pdf_engine(None).unwrap(),
            OsString::from("/from/env")
        );
        // Empty env falls through to discovery.
        unsafe {
            std::env::set_var("SHIFT_PDF_ENGINE", "");
        }
        let discovered = resolve_pdf_engine(None);
        // May succeed if a candidate is installed; either way must not return empty env.
        if let Ok(engine) = discovered {
            assert!(!engine.is_empty());
        }

        unsafe {
            match previous {
                Some(value) => std::env::set_var("SHIFT_PDF_ENGINE", value),
                None => std::env::remove_var("SHIFT_PDF_ENGINE"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn provenance_fields_on_successful_artifact() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("prov");
        let executable = directory.join(format!("shift-pandoc-prov-{suffix}"));
        let input = directory.join(format!("shift-pandoc-prov-in-{suffix}.org"));
        write_fake_pandoc(&executable, "#!/bin/sh\nprintf '# converted\\n'");
        std::fs::write(&input, "* src").unwrap();

        let artifact = PandocModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();

        assert_eq!(artifact.module_id, "pandoc");
        assert_eq!(artifact.pipeline, vec!["pandoc"]);
        assert_eq!(artifact.invocations.len(), 1);
        assert_eq!(artifact.invocations[0].module_id, "pandoc");
        assert!(
            !artifact.invocations[0].argv_display.is_empty(),
            "argv_display should be recorded"
        );
        assert!(
            artifact.invocations[0].argv_display.contains("pandoc")
                || artifact.invocations[0]
                    .argv_display
                    .contains(executable.file_name().unwrap().to_string_lossy().as_ref()),
            "argv_display should mention executable: {}",
            artifact.invocations[0].argv_display
        );
        assert_eq!(artifact.format, OutputFormat::MARKDOWN);
        assert_eq!(artifact.media_type, OutputFormat::MARKDOWN.media_type());
        assert_eq!(
            artifact.file_name,
            format!("{}.md", input.file_stem().unwrap().to_string_lossy())
        );
        assert_eq!(artifact.bytes, b"# converted\n");

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn pandoc_nonzero_exit_surfaces_stderr() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("fail");
        let executable = directory.join(format!("shift-pandoc-fail-{suffix}"));
        let input = directory.join(format!("shift-pandoc-fail-in-{suffix}.md"));
        write_fake_pandoc(
            &executable,
            "#!/bin/sh\necho 'parse error: boom' >&2\nexit 2\n",
        );
        std::fs::write(&input, "bad").unwrap();

        let err = PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Pandoc could not convert"), "{message}");
        assert!(message.contains("boom"), "{message}");

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn input_format_aliases_passed_on_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("alias");
        let executable = directory.join(format!("shift-pandoc-alias-{suffix}"));
        write_fake_pandoc(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf 'ok'",
        );

        let cases = [
            ("adoc", "asciidoc"),
            ("bib", "bibtex"),
            ("enw", "endnotexml"),
            ("htm", "html"),
            ("tex", "latex"),
            ("typ", "typst"),
            ("wiki", "mediawiki"),
        ];
        for (ext, expected_from) in cases {
            let input = directory.join(format!("shift-pandoc-alias-in-{suffix}.{ext}"));
            std::fs::write(&input, "src").unwrap();
            PandocModule::with_executable(&executable)
                .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
                .unwrap();
            let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
            assert!(
                args.contains(expected_from),
                "ext .{ext} should map to --from {expected_from}, args: {args}"
            );
            let _ = std::fs::remove_file(&input);
        }

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
    }

    #[test]
    fn pandoc_input_format_passthrough_for_unknown_extensions() {
        assert_eq!(pandoc_input_format("docx", false), "docx");
        assert_eq!(pandoc_input_format("rst", false), "rst");
        assert_eq!(pandoc_input_format("epub", true), "epub");
        assert_eq!(pandoc_input_format("ODT", false), "ODT");
        // Aliases always lowercase the match arm keys via to_ascii_lowercase.
        assert_eq!(pandoc_input_format("ADOC", false), "asciidoc");
        assert_eq!(pandoc_input_format("HTM", false), "html");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_stem_falls_back_to_converted_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let directory = std::env::temp_dir();
        let suffix = unique_suffix("stem");
        let executable = directory.join(format!("shift-pandoc-stem-{suffix}"));
        // macOS rejects non-UTF-8 path components on disk; the fake binary never
        // opens the input, so a synthetic OsStr path is enough to exercise stem
        // fallback via `to_str()` → None.
        let input = PathBuf::from(OsStr::from_bytes(b"\xff\xfe.md"));
        write_fake_pandoc(&executable, "#!/bin/sh\nprintf 'body'");

        let artifact = PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap();
        assert_eq!(artifact.file_name, "converted.html");

        let _ = std::fs::remove_file(&executable);
    }

    #[cfg(unix)]
    #[test]
    fn cancel_flag_aborts_before_or_during_pandoc() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("cancel");
        let executable = directory.join(format!("shift-pandoc-cancel-{suffix}"));
        let input = directory.join(format!("shift-pandoc-cancel-in-{suffix}.md"));
        write_fake_pandoc(&executable, "#!/bin/sh\nsleep 30\n");
        std::fs::write(&input, "# x").unwrap();

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let options = ConversionOptions {
            cancel: Some(std::sync::Arc::clone(&cancel)),
            ..ConversionOptions::default()
        };
        let err = PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &options)
            .unwrap_err();
        assert!(err.is_cancelled(), "error: {err}");

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn pdf_engine_option_passed_on_argv() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("pdf-opt");
        let executable = directory.join(format!("shift-pandoc-pdf-opt-{suffix}"));
        let input = directory.join(format!("shift-pandoc-pdf-opt-in-{suffix}.md"));
        write_fake_pandoc(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '%%PDF-1.4'",
        );
        std::fs::write(&input, "# x").unwrap();

        let previous = std::env::var_os("SHIFT_PDF_ENGINE");
        unsafe {
            std::env::remove_var("SHIFT_PDF_ENGINE");
        }

        let options = ConversionOptions {
            pandoc: PandocOptions {
                pdf_engine: Some("tectonic".into()),
                ..PandocOptions::default()
            },
            ..ConversionOptions::default()
        };
        PandocModule::with_executable(&executable)
            .convert(&input, OutputFormat::PDF, &options)
            .unwrap();
        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--pdf-engine"), "args: {args}");
        assert!(args.contains("tectonic"), "args: {args}");

        unsafe {
            match previous {
                Some(value) => std::env::set_var("SHIFT_PDF_ENGINE", value),
                None => std::env::remove_var("SHIFT_PDF_ENGINE"),
            }
        }
        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
        let _ = std::fs::remove_file(&input);
    }
}
