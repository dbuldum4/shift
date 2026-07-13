use shift_core::conversion::{
    BatchEnqueueOptions, BatchEvent, BatchQueue, BatchSource, ConversionOptions,
    ConversionRegistry, DiagnosticsReport, FfmpegEncodeMode, FfmpegOptions, FfmpegQuality,
    OutputFormat, default_output_path, looks_like_url, paths_refer_to_same_file, run_batch,
};
use shift_core::preferences::load_module_priority;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("shift-cli: {error}");
            ExitCode::from(1)
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<ExitCode, String> {
    if arguments.is_empty() {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    if arguments
        .first()
        .is_some_and(|value| matches!(value.to_string_lossy().as_ref(), "-h" | "--help" | "help"))
        && arguments.len() == 1
    {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    if arguments.first().is_some_and(|value| value == "formats") && arguments.len() == 1 {
        print_formats();
        return Ok(ExitCode::SUCCESS);
    }

    if arguments
        .first()
        .is_some_and(|value| value == "doctor" || value == "--doctor")
    {
        return run_doctor(&arguments[1..]);
    }

    let mut cursor = 0;
    let mut batch_explicit = false;
    if arguments.first().is_some_and(|value| value == "convert") {
        cursor += 1;
    } else if arguments.first().is_some_and(|value| value == "batch") {
        cursor += 1;
        batch_explicit = true;
    }

    let mut inputs: Vec<OsString> = Vec::new();
    let mut output = None;
    let mut output_dir = None;
    let mut stdout = false;
    let mut force = false;
    let mut target = OutputFormat::MARKDOWN;
    let mut preferred_module: Option<String> = None;
    let mut ffmpeg = FfmpegOptions::default();

    while cursor < arguments.len() {
        let arg = arguments[cursor].to_string_lossy();
        match arg.as_ref() {
            "-o" | "--output" => {
                cursor += 1;
                output = Some(
                    arguments
                        .get(cursor)
                        .map(PathBuf::from)
                        .ok_or_else(|| "--output requires a path".to_owned())?,
                );
            }
            "-O" | "--output-dir" => {
                cursor += 1;
                output_dir = Some(
                    arguments
                        .get(cursor)
                        .map(PathBuf::from)
                        .ok_or_else(|| "--output-dir requires a directory".to_owned())?,
                );
            }
            "--stdout" => stdout = true,
            "--force" => force = true,
            "-t" | "--to" => {
                cursor += 1;
                target = arguments
                    .get(cursor)
                    .ok_or_else(|| "--to requires a format".to_owned())?
                    .to_string_lossy()
                    .parse::<OutputFormat>()
                    .map_err(|error| error.to_string())?;
            }
            "--module" => {
                cursor += 1;
                preferred_module = Some(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--module requires an id".to_owned())?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--start" => {
                cursor += 1;
                ffmpeg.start_secs = Some(parse_secs(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--start requires seconds".to_owned())?,
                    "--start",
                )?);
            }
            "--duration" => {
                cursor += 1;
                ffmpeg.duration_secs = Some(parse_secs(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--duration requires seconds".to_owned())?,
                    "--duration",
                )?);
            }
            "--frame" => {
                cursor += 1;
                ffmpeg.frame_secs = Some(parse_secs(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--frame requires seconds".to_owned())?,
                    "--frame",
                )?);
            }
            "--audio-stream" => {
                cursor += 1;
                ffmpeg.audio_stream = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--audio-stream requires an index".to_owned())?,
                    "--audio-stream",
                )?);
            }
            "--subtitle-stream" => {
                cursor += 1;
                ffmpeg.subtitle_stream = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--subtitle-stream requires an index".to_owned())?,
                    "--subtitle-stream",
                )?);
            }
            "--encode" => {
                cursor += 1;
                ffmpeg.encode_mode = arguments
                    .get(cursor)
                    .ok_or_else(|| "--encode requires auto|copy|reencode".to_owned())?
                    .to_string_lossy()
                    .parse::<FfmpegEncodeMode>()
                    .map_err(|error| error.to_string())?;
            }
            "--quality" => {
                cursor += 1;
                ffmpeg.quality = arguments
                    .get(cursor)
                    .ok_or_else(|| "--quality requires balanced|high|small".to_owned())?
                    .to_string_lossy()
                    .parse::<FfmpegQuality>()
                    .map_err(|error| error.to_string())?;
            }
            "--mono" => ffmpeg.mono = true,
            "--sample-rate" => {
                cursor += 1;
                ffmpeg.sample_rate_hz = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--sample-rate requires Hz".to_owned())?,
                    "--sample-rate",
                )?);
            }
            "--scale-width" => {
                cursor += 1;
                ffmpeg.scale_width = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--scale-width requires pixels".to_owned())?,
                    "--scale-width",
                )?);
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown argument: {value}"));
            }
            _ => {
                inputs.push(arguments[cursor].clone());
            }
        }
        cursor += 1;
    }

    if inputs.is_empty() {
        return Err("missing input file or URL (try `shift-cli --help`)".to_owned());
    }

    if stdout && (output.is_some() || output_dir.is_some()) {
        return Err("use either --stdout or an output path, not both".to_owned());
    }

    if output.is_some() && output_dir.is_some() {
        return Err("use either --output or --output-dir, not both".to_owned());
    }

    let use_batch = batch_explicit || inputs.len() > 1 || output_dir.is_some();
    if use_batch && stdout {
        return Err("batch conversion cannot write to --stdout (use -O/--output-dir)".to_owned());
    }
    if use_batch && output.is_some() && inputs.len() > 1 {
        return Err(
            "batch conversion with multiple inputs requires -O/--output-dir, not -o/--output"
                .to_owned(),
        );
    }

    let registry = build_registry(preferred_module.as_deref())?;
    let options = ConversionOptions {
        ffmpeg,
        cancel: None,
    };

    if use_batch {
        return run_batch_cli(
            inputs, target, options, output_dir, output, force, &registry,
        );
    }

    // Single-file / single-URL path (in-memory convert, then write or stdout).
    let input = &inputs[0];
    let input_url = url_input(input);
    let artifact = if let Some(url) = input_url {
        registry
            .convert_url_with_options(url, target, &options)
            .map_err(|error| error.to_string())?
    } else {
        registry
            .convert_to_with_options(PathBuf::from(input), target, &options)
            .map_err(|error| error.to_string())?
    };

    if stdout {
        use std::io::Write;
        std::io::stdout()
            .write_all(&artifact.bytes)
            .map_err(|error| format!("could not write output: {error}"))?;
    } else {
        let destination = output.unwrap_or_else(|| {
            if input_url.is_some() {
                PathBuf::from(&artifact.file_name)
            } else {
                default_output_path(PathBuf::from(input).as_path(), target)
            }
        });
        let source_path = input_url.is_none().then(|| PathBuf::from(input));
        prepare_destination(&destination, source_path.as_deref(), force)?;
        artifact
            .write_to(&destination)
            .map_err(|error| error.to_string())?;
        println!("{}", destination.display());
    }

    Ok(ExitCode::SUCCESS)
}

