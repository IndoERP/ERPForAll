//! `formatted_read_group`, `formatted_read_grouping_sets`, `web_read_group` and
//! `read_progress_bar` (GRP §4.10–4.11).
//!
//! Group values are formatted for the client (`(id, display_name)` with display names computed
//! once per column as superuser, date buckets as `(start, label)` with UTC bounds for
//! datetimes) and each group gets the domain selecting its records (`__extra_domain`).

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{Date, DateTime, Error, FieldType, RecId, Result, Value, ValueMap};
use erp_registry::{FieldId, ModelSpec};
use erp_sql::param::Param;
use erp_sql::quote;
use rustc_hash::FxHashMap;

use super::read::{Row, web_read_rows};
use super::spec::{Spec, internal_domain_from_value, py_str};
use super::{FormattedGroups, FormattedReadGroup, FormattedReadGroupingSets, WebReadGroup};
use crate::Transaction;
use crate::read_group::spec::{Granularity, parse_spec_or_err};
use crate::read_group::{GroupValue, Groups, ReadGroup, ReadGroupingSets, RecordsAs};
use crate::records::Records;
use crate::search::{compile_for_at, flush_read_inputs, merge_joins, order_for};

// ----- extra domains -------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum FDom {
    Cond(String, &'static str, DVal),
    And(Vec<FDom>),
    Or(Vec<FDom>),
}

#[derive(Debug, Clone)]
enum DVal {
    Value(Value),
    Sub(Box<FDom>),
}

impl FDom {
    fn cond(field: impl Into<String>, op: &'static str, value: Value) -> Self {
        Self::Cond(field.into(), op, DVal::Value(value))
    }

    /// `Domain.AND`: nested conjunctions are flattened.
    fn and(items: Vec<FDom>) -> Self {
        let mut out = Vec::new();
        for item in items {
            match item {
                Self::And(children) => out.extend(children),
                other => out.push(other),
            }
        }
        Self::And(out)
    }

    fn write(&self, out: &mut Vec<Value>) {
        match self {
            Self::Cond(field, op, value) => {
                let value = match value {
                    DVal::Value(v) => v.clone(),
                    DVal::Sub(sub) => Value::list(sub.to_list()),
                };
                out.push(Value::list([Value::str(field), Value::str(op), value]));
            }
            Self::And(children) | Self::Or(children) => {
                let op = if matches!(self, Self::And(_)) {
                    "&"
                } else {
                    "|"
                };
                for _ in 1..children.len() {
                    out.push(Value::str(op));
                }
                for child in children {
                    child.write(out);
                }
            }
        }
    }

    fn to_list(&self) -> Vec<Value> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }
}

// ----- labels --------------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn add_months(date: Date, months: i32) -> Option<Date> {
    let (y, m, d) = date.ymd();
    let total = y * 12 + (m as i32 - 1) + months;
    let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let mut day = d;
    loop {
        if let Some(date) = Date::from_ymd(ny, nm, day) {
            return Some(date);
        }
        day = day.checked_sub(1)?;
    }
}

/// The end of a bucket starting at `start` (`READ_GROUP_TIME_GRANULARITY`).
fn bucket_end(start: Date, time_micros: i64, gran: Granularity) -> Option<(Date, i64)> {
    const HOUR: i64 = 3_600_000_000;
    const DAY: i64 = 24 * HOUR;
    Some(match gran {
        Granularity::Hour => {
            let t = time_micros + HOUR;
            if t >= DAY {
                (Date::from_days(start.days() + 1), t - DAY)
            } else {
                (start, t)
            }
        }
        Granularity::Day => (Date::from_days(start.days() + 1), time_micros),
        Granularity::Week => (Date::from_days(start.days() + 7), time_micros),
        Granularity::Month => (add_months(start, 1)?, time_micros),
        Granularity::Quarter => (add_months(start, 3)?, time_micros),
        Granularity::Year => (add_months(start, 12)?, time_micros),
        _ => return None,
    })
}

/// Weekday of a date, 0 = Monday … 6 = Sunday.
fn weekday(date: Date) -> i32 {
    // 1970-01-01 was a Thursday
    (date.days() + 3).rem_euclid(7)
}

/// `date_utils.weeknumber` for a locale whose weeks start on `first` (0 = Monday) and whose first
/// week contains January 1st (`en_US`: Sunday, one day).
fn week_number(date: Date, first: i32) -> (i32, i32) {
    let back = |d: Date| Date::from_days(d.days() - (weekday(d) - first).rem_euclid(7));
    let (y, _, _) = date.ymd();
    if let Some(next) = Date::from_ymd(y + 1, 1, 1)
        && date >= back(next)
    {
        return (y + 1, 1);
    }
    let Some(jan1) = Date::from_ymd(y, 1, 1) else {
        return (y, 1);
    };
    let first_day = back(jan1);
    (y, (date.days() - first_day.days()).div_euclid(7) + 1)
}

/// The `en_US` label of a bucket (`READ_GROUP_DISPLAY_FORMAT`, babel patterns).
fn bucket_label(date: Date, hour: u32, gran: Granularity) -> String {
    let (y, m, d) = date.ymd();
    let month = MONTHS[(m as usize).saturating_sub(1).min(11)];
    match gran {
        Granularity::Hour => format!("{hour:02}:00 {d:02} {}", &month[..3]),
        Granularity::Day => format!("{d:02} {} {y:04}", &month[..3]),
        Granularity::Week => {
            // Sunday-first weeks (en_US; `res.lang` arrives in M3)
            let (year, week) = week_number(date, 6);
            format!("W{week} {year:04}")
        }
        Granularity::Month => format!("{month} {y:04}"),
        Granularity::Quarter => format!("Q{} {y:04}", (m - 1) / 3 + 1),
        Granularity::Year => format!("{y:04}"),
        _ => String::new(),
    }
}

