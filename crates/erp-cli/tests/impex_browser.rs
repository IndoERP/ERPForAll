// SPDX-License-Identifier: MIT OR Apache-2.0
//! Round-trip a downloaded workbook through the installed Contacts list.
#![allow(clippy::unwrap_used, reason = "fixture assertions")]
#[allow(clippy::panic, reason = "native fixture startup assertions")]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::NativeFixture;
use serde_json::json;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

#[ignore = "requires the web toolchain and Chromium"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contacts_template_preview_import_and_ordered_export() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let assets = tempfile::tempdir().unwrap();
    let toolchain = "set -eu; eval \"$(scripts/web-toolchain.sh env)\"; ";
    let built = Command::new("bash")
        .current_dir(root)
        .args([
            "-c",
            &format!("{toolchain}exec node web/scripts/build-human-ui.mjs --addons web"),
        ])
        .env(
            "ERP_UI_COMPILER",
            Path::new(env!("CARGO_BIN_EXE_erp")).with_file_name("erp-ui-compile"),
        )
        .env("ERP_UI_OUTPUT_DIR", assets.path())
        .status()
        .unwrap();
    assert!(built.success());
    let f = NativeFixture::start_with_assets(|_| {}, Some(assets.path())).await;
    let installed = native_http::json_post(
        f.port,
        &f.cookies,
        "/erp/module/install",
        r#"{"modules":["contacts"]}"#,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    let actions = f
        .client
        .query(
            "SELECT name, res_id::bigint FROM ir_model_data
             WHERE module = 'contacts' AND name IN ('action_contacts', 'action_contact_tags')",
            &[],
        )
        .await
        .unwrap();
    let actions: std::collections::HashMap<String, i64> =
        actions.iter().map(|row| (row.get(0), row.get(1))).collect();
    let mut browser = Command::new("bash")
        .current_dir(root)
        .args([
            "-c",
            &format!("{toolchain}exec node web/scripts/browser-impex.mjs"),
        ])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let config = json!({"origin":format!("http://127.0.0.1:{}",f.port),"cookies":f.cookies,
        "contacts":actions["action_contacts"],"tags":actions["action_contact_tags"]});
    browser
        .stdin
        .take()
        .unwrap()
        .write_all(config.to_string().as_bytes())
        .unwrap();
    assert!(browser.wait().unwrap().success());
    for (table, name) in [
        ("res_partner", "Workbook browser contact"),
        ("res_partner_category", "CSV browser tag"),
    ] {
        // Stored names support translated columns as well as plain text.
        let sql =
            format!("SELECT count(*) FROM {table} WHERE name::text LIKE '%' || $1::text || '%'");
        let count: i64 = f.client.query_one(&sql, &[&name]).await.unwrap().get(0);
        assert_eq!(count, 1, "exactly one {name}");
    }
}
