//! Proxies for models, records, recordsets, and the environment in data expressions.
//!
//! `obj()` returns a [`ModelProxy`]
//! - `obj(id)` returns a [`RecordProxy`]
//! - `obj().search(...)` returns a [`RecordsetProxy`]
//! - `obj().env` returns an [`EnvProxy`]
//! - `obj().sudo(...)` and `obj().with_context(...)` modify environment elevation and context
//!
//! All proxy objects implement [`ExprObject`] and run with limits: a recordset reports
//! its ids through [`ExprObject::meta`], so the evaluator charges them to the budget when a host
//! call or attribute produces it, and a search result above [`MAX_SEARCH_RESULTS`] ids fails.
//!
//! Host data the data loader does not provide is never invented: it fails loudly with
//! a `NameError`.

use std::fmt;
use std::sync::Arc;

pub use crate::context::RefResolver;
use crate::error::{ExprError, ExprResult};
use crate::limits::Budget;
use crate::value::{ExprCallable, ExprObject, ExprValue, Kwargs, Meta, SLOT};

/// Reads a field/attribute of a record: `(model, id, field_name) -> ExprResult<ExprValue>`.
pub type RecordResolver = Arc<dyn Fn(&str, i64, &str) -> ExprResult<ExprValue> + Send + Sync>;

/// A model search query from an expression.
#[derive(Debug, Clone)]
pub struct SearchQuery<'a> {
    /// The model being searched.
    pub model: &'a str,
    /// The domain expression value.
    pub domain: &'a ExprValue,
    /// Number of records to skip.
    pub offset: u64,
    /// Maximum number of records to return, if any.
    pub limit: Option<u64>,
    /// Order specification (e.g. `"name asc"`), if any.
    pub order: Option<&'a str>,
    /// The evaluation context under which the search is executed.
    pub context: &'a ExprValue,
    /// Whether superuser mode is active.
    pub sudo: bool,
}

/// Searches a model with pagination and ordering options: `(&SearchQuery<'_>) -> ExprResult<Vec<i64>>`.
pub type SearchResolver = Arc<dyn Fn(&SearchQuery<'_>) -> ExprResult<Vec<i64>> + Send + Sync>;

/// Resolves an XML ID to its `(model, database_id)`.
pub type XidModelResolver = Arc<dyn Fn(&str) -> Option<(String, i64)> + Send + Sync>;

/// Checks if the current user belongs to a security group: `(group_xml_id) -> ExprResult<bool>`.
pub type GroupCheckResolver = Arc<dyn Fn(&str) -> ExprResult<bool> + Send + Sync>;

/// Checks if a given user belongs to a security group: `(uid, group_xml_id) -> ExprResult<bool>`.
/// Answers `res.users(uid).has_group(..)` for a user other than `env.uid`; an unknown user is an
/// error, never `false`.
pub type UserGroupCheckResolver = Arc<dyn Fn(i64, &str) -> ExprResult<bool> + Send + Sync>;

/// Most record ids one recordset may hold in a data expression (a search result, an x2many
/// value). A host asks its database for at most one more and the proxy fails when it gets it, so
/// an unlimited `search([])` on a large table is an error instead of an unbounded id list.
pub const MAX_SEARCH_RESULTS: usize = 10_000;

/// The board task that delivers host context for data expressions.
pub const HOST_SLICE: &str = "a later release";

/// `NameError` for host data the host environment does not provide.
fn host_missing(name: &str, what: &str) -> ExprError {
    ExprError::unavailable(
        name,
        format!("{what} is not configured on this environment (host context)"),
    )
}

/// A database id of `model`: a positive integer.
fn record_id(model: &str, id: i64) -> ExprResult<i64> {
    if id > 0 {
        Ok(id)
    } else {
        Err(ExprError::value_error(format!(
            "{model} record ids are positive integers, got {id}"
        )))
    }
}

/// Fails when a recordset of `model` would hold more than [`MAX_SEARCH_RESULTS`] ids.
fn check_count(model: &str, what: &str, count: usize) -> ExprResult<()> {
    if count > MAX_SEARCH_RESULTS {
        return Err(ExprError::limit(format!(
            "{what} on {model} gave more than {MAX_SEARCH_RESULTS} records, the limit of a \
             recordset in a data expression"
        )));
    }
    Ok(())
}

fn records_eq(a: &dyn ExprObject, b: &dyn ExprObject) -> bool {
    if a.type_name() != b.type_name() {
        return false;
    }
    let (Some(left), Some(right)) = (a.record_ids(), b.record_ids()) else {
        return false;
    };
    let set = |ids: &[i64]| {
        let mut v = ids.to_vec();
        v.sort_unstable();
        v.dedup();
        v
    };
    set(&left) == set(&right)
}

/// Host callbacks and evaluation context state for proxy objects in expressions.
#[derive(Clone)]
pub struct HostHooks {
    /// Reads a record field: `(model, id, field_name) -> ExprResult<ExprValue>`.
    pub record: Option<RecordResolver>,
    /// Searches a model with pagination and sorting options.
    pub search: Option<SearchResolver>,
    /// Resolves an XML ID: `(xml_id, raise_if_not_found) -> ExprResult<ExprValue>`.
    pub reference: Option<RefResolver>,
    /// Maps an XML ID to `(model, database_id)`.
    pub xid_model: Option<XidModelResolver>,
    /// Checks group membership for the current user: `(group_xml_id) -> ExprResult<bool>`.
    pub group_check: Option<GroupCheckResolver>,
    /// Checks group membership of an explicit user: `(uid, group_xml_id) -> ExprResult<bool>`.
    pub user_group_check: Option<UserGroupCheckResolver>,
    /// Current user ID (`env.uid`), if provided by the host.
    pub user_id: Option<i64>,
    /// Current company ID (`env.company`), if provided by the host.
    pub company_id: Option<i64>,
    /// Allowed company IDs (`env.companies`), if provided by the host.
    pub company_ids: Vec<i64>,
    /// Evaluation context (`env.context`), if provided by the host.
    pub context: ExprValue,
    /// Whether superuser mode is active (`sudo`). Defaults to true for data file evaluation.
    pub sudo: bool,
}

impl Default for HostHooks {
    fn default() -> Self {
        Self {
            record: None,
            search: None,
            reference: None,
            xid_model: None,
            group_check: None,
            user_group_check: None,
            user_id: None,
            company_id: None,
            company_ids: Vec::new(),
            context: ExprValue::None,
            sudo: true,
        }
    }
}

impl fmt::Debug for HostHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostHooks")
            .field("has_record", &self.record.is_some())
            .field("has_search", &self.search.is_some())
            .field("has_reference", &self.reference.is_some())
            .field("has_xid_model", &self.xid_model.is_some())
            .field("has_group_check", &self.group_check.is_some())
            .field("has_user_group_check", &self.user_group_check.is_some())
            .field("user_id", &self.user_id)
            .field("company_id", &self.company_id)
            .field("company_ids", &self.company_ids)
            .field("sudo", &self.sudo)
            .finish()
    }
}

