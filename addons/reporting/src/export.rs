// SPDX-License-Identifier: MIT OR Apache-2.0
//! PDF and XLSX export of a dynamic report (design, §9, §10;
//! ).
//!
//! The browser posts `{"report": id, "options": {...}, "format": "pdf" | "xlsx"}` and never rows.
//! [`prepare`] recomputes the lines as the caller (every line the options unfold, within the
//! report's page bound), applies hide zero and the search text as the screen does, refuses an
//! answer over `reporting.export_max_rows`, and reads the company's letterhead. [`Prepared`] then
//! builds the file: [`Prepared::xlsx`] is CPU work, [`Prepared::document`] is the PDF's document
//! tree, which the node lays out (`report_job::layout`). The route is `POST /erp/reporting/export`
//! in `erp-server`, composed in `erp-cli`.
use erp_addon_web::table_xlsx::{
    self, Cell as XCell, CellValue, HeaderCell, Row as XRow, TableDocument, Workbook,
};
use erp_sdk::prelude::*;
use erp_sdk::report::{
    Alignment, DocTree, DocumentBuilder, DocumentOptions, ImageFit, Limits, Node, NodeId,
    PageSetup, Style,
};
use erp_sdk::serde_json::Value as Json;
use erp_sdk::{Load, Operation};

use crate::line::{Cell, CellKind, Line, LineKind};
use crate::options;
use crate::runtime;
use crate::settings::{self, Bounds};

/// More data columns than this print landscape.
const PORTRAIT_COLUMNS: usize = 5;
const SHEET_MODEL: &str = "reporting.report";
/// A4 in points.
const A4: (f64, f64) = (595.0, 842.0);
const MARGIN: f64 = 36.0;
const LOGO_HEIGHT: f64 = 40.0;

/// The file format asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// A PDF document.
    Pdf,
    /// A one-sheet spreadsheet plus a sheet of the options used.
    Xlsx,
}

impl Format {
    /// The media type of the answer.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        }
    }
    fn extension(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Xlsx => "xlsx",
        }
    }
}

/// A parsed export request.
#[derive(Debug)]
#[non_exhaustive]
pub struct Request {
    /// The `reporting.report` record.
    pub report: i64,
    /// The options as the screen has them (checked strictly like `erp_report_lines`).
    pub options: Json,
    /// The file format.
    pub format: Format,
}

fn bad(message: impl Into<String>) -> Error {
    Error::validation(message.into())
}

/// Parse the posted body.
///
/// # Errors
/// `Validation` for anything but an object with exactly `report`, `options` and `format`: a key
/// of rows or lines is refused as unknown, the server computes them.
pub fn parse(body: &[u8]) -> Result<Request> {
    let value: Json = erp_sdk::serde_json::from_slice(body)
        .map_err(|error| bad(format!("The export request is not JSON: {error}.")))?;
    let object = value
        .as_object()
        .ok_or_else(|| bad("The export request must be an object."))?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !["report", "options", "format"].contains(&key.as_str()))
    {
        return Err(bad(format!(
            "'{unknown}' is not part of an export request: send report, options and format; the server computes the rows."
        )));
    }
    let report = object
        .get("report")
        .and_then(Json::as_i64)
        .ok_or_else(|| bad("report must be the id of a dynamic report."))?;
    let mut options = match object.get("options") {
        Some(Json::Object(map)) => map.clone(),
        _ => return Err(bad("options must be an object.")),
    };
    match options.get("report") {
        None => {
            options.insert("report".into(), Json::from(report));
        }
        Some(found) if found.as_i64() == Some(report) => {}
        Some(_) => return Err(bad("options.report is not the report to export.")),
    }
    let format = match object.get("format").and_then(Json::as_str) {
        Some("pdf") => Format::Pdf,
        Some("xlsx") => Format::Xlsx,
        _ => return Err(bad("format must be \"pdf\" or \"xlsx\".")),
    };
    Ok(Request {
        report,
        options: Json::Object(options),
        format,
    })
}

/// The largest request the route may buffer: the declared setting.
///
/// # Errors
/// As [`settings::read`].
pub async fn request_limit(tx: &mut Transaction) -> Result<usize> {
    let env = tx.env();
    Ok(settings::read(&env, tx).await?.export_max_request_bytes)
}

