//! Spreadsheet adapter: tabular conversion through calamine + csv + rust_xlsxwriter.
//!
//! In-process (no external binary). Reads common workbook formats and delimited
//! text; writes CSV, TSV, and XLSX as **values only** (no styles, charts, or
//! formula evaluation). Document → Markdown stays with MarkItDown / Docling /
//! Pandoc; this module owns sheet-native pairs only.
//!
//! Cell text is preserved as written when encoding XLSX (no boolean/number
//! inference). ISO `YYYY-MM-DD` strings are re-emitted as Excel date cells so
//! calendar dates round-trip cleanly.

use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, InvocationRecord,
    OutputFormat, max_output_bytes,
};
use calamine::{Data, Reader, open_workbook_auto};
use csv::{ReaderBuilder, WriterBuilder};
use rust_xlsxwriter::{ExcelDateTime as XlsxDateTime, Format, Workbook, XlsxError};
use std::fs::{self, File};
use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::Ordering;
use zip::read::ZipArchive;

/// Formats this module can open as a grid of cell values.
const EXTENSIONS: &[&str] = &[
    "xlsx", "xlsm", "xlsb", "xls", "xla", "xlam", "ods", // calamine
    "csv", "tsv", // delimited text
];

/// Tabular writers. Markdown / HTML stay with document engines.
const OUTPUTS: &[OutputFormat] = &[OutputFormat::CSV, OutputFormat::TSV, OutputFormat::XLSX];

/// Delimited text is safe for a second hop (e.g. CSV → Markdown via MarkItDown).
const CHAINABLE: &[OutputFormat] = &[OutputFormat::CSV, OutputFormat::TSV];

/// Soft cap on intermediate grid size (rows × average width). Sparse used
/// ranges can still allocate heavily before final encoding.
const MAX_GRID_CELLS: usize = 2_000_000;
/// Reject inputs larger than this on disk before any parser materializes them.
pub const MAX_SPREADSHEET_INPUT_BYTES: u64 = 64 * 1024 * 1024;
/// For zip-based workbooks (xlsx/ods/…), total uncompressed entry size cap.
pub const MAX_SPREADSHEET_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
/// Aggregate UTF-8 payload of cell strings while building the grid.
pub const MAX_SPREADSHEET_AGGREGATE_CELL_BYTES: usize = 64 * 1024 * 1024;
/// Single cell string ceiling (guards pathological cells before push).
pub const MAX_SPREADSHEET_CELL_BYTES: usize = 1024 * 1024;

/// How often to re-check cancellation while scanning or encoding rows.
const CANCEL_CHECK_EVERY_ROWS: usize = 256;

/// Optional knobs for sheet selection. Defaults read the first sheet.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SpreadsheetOptions {
    /// 1-based sheet index. Ignored when [`Self::sheet_name`] is set.
    pub sheet_index: Option<u32>,
    /// Exact sheet name (case-sensitive, matching Excel). Wins over index.
    pub sheet_name: Option<String>,
}

impl SpreadsheetOptions {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpreadsheetModule;

impl SpreadsheetModule {
    fn convert_grid(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        check_cancel(options)?;

        let knobs = &options.spreadsheet;
        let (sheet_label, rows) = load_grid(input, knobs, options)?;
        check_cancel(options)?;

        let bytes = match output_format.id() {
            "csv" => encode_delimited(&rows, b',', options)?,
            "tsv" => encode_delimited(&rows, b'\t', options)?,
            "xlsx" => encode_xlsx(&rows, options)?,
            other => {
                return Err(ConversionError::new(format!(
                    "spreadsheet module does not write {other}"
                )));
            }
        };

        let limit = max_output_bytes();
        if bytes.len() > limit {
            return Err(ConversionError::new(format!(
                "spreadsheet output exceeded the {} byte limit",
                limit
            )));
        }

        let stem = input
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("converted");
        let file_name = format!("{stem}.{}", output_format.extension());

        let selection = selection_source(knobs);
        let max_cols = rows.iter().map(|row| row.len()).max().unwrap_or(0);
        let argv_display = format!(
            "spreadsheet {} → {} (sheet={sheet_label}, select={selection}, rows={}, cols={max_cols})",
            input
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("?"),
            output_format.id(),
            rows.len()
        );

        Ok(ConversionArtifact {
            file_name,
            media_type: output_format.media_type(),
            bytes,
            format: output_format,
            module_id: self.id(),
            pipeline: vec![self.id()],
            invocations: vec![InvocationRecord {
                module_id: self.id(),
                argv_display,
            }],
        })
    }
}

impl ConversionModule for SpreadsheetModule {
    fn id(&self) -> &'static str {
        "spreadsheet"
    }

