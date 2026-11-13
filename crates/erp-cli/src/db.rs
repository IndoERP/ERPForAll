//! Native runtime grant publication (`erp db publish-grants --config <server.toml>`) and
//! `erp db build-object` (design): build an
//! exclusion constraint or `WITHOUT OVERLAPS` key on a table above `large_table_rows`, where the
//! install refuses it, in a maintenance window the operator chooses.
//!
//! Connects as the role of `DATABASE_URL` (the owner role), like `erp access`; the registry is
//! composed from the addons linked into this binary (the installed modules of a given database
//! come with, as for `erp access`). The work is [`Orm::build_object`].

use std::sync::Arc;

use erp_orm::{BuildObjectOptions, Orm};
use erp_registry::Registry;

use crate::access::{connection_config, pool, registry};
use crate::args::{BuildObjectArgs, DbAction, VacuumTransientArgs};
use crate::error::{CliError, Result};

/// Run an `erp db` action and return what to print.
///
/// # Errors
///
/// [`CliError::InvalidInput`] for a missing or malformed `DATABASE_URL`;
/// [`CliError::Execution`] when the runtime cannot start, protected configuration is invalid,
/// module recovery is pending or committed grants cannot be verified as the runtime role;
/// [`CliError::ModuleOperation`] when publication finds the lifecycle lock busy; [`CliError::Access`] for the errors of
/// the registry, the ORM and the database (exit code 1 for a refusal such as an unknown model or
/// conflicting rows, 2 for a database fault).
pub fn execute(action: &DbAction) -> Result<String> {
    match action {
        DbAction::Init(args) => crate::human::initialize(args),
        DbAction::PublishGrants(args) => crate::human::publish_grants(args),
        DbAction::RuntimeGrantContract => Ok(runtime_grant_contract()),
        DbAction::BuildObject(args) => {
            execute_with(action, registry()?, connection_config(&args.db.database)?)
        }
        // composed as the server does it for the database in the configuration file
        DbAction::VacuumTransient(args) => crate::human::vacuum_transient(args),
        DbAction::GrantVacuumRole(args) => crate::human::grant_vacuum_role(args),
    }
}

/// As [`execute`], with the registry of the database's addons and the connection configuration
/// (database included) given by the caller: the linked set and `DATABASE_URL` in the binary, a
/// test registry and a test database in tests.
///
/// # Errors
///
/// As [`execute`].
pub fn execute_with(
    action: &DbAction,
    registry: Arc<Registry>,
    config: tokio_postgres::Config,
) -> Result<String> {
    // Construct the runtime only for database actions; the capability query is local.
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::Execution(format!("could not start the async runtime: {e}")))
    };
    match action {
        DbAction::RuntimeGrantContract => Ok(runtime_grant_contract()),
        DbAction::Init(args) => runtime()?.block_on(crate::human::initialize_async(args)),
        DbAction::PublishGrants(args) => {
            runtime()?.block_on(crate::human::publish_grants_async(args))
        }
        DbAction::BuildObject(args) => runtime()?.block_on(build_object(args, registry, config)),
        DbAction::VacuumTransient(args) => {
            runtime()?.block_on(vacuum_transient(args, registry, config))
        }
        DbAction::GrantVacuumRole(args) => {
            runtime()?.block_on(crate::human::grant_vacuum_role_async(args))
        }
    }
}

/// Publisher/startup enforce read-only UI assets, execute-only node features and
/// insert-only event storage and monotone UoM references, and the insert-only audit
/// trail. Keep in sync with scripts/human-release.py.
/// Version 7 narrows attachment content writes and publishes append-only usage accounting.
/// This is a local capability declaration, not a claim about a database's current grants.
/// Version 5 adds the job queue's column-level grants; version 6 adds the
/// insert-only audit trail; version 7 adds attachment accounting and version 8 adds the isolated SELECT-only
/// reader. The number is pinned here, in
/// `scripts/human-release.py`, and the tests that pin it.
/// Version 9 adds SELECT-only attachment ownership state.
/// Version 10 adds snapshot-safe pending attachment guard writes.
/// Version 11 adds SELECT-only peer evidence.
/// Version 12 adds owner-only attachment recovery.
/// Version 13 relabels attachment recovery: owner writes, the runtime role reads progress; grants unchanged.
fn runtime_grant_contract() -> String {
    format!(
        r#"{{"version":{RUNTIME_GRANT_CONTRACT_VERSION},"ui_assets":"read-only","node_features":"execute-only","event_outbox":"insert-only","uom_references":"guarded-monotone","job_queue":"column-grants","audit_trail":"insert-only","attachment_usage":"append-only","audit_reader":"select-only","attachment_owner":"read-only","attachment_pending":"guarded-create","attachment_peer":"read-only","attachment_recovery":"owner-write, progress-read"}}"#
    )
}

