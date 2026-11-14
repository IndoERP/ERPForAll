//! Runtime proof of WebAssembly addons: the real `erp` binary installs, upgrades and
//! uninstalls `.erpaddon` packages built from `examples/wasm/sample_library_wasm` and
//! `examples/wasm/sample_library_wasm_policy` into a database that a node is serving, without a restart.
//!
//! - `a_running_server_follows_the_operator_without_a_restart`: a real server process (HTTP), the
//!   operator command in another process, the registry swap observed through the server's own
//!   endpoints, then a restarted server that loads the stored package at start-up.
//! - `operator_installs_two_wasm_addons_and_a_second_node_serves_their_methods`: the operator command
//!   in one process, a second node in this one that loads from the database and calls the methods
//!   through the ORM: the methods of one package, an override with `super` from a second package
//!   (the rule that refuses a renewal is a validation error), the previous ORM unchanged, upgrade,
//!   uninstall and the audit trail.
//! - `the_operator_command_refuses_what_was_not_approved`: the refusals, with nothing written.
//!
//! The tests are `#[ignore]`d in the workspace run and fail, never skip, without the packages:
//! `scripts/wasm-poc.sh` builds them with `erp addon build --target wasm` and `erp addon package`
//! and sets `ERP_POC_PACKAGES` to the directory that holds them.
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

use erp_cli::node::Node;
use erp_fields::{Cell, Date, Value};
use erp_modules::ModuleName;
use erp_orm::{Context, Orm, RawVals, SUPERUSER_ID, Signal, call_method_json};
use erp_testkit::{TestDb, tokio_postgres::config::Host};
use std::sync::Arc;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
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
    headers: BTreeMap<String, Vec<String>>,
    body: String,
}
fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
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
fn cookie(reply: &Reply, name: &str) -> String {
    assert!(
        reply.headers.contains_key("set-cookie"),
        "missing Set-Cookie: status={} body={}",
        reply.status,
        reply.body
    );
    reply.headers["set-cookie"]
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

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        // Build output and installed JavaScript packages (symlinks) are not addon sources.
        if entry.file_name() == "node_modules" || entry.file_name() == "target" {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn config(dir: &Path, db: &TestDb, port: u16, role: &str, addons: &Path) -> PathBuf {
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

fn json_post(port: u16, cookies: &str, path: &str, body: &str) -> Reply {
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

// ---- the packages and the operator command ----------------------------------------------------

fn package(name: &str) -> PathBuf {
    let dir = std::env::var("ERP_POC_PACKAGES").unwrap_or_else(|_| {
        panic!("ERP_POC_PACKAGES must name the directory scripts/wasm-poc.sh fills")
    });
    let path = Path::new(&dir).join(format!("{name}.erpaddon"));
    assert!(path.is_file(), "{} is missing", path.display());
    path
}

fn erp(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_erp"))
        .args(args)
        .output()
        .unwrap()
}

fn operator(verb: &str, config: &Path, db: &str, file: &Path, extra: &[&str]) -> Output {
    let mut args: Vec<&std::ffi::OsStr> = vec!["addon".as_ref(), verb.as_ref(), file.as_os_str()];
    args.extend([
        "-d".as_ref(),
        db.as_ref(),
        "--config".as_ref(),
        config.as_os_str(),
    ]);
    args.extend(extra.iter().map(|a| std::ffi::OsStr::new(*a)));
    erp(&args)
}

fn uninstall(config: &Path, db: &str, name: &str) -> Output {
    erp(&[
        "addon".as_ref(),
        "uninstall".as_ref(),
        name.as_ref(),
        "-d".as_ref(),
        db.as_ref(),
        "--config".as_ref(),
        config.as_os_str(),
    ])
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

/// The one JSON line a successful operator command prints.
fn result_of(out: &Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "exit {:?}: {}",
        out.status.code(),
        text(&out.stderr)
    );
    let stdout = text(&out.stdout);
    let line = stdout.lines().last().unwrap();
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not a JSON line ({e}): {stdout}"))
}

fn refused(out: &Output, code: i32, needle: &str) {
    assert_eq!(out.status.code(), Some(code), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains(needle),
        "expected '{needle}' in: {}",
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).trim().is_empty(),
        "no result line on a refusal"
    );
}

async fn create_role(db: &TestDb, role: &str) {
    db.client()
        .await
        .unwrap()
        .batch_execute(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password'; CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",
            erp_sql::quote(role),
            erp_sql::quote(&format!("{role}_node"))
        ))
        .await
        .unwrap();
}

async fn drop_role(db: TestDb, role: &str) {
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
            "DROP ROLE {}, {}; DROP ROLE IF EXISTS {}",
            erp_sql::quote(role),
            erp_sql::quote(&format!("{role}_node")),
            erp_sql::quote(&login_rollout::role(role))
        ))
        .await
        .unwrap();
}

/// A private copy of the release addon directory (`addons/`). The sample sources live in
/// `examples/wasm`, outside it; they are copied in as well, as a careless deployment would, to show
/// that a dev folder with a `[wasm]` section never competes with the installed package.
fn deployed_addons(dir: &Path) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let addons = dir.join("addons");
    copy_dir(&root.join("addons"), &addons);
    for source in ["sample_library_wasm", "sample_library_wasm_policy"] {
        copy_dir(
            &root.join("examples/wasm").join(source),
            &addons.join(source),
        );
    }
    addons
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn initialize(config: &Path, dir: &Path) {
    private(&dir.join("password"), "fixture-administrator-password");
    let init = erp(&[
        "db".as_ref(),
        "init".as_ref(),
        "--config".as_ref(),
        config.as_os_str(),
        "--admin-login".as_ref(),
        "fixture-admin".as_ref(),
        "--admin-password-file".as_ref(),
        dir.join("password").as_os_str(),
    ]);
    assert!(init.status.success(), "{}", text(&init.stderr));
    login_rollout::prepare_config(config);
}

