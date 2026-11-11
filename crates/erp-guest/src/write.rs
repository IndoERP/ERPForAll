//! Field assignments collected in the guest and sent as set-based writes.
//!
//! Guest code assigns values record by record; nothing crosses the boundary until the buffer is
//! turned into [`Writes`]. Records that received the same set of fields form one group, and each
//! group becomes one `write-columns` call (one typed column per field, one value per id). A value
//! assigned to every record at once with [`WriteBuffer::set_all`] becomes one `write-batch` call,
//! which is also the only way to send `x2many` commands. A field of a group whose values are all
//! empty is sent as `write-batch` of an empty value, because an all-empty column has no type.

use std::collections::BTreeSet;

use crate::error::{Result, error};
use crate::{BoundaryErrorKind, ColumnData, ColumnVector, RecordId, ScalarValue};

/// One `write-batch` call: the same values for every id.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchWrite {
    pub ids: Vec<RecordId>,
    pub values: Vec<(String, ScalarValue)>,
}

/// One `write-columns` call: one column per field, one value per id in the order of `ids`.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnWrite {
    pub ids: Vec<RecordId>,
    pub columns: Vec<ColumnVector>,
}

/// The calls a [`WriteBuffer`] turns into, in the order they must be sent: batch writes first, then
/// column writes. Within a record, a value assigned with [`WriteBuffer::set`] after
/// [`WriteBuffer::set_all`] for the same field therefore wins, as it would in a single write.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Writes {
    pub model: String,
    pub batches: Vec<BatchWrite>,
    pub columns: Vec<ColumnWrite>,
}

impl Writes {
    /// Whether there is nothing to send.
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty() && self.columns.is_empty()
    }
}

/// Assignments to records of one model, held until they are sent.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteBuffer {
    model: String,
    /// Same value for many ids, in assignment order.
    batches: Vec<BatchWrite>,
    /// Per-record values: records in first-assignment order, fields in first-assignment order.
    records: Vec<(RecordId, Vec<(String, ScalarValue)>)>,
}

impl WriteBuffer {
    /// An empty buffer for records of `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            batches: Vec::new(),
            records: Vec::new(),
        }
    }

    /// Model of the records.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Whether nothing was assigned.
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty() && self.records.is_empty()
    }

    /// Assign `value` to `field` of record `id`. Assigning the same field of the same record again
    /// replaces the earlier value, as a second write would.
    ///
    /// `x2many` commands are refused here with `invalid-argument`: they have no column form; send
    /// them with [`WriteBuffer::set_all`].
    pub fn set(
        &mut self,
        id: RecordId,
        field: impl Into<String>,
        value: ScalarValue,
    ) -> Result<()> {
        let field = field.into();
        if matches!(value, ScalarValue::ValX2many(_)) {
            return Err(error(
                BoundaryErrorKind::InvalidArgument,
                format!(
                    "field '{field}' of model '{}': x2many commands are not written per record; use set_all",
                    self.model
                ),
            ));
        }
        let position = match self.records.iter().position(|(record, _)| *record == id) {
            Some(position) => position,
            None => {
                self.records.push((id, Vec::new()));
                self.records.len() - 1
            }
        };
        let values = &mut self.records[position].1;
        match values.iter_mut().find(|(name, _)| *name == field) {
            Some(slot) => slot.1 = value,
            None => values.push((field, value)),
        }
        Ok(())
    }

    /// Assign the same `values` to every record of `ids` in one `write-batch` call.
    pub fn set_all(&mut self, ids: &[RecordId], values: Vec<(String, ScalarValue)>) -> Result<()> {
        for (position, (field, _)) in values.iter().enumerate() {
            if values[..position]
                .iter()
                .any(|(earlier, _)| earlier == field)
            {
                return Err(error(
                    BoundaryErrorKind::InvalidArgument,
                    format!(
                        "field '{field}' of model '{}' is assigned twice in one batch",
                        self.model
                    ),
                ));
            }
        }
        if !ids.is_empty() && !values.is_empty() {
            self.batches.push(BatchWrite {
                ids: ids.to_vec(),
                values,
            });
        }
        Ok(())
    }

    /// Turn the assignments into the calls that send them. A value that does not fit its column
    /// (values of two kinds for one field, an empty x2many value) is refused with `invalid-argument`
    /// naming the field, and nothing is sent.
    pub fn into_writes(self) -> Result<Writes> {
        let WriteBuffer {
            model,
            batches: mut batch_writes,
            records,
        } = self;
        // Records that assigned the same fields form one group, in first-record order.
        let mut groups: Vec<(BTreeSet<String>, Vec<usize>)> = Vec::new();
        for (index, (_, values)) in records.iter().enumerate() {
            let fields: BTreeSet<String> = values.iter().map(|(field, _)| field.clone()).collect();
            match groups
                .iter_mut()
                .find(|(group_fields, _)| *group_fields == fields)
            {
                Some((_, members)) => members.push(index),
                None => groups.push((fields, vec![index])),
            }
        }
        let mut column_writes = Vec::new();
        for (fields, members) in groups {
            let ids: Vec<RecordId> = members.iter().map(|&index| records[index].0).collect();
            let mut columns = Vec::new();
            let mut empty_fields = Vec::new();
            for field in fields {
                let values: Vec<&ScalarValue> = members
                    .iter()
                    .filter_map(|&index| {
                        records[index]
                            .1
                            .iter()
                            .find(|(name, _)| *name == field)
                            .map(|(_, value)| value)
                    })
                    .collect();
                match column_of(&model, &field, &values)? {
                    Some(data) => columns.push(ColumnVector { field, data }),
                    None => empty_fields.push((field, ScalarValue::ValNull)),
                }
            }
            if !empty_fields.is_empty() {
                batch_writes.push(BatchWrite {
                    ids: ids.clone(),
                    values: empty_fields,
                });
            }
            if !columns.is_empty() {
                column_writes.push(ColumnWrite { ids, columns });
            }
        }
        Ok(Writes {
            model,
            batches: batch_writes,
            columns: column_writes,
        })
    }
}

