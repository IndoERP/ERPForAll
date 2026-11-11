//! Cluster selection against real servers (`ERP_TEST_DATABASE_URLS`, see the `cluster` module).
//! Needs `DATABASE_URL` like `smoke.rs`; unreachable clusters are simulated locally (a closed port,
//! a listener that hangs up like an ssh tunnel with no server behind it). The last test checks the
//! configured list itself and runs only when `ERP_TEST_DATABASE_URLS` is set (as in `scripts/ci.sh`,
//! which also passes the clusters its up-front probe found unreachable in
//! `ERP_TESTKIT_UNREACHABLE_CLUSTERS`).
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test helpers"
)]

use erp_testkit::tokio_postgres::Config;
use erp_testkit::tokio_postgres::config::Host;
use erp_testkit::tokio_postgres::error::SqlState;
use erp_testkit::{
    CLUSTER_LOG_ENV, Cluster, ClusterProbe, ClusterState, DATABASE_URLS_ENV, MAX_TIMEOUT_LATENESS,
    PROBED_UNREACHABLE_ENV, TIMEOUT_CHECK_ATTEMPTS, TestDb, TestkitError, UnreachableKind,
    UsageThreshold, clusters_from_env, database_url_from_env, error_chain,
};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpSocket};

/// Labels (`host:port/db`, space-separated) that `scripts/ci.sh` found unreachable before the tests.
const UNREACHABLE_ENV: &str = PROBED_UNREACHABLE_ENV;

fn url() -> String {
    database_url_from_env().expect("DATABASE_URL must be set for erp-testkit tests")
}

/// `host:port` of `DATABASE_URL`, as the testkit labels it.
fn host_port() -> String {
    let config: Config = url().parse().expect("parse DATABASE_URL");
    let host = match config.get_hosts().first().expect("a host") {
        Host::Tcp(name) => name.clone(),
        Host::Unix(path) => path.display().to_string(),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    format!("{host}:{port}")
}

/// A local port that accepts connections, reads the startup packet and hangs up: what a client
/// sees through an ssh tunnel whose far end has no server.
async fn hang_up_listener() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut len = [0u8; 4];
            if socket.read_exact(&mut len).await.is_ok() {
                let body = usize::try_from(u32::from_be_bytes(len))
                    .expect("length fits")
                    .saturating_sub(4)
                    .min(1 << 16);
                let mut rest = vec![0u8; body];
                // The client may give up first; either way the socket is closed next.
                if let Err(error) = socket.read_exact(&mut rest).await {
                    eprintln!("hang-up listener: {error}");
                }
            }
            drop(socket);
        }
    });
    port
}

