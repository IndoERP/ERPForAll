//! Two actual OS processes exercising ordinary Auth::with_membership over test command pipes.
//! This is not HTTP/native transport coverage and does not complete AUTHROL-022/023 by itself.
//! Each child builds a verified base-only or alias-selecting registry, registers real membership,
//! and obtains actual serving admission. Isolated enrollment uses the real operator APIs.
//! Bearers cross private pipes only: no protocol payload is printed in retained evidence/panics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]
mod auth_support;
use auth_support::*;

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use erp_fields::Value;
use erp_orm::node::{
    AddonContent, AddonsDigest, BuildIdentity, Digest, FeatureKey, FeatureRollout, FeatureSet,
    FeatureVersion, LegacyEnrollment, NodeIdentity, NodeMembership, NodeOptions, NodeRole, Rollout,
};
use erp_orm::{Orm, StaticPolicy};
use erp_security::auth::{Auth, AuthConfig, HashPool, Login, LoginError, SessionSecret};
use erp_testkit::{Harness, HarnessBuilder};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const MARK: &str = "@@ROLLOUT ";
const MAX_LINE: usize = 8192;
const LIMIT: Duration = Duration::from_secs(60);
const PREFIX: &str = "ERP_AUTHROL_CHILD_";
const KEY: &str = "auth.login_principal";

fn feature() -> FeatureRollout {
    FeatureRollout::new(
        FeatureKey::new(KEY).unwrap(),
        FeatureVersion::new(1).unwrap(),
    )
}
fn harness_builder(alias: bool) -> HarnessBuilder {
    let mut addons = vec![&erp_addon_base::ADDON, &erp_data::model_data::ADDON];
    if alias {
        addons.push(&erp_test_models::test_login_selection::ADDON);
    }
    Harness::builder(addons)
        .with_decls([erp_data::ir_model_data_decl()])
        .require_core_tables(erp_security::auth::core_tables())
}

/// Bounded framing on both sides, including libtest's nonprotocol stdout lines.
async fn bounded_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unfinished protocol line",
                ))
            };
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| n + 1);
        let used = end.unwrap_or(available.len());
        if line
            .len()
            .checked_add(used)
            .is_none_or(|len| len > MAX_LINE)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "protocol line exceeds bound",
            ));
        }
        line.extend_from_slice(&available[..used]);
        reader.consume(used);
        if end.is_some() {
            line.pop();
            return String::from_utf8(line).map(Some).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "protocol line is not UTF-8",
                )
            });
        }
    }
}

