// SPDX-License-Identifier: MIT OR Apache-2.0
//! MAIL package setup and independent authorization expectations. No live policy builds the oracle.
#![allow(
    dead_code,
    reason = "each acceptance binary uses a subset of shared MAIL helpers"
)]

use crate::multiuser_fixture::{MultiuserFixture, Subject, error};
use crate::native_http::Reply;
use serde_json::{Value, json};

/// Positive int4 identity deliberately outside the fixture's generated record set.
pub const NO_SUCH_ID: i64 = 2_147_483_647;
pub const OUTSIDE: &str = "This seems to be a multi-company issue, but you do not have access to the proper company to access the record anyhow.";

/// MAIL requests never provision credentials. Print only structured errors, never success bodies
/// or headers; this makes a setup failure distinguishable from an authorization reproducer.
#[track_caller]
pub fn success(reply: &Reply) -> Value {
    if reply.status != 200 {
        let body: Value = serde_json::from_str(&reply.body).expect("MAIL error JSON");
        panic!(
            "MAIL request HTTP {}: {}: {}",
            reply.status, body["name"], body["message"]
        );
    }
    crate::multiuser_fixture::success(reply)
}

pub fn bytes() -> &'static str {
    "hello world"
}

/// Disposable records of the installed profile. Products prove parent access before child probes.
pub struct Mail<'a, 'n> {
    pub f: &'a MultiuserFixture<'n>,
    pub a: i64,
    pub b: i64,
    pub shared: i64,
    pub partner_a: i64,
}

impl<'a, 'n> Mail<'a, 'n> {
    pub fn new(f: &'a MultiuserFixture<'n>) -> Self {
        let create = |name: &str, company: Value| {
            success(&f.rpc(
                Subject::Admin,
                "product.template",
                "create",
                json!({"vals_list":[{"name":name,"company_id":company}]}),
            ))[0]
                .as_i64()
                .unwrap()
        };
        let a = create("MAIL product A", json!(f.user(Subject::A).company));
        let b = create("MAIL product B", json!(f.user(Subject::B).company));
        let shared = create("MAIL shared product", json!(null));
        let partner = success(&f.rpc(
            Subject::Admin,
            "res.users",
            "read",
            json!({"ids":[f.user(Subject::A).id],"fields":["partner_id"]}),
        ));
        let partner_a = relation(&partner[0]["partner_id"]);
        let m = Self {
            f,
            a,
            b,
            shared,
            partner_a,
        };
        for (subject, own, hidden) in [(Subject::A, a, b), (Subject::B, b, a)] {
            assert_eq!(
                m.search(subject, "product.template", &[a, b, shared]),
                if subject == Subject::A {
                    json!([a, shared])
                } else {
                    json!([b, shared])
                }
            );
            assert_eq!(
                m.read(subject, "product.template", &[own], &["name"])[0]["id"],
                own
            );
            assert_eq!(
                m.read(subject, "product.template", &[shared], &["name"])[0]["id"],
                shared
            );
            record_denied(
                f,
                &f.rpc(
                    subject,
                    "product.template",
                    "read",
                    json!({"ids":[hidden],"fields":["name"]}),
                ),
                subject,
                "product.template",
                "Product",
                "read",
                true,
            );
        }
        m
    }

