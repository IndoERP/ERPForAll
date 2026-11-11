//! Transport publication boundaries with real Auth, signals and the actual WebApp router.
//! Controlled adapters isolate HTTP publication semantics; these are not native engine evidence.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "auth_support/mod.rs"]
mod support;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use erp_fields::Error;
use erp_orm::{BoxFuture, Changes, Orm, Refresh, Refreshed, Signal, StaticPolicy};
use erp_security::auth::{Auth, AuthConfig, CredentialOptions, SessionSecret};
use erp_server::{
    config::ServerConfig,
    web::{AssetAdmission, LifecycleFailure, Publication, PublicationStage, WebApp, WebLifecycle},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;
use tower::ServiceExt;
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const A: &str = "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";
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
fn rpc(auth: &Auth) -> Arc<erp_rpc::RpcService> {
    Arc::new(erp_rpc::RpcService::new(
        auth.orm().clone(),
        Host,
        erp_rpc::RpcConfig::default(),
    ))
}
fn assets(hash: &str, bytes: &[u8]) -> AssetAdmission {
    assets_in(&asset_store(), hash, bytes)
}
fn assets_in(
    store: &Arc<erp_server::assets::AssetStore>,
    hash: &str,
    bytes: &[u8],
) -> AssetAdmission {
    AssetAdmission::Published(Arc::new(
        erp_server::assets::AssetGeneration::new("elang-1", hash.to_owned())
            .unwrap()
            .asset(
                erp_server::assets::AssetSource::new(
                    &format!("{hash}.js"),
                    "text/javascript; charset=utf-8",
                    bytes.to_vec(),
                )
                .unwrap(),
            )
            .unwrap()
            .build(store)
            .unwrap(),
    ))
}
fn config() -> (tempfile::TempDir, Arc<ServerConfig>) {
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in [
        ("runtime", "postgres://runtime:dummy@127.0.0.1/test"),
        ("owner", "postgres://owner:dummy@127.0.0.1/test"),
        ("proxy", TOKEN),
    ] {
        fs::write(dir.path().join(file), text).unwrap();
        fs::set_permissions(dir.path().join(file), fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(dir.path().join("server.toml"),"database='test'\ndatabase_url_file='runtime'\nowner_database_url_file='owner'\nbind='127.0.0.1:0'\npublic_origin='https://erp.example.test'\nproxy_mode='authenticated'\ntrusted_proxies=['127.0.0.1']\nproxy_token_file='proxy'\naddons_path=['addons']\nassets_manifest='manifest.json'\n").unwrap();
    let entries:Vec<_>=[("js","text/javascript; charset=utf-8"),("css","text/css; charset=utf-8")].into_iter().map(|(ext,mime)|{
        let path=format!("client.{EMPTY}.{ext}");fs::write(dir.path().join(&path),b"").unwrap();
        serde_json::json!({"name":format!("client.{ext}"),"path":path,"sha256":EMPTY,"content_type":mime})
    }).collect();
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema":1,"assets":entries})).unwrap(),
    )
    .unwrap();
    let config = Arc::new(ServerConfig::load(&dir.path().join("server.toml")).unwrap());
    (dir, config)
}
struct Supersede;
impl Refresh for Supersede {
    fn refresh<'a>(&'a self, _: &'a Changes) -> BoxFuture<'a, erp_fields::Result<Refreshed>> {
        Box::pin(async { Ok(Refreshed::Superseded) })
    }
}
async fn node(world: &support::World, supersede: bool) -> Arc<Auth> {
    let client = world.h.db().client().await.unwrap();
    let baseline = Signal::read(&client).await.unwrap();
    let signal = if supersede {
        Signal::new(baseline).with_refresh(Supersede)
    } else {
        Signal::new(baseline)
    };
    let registry = Arc::new(
        erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
            .with_decls([&erp_data::model_data::IR_MODEL_DATA])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, support::pool_of(&world.h, 4))
        .policy(StaticPolicy::AllowAll)
        .signal(signal)
        .trust_caller_resolutions()
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    Arc::new(
        Auth::new(orm, AuthConfig::DEFAULT, world.pool)
            .await
            .unwrap(),
    )
}
async fn world() -> support::World {
    let world = support::world().await;
    support::user_with_password(&world.h, "publication-user", "publication-password").await;
    let mut tx = world.h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    world
}
async fn cookie(auth: &Auth) -> String {
    let login = support::login(auth, "publication-user", "publication-password")
        .await
        .unwrap();
    format!(
        "session_id=test.{}",
        login.session().secret().to_cookie().as_str()
    )
}
fn request(cookie: &str, module: &str) -> Request<Body> {
    request_to("/erp/module/upgrade", cookie, module)
}
fn request_to(path: &str, cookie: &str, module: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("x-erp-proxy-token", TOKEN)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .header("accept", "text/html")
        .header("cookie", cookie)
        .body(Body::from(if path == "/erp/module/uninstall" {
            serde_json::json!({"modules":[module],"impact":format!("uninstall-v1:{}", "a".repeat(64))}).to_string()
        } else {
            serde_json::json!({"modules":[module]}).to_string()
        }))
        .unwrap()
}
struct FailureAdapter {
    pool: erp_testkit::deadpool_postgres::Pool,
    stage: Option<PublicationStage>,
    refreshes: AtomicUsize,
    upgrades: AtomicUsize,
}
impl WebLifecycle for FailureAdapter {
    fn refresh<'a>(&'a self, _: &'a Arc<Auth>) -> BoxFuture<'a, erp_fields::Result<Publication>> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(Error::programming(
                "Unexpected refresh after terminal lifecycle outcome",
            ))
        })
    }
    fn install<'a>(
        &'a self,
        _: &'a Arc<Auth>,
        _: &'a SessionSecret,
        _: Vec<String>,
    ) -> BoxFuture<'a, erp_fields::Result<Publication>> {
        Box::pin(async { Err(Error::validation("Not an install fixture")) })
    }
    fn uninstall<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a SessionSecret,
        modules: Vec<String>,
        impact: String,
    ) -> BoxFuture<'a, Result<Publication, LifecycleFailure>> {
        assert_eq!(impact, format!("uninstall-v1:{}", "a".repeat(64)));
        self.upgrade(auth, secret, modules)
    }
    fn upgrade<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a SessionSecret,
        _: Vec<String>,
    ) -> BoxFuture<'a, Result<Publication, LifecycleFailure>> {
        Box::pin(async move {
            self.upgrades.fetch_add(1, Ordering::SeqCst);
            self.pool
                .get()
                .await
                .unwrap()
                .execute(
                    "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
                    &[],
                )
                .await
                .unwrap();
            let error = match auth.begin_session(secret, CredentialOptions::new()).await {
                Err(error) => error,
                Ok(_) => panic!("fixture failed to supersede request ORM"),
            };
            assert!(error.is_superseded(), "{error}");
            assert_eq!(error.kind(), erp_fields::ErrorKind::Concurrency);
            assert_eq!(
                error.message(),
                "the registry changed in another process and was reloaded: this Orm is superseded; retry on the current one"
            );
            let diagnostic = Error::programming("private fixture diagnostic");
            Err(match self.stage {
                Some(stage) => LifecycleFailure::Committed {
                    stage,
                    error: diagnostic,
                },
                None => LifecycleFailure::Unknown(diagnostic),
            })
        })
    }
}
#[tokio::test]
async fn superseded_dispatch_keeps_terminal_lifecycle_truth() {
    let world = world().await;
    for (path, operation, stage) in [
        (
            "/erp/module/upgrade",
            "Upgrade",
            Some(PublicationStage::RuntimeGrants),
        ),
        ("/erp/module/upgrade", "Upgrade", None),
        (
            "/erp/module/uninstall",
            "Uninstall",
            Some(PublicationStage::PostCommit),
        ),
        (
            "/erp/module/uninstall",
            "Uninstall",
            Some(PublicationStage::RuntimeGrants),
        ),
        (
            "/erp/module/uninstall",
            "Uninstall",
            Some(PublicationStage::Recomposition),
        ),
        (
            "/erp/module/uninstall",
            "Uninstall",
            Some(PublicationStage::Assets),
        ),
        (
            "/erp/module/uninstall",
            "Uninstall",
            Some(PublicationStage::Publication),
        ),
        ("/erp/module/uninstall", "Uninstall", None),
    ] {
        let auth = node(&world, true).await;
        let cookie = cookie(&auth).await;
        let (_dir, config) = config();
        let adapter = Arc::new(FailureAdapter {
            pool: support::pool_of(&world.h, 2),
            stage,
            refreshes: AtomicUsize::new(0),
            upgrades: AtomicUsize::new(0),
        });
        let app = Arc::new(
            WebApp::new_with_store(config, Arc::clone(&auth), asset_store())
                .unwrap()
                .with_rpc(rpc(&auth))
                .with_lifecycle(adapter.clone()),
        );
        let response = app
            .router("127.0.0.1".parse().unwrap())
            .oneshot(request_to(path, &cookie, "notes"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["content-type"], "application/json");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        if let Some(stage) = stage {
            let (stage, detail) = match stage {
                PublicationStage::PostCommit => ("post_commit", "post-commit completion"),
                PublicationStage::RuntimeGrants => ("runtime_grants", "runtime grant publication"),
                PublicationStage::Recomposition => ("recomposition", "runtime recomposition"),
                PublicationStage::Assets => ("assets", "asset publication"),
                PublicationStage::Publication => ("publication", "service publication"),
                other => panic!("unexpected fixture stage {other:?}"),
            };
            assert_eq!(
                body,
                serde_json::json!({"name":"erp.lifecycle.committed","message":format!("{operation} committed but {detail} is incomplete; operator recovery is required"),"outcome":"committed","stage":stage})
            );
        } else {
            assert_eq!(
                body,
                serde_json::json!({"name":"erp.lifecycle.unknown","message":format!("{operation} outcome is unknown; reconcile addon state before any retry"),"outcome":"unknown","stage":null})
            );
        }
        assert_eq!(adapter.refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.upgrades.load(Ordering::SeqCst), 1);
    }
    world.h.cleanup().await.unwrap();
}
struct PublicationAdapter {
    older: Arc<Auth>,
    newer: Arc<Auth>,
    entered: Notify,
    release: Notify,
}
impl WebLifecycle for PublicationAdapter {
    fn refresh<'a>(&'a self, _: &'a Arc<Auth>) -> BoxFuture<'a, erp_fields::Result<Publication>> {
        Box::pin(async { Err(Error::programming("Unexpected fixture refresh")) })
    }
    fn install<'a>(
        &'a self,
        _: &'a Arc<Auth>,
        _: &'a SessionSecret,
        modules: Vec<String>,
    ) -> BoxFuture<'a, erp_fields::Result<Publication>> {
        Box::pin(async move {
            let name = modules[0].clone();
            if name == "older" {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let auth = if name == "older" {
                Arc::clone(&self.older)
            } else {
                Arc::clone(&self.newer)
            };
            let publication = Publication::new(Arc::clone(&auth), rpc(&auth), modules);
            Ok(match name.as_str() {
                "older" => publication.with_assets(assets(EMPTY, b"")),
                "newer" => publication.with_assets(assets(A, b"a")),
                "foreign" => publication.with_assets(assets_in(
                    &erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap(),
                    A,
                    b"a",
                )),
                _ => panic!("unknown fixture operation"),
            })
        })
    }
    fn uninstall<'a>(
        &'a self,
        _: &'a Arc<Auth>,
        _: &'a SessionSecret,
        modules: Vec<String>,
        impact: String,
    ) -> BoxFuture<'a, Result<Publication, LifecycleFailure>> {
        Box::pin(async move {
            assert_eq!(modules, ["notes"]);
            assert_eq!(impact, format!("uninstall-v1:{}", "a".repeat(64)));
            let auth = Arc::clone(&self.newer);
            Ok(Publication::uninstall(
                Arc::clone(&auth),
                rpc(&auth),
                vec!["notes".to_owned(), "notes_extension".to_owned()],
            )
            .with_assets(assets(A, b"a")))
        })
    }
    fn upgrade<'a>(
        &'a self,
        _: &'a Arc<Auth>,
        _: &'a SessionSecret,
        modules: Vec<String>,
    ) -> BoxFuture<'a, Result<Publication, LifecycleFailure>> {
        Box::pin(async move {
            let name = modules[0].clone();
            if name == "older" {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let auth = if name == "older" {
                Arc::clone(&self.older)
            } else {
                Arc::clone(&self.newer)
            };
            let publication =
                Publication::upgrade(Arc::clone(&auth), rpc(&auth), modules, Vec::new());
            Ok(match name.as_str() {
                "missing" => publication,
                "broken" => {
                    publication.with_assets(AssetAdmission::Broken("private broken assets".into()))
                }
                "older" => publication.with_assets(assets(EMPTY, b"")),
                "newer" => publication.with_assets(assets(A, b"a")),
                "foreign" => publication.with_assets(assets_in(
                    &erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap(),
                    A,
                    b"a",
                )),
                _ => panic!("unknown fixture operation"),
            })
        })
    }
}
#[tokio::test]
async fn uninstall_publication_reports_removed_dependents() {
    let world = world().await;
    let auth = node(&world, false).await;
    let cookie = cookie(&auth).await;
    let (_dir, config) = config();
    let adapter = Arc::new(PublicationAdapter {
        older: Arc::clone(&auth),
        newer: Arc::clone(&auth),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let app = Arc::new(
        WebApp::new_with_store(config, Arc::clone(&auth), asset_store())
            .unwrap()
            .with_rpc(rpc(&auth))
            .with_lifecycle(adapter),
    );
    let response = app
        .router("127.0.0.1".parse().unwrap())
        .oneshot(request_to("/erp/module/uninstall", &cookie, "notes"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(
        body["uninstalled"],
        serde_json::json!(["notes", "notes_extension"])
    );
    assert!(body.get("installed").is_none());
    assert!(body.get("upgraded").is_none());
    assert!(body["registry_version"].is_string());
    assert!(body["asset_version"].is_string());
    world.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn upgrade_asset_admission_and_monotonic_publication() {
    let world = world().await;
    let client = world.h.db().client().await.unwrap();
    client
        .execute(
            "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();
    let older = node(&world, false).await;
    client
        .execute(
            "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();
    let newer = node(&world, false).await;
    assert_ne!(
        older.orm().registry().generation(),
        newer.orm().registry().generation(),
        "fixture requires genuinely distinct registry versions"
    );
    assert!(
        older
            .orm()
            .signal()
            .unwrap()
            .applied()
            .get(&erp_orm::SignalKey::Registry)
            < newer
                .orm()
                .signal()
                .unwrap()
                .applied()
                .get(&erp_orm::SignalKey::Registry)
    );
    let cookie = cookie(&newer).await;
    let (_dir, config) = config();
    let adapter = Arc::new(PublicationAdapter {
        older,
        newer: Arc::clone(&newer),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let app = Arc::new(
        WebApp::new_with_store(config, Arc::clone(&newer), asset_store())
            .unwrap()
            .with_rpc(rpc(&newer))
            .with_assets("elang-1", assets(A, b"a"))
            .unwrap()
            .with_lifecycle(adapter.clone()),
    );
    for (module, stage, message) in [
        (
            "missing",
            "publication",
            "Upgrade committed but service publication is incomplete; operator recovery is required",
        ),
        (
            "broken",
            "assets",
            "Upgrade committed but asset publication is incomplete; operator recovery is required",
        ),
    ] {
        let response = app
            .router("127.0.0.1".parse().unwrap())
            .oneshot(request(&cookie, module))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"name":"erp.lifecycle.committed","outcome":"committed","stage":stage,"message":message})
        );
    }
    let router = app.router("127.0.0.1".parse().unwrap());
    let old_request = request(&cookie, "older");
    let slow = tokio::spawn(async move { router.oneshot(old_request).await.unwrap() });
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        adapter.entered.notified(),
    )
    .await
    .unwrap();
    let newer_response = app
        .router("127.0.0.1".parse().unwrap())
        .oneshot(request(&cookie, "newer"))
        .await
        .unwrap();
    assert_eq!(newer_response.status(), StatusCode::OK);
    let newer_result: serde_json::Value =
        serde_json::from_slice(&to_bytes(newer_response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(newer_result["upgraded"], serde_json::json!(["newer"]));
    assert_eq!(newer_result["installed"], serde_json::json!([]));
    adapter.release.notify_one();
    let older_response = slow.await.unwrap();
    assert_eq!(older_response.status(), StatusCode::OK);
    let older_result: serde_json::Value =
        serde_json::from_slice(&to_bytes(older_response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(older_result["upgraded"], serde_json::json!(["older"]));
    assert_eq!(older_result["installed"], serde_json::json!([]));
    assert_eq!(
        older_result["registry_version"],
        newer_result["registry_version"]
    );
    let response = app
        .router("127.0.0.1".parse().unwrap())
        .oneshot(
            Request::builder()
                .uri(format!("/erp/assets/{A}.js"))
                .header("x-erp-proxy-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 4096).await.unwrap().as_ref(),
        b"a"
    );
    drop(client);
    world.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn older_healthy_upgrade_refuses_retained_newer_broken_assets() {
    retained_newer_broken_assets_refuse("/erp/module/upgrade", "Upgrade").await;
}

#[tokio::test]
async fn older_healthy_install_refuses_retained_newer_broken_assets() {
    retained_newer_broken_assets_refuse("/erp/module/install", "Install").await;
}

async fn retained_newer_broken_assets_refuse(path: &str, operation: &str) {
    let path = path.to_owned();
    let world = world().await;
    let client = world.h.db().client().await.unwrap();
    client
        .execute(
            "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();
    let older = node(&world, false).await;
    client
        .execute(
            "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();
    let newer = node(&world, false).await;
    assert_ne!(
        older.orm().registry().generation(),
        newer.orm().registry().generation()
    );
    assert!(
        older
            .orm()
            .signal()
            .unwrap()
            .applied()
            .get(&erp_orm::SignalKey::Registry)
            < newer
                .orm()
                .signal()
                .unwrap()
                .applied()
                .get(&erp_orm::SignalKey::Registry)
    );
    let cookie = cookie(&newer).await;
    let (_dir, config) = config();
    let adapter = Arc::new(PublicationAdapter {
        older,
        newer: Arc::clone(&newer),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let app = Arc::new(
        WebApp::new_with_store(config, Arc::clone(&newer), asset_store())
            .unwrap()
            .with_rpc(rpc(&newer))
            .with_assets(
                "elang-1",
                AssetAdmission::Broken("newer maintenance fixture".into()),
            )
            .unwrap()
            .with_lifecycle(adapter.clone()),
    );
    let router = app.router("127.0.0.1".parse().unwrap());
    let slow = tokio::spawn(async move {
        router
            .oneshot(request_to(&path, &cookie, "older"))
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        adapter.entered.notified(),
    )
    .await
    .unwrap();
    adapter.release.notify_one();
    let response = slow.await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"name":"erp.lifecycle.committed","outcome":"committed","stage":"assets","message":format!("{operation} committed but asset publication is incomplete; operator recovery is required")})
    );
    // The rejected older result must not replace the newer maintenance publication.
    let response = app
        .router("127.0.0.1".parse().unwrap())
        .oneshot(
            Request::builder()
                .uri(format!("/erp/assets/{EMPTY}.js"))
                .header("x-erp-proxy-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(client);
    world.h.cleanup().await.unwrap();
}

fn asset_store() -> Arc<erp_server::assets::AssetStore> {
    static STORE: std::sync::OnceLock<Arc<erp_server::assets::AssetStore>> =
        std::sync::OnceLock::new();
    Arc::clone(
        STORE.get_or_init(|| erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap()),
    )
}

#[tokio::test]
async fn lifecycle_without_initial_assets_refuses_a_foreign_resident_allowance() {
    let world = world().await;
    let auth = node(&world, true).await;
    let cookie = cookie(&auth).await;
    let (_dir, config) = config();
    let adapter = Arc::new(PublicationAdapter {
        older: Arc::clone(&auth),
        newer: Arc::clone(&auth),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let app = Arc::new(
        WebApp::new_with_store(config, Arc::clone(&auth), asset_store())
            .unwrap()
            .with_rpc(rpc(&auth))
            .with_lifecycle(adapter),
    );
    let reply = app
        .router("127.0.0.1".parse().unwrap())
        .oneshot(request(&cookie, "foreign"))
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(reply.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"name":"erp.lifecycle.committed","outcome":"committed","stage":"publication","message":"Upgrade committed but service publication is incomplete; operator recovery is required"})
    );
}

struct ForeignRefreshAdapter {
    newer: Arc<Auth>,
    assets: Arc<erp_server::assets::PublishedAssets>,
    refreshes: AtomicUsize,
}
impl WebLifecycle for ForeignRefreshAdapter {
    fn refresh<'a>(&'a self, _: &'a Arc<Auth>) -> BoxFuture<'a, erp_fields::Result<Publication>> {
        Box::pin(async move {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(
                Publication::new(Arc::clone(&self.newer), rpc(&self.newer), Vec::new())
                    .with_assets(AssetAdmission::Published(Arc::clone(&self.assets))),
            )
        })
    }
    fn install<'a>(
        &'a self,
        _: &'a Arc<Auth>,
        _: &'a SessionSecret,
        _: Vec<String>,
    ) -> BoxFuture<'a, erp_fields::Result<Publication>> {
        Box::pin(async { Err(Error::validation("Not an install fixture")) })
    }
    fn uninstall<'a>(
        &'a self,
        _: &'a Arc<Auth>,
        _: &'a SessionSecret,
        _: Vec<String>,
        _: String,
    ) -> BoxFuture<'a, Result<Publication, LifecycleFailure>> {
        Box::pin(async {
            Err(LifecycleFailure::Refused(Error::validation(
                "Not an uninstall fixture",
            )))
        })
    }
    fn upgrade<'a>(
        &'a self,
        _: &'a Arc<Auth>,
        _: &'a SessionSecret,
        _: Vec<String>,
    ) -> BoxFuture<'a, Result<Publication, LifecycleFailure>> {
        Box::pin(async {
            Err(LifecycleFailure::Refused(Error::validation(
                "Not an upgrade fixture",
            )))
        })
    }
}

#[tokio::test]
async fn refresh_without_initial_assets_cannot_publish_a_foreign_fixed_path() {
    let world = world().await;
    let older = node(&world, true).await;
    let client = world.h.db().client().await.unwrap();
    client
        .execute(
            "INSERT INTO erp_signal_generation(key) VALUES ('registry')",
            &[],
        )
        .await
        .unwrap();
    let newer = node(&world, false).await;
    assert_ne!(
        older.orm().registry().generation(),
        newer.orm().registry().generation()
    );
    // ORM transactions open lazily: the first statement performs the signal admission.
    let mut stale = older.orm().begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let error = stale.query("SELECT 1", &[]).await.unwrap_err();
    drop(stale);
    assert!(error.is_superseded());
    assert_eq!(error.kind(), erp_fields::ErrorKind::Concurrency);
    assert_eq!(
        error.message(),
        "the registry changed in another process and was reloaded: this Orm is superseded; retry on the current one"
    );
    assert!(older.orm().signal().unwrap().is_superseded());
    let foreign_store = erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap();
    let foreign_assets = Arc::new(
        erp_server::assets::AssetGeneration::new("elang-1", A.to_owned())
            .unwrap()
            .asset(
                erp_server::assets::AssetSource::new(
                    &format!("{A}.js"),
                    "text/javascript; charset=utf-8",
                    b"a".to_vec(),
                )
                .unwrap(),
            )
            .unwrap()
            .static_file("/foreign/static/escape.js", &format!("/erp/assets/{A}.js"))
            .unwrap()
            .build(&foreign_store)
            .unwrap(),
    );
    let adapter = Arc::new(ForeignRefreshAdapter {
        newer,
        assets: foreign_assets,
        refreshes: AtomicUsize::new(0),
    });
    let (_dir, config) = config();
    let app = Arc::new(
        WebApp::new_with_store(config, Arc::clone(&older), asset_store())
            .unwrap()
            .with_rpc(rpc(&older))
            .with_lifecycle(adapter.clone()),
    );
    // Without initial with_assets there is no expected runtime. The first safe GET drives
    // refresh on the superseded Auth; the second detects if that refresh improperly published
    // the foreign store and its fixed declaration. Neither request runs a business mutation.
    let mut statuses = Vec::new();
    let mut bodies = Vec::new();
    for _ in 0..2 {
        let reply = app
            .router("127.0.0.1".parse().unwrap())
            .oneshot(
                Request::builder()
                    .uri("/foreign/static/escape.js")
                    .header("x-erp-proxy-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        statuses.push(reply.status());
        bodies.push(to_bytes(reply.into_body(), 16_384).await.unwrap());
    }
    assert_eq!(statuses, [StatusCode::NOT_FOUND, StatusCode::NOT_FOUND]);
    for body in bodies {
        assert_ne!(body.as_ref(), b"a");
    }
    assert_eq!(adapter.refreshes.load(Ordering::SeqCst), 2);
    assert!(Arc::ptr_eq(&app.current_auth().await, &older));
    drop(client);
    world.h.cleanup().await.unwrap();
}