impl HostHooks {
    /// New default host hooks with superuser mode enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the record field resolver.
    pub fn with_record_resolver(
        mut self,
        resolver: impl Fn(&str, i64, &str) -> ExprResult<ExprValue> + Send + Sync + 'static,
    ) -> Self {
        self.record = Some(Arc::new(resolver));
        self
    }

    /// Sets the model search resolver.
    pub fn with_search_resolver(
        mut self,
        resolver: impl Fn(&SearchQuery<'_>) -> ExprResult<Vec<i64>> + Send + Sync + 'static,
    ) -> Self {
        self.search = Some(Arc::new(resolver));
        self
    }

    /// Sets the external ID reference resolver.
    pub fn with_ref_resolver(
        mut self,
        resolver: impl Fn(&str, bool) -> ExprResult<ExprValue> + Send + Sync + 'static,
    ) -> Self {
        self.reference = Some(Arc::new(resolver));
        self
    }

    /// Sets the external ID to model resolver.
    pub fn with_xid_model_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<(String, i64)> + Send + Sync + 'static,
    ) -> Self {
        self.xid_model = Some(Arc::new(resolver));
        self
    }

    /// Sets the group membership checker.
    pub fn with_group_checker(
        mut self,
        checker: impl Fn(&str) -> ExprResult<bool> + Send + Sync + 'static,
    ) -> Self {
        self.group_check = Some(Arc::new(checker));
        self
    }

    /// Sets the per-user group membership checker (`res.users(N).has_group(..)`, N not `env.uid`).
    pub fn with_user_group_checker(
        mut self,
        checker: impl Fn(i64, &str) -> ExprResult<bool> + Send + Sync + 'static,
    ) -> Self {
        self.user_group_check = Some(Arc::new(checker));
        self
    }

    /// Sets the current user ID (`env.uid`).
    pub fn with_user_id(mut self, uid: i64) -> Self {
        self.user_id = Some(uid);
        self
    }

    /// Sets the active company ID (`env.company`).
    pub fn with_company_id(mut self, cid: i64) -> Self {
        self.company_id = Some(cid);
        self
    }

    /// Sets the active allowed company IDs (`env.companies`).
    pub fn with_company_ids(mut self, cids: Vec<i64>) -> Self {
        self.company_ids = cids;
        self
    }

    /// Sets the evaluation context value (`env.context`).
    pub fn with_context(mut self, context: ExprValue) -> Self {
        self.context = context;
        self
    }

    /// Sets superuser elevation flag.
    pub fn with_sudo(mut self, sudo: bool) -> Self {
        self.sudo = sudo;
        self
    }

    /// Creates a record proxy attached to these hooks.
    pub fn record(this: &Arc<Self>, model: &Arc<str>, id: i64) -> ExprValue {
        ExprValue::Object(Arc::new(RecordProxy {
            model: Arc::clone(model),
            id,
            hooks: Arc::clone(this),
        }))
    }

    /// Creates a recordset proxy attached to these hooks.
    pub fn recordset(this: &Arc<Self>, model: &Arc<str>, ids: Vec<i64>) -> ExprValue {
        ExprValue::Object(Arc::new(RecordsetProxy {
            model: Arc::clone(model),
            ids,
            hooks: Arc::clone(this),
        }))
    }

    /// Creates an environment proxy attached to these hooks.
    pub fn env(this: &Arc<Self>) -> ExprValue {
        ExprValue::Object(Arc::new(EnvProxy {
            hooks: Arc::clone(this),
        }))
    }

