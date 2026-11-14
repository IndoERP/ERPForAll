//! The record-thread post path of `mail.thread`: `message_post` (subtype, author, parent,
//! Message-Id, recipients, author auto-subscription, `client_uid` replay, mentions, `silent`) and
//! `_message_log_batch`, with the statement count of a post held constant in the follower count
//! (a ratchet; the §5.1 budget is `DEFERRED_PERFORMANCE`, ADR
//! 0065).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod chat;
mod support;

use std::collections::BTreeMap;

use erp_addon_mail::{Body, Mention, MentionTarget, PostRequest, PostSubtype, PostType, Posted};
use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation};
use erp_testkit::{DomainPolicy, Harness, Otherwise};
use support::*;

const TICKET: &str = "test_mail.ticket";
const CHAT: &str = "test_mail.chat";
/// The engine's refusal of the write-denying test policy (text of the access engine).
const WRITE_DENIED: &str =
    "Your access rights do not let you write 'Mail Test Ticket' (test_mail.ticket) records.";
/// Statement ceiling of the post scenario of `post_statements_do_not_grow_with_followers`, a
/// RATCHET (DEFERRED_PERFORMANCE): may only go down.
const POST_CEILING: u64 = 5;
/// Statement ceiling of a replayed post (same `client_uid`), a RATCHET as above: 1, measured with
/// it.
const REPLAY_CEILING: u64 = 1;

/// A harness with the `ir.model.data` stand-in of `chat` (subtypes are found by external id) and a policy that allows
/// everything, optionally refusing writes on tickets and partners.
async fn harness_with(deny_writes: bool) -> Harness {
    let mut policy = DomainPolicy::builder(Otherwise::Allow);
    if deny_writes {
        policy = policy.rule(TICKET, Operation::Write, Domain::FALSE).rule(
            "res.partner",
            Operation::Write,
            Domain::FALSE,
        );
    }
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .policy(policy.build().unwrap())
    .build()
    .await
    .unwrap()
}

async fn harness() -> Harness {
    harness_with(false).await
}

/// The generic subtypes `mail` ships, with their external ids (the harness loads no data file).
struct Subtypes {
    comment: i64,
    note: i64,
}

async fn seed(tx: &mut Transaction) -> Subtypes {
    let comment = subtype(
        tx,
        "Messages",
        RawVals::new().set_cell("sequence", Cell::Int(0)),
    )
    .await;
    let note = subtype(
        tx,
        "Notes",
        RawVals::new()
            .set_cell("default", Cell::Bool(false))
            .set_cell("internal", Cell::Bool(true))
            .set_cell("sequence", Cell::Int(90)),
    )
    .await;
    for (name, id) in [("mt_comment", comment), ("mt_note", note)] {
        create(
            tx,
            "ir.model.data",
            RawVals::new()
                .set_cell("module", text("mail"))
                .set_cell("name", text(name))
                .set_cell("model", text("mail.message.subtype"))
                .set_cell("res_id", Cell::Id(id)),
        )
        .await;
    }
    // the superuser row the harness inserts has no partner; a message needs an author
    let root = partner(tx, "root").await;
    set_user_partner(tx, 1, Some(root)).await;
    Subtypes { comment, note }
}

/// Set (or clear) the partner of user row `id`.
async fn set_user_partner(tx: &mut Transaction, id: i64, partner: Option<i64>) {
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE res_users SET partner_id = $2 WHERE id = $1",
        &[&id, &partner],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
}

/// A user row for a new partner `name`; returns the partner.
async fn user(tx: &mut Transaction, id: i64, name: &str, share: bool) -> i64 {
    let partner = partner(tx, name).await;
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active, share, partner_id) VALUES ($1, $2, $2, true, $4, $3)",
        &[&id, &name, &partner, &share],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    partner
}

async fn inactive_partner(tx: &mut Transaction, name: &str) -> i64 {
    create(
        tx,
        "res.partner",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("active", Cell::Bool(false)),
    )
    .await
}

fn record_of(env: &Env, model: &str, id: i64) -> Records {
    env.records(model).unwrap().browse([id]).unwrap()
}

fn text_post(body: &str) -> PostRequest {
    PostRequest::new(Body::Text(body.to_owned()))
}

fn comment(body: &str) -> PostRequest {
    let mut request = text_post(body);
    request.message_type = PostType::Comment;
    request.subtype = PostSubtype::Comment;
    request
}

async fn post(tx: &mut Transaction, record: &Records, request: PostRequest) -> Result<Posted> {
    record.call(tx, "message_post", (request,)).await
}

/// Post on `model` record `id` in the transaction's own (superuser) environment.
async fn post_on(
    tx: &mut Transaction,
    model: &str,
    id: i64,
    request: PostRequest,
) -> Result<Posted> {
    let record = record_of(&tx.env(), model, id);
    post(tx, &record, request).await
}

