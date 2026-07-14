//! Versioned session settings shared by the app and CLI (no secrets).

use crate::conversion::{
    ConversionOptions, DefuddleOptions, DoclingImageExportMode, DoclingOptions, DoclingTableMode,
    FfmpegEncodeMode, FfmpegOptions, FfmpegQuality, MarkItDownOptions, OutputFormat, PandocOptions,
    PdfInputOptions,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SETTINGS_VERSION: u32 = 1;
const SETTINGS_FILE_NAME: &str = "session-settings.json";

/// Durable UI/CLI session knobs. Passwords are never stored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSettings {
    pub version: u32,
    pub output_format: String,
    pub batch_output_dir: Option<PathBuf>,
    pub batch_force: bool,
    pub options: SessionConversionOptions,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            output_format: OutputFormat::MARKDOWN.id().to_owned(),
            batch_output_dir: None,
            batch_force: false,
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
}

impl From<&PandocOptions> for SessionPandocOptions {
    fn from(value: &PandocOptions) -> Self {
        Self {
            pdf_engine: value.pdf_engine.clone(),
            standalone: value.standalone,
            toc: value.toc,
            reference_doc: value.reference_doc.clone(),
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
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/Shift"))
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
}
