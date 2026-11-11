//! scheduled messages for record threads (`mail.scheduled.message`), on the
//! production policy (compiled from `ir.access` rows, the principal resolver, the job runner with
//! the real principal resolver). Spec cases and
//! , design.
//!
//! The test lives here and not under `addons/mail/tests` because an addon crate may not depend on
//! `erp-security` (`scripts/check-layers.py`, rule 1). The shipped rows of
//! `addons/mail/security/ir.access.csv` are parsed and inserted as the installer would.
//!
//! Time: the database clock decides "past" and "due". A job is made due by moving its `run_after`
//! into the past with SQL, as the job tests do; nothing sleeps.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_addon_mail::MailThread;
use erp_addon_mail::ScheduledMessageRequest as ScheduleRequest;
use erp_addon_mail::scheduled::{MODEL, SEND_KIND};
use erp_addon_mail::{CommentOption, EditRequest, MAX_SCHEDULED_PER_THREAD, MailScheduledMessage};
use erp_orm::jobs::{EnqueueRequest, JobKey, JobRunner, OnDuplicate, When};
use erp_orm::{Orm, PolicyProvider, UserId};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{ErrorKind, ValueMap};
use erp_security::jobs::SecurityJobPrincipalResolver;
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};

erp_sdk::addon!(name = "test_sched", depends = ["mail"]);

/// A thread model internal users may use unless its name is `locked`.
#[model(
    name = "test_sched.ticket",
    description = "Scheduled Test Ticket",
    inherit = [MailThread],
    addon = "test_sched"
)]
pub struct Ticket {
    #[field(char)]
    pub name: Option<Arc<str>>,
    /// Written by the extension of `_message_created_hook` below.
    #[field(char)]
    pub remark: Option<Arc<str>>,
}

#[extend(MailScheduledMessage, addon = "test_sched")]
pub struct SchedHook {}

#[methods(MailScheduledMessage, addon = "test_sched")]
impl SchedHook {
    #[method(override)]
    async fn _message_created_hook(
        this: &RecordSet<MailScheduledMessage>,
        tx: &mut Transaction,
        message_id: i64,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), (message_id,)).await?;
        let Cell::Id(res_id) = this.as_records().cell(tx, "res_id").await? else {
            return Err(Error::invariant("a scheduled message has no res_id"));
        };
        this.env()
            .records(TICKET)?
            .sudo(true)
            .browse([res_id])?
            .write(
                tx,
                RawVals::new().set_cell("remark", Cell::Str(format!("hook:{message_id}").into())),
            )
            .await
    }
}

const TICKET: &str = "test_sched.ticket";
const ACCESS_CSV: &str = include_str!("../../../addons/mail/security/ir.access.csv");
const MANIFEST: &str = include_str!("../../../addons/mail/manifest.toml");
const HOUR: i64 = 3_600_000_000;

const PAST: &str = "A Scheduled Message cannot be scheduled in the past";
const NOT_THREAD: &str =
    "A message cannot be scheduled on a model that does not have a mail thread.";
const TOO_FAR: &str = "A Scheduled Message cannot be scheduled more than 365 days ahead";
const INTERNAL_ONLY: &str = "Scheduled messages are for internal users.";
const CREATE_REFUSED: &str = "A scheduled message is created with schedule(): a direct create would leave it without a delivery job.";
const DELIVERY_REFUSED: &str = "The delivery fields of a scheduled message (scheduled_date, arm_seq, state, failure_reason) are set by edit, send now and the delivery job only.";
const FAILED_ACCESS: &str =
    "The sender no longer has the right to post on the record, or is no longer an internal user.";
const RECORD_DENIED: &str =
    "You are not allowed to access the record this message is scheduled on.";

// ---------------------------------------------------------------------------------------------
// fixture

fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                chars.next();
                current.push('"');
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut current)),
            (other, _) => current.push(other),
        }
    }
    fields.push(current);
    fields
}

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("mail_scheduled: connection closed with an error: {error}");
        }
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn exec(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    client.execute(sql, params).await.unwrap();
}

async fn text_of(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> String {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn group(client: &Client, name: &str, xmlid: &str) -> i64 {
    let id = one(
        client,
        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
        &[&name],
    )
    .await;
    let (module, local) = xmlid.split_once('.').unwrap();
    exec(
        client,
        "INSERT INTO ir_model_data (module, name, model, res_id) VALUES ($1, $2, 'res.groups', $3)",
        &[&module, &local, &id],
    )
    .await;
    id
}

async fn model_row(client: &Client, model: &str, description: &str) -> i64 {
    one(
        client,
        "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $2, 'base', false) \
         RETURNING id::int8",
        &[&description, &model],
    )
    .await
}

async fn rule(
    client: &Client,
    name: &str,
    model: i64,
    group: i64,
    ops: &str,
    domain: Option<&str>,
) {
    exec(
        client,
        "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
         VALUES ($1, $2, $3, $4, $5, true)",
        &[&name, &model, &group, &ops, &domain],
    )
    .await;
}

/// A user with its own partner, in `groups`; returns (user, partner).
async fn user(client: &Client, login: &str, groups: &[i64], share: bool) -> (i64, i64) {
    let partner = one(
        client,
        "INSERT INTO res_partner (name, active) VALUES ($1, true) RETURNING id::int8",
        &[&login],
    )
    .await;
    let uid = one(
        client,
        "INSERT INTO res_users (name, login, active, share, partner_id) \
         VALUES ($1, $1, true, $2, $3) RETURNING id::int8",
        &[&login, &share, &partner],
    )
    .await;
    for g in groups {
        exec(
            client,
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&uid, g],
        )
        .await;
    }
    (uid, partner)
}

struct World {
    h: Harness,
    orm: Orm,
    client: Client,
    /// internal users: (user, partner)
    ann: (i64, i64),
    bob: (i64, i64),
    /// a system administrator (also an internal user)
    adm: (i64, i64),
    /// a portal user: no rule on tickets, may read partners
    por: (i64, i64),
    /// an internal user who may also write partners (a partner is a thread whose post access is
    /// read: the case where write and post access differ)
    ned: (i64, i64),
    /// a contact record the partners' rules apply to
    p1: i64,
    t1: i64,
    t2: i64,
    comment: i64,
    note: i64,
}

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

fn text(s: &str) -> Cell {
    Cell::Str(s.into())
}

async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    tx.env()
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

async fn world() -> World {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_addon_mail::ADDON,
        &ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap();
    let client = connect(&h).await;
    let g_user = group(&client, "Internal User", "base.group_user").await;
    let g_portal = group(&client, "Portal", "base.group_portal").await;
    let g_public = group(&client, "Public", "base.group_public").await;
    let g_system = group(&client, "Administrator", "base.group_system").await;
    let g_manager = group(&client, "Access Rights", "base.group_erp_manager").await;
    let g_partner_writers = group(
        &client,
        "Partner writers",
        "test_sched.group_partner_writers",
    )
    .await;
    let groups: BTreeMap<&str, i64> = BTreeMap::from([
        ("base.group_user", g_user),
        ("base.group_portal", g_portal),
        ("base.group_public", g_public),
        ("base.group_system", g_system),
    ]);
    let mut models: BTreeMap<&str, i64> = BTreeMap::new();
    for (xmlid, name, description) in [
        ("model_mail_message", "mail.message", "Message"),
        (
            "model_mail_message_subtype",
            "mail.message.subtype",
            "Subtype",
        ),
        ("model_mail_followers", "mail.followers", "Followers"),
        (
            "model_mail_scheduled_message",
            "mail.scheduled.message",
            "Scheduled Message",
        ),
    ] {
        models.insert(xmlid, model_row(&client, name, description).await);
    }
    // the shipped rows, as the installer reads them
    for line in ACCESS_CSV.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f = csv_fields(line);
        let [_, name, model, group, ops, domain] = f.as_slice() else {
            panic!("a row of six columns: {line}");
        };
        // the activity models are not in this fixture's registry; their rows are
        // exercised by `addons/mail/tests/activity.rs`
        if model.starts_with("model_mail_activity") {
            continue;
        }
        let domain = (!domain.is_empty()).then_some(domain.as_str());
        rule(
            &client,
            name,
            models[model.as_str()],
            groups[group.as_str()],
            ops,
            domain,
        )
        .await;
    }
    // the test thread model's own rules: internal users everywhere but the locked record,
    // administrators everywhere
    let ticket = model_row(&client, TICKET, "Scheduled Test Ticket").await;
    rule(
        &client,
        "internal tickets",
        ticket,
        g_user,
        "crud",
        Some("[('name', '!=', 'locked')]"),
    )
    .await;
    rule(&client, "admin tickets", ticket, g_system, "crud", None).await;
    let ann = user(&client, "ann", &[g_user], false).await;
    let bob = user(&client, "bob", &[g_user], false).await;
    let adm = user(&client, "adm", &[g_user, g_system, g_manager], false).await;
    let por = user(&client, "por", &[g_portal], true).await;
    let ned = user(&client, "ned", &[g_user, g_partner_writers], false).await;
    // partners are threads whose post access is read (`res.partner` overrides the hook): internal
    // users and portal users may read them, only the writers may write
    let partner_model = model_row(&client, "res.partner", "Contact").await;
    rule(
        &client,
        "internal partners",
        partner_model,
        g_user,
        "r",
        None,
    )
    .await;
    rule(
        &client,
        "portal partners",
        partner_model,
        g_portal,
        "r",
        None,
    )
    .await;
    rule(
        &client,
        "writer partners",
        partner_model,
        g_partner_writers,
        "crud",
        None,
    )
    .await;
    let p1 = one(
        &client,
        "INSERT INTO res_partner (name, active) VALUES ('contact', true) RETURNING id::int8",
        &[],
    )
    .await;

    let source = PolicySource::build(Arc::clone(h.registry()), pool(&h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(h.registry()), pool(&h, 4))
        .policy(provider)
        .resolver(Resolver::id_order())
        .require_job_kinds(erp_addon_mail::job_kinds().iter())
        .build()
        .unwrap();

    // data, as the superuser
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let comment = create(
        &mut tx,
        "mail.message.subtype",
        RawVals::new()
            .set_cell("name", text("Messages"))
            .set_cell("sequence", Cell::Int(0)),
    )
    .await;
    let note = create(
        &mut tx,
        "mail.message.subtype",
        RawVals::new()
            .set_cell("name", text("Notes"))
            .set_cell("default", Cell::Bool(false))
            .set_cell("internal", Cell::Bool(true))
            .set_cell("sequence", Cell::Int(90)),
    )
    .await;
    for (name, id) in [("mt_comment", comment), ("mt_note", note)] {
        create(
            &mut tx,
            "ir.model.data",
            RawVals::new()
                .set_cell("module", text("mail"))
                .set_cell("name", text(name))
                .set_cell("model", text("mail.message.subtype"))
                .set_cell("res_id", Cell::Id(id)),
        )
        .await;
    }
    let t1 = create(
        &mut tx,
        TICKET,
        RawVals::new().set_cell("name", text("one")),
    )
    .await;
    let t2 = create(
        &mut tx,
        TICKET,
        RawVals::new().set_cell("name", text("two")),
    )
    .await;
    tx.commit().await.unwrap();
    World {
        h,
        orm,
        client,
        ann,
        bob,
        adm,
        por,
        ned,
        p1,
        t1,
        t2,
        comment,
        note,
    }
}

async fn begin(w: &World, uid: i64) -> Transaction {
    w.orm.begin(UserId::new(uid).unwrap()).await.unwrap()
}

/// A superuser transaction.
async fn begin_su(w: &World) -> Transaction {
    w.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap()
}

/// The database clock in microseconds since the epoch.
async fn db_now(w: &World) -> i64 {
    one(
        &w.client,
        "SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::int8",
        &[],
    )
    .await
}

fn at(micros: i64) -> DateTime {
    DateTime::from_micros(micros)
}

fn assert_err(error: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error:?}");
    assert_eq!(error.message(), message);
}

