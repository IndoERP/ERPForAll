//! `POST /erp/session/api-key`: a password exchanged for a device API key through the same
//! login as the web form, against a real tenant Auth, WebApp and RpcService.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "auth_support/mod.rs"]
mod support;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, Response, StatusCode},
};
use erp_sdk::Value;
use erp_security::auth::HashPool;
use erp_server::{config::ServerConfig, web::WebApp};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;

const PROXY_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ROUTE: &str = "/erp/session/api-key";
const PASSWORD: &str = "staple-battery-horse";
const WRONG: &str = "battery-staple-wrong";
const DENIED: &str = r#"{"error":"Wrong login/password"}"#;
const COOLDOWN: &str = "Too many login attempts; try again later.";

#[derive(Debug)]
struct Host;
impl erp_rpc::RpcHost for Host {
    fn language_active(&self, _: &str) -> erp_fields::Result<bool> {
        Ok(false)
    }
    fn has_capability(
        &self,
        _: &erp_orm::Principal,
        _: &erp_orm::Capability,
    ) -> erp_fields::Result<bool> {
        Ok(false)
    }
}

struct Fixture {
    router: Router,
    auth: Arc<erp_security::auth::Auth>,
    pool: &'static HashPool,
    uid: i64,
    h: erp_testkit::Harness,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let world = support::world().await;
    let uid = support::user_with_password(&world.h, "device-user", PASSWORD).await;
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in [
        ("runtime", "postgres://runtime:dummy@127.0.0.1/test"),
        ("owner", "postgres://owner:dummy@127.0.0.1/test"),
        ("proxy", PROXY_TOKEN),
    ] {
        fs::write(dir.path().join(file), text).unwrap();
        fs::set_permissions(dir.path().join(file), fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(dir.path().join("server.toml"), "database='test'\ndatabase_url_file='runtime'\nowner_database_url_file='owner'\nbind='127.0.0.1:0'\npublic_origin='https://erp.example.test'\nproxy_mode='authenticated'\ntrusted_proxies=['127.0.0.1']\nproxy_token_file='proxy'\naddons_path=['addons']\nassets_manifest='manifest.json'\n").unwrap();
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let assets: Vec<_> = [("js", "text/javascript; charset=utf-8"), ("css", "text/css; charset=utf-8")].into_iter().map(|(ext, mime)| {
        let path = format!("client.{digest}.{ext}");
        fs::write(dir.path().join(&path), b"").unwrap();
        serde_json::json!({"name":format!("client.{ext}"),"path":path,"sha256":digest,"content_type":mime})
    }).collect();
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema":1,"assets":assets})).unwrap(),
    )
    .unwrap();
    let support::World { h, auth, pool, .. } = world;
    let auth = Arc::new(auth);
    let rpc = Arc::new(erp_rpc::RpcService::new(
        auth.orm().clone(),
        Host,
        erp_rpc::RpcConfig::default(),
    ));
    let app = Arc::new(
        WebApp::new(
            Arc::new(ServerConfig::load(&dir.path().join("server.toml")).unwrap()),
            Arc::clone(&auth),
        )
        .unwrap()
        .with_rpc(rpc),
    );
    app.set_login_available(true);
    Fixture {
        router: app.router("127.0.0.1".parse().unwrap()),
        auth,
        pool,
        uid,
        h,
        _dir: dir,
    }
}

fn body(login: &str, password: &str) -> String {
    serde_json::json!({"db":"test","login":login,"password":password,"device_name":"Field tablet"})
        .to_string()
}

impl Fixture {
    fn post(&self, path: &str, address: u8) -> axum::http::request::Builder {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("x-erp-proxy-token", PROXY_TOKEN)
            .header("x-forwarded-for", format!("192.0.2.{address}"))
            .header("content-type", "application/json")
    }
    async fn send(&self, request: Request<Body>) -> (Response<Body>, Vec<u8>) {
        let (parts, body) = self
            .router
            .clone()
            .oneshot(request)
            .await
            .unwrap()
            .into_parts();
        (
            Response::from_parts(parts, Body::empty()),
            to_bytes(body, 1 << 20).await.unwrap().to_vec(),
        )
    }
    async fn exchange(
        &self,
        login: &str,
        password: &str,
        address: u8,
    ) -> (Response<Body>, Vec<u8>) {
        self.send(
            self.post(ROUTE, address)
                .body(Body::from(body(login, password)))
                .unwrap(),
        )
        .await
    }
    async fn scalar(&self, sql: &str) -> i64 {
        let mut tx = self.h.begin().await.unwrap();
        let value = tx.query(sql, &[]).await.unwrap()[0].get(0);
        tx.rollback().await.unwrap();
        value
    }
    async fn sessions(&self) -> i64 {
        self.scalar("SELECT count(*) FROM erp_session").await
    }
    async fn keys(&self) -> i64 {
        self.scalar("SELECT count(*) FROM res_users_apikeys").await
    }
    /// The audited auth events, as text, read past the audit schema's privileges.
    async fn auth_events(&self) -> Vec<String> {
        let (client, connection) = self
            .h
            .db()
            .config()
            .connect(erp_testkit::tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        client
            .query(
                "SELECT e::text FROM erp_event.ir_audit_entry a, \
                 jsonb_array_elements(a.events) WITH ORDINALITY t(e, n) \
                 WHERE e->>'type' = 'auth' ORDER BY a.id, t.n",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    }
    /// A web login form post from `address`, with a fresh login page's cookie and token.
    async fn web_login(&self, password: &str, address: u8) -> (Response<Body>, Vec<u8>) {
        let (page, html) = self
            .send(
                Request::builder()
                    .uri("/erp/login")
                    .header("x-erp-proxy-token", PROXY_TOKEN)
                    .header("x-forwarded-for", format!("192.0.2.{address}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(page.status(), StatusCode::OK);
        let prefs = page.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let html = String::from_utf8(html).unwrap();
        let dto: serde_json::Value = serde_json::from_str(
            html.split("id=\"erp-login\">")
                .nth(1)
                .unwrap()
                .split("</script>")
                .next()
                .unwrap(),
        )
        .unwrap();
        let token = dto["csrf"].as_str().unwrap();
        self.send(
            Request::builder()
                .method("POST")
                .uri("/erp/login")
                .header("x-erp-proxy-token", PROXY_TOKEN)
                .header("x-forwarded-for", format!("192.0.2.{address}"))
                .header("cookie", prefs)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "db=test&login=device-user&password={password}&redirect=%2Ferp&csrf_token={token}"
                )))
                .unwrap(),
        )
        .await
    }
}

fn text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn refused(response: &Response<Body>, body: &[u8], status: StatusCode, error: &str) {
    assert_eq!(response.status(), status, "{}", text(body));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(body).unwrap(),
        serde_json::json!({ "error": error })
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(!response.headers().contains_key("set-cookie"));
}

#[tokio::test]
async fn a_password_buys_a_device_key_that_works_until_it_revokes_itself() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let mut tx = f.h.begin().await.unwrap();
    tx.execute("UPDATE res_groups SET api_key_duration = 7", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let sessions = f.sessions().await;
    let keys = f.keys().await;
    let before = f
        .scalar("SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::int8")
        .await;

    let (response, body) = f.exchange("device-user", PASSWORD, 1).await;

    let after = f
        .scalar("SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::int8")
        .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(&body));
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["content-type"], "application/json");
    assert!(!response.headers().contains_key("set-cookie"));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 3, "{json}");
    assert_eq!(json["uid"], f.uid);
    let key = erp_security::auth::Zeroizing::new(json["api_key"].as_str().unwrap().to_owned());
    // no browser session survives the exchange; one key exists
    assert_eq!(f.sessions().await, sessions);
    assert_eq!(f.keys().await, keys + 1);
    // the expiry is the group maximum (7 days), in UTC ISO 8601, as stored
    let mut tx = f.h.begin().await.unwrap();
    let row = &tx
        .query(
            "SELECT to_char(expiration_date, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
             (extract(epoch FROM expiration_date) * 1000000)::int8, name, user_id::int8 \
             FROM res_users_apikeys ORDER BY id DESC LIMIT 1",
            &[],
        )
        .await
        .unwrap()[0];
    assert_eq!(json["expires_at"], row.get::<_, String>(0));
    let week = 7 * 86_400_000_000_i64;
    let expiry: i64 = row.get(1);
    assert!(
        expiry > before + week - 1_000_000 && expiry <= after + week,
        "{expiry} not one week after [{before}, {after}]"
    );
    assert_eq!(row.get::<_, String>(2), "Field tablet");
    assert_eq!(row.get::<_, i64>(3), f.uid);
    tx.rollback().await.unwrap();

