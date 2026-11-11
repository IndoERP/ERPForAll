//! `erp cluster` and `erp dev -d` (SCALE.NODES; spec).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use erp_cli::args::{Cli, ClusterAction, Commands, DevArgs, RolloutAction};
use erp_cli::cluster::execute_with;
use erp_cli::dev::run_dev_with;
use erp_cli::{CliError, DatabaseName, run};
use erp_orm::node::{
    AddonContent, AddonsDigest, BuildIdentity, NodeIdentity, NodeMembership, NodeRole, NodeState,
    Readiness, list_nodes,
};
use erp_test_models::base_users;
use erp_testkit::Harness;

fn parse(args: &[&str]) -> Result<ClusterAction, clap::Error> {
    let mut all = vec!["erp", "cluster"];
    all.extend_from_slice(args);
    let Commands::Cluster(cmd) = Cli::try_parse_from(all)?.command else {
        panic!("expected the cluster command");
    };
    Ok(cmd.action)
}

async fn setup() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    let report = h.orm().sync_schema().await.expect("schema sync");
    assert!(report.is_empty(), "{report:?}");
    h
}

async fn node(h: &Harness, name: &str, host: &str) -> NodeMembership {
    let build = BuildIdentity::__from_build(
        "erp-test-node",
        "1.0.0",
        false,
        Some("0123456789abcdef0123456789abcdef01234567"),
        Some("0"),
    );
    let addons = AddonsDigest::builder()
        .addon("base", "1.0.0", AddonContent::Embedded)
        .unwrap()
        .build()
        .unwrap();
    let identity = NodeIdentity::builder(NodeRole::App, name, host, build, addons)
        .create(h.db().pool())
        .await
        .unwrap();
    let pool = erp_orm::pool::create_pool(h.db().config(), 1).unwrap();
    // warm, as in erp-test-models/tests/node.rs: a cold connect is not part of the bound
    drop(pool.get().await.unwrap());
    NodeMembership::register(Arc::new(identity), pool)
        .await
        .unwrap()
}

async fn cluster(h: &Harness, args: &[&str]) -> Result<String, CliError> {
    let action = parse(args).unwrap();
    let config = h.db().config();
    tokio::task::spawn_blocking(move || execute_with(&action, config))
        .await
        .unwrap()
}

