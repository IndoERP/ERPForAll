//! Host side of the `orm-host` interface (plan §6.1 and §6.3).
//!
//! [`OrmBridge`] is what a guest's imports reach. Before any call goes to the [`OrmBackend`] it
//! checks, in this order: the addon holds the capability for the operation, the batch and payload
//! limits hold, and the arguments agree with the contract. The identity (user, company) is the
//! [`Caller`] the host bound to the store; no argument of any call can name or change it.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::bindings::erpforall::addon::orm_host::LogLevel;
use crate::bindings::erpforall::addon::types::{
    BoundaryError, BoundaryErrorKind, ColumnData, ColumnVector, RecordBatch, ScalarValue,
    X2manyCommand,
};
use crate::error::HostError;
use crate::limits::Limits;

/// A boxed future, so [`OrmBackend`] can be used as a trait object.
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Result of a backend call, in the contract's error type.
pub type BridgeResult<T> = Result<T, BoundaryError>;

/// Builds a [`BoundaryError`].
#[must_use]
pub fn boundary_error(kind: BoundaryErrorKind, message: impl Into<String>) -> BoundaryError {
    BoundaryError {
        kind,
        message: message.into(),
    }
}

/// The authenticated identity a call runs under. Set by the host for each call into the guest
/// (from the session, or from the scoped rights of an AI principal); never read from the
/// guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    user_id: i64,
    company_id: i64,
    allowed_company_ids: Vec<i64>,
}

impl Caller {
    /// Validates the identity: ids are positive and the company is one of the allowed companies.
    pub fn new(
        user_id: i64,
        company_id: i64,
        allowed_company_ids: Vec<i64>,
    ) -> Result<Self, HostError> {
        if user_id <= 0 || company_id <= 0 || allowed_company_ids.iter().any(|id| *id <= 0) {
            return Err(HostError::Config(format!(
                "caller ids must be positive (user {user_id}, company {company_id})"
            )));
        }
        if !allowed_company_ids.contains(&company_id) {
            return Err(HostError::Config(format!(
                "company {company_id} is not in the allowed companies {allowed_company_ids:?}"
            )));
        }
        Ok(Self {
            user_id,
            company_id,
            allowed_company_ids,
        })
    }

    /// The identity of an environment that has no company, such as the System environment of a
    /// module operation (company `0`, no allowed companies). It is exactly the environment's own
    /// identity: the guest runs where native code of that environment runs, and cannot name a
    /// company, because there is none to name (the core methods and the computes of
    /// a package run in such environments).
    pub fn without_company(user_id: i64) -> Result<Self, HostError> {
        if user_id <= 0 {
            return Err(HostError::Config(format!(
                "caller ids must be positive (user {user_id})"
            )));
        }
        Ok(Self {
            user_id,
            company_id: 0,
            allowed_company_ids: Vec::new(),
        })
    }

    /// The user the call runs as.
    #[must_use]
    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// The active company; `0` for an environment without one ([`Caller::without_company`]).
    #[must_use]
    pub fn company_id(&self) -> i64 {
        self.company_id
    }

    /// Companies the user may switch to.
    #[must_use]
    pub fn allowed_company_ids(&self) -> &[i64] {
        &self.allowed_company_ids
    }
}

/// One operation of the `orm-host` interface that needs an approved capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OrmOp {
    /// `search`, `read-columns`.
    Read,
    /// `write-batch`, `write-columns`.
    Write,
    /// `create-batch`.
    Create,
    /// `unlink-batch`.
    Unlink,
    /// `call-method`, `super-call`.
    CallMethod,
}

impl OrmOp {
    /// Every operation; `OrmCapabilities::all` is built from it.
    const ALL: [Self; 5] = [
        Self::Read,
        Self::Write,
        Self::Create,
        Self::Unlink,
        Self::CallMethod,
    ];

    const fn bit(self) -> u8 {
        match self {
            Self::Read => 1,
            Self::Write => 2,
            Self::Create => 4,
            Self::Unlink => 8,
            Self::CallMethod => 16,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Read => "orm:read",
            Self::Write => "orm:write",
            Self::Create => "orm:create",
            Self::Unlink => "orm:unlink",
            Self::CallMethod => "orm:call-method",
        }
    }
}

/// The ORM operations an administrator approved for an addon. Nothing is granted by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[must_use]
pub struct OrmCapabilities(u8);

impl OrmCapabilities {
    /// No operation.
    pub const NONE: Self = Self(0);

    /// Adds `op`.
    pub const fn with(self, op: OrmOp) -> Self {
        Self(self.0 | op.bit())
    }

    /// Reads only.
    pub const fn read_only() -> Self {
        Self::NONE.with(OrmOp::Read)
    }