    // the key authenticates JSON-2
    let count = async |address: u8| {
        f.send(
            f.post("/erp/json/2/res.users/search_count", address)
                .header("authorization", format!("Bearer {}", key.as_str()))
                .body(Body::from(r#"{"domain": []}"#))
                .unwrap(),
        )
        .await
    };
    let (response, body) = count(2).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(&body));
    // the device signs out by revoking its own key through the existing route
    let (response, body) = f
        .send(
            f.post("/erp/json/2/res.users.apikeys/revoke", 3)
                .header("authorization", format!("Bearer {}", key.as_str()))
                .body(Body::from(
                    serde_json::json!({"key": key.as_str()}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(&body));
    let (response, body) = count(4).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "{}",
        text(&body)
    );
    assert_eq!(f.sessions().await, sessions);
}

/// A wrong password, an unknown login and an archived user get the same bytes; the two without
/// a stored hash to check still pay for a (dummy) verify.
#[tokio::test]
async fn a_json_media_type_with_a_charset_parameter_is_accepted() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    for media in [
        "application/json; charset=utf-8",
        "Application/JSON;charset=UTF-8",
    ] {
        let (response, body) = f
            .send(
                Request::builder()
                    .method("POST")
                    .uri(ROUTE)
                    .header("x-erp-proxy-token", PROXY_TOKEN)
                    .header("x-forwarded-for", "192.0.2.9")
                    .header("content-type", media)
                    .body(Body::from(body("device-user", PASSWORD)))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{media}: {}",
            text(&body)
        );
    }
}

#[tokio::test]
async fn refusals_are_byte_identical_and_cost_a_verify() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let archived = support::user_with_password(&f.h, "archived-user", PASSWORD).await;
    let mut tx = f.h.begin().await.unwrap();
    support::write(
        &mut tx,
        "res.users",
        &[archived],
        &[("active", Value::Bool(false))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let sessions = f.sessions().await;

    let (wrong, wrong_body) = f.exchange("device-user", WRONG, 10).await;
    let before = f.pool.stats();
    let (unknown, unknown_body) = f.exchange("nobody-here", PASSWORD, 11).await;
    let (inactive, inactive_body) = f.exchange("archived-user", PASSWORD, 12).await;
    let after = f.pool.stats();

    for (response, body) in [
        (&wrong, &wrong_body),
        (&unknown, &unknown_body),
        (&inactive, &inactive_body),
    ] {
        refused(
            response,
            body,
            StatusCode::UNAUTHORIZED,
            "Wrong login/password",
        );
        assert_eq!(body.as_slice(), DENIED.as_bytes());
    }
    assert_eq!(wrong_body, unknown_body);
    assert_eq!(unknown_body, inactive_body);
    assert_eq!(after.dummy_verifies - before.dummy_verifies, 2);
    assert_eq!(f.sessions().await, sessions);
    assert_eq!(f.keys().await, 0);
}

/// Repeated failures through this route cool the address down for the web form, and the web
/// form's failures cool it down for this route: one throttle.
#[tokio::test]
async fn the_cooldown_is_shared_with_the_web_login_both_ways() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    support::set_param(&f.h, "base.login_cooldown_after", "2").await;
    support::set_param(&f.h, "base.login_cooldown_after_login", "100").await;

    // this route's failures, then both entries are refused
    for _ in 0..2 {
        let (response, body) = f.exchange("device-user", WRONG, 20).await;
        refused(
            &response,
            &body,
            StatusCode::UNAUTHORIZED,
            "Wrong login/password",
        );
    }
    let (response, body) = f.exchange("device-user", PASSWORD, 20).await;
    refused(&response, &body, StatusCode::TOO_MANY_REQUESTS, COOLDOWN);
    let (response, body) = f.web_login(PASSWORD, 20).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(text(&body), COOLDOWN);

    // the web form's failures, then this route is refused
    for _ in 0..2 {
        let (response, body) = f.web_login(WRONG, 21).await;
        assert_eq!(response.status(), StatusCode::OK, "{}", text(&body));
        assert!(text(&body).contains("Wrong login/password"));
    }
    let (response, body) = f.exchange("device-user", PASSWORD, 21).await;
    refused(&response, &body, StatusCode::TOO_MANY_REQUESTS, COOLDOWN);
    assert_eq!(f.keys().await, 0);
}

/// One `api_key.issued` event per key, carrying no secret; a failed commit leaves neither a key,
/// nor an event, nor a session.
#[tokio::test]
async fn issuance_is_audited_once_and_not_at_all_when_the_commit_fails() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let sessions = f.sessions().await;
    let events = f.auth_events().await;

    let (response, body) = f.exchange("device-user", PASSWORD, 30).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let key = json["api_key"].as_str().unwrap().to_owned();
    let added: Vec<String> = f.auth_events().await.split_off(events.len());
    assert_eq!(added.len(), 1, "{added:?}");
    let event: serde_json::Value = serde_json::from_str(&added[0]).unwrap();
    assert_eq!(event["event"], "api_key.issued");
    assert!(!added[0].contains(&key) && !added[0].contains(&key[..16]));
    assert!(!added[0].contains(PASSWORD));

    // the commit fails: a deferred check refuses every new key at COMMIT time
    let (client, connection) =
        f.h.db()
            .config()
            .connect(erp_testkit::tokio_postgres::NoTls)
            .await
            .unwrap();
    tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE FUNCTION refuse_key() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN RAISE EXCEPTION 'refused at commit'; END$$; \
             CREATE CONSTRAINT TRIGGER refuse_key AFTER INSERT ON res_users_apikeys \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION refuse_key();",
        )
        .await
        .unwrap();
    let events = f.auth_events().await;
    let keys = f.keys().await;
    let (response, body) = f.exchange("device-user", PASSWORD, 31).await;
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        text(&body)
    );
    assert!(!response.headers().contains_key("set-cookie"));
    assert!(!text(&body).contains("api_key"));
    assert_eq!(f.auth_events().await, events);
    assert_eq!(f.keys().await, keys);
    assert_eq!(f.sessions().await, sessions);
}