fn build_registry(preferred_module: Option<&str>) -> Result<ConversionRegistry, String> {
    let registry = ConversionRegistry::default();
    if let Some(module) = preferred_module {
        if !registry.has_module(module) {
            let known = registry
                .modules()
                .map(|module| module.id())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "unknown module `{module}` (known: {known}). Try `shift-cli formats`."
            ));
        }
        Ok(registry.with_priority(&[module]))
    } else {
        Ok(registry.with_priority(&load_module_priority()))
    }
}

fn run_batch_cli(
    inputs: Vec<OsString>,
    target: OutputFormat,
    options: ConversionOptions,
    output_dir: Option<PathBuf>,
    single_output: Option<PathBuf>,
    force: bool,
    registry: &ConversionRegistry,
) -> Result<ExitCode, String> {
    let mut queue = BatchQueue::new();
    let mut enqueue = BatchEnqueueOptions::new(target);
    enqueue.conversion = options;
    enqueue.force = force;
    enqueue.output_dir = output_dir;

    for input in &inputs {
        let source = BatchSource::from_path_or_url(PathBuf::from(input));
        queue.enqueue(source, &enqueue);
    }

    // Single input + -o path: pin the destination for that one item.
    if let Some(path) = single_output {
        if queue.len() == 1 {
            queue.items_mut()[0].destination = path;
        }
    }

    // Process-wide cancel flag installed once; each batch call resets it.
    let cancel = install_ctrl_c_handler();

    let summary = run_batch(&mut queue, registry, &cancel, |event| match event {
        BatchEvent::ItemStarted {
            source_name,
            destination,
            ..
        } => {
            eprintln!("… {source_name} → {}", destination.display());
        }
        BatchEvent::ItemSucceeded { path, .. } => {
            println!("{}", path.display());
        }
        BatchEvent::ItemFailed {
            source_name, error, ..
        } => {
            eprintln!("shift-cli: failed {source_name}: {error}");
        }
        BatchEvent::ItemCancelled { source_name, .. } => {
            eprintln!("shift-cli: cancelled {source_name}");
        }
        BatchEvent::Progress(progress) => {
            if progress.total > 1 {
                eprint!(
                    "\r[{}/{}] {} ok · {} failed · {} cancelled",
                    progress.completed(),
                    progress.total,
                    progress.succeeded,
                    progress.failed,
                    progress.cancelled
                );
                if progress.is_idle() {
                    eprintln!();
                }
            }
        }
    });

    if queue.len() > 1 {
        eprintln!(
            "batch complete: {} succeeded, {} failed, {} cancelled",
            summary.succeeded, summary.failed, summary.cancelled
        );
    }

    Ok(ExitCode::from(summary.exit_code()))
}