/// Whether line `i` of `lines` is shown: hide zero keeps a line with a non-zero value in itself or
/// below it (text counts as zero); the search keeps matches, their ancestors and their
/// descendants; a load-more line follows its parent.
fn visible(lines: &[Line], hide_zero: bool, search: &str) -> Vec<bool> {
    let needle = search.to_lowercase();
    let n = lines.len();
    let mut shown = vec![true; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut nonzero = vec![false; n];
    let mut found = vec![false; n];
    let mut inherited: Vec<bool> = Vec::new(); // per stack entry
    for (i, line) in lines.iter().enumerate() {
        let level = line.id.level();
        while stack
            .last()
            .is_some_and(|top| lines[*top].id.level() >= level)
        {
            stack.pop();
            inherited.pop();
        }
        let more = line.kind == LineKind::More;
        let own = !more && !needle.is_empty() && line.name.to_lowercase().contains(&needle);
        let from_above = own || inherited.last().copied().unwrap_or(false);
        stack.push(i);
        inherited.push(from_above);
        if from_above {
            found[i] = true;
        }
        if own {
            stack.iter().for_each(|s| found[*s] = true);
        }
        // A folded group with blank/text-only header cells or another page may contain a nonzero amount.
        let blank_cells = line.cells.iter().all(|cell| {
            matches!(
                cell.kind(),
                CellKind::Empty | CellKind::Text | CellKind::Date
            ) || cell.value().is_none()
        });
        if more || (line.unfoldable && !line.unfolded && blank_cells) || !line.is_zero() {
            stack.iter().for_each(|s| nonzero[*s] = true);
        }
    }
    stack.clear();
    for (i, line) in lines.iter().enumerate() {
        let level = line.id.level();
        while stack
            .last()
            .is_some_and(|top| lines[*top].id.level() >= level)
        {
            stack.pop();
        }
        shown[i] = if line.kind == LineKind::More {
            // a load-more line is always below the line it pages
            stack.last().is_none_or(|parent| shown[*parent])
        } else {
            (!hide_zero || nonzero[i]) && (needle.is_empty() || found[i])
        };
        stack.push(i);
    }
    shown
}

/// A company's letterhead (the fields `web.external_layout` prints, design §17 decision 6).
#[derive(Debug, Default)]
struct Letterhead {
    name: String,
    address: Vec<String>,
    logo: Option<Vec<u8>>,
}

async fn letterhead(env: &Env, tx: &mut Transaction, company: Option<i64>) -> Result<Letterhead> {
    let Some(company) = company else {
        return Ok(Letterhead::default());
    };
    let rows = env
        .records("res.company")?
        .browse([company])?
        .read_with(
            tx,
            &["name", "street", "street2", "zip", "city", "logo_web"],
            Load::Raw,
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| bad(format!("Company {company} does not exist.")))?;
    let text = |key: &str| match row.get(key) {
        Some(Value::Str(s)) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };
    let place: Vec<String> = [text("zip"), text("city")].into_iter().flatten().collect();
    let address = [text("street"), text("street2")]
        .into_iter()
        .flatten()
        .chain((!place.is_empty()).then(|| place.join(" ")))
        .collect();
    let logo = match row.get("logo_web") {
        Some(Value::Bytes(bytes)) => Some(bytes.to_vec()),
        None | Some(Value::False) => None,
        Some(other) => {
            return Err(Error::internal(format!(
                "reporting export: the company logo reads as {other:?}"
            )));
        }
    };
    Ok(Letterhead {
        name: text("name").unwrap_or_default(),
        address,
        logo,
    })
}

/// Everything a file needs: computed lines, headers and the letterhead. Built by [`prepare`].
#[derive(Debug)]
pub struct Prepared {
    /// What was asked for.
    pub format: Format,
    title: String,
    /// One period text per column group.
    groups: Vec<String>,
    /// Header text per data column of one group.
    columns: Vec<String>,
    lines: Vec<Line>,
    notes: Vec<(String, String)>,
    letterhead: Letterhead,
    printed: String,
}

/// `from to to`, or the single date.
fn period_text(period: &options::Period) -> String {
    match period.from {
        Some(from) => format!("{} - {}", options::text(from), options::text(period.to)),
        None => options::text(period.to),
    }
}

/// Name and value pairs of the options sheet: nested keys joined with a dot.
fn flatten(prefix: &str, value: &Json, out: &mut Vec<(String, String)>) {
    match value {
        Json::Object(map) => map
            .iter()
            .filter(|(key, _)| {
                !(prefix.is_empty() && matches!(key.as_str(), "columns" | "column_groups"))
            })
            .for_each(|(key, inner)| {
                let name = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&name, inner, out);
            }),
        Json::String(text) => out.push((prefix.to_owned(), text.clone())),
        Json::Null => out.push((prefix.to_owned(), String::new())),
        other => out.push((prefix.to_owned(), other.to_string())),
    }
}

