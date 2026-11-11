//! Native key storage regressions; issuance and expiry policy are exercised separately.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;

#[tokio::test]
async fn schema_accepts_restored_constraints() {
    let h = support::harness().await;
    let c = support::connect(&h).await;
    for _ in 0..3 {
        support::restore_api_key_constraints(&c).await;
        let report = h.orm().sync_schema().await.unwrap();
        assert!(report.is_empty(), "{report:?}");
    }
    c.batch_execute("ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_kind; ALTER TABLE res_users_apikeys ADD CONSTRAINT apikey_kind CHECK (TRUE)").await.unwrap();
    support::restore_api_key_constraints(&c).await;
    let error = h.orm().sync_schema().await.unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Database);
    assert_eq!(
        error.to_string(),
        "completing (after the models) the core table 'res_users_apikeys_private' of 'erp-security' failed: a later release constraint apikey_kind differs"
    );
}

#[tokio::test]
async fn schema_sync_keeps_private_digest() {
    let h = support::harness().await;
    let c = support::connect(&h).await;
    assert!(h.registry().model("res.users.apikeys").is_some());
    for _ in 0..2 {
        let report = h.orm().sync_schema().await.unwrap();
        assert!(report.is_empty(), "{report:?}");
        let row = c.query_one("SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'res_users_apikeys'::regclass AND attname = 'digest' AND NOT attisdropped", &[]).await.unwrap();
        assert_eq!(row.get::<_, String>(0), "bytea");
    }
}

#[tokio::test]
async fn schema_refuses_wrong_digest_type() {
    let h = support::harness().await;
    let c = support::connect(&h).await;
    c.batch_execute("ALTER TABLE res_users_apikeys DROP COLUMN digest CASCADE; ALTER TABLE res_users_apikeys ADD COLUMN digest text").await.unwrap();
    let error = h.orm().sync_schema().await.unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Database);
    assert_eq!(
        error.to_string(),
        "completing (after the models) the core table 'res_users_apikeys_private' of 'erp-security' failed: a later release requires res_users_apikeys.digest bytea"
    );
}

#[tokio::test]
async fn schema_refuses_weakened_constraint() {
    let h = support::harness().await;
    let c = support::connect(&h).await;
    c.batch_execute("ALTER TABLE res_users_apikeys DROP CONSTRAINT apikey_digest_length; ALTER TABLE res_users_apikeys ADD CONSTRAINT apikey_digest_length CHECK (TRUE)").await.unwrap();
    let error = h.orm().sync_schema().await.unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Database);
    assert_eq!(
        error.to_string(),
        "completing (after the models) the core table 'res_users_apikeys_private' of 'erp-security' failed: a later release constraint apikey_digest_length differs"
    );
}

