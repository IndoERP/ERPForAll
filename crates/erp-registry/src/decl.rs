//! Contributions: what an addon declares about models and fields.
//!
//! Every type here is `const`-constructible so `#[model]`/`#[extend]` can emit declarations as
//! `static` items into [`crate::MODEL_DECLS`]. Tests build them at runtime and leak them
//! (`Box::leak`) to get the same `'static` shape. Attributes are `Option`: `None` means "not given",
//! so an extension never overrides what it did not mention (FIELD §4.2 merge rule).

use erp_fields::{Digits, Error, IndexKind, OnDelete, RangeBounds, Result, Value};

use crate::company_access::CompanyAccessDecl;
use crate::exclusion::{ExclusionDecl, PgExtension, TemporalKeyDecl};
use crate::spec::CompanyMatch;

use crate::partition::PartitionDecl;

/// An addon: its name and the addons it depends on ("Registration").
#[derive(Debug)]
pub struct AddonDescriptor {
    /// Technical name.
    pub name: &'static str,
    /// Names of the addons it depends on.
    pub depends: &'static [&'static str],
    /// Declared opt-in for exact numeric storage.
    /// Use [`Self::is_exact_only`] for the effective policy: mandatory names remain
    /// protected even when this raw flag is false.
    pub exact_only: bool,
}

impl AddonDescriptor {
    /// Effective numeric admission policy. Reserved business addon names cannot opt out.
    pub fn is_exact_only(&self) -> bool {
        self.exact_only || Self::requires_exact(self.name)
    }

    /// Whether an addon name is subject to the mandatory L-9 numeric policy.
    pub fn requires_exact(name: &str) -> bool {
        name.starts_with("account")
            || name.starts_with("stock")
            || matches!(name, "product" | "uom")
    }
}

/// The model class a declaration uses (`Model`, `AbstractModel`, `TransientModel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelKind {
    /// A regular model with a table.
    Regular,
    /// An abstract model (mixin): no table, no records.
    Abstract,
    /// A transient (wizard) model.
    Transient,
}

#[derive(Debug)]
pub struct ModelDecl {
    /// Declaring addon.
    pub addon: &'static str,
    /// Model name (`_name`).
    pub name: &'static str,
    /// Parent models (`_inherit`); contains `name` for an in-place extension.
    pub inherit: &'static [&'static str],
    /// Model class kind of this declaration.
    pub kind: ModelKind,
    /// `_description`.
    pub description: Option<&'static str>,
    /// `_table`.
    pub table: Option<&'static str>,
    /// `_log_access`.
    pub log_access: Option<bool>,
    /// `_rec_name`.
    pub rec_name: Option<&'static str>,
    /// `_rec_names_search`: the fields `display_name` searches (name search), when not just
    /// `_rec_name`.
    pub rec_names_search: Option<&'static [&'static str]>,
    /// `_active_name`.
    pub active_name: Option<&'static str>,
    /// `_order`.
    pub order: Option<&'static str>,
    /// `_inherits`: `(parent model, delegate many2one field)` pairs (MODEL §4.4, REL §4.4.7).
    pub inherits: &'static [(&'static str, &'static str)],
    /// `_check_company_auto` (REL §4.12).
    pub check_company_auto: Option<bool>,
    /// `company_match`: how records linking to this model must relate to its company (
    /// rule 1); `None` takes the default of the model's company scope.
    pub company_match: Option<CompanyMatch>,
    /// `company_access`: how the reader's active companies restrict the model's records (
    /// A1-D2); `None` when this definition does not say. Composed through the MRO, newest wins,
    /// each newer one at most as wide as the one before it.
    pub company_access: Option<CompanyAccessDecl>,
    /// `_allow_sudo_commands` (REL-060): `false` makes x2many commands that target this model run
    /// as the transaction's original principal, never as an elevated or switched one.
    pub allow_sudo_commands: Option<bool>,
    /// `_check_inherits_access`: whether the `_inherits` parents' access applies to the model's
    /// records (SEC-013/045; default true).
    pub inherits_access: Option<bool>,
    /// `_parent_name` (hierarchy operators, DOM §4.8).
    pub parent_name: Option<&'static str>,
    /// `_parent_store`: maintain `parent_path` (CRUD §4.11).
    pub parent_store: Option<bool>,
    /// Table fill factor in percent (10–100), `WITH (fillfactor = n)`: free space kept in each heap
    /// page so updates can stay on the page (HOT). `None` leaves PostgreSQL's default.
    pub fillfactor: Option<u8>,
    /// A field setting a restricted parameter of a model that did not allow it is a
    /// composition error. Allow-lists of all declarations of a model add up.
    pub field_parameters: &'static [&'static str],
    /// Fields declared in this declaration.
    pub fields: &'static [FieldDecl],
    /// SQL constraints and indexes declared on the model.
    pub table_objects: &'static [TableObjectDecl],
    /// `row_version = true`: the model gets a server-maintained
    /// `row_version` field. Once a declaration turns it on, a later one cannot turn it off.
    pub row_version: Option<bool>,
    /// Opt into transactional event capture. Defaults to false and composes by OR:
    /// an extension cannot disable another declaration's opt-in. Refused on abstract, transient
    /// and closed event-namespace models when the registry is built.
    pub eventful: bool,
    /// Select successful RPC reads for independent auditing.
    /// Default false; OR-composed through extensions and classical/abstract inheritance.
    pub audit_reads: bool,
    /// `owner_only_stored_fields`: `Some(co_owners)` lets only the addon of
    /// this, the model's creating declaration, and the listed co-owner addons add or change a
    /// materialized column of the model. `None` keeps the open extension policy. Refused on an
    /// extension, a runtime declaration and an abstract model when the registry is built.
    pub owner_only_stored_fields: Option<&'static [&'static str]>,
    /// PostgreSQL extensions the model's raw table objects need (`extensions = [...]`), installed
    /// by schema sync (design §5.3).
    pub extensions: &'static [PgExtension],
    /// A custom model created at runtime (`ir.model` with `state = 'manual'`): `x_name` is its
    /// `_rec_name` fallback (e). Set only by [`crate::Overlay`] for its custom
    /// models; the composer refuses any other declaration that sets it (custom models come from
    /// [`crate::Overlay::add_manual_model`]).
    pub custom: bool,
    /// An extension whose kind is the kind of the model it extends (runtime extensions that do
    /// not state one); `kind` is not checked against the model then. Only extensions may set it.
    pub kind_follows_model: bool,
    /// Declared at runtime from data (a model file or an overlay, see `ModelDef`), not by Rust
    /// code. A runtime declaration may not replace a table object a native declaration made.
    pub runtime: bool,
    /// Source file (for error messages and ordering inside an addon).
    pub file: &'static str,
    /// Source line.
    pub line: u32,
    /// Leaf partitioning: only the declaration that creates the model may set it;
    /// the registry refuses it on an extension.
    pub partition: Option<PartitionDecl>,
    /// A named id sequence shared with other models: the table's `id` draws from it
    /// instead of its own identity, so the ids of every model naming it never collide.
    pub id_sequence: Option<&'static str>,
    /// `None` when this declaration
    /// does not say. Only transient models may state it; composed through the MRO, newest wins
    /// (see [`crate::TransientPolicy`]).
    pub transient_max_age_seconds: Option<u64>,
    /// `None` when this declaration does not say. Only
    /// transient models may state it.
    pub transient_max_count: Option<u64>,
}

