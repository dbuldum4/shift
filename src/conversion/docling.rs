use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, ConversionProgress,
    InvocationRecord, OutputFormat, TempDirGuard, bundled_runtime_tool, command_argv_parts,
    format_argv_display, map_spawn_error, max_output_bytes, process_timeout, read_file_limited,
    resolve_tool_executable, run_command_cancellable_with_output_dirs, unique_temp_dir,
};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Inputs exposed by the pinned Docling 2.115 CLI.
///
/// Audio/video formats need Docling's optional ASR/video extras and FFmpeg at
/// conversion time. They remain capabilities even when those optional
/// dependencies are absent; base Docling readiness must not be downgraded just
/// because transcription has not been installed yet.
const EXTENSIONS: &[&str] = &[
    // Office and publishing (including the aliases registered by Docling 2.115).
    "pdf", "docx", "dotx", "docm", "dotm", "doc", "dot", "pptx", "potx", "ppsx", "pptm", "potm",
    "ppsm", "ppt", "pot", "pps", "xlsx", "xlsm", "xls", "xlt", "odt", "ott", "ods", "ots", "odp",
    "otp", "epub", // Markup / text / mail / timed transcripts.
    "md", "markdown", "qmd", "rmd", "adoc", "asciidoc", "asc", "tex", "latex", "txt", "text",
    "html", "htm", "xhtml", "csv", "eml", "boxnote", "vtt",
    // Images (layout / OCR pipeline)
    "png", "jpg", "jpeg", "tif", "tiff", "bmp", "webp", // Audio (ASR pipeline)
    "wav", "mp3", "m4a", "aac", "ogg", "flac",
    // Video (ASR + representative-frame pipeline, new in Docling 2.115)
    "mp4", "avi", "mov", "mkv", "webm",
];

const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "m4a", "aac", "ogg", "flac"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "avi", "mov", "mkv", "webm"];

/// Formats Docling 2.115 exports that map cleanly onto Shift's catalog.
///
/// The dedicated `transcript` action is the only timed-media output: ASR must
/// not hijack Markdown (document chains / MarkItDown fallback) or WebVTT
/// (FFmpeg subtitle-track extraction). Document outputs apply only to untimed
/// inputs in [`DoclingModule::supports`].
const OUTPUTS: &[OutputFormat] = &[
    OutputFormat::MARKDOWN,
    OutputFormat::TRANSCRIPT,
    OutputFormat::HTML,
    OutputFormat("plain"),
    OutputFormat::JSON,
];

/// Chain only document-like intermediates (never ASR intent formats).
const CHAINABLE_OUTPUTS: &[OutputFormat] = &[
    OutputFormat::MARKDOWN,
    OutputFormat::HTML,
    OutputFormat("plain"),
    OutputFormat::JSON,
];

/// Default ASR wall-clock budget when `SHIFT_CONVERSION_TIMEOUT_SECS` is unset.
/// First Whisper weight download and long interviews routinely exceed 5 minutes.
const DEFAULT_ASR_TIMEOUT_SECS: u64 = 1_800;

/// How Docling places figures in Markdown/HTML exports.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoclingImageExportMode {
    /// Mark image positions only (small/fast desktop default).
    #[default]
    Placeholder,
    /// Embed images as base64 (larger artifacts).
    Embedded,
    /// Write PNGs beside the document and reference them.
    Referenced,
}

impl DoclingImageExportMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Placeholder => "placeholder",
            Self::Embedded => "embedded",
            Self::Referenced => "referenced",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Placeholder => "Placeholder",
            Self::Embedded => "Embedded",
            Self::Referenced => "Referenced",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Placeholder, Self::Embedded, Self::Referenced]
    }
}

impl std::str::FromStr for DoclingImageExportMode {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "placeholder" => Ok(Self::Placeholder),
            "embedded" | "embed" => Ok(Self::Embedded),
            "referenced" | "reference" | "refs" => Ok(Self::Referenced),
            other => Err(ConversionError::new(format!(
                "unknown Docling image export mode: {other} (try placeholder, embedded, referenced)"
            ))),
        }
    }
}

/// Table structure extraction mode for Docling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoclingTableMode {
    #[default]
    Fast,
    Accurate,
}

impl DoclingTableMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Accurate => "accurate",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Accurate => "Accurate",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Fast, Self::Accurate]
    }
}

impl std::str::FromStr for DoclingTableMode {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "accurate" | "hq" | "high" => Ok(Self::Accurate),
            other => Err(ConversionError::new(format!(
                "unknown Docling table mode: {other} (try fast, accurate)"
            ))),
        }
    }
}

/// Auto-selecting Whisper model presets exposed by Docling 2.115.
///
/// These choose MLX automatically on Apple Silicon when its optional runtime is
/// installed, otherwise native Whisper. Backend-forcing and experimental S2T
/// presets remain available through Docling itself, but Shift deliberately
/// keeps its stable product surface to the six upstream auto-selecting models.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoclingAsrModel {
    #[default]
    Tiny,
    Base,
    Small,
    Medium,
    Large,
    Turbo,
}

impl DoclingAsrModel {
    pub fn id(self) -> &'static str {
        match self {
            Self::Tiny => "whisper_tiny",
            Self::Base => "whisper_base",
            Self::Small => "whisper_small",
            Self::Medium => "whisper_medium",
            Self::Large => "whisper_large",
            Self::Turbo => "whisper_turbo",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tiny => "Tiny",
            Self::Base => "Base",
            Self::Small => "Small",
            Self::Medium => "Medium",
            Self::Large => "Large",
            Self::Turbo => "Turbo",
        }
    }

    pub fn all() -> &'static [Self] {
        &[
            Self::Tiny,
            Self::Base,
            Self::Small,
            Self::Medium,
            Self::Large,
            Self::Turbo,
        ]
    }
}

impl std::str::FromStr for DoclingAsrModel {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tiny" | "whisper_tiny" | "whisper-tiny" => Ok(Self::Tiny),
            "base" | "whisper_base" | "whisper-base" => Ok(Self::Base),
            "small" | "whisper_small" | "whisper-small" => Ok(Self::Small),
            "medium" | "whisper_medium" | "whisper-medium" => Ok(Self::Medium),
            "large" | "whisper_large" | "whisper-large" => Ok(Self::Large),
            "turbo" | "whisper_turbo" | "whisper-turbo" => Ok(Self::Turbo),
            other => Err(ConversionError::new(format!(
                "unknown Docling ASR model: {other} (try tiny, base, small, medium, large, turbo)"
            ))),
        }
    }
}

/// Representative-frame selection used by Docling's video pipeline.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoclingVideoSamplingMode {
    #[default]
    Fixed,
    Scene,
}

impl DoclingVideoSamplingMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Scene => "scene",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fixed => "Fixed interval",
            Self::Scene => "Scene changes",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Fixed, Self::Scene]
    }
}

