//! Entry-point enumeration (design note §8.3; first version, owns the final
//! one): every public function of `erp-orm` that can read or write records is listed in
//! [`ENTRY_POINTS`], and every listed one that should pass through the access engine is shown to
//! do so for a user principal.
//!
//! How additions are caught: [`every_public_entry_point_has_a_row`] reads `erp-orm`'s source
//! (`scan`, with `syn`): records are only reachable through the transaction, so every function
//! that can touch one takes `&mut Transaction` (or is a `&mut self` method of `Transaction`). The
//! scan lists every such public function and inherent method of the crate's module tree (test
//! modules skipped); the test fails on a function without a row and on a row without a function.
//! A new public entry point therefore cannot land without someone deciding, in this table,
//! whether it is checked (and proving it) or why it is not. The scan also reads public trait
//! methods and any argument type that mentions `Transaction`; what it cannot see is listed in
//! `scan.rs` (macro-generated items, other crates).
//!
//! How "passes through the engine" is shown: [`checked_entry_points_go_through_the_engine`] runs a
//! probe of every `Checked` row twice, under a recording policy that hides one of two records
//! (`val > 0` on `test_access.some_obj`, every operation):
//! - as a user, the call must refuse the hidden record with the record-level access error, or
//!   leave it out of its answer, **and** the policy must have been asked about
//!   (`test_access.some_obj`, operation) during the call;
//! - as `System` (superuser mode, the engine allows without asking), the same call must succeed
//!   and see the hidden record, so the user's refusal comes from the policy and not from a broken
//!   probe. The exception is `CheckedRefusesSu` (the query AST, which never runs in superuser
//!   mode): there the `System` call must fail with the `Access` error naming.
//!
//! [`sudo_sites_are_listed`] does the same bookkeeping for the `sudo(true)` call sites of every
//! crate and addon (outside tests), and for the engine's own `engine_sudo()` calls in `erp-orm`
//! (superuser mode that keeps an elevation block's lifetime): each needs a row with its
//! reason. [`read_audit_sites_are_listed`] does it for the call sites of the read-audit owner
//! (`ReadAuditOwner::new`, `read_only_audited`, `take_audit_attempt`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "entry_points/scan.rs"]
mod scan;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use erp_orm::{
    AccessDomain, AccessPolicy, BeginOptions, Capability, CompanyId, ExplainRequest, Explanation,
    FormattedReadGroup, FormattedReadGroupingSets, GroupId, GroupSet, InternedGroups, Load,
    NameSearch, Operation, PolicyGeneration, PrincipalBinding, PrincipalBuilder, ProgressBar,
    RawVals, ReadGroup, ReadGroupingSets, Records, Resolution, SearchOptions,
    SearchPanelSelectMultiRange, SearchPanelSelectRange, SnapshotRef, Spec, StreamOptions,
    Transaction, Upsert, UpsertCall, UserId, WebNameSearch, WebReadGroup, WebSave, WebSearchRead,
};
use erp_registry::ModelSpec;
use erp_sdk::{Cell, Command, Domain, Operator, Value, ValueMap};
use erp_test_models::test_access::{Keyed, SomeObj};
use erp_test_models::test_rel::CounterItem;
use erp_test_models::{base_users, test_access, test_rel};
use erp_testkit::{DomainPolicy, Harness, Otherwise};

const OBJ: &str = "test_access.some_obj";
/// The model of the typed-relation probe (`RecordSet::related` needs a `Many2one<C>` field).
const ITEM: &str = "test_rel.counter_item";
/// The model of the upsert probes (a business key on `name`).
const KEYED: &str = "test_access.keyed";
/// The declared message of its key.
const KEYED_MESSAGE: &str = "A keyed object of this name already exists.";
const DEMO: i64 = 2;
/// The refusal of a query AST run in superuser mode.
const NO_SUDO: &str = "A query AST is not run in superuser mode: audited elevated replay is not \
                       supported until a later release and a later release";

/// A stored `search_read` of the probe model (the query AST).
fn query_ast() -> erp_sdk::query::QueryAst {
    erp_sdk::query::QueryAst::parse(
        br#"{"version":1,"kind":"search_read","model":"test_access.some_obj","fields":["name"],"limit":10}"#,
        &erp_sdk::query::QueryLimits::default(),
    )
    .unwrap()
}

/// The model a probe acts on.
fn model_of(name: &str) -> &'static str {
    if name == "RecordSet::related" {
        ITEM
    } else if name.ends_with("::upsert")
        || name.ends_with("::upsert_values")
        || name == "Records::expect_versions"
    {
        KEYED
    } else {
        OBJ
    }
}

// ----- the table ---------------------------------------------------------------------------------

/// What a probe of a checked entry point must show for a user principal.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// The call refuses the hidden record with the record-level access error of `op`.
    Refused(Operation),
    /// The call answers without the hidden record (searches, groupings, filters).
    Hidden(Operation),
    Asked(Operation),
    /// The call refuses with the key's declared `Validation` message and no id: the record is
    /// denied for `op` and the caller may not read it either, so it is not even named (the upsert
    /// of a business key).
    Withheld(Operation),
}