impl ModelDecl {
    /// A regular model named `name`, declared by `addon`, with no fields.
    pub const fn new(addon: &'static str, name: &'static str) -> Self {
        Self {
            addon,
            name,
            inherit: &[],
            kind: ModelKind::Regular,
            description: None,
            table: None,
            log_access: None,
            rec_name: None,
            rec_names_search: None,
            active_name: None,
            order: None,
            inherits: &[],
            check_company_auto: None,
            company_match: None,
            company_access: None,
            allow_sudo_commands: None,
            inherits_access: None,
            parent_name: None,
            parent_store: None,
            fillfactor: None,
            field_parameters: &[],
            fields: &[],
            table_objects: &[],
            row_version: None,
            eventful: false,
            audit_reads: false,
            owner_only_stored_fields: None,
            extensions: &[],
            custom: false,
            kind_follows_model: false,
            runtime: false,
            file: "",
            line: 0,
            partition: None,
            id_sequence: None,
            transient_max_age_seconds: None,
            transient_max_count: None,
        }
    }

    /// An in-place extension of `name`.
    pub const fn extension(
        addon: &'static str,
        name: &'static str,
        inherit: &'static [&'static str],
    ) -> Self {
        let mut decl = Self::new(addon, name);
        decl.inherit = inherit;
        decl
    }

    /// Set `_inherit`.
    pub const fn inherit(mut self, parents: &'static [&'static str]) -> Self {
        self.inherit = parents;
        self
    }

    /// Set the model kind.
    pub const fn kind(mut self, kind: ModelKind) -> Self {
        self.kind = kind;
        self
    }

    /// Set `_description`.
    pub const fn description(mut self, text: &'static str) -> Self {
        self.description = Some(text);
        self
    }

    /// Set `_table`.
    pub const fn table(mut self, table: &'static str) -> Self {
        self.table = Some(table);
        self
    }

    /// Set `_log_access`.
    pub const fn log_access(mut self, on: bool) -> Self {
        self.log_access = Some(on);
        self
    }

    /// Set `transient_max_age_seconds`: seconds after which rows of this transient model may be
    /// cleaned up; `0` disables the age threshold. Refused when the registry is built if the model
    /// is not transient or the value is above [`crate::MAX_TRANSIENT_MAX_AGE_SECONDS`].
    pub const fn transient_max_age_seconds(mut self, seconds: u64) -> Self {
        self.transient_max_age_seconds = Some(seconds);
        self
    }

    /// Set `transient_max_count`: the most rows this transient model keeps; `0` disables the
    /// count threshold. Refused when the registry is built if the model is not transient or the
    /// value is above [`crate::MAX_TRANSIENT_MAX_COUNT`].
    pub const fn transient_max_count(mut self, count: u64) -> Self {
        self.transient_max_count = Some(count);
        self
    }

    /// Set `_rec_name`.
    pub const fn rec_name(mut self, field: &'static str) -> Self {
        self.rec_name = Some(field);
        self
    }

    /// Set `_rec_names_search`.
    pub const fn rec_names_search(mut self, fields: &'static [&'static str]) -> Self {
        self.rec_names_search = Some(fields);
        self
    }

    /// Set `_active_name`.
    pub const fn active_name(mut self, field: &'static str) -> Self {
        self.active_name = Some(field);
        self
    }

    /// Set `_inherits`.
    pub const fn inherits(mut self, parents: &'static [(&'static str, &'static str)]) -> Self {
        self.inherits = parents;
        self
    }

    pub const fn inherits_access(mut self, on: bool) -> Self {
        self.inherits_access = Some(on);
        self
    }

    /// Set `_check_company_auto`.
    pub const fn check_company_auto(mut self, on: bool) -> Self {
        self.check_company_auto = Some(on);
        self
    }

    /// Set `company_match`.
    pub const fn company_match(mut self, m: CompanyMatch) -> Self {
        self.company_match = Some(m);
        self
    }

    /// Set `company_access` (A1-D2; see [`CompanyAccessDecl`]).
    pub const fn company_access(mut self, decl: CompanyAccessDecl) -> Self {
        self.company_access = Some(decl);
        self
    }

    /// Allow the model's fields to set restricted field parameters (see
    /// [`ModelDecl::field_parameters`]).
    pub const fn field_parameters(mut self, names: &'static [&'static str]) -> Self {
        self.field_parameters = names;
        self
    }

    /// Set `_allow_sudo_commands` (default `true`; REL-060).
    pub const fn allow_sudo_commands(mut self, on: bool) -> Self {
        self.allow_sudo_commands = Some(on);
        self
    }

    /// Set `_parent_name`.
    pub const fn parent_name(mut self, field: &'static str) -> Self {
        self.parent_name = Some(field);
        self
    }

    /// Set `_parent_store`.
    pub const fn parent_store(mut self, on: bool) -> Self {
        self.parent_store = Some(on);
        self
    }

    /// Set the table's fill factor (percent, 10–100; checked when the registry is built).
    pub const fn fillfactor(mut self, percent: u8) -> Self {
        self.fillfactor = Some(percent);
        self
    }

    /// Set `_order`.
    pub const fn order(mut self, order: &'static str) -> Self {
        self.order = Some(order);
        self
    }

    /// Set the declared fields.
    pub const fn fields(mut self, fields: &'static [FieldDecl]) -> Self {
        self.fields = fields;
        self
    }

    /// Set the table objects.
    pub const fn table_objects(mut self, objects: &'static [TableObjectDecl]) -> Self {
        self.table_objects = objects;
        self
    }

    /// Opt into transactional event capture.
    ///
    /// The composed flag is true when any declaration or extension sets it; false never disables
    /// a previous opt-in. Only regular models with their own table outside the event namespace
    /// may opt in. Rebuilding without an opting-in declaration removes its contribution.
    pub const fn eventful(mut self, on: bool) -> Self {
        self.eventful = on;
        self
    }

    /// Select successful RPC reads for auditing; false cannot undo another MRO opt-in.
    /// Abstract declarations contribute to descendants; transient models may opt in.
    pub const fn audit_reads(mut self, on: bool) -> Self {
        self.audit_reads = on;
        self
    }

    /// Protect the model's materialized columns: only this declaration's
    /// addon (the owner) and `co_owners` may add a stored column or change one's storage, type,
    /// relation or generated expression. Nonstored fields, VIRTUAL generated fields and companion
    /// models stay open to every addon. A co-owner that is installed must depend on the owner.
    pub const fn owner_only_stored_fields(mut self, co_owners: &'static [&'static str]) -> Self {
        self.owner_only_stored_fields = Some(co_owners);
        self
    }

    /// Set `row_version`: `true` adds the server-maintained `row_version` field.
    pub const fn row_version(mut self, on: bool) -> Self {
        self.row_version = Some(on);
        self
    }

    /// Set the PostgreSQL extensions the model's raw table objects need.
    pub const fn extensions(mut self, extensions: &'static [PgExtension]) -> Self {
        self.extensions = extensions;
        self
    }

    /// Let an extension take the kind of the model it extends (see
    /// [`ModelDecl::kind_follows_model`](ModelDecl#structfield.kind_follows_model)).
    pub const fn kind_follows_model(mut self, on: bool) -> Self {
        self.kind_follows_model = on;
        self
    }

    /// Partition the model's table by time: only on
    /// the declaration that creates the model.
    pub const fn partition(mut self, partition: PartitionDecl) -> Self {
        self.partition = Some(partition);
        self
    }

    /// Draw the table's ids from the named sequence `name`, shared by every model that names it.
    /// Checked when the registry is built: a valid identifier that is neither a table
    /// nor a table's own identity sequence; not on abstract or partitioned models.
    pub const fn id_sequence(mut self, name: &'static str) -> Self {
        self.id_sequence = Some(name);
        self
    }

    /// Whether `name` can be an id sequence name: a PostgreSQL identifier of ASCII letters,
    /// digits, `_` and `$`, not starting with a digit or `$`, at most 63 bytes. The `#[model]`
    /// macro asserts it at compile time (a `const` item), the registry again when it is built.
    pub const fn valid_id_sequence(name: &str) -> bool {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return false;
        }
        if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
            return false;
        }
        let mut i = 1;
        while i < bytes.len() {
            let b = bytes[i];
            if !(b.is_ascii_alphanumeric() || b == b'_' || b == b'$') {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Record the source location.
    pub const fn at(mut self, file: &'static str, line: u32) -> Self {
        self.file = file;
        self.line = line;
        self
    }

    /// True when this declaration extends an existing model in place.
    pub fn is_extension(&self) -> bool {
        self.inherit.contains(&self.name)
    }

    /// How error messages name this declaration.
    pub fn describe(&self) -> String {
        describe_definition(self.name, self.addon, self.file, self.line)
    }
}

/// How error messages name a model definition: `model definition 'x.doc' of addon 'x'`, with
/// ` (file:line)` when the file is known.
pub(crate) fn describe_definition(name: &str, addon: &str, file: &str, line: u32) -> String {
    if file.is_empty() {
        format!("model definition '{name}' of addon '{addon}'")
    } else {
        format!("model definition '{name}' of addon '{addon}' ({file}:{line})")
    }
}

/// A field kind with the parameters given for it.
#[derive(Debug, Clone, Copy)]
pub enum KindDecl {
    /// `Id` (only the core `base` model declares it).
    Id,
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
        currency_field: Option<&'static str>,
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
        values: Option<&'static [(&'static str, &'static str)]>,
        /// A method name replacing the inherited selection.
        method: Option<&'static str>,
        /// `selection_add` entries; a `None` label only positions an existing value.
        add: &'static [(&'static str, Option<&'static str>)],
        /// `ondelete` policies for added values.
        ondelete: &'static [(&'static str, SelectionOnDeleteDecl)],
    },
    /// `Date`.
    Date,
    /// Native UUID without identity policy.
    Uuid,
    /// Exact signed 64-bit scalar.
    BigInt,
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
        comodel: &'static str,
        /// `None` when not given.
        ondelete: Option<OnDelete>,
    },
    /// `One2many(comodel, inverse_name)`.
    One2many {
        /// Target model.
        comodel: &'static str,
        /// The comodel's many2one pointing back.
        inverse: &'static str,
    },
    /// `Many2many(comodel, relation, column1, column2, ondelete=...)`.
    Many2many {
        /// Target model.
        comodel: &'static str,
        /// Relation table (generated when `None`).
        relation: Option<&'static str>,
        /// Column of this model's ids (generated when `None`).
        column1: Option<&'static str>,
        /// Column of the comodel's ids (generated when `None`).
        column2: Option<&'static str>,
        /// `None` when not given (`cascade`).
        ondelete: Option<OnDelete>,
    },
    /// `Reference(selection=[(model, label), ...])`.
    Reference {
        /// Allowed models.
        models: Option<&'static [(&'static str, &'static str)]>,
        /// A method returning the allowed models.
        method: Option<&'static str>,
    },
    /// `Many2oneReference(model_field=...)`.
    Many2oneReference {
        /// The char field holding the model name.
        model_field: &'static str,
    },
    /// The `row_version` field. Only the registry declares it, for models with `row_version =
    /// true`; any other declaration of this kind is refused.
    RowVersion,
    /// `date_range(bounds=...)` (design §5.2).
    DateRange {
        /// Which ends the field's ranges include.
        bounds: RangeBounds,
    },
    /// `datetime_range(bounds=...)` (design §5.2).
    DatetimeRange {
        /// Which ends the field's ranges include.
        bounds: RangeBounds,
    },
}

