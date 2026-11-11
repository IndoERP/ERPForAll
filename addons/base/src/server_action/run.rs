//! The run loop.

use erp_sdk::audit::SuperuserAction;
use erp_sdk::prelude::*;
use erp_sdk::{BoxFuture, Env, FieldType, RealId, Records, Transaction};

use super::registry::{RunnerCtx, RunnerDecl, runner_for, states};
use super::runners::resolve_path;
use super::tree::{EvaluationType, ServerAction, Tree};
use super::{ActionResult, RunRequest, ServerActionError, access};

/// What every action of one run shares: the tree, the runner of each action (checked before the
/// run starts) and the active ids. The environment each node runs with is passed down the run
/// (the caller's, or superuser mode below a `run_as_superuser` action).
pub(crate) struct Frame<'a> {
    tree: Tree,
    /// The runner of each node, in node order.
    runners: Vec<&'static RunnerDecl>,
    active_ids: &'a [RealId],
    /// The action the run started from (recorded with every superuser observation).
    root_action: i64,
}

impl Frame<'_> {
    pub(crate) fn action(&self, node: usize) -> &ServerAction {
        &self.tree.nodes[node].action
    }

    pub(crate) fn children(&self, node: usize) -> &[usize] {
        &self.tree.nodes[node].children
    }
}

/// Run the server action of `req` as the user of `env` (module docs).
///
/// The order: the action is read with its whole tree of children; the caller's right to run the
/// action is checked; then every action of the tree is checked for what refuses it whoever runs
/// it (warnings, a stored value outside its selection, a `run_as_superuser` action without
/// groups, a type without a runner), so such a refusal comes before anything is written; then the
/// action runs, and each child of a `multi` gets its own access check, as the caller, right
/// before it runs (an earlier child may have changed what the caller may write).
///
/// A `run_as_superuser` action (admin-set, with groups) runs with superuser mode: when the run
/// reaches such a node (the root or a descendant) the transaction records the elevation for the
/// audit trail (`Transaction::audited_superuser_env`) and the node and its subtree run
/// with the returned environment. The caller's own right to run the node was checked first, so a
/// refused caller records nothing; a store that cannot take the audit row refuses the run.
///
/// **Any error leaves the transaction in an unknown state**: a runner, or a child's access check,
/// can fail after earlier children wrote. The caller rolls the transaction back (or a savepoint
/// it took before the call) on every error; it never commits after one.
///
/// ```no_run
/// # #![recursion_limit = "256"]
/// # async fn demo(tx: &mut erp_sdk::Transaction, action: erp_addon_base::ir_actions::ActionId)
/// # -> erp_sdk::Result<()> {
/// use erp_addon_base::server_action::{RunRequest, run_server_action};
/// use erp_sdk::RealId;
/// let env = tx.env();
/// let ids: Vec<RealId> = RealId::new(7).into_iter().collect();
/// if let Some(next) = run_server_action(tx, &env, &RunRequest::new(action, "res.partner", &ids)).await? {
///     println!("the client opens {}", next.action_type());
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// [`ServerActionError`]s (as [`Error`]s with the typed error as `source()`): `NotFound`,
/// `ModelMismatch`, `Limit` for a tree over [`super::MAX_DEPTH`] or [`super::MAX_ACTIONS`],
/// `AccessDenied`, `HasWarnings`, `Misconfigured`, `SuperuserNeedsGroups` (a flagged action that
/// lost its groups), `Unsupported`, `UnknownState`, `MissingRunner`, `DuplicateRunner`, and the
/// runners' errors; the ORM's errors of the writes, as they are (an `Access` refusal of a child's
/// write included: children run as the caller).
pub async fn run_server_action(
    tx: &mut Transaction,
    env: &Env,
    req: &RunRequest<'_>,
) -> Result<Option<ActionResult>> {
    let tree = Tree::fetch(tx, req.action()).await?;
    if tree.nodes.iter().any(|node| {
        node.action.state() == "code"
            || node.action.evaluation_type == Some(EvaluationType::Equation)
    }) {
        super::script::check_active_count(req.active_ids().len())?;
        env.context().to_map_bounded(256)?;
    }
    let root = &tree.nodes[0].action;
    if root.model() != req.active_model() {
        return Err(ServerActionError::ModelMismatch {
            name: root.name().to_owned(),
            action_model: root.model().to_owned(),
            active_model: req.active_model().to_owned(),
        }
        .into());
    }
    let records = active_records(env, root, req.active_ids())?;
    access::check_can_run(tx, env, root, &records).await?;
    let runners = check_tree(env.registry(), &tree)?;
    let frame = Frame {
        tree,
        runners,
        active_ids: req.active_ids(),
        root_action: req.action().get(),
    };
    let base = node_env(tx, &frame, env, 0)?;
    run_runner(tx, &frame, 0, records, &base).await
}

