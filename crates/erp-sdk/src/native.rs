//! The native backend of the addon API: the ORM, registry and database crates behind one surface.
//! Selected for every target except `wasm32-*` (`cfg(not(target_family = "wasm"))`); see `lib.rs`.

/// Trusted native SQL-object declarations and their linked registration collector.
/// Addons use this facade instead of depending on framework crates. The registry still requires
/// an independently supplied manifest; these declarations do not authorize request-time DDL.
/// This module is absent from the WebAssembly guest backend.
///
/// ```
/// use erp_sdk::sql_objects::{SqlConditionDecl, SqlConditionKind, SqlObjectDecl};
/// #[erp_sdk::sql_objects::linkme::distributed_slice(erp_sdk::sql_objects::SQL_OBJECT_DECLS)]
/// #[linkme(crate = erp_sdk::sql_objects::linkme)]
/// static GUARD: SqlObjectDecl = SqlObjectDecl::condition("inventory", "immutable",
///     SqlConditionDecl::new("ERP01", "inventory_immutable", "This value cannot be changed.")
///         .kind(SqlConditionKind::Validation));
/// ```
pub mod sql_objects {
    pub use erp_registry::{
        SQL_OBJECT_DECLS, SqlBinding, SqlBodyRender, SqlColumnSet, SqlCompareColumn,
        SqlConditionDecl, SqlConditionKind, SqlDeferral, SqlDep, SqlEvent, SqlFunctionDecl,
        SqlLanguage, SqlLevel, SqlObjectDecl, SqlObjectManifest, SqlResult, SqlScalar, SqlSecurity,
        SqlTableGrantDecl, SqlTableOp, SqlTiming, SqlTriggerDecl, SqlType, SqlVolatility,
    };
    pub use linkme;
}

/// Count-only native uninstall guard declarations.
/// The host's ModuleSystem still requires an independent manifest and orchestrates authority,
/// locks and rollback. The callback receives no SQL/Env/transaction escape. This collector is
/// shared by every native host; it is absent from the WebAssembly guest backend.
pub mod sql_uninstall {
    pub use erp_orm::sql_uninstall::{
        SQL_UNINSTALL_GUARDS, SqlGuardFn, SqlGuardRead, SqlUninstallGuard,
        SqlUninstallGuardManifest,
    };
    pub use linkme;
}

/// Typed native settings declarations; lifecycle composition binds addon ownership.
pub mod settings;

/// Client action tags declared by native addons: the server serves an
/// `ir.actions.client` only for a tag an installed addon declares.
pub mod client_actions;

/// Declared technical system parameters: registration ([`crate::system_parameters!`]) and typed
/// reads limited to the caller's dependency closure.
pub mod params;

/// The durable job queue: declare job kinds, enqueue jobs inside the business
/// transaction, write handlers. The `System` principal is not
/// part of this surface: a kind runs as the principal that enqueued it, and the registry refuses
/// a kind that asks to run as the system unless it belongs to the core or to `base`.
pub mod jobs {
    pub use erp_orm::jobs::{
        Backoff, CancelToken, ChunkReport, ChunkResult, EnqueueOutcome, EnqueueRequest, HandlerRef,
        HistoryPolicy, ItemKey, ItemsChunk, JobContext, JobError, JobHandler, JobKey, JobKindDecl,
        JobKindRequirement, JobRef, JobSettings, JobSettingsCompatibility, JobSettingsSnapshot,
        JobSettingsSource, KindIsolation, MAX_CANCEL_KEYS, OnDuplicate, PrincipalPolicy, Priority,
        RecurringSpec, RunAs, RunOutcome, Setting, StepArgs, StepOutcome, TimeoutClass, When,
    };
    pub use erp_orm::register_job_kind;
}

/// Transactional deltas: projection shapes, exact netting and the synchronous
/// guarded apply., "Synchronous guarded apply".
pub mod delta {
    pub use erp_orm::delta::{
        AppliedRow, CanonicalKey, DeltaLimits, GrainColumn, GrainType, GrainValue, InsertFill,
        MeasureColumn, MeasureGuard, MeasureType, MeasureValue, MeasureVector, NettedBatch,
        ProjectionShape, RESERVED_COLUMNS, SyncApplied, SyncApply, SyncApplyFailure, Uuid16,
        apply_synchronous,
    };
}

