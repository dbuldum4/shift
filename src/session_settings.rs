//! Versioned session settings shared by the app and CLI (no secrets).

use crate::conversion::{
    BatchNamingTemplate, ConversionOptions, DefuddleOptions, DoclingAsrModel,
    DoclingImageExportMode, DoclingOptions, DoclingTableMode, DoclingVideoSamplingMode,
    FfmpegEncodeMode, FfmpegOptions, FfmpegQuality, MarkItDownOptions, OutputFormat, PandocOptions,
    PdfInputOptions, SipsFlip, SipsOptions, SpreadsheetOptions,
};
use crate::history::{DEFAULT_HISTORY_LIMIT, MAX_HISTORY_LIMIT, MIN_HISTORY_LIMIT};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SETTINGS_VERSION: u32 = 3;
const SETTINGS_FILE_NAME: &str = "session-settings.json";

/// Default history sidebar width in logical pixels (matches app constant).
pub const DEFAULT_HISTORY_SIDEBAR_WIDTH: f32 = 240.0;
/// Default output panel width in logical pixels (≈ half of remaining space at launch).
pub const DEFAULT_OUTPUT_PANEL_WIDTH: f32 = 470.0;
/// Default UI font family (Geist Mono, bundled with the app).
pub const DEFAULT_UI_FONT_FAMILY: &str = "Geist Mono";

fn default_history_sidebar_width() -> f32 {
    DEFAULT_HISTORY_SIDEBAR_WIDTH
}

fn default_output_panel_width() -> f32 {
    DEFAULT_OUTPUT_PANEL_WIDTH
}

fn default_ui_font_family() -> String {
    DEFAULT_UI_FONT_FAMILY.to_owned()
}

fn default_history_limit() -> usize {
    DEFAULT_HISTORY_LIMIT
}

fn default_show_archived() -> bool {
    false
}

fn default_batch_naming_template() -> String {
    BatchNamingTemplate::DEFAULT.to_owned()
}

/// Durable UI/CLI session knobs. Passwords are never stored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSettings {
    pub version: u32,
    pub output_format: String,
    pub batch_output_dir: Option<PathBuf>,
    pub batch_force: bool,
    /// Shared batch file-name template. Invalid legacy/manual values fall back
    /// to the safe default when the app initializes.
    #[serde(default = "default_batch_naming_template")]
    pub batch_naming_template: String,
    /// History sidebar width in logical pixels (main window layout).
    #[serde(default = "default_history_sidebar_width")]
    pub history_sidebar_width: f32,
    /// Output panel width in logical pixels (main window layout).
    #[serde(default = "default_output_panel_width")]
    pub output_panel_width: f32,
    /// UI font family name applied to the native app chrome.
    #[serde(default = "default_ui_font_family")]
    pub ui_font_family: String,
    /// Maximum number of conversion history entries to keep.
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
    /// Show archived entries in the history sidebar.
    #[serde(default = "default_show_archived")]
    pub show_archived: bool,
    /// Whether the native app's first-run guide has been completed.
    ///
    /// This is intentionally app-only: the CLI never loads session settings.
    #[serde(default)]
    pub onboarding_completed: bool,
    pub options: SessionConversionOptions,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            output_format: OutputFormat::MARKDOWN.id().to_owned(),
            batch_output_dir: None,
            batch_force: false,
            batch_naming_template: default_batch_naming_template(),
            history_sidebar_width: DEFAULT_HISTORY_SIDEBAR_WIDTH,
            output_panel_width: DEFAULT_OUTPUT_PANEL_WIDTH,
            ui_font_family: DEFAULT_UI_FONT_FAMILY.to_owned(),
            history_limit: DEFAULT_HISTORY_LIMIT,
            show_archived: false,
            onboarding_completed: false,
            options: SessionConversionOptions::default(),
        }
    }
}

impl SessionSettings {
    pub fn output_format(&self) -> OutputFormat {
        self.output_format.parse().unwrap_or(OutputFormat::MARKDOWN)
    }

    pub fn set_output_format(&mut self, format: OutputFormat) {
        self.output_format = format.id().to_owned();
    }

    /// Normalize empty/whitespace font values back to the default.
    pub fn resolved_ui_font_family(&self) -> &str {
        let trimmed = self.ui_font_family.trim();
        if trimmed.is_empty() {
            DEFAULT_UI_FONT_FAMILY
        } else {
            trimmed
        }
    }

    pub fn to_conversion_options(&self) -> ConversionOptions {
        self.options.to_conversion_options()
    }

    pub fn apply_conversion_options(&mut self, options: &ConversionOptions) {
        self.options = SessionConversionOptions::from_conversion_options(options);
    }
}

/// Nested engine knobs that may be persisted (excludes cancel/progress/password).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionConversionOptions {
    pub ffmpeg: SessionFfmpegOptions,
    pub markitdown: SessionMarkItDownOptions,
    pub pandoc: SessionPandocOptions,
    pub defuddle: SessionDefuddleOptions,
    pub docling: SessionDoclingOptions,
    /// Added after v1 shipped, so old settings files omit it.
    #[serde(default)]
    pub sips: SessionSipsOptions,
    /// Tabular sheet selection; added with the spreadsheet module.
    #[serde(default)]
    pub spreadsheet: SessionSpreadsheetOptions,
    pub pdf: SessionPdfInputOptions,
    /// Optional final-artifact size goal. Added after settings schema v2, so
    /// existing files deserialize to no target.
    #[serde(default)]
    pub target_size_bytes: Option<u64>,
}

