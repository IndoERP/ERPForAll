//! Native bearer transport against a real tenant Auth, WebApp and RpcService.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "auth_support/mod.rs"]
mod support;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, Response, StatusCode},
};
use erp_fields::DateTime;
use erp_orm::UserId;
use erp_security::auth::{
    Auth,
    apikey::ApiKeyHasher,
    apikey_store::{IssueRequest, IssuedSecret, KeyKind, KeyScope, issue},
    secret::DatabaseSecret,
};
use erp_server::{config::ServerConfig, web::WebApp};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;

const PROXY_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PATH: &str = "/erp/json/2/res.users/search_count";
const BODY: &str = r#"{"domain": []}"#;

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
    auth: Arc<Auth>,
    key: IssuedSecret,
    key_id: i64,
    /// The user that owns the fixture's key and the cookie session.
    owner_uid: i64,
    session: String,
    other_session: String,
    user_count: i64,
    h: erp_testkit::Harness,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let world = support::world().await;
    let uid = support::user_with_password(&world.h, "key-user", "correct-horse").await;
    support::user_with_password(&world.h, "other-user", "other-horse").await;
    let login = support::login(&world.auth, "key-user", "correct-horse")
        .await
        .unwrap();
    let other_login = support::login(&world.auth, "other-user", "other-horse")
        .await
        .unwrap();
    let session = format!(
        "session_id=test.{}",
        login.session().secret().to_cookie().as_str()
    );
    let other_session = format!(
        "session_id=test.{}",
        other_login.session().secret().to_cookie().as_str()
    );

    let mut tx = world.h.begin().await.unwrap();
    let secret: String = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key='database.secret'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let hasher = ApiKeyHasher::derive(&DatabaseSecret::from_stored(&secret).unwrap()).unwrap();
    let expiry = DateTime::from_micros(
        tx.query(
            "SELECT (extract(epoch FROM now()) * 1000000)::int8 + 3600000000",
            &[],
        )
        .await
        .unwrap()[0]
            .get(0),
    );
    let user_count: i64 = tx
        .query("SELECT count(*) FROM res_users WHERE active", &[])
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    let mut tx = world.h.begin_as(world.admin).await.unwrap();
    let issued = issue(
        &mut tx,
        &hasher,
        &IssueRequest::new(
            UserId::new(uid).unwrap(),
            "HTTP integration",
            KeyKind::User,
            KeyScope::Rpc,
            Some(expiry),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let key_id = issued.id().get();
    tx.commit().await.unwrap();
    let key = issued.into_secret();

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
    let support::World { h, auth, .. } = world;
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
    Fixture {
        router: app.router("127.0.0.1".parse().unwrap()),
        auth,
        key,
        key_id,
        owner_uid: uid,
        session,
        other_session,
        user_count,
        h,
        _dir: dir,
    }
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
    fn bearer(&self, address: u8) -> axum::http::request::Builder {
        self.post(PATH, address)
            .header("authorization", format!("Bearer {}", self.key.expose()))
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
    async fn sessions(&self) -> String {
        let mut tx = self.h.begin().await.unwrap();
        let rows = tx.query("SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY id),'[]'::jsonb)::text FROM erp_session s", &[]).await.unwrap();
        let result = rows[0].get(0);
        tx.rollback().await.unwrap();
        result
    }
}

fn error(response: &Response<Body>, body: &[u8], status: StatusCode, name: &str, message: &str) {
    assert_eq!(
        response.status(),
        status,
        "{}",
        String::from_utf8_lossy(body)
    );
    let json: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(json["name"], name);
    assert_eq!(json["message"], message);
    if status == StatusCode::UNAUTHORIZED {
        assert_eq!(response.headers()["www-authenticate"], "bearer");
    }
}

#[tokio::test]
async fn native_bearer_is_stateless_and_revocation_applies_to_the_next_http_request() {
    let f = fixture().await;
    let before = f.sessions().await;
    for scheme in ["Bearer", "bEaReR"] {
        let (response, body) = f
            .send(
                f.post(PATH, 1)
                    .header("authorization", format!("{scheme} {}", f.key.expose()))
                    .body(Body::from(BODY))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!(f.user_count)
        );
        assert!(!response.headers().contains_key("set-cookie"));
    }
    assert_eq!(f.sessions().await, before);
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        tx.execute("DELETE FROM res_users_apikeys WHERE id=$1", &[&f.key_id])
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
    let (response, body) = f.send(f.bearer(1).body(Body::from(BODY)).unwrap()).await;
    error(
        &response,
        &body,
        StatusCode::UNAUTHORIZED,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
    assert!(!String::from_utf8_lossy(&body).contains(f.key.expose()));
    assert_eq!(f.sessions().await, before);
}

#[tokio::test]
async fn missing_invalid_and_duplicate_bearer_headers_have_a_redacted_challenge() {
    let f = fixture().await;
    let (response, body) = f
        .send(f.post(PATH, 1).body(Body::from(BODY)).unwrap())
        .await;
    error(
        &response,
        &body,
        StatusCode::UNAUTHORIZED,
        "werkzeug.exceptions.Unauthorized",
        "User not authenticated, use an API Key with a Bearer Authorization header.",
    );
    let unknown = erp_security::auth::apikey::GeneratedApiKey::generate().unwrap();
    for (i, value) in [
        "Bearer".to_owned(),
        "Basic abc".to_owned(),
        "Bearer unknown-native-key".to_owned(),
        format!("Bearer {}", unknown.expose()),
        "Bearer erpk_é".to_owned(),
        format!("Bearer {}", "x".repeat(2048)),
    ]
    .into_iter()
    .enumerate()
    {
        let (response, body) = f
            .send(
                f.post(PATH, u8::try_from(i + 2).unwrap())
                    .header("authorization", &value)
                    .body(Body::from(BODY))
                    .unwrap(),
            )
            .await;
        error(
            &response,
            &body,
            StatusCode::UNAUTHORIZED,
            "werkzeug.exceptions.Unauthorized",
            "Invalid apikey",
        );
        assert!(!String::from_utf8_lossy(&body).contains(&value));
    }
    let legacy = "0123456789abcdef0123456789abcdef01234567";
    let (response, body) = f
        .send(
            f.post(PATH, 31)
                .header("authorization", format!("Bearer {legacy}"))
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    error(
        &response,
        &body,
        StatusCode::UNAUTHORIZED,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
    assert!(!String::from_utf8_lossy(&body).contains(legacy));
    let (response, body) = f
        .send(
            f.bearer(20)
                .header("authorization", format!("Bearer {}", f.key.expose()))
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    error(
        &response,
        &body,
        StatusCode::UNAUTHORIZED,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
    assert!(!String::from_utf8_lossy(&body).contains(f.key.expose()));
}

#[tokio::test]
async fn cookie_precedence_never_replaces_key_authority_or_touches_sessions() {
    let f = fixture().await;
    let before = f.sessions().await;
    let (response, body) = f
        .send(
            f.bearer(1)
                .header("cookie", &f.session)
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(!response.headers().contains_key("set-cookie"));
    assert_eq!(f.sessions().await, before);
    let (response, body) = f
        .send(
            f.bearer(2)
                .header("cookie", &f.other_session)
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    error(
        &response,
        &body,
        StatusCode::FORBIDDEN,
        "erp.exceptions.AccessError",
        "Session user does not match the used apikey.",
    );
    let (response, body) = f
        .send(
            f.post(PATH, 3)
                .header("authorization", "Bearer invalid-key")
                .header("cookie", &f.session)
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    error(
        &response,
        &body,
        StatusCode::UNAUTHORIZED,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
    assert_eq!(f.sessions().await, before);
    let login = support::login(&f.auth, "other-user", "other-horse")
        .await
        .unwrap();
    let stale_cookie = format!(
        "session_id=test.{}",
        login.session().secret().to_cookie().as_str()
    );
    assert!(f.auth.logout(login.session().id()).await.unwrap());
    let after_revoke = f.sessions().await;
    let (response, body) = f
        .send(
            f.bearer(4)
                .header("cookie", stale_cookie)
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(f.sessions().await, after_revoke);
}

#[tokio::test]
async fn bearer_origin_and_install_boundaries_remain_explicit() {
    let f = fixture().await;
    let (response, body) = f
        .send(
            f.bearer(1)
                .header("origin", "https://erp.example.test")
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (response, body) = f
        .send(
            f.bearer(2)
                .header("origin", "https://other.invalid")
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body.as_slice(),
        b"Missing \"Authorization\" or Sec-headers for interactive usage."
    );
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
    let (response, body) = f
        .send(
            f.post("/erp/module/install", 3)
                .header("authorization", format!("Bearer {}", f.key.expose()))
                .header("sec-fetch-site", "same-origin")
                .body(Body::from("{\"modules\":[]}"))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body.as_slice(), b"user is not connected");
    let (response, body) = f
        .send(
            f.post("/erp/module/install", 4)
                .header("authorization", format!("Bearer {}", f.key.expose()))
                .header("cookie", &f.session)
                .body(Body::from("{\"modules\":[]}"))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body.as_slice(),
        b"Missing \"Authorization\" or Sec-headers for interactive usage."
    );
}

#[tokio::test]
async fn native_bearer_cooldown_is_shared_and_does_not_create_login_or_trusted_rows() {
    use erp_security::auth::{
        HashPoolConfig,
        throttle::{ThrottleKey, address_key},
    };
    use std::time::Duration;

    let f = fixture().await;
    support::set_param(&f.h, "base.login_cooldown_after", "2").await;
    support::set_param(&f.h, "base.login_cooldown_duration", "600").await;
    let settings = f.auth.settings().await.unwrap();
    let cooldown = settings.settings().address_cooldown();
    assert_eq!(cooldown.after(), 2);
    assert_eq!(cooldown.duration(), Duration::from_secs(600));
    let mut tx = f.h.begin().await.unwrap();
    let other_before: String = tx.query("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY kind, key_hash),'[]'::jsonb)::text FROM erp_login_throttle t WHERE kind IN ('login','trusted')", &[]).await.unwrap()[0].get(0);
    tx.rollback().await.unwrap();
    for _ in 0..cooldown.after() {
        let (response, body) = f
            .send(
                f.post(PATH, 80)
                    .header("authorization", "Bearer malformed-native-token")
                    .body(Body::from(BODY))
                    .unwrap(),
            )
            .await;
        error(
            &response,
            &body,
            StatusCode::UNAUTHORIZED,
            "werkzeug.exceptions.Unauthorized",
            "Invalid apikey",
        );
    }
    // A new authentication service has no process-local failures from the two requests above.
    let node_auth =
        Arc::new(support::node_with(&f.h, 4, support::own_pool(HashPoolConfig::DEFAULT)).await);
    let rpc = Arc::new(erp_rpc::RpcService::new(
        node_auth.orm().clone(),
        Host,
        erp_rpc::RpcConfig::default(),
    ));
    let node_app = Arc::new(
        WebApp::new(
            Arc::new(ServerConfig::load(&f._dir.path().join("server.toml")).unwrap()),
            node_auth,
        )
        .unwrap()
        .with_rpc(rpc),
    );
    let response = node_app
        .router("127.0.0.1".parse().unwrap())
        .oneshot(f.bearer(80).body(Body::from(BODY)).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let response = Response::from_parts(parts, Body::empty());
    let body = to_bytes(body, 1 << 20).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let retry_after = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!((1..=600).contains(&retry_after));
    error(
        &response,
        &body,
        StatusCode::TOO_MANY_REQUESTS,
        "erp.rate_limited",
        &format!("Rate limit exceeded; retry after {retry_after} seconds."),
    );
    let address = address_key("192.0.2.80".parse().unwrap());
    let key = f
        .auth
        .throttle_key(ThrottleKey::Address(&address))
        .await
        .unwrap();
    let mut tx = f.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT failures FROM erp_login_throttle WHERE kind='address' AND key_hash=$1",
            &[&key.as_slice()],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>(0), 2);
    let other_after: String = tx.query("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY kind, key_hash),'[]'::jsonb)::text FROM erp_login_throttle t WHERE kind IN ('login','trusted')", &[]).await.unwrap()[0].get(0);
    tx.rollback().await.unwrap();
    assert_eq!(other_after, other_before);
}

#[tokio::test]
async fn same_origin_browser_cookie_without_authorization_still_dispatches() {
    let f = fixture().await;
    let (response, body) = f
        .send(
            f.post(PATH, 1)
                .header("sec-fetch-site", "same-origin")
                .header("origin", "https://erp.example.test")
                .header("cookie", &f.session)
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!(f.user_count)
    );
}

/// A rolled-back preflight cannot retain the only pool slot needed by RPC execution.
#[tokio::test]
async fn preflight_releases_its_connection_before_bearer_or_cookie_dispatch() {
    use erp_security::auth::{AuthConfig, HashPoolConfig};
    let f = fixture().await;
    let pool = support::pool_of(&f.h, 2);
    let auth = Arc::new(
        Auth::new(
            support::orm_on(&f.h, pool.clone()).await,
            AuthConfig::DEFAULT,
            support::own_pool(HashPoolConfig::DEFAULT),
        )
        .await
        .unwrap(),
    );
    let rpc = Arc::new(erp_rpc::RpcService::new(
        auth.orm().clone(),
        Host,
        erp_rpc::RpcConfig::builder()
            .deadline_secs(10)
            .build()
            .unwrap(),
    ));
    let app = Arc::new(
        WebApp::new(
            Arc::new(ServerConfig::load(&f._dir.path().join("server.toml")).unwrap()),
            auth,
        )
        .unwrap()
        .with_rpc(rpc),
    );
    let router = app.router("127.0.0.1".parse().unwrap());
    let held = pool.get().await.unwrap();
    for bearer in [true, false] {
        let request = if bearer {
            f.bearer(90)
        } else {
            f.post(PATH, 90)
                .header("cookie", &f.session)
                .header("sec-fetch-site", "same-origin")
        }
        .body(Body::from(BODY))
        .unwrap();
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            router.clone().oneshot(request),
        )
        .await
        .expect("one free slot must suffice for preflight then dispatch")
        .unwrap();
        let (parts, body) = reply.into_parts();
        let body = to_bytes(body, 1 << 20).await.unwrap();
        assert_eq!(
            parts.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!(f.user_count)
        );
    }
    assert_eq!(
        held.query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
}

/// process flood admission runs once before external bearer database work.
#[tokio::test]
async fn bearer_prefilter_charges_once_and_refuses_floods_without_database_work() {
    use erp_security::auth::{AuthConfig, HashPoolConfig, throttle::address_key};
    let f = fixture().await;
    let config = AuthConfig::DEFAULT.with_login_prefilter(5, 0.1).unwrap();
    let auth = Arc::new(
        Auth::new(
            support::orm_on(&f.h, support::pool_of(&f.h, 4)).await,
            config,
            support::own_pool(HashPoolConfig::DEFAULT),
        )
        .await
        .unwrap(),
    );
    erp_security::auth::test_support::freeze_prefilter(&auth, std::time::Instant::now()).unwrap();
    let rpc = Arc::new(erp_rpc::RpcService::new(
        auth.orm().clone(),
        Host,
        erp_rpc::RpcConfig::default(),
    ));
    let app = Arc::new(
        WebApp::new(
            Arc::new(ServerConfig::load(&f._dir.path().join("server.toml")).unwrap()),
            Arc::clone(&auth),
        )
        .unwrap()
        .with_rpc(rpc),
    );
    let router = app.router("127.0.0.1".parse().unwrap());
    // One actual request performs preflight and execution credential checks, yet takes one token.
    let reply = router
        .clone()
        .oneshot(f.bearer(91).body(Body::from(BODY)).unwrap())
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    let address = address_key("192.0.2.91".parse().unwrap());
    for _ in 1..config.login_burst() {
        assert!(
            auth.prefilter().admit(&address),
            "RPC rechecks must not charge extra flood tokens"
        );
    }
    assert!(
        !auth.prefilter().admit(&address),
        "the external bearer request must charge its flood token"
    );
    let mut tx = f.h.begin().await.unwrap();
    let before: i64 = tx
        .query("SELECT count(*) FROM erp_login_throttle", &[])
        .await
        .unwrap()[0]
        .get(0);
    // Any credential opening now fails: a prefilter refusal must not read the missing key table.
    tx.batch("ALTER TABLE res_users_apikeys RENAME TO prefilter_hidden_keys")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for duplicate in [false, true] {
        let request = f
            .post(PATH, 91)
            .header("authorization", "Basic rejected-by-prefilter");
        let request = if duplicate {
            request.header("authorization", "Bearer duplicate")
        } else {
            request
        };
        let reply = router
            .clone()
            .oneshot(request.body(Body::from(BODY)).unwrap())
            .await
            .unwrap();
        let (parts, body) = reply.into_parts();
        let body = to_bytes(body, 1 << 20).await.unwrap();
        assert_eq!(parts.headers["retry-after"], "1");
        error(
            &Response::from_parts(parts, Body::empty()),
            &body,
            StatusCode::TOO_MANY_REQUESTS,
            "erp.rate_limited",
            "Rate limit exceeded; retry after 1 seconds.",
        );
    }
    let mut tx = f.h.begin().await.unwrap();
    let after: i64 = tx
        .query("SELECT count(*) FROM erp_login_throttle", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(
        after, before,
        "flood refusals never enter the shared failure store"
    );
    tx.batch("ALTER TABLE prefilter_hidden_keys RENAME TO res_users_apikeys")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let reply = router
        .oneshot(f.bearer(92).body(Body::from(BODY)).unwrap())
        .await
        .unwrap();
    assert_eq!(
        reply.status(),
        StatusCode::OK,
        "another address retains its own bucket"
    );
}

impl Fixture {
    async fn key_count(&self) -> i64 {
        let mut tx = self.h.begin().await.unwrap();
        let count = tx
            .query("SELECT count(*) FROM res_users_apikeys", &[])
            .await
            .unwrap()[0]
            .get(0);
        tx.rollback().await.unwrap();
        count
    }
    async fn management(&self, method: &str, body: serde_json::Value) -> (Response<Body>, Vec<u8>) {
        self.send(
            self.post(&format!("/erp/json/2/res.users.apikeys/{method}"), 101)
                .header("authorization", format!("Bearer {}", self.key.expose()))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }
    async fn expiry(&self) -> String {
        let mut tx = self.h.begin().await.unwrap();
        let expiry = tx.query("SELECT to_char(now() AT TIME ZONE 'utc' + interval '1 hour', 'YYYY-MM-DD HH24:MI:SS')", &[]).await.unwrap()[0].get(0);
        tx.rollback().await.unwrap();
        expiry
    }
}

#[tokio::test]
async fn programmatic_renewal_and_revoke_are_transactional() {
    let f = fixture().await;
    let count = f.key_count().await;
    let mut payload = serde_json::json!({"key":f.key.expose(), "scope":"rpc", "name":"Renewed", "expiration_date":"2099-01-01 00:00:00"});
    let (response, body) = f.management("generate", payload.clone()).await;
    error(
        &response,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "erp.exceptions.UserError",
        "Programmatic API keys are not enabled",
    );
    let (response, body) = f
        .management("revoke", serde_json::json!({"key":f.key.expose()}))
        .await;
    error(
        &response,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "erp.exceptions.UserError",
        "Programmatic API keys are not enabled",
    );
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let (response, body) = f.management("generate", payload.clone()).await;
    error(
        &response,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "erp.exceptions.ValidationError",
        "You cannot exceed 1.0 days.",
    );
    payload["expiration_date"] = "bad-date".into();
    let (response, body) = f.management("generate", payload.clone()).await;
    error(
        &response,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "erp.exceptions.ValidationError",
        "expiration_date must be a UTC date or datetime",
    );
    payload["expiration_date"] = f.expiry().await.into();
    payload["scope"] = "mcp".into();
    let (response, body) = f.management("generate", payload.clone()).await;
    error(
        &response,
        &body,
        StatusCode::FORBIDDEN,
        "erp.exceptions.AccessDenied",
        "The provided API key is invalid or does not belong to the current user.",
    );
    payload["scope"] = "rpc".into();
    let (response, body) = f
        .send(
            f.post("/erp/json/2/res.users.apikeys/generate", 102)
                .header("cookie", &f.other_session)
                .header("sec-fetch-site", "same-origin")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await;
    error(
        &response,
        &body,
        StatusCode::FORBIDDEN,
        "erp.exceptions.AccessDenied",
        "The provided API key is invalid or does not belong to the current user.",
    );
    for (method, message) in [
        (
            "generate",
            "The provided API key is invalid or does not belong to the current user.",
        ),
        ("revoke", "The provided API key is invalid."),
    ] {
        let mut invalid = if method == "generate" {
            payload.clone()
        } else {
            serde_json::json!({"key":f.key.expose()})
        };
        invalid["key"] = "malformed-secret-never-echo".into();
        let (response, body) = f.management(method, invalid).await;
        error(
            &response,
            &body,
            StatusCode::FORBIDDEN,
            "erp.exceptions.AccessDenied",
            message,
        );
        assert!(!String::from_utf8_lossy(&body).contains("malformed-secret-never-echo"));
    }
    support::set_param(&f.h, "base.programmatic_api_keys_limit", "1").await;
    let (response, body) = f.management("generate", payload.clone()).await;
    error(
        &response,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "erp.exceptions.UserError",
        "Limit of 1 API keys is reached for programmatic creation",
    );
    support::set_param(&f.h, "base.programmatic_api_keys_limit", "2").await;
    assert_eq!(f.key_count().await, count);
    let (response, body) = f.management("generate", payload.clone()).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let renewed =
        erp_security::auth::Zeroizing::new(serde_json::from_slice::<String>(&body).unwrap());
    assert_ne!(renewed.as_str(), f.key.expose());
    assert_eq!(f.key_count().await, count + 1);
    let mut metadata = f.h.begin().await.unwrap();
    let rows = metadata
        .query(
            "SELECT name,expiration_date,scope FROM res_users_apikeys WHERE id<>$1",
            &[&f.key_id],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "Renewed");
    assert_eq!(
        Some(rows[0].get::<_, DateTime>(1)),
        DateTime::parse_odoo(payload["expiration_date"].as_str().unwrap())
    );
    assert_eq!(rows[0].get::<_, String>(2), "rpc");
    metadata.rollback().await.unwrap();
    drop(metadata);
    let (response, body) = f
        .send(
            f.post(PATH, 103)
                .header("authorization", format!("Bearer {}", renewed.as_str()))
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    support::set_param(&f.h, "base.programmatic_api_keys_limit", "3").await;
    let mut chained = payload.clone();
    chained["key"] = renewed.as_str().into();
    chained["name"] = "Third key".into();
    let (response, body) = f
        .send(
            f.post("/erp/json/2/res.users.apikeys/generate", 107)
                .header("authorization", format!("Bearer {}", renewed.as_str()))
                .body(Body::from(chained.to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let third =
        erp_security::auth::Zeroizing::new(serde_json::from_slice::<String>(&body).unwrap());
    assert_ne!(third.as_str(), renewed.as_str());
    assert_ne!(third.as_str(), f.key.expose());
    assert_eq!(f.key_count().await, count + 2);
    let (response, body) = f
        .send(
            f.post("/erp/json/2/res.users.apikeys/revoke", 108)
                .header("authorization", format!("Bearer {}", renewed.as_str()))
                .body(Body::from(
                    serde_json::json!({"key":renewed.as_str()}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        true
    );
    assert_eq!(f.key_count().await, count + 1);
    // Only the third key remains next to the fixture's, with its own name, expiry and the `rpc`
    // scope.
    let mut metadata = f.h.begin().await.unwrap();
    let rows = metadata
        .query(
            "SELECT name,scope,expiration_date FROM res_users_apikeys WHERE id<>$1",
            &[&f.key_id],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "Third key");
    assert_eq!(rows[0].get::<_, String>(1), "rpc");
    assert_eq!(
        Some(rows[0].get::<_, DateTime>(2)),
        DateTime::parse_odoo(chained["expiration_date"].as_str().unwrap())
    );
    metadata.rollback().await.unwrap();
    drop(metadata);
    let (response, body) = f
        .send(
            f.post(PATH, 104)
                .header("authorization", format!("Bearer {}", renewed.as_str()))
                .body(Body::from(BODY))
                .unwrap(),
        )
        .await;
    error(
        &response,
        &body,
        StatusCode::UNAUTHORIZED,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
    assert!(!String::from_utf8_lossy(&body).contains(renewed.as_str()));
    // Current-key self revocation commits successfully; only subsequent requests fail.
    let (response, body) = f
        .management("revoke", serde_json::json!({"key":f.key.expose()}))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (response, body) = f.send(f.bearer(105).body(Body::from(BODY)).unwrap()).await;
    error(
        &response,
        &body,
        StatusCode::UNAUTHORIZED,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
}

/// event each, with the right cause: programmatic revocation by another key, self-revocation by
/// the presented key, owner revocation through a cookie session)
#[tokio::test]
async fn managed_generate_and_revoke_are_audited_with_their_cause() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    support::set_param(&f.h, "base.programmatic_api_keys_limit", "4").await;
    let generate = async |bearer: &str, address: u8| {
        let payload = serde_json::json!({"key":bearer, "scope":"rpc", "name":"Audited renewal name", "expiration_date": f.expiry().await});
        let (response, body) = f
            .send(
                f.post("/erp/json/2/res.users.apikeys/generate", address)
                    .header("authorization", format!("Bearer {bearer}"))
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&body)
        );
        erp_security::auth::Zeroizing::new(serde_json::from_slice::<String>(&body).unwrap())
    };
    let revoke = async |bearer: &str, key: &str, address: u8| {
        let (response, body) = f
            .send(
                f.post("/erp/json/2/res.users.apikeys/revoke", address)
                    .header("authorization", format!("Bearer {bearer}"))
                    .body(Body::from(serde_json::json!({"key":key}).to_string()))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&body)
        );
    };
    let first = generate(f.key.expose(), 111).await;
    revoke(f.key.expose(), first.as_str(), 112).await; // another key: programmatic
    let second = generate(f.key.expose(), 113).await;
    revoke(second.as_str(), second.as_str(), 114).await; // the presented key: self
    // the owner's cookie session revokes the old key: owner
    let (response, body) = f
        .send(
            f.post("/erp/json/2/res.users.apikeys/revoke", 115)
                .header("cookie", &f.session)
                .header("sec-fetch-site", "same-origin")
                .body(Body::from(
                    serde_json::json!({"key":f.key.expose()}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (client, connection) =
        f.h.db()
            .config()
            .connect(erp_testkit::tokio_postgres::NoTls)
            .await
            .unwrap();
    tokio::spawn(connection);
    let rows = client
        .query(
            "SELECT e->>'event', (e->>'key_id')::int8, e->'cause'->>'kind', \
             (e->'cause'->>'by_key')::int8, e->>'owner' \
             FROM erp_event.ir_audit_entry a, jsonb_array_elements(a.events) WITH ORDINALITY t(e, n) \
             WHERE e->>'type' = 'auth' ORDER BY a.id, t.n",
            &[],
        )
        .await
        .unwrap();
    let events: Vec<(String, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    // ids of the two replacements come from their own issuance events
    let first_id = events[1].1;
    let second_id = events[3].1;
    assert!(first_id != f.key_id && second_id != f.key_id && first_id != second_id);
    assert_eq!(
        events,
        vec![
            ("api_key.issued".to_owned(), f.key_id), // the fixture's direct `issue`, exactly one
            ("api_key.issued".to_owned(), first_id), // generate: exactly one event
            ("api_key.revoked".to_owned(), first_id),
            ("api_key.issued".to_owned(), second_id),
            ("api_key.revoked".to_owned(), second_id),
            ("api_key.revoked".to_owned(), f.key_id),
        ]
    );
    let cause = |i: usize| rows[i].get::<_, Option<String>>(2);
    assert_eq!(cause(2).as_deref(), Some("programmatic"));
    assert_eq!(rows[2].get::<_, Option<i64>>(3), Some(f.key_id));
    assert_eq!(cause(4).as_deref(), Some("self"));
    assert_eq!(rows[4].get::<_, Option<i64>>(3), None);
    assert_eq!(cause(5).as_deref(), Some("owner"));
    assert_eq!(rows[5].get::<_, Option<i64>>(3), None);
    let owner = rows[1].get::<_, String>(4);
    for row in &rows {
        assert_eq!(row.get::<_, String>(4), owner);
    }
    // row-level attribution of the key-authenticated RPC rows (1..=4) and the cookie row (5)
    let attribution = client
        .query(
            "SELECT a.origin, a.actor_kind, a.actor_id, a.subject_id FROM erp_event.ir_audit_entry a \
             WHERE EXISTS (SELECT 1 FROM jsonb_array_elements(a.events) e WHERE e->>'type'='auth') \
             ORDER BY a.id",
            &[],
        )
        .await
        .unwrap();
    let rpc_rows: Vec<_> = attribution
        .iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                r.get::<_, Option<String>>(1),
                r.get::<_, Option<i64>>(2),
                r.get::<_, Option<i64>>(3),
            )
        })
        .collect();
    assert_eq!(rpc_rows.len(), 6, "one audit row per audited operation");
    for (i, (origin, kind, id, _subject)) in rpc_rows.iter().enumerate().take(5).skip(1) {
        assert_eq!(origin, "rpc", "row {i}");
        assert_eq!(kind.as_deref(), Some("api_key_user"), "row {i}");
        // generate and revoke by the old key carry the old key; the self-revoke carries the
        // second key that made the call
        let expected = if i == 4 { second_id } else { f.key_id };
        assert_eq!(*id, Some(expected), "row {i}");
    }
    assert_eq!(rpc_rows[5].0, "rpc");
    // a cookie session acts as the user itself: kind "user", actor id and subject are the owner
    assert_eq!(rpc_rows[5].1.as_deref(), Some("user"));
    assert_eq!(
        rpc_rows[5].2,
        Some(f.owner_uid),
        "the cookie row's actor id is the owner's user id"
    );
    assert_eq!(
        rpc_rows[5].3,
        Some(f.owner_uid),
        "the cookie row's subject is the owner's user id"
    );
    let all: String = client
        .query_one(
            "SELECT coalesce(string_agg(a::text, ' '), '') FROM erp_event.ir_audit_entry a",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    for secret in [f.key.expose(), first.as_str(), second.as_str()] {
        assert!(!all.contains(secret));
    }
    assert!(!all.contains("Audited renewal name"));
}

#[tokio::test]
async fn key_generation_refuses_idempotency_before_creating_a_key() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let before = f.key_count().await;
    let payload = serde_json::json!({"key":f.key.expose(), "scope":"rpc", "name":"Never persisted", "expiration_date": f.expiry().await});
    let (response, body) = f
        .send(
            f.post("/erp/json/2/res.users.apikeys/generate", 106)
                .header("authorization", format!("Bearer {}", f.key.expose()))
                .header("idempotency-key", "550e8400-e29b-41d4-a716-446655440000")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await;
    error(
        &response,
        &body,
        StatusCode::UNPROCESSABLE_ENTITY,
        "werkzeug.exceptions.UnprocessableEntity",
        "Idempotency-Key is not accepted for API key generation: credentials are returned only once.",
    );
    assert_eq!(f.key_count().await, before);
    let mut tx = f.h.begin().await.unwrap();
    let count: i64 = tx
        .query("SELECT count(*) FROM ir_idempotency_key", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn auth_collection_includes_expired_keys() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    tx.execute("UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' - interval '1 second' WHERE id=$1", &[&f.key_id]).await.unwrap();
    tx.commit().await.unwrap();
    let report = f.auth.gc().await.unwrap();
    assert_eq!(report.api_keys, 1);
    assert_eq!(f.key_count().await, 0);
}

/// own transaction with its own audit event)
#[tokio::test]
async fn auth_collection_loops_bounded_batches_with_one_event_each() {
    let f = fixture().await;
    let (client, connection) =
        f.h.db()
            .config()
            .connect(erp_testkit::tokio_postgres::NoTls)
            .await
            .unwrap();
    tokio::spawn(connection);
    client
        .execute(
            "INSERT INTO res_users_apikeys (name,user_id,created_by,kind,scope,digest,digest_kid,prefix,\
             models,methods,read_only,capabilities,expiration_date,create_date) \
             SELECT name,user_id,created_by,kind,scope,sha256(('bulk'||g)::text::bytea),digest_kid,prefix,\
             models,methods,read_only,capabilities,now() AT TIME ZONE 'utc' - interval '1 hour',create_date \
             FROM res_users_apikeys, generate_series(1,1100) g WHERE id=$1",
            &[&f.key_id],
        )
        .await
        .unwrap();
    let report = f.auth.gc().await.unwrap();
    assert_eq!(report.api_keys, 1100);
    let sizes: Vec<i32> = client
        .query(
            "SELECT jsonb_array_length(e->'key_ids')::int4 FROM erp_event.ir_audit_entry a, \
             jsonb_array_elements(a.events) e WHERE e->>'event'='api_keys.collected' ORDER BY a.id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(sizes, [1000, 100], "one bounded event per committed batch");
    assert_eq!(f.key_count().await, 1, "the live fixture key stays");
}

#[tokio::test]
async fn managed_revocation_is_visible_on_another_node() {
    use erp_security::auth::{ApiKeyRequest, CredentialOptions, HashPoolConfig};
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let a = support::node_with(&f.h, 4, support::own_pool(HashPoolConfig::DEFAULT)).await;
    let b = support::node_with(&f.h, 4, support::own_pool(HashPoolConfig::DEFAULT)).await;
    let request = || {
        ApiKeyRequest::new(
            f.key.expose(),
            KeyScope::Rpc,
            "192.0.2.120".parse().unwrap(),
        )
    };
    let mut before = b
        .begin_api_key(request(), CredentialOptions::new())
        .await
        .unwrap()
        .tx;
    before.rollback().await.unwrap();
    drop(before);
    // The never-used key is due: dropping the ticket launches its last-use write on another
    // connection. Let it end, or node a's revocation of the same row can fail to serialize
    // (the RPC layer retries that; this direct caller does not).
    let started = std::time::Instant::now();
    while b.stats().last_use_writes < 1 {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the last-use write never ended"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Revocation is audited: a direct (non-HTTP) caller supplies the operation identity the
    // RPC layer would, and node a checks its audit store once, as a server does at startup.
    a.orm().check_audit_store().await.unwrap();
    let mut tx = a
        .begin_api_key(
            request(),
            CredentialOptions::new()
                .operation_identity(erp_orm::OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
        .tx;
    let generation: i64 = tx.query("SELECT coalesce(sum(weight),0)::int8 FROM erp_signal_generation WHERE key='cache:apikeys'",&[]).await.unwrap()[0].get(0);
    assert!(
        erp_security::auth::apikey_manage::revoke(&mut tx, f.key.expose())
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    drop(tx);
    let e = b
        .begin_api_key(request(), CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind(), erp_fields::ErrorKind::AccessDenied);
    assert_eq!(e.message(), "Invalid apikey");
    let mut tx = f.h.begin().await.unwrap();
    let after: i64 = tx.query("SELECT coalesce(sum(weight),0)::int8 FROM erp_signal_generation WHERE key='cache:apikeys'",&[]).await.unwrap()[0].get(0);
    assert_eq!(after, generation + 1);
    assert_eq!(
        tx.query(
            "SELECT count(*)::int8 FROM res_users_apikeys WHERE id=$1",
            &[&f.key_id]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn generic_key_deletion_is_refused() {
    let f = fixture().await;
    let count = f.key_count().await;
    let (response, body) = f
        .management("unlink", serde_json::json!({"ids":[f.key_id]}))
        .await;
    error(
        &response,
        &body,
        StatusCode::FORBIDDEN,
        "erp.exceptions.AccessError",
        "API keys must be revoked through the dedicated revoke method",
    );
    assert_eq!(f.key_count().await, count);
}

impl Fixture {
    /// Poll the counters of this fixture's `Auth` until `ready` holds.
    async fn until_stats(
        &self,
        what: &str,
        ready: impl Fn(&erp_security::auth::AuthStats) -> bool,
    ) -> erp_security::auth::AuthStats {
        let started = std::time::Instant::now();
        loop {
            let stats = self.auth.stats();
            if ready(&stats) {
                return stats;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "never: {what}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

/// one JSON-2 request with a due key opens the key in the preflight
/// and again in the RPC attempt. The credential keeps the first ticket and discards the second, so
/// the write runs once: one row written, no no-op and no skip from a duplicate ticket. The key table
/// is locked in SHARE mode while the request runs (the route reads it, the `search_count` call does
/// not write it), so the first ticket's write is blocked and holds the one-permit gate: a duplicate
/// ticket dropped by the handler would be counted as skipped before the response returns.
#[tokio::test]
async fn one_bearer_request_with_a_due_key_writes_last_use_once() {
    let f = fixture().await;
    let mut lock = f.h.begin().await.unwrap();
    lock.batch("LOCK TABLE res_users_apikeys IN SHARE MODE")
        .await
        .unwrap();
    let (response, body) = f.send(f.bearer(1).body(Body::from(BODY)).unwrap()).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(f.auth.stats().last_use_skipped, 0);
    support::until_lock_wait(&f.h).await;
    lock.commit().await.unwrap();
    let stats = f
        .until_stats("the write is counted", |s| s.last_use_writes == 1)
        .await;
    assert_eq!(
        (
            stats.last_use_noops,
            stats.last_use_skipped,
            stats.last_use_errors
        ),
        (0, 0, 0)
    );
    let mut tx = f.h.begin().await.unwrap();
    let set: bool = tx
        .query(
            "SELECT last_used_at IS NOT NULL FROM res_users_apikeys WHERE id=$1",
            &[&f.key_id],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    assert!(set);
}

/// a JSON-2 request that revokes its own due key succeeds. The ticket
/// is held until the request has ended, so the write runs after the delete committed. Derived from
/// the code: it finds no row, so it is counted as a no-op, not as a write and not as an error.
#[tokio::test]
async fn bearer_self_revoke_with_a_due_key_succeeds_and_the_late_write_is_a_noop() {
    let f = fixture().await;
    support::set_param(&f.h, "base.enable_programmatic_api_keys", "True").await;
    let (response, body) = f
        .management("revoke", serde_json::json!({"key": f.key.expose()}))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(f.key_count().await, 0);
    let stats = f
        .until_stats("the late write found no row", |s| s.last_use_noops == 1)
        .await;
    assert_eq!(
        (
            stats.last_use_writes,
            stats.last_use_skipped,
            stats.last_use_errors
        ),
        (0, 0, 0)
    );
}
