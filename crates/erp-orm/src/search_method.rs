//! Generic bounded search-domain capability for nonstored fields.
//!
//! A nonstored field may declare `search_method = "_search_has_vat"`.
//! A specialized synchronous native search-domain entry, registered through the normal model/addon
//! MRO, receives an immutable validated [`FieldSearchRequest`], a [`FieldSearchContext`] (read-only
//! environment, original model/field, cooperative budget charging), and its next-provider
//! continuation [`FieldSearchSuper`]. It returns `Result<Domain>`.
//!
//! The domain preparation stage expands supported nonstored leaves into returned domains
//! **before** SQL planning; related field paths preserve their relational quantifier/path prefix.
//! All resulting leaves go through ordinary model/field/record/company checks and normalization.

use std::sync::{Arc, Mutex, Weak};

use erp_domain::{Domain, DomainKind, Operator, Value};
use erp_fields::{Error, FieldType, Result};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};
use erp_sql::reference::ReferenceQuery;
use rustc_hash::FxHashMap;

use crate::Transaction;
use crate::env::Env;
use crate::field_access::FieldOp;

/// Weak operation state: root compilers own the budget; later queries start fresh.
#[derive(Default)]
pub(crate) struct SearchState(Mutex<Weak<Mutex<SearchBudget>>>);

pub(crate) fn scope(tx: &Transaction) -> Result<Arc<Mutex<SearchBudget>>> {
    let mut slot = tx
        .search_state
        .0
        .lock()
        .map_err(|_| Error::programming("search_method scope lock poisoned"))?;
    if let Some(active) = slot.upgrade() {
        return Ok(active);
    }
    let budget = Arc::new(Mutex::new(SearchBudget::new(
        tx.shared.config.search_limits,
    )?));
    *slot = Arc::downgrade(&budget);
    Ok(budget)
}

#[derive(Clone, Copy)]
struct ExpandContext<'a> {
    tx: &'a Transaction,
    env: &'a Env,
    field_checks: bool,
    external: bool,
    programming_context: Option<&'a Arc<str>>,
}

impl ExpandContext<'_> {
    /// Classify only errors generated while validating this domain, never callback refusals.
    fn invalid(&self, message: String) -> Error {
        if let Some(context) = self.programming_context {
            Error::programming(format!("{context}: {message}"))
        } else if self.external {
            Error::validation(message)
        } else {
            Error::programming(message)
        }
    }
}

/// Search-domain expansion resource limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SearchLimits {
    /// Maximum recursive expansion depth (default: 32).
    pub max_depth: usize,
    /// Maximum total expanded domain AST nodes (default: 4096).
    pub max_ast_nodes: usize,
    /// Maximum cooperative work units (default: 8192).
    pub max_work_units: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_ast_nodes: 4096,
            max_work_units: 8192,
        }
    }
}

impl SearchLimits {
    pub(crate) fn validate(self) -> Result<()> {
        let defaults = Self::default();
        for (name, value, ceiling) in [
            ("max_depth", self.max_depth, defaults.max_depth),
            ("max_ast_nodes", self.max_ast_nodes, defaults.max_ast_nodes),
            (
                "max_work_units",
                self.max_work_units,
                defaults.max_work_units,
            ),
        ] {
            if value == 0 || value > ceiling {
                return Err(Error::programming(format!(
                    "search_method {name} must be positive and no greater than its default"
                )));
            }
        }
        Ok(())
    }
}

/// Operation-local search-method expansion budget.
#[derive(Debug)]
pub struct SearchBudget {
    limits: SearchLimits,
    active_stack: Vec<(ModelId, FieldId, Operator)>,
    total_expanded_nodes: usize,
    total_bytes: usize,
    work_units_remaining: usize,
    sticky_error: Option<Arc<Error>>,
}

