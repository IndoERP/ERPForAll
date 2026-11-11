//! Field types after setup, and their database column types (FIELD §4.5, §4.8;,
//! ).

use std::fmt;

use crate::range::RangeBounds;

/// Precision of a `Float` with `digits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Digits {
    /// `digits=(precision, scale)`: values are rounded half away from zero to `scale` decimals.
    Fixed {
        /// Total digits (informational, reported to the web client).
        precision: u32,
        /// Decimal places kept.
        scale: u32,
    },
    /// `digits=0` or `digits=False`: a `numeric` column without rounding.
    Unrounded,
    /// `digits='Name'`: named decimal precision resolved through `decimal.precision` with fallback (16, 2).
    Named(&'static str),
}

/// Index requested on a field (`index=` attribute).
///
/// Not exhaustive: later slices add kinds, and every property below is defined here, so a new
/// kind is a compile error in this file only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexKind {
    /// `index=True` / `'btree'`.
    Btree,
    /// `'btree_not_null'`: a partial btree index on non-null values.
    BtreeNotNull,
    /// `'trigram'`: a GIN trigram index (needs `pg_trgm`).
    Trigram,
    /// `'brin'`: a block-range index, for `date`, `datetime` and `integer` columns whose values
    /// follow the insert order (creation dates of append-mostly tables). Thousands of times
    /// smaller than a btree; useless on columns that are not correlated with the physical order.
    Brin,
    /// `'gin'`: a GIN index with `jsonb_path_ops` on a `json` field, for containment (`@>`) and
    /// JSON path (`@?`, `@@`) conditions.
    Gin,
}

impl IndexKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Btree => "btree",
            Self::BtreeNotNull => "btree_not_null",
            Self::Trigram => "trigram",
            Self::Brin => "brin",
            Self::Gin => "gin",
        }
    }

    /// The kind named `name` in an `index = "<name>"` attribute (`true` is `btree`).
    ///
    /// ```
    /// use erp_fields::IndexKind;
    /// assert_eq!(IndexKind::from_name("brin"), Some(IndexKind::Brin));
    /// assert_eq!(IndexKind::from_name("hash"), None);
    /// ```
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "btree" => Self::Btree,
            "btree_not_null" => Self::BtreeNotNull,
            "trigram" => Self::Trigram,
            "brin" => Self::Brin,
            "gin" => Self::Gin,
            _ => return None,
        })
    }

    /// The PostgreSQL access method of the index (`USING <method>`).
    pub fn method(self) -> &'static str {
        match self {
            Self::Btree | Self::BtreeNotNull => "btree",
            Self::Trigram | Self::Gin => "gin",
            Self::Brin => "brin",
        }
    }

    /// The operator class of the indexed column when it is not the column type's default one.
    pub fn opclass(self) -> Option<&'static str> {
        match self {
            Self::Trigram => Some("gin_trgm_ops"),
            Self::Gin => Some("jsonb_path_ops"),
            Self::Btree | Self::BtreeNotNull | Self::Brin => None,
        }
    }

    /// True when the index is partial on the non-null values of the column.
    pub fn not_null_only(self) -> bool {
        match self {
            Self::BtreeNotNull => true,
            Self::Btree | Self::Trigram | Self::Brin | Self::Gin => false,
        }
    }

    /// The PostgreSQL extension the kind needs, if any.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Self::Trigram => Some("pg_trgm"),
            Self::Btree | Self::BtreeNotNull | Self::Brin | Self::Gin => None,
        }
    }

    /// The registry refuses the kind on other fields.
    ///
    /// ```
    /// use erp_fields::{FieldType, IndexKind};
    /// assert!(IndexKind::Brin.accepts(&FieldType::Datetime));
    /// assert!(!IndexKind::Brin.accepts(&FieldType::Char { size: None }));
    /// assert_eq!(IndexKind::Gin.field_types(), Some(&["json"][..]));
    /// ```
    pub fn field_types(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Brin => Some(&["date", "datetime", "integer"]),
            Self::Gin => Some(&["json"]),
            Self::Btree | Self::BtreeNotNull | Self::Trigram => None,
        }
    }

    /// True when a field of type `ty` may declare this kind (see [`IndexKind::field_types`]).
    pub fn accepts(self, ty: &FieldType) -> bool {
        self.field_types()
            .is_none_or(|types| types.contains(&ty.type_name()))
    }
}