/// The environment `node` and its subtree run with: `parent`'s, unless the node carries the
/// `run_as_superuser` flag, which records the elevation and returns superuser mode (
/// ). A flagged node inside an already elevated subtree records its own observation
/// (the transaction deduplicates by action) and changes nothing else.
fn node_env(tx: &mut Transaction, frame: &Frame<'_>, parent: &Env, node: usize) -> Result<Env> {
    let action = frame.action(node);
    if !action.run_as_superuser {
        return Ok(parent.clone());
    }
    tx.audited_superuser_env(
        parent,
        &SuperuserAction {
            action_id: action.id().get(),
            root_action_id: frame.root_action,
            group_ids: action.groups.iter().map(|g| g.get()).collect(),
            flag_write_uid: action.write_uid,
            flag_write_date_utc_micros: action.write_date_utc_micros,
        },
    )
}

/// The active records in the caller's environment, on the action's model.
fn active_records(env: &Env, action: &ServerAction, ids: &[RealId]) -> Result<Records> {
    env.records(action.model())?
        .browse(ids.iter().map(|id| id.get()))
}

/// What refuses an action whoever runs it, for every action of the tree in node order (the
/// action first): warnings (a child's warning is its parent's too, so the action asked for
/// reports it), a stored value outside its selection, `run_as_superuser` without groups (the
/// write rules were bypassed, and the run never falls back to the write check for such an
/// action), a type the registry does not have, an
/// `object_write` without the value type or many2many operation it needs, a type without
/// exactly one runner. Returns the runner of each node.
fn check_tree(registry: &erp_sdk::Registry, tree: &Tree) -> Result<Vec<&'static RunnerDecl>> {
    let states = states(registry)?;
    let mut runners = Vec::with_capacity(tree.nodes.len());
    for node in &tree.nodes {
        let action = &node.action;
        if node.warning {
            return Err(ServerActionError::HasWarnings {
                name: action.name().to_owned(),
            }
            .into());
        }
        if let Some(defect) = &node.defect {
            return Err(defect.clone().into());
        }
        if action.run_as_superuser && action.groups.is_empty() {
            return Err(ServerActionError::SuperuserNeedsGroups {
                name: action.name().to_owned(),
            }
            .into());
        }
        if !states.contains(&action.state()) {
            return Err(ServerActionError::UnknownState {
                name: action.name().to_owned(),
                state: action.state().to_owned(),
            }
            .into());
        }
        if action.state() == "object_write" {
            check_object_write(registry, action)?;
        }
        runners.push(runner_for(registry, action.state())?);
    }
    Ok(runners)
}

