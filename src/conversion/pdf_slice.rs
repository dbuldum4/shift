//! PDF page-range extraction via `qpdf` for shared preprocess before convert.

use super::{
    ConversionError, max_output_bytes, process_timeout, resolve_tool_executable,
    run_command_cancellable,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Extract inclusive 1-based pages into a temporary PDF.
///
/// `to == None` means "through the last page" (`N-z` in qpdf). Uses
/// `qpdf --empty --pages in.pdf N-M -- out.pdf`. The returned path lives in a
/// unique temp directory; callers should delete the parent directory when done
/// (registry does this via a temp-dir guard).
pub fn extract_pdf_pages(
    input: &Path,
    from: u32,
    to: Option<u32>,
    password: Option<&str>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, ConversionError> {
    validate_page_range(from, to)?;

    if !input.is_file() {
        return Err(ConversionError::new(format!(
            "PDF input is not a readable file: {}",
            input.display()
        )));
    }

    let work_dir = unique_temp_dir("shift-pdf-slice")?;
    let cleanup_on_error = TempDirGuard(work_dir.clone());
    let output = work_dir.join("sliced.pdf");

    let executable = resolve_tool_executable("SHIFT_QPDF_BIN", "qpdf", &[]);
    let range = match to {
        Some(to) => format!("{from}-{to}"),
        None => format!("{from}-z"),
    };
    let mut command = Command::new(&executable);
    command.arg("--empty").arg("--pages").arg(input);
    if let Some(password) = password.map(str::trim).filter(|value| !value.is_empty()) {
        command.arg(format!("--password={password}"));
    }
    command.arg(&range).arg("--").arg(&output);

    let result = run_command_cancellable(command, process_timeout(), max_output_bytes(), cancel)
        .map_err(|error| {
            if error.is_executable_not_found() {
                ConversionError::new(
                    "qpdf is not installed (needed for PDF page ranges). \
                 Install it with `brew install qpdf`, or set SHIFT_QPDF_BIN.",
                )
            } else {
                error
            }
        })?;

    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let detail = if detail.is_empty() {
            let stdout = String::from_utf8_lossy(&result.stdout).trim().to_owned();
            if stdout.is_empty() {
                format!("process exited with {}", result.status)
            } else {
                stdout
            }
        } else {
            detail
        };
        let range_label = match to {
            Some(to) => format!("{from}-{to}"),
            None => format!("{from}-end"),
        };
        return Err(ConversionError::new(format!(
            "qpdf could not extract pages {range_label} from {}: {detail}",
            input.display()
        )));
    }

    if !output.is_file() {
        return Err(ConversionError::new(format!(
            "qpdf finished but did not write {}",
            output.display()
        )));
    }

    // Success: leak the dir to the caller (do not delete on drop).
    std::mem::forget(cleanup_on_error);
    Ok(output)
}

fn validate_page_range(from: u32, to: Option<u32>) -> Result<(), ConversionError> {
    if from == 0 {
        return Err(ConversionError::new(
            "PDF page range start must be >= 1 (pages are 1-based)",
        ));
    }
    if let Some(to) = to {
        if to == 0 {
            return Err(ConversionError::new(
                "PDF page range end must be >= 1 (pages are 1-based)",
            ));
        }
        if from > to {
            return Err(ConversionError::new(format!(
                "PDF page range start ({from}) must be <= end ({to})"
            )));
        }
    }
    Ok(())
}

fn unique_temp_dir(prefix: &str) -> Result<PathBuf, ConversionError> {
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir(&path).map_err(|error| {
        ConversionError::new(format!(
            "could not create temporary PDF slice directory {}: {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_based_or_inverted_ranges() {
        assert!(validate_page_range(0, Some(1)).is_err());
        assert!(validate_page_range(1, Some(0)).is_err());
        assert!(validate_page_range(5, Some(3)).is_err());
        assert!(validate_page_range(1, Some(1)).is_ok());
        assert!(validate_page_range(2, Some(10)).is_ok());
        assert!(validate_page_range(3, None).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn extract_uses_qpdf_argv_shape() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;

        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let fake = directory.join(format!("shift-qpdf-test-{suffix}"));
        let input = directory.join(format!("shift-qpdf-input-{suffix}.pdf"));
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\n# last arg is output path\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%%PDF-1.4 sliced' > \"$out\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        // SAFETY: serialized behind ENV_LOCK.
        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let sliced = extract_pdf_pages(&input, 2, Some(4), Some("s3cret"), None).unwrap();
        assert!(sliced.is_file());
        assert_eq!(std::fs::read(&sliced).unwrap(), b"%PDF-1.4 sliced");

        let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
        assert!(args.contains("--empty"), "args: {args}");
        assert!(args.contains("--pages"), "args: {args}");
        assert!(args.contains("2-4"), "args: {args}");
        assert!(args.contains("--password=s3cret"), "args: {args}");

        if let Some(parent) = sliced.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(format!("{}.args", fake.display()));
        let _ = std::fs::remove_file(&input);
    }
}
