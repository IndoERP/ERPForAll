// SPDX-License-Identifier: MIT OR Apache-2.0
//! Executes the actual event branch of the publication predicate against real owner/runtime roles.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_test_models::base_users;
use erp_testkit::{Harness, tokio_postgres::NoTls};

#[tokio::test]
async fn publication_predicate_detects_direct_public_inherited_and_default_acl_leaks() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let cfg = h.read_only_role().await.unwrap();
    let role = cfg.get_user().unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_audit_chain_schema().await.unwrap();
    tx.grant_event_runtime_role(role).await.unwrap();
    tx.commit().await.unwrap();
    let (runtime, conn) = cfg.connect(NoTls).await.unwrap();
    tokio::spawn(conn);
    // Use the exact block shipped by the CLI; this is not a hand-maintained copy of its SQL.
    let source = include_str!("../src/human.rs");
    let sql = source
        .split("OR /*erp_event*/ ")
        .nth(1)
        .unwrap()
        .split("/*end erp_event*/")
        .next()
        .unwrap();
    let sql = format!("SELECT {sql}");
    assert!(
        !runtime
            .query_one(&sql, &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let c = h.db().client().await.unwrap();
    let inherited = format!("{}_chain_leak", h.db().name());
    c.batch_execute(&format!(
        "CREATE ROLE {} NOLOGIN; GRANT {} TO {}",
        erp_sql::quote(&inherited),
        erp_sql::quote(&inherited),
        erp_sql::quote(role)
    ))
    .await
    .unwrap();
    for grantee in [
        erp_sql::quote(role),
        "PUBLIC".to_owned(),
        erp_sql::quote(&inherited),
    ] {
        for grant in [
            "SELECT ON erp_event.ir_audit_chain",
            "UPDATE(reason) ON erp_event.ir_audit_chain",
            "USAGE ON SEQUENCE erp_event.ir_audit_seal_id_seq",
        ] {
            c.batch_execute(&format!("GRANT {grant} TO {grantee}"))
                .await
                .unwrap();
            assert!(
                runtime
                    .query_one(&sql, &[])
                    .await
                    .unwrap()
                    .get::<_, bool>(0),
                "{grant}"
            );
            c.batch_execute(&format!("REVOKE {grant} FROM {grantee}"))
                .await
                .unwrap();
        }
    }
    // Default ACLs are effective privileges on newly created chain objects too.
    c.batch_execute("DROP TABLE erp_event.ir_audit_seal; DROP TABLE erp_event.ir_audit_chain")
        .await
        .unwrap();
    c.batch_execute(&format!(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA erp_event GRANT SELECT ON TABLES TO {}",
        erp_sql::quote(role)
    ))
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_audit_chain_schema().await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        runtime
            .query_one(&sql, &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let mut tx = h.begin().await.unwrap();
    tx.grant_event_runtime_role(role).await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        !runtime
            .query_one(&sql, &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    c.batch_execute(&format!("ALTER DEFAULT PRIVILEGES IN SCHEMA erp_event REVOKE SELECT ON TABLES FROM {}; REVOKE {} FROM {}; DROP ROLE {}", erp_sql::quote(role), erp_sql::quote(&inherited), erp_sql::quote(role), erp_sql::quote(&inherited))).await.unwrap();
}
