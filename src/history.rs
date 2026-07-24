//! Persistent conversion history backed by SQLite.
//!
//! History lives under Application Support in a SQLite database with an FTS5
//! virtual table for full-text search. Legacy binary blobs are imported once and
//! then moved aside so they cannot block subsequent launches.

use rusqlite::{Connection, params, params_from_iter};
use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};

/// Full artifact bytes retained per history entry; larger results store metadata only.
pub const MAX_HISTORY_ARTIFACT_BYTES: usize = 512 * 1024;
/// Default cap for retained history entries.
pub const DEFAULT_HISTORY_LIMIT: usize = 30;
/// Minimum persisted history limit.
pub const MIN_HISTORY_LIMIT: usize = 1;
/// Maximum persisted history limit.
pub const MAX_HISTORY_LIMIT: usize = 30_000;
/// Kept for callers that used the older constant name.
pub const MAX_HISTORY_ENTRIES: usize = DEFAULT_HISTORY_LIMIT;

const MAGIC: &[u8] = b"SHIFT_HISTORY_V1\n";

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
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadedHistory {
    pub entries: Vec<StoredHistoryEntry>,
    pub next_id: u64,
}

/// Application Support path for the SQLite history store, when HOME is available.
pub fn history_db_path() -> Option<PathBuf> {
    support_dir().map(|dir| dir.join("history.sqlite"))
}

/// Path to the legacy binary history blob.
fn history_legacy_path() -> Option<PathBuf> {
    support_dir().map(|dir| dir.join("history"))
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

/// Store a file source in a privacy-friendlier form: absolute paths under the
/// user's home directory are converted to a `~` prefix so the history database
/// does not embed the full home directory name.
fn store_source_path(path: &Path) -> String {
    if let Some(home) = home_dir_for_history() {
        if let Ok(rest) = path.strip_prefix(&home) {
            let rest = rest.to_string_lossy();
            if rest.is_empty() {
                return "~".to_owned();
            }
            return format!("~/{rest}");
        }
    }
    path.to_string_lossy().into_owned()
}

/// Reverse [`store_source_path`], expanding `~/` to the current home directory.
fn restore_source_path(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw == "~" {
        if let Some(home) = home_dir_for_history() {
            return home;
        }
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home_dir_for_history() {
            return home.join(rest);
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
        "ffmpeg" => "ffmpeg",
        _ => "unknown",
    }
}

fn initialize_history_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS history (
            id INTEGER PRIMARY KEY,
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
        ",
    )
}

/// Open (or create) the history database at the given path and ensure the schema exists.
pub fn open_history(path: impl AsRef<Path>) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(path)?;
    initialize_history_schema(&conn)?;
    Ok(conn)
}

/// Load history from disk. Missing or corrupt stores yield an empty list.
pub fn load_history() -> LoadedHistory {
    let Some(db_path) = history_db_path() else {
        return LoadedHistory {
            entries: Vec::new(),
            next_id: 1,
        };
    };
    let legacy_path = history_legacy_path();

    let mut legacy_bytes: Option<Vec<u8>> = None;
    if !db_path.exists() {
        if let Some(ref legacy) = legacy_path {
            if legacy.exists() {
                if let Ok(bytes) = std::fs::read(legacy) {
                    legacy_bytes = Some(bytes);
                }
            }
        }
    }

    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let conn = match open_history(&db_path) {
        Ok(conn) => conn,
        Err(_) => {
            return LoadedHistory {
                entries: Vec::new(),
                next_id: 1,
            };
        }
    };

    if let Some(ref legacy) = legacy_path {
        if let Some(bytes) = legacy_bytes {
            if import_legacy_history(&conn, &bytes).is_ok() {
                let backup = legacy.with_extension("legacy.bak");
                let _ = std::fs::rename(legacy, &backup);
            }
        }
    }

    match history_entries(&conn, true) {
        Ok(entries) => {
            let max_id = entries.iter().map(|e| e.id).max().unwrap_or(0);
            let next_id = max_id.saturating_add(1).max(1);
            LoadedHistory { entries, next_id }
        }
        Err(_) => LoadedHistory {
            entries: Vec::new(),
            next_id: 1,
        },
    }
}

