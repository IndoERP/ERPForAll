//! Owned model declarations for runtime sources: `models/*.toml` of data-only addons and custom
//! (`x_`) models and fields.
//!
//! The composer works on `&'static` [`ModelDecl`]s (the shape `#[model]` emits). An owned
//! [`ModelDef`] becomes one through [`ModelDef::to_decl`], which interns it: every *distinct*
//! definition is leaked once for the life of the process and reused afterwards, so rebuilding a
//! registry from the same definitions allocates nothing new (design note).
//! Interned bytes are bounded twice: each database's declarations are charged to its
//! [`InternAccount`] (at most [`DATABASE_INTERN_QUOTA_BYTES`] by default), so one database churning
//! custom fields exhausts only its own quota (isolation); the whole process is capped
//! by its intern budget as a backstop ([`INTERN_BUDGET_BYTES`] by default; a server sizes it to
//! its databases with [`set_intern_budget`] and [`intern_budget_for`]). A definition that would
//! exceed either is refused with a `ResourceExhausted` error instead of growing memory without
//! bound.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use erp_fields::{Digits, Error, ErrorKind, IndexKind, OnDelete, RangeBounds, Result};

use crate::company_access::CompanyAccessDef;
use crate::decl::{
    AddonDescriptor, DefaultDecl, FieldDecl, KindDecl, ModelDecl, ModelKind, SelectionOnDeleteDecl,
    StaticValue, TableObjectDecl, TableObjectKind,
};
use crate::exclusion::{ExclusionDecl, MAX_CONSTRAINT_COLUMNS, TemporalKeyDecl};
use crate::spec::CompanyMatch;
use crate::typed_index::{IndexDecl, IndexDef};

/// Longest model or addon name a runtime definition may use, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// Longest text attribute (label, help, domain, context, related path, groups, order, selection
/// value or label) a runtime definition may use, in bytes.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Most fields one runtime definition may declare (PostgreSQL allows 1600 columns per table).
pub const MAX_FIELDS_PER_MODEL: usize = 1600;
/// Most parents (`inherit`) one runtime definition may list.
pub const MAX_PARENTS: usize = 64;
/// Most `selection`/`selection_add` entries one field may list.
pub const MAX_SELECTION_ENTRIES: usize = 4096;
/// Largest `digits` scale (PostgreSQL `numeric` keeps at most 1000 decimal places).
pub const MAX_NUMERIC_SCALE: u32 = 1000;
/// The default of the process intern budget: the most bytes the process may intern for runtime
/// definitions over its lifetime (leaked strings, slices and declarations plus the interner's own
/// keys, with a fixed per-allocation overhead). Room for 8 databases at their full default quota;
/// a server with more databases raises it with [`set_intern_budget`] (see
/// [`intern_budget_for`]).
pub const INTERN_BUDGET_BYTES: usize = 256 * 1024 * 1024;
/// Most bytes one database's runtime declarations may intern over the life of the process (the
/// default and the largest limit of an [`InternAccount`]).
pub const DATABASE_INTERN_QUOTA_BYTES: usize = 32 * 1024 * 1024;

/// Most typed table objects (exclusions, temporal keys) one runtime definition may declare.
pub const MAX_TABLE_OBJECTS_PER_MODEL: usize = 256;
/// Bytes charged per heap allocation on top of its payload (allocator and table bookkeeping).
const ALLOC_OVERHEAD: usize = 48;

/// A field kind with its parameters, owned. Runtime sources cannot declare selection methods, `Reference` or `Many2oneReference` fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KindDef {
    /// `Boolean`.
    Boolean,
    /// `Integer`.
    Integer,
    /// `Float(digits=...)`.
    Float {
        /// `None` when not given.
        digits: Option<Digits>,
    },
    /// `Monetary(currency_field=...)`.
    Monetary {
        /// Currency field name.
        currency_field: Option<String>,
    },
    /// `Char(size=...)`.
    Char {
        /// `None` when not given.
        size: Option<u32>,
    },
    /// `Text`.
    Text,
    /// `Html`.
    Html,
    /// `Selection(selection=..., selection_add=...)`.
    Selection {
        /// A static list replacing the inherited one.
        values: Option<Vec<(String, String)>>,
        /// `selection_add` entries; a `None` label only positions an existing value.
        add: Vec<(String, Option<String>)>,
        /// `ondelete` policies for added values.
        ondelete: Vec<(String, erp_fields::SelectionOnDelete)>,
    },
    /// `Date`.
    Date,
    /// Native UUID without identity policy.
    Uuid,
    /// Native full-width unsigned PostgreSQL transaction identifier.
    Xid8,
    /// Explicit-offset, bounded microsecond instant.
    TimestampTz,
    /// `Datetime`.
    Datetime,
    /// `Json`.
    Json,
    /// `Many2one(comodel, ondelete=...)`.
    Many2one {
        /// Target model.
        comodel: String,
        /// `None` when not given.
        ondelete: Option<OnDelete>,
    },
    /// `One2many(comodel, inverse_name)`.
    One2many {
        /// Target model.
        comodel: String,
        /// The comodel's many2one pointing back.
        inverse: String,
    },
    /// `Many2many(comodel, relation, column1, column2, ondelete=...)`.
    Many2many {
        /// Target model.
        comodel: String,
        /// The relation table and its two columns; generated when `None`.
        table: Option<M2mTable>,
        /// `None` when not given (`cascade`).
        ondelete: Option<OnDelete>,
    },
    /// `date_range` with its bounds (design §5.2).
    DateRange {
        /// Which ends the field's ranges include.
        bounds: RangeBounds,
    },
    /// `datetime_range` with its bounds (design §5.2).
    DatetimeRange {
        /// Which ends the field's ranges include.
        bounds: RangeBounds,
    },
}

impl KindDef {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Integer => "integer",
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
            Self::DateRange { .. } => "date_range",
            Self::DatetimeRange { .. } => "datetime_range",
        }
    }
}

/// An explicit many2many relation table: `relation`, `column1` (this model) and `column2` (the
/// comodel). The three go together (FIELD §4.10).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct M2mTable {
    /// Relation table name.
    pub relation: String,
    /// Column pointing at this model.
    pub column1: String,
    /// Column pointing at the comodel.
    pub column2: String,
}

impl M2mTable {
    /// A relation table with its two columns.
    pub fn new(
        relation: impl Into<String>,
        column1: impl Into<String>,
        column2: impl Into<String>,
    ) -> Self {
        Self {
            relation: relation.into(),
            column1: column1.into(),
            column2: column2.into(),
        }
    }
}

/// A static default value, owned.
///
/// Floats compare by bit pattern (`NaN` equals itself, `0.0` differs from `-0.0`): two
/// definitions are the same declaration only when every value is bit-identical, which is what
/// interning needs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DefaultValue {
    False,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string.
    Str(String),
    /// `default=None`: drop the inherited default.
    Remove,
}

impl PartialEq for DefaultValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::False, Self::False) | (Self::Remove, Self::Remove) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            (Self::Str(a), Self::Str(b)) => a == b,
            (
                Self::False
                | Self::Remove
                | Self::Bool(_)
                | Self::Int(_)
                | Self::Float(_)
                | Self::Str(_),
                _,
            ) => false,
        }
    }
}

impl Eq for DefaultValue {}

impl Hash for DefaultValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::False | Self::Remove => {}
            Self::Bool(b) => b.hash(state),
            Self::Int(i) => i.hash(state),
            Self::Float(f) => f.to_bits().hash(state),
            Self::Str(s) => s.hash(state),
        }
    }
}

/// One field declaration, owned. `None` means "not given", as in [`FieldDecl`]: an extension
/// never overrides an attribute it does not mention.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FieldDef {
    /// Field name.
    pub name: String,
    /// Kind and kind parameters.
    pub kind: KindDef,
    /// `string`.
    pub string: Option<String>,
    /// `help`.
    pub help: Option<String>,
    /// `required`.
    pub required: Option<bool>,
    /// `readonly`.
    pub readonly: Option<bool>,
    /// `store`.
    pub store: Option<bool>,
    /// `copy`.
    pub copy: Option<bool>,
    /// `index`: `Some(None)` is an explicit `index=False`.
    pub index: Option<Option<IndexKind>>,
    /// Static `default`.
    pub default: Option<DefaultValue>,
    /// `related` path.
    pub related: Option<String>,
    /// `compute` method name: a package addon's computed field. The dependencies
    /// come from the compute implementation the runtime registers for that name.
    pub compute: Option<String>,
    /// `compute_sudo`.
    pub compute_sudo: Option<bool>,
    /// `groups`.
    pub groups: Option<String>,
    /// Relational `domain=` in JSON list syntax.
    pub domain: Option<String>,
    /// Relational `context=` as a JSON object.
    pub context: Option<String>,
    /// `check_company`.
    pub check_company: Option<bool>,
    /// `translate`: translatable textual field stored as JSONB with language keys.
    pub translate: Option<bool>,
    /// Explicit host HTML policy; omitted Html declarations use the full default allowlists.
    pub html_policy: Option<erp_fields::html::FieldPolicy>,
    /// Generated column expression: `Some(Some(expr))` to set, `Some(None)` for explicit removal.
    pub generated: Option<Option<String>>,
    /// Storage kind of the generated column (virtual or stored).
    pub generated_storage: Option<crate::generated::GeneratedStorage>,
}

