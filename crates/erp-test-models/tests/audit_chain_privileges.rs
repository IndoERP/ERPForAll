// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/audit_chain.rs"]
mod support;
use erp_testkit::tokio_postgres::NoTls;
use support::*;

#[tokio::test]
async fn runtime_cannot_access_chain_tables_or_sequence() {
    let h = installed().await;
    let cfg = h.read_only_role().await.unwrap();
    let role = cfg.get_user().unwrap();
    let mut tx = begin(&h).await;
    tx.grant_event_runtime_role(role).await.unwrap();
    tx.commit().await.unwrap();
    let (runtime, conn) = cfg.connect(NoTls).await.unwrap();
    tokio::spawn(conn);
    for table in ["ir_audit_chain", "ir_audit_seal"] {
        for sql in [
            format!("SELECT * FROM erp_event.{table}"),
            format!("INSERT INTO erp_event.{table} DEFAULT VALUES"),
            format!("UPDATE erp_event.{table} SET format_version=1"),
            format!("DELETE FROM erp_event.{table}"),
            format!("TRUNCATE erp_event.{table}"),
        ] {
            let e = runtime.batch_execute(&sql).await.unwrap_err();
            assert_eq!(e.code().unwrap().code(), "42501");
            assert_eq!(
                e.as_db_error().unwrap().message(),
                format!("permission denied for table {table}")
            );
        }
        let rights = runtime.query_one("SELECT has_table_privilege($1,'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER,MAINTAIN') OR has_any_column_privilege($1,'SELECT,INSERT,UPDATE,REFERENCES')", &[&format!("erp_event.{table}")]).await.unwrap();
        assert!(!rights.get::<_, bool>(0));
    }
    for sql in [
        "SELECT nextval('erp_event.ir_audit_seal_id_seq')",
        "SELECT currval('erp_event.ir_audit_seal_id_seq')",
        "SELECT setval('erp_event.ir_audit_seal_id_seq',1)",
    ] {
        let e = runtime.query_one(sql, &[]).await.unwrap_err();
        assert_eq!(e.code().unwrap().code(), "42501");
        assert_eq!(
            e.as_db_error().unwrap().message(),
            "permission denied for sequence ir_audit_seal_id_seq"
        );
    }
    let mut tx = begin(&h).await;
    tx.batch(&format!("GRANT SELECT ON erp_event.ir_audit_chain TO {}; GRANT UPDATE(reason) ON erp_event.ir_audit_chain TO {}", erp_sql::quote(role), erp_sql::quote(role))).await.unwrap();
    tx.grant_event_runtime_role(role).await.unwrap();
    tx.commit().await.unwrap();
    let leak: bool = runtime.query_one("SELECT has_any_column_privilege('erp_event.ir_audit_chain','SELECT,INSERT,UPDATE,REFERENCES')", &[]).await.unwrap().get(0);
    assert!(
        !leak,
        "republish must clear column grants as well as table grants"
    );
}

#[tokio::test]
async fn runtime_database_login_cannot_initialize_even_as_system() {
    let h = installed().await;
    let cfg = h.read_only_role().await.unwrap();
    let mut tx = begin(&h).await;
    tx.grant_event_runtime_role(cfg.get_user().unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let orm = erp_orm::Orm::builder(
        std::sync::Arc::clone(h.registry()),
        erp_orm::pool::create_pool(cfg, 2).unwrap(),
    )
    .build()
    .unwrap();
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID)
                .isolation(erp_orm::Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let e = tx
        .initialize_audit_chain(
            &erp_orm::audit::seal::EpochReason::new("operator.initial").unwrap(),
        )
        .await
        .unwrap_err();
    refused(
        &e,
        "a later release: erp_event has incompatible owner; owner repair is required",
    );
    tx.rollback().await.unwrap();
}
