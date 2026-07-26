//! Runtime readiness diagnostics for external conversion engines.
//!
//! Registered capability (what a module *claims* to convert) is independent of
//! whether the engine binary is installed on this machine. This module probes
//! PATH, project-local installs, and env overrides, reports versions, and
//! supplies install hints for both the Settings UI and `shift-cli doctor`.

use super::pandoc::{pdf_engine_candidates, resolve_pdf_engine};
use super::process::{
    bundled_runtime_tool, clear_tool_discovery_cache, find_executable, is_runnable,
    resolve_tool_path, run_command,
};
use super::{ConversionRegistry, OutputFormat};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

/// Wall-clock budget for a single version probe.
///
/// Python CLIs (Docling, MarkItDown) can take several seconds to import.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const VERSION_PROBE_MAX_BYTES: usize = 64 * 1024;

/// Whether an external engine (or PDF backend) is usable right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    /// Binary resolved and a version probe succeeded (or path is runnable).
    Ready,
    /// Not found on PATH, env override, or project-local locations.
    Missing,
}

impl Readiness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
        }
    }

    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

impl fmt::Display for Readiness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// How a conversion pair relates to engine readiness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatAvailability {
    /// A registered route exists and every engine it needs is ready.
    Available,
    /// Modules advertise the pair, but a required engine is missing.
    SupportedUnavailable,
    /// No registered module (or chain) handles this pair.
    Unsupported,
}

impl FormatAvailability {
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::SupportedUnavailable => "supported (engine missing)",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Status of one conversion engine (MarkItDown, Pandoc, …).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineDiagnostic {
    pub id: &'static str,
    pub label: &'static str,
    pub readiness: Readiness,
    pub version: Option<String>,
    pub resolved_path: Option<PathBuf>,
    pub env_override: &'static str,
    pub install_hint: String,
    pub notes: Option<String>,
}

/// Status of one Pandoc PDF backend candidate (or a custom `SHIFT_PDF_ENGINE`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PdfEngineDiagnostic {
    pub name: String,
    pub readiness: Readiness,
    pub version: Option<String>,
    pub resolved_path: Option<PathBuf>,
    pub selected: bool,
}

/// Full machine report used by Settings and `shift-cli doctor`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsReport {
    pub engines: Vec<EngineDiagnostic>,
    pub pdf_engines: Vec<PdfEngineDiagnostic>,
    pub selected_pdf_engine: Option<String>,
}