impl SearchBudget {
    /// Create a new budget from limits.
    pub fn new(limits: SearchLimits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            limits,
            active_stack: Vec::new(),
            total_expanded_nodes: 0,
            total_bytes: 0,
            work_units_remaining: limits.max_work_units,
            sticky_error: None,
        })
    }

    /// Check if a sticky error was previously recorded.
    pub fn check_sticky(&self) -> Result<()> {
        if let Some(error) = &self.sticky_error {
            return Err(retained_search_error(error));
        }
        Ok(())
    }

    /// Charge cooperative work units.
    pub fn charge_work(&mut self, units: usize) -> Result<()> {
        self.check_sticky()?;
        if self.work_units_remaining < units {
            let msg = format!(
                "search_method work budget exhausted (requested {units} units, {} remaining)",
                self.work_units_remaining
            );
            return Err(self.record_failure(Error::programming(msg)));
        }
        self.work_units_remaining -= units;
        Ok(())
    }

    /// Push an active expansion identity onto the recursion stack.
    pub fn push(&mut self, model: &ModelSpec, field: &FieldSpec, op: Operator) -> Result<()> {
        self.enter(model, field, op, true)
    }

    /// Cycle and depth checks; `charge` is false only for framework rewrites of the
    /// caller's own domain, which does not budget.
    fn enter(
        &mut self,
        model: &ModelSpec,
        field: &FieldSpec,
        op: Operator,
        charge: bool,
    ) -> Result<()> {
        self.check_sticky()?;
        if charge {
            self.charge_work(1)?;
        }
        if self.active_stack.contains(&(model.id, field.id, op)) {
            let msg = format!(
                "recursive search_method cycle detected for field '{}.{}' with operator '{:?}'",
                model.name, field.name, op
            );
            return Err(self.record_failure(Error::programming(msg)));
        }
        if self.active_stack.len() >= self.limits.max_depth {
            let msg = format!(
                "search_method expansion depth limit exceeded ({})",
                self.limits.max_depth
            );
            return Err(self.record_failure(Error::programming(msg)));
        }
        self.active_stack.push((model.id, field.id, op));
        Ok(())
    }

    /// Pop the active expansion identity from the recursion stack.
    pub fn pop(&mut self) {
        self.active_stack.pop();
    }

    /// Record newly expanded AST nodes.
    pub fn record_nodes(&mut self, count: usize) -> Result<()> {
        self.check_sticky()?;
        self.total_expanded_nodes = self
            .total_expanded_nodes
            .checked_add(count)
            .ok_or_else(|| Error::programming("search_method node accounting overflow"))?;
        if self.total_expanded_nodes > self.limits.max_ast_nodes {
            let msg = format!(
                "search_method total expanded AST node limit exceeded ({} > {})",
                self.total_expanded_nodes, self.limits.max_ast_nodes
            );
            return Err(self.record_failure(Error::programming(msg)));
        }
        Ok(())
    }

    /// Record a failure so catching a continuation cannot hide the error.
    pub fn record_failure(&mut self, error: Error) -> Error {
        let retained = self.sticky_error.get_or_insert_with(|| Arc::new(error));
        retained_search_error(retained)
    }
}

fn retained_search_error(original: &Arc<Error>) -> Error {
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

/// Validated search request for a nonstored field.
#[derive(Debug, Clone)]
pub struct FieldSearchRequest<'a> {
    operator: Operator,
    operand: &'a Value,
}

impl<'a> FieldSearchRequest<'a> {
    /// Create a new request.
    fn new(operator: Operator, operand: &'a Value) -> Self {
        Self { operator, operand }
    }

    /// The condition operator.
    pub fn operator(&self) -> Operator {
        self.operator
    }

    /// The typed operand value.
    pub fn operand(&self) -> &Value {
        self.operand
    }
}

/// Context passed to a `#[search_method]` provider.
pub struct FieldSearchContext<'a> {
    /// Read-only environment.
    env: &'a Env,
    /// Model specification of the field.
    model: &'a ModelSpec,
    /// Field specification.
    field: &'a FieldSpec,
    budget: &'a mut SearchBudget,
}

impl<'a> FieldSearchContext<'a> {
    /// Create a context.
    fn new(
        env: &'a Env,
        model: &'a ModelSpec,
        field: &'a FieldSpec,
        budget: &'a mut SearchBudget,
    ) -> Self {
        Self {
            env,
            model,
            field,
            budget,
        }
    }

    /// Read-only environment.
    pub fn env(&self) -> &Env {
        self.env
    }

    /// Model specification.
    pub fn model(&self) -> &ModelSpec {
        self.model
    }

    /// Field specification.
    pub fn field(&self) -> &FieldSpec {
        self.field
    }

    /// Charge cooperative work units.
    pub fn charge_work(&mut self, units: usize) -> Result<()> {
        self.budget.charge_work(units)
    }
}

/// Next-provider continuation of a `#[search_method(override)]`.
#[derive(Clone, Copy)]
pub struct FieldSearchSuper<'a> {
    rest: &'a [&'static FieldSearchImpl],
}

impl<'a> FieldSearchSuper<'a> {
    /// Invoke the super provider in the method resolution order.
    pub fn call(
        self,
        req: &FieldSearchRequest<'_>,
        ctx: &mut FieldSearchContext<'_>,
    ) -> Result<Domain> {
        let result = (|| {
            ctx.charge_work(1)?;
            validate_operand_for_field(ctx.model, ctx.field, req.operator, req.operand)?;
            match self.rest.split_first() {
                Some((first, rest)) => {
                    let domain = (first.func)(req, ctx, FieldSearchSuper { rest })?;
                    admit_returned_domain(&domain, ctx.budget, ctx.model, ctx.field)?;
                    Ok(domain)
                }
                None => Err(Error::programming(format!(
                    "no super search_method implementation for field '{}.{}'",
                    ctx.model.name, ctx.field.name
                ))),
            }
        })();
        let result = result.map_err(|error| ctx.budget.record_failure(error));
        ctx.budget.check_sticky()?;
        result
    }
}

impl std::fmt::Debug for FieldSearchSuper<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FieldSearchSuper")
            .field("remaining", &self.rest.len())
            .finish_non_exhaustive()
    }
}

