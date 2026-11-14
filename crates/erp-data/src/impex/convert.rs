//! Conversion needs answers from the database (name searches, external ids, existing ids). The
//! converter asks them through [`Lookups`] and is run twice per batch: once with a
//! [`Collector`] that records every question, then, after the questions are answered with a few
//! set-based statements, with the answers ([`Answers`]). Both runs take the same branches because
//! the questions asked never depend on earlier answers within a record.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use erp_fields::convert::{FieldRef, to_cell};
use erp_fields::{Cell, Date, DateTime, Decimal, Error, FieldType, RecId, Result, Value, ValueMap};
use erp_orm::{Command, RawVals};
use erp_registry::{FieldSpec, ModelId, ModelSpec, Registry};

use super::extract::{Key, Raw, RawRecord, comodel};
use super::message::MessageType;

// ----- messages ---------------------------------------------------------------------------------

/// A piece of a message template.
#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Text(String),
    Field,
}

/// A message whose field label is filled in when it is logged.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Template(Vec<Piece>);

impl Template {
    fn new() -> Self {
        Self::default()
    }

    fn text(mut self, text: impl Into<String>) -> Self {
        self.0.push(Piece::Text(text.into()));
        self
    }

    fn field(mut self) -> Self {
        self.0.push(Piece::Field);
        self
    }

    /// The message with `label` as the field.
    pub(crate) fn render(&self, label: &str) -> String {
        let mut out = String::new();
        for piece in &self.0 {
            match piece {
                Piece::Text(t) => out.push_str(t),
                Piece::Field => out.push_str(label),
            }
        }
        out
    }

    /// `%(field)s` → `%(field)s/<suffix>` (an error inside a one2many line names the line's field).
    fn nest(self, suffix: &str) -> Self {
        let mut pieces = Vec::with_capacity(self.0.len() + 1);
        for piece in self.0 {
            match piece {
                Piece::Field => {
                    pieces.push(Piece::Field);
                    pieces.push(Piece::Text(format!("/{suffix}")));
                }
                text => pieces.push(text),
            }
        }
        Self(pieces)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Extra {
    pub(crate) moreinfo: Option<Value>,
    pub(crate) field_path: Option<Vec<String>>,
    pub(crate) field_type: Option<String>,
    pub(crate) value: Option<String>,
}

/// A problem found while converting one field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Issue {
    pub(crate) kind: MessageType,
    pub(crate) template: Template,
    pub(crate) extra: Option<Box<Extra>>,
    pub(crate) record_level: bool,
    /// A reference that matched no record (`db_id_for`'s "No matching record found").
    pub(crate) not_found: bool,
}

impl Issue {
    fn error(template: Template) -> Self {
        Self {
            kind: MessageType::Error,
            template,
            extra: None,
            record_level: false,
            not_found: false,
        }
    }

    fn warning(template: Template) -> Self {
        Self {
            kind: MessageType::Warning,
            ..Self::error(template)
        }
    }

    fn with_extra(mut self, extra: Extra) -> Self {
        self.extra = Some(Box::new(extra));
        self
    }
}

/// An issue of one top-level field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FieldIssue {
    pub(crate) field: &'static str,
    pub(crate) label: String,
    pub(crate) issue: Issue,
}

// ----- lookups ----------------------------------------------------------------------------------

/// Questions the converter asks the database.
pub(crate) trait Lookups {
    /// `name_search(name, operator='=')` on `comodel`: matching ids in order.
    fn name(&mut self, comodel: &ModelSpec, name: &str) -> Vec<i64>;
    /// A fully qualified external id whose record exists in `comodel`'s table: its model and id.
    fn xid(&mut self, comodel: &ModelSpec, xid: &str) -> Option<(String, i64)>;
    /// Whether `id` exists in `comodel`.
    fn dbid(&mut self, comodel: &ModelSpec, id: i64) -> bool;
}

/// Records the questions; answers as if every reference existed once.
#[derive(Debug, Default)]
pub(crate) struct Collector {
    pub(crate) names: Vec<(ModelId, String)>,
    pub(crate) xids: Vec<(ModelId, String)>,
    pub(crate) dbids: Vec<(ModelId, i64)>,
}

impl Lookups for Collector {
    fn name(&mut self, comodel: &ModelSpec, name: &str) -> Vec<i64> {
        self.names.push((comodel.id, name.to_owned()));
        vec![1]
    }

    fn xid(&mut self, comodel: &ModelSpec, xid: &str) -> Option<(String, i64)> {
        self.xids.push((comodel.id, xid.to_owned()));
        Some((comodel.name.to_owned(), 1))
    }

