//! sips adapter: still-image conversion through macOS ImageIO.
//!
//! `sips` ships with macOS, so this module needs no install step. It covers the
//! image families no other engine reads — HEIC/HEIF (iPhone photos), AVIF, SVG,
//! JPEG XL, and camera RAW — and writes the still formats ImageIO reports as
//! writable. WEBP is deliberately absent: ImageIO decodes it but cannot encode
//! it, so that output stays with FFmpeg.

use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, InvocationRecord,
    OutputFormat, TempDirGuard, command_argv_parts, format_argv_display, map_spawn_error,
    max_output_bytes, process_timeout, read_file_limited, resolve_tool_executable,
    run_command_cancellable, unique_temp_dir,
};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Image types ImageIO decodes. Verified against `sips --formats` (sips-316).
///
/// The RAW block is a bonus no other module reads; `sips` normalizes it through
/// the same pipeline as everything else.
const EXTENSIONS: &[&str] = &[
    // Modern still formats (the gap this module exists to close)
    "heic", "heif", "heics", "avif", "avci", "svg", "jxl", // Common raster
    "png", "jpg", "jpeg", "tiff", "tif", "bmp", "gif", "webp", "ico", "jp2", "psd", "tga", "exr",
    "sgi", "pict", "pbm", // Camera RAW
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "raf", "rw2", "pef",
    "dcr", "mrw", "mos", "erf", "iiq", "srw", "3fr", "fff", "rwl",
];

/// Still formats ImageIO writes, restricted to Shift's catalog.
///
/// Every entry here is selectable through the `sips -s format` CLI. ImageIO's
/// `sips --formats` output is broader than this: on current macOS it marks HEIC
/// and AVIF writable, but the CLI cannot select those encoders and produces no
/// file. Keep those formats out of dispatch until sips exposes a working
/// command-line identifier.
const OUTPUTS: &[OutputFormat] = &[
    OutputFormat::PNG,
    OutputFormat::JPG,
    OutputFormat::TIFF,
    OutputFormat::GIF,
    OutputFormat::BMP,
    OutputFormat::JP2,
    OutputFormat::ICNS,
    OutputFormat::PDF,
];

/// Raster outputs another module can consume after a first hop.
///
/// `PDF` is excluded: a rasterized page is a worse input for the document
/// engines than the original image. `ICNS` is excluded because it only encodes
/// at icon dimensions.
const CHAINABLE: &[OutputFormat] = &[
    OutputFormat::PNG,
    OutputFormat::JPG,
    OutputFormat::TIFF,
    OutputFormat::GIF,
    OutputFormat::BMP,
];

/// Encoder quality for lossy destinations (`-s formatOptions`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SipsQuality {
    #[default]
    Balanced,
    High,
    Small,
}

impl SipsQuality {
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

    /// Value passed to `sips -s formatOptions`.
    pub fn format_option(self) -> &'static str {
        match self {
            Self::Balanced => "normal",
            Self::High => "best",
            Self::Small => "low",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Balanced, Self::High, Self::Small]
    }
}

impl std::str::FromStr for SipsQuality {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "balanced" | "default" | "medium" | "normal" => Ok(Self::Balanced),
            "high" | "hq" | "best" => Ok(Self::High),
            "small" | "low" | "compact" => Ok(Self::Small),
            other => Err(ConversionError::new(format!(
                "unknown sips quality: {other} (try balanced, high, small)"
            ))),
        }
    }
}

/// Mirror axis for `sips --flip`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SipsFlip {
    Horizontal,
    Vertical,
}

impl SipsFlip {
    pub fn id(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Horizontal => "Horizontal",
            Self::Vertical => "Vertical",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Horizontal, Self::Vertical]
    }
}

impl std::str::FromStr for SipsFlip {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "horizontal" | "h" | "x" => Ok(Self::Horizontal),
            "vertical" | "v" | "y" => Ok(Self::Vertical),
            other => Err(ConversionError::new(format!(
                "unknown sips flip axis: {other} (try horizontal, vertical)"
            ))),
        }
    }
}

/// Optional knobs for still-image conversion. Default is a plain re-encode.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SipsOptions {
    /// Fit the image inside a square of this many pixels (`-Z`), preserving
    /// aspect ratio. Never upscales.
    pub max_dimension: Option<u32>,
    pub quality: SipsQuality,
    /// Clockwise rotation in degrees (`-r`).
    pub rotate_degrees: Option<u32>,
    pub flip: Option<SipsFlip>,
    /// Drop the embedded color profile (`--deleteColorManagementProperties`).
    pub strip_color_profile: bool,
}

impl SipsOptions {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Clone, Debug)]
pub struct SipsModule {
    executable: OsString,
}

impl Default for SipsModule {
    fn default() -> Self {
        Self {
            executable: discover_executable(),
        }
    }
}

