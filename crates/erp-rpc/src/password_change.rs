//! Closed managed password boundary; no browser credential or arbitrary executor is stored here.
use crate::RpcFailure;
use erp_fields::{Error, Result, Value, ValueMap};
use erp_orm::{Context, Principal, rpc::RpcArgs};
use std::sync::{Mutex, PoisonError};

/// Fixed session operations whose authentication service owns the transaction.
/// Exhaustive: adding an operation requires updating every authority consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordOperation {
    /// Open selected-user reset metadata after actor confirmation.
    OpenReset,
    /// Open the current actor's own-password metadata.
    OpenOwn,
    /// Verify an explicit old password and change the actor's password.
    LegacyChange,
    /// Verify the actor's password and refresh the shared identity window.
    ConfirmIdentity,
    /// Submit an owned own-password form with exact confirmation.
    ChangeOwn,
    /// Submit selected immutable lines of an owned reset wizard atomically.
    ResetWizard,
    /// Submit one immutable line through the same reset authority.
    ResetLine,
}
impl PasswordOperation {
    /// Whether the operation can change a stored password. Only these report an unknown
    /// commit when the deadline cancels them; the open and identity-confirmation operations
    /// never touch a credential, so a timeout there is a plain deadline error.
    pub(crate) const fn changes_password(self) -> bool {
        matches!(
            self,
            Self::LegacyChange | Self::ChangeOwn | Self::ResetWizard | Self::ResetLine
        )
    }
    pub(crate) fn lookup(model: &str, method: &str) -> Option<Self> {
        Some(match (model, method) {
            ("res.users", "action_change_password_wizard") => Self::OpenReset,
            ("res.users", "preference_change_password") => Self::OpenOwn,
            ("res.users", "change_password") => Self::LegacyChange,
            ("res.users.identitycheck", "run_check") => Self::ConfirmIdentity,
            ("change.password.own", "change_password") => Self::ChangeOwn,
            ("change.password.wizard", "change_password_button") => Self::ResetWizard,
            ("change.password.user", "change_password_button") => Self::ResetLine,
            _ => return None, // every other method retains ordinary RPC execution
        })
    }
}
/// Validated RPC input, with a refusal-only gate reused by the authentication transaction.
/// Debug intentionally excludes every argument and context value.
#[non_exhaustive]
pub struct PasswordCall<'a> {
    pub(crate) operation: PasswordOperation,
    pub(crate) route: &'a crate::Route,
    pub(crate) ids: Vec<i64>,
    pub(crate) args: RpcArgs,
    pub(crate) context: Context,
    pub(crate) gate: &'a (dyn Fn(&Principal) -> std::result::Result<(), RpcFailure> + Send + Sync),
    pub(crate) failure: &'a Mutex<Option<RpcFailure>>,
}
impl std::fmt::Debug for PasswordCall<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordCall")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
impl PasswordCall<'_> {
    /// The closed operation selected after exposure and binding.
    pub fn operation(&self) -> PasswordOperation {
        self.operation
    }
    /// Record identifiers bound by RPC; these never establish ownership or authority.
    pub fn ids(&self) -> &[i64] {
        &self.ids
    }
    /// Convert a bound parameter with the ordinary RPC argument diagnostic.
    /// # Errors
    /// The normal strict binding error for a missing or incorrectly typed argument.
    pub fn take<T: erp_orm::rpc::FromRpc>(
        &mut self,
        name: &'static str,
    ) -> std::result::Result<T, RpcFailure> {
        self.args
            .take(name)
            .map_err(|e| crate::binding::bind_failure(self.route, e))
    }
    /// Bound caller context; it conveys no identity authority.
    pub fn context(&self) -> &Context {
        &self.context
    }
    /// Recheck RPC gates against the fresh principal in the mutation's transaction.
    /// # Errors
    /// A gate refusal; its original protocol failure is retained by RPC for rendering.
    pub fn authorize(&self, principal: &Principal) -> Result<()> {
        match (self.gate)(principal) {
            Ok(()) => Ok(()),
            Err(error) => {
                *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
                Err(Error::access("Password operation denied by RPC policy."))
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ReplyKind {
    Required,
    Own(i64),
    Reset(i64),
    Closed,
    Changed,
}
/// A fixed, secret-free result whose largest serialization is admitted before execution.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct PasswordReply(ReplyKind);
impl PasswordReply {
    /// Ask the browser to confirm its current actor's identity.
    pub fn required() -> Self {
        Self(ReplyKind::Required)
    }
    /// Complete a form operation.
    pub fn closed() -> Self {
        Self(ReplyKind::Closed)
    }
    /// Successful legacy change_password boolean result.
    pub fn changed() -> Self {
        Self(ReplyKind::Changed)
    }
    /// Open an own-password wizard already created by the server.
    /// # Errors
    /// Validation for an identifier outside the positive JSON-safe integer range.
    pub fn own(id: i64) -> Result<Self> {
        valid_id(id)?;
        Ok(Self(ReplyKind::Own(id)))
    }
    /// Open an administrator wizard already created by the server.
    /// # Errors
    /// Validation for an identifier outside the positive JSON-safe integer range.
    pub fn reset(id: i64) -> Result<Self> {
        valid_id(id)?;
        Ok(Self(ReplyKind::Reset(id)))
    }
    pub(crate) fn possible() -> [Self; 5] {
        [
            Self::required(),
            Self::closed(),
            Self::changed(),
            Self(ReplyKind::Own(9_007_199_254_740_991)),
            Self(ReplyKind::Reset(9_007_199_254_740_991)),
        ]
    }
    pub(crate) fn value(self) -> Value {
        let (model, name, id) = match self.0 {
            ReplyKind::Changed => return Value::Bool(true),
            ReplyKind::Closed => {
                return Value::Map(std::sync::Arc::new(ValueMap::from_iter([(
                    "type".into(),
                    Value::str("ir.actions.act_window_close"),
                )])));
            }
            ReplyKind::Required => ("res.users.identitycheck", "Confirm your identity", None),
            ReplyKind::Own(id) => ("change.password.own", "Change password", Some(id)),
            ReplyKind::Reset(id) => ("change.password.wizard", "Reset passwords", Some(id)),
        };
        let mut value = ValueMap::from_iter([
            ("type".into(), Value::str("ir.actions.act_window")),
            ("name".into(), Value::str(name)),
            ("res_model".into(), Value::str(model)),
            ("target".into(), Value::str("new")),
            ("view_mode".into(), Value::str("form")),
        ]);
        if let Some(id) = id {
            value.insert("res_id".into(), Value::Int(id));
        }
        Value::Map(std::sync::Arc::new(value))
    }
}
fn valid_id(id: i64) -> Result<()> {
    if (1..=9_007_199_254_740_991).contains(&id) {
        Ok(())
    } else {
        Err(Error::validation(
            "Password wizard identifier is outside the supported range.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::PasswordOperation as Op;

    /// unknown commit)
    #[test]
    fn only_password_mutators_report_an_unknown_commit_on_timeout() {
        let mutating = [
            Op::LegacyChange,
            Op::ChangeOwn,
            Op::ResetWizard,
            Op::ResetLine,
        ];
        let reading = [Op::OpenReset, Op::OpenOwn, Op::ConfirmIdentity];
        assert!(mutating.iter().all(|op| op.changes_password()));
        assert!(reading.iter().all(|op| !op.changes_password()));
    }
}
