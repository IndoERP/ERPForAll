//! the thread API a client calls over JSON-2 (`thread_post`, `thread_follow`,
//! `thread_unfollow`, `thread_set_subtypes`, `thread_followers`, `thread_subtypes`), through the
//! RPC service on the production policy (`erp-security` compiled from the shipped rows of
//! `addons/mail/security/ir.access.csv` and a few rows of the test models, as `mail_thread_access.rs`
//! does). The test lives here and not under `addons/mail/tests` because an addon crate may not
//! depend on `erp-security` or `erp-rpc` (`scripts/check-layers.py`). Design note:
//! .
//!
//! Cases to.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment, ParentRef, UNLINK_HOOK};
use erp_addon_mail::store_fetch::{Audience, StoreTopic, TopicFuture, TopicInput, TopicOutput};
use erp_addon_mail::{MailMessage, MailThread, StoreFields, ThreadFacts};
use erp_blob::{AttachmentRuntime, MemoryStore};
use erp_orm::{GenerationAttachment, Orm, PolicyProvider, Principal, UserId};
use erp_rpc::{Caller, RpcCall, RpcConfig, RpcHost, RpcResponse, RpcService};
use erp_sdk::prelude::*;
use erp_sdk::store_wire::{DecodedPayload, Decoder, FieldValue as Wire, Outcome, WireLimits};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls};
use serde_json::json;

mod attachment_support;

mod ext_a {
    use erp_addon_mail::{PostRequest, Posted};
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_rpc_mail_a", depends = ["test_rpc_mail"]);

    /// First addon overriding `message_post` of the test thread model: marks a subject that starts
    /// with `ext:`.
    #[extend(super::DocA, addon = "test_rpc_mail_a")]
    pub struct DocAExtA {}

    #[methods(super::DocA, addon = "test_rpc_mail_a")]
    impl DocAExtA {
        #[method(override)]
        async fn message_post(
            this: &RecordSet<super::DocA>,
            tx: &mut Transaction,
            mut request: PostRequest,
            sup: Super,
        ) -> Result<Posted> {
            if let Some(subject) = request.subject.as_mut()
                && subject.starts_with("ext:")
            {
                subject.push_str("|a");
            }
            sup.call(tx, this.as_records(), (request,)).await
        }
    }

    /// Overrides the store entry of messages: a subject that starts with `redact:` is replaced,
    /// one that starts with `unknown-field:` gets a field the store schema does not declare.
    #[extend(super::MailMessage, addon = "test_rpc_mail_a")]
    pub struct MessageStoreA {}

    #[methods(super::MailMessage, addon = "test_rpc_mail_a")]
    impl MessageStoreA {
        #[method(override)]
        async fn _to_store(
            this: &RecordSet<super::MailMessage>,
            tx: &mut Transaction,
            fields: super::StoreFields,
            sup: Super,
        ) -> Result<super::StoreFields> {
            let mut out: super::StoreFields = sup.call(tx, this.as_records(), (fields,)).await?;
            let subjects: Vec<(u64, String)> = out
                .iter()
                .filter_map(|(model, id, field, value)| match value {
                    super::Wire::String(text) if model == "mail.message" && field == "subject" => {
                        Some((id, text.clone()))
                    }
                    _ => None,
                })
                .collect();
            for (id, subject) in subjects {
                if subject.starts_with("redact:") {
                    out.set(
                        "mail.message",
                        id,
                        "subject",
                        super::Wire::String("[redacted]".to_owned()),
                    );
                }
                if subject.starts_with("unknown-field:") {
                    out.set("mail.message", id, "nope", super::Wire::Bool(true));
                }
            }
            Ok(out)
        }
    }

    /// Overrides the thread facts: a `test_rpc_mail.doc_r` thread gets a fact of its own, and a
    /// thread named `dup` sets a base fact again.
    #[extend(super::MailThread, addon = "test_rpc_mail_a")]
    pub struct ThreadStoreA {}

    #[methods(super::MailThread, addon = "test_rpc_mail_a")]
    impl ThreadStoreA {
        #[method(override)]
        async fn _store_thread_fields(
            this: &RecordSet<super::MailThread>,
            tx: &mut Transaction,
            facts: super::ThreadFacts,
            sup: Super,
        ) -> Result<super::ThreadFacts> {
            let mut out: super::ThreadFacts = sup.call(tx, this.as_records(), (facts,)).await?;
            let record = this.as_records().ensure_one()?;
            if record.model_name() == super::DOC_R {
                out.set("ext_note", serde_json::json!("r"))?;
            }
            if record.model_name() == super::DOC_P {
                out.set("can_post", serde_json::json!(true))?;
            }
            Ok(out)
        }
    }
}

mod ext_b {
    use erp_addon_mail::{PostRequest, Posted};
    use erp_sdk::prelude::*;

    erp_sdk::addon!(name = "test_rpc_mail_b", depends = ["test_rpc_mail_a"]);

    /// Second addon overriding the same method; it loads after the first.
    #[extend(super::DocA, addon = "test_rpc_mail_b")]
    pub struct DocAExtB {}

    #[methods(super::DocA, addon = "test_rpc_mail_b")]
    impl DocAExtB {
        #[method(override)]
        async fn message_post(
            this: &RecordSet<super::DocA>,
            tx: &mut Transaction,
            mut request: PostRequest,
            sup: Super,
        ) -> Result<Posted> {
            if let Some(subject) = request.subject.as_mut()
                && subject.starts_with("ext:")
            {
                subject.push_str("|b");
            }
            sup.call(tx, this.as_records(), (request,)).await
        }
    }

    /// Runs after the first override: it appends to what that one left.
    #[extend(super::MailMessage, addon = "test_rpc_mail_b")]
    pub struct MessageStoreB {}

    #[methods(super::MailMessage, addon = "test_rpc_mail_b")]
    impl MessageStoreB {
        #[method(override)]
        async fn _to_store(
            this: &RecordSet<super::MailMessage>,
            tx: &mut Transaction,
            fields: super::StoreFields,
            sup: Super,
        ) -> Result<super::StoreFields> {
            let mut out: super::StoreFields = sup.call(tx, this.as_records(), (fields,)).await?;
            let marked: Vec<(u64, String)> = out
                .iter()
                .filter_map(|(model, id, field, value)| match value {
                    super::Wire::String(text)
                        if model == "mail.message"
                            && field == "subject"
                            && text == "[redacted]" =>
                    {
                        Some((id, format!("{text}|b")))
                    }
                    _ => None,
                })
                .collect();
            for (id, text) in marked {
                out.set("mail.message", id, "subject", super::Wire::String(text));
            }
            Ok(out)
        }
    }
}

erp_sdk::addon!(name = "test_rpc_mail", depends = ["mail"]);

/// A thread model whose internal users read and write, whose portal users read and write the
/// record named `shared`, and whose public users read it.
#[model(
    name = "test_rpc_mail.doc_a",
    description = "RPC Doc A",
    inherit = [MailThread],
    addon = "test_rpc_mail"
)]
pub struct DocA {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A thread model internal users may only read (post access `write` by default).
#[model(
    name = "test_rpc_mail.doc_r",
    description = "RPC Doc R",
    inherit = [MailThread],
    addon = "test_rpc_mail"
)]
pub struct DocR {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

/// A thread model portal users may read whole; it shows them only `email_outgoing` messages.
#[model(
    name = "test_rpc_mail.doc_p",
    description = "RPC Doc P",
    inherit = [MailThread],
    addon = "test_rpc_mail"
)]
pub struct DocP {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(DocP, addon = "test_rpc_mail")]
impl DocP {
    #[method(override)]
    async fn _get_customer_portal_message_types(
        _this: &RecordSet<DocP>,
        _tx: &mut Transaction,
        _sup: Super,
    ) -> Result<Vec<String>> {
        Ok(vec!["email_outgoing".to_owned()])
    }
}

#[methods(DocR, addon = "test_rpc_mail")]
impl DocR {
    /// A thread model may declare a `_to_store` of its own next to the mixin's
    /// `_store_thread_fields`: the names do not collide.
    #[method]
    async fn _to_store(
        _this: &RecordSet<DocR>,
        _tx: &mut Transaction,
        mut fields: StoreFields,
    ) -> Result<StoreFields> {
        fields.set("test_rpc_mail.doc_r", 1, "marker", Wire::Bool(true));
        Ok(fields)
    }
}

const DOC_A: &str = "test_rpc_mail.doc_a";
const DOC_P: &str = "test_rpc_mail.doc_p";
const DOC_R: &str = "test_rpc_mail.doc_r";
const ACCESS_CSV: &str = include_str!("../../../addons/mail/security/ir.access.csv");

struct Host;

impl RpcHost for Host {
    fn language_active(&self, _code: &str) -> Result<bool> {
        Ok(false)
    }

    fn has_capability(
        &self,
        _principal: &Principal,
        _capability: &erp_orm::Capability,
    ) -> Result<bool> {
        Ok(false)
    }
}

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
            eprintln!("mail_rpc: connection closed with an error: {error}");
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

/// A person of the fixture: its user and its partner.
#[derive(Clone, Copy)]
struct Who {
    uid: i64,
    partner: i64,
}

struct World {
    h: Harness,
    orm: Orm,
    service: RpcService,
    client: Client,
    ivy: Who,
    otto: Who,
    pat: Who,
    public: Who,
    /// a partner without a user that internal users may not read
    hidden_partner: i64,
    doc_shared: i64,
    doc_secret: i64,
    doc_r: i64,
    doc_p: i64,
    /// `mail.mt_comment`
    comment: i64,
    /// `mail.mt_note`
    note: i64,
    /// for `doc_a`, not internal
    a_news: i64,
    /// for `doc_a`, internal
    a_secret: i64,
    /// for `doc_r` only
    r_news: i64,
}

async fn world() -> World {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_addon_mail::ADDON,
        &ADDON,
        &ext_a::ADDON,
        &ext_b::ADDON,
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
    ]);
    for line in ACCESS_CSV.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f = csv_fields(line);
        let [_, name, model, group, ops, domain] = f.as_slice() else {
            panic!("a row of six columns: {line}");
        };
        // the activity models and the scheduled message model are
        // not in this fixture's registry; their rows are exercised by `addons/mail/tests/activity.rs`
        // and `mail_scheduled.rs`
        if model.starts_with("model_mail_activity") || model == "model_mail_scheduled_message" {
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
    }
    // the test models' own rules: the models decide who may post (write by default)
    let doc_a = model_row(&client, DOC_A, "RPC Doc A").await;
    let doc_r = model_row(&client, DOC_R, "RPC Doc R").await;
    let doc_p = model_row(&client, DOC_P, "RPC Doc P").await;
    let partner_model = model_row(&client, "res.partner", "Contact").await;
    rule(&client, "internal doc a", doc_a, g_user, "crud", None).await;
    rule(
        &client,
        "portal doc a",
        doc_a,
        g_portal,
        "ru",
        Some("[('name', '=', 'shared')]"),
    )
    .await;
    rule(
        &client,
        "public doc a",
        doc_a,
        g_public,
        "r",
        Some("[('name', '=', 'shared')]"),
    )
    .await;
    rule(&client, "internal doc r", doc_r, g_user, "r", None).await;
    rule(&client, "internal doc p", doc_p, g_user, "crud", None).await;
    rule(&client, "portal doc p", doc_p, g_portal, "r", None).await;
    rule(
        &client,
        "internal partner",
        partner_model,
        g_user,
        "r",
        Some("[('name', '!=', 'hidden')]"),
    )
    .await;
    let (public, _) = {
        let (uid, partner) = user(&client, "pub", &[g_public], true).await;
        (Who { uid, partner }, ())
    };
    let ivy = {
        let (uid, partner) = user(&client, "ivy", &[g_user], false).await;
        Who { uid, partner }
    };
    let otto = {
        let (uid, partner) = user(&client, "otto", &[g_user], false).await;
        Who { uid, partner }
    };
    let pat = {
        let (uid, partner) = user(&client, "pat", &[g_portal], true).await;
        Who { uid, partner }
    };
    let hidden_partner = one(
        &client,
        "INSERT INTO res_partner (name, active) VALUES ('hidden', true) RETURNING id::int8",
        &[],
    )
    .await;

    let source = PolicySource::build(Arc::clone(h.registry()), pool(&h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    // the attachment service (files posted with messages): an
    // in-memory blob store and the hook that deletes the files of deleted records
    let blobs = Arc::new(AttachmentRuntime::single(
        "main",
        Arc::new(MemoryStore::new()) as _,
        attachment_support::limits(),
    ));
    let orm = Orm::builder(Arc::clone(h.registry()), pool(&h, 4))
        .policy(provider)
        .resolver(Resolver::id_order())
        .generation_attachment(GenerationAttachment::new(Arc::clone(h.registry()), blobs))
        .unlink_hook(UNLINK_HOOK)
        .build()
        .unwrap();

    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let doc_shared = create(
        &mut tx,
        DOC_A,
        RawVals::new().set_cell("name", text("shared")),
    )
    .await;
    let doc_secret = create(
        &mut tx,
        DOC_A,
        RawVals::new().set_cell("name", text("secret")),
    )
    .await;
    let doc_r_id = create(&mut tx, DOC_R, RawVals::new().set_cell("name", text("r"))).await;
    let doc_p_id = create(&mut tx, DOC_P, RawVals::new().set_cell("name", text("p"))).await;
    let subtype = |name: &'static str,
                   internal: bool,
                   default: bool,
                   seq: i32,
                   model: Option<&'static str>| {
        let mut vals = RawVals::new()
            .set_cell("name", text(name))
            .set_cell("internal", Cell::Bool(internal))
            .set_cell("default", Cell::Bool(default))
            .set_cell("sequence", Cell::Int(seq));
        if let Some(model) = model {
            vals = vals.set_cell("res_model", text(model));
        }
        vals
    };
    let comment = create(
        &mut tx,
        "mail.message.subtype",
        subtype("Messages", false, true, 0, None),
    )
    .await;
    let note = create(
        &mut tx,
        "mail.message.subtype",
        subtype("Notes", true, false, 90, None),
    )
    .await;
    let a_news = create(
        &mut tx,
        "mail.message.subtype",
        subtype("A News", false, true, 10, Some(DOC_A)),
    )
    .await;
    let a_secret = create(
        &mut tx,
        "mail.message.subtype",
        subtype("A Secret", true, true, 20, Some(DOC_A)),
    )
    .await;
    let r_news = create(
        &mut tx,
        "mail.message.subtype",
        subtype("R News", false, true, 10, Some(DOC_R)),
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
    tx.commit().await.unwrap();
    let service = RpcService::new(orm.clone(), Host, RpcConfig::default());
    World {
        h,
        orm,
        service,
        client,
        ivy,
        otto,
        pat,
        public,
        hidden_partner,
        doc_shared,
        doc_secret,
        doc_r: doc_r_id,
        doc_p: doc_p_id,
        comment,
        note,
        a_news,
        a_secret,
        r_news,
    }
}

impl World {
    async fn caller(&self, who: Who) -> Caller {
        let mut tx = self.orm.begin(UserId::new(who.uid).unwrap()).await.unwrap();
        let principal = tx.env().principal().clone();
        tx.rollback().await.unwrap();
        Caller::Principal(principal)
    }

    /// `POST /json/2/<model>/<method>` with `body`, as `who`.
    async fn call(
        &self,
        who: Who,
        model: &str,
        method: &str,
        body: serde_json::Value,
    ) -> RpcResponse {
        let caller = self.caller(who).await;
        self.service
            .call(RpcCall::json2(
                &format!("/json/2/{model}/{method}"),
                serde_json::to_vec(&body).unwrap(),
                caller,
            ))
            .await
    }

    async fn n(&self, sql: &str) -> i64 {
        self.client.query_one(sql, &[]).await.unwrap().get(0)
    }

    async fn cleanup(self) {
        drop(self.service);
        drop(self.orm);
        self.h.cleanup().await.unwrap();
    }
}

fn body_json(response: &RpcResponse) -> serde_json::Value {
    serde_json::from_slice(response.body())
        .unwrap_or_else(|e| panic!("body is not JSON ({e}): {:?}", response.body()))
}

#[track_caller]
fn ok(response: &RpcResponse) -> serde_json::Value {
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    body_json(response)
}

/// An error: HTTP status, the error `name` and the full `message`.
#[track_caller]
fn assert_error(response: &RpcResponse, status: u16, name: &str, message: &str) {
    let text = String::from_utf8_lossy(response.body()).into_owned();
    assert_eq!(response.status(), status, "{text}");
    let body = body_json(response);
    assert_eq!(body["name"], name, "{text}");
    assert_eq!(body["message"], message, "{text}");
}

/// The access engine's refusal of a write on `test_rpc_mail.doc_r`.
const DOC_R_WRITE_DENIED: &str = "You are not allowed to modify 'RPC Doc R' (test_rpc_mail.doc_r) records.\n\nNo group currently allows this operation.\n\nContact your administrator to request access if necessary.";
const ACCESS: &str = "erp.exceptions.AccessError";
const VALIDATION: &str = "erp.exceptions.ValidationError";

fn decode(payload: &serde_json::Value) -> DecodedPayload {
    let schema = erp_addon_mail::store::schema().unwrap().clone();
    let decoder = Decoder::new(WireLimits::default()).with_schema(schema);
    match decoder
        .decode_answer(&serde_json::to_vec(payload).unwrap())
        .unwrap()
    {
        Outcome::Decoded(payload) => payload,
        other => panic!("the payload did not decode: {other:?}"),
    }
}

fn record<'a>(payload: &'a DecodedPayload, model: &str, id: i64) -> BTreeMap<&'a str, &'a Wire> {
    let found = payload
        .upserts()
        .iter()
        .find(|r| r.model().as_str() == model && r.id() == id as u64)
        .unwrap_or_else(|| panic!("no {model} {id} in {payload:?}"));
    found
        .fields()
        .iter()
        .map(|f| (f.name().as_str(), f.value()))
        .collect()
}

