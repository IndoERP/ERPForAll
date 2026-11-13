//! Password changes through installed base and the actual native HTTP server.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration assertions and shared fixture"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::{NativeFixture, Reply, cookie, embedded, json_post, request};
use serde_json::{Value, json};

fn ok(reply: Reply) -> Value {
    assert_eq!(reply.status, 200, "{}", reply.body);
    serde_json::from_str(&reply.body).unwrap()
}
fn error(reply: Reply, kind: &str, message: &str) {
    assert_eq!(
        reply.status,
        if matches!(
            kind,
            "erp.exceptions.AccessDenied" | "erp.exceptions.AccessError"
        ) {
            403
        } else {
            422
        },
        "{}",
        reply.body
    );
    let value: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(value["name"], kind);
    assert_eq!(value["message"], message);
    assert_eq!(value["debug"], "");
}
fn call(f: &NativeFixture, model: &str, method: &str, body: Value) -> Reply {
    json_post(
        f.port,
        &f.cookies,
        &format!("/erp/json/2/{model}/{method}"),
        &body.to_string(),
    )
}
fn login(f: &NativeFixture, name: &str, password: &str) -> Reply {
    let page = request(f.port, "GET", "/erp/login", &[], "");
    let prefs = cookie(&page, "prefs");
    let data = embedded(&page, "erp-login");
    let body = format!(
        "db={}&login={}&password={}&csrf_token={}",
        f.db.name(),
        native_http::encoded(name.as_bytes()),
        native_http::encoded(password.as_bytes()),
        data["csrf"].as_str().unwrap()
    );
    request(
        f.port,
        "POST",
        "/erp/login",
        &[
            ("cookie", &prefs),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &body,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_admin_reset_uses_actor_identity_and_never_target_old_password() {
    let f = NativeFixture::start().await;
    for (model, xmlid, kind) in [
        (
            "change.password.wizard",
            "change_password_wizard_view",
            "form",
        ),
        ("change.password.own", "change_password_own_form", "form"),
        (
            "res.users.identitycheck",
            "change_password_identity_form",
            "form",
        ),
        (
            "change.password.user",
            "change_password_wizard_user_list",
            "list",
        ),
        (
            "change.password.user",
            "change_password_wizard_user_form",
            "form",
        ),
        ("res.users", "settings_users_form", "form"),
    ] {
        let view: i64 = f.client.query_one("SELECT res_id::bigint FROM ir_model_data WHERE module = 'base' AND name = $1 AND model = 'ir.ui.view'", &[&xmlid]).await.unwrap().get(0);
        let described = ok(call(
            &f,
            model,
            "get_views",
            json!({"views":[[view,kind]],"wire_major":1,"options":{"mobile":false}}),
        ));
        assert!(
            described["notices"].as_array().unwrap().is_empty(),
            "{described}"
        );
        assert_eq!(described["views"][0]["type"], kind);
        let nodes = described["views"][0]["nodes"].as_array().unwrap();
        let (fields, button): (&[&str], Option<&str>) = match xmlid {
            "change_password_wizard_view" => (&["user_ids"], Some("change_password_button")),
            "change_password_own_form" => (
                &["new_password", "confirm_password"],
                Some("change_password"),
            ),
            "change_password_identity_form" => (&["password"], Some("run_check")),
            "change_password_wizard_user_list" | "change_password_wizard_user_form" => {
                (&["user_login", "new_passwd"], None)
            }
            "settings_users_form" => (
                &["name", "login", "email", "password_is_own"],
                Some("preference_change_password"),
            ),
            _ => panic!("unknown fixture view"),
        };
        if xmlid == "settings_users_form" {
            // the name heading and the two login labels (Login / Email, Login).
            assert!(
                nodes.iter().any(|node| node["kind"] == "heading"),
                "{described}"
            );
            let mut logins: Vec<&str> = nodes
                .iter()
                .filter(|node| node["kind"] == "field" && node["field"]["name"] == "login")
                .map(|node| node["label"].as_str().unwrap_or_default())
                .collect();
            logins.sort_unstable();
            assert_eq!(logins, ["Login", "Login / Email"], "{described}");
        }
        for field in fields {
            let node = nodes
                .iter()
                .find(|node| node["kind"] == "field" && node["field"]["name"] == *field)
                .expect("installed compiled field");
            if ["password", "new_passwd", "new_password", "confirm_password"].contains(field) {
                assert_eq!(node["widget"], "password");
            }
        }
        if let Some(button) = button {
            assert!(
                nodes
                    .iter()
                    .any(|node| node["kind"] == "button" && node["name"] == button)
            );
        }
    }
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[{"name":"Reset Target","login":"password-target","password":"target-before"}]}),
    ));
    let uid = created.as_array().unwrap()[0].as_i64().unwrap();
    let own_uid: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM res_users WHERE login = 'fixture-admin'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let flags = ok(call(
        &f,
        "res.users",
        "read",
        json!({"ids":[own_uid,uid],"fields":["id","password_is_own"],"context":{"uid":uid}}),
    ));
    for row in flags.as_array().unwrap() {
        assert_eq!(row["password_is_own"], row["id"] == own_uid);
    }

    let action = ok(call(
        &f,
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[uid]}),
    ));
    assert_eq!(action["res_model"], "res.users.identitycheck");
    assert!(
        action.get("context").is_none(),
        "identity action must not store a callable/password"
    );
    error(
        call(
            &f,
            "res.users.identitycheck",
            "run_check",
            json!({"password":"target-before"}),
        ),
        "erp.exceptions.AccessDenied",
        "Access Denied",
    );
    ok(call(
        &f,
        "res.users.identitycheck",
        "run_check",
        json!({"password":"fixture-administrator-password"}),
    ));
    let action = ok(call(
        &f,
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[uid]}),
    ));
    assert_eq!(action["res_model"], "change.password.wizard");
    let wizard = action["res_id"].as_i64().unwrap();
    let rows = f.client.query("SELECT id::bigint, user_id::bigint, user_login FROM change_password_user WHERE wizard_id = $1", &[&wizard]).await.unwrap();
    assert_eq!(rows.len(), 1);
    let line: i64 = rows[0].get(0);
    assert_eq!(rows[0].get::<_, i64>(1), uid);
    assert_eq!(rows[0].get::<_, String>(2), "password-target");
    let closed = ok(call(
        &f,
        "change.password.wizard",
        "change_password_button",
        json!({"ids":[wizard],"changes":[{"line_id":line,"new_passwd":"target-after-sensitive"}]}),
    ));
    assert_eq!(closed, json!({"type":"ir.actions.act_window_close"}));
    assert_eq!(
        login(&f, "password-target", "target-after-sensitive").status,
        303
    );
    assert_ne!(login(&f, "password-target", "target-before").status, 303);
    // A recent window opens immediately, but saving after expiry must ask again.
    let next = ok(call(
        &f,
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[uid]}),
    ));
    assert_eq!(next["res_model"], "change.password.wizard");
    f.client.execute("UPDATE erp_session SET identity_checked_at = clock_timestamp() - interval '10 minutes'", &[]).await.unwrap();
    let next_id = next["res_id"].as_i64().unwrap();
    let next_line: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM change_password_user WHERE wizard_id = $1",
            &[&next_id],
        )
        .await
        .unwrap()
        .get(0);
    let stale = ok(call(
        &f,
        "change.password.wizard",
        "change_password_button",
        json!({"ids":[next_id],"changes":[{"line_id":next_line,"new_passwd":"never-stored-sensitive"}]}),
    ));
    assert_eq!(stale["res_model"], "res.users.identitycheck");
    let columns = f.client.query("SELECT column_name FROM information_schema.columns WHERE table_schema = 'public' AND table_name = ANY($1::text[])", &[&vec!["change_password_user", "change_password_own", "res_users_identitycheck"]]).await.unwrap();
    for row in columns {
        assert!(
            !["new_passwd", "new_password", "confirm_password", "password"]
                .contains(&row.get::<_, String>(0).as_str())
        );
    }
    let stderr = std::fs::read_to_string(&f.stderr_path).unwrap();
    for secret in ["target-after-sensitive", "never-stored-sensitive"] {
        assert!(!stderr.contains(secret));
    }
    f.cleanup().await;
}