/// Selection `ondelete` declaration for compile-time constants (/035).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionOnDeleteDecl {
    /// `'set null'`.
    SetNull,
    /// `'cascade'`.
    Cascade,
    /// `'set default'`.
    SetDefault,
    /// `'set VALUE'`.
    SetValue(&'static str),
    /// Callable: callback name.
    Callback(&'static str),
}

impl SelectionOnDeleteDecl {
    /// Parse an ondelete policy string for compile-time constant slices.
    pub fn parse(
        s: &'static str,
    ) -> std::result::Result<Self, erp_fields::ParseSelectionOnDeleteError> {
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
                Err(erp_fields::ParseSelectionOnDeleteError(s.to_owned()))
            } else {
                Ok(Self::SetValue(target))
            }
        } else {
            Err(erp_fields::ParseSelectionOnDeleteError(s.to_owned()))
        }
    }

    /// Parse an ondelete policy string with field context, returning the standard setup error message.
    pub fn parse_for_field(model: &str, field: &str, key: &str, s: &'static str) -> Result<Self> {
        Self::parse(s).map_err(|_| {
            Error::programming(format!(
                "{model}.{field}: ondelete policy {s:?} for selection value {key:?} is not a valid ondelete policy, please choose one of 'set null', 'set default', 'set [value]', 'cascade' or a callable"
            ))
        })
    }
}