/// A port that refuses connections: an ephemeral port that was bound and released.
fn closed_port() -> u16 {
    let socket = TcpSocket::new_v4().expect("socket");
    socket
        .bind("127.0.0.1:0".parse().expect("address"))
        .expect("bind");
    let port = socket.local_addr().expect("addr").port();
    drop(socket);
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_cluster_is_excluded_and_the_next_one_is_used() {
    let port = closed_port();
    let refused = format!("postgres://nobody@127.0.0.1:{port}/erpforall");
    let db = TestDb::builder()
        .database_urls(format!("{refused} {}", url()))
        .create()
        .await
        .expect("falls over to DATABASE_URL");
    assert_eq!(db.cluster().position(), 2);
    assert_eq!(db.cluster().label(), format!("{}/erpforall", host_port()));
    assert!(
        db.cluster()
            .database_exists(db.name())
            .await
            .expect("lookup"),
        "the database is on the chosen cluster"
    );
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cluster_that_hangs_up_is_excluded_and_the_next_one_is_used() {
    let port = hang_up_listener().await;
    let db = TestDb::builder()
        .database_urls(format!(
            "postgres://nobody@127.0.0.1:{port}/erpforall\n{}",
            url()
        ))
        .create()
        .await
        .expect("falls over to DATABASE_URL");
    assert_eq!(db.cluster().position(), 2);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread")]
async fn all_clusters_unreachable_is_an_error_listing_each() {
    let refused = closed_port();
    let hang_up = hang_up_listener().await;
    let error = TestDb::builder()
        .database_urls(format!(
            "postgres://nobody@127.0.0.1:{refused}/erpforall \
             postgres://nobody@127.0.0.1:{hang_up}/erpforall"
        ))
        .max_usage(UsageThreshold::new(75).expect("75"))
        .create()
        .await
        .expect_err("nothing reachable");
    let TestkitError::NoClusterAvailable { threshold, skipped } = &error else {
        panic!("unexpected error kind: {error:?}");
    };
    assert_eq!(*threshold, 75);
    assert_eq!(skipped.len(), 2);
    assert_eq!(
        skipped[0].cluster,
        format!("cluster 1/2 (127.0.0.1:{refused}/erpforall)")
    );
    assert_eq!(
        skipped[1].cluster,
        format!("cluster 2/2 (127.0.0.1:{hang_up}/erpforall)")
    );
    // The refused message is the OS's own text ("Connection refused (os error 111)" on Linux), so
    // only its stable start is compared; the full message is checked with it below.
    let ClusterState::Excluded {
        reason: refused_reason,
    } = &skipped[0].state
    else {
        panic!("cluster 1 must be excluded: {:?}", skipped[0]);
    };
    assert_eq!(refused_reason.kind(), UnreachableKind::Refused);
    assert!(
        refused_reason.message().starts_with("Connection refused"),
        "{refused_reason}"
    );
    let ClusterState::Excluded {
        reason: closed_reason,
    } = &skipped[1].state
    else {
        panic!("cluster 2 must be excluded: {:?}", skipped[1]);
    };
    assert_eq!(closed_reason.kind(), UnreachableKind::Closed);
    assert_eq!(
        closed_reason.message(),
        "connection closed before the server answered"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "no test database cluster is below 75% of max_connections: \
             cluster 1/2 (127.0.0.1:{refused}/erpforall): excluded, {refused_reason}; \
             cluster 2/2 (127.0.0.1:{hang_up}/erpforall): excluded, connection closed before \
             the server answered"
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_login_stops_selection_instead_of_falling_over() {
    let bad = format!(
        "postgres://erpforall_no_such_role:wrong@{}/erpforall",
        host_port()
    );
    // The second entry is never contacted; a closed port keeps it distinct from the first.
    let port = closed_port();
    let next = format!("postgres://nobody@127.0.0.1:{port}/erpforall");
    let error = TestDb::builder()
        .database_urls(format!("{bad} {next}"))
        .create()
        .await
        .expect_err("a configuration error must not be hidden by the next cluster");
    assert!(
        matches!(error, TestkitError::ClusterConnect { .. }),
        "unexpected error kind: {error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "could not connect to cluster 1/2 ({}/erpforall)",
            host_port()
        )
    );
    assert_eq!(error.unreachable_reason(), None);
    // PostgreSQL answers an unknown role with the same SQLSTATE as a wrong password (28P01), so
    // the role's existence is not revealed; either way it is a login failure, not "unreachable".
    assert_eq!(error.sqlstate(), Some(&SqlState::INVALID_PASSWORD));
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_reports_usage_or_the_typed_unreachable_reason() {
    let reachable = Cluster::parse_list(&url()).expect("DATABASE_URL as a list");
    match reachable[0].probe().await.expect("probe DATABASE_URL") {
        ClusterProbe::Reachable(usage) => {
            assert!(usage.connections >= 1, "the probe itself is connected");
            assert!(usage.max_connections >= usage.connections, "{usage}");
        }
        other => panic!("DATABASE_URL must be reachable: {other:?}"),
    }
    let refused = closed_port();
    let hang_up = hang_up_listener().await;
    let list = Cluster::parse_list(&format!(
        "postgres://nobody@127.0.0.1:{refused}/erpforall \
         postgres://nobody@127.0.0.1:{hang_up}/erpforall"
    ))
    .expect("valid list");
    let ClusterProbe::Unreachable(reason) = list[0].probe().await.expect("probe refused") else {
        panic!("a closed port must be unreachable");
    };
    assert_eq!(reason.kind(), UnreachableKind::Refused);
    let ClusterProbe::Unreachable(reason) = list[1].probe().await.expect("probe hang-up") else {
        panic!("a hang-up must be unreachable");
    };
    assert_eq!(reason.kind(), UnreachableKind::Closed);
    assert_eq!(
        reason.message(),
        "connection closed before the server answered"
    );
}

/// The test cluster raises its statement and lock timeouts on time (TEST.NODE-MACOS): tests that
/// hold a lock and expect the blocked statement to end with SQLSTATE 57014 or 55P03 within a short
/// grace (the node registry's) need it. Fails on a macOS postmaster that launchd starts without
/// `ProcessType = Interactive` (about 145 ms late), with the fix in the message. The check
/// measures up to three times, 250 ms apart, before refusing, so one busy moment of a loaded CI
/// host does not fail it, and every step is bounded (the probe's 15 s, 2 s per sample step), so it
/// cannot hang the suite.
#[tokio::test(flavor = "multi_thread")]
async fn the_test_cluster_raises_its_timeouts_on_time() {
    let cluster = &Cluster::parse_list(&url()).expect("DATABASE_URL as a list")[0];
    let (probe, check) = match cluster.probe_with_timeouts().await {
        Ok(found) => found,
        Err(error) => panic!("{}", error_chain(&error)),
    };
    assert!(matches!(probe, ClusterProbe::Reachable(_)), "{probe:?}");
    let check = check.expect("a reachable cluster is checked");
    assert_eq!(check.statement_samples().len(), 9);
    assert_eq!(check.lock_samples().len(), 9);
    assert!(check.statement_median() <= MAX_TIMEOUT_LATENESS, "{check}");
    assert!(check.lock_median() <= MAX_TIMEOUT_LATENESS, "{check}");
    assert!(
        (1..=TIMEOUT_CHECK_ATTEMPTS).contains(&check.attempts()),
        "{check}"
    );
    // a sample is lateness, never the whole wait: far below the 1 s sleep or backstop it cancels
    for sample in check.statement_samples().iter().chain(check.lock_samples()) {
        assert!(*sample < Duration::from_millis(900), "{check:?}");
    }
}

/// `erp-testkit probe` (what `scripts/ci.sh` runs before the tests): one line per cluster with the
/// typed reason, exit status 3 when some cluster is unreachable, 1 on a login failure.
#[tokio::test(flavor = "multi_thread")]
async fn the_probe_command_reports_every_cluster_and_its_kind() {
    let refused = closed_port();
    let hang_up = hang_up_listener().await;
    let list = format!(
        "postgres://nobody@127.0.0.1:{refused}/erpforall \
         postgres://nobody@127.0.0.1:{hang_up}/erpforall {}",
        url()
    );
    let (status, stdout, stderr) = run_probe(list).await;
    assert_eq!(status, Some(3), "stdout: {stdout}\nstderr: {stderr}");
    let lines: Vec<Vec<&str>> = stdout.lines().map(|l| l.split('\t').collect()).collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    let refused_label = format!("127.0.0.1:{refused}/erpforall");
    assert_eq!(
        lines[0][..4],
        ["unreachable", "1/3", refused_label.as_str(), "refused"]
    );
    assert!(lines[0][4].starts_with("Connection refused"), "{stdout}");
    let hang_up_label = format!("127.0.0.1:{hang_up}/erpforall");
    assert_eq!(
        lines[1],
        [
            "unreachable",
            "2/3",
            hang_up_label.as_str(),
            "closed",
            "connection closed before the server answered"
        ]
    );
    let primary_label = format!("{}/erpforall", host_port());
    assert_eq!(lines[2][..3], ["reachable", "3/3", primary_label.as_str()]);
    assert_eq!(lines[2].len(), 5, "{stdout}");
    assert!(lines[2][3].ends_with("%)"), "{stdout}");
    // a reachable cluster's timeouts are checked, and the line says how late they fire
    assert!(
        lines[2][4].starts_with("statement_timeout ")
            && lines[2][4].contains(" late, lock_timeout "),
        "{stdout}"
    );
    assert!(
        !stdout.contains("nobody"),
        "no user names in the output: {stdout}"
    );

    let bad = format!(
        "postgres://erpforall_no_such_role:wrong@{}/erpforall \
         postgres://nobody@127.0.0.1:{refused}/erpforall",
        host_port()
    );
    let (status, stdout, _) = run_probe(bad).await;
    assert_eq!(status, Some(1), "{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "the next cluster is still probed: {stdout}");
    assert_eq!(
        lines[0],
        format!(
            "error\t1/2\t{primary_label}\tcould not connect to cluster 1/2 ({primary_label}): \
             db error: FATAL: password authentication failed for user \"erpforall_no_such_role\""
        )
    );
    assert!(
        !stdout.contains("wrong"),
        "no password in the output: {stdout}"
    );
    assert!(
        lines[1].starts_with(&format!("unreachable\t2/2\t{refused_label}\trefused\t")),
        "{stdout}"
    );
}

/// Run `erp-testkit probe` with this cluster list (and no `DATABASE_URL`, so the list is all it
/// sees); returns its exit code, stdout and stderr.
async fn run_probe(list: String) -> (Option<i32>, String, String) {
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_erp-testkit"))
            .arg("probe")
            .env(DATABASE_URLS_ENV, list)
            .env_remove("DATABASE_URL")
            .output()
            .expect("run erp-testkit probe");
        (
            output.status.code(),
            String::from_utf8(output.stdout).expect("utf-8 stdout"),
            String::from_utf8(output.stderr).expect("utf-8 stderr"),
        )
    })
    .await
    .expect("probe task")
}

/// A login or configuration
/// failure fails the test. With `ERP_TESTKIT_CLUSTER_LOG` set, each creation must be in the log.
#[tokio::test(flavor = "multi_thread")]
async fn configured_clusters_take_databases_or_are_cleanly_unreachable() {
    if std::env::var_os(DATABASE_URLS_ENV).is_none() {
        eprintln!("skipped: {DATABASE_URLS_ENV} is not set");
        return;
    }
    let clusters = clusters_from_env().expect("valid cluster list");
    // ci.sh lists what its probe found unreachable (and decided about: warning or failure); a
    // cluster unreachable here but not there went down unnoticed and fails the test. Without the
    // variable (a manual run) an unreachable cluster is accepted.
    let reported = std::env::var(UNREACHABLE_ENV).ok();
    let mut reachable = 0;
    for cluster in &clusters {
        match TestDb::builder().cluster(cluster.clone()).create().await {
            Ok(db) => {
                reachable += 1;
                assert_eq!(db.cluster().label(), cluster.label());
                assert_log_has(db.name(), cluster.label());
                db.cleanup().await.expect("cleanup");
            }
            Err(error) => match (error.unreachable_reason(), &reported) {
                (Some(reason), Some(list))
                    if !list.split_whitespace().any(|l| l == cluster.label()) =>
                {
                    panic!(
                        "{cluster} is unreachable ({}: {reason}) but the CI probe did not report \
                         it (reported: {list:?})",
                        reason.kind()
                    )
                }
                (Some(reason), _) => {
                    eprintln!("{cluster} unreachable ({}): {reason}", reason.kind());
                }
                (None, _) => panic!("{cluster}: {}", error_chain(&error)),
            },
        }
    }
    assert!(reachable >= 1, "no configured cluster is reachable");

    let db = TestDb::create()
        .await
        .expect("selection from the environment");
    let position = db.cluster().position();
    assert_eq!(clusters[position - 1].label(), db.cluster().label());
    assert_log_has(db.name(), db.cluster().label());
    db.cleanup().await.expect("cleanup");
}

fn assert_log_has(name: &str, label: &str) {
    let Some(path) = std::env::var_os(CLUSTER_LOG_ENV) else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("read cluster log");
    let line = text
        .lines()
        .find(|line| line.split('\t').nth(1) == Some(name))
        .unwrap_or_else(|| panic!("{name} missing from the cluster log"));
    assert_eq!(line.split('\t').nth(2), Some(label), "{line}");
}
