//! Launch Shift with temporary, empty app state for first-run testing.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const USAGE: &str = "Usage: cargo new-user -- [--dry-run]\n\nLaunch Shift with isolated, empty app state. --dry-run creates and removes the temporary state without launching the app.";

fn new_user_state_dir() -> io::Result<PathBuf> {
    let pid = std::process::id();
    for _ in 0..100 {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("shift-new-user-{pid}-{count}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary Shift user-state directory",
    ))
}

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let dry_run = match args.next().as_deref() {
        None => false,
        Some(value) if value == "--dry-run" => true,
        Some(value) if value == "--help" || value == "-h" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(value) => {
            eprintln!("Unknown argument: {}\n\n{USAGE}", value.to_string_lossy());
            return ExitCode::FAILURE;
        }
    };
    if args.next().is_some() {
        eprintln!("Too many arguments\n\n{USAGE}");
        return ExitCode::FAILURE;
    }

    let state_dir = match new_user_state_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Could not create isolated new-user state: {error}");
            return ExitCode::FAILURE;
        }
    };
    let support_dir = state_dir.join("Application Support/Shift");
    let paste_dir = state_dir.join("paste-staging");
    if let Err(error) =
        fs::create_dir_all(&support_dir).and_then(|_| fs::create_dir_all(&paste_dir))
    {
        eprintln!("Could not initialize isolated new-user state: {error}");
        let _ = fs::remove_dir_all(&state_dir);
        return ExitCode::FAILURE;
    }

    println!(
        "Launching Shift with empty app state: {}",
        state_dir.display()
    );
    if dry_run {
        let _ = fs::remove_dir_all(&state_dir);
        return ExitCode::SUCCESS;
    }
    let result = Command::new("cargo")
        .args(["run", "--bin", "shift"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("SHIFT_APP_SUPPORT_DIR", &support_dir)
        .env("SHIFT_PASTE_STAGING_DIR", &paste_dir)
        .status();

    let _ = fs::remove_dir_all(&state_dir);
    match result {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("Could not launch Shift: {error}");
            ExitCode::FAILURE
        }
    }
}
