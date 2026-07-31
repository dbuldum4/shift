//! FFmpeg adapter: audio/video/image/subtitle conversion with optional encode knobs.

use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, ConversionProgress,
    InvocationRecord, OutputFormat, TempDirGuard, command_argv_parts, format_argv_display,
    map_spawn_error, max_output_bytes, process_timeout, push_flag_path, push_path_arg,
    read_file_limited, resolve_tool_executable, run_command, run_command_cancellable,
    run_command_cancellable_with_output_dirs, run_command_cancellable_with_output_paths,
    unique_temp_dir,
};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// Broad demux surface FFmpeg handles without exotic builds.
const INPUTS: &[&str] = &[
    // Audio
    "aac", "ac3", "aif", "aiff", "amr", "ape", "caf", "dts", "eac3", "flac", "m4a", "m4b", "m4p",
    "mka", "mp3", "mpc", "oga", "ogg", "opus", "spx", "wav", "weba", "wma",
    // Video / containers
    "3gp", "asf", "avi", "divx", "flv", "gif", "m2ts", "m4v", "mk3d", "mkv", "mov", "mp4", "mpeg",
    "mpg", "mts", "mxf", "ogv", "rm", "rmvb", "ts", "vob", "webm", "wmv",
    // Stills (slideshow / image→image)
    "bmp", "jpeg", "jpg", "png", "tif", "tiff", "webp",
];

/// Formats FFmpeg writes that map onto Shift's media catalog.
const OUTPUTS: &[OutputFormat] = OutputFormat::MEDIA;

/// Audio + stills other modules (MarkItDown) can consume after a first hop.
const CHAINABLE: &[OutputFormat] = &[
    OutputFormat::MP3,
    OutputFormat::WAV,
    OutputFormat::FLAC,
    OutputFormat::AAC,
    OutputFormat::M4A,
    OutputFormat::OGG,
    OutputFormat::OPUS,
    OutputFormat::AC3,
    OutputFormat::WMA,
    OutputFormat::CAF,
    OutputFormat::AIFF,
    OutputFormat::PNG,
    OutputFormat::JPG,
    OutputFormat::GIF,
];

/// How FFmpeg should treat codecs when writing the destination container.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FfmpegEncodeMode {
    /// Re-encode with quality presets (reliable default).
    #[default]
    Auto,
    /// Try bitstream copy (`-c copy`). Fails when the destination cannot hold the streams.
    PreferCopy,
    /// Always re-encode (applies quality, mono, sample rate, scale).
    Reencode,
}

impl FfmpegEncodeMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::PreferCopy => "copy",
            Self::Reencode => "reencode",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::PreferCopy => "Stream copy",
            Self::Reencode => "Re-encode",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Auto, Self::PreferCopy, Self::Reencode]
    }
}

impl std::str::FromStr for FfmpegEncodeMode {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "copy" | "stream-copy" | "stream_copy" => Ok(Self::PreferCopy),
            "reencode" | "re-encode" | "encode" => Ok(Self::Reencode),
            other => Err(ConversionError::new(format!(
                "unknown FFmpeg encode mode: {other} (try auto, copy, reencode)"
            ))),
        }
    }
}

/// Output size / fidelity tradeoff when re-encoding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FfmpegQuality {
    #[default]
    Balanced,
    High,
    Small,
}

impl FfmpegQuality {
    pub fn id(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::High => "high",
            Self::Small => "small",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Balanced => "Balanced",
            Self::High => "High quality",
            Self::Small => "Smaller file",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Balanced, Self::High, Self::Small]
    }
}

impl std::str::FromStr for FfmpegQuality {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "balanced" | "default" | "medium" => Ok(Self::Balanced),
            "high" | "hq" => Ok(Self::High),
            "small" | "low" | "compact" => Ok(Self::Small),
            other => Err(ConversionError::new(format!(
                "unknown FFmpeg quality: {other} (try balanced, high, small)"
            ))),
        }
    }
}

/// Optional knobs for media conversion. Empty/default is a plain container conversion.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FfmpegOptions {
    /// Seek before decoding (`-ss` before `-i`), in seconds.
    pub start_secs: Option<f64>,
    /// Limit output duration (`-t`), in seconds.
    pub duration_secs: Option<f64>,
    /// Frame timestamp for still extraction (defaults to `start_secs` or 0).
    pub frame_secs: Option<f64>,
    /// Interval between frames for [`OutputFormat::PNG_SEQUENCE_ZIP`] (seconds).
    pub frame_interval_secs: Option<f64>,
    /// Audio stream index among audio streams (`0:a:N`).
    pub audio_stream: Option<u32>,
    /// Subtitle stream index (`0:s:N`).
    pub subtitle_stream: Option<u32>,
    pub encode_mode: FfmpegEncodeMode,
    pub quality: FfmpegQuality,
    /// Downmix to a single channel when re-encoding audio.
    pub mono: bool,
    /// Target sample rate in Hz when re-encoding audio.
    pub sample_rate_hz: Option<u32>,
    /// Scale video width (height auto) when re-encoding video / GIF / stills.
    pub scale_width: Option<u32>,
    /// Force constant frame rate when re-encoding video.
    pub fps: Option<f64>,
    /// Drop audio on video outputs (`-an`).
    pub mute: bool,
    /// Apply loudness normalization (`-af loudnorm`) when re-encoding audio.
    pub normalize_audio: bool,
    /// Burn embedded subtitle stream into video (forces re-encode).
    pub burn_subtitles: bool,
}

/// Maximum PNG frames written into a sequence ZIP.
pub const MAX_SEQUENCE_FRAMES: u32 = 300;

impl FfmpegOptions {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// True when options require decoding/filters (stream copy is not possible).
    pub fn forces_reencode(&self) -> bool {
        self.mono
            || self.sample_rate_hz.is_some()
            || self.scale_width.is_some()
            || self.fps.is_some()
            || self.mute
            || self.normalize_audio
            || self.burn_subtitles
            || self.frame_interval_secs.is_some()
    }
}

