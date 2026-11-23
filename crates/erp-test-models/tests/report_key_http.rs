// SPDX-License-Identifier: MIT OR Apache-2.0
//! `GET /erp/report/pdf/...` with an API key against a real tenant `Auth`, `WebApp` and
//! `RpcService`. The composition behind the route is the base addon's own lookup and access
//! check (`report_print::find`, `check_records`) with a stand-in layout, so these tests prove the
//! key's user is the principal the checks run as; the layout itself is covered elsewhere.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "auth_support/mod.rs"]
mod support;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, Response, StatusCode},
};
use erp_addon_base::report_print;
use erp_fields::DateTime;
use erp_orm::UserId;
use erp_sdk::{Operation, Operator, Value};
use erp_security::auth::{
    AuthConfig, HashPoolConfig,
    apikey::ApiKeyHasher,
    apikey_store::{IssueRequest, IssuedSecret, KeyKind, KeyScope, issue},
    secret::DatabaseSecret,
};
use erp_server::{
    config::ServerConfig,
    web::{PrintedReport, ReportFormat, WebApp, WebReport},
};
use erp_testkit::{DomainPolicy, Otherwise};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;

const PROXY_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ORIGIN: &str = "https://erp.example.test";
/// A partner with this name is hidden from every reader by the fixture's record rule.
const HIDDEN: &str = "hidden partner";

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

/// The base addon's report lookup and record check, then a stand-in document naming the user it
/// was printed for.
struct Printer;
impl WebReport for Printer {
    fn print<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        format: ReportFormat,
        report: &'a str,
        ids: &'a [i64],
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<PrintedReport>> {
        Box::pin(async move {
            let found = report_print::find(tx, report).await?;
            report_print::check_records(tx, &found, None, ids).await?;
            Ok(PrintedReport {
                file_name: format!("{}.pdf", found.name),
                bytes: format!("%PDF {format:?} uid={}", tx.env().user_id().get()).into_bytes(),
            })
        })
    }
}

struct Fixture {
    router: Router,
    key: IssuedSecret,
    key_id: i64,
    owner_uid: i64,
    session: String,
    other_session: String,
    visible: i64,
    hidden: i64,
    h: erp_testkit::Harness,
    _dir: tempfile::TempDir,
}

/// A record of `model` with these text values, created by the system.
async fn create(tx: &mut erp_orm::Transaction, model: &str, pairs: &[(&str, &str)]) -> i64 {
    let pairs: Vec<(&str, Value)> = pairs.iter().map(|(k, v)| (*k, support::s(v))).collect();
    support::create(tx, model, &pairs).await
}