/// A static default value.
#[derive(Debug, Clone, Copy)]
pub enum StaticValue {
    /// `False`.
    False,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string.
    Str(&'static str),
}

impl StaticValue {
    /// As a dynamic value.
    pub fn to_value(self) -> Value {
        match self {
            Self::False => Value::False,
            Self::Bool(b) => Value::Bool(b),
            Self::Int(i) => Value::Int(i),
            Self::Float(f) => Value::Float(f),
            Self::Str(s) => Value::str(s),
        }
    }
}

/// What a default function receives.
#[derive(Debug, Clone, Copy)]
pub struct DefaultContext {
    /// The current user id.
    pub uid: i64,
}

/// A computed default.
pub type DefaultFn = fn(&DefaultContext) -> Value;

/// A field's `default` parameter.
#[derive(Debug, Clone, Copy)]
pub enum DefaultDecl {
    Value(StaticValue),
    /// A function of the environment.
    Fn(DefaultFn),
    /// `default=None`: drop the inherited default.
    Remove,
}

pub const DEFAULT_TRACKING_SEQUENCE: u32 = 100;

/// One field declaration.
#[derive(Debug, Clone, Copy)]
pub struct FieldDecl {
    /// Field name.
    pub name: &'static str,
    /// Kind and kind parameters.
    pub kind: KindDecl,
    /// `string`.
    pub string: Option<&'static str>,
    /// `help`.
    pub help: Option<&'static str>,
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
    /// `default`.
    pub default: Option<DefaultDecl>,
    /// `prefetch` (default group when true).
    pub prefetch: Option<bool>,
    /// `compute` method name.
    pub compute: Option<&'static str>,
    /// `search_method` name for nonstored searchable fields.
    pub search_method: Option<&'static str>,
    /// `inverse` method name.
    pub inverse: Option<&'static str>,
    /// `related` path.
    pub related: Option<&'static str>,
    /// `precompute`.
    pub precompute: Option<bool>,
    /// `compute_sudo`.
    pub compute_sudo: Option<bool>,
    /// `groups`.
    pub groups: Option<&'static str>,
    /// Explicit `depends=`: replaces the dependencies collected from the compute methods.
    pub depends: Option<&'static [&'static str]>,
    /// Fields a compute reads without wanting a trigger: prefetched, no dependency
    /// edge.
    pub reads: Option<&'static [&'static str]>,
    /// Context keys the computed value depends on (`depends_context`, ENV §4.5).
    pub depends_context: Option<&'static [&'static str]>,
    /// Relational `domain=` in JSON list syntax (`[["important", "=", true]]`).
    pub domain: Option<&'static str>,
    /// Relational `context=` as a JSON object (`{"active_test": false}`).
    pub context: Option<&'static str>,
    /// `check_company` (REL §4.12).
    pub check_company: Option<bool>,
    /// `bypass_search_access` (REL §4.2).
    pub bypass_search_access: Option<bool>,
    /// `delegate` of a many2one (REL §4.4.7).
    pub delegate: Option<bool>,
    /// `server_set`: a `datetime` the ORM fills at insert from the transaction
    /// timestamp, like `create_date`; callers can never write it.
    pub server_set: Option<bool>,
    /// `company_dependent`.
    pub company_dependent: Option<bool>,
    /// `change_default`.
    pub change_default: Option<bool>,
    /// `user_writeable` (`res.users` only): a user may write the field on their
    /// own record without being a user manager.
    pub user_writeable: Option<bool>,
    /// `secret` (redaction v2): the field holds a credential. The audit trail replaces
    /// its value with `{"$redacted":"secret"}` before the call digest is hashed.
    pub secret: Option<bool>,
    /// `tracking`: `Some(Some(sequence))` tracks the field with that display
    /// sequence (`tracking = true` is [`DEFAULT_TRACKING_SEQUENCE`]); `Some(None)` is an explicit
    /// `tracking = false`, which switches tracking off when an extension re-declares the field.
    pub tracking: Option<Option<u32>>,
    /// `translate`: translatable textual field stored as JSONB with language keys.
    pub translate: Option<bool>,
    /// SQL collation declaration, validated at registration.
    pub collation: Option<&'static str>,
    /// `db_default`: schema sync keeps the static default as the column's database
    /// default, so a row written outside the ORM without the column takes it. Stored required
    /// booleans with a static boolean default only (checked at registry build).
    pub db_default: Option<bool>,
    /// Explicit host HTML policy; omitted Html declarations use the full default allowlists.
    pub html_policy: Option<erp_fields::html::FieldPolicy>,
    /// Generated column expression: `Some(Some(expr))` to set, `Some(None)` for explicit removal.
    pub generated: Option<Option<&'static str>>,
    /// Storage kind of the generated column (virtual or stored).
    pub generated_storage: Option<crate::generated::GeneratedStorage>,
    /// Owner-only opt-in to the generated input adapter.
    pub input_adapted: Option<bool>,
}

impl FieldDecl {
    /// Choose sanitizing or explicit untrusted text. No privilege bypass exists.
    pub const fn sanitize(self, enabled: bool) -> Self {
        self.html_policy(if enabled {
            erp_fields::html::FieldPolicy::Sanitize(erp_fields::html::SanitizePolicy::Default(
                erp_fields::html::DefaultPolicy::new(),
            ))
        } else {
            erp_fields::html::FieldPolicy::UnsanitisedText
        })
    }

    /// Select the host HTML policy. Non-HTML fields are refused at final registry assembly.
    pub const fn html_policy(mut self, policy: erp_fields::html::FieldPolicy) -> Self {
        self.html_policy = Some(policy);
        self
    }

    /// Declare SQL collation (`default` or `C`); registration validates the name and storage.
    pub const fn collation(mut self, name: &'static str) -> Self {
        self.collation = Some(name);
        self
    }

    /// A field with only a name and a kind.
    pub const fn new(name: &'static str, kind: KindDecl) -> Self {
        Self {
            name,
            kind,
            string: None,
            help: None,
            required: None,
            readonly: None,
            store: None,
            copy: None,
            index: None,
            default: None,
            prefetch: None,
            compute: None,
            search_method: None,
            inverse: None,
            related: None,
            precompute: None,
            compute_sudo: None,
            groups: None,
            depends: None,
            reads: None,
            depends_context: None,
            domain: None,
            context: None,
            check_company: None,
            bypass_search_access: None,
            delegate: None,
            server_set: None,
            company_dependent: None,
            change_default: None,
            user_writeable: None,
            secret: None,
            tracking: None,
            translate: None,
            collation: None,
            db_default: None,
            html_policy: None,
            generated: None,
            generated_storage: None,
            input_adapted: None,
        }
    }

