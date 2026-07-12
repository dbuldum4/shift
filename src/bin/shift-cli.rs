use shift_core::conversion::{
    ConversionRegistry, OutputFormat, default_output_path, looks_like_url,
};
use shift_core::preferences::load_module_priority;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

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
    let mut target = OutputFormat::MARKDOWN;
    let mut preferred_module: Option<String> = None;
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
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        cursor += 1;
    }

    if stdout && output.is_some() {
        return Err("use either --stdout or --output, not both".to_owned());
    }

    let registry = if let Some(module) = preferred_module.as_ref() {
        ConversionRegistry::default().with_priority(&[module])
    } else {
        ConversionRegistry::default().with_priority(&load_module_priority())
    };

    let input_url = url_input(&input);
    let artifact = if let Some(url) = input_url {
        registry
            .convert_url(url, target)
            .map_err(|error| error.to_string())?
    } else {
        registry
            .convert_to(PathBuf::from(&input), target)
            .map_err(|error| error.to_string())?
    };

    if stdout {
        use std::io::Write;
        std::io::stdout()
            .write_all(&artifact.bytes)
            .map_err(|error| format!("could not write output: {error}"))?;
    } else {
        let output = output.unwrap_or_else(|| {
            if input_url.is_some() {
                PathBuf::from(&artifact.file_name)
            } else {
                default_output_path(PathBuf::from(&input).as_path(), target)
            }
        });
        artifact
            .write_to(&output)
            .map_err(|error| error.to_string())?;
        println!("{}", output.display());
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

fn print_help() {
    println!(
        "Shift converts files and URLs through the same modules as the native app.\n\n\
         Usage:\n  shift-cli <INPUT|URL> [-t <FORMAT>] [-o <OUTPUT>] [--stdout] [--module <ID>]\n  \
         shift-cli convert <INPUT|URL> [-t <FORMAT>] [-o <OUTPUT>] [--stdout]\n  \
         shift-cli formats\n\n\
         URLs (http/https) are extracted with Defuddle.\n\
         Use `shift-cli formats` to list every installed conversion capability.\n\
         If no output is supplied, Shift writes beside the source (or the current directory for URLs)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
