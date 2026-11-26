// SPDX-License-Identifier: MIT OR Apache-2.0
//! The record history in the chatter, as a property of the `mail.thread` mixin
//! (`addons/mail/src/tracking.rs`): a model that does nothing but inherit the mixin gets its
//! creation line, and a write of its `tracking = true` fields one history message per record,
//! written at commit by the mail addon's precommit hook. The overridable methods of the mixin
//! (`_creation_message`, `_track_body`) change what is written. The reader's field groups decide
//! which changes the thread shows.
//!
//! The test lives here because it needs the production policy (`erp-security`), which an addon
//! crate may not depend on (`scripts/check-layers.py`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    reason = "tests"
)]

mod company_rows_support;

use std::sync::Arc;

use company_rows_support::UserKind::Internal;
use company_rows_support::fixture_user_with_partner;
use erp_addon_mail::{MailThread, StoreFields};
use erp_fields::ErrorKind;
use erp_orm::{Orm, PolicyProvider, UserId};
use erp_sdk::prelude::*;
use erp_sdk::store_wire::FieldValue as Wire;
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls};

erp_sdk::addon!(name = "test_history", depends = ["mail"]);

/// A model that only inherits the mixin and marks fields tracked: no code of its own.
#[model(
    name = "test_history.note",
    description = "History Note",
    inherit = [MailThread],
    addon = "test_history"
)]
pub struct Note {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(
        selection,
        selection = [("new", "New"), ("done", "Done")],
        tracking = true,
        string = "Stage"
    )]
    pub stage: Option<Arc<str>>,
    #[field(many2one, comodel = "res.partner", tracking = true, string = "Contact")]
    pub partner_id: Option<RecId>,
    #[field(char, tracking = true, groups = "base.group_system", string = "Secret")]
    pub secret: Option<Arc<str>>,
    #[field(text, tracking = true, string = "Memo")]
    pub memo: Option<Arc<str>>,
}

/// The mixin and nothing else (its creation text is changed by the extension addon below).
#[model(
    name = "test_history.plain",
    description = "Plain Thread",
    inherit = [MailThread],
    addon = "test_history"
)]
pub struct Plain {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// An addon that extends the history through the mixin's methods, as an addon author would.
mod ext {
    use std::collections::BTreeMap;

    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_history_ext", depends = ["test_history"]);

    #[extend(super::Plain, addon = "test_history_ext")]
    pub struct PlainExt;

    #[methods(super::Plain, addon = "test_history_ext")]
    impl PlainExt {
        #[method(override)]
        async fn _creation_message(
            this: &RecordSet<super::Plain>,
            _tx: &mut Transaction,
            _sup: Super,
        ) -> Result<BTreeMap<i64, String>> {
            Ok(this
                .as_records()
                .ids()
                .into_iter()
                .map(|id| (id, "Plain thread opened".to_owned()))
                .collect())
        }
    }

    #[extend(super::Note, addon = "test_history_ext")]
    pub struct NoteExt;

    #[methods(super::Note, addon = "test_history_ext")]
    impl NoteExt {
        #[method(override)]
        async fn _track_body(
            _this: &RecordSet<super::Note>,
            _tx: &mut Transaction,
            changed: BTreeMap<i64, Vec<String>>,
            _sup: Super,
        ) -> Result<BTreeMap<i64, String>> {
            Ok(changed
                .into_iter()
                .filter(|(_, fields)| fields.iter().any(|f| f == "stage"))
                .map(|(id, _)| (id, "Stage changed".to_owned()))
                .collect())
        }
    }
}

const NOTE: &str = "test_history.note";
const PLAIN: &str = "test_history.plain";

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("mail_history: connection closed with an error: {error}");
        }
    });
    client
}

async fn one(
    client: &Client,
    sql: &str,
    params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)],
) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn group(client: &Client, name: &str, xmlid: &str) -> i64 {
    let id = one(
        client,
        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
        &[&name],
    )
    .await;
    xmlid_row(client, xmlid, "res.groups", id).await;
    id
}

async fn xmlid_row(client: &Client, xmlid: &str, model: &str, id: i64) {
    let (module, local) = xmlid.split_once('.').unwrap();
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) VALUES ($1, $2, $3, $4)",
            &[&module, &local, &model, &id],
        )
        .await
        .unwrap();
}

async fn crud(client: &Client, model: &str, group: i64, ops: &str) {
    let model_id = one(
        client,
        "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $1, 'base', false) \
         RETURNING id::int8",
        &[&model],
    )
    .await;
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, active) \
             VALUES ($1, $2, $3, $4, true)",
            &[&format!("{model} {ops}"), &model_id, &group, &ops],
        )
        .await
        .unwrap();
}