/// The class of an entry point.
#[derive(Debug, Clone, Copy)]
enum Class {
    /// Reads or writes records: decided by the engine, proven by a probe.
    Checked(Expect),
    /// As `Checked` for a user, but superuser mode is refused (`Access`, the query AST): the
    /// `System` half of the proof must fail with that error instead of succeeding.
    CheckedRefusesSu(Expect),
    Unchecked(&'static str),
    Gap(&'static str),
}

use Class::{Checked, CheckedRefusesSu, Gap, Unchecked};
use Expect::{Asked, Hidden, Refused, Withheld};
use Operation::{Create, Read, Unlink, Write};

const DISPATCH: &str =
    "dispatch: runs a method chain; the methods' own record entry points are rows of this table";
const RAW_SQL: &str = "raw SQL on the transaction's connection (the reference ERP env.cr.execute): core, migrations and tests; no record rule applies by design";
const TX_CONTROL: &str = "transaction control (commit, rollback, savepoints, flush of writes already checked when they were made, cache invalidation): no record is read or written for the caller";
const NOTICE_CHANNEL: &str = "transaction notice channel (in-memory warnings and notices emitted during execution): no record is read or written";
const SIGNAL: &str = "SIGNAL generations and notifications: no record";
const ROLE_GRANT: &str = "role privileges on core tables: REVOKE/GRANT is DDL, not a record operation; refused by the function itself to every principal but System with full elevation (test: system_only_entry_points.rs, the_role_grants_refuse_a_user_transaction), and the grantee is checked (no PUBLIC, NONE or pg_* role, no superuser, no member of the table owner): no record of a model is read or written";
const SCHEMA: &str = "schema and reflection DDL of the install path (System); no record of a model is read for a caller";
const MODULE_OPERATION: &str = "module operation plumbing, called by the module system on its own System transaction and refused to every other principal by the function itself (tests: system_only_entry_points.rs, operation_control.rs): no record is read or written for a caller; the hooks it calls are methods whose own record entry points are rows of this table";
const ORMCACHE: &str =
    "process-wide ormcache of method results: the cached method checks access when it runs";
const NEW_RECORD: &str = "a new record in memory (the reference ERP new()): no row is read or written; the operation that saves it is checked";
const GROUP_CHECK: &str = "asks the policy about the groups or capabilities of the principal of the environment it is called on, which may be any `with_user(x)` environment (the engine resolves that user, as the reference ERP env.user.has_groups does for env.user); reads no record of any model and cannot elevate";
const PARITY_EXISTS: &str =
    "existence only, no rule (the reference ERP exists(), models.py _existing;)";
const PARITY_LOCK: &str = "row locks without a rule, as the reference ERP (models.py:5056-5112 lock_for_update/try_lock_for_update select ids FOR UPDATE with no access check); a user can lock rows it cannot read: flagged for a later release";
const DEFAULTS: &str = "no record of the model is read: field defaults (ir.default through ir_default::get_model_defaults); default x2many commands check their comodel's model access (defaults.rs); field groups a later release";

/// Every public entry point of `erp-orm` that takes the transaction.
const ENTRY_POINTS: &[(&str, Class)] = &[
    (
        "field_check::base_check_write_fields",
        Unchecked(
            "reserved base field-mask preflight only: no model row is read or written and no access is granted; callers first run Records::check_access, normal writers repeat all checks. Field-mask/commit behavior is exercised by base_user_preferences and base_users_policy",
        ),
    ),
    // reads
    ("Records::cell", Checked(Refused(Read))),
    ("Records::cells", Checked(Refused(Read))),
    ("Records::value", Checked(Refused(Read))),
    ("Records::values", Checked(Refused(Read))),
    ("Records::read", Checked(Refused(Read))),
    ("Records::read_with", Checked(Refused(Read))),
    ("Records::read_rows", Checked(Refused(Read))),
    ("Records::fetch", Checked(Refused(Read))),
    ("Records::related", Checked(Refused(Read))),
    ("Records::reference", Checked(Refused(Read))),
    (
        "Records::read_x2many_capped",
        Unchecked(
            "superuser-mode only (data expressions, as the reference ERP's loader runs as the superuser): refuses any other environment with Access before reading (test: x2many_capped.rs, a_capped_read_refuses_a_non_superuser_environment_and_a_non_singleton); its values are those of read_with under sudo",
        ),
    ),
    ("Records::filtered_domain", Checked(Refused(Read))),
    ("Records::filtered_path", Checked(Refused(Read))),
    ("Records::filtered_values", Checked(Refused(Read))),
    ("Records::mapped_path", Checked(Refused(Read))),
    ("Records::grouped", Checked(Refused(Read))),
    ("Records::grouped_records", Checked(Refused(Read))),
    ("Records::sorted", Checked(Refused(Read))),
    ("Records::web_read", Checked(Refused(Read))),
    ("Records::get_field_translations", Checked(Refused(Read))),
    ("Records::x2many_commands", Checked(Hidden(Read))),
    ("Records::check_access", Checked(Refused(Read))),
    ("Records::has_access", Checked(Hidden(Read))),
    ("Records::filtered_access", Checked(Hidden(Read))),
    // searches and groupings
    ("Records::search", Checked(Hidden(Read))),
    ("Records::search_with", Checked(Hidden(Read))),
    ("Records::stream", Checked(Hidden(Read))),
    ("RowsStream::next", Checked(Refused(Read))),
    (
        "RowsStream::close",
        Unchecked(
            "ends a stream: marks its cursor for the CLOSE that rides the transaction's next statement; no record is read or written",
        ),
    ),
    ("Records::search_count", Checked(Hidden(Read))),
    ("Records::search_query", Checked(Hidden(Read))),
    ("SearchQuery::ids", Checked(Hidden(Read))),
    ("SearchQuery::records", Checked(Hidden(Read))),
    ("SearchQuery::count", Checked(Hidden(Read))),
    ("Records::name_search", Checked(Hidden(Read))),
    ("Records::name_search_with", Checked(Hidden(Read))),
    ("Records::search_read", Checked(Hidden(Read))),
    // the query AST: decided like `search_read` for a user, refused in superuser mode
    ("query::run", CheckedRefusesSu(Hidden(Read))),
    ("query::check", CheckedRefusesSu(Asked(Read))),
    (
        "query::__statement",
        Unchecked(
            "tests only (feature `test-support`, not enabled in this crate, so no probe here): plans like `run`, with the same access checks, refuses superuser mode, and returns the statement without running it",
        ),
    ),
    ("Records::search_after", Checked(Hidden(Read))),
    ("Records::web_search_read", Checked(Hidden(Read))),
    ("Records::web_name_search", Checked(Hidden(Read))),
    ("Records::_read_group", Checked(Hidden(Read))),
    ("Records::read_group", Checked(Hidden(Read))),
    ("Records::_read_grouping_sets", Checked(Hidden(Read))),
    ("Records::formatted_read_group", Checked(Hidden(Read))),
    (
        "Records::formatted_read_grouping_sets",
        Checked(Hidden(Read)),
    ),
    ("Records::web_read_group", Checked(Hidden(Read))),
    ("Records::read_progress_bar", Checked(Hidden(Read))),
    ("Records::search_panel_select_range", Checked(Hidden(Read))),
    (
        "Records::search_panel_select_multi_range",
        Checked(Hidden(Read)),
    ),
    // writes
    ("Records::write", Checked(Refused(Write))),
    ("Records::write_base", Checked(Refused(Write))),
    ("Records::write_values", Checked(Refused(Write))),
    ("Records::set_value", Checked(Refused(Write))),
    ("Records::assign", Checked(Refused(Write))),
    (
        "Records::update_field_translations",
        Checked(Refused(Write)),
    ),
    ("Records::web_save", Checked(Refused(Write))),
    ("Records::web_save_multi", Checked(Refused(Write))),
    ("Records::web_resequence", Checked(Refused(Write))),
    ("Records::onchange", Checked(Refused(Write))),
    ("Records::onchange_batch", Checked(Asked(Create))),
    // business keys: the write rule on the existing rows; a denied row the caller
    // cannot read is withheld
    ("Records::upsert", Checked(Withheld(Write))),
    ("Records::upsert_values", Checked(Withheld(Write))),
    ("RecordSet::upsert", Checked(Withheld(Write))),
    // expected versions name the records with their current versions in a later Conflict: the
    // caller must be able to read them (review SEC-S6)
    ("Records::expect_versions", Checked(Refused(Read))),
    // creates and deletes
    ("Records::create", Checked(Refused(Create))),
    ("Records::create_values", Checked(Refused(Create))),
    ("Records::unlink", Checked(Refused(Unlink))),
    ("Records::web_unlink", Checked(Refused(Unlink))),
    // the typed surface
    ("RecordSet::get", Checked(Refused(Read))),
    ("RecordSet::mapped", Checked(Refused(Read))),
    ("RecordSet::related", Checked(Refused(Read))),
    (
        "RecordSet::context_today",
        Unchecked(
            "reads no record: today's date in the zone of the recordset's environment; the transaction is taken only so the same call compiles in a WebAssembly component",
        ),
    ),
    ("RecordSet::filtered_domain", Checked(Refused(Read))),
    ("RecordSet::filtered", Checked(Refused(Read))),
    ("RecordSet::sorted", Checked(Refused(Read))),
    ("RecordSet::grouped", Checked(Refused(Read))),
    ("RecordSet::search", Checked(Hidden(Read))),
    ("RecordSet::search_with", Checked(Hidden(Read))),
    ("RecordSet::search_count", Checked(Hidden(Read))),
    ("RecordSet::name_search", Checked(Hidden(Read))),
    ("RecordSet::name_search_with", Checked(Hidden(Read))),
    ("RecordSet::search_read", Checked(Hidden(Read))),
    ("RecordSet::write", Checked(Refused(Write))),
    ("RecordSet::set", Checked(Refused(Write))),
    ("RecordSet::create", Checked(Refused(Create))),
    (
        "Transaction::validate_lang_code",
        Unchecked(
            "reads the active codes of res_lang to validate one language code; returns no record \
             data (a later release)",
        ),
    ),
    (
        "Transaction::has_field_access",
        Unchecked(
            "answers the reference ERP's has_field_access for the field mask of the environment it is called \
             on (the pinned policy's masks, `#[field_access]` overrides included); reads no \
             record, grants nothing and cannot elevate (a later release: the attachment access check \
             asks it for res_field)",
        ),
    ),
    ("Transaction::has_group", Unchecked(GROUP_CHECK)),
    ("Transaction::has_groups", Unchecked(GROUP_CHECK)),
    (
        "Transaction::user_has_group",
        Unchecked(
            "asks the policy whether another user is in a group, decided on that user's resolution in this transaction without handing back an environment of that user (no with_user switch A4); reads no record of any model and cannot elevate; the caller decides who may ask (res.users.has_group)",
        ),
    ),
    ("Transaction::has_any_group", Unchecked(GROUP_CHECK)),
    ("Transaction::group_membership", Unchecked(GROUP_CHECK)),
    ("Transaction::has_capability", Unchecked(GROUP_CHECK)),
    (
        "Transaction::take_tracked_changes",
        Unchecked(
            "reads no other record: it flushes and returns the raw old and new values the \
             transaction's own writes captured (a later release) for records the caller \
             already wrote; filtering the values by the reader's field access is the consumer's \
             job (the mail addon, a later release)",
        ),
    ),
    (
        "Transaction::evaluate_guards",
        Unchecked(
            "decides a view bundle's guard plan for the environment's principal and returns only \
             booleans; it runs the engine's model read check on the root model and every model the \
             plan reads itself (tests in erp-security/tests/guard_eval.rs), reads no \
             record and cannot elevate",
        ),
    ),
    (
        "Transaction::companies",
        Unchecked(
            "reads no record: validates the environment's active companies against the subject's newest resolution",
        ),
    ),
    (
        "Transaction::store_snapshot",
        Unchecked(
            "reads no record: flushes the transaction's own writes and returns only the \
             PostgreSQL snapshot descriptor (xmin, xmax, in-progress xids) and the own xid; it \
             refuses a transaction that is not REPEATABLE READ",
        ),
    ),
    (
        "Transaction::enqueue",
        Unchecked(
            "writes one row of the core table erp_job, no record of a model: the boundary is the \
             enqueue API: the job stores the caller's own principal, run_as another \
             principal is refused unless the caller is the system (a later release elevation pending) and \
             a public or link principal is refused; the job's work is checked against that \
             principal when it runs (a later release)",
        ),
    ),
    (
        "Transaction::enqueue_many",
        Unchecked(
            "the batch form of Transaction::enqueue: the same principal capture and refusals, one \
             INSERT into the core table erp_job for all requests (a later release)",
        ),
    ),
    (
        "Transaction::grant_jobs_runtime_role",
        Unchecked(
            "changes privileges on the core job tables, reads and writes no record; only the \
             system principal with full elevation may call it, as the other core-table grants (a later release)",
        ),
    ),
    (
        "JobPrincipalResolver::admit",
        Unchecked(
            "decides which principal a stored job principal is now, reading only what the chunk's \
             own transaction already resolved for the stored subject (the ORM's principal \
             resolver); it reads no record of a model and returns the stored subject or a \
             refusal, never another principal",
        ),
    ),
    (
        "Transaction::job_hold",
        Unchecked(
            "parks one job of the core table erp_job, no record of a model; needs the job administration capability or the system principal",
        ),
    ),
    (
        "Transaction::job_resume",
        Unchecked(
            "returns one parked job of the core table erp_job to ready; same capability gate as job_hold (a later release)",
        ),
    ),
    (
        "Transaction::job_cancel",
        Unchecked(
            "cancels one job of the core table erp_job and writes its history row; same capability gate as job_hold (a later release)",
        ),
    ),
    (
        "Transaction::job_cancel_by_key",
        Unchecked(
            "cancels the live jobs of one kind and dedup key in the core table erp_job and writes their history rows, no record of a model; needs no capability because it touches only jobs whose stored principal is the caller's user (superuser mode, server code only, touches the key's jobs of any user), returns the count and never errors on a missing key",
        ),
    ),
    (
        "Transaction::job_cancel_by_keys",
        Unchecked(
            "the set-based form of job_cancel_by_key (same rules, at most 10 000 keys per call, a typed refusal beyond): cancels the live jobs of one kind and the given dedup keys in the core table erp_job, no record of a model reached only through the thread unlink cascade (`cancel_for_records`), with server-derived keys and a fixed kind",
        ),
    ),
    (
        "Transaction::job_requeue",
        Unchecked(
            "returns one dead letter of the core table erp_job to ready; same capability gate as job_hold (a later release)",
        ),
    ),
    (
        "Transaction::job_discard",
        Unchecked(
            "removes one dead letter of the core table erp_job into history; same capability gate as job_hold (a later release)",
        ),
    ),
    (
        "Transaction::job_upsert_recurring",
        Unchecked(
            "keeps the one recurring row of a schedule in the core table erp_job; only the system principal may call it, the schedule's owner has passed its own access check (a later release)",
        ),
    ),
    (
        "Transaction::job_upsert_recurring_own",
        Unchecked(
            "keeps the recurring row of a user's own schedule in the core table erp_job (ir.cron instead of a superuser switch.2 A4): only a User environment without a with_user switch, outside a block, for a kind that runs as its caller and a run_as equal to that user; never replaces another principal's row (Access); the schedule's owner has passed its own access check",
        ),
    ),
    (
        "Transaction::job_set_run_after",
        Unchecked(
            "moves the due time of a recurring row of the core table erp_job; only the system principal may call it (a later release)",
        ),
    ),
    (
        "Transaction::job_purge_history",
        Unchecked(
            "trims the history table erp_job_run; the queue's own upkeep, only the system principal may call it (a later release)",
        ),
    ),
    (
        "Transaction::job_expire_dead",
        Unchecked(
            "moves old dead letters of erp_job to history; the queue's own upkeep, only the system principal may call it (a later release)",
        ),
    ),
    (
        "Transaction::job_wake",
        Unchecked(
            "pokes the process's job runners after the commit and sends one payload-free NOTIFY; reads and writes no record (a later release)",
        ),
    ),
    (
        "Transaction::job_settings_changed",
        Unchecked(
            "the job_wake of a settings edit: called by the settings write (base ir_config_setting.rs) after a `base.jobs.*` value was stored under that write's own checks, it pokes the runners after the commit and sends one payload-free NOTIFY with the new policy's notify interval; reads and writes no record and grants nothing",
        ),
    ),
    ("RecordSet::exists", Unchecked(PARITY_EXISTS)),
    ("RecordSet::try_lock_for_update", Unchecked(PARITY_LOCK)),
    ("RecordSet::new_record", Unchecked(NEW_RECORD)),
    ("RecordSet::new_with", Unchecked(NEW_RECORD)),
    ("RecordSet::default_get", Unchecked(DEFAULTS)),
    // records without a rule, on purpose
    ("Records::exists", Unchecked(PARITY_EXISTS)),
    ("Records::lock_for_update", Unchecked(PARITY_LOCK)),
    ("Records::try_lock_for_update", Unchecked(PARITY_LOCK)),
    ("Records::default_get", Unchecked(DEFAULTS)),
    ("Transaction::new_record", Unchecked(NEW_RECORD)),
    // dispatch
    ("Records::call", Unchecked(DISPATCH)),
    ("Records::web_call", Unchecked(DISPATCH)),
    ("Records::copy", Unchecked(DISPATCH)),
    ("Records::copy_data", Unchecked(DISPATCH)),
    ("Records::name_create", Unchecked(DISPATCH)),
    ("Records::action_archive", Unchecked(DISPATCH)),
    ("Records::action_unarchive", Unchecked(DISPATCH)),
    ("RecordSet::copy", Unchecked(DISPATCH)),
    ("RecordSet::copy_data", Unchecked(DISPATCH)),
    ("RecordSet::name_create", Unchecked(DISPATCH)),
    ("RecordSet::action_archive", Unchecked(DISPATCH)),
    ("RecordSet::action_unarchive", Unchecked(DISPATCH)),
    ("dispatch::call_method", Unchecked(DISPATCH)),
    ("dispatch::call_method_json", Unchecked(DISPATCH)),
    ("MethodInvoker::invoke", Unchecked(DISPATCH)),
    ("MethodCodec::call_native", Unchecked(DISPATCH)),
    ("DynSuper::call_json", Unchecked(DISPATCH)),
    (
        "Transaction::extension",
        Unchecked(
            "per-transaction state slot of a layer above the ORM (the WASM host's instance pool): no record is read or written",
        ),
    ),
    ("Transaction::poison", Unchecked(TX_CONTROL)),
    ("Super::call", Unchecked(DISPATCH)),
    (
        "AsyncComputeSuper::call",
        Unchecked(
            "compute chains run with compute_sudo semantics decided by the field (SPEC-COMP); their reads and writes go through the rows above",
        ),
    ),
    (
        "Exposed::call",
        Unchecked(
            "RPC exposure table: runs an exposed method through dispatch (scope allowlist at RPC dispatch, DN §5); the method's record entry points are rows here",
        ),
    ),
    (
        "DataCallable::call",
        Unchecked(
            "data-callable level of the exposure table: runs a method named by a <function> of a data file through its binder (the module operation's principal); the method's record entry points are rows here",
        ),
    ),
    // the transaction itself
    // public trait methods that take the transaction (the implementations are the public items of
    // `erp-security`; the traits' methods are the API a new implementor must satisfy)
    (
        "PolicyProvider::rebuild_private",
        Unchecked(
            "builds the policy snapshot from the policy tables in the transaction's own view \
             (SEC-018): the policy itself is read as System by design, no record is read for a caller",
        ),
    ),
    (
        "PrincipalResolver::re_resolve",
        Unchecked(
            "re-reads the subjects' groups and companies after a membership change (SEC-018): \
             principal resolution is System by design, the result is the caller's own resolution",
        ),
    ),
    // pure descriptions that take the transaction by shared reference
    (
        "Records::fields_get",
        Unchecked(
            "describes the registry's fields (the reference ERP fields_get): no record is read; field groups \
             arrive with a later release",
        ),
    ),
    (
        "Records::optimize_domain_in",
        Unchecked(
            "rewrites a domain against the registry and the transaction's settings: no record is \
             read",
        ),
    ),
    ("Transaction::query", Unchecked(RAW_SQL)),
    ("Transaction::query_each", Unchecked(RAW_SQL)),
    ("Transaction::query_typed", Unchecked(RAW_SQL)),
    ("Transaction::execute", Unchecked(RAW_SQL)),
    ("Transaction::batch", Unchecked(RAW_SQL)),
    ("Transaction::open", Unchecked(TX_CONTROL)),
    (
        "Transaction::preflight_native_catalog_sources",
        Unchecked(
            "native lifecycle metadata inspection on the held idle connection; requires the fully elevated System principal before metadata SQL, reads PostgreSQL source identity only and no application record, refuses configured signals before automatic BEGIN reads (test: system_only_entry_points.rs, native_catalog_preflight_requires_system_and_idle_state)",
        ),
    ),
    (
        "wasm_abi1::wasm_abi1_check_fields",
        Unchecked(
            "admission pre-check of explicit field names before the WASM ABI1 host decodes values: reads and writes no record and returns only Ok or an error; when it must refuse (a native-typed field) it first runs the model/record/field access checks so a denied caller sees the access error, and otherwise it decides nothing, because the read, create or write that follows is checked by the engine (callers: compute.rs, erp-wasm-host adapter; test: wasm_native_compute_component.rs, native_fields_package_compute_refuses_native_metadata_after_acl)",
        ),
    ),
    (
        "wasm_abi1::wasm_abi1_check_call",
        Unchecked(
            "refusal-only guest CRUD JSON admission; performs no record mutation and grants no access; native diagnostics first run existing model/record/field checks; normal dispatch independently authorizes admitted calls (test: wasm_dispatch_component::guest_native_only_chains_refuse_native_values_before_mutation)",
        ),
    ),
    ("Transaction::commit", Unchecked(TX_CONTROL)),
    ("Transaction::commit_pipelined", Unchecked(TX_CONTROL)),
    ("Transaction::rollback", Unchecked(TX_CONTROL)),
    ("Transaction::savepoint", Unchecked(TX_CONTROL)),
    ("Transaction::savepoint_outcome", Unchecked(TX_CONTROL)),
    ("Transaction::after_commit", Unchecked(TX_CONTROL)),
    ("Transaction::capture_statements", Unchecked(TX_CONTROL)),
    ("Transaction::clear_notices", Unchecked(NOTICE_CHANNEL)),
    ("Transaction::post_notice", Unchecked(NOTICE_CHANNEL)),
    ("Transaction::take_notices", Unchecked(NOTICE_CHANNEL)),
    (
        "Transaction::take_html_reports",
        Unchecked(
            "transfers attempt-local sanitizer diagnostics containing only field identities and removal metadata, never stored record contents; no access grant or mutation",
        ),
    ),
    (
        "Transaction::require_strict_html",
        Unchecked(
            "only increases HTML rejection for this transaction attempt; cannot relax policy or authorize a write",
        ),
    ),
    (
        "Transaction::set_company_check_chunk_for_tests",
        Unchecked(TX_CONTROL),
    ),
    (
        "Transaction::reset_default_get_count",
        Unchecked(TX_CONTROL),
    ),
    (
        "Transaction::set_copy_limits_for_tests",
        Unchecked(TX_CONTROL),
    ),
    ("Transaction::set_precision", Unchecked(TX_CONTROL)),
    ("Transaction::set_cursor_limit", Unchecked(TX_CONTROL)),
    (
        "Transaction::begin_base_install",
        Unchecked(
            "install-state flag of the transaction object (no record read or written); turning it on needs the Orm's one-time grant and a transaction opened as System",
        ),
    ),
    (
        "Transaction::end_base_install",
        Unchecked("install-state flag of the transaction object (no record read or written)"),
    ),
    (
        "Transaction::base_install_scope",
        Unchecked(
            "scoped base-install RAII guard on the transaction object (no record read or written); requires Orm's one-time grant and a transaction opened as System",
        ),
    ),
    (
        "Transaction::taint_connection",
        Unchecked(
            "marks the transaction's connection to be closed instead of pooled (no record read or written)",
        ),
    ),
    (
        "Transaction::taint_cancelled_for_tests",
        Unchecked(TX_CONTROL),
    ),
    ("Transaction::poison_for_tests", Unchecked(TX_CONTROL)),
    (
        "Transaction::inject_commit_fault",
        Unchecked("test fault injection (feature test-faults)"),
    ),
    (
        "Transaction::flight_for_tests",
        Unchecked("test hook for the pipelined-flight error rules (feature test-faults)"),
    ),
    (
        "Transaction::query_each_for_tests",
        Unchecked("test hook for the streamed-statement error rules (feature test-faults)"),
    ),
    (
        "Transaction::refuse_next_row_for_tests",
        Unchecked("test hook making the next fetched row read as malformed (feature test-faults)"),
    ),
    (
        "Transaction::isolated_for_tests",
        Unchecked(
            "test hook running work in an isolated scope (feature test-faults); work's own calls are rows here",
        ),
    ),
    (
        "Transaction::raw_row_identity_for_tests",
        Unchecked(
            "test hook comparing raw-row and driver-row reads of one statement (feature test-faults)",
        ),
    ),
    ("Transaction::flush", Unchecked(TX_CONTROL)),
    (
        "Transaction::raw_statements_only",
        Unchecked(
            "borrowing scope that only restricts record operations; no record data read or written",
        ),
    ),
    (
        "Transaction::hook_scope",
        Unchecked(
            "borrowing scope that refuses transaction-ending calls and transaction-control SQL inside a hook chain; reads and writes no record",
        ),
    ),
    (
        "Transaction::pure_methods",
        Unchecked(
            "borrowing scope that refuses SQL, record, control and environment effects except the named metadata method chains; reads and writes no record itself (login selection hooks)",
        ),
    ),
    ("Transaction::flush_fields", Unchecked(TX_CONTROL)),
    ("Transaction::flush_model", Unchecked(TX_CONTROL)),
    ("Transaction::invalidate_all", Unchecked(TX_CONTROL)),
    ("Transaction::invalidate_model", Unchecked(TX_CONTROL)),
    ("Transaction::invalidate_records", Unchecked(TX_CONTROL)),
    (
        "Records::modified_external",
        Unchecked(
            "trusted native notification after caller-owned SQL: no authorization by design, no source write or right granted; registry/provenance admission and unchanged-relation dependency traversal only",
        ),
    ),
    (
        "Transaction::protecting",
        Unchecked(
            "protects fields from recomputation while `work` runs; `work`'s own calls are rows here",
        ),
    ),
    (
        "Transaction::mark_policy_dirty",
        Unchecked(
            "marks the policy and memberships for rebuild and re-resolution after core SQL writes (DN §3.4): engine state, over-invalidation only",
        ),
    ),
    (
        "Transaction::mark_scoped_id",
        Unchecked(
            "savepoint-scoped id set keyed by a caller-private marker type (a later release): records an id in memory; no records read or written",
        ),
    ),
    ("Transaction::ormcache", Unchecked(ORMCACHE)),
    ("Transaction::ormcache_get", Unchecked(ORMCACHE)),
    ("Transaction::ormcache_add", Unchecked(ORMCACHE)),
    ("Transaction::ormcache_arc", Unchecked(ORMCACHE)),
    ("Transaction::invalidate_ormcache", Unchecked(ORMCACHE)),
    ("Transaction::install_signal", Unchecked(SIGNAL)),
    (
        "Transaction::open_installing_signal",
        Unchecked(MODULE_OPERATION),
    ),
    ("Transaction::switch_orm", Unchecked(MODULE_OPERATION)),
    ("Transaction::manage_operation", Unchecked(MODULE_OPERATION)),
    ("Transaction::operation_commit", Unchecked(MODULE_OPERATION)),
    (
        "Transaction::operation_commit_classified",
        Unchecked(MODULE_OPERATION),
    ),
    (
        "Transaction::operation_rollback",
        Unchecked(MODULE_OPERATION),
    ),
    (
        "Transaction::operation_switch_orm",
        Unchecked(MODULE_OPERATION),
    ),
    (
        "Transaction::operation_bind_actor",
        Unchecked(MODULE_OPERATION),
    ),
    ("model_hooks::call_init", Unchecked(MODULE_OPERATION)),
    (
        "model_hooks::call_register_hook",
        Unchecked(MODULE_OPERATION),
    ),
    (
        "Transaction::call_selection_callback",
        Unchecked(MODULE_OPERATION),
    ),
    (
        "Transaction::call_selection_callback_in_company",
        Unchecked(MODULE_OPERATION),
    ),
    (
        "Transaction::grant_signal_request_role",
        Unchecked(ROLE_GRANT),
    ),
    ("Transaction::grant_node_role", Unchecked(ROLE_GRANT)),
    (
        "Transaction::install_event_schema",
        Unchecked(
            "operator-only event schema/catalog DDL, no business model record access; requires the fully elevated System principal before advisory lock or SQL (test: system_only_entry_points.rs, install_event_schema_refuses_a_user_before_sql); System installation shape/idempotence covered by event_schema.rs",
        ),
    ),
    (
        "Transaction::ensure_event_partitions",
        Unchecked(
            "operator-only event catalog admission and partition DDL, no business model record access; requires the fully elevated System principal before advisory lock or SQL (test: system_only_entry_points.rs, ensure_event_partitions_refuses_a_user_before_sql); System upkeep covered by event_schema.rs, upkeep_recreates_a_missing_partition",
        ),
    ),
    (
        "Transaction::install_audit_schema",
        Unchecked(
            "operator-only audit table DDL, no business model record access; requires the fully elevated System principal before advisory lock or SQL (test: system_only_entry_points.rs, install_audit_schema_refuses_a_user_before_sql); installation shape, window and forged-shape refusal covered by audit_store.rs",
        ),
    ),
    (
        "Transaction::install_audit_chain_schema",
        Unchecked(
            "owner-only closed chain DDL; System/full admission before SQL is covered by audit_chain_schema.rs, chain_installer_requires_system_before_sql",
        ),
    ),
    (
        "Transaction::initialize_audit_chain",
        Unchecked(
            "owner-only explicit epoch initialization; System/full admission before SQL is covered by audit_chain_epochs.rs, epoch_operations_require_system_before_sql",
        ),
    ),
    (
        "Transaction::seal_audit_batch",
        Unchecked(
            "owner-only safe-prefix sealing; System/full before SQL, real owner isolation and rollback are covered by audit_sealer.rs and audit_sealer_concurrency.rs",
        ),
    ),
    (
        "Transaction::verify_audit_chain",
        Unchecked(
            "operator-only read of the audit chain, its seals and anchors, no business model record access and nothing written; System/full admission before SQL is covered by audit_verify.rs, admission_and_isolation_are_explicit",
        ),
    ),
    (
        "Transaction::new_audit_chain_epoch",
        Unchecked(
            "owner-only linked epoch transition; System/full admission before SQL is covered by audit_chain_epochs.rs, epoch_operations_require_system_before_sql",
        ),
    ),
    (
        "Transaction::ensure_audit_partitions",
        Unchecked(
            "operator-only audit partition DDL, no business model record access; requires the fully elevated System principal before advisory lock or SQL (test: system_only_entry_points.rs, ensure_audit_partitions_refuses_a_user_before_sql); upkeep covered by audit_store.rs, install_makes_thirty_two_partitions_and_is_idempotent",
        ),
    ),
    (
        "Transaction::audit_attachment_claim",
        Unchecked(
            "doc-hidden owner-only claim audit: validates the phase, then before setting the CLI origin reads two booleans (whether the session role owns ir_blob_owner_claim, and whether the claim id exists) and refuses with Access or Programming, leaving the origin unchanged; reads no business model record and writes only the caller's own audit buffer (tests: base_attachment_recovery.rs, claim_audit_refuses_a_request_role and claim_audit_refuses_an_unknown_claim_without_relabelling)",
        ),
    ),
    ("Transaction::audit_auth", Unchecked(AUDIT_RECORD)),
    ("Transaction::audit_call", Unchecked(AUDIT_RECORD)),
    ("Transaction::audit_origin", Unchecked(AUDIT_RECORD)),
    (
        "Transaction::migrate_company_dependent",
        Unchecked(
            "native pre-migration helper: refuses every transaction that is not the superuser's (Access, before the first statement or lock) and then converts one declared company-dependent scalar column under table and schema locks; it reads and writes no record through the ORM (test: company_dependent_schema_migration.rs, limits_and_authority_refuse_without_changing_scalar_data)",
        ),
    ),
    ("Transaction::selection_labels", Unchecked(SELECTION_LABELS)),
    (
        "Transaction::selection_labels_for_lang",
        Unchecked(SELECTION_LABELS),
    ),
    (
        "Transaction::admit_html_translation",
        Unchecked(
            "runs the HTML sanitizer over one translation text for the translation importer (erp-data i18n save), which has already been authorized by its own caller; reads and writes no record, changes no cache and no storage, and returns the admitted text (tests: translation_save.rs,)",
        ),
    ),
    (
        "Transaction::grant_audit_reader_role",
        Unchecked(
            "operator-only privilege publication on the audit event objects, reads and writes no record of a model; refused to every principal but System with full elevation before any statement (test: system_only_entry_points.rs, grant_audit_reader_role_refuses_a_user_transaction); publication behaviour in audit_reader_roles.rs",
        ),
    ),
    ("AuditReader::search", Unchecked(AUDIT_READER)),
    ("AuditReader::export", Unchecked(AUDIT_READER)),
    ("AuditReader::read", Unchecked(AUDIT_READER)),
    ("Transaction::take_audit_attempt", Unchecked(AUDIT_RECORD)),
    (
        "Transaction::audited_superuser_env",
        Unchecked(AUDIT_ELEVATION),
    ),
    (
        "Transaction::audited_unscoped_sudo",
        Unchecked(AUDIT_ELEVATION),
    ),
    (
        "Transaction::elevated",
        Unchecked(
            "opens a scoped elevation block and records the elevation before the body; reads and writes no record itself: the body's record operations are rows of this table and pass the engine with the block's environment, which the compiled policy decides by the scope; refused before the body in a read-only transaction without a read-audit owner, for an invalid scope, a lifted rule the policy refuses or a caller outside its block (tests: principal_block.rs, principal_policy.rs)",
        ),
    ),
    (
        "Transaction::set_next_block_seq_for_tests",
        Unchecked("test hook setting the next elevation block number (feature test-faults)"),
    ),
    (
        "Transaction::exhaust_tx_serial_for_tests",
        Unchecked(
            "test hook making this transaction's next serial come from an exhausted counter (feature test-faults)",
        ),
    ),
    (
        "Transaction::grant_event_runtime_role",
        Unchecked(
            "operator-only event REVOKE/GRANT DDL, no business model record access; check_grantee first requires the fully elevated System principal before SQL (test: system_only_entry_points.rs, grant_event_runtime_role_refuses_a_user_before_sql), then validates the grantee (test: event_grants.rs, grant_refuses_the_owner_public_and_a_missing_table)",
        ),
    ),
    (
        "Transaction::grant_node_feature_roles",
        Unchecked(ROLE_GRANT),
    ),
    ("Transaction::grant_vacuum_role", Unchecked(ROLE_GRANT)),
    (
        "FeatureBinding::snapshot_in",
        Unchecked("bounded membership metadata through isolated helper; no business data"),
    ),
    (
        "FeatureBinding::protect_in",
        Unchecked("validated binding provenance plus serving check; caller owns transaction guard"),
    ),
    (
        "Transaction::revoke_vacuum_request_role",
        Unchecked(ROLE_GRANT),
    ),
    ("Transaction::notify_once", Unchecked(SIGNAL)),
    ("Transaction::signal_bump", Unchecked(SIGNAL)),
    ("Transaction::signal_compact", Unchecked(SIGNAL)),
    ("Transaction::signal_generations", Unchecked(SIGNAL)),
    ("Transaction::init_models", Unchecked(SCHEMA)),
    ("Transaction::init_models_with", Unchecked(SCHEMA)),
    ("Transaction::init_sql_objects", Unchecked(SCHEMA)),
    (
        "Transaction::init_sql_objects_subset",
        Unchecked(
            "the subset form of init_sql_objects for an addon schema step with no model table, called by the module executor (erp-modules system.rs) on its install transaction; DDL of declared SQL objects, no record of a model is read for a caller",
        ),
    ),
    (
        "Transaction::refuse_deferred_sql_objects",
        Unchecked(
            "end-of-lifecycle check of the module executor: reads the registry, the SQL-object provenance table and the PostgreSQL catalog and refuses absent or stale objects and unsafe grants; installs nothing and reads no record of a model",
        ),
    ),
    (
        "Transaction::verify_sql_object_publication",
        Unchecked(
            "read-only check of the installed SQL-object graph and its effective privileges against the registry, called at server start-up and node admission (erp-cli human.rs); takes the exclusive schema lock for its transaction (erp-orm schema_sqlobj.rs) and reads the provenance table and the PostgreSQL catalog, no record of a model",
        ),
    ),
    (
        "Transaction::reconcile_sql_object_grants",
        Unchecked(
            "brings the GRANT/REVOKE of declared SQL objects to their declaration when runtime grants are published (erp-cli human.rs grant_runtime_with_registry, reached from erp db init, erp db publish-grants, the browser module lifecycle and the CLI module operations in addon_life.rs); server start-up only verifies; DDL, not a record operation; never creates or replaces an object and refuses unsafe grants",
        ),
    ),
    (
        "Transaction::operation_reopen_locked",
        Unchecked(
            "lock-first reopening of the guarded uninstall's preparation, called only by the module executor's SQL uninstall guard (erp-modules system/sql_uninstall.rs) on its own operation transaction; refuses a foreign operation control, any principal but System with full elevation, a hook scope and a transaction that already wrote (test: sql_objects_uninstall.rs lock_first_opening_refuses_foreign_control_and_prior_mutations); takes advisory and table locks, reads and writes no record of a model",
        ),
    ),
    (
        "Transaction::plan_sql_object_removal",
        Unchecked(
            "plans the RESTRICT drops of the SQL objects an uninstall removes, called only by the module executor's uninstall (erp-modules system.rs) under its operation control; refuses a foreign control, an unmanaged transaction and any principal but System with full elevation; reads the registry, the SQL-object provenance table and the PostgreSQL catalog, runs no DDL and reads no record of a model",
        ),
    ),
    (
        "Transaction::apply_sql_object_removal",
        Unchecked(
            "runs a removal plan in the transaction that made it (refused in another one, test: sql_objects_uninstall.rs removal_plan_of_another_transaction_is_refused), called only by the module executor's uninstall (erp-modules system.rs) under its operation control, with the same control and principal refusals as the plan; DDL (DROP ... RESTRICT) and provenance rows, no record of a model",
        ),
    ),
    ("reflection::reflect_models", Unchecked(SCHEMA)),
    ("core_ddl::run", Unchecked(SCHEMA)),
    ("core_ddl::run_deferred", Unchecked(SCHEMA)),
    ("core_ddl::pending", Unchecked(SCHEMA)),
    (
        "defaults::add_missing_defaults_batch",
        Unchecked(
            "benchmark export (__bench) of the create path's defaults step; the create that runs it is checked",
        ),
    ),
    // ir.default and sequences
    (
        "ir_default::get_model_defaults",
        Unchecked(
            "raw SQL as the reference ERP's _get_model_defaults (ir_default.py:174-195, execute_query)",
        ),
    ),
    (
        "ir_default::discard_records",
        Unchecked(
            "called by unlink after the records' own check; the reference ERP runs it as superuser (models.py:3691 ir.default sudo(), :3788)",
        ),
    ),
    (
        "ir_default::get",
        Gap(
            "raw SQL; the reference ERP's _get searches ir.default with the caller's rules (ir_default.py:140-172); owner of ir.default (a later release / base)",
        ),
    ),
    (
        "ir_default::set",
        Gap(
            "raw SQL with hand-written ir.access.csv:34-36 checks (group_user own rows, group_system all, field write access; a later release); addon ir.access rows and record rules on ir.default are not seen; moves to the create/write path when ir.default becomes a model (a later release)",
        ),
    ),
    (
        "ir_default::discard_values",
        Gap(
            "raw SQL with hand-written checks (group_user may delete only own rows, else Access, as unlink does; a later release); addon ir.access rows and record rules are not seen; moves to search().unlink() when ir.default becomes a model (a later release)",
        ),
    ),
    (
        "sequence::next_by_code",
        Gap(
            "raw SQL on ir_sequence; the reference ERP checks ir.sequence read access and searches with the caller's rules first (ir_sequence.py:301-303); owner of ir.sequence (base)",
        ),
    ),
    (
        "sequence::next_by_code_with_clock",
        Gap(
            "raw SQL on ir_sequence; the reference ERP checks ir.sequence read access and searches with the caller's rules first (ir_sequence.py:301-303); owner of ir.sequence (base)",
        ),
    ),
    (
        "sequence::next_by_code_with_wait_mode",
        Gap(
            "raw SQL on ir_sequence; the reference ERP checks ir.sequence read access and searches with the caller's rules first (ir_sequence.py:301-303); owner of ir.sequence (base)",
        ),
    ),
    (
        "Transaction::visible_under_company_restriction",
        Unchecked(
            "a probe for core services that decide for a human in a transaction running under another policy (the administrator password reset): returns which of the given ids the named principal sees under the model's generated company restriction; it only narrows, grants nothing and writes nothing, refuses an elevated or System principal and a company outside the principal's companies; not an addon API",
        ),
    ),
    (
        "delta::apply_synchronous",
        Unchecked(
            "writes one adapter-owned projection table (not a model: no ordinary model or RPC access, no record rule) from a netted batch the consumer built inside its own checked source write; returns only the applied measures to that consumer; the access proof of each consumer table is its adapter's reviewed design",
        ),
    ),
    (
        "sequence::next_by_id",
        Gap(
            "raw SQL on ir_sequence; the reference ERP checks ir.sequence read access first (ir_sequence.py:291); owner of ir.sequence (base)",
        ),
    ),
    (
        "sequence::next_by_id_with_wait_mode",
        Gap(
            "raw SQL on ir_sequence; the reference ERP checks ir.sequence read access first (ir_sequence.py:291); owner of ir.sequence (base)",
        ),
    ),
];

/// The audit trail's recording entry points: they touch no model record and read none;
/// they only buffer ids, enums and counts in the caller's own transaction attempt, written with its
/// audit row at commit.
const AUDIT_READER: &str = "reads audit entries from the restricted audit store, authorized by its own checks and not by the probe model: the original authenticated caller's `audit.read` capability, read access to the entry model and its fields, then the caller's current access to every audited target row before the row is returned; a System transaction has no reader and is refused (tests: audit_reader_access.rs, audit_reader_projection_uses_real_current_access_for_every_surface and audit_reader_revocation_company_scope_and_unavailable_reader_fail_closed)";

const SELECTION_LABELS: &str = "returns the (value, label) pairs the registry declares for one static selection field, translated through ir.model.fields.selection with a raw statement, whoever asks: it is the `selection` list `fields_get` shows every user; no record of a business model is read (test: fields_scalar_selection_i18n.rs, spec_field_105_static_selection_labels_follow_context_language)";

const AUDIT_RECORD: &str = "records into the caller's own transaction buffer only (typed ids, enums and counts, no record data read or written); refuses before any statement when the audit store is not ready or the transaction cannot write (tests: audit_store.rs, a_store_that_is_not_ready_refuses_before_any_statement and a_read_only_transaction_refuses_lifecycle_events)";

/// The two audited elevation entry points: `Env::sudo(true)` is already public to core code, these
/// add the recording the audit trail needs before the elevated environment exists.
const AUDIT_ELEVATION: &str = "returns the given environment in superuser mode, as the public `Env::sudo(true)` does, after recording the elevation (reason, cause, original attribution) for the transaction's audit row; an environment of a scoped elevation block is refused before anything is recorded; the recording refuses first when the store is not ready or the transaction is read-only (tests: server_actions.rs flagged_action_runs_as_superuser_and_is_audited; audit_store.rs unscoped_sudo_is_audited_per_call)";

/// Every `sudo(true)` call site outside tests (`<crate>/<file>::<function>`, how many): why it may
/// leave the engine. replaces them with scoped elevation blocks where it can.
const SUDO_SITES: &[(&str, usize, &str)] = &[
    (
        "account/fiscal/settings.rs::read",
        1,
        "reads the fiscal calendar settings (year end, horizon) of a root company for period creation; the framework reads its own configuration, and a user whose entry needs a period needs no settings group; the values are bounded and only named in refusals (a later release)",
    ),
    (
        "account/journal/mod.rs::check_accounts_as_default",
        1,
        "the account side of the default-account type rule: finds the journals, of any company and archived ones included, that use the accounts being written as default account; a shared account is used by journals the writer's company rule hides, and the constraint must not depend on who triggers it; only the journal type and the account id are read (099)",
    ),
    (
        "account/journal/mod.rs::check_default_account_types",
        1,
        "reads only `account_type` of the default accounts of the journals or accounts being checked, so the type rule does not depend on which accounts the caller's company rule shows; the type is only named in refusals, and never for an account the caller may not read (099)",
    ),
    (
        "account/journal/mod.rs::next_code",
        1,
        "proposes a free journal code: reads the codes of the company's and its parent companies' journals, archived and rule-hidden ones included, because `UNIQUE (company_id, code)` counts the company's own and the reference skips the parents' codes too; only for a caller who passed the create check of the model and may create journals in that company; only the codes are read, and only the chosen free code reaches the caller",
    ),
    (
        "account/journal/settings.rs::read",
        1,
        "reads the global journal settings (code length, proposed prefixes, allowed account types) for the journal checks; the framework reads its own configuration, and creating a journal needs no settings group; the values are bounded and only named in refusals (098, 099)",
    ),
    (
        "account/ledger/account.rs::_compute_root_id",
        1,
        "the compute of an account's root node reads `account.root` by the names derived from the codes of the accounts being computed; root nodes are derived rows without a company, and the stored value must not depend on which reader triggered the compute (a later release)",
    ),
    (
        "account/ledger/account.rs::store_code",
        1,
        "the inverse of `code` and `create` store the code row of (account, root company) after the ORM checked the caller's create or write right on the account; the row belongs to the root company of the tree, which a branch user's company rule on `account.code.mapping` would hide",
    ),
    (
        "account/ledger/code.rs::codes_by_account",
        1,
        "reads the code rows of the given accounts in the root company for the computes of `code` and `root_id` and for the code checks; the callers pass accounts their environment already holds, and the rows belong to the root company of the tree, outside a branch user's company rule (029)",
    ),
    (
        "account/ledger/root.rs::ensure_roots",
        1,
        "creates the missing `account.root` nodes for the prefixes of a code that is being stored; called from the create and write of a code row after their own access checks; root nodes are derived rows nobody creates by hand",
    ),
    (
        "account/numbering/mod.rs::from_settings",
        1,
        "reads the global lock-wait setting of journal entry numbering; the framework reads its own configuration, and a user who posts needs no settings group; the value is a bounded number and is not returned to the caller (a later release)",
    ),
    (
        "erp-orm/audit/persist.rs::audited_superuser_env",
        1,
        "the environment of a flagged server action's run, returned only after the elevation was recorded for the audit row; the caller's own right to run the action was checked first by base",
    ),
    (
        "erp-orm/audit/persist.rs::audited_unscoped_sudo",
        1,
        "the audited form of the unscoped escape hatch: records the elevation per call, then returns superuser mode",
    ),
    (
        "erp-security/auth/login.rs::check_credentials",
        1,
        "dispatches `res.users._check_credentials` as the user logging in, in superuser mode, before any session exists (the reference ERP `res_users.py:866-867`)",
    ),
    (
        "base/ir_cron/kind.rs::settle",
        1,
        "the settle chunk of a cron run reaches the admin-notification hook as the system so the warning does not depend on the cron user's rights; it passes only the fixed warning text and reads no record (a later release)",
    ),
    (
        "base/ir_cron/mod.rs::method_direct_trigger",
        1,
        "after the caller's write access on the scheduled action is checked, the trigger rows and the job row are written as the system: the caller may press the button without access to ir.cron.trigger (a later release)",
    ),
    (
        "base/ir_config_parameter.rs::init",
        1,
        "creates the missing default parameters, `database.secret` among them, when base is installed or updated (the reference ERP's `init` writes them as the superuser); `database.secret` is visible only with full elevation",
    ),
    (
        "base/ir_config_parameter.rs::create_default_rows",
        1,
        "writes the default row of each declared technical parameter that has none when the registry is published; keys only, never a caller value",
    ),
    (
        "base/ir_config_parameter.rs::erp_replace_secret",
        1,
        "after the caller's write and create access is checked and the key is a non-identity secret, the secret row is replaced as the system without reading its value back",
    ),
    (
        "base/decimal_precision.rs::precision_defined",
        1,
        "reads the digits of one named decimal precision, whoever asks: the table is administrator-only but a usage name and its digit count are what `fields_get` shows every user, and the reference ERP's `precision_get` reads it with a raw statement (`decimal_precision.py:24-28`); one bounded name in, one integer out",
    ),
    (
        "base/attachments/service.rs::insert",
        1,
        "creates the ir.attachment rows after the explicit parent check of the caller (`access::check`, write access to every parent record and field) and after linking the content to the caller's own bound uploads; the ORM create runs elevated only so the model's overrides accept the server-set columns, inside a private service scope no other crate can open",
    ),
    (
        "base/attachments/service.rs::write_content",
        1,
        "rewrites file_size, checksum and mimetype of attachments the caller was just shown writable by the explicit parent check, with the content linked from the caller's own upload; callers cannot write these columns themselves",
    ),
    (
        "base/attachments/service.rs::adopt",
        1,
        "moves the composer files of the caller (placeholder model, record id 0, created by the caller, not field-bound) onto the record a post is made on, in one set-based write, after the caller of the function checked the right to post on that record (the reference ERP does the same in sudo, `mail_thread.py:2468-2495`); every other id, a missing one included, refuses the whole call with one message before anything moves, and the write runs inside the private service scope",
    ),
    (
        "base/dated_parameter.rs::resolve_many",
        1,
        "reads the dated parameters of the caller's company, its ancestors and the shared scope after the caller's model-level read right and company check, so a record rule that filters rows cannot put a farther value in place of the nearer one (the order of `RateBook::load`); keys and companies are bounded and the answer is typed values only",
    ),
    (
        "base/res_users_settings.rs::own_row",
        1,
        "finds the caller's own `res.users.settings` row by `tx.env().uid()`, never by an argument: the row is selected by that user id alone, so a portal user outside `base.group_user` (who has no `ir.access` row on the model; the public user, whom every anonymous visitor shares, is refused by the caller before this) still reads the settings that are theirs; generic `read`/`search` stay under the `ir.access` rows",
    ),
    (
        "base/res_users_settings.rs::set_res_users_settings",
        1,
        "after refusing the public user and unknown keys and taking the per-user advisory lock, creates the caller's own settings row with `user_id` set from `tx.env().uid()`: `user_id` is writable by `base.group_system` only, so that a user cannot hand a row to another user through generic `write`/`create`, and the method sets it as superuser for the caller alone",
    ),
    (
        "base/res_users_preferences.rs::action_get",
        1,
        "reads the one preferences window action `base.action_res_users_my` as superuser, as the reference ERP's `action_get` does (`res_users.py:829-830`): every internal user opens My Preferences, the action record carries no data of any user, and the client still loads the described form and the user's own record under the caller's rights",
    ),
    (
        "base/res_users_preferences.rs::check_codes",
        1,
        "asks whether a language code names an active `res.lang` row when a user's `lang` is written, whoever writes: the answer is yes or no for one code, and a user without read access on `res.lang` must still get the same refusal as one with it",
    ),
    (
        "base/res_users_preferences.rs::check_stored_preferences",
        1,
        "reads back only the `lang` and `tz` codes of the users this `create` just stored, after the ORM filled in the context defaults, so they are validated the same way whatever the creator may read; the values feed only the yes-or-no code check, whose refusal names only the caller's own submitted or default code",
    ),
    (
        "base/ir_ui_menu.rs::visibility_debug",
        1,
        "reads only the current subject's computed role to narrow technical-menu debug eligibility; the result grants no model, field, action or other subject access",
    ),
    (
        "base/ir_ui_menu.rs::visible_menu_ids",
        5,
        "reads candidate menu metadata and only group/target metadata of caller-readable window/server actions as superuser; visibility still uses caller group membership and model Read, never the filtered groups comodel",
    ),
    (
        "base/res_partner.rs::_compute_partner_share",
        1,
        "the stored compute of partner_share reads the partner's linked users (user_ids, their share and active flags, archived ones included) as superuser: the stored value must be the same whoever triggers the recompute, and a caller who may write a partner need not be able to read its users; the answer is one boolean per partner, no user data leaves the compute",
    ),
    (
        "base/res_partner.rs::parent_names",
        1,
        "reads only the name of the parent partner for the complete-name fallback (PARTNER section 4.3, as the reference ERP reads the parent name as the superuser); the answer is names only, and formatted display reads keep the ordinary compute environment",
    ),
    (
        "base/res_company.rs::link_to_creators",
        1,
        "links only newly created company IDs to the environment user and reserved superuser after the caller's company Create check, inside the company lifecycle guard/fence; no caller-supplied user ID enters the elevated write",
    ),
    (
        "base/res_users.rs::names_of",
        1,
        "formats company-invariant refusals with the offending user's name and names from that user's allowed-company relation; the invalid chosen company is not named; callers are name_of and check_user_companies",
    ),
    (
        "base/res_users.rs::check_user_types",
        1,
        "reads the name of an offending user for the user-type constraint error (the reference ERP reads it as the superuser inside the constraint)",
    ),
    (
        "base/res_users_auth.rs::renamed",
        1,
        "reads the logins a write renames, whatever the writer may read, so their sessions end with the rename (security review round 2 N3)",
    ),
    (
        "erp-data/load/menu.rs::action_names",
        1,
        "a <menuitem> reads the name of its action as the superuser, like env.ref(..).sudo() \
         (install path)",
    ),
    (
        "erp-data/load/mod.rs::eval_user_group",
        1,
        "has_group() on another user in a data expression (install path, trusted content) checks \
         the user exists as the superuser, so a missing user is reported even to a principal that \
         cannot read res.users; the group check itself runs as that user",
    ),
    (
        "erp-data/model_data.rs::check_module_prefixes",
        1,
        "data loading (install path, System) reads ir.model.data",
    ),
    (
        "erp-data/load/batch.rs::existing_records",
        1,
        "data loading (install path, System) checks that the records named by external ids exist",
    ),
    (
        "erp-data/load/process_end.rs::delete_obsolete",
        1,
        "the end-of-run cleanup of an install or upgrade (System) deletes the records of obsolete \
         external ids",
    ),
    (
        "erp-data/model_data.rs::load_records_with",
        1,
        "data loading (install path, System) resolves external ids",
    ),
    (
        "erp-modules/system.rs::record_demo_failure",
        1,
        "the module system records a failed demo in ir.demo_failure (install path, System)",
    ),
    (
        "erp-orm/records.rs::engine_sudo",
        1,
        "the recordset form of `Env::engine_sudo`, crate-private: superuser mode for the engine's own steps listed in this table, keeping an elevation block's lifetime; public `sudo(true)` on a block's records refuses every use instead",
    ),
    (
        "erp-orm/related_company.rs::superuser_records",
        1,
        "a company-bound related read reads the company tree (parent paths), the sequence and names of companies, and the name of the suggested company as superuser, only to decide and word a refusal; the reader's own companies come from its validated environment",
    ),
    (
        "erp-orm/access/records.rs::evidence_display_names",
        1,
        "the access-error evidence lists the display names of the denied records as superuser, like the reference ERP's debug listing (records[:6].sudo().display_name); only on the error path",
    ),
    (
        "erp-orm/commands.rs::o2m_owners",
        1,
        "x2many commands: reads the current owners of one2many lines to apply the command; the command itself is checked on the comodel",
    ),
    (
        "erp-orm/commands.rs::patch_relation_caches",
        2,
        "cache bookkeeping after a relational write (no value returned to the caller)",
    ),
    (
        "erp-orm/commands.rs::write_m2m",
        1,
        "many2many relation rows after the record checks of the command",
    ),
    (
        "erp-orm/company/mod.rs::check_company",
        1,
        "_check_company reads company ids as superuser, as the reference ERP (models.py _check_company: sudo); \
         an integrity rule that returns nothing to the caller",
    ),
    (
        "erp-orm/engine.rs::apply_outputs",
        2,
        "old many2one inverse bookkeeping loads under sudo without returning those values; monetary output normalization uses sudo only for declared compute_sudo groups, while explicit inputs retain caller authorization",
    ),
    (
        "erp-orm/engine.rs::compute_batch",
        1,
        "compute_sudo fields compute as superuser (the reference ERP compute_sudo)",
    ),
    (
        "erp-orm/engine.rs::run_constraints",
        1,
        "constraints run as superuser, as the reference ERP (_validate_fields under sudo)",
    ),
    (
        "erp-orm/engine.rs::settle_reaching",
        1,
        "recompute triggers find dependent records as superuser (the reference ERP modified() under sudo)",
    ),
    (
        "erp-orm/engine.rs::without_foreign_siblings",
        1,
        "a company-bound related field is read ahead as superuser for permissions, like its compute; the company bound of the reader still applies",
    ),
    (
        "erp-orm/hierarchy.rs::expand",
        1,
        "child_of/parent_of expansion as the reference ERP (_search under sudo for the hierarchy)",
    ),
    (
        "erp-orm/hierarchy.rs::resolve_condition",
        1,
        "child_of/parent_of resolution as the reference ERP",
    ),
    (
        "erp-orm/engine.rs::currency_bound_cells",
        3,
        "currency identity column access is privileged, while related aliases honor declared compute_sudo and native pending/nonstored computations retain their execution environment",
    ),
    (
        "erp-orm/engine.rs::related_from_mode",
        1,
        "currency-mode final company-target metadata reads are privileged; Bound retains the original subject and ordinary mode preserves its existing environment",
    ),
    (
        "erp-orm/engine.rs::related_walk",
        1,
        "currency-mode hop company metadata reads are privileged; Bound retains original companies and ordinary mode preserves its existing environment",
    ),
    (
        "erp-orm/monetary.rs::resolve",
        2,
        "monetary currency identity field/fallback/inherited/related reads are privileged; native compute retains declared compute_sudo and related hops retain the original reader's company bound",
    ),
    (
        "erp-orm/monetary.rs::normalize",
        1,
        "currency rounding provider reads are privileged only inside normalization; caller model/record/field write admission and amount publication remain unchanged",
    ),
    (
        "erp-orm/newrec.rs::changed_vals",
        1,
        "onchange: origin values of a new record compared as superuser",
    ),
    (
        "erp-orm/newrec.rs::ensure_new_x2many",
        1,
        "onchange: copies origin x2many lines into the new record",
    ),
    (
        "erp-orm/parent_store.rs::parent_store_move",
        1,
        "parent_path maintenance (the reference ERP _parent_store_update under sudo)",
    ),
    (
        "erp-orm/parent_store.rs::parent_store_moves",
        1,
        "parent_path maintenance",
    ),
    (
        "erp-orm/predicate.rs::access_in_memory",
        1,
        "in-memory access condition: reads targets as superuser, then filters them by the principal's access in SQL",
    ),
    (
        "erp-orm/predicate.rs::filter_records_inner",
        1,
        "filtered_domain reads bypass targets (any!) as superuser, as the reference ERP",
    ),
    (
        "erp-orm/predicate.rs::resolve_any",
        1,
        "filtered_domain: any! evaluates targets as superuser",
    ),
    (
        "erp-orm/read.rs::many2one_labels_inner",
        1,
        "display names of many2one values the caller may read (the reference ERP's read of display_name under sudo for _classic_read)",
    ),
    (
        "erp-orm/read_group/build.rs::many2many_join",
        1,
        "read_group many2many join (the reference ERP _read_group_groupby under sudo for the relation)",
    ),
    (
        "erp-orm/relational.rs::fetch_m2m",
        1,
        "relation rows of x2many reads, filtered by the comodel's access afterwards (SEC-014)",
    ),
    (
        "erp-orm/relational.rs::fetch_o2m",
        1,
        "one2many lines, filtered by the comodel's access afterwards (SEC-014)",
    ),
    (
        "erp-orm/relational.rs::filter_domain",
        1,
        "the field's own domain= on x2many values",
    ),
    (
        "erp-orm/relational.rs::x2many_visible",
        1,
        "x2many visibility filter (SEC-014)",
    ),
    (
        "erp-orm/relcrud.rs::create_values",
        1,
        "_inherits parents created for a child after the child's checks",
    ),
    (
        "erp-orm/relcrud.rs::write_values",
        2,
        "_inherits parents written for a child after the child's checks",
    ),
    (
        "erp-orm/rpc/core.rs::has_access",
        1,
        "RPC has_access answers for the caller (records checked as the caller)",
    ),
    (
        "erp-orm/search.rs::compile_predicate",
        1,
        "access predicates resolve their hierarchy start records as superuser (the reference ERP evaluates the predicate so, models.py:4907-4929)",
    ),
    (
        "erp-orm/search.rs::compile_predicate_at",
        1,
        "access predicates resolve their hierarchy start records as superuser (the reference ERP evaluates the predicate so, models.py:4907-4929)",
    ),
    (
        "erp-orm/search.rs::name_search_with",
        1,
        "display names of records the search already filtered for the caller",
    ),
    (
        "erp-orm/search.rs::prepare_sql_domain_resolved",
        1,
        "a comodel's read rule or an `access` condition's predicate inlined into the caller's domain resolves its hierarchy start records as superuser, as the search's own rule does (the reference ERP evaluates the predicate so, models.py:4907-4929); the resolved rule still filters the caller's rows",
    ),
    (
        "erp-orm/search.rs::read_restriction_at",
        1,
        "access predicates resolve their hierarchy start records as superuser (the reference ERP evaluates the predicate so, models.py:4907-4929)",
    ),
    (
        "erp-orm/search.rs::search_access",
        1,
        "access predicates resolve their hierarchy start records as superuser (the reference ERP evaluates the predicate so, models.py:4907-4929)",
    ),
    (
        "erp-orm/trigger.rs::traversal_env",
        1,
        "recompute triggers traverse as superuser (the reference ERP modified())",
    ),
    (
        "erp-orm/unlink.rs::deletion_dependents",
        1,
        "ondelete cascade/set null bookkeeping after the unlink check",
    ),
    (
        "erp-orm/web/groups.rs::display_names",
        1,
        "group labels of records the grouping already filtered for the caller",
    ),
    (
        "erp-orm/web/read.rs::expand_many2one",
        1,
        "web_read sub-records under the read check of their parent fields",
    ),
    (
        "erp-orm/write.rs::precompute_rows",
        1,
        "precomputed field groups honor declared compute_sudo, including their monetary output normalization; supplied monetary inputs keep caller authorization",
    ),
    (
        "erp-orm/access/records.rs::visible_under_company_restriction",
        1,
        "the company-restriction probe of runs in superuser mode so that the restriction's own relational traversals bypass the comodels' rules, as in every record check; company_ids binds to the companies the caller names, which must be the principal's; only the visible ids come back",
    ),
    (
        "web/pivot_export.rs::limits",
        1,
        "the five declared bounds of the pivot download (global settings the framework reads for itself); a refusal names the bound and its value (a number an internal user may read in Settings anyway), nothing else of the settings leaves; the read check on the document's model runs with the caller's rights",
    ),
    (
        "web/metadata.rs::load_action",
        3,
        "group, model and view-mode columns of the window action the caller opens; the caller's groups and read access on res_model are checked before any value leaves",
    ),
    (
        "web/metadata.rs::load_action_views",
        1,
        "fixed search_view_id/limit presentation columns of the singleton action already admitted by load_action's record-read, action-group and target-model checks; the view descriptions keep the original caller environment and selected-view/field-group checks, while page limits remain validated",
    ),
    (
        "web/metadata.rs::dashboard_description",
        1,
        "the search_view_id and limit columns of the one window action the dashboard request names, read after load_action admitted it with the caller's record-read, action-group and target-model checks (the same columns load_action_views reads); the list and search descriptions are then built in the caller's environment, a search view the caller cannot read only hides the strip, and only the action's own search view id and its validated page limit leave, never a value of a business record",
    ),
    (
        "web/dashboard_settings.rs::read",
        1,
        "the three fixed dashboard settings web.dashboard_max_items, web.dashboard_max_period_days and web.dashboard_periods (global, nonsecret configuration the framework reads for itself, so a dashboard viewer needs no settings group); no business data is read in that environment; the item and day bounds are applied and named in refusals, and the configured period choices are returned to the caller as the strip's period list",
    ),
    (
        "web/metadata.rs::menu_modules",
        1,
        "oldest external-ID module and target ID for the already authorized/pruned menu forest only; set-based identity/size preflights precede bounded presentation reads; no external ID creation or authorization grant (/028)",
    ),
    (
        "web/metadata.rs::load_menus",
        4,
        "ir.ui.menu and ir.actions.act_window presentation rows, kept only when the caller is in the menu/action groups and may read the action's model; byte caps apply before rows are materialized",
    ),
    (
        "mail/mail_thread.rs::unlink",
        3,
        "after the thread records are deleted under the caller's own delete access, removes their `mail.message`, `mail.followers` and `mail.scheduled.message` rows (none has a foreign key to the thread, so nothing else cleans them); the ids searched are exactly the ones just deleted, and a refused delete leaves all three tables untouched",
    ),
    (
        "mail/mail_thread.rs::followers",
        1,
        "finds one keyset page of follower rows of a record the caller was already checked to read, then hands the rows back in the caller's own environment, so a superuser recordset never leaves the method",
    ),
    (
        "mail/mail_thread.rs::unfollow",
        1,
        "deletes the follower rows `subscriptions` found for the requested partners, after the caller's write access (or, for only their own partner, read access) was checked; the follower rows are not readable by every caller who may stop following",
    ),
    (
        "mail/mail_thread.rs::set_subtypes",
        1,
        "rewrites the subtype links of the one follower row found for the partner, after the caller's access to the record was checked (read for their own partner, write otherwise) and `Missing` when the partner does not follow",
    ),
    (
        "mail/mail_thread.rs::follow",
        1,
        "keeps only the active partners among those the caller asked to follow (a user who may write the record cannot necessarily read every partner); runs after the write access (or, for only their own partner, read access and the public-user check) was checked, and only ids that pass are handed on",
    ),
    (
        "mail/mail_thread.rs::check_subtypes",
        1,
        "learns only whether each requested subtype id exists with no model or the record's model, so a wrong id is a validation error and not a foreign-key failure; a portal caller cannot read internal subtypes, and nothing but the ids refused is reported back; callers have checked access to the record first",
    ),
    (
        "mail/mail_thread.rs::_inverse_message_partner_ids",
        1,
        "reads the record's current follower rows to compute which partners to add and remove; the field is writable by internal users only (checked first) and the changes go through `follow` and `unfollow` with their own access checks",
    ),
    (
        "mail/followers.rs::insert_followers",
        2,
        "private helper behind `follow` and `_insert_followers`, after the callers' access checks: reads `partner_share` of the partners to choose their default subtypes (a user following a partner cannot necessarily read it), and finds, creates and updates the follower rows, which are not readable or writable by every user who may follow",
    ),
    (
        "mail/post.rs::create_message",
        1,
        "creates the one `mail.message` row of `message_post` after the caller's `_mail_post_access` check on the record; `mail.message` is writable by system users only until message access lands, and every value comes from the validated request, the context statement and the recipient statement (model, record id, author and subtype are never taken from the caller unchecked)",
    ),
    (
        "mail/post.rs::subscribe_author",
        1,
        "creates the author's follower row after a `message_post` comment, once the caller's post access was checked and the author was found to be the caller's own active internal user's partner that does not follow yet; follower rows are not writable by every user who may post",
    ),
    (
        "mail/scheduled.rs::schedule",
        1,
        "creates the one `mail.scheduled.message` row of a scheduled message after the caller's post access on the record (`_mail_post_access`, as `message_post`), the thread-model, date, partner and bound checks; the table has an access row for system users only, and every value comes from the validated request and the context statement (author and creator are the caller, never taken from the request)",
    ),
    (
        "mail/scheduled.rs::edit",
        1,
        "writes the creator's changes to one scheduled message after the post access on its record was checked and the caller was found to be its creator (`create_uid`); the target (`model`, `res_id`) is not in the patch type and the model's `write` refuses it, and the generation and state fields are set only here",
    ),
    (
        "mail/scheduled.rs::cancel",
        1,
        "passes a superuser-mode environment to `Transaction::job_cancel_by_key` so the delivery job of the row goes with it whoever enqueued it (the cancel is allowed to any internal user who may write the record, which was checked first); the key is the one of the row just locked and deleted, the kind is fixed, and the call is reached only from the `cancel` method after that check, with the key of the row just deleted and a fixed kind",
    ),
    (
        "mail/scheduled.rs::cancel_for_records",
        1,
        "passes a superuser-mode environment to `Transaction::job_cancel_by_keys` so the delivery jobs of the scheduled messages of records being deleted go with them, whoever enqueued them; the keys are those of the rows found by model and record ids just before the `mail.thread` unlink deletes them under the caller's own delete access, the kind is fixed, and the call is reached only through the thread unlink cascade, with server-derived keys and a fixed kind",
    ),
    (
        "mail/scheduled.rs::send_due",
        1,
        "marks the one scheduled message of the job's payload as failed with a bounded reason; runs as the creator's job principal (checked equal to `create_uid` first), after the post attempt raised an access, validation, user or missing error",
    ),
    (
        "mail/scheduled.rs::post_draft",
        1,
        "hands the one scheduled message row to `_message_created_hook` as a privileged recordset after `message_post` succeeded (a user has no table access); the row id is the one just locked and posted, and the post itself ran under the caller's own access",
    ),
    (
        "mail/scheduled.rs::delete_draft",
        1,
        "deletes one scheduled message after a send or a cancel; every caller has checked the post access on its record (cancel, send now) or is the creator's own delivery job, and the row id came from the locked row just read",
    ),
    (
        "mail/post.rs::log_messages",
        1,
        "creates the internal notes of `_message_log_batch`, a private method (not RPC, not data-callable) whose callers check access to the records first, as the reference ERP's sudo log; bodies are escaped text, the subtype is `mail.mt_note` and nobody is notified",
    ),
    (
        "product/product.rs::write",
        1,
        "counts `product.combo.item` rows that use the variants being archived, so a variant in a combo the caller cannot read is still refused; only the count is used and the error names no combo",
    ),
    (
        "product/template.rs::write",
        1,
        "the template form of the combo check: counts combo items that use the template's variants before archiving, count only, no names in the error",
    ),
    (
        "product/attribute.rs::attribute_usage",
        1,
        "finds the attribute lines of active templates that use one attribute, in every company (an attribute is global), so a product the caller cannot read still blocks the archive, delete or variant-mode change; it runs after the caller's write or unlink check on the attribute, and the lines feed only an emptiness test and `visible_product_names`, which searches the templates again with the caller's own access and names a hidden one only as \"restricted products\"",
    ),
    (
        "product/attribute.rs::value_usage",
        1,
        "the value form of `attribute_usage`: elevated only when `refuse_value_use` asks for the global view (the `is_used_on_products` compute passes the caller's environment), after the caller's write or unlink check on the value; the lines feed only an emptiness test and the caller-access name search as above",
    ),
    (
        "product/attribute.rs::unlink",
        2,
        "after the caller's unlink check and `refuse_value_use`, finds the template values of the deleted value and the variants (archived too) that carry them, reading only `active`, to decide whether the value is archived (only archived variants use it) or deleted; a variant the caller cannot read still counts, and no value of either model leaves the method",
    ),
    (
        "product/attribute.rs::_compute_default_extra_price_changed",
        1,
        "searches the template values of the original attribute value on templates of the caller's allowed companies and of their parent companies (a `parent_of` domain on the list from `tx.companies`, taken before the elevated search), and on shared templates, and compares their `price_extra` with the new default; only the boolean leaves the search",
    ),
    (
        "product/packaging_actions.rs::action",
        1,
        "resolves the external identity `product.product_uom_action_view_list` and reads `res_model`, `name`, `target` and `views` of that one window action, refusing one that does not target `product.uom` (the reference ERP `_for_xml_id` reads the action with privileges); the three RPC callers first check read access on their one record and its existence, the domain names the checked record (and, for a unit, the context's `product_ids` as a filter only), and the opened list applies the caller's own access",
    ),
    (
        "product/packaging_lifecycle.rs::before_template_unit_write",
        1,
        "after the caller's write check on the templates whose `uom_id` changes, searches their variants (archived too) and calls the `_update_uom` hook on them with privileges and `skip_uom_conversion`, as the reference ERP's template write does (`product_template.py:839`), so documents of dependent addons follow the unit change; base product's hook changes nothing and the target unit was validated and registered first",
    ),
    (
        "uom/res_groups.rs::_is_uom_feature_enabled",
        1,
        "reads the `uom.group_uom` external identity, that group's implication closure and the superuser's group links to tell whether the global multi-unit feature is on (the reference ERP `_is_feature_enabled` reads the superuser's groups with privileges); a private method without `#[rpc]`, returning one boolean, that authorizes nothing for the caller",
    ),
    (
        "account/tax/settings.rs::limit",
        1,
        "reads the global setting `account.tax_repartition_lines_max` for the tax distribution checks; the framework reads its own configuration, and a user who creates or edits a tax needs no settings group; the value is a bounded number and only named in refusals",
    ),
    (
        "mail/thread_rpc.rs::read_followers",
        1,
        "reads the follower rows of the partners a `thread_follow` or `thread_set_subtypes` caller just named, for the answer's store payload: the caller's standing was checked (public refused, a portal caller limited to its own partner), the method it called checked access to the record, and the row ids come from the lookup of exactly those partners on that one record; a portal caller cannot read the follower table itself, so the rows of its own partner are read with privileges and nothing else is",
    ),
    (
        "mail/thread_load.rs::_to_store",
        1,
        "reads the columns of the messages a `thread` or `thread.messages` page lists, and the subtypes they name, for the store payload: the page's ids come from a search under the caller's own `mail.message` read rules and a thread the caller may read, those rules depend on the thread documents and cannot be remembered (a read as the caller would ask the database to decide the same rows again), and a message the caller sees never names a subtype the caller may not see; `_to_store` is a private method (not RPC, not data-callable) whose recordset is trusted by contract, and it stages no recipients; it also reads the files linked to those same messages (one statement over the page's message ids, the reference ERP reads a readable message's files with privileges, `mail_message.py:1201-1208`), bounded by the SQL to files whose parent is the message's own record and that are not field-bound, so a file of an internal note is staged only when the note itself is on the caller's page",
    ),
    (
        "mail/activity.rs::user_timezones",
        1,
        "reads the `tz` column of the given users in one statement (privileged, as its doc comment says); only the zone names are used, to date deadlines",
    ),
    (
        "mail/activity.rs::subscribe_assignees",
        2,
        "resolves the assignees' partners with a privileged read of `res.users`, then calls `follow` privileged on each activity's document for those partners; no other field is read and no other follower is added",
    ),
    (
        "mail/activity.rs::check_owner_records",
        1,
        "existence probe (`exists`) of the documents that activities name, no field is read; a missing document is refused",
    ),
    (
        "mail/activity.rs::_action_done",
        2,
        "the existence probe of the activities' documents, and the `message_post` on each document (subtype `mail.mt_activities`, the activity type of the activity) for the done message; the done path checks the caller's access to the activities first (a later release review r2: access before the done post)",
    ),
    (
        "mail/activity.rs::document_names",
        1,
        "reads `display_name` of the documents of activities, to name them in the done message and in errors; names only",
    ),
    (
        "mail/activity.rs::user_display_names",
        1,
        "reads the `name` column of the given users privileged (its doc comment), for the done message's assignee names; names only",
    ),
    (
        "mail/activity_mixin.rs::activities_panel",
        2,
        "privileged reads of the type name, category and icon of the activity types and of the partner and name of the users that the caller's own activity panel lists; nothing else is read",
    ),
    (
        "mail/activity_menu.rs::scan_limit",
        1,
        "reads the system parameter `mail.activity.systray.limit` (`get_int`, one non-secret integer the caller cannot read themselves) to bound the activity menu scan; the value is validated and never returned, and the scan itself runs as the caller",
    ),
    (
        "mail/activity_type.rs::unlink",
        1,
        "privileged search (inactive records included) and move of the activities of the types being deleted to the To-Do type, so a type is deletable whichever user's activity uses it; the deletion of the type itself runs as the caller",
    ),
    (
        "mail/activity_type.rs::load_types",
        1,
        "privileged read of the name, summary, delay, default user, default note and model of the types that activities name, so scheduling applies the type's defaults; refuses an id that does not exist",
    ),
    (
        "mail/activity_type.rs::default_type",
        1,
        "privileged search (limit 1) for the first active type of a model or of every model, by sequence (the reference ERP `_default_activity_type_for_model`); only the id leaves the search",
    ),
    (
        "erp-server/web/password_change.rs::execute",
        1,
        "opening the password reset wizard creates its lines in superuser mode, after the actor's manager authority and every target were checked: the line model refuses every other creation, and `create_uid` stays the actor",
    ),
];

// ----- discovery ---------------------------------------------------------------------------------

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_owned()
}

/// Literal `#[path]` modules outside a crate remain part of both security inventories.
#[test]
fn scanner_path_attribute_reads_external_sources() {
    let temp = tempfile::tempdir().unwrap();
    let consumer = temp.path().join("consumer");
    let shared = temp.path().join("shared");
    std::fs::create_dir_all(consumer.join("src")).unwrap();
    std::fs::create_dir_all(shared.join("nested")).unwrap();
    std::fs::write(
        consumer.join("src/lib.rs"),
        "#[path = \"../../shared/source.rs\"] mod external;",
    )
    .unwrap();
    std::fs::write(
        consumer.join("build.rs"),
        "#[path = \"../shared/source.rs\"] mod external;",
    )
    .unwrap();
    std::fs::write(
        shared.join("source.rs"),
        "pub fn checked(tx: &mut Transaction) { tx.sudo(true); }\n#[path = \"nested/child.rs\"] mod child;",
    ).unwrap();
    std::fs::write(
        shared.join("nested/child.rs"),
        "pub fn nested(tx: &mut Transaction) { tx.sudo(true); }",
    )
    .unwrap();
    for (files, prefix) in [
        (
            scan::module_files(&consumer.join("src/lib.rs")),
            "../../shared",
        ),
        (scan::other_roots(&consumer), "../shared"),
    ] {
        assert_eq!(
            scan::transaction_entry_points(&files),
            BTreeMap::from([
                (
                    "external::checked".to_owned(),
                    format!("{prefix}/source.rs")
                ),
                (
                    "external::child::nested".to_owned(),
                    format!("{prefix}/nested/child.rs")
                ),
            ])
        );
        assert_eq!(
            scan::sudo_sites(&files, "fixture"),
            BTreeMap::from([
                (format!("fixture/{prefix}/source.rs::checked"), 1),
                (format!("fixture/{prefix}/nested/child.rs::nested"), 1),
            ])
        );
    }
}

/// A default child of a `#[path]` source is beside that source, regardless of its stem.
#[test]
fn scanner_path_attribute_default_child_uses_parent_directory() {
    assert_scanner_path_attribute_default_child(false);
}

/// Inline module components start at the explicit source's parent, without its stem.
#[test]
fn scanner_path_attribute_inline_default_child_uses_parent_directory() {
    assert_scanner_path_attribute_default_child(true);
}

fn assert_scanner_path_attribute_default_child(inline: bool) {
    let temp = tempfile::tempdir().unwrap();
    let consumer = temp.path().join("consumer");
    let shared = temp.path().join("shared");
    let child = if inline {
        "inline/child.rs"
    } else {
        "child.rs"
    };
    let module = if inline {
        "external::inline::child"
    } else {
        "external::child"
    };
    let actual = shared.join(child);
    // The old scanner incorrectly inserted `source/`. Give that location a valid but
    // different source so a scan that merely succeeds cannot mask the missed real code.
    let decoy = shared.join("source").join(child);
    std::fs::create_dir_all(consumer.join("src")).unwrap();
    std::fs::create_dir_all(actual.parent().unwrap()).unwrap();
    std::fs::create_dir_all(decoy.parent().unwrap()).unwrap();
    std::fs::write(
        consumer.join("src/lib.rs"),
        "#[path = \"../../shared/source.rs\"] mod external;",
    )
    .unwrap();
    std::fs::write(
        consumer.join("build.rs"),
        "#[path = \"../shared/source.rs\"] mod external;",
    )
    .unwrap();
    std::fs::write(
        shared.join("source.rs"),
        if inline {
            "mod inline { mod child; }"
        } else {
            "mod child;"
        },
    )
    .unwrap();
    std::fs::write(
        actual,
        "pub fn actual(tx: &mut Transaction) { tx.sudo(true); }",
    )
    .unwrap();
    std::fs::write(
        decoy,
        "pub fn decoy(tx: &mut Transaction) { tx.sudo(true); tx.sudo(true); }",
    )
    .unwrap();
    for (files, prefix) in [
        (
            scan::module_files(&consumer.join("src/lib.rs")),
            "../../shared",
        ),
        (scan::other_roots(&consumer), "../shared"),
    ] {
        assert_eq!(
            scan::transaction_entry_points(&files),
            BTreeMap::from([(format!("{module}::actual"), format!("{prefix}/{child}"))]),
        );
        assert_eq!(
            scan::sudo_sites(&files, "fixture"),
            BTreeMap::from([(format!("fixture/{prefix}/{child}::actual"), 1)]),
        );
    }
}

fn scanner_panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        panic!("scanner panic did not contain a diagnostic")
    }
}

