//! step 6: `Transaction::store_snapshot` against a real PostgreSQL database.
//! Two explicit transactions on two connections,
//! interleaved step by step.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/currency_fixture.rs"]
mod currency_fixture;

use std::collections::BTreeMap;

use erp_fields::ErrorKind;
use erp_orm::{BeginOptions, Isolation, SUPERUSER_ID, Transaction, Xid8};
use erp_sdk::{Cell, Command, Domain, RawVals, Value};
use erp_test_models::{base_users, test_depends, test_orm};
use erp_testkit::Harness;

async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_depends::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch("CREATE TABLE snap_probe (n int4 NOT NULL)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    h
}

/// A write, which gives the transaction an xid; returns it as `pg_current_xact_id()` reports it.
async fn write_and_xid(tx: &mut Transaction) -> Xid8 {
    tx.execute("INSERT INTO snap_probe (n) VALUES (1)", &[])
        .await
        .unwrap();
    let rows = tx
        .query("SELECT pg_current_xact_id()::text", &[])
        .await
        .unwrap();
    let text: String = rows[0].get(0);
    Xid8::parse(&text).unwrap()
}

/// A stored total as the database holds it, read with raw SQL (which never flushes the ORM).
async fn stored_total(tx: &mut Transaction, order: i64) -> f64 {
    let rows = tx
        .query(
            "SELECT amount_total::float8 FROM test_dep_order WHERE id = $1",
            &[&order],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

#[tokio::test]
async fn store_snapshot_own_xid() {
    let h = harness().await;
    let mut ro = h.begin().await.unwrap();
    let asof = ro.store_snapshot().await.unwrap();
    assert_eq!(
        asof.own(),
        None,
        "a transaction without a write has no own xid"
    );
    ro.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let xid = write_and_xid(&mut tx).await;
    let asof = tx.store_snapshot().await.unwrap();
    assert_eq!(asof.own(), Some(xid));
    assert!(asof.reflects(xid), "the transaction knows its own write");
    assert!(asof.xip().binary_search(&xid).is_err(), "own is not in xip");
    tx.rollback().await.unwrap();

    // Pending ORM writes are flushed before the descriptor is read: an order's stored total is
    // recomputed lazily after a line price changes, and the recompute writes the order row.
    let mut setup = h.begin().await.unwrap();
    let orders = setup.env().records("test_dep.order").unwrap();
    let created = orders
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("o".into()))
                    .set_commands(
                        "line_ids",
                        [Command::create(
                            RawVals::new()
                                .set_cell("product", Cell::Str("p".into()))
                                .set_cell("price_unit", Cell::Float(1.0)),
                        )],
                    ),
            ],
        )
        .await
        .unwrap();
    setup.flush().await.unwrap();
    let order = created.ids()[0];
    setup.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("test_dep.line").unwrap();
    let lines = lines.search(&mut tx, &Domain::TRUE).await.unwrap();
    let mut vals = BTreeMap::new();
    vals.insert("price_unit".to_owned(), Value::Float(5.0));
    lines.write_values(&mut tx, &vals).await.unwrap();
    assert!(
        (stored_total(&mut tx, order).await - 1.0).abs() < f64::EPSILON,
        "precondition: the recompute of the total is still pending, not written"
    );
    let rows = tx
        .query("SELECT pg_current_xact_id()::text", &[])
        .await
        .unwrap();
    let text: String = rows[0].get(0);
    let xid = Xid8::parse(&text).unwrap();
    let asof = tx.store_snapshot().await.unwrap();
    assert!(
        (stored_total(&mut tx, order).await - 5.0).abs() < f64::EPSILON,
        "the pending write reached the database before the descriptor was read"
    );
    assert_eq!(
        asof.own(),
        Some(xid),
        "the flush ran in the same transaction"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn committed_higher_xid_reflected_lower_in_progress_not() {
    let h = harness().await;
    let mut t1 = h.begin().await.unwrap();
    let a = write_and_xid(&mut t1).await;
    let mut t2 = h.begin().await.unwrap();
    let b = write_and_xid(&mut t2).await;
    assert!(b > a, "T2 started its write after T1");
    t2.commit().await.unwrap();

    let mut reader = h.begin().await.unwrap();
    let asof = reader.store_snapshot().await.unwrap();
    assert!(asof.reflects(b), "the committed higher xid is reflected");
    assert!(!asof.reflects(a), "the lower in-progress xid is not");
    assert!(asof.xip().contains(&a), "a is in xip");
    reader.rollback().await.unwrap();

    t1.commit().await.unwrap();
    let mut after = h.begin().await.unwrap();
    let asof = after.store_snapshot().await.unwrap();
    assert!(
        asof.reflects(a) && asof.reflects(b),
        "both reflected after T1 commits"
    );
    after.rollback().await.unwrap();
}

#[tokio::test]
async fn store_snapshot_refuses_read_committed() {
    let h = harness().await;
    let mut rc = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    // A1 needs one snapshot for the whole transaction; READ COMMITTED takes a new one per statement.
    let err = rc.store_snapshot().await.expect_err("refused");
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "store_snapshot: assumption A1 (one snapshot for the whole transaction) needs a REPEATABLE READ \
         transaction; this one was opened with BeginOptions::isolation(ReadCommitted)"
    );
    rc.rollback().await.unwrap();

    // The default isolation level still answers.
    let mut rr = h.begin().await.unwrap();
    rr.store_snapshot().await.unwrap();
    rr.rollback().await.unwrap();
}
