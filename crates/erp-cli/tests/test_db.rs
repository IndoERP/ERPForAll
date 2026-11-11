//! `erp db build-object`: argument parsing, the command against a
//! throwaway database whose tables are above `large_table_rows`, and the `erp` binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::process::{Command, ExitCode, Output};
use std::sync::Arc;

use clap::Parser;
use erp_cli::args::{Cli, Commands, DbAction};
use erp_cli::db::execute_with;
use erp_cli::{CliError, run};
use erp_orm::{BuildTimeout, LockTimeout, SchemaSyncOptions};
use erp_test_models::{base_users, test_scale_excl};
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::config::Host;

fn parse(args: &[&str]) -> Result<DbAction, clap::Error> {
    let mut all = vec!["erp", "db", "build-object"];
    all.extend_from_slice(args);
    let Commands::Db(cmd) = Cli::try_parse_from(all)?.command else {
        panic!("expected the db command");
    };
    Ok(cmd.action)
}

fn build_args(action: DbAction) -> erp_cli::BuildObjectArgs {
    match action {
        DbAction::BuildObject(args) => args,
        other => panic!("expected build-object, got {other:?}"),
    }
}

/// The arguments after `erp db build-object -d prod --model m --object o`.
fn with(extra: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = ["-d", "prod", "--model", "m", "--object", "o"]
        .map(String::from)
        .into();
    all.extend(extra.iter().map(|s| (*s).to_owned()));
    all
}

fn parse_with(extra: &[&str]) -> Result<DbAction, clap::Error> {
    let all = with(extra);
    parse(&all.iter().map(String::as_str).collect::<Vec<_>>())
}

/// defaults to 30 seconds and the build timeout to 6 hours, and a value out of range or not a
/// number is refused at argument parsing with a message naming the bounds)
#[test]
fn build_object_arguments_parse_and_the_timeouts_are_bounded() {
    let args = build_args(
        parse(&[
            "-d",
            "prod",
            "--model",
            "scale.booking",
            "--object",
            "_no_double_booking",
            "--lock-timeout",
            "5",
            "--build-timeout",
            "600",
        ])
        .unwrap(),
    );
    assert_eq!(args.db.database.as_str(), "prod");
    assert_eq!(args.model, "scale.booking");
    assert_eq!(args.object, "_no_double_booking");
    assert_eq!(args.lock_timeout, LockTimeout::try_from(5).unwrap());
    assert_eq!(args.build_timeout, BuildTimeout::try_from(600).unwrap());
    let args = build_args(parse_with(&[]).unwrap());
    assert_eq!(args.lock_timeout.as_secs(), 30, "the default");
    assert_eq!(args.build_timeout.as_secs(), 21_600, "the default");
    // the defaults come from the types, print as Display prints them (`30s`), and that text is
    // accepted back, like a bare number
    assert_eq!(args.lock_timeout, LockTimeout::default());
    assert_eq!(args.build_timeout, BuildTimeout::default());
    assert_eq!(args.lock_timeout.to_string(), "30s");
    let args =
        build_args(parse_with(&["--lock-timeout", "30s", "--build-timeout", "21600s"]).unwrap());
    assert_eq!(args.lock_timeout, LockTimeout::default());
    assert_eq!(args.build_timeout, BuildTimeout::default());
    for (flag, edge) in [
        ("--lock-timeout", "1"),
        ("--lock-timeout", "3600"),
        ("--build-timeout", "60"),
        ("--build-timeout", "86400"),
    ] {
        parse_with(&[flag, edge]).unwrap();
    }
    for (flag, bad, reason) in [
        (
            "--lock-timeout",
            "0",
            "lock timeout of 0 seconds: it must be from 1 to 3600 seconds (0 would wait forever)",
        ),
        (
            "--lock-timeout",
            "3601",
            "lock timeout of 3601 seconds: it must be from 1 to 3600 seconds (0 would wait \
             forever)",
        ),
        (
            "--lock-timeout",
            "1.5",
            "'1.5' is not a whole number of seconds; the lock timeout must be from 1 to 3600 \
             seconds",
        ),
        (
            "--lock-timeout",
            "ten",
            "'ten' is not a whole number of seconds; the lock timeout must be from 1 to 3600 \
             seconds",
        ),
        (
            "--build-timeout",
            "59",
            "build timeout of 59 seconds: it must be from 60 to 86400 seconds",
        ),
        (
            "--build-timeout",
            "86401",
            "build timeout of 86401 seconds: it must be from 60 to 86400 seconds",
        ),
        (
            "--build-timeout",
            "x",
            "'x' is not a whole number of seconds; the build timeout must be from 60 to 86400 \
             seconds",
        ),
    ] {
        let err = parse_with(&[flag, bad]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation, "{bad}");
        // clap wraps the reason in its usage text; the reason itself is ours
        assert!(err.to_string().contains(reason), "{bad}: {err}");
        let mut all = vec!["erp".to_owned(), "db".to_owned(), "build-object".to_owned()];
        all.extend(with(&[flag, bad]));
        assert_eq!(run(all), ExitCode::from(1), "{flag} {bad}");
    }
    // the model, the object and the database are required
    for missing in [
        &["--model", "m", "--object", "o"][..],
        &["-d", "prod", "--object", "o"][..],
        &["-d", "prod", "--model", "m"][..],
    ] {
        let err = parse(missing).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}

async fn scale_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_scale_excl::ADDON])
        .build()
        .await
        .expect("harness")
}