/// The attachment blob store: the `AttachmentStore` trait, the stores, the
/// content detection and the configuration product the composition root attaches to the ORM.
pub use erp_blob as blob;
pub use erp_domain::{Domain, DomainError, Operator, OptimizationLevel};
pub use erp_fields::TextCollation;
/// Exact money primitives (hook H41): `ExactRatio`, `Increment`, `RoundingMode`,
/// `FxSnapshot` and the strict decimal text. Pure; also built for the WASM guest.
pub use erp_fields::money;
pub use erp_fields::{
    BigInt, Blob, Cell, ColumnType, Condition, Date, DateRange, DateTime, DatetimeRange, Decimal,
    Digits, Error, ErrorKind, FieldType, FieldValue, ImageBounds, IndexKind, OnDelete, Range,
    RangeBounds, RangeValue, RealId, RecId, Result, TimestampTz, Uuid, Value, ValueMap, Xid8,
    serde_json,
};
/// The kind of a principal (user, agent, service, public, link, system).
pub use erp_orm::PrincipalKind;
/// Image fields: the database's image limits ([`image::ImagePolicy`]), the system parameter names
/// and the admission function an `Image` field applies (B6).
pub use erp_orm::image;
/// Serialization traits used by the typed SDK wire schemas.
pub mod serde {
    pub use serde::{Deserialize, Serialize};
}
pub use erp_macros::{addon, extend, methods, migration, model, model_ref};
/// The right to mark a transaction as loading `base`'s data; minted only by the ORM builder
/// (`OrmBuilder::build_with_base_install_grant`). Not in the prelude.
pub use erp_orm::BaseInstallGrant;
/// Typed channel for a payload-free notification delivered only if the transaction commits.
pub use erp_orm::NotifyChannel;
/// What a transaction is opened for. Addon code that acts for a caller uses
/// [`PrincipalHint::caller`], [`BeginOptions::caller`], [`Orm::begin_caller`] or
/// [`Orm::env_caller`], which never produce the `System` principal (A4).
pub use erp_orm::PrincipalHint;
/// Company fields that mirror a company-dependent default (`company_default_for`,
/// supplement of 2026-11-15): write the declared default for companies without one.
/// The base addon already calls it at install and at company creation.
pub use erp_orm::company_default_for::apply_declared_defaults;
pub use erp_orm::{
    AccessPolicy, Actor, Assign, AsyncComputeSuper, Base, Batch, BeginOptions, BoxFuture, Cmd,
    Command, CompanyError, ComputeSuper, Context, Cursor, Env, Fetch, Field, FieldAccessCtx,
    FieldAccessSuper, FieldDenialDetail, FieldMask, FieldOp, FieldSearchContext,
    FieldSearchRequest, FieldSearchSuper, GeneratedInput, Granularity, GroupValue, Groups,
    Inherits, Isolation, Load, MAX_NOTICE_MESSAGE_BYTES, MAX_NOTICES_PER_TRANSACTION, Many2one,
    Mapped, MemoStats, Model, NAME_SEARCH_LIMIT, NameSearch, Notice, NoticeKind, OnchangeResult,
    OnchangeWarning, Operation, OperationIdentity, Orm, OrmBuilder, OrmCacheKey, OrmConfig, Page,
    RawVals, ReadGroup, ReadGroupingSets, ReadTx, RecordSet, Records, RecordsAs, RequestInfo,
    ResCompany, ResUsers, Row, RowsStream, SUPERUSER_ID, SavepointOutcome, SearchOptions,
    SearchQuery, SelectionContext, SelectionLimits, SelectionSuper, SqlRow, SqlValue, StaticPolicy,
    StreamOptions, Super, Transaction, TypedRelation, UnlinkHook, UserId, Vals, X2many,
    X2manyValue, missing_reset_target,
};
/// Business keys, upsert and row versions (addon guide "Keys, upsert and row
/// versions").
pub use erp_orm::{
    Accumulable, FieldOf, KeyRef, Upsert, UpsertCall, UpsertOutcome, UpsertResult, Upserted,
    UpsertedRow, WriteBatch, WriteHook, WriteHookFn, WrittenRow,
};
/// `Orm::build_object`: its options and report. An operator tool (`erp db
/// build-object`): the build takes `ACCESS EXCLUSIVE` on the table.
pub use erp_orm::{
    BuildObjectOptions, BuildObjectReport, BuildOutcome, BuildTimeout, LockTimeout, ObjectKind,
};
/// Named capabilities: declare one with [`capability!`]; the
/// `ir.capability` table maps it to groups.
pub use erp_orm::{Capability, capability};
/// Explicit native whole-unit retry policy and failure metadata.
pub use erp_orm::{RetryPolicy, RunFailure, RunOptions, require_top_level_unit};