/// What `object_write` needs from the stored selections, refused before the run (review TD-D1):
/// a value type, and a many2many operation when the field to update is an x2many. Both columns
/// are nullable and a row written outside the ORM may leave them empty. A path that does not
/// resolve is not refused here: the runner reports it with its own error, and it has no field
/// type for the operation to be needed for.
fn check_object_write(registry: &erp_sdk::Registry, action: &ServerAction) -> Result<()> {
    let Some(evaluation_type) = action.evaluation_type else {
        return Err(action
            .misconfigured("its value type is empty, not one of value, sequence, equation")
            .into());
    };
    if evaluation_type != EvaluationType::Value || action.m2m_operation.is_some() {
        return Ok(());
    }
    let Some(path) = action.update_path.as_deref().filter(|p| !p.is_empty()) else {
        return Ok(());
    };
    let Ok((model, field)) = resolve_path(registry, action, path) else {
        return Ok(());
    };
    let spec = registry.model(model).and_then(|m| m.field(field));
    if let Some(spec) = spec
        && matches!(
            spec.ty,
            FieldType::One2many { .. } | FieldType::Many2many { .. }
        )
    {
        return Err(action
            .misconfigured(format!(
                "it updates the {} field {model}.{field}, but its many2many operation is \
                 empty, not one of add, remove, set, clear",
                spec.ty.type_name()
            ))
            .into());
    }
    Ok(())
}

/// Run a child of a `multi` action: its own access check as the caller, then its runner.
pub(crate) fn run_node<'a>(
    tx: &'a mut Transaction,
    frame: &'a Frame<'a>,
    node: usize,
    parent: &'a Env,
) -> BoxFuture<'a, Result<Option<ActionResult>>> {
    Box::pin(async move {
        let action = frame.action(node);
        // `parent` is the caller's environment, or its superuser mode below a flagged action:
        // the groups of the node are still the subject's own either way
        let records = active_records(parent, action, frame.active_ids)?;
        access::check_can_run(tx, parent, action, &records).await?;
        let base = node_env(tx, frame, parent, node)?;
        run_runner(tx, frame, node, records, &base).await
    })
}

/// Run the runner of `node` (checked by [`check_tree`]) on `records`.
async fn run_runner<'a>(
    tx: &'a mut Transaction,
    frame: &'a Frame<'a>,
    node: usize,
    records: Records,
    base: &'a Env,
) -> Result<Option<ActionResult>> {
    let action = frame.action(node);
    if action.state() == "code" || action.evaluation_type == Some(EvaluationType::Equation) {
        // Count before active_env clones caller keys; the three active keys are replaced below.
        base.context().to_map_bounded(256)?;
    }
    let model = action.model();
    let decl = frame.runners[node];
    let run = decl.runner();
    let ids = frame.active_ids;
    if decl.multi() {
        let env = active_env(base, model, ids, ids.first().copied());
        let records = records.with_env(env.clone());
        return run(RunnerCtx {
            tx,
            env: &env,
            records: &records,
            node,
            frame,
            base,
        })
        .await;
    }
    // once per active record, the record's id in the context; the result is the last
    // record's, empty or not, and no record runs nothing
    let mut result = None;
    for (index, id) in ids.iter().enumerate() {
        let env = active_env(base, model, std::slice::from_ref(id), Some(*id));
        let one: Records = records
            .slice(
                Some(isize::try_from(index).map_err(|_| too_many())?),
                Some(isize::try_from(index + 1).map_err(|_| too_many())?),
                1,
            )?
            .with_env(env.clone());
        result = run(RunnerCtx {
            tx: &mut *tx,
            env: &env,
            records: &one,
            node,
            frame,
            base,
        })
        .await?;
    }
    Ok(result)
}

fn too_many() -> Error {
    Error::programming("more active records than a recordset can index")
}

fn active_env(env: &Env, model: &str, ids: &[RealId], active_id: Option<RealId>) -> Env {
    let list: Arc<[Value]> = ids.iter().map(|id| Value::Int(id.get())).collect();
    let mut context = env.context().clone();
    context.set("active_model", Value::str(model));
    context.set("active_ids", Value::List(list));
    match active_id {
        Some(id) => context.set("active_id", Value::Int(id.get())),
        None => context.remove("active_id"),
    }
    env.with_context_replaced(context)
}