/// Schedule as `uid` in its own transaction; committed on success.
async fn schedule(w: &World, uid: i64, request: ScheduleRequest) -> Result<i64> {
    let mut tx = begin(w, uid).await;
    let records = tx.env().records(MODEL)?;
    let id: i64 = records.call(&mut tx, "schedule", (request,)).await?;
    tx.commit().await.unwrap();
    Ok(id)
}

fn request(w: &World, res_id: i64, body: &str, when: i64) -> ScheduleRequest {
    let _ = w;
    ScheduleRequest::new(TICKET, res_id, body, at(when))
}

async fn edit(w: &World, uid: i64, id: i64, patch: EditRequest) -> Result<()> {
    let mut tx = begin(w, uid).await;
    let row = tx.env().records(MODEL)?.browse([id])?;
    row.call::<_, ()>(&mut tx, "edit", (patch,)).await?;
    tx.commit().await.unwrap();
    Ok(())
}

async fn send_now(w: &World, uid: i64, id: i64) -> Result<i64> {
    let mut tx = begin(w, uid).await;
    let row = tx.env().records(MODEL)?.browse([id])?;
    let message: i64 = row.call(&mut tx, "send_now", ()).await?;
    tx.commit().await.unwrap();
    Ok(message)
}

async fn cancel(w: &World, uid: i64, id: i64) -> Result<()> {
    let mut tx = begin(w, uid).await;
    let row = tx.env().records(MODEL)?.browse([id])?;
    row.call::<_, ()>(&mut tx, "cancel", ()).await?;
    tx.commit().await.unwrap();
    Ok(())
}

fn runner(w: &World) -> JobRunner {
    JobRunner::builder(w.orm.clone(), SecurityJobPrincipalResolver)
        .build()
        .unwrap()
}

/// Tick until nothing more is claimed (a tick claims at most the runner's batch); the total.
async fn run_all(w: &World) -> u32 {
    let runner = runner(w);
    let mut total = 0;
    for _ in 0..10 {
        let claimed = runner.tick().await.unwrap().claimed;
        if claimed == 0 {
            break;
        }
        total += claimed;
    }
    total
}

/// Move every delivery job's due time into the past (the clock stays put).
async fn make_due(w: &World) {
    exec(
        &w.client,
        "UPDATE erp_job SET run_after = clock_timestamp() - interval '1 second' \
         WHERE kind = $1 AND state = 'ready'",
        &[&SEND_KIND],
    )
    .await;
}

async fn messages_on(w: &World, res_id: i64) -> i64 {
    one(
        &w.client,
        "SELECT count(*)::int8 FROM mail_message WHERE model = $1 AND res_id = $2",
        &[&TICKET, &res_id],
    )
    .await
}

async fn rows(w: &World) -> i64 {
    one(
        &w.client,
        "SELECT count(*)::int8 FROM mail_scheduled_message",
        &[],
    )
    .await
}

async fn jobs(w: &World) -> i64 {
    one(
        &w.client,
        "SELECT count(*)::int8 FROM erp_job WHERE kind = $1",
        &[&SEND_KIND],
    )
    .await
}

async fn lock_ticket(w: &World, id: i64) {
    exec(
        &w.client,
        "UPDATE test_sched_ticket SET name = 'locked' WHERE id = $1",
        &[&id],
    )
    .await;
}

async fn unlock_ticket(w: &World, id: i64) {
    exec(
        &w.client,
        "UPDATE test_sched_ticket SET name = 'open' WHERE id = $1",
        &[&id],
    )
    .await;
}

/// The topic `thread.scheduled` through `store_fetch`, as `uid`: the whole answer as JSON.
async fn fetch(w: &World, uid: i64, model: &str, res_id: i64) -> Result<Json> {
    let mut tx = begin(w, uid).await;
    let env = tx.env();
    let item: ValueMap = [
        ("name".to_owned(), Value::str("thread.scheduled")),
        (
            "params".to_owned(),
            Value::Json(Arc::new(json!({"model": model, "res_id": res_id}))),
        ),
    ]
    .into_iter()
    .collect();
    let topics = Value::List(vec![Value::Map(item.into())].into());
    let store = env.records("mail.store")?;
    let answer: Value = store.call(&mut tx, "store_fetch", (1_u64, topics)).await?;
    erp_fields::convert::value_to_json(&answer)
}

fn ids_of(answer: &Json) -> Vec<u64> {
    answer["topics"][0]["data"]["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_u64().unwrap())
        .collect()
}

fn record(answer: &Json, id: u64) -> &Json {
    answer["store"]["records"][MODEL]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("record {id} is not in {answer}"))
}

