//! Live execution of `erp module install | upgrade | uninstall`: the operation runs through
//! [`erp_modules::ModuleSystem`] on the database named by `-d`, as the superuser, in the
//! module system's own transaction.
//!
//! The connection credentials come from `DATABASE_URL` only (never from the command line; see
//! ). The addons are discovered from `--addons-path`; the `erp` binary
//! composes the same linked native addons as the server, with their build-generated per-addon
//! provenance. Additional data-only addons remain discoverable through the configured paths.

use std::sync::Arc;

use erp_modules::system::SchemaStepOutcome;
use erp_modules::wasm_lifecycle::{HostLoader, WasmLifecycle, WasmPackages};
use erp_modules::{AddonIndex, ModuleSystem, OperationOutcome, PostCommitFailure, PostCommitStop};
use erp_orm::{Context, SUPERUSER_ID, Signal};
use erp_wasm_host::{Limits, WasmHost};

use crate::access::{connection_config, pool_with_size};
use crate::error::{CliError, Result};
use crate::module::{ExecutionReport, LifecycleRequest, ModuleLifecycleExecutor, ModuleOperation};

/// Connections the module system may use at once: the operation's transaction, its lock
/// connection, and the post-commit phase.
const POOL_SIZE: usize = 4;

/// Default lifecycle executor for live execution: runs the operation through
/// [`erp_modules::ModuleSystem`].
///
/// Needs `DATABASE_URL` in the environment and a database initialized by the module system.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultLifecycleExecutor;

impl ModuleLifecycleExecutor for DefaultLifecycleExecutor {
    fn execute(
        &self,
        operation: ModuleOperation,
        request: &LifecycleRequest<'_>,
    ) -> Result<ExecutionReport> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::Execution(format!("could not start the async runtime: {e}")))?;
        runtime.block_on(run(operation, request))
    }
}

fn operation_error(error: erp_modules::Error) -> CliError {
    CliError::ModuleOperation(Box::new(error))
}

async fn run(
    operation: ModuleOperation,
    request: &LifecycleRequest<'_>,
) -> Result<ExecutionReport> {
    let pool = pool_with_size(connection_config(request.database)?, POOL_SIZE)?;
    // Observe before source discovery/load; every live CLI operation participates in the same
    // durable registry fence as server and package operations.
    let baseline = {
        let client = pool.get().await.map_err(|error| {
            erp_fields::Error::db("could not acquire module baseline connection").with_source(error)
        })?;
        let initialized: bool = client
            .query_one(
                "SELECT to_regclass('public.ir_module_module') IS NOT NULL",
                &[],
            )
            .await
            .map_err(|error| {
                erp_fields::Error::db("could not inspect module initialization").with_source(error)
            })?
            .try_get(0)
            .map_err(|error| {
                erp_fields::Error::db("invalid module initialization state").with_source(error)
            })?;
        if !initialized {
            return Err(operation_error(
                erp_modules::Error::DatabaseNotInitialized {
                    database: request.database.as_str().to_owned(),
                },
            ));
        }
        Signal::read(&client).await?
    };
    let mut index = AddonIndex::new(true);
    for path in request.addons_path {
        index.add_dev_path(crate::identity::configured_addon_path(path)?);
    }
    // The WebAssembly packages stored in the database are loaded as the server and `erp addon`
    // load them: an installed package addon is then a known module here (its source is never on
    // the addon path), so an upgrade of a module it depends on includes it instead of refusing it
    // as unavailable.
    let host = WasmHost::new()
        .map_err(|e| CliError::Execution(format!("could not start the WebAssembly host: {e}")))?;
    let packages = WasmPackages::new(Arc::new(HostLoader::new(Arc::new(host), Limits::default())));
    let builder = ModuleSystem::builder(pool, index)
        .code([
            &erp_addon_base::ADDON,
            &erp_addon_uom::ADDON,
            &erp_addon_mail::ADDON,
            &erp_addon_product::ADDON,
            &erp_addon_web::ADDON,
            &erp_addon_acceptance_notes::ADDON,
            &erp_addon_theme_office_blue::ADDON,
        ])
        .view_types(erp_addon_web::view_type_contributions())
        // Native lifecycle operations must retain and validate authentication's framework DDL
        // at this composition site, just as the serving node does.
        .require_core_tables(erp_security::auth::core_tables())
        .native_artifact_identities(crate::identity::native_artifact_identities()?)
        .package_runtime(packages.clone())
        .signal(Signal::new(baseline).with_refresh(crate::node::OwnerRefresh))
        .lifecycle_hook(Arc::new(crate::ui_assets::AssetLifecycle::new(
            crate::ui_assets::host_options()?,
        )));
    let builder = match request.identity_previous {
        Some(previous) => match crate::identity::previous_release_identities(previous)? {
            crate::identity::PreviousRelease::Verified(hashes) => builder.identity_previous(hashes),
            crate::identity::PreviousRelease::Legacy => builder.identity_previous_enrolled(),
        },
        None => builder,
    };
    let system = Arc::new(builder.build().map_err(operation_error)?);
    // A load of the stored packages and the installed set first: it refuses an uninitialized
    // database and finishes operations a crashed run left pending; its ORM is the
    // caller of the operation.
    let loaded = WasmLifecycle::new(Arc::clone(&system), packages)
        .load()
        .await
        .map_err(operation_error)?;
    let caller = loaded.orm.env(SUPERUSER_ID, Context::new());
    let outcome = match operation {
        ModuleOperation::Install => {
            system
                .button_immediate_install(&caller, request.requested)
                .await
        }
        ModuleOperation::Upgrade => {
            if request.restore_generated_restrictions {
                system
                    .button_immediate_upgrade_restoring(&caller, request.requested)
                    .await
            } else {
                system
                    .button_immediate_upgrade(&caller, request.requested)
                    .await
            }
        }
        ModuleOperation::Uninstall => {
            system
                .button_immediate_uninstall(&caller, request.requested)
                .await
        }
    }
    .map_err(operation_error)?;
    Ok(report_of(&outcome))
}

