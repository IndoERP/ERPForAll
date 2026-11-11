//! `erp cluster` (SCALE.NODES,; design, §7.5): the
//! nodes registered in a database and its rollout window.
//!
//! - `erp cluster nodes -d <db>`: every `erp_node` row, oldest registration first.
//! - `erp cluster rollout open -d <db> --to <digest> [--from <digest>] --for <duration>`.
//! - `erp cluster rollout close -d <db>` and `erp cluster rollout status -d <db>`.
//!
//! Connects as the role of `DATABASE_URL` (the owner role: the heartbeat role may read the
//! window, never open one), the database from `-d`. Malformed digests and durations are refused
//! by the argument parser, before any connection.

mod login_selection;

use erp_orm::node::{NodeRow, Rollout, list_nodes};

use crate::access::{connection_config, pool};
use crate::args::{AccessArgs, ClusterAction, RolloutAction, RolloutOpenArgs};
use crate::error::{CliError, Result};

/// Run an `erp cluster` action and return what to print.
///
/// # Errors
///
/// [`CliError::InvalidInput`] for a missing or malformed `DATABASE_URL`;
/// [`CliError::Execution`] when the runtime cannot start; [`CliError::Access`] for the node
/// registry's errors (exit code 1 for a refused window, 2 for a missing table or a database
/// fault).
pub fn execute(action: &ClusterAction) -> Result<String> {
    let config = connection_config(&database(action).database)?;
    execute_with(action, config)
}

/// As [`execute`], with the connection configuration (database included) given by the caller.
///
/// # Errors
///
/// As [`execute`].
pub fn execute_with(action: &ClusterAction, config: tokio_postgres::Config) -> Result<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Execution(format!("could not start the async runtime: {e}")))?;
    runtime.block_on(run(action, config))
}

fn database(action: &ClusterAction) -> &AccessArgs {
    match action {
        ClusterAction::Nodes(db) => db,
        ClusterAction::LoginSelection(cmd) => login_selection::database(&cmd.action),
        ClusterAction::Rollout(cmd) => match &cmd.action {
            RolloutAction::Open(args) => &args.db,
            RolloutAction::Close(db) | RolloutAction::Status(db) => db,
        },
    }
}

async fn run(action: &ClusterAction, config: tokio_postgres::Config) -> Result<String> {
    let name = database(action).database.as_str().to_owned();
    let pool = pool(config)?;
    match action {
        ClusterAction::Nodes(_) => Ok(nodes(&name, &list_nodes(&pool).await?)),
        ClusterAction::LoginSelection(cmd) => login_selection::run(&name, &pool, &cmd.action).await,
        ClusterAction::Rollout(cmd) => match &cmd.action {
            RolloutAction::Open(args) => open(&name, &pool, args).await,
            RolloutAction::Close(_) => Ok(match Rollout::close(&pool).await? {
                Some(w) => format!(
                    "rollout window closed in database {name}: from {} to {}",
                    w.from(),
                    w.to()
                ),
                None => format!("no open rollout window in database {name}"),
            }),
            RolloutAction::Status(_) => Ok(match Rollout::current(&pool).await? {
                Some(w) if w.is_active() => describe(&name, &w),
                Some(w) => format!(
                    "the rollout window from {} to {} in database {name} ended at {} (not \
                     closed); two versions may no longer become ready side by side",
                    w.from(),
                    w.to(),
                    w.until()
                ),
                None => format!("no open rollout window in database {name}"),
            }),
        },
    }
}

async fn open(
    name: &str,
    pool: &deadpool_postgres::Pool,
    args: &RolloutOpenArgs,
) -> Result<String> {
    let opened_by = opened_by();
    let window = Rollout::open(pool, args.to, args.from, args.duration, Some(&opened_by)).await?;
    Ok(describe(name, &window))
}

/// Who opened a window: the command and, when the environment names one, the user.
fn opened_by() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|u| !u.is_empty() && u.len() <= 64);
    match user {
        Some(user) => format!("erp cluster ({user})"),
        None => "erp cluster".to_owned(),
    }
}

fn describe(name: &str, w: &Rollout) -> String {
    format!(
        "rollout window open in database {name}: from {} to {} until {} (opened by {} at {})",
        w.from(),
        w.to(),
        w.until(),
        w.opened_by(),
        w.opened_at()
    )
}

/// One line per node, under a header.
fn nodes(name: &str, rows: &[NodeRow]) -> String {
    if rows.is_empty() {
        return format!("no node is registered in database {name}");
    }
    let mut out = String::from("NAME\tROLE\tSTATE\tFRESH\tHOST\tPID\tBUILD\tDIGEST\tEXPIRES\tID");
    for r in rows {
        let state = match r.unready_reason() {
            Some(reason) => format!("{} ({reason})", r.state()),
            None => r.state().to_string(),
        };
        let dev = if r.is_dev() { " (dev)" } else { "" };
        out.push_str(&format!(
            "\n{}{dev}\t{}\t{state}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            r.name(),
            r.role(),
            if r.is_fresh() { "yes" } else { "no" },
            r.host(),
            r.pid(),
            r.build_label(),
            r.digest(),
            r.expires_at(),
            r.id()
        ));
    }
    out
}