#[tokio::test]
async fn a_past_date_is_refused_on_create_and_on_reschedule() {
    let w = world().await;
    let now = db_now(&w).await;
    let error = schedule(&w, w.ann.0, request(&w, w.t1, "late", now - HOUR))
        .await
        .unwrap_err();
    assert_err(&error, ErrorKind::Validation, PAST);
    assert_eq!(rows(&w).await, 0);
    assert_eq!(jobs(&w).await, 0);

    let id = schedule(&w, w.ann.0, request(&w, w.t1, "later", now + HOUR))
        .await
        .unwrap();
    let mut patch = EditRequest::new();
    patch.scheduled_date = Some(at(now - HOUR));
    let error = edit(&w, w.ann.0, id, patch).await.unwrap_err();
    assert_err(&error, ErrorKind::Validation, PAST);
    // the refused edit changed nothing: the generation and the job are the first ones
    assert_eq!(
        one(
            &w.client,
            "SELECT arm_seq::int8 FROM mail_scheduled_message WHERE id = $1",
            &[&id]
        )
        .await,
        1
    );
    assert_eq!(jobs(&w).await, 1);
    let day = 24 * HOUR;
    let error = schedule(&w, w.ann.0, request(&w, w.t1, "far", now + 366 * day))
        .await
        .unwrap_err();
    assert_err(&error, ErrorKind::Validation, TOO_FAR);
    let mut patch = EditRequest::new();
    patch.scheduled_date = Some(at(now + 366 * day));
    let error = edit(&w, w.ann.0, id, patch).await.unwrap_err();
    assert_err(&error, ErrorKind::Validation, TOO_FAR);
    let near = schedule(&w, w.ann.0, request(&w, w.t1, "near", now + 364 * day))
        .await
        .unwrap();
    assert!(near > 0);
    assert_eq!(rows(&w).await, 2);
    // and the direct ORM paths say the same
    let mut tx = begin_su(&w).await;
    let error = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse([id])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("scheduled_date", Cell::DateTime(at(now + 400 * day))),
        )
        .await
        .unwrap_err();
    assert_err(&error, ErrorKind::Validation, TOO_FAR);
    drop(tx);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_job_posts_as_the_creator_with_the_drafts_text_and_removes_the_row() {
    let w = world().await;
    let now = db_now(&w).await;
    let mut message = request(&w, w.t1, "Hello <b>there</b>", now + HOUR);
    message.subject = Some("Subject line".to_owned());
    message.partner_ids = vec![w.bob.1];
    let id = schedule(&w, w.ann.0, message).await.unwrap();
    let mut note = request(&w, w.t1, "Internal remark", now + HOUR);
    note.is_note = true;
    let note_id = schedule(&w, w.ann.0, note).await.unwrap();
    assert_eq!(
        messages_on(&w, w.t1).await,
        0,
        "nothing is posted before the date"
    );

    make_due(&w).await;
    let report = runner(&w).tick().await.unwrap();
    assert_eq!(report.claimed, 2);
    assert_eq!(messages_on(&w, w.t1).await, 2);
    let posted = one(
        &w.client,
        "SELECT id::int8 FROM mail_message WHERE model = $1 AND res_id = $2 AND subject = 'Subject line'",
        &[&TICKET, &w.t1],
    )
    .await;
    // authored by the creator, as a comment, to the recipient; the text is escaped, not trusted
    assert_eq!(
        one(
            &w.client,
            "SELECT author_id::int8 FROM mail_message WHERE id = $1",
            &[&posted]
        )
        .await,
        w.ann.1
    );
    assert_eq!(
        one(
            &w.client,
            "SELECT subtype_id::int8 FROM mail_message WHERE id = $1",
            &[&posted]
        )
        .await,
        w.comment
    );
    assert_eq!(
        text_of(
            &w.client,
            "SELECT body FROM mail_message WHERE id = $1",
            &[&posted]
        )
        .await,
        "<p>Hello &lt;b&gt;there&lt;/b&gt;</p>"
    );
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM mail_message_res_partner_rel WHERE mail_message_id = $1 \
             AND res_partner_id = $2",
            &[&posted, &w.bob.1]
        )
        .await,
        1
    );
    let noted = one(
        &w.client,
        "SELECT subtype_id::int8 FROM mail_message WHERE model = $1 AND res_id = $2 AND subject IS NULL",
        &[&TICKET, &w.t1],
    )
    .await;
    assert_eq!(noted, w.note);
    // the rows are gone and the jobs finished (history, no live job)
    assert_eq!(rows(&w).await, 0);
    assert_eq!(jobs(&w).await, 0);
    let _ = (id, note_id);
    w.h.cleanup().await.unwrap();
}

