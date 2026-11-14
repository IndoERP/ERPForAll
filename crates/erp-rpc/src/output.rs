//! JSON output: results and error bodies, written straight to bytes with a byte budget
//! (`limits.rpc_result_bytes`, and design note §8) so an oversized result is
//! refused without building it whole.
//!
//! Numbers, dates and decimals are formatted straight into the output buffer, and result batches
//! (`Value::Row`, `Value::Rows`) are written row by row from their typed columns with
//! the layout's pre-escaped keys: no intermediate map and no `String` per value.

use std::fmt::Write as _;

use erp_fields::Value;
use erp_fields::rows::{CellRef, Rows, RowsRef};

/// Deepest result value written (a method can return nested JSON documents).
pub(crate) const MAX_RESULT_DEPTH: usize = 1024;

/// Why a value could not be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteError {
    /// Over the byte budget.
    TooLarge,
    /// A float that JSON cannot represent (NaN, infinity).
    NonFinite,
    /// Nested deeper than [`MAX_RESULT_DEPTH`].
    TooDeep,
    /// A result batch contradicts itself or a value's formatting failed (never for batches built
    /// by `erp_fields::rows`, which checks them when they are built).
    Inconsistent(&'static str),
    /// A value or batch cell of a kind added after this writer was written.
    Unsupported(&'static str),
}

/// What [`JsonWriter::row_at`] does after [`JsonWriter::step`] read a cell.
enum Step<'a> {
    /// Nothing more: the key was absent, or the cell was written.
    Written,
    /// A dynamic value, written by [`JsonWriter::value_at`].
    Value(&'a Value),
    /// One child row.
    Row(&'a Rows, usize),
    /// Child rows, as an array.
    Children(&'a Rows, erp_fields::rows::ChildPositions<'a>),
}

/// Ids of one id list written between two budget checks.
const IDS_PER_CHECK: usize = 1024;

/// The resolved cell at layout position `pos` of row `row`. Kept out of the recursive writer
/// functions so its error value does not widen their stack frames.
///
/// The error keeps a fixed message and drops the row and position on purpose: `WriteError`
/// holds only `&'static str` so it stays two words, allocates nothing and keeps the recursive
/// frames small, and the case cannot happen (`row_at` reads only rows below the batch length and
/// positions below the layout length, and `Rows::cell` fails only for those or for a column
/// shorter than its batch, which the builder refuses).
#[inline(never)]
fn cell_of(rows: &Rows, row: usize, pos: usize) -> Result<CellRef<'_>, WriteError> {
    rows.cell(row, pos)
        .map_err(|_| WriteError::Inconsistent("a batch cell could not be read"))
}

/// Text or bytes longer than this are followed by a budget check of their own (design note
/// : the overshoot stays bounded by one value).
const LONG_TEXT: usize = 1024;

/// `fmt::Write` into the output bytes, so `write!` formats without a `String`.
struct Bytes<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Bytes<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// A streaming JSON writer with an optional byte budget.
pub(crate) struct JsonWriter {
    out: Vec<u8>,
    limit: Option<usize>,
    /// Per open container: whether a member was written (comma needed).
    stack: Vec<bool>,
    /// The next value completes a `key:` pair.
    after_key: bool,
}

impl JsonWriter {
    /// No budget (error bodies, whose parts are bounded by construction).
    pub(crate) fn unbounded() -> Self {
        Self {
            out: Vec::with_capacity(256),
            limit: None,
            stack: Vec::new(),
            after_key: false,
        }
    }

    /// At most `limit` bytes.
    pub(crate) fn bounded(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..Self::unbounded()
        }
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.out
    }

    fn over(&self) -> bool {
        self.limit.is_some_and(|l| self.out.len() > l)
    }

    fn separate(&mut self) {
        if self.after_key {
            self.after_key = false;
            return;
        }
        if let Some(has) = self.stack.last_mut() {
            if *has {
                self.out.push(b',');
            }
            *has = true;
        }
    }

    pub(crate) fn begin_object(&mut self) {
        self.separate();
        self.out.push(b'{');
        self.stack.push(false);
    }

    pub(crate) fn end_object(&mut self) {
        self.stack.pop();
        self.out.push(b'}');
    }

    pub(crate) fn empty_object(&mut self) {
        self.begin_object();
        self.end_object();
    }

    pub(crate) fn begin_array(&mut self) {
        self.separate();
        self.out.push(b'[');
        self.stack.push(false);
    }

    pub(crate) fn end_array(&mut self) {
        self.stack.pop();
        self.out.push(b']');
    }

    pub(crate) fn key(&mut self, key: &str) {
        self.separate();
        erp_fields::json::write_string(&mut self.out, key);
        self.out.push(b':');
        self.after_key = true;
    }

    /// A key already written as JSON with its colon (a batch layout's `"key":`).
    fn raw_key(&mut self, json_key: &[u8]) {
        self.separate();
        self.out.extend_from_slice(json_key);
        self.after_key = true;
    }

    pub(crate) fn string(&mut self, s: &str) {
        self.separate();
        erp_fields::json::write_string(&mut self.out, s);
    }

    pub(crate) fn int(&mut self, i: i64) {
        self.separate();
        self.out
            .extend_from_slice(itoa::Buffer::new().format(i).as_bytes());
    }

    pub(crate) fn uint(&mut self, value: u64) {
        self.separate();
        self.out
            .extend_from_slice(itoa::Buffer::new().format(value).as_bytes());
    }

    fn boolean(&mut self, b: bool) {
        self.separate();
        self.out
            .extend_from_slice(if b { b"true" } else { b"false" });
    }

    fn falsy(&mut self) {
        self.boolean(false);
    }

    /// `display` formatted into the buffer, between quotes when `quoted`.
    fn formatted(
        &mut self,
        display: impl std::fmt::Display,
        quoted: bool,
    ) -> Result<(), WriteError> {
        self.separate();
        if quoted {
            self.out.push(b'"');
        }
        write!(Bytes(&mut self.out), "{display}")
            .map_err(|_| WriteError::Inconsistent("a value could not be formatted"))?;
        if quoted {
            self.out.push(b'"');
        }
        Ok(())
    }

    fn float(&mut self, f: f64) -> Result<(), WriteError> {
        if !f.is_finite() {
            return Err(WriteError::NonFinite);
        }
        // `{:?}` gives the shortest round-trip text and keeps a `.0` on integral values (`1.0`,
        // as Python's `json.dumps(1.0)`), `1e-7`, `1e21`
        self.formatted(format_args!("{f:?}"), false)
    }

    fn text(&mut self, s: &str) -> Result<(), WriteError> {
        self.string(s);
        if s.len() > LONG_TEXT && self.over() {
            return Err(WriteError::TooLarge);
        }
        Ok(())
    }

    pub(crate) fn null(&mut self) {
        self.separate();
        self.out.extend_from_slice(b"null");
    }

    /// Append an already-written JSON value.
    pub(crate) fn raw(&mut self, json: &[u8]) {
        self.separate();
        self.out.extend_from_slice(json);
    }

    pub(crate) fn value(&mut self, value: &Value) -> Result<(), WriteError> {
        self.value_at(value, 0)
    }

    /// Every nesting kind recurses through here; the leaves are written by
    /// [`JsonWriter::scalar_value`], so each nesting level costs a small stack frame (a result
    /// nested [`MAX_RESULT_DEPTH`] deep is written or refused within 1 MiB of stack in a debug
    /// build, measured 2026-10-19).
    fn value_at(&mut self, value: &Value, depth: usize) -> Result<(), WriteError> {
        if depth > MAX_RESULT_DEPTH {
            return Err(WriteError::TooDeep);
        }
        match value {
            Value::List(items) => self.list_at(items, depth)?,
            Value::Map(map) => self.map_at(map, depth)?,
            Value::Row(row) => self.row_at(row.rows(), row.index(), depth)?,
            Value::Rows(rows) => self.rows_at(rows, depth)?,
            Value::Json(j) => self.json_at(j, depth)?,
            _ => self.scalar_value(value, depth)?,
        }
        if self.over() {
            return Err(WriteError::TooLarge);
        }
        Ok(())
    }

    /// A value that holds no other value.
    #[inline(never)]
    fn scalar_value(&mut self, value: &Value, depth: usize) -> Result<(), WriteError> {
        match value {
            Value::False => self.falsy(),
            Value::Bool(b) => self.boolean(*b),
            Value::BigInt(i) => self.formatted(i, true)?,
            Value::Int(i) => self.int(*i),
            Value::Float(f) => self.float(*f)?,
            Value::Decimal(d) => self.formatted(d.normalize(), false)?,
            Value::Str(s) => self.string(s),
            Value::Bytes(b) => self.bytes(b)?,
            Value::Date(d) => self.formatted(d, true)?,
            Value::DateTime(d) => self.formatted(d, true)?,
            Value::Range(range) => self.range_at(range, depth)?,
            // a variant added to `Value` after this writer: refused, never written as something else
            _ => {
                return Err(WriteError::Unsupported(
                    "a value of a kind this writer does not know",
                ));
            }
        }
        Ok(())
    }

    fn list_at(&mut self, items: &[Value], depth: usize) -> Result<(), WriteError> {
        self.begin_array();
        for item in items {
            self.value_at(item, depth + 1)?;
            if self.over() {
                return Err(WriteError::TooLarge);
            }
        }
        self.end_array();
        Ok(())
    }

    fn map_at(&mut self, map: &erp_fields::ValueMap, depth: usize) -> Result<(), WriteError> {
        self.begin_object();
        for (k, v) in map {
            self.key(k);
            self.value_at(v, depth + 1)?;
            if self.over() {
                return Err(WriteError::TooLarge);
            }
        }
        self.end_object();
        Ok(())
    }

    /// The rows of a batch (or one parent's children) as an array of objects.
    fn rows_at(&mut self, rows: &RowsRef, depth: usize) -> Result<(), WriteError> {
        let positions = rows
            .positions()
            .map_err(|_| WriteError::Inconsistent("child positions past their level"))?;
        self.begin_array();
        for row in positions {
            self.row_at(rows.rows(), row, depth + 1)?;
            // after every row: a row without keys checks nothing itself
            if self.over() {
                return Err(WriteError::TooLarge);
            }
        }
        self.end_array();
        Ok(())
    }

    /// A range value as `[lower, upper]`, `false` for an unbounded end.
    fn range_at(&mut self, range: &erp_fields::RangeValue, depth: usize) -> Result<(), WriteError> {
        if depth.saturating_add(1) > MAX_RESULT_DEPTH {
            return Err(WriteError::TooDeep);
        }
        self.begin_array();
        match range {
            erp_fields::RangeValue::Date(r) => {
                for end in [r.lower(), r.upper()] {
                    match end {
                        Some(d) => self.formatted(d, true)?,
                        None => self.falsy(),
                    }
                }
            }
            erp_fields::RangeValue::Datetime(r) => {
                for end in [r.lower(), r.upper()] {
                    match end {
                        Some(d) => self.formatted(d, true)?,
                        None => self.falsy(),
                    }
                }
            }
        }
        self.end_array();
        Ok(())
    }

    /// Row `row` of `rows` as an object: the present keys in layout order, each key's
    /// pre-escaped bytes copied from the layout and its value written from the batch's resolved
    /// cell. The budget is checked after every value, so an overshoot stays bounded by one value
    /// (and by [`IDS_PER_CHECK`] ids inside an id list).
    fn row_at(&mut self, rows: &Rows, row: usize, depth: usize) -> Result<(), WriteError> {
        if depth > MAX_RESULT_DEPTH {
            return Err(WriteError::TooDeep);
        }
        let layout = rows.layout();
        self.begin_object();
        for pos in 0..layout.len() {
            // the nesting kinds recurse from this frame; reading the cell, writing its key and
            // writing scalars happen in `step`, so this frame (one per nesting level) stays small
            match self.step(rows, row, pos, depth)? {
                Step::Written => {}
                Step::Value(value) => self.value_at(value, depth + 1)?,
                Step::Row(rows, row) => self.row_at(rows, row, depth + 1)?,
                Step::Children(rows, positions) => {
                    self.begin_array();
                    for row in positions {
                        self.row_at(rows, row, depth + 2)?;
                        // after every child row: a row without keys checks nothing itself
                        if self.over() {
                            return Err(WriteError::TooLarge);
                        }
                    }
                    self.end_array();
                }
            }
            if self.over() {
                return Err(WriteError::TooLarge);
            }
        }
        self.end_object();
        Ok(())
    }

    /// Reads the cell at layout position `pos` of row `row` and writes its key (unless absent)
    /// and, for a cell that nests nothing, its value; a nesting cell is handed back to
    /// [`JsonWriter::row_at`].
    #[inline(never)]
    fn step<'a>(
        &mut self,
        rows: &'a Rows,
        row: usize,
        pos: usize,
        depth: usize,
    ) -> Result<Step<'a>, WriteError> {
        let cell = cell_of(rows, row, pos)?;
        if matches!(cell, CellRef::Absent) {
            return Ok(Step::Written);
        }
        self.key_at(rows.layout(), pos)?;
        Ok(match cell {
            CellRef::Value(value) => Step::Value(value),
            CellRef::Row { rows, row } => Step::Row(rows, row),
            CellRef::Children { rows, positions } => {
                // the list itself counts, even empty, as a list of maps does
                if depth + 1 > MAX_RESULT_DEPTH {
                    return Err(WriteError::TooDeep);
                }
                Step::Children(rows, positions)
            }
            other => {
                self.scalar_cell(other, depth + 1)?;
                Step::Written
            }
        })
    }

    /// The pre-escaped `"key":` of layout position `pos`.
    fn key_at(&mut self, layout: &erp_fields::rows::Layout, pos: usize) -> Result<(), WriteError> {
        let json_key = layout
            .json_key(pos)
            .ok_or(WriteError::Inconsistent("a layout key has no JSON bytes"))?;
        self.raw_key(json_key);
        Ok(())
    }

    /// A cell that holds no row: scalars, id lists, labels, JSON documents, ranges.
    #[inline(never)]
    fn scalar_cell(&mut self, cell: CellRef<'_>, depth: usize) -> Result<(), WriteError> {
        if depth > MAX_RESULT_DEPTH {
            return Err(WriteError::TooDeep);
        }
        match cell {
            CellRef::False => self.falsy(),
            CellRef::Bool(b) => self.boolean(b),
            CellRef::BigInt(i) => self.formatted(i, true)?,
            CellRef::Int(i) => self.int(i),
            CellRef::Float(f) => self.float(f)?,
            CellRef::Decimal(d) => self.formatted(d.normalize(), false)?,
            CellRef::Str(s) => self.text(s)?,
            CellRef::Bytes(b) => self.bytes(b)?,
            CellRef::Date(d) => self.formatted(d, true)?,
            CellRef::DateTime(d) => self.formatted(d, true)?,
            CellRef::Json(j) => self.json_at(j, depth)?,
            CellRef::Range(r) => self.range_at(&r, depth)?,
            CellRef::Ids(ids) => {
                // the ids nest one level below the list, as a list of `Value::Int` does
                if !ids.is_empty() && depth.saturating_add(1) > MAX_RESULT_DEPTH {
                    return Err(WriteError::TooDeep);
                }
                self.begin_array();
                for chunk in ids.chunks(IDS_PER_CHECK) {
                    for id in chunk {
                        self.int(id.get());
                    }
                    if self.over() {
                        return Err(WriteError::TooLarge);
                    }
                }
                self.end_array();
            }
            CellRef::Label { id, name } => {
                if depth.saturating_add(1) > MAX_RESULT_DEPTH {
                    return Err(WriteError::TooDeep);
                }
                self.begin_array();
                self.int(id);
                match name {
                    Some(name) => self.text(name)?,
                    None => self.falsy(),
                }
                self.end_array();
            }
            // `row_at` writes the nesting kinds and skips absent keys itself
            CellRef::Absent
            | CellRef::Value(_)
            | CellRef::Row { .. }
            | CellRef::Children { .. } => {
                return Err(WriteError::Inconsistent(
                    "a nested or absent cell reached the scalar writer",
                ));
            }
            // a cell kind added to `CellRef` after this writer: refused, never written as
            // something else
            _ => {
                return Err(WriteError::Unsupported(
                    "a batch cell of a kind this writer does not know",
                ));
            }
        }
        Ok(())
    }

    /// Raw bytes as a base64 string, encoded straight into the buffer.
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        self.separate();
        self.out.push(b'"');
        erp_fields::base64::encode_into(&mut self.out, bytes);
        self.out.push(b'"');
        if bytes.len() > LONG_TEXT && self.over() {
            return Err(WriteError::TooLarge);
        }
        Ok(())
    }

    fn json_at(
        &mut self,
        json: &erp_fields::serde_json::Value,
        depth: usize,
    ) -> Result<(), WriteError> {
        use erp_fields::serde_json::Value as J;
        if depth > MAX_RESULT_DEPTH {
            return Err(WriteError::TooDeep);
        }
        match json {
            J::Null => self.null(),
            J::Bool(b) => {
                self.separate();
                self.out
                    .extend_from_slice(if *b { b"true" } else { b"false" });
            }
            J::Number(n) => self.formatted(n, false)?,
            J::String(s) => self.string(s),
            J::Array(items) => {
                self.begin_array();
                for item in items {
                    self.json_at(item, depth + 1)?;
                    if self.over() {
                        return Err(WriteError::TooLarge);
                    }
                }
                self.end_array();
            }
            J::Object(map) => {
                self.begin_object();
                for (k, v) in map {
                    self.key(k);
                    self.json_at(v, depth + 1)?;
                    if self.over() {
                        return Err(WriteError::TooLarge);
                    }
                }
                self.end_object();
            }
        }
        Ok(())
    }
}

