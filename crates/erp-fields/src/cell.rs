//! Cells: one field value in its storage representation, and the typed-API value traits.
//!
//! A [`Cell`] is what the record cache and SQL parameters hold. Typed addon code never sees cells;
//! it reads and writes Rust types through [`FieldValue`], whose null mapping follows the FIELD spec
//! and (integer, float and boolean read NULL as `0`, `0.0`, `false`).

use std::fmt;
use std::num::NonZeroI64;
use std::sync::Arc;

use rust_decimal::Decimal;

use crate::error::{Error, Result};
use crate::range::{DateRange, DatetimeRange};
use crate::temporal::{Date, DateTime};
use crate::types::Storage;

/// The id of a database row: always positive. Statements that bind ids take `RealId`s, so an
/// in-memory new-record id can never reach SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RealId(NonZeroI64);

impl RealId {
    /// A database id; `None` unless `id > 0`.
    pub const fn new(id: i64) -> Option<Self> {
        if id > 0 {
            match NonZeroI64::new(id) {
                Some(n) => Some(Self(n)),
                None => None,
            }
        } else {
            None
        }
    }

    /// The raw value.
    pub const fn get(self) -> i64 {
        self.0.get()
    }
}

impl fmt::Display for RealId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A record id: a database row ([`RealId`], positive) or an in-memory new record (negative, only
/// created by a transaction). Zero is never an id.
///
/// Database ids stay below 2^62.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecId(NonZeroI64);

/// Offset of new-record ids without an origin.
const NEW_BASE: i64 = 1 << 62;

impl RecId {
    /// The id of a new record whose origin is the database row `origin`; `None` when the origin is
    /// beyond the id range reserved for origins.
    pub const fn new_from_origin(origin: RealId) -> Option<Self> {
        if origin.get() < NEW_BASE {
            match NonZeroI64::new(-origin.get()) {
                Some(n) => Some(Self(n)),
                None => None,
            }
        } else {
            None
        }
    }

    /// The id of the `n`-th new record without an origin (`n >= 1`).
    pub const fn new_unique(n: i64) -> Option<Self> {
        if n >= 1 && n < NEW_BASE {
            match NonZeroI64::new(-(NEW_BASE + n)) {
                Some(id) => Some(Self(id)),
                None => None,
            }
        } else {
            None
        }
    }

    /// The origin of a new record, if it has one.
    pub const fn origin(self) -> Option<RealId> {
        let v = self.0.get();
        match v.checked_neg() {
            Some(p) if v < 0 && p < NEW_BASE => RealId::new(p),
            _ => None,
        }
    }

    /// The counter value `n` of a new record without origin ([`Self::new_unique`]).
    pub const fn unique_index(self) -> Option<i64> {
        let v = self.0.get();
        match v.checked_neg() {
            Some(p) if v < 0 && p > NEW_BASE => Some(p - NEW_BASE),
            _ => None,
        }
    }

    /// The id of a database row; `None` unless `id > 0`.
    pub const fn from_real(id: i64) -> Option<Self> {
        match RealId::new(id) {
            Some(real) => Some(Self::real(real)),
            None => None,
        }
    }

    /// A database id or an in-memory new-record id held in a cache cell (a many2one pointing at
    /// a new record); `None` for 0. Only the transaction creates new-record ids, so negative
    /// values come from its own cache, never from input.
    pub const fn from_raw(id: i64) -> Option<Self> {
        match NonZeroI64::new(id) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// From a database id.
    pub const fn real(id: RealId) -> Self {
        Self(id.0)
    }

    /// The raw value.
    pub const fn get(self) -> i64 {
        self.0.get()
    }

    /// True for in-memory new records.
    pub const fn is_new(self) -> bool {
        self.0.get() < 0
    }

    /// The database id, unless this is a new record.
    pub const fn as_real(self) -> Option<RealId> {
        if self.is_new() {
            None
        } else {
            Some(RealId(self.0))
        }
    }
}

impl From<RealId> for RecId {
    fn from(id: RealId) -> Self {
        Self::real(id)
    }
}

impl fmt::Display for RecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(origin) = self.origin() {
            write!(f, "<NewId origin={origin}>")
        } else if self.is_new() {
            write!(
                f,
                "<NewId 0x{:x}>",
                self.0.get().unsigned_abs() - NEW_BASE as u64
            )
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// One value in storage form. `Null` is SQL NULL.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    /// `bool`.
    Bool(bool),
    /// `int4`.
    Int(i32),
    /// Record id (`id`, many2one).
    Id(i64),
    /// `float8`.
    Float(f64),
    /// `numeric`.
    Decimal(Decimal),
    /// Text (`varchar`, `text`, selection keys).
    Str(Arc<str>),
    /// `date`.
    Date(Date),
    /// `timestamp`.
    DateTime(DateTime),
    /// Native UUID; nil is not SQL NULL.
    Uuid(crate::Uuid),
    /// Native full-width unsigned transaction id.
    Xid8(crate::Xid8),
    /// Bounded timezone-aware microsecond instant.
    TimestampTz(crate::TimestampTz),
    /// `jsonb`.
    Json(Arc<serde_json::Value>),
    /// Ordered record ids: the value of an x2many field (no column; cache and read format).
    Ids(Arc<[RecId]>),
    /// `daterange`: a range of dates, with the bounds of its field.
    DateRange(DateRange),
    /// `tsrange`: a range of naive-UTC datetimes, with the bounds of its field.
    DatetimeRange(DatetimeRange),
}

impl Cell {
    /// True for `Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// True when this cell can be stored in a column of `storage`.
    pub fn fits(&self, storage: Storage) -> bool {
        matches!(
            (self, storage),
            (Self::Null, _)
                | (Self::Bool(_), Storage::Bool)
                | (Self::Int(_), Storage::Int)
                | (Self::Id(_), Storage::Id)
                | (Self::Float(_), Storage::Float)
                | (Self::Decimal(_), Storage::Decimal)
                | (Self::Str(_), Storage::Str)
                | (Self::Date(_), Storage::Date)
                | (Self::DateTime(_), Storage::DateTime)
                | (Self::Uuid(_), Storage::Uuid)
                | (Self::Xid8(_), Storage::Xid8)
                | (Self::TimestampTz(_), Storage::TimestampTz)
                | (Self::Json(_), Storage::Json)
                | (Self::Ids(_), Storage::Ids)
                | (Self::DateRange(_), Storage::DateRange(_))
                | (Self::DatetimeRange(_), Storage::DatetimeRange(_))
        )
    }

    /// Equality used by "skip unchanged" writes: `0.1 + 0.2 != 0.3`, decimals
    /// compare by value, JSON by structure.
    pub fn same_value(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            _ => self == other,
        }
    }
}

fn type_error(expected: &str, cell: &Cell) -> Error {
    Error::programming(format!("expected a {expected} value, found {cell:?}"))
}

/// A Rust type usable as a typed field value.
pub trait FieldValue: Sized + Send + Sync + 'static {
    /// Read from a cell (with the type's null mapping).
    fn from_cell(cell: &Cell) -> Result<Self>;
    /// Convert into a cell.
    fn into_cell(self) -> Cell;
}