/// administrator created by `erp db init` creates a user from only a name, a login and a
/// password; the new user is internal, has the main company as default and only allowed company,
/// and the administrator reads it back)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_admin_creates_an_internal_user_of_the_main_company() {
    let f = NativeFixture::start().await;
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[{"name":"Plain User","login":"plain-user","password":"plain-user-password"}]}),
    ));
    let uid = created.as_array().unwrap()[0].as_i64().unwrap();
    let main: i64 = f
        .client
        .query_one(
            "SELECT res_id::bigint FROM ir_model_data \
              WHERE module = 'base' AND name = 'main_company' AND model = 'res.company'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let row = f
        .client
        .query_one(
            "SELECT share, company_id::bigint, \
                    ARRAY(SELECT cid::bigint FROM res_company_users_rel \
                           WHERE user_id = u.id ORDER BY cid) \
               FROM res_users u WHERE id = $1",
            &[&uid],
        )
        .await
        .unwrap();
    let (share, company, companies): (bool, Option<i64>, Vec<i64>) =
        (row.get(0), row.get(1), row.get(2));
    assert!(!share);
    assert_eq!(company, Some(main));
    assert_eq!(companies, [main]);
    let read = ok(call(
        &f,
        "res.users",
        "read",
        json!({"ids":[uid],"fields":["login"]}),
    ));
    assert_eq!(read[0]["login"], "plain-user");
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_own_change_checks_confirmation_and_keeps_the_existing_cookie() {
    let f = NativeFixture::start().await;
    let uid: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM res_users WHERE login = 'fixture-admin'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    ok(call(
        &f,
        "res.users.identitycheck",
        "run_check",
        json!({"password":"fixture-administrator-password"}),
    ));
    let action = ok(call(
        &f,
        "res.users",
        "preference_change_password",
        json!({"ids":[uid]}),
    ));
    assert_eq!(action["res_model"], "change.password.own");
    let wizard = action["res_id"].as_i64().unwrap();
    error(
        call(
            &f,
            "change.password.own",
            "change_password",
            json!({"ids":[wizard],"new_password":"actor-after-sensitive","confirm_password":"mismatch"}),
        ),
        "erp.exceptions.ValidationError",
        "The new password and its confirmation must be identical.",
    );
    ok(call(
        &f,
        "change.password.own",
        "change_password",
        json!({"ids":[wizard],"new_password":"actor-after-sensitive","confirm_password":"actor-after-sensitive"}),
    ));
    // Same cookie, with no login or Set-Cookie requirement, still performs authenticated RPC.
    let own = ok(call(
        &f,
        "res.users",
        "read",
        json!({"ids":[uid],"fields":["login"]}),
    ));
    assert_eq!(own[0]["login"], "fixture-admin");
    assert_eq!(
        login(&f, "fixture-admin", "actor-after-sensitive").status,
        303
    );
    assert!(
        !std::fs::read_to_string(&f.stderr_path)
            .unwrap()
            .contains("actor-after-sensitive")
    );
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manager_cannot_attach_a_line_to_someone_elses_wizard() {
    let f = NativeFixture::start().await;
    let system: i64 = f.client.query_one("SELECT res_id::bigint FROM ir_model_data WHERE module = 'base' AND name = 'group_system'", &[]).await.unwrap().get(0);
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[{"name":"Other manager","login":"other-manager","password":"other-manager-old","group_ids":[[6,0,[system]]]}]}),
    ));
    let other = created[0].as_i64().unwrap();
    ok(call(
        &f,
        "res.users.identitycheck",
        "run_check",
        json!({"password":"fixture-administrator-password"}),
    ));
    let wizard = ok(call(
        &f,
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[other]}),
    ))["res_id"]
        .as_i64()
        .unwrap();
    let signed = login(&f, "other-manager", "other-manager-old");
    assert_eq!(signed.status, 303);
    let other_cookie = cookie(&signed, "session_id");
    let refused = json_post(
        f.port,
        &other_cookie,
        "/erp/json/2/change.password.user/create",
        &json!({"vals_list":[{"wizard_id":wizard,"user_id":other,"user_login":"spoofed"}]})
            .to_string(),
    );
    // lines are created by opening the wizard only: a generic create answers as an unknown user
    assert_ne!(refused.status, 200, "{}", refused.body);
    let value: Value = serde_json::from_str(&refused.body).unwrap();
    assert_eq!(value["name"], "erp.exceptions.MissingError");
    assert_eq!(
        value["message"],
        "A selected password reset user no longer exists."
    );
    let lines: i64 = f
        .client
        .query_one(
            "SELECT count(*) FROM change_password_user WHERE wizard_id = $1",
            &[&wizard],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(lines, 1, "only the line the open created");
    f.cleanup().await;
}

