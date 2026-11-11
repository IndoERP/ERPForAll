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
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in [
        ("runtime", "postgres://runtime:dummy@127.0.0.1/test"),
        ("owner", "postgres://owner:dummy@127.0.0.1/test"),
        (
            "proxy",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
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
    let assembly_before = erp_addon_web::assets::assembly::assembly_calls();
    assert!(assembly_before < u64::MAX);
    let config = Arc::new(ServerConfig::load(&dir.path().join("server.toml")).unwrap());
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
            blob.gzip()
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