    /// `One2many`.
    pub const fn one2many(
        name: &'static str,
        comodel: &'static str,
        inverse: &'static str,
    ) -> Self {
        Self::new(name, KindDecl::One2many { comodel, inverse })
    }

    /// `Many2many` with a generated relation.
    pub const fn many2many(name: &'static str, comodel: &'static str) -> Self {
        Self::new(
            name,
            KindDecl::Many2many {
                comodel,
                relation: None,
                column1: None,
                column2: None,
                ondelete: None,
            },
        )
    }

    /// `Reference` over a static list of models.
    pub const fn reference(
        name: &'static str,
        models: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self::new(
            name,
            KindDecl::Reference {
                models: Some(models),
                method: None,
            },
        )
    }

    /// `Many2oneReference`.
    pub const fn many2one_reference(name: &'static str, model_field: &'static str) -> Self {
        Self::new(name, KindDecl::Many2oneReference { model_field })
    }

    /// Set the relation table and columns of a `Many2many`.
    pub const fn relation(
        mut self,
        relation: &'static str,
        column1: &'static str,
        column2: &'static str,
    ) -> Self {
        if let KindDecl::Many2many {
            comodel, ondelete, ..
        } = self.kind
        {
            self.kind = KindDecl::Many2many {
                comodel,
                relation: Some(relation),
                column1: Some(column1),
                column2: Some(column2),
                ondelete,
            };
        }
        self
    }

    /// Set a relational `domain` (JSON list syntax).
    pub const fn domain(mut self, domain: &'static str) -> Self {
        self.domain = Some(domain);
        self
    }

    /// Set a relational `context` (JSON object).
    pub const fn context(mut self, context: &'static str) -> Self {
        self.context = Some(context);
        self
    }

    /// Set `check_company`.
    pub const fn check_company(mut self, on: bool) -> Self {
        self.check_company = Some(on);
        self
    }

    /// Set `bypass_search_access`.
    pub const fn bypass_search_access(mut self, on: bool) -> Self {
        self.bypass_search_access = Some(on);
        self
    }

    /// Set `delegate`.
    pub const fn delegate(mut self, on: bool) -> Self {
        self.delegate = Some(on);
        self
    }

    /// Set `server_set`: the ORM fills the field at insert from the transaction timestamp and
    /// refuses every value a caller gives (stored `datetime` fields only, checked by the registry).
    pub const fn server_set(mut self, on: bool) -> Self {
        self.server_set = Some(on);
        self
    }

    /// `Boolean`.
    pub const fn boolean(name: &'static str) -> Self {
        Self::new(name, KindDecl::Boolean)
    }
    /// `Integer`.
    pub const fn integer(name: &'static str) -> Self {
        Self::new(name, KindDecl::Integer)
    }
    /// `Float` without digits.
    pub const fn float(name: &'static str) -> Self {
        Self::new(name, KindDecl::Float { digits: None })
    }
    /// `Float` with digits.
    pub const fn float_digits(name: &'static str, digits: Digits) -> Self {
        Self::new(
            name,
            KindDecl::Float {
                digits: Some(digits),
            },
        )
    }
    /// `Char`.
    pub const fn char(name: &'static str) -> Self {
        Self::new(name, KindDecl::Char { size: None })
    }
    /// `Text`.
    pub const fn text(name: &'static str) -> Self {
        Self::new(name, KindDecl::Text)
    }
    /// `Html`.
    pub const fn html(name: &'static str) -> Self {
        Self::new(name, KindDecl::Html)
    }
    /// `Date`.
    pub const fn date(name: &'static str) -> Self {
        Self::new(name, KindDecl::Date)
    }
    /// Native UUID.
    pub const fn uuid(name: &'static str) -> Self {
        Self::new(name, KindDecl::Uuid)
    }

    /// A signed 64-bit scalar.
    pub const fn bigint(name: &'static str) -> Self {
        Self::new(name, KindDecl::BigInt)
    }

    /// Native full-width unsigned transaction identifier.
    pub const fn xid8(name: &'static str) -> Self {
        Self::new(name, KindDecl::Xid8)
    }

    /// Explicit-offset, bounded microsecond instant.
    pub const fn timestamptz(name: &'static str) -> Self {
        Self::new(name, KindDecl::TimestampTz)
    }

