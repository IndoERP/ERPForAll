//! access to an attachment derives from the record it is attached to, on the
//! production policy (the consumers' API: copy, reattach,
//! unlink with the record)., explicit batched checks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod attachment_support;

use std::sync::Arc;

use attachment_support::{bytes_attachment, count, limits};
use erp_addon_base::attachments::{
    self, AttachmentContent, ListOptions, NewAttachment, ParentRef, UNLINK_HOOK,
};
use erp_blob::{AttachmentRuntime, MemoryStore};
use erp_orm::{GenerationAttachment, Orm, PolicyProvider, UserId};
use erp_sdk::{Cell, ErrorKind, RawVals, Transaction};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};

const NOT_ALLOWED: &str = "Sorry, you are not allowed to access this document. Please contact \
                           your system administrator.";

struct World {
    h: Harness,
    orm: Orm,
    alice: i64,
    bob: i64,
    eve: i64,
    admin: i64,
    visible: i64,
    visible2: i64,
    secret: i64,
}

async fn one(c: &Client, sql: &str, p: &[&(dyn ToSql + Sync)]) -> i64 {
    c.query_one(sql, p).await.unwrap().get(0)
}

async fn user(c: &Client, login: &str, groups: &[i64]) -> i64 {
    let uid = one(
        c,
        "INSERT INTO res_users (name, login, active, share) VALUES ($1, $1, true, false) RETURNING id::int8",
        &[&login],
    )
    .await;
    for g in groups {
        c.execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&uid, g],
        )
        .await
        .unwrap();
    }
    uid
}

async fn group(c: &Client, name: &str, xmlid: &str) -> i64 {
    let id = one(
        c,
        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
        &[&name],
    )
    .await;
    let (module, local) = xmlid.split_once('.').unwrap();
    c.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id) VALUES ($1, $2, 'res.groups', $3)",
        &[&module, &local, &id],
    )
    .await
    .unwrap();
    id
}

async fn model(c: &Client, model: &str) -> i64 {
    one(
        c,
        "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $1, 'base', false) RETURNING id::int8",
        &[&model],
    )
    .await
}

async fn access(c: &Client, name: &str, model: i64, group: i64, ops: &str, domain: Option<&str>) {
    c.execute(
        "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) VALUES ($1, $2, $3, $4, $5, true)",
        &[&name, &model, &group, &ops, &domain],
    )
    .await
    .unwrap();
}

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

async fn world() -> World {
    let store = Arc::new(MemoryStore::new());
    let rt = Arc::new(AttachmentRuntime::single("main", store as _, limits()));
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .unwrap();
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let internal = group(&client, "Internal User", "base.group_user").await;
    let system = group(&client, "Administrator", "base.group_system").await;
    let partners = model(&client, "res.partner").await;
    let attachments_model = model(&client, "ir.attachment").await;
    let users_model = model(&client, "res.users").await;
    let groups_model = model(&client, "res.groups").await;
    let hide_secret = "[('name', '!=', 'Secret')]";
    access(
        &client,
        "partners read",
        partners,
        internal,
        "r",
        Some(hide_secret),
    )
    .await;
    access(
        &client,
        "partners write",
        partners,
        internal,
        "u",
        Some(hide_secret),
    )
    .await;
    access(&client, "partners manage", partners, system, "crud", None).await;
    access(
        &client,
        "attachments system",
        attachments_model,
        system,
        "crud",
        None,
    )
    .await;
    access(
        &client,
        "attachments own read",
        attachments_model,
        internal,
        "r",
        Some("[('create_uid', '=', user.id)]"),
    )
    .await;
    access(
        &client,
        "attachments write",
        attachments_model,
        internal,
        "cud",
        None,
    )
    .await;
    access(&client, "users read", users_model, internal, "r", None).await;
    access(
        &client,
        "users write self",
        users_model,
        internal,
        "u",
        Some("[('id', '=', user.id)]"),
    )
    .await;
    access(&client, "groups read", groups_model, internal, "r", None).await;
    let admin = user(&client, "admin", &[internal, system]).await;
    let alice = user(&client, "alice", &[internal]).await;
    let bob = user(&client, "bob", &[internal]).await;
    let eve = user(&client, "eve", &[]).await;
    let source = PolicySource::build(Arc::clone(h.registry()), pool(&h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(h.registry()), pool(&h, 6))
        .policy(provider)
        .resolver(Resolver::id_order())
        .generation_attachment(GenerationAttachment::new(Arc::clone(h.registry()), rt))
        .unlink_hook(UNLINK_HOOK)
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let mk = |name: &'static str| RawVals::new().set_cell("name", Cell::Str(name.into()));
    let ids = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![mk("Visible"), mk("Visible Too"), mk("Secret")],
        )
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    World {
        h,
        orm,
        alice,
        bob,
        eve,
        admin,
        visible: ids[0],
        visible2: ids[1],
        secret: ids[2],
    }
}

