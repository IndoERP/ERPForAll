//! `mail.followers` storage and the follower methods of `mail.thread` (`followers`, `follow`,
//! `unfollow`, `set_subtypes`) with their statement ceilings (ratchets; the §15.2 budget of
//! is `DEFERRED_PERFORMANCE`), the computed and
//! inverse `message_partner_ids`, and the thread `unlink` cleanup.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation};
use erp_testkit::{DomainPolicy, FieldPolicy, Harness, Otherwise, assert_query_count};
use support::*;

const TICKET: &str = "test_mail.ticket";
/// The engine's refusal of the test policies below (texts of the access engine, not of `mail`).
const WRITE_DENIED: &str =
    "You are not allowed to write 'Mail Test Ticket' (test_mail.ticket) records.";
const UNLINK_DENIED: &str =
    "You are not allowed to unlink 'Mail Test Ticket' (test_mail.ticket) records.";

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .build()
    .await
    .unwrap()
}

/// A harness whose policy refuses every write on tickets (reads allowed) and knows the internal
/// user group, so a user with no group is a non-internal user.
async fn write_denied_harness() -> Harness {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(TICKET, Operation::Write, Domain::FALSE)
        .group_xmlid("base.group_user", erp_sdk::access::GroupId::new(1).unwrap())
        .build()
        .unwrap();
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap()
}

/// User 42 with its own partner, created by the superuser transaction; returns the partner.
async fn user_with_partner(tx: &mut Transaction) -> i64 {
    let partner = partner(tx, "Portal Pat").await;
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active, partner_id) VALUES (42, 'Pat', 'pat', true, $1)",
        &[&partner],
    )
    .await
    .unwrap();
    partner
}

fn ticket_of(env: &Env, id: i64) -> Records {
    env.records(TICKET).unwrap().browse([id]).unwrap()
}

fn of_ticket(id: i64) -> Domain {
    Domain::condition("res_model", Operator::Eq, TICKET).unwrap()
        & Domain::condition("res_id", Operator::Eq, id).unwrap()
}

