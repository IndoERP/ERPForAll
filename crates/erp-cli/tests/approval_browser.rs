// SPDX-License-Identifier: MIT OR Apache-2.0
//! The compiled web client requests and reviews a preference change in separate sessions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/multiuser_fixture.rs"]
mod multiuser_fixture;
#[path = "support/native_http.rs"]
mod native_http;

use multiuser_fixture::{Subject, UserSpec, run_with_assets, success};
use serde_json::json;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

#[test]
fn a_reviewed_change_finishes_in_the_requesters_browser() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let compiler = Path::new(env!("CARGO_BIN_EXE_erp")).with_file_name("erp-ui-compile");
    let assets = tempfile::tempdir().unwrap();
    let built = Command::new("bash").current_dir(root)
        .env("ERP_UI_COMPILER", compiler)
        .env("ERP_UI_OUTPUT_DIR", assets.path())
        .args(["-ec", "toolchain_env=$(scripts/web-toolchain.sh env); eval \"$toolchain_env\"; exec node web/scripts/build-human-ui.mjs --addons web,approvals"])
        .status().unwrap();
    assert!(built.success(), "approval browser assets did not build");
    run_with_assets(assets.path().to_owned(), move |f| {
        success(&native_http::json_post(
            f.native.port,
            &f.native.cookies,
            "/erp/module/install",
            r#"{"modules":["approvals"]}"#,
        ));
        let workflows = f.xmlid("approvals", "configuration_action");
        let unauthorized = f.rpc(
            Subject::A,
            "ir.actions.client",
            "erp_load_web_client_action",
            json!({"ids":[workflows], "wire_major":1}),
        );
        assert_eq!(
            unauthorized.status, 403,
            "configuration requires an administrator"
        );
        let sender = f.user(Subject::A);
        let reviewer = f.user(Subject::Admin);
        let final_reviewer = f.provision(UserSpec {
            login: "compliance-reviewer",
            company: reviewer.company,
            companies: vec![reviewer.company, sender.company],
            groups: vec![f.group("group_user")],
        });
        let recipient = success(&f.rpc(
            Subject::Admin,
            "res.users",
            "read",
            json!({"ids":[sender.id],"fields":["partner_id"]}),
        ));
        let partner = &recipient[0]["partner_id"];
        let partner = partner.as_i64().or_else(|| partner[0].as_i64()).unwrap();
        success(&f.rpc(
            Subject::Admin,
            "res.users",
            "write",
            json!({"ids":[sender.id],"vals":{"notification_type":"inbox"}}),
        ));
        let product = success(&f.rpc(
            Subject::Admin,
            "product.template",
            "create",
            json!({"vals_list":[{"name":"Delivery schedule","company_id":null}]}),
        ))[0]
            .as_i64()
            .unwrap();
        success(&f.rpc(
            Subject::Admin,
            "product.template",
            "thread_post",
            json!({
                "ids":[product], "post":{"body":"The delivery date is confirmed.",
                "subject":"Delivery scheduled","partner_ids":[partner]}
            }),
        ));

        let mut browser = Command::new("bash").current_dir(root)
            .args(["-ec", "toolchain_env=$(scripts/web-toolchain.sh env); eval \"$toolchain_env\"; exec node web/scripts/browser-approval.mjs"])
            .stdin(Stdio::piped()).spawn().unwrap();
        let input = json!({"origin":format!("http://127.0.0.1:{}", f.native.port),
            "requester":sender.login, "reviewer":reviewer.login,
            "configurationAction":f.xmlid("approvals","workflows_window"),
            "legacyConfigurationAction":workflows,
            "legacyRequestAction":f.xmlid("approvals","requests_action"),
            "configurationMenu":f.xmlid("approvals","workflows_menu"),
            "finalReviewer":final_reviewer.login,
            "finalPassword":format!("{} & = boundary £ password", f.native.db.name()),
            "password":format!("{} & = fixture £ password", f.native.db.name())});
        browser
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        assert!(
            browser.wait().unwrap().success(),
            "compiled approval flow failed"
        );
        let saved = success(&f.rpc(
            Subject::Admin,
            "res.users",
            "read",
            json!({"ids":[sender.id],"fields":["phone","write_uid"]}),
        ));
        assert_eq!(saved[0]["phone"], "12345678");
        let actor = &saved[0]["write_uid"];
        assert_eq!(
            actor.as_i64().or_else(|| actor[0].as_i64()),
            Some(sender.id)
        );
    });
}
