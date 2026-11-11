//! The host-access bridge (§5.2, §5.3):
//! the only place a [`GuardedArch`] meets the ORM's guard plan and an evaluation of it.
//!
//! [`plan_of`] derives the canonical [`GuardPlan`] a bundle needs decided. [`filter`] is the
//! checked entry point that turns a [`GuardEvaluation`] into a [`FilteredArch`]: it fails closed
//! before any node is visited when the evaluation is for another principal, was decided under
//! another registry generation, or belongs to a different plan than the bundle's own table.
//!
//! [`FilteredArch`] has no constructor: the filtered serializer ([`crate::Arch`]'s crate-private
//! `write_xml_filtered`) is reachable only through it, and the only way to get one is [`filter`].
//! There is no trait to implement and no closure to pass.
//!
//! The module is compiled only with the `host-access` feature (it depends on `erp-orm`); the pure
//! and guest builds of this crate never see it.

use std::collections::{HashMap, HashSet};

use erp_fields::{Error, Result};
use erp_orm::{
    Env, FieldIdx, GroupClause, GroupId, GroupSpec, GuardEvaluation, GuardIdx, GuardPlan,
    GuardPlanBuilder, GuardPlanLimits, QualifiedXmlId, RecordGuardIdx,
};

use crate::arch::{Arch, FilterDecision, NodeId, NodeIdx};
use crate::guard::{ClauseText, FieldRefKind, FieldRefText, GroupRefText, GuardText, GuardedArch};
use crate::validate::SUB_VIEW_TAGS;

/// Derives the canonical plan of `guarded`: its distinct effective guards in the order of first
/// occurrence in the arch (so a guard's index in the bundle is its index in the plan), its record
/// guards and the fields it shows.
///
/// The host calls it once per cache fill and keeps the `Arc`.
///
/// # Errors
///
/// `Validation` for a term that is not a group external id or a bound of the plan that the bundle
/// exceeds; `Programming` if the plan does not keep the table's positions (a defect, never data).
pub fn plan_of(guarded: &GuardedArch) -> Result<GuardPlan> {
    let table = guarded.table();
    let mut builder = GuardPlanBuilder::new(&table.root_model, GuardPlanLimits::default())?;
    for (i, guard) in table.guards.iter().enumerate() {
        let at = builder.node_guard(spec_of(guard)?)?.get();
        if at != i {
            return Err(Error::programming(format!(
                "guard {i} of the bundle of '{}' has position {at} in its plan",
                table.root_model
            )));
        }
    }
    for (i, (view, ids)) in table.record_guards.iter().enumerate() {
        let groups = ids
            .iter()
            .map(|&id| GroupId::new(id))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::validation(format!("record guard of view {view}: {e}")))?;
        let at = builder.record_guard(*view, &groups)?.get();
        if at != i {
            return Err(Error::programming(format!(
                "record guard {i} (view {view}) of the bundle of '{}' has position {at} in its plan",
                table.root_model
            )));
        }
    }
    let mut distinct = 0usize;
    for field in &table.fields {
        let at = builder.field(&field.model, &field.path)?.get();
        let expected = field.plan_index as usize;
        if at != expected {
            return Err(Error::programming(format!(
                "field '{}.{}' of the bundle has position {at} in its plan, expected {expected}",
                field.model, field.path
            )));
        }
        distinct = distinct.max(at + 1);
    }
    let plan = builder.finish()?;
    if plan.fields().len() != distinct {
        return Err(Error::programming(format!(
            "the plan of '{}' has {} fields, the bundle shows {distinct}",
            table.root_model,
            plan.fields().len()
        )));
    }
    Ok(plan)
}

fn xml_id(r: &GroupRefText) -> Result<QualifiedXmlId> {
    QualifiedXmlId::parse(&r.0).map_err(|e| Error::validation(format!("group reference: {e}")))
}