fn ids(value: &serde_json::Value) -> Vec<i64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect()
}

/// An internal user posts a text with a mention. The answer carries the message and its
/// recipients, the message is a `mail.message` record of the store whose fields equal the stored
/// row, the descriptor carries the writing transaction's own xid and marks the written fields.
#[tokio::test]
async fn an_internal_user_posts_text_and_gets_the_message_in_the_store() {
    let w = world().await;
    let response = w
        .call(
            w.ivy,
            DOC_A,
            "thread_post",
            json!({
                "ids": [w.doc_shared],
                "post": {
                    "body": "Hi <b>Otto</b> & co",
                    "subject": "greetings",
                    "partner_ids": [],
                    "mentions": [{"start": 3, "end": 10, "partner_id": w.otto.partner}],
                    "client_uid": "0f9c1b1e-5d37-4a4b-9a62-3f1e2f7d9b10",
                }
            }),
        )
        .await;
    let answer = ok(&response);
    let message = answer["message_id"].as_i64().unwrap();
    assert_eq!(answer["replayed"], json!(false));
    assert_eq!(ids(&answer["notified_partner_ids"]), vec![w.otto.partner]);
    let payload = decode(&answer["store"]);
    assert_eq!(
        payload.generation().as_str(),
        erp_addon_mail::store_fetch::TopicRegistry::linked()
            .unwrap()
            .schema()
            .generation()
            .as_str(),
        "a thread answer carries the generation of the one registry schema"
    );
    assert!(
        payload.asof().own().is_some(),
        "a writing answer carries its own xid"
    );
    let fields = record(&payload, "mail.message", message);
    // every value of the payload equals the stored row
    let row = w
        .client
        .query_one(
            "SELECT body, subject, message_type, subtype_id::int8, author_id::int8, \
             parent_id::int8, is_internal, model, res_id::int8 FROM mail_message WHERE id = $1",
            &[&message],
        )
        .await
        .unwrap();
    let stored_body: String = row.get(0);
    assert_eq!(fields["body"], &Wire::Html(stored_body.clone()));
    assert_eq!(
        stored_body,
        "<p>Hi <a class=\"o_mail_redirect\" data-oe-model=\"res.partner\" data-oe-id=\"{OTTO}\">&lt;b&gt;Otto</a>&lt;/b&gt; &amp; co</p>"
            .replace("{OTTO}", &w.otto.partner.to_string())
    );
    assert_eq!(fields["subject"], &Wire::String("greetings".to_owned()));
    assert_eq!(
        fields["message_type"],
        &Wire::Selection("comment".to_owned())
    );
    let subtype: i64 = row.get(3);
    assert_eq!(subtype, w.comment);
    assert_eq!(
        fields["subtype_id"],
        &Wire::Many2One(Some(w.comment as u64))
    );
    let author: i64 = row.get(4);
    assert_eq!(author, w.ivy.partner, "the author is the caller's partner");
    assert_eq!(
        fields["author_id"],
        &Wire::Many2One(Some(w.ivy.partner as u64))
    );
    assert_eq!(fields["parent_id"], &Wire::Many2One(None));
    assert_eq!(fields["is_internal"], &Wire::Bool(false));
    assert_eq!(
        fields["partner_ids"],
        &Wire::X2ManyReplace(vec![w.otto.partner as u64])
    );
    assert_eq!(fields["res_model"], &Wire::String(DOC_A.to_owned()));
    assert_eq!(fields["res_id"], &Wire::Int(w.doc_shared));
    // the fields the transaction wrote are marked
    let written: Vec<&str> = payload
        .upserts()
        .iter()
        .flat_map(|r| r.fields())
        .filter(|f| f.written())
        .map(|f| f.name().as_str())
        .collect();
    assert!(written.contains(&"body") && written.contains(&"message_type"));
    // a comment by an internal author who may write the record makes the author follow it
    assert_eq!(
        w.n(&format!(
            "SELECT count(*) FROM mail_followers WHERE res_model = '{DOC_A}' AND res_id = {} \
             AND partner_id = {}",
            w.doc_shared, w.ivy.partner
        ))
        .await,
        1
    );
    w.cleanup().await;
}

async fn post(w: &World, who: Who, doc: i64, post: serde_json::Value) -> RpcResponse {
    w.call(
        who,
        DOC_A,
        "thread_post",
        json!({"ids": [doc], "post": post}),
    )
    .await
}

async fn messages(w: &World) -> i64 {
    w.n("SELECT count(*) FROM mail_message").await
}

/// A client cannot name the author, the model, the record, the sender address, HTML or any other
/// column: every unknown key is refused naming the key, and nothing is written.
#[tokio::test]
async fn forged_and_unknown_keys_are_refused_with_the_key_names() {
    let w = world().await;
    let entry = format!("{DOC_A}.thread_post");
    for (key, value) in [
        ("author_id", json!(1)),
        ("email_from", json!("boss@example.com")),
        ("model", json!("res.users")),
        ("res_id", json!(1)),
        ("body_is_html", json!(true)),
        ("attachments", json!([["name", "content"]])),
        ("tracking_values", json!([])),
        ("subtype_xmlid", json!("mail.mt_note")),
        ("message_id", json!("<x@y>")),
    ] {
        let response = post(&w, w.ivy, w.doc_shared, json!({"body": "x", key: value})).await;
        assert_error(
            &response,
            422,
            VALIDATION,
            &format!(
                "argument 'post' of {entry}: The post has no key '{key}'; its keys are body, \
                 subject, message_type, subtype, parent_id, partner_ids, mentions, silent, \
                 client_uid, attachment_ids."
            ),
        );
    }
    // a body is required, a top-level key outside the signature is refused by the binder
    let response = post(&w, w.ivy, w.doc_shared, json!({})).await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!("argument 'post' of {entry}: A post needs a body."),
    );
    let response = w
        .call(
            w.ivy,
            DOC_A,
            "thread_post",
            json!({"ids": [w.doc_shared], "post": {"body": "x"}, "author_id": 1}),
        )
        .await;
    assert_eq!(
        response.status(),
        422,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    assert_eq!(messages(&w).await, 0, "nothing was written");
    w.cleanup().await;
}

/// HTML is refused whole, text is escaped by the server, and a client may post a comment or a
/// notification, never a server-made type.
#[tokio::test]
async fn an_html_body_is_refused_and_text_is_escaped() {
    let w = world().await;
    let entry = format!("{DOC_A}.thread_post");
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": {"html": "<b onclick=x()>hi</b>"}}),
    )
    .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "argument 'post' of {entry}: An HTML body is refused until HTML fields are sanitized (a later release); post the text with its mentions instead."
        ),
    );
    assert_eq!(messages(&w).await, 0);
    // the same markup as text is stored escaped
    let answer = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": {"text": "<script>alert(1)</script>"}}),
    )
    .await);
    let stored: String = w
        .client
        .query_one(
            "SELECT body FROM mail_message WHERE id = $1",
            &[&answer["message_id"].as_i64().unwrap()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>");
    for (kind, message) in [
        (
            "email",
            "A client posts a comment or a notification, not 'email'; the other message types \
             are made by the server.",
        ),
        (
            "tracking",
            "A client posts a comment or a notification, not 'tracking'; the other message types \
             are made by the server.",
        ),
        (
            "user_notification",
            "A user_notification is not posted on a thread; it is sent with message_notify, \
             which is not implemented yet.",
        ),
    ] {
        let response = post(
            &w,
            w.ivy,
            w.doc_shared,
            json!({"body": "x", "message_type": kind}),
        )
        .await;
        assert_error(
            &response,
            422,
            VALIDATION,
            &format!("argument 'post' of {entry}: {message}"),
        );
    }
    assert_eq!(messages(&w).await, 1);
    w.cleanup().await;
}

#[tokio::test]
async fn message_types_and_subtypes_a_client_may_post() {
    let w = world().await;
    // a note: a notification under the note subtype, which reaches nobody who does not follow
    let note = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "internal", "message_type": "notification", "subtype": "note"}),
    )
    .await);
    let row = w
        .client
        .query_one(
            "SELECT message_type, subtype_id::int8 FROM mail_message WHERE id = $1",
            &[&note["message_id"].as_i64().unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "notification");
    assert_eq!(row.get::<_, i64>(1), w.note);
    // a subtype of the model by external id is not found: the test model has none, and by id
    let by_id = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "news", "subtype": w.a_news}),
    )
    .await);
    let subtype: i64 = w
        .client
        .query_one(
            "SELECT subtype_id::int8 FROM mail_message WHERE id = $1",
            &[&by_id["message_id"].as_i64().unwrap()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(subtype, w.a_news);
    // a subtype of another model is refused by the post path with its own message
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "subtype": w.r_news}),
    )
    .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "Unknown subtype id {} for {DOC_A}: a subtype must exist and have no model or the \
             model of the record.",
            w.r_news
        ),
    );
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "subtype": "mail.mt_nope"}),
    )
    .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "Unknown subtype 'mail.mt_nope' for {DOC_A}: a subtype must exist and have no model \
             or the model of the record."
        ),
    );
    w.cleanup().await;
}

#[tokio::test]
async fn public_users_cannot_post_or_follow() {
    let w = world().await;
    let response = post(&w, w.public, w.doc_shared, json!({"body": "hi"})).await;
    assert_error(
        &response,
        403,
        ACCESS,
        "Public users cannot post on a thread.",
    );
    for (method, body) in [
        (
            "thread_follow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.public.partner]}),
        ),
        (
            "thread_unfollow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.public.partner]}),
        ),
        (
            "thread_set_subtypes",
            json!({"ids": [w.doc_shared], "partner_id": w.public.partner, "subtype_ids": [w.comment]}),
        ),
        ("thread_followers", json!({"ids": [w.doc_shared]})),
        ("thread_subtypes", json!({"ids": [w.doc_shared]})),
    ] {
        let response = w.call(w.public, DOC_A, method, body).await;
        let message = if method == "thread_followers" {
            "Only internal users can list followers."
        } else {
            "Public users cannot use the follower and subtype calls of a thread."
        };
        assert_error(&response, 403, ACCESS, message);
    }
    assert_eq!(messages(&w).await, 0);
    assert_eq!(w.n("SELECT count(*) FROM mail_followers").await, 0);
    w.cleanup().await;
}

/// A portal user posts a comment on a record it may write, under the comment subtype, and nothing
/// else: no note, no other subtype, no recipients, no mentions. It is not told who was notified
/// and does not become a follower. A record its group cannot write refuses the post.
#[tokio::test]
async fn portal_users_post_comments_only() {
    let w = world().await;
    let answer = ok(&post(&w, w.pat, w.doc_shared, json!({"body": "from the portal"})).await);
    assert!(
        answer.get("notified_partner_ids").is_none(),
        "a portal caller is not told who follows the record: {answer}"
    );
    let message = answer["message_id"].as_i64().unwrap();
    let row = w
        .client
        .query_one(
            "SELECT author_id::int8, message_type, subtype_id::int8 FROM mail_message WHERE id = $1",
            &[&message],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), w.pat.partner);
    assert_eq!(row.get::<_, String>(1), "comment");
    assert_eq!(row.get::<_, i64>(2), w.comment);
    let payload = decode(&answer["store"]);
    assert_eq!(
        record(&payload, "mail.message", message)["author_id"],
        &Wire::Many2One(Some(w.pat.partner as u64))
    );
    assert_eq!(
        w.n("SELECT count(*) FROM mail_followers").await,
        0,
        "a portal author is not subscribed"
    );
    let shape = |what: &str| {
        format!(
            "Portal users post comments only (message_type comment, subtype comment), not {what}."
        )
    };
    for (post_body, message) in [
        (
            json!({"body": "x", "message_type": "notification"}),
            shape("message_type 'notification'"),
        ),
        (
            json!({"body": "x", "subtype": "note"}),
            shape("subtype 'note'"),
        ),
        (
            json!({"body": "x", "subtype": "mail.mt_note"}),
            shape("subtype 'mail.mt_note'"),
        ),
        (
            json!({"body": "x", "subtype": w.note}),
            shape(&format!("subtype id {}", w.note)),
        ),
        (
            json!({"body": "x", "partner_ids": [w.ivy.partner]}),
            "Portal users cannot address or mention partners.".to_owned(),
        ),
        (
            json!({"body": "x", "mentions": [{"start": 0, "end": 1, "partner_id": w.ivy.partner}]}),
            "Portal users cannot address or mention partners.".to_owned(),
        ),
    ] {
        let response = post(&w, w.pat, w.doc_shared, post_body).await;
        assert_error(&response, 403, ACCESS, &message);
    }
    // the explicit comment shape is accepted
    ok(&post(
        &w,
        w.pat,
        w.doc_shared,
        json!({"body": "y", "message_type": "comment", "subtype": "mail.mt_comment"}),
    )
    .await);
    // a record the portal group may not write refuses the post with the engine's message
    let response = post(&w, w.pat, w.doc_secret, json!({"body": "z"})).await;
    assert_error(
        &response,
        403,
        ACCESS,
        &format!(
            "Uh-oh! Looks like you have stumbled upon some top-secret records.\n\nSorry, pat \
             (id={}) doesn't have 'write' access to:\n- RPC Doc A (test_rpc_mail.doc_a)\n\nIf \
             you really, really need access, perhaps you can win over your friendly administrator \
             with a batch of freshly baked cookies.",
            w.pat.uid
        ),
    );
    assert_eq!(messages(&w).await, 2);
    w.cleanup().await;
}

/// An internal user addresses or mentions partners it may read. A partner that does not exist and
/// one it may not read are refused alike, so the answer tells nothing about which exists.
#[tokio::test]
async fn recipients_and_mentions_must_be_readable() {
    let w = world().await;
    let refusal = |ids: &str| {
        format!(
            "You may not address partner ids {ids}: they do not exist or you may not read them."
        )
    };
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "partner_ids": [w.hidden_partner]}),
    )
    .await;
    assert_error(
        &response,
        403,
        ACCESS,
        &refusal(&w.hidden_partner.to_string()),
    );
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "partner_ids": [9_999_999, w.otto.partner]}),
    )
    .await;
    assert_error(&response, 403, ACCESS, &refusal("9999999"));
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "mentions": [{"start": 0, "end": 1, "partner_id": w.hidden_partner}]}),
    )
    .await;
    assert_error(
        &response,
        403,
        ACCESS,
        &refusal(&w.hidden_partner.to_string()),
    );
    assert_eq!(messages(&w).await, 0, "nothing was written");
    // a readable partner is addressed
    let answer = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "partner_ids": [w.otto.partner]}),
    )
    .await);
    assert_eq!(ids(&answer["notified_partner_ids"]), vec![w.otto.partner]);
    w.cleanup().await;
}

#[tokio::test]
async fn oversize_input_is_refused() {
    let w = world().await;
    let mentions: Vec<serde_json::Value> = (0..101)
        .map(|i| json!({"start": i, "end": i + 1, "partner_id": w.otto.partner}))
        .collect();
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x".repeat(200), "mentions": mentions}),
    )
    .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        "At most 100 mentions are accepted per post, not 101.",
    );
    let many: Vec<i64> = (1..=1001).collect();
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "partner_ids": many}),
    )
    .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        "At most 1000 partner ids are accepted per post, not 1001.",
    );
    let limit = erp_addon_mail::MAX_BODY_BYTES;
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x".repeat(limit + 1)}),
    )
    .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "A post body holds at most {limit} bytes, not {}.",
            limit + 1
        ),
    );
    assert_eq!(messages(&w).await, 0);
    // The limit counts the stored form: exactly `limit` raw bytes is stored as `<p>` + body +
    // `</p>`, over the store string limit, so it is refused cleanly and nothing is stored; the
    // largest body whose wrapped form fits is accepted and answered.
    let response = post(&w, w.ivy, w.doc_shared, json!({"body": "x".repeat(limit)})).await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "A post body holds at most {limit} bytes, not {}.",
            limit + 7
        ),
    );
    assert_eq!(messages(&w).await, 0);
    ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x".repeat(limit - 7)}),
    )
    .await);
    assert_eq!(messages(&w).await, 1);
    w.cleanup().await;
}

