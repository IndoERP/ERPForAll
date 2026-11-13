//! read access to `mail.message` through the shipped rows of
//! `addons/mail/security/ir.access.csv` and the `thread_access` rule operator, on the production
//! policy (compiled by `erp-security` from `ir.access` rows, principal resolver, database).
//!
//! The test lives here and not under `addons/mail/tests` because an addon crate may not depend on
//! `erp-security` (`scripts/check-layers.py`, rule 1). The rows are the file's own: it is parsed
//! and inserted as the installer would, so the domains compile exactly as shipped.
//!
//! Cases: (document readers), 069 (direct recipients, notified partners), 072
//! (non-employees never see internal messages) on the read operation. 070 and 071 (write and
//! unlink) are the later write slice; they stay unclaimed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod company_rows_support;

use std::collections::BTreeMap;
use std::sync::Arc;

use company_rows_support::UserKind::{External, Internal};
use company_rows_support::fixture_user_with_partner;
use erp_addon_mail::MailThread;
use erp_orm::{Orm, PolicyProvider, UserId};
use erp_sdk::prelude::*;
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::tokio_postgres::{Client, NoTls};
use erp_testkit::{Harness, assert_query_count};

erp_sdk::addon!(name = "test_thread", depends = ["mail"]);

/// A thread model whose internal readers see every record but the secret one.
#[model(
    name = "test_thread.doc_a",
    description = "Thread Doc A",
    inherit = [MailThread],
    addon = "test_thread"
)]
pub struct DocA {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A thread model that appears only on later rows of a listing, readable by internal users only.
#[model(
    name = "test_thread.doc_b",
    description = "Thread Doc B",
    inherit = [MailThread],
    addon = "test_thread"
)]
pub struct DocB {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

const DOC_A: &str = "test_thread.doc_a";
const DOC_B: &str = "test_thread.doc_b";
const ACCESS_CSV: &str = include_str!("../../../addons/mail/security/ir.access.csv");

/// The fields of one CSV record (RFC 4180: double quotes around a field that has commas).
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
            eprintln!("mail_thread_access: connection closed with an error: {error}");
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

async fn exec(
    client: &Client,
    sql: &str,
    params: &[&(dyn erp_testkit::tokio_postgres::types::ToSql + Sync)],
) {
    client.execute(sql, params).await.unwrap();
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

struct World {
    h: Harness,
    orm: Orm,
    internal: i64,
    internal_partner: i64,
    other_partner: i64,
    portal: i64,
    portal_partner: i64,
    public: i64,
    /// message name -> id
    msgs: BTreeMap<&'static str, i64>,
}

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
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

fn text(s: &str) -> Cell {
    Cell::Str(s.into())
}

#[allow(clippy::too_many_lines, reason = "one fixture")]
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
    let groups: BTreeMap<&str, i64> = BTreeMap::from([
        ("base.group_user", g_user),
        ("base.group_portal", g_portal),
        ("base.group_public", g_public),
        ("base.group_system", g_system),
    ]);
    let models: BTreeMap<&str, (&str, i64)> = BTreeMap::from([
        (
            "model_mail_message",
            (
                "mail.message",
                model_row(&client, "mail.message", "Message").await,
            ),
        ),
        (
            "model_mail_message_subtype",
            (
                "mail.message.subtype",
                model_row(&client, "mail.message.subtype", "Subtype").await,
            ),
        ),
        (
            "model_mail_followers",
            (
                "mail.followers",
                model_row(&client, "mail.followers", "Followers").await,
            ),
        ),
        (
            "model_mail_scheduled_message",
            (
                "mail.scheduled.message",
                model_row(&client, "mail.scheduled.message", "Scheduled Message").await,
            ),
        ),
    ]);
    // the shipped rows, as the installer reads them
    let mut shipped = 0;
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
            models[model.as_str()].1,
            groups[group.as_str()],
            ops,
            domain,
        )
        .await;
        shipped += 1;
    }
    assert!(shipped >= 10, "the shipped rows were read: {shipped}");
    // the documents' own rules (the test's, not mail's)
    let doc_a = model_row(&client, DOC_A, "Thread Doc A").await;
    let doc_b = model_row(&client, DOC_B, "Thread Doc B").await;
    rule(
        &client,
        "internal read a",
        doc_a,
        g_user,
        "r",
        Some("[('name', '!=', 'secret')]"),
    )
    .await;
    rule(&client, "internal read b", doc_b, g_user, "r", None).await;
    // installation emits the generated company restrictions; this database has no installation
    company_rows_support::insert_generated(
        &client,
        h.registry(),
        &company_rows_support::PARTNER_AND_USERS,
    )
    .await;
    rule(
        &client,
        "portal read a",
        doc_a,
        g_portal,
        "r",
        Some("[('name', '=', 'shared')]"),
    )
    .await;
    rule(
        &client,
        "public read a",
        doc_a,
        g_public,
        "r",
        Some("[('name', '=', 'shared')]"),
    )
    .await;
    let company = company_rows_support::fixture_company(&client).await;
    let (public, _) =
        fixture_user_with_partner(&client, "pub", &[g_public], External, company).await;
    let (internal, internal_partner) =
        fixture_user_with_partner(&client, "ivy", &[g_user], Internal, company).await;
    let (_, other_partner) =
        fixture_user_with_partner(&client, "otto", &[g_user], Internal, company).await;
    let (portal, portal_partner) =
        fixture_user_with_partner(&client, "pat", &[g_portal], External, company).await;

    let source = PolicySource::build(Arc::clone(h.registry()), pool(&h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    let orm = Orm::builder(Arc::clone(h.registry()), pool(&h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap();

    // data, as the superuser
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let shared = create(
        &mut tx,
        DOC_A,
        RawVals::new().set_cell("name", text("shared")),
    )
    .await;
    let secret = create(
        &mut tx,
        DOC_A,
        RawVals::new().set_cell("name", text("secret")),
    )
    .await;
    let b1 = create(
        &mut tx,
        DOC_B,
        RawVals::new().set_cell("name", text("b one")),
    )
    .await;
    let subtype = create(
        &mut tx,
        "mail.message.subtype",
        RawVals::new().set_cell("name", text("Note")),
    )
    .await;
    let mut msgs = BTreeMap::new();
    let post = |name: &'static str,
                model: &'static str,
                res_id: i64,
                internal: bool,
                author: Option<i64>,
                to: &[i64],
                notified: &[i64]| {
        let mut vals = RawVals::new()
            .set_cell("model", text(model))
            .set_cell("res_id", Cell::Id(res_id))
            .set_cell("message_type", text("comment"))
            .set_cell("subtype_id", Cell::Id(subtype))
            .set_cell("is_internal", Cell::Bool(internal));
        if let Some(author) = author {
            vals = vals.set_cell("author_id", Cell::Id(author));
        }
        let ids = |ps: &[i64]| -> Vec<RecId> {
            ps.iter().map(|p| RecId::from_real(*p).unwrap()).collect()
        };
        vals = vals
            .set_commands("partner_ids", [Command::set(ids(to))])
            .set_commands("notified_partner_ids", [Command::set(ids(notified))]);
        (name, vals)
    };
    let planned = [
        // on a readable document
        post("on_shared", DOC_A, shared, false, None, &[], &[]),
        post("on_shared_internal", DOC_A, shared, true, None, &[], &[]),
        // on the secret document: only a recipient, a notified partner or the author reads
        post(
            "on_secret_nobody",
            DOC_A,
            secret,
            false,
            Some(other_partner),
            &[],
            &[],
        ),
        post(
            "on_secret_to_ivy",
            DOC_A,
            secret,
            false,
            None,
            &[internal_partner],
            &[],
        ),
        post(
            "on_secret_notified_ivy",
            DOC_A,
            secret,
            false,
            None,
            &[],
            &[internal_partner],
        ),
        post(
            "on_secret_by_ivy",
            DOC_A,
            secret,
            false,
            Some(internal_partner),
            &[],
            &[],
        ),
        post(
            "on_secret_to_pat",
            DOC_A,
            secret,
            false,
            None,
            &[portal_partner],
            &[],
        ),
        post(
            "on_secret_to_pat_internal",
            DOC_A,
            secret,
            true,
            None,
            &[portal_partner],
            &[],
        ),
        // on the other model, readable by internal users only
        post("on_b", DOC_B, b1, false, None, &[], &[]),
        // no such document any more
        post("orphan", DOC_A, 987_654, false, None, &[], &[]),
        // user notifications on a readable document: only their recipient reads them
        post(
            "un_to_otto",
            DOC_A,
            shared,
            false,
            None,
            &[other_partner],
            &[],
        ),
        post(
            "un_to_ivy",
            DOC_A,
            shared,
            false,
            None,
            &[internal_partner],
            &[],
        ),
    ];
    for (name, vals) in planned {
        let vals = if name.starts_with("un_") {
            vals.set_cell("message_type", text("user_notification"))
        } else {
            vals
        };
        let id = create(&mut tx, "mail.message", vals).await;
        msgs.insert(name, id);
    }
    tx.commit().await.unwrap();
    World {
        h,
        orm,
        internal,
        internal_partner,
        other_partner,
        portal,
        portal_partner,
        public,
        msgs,
    }
}

async fn listing(w: &World, uid: i64) -> Vec<&'static str> {
    let mut tx = w.orm.begin(UserId::new(uid).unwrap()).await.unwrap();
    let found = tx
        .env()
        .records("mail.message")
        .unwrap()
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap();
    let ids = found.ids();
    let mut names: Vec<&'static str> = w
        .msgs
        .iter()
        .filter(|(_, id)| ids.contains(id))
        .map(|(name, _)| *name)
        .collect();
    names.sort_unstable();
    assert_eq!(
        names.len(),
        ids.len(),
        "every message found is one of the fixture's"
    );
    names
}

/// An internal user reads the messages of the documents it can
/// read, on every thread model at once, and the ones addressed to it or written by it on a
/// document it cannot read; nothing else, and no orphan.
#[tokio::test]
async fn an_internal_user_reads_documents_recipients_and_authors() {
    let w = world().await;
    assert_eq!(
        listing(&w, w.internal).await,
        [
            "on_b",
            "on_secret_by_ivy",
            "on_secret_notified_ivy",
            "on_secret_to_ivy",
            "on_shared",
            "on_shared_internal",
            "un_to_ivy",
        ]
    );
    // a user notification on a readable document is read by its recipient only
    let listed = listing(&w, w.internal).await;
    assert!(!listed.contains(&"un_to_otto"));
    let _ = w.other_partner;
    w.h.cleanup().await.unwrap();
}

/// A portal user never sees an internal message, whichever branch would otherwise give it, reads the others of the documents
/// its group can read and those addressed to it, and finds nothing on a model its group cannot
/// read.
#[tokio::test]
async fn a_portal_user_never_sees_an_internal_message() {
    let w = world().await;
    assert_eq!(
        listing(&w, w.portal).await,
        ["on_secret_to_pat", "on_shared"]
    );
    let _ = (w.portal_partner, w.internal_partner);
    w.h.cleanup().await.unwrap();
}

/// A public user reads the non-internal messages of the documents its group can read, and no
/// internal message, user notification or message addressed to others on a document it cannot.
#[tokio::test]
async fn a_public_user_reads_only_non_internal_messages_of_readable_documents() {
    let w = world().await;
    assert_eq!(listing(&w, w.public).await, ["on_shared"]);
    w.h.cleanup().await.unwrap();
}

/// Revision 1 of the design took the model set from the first batch of candidates and missed a
/// model that appears only later. The model set here comes from the registry: a page that holds
/// only messages of one model does not hide the other model's messages from a later page or a
/// filtered search.
#[tokio::test]
async fn a_model_that_appears_only_on_later_rows_is_found() {
    let w = world().await;
    let mut tx = w.orm.begin(UserId::new(w.internal).unwrap()).await.unwrap();
    let messages = tx.env().records("mail.message").unwrap();
    // the newest rows are on `doc_a` (ids descend); `doc_b`'s message is later in the order
    let first_page = messages
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &erp_orm::SearchOptions {
                limit: Some(2),
                ..erp_orm::SearchOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first_page.len(), 2);
    let on_b = Domain::condition("model", Operator::Eq, DOC_B).unwrap();
    let found = messages.search(&mut tx, &on_b).await.unwrap();
    assert_eq!(found.ids(), vec![w.msgs["on_b"]]);
    let total = messages
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    assert_eq!(total, 7);
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// A listing across thread models is one statement, whatever the number of models and rows.
#[tokio::test]
async fn a_listing_is_one_statement() {
    let w = world().await;
    let mut tx = w.orm.begin(UserId::new(w.internal).unwrap()).await.unwrap();
    let messages = tx.env().records("mail.message").unwrap();
    // warm the principal and the policy, then count
    messages.search(&mut tx, &Domain::TRUE).await.unwrap();
    let found = assert_query_count!(tx, >= 1, <= 1, {
        messages.search(&mut tx, &Domain::TRUE).await.unwrap()
    });
    assert_eq!(found.len(), 7);
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// The operator is the policy's: a client domain that sends it is refused like any unknown
/// operator, and nothing changes in what the user may read.
#[tokio::test]
async fn a_client_domain_cannot_use_the_operator() {
    let err = Domain::from_json(&erp_sdk::serde_json::json!([[
        "res_id",
        "thread_access",
        "read"
    ]]))
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Invalid operator in condition ('res_id', 'thread_access', 'read')"
    );
}

/// The declaration is linked into every binary that links the addon.
#[test]
fn the_addon_declares_the_operator() {
    let mut declared: Vec<&str> = erp_orm::RULE_OPERATORS.iter().map(|d| d.name()).collect();
    // the link order of a distributed slice differs between binaries
    declared.sort_unstable();
    assert_eq!(declared, ["activity_thread_access", "thread_access"]);
    assert_eq!(erp_addon_mail::THREAD_ACCESS.model(), "mail.message");
}
