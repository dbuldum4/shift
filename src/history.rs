//! Persistent conversion history backed by SQLite.
//!
//! History lives under Application Support in a SQLite database. Legacy binary
//! blobs are imported once in a single transaction and only moved aside after
//! the imported row count is verified.
//!
//! # Retention
//!
//! - At most [`MAX_HISTORY_LIMIT`] rows are kept on disk (hard cap). The UI
//!   default is [`DEFAULT_HISTORY_LIMIT`] and is configurable per session.
//! - Each retained artifact payload is capped at [`MAX_HISTORY_ARTIFACT_BYTES`];
//!   larger conversions store metadata only (`ReadyLarge`).
//! - Total stored artifact BLOB bytes across all rows are capped at
//!   [`MAX_HISTORY_TOTAL_ARTIFACT_BYTES`]. Oldest payloads are dropped first
//!   (promoted to `ReadyLarge`) when the budget is exceeded.
//! - Clearing history removes the SQLite store, the legacy blob, and
//!   `history.legacy.bak`, then optionally VACUUMs if a store remains.
//!
//! # Privacy
//!
//! Support directories are created with mode `0700` and the database file with
//! `0600` on Unix. Artifact BLOBs are lazy-loaded: list/search paths read
//! metadata only; full bytes are fetched on demand.

use crate::conversion::redact_url_credentials;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, params, params_from_iter,
};
use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Full artifact bytes retained per history entry; larger results store metadata only.
pub const MAX_HISTORY_ARTIFACT_BYTES: usize = 512 * 1024;
/// Aggregate artifact BLOB budget across the entire history store.
pub const MAX_HISTORY_TOTAL_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;
/// Default cap for retained history entries.
pub const DEFAULT_HISTORY_LIMIT: usize = 30;
/// Minimum persisted history limit.
pub const MIN_HISTORY_LIMIT: usize = 1;
/// Maximum persisted history limit (hard cap enforced on save).
pub const MAX_HISTORY_LIMIT: usize = 500;
/// Kept for callers that used the older constant name.
pub const MAX_HISTORY_ENTRIES: usize = DEFAULT_HISTORY_LIMIT;

/// Bound the legacy blob before reading it into memory during migration.
pub const MAX_LEGACY_HISTORY_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Bound each legacy text field before allocating its decoded buffer.
const MAX_LEGACY_FIELD_BYTES: usize = 1024 * 1024;

/// Max automatic save retries after a failed history persist (app layer).
pub const HISTORY_SAVE_MAX_RETRIES: u32 = 6;
/// Base delay for exponential backoff on history save failure (milliseconds).
pub const HISTORY_SAVE_BASE_DELAY_MS: u64 = 250;

const MAGIC: &[u8] = b"SHIFT_HISTORY_V1\n";
/// Prefix for non-UTF-8 source paths stored as hex-encoded OS bytes.
const OS_PATH_PREFIX: &str = "os:";
const LEGACY_MIGRATION_KEY: &str = "legacy-history-v1-imported";

static HISTORY_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static HISTORY_STORE_EPOCH: AtomicU64 = AtomicU64::new(0);

fn history_store_lock() -> &'static Mutex<()> {
    HISTORY_STORE_LOCK.get_or_init(|| Mutex::new(()))
}

/// Return the current in-process history-store generation.
///
/// Background saves capture this value. A clear operation increments it before
/// taking the store lock, so a save queued before the clear cannot recreate the
/// deleted database after the clear completes.
pub fn history_store_epoch() -> u64 {
    HISTORY_STORE_EPOCH.load(Ordering::SeqCst)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoredSource {
    File(PathBuf),
    Url(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoredOutcome {
    Ready {
        module_id: String,
        file_name: String,
        format: String,
        bytes: Vec<u8>,
    },
    ReadyLarge {
        module_id: String,
        byte_len: usize,
    },
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredHistoryEntry {
    pub id: u64,
    pub source: StoredSource,
    pub name: String,
    pub detail: String,
    pub extension_label: String,
    pub badge_color: u32,
    pub badge_text_color: u32,
    pub output_format: String,
    pub outcome: StoredOutcome,
    pub archived: bool,
    /// When true, a Ready row has artifact bytes on disk that were not loaded.
    pub artifact_deferred: bool,
}

impl StoredHistoryEntry {
    /// Construct an entry with `artifact_deferred = false` (normal in-memory form).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        source: StoredSource,
        name: String,
        detail: String,
        extension_label: String,
        badge_color: u32,
        badge_text_color: u32,
        output_format: String,
        outcome: StoredOutcome,
        archived: bool,
    ) -> Self {
        Self {
            id,
            source,
            name,
            detail,
            extension_label,
            badge_color,
            badge_text_color,
            output_format,
            outcome,
            archived,
            artifact_deferred: false,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadedHistory {
    pub entries: Vec<StoredHistoryEntry>,
    pub next_id: u64,
    /// Human-readable load/schema error to surface in the UI (if any).
    pub load_error: Option<String>,
    /// When true, the in-memory list may be incomplete; callers must not treat
    /// an empty list as a successful wipe signal and must preserve `next_id`.
    pub load_incomplete: bool,
}

/// Application Support path for the SQLite history store, when HOME is available.
pub fn history_db_path() -> Option<PathBuf> {
    support_dir().map(|dir| dir.join("history.sqlite"))
}

/// Path to the legacy binary history blob.
fn history_legacy_path() -> Option<PathBuf> {
    support_dir().map(|dir| dir.join("history"))
}

/// Path to the legacy backup created after a successful migration.
fn history_legacy_bak_path() -> Option<PathBuf> {
    support_dir().map(|dir| dir.join("history.legacy.bak"))
}

/// Parent Application Support directory used by preferences and history.
pub fn support_dir() -> Option<PathBuf> {
    if let Some(override_dir) = std::env::var_os("SHIFT_APP_SUPPORT_DIR") {
        return Some(PathBuf::from(override_dir));
    }
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library/Application Support/Shift"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(|xdg| PathBuf::from(xdg).join("shift"))
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/shift"))
            })
    }
}

fn home_dir_for_history() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Ensure `dir` exists with mode 0700 on Unix.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    reject_symlinked_directory(dir)?;
    std::fs::create_dir_all(dir)?;
    reject_symlinked_directory(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(dir, perms)?;
    }
    Ok(())
}

fn reject_symlinked_directory(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing symlinked history directory: {}", path.display()),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("history path is not a directory: {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Restrict an existing file to mode 0600 on Unix.
fn ensure_private_file(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing symlinked history database: {}", path.display()),
        ));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("history database is not a regular file: {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = metadata.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn canonical_history_open_path(path: &Path) -> Result<PathBuf, rusqlite::Error> {
    if path == Path::new(":memory:") {
        return Ok(path.to_path_buf());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent)
        .map_err(|_| rusqlite::Error::InvalidPath(path.to_path_buf()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| rusqlite::Error::InvalidPath(path.to_path_buf()))?;
    Ok(parent.join(file_name))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = from_hex_digit(bytes[i])?;
        let lo = from_hex_digit(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn from_hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Store a file source without lossy trim. Absolute paths under the user's home
/// directory use a `~/` prefix (UTF-8 only). Non-UTF-8 paths use an `os:` + hex
/// encoding of the raw OS bytes so restore is lossless on Unix.
fn store_source_path(path: &Path) -> String {
    if let Some(home) = home_dir_for_history() {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_owned();
            }
            if let Some(s) = rest.to_str() {
                return format!("~/{s}");
            }
            // Non-UTF-8 path under home: store full path as os-bytes.
            return path_to_os_encoded(path);
        }
    }
    if let Some(s) = path.to_str() {
        return s.to_owned();
    }
    path_to_os_encoded(path)
}

fn path_to_os_encoded(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        format!(
            "{OS_PATH_PREFIX}{}",
            encode_hex(path.as_os_str().as_bytes())
        )
    }
    #[cfg(not(unix))]
    {
        // Windows/other: fall back to lossy only when OsStr is not UTF-8.
        format!(
            "{OS_PATH_PREFIX}{}",
            encode_hex(path.to_string_lossy().as_bytes())
        )
    }
}

/// Reverse [`store_source_path`]. Does **not** trim whitespace (paths may
/// legitimately start or end with spaces on some systems).
fn restore_source_path(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = home_dir_for_history() {
            return home;
        }
        return PathBuf::from("~");
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home_dir_for_history() {
            return home.join(rest);
        }
        return PathBuf::from(raw);
    }
    if let Some(hex) = raw.strip_prefix(OS_PATH_PREFIX) {
        if let Some(bytes) = decode_hex(hex) {
            #[cfg(unix)]
            {
                use std::ffi::OsStr;
                use std::os::unix::ffi::OsStrExt;
                return PathBuf::from(OsStr::from_bytes(&bytes));
            }
            #[cfg(not(unix))]
            {
                if let Ok(s) = std::str::from_utf8(&bytes) {
                    return PathBuf::from(s);
                }
            }
        }
    }
    PathBuf::from(raw)
}

/// Map a stored module id onto a known static string.
pub fn intern_module_id(id: &str) -> &'static str {
    match id {
        "markitdown" => "markitdown",
        "pandoc" => "pandoc",
        "defuddle" => "defuddle",
        "docling" => "docling",
        "spreadsheet" => "spreadsheet",
        "sips" => "sips",
        "ffmpeg" => "ffmpeg",
        "qpdf" => "qpdf",
        _ => "unknown",
    }
}

/// All registered conversion module ids that must intern successfully.
pub const REGISTERED_MODULE_IDS: &[&str] = &[
    "markitdown",
    "pandoc",
    "defuddle",
    "docling",
    "spreadsheet",
    "sips",
    "ffmpeg",
    "qpdf",
];

fn initialize_history_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_kind INTEGER NOT NULL,
            source TEXT NOT NULL,
            name TEXT NOT NULL,
            detail TEXT NOT NULL,
            extension_label TEXT NOT NULL,
            badge_color INTEGER NOT NULL,
            badge_text_color INTEGER NOT NULL,
            output_format TEXT NOT NULL,
            module_id TEXT,
            file_name TEXT,
            format TEXT,
            artifact_bytes BLOB,
            byte_len INTEGER,
            error_message TEXT,
            outcome_kind INTEGER NOT NULL,
            archived INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        );

        CREATE INDEX IF NOT EXISTS idx_history_created ON history(created_at DESC, id DESC);

        CREATE TABLE IF NOT EXISTS history_id_seq (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            next_id INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS history_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;
    migrate_history_to_autoincrement(conn)?;
    sync_id_seq_from_history(conn)?;
    Ok(())
}

