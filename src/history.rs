//! Persistent conversion history for the native app.
//!
//! Stored under Application Support next to module priority. Artifact bytes are
//! capped the same way as the in-memory session list so the file cannot grow
//! without bound.

use std::io::{self, Read, Write};
use std::path::PathBuf;

/// Cap retained history so large conversion artifacts cannot grow without bound.
pub const MAX_HISTORY_ENTRIES: usize = 30;
/// Full artifact bytes retained per history entry; larger results store metadata only.
pub const MAX_HISTORY_ARTIFACT_BYTES: usize = 512 * 1024;

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
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadedHistory {
    pub entries: Vec<StoredHistoryEntry>,
    pub next_id: u64,
}

/// Application Support path for the history file, when HOME is available.
pub fn history_path() -> Option<PathBuf> {
    support_dir().map(|dir| dir.join("history"))
}

/// Parent Application Support directory used by preferences and history.
pub fn support_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support/Shift"))
}

/// Load history from disk. Missing or corrupt files yield an empty list.
pub fn load_history() -> LoadedHistory {
    let Some(path) = history_path() else {
        return LoadedHistory {
            entries: Vec::new(),
            next_id: 1,
        };
    };
    match std::fs::read(&path) {
        Ok(bytes) => decode_history(&bytes).unwrap_or_else(|_| LoadedHistory {
            entries: Vec::new(),
            next_id: 1,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => LoadedHistory {
            entries: Vec::new(),
            next_id: 1,
        },
        Err(_) => LoadedHistory {
            entries: Vec::new(),
            next_id: 1,
        },
    }
}

/// Persist history atomically (write temp + rename). Truncates to
/// [`MAX_HISTORY_ENTRIES`] before writing.
pub fn save_history(entries: &[StoredHistoryEntry], next_id: u64) -> io::Result<()> {
    let Some(path) = history_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the user home directory",
        ));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let truncated: Vec<_> = entries.iter().take(MAX_HISTORY_ENTRIES).cloned().collect();
    let payload = encode_history(&truncated, next_id);

    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&payload)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Remove the on-disk history file (no-op if missing).
pub fn clear_history_store() -> io::Result<()> {
    let Some(path) = history_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the user home directory",
        ));
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

fn encode_history(entries: &[StoredHistoryEntry], next_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + entries.len() * 128);
    out.extend_from_slice(MAGIC);
    write_u64(&mut out, next_id);
    write_u32(&mut out, entries.len() as u32);
    for entry in entries {
        write_entry(&mut out, entry);
    }
    out
}

