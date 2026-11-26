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
    static BUNDLE: std::sync::OnceLock<erp_addon_web::assets::assembly::AssembledBundle> =
        std::sync::OnceLock::new();
    let bundle = BUNDLE.get_or_init(|| {
        erp_addon_web::assets::assembly::assemble(
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
        .unwrap()
    });
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

/// Exercise the composed login hooks through the browser entry point, including
/// concurrent sign-in, server-directed cookie rotation, logout and role revocation.
#[tokio::test]
async fn composed_login_rotates_logs_out_and_revokes_on_membership_change_over_http() {
    use erp_orm::IntoPolicyProvider as _;
    use erp_security::auth::{AuthConfig, HashPoolConfig};
    let policy = Arc::new(ProductionPolicy {
        setup: erp_orm::StaticPolicy::AllowAll
            .into_policy_provider()
            .unwrap(),
        source: std::sync::OnceLock::new(),
    });
    let builder = || {
        erp_testkit::Harness::builder([
            &erp_addon_base::ADDON,
            &erp_data::model_data::ADDON,
            &erp_test_models::test_login_selection::ADDON,
            &erp_test_models::test_login_selection::credential_override::ADDON,
        ])
        .with_decls([erp_data::ir_model_data_decl()])
        .require_core_tables(erp_security::auth::core_tables())
        .identify_transactions()
    };
    let registry = builder().registry_only().unwrap();
    let h = builder()
        .policy(Arc::clone(&policy) as Arc<dyn erp_orm::PolicyProvider>)
        .resolver(erp_security::resolver::Resolver::for_registry(&registry).unwrap())
        .build()
        .await
        .unwrap();
    support::prepare(&h).await;
    let uid = support::user_with_password(&h, "http-canonical", "correct").await;
    let mut setup = h.begin().await.unwrap();
    setup
        .execute(
            "UPDATE res_users SET alias='http-alias' WHERE id=$1",
            &[&uid],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let pool = erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    policy.install(
        erp_security::source::PolicySource::build(Arc::clone(h.registry()), pool)
            .await
            .unwrap(),
    );
    let auth = Arc::new(
        erp_security::auth::test_support::login_selection_auth(
            h.orm().clone(),
            AuthConfig::DEFAULT,
            support::own_pool(HashPoolConfig::DEFAULT),
        )
        .await
        .unwrap(),
    );
    let fixture = fixture();
    let app = published_app(&fixture, &auth);
    let (prefs, token) = login_page_prefs(&app).await;
    let signin = || {
        Request::builder()
            .method("POST")
            .uri("/erp/login")
            .header("x-erp-proxy-token", PROXY_TOKEN)
            .header("cookie", prefs.as_str())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "db=test&login=alias%3Ahttp-alias&password=correct&csrf_token={token}"
            )))
            .unwrap()
    };
    let (one, two) = tokio::join!(send(&app, signin()), send(&app, signin()));
    assert_eq!(one.0, StatusCode::SEE_OTHER);
    assert_eq!(two.0, StatusCode::SEE_OTHER);
    let first = oauth_cookie(&one.1).unwrap();
    let second = oauth_cookie(&two.1).unwrap();
    assert!(first != second);
    assert_eq!(
        support::count(
            &h,
            "erp_session WHERE data->>'fixture.canonical_login'='http-canonical'"
        )
        .await,
        2
    );
    let mut setup = h.begin().await.unwrap();
    setup
        .execute(
            "UPDATE erp_session SET created_at=now()-interval '4 hours' WHERE user_id=$1",
            &[&uid],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let (status, rotated, _) = send(&app, get("/erp/login_successful", Some(&first))).await;
    assert_eq!(status, StatusCode::OK);
    let new = oauth_cookie(&rotated).unwrap();
    assert!(new != first);
    let (status, headers, _) = send(&app, logout(&format!("{prefs}; {new}"), &token, None)).await;
    assert_logout_cleanup(status, &headers);
    assert_eq!(
        support::count(&h, &format!("erp_session WHERE user_id={uid}")).await,
        1
    );
    assert_ne!(
        send(&app, get("/erp/login_successful", Some(&new))).await.0,
        StatusCode::OK
    );
    let mut revoke = h.begin().await.unwrap();
    let group =
        support::named_group(&mut revoke, "test.http_membership", "Changed membership").await;
    revoke
        .execute(
            "INSERT INTO res_groups_users_rel(uid,gid) VALUES ($1,$2)",
            &[&uid, &group],
        )
        .await
        .unwrap();
    revoke.commit().await.unwrap();
    assert_ne!(
        send(&app, get("/erp/login_successful", Some(&second)))
            .await
            .0,
        StatusCode::OK
    );
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

struct OidcReply {
    nonce: String,
    challenge: String,
    subject: String,
    acr: String,
    email: String,
}

struct OidcFixture {
    issuer: String,
    secret: String,
    key: ring::signature::Ed25519KeyPair,
    replies: std::sync::Mutex<std::collections::HashMap<String, OidcReply>>,
    exchanges: std::sync::atomic::AtomicUsize,
}

impl OidcFixture {
    async fn start() -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use ring::{
            rand::{SecureRandom, SystemRandom},
            signature::Ed25519KeyPair,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let random = SystemRandom::new();
        let mut secret = [0; 32];
        random.fill(&mut secret).unwrap();
        let generated = Ed25519KeyPair::generate_pkcs8(&random).unwrap();
        let fixture = Arc::new(Self {
            issuer: format!("http://{}", listener.local_addr().unwrap()),
            secret: URL_SAFE_NO_PAD.encode(secret),
            key: Ed25519KeyPair::from_pkcs8(generated.as_ref()).unwrap(),
            replies: std::sync::Mutex::default(),
            exchanges: std::sync::atomic::AtomicUsize::new(0),
        });
        let router = axum::Router::new()
            .route("/token", axum::routing::post(oidc_token))
            .with_state(Arc::clone(&fixture));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (fixture, task)
    }

    fn config(&self, provision: Option<i64>) -> Fixture {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use ring::signature::KeyPair;
        use std::io::Write;
        let mut fixture = fixture();
        let dir = fixture._dir.path();
        let secret = dir.join("oidc-client");
        fs::write(&secret, &self.secret).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(dir.join("oidc-jwks.json"), serde_json::json!({
            "keys":[{"kty":"OKP","crv":"Ed25519","alg":"EdDSA","kid":"test","x":URL_SAFE_NO_PAD.encode(self.key.public_key().as_ref())}]
        }).to_string()).unwrap();
        let mut path = fs::OpenOptions::new()
            .append(true)
            .open(dir.join("server.toml"))
            .unwrap();
        writeln!(path, "[external_identity]\nclient_secret_file='oidc-client'\nsigning_keys_file='oidc-jwks.json'\n[external_identity.provider]\nissuer='{0}'\nauthorization_endpoint='{0}/authorize'\ntoken_endpoint='{0}/token'\nclient_id='client'\nlabel='Fixture Identity'\nrequired_acr='fixture-mfa'", self.issuer).unwrap();
        if let Some(company) = provision {
            writeln!(path, "[external_identity.provider.provisioning]\ncompany_id={company}\nemail_domain='example.test'").unwrap();
        }
        fixture.config = Arc::new(ServerConfig::load(&dir.join("server.toml")).unwrap());
        fixture
    }

    fn callback(
        &self,
        location: &str,
        code: &str,
        subject: &str,
        acr: &str,
        email: &str,
    ) -> String {
        let location = url::Url::parse(location).unwrap();
        assert_eq!(location.origin().ascii_serialization(), self.issuer);
        let query: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
        assert_eq!(
            query["redirect_uri"],
            "https://erp.example.test/erp/login/external/callback"
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["acr_values"], "fixture-mfa");
        self.replies.lock().unwrap().insert(
            code.into(),
            OidcReply {
                nonce: query["nonce"].clone(),
                challenge: query["code_challenge"].clone(),
                subject: subject.into(),
                acr: acr.into(),
                email: email.into(),
            },
        );
        format!(
            "/erp/login/external/callback?code={code}&state={}&iss={}",
            query["state"],
            form_value(&self.issuer)
        )
    }
}

async fn oidc_token(
    axum::extract::State(fixture): axum::extract::State<Arc<OidcFixture>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use base64::{
        Engine,
        engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    };
    use ring::digest::{SHA256, digest};
    fixture
        .exchanges
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let params: std::collections::HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let authorization = format!(
        "Basic {}",
        STANDARD.encode(format!("client:{}", fixture.secret))
    );
    assert!(
        headers["authorization"] == authorization,
        "provider did not receive valid client authentication"
    );
    assert_eq!(
        params["redirect_uri"],
        "https://erp.example.test/erp/login/external/callback"
    );
    assert_eq!(params["grant_type"], "authorization_code");
    let Some(reply) = fixture.replies.lock().unwrap().remove(&params["code"]) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    assert_eq!(
        URL_SAFE_NO_PAD.encode(digest(&SHA256, params["code_verifier"].as_bytes()).as_ref()),
        reply.challenge
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","kid":"test","alg":"EdDSA"}"#);
    let claims = URL_SAFE_NO_PAD.encode(serde_json::json!({"iss": fixture.issuer, "sub":reply.subject,"aud":"client",
        "iat":now,"exp":now+300,"nonce":reply.nonce,"acr":reply.acr,"email":reply.email,"email_verified":true}).to_string());
    let signed = format!("{header}.{claims}");
    let signature = URL_SAFE_NO_PAD.encode(fixture.key.sign(signed.as_bytes()).as_ref());
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"id_token":format!("{signed}.{signature}")}).to_string(),
    )
        .into_response()
}