/// A retry with the same `client_uid` by the same user on the same record returns the stored
/// message and writes nothing; the same uid for another user or another record is refused.
#[tokio::test]
async fn a_client_uid_replay_returns_the_stored_message() {
    let w = world().await;
    let uid = "5d1b2a54-3c58-4a9e-8f3a-6a0a4b6f2c11";
    let first = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "once", "client_uid": uid, "partner_ids": [w.otto.partner]}),
    )
    .await);
    assert_eq!(first["replayed"], json!(false));
    let again = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "once", "client_uid": uid, "partner_ids": [w.otto.partner]}),
    )
    .await);
    assert_eq!(again["replayed"], json!(true));
    assert_eq!(again["message_id"], first["message_id"]);
    assert_eq!(again["notified_partner_ids"], first["notified_partner_ids"]);
    assert_eq!(messages(&w).await, 1);
    let payload = decode(&again["store"]);
    assert!(
        payload.asof().own().is_none(),
        "a replay writes nothing, its descriptor has no own xid"
    );
    record(
        &payload,
        "mail.message",
        first["message_id"].as_i64().unwrap(),
    );
    let used = "This client_uid was used for another post; choose a new one.";
    let response = post(
        &w,
        w.otto,
        w.doc_shared,
        json!({"body": "x", "client_uid": uid}),
    )
    .await;
    assert_error(&response, 422, VALIDATION, used);
    let response = w
        .call(
            w.ivy,
            DOC_A,
            "thread_post",
            json!({"ids": [w.doc_secret], "post": {"body": "x", "client_uid": uid}}),
        )
        .await;
    assert_error(&response, 422, VALIDATION, used);
    let response = post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "client_uid": "00000000-0000-0000-0000-000000000000"}),
    )
    .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        "A client_uid must not be the nil UUID.",
    );
    assert_eq!(messages(&w).await, 1);
    w.cleanup().await;
}

/// A user who may only read a record cannot post on it, while a model whose post access is read
/// (`res.partner`) lets that user post; a user without write access does not become a follower.
#[tokio::test]
async fn a_user_without_write_posts_only_where_post_access_is_read() {
    let w = world().await;
    let response = w
        .call(
            w.ivy,
            DOC_R,
            "thread_post",
            json!({"ids": [w.doc_r], "post": {"body": "x"}}),
        )
        .await;
    assert_error(&response, 403, ACCESS, DOC_R_WRITE_DENIED);
    let answer = ok(&w
        .call(
            w.ivy,
            "res.partner",
            "thread_post",
            json!({"ids": [w.otto.partner], "post": {"body": "hello otto"}}),
        )
        .await);
    assert_eq!(answer["replayed"], json!(false));
    assert_eq!(
        w.n(&format!(
            "SELECT count(*) FROM mail_message WHERE model = 'res.partner' AND res_id = {} \
             AND author_id = {}",
            w.otto.partner, w.ivy.partner
        ))
        .await,
        1
    );
    assert_eq!(
        w.n("SELECT count(*) FROM mail_followers").await,
        0,
        "a reader who comments does not become a follower"
    );
    // with write access the same comment subscribes the author
    ok(&post(&w, w.ivy, w.doc_shared, json!({"body": "x"})).await);
    assert_eq!(w.n("SELECT count(*) FROM mail_followers").await, 1);
    w.cleanup().await;
}

/// The follower row of `partner` on `model` record `res_id`: (id, sorted subtype ids).
async fn follower(w: &World, model: &str, res_id: i64, partner: i64) -> Option<(i64, Vec<i64>)> {
    let row = w
        .client
        .query_opt(
            "SELECT id::int8 FROM mail_followers WHERE res_model = $1 AND res_id = $2 \
             AND partner_id = $3",
            &[&model, &res_id, &partner],
        )
        .await
        .unwrap()?;
    let id: i64 = row.get(0);
    let subtypes = w
        .client
        .query(
            "SELECT mail_message_subtype_id::int8 FROM mail_followers_mail_message_subtype_rel \
             WHERE mail_followers_id = $1 ORDER BY 1",
            &[&id],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    Some((id, subtypes))
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

/// Following oneself needs read access, following others needs write access, and the answer is
/// the follower records of the named partners. A portal user follows only itself, with the
/// external default subtypes, and may not choose an internal subtype.
#[tokio::test]
async fn following_oneself_needs_read_and_others_need_write() {
    let w = world().await;
    // read-only record: self yes, others no
    let own = ok(&w
        .call(
            w.ivy,
            DOC_R,
            "thread_follow",
            json!({"ids": [w.doc_r], "partner_ids": [w.ivy.partner]}),
        )
        .await);
    let (row_id, subtypes) = follower(&w, DOC_R, w.doc_r, w.ivy.partner).await.unwrap();
    assert_eq!(subtypes, sorted(vec![w.comment, w.r_news]));
    let payload = decode(&own["store"]);
    assert!(payload.asof().own().is_some());
    let fields = record(&payload, "mail.followers", row_id);
    assert_eq!(fields["res_model"], &Wire::String(DOC_R.to_owned()));
    assert_eq!(fields["res_id"], &Wire::Int(w.doc_r));
    assert_eq!(
        fields["partner_id"],
        &Wire::Many2One(Some(w.ivy.partner as u64))
    );
    assert_eq!(
        fields["subtype_ids"],
        &Wire::X2ManyReplace(subtypes.iter().map(|id| *id as u64).collect())
    );
    let response = w
        .call(
            w.ivy,
            DOC_R,
            "thread_follow",
            json!({"ids": [w.doc_r], "partner_ids": [w.otto.partner]}),
        )
        .await;
    assert_error(&response, 403, ACCESS, DOC_R_WRITE_DENIED);
    assert!(follower(&w, DOC_R, w.doc_r, w.otto.partner).await.is_none());
    // writable record: others, with explicit subtypes
    let others = ok(&w
        .call(
            w.ivy,
            DOC_A,
            "thread_follow",
            json!({
                "ids": [w.doc_shared],
                "partner_ids": [w.otto.partner],
                "subtype_ids": [w.comment, w.a_secret],
            }),
        )
        .await);
    let (otto_row, otto_subtypes) = follower(&w, DOC_A, w.doc_shared, w.otto.partner)
        .await
        .unwrap();
    assert_eq!(otto_subtypes, sorted(vec![w.comment, w.a_secret]));
    record(&decode(&others["store"]), "mail.followers", otto_row);
    // a subtype of another model is refused
    let response = w
        .call(
            w.ivy,
            DOC_A,
            "thread_follow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.ivy.partner], "subtype_ids": [w.r_news]}),
        )
        .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "Unknown subtype ids for {DOC_A}: {}. A subtype must exist and have no model or the \
             model of the record.",
            w.r_news
        ),
    );
    // a portal user: itself only, external subtypes only
    let own = ok(&w
        .call(
            w.pat,
            DOC_A,
            "thread_follow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.pat.partner]}),
        )
        .await);
    let (pat_row, pat_subtypes) = follower(&w, DOC_A, w.doc_shared, w.pat.partner)
        .await
        .unwrap();
    assert_eq!(
        pat_subtypes,
        sorted(vec![w.comment, w.a_news]),
        "a portal follower gets the external defaults"
    );
    record(&decode(&own["store"]), "mail.followers", pat_row);
    let only_own =
        "Portal users can only follow, unfollow or set the subtypes of their own partner.";
    for partners in [
        json!([w.otto.partner]),
        json!([w.pat.partner, w.otto.partner]),
    ] {
        let response = w
            .call(
                w.pat,
                DOC_A,
                "thread_follow",
                json!({"ids": [w.doc_shared], "partner_ids": partners}),
            )
            .await;
        assert_error(&response, 403, ACCESS, only_own);
    }
    let response = w
        .call(
            w.pat,
            DOC_A,
            "thread_follow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.pat.partner], "subtype_ids": [w.a_secret]}),
        )
        .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "Unknown subtype ids for {DOC_A}: {}. A subtype must exist and have no model or the \
             model of the record.",
            w.a_secret
        ),
    );
    assert_eq!(
        follower(&w, DOC_A, w.doc_shared, w.pat.partner)
            .await
            .unwrap()
            .1,
        pat_subtypes,
        "the refused call changed nothing"
    );
    // a record the portal group cannot read
    let response = w
        .call(
            w.pat,
            DOC_A,
            "thread_follow",
            json!({"ids": [w.doc_secret], "partner_ids": [w.pat.partner]}),
        )
        .await;
    assert_eq!(
        response.status(),
        403,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    w.cleanup().await;
}

/// Unfollowing answers with a delete marker for each follower record removed. Others need write
/// access; oneself needs none for an internal user; a portal user names only itself.
#[tokio::test]
async fn unfollow_answers_with_delete_markers_and_checks_access() {
    let w = world().await;
    for who in [w.ivy.partner, w.otto.partner] {
        ok(&w
            .call(
                w.ivy,
                DOC_A,
                "thread_follow",
                json!({"ids": [w.doc_shared], "partner_ids": [who]}),
            )
            .await);
    }
    ok(&w
        .call(
            w.pat,
            DOC_A,
            "thread_follow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.pat.partner]}),
        )
        .await);
    let (ivy_row, _) = follower(&w, DOC_A, w.doc_shared, w.ivy.partner)
        .await
        .unwrap();
    let (otto_row, _) = follower(&w, DOC_A, w.doc_shared, w.otto.partner)
        .await
        .unwrap();
    let (pat_row, _) = follower(&w, DOC_A, w.doc_shared, w.pat.partner)
        .await
        .unwrap();
    let removed = ok(&w
        .call(
            w.ivy,
            DOC_A,
            "thread_unfollow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.ivy.partner, w.otto.partner]}),
        )
        .await);
    let payload = decode(&removed["store"]);
    let mut deleted: Vec<(String, u64)> = payload
        .deletes()
        .iter()
        .map(|r| (r.model().as_str().to_owned(), r.id()))
        .collect();
    deleted.sort();
    assert_eq!(
        deleted,
        vec![
            ("mail.followers".to_owned(), ivy_row as u64),
            ("mail.followers".to_owned(), otto_row as u64)
        ]
    );
    assert!(payload.upserts().is_empty());
    assert!(
        follower(&w, DOC_A, w.doc_shared, w.ivy.partner)
            .await
            .is_none()
    );
    assert!(
        follower(&w, DOC_A, w.doc_shared, w.otto.partner)
            .await
            .is_none()
    );
    // a portal user cannot remove another partner, and removes itself
    let response = w
        .call(
            w.pat,
            DOC_A,
            "thread_unfollow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.ivy.partner]}),
        )
        .await;
    assert_error(
        &response,
        403,
        ACCESS,
        "Portal users can only follow, unfollow or set the subtypes of their own partner.",
    );
    let own = ok(&w
        .call(
            w.pat,
            DOC_A,
            "thread_unfollow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.pat.partner]}),
        )
        .await);
    assert_eq!(decode(&own["store"]).deletes().len(), 1);
    assert_eq!(decode(&own["store"]).deletes()[0].id(), pat_row as u64);
    // an internal user without write access removes itself, not others
    ok(&w
        .call(
            w.ivy,
            DOC_R,
            "thread_follow",
            json!({"ids": [w.doc_r], "partner_ids": [w.ivy.partner]}),
        )
        .await);
    let response = w
        .call(
            w.ivy,
            DOC_R,
            "thread_unfollow",
            json!({"ids": [w.doc_r], "partner_ids": [w.otto.partner]}),
        )
        .await;
    assert_error(&response, 403, ACCESS, DOC_R_WRITE_DENIED);
    ok(&w
        .call(
            w.ivy,
            DOC_R,
            "thread_unfollow",
            json!({"ids": [w.doc_r], "partner_ids": [w.ivy.partner]}),
        )
        .await);
    assert!(follower(&w, DOC_R, w.doc_r, w.ivy.partner).await.is_none());
    w.cleanup().await;
}

/// The editor lists the visible, model-valid subtypes in order; changing a follower's subtypes
/// accepts only subtypes valid for the model; an empty list and a partner who does not follow are
/// refused with their messages.
#[tokio::test]
async fn set_subtypes_and_the_editor_list_accept_valid_subtypes_only() {
    let w = world().await;
    ok(&w
        .call(
            w.ivy,
            DOC_A,
            "thread_follow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.ivy.partner]}),
        )
        .await);
    let listed = ok(&w
        .call(
            w.ivy,
            DOC_A,
            "thread_subtypes",
            json!({"ids": [w.doc_shared]}),
        )
        .await);
    assert_eq!(
        ids(&listed["subtype_ids"]),
        vec![w.comment, w.a_news, w.a_secret, w.note],
        "sequence order, the subtype of the other model left out"
    );
    let payload = decode(&listed["store"]);
    assert!(
        payload.asof().own().is_none(),
        "a read answer has no own xid"
    );
    let fields = record(&payload, "mail.message.subtype", w.a_secret);
    assert_eq!(fields["name"], &Wire::String("A Secret".to_owned()));
    assert_eq!(fields["internal"], &Wire::Bool(true));
    // change to the subtypes of the model
    let changed = ok(&w
        .call(
            w.ivy,
            DOC_A,
            "thread_set_subtypes",
            json!({"ids": [w.doc_shared], "partner_id": w.ivy.partner, "subtype_ids": [w.comment, w.a_secret]}),
        )
        .await);
    let (row_id, stored) = follower(&w, DOC_A, w.doc_shared, w.ivy.partner)
        .await
        .unwrap();
    assert_eq!(stored, sorted(vec![w.comment, w.a_secret]));
    assert_eq!(
        record(&decode(&changed["store"]), "mail.followers", row_id)["subtype_ids"],
        &Wire::X2ManyReplace(stored.iter().map(|id| *id as u64).collect())
    );
    let refusals = [
        (
            json!({"ids": [w.doc_shared], "partner_id": w.ivy.partner, "subtype_ids": [w.r_news]}),
            422,
            VALIDATION,
            format!(
                "Unknown subtype ids for {DOC_A}: {}. A subtype must exist and have no model or \
                 the model of the record.",
                w.r_news
            ),
        ),
        (
            json!({"ids": [w.doc_shared], "partner_id": w.ivy.partner, "subtype_ids": []}),
            422,
            VALIDATION,
            "Choose at least one subtype, or unfollow.".to_owned(),
        ),
        (
            json!({"ids": [w.doc_shared], "partner_id": w.otto.partner, "subtype_ids": [w.comment]}),
            404,
            "erp.exceptions.MissingError",
            format!(
                "Partner {} does not follow {DOC_A} {}.",
                w.otto.partner, w.doc_shared
            ),
        ),
    ];
    for (body, status, name, message) in refusals {
        let response = w.call(w.ivy, DOC_A, "thread_set_subtypes", body).await;
        assert_error(&response, status, name, &message);
    }
    assert_eq!(
        follower(&w, DOC_A, w.doc_shared, w.ivy.partner)
            .await
            .unwrap()
            .1,
        stored,
        "the refused calls changed nothing"
    );
    w.cleanup().await;
}

#[tokio::test]
async fn a_portal_caller_cannot_choose_an_internal_subtype() {
    let w = world().await;
    let listed = ok(&w
        .call(
            w.pat,
            DOC_A,
            "thread_subtypes",
            json!({"ids": [w.doc_shared]}),
        )
        .await);
    assert_eq!(
        ids(&listed["subtype_ids"]),
        vec![w.comment, w.a_news],
        "a portal caller sees the non-internal subtypes only"
    );
    ok(&w
        .call(
            w.pat,
            DOC_A,
            "thread_follow",
            json!({"ids": [w.doc_shared], "partner_ids": [w.pat.partner]}),
        )
        .await);
    let response = w
        .call(
            w.pat,
            DOC_A,
            "thread_set_subtypes",
            json!({"ids": [w.doc_shared], "partner_id": w.pat.partner, "subtype_ids": [w.a_secret]}),
        )
        .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "Unknown subtype ids for {DOC_A}: {}. A subtype must exist and have no model or the \
             model of the record.",
            w.a_secret
        ),
    );
    let response = w
        .call(
            w.pat,
            DOC_A,
            "thread_set_subtypes",
            json!({"ids": [w.doc_shared], "partner_id": w.ivy.partner, "subtype_ids": [w.comment]}),
        )
        .await;
    assert_error(
        &response,
        403,
        ACCESS,
        "Portal users can only follow, unfollow or set the subtypes of their own partner.",
    );
    let ok_change = ok(&w
        .call(
            w.pat,
            DOC_A,
            "thread_set_subtypes",
            json!({"ids": [w.doc_shared], "partner_id": w.pat.partner, "subtype_ids": [w.a_news]}),
        )
        .await);
    let (row, stored) = follower(&w, DOC_A, w.doc_shared, w.pat.partner)
        .await
        .unwrap();
    assert_eq!(stored, vec![w.a_news]);
    record(&decode(&ok_change["store"]), "mail.followers", row);
    w.cleanup().await;
}

