// SPDX-License-Identifier: MIT OR Apache-2.0
//! Import files (CSV and XLSX) read into the matrix [`load`](super::load) takes.
//!
//! An import screen hands the user's file to [`read_import_file`] and gets the column titles (the
//! first row) and the data rows as strings, exactly what a pasted or parsed CSV gives it today. It
//! shows them as the preview and passes the same matrix to `load`, so a spreadsheet goes through
//! the same field resolution, conversion and messages as a CSV file.
//!
//! Spreadsheet cells become the text `load` reads: shared and inline strings as written, numbers
//! as stored, booleans as `1`/`0`, cells formatted as dates as `YYYY-MM-DD` (with
//! ` HH:MM:SS` when the value has a time part), formulas as their computed value. What cannot be
//! read faithfully is refused by cell: an error value (`#N/A`), a formula saved without its value,
//! a date before 1 March 1900. Rows that are entirely empty are left out.
//!
//! The input is bounded before and while it is read: the file size, the archive entries, the
//! unpacked size of each part (a small archive cannot unpack into gigabytes), rows, columns and the
//! bytes of one cell.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Cursor, Read};

use erp_fields::{Error, Result};
use quick_xml::Reader;
use quick_xml::events::{BytesRef, BytesStart, Event};

/// Largest import file accepted, as uploaded (16 MiB).
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
/// Most data rows one file may hold (the column-title row not counted).
pub const MAX_FILE_ROWS: usize = 100_000;
/// Most columns one file may hold.
pub const MAX_FILE_COLUMNS: usize = 1_000;
/// Most total materialized cells (title row + squared data rows) one import file may produce.
pub const MAX_FILE_CELLS: usize = 500_000;
/// Largest cumulative UTF-8 text across all materialized cells in one import file (32 MiB).
pub const MAX_TOTAL_TEXT_BYTES: usize = 32 * 1024 * 1024;
/// Largest cell, in bytes of text (a spreadsheet cell holds at most 32 767 characters).
pub const MAX_CELL_BYTES: usize = 128 * 1024;
/// Largest unpacked part of a workbook (the sheet, the shared strings, the styles).
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;
/// Most entries a workbook archive may list.
const MAX_ENTRIES: usize = 10_000;
/// Most shared strings a workbook may declare.
const MAX_SHARED_STRINGS: usize = 1_000_000;
/// Largest cumulative shared-string table memory budget (text bytes + entry headers, 32 MiB).
const MAX_SHARED_STRING_BYTES: usize = 32 * 1024 * 1024;
/// Most worksheets a workbook may declare.
const MAX_SHEETS: usize = 256;
/// Most relationships a workbook relationship part may declare.
const MAX_RELATIONS: usize = 2_048;
/// Most style or number-format entries a styles part may declare.
const MAX_STYLES: usize = 65_536;
/// Most sheet names shown in a single validation error message.
const MAX_SHEET_NAMES_IN_MESSAGE: usize = 20;

/// Cumulative cell-count and text-byte budget for one import file.
#[derive(Default)]
struct CellBudget {
    cells: usize,
    bytes: usize,
}

impl CellBudget {
    fn charge_cells(&mut self, count: usize) -> Result<()> {
        let next = self.cells.saturating_add(count);
        if next > MAX_FILE_CELLS {
            return Err(Error::validation(format!(
                "The import file has more than {MAX_FILE_CELLS} cells; at most {MAX_FILE_CELLS} are accepted."
            )));
        }
        self.cells = next;
        Ok(())
    }

    fn charge_text(&mut self, len: usize) -> Result<()> {
        let next = self.bytes.saturating_add(len);
        if next > MAX_TOTAL_TEXT_BYTES {
            return Err(Error::validation(format!(
                "The import file holds more than {MAX_TOTAL_TEXT_BYTES} bytes of text; at most {MAX_TOTAL_TEXT_BYTES} are accepted."
            )));
        }
        self.bytes = next;
        Ok(())
    }
}

/// The kind of an import file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFileType {
    /// Comma (or `separator`) separated text in UTF-8.
    Csv,
    /// An Office Open XML workbook (`.xlsx`).
    Xlsx,
}

impl ImportFileType {
    /// `csv` or `xlsx`; anything else is refused by name.
    ///
    /// # Errors
    ///
    /// An unknown type.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "csv" => Ok(Self::Csv),
            "xlsx" => Ok(Self::Xlsx),
            other => Err(Error::validation(format!(
                "Import files of type '{other}' are not supported; use csv or xlsx."
            ))),
        }
    }
}

/// What to read from an import file besides its bytes.
#[derive(Debug, Clone, Default)]
pub struct ImportFileOptions {
    /// The worksheet of a workbook to read; needed when the workbook has more than one.
    pub sheet: Option<String>,
    /// The CSV separator: `,` (the default), `;` or a tab.
    pub separator: Option<char>,
}

/// An import file read into column titles and data rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportFile {
    /// The first row: the column titles.
    pub header: Vec<String>,
    /// The data rows, each as wide as `header`.
    pub rows: Vec<Vec<String>>,
    /// The worksheet read, for a workbook.
    pub sheet: Option<String>,
}

fn check_file_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Error::validation(format!(
            "The import file has {} bytes; at most {MAX_FILE_BYTES} are accepted.",
            bytes.len()
        )));
    }
    Ok(())
}

/// Read the sheet selector without decoding any sheet cells. File and archive limits are
/// identical to the regular importer.
pub fn import_sheet_names(bytes: &[u8]) -> Result<Vec<String>> {
    check_file_size(bytes)?;
    let mut workbook = Workbook::open(bytes)?;
    let (sheets, _) = parse_workbook(&workbook.required("xl/workbook.xml")?)?;
    if sheets.is_empty() {
        return Err(not_a_workbook("it lists no sheet"));
    }
    Ok(sheets.into_iter().map(|sheet| sheet.name).collect())
}

/// Read `bytes` as an import file of `kind`.
///
/// # Errors
///
/// A file over the limits, one that is not of `kind`, or one with content that cannot be read
/// faithfully; the message names the cell or the part.
pub fn read_import_file(
    bytes: &[u8],
    kind: ImportFileType,
    options: &ImportFileOptions,
) -> Result<ImportFile> {
    read_file(bytes, kind, options, None)
}

/// Parse a browser upload and retain the physical, one-based row number for each data row.
/// A quoted CSV value can span several lines; an XLSX sheet can omit rows entirely.
pub fn read_import_file_with_positions(
    bytes: &[u8],
    kind: ImportFileType,
    options: &ImportFileOptions,
) -> Result<(ImportFile, Vec<usize>)> {
    let mut positions = Vec::new();
    let file = read_file(bytes, kind, options, Some(&mut positions))?;
    Ok((file, positions))
}

fn read_file(
    bytes: &[u8],
    kind: ImportFileType,
    options: &ImportFileOptions,
    positions: Option<&mut Vec<usize>>,
) -> Result<ImportFile> {
    let mut csv_lines = Vec::new();
    check_file_size(bytes)?;
    let (rows, sheet) = match kind {
        ImportFileType::Csv => {
            if options.sheet.is_some() {
                return Err(Error::validation(
                    "A CSV file has no sheets to choose from.",
                ));
            }
            (
                read_csv(
                    bytes,
                    options.separator.unwrap_or(','),
                    positions.as_ref().map(|_| &mut csv_lines),
                )?,
                None,
            )
        }
        ImportFileType::Xlsx => {
            if options.separator.is_some() {
                return Err(Error::validation("A workbook has no separator to choose."));
            }
            let (rows, sheet) = read_xlsx(bytes, options.sheet.as_deref())?;
            (rows, Some(sheet))
        }
    };
    shape(rows, sheet, &csv_lines, positions)
}