async fn begin(w: &World, uid: i64) -> Transaction {
    w.orm.begin(UserId::new(uid).unwrap()).await.unwrap()
}

async fn attach(w: &World, uid: i64, name: &str, parent: Option<ParentRef>) -> i64 {
    let mut tx = begin(w, uid).await;
    let ids = attachments::create(
        &mut tx,
        vec![bytes_attachment(name, name.as_bytes(), parent)],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    ids[0].get()
}

fn partner(id: i64) -> Option<ParentRef> {
    Some(ParentRef::new("res.partner", id))
}

fn denied(op: &str, ids: &[i64], uid: i64) -> String {
    let records = if ids.is_empty() {
        "the new attachment".to_owned()
    } else {
        format!("{ids:?}")
    };
    format!("{NOT_ALLOWED}\n\n(Operation: {op})\n\nRecords: {records}, User: {uid}")
}

#[tokio::test]
async fn reading_follows_read_access_to_the_record() {
    let w = world().await;
    let on_visible = attach(&w, w.admin, "v.txt", partner(w.visible)).await;
    let on_secret = attach(&w, w.admin, "s.txt", partner(w.secret)).await;
    let mut tx = begin(&w, w.alice).await;
    let handle = attachments::open(&mut tx, on_visible).await.unwrap();
    assert_eq!(handle.read_to_vec(100).await.unwrap(), b"v.txt");
    let err = attachments::open(&mut tx, on_secret).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), denied("read", &[on_secret], w.alice));
    // a listing shows only what the record rules allow, with one message for nothing
    let listed = attachments::for_record(
        &mut tx,
        "res.partner",
        &[w.visible, w.secret],
        ListOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        listed.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![on_visible]
    );
    // a missing attachment looks like a refused one
    let err = attachments::open(&mut tx, 999_999).await.unwrap_err();
    assert_eq!(err.message(), denied("read", &[999_999], w.alice));
}

#[tokio::test]
async fn writing_follows_write_access_to_the_record() {
    let w = world().await;
    let on_secret = attach(&w, w.admin, "s.txt", partner(w.secret)).await;
    let mut tx = begin(&w, w.alice).await;
    let err = tx
        .env()
        .records("ir.attachment")
        .unwrap()
        .browse([on_secret])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("renamed".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), denied("write", &[on_secret], w.alice));
    // creating on a record the user cannot write is refused as well
    let err = attachments::create(
        &mut tx,
        vec![bytes_attachment("n.txt", b"n", partner(w.secret))],
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), denied("create", &[], w.alice));
    // deleting is write access to the parent too
    let err = tx
        .env()
        .records("ir.attachment")
        .unwrap()
        .browse([on_secret])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(err.message(), denied("unlink", &[on_secret], w.alice));
}

