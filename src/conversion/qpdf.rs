//! qpdf adapter for PDF page operations and optimization.
//!
//! Page extract, rotate, and Flate recompress (`Lossless`) are lossless.
//! `Smaller` also JPEG-recompresses suitable images (lossy).

use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, InvocationRecord,
    OutputFormat, TempDirGuard, command_argv_parts, format_argv_display, map_spawn_error,
    max_output_bytes, process_timeout, read_file_limited, resolve_tool_executable,
    run_command_cancellable, unique_temp_dir,
};
use std::ffi::OsString;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;

const INPUTS: &[&str] = &["pdf"];
const OUTPUTS: &[OutputFormat] = &[OutputFormat::PDF, OutputFormat::PDF_PAGES_ZIP];
const MAX_SPLIT_GROUP: u32 = 10_000;
/// Maximum split page files admitted into a page ZIP.
pub const MAX_PDF_ZIP_PAGES: usize = 500;
/// Cap on intermediate split PDFs on disk before / while archiving.
pub const MAX_PDF_ZIP_INTERMEDIATE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PdfCompression {
    /// Preserve existing stream encodings.
    #[default]
    Preserve,
    /// Recompress Flate streams and generate compressed object streams.
    Lossless,
    /// Also allow qpdf to JPEG-compress suitable images.
    Smaller,
}

impl PdfCompression {
    pub fn id(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::Lossless => "lossless",
            Self::Smaller => "smaller",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Preserve => "Preserve",
            Self::Lossless => "Lossless",
            Self::Smaller => "Smaller",
        }
    }
}