    /// Every operation.
    pub const fn all() -> Self {
        let mut caps = Self::NONE;
        let mut i = 0;
        while i < OrmOp::ALL.len() {
            caps = caps.with(OrmOp::ALL[i]);
            i += 1;
        }
        caps
    }

    /// Whether `op` is approved.
    #[must_use]
    pub const fn allows(self, op: OrmOp) -> bool {
        self.0 & op.bit() != 0
    }
}

/// The ORM the bridge forwards to. Every method runs as `caller` with the access rules applied;
/// the implementation (the `erp-orm` adapter, a later slice) owns transactions and security.
///
/// # Contract the adapter must keep (security review r1, dispositions in
/// )
///
/// * **Identity is `caller` only.** The bridge refuses guest arguments that carry identity or
///   authority keys (`uid`, `su`, `allowed_company_ids`, `company_id`, `company_ids`,
///   `force_company`) at the top level, in `kwargs`, or inside any `context` object, and refuses
///   the environment-changing methods (`sudo`, `with_*`, `env`, ...). The backend must still build
///   its environment from `caller` alone and ignore any context key it is handed, so a future
///   argument shape cannot widen it.
/// * **Removal needs the unlink rule.** The bridge requires the `orm:unlink` capability for every
///   x2many `delete`, and (on `write-batch`) for `unlink`, `clear` and `set`, because a one2many
///   whose inverse cascades deletes the target records; the backend must apply the target model's
///   unlink access rule per record whenever a command removes records.
/// * **Dispatch and `order` are the backend's SQL boundary.** The bridge only passes an `order`
///   clause that matches the strict grammar (`field [asc|desc]`, comma separated, bounded) and
///   names that match `[a-z0-9_.]+` / `[a-z_][a-z0-9_]*`; the backend must still map every name
///   through the registry and quote it, never splice it.
/// * **A bridge error after an effect aborts the call.** Size and count checks on a backend answer
///   run after the backend acted (for `call-method` and `create-batch`). When such a check fails
///   the bridge returns the error to the guest *and* marks itself tainted
///   ([`OrmBridge::take_tainted`]). The dispatcher must treat a tainted call as failed: roll back
///   the savepoint or transaction of the whole call (never commit partial work) and not retry
///   inside the same transaction. The backend, for its part, must roll back its own unit of work
///   when it returns `Err`.
/// * **Time.** The CPU epoch deadline only bounds guest code. The backend must bound its own
///   waits (statement timeout, lock timeout); the acceptance (follow-up)
///   adds a per-call timeout around backend futures.
/// * **Nesting.** If the backend dispatches into another addon with a clone of the bridge, the
///   clone shares the call depth counter, so `call-method` re-entry is bounded by
///   [`Limits::stack_depth`].
pub trait OrmBackend: Send + Sync {
    /// Ids matching a domain (JSON text).
    fn search<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        domain_json: &'a str,
        limit: Option<u32>,
        offset: Option<u32>,
        order: Option<&'a str>,
    ) -> BackendFuture<'a, BridgeResult<Vec<i64>>>;

    /// One typed column per requested field.
    fn read_columns<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
        fields: &'a [String],
    ) -> BackendFuture<'a, BridgeResult<RecordBatch>>;

    /// Creates one record per entry; returns the new ids in order.
    fn create_batch<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        values: &'a [Vec<(String, ScalarValue)>],
    ) -> BackendFuture<'a, BridgeResult<Vec<i64>>>;

    /// Writes the same values to every id.
    fn write_batch<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
        values: &'a [(String, ScalarValue)],
    ) -> BackendFuture<'a, BridgeResult<()>>;

    /// Writes one value per id for each column.
    fn write_columns<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
        columns: &'a [ColumnVector],
    ) -> BackendFuture<'a, BridgeResult<()>>;

    /// Deletes the records.
    fn unlink_batch<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        ids: &'a [i64],
    ) -> BackendFuture<'a, BridgeResult<()>>;

    /// Calls a model method through the full override chain.
    fn call_method<'a>(
        &'a self,
        caller: &'a Caller,
        model: &'a str,
        method: &'a str,
        ids: &'a [i64],
        args_json: &'a str,
    ) -> BackendFuture<'a, BridgeResult<String>>;

    /// Calls the implementation below the one that is running, in the method's override chain
    /// (the guest's `super()`). The backend knows which method is running and refuses another
    /// `method`. The default refuses: a backend without an override dispatcher has no chain.
    fn super_call<'a>(
        &'a self,
        _caller: &'a Caller,
        _model: &'a str,
        _method: &'a str,
        _ids: &'a [i64],
        _args_json: &'a str,
    ) -> BackendFuture<'a, BridgeResult<String>> {
        Box::pin(async {
            Err(boundary_error(
                BoundaryErrorKind::Unsupported,
                "this backend has no override dispatcher, so there is no super implementation",
            ))
        })
    }

    /// Receives a guest log event.
    fn log(&self, caller: &Caller, level: LogLevel, target: &str, message: &str);
}