/// A naive local datetime in `tz` as UTC (Python's `replace(tzinfo=…).astimezone(UTC)`).
fn local_to_utc(local: DateTime, tz: &str) -> Option<DateTime> {
    let zone = jiff::tz::TimeZone::get(tz).ok()?;
    let (y, m, d) = local.date().ymd();
    let (h, mi, s, us) = local.time();
    let civil = jiff::civil::DateTime::new(
        i16::try_from(y).ok()?,
        i8::try_from(m).ok()?,
        i8::try_from(d).ok()?,
        i8::try_from(h).ok()?,
        i8::try_from(mi).ok()?,
        i8::try_from(s).ok()?,
        i32::try_from(us).ok()? * 1000,
    )
    .ok()?;
    let ts = zone
        .to_ambiguous_zoned(civil)
        .compatible()
        .ok()?
        .timestamp();
    Some(DateTime::from_micros(ts.as_microsecond()))
}

// ----- group-by formatters -------------------------------------------------------------------

enum Formatter {
    /// `m2o.path`: the comodel's formatter, wrapped in `any`.
    Path(String, Box<Formatter>),
    Many2many(String),
    /// many2one or `id`.
    Many2one(String),
    Bucket {
        field: String,
        gran: Granularity,
        datetime: bool,
    },
    Part {
        field: String,
        gran: Granularity,
    },
    Plain(String),
}

fn formatter(
    registry: &erp_registry::Registry,
    model: &ModelSpec,
    spec: &str,
) -> Result<Formatter> {
    let parts = parse_spec_or_err(spec, true)?;
    let field = model.field(parts.field).ok_or_else(|| {
        Error::validation(format!(
            "Invalid field '{}' on model '{}'",
            parts.field, model.name
        ))
    })?;
    let name = field.name.to_owned();
    if let Some(path) = parts.path
        && matches!(field.ty, FieldType::Many2one { .. })
    {
        let comodel = crate::relational::comodel_of(registry, field)?;
        let sub = match parts.func {
            Some(func) => format!("{path}:{func}"),
            None => path.to_owned(),
        };
        return Ok(Formatter::Path(
            name,
            Box::new(formatter(registry, comodel, &sub)?),
        ));
    }
    Ok(match &field.ty {
        FieldType::Many2many { .. } => Formatter::Many2many(name),
        FieldType::Many2one { .. } | FieldType::Id => Formatter::Many2one(name),
        FieldType::Date | FieldType::Datetime => {
            let gran = parts
                .func
                .and_then(Granularity::parse)
                .ok_or_else(|| Error::validation(format!("Granularity is missing in '{spec}'")))?;
            if gran.is_bucket() {
                Formatter::Bucket {
                    field: name,
                    gran,
                    datetime: matches!(field.ty, FieldType::Datetime),
                }
            } else {
                Formatter::Part { field: name, gran }
            }
        }
        _ => Formatter::Plain(name),
    })
}

/// Display names of the records of a relational column, as superuser, one query at most.
async fn display_names(
    tx: &mut Transaction,
    values: &[GroupValue],
) -> Result<FxHashMap<RecId, Value>> {
    let mut out = FxHashMap::default();
    let Some(first) = values.iter().find_map(GroupValue::as_records) else {
        return Ok(out);
    };
    let mut ids: Vec<RecId> = Vec::new();
    for v in values {
        if let Some(id) = v.record_id()
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Ok(out);
    }
    let records = first.browse_ids(ids.iter().copied()).engine_sudo();
    let names = tx.display_names_for(&records, first.env()).await?;
    for (id, cell) in ids.into_iter().zip(names) {
        out.insert(id, Value::from_cell(&cell));
    }
    Ok(out)
}

impl Formatter {
    /// The formatted values of a column and the domain of each group.
    fn format<'a>(
        &'a self,
        tx: &'a mut Transaction,
        tz: Option<&'a str>,
        values: &'a [GroupValue],
    ) -> crate::dispatch::BoxFuture<'a, Result<Vec<(Value, FDom)>>> {
        Box::pin(async move {
            Ok(match self {
                Self::Path(field, inner) => inner
                    .format(tx, tz, values)
                    .await?
                    .into_iter()
                    .map(|(value, sub)| {
                        let any = FDom::Cond(field.clone(), "any", DVal::Sub(Box::new(sub)));
                        if value.is_truthy() {
                            (value, any)
                        } else {
                            let none =
                                FDom::Cond(field.clone(), "not any", DVal::Value(Value::list([])));
                            (value, FDom::Or(vec![none, any]))
                        }
                    })
                    .collect(),
                Self::Many2many(field) | Self::Many2one(field) => {
                    let names = display_names(tx, values).await?;
                    let m2m = matches!(self, Self::Many2many(_));
                    values
                        .iter()
                        .map(|v| match v.record_id() {
                            Some(id) => (
                                Value::list([
                                    Value::Int(id.get()),
                                    names.get(&id).cloned().unwrap_or(Value::False),
                                ]),
                                FDom::cond(field.clone(), "=", Value::Int(id.get())),
                            ),
                            None if m2m => (
                                Value::False,
                                FDom::cond(field.clone(), "not any", Value::list([])),
                            ),
                            None => (Value::False, FDom::cond(field.clone(), "=", Value::False)),
                        })
                        .collect()
                }
                Self::Bucket {
                    field,
                    gran,
                    datetime,
                } => values
                    .iter()
                    .map(|v| format_bucket(field, *gran, *datetime, tz, v))
                    .collect::<Result<Vec<_>>>()?,
                Self::Part { field, gran } => values
                    .iter()
                    .map(|v| {
                        let value = v.to_value(RecordsAs::Id);
                        let dom =
                            FDom::cond(format!("{field}.{}", gran.name()), "=", value.clone());
                        (value, dom)
                    })
                    .collect(),
                Self::Plain(field) => values
                    .iter()
                    .map(|v| {
                        let value = v.to_value(RecordsAs::Id);
                        (value.clone(), FDom::cond(field.clone(), "=", value))
                    })
                    .collect(),
            })
        })
    }
}

