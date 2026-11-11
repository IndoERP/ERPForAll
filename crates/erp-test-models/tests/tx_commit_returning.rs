//! `Orm::one_flight` (review round 1, lead decision 9 / database H2, type design
//! T5): a transaction of one statement opens, runs it and commits in one round trip (the login's
//! two short transactions). `Transaction::commit_returning` had no caller and is gone (rust
//! review round 2 S-2).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/currency_fixture.rs"]
mod currency_fixture;

use erp_fields::ErrorKind;
use erp_orm::{BeginOptions, CommitError, FinalStatement, Isolation, SUPERUSER_ID};
use erp_test_models::{base_users, test_orm};
use erp_testkit::tokio_postgres::types::{ToSql, Type};
use erp_testkit::{Harness, assert_round_trips};

async fn harness(round_trips: bool) -> Harness {
    let mut builder = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls());
    if round_trips {
        builder = builder.count_round_trips();
    }
    let h = builder.build().await.expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch("CREATE TABLE flight_probe (n int4 NOT NULL CHECK (n > 0))")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    h
}

async fn rows_in_probe(h: &Harness) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let n: i64 = tx
        .query("SELECT count(*) FROM flight_probe", &[])
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    n
}

const INSERT: &str = "INSERT INTO flight_probe (n) VALUES ($1::int4) RETURNING n * 10";

fn rc() -> BeginOptions {
    BeginOptions::new(SUPERUSER_ID)
        .on_primary()
        .isolation(Isolation::ReadCommitted)
}

/// A fresh primary transaction: `BEGIN`, the statement and `COMMIT` in one round trip; the rows
/// come back and the row is committed; the transaction object can begin again.
#[tokio::test]
async fn a_fresh_transaction_opens_runs_and_commits_in_one_round_trip() {
    let h = harness(true).await;
    let wire = h.wire().unwrap().clone();
    let n = 4i32;
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&n, Type::INT4)];
    let rows = assert_round_trips!(wire, == 1, {
        h.orm()
            .one_flight(rc(), FinalStatement::new(INSERT, &params))
            .await
            .unwrap()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>(0), 40);
    assert_eq!(rows_in_probe(&h).await, 1);
    // at READ COMMITTED
    let level = h
        .orm()
        .one_flight(
            rc(),
            FinalStatement::new("SELECT current_setting('transaction_isolation')", &[]),
        )
        .await
        .unwrap();
    assert_eq!(level[0].get::<_, String>(0), "read committed");
    h.cleanup().await.unwrap();
}

/// Types-delta N-b: `one_flight` routes to the primary itself; options without
/// `on_primary()` run the same flight.
#[tokio::test]
async fn one_flight_routes_to_the_primary_itself() {
    let h = harness(false).await;
    let n = 2i32;
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&n, Type::INT4)];
    let rows = h
        .orm()
        .one_flight(
            BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted),
            FinalStatement::new(INSERT, &params),
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i32>(0), 20);
    assert_eq!(rows_in_probe(&h).await, 1);
    h.cleanup().await.unwrap();
}

/// A refused statement in the one flight is a definite rollback with the statement's error, and
/// the connection stays usable.
#[tokio::test]
async fn a_refused_statement_rolls_back_the_flight() {
    let h = harness(false).await;
    let n = 0i32;
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&n, Type::INT4)];
    match h
        .orm()
        .one_flight(rc(), FinalStatement::new(INSERT, &params))
        .await
    {
        Err(CommitError::RolledBack(e)) => {
            assert_eq!(e.kind(), ErrorKind::Validation);
            assert_eq!(e.sqlstate(), Some("23514"));
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(rows_in_probe(&h).await, 0);
    h.cleanup().await.unwrap();
}

/// A read-only transaction refuses a final statement that may write, before anything is sent.
#[tokio::test]
async fn a_read_only_flight_refuses_a_write() {
    let h = harness(false).await;
    let n = 1i32;
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&n, Type::INT4)];
    match h
        .orm()
        .one_flight(
            rc().read_only("flight_probe.read"),
            FinalStatement::new(INSERT, &params),
        )
        .await
    {
        Err(CommitError::RolledBack(e)) => {
            assert_eq!(e.kind(), ErrorKind::Programming);
            assert!(e.message().contains("flight_probe.read"), "{e}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(rows_in_probe(&h).await, 0);
    h.cleanup().await.unwrap();
}
