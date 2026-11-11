//! `Orm::build_object`, the library half of `erp db build-object`
//! (design).
//!
//! Every test starts from a database where `scale.booking`, `scale.rate` and `scale.stay` have
//! their tables above a lowered `large_table_rows` and none of their exclusions or keys, nor the
//! `btree_gist` extension: the state in which the install refuses to build them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::time::{Duration, Instant};

use erp_orm::{
    BuildObjectOptions, BuildOutcome, BuildTimeout, LockTimeout, ObjectKind, SchemaSyncOptions,
};
use erp_registry::ModelSpec;
use erp_sdk::{Condition, ErrorKind};
use erp_test_models::base_users;
use erp_test_models::test_scale_excl;
use erp_testkit::Harness;
use support::{one, rows};

const OBJECT: &str = "_no_double_booking";
const SEEDED: i64 = 12;

async fn orm() -> Harness {
    Harness::builder([&base_users::ADDON, &test_scale_excl::ADDON])
        .build()
        .await
        .expect("harness")
}

fn full_name(model: &ModelSpec, name: &str) -> String {
    model
        .table_objects
        .iter()
        .find(|o| o.name == name)
        .unwrap()
        .full_name
        .clone()
}

/// What the install path wrote for the objects, read before the fixture removes them.
struct Installed {
    check_comment: String,
    excl_comment: String,
    key_comment: String,
}

/// A database whose tables are above the threshold and lack the objects and `btree_gist`.
/// `overlapping` seeds four bookings of one resource at the same time (one of them cancelled,
/// which the constraint's filter excepts) and two overlapping rates; otherwise every row is
/// disjoint. Returns the ids of the bookings and rates, sorted.
async fn large_fixture(overlapping: bool) -> (Harness, Installed, Vec<i64>, Vec<i64>) {
    let h = orm().await;
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let rate = registry.model("scale.rate").unwrap();
    let stay = registry.model("scale.stay").unwrap();
    let excl = full_name(booking, OBJECT);
    let check = erp_sql::exclusion::period_check_name(&excl);
    let key = full_name(rate, "_rate_key");
    let comment = |table: &str, name: &str| {
        format!(
            "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint \
             WHERE conname = '{name}' AND conrelid = '{table}'::regclass"
        )
    };
    let mut tx = h.begin().await.unwrap();
    let installed = Installed {
        check_comment: one(&mut tx, &comment(&booking.table, &check))
            .await
            .unwrap(),
        excl_comment: one(&mut tx, &comment(&booking.table, &excl)).await.unwrap(),
        key_comment: one(&mut tx, &comment(&rate.table, &key)).await.unwrap(),
    };
    for (table, name) in [
        (&booking.table, check.clone()),
        (&booking.table, excl),
        (&rate.table, key),
        (&stay.table, full_name(stay, "_no_double_stay")),
    ] {
        tx.execute(&format!("ALTER TABLE {table} DROP CONSTRAINT {name}"), &[])
            .await
            .unwrap();
    }
    tx.execute("DROP EXTENSION btree_gist", &[]).await.unwrap();
    tx.execute(
        "INSERT INTO scale_resource (name) SELECT 'r' || g FROM generate_series(1, $1::int8) g",
        &[&SEEDED],
    )
    .await
    .unwrap();
    let (bookings, rates) = if overlapping {
        let b = rows(
            &mut tx,
            "INSERT INTO scale_booking (resource_id, start, stop, state) \
             SELECT 1, timestamp '2026-01-05 10:00', timestamp '2026-01-05 12:00', s \
               FROM unnest(ARRAY['confirmed', 'confirmed', 'confirmed', 'cancel']) s RETURNING id::text",
        )
        .await;
        let r = rows(
            &mut tx,
            "INSERT INTO scale_rate (product_id, validity, amount) \
             SELECT 1, daterange(date '2026-01-01', date '2026-02-01' + g), 1 \
               FROM generate_series(0, 1) g RETURNING id::text",
        )
        .await;
        (b, r)
    } else {
        let b = rows(
            &mut tx,
            "INSERT INTO scale_booking (resource_id, start, stop, state) \
             SELECT id, timestamp '2026-01-05' + id * interval '1 hour', \
                    timestamp '2026-01-05' + id * interval '1 hour' + interval '30 minutes', \
                    'confirmed' FROM scale_resource RETURNING id::text",
        )
        .await;
        let r = rows(
            &mut tx,
            "INSERT INTO scale_rate (product_id, validity, amount) \
             SELECT id, daterange(date '2026-01-01', date '2026-02-01'), 1 \
               FROM scale_resource RETURNING id::text",
        )
        .await;
        (b, r)
    };
    tx.commit().await.unwrap();
    let ids = |rows: Vec<Vec<Option<String>>>| -> Vec<i64> {
        let mut ids: Vec<i64> = rows
            .into_iter()
            .map(|r| r[0].as_ref().unwrap().parse().unwrap())
            .collect();
        ids.sort_unstable();
        ids
    };
    (h, installed, ids(bookings), ids(rates))
}