impl FieldDef {
    /// Choose sanitizing or explicit untrusted text. No privilege bypass exists.
    pub fn sanitize(self, enabled: bool) -> Self {
        self.html_policy(if enabled {
            erp_fields::html::FieldPolicy::Sanitize(erp_fields::html::SanitizePolicy::Default(
                erp_fields::html::DefaultPolicy::new(),
            ))
        } else {
            erp_fields::html::FieldPolicy::UnsanitisedText
        })
    }

    /// Select the host HTML policy. Non-HTML fields are refused at final registry assembly.
    pub fn html_policy(mut self, policy: erp_fields::html::FieldPolicy) -> Self {
        self.html_policy = Some(policy);
        self
    }

    /// A field with only a name and a kind.
    ///
    /// ```
    /// use erp_registry::{FieldDef, KindDef};
    /// let field = FieldDef::new("x_color", KindDef::Integer).string("Color");
    /// assert_eq!(field.string.as_deref(), Some("Color"));
    /// ```
    pub fn new(name: impl Into<String>, kind: KindDef) -> Self {
        Self {
            name: name.into(),
            kind,
            string: None,
            help: None,
            required: None,
            readonly: None,
            store: None,
            copy: None,
            index: None,
            default: None,
            related: None,
            compute: None,
            compute_sudo: None,
            groups: None,
            domain: None,
            context: None,
            check_company: None,
            translate: None,
            html_policy: None,
            generated: None,
            generated_storage: None,
        }
    }

    /// Set `string`.
    #[must_use]
    pub fn string(mut self, label: impl Into<String>) -> Self {
        self.string = Some(label.into());
        self
    }

    /// Set `required`.
    #[must_use]
    pub fn required(mut self, on: bool) -> Self {
        self.required = Some(on);
        self
    }

    /// Set `related`.
    #[must_use]
    pub fn related(mut self, path: impl Into<String>) -> Self {
        self.related = Some(path.into());
        self
    }

    /// Set `compute`: the name of the compute method that fills the field.
    #[must_use]
    pub fn compute(mut self, method: impl Into<String>) -> Self {
        self.compute = Some(method.into());
        self
    }

    /// Set `compute_sudo`.
    #[must_use]
    pub fn compute_sudo(mut self, on: bool) -> Self {
        self.compute_sudo = Some(on);
        self
    }

    /// Set a static default.
    #[must_use]
    pub fn default(mut self, value: DefaultValue) -> Self {
        self.default = Some(value);
        self
    }

    /// Set `help`.
    #[must_use]
    pub fn help(mut self, text: impl Into<String>) -> Self {
        self.help = Some(text.into());
        self
    }

    /// Set `readonly`.
    #[must_use]
    pub fn readonly(mut self, on: bool) -> Self {
        self.readonly = Some(on);
        self
    }

    /// Set `store`.
    #[must_use]
    pub fn store(mut self, on: bool) -> Self {
        self.store = Some(on);
        self
    }

    /// Set `copy`.
    #[must_use]
    pub fn copy(mut self, on: bool) -> Self {
        self.copy = Some(on);
        self
    }

    /// Set `index`: `None` is an explicit `index=False`.
    #[must_use]
    pub fn index(mut self, kind: Option<IndexKind>) -> Self {
        self.index = Some(kind);
        self
    }

    /// Set `groups`.
    #[must_use]
    pub fn groups(mut self, groups: impl Into<String>) -> Self {
        self.groups = Some(groups.into());
        self
    }

    /// Set the relational `domain` (JSON list syntax).
    #[must_use]
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Set the relational `context` (a JSON object).
    #[must_use]
    pub fn context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Set `check_company`.
    #[must_use]
    pub fn check_company(mut self, on: bool) -> Self {
        self.check_company = Some(on);
        self
    }

    /// Set translation metadata for a Char, Text or Html field.
    ///
    /// Registry build rejects other types, company-dependent combinations and
    /// translated index keys. ORM construction refuses translated fields until
    /// phase 3/4 supplies runtime persistence.
    ///
    /// ```
    /// use erp_registry::{FieldDef, KindDef};
    /// let field = FieldDef::new("x_name", KindDef::Text).translate(true);
    /// assert_eq!(field.translate, Some(true));
    /// ```
    #[must_use]
    pub fn translate(mut self, on: bool) -> Self {
        self.translate = Some(on);
        self
    }

    /// Set the expression for a database-generated column.
    #[must_use]
    pub fn generated(mut self, expr: impl Into<String>) -> Self {
        self.generated = Some(Some(expr.into()));
        self
    }

    /// Set the storage kind of a database-generated column (virtual or stored).
    #[must_use]
    pub fn generated_storage(mut self, storage: crate::generated::GeneratedStorage) -> Self {
        self.generated_storage = Some(storage);
        self
    }

    /// Explicitly remove generation (refused).
    #[must_use]
    pub fn remove_generated(mut self) -> Self {
        self.generated = Some(None);
        self
    }

    /// Check the size limits of this field definition and that its static default fits its type
    /// (`model` names it in errors).
    ///
    /// # Errors
    ///
    /// A `Validation` error naming the field and the limit or the default that does not fit.
    pub fn check_limits(&self, model: &str) -> Result<()> {
        let at = || format!("field '{}' on model '{}'", clip(&self.name), clip(model));
        check_len(&self.name, MAX_NAME_BYTES, || format!("{}: name", at()))?;
        let texts = [
            ("string", self.string.as_deref()),
            ("help", self.help.as_deref()),
            ("related", self.related.as_deref()),
            ("compute", self.compute.as_deref()),
            ("groups", self.groups.as_deref()),
            ("domain", self.domain.as_deref()),
            ("context", self.context.as_deref()),
        ];
        for (attr, text) in texts {
            if let Some(text) = text {
                check_len(text, MAX_TEXT_BYTES, || format!("{}: `{attr}`", at()))?;
            }
        }
        if let Some(DefaultValue::Str(s)) = &self.default {
            check_len(s, MAX_TEXT_BYTES, || format!("{}: `default`", at()))?;
        }
        if let Some(Some(expr)) = &self.generated {
            check_len(expr, crate::generated::MAX_GENERATED_EXPR_BYTES, || {
                format!("{}: `generated`", at())
            })?;
        }
        self.check_default(model)?;
        match &self.kind {
            KindDef::Monetary {
                currency_field: Some(name),
            } => check_len(name, MAX_NAME_BYTES, || {
                format!("{}: `currency_field`", at())
            }),
            KindDef::Selection {
                values,
                add,
                ondelete: _,
            } => {
                let count = values
                    .as_ref()
                    .map_or(0, Vec::len)
                    .saturating_add(add.len());
                if count > MAX_SELECTION_ENTRIES {
                    return Err(Error::validation(format!(
                        "{}: {count} selection entries, more than the limit of \
                         {MAX_SELECTION_ENTRIES}",
                        at()
                    )));
                }
                let labels = values
                    .iter()
                    .flatten()
                    .flat_map(|(k, l)| [k.as_str(), l.as_str()])
                    .chain(
                        add.iter()
                            .flat_map(|(k, l)| [Some(k.as_str()), l.as_deref()])
                            .flatten(),
                    );
                for text in labels {
                    check_len(text, MAX_TEXT_BYTES, || {
                        format!("{}: selection entry", at())
                    })?;
                }
                Ok(())
            }
            KindDef::Many2one { comodel, .. } => {
                check_len(comodel, MAX_NAME_BYTES, || format!("{}: `comodel`", at()))
            }
            KindDef::One2many { comodel, inverse } => {
                check_len(comodel, MAX_NAME_BYTES, || format!("{}: `comodel`", at()))?;
                check_len(inverse, MAX_NAME_BYTES, || format!("{}: `inverse`", at()))
            }
            KindDef::Many2many { comodel, table, .. } => {
                check_len(comodel, MAX_NAME_BYTES, || format!("{}: `comodel`", at()))?;
                if let Some(t) = table {
                    for (attr, name) in [
                        ("relation", &t.relation),
                        ("column1", &t.column1),
                        ("column2", &t.column2),
                    ] {
                        check_len(name, MAX_NAME_BYTES, || format!("{}: `{attr}`", at()))?;
                    }
                }
                Ok(())
            }
            KindDef::Float {
                digits: Some(Digits::Fixed { scale, .. }),
            } if *scale > MAX_NUMERIC_SCALE => Err(Error::validation(format!(
                "{}: `digits` scale {scale} is larger than {MAX_NUMERIC_SCALE} (PostgreSQL numeric)",
                at()
            ))),
            KindDef::Char { size: Some(0) } => Err(Error::validation(format!(
                "{}: `size` must be at least 1 (leave it out for no limit)",
                at()
            ))),
            KindDef::Boolean
            | KindDef::Integer
            | KindDef::Float { .. }
            | KindDef::Monetary {
                currency_field: None,
            }
            | KindDef::Char { .. }
            | KindDef::Text
            | KindDef::Html
            | KindDef::Date
            | KindDef::Datetime
            | KindDef::Uuid
            | KindDef::Xid8
            | KindDef::TimestampTz
            | KindDef::Json
            | KindDef::DateRange { .. }
            | KindDef::DatetimeRange { .. } => Ok(()),
        }
    }