/// A missing explicit source must fail; a default module file cannot hide the missing path.
#[test]
fn scanner_path_attribute_missing_source_fails_loudly() {
    let temp = tempfile::tempdir().unwrap();
    let lib = temp.path().join("lib.rs");
    std::fs::write(&lib, "#[path = \"missing.rs\"] mod external;").unwrap();
    std::fs::write(temp.path().join("external.rs"), "pub fn skipped() {}").unwrap();
    let message = scanner_panic_message(
        std::panic::catch_unwind(|| scan::module_files(&lib))
            .err()
            .expect("missing source was accepted"),
    );
    // The OS error suffix varies by platform; the scanner's exact path-bearing prefix does not.
    assert!(
        message.starts_with(&format!(
            "cannot read {}: ",
            temp.path().join("missing.rs").display()
        )),
        "{message}"
    );
}

/// Unsupported or ambiguous attribute values cannot silently choose a default module file.
#[test]
fn scanner_path_attribute_invalid_values_fail_loudly() {
    let temp = tempfile::tempdir().unwrap();
    let lib = temp.path().join("lib.rs");
    std::fs::write(temp.path().join("external.rs"), "pub fn skipped() {}").unwrap();
    for (attribute, expected) in [
        (
            "#[path = 42]",
            "module external: #[path] must be one non-empty string literal",
        ),
        (
            "#[path = \"\"]",
            "module external: #[path] must be one non-empty string literal",
        ),
        (
            "#[path = \"external.rs\"] #[path = \"other.rs\"]",
            "module external: duplicate #[path] attributes",
        ),
    ] {
        std::fs::write(&lib, format!("{attribute} mod external;")).unwrap();
        let message = scanner_panic_message(
            std::panic::catch_unwind(|| scan::module_files(&lib))
                .err()
                .expect("invalid path attribute was accepted"),
        );
        assert_eq!(message, expected);
    }
}