fn spec_of(guard: &GuardText) -> Result<GroupSpec> {
    let mut clauses = Vec::with_capacity(guard.clauses.len());
    for ClauseText {
        positives,
        negatives,
    } in guard.clauses.iter()
    {
        clauses.push(GroupClause::new(
            positives.iter().map(xml_id).collect::<Result<Vec<_>>>()?,
            negatives.iter().map(xml_id).collect::<Result<Vec<_>>>()?,
        ));
    }
    Ok(GroupSpec::from_clauses(clauses, guard.never))
}

/// What the decision step reads from an evaluation. [`GuardEvaluation`] is the only production
/// implementation; the trait is crate-private, so no other crate can supply verdicts (code of this
/// crate, its own tests included, can implement it).
pub(crate) trait Verdicts {
    fn plan(&self) -> &GuardPlan;
    fn registry_generation(&self) -> u64;
    fn node_allowed(&self, guard: usize) -> Result<bool>;
    fn record_allowed(&self, guard: usize) -> Result<bool>;
    fn field_readable(&self, field: usize) -> Result<bool>;
}

impl Verdicts for GuardEvaluation<'_> {
    fn plan(&self) -> &GuardPlan {
        GuardEvaluation::plan(self)
    }
    fn registry_generation(&self) -> u64 {
        GuardEvaluation::registry_generation(self)
    }
    fn node_allowed(&self, guard: usize) -> Result<bool> {
        GuardEvaluation::node_allowed(self, GuardIdx::new(guard)?)
    }
    fn record_allowed(&self, guard: usize) -> Result<bool> {
        GuardEvaluation::record_allowed(self, RecordGuardIdx::new(guard)?)
    }
    fn field_readable(&self, field: usize) -> Result<bool> {
        GuardEvaluation::field_readable(self, FieldIdx::new(field)?)
    }
}

/// The checked entry point. Fails closed before any node is visited, in this order: the evaluation
/// is for `env`'s principal; the bundle and the evaluation share a registry generation; the
/// evaluation's plan equals the bundle's own table in content; every record guard is satisfied.
/// Then one decision per node.
///
/// # Errors
///
/// `Access` when the evaluation was decided for another principal, when its plan is not the
/// bundle's (naming the first differing position), or when a record guard is not satisfied (naming
/// the view id). `Validation` when the bundle was captured under another registry generation.
/// `Programming` for a verdict position outside the plan (a defect).
pub fn filter<'a>(
    env: &Env,
    guarded: &'a GuardedArch,
    evaluation: &GuardEvaluation<'_>,
) -> Result<FilteredArch<'a>> {
    if !evaluation.is_for(env) {
        return Err(Error::access(
            "guard evaluation was minted for another principal (or the environment's \
             binding cannot be built in this transaction)",
        ));
    }
    decide(guarded, evaluation)
}

/// Builds the pure H18 callback input through the same checked boundary as [`filter`].
///
/// The returned input borrows the evaluation as well as the environment and bundle, so a safe
/// caller cannot retain it after the request authority has ended. Metadata comes from this exact
/// environment registry; it is descriptive and every output field still requires host access checks.
///
/// # Errors
/// The errors of [`filter`], `Validation` for an environment registry-generation mismatch, or
/// `Programming` if the checked root model is missing from that registry.
pub fn checked_view_input<'a>(
    env: &'a Env,
    guarded: &'a GuardedArch,
    evaluation: &'a GuardEvaluation<'_>,
) -> Result<crate::CheckedViewInput<'a>> {
    let filtered = filter(env, guarded, evaluation)?;
    if env.registry().generation() != guarded.registry_generation() {
        return Err(Error::validation(format!(
            "view bundle for registry generation {} used with environment generation {}",
            guarded.registry_generation(),
            env.registry().generation()
        )));
    }
    let model = env
        .registry()
        .model(&guarded.table().root_model)
        .ok_or_else(|| {
            Error::programming(format!(
                "checked root model '{}' is missing from the environment registry",
                guarded.table().root_model
            ))
        })?;
    Ok(filtered.into_checked_input(model))
}