    /// Reads a record field, attaching these hooks to the returned value.
    pub fn read(this: &Arc<Self>, model: &str, id: i64, field: &str) -> ExprResult<ExprValue> {
        let Some(res) = &this.record else {
            return Err(ExprError::attribute_error(model, field));
        };
        let mut val = res(model, id, field)?;
        val.attach_hooks(Arc::clone(this))?;
        Ok(val)
    }

    /// Queries the group checker for `xmlid`.
    pub fn has_group(&self, xmlid: &str) -> ExprResult<bool> {
        let Some(checker) = &self.group_check else {
            return Err(ExprError::unavailable(
                "has_group",
                "group checks are provided by the host (HostHooks::with_group_checker)",
            ));
        };
        checker(xmlid)
    }
}

/// `sudo` and `with_context` of a model, record or recordset.
fn context_method(target: Arc<dyn ExprObject>, hooks: Arc<HostHooks>, name: &str) -> ExprValue {
    ExprValue::Callable(Arc::new(SelfMethod {
        target,
        hooks,
        kind: if name == "sudo" {
            SelfKind::Sudo
        } else {
            SelfKind::WithContext
        },
    }))
}

// ---- ModelProxy ----------------------------------------------------------------------------

/// A model proxy or empty recordset for `<model>` (the `obj()` callable).
#[derive(Clone)]
pub struct ModelProxy {
    model: Arc<str>,
    hooks: Arc<HostHooks>,
}

impl ModelProxy {
    /// A proxy of `model` with default host callbacks.
    pub fn new(model: impl Into<Arc<str>>) -> Self {
        Self {
            model: model.into(),
            hooks: Arc::new(HostHooks::default()),
        }
    }

    /// A proxy of `model` with the given host hooks.
    pub fn with_hooks(model: impl Into<Arc<str>>, hooks: Arc<HostHooks>) -> Self {
        Self {
            model: model.into(),
            hooks,
        }
    }

    /// The model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The host hooks.
    pub fn hooks(&self) -> &Arc<HostHooks> {
        &self.hooks
    }
}

impl fmt::Debug for ModelProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}()", self.model)
    }
}

impl ExprCallable for ModelProxy {
    fn name(&self) -> &str {
        "browse"
    }

    /// `browse()`: an empty recordset; `browse(id)` / `browse(record)`: one record;
    /// `browse(False)` / `browse(None)` / `browse(<empty recordset>)`: an empty recordset.
    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        if !kwargs.is_empty() {
            return Err(ExprError::type_error(
                "browse() does not take keyword arguments",
            ));
        }
        match args {
            [] => Ok(ExprValue::Object(Arc::new(self.clone()))),
            [ExprValue::Int(i)] => Ok(HostHooks::record(
                &self.hooks,
                &self.model,
                record_id(&self.model, *i)?,
            )),
            [ExprValue::Bool(false) | ExprValue::None] => {
                Ok(HostHooks::recordset(&self.hooks, &self.model, Vec::new()))
            }
            [ExprValue::Object(o)] if o.is_record() => {
                if o.type_name() != &*self.model {
                    return Err(ExprError::type_error(format!(
                        "browse() takes a {} record, not '{}'",
                        self.model,
                        o.type_name()
                    )));
                }
                if !o.is_truthy() {
                    return Ok(HostHooks::recordset(&self.hooks, &self.model, Vec::new()));
                }
                let id = o.as_int()?;
                Ok(HostHooks::record(
                    &self.hooks,
                    &self.model,
                    record_id(&self.model, id)?,
                ))
            }
            [other] => Err(ExprError::type_error(format!(
                "browse() id must be an integer or a record, not '{}'",
                other.type_name()
            ))),
            _ => Err(ExprError::type_error(format!(
                "browse() takes at most 1 argument ({} given)",
                args.len()
            ))),
        }
    }
}

impl ExprObject for ModelProxy {
    fn type_name(&self) -> &str {
        &self.model
    }

    fn repr(&self) -> String {
        format!("{}()", self.model)
    }

    fn is_truthy(&self) -> bool {
        false
    }

    fn record_ids(&self) -> Option<std::borrow::Cow<'_, [i64]>> {
        Some(std::borrow::Cow::Borrowed(&[]))
    }

    fn py_eq(&self, other: &dyn ExprObject) -> bool {
        records_eq(self, other)
    }

    fn with_hooks(&self, hooks: Arc<HostHooks>) -> Option<Arc<dyn ExprObject>> {
        Some(Arc::new(Self {
            model: Arc::clone(&self.model),
            hooks,
        }))
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        ExprCallable::call(self, args, kwargs, budget)
    }

    fn get_attr(&self, name: &str, _budget: &mut Budget) -> ExprResult<ExprValue> {
        match name {
            "env" => Ok(HostHooks::env(&self.hooks)),
            "sudo" | "with_context" => Ok(context_method(
                Arc::new(self.clone()),
                Arc::clone(&self.hooks),
                name,
            )),
            "search" => Ok(ExprValue::Callable(Arc::new(SearchMethod {
                model: Arc::clone(&self.model),
                hooks: Arc::clone(&self.hooks),
            }))),
            "browse" => Ok(ExprValue::Callable(Arc::new(self.clone()))),
            "id" => Ok(ExprValue::Bool(false)),
            "ids" => Ok(ExprValue::list(vec![])),
            _ => Err(ExprError::attribute_error(self.type_name(), name)),
        }
    }
}

