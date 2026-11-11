//! Bounded native selection metadata (amendment A).
use std::sync::{Arc, Mutex, MutexGuard};

use erp_domain::Domain;
use erp_fields::{Error, ErrorKind, FieldType, Result, Value};
use erp_registry::{FieldId, ModelId};

use crate::dispatch::{Chain, MethodDispatchTarget};
use crate::{BoxFuture, Env, Records, Transaction};

/// Positive tightening-only admission limits for one metadata operation.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SelectionLimits {
    pairs: usize,
    bytes: usize,
    depth: usize,
    work: usize,
}
impl Default for SelectionLimits {
    fn default() -> Self {
        Self {
            pairs: 4096,
            bytes: 1048576,
            depth: 16,
            work: 8192,
        }
    }
}
impl SelectionLimits {
    /// Lower the hard caps. Zero and values above any hard cap are Programming errors.
    pub fn new(pairs: usize, bytes: usize, depth: usize, work: usize) -> Result<Self> {
        if pairs == 0
            || pairs > 4096
            || bytes == 0
            || bytes > 1048576
            || depth == 0
            || depth > 16
            || work == 0
            || work > 8192
        {
            return Err(Error::programming(
                "selection limits must be positive and at most 4096 pairs, 1048576 bytes, depth 16 and 8192 work units",
            ));
        }
        Ok(Self {
            pairs,
            bytes,
            depth,
            work,
        })
    }
}

#[derive(Debug)]
struct Identity {
    model: ModelId,
    field: FieldId,
    name: String,
    method: String,
}
#[derive(Debug)]
struct Budget {
    remaining: SelectionLimits,
    active: Vec<Identity>,
    failure: Option<Arc<Error>>,
}
impl Budget {
    fn check(&self) -> Result<()> {
        match &self.failure {
            Some(error) => Err(retained_error(error)),
            None => Ok(()),
        }
    }
    fn error(&mut self, kind: ErrorKind, detail: &str) -> Error {
        if self.failure.is_none() {
            let message = match self.active.last() {
                Some(id) => format!("selection '{}' provider '{}': {detail}", id.name, id.method),
                None => format!("selection: {detail}"),
            };
            self.failure = Some(Arc::new(Error::new(kind, message)));
        }
        match &self.failure {
            Some(error) => retained_error(error),
            None => Error::new(ErrorKind::Internal, "selection failure was not retained"),
        }
    }
    fn charge(&mut self, work: usize) -> Result<()> {
        self.check()?;
        self.remaining.work = self
            .remaining
            .work
            .checked_sub(work)
            .ok_or_else(|| self.error(ErrorKind::ResourceExhausted, "work limit exceeded"))?;
        Ok(())
    }
}

// Re-raising a caught error preserves retry markers, diagnostic context and the original
// source chain as well as kind/message. Keep one original error shared by every refusal.
fn retained_error(original: &Arc<Error>) -> Error {
    let mut error = Error::new(original.kind(), original.message())
        .with_sqlstate(original.sqlstate().map(str::to_owned))
        .with_source(Arc::clone(original));
    if let Some(constraint) = original.constraint() {
        error = error.with_constraint(constraint);
    }
    if let Some(condition) = original.condition() {
        error = error.with_condition(condition);
    }
    if let Some(context) = original.context() {
        error = error.with_context(context.clone());
    }
    error
}

fn locked(budget: &Mutex<Budget>) -> Result<MutexGuard<'_, Budget>> {
    budget
        .lock()
        .map_err(|_| Error::new(ErrorKind::Internal, "selection budget lock poisoned"))
}

struct ResolutionGuard {
    budget: Arc<Mutex<Budget>>,
}
impl Drop for ResolutionGuard {
    fn drop(&mut self) {
        // A poisoned lock remains a loud failure at the outer operation check.
        if let Ok(mut budget) = self.budget.lock() {
            budget.active.pop();
        }
    }
}

/// Specialized native provider entry. Unlike MethodFn it cannot borrow a Transaction.
pub type SelectionFn = for<'a, 'tx> fn(
    &'a mut SelectionContext<'tx>,
    &'a Records,
    SelectionSuper,
) -> BoxFuture<'a, Result<()>>;

/// Remaining specialized provider implementations in native MRO order.
/// Only generated provider overrides receive this value.
#[non_exhaustive]
pub struct SelectionSuper {
    chain: Chain,
    next: usize,
    model: ModelId,
}
impl SelectionSuper {
    /// Invoke the next specialized implementation, sharing budgets and the output sink.
    /// Refuses exhausted chains, another model, ordinary signatures, and prior caught errors.
    pub fn call<'a, 'tx>(
        self,
        ctx: &'a mut SelectionContext<'tx>,
        records: &'a Records,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if records.model_id() != self.model || records.env() != &ctx.env {
                return Err(ctx.error(
                    ErrorKind::Programming,
                    "super called with a different model or environment",
                )?);
            }
            invoke(self.chain, self.next, ctx, records).await
        })
    }
}