/// Rebuild the history table with AUTOINCREMENT when an older schema is present.
fn migrate_history_to_autoincrement(conn: &Connection) -> Result<(), rusqlite::Error> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'history'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Ok(());
    };
    if sql.to_ascii_uppercase().contains("AUTOINCREMENT") {
        return Ok(());
    }
    conn.execute_batch(
        "
        BEGIN IMMEDIATE;
        CREATE TABLE history_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_kind INTEGER NOT NULL,
            source TEXT NOT NULL,
            name TEXT NOT NULL,
            detail TEXT NOT NULL,
            extension_label TEXT NOT NULL,
            badge_color INTEGER NOT NULL,
            badge_text_color INTEGER NOT NULL,
            output_format TEXT NOT NULL,
            module_id TEXT,
            file_name TEXT,
            format TEXT,
            artifact_bytes BLOB,
            byte_len INTEGER,
            error_message TEXT,
            outcome_kind INTEGER NOT NULL,
            archived INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        );
        INSERT INTO history_new (
            id, source_kind, source, name, detail, extension_label,
            badge_color, badge_text_color, output_format, module_id,
            file_name, format, artifact_bytes, byte_len, error_message,
            outcome_kind, archived, created_at
        )
        SELECT
            id, source_kind, source, name, detail, extension_label,
            badge_color, badge_text_color, output_format, module_id,
            file_name, format, artifact_bytes, byte_len, error_message,
            outcome_kind, archived, created_at
        FROM history;
        DROP TABLE history;
        ALTER TABLE history_new RENAME TO history;
        CREATE INDEX IF NOT EXISTS idx_history_created ON history(created_at DESC, id DESC);
        COMMIT;
        ",
    )?;
    Ok(())
}

fn sync_id_seq_from_history(conn: &Connection) -> Result<(), rusqlite::Error> {
    let max_id: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM history", [], |row| {
        row.get(0)
    })?;
    let next = max_id.saturating_add(1).max(1);
    let existing: Option<i64> = conn
        .query_row(
            "SELECT next_id FROM history_id_seq WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        None => {
            conn.execute(
                "INSERT INTO history_id_seq (singleton, next_id) VALUES (1, ?1)",
                params![next],
            )?;
        }
        Some(current) if current < next => {
            conn.execute(
                "UPDATE history_id_seq SET next_id = ?1 WHERE singleton = 1",
                params![next],
            )?;
        }
        Some(_) => {}
    }
    Ok(())
}

/// Open (or create) the history database at the given path and ensure the schema exists.
pub fn open_history(path: impl AsRef<Path>) -> Result<Connection, rusqlite::Error> {
    let path = path.as_ref();
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        ensure_private_dir(parent)
            .map_err(|_| rusqlite::Error::InvalidPath(parent.to_path_buf()))?;
    }
    let flags = OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let open_path = canonical_history_open_path(path)?;
    let conn = Connection::open_with_flags(&open_path, flags)?;
    initialize_history_schema(&conn)?;
    if open_path != Path::new(":memory:") {
        ensure_private_file(&open_path)
            .map_err(|_| rusqlite::Error::InvalidPath(open_path.clone()))?;
    }
    Ok(conn)
}

/// Peek the next id that would be allocated without consuming it.
pub fn peek_next_history_id(conn: &Connection) -> Result<u64, rusqlite::Error> {
    let from_seq: Option<i64> = conn
        .query_row(
            "SELECT next_id FROM history_id_seq WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(n) = from_seq {
        return Ok((n as u64).max(1));
    }
    let max_id: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM history", [], |row| {
        row.get(0)
    })?;
    Ok((max_id as u64).saturating_add(1).max(1))
}

/// Transactionally allocate a new history id that is unique across processes
/// sharing the same database (BEGIN IMMEDIATE + `history_id_seq`).
pub fn allocate_history_id(db_path: impl AsRef<Path>) -> io::Result<u64> {
    let mut conn = open_history(db_path).map_err(|error| io::Error::other(error.to_string()))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| io::Error::other(error.to_string()))?;
    sync_id_seq_from_history(&tx).map_err(|error| io::Error::other(error.to_string()))?;
    let next: i64 = tx
        .query_row(
            "SELECT next_id FROM history_id_seq WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
    tx.execute(
        "UPDATE history_id_seq SET next_id = ?1 WHERE singleton = 1",
        params![next + 1],
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    tx.commit()
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok((next as u64).max(1))
}

/// Allocate via the default history database path when available.
pub fn allocate_history_id_default() -> Option<u64> {
    let path = history_db_path()?;
    allocate_history_id(path).ok()
}

fn legacy_migration_completed(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM history_meta WHERE key = ?1 AND value = 'complete'",
        params![LEGACY_MIGRATION_KEY],
        |_| Ok(true),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// Load history from disk (metadata only — artifact BLOBs are deferred).
///
/// Schema/read failures set [`LoadedHistory::load_error`] and
/// [`LoadedHistory::load_incomplete`] instead of silently reporting success
/// with `next_id = 1`, which would risk overwriting durable ids.
pub fn load_history() -> LoadedHistory {
    let Some(db_path) = history_db_path() else {
        return LoadedHistory {
            entries: Vec::new(),
            next_id: 1,
            load_error: None,
            load_incomplete: false,
        };
    };
    let legacy_path = history_legacy_path();

    if let Some(parent) = db_path.parent() {
        let _ = ensure_private_dir(parent);
    }

    let mut conn = match open_history(&db_path) {
        Ok(conn) => conn,
        Err(error) => {
            // Try to preserve any known next_id from a partially readable DB.
            let next_id = peek_next_id_best_effort(&db_path).unwrap_or(1);
            return LoadedHistory {
                entries: Vec::new(),
                next_id,
                load_error: Some(format!("could not open history database: {error}")),
                load_incomplete: true,
            };
        }
    };

    if let Some(ref legacy) = legacy_path {
        if legacy.exists() && !legacy_migration_completed(&conn) {
            let bytes = match read_legacy_history_bounded(legacy) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return legacy_load_failure(
                        &conn,
                        legacy,
                        format!("could not read legacy history: {error}"),
                    );
                }
            };
            match import_legacy_history(&mut conn, &bytes) {
                Ok(count) => {
                    // Verify imported rows match the decoded entry count.
                    let expected = decode_history(&bytes)
                        .map(|loaded| loaded.entries.len())
                        .unwrap_or(count);
                    if count != expected {
                        return LoadedHistory {
                            entries: Vec::new(),
                            next_id: peek_next_history_id(&conn).unwrap_or(1),
                            load_error: Some(format!(
                                "legacy history import count mismatch: got {count}, expected {expected}"
                            )),
                            load_incomplete: true,
                        };
                    }
                    let backup = legacy.with_extension("legacy.bak");
                    if let Err(error) = std::fs::rename(legacy, &backup) {
                        // Import succeeded; surface rename issues without rolling back rows.
                        let entries = history_entries(&conn, true).unwrap_or_default();
                        let next_id = peek_next_history_id(&conn).unwrap_or(1);
                        return LoadedHistory {
                            entries,
                            next_id,
                            load_error: Some(format!(
                                "history imported but could not archive legacy file: {error}"
                            )),
                            load_incomplete: false,
                        };
                    }
                }
                Err(error) => {
                    return legacy_load_failure(
                        &conn,
                        legacy,
                        format!("legacy history import failed: {error}"),
                    );
                }
            }
        }
    }

    match history_entries(&conn, true) {
        Ok(entries) => {
            let next_id = peek_next_history_id(&conn).unwrap_or_else(|_| {
                let max_id = entries.iter().map(|e| e.id).max().unwrap_or(0);
                max_id.saturating_add(1).max(1)
            });
            LoadedHistory {
                entries,
                next_id,
                load_error: None,
                load_incomplete: false,
            }
        }
        Err(error) => {
            let next_id = peek_next_history_id(&conn)
                .ok()
                .or_else(|| peek_next_id_best_effort(&db_path))
                .unwrap_or(1);
            LoadedHistory {
                entries: Vec::new(),
                next_id: next_id.max(1),
                load_error: Some(format!("could not read history rows: {error}")),
                load_incomplete: true,
            }
        }
    }
}

fn read_legacy_history_bounded(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_LEGACY_HISTORY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "legacy history exceeds the {} byte limit",
                MAX_LEGACY_HISTORY_FILE_BYTES
            ),
        ));
    }

    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_LEGACY_HISTORY_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LEGACY_HISTORY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "legacy history exceeds the {} byte limit",
                MAX_LEGACY_HISTORY_FILE_BYTES
            ),
        ));
    }
    Ok(bytes)
}

fn quarantine_legacy_history(path: &Path) -> Option<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name()?.to_string_lossy();
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let quarantined = parent.join(format!("{name}.bad.{token}"));
    std::fs::rename(path, &quarantined)
        .ok()
        .map(|()| quarantined)
}

fn legacy_load_failure(conn: &Connection, legacy: &Path, message: String) -> LoadedHistory {
    // Keep valid SQLite rows visible even when an obsolete legacy blob is
    // malformed. Quarantine the bad input so a startup failure is not repeated
    // forever; the original bytes remain recoverable under the new name.
    let entries = history_entries(conn, true).unwrap_or_default();
    let next_id = peek_next_history_id(conn).unwrap_or(1);
    let message = match quarantine_legacy_history(legacy) {
        Some(path) => format!("{message}; legacy file quarantined at {}", path.display()),
        None => message,
    };
    LoadedHistory {
        entries,
        next_id,
        load_error: Some(message),
        load_incomplete: true,
    }
}

fn peek_next_id_best_effort(db_path: &Path) -> Option<u64> {
    let flags = OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let open_path = canonical_history_open_path(db_path).ok()?;
    let conn = Connection::open_with_flags(open_path, flags).ok()?;
    // Avoid full schema init; just read what we can.
    if let Ok(n) = conn.query_row(
        "SELECT next_id FROM history_id_seq WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    ) {
        return Some((n as u64).max(1));
    }
    if let Ok(max_id) = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM history", [], |row| {
        row.get::<_, i64>(0)
    }) {
        return Some((max_id as u64).saturating_add(1).max(1));
    }
    if let Ok(seq) = conn.query_row(
        "SELECT seq FROM sqlite_sequence WHERE name = 'history'",
        [],
        |row| row.get::<_, i64>(0),
    ) {
        return Some((seq as u64).saturating_add(1).max(1));
    }
    None
}

/// Incrementally persist history changes to SQLite.
///
/// Only the rows named in `changed_ids` (upserted from `entries`) and
/// `deleted_ids` (removed) are touched; every other stored row is left intact.
/// After applying the delta, row and total-artifact budgets are enforced.
pub fn save_history_delta(
    entries: &[StoredHistoryEntry],
    changed_ids: &[u64],
    deleted_ids: &[u64],
) -> io::Result<()> {
    let Some(db_path) = history_db_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the user home directory",
        ));
    };
    save_history_delta_to(db_path, entries, changed_ids, deleted_ids)
}

/// Write a history delta to a specific SQLite path.
pub fn save_history_delta_to(
    db_path: impl AsRef<Path>,
    entries: &[StoredHistoryEntry],
    changed_ids: &[u64],
    deleted_ids: &[u64],
) -> io::Result<()> {
    let db_path = db_path.as_ref();
    let _guard = history_store_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    save_history_delta_to_locked(db_path, entries, changed_ids, deleted_ids)
}

/// Write a history delta unless a clear operation has superseded the caller's
/// snapshot. The check and write share the history-store lock, so a clear
/// either happens before this save or removes the database after it.
pub fn save_history_delta_to_if_current(
    db_path: impl AsRef<Path>,
    entries: &[StoredHistoryEntry],
    changed_ids: &[u64],
    deleted_ids: &[u64],
    expected_epoch: u64,
) -> io::Result<bool> {
    let db_path = db_path.as_ref();
    let _guard = history_store_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if history_store_epoch() != expected_epoch {
        return Ok(false);
    }
    save_history_delta_to_locked(db_path, entries, changed_ids, deleted_ids)?;
    Ok(true)
}

