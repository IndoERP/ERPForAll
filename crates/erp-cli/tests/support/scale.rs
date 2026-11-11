#[path = "login_rollout.rs"]
mod login_rollout;
// Narrow fixture adapted from human_runtime.rs: private configuration, real CLI and restricted role.
#[path = "runtime_addons.rs"]
mod runtime_addons;

use erp_testkit::{
    TestDb,
    tokio_postgres::{self, config::Host},
};
use std::{
    collections::BTreeMap,
    fs,
    future::Future,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub struct Deadline(pub Instant);
impl Deadline {
    pub fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }
    pub fn remaining(self) -> io::Result<Duration> {
        self.0
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "scale fixture deadline exhausted")
            })
    }
    pub async fn wait<T>(self, future: impl Future<Output = T>) -> io::Result<T> {
        tokio::time::timeout(self.remaining()?, future)
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "scale fixture deadline exhausted")
            })
    }
}
pub struct OwnedChild {
    pub child: Child,
}
impl OwnedChild {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        Ok(Self {
            child: command.spawn()?,
        })
    }
    pub fn stop(&mut self, deadline: Deadline) -> io::Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child.kill()?;
        }
        self.wait(deadline).map(|_| ())
    }
    pub fn wait(&mut self, deadline: Deadline) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(deadline.remaining()?.min(Duration::from_millis(10)));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Best-effort emergency path reports failures; the normal path explicitly checks stop.
        if let Err(error) = self.stop(Deadline::after(Duration::from_secs(1))) {
            eprintln!(
                "scale fixture owned child {} cleanup failed: {error}",
                self.child.id()
            );
        }
    }
}
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: String,
}
pub fn request(
    deadline: Deadline,
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> io::Result<Reply> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream =
        TcpStream::connect_timeout(&address, deadline.remaining()?.min(Duration::from_secs(2)))?;
    let mut wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        wire.push_str(&format!("{name}: {value}\r\n"));
    }
    wire.push_str("\r\n");
    wire.push_str(body);
    stream.set_write_timeout(Some(deadline.remaining()?.min(Duration::from_secs(3))))?;
    stream.write_all(wire.as_bytes())?;
    read_reply(
        &mut DeadlineReader {
            stream: &mut stream,
            deadline,
        },
        deadline,
    )
}

struct DeadlineReader<'a> {
    stream: &'a mut TcpStream,
    deadline: Deadline,
}
impl Read for DeadlineReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(self.deadline.remaining()?.min(Duration::from_secs(3))))?;
        self.stream.read(buffer)
    }
}

const REPLY_LIMIT: usize = 1024 * 1024;

fn framed_reply_length(bytes: &[u8]) -> io::Result<Option<usize>> {
    let Some(boundary) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
        return Ok(None);
    };
    let head =
        std::str::from_utf8(&bytes[..boundary]).map_err(|_| invalid("non-UTF8 HTTP reply"))?;
    let mut length = None;
    for line in head.split("\r\n").skip(1) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("invalid HTTP header"))?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid("chunked HTTP reply unsupported by fixture"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(invalid("ambiguous HTTP content length"));
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid("invalid HTTP content length"));
            }
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| invalid("invalid HTTP content length"))?,
            );
        }
    }
    let length = length.ok_or_else(|| invalid("missing HTTP content length"))?;
    let total = boundary
        .checked_add(4)
        .and_then(|head| head.checked_add(length))
        .filter(|total| *total <= REPLY_LIMIT)
        .ok_or_else(|| invalid("HTTP reply exceeds fixture limit"))?;
    Ok(Some(total))
}

