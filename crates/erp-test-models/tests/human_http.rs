#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "auth_support/mod.rs"]
mod support;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use erp_server::{config::ServerConfig, web::WebApp};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;
#[tokio::test]
async fn login_http_verifies_proxy_and_form_csrf() {
    let world = support::world().await;
    let fixture = fixture();
    let config = Arc::clone(&fixture.config);
    let published = Arc::clone(&fixture.published);
    let hash_path = fixture.hash_path.clone();
    let assembly_before = erp_addon_web::assets::assembly::assembly_calls();
    assert!(assembly_before < u64::MAX);
    // Independent testkit database: one tenant's failed startup must not poison another's shell.
    let broken_world = support::world().await;
    let broken = Arc::new(
        WebApp::new(Arc::clone(&config), Arc::new(broken_world.auth))
            .unwrap()
            .with_assets(
                "elang-1",
                erp_server::web::AssetAdmission::Broken("fixture asset startup failed".into()),
            )
            .unwrap(),
    );
    let broken_response = broken
        .router("127.0.0.1".parse().unwrap())
        .oneshot(
            Request::builder()
                .uri("/erp/login")
                .header(
                    "x-erp-proxy-token",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(broken_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    // The public error renderer intentionally hides the internal startup detail. Assert its
    // complete user message; surrounding branded HTML/layout belongs to the renderer's tests.
    let broken_html = String::from_utf8(
        to_bytes(broken_response.into_body(), 65_536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(broken_html.contains("<h1 id=\"error-title\">Temporarily unavailable</h1>"));
    assert!(broken_html.contains("The application is busy or updating. Please try again shortly."));
    assert!(!broken_html.contains("fixture asset startup failed"));
    // Script/asset requests get no error page: their plain 503 text must not carry the detail either.
    let broken_asset = broken
        .router("127.0.0.1".parse().unwrap())
        .oneshot(
            Request::builder()
                .uri("/erp/assets/main.js")
                .header(
                    "x-erp-proxy-token",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(broken_asset.status(), StatusCode::SERVICE_UNAVAILABLE);
    let broken_asset_text = String::from_utf8(
        to_bytes(broken_asset.into_body(), 65_536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!broken_asset_text.contains("fixture asset startup failed"));
    // A request-path re-admission refusal (for example the assets.memory_bytes limit) answers
    // the same fixed 503 text; its detail goes to the server log only (lead r1 MUST-1).
    let refused_world = support::world().await;
    let refusals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let refused = Arc::new(
        WebApp::new_with_store(
            Arc::clone(&config),
            Arc::new(refused_world.auth),
            asset_store(),
        )
        .unwrap()
        .with_assets(
            "elang-1",
            erp_server::web::AssetAdmission::Published(Arc::clone(&published)),
        )
        .unwrap()
        .with_asset_authority(Arc::new(RefusingAuthority(Arc::clone(&refusals)))),
    );
    refused.set_login_available(true);
    let refused_asset = refused
        .router("127.0.0.1".parse().unwrap())
        .oneshot(
            Request::builder()
                .uri(format!("/erp/assets/{hash_path}"))
                .header(
                    "x-erp-proxy-token",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused_asset.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        refusals.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the hashed asset request must consult the current authority"
    );
    let refused_text = String::from_utf8(
        to_bytes(refused_asset.into_body(), 65_536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!refused_text.contains(REFUSAL_DETAIL), "{refused_text}");
    assert!(
        refused_text.contains("the operator must check the server log"),
        "{refused_text}"
    );
    let app = Arc::new(
        WebApp::new_with_store(config, Arc::new(world.auth), asset_store())
            .unwrap()
            .with_assets(
                "elang-1",
                erp_server::web::AssetAdmission::Published(published),
            )
            .unwrap(),
    );
    // Unknown scheduler health defaults closed before either login method reads a body.
    for method in ["GET", "POST"] {
        let reply = app
            .router("127.0.0.1".parse().unwrap())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/erp/login")
                    .header(
                        "x-erp-proxy-token",
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    app.set_login_available(true);
    let router = app.router("127.0.0.1".parse().unwrap());
    for (uri, cache) in [
        (
            format!("/erp/assets/{hash_path}"),
            "private, max-age=31536000, immutable",
        ),
        ("/fixture/static/main.js".to_owned(), "no-cache"),
    ] {
        let reply = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header(
                        "x-erp-proxy-token",
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    )
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(reply.headers()["cache-control"], cache);
        assert_eq!(reply.headers()["content-encoding"], "gzip");
        assert_eq!(reply.headers()["vary"], "Accept-Encoding");
        let tag = reply.headers()["etag"].to_str().unwrap().to_owned();
        assert_eq!(
            to_bytes(reply.into_body(), 1_000_000)
                .await
                .unwrap()
                .as_ref(),
            fixture.gzip.as_slice()
        );
        let reply = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header(
                        "x-erp-proxy-token",
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    )
                    .header("accept-encoding", "gzip")
                    .header("if-none-match", &tag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(reply.headers()["cache-control"], cache);
        assert_eq!(reply.headers()["etag"], tag);
        assert!(to_bytes(reply.into_body(), 100).await.unwrap().is_empty());
    }
    let excluded = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/fixture/static/main.js")
                .header(
                    "x-erp-proxy-token",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .header("accept-encoding", "*;q=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(excluded.status(), StatusCode::NOT_ACCEPTABLE);
    assert_eq!(excluded.headers()["cache-control"], "no-store");
    let denied = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/erp/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let legacy = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/web/login")
                .header(
                    "x-erp-proxy-token",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::SEE_OTHER);
    assert_eq!(legacy.headers()["location"], "/erp/login");
    let page = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/erp/login")
                .header(
                    "x-erp-proxy-token",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["cache-control"], "no-store");
    let cookie = page.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(cookie.starts_with("prefs="));
    let html =
        String::from_utf8(to_bytes(page.into_body(), 65_536).await.unwrap().to_vec()).unwrap();
    let json = html
        .split("id=\"erp-login\">")
        .nth(1)
        .unwrap()
        .split("</script>")
        .next()
        .unwrap();
    let dto: serde_json::Value = serde_json::from_str(json).unwrap();
    assert!(dto["csrf"].as_str().unwrap().len() > 64);
    assert_eq!(dto["database"], "test");
    let refused = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/erp/login")
                .header(
                    "x-erp-proxy-token",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .header("cookie", cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "db=test&login=admin&password=wrong&redirect=%2Ferp&csrf_token=bad",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        to_bytes(refused.into_body(), 4096).await.unwrap().as_ref(),
        b"Session expired (invalid CSRF token)"
    );
    assert_eq!(
        erp_addon_web::assets::assembly::assembly_calls(),
        assembly_before,
        "HTTP requests must never invoke assembly"
    );
}

/// The proxy token written into the fixture configuration.
const PROXY_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The fixture server configuration (authenticated proxy, `https://erp.example.test`), the
/// published fixture assets, the hashed path of the first blob and its gzip form.
struct Fixture {
    _dir: tempfile::TempDir,
    config: Arc<ServerConfig>,
    published: Arc<erp_server::assets::PublishedAssets>,
    hash_path: String,
    gzip: Vec<u8>,
}

fn fixture() -> Fixture {
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
    let bundle = erp_addon_web::assets::assembly::assemble(
        &[],
        &erp_addon_web::assets::assembly::AssemblyOptions::new(
            "fixture",
            "elang-1",
            "en_US",
            "main",
            erp_sdk::ui_compiler::CompileOptions::new(erp_sdk::ui_compiler::BuildMode::Prod),
        )
        .unwrap(),
    )
    .unwrap();
    let mut generation =
        erp_server::assets::AssetGeneration::new("elang-1", bundle.key().to_owned()).unwrap();
    for blob in bundle.blobs() {
        generation = generation
            .asset(
                erp_server::assets::AssetSource::with_encodings(
                    &format!("{}.{}", blob.sha256(), blob.extension()),
                    blob.media_type(),
                    blob.bytes().to_vec(),
                    blob.gzip().to_vec(),
                    blob.brotli().to_vec(),
                )
                .unwrap(),
            )
            .unwrap();
    }
    let blob = bundle.blobs().first().unwrap();
    let hash_path = format!("{}.{}", blob.sha256(), blob.extension());
    generation = generation
        .static_file(
            "/fixture/static/main.js",
            &format!("/erp/assets/{hash_path}"),
        )
        .unwrap();
    let published = Arc::new(generation.build(&asset_store()).unwrap());
    let config = Arc::new(ServerConfig::load(&dir.path().join("server.toml")).unwrap());
    Fixture {
        gzip: blob.gzip().to_vec(),
        _dir: dir,
        config,
        published,
        hash_path,
    }
}

/// The exact shell policy of `assets.rs` for `nonce` and the fixture origin's authority.
fn shell_csp(origin: &str, nonce: &str) -> String {
    class_policy(origin, &format!("'self' 'nonce-{nonce}'"))
}

/// The threat model §13 `shell`/`login` policy with `script_src` as the `script-src` value.
fn class_policy(origin: &str, script_src: &str) -> String {
    let authority = origin.split_once("://").unwrap().1;
    format!(
        "default-src 'self'; script-src {script_src}; style-src-elem 'self'; style-src-attr 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self' wss://{authority}; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'"
    )
}

/// The nonce of the one import map in a served document.
fn import_map_nonce(html: &str) -> String {
    html.split("<script type=\"importmap\" nonce=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

/// One request through a fresh copy of `router`; status, headers and body text.
async fn send(
    router: &axum::Router,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let reply = router.clone().oneshot(request).await.unwrap();
    let status = reply.status();
    let headers = reply.headers().clone();
    let body = String::from_utf8(
        to_bytes(reply.into_body(), 1_000_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    (status, headers, body)
}

/// A logout form post with `cookie` and `token`, plus any `extra` header.
fn logout(cookie: &str, token: &str, extra: Option<(&str, &str)>) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/erp/session/logout")
        .header("x-erp-proxy-token", PROXY_TOKEN)
        .header("cookie", cookie)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some((name, value)) = extra {
        request = request.header(name, value);
    }
    request
        .body(Body::from(format!("csrf_token={token}")))
        .unwrap()
}

/// A proxied GET of `uri` with an optional `cookie`.
fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .uri(uri)
        .header("x-erp-proxy-token", PROXY_TOKEN);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.body(Body::empty()).unwrap()
}

/// The cleanup a successful logout answers with, whatever the session state was.
fn assert_logout_cleanup(status: StatusCode, headers: &axum::http::HeaderMap) {
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/erp/login");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_cleanup_headers(headers);
}

/// The browser cleanup of a verified logout: `Clear-Site-Data` and exactly the expired session
/// cookie, never cached.
fn assert_cleanup_headers(headers: &axum::http::HeaderMap) {
    assert_eq!(headers["clear-site-data"], "\"cache\", \"storage\"");
    assert_eq!(headers["cache-control"], "no-store");
    let cookies: Vec<&str> = headers
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    // the fixture origin is https, so the expired cookie keeps `Secure`
    assert_eq!(
        cookies,
        ["session_id=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; Secure"]
    );
}

/// No browser cleanup at all: a refused logout clears nothing.
fn assert_no_cleanup(headers: &axum::http::HeaderMap) {
    assert!(!headers.contains_key("clear-site-data"));
    assert!(!headers.contains_key("set-cookie"));
}

#[tokio::test]
async fn logout_and_login_pages_send_their_header_classes() {
    let world = support::world().await;
    let alice = support::user_with_password(&world.h, "alice", "alice-pw").await;
    let fixture = fixture();
    let origin = fixture.config.origin().to_owned();
    let auth = Arc::new(world.auth);
    let app = Arc::new(
        WebApp::new_with_store(
            Arc::clone(&fixture.config),
            Arc::clone(&auth),
            asset_store(),
        )
        .unwrap()
        .with_assets(
            "elang-1",
            erp_server::web::AssetAdmission::Published(Arc::clone(&fixture.published)),
        )
        .unwrap(),
    );
    app.set_login_available(true);
    let router = app.router("127.0.0.1".parse().unwrap());

    // Login page: login class, the shell CSP with this response's nonce, a fresh nonce each time.
    let mut pages = Vec::new();
    for _ in 0..2 {
        let (status, headers, html) = send(&router, get("/erp/login", None)).await;
        assert_eq!(status, StatusCode::OK);
        let nonce = import_map_nonce(&html);
        assert_eq!(
            headers["content-security-policy"],
            shell_csp(&origin, &nonce).as_str()
        );
        assert_eq!(headers["x-frame-options"], "DENY");
        // The page hosts the login form: under `no-referrer` a browser sends `Origin: null` on the
        // form's POST, which the same-origin check refuses (nobody could sign in, 2026-11-12).
        assert_eq!(headers["referrer-policy"], "same-origin");
        assert_eq!(headers["cache-control"], "no-store");
        pages.push((nonce, headers, html));
    }
    assert_ne!(pages[0].0, pages[1].0, "each login page has its own nonce");
    let (_, page_headers, page_html) = pages.swap_remove(0);
    let prefs = page_headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(prefs.starts_with("prefs="));
    let dto: serde_json::Value = serde_json::from_str(
        page_html
            .split("id=\"erp-login\">")
            .nth(1)
            .unwrap()
            .split("</script>")
            .next()
            .unwrap(),
    )
    .unwrap();
    let token = dto["csrf"].as_str().unwrap().to_owned();

    // Legacy login alias: a local redirect, so `no-referrer` and `no-store`.
    let (status, headers, _) = send(&router, get("/web/login", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/erp/login");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(headers["cache-control"], "no-store");

    let login = support::login(&auth, "alice", "alice-pw").await.unwrap();
    let secret = support::cookie(&login);
    let cookies = format!(
        "{prefs}; session_id=test.{}",
        login.session().secret().to_cookie().as_str()
    );
    assert_eq!(support::check_user(&auth, &secret).await.unwrap(), alice);

    // Login success: a fixed HTML document with the login class headers.
    let (status, headers, html) = send(
        &router,
        get("/erp/login_successful", Some(cookies.as_str())),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert!(html.contains("Login successful."), "{html}");
    assert_eq!(
        headers["content-security-policy"],
        shell_csp(&origin, &import_map_nonce(&html)).as_str()
    );
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "same-origin");
    assert_eq!(headers["cache-control"], "no-store");

    // A refused token neither clears browser state nor revokes the session.
    let (status, headers, body) = send(&router, logout(&cookies, "invalid", None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "Session expired (invalid CSRF token)");
    assert_no_cleanup(&headers);
    assert_eq!(support::check_user(&auth, &secret).await.unwrap(), alice);

    // A cross-site form is refused before the token is read, with the same guarantees.
    let (status, headers, body) = send(
        &router,
        logout(&cookies, &token, Some(("sec-fetch-site", "cross-site"))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "cross-origin form refused");
    assert_no_cleanup(&headers);
    assert_eq!(support::check_user(&auth, &secret).await.unwrap(), alice);

    // A valid logout revokes the live session and clears this site's browser state.
    let (status, headers, _) = send(&router, logout(&cookies, &token, None)).await;
    assert_logout_cleanup(status, &headers);
    let refused = support::check_user(&auth, &secret).await.unwrap_err();
    assert_eq!(
        (refused.kind(), refused.message()),
        (
            erp_fields::ErrorKind::SessionExpired,
            "user is not connected"
        )
    );

    // The same cleanup for the session just revoked, for a secret the server never issued and
    // for no session cookie at all. A session past its lifetime is not exercised here.
    let unknown = format!("{prefs}; session_id=test.{}", "A".repeat(43));
    for cookie in [cookies.as_str(), unknown.as_str(), prefs.as_str()] {
        let (status, headers, _) = send(&router, logout(cookie, &token, None)).await;
        assert_logout_cleanup(status, &headers);
    }
}

const REFUSAL_DETAIL: &str =
    "asset publication refused: assets.memory_bytes admission limit exceeded (fixture key k1)";

/// An authority that always refuses, counting how often the server consults it.
struct RefusingAuthority(Arc<std::sync::atomic::AtomicUsize>);

impl erp_server::web::AssetAuthority for RefusingAuthority {
    fn admit<'a>(
        &'a self,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Arc<erp_server::assets::PublishedAssets>>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Err(erp_fields::Error::validation(REFUSAL_DETAIL)) })
    }
}

fn asset_store() -> Arc<erp_server::assets::AssetStore> {
    static STORE: std::sync::OnceLock<Arc<erp_server::assets::AssetStore>> =
        std::sync::OnceLock::new();
    Arc::clone(
        STORE.get_or_init(|| erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap()),
    )
}

#[tokio::test]
async fn a_page_without_published_assets_still_has_the_login_class_policy() {
    let world = support::world().await;
    let fixture = fixture();
    let origin = fixture.config.origin().to_owned();
    let app = Arc::new(
        WebApp::new_with_store(
            Arc::clone(&fixture.config),
            Arc::new(world.auth),
            asset_store(),
        )
        .unwrap(),
    );
    app.set_login_available(true);
    let router = app.router("127.0.0.1".parse().unwrap());
    let (status, headers, html) = send(&router, get("/erp/login", None)).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        !html.contains("<script type=\"importmap\""),
        "no generation, no import map: {html}"
    );
    assert_eq!(
        headers["content-security-policy"],
        class_policy(&origin, "'self'").as_str()
    );
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "same-origin");
    assert_eq!(headers["cache-control"], "no-store");
}

/// The access policy `erp serve` installs (`PolicySource`), bound after the harness exists: the
/// login POST and the shell guard ask `has_group`, which the harness's default policy refuses to
/// answer (a `Programming` error, so a 500). The source reads the database, so it cannot exist
/// before the harness has built it; until [`ProductionPolicy::install`] the harness's own
/// `AllowAll` serves its schema and fixture setup, never a request of a test.
struct ProductionPolicy {
    setup: Arc<dyn erp_orm::PolicyProvider>,
    source: std::sync::OnceLock<erp_security::source::PolicySource>,
}

impl ProductionPolicy {
    fn install(&self, source: erp_security::source::PolicySource) {
        assert!(self.source.set(source).is_ok(), "installed once");
    }
    fn provider(&self) -> &dyn erp_orm::PolicyProvider {
        match self.source.get() {
            Some(source) => source,
            None => self.setup.as_ref(),
        }
    }
}

impl erp_orm::PolicyProvider for ProductionPolicy {
    fn current(&self) -> Arc<dyn erp_orm::AccessPolicy> {
        self.provider().current()
    }
    fn rebuild_private<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Arc<dyn erp_orm::AccessPolicy>>> {
        self.provider().rebuild_private(tx)
    }
}

/// The support world on the production access policy and principal resolver, as `erp serve`
/// composes them (`crates/erp-cli/src/human.rs`, `PolicySource::build`, `Resolver::for_registry`).
async fn production_policy_world() -> support::World {
    use erp_orm::IntoPolicyProvider as _;
    let policy = Arc::new(ProductionPolicy {
        setup: erp_orm::StaticPolicy::AllowAll
            .into_policy_provider()
            .unwrap(),
        source: std::sync::OnceLock::new(),
    });
    let registry = support::builder().registry_only().unwrap();
    let h = support::builder()
        .policy(Arc::clone(&policy) as Arc<dyn erp_orm::PolicyProvider>)
        .resolver(erp_security::resolver::Resolver::for_registry(&registry).unwrap())
        .build()
        .await
        .unwrap();
    let world = support::world_with(
        h,
        erp_security::auth::AuthConfig::DEFAULT,
        erp_security::auth::HashPoolConfig::DEFAULT,
    )
    .await;
    let pool = erp_orm::pool::create_pool(world.h.db().config(), 2).unwrap();
    policy.install(
        erp_security::source::PolicySource::build(Arc::clone(world.h.registry()), pool)
            .await
            .unwrap(),
    );
    world
}

/// The id of `base.<name>` (a `res.groups` record the support world binds).
async fn base_group(h: &erp_testkit::Harness, name: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT res_id FROM ir_model_data \
              WHERE module = 'base' AND name = $1 AND model = 'res.groups'",
            &[&name],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "base.{name} is bound once");
    let id: i64 = rows[0].get(0);
    tx.commit().await.unwrap();
    id
}

/// A user `login` with `password` and exactly the direct groups `groups`, created by the system.
async fn user_in_groups(
    h: &erp_testkit::Harness,
    login: &str,
    password: &str,
    groups: &[i64],
) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let id = support::create(
        &mut tx,
        "res.users",
        &[
            ("name", support::s(login)),
            ("login", support::s(login)),
            ("password", support::s(password)),
            ("group_ids", support::set_cmd(groups)),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    id
}

/// A proxied GET of `uri` with `accept` and an optional `cookie`.
fn get_accepting(uri: &str, accept: &str, cookie: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .uri(uri)
        .header("x-erp-proxy-token", PROXY_TOKEN)
        .header("accept", accept);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.body(Body::empty()).unwrap()
}

/// The `prefs` cookie pair (`prefs=…`) and the login token of a fresh login page.
async fn login_page_prefs(router: &axum::Router) -> (String, String) {
    let (status, headers, html) = send(router, get("/erp/login", None)).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let prefs = headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(prefs.starts_with("prefs="), "{prefs}");
    let dto: serde_json::Value = serde_json::from_str(
        html.split("id=\"erp-login\">")
            .nth(1)
            .unwrap()
            .split("</script>")
            .next()
            .unwrap(),
    )
    .unwrap();
    (prefs, dto["csrf"].as_str().unwrap().to_owned())
}

/// The fixture application with its published assets, login open, on a fresh router.
fn published_app(fixture: &Fixture, auth: &Arc<erp_security::auth::Auth>) -> axum::Router {
    let app = Arc::new(
        WebApp::new_with_store(Arc::clone(&fixture.config), Arc::clone(auth), asset_store())
            .unwrap()
            .with_assets(
                "elang-1",
                erp_server::web::AssetAdmission::Published(Arc::clone(&fixture.published)),
            )
            .unwrap(),
    );
    app.set_login_available(true);
    app.router("127.0.0.1".parse().unwrap())
}

/// shell access guard: a signed-in user outside `base.group_user` opening the shell as
/// an HTML request gets the login-success page in place (200, `login` class, no bootstrap, no
/// redirect, no cookie); a request that does not ask for HTML keeps the 403. No spec case covers
/// the guard, so this test carries no spec tag.
#[tokio::test]
async fn portal_user_opening_the_shell_gets_the_login_success_page() {
    let world = production_policy_world().await;
    let portal = base_group(&world.h, "group_portal").await;
    user_in_groups(&world.h, "pat", "pat-pw", &[portal]).await;
    let fixture = fixture();
    let origin = fixture.config.origin().to_owned();
    let auth = Arc::new(world.auth);
    let router = published_app(&fixture, &auth);
    let (prefs, _) = login_page_prefs(&router).await;
    let login = support::login(&auth, "pat", "pat-pw").await.unwrap();
    let cookies = format!(
        "{prefs}; session_id=test.{}",
        login.session().secret().to_cookie().as_str()
    );

    let (status, headers, html) = send(
        &router,
        get_accepting(
            "/erp",
            "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
            Some(&cookies),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("<h1>Login successful.</h1>"), "{html}");
    assert!(!html.contains("erp-bootstrap"), "{html}");
    assert!(!headers.contains_key("location"));
    assert!(!headers.contains_key("set-cookie"));
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(
        headers["content-security-policy"],
        shell_csp(&origin, &import_map_nonce(&html)).as_str()
    );
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "same-origin");
    assert_eq!(headers["cache-control"], "no-store");

    // Not an HTML request: the 403 stays. A GET is still rendered as the navigation error page,
    // so the status and the page title are what the client sees.
    let (status, _, html) = send(
        &router,
        get_accepting("/erp", "application/json", Some(&cookies)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        html.contains("<h1 id=\"error-title\">Access is restricted</h1>"),
        "{html}"
    );
    assert!(!html.contains("Login successful."), "{html}");
    assert!(!html.contains("erp-bootstrap"), "{html}");
}

#[tokio::test]
async fn logout_cleans_up_on_malformed_session_cookie() {
    let world = support::world().await;
    let alice = support::user_with_password(&world.h, "alice", "alice-pw").await;
    let fixture = fixture();
    let auth = Arc::new(world.auth);
    let router = published_app(&fixture, &auth);
    let (prefs, token) = login_page_prefs(&router).await;
    let login = support::login(&auth, "alice", "alice-pw").await.unwrap();
    let secret = support::cookie(&login);
    let live = login.session().secret().to_cookie().as_str().to_owned();
    let garbage = format!("{prefs}; session_id=nodot");
    // a duplicated envelope that holds the live session: refusing it must not revoke that session
    let duplicate = format!(
        "{prefs}; session_id=test.{live}; session_id=test.{}",
        "A".repeat(43)
    );
    for cookie in [garbage.as_str(), duplicate.as_str()] {
        // Refused before the token is verified: nothing is cleared.
        let (status, headers, body) = send(&router, logout(cookie, "invalid", None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{cookie}");
        assert_eq!(body, "Session expired (invalid CSRF token)", "{cookie}");
        assert_no_cleanup(&headers);
        let (status, headers, body) = send(
            &router,
            logout(cookie, &token, Some(("sec-fetch-site", "cross-site"))),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{cookie}");
        assert_eq!(body, "cross-origin form refused", "{cookie}");
        assert_no_cleanup(&headers);

        // A verified token: the ambiguous cookie is refused, the browser is still cleaned up,
        // and no sign-out is claimed (no redirect).
        let (status, headers, body) = send(&router, logout(cookie, &token, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{cookie}");
        assert_eq!(
            body, "ambiguous or malformed authentication cookie",
            "{cookie}"
        );
        assert!(!headers.contains_key("location"), "{cookie}");
        assert_cleanup_headers(&headers);
        assert_eq!(
            support::check_user(&auth, &secret).await.unwrap(),
            alice,
            "no revocation for {cookie}"
        );
    }
}

/// The unsafe `redirect=` values of TO-SHELL.3 (threat model M4), as the server must see them.
const UNSAFE_REDIRECTS: [&str; 14] = [
    "//evil",
    "https://evil",
    "/\\evil",
    "\\\\evil",
    "/%09/evil",
    "/%0d%0a/evil",
    "/%2F%2Fevil",
    "/%5Cevil",
    "javascript:alert(1)",
    "/%0d%0aSet-Cookie:x",
    // The same vectors as the server sees them after the ordinary single form decoding.
    "/\t/evil",
    "/\r\n/evil",
    "/\r\nSet-Cookie:x",
    "/\\/evil",
];

/// Accepted `redirect=` values and the local destination each becomes: a canonical-origin
/// absolute URL is re-serialized as its path, and a legacy action path maps to `/erp`.
const ACCEPTED_REDIRECTS: [(&str, &str); 3] = [
    ("https://erp.example.test/odoo/x?a=1", "/odoo/x?a=1"),
    ("/erp/action-8?menu=9", "/erp/action-8?menu=9"),
    ("/odoo/action-8?menu=9", "/erp/action-8?menu=9"),
];

/// `value` percent-encoded for a query string or form body: every byte outside the unreserved
/// set as `%XX`, so the server's form decoder yields exactly `value`. (A local encoder: this
/// crate has no URL encoding dependency.)
fn form_value(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[tokio::test]
async fn login_redirect_vectors_on_served_get() {
    let world = support::world().await;
    let fixture = fixture();
    let auth = Arc::new(world.auth);
    let router = published_app(&fixture, &auth);
    for vector in UNSAFE_REDIRECTS {
        let uri = format!("/erp/login?redirect={}", form_value(vector));
        let (status, headers, html) = send(&router, get(&uri, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{vector}");
        assert!(!headers.contains_key("location"), "{vector}");
        assert!(!headers.contains_key("set-cookie"), "{vector}");
        assert!(html.contains("Request could not be processed"), "{vector}");
        assert!(!html.contains("evil"), "{vector}");
    }
    for (input, expected) in ACCEPTED_REDIRECTS {
        let uri = format!("/erp/login?redirect={}", form_value(input));
        let (status, _, html) = send(&router, get(&uri, None)).await;
        assert_eq!(status, StatusCode::OK, "{input}");
        let dto: serde_json::Value = serde_json::from_str(
            html.split("id=\"erp-login\">")
                .nth(1)
                .unwrap()
                .split("</script>")
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(dto["redirect"], expected, "{input}");
    }
}

#[tokio::test]
async fn login_redirect_vectors_on_served_post() {
    let world = production_policy_world().await;
    let internal = base_group(&world.h, "group_user").await;
    user_in_groups(&world.h, "bob", "bob-pw", &[internal]).await;
    let fixture = fixture();
    let auth = Arc::new(world.auth);
    let router = published_app(&fixture, &auth);
    let (prefs, token) = login_page_prefs(&router).await;
    let post = |redirect: &str| {
        Request::builder()
            .method("POST")
            .uri("/erp/login")
            .header("x-erp-proxy-token", PROXY_TOKEN)
            .header("cookie", prefs.as_str())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "db=test&login=bob&password=bob-pw&redirect={}&csrf_token={token}",
                form_value(redirect)
            )))
            .unwrap()
    };
    for vector in UNSAFE_REDIRECTS {
        let (status, headers, body) = send(&router, post(vector)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{vector}");
        assert_eq!(body, "unsafe redirect", "{vector}");
        assert!(!headers.contains_key("location"), "{vector}");
        assert!(
            !headers.contains_key("set-cookie"),
            "no session for {vector}"
        );
    }
    // An internal user is sent to the validated destination (a non-internal one would go to the
    // login-success page whatever the redirect).
    for (input, expected) in ACCEPTED_REDIRECTS {
        let (status, headers, _) = send(&router, post(input)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{input}");
        assert_eq!(headers["location"], expected, "{input}");
        assert!(
            headers["set-cookie"]
                .to_str()
                .unwrap()
                .starts_with("session_id=test."),
            "{input}"
        );
    }
}
