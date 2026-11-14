//! ERPForAll ORM runtime (layer L5).
//!
//! - [`Orm`] holds a database's registry, method dispatch tables, access policy and pool;
//!   [`Orm::begin`] opens a [`Transaction`].
//! - A [`Transaction`] is owned by one task and passed as `&mut` to every data operation.
//!   It owns the columnar record cache shared by all environments.
//! - [`Env`] and recordsets ([`Records`], [`RecordSet<M>`]) are plain values. An `Env` carries the
//!   [`Principal`] it acts.
//! - `create`, `write` and other model methods dispatch through override chains
//!   ([`dispatch`]); `base` implements `create` and `write`.
//! - [`Transaction::init_models`] creates and updates tables, partitioned ones included;
//!   [`Orm::ensure_partitions`], [`Orm::retire_partitions`] and [`Orm::maintain_partitions`] keep
//!   the partition window of leaf models.
//! - Searches ([`Records::search`], [`Records::search_with`], [`Records::search_count`],
//!   [`Records::name_search`], [`Records::fetch`]) optimize `erp-domain` domains to FULL, plan them
//!   to parameterized SQL whose text is memoized per shape, and filter access in SQL
//!   (–6).
//! - In-memory recordset operations ([`Records::filtered_domain`], [`Records::sorted`],
//!   [`Records::grouped`], `filtered*`, `mapped_path`) work column-wise on cached values;
//!   [`Records::exists`] and the row locks ([`Records::lock_for_update`],
//!   [`Records::try_lock_for_update`]) run one `= ANY($1)` statement per chunk of ids.
//!   `exists` and `filtered_domain` are overridable per model.
//!
//! Logging targets follow (`erp::orm`, `erp::sql`) once `tracing` is added.

#![allow(missing_docs)]

// `test-faults` adds an unchecked principal switch and commit-fault injection. It exists for the
// test suites only (debug builds). A release build that has it enabled, through feature
// unification or a dependency that turned it on, must not compile: `debug_assertions` is off in
// every shipped profile (`release`, `bench`).
#[cfg(all(feature = "test-faults", not(debug_assertions)))]
compile_error!(
    "the `test-faults` feature of erp-orm is for tests only and must not be enabled in a \
     release build: remove it from the dependency that enables it (erp-rpc/test-faults)"
);

mod access;
pub(crate) mod archive;
pub mod audit;
mod bitset;
mod build_object;
mod cache;
mod candidate;
mod commands;
mod company;
pub mod company_dependent;
mod company_dependent_runtime;
pub mod compute;
mod context;
mod correlation;
pub use correlation::{CorrelationId, OperationId, OperationIdentity};
pub(crate) mod copy;
mod core_ddl;
mod database_generated;
mod generated_input;
pub use generated_input::GeneratedInput;
mod dberror;
pub(crate) mod defaults;
/// Transactional deltas: the synchronous guarded apply.
pub mod delta;
mod describe;
mod selection;
pub use selection::{SelectionContext, SelectionFn, SelectionLimits, SelectionSuper};
pub mod dispatch;
mod engine;
mod env;
mod event_namespace;
mod event_schema;
pub use event_schema::{EVENT_PREMAKE_DAYS, EventSchemaProblem, EventSchemaReport};
mod event_upkeep;
pub use event_upkeep::{
    AUDIT_TABLE_NAME, EventPartitionWindow, EventTableUpkeep, EventUpkeepReport, EventUpkeepTick,
    OUTBOX_TABLE_NAME,
};
mod generation;
pub use generation::{
    GenerationAttachment, GenerationAttachmentFactory, GenerationCompose, GenerationComposition,
};
mod exclusion;
mod external_modified;
mod field_access;
pub mod field_chain;
mod field_check;
mod grantee;
mod graph;
mod group_check;
pub use group_check::GroupMembership;
mod hierarchy;
mod inherits;
pub mod ir_default;
pub mod jobs;
mod keyset;
pub(crate) mod lang;
pub mod model_hooks;
mod monetary;
pub(crate) mod name_create;
mod newrec;
pub mod node;
pub mod notice;
pub mod onchange;
mod optimize;
mod ormcache;
mod outbox;
mod parent_store;
mod partition;
pub mod pool;
mod precision;
mod predicate;
mod prepare;
mod principal;
pub mod query;
mod read;
pub mod read_group;
mod records;
/// Model reflection in PostgreSQL (`ir.model`, `ir.model.fields`, `ir.model.relation`).
pub mod reflection;
mod related_company;
mod relational;
mod relcrud;
mod route;
mod route_orm;
mod rows_build;
mod rows_stream;
pub mod rpc;
mod rset_ops;
mod schema;
mod schema_company;
pub mod schema_generated;
mod schema_index;
mod schema_sqlobj;
pub use schema_sqlobj::SqlObjectRemoval;
mod schema_sqlobj_roles;
pub use schema_sqlobj_roles::SqlRoleBindings;
mod schema_steps;
pub use schema_company::CompanyMigrationPolicy;
mod search;
pub mod search_method;
/// Sequence numbering (`ir.sequence`).
pub mod sequence;
pub mod signal;
mod store_snapshot;
mod timeouts;
pub mod trace;
mod tracking;
mod transaction;
pub(crate) mod translated;
mod trigger;
mod typed_rel;
mod unlink;
mod upsert;
mod vacuum;
mod vacuum_grant;
mod vals;
mod version;
pub mod web;
mod wire;
mod write;