fn read_reply(reader: &mut impl Read, deadline: Deadline) -> io::Result<Reply> {
    let mut bytes = Vec::new();
    loop {
        deadline.remaining()?;
        if let Some(total) = framed_reply_length(&bytes)? {
            if bytes.len() > total {
                return Err(invalid("HTTP reply exceeds declared content length"));
            }
            if bytes.len() == total {
                return parse_reply(&bytes);
            }
        }
        let mut buffer = [0; 8192];
        let size = reader.read(&mut buffer)?;
        if size == 0 {
            return parse_reply(&bytes);
        }
        if bytes
            .len()
            .checked_add(size)
            .is_none_or(|total| total > REPLY_LIMIT)
        {
            return Err(invalid("HTTP reply exceeds fixture limit"));
        }
        bytes.extend_from_slice(&buffer[..size]);
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn parse_reply(bytes: &[u8]) -> io::Result<Reply> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("non-UTF8 HTTP reply"))?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| invalid("missing HTTP header boundary"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| invalid("missing HTTP status"))?
        .parse()
        .map_err(|_| invalid("invalid HTTP status"))?;
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("invalid HTTP header"))?;
        headers
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.trim().to_owned());
    }
    // The tested small responses use Content-Length. Refuse unsupported framing loudly.
    if headers.contains_key("transfer-encoding") {
        return Err(invalid("chunked HTTP reply unsupported by fixture"));
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|values| values.first())
        .ok_or_else(|| invalid("missing HTTP content length"))?
        .parse()
        .map_err(|_| invalid("invalid HTTP content length"))?;
    if length != body.len() {
        return Err(invalid("truncated HTTP reply"));
    }
    Ok(Reply {
        status,
        headers,
        body: body.to_owned(),
    })
}
pub fn cookie(reply: &Reply, name: &str) -> String {
    reply.headers["set-cookie"]
        .iter()
        .find(|value| value.starts_with(&format!("{name}=")))
        .expect("missing named cookie")
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
            .expect("missing bootstrap script")
            .split("</script>")
            .next()
            .unwrap(),
    )
    .unwrap()
}
pub fn ready(child: &mut OwnedChild, port: u16, deadline: Deadline) -> io::Result<()> {
    loop {
        if let Some(status) = child.child.try_wait()? {
            return Err(io::Error::other(format!(
                "server pid={} exited before readiness: {status}",
                child.child.id()
            )));
        }
        let reply = request(deadline, port, "GET", "/erp/health/ready", &[], "");
        // A transport race must never hide a process that failed during startup.
        if let Some(status) = child.child.try_wait()? {
            return Err(io::Error::other(format!(
                "server pid={} exited during readiness: {status}",
                child.child.id()
            )));
        }
        match reply {
            Ok(reply) if reply.status == 200 => return Ok(()),
            // A bound listener can be waiting for its node's settle interval; only 503 is retryable.
            Ok(reply) if reply.status == 503 => {}
            Ok(reply) => {
                return Err(io::Error::other(format!(
                    "server readiness returned HTTP {}",
                    reply.status
                )));
            }
            // Only this idempotent startup probe retries transport races. Normal requests
            // propagate incomplete/reset replies without retrying any application operation.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
                ) => {}
            Err(error) => return Err(error),
        }
        std::thread::sleep(deadline.remaining()?.min(Duration::from_millis(25)));
    }
}
fn private(path: &Path, value: &[u8]) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn encoded(bytes: &[u8]) -> String {
    let mut value = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            value.push(char::from(*byte));
        } else {
            value.push_str(&format!("%{byte:02X}"));
        }
    }
    value
}
fn url(config: &tokio_postgres::Config, role: Option<&str>) -> String {
    let Host::Tcp(host) = &config.get_hosts()[0] else {
        panic!("scale fixture requires TCP test cluster")
    };
    let (user, password) = match role {
        Some(role) => (role, b"fixture-runtime-password".as_slice()),
        None => (config.get_user().unwrap(), config.get_password().unwrap()),
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
fn addons() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons")
}
fn config(dir: &Path, db: &TestDb, port: u16, role: &str, addons: &Path) -> PathBuf {
    private(&dir.join("owner-url"), url(&db.config(), None).as_bytes());
    private(
        &dir.join("runtime-url"),
        url(&db.config(), Some(role)).as_bytes(),
    );
    private(
        &dir.join("node-url"),
        url(&db.config(), Some(&format!("{role}_node"))).as_bytes(),
    );
    let text = format!(
        "database='{}'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nnode_database_url_file='node-url'\nbind='127.0.0.1:{port}'\npublic_origin='http://erp.example.test'\nproxy_mode='direct'\naddons_path=['{}']\nassets_manifest='manifest.json'\n",
        db.name(),
        addons.display()
    );
    private(&dir.join("server.toml"), text.as_bytes());
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let assets: Vec<_> = [("js", "text/javascript; charset=utf-8"), ("css", "text/css; charset=utf-8")].into_iter().map(|(ext, mime)| {
        let path = format!("client.{digest}.{ext}");
        fs::write(dir.join(&path), b"").unwrap();
        serde_json::json!({"name":format!("client.{ext}"), "path":path,"sha256":digest,"content_type":mime})
    }).collect();
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema":1,"assets":assets})).unwrap(),
    )
    .unwrap();
    dir.join("server.toml")
}
pub struct Fixture {
    pub db: Option<TestDb>,
    pub nodes: Vec<OwnedChild>,
    pub ports: Vec<u16>,
    dirs: Vec<tempfile::TempDir>,
    roles: Vec<String>,
    admin: Option<tokio_postgres::Config>,
}
impl Fixture {
    pub fn empty() -> Self {
        Self {
            db: None,
            nodes: Vec::new(),
            ports: Vec::new(),
            dirs: Vec::new(),
            roles: Vec::new(),
            admin: None,
        }
    }
    pub fn config_path(&self, index: usize) -> PathBuf {
        self.dirs[index].path().join("server.toml")
    }
    pub async fn start(&mut self, deadline: Deadline) {
        // Creation may have committed CREATE DATABASE before returning its RAII owner.
        // Retain that owner before checking admission; canceling creation can leak the DB.
        self.db = Some(TestDb::create().await.unwrap());
        deadline
            .remaining()
            .expect("database creation exhausted work deadline");
        let db = self.db.as_ref().unwrap();
        self.admin = Some(db.config());
        self.admin.as_mut().unwrap().dbname("postgres");
        let role = format!("{}_scale", db.name());
        let client = deadline.wait(db.client()).await.unwrap().unwrap();
        // Track the deterministic name before CREATE so even an ambiguous timeout attempts removal.
        for name in [&role, &format!("{role}_node")] {
            self.roles.push(name.clone());
            let create_role = format!(
                "CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' \
                 NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",
                erp_sql::quote(name)
            );
            deadline
                .wait(client.batch_execute(&create_role))
                .await
                .unwrap()
                .unwrap();
        }
        // Reserve both ports together, then release them immediately before spawning. Bind races fail readiness.
        let listeners: Vec<_> = (0..2)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        self.ports = listeners
            .iter()
            .map(|listener| listener.local_addr().unwrap().port())
            .collect();
        self.dirs = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
        let runtime_addons = self.dirs[0].path().join("runtime-addons");
        runtime_addons::stage_runtime_addons(&addons(), &runtime_addons)
            .expect("stage admitted runtime addons before native node startup");
        deadline
            .remaining()
            .expect("runtime addon staging exhausted work deadline");
        let configs: Vec<_> = self
            .dirs
            .iter()
            .zip(&self.ports)
            .map(|(dir, port)| config(dir.path(), db, *port, &role, &runtime_addons))
            .collect();
        private(
            &self.dirs[0].path().join("password"),
            b"fixture-administrator-password",
        );
        let mut init = Command::new(env!("CARGO_BIN_EXE_erp"));
        init.args(["db", "init", "--config"])
            .arg(&configs[0])
            .args(["--admin-login", "fixture-admin", "--admin-password-file"])
            .arg(self.dirs[0].path().join("password"));
        self.command(&mut init, "db init", deadline);
        let mut install = Command::new(env!("CARGO_BIN_EXE_erp"));
        install
            .args([
                "module",
                "install",
                "acceptance_notes",
                "-d",
                db.name(),
                "--addons-path",
            ])
            .arg(&runtime_addons)
            .env("DATABASE_URL", url(&db.config(), None));
        self.command(&mut install, "offline addon install", deadline);
        let mut grants = Command::new(env!("CARGO_BIN_EXE_erp"));
        grants
            .args(["db", "publish-grants", "--config"])
            .arg(&configs[0]);
        self.command(&mut grants, "runtime grant publication", deadline);
        self.roles.push(login_rollout::prepare(db, &role).await);
        let state = deadline
            .wait(client.query_one(
                "SELECT state FROM ir_module_module WHERE name='acceptance_notes'",
                &[],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.get::<_, String>(0), "installed");
        drop(client);
        drop(listeners);
        for config in configs {
            let mut command = Command::new(env!("CARGO_BIN_EXE_erp"));
            command
                .args(["server", "--config"])
                .arg(config)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            self.nodes
                .push(OwnedChild::spawn(&mut command).expect("spawn real erp server"));
        }
        assert_ne!(self.ports[0], self.ports[1]);
        assert_ne!(self.nodes[0].child.id(), self.nodes[1].child.id());
        for (node, port) in self.nodes.iter_mut().zip(&self.ports) {
            ready(node, *port, deadline).unwrap();
        }
    }
    fn command(&self, command: &mut Command, stage: &str, deadline: Deadline) {
        // Never include Command debug, environment, cookies or raw subprocess output in failures.
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = OwnedChild::spawn(command)
            .unwrap_or_else(|error| panic!("{stage}: spawn failed: {error}"));
        let status = child
            .wait(deadline)
            .unwrap_or_else(|error| panic!("{stage}: {error}"));
        assert!(status.success(), "{stage}: CLI exited {status}");
    }
    pub async fn cleanup(&mut self, deadline: Deadline) -> io::Result<()> {
        let mut failures = Vec::new();
        for node in &mut self.nodes {
            if let Err(error) = node.stop(deadline) {
                failures.push(format!("node {}: {error}", node.child.id()));
            }
        }
        self.nodes.clear();
        if let Some(db) = self.db.take() {
            match deadline.wait(db.cleanup()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("isolated database cleanup: {error}")),
                Err(error) => failures.push(error.to_string()),
            }
        }
        if let Some(admin) = &self.admin {
            // Separate connection survives dropping the dedicated database; role names are quoted.
            // Track each role until its own removal succeeds, including partial setup failures.
            let mut remaining = Vec::new();
            for role in &self.roles {
                let result = deadline
                    .wait(async {
                        let (client, connection) = admin.connect(tokio_postgres::NoTls).await?;
                        let driver = tokio::spawn(connection);
                        let result = client
                            .batch_execute(&format!("DROP ROLE IF EXISTS {}", erp_sql::quote(role)))
                            .await;
                        drop(client);
                        driver.abort();
                        result
                    })
                    .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        failures.push(format!("fixture role cleanup: {error}"));
                        remaining.push(role.clone());
                    }
                    Err(error) => {
                        failures.push(error.to_string());
                        remaining.push(role.clone());
                    }
                }
            }
            self.roles = remaining;
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(failures.join("; ")))
        }
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;

    #[test]
    fn framed_reply_completes_before_connection_reset() {
        struct ResetAfterFrame(&'static [u8]);
        impl Read for ResetAfterFrame {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "fixture peer reset",
                    ));
                }
                let size = output.len().min(self.0.len());
                output[..size].copy_from_slice(&self.0[..size]);
                self.0 = &self.0[size..];
                Ok(size)
            }
        }
        let deadline = Deadline::after(Duration::from_secs(5));
        let reply = read_reply(&mut ResetAfterFrame(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy"), deadline).unwrap();
        assert_eq!(reply.status, 503);
        assert_eq!(reply.body, "busy");
        let error = read_reply(
            &mut ResetAfterFrame(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\n\r\nbus",
            ),
            deadline,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(error.to_string(), "fixture peer reset");
    }
    #[test]
    fn framed_reply_rejects_ambiguous_and_oversized_lengths() {
        for (wire, message) in [
            ("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n".to_owned(), "ambiguous HTTP content length"),
            ("HTTP/1.1 503 Service Unavailable\r\nContent-Length: +4\r\n\r\nbusy".to_owned(), "invalid HTTP content length"),
            (format!("HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\n\r\n", usize::MAX), "HTTP reply exceeds fixture limit"),
        ] {
            let error = read_reply(&mut io::Cursor::new(wire.as_bytes()), Deadline::after(Duration::from_secs(5))).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(error.to_string(), message);
        }
    }
}
