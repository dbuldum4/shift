use super::{ConversionArtifact, ConversionError, ConversionModule, OutputFormat};
use std::ffi::OsString;
use std::path::Path;
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
        OutputFormat::ALL
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
    ) -> Result<ConversionArtifact, ConversionError> {
        let target = output_format.id();
        let input_format = input
            .extension()
            .and_then(|extension| extension.to_str())
            .map(pandoc_input_format)
            .unwrap_or("markdown");
        let output = Command::new(&self.executable)
            .arg(input)
            .arg("--from")
            .arg(input_format)
            .arg("--to")
            .arg(target)
            .arg("--output")
            .arg("-")
            .output()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    ConversionError::new(
                        "Pandoc is not installed. Install it with `brew install pandoc`, or set SHIFT_PANDOC_BIN.",
                    )
                } else {
                    ConversionError::new(format!("could not start Pandoc: {error}"))
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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
        let registered = OutputFormat::ALL
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
            .convert(&input, OutputFormat::DOCX)
            .unwrap();
        assert_eq!(artifact.bytes, b"fake-docx");
        assert_eq!(artifact.format, OutputFormat::DOCX);
        assert_eq!(artifact.module_id, "pandoc");

        std::fs::remove_file(executable).unwrap();
        std::fs::remove_file(input).unwrap();
    }
}