/// `ondelete` of a many2one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnDelete {
    /// `ON DELETE SET NULL`.
    SetNull,
    /// `ON DELETE RESTRICT`.
    Restrict,
    /// `ON DELETE CASCADE`.
    Cascade,
}

impl OnDelete {
    /// SQL action text.
    pub fn sql(self) -> &'static str {
        match self {
            Self::SetNull => "SET NULL",
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
        }
    }
}

/// Selection field `ondelete` policy for selection values added by an extension
/// .
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SelectionOnDelete {
    /// `'set null'`: matching records have their selection value set to None / Null.
    SetNull,
    /// `'cascade'`: matching records are deleted along with their external IDs.
    Cascade,
    /// `'set default'`: matching records have their selection value set to the field's default.
    SetDefault,
    /// `'set <value>'`: matching records have their selection value set to the given value.
    SetValue(String),
    /// Callable: a custom callback invoked with the records.
    Callback(String),
}

impl SelectionOnDelete {
    /// Parse an ondelete policy string.
    pub fn parse(s: &str) -> std::result::Result<Self, ParseSelectionOnDeleteError> {
        let trimmed = s.trim();
        if trimmed == "set null" {
            Ok(Self::SetNull)
        } else if trimmed == "cascade" {
            Ok(Self::Cascade)
        } else if trimmed == "set default" {
            Ok(Self::SetDefault)
        } else if let Some(target) = trimmed.strip_prefix("set ") {
            let target = target.trim();
            if target.is_empty() {
                Err(ParseSelectionOnDeleteError(s.to_owned()))
            } else {
                Ok(Self::SetValue(target.to_owned()))
            }
        } else {
            Err(ParseSelectionOnDeleteError(s.to_owned()))
        }
    }
}

/// Error returned when parsing an unrecognized selection `ondelete` policy string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSelectionOnDeleteError(pub String);

impl std::fmt::Display for ParseSelectionOnDeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unrecognized selection ondelete policy '{}'", self.0)
    }
}

impl std::error::Error for ParseSelectionOnDeleteError {}

/// A field's type with its type-specific parameters, as resolved at registry build.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    /// The record id (`int8` identity column).
    Id,
    /// `Boolean`.
    Boolean,
    /// `Integer` (`int4`).
    Integer,
    /// `Float`: `f64`/`float8` without digits, `Decimal`/`numeric` with digits.
    Float {
        /// `None` when `digits` was not given.
        digits: Option<Digits>,
    },
    /// `Monetary` (`numeric`).
    Monetary {
        /// Name of the currency field.
        currency_field: Option<String>,
    },
    /// `Char`.
    Char {
        /// `size=n` gives `varchar(n)`.
        size: Option<u32>,
    },
    /// `Text`.
    Text,
    /// `Html` (stored as `text`; sanitizing arrives with the sanitizer spec).
    Html,
    Selection {
        /// Static `(value, label)` pairs in order; `None` for a method-backed selection.
        values: Option<Vec<(String, String)>>,
        /// Name of the method returning the values, for a dynamic selection.
        method: Option<String>,
    },
    /// `Date`.
    Date,
    /// `Datetime` (naive UTC).
    Datetime,
    /// Native UUID; text and identity policies are separate from its 128 bits.
    Uuid,
    /// Native unsigned full-width PostgreSQL transaction id.
    Xid8,
    /// Explicit-offset, bounded timezone-aware microsecond instant.
    TimestampTz,
    /// `Json` (`jsonb`).
    Json,
    /// `Many2one`, stored as an `int8` record id.
    Many2one {
        /// Target model name.
        comodel: String,
        /// Database action when the target is deleted.
        ondelete: OnDelete,
    },
    /// `One2many`: no column; the records of `comodel` whose `inverse` many2one points back.
    One2many {
        /// Target model name.
        comodel: String,
        /// The many2one (or many2one_reference) of the comodel pointing back.
        inverse: String,
    },
    /// `Many2many`: no column; pairs in a relation table (`None` for non-stored fields).
    Many2many {
        /// Target model name.
        comodel: String,
        /// The relation table and its columns, when stored.
        relation: Option<M2mRelation>,
        /// Database action on the relation rows when a target is deleted (`cascade` or
        /// `restrict`).
        ondelete: OnDelete,
    },
    /// `Reference`: a `"model,id"` string in a `varchar` column, no foreign key.
    Reference {
        /// Allowed models as `(model, label)` pairs; `None` for a method-backed selection.
        models: Option<Vec<(String, String)>>,
        /// Name of the method returning the allowed models.
        method: Option<String>,
    },
    /// `Many2oneReference`: a record id (`int8`) whose model is in `model_field`.
    Many2oneReference {
        /// The char field holding the model name.
        model_field: String,
    },
    // ----: row versions ----
    /// The `row_version` field of a model declared `row_version = true`: an `int8` bumped by one on
    /// every `UPDATE` of the row (by the ORM and by a `BEFORE UPDATE` trigger for raw SQL). Only the
    /// registry creates it; it is read like an integer and never written by callers.
    RowVersion,
    /// `date_range` (ERPForAll, design §5.2): a range of dates (`daterange`).
    DateRange {
        /// Which ends the field's ranges include.
        bounds: RangeBounds,
    },
    /// `datetime_range` (ERPForAll, design §5.2): a range of naive-UTC datetimes
    /// (`tsrange`).
    DatetimeRange {
        /// Which ends the field's ranges include.
        bounds: RangeBounds,
    },
}

