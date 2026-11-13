//! Company scope and the company-check defaults of the composed registry (2, 7.6;
//! ).
//!
//! A pure pass over the models after relations are known: no I/O. In order it
//!
//! 1. finds every model's [`CompanyScope`] and [`CompanyMatch`], then the
//!    [`CompanyDelegation`] of a concrete model without scope whose `company_id` comes by
//!    `_inherits` from a model scoped by it (amendment A2);
//! 2. decides each relational field's [`CheckCompany`] (declaration, opt-out or default);
//! 3. decides each model's `check_company_auto` (declaration, opt-out or default);
//! 4. refuses a declaration that cannot work (a `check_company` on a model without company, or on
//!    a delegating model's own fields; a check toward `company_ids` that come by delegation).

use erp_fields::{Error, FieldType, Result};
use rustc_hash::FxHashMap;

use crate::spec::{
    CheckCompany, CompanyDelegation, CompanyMatch, CompanyScope, FieldId, Inherited, ManyMatch,
    ModelId, ModelSpec, SingleMatch,
};

/// The model every company field links to.
const COMPANY_MODEL: &str = "res.company";

/// The one model scoped by `company_ids` although it also has a `company_id`.
const USERS_MODEL: &str = "res.users";

/// Run the pass (called by the composer after `relations::finish` and `keys::finish`).
///
/// # Errors
///
/// `Programming`, naming the model and the declaring addon, for a `company_match` on a model that is
/// not company-scoped or of the wrong form, for `parent_of`/`parent_of_any` without a parent path on
/// `res.company`, and for `check_company`/`check_company_auto` declared on a model without company;
/// for a `check_company` toward a model whose `company_ids` come by
/// delegation, and for `check_company`/`check_company_auto` declared on a model whose company comes
/// by delegation (A2-D3), each naming the delegation; for a delegation chain longer than
/// the number of models (a broken invariant: `relations::finish` refuses cycles).
pub(crate) fn finish(
    models: &mut [ModelSpec],
    by_name: &FxHashMap<&'static str, ModelId>,
) -> Result<()> {
    let company_has_path = by_name
        .get(COMPANY_MODEL)
        .is_some_and(|id| models[id.index()].parent_path.is_some());
    let scopes = find_scopes(models, company_has_path)?;
    for (model, scope) in models.iter_mut().zip(&scopes) {
        model.company_scope = *scope;
    }
    let view: &[ModelSpec] = models;
    let delegations = find_delegations(view, &scopes)?;
    // the delegation of each model, named for the refusals (A2-D3): a recorded delegation, or
    // `company_ids` that come by delegation (never recorded)
    let delegated_many: Vec<Option<Via>> = view
        .iter()
        .map(|model| delegated_company_ids(view, model))
        .collect();
    let via: Vec<Option<Via>> = view
        .iter()
        .zip(&delegations)
        .zip(&delegated_many)
        .map(|((model, delegation), many)| {
            delegation
                .map(|d| Via::of(view, model, d.delegate, d.field))
                .or(*many)
        })
        .collect();
    for (model, delegation) in models.iter_mut().zip(&delegations) {
        model.company_delegation = *delegation;
    }
    // a link is checked by default only towards a model that is scoped by a company field
    let linkable: Vec<bool> = scopes
        .iter()
        .map(|s| {
            matches!(
                s,
                Some(CompanyScope::Single { .. } | CompanyScope::Many { .. })
            )
        })
        .collect();
    for i in 0..models.len() {
        let scope = scopes[i];
        let abstract_model = models[i].is_abstract();
        let model_name = models[i].name;
        // 2. fields
        let mut any_on = false;
        for fi in 0..models[i].fields.len() {
            let target_linkable = {
                let f = &models[i].fields[fi];
                f.ty.comodel()
                    .and_then(|c| by_name.get(c))
                    .is_some_and(|id| linkable[id.index()])
            };
            let model_scoped = matches!(
                scope,
                Some(CompanyScope::Single { .. } | CompanyScope::Many { .. })
            );
            let f = &mut models[i].fields[fi];
            let state = if f.inherited.is_some() {
                CheckCompany::Off
            } else {
                match f.check_company_declared {
                    Some(true) => CheckCompany::Declared,
                    Some(false) => CheckCompany::Off,
                    None => {
                        let relational = matches!(
                            f.ty,
                            FieldType::Many2one { .. } | FieldType::Many2many { .. }
                        );
                        if model_scoped
                            && relational
                            && f.store
                            && f.compute.is_none()
                            && f.related.is_none()
                            // per-company values are not stored yet: a company-dependent
                            // link is checked only where it says so (review F6)
                            && !f.company_dependent
                            && !f.delegate
                            && !f.magic
                            && target_linkable
                        {
                            CheckCompany::Default
                        } else {
                            CheckCompany::Off
                        }
                    }
                }
            };
            f.check_company = state;
            if state == CheckCompany::Declared {
                let addon = f
                    .modules
                    .last()
                    .map_or_else(String::new, |a| format!(" (declared in addon '{a}')"));
                let Some(target) = f.ty.comodel() else {
                    return Err(Error::programming(format!(
                        "{model_name}.{} declares check_company, but it is not a relational \
                         field{addon}",
                        f.name,
                    )));
                };
                let target_id = by_name.get(target).copied();
                let target_scope = target_id.and_then(|id| scopes[id.index()]);
                match target_scope {
                    Some(CompanyScope::Single { .. } | CompanyScope::Many { .. }) => {}
                    // the target's company comes by delegation: compared through it (A2-D2)
                    None if target_id.is_some_and(|id| delegations[id.index()].is_some()) => {}
                    Some(CompanyScope::Itself) => {
                        return Err(Error::programming(format!(
                            "{model_name}.{} declares check_company, but it links to res.company \
                             itself, which has no company to compare with{addon}",
                            f.name,
                        )));
                    }
                    _ => {
                        if let Some(v) = target_id.and_then(|id| delegated_many[id.index()]) {
                            return Err(Error::programming(format!(
                                "{model_name}.{} declares check_company, but its target {target} \
                                 has company_ids only through the delegation {target}.{} to {}, \
                                 which the company check does not support{addon}",
                                f.name, v.delegate, v.parent,
                            )));
                        }
                        return Err(Error::programming(format!(
                            "{model_name}.{} declares check_company, but its target {target} has \
                             no company_id or company_ids field linked to res.company, so there \
                             is nothing to compare{addon}",
                            f.name,
                        )));
                    }
                }
            }
            // 4. a declared check on a model without company (abstract mixins are composed into
            // their concrete models, which are checked there)
            if state == CheckCompany::Declared && scope.is_none() && !abstract_model {
                let addon = f
                    .modules
                    .last()
                    .map_or_else(String::new, |a| format!(" (declared in addon '{a}')"));
                if let Some(v) = via[i] {
                    // A2-D3: the record side would need the company read through the delegation
                    // inside the batched statement
                    return Err(Error::programming(format!(
                        "{model_name}.{} declares check_company, but {model_name} has its company \
                         only through the delegation {model_name}.{} to {}; checks on a \
                         delegating model's own fields are not supported{addon}",
                        f.name, v.delegate, v.parent,
                    )));
                }
                return Err(Error::programming(format!(
                    "{model_name}.{} declares check_company, but {model_name} has no company_id \
                     or company_ids field linked to res.company{addon}",
                    f.name,
                )));
            }
            any_on |= state.is_on();
        }
        // 3. model default
        let declared = models[i].mro.iter().find_map(|d| d.check_company_auto);
        let auto = match declared {
            Some(true) if scope.is_none() && !abstract_model => {
                let addon = models[i]
                    .mro
                    .iter()
                    .find(|d| d.check_company_auto.is_some())
                    .map_or("?", |d| d.addon);
                if let Some(v) = via[i] {
                    return Err(Error::programming(format!(
                        "{model_name} declares check_company_auto, but has its company only \
                         through the delegation {model_name}.{} to {}; checks on a delegating \
                         model's own fields are not supported (declared in addon '{addon}')",
                        v.delegate, v.parent,
                    )));
                }
                return Err(Error::programming(format!(
                    "{model_name} declares check_company_auto, but has no company_id or \
                     company_ids field linked to res.company (declared in addon '{addon}')"
                )));
            }
            Some(on) => on,
            None => {
                matches!(
                    scope,
                    Some(CompanyScope::Single { .. } | CompanyScope::Many { .. })
                ) && any_on
            }
        };
        models[i].check_company_auto = auto;
    }
    Ok(())
}

