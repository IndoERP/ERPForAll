//! Explicit old-bundle/new-native-server compatibility rehearsal (not a mocked JSON2 backend).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "fixture assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::NativeFixture;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit browser rehearsal requires compiled old/new assets; run check-browser.sh"]
async fn old_bundle_new_native_panel_manual_recovery() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let old = std::env::var("PANEL_OLD_ASSETS").expect("PANEL_OLD_ASSETS is required");
    let assets = root.join("web/dist/human-ui");
    let fixture = NativeFixture::start_with_assets(|_| {}, Some(&assets)).await;
    let install = native_http::json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/module/install",
        r#"{"modules":["acceptance_notes"]}"#,
    );
    assert_eq!(install.status, 200, "install failed: {}", install.body);
    let action: i64 = fixture.client.query_one("SELECT res_id FROM ir_model_data WHERE module='acceptance_notes' AND name='note_action'", &[]).await.unwrap().get(0);
    // Linked addon files remain immutable. Customize the installed fixture view through the
    // same authenticated model write used by an administrator, retaining cache invalidation.
    let search: i64 = fixture.client.query_one("SELECT res_id FROM ir_model_data WHERE module='acceptance_notes' AND name='note_search'", &[]).await.unwrap().get(0);
    let changed = native_http::json_post(fixture.port, &fixture.cookies,
        "/erp/json/2/ir.ui.view/write", &serde_json::json!({"ids":[search],"vals":{"arch_db":
            "<search><field name='name'/><searchpanel><field name='priority' icon='folder'/><field name='owner_id' select='multi'/></searchpanel></search>"
        }}).to_string());
    assert_eq!(
        changed.status, 200,
        "fixture view write failed: {}",
        changed.body
    );
    let mut child = Command::new("node")
        .arg(root.join("web/scripts/panel-contract-browser.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({
                "origin":format!("http://127.0.0.1:{}",fixture.port),"cookies":fixture.cookies,
                "action":action,"oldAssets":old
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let status = child.wait().unwrap();
    fixture.cleanup().await;
    assert!(
        status.success(),
        "compiled browser compatibility rehearsal failed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit compiled browser rehearsal; requires web/dist/human-ui"]
async fn unsupported_panel_keeps_native_list_open() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let fixture =
        NativeFixture::start_with_assets(|_| {}, Some(&root.join("web/dist/human-ui"))).await;
    let installed = native_http::json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/module/install",
        r#"{"modules":["acceptance_notes"]}"#,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    let action:i64=fixture.client.query_one("SELECT res_id FROM ir_model_data WHERE module='acceptance_notes' AND name='note_action'",&[]).await.unwrap().get(0);
    let search:i64=fixture.client.query_one("SELECT res_id FROM ir_model_data WHERE module='acceptance_notes' AND name='note_search'",&[]).await.unwrap().get(0);
    let changed=native_http::json_post(fixture.port,&fixture.cookies,"/erp/json/2/ir.ui.view/write",&serde_json::json!({"ids":[search],"vals":{"arch_db":"<search><field name='name'/><searchpanel class='private-marker'><field name='priority'/></searchpanel></search>"}}).to_string());
    assert_eq!(changed.status, 200, "{}", changed.body);
    let created = native_http::json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/acceptance_notes.note/create",
        r#"{"vals_list":[{"name":"Panel list remains usable"}]}"#,
    );
    assert_eq!(created.status, 200, "{}", created.body);
    let mut child = Command::new("node")
        .arg(root.join("web/scripts/panel-list-browser.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(serde_json::json!({"origin":format!("http://127.0.0.1:{}",fixture.port),"cookies":fixture.cookies,"action":action}).to_string().as_bytes()).unwrap();
    let status = child.wait().unwrap();
    fixture.cleanup().await;
    assert!(
        status.success(),
        "unsupported panel native list browser failed"
    );
}
