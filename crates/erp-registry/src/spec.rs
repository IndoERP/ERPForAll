//! The composed registry: one [`ModelSpec`] per model with its merged [`FieldSpec`]s.

use erp_fields::{ColumnType, Error, FieldType, IndexKind, Result, Storage};
use rustc_hash::FxHashMap;

use crate::decl::{
    AddonDescriptor, DefaultFn, ModelDecl, ModelKind, SignalTriggerDecl, StaticValue,
    TableObjectKind, TriggerTable,
};
use crate::drop::DroppedField;
use crate::partition::Partition;

/// Index of a model in its registry. Only the registry creates them; obtain one from
/// [`Registry::model_id`] or [`ModelSpec::id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelId(pub(crate) u32);

/// Index of a field in its model. Only the registry creates them; obtain one from
/// [`ModelSpec::field_id`] or [`FieldSpec::id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldId(pub(crate) u32);

impl ModelId {
    /// As a vector index.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl FieldId {
    /// As a vector index.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// ```
/// use erp_registry::CompanyMatch;
/// assert_eq!(CompanyMatch::parse("parent_of").unwrap(), CompanyMatch::ParentOf);
/// assert_eq!(CompanyMatch::ParentOf.as_str(), "parent_of");
/// assert!(CompanyMatch::parse("anything").is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CompanyMatch {
    /// The target has no company or one of the linking record's companies (`company_id`).
    Exact,
    /// The target has no company, or its company is a linking company or one of its ancestors
    /// (`company_id`, needs `res.company` to keep a parent path).
    ParentOf,
    /// The linking record has no company, or the target's `company_ids` meet its companies.
    AnyOf,
    /// The linking record has no company, or some company of the target's `company_ids` is a linking
    /// company or one of its ancestors (needs a parent path on `res.company`).
    ParentOfAny,
}

impl CompanyMatch {
    /// The four spellings, in declaration order.
    pub const NAMES: [&'static str; 4] = ["exact", "parent_of", "any_of", "parent_of_any"];

    /// Parse `"exact"`, `"parent_of"`, `"any_of"` or `"parent_of_any"`.
    ///
    /// # Errors
    ///
    /// `Programming` naming the four spellings for anything else.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "exact" => Ok(Self::Exact),
            "parent_of" => Ok(Self::ParentOf),
            "any_of" => Ok(Self::AnyOf),
            "parent_of_any" => Ok(Self::ParentOfAny),
            other => Err(Error::programming(format!(
                "company_match must be one of \"exact\", \"parent_of\", \"any_of\", \
                 \"parent_of_any\" (got \"{other}\")"
            ))),
        }
    }

    /// The spelling `parse` accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::ParentOf => "parent_of",
            Self::AnyOf => "any_of",
            Self::ParentOfAny => "parent_of_any",
        }
    }

    /// True for the two forms that walk ancestors and need a parent path on `res.company`.
    pub fn needs_parent_path(self) -> bool {
        matches!(self, Self::ParentOf | Self::ParentOfAny)
    }

    /// The form as a [`SingleMatch`] (`company_id` models), `None` for the `company_ids` forms.
    pub fn single(self) -> Option<SingleMatch> {
        match self {
            Self::Exact => Some(SingleMatch::Exact),
            Self::ParentOf => Some(SingleMatch::ParentOf),
            Self::AnyOf | Self::ParentOfAny => None,
        }
    }

    /// The form as a [`ManyMatch`] (`company_ids` models), `None` for the `company_id` forms.
    pub fn many(self) -> Option<ManyMatch> {
        match self {
            Self::AnyOf => Some(ManyMatch::AnyOf),
            Self::ParentOfAny => Some(ManyMatch::ParentOfAny),
            Self::Exact | Self::ParentOf => None,
        }
    }
}

/// The match of a model scoped by a `company_id` many2one: the two forms that make sense there,
/// so a `Single` scope cannot hold a `company_ids` form.
///
/// ```
/// use erp_registry::{CompanyMatch, SingleMatch};
/// assert_eq!(CompanyMatch::from(SingleMatch::ParentOf), CompanyMatch::ParentOf);
/// assert_eq!(CompanyMatch::AnyOf.single(), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SingleMatch {
    /// [`CompanyMatch::Exact`].
    Exact,
    /// [`CompanyMatch::ParentOf`].
    ParentOf,
}