/// Tracked fields (addon guide "Tracked fields").
pub use erp_orm::{
    CREATION_LOG_DISABLE_KEY, PrecommitHook, PrecommitHookFn, TRACKING_DISABLE_KEYS, TrackedChange,
    TrackedRecord,
};
/// Leaf partitioning: the upkeep calls' arguments and report, and the declaration
/// parts a runtime `ModelDecl` uses.
pub use erp_orm::{
    Horizon, KeepAfter, PartitionAction, PartitionEntry, PartitionReport, PartitionStep,
    PartitionWindow,
};
/// Declared access-rule operators (Amendment 1): declare one with [`rule_operator!`] and
/// the policy compiler accepts its spelling in the domain text of `ir.access` rows on the model
/// the declaration names.
pub use erp_orm::{RULE_OPERATORS, RuleOperatorDecl, rule_operator};
pub use erp_registry::{Partition, PartitionDecl, PartitionInterval, PartitionRetain};
/// The audit trail: typed auth lifecycle events and the audited elevation of server actions.
/// There is no free-text event API.
pub mod audit {
    pub use erp_orm::audit::{
        AccountLockDateChanged, AccountLockField, ApiKeyIssued, ApiKeyRevoked, ApiKeysCollected,
        AttachmentProbeRecorded, AuditError, AuditStoreState, AuthEvent, ElevationReason,
        EmergencyReason, EmergencyRevocation, IdentityLink, KeyRouteScope, LinkedBy,
        PasswordChanged, ProbeOutcome, RevokeCause, SecretRotated, SessionCause, SessionRevoked,
        SuperuserAction, UserSessionsRevoked,
    };
}

pub mod sequence {
    pub use erp_orm::sequence::{
        FiscalYear, MAX_NAME_BYTES, MAX_PATTERN_PADDING, MAX_PATTERN_TEXT_BYTES, PeriodKey,
        SequencePattern, SequencePatternBuilder, SequenceReset, SequenceWaitMode, YearDigits,
        draw_by_code, draw_by_id, render_sequence_format, resolve_period_key,
    };
}
/// Record access: what `#[access_domain]` overrides, grant sources and
/// policy-source registrations ([`access::policy_source!`]) use. Principal values enter a domain
/// only as parameters ([`access::PrincipalParam`]).
pub mod access {
    pub use erp_domain::{NowKind, PrincipalParam, UserPath};
    pub use erp_orm::{
        AccessCtx, AccessSuper, ElevationKind, GRANT_SOURCES, GrantDomain, GrantRef, GrantSource,
        GroupClause, GroupId, GroupMembership, GroupSpec, GuardEvaluation, GuardPlan,
        GuardPlanLimits, KindClass, Operation, POLICY_SOURCE_WRITES, PolicySourceWrite,
        PrincipalKind, SourceKind, policy_source,
    };
}
/// The query AST: a stored, versioned, validated `search_read` or `read_group`, built
/// or parsed once and run later by whoever holds it, with that person's rights.
pub mod query {
    pub use erp_domain::query::*;
    pub use erp_fields::json::Json;
    pub use erp_orm::query::{QueryResult, check, run};
}
/// The web client's methods (`web_read`, `web_save`, `web_read_group` …) and their arguments.
pub mod web {
    pub use erp_orm::web::*;
}

