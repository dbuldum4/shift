//! Versioned session settings shared by the app and CLI (no secrets).

use crate::conversion::{
    ConversionOptions, DefuddleOptions, DoclingImageExportMode, DoclingOptions, DoclingTableMode,
    FfmpegEncodeMode, FfmpegOptions, FfmpegQuality, MarkItDownOptions, OutputFormat, PandocOptions,
    PdfInputOptions,
};
use crate::history::{DEFAULT_HISTORY_LIMIT, MAX_HISTORY_LIMIT, MIN_HISTORY_LIMIT};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SETTINGS_VERSION: u32 = 1;
const SETTINGS_FILE_NAME: &str = "session-settings.json";

/// Default history sidebar width in logical pixels (matches app constant).
pub const DEFAULT_HISTORY_SIDEBAR_WIDTH: f32 = 240.0;
/// Default output panel width in logical pixels (≈ half of remaining space at launch).
pub const DEFAULT_OUTPUT_PANEL_WIDTH: f32 = 470.0;
/// Default UI font family (Geist sans, bundled with the app).
pub const DEFAULT_UI_FONT_FAMILY: &str = "Geist";

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

/// Durable UI/CLI session knobs. Passwords are never stored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSettings {
    pub version: u32,
    pub output_format: String,
    pub batch_output_dir: Option<PathBuf>,
    pub batch_force: bool,
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
    pub options: SessionConversionOptions,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            output_format: OutputFormat::MARKDOWN.id().to_owned(),
            batch_output_dir: None,
            batch_force: false,
            history_sidebar_width: DEFAULT_HISTORY_SIDEBAR_WIDTH,
            output_panel_width: DEFAULT_OUTPUT_PANEL_WIDTH,
            ui_font_family: DEFAULT_UI_FONT_FAMILY.to_owned(),
            history_limit: DEFAULT_HISTORY_LIMIT,
            show_archived: false,
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
    pub pdf: SessionPdfInputOptions,
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
            pdf: SessionPdfInputOptions {
                // Never persist passwords.
                page_from: options.pdf.page_from,
                page_to: options.pdf.page_to,
            },
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
            pdf: PdfInputOptions {
                password: None,
                page_from: self.pdf.page_from,
                page_to: self.pdf.page_to,
            },
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
        }
    }
}

impl SessionDoclingOptions {
    fn to_docling_options(&self) -> DoclingOptions {
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
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPdfInputOptions {
    pub page_from: Option<u32>,
    pub page_to: Option<u32>,
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
            if settings.version == 0 {
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
        fs::write(&path, serde_json::to_vec_pretty(&legacy).expect("json")).unwrap();
        let migrated = load_session_settings(&path);
        assert_eq!(
            migrated.history_sidebar_width,
            DEFAULT_HISTORY_SIDEBAR_WIDTH
        );
        assert_eq!(migrated.output_panel_width, DEFAULT_OUTPUT_PANEL_WIDTH);
        assert_eq!(migrated.ui_font_family, DEFAULT_UI_FONT_FAMILY);

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
        };
        settings.options.pdf = SessionPdfInputOptions {
            page_from: Some(3),
            page_to: Some(7),
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
        assert_eq!(conversion.pdf.page_from, Some(3));
        assert_eq!(conversion.pdf.page_to, Some(7));
        assert_eq!(conversion.pdf.password, None);

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
        assert_eq!(value["version"].as_u64(), Some(1));
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
}