/// Function signature for a `#[search_method]` provider.
pub type FieldSearchFn = fn(
    &FieldSearchRequest<'_>,
    &mut FieldSearchContext<'_>,
    FieldSearchSuper<'_>,
) -> Result<Domain>;

/// Search method registration entry linked into the binary.
pub struct FieldSearchImpl {
    /// Addon declaring the implementation.
    pub addon: &'static str,
    /// Model name.
    pub model: &'static str,
    /// Method name.
    pub method: &'static str,
    /// Function pointer.
    pub func: FieldSearchFn,
    /// Source file.
    pub file: &'static str,
    /// Source line.
    pub line: u32,
}

impl FieldSearchImpl {
    /// Source location description.
    pub fn location(&self) -> String {
        format!("{}:{} (addon '{}')", self.file, self.line, self.addon)
    }
}

impl std::fmt::Debug for FieldSearchImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FieldSearchImpl")
            .field("addon", &self.addon)
            .field("model", &self.model)
            .field("method", &self.method)
            .field("file", &self.file)
            .field("line", &self.line)
            .finish()
    }
}

/// Distributed slice collecting all `#[search_method]` entries.
#[linkme::distributed_slice]
pub static FIELD_SEARCH_IMPLS: [FieldSearchImpl];

/// Resolved search-method chains per model and method name.
#[derive(Debug, Default)]
pub struct FieldSearchChains {
    chains: FxHashMap<ModelId, FxHashMap<&'static str, Arc<[&'static FieldSearchImpl]>>>,
    /// Static diagnostic text is shared by every expansion of the registered field.
    contexts: FxHashMap<(ModelId, FieldId), Arc<str>>,
}

impl FieldSearchChains {
    /// Look up the provider chain for a model and method.
    pub fn get(&self, model: ModelId, method: &str) -> Option<&[&'static FieldSearchImpl]> {
        self.chains.get(&model)?.get(method).map(Arc::as_ref)
    }

    /// Build and validate search-method chains for a registry.
    pub fn build(registry: &Registry, extra: &[&'static FieldSearchImpl]) -> Result<Self> {
        let installed: rustc_hash::FxHashSet<&str> =
            registry.addons().iter().map(|a| a.name).collect();
        let mut by_owner: FxHashMap<(&str, &str), Vec<&'static FieldSearchImpl>> =
            FxHashMap::default();

        for imp in FIELD_SEARCH_IMPLS
            .iter()
            .chain(extra.iter().copied())
            .filter(|i| installed.contains(i.addon))
        {
            by_owner
                .entry((imp.model, imp.addon))
                .or_default()
                .push(imp);
        }

        let declared_models: rustc_hash::FxHashSet<(&str, &str)> = registry
            .models()
            .iter()
            .flat_map(|m| m.mro.iter().map(|d| (d.name, d.addon)))
            .collect();

        let mut owners: Vec<(&str, &str)> = by_owner.keys().copied().collect();
        owners.sort_unstable();

        for owner in &owners {
            let Some(list) = by_owner.get_mut(owner) else {
                continue;
            };
            list.sort_by_key(|i| (i.method, i.file, i.line));

            if let Some(first) = list.first()
                && !declared_models.contains(owner)
            {
                return Err(Error::programming(format!(
                    "#[search_method] at {} has no declaration of model '{}' in addon '{}': add an #[extend] (or #[model]) for it in that addon",
                    first.location(),
                    first.model,
                    first.addon
                )));
            }

            if let Some(pair) = list.windows(2).find(|w| w[0].method == w[1].method) {
                return Err(Error::programming(format!(
                    "#[search_method] '{}' of model '{}' is implemented twice by addon '{}': {} and {}",
                    pair[0].method,
                    pair[0].model,
                    pair[0].addon,
                    pair[0].location(),
                    pair[1].location()
                )));
            }
        }

        let mut chains: FxHashMap<
            ModelId,
            FxHashMap<&'static str, Arc<[&'static FieldSearchImpl]>>,
        > = FxHashMap::default();

        for model in registry.models() {
            let mut per_method: FxHashMap<&'static str, Vec<&'static FieldSearchImpl>> =
                FxHashMap::default();
            let mut seen_owners: Vec<(&str, &str)> = Vec::new();

            for decl in &model.mro {
                let owner = (decl.name, decl.addon);
                if seen_owners.contains(&owner) {
                    continue;
                }
                seen_owners.push(owner);

                for imp in by_owner.get(&owner).map(Vec::as_slice).unwrap_or_default() {
                    per_method.entry(imp.method).or_default().push(*imp);
                }
            }

            if !per_method.is_empty() {
                let arc_chains: FxHashMap<&'static str, Arc<[&'static FieldSearchImpl]>> =
                    per_method
                        .into_iter()
                        .map(|(m, list)| (m, Arc::from(list.into_boxed_slice())))
                        .collect();
                chains.insert(model.id, arc_chains);
            }
        }