fn report_of(outcome: &OperationOutcome) -> ExecutionReport {
    ExecutionReport {
        installed: outcome.installed.clone(),
        upgraded: outcome.upgraded.clone(),
        uninstalled: outcome.uninstalled.clone(),
        unavailable: outcome.unavailable.clone(),
        restored_restrictions: outcome
            .restored_restrictions
            .iter()
            .map(|r| format!("{}: {}", r.model, r.what))
            .collect(),
        post_commit_done: outcome.post_commit.done.len(),
        post_commit_failed: outcome
            .post_commit
            .failed
            .iter()
            .map(describe_failure)
            .collect(),
        post_commit_stopped: outcome.post_commit.stopped.as_ref().map(describe_stop),
        schema_nothing_left: outcome
            .post_commit
            .schema
            .iter()
            .filter(|done| done.outcome == SchemaStepOutcome::NothingLeft)
            .map(|done| format!("{}/{}", done.step.module, done.step.key))
            .collect(),
        schema_kept_comments: outcome
            .post_commit
            .schema
            .iter()
            .flat_map(|done| done.report.kept_comments())
            .map(|kept| {
                format!(
                    "{}.{}: {}",
                    kept.table(),
                    kept.column(),
                    kept.comment().unwrap_or("(none)")
                )
            })
            .collect(),
    }
}

fn describe_failure(failure: &PostCommitFailure) -> String {
    let module = &failure.step.module;
    match &failure.sqlstate {
        Some(sqlstate) => format!(
            "{module}/{}: {} (SQLSTATE {sqlstate})",
            failure.step.key, failure.error
        ),
        None => format!("{module}/{}: {}", failure.step.key, failure.error),
    }
}

fn describe_stop(stop: &PostCommitStop) -> String {
    match stop {
        PostCommitStop::Busy {
            holder_pid: Some(pid),
        } => format!(
            "another post-commit phase (server process {pid}) holds the lock and runs the \
             pending steps"
        ),
        PostCommitStop::Busy { holder_pid: None } => {
            "another post-commit phase holds the lock and runs the pending steps".to_owned()
        }
        PostCommitStop::Failed { message, sqlstate } => match sqlstate {
            Some(sqlstate) => format!("{message} (SQLSTATE {sqlstate})"),
            None => message.clone(),
        },
        PostCommitStop::PassLimit { passes } => format!(
            "stopped after {passes} passes over the queue because steps kept being scheduled; \
             the rest stay pending, run the phase again"
        ),
        // `PostCommitStop` is `#[non_exhaustive]`: say so rather than hide a new reason.
        other => format!("{other:?}"),
    }
}
