//! The bridge to `erp-expr`: evaluation contexts for data files, the `ref()`
//! resolver, and the conversion of expression values into field values.
//!
//! `ref()` is synchronous inside the evaluator, so the loader looks the external ids up first
//! (design §3.5): [`literal_refs`] reads the `ref('<literal>')` calls from the parsed expression,
//! the session resolves those names with one lookup, and the evaluator's resolver answers from
//! that map. A name the map does not have (a computed argument) is recorded; [`evaluate`] reports
//! it so the caller can look it up and try again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use erp_expr::{EvalContext, Expr, ExprError, ExprValue, Literal};
use erp_fields::{Date, DateTime, Value, ValueMap};
use erp_registry::Registry;
use time::PrimitiveDateTime;

/// What the resolver knows about an external id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Known {
    /// The model and id of the record.
    Record { model: String, id: i64 },
    /// Looked up, no row.
    Absent,
}

impl Known {
    pub(crate) fn id(&self) -> Option<i64> {
        match self {
            Known::Record { id, .. } => Some(*id),
            Known::Absent => None,
        }
    }
}

pub(crate) fn literal_refs(ast: &Expr, module: &str, out: &mut Vec<String>) {
    scan_refs(ast, module, out, &mut false);
}

/// [`literal_refs`] that also reports, in `dynamic`, whether `ast` calls `ref()` with an argument
/// that is not a string literal (its name is only known when the expression runs, so the caller
/// cannot look it up ahead of time).
pub(crate) fn scan_refs(ast: &Expr, module: &str, out: &mut Vec<String>, dynamic: &mut bool) {
    match ast {
        Expr::Call {
            func,
            args,
            keywords,
        } => {
            let is_ref = match func.as_ref() {
                Expr::Name(name) => name == "ref",
                Expr::Attribute { attr, .. } => attr == "ref",
                _ => false,
            };
            if is_ref {
                let first = args.first().or_else(|| {
                    keywords
                        .iter()
                        .find(|(k, _)| k.as_deref() == Some("xml_id"))
                        .map(|(_, v)| v)
                });
                match first {
                    Some(Expr::Literal(Literal::Str(s))) => out.push(qualify(s, module)),
                    // a computed name: only known when the expression runs
                    _ => *dynamic = true,
                }
            }
            scan_refs(func, module, out, dynamic);
            for a in args {
                scan_refs(a, module, out, dynamic);
            }
            for (_, v) in keywords {
                scan_refs(v, module, out, dynamic);
            }
        }
        Expr::Literal(_) | Expr::Name(_) => {}
        Expr::Unary { operand, .. } => scan_refs(operand, module, out, dynamic),
        Expr::Binary { left, right, .. } => {
            scan_refs(left, module, out, dynamic);
            scan_refs(right, module, out, dynamic);
        }
        Expr::BoolOp { values, .. } => values
            .iter()
            .for_each(|v| scan_refs(v, module, out, dynamic)),
        Expr::Compare {
            left, comparators, ..
        } => {
            scan_refs(left, module, out, dynamic);
            comparators
                .iter()
                .for_each(|v| scan_refs(v, module, out, dynamic));
        }
        Expr::IfExp { test, body, orelse } => {
            scan_refs(test, module, out, dynamic);
            scan_refs(body, module, out, dynamic);
            scan_refs(orelse, module, out, dynamic);
        }
        Expr::Attribute { value, .. } => scan_refs(value, module, out, dynamic),
        Expr::Subscript { value, slice } => {
            scan_refs(value, module, out, dynamic);
            if let erp_expr::SubscriptKind::Index(i) = slice {
                scan_refs(i, module, out, dynamic);
            } else if let erp_expr::SubscriptKind::Slice { lower, upper, step } = slice {
                for part in [lower, upper, step].into_iter().flatten() {
                    scan_refs(part, module, out, dynamic);
                }
            }
        }
        Expr::List(items) | Expr::Tuple(items) | Expr::JoinedStr(items) => {
            items
                .iter()
                .for_each(|v| scan_refs(v, module, out, dynamic));
        }
        Expr::Dict(pairs) => {
            for (k, v) in pairs {
                scan_refs(k, module, out, dynamic);
                scan_refs(v, module, out, dynamic);
            }
        }
        Expr::Walrus { value, .. } => scan_refs(value, module, out, dynamic),
        Expr::FormattedValue { value, .. } => scan_refs(value, module, out, dynamic),
        Expr::ListComp { elt, generators } | Expr::GeneratorExp { elt, generators } => {
            scan_refs(elt, module, out, dynamic);
            for g in generators {
                literal_refs_comprehension(g, module, out, dynamic);
            }
        }
        Expr::Starred(inner) => scan_refs(inner, module, out, dynamic),
        // `Expr` is non-exhaustive: a construct added later is walked when this is extended; an
        // unseen `ref()` inside it is found by the retry in `evaluate` (its name is recorded).
        _ => {}
    }
}