/// The existing Elang expression parser and its versioned, validated wire document.
pub mod expr {
    /// Precise parser failures let addons distinguish bounded-expression refusals from syntax errors.
    pub use erp_expr::ExprError;
    pub use erp_expr::wire;
    pub use erp_expr::{
        EvalLimits, Expr, Literal, SourceParsePreflight, SubscriptKind, UnaryOp, parse,
        parse_with_limits, source_preflight,
    };
}

/// The messaging store wire: schema and answer
/// builders, with the snapshot descriptor of `Transaction::store_snapshot`.
pub mod store_wire {
    pub use erp_orm::store_wire::*;
}

/// Typed extension points of an addon that another addon declares (a registry of the addon, not of
/// core): the addon that owns the registry names a `slot`, the contributing addons push `static`
/// items into [`extensions::EXTENSIONS`], and the owner reads them back with
/// [`extensions::items`]. It exists because an addon crate may not declare a `linkme` slice itself
/// (`unsafe_code` is forbidden there); the slice is declared here once. An item of another type
/// under the same slot is an error, never skipped.
pub mod extensions {
    use std::any::Any;

    use erp_fields::{Error, Result};

    /// One contribution: `item` is a `static` of the type the `slot`'s owner documents.
    #[derive(Debug, Clone, Copy)]
    #[non_exhaustive]
    pub struct Extension {
        /// The owner's slot name, for example `mail.store_topic`.
        pub slot: &'static str,
        /// The contributing addon.
        pub addon: &'static str,
        /// The contributed item.
        pub item: &'static (dyn Any + Send + Sync),
    }

    impl Extension {
        /// A contribution of `addon` to `slot`.
        #[must_use]
        pub const fn new(
            slot: &'static str,
            addon: &'static str,
            item: &'static (dyn Any + Send + Sync),
        ) -> Self {
            Self { slot, addon, item }
        }
    }

    /// Every extension contribution linked into the application.
    #[linkme::distributed_slice]
    pub static EXTENSIONS: [Extension];

    /// The contributions to `slot`, as `T`, with their addon, in link order (not a stable order:
    /// sort by name when order matters).
    ///
    /// # Errors
    /// `Programming` when a contribution to `slot` is not a `T`.
    pub fn items<T: Any>(slot: &str) -> Result<Vec<(&'static str, &'static T)>> {
        let mut out = Vec::new();
        for extension in EXTENSIONS.iter().filter(|e| e.slot == slot) {
            let item = extension.item.downcast_ref::<T>().ok_or_else(|| {
                Error::programming(format!(
                    "addon '{}' contributes an item of the wrong type to the extension slot \
                     '{slot}'",
                    extension.addon
                ))
            })?;
            out.push((extension.addon, item));
        }
        Ok(out)
    }
}

/// Exposing methods over RPC: effect classes, the argument and result conversions
/// `#[rpc]` methods use, and the exposure table.
pub mod rpc {
    pub use erp_orm::rpc::{
        ArgError, ArgShape, Binary, BindError, Effect, Exposed, Exposure, ExposureTable, FromRpc,
        IntoRpc, NotOffered, Offered, PRIMARY_READS, ParamSpec, PrimaryRead, Receiver, RpcArgs,
        json_type, null,
    };
}

