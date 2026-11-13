//! content addressing, dedup, the link protocol, inline and external blobs, upload
//! binding and the declared-type rule.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod attachment_support;

use attachment_support::*;
use erp_addon_base::attachments::{
    self, AttachmentContent, NewAttachment, StageRequest, UploadId, Uploader,
};
use erp_blob::AttachmentStore;
use erp_sdk::{Cell, ErrorKind, RawVals};

const UPLOAD_NOT_USABLE: &str = "The upload is not usable: it does not exist, has expired, was \
                                 already used, or belongs to another user.";

#[tokio::test]
async fn identical_content_is_one_blob_and_the_checksums_match() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let p = partner(&mut tx, "P").await;
    let ids = attachments::create(
        &mut tx,
        vec![
            bytes_attachment("a.txt", b"duplicate content", Some(on_partner(p))),
            bytes_attachment("b.txt", b"duplicate content", Some(on_partner(p))),
        ],
    )
    .await
    .unwrap();
    let a = row(&mut tx, ids[0].get()).await;
    let b = row(&mut tx, ids[1].get()).await;
    assert_eq!(a.1, b.1);
    assert_eq!(a.0, b.0);
    assert!(a.0.is_some());
    assert_eq!(count(&mut tx, "SELECT count(*) FROM ir_blob").await, 1);
    tx.commit().await.unwrap();
    drop(tx);
}

#[tokio::test]
async fn identical_external_content_is_one_key_in_the_store() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let p = partner(&mut tx, "P").await;
    tx.commit().await.unwrap();
    drop(tx);
    let first = stage(&f, &env, "a.txt", b"duplicate content").await;
    let second = stage(&f, &env, "b.txt", b"duplicate content").await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![
            NewAttachment::new("a.txt", AttachmentContent::Upload(first.id)).on(on_partner(p)),
            NewAttachment::new("b.txt", AttachmentContent::Upload(second.id)).on(on_partner(p)),
        ],
    )
    .await
    .unwrap();
    let a = row(&mut tx, ids[0].get()).await;
    let b = row(&mut tx, ids[1].get()).await;
    assert_eq!(a.0, b.0);
    assert_eq!(a.1, b.1, "one checksum");
    assert_eq!(count(&mut tx, "SELECT count(*) FROM ir_blob").await, 1);
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*) FROM ir_blob WHERE store_id IS NOT NULL"
        )
        .await,
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(f.store.content_keys().len(), 1);
}

#[tokio::test]
async fn small_content_is_inline_and_leaves_the_store_empty() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(&mut tx, vec![bytes_attachment("t.txt", b"tiny", None)])
        .await
        .unwrap();
    let blob = row(&mut tx, ids[0].get()).await.0.unwrap();
    assert_eq!(
        count(
            &mut tx,
            &format!("SELECT count(*) FROM ir_blob_inline WHERE blob_id = {blob}")
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*) FROM ir_blob WHERE store_id IS NULL"
        )
        .await,
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert!(f.store.content_keys().is_empty());
    let mut tx = f.h.begin().await.unwrap();
    let handle = attachments::open(&mut tx, ids[0].get()).await.unwrap();
    assert_eq!(handle.read_to_vec(100).await.unwrap(), b"tiny");
}

#[tokio::test]
async fn larger_content_is_an_object_under_its_content_key_and_reads_back() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.commit().await.unwrap();
    drop(tx);
    let up = stage(&f, &env, "big.txt", b"larger than four bytes").await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "big.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let keys = f.store.content_keys();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].contains("/sha256/"), "{keys:?}");
    let mut tx = f.h.begin().await.unwrap();
    let handle = attachments::open(&mut tx, ids[0].get()).await.unwrap();
    assert_eq!(handle.size(), 22);
    assert_eq!(
        handle.read_to_vec(1000).await.unwrap(),
        b"larger than four bytes"
    );
    // a read bounded below the size fails before any byte is fetched
    let err = handle.read_to_vec(10).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "object size in bytes exceeds the limit of 10"
    );
}

