//! The server as deployed stores a file: the real binary, started from a `server.toml` with no
//! `[attachments]` table (the dev deployment's shape), uploads a file for a thread, posts it and
//! loads it back through the HTTP boundary. The test harness of the mail tests attaches its own
//! attachment store, so only this composition proves the server wires one.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/login_rollout.rs"]
mod login_rollout;
#[path = "support/runtime_addons.rs"]
mod runtime_addons;

use erp_testkit::{TestDb, tokio_postgres::config::Host};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn private(path: &Path, value: &str) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn encoded(bytes: &[u8]) -> String {
    let mut result = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            result.push(char::from(*byte));
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

fn url(config: &erp_testkit::tokio_postgres::Config, role: Option<&str>) -> String {
    let Host::Tcp(host) = &config.get_hosts()[0] else {
        panic!("the native fixture requires the configured TCP test cluster")
    };
    let user = role.unwrap_or(config.get_user().unwrap());
    let password = if role.is_some() {
        b"fixture-runtime-password".as_slice()
    } else {
        config.get_password().unwrap()
    };
    format!(
        "postgresql://{}:{}@{}:{}/{}",
        encoded(user.as_bytes()),
        encoded(password),
        host,
        config.get_ports()[0],
        config.get_dbname().unwrap()
    )
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

struct Reply {
    status: u16,
    set_cookies: Vec<String>,
    location: Option<String>,
    body: String,
}

/// One HTTP/1.1 exchange on its own connection (`Connection: close`), read to the end.
fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        wire.push_str(&format!("{name}: {value}\r\n"));
    }
    wire.push_str("\r\n");
    wire.push_str(body);
    stream.write_all(wire.as_bytes()).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap()
        .parse()
        .unwrap();
    let mut set_cookies = Vec::new();
    let mut location = None;
    for line in lines {
        let (name, value) = line.split_once(':').unwrap();
        match name.to_ascii_lowercase().as_str() {
            "set-cookie" => set_cookies.push(value.trim().to_owned()),
            "location" => location = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    Reply {
        status,
        set_cookies,
        location,
        body: body.to_owned(),
    }
}

fn cookie(reply: &Reply, name: &str) -> String {
    reply
        .set_cookies
        .iter()
        .find(|cookie| cookie.starts_with(&format!("{name}=")))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn embedded(reply: &Reply, id: &str) -> serde_json::Value {
    serde_json::from_str(
        reply
            .body
            .split(&format!("id=\"{id}\">"))
            .nth(1)
            .unwrap()
            .split("</script>")
            .next()
            .unwrap(),
    )
    .unwrap()
}

fn await_ready(server: &mut Server, port: u16, stderr: &Path) {
    let until = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            panic!(
                "server exited {status}: {}",
                fs::read_to_string(stderr).unwrap_or_default()
            );
        }
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let wire = format!(
                "GET /erp/health/ready HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            let mut text = String::new();
            if stream.write_all(wire.as_bytes()).is_ok()
                && stream.read_to_string(&mut text).is_ok()
                && text.starts_with("HTTP/1.1 200")
            {
                return;
            }
        }
        assert!(
            Instant::now() < until,
            "server did not become ready: {}",
            fs::read_to_string(stderr).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The server configuration the way the dev deployment writes it: no `[attachments]` table.
fn config(dir: &Path, db: &TestDb, port: u16, role: &str) -> PathBuf {
    private(&dir.join("owner-url"), &url(&db.config(), None));
    private(&dir.join("runtime-url"), &url(&db.config(), Some(role)));
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons");
    let addons = dir.join("runtime-addons");
    runtime_addons::stage_runtime_addons(&source, &addons)
        .expect("stage admitted runtime addons before native server startup");
    let addons = addons.canonicalize().unwrap();
    let text = format!(
        "database='{}'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nbind='127.0.0.1:{port}'\npublic_origin='http://127.0.0.1:{port}'\nproxy_mode='direct'\naddons_path=['{}']\nassets_manifest='manifest.json'\n",
        db.name(),
        addons.display()
    );
    fs::write(dir.join("server.toml"), text).unwrap();
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let assets: Vec<_> = [
        ("js", "text/javascript; charset=utf-8"),
        ("css", "text/css; charset=utf-8"),
    ]
    .into_iter()
    .map(|(ext, mime)| {
        let path = format!("client.{digest}.{ext}");
        fs::write(dir.join(&path), b"").unwrap();
        serde_json::json!({"name": format!("client.{ext}"), "path": path, "sha256": digest, "content_type": mime})
    })
    .collect();
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema": 1, "assets": assets})).unwrap(),
    )
    .unwrap();
    dir.join("server.toml")
}

/// Upload, post and load a file through the real server binary; the store directory appears next to
/// the configuration without any setting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deployed_server_stores_an_uploaded_file_posts_it_and_loads_it_back() {
    let db = TestDb::create().await.unwrap();
    let role = format!("{}_web", db.name());
    let client = db.client().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let config_path = config(dir.path(), &db, port, &role);
    let node_role = format!("{role}_node");
    client
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; \
             CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password'",
            erp_sql::quote(&node_role),
            erp_sql::quote(&role)
        ))
        .await
        .unwrap();
    private(
        &dir.path().join("node-url"),
        &url(&db.config(), Some(&node_role)),
    );
    let text = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        format!("node_database_url_file='node-url'\n{text}"),
    )
    .unwrap();
    private(
        &dir.path().join("password"),
        "fixture-administrator-password",
    );
    let init = Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(["db", "init", "--config"])
        .arg(&config_path)
        .args(["--admin-login", "fixture-admin", "--admin-password-file"])
        .arg(dir.path().join("password"))
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    login_rollout::prepare(&db, &role).await;
    assert!(
        !dir.path().join("attachments").exists(),
        "no store before the server starts"
    );
    let stderr_path = dir.path().join("server.stderr");
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );
    await_ready(&mut server, port, &stderr_path);
    assert!(
        dir.path().join("attachments").is_dir(),
        "the default store directory is created next to server.toml"
    );

    // log in as the administrator the way the browser does
    let login = request(port, "GET", "/erp/login", &[], "");
    assert_eq!(login.status, 200);
    let prefs = cookie(&login, "prefs");
    let csrf = embedded(&login, "erp-login")["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let logged = request(
        port,
        "POST",
        "/erp/login",
        &[
            ("cookie", &prefs),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &format!(
            "db={}&login=fixture-admin&password=fixture-administrator-password&redirect=%2Ferp&csrf_token={csrf}",
            db.name()
        ),
    );
    assert_eq!(logged.status, 303, "{}", logged.body);
    assert_eq!(logged.location.as_deref(), Some("/erp"));
    let cookies = format!("{prefs}; {}", cookie(&logged, "session_id"));
    let call = |method_path: &str, body: &str| {
        request(
            port,
            "POST",
            method_path,
            &[
                ("cookie", &cookies),
                ("content-type", "application/json"),
                ("sec-fetch-site", "same-origin"),
            ],
            body,
        )
    };

    let shell = request(port, "GET", "/erp", &[("cookie", &cookies)], "");
    assert_eq!(shell.status, 200, "{}", shell.body);
    let boot = embedded(&shell, "erp-bootstrap");
    assert_eq!(boot["attachment_limits"]["max_file_bytes"], 134217728);
    assert_eq!(boot["attachment_limits"]["inline_max_bytes"], 2048);
    let policy = call("/erp/json/2/ir.attachment/erp_upload_limits", "{}");
    assert_eq!(policy.status, 200, "{}", policy.body);
    let policy: serde_json::Value = serde_json::from_str(&policy.body).unwrap();
    assert_eq!(policy, boot["attachment_limits"]);

    let install = call("/erp/module/install", r#"{"modules":["mail"]}"#);
    assert_eq!(install.status, 200, "install: {}", install.body);
    // a deployment installs its addons and restarts: serve from the installed registry
    drop(server);
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );
    await_ready(&mut server, port, &stderr_path);
    let partner: i64 = client
        .query_one(
            "SELECT partner_id::int8 FROM res_users WHERE login = 'fixture-admin'",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    // the request shapes the chatter client sends
    let upload = call(
        "/erp/json/2/res.partner/thread_upload",
        &format!(
            r#"{{"ids":[{partner}],"name":"hello.txt","mimetype":"text/plain","data":"aGVsbG8gd29ybGQ="}}"#
        ),
    );
    assert_eq!(
        upload.status,
        200,
        "thread_upload: {} install: {} stderr: {}",
        upload.body,
        install.body,
        fs::read_to_string(&stderr_path).unwrap_or_default()
    );
    let upload: serde_json::Value = serde_json::from_str(&upload.body).unwrap();
    let file = upload["attachment_id"].as_i64().unwrap();
    assert_eq!(upload["file_size"], 11);
    let posted = call(
        "/erp/json/2/res.partner/thread_post",
        &format!(
            r#"{{"ids":[{partner}],"post":{{"body":"with a file","attachment_ids":[{file}]}}}}"#
        ),
    );
    assert_eq!(posted.status, 200, "thread_post: {}", posted.body);
    let message = serde_json::from_str::<serde_json::Value>(&posted.body).unwrap()["message_id"]
        .as_i64()
        .unwrap();
    let loaded = call(
        "/erp/json/2/mail.store/store_fetch",
        &format!(
            r#"{{"wire":1,"topics":[{{"name":"thread","params":{{"model":"res.partner","id":{partner},"messages":true,"attachments":true}}}}]}}"#
        ),
    );
    assert_eq!(loaded.status, 200, "store_fetch: {}", loaded.body);
    let loaded: serde_json::Value = serde_json::from_str(&loaded.body).unwrap();
    let topic = loaded["topics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["topic"] == "thread")
        .unwrap();
    assert_eq!(topic["ok"], true, "{topic}");
    assert_eq!(
        topic["data"]["attachments"]["attachment_ids"],
        serde_json::json!([file])
    );
    let row = client
        .query_one(
            "SELECT res_model::text, res_id::int8 FROM ir_attachment WHERE id = $1::int8",
            &[&file],
        )
        .await
        .unwrap();
    assert_eq!(
        (row.get::<_, String>(0), row.get::<_, i64>(1)),
        ("res.partner".to_owned(), partner)
    );
    let linked: i64 = client
        .query_one(
            "SELECT count(*)::int8 FROM message_attachment_rel WHERE message_id = $1::int8 AND attachment_id = $2::int8",
            &[&message, &file],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(linked, 1);
    // Adopter-boundary fixture: a ready inline intent with digests computed by our blob code.
    // This tests the real server's staged RPC adoption and request-role grants, not HTTP staging.
    let staged_bytes = b"native staged attachment".to_vec();
    let (sha256, sha1) = erp_sdk::blob::digests(&staged_bytes);
    let size = i64::try_from(staged_bytes.len()).unwrap();
    let staged: String = client.query_one(
        "INSERT INTO ir_blob_upload(state,subject_user,actor,expires_at,sha256,sha1,size,detected_mimetype,inline_data,reserved_bytes) SELECT 1,id,'',now()+interval '1 hour',$1,$2,$3,'text/plain',$4,$3 FROM res_users WHERE login='fixture-admin' RETURNING id::text",
        &[&sha256.to_vec(), &sha1.to_vec(), &size, &staged_bytes]
    ).await.unwrap().get(0);
    let response = call("/erp/json/2/res.partner/thread_post", &serde_json::json!({"ids":[partner],"post":{"body":"staged native", "staged_uploads":[{"name":"native.txt", "upload_id":staged}]}}).to_string());
    assert_eq!(response.status, 200, "{}", response.body);
    let posted: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    let row = client.query_one("SELECT a.file_size::int8,a.checksum::text,u.state::int8 FROM message_attachment_rel r JOIN ir_attachment a ON a.id=r.attachment_id CROSS JOIN ir_blob_upload u WHERE r.message_id=$1 AND u.id=$2::text::uuid", &[&posted["message_id"].as_i64().unwrap(),&staged]).await.unwrap();
    assert_eq!(row.get::<_, i64>(0), size);
    assert_eq!(row.get::<_, String>(1), erp_sdk::blob::to_hex(&sha1));
    assert_eq!(row.get::<_, i64>(2), 2);
}