fn secs(n: u64) -> BuildObjectOptions {
    BuildObjectOptions::new()
        .with_lock_timeout(LockTimeout::try_from(n).unwrap())
        .unwrap()
}

/// The constraints named in `names` with their oids (a re-created constraint has a new oid).
async fn oids(h: &Harness, names: &[&str]) -> Vec<Vec<Option<String>>> {
    let list = names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut tx = h.begin().await.unwrap();
    rows(
        &mut tx,
        &format!(
            "SELECT conname, oid::text FROM pg_constraint WHERE conname IN ({list}) ORDER BY 1"
        ),
    )
    .await
}

/// `large_table_rows` creates the extension, the period `CHECK` and the constraint with the
/// canonical comments in one transaction, the upgrade after it issues no DDL for the object, a
/// second build is a no-op; the argument checks of the command are in `erp-cli/tests/test_db.rs`)
#[tokio::test]
async fn build_object_builds_what_the_install_refuses_and_the_upgrade_then_does_nothing() {
    let (h, installed, _, _) = large_fixture(false).await;
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let rate = registry.model("scale.rate").unwrap();
    let excl = full_name(booking, OBJECT);
    let check = erp_sql::exclusion::period_check_name(&excl);
    let key = full_name(rate, "_rate_key");
    let large = SchemaSyncOptions::new().large_table_rows(10);

    // the install refuses the exclusion
    let mut tx = h.begin().await.unwrap();
    let refused = tx
        .init_models_with(&[booking.id], &large)
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::UserError);
    assert!(
        refused.message().ends_with("then retry the install"),
        "{}",
        refused.message()
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // the build
    let started = Instant::now();
    let report = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(5))
        .await
        .unwrap();
    assert!(report.duration() <= started.elapsed());
    assert_eq!(report.kind(), ObjectKind::Exclusion);
    assert_eq!(
        report.lock_timeout().as_secs(),
        5,
        "the value sent to the database"
    );
    assert_eq!(report.build_timeout(), BuildTimeout::default());
    assert_eq!(
        (
            report.model(),
            report.object(),
            report.table(),
            report.outcome()
        ),
        (
            "scale.booking",
            OBJECT,
            "scale_booking",
            BuildOutcome::Built
        )
    );
    let expected: Vec<String> = [
        vec![erp_sql::exclusion::create_extension(
            erp_registry::PgExtension::BtreeGist,
        )],
        erp_sql::exclusion::add_check("scale_booking", &check, &installed.check_comment),
        erp_sql::ddl::add_constraint("scale_booking", &excl, &installed.excl_comment),
    ]
    .concat();
    assert_eq!(report.statements(), expected.as_slice());
    assert_eq!(
        report.to_string(),
        format!(
            "built exclusion '{OBJECT}' on 'scale.booking' (table 'scale_booking') in {:.3}s; \
             statements run:\n  {}",
            report.duration().as_secs_f64(),
            expected.join("\n  ")
        )
    );
    // the extension, the CHECK and the constraint carry the comments the install wrote
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM pg_extension WHERE extname = 'btree_gist'"
        )
        .await
        .as_deref(),
        Some("1")
    );
    for (name, comment) in [
        (&check, &installed.check_comment),
        (&excl, &installed.excl_comment),
    ] {
        assert_eq!(
            one(
                &mut tx,
                &format!(
                    "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint \
                     WHERE conname = '{name}'"
                )
            )
            .await
            .as_ref(),
            Some(comment),
            "{name}"
        );
    }
    drop(tx);

    // the upgrade of the booking model now does nothing for the object (the constraints keep
    // their oids) although the table is still above the limit
    let before = oids(&h, &[&check, &excl]).await;
    assert_eq!(before.len(), 2);
    let mut tx = h.begin().await.unwrap();
    let sync = tx.init_models_with(&[booking.id], &large).await.unwrap();
    assert!(sync.is_complete());
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(oids(&h, &[&check, &excl]).await, before);

    // a second build finds the canonical definitions and changes nothing
    let again = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(5))
        .await
        .unwrap();
    assert_eq!(again.outcome(), BuildOutcome::AlreadyPresent);
    assert!(again.statements().is_empty());
    assert_eq!(
        again.to_string(),
        format!(
            "exclusion '{OBJECT}' on 'scale.booking' (table 'scale_booking') is already present \
             with the canonical definition; nothing to do ({:.3}s)",
            again.duration().as_secs_f64()
        )
    );
    assert_eq!(oids(&h, &[&check, &excl]).await, before);

    // the key: built with the default lock timeout, with the comment the install wrote
    let built = h
        .orm()
        .build_object("scale.rate", "_rate_key", &BuildObjectOptions::new())
        .await
        .unwrap();
    assert_eq!(
        built.statements(),
        erp_sql::ddl::add_constraint("scale_rate", &key, &installed.key_comment).as_slice()
    );
    let mut tx = h.begin().await.unwrap();
    let sync = tx.init_models_with(&[], &large).await.unwrap();
    assert!(
        sync.is_complete(),
        "the whole upgrade passes with both objects built"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `Validation` conflict message listing ids only, and nothing is left behind)