    pub fn create(&self, model: &str, vals: Value) -> i64 {
        success(
            &self
                .f
                .rpc(Subject::Admin, model, "create", json!({"vals_list":[vals]})),
        )[0]
        .as_i64()
        .unwrap()
    }
    pub fn write(&self, subject: Subject, model: &str, ids: &[i64], vals: Value) {
        success(
            &self
                .f
                .rpc(subject, model, "write", json!({"ids":ids,"vals":vals})),
        );
    }
    pub fn read(&self, subject: Subject, model: &str, ids: &[i64], fields: &[&str]) -> Value {
        success(
            &self
                .f
                .rpc(subject, model, "read", json!({"ids":ids,"fields":fields})),
        )
    }
    pub fn search(&self, subject: Subject, model: &str, ids: &[i64]) -> Value {
        // An empty list explicitly means the whole disposable fixture table for rollback checks.
        let domain = if ids.is_empty() {
            json!([])
        } else {
            json!([["id", "in", ids]])
        };
        success(&self.f.rpc(
            subject,
            model,
            "search",
            json!({"domain":domain,"order":"id asc"}),
        ))
    }
    pub fn upload(&self, subject: Subject, parent: i64) -> i64 {
        success(&self.f.rpc(subject, "product.template", "thread_upload", json!({"ids":[parent],"name":"mail-acceptance.txt","mimetype":"text/plain","data":"aGVsbG8gd29ybGQ="})))["attachment_id"].as_i64().unwrap()
    }
    pub fn attach(&self, parent: i64) -> i64 {
        success(&self.f.rpc(Subject::Admin, "product.template", "thread_attach", json!({"ids":[parent],"name":"mail-bound.txt","mimetype":"text/plain","data":"aGVsbG8gd29ybGQ="})))["attachment_id"].as_i64().unwrap()
    }
    pub fn binding(&self, id: i64) -> Value {
        self.read(
            Subject::Admin,
            "ir.attachment",
            &[id],
            &[
                "res_model",
                "res_id",
                "res_field",
                "create_uid",
                "checksum",
                "name",
            ],
        )[0]
        .clone()
    }
    pub fn assert_attachment_denied(&self, reply: &Reply, subject: Subject, op: &str, ids: &[i64]) {
        let records = if ids.is_empty() {
            "the new attachment".to_owned()
        } else {
            format!("{ids:?}")
        };
        error(
            reply,
            403,
            "erp.exceptions.AccessError",
            &format!(
                "Sorry, you are not allowed to access this document. Please contact your system administrator.\n\n(Operation: {op})\n\nRecords: {records}, User: {}",
                self.f.user(subject).id
            ),
        );
    }
    pub fn attachment_denied(&self, subject: Subject, method: &str, ids: &[i64], body: Value) {
        self.assert_attachment_denied(
            &self.f.rpc(subject, "ir.attachment", method, body),
            subject,
            method,
            ids,
        );
    }
    pub fn message(&self, parent: i64, body: &str, extra: Value) -> i64 {
        let mut vals = json!({"model":"product.template","res_id":parent,"body":body,"subject":body,"message_type":"comment"});
        vals.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.create("mail.message", vals)
    }
    pub fn fetch(&self, subject: Subject, topics: Value) -> Value {
        success(&self.f.rpc(
            subject,
            "mail.store",
            "store_fetch",
            json!({"wire":1,"topics":topics}),
        ))
    }
    pub fn thread(&self, subject: Subject, parent: i64) -> Value {
        self.fetch(subject,json!([{"name":"thread","params":{"model":"product.template","id":parent,"attachments":true,"followers":true}}]))
    }
}

pub fn relation(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n.as_i64().unwrap(),
        Value::Array(pair) => pair[0].as_i64().unwrap(),
        _ => panic!("expected relation id or pair"),
    }
}

/// SEC-009's independently spelled non-debug refusal, including the declared company hint.
pub fn record_denied(
    f: &MultiuserFixture<'_>,
    reply: &Reply,
    subject: Subject,
    model: &str,
    description: &str,
    op: &str,
    outside: bool,
) {
    let user = f.user(subject);
    let hint = if outside {
        format!("\n\n{OUTSIDE}")
    } else {
        String::new()
    };
    error(
        reply,
        403,
        "erp.exceptions.AccessError",
        &format!(
            "Uh-oh! Looks like you have stumbled upon some top-secret records.\n\nSorry, {} (id={}) doesn't have '{op}' access to:\n- {description} ({model})\n\nIf you really, really need access, perhaps you can win over your friendly administrator with a batch of freshly baked cookies.{hint}",
            user.login, user.id
        ),
    );
}

pub fn content_denied(denied: &Reply, missing: &Reply) {
    assert_eq!(denied.status, 404);
    assert_eq!(denied.status, missing.status);
    assert_eq!(denied.body, missing.body);
    let headers = |reply: &Reply| {
        reply
            .headers
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "x-request-id" | "date"))
            .map(|(name, values)| (name.clone(), values.clone()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    assert_eq!(headers(denied), headers(missing));
}

pub fn slot(answer: &Value, index: usize) -> &Value {
    &answer["topics"].as_array().expect("store topic slots")[index]
}

pub fn records<'a>(answer: &'a Value, model: &str) -> &'a [Value] {
    match answer["store"]["records"].get(model) {
        // Store wire omits model keys with no records.
        None => &[],
        Some(rows) => rows.as_array().expect("store model record array"),
    }
}
