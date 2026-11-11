//! Running server actions (`ir.actions.server`,; design,
//! brief).
//!
//! [`run_server_action`] runs one action **as the caller**: every check and every write goes
//! through the caller's environment, children of a `multi` action included.
//!
//! the warnings: a child on another model, a child whose groups
//!    differ, a child with warnings, a JSON field at the end of `update_path`, a sequence on a
//!    field that is not text: [`ServerActionError::HasWarnings`];
//!    A state without a runner is an error, never a no-op.
//!
//! The action and its whole tree of children are read in one statement before anything runs,
//! bounded by [`MAX_DEPTH`] levels and [`MAX_ACTIONS`] actions; stored text and groups also obey
//! [`MAX_FIELD_BYTES`], [`MAX_GROUPS`] and [`MAX_TREE_BYTES`] before reaching the driver.
//! A tree over either structural bound (a loop
//! written outside the ORM included) is refused before the first runner starts. After the
//! action's own access check, every action of the tree is checked for what refuses it whoever
//! runs it (warnings, the superuser flag, a type without a runner, a stored value outside its
//! selection) before the first one runs. An error can still come after a child wrote (a later
//! child's access check, a runner's or the ORM's error): **the caller rolls back on every error**
//! ([`run_server_action`]).
//!
//! Built-in runners ([`runners`]): `object_write` and `object_create` (as the caller), `multi`;
//! sandboxed Luau `code` and per-record equations; `object_copy`, `webhook`, and running an action
//! flagged `run_as_superuser` waits for the audit trail: each is a typed
//! [`ServerActionError::Unsupported`] naming that task.
//!
//! Machine view: one statement reads the tree (actions, their models, groups and the fields they
//! name, joined); the access check costs what the access engine's write check costs (memoized per
//! transaction for the records); `object_write` writes every active record in one ORM `write`;
//! nothing is kept between runs, so a run has no state outside its transaction (any node can run
//! any action; a node that dies loses only its transaction).

mod access;
mod error;
mod path;
pub mod registry;
mod run;
pub mod runners;
mod script;
mod tree;

pub use error::{RunLimit, ServerActionError};
pub use registry::{RunnerCtx, RunnerDecl, RunnerFn, SERVER_ACTION_RUNNERS, check_runners};
pub use run::run_server_action;
pub use tree::ServerAction;
pub(crate) use tree::column_error;

use erp_sdk::{RealId, serde_json};

use crate::ir_actions::ActionId;

/// Deepest nesting of `multi` children run (the action itself is level 1; design m2-s3 §12).
pub const MAX_DEPTH: usize = 8;

/// Most actions in one run, the action itself included (design m2-s3 §12).
/// Also caps the unique actions checked by a nonsuperuser relationship mutation's security guard.
pub const MAX_ACTIONS: usize = 256;

/// Maximum UTF-8 bytes in any selected stored text field, including related model/field names.
/// Enforced by SQL before text is transferred to the driver; an excess refuses the whole run.
pub const MAX_FIELD_BYTES: usize = 65_536;

/// Maximum groups on one action. Accepted authorization sets are complete, never truncated.
pub const MAX_GROUPS: usize = 256;

/// Maximum decoded variable payload for one action tree: selected text bytes plus eight bytes
/// per group id. Fixed scalar fields, protocol framing and container overhead are separately
/// bounded by [`MAX_ACTIONS`] and [`MAX_GROUPS`]; this is not a measured heap or wire-byte ceiling.
pub const MAX_TREE_BYTES: usize = 1_048_576;

/// Longest chain of parent actions walked when `parent_id`/`child_ids` are written (the loop
/// check): a chain this long cannot be run from its top anyway ([`MAX_DEPTH`]).
pub const MAX_CHAIN: usize = 256;

/// Most `.`-separated steps in an `update_path`.
pub const MAX_PATH_STEPS: usize = 16;

/// The context and the user are the caller's environment, given to [`run_server_action`].
/// The active records are database rows ([`RealId`]): a new record has nothing to run on.
#[derive(Debug, Clone, Copy)]
pub struct RunRequest<'a> {
    action: ActionId,
    active_model: &'a str,
    active_ids: &'a [RealId],
}

impl<'a> RunRequest<'a> {
    pub fn new(action: ActionId, active_model: &'a str, active_ids: &'a [RealId]) -> Self {
        Self {
            action,
            active_model,
            active_ids,
        }
    }

    /// The action.
    pub fn action(&self) -> ActionId {
        self.action
    }

    /// The model of the active records.
    pub fn active_model(&self) -> &'a str {
        self.active_model
    }

    /// The active records.
    pub fn active_ids(&self) -> &'a [RealId] {
        self.active_ids
    }
}

/// What a run returns for the client to do next: a JSON object whose `type` is a non-empty
/// string (an action dictionary, such as `{"type": "ir.actions.act_window", …}`). Built only
/// through [`ActionResult::new`], so a runner cannot hand anything else back.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionResult {
    /// The `type`, kept apart so reading it needs no fallback.
    kind: String,
    map: serde_json::Map<String, serde_json::Value>,
}

impl ActionResult {
    /// Check `value` is an action dictionary.
    ///
    /// ```
    /// use erp_addon_base::server_action::ActionResult;
    /// use erp_sdk::serde_json::json;
    /// let result = ActionResult::new(json!({"type": "ir.actions.act_url", "url": "/"}))?;
    /// assert_eq!(result.action_type(), "ir.actions.act_url");
    /// assert!(ActionResult::new(json!({"url": "/"})).is_err());
    /// # Ok::<(), erp_addon_base::server_action::ServerActionError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`ServerActionError::InvalidResult`] for a value that is not an object, or whose `type` is
    /// absent, not a string or empty.
    pub fn new(value: serde_json::Value) -> Result<Self, ServerActionError> {
        let invalid = |reason: &str| ServerActionError::InvalidResult {
            reason: reason.to_owned(),
        };
        let serde_json::Value::Object(map) = value else {
            return Err(invalid("it is not a JSON object"));
        };
        match map.get("type") {
            Some(serde_json::Value::String(kind)) if !kind.is_empty() => Ok(Self {
                kind: kind.clone(),
                map,
            }),
            Some(serde_json::Value::String(_)) => Err(invalid("its 'type' is empty")),
            Some(_) => Err(invalid("its 'type' is not a string")),
            None => Err(invalid("it has no 'type'")),
        }
    }

    /// The action type (`type`).
    pub fn action_type(&self) -> &str {
        &self.kind
    }

    /// The dictionary.
    pub fn as_map(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.map
    }

    /// The dictionary as a JSON value.
    pub fn into_value(self) -> serde_json::Value {
        serde_json::Value::Object(self.map)
    }
}
