//! An override is a synchronous function in a `#[methods(Model)]` block:
//!
//! ```text
//! #[field_access(context = ["project_sharing_create"])]
//! fn _field_access(ctx: &FieldAccessCtx<'_>, op: FieldOp, mask: FieldMask, sup: FieldAccessSuper<'_>)
//!     -> Result<FieldMask>
//! ```
//!
//! Overrides of one model form a chain in method-resolution order, newest first, like
//! `#[method(override)]`; the bottom of the chain returns the mask it is given (the
//! policy's group-derived mask). An override calls `sup.call(ctx, op, mask)` and may then narrow
//! what comes back: **it may only narrow**. A bit it sets that the mask below it did not set is a
//! `Programming` error naming the override and the field. It sees no principal values (no uid, no
//! companies), only the [`FieldAccessCtx`]: the model, the principal's kind, group membership and
//! the values of the context keys it declared, so its result depends on nothing the access memo
//! key does not cover. Reading an undeclared context key is a `Programming` error.
//!
//! The chain runs twice per mask, once per [`FieldOp`]: the `readable` set of the result is what
//! the `Read` run leaves, the `writable` set what the `Write` run leaves (the other set of each run
//! is dropped, so an override branches on `op` and narrows that operation's set).

use std::cell::RefCell;
use std::sync::Arc;

use erp_fields::{Error, Result, Value};
use erp_registry::{FieldId, ModelId, ModelSpec, Registry};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::access::{AccessCtx, ElevationKind};
use crate::field_access::{FieldMask, FieldOp};
use crate::principal::KindClass;

/// The erased signature of a `#[field_access]` function.
pub type FieldAccessFn = for<'c, 'a, 's> fn(
    &'c FieldAccessCtx<'a>,
    FieldOp,
    FieldMask,
    FieldAccessSuper<'s>,
) -> Result<FieldMask>;

/// One override contributed by an addon (emitted by `#[field_access]`).
#[derive(Debug)]
pub struct FieldAccessImpl {
    /// Contributing addon.
    pub addon: &'static str,
    /// Model of the declaration it belongs to (`_name` of the `#[methods]` target).
    pub model: &'static str,
    /// Function name.
    pub method: &'static str,
    /// Context keys the override reads.
    pub context: &'static [&'static str],
    /// The function.
    pub run: FieldAccessFn,
    /// Source file.
    pub file: &'static str,
    /// Source line.
    pub line: u32,
}

impl FieldAccessImpl {
    fn location(&self) -> String {
        format!("{}:{} (addon '{}')", self.file, self.line, self.addon)
    }
}

/// Overrides linked into the binary.
#[linkme::distributed_slice]
pub static FIELD_ACCESS_IMPLS: [FieldAccessImpl];

/// The values of the context keys `#[field_access]` overrides declared, read from an
/// environment's context once per binding ([`crate::PrincipalBinding::field_ctx`]). Empty when no
/// installed override declares a key, which is the common case and costs nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct FieldContextValues {
    values: Vec<(&'static str, Option<Value>)>,
}

impl FieldContextValues {
    /// The values of `keys` in `context`.
    pub fn read(keys: &[&'static str], context: &crate::Context) -> Self {
        Self {
            values: keys.iter().map(|k| (*k, context.get(k))).collect(),
        }
    }

    /// The value of `key`: `None` when no override declared the key, `Some(None)` when the context
    /// does not hold it.
    pub fn get(&self, key: &str) -> Option<&Option<Value>> {
        self.values.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// The values of those of `keys` this holds: the view one model's overrides get (a mask memo
    /// keyed on it does not split on keys other models declared).
    pub fn restricted_to(&self, keys: &[&'static str]) -> Self {
        Self {
            values: self
                .values
                .iter()
                .filter(|(k, _)| keys.contains(k))
                .cloned()
                .collect(),
        }
    }

    /// Whether no key is declared.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// What an override sees (engine note §2.2): an [`AccessCtx`] (groups, kind class, elevation
/// kind, the model) plus the values of the context keys the model's overrides declared. No uid, no
/// company, no I/O.
pub struct FieldAccessCtx<'a> {
    access: AccessCtx<'a>,
    declared: &'a FieldContextValues,
    model_keys: &'a [&'static str],
}

impl<'a> FieldAccessCtx<'a> {
    /// The context of `access`'s model (built by [`FieldAccessTable::narrow`]).
    fn new(
        access: AccessCtx<'a>,
        declared: &'a FieldContextValues,
        model_keys: &'a [&'static str],
    ) -> Self {
        Self {
            access,
            declared,
            model_keys,
        }
    }

    /// The group, class and elevation view an `#[access_domain]` override has as well.
    pub fn access(&self) -> &AccessCtx<'a> {
        &self.access
    }