/// Recompute the report as the caller and gather what the file holds.
///
/// # Errors
/// `Access` when the caller may not read the report; `Validation` for a bad request or options,
/// or naming `reporting.export_max_rows` (or the page bound) when the file would hold more rows;
/// the provider's errors. Nothing is truncated.
pub async fn prepare(tx: &mut Transaction, body: &[u8]) -> Result<Prepared> {
    let request = parse(body)?;
    let env = tx.env();
    env.records(SHEET_MODEL)?
        .browse([request.report])?
        .check_access(tx, Operation::Read)
        .await?;
    let (p, lines, hidden) = runtime::export_lines(&env, tx, &request.options).await?;
    let search = if p.searches_on_server() {
        ""
    } else {
        &p.options.search
    };
    let shown = visible(&lines, p.options.hide_zero, search);
    let lines: Vec<Line> = lines
        .into_iter()
        .zip(shown)
        .filter_map(|(line, keep)| keep.then_some(line))
        .map(|mut line| {
            // the screen words the load-more line itself; a file says how many are not in it
            if let (LineKind::More, Some(left)) = (line.kind, line.remaining) {
                line.name = format!("{left} more");
            }
            line
        })
        .collect();
    check_rows(lines.len(), &p.bounds)?;
    let letterhead = letterhead(&env, tx, p.options.companies.first().copied()).await?;
    let mut notes = Vec::new();
    flatten("", &p.options.to_json(&p.def), &mut notes);
    if !hidden.is_empty() {
        notes.push(("hidden_lines".into(), hidden.join(", ")));
    }
    Ok(Prepared {
        format: request.format,
        title: p.def.name.clone(),
        groups: p.options.column_groups.iter().map(period_text).collect(),
        columns: p.def.columns.iter().map(|c| c.name.clone()).collect(),
        lines,
        notes,
        letterhead,
        printed: DateTime::now().to_string(),
    })
}

fn check_rows(rows: usize, bounds: &Bounds) -> Result<()> {
    if rows > bounds.export_max_rows {
        return Err(bad(format!(
            "The export has {rows} rows, more than the {} one export takes (setting {}). Narrow the options or fold lines.",
            bounds.export_max_rows,
            settings::EXPORT_MAX_ROWS
        )));
    }
    Ok(())
}

impl Prepared {
    /// The name offered for the download.
    pub fn file_name(&self) -> String {
        table_xlsx::download_name(&self.title, self.format.extension(), "Report")
    }

    /// Whether the data columns of all column groups are too many for a portrait page.
    pub fn landscape(&self) -> bool {
        self.groups.len() * self.columns.len() > PORTRAIT_COLUMNS
    }

    fn data_columns(&self) -> usize {
        self.groups.len() * self.columns.len()
    }