/// The follower list is a keyset page for internal users: pages follow the follower id, the last
/// page has no `next_after`, a limit out of range is refused, a portal user is refused.
#[tokio::test]
async fn the_follower_page_is_keyset_and_internal_only() {
    let w = world().await;
    for who in [w.ivy.partner, w.otto.partner, w.hidden_partner] {
        ok(&w
            .call(
                w.ivy,
                DOC_A,
                "thread_follow",
                json!({"ids": [w.doc_shared], "partner_ids": [who]}),
            )
            .await);
    }
    let first = ok(&w
        .call(
            w.ivy,
            DOC_A,
            "thread_followers",
            json!({"ids": [w.doc_shared], "limit": 2}),
        )
        .await);
    let first_ids = ids(&first["follower_ids"]);
    assert_eq!(first_ids.len(), 2);
    assert!(first_ids[0] < first_ids[1], "oldest first");
    assert_eq!(first["next_after"], json!(first_ids[1]));
    let payload = decode(&first["store"]);
    let follower_rows = payload
        .upserts()
        .iter()
        .filter(|r| r.model().as_str() == "mail.followers")
        .count();
    assert_eq!(
        follower_rows, 2,
        "the page's followers, with their partners beside them"
    );
    assert!(payload.asof().own().is_none());
    let (ivy_row, _) = follower(&w, DOC_A, w.doc_shared, w.ivy.partner)
        .await
        .unwrap();
    assert_eq!(
        record(&payload, "mail.followers", first_ids[0])["partner_id"],
        &Wire::Many2One(Some(w.ivy.partner as u64))
    );
    assert_eq!(first_ids[0], ivy_row);
    let second = ok(&w
        .call(
            w.ivy,
            DOC_A,
            "thread_followers",
            json!({"ids": [w.doc_shared], "limit": 2, "after": first_ids[1]}),
        )
        .await);
    assert_eq!(ids(&second["follower_ids"]).len(), 1);
    assert_eq!(second["next_after"], serde_json::Value::Bool(false));
    for (body, message) in [
        (
            json!({"ids": [w.doc_shared], "limit": 0}),
            "A follower page holds 1 to 100 followers, not 0.",
        ),
        (
            json!({"ids": [w.doc_shared], "limit": 101}),
            "A follower page holds 1 to 100 followers, not 101.",
        ),
        (
            json!({"ids": [w.doc_shared], "limit": -3}),
            "A follower page holds 1 to 100 followers, not -3.",
        ),
        (
            json!({"ids": [w.doc_shared], "after": -1}),
            "A follower page starts after a follower id, not -1.",
        ),
    ] {
        let response = w.call(w.ivy, DOC_A, "thread_followers", body).await;
        assert_error(&response, 422, VALIDATION, message);
    }
    let response = w
        .call(
            w.pat,
            DOC_A,
            "thread_followers",
            json!({"ids": [w.doc_shared]}),
        )
        .await;
    assert_error(
        &response,
        403,
        ACCESS,
        "Only internal users can list followers.",
    );
    w.cleanup().await;
}

/// Extension: two addons override `message_post` of one model and a client call through JSON-2
/// runs both, the later addon outermost.
#[tokio::test]
async fn two_addons_extending_message_post_are_both_honoured_through_rpc() {
    let w = world().await;
    let answer = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "x", "subject": "ext:hello"}),
    )
    .await);
    let message = answer["message_id"].as_i64().unwrap();
    let stored: String = w
        .client
        .query_one(
            "SELECT subject FROM mail_message WHERE id = $1",
            &[&message],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        stored, "ext:hello|b|a",
        "the later addon wraps the earlier one"
    );
    let payload = decode(&answer["store"]);
    assert_eq!(
        record(&payload, "mail.message", message)["subject"],
        &Wire::String(stored)
    );
    // a subject without the marker is untouched
    let plain = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "y", "subject": "plain"}),
    )
    .await);
    let stored: String = w
        .client
        .query_one(
            "SELECT subject FROM mail_message WHERE id = $1",
            &[&plain["message_id"].as_i64().unwrap()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(stored, "plain");
    w.cleanup().await;
}

/// `false` for a key that is not a boolean is refused naming the key, like any other wrong type.
#[tokio::test]
async fn false_is_a_wrong_type_for_non_boolean_keys() {
    let w = world().await;
    let entry = format!("{DOC_A}.thread_post");
    for (key, message) in [
        (
            "body",
            "The post body is text or an object {\"text\": …}, not boolean.",
        ),
        ("subject", "The subject is text, not boolean."),
        ("message_type", "The message_type is text, not boolean."),
        (
            "subtype",
            "A subtype is comment, note, an external id 'module.name' or an id, not boolean.",
        ),
        ("parent_id", "The parent_id is an integer, not boolean."),
        (
            "partner_ids",
            "The partner_ids are a list of integers, not boolean.",
        ),
        (
            "mentions",
            "The mentions are a list of objects with start, end and partner_id, not boolean.",
        ),
        (
            "client_uid",
            "The client_uid is a UUID string, not boolean.",
        ),
    ] {
        let mut post_body = json!({"body": "x"});
        post_body[key] = json!(false);
        let response = post(&w, w.ivy, w.doc_shared, post_body).await;
        assert_error(
            &response,
            422,
            VALIDATION,
            &format!("argument 'post' of {entry}: {message}"),
        );
    }
    assert_eq!(messages(&w).await, 0);
    w.cleanup().await;
}

/// A portal caller may not post silently, and the parent the flat-thread rule picks for a portal
/// author is never an internal message, so an internal note's id does not reach its store payload.
#[tokio::test]
async fn a_portal_author_gets_no_internal_parent_and_cannot_post_silently() {
    let w = world().await;
    let response = post(
        &w,
        w.pat,
        w.doc_shared,
        json!({"body": "x", "silent": true}),
    )
    .await;
    assert_error(&response, 403, ACCESS, "Portal users cannot post silently.");
    let note = ok(&post(
        &w,
        w.ivy,
        w.doc_shared,
        json!({"body": "internal", "message_type": "notification", "subtype": "note"}),
    )
    .await)["message_id"]
        .as_i64()
        .unwrap();
    // automatic parent: the only message of the record is internal, so there is none
    let first = ok(&post(&w, w.pat, w.doc_shared, json!({"body": "a"})).await);
    let first_id = first["message_id"].as_i64().unwrap();
    let payload = decode(&first["store"]);
    assert_eq!(
        record(&payload, "mail.message", first_id)["parent_id"],
        &Wire::Many2One(None)
    );
    // an explicit internal parent is not taken either
    let second = ok(&post(
        &w,
        w.pat,
        w.doc_shared,
        json!({"body": "b", "parent_id": note}),
    )
    .await);
    let parent: Option<i64> = w
        .client
        .query_one(
            "SELECT parent_id::int8 FROM mail_message WHERE id = $1",
            &[&second["message_id"].as_i64().unwrap()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        parent,
        Some(first_id),
        "the internal parent is dropped and the newest visible comment is taken"
    );
    // a tracking message is not a parent either
    let tracking: i64 = w
        .client
        .query_one(
            "INSERT INTO mail_message (model, res_id, message_type, body, date, is_internal, silent) \
             VALUES ($1, $2, 'tracking', '', now(), false, false) RETURNING id::int8",
            &[&DOC_A, &w.doc_shared],
        )
        .await
        .unwrap()
        .get(0);
    let third = ok(&post(
        &w,
        w.pat,
        w.doc_shared,
        json!({"body": "t", "parent_id": tracking}),
    )
    .await);
    let parent: Option<i64> = w
        .client
        .query_one(
            "SELECT parent_id::int8 FROM mail_message WHERE id = $1",
            &[&third["message_id"].as_i64().unwrap()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(parent, Some(second["message_id"].as_i64().unwrap()));
    // an internal author's reply may use the internal note's thread: its parent is unrestricted
    let ivy = ok(&post(&w, w.ivy, w.doc_shared, json!({"body": "c"})).await);
    let ivy_parent: Option<i64> = w
        .client
        .query_one(
            "SELECT parent_id::int8 FROM mail_message WHERE id = $1",
            &[&ivy["message_id"].as_i64().unwrap()],
        )
        .await
        .unwrap()
        .get(0);
    assert!(ivy_parent.is_some());
    w.cleanup().await;
}

/// (f): a thread answer and the `schema` topic of `store_fetch` carry one and the same
/// generation, the registry's.
#[tokio::test]
async fn a_thread_answer_and_the_schema_topic_carry_the_same_generation() {
    let w = world().await;
    let posted = ok(&w
        .call(
            w.ivy,
            DOC_A,
            "thread_post",
            json!({"ids": [w.doc_shared], "post": {"body": "hello"}}),
        )
        .await);
    let fetched = ok(&w
        .call(
            w.ivy,
            "mail.store",
            "store_fetch",
            json!({"wire": 1, "topics": [{"name": "schema"}]}),
        )
        .await);
    let generation = fetched["topics"][0]["data"]["schema"].as_str().unwrap();
    assert_eq!(posted["store"]["schema"].as_str().unwrap(), generation);
    assert_eq!(fetched["store"]["schema"].as_str().unwrap(), generation);
    assert!(
        generation.starts_with("s1-"),
        "a content-derived token, never the retired literal: {generation}"
    );
    w.cleanup().await;
}

/// (e),: the store-answering thread reads are primary reads on every
/// model that inherits the mixin, and the write calls are not.
#[tokio::test]
async fn the_store_answering_thread_reads_are_primary_reads() {
    let w = world().await;
    let registry = w.h.registry();
    let table = w.orm.exposure();
    for model in ["mail.thread", DOC_A, DOC_R] {
        let id = registry
            .model_id(model)
            .unwrap_or_else(|| panic!("{model}"));
        for method in ["thread_followers", "thread_subtypes"] {
            match table.get(id, method) {
                Some(erp_sdk::rpc::Exposure::Exposed(e)) => {
                    assert!(e.primary_read(), "{model}.{method}");
                    assert_eq!(e.effect(), erp_sdk::rpc::Effect::Read);
                }
                other => panic!("{model}.{method} is not exposed: {other:?}"),
            }
        }
        for method in ["thread_post", "thread_follow", "thread_unfollow"] {
            match table.get(id, method) {
                Some(erp_sdk::rpc::Exposure::Exposed(e)) => {
                    assert!(!e.primary_read(), "{model}.{method}");
                }
                other => panic!("{model}.{method} is not exposed: {other:?}"),
            }
        }
    }
    w.cleanup().await;
}

// ----- the thread and thread.messages topics -------------------------------

const STORE: &str = "mail.store";
const THREAD_DENIED: &str =
    "You are not allowed to read this thread: it does not exist or you may not read it.";

/// `store_fetch` with `topics` as `who`.
async fn fetch(w: &World, who: Who, topics: serde_json::Value) -> RpcResponse {
    w.call(
        who,
        STORE,
        "store_fetch",
        json!({"wire": 1, "topics": topics}),
    )
    .await
}

async fn fetch_ok(w: &World, who: Who, topics: serde_json::Value) -> serde_json::Value {
    ok(&fetch(w, who, topics).await)
}

fn thread_messages(doc: &str, id: i64, params: serde_json::Value) -> serde_json::Value {
    let mut all = json!({"model": doc, "id": id});
    for (key, value) in params.as_object().unwrap() {
        all[key] = value.clone();
    }
    json!([{"name": "thread.messages", "params": all}])
}

/// The `data` of slot `index`, which must have answered.
#[track_caller]
fn slot_data(answer: &serde_json::Value, index: usize) -> &serde_json::Value {
    let slot = &answer["topics"][index];
    assert_eq!(slot["ok"], json!(true), "slot {index}: {slot}");
    &slot["data"]
}

/// A message row on `doc`; returns its id. `kind` is the message type.
async fn message(
    w: &World,
    model: &str,
    doc: i64,
    kind: &str,
    subject: &str,
    subtype: Option<i64>,
    internal: bool,
    author: Option<i64>,
) -> i64 {
    w.client
        .query_one(
            "INSERT INTO mail_message (model, res_id, message_type, body, subject, date, \
             is_internal, silent, subtype_id, author_id) \
             VALUES ($1, $2, $3, $4, $5, now(), $6, false, $7, $8) RETURNING id::int8",
            &[
                &model,
                &doc,
                &kind,
                &format!("<p>{subject}</p>"),
                &subject,
                &internal,
                &subtype,
                &author,
            ],
        )
        .await
        .unwrap()
        .get(0)
}

/// `count` comments on `doc` by ivy, oldest first; returns their ids.
async fn comments(w: &World, doc: i64, count: usize) -> Vec<i64> {
    let mut ids = Vec::new();
    for n in 0..count {
        ids.push(
            message(
                w,
                DOC_A,
                doc,
                "comment",
                &format!("m{n}"),
                Some(w.comment),
                false,
                Some(w.ivy.partner),
            )
            .await,
        );
    }
    ids
}

fn message_ids(data: &serde_json::Value) -> Vec<i64> {
    ids(&data["message_ids"])
}

fn newest_first(ids: &[i64]) -> Vec<i64> {
    ids.iter().rev().copied().collect()
}

/// The stored name of a partner.
async fn partner_name(w: &World, id: i64) -> String {
    w.client
        .query_one("SELECT name FROM res_partner WHERE id = $1", &[&id])
        .await
        .unwrap()
        .get(0)
}

/// The `res.partner` upsert ids of a payload.
fn partner_records(payload: &DecodedPayload) -> Vec<u64> {
    payload
        .upserts()
        .iter()
        .filter(|r| r.model().as_str() == "res.partner")
        .map(|r| r.id())
        .collect()
}

/// A thread answer carries the author's partner record with its name, once per distinct partner,
/// and leaves out a partner the caller may not read (the client then shows its placeholder).
#[tokio::test]
async fn a_thread_answer_carries_the_authors_names_and_omits_an_unreadable_partner() {
    let w = world().await;
    comments(&w, w.doc_shared, 3).await;
    message(
        &w,
        DOC_A,
        w.doc_shared,
        "comment",
        "by the hidden one",
        Some(w.comment),
        false,
        Some(w.hidden_partner),
    )
    .await;
    let answer = fetch_ok(&w, w.ivy, thread_messages(DOC_A, w.doc_shared, json!({}))).await;
    let payload = decode(&answer["store"]);
    let name = partner_name(&w, w.ivy.partner).await;
    assert_eq!(
        record(&payload, "res.partner", w.ivy.partner)["name"],
        &Wire::String(name),
        "the author's name comes from the answer"
    );
    assert_eq!(
        partner_records(&payload),
        vec![w.ivy.partner as u64],
        "three messages by one author: one record; the unreadable partner is omitted"
    );
}

/// The post answer carries the author and the recipients with their names.
#[tokio::test]
async fn a_post_answer_carries_the_author_and_recipient_names() {
    let w = world().await;
    let response = w
        .call(
            w.ivy,
            DOC_A,
            "thread_post",
            json!({"ids": [w.doc_shared], "post": {"body": "hi", "partner_ids": [w.otto.partner]}}),
        )
        .await;
    let payload = decode(&ok(&response)["store"]);
    for partner in [w.ivy.partner, w.otto.partner] {
        assert_eq!(
            record(&payload, "res.partner", partner)["name"],
            &Wire::String(partner_name(&w, partner).await)
        );
    }
}

#[tokio::test]
async fn thread_messages_first_page_is_newest_first_with_store_records() {
    let w = world().await;
    let all = comments(&w, w.doc_shared, 35).await;
    // other threads' messages and user notifications are never listed
    comments(&w, w.doc_secret, 3).await;
    let notice = message(
        &w,
        DOC_A,
        w.doc_shared,
        "user_notification",
        "n",
        None,
        false,
        Some(w.ivy.partner),
    )
    .await;
    w.client
        .execute(
            "INSERT INTO mail_message_res_partner_rel (mail_message_id, res_partner_id) \
             VALUES ($1, $2)",
            &[all.last().unwrap(), &w.otto.partner],
        )
        .await
        .unwrap();
    let answer = fetch_ok(&w, w.ivy, thread_messages(DOC_A, w.doc_shared, json!({}))).await;
    let data = slot_data(&answer, 0);
    assert_eq!(data["model"], json!(DOC_A));
    assert_eq!(data["id"], json!(w.doc_shared));
    let listed = message_ids(data);
    assert_eq!(
        listed,
        newest_first(&all[5..]),
        "30 by default, newest first"
    );
    assert!(!listed.contains(&notice));
    assert_eq!(data["has_more_before"], json!(true));
    assert_eq!(data["has_more_after"], json!(false));
    let payload = decode(&answer["store"]);
    let messages: Vec<_> = payload
        .upserts()
        .iter()
        .filter(|r| r.model().as_str() == "mail.message")
        .collect();
    assert_eq!(messages.len(), 30, "one record per listed id");
    // every record equals the stored row
    for id in &listed {
        let fields = record(&payload, "mail.message", *id);
        let row = w
            .client
            .query_one(
                "SELECT body, subject, message_type, subtype_id::int8, author_id::int8, \
                 parent_id::int8, is_internal, model, res_id::int8 FROM mail_message WHERE id = $1",
                &[id],
            )
            .await
            .unwrap();
        let body: String = row.get(0);
        let subject: String = row.get(1);
        let kind: String = row.get(2);
        let subtype: Option<i64> = row.get(3);
        let author: Option<i64> = row.get(4);
        let parent: Option<i64> = row.get(5);
        let internal: bool = row.get(6);
        let model: String = row.get(7);
        let res_id: i64 = row.get(8);
        assert_eq!(fields["body"], &Wire::Html(body));
        assert_eq!(fields["subject"], &Wire::String(subject));
        assert_eq!(fields["message_type"], &Wire::Selection(kind));
        assert_eq!(
            fields["subtype_id"],
            &Wire::Many2One(subtype.map(|s| s as u64))
        );
        assert_eq!(
            fields["author_id"],
            &Wire::Many2One(author.map(|s| s as u64))
        );
        assert_eq!(
            fields["parent_id"],
            &Wire::Many2One(parent.map(|s| s as u64))
        );
        assert_eq!(fields["is_internal"], &Wire::Bool(internal));
        assert_eq!(fields["res_model"], &Wire::String(model));
        assert_eq!(fields["res_id"], &Wire::Int(res_id));
        assert!(matches!(fields["date"], Wire::Datetime(_)));
    }
    // a page carries no recipients, even for a message that has some
    let newest = record(&payload, "mail.message", *all.last().unwrap());
    assert!(!newest.contains_key("partner_ids"));
    // the subtype the messages name travels with them
    let subtype = record(&payload, "mail.message.subtype", w.comment);
    let name: String = w
        .client
        .query_one(
            "SELECT name->>'en_US' FROM mail_message_subtype WHERE id = $1",
            &[&w.comment],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(subtype["name"], &Wire::String(name));
    assert_eq!(subtype["internal"], &Wire::Bool(false));
    // a smaller page, and the limit bounds
    let small = fetch_ok(
        &w,
        w.ivy,
        thread_messages(DOC_A, w.doc_shared, json!({"limit": 5})),
    )
    .await;
    assert_eq!(message_ids(slot_data(&small, 0)), newest_first(&all[30..]));
    for limit in [0, 101] {
        let r = fetch(
            &w,
            w.ivy,
            thread_messages(DOC_A, w.doc_shared, json!({"limit": limit})),
        )
        .await;
        assert_eq!(r.status(), 422, "{}", String::from_utf8_lossy(r.body()));
        let body = body_json(&r);
        assert_eq!(body["name"], VALIDATION);
        assert_eq!(
            body["message"],
            json!(format!(
                "store topic 'thread.messages': param 'limit' is {limit}; it must be between 1 and 100"
            ))
        );
    }
    w.cleanup().await;
}

fn to_value(json: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match json {
        J::Null => Value::False,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => Value::Int(n.as_i64().unwrap()),
        J::String(text) => Value::str(text),
        J::Array(items) => Value::list(items.iter().map(to_value)),
        J::Object(entries) => Value::Map(Arc::new(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), to_value(value)))
                .collect(),
        )),
    }
}

/// The statements `store_fetch` of `topics` sends as `who` (transaction control excluded), taken
/// from the transaction's own capture: the snapshot, the audience check and every topic read. The
/// call and every slot must have answered.
async fn statements(w: &World, who: Who, topics: &serde_json::Value) -> Vec<String> {
    // begun as the RPC service begins a primary read of a principal, so the statements are those
    // of the real route, the lazy resolution of the caller's groups included
    let mut first = w.orm.begin(UserId::new(who.uid).unwrap()).await.unwrap();
    let principal = first.env().principal().clone();
    first.rollback().await.unwrap();
    let opts = erp_orm::BeginOptions::for_principal(principal)
        .read_only(format!("{STORE}.store_fetch"))
        .on_primary();
    let mut tx = w.orm.begin_opts(opts).await.unwrap();
    tx.capture_statements(true);
    let env = tx.env();
    let answer: Value = env
        .records(STORE)
        .unwrap()
        .call(&mut tx, "store_fetch", (1_u64, to_value(topics)))
        .await
        .expect("the call answered");
    let sent = tx.captured().to_vec();
    tx.rollback().await.unwrap();
    // every slot answered ok before the statements are counted
    let Value::Map(map) = &answer else {
        panic!("store_fetch answers a map: {answer:?}")
    };
    let Some(Value::Json(slots)) = map.get("topics") else {
        panic!("no topics in the answer: {answer:?}")
    };
    for slot in slots.as_array().unwrap() {
        assert_eq!(slot["ok"], json!(true), "{slot}");
    }
    sent
}

/// The statements a call's topics sent: the snapshot and the one resolution of the caller's
/// groups, which the runner pays before the first topic (`StoreTopic::needs_groups`), excluded.
fn topic_statements(sent: &[String]) -> usize {
    sent.iter()
        .filter(|sql| !sql.starts_with("SELECT pg_current_snapshot()") && !is_resolution(sql))
        .count()
}

/// Assert the whole call of a fetch (M2 of the review): one snapshot, exactly one resolution of
/// the caller's groups, the topics' own statements within `topic_ceiling`, and so the whole call
/// within `topic_ceiling + 1` statements once the snapshot is excluded. Returns the topics'
/// statements.
fn whole_call(sent: &[String], topic_ceiling: usize, what: &str) -> usize {
    let snapshots = sent
        .iter()
        .filter(|sql| sql.starts_with("SELECT pg_current_snapshot()"))
        .count();
    assert_eq!(snapshots, 1, "{what}: snapshot statements in {sent:?}");
    let resolutions = sent.iter().filter(|sql| is_resolution(sql)).count();
    assert_eq!(resolutions, 1, "{what}: group resolutions in {sent:?}");
    let used = topic_statements(sent);
    assert!(
        used <= topic_ceiling,
        "{what}: {used} topic statements, ceiling {topic_ceiling}"
    );
    // `used` is defined as everything but the snapshot and the resolution, so the whole call is
    // `used + resolutions` by construction; the checks above (one snapshot, one resolution) and
    // the ceilings are the real assertions
    let total = sent.len() - snapshots;
    assert!(
        total <= topic_ceiling + 1,
        "{what}: whole call sent {total} statements, ceiling {}",
        topic_ceiling + 1
    );
    used
}

/// The statement that resolves the caller's groups and companies on first use.
fn is_resolution(sql: &str) -> bool {
    sql.contains("FROM res_groups_users_rel r WHERE r.uid = u.id")
}

#[tokio::test]
async fn thread_messages_pages_by_keyset_and_refuses_two_cursors() {
    let w = world().await;
    let all = comments(&w, w.doc_shared, 7).await;
    let page = |params: serde_json::Value| thread_messages(DOC_A, w.doc_shared, params);
    let get = |answer: &serde_json::Value| {
        let data = slot_data(answer, 0);
        (
            message_ids(data),
            data["has_more_before"].as_bool().unwrap(),
            data["has_more_after"].as_bool().unwrap(),
        )
    };
    // the newest three, then before the oldest of the page until the thread ends
    let first = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 3}))).await);
    assert_eq!(first, (newest_first(&all[4..]), true, false));
    let second = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 3, "before": all[4]}))).await);
    assert_eq!(second, (newest_first(&all[1..4]), true, true));
    let third = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 3, "before": all[1]}))).await);
    assert_eq!(third, (vec![all[0]], false, true));
    // a page of exactly the remaining messages has nothing before it
    let exact = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 4, "before": all[4]}))).await);
    assert_eq!(exact, (newest_first(&all[0..4]), false, true));
    // after: the nearest three above the cursor, still newest first
    let up = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 3, "after": all[0]}))).await);
    assert_eq!(up, (newest_first(&all[1..4]), true, true));
    let top = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 3, "after": all[3]}))).await);
    assert_eq!(top, (newest_first(&all[4..]), true, false));
    // nothing newer than the newest
    let none = get(&fetch_ok(&w, w.ivy, page(json!({"after": all[6]}))).await);
    assert_eq!(none, (vec![], true, false));
    // two cursors refuse the whole call
    let r = fetch(&w, w.ivy, page(json!({"before": all[4], "after": all[1]}))).await;
    assert_error(
        &r,
        422,
        VALIDATION,
        "thread.messages: pass at most one of before, after and around.",
    );
    let r = fetch(&w, w.ivy, page(json!({"around": all[4], "before": all[5]}))).await;
    assert_error(
        &r,
        422,
        VALIDATION,
        "thread.messages: pass at most one of before, after and around.",
    );
    w.cleanup().await;
}

