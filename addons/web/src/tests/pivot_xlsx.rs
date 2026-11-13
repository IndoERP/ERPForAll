// SPDX-License-Identifier: MIT OR Apache-2.0
//! Unit tests of `pivot_xlsx.rs` (kept in a test path: the staging guard counts inline test modules as production).
use super::*;
use crate::pivot_export::{Cell, HeaderCell, Row};
use std::collections::BTreeMap;
use std::io::Read;

fn header(label: &str, col: usize, colspan: u32, rowspan: u32, bold: bool) -> HeaderCell {
    HeaderCell {
        label: label.into(),
        col,
        colspan,
        rowspan,
        bold,
    }
}
fn int(value: i64, bold: bool) -> Cell {
    Cell {
        value: CellValue::Integer(value),
        bold,
    }
}
fn dec(value: f64, digits: u8, bold: bool) -> Cell {
    Cell {
        value: CellValue::Decimal(value, digits),
        bold,
    }
}
const EMPTY: Cell = Cell {
    value: CellValue::Empty,
    bold: false,
};

/// Two header levels and two measures: a Country row axis, Company and Person column groups, and a Total column.
pub(crate) fn nested() -> Document {
    Document {
        model: "res.partner".into(),
        title: "Partners".into(),
        headers: vec![
            vec![
                header("Country", 0, 1, 3, false),
                header("Total", 1, 4, 1, true),
                header("Total", 5, 2, 2, true),
            ],
            vec![
                header("Company", 1, 2, 1, false),
                header("Person", 3, 2, 1, false),
            ],
            vec![
                header("Count", 1, 1, 1, false),
                header("Score", 2, 1, 1, false),
                header("Count", 3, 1, 1, false),
                header("Score", 4, 1, 1, false),
                header("Count", 5, 1, 1, true),
                header("Score", 6, 1, 1, true),
            ],
        ],
        rows: vec![
            Row {
                label: "Total".into(),
                indent: 0,
                bold: true,
                cells: vec![
                    int(1234, true),
                    dec(2.5, 2, true),
                    int(1, true),
                    EMPTY,
                    int(1235, true),
                    dec(2.5, 2, true),
                ],
            },
            Row {
                label: "Indonesia".into(),
                indent: 1,
                bold: false,
                cells: vec![
                    int(1234, false),
                    dec(2.5, 2, false),
                    int(1, false),
                    EMPTY,
                    int(1235, true),
                    dec(2.5, 2, true),
                ],
            },
        ],
    }
}

/// Slice 1's flat table: one row axis, no column groups, one measure.
fn flat() -> Document {
    Document {
        model: "library.book".into(),
        title: "Books".into(),
        headers: vec![
            vec![
                header("Genre", 0, 1, 2, false),
                header("Total", 1, 1, 1, true),
            ],
            vec![header("Count", 1, 1, 1, false)],
        ],
        rows: vec![
            Row {
                label: "Total".into(),
                indent: 0,
                bold: true,
                cells: vec![int(3, true)],
            },
            Row {
                label: "Fiction".into(),
                indent: 1,
                bold: false,
                cells: vec![int(2, false)],
            },
        ],
    }
}

