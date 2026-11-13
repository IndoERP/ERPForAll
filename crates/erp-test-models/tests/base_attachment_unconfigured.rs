//! A host that composed the ORM without an attachment runtime: a client error that names the setting.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment};
use erp_sdk::ErrorKind;
use erp_test_models::base_users;
use erp_testkit::Harness;

#[tokio::test]
async fn a_server_without_an_attachment_store_answers_a_user_error_naming_the_setting() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    let error = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "hello.txt",
            AttachmentContent::Bytes(b"hello".to_vec()),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.kind().http_status(), 422);
    let text = error.to_string();
    assert!(text.contains("Attachments are not configured:"), "{text}");
    assert!(text.contains("attachments.write_store"), "{text}");
    tx.rollback().await.unwrap();
}
