//! Bounded external process execution for conversion modules.
//!
//! Every converter should run through this helper so timeouts and output size
//! caps are applied uniformly. Callers still own argument construction and
//! error messaging for their engine.

use super::ConversionError;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default wall-clock budget for one converter invocation.
pub const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(300);

/// Default ceiling for captured stdout, stderr, or on-disk converter output.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Max time to wait for stdout/stderr reader threads after the child is killed.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Portable basename length budget for temp names (POSIX `NAME_MAX` is commonly 255).
pub const FS_NAME_MAX: usize = 255;

/// Captured process output (same shape as `std::process::Output`).
#[derive(Debug)]
pub struct LimitedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `command` with a wall-clock deadline and capped stdout/stderr capture.
///
/// On timeout the process (and its process group on Unix) is killed. If either
/// stream exceeds `max_output_bytes`, the process is killed and an error is
/// returned.
pub fn run_command(
    command: Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<LimitedOutput, ConversionError> {
    run_command_cancellable(command, timeout, max_output_bytes, None)
}

/// Like [`run_command`], but also aborts when `cancel` becomes true.
pub fn run_command_cancellable(
    command: Command,
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<LimitedOutput, ConversionError> {
    run_command_cancellable_with_output_paths(command, timeout, max_output_bytes, cancel, &[])
}

/// Like [`run_command_cancellable`], also polling on-disk converter outputs.
///
/// While the child runs, each path in `watch_output_paths` is `stat`ed; if any
/// file grows past `max_output_bytes` the process group is killed and an error
/// is returned. Use this for engines that write artifacts to temp files rather
/// than (or in addition to) stdout.
pub fn run_command_cancellable_with_output_paths(
    command: Command,
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<Arc<AtomicBool>>,
    watch_output_paths: &[PathBuf],
) -> Result<LimitedOutput, ConversionError> {
    let path_limits: Vec<(PathBuf, u64)> = watch_output_paths
        .iter()
        .cloned()
        .map(|path| (path, max_output_bytes as u64))
        .collect();
    run_command_cancellable_with_output_limits(
        command,
        timeout,
        max_output_bytes,
        cancel,
        &path_limits,
        &[],
    )
}

/// Like [`run_command_cancellable_with_output_paths`], also watches the total
/// size of files below each directory. Directory budgets are independent from
/// stdout/stderr and file-output budgets, which lets callers enforce a larger
/// temporary-workspace cap without weakening captured-process-output limits.
pub fn run_command_cancellable_with_output_dirs(
    command: Command,
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<Arc<AtomicBool>>,
    watch_output_paths: &[PathBuf],
    watch_output_dirs: &[(PathBuf, u64)],
) -> Result<LimitedOutput, ConversionError> {
    let path_limits: Vec<(PathBuf, u64)> = watch_output_paths
        .iter()
        .cloned()
        .map(|path| (path, max_output_bytes as u64))
        .collect();
    run_command_cancellable_with_output_limits(
        command,
        timeout,
        max_output_bytes,
        cancel,
        &path_limits,
        watch_output_dirs,
    )
}

/// Run a command while applying independent byte ceilings to captured output,
/// watched files, and watched temporary directories.
pub fn run_command_cancellable_with_output_limits(
    mut command: Command,
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<Arc<AtomicBool>>,
    watch_output_paths: &[(PathBuf, u64)],
    watch_output_dirs: &[(PathBuf, u64)],
) -> Result<LimitedOutput, ConversionError> {
    if cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        return Err(ConversionError::cancelled());
    }

    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Put the child in its own process group so converters that spawn helpers
    // (shell wrappers, Python, Node) can be torn down together on timeout.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            // Distinctive prefix so modules can substitute install hints.
            ConversionError::new(format!("executable not found: {error}"))
        } else {
            ConversionError::new(format!("could not start process: {error}"))
        }
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ConversionError::new("converter stdout pipe was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ConversionError::new("converter stderr pipe was unavailable"))?;

    let max = max_output_bytes;
    let pid = child.id();
    let stdout_thread = thread::spawn(move || read_process_stream(stdout, max, pid));
    let stderr_thread = thread::spawn(move || read_process_stream(stderr, max, pid));

    let status = match wait_with_timeout(
        &mut child,
        timeout,
        cancel.clone(),
        watch_output_paths,
        watch_output_dirs,
    ) {
        WaitOutcome::Exited(status) => status,
        WaitOutcome::TimedOut => {
            // Process group already signalled/reaped; bound reader drain so a
            // wedged descendant holding a pipe cannot hang the caller forever.
            let _ = join_reader_timeout(stdout_thread, READER_JOIN_TIMEOUT);
            let _ = join_reader_timeout(stderr_thread, READER_JOIN_TIMEOUT);
            return Err(ConversionError::new(format!(
                "conversion timed out after {}s",
                timeout.as_secs().max(1)
            )));
        }
        WaitOutcome::Cancelled => {
            let _ = join_reader_timeout(stdout_thread, READER_JOIN_TIMEOUT);
            let _ = join_reader_timeout(stderr_thread, READER_JOIN_TIMEOUT);
            return Err(ConversionError::cancelled());
        }
        WaitOutcome::OutputTooLarge { path, size, limit } => {
            let _ = join_reader_timeout(stdout_thread, READER_JOIN_TIMEOUT);
            let _ = join_reader_timeout(stderr_thread, READER_JOIN_TIMEOUT);
            return Err(ConversionError::new(format!(
                "converter output {} is too large ({} bytes; limit is {} bytes)",
                path.display(),
                size,
                limit
            )));
        }
        WaitOutcome::Error(error) => {
            force_kill(&mut child);
            let _ = join_reader_timeout(stdout_thread, READER_JOIN_TIMEOUT);
            let _ = join_reader_timeout(stderr_thread, READER_JOIN_TIMEOUT);
            return Err(ConversionError::new(format!(
                "could not wait for converter: {error}"
            )));
        }
    };

    if cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        let _ = join_reader_timeout(stdout_thread, READER_JOIN_TIMEOUT);
        let _ = join_reader_timeout(stderr_thread, READER_JOIN_TIMEOUT);
        return Err(ConversionError::cancelled());
    }

    let stdout = join_reader(stdout_thread, "stdout", max_output_bytes)?;
    let stderr = join_reader(stderr_thread, "stderr", max_output_bytes)?;

    Ok(LimitedOutput {
        status,
        stdout,
        stderr,
    })
}

enum WaitOutcome {
    Exited(std::process::ExitStatus),
    TimedOut,
    Cancelled,
    OutputTooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
    Error(std::io::Error),
}

/// Poll until the child exits, `timeout` elapses, cancel fires, or a watched
/// on-disk output exceeds `max_output_bytes`.
///
/// Uses `try_wait` + `child.kill()` so cancel and timeout work on all platforms
/// (Unix also tears down the process group via [`kill_pid`]).
fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
    watch_output_paths: &[(PathBuf, u64)],
    watch_output_dirs: &[(PathBuf, u64)],
) -> WaitOutcome {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A fast converter can create its entire output and exit before
                // the next polling iteration. Check once more after reaping so
                // on-disk limits are hard ceilings, not best-effort checks only
                // for long-running children.
                if let Some(over) = check_watched_output_size(watch_output_paths, watch_output_dirs)
                {
                    return WaitOutcome::OutputTooLarge {
                        path: over.0,
                        size: over.1,
                        limit: over.2,
                    };
                }
                return WaitOutcome::Exited(status);
            }
            Ok(None) => {}
            Err(error) => return WaitOutcome::Error(error),
        }

        if let Some(over) = check_watched_output_size(watch_output_paths, watch_output_dirs) {
            force_kill(child);
            return WaitOutcome::OutputTooLarge {
                path: over.0,
                size: over.1,
                limit: over.2,
            };
        }

        let elapsed = start.elapsed();
        if elapsed >= timeout {
            force_kill(child);
            return WaitOutcome::TimedOut;
        }
        if cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            force_kill(child);
            return WaitOutcome::Cancelled;
        }

        // Sleep only as long as is left of the timeout so cancel/timeout fire
        // promptly instead of overshooting by up to 50 ms.
        thread::sleep(Duration::from_millis(50).min(timeout - elapsed));
    }
}

fn check_watched_output_size(
    paths: &[(PathBuf, u64)],
    dirs: &[(PathBuf, u64)],
) -> Option<(PathBuf, u64, u64)> {
    for (path, limit) in paths {
        if let Ok(metadata) = std::fs::metadata(path) {
            let len = metadata.len();
            if len > *limit {
                return Some((path.clone(), len, *limit));
            }
        }
    }
    for (dir, limit) in dirs {
        let mut pending = vec![dir.clone()];
        let mut total = 0u64;
        while let Some(current) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    pending.push(entry.path());
                } else if file_type.is_file()
                    && let Ok(metadata) = entry.metadata()
                {
                    total = total.saturating_add(metadata.len());
                    if total > *limit {
                        return Some((dir.clone(), total, *limit));
                    }
                }
            }
        }
    }
    None
}

fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        // Negative PID kills the whole process group (set up via process_group(0)).
        // Use absolute system `kill` first because GUI apps often launch with a
        // minimal PATH that does not include /bin or /usr/bin.
        let group_arg = format!("-{pid}");
        let pid_arg = pid.to_string();
        for binary in ["/bin/kill", "/usr/bin/kill"] {
            if Path::new(binary).is_file() {
                let _ = Command::new(binary)
                    .args(["-KILL", &group_arg])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                // Also target the process itself in case process-group setup failed.
                let _ = Command::new(binary)
                    .args(["-KILL", &pid_arg])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Windows/other: process-group kill is unavailable; [`force_kill`] uses
        // `Child::kill` on the same thread that owns the child handle.
        let _ = pid;
    }
}

fn force_kill(child: &mut Child) {
    kill_pid(child.id());
    let _ = child.kill();
    // Drop stdio handles aggressively so reader threads observe EOF even if a
    // descendant briefly holds the write end (Child drops pipes on wait).
    let _ = child.stdout.take();
    let _ = child.stderr.take();
    let _ = child.stdin.take();
    // Reap so the next try_wait/wait does not race a zombie.
    let _ = child.wait();
}