struct World {
    h: Harness,
    client: Client,
    orm: Orm,
    /// group_user only
    ivy: i64,
    ivy_partner: i64,
    /// group_user and group_system
    sam: i64,
    sam_partner: i64,
    note_subtype: i64,
}

async fn world() -> World {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_addon_mail::ADDON,
        &ADDON,
        &ext::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .build()
    .await
    .unwrap();
    let client = connect(&h).await;
    let g_user = group(&client, "Internal User", "base.group_user").await;
    let g_system = group(&client, "Administrator", "base.group_system").await;
    for (model, ops) in [
        (NOTE, "crud"),
        (PLAIN, "crud"),
        ("mail.message", "r"),
        ("mail.message.subtype", "r"),
        ("mail.followers", "r"),
        ("res.partner", "r"),
        ("res.users", "r"),
    ] {
        crud(&client, model, g_user, ops).await;
    }
    company_rows_support::insert_generated(
        &client,
        h.registry(),
        &company_rows_support::MAIL_POLICY_MODELS,
    )
    .await;
    let company = company_rows_support::fixture_company(&client).await;
    let (ivy, ivy_partner) =
        fixture_user_with_partner(&client, "ivy", &[g_user], Internal, company).await;
    let (sam, sam_partner) =
        fixture_user_with_partner(&client, "sam", &[g_user, g_system], Internal, company).await;
    let note_subtype = one(
        &client,
        "INSERT INTO mail_message_subtype (name, internal, \"default\", sequence) \
         VALUES (jsonb_build_object('en_US', 'Note'), true, false, 100) RETURNING id::int8",
        &[],
    )
    .await;
    xmlid_row(
        &client,
        "mail.mt_note",
        "mail.message.subtype",
        note_subtype,
    )
    .await;
    let pool = || erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let source = PolicySource::build(Arc::clone(h.registry()), pool())
        .await
        .unwrap();
    let orm = Orm::builder(Arc::clone(h.registry()), pool())
        .policy(Arc::new(source) as Arc<dyn PolicyProvider>)
        .resolver(Resolver::id_order())
        .precommit_hook(erp_addon_mail::TRACKING_HOOK)
        .build()
        .unwrap();
    World {
        h,
        client,
        orm,
        ivy,
        ivy_partner,
        sam,
        sam_partner,
        note_subtype,
    }
}

/// One stored message of a thread.
#[derive(Debug)]
struct Stored {
    message_type: String,
    body: String,
    author: Option<i64>,
    subtype: Option<i64>,
    internal: bool,
    tracking: Option<serde_json::Value>,
}

async fn thread(client: &Client, model: &str, id: i64) -> Vec<Stored> {
    client
        .query(
            "SELECT message_type, body, author_id::int8, subtype_id::int8, \
             COALESCE(is_internal, false), tracking::text FROM mail_message \
             WHERE model = $1 AND res_id = $2 ORDER BY id",
            &[&model, &id],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| Stored {
            message_type: row.get(0),
            body: row.get(1),
            author: row.get(2),
            subtype: row.get(3),
            internal: row.get(4),
            tracking: row
                .get::<_, Option<String>>(5)
                .map(|text| serde_json::from_str(&text).unwrap()),
        })
        .collect()
}

async fn create(tx: &mut Transaction, model: &str, vals: Vec<RawVals>) -> Vec<i64> {
    tx.env()
        .records(model)
        .unwrap()
        .create(tx, vals)
        .await
        .unwrap()
        .ids()
}

fn named(name: &str) -> RawVals {
    RawVals::new().set_cell("name", Cell::Str(name.into()))
}

async fn notes(w: &World, n: usize) -> Vec<i64> {
    let mut tx = w.orm.begin(UserId::new(w.ivy).unwrap()).await.unwrap();
    let vals = (0..n)
        .map(|i| named(&format!("note {i}")).set_cell("stage", Cell::Str("new".into())))
        .collect();
    let ids = create(&mut tx, NOTE, vals).await;
    tx.commit().await.unwrap();
    ids
}

