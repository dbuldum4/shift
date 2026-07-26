use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, InvocationRecord,
    OutputFormat, bundled_runtime_tool, command_argv_parts, format_argv_display, map_spawn_error,
    max_output_bytes, process_timeout, resolve_tool_executable, run_command_cancellable,
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

/// Optional knobs for MarkItDown. Empty/default matches a plain `markitdown <input>`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MarkItDownOptions {
    /// Keep base64 data URIs in the Markdown (`--keep-data-uris`).
    ///
    /// Off by default because embedded images can inflate artifacts past the
    /// process output cap.
    pub keep_data_uris: bool,
}

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
    let mut candidates = Vec::new();
    if let Some(bundled) = bundled_runtime_tool("markitdown") {
        candidates.push(bundled);
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".venv/bin/markitdown"));
    resolve_tool_executable("SHIFT_MARKITDOWN_BIN", "markitdown", &candidates)
}

impl MarkItDownModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn run(
        &self,
        command: Command,
        options: &ConversionOptions,
    ) -> Result<super::LimitedOutput, ConversionError> {
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

        let mut command = Command::new(&self.executable);
        command.arg(input);
        if options.markitdown.keep_data_uris {
            command.arg("--keep-data-uris");
        }

        let display_parts = command_argv_parts(&command);
        let argv_display = format_argv_display(&display_parts);

        let output = self.run(command, options)?;
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
            pipeline: vec![self.id()],
            invocations: vec![InvocationRecord {
                module_id: self.id(),
                argv_display,
            }],
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

    #[test]
    fn passes_keep_data_uris_when_enabled() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-markitdown-opts-{suffix}"));
        let input = directory.join(format!("shift-markitdown-input-{suffix}.txt"));
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '# ok\\n'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "source").unwrap();

        let options = ConversionOptions {
            markitdown: MarkItDownOptions {
                keep_data_uris: true,
            },
            ..ConversionOptions::default()
        };
        MarkItDownModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--keep-data-uris"), "args: {args}");

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn fails_when_markitdown_exits_with_error() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-markitdown-fail-{suffix}"));
        let input = directory.join(format!("shift-markitdown-fail-input-{suffix}.txt"));
        std::fs::write(
            &executable,
            "#!/bin/sh\necho 'intentional failure' >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "source").unwrap();

        let result = MarkItDownModule::with_executable(&executable).convert(
            &input,
            OutputFormat::MARKDOWN,
            &ConversionOptions::default(),
        );

        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(
            message.contains("MarkItDown could not convert"),
            "{message}"
        );

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn rejects_non_markdown_output() {
        let err = MarkItDownModule::with_executable("/bin/cat")
            .convert(
                Path::new("notes.pdf"),
                OutputFormat::PDF,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("MarkItDown only produces Markdown"),
            "{err}"
        );
    }

    #[test]
    fn reports_capability_lists() {
        let module = MarkItDownModule::with_executable("/bin/cat");
        let inputs = module.input_extensions();
        assert!(!inputs.is_empty());
        for required in ["pdf", "docx", "html"] {
            assert!(
                inputs.contains(&required),
                "input_extensions missing {required}: {inputs:?}"
            );
        }
        assert_eq!(module.output_formats(), &[OutputFormat::MARKDOWN]);
        assert_eq!(module.chainable_output_formats(), module.output_formats());
        assert_eq!(module.chainable_output_formats(), &[OutputFormat::MARKDOWN]);
    }

    #[test]
    fn empty_stdout_still_yields_named_markdown_artifact() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let executable = directory.join(format!("shift-markitdown-empty-{suffix}"));
        let input = directory.join(format!("shift-markitdown-empty-input-{suffix}.txt"));
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "source").unwrap();

        let artifact = MarkItDownModule::with_executable(&executable)
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
        assert_eq!(artifact.bytes, b"");
        assert_eq!(artifact.media_type, "text/markdown");
        assert_eq!(artifact.format, OutputFormat::MARKDOWN);

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn successful_convert_records_markitdown_provenance() {
        let input = std::env::temp_dir().join(format!(
            "shift-markitdown-prov-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&input, "# Converted\n").unwrap();

        let artifact = MarkItDownModule::with_executable("/bin/cat")
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();

        assert_eq!(artifact.module_id, "markitdown");
        assert_eq!(artifact.pipeline, vec!["markitdown"]);
        assert_eq!(artifact.format, OutputFormat::MARKDOWN);
        assert_eq!(artifact.invocations.len(), 1);
        assert_eq!(artifact.invocations[0].module_id, "markitdown");
        assert!(
            !artifact.invocations[0].argv_display.is_empty(),
            "argv_display should be recorded"
        );

        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn missing_executable_fails_cleanly() {
        let missing = std::env::temp_dir().join(format!(
            "shift-markitdown-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let input = std::env::temp_dir().join(format!(
            "shift-markitdown-missing-input-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&input, "source").unwrap();

        let err = MarkItDownModule::with_executable(&missing)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("MarkItDown is not installed")
                || message.contains("executable not found"),
            "{message}"
        );

        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn keep_data_uris_false_does_not_pass_flag() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let executable = directory.join(format!("shift-markitdown-no-keep-{suffix}"));
        let input = directory.join(format!("shift-markitdown-no-keep-input-{suffix}.txt"));
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '# ok\\n'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "source").unwrap();

        let options = ConversionOptions {
            markitdown: MarkItDownOptions {
                keep_data_uris: false,
            },
            ..ConversionOptions::default()
        };
        MarkItDownModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(
            !args.contains("--keep-data-uris"),
            "keep_data_uris=false must not pass the flag, args: {args}"
        );

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
        let _ = std::fs::remove_file(&input);
    }
}
