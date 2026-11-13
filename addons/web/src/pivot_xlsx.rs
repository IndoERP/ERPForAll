// SPDX-License-Identifier: MIT OR Apache-2.0
//! The workbook of the pivot download: one sheet, header cells merged as drawn,
//! totals bold, numbers written as numbers with a format, every text cell guarded against formula
//! injection, column widths set from the content within a bound.
//!
//! Formula-injection rule (OWASP "CSV Injection": a cell whose text starts with `=`, `+`, `-`, `@`,
//! a tab or a carriage return can be taken for a formula when it is edited or exported again): such
//! a text is written as an explicit string cell, never through a formula call, with the spreadsheet's
//! own quote-prefix flag (the stored apostrophe: the cell is text, the apostrophe is not shown), and
//! its characters stay exactly as posted. Every other text cell is a plain string cell too, so no
//! cell of the file is a formula.

use crate::pivot_export::{CellValue, Document, Workbook};
use erp_sdk::prelude::*;
use rust_xlsxwriter::{Color, Format, FormatAlign, FormatBorder, XlsxError};
use std::collections::HashMap;

/// Column width bounds in characters: narrow enough to read a table, wide enough for a label.
const MIN_WIDTH: f64 = 10.0;
const MAX_WIDTH: f64 = 60.0;
/// A spreadsheet sheet name: at most 31 UTF-16 units, none of `[ ] : * ? / \`.
const SHEET_NAME_CHARS: usize = 31;
const FILE_NAME_CHARS: usize = 100;

fn guarded(text: &str) -> bool {
    matches!(
        text.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r')
    )
}

/// A sheet name the spreadsheet accepts, from the action title.
fn sheet_name(title: &str) -> String {
    let kept: String = title
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') && !c.is_control())
        .collect();
    // the limit counts UTF-16 units; the cut comes first so the trims see the final ends
    let mut units = 0;
    let kept: String = kept
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= SHEET_NAME_CHARS
        })
        .collect();
    let kept = kept.trim_matches(|c: char| c == '\'' || c.is_whitespace());
    // the format reserves "History" in English spreadsheets (the library lets it through)
    if kept.is_empty() || kept.eq_ignore_ascii_case("history") {
        "Pivot".to_owned()
    } else {
        kept.to_owned()
    }
}

