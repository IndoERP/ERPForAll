//! Leaf partitioning: the owner's `partition(...)` declaration, its partition key and the rules the
//! registry enforces at composition.
//!
//! A partitioned model keeps its rows in one table per time interval (`hour`, `day`, `week` or
//! `month`) of its partition key: `create_date`, or a stored, required `datetime` field declared
//! `server_set`. Either way the key is the time the server inserted the row, never a value a caller
//! chose. Only the declaration that creates the model may partition it, and only when no field
//! references it (a *leaf* model): its primary key is `(id, key)`, so `id` alone is not unique and a
//! foreign key to it could not exist.
//!
//! The interval arithmetic and the partition names live here (pure functions, layer L2) so the SQL
//! builders, schema sync and the upkeep calls agree on them.

use erp_fields::{Date, DateTime, Error, FieldType, Result};
use rustc_hash::FxHashSet;

use crate::decl::{ModelDecl, ModelKind, TableObjectKind};
use crate::drop::{DropReason, Sink};
use crate::spec::{FieldId, FieldSpec, ModelSpec};

/// The decision the partitioning errors point to (its number is assigned when the ADR is accepted).
pub const PARTITION_ADR: &str = "see the documentation";

/// Largest `premake` and `retain` a declaration may give, in intervals.
pub const MAX_PARTITION_INTERVALS: u32 = 1_000;

/// Smallest `premake` of an hourly partition: upkeep runs may be up to one hour apart (the bound of
/// `base.partition_upkeep_interval_secs`), so one hour ahead can run out before the next run.
pub const MIN_HOURLY_PREMAKE: u32 = 2;

/// Longest table name of a partitioned model in bytes: its partition names append `_p` and up to
/// ten digits (`_p2026101603` for an hour), and PostgreSQL identifiers end at 63 bytes.
pub const MAX_PARTITIONED_TABLE_LEN: usize = 51;

/// The policy-source and membership models of the access engine: never partitioned.
pub const POLICY_SOURCE_MODELS: [&str; 6] = [
    "ir.access",
    "res.groups",
    "res.groups.privilege",
    "ir.capability",
    "ir.model.fields",
    "res.users",
];

const MICROS_PER_HOUR: i64 = 3_600 * 1_000_000;
const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;
const MICROS_PER_WEEK: i64 = 7 * MICROS_PER_DAY;

/// The time span one partition covers. Weeks start on Monday; all bounds are UTC (naive datetimes
/// mean UTC, FIELD §4.8.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PartitionInterval {
    /// One partition per hour, named `<table>_p<YYYYMMDDHH>`.
    Hour,
    /// One partition per day, named `<table>_p<YYYYMMDD>`.
    Day,
    /// One partition per week (Monday to Monday), named after its Monday `<table>_p<YYYYMMDD>`.
    Week,
    /// One partition per calendar month, named `<table>_p<YYYYMM>`.
    Month,
}

/// The midnight of `date` without overflowing (a date far outside PostgreSQL's range has none).
fn midnight(date: Date) -> Option<DateTime> {
    i64::from(date.days())
        .checked_mul(MICROS_PER_DAY)
        .map(DateTime::from_micros)
}

/// `t` rounded down to a multiple of `unit` microseconds since the epoch.
fn floor_to(t: DateTime, unit: i64) -> Option<DateTime> {
    t.micros()
        .div_euclid(unit)
        .checked_mul(unit)
        .map(DateTime::from_micros)
}

impl PartitionInterval {
    /// The declaration word: `hour`, `day`, `week` or `month`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    /// `n` intervals in words, for messages: `1 day`, `2 days`.
    pub fn count(self, n: u32) -> String {
        if n == 1 {
            format!("1 {}", self.name())
        } else {
            format!("{n} {}s", self.name())
        }
    }