    fn label(&self) -> &'static str {
        "Spreadsheet"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        EXTENSIONS
    }

    fn output_formats(&self) -> &[OutputFormat] {
        OUTPUTS
    }

    fn chainable_output_formats(&self) -> &[OutputFormat] {
        CHAINABLE
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !OUTPUTS.contains(&output_format) {
            return Err(ConversionError::new(format!(
                "spreadsheet module does not write {}",
                output_format.label()
            )));
        }
        self.convert_grid(input, output_format, options)
    }
}

fn check_cancel(options: &ConversionOptions) -> Result<(), ConversionError> {
    if options
        .cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        return Err(ConversionError::new("conversion cancelled"));
    }
    Ok(())
}

fn selection_source(options: &SpreadsheetOptions) -> &'static str {
    if options
        .sheet_name
        .as_ref()
        .map(|value| value.trim())
        .is_some_and(|value| !value.is_empty())
    {
        "name"
    } else if options.sheet_index.is_some() {
        "index"
    } else {
        "default"
    }
}

fn load_grid(
    input: &Path,
    sheet: &SpreadsheetOptions,
    options: &ConversionOptions,
) -> Result<(String, Vec<Vec<String>>), ConversionError> {
    ensure_input_budgets(input)?;

    let ext = input
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "csv" => Ok(("csv".into(), read_delimited(input, b',', options)?)),
        "tsv" => Ok(("tsv".into(), read_delimited(input, b'\t', options)?)),
        _ => read_workbook(input, sheet, options),
    }
}

/// File size + zip decompressed budgets checked **before** full grid materialization.
fn ensure_input_budgets(path: &Path) -> Result<(), ConversionError> {
    let metadata = fs::metadata(path).map_err(|error| {
        ConversionError::new(format!(
            "could not stat spreadsheet {}: {error}",
            path.display()
        ))
    })?;
    let len = metadata.len();
    if len > MAX_SPREADSHEET_INPUT_BYTES {
        return Err(ConversionError::new(format!(
            "spreadsheet input exceeds the {MAX_SPREADSHEET_INPUT_BYTES} byte file size limit ({len} bytes)"
        )));
    }

    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    // OOXML / ODS containers are zip; bound uncompressed size before calamine expands them.
    if matches!(
        ext.as_str(),
        "xlsx" | "xlsm" | "xlsb" | "xla" | "xlam" | "ods"
    ) {
        ensure_zip_decompressed_budget(path)?;
    }
    Ok(())
}

fn ensure_zip_decompressed_budget(path: &Path) -> Result<(), ConversionError> {
    let file = File::open(path).map_err(|error| {
        ConversionError::new(format!(
            "could not open spreadsheet {}: {error}",
            path.display()
        ))
    })?;
    let mut archive = match ZipArchive::new(file) {
        Ok(archive) => archive,
        // Not a zip (legacy BIFF, corrupt header): leave further validation to calamine.
        Err(_) => return Ok(()),
    };
    let mut total = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| {
            ConversionError::new(format!(
                "could not inspect spreadsheet zip entry in {}: {error}",
                path.display()
            ))
        })?;
        total = total.saturating_add(entry.size());
        if total > MAX_SPREADSHEET_DECOMPRESSED_BYTES {
            return Err(ConversionError::new(format!(
                "spreadsheet decompressed size exceeds the {MAX_SPREADSHEET_DECOMPRESSED_BYTES} byte limit"
            )));
        }
    }
    Ok(())
}