impl std::str::FromStr for PdfCompression {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "preserve" | "none" | "off" => Ok(Self::Preserve),
            "lossless" | "compress" => Ok(Self::Lossless),
            "smaller" | "images" | "optimized" => Ok(Self::Smaller),
            _ => Err(ConversionError::new(format!(
                "unknown PDF compression `{value}` (expected preserve, lossless, or smaller)"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct QpdfModule {
    executable: OsString,
}

impl Default for QpdfModule {
    fn default() -> Self {
        Self {
            executable: resolve_tool_executable("SHIFT_QPDF_BIN", "qpdf", &[]),
        }
    }
}

impl QpdfModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn convert_pdf(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        validate_options(output_format, &options.pdf)?;
        let stem = input
            .file_stem()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsStr::new("converted"));
        let work_dir = unique_temp_dir("shift-qpdf-toolkit")?;
        let _cleanup = TempDirGuard(work_dir.clone());
        let produced = if output_format == OutputFormat::PDF_PAGES_ZIP {
            work_dir.join("page-%d.pdf")
        } else {
            work_dir.join("converted.pdf")
        };

        let mut command = Command::new(&self.executable);
        // Warnings-only runs (exit 3 by default) still write output; treat them
        // as success so ordinary real-world PDFs do not fail the toolkit.
        command.arg("--warning-exit-0");
        command.arg(input);
        add_password_file(&mut command, &work_dir, options.pdf.password.as_deref())?;
        if options
            .pdf
            .password
            .as_deref()
            .is_some_and(|password| !password.trim().is_empty())
        {
            // The artifact must be consumable after conversion without asking
            // for the input password again.
            command.arg("--decrypt");
        }
        add_page_selection(&mut command, &options.pdf);
        add_rewrite_options(&mut command, &options.pdf);
        if output_format == OutputFormat::PDF_PAGES_ZIP {
            command.arg(format!(
                "--split-pages={}",
                options.pdf.split_pages.unwrap_or(1)
            ));
        }
        command.arg(&produced);

        let invocation = InvocationRecord {
            module_id: self.id(),
            argv_display: format_argv_display(&command_argv_parts(&command)),
        };
        if let Some(progress) = options.progress.as_ref() {
            progress(super::ConversionProgress::Phase(
                if output_format == OutputFormat::PDF_PAGES_ZIP {
                    "Splitting PDF pages…"
                } else {
                    "Optimizing PDF…"
                }
                .into(),
            ));
        }
        let output = run_command_cancellable(
            command,
            process_timeout(),
            max_output_bytes(),
            options.cancel.clone(),
        )
        .map_err(|error| {
            map_spawn_error(
                error,
                "qpdf is not installed. Install it with `brew install qpdf`, \
                 or set SHIFT_QPDF_BIN.",
            )
        })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if detail.is_empty() {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if stdout.is_empty() {
                    format!("process exited with {}", output.status)
                } else {
                    stdout
                }
            } else {
                detail
            };
            return Err(ConversionError::new(format!(
                "qpdf could not process {}: {detail}",
                input.display()
            )));
        }

        let (bytes, file_name, media_type) = if output_format == OutputFormat::PDF_PAGES_ZIP {
            let archive = work_dir.join("pdf-pages.zip");
            zip_split_pdfs(&work_dir, &archive)?;
            let bytes = read_file_limited(&archive, max_output_bytes()).map_err(|error| {
                ConversionError::new(format!(
                    "qpdf page archive was not readable at {}: {error}",
                    archive.display()
                ))
            })?;
            (
                bytes,
                format!("{}-pages.zip", stem.to_string_lossy()),
                "application/zip",
            )
        } else {
            let bytes = read_file_limited(&produced, max_output_bytes()).map_err(|error| {
                ConversionError::new(format!(
                    "qpdf finished but did not write {}: {error}",
                    produced.display()
                ))
            })?;
            (
                bytes,
                format!("{}.pdf", stem.to_string_lossy()),
                "application/pdf",
            )
        };

        Ok(ConversionArtifact {
            bytes,
            file_name,
            media_type,
            format: output_format,
            module_id: self.id(),
            pipeline: vec![self.id()],
            invocations: vec![invocation],
        })
    }
}

fn validate_options(
    output_format: OutputFormat,
    options: &super::PdfInputOptions,
) -> Result<(), ConversionError> {
    if !OUTPUTS.contains(&output_format) {
        return Err(ConversionError::new(format!(
            "qpdf cannot produce {}",
            output_format.label()
        )));
    }
    if options.page_from == Some(0) || options.page_to == Some(0) {
        return Err(ConversionError::new(
            "PDF pages are 1-based and must be at least 1",
        ));
    }
    if let (Some(from), Some(to)) = (options.page_from, options.page_to)
        && from > to
    {
        return Err(ConversionError::new(format!(
            "PDF page range start ({from}) must be <= end ({to})"
        )));
    }
    if let Some(degrees) = options.rotate_degrees
        && !matches!(degrees, 90 | 180 | 270)
    {
        return Err(ConversionError::new(
            "PDF rotation must be 90, 180, or 270 degrees",
        ));
    }
    // Stale split_pages from session/UI ZIP state is ignored for PDF rewrites.
    // Callers that intend ZIP should select PDF_PAGES_ZIP; the CLI rejects
    // --pdf-split-pages without that target at parse time.
    if output_format == OutputFormat::PDF_PAGES_ZIP
        && let Some(group) = options.split_pages
        && !(1..=MAX_SPLIT_GROUP).contains(&group)
    {
        return Err(ConversionError::new(format!(
            "PDF split group must be between 1 and {MAX_SPLIT_GROUP} pages"
        )));
    }
    if output_format == OutputFormat::PDF_PAGES_ZIP
        && let Some(estimate) = estimate_selected_page_count(options)
        && estimate > MAX_PDF_ZIP_PAGES
    {
        return Err(ConversionError::new(format!(
            "PDF page selection would produce about {estimate} files (limit is {MAX_PDF_ZIP_PAGES}); narrow the page range"
        )));
    }
    Ok(())
}

/// Best-effort page count from an inclusive from/to range (unknown upper bound → None).
fn estimate_selected_page_count(options: &super::PdfInputOptions) -> Option<usize> {
    let from = options.page_from.unwrap_or(1) as usize;
    let to = options.page_to? as usize;
    if to < from {
        return None;
    }
    Some(to - from + 1)
}

fn add_password_file(
    command: &mut Command,
    work_dir: &Path,
    password: Option<&str>,
) -> Result<(), ConversionError> {
    let Some(password) = password.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let password_file = work_dir.join("password.txt");
    fs::write(&password_file, password.as_bytes())
        .map_err(|error| ConversionError::new(format!("could not write PDF password: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&password_file)
            .map_err(|error| ConversionError::new(format!("could not stat PDF password: {error}")))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&password_file, permissions).map_err(|error| {
            ConversionError::new(format!("could not restrict PDF password file: {error}"))
        })?;
    }
    command.arg(format!("--password-file={}", password_file.display()));
    Ok(())
}

fn add_page_selection(command: &mut Command, options: &super::PdfInputOptions) {
    if !options.needs_slice() {
        return;
    }
    let from = options.page_from.unwrap_or(1);
    let range = options
        .page_to
        .map(|to| format!("{from}-{to}"))
        .unwrap_or_else(|| format!("{from}-z"));
    command.arg("--pages").arg(".").arg(range).arg("--");
}

fn add_rewrite_options(command: &mut Command, options: &super::PdfInputOptions) {
    match options.compression {
        PdfCompression::Preserve => {}
        PdfCompression::Lossless | PdfCompression::Smaller => {
            command
                .arg("--stream-data=compress")
                .arg("--recompress-flate")
                .arg("--compression-level=9")
                .arg("--object-streams=generate");
            if options.compression == PdfCompression::Smaller {
                command.arg("--optimize-images").arg("--jpeg-quality=82");
            }
        }
    }
    if let Some(degrees) = options.rotate_degrees {
        command.arg(format!("--rotate=+{degrees}:1-z"));
    }
    if options.linearize {
        command.arg("--linearize");
    }
}

fn zip_split_pdfs(directory: &Path, archive: &Path) -> Result<(), ConversionError> {
    let mut pages: Vec<PathBuf> = fs::read_dir(directory)
        .map_err(|error| {
            ConversionError::new(format!(
                "could not list split PDFs in {}: {error}",
                directory.display()
            ))
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        })
        .collect();
    pages.sort();
    if pages.is_empty() {
        return Err(ConversionError::new(
            "qpdf did not produce any pages for the PDF archive",
        ));
    }
    if pages.len() > MAX_PDF_ZIP_PAGES {
        return Err(ConversionError::new(format!(
            "qpdf produced {} page files (limit is {MAX_PDF_ZIP_PAGES}); narrow the page range or increase split group size",
            pages.len()
        )));
    }

    let mut intermediate_bytes = 0u64;
    for page in &pages {
        let len = fs::metadata(page).map(|meta| meta.len()).unwrap_or(0);
        intermediate_bytes = intermediate_bytes.saturating_add(len);
        if intermediate_bytes > MAX_PDF_ZIP_INTERMEDIATE_BYTES {
            return Err(ConversionError::new(format!(
                "split PDF intermediates exceed the {MAX_PDF_ZIP_INTERMEDIATE_BYTES} byte disk budget"
            )));
        }
    }

    let output_limit = max_output_bytes() as u64;
    let file = fs::File::create(archive).map_err(|error| {
        ConversionError::new(format!(
            "could not create PDF page ZIP {}: {error}",
            archive.display()
        ))
    })?;
    // Stream each split PDF into the archive without buffering whole pages.
    let mut zip = zip::ZipWriter::new(file);
    let zip_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut written_uncompressed = 0u64;
    for page in pages {
        let name = page
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("page.pdf");
        zip.start_file(name, zip_options).map_err(|error| {
            ConversionError::new(format!("could not add {name} to PDF page ZIP: {error}"))
        })?;
        let mut reader = BufReader::new(fs::File::open(&page).map_err(|error| {
            ConversionError::new(format!(
                "could not read split PDF {}: {error}",
                page.display()
            ))
        })?);
        let copied = std::io::copy(&mut reader, &mut zip).map_err(|error| {
            ConversionError::new(format!("could not write {name} to PDF page ZIP: {error}"))
        })?;
        written_uncompressed = written_uncompressed.saturating_add(copied);
        // Compressed size is usually smaller, but reject runaway archives early.
        if written_uncompressed
            > output_limit
                .saturating_mul(4)
                .max(MAX_PDF_ZIP_INTERMEDIATE_BYTES)
        {
            return Err(ConversionError::new(
                "PDF page ZIP is growing beyond the allowed intermediate budget",
            ));
        }
    }
    zip.finish()
        .map_err(|error| ConversionError::new(format!("could not finish PDF page ZIP: {error}")))?;
    if let Ok(meta) = fs::metadata(archive)
        && meta.len() > output_limit
    {
        return Err(ConversionError::new(format!(
            "PDF page ZIP exceeds the {} byte output limit",
            output_limit
        )));
    }
    Ok(())
}

impl ConversionModule for QpdfModule {
    fn id(&self) -> &'static str {
        "qpdf"
    }

    fn label(&self) -> &'static str {
        "qpdf"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        INPUTS
    }