    /// The start of the interval holding `t`; `None` only for instants no partition can cover
    /// (beyond the range of a 64-bit microsecond count).
    ///
    /// ```
    /// use erp_fields::{Date, DateTime};
    /// use erp_registry::PartitionInterval;
    /// let t = DateTime::from_parts(Date::from_ymd(2026, 10, 16).unwrap(), 13, 45, 0, 0).unwrap();
    /// let week = PartitionInterval::Week.start_of(t).unwrap();
    /// assert_eq!(week.to_string(), "2026-10-12 00:00:00"); // the Monday before
    /// ```
    pub fn start_of(self, t: DateTime) -> Option<DateTime> {
        match self {
            Self::Hour => floor_to(t, MICROS_PER_HOUR),
            Self::Day => floor_to(t, MICROS_PER_DAY),
            Self::Week => {
                // 1970-01-01 was a Thursday: Monday is 3 days before an epoch-aligned week start
                let days = i64::from(t.date().days());
                let monday = days.checked_sub(days.checked_add(3)?.rem_euclid(7))?;
                midnight(Date::from_days(i32::try_from(monday).ok()?))
            }
            Self::Month => {
                let (year, month, _) = t.date().ymd();
                midnight(Date::from_ymd(year, month, 1)?)
            }
        }
    }

    /// `start` moved by `n` intervals forward (`n > 0`) or backward (`n < 0`), with checked
    /// arithmetic; `start` must be the start of an interval ([`PartitionInterval::start_of`]).
    pub fn shift(self, start: DateTime, n: i64) -> Option<DateTime> {
        let by = |unit: i64| {
            n.checked_mul(unit)
                .and_then(|delta| start.micros().checked_add(delta))
                .map(DateTime::from_micros)
        };
        match self {
            Self::Hour => by(MICROS_PER_HOUR),
            Self::Day => by(MICROS_PER_DAY),
            Self::Week => by(MICROS_PER_WEEK),
            Self::Month => {
                let (year, month, _) = start.date().ymd();
                let index = i64::from(year)
                    .checked_mul(12)?
                    .checked_add(i64::from(month) - 1)?
                    .checked_add(n)?;
                let year = i32::try_from(index.div_euclid(12)).ok()?;
                let month = u32::try_from(index.rem_euclid(12) + 1).ok()?;
                midnight(Date::from_ymd(year, month, 1)?)
            }
        }
    }

    /// The start of the interval after the one starting at `start`.
    pub fn next(self, start: DateTime) -> Option<DateTime> {
        self.shift(start, 1)
    }

    /// The digits of a partition name for the interval starting at `start`: `YYYYMMDDHH` (hour),
    /// `YYYYMMDD` (day, week) or `YYYYMM` (month). `None` for years outside 1–9999.
    pub fn suffix(self, start: DateTime) -> Option<String> {
        let (year, month, day) = start.date().ymd();
        if !(1..=9999).contains(&year) {
            return None;
        }
        let (hour, _, _, _) = start.time();
        Some(match self {
            Self::Hour => format!("{year:04}{month:02}{day:02}{hour:02}"),
            Self::Day | Self::Week => format!("{year:04}{month:02}{day:02}"),
            Self::Month => format!("{year:04}{month:02}"),
        })
    }

    /// The interval start a partition-name suffix stands for, when the suffix has this interval's
    /// exact form and names the start of an interval (a week suffix must be a Monday).
    pub fn parse_suffix(self, suffix: &str) -> Option<DateTime> {
        let len = match self {
            Self::Hour => 10,
            Self::Day | Self::Week => 8,
            Self::Month => 6,
        };
        if suffix.len() != len || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let number = |range: std::ops::Range<usize>| suffix.get(range)?.parse::<u32>().ok();
        let year = i32::try_from(number(0..4)?).ok()?;
        // `suffix` names years 1 to 9999 only; a year 0 suffix is no partition of ours
        if year < 1 {
            return None;
        }
        let month = number(4..6)?;
        let day = if len >= 8 { number(6..8)? } else { 1 };
        let hour = if len == 10 { number(8..10)? } else { 0 };
        let start = DateTime::from_parts(Date::from_ymd(year, month, day)?, hour, 0, 0, 0)?;
        (self.start_of(start)? == start).then_some(start)
    }
}

/// How long a partitioned model keeps its partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PartitionRetain {
    /// Partitions whose whole range is older than this many intervals are detached and dropped by
    /// the upkeep (1 to [`MAX_PARTITION_INTERVALS`]).
    Intervals(u32),
    /// Every partition is kept (`retain = "all"`); the upkeep never retires one.
    All,
}

impl PartitionRetain {
    /// The number of intervals kept, or `None` for [`PartitionRetain::All`].
    pub const fn intervals(self) -> Option<u32> {
        match self {
            Self::Intervals(n) => Some(n),
            Self::All => None,
        }
    }
}