impl DiagnosticsReport {
    /// Probe every registered engine and PDF backend on this machine.
    ///
    /// Engine and PDF probes run in parallel so Settings refresh is not blocked
    /// on sequential multi-second Python cold starts.
    ///
    /// Callers that cache a `ConversionRegistry` must rebuild it after calling
    /// `collect()`, because module instances capture resolved executable paths
    /// and a new probe may discover a tool installed since the registry was built.
    pub fn collect() -> Self {
        // A manual Refresh (or the first probe at startup) must see the current
        // PATH / install state, not a memoized answer from earlier in the process.
        clear_tool_discovery_cache();

        let engines = thread::scope(|scope| {
            let markitdown = scope.spawn(probe_markitdown);
            let pandoc = scope.spawn(probe_pandoc);
            let defuddle = scope.spawn(probe_defuddle);
            let docling = scope.spawn(probe_docling);
            let ffmpeg = scope.spawn(probe_ffmpeg);
            vec![
                markitdown.join().expect("markitdown probe"),
                pandoc.join().expect("pandoc probe"),
                defuddle.join().expect("defuddle probe"),
                docling.join().expect("docling probe"),
                ffmpeg.join().expect("ffmpeg probe"),
            ]
        });

        let selected = resolve_pdf_engine(None)
            .ok()
            .map(|value| value.to_string_lossy().into_owned());
        let selected_normalized = selected.as_ref().map(|value| normalize_tool_name(value));

        let mut pdf_engines: Vec<PdfEngineDiagnostic> = thread::scope(|scope| {
            let handles: Vec<_> = pdf_engine_candidates()
                .iter()
                .copied()
                .map(|name| {
                    let selected = selected.clone();
                    let selected_normalized = selected_normalized.clone();
                    scope.spawn(move || {
                        let path = find_executable(name);
                        let readiness = if path.is_some() {
                            Readiness::Ready
                        } else {
                            Readiness::Missing
                        };
                        let version = path
                            .as_ref()
                            .and_then(|p| probe_version(p.as_os_str(), &["--version"]));
                        let is_selected = selected_normalized
                            .as_ref()
                            .is_some_and(|chosen| chosen == name)
                            || selected.as_ref().is_some_and(|raw| {
                                Path::new(raw)
                                    .file_name()
                                    .and_then(|f| f.to_str())
                                    .is_some_and(|f| f == name)
                            });
                        PdfEngineDiagnostic {
                            name: name.to_owned(),
                            readiness,
                            version,
                            resolved_path: path,
                            selected: is_selected,
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("pdf engine probe"))
                .collect()
        });

        // Custom SHIFT_PDF_ENGINE path not in the built-in candidate list.
        if let Some(raw) = &selected {
            let basename = normalize_tool_name(raw);
            let already_listed = pdf_engines.iter().any(|engine| {
                engine.selected
                    || engine.name == basename
                    || engine
                        .resolved_path
                        .as_ref()
                        .is_some_and(|path| path.as_os_str() == OsStr::new(raw))
            });
            if !already_listed {
                let path = PathBuf::from(raw);
                let resolved = if is_runnable(&path) {
                    Some(path.clone())
                } else {
                    find_executable(raw.as_str())
                };
                let readiness =
                    if resolved.as_ref().is_some_and(|p| is_runnable(p)) || is_runnable(&path) {
                        Readiness::Ready
                    } else {
                        Readiness::Missing
                    };
                let version = resolved
                    .as_ref()
                    .or(Some(&path))
                    .filter(|p| is_runnable(p))
                    .and_then(|p| probe_version(p.as_os_str(), &["--version"]));
                pdf_engines.push(PdfEngineDiagnostic {
                    name: basename,
                    readiness,
                    version,
                    resolved_path: resolved.or_else(|| path.is_absolute().then_some(path)),
                    selected: true,
                });
            }
        }

        Self {
            engines,
            pdf_engines,
            selected_pdf_engine: selected,
        }
    }

    pub fn engine(&self, id: &str) -> Option<&EngineDiagnostic> {
        self.engines.iter().find(|engine| engine.id == id)
    }

    pub fn is_engine_ready(&self, id: &str) -> bool {
        self.engine(id)
            .map(|engine| engine.readiness.is_ready())
            .unwrap_or(false)
    }

    pub fn any_pdf_engine_ready(&self) -> bool {
        self.pdf_engines
            .iter()
            .any(|engine| engine.readiness.is_ready())
            || self.selected_pdf_engine.as_ref().is_some_and(|path| {
                let p = Path::new(path);
                is_runnable(p) || find_executable(path).is_some()
            })
    }

    /// Ready engines among the five conversion modules.
    pub fn ready_engine_count(&self) -> usize {
        self.engines
            .iter()
            .filter(|engine| engine.readiness.is_ready())
            .count()
    }

    pub fn missing_engines(&self) -> impl Iterator<Item = &EngineDiagnostic> {
        self.engines
            .iter()
            .filter(|engine| !engine.readiness.is_ready())
    }

    /// Script-friendly exit code for `shift-cli doctor`.
    ///
    /// - `0` — at least one conversion engine is ready (a usable partial install)
    /// - `1` — no conversion engines are ready
    ///
    /// Optional engines (Defuddle, Docling) and PDF backends do **not** fail the
    /// exit code. Scripts that need a full install should check `complete=true`
    /// in `--script` output or individual `engine.*=ready` lines.
    pub fn exit_code(&self) -> i32 {
        if self.ready_engine_count() > 0 { 0 } else { 1 }
    }

    pub fn is_healthy(&self) -> bool {
        self.exit_code() == 0
    }

    /// Every conversion engine is ready and at least one PDF engine is ready.
    pub fn is_complete(&self) -> bool {
        self.missing_engines().next().is_none() && self.any_pdf_engine_ready()
    }

    /// Human-readable multi-line report (stdout for doctor).
    pub fn render_text(&self) -> String {
        let sanitize = |value: &str| -> String {
            value
                .chars()
                .map(|ch| if ch.is_control() { '\u{FFFD}' } else { ch })
                .collect()
        };

        let mut out = String::new();
        out.push_str("Shift diagnostics\n");
        out.push_str("=================\n\n");
        out.push_str(
            "Registered capability is what modules advertise. Conversion is currently\n\
             available only when the matching external engine is installed and ready.\n\n",
        );

        out.push_str("Conversion engines\n");
        out.push_str("------------------\n");
        for engine in &self.engines {
            let version = sanitize(engine.version.as_deref().unwrap_or(
                if engine.readiness.is_ready() {
                    "unknown version"
                } else {
                    "—"
                },
            ));
            let path = sanitize(
                &engine
                    .resolved_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not found".into()),
            );
            out.push_str(&format!(
                "{:<12} {:<8} {:<16} {}\n",
                engine.label,
                engine.readiness.label().to_uppercase(),
                version,
                path
            ));
            if !engine.readiness.is_ready() {
                out.push_str(&format!(
                    "             install: {}\n",
                    sanitize(&engine.install_hint)
                ));
                out.push_str(&format!(
                    "             config:  set {} to an absolute path\n",
                    engine.env_override
                ));
            }
            if let Some(notes) = &engine.notes {
                out.push_str(&format!("             note:    {}\n", sanitize(notes)));
            }
        }

        out.push('\n');
        out.push_str("PDF engines (Pandoc)\n");
        out.push_str("--------------------\n");
        if let Some(selected) = &self.selected_pdf_engine {
            out.push_str(&format!("selected: {}\n", sanitize(selected)));
        } else {
            out.push_str("selected: none\n");
        }
        for engine in &self.pdf_engines {
            let marker = if engine.selected { " *" } else { "" };
            let version = sanitize(engine.version.as_deref().unwrap_or(
                if engine.readiness.is_ready() {
                    "unknown"
                } else {
                    "—"
                },
            ));
            let path = sanitize(
                &engine
                    .resolved_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not found".into()),
            );
            out.push_str(&format!(
                "  {:<12} {:<8} {:<16} {}{}\n",
                engine.name,
                engine.readiness.label().to_uppercase(),
                version,
                path,
                marker
            ));
        }
        if !self.any_pdf_engine_ready() {
            out.push_str(
                "\n  install: brew install typst   # recommended lightweight engine\n\
                   \x20          brew install --cask basictex\n\
                   \x20          or set SHIFT_PDF_ENGINE=/path/to/engine\n",
            );
        }

        out.push('\n');
        out.push_str("Summary\n");
        out.push_str("-------\n");
        out.push_str(&format!(
            "engines ready: {}/{}\n",
            self.ready_engine_count(),
            self.engines.len()
        ));
        out.push_str(&format!(
            "pdf engine:    {}\n",
            if self.any_pdf_engine_ready() {
                "ready"
            } else {
                "missing"
            }
        ));
        out.push_str(&format!(
            "status:        {}\n",
            if self.is_complete() {
                "complete"
            } else if self.is_healthy() {
                "ok (partial)"
            } else {
                "degraded"
            }
        ));
        out.push_str(&format!(
            "complete:      {}\n",
            if self.is_complete() { "yes" } else { "no" }
        ));
        out.push_str(&format!("exit code:     {}\n", self.exit_code()));
        out
    }

    /// Compact machine-readable lines (`key=value`) for scripts.
    ///
    /// Values that contain whitespace, `=`, or quotes are double-quoted with
    /// backslash escapes so naive shell parsers stay reliable.
    pub fn render_script(&self) -> String {
        let mut out = String::new();
        for engine in &self.engines {
            out.push_str(&format!(
                "engine.{}={} version={} path={}\n",
                engine.id,
                engine.readiness.label(),
                script_value(engine.version.as_deref().unwrap_or("")),
                script_value(
                    &engine
                        .resolved_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )
            ));
        }
        out.push_str(&format!(
            "pdf.selected={}\n",
            script_value(self.selected_pdf_engine.as_deref().unwrap_or(""))
        ));
        out.push_str(&format!(
            "pdf.ready={}\n",
            if self.any_pdf_engine_ready() {
                "true"
            } else {
                "false"
            }
        ));
        for engine in &self.pdf_engines {
            out.push_str(&format!(
                "pdf.engine.{}={} version={} selected={}\n",
                engine.name,
                engine.readiness.label(),
                script_value(engine.version.as_deref().unwrap_or("")),
                engine.selected
            ));
        }
        out.push_str(&format!("healthy={}\n", self.is_healthy()));
        out.push_str(&format!("complete={}\n", self.is_complete()));
        out.push_str(&format!("exit_code={}\n", self.exit_code()));
        out
    }
}

/// Whether a registered conversion route for `input` → `output` is currently runnable.
pub fn format_availability(
    registry: &ConversionRegistry,
    report: &DiagnosticsReport,
    input: &Path,
    output: OutputFormat,
) -> FormatAvailability {
    let Some(route_modules) = registry.route_module_ids(input, output) else {
        return FormatAvailability::Unsupported;
    };
    if route_modules
        .iter()
        .all(|id| module_ready_for_output(report, id, output))
    {
        FormatAvailability::Available
    } else {
        FormatAvailability::SupportedUnavailable
    }
}

/// Outputs that modules advertise for this input (capability only).
pub fn supported_outputs(registry: &ConversionRegistry, input: &Path) -> Vec<OutputFormat> {
    registry.available_outputs(input)
}

/// Outputs whose conversion routes are ready on this machine right now.
pub fn available_ready_outputs(
    registry: &ConversionRegistry,
    report: &DiagnosticsReport,
    input: &Path,
) -> Vec<OutputFormat> {
    supported_outputs(registry, input)
        .into_iter()
        .filter(|output| {
            format_availability(registry, report, input, *output) == FormatAvailability::Available
        })
        .collect()
}

/// URL outputs that are ready right now (capability ∩ engine readiness).
pub fn available_ready_url_outputs(
    registry: &ConversionRegistry,
    report: &DiagnosticsReport,
) -> Vec<OutputFormat> {
    registry
        .available_url_outputs()
        .into_iter()
        .filter(|output| {
            // URL routes always start at Defuddle (or a Defuddle chain).
            if !report.is_engine_ready("defuddle") {
                return false;
            }
            // Two-step URL routes need the second module as well; approximate by
            // checking whether any ready module can finish the advertised output.
            registry
                .url_route_module_ids(*output)
                .map(|ids| {
                    ids.iter()
                        .all(|id| module_ready_for_output(report, id, *output))
                })
                .unwrap_or(false)
        })
        .collect()
}

fn module_ready_for_output(
    report: &DiagnosticsReport,
    module_id: &str,
    output: OutputFormat,
) -> bool {
    if !report.is_engine_ready(module_id) {
        return false;
    }
    // Pandoc PDF additionally needs a PDF engine.
    if module_id == "pandoc" && output == OutputFormat::PDF {
        return report.any_pdf_engine_ready();
    }
    true
}

// --- Probes -----------------------------------------------------------------

fn probe_markitdown() -> EngineDiagnostic {
    let install_hint =
        "python3 -m pip install 'markitdown[all]'  (or: uv pip install 'markitdown[all]')".into();
    let env_override = "SHIFT_MARKITDOWN_BIN";
    let resolved = resolve_tool_path(
        env_override,
        "markitdown",
        &[project_venv_bin("markitdown")],
    );
    finish_engine_probe(
        "markitdown",
        "MarkItDown",
        resolved,
        env_override,
        install_hint,
        &["--version"],
        None,
    )
}

fn probe_pandoc() -> EngineDiagnostic {
    let install_hint = "brew install pandoc".into();
    let env_override = "SHIFT_PANDOC_BIN";
    let resolved = resolve_tool_path(env_override, "pandoc", &[]);
    finish_engine_probe(
        "pandoc",
        "Pandoc",
        resolved,
        env_override,
        install_hint,
        &["--version"],
        Some("PDF output also needs a PDF engine (see below).".into()),
    )
}

fn probe_defuddle() -> EngineDiagnostic {
    let env_override = "SHIFT_DEFUDDLE_BIN";
    let mut locals = Vec::new();
    if let Some(bundled) = bundled_runtime_tool("defuddle") {
        locals.push(bundled);
    }
    locals.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("node_modules/.bin/defuddle"));

    let node = find_executable("node");
    let install_hint = if node.is_none() {
        "brew install node  # Shift ships Defuddle; Node is required to run it\n# or: export SHIFT_NODE_BIN=/absolute/path/to/node".into()
    } else {
        "npm install -g defuddle  # or set SHIFT_DEFUDDLE_BIN=/absolute/path/to/defuddle".into()
    };
    let notes = if node.is_none() {
        Some(
            "Packaged Shift embeds Defuddle but needs a system Node binary \
             (Homebrew, nvm, fnm, volta, asdf, or mise)."
                .into(),
        )
    } else {
        None
    };

    let mut diagnostic = finish_engine_probe(
        "defuddle",
        "Defuddle",
        resolve_tool_path(env_override, "defuddle", &locals),
        env_override,
        install_hint,
        &["--version"],
        notes,
    );

    // A packaged shell launcher can exist and still fail when Node is missing.
    // Prefer Missing so Settings and failure install hints match runtime reality.
    if diagnostic.readiness.is_ready() && diagnostic.version.is_none() && node.is_none() {
        diagnostic.readiness = Readiness::Missing;
        if diagnostic.notes.is_none() {
            diagnostic.notes =
                Some("Defuddle launcher found, but Node.js is not available to run it.".into());
        }
    }

    diagnostic
}

fn probe_docling() -> EngineDiagnostic {
    let install_hint = "pip install docling  (or: uv pip install docling)".into();
    let env_override = "SHIFT_DOCLING_BIN";
    let resolved = resolve_tool_path(env_override, "docling", &[project_venv_bin("docling")]);
    finish_engine_probe(
        "docling",
        "Docling",
        resolved,
        env_override,
        install_hint,
        &["--version"],
        Some("First runs may download model weights.".into()),
    )
}

fn probe_ffmpeg() -> EngineDiagnostic {
    let install_hint = "brew install ffmpeg".into();
    let env_override = "SHIFT_FFMPEG_BIN";
    let resolved = resolve_tool_path(env_override, "ffmpeg", &[]);
    finish_engine_probe(
        "ffmpeg",
        "FFmpeg",
        resolved,
        env_override,
        install_hint,
        &["-version"],
        None,
    )
}

fn project_venv_bin(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".venv/bin")
        .join(name)
}

fn finish_engine_probe(
    id: &'static str,
    label: &'static str,
    resolved: Option<PathBuf>,
    env_override: &'static str,
    install_hint: String,
    version_args: &[&str],
    notes: Option<String>,
) -> EngineDiagnostic {
    let Some(path) = resolved else {
        return EngineDiagnostic {
            id,
            label,
            readiness: Readiness::Missing,
            version: None,
            resolved_path: None,
            env_override,
            install_hint,
            notes,
        };
    };

    let runnable = is_runnable(&path);
    if !runnable && find_executable(path.as_os_str()).is_none() {
        return EngineDiagnostic {
            id,
            label,
            readiness: Readiness::Missing,
            version: None,
            resolved_path: Some(path),
            env_override,
            install_hint,
            notes,
        };
    }

    let version = probe_version(path.as_os_str(), version_args);
    // Prefer Ready when the binary is present even if version parsing fails
    // (unusual flags or slow cold starts that still prove the path is real).
    let readiness = if runnable || version.is_some() {
        Readiness::Ready
    } else {
        Readiness::Missing
    };

    EngineDiagnostic {
        id,
        label,
        readiness,
        version,
        resolved_path: Some(path),
        env_override,
        install_hint,
        notes,
    }
}

fn probe_version(executable: &OsStr, args: &[&str]) -> Option<String> {
    let mut command = Command::new(executable);
    for arg in args {
        command.arg(arg);
    }
    let output = run_command(command, VERSION_PROBE_TIMEOUT, VERSION_PROBE_MAX_BYTES).ok()?;
    // Only parse version text when the probe exited successfully. Failed
    // processes often print usage/errors that look like version lines.
    // Readiness can still be Ready via is_runnable when the binary exists.
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    parse_version_text(&text)
}

/// Quote script values that would break naive `key=value` parsers.
fn script_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let needs_quotes = value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '=' | '"' | '\\' | '\'' | '\n' | '\r'));
    if !needs_quotes {
        return value.to_owned();
    }
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' | '"' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn parse_version_text(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;

    // "Docling version: 2.112.0"
    if let Some(rest) = line.strip_prefix("Docling version:") {
        let v = rest.trim();
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }

    // "ffmpeg version 8.1.2 Copyright ..."
    if let Some(rest) = line.strip_prefix("ffmpeg version ") {
        let v = rest.split_whitespace().next().unwrap_or(rest).to_owned();
        return Some(v);
    }

    // "pandoc 3.10", "markitdown 0.1.5", "typst 0.15.0 (...)", bare "0.19.1"
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() == 1 {
        return Some(tokens[0].to_owned());
    }
    // Prefer the first token that looks like a version number.
    for token in &tokens {
        if looks_like_version(token) {
            return Some((*token).to_owned());
        }
    }
    // Fallback: second token (common "name version" form).
    if tokens.len() >= 2 {
        return Some(tokens[1].trim_matches(|c| c == ',' || c == ';').to_owned());
    }
    Some(line.to_owned())
}

