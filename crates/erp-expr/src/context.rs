//! The host's evaluation context: the names an expression can see, the limits, the clock.
//!
//! An `EvalContext` is never modified by evaluation: comprehension variables and `:=` bindings live
//! in an evaluation-local scope (review round 2: "walrus never writes to the host context"), so one
//! context can be shared by any number of evaluations and threads.

use std::sync::Arc;

use indexmap::IndexMap;
use time::PrimitiveDateTime;

use crate::args::{Sig, required, str_arg};
use crate::builtins::command::CommandNamespace;
use crate::builtins::{dates, python_builtins};
use crate::error::{ExprError, ExprResult};
use crate::limits::{Budget, EvalLimits};
use crate::value::{ExprCallable, ExprValue, Kwargs};

pub type Clock = Arc<dyn Fn() -> PrimitiveDateTime + Send + Sync>;

/// Resolver of `ref(xml_id, raise_if_not_found=True)`, provided by the data loader.
pub type RefResolver = Arc<dyn Fn(&str, bool) -> ExprResult<ExprValue> + Send + Sync>;

use crate::proxy::{HasGroupFunction, HostHooks, ModelProxy, SearchQuery};

/// Names, limits and clock of an evaluation. Cheap to clone (values are shared).
#[derive(Clone)]
pub struct EvalContext {
    vars: IndexMap<String, ExprValue>,
    limits: EvalLimits,
    clock: Option<Clock>,
    model: Option<Arc<str>>,
    hooks: Arc<HostHooks>,
}

impl std::fmt::Debug for EvalContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalContext")
            .field("names", &self.vars.keys().collect::<Vec<_>>())
            .field("limits", &self.limits)
            .field("clock", &self.clock.is_some())
            .field("model", &self.model)
            .field("hooks", &self.hooks)
            .finish()
    }
}

impl Default for EvalContext {
    fn default() -> Self {
        Self::standard()
    }
}

pub(crate) fn host_provided(name: &str) -> Option<&'static str> {
    match name {
        "obj" => Some("record access is provided by the data loader (EvalContext::with_model)"),
        "env" => Some("environment access is provided via obj().env (EvalContext::with_model)"),
        "user" | "has_group" => {
            Some("user and group checks are provided by the host and are not available before M3")
        }
        "ref" => Some(
            "XML-id references are provided by the data loader (EvalContext::with_ref_resolver)",
        ),
        "version" => {
            Some("the the reference ERP series is provided by the host (EvalContext::set)")
        }
        _ => None,
    }
}

impl EvalContext {
    /// A context with no names at all.
    pub fn empty() -> Self {
        Self {
            vars: IndexMap::new(),
            limits: EvalLimits::default(),
            clock: None,
            model: None,
            hooks: Arc::new(HostHooks::default()),
        }
    }

    /// No `ref`, `obj`, `version` or clock:
    /// the host adds those.
    pub fn standard() -> Self {
        let mut ctx = Self::empty();
        for (name, value) in python_builtins() {
            ctx.vars.insert(name.to_string(), value);
        }
        ctx.vars.insert(
            "Command".to_string(),
            ExprValue::Object(Arc::new(CommandNamespace)),
        );
        for (name, value) in dates::date_names(None) {
            ctx.vars.insert(name.to_string(), value);
        }
        ctx
    }

    /// Limits for evaluations with this context. [`EvalLimits`] can only hold values at or below
    /// the ceiling, so this can tighten but never lift a limit.
    pub fn with_limits(mut self, limits: EvalLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> EvalLimits {
        self.limits
    }

    /// Installs a clock and (re)binds `time`, `datetime` and `DateTime` to use it.
    pub fn with_clock(
        mut self,
        clock: impl Fn() -> PrimitiveDateTime + Send + Sync + 'static,
    ) -> Self {
        let clock: Clock = Arc::new(clock);
        self.clock = Some(clock.clone());
        for (name, value) in dates::date_names(Some(clock)) {
            if matches!(name, "time" | "datetime" | "DateTime") {
                self.vars.insert(name.to_string(), value);
            }
        }
        self
    }

    /// Configures host callbacks and environment state in one bundle (Review S5).
    pub fn with_host_hooks(mut self, hooks: Arc<HostHooks>) -> Self {
        if hooks.group_check.is_some() {
            self.vars.insert(
                "has_group".to_string(),
                ExprValue::Callable(Arc::new(HasGroupFunction(Arc::clone(&hooks)))),
            );
        }
        if let Some(reference) = &hooks.reference {
            self.vars.insert(
                "ref".to_string(),
                ExprValue::Callable(Arc::new(RefFunction(Arc::clone(reference)))),
            );
        }
        self.hooks = hooks;
        self.rebind_obj();
        self
    }

    /// Accessor for current host hooks.
    pub fn hooks(&self) -> &Arc<HostHooks> {
        &self.hooks
    }

    /// Binds `ref(xml_id, raise_if_not_found=True)` to a resolver.
    pub fn with_ref_resolver(
        mut self,
        resolver: impl Fn(&str, bool) -> ExprResult<ExprValue> + Send + Sync + 'static,
    ) -> Self {
        let resolver: RefResolver = Arc::new(resolver);
        self.vars.insert(
            "ref".to_string(),
            ExprValue::Callable(Arc::new(RefFunction(Arc::clone(&resolver)))),
        );
        let mut hooks = (*self.hooks).clone();
        hooks.reference = Some(resolver);
        self.hooks = Arc::new(hooks);
        self.rebind_obj();
        self
    }

    fn rebind_obj(&mut self) {
        if let Some(model) = &self.model {
            let proxy = ModelProxy::with_hooks(Arc::clone(model), Arc::clone(&self.hooks));
            self.set("obj", ExprValue::Object(Arc::new(proxy)));
        }
    }

    /// Binds `<model>` to `obj()` in the context.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(Arc::from(model.into().as_str()));
        self.rebind_obj();
        self
    }