fn start_server(config: &Path, port: u16, stderr_path: &Path) -> Server {
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(config)
            .stdout(Stdio::null())
            .stderr(fs::File::create(stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            let login = request(port, "GET", "/erp/login", &[], "");
            if login.status == 200 {
                return server;
            }
            assert_eq!(
                login.status, 503,
                "unexpected startup response: {}",
                login.body
            );
        }
        if let Some(status) = server.0.try_wait().unwrap() {
            panic!(
                "server exited {status}: {}",
                fs::read_to_string(stderr_path).unwrap()
            );
        }
        assert!(Instant::now() < deadline, "server did not become available");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn login(port: u16, db: &str) -> String {
    login_as(port, db, "fixture-admin", "fixture-administrator-password")
}

fn login_as(port: u16, db: &str, login: &str, password: &str) -> String {
    let page = request(port, "GET", "/erp/login", &[], "");
    let prefs = cookie(&page, "prefs");
    let data = embedded(&page, "erp-login");
    let form = format!(
        "db={db}&login={login}&password={password}&csrf_token={}",
        data["csrf"].as_str().unwrap()
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
    format!("{prefs}; {}", cookie(&logged, "session_id"))
}

fn create_book(port: u16, cookies: &str, vals: &str) -> Reply {
    json_post(
        port,
        cookies,
        "/erp/json/2/sample_library.book/create",
        &format!(r#"{{"vals_list":[{vals}]}}"#),
    )
}

/// Ask `attempt` until it answers 200 (a server that is rebuilding its registry answers the
/// requests of that moment with a retryable error) or `what` times out.
fn eventually_ok(what: &str, mut attempt: impl FnMut() -> Reply) -> Reply {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = attempt();
        if reply.status == 200 {
            return reply;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: still {} after 30 s: {}",
            reply.status,
            reply.body
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn eventually_refused(what: &str, mut attempt: impl FnMut() -> Reply) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = attempt();
        if reply.status != 200 {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: still served after 30 s");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Runs `body` on a thread with a large stack: a full install chain exceeds the default one.
fn big_stack(body: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(body);
        })
        .unwrap()
        .join()
        .unwrap();
}

// ---- 1. a running server -----------------------------------------------------------------------

/// stored packages at start-up; no LISTEN-delivery or cache-key tuple proof)
#[test]
#[ignore = "needs the packages built by scripts/wasm-poc.sh (ERP_POC_PACKAGES)"]
fn a_running_server_follows_the_operator_without_a_restart() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let role = format!("{}_web", db.name());
        create_role(&db, &role).await;
        let client = db.client().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let addons = deployed_addons(dir.path());
        let port = free_port();
        let config = config(dir.path(), &db, port, &role, &addons);
        initialize(&config, dir.path());
        let stderr_path = dir.path().join("server.stderr");
        let server = start_server(&config, port, &stderr_path);
        let cookies = login(port, db.name());
        let installed = json_post(
            port,
            &cookies,
            "/erp/module/install",
            r#"{"modules":["sample_library"]}"#,
        );
        assert_eq!(installed.status, 200, "{}", installed.body);

        // the source folder of a package addon is on this server's addon path (see
        // `deployed_addons`): the browser cannot install it as a native addon
        let browser = json_post(
            port,
            &cookies,
            "/erp/module/install",
            r#"{"modules":["sample_library_wasm"]}"#,
        );
        assert_ne!(browser.status, 200, "{}", browser.body);
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM ir_module_module WHERE name = 'sample_library_wasm' AND state <> 'uninstalled'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );

        // before: the book has no `times_lent`
        let before = create_book(port, &cookies, r#"{"title":"Before","times_lent":3}"#);
        assert_ne!(before.status, 200, "{}", before.body);

        // the operator installs the package from another process
        let done = result_of(&operator(
            "install",
            &config,
            db.name(),
            &package("sample_library_wasm"),
            &["--approve", "none"],
        ));
        assert_eq!(done["operation"], "install");
        assert_eq!(done["addon"], "sample_library_wasm");
        assert_eq!(
            done["installed"],
            serde_json::json!(["sample_library_wasm"])
        );
        assert_eq!(done["runtime_grants"], "published");

        // the same server, no restart: the field exists and is stored
        eventually_ok("the new field after install", || {
            create_book(port, &cookies, r#"{"title":"After","times_lent":3}"#)
        });
        assert_eq!(
            client
                .query_one(
                    "SELECT times_lent FROM sample_library_book WHERE title='After'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, i32>(0),
            3
        );

        // a release activation upgrades the native modules with an addon path that never holds the
        // package's source (production deploys only `addons/`): the installed package is loaded from
        // the database, so it is a known module, and the upgrade leaves it out of the modules it
        // upgrades (a package changes only through the package lifecycle), instead of refusing it
        // as unavailable
        let package_snapshot_sql = "SELECT jsonb_build_object(
            'modules', (SELECT jsonb_agg(jsonb_build_array(name, state, latest_version) ORDER BY name)
                FROM ir_module_module WHERE name = 'sample_library_wasm'),
            'packages', (SELECT jsonb_agg(jsonb_build_array(id, name, sha256, md5(package_data)) ORDER BY id) FROM ir_module_package),
            'audit', (SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM ir_module_wasm_audit a)
        )::text";
        let package_before: String = client
            .query_one(package_snapshot_sql, &[])
            .await
            .unwrap()
            .get(0);
        let release_addons = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons");
        let upgrade = Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["module", "upgrade", "-d", db.name(), "--addons-path"])
            .arg(&release_addons)
            .args(["base", "sample_library"])
            .env("DATABASE_URL", url(&db.config(), None))
            .output()
            .unwrap();
        assert!(
            upgrade.status.success(),
            "module upgrade with an installed package: {}",
            text(&upgrade.stderr)
        );
        assert_eq!(
            client
                .query_one(package_snapshot_sql, &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            package_before,
            "compatible native upgrade preserves package version, archive and lifecycle audit"
        );
        let report = text(&upgrade.stdout);
        assert!(
            report.contains("sample_library") && !report.contains("sample_library_wasm"),
            "the native modules are upgraded and the package addon is left as it is: {report}"
        );
        assert_eq!(
            client
                .query_one(
                    "SELECT state FROM ir_module_module WHERE name = 'sample_library_wasm'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, String>(0),
            "installed"
        );
        eventually_ok("the package field after a native upgrade", || {
            create_book(
                port,
                &cookies,
                r#"{"title":"After upgrade","times_lent":1}"#,
            )
        });

        // A native upgrade cannot commit a parent arch that breaks the installed package's
        // XPath. The release path has no package sources; only the database carries its views.
        let release_copy = dir.path().join("release-addons");
        copy_dir(&release_addons, &release_copy);
        // a changed data-only addon needs an explicit previous release whose
        // admitted bytes match the installed identity. This is an isolated synthetic release
        // receipt, not a claim about repository/deployment provenance.
        let previous_release = fs::canonicalize(dir.path())
            .unwrap()
            .join("previous-release");
        let mut hashes = BTreeMap::new();
        let mut identities = BTreeMap::new();
        // The previous release authenticates every installed native addon, not just the one
        // being changed. Packages authenticate their currently stored archive separately.
        for row in client
            .query(
                "SELECT name, content_hash FROM installed_addon_identity WHERE source_kind <> 'package' ORDER BY name",
                &[],
            )
            .await
            .unwrap()
        {
            let name: String = row.get(0);
            let previous_addon = previous_release.join("addons").join(&name);
            copy_dir(&release_addons.join(&name), &previous_addon);
            let previous = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                &name,
                &previous_addon,
            )
            .unwrap();
            assert_eq!(previous.digest(), row.get::<_, String>(1));
            for (path, bytes) in previous.files() {
                hashes.insert(
                    format!("addons/{name}/{path}"),
                    erp_modules::package::sha256_hex(bytes),
                );
            }
            identities.insert(name, previous.digest().to_owned());
        }
        let fixture_source = "0".repeat(40);
        fs::write(previous_release.join("REVISION"), &fixture_source).unwrap();
        fs::write(
            previous_release.join("PACKAGE.json"),
            serde_json::to_vec(&serde_json::json!({
                "source": fixture_source,
                "sha256": hashes,
                "addon_identities": identities,
            }))
            .unwrap(),
        )
        .unwrap();
        let views_path = release_copy.join("sample_library/views/sample_library_views.xml");
        let original = fs::read_to_string(&views_path).unwrap();
        let broken = original.replace("<field name=\"available\"/>", "");
        assert_ne!(
            broken, original,
            "fixture must remove the package's XPath target"
        );
        fs::write(&views_path, broken).unwrap();
        let snapshot_sql = "SELECT jsonb_build_object(
            'views', (SELECT jsonb_agg(jsonb_build_array(id, arch_db) ORDER BY id) FROM ir_ui_view),
            'modules', (SELECT jsonb_agg(jsonb_build_array(name, state, latest_version) ORDER BY name) FROM ir_module_module),
            'packages', (SELECT jsonb_agg(jsonb_build_array(id, name, sha256, md5(package_data)) ORDER BY id) FROM ir_module_package),
            'audit', (SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM ir_module_wasm_audit a),
            'identities', (SELECT jsonb_agg(to_jsonb(i) ORDER BY name) FROM installed_addon_identity i),
            'identity_audit', (SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM installed_addon_identity_audit a),
            'generation', (SELECT jsonb_agg(to_jsonb(g)) FROM erp_signal_generation g)
        )::text";
        let snapshot: String = client.query_one(snapshot_sql, &[]).await.unwrap().get(0);
        let native_upgrade = || {
            Command::new(env!("CARGO_BIN_EXE_erp"))
                .args(["module", "upgrade", "-d", db.name(), "--addons-path"])
                .arg(&release_copy)
                .arg("--identity-previous")
                .arg(&previous_release)
                .arg("sample_library")
                .env("DATABASE_URL", url(&db.config(), None))
                .output()
                .unwrap()
        };
        // Both native book views lose the target; whichever is checked first must identify
        // the exact package extension, rather than merely mentioning the package name.
        let names_package_view = |diagnostic: &str| {
            [
                "sample_library_wasm.book_form_lend",
                "sample_library_wasm.book_list_lend",
            ]
            .iter()
            .any(|xml_id| diagnostic.contains(xml_id))
        };
        let rejected = native_upgrade();
        // A validation refusal is exit 1 at the final check or exit 4 when a data write's
        // validator catches it earlier (ModuleOperation::StepFailed). Dynamic view ids and
        // source paths prevent a whole diagnostic comparison; require the stable reason/owner.
        assert!(
            matches!(rejected.status.code(), Some(1 | 4)),
            "{}",
            text(&rejected.stderr)
        );
        assert!(text(&rejected.stdout).trim().is_empty());
        assert!(
            text(&rejected.stderr).contains("cannot be located in parent view"),
            "{}",
            text(&rejected.stderr)
        );
        assert!(
            names_package_view(&text(&rejected.stderr)),
            "{}",
            text(&rejected.stderr)
        );
        assert_eq!(
            client
                .query_one(snapshot_sql, &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            snapshot,
            "refusal rolls back views, catalog, package archive/audit and registry generation"
        );
        let intact = form_views(port, &cookies, "sample_library.book");
        assert_eq!(intact.status, 200, "{}", intact.body);
        assert!(intact.body.contains("times_lent"), "{}", intact.body);

        // A repair of the native source succeeds on retry, with the same installed package.
        fs::write(&views_path, original).unwrap();
        // End migrations run after normal data checks. SQL can therefore invalidate the
        // parent without going through the view write validator: the final check must catch it.
        let manifest_path = release_copy.join("sample_library/manifest.toml");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        let parsed = erp_modules::Manifest::from_toml("sample_library", &manifest).unwrap();
        let installed_version = parsed.version().semver().unwrap();
        let next_version = erp_modules::version::SemverVersion::new(
            installed_version.major,
            installed_version.minor.checked_add(1).unwrap(),
            0,
        );
        assert!(next_version > *installed_version);
        let upgraded_manifest = manifest.replace(
            &format!("version = \"{}\"", parsed.version().raw()),
            &format!("version = \"{next_version}\""),
        );
        assert_ne!(manifest, upgraded_manifest);
        fs::write(&manifest_path, upgraded_manifest).unwrap();
        let migration_dir = release_copy.join("sample_library/migrations/0.0.0");
        fs::create_dir_all(&migration_dir).unwrap();
        let migration = migration_dir.join("end-break-package-view.sql");
        fs::write(
            &migration,
            r#"UPDATE ir_ui_view SET arch_db = replace(arch_db, '<field name="available"/>', '')
               WHERE id IN (SELECT res_id FROM ir_model_data
                 WHERE module = 'sample_library' AND model = 'ir.ui.view');"#,
        )
        .unwrap();
        let rejected = native_upgrade();
        let native_targets: i64 = client
            .query_one(
                r#"SELECT count(*) FROM ir_ui_view v JOIN ir_model_data md ON md.res_id = v.id
                   WHERE md.module = 'sample_library' AND md.model = 'ir.ui.view'
                     AND v.arch_db LIKE '%<field name="available"/>%'"#,
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            rejected.status.code(),
            Some(1),
            "end-migration refusal: remaining native targets={native_targets} stdout={} stderr={}",
            text(&rejected.stdout),
            text(&rejected.stderr)
        );
        assert!(text(&rejected.stdout).trim().is_empty());
        assert!(
            text(&rejected.stderr).contains("cannot be located in parent view")
                && names_package_view(&text(&rejected.stderr)),
            "{}",
            text(&rejected.stderr)
        );
        assert_eq!(
            client
                .query_one(snapshot_sql, &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            snapshot,
            "end-migration refusal rolls back the complete operation"
        );
        let intact = form_views(port, &cookies, "sample_library.book");
        assert_eq!(intact.status, 200, "{}", intact.body);
        assert!(intact.body.contains("times_lent"), "{}", intact.body);
        fs::remove_file(migration).unwrap();
        let repaired = native_upgrade();
        assert!(repaired.status.success(), "{}", text(&repaired.stderr));
        let intact = eventually_ok("package view after repaired native upgrade", || {
            form_views(port, &cookies, "sample_library.book")
        });
        assert!(intact.body.contains("times_lent"), "{}", intact.body);

        // the operator removes it: the server stops accepting the field, and keeps the data it had
        let removed = result_of(&uninstall(&config, db.name(), "sample_library_wasm"));
        assert_eq!(
            removed["uninstalled"],
            serde_json::json!(["sample_library_wasm"])
        );
        eventually_refused("the field after uninstall", || {
            create_book(port, &cookies, r#"{"title":"Gone","times_lent":1}"#)
        });
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM sample_library_book", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            2,
            "the books created while the addon was installed stay"
        );

        // installed again, then the server restarts: the stored package is loaded at start-up
        result_of(&operator(
            "install",
            &config,
            db.name(),
            &package("sample_library_wasm"),
            &["--approve", "none"],
        ));
        eventually_ok("the field after the second install", || {
            create_book(port, &cookies, r#"{"title":"Again","times_lent":4}"#)
        });
        // OPS.WASM-UPGRADE-503: warm node, actual operator upgrade, then first safe
        // login navigation. No polling away the stale response and no server restart.
        let upgraded = result_of(&operator(
            "upgrade",
            &config,
            db.name(),
            &package("sample_library_wasm-next"),
            &["--approve", "none"],
        ));
        assert_eq!(upgraded["operation"], "upgrade");
        let navigation = request(port, "GET", "/erp/login", &[], "");
        assert_eq!(navigation.status, 200, "{}", navigation.body);
        assert!(navigation.body.contains("erp-login"));
        let fresh_cookies = login(port, db.name());
        let after_upgrade = create_book(
            port,
            &fresh_cookies,
            r#"{"title":"First write after navigation","times_lent":6}"#,
        );
        assert_eq!(after_upgrade.status, 200, "{}", after_upgrade.body);
        drop(server);
        let restarted = start_server(&config, port, &stderr_path);
        let cookies = login(port, db.name());
        let at_start = create_book(port, &cookies, r#"{"title":"Restarted","times_lent":5}"#);
        assert_eq!(at_start.status, 200, "{}", at_start.body);
        let log = fs::read_to_string(&stderr_path).unwrap();
        assert!(!log.contains("panicked"), "{log}");
        drop(restarted);
        drop(client);
        drop_role(db, &role).await;
    });
}

#[path = "support/wizard_browser.rs"]
mod wizard_browser;

/// Hand the live installed component fixture to the compiled-client browser lane.
fn installed_browser(port: u16, db: &str, dir: &Path, book: i64, borrower: i64) {
    let contract = serde_json::json!({
        "deniedError": {"status":403,"name":"erp.exceptions.AccessError","message":"Your access rights do not let you access 'Library Book' (sample_library.book) records.\n\nGroups that may do this:\n\t- Access Administration\n\t- Internal User\n\t- Regular User\n\t- Settings Administration\n\nAsk an administrator if you need this access."},
        "fixtureKind":"wasm", "schema": 1, "url": format!("http://127.0.0.1:{port}"), "database": db,
        "login": "reader", "password": "reader-password",
        "deniedLogin": "outsider", "deniedPassword": "outsider-password",
        "bookModel": "sample_library.book", "wizardModel": "sample_library.lend.wizard",
        "loanModel": "sample_library.loan", "bookId": book, "bookTitle": "Browser Kindred",
        "borrowerId": borrower, "borrowerName": "Octavia",
        "openMethod": "lend", "confirmMethod": "action_confirm",
    });
    wizard_browser::run(dir, contract);
}

// ---- 1b. screens and buttons -------------------------------------------------------------------

/// The body of a successful JSON-2 reply.
#[track_caller]
fn ok_json(reply: &Reply) -> serde_json::Value {
    assert_eq!(reply.status, 200, "{}", reply.body);
    serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", reply.body))
}

/// `get_views` of the default form view of `model`, as the web client asks for it.
fn form_views(port: u16, cookies: &str, model: &str) -> Reply {
    json_post(
        port,
        cookies,
        &format!("/erp/json/2/{model}/get_views"),
        r#"{"views":[[null,"form"]],"wire_major":1,"options":{"mobile":false}}"#,
    )
}

/// A user an administrator creates through the API: `groups` are external ids of `base`.
fn create_user(port: u16, cookies: &str, login: &str, group: Option<i64>) {
    let partner = ok_json(&json_post(
        port,
        cookies,
        "/erp/json/2/res.partner/create",
        &format!(r#"{{"vals_list":[{{"name":"{login}"}}]}}"#),
    ));
    let partner = partner[0].as_i64().unwrap();
    let groups = group.map_or_else(String::new, |g| format!(r#","group_ids":[[6,0,[{g}]]]"#));
    ok_json(&json_post(
        port,
        cookies,
        "/erp/json/2/res.users/create",
        &format!(
            r#"{{"vals_list":[{{"name":"{login}","login":"{login}","password":"{login}-password","partner_id":{partner},"company_id":1,"company_ids":[[6,0,[1]]]{groups}}}]}}"#
        ),
    ));
}

/// `get_views`, the method call a button makes (JSON-2 on the record) runs a WebAssembly method as the
/// signed-in user, an ordinary user with rights can lend and one without cannot, and uninstalling
/// removes the screens)
#[test]
#[ignore = "needs the packages built by scripts/wasm-poc.sh (ERP_POC_PACKAGES)"]
fn a_package_brings_its_screens_and_buttons_to_the_browser() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let role = format!("{}_web", db.name());
        create_role(&db, &role).await;
        let client = db.client().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let addons = deployed_addons(dir.path());
        let port = free_port();
        let config = config(dir.path(), &db, port, &role, &addons);
        if std::env::var_os("ERP_WIZARD_BROWSER_DRIVER").is_some() {
            let assets = std::env::var_os("ERP_WIZARD_ASSETS")
                .expect("browser lane requires compiled ERP_WIZARD_ASSETS");
            copy_dir(Path::new(&assets), dir.path());
        }
        initialize(&config, dir.path());
        let stderr_path = dir.path().join("server.stderr");
        let server = start_server(&config, port, &stderr_path);
        let admin = login(port, db.name());
        let installed = json_post(
            port,
            &admin,
            "/erp/module/install",
            r#"{"modules":["sample_library"]}"#,
        );
        assert_eq!(installed.status, 200, "{}", installed.body);

        // the package carries its view file, and SHA256SUMS covers it (opening verifies every file)
        let opened =
            erp_modules::package::ErpAddonPackage::open(package("sample_library_wasm")).unwrap();
        assert!(
            opened
                .get_file("views/sample_library_wasm_views.xml")
                .is_some()
        );
        assert!(opened.get_file("security/ir.access.csv").is_some());
        assert_eq!(
            opened.manifest().data(),
            [
                "security/ir.access.csv",
                "views/sample_library_wasm_views.xml"
            ]
        );

        // before: the book form has none of the package's screen parts
        let before = form_views(port, &admin, "sample_library.book");
        assert!(!before.body.contains("times_lent"), "{}", before.body);

        // two ordinary users: an internal user (the sample's access rows grant that group the
        // library models) and a portal user, who has no rights on them
        let group = async |name: &str| -> i64 {
            client
                .query_one(
                    "SELECT res_id FROM ir_model_data WHERE module = 'base' AND name = $1 AND model = 'res.groups'",
                    &[&name],
                )
                .await
                .unwrap()
                .get(0)
        };
        create_user(port, &admin, "reader", Some(group("group_user").await));
        create_user(port, &admin, "outsider", Some(group("group_portal").await));
        let reader = login_as(port, db.name(), "reader", "reader-password");

        // a data file that gives a one2many of the real registry a link is refused (the field types
        // come from the registry of this install), and nothing is installed
        {
            let good = erp_modules::package::ErpAddonPackage::open(package("sample_library_wasm"))
                .unwrap();
            let manifest = std::str::from_utf8(good.get_file("manifest.toml").unwrap())
                .unwrap()
                .replace("views/sample_library_wasm_views.xml", "views/bad.xml");
            let mut writer = erp_modules::package::ErpAddonPackageWriter::new();
            writer.add_file("manifest.toml", manifest);
            writer.add_file(
                "wasm/sample_library_wasm.wasm",
                good.wasm_binary().unwrap().to_vec(),
            );
            writer.add_file(
                "security/ir.access.csv",
                good.get_file("security/ir.access.csv").unwrap().to_vec(),
            );
            writer.add_file(
                "views/bad.xml",
                r#"<erp><record id="m" model="ir.ui.menu"><field name="name">x</field><field name="child_id" eval="[(4, ref('base.menu_administration'), 0)]"/></record></erp>"#,
            );
            let bad = dir.path().join("bad.erpaddon");
            fs::write(&bad, writer.build_zip().unwrap()).unwrap();
            refused(
                &operator("install", &config, db.name(), &bad, &["--approve", "none"]),
                2,
                "'child_id' is a one2many",
            );
            assert_eq!(
                client
                    .query_one("SELECT count(*) FROM ir_module_package", &[])
                    .await
                    .unwrap()
                    .get::<_, i64>(0),
                0
            );
        }

        // the operator installs the package; its views are loaded in the install transaction
        let done = result_of(&operator(
            "install",
            &config,
            db.name(),
            &package("sample_library_wasm"),
            &["--approve", "none"],
        ));
        assert_eq!(
            done["installed"],
            serde_json::json!(["sample_library_wasm"])
        );
        // the administrator is told which methods the package opens to clients
        assert_eq!(
            done["rpc_methods"],
            serde_json::json!([
                "sample_library.book.give_back",
                "sample_library.book.lend",
                "sample_library.lend.wizard.action_confirm",
                "sample_library.loan.give_back",
                "sample_library.loan.renew",
            ])
        );
        assert_eq!(
            done["extended_models"],
            serde_json::json!(["sample_library.book", "sample_library.loan"])
        );
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM ir_model_data WHERE module = 'sample_library_wasm' AND model = 'ir.ui.view'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            4,
            "the three inherited views and the Lend wizard's form belong to the package"
        );

        let catalog = ok_json(&eventually_ok("the Addons list after install", || {
            json_post(
                port,
                &admin,
                "/erp/json/2/ir.module.module/erp_available_addons",
                "{}",
            )
        }));
        let listed = catalog["addons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|addon| addon["name"] == "sample_library_wasm")
            .unwrap_or_else(|| panic!("sample_library_wasm missing in {catalog}"))
            .clone();
        assert_eq!(
            (
                &listed["label"],
                &listed["state"],
                &listed["installable"],
                &listed["category"],
                &listed["application"],
            ),
            (
                &serde_json::json!("Sample Library WASM"),
                &serde_json::json!("installed"),
                &serde_json::json!(false),
                &serde_json::json!("Sample"),
                &serde_json::json!(false),
            ),
            "{listed}"
        );
        // Module info: the manifest's licence, website and direct dependencies, read back from the
        // installed module row (this addon is not in the addons directory).
        assert_eq!(
            (&listed["license"], &listed["website"], &listed["depends"]),
            (
                &serde_json::json!("MIT OR Apache-2.0"),
                &serde_json::json!(""),
                &serde_json::json!(["sample_library"]),
            ),
            "{listed}"
        );

        // the same server, no restart: the form the web client asks for now carries the
        // package's fields (buttons wait for the form grammar; the call a button
        // would make is tested below)
        let book_views = eventually_ok("the book form after install", || {
            let reply = form_views(port, &admin, "sample_library.book");
            if reply.status == 200 && !reply.body.contains("times_lent") {
                // the registry swap has not reached this request yet
                return Reply {
                    status: 503,
                    ..reply
                };
            }
            reply
        });
        let book_form = serde_json::to_string(&ok_json(&book_views)["views"][0]).unwrap();
        for needle in ["times_lent", "last_borrower"] {
            assert!(
                book_form.contains(needle),
                "{needle} missing in {book_form}"
            );
        }
        for view_type in ["form", "list"] {
            let reply = json_post(
                port,
                &reader,
                "/erp/json/2/sample_library.book/get_views",
                &format!(
                    r#"{{"views":[[null,"{view_type}"]],"wire_major":1,"options":{{"mobile":false}}}}"#
                ),
            );
            let view = ok_json(&reply)["views"][0].clone();
            let nodes = view["nodes"].as_array().expect("view nodes");
            for field in ["popular", "lending_summary"] {
                assert!(
                    nodes
                        .iter()
                        .any(|node| node["kind"] == "field" && node["field"]["name"] == field),
                    "{field} missing in {view_type}: {view}"
                );
            }
        }
        let loan_views = form_views(port, &admin, "sample_library.loan");
        let loan_metadata = ok_json(&loan_views);
        let loan_nodes = loan_metadata["views"][0]["nodes"].as_array().unwrap();
        let header = loan_nodes
            .iter()
            .filter(|node| node["kind"] == "header")
            .collect::<Vec<_>>();
        assert_eq!(header.len(), 1, "{loan_metadata}");
        let children = header[0]["children"].as_array().unwrap();
        let header_nodes = children
            .iter()
            .map(|index| &loan_nodes[usize::try_from(index.as_u64().unwrap()).unwrap()])
            .collect::<Vec<_>>();
        assert_eq!(header_nodes[0]["widget"], "statusbar");
        assert_eq!(header_nodes[0]["field"]["name"], "state");
        assert_eq!(header_nodes[1]["name"], "renew");
        assert_eq!(header_nodes[2]["name"], "give_back");
        let loan_form = serde_json::to_string(&loan_metadata["views"][0]).unwrap();
        for needle in ["renewals", "renewal_note"] {
            assert!(
                loan_form.contains(needle),
                "{needle} missing in {loan_form}"
            );
        }

        // a book and a borrower (as the administrator), then the Lend button as an ordinary user:
        // a record method of the package over JSON-2 that opens the package's wizard, whose
        // confirm button creates the loan
        let book = ok_json(&create_book(port, &admin, r#"{"title":"Kindred"}"#))[0]
            .as_i64()
            .unwrap();
        let borrower = ok_json(&json_post(
            port,
            &admin,
            "/erp/json/2/res.partner/create",
            r#"{"vals_list":[{"name":"Octavia"}]}"#,
        ))[0]
            .as_i64()
            .unwrap();
        let lend = |cookies: &str| {
            json_post(
                port,
                cookies,
                "/erp/json/2/sample_library.book/lend",
                &format!(r#"{{"ids":[{book}]}}"#),
            )
        };
        let action = ok_json(&lend(&reader));
        assert_eq!(
            (
                &action["type"],
                &action["res_model"],
                &action["target"],
                &action["context"]["default_book_id"],
            ),
            (
                &serde_json::json!("ir.actions.act_window"),
                &serde_json::json!("sample_library.lend.wizard"),
                &serde_json::json!("new"),
                &serde_json::json!(book),
            ),
            "{action}"
        );
        let due = action["context"]["default_due_date"]
            .as_str()
            .unwrap_or_else(|| panic!("no default due date in {action}"))
            .to_owned();
        // the web client opens the action by loading the wizard's form: it must render, with the
        // book, the borrower and the due date
        let wizard_form = ok_json(&form_views(port, &reader, "sample_library.lend.wizard"));
        let wizard_arch = wizard_form.to_string();
        for field in ["book_id", "borrower_id", "due_date", "action_confirm"] {
            assert!(
                wizard_arch.contains(&format!("\"{field}\"")),
                "{field} missing from the wizard form: {wizard_form}"
            );
        }
        let defaults = ok_json(&json_post(
            port,
            &reader,
            "/erp/json/2/sample_library.lend.wizard/default_get",
            &serde_json::json!({"fields":["book_id","borrower_id","due_date"],
                "context":action["context"]})
            .to_string(),
        ));
        assert_eq!(defaults["book_id"], book, "{defaults}");
        assert_eq!(defaults["due_date"], due, "{defaults}");
        let changed = ok_json(&json_post(
            port, &reader, "/erp/json/2/sample_library.lend.wizard/onchange",
            &serde_json::json!({"ids":[],"values":{"book_id":book,"borrower_id":borrower,"due_date":due},
                "field_names":["borrower_id","due_date"],
                "fields_spec":{"book_id":{},"borrower_id":{},"due_date":{}},
                "context":action["context"]}).to_string(),
        ));
        assert!(changed["value"].is_object(), "{changed}");
        // the book comes from the Lend button and is read-only in the form (owner feedback 2026-10-28)
        assert_eq!(
            &wizard_form["fields"]["sample_library.lend.wizard"]["book_id"]["readonly"],
            &serde_json::json!(true),
            "{wizard_form}"
        );
        // the client never sends a read-only field: create takes the book from the action context
        let wizard = ok_json(&json_post(
            port,
            &reader,
            "/erp/json/2/sample_library.lend.wizard/create",
            &format!(
                r#"{{"vals_list":[{{"borrower_id":{borrower},"due_date":"{due}"}}],"context":{{"default_book_id":{book}}}}}"#
            ),
        ))[0]
            .as_i64()
            .unwrap();
        let confirmed = ok_json(&json_post(
            port,
            &reader,
            "/erp/json/2/sample_library.lend.wizard/action_confirm",
            &format!(r#"{{"ids":[{wizard}]}}"#),
        ));
        assert_eq!(
            (&confirmed["tag"], &confirmed["params"]["next"]["type"]),
            (
                &serde_json::json!("display_notification"),
                &serde_json::json!("ir.actions.act_window_close"),
            ),
            "{confirmed}"
        );
        let loan = client
            .query_one(
                "SELECT state, due_date::text, borrower_id FROM sample_library_loan WHERE book_id = $1",
                &[&book],
            )
            .await
            .unwrap();
        assert_eq!(
            (
                loan.get::<_, String>(0),
                loan.get::<_, String>(1),
                loan.get::<_, i64>(2)
            ),
            ("out".to_owned(), due.clone(), borrower)
        );
        let row = client
            .query_one(
                "SELECT times_lent, last_borrower, available FROM sample_library_book WHERE id = $1",
                &[&book],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, i32>(0), 1);
        assert_eq!(row.get::<_, String>(1), "Octavia");
        assert!(!row.get::<_, bool>(2), "a lent book is not available");
        // a lent book cannot be lent again
        let again = lend(&reader);
        assert_ne!(again.status, 200, "{}", again.body);
        // Return on the book closes its loan and makes it available again
        assert_eq!(
            ok_json(&json_post(
                port,
                &reader,
                "/erp/json/2/sample_library.book/give_back",
                &format!(r#"{{"ids":[{book}]}}"#),
            )),
            serde_json::json!(true)
        );
        let back = client
            .query_one(
                "SELECT b.available, l.state, l.return_date IS NOT NULL FROM sample_library_book b JOIN sample_library_loan l ON l.book_id = b.id WHERE b.id = $1",
                &[&book],
            )
            .await
            .unwrap();
        assert_eq!(
            (
                back.get::<_, bool>(0),
                back.get::<_, String>(1),
                back.get::<_, bool>(2)
            ),
            (true, "returned".to_owned(), true)
        );
        // the reader also sees the screen
        let reader_form = form_views(port, &reader, "sample_library.book");
        assert!(
            reader_form.body.contains("times_lent"),
            "{}",
            reader_form.body
        );

        // A removed row proves missing-record handling, never vacuum/expiry behavior.
        assert_eq!(
            ok_json(&json_post(
                port,
                &reader,
                "/erp/json/2/sample_library.lend.wizard/unlink",
                &serde_json::json!({"ids":[wizard]}).to_string()
            )),
            true
        );
        let missing = json_post(
            port,
            &reader,
            "/erp/json/2/sample_library.lend.wizard/action_confirm",
            &serde_json::json!({"ids":[wizard]}).to_string(),
        );
        assert_eq!(missing.status, 404, "{}", missing.body);
        let error: serde_json::Value = serde_json::from_str(&missing.body).unwrap();
        assert_eq!(error["name"], "erp.exceptions.MissingError");
        let reader_id: i64 = client
            .query_one("SELECT id FROM res_users WHERE login = $1", &[&"reader"])
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            error["message"],
            format!(
                "Record does not exist or has been deleted.\n(Record: sample_library.lend.wizard({wizard},), User: {reader_id})"
            )
        );
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM sample_library_loan WHERE book_id = $1",
                    &[&book]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            1,
            "missing wizard cannot create a second loan"
        );

        if std::env::var_os("ERP_WIZARD_BROWSER_DRIVER").is_some() {
            let browser_book =
                ok_json(&create_book(port, &admin, r#"{"title":"Browser Kindred"}"#))[0]
                    .as_i64()
                    .unwrap();
            installed_browser(port, db.name(), dir.path(), browser_book, borrower);
        }

        // a user without rights on the library: refused, nothing changes
        let outsider = login_as(port, db.name(), "outsider", "outsider-password");
        let refused = lend(&outsider);
        assert_eq!(refused.status, 403, "{}", refused.body);
        let denied: serde_json::Value = serde_json::from_str(&refused.body).unwrap();
        assert_eq!(
            denied["name"], "erp.exceptions.AccessError",
            "{}",
            refused.body
        );
        assert_eq!(
            denied["message"],
            "Your access rights do not let you access 'Library Book' (sample_library.book) records.\n\nGroups that may do this:\n\t- Access Administration\n\t- Internal User\n\t- Regular User\n\t- Settings Administration\n\nAsk an administrator if you need this access."
        );
        assert_eq!(
            client
                .query_one(
                    "SELECT times_lent FROM sample_library_book WHERE id = $1",
                    &[&book]
                )
                .await
                .unwrap()
                .get::<_, i32>(0),
            1,
            "a refused call changes nothing"
        );
        // a name that is not a public method of the package stays unreachable
        let unknown = json_post(
            port,
            &reader,
            "/erp/json/2/sample_library.book/_lend_private",
            &format!(r#"{{"ids":[{book}]}}"#),
        );
        assert!(matches!(unknown.status, 403 | 404), "{}", unknown.body);

        // uninstall: the screens go with the package, the data stays
        let removed = result_of(&uninstall(&config, db.name(), "sample_library_wasm"));
        assert_eq!(removed["rpc_methods"], serde_json::json!([]));
        eventually_refused("lend after uninstall", || lend(&reader));
        let gone = lend(&reader);
        assert!(
            matches!(gone.status, 403 | 404),
            "{} {}",
            gone.status,
            gone.body
        );
        eventually_ok("the book form after uninstall", || {
            let reply = form_views(port, &admin, "sample_library.book");
            if reply.status == 200 && reply.body.contains("times_lent") {
                return Reply {
                    status: 503,
                    ..reply
                };
            }
            reply
        });
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM ir_ui_view WHERE name LIKE 'sample_library.%.lend' OR name LIKE 'sample_library.%.renew'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM ir_model_data WHERE module = 'sample_library_wasm'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            0,
            "no external id of the package is left"
        );
        let log = fs::read_to_string(&stderr_path).unwrap();
        assert!(!log.contains("panicked"), "{log}");
        drop(server);
        drop(client);
        drop_role(db, &role).await;
    });
}

// Change a real compiled package's archive identity without replacing its guest implementation.
fn catalogue_package_variant(version: Option<&str>) -> erp_modules::package::ErpAddonPackage {
    use erp_modules::package::{ErpAddonPackage, ErpAddonPackageWriter};
    let original = ErpAddonPackage::open(package("sample_library_wasm")).unwrap();
    let mut writer = ErpAddonPackageWriter::new();
    for path in original.file_paths().filter(|path| *path != "SHA256SUMS") {
        let bytes = original.get_file(path).unwrap();
        if path == "manifest.toml"
            && let Some(version) = version
        {
            let manifest = std::str::from_utf8(bytes).unwrap();
            let old = format!("version = \"{}\"", original.version());
            assert_eq!(manifest.matches(&old).count(), 1);
            writer.add_file(
                path,
                manifest.replace(&old, &format!("version = \"{version}\"")),
            );
        } else {
            writer.add_file(path, bytes);
        }
    }
    writer.add_file("catalogue-marker.txt", "identity replacement fixture");
    ErpAddonPackage::from_bytes(&writer.build_zip().unwrap()).unwrap()
}

async fn assert_catalogue_package_refused(
    db: &TestDb,
    addons: &Path,
    config: &Path,
    stderr: &Path,
    reason: &str,
) {
    use erp_modules::{Error, WasmReason};
    let node = open_node(db, addons).await;
    let error = node
        .lifecycle()
        .admit_installed_packages()
        .await
        .unwrap_err();
    match &error {
        Error::WasmAddon {
            addon,
            reason: WasmReason::StoredPackage(message),
        } => {
            assert_eq!(addon.as_str(), "sample_library_wasm");
            assert_eq!(message, reason);
        }
        other => panic!("expected stored-package refusal, got {other:?}"),
    }
    let expected = format!("Error: {error}\n");
    drop(node);
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(config)
            .stdout(Stdio::null())
            .stderr(fs::File::create(stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "invalid package startup did not refuse"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_to_string(stderr).unwrap(), expected);
    assert_eq!(
        db.client()
            .await
            .unwrap()
            .query_one("SELECT count(*) FROM erp_node", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
}

#[derive(Clone, Copy)]
enum CatalogueChange {
    Install,
    Upgrade,
    Uninstall,
}

fn stop_catalogue_server(mut server: Server) {
    assert!(
        Command::new("kill")
            .args(["-TERM", &server.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert!(status.success(), "native shutdown failed: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "native shutdown timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "built and required by CI stage wasm-startup-catalogue via scripts/wasm-poc.sh --startup-catalogue"]
fn native_startup_catalogue_includes_installed_packages() {
    big_stack(async {
        clone_start_probe().await;
        native_catalogue_roundtrip(CatalogueChange::Upgrade).await;
        attachment_maintenance_with_package_computes().await;
    });
}

#[test]
#[ignore = "required by CI stage wasm-startup-catalogue"]
fn native_startup_catalogue_refuses_added_package() {
    big_stack(native_catalogue_roundtrip(CatalogueChange::Install));
}

#[test]
#[ignore = "required by CI stage wasm-startup-catalogue"]
fn native_startup_catalogue_refuses_removed_package() {
    big_stack(native_catalogue_roundtrip(CatalogueChange::Uninstall));
}

/// The in-app attachment maintenance ORM is built from the served registry, which holds the
/// installed package's computed fields: the server starts with it enabled and the runner claims
/// a maintenance job (it used to stop the server at startup: compute method not found).
async fn attachment_maintenance_with_package_computes() {
    let db = TestDb::create().await.unwrap();
    let role = format!("{}_web", db.name());
    create_role(&db, &role).await;
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let addons = dir.path().join("addons");
    runtime_addons::stage_runtime_addons(&root.join("addons"), &addons).unwrap();
    let port = free_port();
    let config_path = config(dir.path(), &db, port, &role, &addons);
    initialize(&config_path, dir.path());
    let node = open_node(&db, &addons).await;
    let env = node.system().system_env(Context::new());
    let installed = node
        .system()
        .button_immediate_install(&env, &[ModuleName::new("sample_library").unwrap()])
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());
    drop(installed);
    drop(node);
    // sample_library_wasm declares two computed fields of sample_library.book
    let installed = result_of(&operator(
        "install",
        &config_path,
        db.name(),
        &package("sample_library_wasm"),
        &["--approve", "none"],
    ));
    assert_eq!(installed["addon"], "sample_library_wasm", "{installed}");
    let mut text = fs::read_to_string(&config_path).unwrap();
    text.push_str("attachment_maintenance_run_in_app=true\n");
    private(&config_path, &text);
    let client = db.client().await.unwrap();
    let stderr = dir.path().join("maintenance.stderr");
    let mut server = start_server(&config_path, port, &stderr);
    client
            .batch_execute("INSERT INTO ir_blob_usage_delta(bytes) SELECT 0 FROM generate_series(1,7); UPDATE erp_job SET run_after=clock_timestamp() WHERE kind='base.attachment_usage_compact'")
            .await
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            panic!(
                "server with attachment maintenance exited {status}: {}",
                fs::read_to_string(&stderr).unwrap()
            );
        }
        let ran: i64 = client
                .query_one("SELECT count(*) FROM erp_job WHERE kind='base.attachment_usage_compact' AND state='ready' AND started_at IS NOT NULL AND last_error_code IS NULL AND run_after>clock_timestamp()", &[])
                .await
                .unwrap()
                .get(0);
        if ran > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "attachment maintenance did not run: {}",
            fs::read_to_string(&stderr).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop_catalogue_server(server);
    drop(client);
    drop_role(db, &role).await;
}

async fn clone_start_probe() {
    let db = TestDb::create().await.unwrap();
    let role = format!("{}_web", db.name());
    create_role(&db, &role).await;
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let addons = dir.path().join("addons");
    runtime_addons::stage_runtime_addons(&root.join("addons"), &addons).unwrap();
    let config = config(dir.path(), &db, free_port(), &role, &addons);
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    initialize(&config, dir.path());
    let node = open_node(&db, &addons).await;
    let env = node.system().system_env(Context::new());
    let installed = node
        .system()
        .button_immediate_install(&env, &[ModuleName::new("sample_library").unwrap()])
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());
    drop(installed);
    drop(node);
    let installed = result_of(&operator(
        "install",
        &config,
        db.name(),
        &package("sample_library_wasm"),
        &["--approve", "none"],
    ));
    assert_eq!(installed["addon"], "sample_library_wasm");
    assert!(!addons.join("sample_library_wasm").exists());
    let output = Command::new("bash")
        .arg(root.join("tests/scripts/fixtures/human_clone_start/python.sh"))
        .arg("-I")
        .arg(root.join("tests/scripts/fixtures/human_clone_start/native_probe.py"))
        .arg(root)
        .arg(env!("CARGO_BIN_EXE_erp"))
        .arg(&config)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    let proof: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    println!("clone startup fixture: {proof}");
    assert_eq!(proof["status"], "PASS");
    assert_eq!(proof["login_http"], 200);
    assert_eq!(proof["child_reaped"], true);
    assert_eq!(proof["startup_failure_refused"], true);
    assert!(
        proof["inventory"]["modules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "base")
    );
    assert_eq!(
        proof["inventory"]["packages"][0]["name"],
        "sample_library_wasm"
    );
    drop_role(db, &role).await;
}

async fn native_catalogue_roundtrip(change: CatalogueChange) {
    let db = TestDb::create().await.unwrap();
    let role = format!("{}_web", db.name());
    create_role(&db, &role).await;
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let addons = dir.path().join("addons");
    runtime_addons::stage_runtime_addons(&root.join("addons"), &addons).unwrap();
    for name in ["sample_library_wasm", "sample_library_wasm_policy"] {
        assert!(
            !addons.join(name).exists(),
            "package source must be outside the offered folders"
        );
    }
    let port = free_port();
    let config_path = config(dir.path(), &db, port, &role, &addons);
    initialize(&config_path, dir.path());
    let node = open_node(&db, &addons).await;
    let env = node.system().system_env(Context::new());
    let installed = node
        .system()
        .button_immediate_install(&env, &[ModuleName::new("sample_library").unwrap()])
        .await
        .unwrap();
    assert!(installed.post_commit.is_clean());
    drop(installed);
    drop(node);
    let initial_packages: &[&str] = match change {
        CatalogueChange::Install => &["sample_library_wasm"],
        CatalogueChange::Upgrade | CatalogueChange::Uninstall => {
            &["sample_library_wasm", "sample_library_wasm_policy"]
        }
    };
    for &name in initial_packages {
        let installed = result_of(&operator(
            "install",
            &config_path,
            db.name(),
            &package(name),
            &["--approve", "none"],
        ));
        assert_eq!(installed["addon"], name, "{installed}");
    }
    let client = db.client().await.unwrap();
    let stored = client.query_one("SELECT sha256::text, package_data FROM ir_module_package WHERE name='sample_library_wasm' ORDER BY id DESC LIMIT 1", &[]).await.unwrap();
    let original_sha: String = stored.get::<_, String>(0).trim_end().to_owned();
    let original_bytes: Vec<u8> = stored.get(1);
    let bad_sha = "00".repeat(32);
    client
        .execute(
            "UPDATE ir_module_package SET sha256=$1 WHERE name='sample_library_wasm'",
            &[&bad_sha],
        )
        .await
        .unwrap();
    assert_catalogue_package_refused(
        &db,
        &addons,
        &config_path,
        &dir.path().join("hash-refusal.stderr"),
        &format!("the stored archive hashes to {original_sha} but its row says {bad_sha}"),
    )
    .await;
    let unaudited = catalogue_package_variant(None);
    client.execute("UPDATE ir_module_package SET sha256=$1, package_data=$2 WHERE name='sample_library_wasm'", &[&unaudited.archive_sha256(), &unaudited.archive_bytes()]).await.unwrap();
    assert_catalogue_package_refused(
        &db,
        &addons,
        &config_path,
        &dir.path().join("audit-refusal.stderr"),
        "no install or upgrade row of the lifecycle audit records this archive",
    )
    .await;
    client.execute("UPDATE ir_module_package SET sha256=$1, package_data=$2 WHERE name='sample_library_wasm'", &[&original_sha, &original_bytes]).await.unwrap();
    let stderr = dir.path().join("server.stderr");
    let server = start_server(&config_path, port, &stderr);
    assert_eq!(
        request(port, "GET", "/erp/health/ready", &[], "").status,
        200
    );
    let admin = login(port, db.name());
    let form = form_views(port, &admin, "sample_library.book");
    assert!(ok_json(&form).to_string().contains("times_lent"));
    let book = ok_json(&create_book(
        port,
        &admin,
        r#"{"title":"Catalogue regression"}"#,
    ))[0]
        .as_i64()
        .unwrap();
    let action = ok_json(&json_post(
        port,
        &admin,
        "/erp/json/2/sample_library.book/lend",
        &format!(r#"{{"ids":[{book}]}}"#),
    ));
    assert_eq!(action["res_model"], "sample_library.lend.wizard");

    // A second process reads the packages independently, with the same shared configuration.
    let peer_port = free_port();
    let peer_config = dir.path().join("peer.toml");
    let original = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &peer_config,
        original.replace(
            &format!("bind='127.0.0.1:{port}'"),
            &format!("bind='127.0.0.1:{peer_port}'"),
        ),
    )
    .unwrap();
    let peer = start_server(&peer_config, peer_port, &dir.path().join("peer.stderr"));
    assert_eq!(
        request(peer_port, "GET", "/erp/health/ready", &[], "").status,
        200
    );
    login(peer_port, db.name());
    let rows = client
        .query(
            "SELECT digest FROM erp_node WHERE state='ready' ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, Vec<u8>>(0), rows[1].get::<_, Vec<u8>>(0));
    let details = client
        .query(
            "SELECT payload FROM erp_node_detail WHERE kind='addons'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(details.len(), 2);
    for row in details {
        let payload: Vec<u8> = row.get(0);
        for name in ["base", "base_data", "web"]
            .into_iter()
            .chain(initial_packages.iter().copied())
        {
            assert!(
                payload
                    .windows(name.len())
                    .any(|bytes| bytes == name.as_bytes()),
                "missing catalogue entry {name}"
            );
        }
    }
    for &name in initial_packages {
        let archive = erp_modules::package::ErpAddonPackage::open(package(name)).unwrap();
        let expected: erp_orm::node::Digest = archive.archive_sha256().parse().unwrap();
        for row in client
            .query(
                "SELECT payload FROM erp_node_detail WHERE kind='addons'",
                &[],
            )
            .await
            .unwrap()
        {
            let payload: Vec<u8> = row.get(0);
            assert!(
                payload
                    .windows(32)
                    .any(|bytes| bytes == expected.as_bytes()),
                "package archive hash absent from membership catalogue"
            );
        }
    }
    // A generation change with unchanged sources must still compose under this identity.
    let owner = open_node(&db, &addons).await;
    let loaded = owner.load().await.unwrap();
    let mut tx = loaded.orm.begin(SUPERUSER_ID).await.unwrap();
    tx.signal_bump(&erp_orm::SignalKey::Registry, erp_orm::SelfApplied::No)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    eventually_ok("unchanged catalogue refresh", || {
        request(port, "GET", "/erp/login", &[], "")
    });
    eventually_ok("unchanged peer catalogue refresh", || {
        request(peer_port, "GET", "/erp/login", &[], "")
    });
    drop(loaded);
    drop(owner);

    match change {
        CatalogueChange::Upgrade => {
            // Same names do not authorize different archive content under the old membership.
            let next = catalogue_package_variant(Some("99.0.0"));
            assert_ne!(next.archive_sha256(), original_sha);
            let replacement = dir.path().join("replacement.erpaddon");
            fs::write(&replacement, next.archive_bytes()).unwrap();
            let upgraded = result_of(&operator(
                "upgrade",
                &config_path,
                db.name(),
                &replacement,
                &["--approve", "none"],
            ));
            assert_eq!(upgraded["archive_sha256"], next.archive_sha256());
        }
        CatalogueChange::Install => {
            let installed = result_of(&operator(
                "install",
                &config_path,
                db.name(),
                &package("sample_library_wasm_policy"),
                &["--approve", "none"],
            ));
            assert_eq!(installed["addon"], "sample_library_wasm_policy");
        }
        CatalogueChange::Uninstall => {
            let removed = result_of(&uninstall(
                &config_path,
                db.name(),
                "sample_library_wasm_policy",
            ));
            assert_eq!(removed["addon"], "sample_library_wasm_policy");
        }
    }
    for (node_port, log) in [
        (port, &stderr),
        (peer_port, &dir.path().join("peer.stderr")),
    ] {
        eventually_refused("changed catalogue must not publish", || {
            request(node_port, "GET", "/erp/login", &[], "")
        });
        assert_eq!(request(node_port, "GET", "/erp/login", &[], "").status, 503);
        let log = fs::read_to_string(log).unwrap();
        assert!(log.contains("addon catalogue identity changed; drain and restart node membership before authentication publication"), "{log}");
    }
    stop_catalogue_server(peer);
    stop_catalogue_server(server);
    // A fresh membership can serve the new catalogue after the old nodes have departed.
    let restarted = start_server(&config_path, port, &dir.path().join("restart.stderr"));
    assert_eq!(
        request(port, "GET", "/erp/health/ready", &[], "").status,
        200
    );
    login(port, db.name());
    let current = client
        .query_one("SELECT digest FROM erp_node WHERE state='ready'", &[])
        .await
        .unwrap()
        .get::<_, Vec<u8>>(0);
    assert_ne!(current, rows[0].get::<_, Vec<u8>>(0));
    stop_catalogue_server(restarted);
    drop(client);
    drop_role(db, &role).await;
}

// ---- 2. two packages, a second node ------------------------------------------------------------

async fn open_node(db: &TestDb, addons: &Path) -> Node {
    let client = db.pool().get().await.unwrap();
    let baseline = Signal::read(&client).await.unwrap();
    drop(client);
    Node::open(db.pool().clone(), &[addons.to_path_buf()], Some(baseline)).unwrap()
}

/// Real installed metadata through the native get_views producer (ordinary test user policy).
async fn header_description(orm: &Orm) -> serde_json::Value {
    form_description(orm, "sample_library.loan").await
}

async fn form_description(orm: &Orm, model: &str) -> serde_json::Value {
    let mut tx = orm
        .begin_with(operator_user(), company_context())
        .await
        .unwrap();
    let records = tx.env().records(model).unwrap();
    let erp_orm::rpc::Exposure::Exposed(method) = tx
        .exposure()
        .get(records.model_id(), "get_views")
        .unwrap()
        .clone()
    else {
        panic!("get_views exposed")
    };
    let args = erp_fields::ValueMap::from_iter([
        (
            "views".into(),
            Value::List(vec![Value::List(vec![Value::False, Value::str("form")].into())].into()),
        ),
        ("wire_major".into(), Value::Int(1)),
        (
            "options".into(),
            Value::Map(Arc::new(erp_fields::ValueMap::from_iter([(
                "mobile".into(),
                Value::Bool(false),
            )]))),
        ),
    ]);
    let value = method
        .call(&mut tx, records, erp_orm::rpc::RpcArgs::new(args))
        .await
        .unwrap();
    let json = erp_fields::convert::value_to_json(&value).unwrap();
    tx.rollback().await.unwrap();
    json
}

/// The administrator the tests act as: a guest asks the host for nothing in a System or elevated
/// environment (security review r1 H1), so the package's methods run for a user.
fn operator_user() -> erp_orm::UserId {
    erp_orm::UserId::new(2).unwrap()
}

fn company_context() -> Context {
    Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))])
}

/// The ORM of `loaded` (the node's, which only the System principal may use) rebuilt with the
/// access policy of a test that allows every user, plus the layers, computes and constraints of
/// the packages the node holds: the way to call a package's methods as an ordinary user without
/// a login (`a_running_server_follows_the_operator_without_a_restart` does it over HTTP).
fn user_orm(node: &Node, db: &TestDb, loaded: &Orm) -> Orm {
    let checks = node.package_checks().unwrap();
    Orm::builder(Arc::clone(loaded.registry()), db.pool().clone())
        .policy(erp_orm::StaticPolicy::AllowAll)
        .trust_caller_resolutions()
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .constraints(checks.constraints)
        .computes(checks.computes)
        .wasm_methods(node.wasm_methods(loaded).unwrap())
        .build()
        .unwrap()
}

/// One method call at the top of the chain, committed when it succeeds (what an RPC call does).
async fn call(
    orm: &Orm,
    model: &str,
    id: i64,
    method: &str,
    args: &str,
) -> erp_fields::Result<String> {
    // a guest call runs for a caller with a company
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    let mut tx = orm.begin_with(operator_user(), ctx).await?;
    let records = tx.env().records(model)?.browse(vec![id])?;
    match call_method_json(&mut tx, &records, method, args).await {
        Ok(result) => {
            tx.commit().await?;
            Ok(result)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

async fn scalar(orm: &Orm, sql: &str) -> String {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx.query(sql, &[]).await.unwrap();
    tx.rollback().await.unwrap();
    rows[0]
        .try_get::<_, String>(0)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// One field of one record, read through the ORM (so computed fields are computed), committed
/// never.
async fn field(orm: &Orm, model: &str, id: i64, field: &str) -> String {
    let mut tx = orm
        .begin_with(operator_user(), company_context())
        .await
        .unwrap();
    let records = tx.env().records(model).unwrap().browse(vec![id]).unwrap();
    let cell = records.cell(&mut tx, field).await.unwrap();
    tx.rollback().await.unwrap();
    match cell {
        Cell::Str(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

/// `write` with dynamic values as one committed transaction (what the RPC write does).
async fn write_loan(orm: &Orm, id: i64, field: &str, value: Value) -> erp_fields::Result<()> {
    let mut tx = orm
        .begin_with(operator_user(), company_context())
        .await
        .unwrap();
    let records = tx.env().records("sample_library.loan")?.browse(vec![id])?;
    let mut values = BTreeMap::new();
    values.insert(field.to_owned(), value);
    match records.write_values(&mut tx, &values).await {
        Ok(()) => tx.commit().await,
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

async fn unlink_loan(orm: &Orm, id: i64) -> erp_fields::Result<()> {
    let mut tx = orm
        .begin_with(operator_user(), company_context())
        .await
        .unwrap();
    let records = tx.env().records("sample_library.loan")?.browse(vec![id])?;
    match records.unlink(&mut tx).await {
        Ok(()) => tx.commit().await,
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

async fn new_loan(orm: &Orm) -> (i64, i64) {
    let mut tx = orm
        .begin_with(operator_user(), company_context())
        .await
        .unwrap();
    let book = tx
        .env()
        .records("sample_library.book")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("title", Cell::Str("  Kindred  ".into()))],
        )
        .await
        .unwrap();
    let partner = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Reader".into()))],
        )
        .await
        .unwrap();
    let loan = tx
        .env()
        .records("sample_library.loan")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("book_id", Cell::Id(book.ids()[0]))
                    .set_cell("borrower_id", Cell::Id(partner.ids()[0]))
                    .set_cell("state", Cell::Str("out".into())),
            ],
        )
        .await
        .unwrap();
    let ids = (book.ids()[0], loan.ids()[0]);
    tx.commit().await.unwrap();
    ids
}

/// The Lend button and its wizard the way the web client runs them, then Return: `lend` answers
/// the wizard's window action, the wizard is created with the action's defaults and a new
/// borrower, its confirm button creates the loan, and `give_back` on that loan closes it (other
/// loans of the book stay as they are).
async fn lend_and_return(orm: &Orm, book: i64, borrower: &str) {
    let action: serde_json::Value = serde_json::from_str(
        &call(orm, "sample_library.book", book, "lend", "null")
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        action["res_model"], "sample_library.lend.wizard",
        "{action}"
    );
    let due = action["context"]["default_due_date"]
        .as_str()
        .unwrap_or_else(|| panic!("no default due date in {action}"))
        .to_owned();
    let mut tx = orm
        .begin_with(operator_user(), company_context())
        .await
        .unwrap();
    let partner = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str(borrower.into()))],
        )
        .await
        .unwrap();
    let wizard = tx
        .env()
        .records("sample_library.lend.wizard")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("book_id", Cell::Id(book))
                    .set_cell("borrower_id", Cell::Id(partner.ids()[0]))
                    .set_cell(
                        "due_date",
                        Cell::Date(Date::parse_odoo(&due).expect("the action's due date")),
                    ),
            ],
        )
        .await
        .unwrap();
    let wizard = wizard.ids()[0];
    tx.commit().await.unwrap();
    let done: serde_json::Value = serde_json::from_str(
        &call(
            orm,
            "sample_library.lend.wizard",
            wizard,
            "action_confirm",
            "null",
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(done["tag"], "display_notification", "{done}");
    let lent: i64 = scalar(
        orm,
        &format!(
            "SELECT max(id)::text FROM sample_library_loan WHERE book_id = {book} AND state = 'out'"
        ),
    )
    .await
    .parse()
    .unwrap();
    assert_eq!(
        call(orm, "sample_library.loan", lent, "give_back", "null")
            .await
            .unwrap(),
        "true"
    );
}

/// previous ORM keeps what it had)
/// the override with `super` across two packages, the error path, the audit trail)
#[test]
#[ignore = "needs the packages built by scripts/wasm-poc.sh (ERP_POC_PACKAGES)"]
fn operator_installs_two_wasm_addons_and_a_second_node_serves_their_methods() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let role = format!("{}_web", db.name());
        create_role(&db, &role).await;
        let dir = tempfile::tempdir().unwrap();
        let addons = deployed_addons(dir.path());
        let config = config(dir.path(), &db, free_port(), &role, &addons);
        let old_view = include_str!("fixtures/office-blue-previous/library-views.xml");
        let old_manifest = include_str!("fixtures/office-blue-previous/library-manifest.toml");
        let library_views = addons.join("sample_library/views/sample_library_views.xml");
        let library_manifest = addons.join("sample_library/manifest.toml");
        let new_view = fs::read_to_string(&library_views).unwrap();
        let new_manifest = fs::read_to_string(&library_manifest).unwrap();
        fs::write(&library_views, old_view).unwrap();
        fs::write(&library_manifest, old_manifest).unwrap();
        initialize(&config, dir.path());

        // node B: this process. The data-only addon goes in through the module system.
        let node_b = open_node(&db, &addons).await;
        let loaded = node_b.load().await.unwrap();
        let sample = [ModuleName::new("sample_library").unwrap()];
        let env = node_b.system().system_env(Context::new());
        let base = node_b
            .system()
            .button_immediate_install(&env, &sample)
            .await
            .unwrap();
        assert!(base.post_commit.is_clean());
        let granted = user_orm(&node_b, &db, &base.orm);
        let (book, loan) = new_loan(&granted).await;
        let old_standalone = header_description(&granted).await;
        assert!(!old_standalone.to_string().contains("statusbar"));

        assert!(
            call(&granted, "sample_library.book", book, "lend", "null")
                .await
                .is_err(),
            "before the package: no such method"
        );
        drop(loaded);

        // node A: the operator command installs package 1
        let first = result_of(&operator(
            "install",
            &config,
            db.name(),
            &package("sample_library_wasm-previous"),
            &["--approve", "none"],
        ));
        assert_eq!(first["version"], "1.3.4");
        let sha = first["archive_sha256"].as_str().unwrap().to_owned();
        assert_eq!(sha.len(), 64);

        let old_orm = node_b.load().await.unwrap().orm;
        let old_orm = user_orm(&node_b, &db, &old_orm);
        let old_inherited = header_description(&old_orm).await;
        let old_nodes = old_inherited["views"][0]["nodes"].as_array().unwrap();
        assert_eq!(
            old_nodes
                .iter()
                .filter(|node| node["kind"] == "header")
                .count(),
            1
        );
        assert!(!old_nodes.iter().any(|node| node["widget"] == "statusbar"));
        assert_eq!(
            old_nodes
                .iter()
                .filter(|node| node["field"]["name"] == "state")
                .count(),
            1
        );

        // Prepare a verified prior source receipt for the exact old base files installed above.
        let client = db.client().await.unwrap();
        let previous_release = fs::canonicalize(dir.path())
            .unwrap()
            .join("previous-header-release");
        let mut file_hashes = BTreeMap::new();
        let mut identities = BTreeMap::new();
        let mut previous = BTreeMap::new();
        for row in client.query("SELECT name, content_hash FROM installed_addon_identity WHERE source_kind <> 'package' ORDER BY name", &[]).await.unwrap() {
            let name: String = row.get(0);
            let folder = previous_release.join("addons").join(&name);
            copy_dir(&addons.join(&name), &folder);
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(&name, &folder).unwrap();
            assert_eq!(snapshot.digest(), row.get::<_, String>(1));
            for (path, bytes) in snapshot.files() { file_hashes.insert(format!("addons/{name}/{path}"), erp_modules::package::sha256_hex(bytes)); }
            identities.insert(name.clone(), snapshot.digest().to_owned());
            previous.insert(ModuleName::new(&name).unwrap(), snapshot.digest().to_owned());
        }
        fs::write(previous_release.join("REVISION"), "0".repeat(40)).unwrap();
        fs::write(previous_release.join("PACKAGE.json"), serde_json::to_vec(&serde_json::json!({"source":"0".repeat(40),"sha256":file_hashes,"addon_identities":identities})).unwrap()).unwrap();
        // Coordinated maintenance train: migrate the base view, publish its identity, then
        // upgrade the stored extension before any human form is reopened.
        fs::write(&library_views, new_view).unwrap();
        fs::write(&library_manifest, new_manifest).unwrap();
        let native_upgrade = Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["module", "upgrade", "-d", db.name(), "--addons-path"])
            .arg(&addons)
            .arg("--identity-previous")
            .arg(&previous_release)
            .arg("sample_library")
            .env("DATABASE_URL", url(&db.config(), None))
            .output()
            .unwrap();
        assert!(
            native_upgrade.status.success(),
            "{}",
            text(&native_upgrade.stderr)
        );
        node_b.lifecycle().admit_installed_packages().await.unwrap();
        node_b
            .system()
            .identity_activate(
                &previous,
                "fixture-operator",
                "isolated coordinated header upgrade after base migration",
            )
            .await
            .unwrap();
        let header_upgrade = result_of(&operator(
            "upgrade",
            &config,
            db.name(),
            &package("sample_library_wasm"),
            &["--approve", "none"],
        ));
        assert_eq!(header_upgrade["version"], "1.3.6");
        drop(client);

        // node B follows: load() reads the stored package, verifies it, builds registry and ORM
        let one = {
            let system = node_b.load().await.unwrap().orm;
            user_orm(&node_b, &db, &system)
        };
        let book_view = form_description(&one, "sample_library.book").await;
        let book_nodes = book_view["views"][0]["nodes"].as_array().unwrap();
        assert!(
            book_nodes
                .iter()
                .any(|n| n["kind"] == "stat_button" && n["name"] == "open_loans"),
            "{book_view}"
        );
        assert!(
            book_nodes
                .iter()
                .any(|n| n["widget"] == "relation_count" && n["field"]["name"] == "loan_ids"),
            "{book_view}"
        );
        let action: serde_json::Value = serde_json::from_str(
            &call(&one, "sample_library.book", book, "open_loans", "null")
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            action["domain"],
            serde_json::json!([["book_id", "=", book]])
        );
        assert_eq!(action["target"], "current");
        let new_inherited = header_description(&one).await;
        let new_nodes = new_inherited["views"][0]["nodes"].as_array().unwrap();
        let headers: Vec<_> = new_nodes
            .iter()
            .filter(|node| node["kind"] == "header")
            .collect();
        assert_eq!(headers.len(), 1);
        let children = headers[0]["children"].as_array().unwrap();
        let header_nodes: Vec<_> = children
            .iter()
            .map(|index| &new_nodes[usize::try_from(index.as_u64().unwrap()).unwrap()])
            .collect();
        assert_eq!(header_nodes[0]["widget"], "statusbar");
        assert_eq!(header_nodes[0]["field"]["name"], "state");
        assert_eq!(header_nodes[1]["name"], "renew");
        assert_eq!(header_nodes[2]["name"], "give_back");
        assert_eq!(
            field(&one, "sample_library.loan", loan, "state").await,
            "out"
        );
        // the stored computed column was filled for the book that existed before the package (the
        // operator's environment has no company: the compute still ran)
        assert_eq!(
            scalar(
                &one,
                &format!("SELECT popular::text FROM sample_library_book WHERE id = {book}")
            )
            .await,
            "false",
            "the backfill of the new stored computed field"
        );
        // the book is lent twice through the wizard
        lend_and_return(&one, book, "Reader").await;
        lend_and_return(&one, book, "Second").await;
        assert_eq!(
            scalar(&one, &format!("SELECT times_lent::text || ':' || last_borrower FROM sample_library_book WHERE id = {book}")).await,
            "2:Second"
        );
        // the override of create (WebAssembly over the core create) trims the title of a book
        // created now; the book created before the package existed keeps its spaces
        let (trimmed, _) = new_loan(&one).await;
        assert_eq!(
            scalar(
                &one,
                &format!("SELECT title FROM sample_library_book WHERE id = {trimmed}")
            )
            .await,
            "Kindred"
        );
        assert_eq!(
            scalar(
                &one,
                &format!("SELECT title FROM sample_library_book WHERE id = {book}")
            )
            .await,
            "  Kindred  "
        );
        // computed fields of the package: the text is computed on read, the stored flag is kept
        // current in the database when `lend` changes the counter it depends on
        assert_eq!(
            field(&one, "sample_library.book", book, "lending_summary").await,
            "Lent 2 time(s), last to Second"
        );
        assert_eq!(
            scalar(
                &one,
                &format!("SELECT popular::text FROM sample_library_book WHERE id = {book}")
            )
            .await,
            "false"
        );
        for expected in ["1", "2", "3"] {
            assert_eq!(
                call(&one, "sample_library.loan", loan, "renew", r#"["again"]"#)
                    .await
                    .unwrap(),
                expected,
                "without the policy package a loan renews without limit"
            );
        }

        // a constraint of the package: more renewals than the limit fails the write, the
        // transaction rolls back, and the loan keeps its three renewals
        let too_many = write_loan(&one, loan, "renewals", Value::Int(6))
            .await
            .unwrap_err();
        assert_eq!(
            too_many.kind(),
            erp_fields::ErrorKind::Validation,
            "{too_many}"
        );
        assert!(too_many.to_string().contains("at most 5"), "{too_many}");
        assert_eq!(
            scalar(
                &one,
                &format!("SELECT renewals::text FROM sample_library_loan WHERE id = {loan}")
            )
            .await,
            "3"
        );
        // an override of write with super: the due date edited by hand stamps the note, then the
        // core write stores both
        write_loan(&one, loan, "due_date", Value::Str("2030-01-31".into()))
            .await
            .unwrap();
        assert_eq!(
            scalar(&one, &format!("SELECT due_date::text || ':' || renewal_note FROM sample_library_loan WHERE id = {loan}")).await,
            "2030-01-31:due date edited by hand"
        );
        // an override of unlink with super: a renewed loan is kept, an unrenewed one is deleted
        let kept = unlink_loan(&one, loan).await.unwrap_err();
        assert_eq!(kept.kind(), erp_fields::ErrorKind::Validation, "{kept}");
        assert!(kept.to_string().contains("kept"), "{kept}");
        let (_, unrenewed) = new_loan(&one).await;
        unlink_loan(&one, unrenewed).await.unwrap();
        assert_eq!(
            scalar(
                &one,
                &format!("SELECT count(*)::text FROM sample_library_loan WHERE id = {unrenewed}")
            )
            .await,
            "0"
        );

        // package 2 is refused while it is not approved as asked, and does not depend on nothing
        let policy = package("sample_library_wasm_policy");
        refused(
            &operator(
                "install",
                &config,
                db.name(),
                &policy,
                &["--approve", "network:outbound:example.org"],
            ),
            1,
            "capabilities",
        );
        assert_eq!(
            scalar(&one, "SELECT count(*)::text FROM ir_module_module WHERE name = 'sample_library_wasm_policy' AND state = 'installed'").await,
            "0"
        );

        // node A installs package 2: an override with `super`, from its own package
        let second = result_of(&operator(
            "install",
            &config,
            db.name(),
            &policy,
            &["--approve", "none"],
        ));
        assert_eq!(
            second["installed"],
            serde_json::json!(["sample_library_wasm_policy"])
        );
        let two = {
            let system = node_b.load().await.unwrap().orm;
            user_orm(&node_b, &db, &system)
        };

        // the loan that already renewed three times is over the limit: a validation error, nothing written
        let over = call(&two, "sample_library.loan", loan, "renew", r#"["x"]"#)
            .await
            .unwrap_err();
        assert_eq!(over.kind(), erp_fields::ErrorKind::Validation, "{over}");
        assert!(over.to_string().contains("at most 2 times"), "{over}");
        assert_eq!(
            scalar(
                &two,
                &format!("SELECT renewals::text FROM sample_library_loan WHERE id = {loan}")
            )
            .await,
            "3"
        );
        // a fresh loan renews twice through `super`, the third is refused
        let (_, fresh) = new_loan(&two).await;
        for expected in ["1", "2"] {
            assert_eq!(
                call(&two, "sample_library.loan", fresh, "renew", r#"["ok"]"#)
                    .await
                    .unwrap(),
                expected
            );
        }
        let third = call(&two, "sample_library.loan", fresh, "renew", r#"["no"]"#)
            .await
            .unwrap_err();
        assert!(third.to_string().contains("at most 2 times"), "{third}");
        assert_eq!(
            scalar(&two, &format!("SELECT renewals::text || ':' || renewal_note FROM sample_library_loan WHERE id = {fresh}")).await,
            "2:ok",
            "the refused call wrote nothing"
        );
        // the lock field of the second package refuses before the first package runs
        let (_, locked) = new_loan(&two).await;
        {
            let mut tx = two.begin(SUPERUSER_ID).await.unwrap();
            tx.env()
                .records("sample_library.loan")
                .unwrap()
                .browse(vec![locked])
                .unwrap()
                .write(
                    &mut tx,
                    RawVals::new().set_cell("renewal_locked", Cell::Bool(true)),
                )
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        let refused_lock = call(&two, "sample_library.loan", locked, "renew", r#"["x"]"#)
            .await
            .unwrap_err();
        assert!(
            refused_lock.to_string().contains("locked"),
            "{refused_lock}"
        );
        assert_eq!(
            scalar(&two, &format!("SELECT coalesce(renewals, 0)::text FROM sample_library_loan WHERE id = {locked}")).await,
            "0"
        );
        // the ORM node B had before package 2 is untouched: it still renews without the rule
        assert_eq!(
            call(
                &one,
                "sample_library.loan",
                loan,
                "renew",
                r#"["old registry"]"#
            )
            .await
            .unwrap(),
            "4"
        );
        // package 1 cannot go while package 2 depends on it
        refused(
            &uninstall(&config, db.name(), "sample_library_wasm"),
            1,
            "sample_library_wasm_policy",
        );

        // upgrade package 1 to the next version (scripts/wasm-poc.sh builds it with the minor
        // version after the manifest's)
        let upgraded = result_of(&operator(
            "upgrade",
            &config,
            db.name(),
            &package("sample_library_wasm-next"),
            &["--approve", "none"],
        ));
        assert!(
            upgraded["upgraded"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n == "sample_library_wasm")
        );
        let three = {
            let system = node_b.load().await.unwrap().orm;
            user_orm(&node_b, &db, &system)
        };
        lend_and_return(&three, book, "After upgrade").await;
        assert_eq!(
            scalar(
                &three,
                &format!("SELECT times_lent::text FROM sample_library_book WHERE id = {book}")
            )
            .await,
            "3"
        );
        // after the upgrade the third lending makes the book popular, and the text follows
        assert_eq!(
            field(&three, "sample_library.book", book, "lending_summary").await,
            "Lent 3 time(s), last to After upgrade"
        );
        assert_eq!(
            scalar(
                &three,
                &format!("SELECT popular::text FROM sample_library_book WHERE id = {book}")
            )
            .await,
            "true"
        );
        // the same version again is not an upgrade
        refused(
            &operator(
                "upgrade",
                &config,
                db.name(),
                &package("sample_library_wasm-next"),
                &["--approve", "none"],
            ),
            1,
            "not newer",
        );

        // uninstall in reverse order
        result_of(&uninstall(&config, db.name(), "sample_library_wasm_policy"));
        result_of(&uninstall(&config, db.name(), "sample_library_wasm"));
        let after = {
            let system = node_b.load().await.unwrap().orm;
            user_orm(&node_b, &db, &system)
        };
        assert!(
            call(&after, "sample_library.book", book, "lend", "null")
                .await
                .is_err()
        );
        assert_eq!(
            scalar(&after, "SELECT count(*)::text FROM information_schema.columns WHERE table_name = 'sample_library_book' AND column_name = 'times_lent'").await,
            "0",
            "the fields of the package go with it"
        );

        // the audit trail names every operation and the package it was for
        let trail = scalar(
            &after,
            "SELECT string_agg(operation || ':' || module, ',' ORDER BY id) FROM ir_module_wasm_audit",
        )
        .await;
        assert_eq!(
            trail,
            "install:sample_library_wasm,upgrade:sample_library_wasm,install:sample_library_wasm_policy,upgrade:sample_library_wasm,uninstall:sample_library_wasm_policy,uninstall:sample_library_wasm"
        );
        drop(after);
        drop_role(db, &role).await;
    });
}

// ---- 3. refusals ---------------------------------------------------------------------------------

/// nothing written)
#[test]
#[ignore = "needs the packages built by scripts/wasm-poc.sh (ERP_POC_PACKAGES)"]
fn the_operator_command_refuses_what_was_not_approved() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let role = format!("{}_web", db.name());
        create_role(&db, &role).await;
        let dir = tempfile::tempdir().unwrap();
        let addons = deployed_addons(dir.path());
        let config = config(dir.path(), &db, free_port(), &role, &addons);
        initialize(&config, dir.path());
        let node = open_node(&db, &addons).await;
        let env = node.system().system_env(Context::new());
        let _ = node
            .system()
            .button_immediate_install(&env, &[ModuleName::new("sample_library").unwrap()])
            .await
            .unwrap();
        let file = package("sample_library_wasm");

        // no approval given: the command line is refused before anything runs (usage error)
        let none = erp(&[
            "addon".as_ref(),
            "install".as_ref(),
            file.as_os_str(),
            "-d".as_ref(),
            db.name().as_ref(),
            "--config".as_ref(),
            config.as_os_str(),
        ]);
        assert_eq!(none.status.code(), Some(2), "{}", text(&none.stderr));
        assert!(
            text(&none.stderr).contains("--approve"),
            "{}",
            text(&none.stderr)
        );
        // 'none' next to a capability is not an approval
        refused(
            &operator(
                "install",
                &config,
                db.name(),
                &file,
                &["--approve", "none,network:outbound:example.org"],
            ),
            1,
            "cannot be combined",
        );
        // approving something the package does not ask for is stale consent
        refused(
            &operator(
                "install",
                &config,
                db.name(),
                &file,
                &["--approve", "network:outbound:example.org"],
            ),
            1,
            "does not request",
        );
        // empty elements and a shouted keyword are not approvals
        refused(
            &operator("install", &config, db.name(), &file, &["--approve", ","]),
            1,
            "is empty",
        );
        refused(
            &operator("install", &config, db.name(), &file, &["--approve", "NONE"]),
            1,
            "lowercase",
        );
        // an acknowledgement for client code the package does not have
        refused(
            &operator(
                "install",
                &config,
                db.name(),
                &file,
                &["--approve", "none", "--ack-client-digest", &"0".repeat(64)],
            ),
            1,
            "no static/dist",
        );
        // another database than the configured one
        refused(
            &operator(
                "install",
                &config,
                "some_other_db",
                &file,
                &["--approve", "none"],
            ),
            1,
            "differs from the database",
        );
        // a file that is not a package
        let junk = dir.path().join("junk.erpaddon");
        fs::write(&junk, b"not a zip").unwrap();
        let out = operator("install", &config, db.name(), &junk, &["--approve", "none"]);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
        // the policy package needs package 1 installed first (the lifecycle never installs dependencies)
        refused(
            &operator(
                "install",
                &config,
                db.name(),
                &package("sample_library_wasm_policy"),
                &["--approve", "none"],
            ),
            1,
            "sample_library_wasm",
        );
        // uninstalling what is not installed
        refused(
            &uninstall(&config, db.name(), "sample_library_wasm"),
            1,
            "not installed",
        );

        let rows = scalar(
            &node.load().await.unwrap().orm,
            "SELECT count(*)::text FROM ir_module_module WHERE name LIKE 'sample_library_wasm%' AND state <> 'uninstalled'",
        )
        .await;
        assert_eq!(rows, "0", "nothing was installed by the refused commands");
        drop(node);
        drop_role(db, &role).await;
    });
}

// ---- 4. grants that fail after the commit --------------------------------------------------------

/// the operator is told the install is committed and which command finishes the job; review r1 M2)
#[test]
#[ignore = "needs the packages built by scripts/wasm-poc.sh (ERP_POC_PACKAGES)"]
fn a_failed_grant_publication_says_the_install_is_committed() {
    big_stack(async {
        let db = TestDb::create().await.unwrap();
        let role = format!("{}_web", db.name());
        create_role(&db, &role).await;
        let dir = tempfile::tempdir().unwrap();
        let addons = deployed_addons(dir.path());
        let config = config(dir.path(), &db, free_port(), &role, &addons);
        initialize(&config, dir.path());
        // from now on the configuration names a runtime role that does not exist, so the grants fail
        let missing = format!("{}_missing", db.name());
        private(
            &dir.path().join("runtime-url"),
            &url(&db.config(), Some(&missing)),
        );
        let node = open_node(&db, &addons).await;
        let env = node.system().system_env(Context::new());
        let _ = node
            .system()
            .button_immediate_install(&env, &[ModuleName::new("sample_library").unwrap()])
            .await
            .unwrap();

        let out = operator(
            "install",
            &config,
            db.name(),
            &package("sample_library_wasm"),
            &["--approve", "none"],
        );
        assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
        let stderr = text(&out.stderr);
        assert!(stderr.contains("COMMITTED"), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "erp db publish-grants --config {}",
                config.display()
            )),
            "{stderr}"
        );
        assert_eq!(
            scalar(
                &node.load().await.unwrap().orm,
                "SELECT state FROM ir_module_module WHERE name = 'sample_library_wasm'"
            )
            .await,
            "installed",
            "the install is committed, as the message says"
        );
        drop(node);
        drop_role(db, &role).await;
    });
}