/// `partition(interval = ..., premake = ..., retain = ..., by = ...)` on the declaration that
/// creates a model (`#[model(partition = partition(...))]`). Validated when the registry is built.
///
/// ```
/// use erp_registry::{ModelDecl, PartitionDecl, PartitionInterval, PartitionRetain};
/// static OUTBOX: ModelDecl = ModelDecl::new("my_addon", "my.event").partition(
///     PartitionDecl::new(PartitionInterval::Day, 7, PartitionRetain::Intervals(30)),
/// );
/// assert_eq!(OUTBOX.partition.unwrap().premake(), 7);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PartitionDecl {
    interval: PartitionInterval,
    premake: u32,
    retain: PartitionRetain,
    by: Option<&'static str>,
}

impl PartitionDecl {
    /// Partitions of `interval`, `premake` intervals created ahead of the current one, kept as
    /// `retain` says. The key is `create_date` unless [`PartitionDecl::by`] names another field.
    pub const fn new(interval: PartitionInterval, premake: u32, retain: PartitionRetain) -> Self {
        Self {
            interval,
            premake,
            retain,
            by: None,
        }
    }

    /// Partition by `field` instead of `create_date`: a stored, required `datetime` field declared
    /// `server_set` (checked when the registry is built).
    pub const fn by(mut self, field: &'static str) -> Self {
        self.by = Some(field);
        self
    }

    /// The declared interval.
    pub const fn interval(&self) -> PartitionInterval {
        self.interval
    }

    /// Intervals created ahead of the current one.
    pub const fn premake(&self) -> u32 {
        self.premake
    }

    /// How long partitions are kept.
    pub const fn retain(&self) -> PartitionRetain {
        self.retain
    }

    /// The key field named with `by`, if any (`None`: `create_date`).
    pub const fn key_field(&self) -> Option<&'static str> {
        self.by
    }
}

/// A model's partitioning after the registry checked it: interval, window and the key field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Partition {
    interval: PartitionInterval,
    premake: u32,
    retain: PartitionRetain,
    key: FieldId,
    key_name: &'static str,
}

impl Partition {
    /// The interval of one partition.
    pub const fn interval(&self) -> PartitionInterval {
        self.interval
    }

    /// Intervals created ahead of the current one (1 to [`MAX_PARTITION_INTERVALS`]).
    pub const fn premake(&self) -> u32 {
        self.premake
    }

    /// How long partitions are kept.
    pub const fn retain(&self) -> PartitionRetain {
        self.retain
    }

    /// The partition key field.
    pub const fn key(&self) -> FieldId {
        self.key
    }

    /// The partition key's column name.
    pub const fn key_name(&self) -> &'static str {
        self.key_name
    }

    /// The name of the partition of `table` that starts at `start` (an interval start).
    pub fn partition_name(&self, table: &str, start: DateTime) -> Option<String> {
        partition_name(table, self.interval, start)
    }
}

/// `<table>_p<suffix>`: the name of the partition of `table` for the interval starting at `start`
/// (`None` when the start has no name, see [`PartitionInterval::suffix`]).
pub fn partition_name(table: &str, interval: PartitionInterval, start: DateTime) -> Option<String> {
    interval
        .suffix(start)
        .map(|suffix| format!("{table}_p{suffix}"))
}

/// What [`resolve`] needs to know about the model being built.
pub(crate) struct Subject<'a> {
    pub(crate) name: &'static str,
    pub(crate) kind: ModelKind,
    pub(crate) mro: &'a [&'static ModelDecl],
    pub(crate) table: &'a str,
    pub(crate) fields: &'a [FieldSpec],
    pub(crate) fillfactor: Option<u8>,
    pub(crate) namespace: crate::TableNamespace,
}