        let mut contexts = FxHashMap::default();
        // Validate that every nonstored field in the registry declaring search_method has an implementation
        for model in registry.models() {
            for field in &model.fields {
                // Delegated copies resolve the owning parent's field, not a child MRO entry.
                if field.inherited.is_some() {
                    continue;
                }
                if let Some(method_name) = field.search_method {
                    let has_impl = chains
                        .get(&model.id)
                        .and_then(|m| m.get(method_name))
                        .is_some_and(|list| !list.is_empty());
                    if !has_impl {
                        return Err(Error::programming(format!(
                            "field '{}.{}' declares search_method '{method_name}', but no implementation was found in installed addons",
                            model.name, field.name
                        )));
                    }
                    contexts.insert(
                        (model.id, field.id),
                        Arc::from(format!(
                            "search_method '{method_name}' for field '{}.{}' returned an invalid domain",
                            model.name, field.name
                        )),
                    );
                }
            }
        }

        // Validate that every search_method implementation on a model matches a declared field with search_method
        for model in registry.models() {
            if let Some(model_chains) = chains.get(&model.id) {
                for &method_name in model_chains.keys() {
                    let field_matches = model
                        .fields
                        .iter()
                        .any(|f| f.inherited.is_none() && f.search_method == Some(method_name));
                    if !field_matches {
                        return Err(Error::programming(format!(
                            "#[search_method] '{method_name}' on model '{}' has no declared field with search_method = '{method_name}'",
                            model.name
                        )));
                    }
                }
            }
        }

        Ok(Self { chains, contexts })
    }
}

/// Count native domain nodes, including relational subdomains, without recursive traversal.
pub fn count_domain_nodes(domain: &Domain) -> usize {
    let mut stack = vec![domain];
    let mut count = 0usize;
    while let Some(d) = stack.pop() {
        count = count.saturating_add(1);
        match d.kind() {
            DomainKind::Not(child) => stack.push(child),
            DomainKind::And(children) | DomainKind::Or(children) => stack.extend(children),
            DomainKind::Condition(c) => {
                if let Value::Domain(sub) = c.value() {
                    stack.push(sub);
                } else if let Some(query) = ReferenceQuery::of(c.value()) {
                    stack.push(query.sub());
                }
            }
            DomainKind::True | DomainKind::False | DomainKind::Custom(_) => {}
        }
    }
    count
}

// Validate before recursive expansion, field checks, operand cloning or path wrapping.
// The stack is capped before growth; each visit and queued child charges shared work.
fn admit_returned_domain(
    domain: &Domain,
    budget: &mut SearchBudget,
    model: &ModelSpec,
    field: &FieldSpec,
) -> Result<()> {
    let method = field.search_method.ok_or_else(|| {
        Error::programming(format!(
            "field '{}.{}' has no declared search_method during output admission",
            model.name, field.name
        ))
    })?;
    admit_attributed_domain(domain, budget, false, || {
        format!(
            "search_method '{method}' for field '{}.{}' returned an invalid domain",
            model.name, field.name
        )
    })
}

/// Attribute only this pure admission walk's failures, without copying an unadmitted AST.
fn admit_attributed_domain(
    domain: &Domain,
    budget: &mut SearchBudget,
    framework: bool,
    context: impl FnOnce() -> String,
) -> Result<()> {
    budget.check_sticky()?;
    if let Err(error) = admit_domain(domain, budget, framework) {
        let error =
            Error::programming(format!("{}: {}", context(), error.message())).with_source(error);
        // No failure existed on entry and this walk invokes no callbacks. Replace only
        // the failure created by this admission, before any continuation can catch it.
        // Callback/business/access/database failures never enter this attribution path.
        budget.sticky_error = Some(Arc::new(error));
        return budget.check_sticky();
    }
    Ok(())
}