/// Everything a store needs to answer a guest's `orm-host` imports.
#[derive(Clone)]
pub struct OrmBridge {
    backend: Arc<dyn OrmBackend>,
    caller: Caller,
    capabilities: OrmCapabilities,
    limits: Limits,
    /// Which methods `call-method` may name. Declared (empty) until the host says otherwise.
    callable: MethodPolicy,
    /// Nesting of `call-method`, shared by clones.
    depth: Arc<AtomicU32>,
    /// Set when a post-effect check failed; the dispatcher must roll back.
    tainted: Arc<AtomicBool>,
    /// Log events forwarded and dropped, shared by clones.
    log_events: Arc<AtomicU64>,
    dropped_logs: Arc<AtomicU64>,
}

/// Which methods a guest may name in `call-method`.
#[derive(Clone)]
enum MethodPolicy {
    /// Exactly these names, on any model (still subject to the built-in rules and capabilities).
    Declared(Arc<HashSet<String>>),
    /// Exactly these `(model, method)` pairs: a name allowed on one model never unlocks the same
    /// name on another (security review H1).
    Pairs(Arc<HashSet<(String, String)>>),
    /// Any name the built-in rules allow; an explicit opt-in.
    Any,
}

/// Decrements the call depth when a `call-method` ends, however it ends.
struct DepthGuard(Arc<AtomicU32>);

impl Drop for DepthGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for OrmBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrmBridge")
            .field("caller", &self.caller)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

/// Longest log message forwarded, in bytes; longer ones are cut at a character boundary.
const MAX_LOG_BYTES: usize = 4096;
/// Longest log target forwarded, in bytes.
const MAX_LOG_TARGET_BYTES: usize = 256;
/// Log events forwarded per bridge; later ones are counted in [`OrmBridge::dropped_logs`].
const MAX_LOG_EVENTS: u64 = 10_000;
/// Longest model, field or method name, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// Most fields in one `read-columns` call.
pub const MAX_FIELDS: usize = 1024;
/// Longest `order` clause, in bytes.
pub const MAX_ORDER_BYTES: usize = 256;
/// Most terms in an `order` clause.
pub const MAX_ORDER_TERMS: usize = 8;
/// Largest `search` offset.
pub const MAX_OFFSET: u32 = 100_000_000;

