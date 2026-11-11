//! The web client's *specification* (GRP §4.8): a nested description of the fields to return,
//! and the keyword-argument parsing shared by the web methods on the dynamic surface.
//!
//! Everything here reads client input, so malformed values are `Validation` errors (HTTP 422,
//! ) naming the method and the argument.

use std::borrow::Cow;
use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{Date, Error, Result, Value, ValueMap};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpecItem {
    /// Nested specification (`'fields'`); `Some(empty)` differs from `None`.
    pub fields: Option<Spec>,
    /// Context for the co-records (`'context'`). Shared, like [`Spec`]'s fields: the onchange
    /// diff copies the items of the fields that changed, once per call.
    pub context: Option<Arc<[(String, Value)]>>,
    /// Most co-records read per record (`'limit'`, x2many with `fields` only).
    pub limit: Option<u64>,
    /// Order of x2many ids (`'order'`).
    pub order: Option<String>,
}

impl SpecItem {
    /// `{'fields': spec}`.
    pub fn fields(spec: Spec) -> Self {
        Self {
            fields: Some(spec),
            ..Self::default()
        }
    }

    /// No key at all (`{}`).
    pub fn is_empty(&self) -> bool {
        self.fields.is_none()
            && self.context.is_none()
            && self.limit.is_none()
            && self.order.is_none()
    }

    /// Set `limit`.
    #[must_use]
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set `order`.
    #[must_use]
    pub fn order(mut self, order: impl Into<String>) -> Self {
        self.order = Some(order.into());
        self
    }

    /// Set `context`.
    #[must_use]
    pub fn context<K: Into<String>>(mut self, pairs: impl IntoIterator<Item = (K, Value)>) -> Self {
        self.context = Some(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect());
        self
    }

    fn to_value(&self) -> Value {
        let mut map = ValueMap::new();
        if let Some(fields) = &self.fields {
            map.insert("fields".to_owned(), fields.to_value());
        }
        if let Some(context) = &self.context {
            map.insert(
                "context".to_owned(),
                Value::Map(Arc::new(context.iter().cloned().collect())),
            );
        }
        if let Some(limit) = self.limit {
            map.insert("limit".to_owned(), Value::Int(limit as i64));
        }
        if let Some(order) = &self.order {
            map.insert("order".to_owned(), Value::str(order));
        }
        Value::Map(Arc::new(map))
    }
}

/// Deepest nesting of a specification accepted from a client.
pub const MAX_SPEC_DEPTH: usize = 16;

/// Most fields (all levels together) of a specification accepted from a client.
pub const MAX_SPEC_FIELDS: usize = 5_000;

/// A specification: field name → what to return, in order.
///
/// The fields are shared (`Arc`), so a clone is a reference count, never a copy of the tree: the
/// method chain takes a specification by value, and `onchange_batch` hands the same one to every
/// entry's `onchange` (review J2-WEB N8). An empty specification holds nothing and allocates
/// nothing; one being built by [`Spec::with`] is copied only while it is shared.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    /// `None` when empty.
    items: Option<Arc<Vec<(String, SpecItem)>>>,
}

impl PartialEq for Spec {
    fn eq(&self, other: &Self) -> bool {
        self.items() == other.items()
    }
}

impl Spec {
    /// An empty specification (reads only `id`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a field read as is (`{}`).
    #[must_use]
    pub fn field(self, name: impl Into<String>) -> Self {
        self.with(name, SpecItem::default())
    }