fn literal_refs_comprehension(
    g: &erp_expr::Comprehension,
    module: &str,
    out: &mut Vec<String>,
    dynamic: &mut bool,
) {
    scan_refs(&g.iter, module, out, dynamic);
    for cond in &g.ifs {
        scan_refs(cond, module, out, dynamic);
    }
}

/// `module.name`, or `name` alone qualified with `module`.
pub(crate) fn qualify(name: &str, module: &str) -> String {
    if name.contains('.') {
        name.to_owned()
    } else {
        format!("{module}.{name}")
    }
}

/// Search requirement collected during evaluation when not found in cache.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SearchRequirement {
    pub(crate) target_model: String,
    pub(crate) domain_val: ExprValue,
    pub(crate) offset: u64,
    pub(crate) limit: Option<u64>,
    pub(crate) order: Option<String>,
    pub(crate) context_val: ExprValue,
    pub(crate) sudo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SearchKey {
    pub(crate) model: String,
    pub(crate) domain_repr: String,
    pub(crate) offset: u64,
    pub(crate) limit: Option<u64>,
    pub(crate) order: Option<String>,
    pub(crate) context_repr: String,
    pub(crate) sudo: bool,
}

impl SearchRequirement {
    pub(crate) fn key(&self) -> SearchKey {
        SearchKey {
            model: self.target_model.clone(),
            domain_repr: format!("{:?}", self.domain_val),
            offset: self.offset,
            limit: self.limit,
            order: self.order.clone(),
            context_repr: format!("{:?}", self.context_val),
            sudo: self.sudo,
        }
    }
}

pub(crate) type SearchRequirementStore = Arc<Mutex<Vec<SearchRequirement>>>;

/// How one evaluation ended when it did not produce a value.
pub(crate) struct EvalFailure {
    /// The error of the evaluator.
    pub(crate) error: ExprError,
    /// Names the resolver was asked for and did not know.
    pub(crate) unknown: Vec<String>,
    /// Record fields needed: (model, id, field_name).
    pub(crate) missing_fields: Vec<(String, i64, String)>,
    /// Searches needed.
    pub(crate) missing_searches: Vec<SearchRequirement>,
    /// Security group XML IDs needed: qualified XML ID.
    pub(crate) missing_groups: Vec<String>,
    /// Group checks of an explicit user needed: (user id, qualified group XML ID).
    pub(crate) missing_user_groups: Vec<(i64, String)>,
}

/// What one evaluation may answer from without the database: the external ids (`ref()`), the
/// record fields and searches fetched by earlier rounds, and the registry the field names are
/// checked against.
pub(crate) struct Lookups<'a> {
    pub(crate) known: &'a HashMap<String, Known>,
    pub(crate) fields: &'a HashMap<(String, i64, String), ExprValue>,
    pub(crate) searches: &'a HashMap<SearchKey, Vec<i64>>,
    pub(crate) groups: &'a HashMap<String, bool>,
    pub(crate) user_groups: &'a HashMap<(i64, String), bool>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) user_id: i64,
    pub(crate) company_id: Option<i64>,
    pub(crate) company_ids: Vec<i64>,
    pub(crate) context: ExprValue,
}

