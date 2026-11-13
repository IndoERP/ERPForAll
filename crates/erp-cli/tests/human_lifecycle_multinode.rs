//! Multi-node lifecycle boundaries with real binary peer processes over a shared database.
//! Verifies two nodes, concurrent operations, authority revocation, stale impact, node loss
//! and survivor maintenance/refresh across native server processes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration multi-node assertions and shared helpers"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::*;
use serde_json::{Value, json};

fn request_http_ready(port: u16) -> u16 {
    request(port, "GET", "/erp/health/ready", &[], "").status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_two_nodes_share_database_and_serve_detail() {
    let fixture = NativeFixture::start().await;
    let peer = fixture.peer();
    assert_eq!(request_http_ready(fixture.port), 200);
    assert_eq!(request_http_ready(peer.port), 200);

    let cat_a = catalogue(fixture.port, &fixture.cookies);
    let cat_b = catalogue(peer.port, &fixture.cookies);
    assert_eq!(cat_a, cat_b, "peer catalogue drifted from primary");
    assert_eq!(state_of(&cat_a, "base"), Some("installed".to_owned()));

    for port in [fixture.port, peer.port] {
        let detail = json_post(
            port,
            &fixture.cookies,
            "/erp/json/2/ir.module.module/erp_addon_detail",
            r#"{"name":"base"}"#,
        );
        assert_eq!(detail.status, 200, "port {port} detail: {}", detail.body);
        let doc: Value = serde_json::from_str(&detail.body).unwrap();
        assert_eq!(doc["name"], "base");
        assert_eq!(doc["state"], "installed");

        let preview = json_post(
            port,
            &fixture.cookies,
            "/erp/json/2/ir.module.module/erp_preview_uninstall",
            r#"{"modules":["base"]}"#,
        );
        assert_eq!(preview.status, 422, "port {port} preview: {}", preview.body);
        let err: Value = serde_json::from_str(&preview.body).unwrap();
        assert_eq!(err["name"], "erp.exceptions.ValidationError");
        assert_eq!(
            err["message"],
            "Uninstall impact cannot describe package runtime lifecycle callbacks; unsupported until a later release"
        );
        assert!(err.get("fingerprint").is_none());
    }

    drop(peer);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_authority_revocation_denies_both_nodes() {
    let fixture = NativeFixture::start().await;
    let peer = fixture.peer();
    assert_eq!(request_http_ready(fixture.port), 200);
    assert_eq!(request_http_ready(peer.port), 200);

    let uid: i64 = fixture
        .client
        .query_one("SELECT id FROM res_users WHERE login='fixture-admin'", &[])
        .await
        .unwrap()
        .get(0);
    let gid: i64 = fixture
        .client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system'",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    let before: Value = fixture
        .client
        .query_one(
            "SELECT jsonb_build_object('modules',(SELECT jsonb_agg(to_jsonb(m) ORDER BY id) FROM ir_module_module m),'audit',(SELECT count(*) FROM erp_module_lifecycle_audit),'identities',(SELECT jsonb_agg(to_jsonb(i) ORDER BY name) FROM installed_addon_identity i))",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    fixture
        .client
        .execute(
            "DELETE FROM res_groups_users_rel WHERE uid=$1 AND gid=$2",
            &[&uid, &gid],
        )
        .await
        .unwrap();

    let uninstall_body = format!(
        r#"{{"modules":["base"],"impact":"uninstall-v1:{}"}}"#,
        "0".repeat(64)
    );

    // Detail, preview and the uninstall admission use the addon administration check; upgrade
    // is authorized by the engine itself, with the module operation's message.
    let addon_admin = "Addon administration requires system administration authority";
    let engine_admin = "Module installation requires system administration authority";
    for port in [fixture.port, peer.port] {
        for (path, body, message) in [
            (
                "/erp/json/2/ir.module.module/erp_addon_detail",
                r#"{"name":"base"}"#,
                addon_admin,
            ),
            (
                "/erp/json/2/ir.module.module/erp_preview_uninstall",
                r#"{"modules":["base"]}"#,
                addon_admin,
            ),
            (
                "/erp/module/upgrade",
                r#"{"modules":["base"]}"#,
                engine_admin,
            ),
            (
                "/erp/module/uninstall",
                uninstall_body.as_str(),
                addon_admin,
            ),
        ] {
            let denied = json_post(port, &fixture.cookies, path, body);
            assert_eq!(
                denied.status, 403,
                "port {port} {path} accepted revoked authority: {}",
                denied.body
            );
            let val: Value = serde_json::from_str(&denied.body).unwrap();
            assert_eq!(val["name"], "erp.exceptions.AccessError");
            assert_eq!(val["message"], message, "port {port} {path}");
        }
    }

    fixture
        .client
        .execute("DELETE FROM erp_session WHERE user_id=$1", &[&uid])
        .await
        .unwrap();

    for port in [fixture.port, peer.port] {
        for (path, body) in [
            ("/erp/module/upgrade", r#"{"modules":["base"]}"#),
            ("/erp/module/uninstall", uninstall_body.as_str()),
            (
                "/erp/json/2/ir.module.module/erp_addon_detail",
                r#"{"name":"base"}"#,
            ),
            (
                "/erp/json/2/ir.module.module/erp_preview_uninstall",
                r#"{"modules":["base"]}"#,
            ),
        ] {
            let denied = json_post(port, &fixture.cookies, path, body);
            assert_eq!(
                denied.status, 401,
                "port {port} {path} accepted deleted session: {}",
                denied.body
            );
            assert_eq!(denied.body, "user is not connected");
        }
    }

    let after: Value = fixture
        .client
        .query_one(
            "SELECT jsonb_build_object('modules',(SELECT jsonb_agg(to_jsonb(m) ORDER BY id) FROM ir_module_module m),'audit',(SELECT count(*) FROM erp_module_lifecycle_audit),'identities',(SELECT jsonb_agg(to_jsonb(i) ORDER BY name) FROM installed_addon_identity i))",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        after, before,
        "denied requests on both nodes altered durable lifecycle state or audit"
    );

    drop(peer);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_stale_impact_refuses_confirmation_on_any_node() {
    let fixture = NativeFixture::start().await;
    let peer = fixture.peer();
    assert_eq!(request_http_ready(fixture.port), 200);
    assert_eq!(request_http_ready(peer.port), 200);

    let snapshot = "SELECT jsonb_build_object('modules',(SELECT jsonb_agg(to_jsonb(m) ORDER BY id) FROM ir_module_module m),'audit',(SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM erp_module_lifecycle_audit a),'identities',(SELECT jsonb_agg(to_jsonb(i) ORDER BY name) FROM installed_addon_identity i))";
    let before: Value = fixture
        .client
        .query_one(snapshot, &[])
        .await
        .unwrap()
        .get(0);

    for port in [fixture.port, peer.port] {
        let malformed = json_post(
            port,
            &fixture.cookies,
            "/erp/module/uninstall",
            r#"{"modules":["base"],"impact":"malformed"}"#,
        );
        assert_eq!(malformed.status, 400, "port {port}: {}", malformed.body);
        assert_eq!(malformed.body, "Invalid uninstall impact fingerprint");

        let empty_modules = json_post(
            port,
            &fixture.cookies,
            "/erp/module/uninstall",
            &format!(
                r#"{{"modules":[],"impact":"uninstall-v1:{}"}}"#,
                "0".repeat(64)
            ),
        );
        assert_eq!(
            empty_modules.status, 400,
            "port {port}: {}",
            empty_modules.body
        );
        assert_eq!(
            empty_modules.body,
            "Uninstall requires between 1 and 64 addon names"
        );

        let well_formed_fingerprint = format!("uninstall-v1:{}", "0".repeat(64));
        let confirmation = json!({
            "modules": ["base"],
            "impact": well_formed_fingerprint,
        })
        .to_string();

        let refused = json_post(
            port,
            &fixture.cookies,
            "/erp/module/uninstall",
            &confirmation,
        );
        assert_eq!(refused.status, 422, "port {port}: {}", refused.body);
        let err: Value = serde_json::from_str(&refused.body).unwrap();
        assert_eq!(err["name"], "erp.exceptions.ValidationError");
        assert_eq!(
            err["message"],
            "Uninstall impact cannot describe custom index support on table 'ir_dated_parameter'; unsupported until a later release"
        );
    }

    let after: Value = fixture
        .client
        .query_one(snapshot, &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        after, before,
        "refused confirmation changed durable state or audit on any node"
    );

    drop(peer);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_concurrent_operations_serialize_without_corruption() {
    let fixture = NativeFixture::start().await;
    let peer = fixture.peer();
    assert_eq!(request_http_ready(fixture.port), 200);
    assert_eq!(request_http_ready(peer.port), 200);

    let port_a = fixture.port;
    let port_b = peer.port;
    let cookies_a = fixture.cookies.clone();
    let cookies_b = fixture.cookies.clone();

    let task_a = tokio::task::spawn_blocking(move || {
        json_post(
            port_a,
            &cookies_a,
            "/erp/module/upgrade",
            r#"{"modules":["base"]}"#,
        )
    });
    let task_b = tokio::task::spawn_blocking(move || {
        json_post(
            port_b,
            &cookies_b,
            "/erp/module/upgrade",
            r#"{"modules":["base"]}"#,
        )
    });

    let (res_a, res_b) = tokio::join!(task_a, task_b);
    let reply_a = res_a.unwrap();
    let reply_b = res_b.unwrap();

    // One upgrade holds the module operation lock; an overlapping one is refused with the
    // registry and is refused, not replayed.
    let concurrent = serde_json::json!({
        "name": "erp.exceptions.UserError",
        "message": "ERPForAll is currently processing another module operation.\nPlease try again later or contact your system administrator.",
    });
    for (node, reply) in [(1, &reply_a), (2, &reply_b)] {
        let accepted = match reply.status {
            200 => true,
            422 => {
                let err: Value = serde_json::from_str(&reply.body).unwrap();
                err["name"] == concurrent["name"] && err["message"] == concurrent["message"]
            }
            503 => reply.body == "Server busy; retry later.",
            _ => false,
        };
        assert!(
            accepted,
            "node {node} unexpected concurrent status {}: {}",
            reply.status, reply.body
        );
    }
    assert!(
        reply_a.status == 200 || reply_b.status == 200,
        "neither concurrent upgrade succeeded: {} {} / {} {}",
        reply_a.status,
        reply_a.body,
        reply_b.status,
        reply_b.body
    );

    for port in [fixture.port, peer.port] {
        // A safe navigation brings a node that did not run the upgrade onto the new registry.
        let navigation = request(
            port,
            "GET",
            "/erp",
            &[("cookie", fixture.cookies.as_str())],
            "",
        );
        assert_eq!(
            navigation.status, 200,
            "port {port} navigation after concurrency: {}",
            navigation.body
        );
        let reply = json_post(
            port,
            &fixture.cookies,
            "/erp/json/2/ir.module.module/erp_addon_detail",
            r#"{"name":"base"}"#,
        );
        assert_eq!(
            reply.status, 200,
            "port {port} detail after concurrency: {}",
            reply.body
        );
        let doc: Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(doc["state"], "installed");
    }

    drop(peer);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_node_loss_releases_locks_allowing_survivor_progress() {
    let mut fixture = NativeFixture::start().await;
    let peer = fixture.peer();
    assert_eq!(request_http_ready(fixture.port), 200);
    assert_eq!(request_http_ready(peer.port), 200);

    fixture.server.stop();

    // The killed process holds no listener: it refuses the connection, so it cannot answer ready.
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", fixture.port)).is_err(),
        "stopped primary still accepts connections"
    );

    assert_eq!(request_http_ready(peer.port), 200);

    let cat = catalogue(peer.port, &fixture.cookies);
    assert_eq!(state_of(&cat, "base"), Some("installed".to_owned()));

    let detail = json_post(
        peer.port,
        &fixture.cookies,
        "/erp/json/2/ir.module.module/erp_addon_detail",
        r#"{"name":"base"}"#,
    );
    assert_eq!(detail.status, 200, "survivor detail: {}", detail.body);
    let doc: Value = serde_json::from_str(&detail.body).unwrap();
    assert_eq!(doc["name"], "base");
    assert_eq!(doc["state"], "installed");

    let preview = json_post(
        peer.port,
        &fixture.cookies,
        "/erp/json/2/ir.module.module/erp_preview_uninstall",
        r#"{"modules":["base"]}"#,
    );
    assert_eq!(preview.status, 422, "survivor preview: {}", preview.body);

    drop(peer);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_survivor_maintenance_and_refresh() {
    let fixture = NativeFixture::start().await;
    let peer = fixture.peer();
    assert_eq!(request_http_ready(fixture.port), 200);
    assert_eq!(request_http_ready(peer.port), 200);

    fixture
        .client
        .execute(
            "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();

    // node rebuilds on the way out, so the next POST runs on the complete new services.
    let detail = || {
        json_post(
            peer.port,
            &fixture.cookies,
            "/erp/json/2/ir.module.module/erp_addon_detail",
            r#"{"name":"base"}"#,
        )
    };
    let stale = detail();
    assert_eq!(stale.status, 503, "stale survivor POST: {}", stale.body);
    assert_eq!(stale.body, "Server busy; retry later.");
    let refreshed = detail();
    assert_eq!(
        refreshed.status, 200,
        "survivor refresh detail: {}",
        refreshed.body
    );
    let doc: Value = serde_json::from_str(&refreshed.body).unwrap();
    assert_eq!(doc["name"], "base");
    assert_eq!(doc["state"], "installed");

    fixture
        .client
        .execute(
            "UPDATE ir_module_module SET state='to upgrade' WHERE name='base'",
            &[],
        )
        .await
        .unwrap();
    fixture
        .client
        .execute(
            "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();

    // A safe navigation retries once after a successful rebuild; a pending module operation
    // makes the rebuild fail, so even the navigation stays in maintenance.
    let navigation = request(
        peer.port,
        "GET",
        "/erp",
        &[("cookie", fixture.cookies.as_str())],
        "",
    );
    assert_eq!(
        navigation.status, 503,
        "survivor navigation in maintenance answered non-503: {}",
        navigation.body
    );
    let maintenance = detail();
    assert_eq!(
        maintenance.status, 503,
        "survivor in maintenance answered non-503: {}",
        maintenance.body
    );

    fixture
        .client
        .execute(
            "UPDATE ir_module_module SET state='installed' WHERE name='base'",
            &[],
        )
        .await
        .unwrap();
    fixture
        .client
        .execute(
            "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();

    // The failed rebuilds above started the node's rebuild backoff (1 s, doubling; at most two
    // failures were recorded, so 2 s). Wait it out: this is the specified delay, not a race.
    std::thread::sleep(std::time::Duration::from_millis(2_100));
    let navigation = request(
        peer.port,
        "GET",
        "/erp",
        &[("cookie", fixture.cookies.as_str())],
        "",
    );
    assert_eq!(
        navigation.status, 200,
        "restored survivor navigation: {}",
        navigation.body
    );
    let restored = detail();
    assert_eq!(
        restored.status, 200,
        "restored survivor detail: {}",
        restored.body
    );
    let doc: Value = serde_json::from_str(&restored.body).unwrap();
    assert_eq!(doc["state"], "installed");

    drop(peer);
    fixture.cleanup().await;
}
