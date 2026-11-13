// SPDX-License-Identifier: MIT OR Apache-2.0
//! Same-host protocol fixtures; these do not certify a deployed shared filesystem.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#![cfg(any(target_os = "linux", target_os = "macos"))]
use erp_blob::{
    LocalStore, Namespace,
    peer::{LocalPeer, PEER_STEPS, verify_transcript},
};
use std::sync::Arc;

#[tokio::test]
async fn peer_protocol_checks_both_directions_and_preserves_real_markers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(dir.path()).await.unwrap());
    let real_ns = Namespace::random().unwrap();
    let real_marker = erp_blob::OwnerMarker::new(
        erp_blob::OwnerIdentity::new(42, 1234, real_ns.clone()).unwrap(),
        1,
        Namespace::random().unwrap(),
        0,
    )
    .unwrap();
    use erp_blob::AttachmentStore;
    let real_version = store.put_owner(&real_ns, &real_marker, None).await.unwrap();
    store
        .create_contested(&real_ns, &real_marker)
        .await
        .unwrap();
    let session = Namespace::random().unwrap();
    std::fs::write(dir.path().join("unrelated"), b"preserve").unwrap();
    let mut a = LocalPeer::create(store.clone(), session.clone()).unwrap();
    let mut b = LocalPeer::join(store, session.clone()).unwrap();
    let mut left = Vec::new();
    let mut right = Vec::new();
    for phase in 0..PEER_STEPS {
        left.push(a.step(phase).await.unwrap().value());
        right.push(b.step(phase).await.unwrap().value());
    }
    verify_transcript(&left, &right).unwrap();
    drop(b);
    a.cleanup().unwrap();
    let reopened = LocalStore::open(dir.path()).await.unwrap();
    assert_eq!(
        reopened.owner(&real_ns).await.unwrap().unwrap().version(),
        &real_version
    );
    assert_eq!(
        reopened.contested(&real_ns).await.unwrap(),
        Some(real_marker)
    );
    assert!(!dir.path().join(session.as_str()).exists());
    assert_eq!(
        std::fs::read(dir.path().join("unrelated")).unwrap(),
        b"preserve"
    );
}

#[tokio::test]
async fn peer_protocol_rejects_phase_skip_and_private_namespace_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(dir.path()).await.unwrap());
    let session = Namespace::random().unwrap();
    let mut a = LocalPeer::create(store.clone(), session.clone()).unwrap();
    let e = a.step(1).await.unwrap_err();
    assert!(matches!(e, erp_blob::BlobError::PeerProbe { .. }));
    assert_eq!(
        e.to_string(),
        "attachment peer probe refused: unexpected local phase"
    );
    let e = LocalPeer::create(store, session).unwrap_err();
    assert!(matches!(e, erp_blob::BlobError::PeerProbe { .. }));
    assert_eq!(
        e.to_string(),
        "attachment peer probe refused: probe namespace already exists"
    );
}

#[test]
fn peer_transcript_rejects_double_winner_missing_peer_and_false_guard() {
    for (left, right) in [(vec![], vec![]), (vec![1; 16], vec![1; 16])] {
        let e = verify_transcript(&left, &right).unwrap_err();
        assert!(matches!(e, erp_blob::BlobError::PeerProbe { .. }));
        assert_eq!(
            e.to_string(),
            if left.is_empty() {
                "attachment peer probe refused: two complete transcripts are required"
            } else {
                "attachment peer probe refused: exclusive creation did not have exactly one winner"
            }
        );
    }
}

#[test]
fn peer_process_child() {
    use std::io::{Read, Write};
    let Some(root) = std::env::var_os("ERP_PEER_CHILD_ROOT") else {
        return;
    };
    let ns = Namespace::parse(&std::env::var("ERP_PEER_CHILD_SESSION").unwrap()).unwrap();
    let creator = std::env::var("ERP_PEER_CHILD_SLOT").unwrap() == "0";
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let store = Arc::new(LocalStore::open(root).await.unwrap());
        let mut peer = if creator {
            LocalPeer::create(store, ns)
        } else {
            LocalPeer::join(store, ns)
        }
        .unwrap();
        println!("PEER:ready");
        std::io::stdout().flush().unwrap();
        for phase in 0..PEER_STEPS {
            let mut input = [0];
            std::io::stdin().read_exact(&mut input).unwrap();
            assert_eq!(input[0], phase);
            let value = peer.step(phase).await.unwrap().value();
            println!("PEER:{value}");
            std::io::stdout().flush().unwrap();
        }
        let mut done = [0];
        std::io::stdin().read_exact(&mut done).unwrap();
        if creator {
            peer.cleanup().unwrap();
        }
    });
}

