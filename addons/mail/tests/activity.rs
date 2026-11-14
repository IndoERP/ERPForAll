//! `mail.activity`, `mail.activity.type` and `mail.activity.mixin`: defaults and
//! states, assignment and subscription, done (feedback message, archive), reschedule, the mixin's
//! automation helpers, the panel data, the activity menu (to 387 and 389; 388, the
//! company case, is tested in `crates/erp-test-models/tests/mail_activity_menu_company.rs`:
//! building a principal that belongs to several companies needs `erp-orm` types that the layer
//! check closes to addon crates) and the access checks.
//!
//! Not asserted here, with the reason: (bus:), (attachments;
//! the typed refusal is tested untagged), and 135 (chaining:),
//! condition), (searching a computed state needs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod chat;
mod support;

use erp_addon_mail::{
    ActivityGroup, ActivityGroups, MAX_OPEN_RECORDS, MailActivityMixin, MailThread, OpenState,
    ScheduleRequest, activity_group_action, activity_groups,
};
use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation, ValueMap};
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
        "Your access rights do not let you write 'Mail Test Task' (test_mail.task) records."
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

/// A thread model with activities whose records can be archived.
#[model(
    name = "test_mail.notebook",
    description = "Mail Test Notebook",
    inherit = [MailThread, MailActivityMixin],
    addon = "test_mail"
)]
pub struct Notebook {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
}

const NOTEBOOK: &str = "test_mail.notebook";
const LIMIT: &str = "mail.activity.systray.limit";

async fn notebook(tx: &mut Transaction, name: &str) -> i64 {
    create(tx, NOTEBOOK, RawVals::new().set_cell("name", text(name))).await
}

/// An activity of `model`'s record `res_id`, assigned to `user_id`, due on `date`; created
/// privileged, as the harness principal.
async fn assigned(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    res_id: i64,
    user_id: i64,
    date: Date,
) -> i64 {
    let vals = RawVals::new()
        .set_cell("res_model", text(model))
        .set_cell("res_id", Cell::Id(res_id))
        .set_cell("user_id", Cell::Id(user_id))
        .set_cell("date_deadline", Cell::Date(date));
    env.records("mail.activity")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

/// A personal to-do (no document) of `user_id` due on `date`.
async fn personal(tx: &mut Transaction, env: &Env, user_id: i64, date: Date) -> i64 {
    let vals = RawVals::new()
        .set_cell("user_id", Cell::Id(user_id))
        .set_cell("date_deadline", Cell::Date(date));
    env.records("mail.activity")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

async fn archive(tx: &mut Transaction, env: &Env, model: &str, id: i64) {
    rec(env, model, id)
        .write(tx, RawVals::new().set_cell("active", Cell::Bool(false)))
        .await
        .unwrap();
}

fn group<'a>(menu: &'a ActivityGroups, model: &str) -> &'a ActivityGroup {
    menu.groups()
        .iter()
        .find(|group| group.model() == model)
        .unwrap_or_else(|| panic!("no group for {model}"))
}

/// The counts of `(overdue, today, planned)` among the activities `ids`, by their computed state.
async fn tally(tx: &mut Transaction, ids: &[i64]) -> (u32, u32, u32) {
    let mut counts = (0, 0, 0);
    for id in ids {
        match state(tx, *id).await.as_str() {
            "overdue" => counts.0 += 1,
            "today" => counts.1 += 1,
            "planned" => counts.2 += 1,
            other => panic!("state {other}"),
        }
    }
    counts
}

