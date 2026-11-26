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

use multiuser_fixture::{Subject, run_with_assets, success};
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
        .args(["-ec", "toolchain_env=$(scripts/web-toolchain.sh env); eval \"$toolchain_env\"; exec node web/scripts/build-human-ui.mjs"])
        .status().unwrap();
    assert!(built.success(), "approval browser assets did not build");
    run_with_assets(assets.path().to_owned(), move |f| {
        let sender = f.user(Subject::A);
        let reviewer = f.user(Subject::Admin);
        success(&f.rpc(Subject::Admin, "ir.approval.policy", "create", json!({"vals_list":[{
            "name":"Review preferences in the browser", "principal_kind":"user", "effect":"write",
            "model":"res.users", "method":"web_save", "outcome":"approve",
            "approver_user_ids":[[6,0,[reviewer.id]]]
        }]})));
        let mut browser = Command::new("bash").current_dir(root)
            .args(["-ec", "toolchain_env=$(scripts/web-toolchain.sh env); eval \"$toolchain_env\"; exec node web/scripts/browser-approval.mjs"])
            .stdin(Stdio::piped()).spawn().unwrap();
        let input = json!({"origin":format!("http://127.0.0.1:{}", f.native.port),
            "requester":sender.login, "reviewer":reviewer.login, "password":format!("{} & = fixture £ password", f.native.db.name())});
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
