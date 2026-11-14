//! `erp addon install | upgrade | uninstall`: the operator's entry point for
//! WebAssembly package addons.
//!
//! The operator names a `.erpaddon` file, the database, the server's deployment configuration and
//! what they approve. Nothing is approved by default: the capabilities the package requests are
//! listed with `--approve` (or `none` when it requests none) and the digest of its client code is
//! acknowledged with `--ack-client-digest`; a package that asks for more than was approved, or
//! ships client code that was not acknowledged, is refused before anything is written.
//!
//! The operation runs through [`crate::node::Node`], the composition the server uses, as the
//! System principal in the module system's own transaction. The module system bumps the registry
//! generation in that transaction, which is what makes running servers rebuild their registry
//! (and load the package) without a restart. Afterwards the tables the package created are
//! granted to the server's runtime role, as the browser installation does.
//!
//! Prints one JSON line on success. Design; operator guide,
//! "Install a WebAssembly addon".

use std::path::Path;

use erp_modules::OperationOutcome;
use erp_modules::manifest::ModuleName;
use erp_modules::package::{AdminConsent, ConsentedPackage, ErpAddonPackage};
use erp_orm::{Context, SUPERUSER_ID, Signal};
use erp_server::config::ServerConfig;

use crate::access::pool_with_size;
use crate::args::{AddonInstallArgs, AddonUninstallArgs, DatabaseName};
use crate::error::{CliError, Result};
use crate::node::Node;

/// The approval word for a package that requests no capability.
const NONE: &str = "none";

/// What `install` and `upgrade` do with the package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Install,
    Upgrade,
}

impl Operation {
    fn word(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
        }
    }
}

fn module_error(error: erp_modules::Error) -> CliError {
    CliError::ModuleOperation(Box::new(error))
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Execution(format!("could not start the async runtime: {e}")))
}

/// The server configuration, which must name the database of `-d`: a package never goes to a
/// database other than the one the operator typed.
fn configuration(path: &Path, database: &DatabaseName) -> Result<ServerConfig> {
    let config = ServerConfig::load(path).map_err(|e| CliError::Execution(e.to_string()))?;
    if config.database() != database.as_str() {
        return Err(CliError::InvalidInput {
            arg: "--database".to_owned(),
            reason: format!(
                "'{}' differs from the database '{}' of the configuration",
                database,
                config.database()
            ),
        });
    }
    Ok(config)
}

/// The administrator's consent as the command line states it.
fn consent_of(
    package: &ErpAddonPackage,
    approve: &[String],
    ack: Option<&str>,
) -> Result<AdminConsent> {
    let mut consent = AdminConsent::new(package.name().clone());
    // an empty element (`--approve ,` or `--approve ''`) approves nothing by accident: refuse it
    if let Some(position) = approve.iter().position(|a| a.trim().is_empty()) {
        return Err(CliError::InvalidInput {
            arg: "--approve".to_owned(),
            reason: format!(
                "element {} is empty: list capabilities, or write `none` to approve nothing",
                position + 1
            ),
        });
    }
    // only the lowercase word is the keyword; `NONE` would otherwise be approved as a capability
    if let Some(word) = approve
        .iter()
        .find(|a| a.eq_ignore_ascii_case(NONE) && a.as_str() != NONE)
    {
        return Err(CliError::InvalidInput {
            arg: "--approve".to_owned(),
            reason: format!("'{word}': the keyword is the lowercase word `none`"),
        });
    }
    if approve.iter().any(|a| a == NONE) {
        if approve.len() != 1 {
            return Err(CliError::InvalidInput {
                arg: "--approve".to_owned(),
                reason: "'none' cannot be combined with capabilities".to_owned(),
            });
        }
    } else {
        for capability in approve {
            consent.approve_capability(capability.trim());
        }
    }
    if let Some(digest) = ack {
        consent.set_client_dist_digest_ack(digest.trim());
    }
    Ok(consent)
}

/// A node over the configured database and the caller of its operations: the System principal on
/// the ORM of the installed set, loaded first (as `erp module` does), which also refuses a database
/// that is not initialized and finishes operations a crash left pending.
async fn open(config: &ServerConfig) -> Result<(Node, erp_orm::Env)> {
    let pool = pool_with_size(config.owner_connection(), 1)?;
    let client = pool
        .get()
        .await
        .map_err(|e| CliError::Execution(format!("could not connect to the database: {e}")))?;
    let baseline = Signal::read(&client).await?;
    drop(client);
    let node = Node::open_configured(config, Some(baseline))?;
    let loaded = node.load().await.map_err(module_error)?;
    let caller = loaded.orm.env(SUPERUSER_ID, Context::new());
    Ok((node, caller))
}

/// Grant the runtime role the tables the operation created, in a transaction of its own after the
/// operation committed (the same step the browser installation takes).
///
/// The operation is already committed here and running servers have seen the registry change; they
/// stay in maintenance until the grants exist. A failure therefore says so and names the command
/// that finishes the job, and a retry of the install is not what to do.
async fn publish_grants(
    outcome: &OperationOutcome,
    config: &ServerConfig,
    config_path: &Path,
) -> Result<()> {
    let published = async {
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await?;
        crate::human::grant_runtime(&mut tx, config).await?;
        tx.commit().await?;
        crate::audit_reader::verify_configured(config).await?;
        Ok::<(), CliError>(())
    }
    .await;
    published.map_err(|e| grants_failed(config_path, &e))
}