/// Split the first row off as the titles and square the data rows to its width.
fn shape(
    rows: Vec<Vec<String>>,
    sheet: Option<String>,
    csv_lines: &[usize],
    mut positions: Option<&mut Vec<usize>>,
) -> Result<ImportFile> {
    let mut rows = rows
        .into_iter()
        .enumerate()
        .filter(|(_, row)| row.iter().any(|cell| !cell.is_empty()));
    let Some((_, mut header)) = rows.next() else {
        return Err(Error::validation(
            "The import file is empty: its first row must hold the column titles.",
        ));
    };
    while header.last().is_some_and(String::is_empty) {
        header.pop();
    }
    if let Some(position) = header.iter().position(String::is_empty) {
        return Err(Error::validation(format!(
            "Column {} of the title row is empty; every imported column needs a title.",
            column_name(position)
        )));
    }
    let width = header.len();
    let mut shaped_cells = width;
    if shaped_cells > MAX_FILE_CELLS {
        return Err(Error::validation(format!(
            "The import file has more than {MAX_FILE_CELLS} cells; at most {MAX_FILE_CELLS} are accepted."
        )));
    }
    let mut data = Vec::new();
    for (index, (source, mut row)) in rows.enumerate() {
        if data.len() >= MAX_FILE_ROWS {
            return Err(Error::validation(format!(
                "The import file has more than {MAX_FILE_ROWS} data rows; at most {MAX_FILE_ROWS} are accepted."
            )));
        }
        if let Some(extra) = row.iter().skip(width).position(|cell| !cell.is_empty()) {
            return Err(Error::validation(format!(
                "Data row {} has a value in column {}, after the last titled column.",
                index + 1,
                column_name(width + extra)
            )));
        }
        shaped_cells = shaped_cells.saturating_add(width);
        if shaped_cells > MAX_FILE_CELLS {
            return Err(Error::validation(format!(
                "The import file has more than {MAX_FILE_CELLS} cells; at most {MAX_FILE_CELLS} are accepted."
            )));
        }
        row.resize(width, String::new());
        if let Some(ref mut positions) = positions {
            positions.push(csv_lines.get(source).copied().unwrap_or(source + 1));
        }
        data.push(row);
    }
    Ok(ImportFile {
        header,
        rows: data,
        sheet,
    })
}

fn read_csv(
    bytes: &[u8],
    separator: char,
    mut source_lines: Option<&mut Vec<usize>>,
) -> Result<Vec<Vec<String>>> {
    let delimiter = match separator {
        ',' => b',',
        ';' => b';',
        '\t' => b'\t',
        other => {
            return Err(Error::validation(format!(
                "The CSV separator {other:?} is not supported; use a comma, a semicolon or a tab."
            )));
        }
    };
    let text = std::str::from_utf8(bytes).map_err(|error| {
        Error::validation(format!(
            "The CSV file is not UTF-8 text (byte {}); save it as CSV UTF-8.",
            error.valid_up_to() + 1
        ))
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(text.as_bytes());
    let mut rows = Vec::new();
    let mut budget = CellBudget::default();
    let mut cursor = 0;
    let mut line_number = 1;
    for record in reader.records() {
        let record = record
            .map_err(|error| Error::validation(format!("The CSV file cannot be read: {error}")))?;
        if let Some(ref mut lines) = source_lines {
            // csv's saved position can precede blank lines and the LF of a CRLF.
            // Advance across those bytes to the first character of this record.
            let pos = record.position().ok_or_else(|| {
                Error::programming("CSV reader did not return a record position.")
            })?;
            let mut start = usize::try_from(pos.byte())
                .map_err(|_| Error::validation("CSV record offset is too large."))?;
            let bytes = text.as_bytes();
            while matches!(bytes.get(start), Some(b'\r' | b'\n')) {
                start += 1;
            }
            while cursor < start {
                if bytes[cursor] == b'\r'
                    || bytes[cursor] == b'\n' && (cursor == 0 || bytes[cursor - 1] != b'\r')
                {
                    line_number += 1;
                }
                cursor += 1;
            }
            lines.push(line_number);
        }
        check_counts(rows.len(), record.len())?;
        budget.charge_cells(record.len())?;
        let line = rows.len() + 1;
        let row = record
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                check_cell(cell, || format!("{}{line}", column_name(column)))?;
                budget.charge_text(cell.len())?;
                Ok(cell.to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        rows.push(row);
    }
    Ok(rows)
}

/// `rows` already read (titles included) and the width of the next row, against the limits.
fn check_counts(rows: usize, width: usize) -> Result<()> {
    if rows > MAX_FILE_ROWS {
        return Err(Error::validation(format!(
            "The import file has more than {MAX_FILE_ROWS} data rows; at most {MAX_FILE_ROWS} are accepted."
        )));
    }
    if width > MAX_FILE_COLUMNS {
        return Err(Error::validation(format!(
            "The import file has {width} columns; at most {MAX_FILE_COLUMNS} are accepted."
        )));
    }
    Ok(())
}

fn check_cell(text: &str, cell: impl FnOnce() -> String) -> Result<()> {
    if text.len() > MAX_CELL_BYTES {
        return Err(Error::validation(format!(
            "Cell {} holds {} bytes of text; at most {MAX_CELL_BYTES} are accepted.",
            cell(),
            text.len()
        )));
    }
    Ok(())
}

/// The spreadsheet name of the 0-based `column` (`A`, `Z`, `AA`).
fn column_name(column: usize) -> String {
    let mut n = column + 1;
    let mut name = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        name.push(b'A' + u8::try_from(rem).unwrap_or(0));
        n = (n - 1) / 26;
    }
    name.reverse();
    String::from_utf8(name).unwrap_or_default()
}

// ---- XLSX -------------------------------------------------------------------------------------

fn not_a_workbook(detail: impl std::fmt::Display) -> Error {
    Error::validation(format!(
        "Cannot open this upload as a valid .xlsx spreadsheet ({detail}). Export it again in .xlsx format and retry."
    ))
}

struct Workbook<'a> {
    archive: zip::ZipArchive<Cursor<&'a [u8]>>,
}

impl<'a> Workbook<'a> {
    fn open(bytes: &'a [u8]) -> Result<Self> {
        let archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(not_a_workbook)?;
        if archive.len() > MAX_ENTRIES {
            return Err(not_a_workbook(format!(
                "{} archive entries, at most {MAX_ENTRIES}",
                archive.len()
            )));
        }
        Ok(Self { archive })
    }

    /// The unpacked text of part `name`; `None` when the workbook has no such part.
    fn part(&mut self, name: &str) -> Result<Option<String>> {
        let file = match self.archive.by_name(name) {
            Ok(file) => file,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(error) => return Err(not_a_workbook(format!("part {name}: {error}"))),
        };
        if file.size() > MAX_PART_BYTES {
            return Err(not_a_workbook(format!(
                "part {name} unpacks to {} bytes, at most {MAX_PART_BYTES}",
                file.size()
            )));
        }
        let mut bytes = Vec::new();
        // the declared size is the archive's claim; the read itself is bounded too
        file.take(MAX_PART_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| not_a_workbook(format!("part {name}: {error}")))?;
        if bytes.len() as u64 > MAX_PART_BYTES {
            return Err(not_a_workbook(format!(
                "part {name} unpacks to more than {MAX_PART_BYTES} bytes"
            )));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| not_a_workbook(format!("part {name} is not UTF-8 XML")))
    }

    fn required(&mut self, name: &str) -> Result<String> {
        self.part(name)?
            .ok_or_else(|| not_a_workbook(format!("the part {name} is missing")))
    }
}

/// One `<sheet>` of `xl/workbook.xml`.
struct SheetEntry {
    name: String,
    relation: String,
}

fn read_xlsx(bytes: &[u8], wanted: Option<&str>) -> Result<(Vec<Vec<String>>, String)> {
    let mut workbook = Workbook::open(bytes)?;
    let (sheets, date1904) = parse_workbook(&workbook.required("xl/workbook.xml")?)?;
    let sheet = match wanted {
        Some(name) => sheets.iter().find(|s| s.name == name).ok_or_else(|| {
            Error::validation(format!(
                "The workbook has no sheet named '{name}'; its sheets are {}.",
                sheet_names(&sheets)
            ))
        })?,
        None => match sheets.as_slice() {
            [only] => only,
            [] => return Err(not_a_workbook("it lists no sheet")),
            _ => {
                return Err(Error::validation(format!(
                    "The workbook has {} sheets ({}); choose the sheet to import.",
                    sheets.len(),
                    sheet_names(&sheets)
                )));
            }
        },
    };
    let targets = parse_relations(&workbook.required("xl/_rels/workbook.xml.rels")?)?;
    let target = targets
        .get(&sheet.relation)
        .ok_or_else(|| not_a_workbook(format!("sheet '{}' has no part", sheet.name)))?;
    let path = match target.strip_prefix('/') {
        Some(absolute) => absolute.to_owned(),
        None => format!("xl/{target}"),
    };
    let shared = match workbook.part("xl/sharedStrings.xml")? {
        Some(xml) => parse_shared_strings(&xml)?,
        None => Vec::new(),
    };
    let dates = match workbook.part("xl/styles.xml")? {
        Some(xml) => parse_date_styles(&xml)?,
        None => Vec::new(),
    };
    let sheet_name = sheet.name.clone();
    let sheet_xml = workbook.required(&path)?;
    let context = SheetContext {
        shared: &shared,
        dates: &dates,
        date1904,
    };
    let rows = parse_sheet(&sheet_xml, &context)?;
    drop(sheet_xml);
    drop(shared);
    drop(dates);
    Ok((rows, sheet_name))
}

fn sheet_names(sheets: &[SheetEntry]) -> String {
    let mut shown = sheets
        .iter()
        .take(MAX_SHEET_NAMES_IN_MESSAGE)
        .map(|s| format!("'{}'", s.name))
        .collect::<Vec<_>>()
        .join(", ");
    if sheets.len() > MAX_SHEET_NAMES_IN_MESSAGE {
        shown.push_str(&format!(
            ", and {} more",
            sheets.len() - MAX_SHEET_NAMES_IN_MESSAGE
        ));
    }
    shown
}

fn xml_error(part: &str, error: impl std::fmt::Display) -> Error {
    not_a_workbook(format!("{part}: {error}"))
}

fn reader(xml: &str) -> Reader<&[u8]> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    reader
}