/// The match of a model scoped by a `company_ids` many2many: the two forms that make sense there.
///
/// ```
/// use erp_registry::{CompanyMatch, ManyMatch};
/// assert_eq!(CompanyMatch::from(ManyMatch::AnyOf), CompanyMatch::AnyOf);
/// assert_eq!(CompanyMatch::Exact.many(), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ManyMatch {
    /// [`CompanyMatch::AnyOf`].
    AnyOf,
    /// [`CompanyMatch::ParentOfAny`].
    ParentOfAny,
}

impl From<SingleMatch> for CompanyMatch {
    fn from(m: SingleMatch) -> Self {
        match m {
            SingleMatch::Exact => Self::Exact,
            SingleMatch::ParentOf => Self::ParentOf,
        }
    }
}

impl From<ManyMatch> for CompanyMatch {
    fn from(m: ManyMatch) -> Self {
        match m {
            ManyMatch::AnyOf => Self::AnyOf,
            ManyMatch::ParentOfAny => Self::ParentOfAny,
        }
    }
}

/// How a model's records belong to companies.
///
/// ```
/// use erp_registry::CompanyScope;
/// assert_eq!(CompanyScope::Itself.matching(), None);
/// assert_eq!(CompanyScope::Itself.field(), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompanyScope {
    /// `res.company`: the record is its own company.
    Itself,
    /// A `company_id` many2one.
    Single {
        /// The `company_id` field.
        field: FieldId,
        /// How targets must relate.
        matching: SingleMatch,
    },
    /// A `company_ids` many2many.
    Many {
        /// The `company_ids` field.
        field: FieldId,
        /// How targets must relate.
        matching: ManyMatch,
    },
}

impl CompanyScope {
    /// The match the model asks of records linking to it (`None` for `res.company` itself).
    pub fn matching(self) -> Option<CompanyMatch> {
        match self {
            Self::Itself => None,
            Self::Single { matching, .. } => Some(matching.into()),
            Self::Many { matching, .. } => Some(matching.into()),
        }
    }

    /// The company field of the scope (`None` for `res.company` itself).
    pub fn field(self) -> Option<FieldId> {
        match self {
            Self::Itself => None,
            Self::Single { field, .. } | Self::Many { field, .. } => Some(field),
        }
    }
}

/// A relational field's company check, after defaults and opt-outs.
///
/// ```
/// use erp_registry::CheckCompany;
/// assert!(CheckCompany::Default.is_on() && CheckCompany::Declared.is_on());
/// assert!(!CheckCompany::Off.is_on());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CheckCompany {
    /// Not checked (declared `false`, or no rule of the default applies).
    Off,
    Declared,
    /// Checked because the registry's default says so.
    Default,
}

impl CheckCompany {
    /// True when the field is checked.
    pub fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Whose rights a computed or related field is read with, and whether the companies of the
/// records it passes bound it (review T6: one value, so the superuser read and
/// the company bound cannot disagree).
///
/// ```
/// use erp_registry::ComputeRead;
/// assert!(ComputeRead::Superuser.is_superuser() && !ComputeRead::Superuser.is_company_bound());
/// assert!(ComputeRead::CompanyBound.is_superuser() && ComputeRead::CompanyBound.is_company_bound());
/// assert!(!ComputeRead::AsCaller.is_superuser());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComputeRead {
    /// Read with the caller's rights (`compute_sudo = false`; the default of a stored or plain
    /// computed field).
    AsCaller,
    Superuser,
    /// A non-stored related field whose declaration left `compute_sudo` unset: read as superuser,
    /// but every record reached through a company-scoped hop must belong to the reader's active
    /// companies, or to none.
    CompanyBound,
}

impl ComputeRead {
    /// True when the value is computed as superuser (`Superuser`, `CompanyBound`).
    pub const fn is_superuser(self) -> bool {
        matches!(self, Self::Superuser | Self::CompanyBound)
    }

    /// True for the company-bound read.
    pub const fn is_company_bound(self) -> bool {
        matches!(self, Self::CompanyBound)
    }
}