fn format_bucket(
    field: &str,
    gran: Granularity,
    datetime: bool,
    tz: Option<&str>,
    value: &GroupValue,
) -> Result<(Value, FDom)> {
    let bad = || {
        Error::programming(format!(
            "cannot format the {} bucket {value:?}",
            gran.name()
        ))
    };
    let (start_date, start_time) = match value {
        GroupValue::Date(d) => (*d, 0i64),
        GroupValue::DateTime(dt) => {
            let (h, m, s, us) = dt.time();
            (
                dt.date(),
                ((i64::from(h) * 60 + i64::from(m)) * 60 + i64::from(s)) * 1_000_000
                    + i64::from(us),
            )
        }
        GroupValue::False | GroupValue::Null => {
            return Ok((Value::False, FDom::cond(field, "=", Value::False)));
        }
        other => {
            return Err(Error::programming(format!(
                "a {} bucket of {field} holds {other:?}, not a date",
                gran.name()
            )));
        }
    };
    let (end_date, end_time) = bucket_end(start_date, start_time, gran).ok_or_else(bad)?;
    let hour = (start_time / 3_600_000_000) as u32;
    let label = bucket_label(start_date, hour, gran);
    let (start, end) = if datetime {
        let local = |date: Date, time: i64| {
            date.checked_at_midnight()
                .and_then(|m| m.micros().checked_add(time))
                .map(DateTime::from_micros)
                .ok_or_else(bad)
        };
        let local_start = local(start_date, start_time)?;
        let local_end = local(end_date, end_time)?;
        let (s, e) = match tz {
            Some(tz) => (
                local_to_utc(local_start, tz).ok_or_else(bad)?,
                local_to_utc(local_end, tz).ok_or_else(bad)?,
            ),
            None => (local_start, local_end),
        };
        (s.to_string(), e.to_string())
    } else {
        (start_date.to_string(), end_date.to_string())
    };
    let dom = FDom::and(vec![
        FDom::cond(field, ">=", Value::str(&start)),
        FDom::cond(field, "<", Value::str(&end)),
    ]);
    Ok((Value::list([Value::str(&start), Value::str(&label)]), dom))
}

/// The context timezone when it is a known zone.
fn context_tz(records: &Records) -> Option<String> {
    records
        .env()
        .context()
        .tz()
        .filter(|tz| DateTime::now().offset_seconds(tz).is_some())
        .map(str::to_owned)
}

async fn format_groups(
    tx: &mut Transaction,
    records: &Records,
    groupby: &[String],
    aggregates: &[String],
    groups: &Groups,
) -> Result<Vec<Row>> {
    let registry = Arc::clone(records.env().registry());
    let model = registry.model_by_id(records.model_id());
    let tz = context_tz(records);
    let mut rows: Vec<Row> = (0..groups.len()).map(|_| Row::new()).collect();
    let mut domains: Vec<Vec<FDom>> = (0..groups.len()).map(|_| Vec::new()).collect();
    for (i, spec) in groupby.iter().enumerate() {
        let f = formatter(&registry, model, spec)?;
        let formatted = f.format(tx, tz.as_deref(), groups.column(i)).await?;
        for ((row, doms), (value, dom)) in rows.iter_mut().zip(&mut domains).zip(formatted) {
            row.insert(spec.clone(), value);
            doms.push(dom);
        }
    }
    for (row, doms) in rows.iter_mut().zip(domains) {
        let extra = if doms.is_empty() {
            Vec::new()
        } else {
            FDom::and(doms).to_list()
        };
        row.insert("__extra_domain".to_owned(), Value::list(extra));
    }
    for (j, spec) in aggregates.iter().enumerate() {
        for (row, value) in rows.iter_mut().zip(groups.column(groupby.len() + j)) {
            row.insert(spec.clone(), value.to_value(RecordsAs::Ids));
        }
    }
    Ok(rows)
}

fn web_aggregates(aggregates: &[String]) -> Vec<String> {
    aggregates
        .iter()
        .map(|a| a.replace(":recordset", ":array_agg"))
        .collect()
}

fn refuse_unsupported_context(records: &Records) -> Result<()> {
    let context = records.env().context();
    if context
        .get("fill_temporal")
        .is_some_and(|v| v.is_truthy() || matches!(v, Value::Map(_)))
    {
        return Err(Error::programming(
            "the fill_temporal context key of grouped reads is not supported until M4 (web \
             client views)",
        ));
    }
    Ok(())
}