/// failed draft stays visible with its reason instead)
#[tokio::test]
async fn a_failed_post_keeps_the_draft_visible_and_the_others_still_send() {
    let w = world().await;
    let now = db_now(&w).await;
    let failing = schedule(&w, w.ann.0, request(&w, w.t1, "fails", now + HOUR))
        .await
        .unwrap();
    let fine = schedule(&w, w.bob.0, request(&w, w.t2, "works", now + HOUR))
        .await
        .unwrap();
    // the creator of the first loses the right to post on its record before the date
    lock_ticket(&w, w.t1).await;
    make_due(&w).await;
    let report = runner(&w).tick().await.unwrap();
    assert_eq!(report.claimed, 2);
    assert_eq!(messages_on(&w, w.t1).await, 0);
    assert_eq!(messages_on(&w, w.t2).await, 1);
    assert_eq!(
        text_of(
            &w.client,
            "SELECT state FROM mail_scheduled_message WHERE id = $1",
            &[&failing]
        )
        .await,
        "failed"
    );
    let reason = text_of(
        &w.client,
        "SELECT failure_reason FROM mail_scheduled_message WHERE id = $1",
        &[&failing],
    )
    .await;
    assert!(!reason.is_empty() && reason.len() <= 512, "{reason}");
    // the failure is a finished job, not a dead letter: nothing is left to retry by itself
    assert_eq!(jobs(&w).await, 0);
    let _ = fine;
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn scheduling_needs_a_thread_model_post_access_and_room_on_the_record() {
    let w = world().await;
    let now = db_now(&w).await;
    // a model without a mail thread
    let mut other = ScheduleRequest::new("res.currency", 1, "x", at(now + HOUR));
    other.partner_ids = vec![];
    let error = schedule(&w, w.ann.0, other).await.unwrap_err();
    assert_err(&error, ErrorKind::Validation, NOT_THREAD);
    // a model that is not in the registry at all gets the same answer
    let error = schedule(
        &w,
        w.ann.0,
        ScheduleRequest::new("no.such.model", 1, "x", at(now + HOUR)),
    )
    .await
    .unwrap_err();
    assert_err(&error, ErrorKind::Validation, NOT_THREAD);
    // a user with no right on the ticket model, and a record the policy hides from internal users
    let denied = schedule(&w, w.por.0, request(&w, w.t1, "x", now + HOUR))
        .await
        .unwrap_err();
    assert_err(&denied, ErrorKind::Access, INTERNAL_ONLY);
    lock_ticket(&w, w.t2).await;
    let denied = schedule(&w, w.ann.0, request(&w, w.t2, "x", now + HOUR))
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::Access, "{denied:?}");
    unlock_ticket(&w, w.t2).await;
    assert_eq!(rows(&w).await, 0);
    assert_eq!(jobs(&w).await, 0);
    // the allowed case: the row is the caller's, nothing is posted, one job is queued
    let id = schedule(&w, w.ann.0, request(&w, w.t1, "ok", now + HOUR))
        .await
        .unwrap();
    let row = w
        .client
        .query_one(
            "SELECT create_uid::int8, author_id::int8, state, arm_seq::int8 \
             FROM mail_scheduled_message WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), w.ann.0);
    assert_eq!(row.get::<_, i64>(1), w.ann.1);
    assert_eq!(row.get::<_, String>(2), "scheduled");
    assert_eq!(row.get::<_, i64>(3), 1);
    assert_eq!(messages_on(&w, w.t1).await, 0);
    let job = w
        .client
        .query_one(
            "SELECT dedup_key, payload::text FROM erp_job WHERE kind = $1",
            &[&SEND_KIND],
        )
        .await
        .unwrap();
    assert_eq!(job.get::<_, String>(0), format!("{id}:1"));
    assert_eq!(
        job.get::<_, String>(1),
        format!("{{\"id\": {id}, \"seq\": 1}}")
    );
    // room: a record holding the maximum refuses one more
    exec(
        &w.client,
        "INSERT INTO mail_scheduled_message (model, res_id, body, scheduled_date, author_id, \
         state, arm_seq, is_note, autofollow_recipients, slot) \
         SELECT $1, $2, 'bulk', now() AT TIME ZONE 'UTC' + interval '1 day', $3, 'scheduled', 1, \
         false, false, g FROM generate_series(1, $4::int4) g",
        &[
            &TICKET,
            &w.t2,
            &w.ann.1,
            &i32::try_from(MAX_SCHEDULED_PER_THREAD).unwrap(),
        ],
    )
    .await;
    let error = schedule(&w, w.ann.0, request(&w, w.t2, "one more", now + HOUR))
        .await
        .unwrap_err();
    assert_err(
        &error,
        ErrorKind::Validation,
        "At most 100 messages can be scheduled on one record, and this one has 100.",
    );
    // write access, not post access, on a record where they differ; internal users only
    write_access_on_the_record_decides_not_the_post_access(&w, now).await;
    w.h.cleanup().await.unwrap();
}

/// exist)
#[tokio::test]
async fn attachments_and_cc_are_refused_naming_their_slice() {
    let w = world().await;
    let now = db_now(&w).await;
    let mut with_attachment = request(&w, w.t1, "x", now + HOUR);
    with_attachment.attachment_ids = vec![5];
    let error = schedule(&w, w.ann.0, with_attachment).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::Validation,
        "Attachments on a scheduled message need ir.attachment (a later release), which is not \
         available yet.",
    );
    let mut with_cc = request(&w, w.t1, "x", now + HOUR);
    with_cc.partner_cc_ids = vec![w.bob.1];
    let error = schedule(&w, w.ann.0, with_cc).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::Validation,
        "Cc recipients on a scheduled message need partner_cc_ids on mail.message (the outgoing \
         mail slice), which is not available yet.",
    );
    assert_eq!(rows(&w).await, 0);
    assert_eq!(jobs(&w).await, 0);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn only_the_creator_edits_a_new_date_rearms_and_the_target_cannot_change() {
    let w = world().await;
    let now = db_now(&w).await;
    let id = schedule(&w, w.ann.0, request(&w, w.t1, "first", now + HOUR))
        .await
        .unwrap();
    // another user who may post on the record cannot edit
    let mut patch = EditRequest::new();
    patch.body = Some("hijacked".to_owned());
    let error = edit(&w, w.bob.0, id, patch).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::Access,
        "Only the author of a scheduled message can edit it.",
    );
    // a user who may not post on the record learns nothing but the fixed refusal
    let mut patch = EditRequest::new();
    patch.body = Some("x".to_owned());
    let error = edit(&w, w.por.0, id, patch).await.unwrap_err();
    assert_err(&error, ErrorKind::Access, INTERNAL_ONLY);
    // an internal user who may not write the record learns nothing but the fixed refusal
    lock_ticket(&w, w.t1).await;
    let mut patch = EditRequest::new();
    patch.body = Some("x".to_owned());
    let error = edit(&w, w.ann.0, id, patch).await.unwrap_err();
    assert_err(&error, ErrorKind::Access, RECORD_DENIED);
    unlock_ticket(&w, w.t1).await;
    // the creator edits the text: same generation, same single job
    let mut patch = EditRequest::new();
    patch.body = Some("second".to_owned());
    patch.composition_comment_option = Some(CommentOption::Forward);
    edit(&w, w.ann.0, id, patch).await.unwrap();
    let row = w
        .client
        .query_one(
            "SELECT body, arm_seq::int8, composition_comment_option \
             FROM mail_scheduled_message WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "second");
    assert_eq!(row.get::<_, i64>(1), 1);
    assert_eq!(row.get::<_, String>(2), "forward");
    assert_eq!(jobs(&w).await, 1);
    // an empty edit is refused
    let error = edit(&w, w.ann.0, id, EditRequest::new()).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::Validation,
        "An edit changes at least one field of the scheduled message.",
    );
    // a new date advances the generation: the old generation's job is cancelled and exactly one
    // live job, of the new generation, remains
    let mut patch = EditRequest::new();
    patch.scheduled_date = Some(at(now + 2 * HOUR));
    edit(&w, w.ann.0, id, patch).await.unwrap();
    assert_eq!(
        one(
            &w.client,
            "SELECT arm_seq::int8 FROM mail_scheduled_message WHERE id = $1",
            &[&id]
        )
        .await,
        2
    );
    assert_eq!(jobs(&w).await, 1);
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM erp_job WHERE kind = $1 AND dedup_key = $2",
            &[&SEND_KIND, &format!("{id}:2")]
        )
        .await,
        1
    );
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM erp_job_run WHERE kind = $1 AND outcome = 'cancelled'",
            &[&SEND_KIND]
        )
        .await,
        1
    );
    // the one job posts the edited text once
    make_due(&w).await;
    assert_eq!(runner(&w).tick().await.unwrap().claimed, 1);
    assert_eq!(messages_on(&w, w.t1).await, 1);
    assert_eq!(
        text_of(
            &w.client,
            "SELECT body FROM mail_message WHERE model = $1 AND res_id = $2",
            &[&TICKET, &w.t1]
        )
        .await,
        "<p>second</p>"
    );
    assert_eq!(rows(&w).await, 0);
    let other = schedule(&w, w.ann.0, request(&w, w.t1, "again", now + HOUR))
        .await
        .unwrap();
    for field in ["res_id", "model"] {
        let mut tx = begin_su(&w).await;
        let vals = if field == "res_id" {
            RawVals::new().set_cell("res_id", Cell::Id(w.t2))
        } else {
            RawVals::new().set_cell("model", text("test_sched.other"))
        };
        let error = tx
            .env()
            .records(MODEL)
            .unwrap()
            .browse([other])
            .unwrap()
            .write(&mut tx, vals)
            .await
            .unwrap_err();
        assert_err(
            &error,
            ErrorKind::UserError,
            "You are not allowed to change the target record of a scheduled message.",
        );
    }
    // the direct ORM paths
    a_direct_orm_create_or_write_of_the_delivery_fields_is_refused(&w, now).await;
    w.h.cleanup().await.unwrap();
}