/// A field after merging all its declarations (FIELD §4.2).
#[derive(Debug, Clone)]
pub struct FieldSpec {
    /// Position in the model.
    pub id: FieldId,
    /// Name.
    pub name: &'static str,
    /// Pre-quoted identifier: `"name"`.
    pub(crate) quoted_name: std::sync::Arc<str>,
    /// Type with parameters.
    pub ty: FieldType,
    /// Label.
    pub string: String,
    /// Tooltip.
    pub help: Option<&'static str>,
    /// NOT NULL in the database.
    pub required: bool,
    /// UI read-only.
    pub readonly: bool,
    /// Persisted.
    pub store: bool,
    /// Copied by `copy()`.
    pub copy: bool,
    /// Requested index.
    pub index: Option<IndexKind>,
    /// Resolved default.
    pub default: Option<FieldDefault>,
    /// In the model's default prefetch group.
    pub prefetch: bool,
    /// Compute method.
    pub compute: Option<&'static str>,
    /// Search method for nonstored searchable fields.
    pub search_method: Option<&'static str>,
    /// Inverse method.
    pub inverse: Option<&'static str>,
    /// Related path.
    pub related: Option<&'static str>,
    /// `precompute` as declared (stored computed and stored related fields only): `None` lets the
    /// ORM precompute when it can, `Some(false)` opts out, `Some(true)` asserts that
    /// it can, an error at ORM assembly otherwise, and protects the field for the rest of the
    /// create (D7).
    pub precompute: Option<bool>,
    /// Whose rights the computed or related value is read with (`compute_sudo` and the company
    /// bound as one value). Read it through [`FieldSpec::compute_sudo`] and
    /// [`FieldSpec::company_bound`]; the enum can grow (review TD1, TD2).
    pub(crate) compute_read: ComputeRead,
    /// Group restriction.
    pub groups: Option<&'static str>,
    /// Explicit dependencies (`depends=`), replacing the compute methods' ones.
    pub depends: Option<&'static [&'static str]>,
    /// Fields read by the compute without a trigger.
    pub reads: &'static [&'static str],
    /// Context keys of a context-dependent computed field.
    pub depends_context: &'static [&'static str],
    /// Addons that declared the field, in override order (`_modules`).
    pub modules: Vec<&'static str>,
    /// One of the four audit fields added by log access.
    pub magic: bool,
    /// A custom field declared at runtime (`ir.model.fields` with `state = 'manual'`); manual
    /// declarations are not listed in [`FieldSpec::modules`].
    pub manual: bool,
    /// Relational `domain=` (JSON list syntax), parsed by the ORM.
    pub domain: Option<&'static str>,
    /// Relational `context=` (JSON object), parsed by the ORM.
    pub context: Option<&'static str>,
    /// `check_company` after defaults and opt-outs (REL §4.12); resolved by the
    /// registry's company pass.
    pub check_company: CheckCompany,
    /// What the declarations said about `check_company`: `None` when no declaration set it. The
    /// company pass reads it to tell an opt-out from "not declared".
    pub(crate) check_company_declared: Option<bool>,
    /// `bypass_search_access` (REL §4.2; forced on delegate fields).
    pub bypass_search_access: bool,
    /// The many2one of an `_inherits` entry (REL §4.4.7).
    pub delegate: bool,
    /// `company_dependent`.
    pub company_dependent: bool,
    /// `translate`: translatable textual field stored as JSONB with language keys.
    pub translate: bool,
    /// Resolved HTML policy, present exactly for Html fields. Raw text is never markup-trusted.
    pub html_policy: Option<erp_fields::html::FieldPolicy>,
    /// `change_default`.
    pub change_default: bool,
    /// `user_writeable`: a user may write this field of their own `res.users` record
    /// .
    pub user_writeable: bool,
    /// `secret`: the field holds a credential; the audit call digest redacts its value
    /// (redaction v2).
    pub secret: bool,
    /// Only stored fields of the model's own
    /// table and stored many2many fields are tracked.
    pub tracking: Option<u32>,
    /// A field of an `_inherits` parent exposed on this model (MODEL-020).
    pub inherited: Option<Inherited>,
    /// Filled by the ORM at insert from the transaction timestamp, like `create_date`; a value
    /// from a caller is refused. Always readonly and not copied.
    pub server_set: bool,
    /// Merged selection `ondelete` policies by selection value (/035`).
    pub selection_ondelete: rustc_hash::FxHashMap<String, erp_fields::SelectionOnDelete>,
    /// Generation metadata: physical materialization, source expression, dependencies, and AST.
    pub(crate) generated: Option<crate::generated::GeneratedSpec>,
}

/// Where an inherited field (`_inherits`) reads and writes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inherited {
    /// The delegate many2one of this model.
    pub delegate: FieldId,
    /// The parent model.
    pub parent: ModelId,
    /// The field on the parent (itself possibly inherited, for several levels).
    pub parent_field: FieldId,
}