/// A second company "Other company", in the main company's currency.
async fn other_company(f: &NativeFixture) -> i64 {
    let main = base_xid(f, "main_company").await;
    let currency: i64 = f
        .client
        .query_one(
            "SELECT currency_id::bigint FROM res_company WHERE id = $1",
            &[&main],
        )
        .await
        .unwrap()
        .get(0);
    ok(call(
        f,
        "res.company",
        "create",
        json!({"vals_list":[{"name":"Other company","currency_id":currency}]}),
    ))[0]
        .as_i64()
        .unwrap()
}

/// refused for an access manager of the main company only, identical to an unknown user)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_manager_cannot_reset_an_internal_user_of_another_company() {
    let f = NativeFixture::start().await;
    let manager_group = base_xid(&f, "group_erp_manager").await;
    let internal_group = base_xid(&f, "group_user").await;
    let other = other_company(&f).await;
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[
            {"name":"Main manager","login":"main-manager","password":"manager-before","group_ids":[[6,0,[manager_group]]]},
            {"name":"Far internal","login":"far-internal","password":"far-before","group_ids":[[6,0,[internal_group]]]},
            {"name":"Near internal","login":"near-internal","password":"near-before","group_ids":[[6,0,[internal_group]]]}
        ]}),
    ));
    let (far, near) = (created[1].as_i64().unwrap(), created[2].as_i64().unwrap());
    move_to_company(&f, far, other).await;
    for uid in [far, near] {
        let share: bool = f
            .client
            .query_one("SELECT share FROM res_users WHERE id = $1", &[&uid])
            .await
            .unwrap()
            .get(0);
        assert!(!share, "user {uid} is internal");
    }
    let signed = login(&f, "main-manager", "manager-before");
    assert_eq!(signed.status, 303);
    let manager_cookie = cookie(&signed, "session_id");
    let as_manager = |model: &str, method: &str, body: Value| {
        json_post(
            f.port,
            &manager_cookie,
            &format!("/erp/json/2/{model}/{method}"),
            &body.to_string(),
        )
    };
    ok(as_manager(
        "res.users.identitycheck",
        "run_check",
        json!({"password":"manager-before"}),
    ));
    let never: i64 = f
        .client
        .query_one(
            "SELECT COALESCE(max(id), 0)::bigint + 1000 FROM res_users",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let unknown = answer(&as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[never]}),
    ));
    assert_ne!(unknown.0, 200, "an unknown user is refused: {unknown:?}");
    let hidden = answer(&as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[far]}),
    ));
    assert_eq!(hidden, unknown, "an internal user of another company");
    // submitting: the near user leaves the manager's companies after the wizard opened
    let wizard = ok(as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[near]}),
    ))["res_id"]
        .as_i64()
        .unwrap();
    let line: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM change_password_user WHERE wizard_id = $1",
            &[&wizard],
        )
        .await
        .unwrap()
        .get(0);
    move_to_company(&f, near, other).await;
    let by_line = answer(&as_manager(
        "change.password.user",
        "change_password_button",
        json!({"ids":[line],"new_passwd":"near-after"}),
    ));
    assert_eq!(by_line, unknown, "submit by line");
    let lines: i64 = f
        .client
        .query_one(
            "SELECT count(*) FROM change_password_user WHERE user_id = $1",
            &[&far],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(lines, 0, "no wizard line names a refused target");
    assert_eq!(login(&f, "near-internal", "near-before").status, 303);
    assert_ne!(login(&f, "near-internal", "near-after").status, 303);
    f.cleanup().await;
}