    /// Add a field with options.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, item: SpecItem) -> Self {
        let name = name.into();
        let items = Arc::make_mut(self.items.get_or_insert_with(Arc::default));
        match items.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = item,
            None => items.push((name, item)),
        }
        self
    }

    fn items(&self) -> &[(String, SpecItem)] {
        self.items.as_deref().map_or(&[], Vec::as_slice)
    }

    fn from_items(items: Vec<(String, SpecItem)>) -> Self {
        Self {
            items: (!items.is_empty()).then(|| Arc::new(items)),
        }
    }

    /// The field names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.items().iter().map(|(n, _)| n.as_str())
    }

    /// The fields and their options.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SpecItem)> {
        self.items().iter().map(|(n, i)| (n.as_str(), i))
    }

    /// The options of a field.
    pub fn get(&self, name: &str) -> Option<&SpecItem> {
        self.items().iter().find(|(n, _)| n == name).map(|(_, i)| i)
    }

    /// Whether the field is in the specification.
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// No field.
    pub fn is_empty(&self) -> bool {
        self.items().is_empty()
    }

    /// Number of fields.
    pub fn len(&self) -> usize {
        self.items().len()
    }

    /// The same specification without `name`.
    pub fn without(&self, name: &str) -> Self {
        Self::from_items(
            self.items()
                .iter()
                .filter(|(n, _)| n != name)
                .cloned()
                .collect(),
        )
    }

    /// Parse a specification from client input (a dictionary of dictionaries), keeping the
    /// client's field order. Keys of the inner dictionaries other than `fields`, `context`, `limit`
    /// and `order` are refused, and so are specifications nested deeper than
    /// [`MAX_SPEC_DEPTH`] levels or naming more than [`MAX_SPEC_FIELDS`] fields in total.
    pub fn from_value(value: &Value) -> Result<Self> {
        let mut count = 0usize;
        Self::from_value_at(value, 1, &mut count)
    }

    fn from_value_at(value: &Value, depth: usize, count: &mut usize) -> Result<Self> {
        if depth > MAX_SPEC_DEPTH {
            return Err(Error::validation(format!(
                "Invalid specification: nested deeper than {MAX_SPEC_DEPTH} levels"
            )));
        }
        let Value::Map(map) = value else {
            return Err(Error::validation(format!(
                "Invalid specification {value}: expected a dictionary of field names"
            )));
        };
        *count += map.len();
        if *count > MAX_SPEC_FIELDS {
            return Err(Error::validation(format!(
                "Invalid specification: more than {MAX_SPEC_FIELDS} fields"
            )));
        }
        let mut items = Vec::with_capacity(map.len());
        for (name, options) in map.iter() {
            let options = match options {
                Value::Map(o) => o,
                // `{'field': False}` / `None` read like `{}` in the JS client's payloads
                Value::False => {
                    items.push((name.clone(), SpecItem::default()));
                    continue;
                }
                other => {
                    return Err(Error::validation(format!(
                        "Invalid specification of field '{name}': {other} is not a dictionary"
                    )));
                }
            };
            let mut item = SpecItem::default();
            for (key, v) in options.iter() {
                match key.as_str() {
                    "fields" => item.fields = Some(Self::from_value_at(v, depth + 1, count)?),
                    "context" => item.context = Some(context_pairs(v, name)?.into()),
                    "limit" => {
                        item.limit = match v {
                            Value::False => None,
                            Value::Int(n) if *n >= 0 => Some(*n as u64),
                            other => {
                                return Err(Error::validation(format!(
                                    "Invalid limit {other} for field '{name}'"
                                )));
                            }
                        }
                    }
                    "order" => {
                        item.order = match v {
                            Value::False => None,
                            Value::Str(s) if s.trim().is_empty() => None,
                            Value::Str(s) => Some(s.to_string()),
                            other => {
                                return Err(Error::validation(format!(
                                    "Invalid order {other} for field '{name}'"
                                )));
                            }
                        }
                    }
                    other => {
                        return Err(Error::validation(format!(
                            "Invalid specification of field '{name}': unknown key '{other}'"
                        )));
                    }
                }
            }
            items.push((name.clone(), item));
        }
        Ok(Self::from_items(items))
    }

    /// The specification as a dynamic value.
    pub fn to_value(&self) -> Value {
        Value::Map(Arc::new(
            self.items()
                .iter()
                .map(|(n, i)| (n.clone(), i.to_value()))
                .collect(),
        ))
    }
}

