// SPDX-License-Identifier: MIT OR Apache-2.0
//! Peer evidence authority; same-host tests are not actual shared-mount certification.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod attachment_support;
use attachment_support::*;
use erp_addon_base::attachments::peer_probe::{self, Participant};
use erp_orm::{BeginOptions, Isolation, PrincipalHint};
async fn begin(f: &Fixture) -> erp_sdk::Transaction {
    f.h.orm()
        .begin_opts(
            BeginOptions::new(PrincipalHint::system())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                .isolation(Isolation::ReadCommitted),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn absent_peer_evidence_refuses_and_install_creates_no_success() {
    let f = fixture(external_limits()).await;
    let mut tx = begin(&f).await;
    let who = Participant::new("node-a", "host-a", &"a".repeat(64)).unwrap();
    let e = peer_probe::admit(&mut tx, "main", &who).await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_peer_probe").await,
        0
    );
    f.h.cleanup().await.unwrap();
}

#[test]
fn participant_rejects_unbounded_and_malformed_fields() {
    for (node, host, mount) in [
        ("", "host", "a".repeat(64)),
        ("node", "", "a".repeat(64)),
        ("node", "host", "x".repeat(64)),
    ] {
        let e = Participant::new(node, host, &mount).unwrap_err();
        assert_eq!(e.kind(), erp_sdk::ErrorKind::Validation);
        assert_eq!(e.message(), "Invalid attachment peer participant binding");
    }
}

#[tokio::test]
async fn complete_pair_is_audited_and_changed_binding_refuses() {
    use erp_blob::{
        AttachmentRuntime, AttachmentStore, LocalStore,
        peer::{LocalPeer, PEER_STEPS},
    };
    use erp_testkit::Harness;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(dir.path()).await.unwrap());
    let rt = Arc::new(AttachmentRuntime::single(
        "main",
        store.clone() as Arc<dyn AttachmentStore>,
        external_limits(),
    ));
    let h = Harness::builder([&erp_test_models::base_users::ADDON, &erp_addon_base::ADDON])
        .attach(rt)
        .build()
        .await
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(PrincipalHint::system())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                .isolation(Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    erp_addon_base::attachments::ownership::prepare(
        &mut tx,
        "main",
        erp_addon_base::attachments::ownership::Purpose::Heartbeat,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mount = store.peer_binding().unwrap();
    let a = Participant::new("node-a", "host-a", &mount).unwrap();
    let b = Participant::new("node-b", "host-b", &mount).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(PrincipalHint::system())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                .isolation(Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let session = peer_probe::start(&mut tx, "main", &store.fingerprint().unwrap(), &a, 30)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut left = LocalPeer::create(store.clone(), session.clone()).unwrap();
    let mut right = LocalPeer::join(store.clone(), session.clone()).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(PrincipalHint::system())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                .isolation(Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let same_host = Participant::new("node-b", "host-a", &mount).unwrap();
    let e = peer_probe::join(&mut tx, "main", &session, &same_host)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "Attachment peer probe requires two distinct nodes on distinct hosts"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(PrincipalHint::system())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                .isolation(Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    peer_probe::join(&mut tx, "main", &session, &b)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    for phase in 0..PEER_STEPS {
        for (who, local) in [(&a, &mut left), (&b, &mut right)] {
            let observation = local.step(phase).await.unwrap();
            let mut tx = h
                .orm()
                .begin_opts(
                    BeginOptions::new(PrincipalHint::system())
                        .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                        .isolation(Isolation::ReadCommitted),
                )
                .await
                .unwrap();
            peer_probe::record(&mut tx, "main", who, &observation)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            drop(tx);
            if phase == 0 {
                let mut tx = h
                    .orm()
                    .begin_opts(
                        BeginOptions::new(PrincipalHint::system())
                            .isolation(Isolation::ReadCommitted),
                    )
                    .await
                    .unwrap();
                let e = peer_probe::record(&mut tx, "main", who, &observation)
                    .await
                    .unwrap_err();
                assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
                assert_eq!(
                    e.message(),
                    "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes."
                );
                tx.rollback().await.unwrap();
                drop(tx);
            }
        }
    }
    drop(right);
    let cleaned = left.cleanup().unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(PrincipalHint::system())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                .isolation(Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    peer_probe::finalize(&mut tx, "main", &a, cleaned)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(PrincipalHint::system())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                .isolation(Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    peer_probe::admit(&mut tx, "main", &a).await.unwrap();
    assert_eq!(count(&mut tx,"SELECT count(*) FROM erp_event.ir_audit_entry e CROSS JOIN LATERAL jsonb_array_elements(e.events) j WHERE j->>'event'='attachment.peer_probe' AND j->>'outcome'='verified'").await,1);
    assert_eq!(count(&mut tx,"SELECT count(*) FROM erp_event.ir_audit_entry e CROSS JOIN LATERAL jsonb_array_elements(e.events) j WHERE j->>'event'='attachment.peer_probe' AND j->>'outcome'='verified' AND j->'participants'='[\"node-a\",\"node-b\"]'::jsonb").await,1);
    tx.rollback().await.unwrap();
    drop(tx);
    let config=erp_blob::AttachmentConfig::from_toml(&format!("[attachments]\nwrite_store='main'\n[attachments.stores.main]\nkind='local'\npath={}\nshared_filesystem=false\n",serde_json::to_string(dir.path().to_str().unwrap()).unwrap())).unwrap();
    let unshared = Arc::new(AttachmentRuntime::open(config).await.unwrap());
    let changed = h
        .orm()
        .rebuild(Arc::clone(h.registry()))
        .generation_attachment(erp_orm::GenerationAttachment::new(
            Arc::clone(h.registry()),
            unshared,
        ))
        .build()
        .unwrap();
    let mut changed_tx = changed
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let e = peer_probe::admit(&mut changed_tx, "main", &a)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes."
    );
    changed_tx.rollback().await.unwrap();
    drop(changed_tx);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let outsider = Participant::new("node-c", "host-c", &mount).unwrap();
    let e = peer_probe::admit(&mut tx, "main", &outsider)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    std::fs::remove_file(dir.path().join(".store-id")).unwrap();
    std::fs::write(
        dir.path().join(".store-id"),
        erp_blob::Namespace::random().unwrap().as_str(),
    )
    .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(PrincipalHint::system())
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap())
                .isolation(Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let e = peer_probe::admit(&mut tx, "main", &a).await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn peer_authority_is_owner_written_and_effective_privileges_are_checked() {
    use erp_testkit::tokio_postgres::{NoTls, error::SqlState};
    let f = fixture(external_limits()).await;
    let config = f.h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap();
    let owner = f.h.db().client().await.unwrap();
    let source = include_str!("../../erp-cli/src/human.rs");
    let term = source
        .split("/*attachment_owner*/")
        .nth(1)
        .unwrap()
        .split("/*end attachment_owner*/")
        .next()
        .unwrap();
    let narrowing = source
        .split("/*attachment_owner_grants*/")
        .nth(1)
        .unwrap()
        .split('"')
        .nth(1)
        .unwrap()
        .replace("{role_sql}", &erp_sql::quote(role));
    owner
        .batch_execute(&format!(
            "GRANT SELECT,INSERT,UPDATE,DELETE ON ir_blob_peer_probe TO {}",
            erp_sql::quote(role)
        ))
        .await
        .unwrap();
    let (client, connection) = config.connect(NoTls).await.unwrap();
    tokio::spawn(connection);
    let flag = async || {
        client
            .query_one(&format!("SELECT {term}"), &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    };
    assert!(flag().await);
    owner.batch_execute(&narrowing).await.unwrap();
    assert!(!flag().await);
    for sql in [
        "UPDATE ir_blob_peer_probe SET status='verified'",
        "DELETE FROM ir_blob_peer_probe",
        "TRUNCATE ir_blob_peer_probe",
    ] {
        let e = client.batch_execute(sql).await.unwrap_err();
        assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
        assert_eq!(
            e.as_db_error().unwrap().message(),
            "permission denied for table ir_blob_peer_probe"
        );
    }
    for permission in [
        "UPDATE",
        "DELETE",
        "TRUNCATE",
        "REFERENCES",
        "TRIGGER",
        "MAINTAIN",
        "UPDATE(status)",
    ] {
        owner
            .batch_execute(&format!(
                "GRANT {permission} ON ir_blob_peer_probe TO PUBLIC"
            ))
            .await
            .unwrap();
        assert!(flag().await);
        owner.batch_execute("REVOKE ALL ON ir_blob_peer_probe FROM PUBLIC;REVOKE UPDATE(status) ON ir_blob_peer_probe FROM PUBLIC").await.unwrap();
        assert!(!flag().await);
    }
    owner
        .batch_execute("DROP TABLE ir_blob_peer_probe")
        .await
        .unwrap();
    assert!(flag().await);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn audit_failure_cannot_publish_a_new_peer_attempt() {
    use erp_addon_base::attachments::ownership::{self, Purpose};
    use erp_blob::AttachmentStore;
    let f = fixture(external_limits()).await;
    let mut tx = begin(&f).await;
    ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let owner = f.h.db().client().await.unwrap();
    let partition: String = owner
        .query_one(
            "SELECT 'ir_audit_entry_p' || to_char((now() AT TIME ZONE 'UTC')::date,'YYYYMMDD')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    owner
        .batch_execute(&format!(
            "DROP TABLE erp_event.{}",
            erp_sql::quote(&partition)
        ))
        .await
        .unwrap();
    let who = Participant::new("node-a", "host-a", &"a".repeat(64)).unwrap();
    let mut tx = begin(&f).await;
    peer_probe::start(&mut tx, "main", &f.store.fingerprint().unwrap(), &who, 30)
        .await
        .unwrap();
    let e = tx.commit().await.unwrap_err();
    drop(tx);
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "a later release (insertion): the audit INSERT failed (SQLSTATE 23514); no partition of erp_event.ir_audit_entry covers the transaction's day; the event partition upkeep has not run (it runs at server start and hourly; `erp audit maintain` refreshes both event tables)"
    );
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_peer_probe").await,
        0
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upgrade_expiry_replacement_and_failure_never_invent_admission() {
    use erp_addon_base::attachments::ownership::{self, Purpose};
    use erp_blob::AttachmentStore;
    let f = fixture(external_limits()).await;
    let mut tx = begin(&f).await;
    let pending = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let owner = f.h.db().client().await.unwrap();
    owner
        .batch_execute("DROP TABLE ir_blob_peer_probe")
        .await
        .unwrap();
    owner
        .batch_execute(erp_addon_base::attachments::schema::BLOB_DDL)
        .await
        .unwrap();
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_peer_probe").await,
        0
    );
    let who = Participant::new("node-a", "host-a", &"a".repeat(64)).unwrap();
    let mut tx = begin(&f).await;
    let first = peer_probe::start(&mut tx, "main", &f.store.fingerprint().unwrap(), &who, 30)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    owner
        .batch_execute(erp_addon_base::attachments::schema::BLOB_DDL)
        .await
        .unwrap();
    let row=owner.query_one("SELECT p.session::text,o.pending_marker FROM ir_blob_peer_probe p JOIN ir_blob_owner_state o ON o.store_id=p.store_id",&[]).await.unwrap();
    assert_eq!(row.get::<_, String>(0), first.as_str());
    assert_eq!(row.get::<_, Vec<u8>>(1), pending.marker().encode());
    owner
        .batch_execute(
            "UPDATE ir_blob_peer_probe SET deadline=clock_timestamp()-interval '1 second'",
        )
        .await
        .unwrap();
    let other = Participant::new("node-b", "host-b", &"b".repeat(64)).unwrap();
    let mut tx = begin(&f).await;
    let e = peer_probe::join(&mut tx, "main", &first, &other)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = begin(&f).await;
    assert_eq!(
        peer_probe::status(&mut tx, "main").await.unwrap(),
        "expired"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = begin(&f).await;
    let second = peer_probe::start(&mut tx, "main", &f.store.fingerprint().unwrap(), &who, 30)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_ne!(first, second);
    let mut tx = begin(&f).await;
    let e = peer_probe::join(&mut tx, "main", &first, &other)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = begin(&f).await;
    peer_probe::fail(&mut tx, "main", &second, &who)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = begin(&f).await;
    assert_eq!(peer_probe::status(&mut tx, "main").await.unwrap(), "failed");
    let e = peer_probe::admit(&mut tx, "main", &who).await.unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment shared-filesystem peer evidence is missing or invalid; run erp attachments probe-store --peer on both nodes."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