/// Report template admission (`erp_report`): the closed print profile and the
/// attachment field check that `base` runs when a report action and its template load; and the
/// compile, plan, load and evaluate steps that turn a report and its records into a document
/// (`base::report_print`). No layout: the node lays the document out.
///
/// The list is explicit: an item `erp_report` adds reaches addons only when it is added here.
/// Native addons can encode a barcode on a physical printer grid without a direct dependency:
///
/// ```
/// use erp_sdk::report::{Code, Limits, ModuleGrid, QrLevel, ReportError, Symbology};
/// let grid = ModuleGrid::new(203)?;
/// let code = Code::encode(Symbology::Code128, "WH/OUT/00012", QrLevel::Low, &Limits::default())?;
/// let graphic = code.fit(240.0, 36.0, grid)?;
/// assert!(!graphic.vector().rectangle_list().is_empty());
/// # Ok::<(), ReportError>(())
/// ```
pub mod report {
    /// The context a report program evaluates in (`docs`, `company`, plain values).
    pub use erp_expr::EvalContext;
    pub use erp_orm::report_helpers::{REPORT_HELPERS, ReportHelperDecl};
    pub use erp_report::{
        Alignment, AttachmentPolicy, Code, Diagnostic, DocTree, DocumentBuilder, DocumentOptions,
        EvalOptions, FieldInfo, HelperDependencies, ImageFit, LangFormat, Language, Limits,
        ModuleGrid, Node, NodeId, PageSetup, PictureQuality, PlanContext, ProfileKind, QrLevel,
        RecordStore, ReportError, ReportLimits, ReportProgram, RestrictedField, Style, Symbology,
        TemplateSource, Validation, check_attachment, render_html, validate_arch,
    };
    /// The data stage under the caller's rights: `load_records` fills the store a program
    /// evaluates from; `field_info` describes the registry's fields to the planner.
    pub use erp_report_data::{
        DataError, HtmlImagePolicy, LoadOptions, field_info, load as load_records,
        load_with_options as load_records_with_options, report_helpers,
    };
}

/// The view arch engine (`erp_views`): parsing, combining and validating
/// view archs, and the pure half of the `ir.ui.view` records (`mode` rules, validation targets,
/// typed errors).
///
/// The list is explicit: an item `erp_views` adds reaches addons only when it is added here.
pub mod views {
    /// Raw human UI transport DTOs; server admission and authorization remain required.
    pub use erp_views::wire;
    pub use erp_views::{
        Arch, CombinedView, ExpressionKind, GuardLookup, GuardedArch, Inheritance, LimitKind,
        NodeIdx, NodeOrigin, NodeRef, ViewCatalogue, ViewDescriptor, ViewError, ViewLimits,
        ViewResult, XpathErrorKind, check_xml_nesting, expression_references,
        panel_domain_references, root_tag, validate_dashboard, validate_panel_domain,
    };

    /// The checked bridge between a [`GuardedArch`] and the ORM's guard plan: the only way to
    /// serve a filtered arch.
    pub mod host_access {
        pub use erp_views::host_access::{
            FilteredArch, checked_panel_view_input, checked_view_input, filter,
            panel_syntax_notice, plan_of,
        };
    }

