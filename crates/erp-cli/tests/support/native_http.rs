#![allow(dead_code, reason = "shared HTTP test fixture helpers")]
#[path = "login_rollout.rs"]
pub mod login_rollout;
use erp_testkit::{TestDb, tokio_postgres::config::Host};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub fn private(path: &Path, value: &str) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
pub fn encoded(bytes: &[u8]) -> String {
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
pub fn url(config: &erp_testkit::tokio_postgres::Config, role: Option<&str>) -> String {
    let Host::Tcp(host) = &config.get_hosts()[0] else {
        panic!("native HTTP fixture requires the configured TCP test cluster")
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
pub struct Server(Child);
impl Server {
    pub fn stop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}
pub struct Reply {
    pub status: u16,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: String,
}
pub fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
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
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').unwrap();
        headers
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.trim().to_owned());
    }
    Reply {
        status,
        headers,
        body: body.to_owned(),
    }
}
pub fn cookie(reply: &Reply, name: &str) -> String {
    reply.headers["set-cookie"]
        .iter()
        .find(|cookie| cookie.starts_with(&format!("{name}=")))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}
pub fn embedded(reply: &Reply, id: &str) -> serde_json::Value {
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

pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        // Build output and installed JavaScript packages (symlinks) are not addon sources.
        if entry.file_name() == "node_modules" || entry.file_name() == "target" {
            continue;
        }
        let kind = entry.file_type().unwrap();
        assert!(
            !kind.is_symlink(),
            "fixture source must not contain links: {}",
            entry.path().display()
        );
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            assert!(
                kind.is_file(),
                "fixture source must contain regular files: {}",
                entry.path().display()
            );
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

pub fn config(dir: &Path, db: &TestDb, port: u16, role: &str, addons: &Path) -> PathBuf {
    private(&dir.join("owner-url"), &url(&db.config(), None));
    private(&dir.join("runtime-url"), &url(&db.config(), Some(role)));
    private(
        &dir.join("node-url"),
        &url(&db.config(), Some(&format!("{role}_node"))),
    );
    let text = format!(
        "database='{}'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nnode_database_url_file='node-url'\nbind='127.0.0.1:{port}'\npublic_origin='http://127.0.0.1:{port}'\nproxy_mode='direct'\naddons_path=['{}']\nassets_manifest='manifest.json'\n",
        db.name(),
        addons.display()
    );
    fs::write(dir.join("server.toml"), text).unwrap();
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let assets: Vec<_> = [("js", "text/javascript; charset=utf-8"), ("css", "text/css; charset=utf-8")]
        .into_iter()
        .map(|(ext, mime)| {
            let path = format!("client.{digest}.{ext}");
            fs::write(dir.join(&path), b"").unwrap();
            serde_json::json!({"name":format!("client.{ext}"),"path":path,"sha256":digest,"content_type":mime})
        })
        .collect();
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema":1,"assets":assets})).unwrap(),
    )
    .unwrap();
    dir.join("server.toml")
}

pub fn json_post(port: u16, cookies: &str, path: &str, body: &str) -> Reply {
    request(
        port,
        "POST",
        path,
        &[
            ("cookie", cookies),
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
        ],
        body,
    )
}

pub fn catalogue(port: u16, cookies: &str) -> serde_json::Value {
    let reply = json_post(
        port,
        cookies,
        "/erp/json/2/ir.module.module/erp_available_addons",
        "{}",
    );
    assert_eq!(reply.status, 200, "catalogue: {}", reply.body);
    serde_json::from_str(&reply.body).unwrap()
}