/// The relation table of a stored many2many field (REL §4.7.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct M2mRelation {
    /// Table name.
    pub table: String,
    /// Column holding the ids of the field's model.
    pub column1: String,
    /// Column holding the ids of the comodel.
    pub column2: String,
}

impl FieldType {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Id | Self::Integer | Self::RowVersion => "integer",
            Self::Boolean => "boolean",
            Self::Float { .. } => "float",
            Self::Monetary { .. } => "monetary",
            Self::Char { .. } => "char",
            Self::Text => "text",
            Self::Html => "html",
            Self::Selection { .. } => "selection",
            Self::Date => "date",
            Self::Datetime => "datetime",
            Self::Uuid => "uuid",
            Self::Xid8 => "xid8",
            Self::TimestampTz => "timestamptz",
            Self::Json => "json",
            Self::Many2one { .. } => "many2one",
            Self::One2many { .. } => "one2many",
            Self::Many2many { .. } => "many2many",
            Self::Reference { .. } => "reference",
            Self::Many2oneReference { .. } => "many2one_reference",
            Self::DateRange { .. } => "date_range",
            Self::DatetimeRange { .. } => "datetime_range",
        }
    }

    /// The bounds of a `date_range` or `datetime_range` field.
    pub fn range_bounds(&self) -> Option<RangeBounds> {
        match self {
            Self::DateRange { bounds } | Self::DatetimeRange { bounds } => Some(*bounds),
            _ => None,
        }
    }

    /// The comodel of a many2one, one2many or many2many field.
    pub fn comodel(&self) -> Option<&str> {
        match self {
            Self::Many2one { comodel, .. }
            | Self::One2many { comodel, .. }
            | Self::Many2many { comodel, .. } => Some(comodel),
            _ => None,
        }
    }

    /// Database action on delete when relational.
    pub fn ondelete(&self) -> Option<OnDelete> {
        match self {
            Self::Many2one { ondelete, .. } | Self::Many2many { ondelete, .. } => Some(*ondelete),
            _ => None,
        }
    }

    /// Whether translation metadata is valid for this type (`Char`, `Text`, `Html`).
    /// Runtime translation support is a separate ORM admission requirement.
    ///
    /// ```
    /// use erp_fields::FieldType;
    /// assert!(FieldType::Text.supports_translation());
    /// assert!(!FieldType::Integer.supports_translation());
    /// ```
    #[must_use]
    pub fn supports_translation(&self) -> bool {
        matches!(self, Self::Char { .. } | Self::Text | Self::Html)
    }

    /// Many2one, one2many or many2many.
    pub fn is_relational(&self) -> bool {
        self.comodel().is_some()
    }

    /// One2many or many2many.
    pub fn is_x2many(&self) -> bool {
        matches!(self, Self::One2many { .. } | Self::Many2many { .. })
    }

    /// The column this type needs, or `None` when it has no column of its own (`id` is created with
    /// the table).
    pub fn column_type(&self) -> Option<ColumnType> {
        Some(match self {
            Self::Id => return None,
            Self::Boolean => ColumnType::Bool,
            Self::Integer => ColumnType::Int4,
            Self::Float { digits: None } => ColumnType::Float8,
            Self::Float { digits: Some(_) } | Self::Monetary { .. } => ColumnType::Numeric,
            Self::Char { size } => ColumnType::Varchar(*size),
            Self::Text | Self::Html => ColumnType::Text,
            Self::Selection { .. } => ColumnType::Varchar(None),
            Self::Date => ColumnType::Date,
            Self::Datetime => ColumnType::Timestamp,
            Self::Uuid => ColumnType::Uuid,
            Self::Xid8 => ColumnType::Xid8,
            Self::TimestampTz => ColumnType::TimestampTz,
            Self::Json => ColumnType::Jsonb,
            Self::Many2one { .. } | Self::Many2oneReference { .. } | Self::RowVersion => {
                ColumnType::Int8
            }
            Self::Reference { .. } => ColumnType::Varchar(None),
            Self::One2many { .. } | Self::Many2many { .. } => return None,
            Self::DateRange { bounds } => ColumnType::DateRange(*bounds),
            Self::DatetimeRange { bounds } => ColumnType::TsRange(*bounds),
        })
    }

    /// The physical cell representation of values of this type.
    pub fn storage(&self) -> Storage {
        match self {
            Self::Id
            | Self::Many2one { .. }
            | Self::Many2oneReference { .. }
            | Self::RowVersion => Storage::Id,
            Self::One2many { .. } | Self::Many2many { .. } => Storage::Ids,
            Self::Reference { .. } => Storage::Str,
            Self::Boolean => Storage::Bool,
            Self::Integer => Storage::Int,
            Self::Float { digits: None } => Storage::Float,
            Self::Float { digits: Some(_) } | Self::Monetary { .. } => Storage::Decimal,
            Self::Char { .. } | Self::Text | Self::Html | Self::Selection { .. } => Storage::Str,
            Self::Date => Storage::Date,
            Self::Datetime => Storage::DateTime,
            Self::Uuid => Storage::Uuid,
            Self::Xid8 => Storage::Xid8,
            Self::TimestampTz => Storage::TimestampTz,
            Self::Json => Storage::Json,
            Self::DateRange { bounds } => Storage::DateRange(*bounds),
            Self::DatetimeRange { bounds } => Storage::DatetimeRange(*bounds),
        }
    }

    /// True when two types are the same kind (ignoring parameters).
    pub fn same_kind(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// How values of a field are represented in cells and cache columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Storage {
    /// `bool`.
    Bool,
    /// `i32`.
    Int,
    /// `i64` record ids.
    Id,
    /// `f64`.
    Float,
    /// `rust_decimal::Decimal`.
    Decimal,
    /// Text.
    Str,
    /// [`crate::Date`].
    Date,
    /// [`crate::DateTime`].
    DateTime,
    /// [`crate::Uuid`], with nil distinct from SQL NULL.
    Uuid,
    /// [`crate::Xid8`], with all unsigned bits retained.
    Xid8,
    /// [`crate::TimestampTz`], distinct from naive Datetime.
    TimestampTz,
    /// JSON document.
    Json,
    /// Ordered record ids (x2many values).
    Ids,
    /// [`crate::DateRange`], sent and read with the field's bounds.
    DateRange(RangeBounds),
    /// [`crate::DatetimeRange`], sent and read with the field's bounds.
    DatetimeRange(RangeBounds),
}

