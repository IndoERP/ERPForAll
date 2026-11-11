//! Field access checks on the fields a query names: the fields of a domain, of an
//! order and of a group-by.

use std::sync::Arc;

use erp_domain::{Condition, Domain, DomainKind, Operator, Value};
use erp_fields::{Error, Result};
use erp_registry::{FieldId, ModelId, ModelSpec};
use erp_sql::reference::ReferenceQuery;
use rustc_hash::FxHashMap;

use crate::env::Env;
use crate::field_access::{FieldMask, FieldOp};
use crate::transaction::Transaction;

/// Framework-owned native admission bridge for base's lifecycle fences.
///
/// The caller must first prepare model/record access through `Records::check_access`.
/// This performs the same authoritative scalar and command-field mask checks as the base
/// writer, without SQL mutations, so denied input cannot poison an earlier valid write by
/// acquiring a fence first. It grants no access and does not replace the writer's checks.
/// This is a reserved bridge, not a supported addon extension point. It is reserved by
/// convention only: any crate that enables `erp-sdk`'s `__base-core-ddl` feature can name it.
/// Calling it is harmless, because it only refuses and never authorizes; without a prepared
/// binding it returns an error rather than allowing.
///
/// # Errors
/// Existing record-binding, policy freshness and field-authorization errors.
#[doc(hidden)]
pub fn base_check_write_fields(
    tx: &Transaction,
    records: &crate::Records,
    vals: &crate::RawVals,
) -> Result<()> {
    tx.check_records(records)?;
    tx.check_field_names(
        records.env(),
        records.model_id(),
        vals.names(),
        FieldOp::Write,
    )
}

/// The one place
/// that decides it: every field check below asks this, no ORM call site tests `su()` for fields.
#[inline]
fn bypasses_field_groups(env: &Env) -> bool {
    env.principal().elevation().is_full()
}

impl Transaction {
    /// Superuser mode (`Full` elevation) is allowed everything.
    ///
    /// ```no_run
    /// # async fn example(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let env = tx.env();
    /// let may_read = tx.has_field_access(&env, "res.partner", "vat", erp_orm::FieldOp::Read).await?;
    /// # let _ = may_read; Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// The binding's and the policy's errors; never a refusal (that is `Ok(false)`).
    pub async fn has_field_access(
        &mut self,
        env: &Env,
        model: &str,
        field: &str,
        op: FieldOp,
    ) -> Result<bool> {
        let Some(spec) = self.shared.registry.model(model) else {
            return Ok(false);
        };
        let Some(field) = spec.field(field) else {
            return Ok(false);
        };
        let (model_id, field_id) = (spec.id, field.id);
        if bypasses_field_groups(env) {
            return Ok(true);
        }
        self.prepare_access(env).await?;
        Ok(self.field_mask(env, model_id)?.allows(field_id, op))
    }

    /// The fields of `model` that `env` may read and write (engine note §2.2): the pinned policy's
    /// mask, which already includes the model's `#[field_access]` overrides. Superuser mode (`Full`
    /// elevation) is decided here and nowhere else: it bypasses field groups, so the mask is
    /// [`FieldMask::all`] and the policy is not asked. No statements.
    ///
    /// # Errors
    ///
    /// The binding's and the policy's errors.
    pub(crate) fn field_mask(&self, env: &Env, model: ModelId) -> Result<Arc<FieldMask>> {
        if bypasses_field_groups(env) {
            return Ok(FieldMask::shared_all());
        }
        let spec: &ModelSpec = self.shared.registry.model_by_id(model);
        let binding = self.binding(env)?;
        let mask = self.policy().field_access(&binding, spec)?;
        let fields = self.shared.graph.generated_read_fields(model);
        if fields.is_empty() {
            return Ok(mask);
        }
        let mut narrowed = None;
        let mut masks = FxHashMap::default();
        masks.insert(model, Arc::clone(&mask));
        for field_id in fields {
            let field = spec.field_by_id(*field_id);
            let read = self
                .shared
                .graph
                .generated_read((model, field.id))
                .ok_or_else(|| Error::programming("Generated access path was not prepared"))?;
            let mut readable = true;
            for (owner, source) in &read.fields {
                if let std::collections::hash_map::Entry::Vacant(entry) = masks.entry(*owner) {
                    let owner_spec = self.shared.registry.model_by_id(*owner);
                    entry.insert(self.policy().field_access(&binding, owner_spec)?);
                }
                let owner_mask = masks
                    .get(owner)
                    .ok_or_else(|| Error::programming("Generated access mask was not prepared"))?;
                if !owner_mask.readable(*source) {
                    readable = false;
                    break;
                }
            }
            if !readable {
                let out = narrowed.get_or_insert_with(|| {
                    mask.clamped_to(&FieldMask::full(spec.fields.len()), spec)
                });
                out.deny(field.id, FieldOp::Read)?;
            }
        }
        Ok(narrowed.map_or(mask, Arc::new))
    }