    /// Approximate heap bytes owned by this field definition (see [`ModelDef::heap_bytes`]).
    fn heap_bytes(&self) -> usize {
        let text = |s: &str| s.len().saturating_add(ALLOC_OVERHEAD);
        let opt = |s: &Option<String>| s.as_deref().map_or(0, text);
        let mut total = text(&self.name)
            .saturating_add(opt(&self.string))
            .saturating_add(opt(&self.help))
            .saturating_add(opt(&self.related))
            .saturating_add(opt(&self.compute))
            .saturating_add(opt(&self.groups))
            .saturating_add(opt(&self.domain))
            .saturating_add(opt(&self.context));
        if let Some(DefaultValue::Str(s)) = &self.default {
            total = total.saturating_add(text(s));
        }
        let kind = match &self.kind {
            KindDef::Monetary { currency_field } => opt(currency_field),
            KindDef::Selection {
                values,
                add,
                ondelete: _,
            } => {
                let mut bytes = ALLOC_OVERHEAD.saturating_mul(2);
                for (k, l) in values.iter().flatten() {
                    bytes = bytes.saturating_add(text(k)).saturating_add(text(l));
                }
                for (k, l) in add {
                    bytes = bytes.saturating_add(text(k)).saturating_add(opt(l));
                }
                bytes
            }
            KindDef::Many2one { comodel, .. } => text(comodel),
            KindDef::One2many { comodel, inverse } => text(comodel).saturating_add(text(inverse)),
            KindDef::Many2many { comodel, table, .. } => {
                let names = table.as_ref().map_or(0, |t| {
                    text(&t.relation)
                        .saturating_add(text(&t.column1))
                        .saturating_add(text(&t.column2))
                });
                text(comodel).saturating_add(names)
            }
            KindDef::Boolean
            | KindDef::Integer
            | KindDef::Float { .. }
            | KindDef::Char { .. }
            | KindDef::Text
            | KindDef::Html
            | KindDef::Date
            | KindDef::Datetime
            | KindDef::Uuid
            | KindDef::Xid8
            | KindDef::TimestampTz
            | KindDef::Json
            | KindDef::DateRange { .. }
            | KindDef::DatetimeRange { .. } => 0,
        };
        total.saturating_add(kind)
    }

    /// A static default must fit the field type: `False` and `Remove` fit any field; booleans a
    /// boolean field; integers an integer, float or monetary field; finite floats a float or
    /// monetary field; strings a char, text, html, selection, date or datetime field. Relational
    /// and JSON fields take no static value.
    fn check_default(&self, model: &str) -> Result<()> {
        let Some(default) = &self.default else {
            return Ok(());
        };
        let fits = match (default, &self.kind) {
            (DefaultValue::False | DefaultValue::Remove, _)
            | (DefaultValue::Bool(_), KindDef::Boolean)
            | (
                DefaultValue::Int(_),
                KindDef::Integer | KindDef::Float { .. } | KindDef::Monetary { .. },
            )
            | (
                DefaultValue::Str(_),
                KindDef::Char { .. }
                | KindDef::Text
                | KindDef::Html
                | KindDef::Selection { .. }
                | KindDef::Date
                | KindDef::Datetime,
            ) => true,
            (DefaultValue::Float(f), KindDef::Float { .. } | KindDef::Monetary { .. }) => {
                f.is_finite()
            }
            (
                DefaultValue::Bool(_)
                | DefaultValue::Int(_)
                | DefaultValue::Float(_)
                | DefaultValue::Str(_),
                _,
            ) => false,
        };
        if fits {
            return Ok(());
        }
        let what = match default {
            DefaultValue::Bool(_) => "a boolean",
            DefaultValue::Int(_) => "an integer",
            DefaultValue::Float(f) if !f.is_finite() => "a non-finite float",
            DefaultValue::Float(_) => "a float",
            DefaultValue::Str(_) => "a string",
            DefaultValue::False | DefaultValue::Remove => "a value",
        };
        Err(Error::validation(format!(
            "field '{}' on model '{model}': {what} is not a valid default for a field of type '{}'",
            self.name,
            self.kind.type_name()
        )))
    }

    fn to_decl(&self, interner: &mut Session<'_>) -> Result<FieldDecl> {
        let kind = match &self.kind {
            KindDef::Boolean => KindDecl::Boolean,
            KindDef::Integer => KindDecl::Integer,
            KindDef::Float { digits } => KindDecl::Float { digits: *digits },
            KindDef::Monetary { currency_field } => KindDecl::Monetary {
                currency_field: interner.opt_str(currency_field.as_deref())?,
            },
            KindDef::Char { size } => KindDecl::Char { size: *size },
            KindDef::Text => KindDecl::Text,
            KindDef::Html => KindDecl::Html,
            KindDef::Selection {
                values,
                add,
                ondelete,
            } => KindDecl::Selection {
                values: match values {
                    None => None,
                    Some(values) => {
                        let mut out = Vec::with_capacity(values.len());
                        for (k, l) in values {
                            out.push((interner.str(k)?, interner.str(l)?));
                        }
                        Some(interner.slice(out)?)
                    }
                },
                method: None,
                add: {
                    let mut out = Vec::with_capacity(add.len());
                    for (k, l) in add {
                        out.push((interner.str(k)?, interner.opt_str(l.as_deref())?));
                    }
                    interner.slice(out)?
                },
                ondelete: {
                    let mut out = Vec::with_capacity(ondelete.len());
                    for (k, pol) in ondelete {
                        let decl_pol = match pol {
                            erp_fields::SelectionOnDelete::SetNull => {
                                SelectionOnDeleteDecl::SetNull
                            }
                            erp_fields::SelectionOnDelete::Cascade => {
                                SelectionOnDeleteDecl::Cascade
                            }
                            erp_fields::SelectionOnDelete::SetDefault => {
                                SelectionOnDeleteDecl::SetDefault
                            }
                            erp_fields::SelectionOnDelete::SetValue(v) => {
                                SelectionOnDeleteDecl::SetValue(interner.str(v)?)
                            }
                            erp_fields::SelectionOnDelete::Callback(c) => {
                                SelectionOnDeleteDecl::Callback(interner.str(c)?)
                            }
                            _ => {
                                return Err(Error::programming(
                                    "unsupported selection ondelete policy",
                                ));
                            }
                        };
                        out.push((interner.str(k)?, decl_pol));
                    }
                    interner.slice(out)?
                },
            },
            KindDef::Date => KindDecl::Date,
            KindDef::Datetime => KindDecl::Datetime,
            KindDef::Uuid => KindDecl::Uuid,
            KindDef::Xid8 => KindDecl::Xid8,
            KindDef::TimestampTz => KindDecl::TimestampTz,
            KindDef::Json => KindDecl::Json,
            KindDef::Many2one { comodel, ondelete } => KindDecl::Many2one {
                comodel: interner.str(comodel)?,
                ondelete: *ondelete,
            },
            KindDef::One2many { comodel, inverse } => KindDecl::One2many {
                comodel: interner.str(comodel)?,
                inverse: interner.str(inverse)?,
            },
            KindDef::DateRange { bounds } => KindDecl::DateRange { bounds: *bounds },
            KindDef::DatetimeRange { bounds } => KindDecl::DatetimeRange { bounds: *bounds },
            KindDef::Many2many {
                comodel,
                table,
                ondelete,
            } => {
                let (relation, column1, column2) = match table {
                    None => (None, None, None),
                    Some(t) => (
                        Some(interner.str(&t.relation)?),
                        Some(interner.str(&t.column1)?),
                        Some(interner.str(&t.column2)?),
                    ),
                };
                KindDecl::Many2many {
                    comodel: interner.str(comodel)?,
                    relation,
                    column1,
                    column2,
                    ondelete: *ondelete,
                }
            }
        };
        let mut fd = FieldDecl::new(interner.str(&self.name)?, kind);
        fd.string = interner.opt_str(self.string.as_deref())?;
        fd.help = interner.opt_str(self.help.as_deref())?;
        fd.required = self.required;
        fd.readonly = self.readonly;
        fd.store = self.store;
        fd.copy = self.copy;
        fd.index = self.index;
        fd.default = match &self.default {
            None => None,
            Some(DefaultValue::False) => Some(DefaultDecl::Value(StaticValue::False)),
            Some(DefaultValue::Bool(b)) => Some(DefaultDecl::Value(StaticValue::Bool(*b))),
            Some(DefaultValue::Int(i)) => Some(DefaultDecl::Value(StaticValue::Int(*i))),
            Some(DefaultValue::Float(f)) => Some(DefaultDecl::Value(StaticValue::Float(*f))),
            Some(DefaultValue::Str(s)) => {
                Some(DefaultDecl::Value(StaticValue::Str(interner.str(s)?)))
            }
            Some(DefaultValue::Remove) => Some(DefaultDecl::Remove),
        };
        fd.related = interner.opt_str(self.related.as_deref())?;
        fd.compute = interner.opt_str(self.compute.as_deref())?;
        fd.compute_sudo = self.compute_sudo;
        fd.groups = interner.opt_str(self.groups.as_deref())?;
        fd.domain = interner.opt_str(self.domain.as_deref())?;
        fd.context = interner.opt_str(self.context.as_deref())?;
        fd.check_company = self.check_company;
        fd.translate = self.translate;
        fd.html_policy = self.html_policy;
        fd.generated = match &self.generated {
            None => None,
            Some(None) => Some(None),
            Some(Some(expr)) => Some(Some(interner.str(expr)?)),
        };
        fd.generated_storage = self.generated_storage;
        Ok(fd)
    }
}

