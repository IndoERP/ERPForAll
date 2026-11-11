//! Independent REPEATABLE READ database connections, with deterministic snapshot ordering.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration assertions"
)]
use erp_sdk::prelude::*;
use erp_testkit::Harness;
#[path = "support/guard_fixture.rs"]
mod guard_fixture;

async fn fixture() -> (Harness, i64) {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .await
        .unwrap();
    guard_fixture::install(&h).await;
    let mut tx = h.begin().await.unwrap();
    let unit = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Root".into()))],
        )
        .await
        .unwrap();
    let id = unit.ids()[0];
    tx.commit().await.unwrap();
    (h, id)
}

#[tokio::test]
async fn already_marked_consumers_both_succeed_without_advancing_fence() {
    let (h, id) = fixture().await;
    let a = h.db().client().await.unwrap();
    let b = h.db().client().await.unwrap();
    a.query(
        "SELECT * FROM public.erp_uom_register($1::bigint[])",
        &[&vec![id]],
    )
    .await
    .unwrap();
    let before: String = a
        .query_one("SELECT xmin::text FROM uom_reference_fence", &[])
        .await
        .unwrap()
        .get(0);
    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    let aid: i32 = a
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let bid: i32 = b
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    assert_ne!(aid, bid);
    for client in [&a, &b] {
        let rows = client
            .query(
                "SELECT * FROM public.erp_uom_register($1::bigint[])",
                &[&vec![id]],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, i64>(0), id);
    }
    a.batch_execute("COMMIT").await.unwrap();
    b.batch_execute("COMMIT").await.unwrap();
    let after: String = a
        .query_one("SELECT xmin::text FROM uom_reference_fence", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(after, before);
    drop(a);
    drop(b);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn first_use_after_a_competing_commit_requires_whole_transaction_retry() {
    let (h, id) = fixture().await;
    let a = h.db().client().await.unwrap();
    let b = h.db().client().await.unwrap();
    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    b.query_one("SELECT singleton FROM uom_reference_fence", &[])
        .await
        .unwrap();
    a.query(
        "SELECT * FROM public.erp_uom_register($1::bigint[])",
        &[&vec![id]],
    )
    .await
    .unwrap();
    a.batch_execute("COMMIT").await.unwrap();
    let error = b
        .query(
            "SELECT * FROM public.erp_uom_register($1::bigint[])",
            &[&vec![id]],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "40001");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "could not serialize access due to concurrent update"
    );
    b.batch_execute("ROLLBACK").await.unwrap();
    let rows = b
        .query(
            "SELECT * FROM public.erp_uom_register($1::bigint[])",
            &[&vec![id]],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    drop(a);
    drop(b);
    h.cleanup().await.unwrap();
}

async fn child(h: &Harness, parent: i64, ratio: i64) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Race child".into()))
                    .set_cell("relative_uom_id", Cell::Id(parent))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::from(ratio))),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    id
}

