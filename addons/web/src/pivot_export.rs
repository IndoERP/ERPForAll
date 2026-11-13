// SPDX-License-Identifier: MIT OR Apache-2.0
//! The spreadsheet download of the pivot view.
//!
//! The browser posts the table as it is on screen; the server formats what was posted and reads no
//! data. This module owns the five declared bounds (`web.pivot_export_*` settings), the typed
//! document parser (every refusal names the path of the offending value) and the access check; the
//! workbook itself is written by [`crate::pivot_xlsx`]. The route (`POST /erp/pivot/export`) lives
//! in `erp-server`, composed in `erp-cli` the way the attachment content route is.
//!
//! The document, with `kind` one of `integer`, `float`, `monetary`, `empty`:
//!
//! ```json
//! { "model": "library.book", "title": "Books",
//!   "headers": [[{"label": "", "rowspan": 2}, {"label": "Total", "colspan": 2, "bold": true}],
//!               [{"label": "Count"}, {"label": "Pages"}]],
//!   "rows": [{"label": "Total", "indent": 0, "bold": true,
//!             "cells": [{"kind": "integer", "value": 3, "bold": true},
//!                       {"kind": "float", "value": 1.5, "digits": 2, "bold": true}]}] }
//! ```
//!
//! Header rows are laid out like an HTML table: each cell goes to the next free column of its row
//! (a cell above with a `rowspan` has already taken its place), so nested headers of any depth are
//! posted exactly as they are drawn.

use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::Operation;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{self, Map, Value as Json};
use erp_sdk::settings::{ScopeKind, SettingDecl};
use erp_sdk::views::wire::SettingsScope;

/// One declared bound: setting key, label, default, floor and hard bound (the setting's own range).
struct Bound {
    key: &'static str,
    label: &'static str,
    default: i64,
    floor: i64,
    hard: i64,
}

/// Maximum cells of the sheet: the label column and the header rows count with the body.
pub const MAX_CELLS: &str = "web.pivot_export_max_cells";
/// Maximum number of header rows.
pub const MAX_HEADER_DEPTH: &str = "web.pivot_export_max_header_depth";
/// Maximum data columns (the label column is extra).
pub const MAX_COLUMNS: &str = "web.pivot_export_max_columns";
/// Maximum bytes of one label.
pub const MAX_LABEL_BYTES: &str = "web.pivot_export_max_label_bytes";
/// Maximum bytes of the posted document.
pub const MAX_REQUEST_BYTES: &str = "web.pivot_export_max_request_bytes";

// Hard bounds: a spreadsheet has 16,384 columns (one is the label column) and 1,048,576 rows; the
// cell and byte bounds keep one export well inside the memory of a request (the document is parsed
// whole, about ten times its size, and the workbook is built in memory).
const BOUNDS: [Bound; 5] = [
    Bound {
        key: MAX_CELLS,
        label: "Pivot download: maximum cells",
        default: 100_000,
        floor: 100,
        hard: 500_000,
    },
    Bound {
        key: MAX_HEADER_DEPTH,
        label: "Pivot download: maximum header rows",
        default: 16,
        floor: 2,
        hard: 64,
    },
    Bound {
        key: MAX_COLUMNS,
        label: "Pivot download: maximum columns",
        default: 16_383,
        floor: 2,
        hard: 16_383,
    },
    Bound {
        key: MAX_LABEL_BYTES,
        label: "Pivot download: maximum bytes of a label",
        default: 1024,
        floor: 64,
        hard: 4096,
    },
    Bound {
        key: MAX_REQUEST_BYTES,
        label: "Pivot download: maximum request bytes",
        default: 8 * 1024 * 1024,
        floor: 64 * 1024,
        hard: 32 * 1024 * 1024,
    },
];

fn settings() -> Result<Vec<SettingDecl>> {
    BOUNDS
        .iter()
        .map(|bound| {
            SettingDecl::integer(
                bound.key,
                bound.label,
                ScopeKind::Global,
                bound.default,
                (bound.floor, bound.hard),
            )?
            .block("Pivot download")?
            .access(&["base.group_user"], &["base.group_system"])
        })
        .collect()
}
erp_sdk::setting_declarations!(settings);

/// The five bounds as read from the settings (or their defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub cells: usize,
    pub header_depth: usize,
    pub columns: usize,
    pub label_bytes: usize,
    pub request_bytes: usize,
}

