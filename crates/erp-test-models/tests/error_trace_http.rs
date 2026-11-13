//! Request ids, refusal logs and browser error reports over the real HTTP stack
//! : a signed-in user, a real `Auth` and RPC
//! service on the test database, and the server's own log lines captured in process.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "auth_support/mod.rs"]
mod support;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, Response, StatusCode},
};
use erp_server::{
    config::ServerConfig,
    logging::{LineLogger, LogFilter},
    web::WebApp,
};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The log lines of this thread (a `#[tokio::test]` runs on one thread, its tasks included).
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Sink {
    fn lines(&self) -> Vec<String> {
        let text = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
        text.lines().map(str::to_owned).collect()
    }
    fn matching(&self, needle: &str) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| l.contains(needle))
            .collect()
    }
}

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
    session_cookie: String,
    uid: i64,
    log: Sink,
    _guard: tracing::subscriber::DefaultGuard,
    _dir: tempfile::TempDir,
    /// Dropping the harness drops the test database.
    _harness: erp_testkit::Harness,
}

async fn fixture(filter: &str) -> Fixture {
    let world = support::world().await;
    let uid = support::user_with_password(&world.h, "tracer", "correct-horse").await;
    let login = support::login(&world.auth, "tracer", "correct-horse")
        .await
        .unwrap();
    let session_cookie = format!(
        "session_id=test.{}",
        login.session().secret().to_cookie().as_str()
    );
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in [
        ("runtime", "postgres://runtime:dummy@127.0.0.1/test"),
        ("owner", "postgres://owner:dummy@127.0.0.1/test"),
        ("proxy", TOKEN),
    ] {
        fs::write(dir.path().join(file), text).unwrap();
        fs::set_permissions(dir.path().join(file), fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(dir.path().join("server.toml"), "database='test'\ndatabase_url_file='runtime'\nowner_database_url_file='owner'\nbind='127.0.0.1:0'\npublic_origin='https://erp.example.test'\nproxy_mode='authenticated'\ntrusted_proxies=['127.0.0.1']\nproxy_token_file='proxy'\naddons_path=['addons']\nassets_manifest='manifest.json'\n").unwrap();
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let entries: Vec<_> = [("js", "text/javascript; charset=utf-8"), ("css", "text/css; charset=utf-8")].into_iter().map(|(ext, mime)| {
        let path = format!("client.{digest}.{ext}");
        fs::write(dir.path().join(&path), b"").unwrap();
        serde_json::json!({"name":format!("client.{ext}"),"path":path,"sha256":digest,"content_type":mime})
    }).collect();
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema":1,"assets":entries})).unwrap(),
    )
    .unwrap();
    let support::World {
        h: harness, auth, ..
    } = world;
    let auth = Arc::new(auth);
    let rpc = Arc::new(erp_rpc::RpcService::new(
        auth.orm().clone(),
        Host,
        erp_rpc::RpcConfig::default(),
    ));
    let app = Arc::new(
        WebApp::new(
            Arc::new(ServerConfig::load(&dir.path().join("server.toml")).unwrap()),
            auth,
        )
        .unwrap()
        .with_rpc(rpc),
    );
    // This isolated transport fixture tests tracing; native GC supervision is covered separately.
    app.set_login_available(true);
    let log = Sink::default();
    let guard = tracing::subscriber::set_default(LineLogger::new(
        LogFilter::parse(filter).unwrap(),
        log.clone(),
    ));
    Fixture {
        router: app.router("127.0.0.1".parse().unwrap()),
        session_cookie,
        uid,
        log,
        _guard: guard,
        _dir: dir,
        _harness: harness,
    }
}

impl Fixture {
    /// A same-origin JSON POST as the signed-in browser sends it.
    fn post(&self, path: &str) -> axum::http::request::Builder {
        self.post_as(path, "same-origin", "application/json")
    }
    fn post_as(&self, path: &str, site: &str, content_type: &str) -> axum::http::request::Builder {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("x-erp-proxy-token", TOKEN)
            .header("sec-fetch-site", site)
            .header("content-type", content_type)
            .header("cookie", &self.session_cookie)
    }
    async fn send(&self, request: Request<Body>) -> Response<Body> {
        self.router.clone().oneshot(request).await.unwrap()
    }
    async fn json_post(
        &self,
        path: &str,
        id: Option<&str>,
        body: &str,
    ) -> (Response<Body>, Vec<u8>) {
        let mut builder = self.post(path);
        if let Some(id) = id {
            builder = builder.header("x-request-id", id);
        }
        let response = self
            .send(builder.body(Body::from(body.to_owned())).unwrap())
            .await;
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, 1 << 20).await.unwrap().to_vec();
        (Response::from_parts(parts, Body::empty()), bytes)
    }
}