/// A field's default after merging (a removed default is simply absent).
#[derive(Debug, Clone, Copy)]
pub enum FieldDefault {
    /// A static value.
    Value(StaticValue),
    /// A function of the environment.
    Fn(DefaultFn),
}

impl FieldSpec {
    /// Computed as superuser (`compute_sudo`; also true for a company-bound related field).
    pub const fn compute_sudo(&self) -> bool {
        self.compute_read.is_superuser()
    }

    /// A non-stored related field read company-bound: its declaration left
    /// `compute_sudo` unset, so it is read as superuser but only through hop records of the
    /// reader's companies. The registry only sets [`ComputeRead::CompanyBound`] on such fields
    /// (an `_inherits` copy is reset to the caller's read, which has no related path); the test
    /// here repeats it so no caller has to (review RD7).
    pub const fn company_bound(&self) -> bool {
        self.compute_read.is_company_bound() && self.related.is_some() && !self.store
    }

    /// The physical cell representation of this field's values. A translated field is stored as a
    /// jsonb language map, so it is [`Storage::Json`] whatever its type says.
    pub fn storage(&self) -> Storage {
        if self.translate {
            Storage::Json
        } else {
            self.ty.storage()
        }
    }

    /// The database column: stored fields whose type has one (`id` is created with the table).
    pub fn column(&self) -> Option<ColumnType> {
        if self.store {
            if self.company_dependent || self.translate {
                Some(ColumnType::Jsonb)
            } else {
                self.ty.column_type()
            }
        } else {
            None
        }
    }

    /// True when the field has its own column.
    pub fn has_column(&self) -> bool {
        self.column().is_some()
    }

    /// Computed or related (its value comes from a compute method or a path).
    pub fn is_computed(&self) -> bool {
        self.compute.is_some() || self.related.is_some()
    }

    /// True when the field has a search method.
    pub fn has_search_method(&self) -> bool {
        self.search_method.is_some()
    }

    /// True when the field is database-generated.
    pub fn is_generated(&self) -> bool {
        self.generated.is_some()
    }

    /// Access the field's generated column specification, if generated.
    pub fn generated(&self) -> Option<&crate::generated::GeneratedSpec> {
        self.generated.as_ref()
    }

    /// True when the field is a VIRTUAL generated column.
    pub fn is_virtual(&self) -> bool {
        self.generated.as_ref().is_some_and(|g| g.is_virtual())
    }

    /// True when the field is a STORED generated column.
    pub fn is_stored_generated(&self) -> bool {
        self.generated.as_ref().is_some_and(|g| g.is_stored())
    }

    /// True when the field can be assigned to by callers or ORM write operations.
    pub fn is_assignment_eligible(&self) -> bool {
        !self.is_generated() && !self.server_set && self.ty != FieldType::Id
    }

    /// The pre-quoted column name: `"name"`.
    pub fn quoted_name(&self) -> &str {
        &self.quoted_name
    }
}

/// A table object with its full database name.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TableObject {
    /// The declared name (`_positive_color`), as messages name the object; the newest
    /// declaration's when several share the full name.
    pub name: &'static str,
    /// `<table>_<name>` shortened to 63 bytes.
    pub full_name: String,
    /// Declared kind and definition.
    pub kind: TableObjectKind,
    /// The newest declaration came from a model file or overlay (runtime data), not from Rust
    /// code; schema sync never replaces an object it did not create on a runtime declaration's
    /// behalf.
    pub runtime: bool,
}

/// The Postgres schema name of the closed event namespace (A2-D5).
pub const EVENT_SCHEMA: &str = "erp_event";

/// Where a model's table lives. `Event` is the one closed schema of the outbox/audit tables (ADR
/// 0039 A2-D5): only the core declaration path ([`crate::RegistryBuilder::with_core_event_decls`])
/// selects it, and such a model is a closed leaf (no extension, `_inherit`, `_inherits` or relation to
/// or from it, see the registry build).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TableNamespace {
    /// The connection's current schema (every ordinary model).
    #[default]
    Current,
    /// The dedicated event schema ([`EVENT_SCHEMA`]).
    Event,
}