/// True when this path is something the FFmpeg module can open.
pub fn input_looks_like_media(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            INPUTS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

/// True when the format is written by the FFmpeg module.
pub fn is_ffmpeg_output(format: OutputFormat) -> bool {
    OUTPUTS.contains(&format)
}

pub fn is_audio_output(format: OutputFormat) -> bool {
    matches!(
        format.id(),
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "opus" | "ac3" | "wma" | "caf" | "aiff"
    )
}

pub fn is_video_output(format: OutputFormat) -> bool {
    matches!(
        format.id(),
        "mp4" | "webm" | "mkv" | "mov" | "avi" | "gif" | "m4v" | "mpeg" | "ts" | "3gp"
    )
}

pub fn is_image_output(format: OutputFormat) -> bool {
    matches!(format.id(), "png" | "jpg" | "webp")
}

pub fn is_subtitle_output(format: OutputFormat) -> bool {
    matches!(format.id(), "srt" | "vtt")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TargetBitrates {
    audio_bps: Option<u64>,
    video_bps: Option<u64>,
}

impl TargetBitrates {
    fn scale(self, factor: f64) -> Self {
        Self {
            audio_bps: self
                .audio_bps
                .map(|value| ((value as f64 * factor) as u64).max(24_000)),
            video_bps: self
                .video_bps
                .map(|value| ((value as f64 * factor) as u64).max(80_000)),
        }
    }
}

pub fn ffmpeg_supports_target_size_output(format: OutputFormat) -> bool {
    matches!(
        format.id(),
        "mp3"
            | "aac"
            | "m4a"
            | "ogg"
            | "opus"
            | "ac3"
            | "wma"
            | "mp4"
            | "webm"
            | "mkv"
            | "mov"
            | "avi"
            | "m4v"
            | "mpeg"
            | "ts"
            | "3gp"
    )
}

#[derive(Clone, Debug)]
pub struct FfmpegModule {
    executable: OsString,
    ffprobe_executable: OsString,
    /// WEBP encoding requires libwebp at compile time. Many macOS ffmpeg
    /// installs (including the one on this developer machine) have the muxer
    /// but no encoder, so we probe once and hide WEBP from dispatch rather
    /// than fail at the end of a conversion.
    webp_encoder_available: bool,
    /// Runtime output list: full `MEDIA` when libwebp is available, otherwise
    /// `MEDIA` without `WEBP`.
    outputs: Vec<OutputFormat>,
}

impl Default for FfmpegModule {
    fn default() -> Self {
        // Absolute path when found so GUI apps with a minimal PATH match
        // diagnostics readiness (PATH + common_bin_dirs).
        let executable = resolve_tool_executable("SHIFT_FFMPEG_BIN", "ffmpeg", &[]);
        let ffprobe_executable = resolve_tool_executable("SHIFT_FFPROBE_BIN", "ffprobe", &[]);
        let webp_encoder_available = cached_webp_encoder_available(&executable);
        let outputs = ffmpeg_outputs(webp_encoder_available);
        Self {
            executable,
            ffprobe_executable,
            webp_encoder_available,
            outputs,
        }
    }
}

impl FfmpegModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        let executable = executable.into();
        Self {
            ffprobe_executable: executable.clone(),
            executable,
            // Unit tests that provide a fake or bare-name binary should not be
            // blocked by a developer's real ffmpeg configuration.
            webp_encoder_available: true,
            outputs: OutputFormat::MEDIA.to_vec(),
        }
    }

    pub fn with_executables(
        executable: impl Into<OsString>,
        ffprobe_executable: impl Into<OsString>,
    ) -> Self {
        Self {
            executable: executable.into(),
            ffprobe_executable: ffprobe_executable.into(),
            webp_encoder_available: true,
            outputs: OutputFormat::MEDIA.to_vec(),
        }
    }

    fn output_file_name(stem: &std::ffi::OsStr, output_format: OutputFormat) -> PathBuf {
        let mut output = PathBuf::from(stem);
        output.set_extension(output_format.extension());
        output
    }

    fn convert_with_cli(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !self.output_formats().contains(&output_format) {
            return Err(ConversionError::new(format!(
                "FFmpeg does not produce {}",
                output_format.label()
            )));
        }

        let stem = input
            .file_stem()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsStr::new("converted"));

        if output_format == OutputFormat::PNG_SEQUENCE_ZIP {
            return self.convert_png_sequence_zip(input, stem, options);
        }

        let work_dir = unique_temp_dir("shift-ffmpeg")?;
        let cleanup = TempDirGuard(work_dir.clone());
        let produced = work_dir.join(Self::output_file_name(stem, output_format));
        let mut target_bitrates = match options.target_size_bytes {
            Some(target) => Some(self.target_bitrates(
                input,
                output_format,
                &options.ffmpeg,
                target,
                options,
            )?),
            None => None,
        };
        let max_attempts = if target_bitrates.is_some() { 4 } else { 1 };
        let mut invocations = Vec::new();
        let mut fitted_bytes = None;
        let mut smallest = u64::MAX;

        for attempt in 0..max_attempts {
            let _ = fs::remove_file(&produced);
            let mut command = self.build_command_with_target(
                input,
                &produced,
                output_format,
                &options.ffmpeg,
                target_bitrates,
            )?;
            let progress_path = if options.progress.is_some() {
                let path = work_dir.join(format!("ffmpeg-progress-{attempt}.txt"));
                fs::write(&path, b"").ok();
                command.arg("-progress").arg(&path);
                command.arg("-stats_period").arg("0.5");
                Some(path)
            } else {
                None
            };

            invocations.push(InvocationRecord {
                module_id: self.id(),
                argv_display: format_argv_display(&command_argv_parts(&command)),
            });
            report_phase(
                options,
                if target_bitrates.is_some() {
                    "FFmpeg fitting output…"
                } else {
                    "FFmpeg converting…"
                },
            );
            let progress_stop = spawn_progress_watcher(progress_path, options);
            let output = run_command_cancellable_with_output_paths(
                command,
                process_timeout(),
                max_output_bytes(),
                options.cancel.clone(),
                std::slice::from_ref(&produced),
            );
            stop_progress_watcher(progress_stop);
            let output = output.map_err(|error| {
                map_spawn_error(
                    error,
                    "FFmpeg is not installed. Install it with `brew install ffmpeg`, \
                     or set SHIFT_FFMPEG_BIN.",
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
                return Err(ConversionError::new(format!(
                    "FFmpeg could not convert {} to {}: {detail}",
                    input.display(),
                    output_format.label()
                )));
            }

            let actual_size = fs::metadata(&produced)
                .map_err(|error| {
                    ConversionError::new(format!(
                        "FFmpeg finished but did not write {}: {error}",
                        produced.display()
                    ))
                })?
                .len();
            smallest = smallest.min(actual_size);
            if options
                .target_size_bytes
                .is_none_or(|target| actual_size <= target)
            {
                fitted_bytes = Some(read_file_limited(&produced, max_output_bytes()).map_err(
                    |error| {
                        ConversionError::new(format!(
                            "FFmpeg output was not readable at {}: {error}",
                            produced.display()
                        ))
                    },
                )?);
                break;
            }

            let target = options.target_size_bytes.unwrap_or_default();
            let factor = (target as f64 / actual_size as f64 * 0.94).clamp(0.20, 0.92);
            target_bitrates = target_bitrates.map(|bitrates| bitrates.scale(factor));
        }

        let bytes = fitted_bytes.ok_or_else(|| {
            ConversionError::new(format!(
                "media could not fit under {} bytes after {max_attempts} passes \
                 (smallest attempt was {smallest} bytes); choose a larger target, \
                 shorter duration, or smaller dimensions \
                 (video planning floors ~80 kbps video / ~32 kbps audio)",
                options.target_size_bytes.unwrap_or_default()
            ))
        })?;

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
            invocations,
        })
    }

    fn convert_png_sequence_zip(
        &self,
        input: &Path,
        stem: &std::ffi::OsStr,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        validate_options(&options.ffmpeg)?;
        let interval = options.ffmpeg.frame_interval_secs.unwrap_or(1.0);
        if !interval.is_finite() || interval <= 0.0 {
            return Err(ConversionError::new(
                "FFmpeg frame interval must be a positive number of seconds",
            ));
        }
        if options.ffmpeg.encode_mode == FfmpegEncodeMode::PreferCopy {
            return Err(ConversionError::new(
                "stream copy cannot produce a PNG frame sequence; choose Auto/Re-encode",
            ));
        }

        let work_dir = unique_temp_dir("shift-ffmpeg-seq")?;
        let cleanup = TempDirGuard(work_dir.clone());
        let frames_dir = work_dir.join("frames");
        fs::create_dir_all(&frames_dir).map_err(|error| {
            ConversionError::new(format!(
                "could not create frame directory {}: {error}",
                frames_dir.display()
            ))
        })?;
        let pattern = frames_dir.join("frame_%04d.png");

        let mut command = Command::new(&self.executable);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y");

        if let Some(secs) = options.ffmpeg.start_secs {
            command.arg("-ss").arg(format_timestamp(secs));
        }
        push_flag_path(&mut command, "-i", input);
        if let Some(secs) = options.ffmpeg.duration_secs {
            command.arg("-t").arg(format_timestamp(secs));
        }

        let fps = 1.0 / interval;
        let mut filters = vec![format!("fps={fps}")];
        if let Some(width) = options.ffmpeg.scale_width {
            filters.push(format!("scale={width}:-2"));
        }
        command.arg("-vf").arg(filters.join(","));
        command
            .arg("-frames:v")
            .arg(MAX_SEQUENCE_FRAMES.to_string());
        push_path_arg(&mut command, &pattern)?;

        let invocation = InvocationRecord {
            module_id: self.id(),
            argv_display: format_argv_display(&command_argv_parts(&command)),
        };

        report_phase(options, "FFmpeg extracting frames…");
        let output = run_command_cancellable_with_output_dirs(
            command,
            process_timeout(),
            max_output_bytes(),
            options.cancel.clone(),
            &[],
            &[(frames_dir.clone(), max_output_bytes() as u64)],
        )
        .map_err(|error| {
            map_spawn_error(
                error,
                "FFmpeg is not installed. Install it with `brew install ffmpeg`, \
                 or set SHIFT_FFMPEG_BIN.",
            )
        })?;

        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(ConversionError::new(format!(
                "FFmpeg could not extract frame sequence from {}: {}",
                input.display(),
                if detail.is_empty() {
                    output.status.to_string()
                } else {
                    detail
                }
            )));
        }

        let stem_str = {
            let value = stem.to_string_lossy();
            if value.is_empty() {
                "converted".to_owned()
            } else {
                value.into_owned()
            }
        };
        let zip_path = work_dir.join(format!("{stem_str}.zip"));
        zip_png_frames(&frames_dir, &zip_path)?;
        let bytes = read_file_limited(&zip_path, max_output_bytes()).map_err(|error| {
            ConversionError::new(format!(
                "FFmpeg frame ZIP was not readable at {}: {error}",
                zip_path.display()
            ))
        })?;

        drop(cleanup);

        Ok(ConversionArtifact {
            file_name: format!("{stem_str}.zip"),
            media_type: OutputFormat::PNG_SEQUENCE_ZIP.media_type(),
            bytes,
            format: OutputFormat::PNG_SEQUENCE_ZIP,
            module_id: self.id(),
            pipeline: vec![self.id()],
            invocations: vec![invocation],
        })
    }

    #[cfg(test)]
    fn build_command(
        &self,
        input: &Path,
        produced: &Path,
        output_format: OutputFormat,
        options: &FfmpegOptions,
    ) -> Result<Command, ConversionError> {
        self.build_command_with_target(input, produced, output_format, options, None)
    }

    fn build_command_with_target(
        &self,
        input: &Path,
        produced: &Path,
        output_format: OutputFormat,
        options: &FfmpegOptions,
        target_bitrates: Option<TargetBitrates>,
    ) -> Result<Command, ConversionError> {
        validate_options(options)?;

        let mut command = Command::new(&self.executable);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y");

        // Input-side seek is faster for long media, but still-image extraction
        // needs output-side `-ss` so it lands on the exact frame (not the
        // nearest keyframe) for long-GOP sources.
        if let Some(secs) = options.start_secs {
            if !is_image_output(output_format) {
                command.arg("-ss").arg(format_timestamp(secs));
            }
        }

        push_flag_path(&mut command, "-i", input);

        if is_image_output(output_format) {
            if let Some(secs) = options.frame_secs.or(options.start_secs) {
                command.arg("-ss").arg(format_timestamp(secs));
            }
        }

        if let Some(secs) = options.duration_secs {
            if !is_image_output(output_format) && !is_subtitle_output(output_format) {
                command.arg("-t").arg(format_timestamp(secs));
            }
        }

        apply_stream_maps(&mut command, output_format, options);
        apply_encode_settings(&mut command, input, output_format, options, target_bitrates)?;

        // Trailing output path: absolute, rejected if option-like as a bare arg.
        push_path_arg(&mut command, produced)?;
        Ok(command)
    }

    fn target_bitrates(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &FfmpegOptions,
        target_bytes: u64,
        conversion: &ConversionOptions,
    ) -> Result<TargetBitrates, ConversionError> {
        if !ffmpeg_supports_target_size_output(output_format) {
            return Err(ConversionError::new(format!(
                "FFmpeg cannot fit {} output to a target size",
                output_format.label()
            )));
        }
        if options.encode_mode == FfmpegEncodeMode::PreferCopy {
            return Err(ConversionError::new(
                "stream copy cannot guarantee a target size; choose Auto or Re-encode",
            ));
        }
        let duration = if let Some(duration) = options.duration_secs {
            duration
        } else {
            let total = self.probe_duration(input, conversion)?;
            (total - options.start_secs.unwrap_or(0.0)).max(0.0)
        };
        if !duration.is_finite() || duration <= 0.0 {
            return Err(ConversionError::new(
                "could not determine a positive media duration for target-size encoding; \
                 set Duration explicitly",
            ));
        }

        // Reserve six percent for container/index overhead. Subsequent passes
        // correct encoder/container variance using the actual artifact size.
        let total_bps = ((target_bytes as f64 * 8.0 / duration) * 0.94) as u64;
        if is_audio_output(output_format) {
            return Ok(TargetBitrates {
                audio_bps: Some(total_bps.clamp(24_000, 320_000)),
                video_bps: None,
            });
        }

        let audio_bps = if options.mute {
            None
        } else {
            Some((total_bps / 6).clamp(32_000, 128_000))
        };
        let video_bps = total_bps.saturating_sub(audio_bps.unwrap_or(0)).max(80_000);
        Ok(TargetBitrates {
            audio_bps,
            video_bps: Some(video_bps),
        })
    }

    fn probe_duration(
        &self,
        input: &Path,
        options: &ConversionOptions,
    ) -> Result<f64, ConversionError> {
        let mut command = Command::new(&self.ffprobe_executable);
        command
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1");
        push_path_arg(&mut command, input)?;
        let output = run_command_cancellable(
            command,
            Duration::from_secs(15),
            64 * 1024,
            options.cancel.clone(),
        )
        .map_err(|error| {
            map_spawn_error(
                error,
                "ffprobe is required to fit media to a target size. Install FFmpeg \
                 with `brew install ffmpeg`, set SHIFT_FFPROBE_BIN, or set Duration.",
            )
        })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(ConversionError::new(format!(
                "ffprobe could not determine media duration: {}",
                if detail.is_empty() {
                    output.status.to_string()
                } else {
                    detail
                }
            )));
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<f64>()
            .map_err(|_| ConversionError::new("ffprobe returned an invalid media duration"))
    }
}

pub fn validate_ffmpeg_options(options: &FfmpegOptions) -> Result<(), ConversionError> {
    validate_options(options)
}

