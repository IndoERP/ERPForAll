//! An addon adds a value to `ir.actions.server.state` with `selection_add` and declares its
//! runner with [`server_action_runner!`](crate::server_action_runner). A declaration belongs to
//! the addon whose descriptor it names, by default the declaring crate's own `ADDON` (so the
//! name is not typed by hand), and counts only when an addon of that name is installed. The name
//! is the addon's identity: the module system installs `base` with a descriptor of its own. [`check_runners`] (called by `_register_hook` of
//! `ir.actions.server` on the final registry) refuses two runners for one state, a runner whose
//! state is not in the selection, and a state without a runner; the run looks its runner up the
//! same way and refuses the same cases for the action it runs.

use erp_sdk::prelude::*;
use erp_sdk::{AddonDescriptor, BoxFuture, Env, FieldType, Records, Transaction};

use super::run::Frame;
use super::tree::ServerAction;
use super::{ActionResult, ServerActionError};

/// A runner: runs one action on the records of its context.
pub type RunnerFn = for<'a> fn(RunnerCtx<'a>) -> BoxFuture<'a, Result<Option<ActionResult>>>;

/// A runner declaration ([`server_action_runner!`](crate::server_action_runner)).
#[derive(Debug)]
pub struct RunnerDecl {
    addon: &'static AddonDescriptor,
    state: &'static str,
    multi: bool,
    run: RunnerFn,
    file: &'static str,
    line: u32,
}

impl RunnerDecl {
    /// Used by [`server_action_runner!`](crate::server_action_runner); not an API.
    #[doc(hidden)]
    pub const fn __declare(
        addon: &'static AddonDescriptor,
        state: &'static str,
        multi: bool,
        run: RunnerFn,
        file: &'static str,
        line: u32,
    ) -> Self {
        Self {
            addon,
            state,
            multi,
            run,
            file,
            line,
        }
    }

    /// The name of the addon that declares it.
    pub fn addon(&self) -> &'static str {
        self.addon.name
    }

    /// The state it runs.
    pub fn state(&self) -> &'static str {
        self.state
    }

    /// Runs once for all active records (`true`) or once per record (`false`).
    pub fn multi(&self) -> bool {
        self.multi
    }

    /// `file:line (addon 'x')`.
    pub fn location(&self) -> String {
        format!("{}:{} (addon '{}')", self.file, self.line, self.addon.name)
    }

    pub(crate) fn runner(&self) -> RunnerFn {
        self.run
    }
}

/// Every runner linked into the binary.
#[erp_sdk::__private::linkme::distributed_slice]
#[linkme(crate = erp_sdk::__private::linkme)]
pub static SERVER_ACTION_RUNNERS: [RunnerDecl];

/// Declare the runner of a server action state: a `static` in [`SERVER_ACTION_RUNNERS`].
///
/// The runner belongs to the declaring crate's addon (`crate::ADDON`, made by
/// [`erp_sdk::addon!`]); a crate that holds several addons (test fixtures) names the descriptor
/// with `addon: PATH`. Either way the declaration counts only where an addon of that descriptor's
/// name is installed.
///
/// ```
/// use erp_addon_base::server_action::{ActionResult, RunnerCtx};
/// use erp_sdk::{BoxFuture, Result, serde_json::json};
///
/// erp_sdk::addon!(name = "mail_demo");
///
/// fn open_inbox(_ctx: RunnerCtx<'_>) -> BoxFuture<'_, Result<Option<ActionResult>>> {
///     Box::pin(async move {
///         Ok(Some(ActionResult::new(json!({"type": "ir.actions.client", "tag": "inbox"}))?))
///     })
/// }
///
/// erp_addon_base::server_action_runner!(
///     /// `inbox`: opens the inbox (added to `state` by `selection_add` in addon `mail_demo`).
///     INBOX = { state: "inbox", multi: true, run: open_inbox }
/// );
///
/// fn main() {
///     assert_eq!(INBOX.state(), "inbox");
///     assert_eq!(INBOX.addon(), "mail_demo");
/// }
/// ```
#[macro_export]
#[allow(
    clippy::crate_in_macro_def,
    reason = "`crate::ADDON` must name the calling crate's addon: that is how a runner belongs \
              to the addon that declares it (review TD-1)"
)]
macro_rules! server_action_runner {
    ($(#[$meta:meta])* $vis:vis $ident:ident = {
        state: $state:literal, multi: $multi:literal, run: $run:path $(,)?
    }) => {
        $crate::server_action_runner!(
            $(#[$meta])* $vis $ident = {
                addon: crate::ADDON, state: $state, multi: $multi, run: $run
            }
        );
    };
    ($(#[$meta:meta])* $vis:vis $ident:ident = {
        addon: $addon:path, state: $state:literal, multi: $multi:literal, run: $run:path $(,)?
    }) => {
        $(#[$meta])*
        #[$crate::__linkme::distributed_slice($crate::server_action::SERVER_ACTION_RUNNERS)]
        #[linkme(crate = $crate::__linkme)]
        $vis static $ident: $crate::server_action::RunnerDecl =
            $crate::server_action::RunnerDecl::__declare(
                &$addon,
                $state,
                $multi,
                $run,
                ::core::file!(),
                ::core::line!(),
            );
    };
}

/// What a runner gets: the transaction, the caller's environment with the run's context
/// (`active_model`, `active_ids`, `active_id`), the action and the active records (one record for
/// a runner declared non-`multi`). Everything it writes goes through this environment: never
/// `sudo`.
pub struct RunnerCtx<'a> {
    pub(crate) tx: &'a mut Transaction,
    pub(crate) env: &'a Env,
    pub(crate) records: &'a Records,
    pub(crate) node: usize,
    pub(crate) frame: &'a Frame<'a>,
    /// The environment this node runs with before the run's context keys: the caller's, or
    /// superuser mode below a `run_as_superuser` action. Its children start from it.
    pub(crate) base: &'a Env,
}

