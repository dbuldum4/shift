//! Shared application core used by both the native app and `shift-cli`.

pub mod artifact_cache;
pub mod conversion;
pub mod history;
pub mod preferences;
pub mod session_settings;

pub use artifact_cache::{
    DEFAULT_CACHE_MAX_BYTES, DEFAULT_CACHE_TTL, PurgeStats, artifact_cache_dir,
    cache_artifact_bytes, cache_artifact_file, ensure_artifact_cache_dir, purge_artifact_cache,
    purge_artifact_cache_defaults,
};
pub use session_settings::{
    SessionConversionOptions, SessionSettings, application_support_dir,
    default_session_settings_path, load_default_session_settings, load_session_settings,
    save_default_session_settings, save_session_settings,
};