/// The partitioning of a model being built: taken from the declaration that creates it (never from
/// an extension, never inherited by a model with a new name) and checked against the model alone.
/// The rules that look across models run in [`check_references`].
///
/// # Errors
///
/// `Programming` naming the model: an extension sets `partition`, a policy-source model, an
/// abstract or transient model, `premake`/`retain` outside 1–1000, an hourly `premake` below
/// [`MIN_HOURLY_PREMAKE`], a fill factor, a table name
/// too long for its partition names, or a key that is neither `create_date` nor a stored,
/// required `datetime` field declared `server_set`.
pub(crate) fn resolve(subject: &Subject<'_>) -> Result<Option<Partition>> {
    let name = subject.name;
    let own: Vec<&'static ModelDecl> = subject
        .mro
        .iter()
        .copied()
        .filter(|d| d.name == name)
        .collect();
    // the oldest declaration of the name that is not an extension created the model
    let owner = own.iter().rev().find(|d| !d.is_extension());
    if own
        .iter()
        .any(|d| d.is_extension() && d.partition.is_some())
    {
        return Err(Error::programming(match owner {
            Some(owner) => format!(
                "partitioning of '{name}' can only be declared by its owner ({})",
                owner.addon
            ),
            // every composed model has its creating declaration in its MRO; said plainly if not
            None => format!(
                "partitioning of '{name}' can only be declared by the declaration that creates it"
            ),
        }));
    }
    let Some(decl) = owner.and_then(|d| d.partition) else {
        return Ok(None);
    };
    if POLICY_SOURCE_MODELS.contains(&name) {
        return Err(Error::programming(format!(
            "model '{name}' is a policy source and cannot be partitioned"
        )));
    }
    match subject.kind {
        ModelKind::Regular => {}
        ModelKind::Abstract => {
            return Err(Error::programming(format!(
                "model '{name}' is abstract and cannot be partitioned: it has no table"
            )));
        }
        ModelKind::Transient => {
            return Err(Error::programming(format!(
                "model '{name}' is transient and cannot be partitioned: its rows are removed by \
                 age, not by retiring partitions"
            )));
        }
    }
    let in_range = |n: u32| (1..=MAX_PARTITION_INTERVALS).contains(&n);
    if !in_range(decl.premake) {
        return Err(Error::programming(format!(
            "partition of '{name}': premake must be 1 to {MAX_PARTITION_INTERVALS} intervals (got \
             {})",
            decl.premake
        )));
    }
    if decl.interval == PartitionInterval::Hour && decl.premake < MIN_HOURLY_PREMAKE {
        return Err(Error::programming(format!(
            "partition of '{name}': an hourly partition needs premake of at least \
             {MIN_HOURLY_PREMAKE} intervals, because upkeep may run up to one hour apart (got {})",
            decl.premake
        )));
    }
    if let PartitionRetain::Intervals(n) = decl.retain
        && !in_range(n)
    {
        return Err(Error::programming(format!(
            "partition of '{name}': retain must be 1 to {MAX_PARTITION_INTERVALS} intervals or \
             \"all\" (got {n})"
        )));
    }
    if subject.fillfactor.is_some() {
        return Err(Error::programming(format!(
            "model '{name}' is partitioned: a fill factor cannot be set on a partitioned table"
        )));
    }
    if subject.table.len() > MAX_PARTITIONED_TABLE_LEN {
        return Err(Error::programming(format!(
            "table '{}' of partitioned model '{name}' is longer than {MAX_PARTITIONED_TABLE_LEN} \
             bytes, too long for its partition names",
            subject.table
        )));
    }
    let key = decl.by.unwrap_or("create_date");
    let field = subject.fields.iter().find(|f| f.name == key);
    let key_field = match field {
        // the audit field the ORM fills from the transaction timestamp
        Some(f) if key == "create_date" && f.magic && f.has_column() => f,
        None if key == "create_date" => {
            return Err(Error::programming(format!(
                "partitioned model '{name}' has no create_date (log_access is off): name a stored, \
                 required datetime field declared server_set with by = \"<field>\""
            )));
        }
        Some(f) if f.is_generated() => {
            return Err(Error::programming(format!(
                "Invalid generated field '{name}.{key}': partition key cannot be a generated field"
            )));
        }
        Some(f)
            if f.server_set
                && f.required
                && f.has_column()
                && (matches!(f.ty, FieldType::Datetime)
                    // a native instant is a key only in the closed event namespace
                    || (matches!(f.ty, FieldType::TimestampTz)
                        && subject.namespace == crate::TableNamespace::Event))
                && f.inherited.is_none() =>
        {
            f
        }
        _ => {
            return Err(Error::programming(format!(
                "partition key '{key}' of '{name}' must be create_date or a stored, required \
                 datetime field declared server_set"
            )));
        }
    };
    Ok(Some(Partition {
        interval: decl.interval,
        premake: decl.premake,
        retain: decl.retain,
        key: key_field.id,
        key_name: key_field.name,
    }))
}