/// administrator is refused until the audited elevation exists)
#[tokio::test]
async fn send_now_posts_once_as_the_creator_and_the_job_then_does_nothing() {
    let w = world().await;
    let now = db_now(&w).await;
    let id = schedule(&w, w.ann.0, request(&w, w.t1, "now please", now + HOUR))
        .await
        .unwrap();
    let error = send_now(&w, w.bob.0, id).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::UserError,
        "You are not allowed to send this scheduled message",
    );
    let error = send_now(&w, w.adm.0, id).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::Access,
        "Sending a message scheduled by another user needs the audited elevation of audit: \
         posting as that user is not available yet.",
    );
    assert_eq!(messages_on(&w, w.t1).await, 0);
    assert_eq!(rows(&w).await, 1);
    let message = send_now(&w, w.ann.0, id).await.unwrap();
    assert_eq!(messages_on(&w, w.t1).await, 1);
    assert_eq!(
        one(
            &w.client,
            "SELECT author_id::int8 FROM mail_message WHERE id = $1",
            &[&message]
        )
        .await,
        w.ann.1
    );
    assert_eq!(rows(&w).await, 0);
    // its job was cancelled with it: nothing left to claim
    assert_eq!(jobs(&w).await, 0);
    make_due(&w).await;
    assert_eq!(runner(&w).tick().await.unwrap().claimed, 0);
    assert_eq!(messages_on(&w, w.t1).await, 1);
    // a post error of the creator's own send reaches the caller and the row stays
    let stuck = schedule(&w, w.ann.0, request(&w, w.t1, "x", now + HOUR))
        .await
        .unwrap();
    exec(
        &w.client,
        "UPDATE mail_scheduled_message SET body = repeat('x', 1048577) WHERE id = $1",
        &[&stuck],
    )
    .await;
    let error = send_now(&w, w.ann.0, stuck).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::Validation,
        "A post body holds at most 1048576 bytes, not 1048577.",
    );
    assert_eq!(rows(&w).await, 1);
    assert_eq!(messages_on(&w, w.t1).await, 1);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cancel_removes_the_row_and_its_job_and_a_job_left_behind_is_a_noop() {
    let w = world().await;
    let now = db_now(&w).await;
    let id = schedule(&w, w.ann.0, request(&w, w.t1, "never sent", now + HOUR))
        .await
        .unwrap();
    // a user who is not internal cannot cancel
    let error = cancel(&w, w.por.0, id).await.unwrap_err();
    assert_err(&error, ErrorKind::Access, INTERNAL_ONLY);
    // an internal user who may not write the record learns nothing else
    lock_ticket(&w, w.t1).await;
    let error = cancel(&w, w.bob.0, id).await.unwrap_err();
    assert_err(&error, ErrorKind::Access, RECORD_DENIED);
    unlock_ticket(&w, w.t1).await;
    assert_eq!(rows(&w).await, 1);
    // another user's own cancel by key cancels nothing of the creator's job
    {
        let mut tx = begin(&w, w.bob.0).await;
        let env = tx.env();
        let key = JobKey::new(&format!("{id}:1")).unwrap();
        assert_eq!(
            tx.job_cancel_by_key(&env, SEND_KIND, &key).await.unwrap(),
            0
        );
        tx.commit().await.unwrap();
    }
    assert_eq!(jobs(&w).await, 1);
    // another internal user who may write the record cancels the row, and the creator's job goes
    // with it: no live job of the kind is left for the row
    cancel(&w, w.bob.0, id).await.unwrap();
    assert_eq!(rows(&w).await, 0);
    assert_eq!(jobs(&w).await, 0);
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM erp_job_run WHERE kind = $1 AND outcome = 'cancelled'",
            &[&SEND_KIND]
        )
        .await,
        1
    );
    make_due(&w).await;
    assert_eq!(run_all(&w).await, 0);
    assert_eq!(messages_on(&w, w.t1).await, 0);
    // an unknown id
    let error = cancel(&w, w.ann.0, 987_654).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::Missing,
        "The scheduled message 987654 does not exist.",
    );
    // a row removed behind the queue's back (here with SQL) leaves its job, which finishes
    // without posting
    let orphan = schedule(&w, w.ann.0, request(&w, w.t1, "gone", now + HOUR))
        .await
        .unwrap();
    exec(
        &w.client,
        "DELETE FROM mail_scheduled_message WHERE id = $1",
        &[&orphan],
    )
    .await;
    assert_eq!(jobs(&w).await, 1);
    make_due(&w).await;
    assert_eq!(run_all(&w).await, 1);
    assert_eq!(jobs(&w).await, 0);
    assert_eq!(messages_on(&w, w.t1).await, 0);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_job_posts_at_the_due_time_once_even_with_two_runners() {
    let w = world().await;
    let now = db_now(&w).await;
    let mut message = request(&w, w.t1, "on time", now + HOUR);
    message.subject = Some("Right on time".to_owned());
    message.partner_ids = vec![w.bob.1];
    let id = schedule(&w, w.ann.0, message).await.unwrap();
    // enqueueing the same generation again yields the one job (the key is the idempotency)
    {
        let mut tx = begin(&w, w.ann.0).await;
        let env = tx.env();
        let again = EnqueueRequest::new(SEND_KIND)
            .payload(&json!({"id": id, "seq": 1}))
            .unwrap()
            .at(When::After(std::time::Duration::from_secs(3600)))
            .dedup(JobKey::new(&format!("{id}:1")).unwrap(), OnDuplicate::Keep);
        let outcome = tx.enqueue(&env, again).await.unwrap();
        assert!(!outcome.is_new());
        tx.commit().await.unwrap();
    }
    assert_eq!(jobs(&w).await, 1);
    // before the date nothing is claimed and nothing is posted
    let (first, second) = (runner(&w), runner(&w));
    let (a, b) = tokio::join!(first.tick(), second.tick());
    assert_eq!(a.unwrap().claimed + b.unwrap().claimed, 0);
    assert_eq!(messages_on(&w, w.t1).await, 0);
    // at the date both runners race for it: one claims it, one message exists
    make_due(&w).await;
    let (a, b) = tokio::join!(first.tick(), second.tick());
    assert_eq!(a.unwrap().claimed + b.unwrap().claimed, 1);
    assert_eq!(messages_on(&w, w.t1).await, 1);
    assert_eq!(rows(&w).await, 0);
    assert_eq!(jobs(&w).await, 0);
    // the message is the creator's, with the row's text, recipient and subtype
    let posted = w
        .client
        .query_one(
            "SELECT m.author_id::int8, m.body, m.subject, m.subtype_id::int8, \
             ARRAY(SELECT r.res_partner_id::int8 FROM mail_message_res_partner_rel r \
                   WHERE r.mail_message_id = m.id ORDER BY 1) \
             FROM mail_message m WHERE m.model = $1 AND m.res_id = $2",
            &[&TICKET, &w.t1],
        )
        .await
        .unwrap();
    assert_eq!(posted.get::<_, i64>(0), w.ann.1);
    assert_eq!(posted.get::<_, String>(1), "<p>on time</p>");
    assert_eq!(posted.get::<_, String>(2), "Right on time");
    assert_eq!(posted.get::<_, i64>(3), w.comment);
    assert_eq!(posted.get::<_, Vec<i64>>(4), vec![w.bob.1]);
    let _ = id;
    w.h.cleanup().await.unwrap();
}

/// The dead letter is simulated with SQL: the queue's own retry and dead-letter path is tested by
/// the jobs tests.
#[tokio::test]
async fn a_failure_keeps_the_draft_a_dead_letter_shows_on_the_record_and_a_retry_works() {
    let w = world().await;
    let now = db_now(&w).await;
    let failing = schedule(&w, w.ann.0, request(&w, w.t1, "fails", now + HOUR))
        .await
        .unwrap();
    let sibling = schedule(&w, w.bob.0, request(&w, w.t2, "works", now + HOUR))
        .await
        .unwrap();
    let dead = schedule(&w, w.ann.0, request(&w, w.t2, "stuck job", now + 2 * HOUR))
        .await
        .unwrap();
    let failing_again = schedule(&w, w.ann.0, request(&w, w.t1, "fails too", now + HOUR))
        .await
        .unwrap();
    lock_ticket(&w, w.t1).await;
    make_due(&w).await;
    exec(
        &w.client,
        "UPDATE erp_job SET run_after = clock_timestamp() + interval '1 day' WHERE dedup_key = $1",
        &[&format!("{dead}:1")],
    )
    .await;
    assert_eq!(run_all(&w).await, 3);
    assert_eq!(messages_on(&w, w.t1).await, 0);
    assert_eq!(messages_on(&w, w.t2).await, 1);
    // the failed drafts are listed as failed with their reason; the sent sibling is gone
    unlock_ticket(&w, w.t1).await;
    let answer = fetch(&w, w.ann.0, TICKET, w.t1).await.unwrap();
    assert_eq!(
        ids_of(&answer),
        vec![
            u64::try_from(failing).unwrap(),
            u64::try_from(failing_again).unwrap()
        ]
    );
    let shown = record(&answer, u64::try_from(failing).unwrap());
    assert_eq!(shown["state"], "failed");
    // a fixed text for the kind of error: the raw access error is not shown to the record's writers
    assert_eq!(shown["failure_reason"], FAILED_ACCESS);
    // a message whose job is a dead letter shows as failed too, naming the job's error code
    exec(
        &w.client,
        "UPDATE erp_job SET state = 'dead', dead_at = clock_timestamp(), last_error_code = 'boom' \
         WHERE dedup_key = $1",
        &[&format!("{dead}:1")],
    )
    .await;
    let answer = fetch(&w, w.ann.0, TICKET, w.t2).await.unwrap();
    assert_eq!(ids_of(&answer), vec![u64::try_from(dead).unwrap()]);
    let shown = record(&answer, u64::try_from(dead).unwrap());
    assert_eq!(shown["state"], "failed");
    assert_eq!(
        shown["failure_reason"],
        "The delivery job failed (boom); an administrator can requeue it."
    );
    // a revoked sender is worded by its code: it is not the operator's to requeue
    exec(
        &w.client,
        "UPDATE erp_job SET last_error_code = 'principal_revoked' WHERE dedup_key = $1",
        &[&format!("{dead}:1")],
    )
    .await;
    let answer = fetch(&w, w.ann.0, TICKET, w.t2).await.unwrap();
    assert_eq!(
        record(&answer, u64::try_from(dead).unwrap())["failure_reason"],
        "The sender can no longer send: the account was archived or lost its access. Edit the message with a new date, or cancel it."
    );
    let _ = sibling;
    // retry one: sending the failed draft now (the creator may post again)
    let message = send_now(&w, w.ann.0, failing).await.unwrap();
    assert!(message > 0);
    assert_eq!(messages_on(&w, w.t1).await, 1);
    // retry the dead one: a new date re-arms it; the dead job was the old generation
    let mut patch = EditRequest::new();
    patch.scheduled_date = Some(at(now + 3 * HOUR));
    edit(&w, w.ann.0, dead, patch).await.unwrap();
    let answer = fetch(&w, w.ann.0, TICKET, w.t2).await.unwrap();
    assert_eq!(
        record(&answer, u64::try_from(dead).unwrap())["state"],
        "scheduled"
    );
    // retry the second failed draft: a new date returns it to scheduled and clears the reason
    let mut patch = EditRequest::new();
    patch.scheduled_date = Some(at(now + 4 * HOUR));
    edit(&w, w.ann.0, failing_again, patch).await.unwrap();
    let row = w
        .client
        .query_one(
            "SELECT state, failure_reason, arm_seq::int8 FROM mail_scheduled_message WHERE id = $1",
            &[&failing_again],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "scheduled");
    assert_eq!(row.get::<_, Option<String>>(1), None);
    assert_eq!(row.get::<_, i64>(2), 2);
    // a row whose job is gone from the queue is never stuck (3)
    a_row_without_a_delivery_job_is_never_stuck(&w, now).await;
    w.h.cleanup().await.unwrap();
}