/// The column holding `values`, or `None` when every value is empty.
fn column_of(model: &str, field: &str, values: &[&ScalarValue]) -> Result<Option<ColumnData>> {
    let Some(first) = values
        .iter()
        .find(|value| !matches!(value, ScalarValue::ValNull))
    else {
        return Ok(None);
    };
    let has_null = values
        .iter()
        .any(|value| matches!(value, ScalarValue::ValNull));
    let mismatch = |value: &ScalarValue| {
        error(
            BoundaryErrorKind::InvalidArgument,
            format!(
                "field '{field}' of model '{model}' mixes {} and {} values",
                scalar_name(first),
                scalar_name(value)
            ),
        )
    };
    macro_rules! collect {
        ($case:ident, $required:ident, $optional:ident) => {{
            let mut required = Vec::with_capacity(values.len());
            let mut optional = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    ScalarValue::$case(inner) => {
                        required.push(inner.clone());
                        optional.push(Some(inner.clone()));
                    }
                    ScalarValue::ValNull => optional.push(None),
                    other => return Err(mismatch(other)),
                }
            }
            if has_null {
                ColumnData::$optional(optional)
            } else {
                ColumnData::$required(required)
            }
        }};
        ($case:ident, $optional:ident) => {{
            let mut optional = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    ScalarValue::$case(inner) => optional.push(Some(inner.clone())),
                    ScalarValue::ValNull => optional.push(None),
                    other => return Err(mismatch(other)),
                }
            }
            ColumnData::$optional(optional)
        }};
    }
    let data = match first {
        ScalarValue::ValBool(_) => collect!(ValBool, ColBool, ColOptBool),
        ScalarValue::ValInt(_) => collect!(ValInt, ColInt, ColOptInt),
        ScalarValue::ValFloat(_) => collect!(ValFloat, ColFloat, ColOptFloat),
        ScalarValue::ValStr(_) => collect!(ValStr, ColString, ColOptString),
        ScalarValue::ValDecimal(_) => collect!(ValDecimal, ColOptDecimal),
        ScalarValue::ValDate(_) => collect!(ValDate, ColOptDate),
        ScalarValue::ValDatetime(_) => collect!(ValDatetime, ColOptDatetime),
        ScalarValue::ValDateRange(_) => collect!(ValDateRange, ColOptDateRange),
        ScalarValue::ValDatetimeRange(_) => collect!(ValDatetimeRange, ColOptDatetimeRange),
        ScalarValue::ValId(_) => collect!(ValId, ColOptId),
        ScalarValue::ValJson(_) => collect!(ValJson, ColOptJson),
        ScalarValue::ValBytes(_) => collect!(ValBytes, ColOptBytes),
        ScalarValue::ValIds(_) => {
            let mut lists = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    ScalarValue::ValIds(ids) => lists.push(ids.clone()),
                    ScalarValue::ValNull => {
                        return Err(error(
                            BoundaryErrorKind::InvalidArgument,
                            format!(
                                "field '{field}' of model '{model}': an x2many value is a list of ids, never empty; \
                                 assign an empty list instead"
                            ),
                        ));
                    }
                    other => return Err(mismatch(other)),
                }
            }
            ColumnData::ColIds(lists)
        }
        // `set` refuses x2many commands and null was skipped above.
        ScalarValue::ValX2many(_) | ScalarValue::ValNull => {
            return Err(error(
                BoundaryErrorKind::InternalError,
                format!(
                    "field '{field}' of model '{model}': value without a column form reached the buffer"
                ),
            ));
        }
    };
    Ok(Some(data))
}

/// The case name of `value` in the contract, for messages.
fn scalar_name(value: &ScalarValue) -> &'static str {
    match value {
        ScalarValue::ValNull => "val-null",
        ScalarValue::ValBool(_) => "val-bool",
        ScalarValue::ValInt(_) => "val-int",
        ScalarValue::ValFloat(_) => "val-float",
        ScalarValue::ValDecimal(_) => "val-decimal",
        ScalarValue::ValStr(_) => "val-str",
        ScalarValue::ValDate(_) => "val-date",
        ScalarValue::ValDatetime(_) => "val-datetime",
        ScalarValue::ValDateRange(_) => "val-date-range",
        ScalarValue::ValDatetimeRange(_) => "val-datetime-range",
        ScalarValue::ValId(_) => "val-id",
        ScalarValue::ValIds(_) => "val-ids",
        ScalarValue::ValX2many(_) => "val-x2many",
        ScalarValue::ValJson(_) => "val-json",
        ScalarValue::ValBytes(_) => "val-bytes",
    }
}