    /// Native addon declarations for field widgets understood by the web client.
    ///
    /// An addon contributes a static declaration with `linkme::distributed_slice`. Metadata
    /// producers use this registry to reject undeclared widget names and incompatible field
    /// kinds before serializing a view.
    #[derive(Debug, Clone, Copy)]
    #[non_exhaustive]
    pub struct WidgetDecl {
        /// Stable client widget identity, for example `acceptance_notes.note_title`.
        pub key: &'static str,
        /// ERP field type names this widget can render.
        pub field_types: &'static [&'static str],
    }
    impl WidgetDecl {
        /// Declare a stable field widget and the field kinds it can render.
        pub const fn new(key: &'static str, field_types: &'static [&'static str]) -> Self {
            Self { key, field_types }
        }
    }

    /// Every native addon-declared field widget linked into the application.
    #[linkme::distributed_slice]
    pub static FIELD_WIDGETS: [WidgetDecl];

    pub use erp_views::{
        BuildView, CheckedContent, CheckedNode, CheckedViewInput, DescriptionBudget,
        DescriptionBuffer, DescriptionBuilder, DescriptionEncoder, DescriptionObjectBuilder,
        DescriptionPayload, DescriptionReservation, DescriptionValue, DescriptionValueBuilder,
        FieldPredicate, FieldReference, InstalledClosure, MetadataFieldSet,
        MetadataFieldSetBuilder, OwnerBoundViewTypes, PublishedViewTypes, ReferenceScope,
        ReferenceVisitor, SchemaNode, SchemaProperty, SchemaReferences, SchemaVariant,
        ValidateView, ViewSchema, ViewTypeContribution, ViewTypeDecl, ViewTypeDeclBuilder,
        ViewTypeDeclFn, calendar_schema, find_view_type, form_schema, graph_schema,
        is_valid_view_type_key, kanban_schema, list_schema, pivot_schema, search_schema,
        validate_view_types,
    };

    /// Every native addon-declared view type linked into the application (H18).
    ///
    /// Each entry builds its declaration on demand because the validated builder is not `const`;
    /// the consumer treats an `Err` as a refused registration.
    #[linkme::distributed_slice]
    pub static VIEW_TYPES: [ViewTypeDeclFn];

    /// The pure half of the `ir.ui.view` records (`erp_views::record`).
    pub mod record {
        pub use erp_views::record::{
            Alone, InvalidView, RecordLimit, Targets, ViewLabel, ViewMode, ViewRecordError,
            ViewRow, inheritance, inheritance_cycle, mode_on_create, mode_on_write, parse_alone,
            validation_targets, widens_validation,
        };
    }

    /// Write-time validation of a combined arch (`erp_views::validate`).
    pub mod validate {
        pub use erp_views::validate::{
            ArchIssue, ArchWarning, ArchWarnings, FULL_VALIDATION_TASK, MAX_GROUP_REFS, QWEB,
            RegistryLookup, Scope, group_refs, group_refs_with_limits, reads_fields,
            validate_combined, validate_combined_in,
        };
    }
}

/// Addon migrations: what a `#[migration]` function receives and the stage names.
///
/// ```
/// erp_sdk::addon!(name = "shop", depends = ["base"]);
///
/// use erp_sdk::migrations::SemverVersion;
/// use erp_sdk::{BoxFuture, Result, Transaction};
///
/// // runs when `shop` is upgraded past 1.1, after its schema update and data files
/// #[erp_sdk::migration(addon = "shop", version = "1.1", stage = post)]
/// async fn fill_reference(tx: &mut Transaction, from: &SemverVersion) -> Result<()> {
///     if from.minor == 0 {
///         tx.execute("UPDATE shop_order SET reference = name WHERE reference IS NULL", &[])
///             .await?;
///     }
///     Ok(())
/// }
///
/// // a plain fn must already have the `MigrationFn` type
/// #[erp_sdk::migration(addon = "shop", version = "1.1", stage = pre, name = "pre-rename")]
/// fn rename_column<'a>(
///     tx: &'a mut Transaction,
///     _from: &'a SemverVersion,
/// ) -> BoxFuture<'a, Result<()>> {
///     Box::pin(async move {
///         tx.batch("ALTER TABLE shop_order RENAME COLUMN ref TO reference").await
///     })
/// }
///
/// fn main() {}
/// ```
pub mod migrations {
    pub use erp_data::migration::{MigrationFn, MigrationStage, SemverVersion};
    pub use erp_orm::CompanyMigrationPolicy;
}

/// The context keys the data loader sets on the writes of a module operation
/// (`erp_data::load::load_state` documents their layout). An addon that reads the load state
/// names the keys through here, so a renamed key breaks its build instead of a run (review T-4).
pub mod load_state {
    pub use erp_data::load::load_state::{
        BATCH_NAMES, BATCH_POSITIONS, BATCH_WRITTEN, CHECK_XML, CHECKED_VIEW_FIELDS,
        DECLARING_MODULE, INSTALL_FILENAME, INSTALL_MODE, INSTALL_MODULE, LOADED_NAMES,
        PENDING_MODULES, TRACK_BATCH,
    };
}

/// A fully qualified external id (`module.name`), as `*_view_ref` context values and group
/// references name records; [`PrincipalError::InvalidXmlId`] says why a value does not parse.
pub use erp_orm::{PrincipalError, QualifiedXmlId};