fn validate_options(options: &FfmpegOptions) -> Result<(), ConversionError> {
    for (label, value) in [("start", options.start_secs), ("frame", options.frame_secs)] {
        if let Some(secs) = value {
            if !secs.is_finite() || secs < 0.0 {
                return Err(ConversionError::new(format!(
                    "FFmpeg {label} must be a non-negative number of seconds"
                )));
            }
        }
    }
    if let Some(secs) = options.duration_secs {
        if !secs.is_finite() || secs <= 0.0 {
            return Err(ConversionError::new(
                "FFmpeg duration must be a positive number of seconds".to_string(),
            ));
        }
    }
    if let Some(secs) = options.frame_interval_secs {
        if !secs.is_finite() || secs <= 0.0 {
            return Err(ConversionError::new(
                "FFmpeg frame interval must be a positive number of seconds",
            ));
        }
    }
    if let Some(fps) = options.fps {
        if !fps.is_finite() || fps <= 0.0 || fps > 240.0 {
            return Err(ConversionError::new(
                "FFmpeg fps must be between 0 and 240 (exclusive of 0)",
            ));
        }
    }
    if let Some(rate) = options.sample_rate_hz {
        if !(8000..=192_000).contains(&rate) {
            return Err(ConversionError::new(
                "FFmpeg sample rate must be between 8000 and 192000 Hz",
            ));
        }
    }
    if let Some(width) = options.scale_width {
        if !(16..=7680).contains(&width) {
            return Err(ConversionError::new(
                "FFmpeg scale width must be between 16 and 7680",
            ));
        }
    }
    Ok(())
}

fn report_phase(options: &ConversionOptions, label: &str) {
    if let Some(sink) = options.progress.as_ref() {
        sink(ConversionProgress::Phase(label.to_owned()));
    }
}

struct ProgressWatchStop(
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread::JoinHandle<()>,
);

fn spawn_progress_watcher(
    progress_path: Option<PathBuf>,
    options: &ConversionOptions,
) -> Option<ProgressWatchStop> {
    let sink = options.progress.clone()?;
    let path = progress_path?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = std::sync::Arc::clone(&stop);
    let duration_hint = options.ffmpeg.duration_secs;
    let handle = thread::spawn(move || {
        let mut last_out_time_ms: Option<u64> = None;
        while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(file) = fs::File::open(&path) {
                let mut out_time_ms = last_out_time_ms;
                for line in BufReader::new(file).lines().map_while(Result::ok) {
                    if let Some(value) = line.strip_prefix("out_time_ms=") {
                        if let Ok(ms) = value.trim().parse::<u64>() {
                            out_time_ms = Some(ms);
                        }
                    } else if let Some(value) = line.strip_prefix("out_time_us=") {
                        // Prefer microseconds when present (newer FFmpeg).
                        if let Ok(us) = value.trim().parse::<u64>() {
                            out_time_ms = Some(us / 1000);
                        }
                    } else if line == "progress=end" {
                        sink(ConversionProgress::Fraction {
                            fraction: 1.0,
                            label: "FFmpeg finished".into(),
                        });
                    }
                }
                if let Some(ms) = out_time_ms {
                    if last_out_time_ms != Some(ms) {
                        last_out_time_ms = Some(ms);
                        let secs = ms as f32 / 1000.0;
                        if let Some(total) = duration_hint.filter(|d| *d > 0.0) {
                            let fraction = (secs / total as f32).clamp(0.0, 0.99);
                            sink(ConversionProgress::Fraction {
                                fraction,
                                label: format!("FFmpeg {secs:.1}s"),
                            });
                        } else {
                            sink(ConversionProgress::Phase(format!("FFmpeg {secs:.1}s")));
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
    Some(ProgressWatchStop(stop, handle))
}

fn stop_progress_watcher(stop: Option<ProgressWatchStop>) {
    if let Some(ProgressWatchStop(flag, handle)) = stop {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = handle.join();
    }
}

fn zip_png_frames(frames_dir: &Path, zip_path: &Path) -> Result<(), ConversionError> {
    let mut entries: Vec<PathBuf> = fs::read_dir(frames_dir)
        .map_err(|error| {
            ConversionError::new(format!(
                "could not list frames in {}: {error}",
                frames_dir.display()
            ))
        })?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension()
                .and_then(|value| value.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        return Err(ConversionError::new(
            "FFmpeg did not produce any PNG frames for the sequence ZIP",
        ));
    }
    if entries.len() > MAX_SEQUENCE_FRAMES as usize {
        entries.truncate(MAX_SEQUENCE_FRAMES as usize);
    }

    let file = fs::File::create(zip_path).map_err(|error| {
        ConversionError::new(format!(
            "could not create ZIP {}: {error}",
            zip_path.display()
        ))
    })?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for path in &entries {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("frame.png");
        let mut reader = BufReader::new(fs::File::open(path).map_err(|error| {
            ConversionError::new(format!("could not read frame {}: {error}", path.display()))
        })?);
        zip.start_file(name, options).map_err(|error| {
            ConversionError::new(format!("could not add {name} to ZIP: {error}"))
        })?;
        std::io::copy(&mut reader, &mut zip).map_err(|error| {
            ConversionError::new(format!("could not write {name} into ZIP: {error}"))
        })?;
    }
    zip.finish().map_err(|error| {
        ConversionError::new(format!(
            "could not finalize ZIP {}: {error}",
            zip_path.display()
        ))
    })?;
    Ok(())
}

fn format_timestamp(secs: f64) -> String {
    // FFmpeg accepts plain seconds; keep a few decimals for frame-accurate seeks.
    if (secs - secs.round()).abs() < 1e-9 {
        format!("{}", secs.round() as i64)
    } else {
        format!("{secs:.3}")
    }
}

fn apply_stream_maps(command: &mut Command, output_format: OutputFormat, options: &FfmpegOptions) {
    if is_subtitle_output(output_format) {
        let index = options.subtitle_stream.unwrap_or(0);
        command.arg("-map").arg(format!("0:s:{index}"));
        return;
    }

    if is_audio_output(output_format) {
        command.arg("-vn");
        if let Some(index) = options.audio_stream {
            command.arg("-map").arg(format!("0:a:{index}"));
        }
        return;
    }

    if is_image_output(output_format) {
        command.arg("-an");
        command.arg("-frames:v").arg("1");
        return;
    }

    // Video (and GIF): optional explicit audio stream pick / mute.
    if options.mute {
        command.arg("-map").arg("0:v:0");
        command.arg("-an");
        return;
    }

    if let Some(index) = options.audio_stream {
        command.arg("-map").arg("0:v:0");
        command.arg("-map").arg(format!("0:a:{index}"));
    }
}

fn apply_encode_settings(
    command: &mut Command,
    input: &Path,
    output_format: OutputFormat,
    options: &FfmpegOptions,
    target_bitrates: Option<TargetBitrates>,
) -> Result<(), ConversionError> {
    if is_subtitle_output(output_format) {
        // Let FFmpeg pick a subtitle encoder for the container (srt/webvtt).
        return Ok(());
    }

    let want_copy = options.encode_mode == FfmpegEncodeMode::PreferCopy
        && !is_image_output(output_format)
        && !options.forces_reencode()
        && target_bitrates.is_none();

    if want_copy {
        command.arg("-c").arg("copy");
        return Ok(());
    }

    if options.encode_mode == FfmpegEncodeMode::PreferCopy {
        return Err(ConversionError::new(
            "stream copy cannot be combined with mono, sample-rate, scale, fps, mute, \
             normalize-audio, burn-subtitles, frame interval, target size, or still-image output; \
             choose Auto/Re-encode or clear those options",
        ));
    }

    // Video filters first, then codecs.
    let mut filters = Vec::new();
    if let Some(width) = options.scale_width {
        if is_video_output(output_format) || is_image_output(output_format) {
            filters.push(format!("scale={width}:-2"));
        }
    }
    if let Some(fps) = options.fps {
        if is_video_output(output_format) || is_image_output(output_format) {
            filters.push(format!("fps={fps}"));
        }
    }
    if options.burn_subtitles && is_video_output(output_format) {
        // Escape path for the subtitles filter (\, :, ').
        let path = input.to_string_lossy();
        let escaped = path
            .replace('\\', "\\\\")
            .replace(':', "\\:")
            .replace('\'', "\\'");
        filters.push(format!("subtitles='{escaped}'"));
    }
    if output_format == OutputFormat::GIF {
        // Compact animated GIF; scale if not already requested.
        if options.scale_width.is_none() && options.fps.is_none() {
            match options.quality {
                FfmpegQuality::High => filters.push("fps=15,scale=640:-2:flags=lanczos".into()),
                FfmpegQuality::Balanced => filters.push("fps=10,scale=480:-2:flags=lanczos".into()),
                FfmpegQuality::Small => filters.push("fps=8,scale=320:-2:flags=lanczos".into()),
            }
        } else if options.fps.is_none() {
            filters.push("fps=10".into());
        }
    }
    let mut filter_string = filters.join(",");
    if output_format == OutputFormat::GIF {
        let palette = "split[s0][s1];[s0]palettegen=max_colors=256[p];[s1][p]paletteuse";
        if filter_string.is_empty() {
            filter_string = palette.to_owned();
        } else {
            filter_string = format!("{filter_string},{palette}");
        }
    }
    if !filter_string.is_empty() {
        command.arg("-vf").arg(filter_string);
    }

    // Audio filters (loudnorm).
    if options.normalize_audio
        && (is_audio_output(output_format) || (is_video_output(output_format) && !options.mute))
    {
        command.arg("-af").arg("loudnorm");
    }

    if is_audio_output(output_format) {
        apply_audio_encode(
            command,
            output_format,
            options,
            target_bitrates.and_then(|target| target.audio_bps),
        );
    } else if is_image_output(output_format) {
        apply_image_encode(command, output_format, options);
    } else if is_video_output(output_format) {
        apply_video_encode(command, output_format, options, target_bitrates);
        if options.mute {
            // Belt-and-suspenders if maps did not already drop audio.
            command.arg("-an");
        }
    }

    if options.mono && !options.mute {
        command.arg("-ac").arg("1");
    }
    if let Some(rate) = options.sample_rate_hz {
        if !options.mute {
            command.arg("-ar").arg(rate.to_string());
        }
    }

    Ok(())
}

fn apply_audio_encode(
    command: &mut Command,
    output_format: OutputFormat,
    options: &FfmpegOptions,
    target_bps: Option<u64>,
) {
    let bitrate = target_bps
        .map(|value| value.to_string())
        .unwrap_or_else(|| {
            match options.quality {
                FfmpegQuality::High => "320k",
                FfmpegQuality::Balanced => "192k",
                FfmpegQuality::Small => "96k",
            }
            .to_owned()
        });
    match output_format.id() {
        "wav" => {
            command.arg("-c:a").arg("pcm_s16le");
        }
        "flac" => {
            command.arg("-c:a").arg("flac");
            let level = match options.quality {
                FfmpegQuality::High => "8",
                FfmpegQuality::Balanced => "5",
                FfmpegQuality::Small => "0",
            };
            command.arg("-compression_level").arg(level);
        }
        "mp3" => {
            command.arg("-c:a").arg("libmp3lame");
            command.arg("-b:a").arg(&bitrate);
        }
        "aac" | "m4a" => {
            command.arg("-c:a").arg("aac");
            command.arg("-b:a").arg(&bitrate);
        }
        "caf" => {
            command.arg("-c:a").arg("pcm_s16le");
        }
        "ogg" | "opus" => {
            command.arg("-c:a").arg("libopus");
            command.arg("-b:a").arg(&bitrate);
        }
        "ac3" => {
            command.arg("-c:a").arg("ac3");
            command.arg("-b:a").arg(&bitrate);
        }
        "wma" => {
            command.arg("-c:a").arg("wmav2");
            command.arg("-b:a").arg(&bitrate);
        }
        "aiff" => {
            command.arg("-c:a").arg("pcm_s16be");
        }
        _ => {
            command.arg("-b:a").arg(&bitrate);
        }
    }
}

fn apply_image_encode(command: &mut Command, output_format: OutputFormat, options: &FfmpegOptions) {
    match output_format.id() {
        "jpg" => {
            let q = match options.quality {
                FfmpegQuality::High => "2",
                FfmpegQuality::Balanced => "3",
                FfmpegQuality::Small => "8",
            };
            command.arg("-q:v").arg(q);
        }
        "png" => {
            let level = match options.quality {
                FfmpegQuality::High => "1",
                FfmpegQuality::Balanced => "3",
                FfmpegQuality::Small => "9",
            };
            command.arg("-compression_level").arg(level);
        }
        "webp" => {
            let q = match options.quality {
                FfmpegQuality::High => "90",
                FfmpegQuality::Balanced => "75",
                FfmpegQuality::Small => "40",
            };
            command.arg("-q:v").arg(q);
        }
        _ => {}
    }
}

fn apply_video_encode(
    command: &mut Command,
    output_format: OutputFormat,
    options: &FfmpegOptions,
    target_bitrates: Option<TargetBitrates>,
) {
    if output_format == OutputFormat::GIF {
        // Palette-based GIF is more complex; fps/scale filters already applied.
        return;
    }

    let crf = match options.quality {
        FfmpegQuality::High => "18",
        FfmpegQuality::Balanced => "23",
        FfmpegQuality::Small => "28",
    };
    let audio_bitrate = target_bitrates
        .and_then(|target| target.audio_bps)
        .map(|value| value.to_string())
        .unwrap_or_else(|| {
            match options.quality {
                FfmpegQuality::High => "192k",
                FfmpegQuality::Balanced => "128k",
                FfmpegQuality::Small => "96k",
            }
            .to_owned()
        });

    if output_format.id() == "webm" {
        command.arg("-c:v").arg("libvpx-vp9");
        if let Some(video_bps) = target_bitrates.and_then(|target| target.video_bps) {
            command.arg("-b:v").arg(video_bps.to_string());
            command.arg("-maxrate").arg(video_bps.to_string());
            command.arg("-bufsize").arg((video_bps * 2).to_string());
        } else {
            command.arg("-crf").arg(crf);
            command.arg("-b:v").arg("0");
        }
        if !options.mute {
            command.arg("-c:a").arg("libopus");
            command.arg("-b:a").arg(&audio_bitrate);
        }
    } else if matches!(
        output_format.id(),
        "mp4" | "m4v" | "mov" | "mkv" | "avi" | "mpeg" | "ts" | "3gp"
    ) {
        command.arg("-c:v").arg("libx264");
        command.arg("-preset").arg(match options.quality {
            FfmpegQuality::High => "slow",
            FfmpegQuality::Balanced => "medium",
            FfmpegQuality::Small => "veryfast",
        });
        if let Some(video_bps) = target_bitrates.and_then(|target| target.video_bps) {
            command.arg("-b:v").arg(video_bps.to_string());
            command.arg("-maxrate").arg(video_bps.to_string());
            command.arg("-bufsize").arg((video_bps * 2).to_string());
        } else {
            command.arg("-crf").arg(crf);
        }
        if !options.mute {
            command.arg("-c:a").arg(if output_format.id() == "mpeg" {
                "mp2"
            } else {
                "aac"
            });
            command.arg("-b:a").arg(&audio_bitrate);
        }
        if matches!(output_format.id(), "mp4" | "m4v" | "mov") {
            command.arg("-movflags").arg("+faststart");
        }
        if output_format.id() == "3gp" && !options.mute {
            // 3GP is picky; force baseline-friendly audio rate if user did not.
            if options.sample_rate_hz.is_none() {
                command.arg("-ar").arg("8000");
            }
            if !options.mono {
                command.arg("-ac").arg("1");
            }
        }
    } else {
        unreachable!(
            "apply_video_encode called with unsupported video output: {}",
            output_format.id()
        );
    }
}

fn probe_webp_encoder(executable: &OsStr) -> bool {
    let mut command = Command::new(executable);
    command.arg("-hide_banner").arg("-encoders");
    match run_command(command, Duration::from_secs(5), 512 * 1024) {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.contains("libwebp")
        }
        _ => false,
    }
}

fn cached_webp_encoder_available(executable: &OsStr) -> bool {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<OsString, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));

    if let Some(available) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(executable)
        .copied()
    {
        return available;
    }

    let available = probe_webp_encoder(executable);
    cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(executable.to_os_string(), available);
    available
}

fn ffmpeg_outputs(webp_encoder_available: bool) -> Vec<OutputFormat> {
    if webp_encoder_available {
        OutputFormat::MEDIA.to_vec()
    } else {
        OutputFormat::MEDIA
            .iter()
            .filter(|&&format| format != OutputFormat::WEBP)
            .copied()
            .collect()
    }
}

impl ConversionModule for FfmpegModule {
    fn id(&self) -> &'static str {
        "ffmpeg"
    }

    fn label(&self) -> &'static str {
        "FFmpeg"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        INPUTS
    }

    fn output_formats(&self) -> &[OutputFormat] {
        &self.outputs
    }

    fn chainable_output_formats(&self) -> &[OutputFormat] {
        CHAINABLE
    }

    fn supports_target_size(&self, output: OutputFormat) -> bool {
        ffmpeg_supports_target_size_output(output)
    }

    fn supports(&self, input: &Path, output: OutputFormat) -> bool {
        if output == OutputFormat::WEBP && !self.webp_encoder_available {
            return false;
        }
        let Some(extension) = input.extension().and_then(|value| value.to_str()) else {
            return false;
        };
        self.input_extensions()
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
            && self.output_formats().contains(&output)
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        self.convert_with_cli(input, output_format, options)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    fn write_fake_ffmpeg(path: &Path) {
        let script = r#"#!/bin/sh
set -e
printf '%s\n' "$*" > "${0}.args"
output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -i) shift 2; continue ;;
    -hide_banner|-nostdin|-y|-vn|-an) shift; continue ;;
    -loglevel|-ss|-t|-map|-c|-c:a|-c:v|-b:a|-b:v|-maxrate|-bufsize|-crf|-preset|-vf|-af|-frames:v|-q:v|-quality|-compression_level|-ac|-ar|-movflags|-progress|-stats_period) shift 2; continue ;;
    -*) shift; continue ;;
    *) output="$1"; shift; continue ;;
  esac