/// Part: the dead letter expired, or an administrator discarded the job, so the
/// `scheduled` row has no job at all. The listing shows it as failed with a fixed reason, and
/// `cancel`, `send_now` and `edit` all still work (a `warn`, no `Invariant`). A job held by an
/// in-flight claim is a race instead: the cancel gets 40001 (retryable), never `Invariant`.
async fn a_row_without_a_delivery_job_is_never_stuck(w: &World, now: i64) {
    const NO_JOB: &str = "The message has no delivery job (it expired or was discarded in the queue). Edit it with a new date, send it now or cancel it.";
    let t3 = one(
        &w.client,
        "INSERT INTO test_sched_ticket (name) VALUES ('three') RETURNING id::int8",
        &[],
    )
    .await;
    let mut ids = Vec::new();
    for body in ["cancelled", "re-armed", "sent now"] {
        ids.push(
            schedule(w, w.ann.0, request(w, t3, body, now + HOUR))
                .await
                .unwrap(),
        );
    }
    // a dead letter that then expired, and jobs deleted outright: no job of the kind is left
    exec(
        &w.client,
        "UPDATE erp_job SET state = 'dead', dead_at = clock_timestamp(), last_error_code = 'boom' \
         WHERE dedup_key = $1",
        &[&format!("{}:1", ids[0])],
    )
    .await;
    exec(
        &w.client,
        "DELETE FROM erp_job WHERE kind = $1 AND dedup_key = ANY($2)",
        &[
            &SEND_KIND,
            &ids.iter().map(|id| format!("{id}:1")).collect::<Vec<_>>(),
        ],
    )
    .await;
    // the listing shows each as failed with the fixed reason
    let answer = fetch(w, w.ann.0, TICKET, t3).await.unwrap();
    for id in &ids {
        let shown = record(&answer, u64::try_from(*id).unwrap());
        assert_eq!(shown["state"], "failed");
        assert_eq!(shown["failure_reason"], NO_JOB);
    }
    // cancel succeeds (it used to fail with Invariant and leave the row for ever)
    cancel(w, w.bob.0, ids[0]).await.unwrap();
    // an edit with a date re-arms it: a new job, listed as scheduled again
    let mut patch = EditRequest::new();
    patch.scheduled_date = Some(at(now + 2 * HOUR));
    edit(w, w.ann.0, ids[1], patch).await.unwrap();
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM erp_job WHERE kind = $1 AND dedup_key = $2",
            &[&SEND_KIND, &format!("{}:2", ids[1])]
        )
        .await,
        1
    );
    let answer = fetch(w, w.ann.0, TICKET, t3).await.unwrap();
    assert_eq!(
        record(&answer, u64::try_from(ids[1]).unwrap())["state"],
        "scheduled"
    );
    // send now still posts
    send_now(w, w.ann.0, ids[2]).await.unwrap();
    assert_eq!(messages_on(w, t3).await, 1);
    // a job another connection holds (a claim in flight) is a race, not a defect
    let racing = schedule(w, w.ann.0, request(w, t3, "racing", now + HOUR))
        .await
        .unwrap();
    let other = connect(&w.h).await;
    other.batch_execute("BEGIN").await.unwrap();
    other
        .query(
            "SELECT 1 FROM erp_job WHERE kind = $1 AND dedup_key = $2 FOR UPDATE",
            &[&SEND_KIND, &format!("{racing}:1")],
        )
        .await
        .unwrap();
    let error = cancel(w, w.ann.0, racing).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency, "{error:?}");
    assert_eq!(error.sqlstate(), Some("40001"));
    assert_eq!(
        error.message(),
        "could not serialize access: the delivery job is held by a claim in flight"
    );
    other.batch_execute("ROLLBACK").await.unwrap();
    // once the claim is gone the repeat cancels the job and the row
    cancel(w, w.ann.0, racing).await.unwrap();
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM erp_job WHERE kind = $1 AND dedup_key = $2",
            &[&SEND_KIND, &format!("{racing}:1")]
        )
        .await,
        0
    );
}

#[tokio::test]
async fn the_topic_lists_in_order_denies_in_its_slot_and_refuses_over_the_bound() {
    let w = world().await;
    let now = db_now(&w).await;
    let late = schedule(&w, w.ann.0, request(&w, w.t1, "late", now + 3 * HOUR))
        .await
        .unwrap();
    let early = schedule(&w, w.bob.0, request(&w, w.t1, "early", now + HOUR))
        .await
        .unwrap();
    let mut middle = request(&w, w.t1, "middle", now + 2 * HOUR);
    middle.partner_ids = vec![w.bob.1];
    middle.is_note = true;
    let middle = schedule(&w, w.ann.0, middle).await.unwrap();
    // ordered by date, whoever created them
    let answer = fetch(&w, w.ann.0, TICKET, w.t1).await.unwrap();
    assert_eq!(answer["topics"][0]["ok"], true);
    let order: Vec<u64> = [early, middle, late]
        .iter()
        .map(|id| u64::try_from(*id).unwrap())
        .collect();
    assert_eq!(ids_of(&answer), order);
    assert_eq!(answer["topics"][0]["data"]["model"], TICKET);
    let shown = record(&answer, order[1]);
    assert_eq!(shown["body"], "middle");
    assert_eq!(shown["is_note"], true);
    assert_eq!(shown["state"], "scheduled");
    assert_eq!(shown["author_id"], u64::try_from(w.ann.1).unwrap());
    assert_eq!(shown["partner_ids"], json!([w.bob.1]));
    // a user who may not post on the record is denied in the topic's own slot
    lock_ticket(&w, w.t1).await;
    let answer = fetch(&w, w.ann.0, TICKET, w.t1).await.unwrap();
    assert_eq!(answer["topics"][0]["ok"], false);
    assert_eq!(answer["topics"][0]["error"]["kind"], "access");
    assert_eq!(answer["topics"][0]["error"]["message"], RECORD_DENIED);
    unlock_ticket(&w, w.t1).await;
    // a portal user is denied by the topic's audience
    let answer = fetch(&w, w.por.0, TICKET, w.t1).await.unwrap();
    assert_eq!(answer["topics"][0]["ok"], false);
    assert_eq!(
        answer["topics"][0]["error"]["message"],
        "You are not allowed to fetch the store topic 'thread.scheduled'."
    );
    // over the bound the whole call fails with the typed error: the database's CHECK on the slot
    // keeps a 101st row out, so a database from before the CHECK is simulated by dropping it
    let check = text_of(
        &w.client,
        "SELECT conname::text FROM pg_constraint WHERE conrelid = 'mail_scheduled_message'::regclass \
         AND contype = 'c' AND pg_get_constraintdef(oid) LIKE '%slot%'",
        &[],
    )
    .await;
    exec(
        &w.client,
        &format!("ALTER TABLE mail_scheduled_message DROP CONSTRAINT \"{check}\""),
        &[],
    )
    .await;
    exec(
        &w.client,
        "INSERT INTO mail_scheduled_message (model, res_id, body, scheduled_date, author_id, \
         state, arm_seq, is_note, autofollow_recipients, slot) \
         SELECT $1, $2, 'bulk', now() AT TIME ZONE 'UTC' + interval '1 day', $3, 'scheduled', 1, \
         false, false, g FROM generate_series(1, $4::int4) g",
        &[
            &TICKET,
            &w.t2,
            &w.ann.1,
            &(i32::try_from(MAX_SCHEDULED_PER_THREAD).unwrap() + 1),
        ],
    )
    .await;
    let error = fetch(&w, w.ann.0, TICKET, w.t2).await.unwrap_err();
    assert_err(
        &error,
        ErrorKind::UserError,
        "Cannot list scheduled messages, too many are scheduled on this record (more than 100).",
    );
    // and one record's rows never leak into another's list
    let answer = fetch(&w, w.ann.0, TICKET, w.t1).await.unwrap();
    assert_eq!(ids_of(&answer).len(), 3);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deleting_the_thread_deletes_its_scheduled_messages_and_only_administrators_have_table_rows()
 {
    let w = world().await;
    let now = db_now(&w).await;
    // default data: the model ships no record, and the manifest names no data file for it
    assert_eq!(rows(&w).await, 0);
    assert!(!MANIFEST.contains("scheduled"), "{MANIFEST}");
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM ir_cron c \
             JOIN ir_act_server s ON s.id = c.ir_actions_server_id \
             JOIN ir_model m ON m.id = s.model_id \
             WHERE m.model = 'mail.scheduled.message' OR s.code LIKE '%_post_messages_cron%'",
            &[]
        )
        .await,
        0
    );
    // the shipped access rows: one, crud, administrators
    let mut found = Vec::new();
    for line in ACCESS_CSV.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f = csv_fields(line);
        if f[2] == "model_mail_scheduled_message" {
            found.push((f[3].clone(), f[4].clone(), f[5].clone()));
        }
    }
    assert_eq!(
        found,
        vec![(
            "base.group_system".to_owned(),
            "crud".to_owned(),
            String::new()
        )]
    );
    // an internal user cannot read or search the table directly
    let mut tx = begin(&w, w.ann.0).await;
    let error = tx
        .env()
        .records(MODEL)
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    drop(tx);
    // deleting the record deletes its scheduled messages, and only its own
    schedule(&w, w.ann.0, request(&w, w.t1, "on one", now + HOUR))
        .await
        .unwrap();
    for body in ["two a", "two b", "two c"] {
        schedule(&w, w.ann.0, request(&w, w.t2, body, now + HOUR))
            .await
            .unwrap();
    }
    assert_eq!(rows(&w).await, 4);
    assert_eq!(jobs(&w).await, 4);
    let mut tx = begin_su(&w).await;
    tx.env()
        .records(TICKET)
        .unwrap()
        .browse([w.t2])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(rows(&w).await, 1);
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM mail_scheduled_message WHERE res_id = $1",
            &[&w.t1]
        )
        .await,
        1
    );
    // the jobs of the deleted record's messages went with them: only the other record's is live
    assert_eq!(jobs(&w).await, 1);
    make_due(&w).await;
    assert_eq!(run_all(&w).await, 1);
    assert_eq!(messages_on(&w, w.t2).await, 0);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_addon_extends_the_created_hook_for_the_job_and_for_send_now() {
    let w = world().await;
    let now = db_now(&w).await;
    schedule(&w, w.ann.0, request(&w, w.t1, "by job", now + HOUR))
        .await
        .unwrap();
    make_due(&w).await;
    assert_eq!(run_all(&w).await, 1);
    let posted = one(
        &w.client,
        "SELECT id::int8 FROM mail_message WHERE model = $1 AND res_id = $2",
        &[&TICKET, &w.t1],
    )
    .await;
    assert_eq!(
        text_of(
            &w.client,
            "SELECT remark FROM test_sched_ticket WHERE id = $1",
            &[&w.t1]
        )
        .await,
        format!("hook:{posted}")
    );
    let id = schedule(&w, w.ann.0, request(&w, w.t2, "by hand", now + HOUR))
        .await
        .unwrap();
    let message = send_now(&w, w.ann.0, id).await.unwrap();
    assert_eq!(
        text_of(
            &w.client,
            "SELECT remark FROM test_sched_ticket WHERE id = $1",
            &[&w.t2]
        )
        .await,
        format!("hook:{message}")
    );
    w.h.cleanup().await.unwrap();
}