/// The delegation a refusal names: the model's delegate field and the parent it delegates to.
#[derive(Clone, Copy)]
struct Via {
    delegate: &'static str,
    parent: &'static str,
}

impl Via {
    /// The delegation of `model` that brings its inherited `field` (`delegate` is the first step).
    fn of(models: &[ModelSpec], model: &ModelSpec, delegate: FieldId, field: FieldId) -> Self {
        let parent = model
            .fields
            .get(field.index())
            .and_then(|f| f.inherited)
            .and_then(|i| models.get(i.parent.index()))
            .map_or("?", |p| p.name);
        let delegate = model.fields.get(delegate.index()).map_or("?", |f| f.name);
        Self { delegate, parent }
    }
}

/// Whether `f` is a link to `res.company` of the given form that is not company-dependent.
fn is_company_link(f: &crate::spec::FieldSpec, many: bool) -> bool {
    let right_type = match &f.ty {
        FieldType::Many2one { comodel, .. } => !many && comodel == COMPANY_MODEL,
        FieldType::Many2many { comodel, .. } => many && comodel == COMPANY_MODEL,
        _ => false,
    };
    right_type && !f.company_dependent
}

/// The delegation of a concrete model without scope (step 1, A2-D1), in model order:
/// its `company_id` is an inherited many2one to `res.company` whose chain of parent fields ends at
/// a model scoped `Single` by that very field. The copy on the child is not stored
/// (`relations::add_inherited_fields`), so only its type is looked at here; the storing model's
/// scope says the rest.
fn find_delegations(
    models: &[ModelSpec],
    scopes: &[Option<CompanyScope>],
) -> Result<Vec<Option<CompanyDelegation>>> {
    let mut out = Vec::with_capacity(models.len());
    for (model, scope) in models.iter().zip(scopes) {
        out.push(delegation_of(models, scopes, model, *scope)?);
    }
    Ok(out)
}

