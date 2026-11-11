//! `mail.activity`, `mail.activity.type` and `mail.activity.mixin`: defaults and
//! states, assignment and subscription, done (feedback message, archive), reschedule, the mixin's
//! automation helpers, the panel data, the systray counts and the access checks. Design note:
//! .
//!
//! Not asserted here, with the reason: (bus:), (attachments:
//! ; the typed refusal is tested untagged), and 135 (chaining:),
//! condition), (searching a computed state needs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod chat;
mod support;

use erp_addon_mail::{MailActivityMixin, MailThread, ScheduleRequest, activity_systray};
use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation};
use erp_testkit::{DomainPolicy, DomainPolicyBuilder, Harness, Otherwise};
use support::*;

const TASK: &str = "test_mail.task";

/// A thread model with activities.
#[model(
    name = "test_mail.task",
    description = "Mail Test Task",
    inherit = [MailThread, MailActivityMixin],
    addon = "test_mail"
)]
pub struct Task {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

async fn harness_with(deny_task_writes: bool) -> Harness {
    harness_policy(|policy| {
        if deny_task_writes {
            policy.rule(TASK, Operation::Write, Domain::FALSE)
        } else {
            policy
        }
    })
    .await
}

/// Tasks are writable only while their name is `writable` (so a test can take the access away
/// by renaming the record).
async fn harness_writable_by_name() -> Harness {
    harness_policy(|policy| {
        policy.rule(
            TASK,
            Operation::Write,
            Domain::condition("name", Operator::Eq, "writable").unwrap(),
        )
    })
    .await
}

async fn harness_policy(shape: impl FnOnce(DomainPolicyBuilder) -> DomainPolicyBuilder) -> Harness {
    let policy = shape(DomainPolicy::builder(Otherwise::Allow));
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

struct Seed {
    todo: i64,
    email: i64,
    activities_subtype: i64,
}

async fn xmlid(tx: &mut Transaction, model: &str, name: &str, id: i64) {
    create(
        tx,
        "ir.model.data",
        RawVals::new()
            .set_cell("module", text("mail"))
            .set_cell("name", text(name))
            .set_cell("model", text(model))
            .set_cell("res_id", Cell::Id(id)),
    )
    .await;
}

async fn activity_type(tx: &mut Transaction, name: &str, sequence: i32, delay: i32) -> i64 {
    create(
        tx,
        "mail.activity.type",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("summary", text(name))
            .set_cell("sequence", Cell::Int(sequence))
            .set_cell("delay_count", Cell::Int(delay)),
    )
    .await
}

/// Subtypes, activity types and external ids the harness does not load from data files, and a
/// partner for the superuser (a message needs an author).
async fn seed(tx: &mut Transaction) -> Seed {
    let note = subtype(
        tx,
        "Notes",
        RawVals::new()
            .set_cell("default", Cell::Bool(false))
            .set_cell("internal", Cell::Bool(true)),
    )
    .await;
    let comment = subtype(tx, "Messages", RawVals::new()).await;
    let activities_subtype = subtype(
        tx,
        "Activities",
        RawVals::new()
            .set_cell("default", Cell::Bool(false))
            .set_cell("internal", Cell::Bool(true)),
    )
    .await;
    for (name, id) in [
        ("mt_note", note),
        ("mt_comment", comment),
        ("mt_activities", activities_subtype),
    ] {
        xmlid(tx, "mail.message.subtype", name, id).await;
    }
    let todo = activity_type(tx, "To-Do", 2, 5).await;
    let email = activity_type(tx, "Email", 3, 0).await;
    let call = activity_type(tx, "Call", 6, 2).await;
    for (name, id) in [
        ("mail_activity_data_todo", todo),
        ("mail_activity_data_email", email),
        ("mail_activity_data_call", call),
    ] {
        xmlid(tx, "mail.activity.type", name, id).await;
    }
    let root = partner(tx, "root").await;
    set_user_partner(tx, 1, Some(root)).await;
    Seed {
        todo,
        email,
        activities_subtype,
    }
}

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

/// A user row with its own partner and an optional time zone; returns the partner.
async fn user(tx: &mut Transaction, id: i64, name: &str, tz: Option<&str>) -> i64 {
    let partner = partner(tx, name).await;
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active, share, partner_id, tz) \
         VALUES ($1, $2, $2, true, false, $3, $4)",
        &[&id, &name, &partner, &tz],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    partner
}

async fn task(tx: &mut Transaction, name: &str) -> i64 {
    create(tx, TASK, RawVals::new().set_cell("name", text(name))).await
}

fn today() -> Date {
    Date::today()
}

fn days(date: Date, n: i32) -> Date {
    Date::from_days(date.days() + n)
}

/// An activity on `task_id` with the given extra values; the caller's environment.
async fn activity(tx: &mut Transaction, env: &Env, task_id: i64, extra: RawVals) -> i64 {
    let mut vals = extra
        .set_cell("res_model", text(TASK))
        .set_cell("res_id", Cell::Id(task_id));
    if !vals.contains("date_deadline") {
        vals = vals.set_cell("date_deadline", Cell::Date(today()));
    }
    env.records("mail.activity")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

fn rec(env: &Env, model: &str, id: i64) -> Records {
    env.records(model).unwrap().browse([id]).unwrap()
}

async fn cell(tx: &mut Transaction, model: &str, id: i64, field: &str) -> Cell {
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    rec(&tx.env(), model, id).cell(tx, field).await.unwrap()
}

async fn state(tx: &mut Transaction, id: i64) -> String {
    match cell(tx, "mail.activity", id, "state").await {
        Cell::Str(state) => state.to_string(),
        other => panic!("state is {other:?}"),
    }
}

fn assert_validation(error: &Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(error.message(), message);
}

fn deadline(date: Date) -> RawVals {
    RawVals::new().set_cell("date_deadline", Cell::Date(date))
}

#[tokio::test]
async fn an_activity_takes_the_first_type_and_its_deadline() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let vals = RawVals::new()
        .set_cell("res_model", text(TASK))
        .set_cell("res_id", Cell::Id(t));
    let id = env
        .records("mail.activity")
        .unwrap()
        .create(&mut tx, vec![vals])
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "activity_type_id").await,
        Cell::Id(s.todo)
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "date_deadline").await,
        Cell::Date(days(today(), 5))
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "summary").await,
        text("To-Do")
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "user_id").await,
        Cell::Id(1)
    );
    // a type without a delay is due today
    let vals = RawVals::new()
        .set_cell("res_model", text(TASK))
        .set_cell("res_id", Cell::Id(t))
        .set_cell("activity_type_id", Cell::Id(s.email));
    let id = env
        .records("mail.activity")
        .unwrap()
        .create(&mut tx, vec![vals])
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "date_deadline").await,
        Cell::Date(today())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_model_without_a_record_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = tx.env();
    for res_id in [None, Some(0)] {
        let mut vals = RawVals::new()
            .set_cell("res_model", text(TASK))
            .set_cell("date_deadline", Cell::Date(today()));
        if let Some(id) = res_id {
            vals = vals.set_cell("res_id", Cell::Id(id));
        }
        let error = env
            .records("mail.activity")
            .unwrap()
            .create(&mut tx, vec![vals])
            .await
            .unwrap_err();
        assert_validation(
            &error,
            "Activities have to be linked to records with a not null res_id.",
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_standalone_activity_needs_an_assignee() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = tx.env();
    let vals = RawVals::new()
        .set_cell("date_deadline", Cell::Date(today()))
        .set_cell("user_id", Cell::Null);
    let error = env
        .records("mail.activity")
        .unwrap()
        .create(&mut tx, vec![vals])
        .await
        .unwrap_err();
    assert_validation(
        &error,
        "Activities must be assigned if not attached to a document.",
    );
    // without a user the creator is the assignee
    let vals = RawVals::new().set_cell("date_deadline", Cell::Date(today()));
    let id = env
        .records("mail.activity")
        .unwrap()
        .create(&mut tx, vec![vals])
        .await
        .unwrap()
        .ids()[0];
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "user_id").await,
        Cell::Id(1)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_state_follows_the_deadline_and_the_archive_flag() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let late = activity(&mut tx, &env, t, deadline(days(today(), -1))).await;
    let now = activity(&mut tx, &env, t, deadline(today())).await;
    let later = activity(&mut tx, &env, t, deadline(days(today(), 1))).await;
    assert_eq!(state(&mut tx, late).await, "overdue");
    assert_eq!(state(&mut tx, now).await, "today");
    assert_eq!(state(&mut tx, later).await, "planned");
    rec(&env, "mail.activity", later)
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    assert_eq!(state(&mut tx, later).await, "done");
    assert_eq!(
        cell(&mut tx, "mail.activity", later, "date_done").await,
        Cell::Date(today())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The state is read in the assignee's time zone: with one deadline and zones a day apart, the
/// same activity is today for one assignee and overdue for the other. The expected dates come
/// from the zone database (jiff), not from the code under test.
#[tokio::test]
async fn the_state_uses_the_assignees_time_zone() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "west", Some("Pacific/Pago_Pago")).await;
    user(&mut tx, 42, "east", Some("Pacific/Kiritimati")).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let west_today = Date::context_today("Pacific/Pago_Pago").unwrap();
    let east_today = Date::context_today("Pacific/Kiritimati").unwrap();
    assert!(east_today > west_today, "the zones are 25 hours apart");
    let a = activity(
        &mut tx,
        &env,
        t,
        deadline(west_today).set_cell("user_id", Cell::Id(41)),
    )
    .await;
    let b = activity(
        &mut tx,
        &env,
        t,
        deadline(west_today).set_cell("user_id", Cell::Id(42)),
    )
    .await;
    assert_eq!(state(&mut tx, a).await, "today");
    assert_eq!(state(&mut tx, b).await, "overdue");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn assigning_an_activity_subscribes_the_assignee() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "u1", None).await;
    let p2 = user(&mut tx, 42, "u2", None).await;
    let t = task(&mut tx, "T").await;
    let as_u1 = tx.env().with_user(41);
    activity(
        &mut tx,
        &as_u1,
        t,
        RawVals::new().set_cell("user_id", Cell::Id(42)),
    )
    .await;
    let followers = ids(
        &mut tx,
        "mail.followers",
        &(Domain::condition("res_model", Operator::Eq, TASK).unwrap()
            & Domain::condition("res_id", Operator::Eq, t).unwrap()
            & Domain::condition("partner_id", Operator::Eq, p2).unwrap()),
    )
    .await;
    assert_eq!(followers.len(), 1);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Reassigning subscribes the new assignee, as creating does.