    /// Fail with the field access error unless `env` may `op` `field`
    /// of `model`. No statements.
    pub(crate) fn check_field(
        &self,
        env: &Env,
        model: ModelId,
        field: FieldId,
        op: FieldOp,
    ) -> Result<()> {
        if bypasses_field_groups(env) {
            return Ok(());
        }
        let mask = self.field_mask(env, model)?;
        if mask.allows(field, op) {
            return Ok(());
        }
        Err(self.field_denied(env, model, field, op))
    }

    /// A local copied-value SQL expression does not authorize its owning records.
    /// Until guarded joins implement that authorization, refuse the alias before execution.
    /// Point reads retain their existing per-target checks; explicit full sudo retains its bypass.
    pub(crate) fn check_query_field(
        &self,
        env: &Env,
        model: ModelId,
        field: FieldId,
    ) -> Result<()> {
        self.check_field(env, model, field, FieldOp::Read)?;
        if !bypasses_field_groups(env)
            && self
                .shared
                .graph
                .generated_read((model, field))
                .is_some_and(|read| read.path.len() > 1)
        {
            let model = self.registry().model_by_id(model);
            return Err(Error::access(format!(
                "Querying generated alias '{}.{}' requires target-record authorization (a later release)",
                model.name,
                model.field_by_id(field).name
            )));
        }
        Ok(())
    }

    /// [`Transaction::check_field`] for several fields of one model: the mask is computed once and
    /// the first denied field is reported (in the order given).
    pub(crate) fn check_fields(
        &self,
        env: &Env,
        model: ModelId,
        fields: impl IntoIterator<Item = FieldId>,
        op: FieldOp,
    ) -> Result<()> {
        if bypasses_field_groups(env) {
            return Ok(());
        }
        let mask = self.field_mask(env, model)?;
        if mask.is_all() {
            return Ok(());
        }
        match fields.into_iter().find(|f| !mask.allows(*f, op)) {
            None => Ok(()),
            Some(field) => Err(self.field_denied(env, model, field, op)),
        }
    }