/// Post on `model` record `id` as user `uid`.
async fn post_as(
    tx: &mut Transaction,
    uid: i64,
    model: &str,
    id: i64,
    request: PostRequest,
) -> Result<Posted> {
    let record = record_of(&tx.env().with_user(uid), model, id);
    post(tx, &record, request).await
}

fn uuid(n: u64) -> Uuid {
    Uuid::parse(&format!("00000000-0000-4000-8000-{n:012}")).unwrap()
}

/// A field of a message, read after a flush from the database.
async fn field(tx: &mut Transaction, id: i64, name: &str) -> Cell {
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    record_of(&tx.env(), "mail.message", id)
        .cell(tx, name)
        .await
        .unwrap()
}

async fn str_field(tx: &mut Transaction, id: i64, name: &str) -> String {
    match field(tx, id, name).await {
        Cell::Str(value) => value.to_string(),
        other => panic!("{name} is not text: {other:?}"),
    }
}

async fn id_field(tx: &mut Transaction, id: i64, name: &str) -> Option<i64> {
    match field(tx, id, name).await {
        Cell::Id(value) => Some(value),
        Cell::Null => None,
        other => panic!("{name} is not an id: {other:?}"),
    }
}

fn following(model: &str, res_id: i64, partner: i64) -> Domain {
    Domain::condition("res_model", Operator::Eq, model).unwrap()
        & Domain::condition("res_id", Operator::Eq, res_id).unwrap()
        & Domain::condition("partner_id", Operator::Eq, partner).unwrap()
}

/// A message of `model` record `res_id` with the given type, as the superuser.
async fn typed_message(tx: &mut Transaction, model: &str, res_id: i64, kind: &str) -> i64 {
    create(
        tx,
        "mail.message",
        RawVals::new()
            .set_cell("model", text(model))
            .set_cell("res_id", Cell::Id(res_id))
            .set_cell("message_type", text(kind)),
    )
    .await
}