fn read_delimited(
    path: &Path,
    delimiter: u8,
    options: &ConversionOptions,
) -> Result<Vec<Vec<String>>, ConversionError> {
    let mut reader = ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_path(path)
        .map_err(|error| {
            ConversionError::new(format!("could not read {}: {error}", path.display()))
        })?;

    let mut rows = Vec::new();
    let mut cell_count = 0usize;
    let mut aggregate_bytes = 0usize;
    for (row_idx, record) in reader.records().enumerate() {
        if row_idx % CANCEL_CHECK_EVERY_ROWS == 0 {
            check_cancel(options)?;
        }
        let record = record.map_err(|error| {
            ConversionError::new(format!("could not parse {}: {error}", path.display()))
        })?;
        cell_count = cell_count.saturating_add(record.len());
        ensure_cell_budget(cell_count)?;
        let mut row = Vec::with_capacity(record.len());
        for cell in record.iter() {
            ensure_cell_string_budget(cell, &mut aggregate_bytes)?;
            row.push(cell.to_owned());
        }
        rows.push(row);
    }
    Ok(rows)
}

fn read_workbook(
    path: &Path,
    sheet: &SpreadsheetOptions,
    options: &ConversionOptions,
) -> Result<(String, Vec<Vec<String>>), ConversionError> {
    let mut workbook = open_workbook_auto(path).map_err(|error| {
        ConversionError::new(format!(
            "could not open spreadsheet {}: {error}",
            path.display()
        ))
    })?;

    let sheet_names = workbook.sheet_names();
    if sheet_names.is_empty() {
        return Err(ConversionError::new(format!(
            "spreadsheet {} has no sheets",
            path.display()
        )));
    }

    let sheet_name = resolve_sheet_name(&sheet_names, sheet)?;
    let range = workbook.worksheet_range(&sheet_name).map_err(|error| {
        ConversionError::new(format!(
            "could not read sheet `{sheet_name}` in {}: {error}",
            path.display()
        ))
    })?;

    // Bound the used range before allocating every cell string.
    let height = range.height();
    let width = range.width();
    let projected = height.saturating_mul(width);
    ensure_cell_budget(projected)?;
    if projected > 0 {
        // Worst-case: every cell could hold MAX_SPREADSHEET_CELL_BYTES; reject
        // ranges that cannot fit under the aggregate budget even at 1 byte/cell.
        if projected > MAX_SPREADSHEET_AGGREGATE_CELL_BYTES {
            return Err(ConversionError::new(format!(
                "spreadsheet used range ({height}×{width}) exceeds the aggregate cell byte budget"
            )));
        }
    }

    let mut rows = Vec::with_capacity(height);
    let mut cell_count = 0usize;
    let mut aggregate_bytes = 0usize;
    for (row_idx, row) in range.rows().enumerate() {
        if row_idx % CANCEL_CHECK_EVERY_ROWS == 0 {
            check_cancel(options)?;
        }
        cell_count = cell_count.saturating_add(row.len());
        ensure_cell_budget(cell_count)?;
        let mut out_row = Vec::with_capacity(row.len());
        for cell in row {
            let text = cell_to_string(cell);
            ensure_cell_string_budget(&text, &mut aggregate_bytes)?;
            out_row.push(text);
        }
        rows.push(out_row);
    }
    Ok((sheet_name, rows))
}

fn ensure_cell_budget(cell_count: usize) -> Result<(), ConversionError> {
    if cell_count > MAX_GRID_CELLS {
        return Err(ConversionError::new(format!(
            "spreadsheet exceeds the {MAX_GRID_CELLS} cell limit; narrow the sheet or split the file"
        )));
    }
    Ok(())
}

fn ensure_cell_string_budget(
    cell: &str,
    aggregate_bytes: &mut usize,
) -> Result<(), ConversionError> {
    let len = cell.len();
    if len > MAX_SPREADSHEET_CELL_BYTES {
        return Err(ConversionError::new(format!(
            "spreadsheet cell exceeds the {MAX_SPREADSHEET_CELL_BYTES} byte limit"
        )));
    }
    *aggregate_bytes = aggregate_bytes.saturating_add(len);
    if *aggregate_bytes > MAX_SPREADSHEET_AGGREGATE_CELL_BYTES {
        return Err(ConversionError::new(format!(
            "spreadsheet cell text exceeds the {MAX_SPREADSHEET_AGGREGATE_CELL_BYTES} byte aggregate limit"
        )));
    }
    Ok(())
}