/// `text` cut to at most `max` bytes at a character boundary.
fn cut(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl OrmBridge {
    /// Binds a backend to an identity, the approved capabilities and the call limits.
    pub fn new(
        backend: Arc<dyn OrmBackend>,
        caller: Caller,
        capabilities: OrmCapabilities,
        limits: Limits,
    ) -> Self {
        Self {
            backend,
            caller,
            capabilities,
            limits,
            callable: MethodPolicy::Declared(Arc::new(HashSet::new())),
            depth: Arc::new(AtomicU32::new(0)),
            tainted: Arc::new(AtomicBool::new(false)),
            log_events: Arc::new(AtomicU64::new(0)),
            dropped_logs: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Declares the methods `call-method` may reach (the positive rule): the addon's manifest or the
    /// registry names them. **Required**: a new bridge declares none, so every `call-method` is
    /// refused until the host declares methods (or opts out with [`OrmBridge::with_any_method`]).
    /// A listed method is still subject to the built-in rules (private methods, environment
    /// changers, mutator and read families) and to its capabilities.
    pub fn with_callable_methods<I, S>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.callable =
            MethodPolicy::Declared(Arc::new(methods.into_iter().map(Into::into).collect()));
        self
    }

    /// Declares the `(model, method)` pairs `call-method` may reach: the positive rule keyed by
    /// model, so a name allowed on one model is not callable on another. Same built-in rules and
    /// capabilities as [`OrmBridge::with_callable_methods`]. The dispatcher uses this form.
    pub fn with_callable_model_methods(mut self, pairs: Arc<HashSet<(String, String)>>) -> Self {
        self.callable = MethodPolicy::Pairs(pairs);
        self
    }

    /// Opts out of the declared-method rule: any method the built-in rules allow is callable. For a
    /// host whose registry already vets which methods an addon may call; the built-in rules and the
    /// capabilities still apply.
    pub fn with_any_method(mut self) -> Self {
        self.callable = MethodPolicy::Any;
        self
    }

    /// Whether a post-effect check failed since the last call; clears the mark. The dispatcher calls
    /// it after the guest call and rolls the call back when it returns `true`.
    pub fn take_tainted(&self) -> bool {
        self.tainted.swap(false, Ordering::SeqCst)
    }

    /// Log events dropped because the per-bridge budget was spent.
    #[must_use]
    pub fn dropped_logs(&self) -> u64 {
        self.dropped_logs.load(Ordering::SeqCst)
    }

    /// The identity calls run under.
    #[must_use]
    pub fn caller(&self) -> &Caller {
        &self.caller
    }

    /// Replaces the identity for the next call into the guest (for example a different user in the
    /// same transaction). Only the host can do this.
    pub fn set_caller(&mut self, caller: Caller) {
        self.caller = caller;
    }

    fn require(&self, op: OrmOp) -> BridgeResult<()> {
        if self.capabilities.allows(op) {
            Ok(())
        } else {
            Err(boundary_error(
                BoundaryErrorKind::AccessDenied,
                format!("capability `{}` is not approved for this addon", op.name()),
            ))
        }
    }

    fn check_ids(&self, what: &str, count: usize) -> BridgeResult<()> {
        let max = self.limits.batch_ids();
        if count > max as usize {
            return Err(boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!("{what}: {count} entries exceed the batch limit of {max}"),
            ));
        }
        Ok(())
    }

    fn check_payload(&self, what: &str, bytes: usize) -> BridgeResult<()> {
        self.limits.check_payload(bytes).map_err(|_| {
            boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!("{what}: {bytes} bytes exceed the payload limit"),
            )
        })
    }

    /// A name that is not empty, within [`MAX_NAME_BYTES`] and made only of `allowed` characters.
    fn check_name(what: &str, value: &str, allowed: fn(char) -> bool) -> BridgeResult<()> {
        if value.is_empty() {
            return Err(boundary_error(
                BoundaryErrorKind::InvalidArgument,
                format!("{what} must not be empty"),
            ));
        }
        if value.len() > MAX_NAME_BYTES {
            return Err(boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!(
                    "{what} is {} bytes, the limit is {MAX_NAME_BYTES}",
                    value.len()
                ),
            ));
        }
        if !value.chars().all(allowed) {
            return Err(boundary_error(
                BoundaryErrorKind::InvalidArgument,
                format!(
                    "{what} `{}` has characters outside its grammar",
                    cut(value, 64)
                ),
            ));
        }
        Ok(())
    }

    /// Model name: `[a-z0-9_.]+`.
    fn check_model(model: &str) -> BridgeResult<()> {
        Self::check_name("model", model, |c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'
        })
    }

    /// Field name: `[a-z_][a-z0-9_]*`.
    fn check_field(field: &str) -> BridgeResult<()> {
        Self::check_name("field", field, is_field_char)?;
        if field.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(boundary_error(
                BoundaryErrorKind::InvalidArgument,
                format!("field `{field}` starts with a digit"),
            ));
        }
        Ok(())
    }

    /// Ids of a call: within the batch limit, positive and without duplicates.
    fn check_id_list(&self, what: &str, ids: &[i64]) -> BridgeResult<()> {
        self.check_ids(what, ids.len())?;
        let mut seen = HashSet::with_capacity(ids.len());
        for id in ids {
            if *id <= 0 {
                return Err(boundary_error(
                    BoundaryErrorKind::InvalidArgument,
                    format!("{what}: id {id} is not a stored record id"),
                ));
            }
            if !seen.insert(*id) {
                return Err(boundary_error(
                    BoundaryErrorKind::InvalidArgument,
                    format!("{what}: id {id} appears more than once"),
                ));
            }
        }
        Ok(())
    }

    /// Field names of a values row, and the unlink capability when the row removes records.
    fn check_row(&self, row: &[(String, ScalarValue)], creating: bool) -> BridgeResult<()> {
        let mut unlink = false;
        for (field, value) in row {
            Self::check_field(field)?;
            match value {
                ScalarValue::ValX2many(cmds) => {
                    unlink |= cmds.iter().any(|c| removes_records(c, creating));
                }
                // A plain id list on write replaces every link, like `set`.
                ScalarValue::ValIds(_) => unlink |= !creating,
                _ => {}
            }
        }
        if unlink {
            self.require(OrmOp::Unlink)?;
        }
        Ok(())
    }

    /// An `order` clause: `field [asc|desc]` terms, comma separated.
    fn check_order(order: &str) -> BridgeResult<()> {
        let bad = |why: String| boundary_error(BoundaryErrorKind::InvalidArgument, why);
        if order.len() > MAX_ORDER_BYTES {
            return Err(boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!(
                    "order is {} bytes, the limit is {MAX_ORDER_BYTES}",
                    order.len()
                ),
            ));
        }
        let terms: Vec<&str> = order.split(',').collect();
        if terms.len() > MAX_ORDER_TERMS {
            return Err(boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!(
                    "order has {} terms, the limit is {MAX_ORDER_TERMS}",
                    terms.len()
                ),
            ));
        }
        for term in terms {
            let mut words = term.split_whitespace();
            let field = words
                .next()
                .ok_or_else(|| bad("order has an empty term".into()))?;
            Self::check_field(field)?;
            match words.next() {
                None => {}
                Some(d) if d.eq_ignore_ascii_case("asc") || d.eq_ignore_ascii_case("desc") => {}
                Some(d) => {
                    return Err(bad(format!(
                        "order direction `{}` is not asc or desc",
                        cut(d, 16)
                    )));
                }
            }
            if words.next().is_some() {
                return Err(bad("order term has more than field and direction".into()));
            }
        }
        Ok(())
    }

    /// Marks the call as one the dispatcher must roll back, and builds the error.
    fn post_effect_error(&self, kind: BoundaryErrorKind, message: String) -> BoundaryError {
        self.tainted.store(true, Ordering::SeqCst);
        boundary_error(kind, message)
    }

    /// `search`.
    pub async fn search(
        &self,
        model: &str,
        domain_json: &str,
        limit: Option<u32>,
        offset: Option<u32>,
        order: Option<&str>,
    ) -> BridgeResult<Vec<i64>> {
        self.require(OrmOp::Read)?;
        Self::check_model(model)?;
        self.check_payload("domain", domain_json.len())?;
        if let Some(offset) = offset
            && offset > MAX_OFFSET
        {
            return Err(boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!("offset {offset} exceeds the limit of {MAX_OFFSET}"),
            ));
        }
        if let Some(order) = order {
            Self::check_order(order)?;
        }
        // The backend never loads more than one batch: an absent or larger limit is lowered.
        let max = self.limits.batch_ids();
        let limit = Some(limit.map_or(max, |l| l.min(max)));
        let ids = self
            .backend
            .search(&self.caller, model, domain_json, limit, offset, order)
            .await?;
        // A search has no effect: an over-limit answer needs no rollback.
        self.check_ids("search result", ids.len())?;
        Ok(ids)
    }

    /// `read-columns`.
    pub async fn read_columns(
        &self,
        model: &str,
        ids: &[i64],
        fields: &[String],
    ) -> BridgeResult<RecordBatch> {
        self.require(OrmOp::Read)?;
        Self::check_model(model)?;
        self.check_id_list("read ids", ids)?;
        if fields.is_empty() {
            return Err(boundary_error(
                BoundaryErrorKind::InvalidArgument,
                "read-columns needs at least one field",
            ));
        }
        if fields.len() > MAX_FIELDS {
            return Err(boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!("{} fields exceed the limit of {MAX_FIELDS}", fields.len()),
            ));
        }
        for field in fields {
            Self::check_field(field)?;
        }
        let batch = self
            .backend
            .read_columns(&self.caller, model, ids, fields)
            .await?;
        let short = batch
            .columns
            .iter()
            .find(|c| crate::column::len(&c.data) != ids.len());
        if batch.ids.len() != ids.len() || batch.columns.len() != fields.len() || short.is_some() {
            return Err(boundary_error(
                BoundaryErrorKind::InternalError,
                format!(
                    "backend answered {} ids and {} columns for {} ids and {} fields of {model}",
                    batch.ids.len(),
                    batch.columns.len(),
                    ids.len(),
                    fields.len()
                ),
            ));
        }
        // A read has no effect: an answer over the payload cap is refused without a rollback.
        let answer = batch
            .columns
            .iter()
            .map(|c| crate::column::bytes(&c.data).saturating_add(c.field.len()))
            .fold(batch.ids.len().saturating_mul(8), usize::saturating_add);
        self.check_payload("read answer", answer)?;
        Ok(batch)
    }

    /// `create-batch`.
    pub async fn create_batch(
        &self,
        model: &str,
        values: &[Vec<(String, ScalarValue)>],
    ) -> BridgeResult<Vec<i64>> {
        self.require(OrmOp::Create)?;
        Self::check_model(model)?;
        self.check_ids("create rows", values.len())?;
        for row in values {
            self.check_row(row, true)?;
        }
        let bytes = values
            .iter()
            .map(|r| row_bytes(r))
            .fold(0usize, usize::saturating_add);
        self.check_payload("create values", bytes)?;
        let created = self
            .backend
            .create_batch(&self.caller, model, values)
            .await?;
        if created.len() != values.len() {
            return Err(self.post_effect_error(
                BoundaryErrorKind::InternalError,
                format!(
                    "backend created {} records for {} rows of {model}",
                    created.len(),
                    values.len()
                ),
            ));
        }
        Ok(created)
    }

    /// `write-batch`.
    pub async fn write_batch(
        &self,
        model: &str,
        ids: &[i64],
        values: &[(String, ScalarValue)],
    ) -> BridgeResult<()> {
        self.require(OrmOp::Write)?;
        Self::check_model(model)?;
        self.check_id_list("write ids", ids)?;
        self.check_row(values, false)?;
        self.check_payload("write values", row_bytes(values))?;
        self.backend
            .write_batch(&self.caller, model, ids, values)
            .await
    }

    /// `write-columns`: every column must hold one value per id.
    pub async fn write_columns(
        &self,
        model: &str,
        ids: &[i64],
        columns: &[ColumnVector],
    ) -> BridgeResult<()> {
        self.require(OrmOp::Write)?;
        Self::check_model(model)?;
        self.check_id_list("write ids", ids)?;
        self.check_ids("write columns", columns.len())?;
        let mut names = HashSet::with_capacity(columns.len());
        let mut bytes = 0usize;
        for column in columns {
            Self::check_field(&column.field)?;
            if !names.insert(column.field.as_str()) {
                return Err(boundary_error(
                    BoundaryErrorKind::InvalidArgument,
                    format!("column `{}` appears more than once", column.field),
                ));
            }
            let len = crate::column::len(&column.data);
            if len != ids.len() {
                return Err(boundary_error(
                    BoundaryErrorKind::InvalidArgument,
                    format!(
                        "column `{}` holds {len} values for {} ids",
                        column.field,
                        ids.len()
                    ),
                ));
            }
            // A `col-ids` column replaces the links of x2many records; on a cascading one2many
            // that deletes the dropped targets.
            if matches!(column.data, ColumnData::ColIds(_)) {
                self.require(OrmOp::Unlink)?;
            }
            bytes = bytes.saturating_add(crate::column::bytes(&column.data));
        }
        self.check_payload("write columns", bytes)?;
        self.backend
            .write_columns(&self.caller, model, ids, columns)
            .await
    }

    /// `unlink-batch`.
    pub async fn unlink_batch(&self, model: &str, ids: &[i64]) -> BridgeResult<()> {
        self.require(OrmOp::Unlink)?;
        Self::check_model(model)?;
        self.check_id_list("unlink ids", ids)?;
        self.backend.unlink_batch(&self.caller, model, ids).await
    }

    /// Enforces the method rules of `call-method` (see [`method_rule`]).
    fn check_method(&self, model: &str, method: &str) -> BridgeResult<()> {
        Self::check_name("method", method, |c| c.is_ascii_alphanumeric() || c == '_')?;
        match method_rule(method) {
            MethodRule::Deny(why) => {
                return Err(boundary_error(
                    BoundaryErrorKind::AccessDenied,
                    format!("method `{method}` is not callable from a guest: {why}"),
                ));
            }
            MethodRule::Needs(ops) => {
                for op in ops {
                    self.require(*op)?;
                }
            }
            MethodRule::Open => {}
        }
        let allowed = match &self.callable {
            MethodPolicy::Declared(names) => names.contains(method),
            MethodPolicy::Pairs(pairs) => pairs.contains(&(model.to_owned(), method.to_owned())),
            MethodPolicy::Any => true,
        };
        if !allowed {
            return Err(boundary_error(
                BoundaryErrorKind::AccessDenied,
                format!("method `{method}` is not declared callable on `{model}` for this addon"),
            ));
        }
        Ok(())
    }

    /// `call-method`.
    pub async fn call_method(
        &self,
        model: &str,
        method: &str,
        ids: &[i64],
        args_json: &str,
    ) -> BridgeResult<String> {
        self.require(OrmOp::CallMethod)?;
        Self::check_model(model)?;
        self.check_method(model, method)?;
        self.check_id_list("call ids", ids)?;
        self.check_payload("call arguments", args_json.len())?;
        check_args(args_json)?;
        // Re-entry is bounded: a backend that dispatches into other addons nests call-method.
        let max = self.limits.stack_depth();
        if self.depth.fetch_add(1, Ordering::SeqCst) >= max {
            self.depth.fetch_sub(1, Ordering::SeqCst);
            return Err(boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!("call-method nesting exceeds the depth limit of {max}"),
            ));
        }
        let _depth = DepthGuard(Arc::clone(&self.depth));
        let result = self
            .backend
            .call_method(&self.caller, model, method, ids, args_json)
            .await?;
        // The method already ran: this error aborts the call and the dispatcher rolls it back.
        if let Err(err) = self.limits.check_payload(result.len()) {
            return Err(self.post_effect_error(
                BoundaryErrorKind::ResourceExhausted,
                format!("call result: {err}"),
            ));
        }
        Ok(result)
    }

    /// `super-call`: the implementation below the running one. The method must be named like the
    /// one running (the backend checks that, since only it knows); the arguments are held to the
    /// same limits and identity rules as `call-method`, and the method rules and the declared list
    /// do not apply because the guest cannot choose which method runs.
    pub async fn super_call(
        &self,
        model: &str,
        method: &str,
        ids: &[i64],
        args_json: &str,
    ) -> BridgeResult<String> {
        self.require(OrmOp::CallMethod)?;
        Self::check_name("method", method, |c| c.is_ascii_alphanumeric() || c == '_')?;
        Self::check_model(model)?;
        // `super()` runs the layers below the running method, which may write or delete: it needs
        // what `call-method` of that method name would need (the backend pins the name to the
        // running one). Only the capability mapping applies; the deny rules and the declared list
        // do not, because the guest cannot choose which method runs.
        if let MethodRule::Needs(ops) = method_rule(method) {
            for op in ops {
                self.require(*op)?;
            }
        }
        self.check_id_list("super ids", ids)?;
        self.check_payload("super arguments", args_json.len())?;
        check_args(args_json)?;
        let max = self.limits.stack_depth();
        if self.depth.fetch_add(1, Ordering::SeqCst) >= max {
            self.depth.fetch_sub(1, Ordering::SeqCst);
            return Err(boundary_error(
                BoundaryErrorKind::ResourceExhausted,
                format!("super-call nesting exceeds the depth limit of {max}"),
            ));
        }
        let _depth = DepthGuard(Arc::clone(&self.depth));
        let result = self
            .backend
            .super_call(&self.caller, model, method, ids, args_json)
            .await?;
        if let Err(err) = self.limits.check_payload(result.len()) {
            return Err(self.post_effect_error(
                BoundaryErrorKind::ResourceExhausted,
                format!("super result: {err}"),
            ));
        }
        Ok(result)
    }

    /// `log`: forwards a bounded message, at most [`MAX_LOG_EVENTS`] per bridge. Control characters
    /// are replaced so a guest cannot forge log lines. Events past the budget are counted in
    /// [`OrmBridge::dropped_logs`]; the first one past it is replaced by one warning.
    pub fn log(&self, level: LogLevel, target: &str, message: &str) {
        let n = self.log_events.fetch_add(1, Ordering::SeqCst);
        if n < MAX_LOG_EVENTS {
            self.backend.log(
                &self.caller,
                level,
                &printable(cut(target, MAX_LOG_TARGET_BYTES)),
                &printable(cut(message, MAX_LOG_BYTES)),
            );
            return;
        }
        self.dropped_logs.fetch_add(1, Ordering::SeqCst);
        if n == MAX_LOG_EVENTS {
            self.backend.log(
                &self.caller,
                LogLevel::Warn,
                "erp_wasm_host",
                "guest log budget exhausted; further events are dropped",
            );
        }
    }
}