fn child_config() -> (erp_testkit::tokio_postgres::Config, bool, bool) {
    let need = |suffix: &str| {
        std::env::var(format!("{PREFIX}{suffix}")).expect("missing child configuration")
    };
    let (alias, capable) = match need("MODE").as_str() {
        "base" => (false, true),
        "alias" => (true, true),
        "incompatible" => (false, false),
        _ => panic!("invalid child mode"),
    };
    let mut config = erp_testkit::tokio_postgres::Config::new();
    let host = need("HOST");
    if host.starts_with('/') {
        config.host_path(host);
    } else {
        config.host(host);
    }
    config.port(need("PORT").parse().expect("invalid child port"));
    config.user(need("USER")).dbname(need("DB"));
    if let Ok(password) = std::env::var(format!("{PREFIX}PASSWORD")) {
        config.password(password);
    }
    match (
        std::env::var(format!("{PREFIX}CONNECT_TIMEOUT_SECS")),
        std::env::var(format!("{PREFIX}CONNECT_TIMEOUT_NANOS")),
    ) {
        (Ok(seconds), Ok(nanos)) => {
            let seconds: u64 = seconds
                .parse()
                .expect("invalid child connect timeout seconds");
            let nanos: u32 = nanos.parse().expect("invalid child connect timeout nanos");
            assert!(
                nanos < 1_000_000_000,
                "invalid child connect timeout precision"
            );
            config.connect_timeout(Duration::new(seconds, nanos));
        }
        (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => {}
        _ => panic!("incomplete child connect timeout configuration"),
    }
    config.application_name(if alias {
        "authrol-alias-child"
    } else {
        "authrol-base-child"
    });
    (config, alias, capable)
}

async fn await_serving(member: &NodeMembership) {
    let mut changes = member.subscribe();
    member.request_ready().await.unwrap();
    tokio::time::timeout(LIMIT, async {
        while !member.is_serving() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("child membership did not reach serving admission");
    assert!(member.is_serving());
}

fn login_answer(result: Result<Login, LoginError>) -> Json {
    match result {
        Ok(login) => json!({"type":"login", "uid":login.uid().get(),
            "cookie":login.session().secret().to_cookie().as_str()}),
        Err(error) => {
            let error: erp_fields::Error = error.into();
            json!({"type":"error", "kind":format!("{:?}", error.kind()), "message":error.message()})
        }
    }
}

/// Self-spawned worker; ordinary Auth API, no fixture selection constructor or raw authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "started only by the rollout process acceptance tests"]
async fn rollout_child_worker() {
    let (config, alias, capable) = child_config();
    let registry = harness_builder(alias).registry_only().unwrap();
    let pool = erp_orm::pool::create_pool(config.clone(), 4).unwrap();
    let baseline = erp_orm::Signal::read(&pool.get().await.unwrap())
        .await
        .unwrap();
    let orm = Orm::builder(registry, pool)
        .signal(erp_orm::Signal::new(baseline))
        .policy(StaticPolicy::AllowAll)
        .trust_caller_resolutions()
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .unwrap();
    // heartbeat_config uses interval / 4 for TCP keepalives; real Linux sockets
    // require that value to contain at least one whole second.
    let options = NodeOptions::builder()
        .heartbeat_interval_for_tests(Duration::from_secs(4))
        .lease_intervals(10)
        .build()
        .unwrap();
    let heartbeat = erp_orm::node::heartbeat_pool(config, &options).unwrap();
    let mut addons = AddonsDigest::builder();
    for addon in orm.registry().addons() {
        addons = addons
            .addon(addon.name, "fixture-1", AddonContent::Embedded)
            .unwrap();
    }
    let mut identity = NodeIdentity::builder(
        NodeRole::App,
        if alias { "alias-peer" } else { "base-peer" },
        "process-fixture",
        BuildIdentity::__from_build(
            "erp-test-auth-process",
            "1",
            false,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("0"),
        ),
        addons.build().unwrap(),
    );
    if capable {
        identity = identity.features(
            FeatureSet::new([(
                FeatureKey::new(KEY).unwrap(),
                FeatureVersion::new(1).unwrap(),
            )])
            .unwrap(),
        );
    }
    let identity = Arc::new(identity.options(options).create(&heartbeat).await.unwrap());
    if !capable {
        incompatible_worker(identity, heartbeat).await;
        return;
    }
    let member = NodeMembership::register(Arc::clone(&identity), heartbeat)
        .await
        .unwrap();
    let hashing: &'static HashPool = waiting_pool(1);
    let auth = Arc::new(
        Auth::with_membership(
            orm,
            AuthConfig::DEFAULT
                .with_throttle_entries(if alias { 128 } else { 64 })
                .unwrap(),
            hashing,
            member.admission(),
        )
        .await
        .unwrap(),
    );
    let model = auth.orm().registry().model_id("res.users").unwrap();
    let base_domain = auth
        .orm()
        .dispatch()
        .is_base_method(model, "_get_login_domain");
    let base_order = auth
        .orm()
        .dispatch()
        .is_base_method(model, "_get_login_order");
    let base_credentials = auth
        .orm()
        .dispatch()
        .is_base_method(model, "_check_credentials");
    assert_eq!(
        (base_domain, base_order, base_credentials),
        (!alias, !alias, true)
    );
    let mut tx = auth.orm().begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let backend: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    tx.rollback().await.unwrap();
    drop(tx);
    println!(
        "{MARK}{}",
        json!({"type":"constructed", "os_pid":std::process::id(),
        "backend_pid":backend, "node":identity.id().to_string(), "digest":identity.digest().to_string(),
        "base_domain":base_domain, "base_order":base_order, "base_credentials":base_credentials})
    );
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut release = None;
    let mut pending = None;
    while let Some(line) = bounded_line(&mut stdin).await.unwrap() {
        let request: Json = serde_json::from_str(&line).expect("invalid command framing");
        let op = request["op"].as_str().expect("missing child operation");
        let response = match op {
            "serve" => {
                await_serving(&member).await;
                json!({"type":"serving", "serving":member.is_serving()})
            }
            "login" => login_answer(
                login_from(
                    &auth,
                    request["login"].as_str().unwrap(),
                    request["password"].as_str().unwrap(),
                    request["address"].as_str().unwrap(),
                )
                .await,
            ),
            "check" => {
                let cookie = SessionSecret::from_cookie(request["cookie"].as_str().unwrap())
                    .expect("invalid cookie format");
                match check_user(&auth, &cookie).await {
                    Ok(uid) => json!({"type":"checked", "uid":uid}),
                    Err(error) => {
                        json!({"type":"error", "kind":format!("{:?}",error.kind()), "message":error.message()})
                    }
                }
            }
            "stats" => {
                json!({"type":"stats", "jobs":hashing.stats().jobs, "entries":auth.config().throttle_entries()})
            }
            "set_policy" => {
                let mut tx = auth.orm().begin(erp_orm::SUPERUSER_ID).await.unwrap();
                let rows = tx.env().records("ir.config_parameter").unwrap();
                let _: String = rows
                    .call(
                        &mut tx,
                        "set_str",
                        (
                            request["key"].as_str().unwrap().to_owned(),
                            Some(request["value"].as_str().unwrap().to_owned()),
                        ),
                    )
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                json!({"type":"policy_written"})
            }
            "check_policy" => {
                let cookie =
                    SessionSecret::from_cookie(request["cookie"].as_str().unwrap()).unwrap();
                let begun = auth
                    .begin_session(&cookie, erp_security::auth::CredentialOptions::new())
                    .await
                    .unwrap();
                let answer = json!({"type":"checked_policy", "rotation":begun.session.rotation_due(), "touch":begun.session.touch_due()});
                let mut tx = begun.tx;
                tx.rollback().await.unwrap();
                answer
            }
            "start_waiting_login" => {
                assert!(
                    pending.is_none() && release.is_none(),
                    "child already has a pending attempt"
                );
                release = Some(hold_permits(hashing, 1).await);
                let task_auth = Arc::clone(&auth);
                let login = request["login"].as_str().unwrap().to_owned();
                let password = request["password"].as_str().unwrap().to_owned();
                let address = request["address"].as_str().unwrap().to_owned();
                pending = Some(tokio::spawn(async move {
                    login_from(&task_auth, &login, &password, &address).await
                }));
                until_waiting(hashing, 1).await;
                json!({"type":"waiting", "waiters":hashing.stats().waiting})
            }
            "finish_waiting_login" => {
                drop(release.take().expect("no held hash permit"));
                login_answer(pending.take().expect("no pending attempt").await.unwrap())
            }
            "drain" => {
                member.drain().await.unwrap();
                json!({"type":"drained", "serving":member.is_serving()})
            }
            "quit" => {
                drop(release.take());
                if let Some(task) = pending.take() {
                    task.await
                        .unwrap()
                        .expect("pending child login failed on quit");
                }
                member.shutdown().await.unwrap();
                println!("{MARK}{}", json!({"type":"stopped"}));
                return;
            }
            _ => panic!("unsupported child operation"),
        };
        println!("{MARK}{response}");
    }
    panic!("parent closed command pipe without explicit shutdown");
}

/// Current executable modelling an unsupported binary declaration, not an old executable.
/// Its real membership calls remain in this separate process; it cannot construct bound Auth.
async fn incompatible_worker(
    identity: Arc<NodeIdentity>,
    heartbeat: erp_testkit::deadpool_postgres::Pool,
) {
    assert_eq!(identity.features().iter().len(), 0);
    println!(
        "{MARK}{}",
        json!({"type":"replacement", "os_pid":std::process::id(),
        "node":identity.id().to_string(), "digest":identity.digest().to_string()})
    );
    let mut member = None;
    let mut stdin = BufReader::new(tokio::io::stdin());
    while let Some(line) = bounded_line(&mut stdin).await.unwrap() {
        let request: Json = serde_json::from_str(&line).expect("invalid command framing");
        let response = match request["op"].as_str().expect("missing child operation") {
            "register" => {
                assert!(member.is_none(), "replacement already registered");
                match NodeMembership::register(Arc::clone(&identity), heartbeat.clone()).await {
                    Ok(registered) => {
                        member = Some(registered);
                        json!({"type":"registered"})
                    }
                    Err(error) => json!({"type":"error", "kind":format!("{:?}", error.kind()),
                        "message":error.to_string(), "sqlstate":error.sqlstate()}),
                }
            }
            "serve" => {
                let member = member.as_ref().expect("replacement not registered");
                await_serving(member).await;
                json!({"type":"serving", "serving":member.is_serving()})
            }
            "quit" => {
                if let Some(member) = member.take() {
                    member.shutdown().await.unwrap();
                }
                println!("{MARK}{}", json!({"type":"stopped"}));
                return;
            }
            _ => panic!("unsupported replacement operation"),
        };
        println!("{MARK}{response}");
    }
    panic!("parent closed replacement pipe without explicit shutdown");
}

/// No Debug implementation: this handle never exposes a bearer-bearing response in a panic.
struct Peer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    os_pid: u32,
    backend: i32,
    node: String,
    digest: Digest,
}
impl Peer {
    async fn start(h: &Harness, alias: bool) -> Self {
        Self::start_mode(h, alias, true).await
    }
    async fn start_mode(h: &Harness, alias: bool, capable: bool) -> Self {
        use erp_testkit::tokio_postgres::config::Host;
        let config = h.db().config();
        assert!(
            config.get_hosts().len() == 1 && config.get_ports().len() == 1,
            "process fixture requires exactly one host and port"
        );
        assert!(
            config.get_options().is_none() && config.get_application_name().is_none(),
            "process fixture cannot silently drop connection options"
        );
        let host = match &config.get_hosts()[0] {
            Host::Tcp(name) => name.clone(),
            Host::Unix(path) => path.to_string_lossy().into_owned(),
        };
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "rollout_child_worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(format!("{PREFIX}HOST"), host)
            .env(format!("{PREFIX}PORT"), config.get_ports()[0].to_string())
            .env(format!("{PREFIX}USER"), config.get_user().unwrap())
            .env(format!("{PREFIX}DB"), config.get_dbname().unwrap())
            .env(
                format!("{PREFIX}MODE"),
                if !capable {
                    "incompatible"
                } else if alias {
                    "alias"
                } else {
                    "base"
                },
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        match config.get_connect_timeout() {
            Some(timeout) => {
                command
                    .env(
                        format!("{PREFIX}CONNECT_TIMEOUT_SECS"),
                        timeout.as_secs().to_string(),
                    )
                    .env(
                        format!("{PREFIX}CONNECT_TIMEOUT_NANOS"),
                        timeout.subsec_nanos().to_string(),
                    );
            }
            None => {
                command
                    .env_remove(format!("{PREFIX}CONNECT_TIMEOUT_SECS"))
                    .env_remove(format!("{PREFIX}CONNECT_TIMEOUT_NANOS"));
            }
        }
        match config.get_password() {
            Some(password) => {
                command.env(
                    format!("{PREFIX}PASSWORD"),
                    String::from_utf8_lossy(password).into_owned(),
                );
            }
            None => {
                command.env_remove(format!("{PREFIX}PASSWORD"));
            }
        }
        let mut child = command.spawn().expect("cannot spawn child");
        let os_pid = child.id().expect("live child has no OS pid");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut peer = Self {
            child,
            stdin,
            stdout,
            os_pid,
            backend: 0,
            node: String::new(),
            digest: Digest::from_bytes([0; 32]),
        };
        let ready = peer.answer().await;
        if !capable {
            assert!(ready["type"] == "replacement", "wrong replacement worker");
            assert_eq!(ready["os_pid"].as_u64(), Some(u64::from(os_pid)));
            peer.node = ready["node"].as_str().unwrap().to_owned();
            peer.digest = ready["digest"].as_str().unwrap().parse().unwrap();
            return peer;
        }
        assert!(
            ready["type"] == "constructed",
            "child failed to construct Auth"
        );
        assert!(
            ready["os_pid"].as_u64() == Some(u64::from(os_pid)),
            "wrong child OS identity"
        );
        assert!(
            ready["base_domain"].as_bool() == Some(!alias)
                && ready["base_order"].as_bool() == Some(!alias)
                && ready["base_credentials"].as_bool() == Some(true),
            "wrong child dispatch composition"
        );
        peer.backend = i32::try_from(ready["backend_pid"].as_i64().unwrap()).unwrap();
        peer.node = ready["node"].as_str().unwrap().to_owned();
        peer.digest = ready["digest"].as_str().unwrap().parse().unwrap();
        peer
    }
    async fn answer(&mut self) -> Json {
        tokio::time::timeout(LIMIT, async {
            loop {
                let line = bounded_line(&mut self.stdout)
                    .await
                    .expect("invalid bounded child output")
                    .expect("child ended before response");
                if let Some((_, payload)) = line.split_once(MARK) {
                    return serde_json::from_str(payload).expect("invalid child response framing");
                }
            }
        })
        .await
        .expect("child response deadline expired")
    }
    async fn call(&mut self, request: Json) -> Json {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "child exited before command"
        );
        let payload = serde_json::to_vec(&request).expect("cannot encode child command");
        assert!(payload.len() < MAX_LINE, "oversized child command");
        self.stdin
            .write_all(&payload)
            .await
            .expect("cannot write child command");
        self.stdin.write_all(b"\n").await.unwrap();
        self.stdin.flush().await.unwrap();
        self.answer().await
    }
    async fn login(&mut self, submitted: &str, password: &str, address: &str) -> Json {
        self.call(json!({"op":"login","login":submitted,"password":password,"address":address}))
            .await
    }
    async fn check(&mut self, secret: &SessionSecret, uid: i64) {
        let answer = self
            .call(json!({"op":"check", "cookie":secret.to_cookie().as_str()}))
            .await;
        assert!(
            answer["type"] == "checked" && answer["uid"].as_i64() == Some(uid),
            "cross-process session check failed"
        );
    }
    async fn finish(mut self) {
        let answer = self.call(json!({"op":"quit"})).await;
        assert!(
            answer["type"] == "stopped",
            "child failed to stop membership"
        );
        let status = tokio::time::timeout(LIMIT, self.child.wait())
            .await
            .expect("child did not exit")
            .unwrap();
        assert!(status.success(), "child exited unsuccessfully");
    }
    async fn kill(mut self) {
        self.child
            .kill()
            .await
            .expect("cannot terminate test-owned child");
    }
}

fn expect_error(answer: &Json, kind: &str, message: &str) {
    assert!(answer["type"] == "error", "expected credential refusal");
    assert_eq!(answer["kind"].as_str(), Some(kind));
    assert_eq!(answer["message"].as_str(), Some(message));
}
fn private_cookie(mut answer: Json, expected: i64) -> SessionSecret {
    assert!(
        answer["type"] == "login" && answer["uid"].as_i64() == Some(expected),
        "login did not return expected identity"
    );
    let cookie = answer["cookie"].take();
    SessionSecret::from_cookie(cookie.as_str().expect("cookie missing"))
        .expect("invalid cookie encoding")
}

struct Fixture {
    h: Harness,
    uid: i64,
    operator: erp_testkit::deadpool_postgres::Pool,
    maintenance: String,
    request: String,
}
impl Fixture {
    async fn new() -> Self {
        let h = harness_builder(true).build().await.unwrap();
        prepare(&h).await;
        let mut signal_tx = h.begin().await.unwrap();
        signal_tx.install_signal().await.unwrap();
        signal_tx.commit().await.unwrap();
        set_param(&h, "base.login_cooldown_after", "100").await;
        set_param(&h, "base.login_cooldown_after_login", "2").await;
        let mut tx = h.begin().await.unwrap();
        let uid = create(
            &mut tx,
            "res.users",
            &[
                ("name", s("Process canonical")),
                ("login", s("canonical")),
                ("password", s("correct")),
                ("alias", s("process_alias")),
                ("priority", Value::from(1)),
            ],
        )
        .await;
        let maintenance = format!("pm_{}", h.db().name());
        let request = format!("pr_{}", h.db().name());
        tx.batch(&format!(
            "CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN",
            erp_sql::quote(&maintenance),
            erp_sql::quote(&request)
        ))
        .await
        .unwrap();
        tx.grant_node_feature_roles(&maintenance, &request)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let operator =
            erp_orm::node::heartbeat_pool(h.db().config(), &NodeOptions::default()).unwrap();
        feature().register(&operator).await.unwrap();
        Self {
            h,
            uid,
            operator,
            maintenance,
            request,
        }
    }
    async fn peers(&self) -> (Peer, Peer) {
        let (mut base, mut alias) =
            tokio::join!(Peer::start(&self.h, false), Peer::start(&self.h, true));
        assert_ne!(base.os_pid, alias.os_pid);
        assert_ne!(base.backend, alias.backend);
        assert_ne!(base.node, alias.node);
        assert_ne!(base.digest, alias.digest);
        // Addon inventories differ; ordinary two-version readiness remains mandatory alongside
        // exact feature approval. This window grants no feature activation authority.
        Rollout::open(
            &self.operator,
            alias.digest,
            Some(base.digest),
            Duration::from_secs(3600),
            Some("isolated-process-test"),
        )
        .await
        .unwrap();
        let (a, b) = tokio::join!(
            base.call(json!({"op":"serve"})),
            alias.call(json!({"op":"serve"}))
        );
        assert!(
            a["serving"] == true && b["serving"] == true,
            "both children must actually serve"
        );
        let revision = match std::env::var("REVISION") {
            Ok(value) => value,
            Err(_) => "unverified-no-REVISION-env".to_owned(),
        };
        println!(
            "rollout_process_evidence source_revision={revision} base_os_pid={} alias_os_pid={} base_backend={} alias_backend={} base_node={} alias_node={}",
            base.os_pid, alias.os_pid, base.backend, alias.backend, base.node, alias.node
        );
        (base, alias)
    }
    async fn enable(&self, peers: &[&Peer]) {
        for peer in peers {
            feature()
                .approve(&self.operator, peer.digest)
                .await
                .unwrap();
        }
        feature()
            .enable(
                &self.operator,
                LegacyEnrollment::confirmed_drained_and_fenced(),
            )
            .await
            .unwrap();
    }
    async fn disable(&self) {
        tokio::time::timeout(Duration::from_secs(20), feature().disable(&self.operator))
            .await
            .expect("disable blocked behind connection-free verification")
            .unwrap();
    }
    async fn cleanup(self) {
        let mut tx = self.h.begin().await.unwrap();
        tx.batch(&format!(
            "DROP OWNED BY {}, {} CASCADE; DROP ROLE {}, {}",
            erp_sql::quote(&self.maintenance),
            erp_sql::quote(&self.request),
            erp_sql::quote(&self.maintenance),
            erp_sql::quote(&self.request)
        ))
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_share_principal_budget_and_survive_node_loss() {
    let fixture = Fixture::new().await;
    let (mut base, mut alias) = fixture.peers().await;
    fixture.enable(&[&base, &alias]).await;
    let secret = private_cookie(
        alias
            .login("alias:process_alias", "correct", "198.51.100.220")
            .await,
        fixture.uid,
    );
    base.check(&secret, fixture.uid).await;
    expect_error(
        &alias
            .login("alias:process_alias", "wrong", "198.51.100.221")
            .await,
        "AccessDenied",
        "Access Denied",
    );
    expect_error(
        &base.login("canonical", "wrong", "198.51.100.222").await,
        "AccessDenied",
        "Access Denied",
    );
    assert_eq!(
        count(
            &fixture.h,
            "erp_login_throttle WHERE kind='principal' AND failures=2"
        )
        .await,
        1
    );
    let before = base.call(json!({"op":"stats"})).await["jobs"]
        .as_u64()
        .unwrap();
    expect_error(
        &base.login("canonical", "correct", "198.51.100.223").await,
        "AccessDenied",
        erp_security::auth::COOLDOWN_MESSAGE,
    );
    assert_eq!(
        base.call(json!({"op":"stats"})).await["jobs"]
            .as_u64()
            .unwrap(),
        before
    );
    alias.kill().await;
    assert!(feature().status(&fixture.operator).await.unwrap().enabled());
    assert_eq!(
        count(
            &fixture.h,
            "erp_login_throttle WHERE kind='principal' AND failures=2"
        )
        .await,
        1
    );
    base.check(&secret, fixture.uid).await;
    expect_error(
        &base.login("canonical", "correct", "198.51.100.224").await,
        "AccessDenied",
        erp_security::auth::COOLDOWN_MESSAGE,
    );
    base.finish().await;
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_preserve_generation_boundary_and_disable_semantics() {
    let fixture = Fixture::new().await;
    let (mut base, mut alias) = fixture.peers().await;
    fixture.enable(&[&base, &alias]).await;
    let secret = private_cookie(
        alias
            .login("alias:process_alias", "correct", "198.51.100.230")
            .await,
        fixture.uid,
    );
    let sessions = count(&fixture.h, "erp_session").await;
    let waiting = alias.call(json!({"op":"start_waiting_login", "login":"alias:process_alias", "password":"correct", "address":"198.51.100.231"})).await;
    assert!(
        waiting["type"] == "waiting" && waiting["waiters"].as_u64().is_some_and(|n| n >= 1),
        "child never reached hash-permit barrier"
    );
    fixture.disable().await;
    fixture.enable(&[&base, &alias]).await;
    expect_error(
        &alias.call(json!({"op":"finish_waiting_login"})).await,
        "Programming",
        "login selection rollout generation changed",
    );
    assert_eq!(count(&fixture.h, "erp_session").await, sessions);
    fixture.disable().await;
    let before = alias.call(json!({"op":"stats"})).await["jobs"]
        .as_u64()
        .unwrap();
    expect_error(
        &alias
            .login("alias:process_alias", "correct", "198.51.100.232")
            .await,
        "Programming",
        "login selection rollout is disabled",
    );
    assert_eq!(
        alias.call(json!({"op":"stats"})).await["jobs"]
            .as_u64()
            .unwrap(),
        before
    );
    base.check(&secret, fixture.uid).await;
    let canonical = private_cookie(
        base.login("canonical", "correct", "198.51.100.233").await,
        fixture.uid,
    );
    base.check(&canonical, fixture.uid).await;
    let drained = alias.call(json!({"op":"drain"})).await;
    assert!(
        drained["type"] == "drained" && drained["serving"] == false,
        "child did not drain"
    );
    expect_error(
        &alias
            .login("alias:process_alias", "correct", "198.51.100.234")
            .await,
        "Programming",
        "login selection membership is not serving",
    );
    tokio::join!(base.finish(), alias.finish());
    fixture.cleanup().await;
}

async fn security_rows(h: &Harness) -> String {
    let mut tx = h.begin().await.unwrap();
    let rows = tx.query("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY kind,key_hash),'[]'::jsonb)::text FROM erp_login_throttle t", &[]).await.unwrap();
    let snapshot = rows[0].get(0);
    tx.rollback().await.unwrap();
    snapshot
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_replacement_joins_only_after_disable_and_drain() {
    let fixture = Fixture::new().await;
    let (mut base, mut alias) = fixture.peers().await;
    fixture.enable(&[&base, &alias]).await;
    let secret = private_cookie(
        alias
            .login("alias:process_alias", "correct", "198.51.100.240")
            .await,
        fixture.uid,
    );
    expect_error(
        &alias
            .login("alias:process_alias", "wrong", "198.51.100.241")
            .await,
        "AccessDenied",
        "Access Denied",
    );
    assert_eq!(
        count(
            &fixture.h,
            "erp_login_throttle WHERE kind='principal' AND failures=1"
        )
        .await,
        1
    );
    assert!(
        count(
            &fixture.h,
            "erp_login_throttle WHERE kind='principal_trusted'"
        )
        .await
            > 0
    );
    let before = security_rows(&fixture.h).await;
    let mut replacement = Peer::start_mode(&fixture.h, false, false).await;
    assert_ne!(replacement.os_pid, base.os_pid);
    assert_ne!(replacement.os_pid, alias.os_pid);
    assert_ne!(replacement.digest, base.digest);
    // Retry the very same identity/process: neither failed attempt may create or extend a row.
    for _ in 0..2 {
        let refused = replacement.call(json!({"op":"register"})).await;
        expect_error(
            &refused,
            "Database",
            &format!(
                "database {}: the registration failed: node feature auth.login_principal refuses unapproved digest",
                fixture.h.db().name()
            ),
        );
        assert_eq!(refused["sqlstate"].as_str(), Some("ERF01"));
        let mut tx = fixture.h.begin().await.unwrap();
        let rows = tx
            .query(
                "SELECT count(*) FROM erp_node WHERE id=$1::text::uuid",
                &[&replacement.node],
            )
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 0);
        tx.rollback().await.unwrap();
    }
    assert!(
        before == security_rows(&fixture.h).await,
        "refused registration changed security rows"
    );
    fixture.disable().await;
    let drained = alias.call(json!({"op":"drain"})).await;
    assert!(drained["type"] == "drained" && drained["serving"] == false);
    alias.finish().await;
    // The ordinary two-version window remains a separate prerequisite for readiness.
    Rollout::open(
        &fixture.operator,
        replacement.digest,
        Some(base.digest),
        Duration::from_secs(3600),
        Some("isolated-rollback-test"),
    )
    .await
    .unwrap();
    assert!(replacement.call(json!({"op":"register"})).await["type"] == "registered");
    let admitted = replacement.call(json!({"op":"serve"})).await;
    assert!(admitted["type"] == "serving" && admitted["serving"] == true);
    let mut tx = fixture.h.begin().await.unwrap();
    let row = tx.query("SELECT state,capabilities::text,expires_at>clock_timestamp() FROM erp_node WHERE id=$1::text::uuid", &[&replacement.node]).await.unwrap();
    assert_eq!(row.len(), 1);
    assert_eq!(row[0].get::<_, String>(0), "ready");
    assert_eq!(row[0].get::<_, String>(1), "{}");
    assert!(row[0].get::<_, bool>(2));
    tx.rollback().await.unwrap();
    assert!(!feature().status(&fixture.operator).await.unwrap().enabled());
    assert!(
        before == security_rows(&fixture.h).await,
        "rollback changed counters or trust"
    );
    base.check(&secret, fixture.uid).await;
    println!(
        "rollback_process_evidence replacement_os_pid={} replacement_node={} incompatible_capabilities=empty serving_after_disable=true",
        replacement.os_pid, replacement.node
    );
    tokio::join!(base.finish(), replacement.finish());
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_observe_session_policy_with_independent_capacity() {
    let fixture = Fixture::new().await;
    let (mut a, mut b) = fixture.peers().await;
    assert_eq!(a.call(json!({"op":"stats"})).await["entries"], 64);
    assert_eq!(b.call(json!({"op":"stats"})).await["entries"], 128);
    let secret = private_cookie(
        a.login("canonical", "correct", "198.51.100.240").await,
        fixture.uid,
    );
    b.check(&secret, fixture.uid).await;
    for (key, value) in [
        ("sessions.rotation_grace_seconds", "5"),
        ("sessions.rotation_interval_seconds", "60"),
        ("sessions.touch_interval_seconds", "3600"),
    ] {
        assert_eq!(
            a.call(json!({"op":"set_policy", "key":key, "value":value}))
                .await["type"],
            "policy_written"
        );
    }
    let mut tx = fixture.h.begin().await.unwrap();
    tx.batch("UPDATE erp_session SET created_at=now()-interval '120 seconds', last_seen_at=now()-interval '120 seconds'").await.unwrap();
    tx.commit().await.unwrap();
    for peer in [&mut a, &mut b] {
        let answer = peer
            .call(json!({"op":"check_policy", "cookie":secret.to_cookie().as_str()}))
            .await;
        assert_eq!(
            answer,
            json!({"type":"checked_policy", "rotation":true, "touch":false})
        );
    }
    a.call(json!({"op":"set_policy", "key":"sessions.touch_interval_seconds", "value":"1"}))
        .await;
    let answer = b
        .call(json!({"op":"check_policy", "cookie":secret.to_cookie().as_str()}))
        .await;
    assert_eq!(
        answer,
        json!({"type":"checked_policy", "rotation":true, "touch":true})
    );
    b.kill().await;
    a.check(&secret, fixture.uid).await;
    a.finish().await;
    fixture.cleanup().await;
}