fn header<'a>(response: &'a Response<Body>, name: &str) -> &'a str {
    response.headers()[name].to_str().unwrap()
}

fn is_uuid(text: &str) -> bool {
    erp_rpc::CorrelationId::parse(text).is_ok()
}

/// anything else is replaced by a fresh UUID.
#[tokio::test]
async fn every_response_carries_a_request_id_and_bad_incoming_ids_are_replaced() {
    let f = fixture("warn").await;
    // a refused request (no proxy token) still has one
    let denied = f
        .send(
            Request::builder()
                .uri("/erp/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(is_uuid(header(&denied, "x-request-id")));
    // two requests, two ids
    let again = f
        .send(
            Request::builder()
                .uri("/erp/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_ne!(
        header(&denied, "x-request-id"),
        header(&again, "x-request-id")
    );
    // a well-formed incoming id is kept (lower-cased, the canonical text)
    let mine = "0194F5A0-7B1C-7D2E-8F3A-123456789ABC";
    let kept = f
        .send(
            Request::builder()
                .uri("/erp/login")
                .header("x-request-id", mine)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(header(&kept, "x-request-id"), mine.to_ascii_lowercase());
    // malformed incoming ids are replaced, never echoed
    for bad in [
        "not-a-uuid",
        "../../etc/passwd",
        "0194f5a0-7b1c-7d2e-8f3a-123456789abc-extra",
        "0194f5a0-7b1c-7d2e-8f3a-12345678ZZZZ",
        "",
        " 0194f5a0-7b1c-7d2e-8f3a-123456789abc",
    ] {
        let replaced = f
            .send(
                Request::builder()
                    .uri("/erp/login")
                    .header("x-request-id", bad)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        let got = header(&replaced, "x-request-id");
        assert!(is_uuid(got), "{bad:?} -> {got:?}");
        assert_ne!(got, bad.trim());
    }
    // two ids in one request: ambiguous, replaced
    let two = f
        .send(
            Request::builder()
                .uri("/erp/login")
                .header("x-request-id", mine)
                .header("x-request-id", "0194f5a0-7b1c-7d2e-8f3a-000000000000")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let got = header(&two, "x-request-id");
    assert!(is_uuid(got));
    assert_ne!(got, mine.to_ascii_lowercase());
    assert_ne!(got, "0194f5a0-7b1c-7d2e-8f3a-000000000000");
}

#[tokio::test]
async fn a_refused_rpc_call_has_one_warn_line_with_the_id_of_its_response() {
    let f = fixture("warn").await;
    let secret = "s3cret-marker-9f2c";
    let id = "0194f5a0-7b1c-7d2e-8f3a-123456789abc";
    let (response, body) = f
        .json_post(
            "/erp/json/2/res.users/search",
            Some(id),
            &format!(r#"["{secret}"]"#),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(header(&response, "x-request-id"), id);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["request_id"], id);
    assert_eq!(body["name"], "werkzeug.exceptions.BadRequest");
    let mut keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "arguments",
            "context",
            "debug",
            "message",
            "name",
            "request_id",
            "timestamp"
        ]
    );
    let lines = f.log.matching(id);
    assert_eq!(lines.len(), 1, "{:#?}", f.log.lines());
    let line = &lines[0];
    assert!(line.contains(" WARN  erp::rpc: rpc call refused"), "{line}");
    for part in [
        "status=400",
        "name=werkzeug.exceptions.BadRequest",
        "model=res.users",
        "method=search",
        &format!("uid={}", f.uid),
        &format!("request_id={id}"),
    ] {
        assert!(line.contains(part), "{part} missing in {line}");
    }
    assert!(
        !line.contains("addon="),
        "no WASM addon on a native method: {line}"
    );
    assert!(
        f.log.lines().iter().all(|l| !l.contains(secret)),
        "no argument value reaches the log: {:#?}",
        f.log.lines()
    );
}

#[tokio::test]
async fn a_generated_id_is_the_same_in_header_body_and_log() {
    let f = fixture("warn").await;
    let (response, body) = f
        .json_post("/erp/json/2/no.such.model/read", None, "{}")
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let id = header(&response, "x-request-id").to_owned();
    assert!(is_uuid(&id));
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["request_id"], id.as_str());
    let lines = f.log.matching(&id);
    assert_eq!(lines.len(), 1, "{:#?}", f.log.lines());
    assert!(lines[0].contains("status=404"), "{}", lines[0]);
    assert!(lines[0].contains("model=no.such.model"), "{}", lines[0]);
}

#[tokio::test]
async fn a_successful_rpc_call_writes_no_log_line() {
    let f = fixture("info").await;
    let (response, body) = f
        .json_post(
            "/erp/json/2/res.users/search_count",
            None,
            r#"{"domain": []}"#,
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(is_uuid(header(&response, "x-request-id")));
    assert_eq!(f.log.lines(), Vec::<String>::new());
}

#[tokio::test]
async fn http_level_refusals_are_logged_once_and_unknown_paths_stay_quiet() {
    let f = fixture("warn").await;
    let denied = f
        .send(
            Request::builder()
                .uri("/erp/login?db=test&password=hunter2")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let id = header(&denied, "x-request-id").to_owned();
    let lines = f.log.matching(&id);
    assert_eq!(lines.len(), 1, "{:#?}", f.log.lines());
    assert!(
        lines[0].contains(" WARN  erp::http: http request refused"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("status=403"), "{}", lines[0]);
    assert!(lines[0].contains("path=/erp/login"), "{}", lines[0]);
    assert!(
        !lines[0].contains("hunter2"),
        "the query is never logged: {}",
        lines[0]
    );
    // wrong media type on a JSON endpoint
    let (wrong, _) = {
        let response = f
            .send(
                f.post_as("/erp/json/2/res.users/search", "same-origin", "text/plain")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await;
        (response, ())
    };
    assert_eq!(wrong.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(f.log.matching(header(&wrong, "x-request-id")).len(), 1);
    // a scanner path
    let before = f.log.lines().len();
    let missing = f
        .send(
            Request::builder()
                .uri("/wp-login.php")
                .header("x-erp-proxy-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(f.log.lines().len(), before, "a 404 is not a warn");
}

#[tokio::test]
async fn a_refused_shell_url_logs_its_fixed_reason_once_and_never_the_query() {
    let f = fixture("warn").await;
    let refused = f
        .send(
            Request::builder()
                .uri("/erp?foo=marker-q7v2")
                .header("x-erp-proxy-token", TOKEN)
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let id = header(&refused, "x-request-id").to_owned();
    let lines = f.log.matching(&id);
    assert_eq!(lines.len(), 1, "{:#?}", f.log.lines());
    let line = &lines[0];
    assert!(
        line.contains(" WARN  erp::http: http request refused"),
        "{line}"
    );
    for part in [
        "status=400",
        "path=/erp ",
        "reason=\"redirect must be an allowed local shell URL\"",
    ] {
        assert!(line.contains(part), "{part} missing in {line}");
    }
    assert!(
        f.log.lines().iter().all(|l| !l.contains("marker-q7v2")),
        "the query is never logged: {:#?}",
        f.log.lines()
    );
}

#[tokio::test]
async fn an_error_page_reference_is_generated_and_logged_beside_the_request_id() {
    let f = fixture("warn").await;
    let supplied = "0194f5a0-7b1c-7d2e-8f3a-123456789abc";
    let denied = f
        .send(
            Request::builder()
                .uri("/erp/login")
                .header("x-request-id", supplied)
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(header(&denied, "x-request-id"), supplied);
    let body = String::from_utf8(
        to_bytes(denied.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let reference = body
        .split("Reference: <code>")
        .nth(1)
        .unwrap()
        .split("</code>")
        .next()
        .unwrap()
        .to_owned();
    assert!(is_uuid(&reference), "{reference}");
    assert_ne!(
        reference, supplied,
        "the client's id is never the page reference"
    );
    let lines = f.log.matching(supplied);
    assert_eq!(lines.len(), 1, "{:#?}", f.log.lines());
    let line = &lines[0];
    assert!(
        line.contains(" WARN  erp::http: http request refused"),
        "{line}"
    );
    for part in [
        "status=403".to_owned(),
        format!("request_id={supplied}"),
        format!("error_id={reference}"),
    ] {
        assert!(line.contains(&part), "{part} missing in {line}");
    }
}

#[tokio::test]
async fn at_info_level_a_not_found_is_logged_with_its_id() {
    let f = fixture("info").await;
    let missing = f
        .send(
            Request::builder()
                .uri("/wp-login.php")
                .header("x-erp-proxy-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let lines = f.log.matching(header(&missing, "x-request-id"));
    assert_eq!(lines.len(), 1, "{:#?}", f.log.lines());
    assert!(
        lines[0].contains(" INFO  erp::http: http request not found"),
        "{}",
        lines[0]
    );
}

fn report(extra: &str) -> String {
    format!(
        r#"{{"kind":"rpc","message":"call failed","path":"/erp/action-1","build":"build-7"{extra}}}"#
    )
}

/// `warn` with the user, the report's own request id and the id of the failed call.
#[tokio::test]
async fn a_client_error_report_is_logged_with_the_user_and_the_failed_calls_id() {
    let f = fixture("warn").await;
    let failed_call = "0194f5a0-7b1c-7d2e-8f3a-123456789abc";
    let (response, body) = f
        .json_post(
            "/erp/client-error",
            None,
            &report(&format!(
                r#","request_id":"{failed_call}","stack":"at foo (a.js:1:1)\nat bar""#
            )),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(body.is_empty(), "never echoed");
    let own = header(&response, "x-request-id").to_owned();
    let lines = f.log.matching("client error report");
    assert_eq!(lines.len(), 1, "{:#?}", f.log.lines());
    let line = &lines[0];
    assert!(line.contains(" WARN  erp::client:"), "{line}");
    for part in [
        format!("uid={}", f.uid),
        format!("request_id={own}"),
        format!("reported_request_id={failed_call}"),
        "kind=rpc".to_owned(),
        "path=/erp/action-1".to_owned(),
        "build=build-7".to_owned(),
        r#"text="call failed""#.to_owned(),
        r#"stack="at foo (a.js:1:1)\nat bar""#.to_owned(),
    ] {
        assert!(line.contains(&part), "{part} missing in {line}");
    }
    assert_eq!(
        f.log.lines().len(),
        1,
        "one line, one event: {:#?}",
        f.log.lines()
    );
}

#[tokio::test]
async fn a_client_error_report_that_breaks_a_rule_is_refused_loudly() {
    let f = fixture("warn").await;
    // not signed in
    let anonymous = f
        .send(
            Request::builder()
                .method("POST")
                .uri("/erp/client-error")
                .header("x-erp-proxy-token", TOKEN)
                .header("sec-fetch-site", "same-origin")
                .header("content-type", "application/json")
                .body(Body::from(report("")))
                .unwrap(),
        )
        .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    // cross-site
    let cross = f
        .send(
            f.post_as("/erp/client-error", "cross-site", "application/json")
                .body(Body::from(report("")))
                .unwrap(),
        )
        .await;
    assert_eq!(cross.status(), StatusCode::FORBIDDEN);
    // a simple (form) content type, which a cross-site form could send without preflight
    let form = f
        .send(
            f.post_as("/erp/client-error", "same-origin", "text/plain")
                .body(Body::from(report("")))
                .unwrap(),
        )
        .await;
    assert_eq!(form.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // GET
    let get = f
        .send(
            Request::builder()
                .uri("/erp/client-error")
                .header("x-erp-proxy-token", TOKEN)
                .header("cookie", &f.session_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);
    // oversized
    let (big, _) = f
        .json_post(
            "/erp/client-error",
            None,
            &report(&format!(r#","stack":"{}""#, "x".repeat(9000))),
        )
        .await;
    assert_eq!(big.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // unknown field, unknown kind, bad id, bad path
    for bad in [
        report(r#","extra":1"#),
        report("").replace("\"rpc\"", "\"nope\""),
        report(r#","request_id":"nope""#),
        report("").replace("/erp/action-1", "/erp?token=abc"),
    ] {
        let (response, body) = f.json_post("/erp/client-error", None, &bad).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
        assert!(!body.is_empty());
    }
    assert!(
        f.log.matching("client error report").is_empty(),
        "refused reports are not logged as reports: {:#?}",
        f.log.lines()
    );
}

#[tokio::test]
async fn client_error_reports_are_rate_limited_per_session() {
    let f = fixture("warn").await;
    for n in 0..10 {
        let (response, _) = f.json_post("/erp/client-error", None, &report("")).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "report {n}");
    }
    let (limited, _) = f.json_post("/erp/client-error", None, &report("")).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&limited, "retry-after"), "60");
    assert_eq!(f.log.matching("client error report").len(), 10);
}