done
case "$output" in
  *.mp3) printf 'ID3fake-mp3' > "$output" ;;
  *.wav) printf 'RIFFfake-wav' > "$output" ;;
  *.flac) printf 'fLaCfake' > "$output" ;;
  *.mp4) printf 'ftypfake-mp4' > "$output" ;;
  *.png) printf 'PNGfake' > "$output" ;;
  frame_%04d.png|*/frame_%04d.png)
    dir=$(dirname "$output")
    printf 'PNGfake' > "$dir/frame_0001.png"
    printf 'PNGfake' > "$dir/frame_0002.png"
    ;;
  *.jpg) printf 'JPEGfake' > "$output" ;;
  *.srt) printf '1\n00:00:00,000 --> 00:00:01,000\nHi\n' > "$output" ;;
  *.vtt) printf 'WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nHi\n' > "$output" ;;
  *.gif) printf 'GIFfake' > "$output" ;;
  *) printf 'fake-media' > "$output" ;;
esac
"#;
        fs::write(path, script).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn write_fake_fit_ffmpeg(path: &Path) {
        let script = r#"#!/bin/sh
set -e
case " $* " in
  *" -show_entries format=duration "*) printf '10.0\n'; exit 0 ;;
esac
printf '%s\n' "$*" > "${0}.args"
output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -i) shift 2; continue ;;
    -hide_banner|-nostdin|-y|-vn|-an) shift; continue ;;
    -loglevel|-ss|-t|-map|-c|-c:a|-c:v|-b:a|-b:v|-maxrate|-bufsize|-crf|-preset|-vf|-af|-frames:v|-q:v|-quality|-compression_level|-ac|-ar|-movflags|-progress|-stats_period) shift 2; continue ;;
    -*) shift; continue ;;
    *) output="$1"; shift; continue ;;
  esac