    /// `Datetime`.
    pub const fn datetime(name: &'static str) -> Self {
        Self::new(name, KindDecl::Datetime)
    }
    /// `Json`.
    pub const fn json(name: &'static str) -> Self {
        Self::new(name, KindDecl::Json)
    }
    /// `date_range` with `bounds`.
    pub const fn date_range(name: &'static str, bounds: RangeBounds) -> Self {
        Self::new(name, KindDecl::DateRange { bounds })
    }
    /// `datetime_range` with `bounds`.
    pub const fn datetime_range(name: &'static str, bounds: RangeBounds) -> Self {
        Self::new(name, KindDecl::DatetimeRange { bounds })
    }
    /// `Selection` with static values.
    pub const fn selection(
        name: &'static str,
        values: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self::new(
            name,
            KindDecl::Selection {
                values: Some(values),
                method: None,
                add: &[],
                ondelete: &[],
            },
        )
    }
    /// `Selection(selection_add=...)`.
    pub const fn selection_add(
        name: &'static str,
        add: &'static [(&'static str, Option<&'static str>)],
    ) -> Self {
        Self::new(
            name,
            KindDecl::Selection {
                values: None,
                method: None,
                add,
                ondelete: &[],
            },
        )
    }
    /// `Selection(selection_add=..., ondelete=...)`.
    pub const fn selection_add_ondelete(
        name: &'static str,
        add: &'static [(&'static str, Option<&'static str>)],
        ondelete: &'static [(&'static str, SelectionOnDeleteDecl)],
    ) -> Self {
        Self::new(
            name,
            KindDecl::Selection {
                values: None,
                method: None,
                add,
                ondelete,
            },
        )
    }
    /// `Many2one`.
    pub const fn many2one(name: &'static str, comodel: &'static str) -> Self {
        Self::new(
            name,
            KindDecl::Many2one {
                comodel,
                ondelete: None,
            },
        )
    }

    /// Set `string`.
    pub const fn string(mut self, label: &'static str) -> Self {
        self.string = Some(label);
        self
    }
    /// Set `help`.
    pub const fn help(mut self, text: &'static str) -> Self {
        self.help = Some(text);
        self
    }
    /// Set `required`.
    pub const fn required(mut self, on: bool) -> Self {
        self.required = Some(on);
        self
    }
    /// Set `readonly`.
    pub const fn readonly(mut self, on: bool) -> Self {
        self.readonly = Some(on);
        self
    }
    /// Set `store`.
    pub const fn store(mut self, on: bool) -> Self {
        self.store = Some(on);
        self
    }
    /// Set `copy`.
    pub const fn copy(mut self, on: bool) -> Self {
        self.copy = Some(on);
        self
    }
    /// Set `index` (`None` for `index=False`).
    pub const fn index(mut self, kind: Option<IndexKind>) -> Self {
        self.index = Some(kind);
        self
    }
    /// Set a static default.
    pub const fn default(mut self, value: StaticValue) -> Self {
        self.default = Some(DefaultDecl::Value(value));
        self
    }
    /// Set a default function.
    pub const fn default_fn(mut self, f: DefaultFn) -> Self {
        self.default = Some(DefaultDecl::Fn(f));
        self
    }
    /// `default=None`.
    pub const fn no_default(mut self) -> Self {
        self.default = Some(DefaultDecl::Remove);
        self
    }
    /// Set `prefetch`.
    pub const fn prefetch(mut self, on: bool) -> Self {
        self.prefetch = Some(on);
        self
    }
    /// Set `compute`.
    pub const fn compute(mut self, method: &'static str) -> Self {
        self.compute = Some(method);
        self
    }
    /// Set `search_method`.
    pub const fn search_method(mut self, method: &'static str) -> Self {
        self.search_method = Some(method);
        self
    }
    /// Set `inverse`.
    pub const fn inverse(mut self, method: &'static str) -> Self {
        self.inverse = Some(method);
        self
    }
    /// Set `related`.
    pub const fn related(mut self, path: &'static str) -> Self {
        self.related = Some(path);
        self
    }
    /// Set `precompute`.
    pub const fn precompute(mut self, on: bool) -> Self {
        self.precompute = Some(on);
        self
    }
    /// Set `compute_sudo`.
    pub const fn compute_sudo(mut self, on: bool) -> Self {
        self.compute_sudo = Some(on);
        self
    }
    /// Set `groups`.
    pub const fn groups(mut self, groups: &'static str) -> Self {
        self.groups = Some(groups);
        self
    }
    /// Set `depends` (explicit dependencies of a computed field).
    pub const fn depends(mut self, paths: &'static [&'static str]) -> Self {
        self.depends = Some(paths);
        self
    }
    /// Set `reads` (fields a compute reads without a trigger).
    pub const fn reads(mut self, names: &'static [&'static str]) -> Self {
        self.reads = Some(names);
        self
    }
    /// Set `depends_context`.
    pub const fn depends_context(mut self, keys: &'static [&'static str]) -> Self {
        self.depends_context = Some(keys);
        self
    }
    /// Set `size` on a `Char`.
    pub const fn size(mut self, size: u32) -> Self {
        if let KindDecl::Char { .. } = self.kind {
            self.kind = KindDecl::Char { size: Some(size) };
        }
        self
    }
    /// Set `ondelete` on a `Many2one`.
    pub const fn ondelete(mut self, action: OnDelete) -> Self {
        match self.kind {
            KindDecl::Many2one { comodel, .. } => {
                self.kind = KindDecl::Many2one {
                    comodel,
                    ondelete: Some(action),
                };
            }
            KindDecl::Many2many {
                comodel,
                relation,
                column1,
                column2,
                ..
            } => {
                self.kind = KindDecl::Many2many {
                    comodel,
                    relation,
                    column1,
                    column2,
                    ondelete: Some(action),
                };
            }
            _ => {}
        }
        self
    }
    /// Set `ondelete` policies on a `Selection`.
    pub const fn selection_ondelete(
        mut self,
        policies: &'static [(&'static str, SelectionOnDeleteDecl)],
    ) -> Self {
        if let KindDecl::Selection {
            values,
            method,
            add,
            ..
        } = self.kind
        {
            self.kind = KindDecl::Selection {
                values,
                method,
                add,
                ondelete: policies,
            };
        }
        self
    }
    /// Set `company_dependent`.
    pub const fn company_dependent(mut self, on: bool) -> Self {
        self.company_dependent = Some(on);
        self
    }
    /// Set `change_default`.
    pub const fn change_default(mut self, on: bool) -> Self {
        self.change_default = Some(on);
        self
    }

    /// Track the field's changes with display `sequence` (`tracking = <sequence>`; `true` is
    /// [`DEFAULT_TRACKING_SEQUENCE`]). The composed model refuses `0`.
    pub const fn tracking(mut self, sequence: u32) -> Self {
        self.tracking = Some(Some(sequence));
        self
    }

    /// Switch tracking off (`tracking = false`), also on a field an earlier declaration tracks.
    pub const fn untracked(mut self) -> Self {
        self.tracking = Some(None);
        self
    }

    /// Set `user_writeable` (a field of `res.users` the user may write on their own record).
    pub const fn user_writeable(mut self, on: bool) -> Self {
        self.user_writeable = Some(on);
        self
    }

    /// Mark the field as holding a credential (a password): the audit digest never covers its
    /// value (`{"$redacted":"secret"}`).
    pub const fn secret(mut self, on: bool) -> Self {
        self.secret = Some(on);
        self
    }

    /// Keep the static default as the column's database default, for a fail-closed
    /// flag that rows written outside the ORM must not be able to leave out. Registry build
    /// refuses it unless the field is a stored, required boolean with a static boolean default,
    /// not company-dependent and not generated; `false` switches it off in an extension.
    ///
    /// ```
    /// use erp_registry::{FieldDecl, StaticValue};
    /// let share = FieldDecl::boolean("share")
    ///     .required(true)
    ///     .default(StaticValue::Bool(true))
    ///     .db_default(true);
    /// assert_eq!(share.db_default, Some(true));
    /// ```
    pub const fn db_default(mut self, on: bool) -> Self {
        self.db_default = Some(on);
        self
    }

    /// Set translation metadata for a Char, Text or Html field.
    ///
    /// Registry build rejects other types, company-dependent combinations and
    /// translated index keys. ORM construction refuses translated fields until
    /// phase 3/4 supplies runtime persistence.
    ///
    /// ```
    /// use erp_registry::FieldDecl;
    /// let field = FieldDecl::char("name").translate(true);
    /// assert_eq!(field.translate, Some(true));
    /// ```
    pub const fn translate(mut self, on: bool) -> Self {
        self.translate = Some(on);
        self
    }

    /// Allow create/write input to be normalized by `_adapt_generated_input`.
    /// Only the field's declaring addon may set or clear this option. It does not make the
    /// database-generated column assignment-eligible.
    ///
    /// ```
    /// use erp_registry::FieldDecl;
    /// let input = FieldDecl::integer("total").generated("source + 1").input_adapted(true);
    /// assert_eq!(input.input_adapted, Some(true));
    /// ```
    pub const fn input_adapted(mut self, enabled: bool) -> Self {
        self.input_adapted = Some(enabled);
        self
    }

    /// Set the expression for a database-generated column.
    pub const fn generated(mut self, expr: &'static str) -> Self {
        self.generated = Some(Some(expr));
        self
    }

    /// Set the storage kind of a database-generated column (virtual or stored).
    pub const fn generated_storage(mut self, storage: crate::generated::GeneratedStorage) -> Self {
        self.generated_storage = Some(storage);
        self
    }

    /// Explicitly remove generation (refused).
    pub const fn remove_generated(mut self) -> Self {
        self.generated = Some(None);
        self
    }
}