/// One model declaration, owned: a new model ([`ModelDef::new`]), or an in-place extension when
/// `inherit` contains `name` ([`ModelDef::extension`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ModelDef {
    /// Declaring addon.
    pub addon: String,
    /// Model name (`_name`).
    pub name: String,
    /// Parent models (`_inherit`); contains `name` for an in-place extension.
    pub inherit: Vec<String>,
    pub kind: Option<ModelKind>,
    /// `_description`.
    pub description: Option<String>,
    /// `_table`.
    pub table: Option<String>,
    /// `_order`.
    pub order: Option<String>,
    /// `_rec_name`.
    pub rec_name: Option<String>,
    /// `_check_company_auto`: `Some(false)` opts the model out of the default company check
    /// .
    pub check_company_auto: Option<bool>,
    /// `company_match`.
    pub company_match: Option<CompanyMatch>,
    /// `company_access` (A1-D2), validated when the registry is built.
    pub company_access: Option<CompanyAccessDef>,
    /// A custom (`ir.model` manual) model; only an [`crate::Overlay`] creates those.
    pub(crate) custom: bool,
    /// Fields in declaration order.
    pub fields: Vec<FieldDef>,
    /// Typed exclusion constraints (`[[model.exclude]]`).
    pub exclusions: Vec<ExclusionDef>,
    /// `WITHOUT OVERLAPS` keys (`[[model.key]]` with `without_overlaps`).
    pub temporal_keys: Vec<TemporalKeyDef>,
    /// Source file (for error messages and ordering inside an addon); empty when none.
    pub file: String,
    /// Source line.
    pub line: u32,
    /// Typed indexes (`[[model.index]]`, design, §9).
    pub indexes: Vec<IndexDef>,
    /// `transient_max_age_seconds`: seconds after which rows of a transient model may be cleaned
    /// up; `0` disables the age threshold. Only transient models may state it; the registry build
    /// refuses it elsewhere and above [`crate::MAX_TRANSIENT_MAX_AGE_SECONDS`].
    pub transient_max_age_seconds: Option<u64>,
    /// `transient_max_count`: the most rows a transient model keeps; `0` disables the count
    /// threshold. Only transient models may state it; the registry build refuses it elsewhere and
    /// above [`crate::MAX_TRANSIENT_MAX_COUNT`].
    pub transient_max_count: Option<u64>,
    /// Opt into transactional event capture; defaults false and composes by OR.
    pub eventful: bool,
}