#[tokio::test]
async fn first_use_and_raw_topology_changes_serialize_in_both_orders() {
    for reference_first in [true, false] {
        let (h, root) = fixture().await;
        let id = child(&h, root, 6).await;
        let a = h.db().client().await.unwrap();
        let b = h.db().client().await.unwrap();
        a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .await
            .unwrap();
        b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .await
            .unwrap();
        b.query_one("SELECT singleton FROM uom_reference_fence", &[])
            .await
            .unwrap();
        if reference_first {
            a.query("SELECT * FROM erp_uom_register($1::bigint[])", &[&vec![id]])
                .await
                .unwrap();
        } else {
            a.execute(
                "UPDATE uom_uom SET relative_factor=7,factor=7 WHERE id=$1",
                &[&id],
            )
            .await
            .unwrap();
        }
        a.batch_execute("COMMIT").await.unwrap();
        let error = if reference_first {
            b.execute(
                "UPDATE uom_uom SET relative_factor=7,factor=7 WHERE id=$1",
                &[&id],
            )
            .await
            .unwrap_err()
        } else {
            b.query("SELECT * FROM erp_uom_register($1::bigint[])", &[&vec![id]])
                .await
                .unwrap_err()
        };
        assert_eq!(error.code().unwrap().code(), "40001");
        assert_eq!(
            error.as_db_error().unwrap().message(),
            "could not serialize access due to concurrent update"
        );
        b.batch_execute("ROLLBACK").await.unwrap();
        if reference_first {
            let error = b
                .execute(
                    "UPDATE uom_uom SET relative_factor=7,factor=7 WHERE id=$1",
                    &[&id],
                )
                .await
                .unwrap_err();
            assert_eq!(error.code().unwrap().code(), "UOM01");
            assert_eq!(
                error.as_db_error().unwrap().message(),
                "A referenced unit of measure would change. Create a new unit instead."
            );
        } else {
            let row = b
                .query_one(
                    "SELECT factor::text FROM erp_uom_register($1::bigint[])",
                    &[&vec![id]],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, String>(0), "7");
        }
        drop(a);
        drop(b);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn descendant_phantoms_cannot_escape_ancestor_protection() {
    for reparent in [false, true] {
        let (h, root) = fixture().await;
        let ancestor = child(&h, root, 2).await;
        let outsider = if reparent {
            Some(child(&h, root, 3).await)
        } else {
            None
        };
        let a = h.db().client().await.unwrap();
        let b = h.db().client().await.unwrap();
        b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .await
            .unwrap();
        b.query_one("SELECT singleton FROM uom_reference_fence", &[])
            .await
            .unwrap();
        let used = if let Some(id) = outsider {
            a.execute("UPDATE uom_uom SET relative_uom_id=$1,relative_factor=3,factor=6,parent_path=$2 WHERE id=$3",&[&ancestor,&format!("{root}/{ancestor}/{id}/"),&id]).await.unwrap();
            id
        } else {
            child(&h, ancestor, 3).await
        };
        a.query(
            "SELECT * FROM erp_uom_register($1::bigint[])",
            &[&vec![used]],
        )
        .await
        .unwrap();
        let error = b
            .execute(
                "UPDATE uom_uom SET relative_factor=4,factor=4 WHERE id=$1",
                &[&ancestor],
            )
            .await
            .unwrap_err();
        assert_eq!(error.code().unwrap().code(), "40001");
        assert_eq!(
            error.as_db_error().unwrap().message(),
            "could not serialize access due to concurrent update"
        );
        b.batch_execute("ROLLBACK").await.unwrap();
        let error = b
            .execute(
                "UPDATE uom_uom SET relative_factor=4,factor=4 WHERE id=$1",
                &[&ancestor],
            )
            .await
            .unwrap_err();
        assert_eq!(error.code().unwrap().code(), "UOM01");
        assert_eq!(
            error.as_db_error().unwrap().message(),
            "A referenced unit of measure would change. Create a new unit instead."
        );
        assert_eq!(
            a.query_one(
                "SELECT factor::text FROM uom_reference_use WHERE uom_id=$1",
                &[&used]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
            "6"
        );
        drop(a);
        drop(b);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn delete_before_first_reference_cannot_leave_a_dangling_marker() {
    let (h, root) = fixture().await;
    let id = child(&h, root, 6).await;
    let a = h.db().client().await.unwrap();
    let b = h.db().client().await.unwrap();
    b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    b.query_one("SELECT singleton FROM uom_reference_fence", &[])
        .await
        .unwrap();
    a.execute("DELETE FROM uom_uom WHERE id=$1", &[&id])
        .await
        .unwrap();
    let error = b
        .query("SELECT * FROM erp_uom_register($1::bigint[])", &[&vec![id]])
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "40001");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "could not serialize access due to concurrent update"
    );
    b.batch_execute("ROLLBACK").await.unwrap();
    let error = b
        .query("SELECT * FROM erp_uom_register($1::bigint[])", &[&vec![id]])
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap().code(), "UOM02");
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "Unit of measure hierarchy is invalid."
    );
    assert_eq!(
        a.query_one("SELECT count(*) FROM uom_reference_use", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    drop(a);
    drop(b);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fence_lock_timeout_is_returned_without_retry() {
    let (h, _) = fixture().await;
    let a = h.db().client().await.unwrap();
    a.batch_execute("BEGIN; UPDATE uom_reference_fence SET singleton=singleton")
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let began_with: String = tx
        .query(
            "SELECT setting FROM pg_settings WHERE name='lock_timeout'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.execute("SET LOCAL lock_timeout='10ms'", &[])
        .await
        .unwrap();
    let error = tx
        .execute("UPDATE uom_reference_fence SET singleton=singleton", &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Lock);
    assert_eq!(error.sqlstate(), Some("55P03"));
    assert_eq!(
        error.message(),
        format!(
            "canceling statement due to lock timeout (lock_timeout was {began_with} ms when the transaction began)"
        )
    );
    tx.rollback().await.unwrap();
    a.batch_execute("ROLLBACK").await.unwrap();
    drop(a);
    h.cleanup().await.unwrap();
}