    fn dbid(&mut self, comodel: &ModelSpec, id: i64) -> bool {
        self.dbids.push((comodel.id, id));
        true
    }
}

/// The answers to collected questions. A question that was not collected first is recorded in
/// `missed` (a bug of the two-pass conversion), so the caller fails instead of reading "not found".
#[derive(Debug, Default)]
pub(crate) struct Answers {
    pub(crate) names: HashMap<(ModelId, String), Vec<i64>>,
    pub(crate) xids: HashMap<(ModelId, String), (String, i64)>,
    pub(crate) asked_xids: HashSet<(ModelId, String)>,
    pub(crate) dbids: HashSet<(ModelId, i64)>,
    pub(crate) asked_dbids: HashSet<(ModelId, i64)>,
    pub(crate) missed: Option<String>,
}

impl Answers {
    fn unanswered(&mut self, question: String) {
        self.missed.get_or_insert(question);
    }
}

impl Lookups for Answers {
    fn name(&mut self, comodel: &ModelSpec, name: &str) -> Vec<i64> {
        match self.names.get(&(comodel.id, name.to_owned())) {
            Some(ids) => ids.clone(),
            None => {
                self.unanswered(format!("name '{name}' of {}", comodel.name));
                Vec::new()
            }
        }
    }

    fn xid(&mut self, comodel: &ModelSpec, xid: &str) -> Option<(String, i64)> {
        let key = (comodel.id, xid.to_owned());
        if !self.asked_xids.contains(&key) {
            self.unanswered(format!("external id '{xid}' of {}", comodel.name));
        }
        self.xids.get(&key).cloned()
    }

    fn dbid(&mut self, comodel: &ModelSpec, id: i64) -> bool {
        if !self.asked_dbids.contains(&(comodel.id, id)) {
            self.unanswered(format!("id {id} of {}", comodel.name));
        }
        self.dbids.contains(&(comodel.id, id))
    }
}

// ----- conversion -------------------------------------------------------------------------------

/// Settings of one `load` call, from its context.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// `module` (default `__import__`): prefix of unqualified external ids.
    pub(crate) module: String,
    /// `import_file`: reference errors carry `field_path`, `field_type` and `value`.
    pub(crate) import_file: bool,
    /// `update_many2many`: many2many references add links instead of replacing them.
    pub(crate) update_many2many: bool,
    /// The environment's timezone, for datetimes.
    pub(crate) tz: String,
}

impl Settings {
    /// `xid` qualified by the import module when it has no module.
    pub(crate) fn qualify(&self, xid: &str) -> String {
        if xid.contains('.') {
            xid.to_owned()
        } else {
            format!("{}.{xid}", self.module)
        }
    }
}

/// A one2many line carrying a new external id, lifted out of `Command::Create` to be created with
/// its own `ir.model.data` entry in the load session.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiftedLine {
    /// Comodel of the one2many line.
    pub(crate) comodel: ModelId,
    /// Inverse Many2one field on the comodel pointing back to the parent.
    pub(crate) inverse: String,
    /// The qualified external id for this line.
    pub(crate) xid: String,
    /// Field values for this child record.
    pub(crate) vals: RawVals,
    /// Any nested lifted lines below this child.
    pub(crate) lifted: Vec<LiftedLine>,
}

/// The converter of one model.
pub(crate) struct Converter<'a> {
    pub(crate) registry: &'a Registry,
    pub(crate) settings: &'a Settings,
}

/// Python `str.lower()` membership.
fn lower(text: &str) -> String {
    text.to_lowercase()
}

/// Translated words of installed languages are not known yet (no `res.lang`).
fn str_to_boolean(value: &str) -> (bool, Option<Issue>) {
    let v = lower(value);
    if matches!(v.as_str(), "1" | "true" | "yes") {
        return (true, None);
    }
    if matches!(v.as_str(), "" | "0" | "false" | "no") {
        return (false, None);
    }
    let issue = Issue::error(
        Template::new()
            .text(format!("Unknown value '{value}' for boolean field '"))
            .field()
            .text("'"),
    )
    .with_extra(Extra {
        moreinfo: Some(Value::str("Use '1' for yes and '0' for no")),
        ..Extra::default()
    });
    (true, Some(issue))
}

/// The outcome of Python's `int(text)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PyInt {
    /// A valid literal that fits `i64`.
    Fits(i64),
    /// A valid literal beyond `i64`.
    Huge,
    /// Not an integer literal.
    Invalid,
}