#[tokio::test]
async fn build_object_refuses_existing_rows_that_conflict_and_leaves_nothing() {
    let (h, _, bookings, rates) = large_fixture(true).await;
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let rate = registry.model("scale.rate").unwrap();
    let excl = full_name(booking, OBJECT);
    let check = erp_sql::exclusion::period_check_name(&excl);
    let key = full_name(rate, "_rate_key");

    let err = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    // three confirmed bookings overlap; the cancelled one is outside the constraint's filter
    let (a, b, c) = (bookings[0], bookings[1], bookings[2]);
    assert_eq!(
        err.message(),
        format!(
            "cannot add exclusion '{OBJECT}' to 'scale.booking': existing records conflict, e.g. \
             ({a}, {b}), ({a}, {c}), ({b}, {c})"
        )
    );
    assert_eq!(err.sqlstate(), Some("23P01"));
    let err = h
        .orm()
        .build_object("scale.rate", "_rate_key", &secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "cannot add key '_rate_key' to 'scale.rate': existing records conflict, e.g. ({}, {})",
            rates[0], rates[1]
        )
    );
    // nothing left behind: no CHECK, no constraint, no extension (all rolled back)
    assert!(oids(&h, &[&check, &excl, &key]).await.is_empty());
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM pg_extension WHERE extname = 'btree_gist'"
        )
        .await
        .as_deref(),
        Some("0")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// inverted period fail the build with the field-named message of the period check)
