//! `erp db vacuum-transient` (package D): argument parsing, the
//! command against a throwaway database, and the `erp` binary's exit codes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use clap::Parser;
use erp_cli::CliError;
use erp_cli::args::{Cli, Commands, DbAction, VacuumTransientArgs};
use erp_cli::db::execute_with;
use erp_test_models::{base_users, test_vacuum};
use erp_testkit::Harness;

fn parse(args: &[&str]) -> Result<VacuumTransientArgs, clap::Error> {
    let mut all = vec!["erp", "db", "vacuum-transient"];
    all.extend_from_slice(args);
    let Commands::Db(cmd) = Cli::try_parse_from(all)?.command else {
        panic!("expected the db command");
    };
    match cmd.action {
        DbAction::VacuumTransient(args) => Ok(args),
        other => panic!("expected vacuum-transient, got {other:?}"),
    }
}

#[test]
fn arguments_name_one_model_or_all_and_the_caps_are_bounded() {
    let one = parse(&["--config", "server.toml", "--model", "test_vac.default"]).unwrap();
    assert_eq!(one.model.as_deref(), Some("test_vac.default"));
    assert!(!one.all);
    let defaults = one.options().unwrap();
    assert_eq!(defaults, erp_orm::VacuumOptions::default());
    let all = parse(&[
        "--config",
        "server.toml",
        "--all",
        "--max-batch-rows",
        "10",
        "--max-batches",
        "2",
        "--max-rows",
        "50",
    ])
    .unwrap();
    assert!(all.all);
    let options = all.options().unwrap();
    assert_eq!(
        (options.batch_rows(), options.batches(), options.rows()),
        (10, 2, 50)
    );
    // exactly one of --model and --all
    for bad in [
        &["--config", "server.toml"][..],
        &["--config", "server.toml", "--all", "--model", "m"][..],
    ] {
        let err = parse(bad).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
                    | clap::error::ErrorKind::ArgumentConflict
            ),
            "{:?}",
            err.kind()
        );
    }
    // the hard caps cannot be raised or disabled: refused at parsing, naming the bound
    for (flag, bad, bound) in [
        ("--max-batch-rows", "0", "between 1 and 1000"),
        ("--max-batch-rows", "1001", "between 1 and 1000"),
        ("--max-batches", "101", "between 1 and 100"),
        ("--max-rows", "0", "between 1 and 100000"),
        ("--max-rows", "100001", "between 1 and 100000"),
    ] {
        let err = parse(&["--config", "server.toml", "--all", flag, bad]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(err.to_string().contains(bound), "{err}");
    }
    let err = parse(&["--config", "server.toml", "--all", "--max-rows", "many"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_vacuum::ADDON])
        .build()
        .await
        .expect("harness")
}

async fn exec(h: &Harness, sql: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.execute(sql, &[]).await.unwrap();
    tx.commit().await.unwrap();
}

async fn count(h: &Harness, table: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    tx.query(&format!("SELECT count(*)::int8 FROM {table}"), &[])
        .await
        .unwrap()[0]
        .get(0)
}

/// `n` rows written two hours ago.
async fn old_rows(h: &Harness, table: &str, n: i64) {
    exec(
        h,
        &format!(
            "INSERT INTO {table} (name, create_date, write_date) SELECT 'r' || i, \
             (now() AT TIME ZONE 'UTC') - interval '2 hours', \
             (now() AT TIME ZONE 'UTC') - interval '2 hours' FROM generate_series(1, {n}) AS i"
        ),
    )
    .await;
}

async fn run(h: &Harness, extra: &[&str]) -> Result<String, CliError> {
    let mut args = vec!["--config", "unused-by-the-test-seam.toml"];
    args.extend_from_slice(extra);
    let args = parse(&args).unwrap();
    let action = DbAction::VacuumTransient(args);
    let registry = Arc::clone(h.registry());
    let config = h.db().config().clone();
    tokio::task::spawn_blocking(move || execute_with(&action, registry, config))
        .await
        .unwrap()
}

