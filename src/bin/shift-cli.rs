use shift_core::conversion::{
    BatchEnqueueOptions, BatchEvent, BatchQueue, BatchSource, ConversionArtifact,
    ConversionOptions, ConversionProgress, ConversionRegistry, DefuddleOptions, DiagnosticsReport,
    DoclingImageExportMode, DoclingOptions, DoclingTableMode, FfmpegEncodeMode, FfmpegOptions,
    FfmpegQuality, MagicPaste, MarkItDownOptions, OutputFormat, PandocOptions, PasteToken,
    PdfInputOptions, default_output_path, ensure_public_url_fetch_allowed, expand_input_paths,
    looks_like_url, materialize_paste_token, parse_magic_paste, prepare_batch_destination,
    run_batch, url_display_host,
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

    let parsed = parse_convert_args(&arguments)?;
    let inputs = resolve_cli_inputs(parsed.inputs, parsed.recursive)?;

    if inputs.is_empty() {
        return Err("missing input file or URL (try `shift-cli --help`)".to_owned());
    }

    if parsed.stdout && (parsed.output.is_some() || parsed.output_dir.is_some()) {
        return Err("use either --stdout or an output path, not both".to_owned());
    }

    if parsed.output.is_some() && parsed.output_dir.is_some() {
        return Err("use either --output or --output-dir, not both".to_owned());
    }

    // Shared with app: public hosts only unless explicitly opted in.
    // `--yes` never re-enables private/LAN fetches.
    if parsed.allow_private_urls {
        // SAFETY: single-threaded CLI entry; set before any fetch.
        unsafe {
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
        }
    }

    let use_batch = parsed.batch_explicit || inputs.len() > 1 || parsed.output_dir.is_some();
    if use_batch && parsed.stdout {
        return Err("batch conversion cannot write to --stdout (use -O/--output-dir)".to_owned());
    }
    if use_batch && parsed.output.is_some() && inputs.len() > 1 {
        return Err(
            "batch conversion with multiple inputs requires -O/--output-dir, not -o/--output"
                .to_owned(),
        );
    }

    let registry = build_registry(parsed.preferred_module.as_deref())?;
    let mut options = ConversionOptions {
        ffmpeg: parsed.ffmpeg,
        markitdown: parsed.markitdown,
        pandoc: parsed.pandoc,
        defuddle: parsed.defuddle,
        docling: parsed.docling,
        pdf: parsed.pdf,
        cancel: None,
        progress: None,
    };
    if parsed.progress {
        options.progress = Some(Arc::new(|progress| match progress {
            ConversionProgress::Phase(label) => {
                eprintln!("  {label}");
            }
            ConversionProgress::Fraction { fraction, label } => {
                eprint!("\r  {label} ({:.0}%)", fraction * 100.0);
            }
        }));
    }

    if use_batch {
        return run_batch_cli(
            inputs,
            parsed.target,
            options,
            parsed.output_dir,
            parsed.output,
            parsed.force,
            parsed.yes,
            &registry,
            parsed.verbose,
        );
    }

    // Single-file / single-URL path (in-memory convert, then write or stdout).
    let input = &inputs[0];
    let classified = classify_cli_input(input)?;
    confirm_network_urls(
        network_urls_from_classified(std::slice::from_ref(&classified)),
        parsed.yes,
    )?;
    let source = materialize_cli_input(classified)?;
    let artifact = match &source {
        BatchSource::Url(url) => {
            eprintln!("shift-cli: fetching {}", url_display_host(url));
            registry
                .convert_url_with_options(url, parsed.target, &options)
                .map_err(|error| error.to_string())?
        }
        BatchSource::File(path) => registry
            .convert_to_with_options(path.clone(), parsed.target, &options)
            .map_err(|error| error.to_string())?,
    };

    if parsed.progress {
        // Clear any in-progress line before final output.
        eprintln!();
    }
    if parsed.verbose {
        print_invocations(&artifact);
    }

    if parsed.stdout {
        use std::io::Write;
        std::io::stdout()
            .write_all(&artifact.bytes)
            .map_err(|error| format!("could not write output: {error}"))?;
    } else {
        let destination = parsed.output.unwrap_or_else(|| match &source {
            BatchSource::Url(_) => PathBuf::from(&artifact.file_name),
            BatchSource::File(path) => default_output_path(path.as_path(), parsed.target),
        });
        let source_path = source.as_file().map(|path| path.to_path_buf());
        // Shared with batch: refuse source overwrite, honor --force, create parents.
        prepare_batch_destination(&destination, source_path.as_deref(), parsed.force)
            .map_err(|error| error.to_string())?;
        artifact
            .write_to(&destination)
            .map_err(|error| error.to_string())?;
        // Full path on stdout so scripts and humans know where the file landed.
        println!("{}", destination.display());
    }

    Ok(ExitCode::SUCCESS)
}

