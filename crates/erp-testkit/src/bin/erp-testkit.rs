//! `erp-testkit sweep`: drop leftover `erpforall_test_*` databases.
//! `erp-testkit probe`: report whether each configured test cluster is reachable.

use std::convert::Infallible;
use std::fmt;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use clap::{Parser, Subcommand};
use erp_testkit::{
    Cluster, ClusterProbe, RUN_ID_ENV, Result as TestkitResult, SweepOptions, TimeoutCheck,
    clusters_from_env, error_chain, sweep,
};

/// `probe` exit status when every cluster answered but at least one is unreachable.
const EXIT_SOME_UNREACHABLE: u8 = 3;

/// A connection string that never shows up in `Debug` output (it can carry a password).
#[derive(Clone)]
struct DatabaseUrl(String);

impl fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DatabaseUrl(<redacted>)")
    }
}

impl FromStr for DatabaseUrl {
    type Err = Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

#[derive(Debug, Parser)]
#[command(name = "erp-testkit", about = "ERPForAll test database utilities")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Drop test databases of a run, older than a cutoff, or left untagged by a crash.
    Sweep {
        /// Server to sweep; the database in the URL is used for admin statements.
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: DatabaseUrl,
        /// Drop every test database tagged with this run id, whatever its age.
        #[arg(long, env = RUN_ID_ENV)]
        run_id: Option<String>,
        /// Drop tagged test databases at least this many hours old.
        #[arg(long, default_value_t = 24)]
        older_than_hours: u64,
        /// Only list what would be dropped.
        #[arg(long)]
        dry_run: bool,
        /// Succeed without sweeping when the server is unreachable (connection refused, timed
        /// out, closed before answering, starting up): for a secondary test cluster that is
        /// stopped during the benchmark window. Any other error still fails.
        #[arg(long)]
        if_reachable: bool,
    },
    /// Probe every cluster of ERP_TEST_DATABASE_URLS (else DATABASE_URL), in order, and print one
    /// tab-separated line each: `reachable <n>/<count> <host:port/db> <usage> <timeouts>` or
    /// `unreachable <n>/<count> <host:port/db> <kind> <message>`. A reachable cluster is also
    /// checked to raise `statement_timeout` and `lock_timeout` on time
    /// (`Cluster::probe_with_timeouts`); one that raises them late is an `error` line. Exit status 0 when all are reachable, 3 when some are
    /// unreachable, 1 on any other error (wrong password, bad list, late timeouts; the remaining
    /// clusters are still probed and printed).
    Probe,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Sweep {
            database_url,
            run_id,
            older_than_hours,
            dry_run,
            if_reachable,
        } => {
            let options = SweepOptions {
                max_age: Duration::from_secs(older_than_hours.saturating_mul(3600)),
                run_id: run_id.filter(|id| !id.is_empty()),
                dry_run,
            };
            match sweep(&database_url.0, &options).await {
                Ok(report) => {
                    let verb = if dry_run { "would drop" } else { "dropped" };
                    for (name, reason) in &report.dropped {
                        println!("{verb} {name} ({reason:?})");
                    }
                    for (name, error) in &report.failed {
                        eprintln!("FAILED {name}: {error}");
                    }
                    println!(
                        "erp-testkit sweep: {} {verb}, {} kept, {} failed",
                        report.dropped.len(),
                        report.kept.len(),
                        report.failed.len()
                    );
                    if report.is_clean() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(error) => match error.unreachable_reason() {
                    Some(reason) if if_reachable => {
                        println!(
                            "erp-testkit sweep: skipped, server unreachable ({}): {reason}",
                            reason.kind()
                        );
                        ExitCode::SUCCESS
                    }
                    _ => {
                        eprintln!("erp-testkit sweep: {}", error_chain(&error));
                        ExitCode::FAILURE
                    }
                },
            }
        }
        Command::Probe => match clusters_from_env() {
            Ok(clusters) => probe_all(&clusters).await,
            Err(error) => {
                eprintln!("erp-testkit probe: {}", error_chain(&error));
                ExitCode::FAILURE
            }
        },
    }
}

/// One output line of `probe` for a cluster (tabs and newlines in messages become spaces, so each
/// cluster stays one line of fixed fields).
fn probe_line(
    cluster: &Cluster,
    count: usize,
    result: &TestkitResult<(ClusterProbe, Option<TimeoutCheck>)>,
) -> String {
    let place = format!("{}/{count}\t{}", cluster.position(), cluster.label());
    let flat = |text: &str| text.replace(['\t', '\n', '\r'], " ");
    match result {
        Ok((ClusterProbe::Reachable(usage), Some(timeouts))) => {
            format!("reachable\t{place}\t{usage}\t{timeouts}")
        }
        // `probe_one` checks the timeouts of every reachable cluster
        Ok((ClusterProbe::Reachable(_), None)) => {
            format!("error\t{place}\treachable, but its timeouts were not checked")
        }
        Ok((ClusterProbe::Unreachable(reason), _)) => format!(
            "unreachable\t{place}\t{}\t{}",
            reason.kind(),
            flat(reason.message())
        ),
        Err(error) => format!("error\t{place}\t{}", flat(&error_chain(error))),
        // ClusterProbe is #[non_exhaustive]: a new outcome must be reported, never mistaken for
        // reachable.
        Ok((other, _)) => format!("error\t{place}\tunknown probe outcome {other:?}"),
    }
}

/// Probe one cluster and, when it is reachable, check on the probe's connection that it raises
/// statement and lock timeouts on time.
async fn probe_one(cluster: &Cluster) -> TestkitResult<(ClusterProbe, Option<TimeoutCheck>)> {
    cluster.probe_with_timeouts().await
}

async fn probe_all(clusters: &[Cluster]) -> ExitCode {
    let mut unreachable = 0usize;
    let mut failed = 0usize;
    for cluster in clusters {
        let result = probe_one(cluster).await;
        let line = probe_line(cluster, clusters.len(), &result);
        match &result {
            Ok((ClusterProbe::Reachable(_), Some(_))) => println!("{line}"),
            Ok((ClusterProbe::Unreachable(_), _)) => {
                unreachable += 1;
                println!("{line}");
            }
            Err(_) | Ok(_) => {
                failed += 1;
                println!("{line}");
            }
        }
    }
    if failed > 0 {
        ExitCode::FAILURE
    } else if unreachable > 0 {
        ExitCode::from(EXIT_SOME_UNREACHABLE)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_the_url() {
        let cli = Cli::parse_from([
            "erp-testkit",
            "sweep",
            "--database-url",
            "postgres://u:hunter2@127.0.0.1:5540/erpforall",
        ]);
        let text = format!("{cli:?}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn if_reachable_is_off_unless_given() {
        let url = "postgres://u@127.0.0.1:5542/erpforall";
        for (args, expected) in [
            (vec!["erp-testkit", "sweep", "--database-url", url], false),
            (
                vec![
                    "erp-testkit",
                    "sweep",
                    "--database-url",
                    url,
                    "--if-reachable",
                ],
                true,
            ),
        ] {
            let Command::Sweep { if_reachable, .. } = Cli::parse_from(args).command else {
                panic!("sweep must parse as Command::Sweep");
            };
            assert_eq!(if_reachable, expected);
        }
    }

    #[test]
    fn probe_takes_no_arguments() {
        assert!(matches!(
            Cli::parse_from(["erp-testkit", "probe"]).command,
            Command::Probe
        ));
        let error = Cli::try_parse_from(["erp-testkit", "probe", "--database-url", "x"])
            .expect_err("probe reads the environment only");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