fn assert_validation(error: &Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn a_post_gets_a_message_id_naming_the_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let posted = post_on(&mut tx, TICKET, t, text_post("Hi")).await.unwrap();
    assert!(!posted.replayed);
    let message_id = str_field(&mut tx, posted.message_id, "message_id").await;
    assert!(
        message_id.starts_with('<') && message_id.ends_with('>'),
        "{message_id}"
    );
    assert!(
        message_id.contains(&format!("-{t}-{TICKET}@")),
        "{message_id}"
    );
    // a second post gets another id
    let again = post_on(&mut tx, TICKET, t, text_post("Hi")).await.unwrap();
    assert_ne!(
        str_field(&mut tx, again.message_id, "message_id").await,
        message_id
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_post_without_subtype_is_a_note() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let posted = post_on(&mut tx, TICKET, t, text_post("Internal note"))
        .await
        .unwrap();
    assert_eq!(
        id_field(&mut tx, posted.message_id, "subtype_id").await,
        Some(s.note)
    );
    assert_eq!(
        str_field(&mut tx, posted.message_id, "message_type").await,
        "notification"
    );
    assert_eq!(str_field(&mut tx, posted.message_id, "model").await, TICKET);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_post_needs_exactly_one_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let (t1, t2) = (ticket(&mut tx, "T1").await, ticket(&mut tx, "T2").await);
    let tickets = tx.env().records(TICKET).unwrap();
    for (records, message) in [
        (
            tickets.browse(Vec::<i64>::new()).unwrap(),
            "A message is posted on exactly one record, not 0 (the reference ERP's message_post needs a \
             singleton).",
        ),
        (
            tickets.browse([t1, t2]).unwrap(),
            "A message is posted on exactly one record, not 2 (the reference ERP's message_post needs a \
             singleton).",
        ),
    ] {
        let error = post(&mut tx, &records, text_post("x")).await.unwrap_err();
        assert_validation(&error, message);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// pointer; a typed post cannot carry it, and the RPC input path that parses it does not exist yet)
#[test]
fn a_post_refuses_user_notification() {
    // A Rust caller cannot build a user_notification post (PostType has no such variant); the
    // selection value coming from a caller is parsed and refused before any statement.
    let error = "user_notification".parse::<PostType>().unwrap_err();
    assert_validation(
        &error,
        "A user_notification is not posted on a thread; it is sent with message_notify, which \
         is not implemented yet.",
    );
    let error = "memo".parse::<PostType>().unwrap_err();
    assert_validation(
        &error,
        "Unknown message type 'memo'; a post is one of email, comment, email_outgoing, \
         notification, auto_comment, out_of_office, tracking.",
    );
    for kind in PostType::ALL {
        assert_eq!(kind.as_str().parse::<PostType>().unwrap(), kind);
    }
}

#[tokio::test]
async fn a_flat_thread_post_replies_to_the_threads_message() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let first = message(&mut tx, TICKET, t, "first").await;
    let posted = post_on(&mut tx, TICKET, t, text_post("reply"))
        .await
        .unwrap();
    assert_eq!(
        id_field(&mut tx, posted.message_id, "parent_id").await,
        Some(first)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_parent_follows_references_flat_thread_rule() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let other = ticket(&mut tx, "Other").await;
    // only system messages: the newest one that is not a user notification
    let n1 = typed_message(&mut tx, TICKET, t, "notification").await;
    typed_message(&mut tx, TICKET, t, "user_notification").await;
    let ticket = record_of(&tx.env(), TICKET, t);
    let p = post(&mut tx, &ticket, text_post("a")).await.unwrap();
    assert_eq!(id_field(&mut tx, p.message_id, "parent_id").await, Some(n1));
    // a comment wins over newer notifications (the post above is one)
    let c1 = typed_message(&mut tx, TICKET, t, "comment").await;
    typed_message(&mut tx, TICKET, t, "notification").await;
    let p = post(&mut tx, &ticket, text_post("b")).await.unwrap();
    assert_eq!(id_field(&mut tx, p.message_id, "parent_id").await, Some(c1));
    // a given parent of the thread is kept; one of another thread is replaced
    let mut request = text_post("c");
    request.parent_id = Some(n1);
    let p = post(&mut tx, &ticket, request).await.unwrap();
    assert_eq!(id_field(&mut tx, p.message_id, "parent_id").await, Some(n1));
    let foreign = message(&mut tx, TICKET, other, "elsewhere").await;
    let mut request = text_post("d");
    request.parent_id = Some(foreign);
    let p = post(&mut tx, &ticket, request).await.unwrap();
    assert_eq!(id_field(&mut tx, p.message_id, "parent_id").await, Some(c1));
    // a thread that is not flat: no parent unless a valid one is given
    let chat = create(&mut tx, CHAT, RawVals::new().set_cell("name", text("C"))).await;
    let chat_first = message(&mut tx, CHAT, chat, "first").await;
    let chat = record_of(&tx.env(), CHAT, chat);
    let p = post(&mut tx, &chat, text_post("e")).await.unwrap();
    assert_eq!(id_field(&mut tx, p.message_id, "parent_id").await, None);
    let mut request = text_post("f");
    request.parent_id = Some(foreign);
    let p = post(&mut tx, &chat, request).await.unwrap();
    assert_eq!(id_field(&mut tx, p.message_id, "parent_id").await, None);
    let mut request = text_post("g");
    request.parent_id = Some(chat_first);
    let p = post(&mut tx, &chat, request).await.unwrap();
    assert_eq!(
        id_field(&mut tx, p.message_id, "parent_id").await,
        Some(chat_first)
    );
    let mut request = text_post("h");
    request.parent_id = Some(0);
    let error = post(&mut tx, &chat, request).await.unwrap_err();
    assert_validation(
        &error,
        "A parent message id must be a positive integer, not 0.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn posting_on_a_partner_needs_read_access_on_a_ticket_write_access() {
    let h = harness_with(true).await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "pat", false).await;
    let contact = partner(&mut tx, "Contact").await;
    let t = ticket(&mut tx, "T").await;
    tx.flush().await.unwrap();
    let posted = post_as(&mut tx, 42, "res.partner", contact, text_post("Hello"))
        .await
        .unwrap();
    assert_eq!(
        str_field(&mut tx, posted.message_id, "model").await,
        "res.partner"
    );
    let error = post_as(&mut tx, 42, TICKET, t, text_post("Hello"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), WRITE_DENIED);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_internal_author_of_a_comment_follows_the_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let own = user(&mut tx, 42, "pat", false).await;
    let t = ticket(&mut tx, "T").await;
    // a note does not subscribe the author
    post_as(&mut tx, 42, TICKET, t, text_post("note"))
        .await
        .unwrap();
    assert!(
        ids(&mut tx, "mail.followers", &following(TICKET, t, own))
            .await
            .is_empty()
    );
    let posted = post_as(&mut tx, 42, TICKET, t, comment("Hello"))
        .await
        .unwrap();
    assert_eq!(
        id_field(&mut tx, posted.message_id, "author_id").await,
        Some(own)
    );
    let rows = ids(&mut tx, "mail.followers", &following(TICKET, t, own)).await;
    assert_eq!(rows.len(), 1);
    // the default subtypes: mt_comment (mt_note is not a default subtype)
    let subtypes = record_of(&tx.env(), "mail.followers", rows[0])
        .cell(&mut tx, "subtype_ids")
        .await
        .unwrap();
    assert_eq!(id_list(subtypes), vec![s.comment]);
    // posting again changes nothing
    post_as(&mut tx, 42, TICKET, t, comment("Again"))
        .await
        .unwrap();
    assert_eq!(
        ids(&mut tx, "mail.followers", &following(TICKET, t, own)).await,
        rows
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn skip_author_subscribe_leaves_the_author_out() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let own = user(&mut tx, 42, "pat", false).await;
    let portal = user(&mut tx, 43, "pam", true).await;
    let t = ticket(&mut tx, "T").await;
    let mut request = comment("Hello");
    request.options.skip_author_subscribe = true;
    post_as(&mut tx, 42, TICKET, t, request).await.unwrap();
    assert!(
        ids(&mut tx, "mail.followers", &following(TICKET, t, own))
            .await
            .is_empty()
    );
    // a share (portal) author is never subscribed
    post_as(&mut tx, 43, TICKET, t, comment("Hi"))
        .await
        .unwrap();
    assert!(
        ids(&mut tx, "mail.followers", &following(TICKET, t, portal))
            .await
            .is_empty()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn autofollow_recipients_makes_the_recipients_follow() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let p45 = partner(&mut tx, "P45").await;
    let t = ticket(&mut tx, "T").await;
    let mut request = text_post("Welcome");
    request.partner_ids = vec![p45];
    request.options.autofollow_recipients = true;
    let posted = post_on(&mut tx, TICKET, t, request).await.unwrap();
    assert_eq!(
        ids(&mut tx, "mail.followers", &following(TICKET, t, p45))
            .await
            .len(),
        1
    );
    assert_eq!(posted.notified_partner_ids, vec![p45]);
    // without the option the recipient is notified but does not follow
    let p46 = partner(&mut tx, "P46").await;
    let mut request = text_post("Hi");
    request.partner_ids = vec![p46];
    post_on(&mut tx, TICKET, t, request).await.unwrap();
    assert!(
        ids(&mut tx, "mail.followers", &following(TICKET, t, p46))
            .await
            .is_empty()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn log_batch_writes_internal_notes_without_notifying() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let mut tickets = Vec::new();
    for i in 0..5 {
        tickets.push(ticket(&mut tx, &format!("T{i}")).await);
    }
    let fan = partner(&mut tx, "Fan").await;
    for t in &tickets {
        follower(&mut tx, TICKET, *t, fan, &[s.note, s.comment]).await;
    }
    let bodies: BTreeMap<i64, String> =
        tickets.iter().map(|t| (*t, format!("Log <{t}>"))).collect();
    let records = tx
        .env()
        .records(TICKET)
        .unwrap()
        .browse(tickets.clone())
        .unwrap();
    // the statement count (flush included) is the same for one record as for five: one ORM call.
    // A first call warms per-transaction caches (it counted 5 against 2 later on a build host), so
    // a throwaway call on another ticket precedes the measured ones.
    let warm = tx
        .env()
        .records(TICKET)
        .unwrap()
        .browse([ticket(&mut tx, "warm-up").await])
        .unwrap();
    let _: Vec<i64> = warm
        .call(
            &mut tx,
            "_message_log_batch",
            (BTreeMap::<i64, String>::new(),),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let first = tx
        .env()
        .records(TICKET)
        .unwrap()
        .browse([tickets[0]])
        .unwrap();
    let before = tx.query_count();
    let _: Vec<i64> = first
        .call(
            &mut tx,
            "_message_log_batch",
            (BTreeMap::from([(tickets[0], "one".to_owned())]),),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let one_count = tx.query_count() - before;
    let before = tx.query_count();
    let messages: Vec<i64> = records
        .call(&mut tx, "_message_log_batch", (bodies,))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let five_count = tx.query_count() - before;
    // read with `--nocapture`: the counts the design note records
    eprintln!("log_batch statements: records=1 count={one_count}; records=5 count={five_count}");
    assert_eq!(
        one_count, five_count,
        "the statement count changed with the record count"
    );
    assert_eq!(messages.len(), 5);
    for (message, t) in messages.iter().zip(&tickets) {
        assert_eq!(id_field(&mut tx, *message, "res_id").await, Some(*t));
        assert_eq!(
            field(&mut tx, *message, "is_internal").await,
            Cell::Bool(true)
        );
        assert_eq!(
            id_field(&mut tx, *message, "subtype_id").await,
            Some(s.note)
        );
        assert_eq!(
            str_field(&mut tx, *message, "body").await,
            format!("<p>Log &lt;{t}&gt;</p>")
        );
        assert!(id_list(field(&mut tx, *message, "notified_partner_ids").await).is_empty());
    }
    // a body for a record that is not logged is refused
    let one = tx
        .env()
        .records(TICKET)
        .unwrap()
        .browse([tickets[0]])
        .unwrap();
    let error = one
        .call::<_, Vec<i64>>(
            &mut tx,
            "_message_log_batch",
            (BTreeMap::from([(tickets[1], "x".to_owned())]),),
        )
        .await
        .unwrap_err();
    assert_validation(
        &error,
        &format!(
            "A log body is given for {TICKET} {}, which is not one of the records logged.",
            tickets[1]
        ),
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn log_batch_refuses_unsaved_and_repeated_ids() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    // an unsaved (in-memory) record has a negative id
    let tickets = tx.env().records(TICKET).unwrap();
    let unsaved = tx
        .new_record(&tickets, RawVals::new(), None, None)
        .await
        .unwrap();
    let unsaved_id = unsaved.rec_ids()[0];
    let error = unsaved
        .call::<_, Vec<i64>>(
            &mut tx,
            "_message_log_batch",
            (BTreeMap::<i64, String>::new(),),
        )
        .await
        .unwrap_err();
    assert_validation(
        &error,
        &format!("Only saved {TICKET} records are logged; unsaved ids: {unsaved_id}."),
    );
    // the same saved record twice
    let twice = tickets.browse([t, t]).unwrap();
    let error = twice
        .call::<_, Vec<i64>>(
            &mut tx,
            "_message_log_batch",
            (BTreeMap::<i64, String>::new(),),
        )
        .await
        .unwrap_err();
    assert_validation(
        &error,
        &format!("A record is logged once per call; repeated {TICKET} ids: {t}."),
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A user without a partner (or without a `res.users` row) cannot author a message (an invariant
/// error: kind `Programming`, condition invariant violation): both the post
/// and the log batch fail loudly and write nothing.
#[tokio::test]
async fn an_author_without_a_partner_fails_loudly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    // user 1 without partner
    set_user_partner(&mut tx, 1, None).await;
    let error = post_on(&mut tx, TICKET, t, text_post("Hi"))
        .await
        .unwrap_err();
    let message = "User 1 has no partner (no res.users row, or its partner_id is empty): a message \
                   needs an author partner. The superuser's partner link comes with \
                   a later release.";
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), message);
    let records = record_of(&tx.env(), TICKET, t);
    let error = records
        .call::<_, Vec<i64>>(
            &mut tx,
            "_message_log_batch",
            (BTreeMap::from([(t, "x".to_owned())]),),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), message);
    // a user id with no res.users row at all: the post and the log batch
    let no_row = "User 777 has no partner (no res.users row, or its partner_id is empty): a \
                  message needs an author partner.";
    let record = record_of(&tx.env().with_user(777), TICKET, t);
    let error = post(&mut tx, &record, text_post("Hi")).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), no_row);
    let error = record
        .call::<_, Vec<i64>>(
            &mut tx,
            "_message_log_batch",
            (BTreeMap::from([(t, "x".to_owned())]),),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(error.message(), no_row);
    let count = ids(&mut tx, "mail.message", &Domain::TRUE).await;
    assert!(count.is_empty(), "{count:?}");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// an HTML body is refused because the post API has no trusted-markup input)
#[tokio::test]
async fn a_text_body_is_escaped_and_html_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let ticket = record_of(&tx.env(), TICKET, t);
    let posted = post(&mut tx, &ticket, text_post("<script>alert(1)</script>"))
        .await
        .unwrap();
    assert_eq!(
        str_field(&mut tx, posted.message_id, "body").await,
        "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>"
    );
    let error = post(
        &mut tx,
        &ticket,
        PostRequest::new(Body::Html("<b>Bold</b>".to_owned())),
    )
    .await
    .unwrap_err();
    assert_validation(
        &error,
        "An HTML body is refused until HTML fields are sanitized (a later release); post \
         the text with its mentions instead.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// plaintext conversion; mention anchors keep the original byte offsets and their own text)
#[tokio::test]
async fn line_breaks_of_a_text_body_become_br_outside_anchors() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let ann = partner(&mut tx, "Ann").await;
    let t = ticket(&mut tx, "T").await;
    let ticket = record_of(&tx.env(), TICKET, t);
    let anchor = |text: &str| {
        format!(
            "<a class=\"o_mail_redirect\" data-oe-model=\"res.partner\" data-oe-id=\"{ann}\">{text}</a>"
        )
    };
    // a mention on a later line, with LF, CRLF and CR breaks and an escaped character
    let mut request = text_post("line1\nline2 @Ann\r\nline3\rl<4");
    request.mentions = vec![Mention {
        start: 12,
        end: 16,
        target: MentionTarget::Partner(ann),
    }];
    let posted = post(&mut tx, &ticket, request).await.unwrap();
    assert_eq!(
        str_field(&mut tx, posted.message_id, "body").await,
        format!(
            "<p>line1<br>line2 {}<br>line3<br>l&lt;4</p>",
            anchor("@Ann")
        )
    );
    // a mention across lines keeps its own line break raw: no markup inside an anchor
    let mut request = text_post("Hi @Ann\nLee bye\n");
    request.mentions = vec![Mention {
        start: 3,
        end: 11,
        target: MentionTarget::Partner(ann),
    }];
    let posted = post(&mut tx, &ticket, request).await.unwrap();
    assert_eq!(
        str_field(&mut tx, posted.message_id, "body").await,
        format!("<p>Hi {} bye<br></p>", anchor("@Ann\nLee"))
    );
    // CRLF is one break, a lone CR is one, a body of only breaks is only breaks
    let posted = post(&mut tx, &ticket, text_post("\n\r\n\r\n\n"))
        .await
        .unwrap();
    assert_eq!(
        str_field(&mut tx, posted.message_id, "body").await,
        "<br><br><br><br>"
    );
    let posted = post(&mut tx, &ticket, text_post("a\r\nb")).await.unwrap();
    assert_eq!(
        str_field(&mut tx, posted.message_id, "body").await,
        "<p>a<br>b</p>"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// overrides it; the notification is the notified-partner rows only, inbox rows, email and bus
/// events come with later slices)
#[tokio::test]
async fn the_after_hook_runs_with_the_new_message() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let chat = create(&mut tx, CHAT, RawVals::new().set_cell("name", text("C"))).await;
    let fan = partner(&mut tx, "Fan").await;
    follower(&mut tx, CHAT, chat, fan, &[s.comment]).await;
    let mut request = text_post("x");
    request.subtype = PostSubtype::Comment;
    let posted = post_on(&mut tx, CHAT, chat, request).await.unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let name = record_of(&tx.env(), CHAT, chat)
        .cell(&mut tx, "name")
        .await
        .unwrap();
    assert_eq!(
        name,
        Cell::Str(format!("after hook: {}", posted.message_id).into())
    );
    assert_eq!(posted.notified_partner_ids, vec![fan]);
    assert_eq!(
        id_list(field(&mut tx, posted.message_id, "notified_partner_ids").await),
        vec![fan]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// partners, internal subtypes reach internal partners only, inactive partners and the author are
/// left out; notification preference, language, user and groups are not returned yet)
#[tokio::test]
async fn recipients_are_the_subtype_followers_and_the_explicit_partners() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let other = subtype(&mut tx, "Other", RawVals::new()).await;
    let own = user(&mut tx, 42, "pat", false).await;
    let internal = user(&mut tx, 43, "ian", false).await;
    let portal = user(&mut tx, 44, "pam", true).await;
    let elsewhere = partner(&mut tx, "Elsewhere").await;
    let archived = inactive_partner(&mut tx, "Archived").await;
    let explicit = partner(&mut tx, "Explicit").await;
    let explicit_archived = inactive_partner(&mut tx, "Explicit archived").await;
    let t = ticket(&mut tx, "T").await;
    for p in [own, internal, portal, archived] {
        follower(&mut tx, TICKET, t, p, &[s.comment, s.note]).await;
    }
    follower(&mut tx, TICKET, t, elsewhere, &[other]).await;
    let mut request = text_post("note");
    request.partner_ids = vec![explicit, explicit_archived];
    let posted = post_as(&mut tx, 42, TICKET, t, request).await.unwrap();
    let mut expected = vec![internal, explicit];
    expected.sort_unstable();
    assert_eq!(posted.notified_partner_ids, expected);
    assert_eq!(
        id_list(field(&mut tx, posted.message_id, "notified_partner_ids").await),
        expected
    );
    // the explicit recipients are stored, the archived one included
    let mut to = vec![explicit, explicit_archived];
    to.sort_unstable();
    assert_eq!(
        id_list(field(&mut tx, posted.message_id, "partner_ids").await),
        to
    );
    // a comment (not internal) reaches the portal follower too
    let posted = post_as(&mut tx, 42, TICKET, t, comment("hi"))
        .await
        .unwrap();
    let mut expected = vec![internal, portal];
    expected.sort_unstable();
    assert_eq!(posted.notified_partner_ids, expected);
    // an unknown partner is refused
    let mut request = text_post("x");
    request.partner_ids = vec![9_000_000];
    let error = post_as(&mut tx, 42, TICKET, t, request).await.unwrap_err();
    assert_validation(&error, "Unknown partner ids: 9000000.");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The post's statement count is the same for 5, 50 and 500 followers (only row counts change),
/// and a replay of the same `client_uid` is cheaper; both ceilings are ratchets
/// (DEFERRED_PERFORMANCE), the exact §5.1 count is a milestone-end target. With
/// `MAIL_POST_STATEMENT_LOG` set, the 50-follower post is logged by the server
/// (`log_statement`), between marker statements, for the evidence script.
#[tokio::test]
async fn post_statements_do_not_grow_with_followers() {
    let h = harness().await;
    let log = std::env::var_os("MAIL_POST_STATEMENT_LOG").is_some();
    let mut counts = Vec::new();
    for n in [5_u64, 50, 500] {
        let mut tx = h.begin().await.unwrap();
        let s = seed(&mut tx).await;
        let own = user(&mut tx, 42, "pat", false).await;
        let t = ticket(&mut tx, "T").await;
        follower(&mut tx, TICKET, t, own, &[s.comment]).await;
        let partners = tx
            .env()
            .records("res.partner")
            .unwrap()
            .create(
                &mut tx,
                (0..n)
                    .map(|i| RawVals::new().set_cell("name", text(&format!("F{i}"))))
                    .collect(),
            )
            .await
            .unwrap()
            .ids();
        let comment_id = RecId::from_real(s.comment).unwrap();
        tx.env()
            .records("mail.followers")
            .unwrap()
            .create(
                &mut tx,
                partners
                    .iter()
                    .map(|p| {
                        RawVals::new()
                            .set_cell("res_model", text(TICKET))
                            .set_cell("res_id", Cell::Id(t))
                            .set_cell("partner_id", Cell::Id(*p))
                            .set_commands("subtype_ids", [Command::set(vec![comment_id])])
                    })
                    .collect(),
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        tx.invalidate_all().await.unwrap();
        let record = record_of(&tx.env().with_user(42), TICKET, t);
        let mut request = comment("Hello");
        request.client_uid = Some(uuid(n));
        let marked = log && n == 50;
        if marked {
            tx.execute("SET LOCAL log_statement = 'all'", &[])
                .await
                .unwrap();
            tx.execute("SELECT 'erpforall-evidence: post begin'", &[])
                .await
                .unwrap();
        }
        let before = tx.query_count();
        let posted = post(&mut tx, &record, request.clone()).await.unwrap();
        tx.flush().await.unwrap();
        let count = tx.query_count() - before;
        if marked {
            tx.execute("SELECT 'erpforall-evidence: replay begin'", &[])
                .await
                .unwrap();
        }
        let before = tx.query_count();
        let replay = post(&mut tx, &record, request).await.unwrap();
        let replay_count = tx.query_count() - before;
        if marked {
            tx.execute("SELECT 'erpforall-evidence: end'", &[])
                .await
                .unwrap();
        }
        assert_eq!(posted.notified_partner_ids.len() as u64, n);
        assert!(replay.replayed);
        assert_eq!(replay.message_id, posted.message_id);
        assert_eq!(replay.notified_partner_ids, posted.notified_partner_ids);
        // read with `--nocapture`: the counts the ceilings were set from
        eprintln!("post statements: followers={n} post={count} replay={replay_count}");
        counts.push((count, replay_count));
        tx.rollback().await.unwrap();
        drop(tx);
    }
    assert!(
        counts.windows(2).all(|pair| pair[0] == pair[1]),
        "the statement count changed with the follower count: {counts:?}"
    );
    let (post_count, replay_count) = counts[0];
    assert!(
        (1..=POST_CEILING).contains(&post_count),
        "post: {post_count}"
    );
    assert!(
        (1..=REPLAY_CEILING).contains(&replay_count),
        "replay: {replay_count}"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_client_uid_replays_only_for_the_same_user_and_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "pat", false).await;
    user(&mut tx, 43, "ian", false).await;
    let fan = partner(&mut tx, "Fan").await;
    let (t, t2) = (ticket(&mut tx, "T").await, ticket(&mut tx, "T2").await);
    let mut request = text_post("once");
    request.partner_ids = vec![fan];
    request.client_uid = Some(uuid(1));
    let first = post_as(&mut tx, 42, TICKET, t, request.clone())
        .await
        .unwrap();
    let again = post_as(&mut tx, 42, TICKET, t, request.clone())
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.message_id, first.message_id);
    assert_eq!(again.notified_partner_ids, vec![fan]);
    let messages = ids(
        &mut tx,
        "mail.message",
        &(Domain::condition("model", Operator::Eq, TICKET).unwrap()
            & Domain::condition("res_id", Operator::Eq, t).unwrap()),
    )
    .await;
    assert_eq!(messages, vec![first.message_id]);
    let other_record = post_as(&mut tx, 42, TICKET, t2, request.clone())
        .await
        .unwrap_err();
    assert_validation(
        &other_record,
        "This client_uid was used for another post; choose a new one.",
    );
    let other_user = post_as(&mut tx, 43, TICKET, t, request).await.unwrap_err();
    assert_validation(
        &other_user,
        "This client_uid was used for another post; choose a new one.",
    );
    let mut nil = text_post("nil");
    nil.client_uid = Some(Uuid::parse("00000000-0000-0000-0000-000000000000").unwrap());
    let error = post_as(&mut tx, 42, TICKET, t, nil).await.unwrap_err();
    assert_validation(&error, "A client_uid must not be the nil UUID.");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Two transactions post with the same `client_uid` at the same moment: the second one does not
/// see the first one's message, and its insert fails on the unique key.
#[tokio::test]
async fn a_concurrent_duplicate_client_uid_fails_on_the_key() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    seed(&mut setup).await;
    let t = ticket(&mut setup, "T").await;
    setup.commit().await.unwrap();
    let mut a = h.begin().await.unwrap();
    let mut b = h.begin().await.unwrap();
    // b's snapshot is taken before a commits
    b.execute("SELECT 1", &[]).await.unwrap();
    let mut request = text_post("race");
    request.client_uid = Some(uuid(7));
    let first = post_on(&mut a, TICKET, t, request.clone()).await.unwrap();
    a.commit().await.unwrap();
    let error = post_on(&mut b, TICKET, t, request).await.unwrap_err();
    assert_validation(
        &error,
        "The database refused the change: A message with this client_uid exists already.",
    );
    b.rollback().await.unwrap();
    drop((a, b));
    // the committed message is the only one
    let mut check = h.begin().await.unwrap();
    let found = ids(
        &mut check,
        "mail.message",
        &Domain::condition("res_id", Operator::Eq, t).unwrap(),
    )
    .await;
    assert_eq!(found, vec![first.message_id]);
    check.rollback().await.unwrap();
    drop(check);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn mentions_render_as_anchors_and_notify_the_partner() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let ann = partner(&mut tx, "Ann").await;
    let t = ticket(&mut tx, "T").await;
    let ticket = record_of(&tx.env(), TICKET, t);
    let mut request = text_post("Hi @Ann <3");
    request.mentions = vec![Mention {
        start: 3,
        end: 7,
        target: MentionTarget::Partner(ann),
    }];
    let posted = post(&mut tx, &ticket, request).await.unwrap();
    assert_eq!(
        str_field(&mut tx, posted.message_id, "body").await,
        format!(
            "<p>Hi <a class=\"o_mail_redirect\" data-oe-model=\"res.partner\" \
             data-oe-id=\"{ann}\">@Ann</a> &lt;3</p>"
        )
    );
    assert_eq!(posted.notified_partner_ids, vec![ann]);
    assert_eq!(
        id_list(field(&mut tx, posted.message_id, "partner_ids").await),
        vec![ann]
    );
    // an anchor typed in the text is text, and reaches nobody
    let forged = format!("<a data-oe-model=\"res.partner\" data-oe-id=\"{ann}\">@Ann</a>");
    let posted = post(&mut tx, &ticket, text_post(&forged)).await.unwrap();
    assert!(posted.notified_partner_ids.is_empty());
    let mut request = text_post("@everyone");
    request.mentions = vec![Mention {
        start: 0,
        end: 9,
        target: MentionTarget::Everyone,
    }];
    let error = post(&mut tx, &ticket, request).await.unwrap_err();
    assert_validation(
        &error,
        "Only partner mentions are accepted on a record thread; channel, role, everyone and here \
         mentions come with a later release.",
    );
    let mut request = text_post("@ghost");
    request.mentions = vec![Mention {
        start: 0,
        end: 6,
        target: MentionTarget::Partner(9_000_000),
    }];
    let error = post(&mut tx, &ticket, request).await.unwrap_err();
    assert_validation(&error, "Unknown partner ids: 9000000.");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn silent_is_stored_and_returned() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let ticket = record_of(&tx.env(), TICKET, t);
    let mut request = text_post("quiet");
    request.silent = true;
    let posted = post(&mut tx, &ticket, request).await.unwrap();
    assert!(posted.silent);
    assert_eq!(
        field(&mut tx, posted.message_id, "silent").await,
        Cell::Bool(true)
    );
    let loud = post(&mut tx, &ticket, text_post("loud")).await.unwrap();
    assert!(!loud.silent);
    assert_eq!(
        field(&mut tx, loud.message_id, "silent").await,
        Cell::Bool(false)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn post_inputs_are_validated() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let foreign = subtype(
        &mut tx,
        "Foreign",
        RawVals::new().set_cell("res_model", text("res.partner")),
    )
    .await;
    let t = ticket(&mut tx, "T").await;
    let ticket = record_of(&tx.env(), TICKET, t);
    let error = post(&mut tx, &ticket, text_post(&"x".repeat((1 << 20) + 1)))
        .await
        .unwrap_err();
    assert_validation(
        &error,
        "A post body holds at most 1048576 bytes, not 1048577.",
    );
    let mut request = text_post("x");
    request.partner_ids = (1..=1001).collect();
    let error = post(&mut tx, &ticket, request).await.unwrap_err();
    assert_validation(
        &error,
        "At most 1000 partner ids are accepted per post, not 1001.",
    );
    let mut request = text_post("x");
    request.partner_ids = vec![-1];
    let error = post(&mut tx, &ticket, request).await.unwrap_err();
    assert_validation(&error, "A partner id must be a positive integer, not -1.");
    let mut request = text_post("x");
    request.subtype = PostSubtype::Id(foreign);
    let error = post(&mut tx, &ticket, request).await.unwrap_err();
    assert_validation(
        &error,
        &format!(
            "Unknown subtype id {foreign} for {TICKET}: a subtype must exist and have no model or \
             the model of the record."
        ),
    );
    let mut request = text_post("x");
    request.subtype = PostSubtype::Xmlid("mail.mt_nothing".to_owned());
    let error = post(&mut tx, &ticket, request).await.unwrap_err();
    assert_validation(
        &error,
        &format!(
            "Unknown subtype 'mail.mt_nothing' for {TICKET}: a subtype must exist and have no \
             model or the model of the record."
        ),
    );
    let mut request = text_post("x");
    request.subtype = PostSubtype::Xmlid("mt_comment".to_owned());
    let error = post(&mut tx, &ticket, request).await.unwrap_err();
    assert_validation(
        &error,
        "A subtype external id is 'module.name', not 'mt_comment'.",
    );
    // a subtype by external id works
    let mut request = text_post("x");
    request.subtype = PostSubtype::Xmlid("mail.mt_comment".to_owned());
    post(&mut tx, &ticket, request).await.unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