async fn fixture() -> Fixture {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule(
            "res.partner",
            Operation::Read,
            erp_sdk::Domain::condition("name", Operator::Ne, HIDDEN).unwrap(),
        )
        .build()
        .unwrap();
    let world = support::world_with(
        support::builder().policy(policy).build().await.unwrap(),
        AuthConfig::DEFAULT,
        HashPoolConfig::DEFAULT,
    )
    .await;
    let uid = support::user_with_password(&world.h, "key-user", "correct-horse").await;
    support::user_with_password(&world.h, "other-user", "other-horse").await;
    let cookie = |login: &erp_security::auth::Login| {
        format!(
            "session_id=test.{}",
            login.session().secret().to_cookie().as_str()
        )
    };
    let session = cookie(
        &support::login(&world.auth, "key-user", "correct-horse")
            .await
            .unwrap(),
    );
    let other_session = cookie(
        &support::login(&world.auth, "other-user", "other-horse")
            .await
            .unwrap(),
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
    let visible = create(&mut tx, "res.partner", &[("name", "visible partner")]).await;
    let hidden = create(&mut tx, "res.partner", &[("name", HIDDEN)]).await;
    // the report actions name templates that must be active QWeb views of the database
    for key in ["test_report.partner_card"] {
        create(
            &mut tx,
            "ir.ui.view",
            &[
                ("name", key),
                ("type", "qweb"),
                ("key", key),
                ("arch_db", &format!("<t t-name=\"{key}\"><div/></t>")),
            ],
        )
        .await;
    }
    let open = create(
        &mut tx,
        "ir.actions.report",
        &[
            ("name", "Partner card"),
            ("model", "res.partner"),
            ("report_name", "test_report.partner_card"),
        ],
    )
    .await;
    assert!(open > 0);
    tx.commit().await.unwrap();

    let mut tx = world.h.begin_as(world.admin).await.unwrap();
    let issued = issue(
        &mut tx,
        &hasher,
        &IssueRequest::new(
            UserId::new(uid).unwrap(),
            "Phone",
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
    fs::write(dir.path().join("server.toml"), format!("database='test'\ndatabase_url_file='runtime'\nowner_database_url_file='owner'\nbind='127.0.0.1:0'\npublic_origin='{ORIGIN}'\nproxy_mode='authenticated'\ntrusted_proxies=['127.0.0.1']\nproxy_token_file='proxy'\naddons_path=['addons']\nassets_manifest='manifest.json'\n")).unwrap();
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
        .with_rpc(rpc)
        .with_report(Arc::new(Printer)),
    );
    Fixture {
        router: app.router("127.0.0.1".parse().unwrap()),
        key,
        key_id,
        owner_uid: uid,
        session,
        other_session,
        visible,
        hidden,
        h,
        _dir: dir,
    }
}

impl Fixture {
    fn get(&self, path: &str, address: u8) -> axum::http::request::Builder {
        Request::builder()
            .method("GET")
            .uri(path)
            .header("x-erp-proxy-token", PROXY_TOKEN)
            .header("x-forwarded-for", format!("192.0.2.{address}"))
    }
    fn with_key(&self, path: &str, address: u8) -> axum::http::request::Builder {
        self.get(path, address)
            .header("authorization", format!("Bearer {}", self.key.expose()))
    }
    fn card(&self, id: i64) -> String {
        format!("/erp/report/pdf/test_report.partner_card/{id}?download=1")
    }
    async fn send(&self, request: axum::http::request::Builder) -> (Response<Body>, String) {
        let (parts, body) = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .into_parts();
        let body = to_bytes(body, 1 << 20).await.unwrap().to_vec();
        (
            Response::from_parts(parts, Body::empty()),
            String::from_utf8(body).unwrap(),
        )
    }
    async fn sessions(&self) -> String {
        let mut tx = self.h.begin().await.unwrap();
        let rows = tx
            .query(
                "SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY id),'[]'::jsonb)::text \
                 FROM erp_session s",
                &[],
            )
            .await
            .unwrap();
        let result = rows[0].get(0);
        tx.rollback().await.unwrap();
        result
    }
    async fn last_used(&self) -> bool {
        let mut tx = self.h.begin().await.unwrap();
        let rows = tx
            .query(
                "SELECT last_used_at IS NOT NULL FROM res_users_apikeys WHERE id=$1",
                &[&self.key_id],
            )
            .await
            .unwrap();
        let used = rows[0].get(0);
        tx.rollback().await.unwrap();
        used
    }
}

/// The JSON error of a refused request: its status, exception name and message.
fn refused(response: &Response<Body>, body: &str, status: StatusCode, message: &str) {
    assert_eq!(response.status(), status, "{body}");
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(json["message"], message, "{body}");
    assert!(!response.headers().contains_key("set-cookie"));
}

#[tokio::test]
async fn a_key_prints_a_pdf_its_user_may_read_without_a_session_or_cookie() {
    let f = fixture().await;
    let before = f.sessions().await;
    let (response, body) = f.send(f.with_key(&f.card(f.visible), 1)).await;
    assert_eq!(response.status(), StatusCode::OK, "{body}");
    assert_eq!(body, format!("%PDF Pdf uid={}", f.owner_uid));
    let headers = response.headers();
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(headers["cache-control"], "private, no-store");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;"),
        "{:?}",
        headers["content-disposition"]
    );
    assert!(!headers.contains_key("set-cookie"));
    // no session was opened for the key, and its use is recorded as on JSON-2
    assert_eq!(f.sessions().await, before);
    let started = std::time::Instant::now();
    while !f.last_used().await {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the key's last use was never recorded"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // the same origin is accepted, as a browser page of this server would send it
    let (response, body) = f
        .send(f.with_key(&f.card(f.visible), 2).header("origin", ORIGIN))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_key_is_held_to_its_users_report_groups_and_record_access() {
    let f = fixture().await;
    let (response, body) = f.send(f.with_key(&f.card(f.hidden), 1)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("%PDF"), "{body}");
    let (response, body) = f
        .send(f.with_key(
            &format!(
                "/erp/report/pdf/test_report.partner_card/{},{}",
                f.visible, f.hidden
            ),
            2,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{body}");
    // (the report's groups are checked by the same `report_print::find` for both credentials;
    // the test access policies do not evaluate group membership, so that refusal is not
    // reproduced here)
    let (response, body) = f
        .send(f.with_key("/erp/report/pdf/test_report.unknown/1", 4))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "{body}");
    // the key's refusal is the JSON error a native client can read; the browser session of the
    // same user is refused too (as the error page of a navigation)
    let (response, key_body) = f.send(f.with_key(&f.card(f.hidden), 6)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{key_body}");
    let json: serde_json::Value = serde_json::from_str(&key_body).unwrap();
    assert_eq!(json["name"], "erp.exceptions.AccessError", "{key_body}");
    let (response, session_body) = f
        .send(f.get(&f.card(f.hidden), 5).header("cookie", &f.session))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{session_body}");
    assert!(!session_body.contains("%PDF"));
}

#[tokio::test]
async fn invalid_revoked_and_foreign_origin_keys_are_refused_alike() {
    let f = fixture().await;
    let unknown = erp_security::auth::apikey::GeneratedApiKey::generate().unwrap();
    for (i, value) in [
        "Bearer".to_owned(),
        "Basic abc".to_owned(),
        format!("Bearer {}", unknown.expose()),
    ]
    .into_iter()
    .enumerate()
    {
        let (response, body) = f
            .send(
                f.get(&f.card(f.visible), u8::try_from(i + 1).unwrap())
                    .header("authorization", &value)
                    .header("cookie", &f.session),
            )
            .await;
        refused(&response, &body, StatusCode::UNAUTHORIZED, "Invalid apikey");
        assert_eq!(response.headers()["www-authenticate"], "bearer");
        assert!(!body.contains(&value));
    }
    let (response, body) = f
        .send(
            f.with_key(&f.card(f.visible), 10)
                .header("origin", "https://other.invalid"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("%PDF"));
    // a cookie of another user next to the key is refused, never used
    let (response, body) = f
        .send(
            f.with_key(&f.card(f.visible), 11)
                .header("cookie", &f.other_session),
        )
        .await;
    refused(
        &response,
        &body,
        StatusCode::FORBIDDEN,
        "Session user does not match the used apikey.",
    );
    let mut tx = f.h.begin().await.unwrap();
    tx.execute("DELETE FROM res_users_apikeys WHERE id=$1", &[&f.key_id])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (response, body) = f.send(f.with_key(&f.card(f.visible), 12)).await;
    refused(&response, &body, StatusCode::UNAUTHORIZED, "Invalid apikey");
    assert!(!body.contains(f.key.expose()));
}

#[tokio::test]
async fn the_html_preview_stays_with_the_browser_session() {
    let f = fixture().await;
    let html = format!("/erp/report/html/test_report.partner_card/{}", f.visible);
    let (response, body) = f.send(f.with_key(&html, 1)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        body,
        "API keys print PDF only; the HTML preview needs a browser session."
    );
    let (response, body) = f.send(f.get(&html, 2).header("cookie", &f.session)).await;
    assert_eq!(response.status(), StatusCode::OK, "{body}");
    assert_eq!(body, format!("%PDF Html uid={}", f.owner_uid));
    // without either credential the route asks for a session, as before
    let (response, _) = f.send(f.get(&f.card(f.visible), 3)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