/// One model of a registry.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Position in the registry.
    pub id: ModelId,
    /// `_name`.
    pub name: &'static str,
    /// `_description`.
    pub description: String,
    /// `_table`.
    pub table: String,
    /// Pre-quoted table name: `"table"`.
    pub(crate) quoted_table: std::sync::Arc<str>,
    /// Where the table lives; only the core declaration path sets [`TableNamespace::Event`].
    pub namespace: TableNamespace,
    /// The table as SQL names it: `"table"`, or `"erp_event"."table"` in the event namespace (each
    /// identifier quoted on its own).
    pub(crate) sql_table: std::sync::Arc<str>,
    /// Regular, abstract or transient.
    pub kind: ModelKind,
    /// A custom model created at runtime (`ir.model` manual, `_custom`).
    pub custom: bool,
    /// `_log_access`.
    pub log_access: bool,
    /// The validated cleanup policy of a transient model (defaults applied, extensions
    /// composed); `None` for a regular or abstract model. Metadata only: no cleanup is active.
    pub transient_policy: Option<crate::transient_policy::TransientPolicy>,
    /// `_order`.
    pub order: &'static str,
    /// `_rec_name`.
    pub rec_name: Option<FieldId>,
    pub rec_names_search: Option<Vec<&'static str>>,
    /// `_active_name`.
    pub active_name: Option<FieldId>,
    /// Parent models (`_inherit`, without itself and `base`).
    pub inherit: Vec<&'static str>,
    /// Declarations in method resolution order (newest first).
    pub mro: Vec<&'static ModelDecl>,
    /// Fields in declaration order.
    pub fields: Vec<FieldSpec>,
    /// Field index by name.
    pub(crate) by_name: FxHashMap<&'static str, FieldId>,
    /// SQL constraints and indexes.
    pub table_objects: Vec<TableObject>,
    /// Fields loaded together on a cache miss (stored columns with `prefetch=True`).
    pub prefetch_fields: Vec<FieldId>,
    /// `_inherits`: `(parent model, delegate many2one)` in declaration order.
    pub inherits: Vec<(ModelId, FieldId)>,
    /// `_check_company_auto` after defaults and opt-outs.
    pub check_company_auto: bool,
    /// How the model's records belong to companies; `None` when the model is not
    /// company-scoped. Resolved by the registry's company pass.
    pub company_scope: Option<CompanyScope>,
    /// The validated company access (A1-D2, A1-D3), set by the registry's company access
    /// pass. A typed state, not an `Option`: a concrete company-scoped model that declares
    /// nothing is [`CompanyAccessState::Undeclared`] during composition and is refused before a
    /// registry is returned, never read as unrestricted (design `m3-company-a1.md` §5).
    pub company_access: crate::company_access::CompanyAccessState,
    /// `_allow_sudo_commands` (REL-060): when `false`, x2many commands targeting this model run
    /// as the transaction's original principal.
    pub allow_sudo_commands: bool,
    /// `_check_inherits_access`: the `_inherits` parents' access applies to the model's records
    /// (true unless a declaration says otherwise; the newest declaration that says wins).
    pub inherits_access: bool,
    /// `_parent_name`, when that field exists (hierarchy operators).
    pub parent_name: Option<FieldId>,
    /// `_parent_store`: the `parent_path` field the ORM maintains (CRUD §4.11).
    pub parent_path: Option<FieldId>,
    /// Table fill factor in percent, when the model sets one; `None` leaves the
    /// table's storage parameter as it is.
    pub fillfactor: Option<u8>,
    /// The named id sequence the table's `id` draws from, shared with the other models
    /// that name it; `None` for the table's own identity.
    pub id_sequence: Option<&'static str>,
    /// Leaf partitioning declared by the model's owner (
    /// §7): the table is partitioned by range of the key and no field references the model.
    pub partition: Option<Partition>,
    // ---- (D3) ----
    /// The `row_version` field, when a declaration set `row_version = true`.
    pub row_version: Option<FieldId>,
    pub tracked: Vec<FieldId>,
    /// Whether any loaded declaration opted this regular model into transactional event capture
    /// . Extensions compose by OR; no extension can disable another's opt-in.
    pub eventful: bool,
    /// Business keys (the checked [`TableObjectKind::Key`] table objects), see
    /// [`ModelSpec::business_key`].
    pub(crate) keys: Vec<crate::keys::BusinessKey>,
}

impl ModelSpec {
    /// The pre-quoted table name: `"table"`.
    pub fn quoted_table(&self) -> &str {
        &self.quoted_table
    }

