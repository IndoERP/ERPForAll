// SPDX-License-Identifier: MIT OR Apache-2.0
//! The attachment schedules kept by a module operation of the native compositions: installing or
//! upgrading `base` creates the recurring attachment rows that are missing, in the operation's
//! own transaction. No runner is started here; the rows run only where a job runner runs.
use erp_modules::{LifecycleChange, LifecycleHook};
use erp_orm::{BoxFuture, PrincipalKind, Transaction};

/// Host lifecycle service registered next to the asset lifecycle in both native compositions
/// (`Node::open` with native addons and the live `erp module` composition).
///
/// When `base` is in the installed or upgraded set of the operation, it calls
/// [`erp_addon_base::attachments::gc::ensure_missing_schedules`] inside the operation's
/// transaction, which the module system opens as the system principal. It does not check the
/// attachment store topology: the module operation's ORM carries no attachment runtime; the
/// topology check stays at upload and before every destructive store call.
#[derive(Debug)]
pub(crate) struct AttachmentScheduleLifecycle;

impl LifecycleHook for AttachmentScheduleLifecycle {
    fn before_commit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        change: LifecycleChange<'a>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            let delta = change.change();
            if !delta
                .installed
                .iter()
                .chain(delta.upgraded)
                .any(|name| name.as_str() == "base")
            {
                return Ok(());
            }
            let env = tx.env();
            if env.principal().kind() != PrincipalKind::System {
                return Err(erp_fields::Error::programming(
                    "The base install/upgrade keeps the attachment schedules as the system \
                     principal, but the module operation's transaction runs as another principal.",
                )
                .into());
            }
            erp_addon_base::attachments::gc::ensure_missing_schedules(tx, &env).await?;
            Ok(())
        })
    }
}