/// A PostgreSQL column type.
///
/// Every variant must be a type built into PostgreSQL (a fixed OID the driver knows): the ORM's
/// statement flights read record columns without a type lookup, because a lookup would queue
/// behind the flight's later statements while their rows wait to be drained (`erp-orm`
/// `transaction/pipelined.rs`, "Draining order"). A new variant needs its row in that module's
/// test `every_column_type_is_built_in`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnType {
    /// `int4`.
    Int4,
    /// `int8` (ids and many2one).
    Int8,
    /// `float8` (`double precision`).
    Float8,
    /// `numeric` without typmod.
    Numeric,
    /// `varchar` or `varchar(n)`.
    Varchar(Option<u32>),
    /// `text`.
    Text,
    /// `bool`.
    Bool,
    /// `date`.
    Date,
    /// `timestamp` (without time zone).
    Timestamp,
    /// Native `uuid`.
    Uuid,
    /// Native `xid8`, never an `int8` or wrapping xid32.
    Xid8,
    /// Native `timestamptz`, not naive `timestamp`.
    TimestampTz,
    /// `jsonb`.
    Jsonb,
    /// `daterange`; the field's bounds decide how values are sent and read.
    DateRange(RangeBounds),
    /// `tsrange`; the field's bounds decide how values are sent and read.
    TsRange(RangeBounds),
}