fn oidc_post(path: &str, cookie: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("x-erp-proxy-token", PROXY_TOKEN)
        .header("cookie", cookie)
        .header("origin", "https://erp.example.test")
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

async fn oidc_start(app: &axum::Router, cookie: &str, token: &str, link: bool) -> String {
    let post = format!(
        "csrf_token={}{}",
        form_value(token),
        if link { "&link=1" } else { "" }
    );
    let (status, headers, body) = send(app, oidc_post("/erp/login/external", cookie, post)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["location"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn oauth_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|value| value.starts_with("session_id="))
        .map(str::to_owned)
}

#[tokio::test]
async fn external_web_login_links_explicitly_checks_mfa_and_revokes_sessions() {
    use erp_security::auth::{ClientAddr, LoginRequest};
    let world = production_policy_world().await;
    let (issuer, task) = OidcFixture::start().await;
    let fixture = issuer.config(None);
    let auth = Arc::new(world.auth);
    let app = published_app(&fixture, &auth);
    let (prefs, csrf) = login_page_prefs(&app).await;
    let (_, _, html) = send(&app, get("/erp/login", Some(&prefs))).await;
    assert!(html.contains("Fixture Identity"));
    let regular = base_group(&world.h, "group_user").await;
    let system = base_group(&world.h, "group_system").await;
    let user = user_in_groups(
        &world.h,
        "person@example.test",
        "fixture-password",
        &[regular],
    )
    .await;
    let mut tx = world.h.begin().await.unwrap();
    tx.env()
        .records("res.users")
        .unwrap()
        .browse([user])
        .unwrap()
        .write_values(
            &mut tx,
            &std::collections::BTreeMap::from([(
                "email".to_owned(),
                support::s("person@example.test"),
            )]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let admin = user_in_groups(
        &world.h,
        "oidc-admin",
        "fixture-password",
        &[regular, system],
    )
    .await;
    let password_login = |login: &str| {
        auth.authenticate(LoginRequest::new(
            login,
            "fixture-password",
            ClientAddr::Known("127.0.0.2".parse().unwrap()),
        ))
    };
    let user_login = password_login("person@example.test").await.unwrap();
    let admin_login = password_login("oidc-admin").await.unwrap();
    assert_eq!(admin_login.uid().get(), admin);
    let with_native = format!(
        "{prefs}; session_id=test.{}",
        user_login.session().secret().to_cookie().as_str()
    );
    let with_admin = format!(
        "{prefs}; session_id=test.{}",
        admin_login.session().secret().to_cookie().as_str()
    );

    let unlinked = oidc_start(&app, &prefs, &csrf, false).await;
    let callback = issuer.callback(
        &unlinked,
        "unlinked",
        "linked-subject",
        "fixture-mfa",
        "person@example.test",
    );
    let (_, headers, _) = send(&app, get(&callback, Some(&prefs))).await;
    assert!(oauth_cookie(&headers).is_none());
    assert_eq!(headers["location"], "/erp/login?external_error=1");
    let (_, _, body) = send(&app, get("/erp/login?external_error=1", Some(&prefs))).await;
    assert!(body.contains("External sign-in was refused"));

    let wrong_account = oidc_start(&app, &with_native, &csrf, true).await;
    let callback = issuer.callback(
        &wrong_account,
        "wrong-account",
        "linked-subject",
        "fixture-mfa",
        "person@example.test",
    );
    let (_, headers, _) = send(&app, get(&callback, Some(&with_admin))).await;
    assert!(oauth_cookie(&headers).is_none());

    let linking = oidc_start(&app, &with_native, &csrf, true).await;
    let callback = issuer.callback(
        &linking,
        "linking",
        "linked-subject",
        "fixture-mfa",
        "person@example.test",
    );
    let (status, _, _) = send(&app, get(&callback, Some(&with_native))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let weak = oidc_start(&app, &prefs, &csrf, false).await;
    let callback = issuer.callback(
        &weak,
        "weak-factor",
        "linked-subject",
        "password",
        "person@example.test",
    );
    let (_, headers, _) = send(&app, get(&callback, Some(&prefs))).await;
    assert!(oauth_cookie(&headers).is_none());

    let authenticating = oidc_start(&app, &prefs, &csrf, false).await;
    let callback = issuer.callback(
        &authenticating,
        "authenticate",
        "linked-subject",
        "fixture-mfa",
        "person@example.test",
    );
    let (other_prefs, _) = login_page_prefs(&app).await;
    let (_, headers, _) = send(&app, get(&callback, Some(&other_prefs))).await;
    assert!(oauth_cookie(&headers).is_none());
    let (status, headers, _) = send(&app, get(&callback, Some(&prefs))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let external = oauth_cookie(&headers).unwrap();
    let session =
        erp_security::auth::SessionSecret::from_cookie(external.split_once('.').unwrap().1)
            .unwrap();
    let mut begun = auth
        .begin_session(&session, erp_security::auth::CredentialOptions::new())
        .await
        .unwrap();
    assert_eq!(begun.session.user().get(), user);
    begun.tx.rollback().await.unwrap();
    let (_, headers, _) = send(&app, get(&callback, Some(&prefs))).await;
    assert!(oauth_cookie(&headers).is_none());

    let (status, _, _) = send(
        &app,
        oidc_post(
            "/erp/session/external/revoke",
            &with_native,
            format!("csrf_token={}", form_value(&csrf)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        auth.begin_session(&session, erp_security::auth::CredentialOptions::new())
            .await
            .is_err()
    );
    let revoked = oidc_start(&app, &prefs, &csrf, false).await;
    let callback = issuer.callback(
        &revoked,
        "revoked",
        "linked-subject",
        "fixture-mfa",
        "person@example.test",
    );
    let (_, headers, _) = send(&app, get(&callback, Some(&prefs))).await;
    assert!(oauth_cookie(&headers).is_none());

    let policy = format!("csrf_token={}&enabled=false", form_value(&csrf));
    let (status, _, _) = send(
        &app,
        oidc_post(
            "/erp/session/external/provider",
            &with_native,
            policy.clone(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let relink = oidc_start(&app, &with_native, &csrf, true).await;
    let callback = issuer.callback(
        &relink,
        "relink",
        "linked-subject",
        "fixture-mfa",
        "person@example.test",
    );
    let (status, headers, _) = send(&app, get(&callback, Some(&with_native))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let external = oauth_cookie(&headers).unwrap();
    let session =
        erp_security::auth::SessionSecret::from_cookie(external.split_once('.').unwrap().1)
            .unwrap();
    let pending = oidc_start(&app, &prefs, &csrf, false).await;
    let callback = issuer.callback(
        &pending,
        "disabled",
        "linked-subject",
        "fixture-mfa",
        "person@example.test",
    );
    let (status, _, _) = send(
        &app,
        oidc_post("/erp/session/external/provider", &with_admin, policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        auth.begin_session(&session, erp_security::auth::CredentialOptions::new())
            .await
            .is_err()
    );
    let exchanges = issuer.exchanges.load(std::sync::atomic::Ordering::Relaxed);
    let (_, headers, _) = send(&app, get(&callback, Some(&prefs))).await;
    assert!(oauth_cookie(&headers).is_none());
    assert_eq!(
        issuer.exchanges.load(std::sync::atomic::Ordering::Relaxed),
        exchanges
    );
    let (_, _, html) = send(&app, get("/erp/login", Some(&prefs))).await;
    assert!(!html.contains("Fixture Identity"));
    let (status, _, _) = send(
        &app,
        oidc_post(
            "/erp/session/external/provider",
            &with_admin,
            format!("csrf_token={}&enabled=true", form_value(&csrf)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        auth.begin_session(&session, erp_security::auth::CredentialOptions::new())
            .await
            .is_err()
    );
    let mut tx = world.h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO ir_config_parameter (key,value) VALUES ('database.neutralized','true')",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (_, _, html) = send(&app, get("/erp/login", Some(&prefs))).await;
    assert!(!html.contains("Fixture Identity"));
    let (status, headers, _) = send(
        &app,
        oidc_post(
            "/erp/login/external",
            &prefs,
            format!("csrf_token={}", form_value(&csrf)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("location"));
    task.abort();
    world.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_web_login_provisions_only_the_configured_portal_company() {
    let world = production_policy_world().await;
    let portal = base_group(&world.h, "group_portal").await;
    let mut setup = world.h.begin().await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut setup)
        .await
        .unwrap();
    let rows = setup
        .query(
            "INSERT INTO res_company (tz, name, currency_id, active) VALUES
                ('UTC', 'Unrelated company', $1, true), ('UTC', 'OIDC portal company', $1, true)
            RETURNING name, id::int8",
            &[&currency],
        )
        .await
        .unwrap();
    let company: i64 = rows
        .iter()
        .find(|row| row.get::<_, &str>(0) == "OIDC portal company")
        .unwrap()
        .get(1);
    setup.commit().await.unwrap();
    let (issuer, task) = OidcFixture::start().await;
    let fixture = issuer.config(Some(company));
    let auth = Arc::new(world.auth);
    let app = published_app(&fixture, &auth);
    let (prefs, csrf) = login_page_prefs(&app).await;
    let rejected = oidc_start(&app, &prefs, &csrf, false).await;
    let callback = issuer.callback(
        &rejected,
        "wrong-domain",
        "outsider",
        "fixture-mfa",
        "outsider@other.test",
    );
    let (_, headers, _) = send(&app, get(&callback, Some(&prefs))).await;
    assert!(oauth_cookie(&headers).is_none());

    let pending = oidc_start(&app, &prefs, &csrf, false).await;
    let callback = issuer.callback(
        &pending,
        "provision",
        "new-subject",
        "fixture-mfa",
        "portal@example.test",
    );
    let (status, headers, body) = send(&app, get(&callback, Some(&prefs))).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(headers["location"], "/erp/login_successful");
    let external = oauth_cookie(&headers).unwrap();
    let secret =
        erp_security::auth::SessionSecret::from_cookie(external.split_once('.').unwrap().1)
            .unwrap();
    let mut begun = auth
        .begin_session(&secret, erp_security::auth::CredentialOptions::new())
        .await
        .unwrap();
    let uid = begun.session.user().get();
    begun.tx.rollback().await.unwrap();
    let mut tx = world.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT company_id, share, COALESCE(password,'')='',
                ARRAY(SELECT cid::int8 FROM res_company_users_rel WHERE user_id=u.id ORDER BY cid),
                ARRAY(SELECT gid::int8 FROM res_groups_users_rel WHERE uid=u.id ORDER BY gid)
            FROM res_users u WHERE id=$1",
            &[&uid],
        )
        .await
        .unwrap();
    assert_eq!(
        (
            rows[0].get::<_, i64>(0),
            rows[0].get::<_, bool>(1),
            rows[0].get::<_, bool>(2),
            rows[0].get::<_, Vec<i64>>(3),
            rows[0].get::<_, Vec<i64>>(4),
        ),
        (company, true, true, vec![company], vec![portal]),
    );
    let rows = tx
        .query(
            "SELECT subject, user_id FROM erp_external_identity WHERE issuer=$1",
            &[&issuer.issuer],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "new-subject");
    assert_eq!(rows[0].get::<_, i64>(1), uid);
    tx.rollback().await.unwrap();
    task.abort();
    world.h.cleanup().await.unwrap();
}
