//! Browser installation of a data-only addon (amendment, 2026-10-27): the real `erp`
//! binary, a nonowner runtime role and real HTTP, with `addons/sample_library` offered by the
//! catalogue and installed from the addon directory without rebuilding the binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::*;
use std::fs;

/// The catalogue offers `sample_library`; installing it by name works from the addon directory;
/// its menu and action reach the web menus and its models work; names that are not installable
/// data-only addons are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_only_addon_installs_from_the_addons_screen() {
    let fixture = NativeFixture::start().await;
    let port = fixture.port;
    let cookies = &fixture.cookies;
    let client = &fixture.client;
    let stderr_path = &fixture.stderr_path;
    // The catalogue is built from the addon directory.
    let before = catalogue(port, cookies);
    assert_eq!(
        state_of(&before, "sample_library").as_deref(),
        Some("uninstalled")
    );
    assert_eq!(
        state_of(&before, "acceptance_notes").as_deref(),
        Some("uninstalled")
    );
    assert_eq!(state_of(&before, "base").as_deref(), Some("installed"));
    assert_eq!(
        state_of(&before, "unlinked_native"),
        None,
        "native code that is not linked is not offered"
    );
    assert!(
        fs::read_to_string(stderr_path)
            .unwrap()
            .contains("unlinked_native is not offered"),
        "the refusal is logged"
    );

    // Refused: a name that is not in the directory, and a folder with unlinked native code.
    for name in ["no_such_addon", "unlinked_native"] {
        let refused = json_post(
            port,
            cookies,
            "/erp/module/install",
            &format!(r#"{{"modules":["{name}"]}}"#),
        );
        assert_ne!(refused.status, 200, "{name}: {}", refused.body);
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM ir_module_module WHERE name=$1 AND state<>'uninstalled'",
                    &[&name]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
    }
    // A path or an extra key is not a name.
    let path = json_post(
        port,
        cookies,
        "/erp/module/install",
        r#"{"modules":["../sample_library"]}"#,
    );
    assert_ne!(path.status, 200, "{}", path.body);

    // Install the data-only addon by name.
    let installed = json_post(
        port,
        cookies,
        "/erp/module/install",
        r#"{"modules":["sample_library"]}"#,
    );
    assert_eq!(installed.status, 200, "install: {}", installed.body);
    let result: serde_json::Value = serde_json::from_str(&installed.body).unwrap();
    assert!(
        result["installed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "sample_library"),
        "{result}"
    );
    assert_eq!(
        state_of(&catalogue(port, cookies), "sample_library").as_deref(),
        Some("installed")
    );
    let menus = json_post(
        port,
        cookies,
        "/erp/json/2/ir.ui.menu/erp_load_web_menus",
        "{}",
    );
    assert_eq!(menus.status, 200, "{}", menus.body);
    assert!(
        menus.body.contains("Library"),
        "menu missing: {}",
        menus.body
    );
    assert!(
        menus.body.contains("Books"),
        "submenu missing: {}",
        menus.body
    );
    let created = json_post(
        port,
        cookies,
        "/erp/json/2/sample_library.book/create",
        r#"{"vals_list":[{"title":"Installed from the Addons screen"}]}"#,
    );
    assert_eq!(created.status, 200, "create: {}", created.body);
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM sample_library_book WHERE title='Installed from the Addons screen'", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    let detail = json_post(
        port,
        cookies,
        "/erp/json/2/ir.module.module/erp_addon_detail",
        r#"{"name":"sample_library"}"#,
    );
    assert_eq!(detail.status, 200, "detail: {}", detail.body);
    let detail: serde_json::Value = serde_json::from_str(&detail.body).unwrap();
    assert_eq!(detail["name"], "sample_library");
    assert_eq!(detail["state"], "installed");
    assert!(detail["installed_version"].is_string());
    assert!(
        detail["menus"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["label"] == "Library")
    );
    // Strict detail arguments fail before any lifecycle effect; nullable version fields survive.
    assert!(detail["available_version"].is_string());
    assert!(detail["menus"]["next_cursor"].is_null());
    for (body, status, kind, message) in [
        (
            r#"{"name":"sample_library","limit":0}"#,
            422,
            "erp.exceptions.ValidationError",
            "Addon detail limit must be between 1 and 100",
        ),
        (
            r#"{"name":"sample_library","limit":101}"#,
            422,
            "erp.exceptions.ValidationError",
            "Addon detail limit must be between 1 and 100",
        ),
        (
            r#"{"name":"sample_library","menus_cursor":"forged"}"#,
            422,
            "erp.exceptions.ValidationError",
            "Addon detail cursor is malformed or stale",
        ),
        (
            r#"{"name":"sample_library","extra":true}"#,
            422,
            "",
            "got an unexpected keyword argument 'extra'",
        ),
    ] {
        let invalid = json_post(
            port,
            cookies,
            "/erp/json/2/ir.module.module/erp_addon_detail",
            body,
        );
        assert_eq!(
            invalid.status, status,
            "detail accepted invalid args: {}",
            invalid.body
        );
        let reply: serde_json::Value = serde_json::from_str(&invalid.body).unwrap();
        if !kind.is_empty() {
            assert_eq!(reply["name"], kind, "{}", invalid.body);
        }
        assert!(
            reply["message"].as_str().unwrap().contains(message),
            "{}",
            invalid.body
        );
    }
    let first_page = json_post(
        port,
        cookies,
        "/erp/json/2/ir.module.module/erp_addon_detail",
        r#"{"name":"sample_library","limit":1}"#,
    );
    assert_eq!(first_page.status, 200, "{}", first_page.body);
    let first_page: serde_json::Value = serde_json::from_str(&first_page.body).unwrap();
    assert_eq!(first_page["menus"]["items"].as_array().unwrap().len(), 1);
    let cursor = first_page["menus"]["next_cursor"]
        .as_str()
        .expect("sample_library has Library and Books menus");
    let next_body =
        serde_json::json!({"name":"sample_library","limit":1,"menus_cursor":cursor}).to_string();
    let next_page = json_post(
        port,
        cookies,
        "/erp/json/2/ir.module.module/erp_addon_detail",
        &next_body,
    );
    assert_eq!(next_page.status, 200, "{}", next_page.body);
    let next_page: serde_json::Value = serde_json::from_str(&next_page.body).unwrap();
    assert_ne!(
        next_page["menus"]["items"][0]["id"],
        first_page["menus"]["items"][0]["id"]
    );
    let menu_id = first_page["menus"]["items"][0]["id"].as_i64().unwrap();
    let old_name:String=client.query_one("UPDATE ir_ui_menu SET name='Changed detail metadata' WHERE id=$1 RETURNING (SELECT name FROM ir_ui_menu WHERE id=$1)",&[&menu_id]).await.unwrap().get(0);
    let stale = json_post(
        port,
        cookies,
        "/erp/json/2/ir.module.module/erp_addon_detail",
        &next_body,
    );
    assert_eq!(
        stale.status, 422,
        "metadata change must invalidate cursor: {}",
        stale.body
    );
    client
        .execute(
            "UPDATE ir_ui_menu SET name=$2 WHERE id=$1",
            &[&menu_id, &old_name],
        )
        .await
        .unwrap();
    // Text admission is checked before JSON metadata is materialized, independently of pages.
    client
        .execute(
            "UPDATE ir_ui_menu SET name=$2 WHERE id=$1",
            &[&menu_id, &"x".repeat(4096)],
        )
        .await
        .unwrap();
    let boundary = json_post(
        port,
        cookies,
        "/erp/json/2/ir.module.module/erp_addon_detail",
        r#"{"name":"sample_library"}"#,
    );
    assert_eq!(
        boundary.status, 200,
        "admitted menu text boundary: {}",
        boundary.body
    );
    client
        .execute(
            "UPDATE ir_ui_menu SET name=$2 WHERE id=$1",
            &[&menu_id, &"x".repeat(4097)],
        )
        .await
        .unwrap();
    let oversized = json_post(
        port,
        cookies,
        "/erp/json/2/ir.module.module/erp_addon_detail",
        r#"{"name":"sample_library"}"#,
    );
    assert_eq!(
        oversized.status, 422,
        "oversized menu text: {}",
        oversized.body
    );
    let oversized: serde_json::Value = serde_json::from_str(&oversized.body).unwrap();
    assert_eq!(oversized["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        oversized["message"],
        "Addon detail metadata exceeds its admission bound"
    );
    client
        .execute(
            "UPDATE ir_ui_menu SET name=$2 WHERE id=$1",
            &[&menu_id, &old_name],
        )
        .await
        .unwrap();
    let audit_before: i64 = client
        .query_one("SELECT count(*) FROM erp_module_lifecycle_audit", &[])
        .await
        .unwrap()
        .get(0);
    let preview = json_post(
        port,
        cookies,
        "/erp/json/2/ir.module.module/erp_preview_uninstall",
        r#"{"modules":["sample_library"]}"#,
    );
    assert_eq!(preview.status, 422, "preview: {}", preview.body);
    let refusal: serde_json::Value = serde_json::from_str(&preview.body).unwrap();
    assert_eq!(refusal["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        refusal["message"],
        "Uninstall impact cannot describe package runtime lifecycle callbacks; unsupported until a later release"
    );
    let uninstall = json_post(
        port,
        cookies,
        "/erp/module/uninstall",
        &format!(
            r#"{{"modules":["sample_library"],"impact":"uninstall-v1:{}"}}"#,
            "0".repeat(64)
        ),
    );
    assert_eq!(uninstall.status, 422, "uninstall: {}", uninstall.body);
    let refusal: serde_json::Value = serde_json::from_str(&uninstall.body).unwrap();
    assert_eq!(refusal["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        refusal["message"],
        "Uninstall impact cannot describe custom index support on table 'ir_dated_parameter'; unsupported until a later release"
    );
    assert_eq!(
        state_of(&catalogue(port, cookies), "sample_library").as_deref(),
        Some("installed")
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM erp_module_lifecycle_audit", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        audit_before,
        "refusals must not write lifecycle audit"
    );
    let upgrade = json_post(
        port,
        cookies,
        "/erp/module/upgrade",
        r#"{"modules":["sample_library"]}"#,
    );
    assert_eq!(upgrade.status, 200, "upgrade: {}", upgrade.body);
    let upgraded: serde_json::Value = serde_json::from_str(&upgrade.body).unwrap();
    assert_eq!(upgraded["upgraded"], serde_json::json!(["sample_library"]));
    assert_eq!(upgraded["installed"], serde_json::json!([]));
    assert!(upgraded["registry_version"].is_string());
    assert_eq!(client.query_one("SELECT count(*) FROM erp_module_lifecycle_audit WHERE operation='upgrade' AND requested=ARRAY['sample_library']::text[]", &[]).await.unwrap().get::<_, i64>(0), 1);
    let created_after_upgrade = json_post(
        port,
        cookies,
        "/erp/json/2/sample_library.book/create",
        r#"{"vals_list":[{"title":"Usable after upgrade publication"}]}"#,
    );
    assert_eq!(
        created_after_upgrade.status, 200,
        "runtime grants/RPC after upgrade: {}",
        created_after_upgrade.body
    );
    assert_eq!(client.query_one("SELECT count(*) FROM sample_library_book WHERE title='Usable after upgrade publication'", &[]).await.unwrap().get::<_, i64>(0), 1);
    fixture.cleanup().await;
}
