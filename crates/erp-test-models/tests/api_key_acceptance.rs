// SPDX-License-Identifier: MIT OR Apache-2.0
//! Whole-case acceptance of native API keys: one test per spec case that
//! walks the complete case, the remaining refusal pins, and functional tests with two nodes over
//! one database. Ordering between nodes comes only from explicit transaction boundaries.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod auth_support;
use auth_support::*;

use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, Response, StatusCode},
};
use erp_fields::{DateTime, ErrorKind};
use erp_orm::{
    ApiKeyId, BeginOptions, OperationIdentity, PrincipalBuilder, Scope, Transaction, UserId,
};
use erp_sdk::Value;
use erp_security::auth::{
    ApiKeyRequest, Auth, AuthConfig, CredentialOptions, HashPoolConfig,
    apikey::{ApiKeyHasher, NativeApiKey},
    apikey_manage::{collect_expired, generate, revoke},
    apikey_store::{IssueRequest, IssuedSecret, KeyKind, KeyScope, issue},
    secret::DatabaseSecret,
};
use erp_server::{config::ServerConfig, web::WebApp};
use tower::ServiceExt;

const GENERATE_INVALID: &str =
    "The provided API key is invalid or does not belong to the current user.";
const NOT_ENABLED: &str = "Programmatic API keys are not enabled";
const ONE_DAY: i64 = 86_400_000_000;
const ONE_HOUR: i64 = 3_600_000_000;

fn request(raw: &str, address: u8) -> ApiKeyRequest<'_> {
    ApiKeyRequest::new(
        raw,
        KeyScope::Rpc,
        format!("192.0.2.{address}").parse().unwrap(),
    )
}