/// The error for runtime grants that could not be published after the operation committed.
fn grants_failed(config_path: &Path, cause: &dyn std::fmt::Display) -> CliError {
    CliError::Execution(format!(
        "the operation COMMITTED, but publishing the runtime grants failed: {cause}. Running \
         servers stay in maintenance until the grants exist: run `erp db publish-grants --config {}` \
         once the cause is fixed. Do not repeat the operation",
        config_path.display()
    ))
}

fn names(list: &[ModuleName]) -> Vec<&str> {
    list.iter().map(ModuleName::as_str).collect()
}

fn report(
    operation: &str,
    addon: &str,
    version: Option<String>,
    sha256: Option<&str>,
    database: &DatabaseName,
    outcome: &OperationOutcome,
    (rpc_methods, extended_models): (&[String], &[String]),
) -> String {
    serde_json::json!({
        "operation": operation,
        "addon": addon,
        "version": version,
        "archive_sha256": sha256,
        "database": database.as_str(),
        "installed": names(&outcome.installed),
        "upgraded": names(&outcome.upgraded),
        "uninstalled": names(&outcome.uninstalled),
        "rpc_methods": rpc_methods,
        "extended_models": extended_models,
        "runtime_grants": "published",
    })
    .to_string()
}

/// The operation committed; a post-commit phase that did not finish is an error exit (5) after the
/// grants were published, as for `erp module`.
fn check_post_commit(outcome: &OperationOutcome) -> Result<()> {
    if outcome.post_commit.is_clean() {
        Ok(())
    } else {
        Err(CliError::PostCommitIncomplete {
            failed: outcome.post_commit.failed.len(),
            stopped: outcome
                .post_commit
                .stopped
                .as_ref()
                .map(|s| format!("{s:?}")),
        })
    }
}

/// `erp addon install` and `erp addon upgrade`.
///
/// # Errors
/// [`CliError::InvalidInput`] for a database that is not the configured one or an unusable
/// approval; [`CliError::ModuleOperation`] for a package that is unreadable, not consented, or
/// refused by the lifecycle (exit 1), and for the module system's own errors.
pub(crate) fn run(operation: Operation, args: &AddonInstallArgs) -> Result<String> {
    runtime()?.block_on(async {
        let config = configuration(&args.config, &args.database)?;
        let package = ErpAddonPackage::open(&args.file).map_err(module_error)?;
        let consent = consent_of(&package, &args.approve, args.ack_client_digest.as_deref())?;
        let package: ConsentedPackage = package.consent(&consent).map_err(module_error)?;
        let (name, version, sha256) = (
            package.name().clone(),
            package.version().to_string(),
            package.archive_sha256().to_owned(),
        );
        let (node, caller) = open(&config).await?;
        let outcome = match operation {
            Operation::Install => node.lifecycle().install(&caller, package).await,
            Operation::Upgrade => node.lifecycle().upgrade(&caller, package).await,
        }
        .map_err(module_error)?;
        publish_grants(&outcome, &config, &args.config).await?;
        check_post_commit(&outcome)?;
        // the client-callable methods the package exposes (opt-in in the source): the consent
        // an administrator gave covers capabilities, so this says what else the install opens
        let rpc_methods = node
            .lifecycle()
            .exposed_methods(&name)
            .map_err(module_error)?;
        let extended = node
            .lifecycle()
            .extended_models(&name)
            .map_err(module_error)?;
        Ok(report(
            operation.word(),
            name.as_str(),
            Some(version),
            Some(&sha256),
            &args.database,
            &outcome,
            (&rpc_methods, &extended),
        ))
    })
}

/// `erp addon uninstall`.
///
/// # Errors
/// As [`run`]; a package addon other installed addons depend on is refused with the dependents
/// named.
pub(crate) fn uninstall(args: &AddonUninstallArgs) -> Result<String> {
    runtime()?.block_on(async {
        let config = configuration(&args.config, &args.database)?;
        let (node, caller) = open(&config).await?;
        let outcome = node
            .lifecycle()
            .uninstall(&caller, &args.name)
            .await
            .map_err(module_error)?;
        publish_grants(&outcome, &config, &args.config).await?;
        check_post_commit(&outcome)?;
        Ok(report(
            "uninstall",
            args.name.as_str(),
            None,
            None,
            &args.database,
            &outcome,
            (&[], &[]),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure after a committed operation says it committed and names the recovery command
    /// with the configuration file the operator used (review r1 M2).
    #[test]
    fn a_grant_failure_says_the_operation_committed_and_names_the_recovery() {
        let error = grants_failed(Path::new("/etc/erp/server.toml"), &"role check failed");
        let text = error.to_string();
        assert!(text.contains("COMMITTED"), "{text}");
        assert!(
            text.contains("erp db publish-grants --config /etc/erp/server.toml"),
            "{text}"
        );
        assert!(text.contains("role check failed"), "{text}");
        assert!(text.contains("Do not repeat"), "{text}");
    }
}
