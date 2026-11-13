// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit operator recovery; owner/audit admission precedes opening mutable stores.
use crate::{
    CliError, Result,
    access::{pool_with_size, registry},
    args::{AttachmentAction, AttachmentReasonArg, AttachmentTargetArgs},
    human,
};
use erp_addon_base::attachments::recovery::{self, Reason};
use erp_orm::{BeginOptions, Isolation, Orm, SUPERUSER_ID};
use erp_sdk::blob::{AttachmentRuntime, Namespace};
use std::sync::Arc;
fn invalid(arg: &str, reason: &str) -> CliError {
    CliError::InvalidInput {
        arg: arg.to_owned(),
        reason: reason.to_owned(),
    }
}
pub(crate) fn execute(action: &AttachmentAction) -> Result<String> {
    let target = match action {
        AttachmentAction::ProbeStore(_) => {
            return Err(invalid(
                "operation",
                "peer probe must use its dedicated operator runner",
            ));
        }
        AttachmentAction::Inspect(t) => t,
        AttachmentAction::Claim(c) => &c.target,
        AttachmentAction::Status(s) => &s.target,
    };
    if target.store.is_empty()
        || target.store.len() > 64
        || !target
            .store
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(invalid(
            "--store",
            "expected a configured store key of 1 to 64 ASCII letters, digits, underscores or hyphens",
        ));
    }
    let operation = match action {
        AttachmentAction::Claim(c) => c.resume.as_deref(),
        AttachmentAction::Status(s) => Some(s.operation.as_str()),
        AttachmentAction::Inspect(_) => None,
        AttachmentAction::ProbeStore(_) => None,
    }
    .map(Namespace::parse)
    .transpose()
    .map_err(|_| invalid("operation", "expected a canonical nonzero operation UUID"))?;
    if let AttachmentAction::Claim(c) = action
        && c.confirm
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 8192)
    {
        return Err(invalid(
            "--confirm",
            "expected the bounded token printed by attachments inspect",
        ));
    }
    human::runtime()?.block_on(execute_async(action, target, operation))
}

// Job settings: like every served `Orm`, this one keeps `JobSettings::default()` (no builder
// outside tests calls `.job_settings`). `recovery::begin` ages a pending owner advancement against
// `job_settings().job_deadline()`, so once a deployment configures `jobs.*`, this builder must load
// the same values as the served binary.
fn owner_orm(
    pool: &deadpool_postgres::Pool,
    runtime: Option<Arc<AttachmentRuntime>>,
) -> Result<Orm> {
    let registry = registry()?;
    let mut builder = Orm::builder(Arc::clone(&registry), pool.clone())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables());
    if let Some(runtime) = runtime {
        builder =
            builder.generation_attachment(erp_orm::GenerationAttachment::new(registry, runtime));
    }
    Ok(builder.build()?)
}

async fn execute_async(
    action: &AttachmentAction,
    target: &AttachmentTargetArgs,
    operation: Option<Namespace>,
) -> Result<String> {
    let config = human::config(&target.config)?;
    let pool = pool_with_size(config.owner_connection(), 1)?;
    let preflight = owner_orm(&pool, None)?;
    let mut tx = preflight
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await?;
    recovery::require_owner(&mut tx).await?;
    tx.rollback().await?;
    drop(tx);
    if matches!(action, AttachmentAction::Claim(_)) {
        let state = preflight.check_audit_store().await?;
        if !state.is_ready() {
            return Err(CliError::Execution(
                "Attachment claim requires a ready audit store; run erp audit maintain before retrying.".into()
            ));
        }
    }
    let runtime = human::open_attachments(&config).await?;
    let orm = owner_orm(&pool, Some(runtime))?;
    if matches!(action, AttachmentAction::Claim(_)) {
        orm.check_audit_store().await?;
    }
    match action {
        AttachmentAction::ProbeStore(_) => Err(invalid(
            "operation",
            "peer probe must use its dedicated operator runner",
        )),
        AttachmentAction::Inspect(_) => {
            let inspected = recovery::inspect(&orm, &target.store).await?;
            let identity = inspected.identity();
            Ok(format!(
                "Selected database: {} (cluster {}, OID {}, incarnation {})\nNamespace: {}\nObserved owner: {:?}\nConfirm: {}\nIsolate competing databases before claim --force.",
                config.database(),
                identity.system_identifier(),
                identity.database_oid(),
                identity.incarnation(),
                inspected.namespace(),
                inspected.owner_marker(),
                inspected.confirmation()
            ))
        }
        AttachmentAction::Status(_) => {
            let id = operation
                .as_ref()
                .ok_or_else(|| invalid("--operation", "missing operation UUID"))?;
            let claim = recovery::resume(&orm, &target.store, id).await?;
            Ok(format!(
                "Operation: {}\nPhase: {}",
                claim.id(),
                claim.phase(&orm).await?
            ))
        }
        AttachmentAction::Claim(args) => {
            let claim = if let Some(id) = operation.as_ref() {
                recovery::resume(&orm, &target.store, id).await?
            } else {
                let inspected = recovery::inspect(&orm, &target.store).await?;
                if args.confirm.as_deref() != Some(inspected.confirmation().as_str()) {
                    return Err(invalid(
                        "--confirm",
                        "ownership changed or confirmation does not match; run attachments inspect again",
                    ));
                }
                let reason = match args.reason {
                    Some(AttachmentReasonArg::RestorePrimary) => Reason::RestorePrimary,
                    Some(AttachmentReasonArg::RetireClone) => Reason::RetireClone,
                    None => {
                        return Err(invalid(
                            "--reason",
                            "a new force claim requires an operator reason",
                        ));
                    }
                };
                recovery::begin(&orm, &inspected, reason).await?
            };
            claim.run(&orm).await?;
            Ok(format!(
                "Operation: {}\nPhase: completed\nRecorded recovery completed. Inspect current ownership before maintenance; cleanup requires a fresh observation window.",
                claim.id()
            ))
        }
    }
}