fn resolve_sheet_name(
    sheet_names: &[String],
    options: &SpreadsheetOptions,
) -> Result<String, ConversionError> {
    if let Some(name) = options
        .sheet_name
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        if sheet_names.iter().any(|candidate| candidate == name) {
            return Ok(name.to_owned());
        }
        return Err(ConversionError::new(format!(
            "sheet `{name}` not found (available: {})",
            sheet_names.join(", ")
        )));
    }

    if let Some(index) = options.sheet_index {
        if index == 0 {
            return Err(ConversionError::new("sheet index is 1-based (got 0)"));
        }
        let zero = (index - 1) as usize;
        return sheet_names.get(zero).cloned().ok_or_else(|| {
            ConversionError::new(format!(
                "sheet index {index} is out of range (workbook has {} sheet{})",
                sheet_names.len(),
                if sheet_names.len() == 1 { "" } else { "s" }
            ))
        });
    }

    Ok(sheet_names[0].clone())
}

fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::Bool(true) => "TRUE".into(),
        Data::Bool(false) => "FALSE".into(),
        Data::Int(value) => value.to_string(),
        Data::Float(value) => format_float(*value),
        Data::String(value) => value.clone(),
        Data::DateTime(dt) => format_excel_datetime(dt),
        Data::DateTimeIso(value) | Data::DurationIso(value) => value.clone(),
        Data::Error(error) => error.to_string(),
    }
}

/// Format a calamine Excel serial as a calendar string (or duration clock).
fn format_excel_datetime(dt: &calamine::ExcelDateTime) -> String {
    if dt.is_duration() {
        return format_day_fraction_as_clock(dt.as_f64());
    }

    let (year, month, day, hour, min, sec, milli) = dt.to_ymd_hms_milli();
    if hour == 0 && min == 0 && sec == 0 && milli == 0 {
        return format!("{year:04}-{month:02}-{day:02}");
    }
    if milli == 0 {
        return format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}");
    }
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{milli:03}")
}

/// Duration cells store a fraction of a 24h day.
fn format_day_fraction_as_clock(fraction: f64) -> String {
    if !fraction.is_finite() {
        return format_float(fraction);
    }
    let total_ms = (fraction.abs() * 24.0 * 60.0 * 60.0 * 1000.0).round() as i64;
    let sign = if fraction < 0.0 { "-" } else { "" };
    let hours = total_ms / (60 * 60 * 1000);
    let rem = total_ms % (60 * 60 * 1000);
    let mins = rem / (60 * 1000);
    let rem = rem % (60 * 1000);
    let secs = rem / 1000;
    let milli = rem % 1000;
    if milli == 0 {
        format!("{sign}{hours:02}:{mins:02}:{secs:02}")
    } else {
        format!("{sign}{hours:02}:{mins:02}:{secs:02}.{milli:03}")
    }
}

fn format_float(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value.fract() == 0.0 && value.abs() < (i64::MAX as f64) {
        return format!("{}", value as i64);
    }
    // Fixed-point so common magnitudes never fall into scientific notation, then
    // trim trailing zeros after the decimal point.
    let text = format!("{value:.15}");
    trim_trailing_zeros(&text)
}