async fn write_as(w: &World, uid: i64, ids: &[i64], vals: RawVals) {
    let mut tx = w.orm.begin(UserId::new(uid).unwrap()).await.unwrap();
    tx.env()
        .records(NOTE)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
        .write(&mut tx, vals)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn a_bare_thread_model_gets_its_creation_line() {
    let w = world().await;
    let ids = notes(&w, 1).await;
    let stored = thread(&w.client, NOTE, ids[0]).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    let line = &stored[0];
    assert_eq!(line.message_type, "notification");
    assert_eq!(
        line.body, "<p>History Note created</p>",
        "plain text, escaped and sanitized"
    );
    assert_eq!(
        line.author,
        Some(w.ivy_partner),
        "the creator is the author"
    );
    assert_eq!(line.subtype, Some(w.note_subtype));
    assert!(line.internal);
    assert!(
        line.tracking.is_none(),
        "a creation line carries no changes"
    );

    // the mixin's `_creation_message`, overridden by an addon, gives the text
    let mut tx = w.orm.begin(UserId::new(w.ivy).unwrap()).await.unwrap();
    let plain = create(&mut tx, PLAIN, vec![named("p")]).await;
    // created and deleted in one transaction: nothing is left to log
    let gone = create(&mut tx, PLAIN, vec![named("gone")]).await;
    tx.env()
        .records(PLAIN)
        .unwrap()
        .browse(gone.clone())
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    for key in ["mail_create_nolog", "tracking_disable"] {
        let quiet = tx
            .env()
            .records(PLAIN)
            .unwrap()
            .with_context([(key, Value::Bool(true))])
            .create(&mut tx, vec![named(key)])
            .await
            .unwrap()
            .ids();
        assert!(
            thread(&w.client, PLAIN, quiet[0]).await.is_empty(),
            "{key} before commit"
        );
        tx.commit().await.unwrap();
        assert!(thread(&w.client, PLAIN, quiet[0]).await.is_empty(), "{key}");
        tx = w.orm.begin(UserId::new(w.ivy).unwrap()).await.unwrap();
    }
    drop(tx);
    let stored = thread(&w.client, PLAIN, plain[0]).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].body, "<p>Plain thread opened</p>");
    assert!(thread(&w.client, PLAIN, gone[0]).await.is_empty());
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_tracked_write_gives_one_history_message_with_its_values_and_author() {
    let w = world().await;
    let ids = notes(&w, 1).await;
    let vals = RawVals::new()
        .set_cell("stage", Cell::Str("done".into()))
        .set_cell("partner_id", Cell::Id(w.sam_partner))
        .set_cell("name", Cell::Str("renamed".into()));
    write_as(&w, w.ivy, &ids, vals).await;
    let stored = thread(&w.client, NOTE, ids[0]).await;
    assert_eq!(
        stored.len(),
        2,
        "creation line and one history message: {stored:?}"
    );
    let history = &stored[1];
    assert_eq!(history.message_type, "tracking");
    assert_eq!(
        history.author,
        Some(w.ivy_partner),
        "the writer is the author"
    );
    assert_eq!(history.subtype, Some(w.note_subtype));
    assert!(history.internal);
    // the extension addon's `_track_body` explains a stage change
    assert_eq!(history.body, "<p>Stage changed</p>");
    let mut changes = history.tracking.clone().unwrap();
    changes
        .as_array_mut()
        .unwrap()
        .sort_by(|a, b| a["n"].as_str().cmp(&b["n"].as_str()));
    assert_eq!(
        Some(changes),
        Some(serde_json::json!([
            {"n": "partner_id", "l": "Contact", "t": "many2one", "g": null, "o": null,
             "v": w.sam_partner, "od": null, "vd": "sam"},
            {"n": "stage", "l": "Stage", "t": "selection", "g": null, "o": "new", "v": "done",
             "od": "New", "vd": "Done"},
        ])),
        "untracked `name` is not in the history"
    );
    // a write that changes no tracked field writes nothing
    write_as(&w, w.ivy, &ids, named("again")).await;
    assert_eq!(thread(&w.client, NOTE, ids[0]).await.len(), 2);
    w.h.cleanup().await.unwrap();
}

/// The messages of a write of many records are written with a fixed number of statements.
#[tokio::test]
async fn two_records_in_one_write_give_two_messages_with_a_fixed_statement_count() {
    let w = world().await;
    let ids = notes(&w, 5).await;
    let mut counts = Vec::new();
    for batch in [&ids[..2], &ids[2..]] {
        let mut tx = w.orm.begin(UserId::new(w.ivy).unwrap()).await.unwrap();
        tx.env()
            .records(NOTE)
            .unwrap()
            .browse(batch.iter().copied())
            .unwrap()
            .write(
                &mut tx,
                RawVals::new()
                    .set_cell("stage", Cell::Str("done".into()))
                    .set_cell("partner_id", Cell::Id(w.ivy_partner)),
            )
            .await
            .unwrap();
        tx.flush().await.unwrap();
        let before = tx.query_count();
        tx.commit().await.unwrap();
        counts.push(tx.query_count() - before);
        for id in batch {
            let stored = thread(&w.client, NOTE, *id).await;
            assert_eq!(stored.len(), 2, "note {id}: {stored:?}");
            assert_eq!(stored[1].message_type, "tracking");
        }
    }
    assert_eq!(
        counts[0], counts[1],
        "2 and 3 records: the commit runs the same statements {counts:?}"
    );
    w.h.cleanup().await.unwrap();
}

