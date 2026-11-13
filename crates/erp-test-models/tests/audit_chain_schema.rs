// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/audit_chain.rs"]
mod support;
use support::*;

#[tokio::test]
async fn closed_chain_schema_install_is_explicit_idempotent_and_transactional() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    assert!(install(&mut tx).await.created());
    assert_eq!(count(&mut tx).await, 0);
    let oid: u32 = tx
        .query("SELECT 'erp_event.ir_audit_chain'::regclass::oid", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert!(!tx.install_audit_chain_schema().await.unwrap().created());
    let again: u32 = tx
        .query("SELECT 'erp_event.ir_audit_chain'::regclass::oid", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(oid, again);
    tx.rollback().await.unwrap();
    let c = owner(&h).await;
    assert!(
        c.query_one(
            "SELECT to_regclass('erp_event.ir_audit_chain') IS NULL",
            &[]
        )
        .await
        .unwrap()
        .get::<_, bool>(0)
    );
}

#[tokio::test]
async fn closed_chain_schema_refuses_catalog_drift() {
    let changes = [
        (
            "ALTER TABLE erp_event.ir_audit_chain DROP CONSTRAINT ir_audit_chain_format; ALTER TABLE erp_event.ir_audit_chain ADD CONSTRAINT ir_audit_chain_format CHECK(format_version=1) NOT VALID",
            "constraints",
        ),
        (
            "ALTER TABLE erp_event.ir_audit_chain ALTER CONSTRAINT ir_audit_chain_previous DEFERRABLE INITIALLY DEFERRED",
            "constraints",
        ),
        (
            "ALTER TABLE erp_event.ir_audit_chain ADD COLUMN surprise text",
            "columns",
        ),
        (
            "ALTER TABLE erp_event.ir_audit_chain ALTER COLUMN reason DROP NOT NULL",
            "columns",
        ),
        (
            "ALTER TABLE erp_event.ir_audit_chain ALTER COLUMN reason SET DEFAULT 'hidden'",
            "columns",
        ),
        (
            "ALTER TABLE erp_event.ir_audit_chain ALTER COLUMN reason TYPE varchar(128)",
            "columns",
        ),
        (
            "ALTER TABLE erp_event.ir_audit_chain DROP CONSTRAINT ir_audit_chain_format",
            "constraints",
        ),
        (
            "ALTER TABLE erp_event.ir_audit_chain DROP CONSTRAINT ir_audit_chain_format; ALTER TABLE erp_event.ir_audit_chain ADD CONSTRAINT ir_audit_chain_format CHECK (format_version > 0)",
            "constraints",
        ),
        (
            "CREATE INDEX unexpected_chain_index ON erp_event.ir_audit_chain(reason)",
            "indexes",
        ),
        (
            "ALTER TABLE erp_event.ir_audit_chain ENABLE ROW LEVEL SECURITY",
            "relation",
        ),
        (
            "CREATE RULE unexpected_chain_rule AS ON DELETE TO erp_event.ir_audit_chain DO INSTEAD NOTHING",
            "triggers/rules/policies",
        ),
    ];
    let h = installed().await;
    for (sql, part) in changes {
        let mut tx = begin(&h).await;
        tx.batch(sql).await.unwrap();
        let e = tx.install_audit_chain_schema().await.unwrap_err();
        refused(
            &e,
            &format!(
                "a later release: erp_event.ir_audit_chain has incompatible {part}; owner repair is required"
            ),
        );
        tx.rollback().await.unwrap();
    }
    let mut tx = begin(&h).await;
    tx.batch("ALTER SEQUENCE erp_event.ir_audit_seal_id_seq CYCLE")
        .await
        .unwrap();
    let e = tx.install_audit_chain_schema().await.unwrap_err();
    refused(
        &e,
        "a later release: erp_event.ir_audit_seal_id_seq has incompatible sequence; owner repair is required",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn closed_chain_schema_refuses_forged_objects() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    tx.batch("CREATE VIEW erp_event.ir_audit_chain AS SELECT 1 AS epoch")
        .await
        .unwrap();
    let e = tx.install_audit_chain_schema().await.unwrap_err();
    refused(
        &e,
        "a later release: erp_event.ir_audit_chain has incompatible relation; owner repair is required",
    );
    tx.rollback().await.unwrap();
    let h = installed().await;
    let role = h.read_only_role().await.unwrap();
    let mut tx = begin(&h).await;
    tx.batch(&format!(
        "ALTER TABLE erp_event.ir_audit_chain OWNER TO {}",
        erp_sql::quote(role.get_user().unwrap())
    ))
    .await
    .unwrap();
    let e = tx.install_audit_chain_schema().await.unwrap_err();
    refused(
        &e,
        "a later release: erp_event.ir_audit_chain has incompatible relation; owner repair is required",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn chain_constraints_reject_invalid_metadata() {
    let h = installed().await;
    for (sql, constraint) in [
        (
            "INSERT INTO erp_event.ir_audit_chain(epoch,format_version,reason) VALUES (0,1,'test')",
            "ir_audit_chain_epoch",
        ),
        (
            "INSERT INTO erp_event.ir_audit_chain(epoch,format_version,reason) VALUES (1,2,'test')",
            "ir_audit_chain_format",
        ),
        (
            "INSERT INTO erp_event.ir_audit_chain(epoch,incarnation,format_version,reason) VALUES (1,'00000000-0000-0000-0000-000000000000',1,'test')",
            "ir_audit_chain_incarnation",
        ),
        (
            "INSERT INTO erp_event.ir_audit_chain(epoch,format_version,reason) VALUES (2,1,'test')",
            "ir_audit_chain_genesis",
        ),
    ] {
        let c = owner(&h).await;
        let e = c.batch_execute(sql).await.unwrap_err();
        assert_eq!(e.code().unwrap().code(), "23514");
        assert_eq!(
            e.as_db_error().unwrap().message(),
            format!(
                "new row for relation \"ir_audit_chain\" violates check constraint \"{constraint}\""
            )
        );
    }
}

#[tokio::test]
async fn chain_installer_requires_system_before_sql() {
    let h = installed().await;
    let mut tx = h.begin_as(2).await.unwrap();
    let before = tx.query_count();
    let e = tx.install_audit_chain_schema().await.unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        e.message(),
        "Transaction::install_audit_chain_schema: only a transaction of the System principal with full elevation may do this (this one acts as user 2)"
    );
    assert_eq!(tx.query_count(), before);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn closed_chain_storage_rejects_disabled_guards_and_inheritance() {
    let h = installed().await;
    let mut tx = begin(&h).await;
    tx.batch("ALTER TABLE erp_event.ir_audit_chain DISABLE TRIGGER ALL")
        .await
        .unwrap();
    let e = tx.install_audit_chain_schema().await.unwrap_err();
    refused(
        &e,
        "a later release: erp_event.ir_audit_chain has incompatible triggers/rules/policies; owner repair is required",
    );
    tx.rollback().await.unwrap();
    let mut tx = begin(&h).await;
    tx.batch("CREATE TABLE erp_event.inherited_chain () INHERITS (erp_event.ir_audit_chain)")
        .await
        .unwrap();
    let e = tx.install_audit_chain_schema().await.unwrap_err();
    refused(
        &e,
        "a later release: erp_event.ir_audit_chain has incompatible relation; owner repair is required",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn seal_constraints_reject_invalid_metadata() {
    use erp_orm::audit::seal::EpochReason;
    let h = installed().await;
    let mut tx = begin(&h).await;
    tx.initialize_audit_chain(&EpochReason::new("constraint.fixture").unwrap())
        .await
        .unwrap();
    seed_seal(&mut tx).await;
    tx.commit().await.unwrap();
    let c = owner(&h).await;
    for (change, constraint) in [
        ("row_count=0", "ir_audit_seal_count"),
        ("batch_no=0", "ir_audit_seal_batch_no"),
        ("batch_digest=decode('00','hex')", "ir_audit_seal_digest"),
        (
            "prev_seal_hash=decode('00','hex')",
            "ir_audit_seal_predecessor",
        ),
        ("seal_hash=decode('00','hex')", "ir_audit_seal_hash"),
        ("format_version=2", "ir_audit_seal_format"),
        ("lower_xid='1',lower_id=-1", "ir_audit_seal_lower_id"),
        ("upper_id=0", "ir_audit_seal_upper_id"),
        ("lower_xid='43',lower_id=1", "ir_audit_seal_order"),
        ("lower_id=1", "ir_audit_seal_initial_cursor"),
    ] {
        let e = c
            .batch_execute(&format!("UPDATE erp_event.ir_audit_seal SET {change}"))
            .await
            .unwrap_err();
        assert_eq!(e.code().unwrap().code(), "23514");
        assert_eq!(
            e.as_db_error().unwrap().message(),
            format!(
                "new row for relation \"ir_audit_seal\" violates check constraint \"{constraint}\""
            )
        );
    }
}

#[tokio::test]
async fn chain_constraints_survive_repeated_catalog_restore() {
    let h = installed().await;
    let mut tx = begin(&h).await;
    tx.batch("SET LOCAL search_path=pg_catalog").await.unwrap();
    for _ in 0..2 {
        let mut constraints = Vec::new();
        for table in ["erp_event.ir_audit_chain", "erp_event.ir_audit_seal"] {
            let rows = tx.query("SELECT conname::text,pg_get_constraintdef(oid),contype='f' FROM pg_constraint WHERE conrelid=$1::text::regclass AND contype<>'n' ORDER BY conname", &[&table]).await.unwrap();
            constraints.extend(rows.iter().map(|r| {
                (
                    table,
                    r.get::<_, String>(0),
                    r.get::<_, String>(1),
                    r.get::<_, bool>(2),
                )
            }));
        }
        // Remove dependent foreign keys first; restore referenced keys before foreign keys.
        for foreign in [true, false] {
            for (table, name, _, _) in constraints.iter().filter(|c| c.3 == foreign) {
                tx.batch(&format!(
                    "ALTER TABLE {table} DROP CONSTRAINT {}",
                    erp_sql::quote(name)
                ))
                .await
                .unwrap();
            }
        }
        for foreign in [false, true] {
            for (table, name, definition, _) in constraints.iter().filter(|c| c.3 == foreign) {
                tx.batch(&format!(
                    "ALTER TABLE {table} ADD CONSTRAINT {} {definition}",
                    erp_sql::quote(name)
                ))
                .await
                .unwrap();
            }
        }
        assert!(!tx.install_audit_chain_schema().await.unwrap().created());
    }
    tx.rollback().await.unwrap();
}