    /// The table name SQL must use: [`ModelSpec::quoted_table`] in the current schema, the
    /// schema-qualified name in the event namespace. Used only by schema admission, partition upkeep
    /// and the event capture insert; other `.table` users are not migrated.
    pub fn sql_table(&self) -> &str {
        &self.sql_table
    }

    /// True when generic schema sync, uninstall and vacuum own the model's table: it has a table
    /// and that table is in the current schema. False for an abstract model and for a model of the
    /// closed event namespace, whose table is created and checked by the event schema install
    /// (A2-D5) and never by a generic loop.
    pub fn has_generic_table(&self) -> bool {
        self.has_table() && self.namespace == TableNamespace::Current
    }

    /// True when the model has a table (`_auto`).
    pub fn has_table(&self) -> bool {
        self.kind != ModelKind::Abstract
    }

    /// True for abstract models.
    pub fn is_abstract(&self) -> bool {
        self.kind == ModelKind::Abstract
    }

    /// A field by name.
    pub fn field(&self, name: &str) -> Option<&FieldSpec> {
        self.by_name.get(name).map(|id| &self.fields[id.index()])
    }

    /// A field id by name.
    pub fn field_id(&self, name: &str) -> Option<FieldId> {
        self.by_name.get(name).copied()
    }

    /// A field by id.
    pub fn field_by_id(&self, id: FieldId) -> &FieldSpec {
        &self.fields[id.index()]
    }

    /// Stored fields with a column, in declaration order.
    pub fn column_fields(&self) -> impl Iterator<Item = &FieldSpec> {
        self.fields.iter().filter(|f| f.has_column())
    }

    /// True when this model is `model` or inherits it through `_inherit` (including `base`).
    pub fn inherits(&self, model: &str) -> bool {
        self.name == model || self.mro.iter().any(|d| d.name == model)
    }

    /// The table object named `full_name`, if declared.
    pub fn table_object(&self, full_name: &str) -> Option<&TableObject> {
        self.table_objects.iter().find(|o| o.full_name == full_name)
    }

    /// Whether this model stays installed when `removing_addons` are removed: at least one declaration
    /// of an addon outside `removing_addons` remains to define it. Declarations of core framework addons
    /// (`base`, `web`, `base_data`) only keep a model alive if they are its root definition.
    pub fn stays_without_addons(&self, removing_addons: &[&str]) -> bool {
        let always_present = |addon: &str| {
            addon == crate::BASE_ADDON.name
                || addon == crate::WEB_ADDON.name
                || addon == "base_data"
        };
        self.mro.iter().any(|decl| {
            !removing_addons.contains(&decl.addon)
                && (!always_present(decl.addon)
                    || (decl.name == self.name && !decl.inherit.contains(&decl.name)))
        })
    }