/// Create a new file with mode `0600` **before** writing secrets.
///
/// On Unix the open uses `OpenOptionsExt::mode(0o600)` so the content is never
/// briefly world-readable under a default umask. Callers should place these
/// files under a private temp directory ([`unique_temp_dir`]). Exclusive
/// creation also prevents a symlink at `path` from redirecting the secret.
pub fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), ConversionError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        ConversionError::new(format!(
            "could not create private file {}: {error}",
            path.display()
        ))
    })?;
    file.write_all(bytes).map_err(|error| {
        ConversionError::new(format!(
            "could not write private file {}: {error}",
            path.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        ConversionError::new(format!(
            "could not sync private file {}: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

/// Create a new regular file with mode `0600` for sensitive intermediate
/// converter output. The exclusive open prevents a symlink at `path` from
/// redirecting the output.
pub fn create_private_file(path: &Path) -> Result<std::fs::File, ConversionError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| {
        ConversionError::new(format!(
            "could not create private file {}: {error}",
            path.display()
        ))
    })
}

/// Absolute path suitable for child argv (prefers canonicalize when the path exists).
pub fn absolute_command_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// True when the path string itself would be parsed as a CLI option (`-…`).
///
/// Absolute paths like `/tmp/-evil.pdf` are safe (they start with `/`). Relative
/// names such as `-rf` or `--help` are not — absolutize before passing them as
/// bare argv operands (see [`push_operand_path`]).
pub fn path_looks_like_option(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().first().copied() == Some(b'-')
}

/// Reject empty paths and operands that would be parsed as CLI flags.
///
/// Prefer [`push_operand_path`], which absolutizes first so relative names like
/// `-notes.md` become `/cwd/-notes.md` and are accepted.
pub fn validate_path_operand(path: &Path) -> Result<(), ConversionError> {
    if path.as_os_str().is_empty() {
        return Err(ConversionError::new("path operand is empty"));
    }
    if path_looks_like_option(path) {
        return Err(ConversionError::new(format!(
            "refusing path operand that looks like a CLI option: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Append an absolute path after a flag that consumes a path value (`-i`, `--out`, …).
///
/// Absolute paths prevent option-injection for relative names starting with `-`.
pub fn push_flag_path(command: &mut Command, flag: impl AsRef<OsStr>, path: &Path) -> PathBuf {
    let absolute = absolute_command_path(path);
    command.arg(flag).arg(&absolute);
    absolute
}

/// Append an absolute path as a positional operand.
///
/// Paths are absolutized so they never begin with `-` on Unix (absolute paths
/// start with `/`), which closes option-injection for relative names like
/// `-rf`. We intentionally do **not** insert a bare `--` separator: several
/// converters and test fakes (including BSD `cat`) reject GNU-style `--`.
///
/// Relative operands whose basename starts with `-` are still rejected when
/// absolutization cannot produce a safe form (empty / non-absolute edge cases).
pub fn push_operand_path(command: &mut Command, path: &Path) -> Result<PathBuf, ConversionError> {
    if path.as_os_str().is_empty() {
        return Err(ConversionError::new("path operand is empty"));
    }
    let absolute = absolute_command_path(path);
    // Absolute Unix paths start with `/` and cannot be mistaken for flags.
    // If absolutization failed to produce a non-option-like path, refuse.
    if path_looks_like_option(&absolute) {
        return Err(ConversionError::new(format!(
            "refusing path operand that looks like a CLI option: {}",
            path.display()
        )));
    }
    command.arg(&absolute);
    Ok(absolute)
}

/// Append an absolute path as a bare positional arg, rejecting option-like names.
pub fn push_path_arg(command: &mut Command, path: &Path) -> Result<PathBuf, ConversionError> {
    push_operand_path(command, path)
}

/// Read a converter-produced file with a hard size ceiling.
pub fn read_file_limited(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ConversionError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        ConversionError::new(format!("could not read {}: {error}", path.display()))
    })?;
    if metadata.len() > max_bytes as u64 {
        return Err(ConversionError::new(format!(
            "converter output {} is too large ({} bytes; limit is {} bytes)",
            path.display(),
            metadata.len(),
            max_bytes
        )));
    }

    let file = std::fs::File::open(path).map_err(|error| {
        ConversionError::new(format!("could not read {}: {error}", path.display()))
    })?;
    let result = read_limited(file, max_bytes).map_err(|error| {
        ConversionError::new(format!("could not read {}: {error}", path.display()))
    })?;
    if result.truncated {
        return Err(ConversionError::new(format!(
            "converter output {} exceeded the {} byte limit",
            path.display(),
            max_bytes
        )));
    }
    Ok(result.bytes)
}

fn join_reader(
    handle: thread::JoinHandle<std::io::Result<ReadResult>>,
    stream: &str,
    max_output_bytes: usize,
) -> Result<Vec<u8>, ConversionError> {
    // Normal exit: readers should drain quickly once the child closed the pipe.
    // Still bound the join so a wedged reader cannot hang the conversion thread.
    let joined = join_reader_timeout(handle, READER_JOIN_TIMEOUT).map_err(|_| {
        ConversionError::new(format!(
            "converter {stream} reader did not finish within {}s after process exit",
            READER_JOIN_TIMEOUT.as_secs().max(1)
        ))
    })?;
    let result = match joined {
        Ok(io_result) => io_result,
        Err(_) => {
            return Err(ConversionError::new(format!(
                "converter {stream} reader panicked"
            )));
        }
    };
    let result = result.map_err(|error| {
        ConversionError::new(format!("could not read converter {stream}: {error}"))
    })?;
    if result.truncated {
        return Err(ConversionError::new(format!(
            "converter {stream} exceeded the {max_output_bytes} byte limit"
        )));
    }
    Ok(result.bytes)
}

/// Join a reader thread, abandoning the join after `timeout` so cancel/timeout
/// paths cannot block forever on a wedged pipe reader.
fn join_reader_timeout<T: Send + 'static>(
    handle: thread::JoinHandle<T>,
    timeout: Duration,
) -> Result<std::thread::Result<T>, ()> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(handle.join());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => Ok(result),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(()),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
    }
}

struct ReadResult {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_process_stream(
    reader: impl Read,
    max_bytes: usize,
    pid: u32,
) -> std::io::Result<ReadResult> {
    let result = read_limited(reader, max_bytes)?;
    if result.truncated {
        // Stop the converter immediately instead of continuing to drain an
        // unbounded stream until the wall-clock timeout expires.
        kill_pid(pid);
    }
    Ok(result)
}

fn read_limited(mut reader: impl Read, max_bytes: usize) -> std::io::Result<ReadResult> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(ReadResult {
                bytes,
                truncated: false,
            });
        }
        if bytes.len().saturating_add(read) > max_bytes {
            let remaining = max_bytes.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..remaining]);
            return Ok(ReadResult {
                bytes,
                truncated: true,
            });
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

/// Effective timeout, overridable via `SHIFT_CONVERSION_TIMEOUT_SECS`.
pub fn process_timeout() -> Duration {
    std::env::var("SHIFT_CONVERSION_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PROCESS_TIMEOUT)
}

/// Effective output ceiling, overridable via `SHIFT_CONVERSION_MAX_OUTPUT_BYTES`.
pub fn max_output_bytes() -> usize {
    std::env::var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
}

/// Whether `path` exists and looks executable (Unix execute bit).
pub fn is_runnable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Common absolute bin directories probed when `PATH` is minimal (GUI apps on macOS).
///
/// Includes Homebrew, TeX, cargo, and version-manager layouts that ship tools
/// outside the default GUI `PATH` (nvm, fnm, volta, asdf, mise).
pub fn common_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/Library/TeX/texbin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".volta/bin"));
        dirs.push(home.join(".asdf/shims"));
        dirs.push(home.join(".local/share/mise/shims"));
        dirs.push(home.join(".mise/shims"));

        // nvm: ~/.nvm/versions/node/<ver>/bin (newest first)
        let nvm_root = std::env::var_os("NVM_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".nvm"));
        append_versioned_bin_dirs(&mut dirs, &nvm_root.join("versions/node"), "bin");

        // fnm: <root>/node-versions/<ver>/installation/bin
        let mut fnm_roots = Vec::new();
        if let Some(dir) = std::env::var_os("FNM_DIR") {
            fnm_roots.push(PathBuf::from(dir));
        }
        fnm_roots.push(home.join(".local/share/fnm"));
        fnm_roots.push(home.join(".fnm"));
        for root in fnm_roots {
            append_versioned_bin_dirs(&mut dirs, &root.join("node-versions"), "installation/bin");
        }
    }
    dirs
}

/// Append `versions_root/<entry>/<relative_bin>` for each version directory,
/// newest-looking names first (lexicographic reverse works for `v22.23.1`).
fn append_versioned_bin_dirs(dirs: &mut Vec<PathBuf>, versions_root: &Path, relative_bin: &str) {
    let Ok(entries) = std::fs::read_dir(versions_root) else {
        return;
    };
    let mut versions: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    versions.sort_by(|left, right| {
        let left_name = left
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let right_name = right
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        // Reverse so higher version strings are tried first.
        version_label_cmp(right_name, left_name)
    });
    for version_dir in versions {
        dirs.push(version_dir.join(relative_bin));
    }
}

/// Compare version-ish directory labels (`v22.23.1`, `22.23.1`) for ordering.
fn version_label_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_parts = version_label_parts(left);
    let right_parts = version_label_parts(right);
    left_parts.cmp(&right_parts)
}

fn version_label_parts(label: &str) -> Vec<u64> {
    label
        .trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

/// Resolve a bare tool name on `PATH` and common install locations.
///
/// Absolute paths that are runnable are returned as-is. Relative names are
/// searched on `PATH`, then in [`common_bin_dirs`].
///
/// Results are memoized process-wide (keyed by `name`): converter discovery is
/// stable for the life of the process, so the PATH / common-dir scan runs once
/// per tool instead of on every `ConversionRegistry::default()`.
pub fn find_executable(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let name = name.as_ref();
    let cache = find_executable_cache();
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(name)
    {
        return cached.clone();
    }
    let resolved = find_executable_uncached(name);
    cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(name.to_owned(), resolved.clone());
    resolved
}

fn find_executable_cache() -> &'static Mutex<HashMap<OsString, Option<PathBuf>>> {
    static CACHE: OnceLock<Mutex<HashMap<OsString, Option<PathBuf>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn find_executable_uncached(name: &OsStr) -> Option<PathBuf> {
    let as_path = Path::new(name);
    if as_path.is_absolute() || as_path.components().count() > 1 {
        return is_runnable(as_path).then(|| as_path.to_path_buf());
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            // Empty PATH components conventionally mean the current directory,
            // which would let a bare tool name resolve to a file in cwd.
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join(name);
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
    }

    for dir in common_bin_dirs() {
        let candidate = dir.join(name);
        if is_runnable(&candidate) {
            return Some(candidate);
        }
    }

    None
}

/// Resolve a conversion tool the same way diagnostics and modules do.
///
/// Order of preference:
/// 1. `env_override` when set (absolute path, existing path, or bare name on PATH)
/// 2. Project-local candidates that are runnable
/// 3. [`find_executable`] for `default_name` (PATH + [`common_bin_dirs`])
///
/// Returns `None` only when nothing is configured and the default name cannot
/// be resolved. When an env override is set to a broken path, that path is
/// still returned so callers can surface it as missing/failed.
pub fn resolve_tool_path(
    env_override: &str,
    default_name: &str,
    local_candidates: &[PathBuf],
) -> Option<PathBuf> {
    let key = ResolveKey::capture(env_override, default_name, local_candidates);
    let cache = resolve_path_cache();
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
    {
        return cached.clone();
    }
    let resolved = resolve_tool_path_uncached(env_override, default_name, local_candidates);
    cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key, resolved.clone());
    resolved
}