impl FieldValue for bool {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(false),
            Cell::Bool(b) => Ok(*b),
            other => Err(type_error("boolean", other)),
        }
    }
    fn into_cell(self) -> Cell {
        Cell::Bool(self)
    }
}

impl FieldValue for i32 {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(0),
            Cell::Int(v) => Ok(*v),
            other => Err(type_error("integer", other)),
        }
    }
    fn into_cell(self) -> Cell {
        Cell::Int(self)
    }
}

// Optional integer columns distinguish absence from a stored zero.
impl FieldValue for Option<i32> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::Int(value) => Ok(Some(*value)),
            other => Err(type_error("integer", other)),
        }
    }

    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::Int)
    }
}

impl FieldValue for f64 {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(0.0),
            Cell::Float(v) => Ok(*v),
            other => Err(type_error("float", other)),
        }
    }
    fn into_cell(self) -> Cell {
        Cell::Float(self)
    }
}

impl FieldValue for Decimal {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(Decimal::ZERO),
            Cell::Decimal(v) => Ok(*v),
            other => Err(type_error("numeric", other)),
        }
    }
    fn into_cell(self) -> Cell {
        Cell::Decimal(self)
    }
}

impl FieldValue for Option<Arc<str>> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::Str(s) => Ok(Some(Arc::clone(s))),
            other => Err(type_error("text", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::Str)
    }
}

