//! the runtime role's privileges on the attachment store registry (`ir_blob_store`),
//! checked as a real login role with the catalogs' effective-rights functions (the grant
//! contract is tested as the role the server runs as;: the server's publication check and
//! its narrowing statement are read from `crates/erp-cli/src/human.rs` as they are written).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod attachment_support;

use attachment_support::*;
use erp_testkit::tokio_postgres::{Client, NoTls, error::SqlState};

const HUMAN_RS: &str = include_str!("../../erp-cli/src/human.rs");

/// The registry term of `MISSING_RUNTIME_GRANTS_SQL`, between its `/*ir_blob_store*/` markers.
fn store_term() -> String {
    let constant = HUMAN_RS
        .split("const MISSING_RUNTIME_GRANTS_SQL: &str = \"")
        .nth(1)
        .expect("the publication predicate");
    let constant = constant.split("\";").next().expect("end of the predicate");
    constant
        .split("/*ir_blob_store*/")
        .nth(1)
        .expect("the ir_blob_store term")
        .split("/*end ir_blob_store*/")
        .next()
        .expect("end marker")
        .to_owned()
}

/// The narrowing statement `grant_runtime` runs, for `role`.
fn narrowing(role: &str) -> String {
    let template = "REVOKE UPDATE, DELETE, TRUNCATE ON public.ir_blob_store FROM {role_sql}";
    assert!(
        HUMAN_RS.contains(template),
        "grant_runtime no longer narrows ir_blob_store with this statement"
    );
    template.replace("{role_sql}", &erp_sql::quote(role))
}

async fn flags(client: &Client) -> bool {
    client
        .query_one(&format!("SELECT {}", store_term()), &[])
        .await
        .unwrap()
        .get(0)
}

/// The runtime role, after the server's blanket grant and its narrowing, registers a store but
/// cannot rewrite or remove one; the publication check passes that state and flags every
/// deviation.
#[tokio::test]
async fn the_runtime_role_registers_stores_and_never_rewrites_or_removes_one() {
    let f = fixture(external_limits()).await;
    let config = f.h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    let password = String::from_utf8(config.get_password().unwrap().to_vec()).unwrap();
    let owner = f.h.db().client().await.unwrap();
    let quoted = erp_sql::quote(&role);
    // the blanket grant of `grant_runtime`, then its narrowing
    owner
        .batch_execute(&format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {quoted}; \
             GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO {quoted}"
        ))
        .await
        .unwrap();
    let mut login = f.h.db().config();
    login.user(&role).password(&password);
    let (runtime, connection) = login.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    assert!(flags(&runtime).await, "the blanket CRUD grant is flagged");
    owner.batch_execute(&narrowing(&role)).await.unwrap();
    assert!(!flags(&runtime).await, "the narrowed state passes");
    // as the role: register a store, read it, and nothing else
    runtime
        .batch_execute(
            "INSERT INTO ir_blob_store (store_key, fingerprint) VALUES ('grant-test', 'fp')",
        )
        .await
        .unwrap();
    let rows = runtime
        .query(
            "SELECT namespace::text FROM ir_blob_store WHERE store_key = 'grant-test'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    for statement in [
        "UPDATE ir_blob_store SET fingerprint = 'other' WHERE store_key = 'grant-test'",
        "UPDATE ir_blob_store SET namespace = namespace WHERE store_key = 'grant-test'",
        "DELETE FROM ir_blob_store WHERE store_key = 'grant-test'",
        "TRUNCATE ir_blob_store CASCADE",
    ] {
        let error = runtime.batch_execute(statement).await.unwrap_err();
        assert_eq!(
            error.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{statement}"
        );
    }
    let deviations = [
        (
            "table UPDATE",
            format!("GRANT UPDATE ON ir_blob_store TO {quoted}"),
        ),
        (
            "table DELETE",
            format!("GRANT DELETE ON ir_blob_store TO {quoted}"),
        ),
        (
            "table TRUNCATE",
            format!("GRANT TRUNCATE ON ir_blob_store TO {quoted}"),
        ),
        (
            "column UPDATE",
            format!("GRANT UPDATE (fingerprint) ON ir_blob_store TO {quoted}"),
        ),
        (
            "PUBLIC UPDATE",
            "GRANT UPDATE ON ir_blob_store TO PUBLIC".to_owned(),
        ),
        (
            "missing INSERT",
            format!("REVOKE INSERT ON ir_blob_store FROM {quoted}"),
        ),
        (
            "missing SELECT",
            format!("REVOKE SELECT ON ir_blob_store FROM {quoted}"),
        ),
    ];
    for (what, change) in deviations {
        owner.batch_execute(&change).await.unwrap();
        assert!(flags(&runtime).await, "{what} must be flagged");
        owner
            .batch_execute(&format!(
                "REVOKE ALL ON ir_blob_store FROM PUBLIC; \
                 REVOKE UPDATE (fingerprint) ON ir_blob_store FROM {quoted}; \
                 GRANT SELECT, INSERT ON ir_blob_store TO {quoted}; {}",
                narrowing(&role)
            ))
            .await
            .unwrap();
        assert!(!flags(&runtime).await, "repairing {what} passes");
    }
    drop(runtime);
    drop(owner);
    f.h.cleanup().await.unwrap();
}