fn save_history_delta_to_locked(
    db_path: &Path,
    entries: &[StoredHistoryEntry],
    changed_ids: &[u64],
    deleted_ids: &[u64],
) -> io::Result<()> {
    if let Some(parent) = db_path.parent() {
        ensure_private_dir(parent)?;
    }

    let mut conn = open_history(db_path).map_err(|error| io::Error::other(error.to_string()))?;
    let tx = conn
        .transaction()
        .map_err(|error| io::Error::other(error.to_string()))?;

    delete_history_entries(&tx, deleted_ids)
        .map_err(|error| io::Error::other(error.to_string()))?;

    if !changed_ids.is_empty() {
        let by_id: HashMap<u64, &StoredHistoryEntry> =
            entries.iter().map(|entry| (entry.id, entry)).collect();
        for id in changed_ids {
            let Some(entry) = by_id.get(id) else {
                continue;
            };
            upsert_history_entry(&tx, entry)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
    }

    enforce_row_limit(&tx, MAX_HISTORY_LIMIT)
        .map_err(|error| io::Error::other(error.to_string()))?;
    enforce_artifact_byte_budget(&tx, MAX_HISTORY_TOTAL_ARTIFACT_BYTES)
        .map_err(|error| io::Error::other(error.to_string()))?;
    sync_id_seq_from_history(&tx).map_err(|error| io::Error::other(error.to_string()))?;

    tx.commit()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let _ = ensure_private_file(db_path);
    Ok(())
}

/// Persist the in-memory history list to SQLite by fully reconciling the stored
/// rows with `entries`. Prefer `save_history_delta` when the caller tracks dirty
/// and deleted IDs.
pub fn save_history(entries: &[StoredHistoryEntry], _next_id: u64) -> io::Result<()> {
    let Some(db_path) = history_db_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the user home directory",
        ));
    };
    let _guard = history_store_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(parent) = db_path.parent() {
        ensure_private_dir(parent)?;
    }

    let changed_ids: Vec<u64> = entries.iter().map(|entry| entry.id).collect();

    let existing_ids = {
        let conn = open_history(&db_path).map_err(|error| io::Error::other(error.to_string()))?;
        stored_history_ids(&conn).map_err(|error| io::Error::other(error.to_string()))?
    };
    let kept: std::collections::HashSet<u64> = changed_ids.iter().copied().collect();
    let deleted_ids: Vec<u64> = existing_ids
        .into_iter()
        .filter(|id| !kept.contains(id))
        .collect();

    save_history_delta_to_locked(&db_path, entries, &changed_ids, &deleted_ids)
}

/// Return the IDs of every row currently stored in the history table.
fn stored_history_ids(conn: &Connection) -> Result<Vec<u64>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT id FROM history")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0).map(|id| id as u64))?;
    rows.collect()
}

fn enforce_row_limit(conn: &Connection, limit: usize) -> Result<(), rusqlite::Error> {
    if limit == 0 {
        return Ok(());
    }
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
    let limit = limit as i64;
    if total > limit {
        let excess = total - limit;
        conn.execute(
            "DELETE FROM history WHERE id IN (
                SELECT id FROM history ORDER BY created_at ASC, id ASC LIMIT ?1
            )",
            params![excess],
        )?;
    }
    Ok(())
}

/// Drop oldest artifact BLOBs (promote Ready → ReadyLarge) until under budget.
fn enforce_artifact_byte_budget(conn: &Connection, budget: usize) -> Result<(), rusqlite::Error> {
    let budget = budget as i64;
    loop {
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(artifact_bytes)), 0) FROM history
             WHERE artifact_bytes IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        if total <= budget {
            break;
        }
        let victim: Option<(i64, i64)> = conn
            .query_row(
                "SELECT id, LENGTH(artifact_bytes) FROM history
                 WHERE artifact_bytes IS NOT NULL AND LENGTH(artifact_bytes) > 0
                 ORDER BY created_at ASC, id ASC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, len)) = victim else {
            break;
        };
        conn.execute(
            "UPDATE history SET
                artifact_bytes = NULL,
                byte_len = COALESCE(byte_len, ?2),
                outcome_kind = CASE WHEN outcome_kind = 0 THEN 1 ELSE outcome_kind END,
                file_name = CASE WHEN outcome_kind = 0 THEN NULL ELSE file_name END,
                format = CASE WHEN outcome_kind = 0 THEN NULL ELSE format END
             WHERE id = ?1",
            params![id, len],
        )?;
    }
    Ok(())
}

/// Remove the on-disk history store (SQLite, legacy blob, and legacy.bak).
///
/// When the SQLite file exists, rows are deleted and the file is VACUUMed
/// before removal so freed pages are not left on disk longer than needed.
pub fn clear_history_store() -> io::Result<()> {
    HISTORY_STORE_EPOCH.fetch_add(1, Ordering::SeqCst);
    let _guard = history_store_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(db_path) = history_db_path() {
        match std::fs::symlink_metadata(&db_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // Remove only the link; never open or vacuum its target.
                let _ = std::fs::remove_file(&db_path);
            }
            Ok(metadata) if metadata.is_file() => {
                // Best-effort secure clear: delete rows + VACUUM, then remove file.
                let flags = OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW;
                if let Ok(open_path) = canonical_history_open_path(&db_path)
                    && let Ok(conn) = Connection::open_with_flags(open_path, flags)
                {
                    let _ = conn.execute_batch(
                        "
                    DELETE FROM history;
                    DELETE FROM history_id_seq;
                    VACUUM;
                    ",
                    );
                }
                let _ = std::fs::remove_file(&db_path);
                // Also remove SQLite sidecars if present.
                let _ = std::fs::remove_file(format!("{}-wal", db_path.display()));
                let _ = std::fs::remove_file(format!("{}-shm", db_path.display()));
                let _ = std::fs::remove_file(format!("{}-journal", db_path.display()));
            }
            _ => {}
        }
    }
    if let Some(legacy) = history_legacy_path() {
        let _ = std::fs::remove_file(&legacy);
    }
    if let Some(bak) = history_legacy_bak_path() {
        let _ = std::fs::remove_file(&bak);
    }
    Ok(())
}

/// Columns written for a history row, shared by the plain-insert and upsert paths.
const HISTORY_ROW_COLUMNS: &str = "id, source_kind, source, name, detail, extension_label,
            badge_color, badge_text_color, output_format, module_id,
            file_name, format, artifact_bytes, byte_len, error_message,
            outcome_kind, archived";
const HISTORY_ROW_PLACEHOLDERS: &str =
    "?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17";

fn insert_entry(tx: &Connection, entry: &StoredHistoryEntry) -> Result<(), rusqlite::Error> {
    write_entry_row(tx, entry, false)
}

/// Insert `entry`, or update the existing row with the same `id` in place.
///
/// `created_at` is intentionally left untouched on update so ordering by
/// insertion time is preserved for pre-existing rows.
fn upsert_history_entry(
    tx: &Connection,
    entry: &StoredHistoryEntry,
) -> Result<(), rusqlite::Error> {
    write_entry_row(tx, entry, true)
}

/// Delete history rows whose `id` appears in `ids`, chunking the `IN (...)`
/// clause so very large lists stay within SQLite's bound-parameter limit.
fn delete_history_entries(tx: &Connection, ids: &[u64]) -> Result<(), rusqlite::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    const CHUNK: usize = 512;
    for chunk in ids.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM history WHERE id IN ({placeholders})");
        let bound = chunk.iter().map(|&id| id as i64);
        tx.execute(&sql, params_from_iter(bound))?;
    }
    Ok(())
}

fn write_entry_row(
    tx: &Connection,
    entry: &StoredHistoryEntry,
    upsert: bool,
) -> Result<(), rusqlite::Error> {
    // If the caller is updating metadata only (deferred Ready with empty bytes),
    // preserve existing artifact_bytes on conflict.
    let preserve_blob = entry.artifact_deferred
        && matches!(
            &entry.outcome,
            StoredOutcome::Ready { bytes, .. } if bytes.is_empty()
        );

    let (source_kind, source) = match &entry.source {
        StoredSource::File(path) => (0i64, store_source_path(path)),
        StoredSource::Url(url) => (1i64, redact_url_credentials(url)),
    };

    let (outcome_kind, module_id, file_name, format, artifact_bytes, byte_len, error_message) =
        match &entry.outcome {
            StoredOutcome::Ready {
                module_id,
                file_name,
                format,
                bytes,
            } => {
                let blob = if preserve_blob {
                    None
                } else {
                    Some(bytes.as_slice())
                };
                let len = if preserve_blob {
                    None
                } else {
                    Some(bytes.len() as i64)
                };
                (
                    0i64,
                    Some(module_id.as_str()),
                    Some(file_name.as_str()),
                    Some(format.as_str()),
                    blob,
                    len,
                    None::<&str>,
                )
            }
            StoredOutcome::ReadyLarge {
                module_id,
                byte_len,
            } => (
                1i64,
                Some(module_id.as_str()),
                None::<&str>,
                None::<&str>,
                None::<&[u8]>,
                Some(*byte_len as i64),
                None::<&str>,
            ),
            StoredOutcome::Failed(message) => (
                2i64,
                None::<&str>,
                None::<&str>,
                None::<&str>,
                None::<&[u8]>,
                None::<i64>,
                Some(message.as_str()),
            ),
        };

    let conflict = if upsert {
        if preserve_blob {
            " ON CONFLICT(id) DO UPDATE SET
            source_kind = excluded.source_kind,
            source = excluded.source,
            name = excluded.name,
            detail = excluded.detail,
            extension_label = excluded.extension_label,
            badge_color = excluded.badge_color,
            badge_text_color = excluded.badge_text_color,
            output_format = excluded.output_format,
            module_id = excluded.module_id,
            file_name = excluded.file_name,
            format = excluded.format,
            error_message = excluded.error_message,
            outcome_kind = excluded.outcome_kind,
            archived = excluded.archived"
        } else {
            " ON CONFLICT(id) DO UPDATE SET
            source_kind = excluded.source_kind,
            source = excluded.source,
            name = excluded.name,
            detail = excluded.detail,
            extension_label = excluded.extension_label,
            badge_color = excluded.badge_color,
            badge_text_color = excluded.badge_text_color,
            output_format = excluded.output_format,
            module_id = excluded.module_id,
            file_name = excluded.file_name,
            format = excluded.format,
            artifact_bytes = excluded.artifact_bytes,
            byte_len = excluded.byte_len,
            error_message = excluded.error_message,
            outcome_kind = excluded.outcome_kind,
            archived = excluded.archived"
        }
    } else {
        ""
    };
    let sql = format!(
        "INSERT INTO history ({HISTORY_ROW_COLUMNS}) VALUES ({HISTORY_ROW_PLACEHOLDERS}){conflict}"
    );

    tx.execute(
        &sql,
        params![
            entry.id as i64,
            source_kind,
            source,
            entry.name,
            entry.detail,
            entry.extension_label,
            entry.badge_color as i64,
            entry.badge_text_color as i64,
            entry.output_format,
            module_id,
            file_name,
            format,
            artifact_bytes,
            byte_len,
            error_message,
            outcome_kind,
            entry.archived as i64,
        ],
    )?;

    // Keep the transactional id allocator ahead of any explicit id we wrote.
    let _ = tx.execute(
        "UPDATE history_id_seq SET next_id = max(next_id, ?1) WHERE singleton = 1",
        params![(entry.id as i64) + 1],
    );
    // If seq row missing, sync will recreate on next open; best-effort insert:
    let _ = tx.execute(
        "INSERT OR IGNORE INTO history_id_seq (singleton, next_id) VALUES (1, ?1)",
        params![(entry.id as i64) + 1],
    );
    Ok(())
}