fn attribute(start: &BytesStart<'_>, part: &str, wanted: &str) -> Result<Option<String>> {
    for attr in start.attributes() {
        let attr = attr.map_err(|error| xml_error(part, error))?;
        if attr.key.local_name().as_ref() == wanted {
            let value = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|error| xml_error(part, error))?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

/// The text an entity reference stands for (the five predefined entities and character
/// references; anything else is refused).
fn entity_char(reference: &BytesRef<'_>, part: &str) -> Result<char> {
    if reference.is_char_ref() {
        return match reference.resolve_char_ref() {
            Ok(Some(ch)) => Ok(ch),
            Ok(None) => Err(xml_error(part, "an invalid character reference")),
            Err(error) => Err(xml_error(part, error)),
        };
    }
    match &**reference {
        "amp" => Ok('&'),
        "lt" => Ok('<'),
        "gt" => Ok('>'),
        "quot" => Ok('"'),
        "apos" => Ok('\''),
        other => Err(xml_error(part, format!("unknown entity &{other};"))),
    }
}

fn parse_workbook(xml: &str) -> Result<(Vec<SheetEntry>, bool)> {
    const PART: &str = "xl/workbook.xml";
    let mut reader = reader(xml);
    let mut sheets = Vec::new();
    let mut date1904 = false;
    loop {
        match reader.read_event().map_err(|e| xml_error(PART, e))? {
            Event::Start(start) | Event::Empty(start) => match start.local_name().as_ref() {
                "workbookPr" => {
                    date1904 = matches!(
                        attribute(&start, PART, "date1904")?.as_deref(),
                        Some("1" | "true")
                    );
                }
                "sheet" => {
                    if sheets.len() >= MAX_SHEETS {
                        return Err(xml_error(PART, format!("more than {MAX_SHEETS} sheets")));
                    }
                    let name = attribute(&start, PART, "name")?
                        .ok_or_else(|| xml_error(PART, "a sheet without a name"))?;
                    let relation = attribute(&start, PART, "id")?
                        .ok_or_else(|| xml_error(PART, "a sheet without a part"))?;
                    sheets.push(SheetEntry { name, relation });
                }
                _ => {}
            },
            Event::DocType(_) => return Err(xml_error(PART, "a DOCTYPE declaration")),
            Event::Eof => return Ok((sheets, date1904)),
            _ => {}
        }
    }
}

fn parse_relations(xml: &str) -> Result<HashMap<String, String>> {
    const PART: &str = "xl/_rels/workbook.xml.rels";
    let mut reader = reader(xml);
    let mut targets = HashMap::new();
    loop {
        match reader.read_event().map_err(|e| xml_error(PART, e))? {
            Event::Start(start) | Event::Empty(start)
                if start.local_name().as_ref() == "Relationship" =>
            {
                if let (Some(id), Some(target)) = (
                    attribute(&start, PART, "Id")?,
                    attribute(&start, PART, "Target")?,
                ) {
                    if targets.len() >= MAX_RELATIONS && !targets.contains_key(&id) {
                        return Err(xml_error(
                            PART,
                            format!("more than {MAX_RELATIONS} relationships"),
                        ));
                    }
                    targets.insert(id, target);
                }
            }
            Event::DocType(_) => return Err(xml_error(PART, "a DOCTYPE declaration")),
            Event::Eof => return Ok(targets),
            _ => {}
        }
    }
}

fn push_shared_chunk(
    current: &mut Option<String>,
    total_bytes: &mut usize,
    chunk: &str,
    part: &str,
) -> Result<()> {
    let Some(value) = current.as_mut() else {
        return Ok(());
    };
    if value.len().saturating_add(chunk.len()) > MAX_CELL_BYTES {
        return Err(xml_error(
            part,
            format!("a shared string exceeds {MAX_CELL_BYTES} bytes"),
        ));
    }
    let next_total = total_bytes.saturating_add(chunk.len());
    if next_total > MAX_SHARED_STRING_BYTES {
        return Err(xml_error(
            part,
            format!("shared strings exceed {MAX_SHARED_STRING_BYTES} bytes"),
        ));
    }
    *total_bytes = next_total;
    value.push_str(chunk);
    Ok(())
}

fn push_shared_entry(
    strings: &mut Vec<String>,
    total_bytes: &mut usize,
    value: String,
    part: &str,
) -> Result<()> {
    if strings.len() >= MAX_SHARED_STRINGS {
        return Err(xml_error(
            part,
            format!("more than {MAX_SHARED_STRINGS} shared strings"),
        ));
    }
    let next_total = total_bytes.saturating_add(std::mem::size_of::<String>());
    if next_total > MAX_SHARED_STRING_BYTES {
        return Err(xml_error(
            part,
            format!("shared strings exceed {MAX_SHARED_STRING_BYTES} bytes"),
        ));
    }
    *total_bytes = next_total;
    strings.push(value);
    Ok(())
}

fn parse_shared_strings(xml: &str) -> Result<Vec<String>> {
    const PART: &str = "xl/sharedStrings.xml";
    let mut reader = reader(xml);
    let mut strings = Vec::new();
    let mut total_bytes = 0usize;
    let mut current: Option<String> = None;
    // depth inside <rPh> (phonetic runs are not part of the text) and whether a <t> is open
    let (mut phonetic, mut text) = (0usize, false);
    loop {
        match reader.read_event().map_err(|e| xml_error(PART, e))? {
            Event::Start(start) => match start.local_name().as_ref() {
                "si" => current = Some(String::new()),
                "rPh" => phonetic += 1,
                "t" => text = phonetic == 0,
                _ => {}
            },
            Event::Empty(start) if start.local_name().as_ref() == "si" => {
                push_shared_entry(&mut strings, &mut total_bytes, String::new(), PART)?;
            }
            Event::End(end) => match end.local_name().as_ref() {
                "si" => {
                    let value = current.take().unwrap_or_default();
                    push_shared_entry(&mut strings, &mut total_bytes, value, PART)?;
                }
                "rPh" => phonetic = phonetic.saturating_sub(1),
                "t" => text = false,
                _ => {}
            },
            Event::Text(chunk) if text => {
                push_shared_chunk(&mut current, &mut total_bytes, &chunk.xml10_content(), PART)?;
            }
            Event::CData(chunk) if text => {
                push_shared_chunk(&mut current, &mut total_bytes, &chunk.xml10_content(), PART)?;
            }
            Event::GeneralRef(entity) if text => {
                let mut buf = [0u8; 4];
                let ch = entity_char(&entity, PART)?;
                push_shared_chunk(
                    &mut current,
                    &mut total_bytes,
                    ch.encode_utf8(&mut buf),
                    PART,
                )?;
            }
            Event::DocType(_) => return Err(xml_error(PART, "a DOCTYPE declaration")),
            Event::Eof => return Ok(strings),
            _ => {}
        }
    }
}

/// Whether each cell style (`cellXfs`, by index) shows its number as a date or time.
fn parse_date_styles(xml: &str) -> Result<Vec<bool>> {
    const PART: &str = "xl/styles.xml";
    let mut reader = reader(xml);
    let mut custom: HashMap<u32, bool> = HashMap::new();
    let mut style_xfs: Vec<u32> = Vec::new();
    let mut styles = Vec::new();
    let mut in_cell_style_xfs = false;
    let mut in_cell_xfs = false;
    loop {
        match reader.read_event().map_err(|e| xml_error(PART, e))? {
            Event::Start(start) if start.local_name().as_ref() == "cellStyleXfs" => {
                in_cell_style_xfs = true;
            }
            Event::Start(start) if start.local_name().as_ref() == "cellXfs" => in_cell_xfs = true,
            Event::Start(start) | Event::Empty(start) => match start.local_name().as_ref() {
                "numFmt" => {
                    let id = attribute(&start, PART, "numFmtId")?
                        .and_then(|id| id.parse::<u32>().ok())
                        .ok_or_else(|| xml_error(PART, "a number format without an id"))?;
                    let code = attribute(&start, PART, "formatCode")?
                        .ok_or_else(|| xml_error(PART, "a number format without a formatCode"))?;
                    if custom.len() >= MAX_STYLES && !custom.contains_key(&id) {
                        return Err(xml_error(
                            PART,
                            format!("more than {MAX_STYLES} number formats"),
                        ));
                    }
                    custom.insert(id, is_date_code(&code));
                }
                "xf" if in_cell_style_xfs => {
                    if style_xfs.len() >= MAX_STYLES {
                        return Err(xml_error(
                            PART,
                            format!("more than {MAX_STYLES} cell style definitions"),
                        ));
                    }
                    let id = attribute(&start, PART, "numFmtId")?
                        .map(|id| id.parse::<u32>())
                        .transpose()
                        .map_err(|_| xml_error(PART, "a cell style with an invalid number format"))?
                        .unwrap_or(0);
                    style_xfs.push(id);
                }
                "xf" if in_cell_xfs => {
                    if styles.len() >= MAX_STYLES {
                        return Err(xml_error(
                            PART,
                            format!("more than {MAX_STYLES} cell styles"),
                        ));
                    }
                    let id = match attribute(&start, PART, "numFmtId")? {
                        Some(raw) => raw.parse::<u32>().map_err(|_| {
                            xml_error(PART, "a cell style with an invalid number format")
                        })?,
                        None => {
                            match attribute(&start, PART, "xfId")? {
                                Some(raw) => {
                                    let xf_id = raw.parse::<usize>().map_err(|_| {
                                        xml_error(PART, "a cell style with an invalid xfId")
                                    })?;
                                    *style_xfs.get(xf_id).ok_or_else(|| {
                                    xml_error(
                                        PART,
                                        format!("a cell style references missing cellStyleXf {xf_id}"),
                                    )
                                })?
                                }
                                None => 0,
                            }
                        }
                    };
                    styles.push(
                        custom
                            .get(&id)
                            .copied()
                            .unwrap_or_else(|| is_date_builtin(id)),
                    );
                }
                _ => {}
            },
            Event::End(end) if end.local_name().as_ref() == "cellStyleXfs" => {
                in_cell_style_xfs = false;
            }
            Event::End(end) if end.local_name().as_ref() == "cellXfs" => in_cell_xfs = false,
            Event::DocType(_) => return Err(xml_error(PART, "a DOCTYPE declaration")),
            Event::Eof => return Ok(styles),
            _ => {}
        }
    }
}

/// The built-in number formats that show a date or a time.
fn is_date_builtin(id: u32) -> bool {
    matches!(id, 14..=22 | 45..=47)
}

/// Whether a custom number format shows a date or a time: it has a date or time letter outside
/// quoted text, escapes and bracketed parts (colours, conditions, locales).
fn is_date_code(code: &str) -> bool {
    let mut chars = code.chars();
    let mut found = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                for inner in chars.by_ref() {
                    if inner == '"' {
                        break;
                    }
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            '[' => {
                let bracket: String = chars.by_ref().take_while(|&c| c != ']').collect();
                // elapsed time ([h], [mm], [ss]) is a time format
                if !bracket.is_empty()
                    && bracket
                        .chars()
                        .all(|c| matches!(c, 'h' | 'H' | 'm' | 'M' | 's' | 'S'))
                {
                    found = true;
                }
            }
            'd' | 'D' | 'm' | 'M' | 'y' | 'Y' | 'h' | 'H' | 's' | 'S' => found = true,
            _ => {}
        }
    }
    found && !code.eq_ignore_ascii_case("general")
}

struct SheetContext<'a> {
    shared: &'a [String],
    dates: &'a [bool],
    date1904: bool,
}

