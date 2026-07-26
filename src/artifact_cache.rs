//! On-disk cache for conversion artifacts (binary copies under Application Support).

use crate::session_settings::application_support_dir;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const CACHE_DIR_NAME: &str = "artifact-cache";
const EXPORT_SUBDIR: &str = "export";
const PASTE_STAGING_SUBDIR: &str = "paste-staging";
const VERSION_FILE_NAME: &str = ".version";
const CACHE_VERSION: &str = "1";
static STAGING_TOKEN: AtomicU64 = AtomicU64::new(0);

fn staging_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
/// Default TTL for cached artifacts (7 days).
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Soft cap on total cache size before oldest entries are purged (512 MiB).
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// True when `path` is a real directory and not a symbolic link.
///
/// Used to prevent cache purge logic from following a symlink and recursively
/// deleting an unintended target directory.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Directory for cached conversion binaries.
pub fn artifact_cache_dir() -> Option<PathBuf> {
    if let Some(override_dir) = std::env::var_os("SHIFT_ARTIFACT_CACHE_DIR") {
        return Some(PathBuf::from(override_dir));
    }
    application_support_dir().map(|dir| dir.join(CACHE_DIR_NAME))
}

/// Default paste-staging directory under the artifact cache (when no env override).
pub fn default_paste_staging_dir() -> Option<PathBuf> {
    artifact_cache_dir().map(|dir| dir.join(PASTE_STAGING_SUBDIR))
}

/// Ensure the cache directory exists and return it.
///
/// If the on-disk cache predates the current `CACHE_VERSION`, stale entries are
/// removed before the directory is returned so format/layout changes cannot
/// serve obsolete artifacts.
pub fn ensure_artifact_cache_dir() -> io::Result<PathBuf> {
    let dir = artifact_cache_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate artifact cache directory",
        )
    })?;

    // Refuse to operate through a symlink: an attacker (or stale link) could
    // redirect cache operations onto an unrelated directory.
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("artifact cache path is a symlink: {}", dir.display()),
            ));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("artifact cache path is not a directory: {}", dir.display()),
            ));
        }
        Ok(_) => {}
        Err(_) => fs::create_dir_all(&dir)?,
    }

    let version_file = dir.join(VERSION_FILE_NAME);
    let version_matches = fs::read_to_string(&version_file)
        .map(|content| content.trim() == CACHE_VERSION)
        .unwrap_or(false);
    if !version_matches {
        purge_cache_dir(&dir)?;
    }
    fs::write(&version_file, CACHE_VERSION)?;
    Ok(dir)
}

fn purge_cache_dir(dir: &Path) -> io::Result<()> {
    if !is_real_dir(dir) {
        return Ok(());
    }
    for entry in fs::read_dir(dir)?.filter_map(Result::ok) {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(VERSION_FILE_NAME) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            // Never follow symlinks; leave them in place.
            continue;
        }
        let _ = if file_type.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
    Ok(())
}

