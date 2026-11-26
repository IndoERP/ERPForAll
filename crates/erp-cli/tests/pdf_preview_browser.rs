// SPDX-License-Identifier: MIT OR Apache-2.0
//! Installed PDF field preview through a group-restricted binary field.
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
fn installed_binary_preview_obeys_the_field_and_record_access_of_the_browser() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let assets = tempfile::tempdir().unwrap();
    let compiler = Path::new(env!("CARGO_BIN_EXE_erp")).with_file_name("erp-ui-compile");
    assert!(Command::new("bash").current_dir(root).env("ERP_UI_COMPILER", compiler)
        .env("ERP_UI_OUTPUT_DIR", assets.path())
        .args(["-ec", "toolchain_env=$(scripts/web-toolchain.sh env); eval \"$toolchain_env\"; exec node web/scripts/build-human-ui.mjs --addons web,hr"])
        .status().unwrap().success());
    run_with_assets(assets.path().to_owned(), move |f| {
        success(&native_http::json_post(
            f.native.port,
            &f.native.cookies,
            "/erp/module/install",
            r#"{"modules":["hr"]}"#,
        ));
        let denied = f.user(Subject::A);
        let officer = f.provision(UserSpec {
            login: "test-pdf-officer",
            company: denied.company,
            companies: vec![denied.company],
            groups: vec![
                f.xmlid("base", "group_user"),
                f.xmlid("hr", "group_hr_manager"),
            ],
        });
        let mut browser = Command::new("bash").current_dir(root)
            .args(["-ec", "toolchain_env=$(scripts/web-toolchain.sh env); eval \"$toolchain_env\"; exec node web/scripts/browser-binary-pdf.mjs"])
            .stdin(Stdio::piped()).spawn().unwrap();
        let config = json!({"origin":format!("http://127.0.0.1:{}",f.native.port),"officer":officer.login,
            "officerPassword":format!("{} & = boundary £ password",f.native.db.name()),"reader":denied.login,
            "readerPassword":format!("{} & = fixture £ password",f.native.db.name()),
            "action":f.xmlid("hr","hr_employee_action"),"company":denied.company});
        browser
            .stdin
            .take()
            .unwrap()
            .write_all(config.to_string().as_bytes())
            .unwrap();
        assert!(
            browser.wait().unwrap().success(),
            "installed PDF preview failed"
        );
    });
}