impl ModelDef {
    /// A new regular model named `name`, declared by `addon`, with no fields.
    ///
    /// ```
    /// use erp_registry::{FieldDef, KindDef, ModelDef};
    /// let tag = ModelDef::new("my_tags", "my.tag")
    ///     .field(FieldDef::new("name", KindDef::Char { size: None }).required(true));
    /// assert!(!tag.is_extension());
    /// ```
    pub fn new(addon: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            addon: addon.into(),
            name: name.into(),
            inherit: Vec::new(),
            kind: None,
            description: None,
            table: None,
            order: None,
            rec_name: None,
            check_company_auto: None,
            company_match: None,
            company_access: None,
            custom: false,
            fields: Vec::new(),
            exclusions: Vec::new(),
            temporal_keys: Vec::new(),
            file: String::new(),
            line: 0,
            indexes: Vec::new(),
            transient_max_age_seconds: None,
            transient_max_count: None,
            eventful: false,
        }
    }

    /// ```
    /// use erp_registry::{FieldDef, KindDef, ModelDef};
    /// let ext = ModelDef::extension("my_tags", "res.partner")
    ///     .field(FieldDef::new("tag_count", KindDef::Integer));
    /// assert!(ext.is_extension());
    /// ```
    pub fn extension(addon: impl Into<String>, name: impl Into<String>) -> Self {
        let mut def = Self::new(addon, name);
        def.inherit = vec![def.name.clone()];
        def
    }

    /// Opt into transactional event capture. False never disables another loaded
    /// declaration's opt-in. Invalid model kinds/namespaces are refused by the registry build.
    #[must_use]
    pub fn eventful(mut self, on: bool) -> Self {
        self.eventful = on;
        self
    }

    /// Set `_check_company_auto`; `false` opts the model out of the default company check.
    #[must_use]
    pub fn check_company_auto(mut self, on: bool) -> Self {
        self.check_company_auto = Some(on);
        self
    }

    /// Set `company_match`.
    #[must_use]
    pub fn company_match(mut self, m: CompanyMatch) -> Self {
        self.company_match = Some(m);
        self
    }

    /// Set `company_access` (A1-D2).
    #[must_use]
    pub fn company_access(mut self, access: CompanyAccessDef) -> Self {
        self.company_access = Some(access);
        self
    }

    /// Set `transient_max_age_seconds` (`0` disables the age threshold). Only a transient model
    /// may state it; the registry build refuses it on any other kind.
    #[must_use]
    pub fn transient_max_age_seconds(mut self, seconds: u64) -> Self {
        self.transient_max_age_seconds = Some(seconds);
        self
    }

    /// Set `transient_max_count` (`0` disables the count threshold). Only a transient model may
    /// state it; the registry build refuses it on any other kind.
    #[must_use]
    pub fn transient_max_count(mut self, count: u64) -> Self {
        self.transient_max_count = Some(count);
        self
    }

    /// Add a field.
    #[must_use]
    pub fn field(mut self, field: FieldDef) -> Self {
        self.fields.push(field);
        self
    }

    /// Add a typed index.
    ///
    /// # Errors
    ///
    /// None here; see [`IndexDef`] for what [`ModelDef::check_limits`] and the registry build
    /// refuse.
    #[must_use]
    pub fn index(mut self, index: IndexDef) -> Self {
        self.indexes.push(index);
        self
    }

    /// Add a typed exclusion constraint.
    #[must_use]
    pub fn exclusion(mut self, exclusion: ExclusionDef) -> Self {
        self.exclusions.push(exclusion);
        self
    }

    /// Add a `WITHOUT OVERLAPS` key.
    #[must_use]
    pub fn temporal_key(mut self, key: TemporalKeyDef) -> Self {
        self.temporal_keys.push(key);
        self
    }

    /// True when this definition extends an existing model in place.
    pub fn is_extension(&self) -> bool {
        self.inherit.contains(&self.name)
    }

    /// True for a custom (`ir.model` manual) model created by an [`crate::Overlay`].
    pub fn is_custom(&self) -> bool {
        self.custom
    }

    /// Check names and sizes against the limits of this module ([`MAX_NAME_BYTES`],
    /// [`MAX_TEXT_BYTES`], [`MAX_FIELDS_PER_MODEL`], [`MAX_PARENTS`], [`MAX_SELECTION_ENTRIES`]).
    /// Semantic checks (does the parent exist, is the field name a valid column) happen when the
    /// registry is built.
    ///
    /// # Errors
    ///
    /// A `Validation` error naming the model (and field) that is too large or badly named.
    pub fn check_limits(&self) -> Result<()> {
        check_len(&self.addon, MAX_NAME_BYTES, || {
            format!("model '{}': addon name", self.name)
        })?;
        check_model_name(&self.name)?;
        if self.inherit.len() > MAX_PARENTS {
            return Err(Error::validation(format!(
                "model '{}': {} parents, more than the limit of {MAX_PARENTS}",
                self.name,
                self.inherit.len()
            )));
        }
        for parent in &self.inherit {
            check_model_name(parent)?;
        }
        if self.fields.len() > MAX_FIELDS_PER_MODEL {
            return Err(Error::validation(format!(
                "model '{}': {} fields, more than the limit of {MAX_FIELDS_PER_MODEL}",
                self.name,
                self.fields.len()
            )));
        }
        for (attr, text) in [
            ("description", self.description.as_deref()),
            ("table", self.table.as_deref()),
            ("order", self.order.as_deref()),
            ("rec_name", self.rec_name.as_deref()),
        ] {
            if let Some(text) = text {
                check_len(text, MAX_TEXT_BYTES, || {
                    format!("model '{}': `{attr}`", self.name)
                })?;
            }
        }
        check_len(&self.file, MAX_TEXT_BYTES, || {
            format!("model '{}': source file name", self.name)
        })?;
        for field in &self.fields {
            field.check_limits(&self.name)?;
        }
        if let Some(access) = &self.company_access {
            access.check_limits(&self.name)?;
        }
        crate::typed_index::check_index_defs(&self.name, &self.indexes, MAX_NAME_BYTES)?;
        self.check_object_limits()?;
        Ok(())
    }

    /// Size limits of the typed table objects.
    fn check_object_limits(&self) -> Result<()> {
        let count = self
            .exclusions
            .len()
            .saturating_add(self.temporal_keys.len());
        if count > MAX_TABLE_OBJECTS_PER_MODEL {
            return Err(Error::validation(format!(
                "model '{}': {count} exclusions and keys, more than the limit of \
                 {MAX_TABLE_OBJECTS_PER_MODEL}",
                self.name
            )));
        }
        let mut names: Vec<(&str, &str)> = Vec::new();
        for e in &self.exclusions {
            names.push(("exclusion", &e.name));
            names.push(("exclusion field", &e.period.0));
            names.push(("exclusion field", &e.period.1));
            names.extend(e.equal.iter().map(|f| ("exclusion field", f.as_str())));
        }
        for k in &self.temporal_keys {
            names.push(("key", &k.name));
            names.push(("key field", &k.range));
            names.extend(k.fields.iter().map(|f| ("key field", f.as_str())));
        }
        for (what, name) in names {
            check_len(name, MAX_NAME_BYTES, || {
                format!("model '{}': {what} name", clip(&self.name))
            })?;
        }
        let lists = self
            .exclusions
            .iter()
            .map(|e| (&e.name, e.equal.len()))
            .chain(self.temporal_keys.iter().map(|k| (&k.name, k.fields.len())));
        for (name, len) in lists {
            if len >= MAX_CONSTRAINT_COLUMNS {
                return Err(Error::validation(format!(
                    "model '{}': '{}' compares {len} fields, more than PostgreSQL's limit of \
                     {} besides the period",
                    clip(&self.name),
                    clip(name),
                    MAX_CONSTRAINT_COLUMNS - 1
                )));
            }
        }
        let texts = self
            .exclusions
            .iter()
            .flat_map(|e| [Some(e.message.as_str()), e.filter.as_deref()])
            .chain(self.temporal_keys.iter().map(|k| Some(k.message.as_str())))
            .chain(self.indexes.iter().map(|i| i.filter.as_deref()))
            .flatten();
        for text in texts {
            check_len(text, MAX_TEXT_BYTES, || {
                format!("model '{}': exclusion, key or index text", clip(&self.name))
            })?;
        }
        Ok(())
    }

    /// The `&'static` declaration the composer uses. Equal definitions return the same
    /// declaration (interned once per process). New bytes are charged to the process budget only;
    /// declarations of a database go through its [`crate::Overlay`], which also charges the
    /// database's [`InternAccount`].
    ///
    /// # Errors
    ///
    /// The errors of [`ModelDef::check_limits`], and a `ResourceExhausted` error when the
    /// definition does not fit the process intern budget ([`intern_budget`]).
    pub fn to_decl(&self) -> Result<&'static ModelDecl> {
        self.intern(None)
    }

    /// [`ModelDef::to_decl`], charging the bytes it newly interns to `account` as well. Reusing
    /// a declaration interned before (by any database) costs nothing.
    ///
    /// # Errors
    ///
    /// The errors of [`ModelDef::to_decl`]; a `ResourceExhausted` error when `account` would
    /// exceed its limit (nothing is charged to the process for the refused part).
    pub(crate) fn intern(&self, account: Option<&InternAccount>) -> Result<&'static ModelDecl> {
        self.check_limits()?;
        let mut interner = Session::open(account);
        if let Some(decl) = interner.table.decls.get(self) {
            return Ok(decl);
        }
        // price the whole definition, charge it once, then build it: a refused charge leaves
        // nothing leaked and nothing charged (review M5)
        self.build_decl(&mut interner)?;
        interner.commit()?;
        let decl = self.build_decl(&mut interner)?;
        let decl: &'static ModelDecl = Box::leak(Box::new(decl));
        interner.table.decls.insert(self.clone(), decl);
        Ok(decl)
    }

    /// The declaration of this definition, its strings and slices taken from `interner` (in a
    /// pricing pass they are placeholders and only the bytes count).
    fn build_decl(&self, interner: &mut Session<'_>) -> Result<ModelDecl> {
        let mut inherit = Vec::with_capacity(self.inherit.len());
        for parent in &self.inherit {
            inherit.push(interner.str(parent)?);
        }
        let inherit = interner.slice(inherit)?;
        let mut fields = Vec::with_capacity(self.fields.len());
        for field in &self.fields {
            fields.push(field.to_decl(interner)?);
        }
        let fields = interner.slice(fields)?;
        let table_objects = self.build_objects(interner)?;
        let (kind, follows) = match self.kind {
            Some(kind) => (kind, false),
            // not stated: an extension follows its model, a new model is regular (field doc)
            None if self.is_extension() => (ModelKind::Regular, true),
            None => (ModelKind::Regular, false),
        };
        let mut decl = ModelDecl::new(interner.str(&self.addon)?, interner.str(&self.name)?)
            .inherit(inherit)
            .kind(kind)
            .kind_follows_model(follows)
            .fields(fields)
            .table_objects(table_objects);
        decl.custom = self.custom;
        decl.runtime = true;
        decl = decl.at(interner.str(&self.file)?, self.line);
        decl.description = interner.opt_str(self.description.as_deref())?;
        decl.table = interner.opt_str(self.table.as_deref())?;
        decl.order = interner.opt_str(self.order.as_deref())?;
        decl.rec_name = interner.opt_str(self.rec_name.as_deref())?;
        decl.check_company_auto = self.check_company_auto;
        decl.company_match = self.company_match;
        decl.transient_max_age_seconds = self.transient_max_age_seconds;
        decl.transient_max_count = self.transient_max_count;
        decl.eventful = self.eventful;
        decl.company_access = match &self.company_access {
            Some(access) => Some(access.intern(&mut |s| interner.str(s))?),
            None => None,
        };
        // the declaration, and the owned copy of this definition kept as the table key
        let key_bytes = self
            .heap_bytes()
            .checked_add(size_of::<ModelDecl>())
            .and_then(|b| b.checked_add(size_of::<ModelDef>()))
            .and_then(|b| b.checked_add(2 * ALLOC_OVERHEAD))
            .ok_or_else(|| Error::programming("runtime declaration size overflows usize"))?;
        interner.cost(key_bytes)?;
        Ok(decl)
    }

    /// The typed table objects as declarations.
    fn build_objects(&self, interner: &mut Session<'_>) -> Result<&'static [TableObjectDecl]> {
        let mut objects = Vec::with_capacity(
            self.indexes.len() + self.exclusions.len() + self.temporal_keys.len(),
        );
        for index in &self.indexes {
            objects.push(index_decl(index, interner)?);
        }
        for e in &self.exclusions {
            let mut equal = Vec::with_capacity(e.equal.len());
            for f in &e.equal {
                equal.push(interner.str(f)?);
            }
            let decl = ExclusionDecl {
                equal: interner.slice(equal)?,
                period: (interner.str(&e.period.0)?, interner.str(&e.period.1)?),
                bounds: e.bounds,
                filter: interner.opt_str(e.filter.as_deref())?,
                message: interner.str(&e.message)?,
            };
            objects.push(TableObjectDecl {
                name: interner.str(&e.name)?,
                kind: TableObjectKind::Exclusion(decl),
            });
        }
        for k in &self.temporal_keys {
            let mut fields = Vec::with_capacity(k.fields.len());
            for f in &k.fields {
                fields.push(interner.str(f)?);
            }
            let decl = TemporalKeyDecl {
                fields: interner.slice(fields)?,
                range: interner.str(&k.range)?,
                message: interner.str(&k.message)?,
            };
            objects.push(TableObjectDecl {
                name: interner.str(&k.name)?,
                kind: TableObjectKind::TemporalKey(decl),
            });
        }
        interner.slice(objects)
    }

    /// Approximate heap bytes owned by this definition (strings and vectors, with
    /// [`ALLOC_OVERHEAD`] per allocation); what interning charges for keeping a copy as key.
    fn heap_bytes(&self) -> usize {
        let text = |s: &str| s.len().saturating_add(ALLOC_OVERHEAD);
        let opt = |s: &Option<String>| s.as_deref().map_or(0, text);
        let mut total = text(&self.addon)
            .saturating_add(text(&self.name))
            .saturating_add(text(&self.file))
            .saturating_add(opt(&self.description))
            .saturating_add(opt(&self.table))
            .saturating_add(opt(&self.order))
            .saturating_add(opt(&self.rec_name));
        if let Some(access) = &self.company_access {
            total = total.saturating_add(access.heap_bytes(text));
        }
        for parent in &self.inherit {
            total = total.saturating_add(text(parent));
        }
        total = total.saturating_add(
            self.fields
                .len()
                .saturating_mul(size_of::<FieldDef>())
                .saturating_add(ALLOC_OVERHEAD),
        );
        for field in &self.fields {
            total = total.saturating_add(field.heap_bytes());
        }
        total = total.saturating_add(index_defs_heap_bytes(&self.indexes));
        let list = |items: &[String]| {
            items
                .iter()
                .fold(ALLOC_OVERHEAD, |acc, s| acc.saturating_add(text(s)))
        };
        for e in &self.exclusions {
            total = total
                .saturating_add(size_of::<ExclusionDef>())
                .saturating_add(text(&e.name))
                .saturating_add(list(&e.equal))
                .saturating_add(text(&e.period.0))
                .saturating_add(text(&e.period.1))
                .saturating_add(opt(&e.filter))
                .saturating_add(text(&e.message));
        }
        for k in &self.temporal_keys {
            total = total
                .saturating_add(size_of::<TemporalKeyDef>())
                .saturating_add(text(&k.name))
                .saturating_add(list(&k.fields))
                .saturating_add(text(&k.range))
                .saturating_add(text(&k.message));
        }
        total
    }
}

