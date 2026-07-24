//! Bounded external process execution for conversion modules.
//!
//! Every converter should run through this helper so timeouts and output size
//! caps are applied uniformly. Callers still own argument construction and
//! error messaging for their engine.

use super::ConversionError;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// Default wall-clock budget for one converter invocation.
pub const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(300);

/// Default ceiling for captured stdout, stderr, or on-disk converter output.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

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
    mut command: Command,
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<Arc<AtomicBool>>,
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

    let status = match wait_with_timeout(&mut child, timeout, cancel.clone()) {
        WaitOutcome::Exited(status) => status,
        WaitOutcome::TimedOut => {
            // Watchdog already signalled and reaped the process group.
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(ConversionError::new(format!(
                "conversion timed out after {}s",
                timeout.as_secs().max(1)
            )));
        }
        WaitOutcome::Cancelled => {
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(ConversionError::cancelled());
        }
        WaitOutcome::Error(error) => {
            force_kill(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(ConversionError::new(format!(
                "could not wait for converter: {error}"
            )));
        }
    };

    if cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
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
    Error(std::io::Error),
}

/// Poll until the child exits, `timeout` elapses, or `cancel` is set.
///
/// Uses `try_wait` + `child.kill()` so cancel and timeout work on all platforms
/// (Unix also tears down the process group via [`kill_pid`]).
fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
) -> WaitOutcome {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return WaitOutcome::Exited(status),
            Ok(None) => {}
            Err(error) => return WaitOutcome::Error(error),
        }

        if start.elapsed() >= timeout {
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

        thread::sleep(Duration::from_millis(50));
    }
}

fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        // Negative PID kills the whole process group (set up via process_group(0)).
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // Also target the process itself in case process-group setup failed.
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
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
    // Reap so the next try_wait/wait does not race a zombie.
    let _ = child.wait();
}

/// Read a converter-produced file with a hard size ceiling.
pub fn read_file_limited(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ConversionError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        ConversionError::new(format!("could not read {}: {error}", path.display()))
    })?;
    if metadata.len() as usize > max_bytes {
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
    let result = handle
        .join()
        .map_err(|_| ConversionError::new(format!("converter {stream} reader panicked")))?
        .map_err(|error| {
            ConversionError::new(format!("could not read converter {stream}: {error}"))
        })?;
    if result.truncated {
        return Err(ConversionError::new(format!(
            "converter {stream} exceeded the {max_output_bytes} byte limit"
        )));
    }
    Ok(result.bytes)
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
    }
    dirs
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
        .expect("executable discovery cache poisoned")
        .get(name)
    {
        return cached.clone();
    }
    let resolved = find_executable_uncached(name);
    cache
        .lock()
        .expect("executable discovery cache poisoned")
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
    if let Some(cached) = cache.lock().expect("tool path cache poisoned").get(&key) {
        return cached.clone();
    }
    let resolved = resolve_tool_path_uncached(env_override, default_name, local_candidates);
    cache
        .lock()
        .expect("tool path cache poisoned")
        .insert(key, resolved.clone());
    resolved
}

/// Cache key for [`resolve_tool_path`] / [`resolve_tool_executable`].
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

fn resolve_executable_cache() -> &'static Mutex<HashMap<ResolveKey, OsString>> {
    static CACHE: OnceLock<Mutex<HashMap<ResolveKey, OsString>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Process-wide counter for temporary directory names.
///
/// Combined with the process id in [`unique_temp_dir`], this guarantees no two
/// parallel workers collide even if the system clock returns the same nanosecond.
static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a process-unique temporary directory for one conversion step.
///
/// The name includes `prefix`, the process id, and a monotonically increasing
/// counter, then the directory is created with `create_dir_all` so it can be
/// reused safely even if the parent path already exists.
pub fn unique_temp_dir(prefix: &str) -> Result<PathBuf, ConversionError> {
    let counter = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("{prefix}-{}-{counter}", std::process::id()));
    std::fs::create_dir_all(&base).map_err(|error| {
        ConversionError::new(format!(
            "could not create temporary directory {}: {error}",
            base.display()
        ))
    })?;
    Ok(base)
}

/// Clear all memoized tool-discovery results so the next diagnostics/probe pass
/// sees the current filesystem/PATH state.
///
/// `ConversionRegistry` does not need to be rebuilt for newly installed tools
/// to show up in output menus, because it stores bare tool names that are
/// resolved at spawn time. Refreshing diagnostics via this clear re-probes the
/// executable paths and readiness.
pub fn clear_tool_discovery_cache() {
    if let Ok(mut cache) = find_executable_cache().lock() {
        cache.clear();
    }
    if let Ok(mut cache) = resolve_path_cache().lock() {
        cache.clear();
    }
    if let Ok(mut cache) = resolve_executable_cache().lock() {
        cache.clear();
    }
}

fn resolve_tool_path_uncached(
    env_override: &str,
    default_name: &str,
    local_candidates: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os(env_override) {
        if !override_path.is_empty() {
            let path = PathBuf::from(&override_path);
            if is_runnable(&path) {
                return Some(path);
            }
            // Surface configured-but-broken paths so diagnostics can show them.
            if path.exists() {
                return Some(path);
            }
            // Bare name (or relative) in the env override.
            if let Some(found) = find_executable(&override_path) {
                return Some(found);
            }
            return Some(PathBuf::from(override_path));
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
    let key = ResolveKey::capture(env_override, default_name, local_candidates);
    let cache = resolve_executable_cache();
    if let Some(cached) = cache
        .lock()
        .expect("tool executable cache poisoned")
        .get(&key)
    {
        return cached.clone();
    }
    let resolved = resolve_tool_path(env_override, default_name, local_candidates)
        .map(|path| path.into_os_string())
        .unwrap_or_else(|| OsString::from(default_name));
    cache
        .lock()
        .expect("tool executable cache poisoned")
        .insert(key, resolved.clone());
    resolved
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::Mutex;
    use std::time::Instant;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
}
