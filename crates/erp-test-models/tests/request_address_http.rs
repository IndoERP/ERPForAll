// SPDX-License-Identifier: MIT OR Apache-2.0
//! JSON-2 over the real `WebApp`: a model method sees the client address the proxy gate
//! resolved (the peer, or the forwarded client of a trusted proxy), with an API key and with a
//! browser session alike; a forwarded address from a peer that is not a trusted proxy is refused
//! before any method runs.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "auth_support/mod.rs"]
mod support;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use erp_fields::DateTime;
use erp_orm::UserId;
use erp_security::auth::{
    AuthConfig, HashPoolConfig,
    apikey::ApiKeyHasher,
    apikey_store::{IssueRequest, IssuedSecret, KeyKind, KeyScope, issue},
    secret::DatabaseSecret,
};
use erp_server::{config::ServerConfig, web::WebApp};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;

const PROXY_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ORIGIN: &str = "https://erp.example.test";
const PROBE: &str = "/erp/json/2/test_rpc.effects/client_address";
/// The trusted proxy: the peer every request of the fixture's router comes from.
const PROXY: &str = "127.0.0.1";

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
    app: Arc<WebApp>,
    key: IssuedSecret,
    session: String,
    _h: erp_testkit::Harness,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let h = erp_testkit::Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_orm::ADDON,
        &erp_test_models::test_rpc::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .require_core_tables(erp_security::auth::core_tables())
    .identify_transactions()
    .build()
    .await
    .unwrap();
    let world = support::world_with(h, AuthConfig::DEFAULT, HashPoolConfig::DEFAULT).await;
    let uid = support::user_with_password(&world.h, "key-user", "correct-horse").await;
    let login = support::login(&world.auth, "key-user", "correct-horse")
        .await
        .unwrap();
    let session = format!(
        "session_id=test.{}",
        login.session().secret().to_cookie().as_str()
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
    tx.rollback().await.unwrap();
    let mut tx = world.h.begin_as(world.admin).await.unwrap();
    let issued = issue(
        &mut tx,
        &hasher,
        &IssueRequest::new(
            UserId::new(uid).unwrap(),
            "Address probe",
            KeyKind::User,
            KeyScope::Rpc,
            Some(expiry),
        )
        .unwrap(),
    )
    .await
    .unwrap();
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
    fs::write(dir.path().join("server.toml"), format!("database='test'\ndatabase_url_file='runtime'\nowner_database_url_file='owner'\nbind='127.0.0.1:0'\npublic_origin='{ORIGIN}'\nproxy_mode='authenticated'\ntrusted_proxies=['{PROXY}']\nproxy_token_file='proxy'\naddons_path=['addons']\nassets_manifest='manifest.json'\n")).unwrap();
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
        app,
        key,
        session,
        _h: h,
        _dir: dir,
    }
}

impl Fixture {
    fn router(&self, peer: &str) -> Router {
        self.app.router(peer.parse().unwrap())
    }
    /// A JSON-2 call of the probe through the trusted proxy, forwarding `client` when given.
    fn through_proxy(&self, client: Option<&str>) -> axum::http::request::Builder {
        let request = Request::builder()
            .method("POST")
            .uri(PROBE)
            .header("content-type", "application/json")
            .header("x-erp-proxy-token", PROXY_TOKEN);
        match client {
            Some(client) => request.header("x-forwarded-for", client),
            None => request,
        }
    }
    fn with_key(&self, request: axum::http::request::Builder) -> axum::http::request::Builder {
        request.header("authorization", format!("Bearer {}", self.key.expose()))
    }
    fn with_session(&self, request: axum::http::request::Builder) -> axum::http::request::Builder {
        request
            .header("cookie", &self.session)
            .header("origin", ORIGIN)
            .header("sec-fetch-site", "same-origin")
    }
    async fn send(
        &self,
        peer: &str,
        request: axum::http::request::Builder,
        body: &str,
    ) -> (StatusCode, String) {
        let response = self
            .router(peer)
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }
    /// What the probe saw: the address in this environment and its derivations, then the
    /// context's own `client_address` key.
    async fn seen(
        &self,
        peer: &str,
        request: axum::http::request::Builder,
        body: &str,
    ) -> serde_json::Value {
        let (status, text) = self.send(peer, request, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        serde_json::from_str(&text).unwrap()
    }
}

#[tokio::test]
async fn a_method_sees_the_forwarded_client_of_a_trusted_proxy() {
    let f = fixture().await;
    let all = |address: &str| serde_json::json!([address, address, address, address, false]);
    let seen = f
        .seen(PROXY, f.with_key(f.through_proxy(Some("192.0.2.10"))), "{}")
        .await;
    assert_eq!(seen, all("192.0.2.10"));
    let seen = f
        .seen(
            PROXY,
            f.with_session(f.through_proxy(Some("2001:db8::10"))),
            "{}",
        )
        .await;
    assert_eq!(seen, all("2001:db8::10"));
    // the trusted proxy probing without a forwarded client is itself the client
    let seen = f.seen(PROXY, f.with_key(f.through_proxy(None)), "{}").await;
    assert_eq!(seen, all(PROXY));
    // a context key of the same name is only a context value
    let seen = f
        .seen(
            PROXY,
            f.with_key(f.through_proxy(Some("192.0.2.11"))),
            r#"{"context": {"client_address": "10.0.0.1"}}"#,
        )
        .await;
    assert_eq!(seen[0], "192.0.2.11");
    assert_eq!(seen[4], "10.0.0.1");
}

#[tokio::test]
async fn a_forwarded_address_from_an_untrusted_peer_never_reaches_a_method() {
    let f = fixture().await;
    let (status, text) = f
        .send(
            "198.51.100.20",
            f.with_key(f.through_proxy(Some("10.0.0.1"))),
            "{}",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert_eq!(text, "untrusted proxy request");
    // nor from the trusted proxy without its token
    let request = Request::builder()
        .method("POST")
        .uri(PROBE)
        .header("content-type", "application/json")
        .header("x-forwarded-for", "10.0.0.1");
    let (status, text) = f.send(PROXY, f.with_key(request), "{}").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    // a malformed forwarded address is refused, never passed on as text
    let (status, text) = f
        .send(
            PROXY,
            f.with_key(f.through_proxy(Some("10.0.0.1, 10.0.0.2"))),
            "{}",
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
}
