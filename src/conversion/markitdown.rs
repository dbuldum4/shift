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

    fn write_fake_markitdown(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn capability_list_covers_documents_images_audio_web_and_archives() {
        let module = MarkItDownModule::with_executable("/bin/cat");
        assert_eq!(module.id(), "markitdown");
        assert_eq!(module.label(), "MarkItDown");
        let inputs = module.input_extensions();

        for required in [
            // Documents
            "pdf", "pptx", "docx", "xlsx", "xls", // Images
            "bmp", "gif", "heic", "jpeg", "jpg", "png", "tif", "tiff", "webp", // Audio
            "aac", "flac", "m4a", "mp3", "ogg", "wav", // Web / structured
            "csv", "htm", "html", "json", "md", "txt", "xml", // Archives
            "zip",
        ] {
            assert!(
                inputs.contains(&required),
                "input_extensions missing {required}: {inputs:?}"
            );
        }
        // No duplicates.
        let mut seen = std::collections::HashSet::new();
        for ext in inputs {
            assert!(seen.insert(*ext), "duplicate extension: {ext}");
        }
        // Only markdown output / chainable.
        assert_eq!(module.output_formats(), &[OutputFormat::MARKDOWN]);
        assert_eq!(module.chainable_output_formats(), &[OutputFormat::MARKDOWN]);
        assert!(module.supports(Path::new("scan.PDF"), OutputFormat::MARKDOWN));
        assert!(module.supports(Path::new("photo.HEIC"), OutputFormat::MARKDOWN));
        assert!(module.supports(Path::new("archive.ZIP"), OutputFormat::MARKDOWN));
        assert!(!module.supports(Path::new("scan.pdf"), OutputFormat::HTML));
        assert!(!module.supports(Path::new("clip.mp4"), OutputFormat::MARKDOWN));
    }

    #[test]
    fn cancel_flag_aborts_conversion() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("cancel");
        let executable = directory.join(format!("shift-markitdown-cancel-{suffix}"));
        let input = directory.join(format!("shift-markitdown-cancel-in-{suffix}.txt"));
        write_fake_markitdown(&executable, "#!/bin/sh\nsleep 30\n");
        std::fs::write(&input, "source").unwrap();

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let options = ConversionOptions {
            cancel: Some(std::sync::Arc::clone(&cancel)),
            ..ConversionOptions::default()
        };
        let err = MarkItDownModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap_err();
        assert!(err.is_cancelled(), "error: {err}");

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn cancel_mid_run_stops_hanging_markitdown() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("mid-cancel");
        let executable = directory.join(format!("shift-markitdown-midcancel-{suffix}"));
        let input = directory.join(format!("shift-markitdown-midcancel-in-{suffix}.txt"));
        write_fake_markitdown(&executable, "#!/bin/sh\nsleep 30\n");
        std::fs::write(&input, "source").unwrap();

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
        let err = MarkItDownModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap_err();
        let _ = watcher.join();
        assert!(err.is_cancelled(), "error: {err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "cancel took too long: {:?}",
            started.elapsed()
        );

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn large_stdout_near_limit_succeeds_and_over_limit_fails() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("large");
        let executable = directory.join(format!("shift-markitdown-large-{suffix}"));
        let input = directory.join(format!("shift-markitdown-large-in-{suffix}.txt"));
        // Emit exactly 100 bytes — tests use run_command_cancellable with process max;
        // we verify module surfaces oversized stdout from the shared helper.
        write_fake_markitdown(
            &executable,
            "#!/bin/sh\n# 200 zero bytes on stdout\ndd if=/dev/zero bs=200 count=1 2>/dev/null\n",
        );
        std::fs::write(&input, "source").unwrap();

        // With default max (64 MiB) 200 bytes succeeds.
        let artifact = MarkItDownModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert_eq!(artifact.bytes.len(), 200);
        assert_eq!(artifact.format, OutputFormat::MARKDOWN);

        // Drive a tight limit via env override used by max_output_bytes().
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES").ok();
        unsafe {
            std::env::set_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES", "64");
        }
        let err = MarkItDownModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("exceeded")
                || message.contains("limit")
                || message.contains("too large"),
            "{message}"
        );
        unsafe {
            match previous {
                Some(value) => std::env::set_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES", value),
                None => std::env::remove_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES"),
            }
        }

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn non_utf8_filename_stem_falls_back_to_converted() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // macOS cannot create non-UTF-8 path components on disk. Use a synthetic
        // path (fake binary ignores contents) so `file_stem().to_str()` is None
        // and the module falls back to "converted.md".
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("nonutf8");
        let executable = directory.join(format!("shift-markitdown-nonutf8-{suffix}"));
        write_fake_markitdown(&executable, "#!/bin/sh\nprintf '# body\\n'");
        let input = PathBuf::from(OsStr::from_bytes(b"\xff\xfe.txt"));

        let artifact = MarkItDownModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert_eq!(artifact.file_name, "converted.md");
        assert_eq!(artifact.bytes, b"# body\n");

        let _ = std::fs::remove_file(&executable);
    }

    #[test]
    fn rejects_html_docx_and_pdf_outputs() {
        let module = MarkItDownModule::with_executable("/bin/cat");
        for format in [
            OutputFormat::HTML,
            OutputFormat::DOCX,
            OutputFormat::PDF,
            OutputFormat::EPUB,
            OutputFormat::MP3,
        ] {
            let err = module
                .convert(
                    Path::new("notes.txt"),
                    format,
                    &ConversionOptions::default(),
                )
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("MarkItDown only produces Markdown"),
                "format {format:?}: {err}"
            );
        }
    }

    #[test]
    fn failure_without_stderr_includes_exit_status() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("silent-fail");
        let executable = directory.join(format!("shift-markitdown-silentfail-{suffix}"));
        let input = directory.join(format!("shift-markitdown-silentfail-in-{suffix}.txt"));
        write_fake_markitdown(&executable, "#!/bin/sh\nexit 3\n");
        std::fs::write(&input, "source").unwrap();

        let err = MarkItDownModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("MarkItDown could not convert"),
            "{message}"
        );
        assert!(
            message.contains("process exited") || message.contains("exit"),
            "{message}"
        );

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn argv_display_includes_input_path_and_optional_flag() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("argv");
        let executable = directory.join(format!("shift-markitdown-argv-{suffix}"));
        let input = directory.join(format!("shift-markitdown-argv-in-{suffix}.docx"));
        write_fake_markitdown(&executable, "#!/bin/sh\nprintf '# ok\\n'");
        std::fs::write(&input, "source").unwrap();

        let options = ConversionOptions {
            markitdown: MarkItDownOptions {
                keep_data_uris: true,
            },
            ..ConversionOptions::default()
        };
        let artifact = MarkItDownModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();
        let argv = &artifact.invocations[0].argv_display;
        assert!(
            argv.contains(input.file_name().unwrap().to_string_lossy().as_ref())
                || argv.contains(&input.display().to_string()),
            "argv should mention input: {argv}"
        );
        assert!(
            argv.contains("keep-data-uris") || argv.contains("--keep-data-uris"),
            "argv should mention keep-data-uris: {argv}"
        );

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn module_defaults_construct_without_panic() {
        // Discover path may or may not exist; construction must be fallible only at convert.
        let _module = MarkItDownModule::default();
        let custom = MarkItDownModule::with_executable("/bin/cat");
        assert_eq!(custom.id(), "markitdown");
    }

    #[test]
    fn unicode_stem_is_preserved_in_artifact_name() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("unicode");
        let input = directory.join(format!("rapor-ç-文档-{suffix}.pdf"));
        std::fs::write(&input, "# unicode\n").unwrap();

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
        assert!(artifact.file_name.contains("rapor"));
        assert!(artifact.file_name.ends_with(".md"));

        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn supports_case_insensitive_extensions() {
        let module = MarkItDownModule::with_executable("/bin/cat");
        for name in [
            "Doc.DOCX",
            "Photo.JPEG",
            "Track.MP3",
            "Page.HTML",
            "Data.JSON",
        ] {
            assert!(
                module.supports(Path::new(name), OutputFormat::MARKDOWN),
                "should support {name}"
            );
        }
    }
}