/// Store `bytes` under a stable key derived from `name` + content hash prefix.
///
/// Returns the path of the written cache file.
pub fn cache_artifact_bytes(name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let _guard = staging_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
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
        write_bytes_via_unique_temp(&path, bytes)?;
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

/// Stage bytes under a user-facing file name for Finder drag-export / Reveal / Open.
///
/// Unlike [`cache_artifact_bytes`], the path prefers the original `file_name` (sanitized)
/// so dragging into Downloads or Documents keeps a readable name. When that name is
/// already occupied by different content, a short content-hash disambiguator is
/// inserted so existing staged files are not overwritten underfoot.
pub fn stage_export_bytes(file_name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let _guard = staging_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let dir = ensure_artifact_cache_dir()?.join(EXPORT_SUBDIR);
    fs::create_dir_all(&dir)?;
    let safe = export_file_name(file_name);
    let hash = simple_hash(bytes);
    let hash_hex = format!("{hash:016x}");

    let preferred = dir.join(&safe);
    if export_file_matches(&preferred, bytes, &hash_hex) {
        return Ok(preferred);
    }

    // Prefer the clean name when free; otherwise disambiguate so we never clobber
    // a different artifact that Finder may still reference.
    let target_name = if preferred.exists() {
        disambiguated_export_name(&safe, hash)
    } else {
        safe.clone()
    };
    let path = dir.join(&target_name);
    if export_file_matches(&path, bytes, &hash_hex) {
        return Ok(path);
    }

    write_export_file(&dir, &target_name, bytes, &hash_hex)?;
    Ok(path)
}

/// Hard-link or copy an existing cache file into the export staging dir under `file_name`.
///
/// Prefer this when the artifact is already on disk so large media is not rewritten.
/// Content is hashed by streaming (not loaded fully into RAM).
pub fn stage_export_file(file_name: &str, source: &Path) -> io::Result<PathBuf> {
    let _guard = staging_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let dir = ensure_artifact_cache_dir()?.join(EXPORT_SUBDIR);
    fs::create_dir_all(&dir)?;
    let safe = export_file_name(file_name);
    let source_meta = fs::metadata(source)?;
    let source_len = source_meta.len();
    let hash = hash_file(source)?;
    let hash_hex = format!("{hash:016x}");

    let preferred = dir.join(&safe);
    if export_file_matches_len(&preferred, source_len, &hash_hex)
        || paths_same_file(source, &preferred)
    {
        let _ = fs::write(hash_sidecar_path(&dir, &safe), &hash_hex);
        return Ok(preferred);
    }

    let target_name = if preferred.exists() && !paths_same_file(source, &preferred) {
        disambiguated_export_name(&safe, hash)
    } else {
        safe.clone()
    };
    let path = dir.join(&target_name);
    if paths_same_file(source, &path) || export_file_matches_len(&path, source_len, &hash_hex) {
        let _ = fs::write(hash_sidecar_path(&dir, &target_name), &hash_hex);
        return Ok(path);
    }

    let tmp = unique_staging_path(&path);
    if fs::hard_link(source, &tmp).is_err() {
        if let Err(error) = fs::copy(source, &tmp) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
    }
    if let Err(error) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    let _ = fs::write(hash_sidecar_path(&dir, &target_name), &hash_hex);
    Ok(path)
}

/// True when `path` is an export-staged file whose hash sidecar matches `bytes`.
pub fn export_matches_bytes(path: &Path, bytes: &[u8]) -> bool {
    let hash_hex = format!("{:016x}", simple_hash(bytes));
    export_file_matches(path, bytes, &hash_hex)
}

fn write_export_file(dir: &Path, name: &str, bytes: &[u8], hash_hex: &str) -> io::Result<()> {
    let path = dir.join(name);
    write_bytes_via_unique_temp(&path, bytes)?;
    let _ = fs::write(hash_sidecar_path(dir, name), hash_hex);
    Ok(())
}

fn unique_staging_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("artifact");
    let token = STAGING_TOKEN.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{name}.{}-{token}.tmp", std::process::id()))
}

fn write_bytes_via_unique_temp(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = unique_staging_path(path);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn hash_sidecar_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!(".{name}.hash"))
}

fn export_file_matches(path: &Path, bytes: &[u8], hash_hex: &str) -> bool {
    export_file_matches_len(path, bytes.len() as u64, hash_hex)
}

fn read_hash_sidecar(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let dir = path.parent().unwrap_or(Path::new(""));
    fs::read_to_string(hash_sidecar_path(dir, file_name))
        .ok()
        .map(|text| text.trim().to_owned())
}

fn export_file_matches_len(path: &Path, len: u64, hash_hex: &str) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if meta.len() != len {
        return false;
    }
    // A mismatching sidecar is authoritative: we only write it when staging, so
    // it means the file content is definitely different. A matching sidecar is
    // not enough by itself (the file may have been edited without updating the
    // sidecar, or mtimes may have equal resolution), so we always verify by
    // hashing the actual file content.
    if let Some(sidecar) = read_hash_sidecar(path) {
        if sidecar != hash_hex {
            return false;
        }
    }
    let Ok(actual) = hash_file(path) else {
        return false;
    };
    format!("{actual:016x}") == hash_hex
}