#[tokio::test]
async fn build_object_lists_at_most_twenty_pairs_and_names_a_broken_period() {
    let (h, _, _, _) = large_fixture(false).await;
    let mut tx = h.begin().await.unwrap();
    // eight more bookings of resource 1 at the same time: 28 pairs overlap
    tx.execute(
        "INSERT INTO scale_booking (resource_id, start, stop, state) \
         SELECT 1, timestamp '2026-03-01 10:00', timestamp '2026-03-01 11:00', 'confirmed' \
           FROM generate_series(1, 8)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let err = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    let (head, pairs) = err
        .message()
        .split_once("existing records conflict, e.g. ")
        .unwrap();
    assert_eq!(
        head,
        format!("cannot add exclusion '{OBJECT}' to 'scale.booking': ")
    );
    assert_eq!(pairs.matches('(').count(), 20, "{pairs}");

    // an inverted period: the CHECK refuses it first, with the text create and write give
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "DELETE FROM scale_booking WHERE start >= timestamp '2026-03-01'",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_booking (resource_id, start, stop, state) \
         VALUES (2, timestamp '2026-04-02', timestamp '2026-04-01', 'confirmed')",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let err = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23514"));
    assert_eq!(
        err.message(),
        format!(
            "cannot add exclusion '{OBJECT}' to 'scale.booking': existing records break the \
             period check: '{}' must be before '{}' on '{}'",
            booking.field("start").unwrap().string,
            booking.field("stop").unwrap().string,
            booking.description
        )
    );
    h.cleanup().await.unwrap();
}

/// the build with a `Lock` error naming the table and the timeout, for the table lock and for the
/// schema lock, and leaves nothing behind)
#[tokio::test]
async fn build_object_gives_up_after_the_lock_timeout() {
    let (h, _, _, _) = large_fixture(false).await;
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let excl = full_name(booking, OBJECT);

    // a reader holding the table: ADD CONSTRAINT needs ACCESS EXCLUSIVE
    let mut holder = h.begin().await.unwrap();
    holder
        .execute("LOCK TABLE scale_booking IN ACCESS SHARE MODE", &[])
        .await
        .unwrap();
    let started = Instant::now();
    let err = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(1))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "waited for the timeout"
    );
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.sqlstate(), Some("55P03"));
    assert_eq!(
        err.message(),
        "table 'scale_booking' is in use (building exclusion '_no_double_booking'); gave up \
         waiting for a lock after 1s (lock_timeout), try again when it has finished or raise \
         --lock-timeout"
    );
    holder.rollback().await.unwrap();
    drop(holder);

    // another schema synchronization holding the schema lock
    let mut other = h.begin().await.unwrap();
    other.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await.unwrap();
    let err = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(1))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(
        err.message(),
        "another schema synchronization of this database is running; gave up waiting for a lock \
         after 1s (lock_timeout), try again when it has finished or raise --lock-timeout"
    );
    other.rollback().await.unwrap();
    drop(other);
    assert!(oids(&h, &[&excl]).await.is_empty(), "nothing left behind");
    h.cleanup().await.unwrap();
}