/// Run the command as `erp db build-object` does, against the harness database and with the
/// registry of its addons (the binary composes the registry of the linked addons only).
async fn build(h: &Harness, extra: &[&str]) -> Result<String, CliError> {
    let name = h.db().name().to_owned();
    let mut args = vec!["-d", name.as_str()];
    args.extend_from_slice(extra);
    let action = parse(&args).unwrap();
    let registry = Arc::clone(h.registry());
    let config = h.db().config().clone();
    tokio::task::spawn_blocking(move || execute_with(&action, registry, config))
        .await
        .unwrap()
}

/// refused upgrade": this tree has no module-state table yet (the lifecycle arrives with M3), so
/// the test asserts the database state the refused upgrade could have changed instead: no
/// constraint, no `CHECK`, no extension)
///
/// The case through the command's own entry point and arguments: `scale.booking` and `scale.rate`
/// above a lowered `large_table_rows` (12 rows, limit 10); the upgrade is refused for the
/// exclusion and for the `WITHOUT OVERLAPS` key with the full messages and leaves nothing; a
/// reader holding the table makes `--lock-timeout 5` fail after 5 s with a `Lock` error;
/// `erp db build-object ... --lock-timeout 5` then builds the extension, the `CHECK` and the
/// constraint with the comments the install path wrote and prints a report; the upgrade after it
/// re-creates neither the constraint nor the `CHECK`; rows that overlap fail the command with the
/// `Validation` conflict message and leave nothing. The argument bounds (0 and above 3600 refused
/// at parsing) are in `build_object_arguments_parse_and_the_timeouts_are_bounded`.
#[tokio::test]
async fn spec_ormx_032_build_object_builds_what_the_install_refuses() {
    let h = scale_harness().await;
    let registry = Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let stay = registry.model("scale.stay").unwrap();
    let rate = registry.model("scale.rate").unwrap();
    let name_of = |m: &erp_registry::ModelSpec, n: &str| {
        m.table_objects
            .iter()
            .find(|o| o.name == n)
            .unwrap()
            .full_name
            .clone()
    };
    let excl = name_of(booking, "_no_double_booking");
    let check = erp_sql::exclusion::period_check_name(&excl);
    let key = name_of(rate, "_rate_key");

    // what the install path wrote, read before the state the install refuses is made
    let installed_comment = |name: String| {
        let h = &h;
        async move { comment(h, &name).await }
    };
    let (excl_comment, check_comment) = (
        installed_comment(excl.clone()).await,
        installed_comment(check.clone()).await,
    );
    let mut tx = h.begin().await.unwrap();
    for (table, name) in [
        (&booking.table, check.clone()),
        (&booking.table, excl.clone()),
        (&rate.table, key.clone()),
        (&stay.table, name_of(stay, "_no_double_stay")),
    ] {
        tx.execute(&format!("ALTER TABLE {table} DROP CONSTRAINT {name}"), &[])
            .await
            .unwrap();
    }
    // CASCADE: the base addon's own exclusion constraints (`ir.dated.parameter`) also use the
    // extension, and the test wants the state "nothing was installed yet" for the models it names
    tx.execute("DROP EXTENSION btree_gist CASCADE", &[])
        .await
        .unwrap();
    tx.execute(
        "INSERT INTO scale_resource (name) SELECT 'r' || g FROM generate_series(1, 12) g",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_booking (resource_id, start, stop, state) \
         SELECT id, timestamp '2026-01-05' + id * interval '1 hour', \
                timestamp '2026-01-05' + id * interval '1 hour' + interval '30 minutes', \
                'confirmed' FROM scale_resource",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_rate (product_id, validity, amount) \
         SELECT id, daterange(date '2026-01-01', date '2026-02-01'), 1 FROM scale_resource",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let large = SchemaSyncOptions::new().large_table_rows(10);
    let both = format!("'{check}', '{excl}', '{key}'");

    // the upgrade is refused for the exclusion and for the key, and leaves nothing
    for (model, object, what, kind) in [
        (
            booking,
            "_no_double_booking",
            "exclusion '_no_double_booking'",
            "an exclusion constraint",
        ),
        (
            rate,
            "_rate_key",
            "key '_rate_key'",
            "a WITHOUT OVERLAPS key",
        ),
    ] {
        let mut tx = h.begin().await.unwrap();
        let refused = tx.init_models_with(&[model.id], &large).await.unwrap_err();
        assert_eq!(refused.kind(), erp_fields::ErrorKind::UserError, "{what}");
        assert_eq!(
            refused.message(),
            format!(
                "cannot add {what} to '{m}' during the install: table '{t}' has more than 10 rows \
                 and {kind} has no online build. Run `erp db build-object -d <db> --model {m} \
                 --object {object}` in a maintenance window (it holds ACCESS EXCLUSIVE on the \
                 table for the build), then retry the install",
                m = model.name,
                t = model.table
            )
        );
        tx.rollback().await.unwrap();
        drop(tx);
        assert_eq!(constraints(&h, &both).await, 0, "{what}");
        assert_eq!(extension_count(&h).await, 0, "{what}");
    }

    // a reader holds the table: the build gives up after the 5 s it was given
    let mut holder = h.begin().await.unwrap();
    holder
        .execute("LOCK TABLE scale_booking IN ACCESS SHARE MODE", &[])
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let args = [
        "--model",
        "scale.booking",
        "--object",
        "_no_double_booking",
        "--lock-timeout",
        "5",
    ];
    let err = build(&h, &args).await.unwrap_err();
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(4500),
        "waited for the 5 s lock timeout, took {:?}",
        started.elapsed()
    );
    assert!(matches!(&err, CliError::Access(e) if e.kind() == erp_fields::ErrorKind::Lock));
    assert_eq!(
        err.to_string(),
        "table 'scale_booking' is in use (building exclusion '_no_double_booking'); gave up \
         waiting for a lock after 5s (lock_timeout), try again when it has finished or raise \
         --lock-timeout"
    );
    holder.rollback().await.unwrap();
    drop(holder);
    assert_eq!(constraints(&h, &both).await, 0);

    // the command
    let report = build(&h, &args).await.unwrap();
    assert!(
        report.starts_with(
            "built exclusion '_no_double_booking' on 'scale.booking' (table 'scale_booking') in "
        ),
        "{report}"
    );
    assert!(
        report.contains("\n  CREATE EXTENSION IF NOT EXISTS \"btree_gist\"\n"),
        "{report}"
    );
    assert!(
        report.contains(&format!("ADD CONSTRAINT \"{check}\"")),
        "{report}"
    );
    assert!(
        report.contains(&format!("ADD CONSTRAINT \"{excl}\"")),
        "{report}"
    );
    assert_eq!(constraints(&h, &format!("'{check}', '{excl}'")).await, 2);
    assert_eq!(extension_count(&h).await, 1, "btree_gist exists afterwards");
    // the comments are the ones the install path wrote
    assert_eq!(comment(&h, &excl).await, excl_comment);
    assert_eq!(comment(&h, &check).await, check_comment);

    // the upgrade after it passes (the table is still above the threshold) and re-creates neither
    let (built_excl, built_check) = (oid(&h, &excl).await, oid(&h, &check).await);
    let mut tx = h.begin().await.unwrap();
    let sync = tx.init_models_with(&[booking.id], &large).await.unwrap();
    assert!(sync.is_complete());
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(oid(&h, &excl).await, built_excl, "exclusion not re-created");
    assert_eq!(oid(&h, &check).await, built_check, "CHECK not re-created");

    // overlapping rows: the command fails with the conflict message and leaves nothing
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        &format!("ALTER TABLE scale_booking DROP CONSTRAINT {check}"),
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        &format!("ALTER TABLE scale_booking DROP CONSTRAINT {excl}"),
        &[],
    )
    .await
    .unwrap();
    let ids: Vec<i64> = tx
        .query(
            "INSERT INTO scale_booking (resource_id, start, stop, state) \
             SELECT 1, timestamp '2026-02-01 10:00', timestamp '2026-02-01 11:00', 'confirmed' \
               FROM generate_series(1, 2) RETURNING id::int8",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    tx.commit().await.unwrap();
    drop(tx);
    let (a, b) = (ids.iter().min().unwrap(), ids.iter().max().unwrap());
    let err = build(&h, &args).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "cannot add exclusion '_no_double_booking' to 'scale.booking': existing records \
             conflict, e.g. ({a}, {b})"
        )
    );
    assert_eq!(err.exit_code(), ExitCode::from(1));
    assert!(matches!(&err, CliError::Access(e) if e.kind() == erp_fields::ErrorKind::Validation));
    assert_eq!(
        constraints(&h, &format!("'{check}', '{excl}'")).await,
        0,
        "nothing left behind"
    );
    h.cleanup().await.unwrap();
}

