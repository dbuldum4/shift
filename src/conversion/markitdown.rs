use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, OutputFormat,
    map_spawn_error, max_output_bytes, process_timeout, resolve_tool_executable,
    run_command_cancellable,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    // Prefer Shift's isolated development runtime when it exists. Packaged
    // builds can provide a bundled path through SHIFT_MARKITDOWN_BIN.
    // Resolves to an absolute path via PATH + common_bin_dirs so GUI apps
    // with a minimal PATH still spawn the same binary diagnostics reports.
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".venv/bin/markitdown");
    resolve_tool_executable("SHIFT_MARKITDOWN_BIN", "markitdown", &[local])
}

impl MarkItDownModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn run(
        &self,
        input: &Path,
        options: &ConversionOptions,
    ) -> Result<super::LimitedOutput, ConversionError> {
        let mut command = Command::new(&self.executable);
        command.arg(input);
        run_command_cancellable(
            command,
            process_timeout(),
            max_output_bytes(),
            options.cancel.clone(),
        )
        .map_err(|error| {
            map_spawn_error(
                error,
                "MarkItDown is not installed. Install the complete runtime with: \
                 python3 -m pip install 'markitdown[all]'",
            )
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

    fn chainable_output_formats(&self) -> &'static [OutputFormat] {
        OUTPUTS
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if output_format != OutputFormat::MARKDOWN {
            return Err(ConversionError::new("MarkItDown only produces Markdown"));
        }
        let output = self.run(input, options)?;
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
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
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