fn context_pairs(value: &Value, field: &str) -> Result<Vec<(String, Value)>> {
    match value {
        Value::Map(map) => Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        Value::False => Ok(Vec::new()),
        other => Err(Error::validation(format!(
            "Invalid context {other} for field '{field}': expected a dictionary"
        ))),
    }
}

// ----- dynamic values to domains -------------------------------------------------------------

fn to_domain_value(value: &Value) -> Result<erp_domain::Value> {
    use erp_domain::Value as D;
    Ok(match value {
        Value::False => D::Bool(false),
        Value::Bool(b) => D::Bool(*b),
        Value::Int(i) => D::Int(*i),
        Value::Float(f) => D::Float(*f),
        Value::Decimal(d) => D::Str(d.to_string()),
        Value::Str(s) => D::Str(s.to_string()),
        Value::Date(d) => D::Date(time_date(*d)?),
        Value::DateTime(dt) => D::Datetime(
            erp_sql::param::primitive_of(*dt)
                .ok_or_else(|| Error::validation(format!("Invalid datetime {dt} in a domain")))?,
        ),
        Value::List(items) => D::List(
            items
                .iter()
                .map(to_domain_value)
                .collect::<Result<Vec<_>>>()?,
        ),
        // a range operand of `overlaps`: `[lower, upper]`, False for an unbounded end
        Value::Range(range) => {
            let ends: Vec<Value> = match range {
                erp_fields::RangeValue::Date(r) => [r.lower(), r.upper()]
                    .into_iter()
                    .map(|end| end.map_or(Value::False, Value::Date))
                    .collect(),
                erp_fields::RangeValue::Datetime(r) => [r.lower(), r.upper()]
                    .into_iter()
                    .map(|end| end.map_or(Value::False, Value::DateTime))
                    .collect(),
            };
            D::List(
                ends.iter()
                    .map(to_domain_value)
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        other => {
            return Err(Error::validation(format!(
                "Invalid value {other} in a domain"
            )));
        }
    })
}

fn time_date(d: Date) -> Result<time::Date> {
    let (y, m, day) = d.ymd();
    let month = u8::try_from(m)
        .ok()
        .and_then(|m| time::Month::try_from(m).ok());
    month
        .and_then(|month| time::Date::from_calendar_date(y, month, day as u8).ok())
        .ok_or_else(|| Error::validation(format!("Invalid date {d} in a domain")))
}

/// Errors are `Validation`.
pub fn domain_from_value(value: &Value) -> Result<Domain> {
    let v = to_domain_value(value)?;
    Ok(Domain::from_value(&v)?)
}

/// A domain built by the server itself in list syntax (extra domains of groups): `any` values are
/// parsed recursively.
pub(crate) fn internal_domain_from_value(value: &Value) -> Result<Domain> {
    let Value::List(items) = value else {
        return domain_from_value(value);
    };
    let items = items
        .iter()
        .map(to_domain_value)
        .collect::<Result<Vec<_>>>()?;
    Ok(Domain::from_list_internal(&items)?)
}

// ----- keyword arguments ---------------------------------------------------------------------

/// Keyword arguments of a web method called with dynamic values (JSON-2 body).
pub(crate) struct Kwargs<'a> {
    method: &'static str,
    map: &'a ValueMap,
    seen: Vec<&'static str>,
}

impl<'a> Kwargs<'a> {
    pub(crate) fn new(method: &'static str, value: &'a Value) -> Result<Self> {
        match value {
            Value::Map(map) => Ok(Self {
                method,
                map,
                seen: Vec::new(),
            }),
            other => Err(Error::validation(format!(
                "{method}() expects keyword arguments, got {other}"
            ))),
        }
    }

    /// The `Validation` error for argument `key` holding a value that is not `what`.
    pub(crate) fn error(&self, key: &str, what: &str, value: &Value) -> Error {
        Error::validation(format!(
            "{}(): invalid value {value} for argument '{key}' ({what})",
            self.method
        ))
    }

    /// A value, `None` when absent.
    pub(crate) fn get(&mut self, key: &'static str) -> Option<&'a Value> {
        self.seen.push(key);
        self.map.get(key)
    }

    /// A required value.
    pub(crate) fn req(&mut self, key: &'static str) -> Result<&'a Value> {
        let method = self.method;
        self.get(key).ok_or_else(|| {
            Error::validation(format!("{method}() missing 1 required argument: '{key}'"))
        })
    }

    pub(crate) fn spec(&mut self, key: &'static str) -> Result<Spec> {
        Spec::from_value(self.req(key)?)
    }

    pub(crate) fn spec_opt(&mut self, key: &'static str) -> Result<Option<Spec>> {
        match self.get(key) {
            None | Some(Value::False) => Ok(None),
            Some(v) => Spec::from_value(v).map(Some),
        }
    }

    pub(crate) fn domain(&mut self, key: &'static str) -> Result<Domain> {
        match self.get(key) {
            None => Ok(Domain::TRUE),
            Some(v) => domain_from_value(v),
        }
    }

    pub(crate) fn domain_opt(&mut self, key: &'static str) -> Result<Option<Domain>> {
        match self.get(key) {
            None | Some(Value::False) => Ok(None),
            Some(v) => domain_from_value(v).map(Some),
        }
    }

    pub(crate) fn str(&mut self, key: &'static str) -> Result<String> {
        match self.req(key)? {
            Value::Str(s) => Ok(s.to_string()),
            other => Err(self.error(key, "a string", other)),
        }
    }

    pub(crate) fn value_opt(&mut self, key: &'static str) -> Option<Value> {
        self.get(key).cloned()
    }

    pub(crate) fn u64_or(&mut self, key: &'static str, default: u64) -> Result<u64> {
        match self.get(key) {
            None | Some(Value::False) => Ok(default),
            Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
            Some(other) => Err(self.error(key, "a non-negative integer", other)),
        }
    }

    /// A non-negative integer, `default` when absent; `null` is refused (the JSON-2 binders'
    /// `Defaulted<u64>`).
    pub(crate) fn u64_present_or(&mut self, key: &'static str, default: u64) -> Result<u64> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
            Some(other) => Err(self.error(key, "a non-negative integer", other)),
        }
    }

    /// `Some(default)` when absent, `None` for `null`, `Some(n)` for a non-negative integer (the
    /// JSON-2 binders' `Nullable<u64>`).
    pub(crate) fn u64_nullable_or(
        &mut self,
        key: &'static str,
        default: u64,
    ) -> Result<Option<u64>> {
        match self.get(key) {
            None => Ok(Some(default)),
            Some(Value::False) => Ok(None),
            Some(Value::Int(n)) if *n >= 0 => Ok(Some(*n as u64)),
            Some(other) => Err(self.error(key, "a non-negative integer or null", other)),
        }
    }

    /// `Some(default)` (borrowed) when absent, `None` for `null`, `Some(s)` for a string (the JSON-2
    /// binders' `Nullable<String>`).
    pub(crate) fn str_nullable_or(
        &mut self,
        key: &'static str,
        default: &'static str,
    ) -> Result<Option<Cow<'static, str>>> {
        match self.get(key) {
            None => Ok(Some(Cow::Borrowed(default))),
            Some(Value::False) => Ok(None),
            Some(Value::Str(s)) => Ok(Some(Cow::Owned(s.to_string()))),
            Some(other) => Err(self.error(key, "a string or null", other)),
        }
    }

    /// Search-panel limits use absent, `null`, `false`, or zero for no limit.
    pub(crate) fn search_panel_limit(&mut self, key: &'static str) -> Result<Option<u64>> {
        match self.get(key) {
            None | Some(Value::False | Value::Bool(false) | Value::Int(0)) => Ok(None),
            Some(Value::Int(n)) if *n > 0 => Ok(Some(*n as u64)),
            Some(other) => Err(self.error(key, "a non-negative integer, false or null", other)),
        }
    }

    pub(crate) fn u64_opt(&mut self, key: &'static str) -> Result<Option<u64>> {
        match self.get(key) {
            None | Some(Value::False) => Ok(None),
            Some(Value::Int(n)) if *n >= 0 => Ok(Some(*n as u64)),
            Some(other) => Err(self.error(key, "a non-negative integer or null", other)),
        }
    }

    pub(crate) fn str_opt(&mut self, key: &'static str) -> Result<Option<String>> {
        match self.get(key) {
            None | Some(Value::False) => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s.to_string())),
            Some(other) => Err(self.error(key, "a string", other)),
        }
    }

    /// A required string (`null` refused).
    pub(crate) fn str_req(&mut self, key: &'static str) -> Result<String> {
        match self.req(key)? {
            Value::Str(s) => Ok(s.to_string()),
            other => Err(self.error(key, "a string", other)),
        }
    }

    /// An integer, `default` when absent; `null` is refused (no `None` default in the signature).
    pub(crate) fn i64_present_or(&mut self, key: &'static str, default: i64) -> Result<i64> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Int(n)) => Ok(*n),
            Some(other) => Err(self.error(key, "an integer", other)),
        }
    }

    /// A domain operator, `default` when absent; `null` and non-strings are refused (the same rule
    /// as the JSON-2 binder's `Defaulted<OperatorArg>` in `rpc/core.rs`).
    pub(crate) fn operator_or(
        &mut self,
        key: &'static str,
        default: erp_domain::Operator,
    ) -> Result<erp_domain::Operator> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Str(op)) => erp_domain::Operator::parse(op)
                .ok_or_else(|| Error::validation(format!("Invalid operator: '{op}'"))),
            Some(other) => Err(self.error(key, "a domain operator", other)),
        }
    }

    pub(crate) fn str_or(&mut self, key: &'static str, default: &str) -> Result<String> {
        Ok(self.str_opt(key)?.unwrap_or_else(|| default.to_owned()))
    }

    pub(crate) fn bool_or(&mut self, key: &'static str, default: bool) -> Result<bool> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::False) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(self.error(key, "a boolean", other)),
        }
    }

    pub(crate) fn strings(&mut self, key: &'static str) -> Result<Vec<String>> {
        match self.get(key) {
            None | Some(Value::False) => Ok(Vec::new()),
            Some(Value::List(items)) => items
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(s.to_string()),
                    other => Err(self.error(key, "a list of strings", other)),
                })
                .collect(),
            Some(other) => Err(self.error(key, "a list of strings", other)),
        }
    }

    pub(crate) fn list(&mut self, key: &'static str) -> Result<Vec<Value>> {
        match self.get(key) {
            None | Some(Value::False) => Ok(Vec::new()),
            Some(Value::List(items)) => Ok(items.to_vec()),
            Some(other) => Err(self.error(key, "a list", other)),
        }
    }

    pub(crate) fn map(&mut self, key: &'static str) -> Result<ValueMap> {
        match self.req(key)? {
            Value::Map(map) => Ok(map.as_ref().clone()),
            other => Err(self.error(key, "a dictionary", other)),
        }
    }

    /// Refuse arguments the method does not take (Python's `TypeError`, 422 on JSON-2).
    pub(crate) fn finish(self) -> Result<()> {
        match self.map.keys().find(|k| !self.seen.contains(&k.as_str())) {
            Some(key) => Err(Error::validation(format!(
                "{}() got an unexpected keyword argument '{key}'",
                self.method
            ))),
            None => Ok(()),
        }
    }
}

/// `str(value)` of a Python value, as `read_progress_bar` keys its result.
pub(crate) fn py_str(value: &Value) -> String {
    match value {
        Value::False => "False".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Decimal(d) => d.to_string(),
        Value::Str(s) => s.to_string(),
        Value::Date(d) => d.to_string(),
        Value::DateTime(d) => d.to_string(),
        other => other.to_string(),
    }
}
