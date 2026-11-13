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

#[tokio::test]
async fn quota_accounting_is_append_only_and_requests_cannot_remove_content() {
    let f = fixture(external_limits()).await;
    let config = f.h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    let owner = f.h.db().client().await.unwrap();
    let quoted = erp_sql::quote(&role);
    owner.batch_execute(&format!("GRANT SELECT,INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA public TO {quoted}; GRANT USAGE,SELECT ON ALL SEQUENCES IN SCHEMA public TO {quoted}")).await.unwrap();
    let grants = HUMAN_RS
        .split("/*attachment_quota_grants*/")
        .nth(1)
        .expect("quota grant narrowing")
        .split("/*end attachment_quota_grants*/")
        .next()
        .unwrap();
    let grants = grants
        .split('"')
        .nth(1)
        .unwrap()
        .replace("{role_sql}", &quoted)
        .replace("\\\n", "");
    owner.batch_execute(&grants).await.unwrap();
    let (runtime, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    let check = HUMAN_RS
        .split("/*attachment_quota*/")
        .nth(1)
        .expect("quota effective-rights predicate")
        .split("/*end attachment_quota*/")
        .next()
        .unwrap();
    let flags = || format!("SELECT {check}");
    assert!(
        !runtime
            .query_one(&flags(), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    runtime.batch_execute("INSERT INTO ir_blob(sha256,size) VALUES(decode(repeat('ab',32),'hex'),7); INSERT INTO ir_blob_upload(subject_user,expires_at,reserved_bytes) VALUES(7,now()+interval '1 day',11)").await.unwrap();
    assert_eq!(
        runtime
            .query_one("SELECT sum(bytes)::int8 FROM ir_blob_usage_delta", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        18
    );
    let orm = erp_orm::Orm::builder(
        std::sync::Arc::clone(f.h.registry()),
        erp_orm::pool::create_pool(config.clone(), 4).unwrap(),
    )
    .policy(erp_orm::StaticPolicy::AllowAll)
    .generation_attachment(erp_orm::GenerationAttachment::new(
        std::sync::Arc::clone(f.h.registry()),
        std::sync::Arc::clone(&f.rt),
    ))
    .build()
    .unwrap();
    let mut tx = orm.begin(erp_orm::PrincipalHint::system()).await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let payload = b"runtime-role attachment";
    let up = erp_addon_base::attachments::Uploader::new(&orm)
        .stage(
            &env,
            &mut payload.as_slice(),
            erp_addon_base::attachments::StageRequest::named("runtime.txt")
                .with_declared_length(payload.len() as u64),
        )
        .await
        .unwrap();
    let mut tx = orm.begin(erp_orm::PrincipalHint::system()).await.unwrap();
    let ids = erp_addon_base::attachments::create(
        &mut tx,
        vec![erp_addon_base::attachments::NewAttachment::new(
            "runtime.txt",
            erp_addon_base::attachments::AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(erp_orm::PrincipalHint::system()).await.unwrap();
    assert_eq!(
        erp_addon_base::attachments::open(&mut tx, ids[0].get())
            .await
            .unwrap()
            .read_to_vec(100)
            .await
            .unwrap(),
        payload
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    for sql in [
        "UPDATE ir_blob_usage_base SET bytes=0",
        "UPDATE ir_blob_usage_delta SET bytes=0",
        "DELETE FROM ir_blob_usage_delta",
        "DELETE FROM ir_blob",
        "DELETE FROM ir_blob_upload",
        "TRUNCATE ir_blob_usage_delta",
        "UPDATE ir_blob_upload SET subject_user=8",
        "UPDATE ir_blob SET size=0",
    ] {
        let err = runtime.batch_execute(sql).await.unwrap_err();
        assert_eq!(err.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{sql}");
        assert!(
            err.as_db_error()
                .unwrap()
                .message()
                .contains("permission denied"),
            "{err}"
        );
    }
    for sql in [
        format!("GRANT UPDATE(bytes) ON ir_blob_usage_delta TO {quoted}"),
        "GRANT DELETE ON ir_blob TO PUBLIC".to_owned(),
    ] {
        owner.batch_execute(&sql).await.unwrap();
        assert!(
            runtime
                .query_one(&flags(), &[])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        owner.batch_execute(&format!("REVOKE UPDATE(bytes) ON ir_blob_usage_delta FROM {quoted}; REVOKE DELETE ON ir_blob FROM PUBLIC")).await.unwrap();
        assert!(
            !runtime
                .query_one(&flags(), &[])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
    }
    drop(runtime);
    drop(owner);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_guard_allows_generation_updates_but_never_identity_or_deletion() {
    let f = fixture(limits()).await;
    let config = f.h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    let quoted = erp_sql::quote(&role);
    let owner = f.h.db().client().await.unwrap();
    owner
        .batch_execute(&format!(
            "GRANT SELECT,INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA public TO {quoted}"
        ))
        .await
        .unwrap();
    let grant_template = HUMAN_RS
        .split("/*attachment_pending_grants*/")
        .nth(1)
        .expect("pending grants")
        .split("/*end attachment_pending_grants*/")
        .next()
        .unwrap();
    let grants = grant_template
        .split('"')
        .nth(1)
        .unwrap()
        .replace("{role_sql}", &quoted)
        .replace("\\\n", "");
    let predicate = HUMAN_RS
        .split("/*attachment_pending*/")
        .nth(1)
        .expect("pending predicate")
        .split("/*end attachment_pending*/")
        .next()
        .unwrap();
    let (request, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    let flags = || format!("SELECT {predicate}");
    assert!(
        request
            .query_one(&flags(), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    owner.batch_execute(&grants).await.unwrap();
    assert!(
        !request
            .query_one(&flags(), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    request.batch_execute("INSERT INTO ir_attachment_pending_guard(subject_user,placeholder_model) VALUES (1,'test.pending'); UPDATE ir_attachment_pending_guard SET generation=NOT generation WHERE subject_user=1").await.unwrap();
    for sql in [
        "UPDATE ir_attachment_pending_guard SET subject_user=2",
        "DELETE FROM ir_attachment_pending_guard",
        "TRUNCATE ir_attachment_pending_guard",
    ] {
        let error = request.batch_execute(sql).await.unwrap_err();
        assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
        assert!(
            error
                .as_db_error()
                .unwrap()
                .message()
                .contains("permission denied")
        );
    }
    owner
        .batch_execute("GRANT UPDATE ON ir_attachment_pending_guard TO PUBLIC")
        .await
        .unwrap();
    assert!(
        request
            .query_one(&flags(), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    owner
        .batch_execute("REVOKE UPDATE ON ir_attachment_pending_guard FROM PUBLIC")
        .await
        .unwrap();
    drop(request);
    drop(owner);
    f.h.cleanup().await.unwrap();
}
