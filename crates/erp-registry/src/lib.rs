//! ERPForAll model metadata (layer L2; pure, builds for `wasm32-wasip2`).
//!
//! Addons contribute [`ModelDecl`]s (natively through the [`MODEL_DECLS`] link-time slice);
//! [`RegistryBuilder`] composes the declarations of an addon set into an immutable [`Registry`]:
//! models with their method resolution order, merged fields and table objects (
//! ). Runtime declarations (`models/*.toml`, custom `x_` models and fields) are
//! [`ModelDef`]s, applied per database through an [`Overlay`]; [`RegistryCache`] shares one
//! registry between databases with the same addon set and overlay (§11).

mod company;
mod company_access;
mod compose;
mod decl;
mod drop;
mod event;
mod exclusion;
pub mod generated;
pub mod ident;
mod keys;
mod order;
mod overlay;
mod owned;
mod owner_fields;
mod partition;
mod relations;
mod rpc;
pub mod settings;
mod share;
mod spec;
mod sqlobj;
pub mod statement_cache;
pub mod tool_schema;
mod transient_policy;
mod typed_index;

pub use company_access::{
    CompanyAccess, CompanyAccessDecl, CompanyAccessDef, CompanyAccessError, CompanyAccessState,
    CompanyBranch, CompanyCardinality, DeclSource, DirectDecl, DirectRestriction, EdgeStorage,
    EmptyCompany, EscapeGap, MAX_OWNER_DEPTH, MAX_UNRESTRICTED_REASON_BYTES, NotApplicable,
    OwnerGap, OwnerRestriction, ParentDecl, Restriction, UnrestrictedDecl,
};
pub use compose::{
    BASE_ADDON, BASE_DECL, MAGIC_FIELD_NAMES, MAX_EVENT_FIELD_NAMES, MAX_TRACKED_FIELDS,
    RegistryBuilder, USERS_DECL, WEB_ADDON, WEB_DECL, static_default,
};
pub use decl::KeyDecl;
pub use decl::{
    AddonDescriptor, DEFAULT_TRACKING_SEQUENCE, DefaultContext, DefaultDecl, DefaultFn, FieldDecl,
    FollowDeleteDecl, KindDecl, MAX_FOLLOW_CACHE_LEN, ModelDecl, ModelKind, SelectionOnDeleteDecl,
    SignalTriggerDecl, StaticValue, TableObjectDecl, TableObjectKind, TriggerSignal, TriggerTable,
};
pub use drop::{DropReason, DroppedField, ManualFieldPolicy};
pub use exclusion::{
    ExclusionDecl, MAX_CONSTRAINT_COLUMNS, PgExtension, TemporalKeyDecl,
    describe as describe_object, period_check_name,
};
pub use generated::{
    GeneratedExpr, GeneratedFunc, GeneratedLiteral, GeneratedSpec, GeneratedStorage,
    MAX_GENERATED_EXPR_BYTES, MAX_GENERATED_EXPR_DEPTH, MAX_GENERATED_EXPR_NODES,
    MAX_GENERATED_SQL_BYTES,
};
pub use keys::{BusinessKey, ROW_VERSION};
pub use order::load_order;
pub use overlay::{
    MANUAL_ADDON, MAX_ADDON_DEFS, MAX_MANUAL_FIELDS, MAX_MANUAL_MODELS, ManualModel, Overlay,
};
pub use owned::{
    DATABASE_INTERN_QUOTA_BYTES, DefaultValue, ExclusionDef, FieldDef, INTERN_BUDGET_BYTES,
    InternAccount, KindDef, M2mTable, MAX_FIELDS_PER_MODEL, MAX_NAME_BYTES, MAX_PARENTS,
    MAX_SELECTION_ENTRIES, MAX_TABLE_OBJECTS_PER_MODEL, MAX_TEXT_BYTES, ModelDef, TemporalKeyDef,
    intern_addon, intern_addon_with_policy, intern_budget, intern_budget_for, interned_bytes,
    set_intern_budget,
};
pub use partition::{
    MAX_PARTITION_INTERVALS, MAX_PARTITIONED_TABLE_LEN, PARTITION_ADR, POLICY_SOURCE_MODELS,
    Partition, PartitionDecl, PartitionInterval, PartitionRetain, partition_name,
};
pub use rpc::{ArgShape, Effect, ParamSpec, Receiver};
pub use share::{Databases, RegistryCache};
pub use spec::{
    CheckCompany, CompanyDelegation, CompanyMatch, CompanyScope, ComputeRead, EVENT_SCHEMA,
    FieldDefault, FieldId, FieldSpec, Inherited, ManyMatch, ModelId, ModelSpec, Registry,
    SingleMatch, TableNamespace, TableObject,
};
pub use sqlobj::SqlObjectLimits;
pub use sqlobj::decl::{
    SqlBodyRender, SqlColumnSet, SqlCompareColumn, SqlConditionDecl, SqlDeferral, SqlDep, SqlEvent,
    SqlFunctionDecl, SqlLanguage, SqlLevel, SqlObjectDecl, SqlObjectManifest, SqlResult, SqlScalar,
    SqlSecurity, SqlTiming, SqlTriggerDecl, SqlType, SqlVolatility,
};
pub use sqlobj::graph::{
    SqlCondition, SqlFunction, SqlModelRef, SqlObject, SqlObjectGraph, SqlObjectId, SqlObjectKind,
    SqlReference, SqlTransition, SqlTrigger, SqlTriggerShape,
};
pub use statement_cache::{ModelStatementCache, SelectShape, StatementCache};
pub use transient_policy::{
    ATTR_MAX_AGE_SECONDS as TRANSIENT_MAX_AGE_ATTR, ATTR_MAX_COUNT as TRANSIENT_MAX_COUNT_ATTR,
    DEFAULT_TRANSIENT_MAX_AGE_SECONDS, MAX_TRANSIENT_MAX_AGE_SECONDS, MAX_TRANSIENT_MAX_COUNT,
    TransientPolicy,
};
pub use typed_index::{
    IndexDecl, IndexDef, IndexMethod, MAX_INDEX_COLUMNS, MAX_INDEXES_PER_MODEL, index_kind_mismatch,
};

/// Model declarations linked into the binary by `#[model]`/`#[extend]` (native builds only).
#[cfg(not(target_family = "wasm"))]
#[linkme::distributed_slice]
pub static MODEL_DECLS: [ModelDecl];

#[cfg(not(target_family = "wasm"))]
fn linked_decls() -> impl Iterator<Item = &'static ModelDecl> {
    MODEL_DECLS.iter()
}

#[cfg(target_family = "wasm")]
fn linked_decls() -> impl Iterator<Item = &'static ModelDecl> {
    std::iter::empty()
}

/// SQL object declarations linked into the binary by native addons (native builds only).
#[cfg(not(target_family = "wasm"))]
#[linkme::distributed_slice]
pub static SQL_OBJECT_DECLS: [SqlObjectDecl];

#[cfg(not(target_family = "wasm"))]
fn linked_sql_objects() -> &'static [SqlObjectDecl] {
    &SQL_OBJECT_DECLS
}

#[cfg(target_family = "wasm")]
fn linked_sql_objects() -> &'static [SqlObjectDecl] {
    &[]
}

/// Re-exported for macro output.
#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub use linkme;
