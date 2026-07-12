use super::{ConversionArtifact, ConversionError, ConversionModule, OutputFormat};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output};

const EXTENSIONS: &[&str] = &[
    // Documents
    "pdf", "pptx", "docx", "xlsx", "xls",
    // Images (metadata, and OCR/image descriptions when configured upstream)
    "bmp", "gif", "heic", "jpeg", "jpg", "png", "tif", "tiff", "webp", // Audio
    "aac", "flac", "m4a", "mp3", "ogg", "wav", // Web and structured/plain text
    "csv", "htm", "html", "json", "md", "txt", "xml", // Archives
    "zip",
];
const OUTPUTS: &[OutputFormat] = &[OutputFormat::MARKDOWN];

#[derive(Clone, Debug)]
pub struct MarkItDownModule {
    executable: OsString,
}

impl Default for MarkItDownModule {
    fn default() -> Self {
        Self {
            executable: discover_executable(),
        }
    }
}

fn discover_executable() -> OsString {
    if let Some(executable) = std::env::var_os("SHIFT_MARKITDOWN_BIN") {
        return executable;
    }

    // Prefer Shift's isolated development runtime when it exists. Packaged
    // builds can provide a bundled path through SHIFT_MARKITDOWN_BIN.
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join(".venv/bin/markitdown");
    if local.is_file() {
        return local.into_os_string();
    }

    OsString::from("markitdown")
}

impl MarkItDownModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn run(&self, input: &Path) -> Result<Output, ConversionError> {
        Command::new(&self.executable)
            .arg(input)
            .output()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    ConversionError::new(
                        "MarkItDown is not installed. Install the complete runtime with: \
                         python3 -m pip install 'markitdown[all]'",
                    )
                } else {
                    ConversionError::new(format!("could not start MarkItDown: {error}"))
                }
            })
    }
}

impl ConversionModule for MarkItDownModule {
    fn id(&self) -> &'static str {
        "markitdown"
    }

    fn label(&self) -> &'static str {
        "MarkItDown"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        EXTENSIONS
    }

    fn output_formats(&self) -> &'static [OutputFormat] {
        OUTPUTS
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
    ) -> Result<ConversionArtifact, ConversionError> {
        if output_format != OutputFormat::MARKDOWN {
            return Err(ConversionError::new("MarkItDown only produces Markdown"));
        }
        let output = self.run(input)?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if detail.is_empty() {
                format!("process exited with {}", output.status)
            } else {
                detail
            };
            return Err(ConversionError::new(format!(
                "MarkItDown could not convert {}: {detail}",
                input.display()
            )));
        }

        let stem = input
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("converted");

        Ok(ConversionArtifact {
            file_name: format!("{stem}.md"),
            media_type: "text/markdown",
            bytes: output.stdout,
            format: OutputFormat::MARKDOWN,
            module_id: self.id(),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn emits_stdout_as_a_named_markdown_artifact() {
        let input = std::env::temp_dir().join(format!(
            "shift-markitdown-test-{}-sample.txt",
            std::process::id()
        ));
        std::fs::write(&input, "# Converted\n").unwrap();

        let artifact = MarkItDownModule::with_executable("/bin/cat")
            .convert(&input, OutputFormat::MARKDOWN)
            .unwrap();

        assert_eq!(
            artifact.file_name,
            format!("{}.md", input.file_stem().unwrap().to_string_lossy())
        );
        assert_eq!(artifact.media_type, "text/markdown");
        assert_eq!(artifact.text(), Some("# Converted\n"));

        std::fs::remove_file(input).unwrap();
    }
}