/// `base.formatted_read_group`.
pub(crate) async fn formatted_read_group(
    tx: &mut Transaction,
    records: &Records,
    args: FormattedReadGroup,
) -> Result<FormattedGroups> {
    refuse_unsupported_context(records)?;
    let aggregates = web_aggregates(&args.aggregates);
    let order = match args.order.as_deref().map(str::trim) {
        Some(o) if !o.is_empty() => o.to_owned(),
        _ => args.groupby.join(", "),
    };
    let mut rg = ReadGroup::new(args.domain.clone())
        .groupby(args.groupby.iter().cloned())
        .aggregates(aggregates.iter().cloned())
        .having(args.having.clone())
        .offset(args.offset)
        .count_groups(args.count_groups)
        .external(true);
    if !order.is_empty() {
        rg = rg.order(order);
    }
    rg.limit = args.limit;
    let groups = records._read_group(tx, rg).await?;
    // group expansion (`read_group_expand`) needs a `group_expand` field attribute: no field
    // has one yet, so there is nothing to expand (M4)
    let rows = format_groups(tx, records, &args.groupby, &aggregates, &groups).await?;
    Ok(FormattedGroups {
        groups: rows,
        group_count: groups.group_count(),
    })
}

/// `base.formatted_read_grouping_sets`.
pub(crate) async fn formatted_read_grouping_sets(
    tx: &mut Transaction,
    records: &Records,
    args: FormattedReadGroupingSets,
) -> Result<Vec<Vec<Value>>> {
    refuse_unsupported_context(records)?;
    let aggregates = web_aggregates(&args.aggregates);
    let order = match args.order.as_deref().map(str::trim) {
        Some(o) if !o.is_empty() => o.to_owned(),
        _ => {
            let mut specs: Vec<&String> = Vec::new();
            for spec in args.grouping_sets.iter().flatten() {
                if !specs.contains(&spec) {
                    specs.push(spec);
                }
            }
            specs
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut sets = ReadGroupingSets::new(args.domain.clone(), args.grouping_sets.clone())
        .aggregates(aggregates.iter().cloned())
        .external(true);
    if !order.is_empty() {
        sets = sets.order(order);
    }
    let results = records._read_grouping_sets(tx, sets).await?;
    let mut out = Vec::with_capacity(results.len());
    for (set, groups) in args.grouping_sets.iter().zip(&results) {
        let rows = format_groups(tx, records, set, &aggregates, groups).await?;
        out.push(rows.into_iter().map(|r| Value::Map(Arc::new(r))).collect());
    }
    Ok(out)
}

// ----- web_read_group ------------------------------------------------------------------------

/// A group of `web_read_group` with what was opened inside it.
#[derive(Debug)]
struct Node {
    row: Row,
    sub: Option<(Vec<Node>, u64)>,
    records: Option<Vec<Value>>,
}

impl Node {
    fn into_value(self) -> Value {
        let mut row = self.row;
        if let Some((groups, length)) = self.sub {
            let mut sub = ValueMap::new();
            sub.insert(
                "groups".to_owned(),
                Value::list(groups.into_iter().map(Node::into_value)),
            );
            sub.insert("length".to_owned(), Value::Int(length as i64));
            row.insert("__groups".to_owned(), Value::Map(Arc::new(sub)));
        }
        if let Some(records) = self.records {
            row.insert("__records".to_owned(), Value::list(records));
        }
        Value::Map(Arc::new(row))
    }
}

/// A last-level group to fill with records.
struct Opening {
    path: Vec<usize>,
    domain: Domain,
    limit: Option<u64>,
    offset: u64,
    count: i64,
}

/// `_formatted_read_group_with_length`: the groups and the number of groups (from the grouped
/// statement itself when the page is full).
async fn groups_with_length(
    tx: &mut Transaction,
    records: &Records,
    args: FormattedReadGroup,
) -> Result<(Vec<Row>, u64)> {
    let (domain, groupby, offset, limit) = (
        args.domain.clone(),
        args.groupby.clone(),
        args.offset,
        args.limit,
    );
    let result = records.formatted_read_group(tx, args).await?;
    let n = result.groups.len() as u64;
    let length = if n == 0 {
        0
    } else if let Some(limit) = limit.filter(|l| *l > 0 && n == *l) {
        match result.group_count {
            Some(total) => total,
            None => {
                let rest = records
                    ._read_group(
                        tx,
                        ReadGroup::new(domain)
                            .groupby(groupby)
                            .offset(limit)
                            .external(true),
                    )
                    .await?;
                limit + rest.len() as u64
            }
        }
    } else {
        n + offset
    };
    Ok((result.groups, length))
}

fn read_group_order(
    model: &ModelSpec,
    dict_order: &[(String, String)],
    groupby: &[String],
    aggregates: &[String],
) -> String {
    if dict_order.is_empty() {
        return groupby.join(", ");
    }
    let mut remaining: Vec<String> = groupby.to_vec();
    let mut spec = Vec::new();
    for (fname, direction) in dict_order {
        if fname == "__count" {
            spec.push(format!("{fname} {direction}"));
            continue;
        }
        if let Some(pos) = remaining
            .iter()
            .position(|g| g == fname || g.starts_with(&format!("{fname}:")))
        {
            let group = remaining.remove(pos);
            spec.push(format!("{group} {direction}"));
        } else if let Some(agg) = aggregates
            .iter()
            .find(|a| a.starts_with(&format!("{fname}:")))
        {
            spec.push(format!("{agg} {direction}"));
        } else if let Some(field) = model.field(fname)
            && matches!(
                field.ty,
                FieldType::Integer | FieldType::Float { .. } | FieldType::Monetary { .. }
            )
        {
            spec.push(format!("{fname}:sum {direction}"));
        }
    }
    spec.extend(remaining);
    spec.join(", ")
}

/// Parse `order` with the search grammar into `(field[.property], "DIR[ NULLS …]")`.
fn dict_order(order: Option<&str>) -> Result<Vec<(String, String)>> {
    let Some(order) = order.filter(|o| !o.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let terms = erp_sql::order::parse_order(order).ok_or_else(|| {
        Error::validation(format!("Invalid order '{order}' for web_read_group()"))
    })?;
    let mut out: Vec<(String, String)> = Vec::new();
    for t in terms {
        let key = match t.property {
            Some(p) => format!("{}.{p}", t.field),
            None => t.field.to_owned(),
        };
        let mut direction = if t.direction.descending() {
            "DESC".to_owned()
        } else {
            "ASC".to_owned()
        };
        match t.nulls {
            erp_sql::order::Nulls::First => direction.push_str(" NULLS FIRST"),
            erp_sql::order::Nulls::Last => direction.push_str(" NULLS LAST"),
            erp_sql::order::Nulls::Default => {}
        }
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = direction,
            None => out.push((key, direction)),
        }
    }
    Ok(out)
}

fn info_field<'a>(info: &'a Value, key: &str) -> Option<&'a Value> {
    match info {
        Value::Map(map) => map.get(key),
        _ => None,
    }
}