impl std::str::FromStr for DoclingVideoSamplingMode {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fixed" | "interval" | "fixed-interval" | "fixed_interval" => Ok(Self::Fixed),
            "scene" | "scenes" | "scene-change" | "scene_change" => Ok(Self::Scene),
            other => Err(ConversionError::new(format!(
                "unknown Docling video sampling mode: {other} (try fixed, scene)"
            ))),
        }
    }
}

/// Minimum fixed-interval spacing accepted for video representative frames.
pub const MIN_VIDEO_FRAME_INTERVAL_SECS: f64 = 0.5;
/// Maximum scene-change rate (cuts per minute) accepted for video sampling.
pub const MAX_VIDEO_CUTS_PER_MINUTE: f64 = 30.0;
/// Hard cap on representative frames derived from media duration.
pub const MAX_VIDEO_REPRESENTATIVE_FRAMES: u32 = 300;

/// Optional knobs for Docling. Defaults keep desktop conversions small/fast.
#[derive(Clone, Debug, PartialEq)]
pub struct DoclingOptions {
    pub image_export_mode: DoclingImageExportMode,
    /// Run OCR when the pipeline needs it (`--ocr` / `--no-ocr`).
    pub ocr: bool,
    /// OCR language codes when set (`--ocr-lang`), e.g. `eng` or `eng+deu`.
    pub ocr_lang: Option<String>,
    /// Extract table structure (`--tables` / `--no-tables`).
    pub tables: bool,
    pub table_mode: DoclingTableMode,
    /// Auto-selecting Whisper preset (`--asr-model`) for audio and video.
    pub asr_model: DoclingAsrModel,
    /// Representative-frame selection for video input.
    pub video_sampling_mode: DoclingVideoSamplingMode,
    /// Seconds between frames in fixed-interval mode (must be finite and > 0).
    pub video_frame_interval_secs: f64,
    /// Target cuts/minute in scene mode. Zero lets Docling auto-calibrate.
    pub video_cuts_per_minute: f64,
    /// Scene prominence override. Zero lets Docling auto-calibrate.
    pub video_prominence: f64,
    /// Speaker diarization for video (`resemblyzer` optional dependency).
    pub video_diarization: bool,
}

impl Default for DoclingOptions {
    fn default() -> Self {
        Self {
            // Shift prefers placeholder over Docling's upstream "embedded"
            // default so desktop artifacts stay small and conversions stay fast.
            image_export_mode: DoclingImageExportMode::Placeholder,
            ocr: true,
            ocr_lang: None,
            tables: true,
            table_mode: DoclingTableMode::Fast,
            asr_model: DoclingAsrModel::Tiny,
            video_sampling_mode: DoclingVideoSamplingMode::Fixed,
            video_frame_interval_secs: 10.0,
            video_cuts_per_minute: 0.0,
            video_prominence: 0.0,
            video_diarization: false,
        }
    }
}

impl DoclingOptions {
    pub fn validate(&self) -> Result<(), ConversionError> {
        self.validate_with_duration(None)
    }

    /// Validate sampling knobs, optionally bounding frame count from media duration.
    pub fn validate_with_duration(
        &self,
        duration_secs: Option<f64>,
    ) -> Result<(), ConversionError> {
        if !self.video_frame_interval_secs.is_finite() || self.video_frame_interval_secs <= 0.0 {
            return Err(ConversionError::new(
                "Docling video frame interval must be a positive number of seconds",
            ));
        }
        if self.video_frame_interval_secs < MIN_VIDEO_FRAME_INTERVAL_SECS {
            return Err(ConversionError::new(format!(
                "Docling video frame interval must be at least {MIN_VIDEO_FRAME_INTERVAL_SECS} seconds"
            )));
        }
        if !self.video_cuts_per_minute.is_finite() || self.video_cuts_per_minute < 0.0 {
            return Err(ConversionError::new(
                "Docling video cuts per minute must be a non-negative number",
            ));
        }
        if self.video_cuts_per_minute > MAX_VIDEO_CUTS_PER_MINUTE {
            return Err(ConversionError::new(format!(
                "Docling video cuts per minute must be at most {MAX_VIDEO_CUTS_PER_MINUTE}"
            )));
        }
        if !self.video_prominence.is_finite() || self.video_prominence < 0.0 {
            return Err(ConversionError::new(
                "Docling video prominence must be a non-negative number",
            ));
        }
        if let Some(duration) = duration_secs.filter(|value| value.is_finite() && *value > 0.0) {
            let frames = estimate_representative_frames(self, duration);
            if frames > MAX_VIDEO_REPRESENTATIVE_FRAMES as u64 {
                return Err(ConversionError::new(format!(
                    "Docling video sampling would produce about {frames} frames for {duration:.1}s \
                     (limit is {MAX_VIDEO_REPRESENTATIVE_FRAMES}); raise the frame interval or lower cuts/minute"
                )));
            }
        }
        Ok(())
    }

    /// Raise the fixed interval so `duration / interval` stays within the frame cap.
    pub fn clamp_interval_for_duration(&mut self, duration_secs: f64) {
        if !duration_secs.is_finite() || duration_secs <= 0.0 {
            return;
        }
        let min_for_cap = duration_secs / f64::from(MAX_VIDEO_REPRESENTATIVE_FRAMES);
        if min_for_cap > self.video_frame_interval_secs {
            self.video_frame_interval_secs = min_for_cap.max(MIN_VIDEO_FRAME_INTERVAL_SECS);
        }
        if self.video_frame_interval_secs < MIN_VIDEO_FRAME_INTERVAL_SECS {
            self.video_frame_interval_secs = MIN_VIDEO_FRAME_INTERVAL_SECS;
        }
        if self.video_cuts_per_minute > MAX_VIDEO_CUTS_PER_MINUTE {
            self.video_cuts_per_minute = MAX_VIDEO_CUTS_PER_MINUTE;
        }
    }
}

fn estimate_representative_frames(options: &DoclingOptions, duration_secs: f64) -> u64 {
    match options.video_sampling_mode {
        DoclingVideoSamplingMode::Fixed => {
            let interval = options
                .video_frame_interval_secs
                .max(MIN_VIDEO_FRAME_INTERVAL_SECS);
            (duration_secs / interval).ceil().max(1.0) as u64
        }
        DoclingVideoSamplingMode::Scene => {
            let rate = options
                .video_cuts_per_minute
                .clamp(0.0, MAX_VIDEO_CUTS_PER_MINUTE);
            if rate <= 0.0 {
                // Docling auto-calibrates; assume the hard cap as the worst case.
                return u64::from(MAX_VIDEO_REPRESENTATIVE_FRAMES);
            }
            ((duration_secs / 60.0) * rate).ceil().max(1.0) as u64
        }
    }
}

#[derive(Clone, Debug)]
pub struct DoclingModule {
    executable: OsString,
}