/// Converts an ORM Context to an ExprValue::Dict.
pub(crate) fn context_to_expr(context: &erp_orm::Context) -> Result<ExprValue, String> {
    let map = context.to_map();
    let mut pairs = Vec::with_capacity(map.len());
    for (k, v) in map {
        pairs.push((ExprValue::str(k.as_str()), value_to_expr(&v)?));
    }
    ExprValue::dict(pairs).map_err(|e| format!("context dictionary cannot be built: {e}"))
}

/// Converts an ExprValue::Dict back to an ORM Context.
pub(crate) fn expr_to_context(value: &ExprValue) -> Result<erp_orm::Context, String> {
    match value {
        ExprValue::Dict(map) => {
            let mut pairs = Vec::with_capacity(map.len());
            for (k, v) in map.iter() {
                let ExprValue::Str(key) = k else {
                    return Err(format!(
                        "context key must be a string, got {}",
                        k.type_name()
                    ));
                };
                pairs.push((key.clone(), to_value(v)?));
            }
            Ok(erp_orm::Context::from_pairs(pairs))
        }
        _ => Err("context must be a dict".to_owned()),
    }
}

/// Evaluate a parsed expression.
pub(crate) fn evaluate(
    ast: &Expr,
    module: &str,
    model: Option<&str>,
    lookups: &Lookups<'_>,
    now: PrimitiveDateTime,
    version: &str,
) -> Result<ExprValue, Box<EvalFailure>> {
    let unknown: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let missing_fields: Arc<Mutex<Vec<(String, i64, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let missing_searches: SearchRequirementStore = Arc::new(Mutex::new(Vec::new()));
    let missing_groups: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let seen = Arc::clone(&unknown);
    let mf = Arc::clone(&missing_fields);
    let ms = Arc::clone(&missing_searches);
    let mg = Arc::clone(&missing_groups);
    let missing_user_groups: Arc<Mutex<Vec<(i64, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mug = Arc::clone(&missing_user_groups);

    let module_name = module.to_owned();
    let known_map = lookups.known.clone();
    let fields_map = lookups.fields.clone();
    let searches_map = lookups.searches.clone();
    let groups_map = lookups.groups.clone();
    let user_groups_map = lookups.user_groups.clone();
    let registry = Arc::clone(&lookups.registry);

    let seen_ref = Arc::clone(&seen);
    let module_ref = module_name.clone();
    let known_ref = known_map.clone();
    let ref_resolver = move |name: &str, raise: bool| {
        let qualified = qualify(name, &module_ref);
        match known_ref.get(&qualified) {
            Some(known) => match known.id() {
                Some(id) => Ok(ExprValue::Int(id)),
                None if raise => Err(ExprError::value_error(format!(
                    "External ID not found in the system: {qualified}"
                ))),
                None => Ok(ExprValue::Bool(false)),
            },
            None => {
                seen_ref
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(qualified.clone());
                Err(ExprError::value_error(format!(
                    "External ID {qualified} was not resolved before the evaluation"
                )))
            }
        }
    };

    let seen_xid = Arc::clone(&seen);
    let module_xid = module_name.clone();
    let known_xid = known_map.clone();
    let xid_resolver = move |name: &str| {
        let qualified = qualify(name, &module_xid);
        match known_xid.get(&qualified) {
            Some(Known::Record { model, id }) => Some((model.clone(), *id)),
            Some(Known::Absent) => None,
            None => {
                seen_xid
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(qualified);
                None
            }
        }
    };

    let mf_clone = Arc::clone(&mf);
    let record_resolver = move |model: &str, id: i64, field: &str| {
        if let Some(model_def) = registry.model(model)
            && model_def.field(field).is_none()
        {
            return Err(ExprError::attribute_error(model, field));
        }
        let key = (model.to_string(), id, field.to_string());
        if let Some(val) = fields_map.get(&key) {
            Ok(val.clone())
        } else {
            mf_clone
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(key);
            Err(ExprError::value_error(format!(
                "Field '{field}' of {model}({id}) was not resolved before evaluation"
            )))
        }
    };

    let ms_clone = Arc::clone(&ms);
    let search_resolver = move |query: &erp_expr::SearchQuery<'_>| {
        let req = SearchRequirement {
            target_model: query.model.to_string(),
            domain_val: query.domain.clone(),
            offset: query.offset,
            limit: query.limit,
            order: query.order.map(|s| s.to_string()),
            context_val: query.context.clone(),
            sudo: query.sudo,
        };
        let key = req.key();
        if let Some(ids) = searches_map.get(&key) {
            Ok(ids.clone())
        } else {
            ms_clone
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(req);
            Err(ExprError::value_error(format!(
                "Search on {} was not resolved before evaluation",
                query.model
            )))
        }
    };

    let mg_clone = Arc::clone(&mg);
    let module_group = module_name.clone();
    let group_checker = move |group_xmlid: &str| {
        let qualified = qualify(group_xmlid, &module_group);
        if let Some(&in_group) = groups_map.get(&qualified) {
            Ok(in_group)
        } else {
            mg_clone
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(qualified.clone());
            Err(ExprError::value_error(format!(
                "Group check '{qualified}' was not resolved before evaluation"
            )))
        }
    };

    let module_user_group = module_name.clone();
    let user_group_checker = move |uid: i64, group_xmlid: &str| {
        let qualified = qualify(group_xmlid, &module_user_group);
        if let Some(&in_group) = user_groups_map.get(&(uid, qualified.clone())) {
            Ok(in_group)
        } else {
            mug.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((uid, qualified.clone()));
            Err(ExprError::value_error(format!(
                "Group check '{qualified}' of res.users({uid}) was not resolved before evaluation"
            )))
        }
    };

    let mut hooks = erp_expr::HostHooks::new()
        .with_record_resolver(record_resolver)
        .with_search_resolver(search_resolver)
        .with_ref_resolver(ref_resolver)
        .with_xid_model_resolver(xid_resolver)
        .with_group_checker(group_checker)
        .with_user_group_checker(user_group_checker)
        .with_user_id(lookups.user_id)
        .with_company_ids(lookups.company_ids.clone())
        .with_context(lookups.context.clone())
        .with_sudo(true);

    if let Some(cid) = lookups.company_id {
        hooks = hooks.with_company_id(cid);
    }

    let mut ctx = EvalContext::standard()
        .with_clock(move || now)
        .with_var("version", ExprValue::str(version))
        .with_host_hooks(Arc::new(hooks));

    if let Some(m) = model {
        ctx = ctx.with_model(m);
    }

    match erp_expr::eval_ast(ast, &ctx) {
        Ok(value) => Ok(value),
        Err(error) => Err(Box::new(EvalFailure {
            error,
            unknown: std::mem::take(&mut *unknown.lock().unwrap_or_else(PoisonError::into_inner)),
            missing_fields: std::mem::take(
                &mut *missing_fields
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            ),
            missing_searches: std::mem::take(
                &mut *missing_searches
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            ),
            missing_groups: std::mem::take(
                &mut *missing_groups
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            ),
            missing_user_groups: std::mem::take(
                &mut *missing_user_groups
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            ),
        })),
    }
}

/// `datetime` of the host clock as the evaluator's naive datetime.
pub(crate) fn clock_now() -> Result<PrimitiveDateTime, String> {
    let micros = DateTime::now().micros();
    let nanos = i128::from(micros)
        .checked_mul(1000)
        .ok_or_else(|| "the clock is out of range".to_owned())?;
    let odt = time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|e| format!("the clock is out of range: {e}"))?;
    Ok(PrimitiveDateTime::new(odt.date(), odt.time()))
}