/// The `tracking` text the thread answer gives `uid` for `message`.
async fn shown(w: &World, uid: i64, message: i64) -> Option<serde_json::Value> {
    let mut tx = w.orm.begin(UserId::new(uid).unwrap()).await.unwrap();
    let fields: StoreFields = tx
        .env()
        .records("mail.message")
        .unwrap()
        .browse([message])
        .unwrap()
        .call(&mut tx, "_to_store", (StoreFields::new(),))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    match fields.get("mail.message", u64::try_from(message).unwrap(), "tracking") {
        Some(Wire::String(text)) => Some(serde_json::from_str(text).unwrap()),
        None => None,
        other => panic!("tracking is JSON text: {other:?}"),
    }
}

#[tokio::test]
async fn a_reader_outside_a_fields_groups_does_not_see_its_change() {
    let w = world().await;
    let ids = notes(&w, 1).await;
    let vals = RawVals::new()
        .set_cell("stage", Cell::Str("done".into()))
        .set_cell("secret", Cell::Str("salary".into()));
    write_as(&w, w.sam, &ids, vals).await;
    let message = one(
        &w.client,
        "SELECT id::int8 FROM mail_message WHERE model = $1 AND res_id = $2 \
         AND message_type = 'tracking'",
        &[&NOTE, &ids[0]],
    )
    .await;
    let names = |value: Option<serde_json::Value>| -> Vec<String> {
        value
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["n"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(shown(&w, w.sam, message).await), ["secret", "stage"]);
    assert_eq!(
        names(shown(&w, w.ivy, message).await),
        ["stage"],
        "the change of a group_system field is not shown to an internal user outside it"
    );
    // a write of the restricted field alone leaves ivy no line at all; sam keeps it
    write_as(
        &w,
        w.sam,
        &ids,
        RawVals::new().set_cell("secret", Cell::Str("bonus".into())),
    )
    .await;
    let only_secret = one(
        &w.client,
        "SELECT max(id)::int8 FROM mail_message WHERE model = $1 AND res_id = $2 \
         AND message_type = 'tracking'",
        &[&NOTE, &ids[0]],
    )
    .await;
    assert_ne!(only_secret, message);
    assert_eq!(names(shown(&w, w.sam, only_secret).await), ["secret"]);
    assert_eq!(shown(&w, w.ivy, only_secret).await, None);

    // the stored values are closed to every generic door, for ivy and for the administrator sam
    for uid in [w.ivy, w.sam] {
        let mut tx = w.orm.begin(UserId::new(uid).unwrap()).await.unwrap();
        let messages = tx.env().records("mail.message").unwrap();
        let read = messages
            .browse([message])
            .unwrap()
            .read_with(&mut tx, &["tracking"], erp_sdk::Load::Raw)
            .await
            .map(|_| ());
        refused(read, "read");
        let fetched = messages
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &erp_orm::SearchOptions::new().fetch_fields(["tracking"]),
            )
            .await
            .map(|_| ());
        refused(fetched, "search_read");
        let on_it = Domain::condition("tracking", Operator::Ne, false).unwrap();
        let searched = messages.search(&mut tx, &on_it).await.map(|_| ());
        refused(searched, "domain");
        tx.rollback().await.unwrap();
    }
    w.h.cleanup().await.unwrap();
}

/// `result` is the field-groups refusal of `mail.message.tracking`.
#[track_caller]
fn refused(result: Result<()>, door: &str) {
    let error = result.expect_err(door);
    assert_eq!(error.kind(), ErrorKind::Access, "{door}: {error:?}");
    assert!(
        error.message().starts_with(
            "You do not have enough rights to access the field \"tracking\" on Message \
             (mail.message)."
        ),
        "{door}: {}",
        error.message()
    );
}

/// The tracking value of the newest history message of note `id`, as stored.
async fn last_changes(w: &World, id: i64) -> (i64, serde_json::Value) {
    let row = w
        .client
        .query_one(
            "SELECT id::int8, tracking::text FROM mail_message WHERE model = $1 AND res_id = $2 \
             AND message_type = 'tracking' ORDER BY id DESC LIMIT 1",
            &[&NOTE, &id],
        )
        .await
        .unwrap();
    (
        row.get(0),
        serde_json::from_str(row.get::<_, &str>(1)).unwrap(),
    )
}

