use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, OutputFormat,
    map_spawn_error, max_output_bytes, process_timeout, run_command,
};
use std::ffi::{OsStr, OsString};
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

#[derive(Clone, Debug)]
pub struct PandocModule {
    executable: OsString,
}

impl Default for PandocModule {
    fn default() -> Self {
        Self {
            executable: std::env::var_os("SHIFT_PANDOC_BIN")
                .unwrap_or_else(|| OsString::from("pandoc")),
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
        _options: &ConversionOptions,
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

        // Pandoc's PDF writer always shells out to an external engine. Default
        // is pdflatex, which is rarely present on a fresh machine. Resolve a
        // lighter engine (Typst first) so DOCX → PDF works after a normal
        // `brew install pandoc typst` setup.
        if output_format == OutputFormat::PDF {
            let engine = resolve_pdf_engine()?;
            command.arg("--pdf-engine").arg(&engine);
        }

        let output = run_command(command, process_timeout(), max_output_bytes()).map_err(
            |error| {
                map_spawn_error(
                    error,
                    "Pandoc is not installed. Install it with `brew install pandoc`, or set SHIFT_PANDOC_BIN.",
                )
            },
        )?;

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

/// Choose a PDF engine for Pandoc.
///
/// Order of preference:
/// 1. `SHIFT_PDF_ENGINE` when set (name or absolute path)
/// 2. First candidate found on `PATH` / common install locations
fn resolve_pdf_engine() -> Result<OsString, ConversionError> {
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

fn find_executable(name: &str) -> Option<PathBuf> {
    let name = OsStr::new(name);

    // Absolute / relative overrides via SHIFT_PDF_ENGINE are handled earlier.
    // Here we only resolve bare tool names.
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
    }

    // GUI-launched macOS apps often inherit a minimal PATH that omits Homebrew
    // and MacTeX. Probe the usual install locations so PDF engines still resolve.
    for dir in common_bin_dirs() {
        let candidate = dir.join(name);
        if is_runnable(&candidate) {
            return Some(candidate);
        }
    }

    None
}

fn common_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/Library/TeX/texbin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs
}

fn is_runnable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
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

    #[test]
    fn pdf_engine_env_override_wins() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: serialized behind ENV_LOCK for the duration of this test.
        unsafe {
            std::env::set_var("SHIFT_PDF_ENGINE", "/custom/bin/typst");
        }
        let engine = resolve_pdf_engine().unwrap();
        assert_eq!(engine, OsString::from("/custom/bin/typst"));
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