/// The contract version this build publishes.: v5 went to (job queue) and v6
/// to (audit trail, includes v5's guarantees); change it here and in
/// `scripts/human-release.py` (`RUNTIME_GRANT_CONTRACTS`) together.
const RUNTIME_GRANT_CONTRACT_VERSION: u32 = 13;

async fn build_object(
    args: &BuildObjectArgs,
    registry: Arc<Registry>,
    config: tokio_postgres::Config,
) -> Result<String> {
    // the authentication tables are required wherever erp-security is linked
    let orm = Orm::builder(registry, pool(config)?)
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .build()?;
    let options = BuildObjectOptions::new()
        .with_lock_timeout(args.lock_timeout)?
        .with_build_timeout(args.build_timeout)?;
    let report = orm
        .build_object(&args.model, &args.object, &options)
        .await?;
    Ok(report.to_string())
}

/// One bounded vacuum invocation per selected model, sequentially, on one connection pool. This is
/// an operator command: the engine call is host-only and no RPC route or WASM import
/// reaches it. A refusal or failure stops the run; the error then carries the lines of the models
/// already done, so nothing a committed batch removed goes unreported.
async fn vacuum_transient(
    args: &VacuumTransientArgs,
    registry: Arc<Registry>,
    config: tokio_postgres::Config,
) -> Result<String> {
    // the test seam: the registry of the given addons and the given connection
    let orm = Orm::builder(registry, pool(config)?)
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .build()?;
    vacuum_with(args, &orm).await
}

/// The command on a ready ORM: the server-composed one in production
/// ([`crate::human::vacuum_transient`]), a test registry's in the library tests.
pub(crate) async fn vacuum_with(args: &VacuumTransientArgs, orm: &Orm) -> Result<String> {
    let registry = Arc::clone(orm.registry());
    let models: Vec<String> = match &args.model {
        Some(model) => {
            // a caller's mistake (exit 1), checked before the engine is reached
            match registry.model(model) {
                None => {
                    return Err(erp_fields::Error::new(
                        erp_fields::ErrorKind::Missing,
                        format!("unknown model '{model}'"),
                    )
                    .into());
                }
                Some(spec) if spec.transient_policy.is_none() => {
                    return Err(erp_fields::Error::new(
                        erp_fields::ErrorKind::Validation,
                        format!(
                            "model '{model}' is not transient: only transient models are vacuumed"
                        ),
                    )
                    .into());
                }
                Some(_) => vec![model.clone()],
            }
        }
        None => registry
            .models()
            .iter()
            .filter(|m| m.transient_policy.is_some())
            .map(|m| m.name.to_owned())
            .collect(),
    };
    let options = args.options()?;
    let mut lines = String::new();
    for model in &models {
        match orm.vacuum_transient(model, options).await {
            Ok(report) => lines.push_str(&format!(
                "{model}: {} (deleted {} rows in {} batches; cycle {} rows in {} batches{}{})\n",
                report.outcome,
                report.deleted_rows,
                report.batches,
                report.cycle_rows,
                report.cycle_batches,
                report
                    .epoch
                    .map(|epoch| format!("; epoch {epoch}"))
                    .unwrap_or_default(),
                if report.aborted_previous_cycle {
                    "; an open cycle of another policy was aborted"
                } else {
                    ""
                },
            )),
            Err(error) => {
                let done = if lines.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nreports of the models already done:\n{}",
                        lines.trim_end()
                    )
                };
                // the kind, the SQLSTATE and the original error (as the source) are kept
                let sqlstate = error.sqlstate().map(str::to_owned);
                return Err(erp_fields::Error::new(
                    error.kind(),
                    format!("{model}: vacuum failed: {}{done}", error.message()),
                )
                .with_sqlstate(sqlstate)
                .with_source(error)
                .into());
            }
        }
    }
    if models.is_empty() {
        lines.push_str("no transient model in the registry: nothing to do\n");
    }
    Ok(lines.trim_end().to_owned())
}
