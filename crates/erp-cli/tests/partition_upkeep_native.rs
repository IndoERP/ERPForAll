// SPDX-License-Identifier: MIT OR Apache-2.0
//! Real native serving startup and periodic policy admission.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::*;
use std::{
    fs,
    net::TcpListener,
    process::{Child, Command, Stdio},
    time::Duration,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_partition_policy_gates_startup_and_periodic_work() {
    let fixture = NativeFixture::start().await;
    fixture.client.execute(
        "UPDATE ir_config_parameter SET value='fixture-invalid-policy' WHERE key='base.partition_upkeep_interval_secs'", &[]
    ).await.unwrap();
    let temporary = std::env::temp_dir().canonicalize().unwrap();
    let dir = tempfile::tempdir_in(temporary).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let path = config(
        dir.path(),
        &fixture.db,
        port,
        &fixture.role,
        &fixture.dir.path().join("addons"),
    );
    let startup_log = dir.path().join("startup.stderr");
    let mut child = StartupProbe(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&startup_log).unwrap())
            .spawn()
            .unwrap(),
    );
    let status = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a corrupt stored policy must refuse startup before serving");
    assert!(!status.success());
    let diagnostic = fs::read_to_string(&startup_log).unwrap();
    // The surrounding tracing/CLI prefixes contain timestamps and are not stable.
    assert!(
        diagnostic
            .contains("base.partition_upkeep_interval_secs must be an integer from 60 to 3600."),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("fixture-invalid-policy"),
        "stored text escaped into startup diagnostics: {diagnostic}"
    );
    fixture.client.execute(
        "UPDATE ir_config_parameter SET value='60' WHERE key='base.partition_upkeep_interval_secs'", &[]
    ).await.unwrap();
    let mut peer = fixture.peer();
    assert_eq!(
        request(peer.port, "GET", "/erp/health/ready", &[], "").status,
        200
    );
    fixture.client.execute(
        "UPDATE ir_config_parameter SET value='fixture-invalid-policy' WHERE key='base.partition_upkeep_interval_secs'", &[]
    ).await.unwrap();
    let stderr = peer.dir.path().join("server.stderr");
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let log = fs::read_to_string(&stderr).unwrap();
            if log.contains("generic partition upkeep policy is invalid; maintenance skipped") {
                assert!(
                    log.contains(
                        "base.partition_upkeep_interval_secs must be an integer from 60 to 3600."
                    ),
                    "{log}"
                );
                assert!(
                    !log.contains("fixture-invalid-policy"),
                    "stored text escaped into periodic diagnostics: {log}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("native serving did not execute its configured periodic upkeep");
    assert_eq!(
        request(peer.port, "GET", "/erp/health/ready", &[], "").status,
        200
    );
    peer.server.terminate();
    drop(peer);
    fixture.cleanup().await;
}

/// Kill only this fixture child if an assertion or bounded startup wait fails.
struct StartupProbe(Child);
impl Drop for StartupProbe {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}