/// The comment of the constraint `name`.
async fn comment(h: &Harness, name: &str) -> String {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            &format!(
                "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint \
                 WHERE conname = '{name}'"
            ),
            &[],
        )
        .await
        .unwrap();
    rows[0].get::<_, String>(0)
}

/// 1 when `btree_gist` is installed, else 0.
async fn extension_count(h: &Harness) -> usize {
    let mut tx = h.begin().await.unwrap();
    tx.query(
        "SELECT 1 FROM pg_extension WHERE extname = 'btree_gist'",
        &[],
    )
    .await
    .unwrap()
    .len()
}

/// The oid of the constraint `name` (a re-created constraint has a new one).
async fn oid(h: &Harness, name: &str) -> String {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            &format!("SELECT oid::text FROM pg_constraint WHERE conname = '{name}'"),
            &[],
        )
        .await
        .unwrap();
    rows[0].get::<_, String>(0)
}

/// How many of the constraints in `names` (a quoted, comma-separated list) exist.
async fn constraints(h: &Harness, names: &str) -> usize {
    let mut tx = h.begin().await.unwrap();
    tx.query(
        &format!("SELECT conname FROM pg_constraint WHERE conname IN ({names})"),
        &[],
    )
    .await
    .unwrap()
    .len()
}

/// A key=value connection string for the harness database (its cluster, its name).
fn conninfo(h: &Harness, dbname: &str) -> String {
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
    parts.push(format!("dbname={}", quote(dbname)));
    parts.join(" ")
}

fn erp(h: &Harness, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(args)
        .args(["-d", h.db().name()])
        .env("DATABASE_URL", conninfo(h, "postgres"))
        .output()
        .unwrap()
}

/// linked addons, which holds `base` only: a model it does not have fails with exit code 1 and the
/// named error; the build itself is exercised through the library entry point in
/// `spec_ormx_032_build_object_builds_what_the_install_refuses`)
#[tokio::test]
async fn the_binary_names_a_model_its_registry_does_not_have() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .expect("harness");
    let out = erp(
        &h,
        &[
            "db",
            "build-object",
            "--model",
            "scale.booking",
            "--object",
            "_no_double_booking",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "Error: unknown model 'scale.booking'\n"
    );
    h.cleanup().await.unwrap();
}
