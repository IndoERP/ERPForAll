//! the attachment service's `adopt` (a post claims the caller's
//! composer files for a record) and the `pending_in` placeholder it claims, on the service
//! layer without the mail addon. The mail-level cases are and in
//! `mail_rpc.rs`,
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod attachment_support;

use attachment_support::*;
use erp_addon_base::attachments::{
    self, AdoptScope, AttachmentContent, MAX_BATCH, NewAttachment, ParentRef,
};
use erp_orm::UserId;
use erp_sdk::{ErrorKind, Transaction};
use erp_testkit::tokio_postgres::NoTls;

const COMPOSER: &str = "mail.compose.message";

async fn user(f: &Fixture, login: &str) -> i64 {
    let (client, connection) = f.h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .query_one(
            "INSERT INTO res_users (name, login, active, share) VALUES ($1, $1, true, false) \
             RETURNING id::int8",
            &[&login],
        )
        .await
        .unwrap()
        .get(0)
}

async fn begin(f: &Fixture, uid: i64) -> Transaction {
    f.h.orm().begin(UserId::new(uid).unwrap()).await.unwrap()
}

fn pending(name: &str) -> NewAttachment {
    NewAttachment::new(name, AttachmentContent::Bytes(name.as_bytes().to_vec()))
        .pending_in(COMPOSER)
}

async fn make(f: &Fixture, uid: i64, item: NewAttachment) -> i64 {
    let mut tx = begin(f, uid).await;
    let ids = attachments::create(&mut tx, vec![item]).await.unwrap();
    tx.commit().await.unwrap();
    ids[0].get()
}

async fn parent_of(f: &Fixture, id: i64) -> (Option<String>, Option<i64>) {
    let mut tx = f.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT res_model, res_id::int8 FROM ir_attachment WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    let out = (rows[0].get(0), rows[0].get(1));
    tx.rollback().await.unwrap();
    out
}

/// A composer file is the placeholder's, standalone to the access rules: its creator opens it
/// (another user's refusal is `adopt_refuses_...`, and the standalone rule itself is
/// `base_attachment_access.rs`).
#[tokio::test]
async fn a_pending_file_names_its_placeholder_and_its_creator_opens_it() {
    let f = fixture(limits()).await;
    let alice = user(&f, "alice").await;
    let file = make(&f, alice, pending("draft.txt")).await;
    assert_eq!(parent_of(&f, file).await, (Some(COMPOSER.to_owned()), None));
    let mut tx = begin(&f, alice).await;
    let handle = attachments::open(&mut tx, file).await.unwrap();
    assert_eq!(handle.read_to_vec(100).await.unwrap(), b"draft.txt");
    tx.rollback().await.unwrap();
}

/// A pending file names a placeholder, not a record or a field.
#[tokio::test]
async fn a_pending_file_takes_no_parent_and_no_field() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let p = partner(&mut tx, "P").await;
    for (item, message) in [
        (
            pending("x").on(ParentRef::new("res.partner", p)),
            "A pending attachment belongs to a placeholder model, not to a record or a field: \
             give no parent and no res_field.",
        ),
        (
            pending("x").field("image"),
            "A pending attachment belongs to a placeholder model, not to a record or a field: \
             give no parent and no res_field.",
        ),
        (
            NewAttachment::new("x", AttachmentContent::Empty).pending_in(""),
            "An attachment's placeholder model name is 1 to 128 bytes.",
        ),
        (
            NewAttachment::new("x", AttachmentContent::Empty).pending_in("ir.audit.entry"),
            "Attachments cannot be attached to ir.audit.entry: it holds security data.",
        ),
    ] {
        let error = attachments::create(&mut tx, vec![item]).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), message);
    }
    tx.rollback().await.unwrap();
}