fn counts_of(group: &ActivityGroup) -> (u32, u32, u32) {
    (
        group.overdue_count(),
        group.today_count(),
        group.planned_count(),
    )
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

async fn set_limit(tx: &mut Transaction, env: &Env, value: &str) {
    let existing = ids(
        tx,
        "ir.config_parameter",
        &Domain::condition("key", Operator::Eq, LIMIT).unwrap(),
    )
    .await;
    if let Some(id) = existing.first() {
        rec(env, "ir.config_parameter", *id)
            .write(tx, RawVals::new().set_cell("value", text(value)))
            .await
            .unwrap();
    } else {
        create(
            tx,
            "ir.config_parameter",
            RawVals::new()
                .set_cell("key", text(LIMIT))
                .set_cell("value", text(value)),
        )
        .await;
    }
}

#[tokio::test]
async fn the_menu_counts_the_callers_open_activities_per_model() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "other", None).await;
    user(&mut tx, 42, "me", None).await;
    let (t1, t2, t3) = (
        task(&mut tx, "T1").await,
        task(&mut tx, "T2").await,
        task(&mut tx, "T3").await,
    );
    let n1 = notebook(&mut tx, "N1").await;
    let admin = tx.env();
    let mine = vec![
        assigned(&mut tx, &admin, TASK, t1, 42, days(today(), -1)).await,
        assigned(&mut tx, &admin, TASK, t2, 42, today()).await,
        assigned(&mut tx, &admin, TASK, t3, 42, days(today(), 1)).await,
    ];
    let book = vec![assigned(&mut tx, &admin, NOTEBOOK, n1, 42, today()).await];
    // a personal to-do of mine: it counts in the Other group, which comes last
    let todo = personal(&mut tx, &admin, 42, today()).await;
    // another user's activity and a done one of mine on the same records
    assigned(&mut tx, &admin, TASK, t1, 41, days(today(), -3)).await;
    let done = assigned(&mut tx, &admin, TASK, t2, 42, days(today(), -9)).await;
    archive(&mut tx, &admin, "mail.activity", done).await;
    let me = tx.env().with_user(42);
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    assert_eq!(menu.groups().len(), 3);
    let tasks = group(&menu, TASK);
    let books = group(&menu, NOTEBOOK);
    assert_eq!(counts_of(tasks), tally(&mut tx, &mine).await);
    assert_eq!(counts_of(books), tally(&mut tx, &book).await);
    assert_eq!(tasks.addon(), Some("test_mail"));
    let registry = me.registry();
    assert_eq!(
        tasks.name(),
        registry.model(TASK).unwrap().description.as_str()
    );
    assert_eq!(
        books.name(),
        registry.model(NOTEBOOK).unwrap().description.as_str()
    );
    assert_eq!(tasks.activity_ids(), sorted(mine.clone()).as_slice());
    assert_eq!(books.activity_ids(), book.as_slice());
    let mut counter = 0;
    for g in menu.groups() {
        assert_eq!(g.total_count(), g.overdue_count() + g.today_count());
        counter += g.total_count();
    }
    assert_eq!(menu.counter(), counter);
    // the named groups come in name order, the Other group last
    let (last, named) = menu.groups().split_last().unwrap();
    assert_eq!(last.model(), "mail.activity");
    assert_eq!(last.activity_ids(), [todo]);
    let names: Vec<&str> = named.iter().map(|g| g.name()).collect();
    assert_eq!(names.len(), 2);
    assert_eq!(names, {
        let mut by_name = names.clone();
        by_name.sort_unstable();
        by_name
    });
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The RPC `mail.activity.erp_activity_groups` answers the same menu as a map.
#[tokio::test]
async fn the_rpc_answers_the_menu_as_a_map() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "me", None).await;
    let t = task(&mut tx, "T").await;
    let admin = tx.env();
    assigned(&mut tx, &admin, TASK, t, 42, days(today(), -1)).await;
    let me = tx.env().with_user(42);
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    let answer = me
        .records("mail.activity")
        .unwrap()
        .call::<_, Value>(&mut tx, "erp_activity_groups", ())
        .await
        .unwrap();
    assert_eq!(answer, menu.to_value());
    let Value::Map(map) = &answer else {
        panic!("not a map: {answer:?}");
    };
    let keys: Vec<&str> = map.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["activity_counter", "groups"]);
    let Some(Value::List(groups)) = map.get("groups") else {
        panic!("groups is not a list: {answer:?}");
    };
    assert_eq!(groups.len(), 1);
    for entry in groups.iter() {
        let Value::Map(entry) = entry else {
            panic!("a group is not a map: {entry:?}");
        };
        let mut keys: Vec<&str> = entry.iter().map(|(key, _)| key.as_str()).collect();
        keys.sort_unstable();
        let mut expected = [
            "model",
            "name",
            "addon",
            "domain",
            "overdue_count",
            "today_count",
            "planned_count",
            "total_count",
            "activity_ids",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// One transaction, three principals: the menu is the caller's every time (no value of one user
/// is served to another, and the first user's answer is the same when they come back).
#[tokio::test]
async fn the_menu_follows_the_user_within_one_transaction() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 41, "first", None).await;
    user(&mut tx, 42, "second", None).await;
    let (t1, t2) = (task(&mut tx, "T1").await, task(&mut tx, "T2").await);
    let admin = tx.env();
    let first = assigned(&mut tx, &admin, TASK, t1, 41, today()).await;
    let second = assigned(&mut tx, &admin, TASK, t2, 42, today()).await;
    let as_41 = tx.env().with_user(41);
    let as_42 = tx.env().with_user(42);
    let before = activity_groups(&as_41, &mut tx).await.unwrap();
    let other = activity_groups(&as_42, &mut tx).await.unwrap();
    let after = activity_groups(&as_41, &mut tx).await.unwrap();
    assert_eq!(group(&before, TASK).activity_ids(), [first]);
    assert_eq!(group(&other, TASK).activity_ids(), [second]);
    assert_eq!(before, after);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn only_the_most_urgent_activity_of_a_record_counts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "me", None).await;
    let t = task(&mut tx, "T").await;
    let admin = tx.env();
    let tomorrow = assigned(&mut tx, &admin, TASK, t, 42, days(today(), 1)).await;
    let yesterday = assigned(&mut tx, &admin, TASK, t, 42, days(today(), -1)).await;
    let now = assigned(&mut tx, &admin, TASK, t, 42, today()).await;
    // the same deadline as the most urgent one, created later: the lowest id wins
    let later = assigned(&mut tx, &admin, TASK, t, 42, days(today(), -1)).await;
    assert!(yesterday < later);
    let me = tx.env().with_user(42);
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    assert_eq!(menu.groups().len(), 1);
    let tasks = group(&menu, TASK);
    assert_eq!(tasks.activity_ids(), [yesterday]);
    assert_eq!(counts_of(tasks), tally(&mut tx, &[yesterday]).await);
    assert_eq!(counts_of(tasks), (1, 0, 0));
    assert!(![tomorrow, now, later].contains(&tasks.activity_ids()[0]));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn activities_without_a_readable_document_count_under_other_activities() {
    let h = harness_policy(|policy| {
        policy
            .rule(
                TASK,
                Operation::Read,
                Domain::condition("name", Operator::Ne, "hidden").unwrap(),
            )
            .deny(NOTEBOOK, Operation::Read)
    })
    .await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "me", None).await;
    let admin = tx.env();
    let hidden = task(&mut tx, "hidden").await;
    let gone = task(&mut tx, "gone").await;
    let unregistered = task(&mut tx, "unregistered").await;
    let book = notebook(&mut tx, "refused").await;
    let counted = vec![
        personal(&mut tx, &admin, 42, days(today(), 2)).await,
        personal(&mut tx, &admin, 42, today()).await,
        assigned(&mut tx, &admin, TASK, hidden, 42, today()).await,
        assigned(&mut tx, &admin, NOTEBOOK, book, 42, days(today(), -1)).await,
        assigned(&mut tx, &admin, TASK, gone, 42, today()).await,
        assigned(&mut tx, &admin, TASK, unregistered, 42, days(today(), -2)).await,
    ];
    // the document of one is deleted underneath it, the model of another is not registered
    tx.flush().await.unwrap();
    tx.execute("DELETE FROM test_mail_task WHERE id = $1", &[&gone])
        .await
        .unwrap();
    tx.execute(
        "UPDATE mail_activity SET res_model = 'gone.model' WHERE id = $1",
        &[&counted[5]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let me = tx.env().with_user(42);
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    assert_eq!(menu.groups().len(), 1, "{menu:?}");
    let other = group(&menu, "mail.activity");
    assert_eq!(other.name(), "Other Activities");
    assert_eq!(other.addon(), Some("mail"));
    assert_eq!(other.domain(), &Value::list([]));
    assert_eq!(other.activity_ids(), sorted(counted.clone()).as_slice());
    assert_eq!(counts_of(other), tally(&mut tx, &counted).await);
    assert_eq!(menu.counter(), other.total_count());
    // nothing names a document, its model, or the model's label
    let shown = format!("{menu:?} {:?}", menu.to_value());
    let registry = me.registry();
    for needle in [
        "hidden",
        "refused",
        "gone.model",
        TASK,
        NOTEBOOK,
        registry.model(TASK).unwrap().description.as_str(),
        registry.model(NOTEBOOK).unwrap().description.as_str(),
    ] {
        assert!(!shown.contains(needle), "{needle} appears in {shown}");
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_archived_document_keeps_its_activities_and_its_group_includes_archived_records() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "me", None).await;
    let t = task(&mut tx, "T").await;
    let n = notebook(&mut tx, "N").await;
    let admin = tx.env();
    let on_book = assigned(&mut tx, &admin, NOTEBOOK, n, 42, today()).await;
    let on_task = assigned(&mut tx, &admin, TASK, t, 42, today()).await;
    archive(&mut tx, &admin, NOTEBOOK, n).await;
    let me = tx.env().with_user(42);
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    let books = group(&menu, NOTEBOOK);
    assert_eq!(books.activity_ids(), [on_book]);
    let archived = Value::list([Value::list([
        Value::str("active"),
        Value::str("in"),
        Value::list([Value::Bool(true), Value::Bool(false)]),
    ])]);
    assert_eq!(books.domain(), &archived);
    let tasks = group(&menu, TASK);
    assert_eq!(tasks.activity_ids(), [on_task]);
    assert_eq!(tasks.domain(), &Value::list([]));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// refusals with their messages, including a stored value that is not a number; the value 1000
/// itself is not exercised)
#[tokio::test]
async fn the_scan_is_bounded_and_newest_activities_come_first() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "me", None).await;
    let admin = tx.env();
    let mut made = Vec::new();
    for name in ["A", "B", "C"] {
        let t = task(&mut tx, name).await;
        made.push(assigned(&mut tx, &admin, TASK, t, 42, today()).await);
    }
    let me = tx.env().with_user(42);
    // no parameter: every activity is scanned
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    assert_eq!(group(&menu, TASK).activity_ids(), sorted(made.clone()));
    set_limit(&mut tx, &admin, "2").await;
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    let newest = sorted(made[1..].to_vec());
    assert_eq!(group(&menu, TASK).activity_ids(), newest.as_slice());
    set_limit(&mut tx, &admin, "0").await;
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    assert_eq!(group(&menu, TASK).activity_ids(), sorted(made.clone()));
    for value in ["-1", "10001"] {
        set_limit(&mut tx, &admin, value).await;
        let error = activity_groups(&me, &mut tx).await.unwrap_err();
        assert_validation(
            &error,
            &format!(
                "The system parameter mail.activity.systray.limit must be between 1 and 10000 \
                 (or 0 for the default 1000), not {value}."
            ),
        );
    }
    // a stored value that is not a number: the parameter reader's own refusal, not a default
    set_limit(&mut tx, &admin, "many").await;
    let error = activity_groups(&me, &mut tx).await.unwrap_err();
    assert_validation(
        &error,
        "ir.config_parameter mail.activity.systray.limit has a stored value that is not of type int",
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
        "Your access rights do not let you read 'Activity' (mail.activity) records."
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

const STATES: [(&str, OpenState); 4] = [
    ("all", OpenState::All),
    ("overdue", OpenState::Overdue),
    ("today", OpenState::Today),
    ("planned", OpenState::Planned),
];

/// The RPC `mail.activity.erp_activity_group_action` as `env`.
async fn open_action(tx: &mut Transaction, env: &Env, model: &str, state: &str) -> Result<Value> {
    env.records("mail.activity")
        .unwrap()
        .call::<_, Value>(
            tx,
            "erp_activity_group_action",
            (model.to_owned(), state.to_owned()),
        )
        .await
}

fn member<'a>(action: &'a Value, key: &str) -> &'a Value {
    let Value::Map(map) = action else {
        panic!("not a map: {action:?}");
    };
    map.get(key)
        .unwrap_or_else(|| panic!("no {key} in {action:?}"))
}