fn admit_domain(domain: &Domain, budget: &mut SearchBudget, framework: bool) -> Result<()> {
    enum Item<'a> {
        Domain(&'a Domain, usize),
        Value(&'a Value, usize),
    }
    let mut stack = vec![Item::Domain(domain, 0)];
    while let Some(item) = stack.pop() {
        budget.charge_work(1)?;
        let (depth, bytes) = match item {
            Item::Domain(d, depth) => {
                budget.record_nodes(1)?;
                match d.kind() {
                    DomainKind::Not(child) => stack.push(Item::Domain(child, depth + 1)),
                    DomainKind::And(children) | DomainKind::Or(children) => {
                        for child in children {
                            if stack.len() >= budget.limits.max_ast_nodes {
                                return Err(Error::programming(format!(
                                    "search_method total expanded AST node limit exceeded ({})",
                                    budget.limits.max_ast_nodes
                                )));
                            }
                            stack.push(Item::Domain(child, depth + 1));
                        }
                    }
                    DomainKind::Condition(c) => {
                        if !framework && c.operator().is_internal() {
                            return Err(Error::programming(format!(
                                "search_method returned internal traversal operator '{}'",
                                c.operator().as_str()
                            )));
                        }
                        stack.push(Item::Value(c.value(), depth + 1));
                        budget.total_bytes = budget
                            .total_bytes
                            .checked_add(c.field_expr().len())
                            .ok_or_else(|| {
                            Error::programming(
                                "search_method returned domain byte accounting overflow",
                            )
                        })?;
                    }
                    DomainKind::Custom(_) if framework => {}
                    DomainKind::Custom(_) => {
                        return Err(Error::programming(
                            "search_method returned a custom domain node",
                        ));
                    }
                    DomainKind::True | DomainKind::False => {}
                }
                (depth, 0)
            }
            Item::Value(v, depth) => {
                let bytes = match v {
                    Value::Domain(sub) => {
                        stack.push(Item::Domain(sub, depth + 1));
                        0
                    }
                    Value::List(values) => {
                        for value in values {
                            if stack.len() >= budget.limits.max_ast_nodes {
                                return Err(Error::programming(format!(
                                    "search_method total expanded AST node limit exceeded ({})",
                                    budget.limits.max_ast_nodes
                                )));
                            }
                            stack.push(Item::Value(value, depth + 1));
                        }
                        0
                    }
                    Value::Set(values) => {
                        for value in values {
                            if stack.len() >= budget.limits.max_ast_nodes {
                                return Err(Error::programming(format!(
                                    "search_method total expanded AST node limit exceeded ({})",
                                    budget.limits.max_ast_nodes
                                )));
                            }
                            stack.push(Item::Value(value, depth + 1));
                        }
                        0
                    }
                    Value::Str(text) => text.len(),
                    Value::Opaque(_) if framework => std::mem::size_of::<Value>(),
                    Value::Opaque(_) => {
                        return Err(Error::programming(
                            "search_method returned an opaque domain value",
                        ));
                    }
                    _ => std::mem::size_of::<Value>(),
                };
                (depth, bytes)
            }
        };
        if depth > erp_domain::MAX_DEPTH {
            return Err(Error::programming(
                "search_method returned domain depth limit exceeded (256)",
            ));
        }
        budget.total_bytes = budget.total_bytes.checked_add(bytes).ok_or_else(|| {
            Error::programming("search_method returned domain byte accounting overflow")
        })?;
        if budget.total_bytes > 1_048_576 {
            return Err(Error::programming(
                "search_method returned domain byte limit exceeded (1048576)",
            ));
        }
    }
    Ok(())
}

/// Validate that operand matches the declared field type before dispatch.
fn validate_operand_for_field(
    model: &ModelSpec,
    field: &FieldSpec,
    operator: Operator,
    operand: &Value,
) -> Result<()> {
    match field.ty {
        FieldType::Boolean => match operator {
            Operator::Eq | Operator::Ne => match operand {
                Value::Bool(_) => Ok(()),
                other => Err(Error::programming(format!(
                    "invalid operand for boolean field '{}.{}': expected boolean, got {other:?}",
                    model.name, field.name
                ))),
            },
            Operator::In | Operator::NotIn => match operand {
                Value::List(list) => {
                    for v in list {
                        if !matches!(v, Value::Bool(_)) {
                            return Err(Error::programming(format!(
                                "invalid operand element for boolean field '{}.{}': expected boolean, got {v:?}",
                                model.name, field.name
                            )));
                        }
                    }
                    Ok(())
                }
                Value::Set(set) => {
                    for v in set {
                        if !matches!(v, Value::Bool(_)) {
                            return Err(Error::programming(format!(
                                "invalid operand element for boolean field '{}.{}': expected boolean, got {v:?}",
                                model.name, field.name
                            )));
                        }
                    }
                    Ok(())
                }
                other => Err(Error::programming(format!(
                    "invalid operand for boolean field '{}.{}': expected list or set of booleans, got {other:?}",
                    model.name, field.name
                ))),
            },
            other => Err(Error::programming(format!(
                "unsupported operator '{other:?}' for boolean field '{}.{}' in search_method",
                model.name, field.name
            ))),
        },
        ref ty => {
            let scalar = |v: &Value| match ty {
                FieldType::Id
                | FieldType::Integer
                | FieldType::RowVersion
                | FieldType::Many2oneReference { .. }
                | FieldType::Many2one { .. }
                | FieldType::One2many { .. }
                | FieldType::Many2many { .. } => matches!(v, Value::Int(_) | Value::Bool(false)),
                FieldType::Float { .. } | FieldType::Monetary { .. } => match v {
                    Value::Float(f) => f.is_finite(),
                    Value::Int(_) | Value::Bool(false) => true,
                    _ => false,
                },
                FieldType::Char { .. }
                | FieldType::Text
                | FieldType::Html
                | FieldType::Selection { .. }
                | FieldType::Reference { .. } => matches!(v, Value::Str(_) | Value::Bool(false)),
                FieldType::Date => matches!(v, Value::Date(_) | Value::Bool(false)),
                FieldType::Datetime => matches!(v, Value::Datetime(_) | Value::Bool(false)),
                FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz => {
                    matches!(v, Value::Bool(false))
                        || erp_sql::param::ScalarKind::of_field(ty)
                            .is_some_and(|kind| erp_sql::param::to_scalar(kind, v).is_ok())
                }
                // No unvalidated pass-through for field kinds without a typed operand contract.
                FieldType::Json
                | FieldType::DateRange { .. }
                | FieldType::DatetimeRange { .. }
                | FieldType::Boolean => false,
            };
            let valid = match operator {
                Operator::In | Operator::NotIn => match operand {
                    Value::List(values) => values.iter().all(&scalar),
                    Value::Set(values) => values.iter().all(&scalar),
                    _ => false,
                },
                Operator::Eq
                | Operator::Ne
                | Operator::EqIfSet
                | Operator::Lt
                | Operator::Gt
                | Operator::Le
                | Operator::Ge => scalar(operand),
                Operator::Like
                | Operator::NotLike
                | Operator::ILike
                | Operator::NotILike
                | Operator::EqLike
                | Operator::NotEqLike
                | Operator::EqILike
                | Operator::NotEqILike => {
                    matches!(
                        ty,
                        FieldType::Char { .. }
                            | FieldType::Text
                            | FieldType::Html
                            | FieldType::Selection { .. }
                    ) && matches!(operand, Value::Str(_))
                }
                _ => false,
            };
            if valid {
                Ok(())
            } else {
                Err(Error::programming(format!(
                    "invalid operand or operator '{operator:?}' for {} field '{}.{}' in search_method",
                    ty.type_name(),
                    model.name,
                    field.name
                )))
            }
        }
    }
}

/// Expand all nonstored searchable field conditions in `domain` before SQL planning.
pub(crate) fn expand_search_domains(
    tx: &Transaction,
    records: &crate::Records,
    domain: &Domain,
) -> Result<Domain> {
    expand_for_model(tx, records.env(), records.spec(), domain, true)
}

pub(crate) fn expand_for_model(
    tx: &Transaction,
    env: &Env,
    model: &ModelSpec,
    domain: &Domain,
    field_checks: bool,
) -> Result<Domain> {
    let scope = scope(tx)?;
    let mut budget = scope
        .lock()
        .map_err(|_| Error::programming("search_method budget lock poisoned"))?;
    let ctx = ExpandContext {
        tx,
        env,
        field_checks,
        external: domain.is_external(),
        programming_context: domain.programming_context(),
    };
    expand_domain_with_budget(&ctx, model, domain, &mut budget, 0)
}

pub(crate) fn expand_policy(
    tx: &Transaction,
    env: &Env,
    model: &ModelSpec,
    domain: &Domain,
) -> Result<Domain> {
    let expanded = expand_for_model(tx, env, model, domain, false)?;
    crate::access::bypass_traversals(tx.registry(), model, &expanded)
}

fn expand_domain_with_budget(
    ctx: &ExpandContext<'_>,
    model: &ModelSpec,
    domain: &Domain,
    budget: &mut SearchBudget,
    depth: usize,
) -> Result<Domain> {
    let programming_context = domain.programming_context().or(ctx.programming_context);
    let ctx = &ExpandContext {
        external: programming_context.is_none() && (ctx.external || domain.is_external()),
        programming_context,
        ..*ctx
    };
    budget.check_sticky()?;
    if depth > erp_domain::MAX_DEPTH {
        return Err(Error::programming(
            "search_method expanded domain depth limit exceeded (256)",
        ));
    }
    let expanded = match domain.kind() {
        DomainKind::True => Ok(Domain::TRUE),
        DomainKind::False => Ok(Domain::FALSE),
        DomainKind::Custom(c) => Ok(Domain::custom(c.clone())),
        DomainKind::Not(child) => {
            let inner = expand_domain_with_budget(ctx, model, child, budget, depth + 1)?;
            Ok(!inner)
        }
        DomainKind::And(children) => {
            let mut res = Domain::TRUE;
            for child in children {
                let c = expand_domain_with_budget(ctx, model, child, budget, depth + 1)?;
                res = res & c;
            }
            Ok(res)
        }
        DomainKind::Or(children) => {
            let mut res = Domain::FALSE;
            for child in children {
                let c = expand_domain_with_budget(ctx, model, child, budget, depth + 1)?;
                res = res | c;
            }
            Ok(res)
        }
        DomainKind::Condition(cond) => {
            expand_condition_with_budget(ctx, model, cond, budget, depth)
        }
    }?;
    // Original user rewrites retain Validation provenance. Explicit provider contexts
    // take precedence, including on descendants of an external compound root.
    Ok(if let Some(context) = ctx.programming_context {
        expanded.with_programming_context(Arc::clone(context))
    } else if ctx.external {
        expanded.into_external()
    } else {
        expanded
    })
}

/// The target model of a reference query.
///
/// # Errors
///
/// `Programming` when the model is not in the registry (a query built for another registry).
pub(crate) fn reference_target<'r>(
    registry: &'r Registry,
    query: &ReferenceQuery,
) -> Result<&'r ModelSpec> {
    registry
        .models()
        .get(query.target().index())
        .ok_or_else(|| {
            Error::programming("a reference query names a model that is not in the registry")
        })
}

