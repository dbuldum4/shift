//! PDF page-range extraction via `qpdf` for shared preprocess before convert.

use super::{
    ConversionError, max_output_bytes, process_timeout, resolve_tool_executable,
    run_command_cancellable_with_output_paths, unique_temp_dir,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Extract inclusive 1-based pages into a temporary PDF, optionally decrypting
/// it first with a password read from a restrictive temporary file.
///
/// `to == None` means "through the last page" (`N-z` in qpdf). Uses
/// `qpdf in.pdf --password-file=... --pages . N-M -- out.pdf` so the password
/// file applies to the primary input. The returned path lives in a unique temp
/// directory; callers should delete the parent directory when done (registry
/// does this via a temp-dir guard).
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
    // Match the toolkit module: warnings-only (default exit 3) still write output.
    command.arg("--warning-exit-0");
    super::push_path_arg(&mut command, input)?;
    if let Some(password) = password.map(str::trim).filter(|value| !value.is_empty()) {
        let password_file = work_dir.join("password.txt");
        // Mode 0600 applied at open — never write secrets under the default umask.
        super::write_secret_file(&password_file, password.as_bytes()).map_err(|error| {
            ConversionError::new(format!("could not write PDF password file: {error}"))
        })?;
        let absolute = super::absolute_command_path(&password_file);
        command
            .arg(format!("--password-file={}", absolute.display()))
            .arg("--decrypt");
    }
    command.arg("--pages").arg(".").arg(&range).arg("--");
    super::push_path_arg(&mut command, &output)?;

    let result = run_command_cancellable_with_output_paths(
        command,
        process_timeout(),
        max_output_bytes(),
        cancel,
        std::slice::from_ref(&output),
    )
    .map_err(|error| {
        if error.is_executable_not_found() {
            ConversionError::new(
                "qpdf is not installed (needed for PDF page ranges and password-protected PDFs). \
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

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env mutations are serialized via crate::ENV_LOCK (shared across shift_core).

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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

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

        // SAFETY: serialized behind crate::ENV_LOCK.
        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let sliced = extract_pdf_pages(&input, 2, Some(4), Some("s3cret"), None).unwrap();
        assert!(sliced.is_file());
        assert_eq!(std::fs::read(&sliced).unwrap(), b"%PDF-1.4 sliced");

        let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
        assert!(args.contains("--pages"), "args: {args}");
        assert!(
            args.contains("--pages . 2-4"),
            "args should use '.' for primary input, args: {args}"
        );
        assert!(args.contains("2-4"), "args: {args}");
        assert!(
            args.contains("--password-file="),
            "password must be passed through a file, args: {args}"
        );
        assert!(
            args.contains("--decrypt"),
            "encrypted input must be decrypted, args: {args}"
        );
        assert!(
            !args.contains("--password=s3cret"),
            "password must not appear on the command line, args: {args}"
        );

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

    #[test]
    fn rejects_missing_input_file() {
        let missing =
            std::env::temp_dir().join(format!("shift-qpdf-missing-{}.pdf", std::process::id()));
        assert!(extract_pdf_pages(&missing, 1, Some(2), None, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn password_only_decrypt_uses_full_range_and_password_file() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let fake = directory.join(format!("shift-qpdf-pwd-{suffix}"));
        let input = directory.join(format!("shift-qpdf-pwd-input-{suffix}.pdf"));
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\n# last arg is output path\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%%PDF-1.4 sliced' > \"$out\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        // SAFETY: serialized behind crate::ENV_LOCK.
        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let sliced = extract_pdf_pages(&input, 1, None, Some("s3cret"), None).unwrap();
        assert!(sliced.is_file());

        let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
        assert!(args.contains("1-z"), "full range expected, args: {args}");
        assert!(
            args.contains("--pages . 1-z"),
            "args should use '.' for primary input, args: {args}"
        );
        assert!(
            args.contains("--password-file="),
            "password must be passed through a file, args: {args}"
        );
        assert!(
            args.contains("--decrypt"),
            "encrypted input must be decrypted, args: {args}"
        );
        assert!(
            !args.contains("--password=s3cret"),
            "password must not appear on argv, args: {args}"
        );

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

    #[cfg(unix)]
    #[test]
    fn fails_when_qpdf_returns_nonzero() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let fake = directory.join(format!("shift-qpdf-fail-{suffix}"));
        let input = directory.join(format!("shift-qpdf-fail-input-{suffix}.pdf"));
        std::fs::write(&fake, "#!/bin/sh\necho 'boom' >&2\nexit 1\n").unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let result = extract_pdf_pages(&input, 2, Some(4), None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("qpdf could not extract")
        );

        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(&input);
    }

    fn unique_suffix(tag: &str) -> String {
        format!(
            "{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn validate_page_range_all_error_messages() {
        let err = validate_page_range(0, Some(1)).unwrap_err();
        assert!(
            err.to_string().contains("start must be >= 1"),
            "error: {err}"
        );
        let err = validate_page_range(0, None).unwrap_err();
        assert!(
            err.to_string().contains("start must be >= 1"),
            "error: {err}"
        );
        let err = validate_page_range(1, Some(0)).unwrap_err();
        assert!(err.to_string().contains("end must be >= 1"), "error: {err}");
        let err = validate_page_range(10, Some(2)).unwrap_err();
        assert!(err.to_string().contains("must be <= end"), "error: {err}");
        assert!(err.to_string().contains("10"));
        assert!(err.to_string().contains("2"));

        // Boundary successes.
        assert!(validate_page_range(1, Some(1)).is_ok());
        assert!(validate_page_range(1, None).is_ok());
        assert!(validate_page_range(u32::MAX, None).is_ok());
        assert!(validate_page_range(u32::MAX, Some(u32::MAX)).is_ok());
        assert!(validate_page_range(u32::MAX - 1, Some(u32::MAX)).is_ok());
    }

    #[test]
    fn rejects_directory_as_pdf_input() {
        let directory =
            std::env::temp_dir().join(format!("shift-qpdf-dir-input-{}", unique_suffix("dir")));
        std::fs::create_dir_all(&directory).unwrap();
        let err = extract_pdf_pages(&directory, 1, Some(1), None, None).unwrap_err();
        assert!(
            err.to_string().contains("not a readable file"),
            "error: {err}"
        );
        let _ = std::fs::remove_dir(&directory);
    }

    #[test]
    fn rejects_missing_input_before_qpdf_spawn() {
        // Validation order: page range first, then file check — missing file
        // still yields a clear error without requiring qpdf.
        let missing = std::env::temp_dir().join(format!(
            "shift-qpdf-missing-early-{}.pdf",
            unique_suffix("miss")
        ));
        let err = extract_pdf_pages(&missing, 1, Some(3), None, None).unwrap_err();
        assert!(
            err.to_string().contains("not a readable file"),
            "error: {err}"
        );
        // Invalid range fails even if path is missing.
        let err = extract_pdf_pages(&missing, 0, Some(1), None, None).unwrap_err();
        assert!(
            err.to_string().contains("start must be >= 1"),
            "error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_qpdf_executable_reports_install_hint() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("no-qpdf");
        let missing = directory.join(format!("shift-qpdf-absent-{suffix}"));
        let input = directory.join(format!("shift-qpdf-absent-in-{suffix}.pdf"));
        let _ = std::fs::remove_file(&missing);
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &missing);
        }

        let err = extract_pdf_pages(&input, 1, Some(1), None, None).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("qpdf is not installed") || message.contains("executable not found"),
            "{message}"
        );

        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    fn write_fake_qpdf(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn page_range_without_password_omits_password_file() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("no-pwd");
        let fake = directory.join(format!("shift-qpdf-nopwd-{suffix}"));
        let input = directory.join(format!("shift-qpdf-nopwd-in-{suffix}.pdf"));
        write_fake_qpdf(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%%PDF-1.4 sliced' > \"$out\"\n",
        );
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let sliced = extract_pdf_pages(&input, 3, Some(5), None, None).unwrap();
        let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
        assert!(args.contains("3-5"), "args: {args}");
        assert!(args.contains("--pages . 3-5"), "args: {args}");
        assert!(
            !args.contains("--password-file="),
            "no password should omit password file, args: {args}"
        );
        assert!(!args.contains("--password="), "args: {args}");

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

    #[cfg(unix)]
    #[test]
    fn password_and_bounded_range_write_restricted_password_file() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("pwd-range");
        let fake = directory.join(format!("shift-qpdf-pwdrange-{suffix}"));
        let input = directory.join(format!("shift-qpdf-pwdrange-in-{suffix}.pdf"));
        // Capture password file path and contents via the argv + side-channel dump.
        write_fake_qpdf(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nfor a in \"$@\"; do\n  case \"$a\" in\n    --password-file=*)\n      pf=\"${a#--password-file=}\"\n      cp \"$pf\" \"${0}.pwd\"\n      ;;\n  esac\ndone\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%%PDF-1.4 sliced' > \"$out\"\n",
        );
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let password = "p@ss w0rd!";
        let sliced = extract_pdf_pages(&input, 1, Some(2), Some(password), None).unwrap();
        let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
        assert!(args.contains("1-2"), "args: {args}");
        assert!(args.contains("--password-file="), "args: {args}");
        assert!(
            !args.contains(password),
            "password must not appear on argv: {args}"
        );

        let pwd_dump = std::fs::read_to_string(format!("{}.pwd", fake.display())).unwrap();
        assert_eq!(pwd_dump, password);

        if let Some(parent) = sliced.parent() {
            // Password file lives next to sliced.pdf; callers delete the whole work dir.
            let password_path = parent.join("password.txt");
            assert!(
                password_path.is_file(),
                "password.txt should remain in the work dir until the caller cleans it up"
            );
            // Assert mode 0600 via metadata — do not parse `ls` (can be empty/fail open).
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&password_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "password file mode must be 0600, got {mode:o}");
            let _ = std::fs::remove_dir_all(parent);
        }
        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(format!("{}.args", fake.display()));
        let _ = std::fs::remove_file(format!("{}.pwd", fake.display()));
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn empty_and_whitespace_password_treated_as_absent() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("empty-pwd");
        let fake = directory.join(format!("shift-qpdf-emptypwd-{suffix}"));
        let input = directory.join(format!("shift-qpdf-emptypwd-in-{suffix}.pdf"));
        write_fake_qpdf(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%%PDF-1.4 sliced' > \"$out\"\n",
        );
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        for password in [Some(""), Some("   "), Some("\t")] {
            let sliced = extract_pdf_pages(&input, 1, Some(1), password, None).unwrap();
            let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
            assert!(
                !args.contains("--password-file="),
                "whitespace/empty password must not pass file, args: {args}"
            );
            if let Some(parent) = sliced.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }

        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(format!("{}.args", fake.display()));
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn open_ended_range_uses_z_suffix() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("open-end");
        let fake = directory.join(format!("shift-qpdf-open-{suffix}"));
        let input = directory.join(format!("shift-qpdf-open-in-{suffix}.pdf"));
        write_fake_qpdf(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%%PDF-1.4 sliced' > \"$out\"\n",
        );
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let sliced = extract_pdf_pages(&input, 7, None, None, None).unwrap();
        let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
        assert!(args.contains("7-z"), "args: {args}");
        assert!(args.contains("--pages . 7-z"), "args: {args}");
        assert!(!args.contains("--password-file="), "args: {args}");

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

    #[cfg(unix)]
    #[test]
    fn qpdf_failure_includes_range_label_for_open_ended() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("fail-open");
        let fake = directory.join(format!("shift-qpdf-failopen-{suffix}"));
        let input = directory.join(format!("shift-qpdf-failopen-in-{suffix}.pdf"));
        write_fake_qpdf(&fake, "#!/bin/sh\necho 'nope' >&2\nexit 1\n");
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let err = extract_pdf_pages(&input, 4, None, None, None).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("qpdf could not extract"), "{message}");
        assert!(
            message.contains("4-end"),
            "open-ended failure label should use N-end: {message}"
        );

        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn qpdf_success_without_output_file_errors() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("no-out");
        let fake = directory.join(format!("shift-qpdf-noout-{suffix}"));
        let input = directory.join(format!("shift-qpdf-noout-in-{suffix}.pdf"));
        // Exit 0 but never write the output path.
        write_fake_qpdf(&fake, "#!/bin/sh\nexit 0\n");
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let err = extract_pdf_pages(&input, 1, Some(1), None, None).unwrap_err();
        assert!(err.to_string().contains("did not write"), "error: {err}");

        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn cancel_before_qpdf_returns_cancelled() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("cancel");
        let fake = directory.join(format!("shift-qpdf-cancel-{suffix}"));
        let input = directory.join(format!("shift-qpdf-cancel-in-{suffix}.pdf"));
        write_fake_qpdf(&fake, "#!/bin/sh\nsleep 30\n");
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let cancel = Arc::new(AtomicBool::new(true));
        let err = extract_pdf_pages(&input, 1, Some(1), None, Some(cancel)).unwrap_err();
        assert!(err.is_cancelled(), "error: {err}");

        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn non_pdf_extension_still_invokes_qpdf_when_file_exists() {
        // extract_pdf_pages does not validate magic/extension — only is_file.
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("nonpdf");
        let fake = directory.join(format!("shift-qpdf-nonpdf-{suffix}"));
        let input = directory.join(format!("shift-qpdf-nonpdf-in-{suffix}.txt"));
        write_fake_qpdf(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%%PDF-1.4 sliced' > \"$out\"\n",
        );
        std::fs::write(&input, b"not really a pdf").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let sliced = extract_pdf_pages(&input, 1, Some(1), None, None).unwrap();
        assert!(sliced.is_file());
        let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
        assert!(
            args.contains(input.file_name().unwrap().to_string_lossy().as_ref()),
            "non-pdf path should still be passed through, args: {args}"
        );

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

    #[cfg(unix)]
    #[test]
    fn single_page_range_argv_shape() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("single");
        let fake = directory.join(format!("shift-qpdf-single-{suffix}"));
        let input = directory.join(format!("shift-qpdf-single-in-{suffix}.pdf"));
        write_fake_qpdf(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%%PDF-1.4 sliced' > \"$out\"\n",
        );
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake);
        }

        let sliced = extract_pdf_pages(&input, 9, Some(9), None, None).unwrap();
        let args = std::fs::read_to_string(format!("{}.args", fake.display())).unwrap();
        assert!(args.contains("9-9"), "args: {args}");
        assert!(args.contains("--pages"), "args: {args}");
        assert!(args.contains("--"), "args: {args}");

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

    #[cfg(unix)]
    #[test]
    fn qpdf_failure_prefers_stderr_stdout_then_status() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir();
        let suffix = unique_suffix("fail-detail");
        let input = directory.join(format!("shift-qpdf-fail-in-{suffix}.pdf"));
        std::fs::write(&input, b"%PDF-1.4 source").unwrap();

        // stderr non-empty
        let fake_err = directory.join(format!("shift-qpdf-stderr-{suffix}"));
        write_fake_qpdf(&fake_err, "#!/bin/sh\necho 'qpdf-stderr' >&2\nexit 1\n");
        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake_err);
        }
        let err = extract_pdf_pages(&input, 1, Some(1), None, None).unwrap_err();
        assert!(err.to_string().contains("qpdf-stderr"), "error: {err}");

        // empty stderr, non-empty stdout
        let fake_out = directory.join(format!("shift-qpdf-stdout-{suffix}"));
        write_fake_qpdf(&fake_out, "#!/bin/sh\necho 'qpdf-stdout'\nexit 1\n");
        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake_out);
        }
        let err = extract_pdf_pages(&input, 1, Some(1), None, None).unwrap_err();
        assert!(err.to_string().contains("qpdf-stdout"), "error: {err}");

        // both empty → process exited with status
        let fake_empty = directory.join(format!("shift-qpdf-empty-{suffix}"));
        write_fake_qpdf(&fake_empty, "#!/bin/sh\nexit 1\n");
        unsafe {
            std::env::set_var("SHIFT_QPDF_BIN", &fake_empty);
        }
        let err = extract_pdf_pages(&input, 1, Some(1), None, None).unwrap_err();
        assert!(
            err.to_string().contains("process exited") || err.to_string().contains("qpdf"),
            "error: {err}"
        );

        unsafe {
            std::env::remove_var("SHIFT_QPDF_BIN");
        }
        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&fake_err);
        let _ = std::fs::remove_file(&fake_out);
        let _ = std::fs::remove_file(&fake_empty);
    }
}