#[tokio::test]
async fn thread_messages_around_a_message() {
    let w = world().await;
    let all = comments(&w, w.doc_shared, 10).await;
    let page = |params: serde_json::Value| thread_messages(DOC_A, w.doc_shared, params);
    let get = |answer: &serde_json::Value| {
        let data = slot_data(answer, 0);
        (
            message_ids(data),
            data["has_more_before"].as_bool().unwrap(),
            data["has_more_after"].as_bool().unwrap(),
        )
    };
    // m5 and m4 below, m6 and m7 above, newest first
    let four = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 4, "around": all[5]}))).await);
    assert_eq!(four, (vec![all[7], all[6], all[5], all[4]], true, true));
    // an odd limit gives the older side the extra message
    let five = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 5, "around": all[5]}))).await);
    assert_eq!(
        five,
        (vec![all[7], all[6], all[5], all[4], all[3]], true, true)
    );
    // a page of one holds the cursor message
    let one = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 1, "around": all[5]}))).await);
    assert_eq!(one, (vec![all[5]], true, true));
    // at the ends the page is shorter than the limit: each side takes its half only
    let low = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 4, "around": all[0]}))).await);
    assert_eq!(low, (vec![all[2], all[1], all[0]], false, true));
    let high = get(&fetch_ok(&w, w.ivy, page(json!({"limit": 4, "around": all[9]}))).await);
    assert_eq!(high, (vec![all[9], all[8]], true, false));
    // an id that is no message of the thread: the nearest older and newer ones, no error
    let far = get(&fetch_ok(
        &w,
        w.ivy,
        page(json!({"limit": 4, "around": all[9] + 1000})),
    )
    .await);
    assert_eq!(far, (vec![all[9], all[8]], true, false));
    w.cleanup().await;
}

#[tokio::test]
async fn an_unreadable_thread_is_denied_before_any_message_is_looked_at() {
    let w = world().await;
    comments(&w, w.doc_shared, 2).await;
    let secret = comments(&w, w.doc_secret, 3).await;
    // the portal user wrote on the secret thread and is a recipient of one of its messages
    let own = message(
        &w,
        DOC_A,
        w.doc_secret,
        "comment",
        "own",
        Some(w.comment),
        false,
        Some(w.pat.partner),
    )
    .await;
    let topics = json!([
        {"name": "messaging.self"},
        {"name": "thread.messages", "params": {"model": DOC_A, "id": w.doc_secret}},
        {"name": "thread", "params": {"model": DOC_A, "id": w.doc_secret}},
        {"name": "thread.messages", "params": {"model": DOC_A, "id": 999_999}},
        {"name": "thread", "params": {"model": DOC_A, "id": 999_999}},
        {"name": "thread.messages", "params": {"model": DOC_A, "id": w.doc_shared}},
    ]);
    let answer = fetch_ok(&w, w.pat, topics.clone()).await;
    let denied = |slot: &serde_json::Value, topic: &str| {
        assert_eq!(slot["topic"], json!(topic));
        assert_eq!(slot["ok"], json!(false), "{slot}");
        assert_eq!(slot["error"]["kind"], json!("access"));
        assert_eq!(slot["error"]["message"], json!(THREAD_DENIED));
    };
    assert_eq!(answer["topics"][0]["ok"], json!(true));
    denied(&answer["topics"][1], "thread.messages");
    denied(&answer["topics"][2], "thread");
    denied(&answer["topics"][3], "thread.messages");
    denied(&answer["topics"][4], "thread");
    // the thread the user may read still answers, and nothing of the secret thread is in the payload
    let shared = slot_data(&answer, 5);
    assert_eq!(message_ids(shared).len(), 2);
    let payload = decode(&answer["store"]);
    for id in secret.iter().chain([&own]) {
        assert!(
            !payload
                .upserts()
                .iter()
                .any(|r| r.model().as_str() == "mail.message" && r.id() == *id as u64),
            "message {id} of the unreadable thread is in the payload"
        );
    }
    // a public user is denied everywhere
    let answer = fetch_ok(&w, w.public, topics).await;
    for index in 1..=5 {
        let slot = &answer["topics"][index];
        assert_eq!(slot["ok"], json!(false), "{slot}");
        assert_eq!(slot["error"]["kind"], json!("access"));
        assert_eq!(
            slot["error"]["message"],
            json!("Public users cannot load a thread.")
        );
    }
    // a name that is no thread model refuses the whole call before anything is read
    for model in ["mail.message.subtype", "mail.thread", "no.such.model"] {
        let r = fetch(
            &w,
            w.ivy,
            json!([
                {"name": "messaging.self"},
                {"name": "thread.messages", "params": {"model": model, "id": 1}}
            ]),
        )
        .await;
        assert_error(
            &r,
            422,
            VALIDATION,
            &format!(
                "'{model}' is not a model with a message thread: it does not exist, is not \
                 installed or does not inherit mail.thread."
            ),
        );
    }
    w.cleanup().await;
}

#[tokio::test]
async fn a_portal_reader_sees_only_customer_visible_messages() {
    let w = world().await;
    let internal_subtype = w.note;
    let doc = w.doc_shared;
    let comment = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "comment",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let note = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "note",
        Some(internal_subtype),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let tracking = message(
        &w,
        DOC_A,
        doc,
        "tracking",
        "tracking",
        Some(w.a_news),
        false,
        None,
    )
    .await;
    let flagged = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "flagged",
        Some(w.comment),
        true,
        Some(w.ivy.partner),
    )
    .await;
    let outgoing = message(
        &w,
        DOC_A,
        doc,
        "email_outgoing",
        "outgoing",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let notice = message(
        &w,
        DOC_A,
        doc,
        "notification",
        "notice",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let page = thread_messages(DOC_A, doc, json!({}));
    let portal = fetch_ok(&w, w.pat, page.clone()).await;
    assert_eq!(message_ids(slot_data(&portal, 0)), vec![outgoing, comment]);
    let internal = fetch_ok(&w, w.ivy, page).await;
    assert_eq!(
        message_ids(slot_data(&internal, 0)),
        vec![notice, outgoing, flagged, tracking, note, comment]
    );
    // a model that overrides the type list changes what the portal user sees
    let on_p = [
        message(
            &w,
            DOC_P,
            w.doc_p,
            "comment",
            "p-comment",
            Some(w.comment),
            false,
            Some(w.ivy.partner),
        )
        .await,
        message(
            &w,
            DOC_P,
            w.doc_p,
            "email_outgoing",
            "p-mail",
            Some(w.comment),
            false,
            Some(w.ivy.partner),
        )
        .await,
    ];
    let shared_p = fetch_ok(&w, w.pat, thread_messages(DOC_P, w.doc_p, json!({}))).await;
    assert_eq!(message_ids(slot_data(&shared_p, 0)), vec![on_p[1]]);
    let internal_p = fetch_ok(&w, w.ivy, thread_messages(DOC_P, w.doc_p, json!({}))).await;
    assert_eq!(
        message_ids(slot_data(&internal_p, 0)),
        vec![on_p[1], on_p[0]]
    );
    w.cleanup().await;
}

/// A follower row of `partner` on `(model, doc)` with `subtypes`; returns its id.
async fn add_follower(w: &World, model: &str, doc: i64, partner: i64, subtypes: &[i64]) -> i64 {
    let id: i64 = w
        .client
        .query_one(
            "INSERT INTO mail_followers (res_model, res_id, partner_id) VALUES ($1, $2, $3) \
             RETURNING id::int8",
            &[&model, &doc, &partner],
        )
        .await
        .unwrap()
        .get(0);
    for subtype in subtypes {
        w.client
            .execute(
                "INSERT INTO mail_followers_mail_message_subtype_rel \
                 (mail_followers_id, mail_message_subtype_id) VALUES ($1, $2)",
                &[&id, subtype],
            )
            .await
            .unwrap();
    }
    id
}

fn thread_topic(params: serde_json::Value) -> serde_json::Value {
    json!([{"name": "thread", "params": params}])
}

#[tokio::test]
async fn the_thread_topic_answers_facts_first_page_and_followers() {
    let w = world().await;
    let doc = w.doc_shared;
    let all = comments(&w, doc, 4).await;
    let f_otto = add_follower(&w, DOC_A, doc, w.otto.partner, &[w.comment, w.a_news]).await;
    let f_pat = add_follower(&w, DOC_A, doc, w.pat.partner, &[w.comment]).await;
    let f_hidden = add_follower(&w, DOC_A, doc, w.hidden_partner, &[]).await;
    // another record's follower and one more follower than the page holds
    add_follower(&w, DOC_A, w.doc_secret, w.otto.partner, &[w.comment]).await;
    let f_ivy = add_follower(&w, DOC_A, doc, w.ivy.partner, &[w.note]).await;
    let answer = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({
            "model": DOC_A, "id": doc, "messages": true, "limit": 3,
            "followers": true, "followers_limit": 2, "default_subject": true
        })),
    )
    .await;
    let data = slot_data(&answer, 0);
    assert_eq!(
        data["thread"],
        json!({
            "model": DOC_A, "id": doc, "can_write": true, "can_post": true,
            "default_subject": "shared"
        })
    );
    assert_eq!(
        data["messages"],
        json!({
            "message_ids": newest_first(&all[1..]),
            "has_more_before": true, "has_more_after": false
        })
    );
    assert_eq!(
        data["followers"],
        json!({
            "count": 4,
            "follower_ids": [f_otto, f_pat],
            "self_follower_id": f_ivy,
            "next_after": f_pat,
        })
    );
    // next_after only while more followers exist than the page holds
    let all_four = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({
            "model": DOC_A, "id": doc, "messages": false, "followers": true, "followers_limit": 4
        })),
    )
    .await;
    let panel = &slot_data(&all_four, 0)["followers"];
    assert_eq!(
        panel["follower_ids"],
        json!([f_otto, f_pat, f_hidden, f_ivy])
    );
    assert_eq!(panel["next_after"], json!(null));
    let payload = decode(&answer["store"]);
    for (id, partner, subtypes) in [
        (f_otto, w.otto.partner, vec![w.comment, w.a_news]),
        (f_pat, w.pat.partner, vec![w.comment]),
        (f_ivy, w.ivy.partner, vec![w.note]),
    ] {
        let fields = record(&payload, "mail.followers", id);
        assert_eq!(fields["res_model"], &Wire::String(DOC_A.to_owned()));
        assert_eq!(fields["res_id"], &Wire::Int(doc));
        assert_eq!(fields["partner_id"], &Wire::Many2One(Some(partner as u64)));
        let mut wanted: Vec<u64> = subtypes.iter().map(|s| *s as u64).collect();
        wanted.sort_unstable();
        assert_eq!(fields["subtype_ids"], &Wire::X2ManyReplace(wanted));
    }
    assert!(
        !payload
            .upserts()
            .iter()
            .any(|r| r.model().as_str() == "mail.followers" && r.id() == f_hidden as u64),
        "the third follower is outside the page"
    );
    // the default subject is the record name cut at a word boundary, a single-record name as is
    let long = "word ".repeat(40);
    let named = new_doc(&w, DOC_A, &long).await;
    let answer = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({"model": DOC_A, "id": named, "default_subject": true})),
    )
    .await;
    let subject = slot_data(&answer, 0)["thread"]["default_subject"]
        .as_str()
        .unwrap();
    assert!(subject.ends_with("..."), "{subject}");
    assert!(subject.chars().count() <= 100, "{subject}");
    assert_eq!(subject, format!("{}...", "word ".repeat(19).trim_end()));
    // without the messages panel the data has no `messages` key and the store no message
    let bare = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({"model": DOC_A, "id": doc, "messages": false})),
    )
    .await;
    let data = slot_data(&bare, 0);
    assert!(data.get("messages").is_none() && data.get("followers").is_none());
    assert!(data["thread"].get("default_subject").is_none());
    assert!(
        decode(&bare["store"])
            .upserts()
            .iter()
            .all(|r| r.model().as_str() != "mail.message")
    );
    // a user who may only read the record: no write, and the post access follows the model
    let read_only = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({"model": DOC_R, "id": w.doc_r, "messages": false})),
    )
    .await;
    assert_eq!(
        slot_data(&read_only, 0)["thread"],
        json!({
            "model": DOC_R, "id": w.doc_r, "can_write": false, "can_post": false,
            "ext_note": "r"
        })
    );
    w.cleanup().await;
}