/// One file of the xlsx archive, read the way a spreadsheet program would: unzip, then XML.
fn part(bytes: &[u8], name: &str) -> String {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("an xlsx file is a zip archive");
    let mut text = String::new();
    archive
        .by_name(name)
        .unwrap_or_else(|_| panic!("the archive has {name}"))
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// Every `<tag ...>` start of `xml` with that name, as its attributes.
fn tags(xml: &str, name: &str) -> Vec<BTreeMap<String, String>> {
    let open = format!("<{name}");
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find(&open) {
        rest = &rest[at + open.len()..];
        if !rest.starts_with([' ', '/', '>']) {
            continue;
        }
        let end = rest.find('>').unwrap();
        let mut attrs = BTreeMap::new();
        let mut body = &rest[..end];
        while let Some(eq) = body.find("=\"") {
            let key = body[..eq].trim().trim_start_matches('/').to_owned();
            let after = &body[eq + 2..];
            let close = after.find('"').unwrap();
            attrs.insert(key, after[..close].to_owned());
            body = &after[close + 1..];
        }
        found.push(attrs);
    }
    found
}

/// What a reader sees at each cell: (style index, type, raw value or shared string text).
struct Sheet {
    cells: BTreeMap<String, (usize, String, String)>,
    merges: Vec<String>,
    xml: String,
    styles: String,
    shared: Vec<String>,
    columns: Vec<(f64, f64)>,
}

fn open(bytes: &[u8]) -> Sheet {
    let xml = part(bytes, "xl/worksheets/sheet1.xml");
    let strings = part(bytes, "xl/sharedStrings.xml");
    let shared = strings
        .split("<si>")
        .skip(1)
        .map(|item| {
            let start = item.find("<t").unwrap();
            let from = item[start..].find('>').unwrap() + start + 1;
            item[from..item.find("</t>").unwrap()]
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("_x000D_", "\r")
        })
        .collect();
    let mut cells = BTreeMap::new();
    for chunk in xml.split("<c ").skip(1) {
        let head = &chunk[..chunk.find('>').unwrap()];
        let attrs = tags(&format!("<c {head}>"), "c").remove(0);
        let value = chunk.find("<v>").map_or(String::new(), |from| {
            chunk[from + 3..chunk.find("</v>").unwrap()].to_owned()
        });
        cells.insert(
            attrs["r"].clone(),
            (
                attrs.get("s").map_or(0, |s| s.parse().unwrap()),
                attrs.get("t").cloned().unwrap_or_default(),
                value,
            ),
        );
    }
    let merges = tags(&xml, "mergeCell")
        .into_iter()
        .map(|a| a["ref"].clone())
        .collect();
    let columns = tags(&xml, "col")
        .into_iter()
        .map(|a| (a["min"].parse().unwrap(), a["width"].parse().unwrap()))
        .collect();
    Sheet {
        cells,
        merges,
        xml,
        styles: part(bytes, "xl/styles.xml"),
        shared,
        columns,
    }
}

impl Sheet {
    /// The text of a string cell, or the number of a numeric one.
    fn text(&self, at: &str) -> &str {
        let (_, kind, value) = &self.cells[at];
        if kind == "s" {
            &self.shared[value.parse::<usize>().unwrap()]
        } else {
            value
        }
    }
    fn xf(&self, at: &str) -> BTreeMap<String, String> {
        let block = &self.styles
            [self.styles.find("<cellXfs").unwrap()..self.styles.find("</cellXfs>").unwrap()];
        tags(block, "xf").remove(self.cells[at].0)
    }
    fn bold(&self, at: &str) -> bool {
        let font: usize = self.xf(at)["fontId"].parse().unwrap();
        let fonts = &self.styles
            [self.styles.find("<fonts").unwrap()..self.styles.find("</fonts>").unwrap()];
        fonts
            .split("<font>")
            .skip(1)
            .nth(font)
            .unwrap()
            .contains("<b/>")
    }
    fn number_format(&self, at: &str) -> String {
        let id = &self.xf(at)["numFmtId"];
        match id.as_str() {
            "0" => "General".into(),
            "3" => "#,##0".into(),
            "4" => "#,##0.00".into(),
            _ => tags(&self.styles, "numFmt")
                .into_iter()
                .find(|f| &f["numFmtId"] == id)
                .map(|f| f["formatCode"].clone())
                .unwrap_or_else(|| format!("builtin {id}")),
        }
    }
}

fn nested_sheet() -> Sheet {
    open(&write(&nested()).unwrap().bytes)
}

#[test]
fn a_nested_table_is_written_with_its_headers_merged_as_drawn() {
    let sheet = nested_sheet();
    let mut merges = sheet.merges.clone();
    merges.sort();
    let mut expected = vec!["A1:A3", "B1:E1", "F1:G2", "B2:C2", "D2:E2"];
    expected.sort();
    assert_eq!(merges, expected);
    for (at, label) in [
        ("A1", "Country"),
        ("B1", "Total"),
        ("F1", "Total"),
        ("B2", "Company"),
        ("D2", "Person"),
        ("B3", "Count"),
        ("C3", "Score"),
        ("G3", "Score"),
        ("A4", "Total"),
        ("A5", "Indonesia"),
    ] {
        assert_eq!(sheet.text(at), label, "{at}");
    }
    assert!(
        sheet.xml.contains("<sheetData>") && !sheet.xml.contains("<f>"),
        "no cell of the sheet is a formula"
    );
}

#[test]
fn numbers_are_written_as_numbers_with_their_format_and_totals_are_bold() {
    let sheet = nested_sheet();
    assert_eq!(
        sheet.cells["B4"].1, "",
        "an integer is a numeric cell, not text"
    );
    assert_eq!(
        (sheet.text("B4"), sheet.number_format("B4")),
        ("1234", "#,##0".to_owned())
    );
    assert_eq!(
        (sheet.text("C4"), sheet.number_format("C4")),
        ("2.5", "#,##0.00".to_owned())
    );
    assert!(
        !sheet.cells.contains_key("E4"),
        "an empty cell is not written at all"
    );
    assert!(
        sheet.bold("B4") && sheet.bold("A4") && sheet.bold("F5"),
        "the total row and the total column are bold"
    );
    assert!(
        !sheet.bold("B5") && !sheet.bold("A5"),
        "an ordinary row is not"
    );
    assert!(
        sheet.bold("B1") && !sheet.bold("B2"),
        "the Total header is bold, a group header is not"
    );
    let indent = |at: &str| sheet.xf(at).get("applyAlignment").cloned();
    assert!(
        indent("A5").is_some() && indent("A4").is_none(),
        "a child row is indented, the total row is not"
    );
}

#[test]
fn a_float_keeps_the_digits_of_its_measure() {
    let mut document = flat();
    document.rows[1].cells = vec![dec(1.0 / 3.0, 3, false)];
    let sheet = open(&write(&document).unwrap().bytes);
    assert_eq!(sheet.number_format("B4"), "#,##0.000");
    assert!(
        sheet.text("B4").starts_with("0.3333"),
        "the cell keeps the full value, only the display is rounded"
    );
}

#[test]
fn the_flat_table_of_slice_one_is_written() {
    let book = write(&flat()).unwrap();
    let sheet = open(&book.bytes);
    let mut merges = sheet.merges.clone();
    merges.sort();
    assert_eq!(merges, vec!["A1:A2"]);
    assert_eq!(
        (
            sheet.text("A1"),
            sheet.text("B1"),
            sheet.text("B2"),
            sheet.text("A3"),
            sheet.text("B3"),
            sheet.text("B4")
        ),
        ("Genre", "Total", "Count", "Total", "3", "2")
    );
    assert_eq!(book.file_name, "Books.xlsx");
    assert!(part(&book.bytes, "xl/workbook.xml").contains("name=\"Books\""));
}

#[test]
fn text_that_could_be_a_formula_is_written_as_text_with_the_quote_prefix() {
    let mut document = flat();
    let bad = [
        "=1+1",
        "+1",
        "-1+2",
        "@SUM(A1)",
        "\t=1",
        "\r=1",
        "=HYPERLINK(\"http://x\",\"y\")",
    ];
    let fine = ["a=b", " =1", "1+1", "Total"];
    document.rows = bad
        .iter()
        .chain(&fine)
        .map(|label| Row {
            label: (*label).into(),
            indent: 0,
            bold: false,
            cells: vec![int(1, false)],
        })
        .collect();
    document.headers[1][0].label = "=Count".into();
    let sheet = open(&write(&document).unwrap().bytes);
    assert!(
        !sheet.xml.contains("<f>") && !sheet.xml.contains("<f "),
        "no cell is a formula"
    );
    for (i, label) in bad.iter().chain(&fine).enumerate() {
        let at = format!("A{}", i + 3);
        assert_eq!(sheet.cells[&at].1, "s", "{label:?} is a string cell");
        assert_eq!(
            sheet.text(&at),
            *label,
            "the text is kept exactly as posted"
        );
        let quoted = sheet
            .xf(&at)
            .get("quotePrefix")
            .is_some_and(|flag| flag == "1");
        assert_eq!(
            quoted,
            i < bad.len(),
            "{label:?}: quote prefix exactly on the risky starts"
        );
    }
    assert_eq!(sheet.text("B2"), "=Count");
    assert_eq!(
        sheet.xf("B2").get("quotePrefix").map(String::as_str),
        Some("1"),
        "header cells are guarded too"
    );
}

#[test]
fn the_sheet_and_file_names_follow_the_title_within_the_spreadsheet_rules() {
    assert_eq!(sheet_name("Sales: [Q1] / what?"), "Sales Q1  what");
    assert_eq!(sheet_name("'quoted'"), "quoted");
    assert_eq!(sheet_name(&"x".repeat(40)).chars().count(), 31);
    assert_eq!(sheet_name("///"), "Pivot");
    // a cut that ends in an apostrophe, the reserved name and an emptied title still give a usable name
    let cut = format!("{}'x", "a".repeat(30));
    assert_eq!(sheet_name(&cut), "a".repeat(30));
    assert_eq!(sheet_name("History"), "Pivot");
    assert_eq!(sheet_name("history"), "Pivot");
    assert_eq!(sheet_name("'''"), "Pivot");
    assert!(sheet_name(&"\u{1F600}".repeat(40)).encode_utf16().count() <= 31);
    // and each of them gives a file, never an error
    for title in [cut.as_str(), "History", "///", "\u{1F600}"] {
        let mut document = flat();
        document.title = title.into();
        assert!(write(&document).is_ok(), "{title:?}");
    }
    assert_eq!(
        file_name("Sales / 2026: \"Q1\"\r\n"),
        "Sales _ 2026_ _Q1___.xlsx"
    );
    assert_eq!(file_name("../../etc/passwd"), "_.._etc_passwd.xlsx");
    assert_eq!(file_name(" . "), "Pivot.xlsx");
}

#[test]
fn column_widths_follow_the_content_within_the_bounds() {
    let mut document = flat();
    document.rows[1].label = "A very long label ".repeat(10);
    let sheet = open(&write(&document).unwrap().bytes);
    // the file stores the width plus the spreadsheet's cell padding (under one character)
    assert!(
        (sheet.columns[0].1 - MAX_WIDTH).abs() < 1.0,
        "{:?}",
        sheet.columns
    );
    assert!(
        sheet.columns[1].1 >= MIN_WIDTH && sheet.columns[1].1 < MAX_WIDTH,
        "{:?}",
        sheet.columns
    );
}