/// Cache key for [`resolve_tool_path`].
///
/// Includes the env override name, default name, and local candidates as
/// required, plus the env override's current value so a changed override does
/// not read a stale result. In production these are all stable, so each tool is
/// resolved once per process.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ResolveKey {
    env_override: String,
    env_value: Option<OsString>,
    default_name: String,
    local_candidates: Vec<PathBuf>,
}

impl ResolveKey {
    fn capture(env_override: &str, default_name: &str, local_candidates: &[PathBuf]) -> Self {
        Self {
            env_override: env_override.to_owned(),
            env_value: std::env::var_os(env_override),
            default_name: default_name.to_owned(),
            local_candidates: local_candidates.to_vec(),
        }
    }
}

fn resolve_path_cache() -> &'static Mutex<HashMap<ResolveKey, Option<PathBuf>>> {
    static CACHE: OnceLock<Mutex<HashMap<ResolveKey, Option<PathBuf>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Atomic counter used to build temporary directory names.
///
/// Combined with the process id, a high-resolution timestamp nonce, and
/// [`std::fs::create_dir`] in [`unique_temp_dir`], this helps avoid collisions
/// across parallel workers and reused process ids. It does not by itself
/// guarantee uniqueness; the create-and-retry loop provides that property.
static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a unique temporary directory for one conversion step.
///
/// The name includes `prefix`, the process id, a monotonically increasing
/// counter, and a high-resolution timestamp nonce. The directory is created
/// atomically with [`std::fs::create_dir`]; if the name already exists, a new
/// nonce is generated and the call retried.
pub fn unique_temp_dir(prefix: &str) -> Result<PathBuf, ConversionError> {
    unique_temp_dir_in(&std::env::temp_dir(), prefix)
}

fn unique_temp_dir_in(base_dir: &Path, prefix: &str) -> Result<PathBuf, ConversionError> {
    // Keep directory basenames well under NAME_MAX even if callers pass long prefixes.
    let slug = bound_temp_slug(prefix, 48);
    for _ in 0..100 {
        let counter = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // Short hex of nanos keeps names unique without multi-decade decimal width.
        let name = format!("{slug}-{}-{counter:x}-{:x}", std::process::id(), nanos);
        debug_assert!(name.len() <= FS_NAME_MAX);
        let base = base_dir.join(&name);
        match create_private_dir(&base) {
            Ok(()) => return Ok(base),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(ConversionError::new(format!(
                    "could not create temporary directory {}: {error}",
                    base.display()
                )));
            }
        }
    }
    Err(ConversionError::new(format!(
        "could not create a unique temporary directory for prefix {prefix} after 100 attempts"
    )))
}

/// Create a directory with mode `0700` on Unix (private by default).
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

/// Sanitize and truncate a temp-name slug, replacing path separators.
fn bound_temp_slug(raw: &str, max_chars: usize) -> String {
    let mut slug = String::new();
    for ch in raw.chars() {
        if slug.len() >= max_chars {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            slug.push(ch);
        } else if ch == '.' || ch == ' ' {
            slug.push('-');
        }
        // Drop other characters (including `/` `\`).
    }
    if slug.is_empty() {
        slug.push_str("shift");
    }
    slug
}

/// Stable short hash of `input` for bounded temp file basenames.
pub fn short_path_hash(input: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

/// Build a temp file basename under [`FS_NAME_MAX`], reserving room for `suffix`.
///
/// Format: `.{slug}-{hash16}-{pid:x}-{counter:x}{suffix}` where `slug` is a
/// short sanitized fragment of `stem`. Callers must pass a suffix that includes
/// any extension (e.g. `.shift-partial`).
pub fn unique_temp_file_name(stem: &str, suffix: &str) -> String {
    unique_temp_file_name_with_counter(
        stem,
        suffix,
        TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed),
    )
}

fn unique_temp_file_name_with_counter(stem: &str, suffix: &str, counter: u64) -> String {
    let pid = std::process::id();
    let hash = short_path_hash(stem);
    // Fixed-width pieces: '-' + 16 hex hash + '-' + up to 8 hex pid + '-' + counter hex.
    let fixed = format!("-{hash:016x}-{pid:x}-{counter:x}");
    let reserved = 1 // leading '.'
        + fixed.len()
        + suffix.len();
    let slug_budget = FS_NAME_MAX.saturating_sub(reserved).min(32);
    let slug = bound_temp_slug(stem, slug_budget.max(1));
    let mut name = format!(".{slug}{fixed}{suffix}");
    if name.len() > FS_NAME_MAX {
        // Extreme suffix: drop the slug entirely.
        name = format!(".t{fixed}{suffix}");
        if name.len() > FS_NAME_MAX {
            // Last resort: hash-only + truncated suffix (should not happen for
            // our known suffixes).
            let keep_suffix = FS_NAME_MAX.saturating_sub(18);
            let short_suffix: String = suffix
                .chars()
                .rev()
                .take(keep_suffix)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            name = format!(".{hash:016x}{short_suffix}");
        }
    }
    debug_assert!(
        name.len() <= FS_NAME_MAX,
        "temp name too long: {}",
        name.len()
    );
    name
}