/// The policy gates are unchanged: with programmatic keys off, or at the per-user limit, the
/// password was right but no key is issued, and the browser session is still discarded.
#[tokio::test]
async fn issuance_refusals_answer_403_and_leave_no_session() {
    let f = fixture().await;
    let sessions = f.sessions().await;
    let (response, body) = f.exchange("device-user", PASSWORD, 40).await;
    refused(
        &response,
        &body,
        StatusCode::FORBIDDEN,
        "Programmatic API keys are not enabled",
    );
    assert_eq!(f.sessions().await, sessions);
    assert_eq!(f.keys().await, 0);

    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    support::set_param(&f.h, "base.programmatic_api_keys_limit", "1").await;
    let (response, body) = f.exchange("device-user", PASSWORD, 41).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(&body));
    let (response, body) = f.exchange("device-user", PASSWORD, 42).await;
    refused(
        &response,
        &body,
        StatusCode::FORBIDDEN,
        "Limit of 1 API keys is reached for programmatic creation",
    );
    assert_eq!(f.sessions().await, sessions);
    assert_eq!(f.keys().await, 1);
}

/// Malformed, oversized and cross-origin requests are refused before the password is checked.
#[tokio::test]
async fn malformed_and_cross_origin_requests_are_refused_without_a_verify() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let jobs = f.pool.stats().jobs;
    let send = async |builder: axum::http::request::Builder, payload: String| {
        f.send(builder.body(Body::from(payload)).unwrap()).await
    };
    let good = body("device-user", PASSWORD);

    let oversized = serde_json::json!({"db":"test","login":"device-user","password":PASSWORD,
        "device_name":"x".repeat(16 * 1024)})
    .to_string();
    let (response, body) = send(f.post(ROUTE, 50), oversized).await;
    refused(
        &response,
        &body,
        StatusCode::PAYLOAD_TOO_LARGE,
        "authentication body exceeds 16384 bytes",
    );
    let unknown = serde_json::json!({"db":"test","login":"device-user","password":PASSWORD,
        "device_name":"Field tablet","scope":"mcp"})
    .to_string();
    let (response, body) = send(f.post(ROUTE, 50), unknown).await;
    refused(
        &response,
        &body,
        StatusCode::BAD_REQUEST,
        "invalid JSON body: unknown, duplicate or missing field",
    );
    let duplicate = format!(
        r#"{{"db":"test","login":"device-user","login":"other","password":"{PASSWORD}","device_name":"Field tablet"}}"#
    );
    let (response, body) = send(f.post(ROUTE, 50), duplicate).await;
    refused(
        &response,
        &body,
        StatusCode::BAD_REQUEST,
        "invalid JSON body: unknown, duplicate or missing field",
    );
    let missing =
        serde_json::json!({"db":"test","login":"device-user","password":PASSWORD}).to_string();
    let (response, body) = send(f.post(ROUTE, 50), missing).await;
    refused(
        &response,
        &body,
        StatusCode::BAD_REQUEST,
        "invalid JSON body: unknown, duplicate or missing field",
    );
    let other_db = serde_json::json!({"db":"other","login":"device-user","password":PASSWORD,
        "device_name":"Field tablet"})
    .to_string();
    let (response, body) = send(f.post(ROUTE, 50), other_db).await;
    refused(
        &response,
        &body,
        StatusCode::BAD_REQUEST,
        "Database not found.",
    );
    let (response, body) = send(
        Request::builder()
            .method("POST")
            .uri(ROUTE)
            .header("x-erp-proxy-token", PROXY_TOKEN)
            .header("x-forwarded-for", "192.0.2.50")
            .header("content-type", "application/x-www-form-urlencoded"),
        good.clone(),
    )
    .await;
    refused(
        &response,
        &body,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "expected application/json",
    );
    for (name, value) in [
        ("origin", "https://attacker.example"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let (response, body) = send(f.post(ROUTE, 50).header(name, value), good.clone()).await;
        refused(
            &response,
            &body,
            StatusCode::FORBIDDEN,
            "cross-origin request refused",
        );
    }
    // the server's own origin and a native client's bare request are accepted
    let (response, body) = send(
        f.post(ROUTE, 51)
            .header("origin", "https://erp.example.test"),
        good.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(&body));
    assert_eq!(
        f.pool.stats().jobs - jobs,
        1,
        "only the accepted request verified"
    );
    // GET is not served
    let (response, _) = f
        .send(
            Request::builder()
                .uri(ROUTE)
                .header("x-erp-proxy-token", PROXY_TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_ne!(response.status(), StatusCode::OK);
}

/// Neither the password nor the issued key reaches a log line or an error body, on success and
/// on every kind of failure.
#[tokio::test]
async fn secrets_never_reach_the_logs_or_an_error_body() {
    let capture = log_capture::start();
    let f = fixture().await;
    let mut bodies = Vec::new();
    // refused after the password was accepted (programmatic keys off)
    bodies.push(f.exchange("device-user", PASSWORD, 60).await.1);
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    bodies.push(f.exchange("device-user", WRONG, 61).await.1);
    bodies.push(f.exchange(PASSWORD, PASSWORD, 62).await.1);
    let (response, body) = f.exchange("device-user", PASSWORD, 63).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", text(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let key = json["api_key"].as_str().unwrap().to_owned();
    // a refusal of the issued key's owner after it worked once (limit reached)
    support::set_param(&f.h, "base.programmatic_api_keys_limit", "1").await;
    bodies.push(f.exchange("device-user", PASSWORD, 64).await.1);

    for body in &bodies {
        let body = text(body);
        assert!(!body.contains(PASSWORD) && !body.contains(WRONG), "{body}");
        assert!(!body.contains(&key), "{body}");
    }
    let lines = capture.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("/erp/session/api-key")),
        "the refusals were logged: {lines:?}"
    );
    for line in &lines {
        assert!(!line.contains(PASSWORD) && !line.contains(WRONG), "{line}");
        assert!(!line.contains(&key), "{line}");
    }
}