/// The ids of the leading `["id", "in", ids]` term and the terms after it.
fn domain_parts(action: &Value) -> (Vec<i64>, Vec<Value>) {
    let Value::List(terms) = member(action, "domain") else {
        panic!("domain is not a list: {action:?}");
    };
    let (first, rest) = terms.split_first().expect("an id term");
    let Value::List(first) = first else {
        panic!("not a term: {first:?}");
    };
    assert_eq!(first[0], Value::str("id"));
    assert_eq!(first[1], Value::str("in"));
    let Value::List(ids) = &first[2] else {
        panic!("ids are not a list: {first:?}");
    };
    let ids = ids
        .iter()
        .map(|id| match id {
            Value::Int(id) => *id,
            other => panic!("id {other:?}"),
        })
        .collect();
    (ids, rest.to_vec())
}

/// Searches `model` as `env` with the returned domain: the id term and the archived term, the only
/// two a group can carry.
async fn search_by_action(
    tx: &mut Transaction,
    env: &Env,
    model: &str,
    action: &Value,
) -> Vec<i64> {
    let (ids, rest) = domain_parts(action);
    let mut terms = vec![Domain::condition("id", Operator::In, ids).unwrap()];
    for term in rest {
        assert_eq!(
            term,
            Value::list([
                Value::str("active"),
                Value::str("in"),
                Value::list([Value::Bool(true), Value::Bool(false)]),
            ])
        );
        terms.push(Domain::condition("active", Operator::In, vec![true, false]).unwrap());
    }
    let mut found = env
        .records(model)
        .unwrap()
        .search(tx, &Domain::and(terms))
        .await
        .unwrap()
        .ids();
    found.sort_unstable();
    found
}