/// Read the bounds. The framework reads its own configuration with elevated rights: a caller who
/// may export needs no settings group, and the model access check still runs on the document.
pub async fn limits(tx: &mut Transaction) -> Result<Limits> {
    let env = tx.env().sudo(true);
    let keys = BOUNDS.iter().map(|bound| bound.key.to_owned()).collect();
    let read = read_values(&env, tx, SettingsScope::Global {}, keys).await?;
    let read = serde_json::to_value(&read)
        .map_err(|error| Error::internal(format!("pivot export settings: {error}")))?;
    let entries = read["values"]
        .as_array()
        .ok_or_else(|| Error::internal("pivot export settings: no values"))?;
    let value = |key: &str| -> Result<usize> {
        let found = entries
            .iter()
            .find(|entry| entry["key"] == key)
            .and_then(|entry| entry["value"].as_i64());
        found
            .and_then(|number| usize::try_from(number).ok())
            .ok_or_else(|| {
                Error::internal(format!(
                    "pivot export setting {key} is not a positive integer"
                ))
            })
    };
    Ok(Limits {
        cells: value(MAX_CELLS)?,
        header_depth: value(MAX_HEADER_DEPTH)?,
        columns: value(MAX_COLUMNS)?,
        label_bytes: value(MAX_LABEL_BYTES)?,
        request_bytes: value(MAX_REQUEST_BYTES)?,
    })
}

/// What the route needs before it reads the body: the largest document it may buffer.
///
/// # Errors
/// Settings read failures.
pub async fn request_limit(tx: &mut Transaction) -> Result<usize> {
    Ok(limits(tx).await?.request_bytes)
}

/// A header cell: its text and how many columns and rows it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderCell {
    pub label: String,
    /// First column of the cell once the header block is laid out (the label column is 0).
    pub col: usize,
    pub colspan: u32,
    pub rowspan: u32,
    pub bold: bool,
}

/// What a body cell holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CellValue {
    Empty,
    Integer(i64),
    /// A float or monetary value with the number of decimals shown.
    Decimal(f64, u8),
}

/// One body cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub value: CellValue,
    pub bold: bool,
}

/// One body row: its header cell and its cells.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub indent: u32,
    pub bold: bool,
    pub cells: Vec<Cell>,
}

/// The parsed document.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub model: String,
    pub title: String,
    pub headers: Vec<Vec<HeaderCell>>,
    pub rows: Vec<Row>,
}

fn refuse(path: &str, what: impl std::fmt::Display) -> Error {
    Error::validation(format!(
        "The pivot download document is invalid at {path}: {what}."
    ))
}

fn over(key: &str, found: usize, limit: usize) -> Error {
    Error::validation(format!(
        "The pivot is too large to download: {found} against the limit of {limit} set by {key}. Filter the table further or ask an administrator to raise the setting."
    ))
}

fn object<'a>(value: &'a Json, path: &str, keys: &[&str]) -> Result<&'a Map<String, Json>> {
    let map = value
        .as_object()
        .ok_or_else(|| refuse(path, "expected an object"))?;
    if let Some(unknown) = map.keys().find(|key| !keys.contains(&key.as_str())) {
        return Err(refuse(&format!("{path}.{unknown}"), "unknown key"));
    }
    Ok(map)
}

fn required<'a>(map: &'a Map<String, Json>, path: &str, key: &str) -> Result<&'a Json> {
    map.get(key)
        .ok_or_else(|| refuse(&format!("{path}.{key}"), "missing"))
}

fn text(map: &Map<String, Json>, path: &str, key: &str, limits: &Limits) -> Result<String> {
    let at = format!("{path}.{key}");
    let value = required(map, path, key)?
        .as_str()
        .ok_or_else(|| refuse(&at, "expected text"))?;
    if value.len() > limits.label_bytes {
        return Err(over(MAX_LABEL_BYTES, value.len(), limits.label_bytes));
    }
    Ok(value.to_owned())
}

/// A model name is at most 128 bytes in the registry; it is not a label and has its own cap.
fn model_name(map: &Map<String, Json>) -> Result<String> {
    let value = required(map, "$", "model")?
        .as_str()
        .ok_or_else(|| refuse("$.model", "expected text"))?;
    if value.len() > 128 {
        return Err(refuse("$.model", "longer than any model name"));
    }
    Ok(value.to_owned())
}

fn small(
    map: &Map<String, Json>,
    path: &str,
    key: &str,
    default: u32,
    min: u32,
    max: u32,
) -> Result<u32> {
    let Some(value) = map.get(key) else {
        return Ok(default);
    };
    let at = format!("{path}.{key}");
    let number = value
        .as_u64()
        .ok_or_else(|| refuse(&at, "expected a whole number"))?;
    u32::try_from(number)
        .ok()
        .filter(|number| (min..=max).contains(number))
        .ok_or_else(|| refuse(&at, format!("expected {min} to {max}")))
}

fn flag(map: &Map<String, Json>, path: &str) -> Result<bool> {
    match map.get("bold") {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| refuse(&format!("{path}.bold"), "expected true or false")),
    }
}