/// A SQL table object declared on a model (`models.Constraint`, `Index`, `UniqueIndex`).
#[derive(Debug, Clone, Copy)]
pub struct TableObjectDecl {
    /// Attribute name, starting with `_` (e.g. `_positive_color`).
    pub name: &'static str,
    /// What to create.
    pub kind: TableObjectKind,
}

/// The kind and definition of a table object.
///
/// Deliberately not `#[non_exhaustive]`: schema sync, the error mapping and the composer match it
/// exhaustively, so that a new kind does not compile until each of them has decided what it does
/// with it (the same rule as [`TriggerSignal`]). Match it with a wildcard only where "any other
/// kind" is the specified behaviour.
#[derive(Debug, Clone, Copy)]
pub enum TableObjectKind {
    /// `ALTER TABLE … ADD CONSTRAINT <name> <definition>`.
    Constraint {
        /// e.g. `CHECK(color >= 0)`.
        definition: &'static str,
        /// User message when violated.
        message: &'static str,
    },
    /// `CREATE INDEX <name> ON <table> <definition>`.
    Index {
        /// e.g. `(base_id, value)`.
        definition: &'static str,
    },
    /// `CREATE UNIQUE INDEX <name> ON <table> <definition>`.
    UniqueIndex {
        /// e.g. `(base_id, value)`.
        definition: &'static str,
        /// User message when violated.
        message: &'static str,
    },
    /// A typed index over field names (design): no SQL, so any
    /// source may declare it. See [`crate::IndexDecl`].
    TypedIndex(crate::IndexDecl),
    // ----: typed table objects (D9) ----
    /// A business key: `ALTER TABLE … ADD CONSTRAINT <name> UNIQUE NULLS NOT DISTINCT (…)`.
    Key(KeyDecl),
    /// A statement-level trigger that bumps a signal generation once per transaction whenever
    /// the table changes. The trigger
    /// function belongs to the core: an addon names the table and the signal, never SQL.
    SignalTrigger(SignalTriggerDecl),
    /// A statement-level `AFTER DELETE` trigger that keeps a key table and a named cache in step
    /// with deletions the database makes on its own (foreign-key cascades, SQL outside the ORM):
    /// see [`FollowDeleteDecl`]. The trigger function belongs to the core.
    FollowDelete(FollowDeleteDecl),
    // ----- typed objects (field names, never SQL) -----
    /// A typed exclusion constraint over a two-field period (`exclude(...)`).
    Exclusion(ExclusionDecl),
    /// A `UNIQUE (…, range WITHOUT OVERLAPS)` key on a range field (`key(..., without_overlaps =
    /// ...)`).
    TemporalKey(TemporalKeyDecl),
}

/// A business key: a named `UNIQUE NULLS NOT DISTINCT` constraint over
/// stored columns of the model's own table, the arbiter of `upsert`. A NULL part means "none"
/// (a quant without a lot), so two rows that differ only by NULL parts conflict.
///
/// Declared with `keys = [key(name = "_quant_key", fields = [...], message = "...")]` in
/// `#[model]`/`#[extend]`; composed by name with the other table objects (later declaration
/// wins). The registry checks the fields when it is built.
///
/// ```
/// use erp_registry::{KeyDecl, TableObjectDecl, TableObjectKind};
/// static KEY: TableObjectDecl = TableObjectDecl {
///     name: "_code_uniq",
///     kind: TableObjectKind::Key(KeyDecl::new(&["code", "company_id"], "The code must be unique.")),
/// };
/// let TableObjectKind::Key(key) = KEY.kind else { unreachable!() };
/// assert!(!key.nulls_distinct);
/// ```
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct KeyDecl {
    /// Field names, in the constraint's column order.
    pub fields: &'static [&'static str],
    /// User message when a row would duplicate a key (shown as `The operation cannot be
    /// completed: <message>`); must not be empty.
    pub message: &'static str,
    /// `nulls_distinct = true`: a plain `UNIQUE` whose NULL parts never conflict. The key still
    /// enforces uniqueness of rows without NULL parts but cannot arbitrate an upsert.
    pub nulls_distinct: bool,
}

impl KeyDecl {
    /// A `NULLS NOT DISTINCT` key over `fields`.
    pub const fn new(fields: &'static [&'static str], message: &'static str) -> Self {
        Self {
            fields,
            message,
            nulls_distinct: false,
        }
    }

    /// Opt out of `NULLS NOT DISTINCT` (see [`KeyDecl::nulls_distinct`](KeyDecl#structfield.nulls_distinct)).
    pub const fn nulls_distinct(mut self, on: bool) -> Self {
        self.nulls_distinct = on;
        self
    }
}

/// Longest cache name a [`FollowDeleteDecl`] may bump, in bytes (the name goes into a
/// transaction-local setting of the trigger function).
pub const MAX_FOLLOW_CACHE_LEN: usize = 40;

/// What a [`TableObjectKind::FollowDelete`] trigger does after rows of its table are deleted, by
/// any statement: with [`FollowDeleteDecl::deleting_keys`], the rows of the key table with the
/// deleted ids are deleted too; and the generation of the named cache `cache` (`cache:<name>`,
/// ) is bumped once per transaction, so every process drops its cached values. The
/// ORM's own `unlink` invalidates the cache in the deleting transaction as well, also for rows a
/// foreign key deletes by cascade (`erp-orm` follows the cascades of the registry).
///
/// Built for tables that share an id space with a key table (`ir_actions` and the
/// action tables, design), where a foreign-key cascade on the typed
/// table would otherwise leave the key row behind. The key table must have a stored char field
/// `type` naming each key row's model: only the key rows of the trigger's own model are deleted.
/// `TRUNCATE` cannot be followed (no transition table), so a companion `BEFORE TRUNCATE` trigger
/// refuses it, also when a `TRUNCATE … CASCADE` of another table reaches the followed one. A
/// `DELETE` sent as SQL in a transaction makes that transaction forget the same caches.
///
/// ```
/// use erp_registry::{TableObjectDecl, TableObjectKind};
///
/// const KEYS: TableObjectDecl = TableObjectDecl {
///     name: "_keys_follow",
///     kind: TableObjectKind::FollowDelete(
///         TableObjectKind::follow_delete("actions").deleting_keys("ir_actions"),
///     ),
/// };
/// let TableObjectKind::FollowDelete(decl) = KEYS.kind else { unreachable!() };
/// assert_eq!(decl.key_table, Some("ir_actions"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FollowDeleteDecl {
    /// Where it is attached: the model's table, or the relation table of one of its many2many
    /// fields (rows a cascade from the comodel removes change the cached values too).
    pub table: TriggerTable,
    /// The table whose rows with the deleted ids are deleted (model table only).
    pub key_table: Option<&'static str>,
    /// The named cache whose generation is bumped: `[a-z0-9_]`, 1 to
    /// [`MAX_FOLLOW_CACHE_LEN`] bytes.
    pub cache: &'static str,
}