/// A record of `model` named `name`, committed; returns its id.
async fn new_doc(w: &World, model: &str, name: &str) -> i64 {
    let mut tx = w.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let id = create(&mut tx, model, RawVals::new().set_cell("name", text(name))).await;
    tx.commit().await.unwrap();
    id
}

#[tokio::test]
async fn unavailable_panels_refuse_by_name() {
    let w = world().await;
    for (panel, owner) in [
        ("activities", "the mail.activity slice"),
        ("scheduled_messages", "the scheduled-messages slice"),
        ("suggested_recipients", "the suggested-recipients slice"),
        (
            "contact_fields",
            "the chatter contact-fields slice (gap G-20)",
        ),
        ("has_pinned_messages", "the message-pin slice (gap G-20)"),
    ] {
        let r = fetch(
            &w,
            w.ivy,
            json!([
                {"name": "messaging.self"},
                {"name": "thread", "params": {"model": DOC_A, "id": w.doc_shared, panel: true}}
            ]),
        )
        .await;
        assert_error(
            &r,
            422,
            VALIDATION,
            &format!("thread: the panel '{panel}' is not available yet; it comes with {owner}."),
        );
        assert!(!String::from_utf8_lossy(r.body()).contains("\"store\""));
        // asked for as false, a panel is accepted
        let answer = fetch_ok(
            &w,
            w.ivy,
            thread_topic(json!({"model": DOC_A, "id": w.doc_shared, panel: false})),
        )
        .await;
        assert_eq!(answer["topics"][0]["ok"], json!(true));
    }
    // a portal user may not list followers: denied in the slot, the other topics answer
    let answer = fetch_ok(
        &w,
        w.pat,
        json!([
            {"name": "messaging.self"},
            {"name": "thread", "params": {"model": DOC_A, "id": w.doc_shared, "followers": true}},
            {"name": "thread", "params": {"model": DOC_A, "id": w.doc_shared}}
        ]),
    )
    .await;
    assert_eq!(answer["topics"][0]["ok"], json!(true));
    assert_eq!(answer["topics"][1]["ok"], json!(false));
    assert_eq!(answer["topics"][1]["error"]["kind"], json!("access"));
    assert_eq!(
        answer["topics"][1]["error"]["message"],
        json!("Only internal users can list followers.")
    );
    assert_eq!(answer["topics"][2]["ok"], json!(true));
    w.cleanup().await;
}

#[tokio::test]
async fn the_message_and_thread_entries_are_dispatched() {
    let w = world().await;
    let doc = w.doc_shared;
    let plain = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "plain",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let hidden = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "redact: secret",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let answer = fetch_ok(&w, w.ivy, thread_messages(DOC_A, doc, json!({}))).await;
    let payload = decode(&answer["store"]);
    // both overrides ran through `sup`, the later one on the result of the earlier one
    assert_eq!(
        record(&payload, "mail.message", hidden)["subject"],
        &Wire::String("[redacted]|b".to_owned())
    );
    assert_eq!(
        record(&payload, "mail.message", plain)["subject"],
        &Wire::String("plain".to_owned())
    );
    // a thread fact an override adds reaches the slot
    let r = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({"model": DOC_R, "id": w.doc_r, "messages": false})),
    )
    .await;
    assert_eq!(slot_data(&r, 0)["thread"]["ext_note"], json!("r"));
    // a base fact set again fails the call and names the fact
    let r = fetch(
        &w,
        w.ivy,
        thread_topic(json!({"model": DOC_P, "id": w.doc_p, "messages": false})),
    )
    .await;
    let text = String::from_utf8_lossy(r.body()).into_owned();
    assert_eq!(r.status(), 500, "{text}");
    assert_eq!(
        body_json(&r)["message"],
        json!(
            "the thread fact 'can_post' is set twice; an override adds facts, it does not \
             replace the base ones"
        )
    );
    // a field the store schema does not declare fails the call and names the topic
    let bad = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "unknown-field: x",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let r = fetch(&w, w.ivy, thread_messages(DOC_A, doc, json!({}))).await;
    let text = String::from_utf8_lossy(r.body()).into_owned();
    assert_eq!(r.status(), 500, "{text}");
    let message_text = body_json(&r)["message"].as_str().unwrap().to_owned();
    assert!(
        message_text.starts_with(
            "store topic 'thread.messages' produced a record the store schema refuses: "
        ),
        "{message_text}"
    );
    assert!(message_text.contains("nope"), "{message_text}");
    assert!(!text.contains("\"store\""), "nothing partial is answered");
    let _ = bad;
    w.cleanup().await;
}

#[tokio::test]
async fn the_budgets_hold_and_one_generation_is_shared() {
    let w = world().await;
    let doc = w.doc_shared;
    let all = comments(&w, doc, 40).await;
    let mut partners = vec![
        w.otto.partner,
        w.pat.partner,
        w.hidden_partner,
        w.ivy.partner,
    ];
    partners.push(w.public.partner);
    for partner in partners {
        add_follower(&w, DOC_A, doc, partner, &[w.comment]).await;
    }
    assert_eq!(erp_addon_mail::thread_load::THREAD_TOPIC.budget(), 11);
    assert_eq!(
        erp_addon_mail::thread_load::THREAD_MESSAGES_TOPIC.budget(),
        6
    );
    let mut seen = Vec::new();
    for who in [w.ivy, w.pat] {
        for (label, limit) in [("small", 1), ("big", 100)] {
            for (cursor, params) in [
                ("latest", json!({})),
                ("before", json!({"before": all[30]})),
                ("after", json!({"after": all[5]})),
                ("around", json!({"around": all[20]})),
            ] {
                let mut all_params = params.clone();
                all_params["limit"] = json!(limit);
                let topics = thread_messages(DOC_A, doc, all_params);
                // `around` runs two searches, so its ceiling is one statement higher
                let ceiling = if cursor == "around" { 7 } else { 6 };
                let used = whole_call(
                    &statements(&w, who, &topics).await,
                    ceiling,
                    &format!("thread.messages {cursor} {label}"),
                );
                seen.push((who.uid, cursor, label, used));
            }
        }
    }
    // the statements do not depend on the page size
    for pair in seen.chunks(8).map(|c| (&c[..4], &c[4..])) {
        for (small, big) in pair.0.iter().zip(pair.1) {
            assert_eq!(small.3, big.3, "{small:?} against {big:?}");
        }
    }
    let panels = json!({"model": DOC_A, "id": doc, "followers": true, "default_subject": true});
    let used = whole_call(
        &statements(&w, w.ivy, &thread_topic(panels)).await,
        11,
        "thread all panels",
    );
    seen.push((w.ivy.uid, "thread", "all panels", used));
    let used = whole_call(
        &statements(
            &w,
            w.pat,
            &thread_topic(json!({"model": DOC_A, "id": doc, "default_subject": true})),
        )
        .await,
        11,
        "thread as portal",
    );
    seen.push((w.pat.uid, "thread", "portal", used));
    eprintln!("measured statements (user, call, size, topic statements): {seen:?}");
    // one schema generation for a page, a post answer and the schema topic
    let page = fetch_ok(&w, w.ivy, thread_messages(DOC_A, doc, json!({}))).await;
    let schema = fetch_ok(&w, w.ivy, json!([{"name": "schema"}])).await;
    let generation = schema["topics"][0]["data"]["schema"].as_str().unwrap();
    assert_eq!(page["store"]["schema"].as_str().unwrap(), generation);
    let posted = ok(&post(&w, w.ivy, doc, json!({"body": "generation"})).await);
    assert_eq!(posted["store"]["schema"].as_str().unwrap(), generation);
    // a topic over its declared budget fails the call with `Programming`, answering nothing
    let r = fetch(
        &w,
        w.ivy,
        json!([{"name": "messaging.self"}, {"name": "test.tiny"}]),
    )
    .await;
    let text = String::from_utf8_lossy(r.body()).into_owned();
    assert_eq!(r.status(), 500, "{text}");
    assert_eq!(
        body_json(&r)["message"],
        json!("store topic 'test.tiny' sent 1 statements; its budget is 0")
    );
    assert!(!text.contains("\"store\""), "{text}");
    w.cleanup().await;
}

fn tiny_run<'a>(
    tx: &'a mut Transaction,
    _input: &'a TopicInput<'a>,
    _out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        tx.query("SELECT 1", &[]).await?;
        Ok(())
    })
}

erp_addon_mail::store_topic!(
    /// A topic that sends one statement under a budget of none.
    TINY_TOPIC = StoreTopic::new("test_rpc_mail", "test.tiny", tiny_run)
        .audience(Audience::LoggedIn)
        .statements(0)
);

const DEEP_PARAMS: &[erp_addon_mail::store_fetch::ParamDecl] =
    &[erp_addon_mail::store_fetch::ParamDecl::boolean("deep").optional()];

erp_addon_mail::store_topic!(
    /// A topic that sends one statement; its budget is none, one more with `deep`.
    TINY_PLUS_TOPIC = StoreTopic::new("test_rpc_mail", "test.tiny_plus", tiny_run)
        .audience(Audience::LoggedIn)
        .params(DEEP_PARAMS)
        .statements(0)
        .statements_plus(&[("deep", 1)])
);

/// The ceiling of a shape: a parameter named in `statements_plus` raises it for the calls that
/// pass it and for no others.
#[tokio::test]
async fn statements_plus_raises_the_ceiling_of_the_named_shape_only() {
    let w = world().await;
    assert_eq!(TINY_PLUS_TOPIC.budget(), 0);
    let r = fetch(
        &w,
        w.ivy,
        json!([{"name": "test.tiny_plus", "params": {"deep": true}}]),
    )
    .await;
    ok(&r);
    // passed as false it is still passed: the shape is chosen by the parameter, not its value
    let r = fetch(
        &w,
        w.ivy,
        json!([{"name": "test.tiny_plus", "params": {"deep": false}}]),
    )
    .await;
    ok(&r);
    let r = fetch(&w, w.ivy, json!([{"name": "test.tiny_plus"}])).await;
    assert_error(
        &r,
        500,
        "erp.programming",
        "store topic 'test.tiny_plus' sent 1 statements; its budget is 0",
    );
    w.cleanup().await;
}

/// A thread model that declares its own `_to_store(StoreFields)` next to the mixin's
/// `_store_thread_fields(ThreadFacts)`: each is dispatched on its own signature.
#[tokio::test]
async fn a_thread_model_may_declare_its_own_to_store() {
    let w = world().await;
    let mut tx = w.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let record = tx.env().records(DOC_R).unwrap().browse([w.doc_r]).unwrap();
    let fields: StoreFields = record
        .call(&mut tx, "_to_store", (StoreFields::new(),))
        .await
        .unwrap();
    assert_eq!(
        fields.get(DOC_R, 1, "marker"),
        Some(&Wire::Bool(true)),
        "the model's own entry hook ran"
    );
    let facts: ThreadFacts = record
        .call(
            &mut tx,
            "_store_thread_fields",
            (ThreadFacts::new(["messages"]),),
        )
        .await
        .unwrap();
    assert_eq!(facts.get("ext_note"), Some(&json!("r")));
    assert_eq!(facts.get("can_write"), Some(&json!(true)));
    tx.rollback().await.unwrap();
    w.cleanup().await;
}

// ----- files on messages --------------------------------------------

const COMPOSER: &str = "mail.compose.message";