/// Serialize a result value within `limit` bytes.
pub(crate) fn serialize(value: &Value, limit: usize) -> Result<Vec<u8>, WriteError> {
    let mut w = JsonWriter::bounded(limit);
    w.value(value)?;
    Ok(w.into_bytes())
}

/// Write one complete report. All names and sample arrays are engine-bounded;
/// field identities come from validated metadata, never the rejected body.
pub(crate) fn html_report(
    w: &mut JsonWriter,
    model: Option<&str>,
    field: Option<&str>,
    language: Option<&str>,
    report: &erp_fields::html::RemovalReport,
) {
    w.begin_object();
    for (key, value) in [("model", model), ("field", field), ("language", language)] {
        w.key(key);
        match value {
            Some(text) => w.string(text),
            None => w.null(),
        }
    }
    w.key("total_count");
    w.uint(report.total_count());
    w.key("entries");
    w.begin_array();
    for entry in report.entries() {
        w.begin_object();
        w.key("kind");
        w.string(entry.kind().as_str());
        w.key("name");
        w.string(entry.name());
        w.key("reason");
        w.string(entry.reason().as_str());
        w.key("count");
        w.uint(entry.count());
        w.key("sample_locations");
        w.begin_array();
        for sample in entry.sample_locations() {
            w.uint(*sample);
        }
        w.end_array();
        w.end_object();
    }
    w.end_array();
    w.end_object();
}