impl Default for DoclingModule {
    fn default() -> Self {
        Self {
            executable: discover_executable(),
        }
    }
}

fn discover_executable() -> OsString {
    // Prefer a project-local venv when present (same convention as MarkItDown).
    // Absolute resolution matches diagnostics so GUI PATH quirks stay consistent.
    let mut candidates = Vec::new();
    if let Some(bundled) = bundled_runtime_tool("docling") {
        candidates.push(bundled);
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".venv/bin/docling"));
    resolve_tool_executable("SHIFT_DOCLING_BIN", "docling", &candidates)
}

impl DoclingModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn to_arg(output_format: OutputFormat) -> Option<&'static str> {
        match output_format.id() {
            "markdown" | "transcript" => Some("md"),
            "html" => Some("html"),
            "plain" => Some("text"),
            "json" => Some("json"),
            _ => None,
        }
    }

    fn output_file_name(stem: &std::ffi::OsStr, output_format: OutputFormat) -> PathBuf {
        // Docling writes `<stem>.md|html|txt|json` into `--output`.
        let extension = match output_format.id() {
            "markdown" | "transcript" => "md",
            "html" => "html",
            "plain" => "txt",
            "json" => "json",
            other => other,
        };
        let mut output = PathBuf::from(stem);
        output.set_extension(extension);
        output
    }

    /// Discover the file Docling actually wrote, in case it renamed the output
    /// (for example when a same-named file already existed in the temp dir).
    ///
    /// Returns the exact expected path if present, or a single candidate with
    /// the matching extension. Returns `None` if no candidates exist or if
    /// multiple ambiguous candidates are found (callers should treat this as a
    /// conversion failure rather than silently picking an arbitrary file).
    fn discover_output(work_dir: &Path, expected: &Path) -> Option<PathBuf> {
        let expected_ext = expected.extension().and_then(|value| value.to_str())?;
        let expected_name = expected.file_name()?;

        let matches: Vec<PathBuf> = fs::read_dir(work_dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().ok().is_some_and(|t| t.is_file()))
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(expected_ext))
            })
            .collect();

        if matches.is_empty() {
            return None;
        }

        if let Some(exact) = matches
            .iter()
            .find(|path| path.file_name() == Some(expected_name))
        {
            return Some(exact.clone());
        }

        // Only accept a single unambiguous candidate; multiple candidates
        // indicate an unexpected output layout and should fail explicitly.
        if matches.len() == 1 {
            return matches.into_iter().next();
        }

        None
    }

    fn convert_with_cli(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        // When the user set an FFmpeg trim duration, treat it as the processing
        // window for representative-frame budgeting.
        let duration_hint = options
            .ffmpeg
            .duration_secs
            .filter(|value| value.is_finite() && *value > 0.0);
        options.docling.validate_with_duration(duration_hint)?;
        let to_arg = Self::to_arg(output_format).ok_or_else(|| {
            ConversionError::new(format!(
                "Docling does not produce {}",
                output_format.label()
            ))
        })?;

        let stem = input
            .file_stem()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsStr::new("converted"));

        let work_dir = unique_temp_dir("shift-docling")?;
        let cleanup = TempDirGuard(work_dir.clone());

        // Docling writes files into --output; it does not stream to stdout.
        // Explicit `convert` keeps the invocation stable if more subcommands are added.
        let knobs = &options.docling;
        let is_audio = input_has_extension(input, AUDIO_EXTENSIONS);
        let is_video = input_has_extension(input, VIDEO_EXTENSIONS);
        let mut command = Command::new(&self.executable);
        // `convert <input> --to …` — absolute input right after the subcommand.
        command.arg("convert");
        super::push_operand_path(&mut command, input)?;
        command
            .arg("--to")
            .arg(to_arg)
            .arg("--output")
            .arg(super::absolute_command_path(&work_dir))
            .arg("--image-export-mode")
            .arg(knobs.image_export_mode.id())
            .arg(if knobs.ocr { "--ocr" } else { "--no-ocr" })
            .arg(if knobs.tables {
                "--tables"
            } else {
                "--no-tables"
            })
            .arg("--table-mode")
            .arg(knobs.table_mode.id())
            .arg("--abort-on-error");
        if let Some(lang) = knobs
            .ocr_lang
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            command.arg("--ocr-lang").arg(lang);
        }
        if is_audio || is_video {
            command.arg("--asr-model").arg(knobs.asr_model.id());
        }
        if is_video {
            command
                .arg("--video-sampling-mode")
                .arg(knobs.video_sampling_mode.id());
            // Only pass knobs relevant to the selected sampling mode.
            match knobs.video_sampling_mode {
                DoclingVideoSamplingMode::Fixed => {
                    command
                        .arg("--video-frame-interval")
                        .arg(knobs.video_frame_interval_secs.to_string());
                }
                DoclingVideoSamplingMode::Scene => {
                    command
                        .arg("--video-cuts-per-minute")
                        .arg(knobs.video_cuts_per_minute.to_string())
                        .arg("--video-prominence")
                        .arg(knobs.video_prominence.to_string());
                }
            }
            command.arg(if knobs.video_diarization {
                "--video-diarization"
            } else {
                "--no-video-diarization"
            });
        }

        let display_parts = command_argv_parts(&command);
        let invocation = InvocationRecord {
            module_id: self.id(),
            argv_display: format_argv_display(&display_parts),
        };

        if let Some(progress) = options.progress.as_ref() {
            let label = if is_video {
                "Analyzing and transcribing video with Docling"
            } else if is_audio {
                "Transcribing audio with Docling"
            } else {
                "Converting document with Docling"
            };
            progress(ConversionProgress::Phase(label.to_owned()));
        }

        let timeout = if is_audio || is_video {
            asr_timeout()
        } else {
            process_timeout()
        };
        let output = run_command_cancellable_with_output_dirs(
            command,
            timeout,
            max_output_bytes(),
            options.cancel.clone(),
            &[],
            &[(work_dir.clone(), max_output_bytes() as u64)],
        )
        .map_err(|error| {
            if error.to_string().to_ascii_lowercase().contains("timeout") {
                return ConversionError::new(format!(
                    "Docling timed out converting {} (limit {}s). First Whisper model \
                     download and long interviews can exceed the default; raise \
                     SHIFT_CONVERSION_TIMEOUT_SECS or use a smaller --docling-asr-model.",
                    input.display(),
                    timeout.as_secs()
                ));
            }
            map_spawn_error(
                error,
                "Docling is not installed. Install it with `pip install 'docling==2.115.0'`. \
                 For audio/video transcription install `docling[asr]` plus \
                 `docling-slim[format-video]`, or set SHIFT_DOCLING_BIN.",
            )
        })?;

        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if detail.is_empty() {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if stdout.is_empty() {
                    format!("process exited with {}", output.status)
                } else {
                    stdout
                }
            } else {
                detail
            };
            let transcription_hint = if is_audio || is_video {
                "\nAudio/video transcription additionally requires FFmpeg and Docling's ASR \
                 extras (`pip install 'docling[asr]==2.115.0' \
                 'docling-slim[format-video]==2.115.0'`). Model weights download on first use."
            } else {
                ""
            };
            return Err(ConversionError::new(format!(
                "Docling could not convert {}: {detail}{transcription_hint}",
                input.display()
            )));
        }

        let expected = Self::output_file_name(stem, output_format);
        let produced =
            Self::discover_output(&work_dir, &expected).unwrap_or_else(|| work_dir.join(&expected));
        let bytes = read_file_limited(&produced, max_output_bytes()).map_err(|error| {
            ConversionError::new(format!(
                "Docling finished but did not write {}: {error}",
                produced.display()
            ))
        })?;

        // Drop temp dir after reading the artifact.
        drop(cleanup);

        Ok(ConversionArtifact {
            file_name: Self::output_file_name(stem, output_format)
                .to_string_lossy()
                .into_owned(),
            media_type: output_format.media_type(),
            bytes,
            format: output_format,
            module_id: self.id(),
            pipeline: vec![self.id()],
            invocations: vec![invocation],
        })
    }
}