impl FieldValue for Option<String> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        Option::<Arc<str>>::from_cell(cell).map(|s| s.map(|s| s.to_string()))
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, |s| Cell::Str(Arc::from(s)))
    }
}

impl FieldValue for Option<Date> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::Date(d) => Ok(Some(*d)),
            other => Err(type_error("date", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::Date)
    }
}

impl FieldValue for Option<DateTime> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::DateTime(d) => Ok(Some(*d)),
            other => Err(type_error("datetime", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::DateTime)
    }
}

impl FieldValue for Option<crate::Uuid> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::Uuid(value) => Ok(Some(*value)),
            other => Err(type_error("uuid", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::Uuid)
    }
}

impl FieldValue for Option<crate::Xid8> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::Xid8(value) => Ok(Some(*value)),
            other => Err(type_error("xid8", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::Xid8)
    }
}

impl FieldValue for Option<crate::TimestampTz> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::TimestampTz(value) => Ok(Some(*value)),
            other => Err(type_error("timestamptz", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::TimestampTz)
    }
}

impl FieldValue for Option<serde_json::Value> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            // A fresh, independent value per read.
            Cell::Json(v) => Ok(Some(serde_json::Value::clone(v))),
            other => Err(type_error("json", other)),
        }
    }
    fn into_cell(self) -> Cell {
        crate::convert::json_cell(self)
    }
}

impl FieldValue for Option<DateRange> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::DateRange(r) => Ok(Some(*r)),
            other => Err(type_error("date range", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::DateRange)
    }
}

impl FieldValue for Option<DatetimeRange> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            Cell::DatetimeRange(r) => Ok(Some(*r)),
            other => Err(type_error("datetime range", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, Cell::DatetimeRange)
    }
}

impl FieldValue for Vec<RecId> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(Vec::new()),
            Cell::Ids(ids) => Ok(ids.to_vec()),
            other => Err(type_error("record ids", other)),
        }
    }
    /// Written as a `SET` of these ids (REL §4.8.2).
    fn into_cell(self) -> Cell {
        Cell::Ids(self.into())
    }
}

impl FieldValue for i64 {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(0),
            Cell::Id(v) => Ok(*v),
            other => Err(type_error("record id", other)),
        }
    }
    /// `0` is the empty value (Integer semantics of `Many2oneReference`, REL §4.11).
    fn into_cell(self) -> Cell {
        if self == 0 {
            Cell::Null
        } else {
            Cell::Id(self)
        }
    }
}

impl FieldValue for Option<RecId> {
    fn from_cell(cell: &Cell) -> Result<Self> {
        match cell {
            Cell::Null => Ok(None),
            // a new record's many2one may point at another new record (ENV §4.9)
            Cell::Id(id) => Ok(RecId::from_raw(*id)),
            other => Err(type_error("record id", other)),
        }
    }
    fn into_cell(self) -> Cell {
        self.map_or(Cell::Null, |id| Cell::Id(id.get()))
    }
}

/// Typed translations map for a translated field.
/// Scalar field readers keep returning the projected string; full maps use an explicit accessor.
///
/// ```
/// use erp_fields::{Cell, FieldTranslations, FieldValue};
/// let values = FieldTranslations::from_cell(&Cell::Str("Base".into()))?;
/// assert_eq!(values.get("en_US"), Some("Base"));
/// assert_eq!(values.len(), 1);
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct FieldTranslations(std::collections::BTreeMap<String, String>);