impl FollowDeleteDecl {
    /// Attach it to the relation table of the model's many2many field `field` instead (no key
    /// table there: the relation's rows carry no ids of the key space).
    #[must_use]
    pub const fn on_relation(mut self, field: &'static str) -> Self {
        self.table = TriggerTable::Relation(field);
        self
    }

    /// Delete the rows of `key_table` that have the ids of the deleted rows.
    #[must_use]
    pub const fn deleting_keys(mut self, key_table: &'static str) -> Self {
        self.key_table = Some(key_table);
        self
    }

    /// Whether `name` is a cache name a follow-delete trigger can bump.
    pub const fn valid_cache(name: &str) -> bool {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_FOLLOW_CACHE_LEN {
            return false;
        }
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl TableObjectKind {
    /// A [`FollowDeleteDecl`] on the model's table bumping `cache` (for the `#[model]` macro, which
    /// names no type outside `TableObjectKind`).
    pub const fn follow_delete(cache: &'static str) -> FollowDeleteDecl {
        FollowDeleteDecl {
            table: TriggerTable::Model,
            key_table: None,
            cache,
        }
    }
}

/// The generation a [`TableObjectKind::SignalTrigger`] bumps.
///
/// Deliberately exhaustive: schema sync (`erp-orm`) matches it to build each signal's trigger
/// function, so a new signal does not compile until its function is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerSignal {
    /// The access policy (`policy`): `ir.access`, groups, capabilities, field groups.
    Policy,
    /// Who belongs to what (`membership`): group memberships, companies, the `active` and `share`
    /// flags of users. Nothing is compiled from it; the generation lets a read
    /// replica that has not replayed a membership change be detected.
    Membership,
}

impl TriggerSignal {
    /// The signal key the trigger inserts (`policy`).
    pub const fn key(self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::Membership => "membership",
        }
    }

    /// The signal of `key`, for the `#[model(signal_triggers = ...)]` parser.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "policy" => Some(Self::Policy),
            "membership" => Some(Self::Membership),
            _ => None,
        }
    }
}

/// The table a [`SignalTriggerDecl`] is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TriggerTable {
    /// The model's own table.
    Model,
    /// The relation table of the model's stored many2many field of this name.
    Relation(&'static str),
}

/// A statement-level trigger bumping `signal` on every `INSERT`, `UPDATE`, `DELETE` and
/// `TRUNCATE` of a table, or on every one but `UPDATE` ([`SignalTriggerDecl::without_updates`]).
///
/// ```
/// use erp_registry::{SignalTriggerDecl, TableObjectDecl, TableObjectKind, TriggerSignal};
///
/// const IMPLIED: TableObjectDecl = TableObjectDecl {
///     name: "_policy_bump_implied",
///     kind: TableObjectKind::SignalTrigger(
///         SignalTriggerDecl::new(TriggerSignal::Policy).on_relation("implied_ids"),
///     ),
/// };
/// assert!(matches!(
///     IMPLIED.kind,
///     TableObjectKind::SignalTrigger(d) if d.events() == "INSERT OR UPDATE OR DELETE OR TRUNCATE"
/// ));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SignalTriggerDecl {
    /// What it bumps.
    pub signal: TriggerSignal,
    /// Where it is attached.
    pub table: TriggerTable,
    /// Whether `UPDATE` fires it.
    pub updates: bool,
    /// When not empty, `UPDATE` fires it only for these columns (`UPDATE OF a, b`); `INSERT`,
    /// `DELETE` and `TRUNCATE` always fire it. Needs `updates`.
    pub update_columns: &'static [&'static str],
}

impl SignalTriggerDecl {
    /// A trigger on the model's table firing on every change.
    pub const fn new(signal: TriggerSignal) -> Self {
        Self {
            signal,
            table: TriggerTable::Model,
            updates: true,
            update_columns: &[],
        }
    }

    /// Attach it to the relation table of the model's many2many field `field` instead.
    #[must_use]
    pub const fn on_relation(mut self, field: &'static str) -> Self {
        self.table = TriggerTable::Relation(field);
        self
    }

    /// Fire on `INSERT`, `DELETE` and `TRUNCATE` only (updates of the table's columns do not
    /// change what the signal covers).
    #[must_use]
    pub const fn without_updates(mut self) -> Self {
        self.updates = false;
        self
    }

    /// Fire on `INSERT`, `DELETE` and `TRUNCATE`, and on `UPDATE` only when one of `columns`
    /// is assigned (`UPDATE OF`): the other columns do not change what the signal covers.
    #[must_use]
    pub const fn updates_of(mut self, columns: &'static [&'static str]) -> Self {
        self.updates = true;
        self.update_columns = columns;
        self
    }

    /// The trigger's events in SQL as the DDL spells them (`INSERT OR UPDATE OR DELETE OR
    /// TRUNCATE`, or `... OR UPDATE OF "a", "b"` for [`SignalTriggerDecl::updates_of`]).
    pub fn events_sql(&self) -> String {
        if self.updates && !self.update_columns.is_empty() {
            let columns: Vec<String> = self
                .update_columns
                .iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect();
            format!(
                "INSERT OR DELETE OR TRUNCATE OR UPDATE OF {}",
                columns.join(", ")
            )
        } else {
            self.events().to_owned()
        }
    }

    /// The trigger's events in SQL (`INSERT OR UPDATE OR DELETE OR TRUNCATE`); see
    /// [`SignalTriggerDecl::events_sql`] for the column-restricted form.
    pub const fn events(&self) -> &'static str {
        if self.updates {
            "INSERT OR UPDATE OR DELETE OR TRUNCATE"
        } else {
            "INSERT OR DELETE OR TRUNCATE"
        }
    }
}