fn discover_executable() -> OsString {
    // sips is part of macOS; /usr/bin is checked explicitly because GUI
    // processes can launch with a trimmed PATH.
    resolve_tool_executable("SHIFT_SIPS_BIN", "sips", &[PathBuf::from("/usr/bin/sips")])
}

impl SipsModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    /// Value for `sips -s format`. Differs from the file extension for JPEG.
    fn format_arg(output_format: OutputFormat) -> Option<&'static str> {
        match output_format.id() {
            "jpg" => Some("jpeg"),
            "png" => Some("png"),
            "tiff" => Some("tiff"),
            "gif" => Some("gif"),
            "bmp" => Some("bmp"),
            // sips rejects the familiar `jp2` alias even though its help text
            // lists JPEG 2000 as writable; ImageIO's UTI is required.
            "jp2" => Some("public.jpeg-2000"),
            "icns" => Some("icns"),
            "pdf" => Some("pdf"),
            _ => None,
        }
    }

    /// Lossy destinations where `formatOptions` changes the result.
    fn honors_quality(output_format: OutputFormat) -> bool {
        sips_supports_target_size_output(output_format)
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
        let format_arg = Self::format_arg(output_format).ok_or_else(|| {
            ConversionError::new(format!("sips does not write {}", output_format.label()))
        })?;

        let stem = input
            .file_stem()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsStr::new("converted"));

        let work_dir = unique_temp_dir("shift-sips")?;
        let cleanup = TempDirGuard(work_dir.clone());
        let produced = work_dir.join(Self::output_file_name(stem, output_format));

        // `--out` writes a new file; the source is never modified in place.
        let knobs = &options.sips;
        let quality_attempts: Vec<String> = if options.target_size_bytes.is_some() {
            // ImageIO accepts numeric percentages for lossy encoders. Try from
            // highest to lowest so the first artifact under the cap is also
            // the best-quality artifact among these deterministic passes.
            [92, 82, 72, 62, 52, 42, 32, 22, 12, 5]
                .into_iter()
                .map(|quality| quality.to_string())
                .collect()
        } else {
            vec![knobs.quality.format_option().to_owned()]
        };
        let mut invocations = Vec::new();
        let mut fitted = None;
        let mut smallest = usize::MAX;

        for (attempt, quality) in quality_attempts.iter().enumerate() {
            let _ = fs::remove_file(&produced);
            let mut command = Command::new(&self.executable);
            command.arg("-s").arg("format").arg(format_arg);
            if Self::honors_quality(output_format) {
                command.arg("-s").arg("formatOptions").arg(quality);
            }
            if let Some(max) = knobs.max_dimension.filter(|value| *value > 0) {
                command.arg("-Z").arg(max.to_string());
            }
            if let Some(degrees) = knobs.rotate_degrees.filter(|value| value % 360 != 0) {
                command.arg("-r").arg((degrees % 360).to_string());
            }
            if let Some(flip) = knobs.flip {
                command.arg("-f").arg(flip.id());
            }
            if knobs.strip_color_profile {
                command.arg("--deleteColorManagementProperties");
            }
            command.arg(input).arg("--out").arg(&produced);

            invocations.push(InvocationRecord {
                module_id: self.id(),
                argv_display: format_argv_display(&command_argv_parts(&command)),
            });
            if let Some(sink) = options.progress.as_ref() {
                sink(super::ConversionProgress::Phase(format!(
                    "Image fit pass {}…",
                    attempt + 1
                )));
            }

            let output = run_command_cancellable(
                command,
                process_timeout(),
                max_output_bytes(),
                options.cancel.clone(),
            )
            .map_err(|error| {
                map_spawn_error(
                    error,
                    "sips could not be launched. It ships with macOS at /usr/bin/sips; \
                     set SHIFT_SIPS_BIN to override.",
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
                    "sips could not convert {}: {detail}{}",
                    input.display(),
                    Self::failure_hint(output_format)
                )));
            }

            // sips exits 0 even when it silently declines to write some formats,
            // so treat a missing file as a failure rather than an empty artifact.
            let bytes = read_file_limited(&produced, max_output_bytes()).map_err(|error| {
                ConversionError::new(format!(
                    "sips finished but did not write {}: {error}{}",
                    produced.display(),
                    Self::failure_hint(output_format)
                ))
            })?;
            smallest = smallest.min(bytes.len());
            if options
                .target_size_bytes
                .is_none_or(|target| bytes.len() as u64 <= target)
            {
                fitted = Some(bytes);
                break;
            }
        }
        let bytes = fitted.ok_or_else(|| {
            ConversionError::new(format!(
                "image could not fit under {} bytes (smallest attempt was {} bytes); \
                 choose a larger target or a smaller max dimension",
                options.target_size_bytes.unwrap_or_default(),
                smallest
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

    /// Extra guidance for destinations with encoder-side constraints.
    fn failure_hint(output_format: OutputFormat) -> &'static str {
        match output_format.id() {
            "icns" => {
                " (ICNS requires square icon dimensions; try setting a max dimension of 128, 256, 512, or 1024)"
            }
            _ => "",
        }
    }
}

pub fn sips_supports_target_size_output(output_format: OutputFormat) -> bool {
    matches!(output_format.id(), "jpg" | "jp2")
}

impl ConversionModule for SipsModule {
    fn id(&self) -> &'static str {
        "sips"
    }

    fn label(&self) -> &'static str {
        "sips"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        EXTENSIONS
    }

    fn output_formats(&self) -> &[OutputFormat] {
        OUTPUTS
    }

    fn chainable_output_formats(&self) -> &[OutputFormat] {
        CHAINABLE
    }

    fn supports_target_size(&self, output: OutputFormat) -> bool {
        Self::honors_quality(output)
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !OUTPUTS.contains(&output_format) {
            return Err(ConversionError::new(format!(
                "sips does not write {}",
                output_format.label()
            )));
        }
        self.convert_with_cli(input, output_format, options)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_suffix(tag: &str) -> String {
        format!(
            "{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Mimics `sips`: records argv, then writes the `--out` file.
    fn write_fake_sips(path: &Path) {
        let script = r#"#!/bin/sh
printf '%s\n' "$*" > "${0}.args"
out=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--out" ]; then
    out="$arg"
    prev=""
    continue
  fi
  case "$arg" in
    --out) prev="--out" ;;
  esac
done
[ -z "$out" ] && exit 1
printf '%s' "fake-image-bytes" > "$out"
"#;
        fs::write(path, script).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    /// Exits 0 without writing anything, like ImageIO declining a destination.
    fn write_silent_sips(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    struct Fixture {
        executable: PathBuf,
        input: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str, input_ext: &str) -> Self {
            let directory = std::env::temp_dir();
            let suffix = unique_suffix(tag);
            let executable = directory.join(format!("shift-sips-test-{suffix}"));
            let input = directory.join(format!("shift-sips-input-{suffix}.{input_ext}"));
            write_fake_sips(&executable);
            fs::write(&input, b"fake source image").unwrap();
            Self { executable, input }
        }

        fn args(&self) -> String {
            fs::read_to_string(format!("{}.args", self.executable.display())).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.executable);
            let _ = fs::remove_file(format!("{}.args", self.executable.display()));
            let _ = fs::remove_file(&self.input);
        }
    }

    #[test]
    fn converts_heic_to_jpg_and_names_output_by_stem() {
        let fixture = Fixture::new("heic", "heic");
        let artifact = SipsModule::with_executable(&fixture.executable)
            .convert(
                &fixture.input,
                OutputFormat::JPG,
                &ConversionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            artifact.file_name,
            format!(
                "{}.jpg",
                fixture.input.file_stem().unwrap().to_string_lossy()
            )
        );
        assert_eq!(artifact.media_type, "image/jpeg");
        assert_eq!(artifact.bytes, b"fake-image-bytes");
        assert_eq!(artifact.module_id, "sips");
        assert_eq!(artifact.format, OutputFormat::JPG);
        assert_eq!(artifact.pipeline, vec!["sips"]);
        assert_eq!(artifact.invocations.len(), 1);

        // `jpg` must be translated to the ImageIO name `jpeg`.
        let args = fixture.args();
        assert!(args.contains("-s format jpeg"), "args: {args}");
        assert!(args.contains("--out"), "args: {args}");
    }

    #[test]
    fn target_size_uses_highest_fitting_numeric_quality() {
        let fixture = Fixture::new("fit", "heic");
        let options = ConversionOptions {
            target_size_bytes: Some(16 * 1024),
            ..ConversionOptions::default()
        };
        let artifact = SipsModule::with_executable(&fixture.executable)
            .convert(&fixture.input, OutputFormat::JPG, &options)
            .unwrap();

        assert!(artifact.bytes.len() <= 16 * 1024);
        assert_eq!(artifact.invocations.len(), 1);
        let args = fixture.args();
        assert!(args.contains("-s formatOptions 92"), "args: {args}");
    }

    #[test]
    fn target_size_capabilities_are_limited_to_lossy_writers() {
        let module = SipsModule::with_executable("/bin/true");
        assert!(module.supports_target_size(OutputFormat::JPG));
        assert!(module.supports_target_size(OutputFormat::JP2));
        assert!(!module.supports_target_size(OutputFormat::PNG));
        assert!(!module.supports_target_size(OutputFormat::TIFF));
        assert!(!module.supports_target_size(OutputFormat::PDF));
    }

    #[test]
    fn source_file_is_never_modified() {
        let fixture = Fixture::new("preserve", "png");
        let before = fs::read(&fixture.input).unwrap();
        SipsModule::with_executable(&fixture.executable)
            .convert(
                &fixture.input,
                OutputFormat::TIFF,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert_eq!(fs::read(&fixture.input).unwrap(), before);
    }

    #[test]
    fn options_map_onto_sips_flags() {
        let fixture = Fixture::new("opts", "png");
        let options = ConversionOptions {
            sips: SipsOptions {
                max_dimension: Some(512),
                quality: SipsQuality::Small,
                rotate_degrees: Some(90),
                flip: Some(SipsFlip::Vertical),
                strip_color_profile: true,
            },
            ..Default::default()
        };
        SipsModule::with_executable(&fixture.executable)
            .convert(&fixture.input, OutputFormat::JPG, &options)
            .unwrap();

        let args = fixture.args();
        assert!(args.contains("-Z 512"), "args: {args}");
        assert!(args.contains("-s formatOptions low"), "args: {args}");
        assert!(args.contains("-r 90"), "args: {args}");
        assert!(args.contains("-f vertical"), "args: {args}");
        assert!(
            args.contains("--deleteColorManagementProperties"),
            "args: {args}"
        );
    }

    #[test]
    fn quality_is_omitted_for_lossless_destinations() {
        let fixture = Fixture::new("lossless", "jpg");
        SipsModule::with_executable(&fixture.executable)
            .convert(
                &fixture.input,
                OutputFormat::PNG,
                &ConversionOptions::default(),
            )
            .unwrap();
        let args = fixture.args();
        assert!(
            !args.contains("formatOptions"),
            "PNG is lossless, so formatOptions should not be sent: {args}"
        );
    }

    #[test]
    fn no_op_rotation_is_not_sent() {
        let fixture = Fixture::new("spin", "png");
        let options = ConversionOptions {
            sips: SipsOptions {
                rotate_degrees: Some(360),
                ..Default::default()
            },
            ..Default::default()
        };
        SipsModule::with_executable(&fixture.executable)
            .convert(&fixture.input, OutputFormat::TIFF, &options)
            .unwrap();
        // Match the flag as a whole argument; temp paths can contain "-r".
        let args = fixture.args();
        assert!(
            !args.split_whitespace().any(|part| part == "-r"),
            "args: {args}"
        );
    }

    #[test]
    fn missing_output_file_is_an_error_even_when_sips_exits_zero() {
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("silent");
        let executable = directory.join(format!("shift-sips-silent-{suffix}"));
        let input = directory.join(format!("shift-sips-silent-in-{suffix}.png"));
        write_silent_sips(&executable);
        fs::write(&input, b"fake").unwrap();

        let error = SipsModule::with_executable(&executable)
            .convert(&input, OutputFormat::ICNS, &ConversionOptions::default())
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("did not write"), "error: {message}");
        // ICNS has a size constraint, so the failure should say so.
        assert!(message.contains("square icon"), "error: {message}");

        let _ = fs::remove_file(&executable);
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn rejects_output_formats_outside_the_declared_set() {
        let module = SipsModule::with_executable("/nonexistent/sips");
        let error = module
            .convert(
                Path::new("photo.png"),
                OutputFormat::WEBP,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("does not write"));
    }

    #[test]
    fn capability_lists_are_self_consistent() {
        let module = SipsModule::with_executable("/nonexistent/sips");

        // WEBP and ICO decode but do not encode, so they may appear as inputs
        // and must never appear as outputs.
        assert!(module.input_extensions().contains(&"webp"));
        assert!(!module.output_formats().contains(&OutputFormat::WEBP));
        assert!(module.input_extensions().contains(&"ico"));

        // Every declared output must have a `-s format` mapping.
        for format in module.output_formats() {
            assert!(
                SipsModule::format_arg(*format).is_some(),
                "no sips format name for {}",
                format.id()
            );
        }
        // Chainable outputs must be a subset of the declared outputs.
        for format in module.chainable_output_formats() {
            assert!(
                module.output_formats().contains(format),
                "{} is chainable but not an output",
                format.id()
            );
        }
        // A rasterized PDF is a poor input for the document engines.
        assert!(
            !module
                .chainable_output_formats()
                .contains(&OutputFormat::PDF)
        );

        // The formats this module exists to unlock.
        for extension in ["heic", "svg", "avif", "jxl", "cr3"] {
            assert!(
                module.supports(Path::new(&format!("a.{extension}")), OutputFormat::JPG),
                "{extension} → jpg should be supported"
            );
        }
        // Case-insensitive extension matching (iPhone exports are uppercase).
        assert!(module.supports(Path::new("IMG_0001.HEIC"), OutputFormat::JPG));
    }
}