/// A composer file of `who` (placeholder model, record 0), created through the attachment service.
async fn composer_file(w: &World, who: Who, name: &str) -> i64 {
    let mut tx = w.orm.begin(UserId::new(who.uid).unwrap()).await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![
            NewAttachment::new(name, AttachmentContent::Bytes(name.as_bytes().to_vec()))
                .pending_in(COMPOSER),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    ids[0].get()
}

/// A file attached to record `(model, res_id)` by the superuser through the service; `field`
/// binds it to a field of the record.
async fn record_file(w: &World, model: &str, res_id: i64, name: &str, field: Option<&str>) -> i64 {
    let mut tx = w.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let mut item = NewAttachment::new(name, AttachmentContent::Bytes(name.as_bytes().to_vec()))
        .on(ParentRef::new(model, res_id));
    if let Some(field) = field {
        item = item.field(field);
    }
    let ids = attachments::create(&mut tx, vec![item]).await.unwrap();
    tx.commit().await.unwrap();
    ids[0].get()
}

/// `(res_model, res_id, create_uid, name, checksum, file_size, mimetype)` of a file, raw.
async fn file_row(
    w: &World,
    id: i64,
) -> (
    Option<String>,
    Option<i64>,
    Option<i64>,
    String,
    Option<String>,
    Option<i32>,
    Option<String>,
) {
    let row = w
        .client
        .query_one(
            "SELECT res_model, res_id::int8, create_uid::int8, name, checksum, file_size, mimetype \
             FROM ir_attachment WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    (
        row.get(0),
        row.get(1),
        row.get(2),
        row.get(3),
        row.get(4),
        row.get(5),
        row.get(6),
    )
}

/// The files linked to `message`, ascending.
async fn linked(w: &World, message: i64) -> Vec<i64> {
    w.client
        .query(
            "SELECT attachment_id::int8 FROM message_attachment_rel WHERE message_id = $1 \
             ORDER BY 1",
            &[&message],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

fn as_u64s(ids: &[i64]) -> Vec<u64> {
    ids.iter().map(|id| *id as u64).collect()
}

/// An internal user posts three files (two of their composer files, one already on the record,
/// one id twice); a portal user posts their composer file; a post without ids links nothing; a
/// retried post links nothing again.
#[tokio::test]
async fn message_post_links_files_and_moves_composer_files_onto_the_record() {
    let w = world().await;
    let doc = w.doc_shared;
    let first = composer_file(&w, w.ivy, "first.txt").await;
    let second = composer_file(&w, w.ivy, "second.txt").await;
    let on_record = record_file(&w, DOC_A, doc, "on-record.txt", None).await;
    for id in [first, second] {
        assert_eq!(file_row(&w, id).await.0.as_deref(), Some(COMPOSER));
    }
    let before: Vec<_> = {
        let mut rows = Vec::new();
        for id in [first, second, on_record] {
            rows.push(file_row(&w, id).await);
        }
        rows
    };
    let answer = ok(&post(
        &w,
        w.ivy,
        doc,
        json!({"body": "with files", "attachment_ids": [second, first, on_record, first]}),
    )
    .await);
    let message = answer["message_id"].as_i64().unwrap();
    let mut expected = vec![first, second, on_record];
    expected.sort_unstable();
    assert_eq!(
        linked(&w, message).await,
        expected,
        "one row per distinct id"
    );
    // the composer files belong to the record now; nothing else about them changed
    for (index, id) in [first, second, on_record].into_iter().enumerate() {
        let after = file_row(&w, id).await;
        assert_eq!(after.0.as_deref(), Some(DOC_A));
        assert_eq!(after.1, Some(doc));
        let mut moved = before[index].clone();
        moved.0 = after.0.clone();
        moved.1 = after.1;
        assert_eq!(after, moved, "only res_model and res_id of {id} moved");
    }
    assert_eq!(before[2].0.as_deref(), Some(DOC_A), "already on the record");
    // the answer's message record lists the files, marked as written by the call
    let payload = decode(&answer["store"]);
    let fields = record(&payload, "mail.message", message);
    assert_eq!(
        fields["attachment_ids"],
        &Wire::X2ManyReplace(as_u64s(&expected))
    );
    let written: Vec<&str> = payload
        .upserts()
        .iter()
        .filter(|r| r.model().as_str() == "mail.message")
        .flat_map(|r| r.fields())
        .filter(|f| f.written())
        .map(|f| f.name().as_str())
        .collect();
    assert!(written.contains(&"attachment_ids"), "{written:?}");
    let file = record(&payload, "ir.attachment", first);
    assert_eq!(file["name"], &Wire::String("first.txt".to_owned()));

    // a portal user posts their own composer file although they have no right on the file table
    let own_file = composer_file(&w, w.pat, "own.txt").await;
    let answer = ok(&post(
        &w,
        w.pat,
        doc,
        json!({"body": "from the portal", "attachment_ids": [own_file]}),
    )
    .await);
    let portal_message = answer["message_id"].as_i64().unwrap();
    assert_eq!(linked(&w, portal_message).await, vec![own_file]);
    let row = file_row(&w, own_file).await;
    assert_eq!((row.0.as_deref(), row.1), (Some(DOC_A), Some(doc)));
    assert_eq!(row.2, Some(w.pat.uid), "the creator stays the portal user");

    // a post without ids links nothing and its answer says the message has no file
    let relations = w.n("SELECT count(*) FROM message_attachment_rel").await;
    let answer = ok(&post(&w, w.ivy, doc, json!({"body": "plain"})).await);
    let plain = answer["message_id"].as_i64().unwrap();
    assert_eq!(
        w.n("SELECT count(*) FROM message_attachment_rel").await,
        relations
    );
    let payload = decode(&answer["store"]);
    assert_eq!(
        record(&payload, "mail.message", plain)["attachment_ids"],
        &Wire::X2ManyReplace(Vec::new())
    );

    // a retried post links nothing again and ignores the ids it names
    let pending = composer_file(&w, w.ivy, "kept-pending.txt").await;
    let uid = "5a6c1d9e-7b1f-4a54-8f0e-0c9d6b2a7e11";
    let kept = composer_file(&w, w.ivy, "kept.txt").await;
    let first_try = ok(&post(
        &w,
        w.ivy,
        doc,
        json!({"body": "once", "client_uid": uid, "attachment_ids": [kept]}),
    )
    .await);
    let retry = ok(&post(
        &w,
        w.ivy,
        doc,
        json!({"body": "once", "client_uid": uid, "attachment_ids": [kept, pending]}),
    )
    .await);
    assert_eq!(retry["replayed"], json!(true));
    assert_eq!(retry["message_id"], first_try["message_id"]);
    let stored = first_try["message_id"].as_i64().unwrap();
    assert_eq!(linked(&w, stored).await, vec![kept]);
    assert_eq!(
        file_row(&w, pending).await.0.as_deref(),
        Some(COMPOSER),
        "a replayed post does not claim the files it names"
    );
    let payload = decode(&retry["store"]);
    assert_eq!(
        record(&payload, "mail.message", stored)["attachment_ids"],
        &Wire::X2ManyReplace(vec![kept as u64])
    );
    w.cleanup().await;
}

/// What a caller may not link refuses the whole post by name and writes nothing.
#[tokio::test]
async fn files_the_caller_may_not_link_refuse_the_post_and_write_nothing() {
    let w = world().await;
    let doc = w.doc_shared;
    let others_composer = composer_file(&w, w.otto, "others.txt").await;
    let on_other_record = record_file(&w, DOC_A, w.doc_secret, "elsewhere.txt", None).await;
    let field_bound = record_file(&w, DOC_A, doc, "bound.txt", Some("name")).await;
    let missing = 9_999_999_i64;
    let on_this_record = record_file(&w, DOC_A, doc, "already-here.txt", None).await;
    for (who, label) in [(w.ivy, "internal"), (w.pat, "portal")] {
        let mine = composer_file(&w, who, &format!("mine-{label}.txt")).await;
        let mut refused = vec![
            ("another user's composer file", others_composer),
            ("a file of another record", on_other_record),
            ("a field-bound file", field_bound),
            ("a missing id", missing),
        ];
        // an internal caller may name a file already on the record; one outside
        // the internal group may name only their own composer files
        if label == "portal" {
            refused.push(("a file already on the record", on_this_record));
        }
        for (what, id) in refused {
            let response = post(
                &w,
                who,
                doc,
                json!({"body": "refused", "attachment_ids": [mine, id]}),
            )
            .await;
            let allowed = if label == "portal" {
                "Only files you uploaded for this purpose can be used."
            } else {
                "Only files you uploaded for this purpose and files already on the record can be \
                 used."
            };
            assert_error(
                &response,
                403,
                ACCESS,
                &format!("These attachments cannot be linked here: [{id}]. {allowed}"),
            );
            assert_eq!(messages(&w).await, 0, "{label}: {what} wrote a message");
            assert_eq!(
                w.n("SELECT count(*) FROM message_attachment_rel").await,
                0,
                "{label}: {what} wrote a relation row"
            );
            assert_eq!(
                file_row(&w, mine).await.0.as_deref(),
                Some(COMPOSER),
                "{label}: {what} moved the valid file"
            );
        }
    }
    let before = file_row(&w, others_composer).await;
    assert_eq!(before.0.as_deref(), Some(COMPOSER));
    // sizes and shapes
    let entry = format!("{DOC_A}.thread_post");
    let response = post(&w, w.ivy, doc, json!({"body": "x", "attachment_ids": [0]})).await;
    assert_error(
        &response,
        422,
        VALIDATION,
        "An attachment id must be a positive integer, not 0.",
    );
    let response = post(
        &w,
        w.ivy,
        doc,
        json!({"body": "x", "attachment_ids": (1..=101).collect::<Vec<i64>>()}),
    )
    .await;
    assert_error(
        &response,
        422,
        VALIDATION,
        "At most 100 attachments are posted with one message, not 101.",
    );
    let response = post(&w, w.ivy, doc, json!({"body": "x", "attachment_ids": "12"})).await;
    assert_error(
        &response,
        422,
        VALIDATION,
        &format!(
            "argument 'post' of {entry}: The attachment_ids are a list of integers, not string."
        ),
    );
    assert_eq!(messages(&w).await, 0);
    assert_eq!(file_row(&w, others_composer).await, before);
    w.cleanup().await;
}

/// Their own composer file is still claimed.
#[tokio::test]
async fn a_file_already_on_a_record_needs_write_access_to_the_record() {
    let w = world().await;
    let contact = w.otto.partner;
    let on_record = record_file(&w, "res.partner", contact, "on-contact.txt", None).await;
    let own = composer_file(&w, w.ivy, "own-contact.txt").await;
    let call = |ids: Vec<i64>| {
        w.call(
            w.ivy,
            "res.partner",
            "thread_post",
            json!({"ids": [contact], "post": {"body": "from a reader", "attachment_ids": ids}}),
        )
    };
    let refused = call(vec![own, on_record]).await;
    let text = String::from_utf8_lossy(refused.body()).into_owned();
    assert_eq!(refused.status(), 403, "{text}");
    assert_eq!(body_json(&refused)["name"], ACCESS, "{text}");
    assert!(text.contains("(Operation: write)"), "{text}");
    assert_eq!(messages(&w).await, 0, "a message was written");
    assert_eq!(
        file_row(&w, own).await.0.as_deref(),
        Some(COMPOSER),
        "the own file moved although the post was refused"
    );
    // their own composer file alone is fine: the right that matters is the right to post
    let answer = ok(&call(vec![own]).await);
    let message = answer["message_id"].as_i64().unwrap();
    assert_eq!(linked(&w, message).await, vec![own]);
    let row = file_row(&w, own).await;
    assert_eq!(
        (row.0.as_deref(), row.1),
        (Some("res.partner"), Some(contact))
    );
    w.cleanup().await;
}

/// Link `file` to `message` as the post path would, by raw SQL (a privileged writer's link: the
/// test needs links the post path refuses to make).
async fn link_file(w: &World, message: i64, file: i64) {
    w.client
        .execute(
            "INSERT INTO message_attachment_rel (message_id, attachment_id) VALUES ($1, $2)",
            &[&message, &file],
        )
        .await
        .unwrap();
}

/// Whether the payload holds a record of `model` with `id`.
fn has_record(payload: &DecodedPayload, model: &str, id: i64) -> bool {
    payload
        .upserts()
        .iter()
        .any(|r| r.model().as_str() == model && r.id() == id as u64)
}

/// The `attachment_ids` the payload's message record carries.
fn carried(payload: &DecodedPayload, message: i64) -> Vec<u64> {
    match record(payload, "mail.message", message)["attachment_ids"] {
        Wire::X2ManyReplace(ids) => ids.clone(),
        other => panic!("attachment_ids of {message} is not a replace list: {other:?}"),
    }
}

#[tokio::test]
async fn message_pages_carry_the_files_posted_with_them() {
    let w = world().await;
    let doc = w.doc_shared;
    let f1 = record_file(&w, DOC_A, doc, "one.txt", None).await;
    let f2 = record_file(&w, DOC_A, doc, "two.txt", None).await;
    let f3 = record_file(&w, DOC_A, doc, "note.txt", None).await;
    let elsewhere = record_file(&w, DOC_A, w.doc_secret, "elsewhere.txt", None).await;
    let with_two = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "two files",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let foreign = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "foreign file",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let none = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "no file",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let internal_note = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "note",
        Some(w.note),
        false,
        Some(w.ivy.partner),
    )
    .await;
    // linked out of order on purpose: the message lists its files ascending
    link_file(&w, with_two, f2).await;
    link_file(&w, with_two, f1).await;
    link_file(&w, foreign, elsewhere).await;
    link_file(&w, internal_note, f3).await;

    for request in [
        thread_messages(DOC_A, doc, json!({})),
        thread_topic(json!({"model": DOC_A, "id": doc})),
    ] {
        let answer = fetch_ok(&w, w.ivy, request).await;
        let payload = decode(&answer["store"]);
        assert_eq!(carried(&payload, with_two), vec![f1 as u64, f2 as u64]);
        assert_eq!(carried(&payload, foreign), Vec::<u64>::new());
        assert_eq!(carried(&payload, none), Vec::<u64>::new());
        assert_eq!(carried(&payload, internal_note), vec![f3 as u64]);
        // the file of another record is not carried at all
        assert!(!has_record(&payload, "ir.attachment", elsewhere));
        // a file record is the stored row's metadata and nothing else
        for id in [f1, f2, f3] {
            let row = file_row(&w, id).await;
            let fields = record(&payload, "ir.attachment", id);
            assert_eq!(fields["name"], &Wire::String(row.3.clone()));
            assert_eq!(fields["mimetype"], &Wire::String(row.6.clone().unwrap()));
            assert_eq!(fields["file_size"], &Wire::Int(i64::from(row.5.unwrap())));
            assert_eq!(fields["checksum"], &Wire::String(row.4.clone().unwrap()));
            assert_eq!(fields["type"], &Wire::Selection("binary".to_owned()));
            assert_eq!(fields["url"], &Wire::String(String::new()));
            assert_eq!(
                fields.len(),
                6,
                "name, mimetype, file_size, checksum, type, url: {fields:?}"
            );
        }
    }

    // the portal reader sees the files of the messages its rules list, not the note's
    let answer = fetch_ok(&w, w.pat, thread_messages(DOC_A, doc, json!({}))).await;
    assert_eq!(
        message_ids(slot_data(&answer, 0)),
        vec![none, foreign, with_two]
    );
    let payload = decode(&answer["store"]);
    assert_eq!(carried(&payload, with_two), vec![f1 as u64, f2 as u64]);
    assert!(has_record(&payload, "ir.attachment", f1));
    assert!(
        !has_record(&payload, "ir.attachment", f3),
        "the note's file"
    );
    assert!(!has_record(&payload, "mail.message", internal_note));
    w.cleanup().await;
}

/// A binary file without a size, or a non-empty one without a checksum, is an invariant failure;
/// an empty file legitimately has no checksum.
#[tokio::test]
async fn a_damaged_file_row_fails_the_page_loudly() {
    let w = world().await;
    let doc = w.doc_shared;
    async fn page(w: &World, doc: i64) -> RpcResponse {
        fetch(w, w.ivy, thread_messages(DOC_A, doc, json!({}))).await
    }
    let m = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "damaged",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let file = record_file(&w, DOC_A, doc, "damaged.txt", None).await;
    link_file(&w, m, file).await;
    let ok_page = page(&w, doc).await;
    assert_eq!(
        ok_page.status(),
        200,
        "{}",
        String::from_utf8_lossy(ok_page.body())
    );
    // an empty file has no checksum and is fine
    w.client
        .execute(
            "UPDATE ir_attachment SET checksum = NULL, file_size = 0 WHERE id = $1",
            &[&file],
        )
        .await
        .unwrap();
    let empty = page(&w, doc).await;
    assert_eq!(
        empty.status(),
        200,
        "{}",
        String::from_utf8_lossy(empty.body())
    );
    // a non-empty file without a checksum is damaged
    w.client
        .execute(
            "UPDATE ir_attachment SET file_size = 5 WHERE id = $1",
            &[&file],
        )
        .await
        .unwrap();
    let r = page(&w, doc).await;
    let text = String::from_utf8_lossy(r.body()).into_owned();
    assert!(r.status() >= 500, "{text}");
    assert!(
        text.contains(&format!(
            "the non-empty binary attachment {file} has no checksum: the attachment service sets it"
        )),
        "{text}"
    );
    // a binary file without a size is damaged
    w.client
        .execute(
            "UPDATE ir_attachment SET file_size = NULL WHERE id = $1",
            &[&file],
        )
        .await
        .unwrap();
    let r = page(&w, doc).await;
    let text = String::from_utf8_lossy(r.body()).into_owned();
    assert!(r.status() >= 500, "{text}");
    assert!(
        text.contains(&format!(
            "the binary attachment {file} has no file_size: the attachment service sets it"
        )),
        "{text}"
    );
    w.cleanup().await;
}

#[tokio::test]
async fn the_attachments_panel_and_the_attachment_count() {
    let w = world().await;
    let doc = w.doc_shared;
    let a1 = record_file(&w, DOC_A, doc, "a1.txt", None).await;
    let a2 = record_file(&w, DOC_A, doc, "a2.txt", None).await;
    let a3 = record_file(&w, DOC_A, doc, "a3.txt", None).await;
    let bound = record_file(&w, DOC_A, doc, "bound.txt", Some("name")).await;
    let elsewhere = record_file(&w, DOC_A, w.doc_secret, "elsewhere.txt", None).await;
    let request = |extra: serde_json::Value| {
        let mut params = json!({"model": DOC_A, "id": doc, "messages": false});
        for (key, value) in extra.as_object().unwrap() {
            params[key] = value.clone();
        }
        thread_topic(params)
    };
    for who in [w.ivy, w.pat] {
        let answer = fetch_ok(&w, who, request(json!({"attachments": true}))).await;
        let data = slot_data(&answer, 0);
        assert_eq!(
            ids(&data["attachments"]["attachment_ids"]),
            vec![a3, a2, a1]
        );
        assert_eq!(data["attachments"]["has_more"], json!(false));
        assert_eq!(data["thread"]["attachment_count"], json!(3));
        let payload = decode(&answer["store"]);
        for id in [a1, a2, a3] {
            assert!(has_record(&payload, "ir.attachment", id));
        }
        assert!(
            !has_record(&payload, "ir.attachment", bound),
            "a field-bound file"
        );
        assert!(!has_record(&payload, "ir.attachment", elsewhere));
    }
    // a limit lists the newest files and says more exist; the count stays exact
    let answer = fetch_ok(
        &w,
        w.ivy,
        request(json!({"attachments": true, "attachments_limit": 2})),
    )
    .await;
    let data = slot_data(&answer, 0);
    assert_eq!(ids(&data["attachments"]["attachment_ids"]), vec![a3, a2]);
    assert_eq!(data["attachments"]["has_more"], json!(true));
    assert_eq!(data["thread"]["attachment_count"], json!(3));
    assert!(!has_record(&decode(&answer["store"]), "ir.attachment", a1));
    // without the panel: no data, no fact, no file in the store
    for extra in [
        json!({}),
        json!({"attachments": false, "attachments_limit": 2}),
    ] {
        let answer = fetch_ok(&w, w.ivy, request(extra)).await;
        let data = slot_data(&answer, 0);
        assert!(data.get("attachments").is_none(), "{data}");
        assert!(data["thread"].get("attachment_count").is_none(), "{data}");
        let payload = decode(&answer["store"]);
        assert!(!has_record(&payload, "ir.attachment", a3));
    }
    // a record without files: an empty panel and a count of zero
    let bare = new_doc(&w, DOC_A, "bare").await;
    let answer = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({"model": DOC_A, "id": bare, "messages": false, "attachments": true})),
    )
    .await;
    let data = slot_data(&answer, 0);
    assert_eq!(
        data["attachments"],
        json!({"attachment_ids": [], "has_more": false})
    );
    assert_eq!(data["thread"]["attachment_count"], json!(0));
    // the limit is bounded by the parameter declaration
    for limit in [0, 101] {
        let r = fetch(
            &w,
            w.ivy,
            request(json!({"attachments": true, "attachments_limit": limit})),
        )
        .await;
        assert_error(
            &r,
            422,
            VALIDATION,
            &format!(
                "store topic 'thread': param 'attachments_limit' is {limit}; it must be between 1 and 100"
            ),
        );
    }
    // an unreadable record is denied in its slot, with no file in the answer
    let answer = fetch_ok(
        &w,
        w.pat,
        thread_topic(json!({"model": DOC_A, "id": w.doc_secret, "attachments": true})),
    )
    .await;
    assert_eq!(answer["topics"][0]["ok"], json!(false));
    assert_eq!(
        answer["topics"][0]["error"]["message"],
        json!(THREAD_DENIED)
    );
    assert!(!String::from_utf8_lossy(&serde_json::to_vec(&answer).unwrap()).contains("elsewhere"));
    w.cleanup().await;
}