pub use access::{
    ACCESS_DOMAIN_IMPLS, AccessCtx, AccessCtxParts, AccessDecision, AccessDomain,
    AccessDomainBuilder, AccessDomainChains, AccessDomainFn, AccessDomainImpl, AccessPolicy,
    AccessSuper, CompanyHint, CredentialBegin, CredentialCheck, CredentialVerdict, ElevationKind,
    EvidenceColumn, EvidenceCompany, EvidenceRow, ExplainDetail, ExplainRequest, ExplainTarget,
    Explanation, FieldIdx, FieldPath, FixedPolicy, GRANT_SOURCES, GrantDomain, GrantRef,
    GrantSource, GroupClause, GroupLookup, GroupRefResolution, GroupSetInterner, GroupSpec,
    GuardEvaluation, GuardIdx, GuardPlan, GuardPlanBuilder, GuardPlanLimits, InternedGroups,
    IntoPolicyProvider, MAX_EXPLAIN_IDS, Operation, POLICY_SOURCE_WRITES, PolicyGeneration,
    PolicyProvider, PolicySourceWrite, PrincipalResolver, RULE_OPERATORS, RecordEvidence,
    RecordGuard, RecordGuardIdx, ResolvedGuardCache, ResolvedGuardCacheConfig,
    ResolvedGuardCacheStats, ResolverStatement, RuleKind, RuleOperatorDecl, RuleOrigin, RuleRef,
    SnapshotOrigin, SnapshotRef, SourceKind, StaticPolicy,
};
pub use build_object::{
    BuildObjectOptions, BuildObjectReport, BuildOutcome, BuildTimeout, LockTimeout,
};
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub use build_object::{override_build_statement_timeout, override_diagnostic_timeouts};
pub use candidate::ReadTx;
pub use commands::Command;
pub use company::{COMPANY_CHECK_CHUNK, CompanyError, CompanyInconsistency};
pub use compute::{
    Assign, AsyncComputeSuper, Batch, BatchData, COMPUTE_IMPLS, CONSTRAINT_IMPLS,
    CandidateComputeFn, ComputeEntry, ComputeImpl, ComputeSuper, ConstraintImpl, Outputs,
    WasmComputeSpec, WasmConstraintSpec, wasm_compute_impl, wasm_constraint_impl,
};
pub use context::Context;
pub use core_ddl::{AfterModels, CORE_DDL, CoreDdl, CoreTableRequirement};
#[doc(hidden)]
pub use core_ddl::{
    CorePhase, pending as core_ddl_pending, refuse_deferred as core_ddl_refuse_deferred,
    run as core_ddl_run, run_deferred as core_ddl_run_deferred, validate as core_ddl_validate,
};
pub use dispatch::{
    BoxFuture, Chain, ChainData, Dispatch, DynSuper, ImplKind, METHOD_CODECS, MethodCodec,
    MethodCodecImpl, MethodDispatchTarget, MethodFn, MethodImpl, MethodInvoker, Super, TypedCodec,
    WasmMethodImpl, call_method, call_method_json,
};
#[doc(hidden)]
pub use dispatch::{MAX_WASM_IMPLS_PER_BUILD, wasm_method_name_is_interned};
pub use env::Env;
/// The messaging store wire crate, for addons: `erp_sdk::store_wire` (addons may not depend on
/// `erp-store-wire` themselves, `scripts/check-layers.py` rule 5).
pub use erp_store_wire as store_wire;
pub use erp_store_wire::{SnapshotDescriptor, Xid8};
pub use exclusion::ObjectKind;
pub use field_access::{FieldDenialDetail, FieldMask, FieldOp};
pub use field_chain::{
    FIELD_ACCESS_IMPLS, FieldAccessCtx, FieldAccessFn, FieldAccessImpl, FieldAccessSuper,
    FieldAccessTable, FieldContextValues,
};
pub use keyset::{Cursor, Page};
pub use notice::{MAX_NOTICE_MESSAGE_BYTES, MAX_NOTICES_PER_TRANSACTION, Notice, NoticeKind};
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub use onchange::count_onchange_visits_for_tests;
pub use onchange::{MAX_ONCHANGE_BATCH_ENTRIES, OnchangeResult, Warning as OnchangeWarning};
pub use ormcache::OrmCacheKey;
pub use partition::{
    Horizon, KeepAfter, MAX_PARTITIONS_PER_CALL, PARTITION_LOCK_TIMEOUT_BOUNDS,
    PARTITION_SHORT_LOCK_TIMEOUT, PartitionAction, PartitionEntry, PartitionReport, PartitionStep,
    PartitionWindow,
};
pub use principal::{
    ActiveCompanySet, Actor, ApiKeyId, BindingKey, BlockId, CAPABILITIES, Capability,
    CompanyContext, CompanyId, Elevation, ElevationKey, ElevationScope, FieldAllow, GrantBinding,
    GroupId, GroupSet, GroupSetId, IdKind, KeyKind, KindClass, LinkId, MAX_CAPABILITY_LEN,
    MAX_ELEVATION_ITEMS, MAX_ITEM_ENTRIES, MAX_KEY_NAME_LEN, MAX_SCOPE_ENTRIES, MAX_SCOPE_NAME_LEN,
    MAX_XML_ID_LEN, MethodAllow, ModelAllow, OperationSet, Principal, PrincipalBinding,
    PrincipalBuilder, PrincipalError, PrincipalHint, PrincipalKind, PrincipalShape, QualifiedXmlId,
    Resolution, ResolutionEpoch, SUPERUSER_ID, Scope, ScopeBuilder, ScopeItem, SnapshotId,
    TxSerial, UserId, missing_reset_target, registered_capability,
};
pub use read::{Load, read_value};
pub use read_group::{Granularity, GroupValue, Groups, ReadGroup, ReadGroupingSets, RecordsAs};
pub use records::{
    Base, Field, Ids, Inherits, Model, RecordSet, Records, ResCompany, ResUsers, Token,
};
pub use relational::CappedIds;
pub use route::{
    ReadPoolFailure, ReadPoolStatus, Route, RoutingStats, spawn_in_unit_of_work, unit_of_work,
};
pub use rows_stream::{RowsStream, StreamOptions};
pub use rset_ops::Mapped;
pub use schema::{
    SCHEMA_LOCK_TIMEOUT, SCHEMA_LOCK_TIMEOUT_DURATION, SCHEMA_STATEMENT_TIMEOUT,
    SCHEMA_STATEMENT_TIMEOUT_DURATION,
};
pub use schema_steps::{
    KeptColumnComment, MAX_STEP_FINGERPRINT_BYTES, MAX_STEP_KEY_BYTES, NoProgress, Replanned,
    Resume, SCHEMA_STEP_STATEMENT_TIMEOUT, SchemaStep, SchemaStepFailure, SchemaStepFailures,
    SchemaStepKey, SchemaStepKind, SchemaStepReport, SchemaSync, SchemaSyncOptions, StepCursor,
    StepFingerprint, StepProgress,
};
pub use search::{Fetch, MemoStats, NAME_SEARCH_LIMIT, NameSearch, SearchOptions, SearchQuery};
pub use search_method::{
    FIELD_SEARCH_IMPLS, FieldSearchChains, FieldSearchContext, FieldSearchFn, FieldSearchImpl,
    FieldSearchRequest, FieldSearchSuper, SearchBudget, SearchLimits,
};
pub use sequence::{
    FiscalYear, PeriodKey, SequencePattern, SequencePatternBuilder, SequenceReset,
    SequenceWaitMode, YearDigits, render_sequence_format, resolve_period_key,
};
pub use signal::{
    CacheName, Changes, Generations, ListenerHandle, ListenerOptions, NotifyChannel, Refresh,
    Refreshed, SelfApplied, Signal, SignalKey, SignalStats,
};
pub use timeouts::Timeouts;
pub use tokio_postgres::Row;
pub use tracking::{
    PrecommitHook, PrecommitHookFn, TRACKING_DISABLE_KEYS, TrackedChange, TrackedRecord,
};
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub use transaction::CommitFault;
pub use transaction::batches::ResponseBatches;
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub use transaction::raw_row::{IdentityMode, IdentityReport};
pub use transaction::raw_row::{SqlRow, SqlValue};
pub use transaction::{
    BaseInstallGrant, BaseInstallGuard, BeginOptions, CommitError, CredentialOptions,
    FinalStatement, HookScope, Isolation, OperationControl, OperationKey, Orm, OrmBuilder,
    OrmConfig, PureMethods, RawStatements, ReadAuditOwner, RetryPolicy, RunFailure, RunOptions,
    SavepointOutcome, Transaction, TxIntent, WriteOp, require_top_level_unit,
};
pub use typed_rel::{Cmd, Many2one, TypedRelation, X2many, X2manyValue};
pub use upsert::{
    Accumulable, FieldOf, KeyRef, Upsert, UpsertCall, UpsertOutcome, UpsertResult, Upserted,
    UpsertedRow,
};
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub use vacuum::vacuum_batch_retries;
pub use vacuum::{VacuumOptions, VacuumOutcome, VacuumReport, transient_policy_fingerprint};
pub use vals::{RawVals, Vals};
pub use version::{
    SelectionCallback, SelectionCallbackFn, UnlinkHook, UnlinkHookFn, WriteBatch, WriteHook,
    WriteHookFn, WrittenRow,
};
pub use web::{
    FormattedGroups, FormattedReadGroup, FormattedReadGroupingSets, MAX_RESEQUENCE_RECORDS,
    MAX_SAVE_MULTI_RECORDS, MAX_SPEC_DEPTH, MAX_SPEC_FIELDS, ProgressBar,
    SEARCH_PANEL_ERROR_MESSAGE, SearchPanelSelectMultiRange, SearchPanelSelectRange, Spec,
    SpecItem, WebNameSearch, WebReadGroup, WebSave, WebSearchRead,
};

/// The dynamic values map of `default_get`, `onchange` and the web methods.
pub use erp_fields::ValueMap;

/// Re-exported for macro output (`erp-sdk` re-exports it again).
#[doc(hidden)]
pub use linkme;

/// Bench entry points exposed for micro-benchmarks.
#[doc(hidden)]
pub mod __bench {
    pub use crate::defaults::add_missing_defaults_batch;
}

#[doc(hidden)]
pub use wire::wasm_abi1_value_to_json;

mod wasm_abi1;
#[doc(hidden)]
pub use wasm_abi1::{is_wasm_abi1_native_refusal, wasm_abi1_check_call, wasm_abi1_check_fields};

// Reserved native bridge: base must admit caller fields before mutating lifecycle fences.
#[doc(hidden)]
pub use field_check::base_check_write_fields;

/// HTML field admission and typed operation reports.
pub mod html;
