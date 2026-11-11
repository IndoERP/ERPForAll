//! The typed errors of running and writing server actions.

use std::fmt;

use erp_sdk::ErrorKind;

use crate::ir_actions::ActionId;

/// A bound of [`super::run_server_action`] or of the loop check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunLimit {
    /// Levels of `multi` children ([`super::MAX_DEPTH`]).
    Depth,
    /// Actions in one run ([`super::MAX_ACTIONS`]).
    Actions,
    /// Actions checked by one relationship mutation ([`super::MAX_ACTIONS`]).
    MutationActions,
    /// Parent actions walked by the loop check ([`super::MAX_CHAIN`]).
    Chain,
    /// Steps of an `update_path` ([`super::MAX_PATH_STEPS`]).
    PathSteps,
    /// Bytes in a selected stored text field ([`super::MAX_FIELD_BYTES`]).
    FieldBytes,
    /// Groups on one action ([`super::MAX_GROUPS`]).
    Groups,
    /// Decoded variable bytes of the tree ([`super::MAX_TREE_BYTES`]).
    TreeBytes,
}

impl RunLimit {
    /// The bound itself.
    pub fn max(self) -> usize {
        match self {
            Self::Depth => super::MAX_DEPTH,
            Self::Actions => super::MAX_ACTIONS,
            Self::MutationActions => super::MAX_ACTIONS,
            Self::Chain => super::MAX_CHAIN,
            Self::PathSteps => super::MAX_PATH_STEPS,
            Self::FieldBytes => super::MAX_FIELD_BYTES,
            Self::Groups => super::MAX_GROUPS,
            Self::TreeBytes => super::MAX_TREE_BYTES,
        }
    }
}

impl fmt::Display for RunLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Depth => "levels of child actions",
            Self::Actions => "actions in one run",
            Self::MutationActions => "actions in one mutation",
            Self::Chain => "parent actions in a chain",
            Self::PathSteps => "steps in an update path",
            Self::FieldBytes => "bytes in a stored field",
            Self::Groups => "groups on one action",
            Self::TreeBytes => "bytes in the action tree payload",
        })
    }
}

/// Why a server action does not run, or why an action write is refused. Converted into
/// [`erp_sdk::Error`] with the kind of [`ServerActionError::kind`], the message of its `Display`
/// and itself as the `source()`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerActionError {
    /// Both the `action` global and return value are non-nil.
    AmbiguousResult,
    /// No script worker permit is available; no VM was enqueued.
    ScriptBusy,
    /// The caller is not in the action's groups, or (no groups) may not write the model or the
    /// active records.
    AccessDenied {
        /// The access engine's refusal, when the write check gave one.
        detail: Option<String>,
    },
    /// Writing `parent_id`/`child_ids` would make an action its own ancestor.
    ChildLoop,
    /// The action or one of its children has warnings.
    HasWarnings {
        /// The action's name.
        name: String,
    },
    /// Only administrators (`base.group_system`, or superuser mode) set `run_as_superuser` or
    /// change an action that carries it.
    SuperuserAdminOnly,
    /// An action with `run_as_superuser` must keep at least one group.
    SuperuserNeedsGroups {
        /// The action's name.
        name: String,
    },
    /// Not built yet: `feature` is added by the board task `task`.
    Unsupported {
        /// What was asked.
        feature: String,
        /// The task that adds it.
        task: &'static str,
    },
    /// A bound was exceeded ([`RunLimit::max`] is the bound).
    Limit {
        /// Which.
        kind: RunLimit,
    },
    /// No server action has this id.
    NotFound {
        /// The id asked for.
        id: ActionId,
    },
    /// The action runs on another model than the active records'.
    ModelMismatch {
        /// The action's name.
        name: String,
        /// The action's model.
        action_model: String,
        /// The model of the request.
        active_model: String,
    },
    /// The action's `state` is not a value of `ir.actions.server.state` in this registry (a row
    /// written outside the ORM, or the addon that added the state is gone).
    UnknownState {
        /// The action's name.
        name: String,
        /// Its state.
        state: String,
    },
    /// No installed addon declares a runner for the state.
    MissingRunner {
        /// The state.
        state: String,
    },
    /// Two installed declarations run the same state.
    DuplicateRunner {
        /// The state.
        state: String,
        /// Where the first is declared.
        first: String,
        /// Where the second is declared.
        second: String,
    },
    /// A runner is declared for a state the selection does not have.
    RunnerWithoutState {
        /// The state.
        state: String,
        /// Where it is declared.
        location: String,
    },
    /// The action's stored configuration cannot be run (no `update_path`, a model that is not in
    /// the registry, …).
    Misconfigured {
        /// The action's name.
        name: String,
        /// What is wrong.
        reason: String,
    },
    /// The `update_path` does not lead to a writable field.
    UpdatePath {
        /// The action's name.
        name: String,
        /// The path.
        path: String,
        /// What is wrong.
        reason: String,
    },
    /// The action's `value` does not convert to the type of the written field.
    BadValue {
        /// The action's name.
        name: String,
        /// `model.field`.
        field: String,
        /// The value (at most 64 characters of it).
        value: String,
        /// The type expected.
        expected: &'static str,
    },
    /// `object_write` does not write fields of this type.
    FieldType {
        /// The action's name.
        name: String,
        /// `model.field`.
        field: String,
        /// The field type.
        type_name: &'static str,
    },
    SystemField {
        /// The action's name.
        name: String,
        /// `model.field`.
        field: String,
    },
    NoRecName {
        /// The model.
        model: String,
    },
    /// The `link_field_id` of an `object_create` does not link the action's model to the created
    /// model.
    LinkField {
        /// The action's name.
        name: String,
        /// What is wrong.
        reason: String,
    },
    /// A runner returned something that is not an action dictionary.
    InvalidResult {
        /// What is wrong with it.
        reason: String,
    },
}