impl<'a> RunnerCtx<'a> {
    /// The transaction.
    pub fn tx(&mut self) -> &mut Transaction {
        self.tx
    }

    /// The caller's environment with the run's context.
    pub fn env(&self) -> &'a Env {
        self.env
    }

    /// The active records, in the caller's environment.
    pub fn records(&self) -> &'a Records {
        self.records
    }

    /// The action.
    pub fn action(&self) -> &'a ServerAction {
        self.frame.action(self.node)
    }

    /// Run the action's children in order, each through the full run (its own access check,
    /// warnings, runner, as the caller); the last result that is not empty.
    pub(crate) async fn run_children(&mut self) -> Result<Option<ActionResult>> {
        let frame = self.frame;
        let mut last = None;
        for &child in frame.children(self.node) {
            if let Some(result) = super::run::run_node(self.tx, frame, child, self.base).await? {
                last = Some(result);
            }
        }
        Ok(last)
    }
}

/// The installed runner declarations: `base`'s ([`super::runners`]) and the linked ones whose
/// addon is in the registry, by name (the module system installs `base` with its own
/// descriptor, so the same static is not required).
fn installed(registry: &erp_sdk::Registry) -> impl Iterator<Item = &'static RunnerDecl> + '_ {
    super::runners::BUILTIN
        .iter()
        .copied()
        .chain(SERVER_ACTION_RUNNERS.iter())
        .filter(|decl| registry.addons().iter().any(|a| a.name == decl.addon.name))
}

/// The values of `ir.actions.server.state` in `registry`.
pub(crate) fn states(registry: &erp_sdk::Registry) -> Result<Vec<&str>> {
    let spec = registry
        .model("ir.actions.server")
        .ok_or_else(|| Error::programming("the registry has no model 'ir.actions.server'"))?;
    match spec.field("state").map(|f| &f.ty) {
        Some(FieldType::Selection {
            values: Some(values),
            ..
        }) => Ok(values.iter().map(|(k, _)| k.as_str()).collect()),
        _ => Err(Error::programming(
            "ir.actions.server.state is not a selection with a list of values",
        )),
    }
}

/// The runner of `state` (a value of the selection): exactly one installed declaration.
pub(crate) fn runner_for(
    registry: &erp_sdk::Registry,
    state: &str,
) -> Result<&'static RunnerDecl, ServerActionError> {
    let mut found = installed(registry).filter(|decl| decl.state == state);
    let Some(first) = found.next() else {
        return Err(ServerActionError::MissingRunner {
            state: state.to_owned(),
        });
    };
    if let Some(second) = found.next() {
        return Err(ServerActionError::DuplicateRunner {
            state: state.to_owned(),
            first: first.location(),
            second: second.location(),
        });
    }
    Ok(first)
}

/// Check the runners of `registry`: one per state of `ir.actions.server.state`, none for a state
/// it does not have. Called by `_register_hook` of `ir.actions.server`.
///
/// # Errors
///
/// [`ServerActionError::DuplicateRunner`], [`ServerActionError::RunnerWithoutState`] or
/// [`ServerActionError::MissingRunner`] (kind `Programming`), the first in state order; a
/// `Programming` error when the registry has no `ir.actions.server` with a listed `state`.
pub fn check_runners(registry: &erp_sdk::Registry) -> Result<()> {
    let states = states(registry)?;
    let mut decls: Vec<&'static RunnerDecl> = installed(registry).collect();
    decls.sort_by_key(|d| (d.state, d.file, d.line));
    if let Some(pair) = decls.windows(2).find(|w| w[0].state == w[1].state) {
        return Err(ServerActionError::DuplicateRunner {
            state: pair[0].state.to_owned(),
            first: pair[0].location(),
            second: pair[1].location(),
        }
        .into());
    }
    if let Some(orphan) = decls.iter().find(|d| !states.contains(&d.state)) {
        return Err(ServerActionError::RunnerWithoutState {
            state: orphan.state.to_owned(),
            location: orphan.location(),
        }
        .into());
    }
    let mut missing: Vec<&str> = states
        .iter()
        .copied()
        .filter(|s| !decls.iter().any(|d| d.state == *s))
        .collect();
    missing.sort_unstable();
    if let Some(state) = missing.first() {
        return Err(ServerActionError::MissingRunner {
            state: (*state).to_owned(),
        }
        .into());
    }
    Ok(())
}