done
count=0
[ -f "${0}.count" ] && count=$(cat "${0}.count")
count=$((count + 1))
printf '%s' "$count" > "${0}.count"
if [ "$count" -eq 1 ]; then size=120000; else size=80000; fi
dd if=/dev/zero of="$output" bs=1 count="$size" 2>/dev/null
"#;
        fs::write(path, script).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn opts(ffmpeg: FfmpegOptions) -> ConversionOptions {
        ConversionOptions {
            ffmpeg,
            ..ConversionOptions::default()
        }
    }

    #[test]
    fn converts_video_to_mp3_via_temp_output_file() {
        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-ffmpeg-test-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        write_fake_ffmpeg(&executable);
        fs::write(&input, b"fake-video").unwrap();

        let artifact = FfmpegModule::with_executable(&executable)
            .convert(&input, OutputFormat::MP3, &ConversionOptions::default())
            .unwrap();

        assert_eq!(artifact.bytes, b"ID3fake-mp3");
        assert_eq!(artifact.module_id, "ffmpeg");
        assert_eq!(artifact.format, OutputFormat::MP3);

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("-vn"), "args: {args}");
        assert!(args.contains("-i"), "args: {args}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn target_size_probes_duration_and_retries_actual_output() {
        // max_output_bytes() reads process-global environment state; serialize
        // this read with tests that temporarily override the limit.
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = format!("{}-fit", std::process::id());
        let executable = directory.join(format!("shift-ffmpeg-test-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        write_fake_fit_ffmpeg(&executable);
        fs::write(&input, b"fake-video").unwrap();

        let options = ConversionOptions {
            target_size_bytes: Some(100_000),
            ..ConversionOptions::default()
        };
        let artifact = FfmpegModule::with_executable(&executable)
            .convert(&input, OutputFormat::MP3, &options)
            .unwrap();

        assert_eq!(artifact.bytes.len(), 80_000);
        assert_eq!(artifact.invocations.len(), 2);
        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("-b:a"), "args: {args}");
        assert!(!args.contains("-c copy"), "args: {args}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(format!("{}.count", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn target_size_capabilities_exclude_lossless_and_non_media_outputs() {
        let module = FfmpegModule::with_executable("/bin/true");
        assert!(module.supports_target_size(OutputFormat::MP3));
        assert!(module.supports_target_size(OutputFormat::MP4));
        assert!(!module.supports_target_size(OutputFormat::WAV));
        assert!(!module.supports_target_size(OutputFormat::FLAC));
        assert!(!module.supports_target_size(OutputFormat::PNG));
        assert!(!module.supports_target_size(OutputFormat::SRT));
    }

    #[test]
    fn target_size_rejects_prefer_copy_before_spawn() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-copy-fit", std::process::id());
        // Prefer a non-existent binary so a regression that reaches spawn fails
        // loudly instead of accidentally succeeding with a system ffmpeg.
        let executable = directory.join(format!("shift-ffmpeg-missing-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        fs::write(&input, b"fake-video").unwrap();

        let options = ConversionOptions {
            target_size_bytes: Some(100_000),
            ffmpeg: FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                duration_secs: Some(10.0),
                ..FfmpegOptions::default()
            },
            ..ConversionOptions::default()
        };
        let err = FfmpegModule::with_executable(&executable)
            .convert(&input, OutputFormat::MP3, &options)
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("stream copy") || message.contains("target size"),
            "{message}"
        );
        assert!(
            message.contains("Auto") || message.contains("Re-encode") || message.contains("stream"),
            "{message}"
        );
        // Must not have attempted to launch the missing binary under another path.
        assert!(!executable.exists());

        let _ = fs::remove_file(&input);
    }

    #[test]
    fn applies_trim_and_stream_copy_flags() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-opts", std::process::id());
        let executable = directory.join(format!("shift-ffmpeg-test-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mkv"));
        write_fake_ffmpeg(&executable);
        fs::write(&input, b"fake").unwrap();

        let options = opts(FfmpegOptions {
            start_secs: Some(12.5),
            duration_secs: Some(30.0),
            encode_mode: FfmpegEncodeMode::PreferCopy,
            ..FfmpegOptions::default()
        });
        let _ = FfmpegModule::with_executable(&executable)
            .convert(&input, OutputFormat::MP4, &options)
            .unwrap();

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("-ss"), "args: {args}");
        assert!(args.contains("12.5"), "args: {args}");
        assert!(args.contains("-t"), "args: {args}");
        assert!(args.contains("30"), "args: {args}");
        assert!(args.contains("-c"), "args: {args}");
        assert!(args.contains("copy"), "args: {args}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn selects_container_compatible_caf_and_mpeg_audio_codecs() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let options = FfmpegOptions::default();

        let caf = module
            .build_command(
                Path::new("input.mp4"),
                Path::new("output.caf"),
                OutputFormat::CAF,
                &options,
            )
            .unwrap();
        let caf_args = caf
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(
            caf_args
                .windows(2)
                .any(|args| args == ["-c:a", "pcm_s16le"])
        );

        let mpeg = module
            .build_command(
                Path::new("input.mp4"),
                Path::new("output.mpeg"),
                OutputFormat::MPEG,
                &options,
            )
            .unwrap();
        let mpeg_args = mpeg
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(mpeg_args.windows(2).any(|args| args == ["-c:a", "mp2"]));
    }

    #[test]
    fn extracts_still_frame_and_subtitles() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-still", std::process::id());
        let executable = directory.join(format!("shift-ffmpeg-test-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        write_fake_ffmpeg(&executable);
        fs::write(&input, b"fake").unwrap();
        let module = FfmpegModule::with_executable(&executable);

        let png = module
            .convert(
                &input,
                OutputFormat::PNG,
                &opts(FfmpegOptions {
                    frame_secs: Some(3.0),
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap();
        assert_eq!(png.bytes, b"PNGfake");
        let png_args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(png_args.contains("-frames:v"), "args: {png_args}");
        assert!(png_args.contains("3"), "args: {png_args}");

        let srt = module
            .convert(
                &input,
                OutputFormat::SRT,
                &opts(FfmpegOptions {
                    subtitle_stream: Some(1),
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap();
        assert!(srt.text().unwrap_or("").contains("Hi"));
        let srt_args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(srt_args.contains("0:s:1"), "args: {srt_args}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn rejects_stream_copy_with_filters() {
        let err = FfmpegModule::with_executable("ffmpeg")
            .convert(
                Path::new("clip.mp4"),
                OutputFormat::MP3,
                &opts(FfmpegOptions {
                    encode_mode: FfmpegEncodeMode::PreferCopy,
                    mono: true,
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("stream copy"), "error: {err}");
    }

    #[test]
    fn rejects_document_output_formats() {
        let err = FfmpegModule::with_executable("ffmpeg")
            .convert(
                Path::new("clip.mp4"),
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("Markdown") || err.to_string().contains("does not produce"),
            "error: {err}"
        );
    }

    #[test]
    fn advertises_expanded_media_surface() {
        let module = FfmpegModule::default();
        assert!(module.supports(Path::new("clip.MP4"), OutputFormat::MP3));
        assert!(module.supports(Path::new("clip.mkv"), OutputFormat::PNG));
        assert!(module.supports(Path::new("clip.mov"), OutputFormat::SRT));
        assert!(module.supports(Path::new("track.ac3"), OutputFormat::FLAC));
        assert!(module.supports(Path::new("photo.webp"), OutputFormat::JPG));
        assert!(module.supports(Path::new("clip.mp4"), OutputFormat::PNG_SEQUENCE_ZIP));
        assert!(!module.supports(Path::new("report.docx"), OutputFormat::MP3));
        assert!(input_looks_like_media(Path::new("a.webm")));
        assert!(!input_looks_like_media(Path::new("a.docx")));
    }

    #[test]
    fn applies_mute_fps_normalize_and_burn_flags() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let options = FfmpegOptions {
            mute: true,
            fps: Some(24.0),
            normalize_audio: true,
            burn_subtitles: true,
            encode_mode: FfmpegEncodeMode::Reencode,
            ..FfmpegOptions::default()
        };
        let command = module
            .build_command(
                Path::new("/tmp/clip.mp4"),
                Path::new("/tmp/out.mp4"),
                OutputFormat::MP4,
                &options,
            )
            .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|a| a == "-an"), "args: {args:?}");
        assert!(args.iter().any(|a| a.contains("fps=24")), "args: {args:?}");
        // mute skips loudnorm on video
        assert!(!args.iter().any(|a| a == "loudnorm"), "args: {args:?}");
        assert!(
            args.iter().any(|a| a.contains("subtitles=")),
            "args: {args:?}"
        );

        let with_audio = FfmpegOptions {
            normalize_audio: true,
            encode_mode: FfmpegEncodeMode::Reencode,
            ..FfmpegOptions::default()
        };
        let command = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.mp3"),
                OutputFormat::MP3,
                &with_audio,
            )
            .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2).any(|w| w == ["-af", "loudnorm"]),
            "{args:?}"
        );
    }

    #[test]
    fn rejects_stream_copy_with_burn_subtitles() {
        let err = FfmpegModule::with_executable("ffmpeg")
            .convert(
                Path::new("clip.mp4"),
                OutputFormat::MP4,
                &opts(FfmpegOptions {
                    encode_mode: FfmpegEncodeMode::PreferCopy,
                    burn_subtitles: true,
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("stream copy"), "error: {err}");
    }

    #[test]
    fn extracts_png_sequence_zip() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-seq", std::process::id());
        let executable = directory.join(format!("shift-ffmpeg-test-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        write_fake_ffmpeg(&executable);
        fs::write(&input, b"fake").unwrap();

        let artifact = FfmpegModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::PNG_SEQUENCE_ZIP,
                &opts(FfmpegOptions {
                    frame_interval_secs: Some(1.0),
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap();

        assert_eq!(artifact.format, OutputFormat::PNG_SEQUENCE_ZIP);
        assert!(artifact.file_name.ends_with(".zip"));
        assert!(!artifact.bytes.is_empty());
        assert!(!artifact.invocations.is_empty());

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn ffmpeg_encode_mode_and_quality_helpers() {
        assert_eq!(FfmpegEncodeMode::Auto.id(), "auto");
        assert_eq!(FfmpegEncodeMode::PreferCopy.label(), "Stream copy");
        assert_eq!(FfmpegEncodeMode::all().len(), 3);
        assert_eq!(
            "stream_copy".parse::<FfmpegEncodeMode>(),
            Ok(FfmpegEncodeMode::PreferCopy)
        );
        assert!("unknown".parse::<FfmpegEncodeMode>().is_err());

        assert_eq!(FfmpegQuality::High.id(), "high");
        assert_eq!(FfmpegQuality::Small.label(), "Smaller file");
        assert_eq!(FfmpegQuality::all().len(), 3);
        assert_eq!("hq".parse::<FfmpegQuality>(), Ok(FfmpegQuality::High));
        assert!("tiny".parse::<FfmpegQuality>().is_err());
    }

    #[test]
    fn ffmpeg_options_default_and_forces_reencode() {
        let default = FfmpegOptions::default();
        assert!(default.is_default());
        assert!(!default.forces_reencode());

        let mut o = default.clone();
        o.mono = true;
        assert!(!o.is_default());
        assert!(o.forces_reencode());

        o = FfmpegOptions::default();
        o.sample_rate_hz = Some(44100);
        assert!(o.forces_reencode());

        o = FfmpegOptions::default();
        o.scale_width = Some(640);
        assert!(o.forces_reencode());

        o = FfmpegOptions::default();
        o.fps = Some(30.0);
        assert!(o.forces_reencode());

        o = FfmpegOptions::default();
        o.mute = true;
        assert!(o.forces_reencode());

        o = FfmpegOptions::default();
        o.normalize_audio = true;
        assert!(o.forces_reencode());

        o = FfmpegOptions::default();
        o.burn_subtitles = true;
        assert!(o.forces_reencode());

        o = FfmpegOptions::default();
        o.frame_interval_secs = Some(1.0);
        assert!(o.forces_reencode());
    }

    #[test]
    fn output_format_classifiers() {
        assert!(is_ffmpeg_output(OutputFormat::MP3));
        assert!(is_ffmpeg_output(OutputFormat::PNG_SEQUENCE_ZIP));
        assert!(!is_ffmpeg_output(OutputFormat::MARKDOWN));

        assert!(is_audio_output(OutputFormat::FLAC));
        assert!(is_audio_output(OutputFormat::CAF));
        assert!(!is_audio_output(OutputFormat::MP4));

        assert!(is_video_output(OutputFormat::MKV));
        assert!(is_video_output(OutputFormat::GIF));
        assert!(!is_video_output(OutputFormat::PNG));

        assert!(is_image_output(OutputFormat::PNG));
        assert!(is_image_output(OutputFormat::JPG));
        assert!(!is_image_output(OutputFormat::SRT));

        assert!(is_subtitle_output(OutputFormat::SRT));
        assert!(is_subtitle_output(OutputFormat::VTT));
        assert!(!is_subtitle_output(OutputFormat::WAV));
    }

    #[test]
    fn validate_options_rejects_invalid_bounds() {
        assert!(validate_options(&FfmpegOptions::default()).is_ok());

        let o = FfmpegOptions {
            start_secs: Some(-1.0),
            ..FfmpegOptions::default()
        };
        assert!(
            validate_options(&o)
                .unwrap_err()
                .to_string()
                .contains("start")
        );

        let o = FfmpegOptions {
            duration_secs: Some(f64::NAN),
            ..FfmpegOptions::default()
        };
        assert!(
            validate_options(&o)
                .unwrap_err()
                .to_string()
                .contains("duration")
        );

        let o = FfmpegOptions {
            frame_secs: Some(f64::INFINITY),
            ..FfmpegOptions::default()
        };
        assert!(
            validate_options(&o)
                .unwrap_err()
                .to_string()
                .contains("frame")
        );

        let o = FfmpegOptions {
            frame_interval_secs: Some(0.0),
            ..FfmpegOptions::default()
        };
        assert!(
            validate_options(&o)
                .unwrap_err()
                .to_string()
                .contains("frame interval")
        );

        let o = FfmpegOptions {
            fps: Some(300.0),
            ..FfmpegOptions::default()
        };
        assert!(
            validate_options(&o)
                .unwrap_err()
                .to_string()
                .contains("fps")
        );

        let o = FfmpegOptions {
            sample_rate_hz: Some(7000),
            ..FfmpegOptions::default()
        };
        assert!(
            validate_options(&o)
                .unwrap_err()
                .to_string()
                .contains("sample rate")
        );

        let o = FfmpegOptions {
            scale_width: Some(5),
            ..FfmpegOptions::default()
        };
        assert!(
            validate_options(&o)
                .unwrap_err()
                .to_string()
                .contains("scale width")
        );
    }

    #[test]
    fn format_timestamp_rounds_and_truncates() {
        assert_eq!(format_timestamp(5.0), "5");
        assert_eq!(format_timestamp(5.0000000001), "5");
        assert_eq!(format_timestamp(5.123456789), "5.123");
        assert_eq!(format_timestamp(0.5), "0.500");
    }

    #[test]
    fn zip_png_frames_empty_and_success() {
        let dir = std::env::temp_dir().join(format!("shift-ffmpeg-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let zip_path = dir.join("out.zip");

        assert!(zip_png_frames(&empty, &zip_path).is_err());

        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        std::fs::write(frames.join("frame_0001.png"), b"PNG1").unwrap();
        std::fs::write(frames.join("frame_0002.png"), b"PNG2").unwrap();
        std::fs::write(frames.join("ignore.txt"), b"text").unwrap();

        zip_png_frames(&frames, &zip_path).unwrap();
        assert!(zip_path.is_file());

        let file = std::fs::File::open(&zip_path).unwrap();
        let archive = zip::ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_stream_and_subtitle_stream_maps() {
        let module = FfmpegModule::with_executable("ffmpeg");

        let mp3 = module
            .build_command(
                Path::new("clip.mp4"),
                Path::new("out.mp3"),
                OutputFormat::MP3,
                &FfmpegOptions {
                    audio_stream: Some(2),
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = mp3
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"0:a:2".to_owned()));

        let srt = module
            .build_command(
                Path::new("clip.mkv"),
                Path::new("out.srt"),
                OutputFormat::SRT,
                &FfmpegOptions {
                    subtitle_stream: Some(1),
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = srt
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"0:s:1".to_owned()));
    }

    #[test]
    fn mute_and_audio_stream_on_video() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let command = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.mp4"),
                OutputFormat::MP4,
                &FfmpegOptions {
                    mute: true,
                    audio_stream: Some(0),
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Mute takes precedence; explicit audio stream is ignored.
        assert!(
            args.iter()
                .any(|a| a == "-map"
                    && args[args.iter().position(|x| x == a).unwrap() + 1] == "0:v:0")
        );
        assert!(args.contains(&"-an".to_owned()));
    }

    #[test]
    fn gif_quality_presets() {
        let module = FfmpegModule::with_executable("ffmpeg");

        for (quality, expected) in [
            (FfmpegQuality::High, "fps=15,scale=640:-2:flags=lanczos"),
            (FfmpegQuality::Balanced, "fps=10,scale=480:-2:flags=lanczos"),
            (FfmpegQuality::Small, "fps=8,scale=320:-2:flags=lanczos"),
        ] {
            let command = module
                .build_command(
                    Path::new("in.mp4"),
                    Path::new("out.gif"),
                    OutputFormat::GIF,
                    &FfmpegOptions {
                        quality,
                        ..FfmpegOptions::default()
                    },
                )
                .unwrap();
            let vf = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .windows(2)
                .find(|w| w[0] == "-vf")
                .map(|w| w[1].clone())
                .unwrap_or_default();
            assert!(vf.contains(expected), "quality {:?} got vf {vf}", quality);
        }
    }

    #[test]
    fn scale_and_fps_filters_apply_to_video_and_stills() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let command = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.mp4"),
                OutputFormat::MP4,
                &FfmpegOptions {
                    scale_width: Some(640),
                    fps: Some(30.0),
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let vf = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .windows(2)
            .find(|w| w[0] == "-vf")
            .map(|w| w[1].clone())
            .unwrap_or_default();
        assert!(vf.contains("scale=640:-2"), "vf: {vf}");
        assert!(vf.contains("fps=30"), "vf: {vf}");

        let command = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.png"),
                OutputFormat::PNG,
                &FfmpegOptions {
                    scale_width: Some(320),
                    fps: Some(60.0),
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let vf = args
            .windows(2)
            .find(|w| w[0] == "-vf")
            .map(|w| w[1].clone())
            .unwrap_or_default();
        assert!(vf.contains("scale=320:-2"), "vf: {vf}");
        assert!(vf.contains("fps=60"), "vf: {vf}");
    }

    #[test]
    fn subtitle_output_ignores_duration() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let command = module
            .build_command(
                Path::new("in.mkv"),
                Path::new("out.srt"),
                OutputFormat::SRT,
                &FfmpegOptions {
                    duration_secs: Some(10.0),
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.contains(&"-t".to_owned()));
    }

    #[test]
    fn mono_and_sample_rate_for_audio_output() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let command = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.mp3"),
                OutputFormat::MP3,
                &FfmpegOptions {
                    mono: true,
                    sample_rate_hz: Some(22050),
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["-ac", "1"]));
        assert!(args.windows(2).any(|w| w == ["-ar", "22050"]));
    }

    #[test]
    fn webm_and_threegp_codec_selection() {
        let module = FfmpegModule::with_executable("ffmpeg");

        let webm = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.webm"),
                OutputFormat::WEBM,
                &FfmpegOptions::default(),
            )
            .unwrap();
        let args: Vec<String> = webm
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["-c:v", "libvpx-vp9"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "libopus"]));
        assert!(args.windows(2).any(|w| w == ["-b:v", "0"]));

        let threegp = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.3gp"),
                OutputFormat::THREEGP,
                &FfmpegOptions::default(),
            )
            .unwrap();
        let args: Vec<String> = threegp
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["-ar", "8000"]));
        assert!(args.windows(2).any(|w| w == ["-ac", "1"]));
    }

    #[test]
    fn image_quality_presets() {
        let module = FfmpegModule::with_executable("ffmpeg");

        let png = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.png"),
                OutputFormat::PNG,
                &FfmpegOptions {
                    quality: FfmpegQuality::Small,
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = png
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["-compression_level", "9"]));

        let jpg = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.jpg"),
                OutputFormat::JPG,
                &FfmpegOptions {
                    quality: FfmpegQuality::High,
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = jpg
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["-q:v", "2"]));
    }

    #[test]
    fn burn_subtitles_with_video_forces_filter() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let command = module
            .build_command(
                Path::new("/tmp/clip.mp4"),
                Path::new("out.mp4"),
                OutputFormat::MP4,
                &FfmpegOptions {
                    burn_subtitles: true,
                    encode_mode: FfmpegEncodeMode::Reencode,
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let vf = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .windows(2)
            .find(|w| w[0] == "-vf")
            .map(|w| w[1].clone())
            .unwrap_or_default();
        assert!(vf.contains("subtitles='"), "vf: {vf}");
    }

    #[test]
    fn png_sequence_rejects_invalid_options() {
        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-ffmpeg-seq-err-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-seq-err-{suffix}.mp4"));
        write_fake_ffmpeg(&executable);
        fs::write(&input, b"fake").unwrap();

        let err = FfmpegModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::PNG_SEQUENCE_ZIP,
                &opts(FfmpegOptions {
                    frame_interval_secs: Some(-1.0),
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("frame interval"));

        let err = FfmpegModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::PNG_SEQUENCE_ZIP,
                &opts(FfmpegOptions {
                    encode_mode: FfmpegEncodeMode::PreferCopy,
                    frame_interval_secs: Some(1.0),
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("stream copy"));

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn png_sequence_honours_scale_width() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-seq-scale", std::process::id());
        let executable = directory.join(format!("shift-ffmpeg-test-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        write_fake_ffmpeg(&executable);
        fs::write(&input, b"fake").unwrap();

        let artifact = FfmpegModule::with_executable(&executable)
            .convert(
                &input,
                OutputFormat::PNG_SEQUENCE_ZIP,
                &opts(FfmpegOptions {
                    frame_interval_secs: Some(2.0),
                    scale_width: Some(640),
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap();

        assert_eq!(artifact.format, OutputFormat::PNG_SEQUENCE_ZIP);

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("-vf"));
        assert!(args.contains("fps=0.5"), "args: {args}");
        assert!(args.contains("scale=640:-2"), "args: {args}");
        assert!(args.contains("-frames:v"));

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn validate_options_every_error_branch() {
        assert!(validate_options(&FfmpegOptions::default()).is_ok());

        // start_secs
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, -100.0] {
            let o = FfmpegOptions {
                start_secs: Some(bad),
                ..FfmpegOptions::default()
            };
            let err = validate_options(&o).unwrap_err().to_string();
            assert!(err.contains("start"), "start={bad}: {err}");
        }
        assert!(
            validate_options(&FfmpegOptions {
                start_secs: Some(0.0),
                ..FfmpegOptions::default()
            })
            .is_ok()
        );

        // frame_secs
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            let o = FfmpegOptions {
                frame_secs: Some(bad),
                ..FfmpegOptions::default()
            };
            let err = validate_options(&o).unwrap_err().to_string();
            assert!(err.contains("frame"), "frame={bad}: {err}");
        }

        // duration_secs
        for bad in [f64::NAN, f64::INFINITY, 0.0, -5.0] {
            let o = FfmpegOptions {
                duration_secs: Some(bad),
                ..FfmpegOptions::default()
            };
            let err = validate_options(&o).unwrap_err().to_string();
            assert!(err.contains("duration"), "duration={bad}: {err}");
        }
        assert!(
            validate_options(&FfmpegOptions {
                duration_secs: Some(0.001),
                ..FfmpegOptions::default()
            })
            .is_ok()
        );

        // frame_interval_secs
        for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let o = FfmpegOptions {
                frame_interval_secs: Some(bad),
                ..FfmpegOptions::default()
            };
            let err = validate_options(&o).unwrap_err().to_string();
            assert!(err.contains("frame interval"), "interval={bad}: {err}");
        }

        // fps
        for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0, 240.1, 1000.0] {
            let o = FfmpegOptions {
                fps: Some(bad),
                ..FfmpegOptions::default()
            };
            let err = validate_options(&o).unwrap_err().to_string();
            assert!(err.contains("fps"), "fps={bad}: {err}");
        }
        assert!(
            validate_options(&FfmpegOptions {
                fps: Some(240.0),
                ..FfmpegOptions::default()
            })
            .is_ok()
        );
        assert!(
            validate_options(&FfmpegOptions {
                fps: Some(0.1),
                ..FfmpegOptions::default()
            })
            .is_ok()
        );

        // sample_rate_hz
        for bad in [0, 1, 7999, 192_001, u32::MAX] {
            let o = FfmpegOptions {
                sample_rate_hz: Some(bad),
                ..FfmpegOptions::default()
            };
            let err = validate_options(&o).unwrap_err().to_string();
            assert!(err.contains("sample rate"), "rate={bad}: {err}");
        }
        for ok in [8000, 44100, 48000, 192_000] {
            assert!(
                validate_options(&FfmpegOptions {
                    sample_rate_hz: Some(ok),
                    ..FfmpegOptions::default()
                })
                .is_ok()
            );
        }

        // scale_width
        for bad in [0, 1, 15, 7681, u32::MAX] {
            let o = FfmpegOptions {
                scale_width: Some(bad),
                ..FfmpegOptions::default()
            };
            let err = validate_options(&o).unwrap_err().to_string();
            assert!(err.contains("scale width"), "width={bad}: {err}");
        }
        for ok in [16, 640, 1920, 7680] {
            assert!(
                validate_options(&FfmpegOptions {
                    scale_width: Some(ok),
                    ..FfmpegOptions::default()
                })
                .is_ok()
            );
        }
    }

    #[test]
    fn stream_copy_conflict_matrix() {
        let conflicts = [
            FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                mono: true,
                ..FfmpegOptions::default()
            },
            FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                sample_rate_hz: Some(44100),
                ..FfmpegOptions::default()
            },
            FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                scale_width: Some(640),
                ..FfmpegOptions::default()
            },
            FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                fps: Some(24.0),
                ..FfmpegOptions::default()
            },
            FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                mute: true,
                ..FfmpegOptions::default()
            },
            FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                normalize_audio: true,
                ..FfmpegOptions::default()
            },
            FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                burn_subtitles: true,
                ..FfmpegOptions::default()
            },
            FfmpegOptions {
                encode_mode: FfmpegEncodeMode::PreferCopy,
                frame_interval_secs: Some(1.0),
                ..FfmpegOptions::default()
            },
        ];
        let module = FfmpegModule::with_executable("ffmpeg");
        for (i, options) in conflicts.into_iter().enumerate() {
            let err = module
                .convert(Path::new("clip.mp4"), OutputFormat::MP4, &opts(options))
                .unwrap_err();
            assert!(
                err.to_string().contains("stream copy"),
                "conflict case {i}: {err}"
            );
        }

        // Still-image output with PreferCopy also fails (cannot copy to a single frame).
        let err = module
            .convert(
                Path::new("clip.mp4"),
                OutputFormat::PNG,
                &opts(FfmpegOptions {
                    encode_mode: FfmpegEncodeMode::PreferCopy,
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("stream copy"), "{err}");
    }

    #[test]
    fn every_media_output_format_classifier() {
        for format in OutputFormat::MEDIA {
            assert!(
                is_ffmpeg_output(*format),
                "{} should be ffmpeg output",
                format.id()
            );
            let audio = is_audio_output(*format);
            let video = is_video_output(*format);
            let image = is_image_output(*format);
            let sub = is_subtitle_output(*format);
            let seq = *format == OutputFormat::PNG_SEQUENCE_ZIP;
            let kinds = [audio, video, image, sub, seq]
                .into_iter()
                .filter(|x| *x)
                .count();
            assert_eq!(
                kinds,
                1,
                "{} must be exactly one of audio/video/image/subtitle/seq (a={audio} v={video} i={image} s={sub} z={seq})",
                format.id()
            );
        }

        // Explicit id matrix.
        for id in [
            "mp3", "wav", "flac", "aac", "m4a", "ogg", "opus", "ac3", "wma", "caf", "aiff",
        ] {
            let f: OutputFormat = id.parse().unwrap();
            assert!(is_audio_output(f), "{id}");
            assert!(!is_video_output(f) && !is_image_output(f) && !is_subtitle_output(f));
        }
        for id in [
            "mp4", "webm", "mkv", "mov", "avi", "gif", "m4v", "mpeg", "ts", "3gp",
        ] {
            let f: OutputFormat = id.parse().unwrap();
            assert!(is_video_output(f), "{id}");
        }
        for id in ["png", "jpg", "webp"] {
            let f: OutputFormat = id.parse().unwrap();
            assert!(is_image_output(f), "{id}");
        }
        for id in ["srt", "vtt"] {
            let f: OutputFormat = id.parse().unwrap();
            assert!(is_subtitle_output(f), "{id}");
        }
        assert!(!is_ffmpeg_output(OutputFormat::MARKDOWN));
        assert!(!is_ffmpeg_output(OutputFormat::PDF));
        assert!(!is_ffmpeg_output(OutputFormat::DOCX));
        assert!(!is_ffmpeg_output(OutputFormat::HTML));
    }

    #[test]
    fn input_looks_like_media_covers_ffmpeg_input_list() {
        // Representative sample of every INPUTS family.
        let media = [
            "a.mp3",
            "a.wav",
            "a.flac",
            "a.aac",
            "a.m4a",
            "a.ogg",
            "a.opus",
            "a.ac3",
            "a.wma",
            "a.caf",
            "a.aiff",
            "a.aif",
            "a.mp4",
            "a.mkv",
            "a.mov",
            "a.webm",
            "a.avi",
            "a.gif",
            "a.m4v",
            "a.mpeg",
            "a.mpg",
            "a.ts",
            "a.3gp",
            "a.wmv",
            "a.flv",
            "a.m2ts",
            "a.mts",
            "a.vob",
            "a.asf",
            "a.divx",
            "a.mxf",
            "a.png",
            "a.jpg",
            "a.jpeg",
            "a.webp",
            "a.bmp",
            "a.tif",
            "a.tiff",
            "A.MP4",
            "Track.WAV",
        ];
        for name in media {
            assert!(
                input_looks_like_media(Path::new(name)),
                "expected media: {name}"
            );
        }
        for name in ["a.docx", "a.pdf", "a.html", "a.md", "a.txt", "a", "a.xyz"] {
            assert!(
                !input_looks_like_media(Path::new(name)),
                "expected non-media: {name}"
            );
        }
    }

    #[test]
    fn build_command_argv_for_format_pairs_matrix() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let pairs = [
            (OutputFormat::MP3, "out.mp3", &["-vn"][..]),
            (OutputFormat::WAV, "out.wav", &["-vn"][..]),
            (OutputFormat::FLAC, "out.flac", &["-vn"][..]),
            (OutputFormat::AAC, "out.aac", &["-vn"][..]),
            (OutputFormat::M4A, "out.m4a", &["-vn"][..]),
            (OutputFormat::OGG, "out.ogg", &["-vn"][..]),
            (OutputFormat::OPUS, "out.opus", &["-vn"][..]),
            (OutputFormat::AC3, "out.ac3", &["-vn"][..]),
            (OutputFormat::WMA, "out.wma", &["-vn"][..]),
            (OutputFormat::AIFF, "out.aiff", &["-vn"][..]),
            (OutputFormat::MP4, "out.mp4", &[][..]),
            (OutputFormat::WEBM, "out.webm", &["libvpx-vp9"][..]),
            (OutputFormat::MKV, "out.mkv", &[][..]),
            (OutputFormat::MOV, "out.mov", &[][..]),
            (OutputFormat::AVI, "out.avi", &[][..]),
            (OutputFormat::GIF, "out.gif", &["palette"][..]),
            (OutputFormat::M4V, "out.m4v", &[][..]),
            (OutputFormat::TS, "out.ts", &[][..]),
            (OutputFormat::PNG, "out.png", &["-frames:v"][..]),
            (OutputFormat::JPG, "out.jpg", &["-frames:v"][..]),
            (OutputFormat::WEBP, "out.webp", &["-frames:v"][..]),
            (OutputFormat::SRT, "out.srt", &["0:s:0"][..]),
            (OutputFormat::VTT, "out.vtt", &["0:s:0"][..]),
        ];
        for (format, out_name, expected_fragments) in pairs {
            let command = module
                .build_command(
                    Path::new("in.mp4"),
                    Path::new(out_name),
                    format,
                    &FfmpegOptions::default(),
                )
                .unwrap_or_else(|e| panic!("build {} failed: {e}", format.id()));
            let args: Vec<String> = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let joined = args.join(" ");
            assert!(
                args.iter().any(|a| a == "-i"),
                "{} missing -i: {joined}",
                format.id()
            );
            assert!(
                args.iter().any(|a| a == out_name
                    || a.ends_with(out_name)
                    || a.ends_with(&format!("/{out_name}"))),
                "{} missing output path: {joined}",
                format.id()
            );
            for frag in expected_fragments {
                assert!(
                    joined.contains(frag),
                    "{} expected fragment {frag:?} in {joined}",
                    format.id()
                );
            }
        }
    }

    #[test]
    fn encode_mode_and_quality_parse_matrix() {
        let mode_cases = [
            ("auto", FfmpegEncodeMode::Auto),
            ("copy", FfmpegEncodeMode::PreferCopy),
            ("stream-copy", FfmpegEncodeMode::PreferCopy),
            ("stream_copy", FfmpegEncodeMode::PreferCopy),
            ("reencode", FfmpegEncodeMode::Reencode),
            ("re-encode", FfmpegEncodeMode::Reencode),
            ("encode", FfmpegEncodeMode::Reencode),
            ("AUTO", FfmpegEncodeMode::Auto),
        ];
        for (input, expected) in mode_cases {
            assert_eq!(input.parse::<FfmpegEncodeMode>().unwrap(), expected);
        }
        for bad in ["", "turbo", "fast", "copy-ish"] {
            assert!(bad.parse::<FfmpegEncodeMode>().is_err(), "{bad}");
        }
        for mode in FfmpegEncodeMode::all() {
            assert_eq!(mode.id().parse::<FfmpegEncodeMode>().unwrap(), *mode);
            assert!(!mode.label().is_empty());
        }

        let quality_cases = [
            ("balanced", FfmpegQuality::Balanced),
            ("default", FfmpegQuality::Balanced),
            ("medium", FfmpegQuality::Balanced),
            ("high", FfmpegQuality::High),
            ("hq", FfmpegQuality::High),
            ("small", FfmpegQuality::Small),
            ("low", FfmpegQuality::Small),
            ("compact", FfmpegQuality::Small),
            ("HIGH", FfmpegQuality::High),
        ];
        for (input, expected) in quality_cases {
            assert_eq!(input.parse::<FfmpegQuality>().unwrap(), expected);
        }
        for bad in ["", "max", "lossless", "tiny"] {
            assert!(bad.parse::<FfmpegQuality>().is_err(), "{bad}");
        }
        for q in FfmpegQuality::all() {
            assert_eq!(q.id().parse::<FfmpegQuality>().unwrap(), *q);
            assert!(!q.label().is_empty());
        }
    }

    #[test]
    fn audio_quality_bitrate_presets_matrix() {
        let module = FfmpegModule::with_executable("ffmpeg");
        for (quality, needle) in [
            (FfmpegQuality::High, "320k"),
            (FfmpegQuality::Balanced, "192k"),
            (FfmpegQuality::Small, "96k"),
        ] {
            let command = module
                .build_command(
                    Path::new("in.wav"),
                    Path::new("out.mp3"),
                    OutputFormat::MP3,
                    &FfmpegOptions {
                        quality,
                        ..FfmpegOptions::default()
                    },
                )
                .unwrap();
            let args: Vec<String> = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(
                args.windows(2).any(|w| w == ["-b:a", needle]),
                "quality {:?} expected -b:a {needle}, got {args:?}",
                quality
            );
            assert!(args.windows(2).any(|w| w == ["-c:a", "libmp3lame"]));
        }
    }

    #[test]
    fn trim_start_on_image_uses_output_side_ss() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let command = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.png"),
                OutputFormat::PNG,
                &FfmpegOptions {
                    start_secs: Some(2.5),
                    ..FfmpegOptions::default()
                },
            )
            .unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // -ss should appear after -i for stills (output-side seek).
        let i_pos = args.iter().position(|a| a == "-i").unwrap();
        let ss_positions: Vec<_> = args
            .iter()
            .enumerate()
            .filter_map(|(idx, a)| (a == "-ss").then_some(idx))
            .collect();
        assert!(
            ss_positions.iter().any(|p| *p > i_pos),
            "expected output-side -ss after -i: {args:?}"
        );
    }

    #[test]
    fn module_metadata_and_outputs_match_media_catalog() {
        let module = FfmpegModule::with_executable("ffmpeg");
        assert_eq!(module.id(), "ffmpeg");
        assert!(!module.label().is_empty());
        assert_eq!(module.output_formats(), OutputFormat::MEDIA);
        for format in OutputFormat::MEDIA {
            assert!(module.supports(Path::new("clip.mp4"), *format));
        }
        assert!(!module.supports(Path::new("doc.docx"), OutputFormat::MP3));
        assert!(!module.supports_url(OutputFormat::MP3));
    }

    #[test]
    fn format_timestamp_matrix() {
        let cases = [
            (0.0, "0"),
            (1.0, "1"),
            (1.5, "1.500"),
            (12.3456, "12.346"),
            (12.3444, "12.344"),
            (100.0000000001, "100"),
        ];
        for (secs, expected) in cases {
            assert_eq!(format_timestamp(secs), expected, "secs={secs}");
        }
    }

    #[test]
    fn converts_more_format_pairs_with_fake_executable() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-pairs", std::process::id());
        let executable = directory.join(format!("shift-ffmpeg-pairs-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-pairs-{suffix}.mp4"));
        write_fake_ffmpeg(&executable);
        fs::write(&input, b"fake").unwrap();
        let module = FfmpegModule::with_executable(&executable);

        let formats = [
            OutputFormat::WAV,
            OutputFormat::FLAC,
            OutputFormat::AAC,
            OutputFormat::OGG,
            OutputFormat::OPUS,
            OutputFormat::MP4,
            OutputFormat::WEBM,
            OutputFormat::MKV,
            OutputFormat::GIF,
            OutputFormat::JPG,
            OutputFormat::WEBP,
            OutputFormat::VTT,
            OutputFormat::AIFF,
            OutputFormat::M4A,
        ];
        for format in formats {
            let artifact = module
                .convert(&input, format, &ConversionOptions::default())
                .unwrap_or_else(|e| panic!("convert to {} failed: {e}", format.id()));
            assert_eq!(artifact.format, format);
            assert_eq!(artifact.module_id, "ffmpeg");
            assert!(
                !artifact.bytes.is_empty(),
                "empty bytes for {}",
                format.id()
            );
            assert!(
                artifact
                    .file_name
                    .ends_with(&format!(".{}", format.extension())),
                "file_name {} for {}",
                artifact.file_name,
                format.id()
            );
        }

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn reencode_with_all_filters_builds_without_error() {
        let module = FfmpegModule::with_executable("ffmpeg");
        let options = FfmpegOptions {
            start_secs: Some(1.0),
            duration_secs: Some(5.0),
            encode_mode: FfmpegEncodeMode::Reencode,
            quality: FfmpegQuality::High,
            mono: true,
            sample_rate_hz: Some(44100),
            scale_width: Some(640),
            fps: Some(24.0),
            normalize_audio: true,
            ..FfmpegOptions::default()
        };
        let command = module
            .build_command(
                Path::new("in.mp4"),
                Path::new("out.mp4"),
                OutputFormat::MP4,
                &options,
            )
            .unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.contains("-ss"));
        assert!(joined.contains("-t"));
        assert!(joined.contains("scale=640:-2"));
        assert!(joined.contains("fps=24"));
        assert!(joined.contains("loudnorm"));
        assert!(joined.contains("-ac"));
        assert!(joined.contains("-ar"));
    }

    #[test]
    fn convert_with_progress_sink_emits_phase_and_progress_args() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-progress", std::process::id());
        let executable = directory.join(format!("shift-ffmpeg-test-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        // Fake that records args, writes a progress file briefly, then emits output.
        let script = r#"#!/bin/sh
set -e
printf '%s\n' "$*" > "${0}.args"
progress=""
output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -progress) progress="$2"; shift 2; continue ;;
    -stats_period) shift 2; continue ;;
    -i) shift 2; continue ;;
    -hide_banner|-nostdin|-y|-vn|-an) shift; continue ;;
    -loglevel|-ss|-t|-map|-c|-c:a|-c:v|-b:a|-b:v|-crf|-preset|-vf|-af|-frames:v|-q:v|-quality|-compression_level|-ac|-ar|-movflags) shift 2; continue ;;
    -*) shift; continue ;;
    *) output="$1"; shift; continue ;;
  esac
