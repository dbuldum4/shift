//! FFmpeg adapter: audio/video/image/subtitle conversion with optional encode knobs.

use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, OutputFormat,
    map_spawn_error, max_output_bytes, process_timeout, read_file_limited, resolve_tool_executable,
    run_command_cancellable,
};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
}

impl FfmpegOptions {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// True when options require decoding/filters (stream copy is not possible).
    pub fn forces_reencode(&self) -> bool {
        self.mono
            || self.sample_rate_hz.is_some()
            || self.scale_width.is_some()
            || self.quality != FfmpegQuality::Balanced
                && self.encode_mode != FfmpegEncodeMode::PreferCopy
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

        let work_dir = unique_temp_dir("shift-ffmpeg")?;
        let cleanup = TempDirGuard(work_dir.clone());
        let produced = work_dir.join(Self::output_file_name(stem, output_format));

        let command = self.build_command(input, &produced, output_format, &options.ffmpeg)?;
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
        apply_encode_settings(&mut command, output_format, options)?;

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

    // Video (and GIF): optional explicit audio stream pick.
    if let Some(index) = options.audio_stream {
        command.arg("-map").arg("0:v:0");
        command.arg("-map").arg(format!("0:a:{index}"));
    }
}

fn apply_encode_settings(
    command: &mut Command,
    output_format: OutputFormat,
    options: &FfmpegOptions,
) -> Result<(), ConversionError> {
    if is_subtitle_output(output_format) {
        // Let FFmpeg pick a subtitle encoder for the container (srt/webvtt).
        return Ok(());
    }

    let want_copy = options.encode_mode == FfmpegEncodeMode::PreferCopy
        && !is_image_output(output_format)
        && !options.mono
        && options.sample_rate_hz.is_none()
        && options.scale_width.is_none();

    if want_copy {
        command.arg("-c").arg("copy");
        return Ok(());
    }

    if options.encode_mode == FfmpegEncodeMode::PreferCopy {
        return Err(ConversionError::new(
            "stream copy cannot be combined with mono, sample-rate, scale, or still-image output; \
             choose Auto/Re-encode or clear those options",
        ));
    }

    // Filters first, then codecs.
    let mut filters = Vec::new();
    if let Some(width) = options.scale_width {
        if is_video_output(output_format) || is_image_output(output_format) {
            filters.push(format!("scale={width}:-2"));
        }
    }
    if output_format == OutputFormat::GIF {
        // Compact animated GIF; scale if not already requested.
        if options.scale_width.is_none() {
            match options.quality {
                FfmpegQuality::High => filters.push("fps=15,scale=640:-2:flags=lanczos".into()),
                FfmpegQuality::Balanced => filters.push("fps=10,scale=480:-2:flags=lanczos".into()),
                FfmpegQuality::Small => filters.push("fps=8,scale=320:-2:flags=lanczos".into()),
            }
        } else {
            filters.push("fps=10".into());
        }
    }
    if !filters.is_empty() {
        command.arg("-vf").arg(filters.join(","));
    }

    if is_audio_output(output_format) {
        apply_audio_encode(command, output_format, options);
    } else if is_image_output(output_format) {
        apply_image_encode(command, output_format, options);
    } else if is_video_output(output_format) {
        apply_video_encode(command, output_format, options);
    }

    if options.mono {
        command.arg("-ac").arg("1");
    }
    if let Some(rate) = options.sample_rate_hz {
        command.arg("-ar").arg(rate.to_string());
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
            command.arg("-c:a").arg("libopus");
            command.arg("-b:a").arg(audio_bitrate);
        }
        "mp4" | "m4v" | "mov" | "mkv" | "avi" | "mpeg" | "ts" | "3gp" => {
            command.arg("-c:v").arg("libx264");
            command.arg("-preset").arg(match options.quality {
                FfmpegQuality::High => "slow",
                FfmpegQuality::Balanced => "medium",
                FfmpegQuality::Small => "veryfast",
            });
            command.arg("-crf").arg(crf);
            command.arg("-c:a").arg(if output_format.id() == "mpeg" {
                "mp2"
            } else {
                "aac"
            });
            command.arg("-b:a").arg(audio_bitrate);
            if matches!(output_format.id(), "mp4" | "m4v" | "mov") {
                command.arg("-movflags").arg("+faststart");
            }
            if output_format.id() == "3gp" {
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
    -loglevel|-ss|-t|-map|-c|-c:a|-c:v|-b:a|-b:v|-crf|-preset|-vf|-frames:v|-q:v|-quality|-compression_level|-ac|-ar|-movflags) shift 2; continue ;;
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
            cancel: None,
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
        assert!(!module.supports(Path::new("report.docx"), OutputFormat::MP3));
        assert!(input_looks_like_media(Path::new("a.webm")));
        assert!(!input_looks_like_media(Path::new("a.docx")));
    }
}