/// Every row has exactly one function and every function exactly one row.
#[test]
fn every_public_entry_point_has_a_row() {
    let files = scan::module_files(&workspace().join("crates/erp-orm/src/lib.rs"));
    let found = scan::transaction_entry_points(&files);
    assert!(
        found.len() > 100,
        "the scan found only {} entry points: is it reading erp-orm's source?",
        found.len()
    );
    let mut rows: BTreeSet<&str> = BTreeSet::new();
    for (name, class) in ENTRY_POINTS {
        assert!(rows.insert(name), "{name} has two rows");
        match class {
            Unchecked(reason) => assert!(!reason.trim().is_empty(), "{name}: no reason"),
            Gap(reason) => {
                assert!(!reason.trim().is_empty(), "{name}: no reason");
                eprintln!("parity gap: {name}: {reason}");
            }
            Checked(_) | CheckedRefusesSu(_) => {}
        }
    }
    let missing: Vec<String> = found
        .iter()
        .filter(|(name, _)| !rows.contains(name.as_str()))
        .map(|(name, file)| format!("{name} (crates/erp-orm/src/{file})"))
        .collect();
    let stale: Vec<&&str> = rows
        .iter()
        .filter(|name| !found.contains_key(**name))
        .collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "public functions taking the transaction without a row in ENTRY_POINTS (add one: Checked \
         with a probe in `probe`, or Unchecked/Gap with the reason): {missing:#?}\nrows without a \
         function: {stale:?}"
    );
}