/// Report envelopes share the ordinary result budget and idempotency bytes.
pub(crate) fn serialize_html(
    value: &Value,
    reports: Option<&[erp_orm::html::HtmlFieldReport]>,
    limit: usize,
) -> Result<Vec<u8>, WriteError> {
    let Some(reports) = reports else {
        return serialize(value, limit);
    };
    let mut w = JsonWriter::bounded(limit);
    w.begin_object();
    w.key("result");
    w.value(value)?;
    w.key("html_reports");
    w.begin_array();
    for report in reports {
        html_report(
            &mut w,
            Some(report.model()),
            Some(report.field()),
            report.language(),
            report.removals(),
        );
        if w.over() {
            return Err(WriteError::TooLarge);
        }
    }
    w.end_array();
    w.end_object();
    if w.over() {
        return Err(WriteError::TooLarge);
    }
    Ok(w.into_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use std::sync::Arc;

    use erp_fields::rows::Rows;
    use erp_fields::{Value, ValueMap};

    use super::*;

    #[test]
    fn values_are_written_as_reference_reads_them() {
        let map: ValueMap = [
            ("id".to_owned(), Value::Int(7)),
            ("name".to_owned(), Value::str("é \"q\" \\ \n\u{1}")),
            ("empty".to_owned(), Value::False),
            ("flag".to_owned(), Value::Bool(true)),
            ("ratio".to_owned(), Value::Float(1.0)),
            ("tiny".to_owned(), Value::Float(1e-7)),
            ("raw".to_owned(), Value::Bytes(Arc::from(&b"x  \x01-"[..]))),
            (
                "doc".to_owned(),
                Value::Json(Arc::new(erp_fields::serde_json::json!({"a": [1, null]}))),
            ),
            (
                "ids".to_owned(),
                Value::list([Value::Int(1), Value::Int(2)]),
            ),
        ]
        .into_iter()
        .collect();
        let bytes = serialize(&Value::Map(Arc::new(map)), 1 << 20).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"id":7,"name":"é \"q\" \\ \n\u0001","empty":false,"flag":true,"ratio":1.0,"tiny":1e-7,"raw":"eCAgAS0=","doc":{"a":[1,null]},"ids":[1,2]}"#
        );
    }

    fn map(pairs: &[(&str, Value)]) -> ValueMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    fn text(value: &Value) -> String {
        String::from_utf8(serialize(value, 1 << 24).unwrap()).unwrap()
    }

    /// The JSON of a batch and of the same data as a list of maps, both written by this writer.
    fn same_json(rows: Rows, maps: Vec<ValueMap>) {
        let as_maps = Value::list(maps.into_iter().map(|m| Value::Map(Arc::new(m))));
        assert_eq!(text(&Value::from(rows)), text(&as_maps));
    }

    #[test]
    fn a_batch_is_written_byte_for_byte_like_its_maps() {
        let date = erp_fields::Date::from_ymd(2026, 10, 19).unwrap();
        let at = erp_fields::DateTime::from_parts(date, 23, 59, 58, 7).unwrap();
        let range = erp_fields::RangeValue::Date(erp_fields::DateRange::new(Some(date), None));
        let maps = vec![
            map(&[
                ("id", Value::Int(i64::MIN)),
                ("naïve \"key\"\n", Value::str("漢字 \u{1} \\ é")),
                ("flag", Value::Bool(false)),
                ("ratio", Value::Float(1e-7)),
                ("big", Value::Float(1e21)),
                ("whole", Value::Float(3.0)),
                (
                    "amount",
                    Value::Decimal(erp_fields::Decimal::new(125_000, 4)),
                ),
                ("day", Value::Date(date)),
                ("at", Value::DateTime(at)),
                ("raw", Value::Bytes(Arc::from(&b"x  \x01-"[..]))),
                (
                    "doc",
                    Value::Json(Arc::new(
                        erp_fields::serde_json::json!({"a": [1, null, 2.5]}),
                    )),
                ),
                ("span", Value::Range(range)),
                ("mixed", Value::list([Value::Int(1), Value::str("a")])),
            ]),
            map(&[
                ("id", Value::Int(i64::MAX)),
                ("naïve \"key\"\n", Value::False),
                ("flag", Value::Bool(true)),
                ("ratio", Value::False),
                ("big", Value::Float(-0.0)),
                ("whole", Value::Float(0.1)),
                ("amount", Value::False),
                ("day", Value::False),
                ("at", Value::False),
                ("raw", Value::False),
                ("doc", Value::False),
                ("span", Value::False),
                ("mixed", Value::str("text")),
            ]),
            // a row without most keys: they are left out, not written as false
            map(&[("id", Value::Int(0)), ("flag", Value::False)]),
        ];
        same_json(Rows::from_maps(maps.clone()).unwrap(), maps);
    }

    #[test]
    fn nested_children_ids_and_labels_are_written_like_maps() {
        use erp_fields::rows::{
            Children, Column, ColumnData, Key, Layout, NO_TARGET, RowsBuilder, Targets,
        };
        let line_codes = ["L0", "L1", "L2"];
        let line_layout =
            Layout::for_model("bench.order.line", [Key::from("id"), Key::from("code")]).unwrap();
        let mut b = RowsBuilder::new(line_layout, 3).unwrap();
        b.column("id", Column::new(ColumnData::Int(vec![1, 2, 3])))
            .unwrap();
        b.column(
            "code",
            Column::new(ColumnData::Str(
                line_codes.iter().map(|c| Some(Arc::from(*c))).collect(),
            )),
        )
        .unwrap();
        let lines = b.finish().unwrap();
        let partners = Rows::from_maps([map(&[("id", Value::Int(10))])]).unwrap();
        let tags = Rows::from_maps([
            map(&[("id", Value::Int(20)), ("code", Value::str("T20"))]),
            map(&[("id", Value::Int(21))]),
        ])
        .unwrap();
        let ids: [Arc<[erp_fields::RecId]>; 2] = [
            [erp_fields::RecId::from_real(5).unwrap()].into(),
            Arc::from(Vec::new()),
        ];
        let layout = Layout::for_model(
            "bench.order",
            [
                "id",
                "partner_id",
                "line_ids",
                "tag_ids",
                "member_ids",
                "user_id",
            ]
            .into_iter()
            .map(Key::from),
        )
        .unwrap();
        let mut b = RowsBuilder::new(layout, 2).unwrap();
        b.column("id", Column::new(ColumnData::Int(vec![1, 2])))
            .unwrap();
        b.column(
            "partner_id",
            Column::new(ColumnData::One(
                Targets::new(partners.clone(), vec![0, NO_TARGET]).unwrap(),
            )),
        )
        .unwrap();
        b.column(
            "line_ids",
            Column::new(ColumnData::Many(
                Children::contiguous(lines.clone(), vec![0, 1, 3]).unwrap(),
            )),
        )
        .unwrap();
        b.column(
            "tag_ids",
            Column::new(ColumnData::Many(
                Children::indexed(tags.clone(), vec![0, 2, 3], vec![1, 0, 0]).unwrap(),
            )),
        )
        .unwrap();
        b.column(
            "member_ids",
            Column::new(ColumnData::Ids(ids.iter().cloned().map(Some).collect())),
        )
        .unwrap();
        b.column(
            "user_id",
            Column::new(ColumnData::Label {
                ids: vec![2, 0],
                names: vec![Some(Arc::from("Mitchell \"Admin\"")), None],
            })
            .with_false(erp_fields::rows::Bitmap::from_rows([1]).unwrap()),
        )
        .unwrap();
        let orders = b.finish().unwrap();
        // the same data as maps, built from the child batches' own maps
        let line_map = |i: usize| Value::Map(Arc::new(lines.row(i).unwrap().to_map().unwrap()));
        let tag_map = |i: usize| Value::Map(Arc::new(tags.row(i).unwrap().to_map().unwrap()));
        let maps = vec![
            map(&[
                ("id", Value::Int(1)),
                (
                    "partner_id",
                    Value::Map(Arc::new(partners.row(0).unwrap().to_map().unwrap())),
                ),
                ("line_ids", Value::list([line_map(0)])),
                ("tag_ids", Value::list([tag_map(1), tag_map(0)])),
                ("member_ids", Value::list([Value::Int(5)])),
                (
                    "user_id",
                    Value::list([Value::Int(2), Value::str("Mitchell \"Admin\"")]),
                ),
            ]),
            map(&[
                ("id", Value::Int(2)),
                ("partner_id", Value::False),
                ("line_ids", Value::list([line_map(1), line_map(2)])),
                ("tag_ids", Value::list([tag_map(0)])),
                ("member_ids", Value::list([])),
                ("user_id", Value::False),
            ]),
        ];
        same_json(orders.clone(), maps.clone());
        // one row, and the children of one parent, alone
        assert_eq!(
            text(&Value::Row(orders.row(1).unwrap())),
            text(&Value::Map(Arc::new(maps[1].clone())))
        );
        assert_eq!(
            text(&orders.get(1, "line_ids").unwrap()),
            text(&Value::list([line_map(1), line_map(2)]))
        );
        // a batch inside a map (web_search_read's `records`)
        let wrapped = |records: Value| {
            Value::Map(Arc::new(map(&[
                ("length", Value::Int(2)),
                ("records", records),
            ])))
        };
        assert_eq!(
            text(&wrapped(Value::from(orders))),
            text(&wrapped(Value::list(
                maps.into_iter().map(|m| Value::Map(Arc::new(m)))
            )))
        );
    }

    #[test]
    fn a_wide_row_and_retyped_columns_are_written_like_maps() {
        let wide = map(&(0..500)
            .map(|i| (format!("field_{i}"), Value::Int(i)))
            .collect::<Vec<_>>()
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect::<Vec<_>>());
        same_json(Rows::from_maps([wide.clone()]).unwrap(), vec![wide]);

        let mut maps = vec![
            map(&[("id", Value::Int(1)), ("hours", Value::Float(1.5))]),
            map(&[("id", Value::Int(2)), ("hours", Value::Float(0.0))]),
        ];
        let mut rows = Rows::from_maps(maps.clone()).unwrap();
        // an override renders hours as "HH:MM" text and blanks zero, a key added to one row
        rows.set(0, "hours", "01:30").unwrap();
        rows.set(1, "hours", Value::False).unwrap();
        rows.insert_key("help").unwrap();
        rows.set(1, "help", "Read me").unwrap();
        maps[0].insert("hours".to_owned(), Value::str("01:30"));
        maps[1].insert("hours".to_owned(), Value::False);
        maps[1].insert("help".to_owned(), Value::str("Read me"));
        same_json(rows, maps);
    }

    #[test]
    fn a_batch_respects_the_budget_floats_and_depth() {
        let maps: Vec<ValueMap> = (0..1000)
            .map(|i| map(&[("id", Value::Int(i)), ("code", Value::str("x".repeat(20)))]))
            .collect();
        let rows = Value::from(Rows::from_maps(maps).unwrap());
        let whole = serialize(&rows, usize::MAX).unwrap().len();
        // over budget in the middle of the batch: refused, not cut short
        assert_eq!(
            serialize(&rows, whole / 2).unwrap_err(),
            WriteError::TooLarge
        );
        assert_eq!(
            serialize(&rows, whole - 1).unwrap_err(),
            WriteError::TooLarge
        );
        assert_eq!(serialize(&rows, whole).unwrap().len(), whole);
        // one long text value over the budget is caught right after it
        let long = Value::from(
            Rows::from_maps([map(&[("note", Value::str("y".repeat(10_000)))])]).unwrap(),
        );
        assert_eq!(serialize(&long, 5_000).unwrap_err(), WriteError::TooLarge);

        let nan = Value::from(Rows::from_maps([map(&[("x", Value::Float(f64::NAN))])]).unwrap());
        assert_eq!(serialize(&nan, 100).unwrap_err(), WriteError::NonFinite);
    }

    /// A value whose innermost `leaf` sits exactly `depth` levels down: `Rows` of one row (or a
    /// list of one map) per two levels, one list for an odd remainder.
    fn nested(leaf: Value, depth: usize, through_rows: bool) -> Value {
        let mut value = leaf;
        for _ in 0..depth / 2 {
            value = if through_rows {
                Value::from(Rows::from_maps([map(&[("x", value)])]).unwrap())
            } else {
                Value::list([Value::Map(Arc::new(map(&[("x", value)])))])
            };
        }
        if depth % 2 == 1 {
            value = Value::list([value]);
        }
        value
    }

    #[test]
    fn nesting_through_rows_stops_exactly_where_nesting_through_maps_does() {
        let label = |depth| {
            let layout = erp_fields::rows::Layout::new([erp_fields::rows::Key::from("user_id")]);
            let mut b = erp_fields::rows::RowsBuilder::new(layout.unwrap(), 1).unwrap();
            b.column(
                "user_id",
                erp_fields::rows::Column::new(erp_fields::rows::ColumnData::Label {
                    ids: vec![2],
                    names: vec![Some(Arc::from("Admin"))],
                }),
            )
            .unwrap();
            // the label's two items sit two levels below the batch
            nested(Value::from(b.finish().unwrap()), depth, true)
        };
        for through_rows in [true, false] {
            for leaf in [Value::Int(0), Value::list([Value::Int(1)])] {
                // the leaf's own items count: a list leaf adds a level
                let extra = usize::from(matches!(leaf, Value::List(_)));
                let at_most = MAX_RESULT_DEPTH - extra;
                let deepest = nested(leaf.clone(), at_most, through_rows);
                assert!(
                    serialize(&deepest, usize::MAX).is_ok(),
                    "{through_rows} {extra}"
                );
                assert_eq!(
                    serialize(&nested(leaf, at_most + 1, through_rows), usize::MAX).unwrap_err(),
                    WriteError::TooDeep,
                    "{through_rows} {extra}"
                );
            }
        }
        // typed id lists and labels nest like the lists they are written as
        let ids = |depth| {
            let rows =
                Rows::from_maps([map(&[("tag_ids", Value::list([Value::Int(1)]))])]).unwrap();
            nested(Value::from(rows), depth, true)
        };
        let as_list = |depth| {
            nested(
                Value::list([Value::Map(Arc::new(map(&[(
                    "tag_ids",
                    Value::list([Value::Int(1)]),
                )])))]),
                depth,
                true,
            )
        };
        for (deep, name) in [
            (&ids as &dyn Fn(usize) -> Value, "ids"),
            (&label, "label"),
            (&as_list, "maps"),
        ] {
            let at_most = MAX_RESULT_DEPTH - 3;
            assert!(serialize(&deep(at_most), usize::MAX).is_ok(), "{name}");
            assert_eq!(
                serialize(&deep(at_most + 1), usize::MAX).unwrap_err(),
                WriteError::TooDeep,
                "{name}"
            );
        }
    }

    /// Writes `value` with a budget of `limit`; the error and how many bytes were written when
    /// it was refused.
    fn refused_at(value: &Value, limit: usize) -> (WriteError, usize) {
        let mut w = JsonWriter::bounded(limit);
        let error = w.value(value).unwrap_err();
        (error, w.into_bytes().len())
    }

    #[test]
    fn a_huge_id_list_and_many_long_texts_are_refused_at_the_budget() {
        use erp_fields::rows::{Column, ColumnData, Key, Layout, RowsBuilder};
        // one row, one many2many with a million ids
        let ids: Arc<[erp_fields::RecId]> = (1..=1_000_000)
            .map(|i| erp_fields::RecId::from_real(i).unwrap())
            .collect();
        let mut b = RowsBuilder::new(Layout::new([Key::from("tag_ids")]).unwrap(), 1).unwrap();
        b.column("tag_ids", Column::new(ColumnData::Ids(vec![Some(ids)])))
            .unwrap();
        let huge = Value::from(b.finish().unwrap());
        let limit = 64 * 1024;
        let (error, written) = refused_at(&huge, limit);
        assert_eq!(error, WriteError::TooLarge);
        // checked every IDS_PER_CHECK ids (at most 8 bytes each here), not after the whole list
        assert!(written <= limit + IDS_PER_CHECK * 8, "{written}");

        // one row of 200 long texts: checked after each text, not once at the end of the row
        let text = "z".repeat(4_000);
        let keys: Vec<(String, Value)> = (0..200)
            .map(|i| (format!("note_{i}"), Value::str(text.as_str())))
            .collect();
        let wide = map(&keys
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect::<Vec<_>>());
        let wide = Value::from(Rows::from_maps([wide]).unwrap());
        let limit = 50_000;
        let (error, written) = refused_at(&wide, limit);
        assert_eq!(error, WriteError::TooLarge);
        assert!(written <= limit + text.len() + 16, "{written}");
        // and many short texts, each under the long-text threshold: checked after every value
        let short = "s".repeat(900);
        let keys: Vec<(String, Value)> = (0..2_000)
            .map(|i| (format!("k{i}"), Value::str(short.as_str())))
            .collect();
        let wide = map(&keys
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect::<Vec<_>>());
        let wide = Value::from(Rows::from_maps([wide]).unwrap());
        let (error, written) = refused_at(&wide, limit);
        assert_eq!(error, WriteError::TooLarge);
        assert!(written <= limit + short.len() + 16, "{written}");
    }

    #[test]
    fn keyless_rows_and_children_are_refused_at_the_budget() {
        use erp_fields::rows::{Children, Column, ColumnData, Key, Layout, RowsBuilder};
        // rows without keys write `{}` and check nothing themselves
        let keyless = Rows::from_maps(vec![ValueMap::default(); 100_000]).unwrap();
        assert_eq!(keyless.keys().count(), 0);
        let limit = 1_000;
        let (error, written) = refused_at(&Value::from(keyless.clone()), limit);
        assert_eq!(error, WriteError::TooLarge);
        assert!(written <= limit + 3, "{written}");
        // the same rows as the children of one parent row
        let mut b = RowsBuilder::new(Layout::new([Key::from("line_ids")]).unwrap(), 1).unwrap();
        b.column(
            "line_ids",
            Column::new(ColumnData::Many(
                Children::contiguous(keyless, vec![0, 100_000]).unwrap(),
            )),
        )
        .unwrap();
        let parent = Value::from(b.finish().unwrap());
        let (error, written) = refused_at(&parent, limit);
        assert_eq!(error, WriteError::TooLarge);
        assert!(written <= limit + 3, "{written}");
        // within the budget both are written whole
        assert!(serialize(&parent, 1 << 20).is_ok());
    }

    #[test]
    fn dates_decimals_ranges_and_labels_match_a_hand_checked_golden() {
        use erp_fields::rows::{Column, ColumnData, Key, Layout, RowsBuilder};
        let date = erp_fields::Date::from_ymd(2026, 10, 19).unwrap();
        let at = erp_fields::DateTime::from_parts(date, 23, 59, 58, 7).unwrap();
        let amount = erp_fields::Decimal::new(125_000, 4);
        let range = erp_fields::RangeValue::Date(erp_fields::DateRange::new(Some(date), None));
        let layout = Layout::for_model(
            "bench.order",
            ["day", "at", "amount", "span", "user_id"].map(Key::from),
        )
        .unwrap();
        let mut b = RowsBuilder::new(layout, 1).unwrap();
        b.column("day", Column::new(ColumnData::Date(vec![date])))
            .unwrap();
        b.column("at", Column::new(ColumnData::DateTime(vec![at])))
            .unwrap();
        b.column("amount", Column::new(ColumnData::Decimal(vec![amount])))
            .unwrap();
        b.column("span", Column::new(ColumnData::Range(vec![Some(range)])))
            .unwrap();
        b.column(
            "user_id",
            Column::new(ColumnData::Label {
                ids: vec![2],
                names: vec![Some(Arc::from("Mitchell \"Admin\""))],
            }),
        )
        .unwrap();
        let rows = b.finish().unwrap();
        let written = text(&Value::Row(rows.row(0).unwrap()));
        assert_eq!(
            written,
            r#"{"day":"2026-10-19","at":"2026-10-19 23:59:58","amount":12.5,"span":["2026-10-19",false],"user_id":[2,"Mitchell \"Admin\""]}"#
        );
        // the format the writer used before batches (origin/main `output.rs`): each value's
        // `to_string`, quoted and escaped as a JSON string; decimals normalized
        let old = |s: String| erp_fields::serde_json::to_string(&s).unwrap();
        let before = format!(
            r#"{{"day":{},"at":{},"amount":{},"span":[{},false],"user_id":[2,{}]}}"#,
            old(date.to_string()),
            old(at.to_string()),
            amount.normalize(),
            old(date.to_string()),
            old("Mitchell \"Admin\"".to_owned()),
        );
        assert_eq!(written, before);
    }

    #[test]
    fn rows_from_maps_are_written_in_the_first_seen_key_order() {
        let rows = Rows::from_maps([
            map(&[("a", Value::Int(1)), ("b", Value::Int(2))]),
            map(&[("b", Value::Int(3)), ("a", Value::Int(4))]),
        ])
        .unwrap();
        assert_eq!(text(&Value::from(rows)), r#"[{"a":1,"b":2},{"a":4,"b":3}]"#);
    }

    #[test]
    fn the_budget_and_depth_are_enforced() {
        let big = Value::list((0..1000).map(Value::Int));
        assert_eq!(serialize(&big, 100).unwrap_err(), WriteError::TooLarge);
        assert!(serialize(&big, 10_000).is_ok());
        assert_eq!(
            serialize(&Value::Float(f64::NAN), 100).unwrap_err(),
            WriteError::NonFinite
        );
        // the leaf exactly MAX_RESULT_DEPTH lists down is written; one more list is refused
        let mut deep = Value::Int(0);
        for _ in 0..MAX_RESULT_DEPTH {
            deep = Value::list([deep]);
        }
        assert!(serialize(&deep, usize::MAX).is_ok());
        assert_eq!(
            serialize(&Value::list([deep]), usize::MAX).unwrap_err(),
            WriteError::TooDeep
        );
    }
}

#[cfg(test)]
mod bigint_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::rows::Rows;
    use erp_fields::{BigInt, ValueMap};
    #[test]
    fn bigint_wire_is_exact_in_dynamic_and_typed_rows() {
        for n in [i64::MIN, 0, 9007199254740993, i64::MAX] {
            let value = Value::BigInt(BigInt::from_i64(n));
            let bytes = serialize(&value, 1024).unwrap();
            assert_eq!(bytes, format!("\"{n}\"").as_bytes());
            assert_eq!(
                serialize(&Value::list([value.clone()]), 1024).unwrap(),
                format!("[\"{n}\"]").as_bytes()
            );
            let map: ValueMap = [("counter".to_owned(), value)].into_iter().collect();
            let batch = Value::from(Rows::from_maps([map]).unwrap());
            assert_eq!(
                serialize(&batch, 1024).unwrap(),
                format!("[{{\"counter\":\"{n}\"}}]").as_bytes()
            );
            assert_eq!(serialize(&batch, 1), Err(WriteError::TooLarge));
        }
        assert_eq!(serialize(&Value::Int(42), 1024).unwrap(), b"42");
        assert_eq!(serialize(&Value::str("42"), 1024).unwrap(), b"\"42\"");
    }
}
