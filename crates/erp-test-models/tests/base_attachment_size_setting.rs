// SPDX-License-Identifier: MIT OR Apache-2.0
//! The database file policy is enforced independently of client checks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod attachment_support;
use attachment_support::*;
use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment};
use erp_sdk::{Cell, ErrorKind, RawVals, Transaction};

const KEY: &str = "base.attachment_max_bytes";
async fn set(tx: &mut Transaction, value: &str) {
    let _: String = tx
        .env()
        .records("ir.config_parameter")
        .unwrap()
        .call(tx, "set_str", (KEY.to_owned(), Some(value.to_owned())))
        .await
        .unwrap();
}

#[tokio::test]
async fn database_limit_refuses_bytes_and_base64_without_client_cooperation() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    set(&mut tx, "4").await;
    tx.commit().await.unwrap();
    for content in [
        AttachmentContent::Bytes(b"12345".to_vec()),
        AttachmentContent::Base64("MTIzNDU=".into()),
    ] {
        let mut tx = f.h.begin().await.unwrap();
        let error = tx
            .savepoint(async |tx| {
                attachments::create(tx, vec![NewAttachment::new("file.txt", content)]).await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "The attachment is larger than the limit of 4 bytes (base.attachment_max_bytes)."
        );
        assert_eq!(
            count(&mut tx, "SELECT count(*) FROM ir_attachment").await,
            0
        );
        assert_eq!(count(&mut tx, "SELECT count(*) FROM ir_blob").await, 0);
        tx.rollback().await.unwrap();
    }
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(&mut tx, vec![bytes_attachment("exact.txt", b"1234", None)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    set(&mut tx, "5").await;
    tx.commit().await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![bytes_attachment("raised.txt", b"12345", None)],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn technical_parameter_refuses_invalid_size_policy() {
    let f = fixture(limits()).await;
    for value in ["0", "-1", "2147483648", "1.5", "no", "9223372036854775808"] {
        let mut tx = f.h.begin().await.unwrap();
        let env = tx.env();
        let error = env
            .records("ir.config_parameter")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("key", Cell::Str(KEY.into()))
                        .set_cell("value", Cell::Str(value.into())),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "base.attachment_max_bytes must be an integer from 1 to 2147483647."
        );
        tx.rollback().await.unwrap();
    }
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn reducing_policy_refuses_staged_adoption_and_copy_atomically() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let up = stage(&f, &env, "staged.txt", b"12345").await;
    let other = stage(&f, &env, "original.txt", b"abcde").await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "original.txt",
            AttachmentContent::Upload(other.id),
        )],
    )
    .await
    .unwrap();
    set(&mut tx, "4").await;
    tx.commit().await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    let error = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "staged.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The attachment is larger than the limit of 4 bytes (base.attachment_max_bytes)."
    );
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_blob_upload WHERE state=1").await,
        1
    );
    let error = attachments::copy(&mut tx, &[ids[0].get()], None)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The attachment is larger than the limit of 4 bytes (base.attachment_max_bytes)."
    );
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_attachment").await,
        1
    );
    tx.commit().await.unwrap();
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_second_runtime_observes_changes_without_restart() {
    use std::sync::Arc;
    let f = fixture(limits()).await;
    let other = erp_sdk::Orm::builder(
        Arc::clone(f.h.registry()),
        erp_orm::pool::create_pool(f.h.db().config(), 4).unwrap(),
    )
    .policy(erp_orm::StaticPolicy::AllowAll)
    .generation_attachment(erp_orm::GenerationAttachment::new(
        Arc::clone(f.h.registry()),
        Arc::clone(&f.rt),
    ))
    .build()
    .unwrap();
    let mut tx = f.h.begin().await.unwrap();
    set(&mut tx, "4").await;
    tx.commit().await.unwrap();
    let mut tx = other.begin(erp_orm::PrincipalHint::system()).await.unwrap();
    let error = attachments::create(&mut tx, vec![bytes_attachment("other.txt", b"12345", None)])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The attachment is larger than the limit of 4 bytes (base.attachment_max_bytes)."
    );
    tx.rollback().await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    set(&mut tx, "5").await;
    tx.commit().await.unwrap();
    let mut tx = other.begin(erp_orm::PrincipalHint::system()).await.unwrap();
    attachments::create(&mut tx, vec![bytes_attachment("other.txt", b"12345", None)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(other);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn unique_create_and_content_replacement_cannot_bypass_policy() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    set(&mut tx, "4").await;
    let error = attachments::create_unique(
        &mut tx,
        vec![
            bytes_attachment("ok.txt", b"1", None),
            bytes_attachment("large.txt", b"12345", None),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The attachment is larger than the limit of 4 bytes (base.attachment_max_bytes)."
    );
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_attachment").await,
        0
    );
    let ids = attachments::create(
        &mut tx,
        vec![bytes_attachment("existing.txt", b"1234", None)],
    )
    .await
    .unwrap();
    set(&mut tx, "3").await;
    let error = attachments::write_content(
        &mut tx,
        &[ids[0].get()],
        AttachmentContent::Bytes(b"abcd".to_vec()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The attachment is larger than the limit of 3 bytes (base.attachment_max_bytes)."
    );
    assert_eq!(row(&mut tx, ids[0].get()).await.2, 4);
    tx.commit().await.unwrap();
    f.h.cleanup().await.unwrap();
}