impl ServerActionError {
    /// The error kind it is reported with.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::ScriptBusy => ErrorKind::ResourceExhausted,
            Self::AmbiguousResult => ErrorKind::Programming,
            Self::AccessDenied { .. } | Self::SuperuserAdminOnly => ErrorKind::Access,
            Self::ChildLoop
            | Self::SuperuserNeedsGroups { .. }
            | Self::ModelMismatch { .. }
            | Self::Misconfigured { .. }
            | Self::UpdatePath { .. }
            | Self::BadValue { .. }
            | Self::FieldType { .. }
            | Self::SystemField { .. }
            | Self::LinkField { .. } => ErrorKind::Validation,
            Self::HasWarnings { .. }
            | Self::Unsupported { .. }
            | Self::Limit { .. }
            | Self::NoRecName { .. } => ErrorKind::UserError,
            Self::NotFound { .. } => ErrorKind::Missing,
            Self::UnknownState { .. }
            | Self::MissingRunner { .. }
            | Self::DuplicateRunner { .. }
            | Self::RunnerWithoutState { .. }
            | Self::InvalidResult { .. } => ErrorKind::Programming,
        }
    }
}

impl fmt::Display for ServerActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AmbiguousResult => {
                f.write_str("A server action set action and returned a value; choose one result.")
            }
            Self::ScriptBusy => {
                f.write_str("All server action script workers are busy; the run was refused.")
            }
            Self::AccessDenied { .. } => {
                f.write_str("You don't have enough access rights to run this action.")
            }
            Self::ChildLoop => f.write_str("Recursion found in child server actions"),
            Self::HasWarnings { name } => write!(
                f,
                "Server action {name} has one or more warnings, address them first."
            ),
            Self::SuperuserAdminOnly => f.write_str(
                "Only administrators (Settings) can set Run as Superuser on a server action or \
                 change an action that has it.",
            ),
            Self::SuperuserNeedsGroups { name } => write!(
                f,
                "Server action {name} runs as superuser: restrict it to at least one group."
            ),
            Self::Unsupported { feature, task } => {
                write!(f, "{feature} is not supported until {task}.")
            }
            Self::Limit { kind } => write!(
                f,
                "The server action has more than {} {kind}; the run is refused.",
                kind.max()
            ),
            Self::NotFound { id } => write!(f, "Server action {id} does not exist."),
            Self::ModelMismatch {
                name,
                action_model,
                active_model,
            } => write!(
                f,
                "Server action {name} runs on model '{action_model}', not on '{active_model}'."
            ),
            Self::UnknownState { name, state } => write!(
                f,
                "Server action {name} has the type '{state}', which ir.actions.server.state does \
                 not have in this registry."
            ),
            Self::MissingRunner { state } => write!(
                f,
                "No runner for server actions of type '{state}': the addon that adds the type \
                 must declare one with server_action_runner!."
            ),
            Self::DuplicateRunner {
                state,
                first,
                second,
            } => write!(
                f,
                "Server actions of type '{state}' have two runners: {first} and {second}."
            ),
            Self::RunnerWithoutState { state, location } => write!(
                f,
                "A runner for server actions of type '{state}' is declared at {location}, but \
                 ir.actions.server.state has no such value: add it with selection_add."
            ),
            Self::Misconfigured { name, reason } => {
                write!(f, "Server action {name} cannot run: {reason}.")
            }
            Self::UpdatePath { name, path, reason } => write!(
                f,
                "Server action {name}: the path '{path}' to update {reason}."
            ),
            Self::BadValue {
                name,
                field,
                value,
                expected,
            } => write!(
                f,
                "Server action {name}: '{value}' is not a valid {expected} for {field}."
            ),
            Self::FieldType {
                name,
                field,
                type_name,
            } => write!(
                f,
                "Server action {name} cannot update {field}: Update Record does not write fields \
                 of type '{type_name}'."
            ),
            Self::SystemField { name, field } => write!(
                f,
                "Server action {name} cannot update {field}: it is a system column, which Update \
                 Record does not write."
            ),
            Self::NoRecName { model } => write!(
                f,
                "Cannot execute name_create, no _rec_name defined on {model}"
            ),
            Self::LinkField { name, reason } => {
                write!(f, "Server action {name}: the link field {reason}.")
            }
            Self::InvalidResult { reason } => write!(
                f,
                "A server action runner returned a result that is not an action: {reason}."
            ),
        }
    }
}

impl std::error::Error for ServerActionError {}

impl From<ServerActionError> for erp_sdk::Error {
    fn from(error: ServerActionError) -> Self {
        erp_sdk::Error::new(error.kind(), error.to_string()).with_source(error)
    }
}

/// At most 64 characters of `text`, for messages that quote stored data.
pub(crate) fn excerpt(text: &str) -> String {
    const MAX: usize = 64;
    match text.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}