    fn output_formats(&self) -> &[OutputFormat] {
        OUTPUTS
    }

    fn chainable_output_formats(&self) -> &[OutputFormat] {
        &[]
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !input
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            return Err(ConversionError::new(format!(
                "qpdf requires a PDF input: {}",
                input.display()
            )));
        }
        self.convert_pdf(input, output_format, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_fake_qpdf(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            path,
            r#"#!/bin/sh
set -e
printf '%s\n' "$*" > "${0}.args"
out=""
for arg in "$@"; do out="$arg"; done
case "$out" in
  *%d*)
    prefix=${out%%%d*}
    suffix=${out#*%d}
    printf '%%PDF-1.4 page one' > "${prefix}01${suffix}"
    printf '%%PDF-1.4 page two' > "${prefix}02${suffix}"
    ;;
  *)
    printf '%%PDF-1.4 rewritten' > "$out"
    ;;
esac
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn compression_parse_and_labels_round_trip() {
        for mode in [
            PdfCompression::Preserve,
            PdfCompression::Lossless,
            PdfCompression::Smaller,
        ] {
            assert_eq!(mode.id().parse::<PdfCompression>().unwrap(), mode);
            assert!(!mode.label().is_empty());
        }
        assert!("tiny".parse::<PdfCompression>().is_err());
    }

    #[test]
    fn validates_page_rotation_split_and_output_contracts() {
        let mut options = super::super::PdfInputOptions {
            rotate_degrees: Some(45),
            ..super::super::PdfInputOptions::default()
        };
        assert!(validate_options(OutputFormat::PDF, &options).is_err());
        options.rotate_degrees = Some(90);
        options.page_from = Some(4);
        options.page_to = Some(2);
        assert!(validate_options(OutputFormat::PDF, &options).is_err());
        options.page_from = Some(2);
        options.page_to = Some(4);
        options.split_pages = Some(0);
        assert!(validate_options(OutputFormat::PDF_PAGES_ZIP, &options).is_err());
        options.split_pages = Some(2);
        // Stale split_pages is ignored for plain PDF rewrites.
        assert!(validate_options(OutputFormat::PDF, &options).is_ok());
        assert!(validate_options(OutputFormat::PDF_PAGES_ZIP, &options).is_ok());
    }

    #[test]
    fn rejects_page_zip_when_selected_range_exceeds_page_budget() {
        let options = super::super::PdfInputOptions {
            page_from: Some(1),
            page_to: Some((MAX_PDF_ZIP_PAGES as u32) + 1),
            split_pages: Some(1),
            ..super::super::PdfInputOptions::default()
        };
        let err = validate_options(OutputFormat::PDF_PAGES_ZIP, &options).unwrap_err();
        assert!(
            err.to_string().contains("limit") || err.to_string().contains("files"),
            "{err}"
        );
        // Unknown upper bound (to open-ended) is not estimated.
        let open_ended = super::super::PdfInputOptions {
            page_from: Some(1),
            page_to: None,
            split_pages: Some(1),
            ..super::super::PdfInputOptions::default()
        };
        assert!(validate_options(OutputFormat::PDF_PAGES_ZIP, &open_ended).is_ok());
    }

    #[test]
    fn zip_split_pdfs_rejects_too_many_intermediate_pages() {
        let directory = std::env::temp_dir().join(format!(
            "shift-qpdf-zip-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&directory).unwrap();
        for i in 0..(MAX_PDF_ZIP_PAGES + 1) {
            fs::write(directory.join(format!("page-{i:04}.pdf")), b"%PDF tiny").unwrap();
        }
        let archive = directory.join("pages.zip");
        let err = zip_split_pdfs(&directory, &archive).unwrap_err();
        assert!(
            err.to_string().contains("limit") || err.to_string().contains("page"),
            "{err}"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn builds_page_and_rewrite_arguments_without_password_contents() {
        let options = super::super::PdfInputOptions {
            password: Some("very secret".into()),
            page_from: Some(2),
            page_to: Some(5),
            rotate_degrees: Some(90),
            compression: PdfCompression::Smaller,
            linearize: true,
            split_pages: Some(2),
        };
        let directory =
            std::env::temp_dir().join(format!("shift-qpdf-args-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let mut command = Command::new("qpdf");
        command.arg("source.pdf");
        add_password_file(&mut command, &directory, options.password.as_deref()).unwrap();
        add_page_selection(&mut command, &options);
        add_rewrite_options(&mut command, &options);
        command
            .arg("--split-pages=2")
            .arg(directory.join("page-%d.pdf"));
        let display = format_argv_display(&command_argv_parts(&command));
        assert!(display.contains("--pages . 2-5 --"), "{display}");
        assert!(display.contains("--rotate=+90:1-z"), "{display}");
        assert!(display.contains("--optimize-images"), "{display}");
        assert!(display.contains("--linearize"), "{display}");
        assert!(display.contains("--password-file="), "{display}");
        assert!(!display.contains("very secret"), "{display}");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn zip_split_pdfs_is_sorted_and_rejects_empty_directories() {
        let directory = std::env::temp_dir().join(format!("shift-qpdf-zip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let archive = directory.join("pages.zip");
        assert!(zip_split_pdfs(&directory, &archive).is_err());
        fs::write(directory.join("page-02.pdf"), b"%PDF second").unwrap();
        fs::write(directory.join("page-01.pdf"), b"%PDF first").unwrap();
        zip_split_pdfs(&directory, &archive).unwrap();
        let file = fs::File::open(&archive).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        assert_eq!(zip.len(), 2);
        assert_eq!(zip.by_index(0).unwrap().name(), "page-01.pdf");
        assert_eq!(zip.by_index(1).unwrap().name(), "page-02.pdf");
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn converts_pdf_and_split_zip_with_provenance() {
        let directory =
            std::env::temp_dir().join(format!("shift-qpdf-convert-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("qpdf");
        let input = directory.join("source.pdf");
        write_fake_qpdf(&executable);
        fs::write(&input, b"%PDF-1.4 source").unwrap();
        let module = QpdfModule::with_executable(&executable);

        let pdf = module
            .convert(
                &input,
                OutputFormat::PDF,
                &ConversionOptions {
                    pdf: super::super::PdfInputOptions {
                        password: Some("not-on-argv".into()),
                        rotate_degrees: Some(90),
                        compression: PdfCompression::Lossless,
                        linearize: true,
                        ..super::super::PdfInputOptions::default()
                    },
                    ..ConversionOptions::default()
                },
            )
            .unwrap();
        assert_eq!(pdf.bytes, b"%PDF-1.4 rewritten");
        assert_eq!(pdf.file_name, "source.pdf");
        assert_eq!(pdf.pipeline, vec!["qpdf"]);
        assert!(pdf.invocations[0].argv_display.contains("--password-file="));
        assert!(pdf.invocations[0].argv_display.contains("--decrypt"));
        assert!(pdf.invocations[0].argv_display.contains("--warning-exit-0"));
        assert!(!pdf.invocations[0].argv_display.contains("not-on-argv"));

        // Stale split_pages from a prior ZIP session must not brick PDF rewrite.
        let pdf_stale_split = module
            .convert(
                &input,
                OutputFormat::PDF,
                &ConversionOptions {
                    pdf: super::super::PdfInputOptions {
                        split_pages: Some(2),
                        ..super::super::PdfInputOptions::default()
                    },
                    ..ConversionOptions::default()
                },
            )
            .unwrap();
        assert_eq!(pdf_stale_split.bytes, b"%PDF-1.4 rewritten");
        assert!(
            !pdf_stale_split.invocations[0]
                .argv_display
                .contains("--split-pages")
        );

        let pages = module
            .convert(
                &input,
                OutputFormat::PDF_PAGES_ZIP,
                &ConversionOptions {
                    pdf: super::super::PdfInputOptions {
                        split_pages: Some(1),
                        ..super::super::PdfInputOptions::default()
                    },
                    ..ConversionOptions::default()
                },
            )
            .unwrap();
        assert_eq!(pages.file_name, "source-pages.zip");
        let cursor = std::io::Cursor::new(pages.bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        assert_eq!(archive.len(), 2);
        assert_eq!(archive.by_index(0).unwrap().name(), "page-01.pdf");
        assert_eq!(archive.by_index(1).unwrap().name(), "page-02.pdf");

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn default_registry_routes_pdf_toolkit_directly() {
        let registry = super::super::ConversionRegistry::default();
        let input = Path::new("report.pdf");
        assert_eq!(
            registry
                .module_for(input, OutputFormat::PDF)
                .expect("PDF rewrite route")
                .id(),
            "qpdf"
        );
        assert_eq!(
            registry
                .module_for(input, OutputFormat::PDF_PAGES_ZIP)
                .expect("PDF split route")
                .id(),
            "qpdf"
        );
        let available = registry.available_outputs(input);
        assert!(available.contains(&OutputFormat::PDF));
        assert!(available.contains(&OutputFormat::PDF_PAGES_ZIP));
    }
}