/// The delegation of one model ([`find_delegations`]).
fn delegation_of(
    models: &[ModelSpec],
    scopes: &[Option<CompanyScope>],
    model: &ModelSpec,
    scope: Option<CompanyScope>,
) -> Result<Option<CompanyDelegation>> {
    if scope.is_some() || model.is_abstract() {
        return Ok(None);
    }
    let Some(f) = model.field("company_id") else {
        return Ok(None);
    };
    let Some(first) = f.inherited else {
        return Ok(None);
    };
    if !is_company_link(f, false) {
        return Ok(None);
    }
    let (storing, storing_field) = chain_end(models, model.name, first)?;
    Ok(match scopes.get(storing.index()).copied().flatten() {
        Some(CompanyScope::Single { field, matching }) if field == storing_field => {
            Some(CompanyDelegation {
                field: f.id,
                delegate: first.delegate,
                storing_model: storing,
                matching,
            })
        }
        _ => None,
    })
}

/// The model and field at the end of an `_inherits` chain that starts at `first` (the parent field
/// may itself be inherited, for several levels).
///
/// # Errors
///
/// `Programming` when the chain is longer than the number of models, or names a model or field
/// that does not exist: `relations::finish` refuses delegation cycles, so either is a broken
/// invariant.
fn chain_end(models: &[ModelSpec], model: &str, first: Inherited) -> Result<(ModelId, FieldId)> {
    let mut step = first;
    for _ in 0..=models.len() {
        let parent_field = models
            .get(step.parent.index())
            .and_then(|p| p.fields.get(step.parent_field.index()))
            .ok_or_else(|| {
                Error::programming(format!(
                    "{model}.company_id is inherited from a model or field that does not exist \
                     (registry invariant)"
                ))
            })?;
        match parent_field.inherited {
            Some(next) => step = next,
            None => return Ok((step.parent, step.parent_field)),
        }
    }
    Err(Error::programming(format!(
        "{model}.company_id is inherited through a delegation chain longer than the number of \
         models (registry invariant: delegation cycles are refused)"
    )))
}

/// The delegation of a concrete model whose `company_ids` many2many to `res.company` comes by
/// `_inherits` (A2-D3: refused as a check target and for checks on the model itself).
fn delegated_company_ids(models: &[ModelSpec], model: &ModelSpec) -> Option<Via> {
    if model.is_abstract() {
        return None;
    }
    let f = model.field("company_ids")?;
    let inherited = f.inherited?;
    is_company_link(f, true).then(|| Via::of(models, model, inherited.delegate, f.id))
}