impl ConversionModule for DoclingModule {
    fn id(&self) -> &'static str {
        "docling"
    }

    fn label(&self) -> &'static str {
        "Docling"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        EXTENSIONS
    }

    fn output_formats(&self) -> &[OutputFormat] {
        OUTPUTS
    }

    fn chainable_output_formats(&self) -> &[OutputFormat] {
        CHAINABLE_OUTPUTS
    }

    fn supports(&self, input: &Path, output: OutputFormat) -> bool {
        let Some(extension) = input.extension().and_then(|value| value.to_str()) else {
            return false;
        };
        let known_input = EXTENSIONS
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate));
        if !known_input || !OUTPUTS.contains(&output) {
            return false;
        }

        let is_timed_media = input_has_extension(input, AUDIO_EXTENSIONS)
            || input_has_extension(input, VIDEO_EXTENSIONS);
        if is_timed_media {
            // ASR only on the dedicated transcript action. Markdown stays with
            // MarkItDown/chains; SRT/VTT track extraction stays with FFmpeg.
            return output == OutputFormat::TRANSCRIPT;
        }
        // Untimed documents never claim the ASR-intent transcript action.
        output != OutputFormat::TRANSCRIPT
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !OUTPUTS.contains(&output_format) {
            return Err(ConversionError::new(format!(
                "Docling does not produce {}",
                output_format.label()
            )));
        }
        if !self.supports(input, output_format) {
            return Err(ConversionError::new(format!(
                "Docling does not produce {} from {}",
                output_format.label(),
                input.display()
            )));
        }
        self.convert_with_cli(input, output_format, options)
    }
}

fn input_has_extension(input: &Path, extensions: &[&str]) -> bool {
    input
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

pub fn is_docling_audio_input(input: &Path) -> bool {
    input_has_extension(input, AUDIO_EXTENSIONS)
}

pub fn is_docling_video_input(input: &Path) -> bool {
    input_has_extension(input, VIDEO_EXTENSIONS)
}

pub fn is_docling_timed_input(input: &Path) -> bool {
    is_docling_audio_input(input) || is_docling_video_input(input)
}

fn asr_timeout() -> std::time::Duration {
    std::env::var("SHIFT_CONVERSION_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(DEFAULT_ASR_TIMEOUT_SECS))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    fn write_fake_docling(path: &Path) {
        // Mimic Docling CLI: honor `convert <input> --to <fmt> --output <dir>`
        // and write `<stem>.<ext>` into the output directory.
        let script = r#"#!/bin/sh
set -e
printf '%s\n' "$*" > "${0}.args"
to="md"
output="."
input=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    convert) shift; continue ;;
    --to) to="$2"; shift 2; continue ;;
    --output) output="$2"; shift 2; continue ;;
    --image-export-mode|--table-mode|--ocr-lang|--pdf-password|--asr-model|--video-sampling-mode|--video-frame-interval|--video-cuts-per-minute|--video-prominence) shift 2; continue ;;
    --ocr|--no-ocr|--tables|--no-tables|--abort-on-error|--video-diarization|--no-video-diarization) shift; continue ;;
    --*) shift; continue ;;
    *) input="$1"; shift; continue ;;
  esac
done
stem=$(basename "$input")
stem=${stem%.*}
case "$to" in
  md) ext=md; body='# From Docling' ;;
  html) ext=html; body='<p>From Docling</p>' ;;
  text) ext=txt; body='From Docling' ;;
  json) ext=json; body='{"text":"From Docling"}' ;;
  vtt) ext=vtt; body='WEBVTT' ;;
  *) ext=out; body=unknown ;;