/// Insert a single entry and trim the table so only the most recent `limit` rows remain.
#[cfg(test)]
pub fn add_history_entry(
    conn: &Connection,
    entry: &StoredHistoryEntry,
    limit: usize,
) -> Result<(), rusqlite::Error> {
    insert_entry(conn, entry)?;
    enforce_row_limit(conn, limit)?;
    Ok(())
}

/// Return all history entries (metadata only — artifact BLOBs are not loaded).
pub fn history_entries(
    conn: &Connection,
    include_archived: bool,
) -> Result<Vec<StoredHistoryEntry>, rusqlite::Error> {
    history_entries_impl(conn, include_archived, false)
}

/// Return all history entries including artifact BLOB payloads.
pub fn history_entries_with_artifacts(
    conn: &Connection,
    include_archived: bool,
) -> Result<Vec<StoredHistoryEntry>, rusqlite::Error> {
    history_entries_impl(conn, include_archived, true)
}

fn history_entries_impl(
    conn: &Connection,
    include_archived: bool,
    include_artifacts: bool,
) -> Result<Vec<StoredHistoryEntry>, rusqlite::Error> {
    let sql = if include_artifacts {
        "SELECT id, source_kind, source, name, detail, extension_label,
                badge_color, badge_text_color, output_format, module_id,
                file_name, format, artifact_bytes, byte_len, error_message,
                outcome_kind, archived,
                CASE WHEN artifact_bytes IS NOT NULL AND LENGTH(artifact_bytes) > 0
                     THEN 1 ELSE 0 END AS has_artifact
         FROM history
         WHERE (archived = 0 OR ?1 = 1)
         ORDER BY created_at DESC, id DESC"
    } else {
        "SELECT id, source_kind, source, name, detail, extension_label,
                badge_color, badge_text_color, output_format, module_id,
                file_name, format, NULL AS artifact_bytes, byte_len, error_message,
                outcome_kind, archived,
                CASE WHEN artifact_bytes IS NOT NULL AND LENGTH(artifact_bytes) > 0
                     THEN 1 ELSE 0 END AS has_artifact
         FROM history
         WHERE (archived = 0 OR ?1 = 1)
         ORDER BY created_at DESC, id DESC"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![include_archived as i64], |row| {
        row_to_entry(row, include_artifacts)
    })?;
    rows.collect()
}

/// Fetch the artifact BLOB for a single history row, if present.
pub fn load_history_artifact(
    conn: &Connection,
    id: u64,
) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    conn.query_row(
        "SELECT artifact_bytes FROM history WHERE id = ?1",
        params![id as i64],
        |row| row.get::<_, Option<Vec<u8>>>(0),
    )
    .optional()
    .map(|opt| opt.flatten().filter(|b| !b.is_empty()))
}

/// Load an artifact BLOB from the default history database path.
pub fn load_history_artifact_default(id: u64) -> io::Result<Option<Vec<u8>>> {
    let Some(db_path) = history_db_path() else {
        return Ok(None);
    };
    if !db_path.is_file() {
        return Ok(None);
    }
    let conn = open_history(db_path).map_err(|error| io::Error::other(error.to_string()))?;
    load_history_artifact(&conn, id).map_err(|error| io::Error::other(error.to_string()))
}

/// Mark an entry as archived. Returns `true` if the row existed.
#[cfg(test)]
pub fn archive_history(conn: &Connection, id: u64) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE history SET archived = 1 WHERE id = ?1",
        params![id as i64],
    )?;
    Ok(changed > 0)
}

/// Delete an entry from history. Returns `true` if the row existed.
#[cfg(test)]
pub fn delete_history(conn: &Connection, id: u64) -> Result<bool, rusqlite::Error> {
    let changed = conn.execute("DELETE FROM history WHERE id = ?1", params![id as i64])?;
    Ok(changed > 0)
}

/// Decode a legacy binary blob and import the rows it contains in one
/// transaction. On any failure the transaction is rolled back and the caller
/// must leave the legacy file in place. Returns the number of imported rows,
/// which equals the decoded entry count on success.
pub fn import_legacy_history(conn: &mut Connection, bytes: &[u8]) -> io::Result<usize> {
    let loaded = decode_history(bytes)?;
    let expected = loaded.entries.len();
    let tx = conn
        .transaction()
        .map_err(|error| io::Error::other(error.to_string()))?;
    for entry in &loaded.entries {
        insert_entry(&tx, entry).map_err(|error| io::Error::other(error.to_string()))?;
    }
    // Verify every decoded id is present.
    let mut found = 0usize;
    for entry in &loaded.entries {
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM history WHERE id = ?1",
                params![entry.id as i64],
                |_| Ok(true),
            )
            .optional()
            .map_err(|error| io::Error::other(error.to_string()))?
            .unwrap_or(false);
        if exists {
            found += 1;
        }
    }
    if found != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("legacy import verification failed: found {found} of {expected} rows"),
        ));
    }
    sync_id_seq_from_history(&tx).map_err(|error| io::Error::other(error.to_string()))?;
    // Advance seq at least to the legacy next_id when larger.
    if loaded.next_id > 1 {
        let _ = tx.execute(
            "UPDATE history_id_seq SET next_id = max(next_id, ?1) WHERE singleton = 1",
            params![loaded.next_id as i64],
        );
    }
    tx.execute(
        "INSERT INTO history_meta (key, value) VALUES (?1, 'complete')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![LEGACY_MIGRATION_KEY],
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    tx.commit()
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(expected)
}

fn row_to_entry(
    row: &rusqlite::Row<'_>,
    include_artifacts: bool,
) -> Result<StoredHistoryEntry, rusqlite::Error> {
    let id = row.get::<_, i64>("id")? as u64;
    let source_kind = row.get::<_, i64>("source_kind")?;
    let source_raw = row.get::<_, String>("source")?;
    let source = match source_kind {
        0 => StoredSource::File(restore_source_path(&source_raw)),
        1 => StoredSource::Url(source_raw),
        _ => {
            return Err(rusqlite::Error::IntegralValueOutOfRange(0, source_kind));
        }
    };
    let name = row.get::<_, String>("name")?;
    let detail = row.get::<_, String>("detail")?;
    let extension_label = row.get::<_, String>("extension_label")?;
    let badge_color = row.get::<_, i64>("badge_color")? as u32;
    let badge_text_color = row.get::<_, i64>("badge_text_color")? as u32;
    let output_format = row.get::<_, String>("output_format")?;
    let outcome_kind = row.get::<_, i64>("outcome_kind")?;
    let archived = row.get::<_, i64>("archived")? != 0;

    let module_id: Option<String> = row.get("module_id")?;
    let file_name: Option<String> = row.get("file_name")?;
    let format: Option<String> = row.get("format")?;
    let artifact_bytes: Option<Vec<u8>> = row.get("artifact_bytes")?;
    let byte_len: Option<i64> = row.get("byte_len")?;
    let error_message: Option<String> = row.get("error_message")?;
    let has_artifact: i64 = row.get("has_artifact").unwrap_or(0);

    let mut artifact_deferred = false;
    let outcome = match outcome_kind {
        0 => {
            let bytes = if include_artifacts {
                artifact_bytes.unwrap_or_default()
            } else {
                if has_artifact != 0 {
                    artifact_deferred = true;
                }
                Vec::new()
            };
            StoredOutcome::Ready {
                module_id: module_id.unwrap_or_default(),
                file_name: file_name.unwrap_or_default(),
                format: format.unwrap_or_default(),
                bytes,
            }
        }
        1 => StoredOutcome::ReadyLarge {
            module_id: module_id.unwrap_or_default(),
            byte_len: byte_len.unwrap_or(0) as usize,
        },
        2 => StoredOutcome::Failed(error_message.unwrap_or_default()),
        _ => {
            return Err(rusqlite::Error::IntegralValueOutOfRange(0, outcome_kind));
        }
    };

    Ok(StoredHistoryEntry {
        id,
        source,
        name,
        detail,
        extension_label,
        badge_color,
        badge_text_color,
        output_format,
        outcome,
        archived,
        artifact_deferred,
    })
}

// Legacy binary format --------------------------------------------------------

#[cfg(test)]
pub(crate) fn encode_history(entries: &[StoredHistoryEntry], next_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + entries.len() * 128);
    out.extend_from_slice(MAGIC);
    write_u64(&mut out, next_id);
    write_u32(&mut out, entries.len() as u32);
    for entry in entries {
        write_entry(&mut out, entry);
    }
    out
}

pub(crate) fn decode_history(bytes: &[u8]) -> io::Result<LoadedHistory> {
    let mut cursor = Cursor::new(bytes);
    let mut magic = [0u8; 17];
    cursor.read_exact(&mut magic)?;
    if magic.as_slice() != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unrecognized history file magic",
        ));
    }
    let next_id = read_u64(&mut cursor)?;
    let count = read_u32(&mut cursor)? as usize;
    let mut entries = Vec::with_capacity(count.min(MAX_HISTORY_ENTRIES));
    for _ in 0..count {
        if entries.len() >= MAX_HISTORY_ENTRIES {
            break;
        }
        entries.push(read_entry(&mut cursor)?);
    }
    Ok(LoadedHistory {
        entries,
        next_id: next_id.max(1),
        load_error: None,
        load_incomplete: false,
    })
}

#[cfg(test)]
fn write_entry(out: &mut Vec<u8>, entry: &StoredHistoryEntry) {
    write_u64(out, entry.id);
    match &entry.source {
        StoredSource::File(path) => {
            out.push(0);
            write_string(out, &store_source_path(path));
        }
        StoredSource::Url(url) => {
            out.push(1);
            write_string(out, url);
        }
    }
    write_string(out, &entry.name);
    write_string(out, &entry.detail);
    write_string(out, &entry.extension_label);
    write_u32(out, entry.badge_color);
    write_u32(out, entry.badge_text_color);
    write_string(out, &entry.output_format);
    match &entry.outcome {
        StoredOutcome::Ready {
            module_id,
            file_name,
            format,
            bytes,
        } => {
            out.push(0);
            write_string(out, module_id);
            write_string(out, file_name);
            write_string(out, format);
            write_bytes(out, bytes);
        }
        StoredOutcome::ReadyLarge {
            module_id,
            byte_len,
        } => {
            out.push(1);
            write_string(out, module_id);
            write_u64(out, *byte_len as u64);
        }
        StoredOutcome::Failed(message) => {
            out.push(2);
            write_string(out, message);
        }
    }
}