/// Checked H18 traversal with complete panel-section and category-dependency pruning.
///
/// Raw panel structure is validated before policy pruning. Denied sections and dependent
/// filters are removed in full before the callback can traverse them; all ordinary view and
/// explicit XML group guards remain enforced. Final emitted references still require the
/// host's independent schema, semantic and access validation. Never substitutes for that pass.
///
/// # Errors
/// The principal/generation/plan/record errors of [`checked_view_input`], invalid raw panel
/// structure, or exhausted shared admission. No denied node becomes available to callbacks.
pub fn checked_panel_view_input<'a>(
    env: &'a Env,
    guarded: &'a GuardedArch,
    evaluation: &'a GuardEvaluation<'_>,
    budget: &crate::DescriptionBudget,
) -> Result<crate::CheckedViewInput<'a>> {
    if !evaluation.is_for(env) {
        return Err(Error::access(
            "guard evaluation was minted for another principal (or the environment's binding cannot be built in this transaction)",
        ));
    }
    verify_binding(guarded, evaluation)?;
    panel_admission::validate(guarded, env.registry(), budget)?;
    let filtered = decide_panel(guarded, evaluation)?;
    if env.registry().generation() != guarded.registry_generation() {
        return Err(Error::validation(
            "Panel input registry generation mismatch.",
        ));
    }
    let model = env
        .registry()
        .model(&guarded.table().root_model)
        .ok_or_else(|| Error::programming("Panel input root model is missing."))?;
    Ok(filtered.into_checked_input(model))
}
/// Fixed located unsupported-panel syntax, when this error was minted by panel admission.
///
/// Companion list producers may show this notice without applying panel restrictions. Other
/// validation/access/resource errors return `None` and must retain their failure behavior.
pub fn panel_syntax_notice(error: &Error) -> Option<String> {
    panel_admission::syntax_notice(error)
}
#[path = "panel_admission.rs"]
mod panel_admission;

/// Steps 2 to 5 of the comparison, on verdicts already known to be for the right principal.
pub(crate) fn decide<'a>(
    guarded: &'a GuardedArch,
    verdicts: &dyn Verdicts,
) -> Result<FilteredArch<'a>> {
    decide_inner(guarded, verdicts, false)
}

pub(crate) fn decide_panel<'a>(
    guarded: &'a GuardedArch,
    verdicts: &dyn Verdicts,
) -> Result<FilteredArch<'a>> {
    decide_inner(guarded, verdicts, true)
}
fn decide_inner<'a>(
    guarded: &'a GuardedArch,
    verdicts: &dyn Verdicts,
    panels: bool,
) -> Result<FilteredArch<'a>> {
    verify_binding(guarded, verdicts)?;
    let table = guarded.table();
    let arch = guarded.arch();
    let mut decisions = vec![FilterDecision::Allow; arch.node_count()].into_boxed_slice();
    for entry in &*table.node_guards {
        let node = entry.node.index();
        let allowed = verdicts.node_allowed(entry.guard as usize)?;
        let slot = decisions.get_mut(node).ok_or_else(|| {
            Error::programming(format!("node {node} of a guard is outside the arch"))
        })?;
        *slot = if !allowed {
            FilterDecision::Deny
        } else if arch.tag_of(entry.node) == "t" && arch.attrs_of(entry.node).is_empty() {
            FilterDecision::Unwrap
        } else {
            FilterDecision::Allow
        };
    }
    // The first unreadable field each node's expression attributes read, by node.
    let mut unreadable_reads: HashMap<usize, &FieldRefText> = HashMap::new();
    // The relational fields whose comodel the user may not read.
    let mut unreadable_comodels: HashSet<usize> = HashSet::new();
    // The nodes whose `domain` reads an unreadable field.
    let mut unreadable_domains: HashSet<usize> = HashSet::new();
    for field in &*table.fields {
        if verdicts.field_readable(field.plan_index as usize)? {
            continue;
        }
        let node = field.node.index();
        if panels && panel_section(arch, field.node) {
            let slot = decisions
                .get_mut(node)
                .ok_or_else(|| Error::programming("panel field outside arch"))?;
            *slot = FilterDecision::Deny;
            continue;
        }
        match field.kind {
            FieldRefKind::Node => {
                let slot = decisions.get_mut(node).ok_or_else(|| {
                    Error::programming(format!("field node {node} is outside the arch"))
                })?;
                *slot = FilterDecision::Deny;
            }
            FieldRefKind::PanelDependency => {
                unreadable_reads.entry(node).or_insert(field);
                unreadable_domains.insert(node);
            }
            FieldRefKind::Expr { ref attribute } => {
                unreadable_reads.entry(node).or_insert(field);
                if &**attribute == "domain" {
                    unreadable_domains.insert(node);
                }
            }
            FieldRefKind::Comodel => {
                unreadable_comodels.insert(node);
            }
        }
    }
    if panels {
        prune_panel_dependencies(guarded, &mut decisions)?;
    }
    if !unreadable_comodels.is_empty() {
        deny_unreadable_comodel_readers(
            guarded,
            &mut decisions,
            &mut unreadable_reads,
            &unreadable_comodels,
            &unreadable_domains,
        );
    }
    if !unreadable_reads.is_empty() {
        refuse_surviving_readers(guarded, &decisions, &unreadable_reads)?;
    }
    Ok(FilteredArch { guarded, decisions })
}