/// A typed exclusion constraint, owned (`[[model.exclude]]` of a model file; see
/// [`crate::ExclusionDecl`]).
///
/// ```
/// use erp_fields::RangeBounds;
/// use erp_registry::{ExclusionDef, ModelDef};
///
/// let booking = ModelDef::extension("my_rules", "resource.booking").exclusion(
///     ExclusionDef::new(
///         "_no_double_booking",
///         ("start", "stop"),
///         RangeBounds::HalfOpen,
///         "This resource is already booked for an overlapping period.",
///     )
///     .equal(["resource_id"])
///     .filter("[('state', '!=', 'cancel')]"),
/// );
/// assert_eq!(booking.exclusions.len(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ExclusionDef {
    /// Name, starting with `_`.
    pub name: String,
    /// Fields compared with `=`.
    pub equal: Vec<String>,
    /// The period's `(from, to)` fields.
    pub period: (String, String),
    /// Which ends of the period are included.
    pub bounds: RangeBounds,
    /// Constant domain filter in list syntax.
    pub filter: Option<String>,
    /// Message when violated.
    pub message: String,
}

impl ExclusionDef {
    /// An exclusion named `name` over the period `(from, to)`.
    pub fn new(
        name: impl Into<String>,
        period: (impl Into<String>, impl Into<String>),
        bounds: RangeBounds,
        message: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            equal: Vec::new(),
            period: (period.0.into(), period.1.into()),
            bounds,
            filter: None,
            message: message.into(),
        }
    }

    /// Set the fields compared with `=`.
    #[must_use]
    pub fn equal(mut self, fields: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.equal = fields.into_iter().map(Into::into).collect();
        self
    }

    /// Set the constant domain filter.
    #[must_use]
    pub fn filter(mut self, domain: impl Into<String>) -> Self {
        self.filter = Some(domain.into());
        self
    }
}

/// A `WITHOUT OVERLAPS` key, owned (`[[model.key]]` with `without_overlaps` in a model file; see
/// [`crate::TemporalKeyDecl`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TemporalKeyDef {
    /// Name, starting with `_`.
    pub name: String,
    /// Fields compared with `=`.
    pub fields: Vec<String>,
    /// The range field that may not overlap.
    pub range: String,
    /// Message when violated.
    pub message: String,
}

impl TemporalKeyDef {
    /// A key named `name` on the range field `range`.
    pub fn new(
        name: impl Into<String>,
        range: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            fields: Vec::new(),
            range: range.into(),
            message: message.into(),
        }
    }

    /// Set the fields compared with `=`.
    #[must_use]
    pub fn fields(mut self, fields: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.fields = fields.into_iter().map(Into::into).collect();
        self
    }
}

/// A typed index of a runtime definition as the table object the composer uses, its strings and
/// slices taken from `interner`.
fn index_decl(index: &IndexDef, interner: &mut Session<'_>) -> Result<TableObjectDecl> {
    let mut names = |list: &[String]| -> Result<&'static [&'static str]> {
        let mut out = Vec::with_capacity(list.len());
        for name in list {
            out.push(interner.str(name)?);
        }
        interner.slice(out)
    };
    let fields = names(&index.fields)?;
    let include = names(&index.include)?;
    let mut decl = IndexDecl::new(fields).method(index.method).include(include);
    if let Some(filter) = interner.opt_str(index.filter.as_deref())? {
        decl = decl.filter(filter);
    }
    Ok(TableObjectDecl::index(interner.str(&index.name)?, decl))
}

/// Approximate heap bytes owned by typed index definitions (see [`ModelDef::heap_bytes`]).
fn index_defs_heap_bytes(indexes: &[IndexDef]) -> usize {
    if indexes.is_empty() {
        // an empty vector does not allocate
        return 0;
    }
    let text = |s: &str| s.len().saturating_add(ALLOC_OVERHEAD);
    let mut total = indexes
        .len()
        .saturating_mul(size_of::<IndexDef>())
        .saturating_add(ALLOC_OVERHEAD);
    for index in indexes {
        total = total.saturating_add(text(&index.name));
        total = total.saturating_add(index.filter.as_deref().map_or(0, text));
        for list in [&index.fields, &index.include] {
            total = total.saturating_add(ALLOC_OVERHEAD);
            for name in list {
                total = total.saturating_add(text(name));
            }
        }
    }
    total
}

pub(crate) fn check_model_name(name: &str) -> Result<()> {
    check_len(name, MAX_NAME_BYTES, || {
        format!("model name '{}'", clip(name))
    })?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'.')
    {
        return Err(Error::validation(format!(
            "The model name can only contain lowercase characters, digits, underscores and dots. \
             (got '{name}')"
        )));
    }
    Ok(())
}

/// `text` for an error message: at most 64 characters, then `…`.
pub(crate) fn clip(text: &str) -> String {
    if text.chars().nth(64).is_some() {
        let head: String = text.chars().take(64).collect();
        format!("{head}…")
    } else {
        text.to_owned()
    }
}

pub(crate) fn check_len(text: &str, max: usize, what: impl FnOnce() -> String) -> Result<()> {
    if text.len() > max {
        let what = what();
        let len = text.len();
        return Err(Error::validation(format!(
            "{what} is {len} bytes long, more than the limit of {max}"
        )));
    }
    Ok(())
}