struct OpenCtx<'a> {
    records: &'a Records,
    domain: &'a Domain,
    aggregates: &'a [String],
    dict_order: &'a [(String, String)],
    has_opening_info: bool,
    unfold_limit: Option<u64>,
    max_opened: usize,
}

#[allow(clippy::too_many_arguments)]
fn open_groups<'a>(
    tx: &'a mut Transaction,
    ctx: &'a OpenCtx<'a>,
    groups: &'a mut [Node],
    path: Vec<usize>,
    groupby: &'a [String],
    auto_unfold: bool,
    parent_info: Option<&'a [Value]>,
    parent_domain: Domain,
    openings: &'a mut Vec<Opening>,
) -> crate::dispatch::BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let registry = Arc::clone(ctx.records.env().registry());
        let model = registry.model_by_id(ctx.records.model_id());
        let Some(spec) = groupby.first() else {
            return Ok(());
        };
        let fname = spec
            .split(':')
            .next()
            .unwrap_or(spec)
            .split('.')
            .next()
            .unwrap_or(spec);
        let relational = model.field(fname).is_some_and(|f| f.ty.is_relational());
        let last = groupby.len() == 1;
        let sub_order = groupby
            .get(1)
            .map(|g| {
                read_group_order(
                    model,
                    ctx.dict_order,
                    std::slice::from_ref(g),
                    ctx.aggregates,
                )
            })
            .unwrap_or_default();
        let mut opened = 0usize;
        for (i, node) in groups.iter_mut().enumerate() {
            let fold = node.row.shift_remove("__fold");
            let fold_info = fold.is_some();
            let folded = fold.is_some_and(|v| v.is_truthy());
            let value = node.row.get(spec.as_str()).cloned().unwrap_or(Value::False);
            let raw = match &value {
                Value::List(pair) if pair.len() == 2 => pair[0].clone(),
                other => other.clone(),
            };
            let mut limit = ctx.unfold_limit;
            let mut offset = 0u64;
            let mut progressbar = None;
            let mut sub_info: Option<&[Value]> = None;
            let info = parent_info
                .filter(|_| ctx.has_opening_info)
                .and_then(|infos| infos.iter().find(|i| info_field(i, "value") == Some(&raw)));
            if let Some(info) = info {
                if info_field(info, "folded").is_some_and(Value::is_truthy) {
                    continue;
                }
                limit = match info_field(info, "limit") {
                    Some(Value::Int(n)) if *n >= 0 => Some(*n as u64),
                    Some(Value::False) => None,
                    other => {
                        return Err(Error::validation(format!(
                            "web_read_group(): invalid limit {} in opening_info",
                            other.map_or_else(|| "None".to_owned(), ToString::to_string)
                        )));
                    }
                };
                offset = match info_field(info, "offset") {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    Some(Value::False) => 0,
                    other => {
                        return Err(Error::validation(format!(
                            "web_read_group(): invalid offset {} in opening_info",
                            other.map_or_else(|| "None".to_owned(), ToString::to_string)
                        )));
                    }
                };
                progressbar = info_field(info, "progressbar_domain")
                    .filter(|v| v.is_truthy())
                    .cloned();
                sub_info = match info_field(info, "groups") {
                    Some(Value::List(items)) => Some(items),
                    _ => None,
                };
            } else if (!auto_unfold && !fold_info)
                || opened >= ctx.max_opened
                || folded
                || (relational && !value.is_truthy())
            {
                continue;
            }
            opened += 1;
            let extra = node
                .row
                .get("__extra_domain")
                .cloned()
                .unwrap_or_else(|| Value::list([]));
            let mut child_path = path.clone();
            child_path.push(i);
            if last {
                let mut domain = parent_domain.clone() & internal_domain_from_value(&extra)?;
                if let Some(pb) = &progressbar {
                    domain = domain & super::spec::domain_from_value(pb)?;
                }
                let count = match node.row.get("__count") {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                };
                if offset > 0 && offset >= count.max(0) as u64 {
                    offset = 0;
                    node.row.insert("__offset".to_owned(), Value::Int(0));
                }
                openings.push(Opening {
                    path: child_path,
                    domain,
                    limit,
                    offset,
                    count,
                });
            } else {
                let mut sub_domain = parent_domain.clone();
                if extra.is_truthy() {
                    sub_domain = sub_domain & internal_domain_from_value(&extra)?;
                }
                let (rows, length) = groups_with_length(
                    tx,
                    ctx.records,
                    FormattedReadGroup {
                        domain: sub_domain.clone() & ctx.domain.clone(),
                        groupby: vec![groupby[1].clone()],
                        aggregates: ctx.aggregates.to_vec(),
                        having: Vec::new(),
                        offset,
                        limit,
                        order: (!sub_order.is_empty()).then(|| sub_order.clone()),
                        count_groups: limit.is_some(),
                    },
                )
                .await?;
                let mut subgroups: Vec<Node> = rows
                    .into_iter()
                    .map(|row| Node {
                        row,
                        sub: None,
                        records: None,
                    })
                    .collect();
                open_groups(
                    tx,
                    ctx,
                    &mut subgroups,
                    child_path,
                    &groupby[1..],
                    false,
                    sub_info,
                    sub_domain,
                    openings,
                )
                .await?;
                node.sub = Some((subgroups, length));
            }
        }
        Ok(())
    })
}