fn disambiguated_export_name(safe: &str, hash: u64) -> String {
    let short = format!("{hash:08x}");
    let path = Path::new(safe);
    match (
        path.file_stem().and_then(|v| v.to_str()),
        path.extension().and_then(|v| v.to_str()),
    ) {
        (Some(stem), Some(ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!("{stem}-{short}.{ext}")
        }
        _ => format!("{safe}-{short}"),
    }
}

fn export_file_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && *value != "." && *value != "..")
        .unwrap_or("artifact");
    // Keep readable names; only neutralize path separators already stripped by file_name().
    let mut chars: Vec<char> = base
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | '\0' => '_',
            other => other,
        })
        .collect();
    if chars.is_empty() {
        chars = "artifact".chars().collect();
    }
    const MAX_CHARS: usize = 180;
    if chars.len() > MAX_CHARS {
        // Preserve extension when truncating long names (character-based).
        let as_string: String = chars.iter().collect();
        if let Some((stem, ext)) = as_string.rsplit_once('.') {
            let ext_len = ext.chars().count();
            if !ext.is_empty() && !ext.contains('/') && ext_len < 32 {
                let keep = MAX_CHARS.saturating_sub(ext_len + 1);
                let stem: String = stem.chars().take(keep).collect();
                return format!("{stem}.{ext}");
            }
        }
        chars.truncate(MAX_CHARS);
    }
    chars.into_iter().collect()
}

fn paths_same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    // Prefer inode identity (covers hard links) before canonicalize.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) {
            if ma.dev() == mb.dev() && ma.ino() == mb.ino() {
                return true;
            }
        }
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Remove cache entries older than `ttl` and, if still over `max_bytes`,
/// delete oldest files until under the budget.
///
/// Walks the cache root recursively so `export/` and `paste-staging/` are
/// included (hash sidecars and staged media would otherwise accumulate forever).
pub fn purge_artifact_cache(ttl: Duration, max_bytes: u64) -> io::Result<PurgeStats> {
    let Some(dir) = artifact_cache_dir() else {
        return Ok(PurgeStats::default());
    };
    if !is_real_dir(&dir) {
        return Ok(PurgeStats::default());
    }

    let now = SystemTime::now();
    let mut entries: Vec<CacheEntry> = Vec::new();
    collect_cache_files(&dir, &now, &mut entries)?;

    let mut stats = PurgeStats::default();
    // Age-based purge.
    entries.retain(|entry| {
        if entry.age > ttl {
            if remove_cache_path(&entry.path).is_ok() {
                stats.removed += 1;
                stats.freed_bytes += entry.len;
            }
            false
        } else {
            true
        }
    });

    // Size budget: oldest first. Skip pure hash sidecars when summing? Include all files.
    let mut total: u64 = entries.iter().map(|e| e.len).sum();
    if total > max_bytes {
        entries.sort_by_key(|e| e.modified);
        for entry in entries {
            if total <= max_bytes {
                break;
            }
            if remove_cache_path(&entry.path).is_ok() {
                total = total.saturating_sub(entry.len);
                stats.removed += 1;
                stats.freed_bytes += entry.len;
            }
        }
    }

    // Drop any empty subdirectories left behind.
    remove_empty_subdirs(&dir);

    Ok(stats)
}

/// Purge paste-staging files older than `ttl` (env override or default under cache).
pub fn purge_paste_staging(ttl: Duration) -> io::Result<PurgeStats> {
    let dir = if let Some(override_dir) = std::env::var_os("SHIFT_PASTE_STAGING_DIR") {
        PathBuf::from(override_dir)
    } else if let Some(cache) = default_paste_staging_dir() {
        cache
    } else {
        std::env::temp_dir().join("shift-paste-staging")
    };
    if !is_real_dir(&dir) {
        return Ok(PurgeStats::default());
    }

    // When staging lives under the artifact cache, recursive purge already covers it.
    if let Some(cache) = artifact_cache_dir() {
        if dir.starts_with(&cache) {
            return Ok(PurgeStats::default());
        }
    }

    let now = SystemTime::now();
    let mut entries = Vec::new();
    collect_cache_files(&dir, &now, &mut entries)?;
    let mut stats = PurgeStats::default();
    for entry in entries {
        if entry.age > ttl && remove_cache_path(&entry.path).is_ok() {
            stats.removed += 1;
            stats.freed_bytes += entry.len;
        }
    }
    Ok(stats)
}