/// The bytes one database has interned, against its limit (isolation). A database
/// keeps one account for the life of the process (see [`crate::Databases`]), so replacing its
/// overlay again and again cannot reset what it has used: interned bytes are never released
/// before a restart.
///
/// ```
/// use std::sync::Arc;
/// use erp_registry::{FieldDef, InternAccount, KindDef, Overlay, RegistryBuilder};
///
/// let account = Arc::new(InternAccount::with_limit(512)?);
/// let mut overlay = Overlay::new().with_account(Arc::clone(&account));
/// overlay.add_manual_field(
///     "res.users",
///     FieldDef::new("x_doc_quota_example", KindDef::Char { size: None }).help("x".repeat(600)),
/// )?;
/// let err = RegistryBuilder::new([]).linked(false).overlay(&overlay).build().unwrap_err();
/// assert_eq!(err.kind(), erp_fields::ErrorKind::ResourceExhausted);
/// assert!(account.used() <= account.limit());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug)]
pub struct InternAccount {
    limit: usize,
    // only changed under the interner lock, so a plain load/store pair is not a race
    used: AtomicUsize,
}

impl Default for InternAccount {
    fn default() -> Self {
        Self::new()
    }
}

impl InternAccount {
    /// An account with the default limit, [`DATABASE_INTERN_QUOTA_BYTES`].
    pub fn new() -> Self {
        Self {
            limit: DATABASE_INTERN_QUOTA_BYTES,
            used: AtomicUsize::new(0),
        }
    }

    /// An account with a lower limit (limits can only be lowered; zero refuses every new byte).
    ///
    /// # Errors
    ///
    /// `Programming` when `limit` is above [`DATABASE_INTERN_QUOTA_BYTES`] (a host
    /// configuration mistake, not user input).
    pub fn with_limit(limit: usize) -> Result<Self> {
        if limit > DATABASE_INTERN_QUOTA_BYTES {
            return Err(Error::programming(format!(
                "an intern account limit can only be lowered: {limit} bytes is more than \
                 {DATABASE_INTERN_QUOTA_BYTES}"
            )));
        }
        Ok(Self {
            limit,
            used: AtomicUsize::new(0),
        })
    }

    /// The limit, in bytes.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The bytes charged so far.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
}

/// The process-wide table of interned runtime declarations. Append-only. Keys come from
/// untrusted sources, so the tables use the standard library's randomly keyed hasher.
struct Interner {
    strs: HashSet<&'static str>,
    decls: HashMap<ModelDef, &'static ModelDecl>,
    addons: HashMap<(String, Vec<String>, bool), &'static AddonDescriptor>,
    bytes: usize,
    /// The process intern budget; `bytes <= budget` always holds.
    budget: usize,
}

impl Default for Interner {
    fn default() -> Self {
        Self {
            strs: HashSet::new(),
            decls: HashMap::new(),
            addons: HashMap::new(),
            bytes: 0,
            budget: INTERN_BUDGET_BYTES,
        }
    }
}

/// The process-wide interner, locked.
fn interner() -> MutexGuard<'static, Interner> {
    static INTERNER: OnceLock<Mutex<Interner>> = OnceLock::new();
    // Every entry is inserted only after it is fully built, so a panic in another thread leaves
    // the table consistent; recovering the guard from a poisoned lock is safe.
    INTERNER
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The process intern budget in bytes: [`INTERN_BUDGET_BYTES`] unless the server set another
/// one with [`set_intern_budget`].
pub fn intern_budget() -> usize {
    interner().budget
}

/// The bytes the process has interned for runtime definitions so far (never released before a
/// restart).
pub fn interned_bytes() -> usize {
    interner().bytes
}

/// Set the process intern budget (a server setting). Raising it lets a server with
/// many databases give each its full quota ([`intern_budget_for`]); lowering it bounds a small
/// host. Definitions over the budget are refused with a `ResourceExhausted` error.
///
/// ```
/// use erp_registry::{intern_budget, intern_budget_for, set_intern_budget};
///
/// // a server with 40 databases at the default quota
/// set_intern_budget(intern_budget_for(40)?)?;
/// assert_eq!(intern_budget(), intern_budget_for(40)?);
/// # Ok::<(), erp_fields::Error>(())
/// ```
///
/// # Errors
///
/// `Programming` when `bytes` is less than what the process has interned already (those bytes
/// cannot be released before a restart; nothing changes then).
pub fn set_intern_budget(bytes: usize) -> Result<()> {
    let mut table = interner();
    if bytes < table.bytes {
        return Err(Error::programming(format!(
            "the intern budget cannot be set to {bytes} bytes: the process has interned {} \
             bytes already",
            table.bytes
        )));
    }
    table.budget = bytes;
    Ok(())
}

/// The process intern budget for a server with `databases` databases: each database's full
/// default quota ([`DATABASE_INTERN_QUOTA_BYTES`]) on top of [`INTERN_BUDGET_BYTES`] for
/// definitions not charged to a database (addon `models/*.toml`, addon descriptors). With it,
/// no database at its quota can push another one over the process budget.
///
/// # Errors
///
/// `Programming` when the total does not fit in `usize`.
pub fn intern_budget_for(databases: usize) -> Result<usize> {
    databases
        .checked_mul(DATABASE_INTERN_QUOTA_BYTES)
        .and_then(|quotas| quotas.checked_add(INTERN_BUDGET_BYTES))
        .ok_or_else(|| {
            Error::programming(format!(
                "the intern budget for {databases} databases does not fit in usize"
            ))
        })
}

/// The locked interner, and the account new bytes are charged to (besides the process budget).
///
/// Interning runs in two passes under one lock: a pricing pass adds up the bytes the value would
/// newly intern (each new string once) without allocating anything, [`Session::commit`] charges
/// that total once, and the building pass leaks the value. A refused charge therefore leaves
/// nothing leaked and nothing charged, and retrying a refused definition does not grow memory.
struct Session<'a> {
    table: MutexGuard<'static, Interner>,
    account: Option<&'a InternAccount>,
    pass: Pass,
}

enum Pass {
    /// Adding up: bytes so far, and the new strings already counted.
    Price { bytes: usize, strs: HashSet<String> },
    /// Charged: allocate for real.
    Build,
}

impl<'a> Session<'a> {
    fn open(account: Option<&'a InternAccount>) -> Self {
        Self {
            table: interner(),
            account,
            pass: Pass::Price {
                bytes: 0,
                strs: HashSet::new(),
            },
        }
    }

    /// Count `bytes` in the pricing pass (the building pass was paid for already).
    fn cost(&mut self, bytes: usize) -> Result<()> {
        if let Pass::Price { bytes: total, .. } = &mut self.pass {
            *total = total
                .checked_add(bytes)
                .ok_or_else(|| Error::programming("runtime declaration size overflows usize"))?;
        }
        Ok(())
    }

    /// End the pricing pass: charge its total to the account (checked first, so a database over
    /// its quota is told so) and to the process budget, and switch to building. Nothing is
    /// charged when either refuses.
    fn commit(&mut self) -> Result<()> {
        let bytes = match std::mem::replace(&mut self.pass, Pass::Build) {
            Pass::Price { bytes, .. } => bytes,
            Pass::Build => {
                return Err(Error::programming(
                    "an intern session was charged twice (interner invariant)",
                ));
            }
        };
        let account_total = match self.account {
            None => None,
            Some(account) => Some(
                account
                    .used()
                    .checked_add(bytes)
                    .filter(|t| *t <= account.limit)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::ResourceExhausted,
                            format!(
                                "the runtime model declarations of this database use more than \
                                 {} bytes (its intern quota); restart the server to release \
                                 definitions that are no longer used",
                                account.limit
                            ),
                        )
                    })?,
            ),
        };
        let budget = self.table.budget;
        let total = self
            .table
            .bytes
            .checked_add(bytes)
            .filter(|t| *t <= budget)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::ResourceExhausted,
                    format!(
                        "runtime model declarations use more than {budget} bytes in this process \
                         (its intern budget); raise it with set_intern_budget or restart the \
                         server to release definitions that are no longer used"
                    ),
                )
            })?;
        self.table.bytes = total;
        if let (Some(account), Some(account_total)) = (self.account, account_total) {
            account.used.store(account_total, Ordering::Relaxed);
        }
        Ok(())
    }

    fn str(&mut self, s: &str) -> Result<&'static str> {
        if let Some(found) = self.table.strs.get(s) {
            return Ok(found);
        }
        match &mut self.pass {
            Pass::Price { bytes, strs } => {
                if !strs.contains(s) {
                    *bytes = bytes
                        .checked_add(s.len().saturating_add(ALLOC_OVERHEAD))
                        .ok_or_else(|| {
                            Error::programming("runtime declaration size overflows usize")
                        })?;
                    strs.insert(s.to_owned());
                }
                // a placeholder: the pricing pass's value is thrown away
                Ok("")
            }
            Pass::Build => {
                let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
                self.table.strs.insert(leaked);
                Ok(leaked)
            }
        }
    }

    fn opt_str(&mut self, s: Option<&str>) -> Result<Option<&'static str>> {
        s.map(|s| self.str(s)).transpose()
    }

    fn slice<T>(&mut self, items: Vec<T>) -> Result<&'static [T]> {
        let bytes = items
            .len()
            .checked_mul(size_of::<T>())
            .and_then(|b| b.checked_add(ALLOC_OVERHEAD))
            .ok_or_else(|| Error::programming("runtime declaration size overflows usize"))?;
        match self.pass {
            Pass::Price { .. } => {
                self.cost(bytes)?;
                // a placeholder: the pricing pass's value is thrown away
                Ok(&[])
            }
            Pass::Build => Ok(Vec::leak(items)),
        }
    }
}