// ---- RecordProxy ---------------------------------------------------------------------------

/// A proxy for a single record `(model, id)`; the id is always a positive database id.
#[derive(Clone)]
pub struct RecordProxy {
    model: Arc<str>,
    id: i64,
    hooks: Arc<HostHooks>,
}

impl RecordProxy {
    /// The record `id` of `model`, with default host callbacks.
    ///
    /// # Errors
    /// `ValueError` when `id` is not a positive integer.
    pub fn new(model: impl Into<Arc<str>>, id: i64) -> ExprResult<Self> {
        let model = model.into();
        let id = record_id(&model, id)?;
        Ok(Self {
            model,
            id,
            hooks: Arc::new(HostHooks::default()),
        })
    }

    /// The record `id` of `model` with the provided host hooks.
    pub fn with_hooks(
        model: impl Into<Arc<str>>,
        id: i64,
        hooks: Arc<HostHooks>,
    ) -> ExprResult<Self> {
        let model = model.into();
        let id = record_id(&model, id)?;
        Ok(Self { model, id, hooks })
    }

    /// The database id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The host hooks.
    pub fn hooks(&self) -> &Arc<HostHooks> {
        &self.hooks
    }
}

impl fmt::Debug for RecordProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({},)", self.model, self.id)
    }
}

impl ExprObject for RecordProxy {
    fn type_name(&self) -> &str {
        &self.model
    }

    fn repr(&self) -> String {
        format!("{}({},)", self.model, self.id)
    }

    fn is_truthy(&self) -> bool {
        true
    }

    fn is_record(&self) -> bool {
        true
    }

    fn as_int(&self) -> ExprResult<i64> {
        Ok(self.id)
    }

    fn with_hooks(&self, hooks: Arc<HostHooks>) -> Option<Arc<dyn ExprObject>> {
        Some(Arc::new(Self {
            model: Arc::clone(&self.model),
            id: self.id,
            hooks,
        }))
    }

    fn record_ids(&self) -> Option<std::borrow::Cow<'_, [i64]>> {
        Some(std::borrow::Cow::Owned(vec![self.id]))
    }

    fn py_eq(&self, other: &dyn ExprObject) -> bool {
        records_eq(self, other)
    }

    fn get_item(&self, key: &ExprValue, _budget: &mut Budget) -> ExprResult<ExprValue> {
        match key {
            ExprValue::Int(0 | -1) => Ok(ExprValue::Object(Arc::new(self.clone()))),
            ExprValue::Int(_) => Err(ExprError::index_error("recordset index out of range")),
            other => Err(ExprError::type_error(format!(
                "recordset indices must be integers, not '{}'",
                other.type_name()
            ))),
        }
    }

    fn get_attr(&self, name: &str, budget: &mut Budget) -> ExprResult<ExprValue> {
        match name {
            "id" => Ok(ExprValue::Int(self.id)),
            "ids" => Ok(ExprValue::list(vec![ExprValue::Int(self.id)])),
            "env" => Ok(HostHooks::env(&self.hooks)),
            "sudo" | "with_context" => {
                budget.alloc(SLOT)?;
                Ok(context_method(
                    Arc::new(self.clone()),
                    Arc::clone(&self.hooks),
                    name,
                ))
            }
            "has_group" if self.model.as_ref() == "res.users" => {
                budget.alloc(SLOT)?;
                Ok(ExprValue::Callable(Arc::new(HasGroupMethod {
                    hooks: Arc::clone(&self.hooks),
                    user: self.id,
                })))
            }
            _ => HostHooks::read(&self.hooks, &self.model, self.id, name),
        }
    }
}

// ---- RecordsetProxy ------------------------------------------------------------------------

/// A recordset of `<model>`: positive database ids, at most [`MAX_SEARCH_RESULTS`] of them.
#[derive(Clone)]
pub struct RecordsetProxy {
    model: Arc<str>,
    ids: Vec<i64>,
    hooks: Arc<HostHooks>,
}

impl RecordsetProxy {
    /// The records `ids` of `model`, with default host callbacks.
    ///
    /// # Errors
    /// `ValueError` for an id that is not a positive integer; `LimitExceeded` for more than
    /// [`MAX_SEARCH_RESULTS`] ids.
    pub fn new(model: impl Into<Arc<str>>, ids: Vec<i64>) -> ExprResult<Self> {
        let model = model.into();
        check_count(&model, "a recordset", ids.len())?;
        for id in &ids {
            record_id(&model, *id)?;
        }
        Ok(Self {
            model,
            ids,
            hooks: Arc::new(HostHooks::default()),
        })
    }

    /// The records `ids` of `model` with the provided host hooks.
    pub fn with_hooks(
        model: impl Into<Arc<str>>,
        ids: Vec<i64>,
        hooks: Arc<HostHooks>,
    ) -> ExprResult<Self> {
        let model = model.into();
        check_count(&model, "a recordset", ids.len())?;
        for id in &ids {
            record_id(&model, *id)?;
        }
        Ok(Self { model, ids, hooks })
    }

    /// The database ids.
    pub fn ids(&self) -> &[i64] {
        &self.ids
    }