fn read_entry(cursor: &mut Cursor<&[u8]>) -> io::Result<StoredHistoryEntry> {
    let id = read_u64(cursor)?;
    let source_kind = read_u8(cursor)?;
    let source_raw = read_string(cursor)?;
    let source = match source_kind {
        0 => StoredSource::File(restore_source_path(&source_raw)),
        1 => StoredSource::Url(source_raw),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown history source kind",
            ));
        }
    };
    let name = read_string(cursor)?;
    let detail = read_string(cursor)?;
    let extension_label = read_string(cursor)?;
    let badge_color = read_u32(cursor)?;
    let badge_text_color = read_u32(cursor)?;
    let output_format = read_string(cursor)?;
    let outcome_kind = read_u8(cursor)?;
    let outcome = match outcome_kind {
        0 => {
            let module_id = read_string(cursor)?;
            let file_name = read_string(cursor)?;
            let format = read_string(cursor)?;
            let bytes = read_bytes(cursor)?;
            StoredOutcome::Ready {
                module_id,
                file_name,
                format,
                bytes,
            }
        }
        1 => {
            let module_id = read_string(cursor)?;
            let byte_len = read_u64(cursor)? as usize;
            StoredOutcome::ReadyLarge {
                module_id,
                byte_len,
            }
        }
        2 => {
            let message = read_string(cursor)?;
            StoredOutcome::Failed(message)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown history outcome kind",
            ));
        }
    };
    Ok(StoredHistoryEntry {
        id,
        source,
        name,
        detail,
        extension_label,
        badge_color,
        badge_text_color,
        output_format,
        outcome,
        archived: false,
        artifact_deferred: false,
    })
}

#[cfg(test)]
fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u64(cursor: &mut Cursor<&[u8]>) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    cursor.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    cursor.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u8(cursor: &mut Cursor<&[u8]>) -> io::Result<u8> {
    let mut byte = [0u8; 1];
    cursor.read_exact(&mut byte)?;
    Ok(byte[0])
}

#[cfg(test)]
fn write_string(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    write_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn read_string(cursor: &mut Cursor<&[u8]>) -> io::Result<String> {
    let len = read_u32(cursor)? as usize;
    if len > MAX_LEGACY_FIELD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("legacy history field exceeds the {MAX_LEGACY_FIELD_BYTES} byte limit"),
        ));
    }
    let mut bytes = vec![0u8; len];
    cursor.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
fn write_bytes(out: &mut Vec<u8>, value: &[u8]) {
    write_u32(out, value.len() as u32);
    out.extend_from_slice(value);
}