fn invoke<'a, 'tx>(
    chain: Chain,
    index: usize,
    ctx: &'a mut SelectionContext<'tx>,
    records: &'a Records,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        ctx.charge(1)?;
        let imp = match chain.targets().get(index) {
            Some(MethodDispatchTarget::Native(imp)) => *imp,
            Some(MethodDispatchTarget::Wasm(_)) => {
                return Err(ctx.error(
                    ErrorKind::Programming,
                    "selection providers are native only; a WebAssembly layer cannot provide one",
                )?);
            }
            None => {
                return Err(ctx.error(ErrorKind::Programming, "specialized super chain exhausted")?);
            }
        };
        let Some(entry) = (imp.entry)().downcast_ref::<SelectionFn>() else {
            return Err(ctx.error(
                ErrorKind::Programming,
                "provider requires the specialized SelectionContext signature",
            )?);
        };
        let entry = *entry;
        let sup = SelectionSuper {
            chain,
            next: index + 1,
            model: records.model_id(),
        };
        let result = entry(ctx, records, sup).await;
        ctx.retain(result)?;
        locked(&ctx.budget)?.check()
    })
}

/// Read-only native provider capability with a checked output sink.
///
/// No transaction accessor, raw SQL, mutation or commit hooks are exposed. Native Rust is
/// trusted executable code; charge cooperative work before allocating/calculating items.
/// This is not a sandbox for arbitrary Rust CPU, allocation, filesystem or network access.
///
/// Transaction internals cannot escape:
/// ```compile_fail
/// fn escape(ctx: &mut erp_orm::SelectionContext<'_>) { let tx = &mut ctx.tx; }
/// ```
/// Raw SQL (including writing functions) is not a capability:
/// ```compile_fail
/// async fn sql(ctx: &mut erp_orm::SelectionContext<'_>) { ctx.query("SELECT writing_function()", &[]).await; }
/// ```
/// Commit callbacks cannot be registered:
/// ```compile_fail
/// fn hook(ctx: &mut erp_orm::SelectionContext<'_>) { ctx.after_commit(|| {}); }
/// ```
/// Records cannot be mutated without a transaction:
/// ```compile_fail
/// async fn write(ctx: &mut erp_orm::SelectionContext<'_>, records: erp_orm::Records) {
///     records.write(ctx, erp_orm::RawVals::new()).await;
/// }
/// ```
#[non_exhaustive]
pub struct SelectionContext<'tx> {
    tx: &'tx mut Transaction,
    env: Env,
    budget: Arc<Mutex<Budget>>,
    output: Vec<(Arc<str>, Arc<str>)>,
}
impl<'tx> SelectionContext<'tx> {
    pub(crate) fn new(tx: &'tx mut Transaction, env: Env) -> Self {
        let remaining = tx.config().selection_limits;
        Self {
            tx,
            env,
            budget: Arc::new(Mutex::new(Budget {
                remaining,
                active: Vec::new(),
                failure: None,
            })),
            output: Vec::new(),
        }
    }
    /// Inspect the caller's environment. No authority or transaction is minted from it.
    pub fn env(&self) -> &Env {
        &self.env
    }
    fn error(&self, kind: ErrorKind, detail: &str) -> Result<Error> {
        Ok(locked(&self.budget)?.error(kind, detail))
    }
    fn retain<T>(&self, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => {
                locked(&self.budget)?.check()?;
                Ok(value)
            }
            Err(error) => {
                let mut budget = locked(&self.budget)?;
                if budget.failure.is_none() {
                    budget.failure = Some(Arc::new(error));
                }
                budget.check()?;
                Err(Error::internal("selection failure was not retained"))
            }
        }
    }
    /// Charge trusted native producer work before performing it. Refusals are sticky.
    pub fn charge(&mut self, work: usize) -> Result<()> {
        locked(&self.budget)?.charge(work)
    }
    /// Append one pair after checking uniqueness, cumulative bytes, pairs and work, before copy.
    /// Duplicate keys are Programming errors; exhausted limits are ResourceExhausted errors.
    pub fn emit(&mut self, key: &str, label: &str) -> Result<()> {
        let mut budget = locked(&self.budget)?;
        budget.check()?;
        if self.output.iter().any(|(k, _)| k.as_ref() == key) {
            return Err(budget.error(ErrorKind::Programming, &format!("duplicate key '{key}'")));
        }
        budget.charge(1)?;
        let Some(bytes) = key
            .len()
            .checked_add(label.len())
            .filter(|n| *n <= budget.remaining.bytes)
        else {
            return Err(budget.error(ErrorKind::ResourceExhausted, "byte limit exceeded"));
        };
        if budget.remaining.pairs == 0 {
            return Err(budget.error(ErrorKind::ResourceExhausted, "pair limit exceeded"));
        }
        budget.remaining.pairs -= 1;
        budget.remaining.bytes -= bytes;
        self.output.push((Arc::from(key), Arc::from(label)));
        Ok(())
    }
    /// Resolve another method-backed field into the same sink, retaining cumulative limits.
    /// Missing fields/providers, ordinary signatures, repeated identities and depth overflow refuse.
    pub fn resolve<'a>(&'a mut self, model: &'a str, field: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let result = self.resolve_inner(model, field).await;
            self.retain(result)
        })
    }
    async fn resolve_inner(&mut self, model: &str, field: &str) -> Result<()> {
        locked(&self.budget)?.check()?;
        let records = self.env.records(model)?;
        self.tx.check_records(&records)?;
        self.tx
            .check_model(&self.env, records.model_id(), crate::Operation::Read)?;
        self.tx
            .check_field_names(&self.env, records.model_id(), [field], crate::FieldOp::Read)?;
        let spec = records.spec();
        let fs = spec.field(field).ok_or_else(|| {
            Error::programming(format!("Invalid selection field '{model}.{field}'"))
        })?;
        let method = match &fs.ty {
            FieldType::Selection {
                method: Some(method),
                ..
            } => method.as_str(),
            _ => {
                return Err(Error::programming(format!(
                    "'{model}.{field}' is not a method-backed selection"
                )));
            }
        };
        {
            let mut budget = locked(&self.budget)?;
            budget.active.push(Identity {
                model: records.model_id(),
                field: fs.id,
                name: format!("{model}.{field}"),
                method: method.to_owned(),
            });
        }
        let _resolution = ResolutionGuard {
            budget: Arc::clone(&self.budget),
        };
        {
            let mut budget = locked(&self.budget)?;
            let previous = budget
                .active
                .iter()
                .rev()
                .skip(1)
                .any(|id| id.model == records.model_id() && id.field == fs.id);
            if previous {
                return Err(budget.error(ErrorKind::Programming, "recursive provider resolution"));
            }
            if budget.active.len() > budget.remaining.depth {
                return Err(budget.error(ErrorKind::ResourceExhausted, "depth limit exceeded"));
            }
        }
        let Some(chain) = self.tx.dispatch().chain(records.model_id(), method) else {
            return Err(self.error(ErrorKind::Programming, "missing specialized provider")?);
        };
        invoke(chain, 0, self, &records).await
    }
    /// Project an authorized stored-text catalog. SQL bounds candidates and payload before
    /// the driver receives strings. Nulls and overflow refuse; no computed callbacks are run.
    /// Domain/order fields must be stored scalar fields; relational/hierarchy requests refuse.
    pub async fn catalog(
        &mut self,
        model: &str,
        domain: &Domain,
        key_field: &str,
        label_field: &str,
        order: &str,
    ) -> Result<()> {
        let result = self
            .catalog_inner(model, domain, key_field, label_field, order)
            .await;
        self.retain(result)
    }
    async fn catalog_inner(
        &mut self,
        model: &str,
        domain: &Domain,
        key_field: &str,
        label_field: &str,
        order: &str,
    ) -> Result<()> {
        self.charge(1)?;
        let records = self.env.records(model);
        let result = match records {
            Ok(records) => {
                let limits = locked(&self.budget)?.remaining;
                let budget = Arc::clone(&self.budget);
                let mut charge = move |work| locked(&budget)?.charge(work);
                let result = records
                    .selection_catalog(
                        self.tx,
                        crate::search::CatalogRequest {
                            domain,
                            fields: [key_field, label_field],
                            order,
                            pairs: limits.pairs,
                            bytes: limits.bytes,
                            charge: &mut charge,
                        },
                    )
                    .await;
                match result {
                    Ok(rows) => {
                        for row in rows {
                            self.charge(1)?;
                            let overflow: bool = row.try_get(2).map_err(decode_error)?;
                            if overflow {
                                return Err(self.error(
                                    ErrorKind::ResourceExhausted,
                                    "catalog output limit exceeded",
                                )?);
                            }
                            let key: Option<&str> = row.try_get(0).map_err(decode_error)?;
                            let label: Option<&str> = row.try_get(1).map_err(decode_error)?;
                            let (Some(key), Some(label)) = (key, label) else {
                                return Err(self.error(
                                    ErrorKind::Programming,
                                    "catalog key and label must be non-null text",
                                )?);
                            };
                            self.emit(key, label)?;
                        }
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        self.retain(result)
    }
    pub(crate) fn take_output(&mut self) -> Result<Value> {
        locked(&self.budget)?.check()?;
        Ok(Value::list(
            std::mem::take(&mut self.output)
                .into_iter()
                .map(|(key, label)| Value::list([Value::Str(key), Value::Str(label)])),
        ))
    }
}

fn decode_error(error: tokio_postgres::Error) -> Error {
    Error::new(
        ErrorKind::Database,
        "could not decode a selection catalog row",
    )
    .with_source(error)
}
