use std::sync::Arc;

use erp_fields::{Value, ValueMap};

/// Severity of an import message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MessageType {
    /// The import fails: nothing is kept.
    Error,
    /// Informational; the import can still succeed.
    Warning,
}

impl MessageType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// The rows of the data matrix one extracted record spans (both ends included).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RowSpan {
    /// First row.
    pub from: usize,
    /// Last row.
    pub to: usize,
}

impl RowSpan {
    /// Rows `from..=to`.
    pub fn new(from: usize, to: usize) -> Self {
        Self { from, to }
    }
}

/// Built with [`ImportMessage::new`] and the `with_*` methods.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ImportMessage {
    /// `type`.
    pub kind: MessageType,
    /// `rows`; absent on the "too many errors" warning.
    pub rows: Option<RowSpan>,
    /// `record`: index of the extracted record; absent on the "too many errors" warning.
    pub record: Option<usize>,
    /// `field`: technical name of the column's field (`.id` for the record's own database id).
    pub field: Option<String>,
    /// `message`, with the field's label substituted.
    pub message: String,
    /// `moreinfo`: a hint (text), the allowed values (list) or an action (dictionary).
    pub moreinfo: Option<Value>,
    /// `field_name`: the field's label, on conversion errors.
    pub field_name: Option<String>,
    /// `field_path`: the column's path, on reference errors of a file import (`import_file`).
    pub field_path: Option<Vec<String>>,
    /// `field_type`: `name`, `external id` or `database id`, on reference errors of a file import.
    pub field_type: Option<String>,
    /// `value`: the value not found (at most 50 characters), on reference errors of a file import.
    pub value: Option<String>,
}

impl ImportMessage {
    pub fn new(kind: MessageType, message: impl Into<String>) -> Self {
        Self {
            kind,
            rows: None,
            record: None,
            field: None,
            message: message.into(),
            moreinfo: None,
            field_name: None,
            field_path: None,
            field_type: None,
            value: None,
        }
    }

    /// A message about record `record` spanning `rows`.
    pub(crate) fn at(kind: MessageType, rows: RowSpan, record: usize, message: String) -> Self {
        Self::new(kind, message).with_rows(rows).with_record(record)
    }

    /// Set `rows`.
    #[must_use]
    pub fn with_rows(mut self, rows: RowSpan) -> Self {
        self.rows = Some(rows);
        self
    }

    /// Set `record`.
    #[must_use]
    pub fn with_record(mut self, record: usize) -> Self {
        self.record = Some(record);
        self
    }

    /// Set `field`.
    #[must_use]
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Set `moreinfo`.
    #[must_use]
    pub fn with_moreinfo(mut self, moreinfo: Value) -> Self {
        self.moreinfo = Some(moreinfo);
        self
    }

    /// Set `field_name`.
    #[must_use]
    pub fn with_field_name(mut self, label: impl Into<String>) -> Self {
        self.field_name = Some(label.into());
        self
    }

    /// Set `field_path`.
    #[must_use]
    pub fn with_field_path<S: Into<String>>(mut self, path: impl IntoIterator<Item = S>) -> Self {
        self.field_path = Some(path.into_iter().map(Into::into).collect());
        self
    }

    /// Set `field_type`.
    #[must_use]
    pub fn with_field_type(mut self, kind: impl Into<String>) -> Self {
        self.field_type = Some(kind.into());
        self
    }

    /// Set `value`.
    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    pub fn to_value(&self) -> Value {
        let mut map = ValueMap::new();
        map.insert("type".to_owned(), Value::str(self.kind.as_str()));
        if let Some(rows) = self.rows {
            let mut span = ValueMap::new();
            span.insert("from".to_owned(), Value::Int(index(rows.from)));
            span.insert("to".to_owned(), Value::Int(index(rows.to)));
            map.insert("rows".to_owned(), Value::Map(Arc::new(span)));
        }
        if let Some(record) = self.record {
            map.insert("record".to_owned(), Value::Int(index(record)));
        }
        if let Some(field) = &self.field {
            map.insert("field".to_owned(), Value::str(field));
        }
        map.insert("message".to_owned(), Value::str(&self.message));
        if let Some(more) = &self.moreinfo {
            map.insert("moreinfo".to_owned(), more.clone());
        }
        if let Some(name) = &self.field_name {
            map.insert("field_name".to_owned(), Value::str(name));
        }
        if let Some(path) = &self.field_path {
            map.insert(
                "field_path".to_owned(),
                Value::list(path.iter().map(Value::str)),
            );
        }
        if let Some(kind) = &self.field_type {
            map.insert("field_type".to_owned(), Value::str(kind));
        }
        if let Some(value) = &self.value {
            map.insert("value".to_owned(), Value::str(value));
        }
        Value::Map(Arc::new(map))
    }
}

/// Row and record indexes as JSON integers. A matrix cannot have more than `i64::MAX` rows on
/// any supported platform (usize is at most 64 bits and slices hold at most `isize::MAX` items),
/// so the conversion never saturates in practice.
fn index(i: usize) -> i64 {
    i64::try_from(i).unwrap_or(i64::MAX)
}

/// The result of `load`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct LoadResult {
    pub ids: Option<Vec<i64>>,
    /// Errors and warnings, in the order they were found.
    pub messages: Vec<ImportMessage>,
    /// The row to resume from when `_import_limit` stopped the import early, else 0.
    pub nextrow: usize,
}

impl LoadResult {
    /// Whether any message is an error.
    pub fn has_errors(&self) -> bool {
        self.messages.iter().any(|m| m.kind == MessageType::Error)
    }

    pub fn to_value(&self) -> Value {
        let mut map = ValueMap::new();
        let ids = match &self.ids {
            Some(ids) => Value::list(ids.iter().map(|id| Value::Int(*id))),
            None => Value::False,
        };
        map.insert("ids".to_owned(), ids);
        map.insert(
            "messages".to_owned(),
            Value::list(self.messages.iter().map(ImportMessage::to_value)),
        );
        map.insert("nextrow".to_owned(), Value::Int(index(self.nextrow)));
        Value::Map(Arc::new(map))
    }
}
