//! The compiled view architecture and per-user group filtering (`get_view`;
//! ).
//!
//! Two stages:
//! 1. Shared compilation (cached in `templates` ormcache): fetches the view inheritance tree
//!    read-only, combines it with [`ViewCatalogue`], and captures the guards into a
//!    [`GuardedArch`] (element `groups` attributes intersected with the enclosing guard and the
//!    field definition's groups, then stripped), with the plan the bundle needs decided.
//! 2. Per-user response (on every call): checks the caller's read access on the model, has the
//!    transaction decide the plan ([`Transaction::evaluate_guards`]) and serializes the allowed
//!    part of the bundle in a single pass through [`host_access::filter`]. A denied node leaves
//!    with its subtree and keeps its tail text; an attribute-less `<t>` that was guarded and
//!    passed is unwrapped. No other path filters an arch.

use std::sync::Arc;

use erp_sdk::access::{GuardPlan, Operation};
use erp_sdk::prelude::*;
use erp_sdk::views::host_access;
use erp_sdk::views::{GuardLookup, GuardedArch, ViewCatalogue, ViewError, ViewLimits};
use erp_sdk::{ErrorKind, OrmCacheKey, Registry};

use super::select::{ViewId, ViewNotice, ViewRequest, select_view};
use super::tree::fetch_trees_read_only;
use super::{MODEL, TEMPLATES, view_error};

/// A shared, compiled view bundle: the
/// stripped arch with its captured guards, and the canonical plan of what must be decided for it.
///
/// Cached in the `templates` ormcache across users and transactions. Crate-private: the only way
/// out of it is [`host_access::filter`], so no addon can read the stripped, unfiltered arch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledView {
    guarded: GuardedArch,
    plan: Arc<GuardPlan>,
}

/// The response of [`get_view`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewResponse {
    arch: String,
    id: ViewId,
    model: Arc<str>,
    notices: Vec<ViewNotice>,
}

impl ViewResponse {
    /// Creates a new view response.
    #[must_use]
    pub fn new(arch: String, id: ViewId, model: Arc<str>, notices: Vec<ViewNotice>) -> Self {
        Self {
            arch,
            id,
            model,
            notices,
        }
    }

    /// The filtered XML architecture.
    #[must_use]
    pub fn arch(&self) -> &str {
        &self.arch
    }

    /// Takes the filtered XML architecture.
    #[must_use]
    pub fn into_arch(self) -> String {
        self.arch
    }

    /// The view id.
    #[must_use]
    pub fn id(&self) -> ViewId {
        self.id
    }

    /// The view id (alias of `id`).
    #[must_use]
    pub fn view_id(&self) -> ViewId {
        self.id
    }

    /// The model name.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Why a `*_view_ref` was not used (empty when it was, or when none was given).
    #[must_use]
    pub fn notices(&self) -> &[ViewNotice] {
        &self.notices
    }
}

/// What [`GuardedArch::capture`] needs from the registry of the transaction.
struct RegistryGuardLookup<'r>(&'r Registry);

impl GuardLookup for RegistryGuardLookup<'_> {
    fn registry_generation(&self) -> u64 {
        self.0.generation()
    }

    fn field_groups(&self, model: &str, field: &str) -> Option<&str> {
        self.0.model(model)?.field(field)?.groups
    }

    fn comodel(&self, model: &str, field: &str) -> Option<&str> {
        self.0.model(model)?.field(field)?.ty.comodel()
    }

    fn has_field(&self, model: &str, field: &str) -> bool {
        self.0
            .model(model)
            .is_some_and(|m| m.field(field).is_some())
    }
}

/// The error of a view-engine step of the compilation: a broken engine invariant is a
/// `Programming` error marked as an invariant violation (never the view author's fault, as
/// `view_error` treats it); every other engine error, a resource limit included, describes the
/// authored views and stays `Validation` with the engine's own message.
fn engine_error(error: ViewError) -> Error {
    match error {
        ViewError::InvariantViolation(_) => Error::invariant(error.to_string()),
        other => Error::new(ErrorKind::Validation, other.to_string()),
    }
}

/// Compiles a view bundle and its plan on cache miss.
async fn compile_view(tx: &mut Transaction, view_id: ViewId, model: &str) -> Result<CompiledView> {
    let trees = fetch_trees_read_only(tx, &[view_id.get()]).await?;
    let mut descriptors = Vec::with_capacity(trees.rows.len());
    for r in trees.rows {
        descriptors.push(r.into_descriptor().map_err(view_error)?);
    }
    let catalogue = ViewCatalogue::new(descriptors).map_err(engine_error)?;
    let combined = catalogue.combine(view_id.get()).map_err(engine_error)?;

    // Record guards: the applied views of the primary chain that are limited to groups. An
    // extension view cannot have `group_ids`, and a view without groups is never
    // a restriction (Q2).
    let applied = combined.applied_view_ids();
    let record_guards: Vec<(i64, &[i64])> = trees
        .views
        .iter()
        .filter(|v| !v.group_ids.is_empty() && applied.contains(&v.id))
        .map(|v| (v.id, v.group_ids.as_slice()))
        .collect();

    let guarded = GuardedArch::capture(
        combined,
        model,
        &record_guards,
        &RegistryGuardLookup(tx.registry()),
        &ViewLimits::default(),
    )
    .map_err(engine_error)?;
    let plan = Arc::new(host_access::plan_of(&guarded)?);
    Ok(CompiledView { guarded, plan })
}