/// What a typed auth event names (`audit::ApiKeyRevoked::new`): the key and its kind.
pub use erp_orm::{ApiKeyId, KeyKind};

/// A validated company id, as [`Transaction::companies`] returns them (the approval policy
/// resolution of `base` names a request's companies with it).
pub use erp_orm::CompanyId;

/// Authentication: what `_check_credentials`
/// overrides take and return, password hashing, the session-end hook and the user context.
pub mod auth {
    pub use erp_security::auth::apikey::ApiKeyHasher;
    pub use erp_security::auth::apikey_manage;
    pub use erp_security::auth::apikey_store::{
        IssueRequest, IssuedApiKey, IssuedSecret, KeyKind, KeyScope, issue,
    };
    pub use erp_security::auth::password::limits as hash_limits;
    pub use erp_security::auth::secret::{DatabaseSecret, random_uuid_text};
    pub use erp_security::auth::settings::{
        CACHE_NAME, COOLDOWN_AFTER_FALLBACK, COOLDOWN_DURATION_FALLBACK, MAX_INACTIVITY_DEFAULT,
        MAX_LIFETIME_DEFAULT, SessionPolicy, keys,
    };
    pub use erp_security::auth::{
        ACCESS_DENIED, AuthInfo, AuthMethod, CheckEnv, Credential, EndReason, EndedSession,
        HashParams, HashPool, LoginCooldownContext, LoginCooldownPolicy, LoginDomain, LoginLookup,
        LoginOrder, LoginOrderTerm, LoginSelectionContext, LoginSelectionLimits, Mfa,
        OrderDirection, SelectionWitness, SessionEnd, SessionEndFn, SessionId, SessionTokenFields,
        StoredHash, StoredPassword, UserContext, Zeroizing, access_denied, check_password,
        core_tables, end_sessions, end_user_sessions, hash_all_with_params, hash_password,
        hash_with_params, load_hash_params, log_plaintext_replaced, user_context, validate_setting,
        validate_setting_in,
    };
    pub use erp_security::session_end_hook;
}

/// Domain literals and AST access (`erp_domain`), for building conditions with values beyond the
/// common `Into<Value>` conversions.
pub mod domain {
    pub use erp_domain::*;
}
pub use erp_registry::{
    AddonDescriptor, CheckCompany, CompanyAccessState, CompanyMatch, CompanyScope, Databases,
    DefaultContext, DefaultValue, DropReason, DroppedField, ExclusionDecl, ExclusionDef, FieldDef,
    FieldSpec, IndexDef, IndexMethod, KindDef, M2mTable, ManualFieldPolicy, ManualModel, ModelDef,
    ModelKind, ModelSpec, Overlay, PgExtension, Registry, RegistryBuilder, RegistryCache,
    TemporalKeyDecl, TemporalKeyDef,
};
/// PostgreSQL identifier rules the registry applies: an addon that names a
/// database object the registry made (a constraint, an index) computes the name the same way.
pub mod ident {
    pub use erp_registry::ident::{MAX_IDENTIFIER_LEN, shorten_identifier};
}

/// AI provider and metering interfaces (AI0.9, core hook H6).
pub mod ai;

pub use ai::{
    BoxModelStream, ChatMessage, CompletionRequest, CompletionResponse, EmbeddingProvider,
    EmbeddingRequest, EmbeddingResponse, FinishReason, MessageRole, ModelProvider, ProviderError,
    StreamChunk, UsageRecord,
};

/// What addon code usually needs.
pub mod prelude {
    pub use std::sync::Arc;

    pub use crate::{
        Assign, Base, Batch, BigInt, Blob, Cell, Cmd, Command, ComputeSuper, Cursor, Date,
        DateRange, DateTime, DatetimeRange, Decimal, Domain, Env, Error, Field, FieldAccessCtx,
        FieldAccessSuper, FieldMask, FieldOp, FieldSearchContext, FieldSearchRequest,
        FieldSearchSuper, GroupValue, Groups, Many2one, NameSearch, Notice, NoticeKind,
        OnchangeResult, Operator, Page, RawVals, ReadGroup, ReadTx, RecId, RecordSet, Records,
        ResCompany, ResUsers, Result, SearchOptions, Super, TimestampTz, Transaction, Upsert,
        UpsertCall, UpsertOutcome, UpsertResult, Uuid, Vals, Value, X2many, Xid8, extend, methods,
        model,
    };
}