impl SessionConversionOptions {
    pub fn from_conversion_options(options: &ConversionOptions) -> Self {
        Self {
            ffmpeg: SessionFfmpegOptions::from(&options.ffmpeg),
            markitdown: SessionMarkItDownOptions {
                keep_data_uris: options.markitdown.keep_data_uris,
            },
            pandoc: SessionPandocOptions::from(&options.pandoc),
            defuddle: SessionDefuddleOptions::from(&options.defuddle),
            docling: SessionDoclingOptions::from(&options.docling),
            sips: SessionSipsOptions::from(&options.sips),
            spreadsheet: SessionSpreadsheetOptions::from(&options.spreadsheet),
            pdf: SessionPdfInputOptions {
                // Never persist passwords.
                page_from: options.pdf.page_from,
                page_to: options.pdf.page_to,
                rotate_degrees: options.pdf.rotate_degrees,
                compression: options.pdf.compression.id().into(),
                linearize: options.pdf.linearize,
                split_pages: options.pdf.split_pages,
            },
            target_size_bytes: options.target_size_bytes,
        }
    }

    pub fn to_conversion_options(&self) -> ConversionOptions {
        ConversionOptions {
            ffmpeg: self.ffmpeg.to_ffmpeg_options(),
            markitdown: MarkItDownOptions {
                keep_data_uris: self.markitdown.keep_data_uris,
            },
            pandoc: self.pandoc.to_pandoc_options(),
            defuddle: self.defuddle.to_defuddle_options(),
            docling: self.docling.to_docling_options(),
            sips: self.sips.to_sips_options(),
            spreadsheet: self.spreadsheet.to_spreadsheet_options(),
            pdf: PdfInputOptions {
                password: None,
                page_from: self.pdf.page_from,
                page_to: self.pdf.page_to,
                rotate_degrees: self.pdf.rotate_degrees,
                compression: self.pdf.compression.parse().unwrap_or_default(),
                linearize: self.pdf.linearize,
                split_pages: self.pdf.split_pages,
            },
            target_size_bytes: self.target_size_bytes,
            cancel: None,
            progress: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionFfmpegOptions {
    pub start_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    pub frame_secs: Option<f64>,
    pub frame_interval_secs: Option<f64>,
    pub audio_stream: Option<u32>,
    pub subtitle_stream: Option<u32>,
    pub encode_mode: String,
    pub quality: String,
    pub mono: bool,
    pub sample_rate_hz: Option<u32>,
    pub scale_width: Option<u32>,
    pub fps: Option<f64>,
    pub mute: bool,
    pub normalize_audio: bool,
    pub burn_subtitles: bool,
}

impl From<&FfmpegOptions> for SessionFfmpegOptions {
    fn from(value: &FfmpegOptions) -> Self {
        Self {
            start_secs: value.start_secs,
            duration_secs: value.duration_secs,
            frame_secs: value.frame_secs,
            frame_interval_secs: value.frame_interval_secs,
            audio_stream: value.audio_stream,
            subtitle_stream: value.subtitle_stream,
            encode_mode: value.encode_mode.id().to_owned(),
            quality: value.quality.id().to_owned(),
            mono: value.mono,
            sample_rate_hz: value.sample_rate_hz,
            scale_width: value.scale_width,
            fps: value.fps,
            mute: value.mute,
            normalize_audio: value.normalize_audio,
            burn_subtitles: value.burn_subtitles,
        }
    }
}

impl SessionFfmpegOptions {
    fn to_ffmpeg_options(&self) -> FfmpegOptions {
        FfmpegOptions {
            start_secs: self.start_secs,
            duration_secs: self.duration_secs,
            frame_secs: self.frame_secs,
            frame_interval_secs: self.frame_interval_secs,
            audio_stream: self.audio_stream,
            subtitle_stream: self.subtitle_stream,
            encode_mode: self
                .encode_mode
                .parse()
                .unwrap_or(FfmpegEncodeMode::default()),
            quality: self.quality.parse().unwrap_or(FfmpegQuality::default()),
            mono: self.mono,
            sample_rate_hz: self.sample_rate_hz,
            scale_width: self.scale_width,
            fps: self.fps,
            mute: self.mute,
            normalize_audio: self.normalize_audio,
            burn_subtitles: self.burn_subtitles,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMarkItDownOptions {
    pub keep_data_uris: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPandocOptions {
    pub pdf_engine: Option<String>,
    pub standalone: bool,
    pub toc: bool,
    pub reference_doc: Option<PathBuf>,
    /// Opt-in Pandoc `@cite` parsing (off by default; see [`PandocOptions::citations`]).
    #[serde(default)]
    pub citations: bool,
}

impl From<&PandocOptions> for SessionPandocOptions {
    fn from(value: &PandocOptions) -> Self {
        Self {
            pdf_engine: value.pdf_engine.clone(),
            standalone: value.standalone,
            toc: value.toc,
            reference_doc: value.reference_doc.clone(),
            citations: value.citations,
        }
    }
}

impl SessionPandocOptions {
    fn to_pandoc_options(&self) -> PandocOptions {
        PandocOptions {
            pdf_engine: self.pdf_engine.clone(),
            standalone: self.standalone,
            toc: self.toc,
            reference_doc: self.reference_doc.clone(),
            citations: self.citations,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionDefuddleOptions {
    pub frontmatter: bool,
    pub lang: Option<String>,
}

impl From<&DefuddleOptions> for SessionDefuddleOptions {
    fn from(value: &DefuddleOptions) -> Self {
        Self {
            frontmatter: value.frontmatter,
            lang: value.lang.clone(),
        }
    }
}

impl SessionDefuddleOptions {
    fn to_defuddle_options(&self) -> DefuddleOptions {
        DefuddleOptions {
            frontmatter: self.frontmatter,
            lang: self.lang.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionDoclingOptions {
    pub image_export_mode: String,
    pub ocr: bool,
    pub ocr_lang: Option<String>,
    pub tables: bool,
    pub table_mode: String,
    #[serde(default = "default_docling_asr_model")]
    pub asr_model: String,
    #[serde(default = "default_docling_video_sampling_mode")]
    pub video_sampling_mode: String,
    #[serde(default = "default_docling_video_frame_interval")]
    pub video_frame_interval_secs: f64,
    #[serde(default)]
    pub video_cuts_per_minute: f64,
    #[serde(default)]
    pub video_prominence: f64,
    #[serde(default)]
    pub video_diarization: bool,
}

fn default_docling_asr_model() -> String {
    DoclingAsrModel::default().id().to_owned()
}

fn default_docling_video_sampling_mode() -> String {
    DoclingVideoSamplingMode::default().id().to_owned()
}

fn default_docling_video_frame_interval() -> f64 {
    DoclingOptions::default().video_frame_interval_secs
}

impl Default for SessionDoclingOptions {
    fn default() -> Self {
        let defaults = DoclingOptions::default();
        Self {
            image_export_mode: defaults.image_export_mode.id().to_owned(),
            ocr: defaults.ocr,
            ocr_lang: defaults.ocr_lang,
            tables: defaults.tables,
            table_mode: defaults.table_mode.id().to_owned(),
            asr_model: defaults.asr_model.id().to_owned(),
            video_sampling_mode: defaults.video_sampling_mode.id().to_owned(),
            video_frame_interval_secs: defaults.video_frame_interval_secs,
            video_cuts_per_minute: defaults.video_cuts_per_minute,
            video_prominence: defaults.video_prominence,
            video_diarization: defaults.video_diarization,
        }
    }
}

impl From<&DoclingOptions> for SessionDoclingOptions {
    fn from(value: &DoclingOptions) -> Self {
        Self {
            image_export_mode: value.image_export_mode.id().to_owned(),
            ocr: value.ocr,
            ocr_lang: value.ocr_lang.clone(),
            tables: value.tables,
            table_mode: value.table_mode.id().to_owned(),
            asr_model: value.asr_model.id().to_owned(),
            video_sampling_mode: value.video_sampling_mode.id().to_owned(),
            video_frame_interval_secs: value.video_frame_interval_secs,
            video_cuts_per_minute: value.video_cuts_per_minute,
            video_prominence: value.video_prominence,
            video_diarization: value.video_diarization,
        }
    }
}

impl SessionDoclingOptions {
    fn to_docling_options(&self) -> DoclingOptions {
        let defaults = DoclingOptions::default();
        DoclingOptions {
            image_export_mode: self
                .image_export_mode
                .parse()
                .unwrap_or(DoclingImageExportMode::default()),
            ocr: self.ocr,
            ocr_lang: self.ocr_lang.clone(),
            tables: self.tables,
            table_mode: self
                .table_mode
                .parse()
                .unwrap_or(DoclingTableMode::default()),
            asr_model: self.asr_model.parse().unwrap_or(DoclingAsrModel::default()),
            video_sampling_mode: self
                .video_sampling_mode
                .parse()
                .unwrap_or(DoclingVideoSamplingMode::default()),
            video_frame_interval_secs: if self.video_frame_interval_secs.is_finite()
                && self.video_frame_interval_secs > 0.0
            {
                self.video_frame_interval_secs
            } else {
                defaults.video_frame_interval_secs
            },
            video_cuts_per_minute: if self.video_cuts_per_minute.is_finite()
                && self.video_cuts_per_minute >= 0.0
            {
                self.video_cuts_per_minute
            } else {
                defaults.video_cuts_per_minute
            },
            video_prominence: if self.video_prominence.is_finite() && self.video_prominence >= 0.0 {
                self.video_prominence
            } else {
                defaults.video_prominence
            },
            video_diarization: self.video_diarization,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSipsOptions {
    pub max_dimension: Option<u32>,
    pub quality: String,
    pub rotate_degrees: Option<u32>,
    /// `horizontal` / `vertical`, or `None` for no mirror.
    pub flip: Option<String>,
    pub strip_color_profile: bool,
}

impl Default for SessionSipsOptions {
    fn default() -> Self {
        let defaults = SipsOptions::default();
        Self {
            max_dimension: defaults.max_dimension,
            quality: defaults.quality.id().to_owned(),
            rotate_degrees: defaults.rotate_degrees,
            flip: defaults.flip.map(|flip| flip.id().to_owned()),
            strip_color_profile: defaults.strip_color_profile,
        }
    }
}

impl From<&SipsOptions> for SessionSipsOptions {
    fn from(value: &SipsOptions) -> Self {
        Self {
            max_dimension: value.max_dimension,
            quality: value.quality.id().to_owned(),
            rotate_degrees: value.rotate_degrees,
            flip: value.flip.map(|flip| flip.id().to_owned()),
            strip_color_profile: value.strip_color_profile,
        }
    }
}

impl SessionSipsOptions {
    fn to_sips_options(&self) -> SipsOptions {
        SipsOptions {
            max_dimension: self.max_dimension,
            quality: self.quality.parse().unwrap_or_default(),
            rotate_degrees: self.rotate_degrees,
            // An unparseable persisted axis falls back to no flip rather than
            // silently mirroring the image.
            flip: self
                .flip
                .as_deref()
                .and_then(|value| value.parse::<SipsFlip>().ok()),
            strip_color_profile: self.strip_color_profile,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionSpreadsheetOptions {
    pub sheet_index: Option<u32>,
    pub sheet_name: Option<String>,
}

impl From<&SpreadsheetOptions> for SessionSpreadsheetOptions {
    fn from(value: &SpreadsheetOptions) -> Self {
        Self {
            sheet_index: value.sheet_index,
            sheet_name: value.sheet_name.clone(),
        }
    }
}

impl SessionSpreadsheetOptions {
    fn to_spreadsheet_options(&self) -> SpreadsheetOptions {
        SpreadsheetOptions {
            sheet_index: self.sheet_index.filter(|index| *index > 0),
            sheet_name: self
                .sheet_name
                .as_ref()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPdfInputOptions {
    pub page_from: Option<u32>,
    pub page_to: Option<u32>,
    #[serde(default)]
    pub rotate_degrees: Option<u16>,
    #[serde(default)]
    pub compression: String,
    #[serde(default)]
    pub linearize: bool,
    #[serde(default)]
    pub split_pages: Option<u32>,
}

/// Resolve the default session settings path under Application Support.
pub fn default_session_settings_path() -> Option<PathBuf> {
    application_support_dir().map(|dir| dir.join(SETTINGS_FILE_NAME))
}

/// Application Support / Shift directory (macOS) or XDG-ish fallback.
pub fn application_support_dir() -> Option<PathBuf> {
    if let Some(override_dir) = std::env::var_os("SHIFT_APP_SUPPORT_DIR") {
        return Some(PathBuf::from(override_dir));
    }
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/Shift"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(|xdg| PathBuf::from(xdg).join("shift"))
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/shift"))
            })
    }
}

/// Load session settings from `path`, or defaults when missing / invalid.
pub fn load_session_settings(path: impl AsRef<Path>) -> SessionSettings {
    let path = path.as_ref();
    let Ok(bytes) = fs::read(path) else {
        return SessionSettings::default();
    };
    match serde_json::from_slice::<SessionSettings>(&bytes) {
        Ok(mut settings) => {
            if settings.version < SETTINGS_VERSION {
                // Existing installations have already learned the core workflow;
                // reserve the first-run guide for genuinely new sessions.
                settings.onboarding_completed = true;
                settings.version = SETTINGS_VERSION;
            }
            settings.history_limit = settings
                .history_limit
                .clamp(MIN_HISTORY_LIMIT, MAX_HISTORY_LIMIT);
            // Future migrations can branch on version here.
            settings
        }
        Err(_) => SessionSettings::default(),
    }
}

/// Atomically write session settings to `path`.
pub fn save_session_settings(path: impl AsRef<Path>, settings: &SessionSettings) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut payload = settings.clone();
    payload.version = SETTINGS_VERSION;
    // Defensive: never serialize a password even if a caller stuffed one somehow.
    let json = serde_json::to_vec_pretty(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load from the default path (or defaults).
pub fn load_default_session_settings() -> SessionSettings {
    match default_session_settings_path() {
        Some(path) => load_session_settings(path),
        None => SessionSettings::default(),
    }
}

/// Save to the default path.
pub fn save_default_session_settings(settings: &SessionSettings) -> io::Result<()> {
    let path = default_session_settings_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate session settings directory",
        )
    })?;
    save_session_settings(path, settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn unique_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "shift-session-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            name
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    struct EnvGuard {
        home: Option<OsString>,
        app_support_dir: Option<OsString>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                home: std::env::var_os("HOME"),
                app_support_dir: std::env::var_os("SHIFT_APP_SUPPORT_DIR"),
            }
        }

        fn apply(&self, home: Option<&std::path::Path>, app_support_dir: Option<&std::path::Path>) {
            unsafe {
                if let Some(path) = home {
                    std::env::set_var("HOME", path.as_os_str());
                } else {
                    std::env::remove_var("HOME");
                }

                if let Some(path) = app_support_dir {
                    std::env::set_var("SHIFT_APP_SUPPORT_DIR", path.as_os_str());
                } else {
                    std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
                }
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.home.take() {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match self.app_support_dir.take() {
                    Some(value) => std::env::set_var("SHIFT_APP_SUPPORT_DIR", value),
                    None => std::env::remove_var("SHIFT_APP_SUPPORT_DIR"),
                }
            }
        }
    }

    #[test]
    fn round_trips_settings_without_password() {
        let dir = unique_dir("roundtrip");
        let path = dir.join("session-settings.json");

        let mut settings = SessionSettings::default();
        settings.set_output_format(OutputFormat::HTML);
        settings.batch_force = true;
        settings.batch_output_dir = Some(PathBuf::from("/tmp/out"));
        settings.batch_naming_template = "{parent}-{stem}.{ext}".into();
        settings.options.docling.ocr_lang = Some("eng".into());
        settings.options.pdf.page_from = Some(2);
        settings.options.pdf.page_to = Some(5);
        settings.options.ffmpeg.mute = true;
        settings.options.pandoc.reference_doc = Some(PathBuf::from("/refs/style.docx"));

        save_session_settings(&path, &settings).unwrap();
        let loaded = load_session_settings(&path);
        assert_eq!(loaded.output_format(), OutputFormat::HTML);
        assert!(loaded.batch_force);
        assert_eq!(loaded.batch_output_dir, Some(PathBuf::from("/tmp/out")));
        assert_eq!(
            loaded.batch_naming_template,
            "{parent}-{stem}.{ext}".to_owned()
        );
        assert_eq!(loaded.history_sidebar_width, DEFAULT_HISTORY_SIDEBAR_WIDTH);
        assert_eq!(loaded.output_panel_width, DEFAULT_OUTPUT_PANEL_WIDTH);
        assert_eq!(loaded.ui_font_family, DEFAULT_UI_FONT_FAMILY);
        assert_eq!(loaded.options.docling.ocr_lang.as_deref(), Some("eng"));
        assert_eq!(loaded.options.pdf.page_from, Some(2));
        assert!(loaded.options.ffmpeg.mute);

        let conversion = loaded.to_conversion_options();
        assert!(conversion.pdf.password.is_none());
        assert_eq!(conversion.pdf.page_from, Some(2));
        assert!(conversion.ffmpeg.mute);
        assert_eq!(
            conversion.pandoc.reference_doc,
            Some(PathBuf::from("/refs/style.docx"))
        );

        // Password in live options is stripped when applying to session.
        let mut live = ConversionOptions::default();
        live.pdf.password = Some("secret".into());
        live.pdf.page_from = Some(1);
        let mut settings2 = SessionSettings::default();
        settings2.apply_conversion_options(&live);
        let json = serde_json::to_string(&settings2).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("password"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = unique_dir("missing");
        let settings = load_session_settings(dir.join("nope.json"));
        assert_eq!(settings, SessionSettings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn panel_widths_round_trip_and_legacy_json_gets_defaults() {
        let dir = unique_dir("panel-widths");
        let path = dir.join("session-settings.json");

        let settings = SessionSettings {
            history_sidebar_width: 300.0,
            output_panel_width: 520.0,
            ui_font_family: "SF Mono".into(),
            ..Default::default()
        };
        save_session_settings(&path, &settings).unwrap();
        let loaded = load_session_settings(&path);
        assert_eq!(loaded.history_sidebar_width, 300.0);
        assert_eq!(loaded.output_panel_width, 520.0);
        assert_eq!(loaded.ui_font_family, "SF Mono");
        assert_eq!(loaded.resolved_ui_font_family(), "SF Mono");

        // Older files without the new fields still load with defaults.
        let mut legacy = serde_json::to_value(SessionSettings::default()).expect("serialize");
        let obj = legacy.as_object_mut().expect("object");
        obj.remove("history_sidebar_width");
        obj.remove("output_panel_width");
        obj.remove("ui_font_family");
        obj.remove("batch_naming_template");
        fs::write(&path, serde_json::to_vec_pretty(&legacy).expect("json")).unwrap();
        let migrated = load_session_settings(&path);
        assert_eq!(
            migrated.history_sidebar_width,
            DEFAULT_HISTORY_SIDEBAR_WIDTH
        );
        assert_eq!(migrated.output_panel_width, DEFAULT_OUTPUT_PANEL_WIDTH);
        assert_eq!(migrated.ui_font_family, DEFAULT_UI_FONT_FAMILY);
        assert_eq!(migrated.batch_naming_template, BatchNamingTemplate::DEFAULT);

        let blank = SessionSettings {
            ui_font_family: "   ".into(),
            ..Default::default()
        };
        assert_eq!(blank.resolved_ui_font_family(), DEFAULT_UI_FONT_FAMILY);

        let _ = fs::remove_dir_all(dir);
    }

    /// Session knobs are re-serialized when UI options change; keep that path cheap.
    #[test]
    fn serialize_deserialize_session_settings_stays_within_budget() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        let mut settings = SessionSettings::default();
        settings.set_output_format(OutputFormat::MP3);
        settings.batch_force = true;
        settings.batch_output_dir = Some(PathBuf::from("/Users/me/Exports/shift-out"));
        settings.options.ffmpeg.mute = true;
        settings.options.ffmpeg.mono = true;
        settings.options.docling.ocr = true;
        settings.options.docling.ocr_lang = Some("eng+deu".into());
        settings.options.defuddle.frontmatter = true;
        settings.options.pandoc.toc = true;
        settings.options.pandoc.reference_doc = Some(PathBuf::from("/Users/me/Templates/ref.docx"));
        settings.options.markitdown.keep_data_uris = true;
        settings.options.pdf.page_from = Some(1);
        settings.options.pdf.page_to = Some(40);

        let start = Instant::now();
        for _ in 0..1_000 {
            let json = serde_json::to_vec(&settings).expect("serialize");
            let back: SessionSettings = serde_json::from_slice(&json).expect("deserialize");
            black_box(back.output_format());
            black_box(settings.to_conversion_options());
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed <= Duration::from_secs(2),
            "session settings serde×1k took {elapsed:?}"
        );
    }

    #[test]
    fn corrupt_json_returns_defaults() {
        let dir = unique_dir("corrupt");
        let path = dir.join("session-settings.json");
        fs::write(&path, b"{not valid json").unwrap();
        let loaded = load_session_settings(&path);
        assert_eq!(loaded, SessionSettings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn history_limit_clamps_on_load() {
        let dir = unique_dir("history-clamp");
        let path = dir.join("session-settings.json");

        let too_low = SessionSettings {
            history_limit: 0,
            ..Default::default()
        };
        save_session_settings(&path, &too_low).unwrap();
        let loaded = load_session_settings(&path);
        assert_eq!(loaded.history_limit, crate::history::MIN_HISTORY_LIMIT);

        let too_high = SessionSettings {
            history_limit: crate::history::MAX_HISTORY_LIMIT + 1,
            ..Default::default()
        };
        save_session_settings(&path, &too_high).unwrap();
        let loaded = load_session_settings(&path);
        assert_eq!(loaded.history_limit, crate::history::MAX_HISTORY_LIMIT);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn version_zero_rewritten_to_current() {
        let dir = unique_dir("version-zero");
        let path = dir.join("session-settings.json");

        let legacy = SessionSettings {
            version: 0,
            ..Default::default()
        };
        let json = serde_json::to_vec(&legacy).unwrap();
        fs::write(&path, json).unwrap();

        let loaded = load_session_settings(&path);
        assert_eq!(loaded.version, SETTINGS_VERSION);
        assert!(loaded.onboarding_completed);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v2_pdf_settings_without_toolkit_fields_migrate_to_defaults() {
        let dir = unique_dir("pdf-toolkit-v2");
        let path = dir.join("session-settings.json");
        let mut legacy = serde_json::to_value(SessionSettings {
            version: 2,
            ..Default::default()
        })
        .expect("serialize v2 settings");
        let pdf = legacy["options"]["pdf"]
            .as_object_mut()
            .expect("serialized PDF options");
        pdf.remove("rotate_degrees");
        pdf.remove("compression");
        pdf.remove("linearize");
        pdf.remove("split_pages");
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let migrated = load_session_settings(&path);
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(migrated.options.pdf, SessionPdfInputOptions::default());
        assert_eq!(
            migrated.to_conversion_options().pdf.compression,
            crate::conversion::PdfCompression::Preserve
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn onboarding_completion_round_trips_and_v1_sessions_are_not_interrupted() {
        let dir = unique_dir("onboarding");
        let path = dir.join("session-settings.json");

        let settings = SessionSettings {
            onboarding_completed: true,
            ..Default::default()
        };
        save_session_settings(&path, &settings).unwrap();
        assert!(load_session_settings(&path).onboarding_completed);

        let legacy = SessionSettings {
            version: 1,
            onboarding_completed: false,
            ..Default::default()
        };
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let migrated = load_session_settings(&path);
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert!(migrated.onboarding_completed);

        assert!(!SessionSettings::default().onboarding_completed);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_output_format_falls_back_to_markdown() {
        let settings = SessionSettings {
            output_format: "not-a-real-format".into(),
            ..Default::default()
        };
        assert_eq!(settings.output_format(), OutputFormat::MARKDOWN);
    }

    #[test]
    fn full_engine_options_round_trip() {
        let dir = unique_dir("full-options");
        let path = dir.join("session-settings.json");

        let mut settings = SessionSettings::default();
        settings.options.ffmpeg = SessionFfmpegOptions {
            start_secs: Some(1.5),
            duration_secs: Some(10.0),
            frame_secs: Some(0.5),
            frame_interval_secs: Some(0.25),
            audio_stream: Some(0),
            subtitle_stream: Some(1),
            encode_mode: "reencode".into(),
            quality: "small".into(),
            mono: true,
            sample_rate_hz: Some(44100),
            scale_width: Some(1280),
            fps: Some(30.0),
            mute: true,
            normalize_audio: true,
            burn_subtitles: true,
        };
        settings.options.markitdown.keep_data_uris = true;
        settings.options.pandoc = SessionPandocOptions {
            pdf_engine: Some("xelatex".into()),
            standalone: true,
            toc: true,
            reference_doc: Some(PathBuf::from("/tmp/ref.docx")),
            citations: true,
        };
        settings.options.defuddle = SessionDefuddleOptions {
            frontmatter: true,
            lang: Some("en".into()),
        };
        settings.options.docling = SessionDoclingOptions {
            image_export_mode: "referenced".into(),
            ocr: true,
            ocr_lang: Some("fra".into()),
            tables: true,
            table_mode: "accurate".into(),
            asr_model: "whisper_turbo".into(),
            video_sampling_mode: "scene".into(),
            video_frame_interval_secs: 4.5,
            video_cuts_per_minute: 3.0,
            video_prominence: 0.02,
            video_diarization: true,
        };
        settings.options.pdf = SessionPdfInputOptions {
            page_from: Some(3),
            page_to: Some(7),
            rotate_degrees: Some(90),
            compression: "lossless".into(),
            linearize: true,
            split_pages: Some(2),
        };
        settings.options.spreadsheet = SessionSpreadsheetOptions {
            sheet_index: Some(2),
            sheet_name: Some("Beta".into()),
        };

        save_session_settings(&path, &settings).unwrap();
        let loaded = load_session_settings(&path);
        assert_eq!(loaded, settings);

        let conversion = loaded.to_conversion_options();
        assert_eq!(conversion.ffmpeg.encode_mode, FfmpegEncodeMode::Reencode);
        assert_eq!(conversion.ffmpeg.quality, FfmpegQuality::Small);
        assert_eq!(conversion.ffmpeg.start_secs, Some(1.5));
        assert!(conversion.ffmpeg.mono);
        assert!(conversion.ffmpeg.mute);
        assert!(conversion.ffmpeg.normalize_audio);
        assert!(conversion.ffmpeg.burn_subtitles);
        assert!(conversion.markitdown.keep_data_uris);
        assert_eq!(conversion.pandoc.pdf_engine.as_deref(), Some("xelatex"));
        assert!(conversion.pandoc.standalone);
        assert!(conversion.pandoc.toc);
        assert_eq!(conversion.pdf.rotate_degrees, Some(90));
        assert_eq!(
            conversion.pdf.compression,
            crate::conversion::PdfCompression::Lossless
        );
        assert!(conversion.pdf.linearize);
        assert_eq!(conversion.pdf.split_pages, Some(2));
        assert_eq!(
            conversion.pandoc.reference_doc,
            Some(PathBuf::from("/tmp/ref.docx"))
        );
        assert!(conversion.pandoc.citations);
        assert!(conversion.defuddle.frontmatter);
        assert_eq!(conversion.defuddle.lang.as_deref(), Some("en"));
        assert_eq!(
            conversion.docling.image_export_mode,
            DoclingImageExportMode::Referenced
        );
        assert!(conversion.docling.ocr);
        assert_eq!(conversion.docling.ocr_lang.as_deref(), Some("fra"));
        assert!(conversion.docling.tables);
        assert_eq!(conversion.docling.table_mode, DoclingTableMode::Accurate);
        assert_eq!(conversion.docling.asr_model, DoclingAsrModel::Turbo);
        assert_eq!(
            conversion.docling.video_sampling_mode,
            DoclingVideoSamplingMode::Scene
        );
        assert_eq!(conversion.docling.video_frame_interval_secs, 4.5);
        assert_eq!(conversion.docling.video_cuts_per_minute, 3.0);
        assert_eq!(conversion.docling.video_prominence, 0.02);
        assert!(conversion.docling.video_diarization);
        assert_eq!(conversion.pdf.page_from, Some(3));
        assert_eq!(conversion.pdf.page_to, Some(7));
        assert_eq!(conversion.pdf.password, None);
        assert_eq!(conversion.spreadsheet.sheet_name.as_deref(), Some("Beta"));
        assert_eq!(conversion.spreadsheet.sheet_index, Some(2));

        // sheet_index 0 is invalid (1-based) and must not survive conversion options.
        let mut zero_index = loaded;
        zero_index.options.spreadsheet.sheet_index = Some(0);
        let stripped = zero_index.to_conversion_options();
        assert_eq!(stripped.spreadsheet.sheet_index, None);
        assert_eq!(stripped.spreadsheet.sheet_name.as_deref(), Some("Beta"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn app_support_dir_override_resolves_default_path() {
        let _lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let guard = EnvGuard::new();
        let override_dir = unique_dir("override");
        let fake_home = unique_dir("fake-home");
        guard.apply(Some(&fake_home), Some(&override_dir));

        assert_eq!(application_support_dir(), Some(override_dir.clone()));
        assert_eq!(
            default_session_settings_path(),
            Some(override_dir.join(SETTINGS_FILE_NAME))
        );

        let settings = SessionSettings {
            history_limit: 7,
            ..Default::default()
        };
        save_default_session_settings(&settings).unwrap();
        let loaded = load_default_session_settings();
        assert_eq!(loaded.history_limit, 7);

        let _ = fs::remove_dir_all(override_dir);
        let _ = fs::remove_dir_all(fake_home);
    }

    #[test]
    fn save_creates_parents_and_writes_pretty_json() {
        let dir = unique_dir("nested");
        let path = dir.join("deeply/nested/session-settings.json");
        let settings = SessionSettings {
            output_format: OutputFormat::HTML.id().into(),
            ..Default::default()
        };
        save_session_settings(&path, &settings).unwrap();

        assert!(path.exists());
        assert!(path.parent().unwrap().exists());

        let bytes = fs::read(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["version"].as_u64(), Some(SETTINGS_VERSION as u64));
        assert_eq!(value["output_format"].as_str(), Some("html"));
        assert_eq!(
            value["history_limit"].as_u64(),
            Some(crate::history::DEFAULT_HISTORY_LIMIT as u64)
        );
        assert!(bytes.contains(&b'\n'));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn history_limit_and_show_archived_round_trip_and_legacy_defaults() {
        let dir = unique_dir("history-legacy");
        let path = dir.join("session-settings.json");

        let settings = SessionSettings {
            history_limit: 7,
            show_archived: true,
            ..Default::default()
        };
        save_session_settings(&path, &settings).unwrap();
        let loaded = load_session_settings(&path);
        assert_eq!(loaded.history_limit, 7);
        assert!(loaded.show_archived);

        let mut legacy = serde_json::to_value(SessionSettings::default()).expect("serialize");
        let obj = legacy.as_object_mut().expect("object");
        obj.remove("history_limit");
        obj.remove("show_archived");
        fs::write(&path, serde_json::to_vec_pretty(&legacy).expect("json")).unwrap();
        let migrated = load_session_settings(&path);
        assert_eq!(
            migrated.history_limit,
            crate::history::DEFAULT_HISTORY_LIMIT
        );
        assert!(!migrated.show_archived);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn apply_conversion_options_strips_password_and_maps_ffmpeg_enums() {
        let mut live = ConversionOptions::default();
        live.ffmpeg.encode_mode = FfmpegEncodeMode::Reencode;
        live.ffmpeg.quality = FfmpegQuality::Small;
        live.ffmpeg.mono = true;
        live.pdf.password = Some("secret".into());
        live.pdf.page_from = Some(5);
        live.pdf.page_to = Some(9);

        let mut settings = SessionSettings::default();
        settings.apply_conversion_options(&live);

        assert_eq!(settings.options.ffmpeg.encode_mode, "reencode");
        assert_eq!(settings.options.ffmpeg.quality, "small");
        assert!(settings.options.ffmpeg.mono);
        assert_eq!(settings.options.pdf.page_from, Some(5));
        assert_eq!(settings.options.pdf.page_to, Some(9));

        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("password"));
        assert!(!json.contains("secret"));

        let conversion = settings.to_conversion_options();
        assert_eq!(conversion.ffmpeg.encode_mode, FfmpegEncodeMode::Reencode);
        assert_eq!(conversion.ffmpeg.quality, FfmpegQuality::Small);
        assert!(conversion.ffmpeg.mono);
        assert_eq!(conversion.pdf.password, None);
        assert_eq!(conversion.pdf.page_from, Some(5));
        assert_eq!(conversion.pdf.page_to, Some(9));
    }

    #[test]
    fn invalid_nested_enum_strings_fall_back_to_defaults_on_load() {
        let dir = unique_dir("bad-enums");
        let path = dir.join("session-settings.json");

        let mut settings = SessionSettings::default();
        settings.options.ffmpeg.encode_mode = "not-a-mode".into();
        settings.options.ffmpeg.quality = "ultra".into();
        settings.options.docling.image_export_mode = "bogus-export".into();
        settings.options.docling.table_mode = "turbo".into();

        // Persist the invalid strings as-is (serde does not validate).
        save_session_settings(&path, &settings).unwrap();
        let loaded = load_session_settings(&path);
        assert_eq!(loaded.options.ffmpeg.encode_mode, "not-a-mode");
        assert_eq!(loaded.options.ffmpeg.quality, "ultra");
        assert_eq!(loaded.options.docling.image_export_mode, "bogus-export");
        assert_eq!(loaded.options.docling.table_mode, "turbo");

        // Conversion mapping falls back to defaults for unknown enum ids.
        let conversion = loaded.to_conversion_options();
        assert_eq!(
            conversion.ffmpeg.encode_mode,
            FfmpegEncodeMode::default(),
            "invalid encode_mode should fall back"
        );
        assert_eq!(
            conversion.ffmpeg.quality,
            FfmpegQuality::default(),
            "invalid quality should fall back"
        );
        assert_eq!(
            conversion.docling.image_export_mode,
            DoclingImageExportMode::default(),
            "invalid image_export_mode should fall back"
        );
        assert_eq!(
            conversion.docling.table_mode,
            DoclingTableMode::default(),
            "invalid table_mode should fall back"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_default_fails_without_home_or_app_support() {
        let _lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let guard = EnvGuard::new();
        guard.apply(None, None);

        assert!(application_support_dir().is_none());
        assert!(default_session_settings_path().is_none());
        let err = save_default_session_settings(&SessionSettings::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("session settings"), "error: {err}");
    }

    #[test]
    fn load_default_without_path_returns_defaults() {
        let _lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let guard = EnvGuard::new();
        guard.apply(None, None);

        assert!(default_session_settings_path().is_none());
        let loaded = load_default_session_settings();
        assert_eq!(loaded, SessionSettings::default());
    }

    #[test]
    fn round_trips_every_ffmpeg_encode_mode_and_quality_string() {
        let encode_aliases = [
            ("auto", FfmpegEncodeMode::Auto),
            ("copy", FfmpegEncodeMode::PreferCopy),
            ("stream-copy", FfmpegEncodeMode::PreferCopy),
            ("stream_copy", FfmpegEncodeMode::PreferCopy),
            ("reencode", FfmpegEncodeMode::Reencode),
            ("re-encode", FfmpegEncodeMode::Reencode),
            ("encode", FfmpegEncodeMode::Reencode),
            ("  AUTO  ", FfmpegEncodeMode::Auto),
            ("Copy", FfmpegEncodeMode::PreferCopy),
        ];
        for (raw, expected) in encode_aliases {
            let settings = SessionSettings {
                options: SessionConversionOptions {
                    ffmpeg: SessionFfmpegOptions {
                        encode_mode: raw.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                settings.to_conversion_options().ffmpeg.encode_mode,
                expected,
                "encode_mode raw={raw:?}"
            );
        }

        let quality_aliases = [
            ("balanced", FfmpegQuality::Balanced),
            ("default", FfmpegQuality::Balanced),
            ("medium", FfmpegQuality::Balanced),
            ("high", FfmpegQuality::High),
            ("hq", FfmpegQuality::High),
            ("small", FfmpegQuality::Small),
            ("low", FfmpegQuality::Small),
            ("compact", FfmpegQuality::Small),
            ("  HIGH  ", FfmpegQuality::High),
            ("Small", FfmpegQuality::Small),
        ];
        for (raw, expected) in quality_aliases {
            let settings = SessionSettings {
                options: SessionConversionOptions {
                    ffmpeg: SessionFfmpegOptions {
                        quality: raw.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                settings.to_conversion_options().ffmpeg.quality,
                expected,
                "quality raw={raw:?}"
            );
        }

        // Canonical ids from the enums also round-trip through save/load.
        let dir = unique_dir("ffmpeg-enums-file");
        let path = dir.join("session-settings.json");
        for mode in FfmpegEncodeMode::all() {
            for quality in FfmpegQuality::all() {
                let mut settings = SessionSettings::default();
                settings.options.ffmpeg.encode_mode = mode.id().into();
                settings.options.ffmpeg.quality = quality.id().into();
                save_session_settings(&path, &settings).unwrap();
                let loaded = load_session_settings(&path);
                let conversion = loaded.to_conversion_options();
                assert_eq!(conversion.ffmpeg.encode_mode, *mode);
                assert_eq!(conversion.ffmpeg.quality, *quality);
            }
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn show_archived_false_and_true_round_trip() {
        let dir = unique_dir("show-archived");
        let path = dir.join("session-settings.json");

        for value in [false, true] {
            let settings = SessionSettings {
                show_archived: value,
                ..Default::default()
            };
            save_session_settings(&path, &settings).unwrap();
            let loaded = load_session_settings(&path);
            assert_eq!(loaded.show_archived, value, "show_archived={value}");
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn batch_output_dir_none_vs_unicode_path() {
        let dir = unique_dir("batch-dir");
        let path = dir.join("session-settings.json");

        let none = SessionSettings {
            batch_output_dir: None,
            ..Default::default()
        };
        save_session_settings(&path, &none).unwrap();
        assert_eq!(load_session_settings(&path).batch_output_dir, None);

        let unicode = PathBuf::from("/tmp/çıkış-klasörü/シフト");
        let some = SessionSettings {
            batch_output_dir: Some(unicode.clone()),
            ..Default::default()
        };
        save_session_settings(&path, &some).unwrap();
        assert_eq!(load_session_settings(&path).batch_output_dir, Some(unicode));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn version_future_still_loads_when_deserializable() {
        let dir = unique_dir("version-future");
        let path = dir.join("session-settings.json");

        let future = SessionSettings {
            version: SETTINGS_VERSION + 1,
            show_archived: true,
            history_limit: 11,
            ..Default::default()
        };
        // Write raw JSON so save_session_settings does not rewrite the future version.
        fs::write(&path, serde_json::to_vec_pretty(&future).unwrap()).unwrap();

        let loaded = load_session_settings(&path);
        assert_eq!(
            loaded.version,
            SETTINGS_VERSION + 1,
            "future version should be preserved on load"
        );
        assert!(loaded.show_archived);
        assert_eq!(loaded.history_limit, 11);
        assert_eq!(loaded.output_format(), OutputFormat::MARKDOWN);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn target_size_round_trips_and_old_json_defaults_to_none() {
        let options = ConversionOptions {
            target_size_bytes: Some(10_000_000),
            ..ConversionOptions::default()
        };
        let stored = SessionConversionOptions::from_conversion_options(&options);
        assert_eq!(stored.target_size_bytes, Some(10_000_000));
        assert_eq!(
            stored.to_conversion_options().target_size_bytes,
            Some(10_000_000)
        );

        let mut old_json = serde_json::to_value(SessionConversionOptions::default()).unwrap();
        old_json
            .as_object_mut()
            .unwrap()
            .remove("target_size_bytes");
        let old: SessionConversionOptions = serde_json::from_value(old_json).unwrap();
        assert_eq!(old.target_size_bytes, None);
    }
}