fn read_bytes(cursor: &mut Cursor<&[u8]>) -> io::Result<Vec<u8>> {
    let len = read_u32(cursor)? as usize;
    if len > MAX_HISTORY_ARTIFACT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("legacy history artifact exceeds the {MAX_HISTORY_ARTIFACT_BYTES} byte limit"),
        ));
    }
    let mut bytes = vec![0u8; len];
    cursor.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(id: u64, name: &str, archived: bool) -> StoredHistoryEntry {
        StoredHistoryEntry {
            id,
            source: StoredSource::File(PathBuf::from("/tmp/sample.txt")),
            name: name.to_owned(),
            detail: "detail".to_owned(),
            extension_label: "TXT".to_owned(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "markdown".to_owned(),
            outcome: StoredOutcome::Ready {
                module_id: "pandoc".to_owned(),
                file_name: "sample.md".to_owned(),
                format: "markdown".to_owned(),
                bytes: b"body".to_vec(),
            },
            archived,
            artifact_deferred: false,
        }
    }

    fn temp_support_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shift-history-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn source_paths_round_trip_and_use_home_prefix() {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("test requires HOME");
        let under_home = home.join("Documents/report.docx");
        let outside = PathBuf::from("/tmp/sample.txt");

        assert_eq!(store_source_path(&under_home), "~/Documents/report.docx");
        assert_eq!(store_source_path(&home), "~");
        assert_eq!(store_source_path(&outside), outside.to_str().unwrap());

        assert_eq!(restore_source_path("~/Documents/report.docx"), under_home);
        assert_eq!(restore_source_path("~"), home);
        assert_eq!(restore_source_path("/tmp/sample.txt"), outside);
    }

    #[test]
    fn source_paths_preserve_whitespace_without_trim() {
        let spaced = PathBuf::from("/tmp/ leading and trailing ");
        let stored = store_source_path(&spaced);
        assert!(
            !stored.starts_with(' ') && stored.contains(" leading"),
            "stored={stored}"
        );
        // Leading/trailing spaces in the stored absolute path must survive restore.
        let raw = "/tmp/ leading and trailing ";
        assert_eq!(restore_source_path(raw), PathBuf::from(raw));
        // Trim would have collapsed this — we must not.
        assert_ne!(restore_source_path(raw), PathBuf::from(raw.trim()));
    }

    #[test]
    fn add_and_retrieve_entries() {
        let conn = open_history(":memory:").unwrap();
        let e1 = sample_entry(1, "first", false);
        let e2 = sample_entry(2, "second", true);
        add_history_entry(&conn, &e1, DEFAULT_HISTORY_LIMIT).unwrap();
        add_history_entry(&conn, &e2, DEFAULT_HISTORY_LIMIT).unwrap();

        let unarchived = history_entries(&conn, false).unwrap();
        assert_eq!(unarchived.len(), 1);
        assert_eq!(unarchived[0].name, "first");

        let all = history_entries(&conn, true).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn limit_enforcement_removes_oldest() {
        let conn = open_history(":memory:").unwrap();
        add_history_entry(&conn, &sample_entry(1, "one", false), 2).unwrap();
        add_history_entry(&conn, &sample_entry(2, "two", false), 2).unwrap();
        add_history_entry(&conn, &sample_entry(3, "three", false), 2).unwrap();

        let entries = history_entries(&conn, false).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 3);
        assert_eq!(entries[1].id, 2);
    }

    #[test]
    fn archive_and_delete_mutate_rows() {
        let conn = open_history(":memory:").unwrap();
        add_history_entry(&conn, &sample_entry(1, "one", false), DEFAULT_HISTORY_LIMIT).unwrap();
        assert!(archive_history(&conn, 1).unwrap());
        let entries = history_entries(&conn, false).unwrap();
        assert!(entries.is_empty());

        assert!(delete_history(&conn, 1).unwrap());
        let all = history_entries(&conn, true).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn upsert_inserts_then_updates_in_place() {
        let conn = open_history(":memory:").unwrap();
        upsert_history_entry(&conn, &sample_entry(1, "before", false)).unwrap();

        conn.execute("UPDATE history SET created_at = 100 WHERE id = 1", [])
            .unwrap();

        upsert_history_entry(&conn, &sample_entry(1, "after", true)).unwrap();

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 1, "upsert must not duplicate the row");

        let all = history_entries(&conn, true).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "after");
        assert!(all[0].archived);

        let created_at: i64 = conn
            .query_row("SELECT created_at FROM history WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(created_at, 100, "created_at must be preserved on update");
    }

    #[test]
    fn delete_history_entries_removes_only_listed_ids() {
        let conn = open_history(":memory:").unwrap();
        for id in 1..=5 {
            insert_entry(&conn, &sample_entry(id, "entry", false)).unwrap();
        }

        delete_history_entries(&conn, &[2, 4]).unwrap();

        let mut remaining: Vec<u64> = history_entries(&conn, true)
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        remaining.sort_unstable();
        assert_eq!(remaining, vec![1, 3, 5]);

        delete_history_entries(&conn, &[]).unwrap();
        delete_history_entries(&conn, &[999]).unwrap();
        assert_eq!(history_entries(&conn, true).unwrap().len(), 3);
    }

    #[test]
    fn legacy_migration_imports_and_sets_archived_false() {
        let entries = vec![
            sample_entry(1, "legacy-one", false),
            sample_entry(2, "legacy-two", false),
        ];
        let legacy = encode_history(&entries, 3);

        let mut conn = open_history(":memory:").unwrap();
        let count = import_legacy_history(&mut conn, &legacy).unwrap();
        assert_eq!(count, 2);

        let loaded = history_entries(&conn, true).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(!loaded[0].archived);
    }

    #[test]
    fn legacy_migration_is_transactional_and_rejects_partial() {
        // Craft a blob with one good entry; import of empty garbage fails entirely.
        let mut conn = open_history(":memory:").unwrap();
        assert!(import_legacy_history(&mut conn, b"not-legacy").is_err());
        assert!(history_entries(&conn, true).unwrap().is_empty());

        // Valid import then verifies count.
        let blob = encode_history(
            &[sample_entry(1, "a", false), sample_entry(2, "b", false)],
            3,
        );
        assert_eq!(import_legacy_history(&mut conn, &blob).unwrap(), 2);
        assert_eq!(history_entries(&conn, true).unwrap().len(), 2);
    }

    #[test]
    fn url_source_round_trips_and_redacts_credentials() {
        let conn = open_history(":memory:").unwrap();
        let entry = StoredHistoryEntry {
            id: 10,
            source: StoredSource::Url("https://user:s3cret@example.com/article?q=1".to_owned()),
            name: "article".to_owned(),
            detail: "from url".to_owned(),
            extension_label: "MD".to_owned(),
            badge_color: 1,
            badge_text_color: 2,
            output_format: "markdown".to_owned(),
            outcome: StoredOutcome::Ready {
                module_id: "defuddle".to_owned(),
                file_name: "article.md".to_owned(),
                format: "markdown".to_owned(),
                bytes: b"# hi".to_vec(),
            },
            archived: false,
            artifact_deferred: false,
        };
        add_history_entry(&conn, &entry, DEFAULT_HISTORY_LIMIT).unwrap();

        let loaded = history_entries_with_artifacts(&conn, true).unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].source {
            StoredSource::Url(url) => {
                assert!(
                    !url.contains("s3cret") && !url.contains("user:"),
                    "credentials must be redacted on store: {url}"
                );
                assert!(url.contains("example.com/article"));
                assert!(url.starts_with("https://"));
            }
            other => panic!("expected Url source, got {other:?}"),
        }
        let plain = StoredHistoryEntry {
            id: 11,
            source: StoredSource::Url("https://example.com/clean".to_owned()),
            name: "clean".to_owned(),
            detail: "from url".to_owned(),
            extension_label: "MD".to_owned(),
            badge_color: 1,
            badge_text_color: 2,
            output_format: "markdown".to_owned(),
            outcome: StoredOutcome::Ready {
                module_id: "defuddle".to_owned(),
                file_name: "clean.md".to_owned(),
                format: "markdown".to_owned(),
                bytes: b"# clean".to_vec(),
            },
            archived: false,
            artifact_deferred: false,
        };
        add_history_entry(&conn, &plain, DEFAULT_HISTORY_LIMIT).unwrap();
        let all = history_entries(&conn, true).unwrap();
        let clean = all.iter().find(|e| e.id == 11).unwrap();
        assert_eq!(
            clean.source,
            StoredSource::Url("https://example.com/clean".to_owned())
        );
    }

    #[test]
    fn ready_large_and_failed_outcomes_persist_without_artifact_bytes() {
        let conn = open_history(":memory:").unwrap();
        let large = StoredHistoryEntry {
            id: 1,
            source: StoredSource::File(PathBuf::from("/tmp/big.bin")),
            name: "big".to_owned(),
            detail: "large".to_owned(),
            extension_label: "BIN".to_owned(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "binary".to_owned(),
            outcome: StoredOutcome::ReadyLarge {
                module_id: "ffmpeg".to_owned(),
                byte_len: MAX_HISTORY_ARTIFACT_BYTES + 1,
            },
            archived: false,
            artifact_deferred: false,
        };
        let failed = StoredHistoryEntry {
            id: 2,
            source: StoredSource::File(PathBuf::from("/tmp/bad.docx")),
            name: "bad".to_owned(),
            detail: "err".to_owned(),
            extension_label: "DOCX".to_owned(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "markdown".to_owned(),
            outcome: StoredOutcome::Failed("converter exploded".to_owned()),
            archived: false,
            artifact_deferred: false,
        };
        add_history_entry(&conn, &large, DEFAULT_HISTORY_LIMIT).unwrap();
        add_history_entry(&conn, &failed, DEFAULT_HISTORY_LIMIT).unwrap();

        let loaded = history_entries(&conn, true).unwrap();
        assert_eq!(loaded.len(), 2);

        let large_row = loaded.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(
            large_row.outcome,
            StoredOutcome::ReadyLarge {
                module_id: "ffmpeg".to_owned(),
                byte_len: MAX_HISTORY_ARTIFACT_BYTES + 1,
            }
        );
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT artifact_bytes FROM history WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(blob.is_none() || blob.as_ref().is_some_and(|b| b.is_empty()));

        let failed_row = loaded.iter().find(|e| e.id == 2).unwrap();
        assert_eq!(
            failed_row.outcome,
            StoredOutcome::Failed("converter exploded".to_owned())
        );
    }

    #[test]
    fn ready_at_max_artifact_bytes_still_stores_payload() {
        let conn = open_history(":memory:").unwrap();
        let bytes = vec![0xABu8; MAX_HISTORY_ARTIFACT_BYTES];
        let entry = StoredHistoryEntry {
            id: 1,
            source: StoredSource::File(PathBuf::from("/tmp/at-limit.bin")),
            name: "at-limit".to_owned(),
            detail: "boundary".to_owned(),
            extension_label: "BIN".to_owned(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "binary".to_owned(),
            outcome: StoredOutcome::Ready {
                module_id: "ffmpeg".to_owned(),
                file_name: "at-limit.bin".to_owned(),
                format: "binary".to_owned(),
                bytes: bytes.clone(),
            },
            archived: false,
            artifact_deferred: false,
        };
        add_history_entry(&conn, &entry, DEFAULT_HISTORY_LIMIT).unwrap();
        let loaded = history_entries_with_artifacts(&conn, true).unwrap();
        match &loaded[0].outcome {
            StoredOutcome::Ready {
                bytes: stored,
                module_id,
                ..
            } => {
                assert_eq!(module_id, "ffmpeg");
                assert_eq!(stored.len(), MAX_HISTORY_ARTIFACT_BYTES);
                assert_eq!(stored, &bytes);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn history_entries_lazy_loads_blobs_metadata_first() {
        let conn = open_history(":memory:").unwrap();
        let entry = sample_entry(1, "lazy", false);
        add_history_entry(&conn, &entry, DEFAULT_HISTORY_LIMIT).unwrap();

        let meta = history_entries(&conn, true).unwrap();
        assert_eq!(meta.len(), 1);
        assert!(meta[0].artifact_deferred);
        match &meta[0].outcome {
            StoredOutcome::Ready { bytes, .. } => assert!(bytes.is_empty()),
            other => panic!("expected Ready, got {other:?}"),
        }

        let blob = load_history_artifact(&conn, 1).unwrap().expect("blob");
        assert_eq!(blob, b"body");

        let full = history_entries_with_artifacts(&conn, true).unwrap();
        match &full[0].outcome {
            StoredOutcome::Ready { bytes, .. } => assert_eq!(bytes, b"body"),
            other => panic!("expected Ready, got {other:?}"),
        }
        assert!(!full[0].artifact_deferred);
    }

    #[test]
    fn total_artifact_byte_budget_promotes_oldest() {
        let dir = temp_support_dir("budget");
        let db = dir.join("history.sqlite");
        // Two Ready payloads of 100 bytes; budget of 150 drops the oldest.
        let e1 = StoredHistoryEntry {
            id: 1,
            source: StoredSource::File(PathBuf::from("/tmp/a")),
            name: "a".into(),
            detail: "d".into(),
            extension_label: "BIN".into(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "binary".into(),
            outcome: StoredOutcome::Ready {
                module_id: "ffmpeg".into(),
                file_name: "a.bin".into(),
                format: "binary".into(),
                bytes: vec![1u8; 100],
            },
            archived: false,
            artifact_deferred: false,
        };
        let e2 = StoredHistoryEntry {
            id: 2,
            source: StoredSource::File(PathBuf::from("/tmp/b")),
            name: "b".into(),
            detail: "d".into(),
            extension_label: "BIN".into(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "binary".into(),
            outcome: StoredOutcome::Ready {
                module_id: "ffmpeg".into(),
                file_name: "b.bin".into(),
                format: "binary".into(),
                bytes: vec![2u8; 100],
            },
            archived: false,
            artifact_deferred: false,
        };
        save_history_delta_to(&db, &[e1, e2], &[1, 2], &[]).unwrap();

        let conn = open_history(&db).unwrap();
        // Manually enforce a tight budget.
        enforce_artifact_byte_budget(&conn, 150).unwrap();
        let blob1 = load_history_artifact(&conn, 1).unwrap();
        let blob2 = load_history_artifact(&conn, 2).unwrap();
        assert!(blob1.is_none(), "oldest blob must be dropped");
        assert!(blob2.is_some(), "newest blob must remain");
        let entries = history_entries(&conn, true).unwrap();
        let e1 = entries.iter().find(|e| e.id == 1).unwrap();
        assert!(matches!(e1.outcome, StoredOutcome::ReadyLarge { .. }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn archive_and_delete_return_false_for_unknown_ids() {
        let conn = open_history(":memory:").unwrap();
        add_history_entry(&conn, &sample_entry(1, "one", false), DEFAULT_HISTORY_LIMIT).unwrap();

        assert!(!archive_history(&conn, 999).unwrap());
        assert!(!delete_history(&conn, 999).unwrap());
        let all = history_entries(&conn, true).unwrap();
        assert_eq!(all.len(), 1);
        assert!(!all[0].archived);
    }

    #[test]
    fn clear_history_store_empties_on_disk_database_and_legacy_bak() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("clear");
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }

        let db_path = history_db_path().expect("override sets support dir");
        let conn = open_history(&db_path).unwrap();
        add_history_entry(&conn, &sample_entry(1, "one", false), DEFAULT_HISTORY_LIMIT).unwrap();
        drop(conn);
        // Simulate leftover migration backup.
        std::fs::write(dir.join("history.legacy.bak"), b"old").unwrap();
        std::fs::write(dir.join("history"), b"legacy").unwrap();
        assert!(db_path.is_file());

        clear_history_store().unwrap();
        assert!(!db_path.exists());
        assert!(!dir.join("history.legacy.bak").exists());
        assert!(!dir.join("history").exists());
        assert!(load_history().entries.is_empty());

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn encode_decode_round_trips_mixed_sources_and_outcomes() {
        let entries = vec![
            StoredHistoryEntry {
                id: 1,
                source: StoredSource::File(PathBuf::from("/tmp/a.txt")),
                name: "ready".to_owned(),
                detail: "d".to_owned(),
                extension_label: "TXT".to_owned(),
                badge_color: 1,
                badge_text_color: 2,
                output_format: "markdown".to_owned(),
                outcome: StoredOutcome::Ready {
                    module_id: "pandoc".to_owned(),
                    file_name: "a.md".to_owned(),
                    format: "markdown".to_owned(),
                    bytes: b"body".to_vec(),
                },
                archived: false,
                artifact_deferred: false,
            },
            StoredHistoryEntry {
                id: 2,
                source: StoredSource::Url("https://example.com/x".to_owned()),
                name: "large".to_owned(),
                detail: "d".to_owned(),
                extension_label: "BIN".to_owned(),
                badge_color: 0,
                badge_text_color: 0,
                output_format: "binary".to_owned(),
                outcome: StoredOutcome::ReadyLarge {
                    module_id: "ffmpeg".to_owned(),
                    byte_len: 9_000_000,
                },
                archived: false,
                artifact_deferred: false,
            },
            StoredHistoryEntry {
                id: 3,
                source: StoredSource::File(PathBuf::from("/tmp/fail.pdf")),
                name: "failed".to_owned(),
                detail: "d".to_owned(),
                extension_label: "PDF".to_owned(),
                badge_color: 0,
                badge_text_color: 0,
                output_format: "markdown".to_owned(),
                outcome: StoredOutcome::Failed("boom".to_owned()),
                archived: true,
                artifact_deferred: false,
            },
        ];
        let encoded = encode_history(&entries, 42);
        let loaded = decode_history(&encoded).unwrap();
        assert_eq!(loaded.next_id, 42);
        assert_eq!(loaded.entries.len(), 3);

        assert_eq!(loaded.entries[0].source, entries[0].source);
        assert_eq!(loaded.entries[0].outcome, entries[0].outcome);
        assert_eq!(loaded.entries[1].source, entries[1].source);
        assert_eq!(loaded.entries[1].outcome, entries[1].outcome);
        assert_eq!(loaded.entries[2].outcome, entries[2].outcome);
        assert!(!loaded.entries[2].archived);
    }

    #[test]
    fn legacy_decoder_rejects_oversized_lengths_before_allocation() {
        let mut field = Vec::new();
        write_u32(
            &mut field,
            (MAX_LEGACY_FIELD_BYTES as u32).saturating_add(1),
        );
        let mut cursor = Cursor::new(field.as_slice());
        let error = read_string(&mut cursor).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let mut artifact = Vec::new();
        write_u32(
            &mut artifact,
            (MAX_HISTORY_ARTIFACT_BYTES as u32).saturating_add(1),
        );
        let mut cursor = Cursor::new(artifact.as_slice());
        let error = read_bytes(&mut cursor).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_history_rejects_garbage_and_wrong_magic() {
        assert!(decode_history(b"").is_err());
        assert!(decode_history(b"not-a-history-blob").is_err());
        assert!(decode_history(b"SHIFT_HISTORY_V0\n").is_err());
        let mut truncated = MAGIC.to_vec();
        truncated.extend_from_slice(&1u64.to_le_bytes());
        assert!(decode_history(&truncated).is_err());
    }

    #[test]
    fn intern_module_id_maps_known_and_unknown() {
        for id in REGISTERED_MODULE_IDS {
            assert_eq!(intern_module_id(id), *id, "module {id}");
            let a: *const str = intern_module_id(id);
            let b: *const str = intern_module_id(id);
            assert_eq!(a, b, "stable intern for {id}");
        }
        assert_eq!(intern_module_id("qpdf"), "qpdf");
        assert_eq!(intern_module_id("custom-engine"), "unknown");
        assert_eq!(intern_module_id(""), "unknown");
    }

    #[test]
    fn store_source_path_keeps_relative_and_empty_paths() {
        let relative = PathBuf::from("relative/file.txt");
        assert_eq!(
            store_source_path(&relative),
            relative.to_str().unwrap().to_owned()
        );
        assert_eq!(restore_source_path("relative/file.txt"), relative);

        let empty = PathBuf::from("");
        assert_eq!(store_source_path(&empty), "");
        assert_eq!(restore_source_path(""), PathBuf::from(""));
        // Spaces-only path is preserved (no trim).
        assert_eq!(restore_source_path("   "), PathBuf::from("   "));
    }

    #[test]
    fn archived_flag_persists_through_sqlite() {
        let conn = open_history(":memory:").unwrap();
        let entry = sample_entry(1, "archived-row", true);
        insert_entry(&conn, &entry).unwrap();
        let loaded = history_entries(&conn, true).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].archived);
        assert!(history_entries(&conn, false).unwrap().is_empty());
    }

    #[test]
    fn save_history_delta_to_upserts_and_deletes() {
        let dir = temp_support_dir("delta-to");
        let db_path = dir.join("history.sqlite");

        let e1 = sample_entry(1, "one", false);
        let e2 = sample_entry(2, "two", false);
        save_history_delta_to(&db_path, &[e1.clone(), e2.clone()], &[1, 2], &[]).unwrap();

        let conn = open_history(&db_path).unwrap();
        let loaded = history_entries(&conn, true).unwrap();
        assert_eq!(loaded.len(), 2);

        let e1_updated = sample_entry(1, "one-renamed", true);
        save_history_delta_to(&db_path, std::slice::from_ref(&e1_updated), &[1, 999], &[2])
            .unwrap();
        let loaded = history_entries(&conn, true).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "one-renamed");
        assert!(loaded[0].archived);

        save_history_delta_to(&db_path, &[], &[], &[]).unwrap();
        assert_eq!(history_entries(&conn, true).unwrap().len(), 1);

        drop(conn);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_history_delta_and_load_history_with_support_dir_override() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("delta-env");
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }

        assert_eq!(support_dir().as_deref(), Some(dir.as_path()));
        assert_eq!(
            history_db_path().as_deref(),
            Some(dir.join("history.sqlite").as_path())
        );

        let empty = load_history();
        assert!(empty.entries.is_empty());
        assert_eq!(empty.next_id, 1);
        assert!(!empty.load_incomplete);

        let large = StoredHistoryEntry {
            id: 1,
            source: StoredSource::File(PathBuf::from("/tmp/big.bin")),
            name: "big".to_owned(),
            detail: "large".to_owned(),
            extension_label: "BIN".to_owned(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "binary".to_owned(),
            outcome: StoredOutcome::ReadyLarge {
                module_id: "ffmpeg".to_owned(),
                byte_len: 1_048_576,
            },
            archived: false,
            artifact_deferred: false,
        };
        let failed = StoredHistoryEntry {
            id: 2,
            source: StoredSource::Url("https://example.com/x".to_owned()),
            name: "bad".to_owned(),
            detail: "err".to_owned(),
            extension_label: "MD".to_owned(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "markdown".to_owned(),
            outcome: StoredOutcome::Failed("nope".to_owned()),
            archived: false,
            artifact_deferred: false,
        };
        save_history_delta(&[large, failed], &[1, 2], &[]).unwrap();

        let populated = load_history();
        assert_eq!(populated.entries.len(), 2);
        assert_eq!(populated.next_id, 3);
        assert!(
            populated
                .entries
                .iter()
                .any(|e| matches!(e.outcome, StoredOutcome::ReadyLarge { .. }))
        );

        let kept = sample_entry(1, "only", false);
        save_history(&[kept], 10).unwrap();
        let after = load_history();
        assert_eq!(after.entries.len(), 1);
        assert_eq!(after.entries[0].name, "only");
        // Seq is monotonic and does not reuse deleted ids (id 2 was removed).
        assert!(
            after.next_id >= 3,
            "next_id must stay ahead of prior allocations, got {}",
            after.next_id
        );

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn archive_then_unarchive_via_upsert() {
        let conn = open_history(":memory:").unwrap();
        add_history_entry(&conn, &sample_entry(1, "row", false), DEFAULT_HISTORY_LIMIT).unwrap();
        assert!(archive_history(&conn, 1).unwrap());
        assert!(history_entries(&conn, false).unwrap().is_empty());
        assert!(history_entries(&conn, true).unwrap()[0].archived);

        upsert_history_entry(&conn, &sample_entry(1, "row", false)).unwrap();
        let active = history_entries(&conn, false).unwrap();
        assert_eq!(active.len(), 1);
        assert!(!active[0].archived);
    }

    #[test]
    fn encode_decode_empty_unicode_and_max_artifact() {
        let empty_names = StoredHistoryEntry {
            id: 1,
            source: StoredSource::File(PathBuf::from("")),
            name: String::new(),
            detail: String::new(),
            extension_label: String::new(),
            badge_color: 0,
            badge_text_color: u32::MAX,
            output_format: String::new(),
            outcome: StoredOutcome::Ready {
                module_id: String::new(),
                file_name: String::new(),
                format: String::new(),
                bytes: Vec::new(),
            },
            archived: false,
            artifact_deferred: false,
        };
        let unicode = StoredHistoryEntry {
            id: 2,
            source: StoredSource::File(PathBuf::from("/tmp/文档.pdf")),
            name: "日本語レポート 🎉".to_owned(),
            detail: "détail — café".to_owned(),
            extension_label: "PDF".to_owned(),
            badge_color: 0xFF00_00FF,
            badge_text_color: 0,
            output_format: "markdown".to_owned(),
            outcome: StoredOutcome::Ready {
                module_id: "pandoc".to_owned(),
                file_name: "レポート.md".to_owned(),
                format: "markdown".to_owned(),
                bytes: "見出し".as_bytes().to_vec(),
            },
            archived: false,
            artifact_deferred: false,
        };
        let max_bytes = vec![0x5Au8; MAX_HISTORY_ARTIFACT_BYTES];
        let at_max = StoredHistoryEntry {
            id: 3,
            source: StoredSource::Url("https://example.com/big".to_owned()),
            name: "at-max".to_owned(),
            detail: "d".to_owned(),
            extension_label: "BIN".to_owned(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "binary".to_owned(),
            outcome: StoredOutcome::Ready {
                module_id: "ffmpeg".to_owned(),
                file_name: "big.bin".to_owned(),
                format: "binary".to_owned(),
                bytes: max_bytes.clone(),
            },
            archived: false,
            artifact_deferred: false,
        };
        let large_meta = StoredHistoryEntry {
            id: 4,
            source: StoredSource::File(PathBuf::from("/tmp/huge")),
            name: "huge".to_owned(),
            detail: "d".to_owned(),
            extension_label: "BIN".to_owned(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "binary".to_owned(),
            outcome: StoredOutcome::ReadyLarge {
                module_id: "ffmpeg".to_owned(),
                byte_len: MAX_HISTORY_ARTIFACT_BYTES * 4,
            },
            archived: false,
            artifact_deferred: false,
        };

        let encoded = encode_history(
            &[
                empty_names.clone(),
                unicode.clone(),
                at_max.clone(),
                large_meta.clone(),
            ],
            0,
        );
        let loaded = decode_history(&encoded).unwrap();
        assert_eq!(loaded.next_id, 1);
        assert_eq!(loaded.entries.len(), 4);
        assert_eq!(loaded.entries[0].name, "");
        assert_eq!(loaded.entries[0].outcome, empty_names.outcome);
        assert_eq!(loaded.entries[1].name, unicode.name);
        match &loaded.entries[2].outcome {
            StoredOutcome::Ready { bytes, .. } => assert_eq!(bytes, &max_bytes),
            other => panic!("expected Ready at max, got {other:?}"),
        }
        assert_eq!(loaded.entries[3].outcome, large_meta.outcome);

        let encoded_max_id = encode_history(&[], u64::MAX);
        assert_eq!(decode_history(&encoded_max_id).unwrap().next_id, u64::MAX);
    }

    #[test]
    fn import_legacy_edges_empty_truncated_and_oversized_next_id() {
        let mut conn = open_history(":memory:").unwrap();

        assert!(import_legacy_history(&mut conn, b"").is_err());
        assert!(import_legacy_history(&mut conn, b"SHIFT_HISTORY_V2\n").is_err());
        assert!(import_legacy_history(&mut conn, MAGIC).is_err());

        let mut truncated = MAGIC.to_vec();
        truncated.extend_from_slice(&7u64.to_le_bytes());
        assert!(import_legacy_history(&mut conn, &truncated).is_err());

        let empty_blob = encode_history(&[], u64::MAX);
        let count = import_legacy_history(&mut conn, &empty_blob).unwrap();
        assert_eq!(count, 0);
        assert!(history_entries(&conn, true).unwrap().is_empty());

        let one = encode_history(&[sample_entry(42, "legacy", false)], 100);
        assert_eq!(import_legacy_history(&mut conn, &one).unwrap(), 1);
        let loaded = history_entries(&conn, true).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, 42);
        assert_eq!(loaded[0].name, "legacy");
        assert_eq!(peek_next_history_id(&conn).unwrap(), 100);
    }

    #[test]
    fn decode_history_caps_entries_at_max_history_entries() {
        let many: Vec<_> = (1..=(MAX_HISTORY_ENTRIES as u64 + 5))
            .map(|id| sample_entry(id, "overflow", false))
            .collect();
        let encoded = encode_history(&many, many.len() as u64 + 1);
        let loaded = decode_history(&encoded).unwrap();
        assert_eq!(loaded.entries.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(loaded.entries[0].id, 1);
        assert_eq!(
            loaded.entries.last().map(|e| e.id),
            Some(MAX_HISTORY_ENTRIES as u64)
        );
    }

    #[test]
    fn history_limit_constants_are_sane() {
        const {
            assert!(MIN_HISTORY_LIMIT >= 1);
            assert!(DEFAULT_HISTORY_LIMIT >= MIN_HISTORY_LIMIT);
            assert!(MAX_HISTORY_LIMIT >= DEFAULT_HISTORY_LIMIT);
            assert!(MAX_HISTORY_ENTRIES == DEFAULT_HISTORY_LIMIT);
            assert!(MAX_HISTORY_ARTIFACT_BYTES >= 1024);
            assert!(MAX_HISTORY_TOTAL_ARTIFACT_BYTES >= MAX_HISTORY_ARTIFACT_BYTES);
            assert!(HISTORY_SAVE_MAX_RETRIES >= 1);
            assert!(HISTORY_SAVE_BASE_DELAY_MS >= 1);
        }
    }

    #[test]
    fn add_history_entry_zero_limit_skips_trim() {
        let conn = open_history(":memory:").unwrap();
        add_history_entry(&conn, &sample_entry(1, "a", false), 0).unwrap();
        add_history_entry(&conn, &sample_entry(2, "b", false), 0).unwrap();
        assert_eq!(history_entries(&conn, true).unwrap().len(), 2);
    }

    #[test]
    fn load_history_imports_legacy_blob_once() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("legacy-load");
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }

        let legacy_path = dir.join("history");
        let blob = encode_history(&[sample_entry(7, "from-legacy", false)], 8);
        std::fs::write(&legacy_path, &blob).unwrap();

        let loaded = load_history();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].id, 7);
        assert_eq!(loaded.entries[0].name, "from-legacy");
        assert_eq!(loaded.next_id, 8);
        assert!(dir.join("history.sqlite").is_file());
        assert!(!legacy_path.exists() || dir.join("history.legacy.bak").exists());

        let again = load_history();
        assert_eq!(again.entries.len(), 1);

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_history_quarantines_malformed_legacy_and_keeps_sqlite_rows() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("legacy-fail");
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }

        let legacy_path = dir.join("history");
        std::fs::write(&legacy_path, b"not a valid history blob").unwrap();
        let db_path = history_db_path().unwrap();
        let conn = open_history(&db_path).unwrap();
        add_history_entry(
            &conn,
            &sample_entry(9, "from-sqlite", false),
            DEFAULT_HISTORY_LIMIT,
        )
        .unwrap();
        drop(conn);

        let loaded = load_history();
        assert!(loaded.load_incomplete);
        assert!(loaded.load_error.is_some());
        assert!(
            !legacy_path.exists(),
            "malformed legacy file must be quarantined"
        );
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].name, "from-sqlite");
        assert!(
            std::fs::read_dir(&dir).unwrap().flatten().any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("history.bad.")),
            "quarantine should preserve the malformed bytes under a recoverable name"
        );

        // The quarantined legacy blob must not make every later startup fail.
        let again = load_history();
        assert!(!again.load_incomplete);
        assert!(again.load_error.is_none());
        assert_eq!(again.entries.len(), 1);

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_legacy_history_is_bounded_and_quarantined() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("legacy-oversized");
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }
        let legacy_path = dir.join("history");
        let file = std::fs::File::create(&legacy_path).unwrap();
        file.set_len(MAX_LEGACY_HISTORY_FILE_BYTES + 1).unwrap();
        drop(file);

        let loaded = load_history();
        assert!(loaded.load_incomplete);
        assert!(
            loaded
                .load_error
                .as_deref()
                .is_some_and(|message| message.contains("exceeds"))
        );
        assert!(!legacy_path.exists());

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_history_save_is_skipped_after_clear_epoch_changes() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("stale-save");
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }
        let db_path = history_db_path().unwrap();
        let epoch = history_store_epoch();
        clear_history_store().unwrap();

        let saved = save_history_delta_to_if_current(
            &db_path,
            &[sample_entry(1, "stale", false)],
            &[1],
            &[],
            epoch,
        )
        .unwrap();
        assert!(!saved);
        assert!(
            !db_path.exists(),
            "a stale save must not recreate a cleared store"
        );

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn support_dir_uses_home_when_no_app_support_override() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_support_dir("home");
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
            std::env::set_var("HOME", &home);
        }

        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                support_dir(),
                Some(home.join("Library/Application Support/Shift"))
            );
            assert_eq!(
                history_db_path(),
                Some(home.join("Library/Application Support/Shift/history.sqlite"))
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(support_dir(), Some(home.join(".local/share/shift")));
        }

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn load_and_save_history_fail_without_support_dir() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
            std::env::remove_var("HOME");
        }

        assert!(support_dir().is_none());
        assert!(history_db_path().is_none());
        let empty = load_history();
        assert!(empty.entries.is_empty());
        assert_eq!(empty.next_id, 1);

        let err = save_history(&[sample_entry(1, "x", false)], 2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("home"));

        let err = save_history_delta(&[sample_entry(1, "x", false)], &[1], &[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        clear_history_store().unwrap();

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn load_history_open_failure_preserves_next_id_and_surfaces_error() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("open-fail");
        // A directory where the sqlite file should be makes Connection::open fail.
        std::fs::create_dir_all(dir.join("history.sqlite")).unwrap();
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }

        let loaded = load_history();
        assert!(loaded.entries.is_empty());
        assert!(loaded.load_incomplete);
        assert!(loaded.load_error.is_some());
        // next_id must not reset in a way that invites overwriting — at least 1.
        assert!(loaded.next_id >= 1);

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn decode_history_rejects_unknown_source_and_outcome_kinds() {
        let mut bad_source = MAGIC.to_vec();
        bad_source.extend_from_slice(&1u64.to_le_bytes());
        bad_source.extend_from_slice(&1u32.to_le_bytes());
        bad_source.extend_from_slice(&1u64.to_le_bytes());
        bad_source.push(99);
        bad_source.extend_from_slice(&0u32.to_le_bytes());
        let err = decode_history(&bad_source).unwrap_err();
        assert!(
            err.to_string().contains("unknown history source kind"),
            "error: {err}"
        );

        let mut bad_outcome = MAGIC.to_vec();
        bad_outcome.extend_from_slice(&1u64.to_le_bytes());
        bad_outcome.extend_from_slice(&1u32.to_le_bytes());
        bad_outcome.extend_from_slice(&1u64.to_le_bytes());
        bad_outcome.push(0);
        bad_outcome.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..3 {
            bad_outcome.extend_from_slice(&0u32.to_le_bytes());
        }
        bad_outcome.extend_from_slice(&0u32.to_le_bytes());
        bad_outcome.extend_from_slice(&0u32.to_le_bytes());
        bad_outcome.extend_from_slice(&0u32.to_le_bytes());
        bad_outcome.push(77);
        let err = decode_history(&bad_outcome).unwrap_err();
        assert!(
            err.to_string().contains("unknown history outcome kind"),
            "error: {err}"
        );
    }

    #[test]
    fn history_entries_rejects_invalid_source_and_outcome_kinds() {
        let conn = open_history(":memory:").unwrap();
        conn.execute(
            "INSERT INTO history (
                id, source_kind, source, name, detail, extension_label,
                badge_color, badge_text_color, output_format, outcome_kind, archived
            ) VALUES (1, 9, 'x', 'n', 'd', 'E', 0, 0, 'md', 0, 0)",
            [],
        )
        .unwrap();
        let err = history_entries(&conn, true).unwrap_err();
        assert!(
            matches!(err, rusqlite::Error::IntegralValueOutOfRange(_, 9)),
            "unexpected: {err:?}"
        );

        conn.execute("DELETE FROM history", []).unwrap();
        conn.execute(
            "INSERT INTO history (
                id, source_kind, source, name, detail, extension_label,
                badge_color, badge_text_color, output_format, outcome_kind, archived
            ) VALUES (2, 0, '/tmp/a', 'n', 'd', 'E', 0, 0, 'md', 9, 0)",
            [],
        )
        .unwrap();
        let err = history_entries(&conn, true).unwrap_err();
        assert!(
            matches!(err, rusqlite::Error::IntegralValueOutOfRange(_, 9)),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn restore_source_path_without_home_keeps_tilde_literal() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("HOME");
        }

        assert_eq!(restore_source_path("~"), PathBuf::from("~"));
        assert_eq!(
            restore_source_path("~/Documents/a.txt"),
            PathBuf::from("~/Documents/a.txt")
        );
        assert_eq!(store_source_path(Path::new("/tmp/x")), "/tmp/x".to_owned());

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn load_history_query_failure_preserves_next_id_and_is_incomplete() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("query-fail");
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }

        let db_path = history_db_path().unwrap();
        {
            let conn = open_history(&db_path).unwrap();
            // Insert a valid high-id row so next_id is known, then a corrupt one.
            insert_entry(&conn, &sample_entry(50, "ok", false)).unwrap();
            conn.execute(
                "INSERT INTO history (
                    id, source_kind, source, name, detail, extension_label,
                    badge_color, badge_text_color, output_format, outcome_kind, archived
                ) VALUES (51, 42, 'x', 'n', 'd', 'E', 0, 0, 'md', 0, 0)",
                [],
            )
            .unwrap();
            // Force seq ahead.
            conn.execute(
                "UPDATE history_id_seq SET next_id = 100 WHERE singleton = 1",
                [],
            )
            .unwrap();
        }

        let loaded = load_history();
        // Full-list query fails on the bad row; must not silently report success
        // with next_id=1.
        assert!(loaded.load_incomplete);
        assert!(loaded.load_error.is_some());
        assert!(
            loaded.next_id >= 100,
            "next_id must be preserved from seq, got {}",
            loaded.next_id
        );

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn allocate_history_id_is_monotonic_across_calls() {
        let dir = temp_support_dir("alloc");
        let db = dir.join("history.sqlite");
        let a = allocate_history_id(&db).unwrap();
        let b = allocate_history_id(&db).unwrap();
        let c = allocate_history_id(&db).unwrap();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(c, 3);
        // Explicit insert of a high id advances the allocator.
        save_history_delta_to(&db, &[sample_entry(50, "high", false)], &[50], &[]).unwrap();
        let d = allocate_history_id(&db).unwrap();
        assert!(d >= 51, "allocator must stay ahead of max id, got {d}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn schema_uses_autoincrement() {
        let conn = open_history(":memory:").unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='history'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            sql.to_ascii_uppercase().contains("AUTOINCREMENT"),
            "schema must use AUTOINCREMENT: {sql}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_and_file_modes() {
        let dir = temp_support_dir("modes");
        let db = dir.join("history.sqlite");
        let _ = open_history(&db).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "support dir mode {dir_mode:o}");
        assert_eq!(file_mode, 0o600, "db file mode {file_mode:o}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn history_database_symlinks_are_not_followed_or_cleared() {
        use std::os::unix::fs::symlink;

        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_support_dir("symlink");
        let target = dir.join("outside.sqlite");
        let link = dir.join("history.sqlite");
        let target_conn = open_history(&target).unwrap();
        add_history_entry(
            &target_conn,
            &sample_entry(1, "keep", false),
            DEFAULT_HISTORY_LIMIT,
        )
        .unwrap();
        drop(target_conn);
        symlink(&target, &link).unwrap();

        assert!(
            open_history(&link).is_err(),
            "database symlink must be rejected"
        );
        unsafe {
            std::env::set_var("SHIFT_APP_SUPPORT_DIR", &dir);
        }
        clear_history_store().unwrap();
        assert!(!link.exists(), "clear should remove only the symlink");
        let conn = open_history(&target).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "clearing a symlink must not mutate its target");

        unsafe {
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_round_trip_lossless() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let raw = b"/tmp/bad\xFF\xFEname";
        let path = PathBuf::from(OsStr::from_bytes(raw));
        let stored = store_source_path(&path);
        assert!(stored.starts_with(OS_PATH_PREFIX), "stored={stored}");
        let restored = restore_source_path(&stored);
        assert_eq!(restored.as_os_str().as_bytes(), raw);
    }

    #[test]
    fn deferred_upsert_preserves_existing_blob() {
        let conn = open_history(":memory:").unwrap();
        insert_entry(&conn, &sample_entry(1, "full", false)).unwrap();
        assert_eq!(
            load_history_artifact(&conn, 1).unwrap().as_deref(),
            Some(b"body".as_slice())
        );

        let mut deferred = sample_entry(1, "meta-only", true);
        deferred.artifact_deferred = true;
        if let StoredOutcome::Ready { bytes, .. } = &mut deferred.outcome {
            bytes.clear();
        }
        upsert_history_entry(&conn, &deferred).unwrap();

        assert_eq!(
            load_history_artifact(&conn, 1).unwrap().as_deref(),
            Some(b"body".as_slice()),
            "deferred metadata upsert must not wipe blob"
        );
        let meta = history_entries(&conn, true).unwrap();
        assert_eq!(meta[0].name, "meta-only");
        assert!(meta[0].archived);
    }
}