#[test]
fn independent_peer_processes_complete_one_consistent_transcript() {
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Command, Stdio},
    };
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::random().unwrap();
    let spawn = |slot: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "peer_process_child", "--nocapture"])
            .env("ERP_PEER_CHILD_ROOT", dir.path())
            .env("ERP_PEER_CHILD_SESSION", ns.as_str())
            .env("ERP_PEER_CHILD_SLOT", slot)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    };
    fn read(reader: &mut impl BufRead) -> String {
        for _ in 0..20 {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if let Some(value) = line.trim().strip_prefix("PEER:") {
                return value.to_owned();
            }
        }
        panic!("child protocol output missing");
    }
    let mut a = spawn("0");
    let mut ra = BufReader::new(a.stdout.take().unwrap());
    assert_eq!(read(&mut ra), "ready");
    let mut b = spawn("1");
    let mut rb = BufReader::new(b.stdout.take().unwrap());
    assert_eq!(read(&mut rb), "ready");
    let mut left = Vec::new();
    let mut right = Vec::new();
    for phase in 0..PEER_STEPS {
        a.stdin.as_mut().unwrap().write_all(&[phase]).unwrap();
        b.stdin.as_mut().unwrap().write_all(&[phase]).unwrap();
        left.push(read(&mut ra).parse().unwrap());
        right.push(read(&mut rb).parse().unwrap());
    }
    verify_transcript(&left, &right).unwrap();
    b.stdin.as_mut().unwrap().write_all(&[PEER_STEPS]).unwrap();
    assert!(b.wait().unwrap().success());
    a.stdin.as_mut().unwrap().write_all(&[PEER_STEPS]).unwrap();
    assert!(a.wait().unwrap().success());
    assert!(!dir.path().join(ns.as_str()).exists());
}

#[tokio::test]
async fn lost_holder_does_not_produce_a_successful_guard_observation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(dir.path()).await.unwrap());
    let ns = Namespace::random().unwrap();
    let mut a = LocalPeer::create(store.clone(), ns.clone()).unwrap();
    let mut b = LocalPeer::join(store, ns).unwrap();
    for phase in 0..=4 {
        a.step(phase).await.unwrap();
        b.step(phase).await.unwrap();
    }
    drop(a);
    let e = b.step(5).await.unwrap_err();
    assert!(matches!(e, erp_blob::BlobError::PeerProbe { .. }));
    assert_eq!(
        e.to_string(),
        "attachment peer probe refused: peer advisory guard did not exclude a second holder"
    );
}

#[tokio::test]
async fn split_roots_and_unsafe_namespaces_cannot_pass_visibility() {
    use std::os::unix::fs::symlink;
    let aroot = tempfile::tempdir().unwrap();
    let broot = tempfile::tempdir().unwrap();
    let a = Arc::new(LocalStore::open(aroot.path()).await.unwrap());
    let b = Arc::new(LocalStore::open(broot.path()).await.unwrap());
    let ns = Namespace::random().unwrap();
    let mut left = LocalPeer::create(a.clone(), ns.clone()).unwrap();
    std::fs::create_dir(broot.path().join(ns.as_str())).unwrap();
    let mut right = LocalPeer::join(b, ns.clone()).unwrap();
    left.step(0).await.unwrap();
    right.step(0).await.unwrap();
    let e = left.step(1).await.unwrap_err();
    assert!(matches!(e, erp_blob::BlobError::PeerProbe { .. }));
    assert_eq!(
        e.to_string(),
        "attachment peer probe refused: peer challenge is missing or differs"
    );
    let unsafe_ns = Namespace::random().unwrap();
    symlink(broot.path(), aroot.path().join(unsafe_ns.as_str())).unwrap();
    let e = LocalPeer::join(a, unsafe_ns).unwrap_err();
    assert!(matches!(e, erp_blob::BlobError::PeerProbe { .. }));
    assert_eq!(
        e.to_string(),
        "attachment peer probe refused: unsafe probe namespace"
    );
}
