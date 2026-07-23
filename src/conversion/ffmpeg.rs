//! FFmpeg adapter: audio/video/image/subtitle conversion with optional encode knobs.

use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, ConversionProgress,
    InvocationRecord, OutputFormat, command_argv_parts, format_argv_display, map_spawn_error,
    max_output_bytes, process_timeout, read_file_limited, resolve_tool_executable,
    run_command_cancellable,
};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

/// Broad demux surface FFmpeg handles without exotic builds.
const INPUTS: &[&str] = &[
    // Audio
    "aac", "ac3", "aif", "aiff", "amr", "ape", "caf", "dts", "eac3", "flac", "m4a", "mp3", "mpc",
    "oga", "ogg", "opus", "spx", "wav", "wma", // Video / containers
    "3gp", "asf", "avi", "divx", "flv", "gif", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg",
    "mts", "mxf", "rm", "rmvb", "ts", "vob", "webm",
    "wmv", // Stills (slideshow / image→image)
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
    matches!(format.id(), "png" | "jpg")
}

pub fn is_subtitle_output(format: OutputFormat) -> bool {
    matches!(format.id(), "srt" | "vtt")
}

#[derive(Clone, Debug)]
pub struct FfmpegModule {
    executable: OsString,
}

impl Default for FfmpegModule {
    fn default() -> Self {
        Self {
            // Absolute path when found so GUI apps with a minimal PATH match
            // diagnostics readiness (PATH + common_bin_dirs).
            executable: resolve_tool_executable("SHIFT_FFMPEG_BIN", "ffmpeg", &[]),
        }
    }
}

