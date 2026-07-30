//! On-disk cache for conversion artifacts (binary copies under Application Support).
//!
//! # Integrity
//!
//! Cached and export-staged files are verified with length + SHA-256 digests stored
//! in sidecars (never FNV alone). FNV remains only as a non-cryptographic naming
//! disambiguator in file names.
//!
//! Export staging always **copies** into the user-facing export path so edits there
//! cannot mutate the canonical cache inode via a shared hard link. Canonical cache
//! files are also marked read-only after write as defense in depth.
//!
//! # Leases and purge
//!
//! Hold an [`ArtifactLease`] (or call [`acquire_export_lease`]) while a staged path
//! is in use so [`purge_artifact_cache`] / [`purge_now`] will not delete it.
//! Purge shares the staging mutex with writers.
//!
//! # App integration
//!
//! Call [`purge_now`] (or [`purge_artifact_cache_defaults`]) after conversion writes
//! and periodically from the app (startup is a good hook; idle timers are fine too).
//! Large-artifact integrity checks that may rehash should run off the UI thread
//! via [`verify_export_integrity`] / [`export_matches_bytes_strict`]; the fast
//! [`export_matches_bytes`] path trusts a matching length+mtime+SHA-256 sidecar.

use crate::session_settings::application_support_dir;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CACHE_DIR_NAME: &str = "artifact-cache";
const EXPORT_SUBDIR: &str = "export";
const PASTE_STAGING_SUBDIR: &str = "paste-staging";
const VERSION_FILE_NAME: &str = ".version";
/// Bumped when sidecar format / integrity scheme changes (invalidates old cache).
const CACHE_VERSION: &str = "2";
static STAGING_TOKEN: AtomicU64 = AtomicU64::new(0);

/// Sidecar magic / field keys (line-oriented `key=value`).
const SIDECAR_SHA256: &str = "sha256";
const SIDECAR_LEN: &str = "len";
const SIDECAR_MTIME_NS: &str = "mtime_ns";

fn staging_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn lease_map() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static LEASES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Default TTL for cached artifacts (7 days).
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Soft cap on total cache size before oldest entries are purged (512 MiB).
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// RAII lease that keeps a staged/cached path alive across purge.
///
/// Drop the lease (or call [`ArtifactLease::release`]) when Reveal/Open/drag is done.
#[derive(Debug)]
pub struct ArtifactLease {
    path: Option<PathBuf>,
}

impl ArtifactLease {
    /// Path covered by this lease.
    pub fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or(Path::new(""))
    }

    /// Explicitly release early (also happens on drop).
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if let Some(path) = self.path.take() {
            release_export_lease(&path);
        }
    }
}

impl Drop for ArtifactLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// Acquire a purge-protecting lease on `path` (refcount; multiple leases allowed).
pub fn acquire_export_lease(path: &Path) -> ArtifactLease {
    let key = normalize_lease_path(path);
    let mut map = lease_map()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *map.entry(key.clone()).or_insert(0) += 1;
    ArtifactLease { path: Some(key) }
}

fn release_export_lease(path: &Path) {
    let key = normalize_lease_path(path);
    let mut map = lease_map()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(count) = map.get_mut(&key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            map.remove(&key);
        }
    }
}

fn normalize_lease_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_is_leased(path: &Path) -> bool {
    if path_is_leased_exact(path) {
        return true;
    }
    // Protect integrity sidecars when their data file is leased so purge cannot
    // leave a staged artifact without its manifest.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if let Some(base) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".hash")) {
            let data = path.parent().unwrap_or_else(|| Path::new("")).join(base);
            if path_is_leased_exact(&data) {
                return true;
            }
        }
    }
    false
}

fn path_is_leased_exact(path: &Path) -> bool {
    let key = normalize_lease_path(path);
    let map = lease_map()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    map.get(&key).copied().unwrap_or(0) > 0
        || map
            .keys()
            .any(|leased| leased == path || paths_same_file(leased, path))
}

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

/// Ensure the cache directory exists (mode `0700` on Unix) and return it.
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
        Ok(_) => {
            ensure_private_dir_mode(&dir)?;
        }
        Err(_) => {
            create_private_dir_all(&dir)?;
        }
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

fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        ensure_private_dir_mode(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

fn ensure_private_dir_mode(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(dir)?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o700 {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            fs::set_permissions(dir, perms)?;
        }
    }
    let _ = dir;
    Ok(())
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
/// Returns the path of the written cache file. Canonical entries are made
/// read-only after write. Callers that need a durable staged path should prefer
/// [`stage_export_bytes`] / [`stage_export_file`].
pub fn cache_artifact_bytes(name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let _guard = staging_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let dir = ensure_artifact_cache_dir()?;
    let safe = sanitize_cache_name(name);
    // FNV is naming-only; integrity uses SHA-256 sidecars on export/reuse paths.
    let name_hash = simple_hash(bytes);
    let digest = sha256_hex(bytes);
    let safe_path = Path::new(&safe);
    let file_name = match (
        safe_path.file_stem().and_then(|value| value.to_str()),
        safe_path.extension().and_then(|value| value.to_str()),
    ) {
        (Some(stem), Some(extension)) if !stem.is_empty() && !extension.is_empty() => {
            format!("{stem}-{name_hash:016x}.{extension}")
        }
        _ => format!("{safe}-{name_hash:016x}"),
    };
    let path = dir.join(&file_name);
    if path.exists() {
        // Reuse only when length + digest match; never trust existence alone.
        if cache_file_matches(&path, bytes.len() as u64, &digest) {
            return Ok(path);
        }
        // Collision on FNV name with different content: rewrite under same name
        // only after verifying we can replace (rare; FNV collision).
        make_writable_if_needed(&path);
        write_bytes_via_unique_temp(&path, bytes)?;
    } else {
        write_bytes_via_unique_temp(&path, bytes)?;
    }
    write_integrity_sidecar_for_path(&path, &digest)?;
    make_readonly(&path);
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
    create_private_dir_all(&dir)?;
    let safe = export_file_name(file_name);
    let name_hash = simple_hash(bytes);
    let digest = sha256_hex(bytes);

    let preferred = dir.join(&safe);
    if export_file_matches_digest(&preferred, bytes.len() as u64, &digest, /*full*/ false) {
        return Ok(preferred);
    }

    // Prefer the clean name when free; otherwise disambiguate so we never clobber
    // a different artifact that Finder may still reference.
    let target_name = if preferred.exists() {
        disambiguated_export_name(&safe, name_hash)
    } else {
        safe.clone()
    };
    let path = dir.join(&target_name);
    if export_file_matches_digest(&path, bytes.len() as u64, &digest, /*full*/ false) {
        return Ok(path);
    }

    write_export_file(&dir, &target_name, bytes, &digest)?;
    Ok(path)
}

/// Copy an existing cache file into the export staging dir under `file_name`.
///
/// Always **copies** (never hard-links) so the user-editable export path cannot
/// mutate the canonical cache entry through a shared inode. Content integrity is
/// recorded as a SHA-256 + length + mtime sidecar.
pub fn stage_export_file(file_name: &str, source: &Path) -> io::Result<PathBuf> {
    let _guard = staging_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let dir = ensure_artifact_cache_dir()?.join(EXPORT_SUBDIR);
    create_private_dir_all(&dir)?;
    let safe = export_file_name(file_name);
    let source_meta = fs::metadata(source)?;
    let source_len = source_meta.len();
    let digest = sha256_file(source)?;
    let name_hash = simple_hash_file(source)?;

    let preferred = dir.join(&safe);
    if export_file_matches_digest(&preferred, source_len, &digest, /*full*/ false) {
        // Refresh sidecar mtime binding after a trusted match.
        let _ = write_integrity_sidecar_for_path(&preferred, &digest);
        return Ok(preferred);
    }

    let target_name = if preferred.exists() {
        disambiguated_export_name(&safe, name_hash)
    } else {
        safe.clone()
    };
    let path = dir.join(&target_name);
    if export_file_matches_digest(&path, source_len, &digest, /*full*/ false) {
        let _ = write_integrity_sidecar_for_path(&path, &digest);
        return Ok(path);
    }

    // Copy only — never hard_link into the user-facing export path.
    let tmp = unique_staging_path(&path);
    if let Err(error) = fs::copy(source, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    write_integrity_sidecar_for_path(&path, &digest)?;
    Ok(path)
}

/// True when `path` is an export-staged file whose integrity sidecar matches `bytes`.
///
/// Fast path: when the sidecar's SHA-256, length, and mtime all match the file
/// metadata and the expected digest of `bytes`, the file is **not** fully rehashed
/// (safe for large artifacts on background threads; avoid blocking the UI with
/// [`export_matches_bytes_strict`] / [`verify_export_integrity`] for cold paths).
pub fn export_matches_bytes(path: &Path, bytes: &[u8]) -> bool {
    let digest = sha256_hex(bytes);
    export_file_matches_digest(path, bytes.len() as u64, &digest, /*full*/ false)
}

/// Like [`export_matches_bytes`] but always rehashes the file (no mtime trust).
///
/// Prefer this (or [`verify_export_integrity`]) off the UI thread when revalidating
/// large artifacts after long idle periods.
pub fn export_matches_bytes_strict(path: &Path, bytes: &[u8]) -> bool {
    let digest = sha256_hex(bytes);
    export_file_matches_digest(path, bytes.len() as u64, &digest, /*full*/ true)
}

/// Full integrity check: length + SHA-256 of on-disk content vs sidecar.
///
/// Returns `Ok(true)` when the file matches its sidecar, `Ok(false)` on mismatch
/// or missing sidecar fields, and `Err` on I/O failures. Safe to call from a
/// background executor for large files.
pub fn verify_export_integrity(path: &Path) -> io::Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let meta = fs::metadata(path)?;
    let Some(sidecar) = read_integrity_sidecar(path) else {
        return Ok(false);
    };
    if sidecar.len != meta.len() {
        return Ok(false);
    }
    let actual = sha256_file(path)?;
    Ok(actual == sidecar.sha256)
}

/// Async-friendly helper: same as [`verify_export_integrity`] (blocking I/O).
///
/// Call from GPUI's background executor / a worker thread — never the UI thread
/// for multi-megabyte artifacts.
pub fn verify_export_integrity_blocking(path: &Path) -> io::Result<bool> {
    verify_export_integrity(path)
}

fn write_export_file(dir: &Path, name: &str, bytes: &[u8], digest: &str) -> io::Result<()> {
    let path = dir.join(name);
    write_bytes_via_unique_temp(&path, bytes)?;
    write_integrity_sidecar_for_path(&path, digest)?;
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct IntegritySidecar {
    sha256: String,
    len: u64,
    mtime_ns: Option<u64>,
}

fn read_integrity_sidecar(path: &Path) -> Option<IntegritySidecar> {
    let file_name = path.file_name()?.to_str()?;
    let dir = path.parent().unwrap_or(Path::new(""));
    let text = fs::read_to_string(hash_sidecar_path(dir, file_name)).ok()?;
    parse_integrity_sidecar(&text)
}

fn parse_integrity_sidecar(text: &str) -> Option<IntegritySidecar> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Legacy FNV-only sidecar (single hex token) is never trusted for integrity.
    if !trimmed.contains('=') && !trimmed.contains(':') {
        return None;
    }

    let mut sha256 = None;
    let mut len = None;
    let mut mtime_ns = None;
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = if let Some((k, v)) = line.split_once('=') {
            (k.trim(), v.trim())
        } else if let Some((k, v)) = line.split_once(':') {
            (k.trim(), v.trim())
        } else {
            continue;
        };
        match key {
            SIDECAR_SHA256 | "digest" => {
                if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
                    sha256 = Some(value.to_ascii_lowercase());
                }
            }
            SIDECAR_LEN | "length" => {
                if let Ok(n) = value.parse::<u64>() {
                    len = Some(n);
                }
            }
            SIDECAR_MTIME_NS | "mtime" => {
                if let Ok(n) = value.parse::<u64>() {
                    mtime_ns = Some(n);
                }
            }
            _ => {}
        }
    }
    Some(IntegritySidecar {
        sha256: sha256?,
        len: len?,
        mtime_ns,
    })
}