fn array<'a>(map: &'a Map<String, Json>, path: &str, key: &str) -> Result<&'a Vec<Json>> {
    required(map, path, key)?
        .as_array()
        .ok_or_else(|| refuse(&format!("{path}.{key}"), "expected a list"))
}

/// Largest whole number a spreadsheet cell keeps exactly.
const MAX_EXACT: i64 = 9_007_199_254_740_991;

fn cell(value: &Json, path: &str) -> Result<Cell> {
    let map = object(value, path, &["kind", "value", "digits", "bold"])?;
    let kind = required(map, path, "kind")?
        .as_str()
        .ok_or_else(|| refuse(&format!("{path}.kind"), "expected text"))?;
    let bold = flag(map, path)?;
    let present = |key: &str| map.contains_key(key);
    let number = |key: &str| -> Result<&Json> { required(map, path, key) };
    let value = match kind {
        "empty" => {
            if let Some(key) = ["value", "digits"].into_iter().find(|key| present(key)) {
                return Err(refuse(
                    &format!("{path}.{key}"),
                    "an empty cell has no value or digits",
                ));
            }
            CellValue::Empty
        }
        "integer" => {
            if present("digits") {
                return Err(refuse(
                    &format!("{path}.digits"),
                    "an integer has no digits",
                ));
            }
            let at = format!("{path}.value");
            let whole = number("value")?
                .as_i64()
                .filter(|whole| whole.abs() <= MAX_EXACT)
                .ok_or_else(|| {
                    refuse(&at, "expected a whole number a spreadsheet keeps exactly")
                })?;
            CellValue::Integer(whole)
        }
        "float" | "monetary" => {
            let at = format!("{path}.value");
            let decimal = number("value")?
                .as_f64()
                .filter(|decimal| decimal.is_finite())
                .ok_or_else(|| refuse(&at, "expected a finite number"))?;
            let at = format!("{path}.digits");
            let digits = required(map, path, "digits")?
                .as_u64()
                .and_then(|digits| u8::try_from(digits).ok())
                .filter(|digits| *digits <= 10)
                .ok_or_else(|| refuse(&at, "expected 0 to 10"))?;
            CellValue::Decimal(decimal, digits)
        }
        other => {
            return Err(refuse(
                &format!("{path}.kind"),
                format!("unknown kind \"{other}\", expected integer, float, monetary or empty"),
            ));
        }
    };
    Ok(Cell { value, bold })
}

/// Parse and bound the document. `limits` are the settings; every refusal names a path or a setting.
fn parse(value: &Json, limits: &Limits) -> Result<Document> {
    let top = object(value, "$", &["model", "title", "headers", "rows"])?;
    let model = model_name(top)?;
    let title = text(top, "$", "title", limits)?;
    let header_rows = array(top, "$", "headers")?;
    let body_rows = array(top, "$", "rows")?;
    if header_rows.is_empty() {
        return Err(refuse("$.headers", "at least one header row is needed"));
    }
    if body_rows.is_empty() {
        return Err(refuse("$.rows", "there is nothing to download: no rows"));
    }
    if header_rows.len() > limits.header_depth {
        return Err(over(
            MAX_HEADER_DEPTH,
            header_rows.len(),
            limits.header_depth,
        ));
    }
    let columns = body_rows[0]
        .get("cells")
        .and_then(Json::as_array)
        .map_or(0, Vec::len);
    if columns == 0 {
        return Err(refuse("$.rows[0].cells", "a row needs at least one cell"));
    }
    if columns > limits.columns {
        return Err(over(MAX_COLUMNS, columns, limits.columns));
    }
    // The whole sheet counts: the label column and every header row too, so a header cannot be made large
    // for free. This comes before the header grid is allocated or a merge is written.
    let cells = (columns + 1).saturating_mul(header_rows.len() + body_rows.len());
    if cells > limits.cells {
        return Err(over(MAX_CELLS, cells, limits.cells));
    }
    let mut rows = Vec::with_capacity(body_rows.len());
    for (index, row) in body_rows.iter().enumerate() {
        let path = format!("$.rows[{index}]");
        let map = object(row, &path, &["label", "indent", "bold", "cells"])?;
        let list = array(map, &path, "cells")?;
        if list.len() != columns {
            return Err(refuse(
                &format!("{path}.cells"),
                format!(
                    "expected {columns} cells like the first row, found {}",
                    list.len()
                ),
            ));
        }
        let cells = list
            .iter()
            .enumerate()
            .map(|(at, item)| cell(item, &format!("{path}.cells[{at}]")))
            .collect::<Result<Vec<_>>>()?;
        rows.push(Row {
            label: text(map, &path, "label", limits)?,
            indent: small(map, &path, "indent", 0, 0, 64)?,
            bold: flag(map, &path)?,
            cells,
        });
    }
    let width = columns + 1;
    let depth = header_rows.len();
    let mut taken = vec![vec![false; width]; depth];
    let mut headers = Vec::with_capacity(depth);
    for (r, row) in header_rows.iter().enumerate() {
        let path = format!("$.headers[{r}]");
        let list = row
            .as_array()
            .ok_or_else(|| refuse(&path, "expected a list of cells"))?;
        let mut parsed = Vec::with_capacity(list.len());
        let mut at = 0;
        for (i, item) in list.iter().enumerate() {
            let path = format!("{path}[{i}]");
            let map = object(item, &path, &["label", "colspan", "rowspan", "bold"])?;
            let mut header = HeaderCell {
                label: text(map, &path, "label", limits)?,
                col: 0,
                colspan: small(map, &path, "colspan", 1, 1, 16_384)?,
                rowspan: small(map, &path, "rowspan", 1, 1, 64)?,
                bold: flag(map, &path)?,
            };
            while at < width && taken[r][at] {
                at += 1;
            }
            header.col = at;
            let (last_col, last_row) = (at + header.colspan as usize, r + header.rowspan as usize);
            if last_col > width {
                return Err(refuse(
                    &path,
                    format!("it covers columns up to {last_col} of {width}"),
                ));
            }
            if last_row > depth {
                return Err(refuse(
                    &path,
                    format!("it covers rows up to {last_row} of {depth}"),
                ));
            }
            for line in &mut taken[r..last_row] {
                if line[at..last_col].iter().any(|used| *used) {
                    return Err(refuse(&path, "it overlaps another header cell"));
                }
                line[at..last_col].fill(true);
            }
            parsed.push(header);
            at = last_col;
        }
        headers.push(parsed);
    }
    if let Some((r, c)) = taken
        .iter()
        .enumerate()
        .find_map(|(r, line)| line.iter().position(|used| !used).map(|c| (r, c)))
    {
        return Err(refuse(
            &format!("$.headers[{r}]"),
            format!("column {c} has no header cell"),
        ));
    }
    Ok(Document {
        model,
        title,
        headers,
        rows,
    })
}