/// One `<c>` being read.
#[derive(Default)]
struct CellState {
    reference: Option<String>,
    kind: Option<String>,
    style: usize,
    value: Option<String>,
    inline: Option<String>,
    formula: bool,
}

/// Where the text being read goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Capture {
    None,
    Value,
    Inline,
}

fn parse_sheet(xml: &str, context: &SheetContext<'_>) -> Result<Vec<Vec<String>>> {
    const PART: &str = "the worksheet";
    let mut reader = reader(xml);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Option<(usize, Vec<String>)> = None;
    let mut cell: Option<CellState> = None;
    let mut capture = Capture::None;
    let mut phonetic = 0usize;
    let mut budget = CellBudget::default();
    loop {
        match reader.read_event().map_err(|e| xml_error(PART, e))? {
            Event::Start(start) => {
                open_sheet_element(
                    &start,
                    &mut rows,
                    &mut row,
                    &mut cell,
                    &mut capture,
                    &mut phonetic,
                )?;
            }
            Event::Empty(start) => {
                open_sheet_element(
                    &start,
                    &mut rows,
                    &mut row,
                    &mut cell,
                    &mut capture,
                    &mut phonetic,
                )?;
                close_sheet_element(
                    start.local_name().as_ref(),
                    context,
                    &mut rows,
                    &mut row,
                    &mut cell,
                    &mut capture,
                    &mut phonetic,
                    &mut budget,
                )?;
            }
            Event::End(end) => {
                close_sheet_element(
                    end.local_name().as_ref(),
                    context,
                    &mut rows,
                    &mut row,
                    &mut cell,
                    &mut capture,
                    &mut phonetic,
                    &mut budget,
                )?;
            }
            Event::Text(chunk) => append(&mut cell, capture, &chunk.xml10_content())?,
            Event::CData(chunk) => append(&mut cell, capture, &chunk.xml10_content())?,
            Event::GeneralRef(entity) if capture != Capture::None => {
                let mut buf = [0u8; 4];
                let ch = entity_char(&entity, PART)?;
                append(&mut cell, capture, ch.encode_utf8(&mut buf))?;
            }
            Event::DocType(_) => return Err(xml_error(PART, "a DOCTYPE declaration")),
            Event::Eof => return Ok(rows),
            _ => {}
        }
    }
}