/// declares, and the lock timeout is bounded from 1 to 3600 seconds)
#[tokio::test]
async fn build_object_names_unknown_models_and_objects_and_bounds_the_timeout() {
    let (h, _, _, _) = large_fixture(false).await;
    let orm = h.orm();
    let err = orm
        .build_object("scale.nothing", OBJECT, &secs(5))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::UserError, "unknown model 'scale.nothing'")
    );
    let err = orm
        .build_object("scale.booking", "_nope", &secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "model 'scale.booking' declares no object named '_nope'; 'scale.booking' declares: \
         '_no_double_booking'"
    );
    let err = orm
        .build_object("scale.resource", OBJECT, &secs(5))
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "model 'scale.resource' declares no object named '_no_double_booking'; 'scale.resource' \
         declares no exclusions or WITHOUT OVERLAPS keys"
    );
    // a table that does not exist yet is built by the install itself
    let mut tx = h.begin().await.unwrap();
    tx.execute("DROP TABLE scale_stay", &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let err = orm
        .build_object("scale.stay", "_no_double_stay", &secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(
        err.message(),
        "cannot build exclusion '_no_double_stay' on 'scale.stay': table 'scale_stay' does not \
         exist in this database; install or upgrade the module first (a new table gets the \
         object in the install itself)"
    );

    for bad in [0, 3601, u64::MAX] {
        let err = LockTimeout::try_from(bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError);
        assert_eq!(
            err.message(),
            format!(
                "lock timeout of {bad} seconds: it must be from 1 to 3600 seconds (0 would wait \
                 forever)"
            )
        );
    }
    let err = "ten".parse::<LockTimeout>().unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::UserError,
            "'ten' is not a whole number of seconds; the lock timeout must be from 1 to 3600 \
             seconds"
        )
    );
    assert_eq!(LockTimeout::try_from(1).unwrap().as_secs(), 1);
    assert_eq!(LockTimeout::try_from(3600).unwrap().as_secs(), 3600);
    for bad in [0, 59, 86_401] {
        let err = BuildTimeout::try_from(bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError);
        assert_eq!(
            err.message(),
            format!("build timeout of {bad} seconds: it must be from 60 to 86400 seconds")
        );
    }
    assert_eq!(BuildTimeout::try_from(60).unwrap().as_secs(), 60);
    assert_eq!(BuildTimeout::try_from(86_400).unwrap().as_secs(), 86_400);
    let defaults = BuildObjectOptions::new();
    assert_eq!(defaults.lock_timeout().as_secs(), 30);
    assert_eq!(defaults.build_timeout().as_secs(), 6 * 3600);
    // the build timeout must be longer than the lock timeout
    for (lock, build) in [(100, 60), (100, 100)] {
        let err = BuildObjectOptions::new()
            .with_lock_timeout(LockTimeout::try_from(lock).unwrap())
            .unwrap()
            .with_build_timeout(BuildTimeout::try_from(build).unwrap())
            .unwrap_err();
        assert_eq!(
            (err.kind(), err.message()),
            (
                ErrorKind::UserError,
                format!(
                    "build timeout of {build} seconds must be longer than the lock timeout of \
                     {lock} seconds"
                )
                .as_str()
            )
        );
    }
    // the other order is refused too: the options that exist are valid
    let err = BuildObjectOptions::new()
        .with_build_timeout(BuildTimeout::try_from(120).unwrap())
        .unwrap()
        .with_lock_timeout(LockTimeout::try_from(120).unwrap())
        .unwrap_err();
    assert_eq!(
        err.message(),
        "build timeout of 120 seconds must be longer than the lock timeout of 120 seconds"
    );
    // the default lock timeout prints as 30s and reads back, bare or with the s
    assert_eq!(LockTimeout::default().to_string(), "30s");
    assert_eq!(
        "30s".parse::<LockTimeout>().unwrap(),
        LockTimeout::default()
    );
    assert_eq!("30".parse::<LockTimeout>().unwrap(), LockTimeout::default());
    assert_eq!(
        BuildTimeout::default()
            .to_string()
            .parse::<BuildTimeout>()
            .unwrap(),
        BuildTimeout::default()
    );
    h.cleanup().await.unwrap();
}

/// own timeout the build still fails with the `Validation` conflict message, saying no pair
/// could be listed, with the original error as its source)
#[tokio::test]
async fn build_object_reports_the_conflict_when_the_diagnostic_query_times_out() {
    // process-wide test hook: nextest runs each test in its own process
    erp_orm::override_diagnostic_timeouts("1ms", "30s");
    let (h, _, _, _) = large_fixture(false).await;
    let mut tx = h.begin().await.unwrap();
    // 20,000 disjoint bookings of one resource, then two at the same time: the only conflicting
    // pair sits at the end of a join the 1 ms limit cannot finish
    tx.execute(
        "INSERT INTO scale_booking (resource_id, start, stop, state) \
         SELECT 1, timestamp '2030-01-01' + g * interval '1 hour', \
                timestamp '2030-01-01' + g * interval '1 hour' + interval '30 minutes', 'confirmed' \
           FROM generate_series(1, 20000) g",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_booking (resource_id, start, stop, state) \
         SELECT 1, timestamp '2029-01-01', timestamp '2029-01-02', 'confirmed' \
           FROM generate_series(1, 2)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let err = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        erp_sql::conflict::conflict_message_unlisted(
            erp_sql::conflict::ConflictObject::Exclusion,
            OBJECT,
            "scale.booking"
        )
    );
    assert_eq!(err.sqlstate(), Some("23P01"));
    let source = std::error::Error::source(&err).expect("the original error is the source");
    let source = source.downcast_ref::<erp_sdk::Error>().unwrap();
    assert_eq!(source.sqlstate(), Some("23P01"));
    h.cleanup().await.unwrap();
}

