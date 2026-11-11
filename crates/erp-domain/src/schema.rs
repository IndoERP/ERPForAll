//! What the optimizer needs to know about models, without depending on the registry.
//!
//! The registry (`erp-registry`, via `erp-orm`) implements [`Schema`]; tests
//! use small fixture schemas.
//!
//! # Memoizing BASIC results (for `erp-orm`, S2)
//!
//! The BASIC result is a pure function of the input domain and [`ModelContext::cache_key`]:
//! key a memo table by `(ctx.cache_key(), domain)` (both `Hash + Eq`; hashing a domain is O(1)).
//! Do not memoize domains holding [`crate::Opaque`] values: they are transaction-bound. Bump
//! [`Schema::version`] whenever field definitions change (registry rebuild, DDL) so stale entries
//! stop matching. [`crate::Domain::optimize`] itself never trusts an optimization level reached
//! under a different key.

use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicUsize;

use time::PrimitiveDateTime;

use crate::error::{ConditionError, ErrorReason};

/// Field types, as far as domain optimization distinguishes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FieldType {
    /// The `id` field (no falsy value: `id` is never `0`).
    Id,
    /// `Boolean`.
    Boolean,
    /// `Integer`.
    Integer,
    /// `Float`.
    Float,
    /// `Monetary`.
    Monetary,
    /// `Char`.
    Char,
    /// `Text`.
    Text,
    /// `Html`.
    Html,
    /// `Date`.
    Date,
    /// `Datetime`.
    Datetime,
    /// Native UUID with no concrete falsy value.
    Uuid,
    /// Native unsigned full-width transaction id with no concrete falsy value.
    Xid8,
    /// Explicit-offset instant, separate from naive Datetime rules.
    TimestampTz,
    /// `Selection`.
    Selection,
    /// `Reference` (`model,id` string, not relational).
    Reference,
    /// `Many2one`.
    Many2one,
    /// `One2many`.
    One2many,
    /// `Many2many`.
    Many2many,
    /// `Many2oneReference` (an integer id whose model is in another field; not relational).
    Many2oneReference,
    /// `Binary`.
    Binary,
    /// `Json`.
    Json,
    /// `Properties`.
    Properties,
    /// `PropertiesDefinition`.
    PropertiesDefinition,
    /// `date_range` (ERPForAll, design §5.2).
    DateRange,
    /// `datetime_range` (ERPForAll, design §5.2).
    DatetimeRange,
}

impl FieldType {
    /// `Many2one`, `One2many` or `Many2many`.
    pub fn is_relational(self) -> bool {
        matches!(self, Self::Many2one | Self::One2many | Self::Many2many)
    }

    /// `One2many` or `Many2many`.
    pub fn is_x2many(self) -> bool {
        matches!(self, Self::One2many | Self::Many2many)
    }

    /// Whether the type has a falsy value that SQL treats like NULL (`0`, `0.0`, `''`, `false`).
    /// Types without one (relational, date, datetime, selection, `id` …) need an explicit
    /// `in [False]` when an inequality is negated (SPEC-DOM §4.3).
    pub fn has_falsy_value(self) -> bool {
        // Exhaustive on purpose: a new field type must decide.
        match self {
            Self::Boolean
            | Self::Integer
            | Self::Float
            | Self::Monetary
            | Self::Char
            | Self::Text
            | Self::Html
            | Self::Many2oneReference => true,
            Self::Id
            | Self::Date
            | Self::Datetime
            | Self::Uuid
            | Self::Xid8
            | Self::TimestampTz
            | Self::Selection
            | Self::Reference
            | Self::Many2one
            | Self::One2many
            | Self::Many2many
            | Self::Binary
            | Self::Json
            | Self::Properties
            | Self::PropertiesDefinition
            | Self::DateRange
            | Self::DatetimeRange => false,
        }
    }
}

/// The definition of one field, as seen by the optimizer.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct FieldInfo<'a> {
    /// Field name.
    pub name: &'a str,
    /// Field type.
    pub field_type: FieldType,
    /// Comodel of a relational field.
    pub comodel: Option<&'a str>,
    pub not_null: bool,
}

impl<'a> FieldInfo<'a> {
    /// A nullable, non-relational field.
    pub fn new(name: &'a str, field_type: FieldType) -> Self {
        FieldInfo {
            name,
            field_type,
            comodel: None,
            not_null: false,
        }
    }

    /// Set the comodel (relational fields).
    pub fn with_comodel(mut self, comodel: &'a str) -> Self {
        self.comodel = Some(comodel);
        self
    }

    /// Mark the field required with a `NOT NULL` column.
    pub fn with_not_null(mut self, not_null: bool) -> Self {
        self.not_null = not_null;
        self
    }
}

/// Field definitions of the models a domain refers to. Shared across threads (one registry per
/// database).
pub trait Schema: Send + Sync {
    /// The field `field` of `model`, or `None` when either does not exist.
    fn field(&self, model: &str, field: &str) -> Option<FieldInfo<'_>>;

    /// Whether `model` exists.
    fn has_model(&self, model: &str) -> bool;

    /// A number that changes whenever field definitions change (the registry generation). Part of
    /// [`ModelContext::cache_key`]; `0` for schemas that never change.
    fn version(&self) -> u64 {
        0
    }
}

