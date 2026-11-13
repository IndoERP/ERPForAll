// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_cli::audit::FileWitness;
use erp_orm::audit::seal::{Anchor, AuditWitness, SealIdentity, WitnessError};
use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn process_child() {
    let Some(path) = std::env::var_os("ERP_WITNESS_TEST_PATH") else {
        return;
    };
    if std::env::var_os("ERP_WITNESS_TEST_HOLD").is_some() {
        let file = std::fs::File::open(path).unwrap();
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        println!("locked");
        std::io::stdout().flush().unwrap();
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
        drop(file);
        return;
    }
    let batch = std::env::var("ERP_WITNESS_TEST_BATCH")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let hash = std::env::var("ERP_WITNESS_TEST_HASH")
        .unwrap()
        .parse::<u8>()
        .unwrap();
    let witness = FileWitness::new(&path).unwrap();
    let anchor = Anchor::new(SealIdentity::new([1; 16], 1, batch).unwrap(), [hash; 32], 1).unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match witness.publish(&anchor) {
            Ok(()) => return,
            Err(WitnessError::Conflict) => std::process::exit(20),
            Err(WitnessError::Busy) if Instant::now() < deadline => std::thread::yield_now(),
            Err(error) => panic!(
                "publish child failed: {error}; batch={batch}; parent={:?}; file={:?}",
                std::fs::metadata(std::path::Path::new(&path).parent().unwrap())
                    .map(|m| m.is_dir()),
                std::fs::metadata(&path).map(|m| m.is_file()),
            ),
        }
    }
}
fn child(path: &std::path::Path, batch: u64, hash: u8) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_child", "--nocapture"])
        .env("ERP_WITNESS_TEST_PATH", path)
        .env("ERP_WITNESS_TEST_BATCH", batch.to_string())
        .env("ERP_WITNESS_TEST_HASH", hash.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

#[test]
fn concurrent_publishers_preserve_distinct_and_identical_records() {
    for (batch, hash, count, conflict) in [(2, 9, 2, false), (1, 9, 1, false), (1, 8, 1, true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("witness.jsonl");
        let mut first = child(&path, 1, 9);
        let mut second = child(&path, batch, hash);
        first.stdin.take().unwrap().write_all(&[1]).unwrap();
        second.stdin.take().unwrap().write_all(&[1]).unwrap();
        let codes = [
            first.wait().unwrap().code().unwrap(),
            second.wait().unwrap().code().unwrap(),
        ];
        if conflict {
            assert!(codes == [0, 20] || codes == [20, 0]);
        } else {
            assert_eq!(codes, [0, 0]);
        }
        assert_eq!(
            FileWitness::new(&path).unwrap().inventory().unwrap().len(),
            count
        );
        assert_eq!(
            std::fs::read(&path)
                .unwrap()
                .iter()
                .filter(|b| **b == b'\n')
                .count(),
            count
        );
    }
}

#[test]
fn killed_holder_releases_lock_without_mutating_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("witness.jsonl");
    std::fs::write(&path, []).unwrap();
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_child", "--nocapture"])
        .env("ERP_WITNESS_TEST_PATH", &path)
        .env("ERP_WITNESS_TEST_HOLD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(holder.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.trim_end().ends_with("locked") {
            break;
        }
        line.clear();
    }
    let witness = FileWitness::new(&path).unwrap();
    assert_eq!(witness.inventory().unwrap_err(), WitnessError::Busy);
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(witness.inventory().unwrap().is_empty());
}
