//! Public JSON-2 cases whose records are issued by a service or carry model-specific access rules.

use super::multiuser_fixture::{MultiuserFixture, Subject, run, success};
use super::multiuser_mail::Mail;
use super::native_http::Reply;
use serde_json::{Value, json};

pub const SERVICE_MODELS: &[&str] = &[
    "ir.capability",
    "ir.config.setting.scope",
    "ir.config.setting.value",
    "ir.cron",
    "ir.cron.trigger",
    "ir.module.category",
    "ir.module.module.dependency",
    "ir.module.module.exclusion",
    "ir.module.post_commit",
    "res.users.apikeys",
    "res.users.log",
    "product.barcode.claim",
    "product.barcode.scope",
    "uom.reference.fence",
    "uom.reference.use",
];

pub const TRANSIENT_MODELS: &[&str] = &[
    "change.password.own",
    "change.password.user",
    "change.password.wizard",
    "res.users.identitycheck",
];

pub const MAIL_MODELS: &[&str] = &[
    "mail.activity",
    "mail.activity.type",
    "mail.followers",
    "mail.message",
    "mail.message.subtype",
    "mail.scheduled.message",
];

const AUTHENTICATED: [Subject; 3] = [Subject::Admin, Subject::A, Subject::B];
const USERS: [Subject; 2] = [Subject::A, Subject::B];
const NO_RECORD: i64 = 2_147_483_647;
const ACCESS: &str = "erp.exceptions.AccessError";
const VALIDATION: &str = "erp.exceptions.ValidationError";
const MISSING: &str = "erp.exceptions.MissingError";
const USER: &str = "erp.exceptions.UserError";

fn error(reply: Reply, status: u16, kind: &str, request: &str) {
    let value: Value = serde_json::from_str(&reply.body).unwrap_or(Value::Null);
    assert_eq!(
        reply.status,
        status,
        "{request} (RPC error kind {:?})",
        value["name"].as_str()
    );
    assert_eq!(value["name"], kind, "{request}");
    assert!(value.get("values").is_none(), "{request}");
}

fn denied(f: &MultiuserFixture<'_>, who: Subject, model: &str, method: &str, body: Value) {
    error(
        f.rpc(who, model, method, body),
        403,
        ACCESS,
        &format!("{model}/{method}"),
    );
}