#[tokio::test]
async fn reassigning_an_activity_subscribes_the_new_assignee() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let p2 = user(&mut tx, 42, "u2", None).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let a = activity(&mut tx, &env, t, RawVals::new()).await;
    rec(&env, "mail.activity", a)
        .write(&mut tx, RawVals::new().set_cell("user_id", Cell::Id(42)))
        .await
        .unwrap();
    let followers = ids(
        &mut tx,
        "mail.followers",
        &Domain::condition("partner_id", Operator::Eq, p2).unwrap(),
    )
    .await;
    assert_eq!(followers.len(), 1);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn feedback_posts_a_message_and_archives_the_activity() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let a = activity(&mut tx, &env, t, RawVals::new()).await;
    let message: Option<i64> = rec(&env, "mail.activity", a)
        .call(
            &mut tx,
            "action_feedback",
            (Some("Completed task".to_owned()), Vec::<i64>::new()),
        )
        .await
        .unwrap();
    let message = message.expect("a message was posted");
    assert_eq!(
        cell(&mut tx, "mail.message", message, "subtype_id").await,
        Cell::Id(s.activities_subtype)
    );
    assert_eq!(
        cell(&mut tx, "mail.message", message, "res_id").await,
        Cell::Id(t)
    );
    assert_eq!(
        cell(&mut tx, "mail.message", message, "mail_activity_type_id").await,
        Cell::Id(s.todo)
    );
    match cell(&mut tx, "mail.message", message, "body").await {
        Cell::Str(body) => {
            assert!(body.contains("Completed task"), "{body}");
            assert!(body.starts_with("<p>To-Do done"), "{body}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "active").await,
        Cell::Bool(false)
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "date_done").await,
        Cell::Date(today())
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "feedback").await,
        text("Completed task")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The attachment half waits for `ir.attachment`: a done call with
/// attachment ids is refused with the typed error, and nothing is archived.
#[tokio::test]
async fn feedback_refuses_attachments_until_ir_attachment_exists() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let a = activity(&mut tx, &env, t, RawVals::new()).await;
    let error = rec(&env, "mail.activity", a)
        .call::<_, Option<i64>>(
            &mut tx,
            "action_feedback",
            (Some("Done".to_owned()), vec![7_i64]),
        )
        .await
        .unwrap_err();
    assert_validation(
        &error,
        "Attachments on an activity's feedback come with a later release (ir.attachment); mark the \
         activity done without attachments.",
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "active").await,
        Cell::Bool(true)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Marking several done at once posts one message each and archives them all (BUG-101's shape
/// without attachments: nothing of one activity reaches another's message).
#[tokio::test]
async fn several_activities_done_at_once_get_one_message_each() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let (t1, t2) = (task(&mut tx, "T1").await, task(&mut tx, "T2").await);
    let env = tx.env();
    let a = activity(
        &mut tx,
        &env,
        t1,
        RawVals::new().set_cell("summary", text("first")),
    )
    .await;
    let b = activity(
        &mut tx,
        &env,
        t2,
        RawVals::new().set_cell("summary", text("second")),
    )
    .await;
    let both = env
        .records("mail.activity")
        .unwrap()
        .browse([a, b])
        .unwrap();
    both.call::<_, Vec<i64>>(&mut tx, "_action_done", (None::<String>, Vec::<i64>::new()))
        .await
        .map(|messages| assert_eq!(messages.len(), 2))
        .unwrap();
    for (task_id, summary) in [(t1, "first"), (t2, "second")] {
        let found = ids(
            &mut tx,
            "mail.message",
            &Domain::condition("res_id", Operator::Eq, task_id).unwrap(),
        )
        .await;
        assert_eq!(found.len(), 1);
        match cell(&mut tx, "mail.message", found[0], "body").await {
            Cell::Str(body) => assert!(body.contains(summary), "{body}"),
            other => panic!("{other:?}"),
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An assignee without write access to the record can still complete their activity; someone
/// else cannot.
#[tokio::test]
async fn the_assignee_completes_without_access_to_the_record_others_cannot() {
    let h = harness_with(true).await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "assignee", None).await;
    user(&mut tx, 42, "other", None).await;
    let t = task(&mut tx, "T").await;
    let admin = tx.env();
    // the superuser creates it (the deny policy does not apply to superuser mode)
    let a = activity(
        &mut tx,
        &admin.sudo(true),
        t,
        RawVals::new().set_cell("user_id", Cell::Id(41)),
    )
    .await;
    let other = tx.env().with_user(42);
    let on_task = Domain::condition("res_id", Operator::Eq, t).unwrap();
    let before = ids(&mut tx, "mail.message", &on_task).await.len();
    // user 42 may read the task but not write it, and the activity is not theirs: refused, and
    // nothing was posted on the task before the refusal
    let error = rec(&other, "mail.activity", a)
        .call::<_, Option<i64>>(
            &mut tx,
            "action_feedback",
            (None::<String>, Vec::<i64>::new()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        error.message(),
        "You are not allowed to write 'Mail Test Task' (test_mail.task) records."
    );
    tx.flush().await.unwrap();
    assert_eq!(ids(&mut tx, "mail.message", &on_task).await.len(), before);
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "active").await,
        Cell::Bool(true)
    );
    let assignee = tx.env().with_user(41);
    rec(&assignee, "mail.activity", a)
        .call::<_, Option<i64>>(
            &mut tx,
            "action_feedback",
            (None::<String>, Vec::<i64>::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "active").await,
        Cell::Bool(false)
    );
    // creating one needs write access to the record (the post access of the model)
    let error = assignee
        .records("mail.activity")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("res_model", text(TASK))
                    .set_cell("res_id", Cell::Id(t)),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// User 42 has no part in the activity but may write the task.
#[tokio::test]
async fn a_user_with_write_access_to_the_record_edits_anothers_activity() {
    let h = harness_writable_by_name().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "assignee", None).await;
    user(&mut tx, 42, "colleague", None).await;
    let t = task(&mut tx, "writable").await;
    let admin = tx.env().sudo(true);
    let mut made = Vec::new();
    for _ in 0..3 {
        made.push(
            activity(
                &mut tx,
                &admin,
                t,
                RawVals::new().set_cell("user_id", Cell::Id(41)),
            )
            .await,
        );
    }
    let colleague = tx.env().with_user(42);
    rec(&colleague, "mail.activity", made[0])
        .call::<_, Option<i64>>(
            &mut tx,
            "action_feedback",
            (None::<String>, Vec::<i64>::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        cell(&mut tx, "mail.activity", made[0], "active").await,
        Cell::Bool(false)
    );
    rec(&colleague, "mail.activity", made[1])
        .call::<_, ()>(&mut tx, "action_reschedule_tomorrow", ())
        .await
        .unwrap();
    assert_eq!(
        cell(&mut tx, "mail.activity", made[1], "date_deadline").await,
        Cell::Date(days(today(), 1))
    );
    rec(&colleague, "mail.activity", made[2])
        .unlink(&mut tx)
        .await
        .unwrap();
    // the record is locked: the same user is refused
    let locked = activity(
        &mut tx,
        &admin,
        t,
        RawVals::new().set_cell("user_id", Cell::Id(41)),
    )
    .await;
    rec(&admin, TASK, t)
        .write(&mut tx, RawVals::new().set_cell("name", text("locked")))
        .await
        .unwrap();
    let error = rec(&colleague, "mail.activity", locked)
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The creator keeps editing and deleting their activity after losing write access to the
/// record (the creator row of the OR); an outsider has neither right.
#[tokio::test]
async fn the_creator_edits_after_losing_access_to_the_record() {
    let h = harness_writable_by_name().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "assignee", None).await;
    user(&mut tx, 42, "creator", None).await;
    user(&mut tx, 43, "outsider", None).await;
    let t = task(&mut tx, "writable").await;
    let creator = tx.env().with_user(42);
    let a = activity(
        &mut tx,
        &creator,
        t,
        RawVals::new().set_cell("user_id", Cell::Id(41)),
    )
    .await;
    let b = activity(
        &mut tx,
        &creator,
        t,
        RawVals::new().set_cell("user_id", Cell::Id(41)),
    )
    .await;
    rec(&tx.env().sudo(true), TASK, t)
        .write(&mut tx, RawVals::new().set_cell("name", text("locked")))
        .await
        .unwrap();
    rec(&creator, "mail.activity", a)
        .write(
            &mut tx,
            RawVals::new().set_cell("summary", text("still mine")),
        )
        .await
        .unwrap();
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "summary").await,
        text("still mine")
    );
    rec(&creator, "mail.activity", a)
        .call::<_, ()>(&mut tx, "action_reschedule_tomorrow", ())
        .await
        .unwrap();
    rec(&creator, "mail.activity", b)
        .unlink(&mut tx)
        .await
        .unwrap();
    let outsider = tx.env().with_user(43);
    let error = rec(&outsider, "mail.activity", a)
        .write(
            &mut tx,
            RawVals::new().set_cell("summary", text("mine now")),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A personal to-do (no record) can be created for yourself only, never for someone else; and
/// writing the owner an activity already has is not a move.
#[tokio::test]
async fn a_personal_todo_is_created_for_yourself_only() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "employee", None).await;
    user(&mut tx, 42, "other", None).await;
    let employee = tx.env().with_user(41);
    let todos = employee.records("mail.activity").unwrap();
    let error = todos
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("user_id", Cell::Id(42))
                    .set_cell("summary", text("for you")),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    let mine = todos
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("user_id", Cell::Id(41))
                    .set_cell("summary", text("for me")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(mine.ids().len(), 1);
    // no assignee given: the caller
    let default = todos
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("summary", text("default"))],
        )
        .await
        .unwrap();
    assert_eq!(
        cell(&mut tx, "mail.activity", default.ids()[0], "user_id").await,
        Cell::Id(41)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Writing the same record again is accepted; a real move is refused.
#[tokio::test]
async fn writing_the_same_owner_is_not_a_move() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let other = task(&mut tx, "U").await;
    let env = tx.env();
    let a = activity(&mut tx, &env, t, RawVals::new()).await;
    rec(&env, "mail.activity", a)
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("res_model", text(TASK))
                .set_cell("res_id", Cell::Id(t)),
        )
        .await
        .unwrap();
    let error = rec(&env, "mail.activity", a)
        .write(&mut tx, RawVals::new().set_cell("res_id", Cell::Id(other)))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A user refused the delete access on activity types cannot delete one, and the refusal comes
/// before any activity is moved to To-Do.
#[tokio::test]
async fn a_refused_type_delete_leaves_the_activities_on_their_type() {
    let h = harness_policy(|policy| policy.deny("mail.activity.type", Operation::Unlink)).await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "employee", None).await;
    let custom = activity_type(&mut tx, "Custom", 50, 0).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let a = activity(
        &mut tx,
        &env,
        t,
        RawVals::new().set_cell("activity_type_id", Cell::Id(custom)),
    )
    .await;
    let employee = tx.env().with_user(41);
    let error = rec(&employee, "mail.activity.type", custom)
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "activity_type_id").await,
        Cell::Id(custom)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The reschedule actions move only active activities, with checked date arithmetic.