/// Clear all memoized tool-discovery results so the next diagnostics/probe pass
/// sees the current filesystem/PATH state.
///
/// `ConversionRegistry` instances capture resolved executable paths when they
/// are built, so callers that refresh diagnostics and discover a newly installed
/// tool must also rebuild their registry. Refreshing diagnostics via this clear
/// re-probes the executable paths and readiness.
pub fn clear_tool_discovery_cache() {
    find_executable_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    resolve_path_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

fn resolve_tool_path_uncached(
    env_override: &str,
    default_name: &str,
    local_candidates: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os(env_override) {
        if !override_path.is_empty() {
            let path = PathBuf::from(&override_path);
            let as_path = Path::new(&override_path);
            if as_path.is_absolute() || as_path.components().count() > 1 {
                // Explicit absolute or relative path: use it as-is.
                if is_runnable(&path) {
                    return Some(path);
                }
                // Surface configured-but-broken paths so diagnostics can show them.
                if path.exists() {
                    return Some(path);
                }
                return Some(path);
            }
            // Bare name: search PATH/common dirs first, never the current directory.
            if let Some(found) = find_executable(&override_path) {
                return Some(found);
            }
            return Some(path);
        }
    }

    for candidate in local_candidates {
        if is_runnable(candidate) {
            return Some(candidate.clone());
        }
    }

    find_executable(default_name)
}

/// Like [`resolve_tool_path`], but always returns a value suitable for
/// `Command::new`: an absolute path when discovery succeeds, otherwise the
/// bare `default_name` so spawn fails with "executable not found".
pub fn resolve_tool_executable(
    env_override: &str,
    default_name: &str,
    local_candidates: &[PathBuf],
) -> OsString {
    resolve_tool_path(env_override, default_name, local_candidates)
        .map(|path| path.into_os_string())
        .unwrap_or_else(|| OsString::from(default_name))
}

/// Locate a converter shipped in `Shift.app/Contents/Resources/runtime/bin`.
///
/// The ancestor walk also supports the bundled CLI under `Resources/bin`,
/// including when Homebrew invokes it through a symlink.
pub fn bundled_runtime_tool(name: &str) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let executable = executable.canonicalize().unwrap_or(executable);
    for ancestor in executable.ancestors() {
        match ancestor.file_name().and_then(|value| value.to_str()) {
            Some("Resources") => return Some(ancestor.join("runtime/bin").join(name)),
            Some("Contents") => {
                return Some(ancestor.join("Resources/runtime/bin").join(name));
            }
            _ => {}
        }
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::Instant;

    /// Restores an env var on drop (including panic unwind).
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: caller must hold crate::ENV_LOCK.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: paired with set under crate::ENV_LOCK.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Restores the process working directory on drop (including panic unwind).
    struct CwdGuard {
        previous: PathBuf,
    }

    impl CwdGuard {
        fn enter(path: &Path) -> Self {
            let previous = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { previous }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn shell_command(path: &Path) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(path);
        cmd
    }

    #[test]
    fn captures_successful_output() {
        let path = std::env::temp_dir().join(format!("shift-process-ok-{}", std::process::id()));
        write_script(&path, "#!/bin/sh\nprintf 'hello'\nprintf 'err' >&2\n");
        let output = run_command(shell_command(&path), Duration::from_secs(5), 1024).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello");
        assert_eq!(output.stderr, b"err");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn times_out_hanging_processes() {
        let path = std::env::temp_dir().join(format!("shift-process-hang-{}", std::process::id()));
        write_script(&path, "#!/bin/sh\nsleep 30\n");
        let started = Instant::now();
        let error =
            run_command(shell_command(&path), Duration::from_millis(300), 1024).unwrap_err();
        let elapsed = started.elapsed();
        assert!(error.to_string().contains("timed out"), "error: {error}");
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout took too long: {elapsed:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_oversized_stdout() {
        let path = std::env::temp_dir().join(format!("shift-process-big-{}", std::process::id()));
        write_script(
            &path,
            "#!/bin/sh\n# Emit more than the 64-byte cap.\ndd if=/dev/zero bs=200 count=1 2>/dev/null\n",
        );
        let error = run_command(shell_command(&path), Duration::from_secs(5), 64).unwrap_err();
        assert!(
            error.to_string().contains("exceeded") || error.to_string().contains("limit"),
            "error: {error}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn oversized_output_kills_a_still_running_process_immediately() {
        let path =
            std::env::temp_dir().join(format!("shift-process-big-hang-{}", std::process::id()));
        write_script(
            &path,
            "#!/bin/sh\ndd if=/dev/zero bs=200 count=1 2>/dev/null\nsleep 30\n",
        );
        let started = Instant::now();
        let error = run_command(shell_command(&path), Duration::from_secs(20), 64).unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            error.to_string().contains("exceeded") || error.to_string().contains("limit"),
            "error: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "output limit took too long to stop the process: {elapsed:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_file_limited_rejects_large_files() {
        let path = std::env::temp_dir().join(format!("shift-process-file-{}", std::process::id()));
        std::fs::write(&path, vec![0_u8; 100]).unwrap();
        let error = read_file_limited(&path, 50).unwrap_err();
        assert!(error.to_string().contains("too large"), "error: {error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancel_before_spawn_returns_cancelled() {
        let path =
            std::env::temp_dir().join(format!("shift-process-pre-cancel-{}", std::process::id()));
        write_script(&path, "#!/bin/sh\necho should-not-run\n");
        let cancel = Arc::new(AtomicBool::new(true));
        let error = run_command_cancellable(
            shell_command(&path),
            Duration::from_secs(5),
            1024,
            Some(Arc::clone(&cancel)),
        )
        .unwrap_err();
        assert!(error.is_cancelled(), "error: {error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancel_mid_run_stops_hanging_process() {
        let path =
            std::env::temp_dir().join(format!("shift-process-mid-cancel-{}", std::process::id()));
        write_script(&path, "#!/bin/sh\nsleep 30\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancel);
        let started = Instant::now();
        let watcher = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel_flag.store(true, Ordering::SeqCst);
        });
        let error = run_command_cancellable(
            shell_command(&path),
            Duration::from_secs(20),
            1024,
            Some(Arc::clone(&cancel)),
        )
        .unwrap_err();
        let _ = watcher.join();
        let elapsed = started.elapsed();
        assert!(error.is_cancelled(), "error: {error}");
        assert!(
            elapsed < Duration::from_secs(5),
            "cancel took too long: {elapsed:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancel_is_distinct_from_timeout() {
        let path =
            std::env::temp_dir().join(format!("shift-process-cancel-kind-{}", std::process::id()));
        write_script(&path, "#!/bin/sh\nsleep 30\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancel);
        let watcher = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            cancel_flag.store(true, Ordering::SeqCst);
        });
        let error = run_command_cancellable(
            shell_command(&path),
            Duration::from_secs(20),
            1024,
            Some(cancel),
        )
        .unwrap_err();
        let _ = watcher.join();
        assert!(error.is_cancelled(), "error: {error}");
        assert!(
            !error.to_string().contains("timed out"),
            "cancel should not surface as timeout: {error}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_process_group_children() {
        // Parent shell sleeps; child sleep is in the same process group thanks to
        // process_group(0). Cancel must tear down the whole group.
        let path =
            std::env::temp_dir().join(format!("shift-process-pg-cancel-{}", std::process::id()));
        write_script(&path, "#!/bin/sh\nsleep 30 &\nwait\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancel);
        let started = Instant::now();
        let watcher = thread::spawn(move || {
            thread::sleep(Duration::from_millis(120));
            cancel_flag.store(true, Ordering::SeqCst);
        });
        let error = run_command_cancellable(
            shell_command(&path),
            Duration::from_secs(20),
            1024,
            Some(Arc::clone(&cancel)),
        )
        .unwrap_err();
        let _ = watcher.join();
        let elapsed = started.elapsed();
        assert!(error.is_cancelled(), "error: {error}");
        assert!(
            elapsed < Duration::from_secs(5),
            "process-group cancel took too long: {elapsed:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn process_timeout_respects_env_and_default() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("SHIFT_CONVERSION_TIMEOUT_SECS").ok();
        unsafe { std::env::remove_var("SHIFT_CONVERSION_TIMEOUT_SECS") };
        assert_eq!(process_timeout(), DEFAULT_PROCESS_TIMEOUT);
        unsafe { std::env::set_var("SHIFT_CONVERSION_TIMEOUT_SECS", "123") };
        assert_eq!(process_timeout(), Duration::from_secs(123));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("SHIFT_CONVERSION_TIMEOUT_SECS", value),
                None => std::env::remove_var("SHIFT_CONVERSION_TIMEOUT_SECS"),
            }
        }
    }

    #[test]
    fn max_output_bytes_respects_env_and_default() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES").ok();
        unsafe { std::env::remove_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES") };
        assert_eq!(max_output_bytes(), DEFAULT_MAX_OUTPUT_BYTES);
        unsafe { std::env::set_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES", "1024") };
        assert_eq!(max_output_bytes(), 1024);
        unsafe {
            match previous {
                Some(value) => std::env::set_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES", value),
                None => std::env::remove_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES"),
            }
        }
    }

    #[test]
    fn is_runnable_requires_regular_executable_file() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exec_path =
            std::env::temp_dir().join(format!("shift-process-runnable-{}", std::process::id()));
        write_script(&exec_path, "#!/bin/sh\necho ok\n");
        assert!(is_runnable(&exec_path));

        let non_exec =
            std::env::temp_dir().join(format!("shift-process-nonexec-{}", std::process::id()));
        std::fs::write(&non_exec, b"not executable").unwrap();
        assert!(!is_runnable(&non_exec));

        let dir = std::env::temp_dir().join(format!("shift-process-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_runnable(&dir));

        let missing =
            std::env::temp_dir().join(format!("shift-process-missing-{}", std::process::id()));
        assert!(!is_runnable(&missing));

        let _ = std::fs::remove_file(&exec_path);
        let _ = std::fs::remove_file(&non_exec);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn find_executable_searches_path_and_common_dirs() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let bin_dir =
            std::env::temp_dir().join(format!("shift-process-bin-{}", std::process::id()));
        std::fs::create_dir_all(&bin_dir).unwrap();
        let tool = bin_dir.join("shift_test_tool");
        write_script(&tool, "#!/bin/sh\necho found\n");

        let previous = std::env::var("PATH").ok();
        let new_path = match &previous {
            Some(old) => format!("{}:{}", bin_dir.display(), old),
            None => bin_dir.display().to_string(),
        };
        unsafe { std::env::set_var("PATH", &new_path) };

        assert_eq!(
            find_executable("shift_test_tool").as_deref(),
            Some(tool.as_path())
        );
        assert!(find_executable("shift_test_tool_definitely_missing").is_none());

        unsafe {
            match previous {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
        let _ = std::fs::remove_file(&tool);
        let _ = std::fs::remove_dir(&bin_dir);
    }

    #[test]
    fn resolve_tool_path_prefers_env_override_local_candidate_and_default() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = std::env::temp_dir().join(format!("shift-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();

        let env_tool = temp.join("env_tool");
        let local_tool = temp.join("local_tool");
        let default_tool = temp.join("default_tool");
        write_script(&env_tool, "#!/bin/sh\necho env\n");
        write_script(&local_tool, "#!/bin/sh\necho local\n");
        write_script(&default_tool, "#!/bin/sh\necho default\n");

        let previous_path = std::env::var("PATH").ok();
        let new_path = match &previous_path {
            Some(old) => format!("{}:{}", temp.display(), old),
            None => temp.display().to_string(),
        };
        unsafe { std::env::set_var("PATH", &new_path) };

        let env_key = "SHIFT_PROCESS_RESOLVE_TEST_BIN";
        let previous_env = std::env::var_os(env_key);

        // Env override wins.
        unsafe { std::env::set_var(env_key, &env_tool) };
        assert_eq!(
            resolve_tool_path(env_key, "default_tool", std::slice::from_ref(&local_tool)),
            Some(env_tool.clone())
        );

        // Configured-but-broken override is still surfaced.
        let broken = temp.join("broken_tool");
        std::fs::write(&broken, b"not executable").unwrap();
        unsafe { std::env::set_var(env_key, &broken) };
        assert_eq!(
            resolve_tool_path(env_key, "default_tool", std::slice::from_ref(&local_tool)),
            Some(broken.clone())
        );

        // Bare name in env override is resolved on PATH.
        unsafe { std::env::set_var(env_key, "default_tool") };
        assert_eq!(
            resolve_tool_path(env_key, "other_default", std::slice::from_ref(&local_tool)),
            Some(default_tool.clone())
        );

        // When env is unset, local candidates take precedence over default search.
        unsafe { std::env::remove_var(env_key) };
        assert_eq!(
            resolve_tool_path(env_key, "default_tool", std::slice::from_ref(&local_tool)),
            Some(local_tool.clone())
        );

        // Fallback to the default name on PATH.
        assert_eq!(
            resolve_tool_path(env_key, "default_tool", &[]),
            Some(default_tool.clone())
        );

        // Nothing matches -> None.
        assert!(resolve_tool_path(env_key, "no_such_tool", &[]).is_none());

        unsafe {
            match previous_env {
                Some(value) => std::env::set_var(env_key, value),
                None => std::env::remove_var(env_key),
            }
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }

        let _ = std::fs::remove_file(&env_tool);
        let _ = std::fs::remove_file(&local_tool);
        let _ = std::fs::remove_file(&default_tool);
        let _ = std::fs::remove_file(&broken);
        let _ = std::fs::remove_dir(&temp);
    }

    #[test]
    fn resolve_tool_executable_falls_back_to_bare_name() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env_key = "SHIFT_PROCESS_RESOLVE_EXEC_BIN";
        unsafe { std::env::remove_var(env_key) };
        assert_eq!(
            resolve_tool_executable(env_key, "missing_default", &[]),
            std::ffi::OsString::from("missing_default")
        );
    }

    #[test]
    fn read_file_limited_reads_and_rejects_by_size() {
        let path =
            std::env::temp_dir().join(format!("shift-process-read-file-{}", std::process::id()));
        std::fs::write(&path, b"hello world").unwrap();

        assert_eq!(read_file_limited(&path, 100).unwrap(), b"hello world");
        assert_eq!(read_file_limited(&path, 11).unwrap(), b"hello world");
        let error = read_file_limited(&path, 5).unwrap_err();
        assert!(error.to_string().contains("too large"), "error: {error}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_limited_respects_maximum_and_reports_truncation() {
        assert!(read_limited(std::io::empty(), 10).unwrap().bytes.is_empty());
        assert!(!read_limited(std::io::empty(), 10).unwrap().truncated);

        let data = b"exactly ten".as_slice();
        let result = read_limited(data, 11).unwrap();
        assert_eq!(result.bytes, b"exactly ten");
        assert!(!result.truncated);

        let result = read_limited(data, 5).unwrap();
        assert_eq!(result.bytes, b"exact");
        assert!(result.truncated);

        let result = read_limited(data, 20).unwrap();
        assert_eq!(result.bytes, b"exactly ten");
        assert!(!result.truncated);
    }

    #[test]
    fn run_command_reports_missing_executable() {
        let missing =
            std::env::temp_dir().join(format!("shift-process-missing-exe-{}", std::process::id()));
        let error = run_command(Command::new(&missing), Duration::from_secs(1), 1024).unwrap_err();
        assert!(error.is_executable_not_found(), "error: {error}");
    }

    #[test]
    fn version_label_cmp_orders_semverish_names() {
        assert_eq!(
            version_label_cmp("v22.23.1", "v18.0.0"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            version_label_cmp("v26.2.0", "v22.23.1"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            version_label_cmp("22.1", "22.1.0"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn common_bin_dirs_includes_nvm_layout_under_home() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("shift-home-nvm-{}", std::process::id()));
        let node_bin = home.join("versions/node/v22.23.1/bin");
        std::fs::create_dir_all(&node_bin).unwrap();
        // Unique name: runners often already have /opt/homebrew/bin/node, so a
        // bare "node" probe would not prove nvm dirs are searched.
        let probe = node_bin.join("shift_nvm_probe_tool");
        write_script(&probe, "#!/bin/sh\necho ok\n");

        let previous_home = std::env::var_os("HOME");
        let previous_nvm = std::env::var_os("NVM_DIR");
        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("NVM_DIR", &home);
            // Minimal GUI-like PATH so discovery must use common_bin_dirs.
            std::env::set_var("PATH", "/usr/bin:/bin");
        }
        clear_tool_discovery_cache();

        let dirs = common_bin_dirs();
        assert!(
            dirs.iter().any(|dir| dir == &node_bin),
            "expected nvm bin dir in {dirs:?}"
        );
        assert_eq!(
            find_executable("shift_nvm_probe_tool").as_deref(),
            Some(probe.as_path())
        );

        unsafe {
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match previous_nvm {
                Some(value) => std::env::set_var("NVM_DIR", value),
                None => std::env::remove_var("NVM_DIR"),
            }
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
        clear_tool_discovery_cache();
        let _ = std::fs::remove_dir_all(&home);
    }

    fn unique_suffix(tag: &str) -> String {
        format!(
            "{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn unique_temp_dir_creates_distinct_directories_with_prefix() {
        // Other process tests temporarily override TMPDIR while exercising
        // failure paths. Serialize this test with those mutations so the
        // normal parallel test runner cannot observe their synthetic path.
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prefix = format!("shift-utd-{}", std::process::id());
        let a = unique_temp_dir(&prefix).unwrap();
        let b = unique_temp_dir(&prefix).unwrap();
        assert!(a.is_dir(), "expected directory: {}", a.display());
        assert!(b.is_dir(), "expected directory: {}", b.display());
        assert_ne!(a, b, "unique_temp_dir must not collide");
        let a_name = a.file_name().and_then(|n| n.to_str()).unwrap();
        let b_name = b.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(
            a_name.starts_with(&prefix),
            "dir name should start with prefix: {a_name}"
        );
        assert!(
            b_name.starts_with(&prefix),
            "dir name should start with prefix: {b_name}"
        );
        assert!(
            a_name.contains(&std::process::id().to_string()),
            "dir name should include pid: {a_name}"
        );
        // Parallel creations stay unique under load.
        let more: Vec<PathBuf> = (0..8).map(|_| unique_temp_dir(&prefix).unwrap()).collect();
        let mut seen = std::collections::HashSet::new();
        seen.insert(a.clone());
        seen.insert(b.clone());
        for path in &more {
            assert!(
                seen.insert(path.clone()),
                "duplicate temp dir: {}",
                path.display()
            );
        }
        for path in more.into_iter().chain([a, b]) {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn clear_tool_discovery_cache_invalidates_memoized_paths() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("cache-clear");
        let bin_dir = std::env::temp_dir().join(format!("shift-process-cache-{suffix}"));
        std::fs::create_dir_all(&bin_dir).unwrap();
        let tool_name = format!("shift_cache_tool_{}", std::process::id());
        let tool = bin_dir.join(&tool_name);
        write_script(&tool, "#!/bin/sh\necho cached\n");

        let previous_path = std::env::var_os("PATH");
        let new_path = match &previous_path {
            Some(old) => format!("{}:{}", bin_dir.display(), old.to_string_lossy()),
            None => bin_dir.display().to_string(),
        };
        unsafe { std::env::set_var("PATH", &new_path) };
        clear_tool_discovery_cache();

        assert_eq!(find_executable(&tool_name).as_deref(), Some(tool.as_path()));

        // Remove the binary; memoized result still returns the old path.
        std::fs::remove_file(&tool).unwrap();
        assert_eq!(
            find_executable(&tool_name).as_deref(),
            Some(tool.as_path()),
            "cache should still report the previously resolved path"
        );

        clear_tool_discovery_cache();
        assert!(
            find_executable(&tool_name).is_none(),
            "after clear, missing tool must not resolve"
        );

        // Re-create and ensure rediscovery works after clear.
        write_script(&tool, "#!/bin/sh\necho again\n");
        clear_tool_discovery_cache();
        assert_eq!(find_executable(&tool_name).as_deref(), Some(tool.as_path()));

        unsafe {
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
        clear_tool_discovery_cache();
        let _ = std::fs::remove_file(&tool);
        let _ = std::fs::remove_dir(&bin_dir);
    }

    #[test]
    fn clear_tool_discovery_cache_also_clears_resolve_tool_path_cache() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("resolve-cache");
        let temp = std::env::temp_dir().join(format!("shift-process-resolve-cache-{suffix}"));
        std::fs::create_dir_all(&temp).unwrap();
        let local = temp.join("local_cached");
        write_script(&local, "#!/bin/sh\necho local\n");

        let env_key = "SHIFT_PROCESS_RESOLVE_CACHE_TEST";
        let previous = std::env::var_os(env_key);
        unsafe { std::env::remove_var(env_key) };
        clear_tool_discovery_cache();

        assert_eq!(
            resolve_tool_path(env_key, "missing_default_xyz", std::slice::from_ref(&local)),
            Some(local.clone())
        );

        // Remove local candidate; cache still returns the old path until clear.
        std::fs::remove_file(&local).unwrap();
        assert_eq!(
            resolve_tool_path(env_key, "missing_default_xyz", std::slice::from_ref(&local)),
            Some(local.clone())
        );

        clear_tool_discovery_cache();
        assert!(
            resolve_tool_path(env_key, "missing_default_xyz", std::slice::from_ref(&local))
                .is_none()
        );

        unsafe {
            match previous {
                Some(value) => std::env::set_var(env_key, value),
                None => std::env::remove_var(env_key),
            }
        }
        clear_tool_discovery_cache();
        let _ = std::fs::remove_dir(&temp);
    }

    #[test]
    fn bundled_runtime_tool_returns_none_for_cargo_test_binary() {
        // Cargo test binaries live under target/.../deps and have no Resources or
        // Contents ancestor, so bundled discovery must return None (not a soft
        // either-way assertion that always passes).
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.canonicalize().unwrap_or(exe);
        let has_bundle_ancestor = exe.ancestors().any(|ancestor| {
            matches!(
                ancestor.file_name().and_then(|n| n.to_str()),
                Some("Resources" | "Contents")
            )
        });
        assert!(
            !has_bundle_ancestor,
            "test binary unexpectedly under app bundle layout: {}",
            exe.display()
        );
        assert!(
            bundled_runtime_tool("pandoc").is_none(),
            "cargo test binary must not resolve a bundled runtime tool"
        );
        assert!(
            bundled_runtime_tool("").is_none(),
            "empty tool name must also be None outside a bundle"
        );
    }

    #[test]
    fn version_label_parts_handles_empty_non_numeric_and_prerelease() {
        assert!(version_label_parts("").is_empty());
        assert!(version_label_parts("v").is_empty());
        assert!(version_label_parts("alpha").is_empty());
        assert!(version_label_parts("---").is_empty());
        assert_eq!(version_label_parts("22"), vec![22]);
        assert_eq!(version_label_parts("v22"), vec![22]);
        assert_eq!(version_label_parts("v22.23.1"), vec![22, 23, 1]);
        assert_eq!(version_label_parts("22.23.1"), vec![22, 23, 1]);
        // Multi-dot / trailing junk still extracts leading numeric segments.
        assert_eq!(version_label_parts("v22.23.1.extra"), vec![22, 23, 1]);
        assert_eq!(version_label_parts("22.23.1-rc.1"), vec![22, 23, 1, 1]);
        assert_eq!(version_label_parts("v18.20.0-pre"), vec![18, 20, 0]);
        assert_eq!(version_label_parts("node-v20.11.0"), vec![20, 11, 0]);
        // Non-ascii separators between digits still split.
        assert_eq!(version_label_parts("1_2_3"), vec![1, 2, 3]);
        assert_eq!(version_label_parts("10.0.0+build.5"), vec![10, 0, 0, 5]);
        // Leading zeros parse as numbers.
        assert_eq!(version_label_parts("v01.02.003"), vec![1, 2, 3]);
    }

    #[test]
    fn version_label_cmp_edge_cases_empty_equal_and_prerelease() {
        use std::cmp::Ordering;
        assert_eq!(version_label_cmp("", ""), Ordering::Equal);
        assert_eq!(version_label_cmp("alpha", "beta"), Ordering::Equal);
        assert_eq!(version_label_cmp("v1", "1"), Ordering::Equal);
        assert_eq!(version_label_cmp("v10.0.0", "v9.9.9"), Ordering::Greater);
        assert_eq!(version_label_cmp("v9.9.9", "v10.0.0"), Ordering::Less);
        assert_eq!(version_label_cmp("22.1.0", "22.1"), Ordering::Greater);
        assert_eq!(version_label_cmp("22.1", "22.1.0"), Ordering::Less);
        // Shared prefix then extra segment wins as longer.
        assert_eq!(
            version_label_cmp("1.2.3-rc.2", "1.2.3-rc.1"),
            Ordering::Greater
        );
        assert_eq!(version_label_cmp("v0.0.0", "v0"), Ordering::Greater);
        // Equal numeric tuples from different string shapes.
        assert_eq!(version_label_cmp("v22.23.1", "22.23.1"), Ordering::Equal);
        // Completely non-numeric labels compare equal (both empty parts).
        assert_eq!(version_label_cmp("latest", "stable"), Ordering::Equal);
        // Mixed: numeric vs non-numeric — numeric parts win as non-empty vs empty.
        assert_eq!(version_label_cmp("v1", "latest"), Ordering::Greater);
        assert_eq!(version_label_cmp("latest", "v1"), Ordering::Less);
    }

    #[test]
    fn common_bin_dirs_includes_home_local_cargo_volta_asdf_mise() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("home-layout");
        let home = std::env::temp_dir().join(format!("shift-home-layout-{suffix}"));
        let expected_relative = [
            ".local/bin",
            ".cargo/bin",
            ".volta/bin",
            ".asdf/shims",
            ".local/share/mise/shims",
            ".mise/shims",
        ];
        for rel in expected_relative {
            std::fs::create_dir_all(home.join(rel)).unwrap();
        }

        let previous_home = std::env::var_os("HOME");
        let previous_nvm = std::env::var_os("NVM_DIR");
        let previous_fnm = std::env::var_os("FNM_DIR");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("NVM_DIR");
            std::env::remove_var("FNM_DIR");
        }

        let dirs = common_bin_dirs();
        for rel in expected_relative {
            let want = home.join(rel);
            assert!(
                dirs.iter().any(|dir| dir == &want),
                "expected {want:?} in common_bin_dirs, got {dirs:?}"
            );
        }
        // Fixed system locations always present.
        assert!(dirs.iter().any(|d| d == Path::new("/opt/homebrew/bin")));
        assert!(dirs.iter().any(|d| d == Path::new("/usr/local/bin")));
        assert!(dirs.iter().any(|d| d == Path::new("/Library/TeX/texbin")));
        // Default nvm layout under HOME when NVM_DIR unset (create a version so it appears).
        let default_nvm_bin = home.join(".nvm/versions/node/v21.0.0/bin");
        std::fs::create_dir_all(&default_nvm_bin).unwrap();
        let dirs_with_nvm = common_bin_dirs();
        assert!(
            dirs_with_nvm.iter().any(|d| d == &default_nvm_bin),
            "default nvm bin under HOME should be present; dirs={dirs_with_nvm:?}"
        );

        unsafe {
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match previous_nvm {
                Some(value) => std::env::set_var("NVM_DIR", value),
                None => std::env::remove_var("NVM_DIR"),
            }
            match previous_fnm {
                Some(value) => std::env::set_var("FNM_DIR", value),
                None => std::env::remove_var("FNM_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn common_bin_dirs_orders_nvm_versions_newest_first() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("nvm-order");
        let home = std::env::temp_dir().join(format!("shift-home-nvm-order-{suffix}"));
        let nvm_root = home.join(".nvm");
        let old_bin = nvm_root.join("versions/node/v18.0.0/bin");
        let mid_bin = nvm_root.join("versions/node/v20.11.0/bin");
        let new_bin = nvm_root.join("versions/node/v22.23.1/bin");
        for bin in [&old_bin, &mid_bin, &new_bin] {
            std::fs::create_dir_all(bin).unwrap();
        }

        let previous_home = std::env::var_os("HOME");
        let previous_nvm = std::env::var_os("NVM_DIR");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("NVM_DIR", &nvm_root);
        }

        let dirs = common_bin_dirs();
        let positions: Vec<_> = [&new_bin, &mid_bin, &old_bin]
            .iter()
            .map(|want| {
                dirs.iter()
                    .position(|d| d == *want)
                    .unwrap_or_else(|| panic!("missing {want:?} in {dirs:?}"))
            })
            .collect();
        assert!(
            positions[0] < positions[1] && positions[1] < positions[2],
            "expected newest nvm first: positions={positions:?}, dirs={dirs:?}"
        );

        unsafe {
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match previous_nvm {
                Some(value) => std::env::set_var("NVM_DIR", value),
                None => std::env::remove_var("NVM_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn common_bin_dirs_includes_fnm_layout_under_home_and_fnm_dir() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("fnm-layout");
        let home = std::env::temp_dir().join(format!("shift-home-fnm-{suffix}"));
        let fnm_custom = home.join("custom-fnm");
        let custom_bin = fnm_custom.join("node-versions/v22.1.0/installation/bin");
        let home_fnm_bin = home.join(".fnm/node-versions/v20.0.0/installation/bin");
        let share_fnm_bin = home.join(".local/share/fnm/node-versions/v18.0.0/installation/bin");
        for bin in [&custom_bin, &home_fnm_bin, &share_fnm_bin] {
            std::fs::create_dir_all(bin).unwrap();
        }
        let probe = custom_bin.join("shift_fnm_probe_tool");
        write_script(&probe, "#!/bin/sh\necho fnm\n");

        let previous_home = std::env::var_os("HOME");
        let previous_fnm = std::env::var_os("FNM_DIR");
        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("FNM_DIR", &fnm_custom);
            std::env::set_var("PATH", "/usr/bin:/bin");
        }
        clear_tool_discovery_cache();

        let dirs = common_bin_dirs();
        for want in [&custom_bin, &home_fnm_bin, &share_fnm_bin] {
            assert!(
                dirs.iter().any(|d| d == want),
                "expected fnm bin {want:?} in {dirs:?}"
            );
        }
        assert_eq!(
            find_executable("shift_fnm_probe_tool").as_deref(),
            Some(probe.as_path())
        );

        unsafe {
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match previous_fnm {
                Some(value) => std::env::set_var("FNM_DIR", value),
                None => std::env::remove_var("FNM_DIR"),
            }
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
        clear_tool_discovery_cache();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn is_runnable_accepts_executable_symlink_and_rejects_non_exec_symlink() {
        let suffix = unique_suffix("symlink");
        let target = std::env::temp_dir().join(format!("shift-process-symlink-target-{suffix}"));
        let link = std::env::temp_dir().join(format!("shift-process-symlink-link-{suffix}"));
        let non_exec_target =
            std::env::temp_dir().join(format!("shift-process-symlink-nonexec-{suffix}"));
        let non_exec_link =
            std::env::temp_dir().join(format!("shift-process-symlink-nonexec-link-{suffix}"));

        write_script(&target, "#!/bin/sh\necho ok\n");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            is_runnable(&link),
            "symlink to executable should be runnable"
        );

        std::fs::write(&non_exec_target, b"not exec").unwrap();
        let _ = std::fs::remove_file(&non_exec_link);
        std::os::unix::fs::symlink(&non_exec_target, &non_exec_link).unwrap();
        assert!(
            !is_runnable(&non_exec_link),
            "symlink to non-executable must not be runnable"
        );

        // Broken symlink is not a regular file.
        let dangling =
            std::env::temp_dir().join(format!("shift-process-symlink-dangling-{suffix}"));
        let _ = std::fs::remove_file(&dangling);
        std::os::unix::fs::symlink(
            std::env::temp_dir().join(format!("shift-process-does-not-exist-{suffix}")),
            &dangling,
        )
        .unwrap();
        assert!(!is_runnable(&dangling));

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_file(&non_exec_link);
        let _ = std::fs::remove_file(&non_exec_target);
        let _ = std::fs::remove_file(&dangling);
    }

    #[test]
    fn is_runnable_rejects_directory_with_execute_bit() {
        let suffix = unique_suffix("dir-exec");
        let dir = std::env::temp_dir().join(format!("shift-process-dir-exec-{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut permissions = std::fs::metadata(&dir).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&dir, permissions).unwrap();
        assert!(
            !is_runnable(&dir),
            "directories must never be runnable tools"
        );
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn resolve_tool_path_empty_env_override_falls_through() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("empty-override");
        let temp = std::env::temp_dir().join(format!("shift-resolve-empty-{suffix}"));
        std::fs::create_dir_all(&temp).unwrap();
        let local = temp.join("local_only");
        write_script(&local, "#!/bin/sh\necho local\n");

        let env_key = "SHIFT_PROCESS_EMPTY_OVERRIDE";
        let previous = std::env::var_os(env_key);
        unsafe { std::env::set_var(env_key, "") };
        clear_tool_discovery_cache();

        assert_eq!(
            resolve_tool_path(env_key, "nope", std::slice::from_ref(&local)),
            Some(local.clone()),
            "empty override should fall through to local candidates"
        );

        unsafe {
            match previous {
                Some(value) => std::env::set_var(env_key, value),
                None => std::env::remove_var(env_key),
            }
        }
        clear_tool_discovery_cache();
        let _ = std::fs::remove_file(&local);
        let _ = std::fs::remove_dir(&temp);
    }

    #[test]
    fn resolve_tool_path_absolute_missing_override_is_still_surfaced() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("abs-missing");
        let missing = std::env::temp_dir().join(format!("shift-resolve-abs-missing-{suffix}"));
        let _ = std::fs::remove_file(&missing);

        let env_key = "SHIFT_PROCESS_ABS_MISSING";
        let previous = std::env::var_os(env_key);
        unsafe { std::env::set_var(env_key, &missing) };
        clear_tool_discovery_cache();

        assert_eq!(
            resolve_tool_path(env_key, "default_unused", &[]),
            Some(missing.clone()),
            "configured absolute path must surface even when missing"
        );

        // resolve_tool_executable returns the same configured path.
        assert_eq!(
            resolve_tool_executable(env_key, "default_unused", &[]),
            missing.into_os_string()
        );

        unsafe {
            match previous {
                Some(value) => std::env::set_var(env_key, value),
                None => std::env::remove_var(env_key),
            }
        }
        clear_tool_discovery_cache();
    }

    #[test]
    fn resolve_tool_path_relative_multi_component_override() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("rel-multi");
        let work = std::env::temp_dir().join(format!("shift-resolve-rel-{suffix}"));
        std::fs::create_dir_all(work.join("nested")).unwrap();
        let tool = work.join("nested/tool.sh");
        write_script(&tool, "#!/bin/sh\necho rel\n");

        // Relative multi-component path is used as-is (not PATH search).
        let previous_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&work).unwrap();

        let env_key = "SHIFT_PROCESS_REL_MULTI";
        let previous = std::env::var_os(env_key);
        unsafe { std::env::set_var(env_key, "nested/tool.sh") };
        clear_tool_discovery_cache();

        let resolved = resolve_tool_path(env_key, "unused", &[]);
        assert_eq!(
            resolved.as_deref(),
            Some(Path::new("nested/tool.sh")),
            "relative multi-component override should be returned as-is"
        );
        assert!(is_runnable(Path::new("nested/tool.sh")));

        unsafe {
            match previous {
                Some(value) => std::env::set_var(env_key, value),
                None => std::env::remove_var(env_key),
            }
        }
        clear_tool_discovery_cache();
        std::env::set_current_dir(previous_cwd).unwrap();
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn resolve_tool_path_skips_non_runnable_local_candidates() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("local-skip");
        let temp = std::env::temp_dir().join(format!("shift-resolve-local-skip-{suffix}"));
        std::fs::create_dir_all(&temp).unwrap();
        let non_exec = temp.join("not_exec");
        let good = temp.join("good_tool");
        std::fs::write(&non_exec, b"nope").unwrap();
        write_script(&good, "#!/bin/sh\necho good\n");

        let env_key = "SHIFT_PROCESS_LOCAL_SKIP";
        let previous = std::env::var_os(env_key);
        unsafe { std::env::remove_var(env_key) };
        clear_tool_discovery_cache();

        assert_eq!(
            resolve_tool_path(env_key, "unused", &[non_exec.clone(), good.clone()]),
            Some(good.clone())
        );
        // Only non-runnable candidates -> fall through to default (missing).
        assert!(resolve_tool_path(env_key, "definitely_missing_xyz", &[non_exec]).is_none());

        unsafe {
            match previous {
                Some(value) => std::env::set_var(env_key, value),
                None => std::env::remove_var(env_key),
            }
        }
        clear_tool_discovery_cache();
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn resolve_tool_executable_returns_resolved_absolute_path() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("exec-abs");
        let temp = std::env::temp_dir().join(format!("shift-resolve-exec-abs-{suffix}"));
        std::fs::create_dir_all(&temp).unwrap();
        let tool = temp.join("abs_tool");
        write_script(&tool, "#!/bin/sh\necho abs\n");

        let env_key = "SHIFT_PROCESS_EXEC_ABS";
        let previous = std::env::var_os(env_key);
        unsafe { std::env::set_var(env_key, &tool) };
        clear_tool_discovery_cache();

        assert_eq!(
            resolve_tool_executable(env_key, "default", &[]),
            tool.clone().into_os_string()
        );

        unsafe {
            match previous {
                Some(value) => std::env::set_var(env_key, value),
                None => std::env::remove_var(env_key),
            }
        }
        clear_tool_discovery_cache();
        let _ = std::fs::remove_file(&tool);
        let _ = std::fs::remove_dir(&temp);
    }

    #[test]
    fn find_executable_accepts_absolute_runnable_path() {
        // Discovery results are process-global; serialize cache clears with
        // the environment-mutating discovery tests.
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("find-abs");
        let tool = std::env::temp_dir().join(format!("shift-find-abs-{suffix}"));
        write_script(&tool, "#!/bin/sh\necho abs\n");
        clear_tool_discovery_cache();
        assert_eq!(find_executable(&tool).as_deref(), Some(tool.as_path()));
        // Non-runnable absolute path returns None.
        let non_exec = std::env::temp_dir().join(format!("shift-find-abs-nonexec-{suffix}"));
        std::fs::write(&non_exec, b"nope").unwrap();
        clear_tool_discovery_cache();
        assert!(find_executable(&non_exec).is_none());
        let _ = std::fs::remove_file(&tool);
        let _ = std::fs::remove_file(&non_exec);
        clear_tool_discovery_cache();
    }

    #[test]
    fn find_executable_skips_empty_path_components() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let suffix = unique_suffix("empty-path");
        let cwd_tool = format!("shift_cwd_tool_{suffix}");
        // Place a same-named file in cwd; empty PATH component must not find it.
        let work = std::env::temp_dir().join(format!("shift-empty-path-{suffix}"));
        std::fs::create_dir_all(&work).unwrap();
        let _cwd = CwdGuard::enter(&work);
        write_script(Path::new(&cwd_tool), "#!/bin/sh\necho cwd\n");

        // Leading empty component + only system bins — must not resolve via cwd.
        // PATH restore is RAII so assertion failure cannot poison later tests.
        let _path = EnvVarGuard::set("PATH", ":/usr/bin:/bin");
        clear_tool_discovery_cache();
        assert!(
            find_executable(&cwd_tool).is_none(),
            "empty PATH component must not resolve tools from cwd"
        );

        clear_tool_discovery_cache();
        let _ = std::fs::remove_file(&cwd_tool);
        // Drop cwd guard before removing work dir.
        drop(_cwd);
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn read_limited_zero_max_bytes_truncates_any_input() {
        let result = read_limited(b"hello".as_slice(), 0).unwrap();
        assert!(result.bytes.is_empty());
        assert!(result.truncated);

        let result = read_limited(std::io::empty(), 0).unwrap();
        assert!(result.bytes.is_empty());
        assert!(!result.truncated, "empty reader is not truncated");
    }

    #[test]
    fn read_file_limited_zero_max_rejects_nonempty_file() {
        let path =
            std::env::temp_dir().join(format!("shift-process-zero-max-{}", unique_suffix("read")));
        std::fs::write(&path, b"x").unwrap();
        let error = read_file_limited(&path, 0).unwrap_err();
        assert!(
            error.to_string().contains("too large") || error.to_string().contains("limit"),
            "error: {error}"
        );
        // Empty file with zero max is ok (metadata len == 0).
        std::fs::write(&path, b"").unwrap();
        assert_eq!(read_file_limited(&path, 0).unwrap(), b"");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_file_limited_missing_path_errors() {
        let missing = std::env::temp_dir().join(format!(
            "shift-process-missing-file-{}",
            unique_suffix("read")
        ));
        let error = read_file_limited(&missing, 100).unwrap_err();
        assert!(
            error.to_string().contains("could not read"),
            "error: {error}"
        );
    }

    #[test]
    fn empty_stdout_command_succeeds_with_empty_buffers() {
        let path = std::env::temp_dir().join(format!(
            "shift-process-empty-stdout-{}",
            unique_suffix("empty")
        ));
        write_script(&path, "#!/bin/sh\nexit 0\n");
        let output = run_command(shell_command(&path), Duration::from_secs(5), 1024).unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn empty_stdout_nonzero_exit_still_captures_status() {
        let path = std::env::temp_dir().join(format!(
            "shift-process-empty-fail-{}",
            unique_suffix("empty-fail")
        ));
        write_script(&path, "#!/bin/sh\nexit 7\n");
        let output = run_command(shell_command(&path), Duration::from_secs(5), 1024).unwrap();
        assert!(!output.status.success());
        assert_eq!(output.status.code(), Some(7));
        assert!(output.stdout.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn run_command_zero_max_bytes_rejects_any_stdout() {
        let path = std::env::temp_dir().join(format!(
            "shift-process-zero-stdout-{}",
            unique_suffix("zero")
        ));
        write_script(&path, "#!/bin/sh\nprintf 'x'\n");
        let error = run_command(shell_command(&path), Duration::from_secs(5), 0).unwrap_err();
        assert!(
            error.to_string().contains("exceeded") || error.to_string().contains("limit"),
            "error: {error}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn run_command_zero_max_bytes_allows_silent_success() {
        let path = std::env::temp_dir().join(format!(
            "shift-process-zero-silent-{}",
            unique_suffix("zero-silent")
        ));
        write_script(&path, "#!/bin/sh\nexit 0\n");
        let output = run_command(shell_command(&path), Duration::from_secs(5), 0).unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn process_timeout_ignores_zero_and_invalid_env() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("SHIFT_CONVERSION_TIMEOUT_SECS").ok();
        unsafe { std::env::set_var("SHIFT_CONVERSION_TIMEOUT_SECS", "0") };
        assert_eq!(process_timeout(), DEFAULT_PROCESS_TIMEOUT);
        unsafe { std::env::set_var("SHIFT_CONVERSION_TIMEOUT_SECS", "not-a-number") };
        assert_eq!(process_timeout(), DEFAULT_PROCESS_TIMEOUT);
        unsafe { std::env::set_var("SHIFT_CONVERSION_TIMEOUT_SECS", "") };
        assert_eq!(process_timeout(), DEFAULT_PROCESS_TIMEOUT);
        unsafe {
            match previous {
                Some(value) => std::env::set_var("SHIFT_CONVERSION_TIMEOUT_SECS", value),
                None => std::env::remove_var("SHIFT_CONVERSION_TIMEOUT_SECS"),
            }
        }
    }

    #[test]
    fn max_output_bytes_ignores_zero_and_invalid_env() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES").ok();
        unsafe { std::env::set_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES", "0") };
        assert_eq!(max_output_bytes(), DEFAULT_MAX_OUTPUT_BYTES);
        unsafe { std::env::set_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES", "abc") };
        assert_eq!(max_output_bytes(), DEFAULT_MAX_OUTPUT_BYTES);
        unsafe {
            match previous {
                Some(value) => std::env::set_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES", value),
                None => std::env::remove_var("SHIFT_CONVERSION_MAX_OUTPUT_BYTES"),
            }
        }
    }

    #[test]
    fn cancel_none_behaves_like_run_command() {
        let path = std::env::temp_dir().join(format!(
            "shift-process-cancel-none-{}",
            unique_suffix("cancel-none")
        ));
        write_script(&path, "#!/bin/sh\nprintf 'ok'\n");
        let output =
            run_command_cancellable(shell_command(&path), Duration::from_secs(5), 1024, None)
                .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn oversized_stderr_is_rejected() {
        let path = std::env::temp_dir().join(format!(
            "shift-process-big-stderr-{}",
            unique_suffix("stderr")
        ));
        write_script(
            &path,
            // Write zeros to the process stderr (not dd's diagnostic stream).
            "#!/bin/sh\nhead -c 200 /dev/zero >&2\n",
        );
        let error = run_command(shell_command(&path), Duration::from_secs(5), 64).unwrap_err();
        assert!(
            error.to_string().contains("exceeded") || error.to_string().contains("limit"),
            "error: {error}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn exact_max_output_bytes_boundary_is_allowed() {
        let path = std::env::temp_dir().join(format!(
            "shift-process-exact-max-{}",
            unique_suffix("exact")
        ));
        // 16 bytes of output with max 16 must succeed.
        write_script(&path, "#!/bin/sh\nprintf '0123456789abcdef'\n");
        let output = run_command(shell_command(&path), Duration::from_secs(5), 16).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 16);
        // One more byte must fail.
        write_script(&path, "#!/bin/sh\nprintf '0123456789abcdefX'\n");
        let error = run_command(shell_command(&path), Duration::from_secs(5), 16).unwrap_err();
        assert!(
            error.to_string().contains("exceeded") || error.to_string().contains("limit"),
            "error: {error}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_limited_large_chunk_boundary() {
        // Larger than the 8 KiB internal chunk to exercise multi-read path.
        let data = vec![b'a'; 20_000];
        let result = read_limited(data.as_slice(), 20_000).unwrap();
        assert_eq!(result.bytes.len(), 20_000);
        assert!(!result.truncated);

        let result = read_limited(data.as_slice(), 19_999).unwrap();
        assert_eq!(result.bytes.len(), 19_999);
        assert!(result.truncated);

        let result = read_limited(data.as_slice(), 8_192).unwrap();
        assert_eq!(result.bytes.len(), 8_192);
        assert!(result.truncated);
    }

    #[test]
    fn append_versioned_bin_dirs_ignores_missing_and_files() {
        let suffix = unique_suffix("versioned");
        let root = std::env::temp_dir().join(format!("shift-versioned-{suffix}"));
        // Missing root: no panic, dirs unchanged.
        let mut dirs = vec![PathBuf::from("/sentinel")];
        append_versioned_bin_dirs(&mut dirs, &root.join("missing"), "bin");
        assert_eq!(dirs, vec![PathBuf::from("/sentinel")]);

        // Root with a file entry (not a dir) is ignored; only version dirs count.
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("not-a-dir"), b"x").unwrap();
        let ver = root.join("v1.2.3");
        std::fs::create_dir_all(&ver).unwrap();
        append_versioned_bin_dirs(&mut dirs, &root, "bin");
        assert!(
            dirs.iter().any(|d| d == &ver.join("bin")),
            "expected version bin, got {dirs:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_after_process_exit_still_reports_cancelled() {
        // Deterministic cancel: keep the child alive until we arm cancel, then
        // allow it to exit. Arming cancel while the child is still running means
        // either the wait-loop Cancelled path or the post-exit cancel check fires
        // — never Ok — so this cannot flake under scheduling races.
        let suffix = unique_suffix("post-cancel");
        let path = std::env::temp_dir().join(format!("shift-process-post-cancel-{suffix}"));
        let running =
            std::env::temp_dir().join(format!("shift-process-post-cancel-{suffix}.running"));
        let exit_signal =
            std::env::temp_dir().join(format!("shift-process-post-cancel-{suffix}.exit"));
        let _ = std::fs::remove_file(&running);
        let _ = std::fs::remove_file(&exit_signal);

        write_script(
            &path,
            &format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nexit 0\n",
                running.display(),
                exit_signal.display()
            ),
        );

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancel);
        let running_flag = running.clone();
        let exit_flag = exit_signal.clone();
        let helper = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !running_flag.is_file() {
                if Instant::now() > deadline {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            // Child is known alive: arm cancel first so success is impossible.
            cancel_flag.store(true, Ordering::SeqCst);
            // Then allow the child to exit (may race with force_kill — both yield cancelled).
            let _ = std::fs::write(&exit_flag, b"");
        });

        let error = run_command_cancellable(
            shell_command(&path),
            Duration::from_secs(5),
            1024,
            Some(Arc::clone(&cancel)),
        )
        .unwrap_err();
        let _ = helper.join();
        assert!(error.is_cancelled(), "error: {error}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&running);
        let _ = std::fs::remove_file(&exit_signal);
    }

    #[test]
    fn resolve_tool_path_bare_name_missing_still_surfaces_name() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_tool_discovery_cache();
        let env_key = "SHIFT_TEST_BARE_MISSING_BIN";
        let old = std::env::var_os(env_key);
        let old_path = std::env::var_os("PATH");
        // Empty PATH so find_executable cannot resolve the bare name.
        unsafe {
            std::env::set_var(env_key, "totally-missing-shift-tool-xyz");
            std::env::set_var("PATH", "");
        }
        clear_tool_discovery_cache();
        let resolved = resolve_tool_path(env_key, "fallback", &[]);
        assert_eq!(
            resolved,
            Some(PathBuf::from("totally-missing-shift-tool-xyz")),
            "bare override should surface even when not found"
        );
        unsafe {
            match old {
                Some(value) => std::env::set_var(env_key, value),
                None => std::env::remove_var(env_key),
            }
            match old_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
        clear_tool_discovery_cache();
    }

    #[test]
    fn unique_temp_dir_fails_when_tmpdir_is_a_file() {
        let real_tmp = std::env::temp_dir();
        let blocker = real_tmp.join(format!("shift-tmpdir-blocker-{}", unique_suffix("tmpdir")));
        std::fs::write(&blocker, b"not-a-dir").unwrap();
        let err = unique_temp_dir_in(&blocker, "shift-unwritable").unwrap_err();
        assert!(
            err.to_string()
                .contains("could not create temporary directory")
                || err.to_string().contains("temporary directory"),
            "error: {err}"
        );
        let _ = std::fs::remove_file(blocker);
    }

    #[cfg(unix)]
    #[test]
    fn unique_temp_dir_is_mode_0700() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir("shift-utd-mode").unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "temp dir mode must be 0700, got {mode:o}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_file_creates_with_mode_0600_before_content() {
        let path = std::env::temp_dir().join(format!("shift-secret-{}", unique_suffix("secret")));
        let _ = std::fs::remove_file(&path);
        write_secret_file(&path, b"s3cret-value").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file mode must be 0600, got {mode:o}");
        assert_eq!(std::fs::read(&path).unwrap(), b"s3cret-value");
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_file_refuses_symlink_targets() {
        use std::os::unix::fs::symlink;

        let suffix = unique_suffix("secret-symlink");
        let target = std::env::temp_dir().join(format!("shift-secret-target-{suffix}"));
        let link = std::env::temp_dir().join(format!("shift-secret-link-{suffix}"));
        std::fs::write(&target, b"keep me").unwrap();
        symlink(&target, &link).unwrap();

        let error = write_secret_file(&link, b"do not redirect").unwrap_err();
        assert!(error.to_string().contains("could not create private file"));
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");

        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn unique_temp_file_name_stays_under_fs_name_max() {
        let long = "x".repeat(1_000);
        for suffix in [".shift-partial", ".shift-bak", ".tmp"] {
            let name = unique_temp_file_name(&long, suffix);
            assert!(
                name.len() <= FS_NAME_MAX,
                "len {} for suffix {suffix}: {name}",
                name.len()
            );
            assert!(name.ends_with(suffix), "{name}");
        }
        // Short stems still produce unique-looking names.
        let a = unique_temp_file_name("out", ".shift-partial");
        let b = unique_temp_file_name("out", ".shift-partial");
        assert_ne!(a, b);
    }

    #[test]
    fn path_operand_helpers_reject_option_like_names() {
        assert!(path_looks_like_option(Path::new("-evil.pdf")));
        assert!(!path_looks_like_option(Path::new("/tmp/-evil.pdf")));
        assert!(!path_looks_like_option(Path::new("good.pdf")));
        assert!(validate_path_operand(Path::new("-evil.pdf")).is_err());
        assert!(validate_path_operand(Path::new("/tmp/-evil.pdf")).is_ok());
        assert!(validate_path_operand(Path::new("good.pdf")).is_ok());

        // Relative option-like names become absolute `/…/-n` and are accepted.
        let mut cmd = Command::new("true");
        let absolute = push_operand_path(&mut cmd, Path::new("-n")).unwrap();
        assert!(absolute.is_absolute(), "{absolute:?}");
        assert!(!path_looks_like_option(&absolute));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.last().map(String::as_str),
            Some(absolute.to_str().unwrap())
        );
        // No bare `--` (BSD cat and similar reject it).
        assert!(!args.iter().any(|a| a == "--"), "{args:?}");
    }

    #[test]
    fn watched_output_path_kills_oversized_on_disk_writer() {
        let suffix = unique_suffix("watch-out");
        let script = std::env::temp_dir().join(format!("shift-process-watch-{suffix}"));
        let out = std::env::temp_dir().join(format!("shift-process-watch-out-{suffix}.bin"));
        let _ = std::fs::remove_file(&out);
        // Grow the output file past the limit, then hang so the wait loop must
        // notice the size (not just process exit).
        write_script(
            &script,
            &format!(
                "#!/bin/sh\ndd if=/dev/zero of='{}' bs=200 count=1 2>/dev/null\nsleep 30\n",
                out.display()
            ),
        );
        let started = Instant::now();
        let error = run_command_cancellable_with_output_paths(
            shell_command(&script),
            Duration::from_secs(20),
            64,
            None,
            std::slice::from_ref(&out),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            error.to_string().contains("too large") || error.to_string().contains("limit"),
            "error: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "on-disk size limit took too long: {elapsed:?}"
        );
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn watched_output_path_checks_fast_exit_after_child_reaps() {
        let suffix = unique_suffix("watch-fast");
        let script = std::env::temp_dir().join(format!("shift-process-watch-fast-{suffix}"));
        let out = std::env::temp_dir().join(format!("shift-process-watch-fast-out-{suffix}.bin"));
        let _ = std::fs::remove_file(&out);
        write_script(
            &script,
            &format!(
                "#!/bin/sh\ndd if=/dev/zero of='{}' bs=200 count=1 2>/dev/null\nexit 0\n",
                out.display()
            ),
        );

        let error = run_command_cancellable_with_output_limits(
            shell_command(&script),
            Duration::from_secs(5),
            64,
            None,
            &[(out.clone(), 128)],
            &[],
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("too large") || error.to_string().contains("limit"),
            "error: {error}"
        );
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn watched_output_directory_kills_oversized_on_disk_writer() {
        let suffix = unique_suffix("watch-dir");
        let script = std::env::temp_dir().join(format!("shift-process-watch-dir-{suffix}"));
        let out_dir = std::env::temp_dir().join(format!("shift-process-watch-dir-out-{suffix}"));
        std::fs::create_dir_all(&out_dir).unwrap();
        write_script(
            &script,
            &format!(
                "#!/bin/sh\ndd if=/dev/zero of='{}/page-1.pdf' bs=200 count=1 2>/dev/null\nsleep 30\n",
                out_dir.display()
            ),
        );
        let started = Instant::now();
        let error = run_command_cancellable_with_output_dirs(
            shell_command(&script),
            Duration::from_secs(20),
            64,
            None,
            &[],
            &[(out_dir.clone(), 64)],
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            error.to_string().contains("too large") || error.to_string().contains("limit"),
            "error: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "on-disk directory size limit took too long: {elapsed:?}"
        );
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn read_file_limited_open_error_after_metadata() {
        // A path that exists as a directory fails File::open after metadata succeeds
        // with is_file size check — actually metadata for dir has size, but we check
        // len first. Use a path that is not readable: on Unix, a file with mode 000.
        let path = std::env::temp_dir().join(format!(
            "shift-process-unreadable-{}",
            unique_suffix("unreadable")
        ));
        std::fs::write(&path, b"secret").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&path, permissions).unwrap();

        // Running as root would still open; skip assertion if open succeeds.
        let result = read_file_limited(&path, 1024);
        // Restore perms for cleanup.
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&path, permissions).unwrap();
        if let Err(error) = result {
            assert!(
                error.to_string().contains("could not read"),
                "error: {error}"
            );
        }
        let _ = std::fs::remove_file(path);
    }
}