    /// The spreadsheet: period row, column row, one row per line (numbers as numbers, the name
    /// indented by level), and a second sheet of the options.
    ///
    /// # Errors
    /// `Internal` when the workbook writer refuses a value.
    pub fn xlsx(&self) -> Result<Workbook> {
        let per_group = self.columns.len();
        let mut periods = vec![header("", 0, 1, 2)];
        let mut labels = Vec::new();
        for (g, period) in self.groups.iter().enumerate() {
            let first = 1 + g * per_group;
            periods.push(header(period, first, per_group.max(1), 1));
            for (c, name) in self.columns.iter().enumerate() {
                labels.push(header(name, first + c, 1, 1));
            }
        }
        let rows = self
            .lines
            .iter()
            .map(|line| {
                let bold = matches!(line.kind, LineKind::Section | LineKind::Total);
                Ok(XRow {
                    label: line.name.clone(),
                    indent: line.id.level(),
                    bold,
                    cells: line
                        .cells
                        .iter()
                        .map(|cell| sheet_cell(cell, bold))
                        .collect::<Result<_>>()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        table_xlsx::write(&TableDocument {
            model: SHEET_MODEL.to_owned(),
            title: self.title.clone(),
            headers: vec![periods, labels],
            rows,
            notes: self.notes.clone(),
        })
    }

    /// The PDF's document: letterhead, title, periods and print time above one table that
    /// repeats its header on every page; landscape from six data columns.
    ///
    /// # Errors
    /// `Validation` when the document breaks a bound of the document model (nodes, pages, picture).
    pub fn document(&self) -> Result<DocTree> {
        let invalid =
            |e: erp_sdk::report::ReportError| bad(format!("The report could not be laid out: {e}"));
        let mut b = DocumentBuilder::new(Limits::default());
        let mut flow: Vec<NodeId> = Vec::new();
        let text = |b: &mut DocumentBuilder, text: &str, style: Style| -> Result<NodeId> {
            let inner = b.add(Node::Text(text.to_owned())).map_err(invalid)?;
            b.add(Node::Block {
                children: vec![inner],
                style,
            })
            .map_err(invalid)
        };
        let style = |size: f64, bold: bool| -> Result<Style> {
            Ok(Style::default()
                .with_size(size)
                .map_err(invalid)?
                .with_bold(bold))
        };
        if let Some(logo) = &self.letterhead.logo {
            flow.push(
                b.add(Node::Image {
                    data: logo.as_slice().into(),
                    width: None,
                    height: Some(LOGO_HEIGHT),
                    fit: ImageFit::Contain,
                    alt: self.letterhead.name.clone(),
                })
                .map_err(invalid)?,
            );
        }
        if !self.letterhead.name.is_empty() {
            flow.push(text(&mut b, &self.letterhead.name, style(12.0, true)?)?);
        }
        for line in &self.letterhead.address {
            flow.push(text(&mut b, line, style(9.0, false)?)?);
        }
        flow.push(b.add(Node::Rule).map_err(invalid)?);
        flow.push(text(&mut b, &self.title, style(16.0, true)?)?);
        flow.push(text(
            &mut b,
            &self.groups.join("   |   "),
            style(9.0, false)?,
        )?);
        flow.push(text(
            &mut b,
            &format!("Printed {} UTC", self.printed),
            style(8.0, false)?,
        )?);

        let width = self.data_columns() + 1;
        let mut header_cells = vec![text(&mut b, "", style(9.0, true)?)?];
        for g in 0..self.groups.len() {
            for name in &self.columns {
                let label = if self.groups.len() > 1 {
                    format!("{name} ({})", self.groups[g])
                } else {
                    name.clone()
                };
                header_cells.push(text(
                    &mut b,
                    &label,
                    style(9.0, true)?.with_alignment(Alignment::End),
                )?);
            }
        }
        let mut cells = Vec::with_capacity(self.lines.len() * width);
        for line in &self.lines {
            let bold = matches!(line.kind, LineKind::Section | LineKind::Total);
            let indent = "\u{a0}".repeat(4 * line.id.level() as usize);
            cells.push(text(
                &mut b,
                &format!("{indent}{}", line.name),
                style(9.0, bold)?,
            )?);
            for cell in &line.cells {
                cells.push(text(
                    &mut b,
                    &pdf_text(cell),
                    style(9.0, bold)?.with_alignment(Alignment::End),
                )?);
            }
        }
        flow.push(
            b.add(Node::Table {
                columns: width,
                header: header_cells,
                cells,
            })
            .map_err(invalid)?,
        );
        let body = b.add(Node::Flow { children: flow }).map_err(invalid)?;
        let (w, h) = if self.landscape() { (A4.1, A4.0) } else { A4 };
        let page = PageSetup::new(w, h, MARGIN).map_err(invalid)?;
        b.finish(
            body,
            DocumentOptions::new("dynamic-report", self.title.clone(), page),
        )
        .map_err(invalid)
    }
}

fn header(label: &str, col: usize, colspan: usize, rowspan: u32) -> HeaderCell {
    HeaderCell {
        label: label.to_owned(),
        col,
        colspan: u32::try_from(colspan).unwrap_or(u32::MAX),
        rowspan,
        bold: true,
    }
}

fn sheet_cell(cell: &Cell, bold: bool) -> Result<XCell> {
    let shown = cell.display();
    let broken =
        |what: &str| Error::internal(format!("reporting export: cell '{shown}' is not {what}"));
    let value = if shown.is_empty() {
        CellValue::Empty
    } else if cell.value().is_none() {
        CellValue::Text(shown.clone())
    } else if cell.kind() == CellKind::Integer {
        CellValue::Integer(shown.parse().map_err(|_| broken("a whole number"))?)
    } else {
        let digits = u8::try_from(cell.scale()).map_err(|_| broken("a number of few decimals"))?;
        CellValue::Decimal(shown.parse().map_err(|_| broken("a number"))?, digits)
    };
    Ok(XCell { value, bold })
}

fn pdf_text(cell: &Cell) -> String {
    match cell.kind() {
        CellKind::Percent if cell.value().is_some() => format!("{} %", cell.display()),
        _ => cell.display(),
    }
}

#[cfg(test)]
#[path = "tests/export.rs"]
mod tests;
