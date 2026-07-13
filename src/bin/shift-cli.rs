use shift_core::conversion::{
    ConversionOptions, ConversionRegistry, FfmpegEncodeMode, FfmpegOptions, FfmpegQuality,
    OutputFormat, default_output_path, looks_like_url, paths_refer_to_same_file,
};
use shift_core::preferences::load_module_priority;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

fn main() {
    if let Err(error) = run(std::env::args_os().skip(1).collect()) {
        eprintln!("shift-cli: {error}");
        std::process::exit(1);
    }
}

fn run(arguments: Vec<OsString>) -> Result<(), String> {
    if arguments.is_empty() {
        print_help();
        return Ok(());
    }

    if arguments.len() == 1 {
        match arguments[0].to_string_lossy().as_ref() {
            "-h" | "--help" | "help" => {
                print_help();
                return Ok(());
            }
            "formats" => {
                print_formats();
                return Ok(());
            }
            _ => {}
        }
    }

    let mut cursor = 0;
    if arguments.first().is_some_and(|value| value == "convert") {
        cursor += 1;
    }

    let input = arguments
        .get(cursor)
        .filter(|value| !value.to_string_lossy().starts_with('-'))
        .cloned()
        .ok_or_else(|| "missing input file or URL (try `shift-cli --help`)".to_owned())?;
    cursor += 1;

    let mut output = None;
    let mut stdout = false;
    let mut force = false;
    let mut target = OutputFormat::MARKDOWN;
    let mut preferred_module: Option<String> = None;
    let mut ffmpeg = FfmpegOptions::default();
    while cursor < arguments.len() {
        match arguments[cursor].to_string_lossy().as_ref() {
            "-o" | "--output" => {
                cursor += 1;
                output = Some(
                    arguments
                        .get(cursor)
                        .map(PathBuf::from)
                        .ok_or_else(|| "--output requires a path".to_owned())?,
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
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        cursor += 1;
    }

    if stdout && output.is_some() {
        return Err("use either --stdout or --output, not both".to_owned());
    }

    let registry = ConversionRegistry::default();
    let registry = if let Some(module) = preferred_module.as_ref() {
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
        registry.with_priority(&[module])
    } else {
        registry.with_priority(&load_module_priority())
    };

    let options = ConversionOptions { ffmpeg };
    let input_url = url_input(&input);
    let artifact = if let Some(url) = input_url {
        registry
            .convert_url_with_options(url, target, &options)
            .map_err(|error| error.to_string())?
    } else {
        registry
            .convert_to_with_options(PathBuf::from(&input), target, &options)
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
                default_output_path(PathBuf::from(&input).as_path(), target)
            }
        });
        let source_path = input_url.is_none().then(|| PathBuf::from(&input));
        prepare_destination(&destination, source_path.as_deref(), force)?;
        artifact
            .write_to(&destination)
            .map_err(|error| error.to_string())?;
        println!("{}", destination.display());
    }

    Ok(())
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
         shift-cli convert <INPUT|URL> [-t <FORMAT>] [-o <OUTPUT>] [--stdout] [--force]\n  \
         shift-cli formats\n\n\
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
         Use `shift-cli formats` to list every installed conversion capability.\n\
         If no output is supplied, Shift writes beside the source (or the current directory for URLs).\n\
         Existing outputs require --force. The source file is never overwritten."
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
            error.contains("--stdout") && error.contains("--output"),
            "{error}"
        );
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
