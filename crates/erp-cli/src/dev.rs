//! Development mode command (`erp dev`).

use std::sync::Arc;

use crate::access::connection_config;
use crate::args::DevArgs;
use crate::error::{CliError, Result};
use crate::module::map_discovery_error;
use erp_modules::AddonIndex;
use erp_orm::node::{NodeIdentity, NodeMembership, NodeOptions, NodeRole};

/// The node name of an `erp dev` process.
const DEV_NODE_NAME: &str = "erp-dev";

/// Run the development environment / dev server.
///
/// Discovers addons from configured paths with dev folder precedence, validates configurations,
/// and prepares the development server. With `-d`, the process registers in that database as a
/// dev node first (design decision 5): a database served by
/// live non-dev nodes is refused, so a development process never joins a production cluster.
/// Until the HTTP server exists the membership is shut down again at once.
///
/// # Errors
///
/// Returns [`CliError::InvalidInput`] if the port is 0 or `DATABASE_URL` is missing.
/// Returns [`CliError::Io`] or [`CliError::Manifest`] if addon discovery fails.
/// Returns [`CliError::Access`] when the dev registration is refused (exit code 1: live non-dev
/// nodes serve the database) or fails (exit code 2: the node tables are missing, a database
/// fault).
/// Returns [`CliError::Execution`] indicating that the HTTP server runtime is scheduled for M3 (erp-server).
pub fn run_dev(args: &DevArgs) -> Result<()> {
    let config = match &args.database {
        Some(database) => Some(connection_config(database)?),
        None => None,
    };
    run(args, config)
}

/// As [`run_dev`], with the connection configuration of `-d` (database included) given by the
/// caller: `DATABASE_URL` in the binary, a test database in tests. `args.database` must be set.
///
/// # Errors
///
/// As [`run_dev`]; [`CliError::InvalidInput`] when `args` names no database.
pub fn run_dev_with(args: &DevArgs, config: tokio_postgres::Config) -> Result<()> {
    if args.database.is_none() {
        return Err(CliError::InvalidInput {
            arg: "database".to_owned(),
            reason: "a connection configuration was given without -d".to_owned(),
        });
    }
    run(args, Some(config))
}

fn run(args: &DevArgs, config: Option<tokio_postgres::Config>) -> Result<()> {
    if args.port == 0 {
        return Err(CliError::InvalidInput {
            arg: "port".to_string(),
            reason: "HTTP port must be greater than 0".to_string(),
        });
    }

    // Build index with dev_mode enabled
    let mut index = AddonIndex::new(true);
    for path in &args.addons_path {
        index.add_dev_path(path);
    }

    let _discovered = index
        .discover_all()
        .map_err(|e| map_discovery_error(e, &args.addons_path))?;

    if let Some(config) = config {
        register_dev(&mut index, config, &args.addons_path)?;
    }

    // Fail loudly: HTTP server engine is scheduled for M3 (erp-server)
    Err(CliError::Execution(
        "Development server HTTP runtime is not yet implemented (scheduled for M3; erp-server)"
            .to_string(),
    ))
}

/// Register this process as a dev node in `database`, then leave again.
fn register_dev(
    index: &mut AddonIndex,
    config: tokio_postgres::Config,
    addons_path: &[std::path::PathBuf],
) -> Result<()> {
    let addons = index
        .digest()
        .map_err(|e| map_discovery_error(e, addons_path))?;
    let host = host_name()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Execution(format!("could not start the async runtime: {e}")))?;
    runtime.block_on(async {
        let options = NodeOptions::default();
        let pool = erp_orm::node::heartbeat_pool(config, &options)?;
        // the digest of a dev node uses the default RPC settings until the server reads its own
        let rpc = erp_rpc::RpcConfig::default();
        let identity = NodeIdentity::builder(
            NodeRole::App,
            DEV_NODE_NAME,
            &host,
            erp_orm::build_identity!(),
            addons,
        )
        .settings(&rpc)?
        .settings(&rpc.limits())?
        .options(options)
        .dev(true)
        .create(&pool)
        .await?;
        let membership = NodeMembership::register(Arc::new(identity), pool).await?;
        membership.shutdown().await?;
        Ok(())
    })
}

/// The host name, for the node row (`gethostname`, through `uname`): printable, 1 to 255 bytes.
fn host_name() -> Result<String> {
    let uname = rustix::system::uname();
    let name = uname.nodename().to_str().map_err(|_| {
        CliError::Execution("could not read the host name: it is not UTF-8".to_owned())
    })?;
    let name = name.trim();
    if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
        return Err(CliError::Execution(format!(
            "the host name {name:?} cannot label a node (1 to 255 bytes without control \
             characters)"
        )));
    }
    Ok(name.to_owned())
}
