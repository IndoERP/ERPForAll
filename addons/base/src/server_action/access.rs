//! Who may run an action (`_can_execute_action_on_records`).

use erp_sdk::prelude::*;
use erp_sdk::{Env, ErrorKind, Operation, Records, Transaction};

use super::ServerActionError;
use super::tree::ServerAction;

/// Without groups: write access on the action's model and
/// on the active records, record rules included (an empty `records` checks the model only).
/// Superuser mode passes the write check, as every access check; it does not add groups.
///
/// # Errors
///
/// [`ServerActionError::AccessDenied`] (kind `Access`) for a refusal; the access engine's other
/// errors as they are.
pub(crate) async fn check_can_run(
    tx: &mut Transaction,
    env: &Env,
    action: &ServerAction,
    records: &Records,
) -> Result<()> {
    if !action.groups.is_empty() {
        return if tx.has_any_group(env, &action.groups).await? {
            Ok(())
        } else {
            Err(ServerActionError::AccessDenied { detail: None }.into())
        };
    }
    match records.check_access(tx, Operation::Write).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::Access => Err(ServerActionError::AccessDenied {
            detail: Some(error.message().to_owned()),
        }
        .into()),
        Err(error) => Err(error),
    }
}
