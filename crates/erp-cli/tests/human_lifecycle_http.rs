//! Native lifecycle HTTP boundaries using the actual binary and a nonowner runtime role.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration fixture assertions and shared helpers"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_denials_have_no_effects() {
    let fixture = NativeFixture::start().await;
    let port = fixture.port;
    let cookie = &fixture.cookies;
    let uid: i64 = fixture
        .client
        .query_one("SELECT id FROM res_users WHERE login='fixture-admin'", &[])
        .await
        .unwrap()
        .get(0);
    // Owner-only fixture seeding creates a real, correctly hashed user key. The HTTP caller
    // must still be refused by the session-only administrative Read binders.
    let secret: String = fixture
        .client
        .query_one(
            "SELECT value FROM ir_config_parameter WHERE key='database.secret'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let secret = erp_security::auth::secret::DatabaseSecret::from_stored(&secret).unwrap();
    let hasher = erp_security::auth::apikey::ApiKeyHasher::derive(&secret).unwrap();
    let key = erp_security::auth::apikey::GeneratedApiKey::generate().unwrap();
    let digest = hasher.digest(&key.token());
    fixture.client.execute("INSERT INTO res_users_apikeys(name,user_id,created_by,kind,scope,digest,digest_kid,prefix,read_only,capabilities,create_date) VALUES('lifecycle-read-boundary',$1,$1,'user','rpc',$2,$3,$4,false,'subject',now() AT TIME ZONE 'utc')",&[&uid,&digest.bytes().as_slice(),&i32::from(digest.kid()),&key.token().display_prefix()]).await.unwrap();
    let bearer = format!("Bearer {}", key.expose());
    let before: serde_json::Value = fixture.client.query_one("SELECT jsonb_build_object('modules',(SELECT jsonb_agg(to_jsonb(m) ORDER BY id) FROM ir_module_module m),'audit',(SELECT count(*) FROM erp_module_lifecycle_audit),'identities',(SELECT jsonb_agg(to_jsonb(i) ORDER BY name) FROM installed_addon_identity i),'identity_audit',(SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM installed_addon_identity_audit a))", &[]).await.unwrap().get(0);
    for path in ["/erp/module/upgrade", "/erp/module/uninstall"] {
        let body = if path.ends_with("uninstall") {
            format!(
                r#"{{"modules":["base"],"impact":"uninstall-v1:{}"}}"#,
                "0".repeat(64)
            )
        } else {
            r#"{"modules":["base"]}"#.to_owned()
        };
        for (headers, status, message) in [
            (
                vec![
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "same-origin"),
                ],
                401,
                "user is not connected",
            ),
            (
                vec![
                    ("cookie", cookie.as_str()),
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "cross-site"),
                ],
                403,
                "Missing \"Authorization\" or Sec-headers for interactive usage.",
            ),
            (
                vec![
                    ("cookie", cookie.as_str()),
                    ("content-type", "text/plain"),
                    ("sec-fetch-site", "same-origin"),
                ],
                415,
                "expected application/json",
            ),
            (
                vec![
                    ("cookie", cookie.as_str()),
                    ("authorization", "Bearer invalid"),
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "same-origin"),
                ],
                403,
                "Lifecycle writes require only a primary browser session",
            ),
        ] {
            let reply = request(port, "POST", path, &headers, &body);
            assert_eq!(reply.status, status, "{path}: {}", reply.body);
            assert_eq!(reply.body, message);
        }
        let reply = request(port, "GET", path, &[("cookie", cookie)], "");
        assert_eq!(reply.status, 405, "{}", reply.body);
        assert_eq!(reply.body, "POST required");
    }
    for (body, status, name, message) in [
        (
            r#"{"name":"sample_library","limit":null}"#,
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            "argument 'limit' of ir.module.module.erp_addon_detail: expected a non-negative integer, got null",
        ),
        (
            r#"{"name":"sample_library","menus_cursor":null}"#,
            422,
            "werkzeug.exceptions.UnprocessableEntity",
            "argument 'menus_cursor' of ir.module.module.erp_addon_detail: expected a string, got null",
        ),
        (
            r#"{"name":"sample_library","limit":101}"#,
            422,
            "erp.exceptions.ValidationError",
            "Addon detail limit must be between 1 and 100",
        ),
        (
            r#"{"name":"sample_library","dependencies_cursor":"forged"}"#,
            422,
            "erp.exceptions.ValidationError",
            "Addon detail cursor is malformed or stale",
        ),
    ] {
        let reply = json_post(
            port,
            cookie,
            "/erp/json/2/ir.module.module/erp_addon_detail",
            body,
        );
        assert_eq!(
            reply.status, status,
            "invalid detail accepted: {}",
            reply.body
        );
        let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(value["name"], name);
        assert_eq!(value["message"], message);
    }
    let preview = json_post(
        port,
        cookie,
        "/erp/json/2/ir.module.module/erp_preview_uninstall",
        r#"{"modules":["base"]}"#,
    );
    assert_eq!(preview.status, 422, "{}", preview.body);
    let value: serde_json::Value = serde_json::from_str(&preview.body).unwrap();
    assert_eq!(value["name"], "erp.exceptions.ValidationError");
    assert_eq!(
        value["message"],
        "Uninstall cannot remove addon 'base' required by protected addon 'web'"
    );
    assert!(value.get("fingerprint").is_none());
    let uninstall_body = format!(
        r#"{{"modules":["base"],"impact":"uninstall-v1:{}"}}"#,
        "0".repeat(64)
    );
    for (path, body) in [
        (
            "/erp/json/2/ir.module.module/erp_addon_detail",
            r#"{"name":"sample_library"}"#,
        ),
        (
            "/erp/json/2/ir.module.module/erp_preview_uninstall",
            r#"{"modules":["base"]}"#,
        ),
        ("/erp/module/uninstall", uninstall_body.as_str()),
    ] {
        let denied = request(
            port,
            "POST",
            path,
            &[
                ("authorization", &bearer),
                ("content-type", "application/json"),
            ],
            body,
        );
        assert_eq!(
            denied.status, 403,
            "API-key administration: {}",
            denied.body
        );
        if path == "/erp/module/uninstall" {
            assert_eq!(
                denied.body,
                "Lifecycle writes require only a primary browser session"
            );
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&denied.body).unwrap();
        assert_eq!(value["name"], "erp.exceptions.AccessError");
        assert_eq!(
            value["message"],
            "Addon administration requires a resolved, unelevated primary session user"
        );
    }
    let gid: i64 = fixture
        .client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system'",
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
    for (path, body) in [
        (
            "/erp/json/2/ir.module.module/erp_addon_detail",
            r#"{"name":"sample_library"}"#,
        ),
        (
            "/erp/json/2/ir.module.module/erp_preview_uninstall",
            r#"{"modules":["base"]}"#,
        ),
        ("/erp/module/uninstall", uninstall_body.as_str()),
    ] {
        let denied = json_post(port, cookie, path, body);
        assert_eq!(denied.status, 403, "nonadmin read: {}", denied.body);
        let value: serde_json::Value = serde_json::from_str(&denied.body).unwrap();
        assert_eq!(value["name"], "erp.exceptions.AccessError");
        assert_eq!(
            value["message"],
            "Addon administration requires system administration authority"
        );
    }
    fixture
        .client
        .execute(
            "INSERT INTO res_groups_users_rel(gid,uid) VALUES($1,$2)",
            &[&gid, &uid],
        )
        .await
        .unwrap();
    fixture
        .client
        .execute("DELETE FROM erp_session WHERE user_id=$1", &[&uid])
        .await
        .unwrap();
    for (path, body) in [
        ("/erp/module/upgrade", r#"{"modules":["base"]}"#),
        ("/erp/module/uninstall", uninstall_body.as_str()),
        (
            "/erp/json/2/ir.module.module/erp_addon_detail",
            r#"{"name":"sample_library"}"#,
        ),
        (
            "/erp/json/2/ir.module.module/erp_preview_uninstall",
            r#"{"modules":["base"]}"#,
        ),
    ] {
        let denied = json_post(port, cookie, path, body);
        assert_eq!(denied.status, 401, "revoked session: {}", denied.body);
        assert_eq!(denied.body, "user is not connected");
    }
    let after: serde_json::Value = fixture.client.query_one("SELECT jsonb_build_object('modules',(SELECT jsonb_agg(to_jsonb(m) ORDER BY id) FROM ir_module_module m),'audit',(SELECT count(*) FROM erp_module_lifecycle_audit),'identities',(SELECT jsonb_agg(to_jsonb(i) ORDER BY name) FROM installed_addon_identity i),'identity_audit',(SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM installed_addon_identity_audit a))", &[]).await.unwrap().get(0);
    assert_eq!(
        after, before,
        "denied lifecycle requests changed durable module state or audit"
    );
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_previews_dependencies_and_publishes_only_the_fresh_plan() {
    let fixture = NativeFixture::start_with_addons(|addons| {
        for (name, depends) in [("life_notes", "web"), ("life_notes_extra", "life_notes")] {
            let addon = addons.join(name);
            std::fs::create_dir_all(addon.join("models")).unwrap();
            std::fs::write(
                addon.join("manifest.toml"),
                format!("name='{name}'\nversion='1.0.0'\nlicense='MIT'\ndepends=['{depends}']\n"),
            )
            .unwrap();
            std::fs::write(
                addon.join("models/notes.toml"),
                format!(
                    r#"
[[model]]
name = "{name}.note"
description = "Note"

[[model.field]]
name = "name"
type = "char"
required = true
"#
                ),
            )
            .unwrap();
        }
    })
    .await;

    let post = |path: &str, body: &str| json_post(fixture.port, &fixture.cookies, path, body);
    let install = post("/erp/module/install", r#"{"modules":["life_notes_extra"]}"#);
    assert_eq!(install.status, 200, "install: {}", install.body);
    let installed: serde_json::Value = serde_json::from_str(&install.body).unwrap();
    for name in ["life_notes", "life_notes_extra"] {
        assert!(
            installed["installed"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == name)
        );
    }

    let upgrade = post("/erp/module/upgrade", r#"{"modules":["life_notes"]}"#);
    assert_eq!(upgrade.status, 200, "upgrade: {}", upgrade.body);
    let upgraded: serde_json::Value = serde_json::from_str(&upgrade.body).unwrap();
    assert!(
        upgraded["upgraded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "life_notes")
    );
    let detail = post(
        "/erp/json/2/ir.module.module/erp_addon_detail",
        r#"{"name":"life_notes"}"#,
    );
    assert_eq!(detail.status, 200, "detail: {}", detail.body);
    let document: serde_json::Value = serde_json::from_str(&detail.body).unwrap();
    assert!(document["uninstall_refusal"].is_null());
    assert_eq!(
        document["dependents"]["items"],
        serde_json::json!(["life_notes_extra"])
    );

    const SNAPSHOT: &str = "SELECT jsonb_build_object(
        'modules',(SELECT jsonb_agg(to_jsonb(m) ORDER BY id) FROM ir_module_module m),
        'audit',(SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM erp_module_lifecycle_audit a),
        'identities',(SELECT jsonb_agg(to_jsonb(i) ORDER BY name) FROM installed_addon_identity i),
        'assets',(SELECT jsonb_agg(bundle_key ORDER BY bundle_key) FROM ir_ui_asset_bundle WHERE expires_at IS NULL),
        'signals',(SELECT jsonb_agg(to_jsonb(g) ORDER BY key,id) FROM erp_signal_generation g))";
    let snapshot = || async {
        fixture
            .client
            .query_one(SNAPSHOT, &[])
            .await
            .unwrap()
            .get::<_, serde_json::Value>(0)
    };
    let before = snapshot().await;
    let preview = || {
        let reply = post(
            "/erp/json/2/ir.module.module/erp_preview_uninstall",
            r#"{"modules":["life_notes"]}"#,
        );
        assert_eq!(reply.status, 200, "preview: {}", reply.body);
        serde_json::from_str::<serde_json::Value>(&reply.body).unwrap()
    };
    let first = preview();
    assert_eq!(
        first["resolved"],
        serde_json::json!(["life_notes", "life_notes_extra"])
    );
    let effects = first["effects"].as_array().unwrap();
    assert!(
        effects
            .iter()
            .any(|effect| effect["component"] == "Addon packages")
    );
    let assets = effects
        .iter()
        .find(|effect| effect["component"] == "Web assets")
        .unwrap();
    assert_eq!(assets["previous"], before["assets"][0]);
    assert_ne!(assets["previous"], assets["next"]);
    assert_eq!(preview(), first);
    // Dismissing the dialog submits no write. Repeated previews preserve every durable effect.
    assert_eq!(snapshot().await, before);

    fixture
        .client
        .execute(
            "INSERT INTO life_notes_note (name) VALUES ('new note')",
            &[],
        )
        .await
        .unwrap();
    let confirm = |preview: &serde_json::Value| {
        post(
            "/erp/module/uninstall",
            &serde_json::json!({
                "modules":["life_notes"],
                "impact": preview["fingerprint"],
            })
            .to_string(),
        )
    };
    let stale = confirm(&first);
    assert_eq!(stale.status, 409, "stale preview: {}", stale.body);
    assert!(stale.body.contains("preview again"), "{}", stale.body);
    assert_eq!(snapshot().await, before);
    let name: String = fixture
        .client
        .query_one("SELECT name FROM life_notes_note", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(name, "new note");

    let fresh = preview();
    assert_ne!(fresh["fingerprint"], first["fingerprint"]);
    assert_eq!(
        fresh["dropped_models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["model"] == "life_notes.note")
            .unwrap()["row_count"],
        "1"
    );
    let success = confirm(&fresh);
    assert_eq!(success.status, 200, "uninstall: {}", success.body);
    let removed: serde_json::Value = serde_json::from_str(&success.body).unwrap();
    assert_eq!(removed["uninstalled"], fresh["resolved"]);
    assert_eq!(snapshot().await["assets"][0], assets["next"]);
    for name in ["life_notes", "life_notes_extra"] {
        assert_eq!(
            state_of(&catalogue(fixture.port, &fixture.cookies), name).as_deref(),
            Some("uninstalled")
        );
    }
    let durable: bool = fixture.client.query_one(
        "SELECT to_regclass('life_notes_note') IS NULL AND
                to_regclass('life_notes_extra_note') IS NULL AND
                EXISTS (SELECT 1 FROM erp_module_lifecycle_audit WHERE operation='uninstall' AND fingerprint=$1)",
        &[&fresh["fingerprint"].as_str().unwrap()],
    ).await.unwrap().get(0);
    assert!(durable, "confirmed effects and audit must commit together");
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_upgrade_reports_committed_runtime_grants_failure() {
    let fixture = NativeFixture::start().await;
    let installed = json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/module/install",
        r#"{"modules":["sample_library"]}"#,
    );
    assert_eq!(installed.status, 200, "install fixture: {}", installed.body);
    let audit_before: i64 = fixture.client.query_one(
        "SELECT count(*) FROM erp_module_lifecycle_audit WHERE operation='upgrade' AND requested=ARRAY['sample_library']::text[]", &[]
    ).await.unwrap().get(0);
    // A fixture-owned, nonowner role introduces an inherited read right that direct REVOKE
    // from the runtime role cannot repair. No existing role or shared database is changed.
    let leak_role = format!("life_grant_{}_{}", std::process::id(), fixture.port);
    fixture.client.batch_execute(&format!(
        "CREATE ROLE {leak} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; GRANT SELECT ON public.installed_addon_identity TO {leak}; GRANT {leak} TO {runtime}",
        leak=erp_sql::quote(&leak_role), runtime=erp_sql::quote(&fixture.role)
    )).await.unwrap();
    let inherited: bool = fixture
        .client
        .query_one(
            "SELECT has_table_privilege($1,'public.installed_addon_identity','SELECT')",
            &[&fixture.role],
        )
        .await
        .unwrap()
        .get(0);
    let reply = json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/module/upgrade",
        r#"{"modules":["sample_library"]}"#,
    );
    let audit_after: i64 = fixture.client.query_one(
        "SELECT count(*) FROM erp_module_lifecycle_audit WHERE operation='upgrade' AND requested=ARRAY['sample_library']::text[]", &[]
    ).await.unwrap().get(0);
    let state: String = fixture
        .client
        .query_one(
            "SELECT state FROM ir_module_module WHERE name='sample_library'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    // Cleanup precedes assertions so the expected RED stage mismatch cannot leave this
    // fixture's inherited membership or role behind on the shared cluster.
    fixture.client.batch_execute(&format!(
        "REVOKE {leak} FROM {runtime}; REVOKE ALL ON public.installed_addon_identity FROM {leak}; DROP ROLE {leak}",
        leak=erp_sql::quote(&leak_role), runtime=erp_sql::quote(&fixture.role)
    )).await.unwrap();
    fixture.cleanup().await;
    assert!(
        inherited,
        "fault injection must establish the effective inherited privilege"
    );
    assert_eq!(
        audit_after,
        audit_before + 1,
        "native engine upgrade audit must persist"
    );
    assert_eq!(
        state, "installed",
        "publication failure must not pretend the engine rolled back"
    );
    assert_eq!(reply.status, 503, "{}", reply.body);
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "name":"erp.lifecycle.committed",
            "message":"Upgrade committed but runtime grant publication is incomplete; operator recovery is required",
            "outcome":"committed",
            "stage":"runtime_grants"
        })
    );
}

/// `force_demo_specs.rs`)
/// administrator session succeeds and one `force_demo` audit row is written)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_force_demo_requires_confirmation_a_browser_session_and_is_audited() {
    let fixture = NativeFixture::start().await;
    let port = fixture.port;
    let cookie = &fixture.cookies;
    let audit = || async {
        let count: i64 = fixture
            .client
            .query_one(
                "SELECT count(*) FROM erp_module_lifecycle_audit WHERE operation='force_demo'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        count
    };
    let demo_base = || async {
        let flag: bool = fixture
            .client
            .query_one("SELECT demo FROM ir_module_module WHERE name='base'", &[])
            .await
            .unwrap()
            .get(0);
        flag
    };
    let path = "/erp/module/force-demo";
    let same_origin = [
        ("cookie", cookie.as_str()),
        ("content-type", "application/json"),
        ("sec-fetch-site", "same-origin"),
    ];

    // an API key or any Authorization header is refused before anything runs
    let reply = request(
        port,
        "POST",
        path,
        &[
            ("cookie", cookie.as_str()),
            ("authorization", "Bearer invalid"),
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
        ],
        r#"{"confirm":true}"#,
    );
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert_eq!(
        reply.body,
        "Lifecycle writes require only a primary browser session"
    );
    // the irreversible confirmation is part of the request
    for body in [r#"{}"#, r#"{"confirm":false}"#, r#"{"confirm":true,"x":1}"#] {
        let reply = request(port, "POST", path, &same_origin, body);
        assert_eq!(reply.status, 400, "{body}: {}", reply.body);
        assert!(reply.body.contains("irreversible"), "{}", reply.body);
    }
    assert_eq!((audit().await, demo_base().await), (0, false));

    let reply = json_post(port, cookie, path, r#"{"confirm":true}"#);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    // `loaded` lists the installed addons that have demo files; of the fixture's addons only
    // `base` ships demo records
    assert_eq!(body["loaded"], serde_json::json!(["base"]), "{body}");
    assert_eq!(body["failed"], serde_json::json!([]), "{body}");
    assert!(body["registry_version"].is_string(), "{body}");
    assert_eq!((audit().await, demo_base().await), (1, true));

    // running it again is allowed and audited again
    let reply = json_post(port, cookie, path, r#"{"confirm":true}"#);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(audit().await, 2);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_read_audit_policy_survives_lifecycle_recomposition() {
    let fixture = NativeFixture::start_with_config("\n[audit_read_policy]\nall_users=true\n").await;
    async fn count(fixture: &NativeFixture) -> i64 {
        fixture.client.query_one("SELECT count(*) FROM erp_event.ir_audit_entry WHERE entry_kind='attempt' AND outcome='read_ok' AND model='res.partner' AND method='search'", &[]).await.unwrap().get(0)
    }
    let before = count(&fixture).await;
    let reply = json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/res.partner/search",
        r#"{"domain":[],"limit":1}"#,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(count(&fixture).await, before + 1);
    let installed = json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/module/install",
        r#"{"modules":["sample_library"]}"#,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    let before = count(&fixture).await;
    let reply = json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/res.partner/search",
        r#"{"domain":[],"limit":1}"#,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(count(&fixture).await, before + 1);
    fixture.cleanup().await;
}
