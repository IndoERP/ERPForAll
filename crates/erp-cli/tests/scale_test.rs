//! Functional shared-state checks across two actual CLI processes; wider scale coverage is pending.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/scale.rs"]
mod scale;
use scale::*;

#[test]
fn harness_stop_reaps_only_owned_child() {
    let mut owned = OwnedChild::spawn(std::process::Command::new("sleep").arg("20")).unwrap();
    let mut other = OwnedChild::spawn(std::process::Command::new("sleep").arg("20")).unwrap();
    let stop = owned.stop(Deadline::after(std::time::Duration::from_secs(2)));
    // Observe before asserting, then clean both children even in the RED version.
    let stopped = owned.child.try_wait().unwrap().is_some();
    let other_alive = other.child.try_wait().unwrap().is_none();
    for child in [&mut owned.child, &mut other.child] {
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
    stop.unwrap();
    owned
        .stop(Deadline::after(std::time::Duration::from_secs(2)))
        .unwrap();
    assert!(
        owned.child.try_wait().unwrap().is_some(),
        "repeated stop remains reaped"
    );
    assert!(stopped, "stop must reap its owned child");
    assert!(
        other_alive,
        "stop must leave unrelated fixture children alive"
    );
}

#[test]
fn harness_reports_failed_child_startup() {
    let mut child =
        OwnedChild::spawn(std::process::Command::new("/bin/sh").args(["-c", "exit 17"])).unwrap();
    child
        .wait(Deadline::after(std::time::Duration::from_secs(2)))
        .unwrap();
    let error = ready(
        &mut child,
        0,
        Deadline::after(std::time::Duration::from_secs(1)),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(
        error.to_string(),
        format!(
            "server pid={} exited before readiness: exit status: 17",
            child.child.id()
        )
    );
}

#[test]
fn harness_readiness_and_command_wait_are_bounded() {
    let mut child = OwnedChild::spawn(std::process::Command::new("sleep").arg("20")).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let error = ready(
        &mut child,
        port,
        Deadline::after(std::time::Duration::from_millis(30)),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(error.to_string(), "scale fixture deadline exhausted");
    let error = child
        .wait(Deadline::after(std::time::Duration::from_millis(30)))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(error.to_string(), "scale fixture deadline exhausted");
    child
        .stop(Deadline::after(std::time::Duration::from_secs(2)))
        .unwrap();
}

#[test]
fn harness_request_reports_truncated_reply_without_echoing_secrets() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut bytes = [0; 1024];
        assert!(stream.read(&mut bytes).unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nshort")
            .unwrap();
    });
    let result = request(
        Deadline::after(std::time::Duration::from_secs(2)),
        port,
        "GET",
        "/",
        &[("cookie", "private-cookie")],
        "",
    );
    peer.join().unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "truncated HTTP reply");
}

// Sequential replay does not cover concurrent overlap or lock-timeout behavior.
#[test]
fn real_cli_nodes_share_session_identity_and_bidirectional_keyed_mutations() {
    use std::time::{Duration, Instant};
    let started = Instant::now();
    let work = Deadline::after(Duration::from_secs(90));
    let total = Deadline::after(Duration::from_secs(115));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut fixture = Fixture::empty();
    // Always run explicit database/role cleanup, including an assertion or startup panic.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(exercise_nodes(&mut fixture, work))
    }));
    let cleanup = runtime.block_on(fixture.cleanup(total));
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "functional fixture including cleanup exceeded its overall budget"
    );
    cleanup.expect("isolated database, runtime role and owned children cleanup");
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn harness_expired_database_admission_retains_owner_for_cleanup() {
    use std::time::Duration;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut fixture = Fixture::empty();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(fixture.start(Deadline::after(Duration::ZERO)))
    }));
    let owned = fixture
        .db
        .as_ref()
        .map(|db| (db.name().to_owned(), db.cluster().clone()));
    runtime
        .block_on(fixture.cleanup(Deadline::after(Duration::from_secs(20))))
        .expect("expired admission must clean the isolated database");
    assert!(outcome.is_err(), "expired work must refuse admission");
    let (name, cluster) =
        owned.expect("expired admission must retain database ownership for cleanup");
    assert!(
        !runtime.block_on(cluster.database_exists(&name)).unwrap(),
        "expired admission left its database behind"
    );
}