#[tokio::test]
async fn erp_cluster_lists_nodes_and_drives_the_rollout_window() {
    // malformed input is refused before any connection (no DATABASE_URL is needed for it)
    let bad_digest = parse(&["rollout", "open", "-d", "db", "--to", "zz", "--for", "30m"]);
    assert!(
        bad_digest
            .unwrap_err()
            .to_string()
            .contains("is not a digest")
    );
    let hex = "ab".repeat(32);
    for bad in ["30", "m", "30x", "5d", "1.5h", ""] {
        let err = parse(&["rollout", "open", "-d", "db", "--to", &hex, "--for", bad]).unwrap_err();
        assert!(
            err.to_string().contains("is not a duration"),
            "{bad}: {err}"
        );
    }
    assert_eq!(
        run([
            "erp", "cluster", "rollout", "open", "-d", "db", "--to", "zz", "--for", "30m"
        ]),
        ExitCode::from(1)
    );
    match parse(&["rollout", "open", "-d", "db", "--to", &hex, "--for", "2h"]).unwrap() {
        ClusterAction::Rollout(cmd) => match cmd.action {
            RolloutAction::Open(args) => {
                assert_eq!(args.duration.as_secs(), 7200);
                assert_eq!(args.to.to_string(), hex);
                assert!(args.from.is_none());
            }
            other => panic!("expected open, got {other:?}"),
        },
        other => panic!("expected rollout, got {other:?}"),
    }

    let h = setup().await;
    let name = h.db().name().to_owned();
    let a = node(&h, "app-1", "h1").await;
    assert_eq!(a.request_ready().await.unwrap(), Readiness::Ready);
    let listing = cluster(&h, &["nodes", "-d", &name]).await.unwrap();
    let mut lines = listing.lines();
    assert_eq!(
        lines.next().unwrap(),
        "NAME\tROLE\tSTATE\tFRESH\tHOST\tPID\tBUILD\tDIGEST\tEXPIRES\tID"
    );
    let line: Vec<&str> = lines.next().unwrap().split('\t').collect();
    assert_eq!(
        &line[..6],
        [
            "app-1",
            "app",
            "ready",
            "yes",
            "h1",
            &std::process::id().to_string()
        ]
    );
    assert_eq!(line[6], a.identity().build().label());
    assert_eq!(line[7], a.identity().digest().to_string());
    assert_eq!(line[9], a.identity().id().to_string());
    assert!(lines.next().is_none());

    let to = "cd".repeat(32);
    let opened = cluster(
        &h,
        &["rollout", "open", "-d", &name, "--to", &to, "--for", "30m"],
    )
    .await
    .unwrap();
    let from = a.identity().digest();
    assert!(
        opened.starts_with(&format!(
            "rollout window open in database {name}: from {from} to {to} until "
        )),
        "{opened}"
    );
    let status = cluster(&h, &["rollout", "status", "-d", &name])
        .await
        .unwrap();
    assert!(
        status.starts_with(&format!(
            "rollout window open in database {name}: from {from} to {to}"
        )),
        "{status}"
    );
    let closed = cluster(&h, &["rollout", "close", "-d", &name])
        .await
        .unwrap();
    assert_eq!(
        closed,
        format!("rollout window closed in database {name}: from {from} to {to}")
    );
    let status = cluster(&h, &["rollout", "status", "-d", &name])
        .await
        .unwrap();
    assert_eq!(status, format!("no open rollout window in database {name}"));
    // a refused window exits 1
    let refused = cluster(
        &h,
        &["rollout", "open", "-d", &name, "--to", &to, "--for", "59s"],
    )
    .await
    .unwrap_err();
    assert_eq!(refused.exit_code(), ExitCode::from(1));
    assert!(refused.to_string().ends_with("got 59 s"), "{refused}");
    a.shutdown().await.unwrap();
}

/// Not a spec case by itself (is the typed refusal): `erp dev -d` registers a dev
/// node, and is refused (exit 1) where a non-dev node is live.
#[tokio::test]
async fn erp_dev_registers_a_dev_node_and_is_refused_on_a_served_database() {
    let h = setup().await;
    let name = h.db().name().to_owned();
    let addons = tempfile::tempdir().unwrap();
    let args = DevArgs::new()
        .with_database(DatabaseName::new(&name).unwrap())
        .with_addons_path(vec![addons.path().to_path_buf()]);
    let dev = |args: DevArgs| {
        let config = h.db().config();
        tokio::task::spawn_blocking(move || run_dev_with(&args, config))
    };
    // no live node: it registers (and leaves again until the HTTP server exists)
    let err = dev(args.clone()).await.unwrap().unwrap_err();
    assert!(matches!(err, CliError::Execution(_)), "{err}");
    let rows = list_nodes(h.db().pool()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].is_dev());
    assert_eq!(rows[0].name(), "erp-dev");
    assert_eq!(rows[0].state(), NodeState::Stopped);
    // a live non-dev node: refused
    let a = node(&h, "app-1", "h1").await;
    let err = dev(args).await.unwrap().unwrap_err();
    assert_eq!(err.exit_code(), ExitCode::from(1));
    assert!(
        err.to_string()
            .contains("erp dev refuses to join a database served by live non-dev nodes: app-1"),
        "{err}"
    );
    a.shutdown().await.unwrap();
}