#[tokio::test]
async fn a_partner_follows_a_record_once() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let p = partner(&mut tx, "P").await;
    follower(&mut tx, TICKET, t, p, &[]).await;
    tx.flush().await.unwrap();
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("mail.followers")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("res_model", text(TICKET))
                            .set_cell("res_id", Cell::Id(t))
                            .set_cell("partner_id", Cell::Id(p)),
                    ],
                )
                .await?;
            tx.flush().await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The operation cannot be completed: A partner can follow a document only once."
    );
    assert_eq!(ids(&mut tx, "mail.followers", &of_ticket(t)).await.len(), 1);
    // the same partner may follow another record of the same model
    let t2 = ticket(&mut tx, "T2").await;
    follower(&mut tx, TICKET, t2, p, &[]).await;
    tx.flush().await.unwrap();
    assert_eq!(
        ids(&mut tx, "mail.followers", &of_ticket(t2)).await.len(),
        1
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// model and of res.partner)
#[tokio::test]
async fn message_partner_ids_lists_the_followers_partners() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let (p1, p2, p3) = (
        partner(&mut tx, "P1").await,
        partner(&mut tx, "P2").await,
        partner(&mut tx, "P3").await,
    );
    follower(&mut tx, TICKET, t, p1, &[]).await;
    follower(&mut tx, TICKET, t, p2, &[]).await;
    // p3 follows p1 (a res.partner thread), not the ticket
    follower(&mut tx, "res.partner", p1, p3, &[]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let env = tx.env();
    let mut expected = vec![p1, p2];
    expected.sort_unstable();
    assert_eq!(
        id_list(
            ticket_of(&env, t)
                .cell(&mut tx, "message_partner_ids")
                .await
                .unwrap()
        ),
        expected
    );
    let p1_record = env.records("res.partner").unwrap().browse([p1]).unwrap();
    assert_eq!(
        id_list(
            p1_record
                .cell(&mut tx, "message_partner_ids")
                .await
                .unwrap()
        ),
        vec![p3]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn followers_pages_one_record_in_id_order_within_three_statements() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let other = ticket(&mut tx, "Other").await;
    let mut created = Vec::new();
    for name in ["A", "B", "C"] {
        let p = partner(&mut tx, name).await;
        created.push(follower(&mut tx, TICKET, t, p, &[]).await);
    }
    let stranger = partner(&mut tx, "S").await;
    follower(&mut tx, TICKET, other, stranger, &[]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    let first: Records = assert_query_count!(tx, >= 1, <= 3, {
        record
            .call(&mut tx, "followers", (None::<i64>, 2_u32))
            .await
            .unwrap()
    });
    assert_eq!(first.ids(), created[..2].to_vec());
    let rest: Records = record
        .call(&mut tx, "followers", (Some(created[1]), 2_u32))
        .await
        .unwrap();
    assert_eq!(rest.ids(), created[2..].to_vec());
    for (limit, message) in [
        (0_u32, "A follower page holds 1 to 100 followers, not 0."),
        (101, "A follower page holds 1 to 100 followers, not 101."),
    ] {
        let error = record
            .call::<_, Records>(&mut tx, "followers", (None::<i64>, limit))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), message);
    }
    let error = record
        .call::<_, Records>(&mut tx, "followers", (Some(0_i64), 10_u32))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "A follower page starts after a follower id, not 0."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// followers stay, and removing another partner needs write access; the internal user's
/// self-removal without any right is implemented but not exercised by a test policy here)
#[tokio::test]
async fn unfollow_removes_the_partners_followers() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let t2 = ticket(&mut tx, "T2").await;
    let (p1, p2) = (partner(&mut tx, "P1").await, partner(&mut tx, "P2").await);
    follower(&mut tx, TICKET, t, p1, &[]).await;
    let kept = follower(&mut tx, TICKET, t, p2, &[]).await;
    let elsewhere = follower(&mut tx, TICKET, t2, p1, &[]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    record
        .call::<_, ()>(&mut tx, "unfollow", (vec![p1],))
        .await
        .unwrap();
    assert_eq!(
        ids(&mut tx, "mail.followers", &of_ticket(t)).await,
        vec![kept]
    );
    assert_eq!(
        ids(&mut tx, "mail.followers", &of_ticket(t2)).await,
        vec![elsewhere]
    );
    // a partner that does not follow is ignored; no id, no statement
    record
        .call::<_, ()>(&mut tx, "unfollow", (vec![p1],))
        .await
        .unwrap();
    assert_query_count!(tx, <= 0, {
        record
            .call::<_, ()>(&mut tx, "unfollow", (Vec::<i64>::new(),))
            .await
            .unwrap()
    });
    let error = record
        .call::<_, ()>(&mut tx, "unfollow", (vec![p2, -3],))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "A partner id must be a positive integer, not -3."
    );
    let error = record
        .call::<_, ()>(&mut tx, "unfollow", ((1..=1001).collect::<Vec<i64>>(),))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "At most 1000 partner ids are accepted per call, not 1001."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// one's own partner asks the policy whether the user is internal and fails loudly when the policy
/// cannot answer, never assuming either answer)
#[tokio::test]
async fn unfollow_others_needs_write_access_and_self_asks_the_policy() {
    let h = write_denied_harness().await;
    let mut tx = h.begin().await.unwrap();
    let own = user_with_partner(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let other = partner(&mut tx, "Other").await;
    let mine = follower(&mut tx, TICKET, t, own, &[]).await;
    let kept = follower(&mut tx, TICKET, t, other, &[]).await;
    tx.flush().await.unwrap();
    let user = tx.env().with_user(42);
    let record = ticket_of(&user, t);
    // the page is found with privileges but handed back in the caller's environment (uid 42,
    // not su); the transaction itself is the harness's superuser one
    let page: Records = record
        .call(&mut tx, "followers", (None::<i64>, 10_u32))
        .await
        .unwrap();
    assert!(!page.env().su());
    assert_eq!(page.env().uid(), 42);
    assert_eq!(page.ids().len(), 2);
    let error = record
        .call::<_, ()>(&mut tx, "unfollow", (vec![other],))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), WRITE_DENIED);
    let error_both = record
        .call::<_, ()>(&mut tx, "unfollow", (vec![own, other],))
        .await
        .unwrap_err();
    assert_eq!(error_both.kind(), ErrorKind::Access);
    assert_eq!(error_both.message(), error.message());
    // the test policy cannot evaluate group specs: the self path refuses instead of guessing
    let error = record
        .call::<_, ()>(&mut tx, "unfollow", (vec![own],))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "this access policy does not evaluate group specs (has_groups): use a compiled policy \
         snapshot"
    );
    let mut both = ids(&mut tx, "mail.followers", &of_ticket(t)).await;
    both.sort_unstable();
    let mut expected = vec![mine, kept];
    expected.sort_unstable();
    assert_eq!(both, expected);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `set_subtypes`; the `follow` route is `follow_with_subtypes_replaces_existing_followers`)
