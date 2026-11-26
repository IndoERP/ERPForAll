// SPDX-License-Identifier: MIT OR Apache-2.0
//! The attachment content route through the real server binary: files uploaded the
//! way the chatter uploads them are read back by id with a browser session, refusals are one `404`,
//! ranges, headers and the missing-content failure are checked on the wire.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
#[path = "support/runtime_addons.rs"]
mod runtime_addons;

use erp_testkit::TestDb;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

/// One exchange with the body kept as bytes (attachment content is not text).
struct Raw {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Raw {
    fn header(&self, name: &str) -> Option<&str> {
        let mut found = self
            .headers
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str());
        let first = found.next();
        assert!(found.next().is_none(), "header {name} repeats");
        first
    }
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }
    /// Everything a client can compare, apart from what differs per request.
    fn comparable(&self) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut headers: Vec<_> = self
            .headers
            .iter()
            .filter(|(key, _)| key != "x-request-id" && key != "date")
            .cloned()
            .collect();
        headers.sort();
        (self.status, headers, self.body.clone())
    }
}

fn raw(port: u16, method: &str, path: &str, headers: &[(&str, &str)]) -> Raw {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut wire =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (name, value) in headers {
        wire.push_str(&format!("{name}: {value}\r\n"));
    }
    wire.push_str("\r\n");
    stream.write_all(wire.as_bytes()).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8(bytes[..split].to_vec()).unwrap();
    let body = bytes[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    Raw {
        status,
        headers,
        body,
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn start(config: &Path, stderr: &Path, port: u16, label: &str) -> Server {
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(config)
            .stdout(Stdio::null())
            .stderr(fs::File::create(stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            panic!(
                "{label} server exited {status}: {}",
                fs::read_to_string(stderr).unwrap_or_default()
            );
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok()
            && raw(port, "GET", "/erp/health/ready", &[]).status == 200
        {
            return server;
        }
        assert!(
            Instant::now() < until,
            "server did not become ready: {}",
            fs::read_to_string(stderr).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn login(port: u16, db: &str, user: &str, password: &str) -> String {
    let page = native_http::request(port, "GET", "/erp/login", &[], "");
    assert_eq!(page.status, 200);
    let prefs = native_http::cookie(&page, "prefs");
    let csrf = native_http::embedded(&page, "erp-login")["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let logged = native_http::request(
        port,
        "POST",
        "/erp/login",
        &[
            ("cookie", &prefs),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!(
            "db={db}&login={user}&password={password}&redirect=%2Ferp&csrf_token={}",
            native_http::encoded(csrf.as_bytes())
        ),
    );
    assert_eq!(logged.status, 303, "login of {user}: {}", logged.body);
    format!("{prefs}; {}", native_http::cookie(&logged, "session_id"))
}

fn ok(reply: &native_http::Reply, what: &str) -> Value {
    assert_eq!(reply.status, 200, "{what}: {}", reply.body);
    serde_json::from_str(&reply.body).unwrap()
}

fn class_headers(reply: &Raw, what: &str) {
    assert_eq!(
        reply.header("content-security-policy"),
        Some("default-src 'none'; sandbox"),
        "{what}"
    );
    assert_eq!(
        reply.header("x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    assert_eq!(
        reply.header("referrer-policy"),
        Some("no-referrer"),
        "{what}"
    );
    assert_eq!(
        reply.header("cross-origin-resource-policy"),
        Some("same-origin"),
        "{what}"
    );
    let cache = if (200..300).contains(&reply.status) {
        "private, no-store"
    } else {
        "no-store"
    };
    assert_eq!(reply.header("cache-control"), Some(cache), "{what}");
}

fn plain_error(reply: &Raw, status: u16, body: &str, what: &str) {
    assert_eq!(reply.status, status, "{what}: {}", reply.text());
    assert_eq!(reply.text(), body, "{what}");
    assert_eq!(
        reply.header("content-type"),
        Some("text/plain; charset=utf-8"),
        "{what}: errors stay plain text, never the HTML error page"
    );
    class_headers(reply, what);
}

#[tokio::test(flavor = "multi_thread")]
async fn local_proxy_handoff_keeps_content_authorization_and_http_headers() {
    use erp_sdk::blob::{
        AttachmentStore, ContentKey, LocalStore, Namespace, StageLimits, StagingKey,
    };
    let store = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let fixture = native_http::NativeFixture::start_with_config(&format!(
        "[attachments]\nwrite_store='main'\n[attachments.stores.main]\nkind='local'\npath={:?}\nx_accel_prefix='/_erp_private/main/'",
        store.path().to_str().unwrap(),
    )).await;
    let port = fixture.port;
    let auth = [("cookie", fixture.cookies.as_str())];
    let call = |path: &str, body: Value| {
        ok(
            &native_http::json_post(port, &fixture.cookies, path, &body.to_string()),
            path,
        )
    };
    call("/erp/module/install", json!({"modules":["mail"]}));
    let partner = call(
        "/erp/json/2/res.partner/create",
        json!({"vals_list":[{"name":"Offload authorization"}]}),
    )[0]
    .as_i64()
    .unwrap();
    let bytes = b"A real local attachment body.";
    let id = call(
        "/erp/json/2/res.partner/thread_upload",
        json!({"ids":[partner],"name":"file.txt","mimetype":"text/plain","data":base64(bytes)}),
    )["attachment_id"]
        .as_i64()
        .unwrap();
    call(
        "/erp/json/2/res.partner/thread_post",
        json!({"ids":[partner],"post":{"body":"Claim","attachment_ids":[id]}}),
    );
    let local = LocalStore::open(store.path()).await.unwrap();
    fixture.client.execute(
        "INSERT INTO ir_blob_store(store_key,fingerprint) VALUES ('main',$1) ON CONFLICT DO NOTHING",
        &[&local.fingerprint().unwrap()],
    ).await.unwrap();
    let row = fixture
        .client
        .query_one(
            "SELECT id, namespace::text FROM ir_blob_store WHERE store_key='main' AND role='own'",
            &[],
        )
        .await
        .unwrap();
    let namespace = Namespace::parse(row.get::<_, &str>(1)).unwrap();
    let staged_key =
        StagingKey::new(namespace.clone(), Namespace::random().unwrap().as_str()).unwrap();
    let mut source = bytes.as_slice();
    let staged = local
        .stage(&staged_key, &mut source, StageLimits::new(2048))
        .await
        .unwrap();
    let key = ContentKey::new(namespace, *staged.sha256());
    local.promote(&staged, &key).await.unwrap();
    let moved = fixture
        .client
        .execute(
            "WITH moved AS (UPDATE ir_blob b SET store_id=$2 FROM ir_attachment a
                        WHERE a.id=$1 AND b.id=a.blob_id AND b.sha256=$3 RETURNING b.id)
         DELETE FROM ir_blob_inline WHERE blob_id IN (SELECT id FROM moved)",
            &[&id, &row.get::<_, i16>(0), &staged.sha256().as_slice()],
        )
        .await
        .unwrap();
    assert_eq!(moved, 1);
    let path = format!("/erp/content/{id}");
    let full = raw(port, "GET", &path, &auth);
    assert_eq!(full.status, 200);
    assert!(full.body.is_empty());
    class_headers(&full, "local proxy handoff");
    assert_eq!(full.header("content-type"), Some("text/plain"));
    assert!(
        full.header("content-disposition")
            .unwrap()
            .starts_with("attachment;")
    );
    let internal = full.header("x-accel-redirect").unwrap();
    let key = internal.strip_prefix("/_erp_private/main/").unwrap();
    assert_eq!(fs::read(store.path().join(key)).unwrap(), bytes);
    assert_eq!(raw(port, "GET", internal, &auth).status, 404);

    let head = raw(port, "HEAD", &path, &auth);
    assert_eq!(head.status, 200);
    assert_eq!(head.header("x-accel-redirect"), None);
    assert_eq!(head.header("etag"), full.header("etag"));
    assert_eq!(
        head.header("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        bytes.len()
    );
    let range = raw(port, "GET", &path, &[auth[0], ("range", "bytes=2-5")]);
    assert_eq!(range.status, 206);
    assert_eq!(range.header("x-accel-redirect"), None);
    assert_eq!(range.body, &bytes[2..=5]);
    let conditional = raw(
        port,
        "GET",
        &path,
        &[auth[0], ("if-none-match", full.header("etag").unwrap())],
    );
    assert_eq!(conditional.status, 304);
    assert_eq!(conditional.header("x-accel-redirect"), None);

    let denied = raw(port, "GET", &path, &[]);
    let absent = raw(port, "GET", "/erp/content/2147000000", &[]);
    assert_eq!(denied.status, 404);
    assert_eq!(denied.comparable(), absent.comparable());
    assert_eq!(denied.header("x-accel-redirect"), None);
    fs::remove_file(store.path().join(key)).unwrap();
    let missing = raw(port, "GET", &path, &auth);
    assert_eq!(missing.status, 500);
    assert_eq!(missing.header("x-accel-redirect"), None);
    fixture.cleanup().await;
}

#[test]
fn the_content_route_serves_readers_and_answers_every_refusal_with_one_404() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(acceptance());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn acceptance() {
    let db = TestDb::create().await.unwrap();
    let role = format!("{}_web", db.name());
    let client = db.client().await.unwrap();
    let temporary = std::env::temp_dir().canonicalize().unwrap();
    let dir = tempfile::tempdir_in(temporary).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons");
    let addons = dir.path().join("runtime-addons");
    runtime_addons::stage_runtime_addons(&source, &addons).unwrap();
    let addons = addons.canonicalize().unwrap();
    let config = native_http::config(dir.path(), &db, port, &role, &addons);
    client
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password'; CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",
            erp_sql::quote(&role),
            erp_sql::quote(&format!("{role}_node"))
        ))
        .await
        .unwrap();
    native_http::private(
        &dir.path().join("password"),
        "fixture-administrator-password",
    );
    let init = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["db", "init", "--config"])
        .arg(&config)
        .args(["--admin-login", "fixture-admin", "--admin-password-file"])
        .arg(dir.path().join("password"))
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    native_http::login_rollout::prepare(&db, &role).await;
    let stderr = dir.path().join("server.stderr");
    let server = start(&config, &stderr, port, "first");
    let admin = login(
        port,
        db.name(),
        "fixture-admin",
        "fixture-administrator-password",
    );
    let call = |cookies: &str, path: &str, body: Value| {
        native_http::json_post(port, cookies, path, &body.to_string())
    };
    ok(
        &call(&admin, "/erp/module/install", json!({"modules":["mail"]})),
        "install mail",
    );
    // a deployment installs its addons and restarts: serve from the installed registry
    drop(server);
    client.execute(
        "INSERT INTO ir_config_parameter (key,value) VALUES ('base.enable_programmatic_api_keys','True') ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value",
        &[],
    ).await.unwrap();
    client.execute("UPDATE res_groups SET api_key_duration=7 WHERE id IN (SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_user')", &[]).await.unwrap();
    let _server = start(&config, &stderr, port, "restarted");

    let xid = async |name: &str| -> i64 {
        client
            .query_one(
                "SELECT res_id::int8 FROM ir_model_data WHERE module='base' AND name=$1",
                &[&name],
            )
            .await
            .unwrap()
            .get(0)
    };
    let main = xid("main_company").await;
    let user_group = xid("group_user").await;
    let portal_group = xid("group_portal").await;
    let admin_id: i64 = client
        .query_one(
            "SELECT id::int8 FROM res_users WHERE login = 'fixture-admin'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let second = ok(
        &call(
            &admin,
            "/erp/json/2/res.company/create",
            json!({"vals_list":[{"name":"Content Route Second Company"}]}),
        ),
        "second company",
    )[0]
    .as_i64()
    .unwrap();
    ok(
        &call(
            &admin,
            "/erp/json/2/res.users/write",
            json!({"ids":[admin_id],"vals":{"company_ids":[[4, second]]}}),
        ),
        "administrator joins the second company",
    );
    let partner = |name: &str, company: i64| {
        ok(
            &call(
                &admin,
                "/erp/json/2/res.partner/create",
                json!({"vals_list":[{"name":name,"company_id":company}]}),
            ),
            name,
        )[0]
        .as_i64()
        .unwrap()
    };
    let in_main = partner("Content Main Record", main);
    let in_second = partner("Content Second Record", second);
    for (login_name, companies, group) in [
        ("content-reader-one", vec![main], user_group),
        ("content-reader-two", vec![main, second], user_group),
        ("content-portal", vec![main], portal_group),
    ] {
        let own = partner(login_name, main);
        ok(
            &call(
                &admin,
                "/erp/json/2/res.users/create",
                json!({"vals_list":[{
                    "name":login_name, "login":login_name, "password":"fixture-content-reader",
                    "partner_id":own, "company_id":main, "company_ids":[[6,0,companies]],
                    "group_ids":[[6,0,[group]]], "lang":"en_US", "tz":"UTC"
                }]}),
            ),
            login_name,
        );
    }
    let one = login(
        port,
        db.name(),
        "content-reader-one",
        "fixture-content-reader",
    );
    let two = login(
        port,
        db.name(),
        "content-reader-two",
        "fixture-content-reader",
    );
    let portal = login(port, db.name(), "content-portal", "fixture-content-reader");
    let uid = async |login_name: &str| -> i64 {
        client
            .query_one(
                "SELECT id::int8 FROM res_users WHERE login = $1",
                &[&login_name],
            )
            .await
            .unwrap()
            .get(0)
    };
    let (one_id, portal_id) = (uid("content-reader-one").await, uid("content-portal").await);

    let upload = |record: i64, name: &str, mimetype: &str, bytes: &[u8]| {
        ok(
            &call(
                &admin,
                "/erp/json/2/res.partner/thread_upload",
                json!({"ids":[record],"name":name,"mimetype":mimetype,"data":base64(bytes)}),
            ),
            name,
        )["attachment_id"]
            .as_i64()
            .unwrap()
    };
    let hello_bytes = b"hello world".to_vec();
    // below the 2 KiB inline threshold: thread_upload carries content in memory only up to it
    let big_bytes: Vec<u8> = (0..2000_u32)
        .map(|i| b"abcdefghijklmnopqrstuvwxyz0123456789"[(i % 36) as usize])
        .collect();
    let png_bytes = {
        // a 1x1 PNG (the server detects the type from the signature)
        let hex = "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082";
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect::<Vec<u8>>()
    };
    let html_bytes = b"<!doctype html><html><body><script>alert(1)</script></body></html>".to_vec();
    let hello = upload(in_main, "hello.txt", "text/plain", &hello_bytes);
    let big = upload(in_main, "big.txt", "text/plain", &big_bytes);
    let png = upload(in_main, "pixel.png", "image/png", &png_bytes);
    let html = upload(in_main, "page.html", "text/html", &html_bytes);
    let other_company = upload(in_second, "second.txt", "text/plain", &hello_bytes);
    let standalone = upload(in_main, "standalone.txt", "text/plain", &hello_bytes);
    let own_standalone = upload(in_main, "own.txt", "text/plain", &hello_bytes);
    let portal_own = upload(in_main, "portal-own.txt", "text/plain", &hello_bytes);
    let deny_listed = upload(in_main, "deny-listed.txt", "text/plain", &hello_bytes);
    let settings_plain = upload(in_main, "settings.txt", "text/plain", &hello_bytes);
    let settings_field = upload(in_main, "settings-field.txt", "text/plain", &hello_bytes);
    let no_rights = upload(in_main, "no-rights.txt", "text/plain", &hello_bytes);
    let blobless = upload(in_main, "blobless.txt", "text/plain", &hello_bytes);
    let lost = upload(
        in_main,
        "lost.txt",
        "text/plain",
        b"content that will be lost",
    );
    // an upload is the composer's until a post claims it for the record
    let post = |record: i64, files: Vec<i64>| {
        ok(
            &call(
                &admin,
                "/erp/json/2/res.partner/thread_post",
                json!({"ids":[record],"post":{"body":"files","attachment_ids":files}}),
            ),
            "thread_post",
        );
    };
    post(in_main, vec![hello, big, png, html, blobless, lost]);
    post(in_second, vec![other_company]);
    client
        .execute(
            "UPDATE ir_attachment SET res_model = NULL, res_id = NULL WHERE id = $1::int8",
            &[&standalone],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE ir_attachment SET blob_id = NULL WHERE id = $1::int8",
            &[&blobless],
        )
        .await
        .unwrap();
    // standalone files whose creator is the reader: the creator rule admits them
    for (id, creator) in [(own_standalone, one_id), (portal_own, portal_id)] {
        client
            .execute(
                "UPDATE ir_attachment SET res_model = NULL, res_id = NULL, create_uid = $2::int8 WHERE id = $1::int8",
                &[&id, &creator],
            )
            .await
            .unwrap();
    }
    // reader one's own settings row: readable by its owner, whose `user_id` field is restricted
    // to the system group (`addons/base/src/res_users_settings.rs`)
    let settings: i64 = client
        .query_one(
            "INSERT INTO res_users_settings (user_id) VALUES ($1::int8) RETURNING id::int8",
            &[&one_id],
        )
        .await
        .unwrap()
        .get(0);
    for (id, field) in [(settings_plain, None), (settings_field, Some("user_id"))] {
        client
            .execute(
                "UPDATE ir_attachment SET res_model = 'res.users.settings', res_id = $2::int8, res_field = $3::text WHERE id = $1::int8",
                &[&id, &settings, &field],
            )
            .await
            .unwrap();
    }
    // a parent model internal users have no read rights on (not on the deny list); it is a
    // registered model, so its 404 comes from the access check, not the unknown-model branch
    let registered: i64 = client
        .query_one(
            "SELECT count(*)::int8 FROM ir_model WHERE model = 'ir.module.module'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(registered, 1, "ir.module.module is a registered model");
    client
        .execute(
            "UPDATE ir_attachment SET res_model = 'ir.module.module', res_id = (SELECT id FROM ir_module_module WHERE name = 'base') WHERE id = $1::int8",
            &[&no_rights],
        )
        .await
        .unwrap();
    // a parent the caller cannot read: a model on the attachment deny list
    client
        .execute(
            "UPDATE ir_attachment SET res_model = 'res.users.apikeys', res_id = 1 WHERE id = $1::int8",
            &[&deny_listed],
        )
        .await
        .unwrap();
    let link = ok(
        &call(
            &admin,
            "/erp/json/2/ir.attachment/create",
            json!({"vals_list":[{"name":"link","type":"url","url":"https://example.com/file",
                "res_model":"res.partner","res_id":in_main}]}),
        ),
        "url attachment",
    )[0]
    .as_i64()
    .unwrap();
    let nonexistent: i64 = client
        .query_one(
            "SELECT coalesce(max(id), 0)::int8 + 1000 FROM ir_attachment",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let get = |cookies: &str, path: &str, extra: &[(&str, &str)]| {
        let mut headers = vec![("cookie", cookies)];
        headers.extend_from_slice(extra);
        raw(port, "GET", path, &headers)
    };

    // the owner reads a text file: exact class headers, attachment disposition
    let read = get(&admin, &format!("/erp/content/{hello}"), &[]);
    assert_eq!(read.status, 200, "{}", read.text());
    assert_eq!(read.body, hello_bytes);
    assert_eq!(read.header("content-type"), Some("text/plain"));
    assert_eq!(read.header("content-length"), Some("11"));
    assert_eq!(
        read.header("content-disposition"),
        Some("attachment; filename=\"hello.txt\"; filename*=UTF-8''hello.txt")
    );
    assert_eq!(read.header("accept-ranges"), Some("bytes"));
    let tag = read.header("etag").unwrap();
    assert_eq!(tag.len(), 66);
    assert!(tag.starts_with('"') && tag.ends_with('"'));
    class_headers(&read, "owner read");
    // HEAD: the same headers, no body
    let head = raw(
        port,
        "HEAD",
        &format!("/erp/content/{hello}"),
        &[("cookie", &admin)],
    );
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-length"), Some("11"));
    assert!(head.body.is_empty());
    assert_eq!(head.header("etag"), Some(tag));
    class_headers(&head, "head");
    let legacy = get(&admin, &format!("/web/content/{hello}"), &[]);
    assert_eq!(legacy.status, 200, "{}", legacy.text());
    assert_eq!(legacy.body, hello_bytes);

    // A real PNG survives image admission and the stored image derivative is addressed by field.
    let tiny_png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    ok(
        &call(
            &admin,
            "/erp/json/2/res.partner/write",
            json!({"ids":[in_main, in_second],"vals":{"image_1920":tiny_png}}),
        ),
        "write partner image",
    );
    for field in ["image_1920", "image_128"] {
        let path = format!("/erp/content/res.partner/{in_main}/{field}");
        let field_read = get(&one, &path, &[]);
        assert_eq!(
            field_read.status,
            200,
            "{field}: {}",
            String::from_utf8_lossy(&field_read.body)
        );
        let field_head = raw(port, "HEAD", &path, &[("cookie", one.as_str())]);
        assert_eq!(field_head.status, 200);
        assert_eq!(
            field_head.header("content-length"),
            field_read.header("content-length")
        );
        assert_eq!(field_head.header("etag"), field_read.header("etag"));
        assert!(field_read.body.starts_with(b"\x89PNG"));
        assert_eq!(field_read.header("content-type"), Some("image/png"));
        let ranged = get(&one, &path, &[("range", "bytes=1-3")]);
        assert_eq!(ranged.status, 206);
        assert_eq!(ranged.body, b"PNG");
        let denied = get(
            &one,
            &format!("/erp/content/res.partner/{in_second}/{field}"),
            &[("if-none-match", "*")],
        );
        assert_eq!(denied.status, 404);
        assert_eq!(denied.header("etag"), None);
        assert_eq!(get(&portal, &path, &[]).status, 404);
    }

    let image = get(&admin, &format!("/erp/content/{png}"), &[]);
    assert_eq!(image.status, 200);
    assert_eq!(image.body, png_bytes);
    assert_eq!(image.header("content-type"), Some("image/png"));
    assert_eq!(
        image.header("content-disposition"),
        Some("inline; filename=\"pixel.png\"; filename*=UTF-8''pixel.png")
    );
    let download = get(&admin, &format!("/erp/content/{png}?download=true"), &[]);
    assert_eq!(
        download.header("content-disposition"),
        Some("attachment; filename=\"pixel.png\"; filename*=UTF-8''pixel.png")
    );
    let octet = get(
        &admin,
        &format!("/erp/content/{png}?mimetype=application/octet-stream"),
        &[],
    );
    assert_eq!(
        octet.header("content-type"),
        Some("application/octet-stream")
    );
    assert!(
        octet
            .header("content-disposition")
            .unwrap()
            .starts_with("attachment;")
    );
    let page = get(&admin, &format!("/erp/content/{html}"), &[]);
    assert_eq!(page.status, 200);
    assert_eq!(
        page.header("content-type"),
        Some("application/octet-stream")
    );
    assert!(
        page.header("content-disposition")
            .unwrap()
            .starts_with("attachment;")
    );
    plain_error(
        &get(
            &admin,
            &format!("/erp/content/{png}?mimetype=text/html"),
            &[],
        ),
        400,
        "The mimetype parameter may only be application/octet-stream.",
        "mimetype override",
    );
    plain_error(
        &get(&admin, &format!("/erp/content/{png}?cids=1"), &[]),
        400,
        "The content route accepts download, mimetype and access_token, each at most once.",
        "unknown query key",
    );

    // ranges, after the access check
    let part = get(
        &admin,
        &format!("/erp/content/{big}"),
        &[("range", "bytes=100-199")],
    );
    assert_eq!(part.status, 206, "{}", part.text());
    assert_eq!(part.body, big_bytes[100..200]);
    assert_eq!(part.header("content-range"), Some("bytes 100-199/2000"));
    assert_eq!(part.header("content-length"), Some("100"));
    class_headers(&part, "206");
    let suffix = get(
        &admin,
        &format!("/erp/content/{hello}"),
        &[("range", "bytes=-5")],
    );
    assert_eq!(suffix.status, 206);
    assert_eq!(suffix.body, b"world");
    assert_eq!(suffix.header("content-range"), Some("bytes 6-10/11"));
    for (range, reason) in [
        (
            "bytes=0-1,4-5",
            "Several byte ranges are not served; ask for one range.",
        ),
        ("bytes=x-", "The Range header is malformed."),
        ("bytes=5000-", "The requested range is outside the content."),
    ] {
        let refused = get(&admin, &format!("/erp/content/{big}"), &[("range", range)]);
        plain_error(&refused, 416, reason, range);
        assert_eq!(
            refused.header("content-range"),
            Some("bytes */2000"),
            "{range}"
        );
    }
    let validated = get(
        &admin,
        &format!("/erp/content/{big}"),
        &[("range", "bytes=0-9"), ("if-range", "\"anything\"")],
    );
    assert_eq!(
        validated.status, 200,
        "stale If-Range returns the whole representation"
    );
    assert_eq!(validated.body, big_bytes);

    let tag_big = validated.header("etag").unwrap();
    assert_ne!(tag, tag_big);
    for method in ["GET", "HEAD"] {
        let conditional = raw(
            port,
            method,
            &format!("/erp/content/{big}"),
            &[
                ("cookie", &admin),
                ("if-none-match", &format!("W/{tag_big}")),
                ("range", "bytes=1-3"),
            ],
        );
        assert_eq!(conditional.status, 304);
        assert!(conditional.body.is_empty());
        assert_eq!(conditional.header("etag"), Some(tag_big));
        let partial = raw(
            port,
            method,
            &format!("/erp/content/{big}"),
            &[
                ("cookie", &admin),
                ("if-range", tag_big),
                ("range", "bytes=1-3"),
            ],
        );
        assert_eq!(partial.status, 206);
        assert_eq!(partial.header("content-length"), Some("3"));
        if method == "GET" {
            assert_eq!(partial.body, big_bytes[1..4]);
        } else {
            assert!(partial.body.is_empty());
        }
    }

    // session and method
    plain_error(
        &raw(port, "GET", &format!("/erp/content/{hello}"), &[]),
        404,
        "Not found",
        "unpublished attachment",
    );
    let invalid_key = get(
        &admin,
        &format!("/erp/content/{hello}"),
        &[("authorization", "Bearer invalid-test-credential")],
    );
    assert_eq!(
        invalid_key.status, 401,
        "a bad key cannot fall back to the cookie"
    );
    let key_reply = native_http::request(port, "POST", "/erp/session/api-key", &[("content-type","application/json")],
        &json!({"db":db.name(), "login":"content-reader-one", "password":"fixture-content-reader","device_name":"attachment test"}).to_string());
    assert_eq!(key_reply.status, 200, "device key issue status");
    let issued: Value = serde_json::from_str(&key_reply.body).unwrap();
    let authorization = format!("Bearer {}", issued["api_key"].as_str().unwrap());
    let delegate = native_http::request(port, "POST", "/erp/session/api-key", &[("content-type","application/json")],
        &json!({"db":db.name(), "login":"content-reader-one", "password":"fixture-content-reader","device_name":"assistant test", "audience":"mcp"}).to_string());
    assert_eq!(delegate.status, 200);
    let delegation: Value = serde_json::from_str(&delegate.body).unwrap();
    let agent_authorization = format!("Bearer {}", delegation["api_key"].as_str().unwrap());
    assert_eq!(
        get(
            &admin,
            &format!("/erp/content/{hello}"),
            &[("authorization", &agent_authorization)]
        )
        .status,
        401
    );
    let tool_request = |method: &str, params: Value| {
        let response = native_http::request(
            port,
            "POST",
            "/erp/mcp",
            &[
                ("content-type", "application/json"),
                ("authorization", agent_authorization.as_str()),
                ("accept", "application/json, text/event-stream"),
            ],
            &json!({"jsonrpc":"2.0","id":17,"method":method,"params":params}).to_string(),
        );
        assert_eq!(response.status, 200);
        serde_json::from_str::<Value>(&response.body).unwrap()
    };
    let initialized = tool_request(
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    );
    assert_eq!(initialized["result"]["capabilities"]["tools"], json!({}));
    let catalog = tool_request("tools/list", json!({}));
    let offered = "res.company.action_all_company_branches";
    assert!(
        catalog["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == offered
                && tool["inputSchema"]["properties"]["ids"]["type"] == "array")
    );
    let invoke = |name: &str, id: i64| {
        tool_request("tools/call", json!({"name":name,"arguments":{"ids":[id]}}))
    };
    assert_eq!(invoke(offered, main)["result"]["isError"], false);
    assert_eq!(invoke(offered, second)["error"]["code"], -32602);
    assert_eq!(invoke("res.company.unlink", main)["error"]["code"], -32602);
    let activity_tool = "mail.activity.action_feedback";
    let includes_activity = |payload: &Value| {
        payload["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == activity_tool)
    };
    assert!(!includes_activity(&catalog));
    let activity = ok(
        &call(
            &one,
            "/erp/json/2/mail.activity/create",
            json!({"vals_list":[{
                "user_id":one_id, "summary":"Approved task", "date_deadline":"2030-01-01"
            }]}),
        ),
        "own activity",
    )[0]
    .as_i64()
    .unwrap();
    ok(
        &call(
            &admin,
            "/erp/json/2/ir.approval.policy/create",
            json!({"vals_list":[{
                "name":"Review assistant feedback", "principal_kind":"agent", "effect":"write",
                "model":"mail.activity", "method":"action_feedback", "outcome":"approve",
                "approver_user_ids":[[6,0,[admin_id]]]
            }]}),
        ),
        "agent policy",
    );
    assert!(includes_activity(&tool_request("tools/list", json!({}))));
    let finish = || {
        let response = native_http::request(port, "POST", "/erp/mcp", &[
            ("content-type","application/json"), ("authorization",&agent_authorization),
            ("idempotency-key","019a0000-0000-7000-8000-000000000017")
        ], &json!({"jsonrpc":"2.0","id":17,"method":"tools/call","params":{
            "name":activity_tool, "arguments":{"ids":[activity],"feedback":"Reviewed","attachment_ids":[]}
        }}).to_string());
        assert_eq!(response.status, 200);
        serde_json::from_str::<Value>(&response.body).unwrap()
    };
    let pending = finish();
    let request_id = pending["result"]["structuredContent"]["approval_request_id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        finish()["result"]["structuredContent"]["approval_request_id"],
        request_id
    );
    assert!(
        client
            .query_one("SELECT active FROM mail_activity WHERE id=$1", &[&activity])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    ok(
        &call(
            &admin,
            "/erp/json/2/ir.approval.request/erp_decide",
            json!({"request_id":request_id,"approve":true}),
        ),
        "approve activity",
    );
    let finished = finish();
    assert_eq!(finished["result"]["isError"], false);
    assert_eq!(finish()["result"], finished["result"]);
    let applied = client
        .query_one(
            "SELECT active, feedback, write_uid FROM mail_activity WHERE id=$1",
            &[&activity],
        )
        .await
        .unwrap();
    assert!(!applied.get::<_, bool>(0));
    assert_eq!(applied.get::<_, String>(1), "Reviewed");
    assert_eq!(applied.get::<_, i64>(2), one_id);
    let revoked = call(
        &one,
        "/erp/json/2/res.users.apikeys/revoke",
        json!({"key": delegation["api_key"]}),
    );
    assert_eq!(revoked.status, 200, "the owner can revoke a delegated key");
    let retired = native_http::request(
        port,
        "POST",
        "/erp/mcp",
        &[
            ("content-type", "application/json"),
            ("authorization", &agent_authorization),
        ],
        r#"{"jsonrpc":"2.0","id":22,"method":"tools/list"}"#,
    );
    assert_eq!(retired.status, 401);
    let local_alias = |target: i64| {
        let created = call(
            &admin,
            "/erp/json/2/ir.attachment/create",
            json!({"vals_list":[{
                "name":"same origin file", "type":"url", "url":format!("http://127.0.0.1:{port}/erp/content/{target}"),
                "res_model":"res.partner", "res_id":in_main
            }]}),
        );
        ok(&created, "local content alias")[0].as_i64().unwrap()
    };
    let hidden_target = local_alias(other_company);
    let hidden = get(&one, &format!("/erp/content/{hidden_target}"), &[]);
    assert_eq!(hidden.status, 404);
    assert!(hidden.header("location").is_none());
    let alias = local_alias(hello);
    let redirected = get(&admin, &format!("/erp/content/{alias}?download=true"), &[]);
    assert_eq!(redirected.status, 307);
    assert_eq!(
        redirected.header("location"),
        Some(format!("/erp/content/{hello}?download=true").as_str())
    );
    assert_eq!(
        raw(port, "GET", &format!("/erp/content/{alias}"), &[]).status,
        404
    );
    let key_read = |id| {
        raw(
            port,
            "GET",
            &format!("/erp/content/{id}"),
            &[("authorization", &authorization)],
        )
    };
    assert_eq!(key_read(hello).body, hello_bytes);
    assert_eq!(key_read(other_company).status, 404);
    assert_eq!(key_read(settings_field).status, 404);
    let share = |id, public_read| {
        let reply = call(
            &admin,
            "/erp/json/2/ir.attachment/erp_share_content",
            json!({"ids":[id], "public_read":public_read, "lifetime_seconds":90}),
        );
        assert_eq!(reply.status, 200, "share failed ({})", reply.status);
        serde_json::from_str::<String>(&reply.body).unwrap()
    };
    let revoke = |id| {
        ok(
            &call(
                &admin,
                "/erp/json/2/ir.attachment/erp_revoke_content",
                json!({"ids":[id]}),
            ),
            "revoke",
        );
    };
    let scoped = share(hello, false);
    assert_eq!(raw(port, "GET", &scoped, &[]).body, hello_bytes);
    assert_eq!(
        raw(port, "HEAD", &scoped, &[("if-none-match", tag)]).status,
        304
    );
    // Move the immutable bytes to the configured local store. The attachment and
    // its previously issued grant must keep their identity across storage placement.
    {
        use erp_sdk::blob::{
            AttachmentStore, ContentKey, LocalStore, Namespace, StageLimits, StagingKey,
        };
        let store = LocalStore::open(dir.path().join("attachments"))
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO ir_blob_store (store_key, fingerprint) VALUES ('main',$1)
            ON CONFLICT DO NOTHING",
                &[&store.fingerprint().unwrap()],
            )
            .await
            .unwrap();
        let row = client
            .query_one(
                "SELECT id, namespace::text, fingerprint FROM ir_blob_store
            WHERE store_key='main' AND role='own'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(2), store.fingerprint().unwrap());
        let namespace = Namespace::parse(&row.get::<_, String>(1)).unwrap();
        let staging =
            StagingKey::new(namespace.clone(), Namespace::random().unwrap().as_str()).unwrap();
        let mut bytes = hello_bytes.as_slice();
        let staged = store
            .stage(&staging, &mut bytes, StageLimits::new(2048))
            .await
            .unwrap();
        let key = ContentKey::new(namespace, *staged.sha256());
        store.promote(&staged, &key).await.unwrap();
        let changed = client
            .execute(
                "WITH moved AS (
            UPDATE ir_blob b SET store_id=$2 FROM ir_attachment a
            WHERE a.id=$1 AND b.id=a.blob_id AND b.sha256=$3 RETURNING b.id
        ) DELETE FROM ir_blob_inline WHERE blob_id IN (SELECT id FROM moved)",
                &[&hello, &row.get::<_, i16>(0), &staged.sha256().as_slice()],
            )
            .await
            .unwrap();
        assert_eq!(changed, 1);
        let offloaded = raw(port, "GET", &scoped, &[("range", "bytes=0-3")]);
        assert_eq!(offloaded.status, 206);
        assert_eq!(offloaded.body, hello_bytes[..4]);
        assert_eq!(offloaded.header("etag"), Some(tag));
        assert_eq!(
            offloaded.header("content-type"),
            read.header("content-type")
        );
        assert_eq!(
            offloaded.header("content-disposition"),
            read.header("content-disposition")
        );
        assert_eq!(
            raw(port, "HEAD", &scoped, &[("if-none-match", tag)]).status,
            304
        );
    }
    let token = scoped.split_once('?').unwrap().1;
    assert_eq!(
        raw(port, "GET", &format!("/erp/content/{big}?{token}"), &[]).status,
        404
    );
    let new_link = share(hello, false);
    assert_eq!(raw(port, "GET", &scoped, &[]).status, 404);
    assert_eq!(raw(port, "GET", &new_link, &[]).body, hello_bytes);
    client.execute("UPDATE ir_attachment_content_grant SET expires_at=clock_timestamp() WHERE attachment_id=$1",&[&hello]).await.unwrap();
    assert_eq!(
        raw(port, "HEAD", &new_link, &[("if-none-match", tag)]).status,
        404
    );
    ok(
        &call(
            &admin,
            "/erp/json/2/ir.attachment/write",
            json!({"ids":[hello],"vals":{"public":true}}),
        ),
        "mark shareable",
    );
    let public_path = share(hello, true);
    assert_eq!(raw(port, "GET", &public_path, &[]).body, hello_bytes);
    assert_eq!(get(&portal, &public_path, &[]).body, hello_bytes);
    revoke(hello);
    assert_eq!(raw(port, "GET", &public_path, &[]).status, 404);
    assert_eq!(get(&portal, &public_path, &[]).status, 404);
    let denied_share = call(
        &one,
        "/erp/json/2/ir.attachment/erp_share_content",
        json!({"ids":[hello],"public_read":false,"lifetime_seconds":60}),
    );
    assert_eq!(denied_share.status, 403);
    let posted = raw(
        port,
        "POST",
        &format!("/erp/content/{hello}"),
        &[("cookie", &admin), ("content-length", "0")],
    );
    plain_error(
        &posted,
        405,
        "Only GET and HEAD are served on the content route.",
        "post",
    );
    assert_eq!(posted.header("allow"), Some("GET, HEAD"));

    // one 404 for every refusal, compared on everything but the request id and date
    let readable = get(&one, &format!("/erp/content/{hello}"), &[]);
    assert_eq!(
        readable.status, 200,
        "reader one reads the main company's record"
    );
    let missing = get(&one, &format!("/erp/content/{nonexistent}"), &[]);
    plain_error(&missing, 404, "Not found", "nonexistent id");
    // the access check comes before Range and If-Range: a refused id never answers 206 or 416
    let missing_range = get(
        &one,
        &format!("/erp/content/{nonexistent}"),
        &[("range", "bytes=0-1,4-5")],
    );
    assert_eq!(
        missing_range.comparable(),
        missing.comparable(),
        "range on a missing id"
    );
    for (id, what) in [
        (other_company, "another company's record"),
        (standalone, "a standalone attachment of another user"),
        (deny_listed, "a parent model on the deny list"),
        (no_rights, "a parent model without read rights"),
        (
            settings_field,
            "a field of the parent restricted to a group the caller lacks",
        ),
        (link, "a URL attachment"),
        (blobless, "an attachment without stored content"),
    ] {
        let refused = get(&one, &format!("/erp/content/{id}"), &[]);
        assert_eq!(refused.comparable(), missing.comparable(), "{what}");
        for extra in [
            vec![("if-none-match", "*")],
            vec![("if-match", tag)],
            vec![("range", "bytes=0-1,4-5")],
            vec![("range", "bytes=0-3"), ("if-range", "\"x\"")],
        ] {
            let refused = get(&one, &format!("/erp/content/{id}"), &extra);
            assert_eq!(
                refused.comparable(),
                missing.comparable(),
                "{what} with {extra:?}"
            );
        }
    }
    // the same parent without the restricted field is readable: the 404 above is the field rule
    let settings_read = get(&one, &format!("/erp/content/{settings_plain}"), &[]);
    assert_eq!(settings_read.status, 200, "{}", settings_read.text());
    // the creator of a standalone file reads it
    let own = get(&one, &format!("/erp/content/{own_standalone}"), &[]);
    assert_eq!(own.status, 200, "{}", own.text());
    let portal_missing = get(&portal, &format!("/erp/content/{nonexistent}"), &[]);
    plain_error(&portal_missing, 404, "Not found", "portal, nonexistent id");
    let portal_refused = get(&portal, &format!("/erp/content/{portal_own}"), &[]);
    assert_eq!(
        portal_refused.comparable(),
        portal_missing.comparable(),
        "portal"
    );
    // the second allowed company is readable without any company selection in the URL
    let second_company = get(&two, &format!("/erp/content/{other_company}"), &[]);
    assert_eq!(second_company.status, 200, "{}", second_company.text());
    assert_eq!(second_company.body, hello_bytes);

    // request id in the body (the store-object path is covered by base_attachment_content)
    client
        .execute(
            "DELETE FROM ir_blob_inline WHERE blob_id = (SELECT blob_id FROM ir_attachment WHERE id = $1::int8)",
            &[&lost],
        )
        .await
        .unwrap();
    let gone = get(&admin, &format!("/erp/content/{lost}"), &[]);
    let request_id = gone.header("x-request-id").unwrap().to_owned();
    plain_error(
        &gone,
        500,
        &format!("AttachmentContentMissing {request_id}"),
        "missing object",
    );
    // HEAD finds the same missing bytes at lookup: 500, no body, the attachment class still holds
    let gone_head = raw(
        port,
        "HEAD",
        &format!("/erp/content/{lost}"),
        &[("cookie", &admin)],
    );
    assert_eq!(gone_head.status, 500);
    assert!(gone_head.body.is_empty());
    class_headers(&gone_head, "head missing object");
    let checksum: String = client
        .query_one(
            "SELECT checksum::text FROM ir_attachment WHERE id = $1::int8",
            &[&lost],
        )
        .await
        .unwrap()
        .get(0);
    let log = fs::read_to_string(&stderr).unwrap();
    assert!(
        log.contains("AttachmentContentMissing") && log.contains(&checksum),
        "the operator log names the digest: {log}"
    );
}