/// A timezone for converting between the user's wall-clock time and UTC. ERPForAll's
/// implementation (on the timezone database) lives in `erp-orm`; this crate only needs the
/// conversion.
pub trait TimeZone: Send + Sync {
    /// The IANA name (`Europe/Brussels`, `UTC` …). Part of [`ModelContext::cache_key`]: two
    /// timezones with the same id must convert identically.
    fn id(&self) -> &str;

    /// The UTC instant of a local wall-clock time. For ambiguous or skipped local times, behave
    /// like Python `zoneinfo` with `fold=0`. `None` when the result leaves the supported range.
    fn local_to_utc(&self, local: PrimitiveDateTime) -> Option<PrimitiveDateTime>;

    /// The local wall-clock time of a UTC instant. `None` when the result leaves the supported
    /// range.
    fn utc_to_local(&self, utc: PrimitiveDateTime) -> Option<PrimitiveDateTime>;
}

/// Coordinated Universal Time (the default when neither the context nor the user sets a timezone).
#[derive(Clone, Copy, Debug, Default)]
pub struct Utc;

impl TimeZone for Utc {
    fn id(&self) -> &str {
        "UTC"
    }

    fn local_to_utc(&self, local: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        Some(local)
    }

    fn utc_to_local(&self, utc: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        Some(utc)
    }
}

static UTC: Utc = Utc;

/// Everything a BASIC optimization result depends on besides the domain: the memoization key
/// (see the module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContextKey {
    /// The model the domain is optimized for.
    pub model: String,
    /// [`TimeZone::id`] of the user's timezone.
    pub timezone: String,
    /// Whether the model holds new records.
    pub has_new_records: bool,
    /// [`Schema::version`].
    pub schema_version: u64,
}

/// The model a domain is optimized for, with the environment facts BASIC optimization reads.
#[derive(Clone, Copy)]
pub struct ModelContext<'a> {
    schema: &'a dyn Schema,
    model: &'a str,
    tz: &'a dyn TimeZone,
    has_new_records: bool,
    /// Hash of the cache key; levels reached under another stamp are not trusted.
    stamp: u64,
    /// Steps taken by the running optimization (global budget), when one is running.
    steps: Option<&'a AtomicUsize>,
}

impl<'a> ModelContext<'a> {
    /// Optimize for `model`, in UTC, for a model holding no new records.
    pub fn new(schema: &'a dyn Schema, model: &'a str) -> Self {
        ModelContext {
            schema,
            model,
            tz: &UTC,
            has_new_records: false,
            stamp: 0,
            steps: None,
        }
        .restamped()
    }

    pub fn with_tz(mut self, tz: &'a dyn TimeZone) -> Self {
        self.tz = tz;
        self.restamped()
    }

    pub fn with_new_records(mut self, has_new_records: bool) -> Self {
        self.has_new_records = has_new_records;
        self.restamped()
    }

    /// The model name.
    pub fn model(&self) -> &'a str {
        self.model
    }

    /// The memoization key of BASIC results for this context.
    pub fn cache_key(&self) -> ContextKey {
        ContextKey {
            model: self.model.to_owned(),
            timezone: self.tz.id().to_owned(),
            has_new_records: self.has_new_records,
            schema_version: self.schema.version(),
        }
    }

    fn restamped(mut self) -> Self {
        let mut hasher = std::hash::DefaultHasher::new();
        self.model.hash(&mut hasher);
        self.tz.id().hash(&mut hasher);
        self.has_new_records.hash(&mut hasher);
        self.schema.version().hash(&mut hasher);
        // 0 is reserved for "valid in any context".
        self.stamp = hasher.finish().max(1);
        self
    }

    pub(crate) fn stamp(&self) -> u64 {
        self.stamp
    }

    pub(crate) fn tz(&self) -> &'a dyn TimeZone {
        self.tz
    }

    pub(crate) fn has_new_records(&self) -> bool {
        self.has_new_records
    }

    pub(crate) fn schema(&self) -> &'a dyn Schema {
        self.schema
    }

    pub(crate) fn steps(&self) -> Option<&'a AtomicUsize> {
        self.steps
    }

    /// The same context counting optimization steps in `steps`.
    pub(crate) fn with_steps<'b>(&self, steps: &'b AtomicUsize) -> ModelContext<'b>
    where
        'a: 'b,
    {
        ModelContext {
            schema: self.schema,
            model: self.model,
            tz: self.tz,
            has_new_records: self.has_new_records,
            stamp: self.stamp,
            steps: Some(steps),
        }
    }

    /// The context for a comodel: same schema, timezone and step budget, no new records.
    pub(crate) fn comodel<'b>(&self, model: &'b str) -> ModelContext<'b>
    where
        'a: 'b,
    {
        ModelContext {
            schema: self.schema,
            model,
            tz: self.tz,
            has_new_records: false,
            stamp: 0,
            steps: self.steps,
        }
        .restamped()
    }

    /// `render` gives the condition text for the error.
    pub(crate) fn field_of(
        &self,
        field_name: &str,
        render: impl FnOnce() -> String,
    ) -> Result<FieldInfo<'a>, ErrorReason> {
        self.schema
            .field(self.model, field_name)
            .ok_or_else(|| ErrorReason::Condition {
                reason: ConditionError::InvalidField {
                    model: self.model.to_owned(),
                    field: field_name.to_owned(),
                },
                condition: render(),
            })
    }
}