/// Paths used by macro output. Not a public API.
#[doc(hidden)]
pub mod __private {
    pub use erp_data::migration::{MIGRATIONS, MigrationImpl, MigrationStage, SemverVersion};
    pub use erp_domain::Domain;
    pub use erp_fields::Value;
    pub use erp_fields::{
        Digits, IndexKind, OnDelete, RangeBounds, RefType, Result, ValueFor, kinds,
    };
    pub use erp_orm::{
        ACCESS_DOMAIN_IMPLS, AccessDomainImpl, Assign, AsyncComputeSuper, Batch, BatchData,
        BoxFuture, COMPUTE_IMPLS, CONSTRAINT_IMPLS, ComputeEntry, ComputeImpl, ComputeSuper,
        ConstraintImpl, FIELD_ACCESS_IMPLS, FIELD_SEARCH_IMPLS, Field, FieldAccessCtx,
        FieldAccessImpl, FieldAccessSuper, FieldMask, FieldOp, FieldSearchContext, FieldSearchImpl,
        FieldSearchRequest, FieldSearchSuper, ImplKind, Inherits, KeyRef, METHOD_CODECS,
        METHOD_WIRE, MethodCodecImpl, MethodFn, MethodImpl, MethodWireImpl, Model, Outputs, ReadTx,
        RecordSet, Records, SelectionContext, SelectionFn, SelectionSuper, SigProbe, SigProbeOther,
        SigProbeSerde, Super, Token, Transaction, TypedCodec, WireCheck, WireCheckOther, WireForm,
        WireProbe, WireProbeOther, WireProbeSerde, WireSeal, call_method,
        dispatch::METHOD_IMPLS,
        onchange::{ONCHANGE_IMPLS, OnchangeImpl},
        rpc::{
            ASSISTANT_OFFERS, AssistantOffer, BindError, Effect, FromRpc, IntoRpc, ParamSpec,
            RPC_IMPLS, Receiver, RpcArgs, RpcDecl, RpcImpl,
        },
    };
    // Reserved bridge for base's framework-owned hierarchy fence (not an addon API).
    #[cfg(feature = "__base-core-ddl")]
    pub use erp_orm::{CoreTableRequirement, base_check_write_fields, core_ddl};
    pub use erp_registry::{
        AddonDescriptor, CompanyAccessDecl, CompanyBranch, CompanyMatch, DEFAULT_TRACKING_SEQUENCE,
        EmptyCompany, ExclusionDecl, FieldDecl, GENERATED_INPUT_ADAPTER_DECLS,
        GeneratedInputAdapterDecl, GeneratedStorage, IndexDecl, IndexMethod, KeyDecl, KindDecl,
        MODEL_DECLS, ModelDecl, ModelKind, PgExtension, SelectionOnDeleteDecl, SignalTriggerDecl,
        StaticValue, TableObjectDecl, TableObjectKind, TemporalKeyDecl, TriggerSignal,
    };
    pub use erp_registry::{PartitionDecl, PartitionInterval, PartitionRetain};
    pub use linkme;

    pub use crate::descriptor::{__model_ref_detail, Contribution, ContributionKind, DESCRIPTORS};
}

/// Sandboxed Luau values/evaluation and caller-bound external-id resolution for addon runners.
/// `env_ref` does not grant target access; consumers must check the returned record's rights.
pub mod script {
    pub use erp_data::{XmlId, model_data::env_ref};
    pub use erp_script::{
        Evaluation, HostError, LimitKind, LuauSandbox, ScriptCancellation, ScriptContext,
        ScriptError, ScriptLimits, ScriptValue,
    };
}

/// Pure Elang template compilation for addon-owned install-time asset assembly.
pub use erp_ui_compiler as ui_compiler;