/// Another model the user may not read is not an error (review M1): inside the
/// sub-view of a relational field whose comodel is unreadable, a node that reads an unreadable
/// field is removed with its subtree, as is a relational field whose own `domain` reads one. The
/// readers that remain are judged by [`refuse_surviving_readers`].
pub(crate) fn deny_unreadable_comodel_readers(
    guarded: &GuardedArch,
    decisions: &mut [FilterDecision],
    unreadable_reads: &mut HashMap<usize, &FieldRefText>,
    unreadable_comodels: &HashSet<usize>,
    unreadable_domains: &HashSet<usize>,
) {
    let arch = guarded.arch();
    // the nodes of the walk above the current one, and the depth of the unreadable sub-view
    // container the walk is inside, if any
    let mut path: Vec<usize> = Vec::new();
    let mut inside: Option<usize> = None;
    for (id, depth) in arch.walk(arch.root_id()) {
        if arch.is_sentinel(id) {
            continue;
        }
        path.truncate(depth);
        if inside.is_some_and(|d| depth <= d) {
            inside = None;
        }
        let node = id.to_idx().index();
        if inside.is_none()
            && SUB_VIEW_TAGS.contains(&arch.tag(id))
            && path.last().is_some_and(|p| unreadable_comodels.contains(p))
        {
            inside = Some(depth);
        }
        path.push(node);
        let own_domain = unreadable_comodels.contains(&node) && unreadable_domains.contains(&node);
        // look the slot up first: a missing slot leaves the reader in place, so
        // `refuse_surviving_readers` still fails the request instead of serving the read
        if (inside.is_some() || own_domain)
            && let Some(slot) = decisions.get_mut(node)
            && unreadable_reads.remove(&node).is_some()
        {
            *slot = FilterDecision::Deny;
        }
    }
}

/// A node that survives the filter may not read a field the user cannot read in an expression
/// attribute: dropping only the expression would change what the client shows, so the whole
/// request fails (5, "never drop only the expression"). A denied node, or one below a
/// denied node, is removed and raises nothing. The first reader in document order is named.
fn refuse_surviving_readers(
    guarded: &GuardedArch,
    decisions: &[FilterDecision],
    unreadable_reads: &HashMap<usize, &FieldRefText>,
) -> Result<()> {
    let arch = guarded.arch();
    let mut denied_at: Option<usize> = None;
    for (id, depth) in arch.walk(arch.root_id()) {
        if arch.is_sentinel(id) {
            continue;
        }
        if let Some(d) = denied_at {
            if depth > d {
                continue;
            }
            denied_at = None;
        }
        let node = id.to_idx();
        if decisions.get(node.index()).copied() == Some(FilterDecision::Deny) {
            denied_at = Some(depth);
            continue;
        }
        if let Some(read) = unreadable_reads.get(&node.index()) {
            let attribute = match &read.kind {
                FieldRefKind::PanelDependency => "domain",
                FieldRefKind::Expr { attribute } => &**attribute,
                FieldRefKind::Node | FieldRefKind::Comodel => {
                    return Err(Error::programming(
                        "a field reference that is not an expression was recorded as an expression read",
                    ));
                }
            };
            let origin = arch.origin(id);
            return Err(Error::access(format!(
                "view {}, line {}: the {attribute} attribute reads the field '{}' of model '{}', \
                 which the user may not read",
                origin.view_id(),
                origin.line(),
                read.path,
                read.model
            )));
        }
    }
    Ok(())
}