fn looks_like_version(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_digit() && token.contains('.')
}

fn normalize_tool_name(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn parse_version_handles_common_tools() {
        assert_eq!(
            parse_version_text("pandoc 3.10\nFeatures: ...\n").as_deref(),
            Some("3.10")
        );
        assert_eq!(
            parse_version_text("markitdown 0.1.5\n").as_deref(),
            Some("0.1.5")
        );
        assert_eq!(parse_version_text("0.19.1\n").as_deref(), Some("0.19.1"));
        assert_eq!(
            parse_version_text("Docling version: 2.112.0\nDocling Core version: 2.87.0\n")
                .as_deref(),
            Some("2.112.0")
        );
        assert_eq!(
            parse_version_text(
                "ffmpeg version 8.1.2 Copyright (c) 2000-2026 the FFmpeg developers\n"
            )
            .as_deref(),
            Some("8.1.2")
        );
        assert_eq!(
            parse_version_text("typst 0.15.0 (unknown commit)\n").as_deref(),
            Some("0.15.0")
        );
    }

    #[test]
    fn readiness_labels_are_stable_for_scripts() {
        assert_eq!(Readiness::Ready.label(), "ready");
        assert_eq!(Readiness::Missing.label(), "missing");
    }

    #[test]
    fn exit_code_is_zero_when_any_engine_is_ready() {
        // A single ready engine is enough for exit 0 (partial installs are ok).
        let partial = DiagnosticsReport {
            engines: vec![
                EngineDiagnostic {
                    id: "pandoc",
                    label: "Pandoc",
                    readiness: Readiness::Ready,
                    version: Some("3.10".into()),
                    resolved_path: Some(PathBuf::from("/usr/bin/pandoc")),
                    env_override: "SHIFT_PANDOC_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "defuddle",
                    label: "Defuddle",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_DEFUDDLE_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
            ],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };
        assert_eq!(partial.exit_code(), 0);
        assert!(partial.is_healthy());
        assert!(!partial.is_complete());

        let engines: Vec<_> = ["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"]
            .into_iter()
            .map(|id| EngineDiagnostic {
                id,
                label: id,
                readiness: Readiness::Ready,
                version: Some("1".into()),
                resolved_path: Some(PathBuf::from(format!("/bin/{id}"))),
                env_override: "X",
                install_hint: String::new(),
                notes: None,
            })
            .collect();
        let complete = DiagnosticsReport {
            engines,
            pdf_engines: vec![PdfEngineDiagnostic {
                name: "typst".into(),
                readiness: Readiness::Ready,
                version: Some("0.15".into()),
                resolved_path: Some(PathBuf::from("/usr/bin/typst")),
                selected: true,
            }],
            selected_pdf_engine: Some("typst".into()),
        };
        assert_eq!(complete.exit_code(), 0);
        assert!(complete.is_complete());

        let empty = DiagnosticsReport {
            engines: vec![EngineDiagnostic {
                id: "ffmpeg",
                label: "FFmpeg",
                readiness: Readiness::Missing,
                version: None,
                resolved_path: None,
                env_override: "SHIFT_FFMPEG_BIN",
                install_hint: String::new(),
                notes: None,
            }],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };
        assert_eq!(empty.exit_code(), 1);
        assert!(!empty.is_healthy());
    }

    #[test]
    fn script_render_includes_exit_code_and_quotes_paths() {
        let report = DiagnosticsReport {
            engines: vec![EngineDiagnostic {
                id: "ffmpeg",
                label: "FFmpeg",
                readiness: Readiness::Missing,
                version: None,
                resolved_path: Some(PathBuf::from("/opt/home path/bin/ffmpeg")),
                env_override: "SHIFT_FFMPEG_BIN",
                install_hint: "brew install ffmpeg".into(),
                notes: None,
            }],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };
        let text = report.render_script();
        assert!(text.contains("engine.ffmpeg=missing"));
        assert!(text.contains("exit_code=1"));
        assert!(text.contains("healthy=false"));
        assert!(text.contains("complete=false"));
        assert!(
            text.contains("path=\"/opt/home path/bin/ffmpeg\""),
            "paths with spaces should be quoted: {text}"
        );
    }

    #[test]
    fn script_value_escapes_special_characters() {
        assert_eq!(script_value("plain"), "plain");
        assert_eq!(script_value("a b"), "\"a b\"");
        assert_eq!(script_value("a=b"), "\"a=b\"");
        assert_eq!(script_value("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[cfg(unix)]
    #[test]
    fn probe_respects_env_override_and_version() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "shift-diag-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-pandoc");
        write_script(&bin, "#!/bin/sh\necho 'pandoc 9.9.9'\n");

        unsafe {
            std::env::set_var("SHIFT_PANDOC_BIN", &bin);
        }
        let engine = probe_pandoc();
        unsafe {
            std::env::remove_var("SHIFT_PANDOC_BIN");
        }

        assert_eq!(engine.readiness, Readiness::Ready);
        assert_eq!(engine.version.as_deref(), Some("9.9.9"));
        assert_eq!(engine.resolved_path.as_deref(), Some(bin.as_path()));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn missing_engine_is_reported() {
        let _guard = ENV_LOCK.lock().unwrap();
        let missing = std::env::temp_dir().join(format!(
            "shift-diag-missing-{}-no-such-bin",
            std::process::id()
        ));
        unsafe {
            std::env::set_var("SHIFT_FFMPEG_BIN", &missing);
        }
        let engine = probe_ffmpeg();
        unsafe {
            std::env::remove_var("SHIFT_FFMPEG_BIN");
        }
        assert_eq!(engine.readiness, Readiness::Missing);
        assert!(engine.install_hint.contains("ffmpeg"));
    }

    #[test]
    fn format_availability_distinguishes_supported_from_ready() {
        use super::super::{
            ConversionArtifact, ConversionError, ConversionModule, ConversionOptions,
        };

        struct AlwaysMarkdown;
        impl ConversionModule for AlwaysMarkdown {
            fn id(&self) -> &'static str {
                "markitdown"
            }
            fn label(&self) -> &'static str {
                "MarkItDown"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["txt"]
            }
            fn output_formats(&self) -> &'static [OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &'static [OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn convert(
                &self,
                _input: &Path,
                output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                Ok(ConversionArtifact {
                    file_name: "x.md".into(),
                    media_type: "text/markdown",
                    bytes: b"# hi".to_vec(),
                    format: output,
                    module_id: self.id(),
                    pipeline: Vec::new(),
                    invocations: Vec::new(),
                })
            }
        }

        let registry = ConversionRegistry::new().with_module(AlwaysMarkdown);
        let mut report = DiagnosticsReport {
            engines: vec![EngineDiagnostic {
                id: "markitdown",
                label: "MarkItDown",
                readiness: Readiness::Missing,
                version: None,
                resolved_path: None,
                env_override: "SHIFT_MARKITDOWN_BIN",
                install_hint: String::new(),
                notes: None,
            }],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };

        let input = Path::new("notes.txt");
        assert_eq!(
            format_availability(&registry, &report, input, OutputFormat::MARKDOWN),
            FormatAvailability::SupportedUnavailable
        );
        assert_eq!(
            format_availability(&registry, &report, input, OutputFormat::PDF),
            FormatAvailability::Unsupported
        );

        report.engines[0].readiness = Readiness::Ready;
        assert_eq!(
            format_availability(&registry, &report, input, OutputFormat::MARKDOWN),
            FormatAvailability::Available
        );
    }

    #[test]
    fn collect_returns_five_engines() {
        // Smoke test against the real machine; does not assert readiness.
        let report = DiagnosticsReport::collect();
        assert_eq!(report.engines.len(), 5);
        assert!(!report.pdf_engines.is_empty());
        let ids: Vec<_> = report.engines.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            vec!["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"]
        );
    }

    #[test]
    fn readiness_and_availability_labels_and_display() {
        assert_eq!(Readiness::Ready.to_string(), "ready");
        assert_eq!(Readiness::Missing.to_string(), "missing");
        assert_eq!(FormatAvailability::Available.label(), "available");
        assert_eq!(
            FormatAvailability::SupportedUnavailable.label(),
            "supported (engine missing)"
        );
        assert_eq!(FormatAvailability::Unsupported.label(), "unsupported");
    }

    #[test]
    fn report_lookups_and_pdf_readiness() {
        let report = DiagnosticsReport {
            engines: vec![
                EngineDiagnostic {
                    id: "markitdown",
                    label: "MarkItDown",
                    readiness: Readiness::Ready,
                    version: Some("1.0".into()),
                    resolved_path: Some(PathBuf::from("/bin/markitdown")),
                    env_override: "SHIFT_MARKITDOWN_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "pandoc",
                    label: "Pandoc",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_PANDOC_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
            ],
            pdf_engines: vec![PdfEngineDiagnostic {
                name: "typst".into(),
                readiness: Readiness::Ready,
                version: Some("0.15".into()),
                resolved_path: Some(PathBuf::from("/bin/typst")),
                selected: true,
            }],
            selected_pdf_engine: Some("typst".into()),
        };

        assert!(report.engine("markitdown").is_some());
        assert!(report.engine("missing").is_none());
        assert!(report.is_engine_ready("markitdown"));
        assert!(!report.is_engine_ready("pandoc"));
        assert!(report.any_pdf_engine_ready());
        assert_eq!(report.missing_engines().count(), 1);
        assert_eq!(report.ready_engine_count(), 1);
        assert!(report.is_healthy());
        assert!(!report.is_complete());
    }

    #[cfg(unix)]
    #[test]
    fn any_pdf_engine_ready_falls_back_to_selected_executable() {
        let dir = std::env::temp_dir().join(format!("shift-diag-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("typst");
        std::fs::write(&exe, "#!/bin/sh\necho typst 0.15.0\n").unwrap();
        let mut permissions = std::fs::metadata(&exe).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&exe, permissions).unwrap();

        let report = DiagnosticsReport {
            engines: vec![],
            pdf_engines: vec![],
            selected_pdf_engine: Some(exe.to_string_lossy().into_owned()),
        };
        assert!(report.any_pdf_engine_ready());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_text_covers_notes_and_unknown_versions() {
        let report = DiagnosticsReport {
            engines: vec![
                EngineDiagnostic {
                    id: "markitdown",
                    label: "MarkItDown",
                    readiness: Readiness::Ready,
                    version: None,
                    resolved_path: Some(PathBuf::from("/bin/markitdown")),
                    env_override: "SHIFT_MARKITDOWN_BIN",
                    install_hint: String::new(),
                    notes: Some("first runs may download models".into()),
                },
                EngineDiagnostic {
                    id: "pandoc",
                    label: "Pandoc",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_PANDOC_BIN",
                    install_hint: "brew install pandoc".into(),
                    notes: None,
                },
            ],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };
        let text = report.render_text();
        assert!(text.contains("Shift diagnostics"));
        assert!(text.contains("Conversion engines"));
        assert!(text.contains("PDF engines (Pandoc)"));
        assert!(text.contains("Summary"));
        assert!(text.contains("unknown version"));
        assert!(text.contains("first runs may download models"));
        assert!(text.contains("brew install pandoc"));
        assert!(text.contains("engines ready: 1/2"));
        assert!(text.contains("pdf engine:    missing"));
        assert!(text.contains("status:        ok (partial)"));
    }

    #[test]
    fn render_script_includes_pdf_entries() {
        let report = DiagnosticsReport {
            engines: vec![],
            pdf_engines: vec![PdfEngineDiagnostic {
                name: "typst".into(),
                readiness: Readiness::Ready,
                version: Some("0.15".into()),
                resolved_path: Some(PathBuf::from("/bin/typst")),
                selected: true,
            }],
            selected_pdf_engine: Some("typst".into()),
        };
        let text = report.render_script();
        assert!(text.contains("pdf.engine.typst=ready"));
        assert!(text.contains("pdf.ready=true"));
        assert!(text.contains("pdf.selected=typst"));
        assert!(text.contains("healthy=false"));
        assert!(text.contains("complete=true"));
    }

    #[test]
    fn supported_and_ready_outputs_filtering() {
        let registry = ConversionRegistry::default();
        let mut report = DiagnosticsReport {
            engines: vec![EngineDiagnostic {
                id: "markitdown",
                label: "MarkItDown",
                readiness: Readiness::Missing,
                version: None,
                resolved_path: None,
                env_override: "SHIFT_MARKITDOWN_BIN",
                install_hint: String::new(),
                notes: None,
            }],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };

        let txt = Path::new("notes.txt");
        assert!(supported_outputs(&registry, txt).contains(&OutputFormat::MARKDOWN));
        assert!(available_ready_outputs(&registry, &report, txt).is_empty());

        report.engines[0].readiness = Readiness::Ready;
        let ready = available_ready_outputs(&registry, &report, txt);
        assert!(ready.contains(&OutputFormat::MARKDOWN));
    }

    #[test]
    fn ready_url_outputs_require_defuddle_and_second_hop() {
        let registry = ConversionRegistry::default();
        let mut report = DiagnosticsReport {
            engines: vec![
                EngineDiagnostic {
                    id: "defuddle",
                    label: "Defuddle",
                    readiness: Readiness::Ready,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_DEFUDDLE_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "pandoc",
                    label: "Pandoc",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_PANDOC_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
            ],
            pdf_engines: vec![PdfEngineDiagnostic {
                name: "typst".into(),
                readiness: Readiness::Ready,
                version: None,
                resolved_path: None,
                selected: true,
            }],
            selected_pdf_engine: Some("typst".into()),
        };

        let ready = available_ready_url_outputs(&registry, &report);
        assert!(ready.contains(&OutputFormat::MARKDOWN));
        assert!(ready.contains(&OutputFormat::HTML));
        assert!(!ready.contains(&OutputFormat::PDF));

        report.engines[1].readiness = Readiness::Ready;
        let ready = available_ready_url_outputs(&registry, &report);
        assert!(ready.contains(&OutputFormat::PDF));
    }

    #[test]
    fn format_availability_pandoc_pdf_needs_pdf_engine() {
        let registry = ConversionRegistry::default();
        let report = DiagnosticsReport {
            engines: vec![EngineDiagnostic {
                id: "pandoc",
                label: "Pandoc",
                readiness: Readiness::Ready,
                version: None,
                resolved_path: None,
                env_override: "SHIFT_PANDOC_BIN",
                install_hint: String::new(),
                notes: None,
            }],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };

        assert_eq!(
            format_availability(
                &registry,
                &report,
                Path::new("report.md"),
                OutputFormat::PDF
            ),
            FormatAvailability::SupportedUnavailable
        );
    }

    #[test]
    fn finish_engine_probe_handles_none_and_missing() {
        let engine = finish_engine_probe(
            "missing",
            "Missing",
            None,
            "SHIFT_MISSING_BIN",
            "install me".into(),
            &["--version"],
            None,
        );
        assert_eq!(engine.readiness, Readiness::Missing);
        assert!(engine.resolved_path.is_none());

        let dir = std::env::temp_dir().join(format!("shift-diag-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let broken = dir.join("broken");
        std::fs::write(&broken, b"not executable").unwrap();
        let engine = finish_engine_probe(
            "broken",
            "Broken",
            Some(broken.clone()),
            "SHIFT_BROKEN_BIN",
            "install me".into(),
            &["--version"],
            None,
        );
        assert_eq!(engine.readiness, Readiness::Missing);
        assert_eq!(engine.resolved_path, Some(broken));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_version_text_and_looks_like_version_edge_cases() {
        assert_eq!(parse_version_text(""), None);
        assert_eq!(parse_version_text("   "), None);
        assert_eq!(parse_version_text("1.2.3"), Some("1.2.3".into()));
        assert_eq!(parse_version_text("foo 1.2.3"), Some("1.2.3".into()));
        assert_eq!(parse_version_text("name value extra"), Some("value".into()));
        assert_eq!(
            parse_version_text("Docling version: 2.0"),
            Some("2.0".into())
        );
        assert_eq!(
            parse_version_text("Docling version: 2.0\n"),
            Some("2.0".into())
        );

        assert!(looks_like_version("1.2.3"));
        assert!(!looks_like_version("v1.2"));
        assert!(!looks_like_version("123"));
        assert!(looks_like_version("1."));
        assert!(!looks_like_version(".1"));
    }

    #[test]
    fn normalize_tool_name_and_script_value_edge_cases() {
        assert_eq!(normalize_tool_name("/usr/bin/ffmpeg"), "ffmpeg");
        assert_eq!(normalize_tool_name("ffmpeg"), "ffmpeg");
        assert_eq!(normalize_tool_name("/path/to/my-tool"), "my-tool");

        assert_eq!(script_value("a b"), "\"a b\"");
        assert_eq!(script_value("a\nb"), "\"a\\nb\"");
        assert_eq!(script_value("a\rb"), "\"a\\rb\"");
        assert_eq!(script_value(""), "");
    }

    #[test]
    fn engine_ids_match_default_registry_module_ids() {
        let report = DiagnosticsReport::collect();
        let registry = ConversionRegistry::default();
        let module_ids: Vec<_> = registry.modules().map(|m| m.id()).collect();
        let engine_ids: Vec<_> = report.engines.iter().map(|e| e.id).collect();
        assert_eq!(
            engine_ids, module_ids,
            "doctor engines must stay aligned with ConversionRegistry::default"
        );
        for engine in &report.engines {
            assert!(!engine.label.is_empty());
            assert!(!engine.env_override.is_empty());
            assert!(engine.env_override.starts_with("SHIFT_"));
            assert!(matches!(
                engine.readiness,
                Readiness::Ready | Readiness::Missing
            ));
        }
    }

    #[test]
    fn format_availability_matrix_for_common_pairs() {
        // Synthetic report: only markitdown + ffmpeg ready.
        let registry = ConversionRegistry::default();
        let report = DiagnosticsReport {
            engines: vec![
                EngineDiagnostic {
                    id: "markitdown",
                    label: "MarkItDown",
                    readiness: Readiness::Ready,
                    version: Some("1".into()),
                    resolved_path: Some(PathBuf::from("/bin/markitdown")),
                    env_override: "SHIFT_MARKITDOWN_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "pandoc",
                    label: "Pandoc",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_PANDOC_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "defuddle",
                    label: "Defuddle",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_DEFUDDLE_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "docling",
                    label: "Docling",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_DOCLING_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "ffmpeg",
                    label: "FFmpeg",
                    readiness: Readiness::Ready,
                    version: Some("8".into()),
                    resolved_path: Some(PathBuf::from("/bin/ffmpeg")),
                    env_override: "SHIFT_FFMPEG_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
            ],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };

        let cases = [
            (
                "notes.txt",
                OutputFormat::MARKDOWN,
                FormatAvailability::Available,
            ),
            ("clip.mp4", OutputFormat::MP3, FormatAvailability::Available),
            ("clip.mp4", OutputFormat::PNG, FormatAvailability::Available),
            (
                "scan.pdf",
                OutputFormat::HTML,
                FormatAvailability::SupportedUnavailable, // docling missing
            ),
            (
                "mystery.xyz",
                OutputFormat::MARKDOWN,
                FormatAvailability::Unsupported,
            ),
            (
                "notes.md",
                OutputFormat::DOCX,
                FormatAvailability::SupportedUnavailable, // pandoc missing
            ),
        ];
        for (input, output, expected) in cases {
            let actual = format_availability(&registry, &report, Path::new(input), output);
            assert_eq!(
                actual,
                expected,
                "availability for {input} → {}",
                output.id()
            );
        }
    }

    #[test]
    fn supported_outputs_cover_media_for_video_input() {
        let registry = ConversionRegistry::default();
        let outs = supported_outputs(&registry, Path::new("clip.mp4"));
        for format in OutputFormat::MEDIA {
            assert!(
                outs.contains(format),
                "supported_outputs missing {}",
                format.id()
            );
        }
    }

    #[test]
    fn readiness_complete_requires_all_engines_and_pdf() {
        let mut engines: Vec<_> = ["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"]
            .into_iter()
            .map(|id| EngineDiagnostic {
                id,
                label: id,
                readiness: Readiness::Ready,
                version: Some("1".into()),
                resolved_path: Some(PathBuf::from(format!("/bin/{id}"))),
                env_override: "X",
                install_hint: String::new(),
                notes: None,
            })
            .collect();
        let mut report = DiagnosticsReport {
            engines: engines.clone(),
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };
        // Without a ready PDF engine, complete may still be false depending on policy.
        let complete_without_pdf = report.is_complete();

        report.pdf_engines.push(PdfEngineDiagnostic {
            name: "typst".into(),
            readiness: Readiness::Ready,
            version: Some("0.15".into()),
            resolved_path: Some(PathBuf::from("/bin/typst")),
            selected: true,
        });
        report.selected_pdf_engine = Some("typst".into());
        assert!(report.is_complete());
        assert!(report.is_healthy());
        assert_eq!(report.exit_code(), 0);

        engines[0].readiness = Readiness::Missing;
        report.engines = engines;
        assert!(!report.is_complete());
        assert!(report.is_healthy()); // still partial ok
        let _ = complete_without_pdf;
    }

    #[test]
    fn script_render_keys_for_all_engines() {
        let engines: Vec<_> = ["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"]
            .into_iter()
            .map(|id| EngineDiagnostic {
                id,
                label: id,
                readiness: Readiness::Missing,
                version: None,
                resolved_path: None,
                env_override: "X",
                install_hint: "hint".into(),
                notes: None,
            })
            .collect();
        let report = DiagnosticsReport {
            engines,
            pdf_engines: vec![],
            selected_pdf_engine: None,
        };
        let script = report.render_script();
        for id in ["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"] {
            assert!(
                script.contains(&format!("engine.{id}=missing")),
                "missing key for {id} in:\n{script}"
            );
        }
        assert!(script.contains("exit_code=1"));
        assert!(script.contains("healthy=false"));
    }

    #[test]
    fn parse_version_remaining_pure_edges() {
        // Blank lines before content.
        assert_eq!(
            parse_version_text("\n\n  \npandoc 3.1\n").as_deref(),
            Some("3.1")
        );
        // CRLF.
        assert_eq!(
            parse_version_text("ffmpeg version 7.0.1 Copyright\r\n").as_deref(),
            Some("7.0.1")
        );
        // Empty Docling version suffix: prefix match rejects empty rest, then
        // second-token fallback yields the leftover label token (not a panic).
        assert_eq!(
            parse_version_text("Docling version: ").as_deref(),
            Some("version:")
        );
        assert_eq!(
            parse_version_text("Docling version:\n").as_deref(),
            Some("version:")
        );

        // Parenthesized commit still yields the version token.
        assert_eq!(
            parse_version_text("typst 0.11.0 (abc1234)\n").as_deref(),
            Some("0.11.0")
        );
        // Version-like second token is preferred even when it still carries a
        // trailing comma (looks_like_version matches before trim fallback).
        assert_eq!(
            parse_version_text("tool 1.0-beta,").as_deref(),
            Some("1.0-beta,")
        );
        // Non-version second token still uses the comma/semicolon trim path.
        assert_eq!(parse_version_text("tool beta,").as_deref(), Some("beta"));
        // Bare multipartite version.
        assert_eq!(parse_version_text("10.20.30").as_deref(), Some("10.20.30"));
        // Non-version first token, version second.
        assert_eq!(
            parse_version_text("release 4.5.6-rc1\n").as_deref(),
            Some("4.5.6-rc1")
        );
        // looks_like_version additional edges.
        assert!(looks_like_version("0.0.1"));
        assert!(looks_like_version("2."));
        assert!(!looks_like_version(""));
        assert!(!looks_like_version("v"));
        assert!(!looks_like_version("v2.0"));
        assert!(!looks_like_version("abc"));
        assert!(!looks_like_version("12"));
        assert!(!looks_like_version(".2.3"));
    }

    #[test]
    fn readiness_is_ready_and_labels_cover_both_variants() {
        assert!(Readiness::Ready.is_ready());
        assert!(!Readiness::Missing.is_ready());
        assert_eq!(Readiness::Ready.label(), "ready");
        assert_eq!(Readiness::Missing.label(), "missing");
        assert_eq!(format!("{}", Readiness::Ready), "ready");
        assert_eq!(format!("{}", Readiness::Missing), "missing");
        assert_eq!(FormatAvailability::Available.label(), "available");
        assert_eq!(
            FormatAvailability::SupportedUnavailable.label(),
            "supported (engine missing)"
        );
        assert_eq!(FormatAvailability::Unsupported.label(), "unsupported");
    }

    #[test]
    fn format_availability_for_every_output_format_all() {
        let registry = ConversionRegistry::default();
        // Synthetic report: all engines ready, PDF engine ready.
        let engines: Vec<_> = ["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"]
            .into_iter()
            .map(|id| EngineDiagnostic {
                id,
                label: id,
                readiness: Readiness::Ready,
                version: Some("1".into()),
                resolved_path: Some(PathBuf::from(format!("/bin/{id}"))),
                env_override: "X",
                install_hint: String::new(),
                notes: None,
            })
            .collect();
        let report = DiagnosticsReport {
            engines,
            pdf_engines: vec![PdfEngineDiagnostic {
                name: "typst".into(),
                readiness: Readiness::Ready,
                version: Some("0.15".into()),
                resolved_path: Some(PathBuf::from("/bin/typst")),
                selected: true,
            }],
            selected_pdf_engine: Some("typst".into()),
        };

        let md = Path::new("notes.md");
        let mut available = 0usize;
        let mut supported_unavailable = 0usize;
        let mut unsupported = 0usize;
        for format in OutputFormat::ALL {
            let avail = format_availability(&registry, &report, md, *format);
            match avail {
                FormatAvailability::Available => available += 1,
                FormatAvailability::SupportedUnavailable => supported_unavailable += 1,
                FormatAvailability::Unsupported => unsupported += 1,
            }
            // Must not panic and must be one of the three variants (exhaustive match).
            assert!(
                matches!(
                    avail,
                    FormatAvailability::Available
                        | FormatAvailability::SupportedUnavailable
                        | FormatAvailability::Unsupported
                ),
                "unexpected availability for {}",
                format.id()
            );
        }
        // Markdown input should support many Pandoc writers when engines are ready.
        assert!(
            available > 0,
            "expected some Available formats for notes.md, got available={available} unsupported={unsupported}"
        );
        let _ = supported_unavailable;

        // Unknown extension is unsupported for every catalog format.
        let mystery = Path::new("file.xyzzy");
        for format in OutputFormat::ALL {
            assert_eq!(
                format_availability(&registry, &report, mystery, *format),
                FormatAvailability::Unsupported,
                "mystery.xyzzy → {}",
                format.id()
            );
        }
    }

    #[test]
    fn missing_version_output_in_text_and_script() {
        let report = DiagnosticsReport {
            engines: vec![
                EngineDiagnostic {
                    id: "ffmpeg",
                    label: "FFmpeg",
                    readiness: Readiness::Ready,
                    version: None,
                    resolved_path: Some(PathBuf::from("/bin/ffmpeg")),
                    env_override: "SHIFT_FFMPEG_BIN",
                    install_hint: String::new(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "pandoc",
                    label: "Pandoc",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_PANDOC_BIN",
                    install_hint: "brew install pandoc".into(),
                    notes: None,
                },
            ],
            pdf_engines: vec![PdfEngineDiagnostic {
                name: "typst".into(),
                readiness: Readiness::Ready,
                version: None,
                resolved_path: Some(PathBuf::from("/bin/typst")),
                selected: true,
            }],
            selected_pdf_engine: Some("typst".into()),
        };

        let text = report.render_text();
        assert!(
            text.contains("unknown version"),
            "ready engine without version should show 'unknown version':\n{text}"
        );
        assert!(
            text.contains("—") || text.contains("missing"),
            "missing engine should not invent a version:\n{text}"
        );
        // PDF ready without version uses "unknown".
        assert!(
            text.contains("unknown"),
            "ready PDF engine without version should mention unknown:\n{text}"
        );

        let script = report.render_script();
        assert!(script.contains("engine.ffmpeg=ready"), "script:\n{script}");
        // Empty version renders as version= (empty value).
        assert!(
            script.contains("engine.ffmpeg=ready version="),
            "missing version should be empty value:\n{script}"
        );
        assert!(script.contains("engine.pandoc=missing"));
        assert!(script.contains("pdf.engine.typst=ready"));
    }

    #[test]
    fn script_key_matrix_mixed_readiness() {
        let ids = ["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"];
        let readiness = [
            Readiness::Ready,
            Readiness::Missing,
            Readiness::Ready,
            Readiness::Missing,
            Readiness::Ready,
        ];
        let engines: Vec<_> = ids
            .iter()
            .zip(readiness.iter())
            .map(|(id, readiness)| EngineDiagnostic {
                id,
                label: id,
                readiness: *readiness,
                version: if readiness.is_ready() {
                    Some("1.0".into())
                } else {
                    None
                },
                resolved_path: if readiness.is_ready() {
                    Some(PathBuf::from(format!("/bin/{id}")))
                } else {
                    None
                },
                env_override: "X",
                install_hint: "hint".into(),
                notes: None,
            })
            .collect();
        let report = DiagnosticsReport {
            engines,
            pdf_engines: vec![
                PdfEngineDiagnostic {
                    name: "typst".into(),
                    readiness: Readiness::Ready,
                    version: Some("0.15".into()),
                    resolved_path: Some(PathBuf::from("/bin/typst")),
                    selected: true,
                },
                PdfEngineDiagnostic {
                    name: "pdflatex".into(),
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    selected: false,
                },
            ],
            selected_pdf_engine: Some("typst".into()),
        };

        let script = report.render_script();
        for (id, readiness) in ids.iter().zip(readiness.iter()) {
            let expected = format!("engine.{id}={}", readiness.label());
            assert!(
                script.contains(&expected),
                "expected {expected} in:\n{script}"
            );
        }
        assert!(script.contains("pdf.engine.typst=ready"));
        assert!(script.contains("pdf.engine.pdflatex=missing"));
        assert!(script.contains("pdf.selected=typst"));
        assert!(script.contains("pdf.ready=true"));
        assert!(script.contains("healthy=true"));
        assert!(script.contains("complete=false")); // missing engines
        assert!(script.contains("exit_code=0"));
        // Paths with no spaces stay unquoted.
        assert!(
            script.contains("path=/bin/markitdown") || script.contains("path=\"/bin/markitdown\"")
        );
    }
}
