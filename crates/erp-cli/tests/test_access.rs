//! `erp access report` and `erp access disable-rule` (Revision 2
//! S3): parsing, and the `erp` binary against a throwaway database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::process::{Command, ExitCode, Output};
use std::sync::Arc;

use clap::Parser;
use erp_cli::args::{AccessAction, Cli, Commands};
use erp_cli::run;
use erp_orm::CAPABILITIES;
use erp_registry::RegistryBuilder;
use erp_security::admin::AccessReport;
use erp_security::loader::load_rows_from;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::config::Host;
use erp_testkit::tokio_postgres::{Client, NoTls};

#[test]
fn access_commands_parse() {
    let cli = Cli::try_parse_from([
        "erp",
        "access",
        "disable-rule",
        "-d",
        "prod",
        "sale.access_order_portal",
    ])
    .unwrap();
    let Commands::Access(cmd) = cli.command else {
        panic!("expected the access command");
    };
    let AccessAction::DisableRule(args) = cmd.action else {
        panic!("expected disable-rule");
    };
    assert_eq!(args.xmlid.as_str(), "sale.access_order_portal");
    assert_eq!(args.db.database.as_str(), "prod");
    let cli = Cli::try_parse_from(["erp", "access", "report", "-d", "prod"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Access(cmd) if matches!(cmd.action, AccessAction::Report(_))
    ));
}

/// A malformed external id is a validation error of the argument (exit code 1), before any
/// connection.
#[test]
fn malformed_external_ids_are_refused() {
    for xmlid in ["no_module", "a.b.c", ".x"] {
        let code = run(["erp", "access", "disable-rule", "-d", "x", xmlid]);
        assert_eq!(code, ExitCode::from(1), "{xmlid}");
    }
}

/// A key=value connection string for the harness database (its cluster, its name).
fn conninfo(h: &Harness) -> String {
    let config = h.db().config();
    let quote = |v: &str| format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"));
    let mut parts = Vec::new();
    for host in config.get_hosts() {
        match host {
            Host::Tcp(name) => parts.push(format!("host={}", quote(name))),
            Host::Unix(path) => parts.push(format!("host={}", quote(&path.to_string_lossy()))),
        }
    }
    for port in config.get_ports() {
        parts.push(format!("port={port}"));
    }
    if let Some(user) = config.get_user() {
        parts.push(format!("user={}", quote(user)));
    }
    if let Some(password) = config.get_password() {
        parts.push(format!(
            "password={}",
            quote(std::str::from_utf8(password).unwrap())
        ));
    }
    parts.push(format!("dbname={}", quote(config.get_dbname().unwrap())));
    parts.join(" ")
}

/// Run `erp <args> -d <harness database>` with the credentials in `DATABASE_URL` (whose own
/// database is another one: `-d` decides).
fn erp(h: &Harness, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(args)
        .args(["-d", h.db().name()])
        .env(
            "DATABASE_URL",
            conninfo(h).replace(h.db().name(), "postgres"),
        )
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("erp-cli test: connection closed with an error: {error}");
        }
    });
    client
}

/// The database `erp access` composes the registry of: `base` and the external ids.
async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .expect("harness")
}

