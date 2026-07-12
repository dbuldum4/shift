use shift_core::conversion::{ConversionRegistry, OutputFormat, default_output_path};
use shift_core::preferences::load_module_priority;
use std::path::PathBuf;

fn main() {
    if let Err(error) = run(std::env::args_os().skip(1).collect()) {
        eprintln!("shift-cli: {error}");
        std::process::exit(1);
    }
}

fn run(arguments: Vec<std::ffi::OsString>) -> Result<(), String> {
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
        .map(PathBuf::from)
        .ok_or_else(|| "missing input file (try `shift-cli --help`)".to_owned())?;
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
    let artifact = registry
        .convert_to(&input, target)
        .map_err(|error| error.to_string())?;

    if stdout {
        use std::io::Write;
        std::io::stdout()
            .write_all(&artifact.bytes)
            .map_err(|error| format!("could not write output: {error}"))?;
    } else {
        let output = output.unwrap_or_else(|| default_output_path(&input, target));
        artifact
            .write_to(&output)
            .map_err(|error| error.to_string())?;
        println!("{}", output.display());
    }

    Ok(())
}

fn print_formats() {
    for module in ConversionRegistry::default().modules() {
        let outputs = module
            .output_formats()
            .iter()
            .map(|format| format.id())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{} ({}): {} -> {outputs}",
            module.label(),
            module.id(),
            module.input_extensions().join(", ")
        );
    }
}

fn print_help() {
    println!(
        "Shift converts files through the same modules as the native app.\n\n\
         Usage:\n  shift-cli <INPUT> [-t <FORMAT>] [-o <OUTPUT>] [--stdout] [--module <ID>]\n  \
         shift-cli convert <INPUT> [-t <FORMAT>] [-o <OUTPUT>] [--stdout]\n  \
         shift-cli formats\n\n\
         Use `shift-cli formats` to list every installed conversion capability.\n\
         If no output is supplied, Shift writes beside the source using the target extension."
    );
}