#[tokio::test]
async fn one_model_is_vacuumed_and_the_outcome_printed() {
    let h = harness().await;
    old_rows(&h, "test_vac_default", 5).await;
    let out = run(&h, &["--model", "test_vac.default"]).await.unwrap();
    assert_eq!(count(&h, "test_vac_default").await, 0);
    assert!(out.contains("test_vac.default: completed"), "{out}");
    assert!(out.contains("deleted 5 rows in 1 batches"), "{out}");
    // a second call is not due yet, and says so
    let out = run(&h, &["--model", "test_vac.default"]).await.unwrap();
    assert!(out.contains("test_vac.default: not_due"), "{out}");
    assert!(out.contains("deleted 0 rows"), "{out}");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn all_vacuums_every_transient_model_in_the_registry_and_no_other() {
    let h = harness().await;
    old_rows(&h, "test_vac_default", 3).await;
    old_rows(&h, "test_vac_counted", 2).await;
    let out = run(&h, &["--all", "--max-batch-rows", "2"]).await.unwrap();
    for model in [
        "test_vac.default",
        "test_vac.counted",
        "test_vac.latch",
        "test_vac.off",
    ] {
        assert!(
            out.contains(&format!("{model}: ")),
            "{model} missing in {out}"
        );
    }
    assert!(!out.contains("test_vac.plain"), "{out}");
    assert!(!out.contains("test_vac.log"), "{out}");
    assert!(out.contains("test_vac.off: disabled"), "{out}");
    assert_eq!(count(&h, "test_vac_default").await, 0);
    assert_eq!(count(&h, "test_vac_counted").await, 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_unknown_or_non_transient_model_fails_with_a_named_error() {
    let h = harness().await;
    for (model, needle) in [
        ("test_vac.nothing", "unknown model"),
        ("test_vac.plain", "is not transient"),
    ] {
        let err = run(&h, &["--model", model]).await.unwrap_err();
        assert!(err.to_string().contains(needle), "{err}");
        assert_eq!(err.exit_code(), std::process::ExitCode::from(1));
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_failing_model_stops_the_run_and_the_error_carries_the_earlier_reports() {
    let h = harness().await;
    // the fixture hook refuses to delete a record named `boom`
    exec(
        &h,
        "INSERT INTO test_vac_default (name, create_date, write_date) VALUES ('boom', \
         (now() AT TIME ZONE 'UTC') - interval '2 hours', (now() AT TIME ZONE 'UTC') - interval '2 hours')",
    )
    .await;
    let err = run(&h, &["--all"]).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("test_vac.default"), "{text}");
    assert!(text.contains("vacuum failed"), "{text}");
    assert_eq!(err.exit_code(), std::process::ExitCode::from(1));
    assert_eq!(
        count(&h, "test_vac_default").await,
        1,
        "the failing batch rolled back"
    );
    h.cleanup().await.unwrap();
}

#[test]
fn the_outcome_has_a_stable_display_form() {
    use erp_orm::VacuumOutcome as O;
    for (outcome, text) in [
        (O::Disabled, "disabled"),
        (O::NotDue, "not_due"),
        (O::Busy, "busy"),
        (O::LostLease, "lost_lease"),
        (O::PolicyChanged, "policy_changed"),
        (O::Completed, "completed"),
        (O::Pending, "pending"),
        (O::BudgetExhausted, "budget_exhausted"),
    ] {
        assert_eq!(outcome.to_string(), text);
    }
}

#[tokio::test]
async fn a_failing_batch_keeps_the_original_error_as_the_source() {
    let h = harness().await;
    exec(
        &h,
        "INSERT INTO test_vac_default (name, create_date, write_date) VALUES ('boom', \
         (now() AT TIME ZONE 'UTC') - interval '2 hours', (now() AT TIME ZONE 'UTC') - interval '2 hours')",
    )
    .await;
    let err = run(&h, &["--model", "test_vac.default"]).await.unwrap_err();
    let CliError::Access(inner) = &err else {
        panic!("expected the engine's error, got {err:?}");
    };
    assert!(
        std::error::Error::source(inner.as_ref()).is_some(),
        "the original error is kept as the source: {inner:?}"
    );
    h.cleanup().await.unwrap();
}