/// Incrementally persist history changes to SQLite.
///
/// Only the rows named in `changed_ids` (upserted from `entries`) and
/// `deleted_ids` (removed) are touched; every other stored row is left intact.
/// This avoids the O(n) rewrite of [`save_history`], which deletes and
/// re-inserts every row on each save. IDs present in `changed_ids` but missing
/// from `entries` are skipped, and IDs listed in both `deleted_ids` and
/// `changed_ids` are deleted (deletions run first).
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
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
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

    tx.commit()
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

/// Persist the in-memory history list to SQLite by fully reconciling the stored
/// rows with `entries`: every entry is upserted and any stored row absent from
/// `entries` is deleted. This preserves the historical full-replace semantics
/// while routing through [`save_history_delta`]. Prefer `save_history_delta`
/// directly when the caller can track dirty and deleted IDs. The `next_id` is
/// retained from the in-memory view but recomputed from the stored IDs on load.
pub fn save_history(entries: &[StoredHistoryEntry], _next_id: u64) -> io::Result<()> {
    let Some(db_path) = history_db_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the user home directory",
        ));
    };
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
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

    save_history_delta(entries, &changed_ids, &deleted_ids)
}

/// Return the IDs of every row currently stored in the history table.
fn stored_history_ids(conn: &Connection) -> Result<Vec<u64>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT id FROM history")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0).map(|id| id as u64))?;
    rows.collect()
}

/// Remove the on-disk history store (no-op if missing).
pub fn clear_history_store() -> io::Result<()> {
    if let Some(db_path) = history_db_path() {
        let _ = std::fs::remove_file(&db_path);
    }
    if let Some(legacy) = history_legacy_path() {
        let _ = std::fs::remove_file(&legacy);
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
    let (source_kind, source) = match &entry.source {
        StoredSource::File(path) => (0i64, store_source_path(path)),
        StoredSource::Url(url) => (1i64, url.clone()),
    };

    let (outcome_kind, module_id, file_name, format, artifact_bytes, byte_len, error_message) =
        match &entry.outcome {
            StoredOutcome::Ready {
                module_id,
                file_name,
                format,
                bytes,
            } => (
                0i64,
                Some(module_id.as_str()),
                Some(file_name.as_str()),
                Some(format.as_str()),
                Some(bytes.as_slice()),
                None::<i64>,
                None::<&str>,
            ),
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

/// Return all history entries, optionally including archived rows.
pub fn history_entries(
    conn: &Connection,
    include_archived: bool,
) -> Result<Vec<StoredHistoryEntry>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT * FROM history WHERE (archived = 0 OR ?1 = 1) ORDER BY created_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![include_archived as i64], row_to_entry)?;
    rows.collect()
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

/// Decode a legacy binary blob and import the rows it contains.
pub fn import_legacy_history(conn: &Connection, bytes: &[u8]) -> io::Result<usize> {
    let loaded = decode_history(bytes)?;
    let mut count = 0;
    for entry in &loaded.entries {
        if insert_entry(conn, entry).is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

fn row_to_entry(row: &rusqlite::Row) -> Result<StoredHistoryEntry, rusqlite::Error> {
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

    let outcome = match outcome_kind {
        0 => StoredOutcome::Ready {
            module_id: module_id.unwrap_or_default(),
            file_name: file_name.unwrap_or_default(),
            format: format.unwrap_or_default(),
            bytes: artifact_bytes.unwrap_or_default(),
        },
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
        }
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
        assert_eq!(
            store_source_path(&outside),
            outside.to_string_lossy().into_owned()
        );

        assert_eq!(restore_source_path("~/Documents/report.docx"), under_home);
        assert_eq!(restore_source_path("~"), home);
        assert_eq!(restore_source_path("/tmp/sample.txt"), outside);
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

        // Pin created_at so we can prove the upsert does not overwrite it.
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

        // Empty list and unknown ids are no-ops.
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

        let conn = open_history(":memory:").unwrap();
        let count = import_legacy_history(&conn, &legacy).unwrap();
        assert_eq!(count, 2);

        let loaded = history_entries(&conn, true).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(!loaded[0].archived);
    }
}