impl ColumnType {
    /// Catalog type name (`pg_type.typname`, `information_schema.columns.udt_name`).
    pub fn udt_name(self) -> &'static str {
        match self {
            Self::Int4 => "int4",
            Self::Int8 => "int8",
            Self::Float8 => "float8",
            Self::Numeric => "numeric",
            Self::Varchar(_) => "varchar",
            Self::Text => "text",
            Self::Bool => "bool",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::Uuid => "uuid",
            Self::Xid8 => "xid8",
            Self::TimestampTz => "timestamptz",
            Self::Jsonb => "jsonb",
            Self::DateRange(_) => "daterange",
            Self::TsRange(_) => "tsrange",
        }
    }

    /// Type as written in DDL.
    pub fn sql(self) -> String {
        match self {
            Self::Varchar(Some(n)) => format!("varchar({n})"),
            other => other.udt_name().to_owned(),
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Int4 | Self::Int8 | Self::Xid8 => 0,
            Self::Varchar(_) | Self::Uuid => 1,
            Self::Date => 2,
            Self::Jsonb | Self::DateRange(_) | Self::TsRange(_) => 3,
            Self::Text => 4,
            Self::Numeric => 5,
            Self::Bool => 6,
            Self::Timestamp | Self::TimestampTz => 7,
            Self::Float8 => 8,
        }
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sql())
    }
}

/// Physical materialization of a generated column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeneratedStorage {
    /// Evaluated on read; no physical storage on disk. Default.
    Virtual,
    /// Evaluated and materialized on write.
    Stored,
}

impl GeneratedStorage {
    /// As a lowercase SQL keyword string: `"virtual"` or `"stored"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Virtual => "virtual",
            Self::Stored => "stored",
        }
    }

    /// As PostgreSQL `pg_attribute.attgenerated` char string: `"v"` or `"s"`.
    pub const fn attgenerated(self) -> &'static str {
        match self {
            Self::Virtual => "v",
            Self::Stored => "s",
        }
    }

    /// Parse from a string ("virtual" or "stored").
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("virtual") {
            Some(Self::Virtual)
        } else if trimmed.eq_ignore_ascii_case("stored") {
            Some(Self::Stored)
        } else {
            None
        }
    }

    /// True if virtual.
    pub const fn is_virtual(self) -> bool {
        matches!(self, Self::Virtual)
    }

    /// True if stored.
    pub const fn is_stored(self) -> bool {
        matches!(self, Self::Stored)
    }
}

/// Generation metadata for a database column.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ColumnGenerated {
    /// Materialization kind (VIRTUAL or STORED).
    pub storage: GeneratedStorage,
    /// Emitted PostgreSQL expression.
    pub expression: String,
}