fn decode_history(bytes: &[u8]) -> io::Result<LoadedHistory> {
    let mut cursor = io::Cursor::new(bytes);
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

fn write_entry(out: &mut Vec<u8>, entry: &StoredHistoryEntry) {
    write_u64(out, entry.id);
    match &entry.source {
        StoredSource::File(path) => {
            out.push(0);
            write_string(out, &path.to_string_lossy());
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

fn read_entry(cursor: &mut io::Cursor<&[u8]>) -> io::Result<StoredHistoryEntry> {
    let id = read_u64(cursor)?;
    let source_kind = read_u8(cursor)?;
    let source_raw = read_string(cursor)?;
    let source = match source_kind {
        0 => StoredSource::File(PathBuf::from(source_raw)),
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
        2 => StoredOutcome::Failed(read_string(cursor)?),
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
    })
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    write_bytes(out, value.as_bytes());
}

fn write_bytes(out: &mut Vec<u8>, value: &[u8]) {
    write_u32(out, value.len() as u32);
    out.extend_from_slice(value);
}

fn read_u8(cursor: &mut io::Cursor<&[u8]>) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    cursor.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn read_u32(cursor: &mut io::Cursor<&[u8]>) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    cursor.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(cursor: &mut io::Cursor<&[u8]>) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    cursor.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_bytes(cursor: &mut io::Cursor<&[u8]>) -> io::Result<Vec<u8>> {
    let len = read_u32(cursor)? as usize;
    let mut buf = vec![0u8; len];
    cursor.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_string(cursor: &mut io::Cursor<&[u8]>) -> io::Result<String> {
    let bytes = read_bytes(cursor)?;
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests that touch HOME / disk so they do not race.
    static LOCK: Mutex<()> = Mutex::new(());

    fn with_temp_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "shift-history-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let previous = std::env::var_os("HOME");
        // SAFETY: tests hold LOCK so only one mutates HOME at a time.
        unsafe {
            std::env::set_var("HOME", &dir);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&dir)));
        match previous {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn sample_entry(id: u64) -> StoredHistoryEntry {
        StoredHistoryEntry {
            id,
            source: StoredSource::File(PathBuf::from("/tmp/report.docx")),
            name: "report.docx".into(),
            detail: "Markdown  ·  via pandoc".into(),
            extension_label: "DOCX".into(),
            badge_color: 0x1a1a1a,
            badge_text_color: 0xcccccc,
            output_format: "markdown".into(),
            outcome: StoredOutcome::Ready {
                module_id: "pandoc".into(),
                file_name: "report.md".into(),
                format: "markdown".into(),
                bytes: b"# Hello\n".to_vec(),
            },
        }
    }

    #[test]
    fn round_trip_ready_url_and_failed_entries() {
        let entries = vec![
            sample_entry(1),
            StoredHistoryEntry {
                id: 2,
                source: StoredSource::Url("https://example.com/a".into()),
                name: "example.com".into(),
                detail: "HTML  ·  via defuddle".into(),
                extension_label: "URL".into(),
                badge_color: 0x111111,
                badge_text_color: 0x888888,
                output_format: "html".into(),
                outcome: StoredOutcome::ReadyLarge {
                    module_id: "defuddle".into(),
                    byte_len: 900_000,
                },
            },
            StoredHistoryEntry {
                id: 3,
                source: StoredSource::File(PathBuf::from("/tmp/broken.pdf")),
                name: "broken.pdf".into(),
                detail: "Markdown  ·  failed".into(),
                extension_label: "PDF".into(),
                badge_color: 1,
                badge_text_color: 2,
                output_format: "markdown".into(),
                outcome: StoredOutcome::Failed("engine missing".into()),
            },
        ];

        let encoded = encode_history(&entries, 4);
        let loaded = decode_history(&encoded).unwrap();
        assert_eq!(loaded.next_id, 4);
        assert_eq!(loaded.entries, entries);
    }

    #[test]
    fn save_load_and_clear_on_disk() {
        with_temp_home(|_| {
            let entries = vec![sample_entry(7)];
            save_history(&entries, 8).unwrap();
            let loaded = load_history();
            assert_eq!(loaded.next_id, 8);
            assert_eq!(loaded.entries, entries);

            clear_history_store().unwrap();
            let empty = load_history();
            assert!(empty.entries.is_empty());
            assert_eq!(empty.next_id, 1);
        });
    }

    #[test]
    fn corrupt_file_yields_empty_history() {
        with_temp_home(|_| {
            let path = history_path().unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"not a history file").unwrap();
            let loaded = load_history();
            assert!(loaded.entries.is_empty());
            assert_eq!(loaded.next_id, 1);
        });
    }

    #[test]
    fn intern_module_id_maps_known_engines() {
        assert_eq!(intern_module_id("pandoc"), "pandoc");
        assert_eq!(intern_module_id("nope"), "unknown");
    }

    /// UI sidebar load path: decode a full history file must stay snappy.
    #[test]
    fn encode_decode_full_sidebar_stays_within_budget() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        let mut entries = Vec::with_capacity(MAX_HISTORY_ENTRIES);
        for id in 1..=MAX_HISTORY_ENTRIES as u64 {
            let mut entry = sample_entry(id);
            entry.name = format!("report-{id}.docx");
            entry.detail = format!("Markdown  ·  via pandoc  ·  #{id}");
            entry.outcome = if id % 4 == 0 {
                StoredOutcome::Failed(format!("missing tool {id}"))
            } else if id % 4 == 1 {
                StoredOutcome::ReadyLarge {
                    module_id: "ffmpeg".into(),
                    byte_len: 12_000_000 + id as usize,
                }
            } else {
                StoredOutcome::Ready {
                    module_id: "pandoc".into(),
                    file_name: format!("report-{id}.md"),
                    format: "markdown".into(),
                    bytes: format!("# Note {id}\n\n").repeat(128).into_bytes(),
                }
            };
            entries.push(entry);
        }

        let start = Instant::now();
        for _ in 0..100 {
            let encoded = encode_history(&entries, MAX_HISTORY_ENTRIES as u64 + 1);
            let loaded = decode_history(&encoded).expect("decode");
            assert_eq!(loaded.entries.len(), MAX_HISTORY_ENTRIES);
            black_box(loaded.next_id);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed <= Duration::from_secs(2),
            "history encode/decode×100 took {elapsed:?}"
        );
    }

    #[test]
    fn intern_module_id_is_hot_path_cheap() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        let ids = [
            "markitdown",
            "pandoc",
            "defuddle",
            "docling",
            "ffmpeg",
            "custom",
            "",
        ];
        let start = Instant::now();
        for _ in 0..50_000 {
            for id in ids {
                black_box(intern_module_id(id));
            }
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed <= Duration::from_secs(1),
            "intern_module_id×350k took {elapsed:?}"
        );
    }
}