fn node_at<'a>(nodes: &'a mut [Node], path: &[usize]) -> Option<&'a mut Node> {
    let (first, rest) = path.split_first()?;
    let node = nodes.get_mut(*first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        node_at(&mut node.sub.as_mut()?.0, rest)
    }
}

/// The records of every opened group in one statement: one sub-select per group (its domain,
/// order, limit and offset), `UNION ALL`, rows numbered within their group. `fetch` columns are
/// fetched into the cache on the way.
async fn batched_searches(
    tx: &mut Transaction,
    records: &Records,
    searches: &[(Domain, Option<u64>, u64)],
    order: &str,
    fetch: &[FieldId],
) -> Result<Vec<Vec<RecId>>> {
    let mut out: Vec<Vec<RecId>> = vec![Vec::new(); searches.len()];
    let registry = Arc::clone(records.env().registry());
    let spec = registry.model_by_id(records.model_id());
    tx.validate_search_language(records.env()).await?;
    let order_by = order_for(tx, records, order, 0).await?;
    let table = quote(&spec.table);
    let columns: String = fetch
        .iter()
        .map(|f| {
            let field = spec.field_by_id(*f);
            let column = format!("{table}.{}", quote(field.name));
            let item = erp_sql::company::load_column(&registry, field, &column).unwrap_or(column);
            format!(", {item}")
        })
        .collect();
    let mut params: Vec<Param> = order_by.params.clone();
    let mut flush: Vec<(erp_registry::ModelId, FieldId)> = order_by.fields.clone();
    let mut subs: Vec<String> = Vec::new();
    for (i, (domain, limit, offset)) in searches.iter().enumerate() {
        let filter = compile_for_at(tx, records, domain, true, params.len()).await?;
        if filter.constant == Some(false) {
            continue;
        }
        params.extend(filter.params.iter().cloned());
        flush.extend(filter.fields.iter().copied());
        erp_sql::order::check_join_conflicts(&[&filter.join_list, &order_by.joins])?;
        let joins = merge_joins(&[&filter.join_list, &order_by.joins]);
        // rows are numbered after the page is cut (a window over every matching row before
        // LIMIT would number the whole group)
        let mut sub = format!(
            "SELECT {table}.{id}{columns} FROM {table}{joins}",
            id = quote("id"),
        );
        if filter.constant != Some(true) {
            sub.push_str(" WHERE ");
            sub.push_str(&filter.sql);
        }
        sub.push_str(&format!(" ORDER BY {}", order_by.sql));
        if let Some(limit) = limit {
            params.push(Param::Count(i64::try_from(*limit).map_err(|_| {
                Error::programming(format!("limit out of range: {limit}"))
            })?));
            sub.push_str(&format!(" LIMIT ${}::int8", params.len()));
        }
        if *offset > 0 {
            params.push(Param::Count(i64::try_from(*offset).map_err(|_| {
                Error::programming(format!("offset out of range: {offset}"))
            })?));
            sub.push_str(&format!(" OFFSET ${}::int8", params.len()));
        }
        subs.push(format!(
            "(SELECT \"__l\".*, {i}::int4 AS \"__g\", row_number() OVER () AS \"__n\" FROM \
             ({sub}) AS \"__l\")"
        ));
    }
    if subs.is_empty() {
        return Ok(out);
    }
    flush.sort_unstable();
    flush.dedup();
    flush_read_inputs(tx, &flush).await?;
    let sql = format!(
        "SELECT * FROM ({}) AS \"__s\" ORDER BY \"__s\".\"__g\", \"__s\".\"__n\"",
        subs.join(" UNION ALL ")
    );
    // rows are cached as they arrive (design note `perf-stream.md` §3.1)
    let cacher = crate::search::RowCacher::new(spec, fetch);
    let g_index = 1 + fetch.len();
    let policy = tx.plan_policy(&params);
    // the row count is not known before the answer: the sink expects as many rows as it has
    // seen so far
    let mut rows = cacher.sink(0);
    tx.query_each_with(&sql, &params, policy, |cache, row| {
        let id = rows.cache_row(cache, row)?;
        let g: i32 = row.try_get(g_index).map_err(|e| {
            Error::new(
                erp_fields::ErrorKind::Database,
                "could not decode a group index",
            )
            .with_source(e)
        })?;
        // `g` is the index of the sub-select this statement numbered itself
        let group = usize::try_from(g)
            .ok()
            .and_then(|g| out.get_mut(g))
            .ok_or_else(|| {
                Error::programming(format!(
                    "the batched group search returned the group index {g} of {}",
                    searches.len()
                ))
            })?;
        group.push(id);
        Ok(())
    })
    .await?;
    Ok(out)
}

/// The stored columns a specification reads (fetched with the records of opened groups).
fn spec_columns(model: &ModelSpec, spec: &Spec) -> Vec<FieldId> {
    let mut out = Vec::new();
    for name in spec.names() {
        let Some(field) = model.field(name) else {
            continue;
        };
        let id = if field.has_column() {
            Some(field.id)
        } else if name == "display_name" {
            model
                .rec_name
                .filter(|f| model.field_by_id(*f).has_column())
        } else {
            None
        };
        if let Some(id) = id
            && !out.contains(&id)
        {
            out.push(id);
        }
    }
    out
}