#[tokio::test]
async fn checksum_is_the_sha1_hex_and_file_size_the_byte_length() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![bytes_attachment("p.txt", b"test payload", None)],
    )
    .await
    .unwrap();
    let (_, checksum, size, mimetype) = row(&mut tx, ids[0].get()).await;
    // hashlib.sha1(b"test payload").hexdigest(), computed outside this code base
    assert_eq!(checksum, "2619edb4ca39727721293a2d0bf1e30c8e40b83a");
    assert_eq!(size, 12);
    assert_eq!(mimetype, "text/plain");
    tx.commit().await.unwrap();
    drop(tx);
}

#[tokio::test]
async fn changing_content_changes_checksum_and_queues_the_old_blob() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(&mut tx, vec![bytes_attachment("o.txt", b"original", None)])
        .await
        .unwrap();
    let before = row(&mut tx, ids[0].get()).await;
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    attachments::write_content(
        &mut tx,
        &[ids[0].get()],
        AttachmentContent::Bytes(b"modified".to_vec()),
    )
    .await
    .unwrap();
    let after = row(&mut tx, ids[0].get()).await;
    assert_ne!(before.1, after.1);
    assert_ne!(before.0, after.0);
    assert_eq!(after.2, 8);
    let queued: i64 = count(
        &mut tx,
        &format!(
            "SELECT count(*) FROM ir_blob_gc_candidate WHERE blob_id = {}",
            before.0.unwrap()
        ),
    )
    .await;
    assert_eq!(queued, 1, "the old blob is a garbage candidate");
    tx.commit().await.unwrap();
    drop(tx);
}

#[tokio::test]
async fn deleting_one_of_two_attachments_keeps_the_shared_content() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![
            bytes_attachment("a.txt", b"shared bytes", None),
            bytes_attachment("b.txt", b"shared bytes", None),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    tx.env()
        .records("ir.attachment")
        .unwrap()
        .browse([ids[0].get()])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    // the content is still there for the other attachment, and a collection leaves it alone
    let report = erp_addon_base::attachments::gc::mark_unreferenced(&mut tx, 100)
        .await
        .unwrap();
    assert_eq!(report.marked, 0, "still referenced");
    let handle = attachments::open(&mut tx, ids[1].get()).await.unwrap();
    assert_eq!(handle.read_to_vec(100).await.unwrap(), b"shared bytes");
    tx.commit().await.unwrap();
    drop(tx);
}

#[tokio::test]
async fn create_unique_refuses_malformed_base64() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let err = attachments::create_unique(
        &mut tx,
        vec![NewAttachment::new(
            "x.txt",
            AttachmentContent::Base64("***not base64***".to_owned()),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(err.message(), "Attachment is not encoded in base64.");
}

#[tokio::test]
async fn create_unique_reuses_an_attachment_with_the_same_checksum_and_type() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let first = attachments::create_unique(&mut tx, vec![bytes_attachment("a.txt", b"same", None)])
        .await
        .unwrap();
    let again = attachments::create_unique(&mut tx, vec![bytes_attachment("b.txt", b"same", None)])
        .await
        .unwrap();
    let other =
        attachments::create_unique(&mut tx, vec![bytes_attachment("c.txt", b"different", None)])
            .await
            .unwrap();
    assert_eq!(first, again);
    assert_ne!(first, other);
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_attachment").await,
        2
    );
    tx.commit().await.unwrap();
    drop(tx);
}

#[tokio::test]
async fn missing_content_is_an_error_never_an_empty_body() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.commit().await.unwrap();
    drop(tx);
    let up = stage(&f, &env, "gone.txt", b"will be lost").await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "gone.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    for key in f.store.content_keys() {
        // remove the object behind the database's back
        let ns = key.split('/').next().unwrap().to_owned();
        let hex = key.rsplit('/').next().unwrap().to_owned();
        let sha = erp_blob::from_hex::<32>(&hex).unwrap();
        let ck = erp_blob::ContentKey::new(erp_blob::Namespace::parse(&ns).unwrap(), sha);
        f.store.delete_many(&[ck]).await.unwrap();
    }
    let mut tx = f.h.begin().await.unwrap();
    let handle = attachments::open(&mut tx, ids[0].get()).await.unwrap();
    let err = handle.read_to_vec(100).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Internal);
    assert_eq!(
        err.message(),
        format!(
            "AttachmentContentMissing: the content of attachment {} is missing from the attachment store",
            ids[0].get()
        )
    );
}