/// The name of a [`Value`] variant, for error messages.
pub(crate) fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::False => "False",
        Value::Bool(_) => "a bool",
        Value::Int(_) => "an int",
        Value::Float(_) => "a float",
        Value::Decimal(_) => "a decimal",
        Value::Str(_) => "a str",
        Value::Bytes(_) => "bytes",
        Value::Date(_) => "a date",
        Value::DateTime(_) => "a datetime",
        Value::Json(_) => "a JSON value",
        Value::List(_) => "a list",
        Value::Map(_) => "a dict",
        Value::Range(_) => "a date range",
        Value::Row(_) => "a result row",
        Value::Rows(_) => "result rows",
        // `Value` is non-exhaustive: a kind added later is named generically until listed here
        _ => "a value of an unlisted kind",
    }
}

/// Converts an ORM field [`Value`] into an [`ExprValue`]. `Err` names what has no expression
/// form (a JSON value, a range, an out-of-range date); nothing is replaced by `None`.
pub(crate) fn value_to_expr(value: &Value) -> Result<ExprValue, String> {
    Ok(match value {
        Value::False => ExprValue::Bool(false),
        Value::Bool(b) => ExprValue::Bool(*b),
        Value::Int(i) => ExprValue::Int(*i),
        Value::Float(f) => ExprValue::Float(*f),
        Value::Str(s) => ExprValue::str(s.as_ref()),
        Value::Decimal(d) => {
            let text = d.to_string();
            ExprValue::Float(
                text.parse::<f64>()
                    .map_err(|e| format!("a decimal {text} that is not a float ({e})"))?,
            )
        }
        Value::Bytes(b) => ExprValue::Bytes(b.to_vec()),
        Value::Date(d) => {
            let (y, m, day) = d.ymd();
            let month = u8::try_from(m)
                .ok()
                .and_then(|m| time::Month::try_from(m).ok());
            let date = month.and_then(|month| {
                u8::try_from(day)
                    .ok()
                    .and_then(|day| time::Date::from_calendar_date(y, month, day).ok())
            });
            ExprValue::Date(date.ok_or_else(|| format!("the date {y}-{m}-{day} out of range"))?)
        }
        Value::DateTime(dt) => {
            let (y, m, day) = dt.date().ymd();
            let (hour, minute, sec, micro) = dt.time();
            let month = u8::try_from(m)
                .ok()
                .and_then(|m| time::Month::try_from(m).ok());
            let date = month.and_then(|month| {
                u8::try_from(day)
                    .ok()
                    .and_then(|day| time::Date::from_calendar_date(y, month, day).ok())
            });
            let clock = match (u8::try_from(hour), u8::try_from(minute), u8::try_from(sec)) {
                (Ok(h), Ok(mi), Ok(s)) => time::Time::from_hms_micro(h, mi, s, micro).ok(),
                _ => None,
            };
            match (date, clock) {
                (Some(date), Some(clock)) => {
                    ExprValue::DateTime(PrimitiveDateTime::new(date, clock))
                }
                _ => {
                    return Err(format!(
                        "the datetime {y}-{m}-{day} {hour}:{minute}:{sec}.{micro} out of range"
                    ));
                }
            }
        }
        Value::List(l) => {
            ExprValue::list(l.iter().map(value_to_expr).collect::<Result<Vec<_>, _>>()?)
        }
        Value::Map(m) => {
            let mut pairs = Vec::with_capacity(m.len());
            for (k, v) in m.iter() {
                pairs.push((ExprValue::str(k.as_str()), value_to_expr(v)?));
            }
            // string keys are always hashable; a failure here is reported, never replaced
            ExprValue::dict(pairs).map_err(|e| format!("a dict that cannot be built: {e}"))?
        }
        // JSON, ranges, result rows and kinds added later have no expression form
        other => return Err(value_kind(other).to_owned()),
    })
}