/// `base.web_read_group`.
pub(crate) async fn web_read_group(
    tx: &mut Transaction,
    records: &Records,
    args: WebReadGroup,
) -> Result<Value> {
    if args.groupby.is_empty() {
        return Err(Error::validation("Need at least one item to groupby"));
    }
    if args.groupby.len() > 64 {
        return Err(Error::validation(format!(
            "'groupby' accepts at most 64 entries, got {}",
            args.groupby.len()
        )));
    }
    let registry = Arc::clone(records.env().registry());
    let model = registry.model_by_id(records.model_id());
    let mut aggregates = args.aggregates.clone();
    if aggregates.len() > 64 {
        return Err(Error::validation(format!(
            "'aggregates' accepts at most 64 entries, got {}",
            aggregates.len()
        )));
    }
    if !aggregates.iter().any(|a| a == "__count") {
        aggregates.push("__count".to_owned());
    }
    let mut domain = args.domain.clone();
    if let Some(active) = model.active_name.map(|f| model.field_by_id(f).name)
        && domain
            .iter_conditions()
            .filter_map(Domain::as_condition)
            .any(|c| c.field_expr() == active)
    {
        domain = domain & Domain::field(active)?.in_(erp_domain::Value::list([true, false]));
    }
    let dict_order = dict_order(args.order.as_deref())?;
    let first = vec![args.groupby[0].clone()];
    let order = read_group_order(model, &dict_order, &first, &aggregates);
    let records_fmt = records.with_context([("formatted_display_name", Value::Bool(true))]);
    let (rows, length) = groups_with_length(
        tx,
        &records_fmt,
        FormattedReadGroup {
            domain: domain.clone(),
            groupby: first,
            aggregates: aggregates.clone(),
            having: Vec::new(),
            offset: args.offset,
            limit: args.limit,
            order: (!order.is_empty()).then_some(order),
            count_groups: args.limit.is_some(),
        },
    )
    .await?;
    let mut groups: Vec<Node> = rows
        .into_iter()
        .map(|row| Node {
            row,
            sub: None,
            records: None,
        })
        .collect();
    let max_opened = match records.env().context().get("max_number_opened_groups") {
        Some(Value::Int(n)) if n > 0 => (n as usize).min(MAX_NUMBER_OPENED_GROUPS),
        _ => MAX_NUMBER_OPENED_GROUPS,
    };
    let opening_info: Option<Vec<Value>> = args.opening_info.clone();
    let ctx = OpenCtx {
        records,
        domain: &domain,
        aggregates: &aggregates,
        dict_order: &dict_order,
        has_opening_info: opening_info.as_ref().is_some_and(|o| !o.is_empty()),
        unfold_limit: args.unfold_read_default_limit,
        max_opened,
    };
    let mut openings: Vec<Opening> = Vec::new();
    open_groups(
        tx,
        &ctx,
        &mut groups,
        Vec::new(),
        &args.groupby,
        args.auto_unfold,
        opening_info.as_deref(),
        Domain::TRUE,
        &mut openings,
    )
    .await?;
    if !openings.is_empty() {
        let mut order_specs: Vec<String> = dict_order
            .iter()
            .filter(|(f, _)| !args.groupby.contains(f) && f != "__count")
            .map(|(f, d)| format!("{f} {d}"))
            .collect();
        for part in model.order.split(',') {
            let fname = part.trim().split(' ').next().unwrap_or_default();
            if !fname.is_empty()
                && !dict_order.iter().any(|(f, _)| f == fname)
                && !args.groupby.iter().any(|g| g == fname)
            {
                order_specs.push(part.trim().to_owned());
            }
        }
        let order = order_specs.join(", ");
        let spec = args.unfold_read_specification.clone().unwrap_or_default();
        super::read::check_spec(records, &spec)?;
        let searches: Vec<(Domain, Option<u64>, u64)> = openings
            .iter()
            .map(|o| {
                if o.count > 0 {
                    (domain.clone() & o.domain.clone(), o.limit, o.offset)
                } else {
                    (Domain::FALSE, o.limit, o.offset)
                }
            })
            .collect();
        let found =
            batched_searches(tx, records, &searches, &order, &spec_columns(model, &spec)).await?;
        let mut all: Vec<RecId> = Vec::new();
        for ids in &found {
            for id in ids {
                if !all.contains(id) {
                    all.push(*id);
                }
            }
        }
        let union = records.browse_ids(all.iter().copied());
        let read = web_read_rows(tx, &union, &spec).await?;
        let by_id: FxHashMap<i64, Row> = read
            .into_iter()
            .filter_map(|row| match row.get("id") {
                Some(Value::Int(i)) => Some((*i, row)),
                _ => None,
            })
            .collect();
        for (opening, ids) in openings.iter().zip(found) {
            if let Some(node) = node_at(&mut groups, &opening.path) {
                node.records = Some(
                    ids.iter()
                        .filter_map(|id| by_id.get(&id.get()).cloned())
                        .map(|row| Value::Map(Arc::new(row)))
                        .collect(),
                );
            }
        }
    }
    if let Some(specs) = &args.groupby_read_specification {
        add_groupby_values(tx, records, specs, &args.groupby, &mut groups).await?;
    }
    let mut out = ValueMap::new();
    out.insert(
        "groups".to_owned(),
        Value::list(groups.into_iter().map(Node::into_value)),
    );
    out.insert("length".to_owned(), Value::Int(length as i64));
    Ok(Value::Map(Arc::new(out)))
}