/// The finished workbook and the name to offer for it.
#[derive(Debug)]
pub struct Workbook {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

/// A posted document that is JSON of a registered model, not yet checked further. The steps of
/// [`export`] are public so a server can run the CPU-bound ones off its async executor.
#[derive(Debug)]
pub struct Posted {
    value: Json,
    model: String,
}

impl Posted {
    /// The model whose read access the caller needs.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Step 0: only an internal user (`base.group_user`) downloads a table, checked before the body is parsed.
///
/// # Errors
/// `Access` for any other caller.
pub async fn require_internal_user(tx: &mut Transaction) -> Result<()> {
    let env = tx.env();
    if tx.has_group(&env, "base.group_user").await? {
        Ok(())
    } else {
        Err(Error::access("The pivot download is for internal users."))
    }
}

/// Step 1 (CPU-bound): the request bound, the JSON text and the model name.
///
/// # Errors
/// `Validation` over the request bound or for a body that is not a JSON object with a text `model`.
pub fn read_posted(body: &[u8], limits: &Limits) -> Result<Posted> {
    if body.len() > limits.request_bytes {
        return Err(over(MAX_REQUEST_BYTES, body.len(), limits.request_bytes));
    }
    let value: Json =
        serde_json::from_slice(body).map_err(|error| refuse("$", format!("not JSON ({error})")))?;
    let top = object(&value, "$", &["model", "title", "headers", "rows"])?;
    let model = model_name(top)?;
    Ok(Posted { value, model })
}

/// Step 2: the caller must be allowed to read the document's model (the ordinary access refusal
/// otherwise): the table was built from a read the caller made, but the server does not take that on trust.
///
/// # Errors
/// `Validation` for an unknown model, `Access` when the caller cannot read it.
pub async fn authorize(tx: &mut Transaction, posted: &Posted) -> Result<()> {
    let env = tx.env();
    if env.registry().model(&posted.model).is_none() {
        return Err(refuse(
            "$.model",
            format!(
                "unknown model \"{}\"",
                posted.model.chars().take(128).collect::<String>()
            ),
        ));
    }
    env.records(&posted.model)?
        .check_access(tx, Operation::Read)
        .await
}

/// Step 3 (CPU-bound): bound and parse the document, write the workbook. Every bound is checked before
/// the header grid is allocated or the writer is called.
///
/// # Errors
/// `Validation`: a malformed document or one over a bound, naming the path or the setting.
pub fn build(posted: &Posted, limits: &Limits) -> Result<Workbook> {
    let document = parse(&posted.value, limits)?;
    crate::pivot_xlsx::write(&document)
}

#[cfg(test)]
#[path = "tests/pivot_export.rs"]
mod tests;
