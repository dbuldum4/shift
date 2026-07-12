use super::{ConversionArtifact, ConversionError, ConversionModule, OutputFormat};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output};
use url::Url;

const EXTENSIONS: &[&str] = &["htm", "html"];
const OUTPUTS: &[OutputFormat] = &[OutputFormat::MARKDOWN, OutputFormat::HTML];

#[derive(Clone, Debug)]
pub struct DefuddleModule {
    executable: OsString,
}

impl Default for DefuddleModule {
    fn default() -> Self {
        Self {
            executable: discover_executable(),
        }
    }
}

fn discover_executable() -> OsString {
    if let Some(executable) = std::env::var_os("SHIFT_DEFUDDLE_BIN") {
        return executable;
    }

    // Prefer a project-local node_modules binary when present.
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("node_modules/.bin/defuddle");
    if local.is_file() {
        return local.into_os_string();
    }

    OsString::from("defuddle")
}

impl DefuddleModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn run(&self, source: &str, markdown: bool) -> Result<Output, ConversionError> {
        let mut command = Command::new(&self.executable);
        command.arg("parse").arg(source);
        if markdown {
            command.arg("--markdown");
        }
        command.output().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ConversionError::new(
                    "Defuddle is not installed. Install it with `npm install -g defuddle`, \
                     or set SHIFT_DEFUDDLE_BIN.",
                )
            } else {
                ConversionError::new(format!("could not start Defuddle: {error}"))
            }
        })
    }

    fn artifact_from_output(
        &self,
        source_label: &str,
        file_stem: &str,
        output_format: OutputFormat,
        output: Output,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if detail.is_empty() {
                format!("process exited with {}", output.status)
            } else {
                detail
            };
            return Err(ConversionError::new(format!(
                "Defuddle could not convert {source_label}: {detail}"
            )));
        }

        Ok(ConversionArtifact {
            file_name: format!("{file_stem}.{}", output_format.extension()),
            media_type: output_format.media_type(),
            bytes: output.stdout,
            format: output_format,
            module_id: self.id(),
        })
    }

    /// Convert a web page URL to a cleaned document via the Defuddle CLI.
    pub fn convert_url(
        &self,
        url: &str,
        output_format: OutputFormat,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !self.supports_url(output_format) {
            return Err(ConversionError::new(format!(
                "Defuddle does not produce {}",
                output_format.label()
            )));
        }
        let url = url.trim();
        if !looks_like_url(url) {
            return Err(ConversionError::new(format!(
                "not a valid http(s) URL: {url}"
            )));
        }

        let markdown = output_format == OutputFormat::MARKDOWN;
        let output = self.run(url, markdown)?;
        self.artifact_from_output(url, &url_file_stem(url), output_format, output)
    }
}

impl ConversionModule for DefuddleModule {
    fn id(&self) -> &'static str {
        "defuddle"
    }

    fn label(&self) -> &'static str {
        "Defuddle"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        EXTENSIONS
    }

    fn output_formats(&self) -> &'static [OutputFormat] {
        OUTPUTS
    }

    fn supports_url(&self, output: OutputFormat) -> bool {
        OUTPUTS.contains(&output)
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !OUTPUTS.contains(&output_format) {
            return Err(ConversionError::new(format!(
                "Defuddle only produces Markdown or HTML, not {}",
                output_format.label()
            )));
        }

        let markdown = output_format == OutputFormat::MARKDOWN;
        let source = input
            .to_str()
            .ok_or_else(|| ConversionError::new("input path is not valid UTF-8"))?;
        let output = self.run(source, markdown)?;
        let stem = input
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("converted");
        self.artifact_from_output(&input.display().to_string(), stem, output_format, output)
    }

    fn convert_url(
        &self,
        url: &str,
        output_format: OutputFormat,
    ) -> Result<ConversionArtifact, ConversionError> {
        DefuddleModule::convert_url(self, url, output_format)
    }
}

/// True for absolute http(s) URLs used as conversion sources.
pub fn looks_like_url(value: &str) -> bool {
    Url::parse(value.trim())
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

fn url_file_stem(url: &str) -> String {
    let parsed = Url::parse(url.trim()).ok();
    let last_path_segment = parsed.as_ref().and_then(|url| {
        url.path_segments()
            .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
    });
    let last = last_path_segment
        .and_then(|segment| Path::new(segment).file_stem())
        .and_then(|value| value.to_str())
        .or_else(|| parsed.as_ref().and_then(Url::host_str))
        .unwrap_or("page");

    let stem: String = last
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect();

    let stem = stem.trim_matches('-');
    if stem.is_empty() {
        "page".into()
    } else {
        stem.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_http_urls() {
        assert!(looks_like_url("https://example.com/article"));
        assert!(looks_like_url("  http://example.com  "));
        assert!(looks_like_url("http://localhost:3000/article"));
        assert!(looks_like_url("http://[::1]/article"));
        assert!(!looks_like_url("ftp://example.com"));
        assert!(!looks_like_url("example.com"));
        assert!(!looks_like_url("not a url"));
        assert!(!looks_like_url("report.docx"));
        assert!(!looks_like_url("https://?path.example"));
    }

    #[test]
    fn url_stem_uses_last_path_segment() {
        assert_eq!(
            url_file_stem("https://example.com/blog/my-post?ref=1"),
            "my-post"
        );
        assert_eq!(url_file_stem("https://example.com/"), "example.com");
        assert_eq!(url_file_stem("https://example.com/page.html"), "page");
    }

    #[cfg(unix)]
    #[test]
    fn converts_a_url_source_with_markdown_flag() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-defuddle-test-{suffix}"));
        // Echo argv so we can assert the CLI shape, then print fake markdown.
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '# Hello\\n'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let artifact = DefuddleModule::with_executable(&executable)
            .convert_url("https://example.com/hello-world", OutputFormat::MARKDOWN)
            .unwrap();

        assert_eq!(artifact.file_name, "hello-world.md");
        assert_eq!(artifact.media_type, "text/markdown");
        assert_eq!(artifact.text(), Some("# Hello\n"));
        assert_eq!(artifact.module_id, "defuddle");

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("parse"));
        assert!(args.contains("https://example.com/hello-world"));
        assert!(args.contains("--markdown"));

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
    }

    #[cfg(unix)]
    #[test]
    fn converts_an_html_file_without_markdown_flag_for_html_output() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-defuddle-html-test-{suffix}"));
        let input = directory.join(format!("shift-defuddle-input-{suffix}.html"));
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '<p>clean</p>'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "<html><body><p>raw</p></body></html>").unwrap();

        let artifact = DefuddleModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML)
            .unwrap();

        assert_eq!(
            artifact.file_name,
            format!("{}.html", input.file_stem().unwrap().to_string_lossy())
        );
        assert_eq!(artifact.bytes, b"<p>clean</p>");

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("parse"));
        assert!(!args.contains("--markdown"));

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
        let _ = std::fs::remove_file(&input);
    }
}