/// Every event of every level and target, with all its fields, from all threads (the login's
/// verdict runs in a task of its own and the hash on the pool's threads).
mod log_capture {
    use std::fmt;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    static LINES: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();

    pub struct Capture;
    impl Capture {
        pub fn lines(&self) -> Vec<String> {
            LINES
                .get()
                .map(|lines| lines.lock().unwrap_or_else(PoisonError::into_inner).clone())
                .unwrap_or_default()
        }
    }

    struct Fields(String);
    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }

    struct All(Arc<Mutex<Vec<String>>>);
    impl All {
        fn keep(&self, line: String) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line);
        }
    }
    impl Subscriber for All {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, attributes: &Attributes<'_>) -> Id {
            let mut fields = Fields(attributes.metadata().name().to_owned());
            attributes.record(&mut fields);
            self.keep(fields.0);
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, values: &Record<'_>) {
            let mut fields = Fields(String::new());
            values.record(&mut fields);
            self.keep(fields.0);
        }
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields(event.metadata().target().to_owned());
            event.record(&mut fields);
            self.keep(fields.0);
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    /// Install the process-wide capture once; every test of this binary shares it.
    pub fn start() -> Capture {
        LINES.get_or_init(|| {
            let lines = Arc::new(Mutex::new(Vec::new()));
            tracing::subscriber::set_global_default(All(Arc::clone(&lines)))
                .expect("no other global subscriber in this test binary");
            lines
        });
        Capture
    }
}