impl ColumnGenerated {
    /// Describe a generated column for owner-role DDL emission.
    ///
    /// `expression` is SQL, not user text: the caller must validate and compile it first.
    /// The registry's generated-expression compiler supplies this value during schema sync.
    ///
    /// ```
    /// use erp_fields::{ColumnGenerated, GeneratedStorage};
    /// let generation = ColumnGenerated::new(GeneratedStorage::Virtual, "\"amount\"");
    /// assert_eq!(generation.storage, GeneratedStorage::Virtual);
    /// ```
    pub fn new(storage: GeneratedStorage, expression: impl Into<String>) -> Self {
        Self {
            storage,
            expression: expression.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_types_per_field_type() {
        let cases = [
            (FieldType::Integer, Some("int4")),
            (FieldType::Float { digits: None }, Some("float8")),
            (
                FieldType::Float {
                    digits: Some(Digits::Fixed {
                        precision: 0,
                        scale: 2,
                    }),
                },
                Some("numeric"),
            ),
            (
                FieldType::Float {
                    digits: Some(Digits::Unrounded),
                },
                Some("numeric"),
            ),
            (
                FieldType::Monetary {
                    currency_field: None,
                },
                Some("numeric"),
            ),
            (FieldType::Char { size: None }, Some("varchar")),
            (FieldType::Char { size: Some(3) }, Some("varchar(3)")),
            (FieldType::Text, Some("text")),
            (FieldType::Html, Some("text")),
            (
                FieldType::Selection {
                    values: None,
                    method: None,
                },
                Some("varchar"),
            ),
            (FieldType::Date, Some("date")),
            (FieldType::Datetime, Some("timestamp")),
            (FieldType::Boolean, Some("bool")),
            (FieldType::Json, Some("jsonb")),
            (
                FieldType::Many2one {
                    comodel: "res.users".into(),
                    ondelete: OnDelete::SetNull,
                },
                Some("int8"),
            ),
            (FieldType::Id, None),
            (FieldType::RowVersion, Some("int8")),
            (
                FieldType::DateRange {
                    bounds: RangeBounds::HalfOpen,
                },
                Some("daterange"),
            ),
            (
                FieldType::DatetimeRange {
                    bounds: RangeBounds::Closed,
                },
                Some("tsrange"),
            ),
        ];
        for (ty, expected) in cases {
            assert_eq!(
                ty.column_type().map(ColumnType::sql).as_deref(),
                expected,
                "{ty:?}"
            );
        }
    }

    #[test]
    fn index_kind_properties() {
        let all = [
            IndexKind::Btree,
            IndexKind::BtreeNotNull,
            IndexKind::Trigram,
            IndexKind::Brin,
            IndexKind::Gin,
        ];
        for kind in all {
            assert_eq!(IndexKind::from_name(kind.as_str()), Some(kind), "{kind:?}");
        }
        let methods: Vec<_> = all.iter().map(|k| k.method()).collect();
        assert_eq!(methods, ["btree", "btree", "gin", "brin", "gin"]);
        let opclasses: Vec<_> = all.iter().map(|k| k.opclass()).collect();
        assert_eq!(
            opclasses,
            [
                None,
                None,
                Some("gin_trgm_ops"),
                None,
                Some("jsonb_path_ops")
            ]
        );
        assert_eq!(
            all.iter().filter(|k| k.not_null_only()).collect::<Vec<_>>(),
            [&IndexKind::BtreeNotNull]
        );
        assert_eq!(IndexKind::Trigram.extension(), Some("pg_trgm"));
        assert_eq!(IndexKind::Gin.extension(), None);
        assert_eq!(IndexKind::from_name("gist"), None);
        assert_eq!(IndexKind::from_name(""), None);
        assert_eq!(IndexKind::from_name("BRIN"), None);
    }

    #[test]
    fn index_kinds_accept_their_field_types() {
        let types = [
            FieldType::Integer,
            FieldType::Float { digits: None },
            FieldType::Char { size: None },
            FieldType::Text,
            FieldType::Date,
            FieldType::Datetime,
            FieldType::Json,
            FieldType::Boolean,
            FieldType::Many2one {
                comodel: "res.users".into(),
                ondelete: OnDelete::SetNull,
            },
        ];
        let accepted = |kind: IndexKind| -> Vec<&'static str> {
            types
                .iter()
                .filter(|t| kind.accepts(t))
                .map(FieldType::type_name)
                .collect()
        };
        assert_eq!(accepted(IndexKind::Brin), ["integer", "date", "datetime"]);
        assert_eq!(accepted(IndexKind::Gin), ["json"]);
        assert_eq!(accepted(IndexKind::Btree).len(), types.len());
        assert_eq!(accepted(IndexKind::Trigram).len(), types.len());
    }

    #[test]
    fn ranks_follow_reference() {
        let mut cols = [
            ColumnType::Float8,
            ColumnType::Bool,
            ColumnType::Varchar(None),
            ColumnType::Int4,
            ColumnType::Date,
            ColumnType::Text,
            ColumnType::Numeric,
            ColumnType::Jsonb,
            ColumnType::Timestamp,
        ];
        cols.sort_by_key(|c| c.rank());
        let names: Vec<_> = cols.iter().map(|c| c.udt_name()).collect();
        assert_eq!(
            names,
            [
                "int4",
                "varchar",
                "date",
                "jsonb",
                "text",
                "numeric",
                "bool",
                "timestamp",
                "float8"
            ]
        );
    }
}
