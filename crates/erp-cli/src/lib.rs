//! ERPForAll command-line interface library.
//!
//! Provides CLI parsing, scaffolding, module management plumbing, and development runner.

pub mod access;
pub mod addon;
mod addon_life;
pub mod args;
mod attachment_content;
mod attachment_recovery;
mod attachment_schedules;
mod attachments;
pub mod audit;
mod audit_reader;
mod auth_revoke;
pub mod cluster;
pub mod db;
mod db_lifecycle;
pub mod dev;
pub mod error;
mod human;
pub mod identity;
mod live;
pub mod module;
pub mod native_addons;
pub mod node;
mod partition_upkeep;
#[cfg(test)]
mod partition_upkeep_tests;
mod pivot_export;
pub mod scaffold;
mod server_runtime;

pub use args::{
    AccessAction, AccessArgs, AccessCommand, AddonAction, AddonBuildArgs, AddonCommand,
    AddonInstallArgs, AddonPackageArgs, AddonUninstallArgs, AuthAction, AuthCommand,
    BuildObjectArgs, BuildTarget, Cli, ClusterAction, ClusterCommand, Commands, DatabaseName,
    DbAction, DbCommand, DbTargetArgs, DemoArgs, DevArgs, DisableRuleArgs, DumpArgs,
    EmergencyReasonArg, EmergencyRevokeArgs, GrantVacuumRoleArgs, ModuleAction, ModuleCommand,
    ModuleOpArgs, NeutralizeArgs, RestoreArgs, RolloutAction, RolloutCommand, RolloutOpenArgs,
    ScaffoldArgs, VacuumTransientArgs,
};
pub use error::{CliError, Result};
pub use module::{
    DefaultLifecycleExecutor, ExecutionReport, LifecycleRequest, ModuleLifecycleExecutor,
    ModuleOperation, ModuleRun,
};

use clap::Parser;
use std::ffi::OsString;
use std::process::ExitCode;

/// Main entry point for the `erp` CLI tool.
///
/// Parses arguments from the provided iterator, executes the corresponding command,
/// handles errors by writing diagnostics to stderr, and returns the appropriate process exit code.
///
/// # Examples
///
/// ```
/// use erp_cli::run;
/// use std::process::ExitCode;
///
/// let code = run(["erp", "--help"]);
/// assert_eq!(code, ExitCode::SUCCESS);
/// ```
pub fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            if let Err(err) = e.print() {
                eprintln!("Failed to write CLI error: {err}");
            }
            if !e.use_stderr() {
                return ExitCode::SUCCESS;
            }
            // Value validation errors (such as invalid database identifier syntax for -d, or invalid module names)
            // represent argument/input validation failures and exit with code 1, consistent with
            // programmatic validation via DatabaseName and CliError::exit_code().
            // Usage errors (such as missing required arguments like omitted -d, or unknown flags) exit with code 2.
            return match e.kind() {
                clap::error::ErrorKind::ValueValidation | clap::error::ErrorKind::InvalidValue => {
                    ExitCode::from(1)
                }
                _ => ExitCode::from(2),
            };
        }
    };

    match execute(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            err.exit_code()
        }
    }
}

/// Executes a parsed CLI command.
fn execute(cli: &Cli) -> Result<()> {
    match cli.command {
        Commands::Server(ref args) => human::serve(args),
        Commands::Attachments(ref cmd) => {
            println!("{}", attachments::execute(&cmd.action)?);
            Ok(())
        }
        Commands::Scaffold(ref args) => {
            let path = scaffold::generate_scaffold(args)?;
            println!(
                "Successfully scaffolded addon '{}' at {}",
                args.name,
                path.display()
            );
            Ok(())
        }
        Commands::Module(ref cmd)
            if matches!(
                cmd.action,
                ModuleAction::IdentityEnroll(_) | ModuleAction::IdentityActivate(_)
            ) =>
        {
            println!("{}", identity::execute(&cmd.action)?);
            Ok(())
        }
        Commands::Module(ref cmd) => {
            let run = module::run_module_command(&cmd.action)?;
            let op = match cmd.action {
                ModuleAction::Install(_) => "Install",
                ModuleAction::Upgrade(_) => "Upgrade",
                ModuleAction::Uninstall(_) => "Uninstall",
                ModuleAction::IdentityEnroll(_) | ModuleAction::IdentityActivate(_) => {
                    return Err(CliError::Execution(
                        "identity command dispatch invariant failed".into(),
                    ));
                }
            };
            match run.report {
                // `--dry-run`: the plan only, nothing touched the database.
                None => {
                    println!(
                        "{op} plan resolved successfully ({} modules):",
                        run.plan.len()
                    );
                    for (i, name) in run.plan.iter().enumerate() {
                        println!("  {}. {}", i + 1, name.as_str());
                    }
                    Ok(())
                }
                Some(report) => {
                    println!("{op} completed:");
                    print!("{report}");
                    // The operation committed; a post-commit phase that did not finish is an
                    // error exit (5), after the report said what ran.
                    match report.post_commit_error() {
                        Some(error) => Err(error),
                        None => Ok(()),
                    }
                }
            }
        }
        Commands::Addon(ref cmd) => {
            match cmd.action {
                AddonAction::Build(ref args) => {
                    print!("{}", addon::build_wasm(args, &addon::SystemToolchain)?);
                }
                AddonAction::Package(ref args) => print!("{}", addon::package(args)?),
                AddonAction::Install(ref args) => {
                    println!("{}", addon_life::run(addon_life::Operation::Install, args)?)
                }
                AddonAction::Upgrade(ref args) => {
                    println!("{}", addon_life::run(addon_life::Operation::Upgrade, args)?)
                }
                AddonAction::Uninstall(ref args) => {
                    println!("{}", addon_life::uninstall(args)?);
                }
            }
            Ok(())
        }
        Commands::Access(ref cmd) => {
            println!("{}", access::execute(&cmd.action)?);
            Ok(())
        }
        Commands::Db(ref cmd) => {
            println!("{}", db::execute(&cmd.action)?);
            Ok(())
        }
        Commands::Audit(ref cmd) => {
            match cmd.action {
                args::AuditAction::Maintain(ref args) => {
                    println!("{}", human::audit_maintain(args)?);
                }
            }
            Ok(())
        }
        Commands::Auth(ref cmd) => {
            match cmd.action {
                AuthAction::EmergencyRevoke(ref args) => {
                    println!("{}", auth_revoke::emergency_revoke(args)?);
                }
            }
            Ok(())
        }
        Commands::Cluster(ref cmd) => {
            println!("{}", cluster::execute(&cmd.action)?);
            Ok(())
        }
        Commands::Dev(ref args) => {
            dev::run_dev(args)?;
            println!(
                "Dev server initialized on port {} (database: {})",
                args.port,
                args.database.as_ref().map(|d| d.as_str()).unwrap_or("none")
            );
            Ok(())
        }
    }
}

/// Host adapter for declared addon UI assets and transactional publication.
pub mod ui_assets;

mod uom_grants;

mod sql_object_publication;
