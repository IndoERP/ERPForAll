// SPDX-License-Identifier: MIT OR Apache-2.0
//! C4 feasibility only: these fixtures do not implement whole ACCANL cases.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

#[path = "support/analytic_marker_probe.rs"]
mod probe;

use probe::Probe;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_without_members_is_rejected_at_commit() {
    // The RED checkpoint omitted scheduling; this exercises the complete declared marker chain.
    let f = Probe::new(true).await;
    let mut tx = f.begin().await;
    tx.batch("INSERT INTO test_anl_probe_owner (name) VALUES ('invalid')")
        .await
        .unwrap();
    let error = tx
        .commit()
        .await
        .expect_err("surviving owner without members must not commit");
    f.assert_invalid(error);
    drop(tx);
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_owner").await,
        0
    );
    f.empty_markers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_control_without_scheduling_accepts_invalid_state() {
    let f = Probe::new(false).await;
    let mut tx = f.begin().await;
    tx.batch("INSERT INTO test_anl_probe_owner (name) VALUES ('negative control')")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_owner").await,
        1
    );
    f.empty_markers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_and_delete_validate_final_membership_and_rollback_every_change() {
    let f = Probe::new(true).await;
    f.seed().await;
    for sql in [
        "UPDATE test_anl_probe_member SET root = 2 WHERE id = 1",
        "UPDATE test_anl_probe_member SET owner_id = 2 WHERE id = 1",
        "DELETE FROM test_anl_probe_member WHERE id = 1",
        "UPDATE test_anl_probe_account SET root = 2 WHERE id = 1",
    ] {
        let mut tx = f.begin().await;
        tx.batch(sql).await.unwrap();
        f.assert_invalid(tx.commit().await.expect_err(sql));
        drop(tx);
        assert_eq!(
            f.scalar("SELECT count(*) FROM test_anl_probe_member WHERE owner_id = 1 AND root = 1")
                .await,
            1
        );
        f.empty_markers().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary_gap_and_root_rekey_can_be_repaired_by_later_statements() {
    let f = Probe::new(true).await;
    f.seed().await;
    let mut tx = f.begin().await;
    tx.batch("DELETE FROM test_anl_probe_member WHERE id = 1")
        .await
        .unwrap();
    tx.batch(
        "INSERT INTO test_anl_probe_member (id, owner_id, account_id, root) VALUES (1, 1, 1, 1)",
    )
    .await
    .unwrap();
    tx.batch("UPDATE test_anl_probe_account SET root = 3 WHERE id = 1")
        .await
        .unwrap();
    tx.batch("UPDATE test_anl_probe_member SET root = 3 WHERE id = 1")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_member WHERE root = 3")
            .await,
        1
    );
    f.empty_markers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_constraint_drain_does_not_hide_later_invalid_writes() {
    let f = Probe::new(true).await;
    f.seed().await;
    for defer_again in [false, true] {
        let mut tx = f.begin().await;
        tx.batch("UPDATE test_anl_probe_owner SET name = 'drained' WHERE id = 1; SET CONSTRAINTS ALL IMMEDIATE").await.unwrap();
        if defer_again {
            tx.batch("SET CONSTRAINTS ALL DEFERRED").await.unwrap();
            tx.batch("DELETE FROM test_anl_probe_member WHERE id = 1")
                .await
                .unwrap();
            f.assert_invalid(
                tx.commit()
                    .await
                    .expect_err("new marker survives prior drain"),
            );
        } else {
            f.assert_invalid(
                tx.batch("DELETE FROM test_anl_probe_member WHERE id = 1")
                    .await
                    .expect_err("immediate constraint refuses statement"),
            );
            tx.rollback().await.unwrap();
        }
        drop(tx);
        f.empty_markers().await;
    }
    let mut tx = f.begin().await;
    tx.batch("DELETE FROM test_anl_probe_member WHERE id = 1")
        .await
        .unwrap();
    f.assert_invalid(
        tx.batch("SET CONSTRAINTS ALL IMMEDIATE")
            .await
            .expect_err("explicit drain sees invalid final state"),
    );
    tx.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multirow_zero_row_and_later_invalid_statement_are_all_checked() {
    let f = Probe::new(true).await;
    f.seed().await;
    let mut tx = f.begin().await;
    tx.batch("UPDATE test_anl_probe_member SET root = root WHERE id = 99; DELETE FROM test_anl_probe_member WHERE id = 99; INSERT INTO test_anl_probe_owner (name) SELECT 'empty' WHERE false").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    f.empty_markers().await;
    let mut tx = f.begin().await;
    tx.batch("UPDATE test_anl_probe_owner SET name = 'changed' WHERE id IN (1, 2)")
        .await
        .unwrap();
    tx.batch("DELETE FROM test_anl_probe_member WHERE owner_id IN (1, 2)")
        .await
        .unwrap();
    f.assert_invalid(
        tx.commit()
            .await
            .expect_err("later multirow deletion checks every owner"),
    );
    drop(tx);
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_owner WHERE name = 'changed'")
            .await,
        0
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_member").await,
        2
    );
    f.empty_markers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_and_savepoint_rollbacks_remove_pending_events() {
    let f = Probe::new(true).await;
    f.seed().await;
    let mut tx = f.begin().await;
    tx.batch("DELETE FROM test_anl_probe_member WHERE id = 1")
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.empty_markers().await;
    let mut tx = f.begin().await;
    tx.batch("UPDATE test_anl_probe_owner SET name = 'kept' WHERE id = 1; SAVEPOINT gap; DELETE FROM test_anl_probe_member WHERE id = 1; ROLLBACK TO SAVEPOINT gap").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_owner WHERE name = 'kept'")
            .await,
        1
    );
    f.empty_markers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_deletion_skips_only_the_deleted_owner_and_cascades_members() {
    let f = Probe::new(true).await;
    f.seed().await;
    let mut tx = f.begin().await;
    tx.batch("DELETE FROM test_anl_probe_member WHERE id = 1; DELETE FROM test_anl_probe_owner WHERE id = 1; DELETE FROM test_anl_probe_owner WHERE id = 2").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_member").await,
        0
    );
    f.empty_markers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_transactions_keep_their_own_pending_events() {
    let f = Probe::new(true).await;
    f.seed().await;
    let mut first = f.begin().await;
    let mut second = f.begin().await;
    first
        .batch("UPDATE test_anl_probe_owner SET name = 'committed' WHERE id = 1")
        .await
        .unwrap();
    second
        .batch("DELETE FROM test_anl_probe_member WHERE id = 2")
        .await
        .unwrap();
    first.commit().await.unwrap();
    f.assert_invalid(
        second
            .commit()
            .await
            .expect_err("other transaction's event remains queued"),
    );
    drop((first, second));
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_owner WHERE name = 'committed'")
            .await,
        1
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM test_anl_probe_member WHERE owner_id = 2")
            .await,
        1
    );
    f.empty_markers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_trigger_shape_is_stable_on_repeat_sync() {
    let f = Probe::new(true).await;
    let before = f
        .scalar("SELECT sum(oid::bigint)::bigint FROM pg_trigger WHERE tgname LIKE 'anl_probe_%'")
        .await;
    f.sync().await;
    assert_eq!(
        f.scalar("SELECT sum(oid::bigint)::bigint FROM pg_trigger WHERE tgname LIKE 'anl_probe_%'")
            .await,
        before
    );
    assert_eq!(f.scalar("SELECT count(*) FROM pg_trigger WHERE tgname = 'anl_probe_deferred' AND tgdeferrable AND tginitdeferred AND tgenabled = 'A'").await, 1);
    assert_eq!(
        f.scalar("SELECT count(*) FROM erp_schema_sql_object WHERE addon = 'test_anl_probe'")
            .await,
        i64::try_from(f.object_count()).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_role_cannot_suppress_markers_or_hide_real_events_with_extra_markers() {
    let f = Probe::new(true).await;
    f.seed().await;
    f.assert_definer_privileges().await;
    let client = f.login().await;
    let who: String = client
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(who, f.request);
    let error = client
        .batch_execute(&format!("SET ROLE {}", erp_sql::quote(&f.definer)))
        .await
        .expect_err("request must not become the definer");
    let db = error.as_db_error().unwrap();
    assert_eq!(db.code().code(), "42501");
    assert_eq!(
        db.message(),
        format!("permission denied to set role \"{}\"", f.definer)
    );
    for (sql, message) in [
        (
            "SELECT * FROM test_anl_probe_marker",
            "permission denied for table test_anl_probe_marker",
        ),
        (
            "UPDATE test_anl_probe_marker SET owner_key = '999'",
            "permission denied for table test_anl_probe_marker",
        ),
        (
            "DELETE FROM test_anl_probe_marker",
            "permission denied for table test_anl_probe_marker",
        ),
        (
            "TRUNCATE test_anl_probe_marker",
            "permission denied for table test_anl_probe_marker",
        ),
        (
            "ALTER TABLE test_anl_probe_marker DISABLE TRIGGER ALL",
            "must be owner of table test_anl_probe_marker",
        ),
    ] {
        let error = client.batch_execute(sql).await.expect_err(sql);
        let db = error.as_db_error().unwrap();
        assert_eq!(db.code().code(), "42501");
        assert_eq!(db.message(), message);
    }
    client
        .batch_execute(
            "BEGIN; INSERT INTO test_anl_probe_marker (owner_key, xact) VALUES ('1', 'forged')",
        )
        .await
        .unwrap();
    client.batch_execute("COMMIT").await.unwrap();
    f.empty_markers().await;
    client.batch_execute("BEGIN; DELETE FROM test_anl_probe_member WHERE id = 1; INSERT INTO test_anl_probe_marker (owner_key) VALUES ('999')").await.unwrap();
    let error = client
        .batch_execute("COMMIT")
        .await
        .expect_err("an extra marker cannot erase the real event");
    let db = error.as_db_error().unwrap();
    assert_eq!(db.code().code(), probe::STATE);
    assert_eq!(db.constraint(), Some(probe::CONDITION));
    assert_eq!(db.message(), "private probe diagnostic");
    f.empty_markers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_stamp_overwrites_a_forged_transaction_identity() {
    let f = Probe::new(true).await;
    f.seed().await;
    let mut tx = f.begin().await;
    tx.batch("INSERT INTO test_anl_probe_marker (owner_key, xact) VALUES ('1', 'forged')")
        .await
        .unwrap();
    let rows = tx
        .query(
            "SELECT xact = pg_current_xact_id()::text FROM test_anl_probe_marker",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get::<_, bool>(0));
    tx.commit().await.unwrap();
    drop(tx);
    f.empty_markers().await;
}