/// The tenant's key hasher, derived from its stored `database.secret` as issuance does.
async fn hasher(h: &erp_testkit::Harness) -> ApiKeyHasher {
    let mut tx = h.begin().await.unwrap();
    let secret: String = tx
        .query(
            "SELECT value FROM ir_config_parameter WHERE key='database.secret'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    ApiKeyHasher::derive(&DatabaseSecret::from_stored(&secret).unwrap()).unwrap()
}

/// The transaction's database clock: expiry is judged against it, never an application clock.
async fn now(tx: &mut Transaction) -> DateTime {
    DateTime::from_micros(
        tx.query("SELECT (extract(epoch FROM now()) * 1000000)::int8", &[])
            .await
            .unwrap()[0]
            .get(0),
    )
}

/// The database clock moved by `offset` microseconds.
async fn from_now(h: &erp_testkit::Harness, offset: i64) -> DateTime {
    let mut tx = h.begin().await.unwrap();
    let clock = now(&mut tx).await;
    tx.rollback().await.unwrap();
    DateTime::from_micros(clock.micros() + offset)
}

/// A key for `subject`, issued by the administrator (a `base.group_system` member may issue
/// for another user), committed; returns the row id and the one-time secret.
async fn issue_for(w: &World, subject: i64, expiry: Option<DateTime>) -> (i64, IssuedSecret) {
    issue_as(w, w.admin, subject, expiry).await
}

async fn issue_own(w: &World, subject: i64, expiry: Option<DateTime>) -> (i64, IssuedSecret) {
    issue_as(w, subject, subject, expiry).await
}

async fn issue_as(
    w: &World,
    caller: i64,
    subject: i64,
    expiry: Option<DateTime>,
) -> (i64, IssuedSecret) {
    w.h.orm().check_audit_store().await.unwrap();
    let hasher = hasher(&w.h).await;
    let mut tx = w.h.begin_as(caller).await.unwrap();
    let issued = issue(
        &mut tx,
        &hasher,
        &IssueRequest::new(
            UserId::new(subject).unwrap(),
            "acceptance",
            KeyKind::User,
            KeyScope::Rpc,
            expiry,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let id = issued.id().get();
    tx.commit().await.unwrap();
    (id, issued.into_secret())
}

async fn sql(h: &erp_testkit::Harness, text: &str) {
    // A key's last-use write runs on its own connection after the request that used the key; a
    // statement here that touches the same row can fail to serialize while it is in flight. The
    // RPC layer retries that; this direct caller does the same, a bounded number of times.
    for attempt in 0..5 {
        let mut tx = h.begin().await.unwrap();
        match tx.batch(text).await {
            Err(ref error) if error.kind() == ErrorKind::Concurrency && attempt < 4 => {
                tx.rollback().await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => {
                other.unwrap();
                tx.commit().await.unwrap();
                return;
            }
        }
    }
}

async fn into_mcp_key(h: &erp_testkit::Harness, id: i64) {
    sql(
        h,
        &format!(
            "UPDATE res_users_apikeys SET kind='agent',capabilities='none',scope='mcp' WHERE id={id}"
        ),
    )
    .await;
}

async fn expire(h: &erp_testkit::Harness, id: i64) {
    sql(
        h,
        &format!(
            "UPDATE res_users_apikeys SET expiration_date=now() AT TIME ZONE 'utc' - interval '1 second' WHERE id={id}"
        ),
    )
    .await;
}

async fn key_ids(h: &erp_testkit::Harness) -> Vec<i64> {
    let mut tx = h.begin().await.unwrap();
    let ids = tx
        .query("SELECT id::int8 FROM res_users_apikeys ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    tx.rollback().await.unwrap();
    ids
}

async fn accepted(auth: &Auth, req: ApiKeyRequest<'_>) -> UserId {
    let mut tx = auth
        .begin_api_key(req, CredentialOptions::new())
        .await
        .unwrap()
        .tx;
    let subject = tx.env().principal().subject();
    tx.rollback().await.unwrap();
    subject
}

async fn refused(auth: &Auth, req: ApiKeyRequest<'_>, kind: ErrorKind, message: &str) {
    let e = auth
        .begin_api_key(req, CredentialOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!((e.kind(), e.message()), (kind, message));
}

fn refusal(e: erp_fields::Error, kind: ErrorKind, message: &str) {
    assert_eq!((e.kind(), e.message()), (kind, message));
}

/// A non-system user whose only group allows keys of at most 30 days.
async fn thirty_day_user(h: &erp_testkit::Harness, login: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let group = create(
        &mut tx,
        "res.groups",
        &[
            ("name", s("Thirty-day API keys")),
            ("api_key_duration", Value::Float(30.0)),
        ],
    )
    .await;
    let uid = user(&mut tx, login, &[group]).await;
    tx.commit().await.unwrap();
    uid
}

/// A world whose database carries the signal table, for tests with a second and third node.
async fn signal_world() -> World {
    let h = builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    world_with(h, AuthConfig::DEFAULT, HashPoolConfig::DEFAULT).await
}

/// An independent node over the world's database: its own ORM, pool and Auth, its audit store
/// checked as a server does at startup.
async fn node(w: &World) -> Auth {
    let auth = node_with(&w.h, 4, own_pool(HashPoolConfig::DEFAULT)).await;
    auth.orm().check_audit_store().await.unwrap();
    auth
}

/// A transaction on `auth`'s node as `principal`, with the operation identity the RPC layer
/// would attach, so its audited key events can commit.
async fn begin_on(auth: &Auth, principal: UserId) -> Transaction {
    auth.orm()
        .begin_opts(
            BeginOptions::new(principal).operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}

// ---- canonical JSON-2 over a node of the world's database ----

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

struct Json2 {
    router: Router,
    _dir: tempfile::TempDir,
}

/// The web application of `api_key_http.rs`, on its own node of `h`'s database.
async fn json2(h: &erp_testkit::Harness) -> Json2 {
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
    let auth = Arc::new(node_with(h, 4, own_pool(HashPoolConfig::DEFAULT)).await);
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
    Json2 {
        router: app.router("127.0.0.1".parse().unwrap()),
        _dir: dir,
    }
}

impl Json2 {
    async fn bearer(&self, key: &str, address: u8) -> (Response<Body>, Vec<u8>) {
        let request = Request::builder()
            .method("POST")
            .uri(PATH)
            .header("x-erp-proxy-token", PROXY_TOKEN)
            .header("x-forwarded-for", format!("192.0.2.{address}"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(BODY))
            .unwrap();
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

// ---- whole cases ----

#[tokio::test]
async fn typed_uid_and_key_check_accepts_only_an_active_owner_with_an_rpc_key() {
    let w = world().await;
    let uid = user_with_password(&w.h, "bearer", "pw").await;
    let expiry = from_now(&w.h, ONE_HOUR).await;
    let (_, k) = issue_for(&w, uid, Some(expiry)).await;
    // k2: a key of the same user whose stored scope is not `rpc`
    let (k2_id, k2) = issue_for(&w, uid, Some(expiry)).await;
    into_mcp_key(&w.h, k2_id).await;
    let owner = UserId::new(uid).unwrap();
    // (uid, k): success
    assert_eq!(
        accepted(&w.auth, request(k.expose(), 1).uid(owner)).await,
        owner
    );
    // (uid, k2): AccessDenied
    refused(
        &w.auth,
        request(k2.expose(), 2).uid(owner),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    // (uid, ''): AccessDenied
    refused(
        &w.auth,
        request("", 3).uid(owner),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    // an inactive user with a valid key: AccessDenied
    sql(
        &w.h,
        &format!("UPDATE res_users SET active=false WHERE id={uid}"),
    )
    .await;
    refused(
        &w.auth,
        request(k.expose(), 4).uid(owner),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
}

#[tokio::test]
async fn issuance_stores_a_digest_and_enforces_group_lifetime_then_expiry_and_collection() {
    // A shadowed `tx` keeps its pooled connection until the end of the test even after commit or
    // rollback, and the harness pool has 4: every transaction is dropped once it ended.
    let w = world().await;
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    let uid = thirty_day_user(&w.h, "thirty").await;
    let hasher = hasher(&w.h).await;
    w.h.orm().check_audit_store().await.unwrap();
    let key = |expiry| {
        IssueRequest::new(
            UserId::new(uid).unwrap(),
            "k",
            KeyKind::User,
            KeyScope::Rpc,
            expiry,
        )
        .unwrap()
    };
    // now+1d: the key is returned once (the handle is consumed by `into_secret`; it is neither
    // Clone nor printable) and stored as its keyed digest with its display prefix
    let mut tx = w.h.begin_as(uid).await.unwrap();
    let clock = now(&mut tx).await;
    let issued = issue(
        &mut tx,
        &hasher,
        &key(Some(DateTime::from_micros(clock.micros() + ONE_DAY))),
    )
    .await
    .unwrap();
    let id = issued.id().get();
    let prefix = issued.prefix().to_owned();
    tx.commit().await.unwrap();
    drop(tx);
    let secret = issued.into_secret();
    let parsed = NativeApiKey::parse(secret.expose()).unwrap();
    let expected = hasher.digest(&parsed);
    let mut tx = w.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT digest, digest_kid, prefix, legacy_hash, row_to_json(k)::text \
             FROM res_users_apikeys k WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap();
    let row = &rows[0];
    // `assert!`, not `assert_eq!`: a failure must not print the stored digest
    assert!(
        row.get::<_, Vec<u8>>(0) == expected.bytes(),
        "the stored digest is not the keyed digest of the issued key"
    );
    assert_eq!(row.get::<_, i32>(1), i32::from(expected.kid()));
    assert_eq!(row.get::<_, String>(2), parsed.display_prefix());
    assert_eq!(prefix, parsed.display_prefix());
    assert!(row.get::<_, Option<String>>(3).is_none());
    assert!(!row.get::<_, String>(4).contains(secret.expose()));
    tx.rollback().await.unwrap();
    drop(tx);
    // no date, a past date, now+60d
    let mut tx = w.h.begin_as(uid).await.unwrap();
    let clock = now(&mut tx).await;
    refusal(
        issue(&mut tx, &hasher, &key(None)).await.err().unwrap(),
        ErrorKind::Validation,
        "The API key must have an expiration date",
    );
    refusal(
        issue(
            &mut tx,
            &hasher,
            &key(Some(DateTime::from_micros(clock.micros() - ONE_HOUR))),
        )
        .await
        .err()
        .unwrap(),
        ErrorKind::Validation,
        "You cannot set an expiration date in the past.",
    );
    refusal(
        issue(
            &mut tx,
            &hasher,
            &key(Some(DateTime::from_micros(clock.micros() + 60 * ONE_DAY))),
        )
        .await
        .err()
        .unwrap(),
        ErrorKind::Validation,
        "You cannot exceed 30.0 days.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // a system user with no date: a persistent key
    let (persistent, _) = issue_for(&w, w.admin, None).await;
    let mut tx = w.h.begin().await.unwrap();
    let stored: Option<DateTime> = tx
        .query(
            "SELECT expiration_date FROM res_users_apikeys WHERE id=$1",
            &[&persistent],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, None);
    tx.rollback().await.unwrap();
    drop(tx);
    // the live key authenticates; once expired it stops, and collection deletes it
    assert_eq!(
        accepted(&w.auth, request(secret.expose(), 10)).await,
        UserId::new(uid).unwrap()
    );
    expire(&w.h, id).await;
    refused(
        &w.auth,
        request(secret.expose(), 11),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    assert_eq!(w.auth.gc().await.unwrap().api_keys, 1);
    assert_eq!(key_ids(&w.h).await, [persistent]);
}

#[tokio::test]
async fn programmatic_management_limit_ownership_scope_and_switch() {
    let w = world().await;
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    let uid = user_with_password(&w.h, "manager", "pw").await;
    let other = user_with_password(&w.h, "stranger", "pw").await;
    let expiry = from_now(&w.h, ONE_HOUR).await;
    let (_, k) = issue_for(&w, uid, Some(expiry)).await;
    // nine more keys, generated with the first
    let mut tx = w.h.begin_as(uid).await.unwrap();
    for _ in 0..9 {
        generate(&mut tx, k.expose(), "rpc", "generated", Some(expiry))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(key_ids(&w.h).await.len(), 10);
    // the 11th reaches the default limit
    let mut tx = w.h.begin_as(uid).await.unwrap();
    refusal(
        generate(&mut tx, k.expose(), "rpc", "eleventh", Some(expiry))
            .await
            .err()
            .unwrap(),
        ErrorKind::UserError,
        "Limit of 10 API keys is reached for programmatic creation",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // works after raising the limit to 11
    set_param(&w.h, "base.programmatic_api_keys_limit", "11").await;
    let mut tx = w.h.begin_as(uid).await.unwrap();
    generate(&mut tx, k.expose(), "rpc", "eleventh", Some(expiry))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(key_ids(&w.h).await.len(), 11);
    // generating with scope `other`
    let mut tx = w.h.begin_as(uid).await.unwrap();
    refusal(
        generate(&mut tx, k.expose(), "other", "scoped", Some(expiry))
            .await
            .err()
            .unwrap(),
        ErrorKind::AccessDenied,
        GENERATE_INVALID,
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // generating as another user with this key
    let mut tx = w.h.begin_as(other).await.unwrap();
    refusal(
        generate(&mut tx, k.expose(), "rpc", "borrowed", Some(expiry))
            .await
            .err()
            .unwrap(),
        ErrorKind::AccessDenied,
        GENERATE_INVALID,
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // disabling the setting after it was enabled: generate and revoke are both refused
    set_param(&w.h, "base.enable_programmatic_api_keys", "False").await;
    let mut tx = w.h.begin_as(uid).await.unwrap();
    refusal(
        generate(&mut tx, k.expose(), "rpc", "disabled", Some(expiry))
            .await
            .err()
            .unwrap(),
        ErrorKind::UserError,
        NOT_ENABLED,
    );
    refusal(
        revoke(&mut tx, k.expose()).await.err().unwrap(),
        ErrorKind::UserError,
        NOT_ENABLED,
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(key_ids(&w.h).await.len(), 11);
}

#[tokio::test]
async fn key_scope_must_equal_the_requested_route_scope() {
    let w = world().await;
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    let uid = user_with_password(&w.h, "bob", "pw").await;
    let expiry = from_now(&w.h, ONE_HOUR).await;
    let (_, k) = issue_for(&w, uid, Some(expiry)).await;
    let (mcp_id, mcp) = issue_for(&w, uid, Some(expiry)).await;
    into_mcp_key(&w.h, mcp_id).await;
    // the `rpc` key matches the `rpc` audience; a request demanding another audience gets no
    // principal, and neither does a stored key of another scope at the `rpc` audience
    assert_eq!(
        accepted(&w.auth, request(k.expose(), 1)).await,
        UserId::new(uid).unwrap()
    );
    refused(
        &w.auth,
        ApiKeyRequest::new(k.expose(), KeyScope::Mcp, "192.0.2.2".parse().unwrap()),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    refused(
        &w.auth,
        request(mcp.expose(), 3),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    // the scope set is closed: a custom scope cannot be requested
    refusal(
        KeyScope::parse("custom_scope").err().unwrap(),
        ErrorKind::Validation,
        "scope must be rpc or mcp",
    );
    // generate with a scope that differs from the key's
    let mut tx = w.h.begin_as(uid).await.unwrap();
    for scope in ["custom_scope", "mcp"] {
        refusal(
            generate(&mut tx, k.expose(), scope, "renewal", Some(expiry))
                .await
                .err()
                .unwrap(),
            ErrorKind::AccessDenied,
            GENERATE_INVALID,
        );
    }
    tx.rollback().await.unwrap();
    // the JSON-2 route (scope `rpc`) answers 401 for a key whose scope does not match it
    let web = json2(&w.h).await;
    let (response, body) = web.bearer(k.expose(), 4).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (response, body) = web.bearer(mcp.expose(), 5).await;
    error(
        &response,
        &body,
        StatusCode::UNAUTHORIZED,
        "werkzeug.exceptions.Unauthorized",
        "Invalid apikey",
    );
}

#[tokio::test]
async fn group_lifetime_expired_verification_and_bounded_collection() {
    let w = world().await;
    // the user issues its own keys (programmatic switch, as's test)
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    let uid = thirty_day_user(&w.h, "bob").await;
    let hasher = hasher(&w.h).await;
    w.h.orm().check_audit_store().await.unwrap();
    // beyond the 30-day group maximum
    let mut tx = w.h.begin_as(uid).await.unwrap();
    let clock = now(&mut tx).await;
    refusal(
        issue(
            &mut tx,
            &hasher,
            &IssueRequest::new(
                UserId::new(uid).unwrap(),
                "too long",
                KeyKind::User,
                KeyScope::Rpc,
                Some(DateTime::from_micros(clock.micros() + 31 * ONE_DAY)),
            )
            .unwrap(),
        )
        .await
        .err()
        .unwrap(),
        ErrorKind::Validation,
        "You cannot exceed 30.0 days.",
    );
    tx.rollback().await.unwrap();
    // an expired key fails verification
    let (expired_id, expired) = issue_own(&w, uid, Some(from_now(&w.h, ONE_HOUR).await)).await;
    let (live_id, live) = issue_own(&w, uid, Some(from_now(&w.h, 29 * ONE_DAY).await)).await;
    let (persistent_id, _) = issue_for(&w, w.admin, None).await;
    expire(&w.h, expired_id).await;
    refused(
        &w.auth,
        request(expired.expose(), 1),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    // collection deletes the expired key and keeps the live and the persistent ones
    assert_eq!(w.auth.gc().await.unwrap().api_keys, 1);
    let mut kept = vec![live_id, persistent_id];
    kept.sort_unstable();
    assert_eq!(key_ids(&w.h).await, kept);
    assert_eq!(
        accepted(&w.auth, request(live.expose(), 2)).await,
        UserId::new(uid).unwrap()
    );
}

// ---- refusal pins ----

#[tokio::test]
async fn restricted_principals_cannot_manage_keys() {
    let w = world().await;
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    let uid = user_with_password(&w.h, "restricted", "pw").await;
    let expiry = from_now(&w.h, ONE_HOUR).await;
    let (id, k) = issue_for(&w, uid, Some(expiry)).await;
    let scope = Scope::builder(ApiKeyId::new(id).unwrap(), "read only")
        .models(erp_orm::ModelAllow::All)
        .methods(erp_orm::MethodAllow::All)
        .read_only(true)
        .build()
        .unwrap();
    let principal = PrincipalBuilder::user(UserId::new(uid).unwrap())
        .scope(scope)
        .build()
        .unwrap();
    let mut tx =
        w.h.orm()
            .begin_opts(BeginOptions::for_principal(principal))
            .await
            .unwrap();
    refusal(
        generate(&mut tx, k.expose(), "rpc", "escape", Some(expiry))
            .await
            .err()
            .unwrap(),
        ErrorKind::Access,
        "Restricted principals cannot manage unrestricted API keys",
    );
    refusal(
        revoke(&mut tx, k.expose()).await.err().unwrap(),
        ErrorKind::Access,
        "Restricted principals cannot manage unrestricted API keys",
    );
    tx.rollback().await.unwrap();
    assert_eq!(key_ids(&w.h).await, [id]);
}

/// a stored agent or service key presented on JSON-2 receives the named
/// refusal as 403 AccessDenied, never authority and never an internal error.
#[tokio::test]
async fn json2_refuses_stored_agent_and_service_keys_by_name() {
    let w = world().await;
    let uid = user_with_password(&w.h, "kinds", "pw").await;
    let (id, k) = issue_for(&w, uid, Some(from_now(&w.h, ONE_HOUR).await)).await;
    let web = json2(&w.h).await;
    for (address, kind) in [(1, "agent"), (2, "service")] {
        sql(
            &w.h,
            &format!(
                "UPDATE res_users_apikeys SET kind='{kind}',capabilities='none' WHERE id={id}"
            ),
        )
        .await;
        let (response, body) = web.bearer(k.expose(), address).await;
        error(
            &response,
            &body,
            StatusCode::FORBIDDEN,
            "erp.exceptions.AccessDenied",
            &format!("API keys of kind '{kind}' are not accepted until a later release"),
        );
        assert!(!String::from_utf8_lossy(&body).contains(k.expose()));
    }
}

// ---- two nodes over one database ----

/// predates the other node's committed issuance cannot use the obsolete count and is refused by
/// the limit on its whole-transaction retry)
#[tokio::test]
async fn last_issuance_slot_contested_from_two_nodes() {
    let w = signal_world().await;
    let uid = user_with_password(&w.h, "contested", "pw").await;
    set_param(&w.h, "base.programmatic_api_keys_limit", "1").await;
    let expiry = from_now(&w.h, ONE_HOUR).await;
    let hasher = hasher(&w.h).await;
    let a = node(&w).await;
    let b = node(&w).await;
    let admin = UserId::new(w.admin).unwrap();
    let req = IssueRequest::new(
        UserId::new(uid).unwrap(),
        "contested",
        KeyKind::User,
        KeyScope::Rpc,
        Some(expiry),
    )
    .unwrap();
    // A takes the subject fence and inserts, not yet committed
    let mut on_a = begin_on(&a, admin).await;
    issue(&mut on_a, &hasher, &req).await.unwrap();
    // B's snapshot is taken before A commits
    let mut on_b = begin_on(&b, admin).await;
    now(&mut on_b).await;
    on_a.commit().await.unwrap();
    // B's fence write meets the row A changed after B's snapshot
    let e = issue(&mut on_b, &hasher, &req).await.err().unwrap();
    assert_eq!(e.kind(), ErrorKind::Concurrency);
    assert_eq!(e.sqlstate(), Some("40001"));
    assert_eq!(
        e.to_string(),
        "could not serialize access due to concurrent update"
    );
    on_b.rollback().await.unwrap();
    // B's retry of the whole transaction sees A's key and the shared limit
    let mut retry = begin_on(&b, admin).await;
    refusal(
        issue(&mut retry, &hasher, &req).await.err().unwrap(),
        ErrorKind::UserError,
        "Limit of 1 API keys is reached for programmatic creation",
    );
    retry.rollback().await.unwrap();
    assert_eq!(key_ids(&w.h).await.len(), 1);
}

/// verification on node B; after A commits, B refuses the key)
#[tokio::test]
async fn uncommitted_revoke_on_one_node_and_verification_on_another() {
    let w = signal_world().await;
    set_param(&w.h, "base.enable_programmatic_api_keys", "True").await;
    let uid = user_with_password(&w.h, "revoked", "pw").await;
    let (_, k) = issue_for(&w, uid, Some(from_now(&w.h, ONE_HOUR).await)).await;
    let a = node(&w).await;
    let b = node(&w).await;
    let owner = UserId::new(uid).unwrap();
    let mut on_a = begin_on(&a, owner).await;
    assert!(revoke(&mut on_a, k.expose()).await.unwrap());
    // The bound turns a blocked verification into a failure instead of a hang; it asserts no
    // duration.
    let subject = tokio::time::timeout(
        Duration::from_secs(30),
        accepted(&b, request(k.expose(), 1)),
    )
    .await
    .expect("a verification on another node does not wait for an uncommitted revoke");
    assert_eq!(subject, owner);
    on_a.commit().await.unwrap();
    refused(
        &b,
        request(k.expose(), 2),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    assert!(key_ids(&w.h).await.is_empty());
}

/// changes verification on node B; after A commits the live key still verifies, the expired one
/// stays refused and is gone)
#[tokio::test]
async fn uncommitted_collection_on_one_node_and_verification_on_another() {
    let w = signal_world().await;
    let uid = user_with_password(&w.h, "collected", "pw").await;
    let expiry = from_now(&w.h, ONE_HOUR).await;
    let (live_id, live) = issue_for(&w, uid, Some(expiry)).await;
    let (expired_id, expired) = issue_for(&w, uid, Some(expiry)).await;
    expire(&w.h, expired_id).await;
    let a = node(&w).await;
    let b = node(&w).await;
    let owner = UserId::new(uid).unwrap();
    let mut on_a = begin_on(&a, erp_orm::SUPERUSER_ID).await;
    assert_eq!(collect_expired(&mut on_a, 10).await.unwrap(), 1);
    // The bound turns a blocked verification into a failure instead of a hang; it asserts no
    // duration.
    let subject = tokio::time::timeout(
        Duration::from_secs(30),
        accepted(&b, request(live.expose(), 1)),
    )
    .await
    .expect("a verification on another node does not wait for an uncommitted collection");
    assert_eq!(subject, owner);
    tokio::time::timeout(
        Duration::from_secs(30),
        refused(
            &b,
            request(expired.expose(), 2),
            ErrorKind::AccessDenied,
            "Invalid apikey",
        ),
    )
    .await
    .expect("the expired key is refused without waiting for the collector");
    on_a.commit().await.unwrap();
    assert_eq!(accepted(&b, request(live.expose(), 3)).await, owner);
    refused(
        &b,
        request(expired.expose(), 4),
        ErrorKind::AccessDenied,
        "Invalid apikey",
    )
    .await;
    assert_eq!(key_ids(&w.h).await, [live_id]);
}