#[tokio::test]
async fn a_model_group_opens_its_records_in_one_state() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "me", None).await;
    let (a, b, c, d) = (
        notebook(&mut tx, "A").await,
        notebook(&mut tx, "B").await,
        notebook(&mut tx, "C").await,
        notebook(&mut tx, "D").await,
    );
    let admin = tx.env();
    assigned(&mut tx, &admin, NOTEBOOK, a, 42, days(today(), -2)).await;
    assigned(&mut tx, &admin, NOTEBOOK, a, 42, days(today(), 4)).await;
    assigned(&mut tx, &admin, NOTEBOOK, b, 42, today()).await;
    assigned(&mut tx, &admin, NOTEBOOK, c, 42, days(today(), 3)).await;
    assigned(&mut tx, &admin, NOTEBOOK, d, 42, days(today(), -1)).await;
    archive(&mut tx, &admin, NOTEBOOK, d).await;
    let me = tx.env().with_user(42);
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    let books = group(&menu, NOTEBOOK);
    let expected = [
        ("all", sorted(vec![a, b, d]), books.total_count()),
        ("overdue", sorted(vec![a, d]), books.overdue_count()),
        ("today", vec![b], books.today_count()),
        ("planned", vec![c], books.planned_count()),
    ];
    for (state, ids, count) in expected {
        let action = open_action(&mut tx, &me, NOTEBOOK, state).await.unwrap();
        assert_eq!(
            member(&action, "type"),
            &Value::str("ir.actions.act_window")
        );
        assert_eq!(member(&action, "name"), &Value::str(books.name()));
        assert_eq!(member(&action, "res_model"), &Value::str(NOTEBOOK));
        assert_eq!(member(&action, "target"), &Value::str("current"));
        assert_eq!(
            member(&action, "views"),
            &Value::list([
                Value::list([Value::Bool(false), Value::str("list")]),
                Value::list([Value::Bool(false), Value::str("form")]),
            ])
        );
        assert_eq!(
            member(&action, "context"),
            &Value::Map(Arc::new(ValueMap::default()))
        );
        let (listed, rest) = domain_parts(&action);
        assert_eq!(listed, ids, "{state}");
        assert_eq!(listed.len(), count as usize, "{state}");
        let Value::List(group_terms) = books.domain() else {
            panic!("the group's domain is not a list");
        };
        assert_eq!(rest.as_slice(), &group_terms[..], "{state}");
        // the list a client opens: the same records, the archived one included
        assert_eq!(
            search_by_action(&mut tx, &me, NOTEBOOK, &action).await,
            ids,
            "{state}"
        );
    }
    // the typed accessor agrees with the RPC for every state
    for (state, parsed) in STATES {
        let typed = activity_group_action(&me, &mut tx, NOTEBOOK, parsed)
            .await
            .unwrap();
        assert_eq!(
            typed,
            open_action(&mut tx, &me, NOTEBOOK, state).await.unwrap()
        );
        assert_eq!(books.open_ids(parsed), domain_parts(&typed).0);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn other_activities_open_the_callers_own_activities_never_the_hidden_documents() {
    let h = harness_policy(|policy| {
        policy
            .rule(
                TASK,
                Operation::Read,
                Domain::condition("name", Operator::Ne, "hidden").unwrap(),
            )
            .deny(NOTEBOOK, Operation::Read)
    })
    .await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "me", None).await;
    let admin = tx.env();
    let hidden = task(&mut tx, "hidden").await;
    let gone = task(&mut tx, "gone").await;
    let book = notebook(&mut tx, "refused").await;
    let todo = personal(&mut tx, &admin, 42, days(today(), 2)).await;
    let on_hidden = assigned(&mut tx, &admin, TASK, hidden, 42, today()).await;
    let on_book = assigned(&mut tx, &admin, NOTEBOOK, book, 42, days(today(), -1)).await;
    let on_gone = assigned(&mut tx, &admin, TASK, gone, 42, days(today(), -3)).await;
    tx.flush().await.unwrap();
    tx.execute("DELETE FROM test_mail_task WHERE id = $1", &[&gone])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    let me = tx.env().with_user(42);
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    let other = group(&menu, "mail.activity");
    let expected = [
        ("all", sorted(vec![on_hidden, on_book, on_gone])),
        ("overdue", sorted(vec![on_book, on_gone])),
        ("today", vec![on_hidden]),
        ("planned", vec![todo]),
    ];
    for (state, ids) in expected {
        let action = open_action(&mut tx, &me, "mail.activity", state)
            .await
            .unwrap();
        assert_eq!(member(&action, "res_model"), &Value::str("mail.activity"));
        assert_eq!(member(&action, "name"), &Value::str("Other Activities"));
        assert_eq!(
            member(&action, "domain"),
            &Value::list([Value::list([
                Value::str("id"),
                Value::str("in"),
                Value::list(ids.iter().map(|id| Value::Int(*id))),
            ])]),
            "{state}"
        );
        let parsed = STATES.iter().find(|(name, _)| *name == state).unwrap().1;
        assert_eq!(other.open_ids(parsed), ids, "{state}");
        // the ids are activity ids: U's own activities, readable as U
        assert_eq!(
            search_by_action(&mut tx, &me, "mail.activity", &action).await,
            ids,
            "{state}"
        );
        // nothing names a hidden or deleted document or its model
        let shown = format!("{action:?}");
        for needle in ["hidden", "refused", TASK, NOTEBOOK] {
            assert!(!shown.contains(needle), "{needle} appears in {shown}");
        }
    }
    // the hidden documents' own ids are not in any answer unless they equal an activity id
    for (_, parsed) in STATES {
        let typed = activity_group_action(&me, &mut tx, "mail.activity", parsed)
            .await
            .unwrap();
        let (ids, _) = domain_parts(&typed);
        assert_eq!(ids, other.open_ids(parsed));
        for id in ids {
            assert!([todo, on_hidden, on_book, on_gone].contains(&id));
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// 1000 records; (iv) the public and portal users are in `mail_activity_install.rs`)
#[tokio::test]
async fn an_open_request_that_cannot_be_answered_is_refused() {
    let h = harness_policy(|policy| policy.deny(TASK, Operation::Read)).await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    user(&mut tx, 42, "me", None).await;
    let admin = tx.env();
    let n = notebook(&mut tx, "N").await;
    let t = task(&mut tx, "T").await;
    let finished = notebook(&mut tx, "Finished").await;
    assigned(&mut tx, &admin, TASK, t, 42, today()).await;
    let done = assigned(&mut tx, &admin, NOTEBOOK, finished, 42, today()).await;
    archive(&mut tx, &admin, "mail.activity", done).await;
    let me = tx.env().with_user(42);
    // (i)
    for state in ["", "ALL", "open"] {
        let error = open_action(&mut tx, &me, NOTEBOOK, state)
            .await
            .unwrap_err();
        assert_validation(
            &error,
            &format!(
                "The activity state {state:?} is not known; use all, overdue, today or planned."
            ),
        );
    }
    assert_validation(
        &OpenState::parse("sometime").unwrap_err(),
        "The activity state \"sometime\" is not known; use all, overdue, today or planned.",
    );
    // (ii) a model that does not exist, one with only a done activity, one U cannot read (its
    // activity counts as "other") and models without activities: one message each, naming only
    // the caller's own input
    for model in [
        "no.such.model",
        NOTEBOOK,
        TASK,
        "res.partner",
        "mail.message",
    ] {
        for state in ["all", "planned"] {
            let error = open_action(&mut tx, &me, model, state).await.unwrap_err();
            assert_validation(
                &error,
                &format!(
                    "You have no open activities on {model:?} any more; open the activity menu \
                     again."
                ),
            );
        }
    }
    assigned(&mut tx, &admin, NOTEBOOK, n, 42, today()).await;
    // (iii) more than 1000 records in a state: the scan bound is raised so that they are all seen
    let batch: Vec<RawVals> = (0..=MAX_OPEN_RECORDS)
        .map(|index| RawVals::new().set_cell("name", text(&format!("bulk {index}"))))
        .collect();
    let bulk = admin
        .records(NOTEBOOK)
        .unwrap()
        .create(&mut tx, batch)
        .await
        .unwrap()
        .ids();
    assert_eq!(bulk.len(), MAX_OPEN_RECORDS + 1);
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO mail_activity (res_model, res_id, date_deadline, user_id, create_uid, \
         active, automated) SELECT $1, id, CURRENT_DATE + 7, 42, 42, true, false \
         FROM unnest($2::bigint[]) AS id",
        &[&NOTEBOOK, &bulk],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    set_limit(&mut tx, &admin, "5000").await;
    let menu = activity_groups(&me, &mut tx).await.unwrap();
    assert_eq!(
        group(&menu, NOTEBOOK).planned_count() as usize,
        MAX_OPEN_RECORDS + 1
    );
    let error = open_action(&mut tx, &me, NOTEBOOK, "planned")
        .await
        .unwrap_err();
    assert_validation(
        &error,
        "1001 records match; a list opens at most 1000. Open them from the records' own list \
         instead.",
    );
    // the other states stay answerable: only the over-long one is refused
    let today_only = open_action(&mut tx, &me, NOTEBOOK, "today").await.unwrap();
    assert_eq!(domain_parts(&today_only).0, vec![n]);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