fn framework_field(field: &FieldSpec) -> bool {
    field.inherited.is_some()
        || (!field.store
            && field.search_method.is_none()
            && (field.related.is_some()
                || (field.name == "display_name"
                    && field.compute == Some("_compute_display_name"))))
}

fn expand_condition_with_budget(
    ctx: &ExpandContext<'_>,
    model: &ModelSpec,
    cond: &erp_domain::Condition,
    budget: &mut SearchBudget,
    depth: usize,
) -> Result<Domain> {
    let tx = ctx.tx;
    let env = ctx.env;
    budget.check_sticky()?;

    // A reference query (Amendment 1) holds a predicate on its target model: search
    // methods inside it expand like those of a comodel sub-domain.
    if cond.operator().is_any()
        && let Some(query) = ReferenceQuery::of(cond.value())
    {
        let target = reference_target(env.registry(), query)?;
        let sub_ctx = ExpandContext {
            field_checks: ctx.field_checks && !cond.operator().is_internal(),
            ..*ctx
        };
        let expanded = expand_domain_with_budget(&sub_ctx, target, query.sub(), budget, depth + 2)?;
        return Domain::condition(
            cond.field_expr(),
            cond.operator(),
            query.with_sub(expanded).into_value(),
        )
        .map_err(Error::from);
    }

    // Check for relational quantifier sub-domains: any / not any
    if matches!(
        cond.operator(),
        Operator::Any | Operator::NotAny | Operator::AnyBypass | Operator::NotAnyBypass
    ) {
        let registry = env.registry();
        if let Some(field) = model.field(cond.field_expr())
            && let Some(comodel_name) = field.ty.comodel()
            && let Some(comodel) = registry.model(comodel_name)
        {
            // Parse every list quantifier before ordinary hierarchy/access preparation.
            // External and provider syntax cannot acquire trusted traversal operators.
            let parsed_sub = if let Value::List(items) = cond.value() {
                let parsed = if ctx.external || ctx.programming_context.is_some() {
                    Domain::from_list(items)
                } else {
                    Domain::from_list_native(items)
                }
                .map_err(|error| ctx.invalid(error.to_string()).with_source(error))?;
                Some(if let Some(context) = ctx.programming_context {
                    // The raw list and its newly materialized AST both consume the same
                    // operation budget. Admit before annotation or recursive expansion.
                    admit_attributed_domain(&parsed, budget, false, || context.to_string())?;
                    parsed.with_programming_context(Arc::clone(context))
                } else {
                    parsed
                })
            } else {
                None
            };
            let sub = match cond.value() {
                Value::Domain(sub) => Some(sub),
                _ => parsed_sub.as_ref(),
            };
            if let Some(sub) = sub {
                // Internal any! bypasses field groups, while company-bound related rewrites
                // still use the original reader (the optimizer has the same distinction).
                let sub_ctx = ExpandContext {
                    field_checks: ctx.field_checks && !cond.operator().is_internal(),
                    ..*ctx
                };
                let expanded_sub =
                    expand_domain_with_budget(&sub_ctx, comodel, sub, budget, depth + 2)?;
                return Domain::condition(
                    cond.field_expr(),
                    cond.operator(),
                    Value::Domain(expanded_sub),
                )
                .map_err(Error::from);
            }
        }
    }

    let field_expr = cond.field_expr();
    if field_expr
        .split('.')
        .take(erp_domain::MAX_DEPTH + 2)
        .count()
        > erp_domain::MAX_DEPTH + 1
    {
        return Err(Error::programming(
            "search_method expanded domain depth limit exceeded (256)",
        ));
    }
    let parts: Vec<&str> = field_expr.split('.').collect();

    if parts.len() == 1 {
        let field_name = parts[0];
        let Some(field) = model.field(field_name) else {
            return Domain::condition(field_expr, cond.operator(), cond.value().clone())
                .map_err(Error::from);
        };

        if framework_field(field) {
            if ctx.field_checks {
                tx.check_field(env, model.id, field.id, FieldOp::Read)?;
            }
            // Framework rewrites (inherits, related, display_name) of the caller's own
            // domain predate and are not search-method expansion: their size
            // follows the caller's domain, which the domain parser already bounds, so
            // they charge no work and skip output admission. Inside a provider's output
            // the same rewrite stays budgeted. Cycle and depth checks apply to both.
            let budgeted = ctx.programming_context.is_some();
            budget.enter(model, field, cond.operator(), budgeted)?;
            let result = (|| {
                let related = crate::optimize::framework_search_domain(
                    env,
                    model,
                    field,
                    cond,
                    ctx.external,
                    ctx.programming_context,
                )?;
                // This AST comes from our existing framework rewrite, whose company guards
                // may contain trusted opaque query values. Native providers never get this exemption.
                if let Some(context) = ctx.programming_context {
                    admit_attributed_domain(&related, budget, true, || context.to_string())?;
                }
                expand_domain_with_budget(ctx, model, &related, budget, depth)
            })();
            budget.pop();
            return result;
        }

        if field.search_method.is_some() {
            // Check field read access on original field before provider dispatch
            if ctx.field_checks {
                tx.check_field(env, model.id, field.id, FieldOp::Read)?;
            }

            // Validate operand against declared field type
            validate_operand_for_field(model, field, cond.operator(), cond.value())
                .map_err(|error| ctx.invalid(error.message().to_owned()).with_source(error))?;

            budget.push(model, field, cond.operator())?;
            let res = (|| {
                let returned_domain = match dispatch_search_method(
                    tx,
                    env,
                    model,
                    field,
                    &FieldSearchRequest::new(cond.operator(), cond.value()),
                    budget,
                ) {
                    Ok(d) => d,
                    Err(e) => {
                        return Err(budget.record_failure(e));
                    }
                };

                admit_returned_domain(&returned_domain, budget, model, field)?;
                if ctx.field_checks {
                    tx.check_domain_fields(env, model, &returned_domain)?;
                }

                // Admission and access checks precede annotation/copying. A provider AST
                // is native framework output even when it was built with a JSON parser.
                let context = tx
                    .shared
                    .search_methods
                    .contexts
                    .get(&(model.id, field.id))
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "missing search_method diagnostic context for field '{}.{}'",
                            model.name, field.name
                        ))
                    })?;
                let returned_domain = returned_domain.with_programming_context(Arc::clone(context));
                let provider_ctx = ExpandContext {
                    external: false,
                    programming_context: Some(context),
                    ..*ctx
                };
                // Recursively expand further providers without inheriting user provenance.
                expand_domain_with_budget(&provider_ctx, model, &returned_domain, budget, depth)
            })();
            budget.pop();
            return res;
        }

        Domain::condition(field_expr, cond.operator(), cond.value().clone()).map_err(Error::from)
    } else {
        // Dotted path: e.g. parent_id.has_vat
        let (hops, last) = parts.split_at(parts.len() - 1);
        let registry = env.registry();
        let mut target_model = model;

        for &hop in hops {
            let Some(f) = target_model.field(hop) else {
                return Domain::condition(field_expr, cond.operator(), cond.value().clone())
                    .map_err(Error::from);
            };
            let Some(co_name) = f.ty.comodel() else {
                return Domain::condition(field_expr, cond.operator(), cond.value().clone())
                    .map_err(Error::from);
            };
            let Some(co) = registry.model(co_name) else {
                return Domain::condition(field_expr, cond.operator(), cond.value().clone())
                    .map_err(Error::from);
            };
            target_model = co;
        }

        let last_name = last[0];
        let Some(target_field) = target_model.field(last_name) else {
            return Domain::condition(field_expr, cond.operator(), cond.value().clone())
                .map_err(Error::from);
        };

        if target_field.search_method.is_some() || framework_field(target_field) {
            let target_depth = hops
                .len()
                .checked_mul(2)
                .and_then(|extra| depth.checked_add(extra))
                .ok_or_else(|| Error::programming("search_method path depth overflow"))?;
            let target = Domain::condition(last_name, cond.operator(), cond.value().clone())
                .map_err(Error::from)?;
            let mut expanded =
                expand_domain_with_budget(ctx, target_model, &target, budget, target_depth)?;
            for hop in hops.iter().rev() {
                budget.record_nodes(1)?;
                expanded = Domain::condition(hop, Operator::Any, expanded).map_err(Error::from)?;
            }
            return Ok(expanded);
        }

        Domain::condition(field_expr, cond.operator(), cond.value().clone()).map_err(Error::from)
    }
}

fn dispatch_search_method(
    tx: &Transaction,
    env: &Env,
    model: &ModelSpec,
    field: &FieldSpec,
    req: &FieldSearchRequest<'_>,
    budget: &mut SearchBudget,
) -> Result<Domain> {
    let method_name = field.search_method.ok_or_else(|| {
        Error::programming(format!(
            "field '{}.{}' has no declared search_method",
            model.name, field.name
        ))
    })?;
    let chains = &tx.shared.search_methods;
    let Some(chain) = chains.get(model.id, method_name) else {
        return Err(Error::programming(format!(
            "search_method '{method_name}' on model '{}' has no registered implementation",
            model.name
        )));
    };
    let Some((first, rest)) = chain.split_first() else {
        return Err(Error::programming(format!(
            "search_method '{method_name}' on model '{}' chain is empty",
            model.name
        )));
    };

    let mut ctx = FieldSearchContext::new(env, model, field, budget);
    let sup = FieldSearchSuper { rest };
    let result = (first.func)(req, &mut ctx, sup);
    let result = result.map_err(|error| ctx.budget.record_failure(error));
    ctx.budget.check_sticky()?;
    result
}
