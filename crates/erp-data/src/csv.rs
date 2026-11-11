//! CSV data files.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::ast::{FieldOp, FieldValue, OpEnv, RecordOp, XmlId};
use crate::error::ParseError;
use crate::lines::LineCounter;
use crate::reader::ConvertMode;

/// One data row with its physical line in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvRow {
    line: usize,
    values: Vec<String>,
}

impl CsvRow {
    /// Physical line where the row starts (a quoted cell may span several lines).
    pub fn line(&self) -> usize {
        self.line
    }

    /// Cells of the loaded columns, as written.
    pub fn values(&self) -> &[String] {
        &self.values
    }
}

impl AsRef<[String]> for CsvRow {
    fn as_ref(&self) -> &[String] {
        &self.values
    }
}

/// A translation value from a CSV row (`column@lang`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CsvTranslation {
    line: usize,
    id: Option<String>,
    field: String,
    lang: String,
    value: String,
}

impl CsvTranslation {
    /// The physical line of the row.
    pub fn line(&self) -> usize {
        self.line
    }

    /// The row's `id` cell; `None` when the file has no `id` column or the cell is empty.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The field the column translates.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The language code of the column.
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// The translated cell (never empty).
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A CSV data file: the target model, the loaded column headers and the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvImport {
    file: Arc<str>,
    model: String,
    fields: Vec<String>,
    rows: Vec<CsvRow>,
    translations: Vec<CsvTranslation>,
    noupdate: bool,
}

impl CsvImport {
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Whether the file requested `noupdate` semantics via header pragma.
    pub fn noupdate(&self) -> bool {
        self.noupdate
    }

    /// The file name as given to the reader.
    pub fn source_file(&self) -> &str {
        &self.file
    }

    /// This file as one [`DataOperation`] (`noupdate` is the file-level default), for loaders
    /// that hand the whole matrix to `load` instead of creating one record per row.
    pub fn into_operation(self, noupdate: bool) -> crate::ast::DataOperation {
        let effective_noupdate = self.noupdate || noupdate;
        crate::ast::DataOperation::Csv(crate::ast::CsvOp {
            import: self,
            env: OpEnv::root(effective_noupdate),
        })
    }

    /// Header cells as written, translation columns (`@`) removed.
    pub fn fields(&self) -> &[String] {
        &self.fields
    }

    pub fn rows(&self) -> &[CsvRow] {
        &self.rows
    }

    /// Translations extracted from `@lang` columns (`column@lang`).
    pub fn translations(&self) -> &[CsvTranslation] {
        &self.translations
    }

    /// One [`RecordOp`] per row.
    pub fn to_record_ops(&self, noupdate: bool) -> Result<Vec<RecordOp>, ParseError> {
        let id_index = self.fields.iter().position(|f| f == "id");
        let effective_noupdate = self.noupdate || noupdate;
        let env = OpEnv::root(effective_noupdate);
        let mut ops = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            let mut id = None;
            let mut fields = Vec::with_capacity(self.fields.len());
            for (i, (name, cell)) in self.fields.iter().zip(&row.values).enumerate() {
                if Some(i) == id_index {
                    if !cell.is_empty() {
                        id = Some(XmlId::parse(cell).map_err(|e| {
                            ParseError::invalid_xml_id(&self.file, row.line, "column 'id'", e)
                        })?);
                    }
                    continue;
                }
                fields.push(FieldOp {
                    name: name.clone(),
                    value: FieldValue::CsvCell((!cell.is_empty()).then(|| cell.clone())),
                    line: row.line,
                    lang: None,
                });
            }
            ops.push(RecordOp {
                id,
                model: self.model.clone(),
                forcecreate: None,
                fields,
                translations: Vec::new(),
                env: env.clone(),
                file: Arc::clone(&self.file),
                line: row.line,
                origin: None,
            });
        }
        Ok(ops)
    }
}

/// Outcome of reading a CSV data file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CsvImportResult {
    Loaded(CsvImport),
    Skipped {
        reason: String,
    },
    /// No header.
    Empty,
}