/// The call sites `scan` finds in every crate and addon of the workspace, outside tests: library
/// roots, binaries and build scripts (crates with no library root too).
fn workspace_sites(
    scan: fn(&[scan::SourceFile], &str) -> BTreeMap<String, usize>,
) -> BTreeMap<String, usize> {
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for dir in ["crates", "addons"] {
        let mut roots: Vec<PathBuf> = std::fs::read_dir(workspace().join(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.join("src/lib.rs").exists())
            .collect();
        roots.sort();
        for root in roots {
            let name = root.file_name().unwrap().to_string_lossy().into_owned();
            let files = scan::module_files(&root.join("src/lib.rs"));
            found.extend(scan(&files, &name));
        }
        // binaries and build scripts (crates with no library root too)
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(workspace().join(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for root in dirs {
            let name = root.file_name().unwrap().to_string_lossy().into_owned();
            let files = scan::other_roots(&root);
            found.extend(scan(&files, &name));
        }
    }
    found
}

/// Every `sudo(true)` call site outside tests has a row with its reason and count.
#[test]
fn sudo_sites_are_listed() {
    let found = workspace_sites(scan::sudo_sites);
    let listed: BTreeMap<String, usize> = SUDO_SITES
        .iter()
        .map(|(site, n, _)| ((*site).to_owned(), *n))
        .collect();
    assert_eq!(listed.len(), SUDO_SITES.len(), "a sudo site has two rows");
    assert_eq!(
        found, listed,
        "sudo(true) call sites differ from SUDO_SITES (add or update the row with its reason)"
    );
}

/// Every call site outside tests of `ReadAuditOwner::new`, `read_only_audited` and
/// `Transaction::take_audit_attempt` (`<crate>/<file>::<function> <callee>`, how many): who
/// declares itself the read-audit owner of a read-only transaction, or takes its evidence out,
/// and why. An owner promises to write the attempt evidence before it releases the result, and
/// taking the evidence out of a transaction that is not the caller's own would let an elevated
/// read go without its row (A2, P4; review cycle 10). Native addon code is bounded by
/// the P6 lint; until then this list is the boundary.
const READ_AUDIT_SITES: &[(&str, usize, &str)] = &[
    (
        "erp-orm/transaction.rs::read_only_audited read_only_audited",
        1,
        "`CredentialOptions::read_only_audited` forwards the owner to the `BeginOptions` it wraps; it declares no owner of its own",
    ),
    (
        "erp-rpc/service.rs::attempt ReadAuditOwner::new",
        2,
        "the RPC service is the read-audit owner of every read-effect call, from a constant, never from request input: once for a session call, once for a credential call",
    ),
    (
        "erp-rpc/service.rs::attempt read_only_audited",
        2,
        "declares that owner on the begin options of a session call and on the credential options of a credential call",
    ),
    (
        "erp-rpc/service.rs::evidence_of take_audit_attempt",
        1,
        "the owner takes the attempt evidence of its own transaction after the body, to write the `read_ok` or attempt row before the result is released",
    ),
];

/// Every read-audit owner call site outside tests has a row with its reason and count.
#[test]
fn read_audit_sites_are_listed() {
    let found = workspace_sites(scan::read_audit_sites);
    let listed: BTreeMap<String, usize> = READ_AUDIT_SITES
        .iter()
        .map(|(site, n, _)| ((*site).to_owned(), *n))
        .collect();
    assert_eq!(
        listed.len(),
        READ_AUDIT_SITES.len(),
        "a read-audit site has two rows"
    );
    assert_eq!(
        found, listed,
        "read-audit owner call sites differ from READ_AUDIT_SITES (add or update the row with \
         its reason)"
    );
}

// ----- the recording policy ----------------------------------------------------------------------

/// The (model, operation) pairs the engine asked the policy about.
type Log = Arc<Mutex<Vec<(String, Operation)>>>;

/// A `DomainPolicy` that records every model check and access domain the engine asks for.
struct Recording {
    inner: DomainPolicy,
    log: Log,
}

impl Recording {
    fn note(&self, model: &ModelSpec, op: Operation) {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((model.name.to_owned(), op));
    }
}

impl AccessPolicy for Recording {
    fn group_refs_are_stable(&self) -> bool {
        self.inner.group_refs_are_stable()
    }

    fn generation(&self) -> PolicyGeneration {
        self.inner.generation()
    }

    fn snapshot(&self) -> SnapshotRef {
        self.inner.snapshot()
    }

    fn needs_resolved_principals(&self) -> bool {
        self.inner.needs_resolved_principals()
    }

    fn intern_groups(&self, groups: &GroupSet) -> erp_fields::Result<InternedGroups> {
        self.inner.intern_groups(groups)
    }

    fn check_model(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_fields::Result<()> {
        self.note(model, op);
        self.inner.check_model(b, model, op)
    }

    fn access_domain(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_fields::Result<AccessDomain> {
        self.note(model, op);
        self.inner.access_domain(b, model, op)
    }

    fn field_access(
        &self,
        b: &PrincipalBinding<'_>,
        model: &ModelSpec,
    ) -> erp_fields::Result<Arc<erp_orm::FieldMask>> {
        self.inner.field_access(b, model)
    }

    fn groups(&self, direct: &[GroupId]) -> erp_fields::Result<GroupSet> {
        self.inner.groups(direct)
    }

    fn allowing_groups(
        &self,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_fields::Result<Vec<GroupId>> {
        self.inner.allowing_groups(model, op)
    }

    fn has_capability(
        &self,
        b: &PrincipalBinding<'_>,
        cap: &Capability,
    ) -> erp_fields::Result<bool> {
        self.inner.has_capability(b, cap)
    }

    fn explain(&self, req: &ExplainRequest<'_>) -> erp_fields::Result<Explanation> {
        self.inner.explain(req)
    }
}

// ----- the probes --------------------------------------------------------------------------------

/// The fixture: `shown` (val 1) and `hidden` (val -1) objects, a category, a container linking
/// both objects; two counter items, `Shown` and `Hidden`, of one counter.
struct Fixture {
    shown: i64,
    hidden: i64,
    categ: i64,
    container: i64,
    items: [i64; 2],
    /// The keyed object the read rule hides.
    keyed_hidden: i64,
}

/// What a probe saw.
#[derive(Debug)]
enum Seen {
    /// The ids the call returned.
    Ids(Vec<i64>),
    /// A count of objects.
    Count(u64),
    /// A structured answer (groups, web results), as text: the objects' names show in it.
    Text(String),
    /// A yes/no answer about the hidden record.
    Bool(bool),
    /// The call returned nothing to look at.
    Done,
}

fn browse(env: &erp_orm::Env, ids: &[i64]) -> Records {
    env.records(OBJ)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}

fn vals(pairs: &[(&str, Value)]) -> ValueMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn row(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn ids_of(rows: &[BTreeMap<String, Value>]) -> Vec<i64> {
    rows.iter()
        .filter_map(|r| match r.get("id") {
            Some(Value::Int(id)) => Some(*id),
            _ => None,
        })
        .collect()
}

/// Run the probe of `name` in `tx`; `None` for a name without a probe.
fn panel_count(value: Value) -> Seen {
    let response = value.dict().expect("panel response dictionary");
    let values = response.get("values").unwrap();
    let Value::Rows(rows) = values else {
        panic!("panel choices must be columnar rows: {values:?}");
    };
    let Value::Int(count) = rows.get(0).unwrap().get("__count").unwrap() else {
        panic!("panel count must be an integer");
    };
    Seen::Count(u64::try_from(count).unwrap())
}

async fn probe(name: &str, tx: &mut Transaction, f: &Fixture) -> Option<erp_fields::Result<Seen>> {
    let env = tx.env();
    let both = [f.shown, f.hidden];
    let hidden = browse(&env, &[f.hidden]);
    let all = browse(&env, &both);
    let objs = |_: &Transaction| env.records(OBJ).unwrap();
    let typed = || env.model::<SomeObj>().unwrap();
    let typed_hidden = typed().browse([f.hidden]).unwrap();
    let typed_all = typed().browse(both).unwrap();
    let everything = Domain::TRUE;
    let opts = SearchOptions::default();
    let named = ["name"];
    let result: erp_fields::Result<Seen> =
        match name {
            "Records::cell" => hidden.cell(tx, "name").await.map(|_| Seen::Done),
            "Records::cells" => all.cells(tx, "name").await.map(|_| Seen::Done),
            "Records::value" => hidden.value(tx, "name").await.map(|_| Seen::Done),
            "Records::values" => all.values(tx, "name").await.map(|_| Seen::Done),
            "Records::read" => all.read(tx, &named).await.map(|_| Seen::Done),
            "Records::read_with" => all
                .read_with(tx, &named, Load::Raw)
                .await
                .map(|_| Seen::Done),
            "Records::read_rows" => all
                .read_rows(tx, &named, Load::Raw)
                .await
                .map(|_| Seen::Done),
            "Records::fetch" => all.fetch(tx, Some(&named)).await.map(|()| Seen::Done),
            "Records::related" => all.related(tx, "categ_id").await.map(|_| Seen::Done),
            "Records::reference" => hidden.reference(tx, "name").await.map(|_| Seen::Done),
            "Records::filtered_domain" => all
                .filtered_domain(tx, &Domain::condition("val", Operator::Gt, -5).unwrap())
                .await
                .map(|_| Seen::Done),
            "Records::filtered_path" => all.filtered_path(tx, "name").await.map(|_| Seen::Done),
            "Records::filtered_values" => all
                .filtered_values(tx, "val", |_| true)
                .await
                .map(|_| Seen::Done),
            "Records::mapped_path" => all.mapped_path(tx, "name").await.map(|_| Seen::Done),
            "Records::grouped" => all.grouped(tx, "name").await.map(|_| Seen::Done),
            "Records::grouped_records" => all
                .grouped_records(tx, "categ_id")
                .await
                .map(|_| Seen::Done),
            "Records::sorted" => all.sorted(tx, Some("val"), false).await.map(|_| Seen::Done),
            "Records::web_read" => all
                .web_read(tx, Spec::new().field("name"))
                .await
                .map(|_| Seen::Done),
            "Records::get_field_translations" => hidden
                .get_field_translations(tx, "name")
                .await
                .map(|_| Seen::Done),
            "Records::x2many_commands" => {
                let container = env
                    .records("test_access.container")
                    .unwrap()
                    .browse([f.container])
                    .unwrap();
                container
                    .x2many_commands(tx, "some_ids")
                    .await
                    .map(|commands| {
                        Seen::Ids(
                            commands
                                .into_iter()
                                .flat_map(|c| match c {
                                    Command::Set(ids) => ids.iter().map(|i| i.get()).collect(),
                                    Command::Link(id) | Command::Update(id, _) => vec![id.get()],
                                    _ => Vec::new(),
                                })
                                .collect(),
                        )
                    })
            }
            "Records::check_access" => hidden.check_access(tx, Read).await.map(|()| Seen::Done),
            "Records::has_access" => hidden.has_access(tx, Read).await.map(Seen::Bool),
            "Records::filtered_access" => all
                .filtered_access(tx, Read)
                .await
                .map(|r| Seen::Ids(r.ids())),
            "Records::search" => objs(tx)
                .search(tx, &everything)
                .await
                .map(|r| Seen::Ids(r.ids())),
            "Records::search_with" => objs(tx)
                .search_with(tx, &everything, &opts)
                .await
                .map(|r| Seen::Ids(r.ids())),
            "Records::stream" => {
                match objs(tx)
                    .stream(tx, &everything, &named, &StreamOptions::new())
                    .await
                {
                    Ok(mut stream) => {
                        let mut ids = Vec::new();
                        loop {
                            match stream.next(tx).await {
                                Ok(Some(window)) => {
                                    for i in window.indices() {
                                        ids.extend(window.i64(i, "id").unwrap());
                                    }
                                }
                                Ok(None) => break Ok(Seen::Ids(ids)),
                                Err(e) => break Err(e),
                            }
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            "RowsStream::next" => match all.stream_records(&named, &StreamOptions::new()) {
                Ok(mut stream) => stream.next(tx).await.map(|_| Seen::Done),
                Err(e) => Err(e),
            },
            "Records::search_count" => objs(tx)
                .search_count(tx, &everything, None)
                .await
                .map(Seen::Count),
            "Records::search_query" | "SearchQuery::ids" => {
                match objs(tx).search_query(tx, &everything, &opts).await {
                    Ok(mut query) => query
                        .ids(tx)
                        .await
                        .map(|ids| Seen::Ids(ids.iter().map(|i| i.get()).collect())),
                    Err(e) => Err(e),
                }
            }
            "SearchQuery::records" => match objs(tx).search_query(tx, &everything, &opts).await {
                Ok(mut query) => query.records(tx).await.map(|r| Seen::Ids(r.ids())),
                Err(e) => Err(e),
            },
            "SearchQuery::count" => match objs(tx).search_query(tx, &everything, &opts).await {
                Ok(query) => query.count(tx).await.map(Seen::Count),
                Err(e) => Err(e),
            },
            "Records::name_search" => objs(tx)
                .name_search(tx, "")
                .await
                .map(|r| Seen::Ids(r.into_iter().map(|(id, _)| id).collect())),
            "Records::name_search_with" => objs(tx)
                .name_search_with(tx, "", &NameSearch::default())
                .await
                .map(|r| Seen::Ids(r.into_iter().map(|(id, _)| id).collect())),
            "Records::search_read" => objs(tx)
                .search_read(tx, &everything, &named, &opts)
                .await
                .map(|rows| Seen::Ids(ids_of(&rows))),
            "query::run" => erp_orm::query::run(tx, &env, &query_ast())
                .await
                .map(|result| match result {
                    erp_orm::query::QueryResult::Rows { rows, .. } => Seen::Ids(ids_of(&rows)),
                    other => Seen::Text(format!("{other:?}")),
                }),
            "query::check" => erp_orm::query::check(tx, &env, &query_ast())
                .await
                .map(|()| Seen::Done),
            "Records::search_after" => objs(tx)
                .search_after(tx, &everything, &SearchOptions::new().limit(10), None)
                .await
                .map(|page| Seen::Ids(page.records.ids())),
            "Records::web_search_read" => objs(tx)
                .web_search_read(
                    tx,
                    WebSearchRead {
                        domain: everything.clone(),
                        specification: Spec::new().field("name"),
                        offset: 0,
                        limit: None,
                        order: None,
                        count_limit: None,
                    },
                )
                .await
                .map(|v| Seen::Text(format!("{v:?}"))),
            "Records::web_name_search" => objs(tx)
                .web_name_search(
                    tx,
                    WebNameSearch {
                        name: String::new(),
                        specification: Spec::new().field("name"),
                        domain: everything.clone(),
                        operator: Operator::ILike,
                        limit: None,
                    },
                )
                .await
                .map(|v| Seen::Text(format!("{v:?}"))),
            "Records::_read_group" => objs(tx)
                ._read_group(tx, ReadGroup::new(everything.clone()).groupby(["name"]))
                .await
                .map(|g| Seen::Text(format!("{g:?}"))),
            "Records::read_group" => objs(tx)
                .read_group(tx, ReadGroup::new(everything.clone()).groupby(["name"]))
                .await
                .map(|g| Seen::Text(format!("{g:?}"))),
            "Records::_read_grouping_sets" => objs(tx)
                ._read_grouping_sets(
                    tx,
                    ReadGroupingSets::new(everything.clone(), vec![vec!["name"]]),
                )
                .await
                .map(|g| Seen::Text(format!("{g:?}"))),
            "Records::formatted_read_group" => objs(tx)
                .formatted_read_group(tx, FormattedReadGroup::new(everything.clone(), ["name"]))
                .await
                .map(|g| Seen::Text(format!("{g:?}"))),
            "Records::formatted_read_grouping_sets" => objs(tx)
                .formatted_read_grouping_sets(
                    tx,
                    FormattedReadGroupingSets {
                        domain: everything.clone(),
                        grouping_sets: vec![vec!["name".to_owned()]],
                        aggregates: Vec::new(),
                        order: None,
                    },
                )
                .await
                .map(|g| Seen::Text(format!("{g:?}"))),
            "Records::web_read_group" => objs(tx)
                .web_read_group(tx, WebReadGroup::new(everything.clone(), ["name"]))
                .await
                .map(|g| Seen::Text(format!("{g:?}"))),
            "Records::read_progress_bar" => objs(tx)
                .read_progress_bar(
                    tx,
                    ProgressBar {
                        domain: everything.clone(),
                        group_by: "name".to_owned(),
                        field: "name".to_owned(),
                        colors: vec!["Shown".to_owned(), "Hidden".to_owned()],
                    },
                )
                .await
                // the groups (names); the colors of every group name both objects
                .map(|bar| match bar {
                    Value::Map(groups) => {
                        Seen::Text(groups.keys().cloned().collect::<Vec<_>>().join(","))
                    }
                    other => Seen::Text(format!("not a map: {other:?}")),
                }),
            "Records::search_panel_select_range" => {
                let mut args = SearchPanelSelectRange::new("categ_id");
                args.enable_counters = true;
                objs(tx)
                    .search_panel_select_range(tx, args)
                    .await
                    .map(panel_count)
            }
            "Records::search_panel_select_multi_range" => {
                let mut args = SearchPanelSelectMultiRange::new("categ_id");
                args.enable_counters = true;
                objs(tx)
                    .search_panel_select_multi_range(tx, args)
                    .await
                    .map(panel_count)
            }
            "Records::write" => hidden
                .write(tx, RawVals::new().set_cell("val", Cell::Int(5)))
                .await
                .map(|()| Seen::Done),
            "Records::write_base" => hidden
                .write_base(tx, RawVals::new().set_cell("val", Cell::Int(5)))
                .await
                .map(|()| Seen::Done),
            "Records::write_values" => {
                let values = row(&[("val", Value::Int(5))]);
                hidden.write_values(tx, &values).await.map(|()| Seen::Done)
            }
            "Records::set_value" => hidden
                .set_value(tx, "val", Value::Int(5))
                .await
                .map(|()| Seen::Done),
            "Records::assign" => {
                let categ = env
                    .records("test_access.categ")
                    .unwrap()
                    .browse([f.categ])
                    .unwrap();
                hidden
                    .assign(tx, "categ_id", &categ)
                    .await
                    .map(|()| Seen::Done)
            }
            "Records::update_field_translations" => hidden
                .update_field_translations(tx, "name", [("en_US", Value::str("New"))])
                .await
                .map(|_| Seen::Done),
            "Records::web_save" => hidden
                .web_save(
                    tx,
                    WebSave {
                        vals: vals(&[("val", Value::Int(5))]),
                        specification: Spec::new(),
                        next_id: None,
                    },
                )
                .await
                .map(|_| Seen::Done),
            "Records::web_save_multi" => hidden
                .web_save_multi(tx, vec![vals(&[("val", Value::Int(5))])], Spec::new())
                .await
                .map(|_| Seen::Done),
            "Records::web_resequence" => all
                .web_resequence(tx, Spec::new(), "val", 0)
                .await
                .map(|_| Seen::Done),
            "Records::onchange" => hidden
                .onchange(
                    tx,
                    vals(&[("val", Value::Int(5))]),
                    vec!["val".to_owned()],
                    Spec::new(),
                )
                .await
                .map(|_| Seen::Done),
            "Records::onchange_batch" => objs(tx)
                .onchange_batch(
                    tx,
                    vec![vals(&[("val", Value::Int(5))])],
                    vec!["val".to_owned()],
                    Spec::new(),
                )
                .await
                .map(|_| Seen::Done),
            "Records::expect_versions" => env
                .records(KEYED)
                .unwrap()
                .browse([f.keyed_hidden])
                .unwrap()
                .expect_versions(
                    tx,
                    [(erp_fields::RecId::from_real(f.keyed_hidden).unwrap(), 1)],
                )
                .await
                .map(|()| Seen::Done),
            "Records::upsert" => {
                let call = UpsertCall::new("_name_key")
                    .with_rows([RawVals::new()
                        .set_cell("name", Cell::Str("Hidden".into()))
                        .set_cell("val", Cell::Int(5))])
                    .with_update(["val"]);
                env.records(KEYED)
                    .unwrap()
                    .upsert(tx, call)
                    .await
                    .map(|_| Seen::Done)
            }
            "Records::upsert_values" => env
                .records(KEYED)
                .unwrap()
                .upsert_values(
                    tx,
                    "_name_key",
                    &[row(&[
                        ("name", Value::str("Hidden")),
                        ("val", Value::Int(5)),
                    ])],
                    &["val"],
                    &[],
                )
                .await
                .map(|_| Seen::Done),
            "RecordSet::upsert" => {
                let keyed = env.model::<Keyed>().unwrap();
                keyed
                    .upsert(
                        tx,
                        Upsert::on(Keyed::NAME_KEY)
                            .rows([keyed.vals().set(Keyed::NAME, "Hidden").set(Keyed::VAL, 5)])
                            .update([Keyed::VAL]),
                    )
                    .await
                    .map(|_| Seen::Done)
            }
            "Records::create" => objs(tx)
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str("New".into()))
                            .set_cell("val", Cell::Int(-2)),
                    ],
                )
                .await
                .map(|_| Seen::Done),
            "Records::create_values" => objs(tx)
                .create_values(
                    tx,
                    &[row(&[("name", Value::str("New")), ("val", Value::Int(-2))])],
                )
                .await
                .map(|_| Seen::Done),
            "Records::unlink" => hidden.unlink(tx).await.map(|()| Seen::Done),
            "Records::web_unlink" => hidden.web_unlink(tx).await.map(|_| Seen::Done),
            "RecordSet::get" => typed_hidden
                .get(tx, SomeObj::NAME)
                .await
                .map(|_| Seen::Done),
            "RecordSet::mapped" => typed_all
                .mapped(tx, SomeObj::NAME)
                .await
                .map(|_| Seen::Done),
            "RecordSet::related" => env
                .model::<CounterItem>()
                .unwrap()
                .browse(f.items)
                .unwrap()
                .related(tx, CounterItem::COUNTER_ID)
                .await
                .map(|_| Seen::Done),
            "RecordSet::filtered_domain" => typed_all
                .filtered_domain(tx, &Domain::condition("val", Operator::Gt, -5).unwrap())
                .await
                .map(|_| Seen::Done),
            "RecordSet::filtered" => typed_all
                .filtered(tx, SomeObj::VAL, |_| true)
                .await
                .map(|_| Seen::Done),
            "RecordSet::sorted" => typed_all
                .sorted(tx, Some("val"), false)
                .await
                .map(|_| Seen::Done),
            "RecordSet::grouped" => typed_all
                .grouped(tx, SomeObj::NAME)
                .await
                .map(|_| Seen::Done),
            "RecordSet::search" => typed()
                .search(tx, &everything)
                .await
                .map(|r| Seen::Ids(r.as_records().ids())),
            "RecordSet::search_with" => typed()
                .search_with(tx, &everything, &opts)
                .await
                .map(|r| Seen::Ids(r.as_records().ids())),
            "RecordSet::search_count" => typed()
                .search_count(tx, &everything, None)
                .await
                .map(Seen::Count),
            "RecordSet::name_search" => typed()
                .name_search(tx, "")
                .await
                .map(|r| Seen::Ids(r.into_iter().map(|(id, _)| id).collect())),
            "RecordSet::name_search_with" => typed()
                .name_search_with(tx, "", &NameSearch::default())
                .await
                .map(|r| Seen::Ids(r.into_iter().map(|(id, _)| id).collect())),
            "RecordSet::search_read" => typed()
                .search_read(tx, &everything, &named, &opts)
                .await
                .map(|rows| Seen::Ids(ids_of(&rows))),
            "RecordSet::write" => typed_hidden
                .write(tx, typed_hidden.vals().set(SomeObj::VAL, 5))
                .await
                .map(|()| Seen::Done),
            "RecordSet::set" => typed_hidden
                .set(tx, SomeObj::VAL, 5)
                .await
                .map(|()| Seen::Done),
            "RecordSet::create" => {
                let set = typed();
                set.create(
                    tx,
                    vec![set.vals().set(SomeObj::NAME, "New").set(SomeObj::VAL, -2)],
                )
                .await
                .map(|_| Seen::Done)
            }
            _ => return None,
        };
    Some(result)
}

/// The record-level refusal's first line (its SEC-009 body is's and changes with it;
/// the probe needs the refusal, its model and its operation).
fn record_header(h: &Harness, model: &str, op: Operation) -> String {
    let spec = h.registry().model(model).unwrap();
    let verb = match op {
        Read => "access",
        Write => "modify",
        Create => "create",
        Unlink => "delete",
    };
    format!(
        "Your access rights do not let you {verb} '{}' ({}) records.",
        spec.description, spec.name
    )
}

/// Whether `seen` holds the hidden record (`shown` must be there either way).
fn sees_hidden(name: &str, seen: &Seen, f: &Fixture) -> bool {
    match seen {
        Seen::Ids(ids) => {
            assert!(
                ids.contains(&f.shown),
                "{name}: the shown record is missing: {ids:?}"
            );
            ids.contains(&f.hidden)
        }
        Seen::Count(n) => {
            assert!(*n >= 1, "{name}: the shown record is missing (count {n})");
            *n == 2
        }
        Seen::Text(text) => {
            assert!(
                text.contains("Shown"),
                "{name}: the shown record is missing: {text}"
            );
            text.contains("Hidden")
        }
        Seen::Bool(yes) => *yes,
        Seen::Done => panic!("{name}: a Hidden probe must return what it saw"),
    }
}

async fn fixture(h: &Harness) -> Fixture {
    let mut tx = h.begin().await.unwrap();
    let categ = tx
        .env()
        .records("test_access.categ")
        .unwrap()
        .create_values(&mut tx, &[row(&[("name", Value::str("c1"))])])
        .await
        .unwrap()
        .ids()[0];
    let created = tx
        .env()
        .records(OBJ)
        .unwrap()
        .create_values(
            &mut tx,
            &[
                row(&[
                    ("name", Value::str("Shown")),
                    ("val", Value::Int(1)),
                    ("categ_id", Value::Int(categ)),
                ]),
                row(&[
                    ("name", Value::str("Hidden")),
                    ("val", Value::Int(-1)),
                    ("categ_id", Value::Int(categ)),
                ]),
            ],
        )
        .await
        .unwrap()
        .ids();
    let container = tx
        .env()
        .records("test_access.container")
        .unwrap()
        .create_values(&mut tx, &[row(&[("name", Value::str("box"))])])
        .await
        .unwrap();
    let linked = browse(&tx.env(), &created);
    container
        .assign(&mut tx, "some_ids", &linked)
        .await
        .unwrap();
    let container = container.ids()[0];
    let counter = tx
        .env()
        .records("test_rel.counter")
        .unwrap()
        .create_values(&mut tx, &[row(&[("name", Value::str("counter"))])])
        .await
        .unwrap()
        .ids()[0];
    let items = tx
        .env()
        .records(ITEM)
        .unwrap()
        .create_values(
            &mut tx,
            &[
                row(&[
                    ("name", Value::str("Shown")),
                    ("counter_id", Value::Int(counter)),
                ]),
                row(&[
                    ("name", Value::str("Hidden")),
                    ("counter_id", Value::Int(counter)),
                ]),
            ],
        )
        .await
        .unwrap()
        .ids();
    let keyed = tx
        .env()
        .records(KEYED)
        .unwrap()
        .create_values(
            &mut tx,
            &[
                row(&[("name", Value::str("Shown")), ("val", Value::Int(1))]),
                row(&[("name", Value::str("Hidden")), ("val", Value::Int(-1))]),
            ],
        )
        .await
        .unwrap()
        .ids();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    Fixture {
        shown: created[0],
        hidden: created[1],
        categ,
        container,
        items: [items[0], items[1]],
        keyed_hidden: keyed[1],
    }
}

async fn begin_user(h: &Harness) -> Transaction {
    let company = CompanyId::new(1).unwrap();
    let principal = PrincipalBuilder::user(UserId::new(DEMO).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), [company], NonZeroU64::MIN).unwrap(),
        )
        .companies(vec![company])
        .build()
        .unwrap();
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap()
}

/// Every `Checked` row: refused or filtered for a user, with the policy asked about the
/// operation; allowed for `System`.
#[tokio::test]
async fn checked_entry_points_go_through_the_engine() {
    let rule = Domain::condition("val", Operator::Gt, 0).unwrap();
    let not_hidden = Domain::condition("name", Operator::Ne, "Hidden").unwrap();
    let log: Log = Arc::default();
    let policy = Recording {
        inner: DomainPolicy::builder(Otherwise::Allow)
            .rule_all(OBJ, &rule)
            .rule_all(ITEM, &not_hidden)
            // only reads are restricted: the hidden object may be written but not read, which the
            // upsert still withholds (review SEC-M1)
            .rule(KEYED, Read, rule.clone())
            .build()
            .unwrap(),
        log: Arc::clone(&log),
    };
    let h = Harness::builder([&base_users::ADDON, &test_access::ADDON, &test_rel::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let f = fixture(&h).await;
    let mut failures = Vec::new();
    for (name, class) in ENTRY_POINTS {
        let (Checked(expect) | CheckedRefusesSu(expect)) = class else {
            continue;
        };
        let refuses_su = matches!(class, CheckedRefusesSu(_));
        let op = match expect {
            Refused(op) | Hidden(op) | Asked(op) | Withheld(op) => *op,
        };
        let model = model_of(name);
        // as a user
        let mut tx = begin_user(&h).await;
        // cleared right before the call under test: only what the probe's own call asks counts
        log.lock().unwrap_or_else(PoisonError::into_inner).clear();
        let Some(user) = probe(name, &mut tx, &f).await else {
            failures.push(format!("{name}: no probe"));
            continue;
        };
        drop(tx);
        let asked = log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(m, asked)| m == model && *asked == op);
        if !asked {
            failures.push(format!(
                "{name}: the policy was never asked about ({model}, {op})"
            ));
        }
        match (expect, &user) {
            (Refused(_), Err(e))
                if e.kind() == erp_fields::ErrorKind::Access
                    && e.message().starts_with(&record_header(&h, model, op)) => {}
            (Hidden(_), Ok(seen)) if !sees_hidden(name, seen, &f) => {}
            (Asked(_), Ok(_)) => {}
            (Withheld(_), Err(e))
                if e.kind() == erp_fields::ErrorKind::Validation
                    && e.message()
                        == format!("The database refused the change: {KEYED_MESSAGE}") => {}
            _ => failures.push(format!(
                "{name}: as a user, expected {expect:?}, got {user:?}"
            )),
        }
        // as System: the probe itself works and sees everything
        let mut tx = h.begin().await.unwrap();
        let system = probe(name, &mut tx, &f).await.unwrap();
        drop(tx);
        if refuses_su {
            match &system {
                Err(e) if e.kind() == erp_fields::ErrorKind::Access && e.message() == NO_SUDO => {}
                other => failures.push(format!(
                    "{name}: as System, expected the superuser-mode refusal, got {other:?}"
                )),
            }
            continue;
        }
        match (expect, &system) {
            (Refused(_) | Asked(_) | Withheld(_), Ok(_)) => {}
            (Hidden(_), Ok(seen)) if sees_hidden(name, seen, &f) => {}
            _ => failures.push(format!(
                "{name}: as System, expected success, got {system:?}"
            )),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    h.cleanup().await.unwrap();
}