/// Parsed convert/batch arguments (engine knobs + I/O flags). Extracted for unit tests.
#[derive(Clone, Debug, PartialEq)]
struct ParsedConvertArgs {
    inputs: Vec<OsString>,
    output: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    stdout: bool,
    force: bool,
    /// Skip interactive network confirms / allow non-TTY network. Does not allow private URLs.
    yes: bool,
    /// Opt into localhost/LAN URL fetches (default: public internet only).
    allow_private_urls: bool,
    target: OutputFormat,
    preferred_module: Option<String>,
    ffmpeg: FfmpegOptions,
    markitdown: MarkItDownOptions,
    pandoc: PandocOptions,
    defuddle: DefuddleOptions,
    docling: DoclingOptions,
    pdf: PdfInputOptions,
    recursive: bool,
    verbose: bool,
    progress: bool,
    batch_explicit: bool,
}

impl Default for ParsedConvertArgs {
    fn default() -> Self {
        Self {
            inputs: Vec::new(),
            output: None,
            output_dir: None,
            stdout: false,
            force: false,
            yes: false,
            allow_private_urls: false,
            target: OutputFormat::MARKDOWN,
            preferred_module: None,
            ffmpeg: FfmpegOptions::default(),
            markitdown: MarkItDownOptions::default(),
            pandoc: PandocOptions::default(),
            defuddle: DefuddleOptions::default(),
            docling: DoclingOptions::default(),
            pdf: PdfInputOptions::default(),
            recursive: false,
            verbose: false,
            progress: false,
            batch_explicit: false,
        }
    }
}

