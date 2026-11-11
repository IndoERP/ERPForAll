//! Values and columnar results of `_read_group` (GRP §4.6).

use std::sync::Arc;

use erp_fields::{Date, DateTime, Decimal, Error, RecId, Result, Value};

use crate::records::Records;

/// One value of a grouped result: a group value or an aggregate.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupValue {
    /// Python `None`: a NULL item of an `array_agg` list.
    Null,
    False,
    /// A boolean.
    Bool(bool),
    /// An integer (counts, integer columns, date parts).
    Int(i64),
    /// A float.
    Float(f64),
    /// An exact decimal (`numeric` columns and sums over them).
    Decimal(Decimal),
    /// Text (char, selection keys …).
    Str(Arc<str>),
    /// A date or a date bucket start.
    Date(Date),
    /// A naive datetime (bucket starts are local wall-clock times in the context timezone).
    DateTime(DateTime),
    /// A JSON value.
    Json(Arc<erp_fields::serde_json::Value>),
    /// A relational group value (one record, or empty) or a `recordset` aggregate.
    Records(Records),
    /// An `array_agg` list.
    List(Vec<GroupValue>),
}

impl GroupValue {
    /// A string value.
    pub fn str(s: impl AsRef<str>) -> Self {
        Self::Str(Arc::from(s.as_ref()))
    }

    /// Python truthiness (an empty recordset or list is falsy).
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Null | Self::False => false,
            Self::Bool(b) => *b,
            Self::Int(i) => *i != 0,
            Self::Float(f) => *f != 0.0,
            Self::Decimal(d) => !d.is_zero(),
            Self::Str(s) => !s.is_empty(),
            Self::Date(_) | Self::DateTime(_) => true,
            Self::Json(j) => erp_fields::convert::json_truthy(j),
            Self::Records(r) => !r.is_empty(),
            Self::List(l) => !l.is_empty(),
        }
    }

    /// The recordset of a relational value.
    pub fn as_records(&self) -> Option<&Records> {
        match self {
            Self::Records(r) => Some(r),
            _ => None,
        }
    }

    /// The single record id of a relational group value (`None` for the empty group).
    pub fn record_id(&self) -> Option<RecId> {
        match self {
            Self::Records(r) if r.len() == 1 => r.rec_ids().first().copied(),
            _ => None,
        }
    }

    /// The dynamic value, as the public `read_group` returns it: recordsets become their id
    /// (`False` when empty) with [`RecordsAs::Id`], their id list with [`RecordsAs::Ids`]. `None`
    /// inside lists becomes `False` (the dynamic layer has one empty value).
    pub fn to_value(&self, records: RecordsAs) -> Value {
        match self {
            Self::Null | Self::False => Value::False,
            Self::Bool(b) => Value::Bool(*b),
            Self::Int(i) => Value::Int(*i),
            Self::Float(f) => Value::Float(*f),
            Self::Decimal(d) => Value::Decimal(*d),
            Self::Str(s) => Value::Str(Arc::clone(s)),
            Self::Date(d) => Value::Date(*d),
            Self::DateTime(d) => Value::DateTime(*d),
            Self::Json(j) => Value::Json(Arc::clone(j)),
            Self::Records(r) => match records {
                RecordsAs::Id => r
                    .rec_ids()
                    .first()
                    .map_or(Value::False, |id| Value::Int(id.get())),
                RecordsAs::Ids => Value::list(r.rec_ids().iter().map(|id| Value::Int(id.get()))),
            },
            Self::List(items) => Value::list(items.iter().map(|v| v.to_value(records))),
        }
    }
}

/// How [`GroupValue::to_value`] renders recordsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordsAs {
    /// A group value: the id, or `False`.
    Id,
    /// An aggregate: the list of ids.
    Ids,
}

impl From<i64> for GroupValue {
    fn from(i: i64) -> Self {
        Self::Int(i)
    }
}

impl From<bool> for GroupValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<&str> for GroupValue {
    fn from(s: &str) -> Self {
        Self::str(s)
    }
}

impl From<f64> for GroupValue {
    fn from(f: f64) -> Self {
        Self::Float(f)
    }
}

impl From<Date> for GroupValue {
    fn from(d: Date) -> Self {
        Self::Date(d)
    }
}