    /// The model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The host hooks.
    pub fn hooks(&self) -> &Arc<HostHooks> {
        &self.hooks
    }
}

impl fmt::Debug for RecordsetProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({:?})", self.model, self.ids)
    }
}

impl ExprObject for RecordsetProxy {
    fn type_name(&self) -> &str {
        &self.model
    }

    fn repr(&self) -> String {
        match self.ids.as_slice() {
            [] => format!("{}()", self.model),
            [one] => format!("{}({one},)", self.model),
            ids => {
                let ids_str = ids
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}({ids_str})", self.model)
            }
        }
    }

    fn is_truthy(&self) -> bool {
        !self.ids.is_empty()
    }

    fn is_record(&self) -> bool {
        true
    }

    /// One slot per id: the evaluator charges a recordset a host produced like a list of ints.
    fn meta(&self) -> Meta {
        Meta {
            bytes: SLOT.saturating_add(self.ids.len().saturating_mul(SLOT)),
            nodes: self.ids.len().saturating_add(1),
            depth: 1,
        }
    }

    fn with_hooks(&self, hooks: Arc<HostHooks>) -> Option<Arc<dyn ExprObject>> {
        Some(Arc::new(Self {
            model: Arc::clone(&self.model),
            ids: self.ids.clone(),
            hooks,
        }))
    }

    fn record_ids(&self) -> Option<std::borrow::Cow<'_, [i64]>> {
        Some(std::borrow::Cow::Borrowed(&self.ids))
    }

    fn py_eq(&self, other: &dyn ExprObject) -> bool {
        records_eq(self, other)
    }

    fn as_int(&self) -> ExprResult<i64> {
        match self.ids.as_slice() {
            [one] => Ok(*one),
            ids => Err(ExprError::value_error(format!(
                "Expected singleton: {}({})",
                self.model,
                ids.len()
            ))),
        }
    }

    fn get_item(&self, key: &ExprValue, _budget: &mut Budget) -> ExprResult<ExprValue> {
        let ExprValue::Int(idx) = key else {
            return Err(ExprError::type_error(format!(
                "recordset indices must be integers, not '{}'",
                key.type_name()
            )));
        };
        let len = i64::try_from(self.ids.len())
            .map_err(|_| ExprError::overflow("recordset length out of range"))?;
        let actual = if *idx < 0 { len + idx } else { *idx };
        match usize::try_from(actual).ok().and_then(|i| self.ids.get(i)) {
            Some(id) => Ok(HostHooks::record(&self.hooks, &self.model, *id)),
            None => Err(ExprError::index_error("recordset index out of range")),
        }
    }

    fn get_slice(
        &self,
        lower: Option<i64>,
        upper: Option<i64>,
        step: Option<i64>,
        budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        let (start, step_val, count) =
            crate::methods::slice_plan(self.ids.len(), lower, upper, step)?;
        budget.alloc(count.saturating_mul(SLOT))?;
        let mut sliced = Vec::with_capacity(count);
        for i in crate::methods::slice_indices(start, step_val, count) {
            sliced.push(self.ids[i]);
        }
        Ok(HostHooks::recordset(&self.hooks, &self.model, sliced))
    }

    fn get_attr(&self, name: &str, budget: &mut Budget) -> ExprResult<ExprValue> {
        match name {
            "id" => Ok(self
                .ids
                .first()
                .copied()
                .map_or(ExprValue::Bool(false), ExprValue::Int)),
            "ids" => Ok(ExprValue::list(
                self.ids.iter().map(|&i| ExprValue::Int(i)).collect(),
            )),
            "env" => Ok(HostHooks::env(&self.hooks)),
            "sudo" | "with_context" => {
                budget.alloc(self.meta().bytes)?;
                Ok(context_method(
                    Arc::new(self.clone()),
                    Arc::clone(&self.hooks),
                    name,
                ))
            }
            "mapped" => {
                budget.alloc(self.meta().bytes)?;
                Ok(ExprValue::Callable(Arc::new(MappedMethod {
                    model: Arc::clone(&self.model),
                    ids: self.ids.clone(),
                    hooks: Arc::clone(&self.hooks),
                })))
            }
            _ => match self.ids.as_slice() {
                [one] => RecordProxy {
                    model: Arc::clone(&self.model),
                    id: *one,
                    hooks: Arc::clone(&self.hooks),
                }
                .get_attr(name, budget),
                _ => Err(ExprError::attribute_error(self.type_name(), name)),
            },
        }
    }
}

// ---- EnvProxy ------------------------------------------------------------------------------

/// Proxy for `env` (accessed through `obj().env`): `env.ref(xid)`, `env['model']`, `env.user`, etc.
#[derive(Clone)]
pub struct EnvProxy {
    hooks: Arc<HostHooks>,
}

impl fmt::Debug for EnvProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<Environment object>")
    }
}

impl ExprObject for EnvProxy {
    fn type_name(&self) -> &str {
        "Environment"
    }

    fn repr(&self) -> String {
        "<Environment object>".to_string()
    }

    fn with_hooks(&self, hooks: Arc<HostHooks>) -> Option<Arc<dyn ExprObject>> {
        Some(Arc::new(Self { hooks }))
    }

