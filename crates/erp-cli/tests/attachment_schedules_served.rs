// SPDX-License-Identifier: MIT OR Apache-2.0
//! The real `erp server` keeps the attachment schedules: every
//! start creates the recurring row of a kind that has none and leaves existing rows, and their due
//! times, as they are. No runner is configured here, so nothing runs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration fixture assertions and shared helpers"
)]
#[path = "support/native_http.rs"]
mod native_http;
use native_http::*;
use std::fs;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const KINDS: [&str; 4] = [
    "base.attachment_gc",
    "base.attachment_clean_staging",
    "base.attachment_reconcile",
    "base.attachment_owner_heartbeat",
];

/// `(state, run_after as text)` of every row of `kind`, oldest first.
async fn rows(fixture: &NativeFixture, kind: &str) -> Vec<(String, String)> {
    fixture
        .client
        .query(
            "SELECT state, run_after::text FROM erp_job WHERE kind = $1 ORDER BY id",
            &[&kind],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

/// A restarted server process on the fixture's configuration, killed on drop.
struct Restarted(Child);
impl Drop for Restarted {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

fn restart(fixture: &NativeFixture) -> Restarted {
    let stderr_path = fixture.dir.path().join("restarted.stderr");
    let mut server = Restarted(
        Command::new(env!("CARGO_BIN_EXE_erp"))
            .args(["server", "--config"])
            .arg(fixture.dir.path().join("server.toml"))
            .stdout(Stdio::null())
            .stderr(fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if TcpStream::connect(("127.0.0.1", fixture.port)).is_ok() {
            match request(fixture.port, "GET", "/erp/health/ready", &[], "").status {
                200 => break,
                503 => {}
                status => panic!("restarted server readiness returned HTTP {status}"),
            }
        }
        if let Some(status) = server.0.try_wait().unwrap() {
            panic!(
                "restarted server exited {status}: {}",
                fs::read_to_string(&stderr_path).unwrap()
            );
        }
        assert!(
            Instant::now() < deadline,
            "restarted server did not become ready"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    server
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_server_start_keeps_one_attachment_schedule_per_kind() {
    let mut fixture = NativeFixture::start().await;
    let mut first = Vec::new();
    for kind in KINDS {
        let found = rows(&fixture, kind).await;
        assert_eq!(found.len(), 1, "{kind}: {found:?}");
        assert_eq!(found[0].0, "ready", "{kind}");
        first.push(found);
    }
    fixture.server.stop();
    // a removed schedule comes back at the next start; the others keep their due time
    fixture
        .client
        .execute(
            "DELETE FROM erp_job WHERE kind = 'base.attachment_reconcile'",
            &[],
        )
        .await
        .unwrap();
    let restarted = restart(&fixture);
    for (kind, before) in KINDS.into_iter().zip(&first) {
        let found = rows(&fixture, kind).await;
        assert_eq!(found.len(), 1, "{kind} after restart: {found:?}");
        assert_eq!(found[0].0, "ready", "{kind}");
        if kind != "base.attachment_reconcile" {
            assert_eq!(&found, before, "{kind}: the start moved an existing row");
        }
    }
    let second: Vec<_> = {
        let mut all = Vec::new();
        for kind in KINDS {
            all.push(rows(&fixture, kind).await);
        }
        all
    };
    drop(restarted);
    // a start with every row present changes none of them
    let again = restart(&fixture);
    for (kind, before) in KINDS.into_iter().zip(&second) {
        assert_eq!(&rows(&fixture, kind).await, before, "{kind}");
    }
    drop(again);
    fixture.cleanup().await;
}