/// A message with `body` on `doc` of the shared record, by raw insert; `subtype` as given.
async fn message_with_body(w: &World, doc: i64, body: &str, subtype: i64) -> i64 {
    w.client
        .query_one(
            "INSERT INTO mail_message (model, res_id, message_type, body, subject, date, \
             is_internal, silent, subtype_id, author_id) \
             VALUES ($1, $2, 'comment', $3, 's', now(), false, false, $4, $5) RETURNING id::int8",
            &[&DOC_A, &doc, &body, &subtype, &w.ivy.partner],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn non_internal_readers_do_not_see_empty_messages() {
    let w = world().await;
    let doc = w.doc_shared;
    // `mail.mt_comment` has no description; give one subtype a description
    w.client
        .execute(
            "UPDATE mail_message_subtype SET description = '{\"en_US\": \"News\"}'::jsonb WHERE id = $1",
            &[&w.a_news],
        )
        .await
        .unwrap();
    let empty = message_with_body(&w, doc, "", w.comment).await;
    let empty_with_file = message_with_body(&w, doc, "", w.comment).await;
    let text = message_with_body(&w, doc, "<p>text</p>", w.comment).await;
    let described = message_with_body(&w, doc, "", w.a_news).await;
    let edited_only = message_with_body(
        &w,
        doc,
        "<span class=\"o-mail-Message-edited\"></span>",
        w.comment,
    )
    .await;
    let file = record_file(&w, DOC_A, doc, "only-a-file.txt", None).await;
    link_file(&w, empty_with_file, file).await;
    let page = thread_messages(DOC_A, doc, json!({}));
    let portal = fetch_ok(&w, w.pat, page.clone()).await;
    assert_eq!(
        message_ids(slot_data(&portal, 0)),
        vec![described, text, empty_with_file]
    );
    let internal = fetch_ok(&w, w.ivy, page).await;
    assert_eq!(
        message_ids(slot_data(&internal, 0)),
        vec![edited_only, described, text, empty_with_file, empty]
    );
    w.cleanup().await;
}

/// How many of `sent` contain `needle` (the page's files: the join to `ir_attachment`; the panel:
/// its window count).
fn file_statements(sent: &[String], needle: &str) -> usize {
    sent.iter().filter(|sql| sql.contains(needle)).count()
}

#[tokio::test]
async fn files_add_one_statement_per_page_and_one_per_panel() {
    let w = world().await;
    let doc = w.doc_shared;
    let all = comments(&w, doc, 40).await;
    for partner in [
        w.otto.partner,
        w.pat.partner,
        w.hidden_partner,
        w.ivy.partner,
    ] {
        add_follower(&w, DOC_A, doc, partner, &[w.comment]).await;
    }
    let mut observed = Vec::new();
    let mut totals = Vec::new();
    // 0 files, then one file on every fifth message, then three files on each of them and 40 on
    // the record: the counts must not move
    for (label, per_message, on_record) in
        [("none", 0_usize, 0_usize), ("one", 1, 0), ("many", 3, 40)]
    {
        if per_message > 0 {
            for message in all.iter().step_by(5) {
                for n in 0..per_message {
                    let file =
                        record_file(&w, DOC_A, doc, &format!("{label}-{message}-{n}"), None).await;
                    link_file(&w, *message, file).await;
                }
            }
        }
        for n in 0..on_record {
            record_file(&w, DOC_A, doc, &format!("record-{n}"), None).await;
        }
        for who in [w.ivy, w.pat] {
            for (size, limit) in [("small", 1), ("big", 100)] {
                for (cursor, params) in [
                    ("latest", json!({})),
                    ("before", json!({"before": all[30]})),
                    ("after", json!({"after": all[5]})),
                    ("around", json!({"around": all[20]})),
                ] {
                    let mut params = params.clone();
                    params["limit"] = json!(limit);
                    let sent = statements(&w, who, &thread_messages(DOC_A, doc, params)).await;
                    let ceiling = if cursor == "around" { 7 } else { 6 };
                    let used = whole_call(
                        &sent,
                        ceiling,
                        &format!("thread.messages {cursor} {size} {label}"),
                    );
                    assert_eq!(
                        file_statements(&sent, "JOIN ir_attachment a"),
                        1,
                        "the files of a page cost one statement ({cursor} {size} {label})"
                    );
                    observed.push((who.uid, label, cursor, size, used));
                    totals.push((who.uid, label, cursor, size, sent.len() - 1));
                }
            }
            let panels = json!({
                "model": DOC_A, "id": doc, "followers": who.uid == w.ivy.uid,
                "default_subject": true, "attachments": true
            });
            let sent = statements(&w, who, &thread_topic(panels)).await;
            let used = whole_call(&sent, 11, &format!("thread all panels {label}"));
            assert_eq!(file_statements(&sent, "JOIN ir_attachment a"), 1);
            assert_eq!(
                file_statements(&sent, "count(*) OVER ()"),
                1,
                "the panel costs one statement"
            );
            observed.push((who.uid, label, "thread", "all panels", used));
            totals.push((who.uid, label, "thread", "all panels", sent.len() - 1));
        }
    }
    // the counts depend on the caller and the cursor, never on the files
    let key = |o: &(i64, &str, &str, &str, usize)| (o.0, o.2.to_owned(), o.3.to_owned());
    for first in observed.iter().filter(|o| o.1 == "none") {
        for other in observed.iter().filter(|o| key(o) == key(first)) {
            assert_eq!(first.4, other.4, "{first:?} against {other:?}");
        }
    }
    eprintln!(
        "measured statements with files (user, files, call, size, topic statements): {observed:?}"
    );
    eprintln!(
        "statements of the whole call, snapshot excluded, the caller's group resolution \
         included (user, files, call, size, statements): {totals:?}"
    );
    w.cleanup().await;
}

#[tokio::test]
async fn deleting_a_message_unlinks_its_files_by_the_record_they_belong_to() {
    let w = world().await;
    let doc = new_doc(&w, DOC_A, "to delete").await;
    let on_record = record_file(&w, DOC_A, doc, "on-record.txt", None).await;
    let first = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "first",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let second = message(
        &w,
        DOC_A,
        doc,
        "comment",
        "second",
        Some(w.comment),
        false,
        Some(w.ivy.partner),
    )
    .await;
    let on_message = record_file(&w, "mail.message", first, "kept-on-message.txt", None).await;
    link_file(&w, first, on_record).await;
    link_file(&w, first, on_message).await;
    link_file(&w, second, on_record).await;
    let exists = |id: i64| format!("SELECT count(*) FROM ir_attachment WHERE id = {id}");
    // deleting the first message
    let mut tx = w.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.env()
        .records("mail.message")
        .unwrap()
        .browse([first])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(linked(&w, first).await, Vec::<i64>::new());
    assert_eq!(w.n(&exists(on_record)).await, 1, "the record's file stays");
    assert_eq!(
        linked(&w, second).await,
        vec![on_record],
        "and the other message lists it"
    );
    assert_eq!(
        w.n(&exists(on_message)).await,
        0,
        "the message's own file goes with it"
    );
    // deleting the record deletes its messages and every file, those of its messages included
    let on_second = record_file(&w, "mail.message", second, "second-own.txt", None).await;
    let mut tx = w.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.env()
        .records(DOC_A)
        .unwrap()
        .browse([doc])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(w.n(&exists(on_record)).await, 0);
    assert_eq!(w.n(&exists(on_second)).await, 0);
    assert_eq!(
        w.n(&format!(
            "SELECT count(*) FROM mail_message WHERE res_id = {doc} AND model = '{DOC_A}'"
        ))
        .await,
        0
    );
    assert_eq!(
        w.n("SELECT count(*) FROM message_attachment_rel").await,
        0,
        "no relation row outlives its message"
    );
    w.cleanup().await;
}

/// What a browser sends for the file `hello.txt` (`hello world`, base64): the request shapes the
/// chatter client uses, so a client call to a method or field this server lacks fails here too.
const HELLO_B64: &str = "aGVsbG8gd29ybGQ=";

async fn upload_for(w: &World, who: Who, method: &str, doc: i64, data: &str) -> RpcResponse {
    w.call(
        who,
        DOC_A,
        method,
        json!({"ids": [doc], "name": "hello.txt", "mimetype": "text/plain", "data": data}),
    )
    .await
}

/// The composer flow of the chatter client end to end: upload (pending, the caller's alone), post
/// naming the file, then the thread load returns it with the message; and the attachments panel
/// flow: attach to the record, load it in the panel.
#[tokio::test]
async fn the_client_uploads_posts_and_loads_a_file_through_the_rpc_service() {
    let w = world().await;
    let doc = w.doc_shared;
    let up = ok(&upload_for(&w, w.ivy, "thread_upload", doc, HELLO_B64).await);
    let file = up["attachment_id"].as_i64().unwrap();
    assert_eq!(up["name"], json!("hello.txt"));
    assert_eq!(up["mimetype"], json!("text/plain"));
    assert_eq!(up["file_size"], json!(11));
    assert_eq!(up["checksum"].as_str().unwrap().len(), 40);
    let row = file_row(&w, file).await;
    assert_eq!(
        (row.0.as_deref(), row.1),
        (Some(COMPOSER), None),
        "no record yet"
    );
    assert_eq!(row.2, Some(w.ivy.uid), "the uploader owns the pending file");
    // the post claims it
    let posted = ok(&post(
        &w,
        w.ivy,
        doc,
        json!({"body": "with a file", "attachment_ids": [file]}),
    )
    .await);
    let message = posted["message_id"].as_i64().unwrap();
    assert_eq!(linked(&w, message).await, vec![file]);
    // the thread load returns the message with its file, and the file as a store record
    let answer = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({"model": DOC_A, "id": doc, "messages": true, "attachments": true})),
    )
    .await;
    let payload = decode(&answer["store"]);
    assert!(has_record(&payload, "ir.attachment", file));
    let fields = record(&payload, "mail.message", message);
    assert_eq!(
        fields["attachment_ids"],
        &Wire::X2ManyReplace(as_u64s(&[file]))
    );
    assert_eq!(
        ids(&slot_data(&answer, 0)["attachments"]["attachment_ids"]),
        vec![file]
    );
    // the panel flow: a file attached to the record itself
    let up = ok(&upload_for(&w, w.ivy, "thread_attach", doc, HELLO_B64).await);
    let panel = up["attachment_id"].as_i64().unwrap();
    let row = file_row(&w, panel).await;
    assert_eq!((row.0.as_deref(), row.1), (Some(DOC_A), Some(doc)));
    let answer = fetch_ok(
        &w,
        w.ivy,
        thread_topic(json!({"model": DOC_A, "id": doc, "messages": false, "attachments": true})),
    )
    .await;
    assert_eq!(
        ids(&slot_data(&answer, 0)["attachments"]["attachment_ids"]),
        vec![panel, file]
    );
    assert_eq!(
        slot_data(&answer, 0)["thread"]["attachment_count"],
        json!(2)
    );
    w.cleanup().await;
}

/// Uploads are bounded and checked: a public caller, a caller who may not post (or write) the
/// record, bad base64, content above the inline threshold and a fifty-first waiting file are each
/// refused by name, and nothing is stored.
#[tokio::test]
async fn uploads_are_access_checked_and_bounded() {
    let w = world().await;
    let doc = w.doc_shared;
    let files = || w.n("SELECT count(*) FROM ir_attachment");
    let response = upload_for(&w, w.public, "thread_upload", doc, HELLO_B64).await;
    assert_error(
        &response,
        403,
        ACCESS,
        "Public users cannot upload files to a thread.",
    );
    let response = upload_for(&w, w.pat, "thread_upload", w.doc_secret, HELLO_B64).await;
    assert_eq!(
        response.status(),
        403,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    // a record the caller may read but not write: neither a waiting file nor a panel file
    for method in ["thread_upload", "thread_attach"] {
        let response = w
            .call(
                w.ivy,
                DOC_R,
                method,
                json!({"ids": [w.doc_r], "name": "hello.txt", "data": HELLO_B64}),
            )
            .await;
        assert_eq!(
            response.status(),
            403,
            "{method}: {}",
            String::from_utf8_lossy(response.body())
        );
    }
    let response = upload_for(&w, w.ivy, "thread_upload", doc, "not base64!").await;
    assert!(
        response.status() >= 400,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let big = "QUFB".repeat(2000);
    let response = upload_for(&w, w.ivy, "thread_upload", doc, &big).await;
    let text = String::from_utf8_lossy(response.body()).into_owned();
    assert_eq!(response.status(), 422, "{text}");
    assert!(text.contains("stream it with"), "{text}");
    assert_eq!(files().await, 0, "a refused upload stored nothing");
    for n in 0..50 {
        composer_file(&w, w.ivy, &format!("f{n}.txt")).await;
    }
    let response = upload_for(&w, w.ivy, "thread_upload", doc, HELLO_B64).await;
    assert_error(
        &response,
        422,
        VALIDATION,
        "At most 50 uploaded files wait for a message; post or remove some first.",
    );
    assert_eq!(files().await, 50);
    // another user's count is their own
    ok(&upload_for(&w, w.otto, "thread_upload", doc, HELLO_B64).await);
    w.cleanup().await;
}

/// A file uploaded for a composer and never posted is removed by the staging cleaner after a day;
/// a younger one, and a file of any other model with record id 0, stay.
#[tokio::test]
async fn an_abandoned_composer_file_is_collected() {
    let w = world().await;
    let old = composer_file(&w, w.ivy, "old.txt").await;
    let young = composer_file(&w, w.ivy, "young.txt").await;
    let other = record_file(&w, DOC_A, w.doc_shared, "kept.txt", None).await;
    w.client
        .execute(
            "UPDATE ir_attachment SET create_date = now() - interval '25 hours' WHERE id = ANY($1)",
            &[&vec![old, other]],
        )
        .await
        .unwrap();
    let mut tx = w.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let report = erp_addon_base::attachments::gc::clean_staging(&mut tx, &env, 100)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(report.abandoned, 1);
    let left: Vec<i64> = w
        .client
        .query("SELECT id FROM ir_attachment ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(left, vec![young, other]);
    w.cleanup().await;
}