impl FieldTranslations {
    /// Build a typed map. All entries are strings by construction.
    pub fn new(values: std::collections::BTreeMap<String, String>) -> Self {
        Self(values)
    }
    /// A language's stored text, without fallback.
    pub fn get(&self, lang: &str) -> Option<&str> {
        self.0.get(lang).map(String::as_str)
    }
    /// Whether there are no stored translations.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Number of stored translations.
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Stored translations in language-code order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
    /// Consume the wrapper.
    pub fn into_inner(self) -> std::collections::BTreeMap<String, String> {
        self.0
    }
    /// Validate the entire stored representation without cloning it.
    ///
    /// # Errors
    /// Returns `Programming` for a non-map JSON cell or any non-string entry.
    pub fn validate_cell(cell: &Cell) -> Result<()> {
        match cell {
            Cell::Null | Cell::Str(_) => Ok(()),
            Cell::Json(j) => {
                let serde_json::Value::Object(map) = j.as_ref() else {
                    return Err(Error::programming(
                        "translated cell must be a language map of strings",
                    ));
                };
                for (lang, value) in map {
                    if !value.is_string() {
                        return Err(Error::programming(format!(
                            "expected a string translation for language '{lang}'"
                        )));
                    }
                }
                Ok(())
            }
            _ => Err(Error::programming(
                "translated cell must be a language map of strings",
            )),
        }
    }
}

impl FieldValue for FieldTranslations {
    fn from_cell(cell: &Cell) -> Result<Self> {
        Self::validate_cell(cell)?;
        match cell {
            Cell::Null => Ok(Self(std::collections::BTreeMap::new())),
            Cell::Json(j) => match j.as_ref() {
                serde_json::Value::Object(map) => {
                    let mut btree = std::collections::BTreeMap::new();
                    for (k, v) in map {
                        let Some(s) = v.as_str() else {
                            return Err(Error::programming(format!(
                                "expected a string translation for language '{k}'"
                            )));
                        };
                        btree.insert(k.clone(), s.to_owned());
                    }
                    Ok(Self(btree))
                }
                _ => Err(type_error("language map", cell)),
            },
            Cell::Str(s) => {
                let mut btree = std::collections::BTreeMap::new();
                btree.insert("en_US".to_string(), s.to_string());
                Ok(Self(btree))
            }
            other => Err(type_error("language map", other)),
        }
    }

    fn into_cell(self) -> Cell {
        let mut map = serde_json::Map::new();
        for (k, v) in self.0 {
            map.insert(k, serde_json::Value::String(v));
        }
        Cell::Json(Arc::new(serde_json::Value::Object(map)))
    }
}

/// Kind markers used by the `#[field]` macro to check at compile time that a field's Rust type
/// matches its kind (`ValueFor<kinds::Char>` is implemented for the text types only, and so on).
pub mod kinds {
    /// `Boolean`.
    pub enum Boolean {}
    /// `Integer`.
    pub enum Integer {}
    /// `Float` without digits.
    pub enum Float {}
    /// `Float` with digits, and `Monetary`.
    pub enum Numeric {}
    /// `Char`, `Text`, `Html`, `Selection`.
    pub enum Text {}
    /// `Date`.
    pub enum Date {}
    /// `Datetime`.
    pub enum Datetime {}
    /// Native UUID.
    pub enum Uuid {}
    /// Native full-width xid8.
    pub enum Xid8 {}
    /// Native timezone-aware timestamp.
    pub enum TimestampTz {}
    /// `Json`.
    pub enum Json {}
    /// `Many2one` (the raw id; records through `Records::related`).
    pub enum Many2one {}
    /// `One2many`, `Many2many` (ordered ids; writes take commands).
    pub enum X2many {}
    /// `Many2oneReference` (an id, 0 when empty).
    pub enum Many2oneReference {}
    /// `date_range`.
    pub enum DateRange {}
    /// `datetime_range`.
    pub enum DatetimeRange {}
}