/// The file name offered for the download: the action title, without path or control characters.
fn file_name(title: &str) -> String {
    let kept: String = title
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let kept: String = kept
        .trim()
        .trim_matches('.')
        .chars()
        .take(FILE_NAME_CHARS)
        .collect();
    format!(
        "{}.xlsx",
        if kept.trim().is_empty() {
            "Pivot"
        } else {
            kept.trim()
        }
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Role {
    Header,
    Label,
    Integer,
    Decimal,
}

fn fail(error: &XlsxError) -> Error {
    Error::internal(format!("the pivot workbook could not be written: {error}"))
}

/// Formats are built once per distinct style: `(role, bold, indent or digits, guarded text)`.
#[derive(Default)]
struct Styles(HashMap<(Role, bool, u8, bool), Format>);

impl Styles {
    fn get(&mut self, role: Role, bold: bool, extra: u8, quote: bool) -> &Format {
        self.0.entry((role, bold, extra, quote)).or_insert_with(|| {
            let mut format = Format::new();
            if bold {
                format = format.set_bold();
            }
            match role {
                Role::Header => {
                    format = format
                        .set_background_color(Color::RGB(0x00E7_E6E6))
                        .set_border(FormatBorder::Thin)
                        .set_align(FormatAlign::Center);
                }
                Role::Label => {
                    if extra > 0 {
                        format = format.set_indent(extra.min(15));
                    }
                }
                Role::Integer => format = format.set_num_format("#,##0"),
                Role::Decimal => {
                    let decimals = if extra == 0 {
                        String::new()
                    } else {
                        format!(".{}", "0".repeat(usize::from(extra)))
                    };
                    format = format.set_num_format(format!("#,##0{decimals}"));
                }
            }
            if quote {
                format = format.set_quote_prefix();
            }
            format
        })
    }
}

fn width_of(text: &str, indent: usize, bold: bool) -> f64 {
    f64::from(
        u32::try_from(text.chars().count() + 2 * indent + 2 + usize::from(bold))
            .unwrap_or(u32::MAX),
    )
}

/// Write the validated document as a workbook.
///
/// # Errors
/// `Internal`: the library refused a value the parser had admitted (a bug, never the caller's).
pub fn write(document: &Document) -> Result<Workbook> {
    write_inner(document).map_err(|error| fail(&error))
}

fn write_inner(document: &Document) -> std::result::Result<Workbook, XlsxError> {
    let mut book = rust_xlsxwriter::Workbook::new();
    let sheet = book.add_worksheet();
    sheet.set_name(sheet_name(&document.title))?;
    let mut styles = Styles::default();
    let depth = document.headers.len();
    let columns = document.rows[0].cells.len();
    let mut widths = vec![MIN_WIDTH; columns + 1];
    let row_of = |r: usize| u32::try_from(r).unwrap_or(u32::MAX);
    let col_of = |c: usize| u16::try_from(c).unwrap_or(u16::MAX);

    for (r, row) in document.headers.iter().enumerate() {
        for header in row {
            let (last_col, last_row) = (
                header.col + header.colspan as usize - 1,
                r + header.rowspan as usize - 1,
            );
            let format = styles.get(Role::Header, header.bold, 0, guarded(&header.label));
            if (last_col, last_row) == (header.col, r) {
                sheet.write_string_with_format(
                    row_of(r),
                    col_of(header.col),
                    header.label.as_str(),
                    format,
                )?;
                widths[header.col] =
                    widths[header.col].max(width_of(&header.label, 0, header.bold));
            } else {
                sheet.merge_range(
                    row_of(r),
                    col_of(header.col),
                    row_of(last_row),
                    col_of(last_col),
                    header.label.as_str(),
                    format,
                )?;
            }
        }
    }
    for (i, row) in document.rows.iter().enumerate() {
        let at = row_of(depth + i);
        let indent = u8::try_from(row.indent).unwrap_or(u8::MAX);
        let format = styles.get(Role::Label, row.bold, indent, guarded(&row.label));
        sheet.write_string_with_format(at, 0, &row.label, format)?;
        widths[0] = widths[0].max(width_of(&row.label, row.indent as usize, row.bold));
        for (j, cell) in row.cells.iter().enumerate() {
            let col = col_of(j + 1);
            match cell.value {
                CellValue::Empty => {}
                CellValue::Integer(whole) => {
                    let format = styles.get(Role::Integer, cell.bold, 0, false);
                    #[allow(
                        clippy::cast_precision_loss,
                        reason = "the parser admits only whole numbers below 2^53"
                    )]
                    sheet.write_number_with_format(at, col, whole as f64, format)?;
                    widths[j + 1] = widths[j + 1].max(width_of(&whole.to_string(), 0, cell.bold));
                }
                CellValue::Decimal(decimal, digits) => {
                    let format = styles.get(Role::Decimal, cell.bold, digits, false);
                    sheet.write_number_with_format(at, col, decimal, format)?;
                    widths[j + 1] = widths[j + 1].max(width_of(
                        &format!("{decimal:.prec$}", prec = usize::from(digits)),
                        0,
                        cell.bold,
                    ));
                }
            }
        }
    }
    for (c, width) in widths.iter().enumerate() {
        sheet.set_column_width(col_of(c), width.clamp(MIN_WIDTH, MAX_WIDTH))?;
    }
    Ok(Workbook {
        file_name: file_name(&document.title),
        bytes: book.save_to_buffer()?,
    })
}

#[cfg(test)]
#[path = "tests/pivot_xlsx.rs"]
mod tests;