impl From<DateTime> for GroupValue {
    fn from(d: DateTime) -> Self {
        Self::DateTime(d)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Groups {
    groupby: Arc<[String]>,
    aggregates: Arc<[String]>,
    columns: Vec<Vec<GroupValue>>,
    len: usize,
    group_count: Option<u64>,
}

impl Groups {
    pub(crate) fn from_columns(
        groupby: Arc<[String]>,
        aggregates: Arc<[String]>,
        columns: Vec<Vec<GroupValue>>,
        len: usize,
    ) -> Self {
        Self {
            groupby,
            aggregates,
            columns,
            len,
            group_count: None,
        }
    }

    /// A result built from rows (for overrides of `_read_group` that compute their own groups).
    /// Every row holds the group values then the aggregates.
    pub fn from_rows(
        groupby: Vec<String>,
        aggregates: Vec<String>,
        rows: Vec<Vec<GroupValue>>,
    ) -> erp_fields::Result<Self> {
        let width = groupby.len() + aggregates.len();
        let mut columns: Vec<Vec<GroupValue>> =
            (0..width).map(|_| Vec::with_capacity(rows.len())).collect();
        let len = rows.len();
        for row in rows {
            if row.len() != width {
                return Err(erp_fields::Error::programming(format!(
                    "a grouped row has {} values, expected {width}",
                    row.len()
                )));
            }
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        Ok(Self {
            groupby: groupby.into(),
            aggregates: aggregates.into(),
            columns,
            len,
            group_count: None,
        })
    }

    /// Check that a result answers a request: the same group-by and aggregate specs, one column
    /// per spec, every column with one value per group. An override returning anything else is a
    /// programming error (never silently truncated).
    pub fn check(&self, groupby: &[String], aggregates: &[String], method: &str) -> Result<()> {
        if *self.groupby != *groupby || *self.aggregates != *aggregates {
            return Err(Error::programming(format!(
                "{method} returned groups for {:?} / {:?}, asked for {groupby:?} / {aggregates:?}",
                self.groupby, self.aggregates
            )));
        }
        let width = groupby.len() + aggregates.len();
        if self.columns.len() != width || self.columns.iter().any(|c| c.len() != self.len) {
            return Err(Error::programming(format!(
                "{method} returned {} columns of uneven length for {} groups (expected {width})",
                self.columns.len(),
                self.len
            )));
        }
        Ok(())
    }

    /// The group-by specs.
    pub fn groupby(&self) -> &[String] {
        &self.groupby
    }

    /// The aggregate specs.
    pub fn aggregates(&self) -> &[String] {
        &self.aggregates
    }

    /// Number of groups.
    pub fn len(&self) -> usize {
        self.len
    }

    /// No group.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The values of column `i` (group-by specs first, then aggregates).
    pub fn column(&self, i: usize) -> &[GroupValue] {
        self.columns.get(i).map_or(&[], Vec::as_slice)
    }

    pub fn rows(&self) -> Vec<Vec<GroupValue>> {
        (0..self.len)
            .map(|r| {
                self.columns
                    .iter()
                    .map(|c| c.get(r).cloned().unwrap_or(GroupValue::False))
                    .collect()
            })
            .collect()
    }

    /// One row.
    pub fn row(&self, r: usize) -> Option<Vec<GroupValue>> {
        (r < self.len).then(|| {
            self.columns
                .iter()
                .map(|c| c.get(r).cloned().unwrap_or(GroupValue::False))
                .collect()
        })
    }

    /// The number of groups before `limit`/`offset`, when it was asked for
    /// ([`crate::ReadGroup::count_groups`]) and the query returned at least one group.
    pub fn group_count(&self) -> Option<u64> {
        self.group_count
    }

    pub(crate) fn set_group_count(&mut self, count: Option<u64>) {
        self.group_count = count;
    }

    /// The given columns restricted to the given rows (dispatching grouping sets).
    pub(crate) fn take_rows(&self, rows: &[usize], columns: &[usize]) -> Vec<Vec<GroupValue>> {
        columns
            .iter()
            .map(|c| {
                rows.iter()
                    .map(|r| {
                        self.columns
                            .get(*c)
                            .and_then(|col| col.get(*r))
                            .cloned()
                            .unwrap_or(GroupValue::False)
                    })
                    .collect()
            })
            .collect()
    }
}