/// Implemented for the Rust types that may hold values of kind `K`.
pub trait ValueFor<K>: FieldValue {}

impl ValueFor<kinds::Boolean> for bool {}
impl ValueFor<kinds::Integer> for i32 {}
impl ValueFor<kinds::Integer> for Option<i32> {}
impl ValueFor<kinds::Float> for f64 {}
impl ValueFor<kinds::Numeric> for Decimal {}
impl ValueFor<kinds::Text> for Option<Arc<str>> {}
impl ValueFor<kinds::Text> for Option<String> {}
impl ValueFor<kinds::Date> for Option<Date> {}
impl ValueFor<kinds::Datetime> for Option<DateTime> {}
impl ValueFor<kinds::Uuid> for Option<crate::Uuid> {}
impl ValueFor<kinds::Xid8> for Option<crate::Xid8> {}
impl ValueFor<kinds::TimestampTz> for Option<crate::TimestampTz> {}
impl ValueFor<kinds::Json> for Option<serde_json::Value> {}
impl ValueFor<kinds::Many2one> for Option<RecId> {}
impl ValueFor<kinds::X2many> for Vec<RecId> {}
impl ValueFor<kinds::Many2oneReference> for i64 {}
impl ValueFor<kinds::DateRange> for Option<DateRange> {}
impl ValueFor<kinds::DatetimeRange> for Option<DatetimeRange> {}

/// `model_ref!` writes them into `describe()` so the installer can check, against
/// the registry, that the field a typed handle names exists and holds what the handle says.
pub trait RefType: FieldValue {
    /// The `ttype` names of the fields that can hold this type, e.g. `["char", "text", "html",
    /// "selection"]` for optional text.
    fn field_kinds() -> &'static [&'static str];

    /// The model a relational field must point to; `None` for a plain value.
    fn comodel() -> Option<&'static str> {
        None
    }
}

macro_rules! ref_type {
    ($($ty:ty => [$($kind:literal),+]),+ $(,)?) => {
        $(impl RefType for $ty {
            fn field_kinds() -> &'static [&'static str] {
                &[$($kind),+]
            }
        })+
    };
}

ref_type! {
    bool => ["boolean"],
    i32 => ["integer"],
    Option<i32> => ["integer"],
    f64 => ["float"],
    Decimal => ["float", "monetary"],
    Option<Arc<str>> => ["char", "text", "html", "selection"],
    Option<String> => ["char", "text", "html", "selection"],
    Option<Date> => ["date"],
    Option<DateTime> => ["datetime"],
    Option<serde_json::Value> => ["json"],
    Option<RecId> => ["many2one"],
    Vec<RecId> => ["one2many", "many2many"],
    i64 => ["many2one_reference"],
    Option<DateRange> => ["date_range"],
    Option<DatetimeRange> => ["datetime_range"],
}

/// Values accepted where a typed field of type `T` is written: `T` itself, plus `&str`/`String`
/// for text fields and plain values for optional ones.
pub trait IntoFieldValue<T> {
    /// Convert.
    fn into_field_value(self) -> T;
}

impl<T: FieldValue> IntoFieldValue<T> for T {
    fn into_field_value(self) -> T {
        self
    }
}

impl IntoFieldValue<Option<Arc<str>>> for &str {
    fn into_field_value(self) -> Option<Arc<str>> {
        Some(Arc::from(self))
    }
}

impl IntoFieldValue<Option<Arc<str>>> for String {
    fn into_field_value(self) -> Option<Arc<str>> {
        Some(Arc::from(self))
    }
}

impl IntoFieldValue<Option<String>> for &str {
    fn into_field_value(self) -> Option<String> {
        Some(self.to_owned())
    }
}

impl IntoFieldValue<Option<Date>> for Date {
    fn into_field_value(self) -> Option<Date> {
        Some(self)
    }
}

impl IntoFieldValue<Option<DateTime>> for DateTime {
    fn into_field_value(self) -> Option<DateTime> {
        Some(self)
    }
}