/// Purge with default TTL and size budget (cache + external paste-staging).
pub fn purge_artifact_cache_defaults() -> io::Result<PurgeStats> {
    let mut stats = purge_artifact_cache(DEFAULT_CACHE_TTL, DEFAULT_CACHE_MAX_BYTES)?;
    let paste = purge_paste_staging(DEFAULT_CACHE_TTL)?;
    stats.removed += paste.removed;
    stats.freed_bytes += paste.freed_bytes;
    Ok(stats)
}

fn collect_cache_files(dir: &Path, now: &SystemTime, out: &mut Vec<CacheEntry>) -> io::Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            // file_type() does not follow symlinks; this keeps us from traversing
            // a symlink to an unrelated directory or deleting a symlink target.
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let meta = entry.metadata()?;
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let age = now.duration_since(modified).unwrap_or_default();
            out.push(CacheEntry {
                path,
                modified,
                len: meta.len(),
                age,
            });
        }
    }
    Ok(())
}

fn remove_cache_path(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

fn remove_empty_subdirs(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && !file_type.is_symlink() {
            // Only removes if empty; ignore errors otherwise.
            let _ = fs::remove_dir(entry.path());
        }
    }
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
    // Keep names short (character count).
    if out.chars().count() > 64 {
        out = out.chars().take(64).collect();
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

fn hash_file(path: &Path) -> io::Result<u64> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    let mut hash: u64 = 0xcbf29ce484222325;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for byte in &buf[..n] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shift-cache-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn caches_and_purges_artifacts_including_export() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("purge");
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

        let export = stage_export_bytes("My Report.md", b"# hello").unwrap();
        assert_eq!(
            export.file_name().and_then(|value| value.to_str()),
            Some("My Report.md")
        );
        assert_eq!(fs::read(&export).unwrap(), b"# hello");
        // Identical content reuses the staged path.
        let export_again = stage_export_bytes("My Report.md", b"# hello").unwrap();
        assert_eq!(export, export_again);
        // Different content does not overwrite the first staged file.
        let export_updated = stage_export_bytes("My Report.md", b"# hello!").unwrap();
        assert_ne!(export_updated, export);
        assert!(
            export_updated
                .file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|n| n.contains("My Report") && n.ends_with(".md"))
        );
        assert_eq!(fs::read(&export).unwrap(), b"# hello");
        assert_eq!(fs::read(&export_updated).unwrap(), b"# hello!");

        // Paste-staging under the cache root is also purged.
        let paste_dir = dir.join(PASTE_STAGING_SUBDIR);
        fs::create_dir_all(&paste_dir).unwrap();
        let paste_file = paste_dir.join("clipboard-image.png");
        fs::write(&paste_file, b"png-bytes").unwrap();

        // Immediate purge with zero TTL removes everything, including export + paste-staging.
        let stats = purge_artifact_cache(Duration::from_secs(0), DEFAULT_CACHE_MAX_BYTES).unwrap();
        assert!(stats.removed >= 3);
        assert!(!path.exists());
        assert!(!export.exists());
        assert!(!export_updated.exists());
        assert!(!paste_file.exists());

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stage_export_file_hardlinks_or_copies() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("export-file");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let source = cache_artifact_bytes("clip.bin", b"binary-payload").unwrap();
        let export = stage_export_file("clip.bin", &source).unwrap();
        assert_eq!(fs::read(&export).unwrap(), b"binary-payload");
        assert!(paths_same_file(&source, &export) || export.is_file());

        // Second stage with same content reuses the path.
        let export2 = stage_export_file("clip.bin", &source).unwrap();
        assert_eq!(export, export2);

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_cache_and_export_staging_reuses_complete_artifact() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("concurrent");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    let cache = cache_artifact_bytes("shared.bin", b"shared-payload").unwrap();
                    stage_export_file("shared.bin", &cache).unwrap()
                })
            })
            .collect();
        let paths: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(paths.iter().all(|path| path == &paths[0]));
        assert_eq!(fs::read(&paths[0]).unwrap(), b"shared-payload");

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn export_file_name_strips_path_components() {
        assert_eq!(export_file_name("../../evil.txt"), "evil.txt");
        assert_eq!(export_file_name("plain.md"), "plain.md");
        assert_eq!(export_file_name(""), "artifact");
    }

    #[test]
    fn export_file_name_truncates_by_chars() {
        let long_stem: String = "あ".repeat(200);
        let name = format!("{long_stem}.md");
        let out = export_file_name(&name);
        assert!(out.ends_with(".md"));
        assert!(out.chars().count() <= 180);
    }

    #[test]
    fn export_matches_bytes_reads_sidecar() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("match");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }
        let path = stage_export_bytes("note.md", b"body").unwrap();
        assert!(export_matches_bytes(&path, b"body"));
        assert!(!export_matches_bytes(&path, b"other"));
        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn export_matches_rejects_same_length_content_edit() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("stale-edit");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }
        let path = stage_export_bytes("note.md", b"body").unwrap();
        assert!(export_matches_bytes(&path, b"body"));
        // Same length, different bytes — must not reuse staged file.
        fs::write(&path, b"xxxx").unwrap();
        assert!(!export_matches_bytes(&path, b"body"));
        // Even if the sidecar still claims the old hash, content wins.
        let sidecar = path.with_file_name(format!(
            ".{}.hash",
            path.file_name().unwrap().to_str().unwrap()
        ));
        assert!(sidecar.is_file() || path.parent().unwrap().join(".note.md.hash").is_file());
        assert!(!export_matches_bytes(&path, b"body"));
        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn version_mismatch_purges_stale_cache_entries() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("version");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        // Simulate an old cache with a stale version file.
        let stale = dir.join("stale-report.pdf");
        fs::write(&stale, b"old").unwrap();
        fs::write(dir.join(VERSION_FILE_NAME), "0").unwrap();

        // Re-ensuring the cache directory should purge the stale entry.
        let ensured = ensure_artifact_cache_dir().unwrap();
        assert_eq!(ensured, dir);
        assert!(!stale.exists(), "stale cache entry should be removed");
        assert_eq!(
            fs::read_to_string(dir.join(VERSION_FILE_NAME))
                .unwrap()
                .trim(),
            CACHE_VERSION
        );

        // A second call with a matching version should leave the directory alone.
        let retained = dir.join("fresh-report.pdf");
        fs::write(&retained, b"new").unwrap();
        let _ = ensure_artifact_cache_dir().unwrap();
        assert!(
            retained.exists(),
            "matching version should not purge entries"
        );

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_artifact_bytes_sanitizes_path_traversal_names() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("traversal");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let path = cache_artifact_bytes("../../evil.pdf", b"%PDF-evil").unwrap();
        assert!(
            path.starts_with(&dir),
            "cached path must stay under cache dir: {}",
            path.display()
        );
        // Single file_name component under cache — not a real parent walk.
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert!(path.is_file());
        let name = path.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        assert!(name.ends_with(".pdf"));
        assert_eq!(fs::read(&path).unwrap(), b"%PDF-evil");

        // sanitize_cache_name itself collapses separators to underscores.
        let sanitized = sanitize_cache_name("../../evil.pdf");
        assert_eq!(sanitized, ".._.._evil.pdf");
        assert!(!sanitized.contains('/'));
        assert_eq!(sanitize_cache_name(""), "artifact");
        assert_eq!(sanitize_cache_name("!!!"), "___");

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_artifact_file_copies_source_content() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("cache-file");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let source_dir = unique_temp_dir("cache-file-src");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("input.wav");
        fs::write(&source, b"RIFF-fake-wav").unwrap();

        let cached = cache_artifact_file(&source).unwrap();
        assert!(cached.starts_with(&dir));
        assert!(cached.is_file());
        assert_eq!(fs::read(&cached).unwrap(), b"RIFF-fake-wav");
        let name = cached.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(name.contains("input") || name.ends_with(".wav"));

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(source_dir);
    }

    #[test]
    fn purge_paste_staging_with_zero_ttl_removes_external_staging_only() {
        let _guard = ENV_LOCK.lock().unwrap();
        let cache_dir = unique_temp_dir("paste-cache");
        let paste_dir = unique_temp_dir("paste-staging");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::create_dir_all(&paste_dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &cache_dir);
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &paste_dir);
        }

        // Keep a regular cache artifact so we can assert paste purge does not touch it.
        let cached = cache_artifact_bytes("keep.bin", b"keep-me").unwrap();
        let paste_file = paste_dir.join("clipboard.png");
        fs::write(&paste_file, b"png").unwrap();

        let stats = purge_paste_staging(Duration::from_secs(0)).unwrap();
        assert!(stats.removed >= 1);
        assert!(!paste_file.exists());
        assert!(
            cached.exists(),
            "purge_paste_staging must not remove artifact-cache files"
        );

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(cache_dir);
        let _ = fs::remove_dir_all(paste_dir);
    }

    #[test]
    fn stage_export_bytes_defaults_blank_names_to_artifact() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("blank-name");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let empty = stage_export_bytes("", b"empty-name").unwrap();
        assert_eq!(empty.file_name().and_then(|n| n.to_str()), Some("artifact"));
        assert_eq!(fs::read(&empty).unwrap(), b"empty-name");

        // Dot / double-dot bases also fall back to "artifact"; different
        // content under that default name is disambiguated rather than clobbered.
        let dot = stage_export_bytes(".", b"dot").unwrap();
        assert_ne!(empty, dot);
        assert!(
            dot.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("artifact"))
        );
        assert_eq!(fs::read(&dot).unwrap(), b"dot");
        assert_eq!(fs::read(&empty).unwrap(), b"empty-name");

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn disambiguated_export_preserves_extension_and_content() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("disambiguate");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let first = stage_export_bytes("report.pdf", b"pdf-one").unwrap();
        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("report.pdf")
        );
        let second = stage_export_bytes("report.pdf", b"pdf-two").unwrap();
        assert_ne!(first, second);
        let second_name = second.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(second_name.starts_with("report-") && second_name.ends_with(".pdf"));
        assert_eq!(fs::read(&first).unwrap(), b"pdf-one");
        assert_eq!(fs::read(&second).unwrap(), b"pdf-two");
        // Same content again reuses the matching staged path (preferred or disambiguated).
        let again = stage_export_bytes("report.pdf", b"pdf-two").unwrap();
        assert_eq!(again, second);

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn purge_artifact_cache_evicts_by_max_bytes() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("max-bytes");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        // Write several sized artifacts with a long TTL so only the size budget triggers.
        let a = cache_artifact_bytes("a.bin", &vec![1u8; 4_000]).unwrap();
        let b = cache_artifact_bytes("b.bin", &vec![2u8; 4_000]).unwrap();
        let c = cache_artifact_bytes("c.bin", &vec![3u8; 4_000]).unwrap();
        assert!(a.is_file() && b.is_file() && c.is_file());

        // Budget small enough that at least one full file must be removed.
        let stats = purge_artifact_cache(Duration::from_secs(86_400), 5_000).unwrap();
        assert!(
            stats.removed >= 1,
            "size budget should free at least one entry, freed={}",
            stats.freed_bytes
        );
        assert!(stats.freed_bytes > 0);

        let remaining: u64 = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter(|e| e.file_name() != VERSION_FILE_NAME)
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum();
        assert!(
            remaining <= 5_000,
            "remaining cache payload {remaining} should be under budget"
        );

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn artifact_cache_dir_and_paste_staging_honor_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("env-override");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        assert_eq!(artifact_cache_dir().as_deref(), Some(dir.as_path()));
        assert_eq!(
            default_paste_staging_dir().as_deref(),
            Some(dir.join(PASTE_STAGING_SUBDIR).as_path())
        );

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn export_matches_bytes_false_for_missing_path() {
        let missing = std::env::temp_dir().join(format!(
            "shift-missing-export-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert!(!missing.exists());
        assert!(!export_matches_bytes(&missing, b"anything"));
    }

    #[test]
    fn sanitize_cache_name_edge_cases() {
        assert_eq!(sanitize_cache_name(""), "artifact");
        assert_eq!(sanitize_cache_name("   "), "___");
        assert_eq!(sanitize_cache_name("!!!@@@"), "______");
        assert_eq!(sanitize_cache_name("a/b\\c\0d"), "a_b_c_d");
        // Control characters become underscores.
        assert_eq!(sanitize_cache_name("a\nb\tc\rd"), "a_b_c_d");
        // Unicode non-ASCII letters are replaced (ascii_alphanumeric only).
        assert_eq!(sanitize_cache_name("café.pdf"), "caf_.pdf");
        assert_eq!(sanitize_cache_name("日本語.md"), "___.md");
        assert_eq!(
            sanitize_cache_name("report-v1_final.PDF"),
            "report-v1_final.PDF"
        );
        // Character-based truncation at 64.
        let long: String = "a".repeat(100);
        let truncated = sanitize_cache_name(&long);
        assert_eq!(truncated.chars().count(), 64);
        assert!(truncated.chars().all(|c| c == 'a'));
        let long_unicode: String = "あ".repeat(80);
        assert_eq!(sanitize_cache_name(&long_unicode).chars().count(), 64);
        // Path-like names collapse separators but keep the extension shape.
        assert_eq!(sanitize_cache_name("a/b/c.md"), "a_b_c.md");
        assert_eq!(sanitize_cache_name(".."), "..");
        assert_eq!(sanitize_cache_name("."), ".");
    }

    #[test]
    fn purge_empty_dir_and_export_only_subdir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let empty = unique_temp_dir("purge-empty");
        fs::create_dir_all(&empty).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &empty);
        }

        // Empty real directory: no files to remove.
        let stats = purge_artifact_cache(Duration::from_secs(0), DEFAULT_CACHE_MAX_BYTES).unwrap();
        assert_eq!(stats.removed, 0);
        assert_eq!(stats.freed_bytes, 0);
        assert!(empty.is_dir());

        // Only export/ subtree populated — recursive purge still clears it.
        let export_dir = empty.join(EXPORT_SUBDIR);
        fs::create_dir_all(&export_dir).unwrap();
        let staged = export_dir.join("only.md");
        fs::write(&staged, b"# only export").unwrap();
        let sidecar = export_dir.join(".only.md.hash");
        fs::write(&sidecar, "deadbeef").unwrap();

        let stats = purge_artifact_cache(Duration::from_secs(0), DEFAULT_CACHE_MAX_BYTES).unwrap();
        assert!(stats.removed >= 2, "expected export file + sidecar removed");
        assert!(!staged.exists());
        assert!(!sidecar.exists());

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(empty);
    }

    #[test]
    fn cache_artifact_file_missing_source_errors() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("missing-src");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let missing = dir.join("does-not-exist.bin");
        let err = cache_artifact_file(&missing).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stage_export_strips_path_like_names() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("pathlike-export");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let path = stage_export_bytes("a/b/c.md", b"# nested name").unwrap();
        assert_eq!(
            path.parent().unwrap().file_name().and_then(|n| n.to_str()),
            Some(EXPORT_SUBDIR)
        );
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("c.md"),
            "path components must be stripped to the final file name"
        );
        assert_eq!(fs::read(&path).unwrap(), b"# nested name");

        // On Unix, `\` is not a path separator so Path::file_name keeps the whole
        // string; export_file_name still neutralizes backslashes to underscores.
        let win = stage_export_bytes("dir\\sub\\note.txt", b"win").unwrap();
        let win_name = win.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(!win_name.contains('\\'));
        assert!(win_name.ends_with("note.txt") || win_name == "dir_sub_note.txt");
        assert_eq!(fs::read(&win).unwrap(), b"win");

        // Traversal-style names must not escape the export dir.
        let evil = stage_export_bytes("../../evil.md", b"nope").unwrap();
        assert!(evil.starts_with(dir.join(EXPORT_SUBDIR)));
        assert_eq!(evil.file_name().and_then(|n| n.to_str()), Some("evil.md"));

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_constants_are_sane() {
        const {
            assert!(DEFAULT_CACHE_TTL.as_secs() == 7 * 24 * 60 * 60);
            assert!(DEFAULT_CACHE_MAX_BYTES == 512 * 1024 * 1024);
            assert!(DEFAULT_CACHE_TTL.as_secs() > 0);
            assert!(DEFAULT_CACHE_MAX_BYTES > 0);
            // TTL is multi-day; size budget is at least tens of MiB.
            assert!(DEFAULT_CACHE_TTL.as_secs() >= 24 * 60 * 60);
            assert!(DEFAULT_CACHE_MAX_BYTES >= 64 * 1024 * 1024);
        }
    }

    #[test]
    fn concurrent_purge_and_write_completes() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp_dir("concurrent-purge");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        // Seed a few files so purge has work to do.
        for i in 0..4 {
            let _ = cache_artifact_bytes(&format!("seed-{i}.bin"), &[i as u8; 64]);
        }

        // Writers tolerate race errors with purge (no shared lock between purge and write).
        let writers: Vec<_> = (0..4)
            .map(|i| {
                std::thread::spawn(move || {
                    cache_artifact_bytes(&format!("live-{i}.bin"), &[0xABu8; 32])
                })
            })
            .collect();
        let purger = std::thread::spawn(|| {
            purge_artifact_cache(Duration::from_secs(0), DEFAULT_CACHE_MAX_BYTES)
        });

        let write_results: Vec<_> = writers
            .into_iter()
            .map(|h| h.join().expect("writer thread"))
            .collect();
        let purge_result = purger.join().expect("purger thread");
        assert!(
            purge_result.is_ok(),
            "purge should not hard-fail: {purge_result:?}"
        );
        // At least some writer attempts completed (Ok or Err — no panics).
        assert_eq!(write_results.len(), 4);

        // After the race settles, a serial write must succeed.
        let after = cache_artifact_bytes("after.bin", b"ok").unwrap();
        assert!(after.is_file());
        assert_eq!(fs::read(&after).unwrap(), b"ok");

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn purge_missing_or_non_dir_cache_is_noop() {
        let _guard = ENV_LOCK.lock().unwrap();
        let missing = unique_temp_dir("purge-missing");
        // Do not create the directory.
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &missing);
        }
        let stats = purge_artifact_cache(Duration::from_secs(0), 1).unwrap();
        assert_eq!(stats, PurgeStats::default());

        // File path (not a real directory) is also a no-op via is_real_dir.
        let file_path = unique_temp_dir("purge-file-path");
        fs::write(&file_path, b"not-a-dir").unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &file_path);
        }
        let stats = purge_artifact_cache(Duration::from_secs(0), 1).unwrap();
        assert_eq!(stats, PurgeStats::default());
        assert!(file_path.is_file());

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_file(&file_path);
        let _ = fs::remove_dir_all(&missing);
    }

    #[test]
    fn simple_hash_is_stable_and_sensitive() {
        assert_eq!(simple_hash(b""), simple_hash(b""));
        assert_ne!(simple_hash(b"a"), simple_hash(b"b"));
        assert_eq!(simple_hash(b"payload"), simple_hash(b"payload"));
        // FNV-1a empty seed.
        assert_eq!(simple_hash(b""), 0xcbf29ce484222325);
    }
}