#[tokio::test]
async fn reschedule_moves_the_active_deadlines() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let a = activity(&mut tx, &env, t, deadline(days(today(), -3))).await;
    let done = activity(&mut tx, &env, t, deadline(days(today(), -3))).await;
    rec(&env, "mail.activity", done)
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    let both = env
        .records("mail.activity")
        .unwrap()
        .browse([a, done])
        .unwrap();
    both.call::<_, ()>(&mut tx, "action_reschedule_today", ())
        .await
        .unwrap();
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "date_deadline").await,
        Cell::Date(today())
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", done, "date_deadline").await,
        Cell::Date(days(today(), -3))
    );
    both.call::<_, ()>(&mut tx, "action_reschedule_tomorrow", ())
        .await
        .unwrap();
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "date_deadline").await,
        Cell::Date(days(today(), 1))
    );
    both.call::<_, ()>(&mut tx, "action_reschedule_nextweek", ())
        .await
        .unwrap();
    let monday = match cell(&mut tx, "mail.activity", a, "date_deadline").await {
        Cell::Date(date) => date,
        other => panic!("{other:?}"),
    };
    // a Monday (1970-01-01 was a Thursday) in the week after today's, 1 to 7 days ahead of today
    assert_eq!((i64::from(monday.days()) + 3).rem_euclid(7), 0);
    let ahead = monday.days() - today().days();
    assert!((1..=7).contains(&ahead), "{ahead}");
    let custom = days(today(), 40);
    both.call::<_, ()>(&mut tx, "action_reschedule_customdate", (custom,))
        .await
        .unwrap();
    assert_eq!(
        cell(&mut tx, "mail.activity", a, "date_deadline").await,
        Cell::Date(custom)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_record_state_is_its_most_urgent_active_activity() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let planned = activity(&mut tx, &env, t, deadline(days(today(), 3))).await;
    let now = activity(&mut tx, &env, t, deadline(today())).await;
    let late = activity(&mut tx, &env, t, deadline(days(today(), -2))).await;
    async fn record_state(tx: &mut Transaction, t: i64) -> Cell {
        let record = rec(&tx.env(), TASK, t);
        tx.flush().await.unwrap();
        tx.invalidate_all().await.unwrap();
        record.cell(tx, "activity_state").await.unwrap()
    }
    assert_eq!(record_state(&mut tx, t).await, text("overdue"));
    rec(&env, "mail.activity", late)
        .call::<_, ()>(&mut tx, "action_done", ())
        .await
        .unwrap();
    assert_eq!(record_state(&mut tx, t).await, text("today"));
    rec(&env, "mail.activity", now)
        .call::<_, ()>(&mut tx, "action_done", ())
        .await
        .unwrap();
    assert_eq!(record_state(&mut tx, t).await, text("planned"));
    rec(&env, "mail.activity", planned)
        .call::<_, ()>(&mut tx, "action_done", ())
        .await
        .unwrap();
    assert_eq!(record_state(&mut tx, t).await, Cell::Null);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The mixin's derived columns follow the nearest active activity.
#[tokio::test]
async fn the_record_shows_its_next_activity() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    activity(
        &mut tx,
        &env,
        t,
        deadline(days(today(), 9)).set_cell("summary", text("later")),
    )
    .await;
    activity(
        &mut tx,
        &env,
        t,
        deadline(days(today(), 2)).set_cell("summary", text("sooner")),
    )
    .await;
    assert_eq!(
        cell(&mut tx, TASK, t, "activity_summary").await,
        text("sooner")
    );
    assert_eq!(
        cell(&mut tx, TASK, t, "activity_date_deadline").await,
        Cell::Date(days(today(), 2))
    );
    assert_eq!(
        cell(&mut tx, TASK, t, "activity_user_id").await,
        Cell::Id(1)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

fn todo_xmlid() -> String {
    "mail.mail_activity_data_todo".to_owned()
}

#[tokio::test]
async fn activity_schedule_creates_an_automated_activity() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let mut request = ScheduleRequest::new();
    request.summary = Some("Automated reminder".to_owned());
    let created = rec(&env, TASK, t)
        .call::<_, Option<Records>>(&mut tx, "activity_schedule", (request,))
        .await
        .unwrap()
        .expect("created");
    let id = created.ids()[0];
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "summary").await,
        text("Automated reminder")
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "automated").await,
        Cell::Bool(true)
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "activity_type_id").await,
        Cell::Id(s.todo)
    );
    // no deadline given: today, not the To-Do type's delay (5 days)
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "date_deadline").await,
        Cell::Date(today())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `activity_schedule` without a deadline is due today even for a type with a delay (Call: 2
/// days); the delay is the default of a plain create only.
#[tokio::test]
async fn activity_schedule_without_a_deadline_ignores_the_type_delay() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let mut request = ScheduleRequest::new();
    request.act_type_xmlid = Some("mail.mail_activity_data_call".to_owned());
    let created = rec(&env, TASK, t)
        .call::<_, Option<Records>>(&mut tx, "activity_schedule", (request,))
        .await
        .unwrap()
        .expect("created");
    let id = created.ids()[0];
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "date_deadline").await,
        Cell::Date(today())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_automation_skip_context_creates_nothing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let record =
        rec(&env, TASK, t).with_context([("mail_activity_automation_skip", Value::Bool(true))]);
    let mut request = ScheduleRequest::new();
    request.summary = Some("Test".to_owned());
    let created = record
        .call::<_, Option<Records>>(&mut tx, "activity_schedule", (request,))
        .await
        .unwrap();
    assert!(created.is_none());
    assert!(
        ids(&mut tx, "mail.activity", &Domain::TRUE)
            .await
            .is_empty()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_type_of_another_model_falls_back_to_the_default() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let other = create(
        &mut tx,
        "mail.activity.type",
        RawVals::new()
            .set_cell("name", text("Elsewhere"))
            .set_cell("res_model", text("res.partner"))
            .set_cell("sequence", Cell::Int(1)),
    )
    .await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let mut request = ScheduleRequest::new();
    request.activity_type_id = Some(other);
    let created = rec(&env, TASK, t)
        .call::<_, Option<Records>>(&mut tx, "activity_schedule", (request,))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cell(
            &mut tx,
            "mail.activity",
            created.ids()[0],
            "activity_type_id"
        )
        .await,
        Cell::Id(s.todo)
    );
    // creating directly with that type is refused
    let error = env
        .records("mail.activity")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("res_model", text(TASK))
                    .set_cell("res_id", Cell::Id(t))
                    .set_cell("activity_type_id", Cell::Id(other)),
            ],
        )
        .await
        .unwrap_err();
    assert_validation(
        &error,
        "Activity type 'Elsewhere' is restricted to res.partner, not to test_mail.task.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn activity_reschedule_changes_deadline_and_assignee() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "new", None).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let record = rec(&env, TASK, t);
    let created = record
        .call::<_, Option<Records>>(&mut tx, "activity_schedule", (ScheduleRequest::new(),))
        .await
        .unwrap()
        .unwrap();
    let id = created.ids()[0];
    let new_date = days(today(), 11);
    let moved = record
        .call::<_, Option<Records>>(
            &mut tx,
            "activity_reschedule",
            (
                vec![todo_xmlid()],
                None::<i64>,
                Some(new_date),
                Some(42_i64),
                true,
            ),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.ids(), vec![id]);
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "date_deadline").await,
        Cell::Date(new_date)
    );
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "user_id").await,
        Cell::Id(42)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn activity_feedback_completes_automated_activities() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let record = rec(&env, TASK, t);
    let mut request = ScheduleRequest::new();
    request.act_type_xmlid = Some("mail.mail_activity_data_call".to_owned());
    let created = record
        .call::<_, Option<Records>>(&mut tx, "activity_schedule", (request,))
        .await
        .unwrap()
        .unwrap();
    let id = created.ids()[0];
    let done: bool = record
        .call(
            &mut tx,
            "activity_feedback",
            (
                vec!["mail.mail_activity_data_call".to_owned()],
                None::<i64>,
                Some("Called customer".to_owned()),
                Vec::<i64>::new(),
                true,
            ),
        )
        .await
        .unwrap();
    assert!(done);
    assert_eq!(
        cell(&mut tx, "mail.activity", id, "active").await,
        Cell::Bool(false)
    );
    let messages = ids(
        &mut tx,
        "mail.message",
        &Domain::condition("res_id", Operator::Eq, t).unwrap(),
    )
    .await;
    assert_eq!(messages.len(), 1);
    match cell(&mut tx, "mail.message", messages[0], "body").await {
        Cell::Str(body) => assert!(body.contains("Called customer"), "{body}"),
        other => panic!("{other:?}"),
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn activity_unlink_deletes_automated_activities() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let record = rec(&env, TASK, t);
    let created = record
        .call::<_, Option<Records>>(&mut tx, "activity_schedule", (ScheduleRequest::new(),))
        .await
        .unwrap()
        .unwrap();
    let manual = activity(&mut tx, &env, t, RawVals::new()).await;
    let deleted: bool = record
        .call(
            &mut tx,
            "activity_unlink",
            (vec![todo_xmlid()], None::<i64>, true),
        )
        .await
        .unwrap();
    assert!(deleted);
    tx.flush().await.unwrap();
    let left = ids(&mut tx, "mail.activity", &Domain::TRUE).await;
    assert!(!left.contains(&created.ids()[0]));
    // a manual (not automated) activity of the same type is not touched
    assert!(left.contains(&manual));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deleting_a_type_moves_every_activity_to_todo() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let custom = activity_type(&mut tx, "Custom", 50, 0).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    let live = activity(
        &mut tx,
        &env,
        t,
        RawVals::new().set_cell("activity_type_id", Cell::Id(custom)),
    )
    .await;
    let done = activity(
        &mut tx,
        &env,
        t,
        RawVals::new().set_cell("activity_type_id", Cell::Id(custom)),
    )
    .await;
    rec(&env, "mail.activity", done)
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    rec(&env, "mail.activity.type", custom)
        .unlink(&mut tx)
        .await
        .unwrap();
    for id in [live, done] {
        assert_eq!(
            cell(&mut tx, "mail.activity", id, "activity_type_id").await,
            Cell::Id(s.todo)
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_todo_master_type_cannot_be_deleted_or_archived() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let s = seed(&mut tx).await;
    let env = tx.env();
    let error = rec(&env, "mail.activity.type", s.todo)
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "You cannot delete To-Do as it is required in various apps."
    );
    let error = rec(&env, "mail.activity.type", s.todo)
        .action_archive(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "The 'To-Do' activity type is used to create reminders from the top bar menu and the \
         command palette. Consequently, it cannot be archived or deleted."
    );
    let error = rec(&env, "mail.activity.type", s.todo)
        .write(&mut tx, RawVals::new().set_cell("res_model", text(TASK)))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "You cannot modify To-Do target model as they are required in various apps."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An activity stays on its record.
#[tokio::test]
async fn an_activity_cannot_move_to_another_record() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let (t1, t2) = (task(&mut tx, "T1").await, task(&mut tx, "T2").await);
    let env = tx.env();
    let a = activity(&mut tx, &env, t1, RawVals::new()).await;
    let error = rec(&env, "mail.activity", a)
        .write(&mut tx, RawVals::new().set_cell("res_id", Cell::Id(t2)))
        .await
        .unwrap_err();
    assert_validation(
        &error,
        "An activity stays on its record: res_model and res_id cannot be changed.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A model without the mixins, an unknown model and a missing record are refused by name.
#[tokio::test]
async fn the_owner_must_be_a_real_record_with_activities() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let tk = ticket(&mut tx, "plain").await;
    let env = tx.env();
    for (model, id, message) in [
        (
            "test_mail.ticket",
            tk,
            "test_mail.ticket has no activities: it must inherit mail.activity.mixin and \
             mail.thread.",
        ),
        ("no.such", 1, "Unknown model 'no.such' for an activity."),
        (
            TASK,
            987_654,
            "test_mail.task 987654 does not exist: an activity needs its record.",
        ),
    ] {
        let error = env
            .records("mail.activity")
            .unwrap()
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("res_model", text(model))
                        .set_cell("res_id", Cell::Id(id))
                        .set_cell("date_deadline", Cell::Date(today())),
                ],
            )
            .await
            .unwrap_err();
        assert_validation(&error, message);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The panel lists the record's active activities nearest first, with type, assignee, state and
/// write permission, and its statement count does not grow with the activities.
#[tokio::test]
async fn the_panel_lists_activities_in_constant_statements() {
    let h = harness().await;
    let mut counts = Vec::new();
    for n in [2_i32, 12] {
        let mut tx = h.begin().await.unwrap();
        let s = seed(&mut tx).await;
        let t = task(&mut tx, "T").await;
        let env = tx.env();
        for i in 0..n {
            activity(
                &mut tx,
                &env,
                t,
                deadline(days(today(), i - 1)).set_cell("summary", text(&format!("a{i}"))),
            )
            .await;
        }
        tx.flush().await.unwrap();
        tx.invalidate_all().await.unwrap();
        let before = tx.query_count();
        let panel = rec(&env, TASK, t)
            .call::<_, erp_addon_mail::ActivityPanel>(&mut tx, "activities_panel", ())
            .await
            .unwrap();
        counts.push(tx.query_count() - before);
        assert_eq!(panel.activities.len(), usize::try_from(n).unwrap());
        let first = &panel.activities[0];
        assert_eq!(first.summary.as_deref(), Some("a0"));
        assert_eq!(first.state, "overdue");
        assert_eq!(first.activity_type_id, Some(s.todo));
        assert_eq!(first.activity_type_name.as_deref(), Some("To-Do"));
        assert_eq!(first.user_id, Some(1));
        assert!(first.can_write);
        assert_eq!(first.display_name(), "a0");
        assert!(
            panel
                .activities
                .windows(2)
                .all(|w| w[0].date_deadline <= w[1].date_deadline)
        );
        tx.rollback().await.unwrap();
        drop(tx);
    }
    assert_eq!(counts[0], counts[1], "statements per panel: {counts:?}");
    h.cleanup().await.unwrap();
}

/// The systray counts the current user's active activities per model by state, in one grouped
/// statement (plus one read of the time zone), whatever their number.
#[tokio::test]
async fn the_systray_counts_by_model_and_state() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "other", None).await;
    let t = task(&mut tx, "T").await;
    let env = tx.env();
    activity(&mut tx, &env, t, deadline(days(today(), -1))).await;
    activity(&mut tx, &env, t, deadline(days(today(), -5))).await;
    activity(&mut tx, &env, t, deadline(today())).await;
    activity(&mut tx, &env, t, deadline(days(today(), 4))).await;
    // another user's and a done one do not count
    activity(
        &mut tx,
        &env,
        t,
        deadline(today()).set_cell("user_id", Cell::Id(42)),
    )
    .await;
    let done = activity(&mut tx, &env, t, deadline(today())).await;
    rec(&env, "mail.activity", done)
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    // a personal to-do
    env.records("mail.activity")
        .unwrap()
        .create(&mut tx, vec![deadline(days(today(), 2))])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let before = tx.query_count();
    let groups = activity_systray(&env, &mut tx).await.unwrap();
    let used = tx.query_count() - before;
    assert!(used <= 2, "systray statements: {used}");
    assert_eq!(groups.len(), 2);
    let task_group = groups
        .iter()
        .find(|g| g.res_model.as_deref() == Some(TASK))
        .unwrap();
    assert_eq!(
        (task_group.overdue, task_group.today, task_group.planned),
        (2, 1, 1)
    );
    assert_eq!(task_group.total(), 4);
    assert_eq!(task_group.todo(), 3);
    let personal = groups.iter().find(|g| g.res_model.is_none()).unwrap();
    assert_eq!(
        (personal.overdue, personal.today, personal.planned),
        (0, 0, 1)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Someone else completing an activity posts the message as themselves and says whom it was
/// assigned to.
#[tokio::test]
async fn completing_anothers_activity_posts_as_the_caller_naming_the_assignee() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "assignee", None).await;
    let finisher = user(&mut tx, 42, "finisher", None).await;
    let t = task(&mut tx, "T").await;
    let admin = tx.env().sudo(true);
    let a = activity(
        &mut tx,
        &admin,
        t,
        RawVals::new()
            .set_cell("user_id", Cell::Id(41))
            .set_cell("summary", text("Check the stock")),
    )
    .await;
    let as_42 = tx.env().with_user(42);
    let message = rec(&as_42, "mail.activity", a)
        .call::<_, Option<i64>>(
            &mut tx,
            "action_feedback",
            (Some("Counted".to_owned()), Vec::<i64>::new()),
        )
        .await
        .unwrap()
        .expect("message");
    assert_eq!(
        cell(&mut tx, "mail.message", message, "author_id").await,
        Cell::Id(finisher)
    );
    assert_eq!(
        cell(&mut tx, "mail.message", message, "body").await,
        text("<p>To-Do done (Check the stock) (originally assigned to assignee): Counted</p>")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `activity_state` is computed with the caller's access: a value computed for the superuser is
/// not reused for a user who cannot read the activities.
#[tokio::test]
async fn the_record_state_is_not_shared_between_callers() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("mail.activity", Operation::Read, Domain::FALSE)
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
    seed(&mut tx).await;
    user(&mut tx, 42, "blind", None).await;
    let t = task(&mut tx, "T").await;
    let admin = tx.env().sudo(true);
    activity(&mut tx, &admin, t, deadline(today())).await;
    tx.flush().await.unwrap();
    let as_admin = rec(&admin, TASK, t)
        .cell(&mut tx, "activity_state")
        .await
        .unwrap();
    assert_eq!(as_admin, text("today"));
    let as_blind = tx.env().with_user(42);
    // computed for the caller, not taken from the superuser's cached value: refused loudly
    let error = rec(&as_blind, TASK, t)
        .cell(&mut tx, "activity_state")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        error.message(),
        "You are not allowed to read 'Activity' (mail.activity) records."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deleting_anothers_activity_needs_write_not_delete_on_the_record() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(TASK, Operation::Unlink, Domain::FALSE)
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
    seed(&mut tx).await;
    user(&mut tx, 41, "assignee", None).await;
    user(&mut tx, 42, "colleague", None).await;
    let t = task(&mut tx, "T").await;
    let admin = tx.env().sudo(true);
    let a = activity(
        &mut tx,
        &admin,
        t,
        RawVals::new().set_cell("user_id", Cell::Id(41)),
    )
    .await;
    // the colleague cannot delete the task itself
    let colleague = tx.env().with_user(42);
    assert!(
        rec(&colleague, TASK, t)
            .unlink(&mut tx)
            .await
            .is_err_and(|error| error.kind() == ErrorKind::Access)
    );
    rec(&colleague, "mail.activity", a)
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert!(
        !ids(&mut tx, "mail.activity", &Domain::TRUE)
            .await
            .contains(&a)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