#[tokio::test]
async fn an_upload_is_bound_to_its_subject_unexpired_and_single_use() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.commit().await.unwrap();
    drop(tx);
    let up = stage(&f, &env, "u.txt", b"bound upload").await;

    // another subject: one message
    let mut other = f.h.begin_as(7).await.unwrap();
    let err = attachments::create(
        &mut other,
        vec![NewAttachment::new(
            "u.txt",
            AttachmentContent::Upload(up.id.clone()),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), UPLOAD_NOT_USABLE);
    other.rollback().await.unwrap();
    drop(other);

    // a malformed id and an unknown id: the same message
    assert_eq!(
        UploadId::parse("nope").unwrap_err().message(),
        UPLOAD_NOT_USABLE
    );
    let unknown = UploadId::parse("018f4f6e-7a3c-7c3e-8a9b-0123456789ab").unwrap();
    let mut tx = f.h.begin().await.unwrap();
    let err = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "u.txt",
            AttachmentContent::Upload(unknown),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(err.message(), UPLOAD_NOT_USABLE);
    tx.rollback().await.unwrap();
    drop(tx);

    // the owner links it once; the second use fails with the same message
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "u.txt",
            AttachmentContent::Upload(up.id.clone()),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    let err = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "u2.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(err.message(), UPLOAD_NOT_USABLE);
    tx.rollback().await.unwrap();
    drop(tx);

    // an expired upload: the same message
    let fresh = stage(&f, &env, "e.txt", b"expiring upload").await;
    let mut tx = f.h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_blob_upload SET expires_at = now() - interval '1 second' WHERE id = $1::text::uuid",
        &[&fresh.id.as_str()],
    )
    .await
    .unwrap();
    let err = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "e.txt",
            AttachmentContent::Upload(fresh.id),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(err.message(), UPLOAD_NOT_USABLE);
}

#[tokio::test]
async fn writing_the_location_parameter_is_refused() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let err = tx
        .env()
        .records("ir.config_parameter")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("key", Cell::Str("ir_attachment.location".into()))
                    .set_cell("value", Cell::Str("db".into())),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "ir_attachment.location cannot be set: attachment storage is server configuration \
         (attachments.write_store), not a database parameter."
    );
}

/// cleanup leaves no row/object/blob; HTTP 422 mapping belongs to the upload route)
#[tokio::test]
async fn a_declared_type_mismatch_is_refused_and_leaves_nothing_after_cleanup() {
    let f = fixture(external_limits()).await;
    admit_cleanup(f.h.orm()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.commit().await.unwrap();
    drop(tx);
    let mut body: &[u8] = b"<html><body>not an image</body></html>";
    let err = Uploader::new(f.h.orm())
        .stage(
            &env,
            &mut body,
            StageRequest::named("a.png").declared("image/png"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "declared content type image/png does not match the content, which is text/html (and fenced cleanup pending; the committed reservation is retained)"
    );
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE state=3 AND reserved_bytes>0"
        )
        .await,
        1
    );
    assert_eq!(f.store.staging_keys().len(), 1);
    let mut clean =
        f.h.orm()
            .begin_opts(
                erp_orm::BeginOptions::new(erp_orm::PrincipalHint::system())
                    .isolation(erp_orm::Isolation::ReadCommitted),
            )
            .await
            .unwrap();
    let env = clean.env();
    attachments::gc::clean_staging(&mut clean, &env, 100)
        .await
        .unwrap();
    clean.commit().await.unwrap();
    drop(clean);
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_blob_upload").await,
        0
    );
    assert_eq!(count(&mut tx, "SELECT count(*) FROM ir_blob").await, 0);
    assert!(f.store.staging_keys().is_empty());
    assert!(f.store.content_keys().is_empty());
}