async fn exercise_nodes(fixture: &mut Fixture, work: Deadline) {
    fixture.start(work).await;
    let db = fixture.db.as_ref().unwrap();
    let client = work.wait(db.client()).await.unwrap().unwrap();
    let uid: i64 = work
        .wait(client.query_one("SELECT id FROM res_users WHERE login='fixture-admin'", &[]))
        .await
        .unwrap()
        .unwrap()
        .get(0);
    let [a, b] = fixture.ports.as_slice() else {
        panic!("expected exactly two nodes")
    };
    let login = request(work, *a, "GET", "/erp/login", &[], "").unwrap();
    assert_eq!(login.status, 200);
    let prefs = cookie(&login, "prefs");
    let body = format!(
        "db={}&login=fixture-admin&password=fixture-administrator-password&csrf_token={}",
        db.name(),
        embedded(&login, "erp-login")["csrf"].as_str().unwrap()
    );
    let logged = request(
        work,
        *a,
        "POST",
        "/erp/login",
        &[
            ("cookie", &prefs),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        &body,
    )
    .unwrap();
    assert_eq!(logged.status, 303);
    let cookies = format!("{prefs}; {}", cookie(&logged, "session_id"));
    let local = request(work, *a, "GET", "/erp", &[("cookie", &cookies)], "").unwrap();
    let shared = request(work, *b, "GET", "/erp", &[("cookie", &cookies)], "").unwrap();
    assert_eq!(local.status, 200);
    assert_eq!(shared.status, 200);
    let identity_a = embedded(&local, "erp-bootstrap")["identity"].clone();
    let identity_b = embedded(&shared, "erp-bootstrap")["identity"].clone();
    assert_eq!(identity_a["uid"], uid);
    assert_eq!(identity_b["uid"], uid);
    assert_eq!(identity_b, identity_a);
    for cookie_header in [
        None,
        Some(format!(
            "{prefs}; session_id={}.{}",
            db.name(),
            "A".repeat(43)
        )),
    ] {
        let headers: Vec<_> = match &cookie_header {
            Some(cookie) => vec![("cookie", cookie.as_str())],
            None => Vec::new(),
        };
        let refused = request(work, *b, "GET", "/erp", &headers, "").unwrap();
        assert_eq!(refused.status, 303);
        assert_eq!(refused.headers["location"], ["/erp/login?redirect=%2Ferp"]);
        assert!(!refused.body.contains("id=\"erp-bootstrap\""));
    }
    for (source, target, key, name) in [
        (
            *a,
            *b,
            "01234567-89ab-4def-8123-456789abcdef",
            "Scale A to B",
        ),
        (
            *b,
            *a,
            "01234567-89ab-4def-8123-456789abcdee",
            "Scale B to A",
        ),
    ] {
        let body = serde_json::json!({"vals_list":[{"name":name}]}).to_string();
        let headers = [
            ("cookie", cookies.as_str()),
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
            ("idempotency-key", key),
        ];
        let created = request(
            work,
            source,
            "POST",
            "/erp/json/2/acceptance_notes.note/create",
            &headers,
            &body,
        )
        .unwrap();
        assert_eq!(created.status, 200, "keyed create on source node");
        assert!(!created.headers.contains_key("idempotent-replayed"));
        let ids: Vec<i64> = serde_json::from_str(&created.body).unwrap();
        assert_eq!(ids.len(), 1);
        let replay = request(
            work,
            target,
            "POST",
            "/erp/json/2/acceptance_notes.note/create",
            &headers,
            &body,
        )
        .unwrap();
        assert_eq!(replay.status, created.status);
        assert_eq!(replay.body, created.body);
        assert_eq!(
            replay.headers.get("idempotent-replayed"),
            Some(&vec!["true".to_owned()]),
            "cross-process replay must expose the RPC replay indicator"
        );
        let row = work
            .wait(client.query_one(
                "SELECT count(*), min(id), min(owner_id) FROM acceptance_notes_note WHERE name=$1",
                &[&name],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.get::<_, i64>(0),
            1,
            "one DB effect across both processes"
        );
        assert_eq!(row.get::<_, Option<i64>>(1), Some(ids[0]));
        assert_eq!(row.get::<_, Option<i64>>(2), Some(uid));
    }
    for node in &mut fixture.nodes {
        assert!(
            node.child.try_wait().unwrap().is_none(),
            "both servers must remain alive throughout cross-node assertions"
        );
    }
}

macro_rules! pending {
    ($name:ident, $blocker:literal) => {
        #[test]
        #[ignore = $blocker]
        fn $name() {
            panic!($blocker);
        }
    };
}
pending!(
    pending_signal_listener_cross_node_refresh,
    "a later release: real signal/listener wiring required; G5 remains unmet"
);
pending!(
    pending_registry_refresh_between_nodes,
    "a later release: registry publication/invalidation across live processes required"
);
pending!(
    pending_access_policy_refresh_between_nodes,
    "a later release: access-policy listener refresh required"
);
pending!(
    pending_api_key_memo_invalidation_between_nodes,
    "a later release: API-key memo invalidation listener wiring required"
);
pending!(
    pending_bus_reconnect_between_nodes,
    "bus: live bus reconnect and delivery required"
);
pending!(
    pending_jobs_singleton_scheduling_between_nodes,
    "jobs: live singleton/job lease scheduling required"
);
pending!(
    pending_shared_admission_and_rate_limits,
    "SCALE.LIMITS: cluster-wide admission/rate limiting required"
);
pending!(
    pending_load_balancer_and_node_loss,
    "server: load-balancer and writer/process-loss acceptance required; not full"
);
pending!(
    pending_shared_files_between_nodes,
    "SCALE.STORE-PROFILE / SCALE.NODES: shared-file backend and multi-node file acceptance required"
);

/// two actual server processes register distinct fresh ready leases.
#[test]
fn real_cli_nodes_register_ready_memberships() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let work = Deadline::after(std::time::Duration::from_secs(100));
    let total = Deadline::after(std::time::Duration::from_secs(120));
    let mut fixture = Fixture::empty();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            fixture.start(work).await;
            let client = work.wait(fixture.db.as_ref().unwrap().client()).await.unwrap().unwrap();
            let rows = work.wait(client.query("SELECT pid, state, expires_at > now(), capabilities ->> 'auth.login_principal' FROM erp_node WHERE role='app' AND state <> 'stopped' ORDER BY pid", &[])).await.unwrap().unwrap();
            assert_eq!(rows.len(), 2, "both actual server processes must register");
            let mut expected: Vec<i32> = fixture.nodes.iter().map(|node| i32::try_from(node.child.id()).unwrap()).collect();
            expected.sort();
            for (row, pid) in rows.iter().zip(expected) {
                assert_eq!(row.get::<_, i32>(0), pid);
                assert_eq!(row.get::<_, String>(1), "ready");
                assert!(row.get::<_, bool>(2), "ready lease must be fresh");
                assert_eq!(row.get::<_, Option<String>>(3).as_deref(), Some("1"), "native binary must declare login rollout support");
            }
            for port in &fixture.ports {
                let reply = request(work, *port, "GET", "/erp/health/ready", &[], "").unwrap();
                assert_eq!(reply.status, 200);
            }
            let runtime_role = format!("{}_scale", fixture.db.as_ref().unwrap().name());
            let node_role = format!("{runtime_role}_node");
            let privileges = client.query_one(
                "SELECT has_table_privilege($1, 'erp_node','SELECT'), has_table_privilege($2, 'res_users','SELECT')",
                &[&runtime_role, &node_role],
            ).await.unwrap();
            assert!(!privileges.get::<_, bool>(0), "request role must not read membership rows");
            assert!(!privileges.get::<_, bool>(1), "heartbeat role must not read users");
            // Only the still-running owned child is signaled; no process groups or foreign PIDs.
            let child = &mut fixture.nodes[0];
            assert!(child.child.try_wait().unwrap().is_none());
            let pid = child.child.id();
            assert!(std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().unwrap().success());
            assert!(child.wait(work).unwrap().success(), "TERM must shut down cleanly");
            let stopped = client.query_one("SELECT state FROM erp_node WHERE pid=$1", &[&i32::try_from(pid).unwrap()]).await.unwrap();
            assert_eq!(stopped.get::<_, String>(0), "stopped");
            assert_eq!(request(work, fixture.ports[1], "GET", "/erp/health/ready", &[], "").unwrap().status, 200);
        });
    }));
    runtime.block_on(fixture.cleanup(total)).unwrap();
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn real_cli_nodes_login_rollout_shares_durable_principal_accounting() {
    use erp_orm::node::{
        Digest, FeatureKey, FeatureRollout, FeatureVersion, LegacyEnrollment, NodeOptions,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let work = Deadline::after(std::time::Duration::from_secs(110));
    let total = Deadline::after(std::time::Duration::from_secs(130));
    let mut fixture = Fixture::empty();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async{
  fixture.start(work).await;let db=fixture.db.as_ref().unwrap();let client=db.client().await.unwrap();
  let rows=client.query("SELECT pid,digest,capabilities->>'auth.login_principal' FROM erp_node WHERE state='ready' AND expires_at>clock_timestamp() ORDER BY pid",&[]).await.unwrap();assert_eq!(rows.len(),2);
  let feature=FeatureRollout::new(FeatureKey::new("auth.login_principal").unwrap(),FeatureVersion::new(1).unwrap());let pool=erp_orm::node::heartbeat_pool(db.config(),&NodeOptions::default()).unwrap();
  for row in rows {let pid:i32=row.get(0);let digest:Vec<u8>=row.get(1);assert_eq!(row.get::<_,Option<String>>(2).as_deref(),Some("1"));println!("native_rollout_process_pid={pid}");feature.approve(&pool,Digest::from_bytes(digest.try_into().unwrap())).await.unwrap();}
  // This isolated fixture contains only its two owned processes; it does not attest a deployment cutover.
  let on=feature.enable(&pool,LegacyEnrollment::confirmed_drained_and_fenced()).await.unwrap();assert!(on.enabled());assert_eq!(on.generation(),1);
  for port in &fixture.ports {
   let page=request(work,*port,"GET","/erp/login",&[],"").unwrap();assert_eq!(page.status,200);let prefs=cookie(&page,"prefs");let csrf=embedded(&page,"erp-login")["csrf"].as_str().unwrap().to_owned();let body=format!("db={}&login=fixture-admin&password=wrong&csrf_token={csrf}",db.name());
   assert_eq!(request(work,*port,"POST","/erp/login",&[("cookie",&prefs),("content-type","application/x-www-form-urlencoded")],&body).unwrap().status,200);
  }
  let count:i64=client.query_one("SELECT failures::int8 FROM erp_login_throttle WHERE kind='principal'",&[]).await.unwrap().get(0);assert_eq!(count,2);
  let page=request(work,fixture.ports[0],"GET","/erp/login",&[],"").unwrap();let prefs=cookie(&page,"prefs");let csrf=embedded(&page,"erp-login")["csrf"].as_str().unwrap().to_owned();let body=format!("db={}&login=fixture-admin&password=fixture-administrator-password&csrf_token={csrf}",db.name());let logged=request(work,fixture.ports[0],"POST","/erp/login",&[("cookie",&prefs),("content-type","application/x-www-form-urlencoded")],&body).unwrap();assert_eq!(logged.status,303);let cookies=format!("{prefs}; {}",cookie(&logged,"session_id"));
  assert_eq!(request(work,fixture.ports[1],"GET","/erp",&[("cookie",&cookies)],"").unwrap().status,200);
  let off=feature.disable(&pool).await.unwrap();assert!(!off.enabled());assert_eq!(off.generation(),2);assert_eq!(request(work,fixture.ports[1],"GET","/erp",&[("cookie",&cookies)],"").unwrap().status,200);
  let retained:i64=client.query_one("SELECT failures::int8 FROM erp_login_throttle WHERE kind='principal'",&[]).await.unwrap().get(0);assert_eq!(retained,2);
  fixture.nodes[0].stop(work).unwrap();assert_eq!(request(work,fixture.ports[1],"GET","/erp/health/ready",&[],"").unwrap().status,200);
 })
    }));
    runtime.block_on(fixture.cleanup(total)).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[test]