impl IntoFieldValue<Option<DateRange>> for DateRange {
    fn into_field_value(self) -> Option<DateRange> {
        Some(self)
    }
}

impl IntoFieldValue<Option<DatetimeRange>> for DatetimeRange {
    fn into_field_value(self) -> Option<DatetimeRange> {
        Some(self)
    }
}

impl IntoFieldValue<Option<RecId>> for RecId {
    fn into_field_value(self) -> Option<RecId> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_mapping() {
        assert!(!bool::from_cell(&Cell::Null).unwrap());
        assert_eq!(i32::from_cell(&Cell::Null).unwrap(), 0);
        assert_eq!(f64::from_cell(&Cell::Null).unwrap(), 0.0);
        assert_eq!(Decimal::from_cell(&Cell::Null).unwrap(), Decimal::ZERO);
        assert_eq!(Option::<Arc<str>>::from_cell(&Cell::Null).unwrap(), None);
        assert_eq!(Option::<Date>::from_cell(&Cell::Null).unwrap(), None);
        assert_eq!(
            Option::<serde_json::Value>::from_cell(&Cell::Null).unwrap(),
            None
        );
    }

    #[test]
    fn optional_integer_preserves_absence_and_rejects_other_types() {
        assert_eq!(Option::<i32>::from_cell(&Cell::Null).unwrap(), None);
        assert_eq!(Option::<i32>::from_cell(&Cell::Int(0)).unwrap(), Some(0));
        assert_eq!(
            Option::<i32>::from_cell(&Cell::Int(840)).unwrap(),
            Some(840)
        );
        assert_eq!(None::<i32>.into_cell(), Cell::Null);
        assert_eq!(Some(840_i32).into_cell(), Cell::Int(840));
        let error = Option::<i32>::from_cell(&Cell::Bool(false)).unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "expected a integer value, found Bool(false)"
        );
    }

    #[test]
    fn rec_id_rejects_zero() {
        assert!(RecId::from_real(0).is_none());
        assert!(
            RecId::from_real(-3).is_none(),
            "only the transaction creates new-record ids"
        );
        assert_eq!(RecId::from_real(7).unwrap().to_string(), "7");
        assert_eq!(RecId::from_real(7).unwrap().as_real(), RealId::new(7));
        assert!(RealId::new(-1).is_none() && RealId::new(0).is_none());
    }

    #[test]
    fn new_record_ids_and_their_bounds() {
        let origin = RealId::new(42).unwrap();
        let with_origin = RecId::new_from_origin(origin).unwrap();
        assert!(with_origin.is_new() && with_origin.as_real().is_none());
        assert_eq!(with_origin.origin(), Some(origin));
        assert_eq!(
            RecId::new_from_origin(origin),
            Some(with_origin),
            "same origin, same id"
        );
        assert_eq!(with_origin.to_string(), "<NewId origin=42>");
        // origins must stay below 2^62
        let top = RealId::new((1 << 62) - 1).unwrap();
        assert!(RecId::new_from_origin(top).is_some());
        assert!(RecId::new_from_origin(RealId::new(1 << 62).unwrap()).is_none());
        assert!(RecId::new_from_origin(RealId::new(i64::MAX).unwrap()).is_none());
        // unique new ids: 1 ..= 2^62 - 1, without origin, distinct from every origin id
        let first = RecId::new_unique(1).unwrap();
        assert!(first.is_new() && first.origin().is_none());
        assert_ne!(first, RecId::new_unique(2).unwrap());
        assert_eq!(first.to_string(), "<NewId 0x1>");
        assert!(RecId::new_unique(0).is_none() && RecId::new_unique(-1).is_none());
        let last = RecId::new_unique((1 << 62) - 1).unwrap();
        assert_eq!(last.get(), i64::MIN + 1);
        assert!(last.origin().is_none());
        assert!(RecId::new_unique(1 << 62).is_none());
    }

    #[test]
    fn mismatched_cells_are_programming_errors() {
        let err = i32::from_cell(&Cell::Str("x".into())).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Programming);
    }
}