impl FfmpegModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
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
        if !OUTPUTS.contains(&output_format) {
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

        let mut command = self.build_command(input, &produced, output_format, &options.ffmpeg)?;
        let progress_path = if options.progress.is_some() {
            let path = work_dir.join("ffmpeg-progress.txt");
            // Truncate so a stale file cannot confuse the reader.
            fs::write(&path, b"").ok();
            command.arg("-progress").arg(&path);
            command.arg("-stats_period").arg("0.5");
            Some(path)
        } else {
            None
        };

        let invocation = InvocationRecord {
            module_id: self.id(),
            argv_display: format_argv_display(&command_argv_parts(&command)),
        };

        report_phase(options, "FFmpeg converting…");
        let progress_stop = spawn_progress_watcher(progress_path.clone(), options);

        let output = run_command_cancellable(
            command,
            process_timeout(),
            max_output_bytes(),
            options.cancel.clone(),
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

        let bytes = read_file_limited(&produced, max_output_bytes()).map_err(|error| {
            ConversionError::new(format!(
                "FFmpeg finished but did not write {}: {error}",
                produced.display()
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
            invocations: vec![invocation],
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
        command.arg("-i").arg(input);
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
        command.arg(&pattern);

        let invocation = InvocationRecord {
            module_id: self.id(),
            argv_display: format_argv_display(&command_argv_parts(&command)),
        };

        report_phase(options, "FFmpeg extracting frames…");
        let output = run_command_cancellable(
            command,
            process_timeout(),
            max_output_bytes(),
            options.cancel.clone(),
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

    fn build_command(
        &self,
        input: &Path,
        produced: &Path,
        output_format: OutputFormat,
        options: &FfmpegOptions,
    ) -> Result<Command, ConversionError> {
        validate_options(options)?;

        let mut command = Command::new(&self.executable);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y");

        // Input-side seek is faster for long media.
        let input_seek = if is_image_output(output_format) {
            options.frame_secs.or(options.start_secs)
        } else {
            options.start_secs
        };
        if let Some(secs) = input_seek {
            command.arg("-ss").arg(format_timestamp(secs));
        }

        command.arg("-i").arg(input);

        if let Some(secs) = options.duration_secs {
            if !is_image_output(output_format) && !is_subtitle_output(output_format) {
                command.arg("-t").arg(format_timestamp(secs));
            }
        }

        apply_stream_maps(&mut command, output_format, options);
        apply_encode_settings(&mut command, input, output_format, options)?;

        command.arg(produced);
        Ok(command)
    }
}

fn validate_options(options: &FfmpegOptions) -> Result<(), ConversionError> {
    for (label, value) in [
        ("start", options.start_secs),
        ("duration", options.duration_secs),
        ("frame", options.frame_secs),
    ] {
        if let Some(secs) = value {
            if !secs.is_finite() || secs < 0.0 {
                return Err(ConversionError::new(format!(
                    "FFmpeg {label} must be a non-negative number of seconds"
                )));
            }
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

struct ProgressWatchStop(std::sync::Arc<std::sync::atomic::AtomicBool>);

fn spawn_progress_watcher(
    progress_path: Option<PathBuf>,
    options: &ConversionOptions,
) -> Option<ProgressWatchStop> {
    let sink = options.progress.clone()?;
    let path = progress_path?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = std::sync::Arc::clone(&stop);
    let duration_hint = options.ffmpeg.duration_secs;
    thread::spawn(move || {
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
    Some(ProgressWatchStop(stop))
}

fn stop_progress_watcher(stop: Option<ProgressWatchStop>) {
    if let Some(ProgressWatchStop(flag)) = stop {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
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
) -> Result<(), ConversionError> {
    if is_subtitle_output(output_format) {
        // Let FFmpeg pick a subtitle encoder for the container (srt/webvtt).
        return Ok(());
    }

    let want_copy = options.encode_mode == FfmpegEncodeMode::PreferCopy
        && !is_image_output(output_format)
        && !options.forces_reencode();

    if want_copy {
        command.arg("-c").arg("copy");
        return Ok(());
    }

    if options.encode_mode == FfmpegEncodeMode::PreferCopy {
        return Err(ConversionError::new(
            "stream copy cannot be combined with mono, sample-rate, scale, fps, mute, \
             normalize-audio, burn-subtitles, frame interval, or still-image output; \
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
    if !filters.is_empty() {
        command.arg("-vf").arg(filters.join(","));
    }

    // Audio filters (loudnorm).
    if options.normalize_audio
        && (is_audio_output(output_format) || (is_video_output(output_format) && !options.mute))
    {
        command.arg("-af").arg("loudnorm");
    }

    if is_audio_output(output_format) {
        apply_audio_encode(command, output_format, options);
    } else if is_image_output(output_format) {
        apply_image_encode(command, output_format, options);
    } else if is_video_output(output_format) {
        apply_video_encode(command, output_format, options);
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

fn apply_audio_encode(command: &mut Command, output_format: OutputFormat, options: &FfmpegOptions) {
    let bitrate = match options.quality {
        FfmpegQuality::High => "320k",
        FfmpegQuality::Balanced => "192k",
        FfmpegQuality::Small => "96k",
    };
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
            command.arg("-b:a").arg(bitrate);
        }
        "aac" | "m4a" => {
            command.arg("-c:a").arg("aac");
            command.arg("-b:a").arg(bitrate);
        }
        "caf" => {
            command.arg("-c:a").arg("pcm_s16le");
        }
        "ogg" | "opus" => {
            command.arg("-c:a").arg("libopus");
            command.arg("-b:a").arg(bitrate);
        }
        "ac3" => {
            command.arg("-c:a").arg("ac3");
            command.arg("-b:a").arg(bitrate);
        }
        "wma" => {
            command.arg("-c:a").arg("wmav2");
            command.arg("-b:a").arg(bitrate);
        }
        "aiff" => {
            command.arg("-c:a").arg("pcm_s16be");
        }
        _ => {
            command.arg("-b:a").arg(bitrate);
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
        _ => {}
    }
}

fn apply_video_encode(command: &mut Command, output_format: OutputFormat, options: &FfmpegOptions) {
    if output_format == OutputFormat::GIF {
        // Palette-based GIF is more complex; fps/scale filters already applied.
        return;
    }

    let crf = match options.quality {
        FfmpegQuality::High => "18",
        FfmpegQuality::Balanced => "23",
        FfmpegQuality::Small => "28",
    };
    let audio_bitrate = match options.quality {
        FfmpegQuality::High => "192k",
        FfmpegQuality::Balanced => "128k",
        FfmpegQuality::Small => "96k",
    };

    match output_format.id() {
        "webm" => {
            command.arg("-c:v").arg("libvpx-vp9");
            command.arg("-crf").arg(crf);
            command.arg("-b:v").arg("0");
            if !options.mute {
                command.arg("-c:a").arg("libopus");
                command.arg("-b:a").arg(audio_bitrate);
            }
        }
        "mp4" | "m4v" | "mov" | "mkv" | "avi" | "mpeg" | "ts" | "3gp" => {
            command.arg("-c:v").arg("libx264");
            command.arg("-preset").arg(match options.quality {
                FfmpegQuality::High => "slow",
                FfmpegQuality::Balanced => "medium",
                FfmpegQuality::Small => "veryfast",
            });
            command.arg("-crf").arg(crf);
            if !options.mute {
                command.arg("-c:a").arg(if output_format.id() == "mpeg" {
                    "mp2"
                } else {
                    "aac"
                });
                command.arg("-b:a").arg(audio_bitrate);
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
        }
        _ => {
            command.arg("-crf").arg(crf);
        }
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

    fn output_formats(&self) -> &'static [OutputFormat] {
        OUTPUTS
    }

    fn chainable_output_formats(&self) -> &'static [OutputFormat] {
        CHAINABLE
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

fn unique_temp_dir(prefix: &str) -> Result<PathBuf, ConversionError> {
    let base = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&base).map_err(|error| {
        ConversionError::new(format!(
            "could not create temporary directory {}: {error}",
            base.display()
        ))
    })?;
    Ok(base)
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_fake_ffmpeg(path: &Path) {
        let script = r#"#!/bin/sh
set -e
printf '%s\n' "$*" > "${0}.args"
output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -i) shift 2; continue ;;
    -hide_banner|-nostdin|-y|-vn|-an) shift; continue ;;
    -loglevel|-ss|-t|-map|-c|-c:a|-c:v|-b:a|-b:v|-crf|-preset|-vf|-af|-frames:v|-q:v|-quality|-compression_level|-ac|-ar|-movflags|-progress|-stats_period) shift 2; continue ;;
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
}