    /// The model the mask is for.
    pub fn model(&self) -> &'a ModelSpec {
        self.access.model()
    }

    /// The principal's kind class.
    pub fn class(&self) -> KindClass {
        self.access.class()
    }

    /// The elevation kind.
    pub fn elevation(&self) -> ElevationKind {
        self.access.elevation()
    }

    /// Whether the principal's groups contain the group with external id `xmlid`
    /// (`module.name`).
    ///
    /// # Errors
    ///
    /// `Programming` for a malformed external id or one that names no group.
    pub fn has_group(&self, xmlid: &str) -> Result<bool> {
        self.access.has_group(xmlid)
    }

    /// The id of the model's field `name`, for the mask methods.
    ///
    /// # Errors
    ///
    /// `Programming` when the model has no such field (an override naming a field that was
    /// removed or misspelled fails loudly, never skips).
    pub fn field(&self, name: &str) -> Result<FieldId> {
        self.model().field_id(name).ok_or_else(|| {
            Error::programming(format!(
                "a field access override of {} names the field '{name}', which the model does not \
                 have",
                self.model().name
            ))
        })
    }

    /// The value of context key `key`, which the override declared in
    /// `#[field_access(context = [..])]`; `None` when the context does not hold it.
    ///
    /// # Errors
    ///
    /// `Programming` for a key the model's overrides did not declare.
    pub fn context(&self, key: &str) -> Result<Option<&Value>> {
        let declared = self.model_keys.contains(&key);
        match self.declared.get(key) {
            Some(value) if declared => Ok(value.as_ref()),
            _ => Err(Error::programming(format!(
                "a field access override of {} read the context key '{key}', which no \
                 #[field_access(context = [..])] of the model declares",
                self.model().name
            ))),
        }
    }
}

/// The rest of an override chain: what `super()` calls.
#[derive(Clone, Copy)]
pub struct FieldAccessSuper<'s> {
    rest: &'s [&'static FieldAccessImpl],
    /// The result of the last `call`, for the narrowing check of the caller.
    last: &'s RefCell<Option<FieldMask>>,
    /// The mask the override owning this `super` was given: a mask passed to `call` may not be
    /// wider than it (an override only narrows, also before it calls `super`).
    bound: &'s FieldMask,
    /// The override owning this `super`; `None` for the start of the chain.
    owner: Option<&'static FieldAccessImpl>,
    /// The operation of this run: `call` refuses any other.
    run_op: FieldOp,
}