const MAX_NUMBER_OPENED_GROUPS: usize = 10;

async fn add_groupby_values(
    tx: &mut Transaction,
    records: &Records,
    specs: &BTreeMap<String, Spec>,
    groupby: &[String],
    groups: &mut [Node],
) -> Result<()> {
    if !groupby.iter().any(|g| specs.contains_key(g)) {
        return Ok(());
    }
    let registry = Arc::clone(records.env().registry());
    let model = registry.model_by_id(records.model_id());
    let mut level: Vec<&mut Node> = groups.iter_mut().collect();
    for spec_name in groupby {
        if let Some(read_spec) = specs.get(spec_name) {
            let field = model.field(spec_name).ok_or_else(|| {
                Error::validation(format!(
                    "Invalid field '{spec_name}' on model '{}'",
                    model.name
                ))
            })?;
            let comodel = field.ty.comodel().ok_or_else(|| {
                Error::validation(format!(
                    "We can only read extra info from a relational field ('{spec_name}')"
                ))
            })?;
            let ids: Vec<i64> = level
                .iter()
                .filter_map(|n| match n.row.get(spec_name.as_str()) {
                    Some(Value::List(pair)) => match pair.first() {
                        Some(Value::Int(i)) => Some(*i),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            let co = records
                .env()
                .records(comodel)?
                .browse(ids.iter().copied())?;
            let rows = web_read_rows(tx, &co, read_spec).await?;
            let by_id: FxHashMap<i64, Row> = rows
                .into_iter()
                .filter_map(|row| match row.get("id") {
                    Some(Value::Int(i)) => Some((*i, row)),
                    _ => None,
                })
                .collect();
            for node in level.iter_mut() {
                let value = match node.row.get(spec_name.as_str()) {
                    Some(Value::List(pair)) => match pair.first() {
                        Some(Value::Int(i)) => by_id
                            .get(i)
                            .cloned()
                            .map_or(Value::False, |r| Value::Map(Arc::new(r))),
                        _ => Value::False,
                    },
                    _ => {
                        let mut only = Row::new();
                        only.insert("id".to_owned(), Value::False);
                        Value::Map(Arc::new(only))
                    }
                };
                node.row.insert("__values".to_owned(), value);
            }
        }
        level = level
            .into_iter()
            .flat_map(|n| match n.sub.as_mut() {
                Some((subs, _)) => subs.iter_mut().collect::<Vec<_>>(),
                None => Vec::new(),
            })
            .collect();
    }
    Ok(())
}

/// `base.read_progress_bar`.
pub(crate) async fn read_progress_bar(
    tx: &mut Transaction,
    records: &Records,
    args: super::ProgressBar,
) -> Result<Value> {
    let groups = records
        .formatted_read_group(
            tx,
            FormattedReadGroup {
                domain: args.domain.clone(),
                groupby: vec![args.group_by.clone(), args.field.clone()],
                aggregates: vec!["__count".to_owned()],
                having: Vec::new(),
                offset: 0,
                limit: None,
                order: None,
                count_groups: false,
            },
        )
        .await?;
    let mut out: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    for group in groups.groups {
        let Some(Value::Str(color)) = group.get(&args.field) else {
            continue;
        };
        if !args.colors.iter().any(|c| **c == **color) {
            continue;
        }
        let key = match group.get(&args.group_by) {
            Some(Value::List(pair)) if pair.len() == 2 => py_str(&pair[0]),
            Some(other) => py_str(other),
            None => "False".to_owned(),
        };
        let count = match group.get("__count") {
            Some(Value::Int(n)) => *n,
            _ => 0,
        };
        let entry = out
            .entry(key)
            .or_insert_with(|| args.colors.iter().map(|c| (c.clone(), 0)).collect());
        *entry.entry(color.to_string()).or_insert(0) += count;
    }
    Ok(Value::Map(Arc::new(
        out.into_iter()
            .map(|(k, colors)| {
                (
                    k,
                    Value::Map(Arc::new(
                        colors
                            .into_iter()
                            .map(|(c, n)| (c, Value::Int(n)))
                            .collect(),
                    )),
                )
            })
            .collect(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_en_us() {
        let d = Date::from_ymd(2022, 1, 23).unwrap();
        assert_eq!(bucket_label(d, 0, Granularity::Week), "W5 2022");
        let d = Date::from_ymd(2022, 1, 30).unwrap();
        assert_eq!(bucket_label(d, 0, Granularity::Week), "W6 2022");
        let d = Date::from_ymd(2022, 1, 29).unwrap();
        assert_eq!(bucket_label(d, 0, Granularity::Day), "29 Jan 2022");
        let d = Date::from_ymd(2022, 1, 1).unwrap();
        assert_eq!(bucket_label(d, 0, Granularity::Month), "January 2022");
        assert_eq!(bucket_label(d, 0, Granularity::Quarter), "Q1 2022");
        assert_eq!(bucket_label(d, 14, Granularity::Hour), "14:00 01 Jan");
        // a week overlapping the new year belongs to the next year
        let d = Date::from_ymd(2015, 12, 27).unwrap();
        assert_eq!(week_number(d, 6), (2016, 1));
    }

    #[test]
    fn extra_domains_flatten_conjunctions() {
        let d = FDom::and(vec![
            FDom::cond("key", "=", Value::Int(1)),
            FDom::and(vec![
                FDom::cond("date", ">=", Value::str("a")),
                FDom::cond("date", "<", Value::str("b")),
            ]),
        ]);
        let list = d.to_list();
        assert_eq!(list.len(), 5);
        assert_eq!(list[0], Value::str("&"));
        assert_eq!(list[1], Value::str("&"));
    }
}