/// The leaf rule (design §7.2): no many2one (an `_inherits` delegate is one) or many2many may
/// point to a partitioned model, and a partitioned model has no many2many of its own (its relation
/// table would reference a non-unique `id`). Soft references (`res_model`/`res_id`, `Reference`
/// fields) carry the model in data and are allowed. Runs once the relational setup is done.
///
/// A custom (`x_`) field that references a partitioned model is refused like any broken custom
/// field: under [`crate::ManualFieldPolicy::DropBroken`] it is left out and reported.
///
/// # Errors
///
/// `Programming` naming the partitioned model and the first referencing field, in registry and
/// field order.
pub(crate) fn check_references(models: &[ModelSpec], sink: &mut Sink) -> Result<()> {
    let partitioned: FxHashSet<&str> = models
        .iter()
        .filter(|m| m.partition.is_some())
        .map(|m| m.name)
        .collect();
    if partitioned.is_empty() {
        return Ok(());
    }
    for model in models {
        for field in &model.fields {
            // an `_inherits` copy is reported through the parent's own field
            if field.inherited.is_some() {
                continue;
            }
            let target = match &field.ty {
                FieldType::Many2one { comodel, .. } => comodel.as_str(),
                // a many2many of a partitioned model references it through its relation table
                FieldType::Many2many { .. } if partitioned.contains(model.name) => model.name,
                FieldType::Many2many { comodel, .. } => comodel.as_str(),
                // one2many is the reverse of a many2one (refused there); references and
                // many2one_references are soft (the model name is data); the rest are no references
                FieldType::One2many { .. }
                | FieldType::Reference { .. }
                | FieldType::Many2oneReference { .. }
                | FieldType::Id
                | FieldType::Boolean
                | FieldType::BigInt
                | FieldType::Integer
                | FieldType::Float { .. }
                | FieldType::Monetary { .. }
                | FieldType::Char { .. }
                | FieldType::Text
                | FieldType::Html
                | FieldType::Selection { .. }
                | FieldType::Date
                | FieldType::Datetime
                | FieldType::Uuid
                | FieldType::Xid8
                | FieldType::TimestampTz
                | FieldType::DateRange { .. }
                | FieldType::DatetimeRange { .. }
                | FieldType::RowVersion
                | FieldType::Json => continue,
            };
            if partitioned.contains(target) {
                let error = Error::programming(format!(
                    "model '{target}' is partitioned, so no field may reference it; '{}.{}' does",
                    model.name, field.name
                ));
                sink.field(&model.mro, field, DropReason::Refused, error)?;
            }
        }
    }
    Ok(())
}

/// The rules for the table objects of a partitioned model (design §7.1, §10), checked once the
/// models are composed:
///
/// - an exclusion or a `WITHOUT OVERLAPS` key must compare the partition key with `=`, because
///   PostgreSQL builds such a constraint per partition and a partitioned table's unique-like
///   constraint must include the key;
/// - a business key (the arbiter of `upsert`) is refused outright: the key would contain the
///   partition key, which the server sets on every insert, so no row could ever match it;
/// - a `UNIQUE` or `PRIMARY KEY` constraint or a unique index must list the partition key in its
///   column list (the first parenthesised group of its definition, as an identifier; `INCLUDE`
///   columns and a `WHERE` clause do not count), because PostgreSQL refuses it otherwise when the
///   schema is synchronized. The check reads simple column lists; an expression that merely
///   mentions the key is not told apart from the column itself.
///
/// # Errors
///
/// `Programming` naming the model and the first offending object in registry order.
pub(crate) fn check_objects(models: &[ModelSpec]) -> Result<()> {
    for model in models {
        let Some(partition) = model.partition else {
            continue;
        };
        let key = partition.key_name();
        for object in &model.table_objects {
            let name = object.name;
            let model_name = model.name;
            match object.kind {
                TableObjectKind::Key(_) => {
                    return Err(Error::programming(format!(
                        "business key '{name}' of partitioned model '{model_name}' cannot be an \
                         upsert key: a unique constraint of a partitioned table must include the \
                         partition key '{key}', which the server sets on every insert, so no row \
                         could ever match it; rows of a partitioned model are inserted with \
                         create, not upserted"
                    )));
                }
                TableObjectKind::Exclusion(exclusion) if !exclusion.equal.contains(&key) => {
                    return Err(Error::programming(format!(
                        "exclusion '{name}' of partitioned model '{model_name}' must include the \
                         partition key '{key}'"
                    )));
                }
                TableObjectKind::TemporalKey(temporal) if !temporal.fields.contains(&key) => {
                    return Err(Error::programming(format!(
                        "key '{name}' of partitioned model '{model_name}' must include the \
                         partition key '{key}'"
                    )));
                }
                TableObjectKind::Constraint { definition, .. }
                    if is_unique_definition(definition) && !lists_column(definition, key) =>
                {
                    return Err(Error::programming(format!(
                        "unique constraint '{name}' of partitioned model '{model_name}' must \
                         include the partition key '{key}'"
                    )));
                }
                TableObjectKind::UniqueIndex { definition, .. }
                    if !lists_column(definition, key) =>
                {
                    return Err(Error::programming(format!(
                        "unique index '{name}' of partitioned model '{model_name}' must include \
                         the partition key '{key}'"
                    )));
                }
                TableObjectKind::Exclusion(_)
                | TableObjectKind::TemporalKey(_)
                | TableObjectKind::Constraint { .. }
                | TableObjectKind::Index { .. }
                | TableObjectKind::UniqueIndex { .. }
                | TableObjectKind::TypedIndex(_)
                | TableObjectKind::SignalTrigger(_)
                | TableObjectKind::FollowDelete(_) => {}
            }
        }
    }
    Ok(())
}