#[test]
fn login_selection_commands_require_explicit_enrollment_and_valid_digest() {
    use erp_cli::args::LoginSelectionAction;
    let ClusterAction::LoginSelection(cmd) = parse(&[
        "login-selection",
        "enable",
        "-d",
        "db",
        "--confirm-legacy-drained-and-fenced",
    ])
    .unwrap() else {
        panic!("login selection")
    };
    assert!(matches!(cmd.action, LoginSelectionAction::Enable(_)));
    assert_eq!(
        parse(&["login-selection", "enable", "-d", "db"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
        parse(&["login-selection", "approve", "-d", "db", "--digest", "zz"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::ValueValidation
    );
    assert!(
        parse(&[
            "login-selection",
            "publish",
            "-d",
            "db",
            "--maintenance-role",
            "maint",
            "--request-role",
            "runtime"
        ])
        .is_ok()
    );
    assert!(parse(&["login-selection", "status", "-d", "db"]).is_ok());
    assert!(parse(&["login-selection", "disable", "-d", "db"]).is_ok());
}

#[tokio::test]
async fn login_selection_cli_publishes_and_operates_real_state() {
    use erp_orm::node::{FeatureKey, FeatureSet, FeatureVersion, NodeOptions};
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .await
        .unwrap();
    let db = h.db().name().to_owned();
    let m = format!("cm_{db}");
    let r = format!("cr_{db}");
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "CREATE ROLE {} NOLOGIN;CREATE ROLE {} NOLOGIN",
        erp_sql::quote(&m),
        erp_sql::quote(&r)
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let initial = cluster(&h, &["login-selection", "status", "-d", &db])
        .await
        .unwrap();
    let published = cluster(
        &h,
        &[
            "login-selection",
            "publish",
            "-d",
            &db,
            "--maintenance-role",
            &m,
            "--request-role",
            &r,
        ],
    )
    .await
    .unwrap();
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &NodeOptions::default()).unwrap();
    let identity = NodeIdentity::builder(
        NodeRole::App,
        "cli-feature",
        "host",
        BuildIdentity::__from_build(
            "cli-feature",
            "1",
            false,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("0"),
        ),
        AddonsDigest::builder().build().unwrap(),
    )
    .features(
        FeatureSet::new([(
            FeatureKey::new("auth.login_principal").unwrap(),
            FeatureVersion::new(1).unwrap(),
        )])
        .unwrap(),
    )
    .create(&pool)
    .await
    .unwrap();
    let digest = identity.digest().to_string();
    let member = NodeMembership::register(Arc::new(identity), pool)
        .await
        .unwrap();
    let approved = cluster(
        &h,
        &["login-selection", "approve", "-d", &db, "--digest", &digest],
    )
    .await
    .unwrap();
    let enabled = cluster(
        &h,
        &[
            "login-selection",
            "enable",
            "-d",
            &db,
            "--confirm-legacy-drained-and-fenced",
        ],
    )
    .await
    .unwrap();
    let state = cluster(&h, &["login-selection", "status", "-d", &db])
        .await
        .unwrap();
    let disabled = cluster(&h, &["login-selection", "disable", "-d", &db])
        .await
        .unwrap();
    let again = cluster(&h, &["login-selection", "disable", "-d", &db])
        .await
        .unwrap();
    member.shutdown().await.unwrap();
    let mut cleanup = h.begin().await.unwrap();
    cleanup
        .batch(&format!(
            "DROP OWNED BY {},{} CASCADE;DROP ROLE {},{}",
            erp_sql::quote(&m),
            erp_sql::quote(&r),
            erp_sql::quote(&m),
            erp_sql::quote(&r)
        ))
        .await
        .unwrap();
    cleanup.commit().await.unwrap();
    assert_eq!(
        initial,
        format!(
            "login selection in database {db}: enabled=false generation=0 version=1 approved=[] incompatible=0"
        )
    );
    assert_eq!(
        published,
        format!(
            "login selection helpers published in database {db}: maintenance={m} request={r}; feature state unchanged"
        )
    );
    assert_eq!(
        approved,
        format!(
            "login selection in database {db}: enabled=false generation=0 version=1 approved=[{digest}] incompatible=0"
        )
    );
    assert_eq!(
        enabled,
        format!(
            "login selection in database {db}: enabled=true generation=1 version=1 approved=[{digest}] incompatible=0"
        )
    );
    assert_eq!(state, enabled);
    assert_eq!(
        disabled,
        format!(
            "login selection in database {db}: enabled=false generation=2 version=1 approved=[{digest}] incompatible=0"
        )
    );
    assert_eq!(again, disabled);
}