    fn get_item(&self, key: &ExprValue, _budget: &mut Budget) -> ExprResult<ExprValue> {
        let Some(model) = key.as_str() else {
            return Err(ExprError::type_error(format!(
                "Environment keys must be strings, not '{}'",
                key.type_name()
            )));
        };
        Ok(ExprValue::Object(Arc::new(ModelProxy::with_hooks(
            Arc::from(model),
            Arc::clone(&self.hooks),
        ))))
    }

    fn get_attr(&self, name: &str, _budget: &mut Budget) -> ExprResult<ExprValue> {
        match name {
            "ref" => Ok(ExprValue::Callable(Arc::new(EnvRefMethod {
                hooks: Arc::clone(&self.hooks),
            }))),
            "user" => {
                let Some(uid) = self.hooks.user_id else {
                    return Err(host_missing("env.user", "the current user"));
                };
                Ok(HostHooks::record(&self.hooks, &Arc::from("res.users"), uid))
            }
            "uid" => {
                let Some(uid) = self.hooks.user_id else {
                    return Err(host_missing("env.uid", "the current user id"));
                };
                Ok(ExprValue::Int(uid))
            }
            "company" => {
                if let Some(cid) = self.hooks.company_id {
                    if cid > 0 {
                        Ok(HostHooks::record(
                            &self.hooks,
                            &Arc::from("res.company"),
                            cid,
                        ))
                    } else {
                        Ok(HostHooks::recordset(
                            &self.hooks,
                            &Arc::from("res.company"),
                            Vec::new(),
                        ))
                    }
                } else if self.hooks.user_id.is_some()
                    || !matches!(self.hooks.context, ExprValue::None)
                {
                    Ok(HostHooks::recordset(
                        &self.hooks,
                        &Arc::from("res.company"),
                        Vec::new(),
                    ))
                } else {
                    Err(host_missing("env.company", "the current company"))
                }
            }
            "companies" => {
                if self.hooks.company_ids.is_empty()
                    && self.hooks.company_id.is_none()
                    && self.hooks.user_id.is_none()
                    && matches!(self.hooks.context, ExprValue::None)
                {
                    return Err(host_missing("env.companies", "the allowed companies"));
                }
                Ok(HostHooks::recordset(
                    &self.hooks,
                    &Arc::from("res.company"),
                    self.hooks.company_ids.clone(),
                ))
            }
            "context" => {
                if matches!(self.hooks.context, ExprValue::None) {
                    return Err(host_missing("env.context", "the evaluation context"));
                }
                Ok(self.hooks.context.clone())
            }
            "su" => Ok(ExprValue::Bool(self.hooks.sudo)),
            _ => Err(ExprError::attribute_error(self.type_name(), name)),
        }
    }
}

#[derive(Clone)]
struct EnvRefMethod {
    hooks: Arc<HostHooks>,
}

impl fmt::Debug for EnvRefMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in method ref of Environment>")
    }
}

impl ExprCallable for EnvRefMethod {
    fn name(&self) -> &str {
        "ref"
    }

    /// `env.ref(xml_id)`: the record the external id names, of the model it was created in.
    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        if !kwargs.is_empty() {
            return Err(ExprError::type_error(
                "ref() does not take keyword arguments",
            ));
        }
        let [ExprValue::Str(xml_id)] = args else {
            return Err(ExprError::type_error("ref() requires an XML ID string"));
        };
        let Some(xid_model) = &self.hooks.xid_model else {
            return Err(ExprError::unavailable(
                "env.ref",
                "the model of an external id is provided by the data loader \
                 (EvalContext::with_xid_model_resolver)",
            ));
        };
        if let Some((model, id)) = xid_model(xml_id) {
            let model: Arc<str> = Arc::from(model.as_str());
            let id = record_id(&model, id)?;
            return Ok(HostHooks::record(&self.hooks, &model, id));
        }
        if let Some(reference) = &self.hooks.reference {
            reference(xml_id, true)?;
            return Err(ExprError::value_error(format!(
                "External ID {xml_id} has a record id but no model"
            )));
        }
        Err(ExprError::value_error(format!(
            "External ID not found in the system: {xml_id}"
        )))
    }
}

#[derive(Clone)]
struct SearchMethod {
    model: Arc<str>,
    hooks: Arc<HostHooks>,
}

impl fmt::Debug for SearchMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in method search of {}>", self.model)
    }
}

/// `limit` of `search()`: `None` or a positive integer.
fn search_limit(value: &ExprValue) -> ExprResult<Option<u64>> {
    match value {
        ExprValue::None => Ok(None),
        ExprValue::Int(n) if *n > 0 => u64::try_from(*n)
            .map(Some)
            .map_err(|_| ExprError::overflow("search() limit out of range")),
        ExprValue::Int(n) => Err(ExprError::value_error(format!(
            "search() limit must be a positive integer or None, got {n}"
        ))),
        other => Err(ExprError::type_error(format!(
            "search() limit must be an integer or None, not '{}'",
            other.type_name()
        ))),
    }
}

impl ExprCallable for SearchMethod {
    fn name(&self) -> &str {
        "search"
    }