fn is_field_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
}

/// `text` with control characters replaced by a space.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Whether an x2many command can remove target records. On `write` that is every command but
/// `link` (a one2many whose inverse cascades deletes on `unlink`, `clear` and `set`); on `create`
/// there are no earlier links, so only `delete` removes.
fn removes_records(cmd: &X2manyCommand, creating: bool) -> bool {
    match cmd {
        X2manyCommand::Delete(_) => true,
        X2manyCommand::Unlink(_) | X2manyCommand::Clear | X2manyCommand::Set(_) => !creating,
        X2manyCommand::Link(_) => false,
    }
}

/// What `call-method` demands for a method name.
enum MethodRule {
    /// No built-in restriction.
    Open,
    /// These capabilities, besides `orm:call-method`.
    Needs(&'static [OrmOp]),
    /// Never callable from a guest.
    Deny(&'static str),
}

/// Built-in `call-method` rules, by method name (ASCII case-insensitive).
///
/// The bridge has no registry, so this is the floor under the required positive rule
/// ([`OrmBridge::with_callable_methods`]): private (`_`) methods and internals are denied; the
/// methods that change identity or environment are denied; every known mutator family needs the
/// capability of what it does. Value-carrying mutators also need `orm:unlink`, because their
/// arguments are JSON the bridge cannot inspect and may hold x2many remove commands.
fn method_rule(method: &str) -> MethodRule {
    let m = method.to_ascii_lowercase();
    if m.starts_with('_') {
        return MethodRule::Deny("private method");
    }
    if m.starts_with("with_") || matches!(m.as_str(), "sudo" | "env" | "browse" | "as_user") {
        return MethodRule::Deny("changes the identity or the environment");
    }
    if m.ends_with("_multi")
        || m.starts_with("flush")
        || m.starts_with("invalidate")
        || m.starts_with("clear_cache")
    {
        return MethodRule::Deny("bulk or cache internals");
    }
    const READ: &[OrmOp] = &[OrmOp::Read];
    if matches!(
        m.as_str(),
        "read"
            | "name_search"
            | "name_get"
            | "export_data"
            | "fields_get"
            | "default_get"
            | "exists"
            | "web_read"
            | "web_search_read"
            | "web_name_search"
    ) || m.starts_with("search")
        || m.starts_with("read_")
        || m.starts_with("web_read")
    {
        return MethodRule::Needs(READ);
    }
    if m.starts_with("web_") {
        // `web_save`, `web_resequence`, ...: they read and write through one call.
        return MethodRule::Needs(&[OrmOp::Read, OrmOp::Write, OrmOp::Create, OrmOp::Unlink]);
    }
    const CREATE: &[OrmOp] = &[OrmOp::Create, OrmOp::Unlink];
    const WRITE: &[OrmOp] = &[OrmOp::Write, OrmOp::Unlink];
    const LOAD: &[OrmOp] = &[OrmOp::Create, OrmOp::Write, OrmOp::Unlink];
    match m.as_str() {
        "create" | "copy" | "copy_data" | "copy_translations" => MethodRule::Needs(CREATE),
        "name_create" => MethodRule::Needs(&[OrmOp::Create]),
        "write" | "update" => MethodRule::Needs(WRITE),
        "toggle_active" | "action_archive" | "action_unarchive" => {
            MethodRule::Needs(&[OrmOp::Write])
        }
        "load" | "import_data" => MethodRule::Needs(LOAD),
        "unlink" => MethodRule::Needs(&[OrmOp::Unlink]),
        _ => MethodRule::Open,
    }
}

/// Keys that name identity or authority.
pub(crate) const FORBIDDEN_KEYS: [&str; 6] = [
    "uid",
    "su",
    "allowed_company_ids",
    "company_id",
    "company_ids",
    "force_company",
];

fn forbidden_key(map: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    map.keys()
        .find(|k| FORBIDDEN_KEYS.iter().any(|f| k.eq_ignore_ascii_case(f)))
        .map(String::as_str)
}

/// Validates the guest's JSON arguments: well-formed, and no identity or authority key at the top
/// level, in `kwargs`, or in any object under a `context` key.
fn check_args(args_json: &str) -> BridgeResult<()> {
    let value: serde_json::Value = serde_json::from_str(args_json).map_err(|e| {
        boundary_error(
            BoundaryErrorKind::InvalidArgument,
            format!("call arguments are not valid JSON: {e}"),
        )
    })?;
    let refuse = |key: &str| {
        boundary_error(
            BoundaryErrorKind::AccessDenied,
            format!("call arguments carry the identity key `{key}`; identity comes from the host"),
        )
    };
    let mut stack = vec![(&value, true)];
    while let Some((v, top)) = stack.pop() {
        match v {
            serde_json::Value::Object(map) => {
                // Top-level keys and the keys of `kwargs` are method arguments; any `context`
                // object, wherever it sits, is an environment.
                if top && let Some(key) = forbidden_key(map) {
                    return Err(refuse(key));
                }
                for (k, child) in map {
                    if k == "context"
                        && let serde_json::Value::Object(ctx) = child
                        && let Some(key) = forbidden_key(ctx)
                    {
                        return Err(refuse(key));
                    }
                    let kwargs = top && k == "kwargs";
                    if kwargs
                        && let serde_json::Value::Object(kw) = child
                        && let Some(key) = forbidden_key(kw)
                    {
                        return Err(refuse(key));
                    }
                    stack.push((child, false));
                }
            }
            serde_json::Value::Array(items) => {
                stack.extend(items.iter().map(|i| (i, false)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Bytes a row of values carries in strings, JSON and binary payloads.
fn row_bytes(row: &[(String, ScalarValue)]) -> usize {
    row.iter()
        .map(|(field, value)| {
            field.len().saturating_add(match value {
                ScalarValue::ValStr(s) | ScalarValue::ValDecimal(s) | ScalarValue::ValJson(s) => {
                    s.len()
                }
                ScalarValue::ValBytes(b) => b.len(),
                ScalarValue::ValIds(ids) => ids.len().saturating_mul(8),
                ScalarValue::ValX2many(cmds) => cmds
                    .iter()
                    .map(|c| match c {
                        X2manyCommand::Set(ids) => ids.len().saturating_mul(8).saturating_add(16),
                        _ => 16,
                    })
                    .fold(0usize, usize::saturating_add),
                ScalarValue::ValNull
                | ScalarValue::ValBool(_)
                | ScalarValue::ValInt(_)
                | ScalarValue::ValFloat(_)
                | ScalarValue::ValDate(_)
                | ScalarValue::ValDatetime(_)
                | ScalarValue::ValDateRange(_)
                | ScalarValue::ValDatetimeRange(_)
                | ScalarValue::ValId(_) => 16,
            })
        })
        .fold(0usize, usize::saturating_add)
}
