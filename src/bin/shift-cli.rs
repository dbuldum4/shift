use shift_core::conversion::{
    BatchEnqueueOptions, BatchEvent, BatchQueue, BatchSource, ConversionArtifact,
    ConversionOptions, ConversionProgress, ConversionRegistry, DefuddleOptions, DiagnosticsReport,
    DoclingAsrModel, DoclingImageExportMode, DoclingOptions, DoclingTableMode,
    DoclingVideoSamplingMode, FfmpegEncodeMode, FfmpegOptions, FfmpegQuality, MagicPaste,
    MarkItDownOptions, OutputFormat, PandocOptions, PasteToken, PdfCompression, PdfInputOptions,
    SipsFlip, SipsOptions, SipsQuality, SpreadsheetOptions, default_output_path,
    ensure_public_url_fetch_allowed, expand_input_paths, looks_like_url, materialize_paste_token,
    parse_magic_paste, prepare_batch_destination, run_batch, url_display_host,
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
        return Ok(ExitCode::FAILURE);
    }

    if arguments
        .first()
        .is_some_and(|value| matches!(value.to_string_lossy().as_ref(), "-h" | "--help" | "help"))
    {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    if arguments
        .first()
        .is_some_and(|value| value == "--version" || value == "version")
    {
        println!("shift-cli {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    if arguments.first().is_some_and(|value| value == "formats") {
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
    let cancel = install_ctrl_c_handler();
    let mut options = ConversionOptions {
        ffmpeg: parsed.ffmpeg,
        markitdown: parsed.markitdown,
        pandoc: parsed.pandoc,
        defuddle: parsed.defuddle,
        docling: parsed.docling,
        sips: parsed.sips,
        spreadsheet: parsed.spreadsheet,
        pdf: parsed.pdf,
        target_size_bytes: parsed.target_size_bytes,
        cancel: Some(Arc::clone(&cancel)),
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
    let source = materialize_cli_input(classified, Some(Arc::clone(&cancel)))?;
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
    sips: SipsOptions,
    spreadsheet: SpreadsheetOptions,
    pdf: PdfInputOptions,
    target_size_bytes: Option<u64>,
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
            sips: SipsOptions::default(),
            spreadsheet: SpreadsheetOptions::default(),
            pdf: PdfInputOptions::default(),
            target_size_bytes: None,
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
            "--target-size" => {
                cursor += 1;
                parsed.target_size_bytes = Some(parse_byte_size(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--target-size requires a size such as 10MB".to_owned())?,
                    "--target-size",
                )?);
            }
            "--keep-data-uris" => parsed.markitdown.keep_data_uris = true,
            "--standalone" => parsed.pandoc.standalone = true,
            "--toc" => parsed.pandoc.toc = true,
            "--citations" => parsed.pandoc.citations = true,
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
            "--docling-asr-model" => {
                cursor += 1;
                parsed.docling.asr_model = arguments
                    .get(cursor)
                    .ok_or_else(|| {
                        "--docling-asr-model requires tiny|base|small|medium|large|turbo".to_owned()
                    })?
                    .to_string_lossy()
                    .parse::<DoclingAsrModel>()
                    .map_err(|error| error.to_string())?;
            }
            "--docling-video-sampling" => {
                cursor += 1;
                parsed.docling.video_sampling_mode = arguments
                    .get(cursor)
                    .ok_or_else(|| "--docling-video-sampling requires fixed|scene".to_owned())?
                    .to_string_lossy()
                    .parse::<DoclingVideoSamplingMode>()
                    .map_err(|error| error.to_string())?;
            }
            "--docling-video-frame-interval" => {
                cursor += 1;
                parsed.docling.video_frame_interval_secs = parse_positive_number(
                    arguments.get(cursor).ok_or_else(|| {
                        "--docling-video-frame-interval requires seconds".to_owned()
                    })?,
                    "--docling-video-frame-interval",
                    false,
                )?;
            }
            "--docling-video-cuts-per-minute" => {
                cursor += 1;
                parsed.docling.video_cuts_per_minute = parse_positive_number(
                    arguments.get(cursor).ok_or_else(|| {
                        "--docling-video-cuts-per-minute requires a rate".to_owned()
                    })?,
                    "--docling-video-cuts-per-minute",
                    true,
                )?;
            }
            "--docling-video-prominence" => {
                cursor += 1;
                parsed.docling.video_prominence = parse_positive_number(
                    arguments.get(cursor).ok_or_else(|| {
                        "--docling-video-prominence requires a threshold".to_owned()
                    })?,
                    "--docling-video-prominence",
                    true,
                )?;
            }
            "--docling-video-diarization" => parsed.docling.video_diarization = true,
            "--no-docling-video-diarization" => parsed.docling.video_diarization = false,
            "--sips-max-dimension" => {
                cursor += 1;
                parsed.sips.max_dimension = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--sips-max-dimension requires pixels".to_owned())?,
                    "--sips-max-dimension",
                )?);
            }
            "--sips-quality" => {
                cursor += 1;
                parsed.sips.quality = arguments
                    .get(cursor)
                    .ok_or_else(|| "--sips-quality requires balanced|high|small".to_owned())?
                    .to_string_lossy()
                    .parse::<SipsQuality>()
                    .map_err(|error| error.to_string())?;
            }
            "--sips-rotate" => {
                cursor += 1;
                parsed.sips.rotate_degrees = Some(parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--sips-rotate requires degrees clockwise".to_owned())?,
                    "--sips-rotate",
                )?);
            }
            "--sips-flip" => {
                cursor += 1;
                parsed.sips.flip = Some(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--sips-flip requires horizontal|vertical".to_owned())?
                        .to_string_lossy()
                        .parse::<SipsFlip>()
                        .map_err(|error| error.to_string())?,
                );
            }
            "--sips-strip-profile" => parsed.sips.strip_color_profile = true,
            "--sheet" | "--sheet-name" => {
                cursor += 1;
                let name = arguments
                    .get(cursor)
                    .ok_or_else(|| "--sheet requires a sheet name".to_owned())?
                    .to_string_lossy()
                    .trim()
                    .to_owned();
                if name.is_empty() {
                    return Err("--sheet requires a non-empty sheet name".to_owned());
                }
                parsed.spreadsheet.sheet_name = Some(name);
            }
            "--sheet-index" => {
                cursor += 1;
                let raw = arguments
                    .get(cursor)
                    .ok_or_else(|| "--sheet-index requires a 1-based index".to_owned())?
                    .to_string_lossy();
                let index = raw.trim().parse::<u32>().map_err(|_| {
                    format!("--sheet-index expects a positive integer (got `{raw}`)")
                })?;
                if index == 0 {
                    return Err("--sheet-index is 1-based (got 0)".to_owned());
                }
                parsed.spreadsheet.sheet_index = Some(index);
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
            "--pdf-rotate" => {
                cursor += 1;
                let degrees = parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--pdf-rotate requires 90, 180, or 270".to_owned())?,
                    "--pdf-rotate",
                )?;
                if !matches!(degrees, 90 | 180 | 270) {
                    return Err("--pdf-rotate requires 90, 180, or 270".to_owned());
                }
                parsed.pdf.rotate_degrees = Some(degrees as u16);
            }
            "--pdf-compression" => {
                cursor += 1;
                parsed.pdf.compression = arguments
                    .get(cursor)
                    .ok_or_else(|| {
                        "--pdf-compression requires preserve|lossless|smaller".to_owned()
                    })?
                    .to_string_lossy()
                    .parse::<PdfCompression>()
                    .map_err(|error| error.to_string())?;
            }
            "--pdf-linearize" => parsed.pdf.linearize = true,
            "--pdf-split-pages" => {
                cursor += 1;
                let pages = parse_u32(
                    arguments
                        .get(cursor)
                        .ok_or_else(|| "--pdf-split-pages requires a page count".to_owned())?,
                    "--pdf-split-pages",
                )?;
                if pages == 0 {
                    return Err("--pdf-split-pages must be at least 1".to_owned());
                }
                parsed.pdf.split_pages = Some(pages);
            }
            "--" => {
                cursor += 1;
                while cursor < arguments.len() {
                    parsed.inputs.push(arguments[cursor].clone());
                    cursor += 1;
                }
                break;
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "unknown argument: {value} (try `shift-cli --help`)"
                ));
            }
            _ => {
                parsed.inputs.push(arguments[cursor].clone());
            }
        }
        cursor += 1;
    }

    if parsed.pdf.page_from == Some(0) {
        return Err("--page-from must be a 1-based page number".to_owned());
    }
    if parsed.pdf.page_to == Some(0) {
        return Err("--page-to must be a 1-based page number".to_owned());
    }
    if let (Some(from), Some(to)) = (parsed.pdf.page_from, parsed.pdf.page_to) {
        if from > to {
            return Err("--page-from must be <= --page-to".to_owned());
        }
    }
    if parsed.pdf.split_pages.is_some() && parsed.target != OutputFormat::PDF_PAGES_ZIP {
        return Err(
            "--pdf-split-pages requires --to pdf-pages-zip (not plain PDF rewrite)".to_owned(),
        );
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
fn materialize_cli_input(
    input: ClassifiedInput,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BatchSource, String> {
    match input {
        ClassifiedInput::Token(token) => {
            materialize_paste_token(&token, cancel).map_err(|error| error.to_string())
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

    // Process-wide cancel flag installed once; each batch call resets it.
    let cancel = install_ctrl_c_handler();

    // Classify first (no network), confirm all network tokens, then download.
    let mut classified = Vec::with_capacity(inputs.len());
    for input in &inputs {
        classified.push(classify_cli_input(input)?);
    }
    confirm_network_urls(network_urls_from_classified(&classified), yes)?;
    for item in classified {
        let source = materialize_cli_input(item, Some(Arc::clone(&cancel)))?;
        queue.enqueue(source, &enqueue);
    }

    // Single input + -o path: pin the destination for that one item.
    if let Some(path) = single_output {
        if queue.len() == 1 {
            queue.items_mut()[0].destination = path;
        }
    }

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

/// Probe conversion engines. Exit codes are stable for scripts:
///
/// - `0` — at least one conversion engine is ready (usable partial install;
///   built-in Spreadsheet alone is enough)
/// - `1` — no conversion engines are ready (or doctor flags were invalid)
///
/// Built-in engines, optional engines, and PDF backends do not fail the exit
/// code when other tools are missing. Use `--script` and check `complete=true`
/// or individual `engine.*=ready` lines when a full install is required.
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

    Ok(ExitCode::from(report.exit_code().clamp(0, 255) as u8))
}

fn print_doctor_help() {
    println!(
        "Usage: shift-cli doctor [--script] [--quiet]\n\n\
         Probe conversion engines (built-in Spreadsheet; MarkItDown, Pandoc,\n\
         Defuddle, Docling, FFmpeg, sips on macOS) and PDF backends.\n\
         Reports installed/missing status, detected versions, and install hints.\n\n\
         Options:\n  \
         --script, -s   Emit key=value lines for scripts\n  \
         --quiet,  -q   Suppress output; rely on the exit code only\n\n\
         Exit codes:\n  \
         0  at least one conversion engine is ready (built-ins alone count)\n  \
         1  no conversion engines are ready\n\n\
         Built-in engines and optional engines (Defuddle, Docling) plus PDF\n\
         backends do not fail the exit code when other tools are missing. For a\n\
         full install gate, use `--script` and require `complete=true` or\n\
         specific `engine.<id>=ready` lines — not exit code alone.\n\n\
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

fn parse_positive_number(value: &OsStr, flag: &str, allow_zero: bool) -> Result<f64, String> {
    let raw = value
        .to_str()
        .ok_or_else(|| format!("{flag} value is not valid UTF-8"))?;
    let parsed = raw
        .parse::<f64>()
        .map_err(|_| format!("{flag} expects a number"))?;
    let in_range = parsed.is_finite() && (parsed > 0.0 || (allow_zero && parsed == 0.0));
    if in_range {
        Ok(parsed)
    } else if allow_zero {
        Err(format!("{flag} expects a non-negative number"))
    } else {
        Err(format!("{flag} expects a positive number"))
    }
}

fn parse_byte_size(value: &OsStr, flag: &str) -> Result<u64, String> {
    let raw = value.to_string_lossy();
    let normalized = raw.trim().to_ascii_lowercase().replace(' ', "");
    let (number, multiplier) = if let Some(value) = normalized.strip_suffix("gib") {
        (value, 1024_u64.pow(3))
    } else if let Some(value) = normalized.strip_suffix("gb") {
        (value, 1_000_000_000)
    } else if let Some(value) = normalized.strip_suffix("mib") {
        (value, 1024_u64.pow(2))
    } else if let Some(value) = normalized.strip_suffix("mb") {
        (value, 1_000_000)
    } else if let Some(value) = normalized.strip_suffix("kib") {
        (value, 1024)
    } else if let Some(value) = normalized.strip_suffix("kb") {
        (value, 1_000)
    } else if let Some(value) = normalized.strip_suffix('b') {
        (value, 1)
    } else {
        // A bare value is user-facing megabytes, matching the native app.
        (normalized.as_str(), 1_000_000)
    };
    let number = number
        .parse::<f64>()
        .map_err(|_| format!("{flag} expects a size such as 10MB (got `{raw}`)"))?;
    let bytes = number * multiplier as f64;
    if !bytes.is_finite() || bytes < 16.0 * 1024.0 || bytes > u64::MAX as f64 {
        return Err(format!(
            "{flag} must be a finite size of at least 16KiB (got `{raw}`)"
        ));
    }
    Ok(bytes.round() as u64)
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
         Fit to size (FFmpeg lossy media and sips JPG/JP2):\n  \
         --target-size <SIZE>    Fit supported output under SIZE (10MB, 750KiB;\n  \
                                 bare values are interpreted as MB)\n\n\
         PDF toolkit options (qpdf):\n  \
         --pdf-password <SECRET> Password for encrypted PDFs\n  \
         --pages <FROM-TO>       1-based inclusive page range (e.g. 2-5)\n  \
         --page-from <N>         1-based start page\n  \
         --page-to <N>           1-based end page\n  \
         --pdf-rotate <DEG>      Rotate selected pages by 90, 180, or 270\n  \
         --pdf-compression <MODE> preserve|lossless|smaller\n  \
         --pdf-linearize         Optimize PDF output for web delivery\n  \
         --pdf-split-pages <N>   Pages per PDF in pdf-pages-zip output\n\n\
         MarkItDown options:\n  \
         --keep-data-uris        Keep base64 data URIs in Markdown output\n\n\
         Pandoc options:\n  \
         --standalone            Produce a standalone document (-s)\n  \
         --toc                   Include a table of contents\n  \
         --citations             Parse @cite keys in Markdown (off by default)\n  \
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
         --docling-table-mode fast|accurate\n  \
         --docling-asr-model tiny|base|small|medium|large|turbo\n  \
         --docling-video-sampling fixed|scene\n  \
         --docling-video-frame-interval <SEC>\n  \
         --docling-video-cuts-per-minute <N>  0 = auto scene sensitivity\n  \
         --docling-video-prominence <N>       0 = auto scene sensitivity\n  \
         --docling-video-diarization / --no-docling-video-diarization\n\n\
         Image options (sips, macOS only):\n  \
         --sips-max-dimension <PX>  Fit inside PX x PX, preserving aspect\n  \
         --sips-quality balanced|high|small\n  \
         --sips-rotate <DEGREES>    Rotate clockwise\n  \
         --sips-flip horizontal|vertical\n  \
         --sips-strip-profile       Drop the embedded color profile\n\n\
         Spreadsheet options (values-only; cell text is preserved as written):\n  \
         --sheet <NAME>             Sheet name (exact, case-sensitive)\n  \
         --sheet-index <N>          Sheet index (1-based; ignored if --sheet set)\n\n\
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

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
            "SHIFT_SIPS_BIN",
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

        // All external engines missing: spreadsheet is still built-in and ready,
        // so doctor stays exit 0 (usable partial install).
        unsafe {
            for key in engine_vars {
                std::env::set_var(key, &missing);
            }
        }
        let empty = DiagnosticsReport::collect();
        assert!(
            empty.is_engine_ready("spreadsheet"),
            "spreadsheet must stay ready without external tools"
        );
        assert_eq!(empty.exit_code(), 0);
        assert_eq!(
            run(args(&["doctor", "--quiet"])).unwrap(),
            ExitCode::SUCCESS
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
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
            "--citations",
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
        assert!(parsed.pandoc.citations);
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
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    #[test]
    fn rejects_stdout_with_multi_input_batch() {
        let error = run(args(&["a.md", "b.md", "-t", "html", "--stdout"])).unwrap_err();
        assert!(error.contains("--stdout"), "{error}");
        assert!(
            error.contains("batch") || error.contains("output-dir"),
            "{error}"
        );
    }

    #[test]
    fn rejects_stdout_with_output_dir() {
        let error = run(args(&["a.md", "-t", "html", "--stdout", "-O", "/tmp/out"])).unwrap_err();
        assert!(
            error.contains("--stdout")
                && (error.contains("output path") || error.contains("batch")),
            "{error}"
        );
    }

    #[test]
    fn rejects_output_and_output_dir_together() {
        let error = run(args(&[
            "a.md", "-t", "html", "-o", "out.html", "-O", "/tmp/out",
        ]))
        .unwrap_err();
        assert!(
            error.contains("--output") && error.contains("--output-dir"),
            "{error}"
        );
    }

    #[test]
    fn rejects_unknown_output_format() {
        let error = run(args(&["file.md", "-t", "not-a-format"])).unwrap_err();
        assert!(
            error.contains("unknown output format") || error.contains("not-a-format"),
            "{error}"
        );
    }

    #[test]
    fn parse_ffmpeg_numeric_flags_and_rejects_invalid() {
        let parsed = parse_convert_args(&args(&[
            "clip.mp4",
            "--start",
            "1.5",
            "--duration",
            "10",
            "--frame",
            "0",
            "--audio-stream",
            "1",
            "--sample-rate",
            "48000",
            "--scale-width",
            "1280",
            "--encode",
            "reencode",
            "--quality",
            "high",
            "-t",
            "mp3",
        ]))
        .unwrap();
        assert_eq!(parsed.ffmpeg.start_secs, Some(1.5));
        assert_eq!(parsed.ffmpeg.duration_secs, Some(10.0));
        assert_eq!(parsed.ffmpeg.frame_secs, Some(0.0));
        assert_eq!(parsed.ffmpeg.audio_stream, Some(1));
        assert_eq!(parsed.ffmpeg.sample_rate_hz, Some(48000));
        assert_eq!(parsed.ffmpeg.scale_width, Some(1280));
        assert_eq!(parsed.ffmpeg.encode_mode, FfmpegEncodeMode::Reencode);
        assert_eq!(parsed.ffmpeg.quality, FfmpegQuality::High);

        let bad_start =
            parse_convert_args(&args(&["clip.mp4", "--start", "nope", "-t", "mp3"])).unwrap_err();
        assert!(bad_start.contains("--start"), "{bad_start}");
        assert!(
            bad_start.contains("seconds") || bad_start.contains("number"),
            "{bad_start}"
        );

        let bad_fps =
            parse_convert_args(&args(&["clip.mp4", "--fps", "x", "-t", "mp4"])).unwrap_err();
        assert!(bad_fps.contains("--fps"), "{bad_fps}");

        let bad_stream =
            parse_convert_args(&args(&["clip.mp4", "--audio-stream", "-1", "-t", "mp3"]))
                .unwrap_err();
        assert!(
            bad_stream.contains("--audio-stream") || bad_stream.contains("integer"),
            "{bad_stream}"
        );

        let bad_encode =
            parse_convert_args(&args(&["clip.mp4", "--encode", "turbo", "-t", "mp4"])).unwrap_err();
        assert!(
            bad_encode.contains("encode") || bad_encode.contains("turbo"),
            "{bad_encode}"
        );

        let bad_quality =
            parse_convert_args(&args(&["clip.mp4", "--quality", "max", "-t", "mp4"])).unwrap_err();
        assert!(
            bad_quality.contains("quality") || bad_quality.contains("max"),
            "{bad_quality}"
        );
    }

    #[test]
    fn parses_pdf_toolkit_flags_and_rejects_invalid_values() {
        let parsed = parse_convert_args(&args(&[
            "scan.pdf",
            "--to",
            "pdf-pages-zip",
            "--pages",
            "2-8",
            "--pdf-rotate",
            "90",
            "--pdf-compression",
            "smaller",
            "--pdf-linearize",
            "--pdf-split-pages",
            "2",
        ]))
        .unwrap();
        assert_eq!(parsed.target, OutputFormat::PDF_PAGES_ZIP);
        assert_eq!(parsed.pdf.page_from, Some(2));
        assert_eq!(parsed.pdf.page_to, Some(8));
        assert_eq!(parsed.pdf.rotate_degrees, Some(90));
        assert_eq!(parsed.pdf.compression, PdfCompression::Smaller);
        assert!(parsed.pdf.linearize);
        assert_eq!(parsed.pdf.split_pages, Some(2));

        for invalid in ["0", "45", "360"] {
            let error =
                parse_convert_args(&args(&["scan.pdf", "--pdf-rotate", invalid])).unwrap_err();
            assert!(error.contains("--pdf-rotate"), "{error}");
        }
        let error =
            parse_convert_args(&args(&["scan.pdf", "--pdf-compression", "maximum"])).unwrap_err();
        assert!(error.contains("compression"), "{error}");
        let error = parse_convert_args(&args(&["scan.pdf", "--pdf-split-pages", "0"])).unwrap_err();
        assert!(error.contains("at least 1"), "{error}");
        let error = parse_convert_args(&args(&[
            "scan.pdf",
            "--to",
            "pdf",
            "--pdf-split-pages",
            "2",
        ]))
        .unwrap_err();
        assert!(
            error.contains("pdf-pages-zip"),
            "split pages without ZIP target: {error}"
        );
    }

    #[test]
    fn parse_secs_and_u32_helpers() {
        assert_eq!(parse_secs(OsStr::new("2.5"), "--start").unwrap(), 2.5);
        assert_eq!(parse_secs(OsStr::new("0"), "--duration").unwrap(), 0.0);
        let err = parse_secs(OsStr::new("abc"), "--fps").unwrap_err();
        assert!(err.contains("--fps"), "{err}");

        assert_eq!(parse_u32(OsStr::new("42"), "--sample-rate").unwrap(), 42);
        let err = parse_u32(OsStr::new("3.5"), "--scale-width").unwrap_err();
        assert!(err.contains("--scale-width"), "{err}");
        let err = parse_u32(OsStr::new("-2"), "--audio-stream").unwrap_err();
        assert!(err.contains("--audio-stream"), "{err}");
    }

    #[test]
    fn parses_target_sizes_with_units_and_megabyte_default() {
        assert_eq!(
            parse_byte_size(OsStr::new("10MB"), "--target-size").unwrap(),
            10_000_000
        );
        assert_eq!(
            parse_byte_size(OsStr::new("1.5 MiB"), "--target-size").unwrap(),
            1_572_864
        );
        assert_eq!(
            parse_byte_size(OsStr::new("2"), "--target-size").unwrap(),
            2_000_000
        );
        assert!(parse_byte_size(OsStr::new("1KB"), "--target-size").is_err());

        let parsed =
            parse_convert_args(&args(&["clip.mp4", "--target-size", "25MB", "-t", "mp4"])).unwrap();
        assert_eq!(parsed.target_size_bytes, Some(25_000_000));
    }

    #[test]
    fn parse_rejects_zero_and_inverted_page_flags() {
        let zero_from =
            parse_convert_args(&args(&["doc.pdf", "--page-from", "0", "--page-to", "3"]))
                .unwrap_err();
        assert!(
            zero_from.contains("1-based") || zero_from.contains("page-from"),
            "{zero_from}"
        );

        let inverted =
            parse_convert_args(&args(&["doc.pdf", "--page-from", "9", "--page-to", "2"]))
                .unwrap_err();
        assert!(
            inverted.contains("page-from") && inverted.contains("page-to"),
            "{inverted}"
        );
    }

    #[test]
    fn parse_pages_rejects_zero_and_non_numeric() {
        let zero = parse_pages_range(OsStr::new("0-2"), "--pages").unwrap_err();
        assert!(zero.contains("1-based") || zero.contains("page"), "{zero}");

        let bad = parse_pages_range(OsStr::new("a-b"), "--pages").unwrap_err();
        assert!(bad.contains("--pages"), "{bad}");

        let empty = parse_pages_range(OsStr::new(""), "--pages").unwrap_err();
        assert!(empty.contains("--pages"), "{empty}");
    }

    #[test]
    fn preferred_module_promotes_via_build_registry() {
        let registry = build_registry(Some("pandoc")).unwrap();
        assert!(registry.has_module("pandoc"));
        // Unknown module is rejected with a helpful list.
        let error = match build_registry(Some("not-a-module")) {
            Ok(_) => panic!("expected unknown module error"),
            Err(message) => message,
        };
        assert!(error.contains("unknown module"), "{error}");
        assert!(error.contains("not-a-module"), "{error}");
    }

    #[test]
    fn default_output_path_unicode_stem_and_prepare_destination() {
        let dir = unique_temp("unicode-dest");
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("rapor-ç.html");
        std::fs::write(&source, b"<p>src</p>").unwrap();

        let dest = default_output_path(&source, OutputFormat::HTML);
        assert_eq!(dest, dir.join("rapor-ç.converted.html"));
        assert_ne!(dest, source);

        assert!(prepare_batch_destination(&dest, Some(&source), false).is_ok());
        // Existing destination still requires --force.
        std::fs::write(&dest, b"old").unwrap();
        let error = prepare_batch_destination(&dest, Some(&source), false).unwrap_err();
        assert!(error.to_string().contains("--force"), "{error}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn is_network_or_file_url_input_classifies_urls() {
        assert!(is_network_or_file_url_input(OsStr::new(
            "https://example.com/a"
        )));
        assert!(is_network_or_file_url_input(OsStr::new(
            "http://example.com"
        )));
        assert!(is_network_or_file_url_input(OsStr::new("file:///tmp/x.md")));
        assert!(is_network_or_file_url_input(OsStr::new(
            "FILE://localhost/tmp/x"
        )));
        assert!(!is_network_or_file_url_input(OsStr::new("report.pdf")));
        assert!(!is_network_or_file_url_input(OsStr::new("/tmp/notes.md")));
    }

    #[test]
    fn recursive_empty_directory_reports_no_convertible_files() {
        let dir = unique_temp("empty-recursive");
        std::fs::create_dir_all(&dir).unwrap();
        let error = run(args(&[
            dir.to_str().unwrap(),
            "--recursive",
            "-t",
            "markdown",
        ]))
        .unwrap_err();
        assert!(
            error.contains("no convertible") || error.contains("directory"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_multi_input_with_single_output_path() {
        let error = run(args(&["a.md", "b.md", "-t", "html", "-o", "out.html"])).unwrap_err();
        assert!(
            error.contains("output-dir") || error.contains("multiple inputs"),
            "{error}"
        );
    }

    #[test]
    fn parse_requires_value_for_value_taking_flags() {
        let missing_to = parse_convert_args(&args(&["file.md", "-t"])).unwrap_err();
        assert!(missing_to.contains("--to"), "{missing_to}");

        let missing_module = parse_convert_args(&args(&["file.md", "--module"])).unwrap_err();
        assert!(missing_module.contains("--module"), "{missing_module}");

        let missing_start = parse_convert_args(&args(&["clip.mp4", "--start"])).unwrap_err();
        assert!(missing_start.contains("--start"), "{missing_start}");
    }

    #[test]
    fn cli_accepted_format_ids_match_output_format_catalog() {
        // Every OutputFormat::ALL id must parse via the same FromStr used by -t/--to.
        for format in OutputFormat::ALL {
            let id = format.id();
            let parsed = parse_convert_args(&args(&["file.bin", "-t", id]))
                .unwrap_or_else(|e| panic!("CLI rejected catalog id {id}: {e}"));
            assert_eq!(parsed.target.id(), id);

            // Extension aliases that uniquely map should also work when they equal the id.
            let ext = format.extension();
            if ext != id && ext.parse::<OutputFormat>().map(|f| f.id()) == Ok(id) {
                let parsed = parse_convert_args(&args(&["file.bin", "-t", ext])).unwrap();
                assert_eq!(parsed.target.extension(), ext);
            }
        }

        // Documented aliases accepted by FromStr.
        for (alias, expected) in [
            ("md", "markdown"),
            ("jpeg", "jpg"),
            ("png-zip", "png-sequence-zip"),
            ("mpg", "mpeg"),
        ] {
            let parsed = parse_convert_args(&args(&["file.bin", "-t", alias])).unwrap();
            assert_eq!(parsed.target.id(), expected, "alias {alias}");
        }

        // Reject anything outside the catalog.
        for bad in ["not-a-format", "docx-plus", "audio", "video", ""] {
            // Empty -t value is a missing-value error; others are unknown format.
            if bad.is_empty() {
                continue;
            }
            let err = parse_convert_args(&args(&["file.bin", "-t", bad])).unwrap_err();
            assert!(
                err.contains("unknown output format") || err.contains(bad),
                "bad={bad:?} err={err}"
            );
        }
    }

    #[test]
    fn cli_module_ids_match_default_registry() {
        let registry = ConversionRegistry::default();
        let module_ids: Vec<_> = registry.modules().map(|m| m.id()).collect();
        #[cfg(target_os = "macos")]
        let expected = vec![
            "markitdown",
            "pandoc",
            "defuddle",
            "docling",
            "qpdf",
            "spreadsheet",
            "sips",
            "ffmpeg",
        ];
        #[cfg(not(target_os = "macos"))]
        let expected = vec![
            "markitdown",
            "pandoc",
            "defuddle",
            "docling",
            "qpdf",
            "spreadsheet",
            "ffmpeg",
        ];
        assert_eq!(module_ids, expected);

        for id in &module_ids {
            let built = build_registry(Some(id)).unwrap();
            // Preferred module is sorted first.
            assert_eq!(built.modules().next().unwrap().id(), *id);
            assert!(built.has_module(id));
        }

        let err = match build_registry(Some("not-a-module")) {
            Ok(_) => panic!("expected unknown module error"),
            Err(message) => message,
        };
        assert!(err.contains("unknown module"));
        for id in &module_ids {
            assert!(
                err.contains(id),
                "error should list known module {id}: {err}"
            );
        }
    }

    #[test]
    fn parse_to_flag_long_and_short_forms() {
        let gfm: OutputFormat = "gfm".parse().unwrap();
        let plain: OutputFormat = "plain".parse().unwrap();
        for (args_list, expected) in [
            (args(&["a.md", "-t", "html"]), OutputFormat::HTML),
            (args(&["a.md", "--to", "pdf"]), OutputFormat::PDF),
            (args(&["a.md", "-t", "mp3"]), OutputFormat::MP3),
            (
                args(&["a.md", "--to", "png-sequence-zip"]),
                OutputFormat::PNG_SEQUENCE_ZIP,
            ),
            (args(&["a.md", "-t", "gfm"]), gfm),
            (args(&["a.md", "-t", "plain"]), plain),
        ] {
            let parsed = parse_convert_args(&args_list).unwrap();
            assert_eq!(parsed.target, expected);
        }
    }

    #[test]
    fn parse_defuddle_and_markitdown_flags() {
        let parsed = parse_convert_args(&args(&[
            "https://example.com",
            "--frontmatter",
            "--lang",
            "de",
            "--keep-data-uris",
            "-t",
            "markdown",
            "--yes",
        ]))
        .unwrap();
        assert!(parsed.defuddle.frontmatter);
        assert_eq!(parsed.defuddle.lang.as_deref(), Some("de"));
        assert!(parsed.markitdown.keep_data_uris);
        assert!(parsed.yes);
    }

    #[test]
    fn parse_batch_subcommand_shape() {
        // `batch` as first token is treated as an input path unless it is a subcommand.
        // The CLI uses multi-input detection; ensure batch keyword still parses convert flags.
        let parsed = parse_convert_args(&args(&[
            "batch", "a.md", "b.md", "-t", "html", "-O", "/tmp/out", "--force",
        ]))
        .unwrap();
        assert!(parsed.force);
        assert_eq!(parsed.output_dir.as_deref(), Some(Path::new("/tmp/out")));
        assert!(parsed.inputs.len() >= 2);
        assert_eq!(parsed.target, OutputFormat::HTML);
    }

    #[test]
    fn parse_all_boolean_media_flags() {
        let parsed = parse_convert_args(&args(&[
            "clip.mp4",
            "--mute",
            "--normalize-audio",
            "--burn-subtitles",
            "--mono",
            "-t",
            "mp4",
        ]))
        .unwrap();
        assert!(parsed.ffmpeg.mute);
        assert!(parsed.ffmpeg.normalize_audio);
        assert!(parsed.ffmpeg.burn_subtitles);
        assert!(parsed.ffmpeg.mono);
    }

    #[test]
    fn parse_docling_ocr_and_table_mode_flags() {
        let parsed = parse_convert_args(&args(&[
            "scan.pdf",
            "--docling-ocr",
            "--docling-table-mode",
            "accurate",
            "--ocr-lang",
            "eng",
            "-t",
            "markdown",
        ]))
        .unwrap();
        assert!(parsed.docling.ocr);
        assert_eq!(parsed.docling.ocr_lang.as_deref(), Some("eng"));
        assert_eq!(parsed.target, OutputFormat::MARKDOWN);

        let parsed =
            parse_convert_args(&args(&["scan.pdf", "--no-docling-ocr", "-t", "html"])).unwrap();
        assert!(!parsed.docling.ocr);
    }

    #[test]
    fn formats_command_lists_every_registry_module() {
        // `formats` prints module lines; ensure it exits successfully and modules exist.
        assert!(run(args(&["formats"])).is_ok());
        let registry = ConversionRegistry::default();
        for module in registry.modules() {
            assert!(
                build_registry(Some(module.id())).is_ok(),
                "module {} from formats surface must be selectable",
                module.id()
            );
        }
    }

    #[test]
    fn version_and_help_flags() {
        assert!(run(args(&["--version"])).is_ok());
        assert!(run(args(&["version"])).is_ok());
        assert!(run(args(&["--help"])).is_ok());
        assert!(run(args(&["help"])).is_ok());
        assert!(run(args(&["-h"])).is_ok());
    }

    #[test]
    fn reject_unknown_format_across_aliases() {
        for bad in [
            "docx2",
            "markdownn",
            "mp33",
            "text",
            "application/pdf",
            "image/png",
        ] {
            let err = run(args(&["file.md", "-t", bad])).unwrap_err();
            assert!(
                err.contains("unknown output format") || err.contains(bad),
                "bad={bad} err={err}"
            );
        }
    }

    #[test]
    fn parse_subtitle_stream_and_frame_interval() {
        let parsed = parse_convert_args(&args(&[
            "clip.mkv",
            "--subtitle-stream",
            "2",
            "--frame-interval",
            "1.25",
            "-t",
            "srt",
        ]))
        .unwrap();
        assert_eq!(parsed.ffmpeg.subtitle_stream, Some(2));
        assert_eq!(parsed.ffmpeg.frame_interval_secs, Some(1.25));
        assert_eq!(parsed.target, OutputFormat::SRT);
    }

    #[test]
    fn network_confirm_skips_file_only_batches() {
        let sources = [
            BatchSource::File(PathBuf::from("a.pdf")),
            BatchSource::File(PathBuf::from("b.pdf")),
        ];
        assert!(confirm_network_sources(&sources, false).is_ok());
    }

    #[test]
    fn doctor_quiet_vs_verbose_acceptance() {
        // Quiet: exit code only (no panic / argument rejection).
        let quiet = run(args(&["doctor", "--quiet"])).unwrap();
        assert!(
            quiet == ExitCode::SUCCESS || quiet == ExitCode::from(1),
            "quiet doctor exit"
        );
        let quiet_short = run(args(&["doctor", "-q"])).unwrap();
        assert!(
            quiet_short == ExitCode::SUCCESS || quiet_short == ExitCode::from(1),
            "quiet -q doctor exit"
        );

        // Default (verbose text) doctor is accepted.
        let verbose = run(args(&["doctor"])).unwrap();
        assert!(
            verbose == ExitCode::SUCCESS || verbose == ExitCode::from(1),
            "default doctor exit"
        );

        // Script mode without quiet.
        assert!(run(args(&["doctor", "--script"])).is_ok());
        assert!(run(args(&["doctor", "-s"])).is_ok());

        // Combinations.
        assert!(run(args(&["doctor", "--script", "--quiet"])).is_ok());
        assert!(run(args(&["doctor", "-s", "-q"])).is_ok());
        assert!(run(args(&["doctor", "--help"])).is_ok());
        assert!(run(args(&["doctor", "-h"])).is_ok());
    }

    #[test]
    fn recursive_multi_dir_expands_convertible_files() {
        let root = unique_temp("multi-dir");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("one.md"), b"# a\n").unwrap();
        std::fs::write(b.join("two.html"), b"<p>b</p>").unwrap();
        // Nested unsupported file should be ignored by expansion.
        std::fs::write(a.join("skip.xyz"), b"nope").unwrap();

        let expanded =
            resolve_cli_inputs(args(&[a.to_str().unwrap(), b.to_str().unwrap()]), true).unwrap();
        let expanded_paths: Vec<PathBuf> = expanded.into_iter().map(PathBuf::from).collect();
        assert!(
            expanded_paths.iter().any(|p| p.ends_with("one.md")),
            "expected one.md in {expanded_paths:?}"
        );
        assert!(
            expanded_paths.iter().any(|p| p.ends_with("two.html")),
            "expected two.html in {expanded_paths:?}"
        );
        assert!(
            !expanded_paths.iter().any(|p| p.ends_with("skip.xyz")),
            "unsupported extension should not expand: {expanded_paths:?}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn batch_with_force_parses_and_prepare_destination_honors_it() {
        let parsed = parse_convert_args(&args(&[
            "batch",
            "a.md",
            "b.md",
            "-t",
            "html",
            "-O",
            "/tmp/shift-batch-out",
            "--force",
        ]))
        .unwrap();
        assert!(parsed.batch_explicit);
        assert!(parsed.force);
        assert_eq!(
            parsed.output_dir.as_deref(),
            Some(Path::new("/tmp/shift-batch-out"))
        );
        assert_eq!(parsed.inputs.len(), 2);

        let dir = unique_temp("batch-force");
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("page.md");
        let dest = dir.join("page.converted.html");
        std::fs::write(&source, b"# hi\n").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        assert!(prepare_batch_destination(&dest, Some(&source), true).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn allow_private_urls_flag_parses_and_defaults_off() {
        let off = parse_convert_args(&args(&["https://example.com", "-t", "markdown"])).unwrap();
        assert!(!off.allow_private_urls);

        let on = parse_convert_args(&args(&[
            "https://example.com",
            "--allow-private-urls",
            "-t",
            "html",
            "--yes",
        ]))
        .unwrap();
        assert!(on.allow_private_urls);
        assert!(on.yes);
        assert_eq!(on.target, OutputFormat::HTML);
    }

    #[test]
    fn convert_with_module_each_known_id() {
        let registry = ConversionRegistry::default();
        let module_ids: Vec<_> = registry.modules().map(|m| m.id()).collect();
        for id in &module_ids {
            let parsed = parse_convert_args(&args(&["notes.md", "-t", "markdown", "--module", id]))
                .unwrap_or_else(|e| panic!("module {id}: {e}"));
            assert_eq!(parsed.preferred_module.as_deref(), Some(*id));
            let built = build_registry(Some(id)).unwrap();
            assert_eq!(built.modules().next().unwrap().id(), *id);
        }
    }

    #[test]
    fn meta_commands_do_not_require_inputs_or_stdin() {
        // These paths never call parse_convert_args / confirm_network_urls.
        for argv in [
            args(&["--help"]),
            args(&["help"]),
            args(&["-h"]),
            args(&["--version"]),
            args(&["version"]),
            args(&["formats"]),
            args(&["doctor", "--quiet"]),
            args(&["doctor", "--help"]),
        ] {
            assert!(
                run(argv.clone()).is_ok(),
                "meta command should not require inputs: {argv:?}"
            );
        }
    }

    #[test]
    fn long_help_surfaces_accepted() {
        assert_eq!(run(args(&["--help"])).unwrap(), ExitCode::SUCCESS);
        assert_eq!(run(args(&["help"])).unwrap(), ExitCode::SUCCESS);
        assert_eq!(run(args(&["doctor", "--help"])).unwrap(), ExitCode::SUCCESS);
        // Empty args print help but exit failure (documented usage path).
        assert_eq!(run(args(&[])).unwrap(), ExitCode::FAILURE);
    }

    #[test]
    fn rejects_empty_to_value() {
        let err = parse_convert_args(&args(&["file.md", "-t", ""])).unwrap_err();
        assert!(
            err.contains("unknown output format")
                || err.contains("format")
                || err.contains("empty"),
            "empty -t should be rejected: {err}"
        );

        let err = run(args(&["file.md", "--to", ""])).unwrap_err();
        assert!(
            err.contains("unknown output format") || err.contains("format"),
            "empty --to via run: {err}"
        );
    }

    #[test]
    fn pages_with_only_from_or_only_to_are_allowed() {
        let only_from =
            parse_convert_args(&args(&["doc.pdf", "--page-from", "3", "-t", "markdown"])).unwrap();
        assert_eq!(only_from.pdf.page_from, Some(3));
        assert_eq!(only_from.pdf.page_to, None);

        let only_to =
            parse_convert_args(&args(&["doc.pdf", "--page-to", "9", "-t", "markdown"])).unwrap();
        assert_eq!(only_to.pdf.page_from, None);
        assert_eq!(only_to.pdf.page_to, Some(9));

        // Both still work together.
        let both = parse_convert_args(&args(&[
            "doc.pdf",
            "--page-from",
            "2",
            "--page-to",
            "4",
            "-t",
            "html",
        ]))
        .unwrap();
        assert_eq!(both.pdf.page_from, Some(2));
        assert_eq!(both.pdf.page_to, Some(4));
    }

    #[test]
    fn parse_convert_subcommand_does_not_set_batch_explicit() {
        let parsed = parse_convert_args(&args(&[
            "convert", "notes.md", "-t", "html", "-o", "out.html",
        ]))
        .unwrap();
        assert!(!parsed.batch_explicit);
        assert_eq!(parsed.inputs, args(&["notes.md"]));
        assert_eq!(parsed.target, OutputFormat::HTML);
        assert_eq!(parsed.output.as_deref(), Some(Path::new("out.html")));
    }

    #[test]
    fn parse_end_of_options_allows_dash_prefixed_inputs() {
        let parsed =
            parse_convert_args(&args(&["-t", "markdown", "--", "-weird-name.md", "--also"]))
                .unwrap();
        assert_eq!(parsed.inputs, args(&["-weird-name.md", "--also"]));
        assert_eq!(parsed.target, OutputFormat::MARKDOWN);
    }

    #[test]
    fn parse_short_flag_forms_for_io_and_yes_verbose() {
        let parsed =
            parse_convert_args(&args(&["a.md", "-o", "out.md", "-y", "-v", "-t", "html"])).unwrap();
        assert_eq!(parsed.output.as_deref(), Some(Path::new("out.md")));
        assert!(parsed.yes);
        assert!(parsed.verbose);
        assert_eq!(parsed.target, OutputFormat::HTML);

        let parsed = parse_convert_args(&args(&[
            "a.md",
            "b.md",
            "-O",
            "/tmp/batch-out",
            "-t",
            "pdf",
        ]))
        .unwrap();
        assert_eq!(
            parsed.output_dir.as_deref(),
            Some(Path::new("/tmp/batch-out"))
        );
        assert_eq!(parsed.inputs.len(), 2);
    }

    #[test]
    fn parse_all_docling_cli_flags_including_images_and_tables() {
        for (mode_str, mode) in [
            ("placeholder", DoclingImageExportMode::Placeholder),
            ("embedded", DoclingImageExportMode::Embedded),
            ("referenced", DoclingImageExportMode::Referenced),
            ("embed", DoclingImageExportMode::Embedded),
            ("refs", DoclingImageExportMode::Referenced),
        ] {
            let parsed = parse_convert_args(&args(&[
                "scan.pdf",
                "--docling-images",
                mode_str,
                "-t",
                "markdown",
            ]))
            .unwrap();
            assert_eq!(
                parsed.docling.image_export_mode, mode,
                "mode_str={mode_str}"
            );
        }

        let tables_on =
            parse_convert_args(&args(&["scan.pdf", "--docling-tables", "-t", "html"])).unwrap();
        assert!(tables_on.docling.tables);

        let tables_off =
            parse_convert_args(&args(&["scan.pdf", "--no-docling-tables", "-t", "html"])).unwrap();
        assert!(!tables_off.docling.tables);

        let combo = parse_convert_args(&args(&[
            "scan.pdf",
            "--docling-images",
            "referenced",
            "--no-docling-ocr",
            "--docling-tables",
            "--docling-table-mode",
            "accurate",
            "--ocr-lang",
            "eng+fra",
            "-t",
            "plain",
        ]))
        .unwrap();
        assert_eq!(
            combo.docling.image_export_mode,
            DoclingImageExportMode::Referenced
        );
        assert!(!combo.docling.ocr);
        assert!(combo.docling.tables);
        assert_eq!(combo.docling.table_mode, DoclingTableMode::Accurate);
        assert_eq!(combo.docling.ocr_lang.as_deref(), Some("eng+fra"));
        assert_eq!(combo.target, "plain".parse::<OutputFormat>().unwrap());

        let asr_and_video = parse_convert_args(&args(&[
            "clip.mp4",
            "-t",
            "transcript",
            "--docling-asr-model",
            "turbo",
            "--docling-video-sampling",
            "scene",
            "--docling-video-frame-interval",
            "2.5",
            "--docling-video-cuts-per-minute",
            "4",
            "--docling-video-prominence",
            "0.02",
            "--docling-video-diarization",
        ]))
        .unwrap();
        assert_eq!(asr_and_video.target, OutputFormat::TRANSCRIPT);
        assert_eq!(asr_and_video.docling.asr_model, DoclingAsrModel::Turbo);
        assert_eq!(
            asr_and_video.docling.video_sampling_mode,
            DoclingVideoSamplingMode::Scene
        );
        assert_eq!(asr_and_video.docling.video_frame_interval_secs, 2.5);
        assert_eq!(asr_and_video.docling.video_cuts_per_minute, 4.0);
        assert_eq!(asr_and_video.docling.video_prominence, 0.02);
        assert!(asr_and_video.docling.video_diarization);

        let bad_images =
            parse_convert_args(&args(&["scan.pdf", "--docling-images", "huge"])).unwrap_err();
        assert!(
            bad_images.contains("image export") || bad_images.contains("huge"),
            "{bad_images}"
        );
        let bad_table_mode =
            parse_convert_args(&args(&["scan.pdf", "--docling-table-mode", "turbo"])).unwrap_err();
        assert!(
            bad_table_mode.contains("table mode") || bad_table_mode.contains("turbo"),
            "{bad_table_mode}"
        );
        for (flag, value) in [
            ("--docling-video-frame-interval", "0"),
            ("--docling-video-cuts-per-minute", "-1"),
            ("--docling-video-prominence", "NaN"),
        ] {
            let error = parse_convert_args(&args(&["clip.mp4", flag, value])).unwrap_err();
            assert!(error.contains("expects"), "{flag}: {error}");
        }
    }

    #[test]
    fn parse_spreadsheet_sheet_flags() {
        let by_name =
            parse_convert_args(&args(&["book.xlsx", "--sheet", "Summary", "-t", "csv"])).unwrap();
        assert_eq!(by_name.spreadsheet.sheet_name.as_deref(), Some("Summary"));
        assert_eq!(by_name.target, OutputFormat::CSV);

        let by_index =
            parse_convert_args(&args(&["book.xlsx", "--sheet-index", "2", "-t", "tsv"])).unwrap();
        assert_eq!(by_index.spreadsheet.sheet_index, Some(2));
        assert_eq!(by_index.target, OutputFormat::TSV);

        let plain = parse_convert_args(&args(&["book.xlsx", "-t", "csv"])).unwrap();
        assert!(plain.spreadsheet.is_default());
    }

    #[test]
    fn parse_sips_image_flags() {
        let parsed = parse_convert_args(&args(&[
            "IMG_0001.HEIC",
            "--sips-max-dimension",
            "1024",
            "--sips-quality",
            "small",
            "--sips-rotate",
            "90",
            "--sips-flip",
            "vertical",
            "--sips-strip-profile",
            "-t",
            "jpg",
        ]))
        .unwrap();
        assert_eq!(parsed.sips.max_dimension, Some(1024));
        assert_eq!(parsed.sips.quality, SipsQuality::Small);
        assert_eq!(parsed.sips.rotate_degrees, Some(90));
        assert_eq!(parsed.sips.flip, Some(SipsFlip::Vertical));
        assert!(parsed.sips.strip_color_profile);
        assert_eq!(parsed.target, OutputFormat::JPG);

        // Defaults stay inert so unrelated conversions are unaffected.
        let plain = parse_convert_args(&args(&["a.png", "-t", "jpg"])).unwrap();
        assert!(plain.sips.is_default());

        for (argv, needle) in [
            (args(&["a.png", "--sips-quality", "max"]), "quality"),
            (args(&["a.png", "--sips-flip", "diagonal"]), "flip"),
            (
                args(&["a.png", "--sips-max-dimension", "big"]),
                "--sips-max-dimension",
            ),
            (args(&["a.png", "--sips-rotate", "-90"]), "--sips-rotate"),
        ] {
            let err = parse_convert_args(&argv).unwrap_err();
            assert!(err.contains(needle), "argv={argv:?} error={err}");
        }

        for (argv, needle) in [
            (
                args(&["a.png", "--sips-max-dimension"]),
                "--sips-max-dimension",
            ),
            (args(&["a.png", "--sips-quality"]), "--sips-quality"),
            (args(&["a.png", "--sips-rotate"]), "--sips-rotate"),
            (args(&["a.png", "--sips-flip"]), "--sips-flip"),
            (args(&["a.xlsx", "--sheet"]), "--sheet"),
            (args(&["a.xlsx", "--sheet-index"]), "--sheet-index"),
            (args(&["a.xlsx", "--sheet-index", "0"]), "1-based"),
        ] {
            let err = parse_convert_args(&argv).unwrap_err();
            assert!(err.contains(needle), "argv={argv:?} error={err}");
        }
    }

    #[test]
    fn parse_pandoc_standalone_toc_and_pdf_engine() {
        let parsed = parse_convert_args(&args(&[
            "notes.md",
            "--standalone",
            "--toc",
            "--pdf-engine",
            "xelatex",
            "--citations",
            "-t",
            "pdf",
        ]))
        .unwrap();
        assert!(parsed.pandoc.standalone);
        assert!(parsed.pandoc.toc);
        assert!(parsed.pandoc.citations);
        assert_eq!(parsed.pandoc.pdf_engine.as_deref(), Some("xelatex"));
        assert_eq!(parsed.target, OutputFormat::PDF);
    }

    #[test]
    fn parse_multi_module_flag_combo_does_not_conflict() {
        // Engine knobs are independent; setting several families at once is allowed.
        // (There is no CLI rejection of "conflicting module flags".)
        let parsed = parse_convert_args(&args(&[
            "mixed.bin",
            "-t",
            "markdown",
            "--module",
            "docling",
            "--keep-data-uris",
            "--standalone",
            "--toc",
            "--frontmatter",
            "--lang",
            "en",
            "--docling-images",
            "embedded",
            "--no-docling-ocr",
            "--no-docling-tables",
            "--docling-table-mode",
            "fast",
            "--ocr-lang",
            "eng",
            "--pdf-password",
            "secret",
            "--pages",
            "1-2",
            "--mute",
            "--mono",
            "--encode",
            "copy",
            "--quality",
            "small",
            "--verbose",
            "--progress",
            "--force",
            "-y",
        ]))
        .unwrap();
        assert_eq!(parsed.preferred_module.as_deref(), Some("docling"));
        assert!(parsed.markitdown.keep_data_uris);
        assert!(parsed.pandoc.standalone && parsed.pandoc.toc);
        assert!(parsed.defuddle.frontmatter);
        assert_eq!(parsed.defuddle.lang.as_deref(), Some("en"));
        assert_eq!(
            parsed.docling.image_export_mode,
            DoclingImageExportMode::Embedded
        );
        assert!(!parsed.docling.ocr && !parsed.docling.tables);
        assert_eq!(parsed.pdf.password.as_deref(), Some("secret"));
        assert_eq!(parsed.pdf.page_from, Some(1));
        assert_eq!(parsed.pdf.page_to, Some(2));
        assert!(parsed.ffmpeg.mute && parsed.ffmpeg.mono);
        assert_eq!(parsed.ffmpeg.encode_mode, FfmpegEncodeMode::PreferCopy);
        assert_eq!(parsed.ffmpeg.quality, FfmpegQuality::Small);
        assert!(parsed.verbose && parsed.progress && parsed.force && parsed.yes);
    }

    #[test]
    fn parse_requires_values_for_remaining_flags() {
        for (argv, needle) in [
            (args(&["a.md", "--output"]), "--output"),
            (args(&["a.md", "-o"]), "--output"),
            (args(&["a.md", "--output-dir"]), "--output-dir"),
            (args(&["a.md", "-O"]), "--output-dir"),
            (args(&["a.md", "--docling-images"]), "--docling-images"),
            (
                args(&["a.md", "--docling-table-mode"]),
                "--docling-table-mode",
            ),
            (args(&["a.md", "--ocr-lang"]), "--ocr-lang"),
            (args(&["a.md", "--pdf-password"]), "--pdf-password"),
            (args(&["a.md", "--page-from"]), "--page-from"),
            (args(&["a.md", "--page-to"]), "--page-to"),
            (args(&["a.md", "--pages"]), "--pages"),
            (args(&["a.md", "--pdf-engine"]), "--pdf-engine"),
            (args(&["a.md", "--reference-doc"]), "--reference-doc"),
            (args(&["a.md", "--lang"]), "--lang"),
            (args(&["clip.mp4", "--duration"]), "--duration"),
            (args(&["clip.mp4", "--frame"]), "--frame"),
            (args(&["clip.mp4", "--frame-interval"]), "--frame-interval"),
            (
                args(&["clip.mp4", "--subtitle-stream"]),
                "--subtitle-stream",
            ),
            (args(&["clip.mp4", "--encode"]), "--encode"),
            (args(&["clip.mp4", "--quality"]), "--quality"),
            (args(&["clip.mp4", "--sample-rate"]), "--sample-rate"),
            (args(&["clip.mp4", "--scale-width"]), "--scale-width"),
            (args(&["clip.mp4", "--fps"]), "--fps"),
            (args(&["clip.mp4", "--audio-stream"]), "--audio-stream"),
        ] {
            let err = parse_convert_args(&argv).unwrap_err();
            assert!(
                err.contains(needle),
                "argv={argv:?} expected needle {needle:?} in {err}"
            );
        }
    }

    #[test]
    fn rejects_zero_page_to() {
        let err = parse_convert_args(&args(&["doc.pdf", "--page-to", "0", "-t", "markdown"]))
            .unwrap_err();
        assert!(err.contains("1-based") || err.contains("page-to"), "{err}");
    }

    #[test]
    fn recursive_mixes_files_dirs_and_preserves_urls() {
        let root = unique_temp("mix-recursive");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("inside.md"), b"# inside\n").unwrap();
        let loose = root.join("loose.html");
        std::fs::write(&loose, b"<p>loose</p>").unwrap();
        // Empty dir alone would error; mixed with a file it should expand the dir.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        let expanded = resolve_cli_inputs(
            args(&[
                loose.to_str().unwrap(),
                nested.to_str().unwrap(),
                "https://example.com/page",
                "file:///tmp/local.md",
            ]),
            true,
        )
        .unwrap();
        let paths: Vec<String> = expanded
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            paths.iter().any(|p| p.ends_with("loose.html")),
            "loose file kept: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("inside.md")),
            "nested expand: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "https://example.com/page"),
            "url preserved: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "file:///tmp/local.md"),
            "file url preserved: {paths:?}"
        );

        // Empty directory among other inputs still fails when that dir expands to nothing.
        let err = resolve_cli_inputs(
            args(&[loose.to_str().unwrap(), empty.to_str().unwrap()]),
            true,
        )
        .unwrap_err();
        assert!(
            err.contains("no convertible") || err.contains("empty"),
            "{err}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recursive_keeps_unexpandable_file_path() {
        let dir = unique_temp("keep-file");
        std::fs::create_dir_all(&dir).unwrap();
        let weird = dir.join("no-extension");
        std::fs::write(&weird, b"data").unwrap();
        let expanded = resolve_cli_inputs(args(&[weird.to_str().unwrap()]), true).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(PathBuf::from(&expanded[0]), weird);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn doctor_script_keys_are_stable_across_env() {
        use shift_core::conversion::DiagnosticsReport;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp("doctor-keys");
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing-bin");
        let engine_vars = [
            "SHIFT_MARKITDOWN_BIN",
            "SHIFT_PANDOC_BIN",
            "SHIFT_DEFUDDLE_BIN",
            "SHIFT_DOCLING_BIN",
            "SHIFT_SIPS_BIN",
            "SHIFT_FFMPEG_BIN",
        ];
        unsafe {
            for key in engine_vars {
                std::env::set_var(key, &missing);
            }
            std::env::remove_var("SHIFT_PDF_ENGINE");
        }

        let script = DiagnosticsReport::collect().render_script();
        // Stable key set scripts can depend on regardless of readiness.
        for key in [
            "engine.markitdown=",
            "engine.pandoc=",
            "engine.defuddle=",
            "engine.docling=",
            "engine.ffmpeg=",
            "pdf.selected=",
            "pdf.ready=",
            "healthy=",
            "complete=",
            "exit_code=",
        ] {
            assert!(
                script.contains(key),
                "doctor --script missing stable key {key:?} in:\n{script}"
            );
        }
        // Each engine line carries version= and path= fields.
        for engine in [
            "markitdown",
            "pandoc",
            "defuddle",
            "docling",
            "spreadsheet",
            "ffmpeg",
        ] {
            let line = script
                .lines()
                .find(|l| l.starts_with(&format!("engine.{engine}=")))
                .unwrap_or_else(|| panic!("missing engine.{engine} line"));
            assert!(
                line.contains("version=") && line.contains("path="),
                "engine line shape: {line}"
            );
            assert!(
                line.contains("=missing") || line.contains("=ready"),
                "readiness label on {line}"
            );
        }

        // --doctor alias behaves like doctor.
        assert!(run(args(&["--doctor", "--quiet"])).is_ok());

        unsafe {
            for key in engine_vars {
                std::env::remove_var(key);
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn batch_subcommand_single_input_with_output_path_parses() {
        let parsed = parse_convert_args(&args(&[
            "batch",
            "only.md",
            "-t",
            "html",
            "-o",
            "/tmp/only.html",
            "--force",
        ]))
        .unwrap();
        assert!(parsed.batch_explicit);
        assert_eq!(parsed.inputs, args(&["only.md"]));
        assert_eq!(parsed.output.as_deref(), Some(Path::new("/tmp/only.html")));
        assert!(parsed.force);
        // Explicit batch + single -o is allowed at parse time; run path pins dest.
    }

    #[test]
    fn batch_explicit_without_inputs_fails_at_parse() {
        let err =
            parse_convert_args(&args(&["batch", "-t", "html", "-O", "/tmp/out"])).unwrap_err();
        assert!(err.contains("missing input"), "{err}");
    }

    #[test]
    fn classify_page_url_local_path_and_multi_token_rejection() {
        match classify_cli_input(OsStr::new("https://example.com/article")).unwrap() {
            ClassifiedInput::Token(PasteToken::PageUrl(url)) => {
                assert_eq!(url, "https://example.com/article");
            }
            other => panic!("expected page url, got {other:?}"),
        }
        match classify_cli_input(OsStr::new("/tmp/notes.md")).unwrap() {
            ClassifiedInput::Token(PasteToken::LocalPath(path)) => {
                assert_eq!(path, Path::new("/tmp/notes.md"));
            }
            ClassifiedInput::Path(path) => {
                assert_eq!(path, Path::new("/tmp/notes.md"));
            }
            other => panic!("expected local path, got {other:?}"),
        }
        // Multiple tokens in one argument are rejected.
        let err = classify_cli_input(OsStr::new("a.md b.md")).unwrap_err();
        assert!(
            err.contains("single path") || err.contains("multiple"),
            "{err}"
        );
    }

    #[test]
    fn materialize_path_variant_and_network_urls_list() {
        // ClassifiedInput::Path skips existence checks (non-UTF-8 / unclassified tokens).
        let source =
            materialize_cli_input(ClassifiedInput::Path(PathBuf::from("/tmp/x.pdf")), None)
                .unwrap();
        assert_eq!(source.as_file(), Some(Path::new("/tmp/x.pdf")));

        let items = [
            classify_cli_input(OsStr::new("https://example.com/a")).unwrap(),
            classify_cli_input(OsStr::new("/local.md")).unwrap(),
            classify_cli_input(OsStr::new("https://cdn.example.com/f.pdf")).unwrap(),
        ];
        let urls = network_urls_from_classified(&items);
        assert_eq!(
            urls,
            vec!["https://example.com/a", "https://cdn.example.com/f.pdf"]
        );
    }

    #[test]
    fn parse_pages_half_open_and_whitespace() {
        let err = parse_pages_range(OsStr::new("2-"), "--pages").unwrap_err();
        assert!(err.contains("FROM-TO") || err.contains("--pages"), "{err}");
        let err = parse_pages_range(OsStr::new("-5"), "--pages").unwrap_err();
        assert!(err.contains("FROM-TO") || err.contains("--pages"), "{err}");
        let (from, to) = parse_pages_range(OsStr::new(" 3 - 7 "), "--pages").unwrap();
        assert_eq!(from, Some(3));
        assert_eq!(to, Some(7));
    }

    #[cfg(unix)]
    #[test]
    fn convert_stdout_single_file_happy_path_with_fake() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp("stdout-happy");
        std::fs::create_dir_all(&dir).unwrap();
        let pandoc = dir.join("fake-pandoc");
        write_fake_pandoc(&pandoc);
        let input = dir.join("page.html");
        std::fs::write(&input, b"<p>hello</p>").unwrap();

        unsafe {
            std::env::set_var("SHIFT_PANDOC_BIN", &pandoc);
        }

        // Happy path: single input + --stdout + preferred module.
        let result = run(args(&[
            "convert",
            input.to_str().unwrap(),
            "-t",
            "markdown",
            "--stdout",
            "--module",
            "pandoc",
            "-v",
        ]));

        unsafe {
            std::env::remove_var("SHIFT_PANDOC_BIN");
        }

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        // Sibling default path must not be created under --stdout.
        assert!(!dir.join("page.md").exists());
        assert!(!dir.join("page.converted.md").exists());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn batch_single_file_with_output_dir_forces_batch_path() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp("batch-one");
        std::fs::create_dir_all(&dir).unwrap();
        let pandoc = dir.join("fake-pandoc");
        write_fake_pandoc(&pandoc);
        let input = dir.join("solo.html");
        std::fs::write(&input, b"<p>solo</p>").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        unsafe {
            std::env::set_var("SHIFT_PANDOC_BIN", &pandoc);
        }

        let result = run(args(&[
            input.to_str().unwrap(),
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

        assert!(result.is_ok(), "{result:?}");
        // Batch destination lands under -O even for a single input.
        let any_out = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.path().is_file());
        assert!(any_out, "expected an output file under {}", out.display());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn non_recursive_rejects_each_directory_input() {
        let root = unique_temp("non-rec");
        let a = root.join("a");
        std::fs::create_dir_all(&a).unwrap();
        let err = resolve_cli_inputs(args(&[a.to_str().unwrap(), "file.md"]), false).unwrap_err();
        assert!(
            err.contains("directory") && err.contains("--recursive"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