/// A model, a group with an external id, an active rule `x.access_thing` (committed); the rule's
/// id.
async fn seed(client: &Client) -> i64 {
    let one = |row: erp_testkit::tokio_postgres::Row| -> i64 { row.get(0) };
    let model = one(client
        .query_one(
            "INSERT INTO ir_model (name, model, state, transient) \
             VALUES ('Thing', 'x_thing', 'manual', false) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap());
    let group = one(client
        .query_one(
            "INSERT INTO res_groups (name) VALUES ('Readers') RETURNING id::int8",
            &[],
        )
        .await
        .unwrap());
    let rule = one(client
        .query_one(
            "INSERT INTO ir_access (name, model_id, group_id, operation, active) \
             VALUES ('readers read things', $1, $2, 'r', true) RETURNING id::int8",
            &[&model, &group],
        )
        .await
        .unwrap());
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('x', 'access_thing', 'ir.access', $1), ('x', 'group_readers', 'res.groups', $2)",
            &[&rule, &group],
        )
        .await
        .unwrap();
    rule
}

async fn policy_rows(client: &Client) -> i64 {
    client
        .query_one(
            "SELECT count(*)::int8 FROM erp_signal_generation WHERE key = 'policy'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// `disable-rule` switches the rule off through the ORM as the System principal (audit fields
/// written, the policy generation bumped by the trigger), and says so; a second run changes
/// nothing and says that.
#[tokio::test]
async fn disable_rule_switches_a_rule_off_once() {
    let h = harness().await;
    let client = connect(&h).await;
    let rule = seed(&client).await;
    let bumps = policy_rows(&client).await;

    let out = erp(&h, &["access", "disable-rule", "x.access_thing"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!("access rule {rule} 'readers read things' on x_thing disabled\n")
    );
    let row = client
        .query_one(
            "SELECT active, write_uid::int8 FROM ir_access WHERE id = $1",
            &[&rule],
        )
        .await
        .unwrap();
    assert_eq!((row.get::<_, bool>(0), row.get::<_, i64>(1)), (false, 1));
    assert_eq!(policy_rows(&client).await, bumps + 1);

    let out = erp(&h, &["access", "disable-rule", "x.access_thing"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!("access rule {rule} 'readers read things' on x_thing was already disabled\n")
    );
    assert_eq!(policy_rows(&client).await, bumps + 1, "nothing written");
    drop(client);
    h.cleanup().await.unwrap();
}

/// Unknown external ids and ids of other models are the caller's mistake (exit code 1, the
/// message on stderr); nothing is written.
#[tokio::test]
async fn disable_rule_refuses_unknown_and_foreign_ids() {
    let h = harness().await;
    let client = connect(&h).await;
    seed(&client).await;
    let bumps = policy_rows(&client).await;
    for (xmlid, message) in [
        ("x.nope", "no record has the external id 'x.nope'"),
        (
            "x.group_readers",
            "the external id 'x.group_readers' names a res.groups record, not an access rule",
        ),
    ] {
        let out = erp(&h, &["access", "disable-rule", xmlid]);
        assert_eq!(out.status.code(), Some(1), "{xmlid}");
        assert_eq!(text(&out.stderr), format!("Error: {message}\n"));
        assert!(out.stdout.is_empty());
    }
    assert_eq!(policy_rows(&client).await, bumps);
    drop(client);
    h.cleanup().await.unwrap();
}

/// `report` prints the report of what the loader reads. Every capability linked into this test
/// (a superset of the `erp` binary's) gets a mapping, so both sides list no unmapped one.
#[tokio::test]
async fn report_prints_what_the_loader_reads() {
    let h = harness().await;
    let mut client = connect(&h).await;
    seed(&client).await;
    for capability in CAPABILITIES.iter() {
        client
            .execute(
                "INSERT INTO ir_capability (name, groups) VALUES ($1, 'base.group_system')",
                &[&capability.name()],
            )
            .await
            .unwrap();
    }
    let out = erp(&h, &["access", "report"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let rows = load_rows_from(&mut client).await.unwrap();
    let registry = Arc::new(
        RegistryBuilder::new([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
            .with_decls([&erp_data::IR_MODEL_DATA])
            .build()
            .unwrap(),
    );
    assert_eq!(
        text(&out.stdout),
        format!("{}\n", AccessReport::build(&rows, registry).unwrap())
    );
    assert!(text(&out.stdout).starts_with(&format!("policy generation {}\n", rows.generation())));
    drop(client);
    h.cleanup().await.unwrap();
}

/// Connection problems are input errors that never echo the connection string; credentials are
/// never taken from the command line.
#[test]
fn connection_strings_are_checked_and_never_echoed() {
    let out = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["access", "report", "-d", "x"])
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "Error: Invalid argument 'DATABASE_URL': not set: the connection credentials come from \
         the environment\n"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["access", "report", "-d", "x"])
        .env("DATABASE_URL", "postgres://u:s3cret@[::1/x")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert_eq!(
        stderr,
        "Error: Invalid argument 'DATABASE_URL': not a PostgreSQL connection string (URL or \
         key=value form)\n"
    );
    assert!(!stderr.contains("s3cret"));
    // no flag takes credentials
    let refused = Cli::try_parse_from(["erp", "access", "report", "-d", "x", "--db-url", "y"]);
    assert!(refused.is_err());
    assert!(
        Cli::try_parse_from(["erp", "access", "report"]).is_err(),
        "-d is required"
    );
}