/// The model of a CSV data file: the file name without extension, up to the first `-`
/// (`ir.access.csv` → `ir.access`, `res.partner-extra.csv` → `res.partner`).
/// `None` when the file name has no usable stem.
pub fn model_from_filename(filename: &str) -> Option<String> {
    let stem = Path::new(filename).file_stem()?.to_str()?;
    let model = stem.split_once('-').map_or(stem, |(model, _)| model);
    (!model.is_empty()).then(|| model.to_string())
}

/// Read a CSV data file.
///
/// - A UTF-8 BOM is not content; line numbers are physical lines.
/// - Columns whose header is `field@lang` (translations) are not loaded as fields; they are
///   returned as [`CsvImport::translations`]. A header with `@` and an empty field or language
///   is an error.
/// - A row with a different number of cells than the header is an error.
pub fn parse_csv(
    content: &str,
    filename: &str,
    module: &str,
    mode: ConvertMode,
) -> Result<CsvImportResult, ParseError> {
    // a leading BOM is dropped; without one the content is used as is.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = LineCounter::new(content);
    let fail = |line: usize, detail: String| {
        ParseError::csv(
            filename,
            line,
            format!(
                "Module loading {module} failed: file {filename} could not be processed:\n{detail}"
            ),
        )
    };
    let Some(model) = model_from_filename(filename) else {
        return Err(fail(
            1,
            format!("cannot derive a model name from the file name {filename:?}"),
        ));
    };
    let mut noupdate = false;
    let mut csv_start_offset = content.len();
    let mut cursor = 0;

    while cursor < content.len() {
        let remainder = &content[cursor..];
        let line_len = match remainder.find('\n') {
            Some(idx) => idx + 1,
            None => remainder.len(),
        };
        let line_str = &remainder[..line_len];
        let trimmed = line_str.trim_matches(|c| c == '\r' || c == '\n');
        let trimmed_leading = trimmed.trim_start();

        if trimmed_leading.is_empty() {
            cursor += line_len;
            continue;
        }

        if let Some(directive) = trimmed_leading.strip_prefix('#') {
            let directive_line = lines.line_at(cursor);
            let body = directive.trim();
            let (key, val) = match body.split_once([':', '=']) {
                Some((k, v)) => (k.trim(), v.trim()),
                None => {
                    return Err(fail(
                        directive_line,
                        format!(
                            "malformed header directive '#{body}'; expected 'key: value' or 'key=value'"
                        ),
                    ));
                }
            };
            if key.eq_ignore_ascii_case("noupdate") {
                match val.to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "on" => noupdate = true,
                    "0" | "false" | "no" | "off" => noupdate = false,
                    _ => {
                        return Err(fail(
                            directive_line,
                            format!(
                                "invalid boolean value '{val}' for directive 'noupdate'; expected 1/0, true/false, yes/no, on/off"
                            ),
                        ));
                    }
                }
            } else {
                return Err(fail(
                    directive_line,
                    format!("unknown header directive '{key}'"),
                ));
            }
            cursor += line_len;
        } else {
            csv_start_offset = cursor;
            break;
        }
    }

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(&content.as_bytes()[csv_start_offset..]);
    let mut records = reader.records();

    let header = match records.next() {
        None => return Ok(CsvImportResult::Empty),
        Some(Err(e)) => {
            return Err(fail(
                error_line(&mut lines, &e, csv_start_offset),
                e.to_string(),
            ));
        }
        Some(Ok(record)) => record,
    };
    let header_line = record_line(&mut lines, &header, csv_start_offset);
    let mut raw_fields: Vec<String> = Vec::with_capacity(header.len());
    let mut seen = HashSet::new();
    for cell in header.iter() {
        if cell.is_empty() {
            return Err(fail(
                header_line,
                "CSV header contains an empty column name".to_string(),
            ));
        }
        if !seen.insert(cell) {
            return Err(fail(
                header_line,
                format!("duplicate header column '{cell}' in CSV"),
            ));
        }
        raw_fields.push(cell.to_string());
    }

    if mode == ConvertMode::Update && !raw_fields.iter().any(|f| f == "id") {
        return Ok(CsvImportResult::Skipped {
            reason: format!(
                "{filename}: import specification does not contain 'id' and the module is being \
                 updated; file skipped"
            ),
        });
    }

    let id_index = raw_fields.iter().position(|f| f == "id");
    let mut trans_cols = Vec::new();
    let mut keep = Vec::with_capacity(raw_fields.len());
    for (i, cell) in raw_fields.iter().enumerate() {
        if let Some((fname, lang)) = cell.split_once('@') {
            // same rule as `<field name="f@lang">`: both halves are required
            if fname.is_empty() || lang.is_empty() {
                return Err(fail(
                    header_line,
                    format!(
                        "CSV header column '{cell}' must name both a field and a language \
                         (e.g. 'name@fr_FR')"
                    ),
                ));
            }
            trans_cols.push((i, fname.to_string(), lang.to_string()));
            keep.push(false);
        } else {
            keep.push(true);
        }
    }
    let fields: Vec<String> = raw_fields
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(f, _)| f.clone())
        .collect();
    if fields.is_empty() {
        if let Some((_, fname, lang)) = trans_cols.first() {
            return Err(fail(
                header_line,
                format!(
                    "CSV file has only translation columns (first: '{fname}@{lang}'); a file of \
                     only translation columns is not supported until a later release (translation-only CSV)"
                ),
            ));
        }
        return Ok(CsvImportResult::Empty);
    }

    let mut rows = Vec::new();
    let mut translations = Vec::new();
    for record in records {
        let record = record
            .map_err(|e| fail(error_line(&mut lines, &e, csv_start_offset), e.to_string()))?;
        let line = record_line(&mut lines, &record, csv_start_offset);
        if record.len() != raw_fields.len() {
            return Err(fail(
                line,
                format!(
                    "line {line}: row has {} columns, expected {}",
                    record.len(),
                    raw_fields.len()
                ),
            ));
        }
        let values: Vec<String> = record
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(c, _)| c.to_string())
            .collect();
        let row_id = id_index.and_then(|idx| record.get(idx)).and_then(|id| {
            let id = id.trim();
            (!id.is_empty()).then(|| id.to_string())
        });
        for &(col_idx, ref fname, ref lang) in &trans_cols {
            if let Some(cell) = record.get(col_idx)
                && !cell.is_empty()
            {
                translations.push(CsvTranslation {
                    line,
                    id: row_id.clone(),
                    field: fname.clone(),
                    lang: lang.clone(),
                    value: cell.to_string(),
                });
            }
        }
        if values.iter().all(String::is_empty) {
            continue;
        }
        rows.push(CsvRow { line, values });
    }

    Ok(CsvImportResult::Loaded(CsvImport {
        file: Arc::from(filename),
        model,
        fields,
        rows,
        translations,
        noupdate,
    }))
}

/// Physical line where a record starts. The CSV reader records the offset before the blank
/// lines it skips, so line ends are skipped first. Reader records always carry a position.
fn record_line(
    lines: &mut LineCounter<'_>,
    record: &csv::StringRecord,
    base_offset: usize,
) -> usize {
    line_of(lines, record.position(), base_offset)
}

fn error_line(lines: &mut LineCounter<'_>, err: &csv::Error, base_offset: usize) -> usize {
    line_of(lines, err.position(), base_offset)
}

fn line_of(
    lines: &mut LineCounter<'_>,
    position: Option<&csv::Position>,
    base_offset: usize,
) -> usize {
    // The csv reader sets a position on every record and on every parse error it reports; the
    // offset comes from a `&str` in memory, so it fits in `usize`.
    let offset = position.map_or(0, |p| usize::try_from(p.byte()).unwrap_or(usize::MAX));
    lines.line_of_content_at(base_offset.saturating_add(offset))
}