impl FieldAccessSuper<'_> {
    /// Run the overrides below this one (finally the policy's mask) on `mask`.
    ///
    /// # Errors
    ///
    /// The lower override's error, or `Programming` when the mask passed in is wider than the
    /// calling override's own input, a lower override widened the mask it was given, or `op` is
    /// not the operation of the run the override was called for.
    pub fn call(
        &self,
        ctx: &FieldAccessCtx<'_>,
        op: FieldOp,
        mask: FieldMask,
    ) -> Result<FieldMask> {
        if op != self.run_op {
            let who = self.owner.map_or_else(
                || "the field access chain".to_owned(),
                |o| format!("field access override {}.{}", ctx.model().name, o.method),
            );
            return Err(Error::programming(format!(
                "{who} called super with {op:?} in a {:?} run",
                self.run_op
            )));
        }
        if let Some(owner) = self.owner
            && let Some(field) = mask.first_widened(self.bound, ctx.model())
        {
            return Err(Error::programming(format!(
                "field access override {}.{} passed super a mask wider than its input at field \
                 '{}'",
                ctx.model().name,
                owner.method,
                ctx.model().field_by_id(field).name
            )));
        }
        let Some((first, rest)) = self.rest.split_first() else {
            *self.last.borrow_mut() = Some(mask.clone());
            return Ok(mask);
        };
        let below: RefCell<Option<FieldMask>> = RefCell::new(None);
        let input = mask.clone();
        let out = (first.run)(
            ctx,
            op,
            mask,
            FieldAccessSuper {
                rest,
                last: &below,
                bound: &input,
                owner: Some(first),
                run_op: op,
            },
        )?;
        // narrowing is relative to the override's input and to what its own `super` returned
        let baseline = below.into_inner();
        let widened = out.first_widened(&input, ctx.model()).or_else(|| {
            baseline
                .as_ref()
                .and_then(|b| out.first_widened(b, ctx.model()))
        });
        if let Some(field) = widened {
            return Err(Error::programming(format!(
                "field access override {}.{} widened field '{}'",
                ctx.model().name,
                first.method,
                ctx.model().field_by_id(field).name
            )));
        }
        *self.last.borrow_mut() = Some(out.clone());
        Ok(out)
    }
}

#[derive(Debug, Default)]
struct Chain {
    impls: Vec<&'static FieldAccessImpl>,
    keys: Vec<&'static str>,
}

/// The `#[field_access]` chains of a registry.
#[derive(Debug, Default)]
pub struct FieldAccessTable {
    chains: FxHashMap<ModelId, Chain>,
    all_keys: Vec<&'static str>,
}

impl FieldAccessTable {
    /// Build the chains of every model from the linked overrides plus `extra`.
    ///
    /// # Errors
    ///
    /// `Programming` when an override's addon declares no such model, an addon implements two
    /// overrides for one model, or a declared context key is empty.
    pub fn build(registry: &Registry, extra: &[&'static FieldAccessImpl]) -> Result<Self> {
        let installed: FxHashSet<&str> = registry.addons().iter().map(|a| a.name).collect();
        let mut by_owner: FxHashMap<(&str, &str), Vec<&'static FieldAccessImpl>> =
            FxHashMap::default();
        for imp in FIELD_ACCESS_IMPLS
            .iter()
            .chain(extra.iter().copied())
            .filter(|i| installed.contains(i.addon))
        {
            by_owner
                .entry((imp.model, imp.addon))
                .or_default()
                .push(imp);
        }
        let declared: FxHashSet<(&str, &str)> = registry
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
            list.sort_by_key(|i| (i.file, i.line));
            if let Some(first) = list.first()
                && !declared.contains(owner)
            {
                return Err(Error::programming(format!(
                    "#[field_access] at {} has no declaration of model '{}' in addon '{}': add \
                     an #[extend] (or #[model]) for it in that addon",
                    first.location(),
                    first.model,
                    first.addon
                )));
            }
            if let Some(pair) = list.windows(2).find(|w| w[0].method == w[1].method) {
                return Err(Error::programming(format!(
                    "#[field_access] '{}' of model '{}' is implemented twice by addon '{}': {} \
                     and {}",
                    pair[0].method,
                    pair[0].model,
                    pair[0].addon,
                    pair[0].location(),
                    pair[1].location()
                )));
            }
            if list.len() > 1 {
                return Err(Error::programming(format!(
                    "addon '{}' has {} #[field_access] overrides for model '{}' ({} and {}): one \
                     per addon and model, like the reference ERP's single `_has_field_access` per class",
                    list[0].addon,
                    list.len(),
                    list[0].model,
                    list[0].location(),
                    list[1].location()
                )));
            }
        }
        let mut chains = FxHashMap::default();
        for model in registry.models() {
            let mut chain = Chain::default();
            let mut seen: Vec<(&str, &str)> = Vec::new();
            for decl in &model.mro {
                let owner = (decl.name, decl.addon);
                if seen.contains(&owner) {
                    continue;
                }
                seen.push(owner);
                for imp in by_owner.get(&owner).map(Vec::as_slice).unwrap_or_default() {
                    chain.impls.push(*imp);
                    for key in imp.context {
                        if key.is_empty() {
                            return Err(Error::programming(format!(
                                "#[field_access] at {} declares an empty context key",
                                imp.location()
                            )));
                        }
                        if !chain.keys.contains(key) {
                            chain.keys.push(key);
                        }
                    }
                }
            }
            if !chain.impls.is_empty() {
                chains.insert(model.id, chain);
            }
        }
        let mut all_keys: Vec<&'static str> = Vec::new();
        let mut models: Vec<&ModelId> = chains.keys().collect();
        models.sort_unstable();
        for model in models {
            for key in &chains[model].keys {
                if !all_keys.contains(key) {
                    all_keys.push(key);
                }
            }
        }
        Ok(Self { chains, all_keys })
    }