/// Functional ceilings that catch a statement per partner or per row, not
/// performance claims; they may only go down.
const SCHEDULE_CEILING: u64 = 7;
const EDIT_CEILING: u64 = 10;
const SEND_NOW_CEILING: u64 = 16;
const CANCEL_CEILING: u64 = 8;

/// The statements `schedule` sends for `partners` recipients.
async fn schedule_statements(w: &World, partners: Vec<i64>) -> (i64, u64) {
    let now = db_now(w).await;
    let mut message = request(w, w.t1, "counted", now + HOUR);
    message.partner_ids = partners;
    let mut tx = begin(w, w.ann.0).await;
    let before = tx.query_count();
    let records = tx.env().records(MODEL).unwrap();
    let id: i64 = records.call(&mut tx, "schedule", (message,)).await.unwrap();
    tx.flush().await.unwrap();
    let used = tx.query_count() - before;
    tx.commit().await.unwrap();
    (id, used)
}

#[tokio::test]
async fn the_methods_send_a_bounded_number_of_statements() {
    let w = world().await;
    let now = db_now(&w).await;
    // the first call of a transaction family loads what it needs once: warm up, then count
    schedule_statements(&w, vec![]).await;
    // the count does not grow with the recipients
    let (id, few) = schedule_statements(&w, vec![w.bob.1, w.adm.1]).await;
    let crowd: Vec<i64> = {
        let mut ids = Vec::new();
        for n in 0..20 {
            ids.push(
                one(
                    &w.client,
                    "INSERT INTO res_partner (name, active) VALUES ($1, true) RETURNING id::int8",
                    &[&format!("recipient {n}")],
                )
                .await,
            );
        }
        ids
    };
    let (_, many) = schedule_statements(&w, crowd).await;
    eprintln!("MEASURED schedule {few} (20 recipients: {many})");
    assert_eq!(few, many);
    assert!(few <= SCHEDULE_CEILING, "schedule used {few}");
    let mut tx = begin(&w, w.ann.0).await;
    let before = tx.query_count();
    let row = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    let mut patch = EditRequest::new();
    patch.scheduled_date = Some(at(now + 2 * HOUR));
    patch.partner_ids = Some(vec![w.bob.1]);
    row.call::<_, ()>(&mut tx, "edit", (patch,)).await.unwrap();
    tx.flush().await.unwrap();
    let used = tx.query_count() - before;
    tx.commit().await.unwrap();
    eprintln!("MEASURED edit {used}");
    assert!(used <= EDIT_CEILING, "edit used {used}");
    let mut tx = begin(&w, w.ann.0).await;
    let before = tx.query_count();
    let row = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    let _: i64 = row.call(&mut tx, "send_now", ()).await.unwrap();
    tx.flush().await.unwrap();
    let used = tx.query_count() - before;
    tx.commit().await.unwrap();
    eprintln!("MEASURED send_now {used}");
    assert!(used <= SEND_NOW_CEILING, "send_now used {used}");
    let id = schedule(&w, w.ann.0, request(&w, w.t1, "cancelled", now + HOUR))
        .await
        .unwrap();
    let mut tx = begin(&w, w.bob.0).await;
    let before = tx.query_count();
    let row = tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    row.call::<_, ()>(&mut tx, "cancel", ()).await.unwrap();
    tx.flush().await.unwrap();
    let used = tx.query_count() - before;
    tx.commit().await.unwrap();
    eprintln!("MEASURED cancel {used}");
    assert!(used <= CANCEL_CEILING, "cancel used {used}");
    w.h.cleanup().await.unwrap();
}

async fn write_access_on_the_record_decides_not_the_post_access(w: &World, now: i64) {
    let base_rows = rows(w).await;
    let on_partner = |when: i64| ScheduleRequest::new("res.partner", w.p1, "hello", at(when));
    // the read-only internal user is refused with the engine's write refusal
    let error = schedule(w, w.ann.0, on_partner(now + HOUR))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert!(error.message().contains("write"), "{}", error.message());
    // the portal user is refused as a non-internal user, whatever the partner's rules say
    let error = schedule(w, w.por.0, on_partner(now + HOUR))
        .await
        .unwrap_err();
    assert_err(&error, ErrorKind::Access, INTERNAL_ONLY);
    assert_eq!(rows(w).await, base_rows);
    // the user who may write the partner schedules; another internal user who may only read
    // it cannot list or cancel
    let id = schedule(w, w.ned.0, on_partner(now + HOUR)).await.unwrap();
    let denied = fetch(w, w.ann.0, "res.partner", w.p1).await.unwrap();
    assert_eq!(denied["topics"][0]["ok"], false);
    assert_eq!(denied["topics"][0]["error"]["kind"], "access");
    assert_eq!(denied["topics"][0]["error"]["message"], RECORD_DENIED);
    let error = cancel(w, w.ann.0, id).await.unwrap_err();
    assert_err(&error, ErrorKind::Access, RECORD_DENIED);
    assert_eq!(rows(w).await, base_rows + 1);
    let listed = fetch(w, w.ned.0, "res.partner", w.p1).await.unwrap();
    assert_eq!(ids_of(&listed), vec![u64::try_from(id).unwrap()]);
    // and a missing record is the same denial slot, not a whole-call Missing
    let gone = fetch(w, w.ned.0, "res.partner", 987_654).await.unwrap();
    assert_eq!(gone["topics"][0]["ok"], false);
    assert_eq!(gone["topics"][0]["error"]["message"], RECORD_DENIED);
    cancel(w, w.ned.0, id).await.unwrap();
    assert_eq!(rows(w).await, base_rows);
}