fn append(cell: &mut Option<CellState>, capture: Capture, text: &str) -> Result<()> {
    let Some(cell) = cell.as_mut() else {
        return Ok(());
    };
    let target = match capture {
        Capture::None => return Ok(()),
        Capture::Value => cell.value.get_or_insert_with(String::new),
        Capture::Inline => cell.inline.get_or_insert_with(String::new),
    };
    if target.len() + text.len() > MAX_CELL_BYTES {
        return Err(Error::validation(format!(
            "Cell {} holds more than {MAX_CELL_BYTES} bytes of text.",
            cell.reference.as_deref().unwrap_or("?")
        )));
    }
    target.push_str(text);
    Ok(())
}

fn open_sheet_element(
    start: &BytesStart<'_>,
    rows: &mut [Vec<String>],
    row: &mut Option<(usize, Vec<String>)>,
    cell: &mut Option<CellState>,
    capture: &mut Capture,
    phonetic: &mut usize,
) -> Result<()> {
    const PART: &str = "the worksheet";
    match start.local_name().as_ref() {
        "row" => {
            let index =
                match attribute(start, PART, "r")? {
                    Some(r) => {
                        r.parse::<usize>().ok().filter(|r| *r >= 1).ok_or_else(|| {
                            xml_error(PART, format!("an invalid row number '{r}'"))
                        })? - 1
                    }
                    None => rows.len(),
                };
            if index < rows.len() {
                return Err(xml_error(
                    PART,
                    format!("row {} is out of order", index + 1),
                ));
            }
            check_counts(index, 0)?;
            *row = Some((index, Vec::new()));
        }
        "c" => {
            let style = attribute(start, PART, "s")?
                .map(|s| s.parse::<usize>())
                .transpose()
                .map_err(|_| xml_error(PART, "a cell with an invalid style"))?
                .unwrap_or(0);
            *cell = Some(CellState {
                reference: attribute(start, PART, "r")?,
                kind: attribute(start, PART, "t")?,
                style,
                ..CellState::default()
            });
        }
        "v" => {
            *capture = Capture::Value;
            if let Some(cell) = cell.as_mut() {
                cell.value.get_or_insert_with(String::new);
            }
        }
        "f" => {
            if let Some(cell) = cell.as_mut() {
                cell.formula = true;
            }
        }
        "rPh" => *phonetic += 1,
        "t" if *phonetic == 0 && cell.is_some() => *capture = Capture::Inline,
        _ => {}
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the reader's state, passed by part"
)]
fn close_sheet_element(
    name: &str,
    context: &SheetContext<'_>,
    rows: &mut Vec<Vec<String>>,
    row: &mut Option<(usize, Vec<String>)>,
    cell: &mut Option<CellState>,
    capture: &mut Capture,
    phonetic: &mut usize,
    budget: &mut CellBudget,
) -> Result<()> {
    match name {
        "v" | "t" => *capture = Capture::None,
        "rPh" => *phonetic = phonetic.saturating_sub(1),
        "c" => {
            let Some(done) = cell.take() else {
                return Ok(());
            };
            let Some((row_index, cells)) = row.as_mut() else {
                return Err(xml_error("the worksheet", "a cell outside a row"));
            };
            let column = match done.reference.as_deref() {
                Some(reference) => cell_column(reference, *row_index)?,
                None => cells.len(),
            };
            if column < cells.len() {
                return Err(xml_error(
                    "the worksheet",
                    format!("cell {} is out of order", column_name(column)),
                ));
            }
            check_counts(*row_index, column + 1)?;
            let reference = format!("{}{}", column_name(column), *row_index + 1);
            let text = cell_text(&done, &reference, context)?;
            let added_cells = (column + 1).saturating_sub(cells.len());
            budget.charge_cells(added_cells)?;
            budget.charge_text(text.len())?;
            cells.resize(column, String::new());
            cells.push(text);
        }
        "row" => {
            if let Some((index, cells)) = row.take() {
                rows.resize(index, Vec::new());
                rows.push(cells);
            }
        }
        _ => {}
    }
    Ok(())
}

/// The 0-based column of a reference such as `C5`; its row must be `row` (0-based).
fn cell_column(reference: &str, row: usize) -> Result<usize> {
    let letters = reference
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(reference.len());
    let (column, number) = reference.split_at(letters);
    let valid = !column.is_empty()
        && column.len() <= 3
        && column.bytes().all(|b| b.is_ascii_uppercase())
        && number.parse::<usize>().ok() == Some(row + 1);
    if !valid {
        return Err(xml_error(
            "the worksheet",
            format!("an invalid cell reference '{reference}'"),
        ));
    }
    Ok(column
        .bytes()
        .fold(0usize, |n, b| n * 26 + usize::from(b - b'A' + 1))
        - 1)
}