    /// `search(domain, offset=0, limit=None, order=None)`.
    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        if args.len() > 4 {
            return Err(ExprError::type_error(format!(
                "search() takes at most 4 positional arguments ({} given)",
                args.len()
            )));
        }
        let mut params: [Option<&ExprValue>; 4] = [None; 4];
        for (slot, value) in params.iter_mut().zip(args) {
            *slot = Some(value);
        }
        for (key, value) in kwargs.iter() {
            let pos = match key.as_str() {
                "domain" => 0,
                "offset" => 1,
                "limit" => 2,
                "order" => 3,
                other => {
                    return Err(ExprError::type_error(format!(
                        "search() got an unexpected keyword argument '{other}'"
                    )));
                }
            };
            if params[pos].is_some() {
                return Err(ExprError::type_error(format!(
                    "search() got multiple values for argument '{key}'"
                )));
            }
            params[pos] = Some(value);
        }
        let [domain, offset, limit, order] = params;
        let Some(domain) = domain else {
            return Err(ExprError::type_error(
                "search() missing 1 required positional argument: 'domain'",
            ));
        };
        let offset = match offset {
            None | Some(ExprValue::None) => 0,
            Some(ExprValue::Int(n)) if *n >= 0 => u64::try_from(*n)
                .map_err(|_| ExprError::overflow("search() offset out of range"))?,
            Some(ExprValue::Int(n)) => {
                return Err(ExprError::value_error(format!(
                    "search() offset must be a non-negative integer or None, got {n}"
                )));
            }
            Some(other) => {
                return Err(ExprError::type_error(format!(
                    "search() offset must be an integer or None, not '{}'",
                    other.type_name()
                )));
            }
        };
        let limit = match limit {
            Some(value) => search_limit(value)?,
            None => None,
        };
        let order = match order {
            None | Some(ExprValue::None) => None,
            Some(ExprValue::Str(s)) => Some(s.as_str()),
            Some(other) => {
                return Err(ExprError::type_error(format!(
                    "search() order must be a string or None, not '{}'",
                    other.type_name()
                )));
            }
        };
        let Some(res) = &self.hooks.search else {
            return Err(ExprError::value_error(
                "search() is not available in this context",
            ));
        };
        let query = SearchQuery {
            model: &self.model,
            domain,
            offset,
            limit,
            order,
            context: &self.hooks.context,
            sudo: self.hooks.sudo,
        };
        let ids = res(&query)?;
        check_count(&self.model, "search()", ids.len())?;
        for id in &ids {
            record_id(&self.model, *id)?;
        }
        Ok(HostHooks::recordset(&self.hooks, &self.model, ids))
    }
}

#[derive(Debug, Clone, Copy)]
enum SelfKind {
    Sudo,
    WithContext,
}

#[derive(Clone)]
struct SelfMethod {
    target: Arc<dyn ExprObject>,
    hooks: Arc<HostHooks>,
    kind: SelfKind,
}

impl fmt::Debug for SelfMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in method {} of {:?}>", self.name(), self.target)
    }
}

impl ExprCallable for SelfMethod {
    fn name(&self) -> &str {
        match self.kind {
            SelfKind::Sudo => "sudo",
            SelfKind::WithContext => "with_context",
        }
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        match self.kind {
            SelfKind::Sudo => {
                let flag = match (args, kwargs.is_empty()) {
                    ([], true) => true,
                    ([ExprValue::Bool(b)], true) => *b,
                    ([other], true) => {
                        return Err(ExprError::type_error(format!(
                            "sudo() flag must be a boolean, not '{}'",
                            other.type_name()
                        )));
                    }
                    ([], false) if kwargs.len() == 1 => match kwargs.get_index(0) {
                        Some((key, ExprValue::Bool(b))) if key == "flag" => *b,
                        Some((key, other)) if key == "flag" => {
                            return Err(ExprError::type_error(format!(
                                "sudo() flag must be a boolean, not '{}'",
                                other.type_name()
                            )));
                        }
                        Some((key, _)) => {
                            return Err(ExprError::type_error(format!(
                                "sudo() got an unexpected keyword argument '{key}'"
                            )));
                        }
                        None => true,
                    },
                    _ => {
                        return Err(ExprError::type_error(
                            "sudo() takes at most 1 argument ('flag')",
                        ));
                    }
                };
                if flag == self.hooks.sudo {
                    return Ok(ExprValue::Object(Arc::clone(&self.target)));
                }
                let mut new_hooks = (*self.hooks).clone();
                new_hooks.sudo = flag;
                let new_hooks = Arc::new(new_hooks);
                self.target
                    .with_hooks(new_hooks)
                    .ok_or_else(|| {
                        ExprError::value_error("object does not support attaching hooks in sudo")
                    })
                    .map(ExprValue::Object)
            }
            SelfKind::WithContext => {
                if args.is_empty() && kwargs.is_empty() {
                    return Ok(ExprValue::Object(Arc::clone(&self.target)));
                }
                if args.len() > 1 {
                    return Err(ExprError::type_error(format!(
                        "with_context() takes at most 1 positional argument ({} given)",
                        args.len()
                    )));
                }
                let mut pairs: Vec<(ExprValue, ExprValue)> = Vec::new();
                match args.first() {
                    Some(ExprValue::None) | None => {
                        if let ExprValue::Dict(d) = &self.hooks.context {
                            for (k, v) in d.iter() {
                                pairs.push((k.clone(), v.clone()));
                            }
                        }
                    }
                    Some(ExprValue::Dict(d)) => {
                        for (k, v) in d.iter() {
                            pairs.push((k.clone(), v.clone()));
                        }
                        if d.get_str("allowed_company_ids").is_none() {
                            let allowed = match &self.hooks.context {
                                ExprValue::Dict(cur) => cur.get_str("allowed_company_ids"),
                                _ => None,
                            };
                            if let Some(val) = allowed {
                                pairs.push((ExprValue::str("allowed_company_ids"), val.clone()));
                            }
                        }
                    }
                    Some(other) => {
                        return Err(ExprError::type_error(format!(
                            "with_context() argument must be a dict, not '{}'",
                            other.type_name()
                        )));
                    }
                }
                for (k, v) in kwargs.iter() {
                    pairs.push((ExprValue::str(k.as_str()), v.clone()));
                }
                let new_context = ExprValue::dict(pairs)?;
                let mut new_hooks = (*self.hooks).clone();
                new_hooks.context = new_context;
                let new_hooks = Arc::new(new_hooks);
                self.target
                    .with_hooks(new_hooks)
                    .ok_or_else(|| {
                        ExprError::value_error(
                            "object does not support attaching hooks in with_context",
                        )
                    })
                    .map(ExprValue::Object)
            }
        }
    }
}