done
if [ -n "$progress" ]; then
  printf 'out_time_ms=500\nout_time_us=1500000\nprogress=end\n' > "$progress"
  sleep 0.3
fi
printf 'ID3fake-mp3' > "$output"
"#;
        fs::write(&executable, script).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        fs::write(&input, b"fake-video").unwrap();

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_cb = Arc::clone(&events);
        let options = ConversionOptions {
            ffmpeg: FfmpegOptions {
                duration_secs: Some(10.0),
                ..FfmpegOptions::default()
            },
            progress: Some(Arc::new(move |p| {
                events_cb.lock().unwrap().push(p);
            })),
            ..ConversionOptions::default()
        };

        let artifact = FfmpegModule::with_executable(&executable)
            .convert(&input, OutputFormat::MP3, &options)
            .unwrap();
        assert_eq!(artifact.bytes, b"ID3fake-mp3");

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("-progress"), "args: {args}");
        assert!(args.contains("-stats_period"), "args: {args}");

        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ConversionProgress::Phase(_))),
            "expected Phase events, got {events:?}"
        );
        // Progress watcher may or may not have scanned the file before stop;
        // phase emission from report_phase is the hard requirement.

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn png_sequence_honors_trim_and_rejects_failed_ffmpeg() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-seq-trim", std::process::id());
        let executable = directory.join(format!("shift-ffmpeg-test-{suffix}"));
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        write_fake_ffmpeg(&executable);
        fs::write(&input, b"fake-video").unwrap();

        let options = opts(FfmpegOptions {
            start_secs: Some(1.5),
            duration_secs: Some(2.0),
            frame_interval_secs: Some(0.5),
            scale_width: Some(320),
            ..FfmpegOptions::default()
        });
        let artifact = FfmpegModule::with_executable(&executable)
            .convert(&input, OutputFormat::PNG_SEQUENCE_ZIP, &options)
            .unwrap();
        assert_eq!(artifact.format, OutputFormat::PNG_SEQUENCE_ZIP);
        assert!(!artifact.bytes.is_empty());

        let args = fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("-ss"), "args: {args}");
        assert!(args.contains("-t"), "args: {args}");
        assert!(args.contains("fps="), "args: {args}");
        assert!(args.contains("scale=320:-2"), "args: {args}");

        // Failing fake for sequence extraction.
        let fail_exe = directory.join(format!("shift-ffmpeg-fail-{suffix}"));
        fs::write(&fail_exe, "#!/bin/sh\necho boom >&2\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&fail_exe).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fail_exe, permissions).unwrap();
        let err = FfmpegModule::with_executable(&fail_exe)
            .convert(
                &input,
                OutputFormat::PNG_SEQUENCE_ZIP,
                &opts(FfmpegOptions {
                    frame_interval_secs: Some(1.0),
                    ..FfmpegOptions::default()
                }),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("frame sequence") || err.to_string().contains("FFmpeg"),
            "error: {err}"
        );

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(format!("{}.args", executable.display()));
        let _ = fs::remove_file(&fail_exe);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn convert_failure_prefers_stderr_then_stdout_then_status() {
        let directory = std::env::temp_dir();
        let suffix = format!("{}-fail-detail", std::process::id());
        let input = directory.join(format!("shift-ffmpeg-input-{suffix}.mp4"));
        fs::write(&input, b"fake").unwrap();

        // stderr non-empty
        let exe_err = directory.join(format!("shift-ffmpeg-stderr-{suffix}"));
        fs::write(&exe_err, "#!/bin/sh\necho 'stderr-detail' >&2\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&exe_err).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&exe_err, permissions).unwrap();
        let err = FfmpegModule::with_executable(&exe_err)
            .convert(&input, OutputFormat::MP3, &ConversionOptions::default())
            .unwrap_err();
        assert!(err.to_string().contains("stderr-detail"), "error: {err}");

        // empty stderr, non-empty stdout
        let exe_out = directory.join(format!("shift-ffmpeg-stdout-{suffix}"));
        fs::write(&exe_out, "#!/bin/sh\necho 'stdout-only'\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&exe_out).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&exe_out, permissions).unwrap();
        let err = FfmpegModule::with_executable(&exe_out)
            .convert(&input, OutputFormat::MP3, &ConversionOptions::default())
            .unwrap_err();
        assert!(err.to_string().contains("stdout-only"), "error: {err}");

        // both empty
        let exe_empty = directory.join(format!("shift-ffmpeg-empty-{suffix}"));
        fs::write(&exe_empty, "#!/bin/sh\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&exe_empty).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&exe_empty, permissions).unwrap();
        let err = FfmpegModule::with_executable(&exe_empty)
            .convert(&input, OutputFormat::MP3, &ConversionOptions::default())
            .unwrap_err();
        assert!(
            err.to_string().contains("process exited") || err.to_string().contains("FFmpeg"),
            "error: {err}"
        );

        let _ = fs::remove_file(&input);
        let _ = fs::remove_file(&exe_err);
        let _ = fs::remove_file(&exe_out);
        let _ = fs::remove_file(&exe_empty);
    }
}