/// Install a process-wide SIGINT handler (once) and return the shared cancel flag.
///
/// The flag is reset to `false` on every call so sequential batch runs in one
/// process each start uncancelled. Concurrent multi-batch is not supported.
///
/// Lifetime: the `Arc` lives for the process; only one handler is registered.
fn install_ctrl_c_handler() -> Arc<AtomicBool> {
    #[cfg(unix)]
    {
        use std::sync::Once;
        use std::sync::OnceLock;
        static CANCEL: OnceLock<Arc<AtomicBool>> = OnceLock::new();
        static INSTALL: Once = Once::new();
        let cancel = CANCEL
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone();
        cancel.store(false, Ordering::SeqCst);
        INSTALL.call_once(|| {
            unsafe extern "C" {
                fn signal(sig: i32, handler: Option<extern "C" fn(i32)>) -> usize;
            }
            extern "C" fn handle_sigint(_: i32) {
                if let Some(flag) = CANCEL.get() {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            const SIGINT: i32 = 2;
            // SAFETY: SIGINT handler only stores to an AtomicBool; async-signal-safe.
            unsafe {
                let _ = signal(SIGINT, Some(handle_sigint));
            }
        });
        cancel
    }
    #[cfg(not(unix))]
    {
        Arc::new(AtomicBool::new(false))
    }
}

/// Probe external engines. Exit codes are stable for scripts:
///
/// - `0` — at least one conversion engine is ready (usable partial install)
/// - `1` — no conversion engines are ready (or doctor flags were invalid)
///
/// Optional engines and PDF backends do not fail the exit code. Use
/// `--script` and check `complete=true` or individual `engine.*=ready` lines
/// when a full install is required.
///
/// Pass `--script` for `key=value` lines, or `--quiet` for exit code only.
fn run_doctor(arguments: &[OsString]) -> Result<ExitCode, String> {
    let mut script = false;
    let mut quiet = false;
    for argument in arguments {
        match argument.to_string_lossy().as_ref() {
            "--script" | "-s" => script = true,
            "--quiet" | "-q" => quiet = true,
            "-h" | "--help" => {
                print_doctor_help();
                return Ok(ExitCode::SUCCESS);
            }
            unknown => {
                return Err(format!(
                    "unknown doctor argument: {unknown} (try `shift-cli doctor --help`)"
                ));
            }
        }
    }

    let report = DiagnosticsReport::collect();
    if !quiet {
        if script {
            print!("{}", report.render_script());
        } else {
            print!("{}", report.render_text());
        }
    }

    Ok(ExitCode::from(report.exit_code() as u8))
}

fn print_doctor_help() {
    println!(
        "Usage: shift-cli doctor [--script] [--quiet]\n\n\
         Probe MarkItDown, Pandoc, Defuddle, Docling, FFmpeg, and PDF engines.\n\
         Reports installed/missing status, detected versions, and install hints.\n\n\
         Options:\n  \
         --script, -s   Emit key=value lines for scripts\n  \
         --quiet,  -q   Suppress output; rely on the exit code only\n\n\
         Exit codes:\n  \
         0  at least one conversion engine is ready\n  \
         1  no conversion engines are ready\n\n\
         Optional engines (Defuddle, Docling) and PDF backends do not fail the\n\
         exit code. For a full install gate, use `--script` and require\n\
         `complete=true` or specific `engine.<id>=ready` lines.\n\n\
         Registered capability (what modules advertise) is listed by\n\
         `shift-cli formats`. Doctor reports whether conversion is currently\n\
         available on this machine."
    );
}

/// Refuse source overwrite always; require `--force` for other existing files.
fn prepare_destination(
    destination: &Path,
    source: Option<&Path>,
    force: bool,
) -> Result<(), String> {
    if let Some(source) = source {
        if paths_refer_to_same_file(source, destination) {
            return Err(format!(
                "refusing to overwrite source file {} (choose a different -o path)",
                source.display()
            ));
        }
    }

    if destination.exists() && !force {
        return Err(format!(
            "output already exists: {} (pass --force to overwrite)",
            destination.display()
        ));
    }

    Ok(())
}

fn url_input(input: &OsStr) -> Option<&str> {
    input.to_str().filter(|value| looks_like_url(value))
}

fn print_formats() {
    for module in ConversionRegistry::default().modules() {
        let outputs = module
            .output_formats()
            .iter()
            .map(|format| format.id())
            .collect::<Vec<_>>()
            .join(", ");
        let inputs = if module.supports_url(OutputFormat::MARKDOWN)
            || module.supports_url(OutputFormat::HTML)
        {
            let mut parts = module.input_extensions().to_vec();
            parts.push("url");
            parts.join(", ")
        } else {
            module.input_extensions().join(", ")
        };
        println!(
            "{} ({}): {inputs} -> {outputs}",
            module.label(),
            module.id(),
        );
    }
}

fn parse_secs(value: &OsStr, flag: &str) -> Result<f64, String> {
    value
        .to_str()
        .ok_or_else(|| format!("{flag} value is not valid UTF-8"))?
        .parse::<f64>()
        .map_err(|_| format!("{flag} expects a number of seconds"))
}

fn parse_u32(value: &OsStr, flag: &str) -> Result<u32, String> {
    value
        .to_str()
        .ok_or_else(|| format!("{flag} value is not valid UTF-8"))?
        .parse::<u32>()
        .map_err(|_| format!("{flag} expects a non-negative integer"))
}

fn print_help() {
    println!(
        "Shift converts files and URLs through the same modules as the native app.\n\n\
         Usage:\n  shift-cli <INPUT|URL> [-t <FORMAT>] [-o <OUTPUT>] [--stdout] [--force] [--module <ID>]\n  \
         shift-cli convert <INPUT|URL> …\n  \
         shift-cli batch <INPUT|URL>… [-t <FORMAT>] [-O <DIR>] [--force]\n  \
         shift-cli <INPUT>… -O <DIR> [-t <FORMAT>]   # multi-file batch (shared queue)\n  \
         shift-cli formats\n  \
         shift-cli doctor [--script] [--quiet]\n\n\
         Batch options:\n  \
         -O, --output-dir <DIR>  Write every output into DIR (creates if needed)\n  \
         --force                 Overwrite existing outputs\n  \
         Ctrl-C                  Cancel the active item and remaining queue\n\n\
         Media (FFmpeg) options:\n  \
         --start <SEC>           Seek to timestamp before converting\n  \
         --duration <SEC>        Limit output length\n  \
         --frame <SEC>           Still-image frame time (png/jpg/webp)\n  \
         --audio-stream <N>      Audio stream index (0-based among audio streams)\n  \
         --subtitle-stream <N>   Subtitle stream index for srt/vtt\n  \
         --encode auto|copy|reencode\n  \
         --quality balanced|high|small\n  \
         --mono                  Downmix to mono when re-encoding\n  \
         --sample-rate <HZ>      Audio sample rate when re-encoding\n  \
         --scale-width <PX>      Scale video/image width (height auto)\n\n\
         URLs (http/https) are extracted with Defuddle.\n\
         Use `shift-cli formats` to list registered conversion capability.\n\
         Use `shift-cli doctor` to see which engines are installed and ready.\n\
         Overwrite policy (single-file and batch):\n  \
         - Existing outputs require --force (otherwise the item fails).\n  \
         - The source file is never overwritten.\n  \
         - Batch only: when two inputs resolve to the same output name in one\n    \
         queue, later items get stem-1.ext, stem-2.ext, … so both can succeed.\n  \
         Single-file: if no -o is supplied, Shift writes beside the source.\n  \
         Batch: prefer -O/--output-dir for multi-file runs."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
    /// Serializes tests that mutate process-wide converter env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn unique_temp(name: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "shift-cli-test-{}-{}-{}",
            std::process::id(),
            n,
            name
        ))
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[cfg(unix)]
    fn write_fake_pandoc(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\nprintf '# converted by fake pandoc\\n'").unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_inputs_remain_file_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let input = OsString::from_vec(b"report-\xff.pdf".to_vec());

        assert_eq!(url_input(&input), None);
        assert_eq!(
            PathBuf::from(input).as_os_str().as_bytes(),
            b"report-\xff.pdf"
        );
    }

    #[test]
    fn help_and_formats_exit_successfully() {
        assert!(run(args(&["--help"])).is_ok());
        assert!(run(args(&["formats"])).is_ok());
        assert!(run(args(&[])).is_ok());
    }

    #[test]
    fn doctor_runs_and_returns_stable_exit_code() {
        let code = run(args(&["doctor", "--quiet"])).unwrap();
        // On a fully provisioned machine this is SUCCESS; otherwise failure. Either is valid.
        assert!(
            code == ExitCode::SUCCESS || code == ExitCode::from(1),
            "unexpected doctor exit code"
        );
    }

    #[test]
    fn doctor_script_mode_succeeds_as_result() {
        // Output goes to stdout; we only assert the command is accepted.
        assert!(run(args(&["doctor", "--script", "--quiet"])).is_ok());
    }

    #[test]
    fn doctor_rejects_unknown_flags() {
        let error = run(args(&["doctor", "--nope"])).unwrap_err();
        assert!(error.contains("unknown doctor argument"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn doctor_exit_and_script_keys_follow_env_overrides() {
        use shift_core::conversion::DiagnosticsReport;
        use std::os::unix::fs::PermissionsExt;

        let _guard = ENV_LOCK.lock().unwrap();
        let dir = unique_temp("doctor-env");
        std::fs::create_dir_all(&dir).unwrap();

        let pandoc = dir.join("fake-pandoc");
        std::fs::write(&pandoc, "#!/bin/sh\necho 'pandoc 1.2.3'\n").unwrap();
        let mut permissions = std::fs::metadata(&pandoc).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&pandoc, permissions).unwrap();

        let missing = dir.join("missing-bin");
        let engine_vars = [
            "SHIFT_MARKITDOWN_BIN",
            "SHIFT_PANDOC_BIN",
            "SHIFT_DEFUDDLE_BIN",
            "SHIFT_DOCLING_BIN",
            "SHIFT_FFMPEG_BIN",
        ];

        unsafe {
            for key in engine_vars {
                std::env::set_var(key, &missing);
            }
            std::env::set_var("SHIFT_PANDOC_BIN", &pandoc);
            std::env::remove_var("SHIFT_PDF_ENGINE");
        }

        let report = DiagnosticsReport::collect();
        let script = report.render_script();
        assert!(
            script.contains("engine.pandoc=ready"),
            "expected ready pandoc in script:\n{script}"
        );
        assert!(
            script.contains("version=1.2.3") || script.contains("version=\"1.2.3\""),
            "expected pandoc version in script:\n{script}"
        );
        assert!(
            script.contains(&format!("path={}", pandoc.display()))
                || script.contains(&format!("path=\"{}\"", pandoc.display())),
            "expected pandoc path in script:\n{script}"
        );
        assert!(
            script.contains("engine.ffmpeg=missing"),
            "expected missing ffmpeg:\n{script}"
        );
        assert!(
            script.contains("exit_code=0"),
            "partial install should exit 0:\n{script}"
        );
        assert!(
            script.contains("complete=false"),
            "expected incomplete install:\n{script}"
        );

        let code = run(args(&["doctor", "--quiet"])).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);

        // All engines missing → exit 1.
        unsafe {
            for key in engine_vars {
                std::env::set_var(key, &missing);
            }
        }
        let empty = DiagnosticsReport::collect();
        assert_eq!(empty.exit_code(), 1);
        assert_eq!(
            run(args(&["doctor", "--quiet"])).unwrap(),
            ExitCode::from(1)
        );

        unsafe {
            for key in engine_vars {
                std::env::remove_var(key);
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_missing_input() {
        let error = run(args(&["-t", "html"])).unwrap_err();
        assert!(error.contains("missing input"), "{error}");
    }

    #[test]
    fn rejects_unknown_arguments() {
        let error = run(args(&["file.md", "--nope"])).unwrap_err();
        assert!(error.contains("unknown argument"), "{error}");
    }

    #[test]
    fn rejects_stdout_with_output() {
        let error = run(args(&["file.md", "--stdout", "-o", "out.md"])).unwrap_err();
        assert!(
            error.contains("--stdout") && error.contains("output path"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn batch_writes_multiple_files_to_output_dir() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = unique_temp("batch-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let pandoc = dir.join("fake-pandoc");
        write_fake_pandoc(&pandoc);
        let a = dir.join("a.html");
        let b = dir.join("b.html");
        std::fs::write(&a, b"<p>a</p>").unwrap();
        std::fs::write(&b, b"<p>b</p>").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        unsafe {
            std::env::set_var("SHIFT_PANDOC_BIN", &pandoc);
        }

        let result = run(args(&[
            "batch",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "-t",
            "html",
            "-O",
            out.to_str().unwrap(),
            "--module",
            "pandoc",
            "--force",
        ]));

        unsafe {
            std::env::remove_var("SHIFT_PANDOC_BIN");
        }

        assert!(result.is_ok(), "batch failed: {result:?}");
        assert!(out.join("a.converted.html").is_file() || out.join("a.html").is_file());
        assert!(out.join("b.converted.html").is_file() || out.join("b.html").is_file());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_unknown_module_ids() {
        let error = run(args(&["file.md", "--module", "pandocx"])).unwrap_err();
        assert!(error.contains("unknown module"), "{error}");
        assert!(error.contains("pandocx"), "{error}");
    }

    #[test]
    fn prepare_destination_never_overwrites_source() {
        let dir = unique_temp("src-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("page.html");
        std::fs::write(&source, b"<p>src</p>").unwrap();

        let error = prepare_destination(&source, Some(&source), true).unwrap_err();
        assert!(error.contains("refusing to overwrite source"), "{error}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prepare_destination_requires_force_for_existing_outputs() {
        let dir = unique_temp("out-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("page.html");
        let dest = dir.join("page.converted.html");
        std::fs::write(&source, b"<p>src</p>").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        let error = prepare_destination(&dest, Some(&source), false).unwrap_err();
        assert!(error.contains("--force"), "{error}");

        assert!(prepare_destination(&dest, Some(&source), true).is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_output_path_is_distinct_for_same_extension() {
        let input = Path::new("/tmp/notes/page.html");
        let output = default_output_path(input, OutputFormat::HTML);
        assert_ne!(output, input);
        assert_eq!(output, Path::new("/tmp/notes/page.converted.html"));
    }

    #[cfg(unix)]
    #[test]
    fn convert_writes_beside_source_without_clobbering_it() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = unique_temp("convert-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let pandoc = dir.join("fake-pandoc");
        write_fake_pandoc(&pandoc);
        let input = dir.join("page.html");
        std::fs::write(&input, b"<p>hello</p>").unwrap();
        let expected = dir.join("page.converted.html");
        let _ = std::fs::remove_file(&expected);

        // SAFETY: test-only env mutation, serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("SHIFT_PANDOC_BIN", &pandoc);
        }

        let result = run(args(&[
            input.to_str().unwrap(),
            "-t",
            "html",
            "--module",
            "pandoc",
        ]));

        unsafe {
            std::env::remove_var("SHIFT_PANDOC_BIN");
        }

        assert!(result.is_ok(), "convert failed: {result:?}");
        assert!(
            expected.is_file(),
            "expected output at {}",
            expected.display()
        );
        assert_eq!(std::fs::read_to_string(&input).unwrap(), "<p>hello</p>");
        assert_eq!(
            std::fs::read_to_string(&expected).unwrap(),
            "# converted by fake pandoc\n"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn convert_rejects_explicit_output_equal_to_source() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = unique_temp("overwrite-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let pandoc = dir.join("fake-pandoc");
        write_fake_pandoc(&pandoc);
        let input = dir.join("page.html");
        std::fs::write(&input, b"<p>hello</p>").unwrap();

        unsafe {
            std::env::set_var("SHIFT_PANDOC_BIN", &pandoc);
        }

        let error = run(args(&[
            input.to_str().unwrap(),
            "-t",
            "html",
            "-o",
            input.to_str().unwrap(),
            "--module",
            "pandoc",
            "--force",
        ]))
        .unwrap_err();

        unsafe {
            std::env::remove_var("SHIFT_PANDOC_BIN");
        }

        assert!(error.contains("refusing to overwrite source"), "{error}");
        assert_eq!(std::fs::read_to_string(&input).unwrap(), "<p>hello</p>");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn convert_stdout_emits_bytes_without_writing_files() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = unique_temp("stdout-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let pandoc = dir.join("fake-pandoc");
        write_fake_pandoc(&pandoc);
        let input = dir.join("notes.md");
        std::fs::write(&input, b"# hi\n").unwrap();

        unsafe {
            std::env::set_var("SHIFT_PANDOC_BIN", &pandoc);
        }

        let result = run(args(&[
            input.to_str().unwrap(),
            "-t",
            "html",
            "--stdout",
            "--module",
            "pandoc",
        ]));

        unsafe {
            std::env::remove_var("SHIFT_PANDOC_BIN");
        }

        assert!(result.is_ok(), "{result:?}");
        // No default sibling file should have been created for --stdout.
        assert!(!dir.join("notes.html").exists());

        let _ = std::fs::remove_dir_all(dir);
    }
}
