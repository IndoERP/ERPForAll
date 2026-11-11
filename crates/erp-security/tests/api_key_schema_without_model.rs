//! Separate link root: deliberately does not link the base addon/model/hooks.
#![allow(clippy::unwrap_used, reason = "tests")]

#[tokio::test]
async fn registry_without_key_model_skips_private_ddl() {
    let h = erp_testkit::Harness::builder([])
        .require_core_tables(erp_security::auth::core_tables())
        .linked(false)
        .build()
        .await
        .unwrap();
    let c = h.db().client().await.unwrap();
    let row = c
        .query_one("SELECT to_regclass('res_users_apikeys')::text", &[])
        .await
        .unwrap();
    assert!(row.get::<_, Option<String>>(0).is_none());
    let mut tx = h.begin().await.unwrap();
    let hasher = erp_security::auth::apikey::ApiKeyHasher::derive(
        &erp_security::auth::secret::DatabaseSecret::from_stored(
            "0123456789abcdef0123456789abcdef",
        )
        .unwrap(),
    )
    .unwrap();
    use erp_security::auth::apikey_store::{IssueRequest, KeyKind, KeyScope, issue};
    let request = IssueRequest::new(
        erp_orm::UserId::new(20).unwrap(),
        "label",
        KeyKind::User,
        KeyScope::Rpc,
        None,
    )
    .unwrap();
    let error = issue(&mut tx, &hasher, &request).await.err().unwrap();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        "a later release requires res.users.apikeys model"
    );
}
