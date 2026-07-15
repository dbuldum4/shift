//! Bounded external process execution for conversion modules.
//!
//! Every converter should run through this helper so timeouts and output size
//! caps are applied uniformly. Callers still own argument construction and
//! error messaging for their engine.

use super::ConversionError;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
            .args(["-KILL", &format!("-{pid}")])
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
pub fn find_executable(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let name = name.as_ref();
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
) -> std::ffi::OsString {
    resolve_tool_path(env_override, default_name, local_candidates)
        .map(|path| path.into_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from(default_name))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::Instant;

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn captures_successful_output() {
        let path = std::env::temp_dir().join(format!("shift-process-ok-{}", std::process::id()));
        write_script(&path, "#!/bin/sh\nprintf 'hello'\nprintf 'err' >&2\n");
        let output = run_command(Command::new(&path), Duration::from_secs(5), 1024).unwrap();
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
        let error = run_command(Command::new(&path), Duration::from_millis(300), 1024).unwrap_err();
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
        let error = run_command(Command::new(&path), Duration::from_secs(5), 64).unwrap_err();
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
        let error = run_command(Command::new(&path), Duration::from_secs(20), 64).unwrap_err();
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
            Command::new(&path),
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
            Command::new(&path),
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
            Command::new(&path),
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
            Command::new(&path),
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
}