/// stopped with an error naming the object, the table and the limit, and nothing is left behind)
#[tokio::test]
async fn build_object_stops_a_statement_that_runs_past_the_build_timeout() {
    // process-wide test hook (the real minimum is one minute)
    // sent in milliseconds: zero (no limit) and fractions are refused
    for refused in [Duration::ZERO, Duration::from_micros(1_500)] {
        let err = erp_orm::override_build_statement_timeout(refused).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "the build statement timeout override must be a positive whole number of \
                 milliseconds, got {refused:?}"
            )
        );
    }
    erp_orm::override_build_statement_timeout(Duration::from_millis(1)).unwrap();
    let (h, _, _, _) = large_fixture(false).await;
    // Hold the exact advisory lock the build requests. Its schema-lock statement must remain
    // pending, so the 1 ms statement timeout fires independently of table size or host load.
    let mut holder = h.begin().await.unwrap();
    holder.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await.unwrap();
    let err = h
        .orm()
        .build_object("scale.booking", OBJECT, &secs(5))
        .await
        .unwrap_err();
    holder.rollback().await.unwrap();
    drop(holder);
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(err.sqlstate(), Some("57014"));
    assert_eq!(
        err.message(),
        "building exclusion '_no_double_booking' on 'scale.booking' (table 'scale_booking') ran \
         past the build timeout of 21600s and was stopped; nothing was changed. Raise it with \
         --build-timeout (up to 86400 seconds)"
    );
    // The 1 ms limit stops whichever statement of the build is running when it fires (the
    // catalog read, CREATE EXTENSION, the GiST build); the root of the error chain is always the
    // server's statement timeout, classified as the build's own limit (the build tells the
    // transaction the limit it set). A timeout raised while the server answered a preparation's
    // Sync used to be dropped by the driver, and the next statement's 25P02 was reported instead
    // (TEST.FLAKE-BUILD-TIMEOUT; driver_sync_errors.rs). The build's ROLLBACK also runs under
    // the 1 ms limit; if the timer stopped it, `discard_after` would release the connection and
    // keep this error's kind, SQLSTATE and condition, so the assertions below still hold.
    let mut root = None;
    let mut next = std::error::Error::source(&err);
    while let Some(source) = next {
        if let Some(found) = source.downcast_ref::<erp_sdk::Error>() {
            root = Some(found);
        }
        next = source.source();
    }
    let root = root.expect("the database error is in the source chain");
    assert_eq!(root.kind(), ErrorKind::Database);
    assert_eq!(root.sqlstate(), Some("57014"));
    assert_eq!(root.condition(), Some(Condition::StatementTimeout));
    let registry = std::sync::Arc::clone(h.registry());
    let excl = full_name(registry.model("scale.booking").unwrap(), OBJECT);
    assert!(oids(&h, &[&excl]).await.is_empty());
    h.cleanup().await.unwrap();
}

/// range fails with the database's own reason, not a period-check message; NULL ranges are
/// accepted)
#[tokio::test]
async fn build_object_names_why_a_key_cannot_index_an_empty_range() {
    let (h, _, _, _) = large_fixture(false).await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO scale_rate (product_id, validity, amount) VALUES (1, 'empty'::daterange, 1)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let err = h
        .orm()
        .build_object("scale.rate", "_rate_key", &secs(5))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23514"));
    assert_eq!(
        err.message(),
        "cannot add key '_rate_key' to 'scale.rate': existing records cannot be indexed: empty WITHOUT \
         OVERLAPS value found in column \"validity\" in relation \"scale_rate\""
    );
    // NULL ranges do not make the key fail
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "DELETE FROM scale_rate WHERE validity = 'empty'::daterange",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_rate (product_id, validity, amount) VALUES (1, NULL, 1)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let report = h
        .orm()
        .build_object("scale.rate", "_rate_key", &secs(5))
        .await
        .unwrap();
    assert_eq!(report.kind(), ObjectKind::TemporalKey);
    assert_eq!(report.outcome(), BuildOutcome::Built);
    h.cleanup().await.unwrap();
}