/// Get the detailed composition of the requested view, filtered for `env`'s user.
///
/// # Errors
///
/// `Access` when `env` cannot read `req.model`, or when the view is limited to groups the user is
/// not in (Q2); `Missing` or `Validation` for bad
/// view specifications; database errors.
pub async fn get_view(
    tx: &mut Transaction,
    env: &Env,
    req: &ViewRequest<'_>,
) -> Result<ViewResponse> {
    with_checked_view(tx, env, req, |input, view_id, notices| {
        Ok(ViewResponse::new(
            input.to_xml(),
            view_id,
            Arc::from(input.root_model()),
            notices.to_vec(),
        ))
    })
    .await
}

/// Select, combine and check one view, then synchronously consume its allowed typed traversal.
///
/// The callback runs while the transaction's actual guard evaluation is alive. Its return value
/// cannot borrow the input, preventing the checked traversal from escaping the authority binding.
/// XML and H18 consumers share selection, cache, record/node guards and field filtering.
///
/// # Errors
/// The same selection/access/database errors as [`get_view`], plus errors returned by `consume`.
pub async fn with_checked_view<R>(
    tx: &mut Transaction,
    env: &Env,
    req: &ViewRequest<'_>,
    consume: impl for<'a> FnOnce(
        &erp_sdk::views::CheckedViewInput<'a>,
        ViewId,
        &[ViewNotice],
    ) -> Result<R>,
) -> Result<R> {
    with_checked_view_inner(tx, env, req, None, consume).await
}

/// Select and admit an H18 view with whole-section panel pruning before the callback.
///
/// Raw panel syntax/types/dependencies are checked before any access pruning. Known denied
/// sections and dependent filters are then removed together, so callbacks never see denied
/// nodes. Ordinary XML [`with_checked_view`] and [`get_view`] retain their denial behavior.
/// The callback's emitted field references still require independent final host authorization.
///
/// # Errors
/// The errors of [`with_checked_view`], malformed panel structure and shared admission refusal.
pub async fn with_checked_panel_view<R>(
    tx: &mut Transaction,
    env: &Env,
    req: &ViewRequest<'_>,
    budget: &erp_sdk::views::DescriptionBudget,
    consume: impl for<'a> FnOnce(
        &erp_sdk::views::CheckedViewInput<'a>,
        ViewId,
        &[ViewNotice],
    ) -> Result<R>,
) -> Result<R> {
    with_checked_view_inner(tx, env, req, Some(budget), consume).await
}
async fn with_checked_view_inner<R>(
    tx: &mut Transaction,
    env: &Env,
    req: &ViewRequest<'_>,
    panel_budget: Option<&erp_sdk::views::DescriptionBudget>,
    consume: impl for<'a> FnOnce(
        &erp_sdk::views::CheckedViewInput<'a>,
        ViewId,
        &[ViewNotice],
    ) -> Result<R>,
) -> Result<R> {
    let records = env.records(req.model())?;
    records.check_access(tx, Operation::Read).await?;

    // 2. select_view, then templates cache keyed (view id, view type)
    let selected = select_view(tx, req).await?;
    let view_id = selected.id();
    let model: Arc<str> = Arc::from(req.model());
    let view_type_str = req.view_type().as_str().to_owned();

    let cache_key = OrmCacheKey::new(
        MODEL,
        "get_view",
        vec![Value::Int(view_id.get()), Value::str(view_type_str)],
    );

    let model_clone = Arc::clone(&model);
    let compiled: Arc<CompiledView> = tx
        .ormcache_arc(TEMPLATES, cache_key, async move |tx: &mut Transaction| {
            compile_view(tx, view_id, &model_clone).await
        })
        .await?;

    // 3. The transaction decides the plan; the bridge compares it with the bundle and filters
    let evaluation = tx.evaluate_guards(env, &compiled.plan).await?;
    let input = match panel_budget {
        Some(budget) => {
            host_access::checked_panel_view_input(env, &compiled.guarded, &evaluation, budget)?
        }
        None => host_access::checked_view_input(env, &compiled.guarded, &evaluation)?,
    };
    consume(&input, view_id, selected.notices())
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_sdk::views::LimitKind;

    /// The engine's invariant breach is a `Programming` invariant error; a limit and any other
    /// engine error stay `Validation` (review S5).
    #[test]
    fn engine_errors_keep_their_kind() {
        let e = engine_error(ViewError::InvariantViolation("broken"));
        assert_eq!(e.kind(), ErrorKind::Programming);
        assert!(e.is_invariant_violation());
        let e = engine_error(ViewError::LimitExceeded {
            limit: LimitKind::CheckGroupRefs,
            max: 1,
            actual: Some(2),
        });
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert!(!e.is_invariant_violation());
        let e = engine_error(ViewError::EmptyArch);
        assert_eq!(e.kind(), ErrorKind::Validation);
    }
}