/// Part: creation and the delivery fields belong to `schedule`, `edit` and the
/// job: a direct ORM create or write is refused, with the data checked first, so a past date is
/// refused on every path.
async fn a_direct_orm_create_or_write_of_the_delivery_fields_is_refused(w: &World, now: i64) {
    let (rows0, jobs0) = (rows(w).await, jobs(w).await);
    let id = schedule(w, w.ann.0, request(w, w.t1, "kept", now + HOUR))
        .await
        .unwrap();
    let vals = |date: i64| {
        RawVals::new()
            .set_cell("model", text(TICKET))
            .set_cell("res_id", Cell::Id(w.t1))
            .set_cell("scheduled_date", Cell::DateTime(at(date)))
            .set_cell("author_id", Cell::Id(w.ann.1))
            .set_cell("slot", Cell::Int(50))
    };
    // a create is refused, even as the superuser
    let mut tx = begin_su(w).await;
    let records = tx.env().records(MODEL).unwrap();
    let error = records
        .create(&mut tx, vec![vals(now + HOUR)])
        .await
        .unwrap_err();
    assert_err(&error, ErrorKind::UserError, CREATE_REFUSED);
    // with a past date the data is refused first, as on every other path
    let error = records
        .create(&mut tx, vec![vals(now - HOUR)])
        .await
        .unwrap_err();
    assert_err(&error, ErrorKind::Validation, PAST);
    // and with a model that is not a mail thread
    let error = records
        .create(
            &mut tx,
            vec![vals(now + HOUR).set_cell("model", text("res.currency"))],
        )
        .await
        .unwrap_err();
    assert_err(&error, ErrorKind::Validation, NOT_THREAD);
    drop(tx);
    // a write of any delivery field is refused; a past date is refused as a date
    let row = |tx: &Transaction| tx.env().records(MODEL).unwrap().browse([id]).unwrap();
    for (field, cell) in [
        ("scheduled_date", Cell::DateTime(at(now + 2 * HOUR))),
        ("arm_seq", Cell::Int(9)),
        ("state", text("failed")),
        ("failure_reason", text("made up")),
    ] {
        let mut tx = begin_su(w).await;
        let error = row(&tx)
            .write(&mut tx, RawVals::new().set_cell(field, cell))
            .await
            .unwrap_err();
        assert_err(&error, ErrorKind::UserError, DELIVERY_REFUSED);
    }
    let mut tx = begin_su(w).await;
    let error = row(&tx)
        .write(
            &mut tx,
            RawVals::new().set_cell("scheduled_date", Cell::DateTime(at(now - HOUR))),
        )
        .await
        .unwrap_err();
    assert_err(&error, ErrorKind::Validation, PAST);
    drop(tx);
    // the text fields are not delivery fields: an administrator's ORM write of them is allowed
    let mut tx = begin_su(w).await;
    row(&tx)
        .write(&mut tx, RawVals::new().set_cell("body", text("corrected")))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        text_of(
            &w.client,
            "SELECT body FROM mail_scheduled_message WHERE id = $1",
            &[&id]
        )
        .await,
        "corrected"
    );
    // nothing changed under the delivery fields, and the one job is still there
    let state = w
        .client
        .query_one(
            "SELECT state, arm_seq::int8, scheduled_date > now() AT TIME ZONE 'UTC' \
             FROM mail_scheduled_message WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(state.get::<_, String>(0), "scheduled");
    assert_eq!(state.get::<_, i64>(1), 1);
    assert!(state.get::<_, bool>(2));
    assert_eq!(rows(w).await, rows0 + 1);
    assert_eq!(jobs(w).await, jobs0 + 1);
    // leave the world as the caller found it
    cancel(w, w.ann.0, id).await.unwrap();
}

/// The delivery job of an older generation does nothing (outcome `superseded`): the row is
/// advanced behind the queue's back with SQL, which no method does, to reach the guard.
#[tokio::test]
async fn a_job_of_an_older_generation_posts_nothing() {
    let w = world().await;
    let now = db_now(&w).await;
    let id = schedule(&w, w.ann.0, request(&w, w.t1, "old", now + HOUR))
        .await
        .unwrap();
    exec(
        &w.client,
        "UPDATE mail_scheduled_message SET arm_seq = 2 WHERE id = $1",
        &[&id],
    )
    .await;
    make_due(&w).await;
    assert_eq!(run_all(&w).await, 1);
    assert_eq!(messages_on(&w, w.t1).await, 0);
    assert_eq!(rows(&w).await, 1);
    assert_eq!(jobs(&w).await, 0);
    w.h.cleanup().await.unwrap();
}

/// The per-record bound holds across connections: two schedulers at 99 rows, one commits the
/// 100th, the other is refused with the bound (not a 101st row). The advisory lock keyed on the
/// record serializes them; the unique (record, slot) key catches the loser that read a count from
/// before the winner's commit (REPEATABLE READ), and the unit of work repeats it.
#[tokio::test]
async fn two_schedulers_at_the_bound_one_commits_and_the_other_is_refused() {
    let w = world().await;
    let now = db_now(&w).await;
    exec(
        &w.client,
        "INSERT INTO mail_scheduled_message (model, res_id, body, scheduled_date, author_id, \
         state, arm_seq, is_note, autofollow_recipients, slot) \
         SELECT $1, $2, 'bulk', now() AT TIME ZONE 'UTC' + interval '1 day', $3, 'scheduled', 1, \
         false, false, g FROM generate_series(1, 99) g",
        &[&TICKET, &w.t2, &w.ann.1],
    )
    .await;
    // the first scheduler takes the 100th place and keeps its transaction open
    let mut first = begin(&w, w.ann.0).await;
    let records = first.env().records(MODEL).unwrap();
    let id: i64 = records
        .call(
            &mut first,
            "schedule",
            (request(&w, w.t2, "100th", now + HOUR),),
        )
        .await
        .unwrap();
    // the second scheduler, on another connection, waits for the record's lock; the first commits
    // only once it is seen waiting
    let second = w.orm.run(
        UserId::new(w.bob.0).unwrap(),
        erp_orm::Context::new(),
        async |tx: &mut Transaction| -> Result<i64> {
            let records = tx.env().records(MODEL)?;
            records
                .call(tx, "schedule", (request(&w, w.t2, "101st", now + HOUR),))
                .await
        },
    );
    let commit_when_blocked = async {
        for _ in 0..200 {
            let waiting = one(
                &w.client,
                "SELECT count(*)::int8 FROM pg_stat_activity WHERE datname = current_database() \
                 AND wait_event_type = 'Lock' AND wait_event = 'advisory'",
                &[],
            )
            .await;
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        first.commit().await.unwrap();
    };
    let (refused, ()) = tokio::join!(second, commit_when_blocked);
    let error = refused.unwrap_err();
    assert_err(
        &error,
        ErrorKind::Validation,
        "At most 100 messages can be scheduled on one record, and this one has 100.",
    );
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM mail_scheduled_message WHERE res_id = $1",
            &[&w.t2]
        )
        .await,
        100
    );
    assert_eq!(
        one(
            &w.client,
            "SELECT count(*)::int8 FROM mail_scheduled_message WHERE id = $1 AND slot = 100",
            &[&id]
        )
        .await,
        1
    );
    w.h.cleanup().await.unwrap();
}

/// The lock class of the per-record bound is the one registered in `erp_sql::locks`, and differs
/// from every other class the repository registers.
#[test]
fn the_lock_class_is_registered_and_distinct() {
    assert_eq!(
        erp_addon_mail::scheduled::THREAD_LOCK_CLASS,
        erp_sql::locks::MAIL_SCHEDULED_THREAD_LOCK_CLASS
    );
    for other in [
        1701,
        1702,
        erp_sql::locks::JOBS_LOCK_CLASS,
        erp_orm::node::LOCK_CLASS,
    ] {
        assert_ne!(erp_addon_mail::scheduled::THREAD_LOCK_CLASS, other);
    }
}