/// answers as an unknown user and creates no line: lines are created by opening the wizard)
/// user of another company)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_create_of_a_reset_line_answers_as_an_unknown_user() {
    let f = NativeFixture::start().await;
    let manager_group = base_xid(&f, "group_erp_manager").await;
    let portal_group = base_xid(&f, "group_portal").await;
    let other = other_company(&f).await;
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[
            {"name":"Main manager","login":"main-manager","password":"manager-before","group_ids":[[6,0,[manager_group]]]},
            {"name":"Far portal","login":"far-portal","password":"far-before","group_ids":[[6,0,[portal_group]]]}
        ]}),
    ));
    let far = created[1].as_i64().unwrap();
    move_to_company(&f, far, other).await;
    let signed = login(&f, "main-manager", "manager-before");
    assert_eq!(signed.status, 303);
    let manager_cookie = cookie(&signed, "session_id");
    let as_manager = |model: &str, method: &str, body: Value| {
        json_post(
            f.port,
            &manager_cookie,
            &format!("/erp/json/2/{model}/{method}"),
            &body.to_string(),
        )
    };
    ok(as_manager(
        "res.users.identitycheck",
        "run_check",
        json!({"password":"manager-before"}),
    ));
    let never: i64 = f
        .client
        .query_one(
            "SELECT COALESCE(max(id), 0)::bigint + 1000 FROM res_users",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let unknown = answer(&as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[never]}),
    ));
    assert_ne!(unknown.0, 200, "an unknown user is refused: {unknown:?}");
    let wizard = ok(as_manager(
        "change.password.wizard",
        "create",
        json!({"vals_list":[{}]}),
    ))[0]
        .as_i64()
        .unwrap();
    for (name, user) in [("far", far), ("never", never)] {
        let reply = as_manager(
            "change.password.user",
            "create",
            json!({"vals_list":[{"wizard_id":wizard,"user_id":user}]}),
        );
        assert!(!reply.body.contains("far-portal"), "{name}: {}", reply.body);
        assert_eq!(answer(&reply), unknown, "{name}: {}", reply.body);
    }
    let by_wizard = as_manager(
        "change.password.wizard",
        "create",
        json!({"vals_list":[{"user_ids":[[0,0,{"user_id":far}]]}]}),
    );
    assert!(!by_wizard.body.contains("far-portal"), "{}", by_wizard.body);
    assert_eq!(answer(&by_wizard), unknown, "{}", by_wizard.body);
    let lines: i64 = f
        .client
        .query_one("SELECT count(*) FROM change_password_user", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(lines, 0, "no generic create made a line");
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_submission_refuses_generic_save_replay_and_wrong_legacy_password() {
    let f = NativeFixture::start().await;
    let uid: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM res_users WHERE login = 'fixture-admin'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    ok(call(
        &f,
        "res.users.identitycheck",
        "run_check",
        json!({"password":"fixture-administrator-password"}),
    ));
    let wizard = ok(call(
        &f,
        "res.users",
        "preference_change_password",
        json!({"ids":[uid]}),
    ))["res_id"]
        .as_i64()
        .unwrap();
    error(
        call(
            &f,
            "change.password.own",
            "write",
            json!({"ids":[wizard],"vals":{"new_password":"must-not-persist"}}),
        ),
        "erp.exceptions.ValidationError",
        "Passwords must be submitted with the dedicated password operation.",
    );
    let reply = request(
        f.port,
        "POST",
        "/erp/json/2/res.users/change_password",
        &[
            ("cookie", &f.cookies),
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
            ("idempotency-key", "018f0000-0000-7000-8000-000000000001"),
        ],
        &json!({"old_passwd":"fixture-administrator-password","new_passwd":"must-not-replay"})
            .to_string(),
    );
    error(
        reply,
        "erp.exceptions.ValidationError",
        "Idempotency keys are not supported for password operations.",
    );
    error(
        call(
            &f,
            "res.users",
            "change_password",
            json!({"old_passwd":"wrong-current-password","new_passwd":"must-not-set"}),
        ),
        "erp.exceptions.AccessDenied",
        "Access Denied",
    );
    assert_eq!(
        ok(call(
            &f,
            "res.users",
            "change_password",
            json!({"old_passwd":"fixture-administrator-password","new_passwd":"legacy-after"})
        )),
        true
    );
    assert_eq!(
        ok(call(
            &f,
            "res.users",
            "read",
            json!({"ids":[uid],"fields":["login"]})
        ))[0]["login"],
        "fixture-admin"
    );
    let denied = request(
        f.port,
        "POST",
        "/erp/json/2/res.users.identitycheck/run_check",
        &[
            ("cookie", &f.cookies),
            ("content-type", "application/json"),
            ("sec-fetch-site", "cross-site"),
        ],
        "{\"password\":\"legacy-after\"}",
    );
    assert_eq!(denied.status, 403);
    let stderr = std::fs::read_to_string(&f.stderr_path).unwrap();
    for secret in [
        "must-not-persist",
        "must-not-replay",
        "must-not-set",
        "wrong-current-password",
        "legacy-after",
    ] {
        assert!(!stderr.contains(secret), "server logged secret input");
    }
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_submission_keeps_ordinary_strict_rpc_binding() {
    let f = NativeFixture::start().await;
    error(
        call(&f, "res.users.identitycheck", "run_check", json!({})),
        "werkzeug.exceptions.UnprocessableEntity",
        "missing a required argument: 'password'",
    );
    error(
        call(
            &f,
            "res.users.identitycheck",
            "run_check",
            json!({"password":"probe","unexpected":true}),
        ),
        "werkzeug.exceptions.UnprocessableEntity",
        "got an unexpected keyword argument 'unexpected'",
    );
    error(
        call(
            &f,
            "res.users.identitycheck",
            "run_check",
            json!({"password":123}),
        ),
        "werkzeug.exceptions.UnprocessableEntity",
        "argument 'password' of res.users.identitycheck.run_check: expected a string, got integer",
    );
    assert!(
        !std::fs::read_to_string(&f.stderr_path)
            .unwrap()
            .contains("probe")
    );
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_manager_without_settings_authority_can_reset_another_user() {
    let f = NativeFixture::start().await;
    let group: i64 = f.client.query_one("SELECT res_id::bigint FROM ir_model_data WHERE module='base' AND name='group_erp_manager'", &[]).await.unwrap().get(0);
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[
            {"name":"Access manager","login":"narrow-manager","password":"manager-before","group_ids":[[6,0,[group]]]},
            {"name":"Managed target","login":"narrow-target","password":"target-before"}
        ]}),
    ));
    let target = created[1].as_i64().unwrap();
    let signed = login(&f, "narrow-manager", "manager-before");
    assert_eq!(signed.status, 303);
    let manager_cookie = cookie(&signed, "session_id");
    let as_manager = |model: &str, method: &str, body: Value| {
        json_post(
            f.port,
            &manager_cookie,
            &format!("/erp/json/2/{model}/{method}"),
            &body.to_string(),
        )
    };
    ok(as_manager(
        "res.users.identitycheck",
        "run_check",
        json!({"password":"manager-before"}),
    ));
    let action = ok(as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[target]}),
    ));
    assert_eq!(action["res_model"], "change.password.wizard");
    let wizard = action["res_id"].as_i64().unwrap();
    let line: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM change_password_user WHERE wizard_id=$1",
            &[&wizard],
        )
        .await
        .unwrap()
        .get(0);
    error(
        as_manager(
            "change.password.user",
            "write",
            json!({"ids":[line],"vals":{"user_id":created[0]}}),
        ),
        "erp.exceptions.AccessError",
        "Password wizard targets cannot be changed.",
    );
    error(
        as_manager(
            "change.password.wizard",
            "write",
            json!({"ids":[wizard],"vals":{"user_ids":[[5,0,0]]}}),
        ),
        "erp.exceptions.AccessError",
        "Password wizard targets cannot be changed.",
    );
    let closed = ok(as_manager(
        "change.password.user",
        "change_password_button",
        json!({"ids":[line],"new_passwd":"target-after"}),
    ));
    assert_eq!(closed["type"], "ir.actions.act_window_close");
    assert_eq!(login(&f, "narrow-target", "target-after").status, 303);
    assert_ne!(login(&f, "narrow-target", "target-before").status, 303);
    f.cleanup().await;
}

