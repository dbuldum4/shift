//! Bounded external process execution for conversion modules.
//!
//! Every converter should run through this helper so timeouts and output size
//! caps are applied uniformly. Callers still own argument construction and
//! error messaging for their engine.

use super::ConversionError;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
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
    mut command: Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<LimitedOutput, ConversionError> {
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

    let status = match wait_with_timeout(&mut child, timeout) {
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
        WaitOutcome::Error(error) => {
            force_kill(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(ConversionError::new(format!(
                "could not wait for converter: {error}"
            )));
        }
    };

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
    Error(std::io::Error),
}

/// Block until the child exits or `timeout` elapses.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> WaitOutcome {
    let timed_out = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let pid = child.id();
    let flag = Arc::clone(&timed_out);

    let watchdog = thread::spawn(move || {
        // `recv_timeout` errors on timeout or if the sender is dropped.
        if rx.recv_timeout(timeout).is_err() {
            flag.store(true, Ordering::SeqCst);
            kill_pid(pid);
        }
    });

    let wait_result = child.wait();
    // Cancel the watchdog if we finished first.
    let _ = tx.send(());
    let _ = watchdog.join();

    match wait_result {
        Ok(status) if timed_out.load(Ordering::SeqCst) => {
            let _ = status;
            WaitOutcome::TimedOut
        }
        Ok(status) => WaitOutcome::Exited(status),
        Err(error) => WaitOutcome::Error(error),
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
        let _ = pid;
    }
}

fn force_kill(child: &mut Child) {
    kill_pid(child.id());
    let _ = child.kill();
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
}