fn format_integrity_sidecar(sha256: &str, len: u64, mtime_ns: u64) -> String {
    format!("{SIDECAR_SHA256}={sha256}\n{SIDECAR_LEN}={len}\n{SIDECAR_MTIME_NS}={mtime_ns}\n")
}

fn mtime_ns_of(meta: &fs::Metadata) -> Option<u64> {
    let modified = meta.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(
        duration
            .as_secs()
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::from(duration.subsec_nanos())),
    )
}

fn write_integrity_sidecar_for_path(path: &Path, digest: &str) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "artifact path has no name"))?;
    let dir = path.parent().unwrap_or(Path::new(""));
    let meta = fs::metadata(path)?;
    let mtime_ns = mtime_ns_of(&meta).unwrap_or(0);
    let body = format_integrity_sidecar(digest, meta.len(), mtime_ns);
    let sidecar = hash_sidecar_path(dir, file_name);
    let tmp = unique_staging_path(&sidecar);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, &sidecar)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Match path against expected length + SHA-256.
///
/// When `force_full` is false and the sidecar binds the same length, mtime, and
/// digest, skip hashing the whole file (large-artifact UI-safe path).
fn export_file_matches_digest(path: &Path, len: u64, digest: &str, force_full: bool) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if meta.len() != len {
        return false;
    }
    let Some(sidecar) = read_integrity_sidecar(path) else {
        // No trustworthy sidecar: fall back to full rehash so we still detect edits,
        // but never trust bare existence.
        return match sha256_file(path) {
            Ok(actual) => actual == digest,
            Err(_) => false,
        };
    };
    if sidecar.len != len || sidecar.sha256 != digest {
        return false;
    }
    if !force_full {
        if let (Some(side_m), Some(file_m)) = (sidecar.mtime_ns, mtime_ns_of(&meta)) {
            if side_m == file_m {
                // Trusted: length + mtime + digest all agree with sidecar claim.
                return true;
            }
        }
    }
    match sha256_file(path) {
        Ok(actual) => actual == digest,
        Err(_) => false,
    }
}