/// Whether a constraint definition starts with `UNIQUE` or `PRIMARY KEY` (any letter case).
fn is_unique_definition(definition: &str) -> bool {
    let head = definition.trim_start();
    ["UNIQUE", "PRIMARY KEY"].iter().any(|keyword| {
        head.get(..keyword.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(keyword))
    })
}

/// Whether `column` appears as an identifier in the first parenthesised group of `definition`
/// (`UNIQUE (a, b) INCLUDE (c)`, `USING btree (a, b) WHERE …`): an unquoted word compares without
/// regard to letter case, a quoted identifier exactly; string literals are skipped.
fn lists_column(definition: &str, column: &str) -> bool {
    let mut depth = 0_u32;
    let mut word = String::new();
    let mut chars = definition.chars().peekable();
    let hit = |word: &mut String| {
        let found = word.eq_ignore_ascii_case(column);
        word.clear();
        found
    };
    while let Some(c) = chars.next() {
        match c {
            '(' => depth = depth.saturating_add(1),
            ')' => {
                if depth > 0 && hit(&mut word) {
                    return true;
                }
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return false;
                }
            }
            _ if depth == 0 => {}
            '\'' => {
                // a string literal (`''` is an escaped quote inside it)
                while let Some(q) = chars.next() {
                    if q == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                word.clear();
            }
            '"' => {
                let mut quoted = String::new();
                while let Some(q) = chars.next() {
                    if q == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            quoted.push('"');
                        } else {
                            break;
                        }
                    } else {
                        quoted.push(q);
                    }
                }
                if quoted == column {
                    return true;
                }
            }
            c if c.is_ascii_alphanumeric() || c == '_' || c == '$' => word.push(c),
            _ => {
                if hit(&mut word) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_lists() {
        assert!(lists_column("UNIQUE (code, create_date)", "create_date"));
        assert!(lists_column("unique(Create_Date)", "create_date"));
        assert!(lists_column("(code, \"create_date\")", "create_date"));
        assert!(!lists_column("(code, \"Create_Date\")", "create_date"));
        assert!(lists_column(
            "USING btree (code, create_date) WHERE x",
            "create_date"
        ));
        // INCLUDE columns, a WHERE clause, a longer word and a literal are not key columns
        assert!(!lists_column(
            "UNIQUE (code) INCLUDE (create_date)",
            "create_date"
        ));
        assert!(!lists_column(
            "(code) WHERE create_date > now()",
            "create_date"
        ));
        assert!(!lists_column("(code, create_date_2)", "create_date"));
        assert!(!lists_column("(code, 'create_date')", "create_date"));
        assert!(!lists_column("(code)", "create_date"));
        assert!(!lists_column("", "create_date"));
        assert!(is_unique_definition("  Primary Key (id)"));
        assert!(is_unique_definition("UNIQUE(a)"));
        assert!(!is_unique_definition("CHECK(a > 0)"));
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime {
        DateTime::from_parts(Date::from_ymd(y, m, d).unwrap(), h, min, 7, 123).unwrap()
    }

    #[test]
    fn interval_starts() {
        let t = at(2026, 10, 18, 13, 45); // a Sunday
        assert_eq!(
            PartitionInterval::Hour.start_of(t).unwrap().to_string(),
            "2026-10-18 13:00:00"
        );
        assert_eq!(
            PartitionInterval::Day.start_of(t).unwrap().to_string(),
            "2026-10-18 00:00:00"
        );
        assert_eq!(
            PartitionInterval::Week.start_of(t).unwrap().to_string(),
            "2026-10-12 00:00:00"
        );
        assert_eq!(
            PartitionInterval::Month.start_of(t).unwrap().to_string(),
            "2026-10-01 00:00:00"
        );
        // before the epoch (div_euclid, not truncation toward zero)
        let old = at(1969, 12, 31, 23, 59);
        assert_eq!(
            PartitionInterval::Day.start_of(old).unwrap().to_string(),
            "1969-12-31 00:00:00"
        );
        assert_eq!(
            PartitionInterval::Week.start_of(old).unwrap().to_string(),
            "1969-12-29 00:00:00"
        );
    }

    #[test]
    fn shifting_crosses_months_and_years() {
        let jan = PartitionInterval::Month
            .start_of(at(2026, 1, 9, 0, 0))
            .unwrap();
        assert_eq!(
            PartitionInterval::Month.shift(jan, -1).unwrap().to_string(),
            "2025-12-01 00:00:00"
        );
        assert_eq!(
            PartitionInterval::Month.shift(jan, 13).unwrap().to_string(),
            "2027-02-01 00:00:00"
        );
        let day = PartitionInterval::Day
            .start_of(at(2026, 2, 28, 5, 0))
            .unwrap();
        assert_eq!(
            PartitionInterval::Day.next(day).unwrap().to_string(),
            "2026-03-01 00:00:00"
        );
        // overflow is None, never a wrapped value
        assert_eq!(
            PartitionInterval::Hour.shift(DateTime::from_micros(i64::MAX - 1), 1),
            None
        );
        assert_eq!(PartitionInterval::Month.shift(jan, i64::MAX), None);
        assert_eq!(
            PartitionInterval::Day.start_of(DateTime::from_micros(i64::MIN)),
            None
        );
    }

    #[test]
    fn suffixes_round_trip() {
        let t = at(2026, 10, 16, 3, 30);
        for (interval, want) in [
            (PartitionInterval::Hour, "2026101603"),
            (PartitionInterval::Day, "20261016"),
            (PartitionInterval::Week, "20261012"),
            (PartitionInterval::Month, "202610"),
        ] {
            let start = interval.start_of(t).unwrap();
            let suffix = interval.suffix(start).unwrap();
            assert_eq!(suffix, want);
            assert_eq!(interval.parse_suffix(&suffix), Some(start));
        }
        // a month suffix is not a day suffix, a Tuesday is not a week start, junk is refused
        assert_eq!(PartitionInterval::Day.parse_suffix("202610"), None);
        assert_eq!(PartitionInterval::Week.parse_suffix("20261013"), None);
        assert_eq!(PartitionInterval::Day.parse_suffix("20261340"), None);
        assert_eq!(PartitionInterval::Day.parse_suffix("2026-1016"), None);
        // year 0 is never produced by `suffix` and is refused by `parse_suffix`
        for interval in [
            PartitionInterval::Hour,
            PartitionInterval::Day,
            PartitionInterval::Week,
            PartitionInterval::Month,
        ] {
            let zero = match interval {
                PartitionInterval::Hour => "0000010100",
                PartitionInterval::Day | PartitionInterval::Week => "00000101",
                PartitionInterval::Month => "000001",
            };
            assert_eq!(interval.parse_suffix(zero), None, "{interval:?}");
        }
        assert_eq!(
            PartitionInterval::Hour.parse_suffix("２０２６１０１６０３"),
            None
        );
        assert_eq!(
            partition_name("scale_event", PartitionInterval::Day, t),
            Some("scale_event_p20261016".to_owned())
        );
        // no name for a year PostgreSQL's ISO format cannot carry in four digits
        let far = midnight(Date::from_ymd(10_000, 1, 1).unwrap()).unwrap();
        assert_eq!(PartitionInterval::Day.suffix(far), None);
    }

    #[test]
    fn counts_in_words() {
        assert_eq!(PartitionInterval::Day.count(1), "1 day");
        assert_eq!(PartitionInterval::Day.count(2), "2 days");
        assert_eq!(PartitionInterval::Month.count(0), "0 months");
    }
}