#[tokio::test]
async fn generic_mutations_refuse_even_for_system() {
    use erp_security::auth::{
        apikey::ApiKeyHasher,
        apikey_store::{IssueRequest, KeyKind, KeyScope, issue},
        secret::DatabaseSecret,
    };
    let h = support::harness().await;
    let mut tx = h.begin().await.unwrap();
    let keys = tx.env().records("res.users.apikeys").unwrap();
    let error = keys
        .create_values(&mut tx, &[Default::default()])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        error.to_string(),
        "API keys must be created through a later release issuance"
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // Exercise the refusal against a real committed credential. Empty writes
    // alone could hide a bypass that accepts nonempty authority/metadata edits.
    let c = support::connect(&h).await;
    c.batch_execute("UPDATE res_users SET active=true WHERE id=1; INSERT INTO res_groups(id,name) VALUES(10,'Settings'); INSERT INTO ir_model_data(module,name,model,res_id) VALUES('base','group_system','res.groups',10); INSERT INTO res_groups_users_rel(uid,gid) VALUES(1,10)").await.unwrap();
    let hash = ApiKeyHasher::derive(
        &DatabaseSecret::from_stored("0123456789abcdef0123456789abcdef").unwrap(),
    )
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let request = IssueRequest::new(
        erp_orm::UserId::new(1).unwrap(),
        "Immutable credential",
        KeyKind::User,
        KeyScope::Rpc,
        None,
    )
    .unwrap();
    let issued = issue(&mut tx, &hash, &request).await.unwrap();
    let id = issued.id().get();
    tx.commit().await.unwrap();
    drop(tx);
    // The secret is never exposed or printed by this storage regression.
    drop(issued);
    let before: String = c
        .query_one(
            "SELECT row_to_json(k)::text FROM res_users_apikeys k WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()
        .get(0);
    for vals in [
        erp_orm::RawVals::new(),
        erp_orm::RawVals::new()
            .set_cell("name", erp_fields::Cell::Str("Changed".into()))
            .set_cell("read_only", erp_fields::Cell::Bool(true)),
    ] {
        let mut tx = h.begin().await.unwrap();
        let keys = tx
            .env()
            .records("res.users.apikeys")
            .unwrap()
            .browse([id])
            .unwrap();
        let error = keys.write(&mut tx, vals).await.unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
        assert_eq!(
            error.to_string(),
            "API key authority and credentials are immutable"
        );
        tx.rollback().await.unwrap();
    }
    let mut tx = h.begin().await.unwrap();
    let keys = tx
        .env()
        .records("res.users.apikeys")
        .unwrap()
        .browse([id])
        .unwrap();
    let error = keys.unlink(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        error.to_string(),
        "API keys must be revoked through the dedicated revoke method"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let after: String = c
        .query_one(
            "SELECT row_to_json(k)::text FROM res_users_apikeys k WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        after, before,
        "generic mutations must leave the stored credential unchanged"
    );
}

/// Database-isolation preflight, not API-key feature acceptance. Demonstrates why
/// a lock-only issuance count is unsafe under the ORM's default Repeatable Read.
#[tokio::test]
async fn repeatable_read_needs_a_subject_version_fence() {
    let h = support::harness().await;
    let a = support::connect(&h).await;
    let b = support::connect(&h).await;
    a.batch_execute("CREATE TABLE key_fence_subject (id bigint PRIMARY KEY); CREATE TABLE key_fence_rows (id bigint); INSERT INTO key_fence_subject VALUES (1)").await.unwrap();
    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT id FROM key_fence_subject WHERE id = 1 FOR UPDATE").await.unwrap();
    b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    let before: i64 = b
        .query_one("SELECT count(*) FROM key_fence_rows", &[])
        .await
        .unwrap()
        .get(0);
    a.batch_execute("INSERT INTO key_fence_rows VALUES (1); COMMIT")
        .await
        .unwrap();
    b.batch_execute("SELECT id FROM key_fence_subject WHERE id = 1 FOR UPDATE")
        .await
        .unwrap();
    let stale: i64 = b
        .query_one("SELECT count(*) FROM key_fence_rows", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(before, stale, "a row lock did not refresh the RR snapshot");
    b.batch_execute("ROLLBACK").await.unwrap();
    let committed: i64 = b
        .query_one("SELECT count(*) FROM key_fence_rows", &[])
        .await
        .unwrap()
        .get(0);
    assert!(
        committed > stale,
        "the later committed row is invisible to the old snapshot"
    );

    a.batch_execute(
        "BEGIN ISOLATION LEVEL REPEATABLE READ; UPDATE key_fence_subject SET id = id WHERE id = 1",
    )
    .await
    .unwrap();
    b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM key_fence_rows")
        .await
        .unwrap();
    a.batch_execute("INSERT INTO key_fence_rows VALUES (2); COMMIT")
        .await
        .unwrap();
    let error = b
        .batch_execute("SELECT id FROM key_fence_subject WHERE id = 1 FOR UPDATE")
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        Some(&erp_testkit::tokio_postgres::error::SqlState::T_R_SERIALIZATION_FAILURE)
    );
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "could not serialize access due to concurrent update"
    );
    b.batch_execute("ROLLBACK").await.unwrap();
}
