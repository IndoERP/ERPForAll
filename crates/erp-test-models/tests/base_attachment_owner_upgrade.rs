// SPDX-License-Identifier: MIT OR Apache-2.0
//! Additive ownership DDL and effective runtime-role grants.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod attachment_support;
use attachment_support::*;
use erp_testkit::tokio_postgres::{NoTls, error::SqlState};
const HUMAN: &str = include_str!("../../erp-cli/src/human.rs");
fn term() -> &'static str {
    HUMAN
        .split("/*attachment_owner*/")
        .nth(1)
        .unwrap()
        .split("/*end attachment_owner*/")
        .next()
        .unwrap()
}
fn grants(role: &str) -> String {
    HUMAN
        .split("/*attachment_owner_grants*/")
        .nth(1)
        .unwrap()
        .split('"')
        .nth(1)
        .unwrap()
        .replace("{role_sql}", &erp_sql::quote(role))
}

#[tokio::test]
async fn repeated_upgrade_preserves_rows_and_coordinator_intent() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let id = partner(&mut tx, "upgrade").await;
    tx.commit().await.unwrap();
    let owner = f.h.db().client().await.unwrap();
    // Reconstruct the prior schema by removing only the newly added infrastructure. Existing
    // attachment/quota DDL and rows remain; this is not a previous-binary rehearsal.
    owner
        .batch_execute("DROP TABLE ir_blob_owner_state; DROP TABLE ir_blob_owner_identity")
        .await
        .unwrap();
    owner
        .batch_execute(erp_addon_base::attachments::schema::BLOB_DDL)
        .await
        .unwrap();
    assert_eq!(
        owner
            .query_one("SELECT count(*) FROM ir_blob_owner_state", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    let mut tx =
        f.h.orm()
            .begin_opts(
                erp_orm::BeginOptions::new(erp_orm::PrincipalHint::system())
                    .isolation(erp_orm::Isolation::ReadCommitted),
            )
            .await
            .unwrap();
    let p = erp_addon_base::attachments::ownership::prepare(
        &mut tx,
        "main",
        erp_addon_base::attachments::ownership::Purpose::Heartbeat,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    owner
        .batch_execute(erp_addon_base::attachments::schema::BLOB_DDL)
        .await
        .unwrap();
    let pending: Vec<u8> = owner
        .query_one("SELECT pending_marker FROM ir_blob_owner_state", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(pending, p.marker().encode());
    assert_eq!(
        owner
            .query_one("SELECT id FROM res_partner WHERE name='upgrade'", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        id
    );
    // This also violates the expected-version tuple: a create-only intent has no ETag.
    // PostgreSQL checks that cross-field constraint before the inline token constraint.
    let error = owner
        .batch_execute("UPDATE ir_blob_owner_state SET pending_etag='*'")
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::CHECK_VIOLATION));
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "new row for relation \"ir_blob_owner_state\" violates check constraint \"ir_blob_owner_state_check4\""
    );
    let error = owner
        .batch_execute("UPDATE ir_blob_owner_state SET mode='uninitialized'")
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::CHECK_VIOLATION));
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "new row for relation \"ir_blob_owner_state\" violates check constraint \"ir_blob_owner_uninitialized_check\""
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn actual_runtime_role_cannot_mutate_ownership_and_effective_grants_detect_escalation() {
    let f = fixture(external_limits()).await;
    let config = f.h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap();
    let owner = f.h.db().client().await.unwrap();
    owner
        .batch_execute(&format!(
            "GRANT SELECT,INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA public TO {}",
            erp_sql::quote(role)
        ))
        .await
        .unwrap();
    let (runtime, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    let flagged = async || {
        runtime
            .query_one(&format!("SELECT {}", term()), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    };
    assert!(flagged().await);
    owner.batch_execute(&grants(role)).await.unwrap();
    assert!(!flagged().await);
    runtime
        .query("SELECT mode,last_success_at FROM ir_blob_owner_state", &[])
        .await
        .unwrap();
    for sql in [
        "UPDATE ir_blob_owner_state SET mode='healthy'",
        "DELETE FROM ir_blob_owner_state",
        "TRUNCATE ir_blob_owner_state",
        "UPDATE ir_blob_owner_identity SET incarnation=gen_random_uuid()",
    ] {
        let error = runtime.batch_execute(sql).await.unwrap_err();
        assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
        assert_eq!(
            error.as_db_error().unwrap().message(),
            if sql.contains("identity") {
                "permission denied for table ir_blob_owner_identity"
            } else {
                "permission denied for table ir_blob_owner_state"
            }
        );
    }
    for permission in [
        "UPDATE",
        "DELETE",
        "TRUNCATE",
        "REFERENCES",
        "TRIGGER",
        "MAINTAIN",
        "UPDATE(mode)",
    ] {
        owner
            .batch_execute(&format!(
                "GRANT {permission} ON ir_blob_owner_state TO PUBLIC"
            ))
            .await
            .unwrap();
        assert!(flagged().await, "effective PUBLIC {permission}");
        owner.batch_execute("REVOKE ALL ON ir_blob_owner_state FROM PUBLIC; REVOKE UPDATE(mode) ON ir_blob_owner_state FROM PUBLIC").await.unwrap();
        assert!(!flagged().await);
    }
    let inherited = format!("owner_{}", f.h.db().name());
    let inherited_sql = erp_sql::quote(&inherited);
    owner.batch_execute(&format!("CREATE ROLE {inherited_sql} NOLOGIN; GRANT UPDATE ON ir_blob_owner_state TO {inherited_sql}; GRANT {inherited_sql} TO {}", erp_sql::quote(role))).await.unwrap();
    assert!(flagged().await, "inherited write rights are unsafe");
    owner.batch_execute(&format!("REVOKE {inherited_sql} FROM {}; DROP OWNED BY {inherited_sql}; DROP ROLE {inherited_sql}",erp_sql::quote(role))).await.unwrap();
    assert!(!flagged().await);
    owner
        .batch_execute("DROP TABLE ir_blob_owner_state")
        .await
        .unwrap();
    assert!(flagged().await, "half-installed schema refuses publication");
    drop(runtime);
    f.h.cleanup().await.unwrap();
}