    /// [`Transaction::check_fields`] for field names. A name the model does not have is skipped:
    /// the operation reports it itself, as an invalid field, once the access checks passed.
    pub(crate) fn check_field_names<'n>(
        &self,
        env: &Env,
        model: ModelId,
        names: impl IntoIterator<Item = &'n str>,
        op: FieldOp,
    ) -> Result<()> {
        let spec = self.shared.registry.model_by_id(model);
        self.check_fields(
            env,
            model,
            names.into_iter().filter_map(|n| spec.field_id(n)),
            op,
        )
    }

    /// The error of a denied field operation, with the policy's debug detail when it has one.
    fn field_denied(&self, env: &Env, model: ModelId, field: FieldId, op: FieldOp) -> Error {
        let spec: &ModelSpec = self.shared.registry.model_by_id(model);
        let fs = spec.field_by_id(field);
        tracing::info!(
            target: "erp::orm",
            uid = env.uid(),
            model = spec.name,
            field = fs.name,
            operation = op.as_str(),
            "Access Denied by field groups"
        );
        let detail = if env.principal().debug() {
            let asked = self
                .binding(env)
                .and_then(|b| self.policy().field_denial_detail(&b, spec, fs, op));
            match asked {
                Ok(detail) => detail,
                Err(e) => return e,
            }
        } else {
            None
        };
        crate::field_access::field_denied(
            spec,
            fs.name,
            op,
            detail.as_ref().map(|d| (env.uid(), d)),
        )
    }

    /// `Read` access to every field `domain` names: directly, through dotted paths (each step on
    /// its own model), inside `any` / `not any` sub-domains (on the comodel) and for `_inherits`
    /// children (the inherited field carries the parent field's groups).
    /// No statements.
    ///
    /// Custom nodes (built by the ORM itself) and unknown field names are skipped here: the
    /// latter are refused by the optimizer right after, with its own error.
    pub(crate) fn check_domain_fields(
        &self,
        env: &Env,
        model: &ModelSpec,
        domain: &Domain,
    ) -> Result<()> {
        if bypasses_field_groups(env) {
            return Ok(());
        }
        self.walk_domain(env, model, domain, false)
    }

    /// Preserve generated alias identity in record-rule predicates before optimization expands
    /// inherited/related fields. Ordinary policy fields retain their privileged field semantics.
    pub(crate) fn check_generated_query_domain(
        &self,
        env: &Env,
        model: &ModelSpec,
        domain: &Domain,
    ) -> Result<()> {
        if bypasses_field_groups(env) {
            return Ok(());
        }
        self.walk_domain(env, model, domain, true)
    }

    fn walk_domain(
        &self,
        env: &Env,
        model: &ModelSpec,
        domain: &Domain,
        generated_only: bool,
    ) -> Result<()> {
        match domain.kind() {
            DomainKind::True | DomainKind::False | DomainKind::Custom(_) => Ok(()),
            DomainKind::Not(child) => self.walk_domain(env, model, child, generated_only),
            DomainKind::And(children) | DomainKind::Or(children) => children
                .iter()
                .try_for_each(|child| self.walk_domain(env, model, child, generated_only)),
            DomainKind::Condition(condition) => {
                self.walk_condition(env, model, condition, generated_only)
            }
        }
    }

    fn walk_condition(
        &self,
        env: &Env,
        model: &ModelSpec,
        condition: &Condition,
        generated_only: bool,
    ) -> Result<()> {
        let registry = env.registry();
        let mut current = model;
        let mut segments = condition.field_expr().split('.').peekable();
        while let Some(name) = segments.next() {
            let Some(field) = current.field(name) else {
                return Ok(());
            };
            let last = segments.peek().is_none();
            let bypass = last && condition.operator().is_internal();
            if (!bypass || generated_only)
                && (!generated_only
                    || self
                        .shared
                        .graph
                        .generated_read((current.id, field.id))
                        .is_some())
            {
                self.check_query_field(env, current.id, field.id)?;
            }
            let comodel = field.ty.comodel().and_then(|name| registry.model(name));
            if last {
                // a reference query's predicate is on its target model (Amendment 1):
                // checked like a comodel sub-domain of a bypass traversal
                if let Some(query) = ReferenceQuery::of(condition.value())
                    && generated_only
                    && condition.operator().is_any()
                {
                    let target = crate::search_method::reference_target(registry, query)?;
                    return self.walk_domain(env, target, query.sub(), generated_only);
                }
                if let (Value::Domain(sub), Some(co)) = (condition.value(), comodel)
                    && (matches!(condition.operator(), Operator::Any | Operator::NotAny)
                        || (generated_only && condition.operator().is_any()))
                {
                    return self.walk_domain(env, co, sub, generated_only);
                }
                return Ok(());
            }
            match comodel {
                Some(co) => current = co,
                // a property or date part after a plain field: nothing more to check
                None => return Ok(()),
            }
        }
        Ok(())
    }

    /// `Read` access to the `(model, field)` pairs a compiled order or a group-by reads (the
    /// comodels' order fields of a many2one order term included).
    pub(crate) fn check_read_pairs<'f>(
        &self,
        env: &Env,
        pairs: impl IntoIterator<Item = &'f (ModelId, erp_registry::FieldId)>,
    ) -> Result<()> {
        for (model, field) in pairs {
            self.check_query_field(env, *model, *field)?;
        }
        Ok(())
    }
}