/// The `ir_model_data` record id of `base.<name>`.
async fn base_xid(f: &NativeFixture, name: &str) -> i64 {
    f.client
        .query_one(
            "SELECT res_id::bigint FROM ir_model_data WHERE module = 'base' AND name = $1",
            &[&name],
        )
        .await
        .unwrap()
        .get(0)
}

/// Move `user` to `company` alone (setup only: the user form keeps company_ids admin-only).
async fn move_to_company(f: &NativeFixture, user: i64, company: i64) {
    f.client
        .batch_execute(&format!(
            "UPDATE res_users SET company_id = {company} WHERE id = {user};
             DELETE FROM res_company_users_rel WHERE user_id = {user};
             INSERT INTO res_company_users_rel (cid, user_id) VALUES ({company}, {user});"
        ))
        .await
        .unwrap();
}

/// The status, error `name` and message of an error reply.
fn answer(reply: &Reply) -> (u16, Value, Value) {
    let value: Value = serde_json::from_str(&reply.body).unwrap();
    (
        reply.status,
        value["name"].clone(),
        value["message"].clone(),
    )
}

/// The text of the Settings administrator refusal (Amendment 1).
const SETTINGS_ONLY: &str =
    "Only a Settings administrator can reset the password of a Settings administrator.";

/// wizard open and in Auth at submit; a refused reset changes no password and writes no audit)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_manager_cannot_reset_a_settings_administrator() {
    let f = NativeFixture::start().await;
    let manager_group = base_xid(&f, "group_erp_manager").await;
    let system_group = base_xid(&f, "group_system").await;
    let admin: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM res_users WHERE login = 'fixture-admin'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[
            {"name":"Rights manager","login":"rights-manager","password":"manager-before","group_ids":[[6,0,[manager_group]]]},
            {"name":"Promoted later","login":"promoted-later","password":"promoted-before"}
        ]}),
    ));
    let promoted = created[1].as_i64().unwrap();
    let signed = login(&f, "rights-manager", "manager-before");
    assert_eq!(signed.status, 303);
    let cookie_value = cookie(&signed, "session_id");
    let as_manager = |model: &str, method: &str, body: Value| {
        json_post(
            f.port,
            &cookie_value,
            &format!("/erp/json/2/{model}/{method}"),
            &body.to_string(),
        )
    };
    ok(as_manager(
        "res.users.identitycheck",
        "run_check",
        json!({"password":"manager-before"}),
    ));
    // opening: the administrator holds base.group_system
    error(
        as_manager(
            "res.users",
            "action_change_password_wizard",
            json!({"ids":[admin]}),
        ),
        "erp.exceptions.AccessError",
        SETTINGS_ONLY,
    );
    let lines: i64 = f
        .client
        .query_one(
            "SELECT count(*) FROM change_password_user WHERE user_id = $1",
            &[&admin],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(lines, 0, "no wizard line names a refused target");
    // submitting: the target becomes a Settings administrator after the wizard opened
    let action = ok(as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[promoted]}),
    ));
    let wizard = action["res_id"].as_i64().unwrap();
    let line: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM change_password_user WHERE wizard_id = $1",
            &[&wizard],
        )
        .await
        .unwrap()
        .get(0);
    f.client
        .execute(
            "INSERT INTO res_groups_users_rel (gid, uid) VALUES ($1, $2)",
            &[&system_group, &promoted],
        )
        .await
        .unwrap();
    error(
        as_manager(
            "change.password.user",
            "change_password_button",
            json!({"ids":[line],"new_passwd":"promoted-after"}),
        ),
        "erp.exceptions.AccessError",
        SETTINGS_ONLY,
    );
    error(
        as_manager(
            "change.password.wizard",
            "change_password_button",
            json!({"ids":[wizard],"changes":[{"line_id":line,"new_passwd":"promoted-after"}]}),
        ),
        "erp.exceptions.AccessError",
        SETTINGS_ONLY,
    );
    assert_eq!(
        login(&f, "fixture-admin", "fixture-administrator-password").status,
        303
    );
    assert_eq!(login(&f, "promoted-later", "promoted-before").status, 303);
    assert_ne!(login(&f, "promoted-later", "promoted-after").status, 303);
    let stderr = std::fs::read_to_string(&f.stderr_path).unwrap();
    assert!(
        !stderr.contains("password_changed"),
        "a refused reset wrote an audit event:\n{stderr}"
    );
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_manager_resets_a_user_with_a_unicode_password_and_it_is_audited() {
    let f = NativeFixture::start().await;
    let group = base_xid(&f, "group_erp_manager").await;
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[
            {"name":"Rights manager","login":"rights-manager","password":"manager-before","group_ids":[[6,0,[group]]]},
            {"name":"Unicode target","login":"unicode-target","password":"target-before"}
        ]}),
    ));
    let manager_uid = created[0].as_i64().unwrap();
    let target = created[1].as_i64().unwrap();
    let signed = login(&f, "rights-manager", "manager-before");
    assert_eq!(signed.status, 303);
    let cookie_value = cookie(&signed, "session_id");
    let as_manager = |model: &str, method: &str, body: Value| {
        json_post(
            f.port,
            &cookie_value,
            &format!("/erp/json/2/{model}/{method}"),
            &body.to_string(),
        )
    };
    ok(as_manager(
        "res.users.identitycheck",
        "run_check",
        json!({"password":"manager-before"}),
    ));
    let action = ok(as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[target]}),
    ));
    let wizard = action["res_id"].as_i64().unwrap();
    let line: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM change_password_user WHERE wizard_id=$1",
            &[&wizard],
        )
        .await
        .unwrap()
        .get(0);
    let unicode = "pässwörd-日本語-🔑";
    let closed = ok(as_manager(
        "change.password.user",
        "change_password_button",
        json!({"ids":[line],"new_passwd":unicode}),
    ));
    assert_eq!(closed["type"], "ir.actions.act_window_close");
    assert_eq!(login(&f, "unicode-target", unicode).status, 303);
    assert_ne!(login(&f, "unicode-target", "target-before").status, 303);
    let stderr = std::fs::read_to_string(&f.stderr_path).unwrap();
    let event = stderr
        .lines()
        .find(|l| l.contains("password_changed"))
        .unwrap_or_else(|| panic!("no audit event in server stderr:\n{stderr}"));
    let tokens: Vec<&str> = event.split_whitespace().collect();
    for want in [
        format!("actor={manager_uid}"),
        format!("target_user={target}"),
        "event=password_changed".to_owned(),
    ] {
        assert!(tokens.contains(&want.as_str()), "{want} not in {event}");
    }
    assert!(
        tokens.iter().any(|t| t.starts_with("operation=")),
        "{event}"
    );
    assert!(tokens.iter().any(|t| t.starts_with("client=")), "{event}");
    assert_eq!(
        stderr
            .lines()
            .filter(|l| l.contains("password_changed"))
            .count(),
        1,
        "exactly one target was changed"
    );
    assert!(
        !stderr.contains("manager-before"),
        "a password reached stderr"
    );
    assert!(!stderr.contains(unicode), "the password reached stderr");
    f.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_manager_cannot_reset_a_portal_user_of_another_company() {
    let f = NativeFixture::start().await;
    let manager_group = base_xid(&f, "group_erp_manager").await;
    let portal_group = base_xid(&f, "group_portal").await;
    let main = base_xid(&f, "main_company").await;
    let currency: i64 = f
        .client
        .query_one(
            "SELECT currency_id::bigint FROM res_company WHERE id = $1",
            &[&main],
        )
        .await
        .unwrap()
        .get(0);
    let other = ok(call(
        &f,
        "res.company",
        "create",
        json!({"vals_list":[{"name":"Other company","currency_id":currency}]}),
    ))[0]
        .as_i64()
        .unwrap();
    let created = ok(call(
        &f,
        "res.users",
        "create",
        json!({"vals_list":[
            {"name":"Main manager","login":"main-manager","password":"manager-before","group_ids":[[6,0,[manager_group]]]},
            {"name":"Far portal","login":"far-portal","password":"far-before","group_ids":[[6,0,[portal_group]]]},
            {"name":"Near portal","login":"near-portal","password":"near-before","group_ids":[[6,0,[portal_group]]]}
        ]}),
    ));
    let (far, near) = (created[1].as_i64().unwrap(), created[2].as_i64().unwrap());
    move_to_company(&f, far, other).await;
    let signed = login(&f, "main-manager", "manager-before");
    assert_eq!(signed.status, 303);
    let manager_cookie = cookie(&signed, "session_id");
    let as_manager = |model: &str, method: &str, body: Value| {
        json_post(
            f.port,
            &manager_cookie,
            &format!("/erp/json/2/{model}/{method}"),
            &body.to_string(),
        )
    };
    ok(as_manager(
        "res.users.identitycheck",
        "run_check",
        json!({"password":"manager-before"}),
    ));
    // the reference: a user id that never existed
    let never: i64 = f
        .client
        .query_one(
            "SELECT COALESCE(max(id), 0)::bigint + 1000 FROM res_users",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let unknown = answer(&as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[never]}),
    ));
    assert_ne!(unknown.0, 200, "an unknown user is refused: {unknown:?}");
    // opening: the far user is outside the manager's companies
    let hidden = answer(&as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[far]}),
    ));
    assert_eq!(hidden, unknown, "a hidden user answers as an unknown one");
    let lines: i64 = f
        .client
        .query_one(
            "SELECT count(*) FROM change_password_user WHERE user_id = $1",
            &[&far],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(lines, 0, "no wizard line names a refused target");
    // submitting: the near user leaves the manager's companies after the wizard opened
    let action = ok(as_manager(
        "res.users",
        "action_change_password_wizard",
        json!({"ids":[near]}),
    ));
    let wizard = action["res_id"].as_i64().unwrap();
    let line: i64 = f
        .client
        .query_one(
            "SELECT id::bigint FROM change_password_user WHERE wizard_id = $1",
            &[&wizard],
        )
        .await
        .unwrap()
        .get(0);
    move_to_company(&f, near, other).await;
    let by_line = answer(&as_manager(
        "change.password.user",
        "change_password_button",
        json!({"ids":[line],"new_passwd":"near-after"}),
    ));
    assert_eq!(by_line, unknown, "submit by line");
    let by_wizard = answer(&as_manager(
        "change.password.wizard",
        "change_password_button",
        json!({"ids":[wizard],"changes":[{"line_id":line,"new_passwd":"near-after"}]}),
    ));
    assert_eq!(by_wizard, unknown, "submit by wizard");
    assert_eq!(login(&f, "near-portal", "near-before").status, 303);
    assert_eq!(login(&f, "far-portal", "far-before").status, 303);
    assert_ne!(login(&f, "near-portal", "near-after").status, 303);
    let stderr = std::fs::read_to_string(&f.stderr_path).unwrap();
    assert!(
        !stderr.contains("password_changed"),
        "a refused reset wrote an audit event:\n{stderr}"
    );
    f.cleanup().await;
}