/// A `&'static` addon descriptor for an addon known only at runtime (a data-only addon read from
/// a directory or package). Equal `(name, depends)` pairs return the same descriptor.
///
/// # Errors
///
/// A `Validation` error when a name is longer than [`MAX_NAME_BYTES`] or there are more than
/// [`MAX_PARENTS`] dependencies; a `ResourceExhausted` error when the process intern budget is
/// used up ([`intern_budget`]).
pub fn intern_addon(name: &str, depends: &[&str]) -> Result<&'static AddonDescriptor> {
    intern_addon_with_policy(name, depends, false)
}

/// Intern an addon including its exact-only admission policy (approved L-9).
///
/// # Errors
///
/// The same bounded-name, dependency and resource errors as [`intern_addon`].
/// Mandatory business addon names remain protected even when `exact_only` is false.
pub fn intern_addon_with_policy(
    name: &str,
    depends: &[&str],
    exact_only: bool,
) -> Result<&'static AddonDescriptor> {
    check_len(name, MAX_NAME_BYTES, || {
        format!("addon name '{}'", clip(name))
    })?;
    if depends.len() > MAX_PARENTS {
        return Err(Error::validation(format!(
            "addon '{name}': {} dependencies, more than the limit of {MAX_PARENTS}",
            depends.len()
        )));
    }
    for dep in depends {
        check_len(dep, MAX_NAME_BYTES, || {
            format!("addon '{name}': dependency")
        })?;
    }
    let mut interner = Session::open(None);
    let key = (
        name.to_owned(),
        depends.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>(),
        exact_only || AddonDescriptor::requires_exact(name),
    );
    if let Some(found) = interner.table.addons.get(&key) {
        return Ok(found);
    }
    // the descriptor, and the owned key
    let key_bytes = key
        .1
        .iter()
        .fold(key.0.len().saturating_add(ALLOC_OVERHEAD), |acc, d| {
            acc.saturating_add(d.len()).saturating_add(ALLOC_OVERHEAD)
        })
        .saturating_add(size_of::<AddonDescriptor>())
        .saturating_add(ALLOC_OVERHEAD);
    let build = |interner: &mut Session<'_>| -> Result<AddonDescriptor> {
        let mut deps = Vec::with_capacity(depends.len());
        for dep in depends {
            deps.push(interner.str(dep)?);
        }
        let depends = interner.slice(deps)?;
        let name = interner.str(name)?;
        interner.cost(key_bytes)?;
        Ok(AddonDescriptor {
            name,
            depends,
            exact_only: key.2,
        })
    };
    // priced and charged once before anything is leaked (see `Session`)
    build(&mut interner)?;
    interner.commit()?;
    let descriptor: &'static AddonDescriptor = Box::leak(Box::new(build(&mut interner)?));
    interner.table.addons.insert(key, descriptor);
    Ok(descriptor)
}

/// The process intern budget is global, and `cargo test` runs the tests of a binary on threads
/// of one process. A test that lowers or reads the budget takes `exclusive`; every test that
/// interns takes `shared`, so none runs while the budget is lowered. (nextest runs each test in
/// its own process, where the lock is uncontended.)
#[cfg(test)]
pub(crate) mod budget_lock {
    use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

    static BUDGET: RwLock<()> = RwLock::new(());

    pub(crate) fn shared() -> RwLockReadGuard<'static, ()> {
        BUDGET.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn exclusive() -> RwLockWriteGuard<'static, ()> {
        BUDGET.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::budget_lock::{exclusive, shared};

    use super::*;

    /// A definition with strings nobody else interns (the interner is process-wide).
    fn fresh(tag: &str, help_bytes: usize) -> ModelDef {
        ModelDef::extension(format!("owned_{tag}_addon"), format!("owned.{tag}.model")).field(
            FieldDef::new(format!("x_owned_{tag}"), KindDef::Char { size: None })
                .string(format!("Label of {tag}"))
                .help("h".repeat(help_bytes)),
        )
    }

    /// Review M5: a definition the account refuses charges nothing, however far interning got
    /// before the refusal, and a retry is refused the same way without growing the account.
    #[test]
    fn refused_intern_leaves_the_account_unchanged() {
        let _budget = shared();
        let account = InternAccount::with_limit(4096).unwrap();
        // the strings fit the limit one by one; the whole definition does not
        let def = fresh("refused", 3000);
        for _ in 0..3 {
            let err = def.intern(Some(&account)).unwrap_err();
            assert_eq!(err.kind(), erp_fields::ErrorKind::ResourceExhausted);
            assert_eq!(
                err.message(),
                "the runtime model declarations of this database use more than 4096 bytes (its \
                 intern quota); restart the server to release definitions that are no longer used"
            );
            assert_eq!(account.used(), 0);
        }
        // a definition that fits is charged once, and reusing it costs nothing
        let small = fresh("fits", 10);
        let decl = small.intern(Some(&account)).unwrap();
        let used = account.used();
        assert!(used > 0 && used <= account.limit());
        assert!(std::ptr::eq(decl, small.intern(Some(&account)).unwrap()));
        assert_eq!(account.used(), used);
    }

    /// Review N2: the process budget is a setting. A definition over it is refused with
    /// `ResourceExhausted`, charging nothing; raising the budget lets it in; the budget cannot
    /// go below what is interned. (nextest runs each test in its own process, so changing the
    /// process budget here touches no other test.)
    #[test]
    fn the_process_intern_budget_is_configurable() {
        let _budget = exclusive();
        assert_eq!(intern_budget(), INTERN_BUDGET_BYTES);
        let used = interned_bytes();
        set_intern_budget(used).unwrap();
        let def = fresh("budget", 100);
        let err = def.to_decl().unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(
            err.message(),
            format!(
                "runtime model declarations use more than {used} bytes in this process (its \
                 intern budget); raise it with set_intern_budget or restart the server to \
                 release definitions that are no longer used"
            )
        );
        assert_eq!(
            interned_bytes(),
            used,
            "a refused definition charges nothing"
        );
        // the database quota is a resource limit too, and is checked first
        let account = InternAccount::with_limit(0).unwrap();
        let err = def.intern(Some(&account)).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::ResourceExhausted);
        assert_eq!(
            err.message(),
            "the runtime model declarations of this database use more than 0 bytes (its intern \
             quota); restart the server to release definitions that are no longer used"
        );

        set_intern_budget(intern_budget_for(1).unwrap()).unwrap();
        def.to_decl().unwrap();
        let grown = interned_bytes();
        assert!(grown > used);
        let err = set_intern_budget(grown - 1).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "the intern budget cannot be set to {} bytes: the process has interned {grown} \
                 bytes already",
                grown - 1
            )
        );
        assert_eq!(intern_budget(), intern_budget_for(1).unwrap());
    }

    /// Review N2: the budget for a server scales with its databases; overflow is an error.
    #[test]
    fn the_intern_budget_scales_with_the_database_count() {
        let _budget = shared();
        assert_eq!(intern_budget_for(0).unwrap(), INTERN_BUDGET_BYTES);
        assert_eq!(
            intern_budget_for(100).unwrap() - intern_budget_for(99).unwrap(),
            DATABASE_INTERN_QUOTA_BYTES
        );
        let err = intern_budget_for(usize::MAX).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "the intern budget for {} databases does not fit in usize",
                usize::MAX
            )
        );
    }
}