/// Method callable for `user.has_group(group_xmlid)`.
#[derive(Clone)]
struct HasGroupMethod {
    hooks: Arc<HostHooks>,
    /// The `res.users` record the method is bound to.
    user: i64,
}

impl fmt::Debug for HasGroupMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in method has_group of res.users>")
    }
}

impl ExprCallable for HasGroupMethod {
    fn name(&self) -> &str {
        "has_group"
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        if !kwargs.is_empty() {
            return Err(ExprError::type_error(
                "has_group() does not take keyword arguments",
            ));
        }
        let [ExprValue::Str(xmlid)] = args else {
            return Err(ExprError::type_error(
                "has_group() requires a group XML ID string",
            ));
        };
        if xmlid.is_empty() {
            return Err(ExprError::value_error(
                "has_group() needs a non-empty group XML ID",
            ));
        }
        // The current user keeps the plain checker. Another user's record is answered by the
        // per-user checker, never by the current user's membership; without one it refuses.
        if self.hooks.user_id != Some(self.user) {
            if let Some(checker) = &self.hooks.user_group_check {
                return checker(self.user, xmlid).map(ExprValue::Bool);
            }
            let current = self
                .hooks
                .user_id
                .map_or_else(|| "not configured".to_owned(), |uid| uid.to_string());
            return Err(ExprError::value_error(format!(
                "has_group() on res.users({}) is not supported: this host sets no per-user group \
                 checker (HostHooks::with_user_group_checker) and answers only for the current \
                 user (env.uid={current})",
                self.user
            )));
        }
        self.hooks.has_group(xmlid).map(ExprValue::Bool)
    }
}

/// Function callable for top-level `has_group(group_xmlid)`.
#[derive(Clone)]
pub(crate) struct HasGroupFunction(pub(crate) Arc<HostHooks>);

impl fmt::Debug for HasGroupFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<function has_group>")
    }
}

impl ExprCallable for HasGroupFunction {
    fn name(&self) -> &str {
        "has_group"
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        if !kwargs.is_empty() {
            return Err(ExprError::type_error(
                "has_group() does not take keyword arguments",
            ));
        }
        let [ExprValue::Str(xmlid)] = args else {
            return Err(ExprError::type_error(
                "has_group() requires a group XML ID string",
            ));
        };
        if xmlid.is_empty() {
            return Err(ExprError::value_error(
                "has_group() needs a non-empty group XML ID",
            ));
        }
        self.0.has_group(xmlid).map(ExprValue::Bool)
    }
}

#[derive(Clone)]
struct MappedMethod {
    model: Arc<str>,
    ids: Vec<i64>,
    hooks: Arc<HostHooks>,
}

impl fmt::Debug for MappedMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in method mapped of {}>", self.model)
    }
}

impl ExprCallable for MappedMethod {
    fn name(&self) -> &str {
        "mapped"
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        if !kwargs.is_empty() {
            return Err(ExprError::type_error(
                "mapped() does not take keyword arguments",
            ));
        }
        let [ExprValue::Str(field)] = args else {
            return Err(ExprError::type_error(
                "mapped() requires a field name string",
            ));
        };
        let mut results = Vec::with_capacity(self.ids.len());
        // an unresolved read (the host's lookup round has not fetched it yet) does not stop the
        // loop: every record is asked for, so one lookup round fetches all the values, and the
        // first error is reported at the end (a budget error stops at once, as before)
        let mut first_error = None;
        for id in &self.ids {
            budget.tick()?;
            let value = match HostHooks::read(&self.hooks, &self.model, *id, field) {
                Ok(value) => value,
                Err(e) if e.class() == "LimitExceeded" => return Err(e),
                Err(e) => {
                    first_error.get_or_insert(e);
                    continue;
                }
            };
            // each value is charged before the next read, so a budget a few values fill stops
            // the loop there (review CF-1 of the EXPR-CLOSE fixes); the evaluator charges the
            // returned list once more, a deliberate double count that keeps the bound simple
            budget.admit(&value)?;
            budget.alloc(crate::value::SLOT)?;
            results.push(value);
        }
        if let Some(e) = first_error {
            return Err(e);
        }
        Ok(ExprValue::list(results))
    }
}