fn real_cli_nodes_login_rollout_startup_failure_cleans_its_membership() {
    use std::os::unix::fs::PermissionsExt;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let work = Deadline::after(std::time::Duration::from_secs(110));
    let total = Deadline::after(std::time::Duration::from_secs(130));
    let mut fixture = Fixture::empty();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            fixture.start(work).await;
            let config = fixture.config_path(0);
            let invalid = config.with_file_name("failed-start.toml");
            let text = std::fs::read_to_string(&config).unwrap().replace(
                "assets_manifest='manifest.json'",
                "assets_manifest='missing-fixture-manifest.json'",
            );
            std::fs::write(&invalid, text).unwrap();
            std::fs::set_permissions(&invalid, std::fs::Permissions::from_mode(0o600)).unwrap();
            let mut child = OwnedChild::spawn(
                std::process::Command::new(env!("CARGO_BIN_EXE_erp"))
                    .args(["server", "--config"])
                    .arg(&invalid)
                    .env("RUST_LOG", "off")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped()),
            )
            .unwrap();
            let pid = i32::try_from(child.child.id()).unwrap();
            let status = child.wait(work).unwrap();
            let mut stderr = String::new();
            std::io::Read::read_to_string(&mut child.child.stderr.take().unwrap(), &mut stderr)
                .unwrap();
            assert_eq!(status.code(), Some(2));
            assert_eq!(
                stderr,
                "Error: Execution error: compiled client asset manifest or content is invalid\n"
            );
            let client = fixture.db.as_ref().unwrap().client().await.unwrap();
            let row = client
                .query_one(
                    "SELECT state,expires_at<=clock_timestamp() FROM erp_node WHERE pid=$1",
                    &[&pid],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, String>(0), "stopped");
            assert!(row.get::<_, bool>(1));
            println!("failed_native_startup_pid={pid} membership=stopped");
            let role = format!("{}_scale", fixture.db.as_ref().unwrap().name());
            client
                .batch_execute(&format!(
                    "GRANT UPDATE(generation) ON erp_node_feature TO {}",
                    erp_sql::quote(&role)
                ))
                .await
                .unwrap();
            let mut leaked = OwnedChild::spawn(
                std::process::Command::new(env!("CARGO_BIN_EXE_erp"))
                    .args(["server", "--config"])
                    .arg(&config)
                    .env("RUST_LOG", "off")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped()),
            )
            .unwrap();
            let leaked_pid = i32::try_from(leaked.child.id()).unwrap();
            let status = leaked.wait(work).unwrap();
            let mut diagnostic = String::new();
            std::io::Read::read_to_string(
                &mut leaked.child.stderr.take().unwrap(),
                &mut diagnostic,
            )
            .unwrap();
            client
                .batch_execute(&format!(
                    "REVOKE UPDATE(generation) ON erp_node_feature FROM {}",
                    erp_sql::quote(&role)
                ))
                .await
                .unwrap();
            assert_eq!(status.code(), Some(2));
            assert_eq!(
                diagnostic,
                "Error: request role must not access node membership tables\n"
            );
            assert_eq!(
                client
                    .query_one("SELECT count(*) FROM erp_node WHERE pid=$1", &[&leaked_pid])
                    .await
                    .unwrap()
                    .get::<_, i64>(0),
                0
            );
        })
    }));
    runtime.block_on(fixture.cleanup(total)).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