/// `adopt` moves the caller's composer files, leaves the files already on the record, and a
/// second call is a no-op.
#[tokio::test]
async fn adopt_moves_the_callers_composer_files_and_keeps_those_on_the_record() {
    let f = fixture(limits()).await;
    let alice = user(&f, "alice").await;
    let mut tx = f.h.begin().await.unwrap();
    let p = partner(&mut tx, "P").await;
    tx.commit().await.unwrap();
    let draft = make(&f, alice, pending("draft.txt")).await;
    let on_record = make(
        &f,
        alice,
        NewAttachment::new("on.txt", AttachmentContent::Bytes(b"on".to_vec()))
            .on(ParentRef::new("res.partner", p)),
    )
    .await;
    let mut tx = begin(&f, alice).await;
    attachments::adopt(
        &mut tx,
        &[draft, on_record, draft],
        ParentRef::new("res.partner", p),
        &[COMPOSER],
        AdoptScope::ComposerAndRecord,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for id in [draft, on_record] {
        assert_eq!(
            parent_of(&f, id).await,
            (Some("res.partner".to_owned()), Some(p))
        );
    }
    let mut tx = begin(&f, alice).await;
    attachments::adopt(
        &mut tx,
        &[draft],
        ParentRef::new("res.partner", p),
        &[COMPOSER],
        AdoptScope::ComposerAndRecord,
    )
    .await
    .unwrap();
    attachments::adopt(
        &mut tx,
        &[],
        ParentRef::new("res.partner", p),
        &[COMPOSER],
        AdoptScope::ComposerAndRecord,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
}

/// Everything else refuses the whole call by name and moves nothing.
#[tokio::test]
async fn adopt_refuses_what_is_not_the_callers_and_moves_nothing() {
    let f = fixture(limits()).await;
    let (alice, bob) = (user(&f, "alice").await, user(&f, "bob").await);
    let mut tx = f.h.begin().await.unwrap();
    let p = partner(&mut tx, "P").await;
    let q = partner(&mut tx, "Q").await;
    tx.commit().await.unwrap();
    let mine = make(&f, alice, pending("mine.txt")).await;
    let bobs = make(&f, bob, pending("bobs.txt")).await;
    let elsewhere = make(
        &f,
        alice,
        NewAttachment::new("e.txt", AttachmentContent::Bytes(b"e".to_vec()))
            .on(ParentRef::new("res.partner", q)),
    )
    .await;
    let other_placeholder = make(
        &f,
        alice,
        NewAttachment::new("o.txt", AttachmentContent::Bytes(b"o".to_vec()))
            .pending_in("mail.other.placeholder"),
    )
    .await;
    for refused in [bobs, elsewhere, other_placeholder, 9_999_999] {
        let mut tx = begin(&f, alice).await;
        let error = attachments::adopt(
            &mut tx,
            &[mine, refused],
            ParentRef::new("res.partner", p),
            &[COMPOSER],
            AdoptScope::ComposerAndRecord,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            format!(
                "These attachments cannot be linked here: [{refused}]. Only files you uploaded \
                 for this purpose and files already on the record can be used."
            )
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            parent_of(&f, mine).await,
            (Some(COMPOSER.to_owned()), None),
            "the valid file did not move"
        );
    }
}

/// Shapes: non-positive ids, too many ids, a parent that cannot hold files.
#[tokio::test]
async fn adopt_bounds_its_input() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let p = partner(&mut tx, "P").await;
    let error = attachments::adopt(
        &mut tx,
        &[0],
        ParentRef::new("res.partner", p),
        &[COMPOSER],
        AdoptScope::ComposerAndRecord,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "An attachment id is a positive number, not 0."
    );
    let many: Vec<i64> = (1..=i64::try_from(MAX_BATCH).unwrap() + 1).collect();
    let error = attachments::adopt(
        &mut tx,
        &many,
        ParentRef::new("res.partner", p),
        &[COMPOSER],
        AdoptScope::ComposerAndRecord,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!("At most {MAX_BATCH} attachments are claimed in one call.")
    );
    let error = attachments::adopt(
        &mut tx,
        &[1],
        ParentRef::new("res.partner", 0),
        &[COMPOSER],
        AdoptScope::ComposerAndRecord,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "An attachment's parent record id must be positive."
    );
    tx.rollback().await.unwrap();
}

/// With the own-composer scope (a caller who is not an internal user) a file already on the
/// record is refused by name like any other, the caller's composer file is still moved, and
/// nothing moves when one id is refused.
#[tokio::test]
async fn adopt_with_the_composer_scope_refuses_files_already_on_the_record() {
    let f = fixture(limits()).await;
    let alice = user(&f, "alice").await;
    let mut tx = f.h.begin().await.unwrap();
    let p = partner(&mut tx, "P").await;
    tx.commit().await.unwrap();
    let draft = make(&f, alice, pending("draft.txt")).await;
    let on_record = make(
        &f,
        alice,
        NewAttachment::new("on.txt", AttachmentContent::Bytes(b"on".to_vec()))
            .on(ParentRef::new("res.partner", p)),
    )
    .await;
    let mut tx = begin(&f, alice).await;
    let error = attachments::adopt(
        &mut tx,
        &[draft, on_record],
        ParentRef::new("res.partner", p),
        &[COMPOSER],
        AdoptScope::ComposerOnly,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        format!(
            "These attachments cannot be linked here: [{on_record}]. Only files you uploaded for \
             this purpose can be used."
        )
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        parent_of(&f, draft).await,
        (Some(COMPOSER.to_owned()), None)
    );
    let mut tx = begin(&f, alice).await;
    attachments::adopt(
        &mut tx,
        &[draft],
        ParentRef::new("res.partner", p),
        &[COMPOSER],
        AdoptScope::ComposerOnly,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        parent_of(&f, draft).await,
        (Some("res.partner".to_owned()), Some(p))
    );
}

/// Two claims of the same composer file at once (a double submit), both waiting on a lock held
/// by a third transaction: when it is released one claim wins and the other ends cleanly (the
/// file is on the record, which the composer scope refuses, or a concurrency error from the
/// engine). They never deadlock (40P01), which a shared lock followed by the update would do:
/// both claims would take the shared lock and then wait for each other's update.
#[tokio::test]
async fn two_concurrent_claims_of_one_file_end_with_one_winner_and_no_deadlock() {
    let f = fixture(limits()).await;
    let alice = user(&f, "alice").await;
    let mut tx = f.h.begin().await.unwrap();
    let p = partner(&mut tx, "P").await;
    tx.commit().await.unwrap();
    let draft = make(&f, alice, pending("draft.txt")).await;
    // the holder: both claims queue behind its row lock
    let mut holder = f.h.begin().await.unwrap();
    holder
        .query(
            "SELECT id FROM ir_attachment WHERE id = $1 FOR NO KEY UPDATE",
            &[&draft],
        )
        .await
        .unwrap();
    let mut claims = Vec::new();
    for _ in 0..2 {
        let mut tx = begin(&f, alice).await;
        claims.push(tokio::spawn(async move {
            let result = attachments::adopt(
                &mut tx,
                &[draft],
                ParentRef::new("res.partner", p),
                &[COMPOSER],
                AdoptScope::ComposerOnly,
            )
            .await;
            match &result {
                Ok(()) => tx.commit().await.unwrap(),
                Err(_) => tx.rollback().await.unwrap(),
            }
            result
        }));
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        claims.iter().all(|c| !c.is_finished()),
        "a claim did not wait for the lock holder"
    );
    holder.rollback().await.unwrap();
    let mut results = Vec::new();
    for claim in claims {
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), claim)
            .await
            .expect("a claim ended after the lock was released")
            .unwrap();
        results.push(result);
    }
    let wins = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "{results:?}");
    for error in results.iter().filter_map(|r| r.as_ref().err()) {
        assert!(
            matches!(
                error.kind(),
                ErrorKind::Access | ErrorKind::Concurrency | ErrorKind::Conflict | ErrorKind::Lock
            ),
            "{error:?}"
        );
        assert!(
            !error.message().to_lowercase().contains("deadlock"),
            "{error:?}"
        );
    }
    assert_eq!(
        parent_of(&f, draft).await,
        (Some("res.partner".to_owned()), Some(p))
    );
}