    /// The context keys the overrides of `model` declared.
    pub fn context_keys(&self, model: ModelId) -> &[&'static str] {
        self.chains
            .get(&model)
            .map(|c| c.keys.as_slice())
            .unwrap_or_default()
    }

    /// Every context key any override declared (the engine reads their values into
    /// [`crate::PrincipalBinding::field_ctx`]).
    pub fn all_context_keys(&self) -> &[&'static str] {
        &self.all_keys
    }

    /// Whether `model` has overrides.
    pub fn has_overrides(&self, model: ModelId) -> bool {
        self.chains.contains_key(&model)
    }

    /// Run `access.model()`'s overrides on the policy's mask `base`; a model without overrides
    /// returns `base` itself. `declared` is [`crate::PrincipalBinding::field_ctx`]. The policy calls
    /// this from [`crate::AccessPolicy::field_access`] and memoizes the result (masks of models
    /// without declared keys by group set, class, elevation and debug; the others also by the
    /// declared values).
    ///
    /// # Errors
    ///
    /// An override's error, or `Programming` when one widens the mask or reads an undeclared
    /// context key.
    pub fn narrow(
        &self,
        access: AccessCtx<'_>,
        declared: &FieldContextValues,
        base: Arc<FieldMask>,
    ) -> Result<Arc<FieldMask>> {
        let spec = access.model();
        let Some(chain) = self.chains.get(&spec.id) else {
            return Ok(base);
        };
        let ctx = FieldAccessCtx::new(access, declared, &chain.keys);
        // overrides refine an explicit mask: `all()` has no field count to refine
        let start = if base.is_all() {
            FieldMask::full(spec.fields.len())
        } else {
            (*base).clone()
        };
        let run = |op: FieldOp| -> Result<FieldMask> {
            let last = RefCell::new(None);
            FieldAccessSuper {
                rest: &chain.impls,
                last: &last,
                bound: &start,
                owner: None,
                run_op: op,
            }
            .call(&ctx, op, start.clone())
        };
        // backstop: whatever the chain returned is clamped to the policy's mask
        let read = run(FieldOp::Read)?.clamped_to(&start, spec);
        let write = run(FieldOp::Write)?.clamped_to(&start, spec);
        Ok(Arc::new(start.combined(&read, &write, spec)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    /// A model sees only the context keys its own overrides declared.
    #[test]
    fn context_values_restricted_to_a_models_keys() {
        let context = crate::Context::default().with("a", Value::Bool(true));
        let all = FieldContextValues::read(&["a", "b"], &context);
        let own = all.restricted_to(&["a"]);
        assert_eq!(own.get("a"), Some(&Some(Value::Bool(true))));
        assert_eq!(own.get("b"), None);
        assert_eq!(all.restricted_to(&[]), FieldContextValues::default());
    }
}