#[tokio::test]
async fn a_field_bound_attachment_needs_access_to_that_field() {
    let w = world().await;
    let mut tx = begin(&w, w.alice).await;
    let own = ParentRef::new("res.users", w.alice);
    // `name` is user-writeable, `login` is not
    let ok = NewAttachment::new("sig.txt", AttachmentContent::Bytes(b"sig".to_vec()))
        .on(own.clone())
        .field("name");
    attachments::create(&mut tx, vec![ok]).await.unwrap();
    let bad = NewAttachment::new("login.txt", AttachmentContent::Bytes(b"x".to_vec()))
        .on(own)
        .field("login");
    let err = attachments::create(&mut tx, vec![bad]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(err.message(), denied("create", &[], w.alice));
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn a_standalone_attachment_belongs_to_its_creator_and_the_system_group() {
    let w = world().await;
    let id = attach(&w, w.alice, "mine.txt", None).await;
    for (uid, allowed) in [(w.alice, true), (w.admin, true), (w.bob, false)] {
        let mut tx = begin(&w, uid).await;
        let result = attachments::open(&mut tx, id).await;
        assert_eq!(result.is_ok(), allowed, "uid {uid}");
        if !allowed {
            assert_eq!(result.unwrap_err().message(), denied("read", &[id], uid));
        }
        // the generic search shows the creator's own rows only (and the system group's all)
        let found = tx
            .env()
            .records("ir.attachment")
            .unwrap()
            .search(&mut tx, &erp_sdk::Domain::TRUE)
            .await
            .unwrap()
            .ids();
        assert_eq!(found.contains(&id), allowed, "uid {uid}");
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn an_attachment_cannot_be_attached_to_itself() {
    let w = world().await;
    let id = attach(&w, w.admin, "loop.txt", None).await;
    let mut tx = begin(&w, w.admin).await;
    let err = tx
        .env()
        .records("ir.attachment")
        .unwrap()
        .browse([id])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("res_model", Cell::Str("ir.attachment".into()))
                .set_cell("res_id", Cell::Id(id)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "You cannot attach an attachment to itself.\nAttachment ir.attachment({id},) cannot have res_id: ir.attachment({id},)"
        )
    );
}

#[tokio::test]
async fn a_binary_attachment_with_a_url_needs_the_serving_group() {
    let w = world().await;
    let url_vals = || {
        RawVals::new()
            .set_cell("name", Cell::Str("page".into()))
            .set_cell("type", Cell::Str("binary".into()))
            .set_cell("url", Cell::Str("/web/static/x.png".into()))
    };
    let mut tx = begin(&w, w.alice).await;
    let err = tx
        .env()
        .records("ir.attachment")
        .unwrap()
        .create(&mut tx, vec![url_vals()])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Sorry, you are not allowed to write on this document"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = begin(&w, w.admin).await;
    tx.env()
        .records("ir.attachment")
        .unwrap()
        .create(&mut tx, vec![url_vals()])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn copying_a_binary_attachment_with_a_url_needs_the_serving_group() {
    let w = world().await;
    let mut tx = begin(&w, w.admin).await;
    let served = tx
        .env()
        .records("ir.attachment")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("page".into()))
                    .set_cell("type", Cell::Str("binary".into()))
                    .set_cell("url", Cell::Str("/web/static/x.png".into()))
                    .set_cell("res_model", Cell::Str("res.partner".into()))
                    .set_cell("res_id", Cell::Id(w.visible)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    // she may read the original (its record is visible to her) and write the target record
    let mut tx = begin(&w, w.alice).await;
    let err = attachments::copy(
        &mut tx,
        &[served],
        Some(ParentRef::new("res.partner", w.visible2)),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err:?}");
    assert_eq!(
        err.message(),
        "Sorry, you are not allowed to write on this document"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // the generic ORM copy takes the same path
    let mut tx = begin(&w, w.alice).await;
    let err = tx
        .env()
        .records("ir.attachment")
        .unwrap()
        .browse([served])
        .unwrap()
        .copy(&mut tx, None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err:?}");
    assert_eq!(
        err.message(),
        "Sorry, you are not allowed to write on this document"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = begin(&w, w.alice).await;
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*) FROM ir_attachment WHERE type = 'binary' AND url IS NOT NULL"
        )
        .await,
        1,
        "no copy was created"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // the serving group may copy it
    let mut tx = begin(&w, w.admin).await;
    let copies = attachments::copy(
        &mut tx,
        &[served],
        Some(ParentRef::new("res.partner", w.visible2)),
    )
    .await
    .unwrap();
    assert_eq!(copies.len(), 1);
    let url: Option<String> = tx
        .query(
            "SELECT url FROM ir_attachment WHERE id = $1",
            &[&copies[0].get()],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(url.as_deref(), Some("/web/static/x.png"));
    let listed =
        attachments::for_record(&mut tx, "res.partner", &[w.visible], ListOptions::default())
            .await
            .unwrap();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].public);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn create_unique_only_reuses_attachments_the_caller_can_write() {
    let w = world().await;
    let admins = attach(&w, w.admin, "same", partner(w.secret)).await;
    let mut tx = begin(&w, w.alice).await;
    let got = attachments::create_unique(&mut tx, vec![bytes_attachment("same", b"same", None)])
        .await
        .unwrap();
    assert_ne!(
        got[0].get(),
        admins,
        "the admin's attachment on a hidden record is not reused"
    );
    tx.commit().await.unwrap();
    drop(tx);
    // her own attachment is reused by her
    let mut tx = begin(&w, w.alice).await;
    let again = attachments::create_unique(&mut tx, vec![bytes_attachment("same", b"same", None)])
        .await
        .unwrap();
    assert_eq!(again[0].get(), got[0].get());
}

#[tokio::test]
async fn a_user_without_a_row_cannot_list_open_or_read_attachments() {
    let w = world().await;
    let on_visible = attach(&w, w.admin, "v.txt", partner(w.visible)).await;
    let mut tx = begin(&w, w.eve).await;
    assert!(
        attachments::for_record(&mut tx, "res.partner", &[w.visible], ListOptions::default())
            .await
            .unwrap()
            .is_empty()
    );
    let err = attachments::open(&mut tx, on_visible).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    let err = tx
        .env()
        .records("ir.attachment")
        .unwrap()
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
}

#[tokio::test]
async fn a_listing_leaves_out_field_bound_attachments_unless_asked() {
    let w = world().await;
    let mut tx = begin(&w, w.admin).await;
    let plain = bytes_attachment("plain.txt", b"plain", partner(w.visible));
    let bound = bytes_attachment("bound.txt", b"bound", partner(w.visible)).field("name");
    attachments::create(&mut tx, vec![plain, bound])
        .await
        .unwrap();
    let default =
        attachments::for_record(&mut tx, "res.partner", &[w.visible], ListOptions::default())
            .await
            .unwrap();
    assert_eq!(
        default.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        vec!["plain.txt"]
    );
    let mut options = ListOptions::default();
    options.res_field = Some("name".to_owned());
    let bound_only = attachments::for_record(&mut tx, "res.partner", &[w.visible], options)
        .await
        .unwrap();
    assert_eq!(
        bound_only
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        vec!["bound.txt"]
    );
    tx.rollback().await.unwrap();
}

/// The consumers' API: move, copy and unlink by record without SQL.
#[tokio::test]
async fn attachments_move_copy_and_follow_their_record() {
    let w = world().await;
    let id = attach(&w, w.alice, "doc.txt", partner(w.visible)).await;
    // attachments move to another record
    let mut tx = begin(&w, w.alice).await;
    attachments::reattach(
        &mut tx,
        &[id],
        Some(ParentRef::new("res.partner", w.visible2)),
    )
    .await
    .unwrap();
    assert_eq!(
        attachments::for_record(
            &mut tx,
            "res.partner",
            &[w.visible2],
            ListOptions::default()
        )
        .await
        .unwrap()
        .len(),
        1
    );
    // moving onto a record she cannot write is refused
    let err = attachments::reattach(
        &mut tx,
        &[id],
        Some(ParentRef::new("res.partner", w.secret)),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    tx.rollback().await.unwrap();
    drop(tx);
    // a copy references the same content
    let mut tx = begin(&w, w.alice).await;
    let copies = attachments::copy(
        &mut tx,
        &[id],
        Some(ParentRef::new("res.partner", w.visible2)),
    )
    .await
    .unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_blob").await,
        1,
        "one blob, two references"
    );
    assert_eq!(
        count(&mut tx, "SELECT count(DISTINCT blob_id) FROM ir_attachment").await,
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    // deleting the record deletes its attachments, and the blob stays for the other reference
    let mut tx = begin(&w, w.admin).await;
    tx.env()
        .records("res.partner")
        .unwrap()
        .browse([w.visible2])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(
        count(&mut tx, &format!("SELECT count(*) FROM ir_attachment WHERE res_model = 'res.partner' AND res_id = {}", w.visible2)).await,
        0
    );
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_attachment").await,
        1
    );
    tx.commit().await.unwrap();
    let _ = (w.bob, &w.h);
}