fn probes(id: i64, vals: Value) -> [(&'static str, Value); 6] {
    [
        ("search", json!({"domain":[["id","=",id]]})),
        ("read", json!({"ids":[id],"fields":["id"]})),
        (
            "read_group",
            json!({"domain":[["id","=",id]],"groupby":[],"aggregates":["__count"],"limit":1}),
        ),
        ("create", json!({"vals_list":[vals]})),
        ("write", json!({"ids":[id],"vals":vals})),
        ("unlink", json!({"ids":[id]})),
    ]
}

#[test]
fn public_json2_service_models_refuse_direct_access() {
    run(|f| {
        for &model in SERVICE_MODELS {
            for who in AUTHENTICATED {
                for (method, body) in probes(NO_RECORD, json!({})) {
                    denied(f, who, model, method, body);
                }
            }
        }
    });
}

#[test]
fn public_json2_transient_rows_are_owned_and_passwords_stay_request_local() {
    run(|f| {
        for (model, secret) in [
            ("change.password.own", "new_password"),
            ("res.users.identitycheck", "password"),
        ] {
            let ids: Vec<_> = AUTHENTICATED
                .into_iter()
                .map(|who| {
                    success(&f.rpc(who, model, "create", json!({"vals_list":[{}]})))[0]
                        .as_i64()
                        .unwrap()
                })
                .collect();
            for (index, who) in AUTHENTICATED.into_iter().enumerate() {
                let own = ids[index];
                let other = ids[(index + 1) % ids.len()];
                let domain = json!([["id", "in", ids]]);
                assert_eq!(
                    success(&f.rpc(who, model, "search", json!({"domain":domain}))),
                    json!([own])
                );
                assert_eq!(
                    success(&f.rpc(
                        who,
                        model,
                        "read_group",
                        json!({"domain":domain,"groupby":[],"aggregates":["__count"],"limit":1})
                    )),
                    json!([[1]])
                );
                let row = success(&f.rpc(who, model, "read", json!({"ids":[own],"fields":["id"]})));
                assert_eq!(row, json!([{"id":own}]));
                denied(
                    f,
                    who,
                    model,
                    "read",
                    json!({"ids":[other],"fields":["id"]}),
                );
                denied(f, who, model, "unlink", json!({"ids":[other]}));
                error(
                    f.rpc(
                        who,
                        model,
                        "write",
                        json!({"ids":[own],"vals":{(secret):"test-write-marker"}}),
                    ),
                    422,
                    VALIDATION,
                    model,
                );
                let after =
                    success(&f.rpc(who, model, "read", json!({"ids":[own],"fields":[secret]})));
                assert_eq!(after[0][secret], false, "{model} returned a secret");
            }
            for (who, own) in AUTHENTICATED.into_iter().zip(ids) {
                success(&f.rpc(who, model, "unlink", json!({"ids":[own]})));
            }
        }

        let wizard = "change.password.wizard";
        let line = "change.password.user";
        let admin = Subject::Admin;
        let id = success(&f.rpc(admin, wizard, "create", json!({"vals_list":[{}]})))[0]
            .as_i64()
            .unwrap();
        let selected = json!({"wizard_id":id,"user_id":f.user(Subject::A).id});
        for who in AUTHENTICATED {
            error(
                f.rpc(who, line, "create", json!({"vals_list":[selected]})),
                404,
                MISSING,
                line,
            );
        }
        for who in USERS {
            for (method, body) in probes(id, json!({"user_ids":[[5,0,0]]})) {
                denied(f, who, wizard, method, body);
            }
            for (method, body) in probes(NO_RECORD, selected.clone()) {
                if method != "create" {
                    denied(f, who, line, method, body);
                }
            }
        }
        assert_eq!(
            success(&f.rpc(admin, wizard, "search", json!({"domain":[["id","=",id]]}))),
            json!([id])
        );
        assert_eq!(
            success(&f.rpc(
                admin,
                wizard,
                "read",
                json!({"ids":[id],"fields":["user_ids"]})
            ))[0]["user_ids"],
            json!([])
        );
        denied(
            f,
            admin,
            wizard,
            "write",
            json!({"ids":[id],"vals":{"user_ids":[[5,0,0]]}}),
        );
        assert_eq!(
            success(&f.rpc(
                admin,
                line,
                "search",
                json!({"domain":[["wizard_id","=",id]]})
            )),
            json!([])
        );
        success(&f.rpc(admin, wizard, "unlink", json!({"ids":[id]})));
    });
}

#[test]
fn public_json2_mail_crud_uses_model_and_record_grants() {
    run(|f| {
        let mail = Mail::new(f);
        let admin = Subject::Admin;
        let kind = mail.create("mail.activity.type", json!({"name":"Matrix activity type"}));
        let subtype = mail.create("mail.message.subtype", json!({"name":"Matrix subtype"}));
        let message = mail.message(mail.a, "Matrix message", json!({}));
        let hidden = mail.message(mail.b, "Other company matrix message", json!({}));
        for (who, count) in [(admin, 2), (Subject::A, 1), (Subject::B, 1)] {
            let args = json!({"domain":[["id","in",[message,hidden]]],"groupby":[],"aggregates":["__count"],"limit":1});
            assert_eq!(
                success(&f.rpc(who, "mail.message", "read_group", args)),
                json!([[count]])
            );
        }
        let follower = mail.create(
            "mail.followers",
            json!({"res_model":"product.template","res_id":mail.b,"partner_id":mail.partner_a}),
        );
        for (model, id, vals, update) in [
            (
                "mail.activity.type",
                kind,
                json!({"name":"New type"}),
                json!({"name":"Type renamed"}),
            ),
            (
                "mail.message",
                message,
                json!({"model":"product.template","res_id":mail.a,"body":"Matrix proposed"}),
                json!({"body":"Matrix updated"}),
            ),
            (
                "mail.followers",
                follower,
                json!({"res_model":"product.template","res_id":mail.a,"partner_id":mail.partner_a}),
                json!({"subtype_ids":[[6,0,[subtype]]]}),
            ),
            (
                "mail.message.subtype",
                subtype,
                json!({"name":"New subtype"}),
                json!({"name":"Subtype renamed"}),
            ),
        ] {
            for who in AUTHENTICATED {
                let visible = model != "mail.message" || who != Subject::B;
                assert_eq!(
                    mail.search(who, model, &[id]),
                    if visible { json!([id]) } else { json!([]) },
                    "{model}"
                );
                if visible {
                    assert_eq!(mail.read(who, model, &[id], &["id"])[0]["id"], id);
                } else {
                    denied(f, who, model, "read", json!({"ids":[id],"fields":["id"]}));
                }
                if who != admin {
                    denied(f, who, model, "create", json!({"vals_list":[vals]}));
                    denied(f, who, model, "write", json!({"ids":[id],"vals":update}));
                    denied(f, who, model, "unlink", json!({"ids":[id]}));
                }
            }
            mail.write(admin, model, &[id], update);
            success(&f.rpc(admin, model, "unlink", json!({"ids":[id]})));
        }

        let model = "mail.activity";
        let mut personal = Vec::new();
        for who in AUTHENTICATED {
            let vals = json!({"summary":"Matrix activity","activity_type_id":null,"date_deadline":"2030-01-02","user_id":f.user(who).id});
            let id = success(&f.rpc(who, model, "create", json!({"vals_list":[vals]})))[0]
                .as_i64()
                .unwrap();
            personal.push(id);
        }
        for (index, who) in AUTHENTICATED.into_iter().enumerate() {
            let own = personal[index];
            let other = personal[(index + 1) % personal.len()];
            assert_eq!(mail.read(who, model, &[own], &["id"])[0]["id"], own);
            if who != admin {
                assert_eq!(mail.search(who, model, &personal), json!([own]));
                denied(
                    f,
                    who,
                    model,
                    "read",
                    json!({"ids":[other],"fields":["id"]}),
                );
                denied(
                    f,
                    who,
                    model,
                    "write",
                    json!({"ids":[other],"vals":{"summary":"refused"}}),
                );
                denied(f, who, model, "unlink", json!({"ids":[other]}));
            }
            mail.write(who, model, &[own], json!({"summary":"Matrix changed"}));
        }
        for (who, id) in AUTHENTICATED.into_iter().zip(personal) {
            success(&f.rpc(who, model, "unlink", json!({"ids":[id]})));
        }

        let model = "mail.scheduled.message";
        let soon =
            erp_fields::DateTime::from_micros(erp_fields::DateTime::now().micros() + 3_600_000_000)
                .to_string();
        let scheduled = json!({"model":"product.template","res_id":mail.shared,"scheduled_date":soon,"author_id":mail.partner_a,"slot":1});
        for who in USERS {
            for (method, body) in probes(NO_RECORD, json!({"subject":"Matrix schedule"})) {
                if method != "create" {
                    denied(f, who, model, method, body);
                }
            }
        }
        assert_eq!(mail.search(admin, model, &[NO_RECORD]), json!([]));
        assert_eq!(
            mail.read(admin, model, &[NO_RECORD], &["subject"]),
            json!([])
        );
        // A direct writer cannot move a recipient's message to a different thread.
        error(
            f.rpc(
                admin,
                model,
                "write",
                json!({"ids":[NO_RECORD],"vals":{"res_id":mail.a}}),
            ),
            422,
            USER,
            model,
        );
        for who in AUTHENTICATED {
            error(
                f.rpc(who, model, "create", json!({"vals_list":[scheduled]})),
                422,
                USER,
                model,
            );
        }
    });
}

#[test]
fn public_json2_nullable_relation_and_limited_groups_round_trip() {
    run(|f| {
        let mail = Mail::new(f);
        let model = "mail.message.subtype";
        let kind = mail.create(model, json!({"name":"Nullable parent"}));
        let one = mail.create(
            model,
            json!({"name":"Grouped first","parent_id":kind,"sequence":31}),
        );
        let two = mail.create(
            model,
            json!({"name":"Grouped second","parent_id":kind,"sequence":32}),
        );
        let admin = Subject::Admin;
        assert_eq!(
            mail.read(admin, model, &[one], &["parent_id"])[0]["parent_id"][0],
            kind
        );
        mail.write(admin, model, &[one], json!({"parent_id":null}));
        assert_eq!(
            mail.read(admin, model, &[one], &["parent_id"])[0]["parent_id"],
            false
        );
        let domain = json!([["id", "in", [one, two]]]);
        for who in AUTHENTICATED {
            let request = |offset| json!({"domain":domain,"groupby":["sequence"],"aggregates":["__count"],"order":"sequence ASC","limit":1,"offset":offset});
            for (offset, expected) in [(0, json!([[31, 1]])), (1, json!([[32, 1]])), (2, json!([]))]
            {
                assert_eq!(
                    success(&f.rpc(who, model, "read_group", request(offset))),
                    expected
                );
            }
        }
        error(f.rpc(admin, model, "read_group", json!({"domain":domain,"groupby":vec!["sequence";65],"aggregates":["__count"],"limit":1})), 422, VALIDATION, "mail subtype grouped-read input cap");
    });
}