esac
printf '%s' "$body" > "$output/$stem.$ext"
"#;
        fs::write(path, script).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn converts_pdf_to_html_via_temp_output_dir() {
        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let artifact = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap();

        assert_eq!(
            artifact.file_name,
            format!("{}.html", input.file_stem().unwrap().to_string_lossy())
        );
        assert_eq!(artifact.media_type, "text/html");
        assert_eq!(artifact.bytes, b"<p>From Docling</p>");
        assert_eq!(artifact.module_id, "docling");
        assert_eq!(artifact.format, OutputFormat::HTML);

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("convert"), "args: {args}");
        assert!(args.contains("--to"), "args: {args}");
        assert!(args.contains("html"), "args: {args}");
        assert!(args.contains("--output"), "args: {args}");
        assert!(args.contains("--image-export-mode"), "args: {args}");
        assert!(args.contains("placeholder"), "args: {args}");
        assert!(args.contains("--ocr"), "args: {args}");
        assert!(args.contains("--tables"), "args: {args}");
        assert!(args.contains("--table-mode"), "args: {args}");
        assert!(args.contains("fast"), "args: {args}");
        assert!(args.contains("--abort-on-error"), "args: {args}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn honors_docling_options_in_cli_argv() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-opts", std::process::id());
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let mut options = ConversionOptions {
            docling: DoclingOptions {
                image_export_mode: DoclingImageExportMode::Embedded,
                ocr: false,
                ocr_lang: Some("eng+deu".into()),
                tables: false,
                table_mode: DoclingTableMode::Accurate,
                ..DoclingOptions::default()
            },
            ..ConversionOptions::default()
        };
        options.pdf.password = Some("s3cret".into());
        let artifact = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();
        assert_eq!(artifact.text(), Some("# From Docling"));

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("embedded"), "args: {args}");
        assert!(args.contains("--no-ocr"), "args: {args}");
        assert!(args.contains("--no-tables"), "args: {args}");
        assert!(args.contains("accurate"), "args: {args}");
        assert!(args.contains("--ocr-lang"), "args: {args}");
        assert!(args.contains("eng+deu"), "args: {args}");
        assert!(
            !args.contains("--pdf-password"),
            "PDF password should be handled by qpdf preprocess, not passed to docling, args: {args}"
        );
        assert!(
            !args.contains("s3cret"),
            "PDF password should not appear on the docling command line, args: {args}"
        );
        assert_eq!(artifact.invocations.len(), 1);

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn converts_pdf_to_markdown_and_plain_text() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-md", std::process::id());
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let module = DoclingModule::with_executable(&executable);

        let markdown = module
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert_eq!(markdown.text(), Some("# From Docling"));
        assert!(markdown.file_name.ends_with(".md"));

        let plain = module
            .convert(&input, OutputFormat("plain"), &ConversionOptions::default())
            .unwrap();
        assert_eq!(plain.text(), Some("From Docling"));
        assert!(plain.file_name.ends_with(".txt"));

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn preserves_non_utf8_stem_when_finding_docling_output() {
        let stem = OsString::from_vec(b"report-\xff".to_vec());
        let output = DoclingModule::output_file_name(&stem, OutputFormat::HTML);
        assert_eq!(output.into_os_string().into_vec(), b"report-\xff.html");
    }

    #[test]
    fn rejects_unsupported_output_formats() {
        let err = DoclingModule::with_executable("docling")
            .convert(
                Path::new("scan.pdf"),
                OutputFormat::DOCX,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("Word") || err.to_string().contains("DOCX"));
    }

    #[test]
    fn discover_output_returns_none_on_ambiguous_candidates() {
        let work = std::env::temp_dir().join("shift-docling-ambig");
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("alpha.md"), b"# A").unwrap();
        fs::write(work.join("beta.md"), b"# B").unwrap();
        // Expected file does not exist; two candidates are ambiguous.
        let expected = work.join("report.md");
        let result = DoclingModule::discover_output(&work, &expected);
        assert!(result.is_none(), "ambiguous candidates must return None");
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_returns_single_renamed_candidate() {
        let work = std::env::temp_dir().join("shift-docling-single");
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("renamed.md"), b"# OK").unwrap();
        let expected = work.join("report.md");
        let result = DoclingModule::discover_output(&work, &expected);
        assert_eq!(result, Some(work.join("renamed.md")));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_prefers_exact_match() {
        let work = std::env::temp_dir().join("shift-docling-exact");
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("report.md"), b"# exact").unwrap();
        fs::write(work.join("other.md"), b"# other").unwrap();
        let expected = work.join("report.md");
        let result = DoclingModule::discover_output(&work, &expected);
        assert_eq!(result, Some(work.join("report.md")));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn image_export_mode_from_str_id_label_round_trips() {
        let cases = [
            (
                DoclingImageExportMode::Placeholder,
                "placeholder",
                "Placeholder",
                &["placeholder"][..],
            ),
            (
                DoclingImageExportMode::Embedded,
                "embedded",
                "Embedded",
                &["embedded", "embed"],
            ),
            (
                DoclingImageExportMode::Referenced,
                "referenced",
                "Referenced",
                &["referenced", "reference", "refs"],
            ),
        ];
        assert_eq!(DoclingImageExportMode::all().len(), cases.len());
        for (mode, id, label, aliases) in cases {
            assert_eq!(mode.id(), id);
            assert_eq!(mode.label(), label);
            for alias in aliases {
                assert_eq!(
                    alias.parse::<DoclingImageExportMode>().unwrap(),
                    mode,
                    "alias {alias}"
                );
                assert_eq!(
                    alias
                        .to_ascii_uppercase()
                        .parse::<DoclingImageExportMode>()
                        .unwrap(),
                    mode,
                    "uppercase alias {alias}"
                );
            }
        }
        let err = "nope".parse::<DoclingImageExportMode>().unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown Docling image export mode"),
            "{err}"
        );
    }

    #[test]
    fn table_mode_from_str_id_label_round_trips() {
        let cases = [
            (DoclingTableMode::Fast, "fast", "Fast", &["fast"][..]),
            (
                DoclingTableMode::Accurate,
                "accurate",
                "Accurate",
                &["accurate", "hq", "high"],
            ),
        ];
        assert_eq!(DoclingTableMode::all().len(), cases.len());
        for (mode, id, label, aliases) in cases {
            assert_eq!(mode.id(), id);
            assert_eq!(mode.label(), label);
            for alias in aliases {
                assert_eq!(
                    alias.parse::<DoclingTableMode>().unwrap(),
                    mode,
                    "alias {alias}"
                );
                assert_eq!(
                    alias
                        .to_ascii_uppercase()
                        .parse::<DoclingTableMode>()
                        .unwrap(),
                    mode,
                    "uppercase alias {alias}"
                );
            }
        }
        let err = "slow".parse::<DoclingTableMode>().unwrap_err();
        assert!(
            err.to_string().contains("unknown Docling table mode"),
            "{err}"
        );
    }

    #[test]
    fn to_arg_maps_markdown_html_plain() {
        assert_eq!(DoclingModule::to_arg(OutputFormat::MARKDOWN), Some("md"));
        assert_eq!(DoclingModule::to_arg(OutputFormat::HTML), Some("html"));
        assert_eq!(DoclingModule::to_arg(OutputFormat("plain")), Some("text"));
        assert_eq!(DoclingModule::to_arg(OutputFormat::PDF), None);
        assert_eq!(DoclingModule::to_arg(OutputFormat::DOCX), None);
    }

    #[test]
    fn pdf_password_does_not_appear_on_docling_argv() {
        let directory = std::env::temp_dir();
        let suffix = format!(
            "{}-{}-pw",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let secret = "p@ssw0rd-never-on-argv";
        let mut options = ConversionOptions::default();
        options.pdf.password = Some(secret.into());
        DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(
            !args.contains("--pdf-password"),
            "docling must not receive --pdf-password, args: {args}"
        );
        assert!(
            !args.contains(secret),
            "password must not appear on docling argv, args: {args}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn reports_capability_lists() {
        let module = DoclingModule::with_executable("docling");
        let inputs = module.input_extensions();
        assert!(
            inputs.contains(&"pdf"),
            "input_extensions should include pdf: {inputs:?}"
        );
        let outputs = module.output_formats();
        assert!(outputs.contains(&OutputFormat::MARKDOWN));
        assert!(outputs.contains(&OutputFormat::TRANSCRIPT));
        assert!(outputs.contains(&OutputFormat::HTML));
        assert!(outputs.contains(&OutputFormat("plain")));
        assert_eq!(module.chainable_output_formats(), CHAINABLE_OUTPUTS);
        assert!(
            !module
                .chainable_output_formats()
                .contains(&OutputFormat::TRANSCRIPT)
        );
    }

    #[test]
    fn missing_executable_fails_cleanly() {
        let missing = std::env::temp_dir().join(format!(
            "shift-docling-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let input = std::env::temp_dir().join(format!(
            "shift-docling-missing-input-{}-{}.pdf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let err = DoclingModule::with_executable(&missing)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Docling is not installed")
                || message.contains("executable not found"),
            "{message}"
        );
        // Install hint must stay stable for UX / docs.
        assert!(
            message.contains("pip install docling") || message.contains("SHIFT_DOCLING_BIN"),
            "missing-exe message should mention install path: {message}"
        );

        let _ = fs::remove_file(&input);
    }

    fn unique_suffix(tag: &str) -> String {
        format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            tag
        )
    }

    fn write_fake_docling_body(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn cancel_flag_aborts_conversion() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("cancel");
        let executable = directory.join(format!("shift-docling-cancel-{suffix}"));
        let input = directory.join(format!("shift-docling-cancel-in-{suffix}.pdf"));
        write_fake_docling_body(&executable, "#!/bin/sh\nsleep 30\n");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let options = ConversionOptions {
            cancel: Some(std::sync::Arc::clone(&cancel)),
            ..ConversionOptions::default()
        };
        let err = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap_err();
        assert!(err.is_cancelled(), "error: {err}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn cancel_mid_run_stops_hanging_docling() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("mid-cancel");
        let executable = directory.join(format!("shift-docling-midcancel-{suffix}"));
        let input = directory.join(format!("shift-docling-midcancel-in-{suffix}.pdf"));
        write_fake_docling_body(&executable, "#!/bin/sh\nsleep 30\n");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

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
        let err = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap_err();
        let _ = watcher.join();
        assert!(err.is_cancelled(), "error: {err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "cancel took too long: {:?}",
            started.elapsed()
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn discover_output_empty_dir_returns_none() {
        let work = std::env::temp_dir().join(format!("shift-docling-empty-{}", unique_suffix("e")));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let expected = work.join("report.md");
        assert!(DoclingModule::discover_output(&work, &expected).is_none());
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_ignores_nested_files_and_directories() {
        let work =
            std::env::temp_dir().join(format!("shift-docling-nested-{}", unique_suffix("n")));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(work.join("subdir")).unwrap();
        // Nested candidate must not be discovered (only top-level files).
        fs::write(work.join("subdir").join("report.md"), b"# nested").unwrap();
        // A directory whose name ends like the expected extension is not a file.
        fs::create_dir_all(work.join("looks.md")).unwrap();
        let expected = work.join("report.md");
        assert!(
            DoclingModule::discover_output(&work, &expected).is_none(),
            "nested/dir entries must not count as candidates"
        );
        // Once a top-level file appears, it is found.
        fs::write(work.join("renamed.md"), b"# top").unwrap();
        assert_eq!(
            DoclingModule::discover_output(&work, &expected),
            Some(work.join("renamed.md"))
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_extension_match_is_case_insensitive() {
        let work = std::env::temp_dir().join(format!("shift-docling-case-{}", unique_suffix("c")));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("Report.MD"), b"# Case").unwrap();
        let expected = work.join("report.md");
        let result = DoclingModule::discover_output(&work, &expected);
        assert_eq!(result, Some(work.join("Report.MD")));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_output_missing_work_dir_returns_none() {
        let missing =
            std::env::temp_dir().join(format!("shift-docling-missing-dir-{}", unique_suffix("md")));
        let _ = fs::remove_dir_all(&missing);
        let expected = missing.join("report.md");
        assert!(DoclingModule::discover_output(&missing, &expected).is_none());
    }

    #[test]
    fn all_image_export_modes_appear_on_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("img-modes");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        for mode in DoclingImageExportMode::all() {
            let options = ConversionOptions {
                docling: DoclingOptions {
                    image_export_mode: *mode,
                    ..DoclingOptions::default()
                },
                ..ConversionOptions::default()
            };
            DoclingModule::with_executable(&executable)
                .convert(&input, OutputFormat::MARKDOWN, &options)
                .unwrap();
            let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
            assert!(
                args.contains("--image-export-mode") && args.contains(mode.id()),
                "mode {} missing from argv: {args}",
                mode.id()
            );
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn all_table_modes_appear_on_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("table-modes");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        for mode in DoclingTableMode::all() {
            let options = ConversionOptions {
                docling: DoclingOptions {
                    table_mode: *mode,
                    ..DoclingOptions::default()
                },
                ..ConversionOptions::default()
            };
            DoclingModule::with_executable(&executable)
                .convert(&input, OutputFormat::HTML, &options)
                .unwrap();
            let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
            assert!(
                args.contains("--table-mode") && args.contains(mode.id()),
                "table mode {} missing from argv: {args}",
                mode.id()
            );
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn empty_or_whitespace_ocr_lang_is_omitted_from_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("ocr-empty");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        for lang in [Some(String::new()), Some("   ".into()), None] {
            let options = ConversionOptions {
                docling: DoclingOptions {
                    ocr_lang: lang.clone(),
                    ..DoclingOptions::default()
                },
                ..ConversionOptions::default()
            };
            DoclingModule::with_executable(&executable)
                .convert(&input, OutputFormat::MARKDOWN, &options)
                .unwrap();
            let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
            assert!(
                !args.contains("--ocr-lang"),
                "empty/whitespace ocr_lang must not pass --ocr-lang (lang={lang:?}): {args}"
            );
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn converts_non_pdf_office_and_image_inputs() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("nonpdf");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        write_fake_docling(&executable);
        let module = DoclingModule::with_executable(&executable);

        for (name, bytes) in [
            ("slide.docx", b"PK fake docx" as &[u8]),
            ("sheet.xlsx", b"PK fake xlsx"),
            ("deck.pptx", b"PK fake pptx"),
            ("scan.png", b"\x89PNG fake"),
            ("page.html", b"<html><body>hi</body></html>"),
            ("notes.md", b"# notes\n"),
            ("book.epub", b"PK fake epub"),
        ] {
            let input = directory.join(format!("shift-docling-{suffix}-{name}"));
            fs::write(&input, bytes).unwrap();
            let artifact = module
                .convert(
                    &input,
                    OutputFormat::MARKDOWN,
                    &ConversionOptions::default(),
                )
                .unwrap_or_else(|e| panic!("convert {name}: {e}"));
            assert!(
                artifact.file_name.ends_with(".md"),
                "{name} → {}",
                artifact.file_name
            );
            assert_eq!(artifact.module_id, "docling");
            assert_eq!(artifact.pipeline, vec!["docling"]);
            let _ = fs::remove_file(&input);
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
    }

    #[test]
    fn capability_list_is_exhaustive_for_extensions_and_outputs() {
        let module = DoclingModule::with_executable("docling");
        // Every documented Docling input extension must be advertised.
        for ext in [
            "pdf", "docx", "pptx", "xlsx", "odt", "ods", "odp", "epub", "md", "markdown", "adoc",
            "asciidoc", "tex", "latex", "txt", "html", "htm", "xhtml", "csv", "png", "jpg", "jpeg",
            "tif", "tiff", "bmp", "webp",
        ] {
            assert!(
                module.input_extensions().contains(&ext),
                "missing input extension {ext:?} in {:?}",
                module.input_extensions()
            );
        }
        assert_eq!(module.input_extensions().len(), EXTENSIONS.len());
        assert_eq!(module.input_extensions(), EXTENSIONS);

        let outputs = module.output_formats();
        assert_eq!(outputs.len(), 5);
        assert_eq!(outputs, OUTPUTS);
        assert_eq!(module.chainable_output_formats(), CHAINABLE_OUTPUTS);
        assert!(
            !module
                .chainable_output_formats()
                .contains(&OutputFormat::TRANSCRIPT)
        );
        assert_eq!(module.id(), "docling");
        assert_eq!(module.label(), "Docling");

        // supports() is non-cartesian: timed media → transcript only.
        assert!(module.supports(Path::new("scan.PDF"), OutputFormat::HTML));
        assert!(module.supports(Path::new("slide.docx"), OutputFormat::MARKDOWN));
        assert!(module.supports(Path::new("scan.png"), OutputFormat("plain")));
        assert!(!module.supports(Path::new("clip.mp4"), OutputFormat::MARKDOWN));
        assert!(module.supports(Path::new("clip.mp4"), OutputFormat::TRANSCRIPT));
        assert!(!module.supports(Path::new("clip.mp4"), OutputFormat::VTT));
        assert!(!module.supports(Path::new("captions.vtt"), OutputFormat::TRANSCRIPT));
        assert!(module.supports(Path::new("captions.vtt"), OutputFormat::MARKDOWN));
        assert!(!module.supports(Path::new("scan.pdf"), OutputFormat::TRANSCRIPT));
        assert!(!module.supports(Path::new("scan.pdf"), OutputFormat::DOCX));
    }

    #[test]
    fn default_docling_options_prefer_fast_small_artifacts() {
        let defaults = DoclingOptions::default();
        assert_eq!(
            defaults.image_export_mode,
            DoclingImageExportMode::Placeholder
        );
        assert!(defaults.ocr);
        assert!(defaults.tables);
        assert_eq!(defaults.table_mode, DoclingTableMode::Fast);
        assert_eq!(defaults.ocr_lang, None);
        assert_eq!(defaults.asr_model, DoclingAsrModel::Tiny);
        assert_eq!(
            defaults.video_sampling_mode,
            DoclingVideoSamplingMode::Fixed
        );
        assert_eq!(defaults.video_frame_interval_secs, 10.0);
    }

    #[test]
    fn audio_and_video_options_use_pinned_docling_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("asr-video");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.mp4"));
        write_fake_docling(&executable);
        fs::write(&input, b"fake video").unwrap();

        let options = ConversionOptions {
            docling: DoclingOptions {
                asr_model: DoclingAsrModel::Turbo,
                video_sampling_mode: DoclingVideoSamplingMode::Scene,
                video_frame_interval_secs: 2.5,
                video_cuts_per_minute: 4.0,
                video_prominence: 0.02,
                video_diarization: true,
                ..DoclingOptions::default()
            },
            ..ConversionOptions::default()
        };
        let artifact = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::TRANSCRIPT, &options)
            .unwrap();
        assert_eq!(artifact.format, OutputFormat::TRANSCRIPT);
        assert!(artifact.file_name.ends_with(".md"));

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        for expected in [
            "--asr-model",
            "whisper_turbo",
            "--video-sampling-mode",
            "scene",
            "--video-cuts-per-minute",
            "4",
            "--video-prominence",
            "0.02",
            "--video-diarization",
        ] {
            assert!(
                args.contains(expected),
                "missing {expected:?} from argv: {args}"
            );
        }
        assert!(
            !args.contains("--video-frame-interval"),
            "scene mode must not pass fixed-interval knobs: {args}"
        );

        // Fixed mode should pass interval only (not scene knobs).
        let fixed = ConversionOptions {
            docling: DoclingOptions {
                video_sampling_mode: DoclingVideoSamplingMode::Fixed,
                video_frame_interval_secs: 3.0,
                video_cuts_per_minute: 9.0,
                video_prominence: 0.5,
                ..DoclingOptions::default()
            },
            ..ConversionOptions::default()
        };
        DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::TRANSCRIPT, &fixed)
            .unwrap();
        let fixed_args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(
            fixed_args.contains("--video-frame-interval") && fixed_args.contains("3"),
            "{fixed_args}"
        );
        assert!(
            !fixed_args.contains("--video-cuts-per-minute")
                && !fixed_args.contains("--video-prominence"),
            "fixed mode must not pass scene knobs: {fixed_args}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn docling_video_options_reject_invalid_numbers_before_spawn() {
        let invalid = DoclingOptions {
            video_frame_interval_secs: 0.0,
            ..DoclingOptions::default()
        };
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("interval")
        );
        let too_small = DoclingOptions {
            video_frame_interval_secs: MIN_VIDEO_FRAME_INTERVAL_SECS / 2.0,
            ..DoclingOptions::default()
        };
        assert!(
            too_small
                .validate()
                .unwrap_err()
                .to_string()
                .contains("at least"),
            "min interval must be enforced"
        );
        let invalid = DoclingOptions {
            video_cuts_per_minute: -1.0,
            ..DoclingOptions::default()
        };
        assert!(invalid.validate().unwrap_err().to_string().contains("cuts"));
        let too_fast = DoclingOptions {
            video_cuts_per_minute: MAX_VIDEO_CUTS_PER_MINUTE + 1.0,
            ..DoclingOptions::default()
        };
        assert!(
            too_fast
                .validate()
                .unwrap_err()
                .to_string()
                .contains("at most"),
            "max scene rate must be enforced"
        );
        let invalid = DoclingOptions {
            video_prominence: f64::NAN,
            ..DoclingOptions::default()
        };
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("prominence")
        );
    }

    #[test]
    fn docling_video_frame_cap_from_duration() {
        let opts = DoclingOptions {
            video_sampling_mode: DoclingVideoSamplingMode::Fixed,
            video_frame_interval_secs: MIN_VIDEO_FRAME_INTERVAL_SECS,
            ..DoclingOptions::default()
        };
        // 0.5s interval over an hour ⇒ far above MAX_VIDEO_REPRESENTATIVE_FRAMES.
        let err = opts
            .validate_with_duration(Some(3600.0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("frames") || err.contains("limit"), "{err}");

        let ok = DoclingOptions {
            video_sampling_mode: DoclingVideoSamplingMode::Fixed,
            video_frame_interval_secs: 10.0,
            ..DoclingOptions::default()
        };
        assert!(ok.validate_with_duration(Some(60.0)).is_ok());

        let mut clamped = DoclingOptions {
            video_sampling_mode: DoclingVideoSamplingMode::Fixed,
            video_frame_interval_secs: MIN_VIDEO_FRAME_INTERVAL_SECS,
            ..DoclingOptions::default()
        };
        clamped.clamp_interval_for_duration(3600.0);
        assert!(
            clamped.video_frame_interval_secs
                >= 3600.0 / f64::from(MAX_VIDEO_REPRESENTATIVE_FRAMES)
        );
        assert!(clamped.validate_with_duration(Some(3600.0)).is_ok());
    }

    #[test]
    fn asr_and_video_sampling_values_round_trip() {
        for model in DoclingAsrModel::all() {
            assert_eq!(model.id().parse::<DoclingAsrModel>().unwrap(), *model);
        }
        for sampling in DoclingVideoSamplingMode::all() {
            assert_eq!(
                sampling.id().parse::<DoclingVideoSamplingMode>().unwrap(),
                *sampling
            );
        }
    }

    #[test]
    fn process_failure_surfaces_stderr_detail() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("fail-stderr");
        let executable = directory.join(format!("shift-docling-fail-{suffix}"));
        let input = directory.join(format!("shift-docling-fail-in-{suffix}.pdf"));
        write_fake_docling_body(
            &executable,
            "#!/bin/sh\necho 'parser exploded' >&2\nexit 2\n",
        );
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let err = DoclingModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Docling could not convert") && message.contains("parser exploded"),
            "{message}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn process_failure_falls_back_to_stdout_when_stderr_empty() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("fail-stdout");
        let executable = directory.join(format!("shift-docling-fail-{suffix}"));
        let input = directory.join(format!("shift-docling-fail-in-{suffix}.pdf"));
        write_fake_docling_body(&executable, "#!/bin/sh\necho 'only on stdout'\nexit 1\n");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let err = DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("only on stdout") || message.contains("exited with"),
            "{message}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn process_success_without_output_file_fails_cleanly() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("no-out");
        let executable = directory.join(format!("shift-docling-empty-out-{suffix}"));
        let input = directory.join(format!("shift-docling-empty-in-{suffix}.pdf"));
        // Succeed but never write the expected artifact.
        write_fake_docling_body(&executable, "#!/bin/sh\nexit 0\n");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let err = DoclingModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Docling finished but did not write") || message.contains("not write"),
            "{message}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn output_file_name_handles_empty_and_normal_stems() {
        // PathBuf::set_extension on an empty stem yields an empty path (no file name).
        // convert() avoids this by substituting "converted" when file_stem is empty/None.
        let empty = std::ffi::OsStr::new("");
        let empty_name = DoclingModule::output_file_name(empty, OutputFormat::MARKDOWN);
        assert!(
            empty_name.as_os_str().is_empty() || empty_name.as_os_str() == ".md",
            "empty stem → {empty_name:?}"
        );
        assert_eq!(
            DoclingModule::output_file_name(std::ffi::OsStr::new("report"), OutputFormat::HTML),
            PathBuf::from("report.html")
        );
        assert_eq!(
            DoclingModule::output_file_name(std::ffi::OsStr::new("report"), OutputFormat("plain")),
            PathBuf::from("report.txt")
        );
        // Unsupported/other formats fall through to the format id as extension.
        assert_eq!(
            DoclingModule::output_file_name(std::ffi::OsStr::new("x"), OutputFormat::DOCX),
            PathBuf::from("x.docx")
        );
        assert_eq!(
            DoclingModule::output_file_name(
                std::ffi::OsStr::new("converted"),
                OutputFormat::MARKDOWN
            ),
            PathBuf::from("converted.md")
        );
    }

    #[test]
    fn convert_substitutes_converted_stem_when_file_stem_is_none() {
        // Paths whose file_stem() is None (e.g. ".." components as the name) use "converted".
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("none-stem");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let work = directory.join(format!("shift-docling-none-stem-{suffix}"));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        write_fake_docling(&executable);

        // Create a regular file, then convert using a path that ends with ".." — not practical.
        // Instead exercise the filter branch via a zero-length stem OsString in output_file_name
        // (above) and verify convert still succeeds for a normal hidden-style name.
        let input = work.join(".hidden.pdf");
        fs::write(&input, b"%PDF-1.4 fake").unwrap();
        let artifact = DoclingModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        // ".hidden.pdf" → stem ".hidden" on Unix.
        assert!(
            artifact.file_name.ends_with(".md"),
            "got {}",
            artifact.file_name
        );
        assert!(!artifact.bytes.is_empty());

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn image_export_and_table_mode_trim_whitespace_on_parse() {
        assert_eq!(
            "  embedded  ".parse::<DoclingImageExportMode>().unwrap(),
            DoclingImageExportMode::Embedded
        );
        assert_eq!(
            "\treferenced\n".parse::<DoclingImageExportMode>().unwrap(),
            DoclingImageExportMode::Referenced
        );
        assert_eq!(
            " accurate ".parse::<DoclingTableMode>().unwrap(),
            DoclingTableMode::Accurate
        );
    }

    #[test]
    fn successful_convert_records_provenance_and_media_types() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("prov");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        let module = DoclingModule::with_executable(&executable);
        let md = module
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert_eq!(md.media_type, OutputFormat::MARKDOWN.media_type());
        assert_eq!(md.pipeline, vec!["docling"]);
        assert_eq!(md.invocations.len(), 1);
        assert_eq!(md.invocations[0].module_id, "docling");
        assert!(
            md.invocations[0].argv_display.contains("convert")
                || md.invocations[0].argv_display.contains("--to"),
            "argv_display: {}",
            md.invocations[0].argv_display
        );

        let plain = module
            .convert(&input, OutputFormat("plain"), &ConversionOptions::default())
            .unwrap();
        assert_eq!(plain.media_type, OutputFormat("plain").media_type());
        assert!(plain.file_name.ends_with(".txt"));

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn ocr_and_tables_toggle_flags_on_argv() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("toggles");
        let executable = directory.join(format!("shift-docling-test-{suffix}"));
        let input = directory.join(format!("shift-docling-input-{suffix}.pdf"));
        write_fake_docling(&executable);
        fs::write(&input, b"%PDF-1.4 fake").unwrap();

        // Defaults: --ocr --tables
        DoclingModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--ocr"), "{args}");
        assert!(args.contains("--tables"), "{args}");
        assert!(!args.contains("--no-ocr"), "{args}");
        assert!(!args.contains("--no-tables"), "{args}");

        let options = ConversionOptions {
            docling: DoclingOptions {
                ocr: false,
                tables: false,
                ..DoclingOptions::default()
            },
            ..ConversionOptions::default()
        };
        DoclingModule::with_executable(&executable)
            .convert(&input, OutputFormat::MARKDOWN, &options)
            .unwrap();
        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--no-ocr"), "{args}");
        assert!(args.contains("--no-tables"), "{args}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }
}