/// Python's `int(text)` on decimal literals: surrounding whitespace, an optional sign, digits
/// with single underscores between them.
fn py_int(text: &str) -> PyInt {
    let t = text.trim();
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let bytes = digits.as_bytes();
    if bytes.is_empty()
        || !bytes.iter().all(|b| b.is_ascii_digit() || *b == b'_')
        || bytes.first() == Some(&b'_')
        || bytes.last() == Some(&b'_')
        || digits.contains("__")
    {
        return PyInt::Invalid;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    let signed = if negative { format!("-{clean}") } else { clean };
    match signed.parse::<i64>() {
        Ok(n) => PyInt::Fits(n),
        Err(_) => PyInt::Huge,
    }
}

/// Python's `float(text)` for finite values (Rust's grammar after trimming; non-finite values are
/// refused here, the ORM would refuse them anyway).
fn py_float(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|f| f.is_finite())
}

/// Python's `float(text)` for a numeric column, kept exact: a finite decimal literal (with an
/// exponent or not) that fits a 96-bit decimal.
fn py_decimal(text: &str) -> Option<Decimal> {
    py_float(text)?;
    let t = text.trim();
    Decimal::from_str(t)
        .or_else(|_| Decimal::from_scientific(t))
        .ok()
}

fn local_to_utc(local: DateTime, tz: &str) -> Option<DateTime> {
    const MICROS: i64 = 1_000_000;
    let shift = |offset: i32| {
        i64::from(offset)
            .checked_mul(MICROS)
            .and_then(|d| local.micros().checked_sub(d))
            .map(DateTime::from_micros)
    };
    let guess = shift(local.offset_seconds(tz)?)?;
    shift(guess.offset_seconds(tz)?)
}

fn action(res_model: &str, domain_model: Option<&str>) -> Value {
    let mut map = ValueMap::new();
    map.insert("name".to_owned(), Value::str("Possible Values"));
    map.insert("type".to_owned(), Value::str("ir.actions.act_window"));
    map.insert("target".to_owned(), Value::str("new"));
    map.insert("view_mode".to_owned(), Value::str("list,form"));
    map.insert(
        "views".to_owned(),
        Value::list([
            Value::list([Value::False, Value::str("list")]),
            Value::list([Value::False, Value::str("form")]),
        ]),
    );
    let mut context = ValueMap::new();
    context.insert("create".to_owned(), Value::Bool(false));
    map.insert("context".to_owned(), Value::Map(Arc::new(context)));
    map.insert("help".to_owned(), Value::str("See all possible values"));
    map.insert("res_model".to_owned(), Value::str(res_model));
    if let Some(model) = domain_model {
        map.insert(
            "domain".to_owned(),
            Value::list([Value::list([
                Value::str("model"),
                Value::str("="),
                Value::str(model),
            ])]),
        );
    }
    Value::Map(Arc::new(map))
}

/// What a field converts to.
enum Converted {
    Cell(Cell),
    Commands(Vec<Command>),
}

fn field_path<'a>(hierarchy: &[String], field: &str, value: &Raw<'a>) -> Vec<String> {
    let mut path: Vec<String> = hierarchy.to_vec();
    path.push(field.to_owned());
    let mut current = value;
    while let Raw::Records(records) = current {
        let Some((key, inner)) = records.first().and_then(|r| r.entries.first()) else {
            break;
        };
        if let Key::Field(name) = key {
            path.push(name.to_string());
        }
        current = inner;
    }
    path
}