fn trim_trailing_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    let trimmed = text.trim_end_matches('0');
    if trimmed.ends_with('.') {
        trimmed.trim_end_matches('.').to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn encode_delimited(
    rows: &[Vec<String>],
    delimiter: u8,
    options: &ConversionOptions,
) -> Result<Vec<u8>, ConversionError> {
    let mut buffer = Vec::new();
    {
        let mut writer = WriterBuilder::new()
            .delimiter(delimiter)
            .from_writer(Cursor::new(&mut buffer));
        for (row_idx, row) in rows.iter().enumerate() {
            if row_idx % CANCEL_CHECK_EVERY_ROWS == 0 {
                check_cancel(options)?;
            }
            writer.write_record(row).map_err(|error| {
                ConversionError::new(format!("could not write delimited output: {error}"))
            })?;
        }
        writer
            .flush()
            .map_err(|error| ConversionError::new(format!("could not flush output: {error}")))?;
    }
    Ok(buffer)
}

fn encode_xlsx(
    rows: &[Vec<String>],
    options: &ConversionOptions,
) -> Result<Vec<u8>, ConversionError> {
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    // Re-emit ISO calendar dates as Excel date cells; everything else is text.
    let date_format = Format::new().set_num_format("yyyy-mm-dd");

    for (row_idx, row) in rows.iter().enumerate() {
        if row_idx % CANCEL_CHECK_EVERY_ROWS == 0 {
            check_cancel(options)?;
        }
        let row_u32 = u32::try_from(row_idx)
            .map_err(|_| ConversionError::new("spreadsheet has too many rows for XLSX"))?;
        for (col_idx, cell) in row.iter().enumerate() {
            let col_u16 = u16::try_from(col_idx)
                .map_err(|_| ConversionError::new("spreadsheet has too many columns for XLSX"))?;
            write_xlsx_cell(worksheet, row_u32, col_u16, cell, &date_format).map_err(|error| {
                ConversionError::new(format!("could not write XLSX cell: {error}"))
            })?;
        }
    }

    workbook
        .save_to_buffer()
        .map_err(|error| ConversionError::new(format!("could not write XLSX: {error}")))
}

fn write_xlsx_cell(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u16,
    cell: &str,
    date_format: &Format,
) -> Result<(), XlsxError> {
    if cell.is_empty() {
        return Ok(());
    }
    // ISO date `YYYY-MM-DD` → Excel date serial when possible (values-only date
    // round-trip). Booleans and numbers stay as text so labels like "true" or
    // "00123" are not rewritten.
    if cell.len() == 10
        && cell.as_bytes().get(4) == Some(&b'-')
        && cell.as_bytes().get(7) == Some(&b'-')
    {
        if let Ok(date) = XlsxDateTime::parse_from_str(cell) {
            return worksheet
                .write_with_format(row, col, &date, date_format)
                .map(|_| ());
        }
    }
    worksheet.write_string(row, col, cell).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(tag: &str, ext: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("shift-spreadsheet-{tag}-{n}.{ext}"))
    }

    fn write_temp(tag: &str, ext: &str, bytes: &[u8]) -> PathBuf {
        let path = temp_path(tag, ext);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn converts_csv_to_tsv_and_back() {
        let input = write_temp("round", "csv", b"name,age\nAda,36\n");
        let module = SpreadsheetModule;
        let tsv = module
            .convert(&input, OutputFormat::TSV, &ConversionOptions::default())
            .unwrap();
        assert_eq!(tsv.format, OutputFormat::TSV);
        assert!(tsv.file_name.ends_with(".tsv"));
        assert_eq!(String::from_utf8_lossy(&tsv.bytes), "name\tage\nAda\t36\n");

        let tsv_path = write_temp("from-tsv", "tsv", &tsv.bytes);
        let csv = module
            .convert(&tsv_path, OutputFormat::CSV, &ConversionOptions::default())
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&csv.bytes), "name,age\nAda,36\n");
        let _ = fs::remove_file(input);
        let _ = fs::remove_file(tsv_path);
    }

    #[test]
    fn converts_csv_to_xlsx_and_back_to_csv() {
        let input = write_temp("toxlsx", "csv", b"a,b\n1,2\n");
        let module = SpreadsheetModule;
        let xlsx = module
            .convert(&input, OutputFormat::XLSX, &ConversionOptions::default())
            .unwrap();
        assert_eq!(xlsx.format, OutputFormat::XLSX);
        assert_eq!(xlsx.module_id, "spreadsheet");
        assert!(!xlsx.bytes.is_empty());
        // ZIP signature
        assert_eq!(&xlsx.bytes[..2], b"PK");

        let xlsx_path = write_temp("book", "xlsx", &xlsx.bytes);
        let csv = module
            .convert(&xlsx_path, OutputFormat::CSV, &ConversionOptions::default())
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&csv.bytes), "a,b\n1,2\n");
        let _ = fs::remove_file(input);
        let _ = fs::remove_file(xlsx_path);
    }

    #[test]
    fn xlsx_write_preserves_leading_zeros_and_true_labels() {
        let input = write_temp("coerce", "csv", b"code,flag\n00123,true\n");
        let module = SpreadsheetModule;
        let xlsx = module
            .convert(&input, OutputFormat::XLSX, &ConversionOptions::default())
            .unwrap();
        let xlsx_path = write_temp("coerce-book", "xlsx", &xlsx.bytes);
        let csv = module
            .convert(&xlsx_path, OutputFormat::CSV, &ConversionOptions::default())
            .unwrap();
        let text = String::from_utf8_lossy(&csv.bytes);
        assert!(text.contains("00123"), "leading zeros lost: {text}");
        assert!(text.contains("true"), "label coerced: {text}");
        let _ = fs::remove_file(input);
        let _ = fs::remove_file(xlsx_path);
    }

    #[test]
    fn excel_date_cells_export_as_calendar_strings() {
        let mut workbook = Workbook::new();
        {
            let sheet = workbook.add_worksheet();
            let date = XlsxDateTime::parse_from_str("2023-01-15").unwrap();
            let date_format = Format::new().set_num_format("yyyy-mm-dd");
            sheet.write_with_format(0, 0, &date, &date_format).unwrap();
            sheet.write_string(0, 1, "label").unwrap();
        }
        let bytes = workbook.save_to_buffer().unwrap();
        let path = write_temp("dates", "xlsx", &bytes);
        let csv = SpreadsheetModule
            .convert(&path, OutputFormat::CSV, &ConversionOptions::default())
            .unwrap();
        let text = String::from_utf8_lossy(&csv.bytes);
        assert!(
            text.contains("2023-01-15"),
            "expected calendar date, got serial-like output: {text}"
        );
        assert!(!text.contains("44941") && !text.contains("44942"), "{text}");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn sheet_index_and_name_select_workbook_sheet() {
        // Build a two-sheet workbook with rust_xlsxwriter.
        let mut workbook = Workbook::new();
        {
            let first = workbook.add_worksheet();
            first.set_name("Alpha").unwrap();
            first.write_string(0, 0, "from-alpha").unwrap();
        }
        {
            let second = workbook.add_worksheet();
            second.set_name("Beta").unwrap();
            second.write_string(0, 0, "from-beta").unwrap();
        }
        let bytes = workbook.save_to_buffer().unwrap();
        let path = write_temp("sheets", "xlsx", &bytes);
        let module = SpreadsheetModule;

        let by_name = module
            .convert(
                &path,
                OutputFormat::CSV,
                &ConversionOptions {
                    spreadsheet: SpreadsheetOptions {
                        sheet_name: Some("Beta".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            String::from_utf8_lossy(&by_name.bytes).contains("from-beta"),
            "got {}",
            String::from_utf8_lossy(&by_name.bytes)
        );
        assert!(
            by_name.invocations[0].argv_display.contains("select=name"),
            "{}",
            by_name.invocations[0].argv_display
        );

        let by_index = module
            .convert(
                &path,
                OutputFormat::CSV,
                &ConversionOptions {
                    spreadsheet: SpreadsheetOptions {
                        sheet_index: Some(2),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&by_index.bytes).contains("from-beta"));
        assert!(
            by_index.invocations[0]
                .argv_display
                .contains("select=index"),
            "{}",
            by_index.invocations[0].argv_display
        );
        assert!(
            by_index.invocations[0].argv_display.contains("cols="),
            "{}",
            by_index.invocations[0].argv_display
        );

        let missing = module.convert(
            &path,
            OutputFormat::CSV,
            &ConversionOptions {
                spreadsheet: SpreadsheetOptions {
                    sheet_name: Some("Nope".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(missing.is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn supports_case_insensitive_extensions() {
        let module = SpreadsheetModule;
        assert!(module.supports(Path::new("Book.XLSX"), OutputFormat::CSV));
        assert!(module.supports(Path::new("data.CSV"), OutputFormat::XLSX));
        assert!(!module.supports(Path::new("notes.md"), OutputFormat::CSV));
        assert!(!module.supports(Path::new("sheet.xlsx"), OutputFormat::MARKDOWN));
    }

    #[test]
    fn rejects_zero_sheet_index() {
        let path = write_temp("z", "csv", b"a\n1\n");
        // CSV path ignores sheet index; use a workbook.
        let mut workbook = Workbook::new();
        workbook.add_worksheet().write_string(0, 0, "x").unwrap();
        let bytes = workbook.save_to_buffer().unwrap();
        let xlsx = write_temp("zidx", "xlsx", &bytes);
        let err = SpreadsheetModule
            .convert(
                &xlsx,
                OutputFormat::CSV,
                &ConversionOptions {
                    spreadsheet: SpreadsheetOptions {
                        sheet_index: Some(0),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("1-based"), "{err}");
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(xlsx);
    }

    #[test]
    fn format_float_keeps_integers_clean() {
        assert_eq!(format_float(3.0), "3");
        assert_eq!(format_float(3.5), "3.5");
    }

    #[test]
    fn format_float_avoids_scientific_notation() {
        assert_eq!(format_float(1e20), "100000000000000000000");
        assert_eq!(format_float(1e-10), "0.0000000001");
        assert!(!format_float(1.23e15).contains('e') && !format_float(1.23e15).contains('E'));
    }

    #[test]
    fn ensure_cell_budget_rejects_oversize_grids() {
        assert!(ensure_cell_budget(MAX_GRID_CELLS).is_ok());
        let err = ensure_cell_budget(MAX_GRID_CELLS + 1).unwrap_err();
        assert!(err.to_string().contains("cell limit"), "{err}");
    }

    #[test]
    fn ensure_input_budgets_rejects_oversize_files() {
        let path = std::env::temp_dir().join(format!(
            "shift-sheet-oversize-{}-{}.csv",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // Sparse file if the OS allows; otherwise write a small marker and
        // temporarily exercise the check via a renamed huge path is not portable.
        // Write just over the limit when feasible; for CI speed, mock by writing
        // a modest file and calling the size check logic directly when large
        // writes are too expensive.
        let oversize = MAX_SPREADSHEET_INPUT_BYTES.saturating_add(1);
        if let Ok(file) = File::create(&path) {
            let _ = file.set_len(oversize);
        }
        if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_SPREADSHEET_INPUT_BYTES {
            let err = ensure_input_budgets(&path).unwrap_err();
            assert!(
                err.to_string().contains("file size limit"),
                "unexpected: {err}"
            );
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ensure_cell_string_budget_tracks_aggregate() {
        let mut aggregate = 0usize;
        ensure_cell_string_budget("hello", &mut aggregate).unwrap();
        assert_eq!(aggregate, 5);
        let big = "x".repeat(MAX_SPREADSHEET_CELL_BYTES + 1);
        let err = ensure_cell_string_budget(&big, &mut aggregate).unwrap_err();
        assert!(err.to_string().contains("cell exceeds"), "{err}");
        let mut aggregate = MAX_SPREADSHEET_AGGREGATE_CELL_BYTES - 2;
        let err = ensure_cell_string_budget("abcd", &mut aggregate).unwrap_err();
        assert!(err.to_string().contains("aggregate"), "{err}");
    }

    #[test]
    fn selection_source_labels() {
        assert_eq!(selection_source(&SpreadsheetOptions::default()), "default");
        assert_eq!(
            selection_source(&SpreadsheetOptions {
                sheet_index: Some(2),
                ..Default::default()
            }),
            "index"
        );
        assert_eq!(
            selection_source(&SpreadsheetOptions {
                sheet_name: Some("Beta".into()),
                sheet_index: Some(9),
            }),
            "name"
        );
    }
}