fn parse_convert_args(arguments: &[OsString]) -> Result<ParsedConvertArgs, String> {
    let mut cursor = 0;
    let mut parsed = ParsedConvertArgs::default();

    if arguments.first().is_some_and(|value| value == "convert") {
        cursor += 1;
    } else if arguments.first().is_some_and(|value| value == "batch") {
        cursor += 1;
        parsed.batch_explicit = true;
    }

    while cursor < arguments.len() {
        let arg = arguments[cursor].to_string_lossy();
        match arg.as_ref() {
            "-o" | "--output" => {
                cursor += 1;
                parsed.output = Some(
                    arguments
                        .get(cursor)
                        .map(PathBuf::from)
                        .ok_or_else(|| "--output requires a path".to_owned())?,
                );
            }
            "-O" | "--output-dir" => {
                cursor += 1;
                parsed.output_dir = Some(
                    arguments
                        .get(cursor)
                        .map(PathBuf::from)
                        .ok_or_else(|| "--output-dir requires a directory".to_owned())?,
                );
            }
            "--stdout" => parsed.stdout = true,
            "--force" => parsed.force = true,
            "--yes" | "-y" => parsed.yes = true,
            "--allow-private-urls" => parsed.allow_private_urls = true,
            "--recursive" => parsed.recursive = true,
            "--verbose" | "-v" => parsed.verbose = true,
            "--progress" => parsed.progress = true,
            "-t" | "--to" => {
                cursor += 1;
                parsed.target = arguments
                    .get(cursor)
                    .ok_or_else(|| "--to requires a format".to_owned())?
                    .to_string_lossy()
                    .parse::<OutputFormat>()
                    .map_err(|error| error.to_string())?;
            }
            "--module" => {
                cursor += 1;
                parsed.preferred_module = Some(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--module requires an id".to_owned())?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--start" => {
                cursor += 1;
                parsed.ffmpeg.start_secs = Some(parse_secs(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--start requires seconds".to_owned())?,
                    "--start",
                )?);
            }
            "--duration" => {
                cursor += 1;
                parsed.ffmpeg.duration_secs = Some(parse_secs(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--duration requires seconds".to_owned())?,
                    "--duration",
                )?);
            }
            "--frame" => {
                cursor += 1;
                parsed.ffmpeg.frame_secs = Some(parse_secs(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--frame requires seconds".to_owned())?,
                    "--frame",
                )?);
            }
            "--frame-interval" => {
                cursor += 1;
                parsed.ffmpeg.frame_interval_secs = Some(parse_secs(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--frame-interval requires seconds".to_owned())?,
                    "--frame-interval",
                )?);
            }
            "--audio-stream" => {
                cursor += 1;
                parsed.ffmpeg.audio_stream = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--audio-stream requires an index".to_owned())?,
                    "--audio-stream",
                )?);
            }
            "--subtitle-stream" => {
                cursor += 1;
                parsed.ffmpeg.subtitle_stream = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--subtitle-stream requires an index".to_owned())?,
                    "--subtitle-stream",
                )?);
            }
            "--encode" => {
                cursor += 1;
                parsed.ffmpeg.encode_mode = arguments
                    .get(cursor)
                    .ok_or_else(|| "--encode requires auto|copy|reencode".to_owned())?
                    .to_string_lossy()
                    .parse::<FfmpegEncodeMode>()
                    .map_err(|error| error.to_string())?;
            }
            "--quality" => {
                cursor += 1;
                parsed.ffmpeg.quality = arguments
                    .get(cursor)
                    .ok_or_else(|| "--quality requires balanced|high|small".to_owned())?
                    .to_string_lossy()
                    .parse::<FfmpegQuality>()
                    .map_err(|error| error.to_string())?;
            }
            "--mono" => parsed.ffmpeg.mono = true,
            "--mute" => parsed.ffmpeg.mute = true,
            "--normalize-audio" => parsed.ffmpeg.normalize_audio = true,
            "--burn-subtitles" => parsed.ffmpeg.burn_subtitles = true,
            "--fps" => {
                cursor += 1;
                parsed.ffmpeg.fps = Some(parse_secs(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--fps requires a frame rate".to_owned())?,
                    "--fps",
                )?);
            }
            "--sample-rate" => {
                cursor += 1;
                parsed.ffmpeg.sample_rate_hz = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--sample-rate requires Hz".to_owned())?,
                    "--sample-rate",
                )?);
            }
            "--scale-width" => {
                cursor += 1;
                parsed.ffmpeg.scale_width = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--scale-width requires pixels".to_owned())?,
                    "--scale-width",
                )?);
            }
            "--keep-data-uris" => parsed.markitdown.keep_data_uris = true,
            "--standalone" => parsed.pandoc.standalone = true,
            "--toc" => parsed.pandoc.toc = true,
            "--pdf-engine" => {
                cursor += 1;
                parsed.pandoc.pdf_engine = Some(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--pdf-engine requires a name or path".to_owned())?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--reference-doc" => {
                cursor += 1;
                parsed.pandoc.reference_doc = Some(
                    arguments
                        .get(cursor)
                        .map(PathBuf::from)
                        .ok_or_else(|| "--reference-doc requires a path".to_owned())?,
                );
            }
            "--frontmatter" => parsed.defuddle.frontmatter = true,
            "--lang" => {
                cursor += 1;
                parsed.defuddle.lang = Some(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--lang requires a BCP 47 code".to_owned())?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--docling-images" => {
                cursor += 1;
                parsed.docling.image_export_mode = arguments
                    .get(cursor)
                    .ok_or_else(|| {
                        "--docling-images requires placeholder|embedded|referenced".to_owned()
                    })?
                    .to_string_lossy()
                    .parse::<DoclingImageExportMode>()
                    .map_err(|error| error.to_string())?;
            }
            "--docling-ocr" => parsed.docling.ocr = true,
            "--no-docling-ocr" => parsed.docling.ocr = false,
            "--docling-tables" => parsed.docling.tables = true,
            "--no-docling-tables" => parsed.docling.tables = false,
            "--docling-table-mode" => {
                cursor += 1;
                parsed.docling.table_mode = arguments
                    .get(cursor)
                    .ok_or_else(|| "--docling-table-mode requires fast|accurate".to_owned())?
                    .to_string_lossy()
                    .parse::<DoclingTableMode>()
                    .map_err(|error| error.to_string())?;
            }
            "--ocr-lang" => {
                cursor += 1;
                parsed.docling.ocr_lang = Some(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--ocr-lang requires a language code".to_owned())?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--pdf-password" => {
                cursor += 1;
                parsed.pdf.password = Some(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--pdf-password requires a value".to_owned())?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--page-from" => {
                cursor += 1;
                parsed.pdf.page_from = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--page-from requires a 1-based page number".to_owned())?,
                    "--page-from",
                )?);
            }
            "--page-to" => {
                cursor += 1;
                parsed.pdf.page_to = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--page-to requires a 1-based page number".to_owned())?,
                    "--page-to",
                )?);
            }
            "--pages" => {
                cursor += 1;
                let value = arguments
                    .get(cursor)
                    .ok_or_else(|| "--pages requires FROM-TO (e.g. 2-5)".to_owned())?;
                let (from, to) = parse_pages_range(value, "--pages")?;
                parsed.pdf.page_from = from;
                parsed.pdf.page_to = to;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown argument: {value}"));
            }
            _ => {
                parsed.inputs.push(arguments[cursor].clone());
            }
        }
        cursor += 1;
    }

    if parsed.inputs.is_empty() {
        return Err("missing input file or URL (try `shift-cli --help`)".to_owned());
    }

    Ok(parsed)
}

/// Expand directories when `--recursive`, or reject bare directory inputs.
fn resolve_cli_inputs(inputs: Vec<OsString>, recursive: bool) -> Result<Vec<OsString>, String> {
    if recursive {
        let mut out = Vec::new();
        for input in inputs {
            if is_network_or_file_url_input(&input) {
                out.push(input);
                continue;
            }
            let path = PathBuf::from(&input);
            let expanded =
                expand_input_paths(&[path.as_path()], true).map_err(|error| error.to_string())?;
            if expanded.is_empty() {
                // Keep the original path so conversion can report a useful error
                // (unsupported extension, missing file, etc.).
                if path.is_dir() {
                    return Err(format!(
                        "no convertible files found under {}",
                        path.display()
                    ));
                }
                out.push(input);
            } else {
                out.extend(expanded.into_iter().map(PathBuf::into_os_string));
            }
        }
        if out.is_empty() {
            return Err("no convertible inputs after expanding directories".to_owned());
        }
        Ok(out)
    } else {
        for input in &inputs {
            if is_network_or_file_url_input(input) {
                continue;
            }
            let path = Path::new(input);
            if path.is_dir() {
                return Err(format!(
                    "{} is a directory; pass --recursive to expand folders",
                    path.display()
                ));
            }
        }
        Ok(inputs)
    }
}

/// Classified CLI argument before any network I/O.
#[derive(Debug)]
enum ClassifiedInput {
    Token(PasteToken),
    /// Non-UTF-8 or unclassified token treated as a filesystem path.
    Path(PathBuf),
}

/// Classify one CLI argument without downloading remote files.
fn classify_cli_input(input: &OsStr) -> Result<ClassifiedInput, String> {
    if let Some(text) = input.to_str() {
        match parse_magic_paste(text) {
            MagicPaste::Single(token) => return Ok(ClassifiedInput::Token(token)),
            MagicPaste::Multiple(_) => {
                return Err(format!(
                    "each argument must be a single path or URL (got multiple tokens): {text}"
                ));
            }
            MagicPaste::Empty => {
                let trimmed = text.trim();
                if trimmed.to_ascii_lowercase().starts_with("file:") {
                    return Err(format!(
                        "invalid file:// URL (use a local path or a valid file:// path): {trimmed}"
                    ));
                }
            }
        }
    }
    // Non-UTF-8 paths or unclassified tokens: treat as a local path.
    Ok(ClassifiedInput::Path(PathBuf::from(input)))
}

fn network_urls_from_classified(items: &[ClassifiedInput]) -> Vec<&str> {
    items
        .iter()
        .filter_map(|item| match item {
            ClassifiedInput::Token(PasteToken::PageUrl(url) | PasteToken::RemoteFileUrl(url)) => {
                Some(url.as_str())
            }
            ClassifiedInput::Token(PasteToken::LocalPath(_)) | ClassifiedInput::Path(_) => None,
        })
        .collect()
}

/// Materialize a classified input (downloads remote files when needed).
fn materialize_cli_input(input: ClassifiedInput) -> Result<BatchSource, String> {
    match input {
        ClassifiedInput::Token(token) => {
            materialize_paste_token(&token).map_err(|error| error.to_string())
        }
        ClassifiedInput::Path(path) => Ok(BatchSource::File(path)),
    }
}

fn print_invocations(artifact: &ConversionArtifact) {
    if artifact.invocations.is_empty() {
        eprintln!("# invocations: (none recorded)");
        return;
    }
    for record in &artifact.invocations {
        eprintln!("# {} {}", record.module_id, record.argv_display);
    }
}

fn parse_pages_range(value: &OsStr, flag: &str) -> Result<(Option<u32>, Option<u32>), String> {
    let text = value
        .to_str()
        .ok_or_else(|| format!("{flag} value is not valid UTF-8"))?
        .trim();
    if text.is_empty() {
        return Err(format!("{flag} expects FROM-TO (e.g. 1-3)"));
    }

    let (from_text, to_text) = if let Some((from, to)) = text.split_once('-') {
        (from.trim(), to.trim())
    } else {
        // Single page number: N → pages N-N.
        (text, text)
    };

    if from_text.is_empty() || to_text.is_empty() {
        return Err(format!("{flag} expects FROM-TO (e.g. 1-3)"));
    }

    let from = from_text
        .parse::<u32>()
        .map_err(|_| format!("{flag} expects 1-based page numbers (got `{text}`)"))?;
    let to = to_text
        .parse::<u32>()
        .map_err(|_| format!("{flag} expects 1-based page numbers (got `{text}`)"))?;

    if from == 0 || to == 0 {
        return Err(format!("{flag} page numbers are 1-based"));
    }
    if from > to {
        return Err(format!("{flag} FROM must be <= TO"));
    }

    Ok((Some(from), Some(to)))
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

#[allow(clippy::too_many_arguments)]
fn run_batch_cli(
    inputs: Vec<OsString>,
    target: OutputFormat,
    options: ConversionOptions,
    output_dir: Option<PathBuf>,
    single_output: Option<PathBuf>,
    force: bool,
    yes: bool,
    registry: &ConversionRegistry,
    verbose: bool,
) -> Result<ExitCode, String> {
    let mut queue = BatchQueue::new();
    let mut enqueue = BatchEnqueueOptions::new(target);
    enqueue.conversion = options;
    enqueue.force = force;
    enqueue.output_dir = output_dir;

    // Classify first (no network), confirm all network tokens, then download.
    let mut classified = Vec::with_capacity(inputs.len());
    for input in &inputs {
        classified.push(classify_cli_input(input)?);
    }
    confirm_network_urls(network_urls_from_classified(&classified), yes)?;
    for item in classified {
        let source = materialize_cli_input(item)?;
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
        BatchEvent::ItemSucceeded {
            path, module_id, ..
        } => {
            if verbose {
                eprintln!("# module {module_id}");
            }
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
        BatchEvent::ItemProgress {
            fraction, label, ..
        } => {
            if let Some(fraction) = fraction {
                eprint!("\r  {label} ({:.0}%)", fraction * 100.0);
            } else {
                eprint!("\r  {label}");
            }
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

/// http(s) page/file URLs and `file://` links — not expanded as directories.
fn is_network_or_file_url_input(input: &OsStr) -> bool {
    input.to_str().is_some_and(|value| {
        let value = value.trim();
        looks_like_url(value) || value.starts_with("file://") || value.starts_with("FILE://")
    })
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
         General options:\n  \
         -t, --to <FORMAT>       Output format id (default: markdown)\n  \
         -o, --output <PATH>     Write a single output to PATH\n  \
         -O, --output-dir <DIR>  Write every batch output into DIR\n  \
         --stdout                Write bytes to stdout (single input only)\n  \
         --force                 Overwrite existing outputs\n  \
         --yes, -y               Skip interactive confirms (network fetch); scripts\n  \
         --allow-private-urls    Allow localhost/LAN URL fetches (default: public only)\n  \
         --module <ID>           Prefer a conversion module (see `formats`)\n  \
         --recursive             Expand directory inputs into convertible files\n  \
         --verbose, -v           Print redacted converter invocations on stderr\n  \
         --progress              Print per-conversion progress on stderr\n  \
         Ctrl-C                  Cancel the active batch item and remaining queue\n\n\
         Media (FFmpeg) options:\n  \
         --start <SEC>           Seek to timestamp before converting\n  \
         --duration <SEC>        Limit output length\n  \
         --frame <SEC>           Still-image frame time (png/jpg)\n  \
         --frame-interval <SEC>  Seconds between frames (png-sequence-zip)\n  \
         --fps <N>               Force constant frame rate when re-encoding\n  \
         --mute                  Drop audio on video outputs\n  \
         --normalize-audio       Apply loudness normalization when re-encoding\n  \
         --burn-subtitles        Burn embedded subtitles into video\n  \
         --audio-stream <N>      Audio stream index (0-based among audio streams)\n  \
         --subtitle-stream <N>   Subtitle stream index for srt/vtt\n  \
         --encode auto|copy|reencode\n  \
         --quality balanced|high|small\n  \
         --mono                  Downmix to mono when re-encoding\n  \
         --sample-rate <HZ>      Audio sample rate when re-encoding\n  \
         --scale-width <PX>      Scale video/image width (height auto)\n\n\
         PDF input options:\n  \
         --pdf-password <SECRET> Password for encrypted PDFs\n  \
         --pages <FROM-TO>       1-based inclusive page range (e.g. 2-5)\n  \
         --page-from <N>         1-based start page\n  \
         --page-to <N>           1-based end page\n\n\
         MarkItDown options:\n  \
         --keep-data-uris        Keep base64 data URIs in Markdown output\n\n\
         Pandoc options:\n  \
         --standalone            Produce a standalone document (-s)\n  \
         --toc                   Include a table of contents\n  \
         --pdf-engine <NAME>     PDF engine override (else SHIFT_PDF_ENGINE / auto)\n  \
         --reference-doc <PATH>  Style reference for docx/odt writers\n\n\
         Defuddle options:\n  \
         --frontmatter           Prepend YAML frontmatter\n  \
         --lang <CODE>           Preferred language (BCP 47, e.g. en)\n\n\
         Docling options:\n  \
         --docling-images placeholder|embedded|referenced\n  \
         --docling-ocr / --no-docling-ocr\n  \
         --ocr-lang <CODE>       OCR language(s), e.g. eng or eng+deu\n  \
         --docling-tables / --no-docling-tables\n  \
         --docling-table-mode fast|accurate\n\n\
         Inputs may be local paths, file:// URLs, page URLs (http/https,\n\
         extracted with Defuddle), or direct file URLs (downloaded then\n\
         converted). URL fetches are public-internet only by default (no\n\
         localhost/LAN). Use a file path for local content, or pass\n\
         --allow-private-urls / SHIFT_ALLOW_PRIVATE_URLS=1. On a TTY, network\n\
         fetches (page URLs and direct file downloads) ask for confirmation\n\
         unless --yes is set. Non-interactive (non-TTY) runs require --yes for\n\
         any network fetch. --yes never unlocks private hosts.\n\
         Directory inputs require --recursive (union of registered extensions).\n\
         Use `shift-cli formats` to list registered conversion capability.\n\
         Use `shift-cli doctor` to see which engines are installed and ready.\n\
         Overwrite policy (single-file and batch):\n  \
         - Existing outputs require --force (otherwise the item fails).\n  \
         - The source file is never overwritten.\n  \
         - Missing parent directories of -o / -O paths are created.\n  \
         - Batch only: when two inputs resolve to the same output name in one\n    \
         queue, later items get stem-1.ext, stem-2.ext, … so both can succeed.\n  \
         Single-file: if no -o is supplied, Shift writes beside the source.\n  \
         Batch: prefer -O/--output-dir for multi-file runs."
    );
}

/// Confirm outbound URL fetches (page + remote file) before materialization.
///
/// Private hosts fail fast via shared policy unless `--allow-private-urls`.
/// On a TTY, prompts unless `--yes`. Non-TTY requires `--yes` (no silent network).
fn confirm_network_urls(urls: Vec<&str>, yes: bool) -> Result<(), String> {
    if urls.is_empty() {
        return Ok(());
    }

    // Fail fast on private hosts with a clear message (before any confirm/download).
    for url in &urls {
        ensure_public_url_fetch_allowed(url).map_err(|error| error.to_string())?;
    }

    let summary = if urls.len() == 1 {
        format!("Fetch public URL {}?", urls[0])
    } else {
        let hosts: Vec<String> = urls.iter().map(|url| url_display_host(url)).collect();
        format!("Fetch {} public URL(s) ({})?", urls.len(), hosts.join(", "))
    };

    if yes {
        eprintln!("shift-cli: {summary} (--yes)");
        return Ok(());
    }

    if !stdin_is_tty() {
        return Err(
            "network fetch requires --yes in non-interactive mode (no TTY for confirmation)"
                .to_owned(),
        );
    }

    eprint!("shift-cli: {summary} [y/N] ");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("could not read confirmation: {error}"))?;
    let answer = line.trim().to_ascii_lowercase();
    if matches!(answer.as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err("cancelled (network fetch not confirmed; pass --yes to skip)".to_owned())
    }
}

/// Confirm network sources after materialization (legacy shape for tests).
#[cfg(test)]
fn confirm_network_sources(sources: &[BatchSource], yes: bool) -> Result<(), String> {
    let urls: Vec<&str> = sources
        .iter()
        .filter_map(|source| match source {
            BatchSource::Url(url) => Some(url.as_str()),
            BatchSource::File(_) => None,
        })
        .collect();
    confirm_network_urls(urls, yes)
}

fn stdin_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
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

        assert!(!is_network_or_file_url_input(&input));
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
    fn parses_yes_and_allow_private_url_flags() {
        let parsed = parse_convert_args(&args(&[
            "https://example.com",
            "--yes",
            "--allow-private-urls",
            "-t",
            "markdown",
        ]))
        .unwrap();
        assert!(parsed.yes);
        assert!(parsed.allow_private_urls);
        assert_eq!(parsed.target, OutputFormat::MARKDOWN);
    }

    #[test]
    fn confirm_network_rejects_private_hosts_without_allow() {
        // Ensure default public-only policy for this process.
        let _env = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        let sources = [BatchSource::Url("http://127.0.0.1/x".into())];
        let error = confirm_network_sources(&sources, true).unwrap_err();
        assert!(
            error.contains("non-public") || error.contains("public internet"),
            "{error}"
        );
    }

    #[test]
    fn classify_keeps_remote_file_url_until_materialize() {
        let classified =
            classify_cli_input(OsStr::new("https://cdn.example.com/docs/report.pdf")).unwrap();
        match &classified {
            ClassifiedInput::Token(PasteToken::RemoteFileUrl(url)) => {
                assert_eq!(url, "https://cdn.example.com/docs/report.pdf");
            }
            other => panic!("expected remote file token, got {other:?}"),
        }
        let urls = network_urls_from_classified(std::slice::from_ref(&classified));
        assert_eq!(urls, vec!["https://cdn.example.com/docs/report.pdf"]);
    }

    #[test]
    fn classify_rejects_invalid_file_urls() {
        let error = classify_cli_input(OsStr::new("file://hostname/only")).unwrap_err();
        assert!(
            error.contains("invalid file://") || error.contains("file://"),
            "{error}"
        );
    }

    #[test]
    fn confirm_network_urls_includes_remote_file_urls() {
        let _env = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        let error = confirm_network_urls(vec!["http://127.0.0.1/secret.pdf"], true).unwrap_err();
        assert!(
            error.contains("non-public") || error.contains("public internet"),
            "{error}"
        );
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

        let error = prepare_batch_destination(&source, Some(&source), true).unwrap_err();
        assert!(
            error.to_string().contains("refusing to overwrite source"),
            "{error}"
        );

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

        let error = prepare_batch_destination(&dest, Some(&source), false).unwrap_err();
        assert!(error.to_string().contains("--force"), "{error}");

        assert!(prepare_batch_destination(&dest, Some(&source), true).is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prepare_destination_creates_missing_parents() {
        let dir = unique_temp("parents");
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("page.html");
        std::fs::write(&source, b"<p>src</p>").unwrap();
        let dest = dir.join("nested").join("deep").join("out.md");

        assert!(prepare_batch_destination(&dest, Some(&source), false).is_ok());
        assert!(dest.parent().unwrap().is_dir());

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

    #[test]
    fn parse_media_flags_into_ffmpeg_options() {
        let parsed = parse_convert_args(&args(&[
            "clip.mp4",
            "--mute",
            "--fps",
            "30",
            "--normalize-audio",
            "--burn-subtitles",
            "--frame-interval",
            "0.5",
            "-t",
            "png-sequence-zip",
        ]))
        .unwrap();
        assert!(parsed.ffmpeg.mute);
        assert_eq!(parsed.ffmpeg.fps, Some(30.0));
        assert!(parsed.ffmpeg.normalize_audio);
        assert!(parsed.ffmpeg.burn_subtitles);
        assert_eq!(parsed.ffmpeg.frame_interval_secs, Some(0.5));
        assert_eq!(parsed.target, OutputFormat::PNG_SEQUENCE_ZIP);
    }

    #[test]
    fn parse_pdf_docling_and_pandoc_flags() {
        let parsed = parse_convert_args(&args(&[
            "report.pdf",
            "--ocr-lang",
            "eng+deu",
            "--pdf-password",
            "s3cret",
            "--pages",
            "2-5",
            "--reference-doc",
            "/refs/style.docx",
            "--verbose",
            "--progress",
            "--recursive",
        ]))
        .unwrap();
        assert_eq!(parsed.docling.ocr_lang.as_deref(), Some("eng+deu"));
        assert_eq!(parsed.pdf.password.as_deref(), Some("s3cret"));
        assert_eq!(parsed.pdf.page_from, Some(2));
        assert_eq!(parsed.pdf.page_to, Some(5));
        assert_eq!(
            parsed.pandoc.reference_doc.as_deref(),
            Some(Path::new("/refs/style.docx"))
        );
        assert!(parsed.verbose);
        assert!(parsed.progress);
        assert!(parsed.recursive);
    }

    #[test]
    fn parse_page_from_to_flags() {
        let parsed =
            parse_convert_args(&args(&["doc.pdf", "--page-from", "3", "--page-to", "7"])).unwrap();
        assert_eq!(parsed.pdf.page_from, Some(3));
        assert_eq!(parsed.pdf.page_to, Some(7));
    }

    #[test]
    fn parse_pages_single_number() {
        let (from, to) = parse_pages_range(OsStr::new("4"), "--pages").unwrap();
        assert_eq!(from, Some(4));
        assert_eq!(to, Some(4));
    }

    #[test]
    fn parse_pages_rejects_inverted_range() {
        let error = parse_pages_range(OsStr::new("9-2"), "--pages").unwrap_err();
        assert!(error.contains("FROM must be <="), "{error}");
    }

    #[test]
    fn rejects_directory_input_without_recursive() {
        let dir = unique_temp("dir-input");
        std::fs::create_dir_all(&dir).unwrap();
        let error = run(args(&[dir.to_str().unwrap(), "-t", "markdown"])).unwrap_err();
        assert!(error.contains("directory"), "{error}");
        assert!(error.contains("--recursive"), "{error}");
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