pub fn state_of(catalogue: &serde_json::Value, name: &str) -> Option<String> {
    catalogue["addons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|addon| addon["name"] == name)
        .map(|addon| addon["state"].as_str().unwrap().to_owned())
}

pub struct NativeFixture {
    pub db: TestDb,
    pub client: erp_testkit::deadpool_postgres::Client,
    pub server: Server,
    pub dir: tempfile::TempDir,
    pub role: String,
    pub port: u16,
    pub cookies: String,
    pub stderr_path: PathBuf,
}

/// A second native server process sharing the fixture's durable database and addon sources.
/// Drop it before cleaning up the owning fixture database.
pub struct NativePeer {
    pub server: Server,
    pub dir: tempfile::TempDir,
    pub port: u16,
}

impl NativeFixture {
    pub fn peer(&self) -> NativePeer {
        let temporary = std::env::temp_dir().canonicalize().unwrap();
        let dir = tempfile::tempdir_in(temporary).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let config_path = config(
            dir.path(),
            &self.db,
            port,
            &self.role,
            &self.dir.path().join("addons"),
        );
        let text = fs::read_to_string(&config_path).unwrap();
        let original_origin = format!("public_origin='http://127.0.0.1:{port}'");
        assert!(
            text.contains(&original_origin),
            "peer config lost its origin setting"
        );
        let mut text = text.replace(
            &original_origin,
            &format!("public_origin='http://127.0.0.1:{}'", self.port),
        );
        text.push_str("node_name='panel-peer'\n");
        fs::write(&config_path, text).unwrap();
        let stderr_path = dir.path().join("server.stderr");
        let mut server = Server(
            Command::new(env!("CARGO_BIN_EXE_erp"))
                .args(["server", "--config"])
                .arg(config_path)
                .stdout(Stdio::null())
                .stderr(fs::File::create(&stderr_path).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                let ready = request(port, "GET", "/erp/health/ready", &[], "");
                match ready.status {
                    200 => break,
                    503 => {}
                    status => panic!("peer readiness returned HTTP {status}"),
                }
            }
            if let Some(status) = server.0.try_wait().unwrap() {
                panic!(
                    "peer exited {status}: {}",
                    fs::read_to_string(&stderr_path).unwrap()
                );
            }
            assert!(Instant::now() < deadline, "peer did not become available");
            std::thread::sleep(Duration::from_millis(100));
        }
        NativePeer { server, dir, port }
    }

    pub async fn start() -> Self {
        Self::start_with_addons(|_| {}).await
    }

    /// Prepare additional fixture addons before the native binary discovers its catalogue.
    pub async fn start_with_addons(prepare: impl FnOnce(&Path)) -> Self {
        Self::start_with_assets(prepare, None).await
    }

    /// Explicit compiled-asset fixture; ordinary native HTTP fixtures keep their empty assets.
    pub async fn start_with_assets(prepare: impl FnOnce(&Path), assets: Option<&Path>) -> Self {
        Self::start_full(prepare, assets, "").await
    }

    /// As [`Self::start`], with `extra` lines appended to the server's configuration file.
    pub async fn start_with_config(extra: &str) -> Self {
        Self::start_full(|_| {}, None, extra).await
    }

    async fn start_full(prepare: impl FnOnce(&Path), assets: Option<&Path>, extra: &str) -> Self {
        let db = TestDb::create().await.unwrap();
        let role = format!("{}_web", db.name());
        let client = db.client().await.unwrap();
        // The configured search prefix is trusted; keep each admitted addon root a real directory.
        let temporary = std::env::temp_dir().canonicalize().unwrap();
        let dir = tempfile::tempdir_in(temporary).unwrap();
        // A private copy of the release addon directory, plus one folder with native code that this
        // binary does not link: it must be refused, not listed.
        let addons = dir.path().join("addons");
        copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("addons"),
            &addons,
        );
        prepare(&addons);
        let unlinked = addons.join("unlinked_native");
        fs::create_dir_all(unlinked.join("src")).unwrap();
        fs::write(
            unlinked.join("manifest.toml"),
            "name='Unlinked Native'\ndepends=['base']\n",
        )
        .unwrap();
        fs::write(
            unlinked.join("Cargo.toml"),
            "[package]\nname='unlinked_native'\n",
        )
        .unwrap();
        fs::write(unlinked.join("src/lib.rs"), "").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let config_path = config(dir.path(), &db, port, &role, &addons);
        if !extra.is_empty() {
            let mut text = fs::read_to_string(&config_path).unwrap();
            text.push_str(extra);
            text.push('\n');
            fs::write(&config_path, text).unwrap();
        }
        if let Some(assets) = assets {
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(assets.join("manifest.json")).unwrap()).unwrap();
            for asset in manifest["assets"].as_array().unwrap() {
                let name = asset["path"].as_str().unwrap();
                assert!(Path::new(name).components().count() == 1 && name.starts_with("client."));
                fs::copy(assets.join(name), dir.path().join(name)).unwrap();
            }
            fs::copy(
                assets.join("manifest.json"),
                dir.path().join("manifest.json"),
            )
            .unwrap();
        }
        private(
            &dir.path().join("password"),
            "fixture-administrator-password",
        );
        client
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password'; CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",
            erp_sql::quote(&role),
            erp_sql::quote(&format!("{role}_node"))
        ))
        .await
        .unwrap();
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
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                let ready = request(port, "GET", "/erp/health/ready", &[], "");
                match ready.status {
                    200 => break,
                    // Membership settles before the listener admits ordinary requests.
                    503 => {}
                    status => panic!("server readiness returned HTTP {status}"),
                }
            }
            if let Some(status) = server.0.try_wait().unwrap() {
                panic!(
                    "server exited {status}: {}",
                    fs::read_to_string(&stderr_path).unwrap()
                );
            }
            assert!(Instant::now() < deadline, "server did not become available");
            std::thread::sleep(Duration::from_millis(100));
        }
        let login = request(port, "GET", "/erp/login", &[], "");
        let prefs = cookie(&login, "prefs");
        let login_data = embedded(&login, "erp-login");
        let form = format!(
            "db={}&login=fixture-admin&password=fixture-administrator-password&csrf_token={}",
            db.name(),
            login_data["csrf"].as_str().unwrap()
        );
        let logged = request(
            port,
            "POST",
            "/erp/login",
            &[
                ("cookie", &prefs),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            &form,
        );
        assert_eq!(logged.status, 303);
        let cookies = format!("{prefs}; {}", cookie(&logged, "session_id"));

        Self {
            db,
            client,
            server,
            dir,
            role,
            port,
            cookies,
            stderr_path,
        }
    }
    pub async fn cleanup(self) {
        let Self {
            server,
            client,
            db,
            role,
            dir,
            ..
        } = self;
        drop(server);
        drop(dir);
        drop(client);
        let mut admin = db.config();
        admin.dbname("postgres");
        db.cleanup().await.unwrap();
        let (client, connection) = admin
            .connect(erp_testkit::tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move { connection.await.unwrap() });
        client
            .batch_execute(&format!(
                "DROP ROLE {}, {}, {}",
                erp_sql::quote(&role),
                erp_sql::quote(&format!("{role}_node")),
                erp_sql::quote(&login_rollout::role(&role))
            ))
            .await
            .unwrap();
    }
}