/// An expression value as a field value. `Err` names what cannot be stored (`a timedelta`).
pub(crate) fn to_value(value: &ExprValue) -> Result<Value, String> {
    Ok(match value {
        ExprValue::None => Value::False,
        ExprValue::Bool(b) => Value::Bool(*b),
        ExprValue::Int(i) => Value::Int(*i),
        ExprValue::Float(f) => Value::Float(*f),
        ExprValue::Str(s) => Value::str(s),
        ExprValue::Bytes(b) => Value::Bytes(Arc::from(b.as_slice())),
        ExprValue::Date(d) => Value::Date(date_of(*d)?),
        ExprValue::DateTime(dt) => Value::DateTime(datetime_of(*dt)?),
        ExprValue::List(items) | ExprValue::Tuple(items) => Value::List(
            items
                .iter()
                .map(to_value)
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        ),
        ExprValue::Dict(map) => {
            let mut out = ValueMap::new();
            for (k, v) in map.iter() {
                let ExprValue::Str(key) = k else {
                    return Err(format!("a dict with a {} key", k.type_name()));
                };
                out.insert(key.clone(), to_value(v)?);
            }
            Value::Map(Arc::new(out))
        }
        ExprValue::Object(o) => {
            if o.is_record() {
                if let Ok(id) = o.as_int() {
                    Value::Int(id)
                } else if !o.is_truthy() {
                    Value::False
                } else {
                    return Err(format!("a {}", o.type_name()));
                }
            } else {
                return Err(format!("a {}", o.type_name()));
            }
        }
        other => return Err(format!("a {}", other.type_name())),
    })
}