    /// Stored selection additions that `addons` contributed to fields of this model.
    /// Returns `(field_name, added_values)`.
    pub fn selection_additions_from_addons<'a>(
        &'a self,
        addons: &'a [&str],
    ) -> impl Iterator<Item = (&'a str, Vec<&'a str>)> {
        self.mro
            .iter()
            .filter(move |decl| addons.contains(&decl.addon))
            .flat_map(move |decl| {
                decl.fields.iter().filter_map(move |field| {
                    let crate::KindDecl::Selection { add, .. } = &field.kind else {
                        return None;
                    };
                    let stored = self.field(field.name).is_some_and(|f| f.store);
                    if !stored || add.is_empty() {
                        return None;
                    }
                    let values: Vec<&str> = add.iter().map(|(value, _)| *value).collect();
                    Some((field.name, values))
                })
            })
    }

    /// Whether a follow-delete trigger on the model's own table deletes the rows of `key_table`
    /// with the deleted ids ([`crate::FollowDeleteDecl::deleting_keys`]).
    pub fn follows_deletes_into(&self, key_table: &str) -> bool {
        self.table_objects.iter().any(|o| {
            matches!(
                o.kind,
                crate::TableObjectKind::FollowDelete(d)
                    if d.key_table == Some(key_table) && d.table == TriggerTable::Model
            )
        })
    }

    /// The table a signal trigger of this model is attached to: the model's table, or the
    /// relation table of the stored many2many field it names.
    ///
    /// # Errors
    ///
    /// `Programming` when the field is missing, not a stored many2many, or an `_inherits`
    /// copy (its relation table belongs to the parent model, which declares the trigger).
    pub fn signal_trigger_table(&self, decl: &SignalTriggerDecl) -> Result<&str> {
        self.trigger_table(decl.table, "signal trigger")
    }

    /// The table a follow-delete trigger of this model is attached to
    /// ([`crate::FollowDeleteDecl`]), as [`ModelSpec::signal_trigger_table`].
    ///
    /// # Errors
    ///
    /// As [`ModelSpec::signal_trigger_table`].
    pub fn follow_delete_table(&self, decl: &crate::FollowDeleteDecl) -> Result<&str> {
        self.trigger_table(decl.table, "follow-delete trigger")
    }

    fn trigger_table(&self, table: TriggerTable, what: &str) -> Result<&str> {
        let field = match table {
            TriggerTable::Model => return Ok(&self.table),
            TriggerTable::Relation(field) => field,
        };
        let refused = |why: &str| {
            Error::programming(format!(
                "{what} on the relation of {}.{field}: {why}",
                self.name
            ))
        };
        let spec = self
            .field(field)
            .ok_or_else(|| refused("the model has no such field"))?;
        if spec.inherited.is_some() {
            return Err(refused(
                "the field is inherited through _inherits; declare the trigger on the model that \
                 owns it",
            ));
        }
        match &spec.ty {
            FieldType::Many2many {
                relation: Some(relation),
                ..
            } => Ok(&relation.table),
            FieldType::Many2many { relation: None, .. } => {
                Err(refused("the many2many is not stored (no relation table)"))
            }
            other => Err(refused(&format!(
                "the field is a {}, not a many2many",
                other.type_name()
            ))),
        }
    }
}

/// The composed metadata of one database: immutable, shared by `Arc`.
#[derive(Debug)]
pub struct Registry {
    pub(crate) generation: u64,
    pub(crate) addons: Vec<&'static AddonDescriptor>,
    pub(crate) models: Vec<ModelSpec>,
    pub(crate) by_name: FxHashMap<&'static str, ModelId>,
    pub(crate) dropped: Vec<DroppedField>,
    pub(crate) statement_cache: crate::statement_cache::StatementCache,
}

impl Registry {
    /// Cached statement texts per model on this registry generation (PERF.SQL-TEXT).
    pub fn statement_cache(&self) -> &crate::statement_cache::StatementCache {
        &self.statement_cache
    }

    /// The statement cache slot of `spec`, which must be a model of this registry.
    ///
    /// # Panics
    ///
    /// When `spec` is not a model of this registry: a `ModelId` of another generation would
    /// otherwise select another model's slot and return another table's statement text. The
    /// check is one bounds-checked index and one pointer compare, in every build.
    pub fn statement_cache_of(
        &self,
        spec: &ModelSpec,
    ) -> &crate::statement_cache::ModelStatementCache {
        assert!(
            self.models
                .get(spec.id.index())
                .is_some_and(|own| std::ptr::eq(own, spec)),
            "ModelSpec `{}` does not belong to this registry (generation {})",
            spec.name,
            self.generation
        );
        self.statement_cache.model(spec.id)
    }
    /// Custom fields left out of this registry because they are broken (built with
    /// [`crate::ManualFieldPolicy::DropBroken`]), in the order of their overlay within each
    /// composition round; empty for a strict build. A health check reads them here: the build
    /// logs nothing.
    pub fn dropped_fields(&self) -> &[DroppedField] {
        &self.dropped
    }

    /// Registry generation (bumped by every rebuild of a database's registry).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Installed addons in load order.
    pub fn addons(&self) -> &[&'static AddonDescriptor] {
        &self.addons
    }

    /// All models, in creation order.
    pub fn models(&self) -> &[ModelSpec] {
        &self.models
    }

    /// A model by name.
    pub fn model(&self, name: &str) -> Option<&ModelSpec> {
        self.by_name.get(name).map(|id| &self.models[id.index()])
    }

    /// A model id by name.
    pub fn model_id(&self, name: &str) -> Option<ModelId> {
        self.by_name.get(name).copied()
    }

    /// A model by id.
    pub fn model_by_id(&self, id: ModelId) -> &ModelSpec {
        &self.models[id.index()]
    }

    /// True when the registry has a model of that name.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// The model owning table `table`, if any.
    pub fn model_by_table(&self, table: &str) -> Option<&ModelSpec> {
        self.models.iter().find(|m| m.table == table)
    }
}