fn cell_text(cell: &CellState, reference: &str, context: &SheetContext<'_>) -> Result<String> {
    let value = cell.value.as_deref();
    if cell.formula && value.is_none() && cell.kind.as_deref() != Some("inlineStr") {
        return Err(Error::validation(format!(
            "Cell {reference} holds a formula saved without its value. Open the file in a spreadsheet program and save it again, or replace the formula by its value."
        )));
    }
    let text: Cow<'_, str> = match cell.kind.as_deref() {
        Some("s") => {
            let index = value
                .and_then(|v| v.trim().parse::<usize>().ok())
                .ok_or_else(|| {
                    xml_error(
                        "the worksheet",
                        format!("cell {reference} has no string index"),
                    )
                })?;
            Cow::Borrowed(
                context
                    .shared
                    .get(index)
                    .ok_or_else(|| {
                        xml_error(
                            "the worksheet",
                            format!("cell {reference} names a missing shared string"),
                        )
                    })?
                    .as_str(),
            )
        }
        Some("inlineStr") => Cow::Borrowed(cell.inline.as_deref().unwrap_or("")),
        Some("str") => Cow::Borrowed(value.unwrap_or("")),
        Some("b") => match value.map(str::trim) {
            Some("1") => Cow::Borrowed("1"),
            Some("0") => Cow::Borrowed("0"),
            other => {
                return Err(xml_error(
                    "the worksheet",
                    format!("cell {reference} has an invalid boolean {other:?}"),
                ));
            }
        },
        Some("e") => {
            return Err(Error::validation(format!(
                "Cell {reference} holds the error value {}; correct it before importing.",
                value.unwrap_or("#VALUE!")
            )));
        }
        Some("d") => Cow::Owned(iso_date_text(value.unwrap_or(""), reference)?),
        None | Some("n") => match value.map(str::trim) {
            None | Some("") => Cow::Borrowed(""),
            Some(number) => {
                let is_date = if cell.style == 0 {
                    context.dates.first().copied().unwrap_or(false)
                } else {
                    *context.dates.get(cell.style).ok_or_else(|| {
                        xml_error(
                            "the worksheet",
                            format!("cell {reference} references missing style {}", cell.style),
                        )
                    })?
                };
                if is_date {
                    Cow::Owned(serial_date(number, context.date1904, reference)?)
                } else {
                    Cow::Borrowed(number)
                }
            }
        },
        Some(other) => {
            return Err(xml_error(
                "the worksheet",
                format!("cell {reference} has the unknown type '{other}'"),
            ));
        }
    };
    check_cell(&text, || reference.to_owned())?;
    Ok(text.into_owned())
}

/// A cell of type `d` holds an ISO 8601 date or date-time: kept as `YYYY-MM-DD[ HH:MM:SS]`.
fn iso_date_text(value: &str, reference: &str) -> Result<String> {
    let value = value.trim();
    let invalid = || {
        Error::validation(format!(
            "Cell {reference} holds the date '{value}', which cannot be read."
        ))
    };
    let (date_str, time_part) = match value.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (value, None),
    };
    let date_bytes = date_str.as_bytes();
    if date_bytes.len() != 10 || date_bytes[4] != b'-' || date_bytes[7] != b'-' {
        return Err(invalid());
    }
    let year: i32 = date_str[0..4].parse().map_err(|_| invalid())?;
    let month_num: u8 = date_str[5..7].parse().map_err(|_| invalid())?;
    let day: u8 = date_str[8..10].parse().map_err(|_| invalid())?;
    let month = time::Month::try_from(month_num).map_err(|_| invalid())?;
    let date = time::Date::from_calendar_date(year, month, day).map_err(|_| invalid())?;
    let formatted_date = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    );
    let Some(raw_time) = time_part else {
        return Ok(formatted_date);
    };
    let raw_time = raw_time.strip_suffix('Z').unwrap_or(raw_time);
    if raw_time.contains('+') || raw_time.contains('-') {
        return Err(Error::validation(format!(
            "Cell {reference} holds the date '{value}' with a timezone offset, which is not supported."
        )));
    }
    let (hms, frac) = match raw_time.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (raw_time, None),
    };
    if let Some(frac) = frac
        && (frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid());
    }
    let hms_bytes = hms.as_bytes();
    if hms_bytes.len() != 8 || hms_bytes[2] != b':' || hms_bytes[5] != b':' {
        return Err(invalid());
    }
    let hour: u8 = hms[0..2].parse().map_err(|_| invalid())?;
    let minute: u8 = hms[3..5].parse().map_err(|_| invalid())?;
    let second: u8 = hms[6..8].parse().map_err(|_| invalid())?;
    let parsed_time = time::Time::from_hms(hour, minute, second).map_err(|_| invalid())?;
    if parsed_time == time::Time::MIDNIGHT {
        Ok(formatted_date)
    } else {
        Ok(format!(
            "{formatted_date} {:02}:{:02}:{:02}",
            parsed_time.hour(),
            parsed_time.minute(),
            parsed_time.second()
        ))
    }
}