fn date_of(d: time::Date) -> Result<Date, String> {
    let month: u8 = d.month().into();
    Date::from_ymd(d.year(), u32::from(month), u32::from(d.day()))
        .ok_or_else(|| format!("the date {d} is out of range"))
}

fn datetime_of(dt: PrimitiveDateTime) -> Result<DateTime, String> {
    let date = date_of(dt.date())?;
    let t = dt.time();
    DateTime::from_parts(
        date,
        u32::from(t.hour()),
        u32::from(t.minute()),
        u32::from(t.second()),
        t.microsecond(),
    )
    .ok_or_else(|| format!("the datetime {dt} is out of range"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;

    /// A value with no expression form is an error naming its kind, never `None` (review D2).
    #[test]
    fn values_without_an_expression_form_are_errors() {
        let json = Value::Json(Arc::new(erp_fields::serde_json::json!({"a": 1})));
        assert_eq!(value_to_expr(&json), Err("a JSON value".to_owned()));
        let nested = Value::list([Value::Int(1), json]);
        assert_eq!(value_to_expr(&nested), Err("a JSON value".to_owned()));
        let mut map = ValueMap::new();
        map.insert("k".to_owned(), Value::Int(2));
        assert_eq!(
            value_to_expr(&Value::Map(Arc::new(map))),
            Ok(ExprValue::dict_str([("k", ExprValue::Int(2))]))
        );
    }

    /// A NUMERIC column's decimal reads as a Python `float`, never a `str` (so `price * 2` is
    /// arithmetic, not string repetition).
    #[test]
    fn decimals_read_as_floats() {
        let d = Value::Decimal("12.50".parse::<erp_fields::Decimal>().unwrap());
        assert_eq!(value_to_expr(&d), Ok(ExprValue::Float(12.5)));
    }

    /// Records nested in a dict, a list and a tuple convert to their ids; an empty recordset to
    /// `False` ('s conversion, unit level).
    #[test]
    fn records_inside_collections_convert_to_ids() {
        let rec = ExprValue::Object(Arc::new(erp_expr::RecordProxy::new("test.cat", 5).unwrap()));
        let empty = ExprValue::Object(Arc::new(
            erp_expr::RecordsetProxy::new("test.cat", Vec::new()).unwrap(),
        ));
        let value = ExprValue::dict_str([
            ("one", rec.clone()),
            ("none", empty),
            ("many", ExprValue::list(vec![ExprValue::tuple(vec![rec])])),
        ]);
        let mut expected = ValueMap::new();
        expected.insert("one".to_owned(), Value::Int(5));
        expected.insert("none".to_owned(), Value::False);
        expected.insert(
            "many".to_owned(),
            Value::list([Value::list([Value::Int(5)])]),
        );
        assert_eq!(to_value(&value), Ok(Value::Map(Arc::new(expected))));
    }
}