/// in base_attachment_gc)
#[tokio::test]
async fn a_rolled_back_link_leaves_no_row_and_the_upload_usable() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.commit().await.unwrap();
    drop(tx);
    let up = stage(&f, &env, "r.txt", b"rolled back content").await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "r.txt",
            AttachmentContent::Upload(up.id.clone()),
        )],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_attachment").await,
        0
    );
    assert_eq!(count(&mut tx, "SELECT count(*) FROM ir_blob").await, 0);
    // the retry links it
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "r.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(f.store.content_keys().len(), 1);
}

/// Content above the inline threshold must come through an upload, not through memory.
#[tokio::test]
async fn bytes_above_the_inline_threshold_need_an_upload() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let err = attachments::create(
        &mut tx,
        vec![bytes_attachment("big.txt", b"too big for memory", None)],
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Content above 4 bytes is not passed in memory: stream it with attachments::Uploader::stage \
         and link the upload."
    );
}

#[tokio::test]
async fn the_url_limit_is_one_value_in_the_api_and_the_column() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let column: i32 = tx
        .query(
            "SELECT character_maximum_length FROM information_schema.columns \
             WHERE table_name = 'ir_attachment' AND column_name = 'url'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(usize::try_from(column).unwrap(), attachments::URL_MAX_BYTES);
    let longest = format!(
        "https://example.com/{}",
        "a".repeat(attachments::URL_MAX_BYTES - "https://example.com/".len())
    );
    assert_eq!(longest.len(), attachments::URL_MAX_BYTES);
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "l",
            AttachmentContent::Url(longest.clone()),
        )],
    )
    .await
    .unwrap();
    let err = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "l2",
            AttachmentContent::Url(format!("{longest}a")),
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "An attachment URL is an http or https address of at most 1024 bytes without spaces or \
         control characters."
    );
}

/// A body that yields ten bytes and then fails, as a dropped client connection does.
struct BrokenBody {
    sent: usize,
}

impl tokio::io::AsyncRead for BrokenBody {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        const BODY: &[u8] = b"0123456789";
        if self.sent >= BODY.len() {
            return std::task::Poll::Ready(Err(std::io::Error::other("connection reset")));
        }
        let n = buf.remaining().min(BODY.len() - self.sent);
        buf.put_slice(&BODY[self.sent..self.sent + n]);
        self.sent += n;
        std::task::Poll::Ready(Ok(()))
    }
}

/// The node's stream admission: a stage on a full node fails at once with the typed busy error
/// (409 `Lock`, retry) and leaves no upload row; a stage that fails mid-stream gives its permit
/// back, so the next one is admitted.
#[tokio::test]
async fn a_full_node_answers_busy_and_a_failed_stream_returns_its_permit() {
    let f = fixture(external_limits().with_stream_permits_for_tests(1)).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.commit().await.unwrap();
    drop(tx);
    let held = f.rt.try_stream_permit().unwrap();
    let mut body: &[u8] = b"waiting for a permit";
    let err = Uploader::new(f.h.orm())
        .stage(&env, &mut body, StageRequest::named("busy.txt"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(
        err.message(),
        "every attachment stream of this node is in use (attachments.stream_permits_per_node = 1, \
         per node): retry shortly"
    );
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_blob_upload").await,
        0
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(held);
    assert_eq!(f.rt.available_stream_permits(), 1);
    let mut broken = BrokenBody { sent: 0 };
    Uploader::new(f.h.orm())
        .stage(&env, &mut broken, StageRequest::named("broken.txt"))
        .await
        .unwrap_err();
    assert_eq!(f.rt.available_stream_permits(), 1, "returned on the error");
    let up = stage(&f, &env, "after.txt", b"admitted again").await;
    assert_eq!(up.size, 14);
    assert_eq!(f.rt.available_stream_permits(), 1, "returned after success");
}