    /// Configures record attribute/field resolution.
    pub fn with_record_resolver(
        mut self,
        resolver: impl Fn(&str, i64, &str) -> ExprResult<ExprValue> + Send + Sync + 'static,
    ) -> Self {
        let mut hooks = (*self.hooks).clone();
        hooks.record = Some(Arc::new(resolver));
        self.hooks = Arc::new(hooks);
        self.rebind_obj();
        self
    }

    /// Configures model search resolution.
    pub fn with_search_resolver(
        mut self,
        resolver: impl Fn(&SearchQuery<'_>) -> ExprResult<Vec<i64>> + Send + Sync + 'static,
    ) -> Self {
        let mut hooks = (*self.hooks).clone();
        hooks.search = Some(Arc::new(resolver));
        self.hooks = Arc::new(hooks);
        self.rebind_obj();
        self
    }

    /// Configures XML ID to `(model, id)` resolution.
    pub fn with_xid_model_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<(String, i64)> + Send + Sync + 'static,
    ) -> Self {
        let mut hooks = (*self.hooks).clone();
        hooks.xid_model = Some(Arc::new(resolver));
        self.hooks = Arc::new(hooks);
        self.rebind_obj();
        self
    }

    /// Configures group membership check resolution.
    pub fn with_group_checker(
        self,
        checker: impl Fn(&str) -> ExprResult<bool> + Send + Sync + 'static,
    ) -> Self {
        let mut hooks = (*self.hooks).clone();
        hooks.group_check = Some(Arc::new(checker));
        self.with_host_hooks(Arc::new(hooks))
    }

    /// Binds a name (replacing any previous binding).
    pub fn set(&mut self, name: impl Into<String>, value: ExprValue) {
        self.vars.insert(name.into(), value);
    }

    /// Builder form of [`EvalContext::set`].
    pub fn with_var(mut self, name: impl Into<String>, value: ExprValue) -> Self {
        self.set(name, value);
        self
    }

    /// Binds a host function. Its results are charged to the budget by the evaluator.
    pub fn set_function<F>(&mut self, name: impl Into<String>, f: F)
    where
        F: Fn(&[ExprValue], &Kwargs, &mut Budget) -> ExprResult<ExprValue> + Send + Sync + 'static,
    {
        let name = name.into();
        self.set(
            name.clone(),
            ExprValue::Callable(Arc::new(HostFunction { name, f })),
        );
    }

    pub fn get(&self, name: &str) -> Option<&ExprValue> {
        self.vars.get(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<ExprValue> {
        self.vars.shift_remove(name)
    }
}

struct HostFunction<F> {
    name: String,
    f: F,
}

impl<F> std::fmt::Debug for HostFunction<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<function {}>", self.name)
    }
}

impl<F> ExprCallable for HostFunction<F>
where
    F: Fn(&[ExprValue], &Kwargs, &mut Budget) -> ExprResult<ExprValue> + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        (self.f)(args, kwargs, budget)
    }
}

struct RefFunction(RefResolver);

impl std::fmt::Debug for RefFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<function ref>")
    }
}

impl ExprCallable for RefFunction {
    fn name(&self) -> &str {
        "ref"
    }

    fn call(
        &self,
        args: &[ExprValue],
        kwargs: &Kwargs,
        _budget: &mut Budget,
    ) -> ExprResult<ExprValue> {
        let sig = Sig {
            name: "ref",
            params: &["xml_id", "raise_if_not_found"],
            required: 1,
            pos_only: 0,
            max_pos: 2,
        };
        let slots = sig.bind(args, kwargs)?;
        let xml_id = str_arg("ref", "xml_id", required(&sig, &slots, 0)?)?;
        let raise = match slots.get(1).copied().flatten() {
            None => true,
            Some(v) => v.is_truthy(),
        };
        if xml_id.is_empty() {
            return Err(ExprError::value_error("ref() needs a non-empty XML id"));
        }
        (self.0)(xml_id, raise)
    }
}