/// Compares `plan` with the bundle's table position by position, without allocating. The first
/// difference is named.
fn check_plan(guarded: &GuardedArch, plan: &GuardPlan) -> Result<()> {
    let table = guarded.table();
    let differs = |what: String| {
        Error::access(format!(
            "guard evaluation does not belong to this view bundle ({what} differs)"
        ))
    };
    if plan.root_model() != &*table.root_model {
        return Err(differs(format!(
            "root model: '{}' against '{}'",
            plan.root_model(),
            table.root_model
        )));
    }

    if plan.node_guards().len() != table.guards.len() {
        return Err(differs(format!(
            "guard count: {} against {}",
            plan.node_guards().len(),
            table.guards.len()
        )));
    }
    for (i, (spec, guard)) in plan.node_guards().iter().zip(&*table.guards).enumerate() {
        if !spec_equals(spec, guard) {
            return Err(differs(format!("guard {i}")));
        }
    }

    if plan.record_guards().len() != table.record_guards.len() {
        return Err(differs(format!(
            "record guard count: {} against {}",
            plan.record_guards().len(),
            table.record_guards.len()
        )));
    }
    for (i, (rg, (view, ids))) in plan
        .record_guards()
        .iter()
        .zip(&*table.record_guards)
        .enumerate()
    {
        let same = rg.view_id() == *view
            && rg.groups().len() == ids.len()
            && rg.groups().iter().zip(&**ids).all(|(g, &id)| g.get() == id);
        if !same {
            return Err(differs(format!("record guard {i} (view {view})")));
        }
    }

    let mut distinct = 0usize;
    for (i, field) in table.fields.iter().enumerate() {
        let at = field.plan_index as usize;
        let same = plan
            .fields()
            .get(at)
            .is_some_and(|f| f.model() == &*field.model && f.path() == &*field.path);
        if !same {
            return Err(differs(format!(
                "field {i} ('{}.{}')",
                field.model, field.path
            )));
        }
        distinct = distinct.max(at + 1);
    }
    if plan.fields().len() != distinct {
        return Err(differs(format!(
            "field count: {} against {distinct}",
            plan.fields().len()
        )));
    }
    Ok(())
}

fn spec_equals(spec: &GroupSpec, guard: &GuardText) -> bool {
    if spec.is_never() != guard.never || spec.clauses().len() != guard.clauses.len() {
        return false;
    }
    spec.clauses()
        .iter()
        .zip(&*guard.clauses)
        .all(|(clause, text)| {
            refs_equal(clause.positives(), &text.positives)
                && refs_equal(clause.negatives(), &text.negatives)
        })
}

fn refs_equal(ids: &[QualifiedXmlId], texts: &[GroupRefText]) -> bool {
    ids.len() == texts.len() && ids.iter().zip(texts).all(|(id, t)| id.as_str() == &*t.0)
}

/// A traversal handle over the allowed part of one [`GuardedArch`] for one evaluation.
///
/// Private constructor; no `new`, no `from_decisions`, no trait to implement: the only producer is
/// [`filter`]. Its serializer is the arch's filtered writer with the decisions `filter` made.
#[derive(Debug)]
pub struct FilteredArch<'a> {
    guarded: &'a GuardedArch,
    decisions: Box<[FilterDecision]>,
}

