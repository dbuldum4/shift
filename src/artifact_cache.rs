//! On-disk cache for conversion artifacts (binary copies under Application Support).

use crate::session_settings::application_support_dir;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const CACHE_DIR_NAME: &str = "artifact-cache";
/// Default TTL for cached artifacts (7 days).
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Soft cap on total cache size before oldest entries are purged (512 MiB).
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Directory for cached conversion binaries.
pub fn artifact_cache_dir() -> Option<PathBuf> {
    if let Some(override_dir) = std::env::var_os("SHIFT_ARTIFACT_CACHE_DIR") {
        return Some(PathBuf::from(override_dir));
    }
    application_support_dir().map(|dir| dir.join(CACHE_DIR_NAME))
}

/// Ensure the cache directory exists and return it.
pub fn ensure_artifact_cache_dir() -> io::Result<PathBuf> {
    let dir = artifact_cache_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate artifact cache directory",
        )
    })?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Store `bytes` under a stable key derived from `name` + content hash prefix.
///
/// Returns the path of the written cache file.
pub fn cache_artifact_bytes(name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let dir = ensure_artifact_cache_dir()?;
    let safe = sanitize_cache_name(name);
    let hash = simple_hash(bytes);
    let safe_path = Path::new(&safe);
    let file_name = match (
        safe_path.file_stem().and_then(|value| value.to_str()),
        safe_path.extension().and_then(|value| value.to_str()),
    ) {
        (Some(stem), Some(extension)) if !stem.is_empty() && !extension.is_empty() => {
            format!("{stem}-{hash:016x}.{extension}")
        }
        _ => format!("{safe}-{hash:016x}"),
    };
    let path = dir.join(file_name);
    if !path.exists() {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

/// Copy an existing file into the cache; returns the cache path.
pub fn cache_artifact_file(source: &Path) -> io::Result<PathBuf> {
    let bytes = fs::read(source)?;
    let name = source
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("artifact");
    cache_artifact_bytes(name, &bytes)
}

/// Remove cache entries older than `ttl` and, if still over `max_bytes`,
/// delete oldest files until under the budget.
pub fn purge_artifact_cache(ttl: Duration, max_bytes: u64) -> io::Result<PurgeStats> {
    let Some(dir) = artifact_cache_dir() else {
        return Ok(PurgeStats::default());
    };
    if !dir.is_dir() {
        return Ok(PurgeStats::default());
    }

    let now = SystemTime::now();
    let mut entries: Vec<CacheEntry> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let meta = entry.metadata()?;
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or_default();
        entries.push(CacheEntry {
            path,
            modified,
            len: meta.len(),
            age,
        });
    }

    let mut stats = PurgeStats::default();
    // Age-based purge.
    entries.retain(|entry| {
        if entry.age > ttl {
            if fs::remove_file(&entry.path).is_ok() {
                stats.removed += 1;
                stats.freed_bytes += entry.len;
            }
            false
        } else {
            true
        }
    });

    // Size budget: oldest first.
    let mut total: u64 = entries.iter().map(|e| e.len).sum();
    if total > max_bytes {
        entries.sort_by_key(|e| e.modified);
        for entry in entries {
            if total <= max_bytes {
                break;
            }
            if fs::remove_file(&entry.path).is_ok() {
                total = total.saturating_sub(entry.len);
                stats.removed += 1;
                stats.freed_bytes += entry.len;
            }
        }
    }

    Ok(stats)
}

/// Purge with default TTL and size budget.
pub fn purge_artifact_cache_defaults() -> io::Result<PurgeStats> {
    purge_artifact_cache(DEFAULT_CACHE_TTL, DEFAULT_CACHE_MAX_BYTES)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PurgeStats {
    pub removed: usize,
    pub freed_bytes: u64,
}

struct CacheEntry {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
    age: Duration,
}

fn sanitize_cache_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out = "artifact".into();
    }
    // Keep names short.
    if out.len() > 64 {
        out.truncate(64);
    }
    out
}

fn simple_hash(bytes: &[u8]) -> u64 {
    // FNV-1a 64-bit — fine for cache keys, not cryptographic.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn caches_and_purges_artifacts() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "shift-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: serialized behind ENV_LOCK.
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let path = cache_artifact_bytes("report.pdf", b"%PDF-fake").unwrap();
        assert!(path.is_file());
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("pdf")
        );
        assert_eq!(fs::read(&path).unwrap(), b"%PDF-fake");

        // Immediate purge with zero TTL removes everything.
        let stats = purge_artifact_cache(Duration::from_secs(0), DEFAULT_CACHE_MAX_BYTES).unwrap();
        assert!(stats.removed >= 1);
        assert!(!path.exists());

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }
}