/// A date serial number (days since the workbook's epoch, the fraction being the time of day)
/// as `YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS`.
fn serial_date(number: &str, date1904: bool, reference: &str) -> Result<String> {
    let unreadable = || {
        Error::validation(format!(
            "Cell {reference} is formatted as a date but holds '{number}', which is not a date."
        ))
    };
    let serial: f64 = number.parse().map_err(|_| unreadable())?;
    if !serial.is_finite() || serial < 0.0 || serial >= 2_958_466.0 {
        return Err(unreadable());
    }
    // the 1900 system counts a 29 February 1900 that never was: day 61 is 1 March 1900, so
    // counting from 30 December 1899 is right from there on; earlier serials are refused.
    if serial < 1.0 && !date1904 {
        return Err(Error::validation(format!(
            "Cell {reference} holds a time of day without a date; import it as text or as a number."
        )));
    }
    if !date1904 && serial < 61.0 {
        return Err(Error::validation(format!(
            "Cell {reference} holds a date before 1 March 1900, which cannot be read exactly."
        )));
    }
    let seconds_total = (serial * 86_400.0).round();
    let days = (seconds_total / 86_400.0).floor();
    let seconds = (seconds_total - days * 86_400.0) as i64;
    let epoch: i64 = if date1904 { 2_416_481 } else { 2_415_019 };
    let julian = i32::try_from(epoch + days as i64).map_err(|_| unreadable())?;
    let date = time::Date::from_julian_day(julian).map_err(|_| unreadable())?;
    let day = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    );
    if seconds == 0 {
        return Ok(day);
    }
    Ok(format!(
        "{day} {:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// A workbook of `sheets` (name, sheet XML), with optional shared strings and styles.
    fn workbook(
        sheets: &[(&str, &str)],
        shared: Option<&str>,
        styles: Option<&str>,
        date1904: bool,
    ) -> Vec<u8> {
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        {
            let mut add = |name: &str, body: &str| {
                out.start_file(name, options).unwrap();
                out.write_all(body.as_bytes()).unwrap();
            };
            let entries: String = sheets
                .iter()
                .enumerate()
                .map(|(i, (name, _))| {
                    format!(
                        r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#,
                        n = i + 1
                    )
                })
                .collect();
            add(
                "xl/workbook.xml",
                &format!(
                    r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><workbookPr date1904="{}"/><sheets>{entries}</sheets></workbook>"#,
                    u8::from(date1904)
                ),
            );
            let relations: String = sheets
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    format!(
                        r#"<Relationship Id="rId{n}" Target="worksheets/sheet{n}.xml"/>"#,
                        n = i + 1
                    )
                })
                .collect();
            add(
                "xl/_rels/workbook.xml.rels",
                &format!(r#"<Relationships>{relations}</Relationships>"#),
            );
            for (i, (_, xml)) in sheets.iter().enumerate() {
                add(&format!("xl/worksheets/sheet{}.xml", i + 1), xml);
            }
            if let Some(shared) = shared {
                add("xl/sharedStrings.xml", shared);
            }
            if let Some(styles) = styles {
                add("xl/styles.xml", styles);
            }
        }
        out.finish().unwrap().into_inner()
    }
    use zip::ZipWriter;

    fn sheet(rows: &str) -> String {
        format!(r#"<worksheet><sheetData>{rows}</sheetData></worksheet>"#)
    }

    const SHARED: &str = r#"<sst><si><t>Name</t></si><si><t>Start</t></si><si><r><t>Ada </t></r><r><t>Lovelace</t></r><rPh><t>ignored</t></rPh></si><si><t>Tom &amp; Jerry</t></si></sst>"#;
    const STYLES: &str = r#"<styleSheet><numFmts><numFmt numFmtId="164" formatCode="dd/mm/yyyy"/><numFmt numFmtId="165" formatCode="0.00&quot;d&quot;"/></numFmts><cellStyleXfs><xf numFmtId="14"/></cellStyleXfs><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="22"/><xf numFmtId="165"/></cellXfs></styleSheet>"#;

    fn xlsx(bytes: &[u8]) -> Result<ImportFile> {
        read_import_file(bytes, ImportFileType::Xlsx, &ImportFileOptions::default())
    }

    #[test]
    fn a_workbook_reads_into_titles_and_rows_like_a_csv_file() {
        let rows = sheet(concat!(
            r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="inlineStr"><is><t>Active</t></is></c><c r="D1" t="str"><f>"Score"</f><v>Score</v></c></row>"#,
            r#"<row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2" s="1"><v>45366</v></c><c r="C2" t="b"><v>1</v></c><c r="D2"><f>1+1</f><v>2</v></c></row>"#,
            r#"<row r="3"><c r="A3" t="s"><v>3</v></c><c r="B3" s="2"><v>45366.5</v></c><c r="D3" s="3"><v>1.5</v></c></row>"#,
            r#"<row r="5"><c r="A5"><v>0.1</v></c></row>"#,
        ));
        let file = xlsx(&workbook(
            &[("Data", &rows)],
            Some(SHARED),
            Some(STYLES),
            false,
        ))
        .unwrap();
        assert_eq!(file.sheet.as_deref(), Some("Data"));
        assert_eq!(file.header, ["Name", "Start", "Active", "Score"]);
        assert_eq!(
            file.rows,
            [
                vec!["Ada Lovelace", "2024-03-15", "1", "2"],
                vec!["Tom & Jerry", "2024-03-15 12:00:00", "", "1.5"],
                vec!["0.1", "", "", ""],
            ]
        );
        let csv = read_import_file(
            "Name,Start\r\nAda,2024-03-15\r\n".as_bytes(),
            ImportFileType::Csv,
            &ImportFileOptions::default(),
        )
        .unwrap();
        assert_eq!(csv.header, ["Name", "Start"]);
        assert_eq!(csv.rows, [vec!["Ada", "2024-03-15"]]);
        assert_eq!(csv.sheet, None);
    }

    #[test]
    fn web_import_reports_real_rows_after_blanks_and_multiline_cells() {
        let (csv, lines) = read_import_file_with_positions(
            b"\r\nname,note\r\n\r\nAda,\"line 1\r\nline 2\"\r\n,\r\nTia,ok\r\n",
            ImportFileType::Csv,
            &ImportFileOptions::default(),
        )
        .unwrap();
        assert_eq!(csv.rows.len(), 2);
        assert_eq!(lines, [4, 7]);
        let rows = sheet(
            r#"<row r="3"><c r="A3" t="inlineStr"><is><t>Name</t></is></c></row><row r="10"><c r="A10" t="inlineStr"><is><t>Tia</t></is></c></row>"#,
        );
        let (_, lines) = read_import_file_with_positions(
            &workbook(&[("Data", &rows)], None, None, false),
            ImportFileType::Xlsx,
            &ImportFileOptions::default(),
        )
        .unwrap();
        assert_eq!(lines, [10]);
    }

    #[test]
    fn the_1904_date_system_is_followed() {
        let rows = sheet(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Day</t></is></c></row><row r="2"><c r="A2" s="1"><v>0</v></c></row>"#,
        );
        let file = xlsx(&workbook(&[("S", &rows)], None, Some(STYLES), true)).unwrap();
        assert_eq!(file.rows, [vec!["1904-01-01"]]);
    }

    #[test]
    fn several_sheets_need_a_choice_and_an_unknown_sheet_is_named() {
        let first = sheet(r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Help</t></is></c></row>"#);
        let second = sheet(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>Ada</t></is></c></row>"#,
        );
        let bytes = workbook(&[("Help", &first), ("Data", &second)], None, None, false);
        assert_eq!(import_sheet_names(&bytes).unwrap(), ["Help", "Data"]);
        let error = xlsx(&bytes).unwrap_err();
        assert_eq!(
            error.message(),
            "The workbook has 2 sheets ('Help', 'Data'); choose the sheet to import."
        );
        let chosen = read_import_file(
            &bytes,
            ImportFileType::Xlsx,
            &ImportFileOptions {
                sheet: Some("Data".to_owned()),
                separator: None,
            },
        )
        .unwrap();
        assert_eq!(chosen.rows, [vec!["Ada"]]);
        let unknown = read_import_file(
            &bytes,
            ImportFileType::Xlsx,
            &ImportFileOptions {
                sheet: Some("Other".to_owned()),
                separator: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            unknown.message(),
            "The workbook has no sheet named 'Other'; its sheets are 'Help', 'Data'."
        );
    }

    #[test]
    fn unreadable_cells_are_refused_by_reference() {
        let header = r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Value</t></is></c></row>"#;
        let cases = [
            (
                r#"<row r="2"><c r="A2"><f>SUM(B1:B9)</f></c></row>"#,
                "Cell A2 holds a formula saved without its value.",
            ),
            (
                r#"<row r="2"><c r="A2" t="e"><v>#N/A</v></c></row>"#,
                "Cell A2 holds the error value #N/A; correct it before importing.",
            ),
            (
                r#"<row r="2"><c r="A2" s="1"><v>59</v></c></row>"#,
                "Cell A2 holds a date before 1 March 1900, which cannot be read exactly.",
            ),
        ];
        for (row, expected) in cases {
            let rows = sheet(&format!("{header}{row}"));
            let error = xlsx(&workbook(&[("S", &rows)], None, Some(STYLES), false)).unwrap_err();
            assert!(error.message().starts_with(expected), "{}", error.message());
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        }
    }

    #[test]
    fn values_beyond_the_titled_columns_and_empty_titles_are_refused() {
        let rows = sheet(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c></row><row r="2"><c r="C2" t="inlineStr"><is><t>stray</t></is></c></row>"#,
        );
        let error = xlsx(&workbook(&[("S", &rows)], None, None, false)).unwrap_err();
        assert_eq!(
            error.message(),
            "Data row 1 has a value in column C, after the last titled column."
        );
        let error = read_import_file(
            b"Name,,Code\nA,B,C\n",
            ImportFileType::Csv,
            &ImportFileOptions::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.message(),
            "Column B of the title row is empty; every imported column needs a title."
        );
    }

    #[test]
    fn limits_and_wrong_files_fail_loudly() {
        let error = xlsx(b"Name,Code\n").unwrap_err();
        assert!(
            error
                .message()
                .starts_with("Cannot open this upload as a valid .xlsx spreadsheet")
        );
        let big = vec![b'a'; MAX_FILE_BYTES + 1];
        let error =
            read_import_file(&big, ImportFileType::Csv, &ImportFileOptions::default()).unwrap_err();
        assert!(error.message().contains("at most 16777216 are accepted"));
        let far = sheet(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c></row><row r="200002"><c r="A200002"><v>1</v></c></row>"#,
        );
        let error = xlsx(&workbook(&[("S", &far)], None, None, false)).unwrap_err();
        assert!(error.message().contains("data rows"), "{}", error.message());
        let wide = sheet(r#"<row r="1"><c r="ZZZ1"><v>1</v></c></row>"#);
        let error = xlsx(&workbook(&[("S", &wide)], None, None, false)).unwrap_err();
        assert!(
            error.message().contains("columns; at most 1000"),
            "{}",
            error.message()
        );
        let error = read_import_file(
            &[0xff, 0xfe],
            ImportFileType::Csv,
            &ImportFileOptions::default(),
        )
        .unwrap_err();
        assert!(
            error
                .message()
                .starts_with("The CSV file is not UTF-8 text")
        );
        assert!(ImportFileType::parse("xls").is_err());
    }

    #[test]
    fn a_part_that_unpacks_beyond_its_bound_is_refused() {
        // a highly compressible shared-strings part declared over the bound
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        out.start_file("xl/workbook.xml", options).unwrap();
        out.write_all(
            br#"<workbook><sheets><sheet name="S" r:id="rId1" xmlns:r="r"/></sheets></workbook>"#,
        )
        .unwrap();
        out.start_file("xl/_rels/workbook.xml.rels", options)
            .unwrap();
        out.write_all(br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#)
            .unwrap();
        out.start_file("xl/sharedStrings.xml", options).unwrap();
        let chunk = vec![b' '; 1024 * 1024];
        for _ in 0..=MAX_PART_BYTES / (1024 * 1024) {
            out.write_all(&chunk).unwrap();
        }
        let bytes = out.finish().unwrap().into_inner();
        assert!(bytes.len() < MAX_FILE_BYTES);
        let error = xlsx(&bytes).unwrap_err();
        assert!(
            error.message().contains("unpacks to"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn csv_separators_and_bom() {
        let file = read_import_file(
            "\u{feff}Name;Code\nAda;1\n".as_bytes(),
            ImportFileType::Csv,
            &ImportFileOptions {
                sheet: None,
                separator: Some(';'),
            },
        )
        .unwrap();
        assert_eq!(file.header, ["Name", "Code"]);
        assert_eq!(file.rows, [vec!["Ada", "1"]]);
        assert!(
            read_import_file(
                b"a|b",
                ImportFileType::Csv,
                &ImportFileOptions {
                    sheet: None,
                    separator: Some('|'),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn date_formats_are_told_from_number_formats() {
        assert!(is_date_code("dd/mm/yyyy"));
        assert!(is_date_code("[$-421]d mmmm yyyy"));
        assert!(is_date_code("[h]:mm"));
        assert!(!is_date_code("0.00\"d\""));
        assert!(!is_date_code("0.00_d"));
        assert!(!is_date_code("0.00*m"));
        assert!(!is_date_code("[Red]#,##0"));
        assert!(!is_date_code("General"));
        assert!(!is_date_code("0.00E+00"));
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(cell_column("AA3", 2).unwrap(), 26);
        assert!(cell_column("A3", 1).is_err());
    }

    #[test]
    fn empty_string_formula_and_inherited_date_style_work() {
        let styles = r#"<styleSheet><cellStyleXfs><xf numFmtId="14"/></cellStyleXfs><cellXfs><xf numFmtId="0"/><xf xfId="0"/></cellXfs></styleSheet>"#;
        let rows = sheet(concat!(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Formula</t></is></c><c r="B1" t="inlineStr"><is><t>Day</t></is></c></row>"#,
            r#"<row r="2"><c r="A2" t="str"><f>IF(1=0,"x","")</f><v/></c><c r="B2" s="1"><v>45366</v></c></row>"#,
        ));
        let file = xlsx(&workbook(&[("S", &rows)], None, Some(styles), false)).unwrap();
        assert_eq!(file.rows, [vec!["", "2024-03-15"]]);
    }

    #[test]
    fn missing_style_index_and_bad_numfmt_fail_loudly() {
        let rows = sheet(concat!(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Day</t></is></c></row>"#,
            r#"<row r="2"><c r="A2" s="99"><v>45366</v></c></row>"#,
        ));
        let error = xlsx(&workbook(&[("S", &rows)], None, Some(STYLES), false)).unwrap_err();
        assert!(
            error.message().contains("missing style 99"),
            "{}",
            error.message()
        );
        let bad_styles = r#"<styleSheet><numFmts><numFmt numFmtId="164"/></numFmts><cellXfs><xf numFmtId="164"/></cellXfs></styleSheet>"#;
        let error = parse_date_styles(bad_styles).unwrap_err();
        assert!(
            error.message().contains("formatCode"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn iso_date_text_validates_calendar_and_refuses_offsets() {
        assert_eq!(iso_date_text("2024-03-15", "A2").unwrap(), "2024-03-15");
        assert_eq!(
            iso_date_text("2024-03-15T00:00:00.000Z", "A2").unwrap(),
            "2024-03-15"
        );
        assert_eq!(
            iso_date_text("2024-03-15T12:30:45.5Z", "A2").unwrap(),
            "2024-03-15 12:30:45"
        );
        let tz_err = iso_date_text("2024-03-15T12:30:45+02:00", "A2").unwrap_err();
        assert!(tz_err.message().contains("timezone offset"));
        assert!(iso_date_text("2024-02-30", "A2").is_err());
        assert!(iso_date_text("2024-03-15T25:00:00", "A2").is_err());
        assert!(iso_date_text("2024-03-15T12:00:00.abc", "A2").is_err());
    }

    #[test]
    fn cumulative_cell_and_text_budgets_block_amplification_and_padding() {
        // Shared-string amplification: one 64 KiB string referenced 600 times (~38 MiB > 32 MiB)
        let big = "a".repeat(64 * 1024);
        let shared = format!("<sst><si><t>Name</t></si><si><t>{big}</t></si></sst>");
        let mut rows = String::from(r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#);
        for row in 2..=601 {
            rows.push_str(&format!(
                r#"<row r="{row}"><c r="A{row}" t="s"><v>1</v></c></row>"#
            ));
        }
        let error = xlsx(&workbook(
            &[("S", &sheet(&rows))],
            Some(&shared),
            None,
            false,
        ))
        .unwrap_err();
        assert!(
            error.message().contains("bytes of text"),
            "{}",
            error.message()
        );

        // Dense-padding bomb in shape(): 1,000 titled columns x 600 sparse rows = 600,000 > 500,000
        let header: Vec<String> = (0..1_000).map(|i| format!("C{i}")).collect();
        let mut sparse = Vec::with_capacity(601);
        sparse.push(header);
        for _ in 0..600 {
            sparse.push(vec!["x".to_owned()]);
        }
        let error = shape(sparse, None, &[], None).unwrap_err();
        assert!(
            error.message().contains("more than 500000 cells"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn empty_shared_string_elements_are_bounded() {
        let mut xml = String::with_capacity((MAX_SHARED_STRINGS + 1) * 5 + 16);
        xml.push_str("<sst>");
        for _ in 0..=MAX_SHARED_STRINGS {
            xml.push_str("<si/>");
        }
        xml.push_str("</sst>");
        let error = parse_shared_strings(&xml).unwrap_err();
        assert!(
            error.message().contains("shared strings"),
            "{}",
            error.message()
        );
    }
}