impl<'a> FilteredArch<'a> {
    pub(crate) fn into_checked_input(
        self,
        model: &'a erp_registry::ModelSpec,
    ) -> crate::CheckedViewInput<'a> {
        crate::CheckedViewInput::from_parts(self.guarded.arch(), self.decisions, model)
    }

    /// The decision of `node`; a node outside the decision table is denied.
    fn decision(&self, node: NodeIdx) -> FilterDecision {
        self.decisions
            .get(node.index())
            .copied()
            .unwrap_or(FilterDecision::Deny)
    }

    /// Appends the allowed part of the arch as XML in one pass; text and tails as the arch's own
    /// serializer writes them (a denied node's tail stays, an unwrapped `<t>` keeps its content).
    pub fn write_xml(&self, out: &mut String) {
        let arch = self.guarded.arch();
        arch.write_xml_filtered(
            arch.root_node_idx(),
            false,
            &|node| self.decision(node),
            out,
        );
    }

    /// The allowed part of the arch as XML.
    #[must_use]
    pub fn to_xml(&self) -> String {
        let arch = self.guarded.arch();
        let mut out = String::with_capacity(arch.to_xml().len());
        self.write_xml(&mut out);
        out
    }

    /// Elements that are written: not denied and not below a denied element. An unwrapped `<t>`
    /// counts (its content is written), as does every allowed element.
    #[must_use]
    pub fn allowed_node_count(&self) -> usize {
        let arch = self.guarded.arch();
        let mut count = 0usize;
        let mut denied_at: Option<usize> = None;
        for (id, depth) in arch.walk(arch.root_id()) {
            if arch.is_sentinel(id) {
                continue;
            }
            if let Some(d) = denied_at {
                if depth > d {
                    continue;
                }
                denied_at = None;
            }
            if self.decision(id.to_idx()) == FilterDecision::Deny {
                denied_at = Some(depth);
                continue;
            }
            count += 1;
        }
        count
    }
}

fn verify_binding(guarded: &GuardedArch, verdicts: &dyn Verdicts) -> Result<()> {
    if guarded.registry_generation() != verdicts.registry_generation() {
        return Err(Error::validation(format!(
            "view bundle for registry generation {} served under generation {}; refresh the \
             'templates' cache",
            guarded.registry_generation(),
            verdicts.registry_generation()
        )));
    }
    check_plan(guarded, verdicts.plan())?;

    let table = guarded.table();
    for (i, (view, _)) in table.record_guards.iter().enumerate() {
        if !verdicts.record_allowed(i)? {
            return Err(Error::access(format!(
                "view {view} is restricted to groups the user is not in"
            )));
        }
    }

    Ok(())
}
fn panel_section(arch: &Arch, node: NodeIdx) -> bool {
    node.index() < arch.node_count()
        && arch
            .parent(NodeId::from_idx(node))
            .is_some_and(|parent| arch.tag(parent) == "searchpanel")
}
fn prune_panel_dependencies(guarded: &GuardedArch, decisions: &mut [FilterDecision]) -> Result<()> {
    let arch = guarded.arch();
    // Fixed point is bounded by the captured node count; each changed pass removes a section.
    for _ in 0..64 {
        let mut changed = false;
        for field in &*guarded.table().fields {
            if field.kind != FieldRefKind::PanelDependency {
                continue;
            }
            if field.node.index() >= arch.node_count() {
                return Err(Error::programming("panel dependency outside arch"));
            }
            let node = NodeId::from_idx(field.node);
            let parent = arch
                .parent(node)
                .ok_or_else(|| Error::programming("panel dependency has no parent"))?;
            let denied = arch.element_children(parent).any(|candidate| {
                arch.attr(candidate, "name") == Some(field.path.as_ref())
                    && arch.attr(candidate, "select") != Some("multi")
                    && (decisions[candidate.to_idx().index()] == FilterDecision::Deny
                        || matches!(
                            arch.attr(candidate, "invisible"),
                            Some("1" | "true" | "True")
                        ))
            });
            if denied && decisions[field.node.index()] != FilterDecision::Deny {
                decisions[field.node.index()] = FilterDecision::Deny;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
    }
    Err(Error::programming(
        "panel dependency pruning did not converge",
    ))
}