#[tokio::test]
async fn set_subtypes_replaces_a_followers_subtypes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let p = partner(&mut tx, "P").await;
    let s1 = subtype(&mut tx, "S1", RawVals::new()).await;
    let s2 = subtype(&mut tx, "S2", RawVals::new()).await;
    let f = follower(&mut tx, TICKET, t, p, &[s1]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    record
        .call::<_, ()>(&mut tx, "set_subtypes", (p, vec![s2]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let follower_record = tx
        .env()
        .records("mail.followers")
        .unwrap()
        .browse([f])
        .unwrap();
    assert_eq!(
        id_list(follower_record.cell(&mut tx, "subtype_ids").await.unwrap()),
        vec![s2]
    );
    // an empty set is refused and leaves the follower as it was
    let error = record
        .call::<_, ()>(&mut tx, "set_subtypes", (p, Vec::<i64>::new()))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Choose at least one subtype, or unfollow.");
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        id_list(follower_record.cell(&mut tx, "subtype_ids").await.unwrap()),
        vec![s2]
    );
    let stranger = partner(&mut tx, "Stranger").await;
    let error = record
        .call::<_, ()>(&mut tx, "set_subtypes", (stranger, vec![s1]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Missing);
    assert_eq!(
        error.message(),
        format!("Partner {stranger} does not follow test_mail.ticket {t}.")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn set_subtypes_of_another_partner_needs_write_access() {
    let h = write_denied_harness().await;
    let mut tx = h.begin().await.unwrap();
    let own = user_with_partner(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let other = partner(&mut tx, "Other").await;
    let s1 = subtype(&mut tx, "S1", RawVals::new()).await;
    let mine = follower(&mut tx, TICKET, t, own, &[]).await;
    follower(&mut tx, TICKET, t, other, &[]).await;
    tx.flush().await.unwrap();
    let record = ticket_of(&tx.env().with_user(42), t);
    let error = record
        .call::<_, ()>(&mut tx, "set_subtypes", (other, vec![s1]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), WRITE_DENIED);
    record
        .call::<_, ()>(&mut tx, "set_subtypes", (own, vec![s1]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let follower_record = tx
        .env()
        .records("mail.followers")
        .unwrap()
        .browse([mine])
        .unwrap();
    assert_eq!(
        id_list(follower_record.cell(&mut tx, "subtype_ids").await.unwrap()),
        vec![s1]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// other records and of another model with the same id stay; scheduled messages arrive with a
/// later slice)
#[tokio::test]
async fn deleting_a_thread_deletes_its_messages_and_followers() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let t2 = ticket(&mut tx, "T2").await;
    let (p1, p2) = (partner(&mut tx, "P1").await, partner(&mut tx, "P2").await);
    for body in ["one", "two", "three"] {
        message(&mut tx, TICKET, t, body).await;
    }
    follower(&mut tx, TICKET, t, p1, &[]).await;
    follower(&mut tx, TICKET, t, p2, &[]).await;
    let kept_message = message(&mut tx, TICKET, t2, "stays").await;
    let kept_follower = follower(&mut tx, TICKET, t2, p1, &[]).await;
    // same id, other model: not the deleted record's thread
    let foreign_message = message(&mut tx, "res.partner", t, "foreign").await;
    let foreign_follower = follower(&mut tx, "res.partner", t, p2, &[]).await;
    tx.flush().await.unwrap();
    let messages_of = |model: &str, id: i64| {
        Domain::condition("model", Operator::Eq, model).unwrap()
            & Domain::condition("res_id", Operator::Eq, id).unwrap()
    };
    let mut own = ids(&mut tx, "mail.message", &messages_of(TICKET, t)).await;
    assert_eq!(own.len(), 3);
    // message_ids lists the thread's messages, user-specific notifications excluded
    let notice = message(&mut tx, TICKET, t, "only for one user").await;
    tx.env()
        .records("mail.message")
        .unwrap()
        .browse([notice])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("message_type", text("user_notification")),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    own.sort_unstable();
    assert_eq!(
        id_list(
            ticket_of(&tx.env(), t)
                .cell(&mut tx, "message_ids")
                .await
                .unwrap()
        ),
        own
    );

    ticket_of(&tx.env(), t).unlink(&mut tx).await.unwrap();
    assert!(
        ids(&mut tx, "mail.message", &messages_of(TICKET, t))
            .await
            .is_empty()
    );
    assert!(
        ids(&mut tx, "mail.followers", &of_ticket(t))
            .await
            .is_empty()
    );
    assert_eq!(
        ids(&mut tx, "mail.message", &messages_of(TICKET, t2)).await,
        vec![kept_message]
    );
    assert_eq!(
        ids(&mut tx, "mail.followers", &of_ticket(t2)).await,
        vec![kept_follower]
    );
    assert_eq!(
        ids(&mut tx, "mail.message", &messages_of("res.partner", t)).await,
        vec![foreign_message]
    );
    let partner_followers = Domain::condition("res_model", Operator::Eq, "res.partner").unwrap()
        & Domain::condition("res_id", Operator::Eq, t).unwrap();
    assert_eq!(
        ids(&mut tx, "mail.followers", &partner_followers).await,
        vec![foreign_follower]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A thread the caller may not delete keeps its messages and followers.
#[tokio::test]
async fn a_refused_thread_deletion_deletes_nothing() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(TICKET, Operation::Unlink, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES (42, 'U', 'u', true)",
        &[],
    )
    .await
    .unwrap();
    let t = ticket(&mut tx, "T").await;
    let p = partner(&mut tx, "P").await;
    let m = message(&mut tx, TICKET, t, "kept").await;
    let f = follower(&mut tx, TICKET, t, p, &[]).await;
    tx.flush().await.unwrap();
    let error = tx
        .savepoint(async |tx| ticket_of(&tx.env().with_user(42), t).unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), UNLINK_DENIED);
    let of_messages = Domain::condition("model", Operator::Eq, TICKET).unwrap()
        & Domain::condition("res_id", Operator::Eq, t).unwrap();
    assert_eq!(ids(&mut tx, "mail.message", &of_messages).await, vec![m]);
    assert_eq!(ids(&mut tx, "mail.followers", &of_ticket(t)).await, vec![f]);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Evidence for why `follow` creates and updates follower rows through the ORM and not through the
/// upsert (design note "Assumptions and stops", lead decision 2): the upsert, the only
/// one-statement insert-or-keep path addon code has, refuses the subtype links a follower needs,
/// so `follow` takes more statements than the deferred §15.2 target. The ORM upsert is not changed
/// in this slice; if it ever accepts x2many values this test fails and `follow` can be revisited.
#[tokio::test]
async fn follower_upsert_refuses_subtype_links() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let p = partner(&mut tx, "P").await;
    let s = subtype(&mut tx, "S", RawVals::new()).await;
    tx.flush().await.unwrap();
    let call =
        UpsertCall::new("_mail_followers_res_partner_res_model_id_uniq").with_rows([RawVals::new(
        )
        .set_cell("res_model", text(TICKET))
        .set_cell("res_id", Cell::Id(t))
        .set_cell("partner_id", Cell::Id(p))
        .set_commands(
            "subtype_ids",
            [Command::set([RecId::from_real(s).unwrap()])],
        )]);
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("mail.followers")?
                .upsert(tx, call)
                .await
                .map(drop)
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "upsert on 'mail.followers': row 0: x2many values ('subtype_ids') are not supported by \
         upsert"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The ceiling may only
/// go down (design note "Statement budgets"). Run as the superuser only.
#[tokio::test]
async fn unfollow_statement_ceiling_is_six() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let p = partner(&mut tx, "P").await;
    follower(&mut tx, TICKET, t, p, &[]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    let before = tx.query_count();
    assert_query_count!(tx, >= 1, <= 6, {
        record
            .call::<_, ()>(&mut tx, "unfollow", (vec![p],))
            .await
            .unwrap()
    });
    // read with `--nocapture`: the count the ceiling was set from
    eprintln!("unfollow statements: {}", tx.query_count() - before);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Statement ceiling of `set_subtypes` (flush included), a RATCHET and DEFERRED_PERFORMANCE
/// , not the §15.2 budget (three), which is lowered at milestone end.
/// The ceiling may only go down. Run as the superuser only.
#[tokio::test]
async fn set_subtypes_statement_ceiling_is_six() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let p = partner(&mut tx, "P").await;
    let s1 = subtype(&mut tx, "S1", RawVals::new()).await;
    let s2 = subtype(&mut tx, "S2", RawVals::new()).await;
    follower(&mut tx, TICKET, t, p, &[s1]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    let before = tx.query_count();
    assert_query_count!(tx, >= 1, <= 6, {
        record
            .call::<_, ()>(&mut tx, "set_subtypes", (p, vec![s2]))
            .await
            .unwrap();
        tx.flush().await.unwrap()
    });
    // read with `--nocapture`: the count the ceiling was set from
    eprintln!("set_subtypes statements: {}", tx.query_count() - before);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A real user row for `partner` with the stored user type `share` that base keeps up to date
/// (false: an internal user, true: a portal user). `login` must be unique in the transaction.
async fn add_user(
    tx: &mut Transaction,
    id: i64,
    login: &str,
    partner: i64,
    share: bool,
    active: bool,
) {
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active, share, partner_id) VALUES ($1, $2, $2, $4, $5, $3)",
        &[&id, &login, &partner, &active, &share],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
}

async fn subtypes_of(tx: &mut Transaction, follower: i64) -> Vec<i64> {
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let rows = tx
        .env()
        .records("mail.followers")
        .unwrap()
        .browse([follower])
        .unwrap();
    id_list(rows.cell(tx, "subtype_ids").await.unwrap())
}

/// The follower row of (`TICKET`, `res_id`, `partner`), if any.
async fn row_of(tx: &mut Transaction, res_id: i64, partner: i64) -> Option<i64> {
    let domain =
        of_ticket(res_id) & Domain::condition("partner_id", Operator::Eq, partner).unwrap();
    ids(tx, "mail.followers", &domain).await.first().copied()
}

#[tokio::test]
async fn follow_without_subtypes_gives_the_default_subtypes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let (internal, portal, kept) = (
        partner(&mut tx, "Internal").await,
        partner(&mut tx, "Portal").await,
        partner(&mut tx, "Kept").await,
    );
    // internal: an active internal user; portal: a portal user; kept: an internal user too.
    // `lapsed` has only an archived internal user and `nobody` no user: both count as share.
    let (lapsed, nobody) = (
        partner(&mut tx, "Lapsed").await,
        partner(&mut tx, "Nobody").await,
    );
    add_user(&mut tx, 101, "internal", internal, false, true).await;
    add_user(&mut tx, 102, "portal", portal, true, true).await;
    add_user(&mut tx, 103, "kept", kept, false, true).await;
    add_user(&mut tx, 104, "lapsed", lapsed, false, false).await;
    let public_default = subtype(&mut tx, "Public default", RawVals::new()).await;
    let ticket_internal = subtype(
        &mut tx,
        "Ticket internal",
        RawVals::new()
            .set_cell("res_model", text(TICKET))
            .set_cell("internal", Cell::Bool(true)),
    )
    .await;
    let not_default = subtype(
        &mut tx,
        "Not default",
        RawVals::new().set_cell("default", Cell::Bool(false)),
    )
    .await;
    let elsewhere = subtype(
        &mut tx,
        "Other model",
        RawVals::new().set_cell("res_model", text("res.partner")),
    )
    .await;
    let existing = follower(&mut tx, TICKET, t, kept, &[not_default]).await;
    tx.flush().await.unwrap();
    let defaults: erp_addon_mail::DefaultSubtypes = tx
        .env()
        .records("mail.message.subtype")
        .unwrap()
        .call(&mut tx, "default_subtypes", (TICKET.to_owned(),))
        .await
        .unwrap();
    // fixtures: the model's own and the generic defaults, split by `internal`
    assert!(defaults.all().contains(&public_default));
    assert!(defaults.all().contains(&ticket_internal));
    assert!(!defaults.all().contains(&not_default));
    assert!(!defaults.all().contains(&elsewhere));
    assert!(defaults.external().contains(&public_default));
    assert!(!defaults.external().contains(&ticket_internal));
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    record
        .call::<_, ()>(
            &mut tx,
            "follow",
            (
                vec![internal, portal, kept, lapsed, nobody],
                None::<Vec<i64>>,
            ),
        )
        .await
        .unwrap();
    let row = row_of(&mut tx, t, internal)
        .await
        .expect("internal follows");
    let mut all = defaults.all().to_vec();
    all.sort_unstable();
    assert_eq!(subtypes_of(&mut tx, row).await, all);
    let row = row_of(&mut tx, t, portal).await.expect("portal follows");
    let mut external = defaults.external().to_vec();
    external.sort_unstable();
    assert_eq!(subtypes_of(&mut tx, row).await, external);
    for share_only in [lapsed, nobody] {
        let row = row_of(&mut tx, t, share_only).await.expect("follows");
        assert_eq!(subtypes_of(&mut tx, row).await, external);
    }
    // a partner that followed already keeps its row and its subtypes
    assert_eq!(row_of(&mut tx, t, kept).await, Some(existing));
    assert_eq!(subtypes_of(&mut tx, existing).await, vec![not_default]);
    record
        .call::<_, ()>(&mut tx, "follow", (vec![internal], Some(Vec::<i64>::new())))
        .await
        .unwrap();
    assert_eq!(ids(&mut tx, "mail.followers", &of_ticket(t)).await.len(), 5);
    // no partner, no statement
    assert_query_count!(tx, <= 0, {
        record
            .call::<_, ()>(&mut tx, "follow", (Vec::<i64>::new(), None::<Vec<i64>>))
            .await
            .unwrap()
    });
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// archived)
#[tokio::test]
async fn follow_gives_the_superuser_partner_the_default_subtypes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let su_partner = partner(&mut tx, "Superuser partner").await;
    let su = erp_sdk::SUPERUSER_ID.get();
    tx.flush().await.unwrap();
    let present = tx
        .query("SELECT id FROM res_users WHERE id = $1", &[&su])
        .await
        .unwrap();
    if !present.is_empty() {
        tx.execute(
            "UPDATE res_users SET active = FALSE, share = FALSE, partner_id = $2 WHERE id = $1",
            &[&su, &su_partner],
        )
        .await
        .unwrap();
    } else {
        add_user(&mut tx, su, "__system__", su_partner, false, false).await;
    }
    tx.invalidate_all().await.unwrap();
    let ticket_internal = subtype(
        &mut tx,
        "Ticket internal",
        RawVals::new()
            .set_cell("res_model", text(TICKET))
            .set_cell("internal", Cell::Bool(true)),
    )
    .await;
    ticket_of(&tx.env(), t)
        .call::<_, ()>(&mut tx, "follow", (vec![su_partner], None::<Vec<i64>>))
        .await
        .unwrap();
    let row = row_of(&mut tx, t, su_partner).await.expect("follows");
    assert!(subtypes_of(&mut tx, row).await.contains(&ticket_internal));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn follow_with_subtypes_replaces_existing_followers() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let (existing, fresh) = (
        partner(&mut tx, "Existing").await,
        partner(&mut tx, "Fresh").await,
    );
    let s1 = subtype(&mut tx, "S1", RawVals::new()).await;
    let s2 = subtype(&mut tx, "S2", RawVals::new()).await;
    let own_model = subtype(
        &mut tx,
        "S3",
        RawVals::new().set_cell("res_model", text(TICKET)),
    )
    .await;
    let row = follower(&mut tx, TICKET, t, existing, &[s1]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    record
        .call::<_, ()>(
            &mut tx,
            "follow",
            (vec![existing, fresh], Some(vec![s2, own_model])),
        )
        .await
        .unwrap();
    let mut expected = vec![s2, own_model];
    expected.sort_unstable();
    assert_eq!(row_of(&mut tx, t, existing).await, Some(row));
    assert_eq!(subtypes_of(&mut tx, row).await, expected);
    let created = row_of(&mut tx, t, fresh).await.expect("fresh follows");
    assert_eq!(subtypes_of(&mut tx, created).await, expected);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn follow_drops_inactive_partners() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let (active, archived) = (
        partner(&mut tx, "Active").await,
        partner(&mut tx, "Archived").await,
    );
    tx.env()
        .records("res.partner")
        .unwrap()
        .browse([archived])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    record
        .call::<_, ()>(
            &mut tx,
            "follow",
            (vec![active, archived], None::<Vec<i64>>),
        )
        .await
        .unwrap();
    assert!(row_of(&mut tx, t, active).await.is_some());
    assert_eq!(row_of(&mut tx, t, archived).await, None);
    // only inactive partners: nothing is followed and nothing fails
    record
        .call::<_, ()>(&mut tx, "follow", (vec![archived], None::<Vec<i64>>))
        .await
        .unwrap();
    assert_eq!(ids(&mut tx, "mail.followers", &of_ticket(t)).await.len(), 1);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// following one's own partner needs only read access, refuses a user who may not read, and asks
/// the policy whether the user is public. The two halves run under separate test policies, as no
/// test policy refuses writes and also evaluates groups)
#[tokio::test]
async fn follow_others_needs_write_access_and_self_needs_read_access() {
    // write refused: another partner is refused, one's own partner passes the read check and
    // then asks the policy for the public group, which this policy cannot answer
    let h = write_denied_harness().await;
    let mut tx = h.begin().await.unwrap();
    let own = user_with_partner(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    let other = partner(&mut tx, "Other").await;
    tx.flush().await.unwrap();
    let record = ticket_of(&tx.env().with_user(42), t);
    for partners in [vec![other], vec![own, other]] {
        let error = record
            .call::<_, ()>(&mut tx, "follow", (partners, None::<Vec<i64>>))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(error.message(), WRITE_DENIED);
    }
    let error = record
        .call::<_, ()>(&mut tx, "follow", (vec![own], None::<Vec<i64>>))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(
        error.message().contains("does not evaluate group specs"),
        "{error}"
    );
    assert!(
        ids(&mut tx, "mail.followers", &of_ticket(t))
            .await
            .is_empty()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();

    // read refused: a user who may not read the record cannot follow it either
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(TICKET, Operation::Read, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let own = user_with_partner(&mut tx).await;
    let t = ticket(&mut tx, "T").await;
    tx.flush().await.unwrap();
    let record = ticket_of(&tx.env().with_user(42), t);
    let error = record
        .call::<_, ()>(&mut tx, "follow", (vec![own], None::<Vec<i64>>))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert!(
        ids(&mut tx, "mail.followers", &of_ticket(t))
            .await
            .is_empty()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();

    // a policy that knows groups: an internal user follows themselves, a public user may not
    let policy = FieldPolicy::new()
        .enforce_groups()
        .member(42, ["base.group_user"])
        .member(43, ["base.group_public"]);
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let own = user_with_partner(&mut tx).await;
    let public_partner = partner(&mut tx, "Public").await;
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active, partner_id) VALUES (43, 'Pub', 'pub', true, $1)",
        &[&public_partner],
    )
    .await
    .unwrap();
    let t = ticket(&mut tx, "T").await;
    tx.flush().await.unwrap();
    ticket_of(&tx.env().with_user(42), t)
        .call::<_, ()>(&mut tx, "follow", (vec![own], None::<Vec<i64>>))
        .await
        .unwrap();
    assert!(row_of(&mut tx, t, own).await.is_some());
    let error = ticket_of(&tx.env().with_user(43), t)
        .call::<_, ()>(&mut tx, "follow", (vec![public_partner], None::<Vec<i64>>))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(error.message(), "Public users cannot follow records.");
    assert_eq!(row_of(&mut tx, t, public_partner).await, None);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn insert_followers_update_adds_subtypes_and_never_removes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let p = partner(&mut tx, "P").await;
    let s1 = subtype(&mut tx, "S1", RawVals::new()).await;
    let s2 = subtype(&mut tx, "S2", RawVals::new()).await;
    let row = follower(&mut tx, TICKET, t, p, &[s1]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    record
        .call::<_, ()>(
            &mut tx,
            "_insert_followers",
            (vec![p], Some(vec![s1, s2]), "update".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(subtypes_of(&mut tx, row).await, vec![s1, s2]);
    // a smaller set under `update` removes nothing
    record
        .call::<_, ()>(
            &mut tx,
            "_insert_followers",
            (vec![p], Some(vec![s2]), "update".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(subtypes_of(&mut tx, row).await, vec![s1, s2]);
    // `skip` touches nothing, `replace` makes the set exact
    record
        .call::<_, ()>(
            &mut tx,
            "_insert_followers",
            (vec![p], Some(vec![s1]), "skip".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(subtypes_of(&mut tx, row).await, vec![s1, s2]);
    record
        .call::<_, ()>(
            &mut tx,
            "_insert_followers",
            (vec![p], Some(vec![s1]), "replace".to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(subtypes_of(&mut tx, row).await, vec![s1]);
    let error = record
        .call::<_, ()>(
            &mut tx,
            "_insert_followers",
            (vec![p], Some(vec![s1]), "force".to_owned()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The existing-follower policy is skip, replace or update, not 'force'."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn writing_message_partner_ids_follows_and_unfollows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let (p1, p2, p3) = (
        partner(&mut tx, "P1").await,
        partner(&mut tx, "P2").await,
        partner(&mut tx, "P3").await,
    );
    let s = subtype(&mut tx, "S", RawVals::new()).await;
    follower(&mut tx, TICKET, t, p1, &[s]).await;
    let kept = follower(&mut tx, TICKET, t, p2, &[s]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    let wanted = [p2, p3].map(|id| RecId::from_real(id).unwrap());
    record
        .write(
            &mut tx,
            RawVals::new().set_commands("message_partner_ids", [Command::set(wanted)]),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(row_of(&mut tx, t, p1).await, None);
    assert_eq!(row_of(&mut tx, t, p2).await, Some(kept));
    let added = row_of(&mut tx, t, p3).await.expect("p3 follows");
    let defaults: erp_addon_mail::DefaultSubtypes = tx
        .env()
        .records("mail.message.subtype")
        .unwrap()
        .call(&mut tx, "default_subtypes", (TICKET.to_owned(),))
        .await
        .unwrap();
    let mut all = defaults.all().to_vec();
    all.sort_unstable();
    assert_eq!(subtypes_of(&mut tx, added).await, all);
    assert_eq!(subtypes_of(&mut tx, kept).await, vec![s]);
    assert_eq!(
        id_list(record.cell(&mut tx, "message_partner_ids").await.unwrap()),
        vec![p2, p3]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn writing_too_many_message_partner_ids_names_the_field() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let vals: Vec<RawVals> = (0..1001)
        .map(|i| RawVals::new().set_cell("name", text(&format!("Bulk {i}"))))
        .collect();
    let created = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(&mut tx, vals)
        .await
        .unwrap();
    let wanted: Vec<RecId> = created
        .ids()
        .into_iter()
        .map(|id| RecId::from_real(id).unwrap())
        .collect();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let error = ticket_of(&tx.env(), t)
        .write(
            &mut tx,
            RawVals::new().set_commands("message_partner_ids", [Command::set(wanted)]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(
        error
            .message()
            .starts_with("message_partner_ids adds 1001 partners")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn writing_message_partner_ids_that_removes_too_many_names_the_field() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let vals: Vec<RawVals> = (0..1001)
        .map(|i| RawVals::new().set_cell("name", text(&format!("Bulk {i}"))))
        .collect();
    let created = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(&mut tx, vals)
        .await
        .unwrap();
    for id in created.ids() {
        follower(&mut tx, TICKET, t, id, &[]).await;
    }
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let error = ticket_of(&tx.env(), t)
        .write(
            &mut tx,
            RawVals::new().set_commands("message_partner_ids", [Command::clear()]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(
        error
            .message()
            .starts_with("message_partner_ids removes 1001 partners")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Only internal users write `message_partner_ids`; the refusal leaves the followers alone.
#[tokio::test]
async fn a_portal_user_cannot_write_message_partner_ids() {
    let policy = FieldPolicy::new()
        .enforce_groups()
        .member(42, ["base.group_user"])
        .member(43, ["base.group_portal"]);
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let (p1, p2) = (partner(&mut tx, "P1").await, partner(&mut tx, "P2").await);
    follower(&mut tx, TICKET, t, p1, &[]).await;
    tx.flush().await.unwrap();
    let portal = ticket_of(&tx.env().with_user(43), t);
    let error = tx
        .savepoint(async |tx| {
            portal
                .write(
                    tx,
                    RawVals::new().set_commands(
                        "message_partner_ids",
                        [Command::set([RecId::from_real(p2).unwrap()])],
                    ),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    assert_eq!(error.message(), "Cannot write on message partners");
    assert_eq!(row_of(&mut tx, t, p2).await, None);
    assert!(row_of(&mut tx, t, p1).await.is_some());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A subtype id that does not exist, or belongs to another model, is refused before anything is
/// written, by `set_subtypes` and by `follow` with explicit subtypes.
#[tokio::test]
async fn subtype_ids_are_validated_before_writing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let (p, fresh) = (partner(&mut tx, "P").await, partner(&mut tx, "Fresh").await);
    let good = subtype(&mut tx, "Good", RawVals::new()).await;
    let foreign = subtype(
        &mut tx,
        "Foreign",
        RawVals::new().set_cell("res_model", text("res.partner")),
    )
    .await;
    let row = follower(&mut tx, TICKET, t, p, &[good]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    let missing = foreign + 1_000_000;
    let message = format!(
        "Unknown subtype ids for test_mail.ticket: {foreign}, {missing}. A subtype must exist and \
         have no model or the model of the record."
    );
    let error = record
        .call::<_, ()>(&mut tx, "set_subtypes", (p, vec![good, foreign, missing]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message);
    let error = record
        .call::<_, ()>(
            &mut tx,
            "follow",
            (vec![p, fresh], Some(vec![good, foreign, missing])),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), message);
    // nothing was written: the follower keeps its subtype and the fresh partner does not follow
    assert_eq!(subtypes_of(&mut tx, row).await, vec![good]);
    assert_eq!(row_of(&mut tx, t, fresh).await, None);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Statement ceiling of `follow` for one new partner with default subtypes plus one explicit
/// replace (flush included), a RATCHET, DEFERRED_PERFORMANCE: it only guards against
/// growth until the milestone-end pass lowers it. One-sided on purpose. Superuser only (a user's
/// access checks add statements that are not measured).
#[tokio::test]
async fn follow_statement_ceiling() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let (p, q) = (partner(&mut tx, "P").await, partner(&mut tx, "Q").await);
    let s1 = subtype(&mut tx, "S1", RawVals::new()).await;
    let s2 = subtype(&mut tx, "S2", RawVals::new()).await;
    follower(&mut tx, TICKET, t, q, &[s1]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let record = ticket_of(&tx.env(), t);
    let before = tx.query_count();
    assert_query_count!(tx, >= 1, <= FOLLOW_DEFAULTS_CEILING, {
        record
            .call::<_, ()>(&mut tx, "follow", (vec![p], None::<Vec<i64>>))
            .await
            .unwrap();
        tx.flush().await.unwrap()
    });
    let defaults = tx.query_count() - before;
    tx.invalidate_all().await.unwrap();
    let before = tx.query_count();
    assert_query_count!(tx, >= 1, <= FOLLOW_EXPLICIT_CEILING, {
        record
            .call::<_, ()>(&mut tx, "follow", (vec![q], Some(vec![s2])))
            .await
            .unwrap();
        tx.flush().await.unwrap()
    });
    let explicit = tx.query_count() - before;
    // read with `--nocapture`: the counts the ceilings were set from
    eprintln!("follow statements: defaults={defaults} explicit_replace={explicit}");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Ceiling of `follow` with default subtypes, a new partner (see `follow_statement_ceiling`).
const FOLLOW_DEFAULTS_CEILING: u64 = 8;
/// Ceiling of `follow` with explicit subtypes replacing an existing follower's.
const FOLLOW_EXPLICIT_CEILING: u64 = 9;

#[tokio::test]
async fn message_follower_ids_is_internal_only() {
    let policy = FieldPolicy::new()
        .enforce_groups()
        .member(42, ["base.group_user"])
        .member(43, ["base.group_portal"]);
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let p = partner(&mut tx, "P").await;
    let row = follower(&mut tx, TICKET, t, p, &[]).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let internal = ticket_of(&tx.env().with_user(42), t);
    assert_eq!(
        id_list(
            internal
                .cell(&mut tx, "message_follower_ids")
                .await
                .unwrap()
        ),
        vec![row]
    );
    tx.invalidate_all().await.unwrap();
    let portal = ticket_of(&tx.env().with_user(43), t);
    let error = portal
        .cell(&mut tx, "message_follower_ids")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    // the computed partner list reads the rows privileged: it still works for the same caller
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        id_list(portal.cell(&mut tx, "message_partner_ids").await.unwrap()),
        vec![p]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// caller whose policy gives no read access to `mail.message` sees no messages through it until
/// the ACCESS slice adds the thread-access rule)
#[tokio::test]
async fn message_ids_follows_the_callers_message_access() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("mail.message", Operation::Read, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_mail::ADDON,
        &support::ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let t = ticket(&mut tx, "T").await;
    let m = message(&mut tx, TICKET, t, "hello").await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    // the superuser still sees the message
    assert_eq!(
        id_list(
            ticket_of(&tx.env(), t)
                .cell(&mut tx, "message_ids")
                .await
                .unwrap()
        ),
        vec![m]
    );
    tx.invalidate_all().await.unwrap();
    let user = ticket_of(&tx.env().with_user(42), t);
    assert!(id_list(user.cell(&mut tx, "message_ids").await.unwrap()).is_empty());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