/// The scope of every model (step 1), in model order.
fn find_scopes(models: &[ModelSpec], company_has_path: bool) -> Result<Vec<Option<CompanyScope>>> {
    let mut out = Vec::with_capacity(models.len());
    for model in models {
        let declared = model
            .mro
            .iter()
            .find(|d| d.company_match.is_some())
            .and_then(|d| d.company_match.map(|m| (m, d.addon)));
        let base = base_scope(model);
        let scope = match (base, declared) {
            (Some(CompanyScope::Single { field, .. }), found) => {
                let matching = match found {
                    None => SingleMatch::Exact,
                    Some((m, addon)) => {
                        let Some(single) = m.single() else {
                            return Err(form_error(model.name, m, addon, false));
                        };
                        path_check(model.name, m, addon, company_has_path)?;
                        single
                    }
                };
                Some(CompanyScope::Single { field, matching })
            }
            (Some(CompanyScope::Many { field, .. }), found) => {
                let matching = match found {
                    None => ManyMatch::AnyOf,
                    Some((m, addon)) => {
                        let Some(many) = m.many() else {
                            return Err(form_error(model.name, m, addon, true));
                        };
                        path_check(model.name, m, addon, company_has_path)?;
                        many
                    }
                };
                Some(CompanyScope::Many { field, matching })
            }
            // an abstract mixin's match is composed into its concrete models (review R5)
            (None, Some(_)) if model.is_abstract() => None,
            (_, Some((m, addon))) => {
                // `res.company` itself is the company: a match is meaningless there too
                return Err(Error::programming(format!(
                    "{}: company_match = \"{}\" needs a company_id or company_ids field linked to \
                     res.company (declared in addon '{addon}')",
                    model.name,
                    m.as_str()
                )));
            }
            (other, None) => other,
        };
        out.push(scope);
    }
    Ok(out)
}

/// `Itself`, `Single`, `Many` or none by field shape, before `company_match`.
fn base_scope(model: &ModelSpec) -> Option<CompanyScope> {
    if model.is_abstract() {
        return None;
    }
    if model.name == COMPANY_MODEL {
        return Some(CompanyScope::Itself);
    }
    let links_company = |name: &str, many: bool| -> Option<FieldId> {
        let f = model.field(name)?;
        let right_type = match &f.ty {
            FieldType::Many2one { comodel, .. } => !many && comodel == COMPANY_MODEL,
            FieldType::Many2many { comodel, .. } => many && comodel == COMPANY_MODEL,
            _ => false,
        };
        (right_type && f.store && !f.company_dependent && f.inherited.is_none()).then_some(f.id)
    };
    if model.name == USERS_MODEL
        && let Some(field) = links_company("company_ids", true)
    {
        return Some(CompanyScope::Many {
            field,
            matching: ManyMatch::AnyOf,
        });
    }
    if let Some(field) = links_company("company_id", false) {
        return Some(CompanyScope::Single {
            field,
            matching: SingleMatch::Exact,
        });
    }
    links_company("company_ids", true).map(|field| CompanyScope::Many {
        field,
        matching: ManyMatch::AnyOf,
    })
}

fn form_error(model: &str, m: CompanyMatch, addon: &str, many: bool) -> Error {
    if many {
        Error::programming(format!(
            "{model}: company_match = \"{}\" needs a company_id field; {model} links companies \
             through company_ids (use \"any_of\" or \"parent_of_any\") (declared in addon \
             '{addon}')",
            m.as_str()
        ))
    } else {
        Error::programming(format!(
            "{model}: company_match = \"{}\" needs a company_ids field; {model} links one company \
             through company_id (use \"exact\" or \"parent_of\") (declared in addon '{addon}')",
            m.as_str()
        ))
    }
}

fn path_check(model: &str, m: CompanyMatch, addon: &str, company_has_path: bool) -> Result<()> {
    if m.needs_parent_path() && !company_has_path {
        return Err(Error::programming(format!(
            "{model}: company_match = \"{}\" needs res.company to keep a parent path \
             (_parent_store) (declared in addon '{addon}')",
            m.as_str()
        )));
    }
    Ok(())
}
