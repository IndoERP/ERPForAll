// SPDX-License-Identifier: MIT OR Apache-2.0
//! The checked read behind the content route (`attachments::content`) on a
//! real database and the in-memory store: ranges of stored content, the per-node stream permit,
//! a missing object, and the single "nothing to serve" answer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod attachment_support;

use attachment_support::*;
use erp_addon_base::attachments::content::{self, ContentLookup, ContentOpenError};
use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment};
use erp_blob::{AttachmentStore, ByteRange};
use tokio::io::AsyncReadExt;

async fn read_all(mut stream: content::ContentStream) -> Vec<u8> {
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn the_route_read_serves_ranges_holds_a_permit_and_types_missing_content() {
    let f = fixture(external_limits().with_stream_permits_for_tests(1)).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let p = partner(&mut tx, "Content Holder").await;
    tx.commit().await.unwrap();
    drop(tx);
    let stored = b"stored content of the route".to_vec();
    let up = stage(&f, &env, "stored.txt", &stored).await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![
            NewAttachment::new("stored.txt", AttachmentContent::Upload(up.id)).on(on_partner(p)),
            NewAttachment::new("empty.txt", AttachmentContent::Empty).on(on_partner(p)),
            NewAttachment::new(
                "link",
                AttachmentContent::Url("https://example.com/file".into()),
            )
            .on(on_partner(p)),
            bytes_attachment("tiny.txt", b"abcd", Some(on_partner(p))),
        ],
    )
    .await
    .unwrap();
    let (stored_id, empty_id, link_id, tiny_id) =
        (ids[0].get(), ids[1].get(), ids[2].get(), ids[3].get());
    let nonexistent = count(
        &mut tx,
        "SELECT (coalesce(max(id), 0) + 100)::int8 FROM ir_attachment",
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);

    let mut tx = f.h.begin().await.unwrap();
    let ContentLookup::Found(found) = content::lookup_content(&mut tx, stored_id).await.unwrap()
    else {
        panic!("a readable stored attachment is found")
    };
    for id in [nonexistent, 0, -4, empty_id, link_id] {
        assert!(
            matches!(
                content::lookup_content(&mut tx, id).await.unwrap(),
                ContentLookup::NotFound
            ),
            "{id}: one answer for nothing to serve"
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    // the transaction has ended: the content is still readable
    assert_eq!(found.attachment_id(), stored_id);
    assert_eq!(found.name(), "stored.txt");
    assert_eq!(found.size(), stored.len() as u64);
    assert_eq!(found.mimetype(), "text/plain");
    let whole = found.open(None).await.unwrap();
    // the only permit of the node is held by the open stream
    assert!(matches!(
        found.open(None).await,
        Err(ContentOpenError::Busy { permits: 1 })
    ));
    assert_eq!(read_all(whole).await, stored);
    let part = found.open(Some(ByteRange::new(7, 7))).await.unwrap();
    assert_eq!(read_all(part).await, stored[7..14]);

    // the object disappears behind the database's back
    for key in f.store.content_keys() {
        let ns = key.split('/').next().unwrap().to_owned();
        let hex = key.rsplit('/').next().unwrap().to_owned();
        let sha = erp_blob::from_hex::<32>(&hex).unwrap();
        let ck = erp_blob::ContentKey::new(erp_blob::Namespace::parse(&ns).unwrap(), sha);
        f.store.delete_many(&[ck]).await.unwrap();
    }
    assert!(matches!(
        found.open(None).await,
        Err(ContentOpenError::Missing)
    ));

    // a blob row without bytes anywhere is missing content, named for the log
    let mut tx = f.h.begin().await.unwrap();
    let (blob, checksum, _, _) = row(&mut tx, tiny_id).await;
    tx.execute(
        "DELETE FROM ir_blob_inline WHERE blob_id = $1",
        &[&blob.unwrap()],
    )
    .await
    .unwrap();
    match content::lookup_content(&mut tx, tiny_id).await.unwrap() {
        ContentLookup::Missing { attachment, digest } => {
            assert_eq!(attachment, tiny_id);
            assert_eq!(digest, checksum);
        }
        other => panic!("a blob row without bytes is missing content, not {other:?}"),
    }
    tx.rollback().await.unwrap();
}
