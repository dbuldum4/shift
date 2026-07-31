//! Shared application core used by both the native app and `shift-cli`.

pub mod artifact_cache;
pub mod conversion;
pub mod history;
pub mod preferences;
pub mod recipes;
pub mod session_settings;

pub use artifact_cache::{
    ArtifactLease, DEFAULT_CACHE_MAX_BYTES, DEFAULT_CACHE_TTL, PurgeStats, acquire_export_lease,
    artifact_cache_dir, cache_artifact_bytes, cache_artifact_file, default_paste_staging_dir,
    ensure_artifact_cache_dir, export_matches_bytes, export_matches_bytes_strict,
    purge_artifact_cache, purge_artifact_cache_defaults, purge_now, purge_paste_staging,
    stage_export_bytes, stage_export_file, verify_export_integrity,
    verify_export_integrity_blocking,
};
pub use recipes::{
    ConversionRecipe, RecipeDestination, RecipeError, RecipeStore, default_recipe_store_path,
    load_default_recipe_store, load_recipe_store, save_default_recipe_store, save_recipe_store,
    validate_naming_template, validate_recipe_name,
};
pub use session_settings::{
    MAX_SETTINGS_FILE_BYTES, SessionConversionOptions, SessionSettings, SessionSettingsLoad,
    application_support_dir, default_session_settings_path, load_default_session_settings,
    load_default_session_settings_detailed, load_session_settings, load_session_settings_detailed,
    quarantine_settings_file, save_default_session_settings, save_session_settings,
};

/// Serializes `shift_core` tests that mutate process-global state: env vars
/// (`HOME`, `PATH`, `TMPDIR`, `SHIFT_*`), and `std::env::set_current_dir`.
/// Every env-mutating test in this crate must hold this lock for the full
/// mutation window and restore via `Drop` (not only on the success path).
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