/// change of a restricted field stays restricted once the field is no longer declared; a stored
/// change without groups of a field the registry does not know is shown to nobody)
#[tokio::test]
async fn a_change_keeps_its_fields_groups_when_the_field_is_gone() {
    let w = world().await;
    let ids = notes(&w, 1).await;
    let vals = RawVals::new()
        .set_cell("stage", Cell::Str("done".into()))
        .set_cell("secret", Cell::Str("salary".into()));
    write_as(&w, w.sam, &ids, vals).await;
    let (message, stored) = last_changes(&w, ids[0]).await;
    let groups = |name: &str| {
        stored
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["n"] == name)
            .unwrap()["g"]
            .clone()
    };
    assert_eq!(groups("secret"), serde_json::json!("base.group_system"));
    assert_eq!(groups("stage"), serde_json::Value::Null);
    // the same changes as if the fields were gone from the registry: renamed to undeclared names,
    // plus a change written before groups were stored (no "g") and one of a declared field
    let gone = serde_json::json!([
        {"n": "x_gone_restricted", "l": "Gone", "t": "char", "g": "base.group_system",
         "o": null, "v": "salary"},
        {"n": "x_gone_open", "l": "Gone open", "t": "char", "g": null, "o": null, "v": "a"},
        {"n": "x_gone_unknown", "l": "Old", "t": "char", "o": null, "v": "b"},
        {"n": "secret", "l": "Secret", "t": "char", "o": null, "v": "c"},
    ]);
    w.client
        .execute(
            "UPDATE mail_message SET tracking = $1::text::jsonb WHERE id = $2",
            &[&gone.to_string(), &message],
        )
        .await
        .unwrap();
    let names = |value: Option<serde_json::Value>| -> Vec<String> {
        let changes = value.unwrap();
        assert!(
            changes
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c.get("g").is_none()),
            "the stored groups are not sent to the reader: {changes}"
        );
        changes
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["n"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(shown(&w, w.sam, message).await),
        ["x_gone_restricted", "x_gone_open", "secret"]
    );
    assert_eq!(
        names(shown(&w, w.ivy, message).await),
        ["x_gone_open"],
        "a gone group_system field stays closed to an internal user outside it"
    );
    w.h.cleanup().await.unwrap();
}

/// A tracked text value is stored up to the system parameter `mail.tracking_value_max_chars`
/// and cut there with a visible marker; a display text equal to the raw value is not stored again.
#[tokio::test]
async fn a_long_tracked_text_is_cut_at_the_bound_and_stored_once() {
    let w = world().await;
    w.client
        .execute(
            "INSERT INTO ir_config_parameter (key, value) \
             VALUES ('mail.tracking_value_max_chars', '100')",
            &[],
        )
        .await
        .unwrap();
    let ids = notes(&w, 1).await;
    // exactly at the bound: kept whole (multi-byte text, counted in characters)
    let at_bound: String = "é".repeat(100);
    write_as(
        &w,
        w.ivy,
        &ids,
        RawVals::new().set_cell("memo", Cell::Str(at_bound.as_str().into())),
    )
    .await;
    let (_, stored) = last_changes(&w, ids[0]).await;
    assert_eq!(
        stored,
        serde_json::json!([
            {"n": "memo", "l": "Memo", "t": "text", "g": null, "o": null, "v": at_bound,
             "od": null},
        ]),
        "the display text equal to the raw value is not stored a second time"
    );
    // one character over the bound: cut to the bound, with the marker
    let over: String = "x".repeat(101);
    write_as(
        &w,
        w.ivy,
        &ids,
        RawVals::new().set_cell("memo", Cell::Str(over.as_str().into())),
    )
    .await;
    let (_, stored) = last_changes(&w, ids[0]).await;
    let cut = format!("{}\u{2026}", "x".repeat(100));
    assert_eq!(
        stored,
        serde_json::json!([
            {"n": "memo", "l": "Memo", "t": "text", "g": null, "o": at_bound, "v": cut},
        ])
    );
    // a value outside the parameter's bounds is refused by name at the write that needs it
    w.client
        .execute(
            "UPDATE ir_config_parameter SET value = '5' WHERE key = 'mail.tracking_value_max_chars'",
            &[],
        )
        .await
        .unwrap();
    let mut tx = w.orm.begin(UserId::new(w.ivy).unwrap()).await.unwrap();
    tx.env()
        .records(NOTE)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("memo", Cell::Str("short".into())),
        )
        .await
        .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert!(
        error.message().contains("mail.tracking_value_max_chars"),
        "{}",
        error.message()
    );
    w.h.cleanup().await.unwrap();
}