impl Converter<'_> {
    /// Convert one record of `spec`.
    /// `hierarchy` names the one2many fields this record is a line of.
    pub(crate) fn record<'a>(
        &self,
        spec: &ModelSpec,
        record: &RawRecord<'a>,
        hierarchy: &[String],
        lookups: &mut dyn Lookups,
    ) -> Result<(RawVals, Vec<FieldIssue>, Vec<LiftedLine>)> {
        let mut vals = RawVals::new();
        let mut issues = Vec::new();
        let mut lifted = Vec::new();
        for (key, raw) in &record.entries {
            if key.is_reference() {
                continue;
            }
            let Key::Field(name) = key else { continue };
            let field = spec
                .field(name.as_ref())
                .ok_or_else(|| Error::programming(format!("Invalid field name '{name}'")))?;
            let label = field.string.clone();
            let outcome = if raw.is_truthy() {
                self.field(spec, field, raw, hierarchy, lookups)?
            } else {
                empty(spec, field).map(|converted| (converted, Vec::new(), Vec::new()))
            };
            match outcome {
                Ok((converted, warnings, field_lifted)) => {
                    match converted {
                        Converted::Cell(cell) => {
                            vals.insert(field.name.into(), cell);
                        }
                        Converted::Commands(commands) => {
                            if !commands.is_empty() {
                                vals.insert_commands(field.name.into(), commands);
                            }
                        }
                    }
                    for issue in warnings {
                        issues.push(FieldIssue {
                            field: field.name,
                            label: label.clone(),
                            issue,
                        });
                    }
                    lifted.extend(field_lifted);
                }
                Err(mut issue) => {
                    if self.settings.import_file
                        && let Some(extra) = &mut issue.extra
                        && extra.field_path.is_none()
                        && (extra.moreinfo.is_some()
                            || extra.field_type.is_some()
                            || extra.value.is_some())
                    {
                        extra.field_path = Some(field_path(hierarchy, field.name, raw));
                    }
                    issues.push(FieldIssue {
                        field: field.name,
                        label,
                        issue,
                    });
                }
            }
        }
        Ok((vals, issues, lifted))
    }

    /// Convert one non-empty value: `Err` is the field's error, the warnings come with the value.
    /// `Result::Err` of the outer result is a failure of the import itself (a programming error).
    #[allow(
        clippy::type_complexity,
        reason = "two levels of failure, documented above"
    )]
    fn field<'a>(
        &self,
        spec: &ModelSpec,
        field: &FieldSpec,
        raw: &Raw<'a>,
        hierarchy: &[String],
        lookups: &mut dyn Lookups,
    ) -> Result<std::result::Result<(Converted, Vec<Issue>, Vec<LiftedLine>), Issue>> {
        let text = match raw {
            Raw::Text(t) => Some(*t),
            Raw::Records(_) => None,
        };
        let records = match raw {
            Raw::Records(r) => Some(r.as_slice()),
            Raw::Text(_) => None,
        };
        let not_text = || {
            Error::programming(format!(
                "field '{}' of '{}' received sub-records",
                field.name, spec.name
            ))
        };
        let scalar = |value: Value, warnings: Vec<Issue>| {
            let fref = FieldRef {
                model: spec.name,
                field: field.name,
                label: &field.string,
            };
            match to_cell(fref, &field.ty, &value) {
                Ok(cell) => Ok((Converted::Cell(cell), warnings)),
                Err(e) => Err(Issue::error(Template::new().text(e.message()))),
            }
        };
        let invalid = |value: &str, what: &str| {
            Issue::error(
                Template::new()
                    .text(format!("'{value}' does not seem to be {what} for field '"))
                    .field()
                    .text("'"),
            )
            .with_extra(Extra::default())
        };
        let outcome = match &field.ty {
            FieldType::Id => Ok((Converted::Cell(Cell::Null), Vec::new())),
            // the row version is set by the server only
            FieldType::RowVersion => Err(Issue::error(
                Template::new()
                    .text("field '")
                    .field()
                    .text("' is set by the server and cannot be imported"),
            )
            .with_extra(Extra::default())),
            FieldType::Boolean => {
                let value = text.ok_or_else(not_text)?;
                let (b, issue) = str_to_boolean(value);
                scalar(Value::Bool(b), issue.into_iter().collect())
            }
            FieldType::Integer => {
                let value = text.ok_or_else(not_text)?;
                match py_int(value) {
                    PyInt::Fits(n) if i32::try_from(n).is_ok() => scalar(Value::Int(n), Vec::new()),
                    PyInt::Fits(_) | PyInt::Huge => Err(Issue {
                        record_level: true,
                        ..Issue::error(Template::new().text("integer out of range\n"))
                    }),
                    PyInt::Invalid => Err(invalid(value, "an integer")),
                }
            }
            FieldType::Many2oneReference { .. } => {
                let value = text.ok_or_else(not_text)?;
                match py_int(value) {
                    PyInt::Fits(n) => scalar(Value::Int(n), Vec::new()),
                    PyInt::Huge => Err(Issue {
                        record_level: true,
                        ..Issue::error(Template::new().text("bigint out of range\n"))
                    }),
                    PyInt::Invalid => Err(invalid(value, "an integer")),
                }
            }
            FieldType::Float { digits: None } => {
                let value = text.ok_or_else(not_text)?;
                match py_float(value) {
                    Some(f) => scalar(Value::Float(f), Vec::new()),
                    None => Err(invalid(value, "a number")),
                }
            }
            // numeric columns: parsed exactly as a decimal
            FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. } => {
                let value = text.ok_or_else(not_text)?;
                match py_decimal(value) {
                    Some(d) => scalar(Value::Decimal(d), Vec::new()),
                    None => Err(invalid(value, "a number")),
                }
            }
            FieldType::BigInt | FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz => {
                scalar(Value::str(text.ok_or_else(not_text)?), Vec::new())
            }
            FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Html
            | FieldType::Reference { .. } => {
                scalar(Value::str(text.ok_or_else(not_text)?), Vec::new())
            }
            FieldType::Selection { values, .. } => {
                let value = text.ok_or_else(not_text)?;
                let items = values.as_ref().ok_or_else(|| {
                    Error::programming(format!(
                        "importing into '{}' of '{}' needs its selection method, which the \
                         import does not call until M3",
                        field.name, spec.name
                    ))
                })?;
                let wanted = lower(value);
                match items
                    .iter()
                    .find(|(key, label)| lower(key) == wanted || lower(label) == wanted)
                {
                    Some((key, _)) => scalar(Value::str(key), Vec::new()),
                    None => Err(Issue::error(
                        Template::new()
                            .text(format!("Value '{value}' not found in selection field '"))
                            .field()
                            .text("'"),
                    )
                    .with_extra(Extra {
                        moreinfo: Some(Value::list(items.iter().filter_map(|(key, label)| {
                            if !label.is_empty() {
                                Some(Value::str(label))
                            } else if !key.is_empty() {
                                Some(Value::str(key))
                            } else {
                                None
                            }
                        }))),
                        ..Extra::default()
                    })),
                }
            }
            FieldType::Date => {
                let value = text.ok_or_else(not_text)?;
                match Date::parse_odoo(value) {
                    Some(d) => scalar(Value::Date(d), Vec::new()),
                    None => Err(Issue::error(
                        Template::new()
                            .text(format!(
                                "'{value}' does not seem to be a valid date for field '"
                            ))
                            .field()
                            .text("'"),
                    )
                    .with_extra(Extra {
                        moreinfo: Some(Value::str("Use the format '2012-12-31'")),
                        ..Extra::default()
                    })),
                }
            }
            FieldType::Datetime => {
                let value = text.ok_or_else(not_text)?;
                let parsed = DateTime::parse_odoo(value)
                    .and_then(|local| local_to_utc(local, &self.settings.tz));
                match parsed {
                    Some(dt) => scalar(Value::DateTime(dt), Vec::new()),
                    None => Err(Issue::error(
                        Template::new()
                            .text(format!(
                                "'{value}' does not seem to be a valid datetime for field '"
                            ))
                            .field()
                            .text("'"),
                    )
                    .with_extra(Extra {
                        moreinfo: Some(Value::str("Use the format '2012-12-31 23:59:59'")),
                        ..Extra::default()
                    })),
                }
            }
            FieldType::Json => {
                let value = text.ok_or_else(not_text)?;
                match serde_json::from_str::<serde_json::Value>(value) {
                    Ok(json) => scalar(Value::Json(Arc::new(json)), Vec::new()),
                    Err(_) => Err(Issue::error(
                        Template::new()
                            .text(format!(
                                "'{value}' does not seem to be a valid JSON for field '"
                            ))
                            .field()
                            .text("'"),
                    )
                    .with_extra(Extra::default())),
                }
            }
            FieldType::Many2one { .. } => {
                let records = records.ok_or_else(not_text)?;
                let comodel = comodel(self.registry, field)?;
                match self.many2one(comodel, records, lookups) {
                    Ok((id, warnings)) => {
                        Ok((Converted::Cell(id.map_or(Cell::Null, Cell::Id)), warnings))
                    }
                    Err(issue) => Err(issue),
                }
            }
            FieldType::Many2many { .. } => {
                let records = records.ok_or_else(not_text)?;
                let comodel = comodel(self.registry, field)?;
                self.many2many(comodel, records, lookups)
            }
            FieldType::One2many { .. } => {
                let records = records.ok_or_else(not_text)?;
                let comodel = comodel(self.registry, field)?;
                return self.one2many(spec, field, comodel, records, hierarchy, lookups);
            }
            // range fields have no text form an import accepts yet
            FieldType::DateRange { .. } | FieldType::DatetimeRange { .. } => Err(Issue::error(
                Template::new()
                    .text("Importing the range field '")
                    .field()
                    .text("' is not supported until a later milestone (range fields in imports)"),
            )
            .with_extra(Extra::default())),
        };
        Ok(outcome.map(|(converted, warnings)| (converted, warnings, Vec::new())))
    }

    fn referencing_key(record: &RawRecord<'_>) -> std::result::Result<Key, Issue> {
        if record.entries.iter().any(|(k, _)| !k.is_reference()) {
            return Err(Issue::error(Template::new().text(
                "Can not create Many-To-One records indirectly, import the field separately",
            )));
        }
        match record.entries.as_slice() {
            [(key, _)] => Ok(key.clone()),
            _ => Err(Issue::error(
                Template::new()
                    .text("Ambiguous specification for field '")
                    .field()
                    .text("', only provide one of name, external id or database id"),
            )),
        }
    }

    fn single<'a, 'r>(
        records: &'r [RawRecord<'a>],
    ) -> std::result::Result<&'r RawRecord<'a>, Issue> {
        match records {
            [record] => Ok(record),
            _ => Err(Issue::error(
                Template::new().text("too many values to unpack (expected 1)"),
            )),
        }
    }

    fn reference_text<'a>(
        record: &RawRecord<'a>,
        key: &Key,
    ) -> std::result::Result<&'a str, Issue> {
        match record.get(key) {
            Some(Raw::Text(t)) => Ok(*t),
            _ => Err(Issue::error(Template::new().text(
                "Can not create Many-To-One records indirectly, import the field separately",
            ))),
        }
    }

    fn many2one<'a>(
        &self,
        comodel: &ModelSpec,
        records: &[RawRecord<'a>],
        lookups: &mut dyn Lookups,
    ) -> std::result::Result<(Option<i64>, Vec<Issue>), Issue> {
        let record = Self::single(records)?;
        let key = Self::referencing_key(record)?;
        let value = Self::reference_text(record, &key)?;
        self.db_id_for(comodel, &key, value, lookups)
    }

    fn many2many<'a>(
        &self,
        comodel: &ModelSpec,
        records: &[RawRecord<'a>],
        lookups: &mut dyn Lookups,
    ) -> std::result::Result<(Converted, Vec<Issue>), Issue> {
        let record = Self::single(records)?;
        let key = Self::referencing_key(record)?;
        let value = Self::reference_text(record, &key)?;
        let mut ids = Vec::new();
        let mut warnings = Vec::new();
        for reference in value.split(',') {
            let (id, ws) = self.db_id_for(comodel, &key, reference, lookups)?;
            warnings.extend(ws);
            if let Some(id) = id.and_then(RecId::from_real) {
                ids.push(id);
            }
        }
        let commands = if self.settings.update_many2many {
            ids.into_iter().map(Command::Link).collect()
        } else {
            vec![Command::Set(ids)]
        };
        Ok((Converted::Commands(commands), warnings))
    }

    #[allow(clippy::type_complexity, reason = "same two levels as `field`")]
    fn one2many<'a>(
        &self,
        spec: &ModelSpec,
        field: &FieldSpec,
        comodel: &ModelSpec,
        records: &[RawRecord<'a>],
        hierarchy: &[String],
        lookups: &mut dyn Lookups,
    ) -> Result<std::result::Result<(Converted, Vec<Issue>, Vec<LiftedLine>), Issue>> {
        let mut warnings = Vec::new();
        let mut lifted = Vec::new();
        // a single row with only a reference: `ref1,ref2,…` as for many2many
        let split: Vec<RawRecord<'a>>;
        let lines: &[RawRecord<'a>] = match records {
            [only] if only.entries.iter().all(|(k, _)| k.is_reference()) => {
                let key = match Self::referencing_key(only) {
                    Ok(key) => key,
                    Err(issue) => return Ok(Err(issue)),
                };
                let value = match Self::reference_text(only, &key) {
                    Ok(value) => value,
                    Err(issue) => return Ok(Err(issue)),
                };
                split = value
                    .split(',')
                    .map(|item| RawRecord {
                        entries: vec![(key.clone(), Raw::Text(item))],
                    })
                    .collect();
                &split
            }
            _ => records,
        };
        let mut nested = hierarchy.to_vec();
        nested.push(field.name.to_owned());
        let mut commands = Vec::new();
        for line in lines {
            let writable = RawRecord {
                entries: line
                    .entries
                    .iter()
                    .filter(|(k, _)| !k.is_reference())
                    .cloned()
                    .collect(),
            };
            let refs = RawRecord {
                entries: line
                    .entries
                    .iter()
                    .filter(|(k, _)| k.is_reference())
                    .cloned()
                    .collect(),
            };
            let (vals, issues, child_lifted) = self.record(comodel, &writable, &nested, lookups)?;
            for found in issues {
                if found.issue.kind == MessageType::Warning {
                    warnings.push(found.issue);
                } else {
                    // the first error of a line fails the whole field, naming the line's field
                    let mut issue = found.issue;
                    issue.template = issue.template.nest(&found.label);
                    return Ok(Err(issue));
                }
            }
            let mut id = None;
            let mut new_xid = None;
            if !refs.entries.is_empty() {
                let key = match Self::referencing_key(&refs) {
                    Ok(key) => key,
                    Err(issue) => return Ok(Err(issue)),
                };
                let value = match Self::reference_text(&refs, &key) {
                    Ok(value) => value,
                    Err(issue) => return Ok(Err(issue)),
                };
                match self.db_id_for(comodel, &key, value, lookups) {
                    Ok((found, ws)) => {
                        warnings.extend(ws);
                        id = found;
                    }
                    Err(issue) if issue.not_found && key.field() == Some("id") => {
                        new_xid = Some(self.settings.qualify(value));
                    }
                    Err(issue) => return Ok(Err(issue)),
                }
            }
            if let Some(xid) = new_xid {
                let inverse = match &field.ty {
                    FieldType::One2many { inverse, .. } => inverse.clone(),
                    _ => {
                        return Err(Error::programming(format!(
                            "field '{}' of '{}' is not one2many",
                            field.name, spec.name
                        )));
                    }
                };
                lifted.push(LiftedLine {
                    comodel: comodel.id,
                    inverse,
                    xid,
                    vals,
                    lifted: child_lifted,
                });
            } else {
                if !child_lifted.is_empty() {
                    return Ok(Err(Issue::error(
                        Template::new()
                            .text(
                                "creating one2many lines with new external ids nested under a line \
                                 without an external id in field '",
                            )
                            .field()
                            .text("' is not supported until a later release"),
                    )));
                }
                match id.and_then(RecId::from_real) {
                    Some(rec) => {
                        commands.push(Command::Link(rec));
                        commands.push(Command::Update(rec, vals));
                    }
                    None => commands.push(Command::Create(vals)),
                }
            }
        }
        Ok(Ok((Converted::Commands(commands), warnings, lifted)))
    }

    fn db_id_for(
        &self,
        comodel: &ModelSpec,
        key: &Key,
        value: &str,
        lookups: &mut dyn Lookups,
    ) -> std::result::Result<(Option<i64>, Vec<Issue>), Issue> {
        let mut warnings = Vec::new();
        let (field_type, id, moreinfo) = match key {
            Key::Field(name) if name.as_ref() == ".id" => {
                let moreinfo = action("ir.model.data", Some(comodel.name));
                if !str_to_boolean(value).0 {
                    return Ok((None, warnings));
                }
                let id = match py_int(value) {
                    PyInt::Fits(n) => n,
                    PyInt::Huge | PyInt::Invalid => {
                        return Err(Issue::error(
                            Template::new()
                                .text(format!("Invalid database id '{value}' for the field '"))
                                .field()
                                .text("'"),
                        )
                        .with_extra(Extra {
                            moreinfo: Some(moreinfo),
                            ..Extra::default()
                        }));
                    }
                };
                let found = (id > 0 && lookups.dbid(comodel, id)).then_some(id);
                ("database id", found, moreinfo)
            }
            Key::Field(name) if name.as_ref() == "id" => {
                let moreinfo = action("ir.model.data", Some(comodel.name));
                if !str_to_boolean(value).0 {
                    return Ok((None, warnings));
                }
                let xid = self.settings.qualify(value);
                let found = match lookups.xid(comodel, &xid) {
                    Some((model, res_id)) => {
                        if model != comodel.name {
                            return Err(Issue::error(Template::new().text(format!(
                                "Invalid external ID {xid}: expected model '{}', found '{model}'",
                                comodel.name
                            ))));
                        }
                        Some(res_id)
                    }
                    None => None,
                };
                ("external id", found, moreinfo)
            }
            Key::Name => {
                let moreinfo = action(comodel.name, None);
                if value.is_empty() {
                    return Ok((None, warnings));
                }
                let ids = lookups.name(comodel, value);
                if ids.len() > 1 {
                    warnings.push(Issue::warning(
                        Template::new()
                            .text(format!(
                                "Found multiple matches for value \"{value}\" in field \""
                            ))
                            .field()
                            .text(format!("\" ({} matches)", ids.len())),
                    ));
                }
                ("name", ids.first().copied(), moreinfo)
            }
            Key::Field(other) => {
                return Err(Issue::error(
                    Template::new().text(format!("Unknown sub-field “{other}”")),
                ));
            }
        };
        if id.is_some() {
            return Ok((id, warnings));
        }
        let shown: String = if self.settings.import_file {
            value.chars().take(50).collect()
        } else {
            value.to_owned()
        };
        let mut extra = Extra {
            moreinfo: Some(moreinfo),
            ..Extra::default()
        };
        if self.settings.import_file {
            extra.value = Some(shown.clone());
            extra.field_type = Some(field_type.to_owned());
        }
        Err(Issue {
            not_found: true,
            ..Issue::error(
                Template::new()
                    .text(format!(
                        "No matching record found for {field_type} '{shown}' in field '"
                    ))
                    .field()
                    .text("'"),
            )
            .with_extra(extra)
        })
    }
}

