//! R1 db H1 (review round 1): `BeginOptions::isolation`. A READ COMMITTED
//! transaction reads a new snapshot per statement; the default REPEATABLE READ keeps the first.
//! Two explicit transactions on two connections, interleaved step by step. (A credential
//! transaction at READ COMMITTED is no longer representable: `Orm::begin_credential` takes
//! `CredentialOptions`, which has no isolation setting; review round 1 type design T4.)
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

#[path = "support/currency_fixture.rs"]
mod currency_fixture;

use erp_fields::ErrorKind;
use erp_orm::{BeginOptions, Isolation, SUPERUSER_ID};
use erp_test_models::{base_users, test_orm};
use erp_testkit::Harness;

async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch("CREATE TABLE iso_probe (n int4 NOT NULL)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    h
}

async fn count(tx: &mut erp_orm::Transaction) -> i64 {
    tx.query("SELECT count(*) FROM iso_probe", &[])
        .await
        .unwrap()[0]
        .get(0)
}

/// Insert one row in its own committed transaction.
async fn insert_committed(h: &Harness) {
    let mut other = h.begin().await.unwrap();
    other
        .execute("INSERT INTO iso_probe (n) VALUES (1)", &[])
        .await
        .unwrap();
    other.commit().await.unwrap();
}

/// READ COMMITTED sees a row committed after its first statement; the transaction's isolation
/// level is the one asked for.
#[tokio::test]
async fn read_committed_sees_rows_committed_after_its_first_statement() {
    let h = harness().await;
    let mut rc = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let level: String = rc
        .query("SELECT current_setting('transaction_isolation')", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(level, "read committed");
    assert_eq!(count(&mut rc).await, 0);
    insert_committed(&h).await;
    assert_eq!(count(&mut rc).await, 1);
    rc.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The default stays REPEATABLE READ: the same interleaving keeps the first snapshot.
#[tokio::test]
async fn default_repeatable_read_keeps_its_first_snapshot() {
    let h = harness().await;
    let mut rr = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID))
        .await
        .unwrap();
    let level: String = rr
        .query("SELECT current_setting('transaction_isolation')", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(level, "repeatable read");
    assert_eq!(count(&mut rr).await, 0);
    insert_committed(&h).await;
    assert_eq!(count(&mut rr).await, 0);
    rr.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

/// READ COMMITTED combines with read-only.
#[tokio::test]
async fn read_committed_read_only_is_read_only() {
    let h = harness().await;
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .isolation(Isolation::ReadCommitted)
                .read_only("iso.probe"),
        )
        .await
        .unwrap();
    let row = &tx
        .query(
            "SELECT current_setting('transaction_isolation'), \
             current_setting('transaction_read_only')",
            &[],
        )
        .await
        .unwrap()[0];
    let (level, ro): (String, String) = (row.get(0), row.get(1));
    assert_eq!((level.as_str(), ro.as_str()), ("read committed", "on"));
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The message the record API refuses a READ COMMITTED transaction with.
fn record_api_refusal(records: &erp_orm::Records) -> String {
    format!(
        "{records}: the record API needs a REPEATABLE READ transaction; this one was opened with \
         BeginOptions::isolation(ReadCommitted), which allows raw statements only (query, \
         execute, batch)"
    )
}

/// Lead ruling (a), review of R1B (types 1, silent 1): a READ COMMITTED transaction refuses ORM
/// writes, reads and searches before sending anything; raw statements still run in it.
#[tokio::test]
async fn read_committed_refuses_the_record_api() {
    let h = harness().await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let mixed = tx.env().records("test_orm.mixed").unwrap();
    let before = tx.query_count();
    let mut vals = std::collections::BTreeMap::new();
    vals.insert("integer".to_owned(), erp_fields::Value::Int(1));
    let e = mixed.create_values(&mut tx, &[vals]).await.unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::Programming, record_api_refusal(&mixed).as_str())
    );
    let e = mixed
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind(), e.message()),
        (ErrorKind::Programming, record_api_refusal(&mixed).as_str())
    );
    assert_eq!(tx.query_count(), before, "nothing was sent");
    assert_eq!(count(&mut tx).await, 0, "raw statements still run");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