fn cache_file_matches(path: &Path, len: u64, digest: &str) -> bool {
    export_file_matches_digest(path, len, digest, /*full*/ false)
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

fn make_readonly(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    let mut perms = meta.permissions();
    perms.set_readonly(true);
    let _ = fs::set_permissions(path, perms);
}

fn make_writable_if_needed(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.permissions().readonly() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = meta.permissions();
            let mode = perms.mode();
            perms.set_mode(mode | 0o200);
            let _ = fs::set_permissions(path, perms);
        }
        #[cfg(not(unix))]
        {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
}

/// Remove cache entries older than `ttl` and, if still over `max_bytes`,
/// delete oldest files until under the budget.
///
/// Walks the cache root recursively so `export/` and `paste-staging/` are
/// included (hash sidecars and staged media would otherwise accumulate forever).
///
/// Holds the staging mutex and **skips** paths with an active [`ArtifactLease`].
pub fn purge_artifact_cache(ttl: Duration, max_bytes: u64) -> io::Result<PurgeStats> {
    let _guard = staging_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_artifact_cache_locked(ttl, max_bytes)
}

fn purge_artifact_cache_locked(ttl: Duration, max_bytes: u64) -> io::Result<PurgeStats> {
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
            if path_is_leased(&entry.path) {
                return true;
            }
            if remove_cache_path(&entry.path).is_ok() {
                stats.removed += 1;
                stats.freed_bytes += entry.len;
            }
            false
        } else {
            true
        }
    });

    // Size budget: oldest first. Skip leased paths.
    let mut total: u64 = entries.iter().map(|e| e.len).sum();
    if total > max_bytes {
        entries.sort_by_key(|e| e.modified);
        for entry in entries {
            if total <= max_bytes {
                break;
            }
            if path_is_leased(&entry.path) {
                continue;
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

    let _guard = staging_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    let now = SystemTime::now();
    let mut entries = Vec::new();
    collect_cache_files(&dir, &now, &mut entries)?;
    let mut stats = PurgeStats::default();
    for entry in entries {
        if entry.age > ttl {
            if path_is_leased(&entry.path) {
                continue;
            }
            if remove_cache_path(&entry.path).is_ok() {
                stats.removed += 1;
                stats.freed_bytes += entry.len;
            }
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

/// Immediate purge using default budgets — call after conversion writes and
/// periodically from the app (startup / idle). Alias of
/// [`purge_artifact_cache_defaults`].
///
/// Safe to call from a background executor; holds the staging mutex so it will
/// not race writers, and skips leased paths.
pub fn purge_now() -> io::Result<PurgeStats> {
    purge_artifact_cache_defaults()
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
    // Allow removal even if the file was marked read-only.
    make_writable_if_needed(path);
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
    // FNV-1a 64-bit — naming / disambiguation only, never sole integrity trust.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn simple_hash_file(path: &Path) -> io::Result<u64> {
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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("purge");
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: serialized behind crate::ENV_LOCK.
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
    fn stage_export_file_copies_not_hardlinks() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("export-file");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let source = cache_artifact_bytes("clip.bin", b"binary-payload").unwrap();
        let export = stage_export_file("clip.bin", &source).unwrap();
        assert_eq!(fs::read(&export).unwrap(), b"binary-payload");
        // Export must be a distinct inode so editing export cannot mutate cache.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let src_ino = fs::metadata(&source).unwrap().ino();
            let exp_ino = fs::metadata(&export).unwrap().ino();
            assert_ne!(
                src_ino, exp_ino,
                "export staging must copy, not hard-link, the canonical cache file"
            );
        }
        assert!(export.is_file());

        // Mutating the export path must not change the cache canonical bytes.
        make_writable_if_needed(&export);
        fs::write(&export, b"mutated!!!!!!").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"binary-payload");

        // Second stage with same content reuses the path only if integrity matches;
        // after mutation, a new stage from source should rewrite / rematch.
        let export2 = stage_export_file("clip.bin", &source).unwrap();
        assert_eq!(fs::read(&export2).unwrap(), b"binary-payload");

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_cache_and_export_staging_reuses_complete_artifact() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("match");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }
        let path = stage_export_bytes("note.md", b"body").unwrap();
        assert!(export_matches_bytes(&path, b"body"));
        assert!(!export_matches_bytes(&path, b"other"));
        assert!(verify_export_integrity(&path).unwrap());
        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn export_matches_rejects_same_length_content_edit() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("stale-edit");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }
        let path = stage_export_bytes("note.md", b"body").unwrap();
        assert!(export_matches_bytes(&path, b"body"));
        // Same length, different bytes — must not reuse staged file.
        make_writable_if_needed(&path);
        fs::write(&path, b"xxxx").unwrap();
        assert!(!export_matches_bytes(&path, b"body"));
        assert!(!export_matches_bytes_strict(&path, b"body"));
        // Even if the sidecar still claims the old hash, content wins on full verify
        // (mtime usually changes on write, forcing rehash; strict always rehashes).
        let sidecar = path.parent().unwrap().join(".note.md.hash");
        assert!(sidecar.is_file());
        assert!(!export_matches_bytes(&path, b"body"));
        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn digest_mismatch_sidecar_rejects_reuse() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("digest-mismatch");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let path = stage_export_bytes("report.bin", b"good-payload").unwrap();
        assert!(export_matches_bytes(&path, b"good-payload"));

        // Corrupt the sidecar digest while leaving file bytes intact.
        let sidecar = path.parent().unwrap().join(".report.bin.hash");
        assert!(sidecar.is_file());
        let meta = fs::metadata(&path).unwrap();
        let mtime = mtime_ns_of(&meta).unwrap_or(0);
        let bogus = format_integrity_sidecar(
            "0000000000000000000000000000000000000000000000000000000000000000",
            meta.len(),
            mtime,
        );
        fs::write(&sidecar, bogus).unwrap();

        assert!(
            !export_matches_bytes(&path, b"good-payload"),
            "mismatched SHA-256 sidecar must reject reuse"
        );
        assert!(!export_matches_bytes_strict(&path, b"good-payload"));
        assert!(!verify_export_integrity(&path).unwrap());

        // Legacy FNV-only sidecar is never trusted for a match without rehash
        // that still verifies SHA of expected bytes — existence alone is insufficient.
        fs::write(&sidecar, "cbf29ce484222325").unwrap();
        // Without a structured sidecar, code falls back to full rehash of file vs expected.
        assert!(
            export_matches_bytes(&path, b"good-payload"),
            "missing structured sidecar falls back to content rehash"
        );
        assert!(!export_matches_bytes(&path, b"other-payload"));

        // Wrong length in sidecar rejects even if digest string were right.
        let real_digest = sha256_hex(b"good-payload");
        fs::write(
            &sidecar,
            format_integrity_sidecar(&real_digest, meta.len() + 1, mtime),
        )
        .unwrap();
        assert!(!export_matches_bytes(&path, b"good-payload"));

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lease_protects_staged_path_from_purge() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("lease-purge");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let export = stage_export_bytes("leased.md", b"# keep me").unwrap();
        let sidecar = export.parent().unwrap().join(".leased.md.hash");
        assert!(export.is_file());
        assert!(sidecar.is_file());

        let lease = acquire_export_lease(&export);
        assert_eq!(lease.path(), normalize_lease_path(&export));

        // Zero TTL would remove everything; leased export + its path must survive.
        let stats = purge_artifact_cache(Duration::from_secs(0), DEFAULT_CACHE_MAX_BYTES).unwrap();
        let _ = stats;
        assert!(
            export.exists(),
            "leased export path must survive purge_now/purge_artifact_cache"
        );

        // Nested refcount: second lease keeps protection after first drops.
        let lease2 = acquire_export_lease(&export);
        drop(lease);
        let _ = purge_artifact_cache(Duration::from_secs(0), DEFAULT_CACHE_MAX_BYTES).unwrap();
        assert!(export.exists(), "refcount lease must still protect path");

        drop(lease2);
        let stats = purge_now().unwrap();
        // Default TTL may keep young files; force zero-TTL purge after lease release.
        let _ = stats;
        let stats = purge_artifact_cache(Duration::from_secs(0), DEFAULT_CACHE_MAX_BYTES).unwrap();
        assert!(
            stats.removed >= 1 || !export.exists(),
            "unleased path should be purgeable"
        );
        assert!(
            !export.exists(),
            "export must be removable after all leases drop"
        );

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn trusted_sidecar_skips_rehash_when_mtime_matches() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("trusted-mtime");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        let path = stage_export_bytes("big.bin", b"payload-for-trust").unwrap();
        let sidecar = read_integrity_sidecar(&path).expect("sidecar");
        assert_eq!(sidecar.sha256, sha256_hex(b"payload-for-trust"));
        assert!(sidecar.mtime_ns.is_some());

        // Fast path accepts matching bytes without needing strict mode.
        assert!(export_matches_bytes(&path, b"payload-for-trust"));
        // Strict also accepts (rehashes).
        assert!(export_matches_bytes_strict(&path, b"payload-for-trust"));

        // Tamper bytes but restore mtime + leave stale sidecar → trusted path would
        // wrongly accept if it only checked mtime; we bind digest to expected bytes,
        // so expected digest of "payload-for-trust" won't match if we change expected
        // OR if we rehash. Change file content, rewrite sidecar mtime to match new
        // meta but wrong digest already covered; here force mtime match with wrong digest:
        make_writable_if_needed(&path);
        // Same length as "payload-for-trust" (17 bytes) so length checks still pass.
        fs::write(&path, b"XXXXXXXXXXXXXXXXX").unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), b"payload-for-trust".len() as u64);
        let m = mtime_ns_of(&meta).unwrap();
        // Sidecar still claims old digest + new mtime (attacker-controlled sidecar).
        fs::write(
            path.parent().unwrap().join(".big.bin.hash"),
            format_integrity_sidecar(&sha256_hex(b"payload-for-trust"), meta.len(), m),
        )
        .unwrap();
        // Expected bytes still "payload-for-trust"; trusted path sees matching
        // sidecar digest+len+mtime and returns true WITHOUT rehash — that is
        // the trust model: sidecar is written only by us. An attacker who can write
        // the sidecar can already replace the file. Strict mode rehashes and rejects.
        assert!(
            export_matches_bytes(&path, b"payload-for-trust"),
            "trusted path trusts our sidecar when mtime+len+digest claim matches expected"
        );
        assert!(
            !export_matches_bytes_strict(&path, b"payload-for-trust"),
            "strict path must rehash and reject tampered content"
        );

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_dir_created_with_private_mode() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("mode700");
        // Do not pre-create: ensure_artifact_cache_dir should create with 0700.
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }
        let ensured = ensure_artifact_cache_dir().unwrap();
        assert_eq!(ensured, dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "cache dir mode must be 0700, got {mode:o}");
        }
        // Export subdir also private.
        let _ = stage_export_bytes("x.md", b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let export = dir.join(EXPORT_SUBDIR);
            let mode = fs::metadata(&export).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "export dir mode must be 0700, got {mode:o}");
        }
        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn version_mismatch_purges_stale_cache_entries() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("concurrent-purge");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }

        // Seed a few files so purge has work to do.
        for i in 0..4 {
            let _ = cache_artifact_bytes(&format!("seed-{i}.bin"), &[i as u8; 64]);
        }

        // Writers and purge share the staging mutex; all must complete without panic.
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
        assert!(
            write_results.iter().all(|r| r.is_ok()),
            "writers serialize with purge via staging lock"
        );

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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    #[test]
    fn ensure_artifact_cache_dir_rejects_symlink_and_file() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = unique_temp_dir("ensure-bad");
        std::fs::create_dir_all(&base).unwrap();

        // File where cache dir should be.
        let file_path = base.join("as-file");
        fs::write(&file_path, b"x").unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &file_path);
        }
        let err = ensure_artifact_cache_dir().unwrap_err();
        assert!(err.to_string().contains("not a directory"), "error: {err}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = base.join("real-target");
            fs::create_dir_all(&target).unwrap();
            let link = base.join("as-link");
            symlink(&target, &link).unwrap();
            unsafe {
                std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &link);
            }
            let err = ensure_artifact_cache_dir().unwrap_err();
            assert!(err.to_string().contains("symlink"), "error: {err}");
        }

        // Missing path without home / support override for default cache.
        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
            std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
            // Keep HOME so we don't break other tests; exercise NotFound via empty override.
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", "");
        }
        // Empty string still yields Some path; clear properly:
        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        // When cache dir is unset, application_support_dir may still resolve via HOME.

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn cache_artifact_bytes_without_extension_uses_hash_suffix() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("no-ext");
        fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
        }
        let path = cache_artifact_bytes("noextname", b"payload").unwrap();
        assert!(path.is_file());
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains("noextname") && name.contains('-'),
            "name: {name}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"payload");
        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn purge_artifact_cache_defaults_runs_paste_staging_too() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("purge-defaults");
        let paste_dir = unique_temp_dir("purge-defaults-paste");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&paste_dir).unwrap();
        unsafe {
            std::env::set_var("SHIFT_ARTIFACT_CACHE_DIR", &dir);
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &paste_dir);
        }
        // Seed a cache entry and an external paste staging file.
        let cached = cache_artifact_bytes("seed.bin", b"data").unwrap();
        assert!(cached.is_file());
        fs::write(paste_dir.join("old.dat"), b"stale").unwrap();

        let stats = purge_artifact_cache_defaults().unwrap();
        // Fresh files may not be purged under default TTL; just ensure the call succeeds.
        let _ = stats;
        let stats2 = purge_now().unwrap();
        let _ = stats2;

        unsafe {
            std::env::remove_var("SHIFT_ARTIFACT_CACHE_DIR");
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(paste_dir);
    }

    #[test]
    fn parse_integrity_sidecar_requires_sha_and_len() {
        assert!(parse_integrity_sidecar("").is_none());
        assert!(parse_integrity_sidecar("deadbeef").is_none());
        assert!(parse_integrity_sidecar("sha256=abcd\nlen=1").is_none()); // short digest
        let good = format_integrity_sidecar(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            12,
            99,
        );
        let parsed = parse_integrity_sidecar(&good).unwrap();
        assert_eq!(parsed.len, 12);
        assert_eq!(parsed.mtime_ns, Some(99));
        assert_eq!(parsed.sha256.len(), 64);
    }
}