fn empty(spec: &ModelSpec, field: &FieldSpec) -> std::result::Result<Converted, Issue> {
    if field.ty.is_x2many() {
        return Ok(Converted::Commands(vec![Command::Clear]));
    }
    let fref = FieldRef {
        model: spec.name,
        field: field.name,
        label: &field.string,
    };
    to_cell(fref, &field.ty, &Value::False)
        .map(Converted::Cell)
        .map_err(|e| Issue::error(Template::new().text(e.message())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_int_literals() {
        assert_eq!(py_int(" 42 "), PyInt::Fits(42));
        assert_eq!(py_int("-0"), PyInt::Fits(0));
        assert_eq!(py_int("+7"), PyInt::Fits(7));
        assert_eq!(py_int("1_000"), PyInt::Fits(1000));
        assert_eq!(py_int("99999999999999999999"), PyInt::Huge);
        assert_eq!(py_int("zorglub"), PyInt::Invalid);
        assert_eq!(py_int(""), PyInt::Invalid);
        assert_eq!(py_int("1__0"), PyInt::Invalid);
        assert_eq!(py_int("_1"), PyInt::Invalid);
        assert_eq!(py_int("1.0"), PyInt::Invalid);
    }

    #[test]
    fn booleans_follow_reference_words() {
        for word in ["0", "no", "false", "FALSE", ""] {
            assert_eq!(str_to_boolean(word), (false, None), "{word}");
        }
        for word in ["1", "yes", "true", "TRUE"] {
            assert_eq!(str_to_boolean(word), (true, None), "{word}");
        }
        let (value, issue) = str_to_boolean("VRAI");
        assert!(value);
        let issue = issue.unwrap();
        assert_eq!(issue.kind, MessageType::Error);
        assert_eq!(
            issue.template.render("Value"),
            "Unknown value 'VRAI' for boolean field 'Value'"
        );
    }

    #[test]
    fn nested_template_names_the_line_field() {
        let t = Template::new().text("in '").field().text("'").nest("M2O");
        assert_eq!(t.render("Value"), "in 'Value/M2O'");
    }

    #[test]
    fn local_datetime_to_utc() {
        let local = DateTime::parse_odoo("2012-01-01 10:00:00").unwrap();
        assert_eq!(local_to_utc(local, "UTC"), Some(local));
        let utc = local_to_utc(local, "Europe/Brussels").unwrap();
        assert_eq!(utc.to_string(), "2012-01-01 09:00:00");
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::{TimestampTz, Uuid, Xid8};
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
    #[test]
    fn native_fields_import_values_remain_native_and_strict() {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "native_import",
            depends: &["base"],
            exact_only: false,
        };
        static MODEL: ModelDecl = ModelDecl::new("native_import", "native.import").fields(
            const {
                &[
                    FieldDecl::uuid("u"),
                    FieldDecl::xid8("x"),
                    FieldDecl::timestamptz("t"),
                ]
            },
        );
        let registry = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([&MODEL])
            .build()
            .unwrap();
        let spec = registry.model("native.import").unwrap();
        let settings = Settings {
            module: "native_import".into(),
            import_file: true,
            update_many2many: false,
            tz: "Asia/Jakarta".into(),
        };
        let converter = Converter {
            registry: &registry,
            settings: &settings,
        };
        for (name, text, expected) in [
            (
                "u",
                "00000000-0000-0000-0000-000000000000",
                Cell::Uuid(Uuid::NIL),
            ),
            (
                "x",
                "18446744073709551615",
                Cell::Xid8(Xid8::from_u64(u64::MAX)),
            ),
            (
                "t",
                "2024-03-01T01:00:00.123456+07:00",
                Cell::TimestampTz(TimestampTz::parse("2024-02-29T18:00:00.123456Z").unwrap()),
            ),
        ] {
            let mut lookups = Answers::default();
            let (actual, warnings, lifted) = converter
                .field(
                    spec,
                    spec.field(name).unwrap(),
                    &Raw::Text(text),
                    &[],
                    &mut lookups,
                )
                .unwrap()
                .unwrap();
            let Converted::Cell(actual) = actual else {
                panic!("native scalar became commands")
            };
            assert_eq!(actual, expected);
            assert!(warnings.is_empty());
            assert!(lifted.is_empty());
        }
        let issue = converter
            .field(
                spec,
                spec.field("x").unwrap(),
                &Raw::Text("-1"),
                &[],
                &mut Answers::default(),
            )
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(issue.kind, MessageType::Error);
        assert_eq!(
            issue.template.render("Xid"),
            "native.import.x: invalid xid8: expected canonical unsigned 64-bit decimal text"
        );
    }
}