#[tokio::test]
async fn mcp_connection_restrictions_and_owner_lifecycle() {
    use erp_security::auth::{
        ApiKeyRequest, CredentialOptions, apikey_manage, apikey_store::KeyScope,
    };
    use serde_json::json;
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let exchange = async |access: serde_json::Value| {
        f.send(f.post(ROUTE, 9).body(Body::from(json!({
            "db":"test","login":"device-user","password":PASSWORD,"device_name":"Assistant","audience":"mcp","mcp_access":access
        }).to_string())).unwrap()).await
    };
    let start = f.keys().await;
    for invalid in [
        json!(null),
        json!([false]),
        json!({}),
        json!({"read_only":false,"models":[]}),
        json!({"read_only":true,"methods":null}),
        json!({"read_only":true,"methods":["read","read"]}),
        json!({"read_only":true,"models":["sale.*"]}),
        json!({"read_only":true,"capabilities":"subject"}),
        json!({"read_only":true,"expires_at":"2000-01-01T00:00:00Z"}),
    ] {
        let (response, _) = exchange(invalid).await;
        assert!(response.status().is_client_error());
    }
    assert_eq!(f.keys().await, start);
    let mut tx = f.h.begin().await.unwrap();
    let expiry:String = tx.query("SELECT to_char((now() AT TIME ZONE 'utc') + interval '1 hour','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')", &[]).await.unwrap()[0].get(0);
    tx.rollback().await.unwrap();
    let (res, bytes) = exchange(json!({"read_only":true,"models":["res.users"],"methods":["search_read"],"expires_at":expiry})).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["cache-control"], "no-store");
    let data: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(data["expires_at"], expiry);
    let token = data["api_key"].as_str().unwrap();
    let check = async |audience| {
        f.auth
            .begin_api_key(
                ApiKeyRequest::new(token, audience, "192.0.2.70".parse().unwrap()),
                CredentialOptions::new(),
            )
            .await
    };
    assert!(check(KeyScope::Rpc).await.is_err());
    let mut session = check(KeyScope::Mcp).await.unwrap();
    let env = session.tx.env();
    let scope = env.principal().scope().unwrap();
    assert!(
        scope.read_only()
            && scope.models().allows("res.users")
            && scope.methods().allows("search_read")
    );
    assert!(!scope.models().allows("res.partner") && !scope.methods().allows("create"));
    assert!(apikey_manage::require_connection_owner(&session.tx).is_err());
    let id = scope.key().get();
    session.tx.rollback().await.unwrap();
    drop(session.last_use.take());

    let as_user = async |login: &str| {
        let actor = support::login(&f.auth, login, PASSWORD).await.unwrap();
        f.auth
            .begin_session(
                actor.session().secret(),
                CredentialOptions::new()
                    .operation_identity(erp_orm::OperationIdentity::generate().unwrap()),
            )
            .await
            .unwrap()
            .tx
    };
    support::user_with_password(&f.h, "mcp-peer", PASSWORD).await;
    let mut peer = as_user("mcp-peer").await;
    let result: Value = peer
        .env()
        .records("res.users")
        .unwrap()
        .call(&mut peer, "erp_mcp_connections", (None::<i64>,))
        .await
        .unwrap();
    assert!(matches!(result, Value::Json(doc) if doc["items"].as_array().unwrap().is_empty()));
    assert!(
        apikey_manage::revoke_mcp_connection(&mut peer, id)
            .await
            .is_err()
    );
    peer.rollback().await.unwrap();
    let mut owner = as_user("device-user").await;
    let users = owner.env().records("res.users").unwrap();
    let result: Value = users
        .call(&mut owner, "erp_mcp_connections", (None::<i64>,))
        .await
        .unwrap();
    assert!(
        matches!(result, Value::Json(doc) if doc["items"][0]["name"] == "Assistant" &&
        doc["items"][0]["models"] == json!(["res.users"]) && doc["items"][0]["id"] == id &&
        !doc.to_string().contains(token) && doc["items"][0].as_object().unwrap().len() == 8)
    );
    let removed: bool = users
        .call(&mut owner, "erp_revoke_mcp_connection", (id,))
        .await
        .unwrap();
    assert!(removed);
    owner.commit().await.unwrap();
    assert!(check(KeyScope::Mcp).await.is_err());
}
