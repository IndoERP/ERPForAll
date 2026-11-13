// SPDX-License-Identifier: MIT OR Apache-2.0
//! Included in local::owner so fault points stay private to the unit-test binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Clock(AtomicU64);
impl Hooks for Clock {
    fn now(&self) -> Result<u64, BlobError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
fn marker(ns: &Namespace) -> OwnerMarker {
    OwnerMarker::new(
        crate::OwnerIdentity::new(42, 16384, ns.clone()).unwrap(),
        1,
        Namespace::random().unwrap(),
        1234,
    )
    .unwrap()
}
fn fixture() -> (tempfile::TempDir, Namespace, Directory, Clock) {
    let root = tempfile::tempdir().unwrap();
    let ns = Namespace::random().unwrap();
    let dir = Directory::open(root.path(), &ns).unwrap();
    (root, ns, dir, Clock(AtomicU64::new(1000)))
}
fn unavailable(error: BlobError, message: &str) {
    assert!(matches!(
        error,
        BlobError::Unavailable {
            retryable: true,
            ..
        }
    ));
    assert_eq!(
        error.to_string(),
        format!("the attachment store is unavailable (retryable): {message}")
    );
}

#[test]
fn lease_records_are_bounded_and_expiry_is_checked() {
    let ns = Namespace::random().unwrap();
    let held = Lease::new(&marker(&ns), 1000).unwrap();
    assert_eq!(Lease::decode(&held.encode()).unwrap(), held);
    for bad in [vec![], vec![0; 257], vec![255; 80]] {
        let error = Lease::decode(&bad).unwrap_err();
        assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
        assert_eq!(
            error.to_string(),
            "the attachment owner marker is invalid: expected bounded local owner lease"
        );
    }
    let error = Lease::new(&marker(&ns), u64::MAX).unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: local owner lease expiry overflow"
    );
}

#[test]
fn expired_lock_recovery_never_releases_a_successor() {
    let (_root, ns, dir, clock) = fixture();
    let first = acquire(&dir, &marker(&ns), &clock).unwrap();
    unavailable(
        acquire(&dir, &marker(&ns), &clock).unwrap_err(),
        "local owner lease is busy; retry",
    );
    clock.0.store(2000, Ordering::SeqCst);
    let next = acquire(&dir, &marker(&ns), &clock).unwrap();
    unavailable(
        release(&dir, &first, &clock).unwrap_err(),
        "local owner lease was lost; retry",
    );
    assert_eq!(read_lease(&dir).unwrap(), Some(next.clone()));
    release(&dir, &next, &clock).unwrap();
}

#[test]
fn incomplete_lock_refuses_instead_of_guessing_expiry() {
    let (_root, ns, dir, clock) = fixture();
    dir.create(LOCK, b"incomplete").unwrap();
    let error = acquire(&dir, &marker(&ns), &clock).unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: expected bounded local owner lease"
    );
    assert_eq!(dir.read(LOCK).unwrap().unwrap(), b"incomplete");
}

#[test]
fn changed_expired_candidate_is_restored_without_replacement() {
    let (_root, ns, dir, clock) = fixture();
    let expired = acquire(&dir, &marker(&ns), &clock).unwrap();
    clock.0.store(2000, Ordering::SeqCst);
    release(&dir, &expired, &clock).unwrap();
    let successor = acquire(&dir, &marker(&ns), &clock).unwrap();
    let _guard = dir.guard().unwrap();
    assert!(!break_expired(&dir, &expired, &clock).unwrap());
    assert_eq!(read_lease(&dir).unwrap(), Some(successor));
}

#[test]
fn restore_collision_preserves_both_holders() {
    let (_root, ns, dir, clock) = fixture();
    let original = acquire(&dir, &marker(&ns), &clock).unwrap();
    let moved = ".owner.recovery-test";
    dir.rename_no_replace(LOCK, moved).unwrap();
    let successor = Lease::new(&marker(&ns), 2000).unwrap();
    dir.create(LOCK, &successor.encode()).unwrap();
    let error = restore(&dir, moved).unwrap_err();
    unavailable(
        error,
        "local owner lease restoration failed; recovery file retained",
    );
    assert_eq!(
        Lease::decode(&dir.read(moved).unwrap().unwrap()).unwrap(),
        original
    );
    assert_eq!(read_lease(&dir).unwrap(), Some(successor));
}

struct Fault {
    phase: Phase,
}
impl Hooks for Fault {
    fn now(&self) -> Result<u64, BlobError> {
        Ok(1000)
    }
    fn at(&self, phase: Phase, _dir: &Directory) -> Result<(), BlobError> {
        if phase == self.phase {
            Err(busy("injected local owner I/O failure"))
        } else {
            Ok(())
        }
    }
}
#[test]
fn publication_faults_are_errors_and_keep_a_complete_recoverable_owner() {
    for phase in [
        Phase::BeforeTempSync,
        Phase::BeforePublish,
        Phase::AfterPublish,
        Phase::BeforeDirectorySync,
        Phase::BeforeRelease,
    ] {
        let (_root, ns, dir, clock) = fixture();
        let first = marker(&ns);
        let version = put(&dir, &ns, &first, None, &clock).unwrap();
        let next = marker(&ns);
        let error = put(&dir, &ns, &next, Some(&version), &Fault { phase }).unwrap_err();
        unavailable(error, "injected local owner I/O failure");
        let found = current(&dir, &ns).unwrap().unwrap();
        if matches!(phase, Phase::BeforeTempSync | Phase::BeforePublish) {
            assert_eq!(found.marker(), &first);
        } else {
            assert_eq!(found.marker(), &next);
        }
    }
}

#[test]
fn no_replace_and_guard_probes_do_not_overwrite_or_admit_contention() {
    let (_root, _ns, dir, _clock) = fixture();
    dir.create("a", b"a").unwrap();
    dir.create("b", b"b").unwrap();
    let error = dir.rename_no_replace("a", "b").unwrap_err();
    assert_eq!(error, rustix::io::Errno::EXIST);
    assert_eq!(dir.read("a").unwrap().unwrap(), b"a");
    assert_eq!(dir.read("b").unwrap().unwrap(), b"b");
    let _guard = dir.guard().unwrap();
    unavailable(dir.guard().unwrap_err(), "local owner guard is busy; retry");
}

#[test]
fn namespace_symlink_and_nonregular_markers_refuse() {
    use std::os::unix::fs::symlink;
    let (root, ns, dir, _clock) = fixture();
    std::fs::create_dir(root.path().join(ns.as_str()).join("_owner")).unwrap();
    let error = current(&dir, &ns).unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: local marker must be a regular file without symlinks"
    );
    let foreign = Namespace::random().unwrap();
    symlink(
        root.path().join(ns.as_str()),
        root.path().join(foreign.as_str()),
    )
    .unwrap();
    let error = Directory::open(root.path(), &foreign).unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: local namespace must be a directory without symlinks"
    );
}

struct ProcessHooks {
    phase: String,
    clock: std::path::PathBuf,
}
impl Hooks for ProcessHooks {
    fn now(&self) -> Result<u64, BlobError> {
        let text =
            std::fs::read_to_string(&self.clock).map_err(|e| io("read the fixture clock", e))?;
        text.parse().map_err(|_| invalid("invalid fixture clock"))
    }
    fn at(&self, phase: Phase, _dir: &Directory) -> Result<(), BlobError> {
        if format!("{phase:?}") == self.phase {
            println!("OWNER_PAUSED");
            std::io::stdout().flush().unwrap();
            let mut byte = [0];
            std::io::stdin().read_exact(&mut byte).unwrap();
        }
        Ok(())
    }
}

// Environment selection exists only inside this test module, never in the shipped library.
#[test]
fn owner_process_child() {
    let Some(root) = std::env::var_os("ERP_OWNER_UNIT_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let ns = Namespace::parse(&std::env::var("ERP_OWNER_UNIT_NS").unwrap()).unwrap();
    let hooks = ProcessHooks {
        phase: std::env::var("ERP_OWNER_UNIT_PHASE").unwrap(),
        clock: root.join("clock"),
    };
    let dir = Directory::open(&root, &ns).unwrap();
    let initial = current(&dir, &ns).unwrap().unwrap();
    match put(&dir, &ns, &marker(&ns), Some(initial.version()), &hooks) {
        Ok(_) => println!("OWNER_DONE:OK"),
        Err(error) => println!("OWNER_DONE:{error}"),
    }
}

struct Child {
    process: std::process::Child,
    output: std::sync::mpsc::Receiver<String>,
    reader: Option<std::thread::JoinHandle<()>>,
    reaped: bool,
}
impl Child {
    fn start(root: &Path, ns: &Namespace, phase: Phase) -> Self {
        use std::io::BufRead;
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "local::owner::tests::owner_process_child",
                "--nocapture",
            ])
            .env("ERP_OWNER_UNIT_ROOT", root)
            .env("ERP_OWNER_UNIT_NS", ns.as_str())
            .env("ERP_OWNER_UNIT_PHASE", format!("{phase:?}"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = process.stdout.take().unwrap();
        let (send, output) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                // A closed receiver means the parent already finished or failed its assertion.
                if send.send(line.unwrap()).is_err() {
                    return;
                }
            }
        });
        let child = Self {
            process,
            output,
            reader: Some(reader),
            reaped: false,
        };
        child.until("OWNER_PAUSED");
        child
    }
    fn until(&self, prefix: &str) -> String {
        loop {
            let line = self
                .output
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap();
            if line.starts_with(prefix) {
                return line;
            }
        }
    }
    fn resume(mut self) -> String {
        self.process.stdin.take().unwrap().write_all(b"x").unwrap();
        let result = self.until("OWNER_DONE:");
        assert!(self.process.wait().unwrap().success());
        self.reaped = true;
        self.reader.take().unwrap().join().unwrap();
        result
    }
    fn terminate(mut self) {
        self.process.kill().unwrap();
        self.process.wait().unwrap();
        self.reaped = true;
        self.reader.take().unwrap().join().unwrap();
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            match self.process.try_wait().unwrap() {
                Some(_) => {}
                None => {
                    self.process.kill().unwrap();
                    self.process.wait().unwrap();
                }
            }
        }
    }
}

#[test]
fn paused_holder_before_validation_loses_to_the_successor() {
    let (root, ns, dir, clock) = fixture();
    std::fs::write(root.path().join("clock"), b"1000").unwrap();
    let version = put(&dir, &ns, &marker(&ns), None, &clock).unwrap();
    let child = Child::start(root.path(), &ns, Phase::AfterAcquire);
    clock.0.store(2000, Ordering::SeqCst);
    std::fs::write(root.path().join("clock"), b"2000").unwrap();
    let winner = marker(&ns);
    let won = put(&dir, &ns, &winner, Some(&version), &clock).unwrap();
    assert_eq!(
        child.resume(),
        "OWNER_DONE:the attachment store is unavailable (retryable): local owner lease was lost; retry"
    );
    assert_eq!(current(&dir, &ns).unwrap().unwrap().version(), &won);
    assert_eq!(current(&dir, &ns).unwrap().unwrap().marker(), &winner);
}

#[test]
fn paused_holder_after_validation_excludes_a_breaker() {
    let (root, ns, dir, clock) = fixture();
    std::fs::write(root.path().join("clock"), b"1000").unwrap();
    let version = put(&dir, &ns, &marker(&ns), None, &clock).unwrap();
    let child = Child::start(root.path(), &ns, Phase::AfterValidation);
    clock.0.store(2000, Ordering::SeqCst);
    std::fs::write(root.path().join("clock"), b"2000").unwrap();
    unavailable(
        put(&dir, &ns, &marker(&ns), Some(&version), &clock).unwrap_err(),
        "local owner guard is busy; retry",
    );
    assert_eq!(child.resume(), "OWNER_DONE:OK");
    assert_ne!(current(&dir, &ns).unwrap().unwrap().version(), &version);
}

#[test]
fn process_crashes_preserve_atomic_owner_publication() {
    for phase in [
        Phase::AfterMove,
        Phase::AfterLeaseSync,
        Phase::AfterAcquire,
        Phase::BeforeTempSync,
        Phase::BeforePublish,
        Phase::AfterPublish,
        Phase::BeforeRelease,
    ] {
        let (root, ns, dir, clock) = fixture();
        std::fs::write(root.path().join("clock"), b"1000").unwrap();
        let first = marker(&ns);
        let version = put(&dir, &ns, &first, None, &clock).unwrap();
        if phase == Phase::AfterMove {
            // A breaker dies after moving a verified-looking expired candidate, before reading it.
            acquire(&dir, &marker(&ns), &clock).unwrap();
            clock.0.store(2000, Ordering::SeqCst);
            std::fs::write(root.path().join("clock"), b"2000").unwrap();
        }
        let child = Child::start(root.path(), &ns, phase);
        child.terminate();
        let found = current(&dir, &ns).unwrap().unwrap();
        if matches!(phase, Phase::AfterPublish | Phase::BeforeRelease) {
            assert_ne!(found.version(), &version);
        } else {
            assert_eq!(found.marker(), &first);
            assert_eq!(found.version(), &version);
        }
        clock.0.store(2000, Ordering::SeqCst);
        let next = marker(&ns);
        put(&dir, &ns, &next, Some(found.version()), &clock).unwrap();
        assert_eq!(current(&dir, &ns).unwrap().unwrap().marker(), &next);
    }
}

#[test]
fn crash_during_lease_creation_is_explicit_malformed_refusal() {
    let (root, ns, dir, clock) = fixture();
    std::fs::write(root.path().join("clock"), b"1000").unwrap();
    let version = put(&dir, &ns, &marker(&ns), None, &clock).unwrap();
    Child::start(root.path(), &ns, Phase::AfterLockCreate).terminate();
    clock.0.store(2000, Ordering::SeqCst);
    let error = put(&dir, &ns, &marker(&ns), Some(&version), &clock).unwrap_err();
    assert!(matches!(error, BlobError::InvalidOwnerMarker { .. }));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker is invalid: expected bounded local owner lease"
    );
    assert_eq!(current(&dir, &ns).unwrap().unwrap().version(), &version);
}

#[test]
fn unsupported_primitives_fail_closed() {
    for error in [Errno::NOSYS, Errno::NOTSUP, Errno::INVAL] {
        let error = primitive(error);
        assert!(matches!(error, BlobError::OwnerCapabilityMissing { .. }));
        assert_eq!(
            error.to_string(),
            "the attachment store lacks required owner-fence capability: local no-replace rename or advisory lock"
        );
    }
}

#[test]
fn changed_expiry_is_restored_even_with_the_same_nonce() {
    let (_root, ns, dir, clock) = fixture();
    let expired = acquire(&dir, &marker(&ns), &clock).unwrap();
    clock.0.store(2000, Ordering::SeqCst);
    let mut renewed = expired.clone();
    renewed.expires = 2030;
    let _guard = dir.guard().unwrap();
    dir.unlink(LOCK).unwrap();
    dir.create(LOCK, &renewed.encode()).unwrap();
    assert!(!break_expired(&dir, &expired, &clock).unwrap());
    assert_eq!(read_lease(&dir).unwrap(), Some(renewed));
}

#[tokio::test]
async fn probe_namespace_collision_preserves_the_existing_owner() {
    use crate::AttachmentStore;
    let root = tempfile::tempdir().unwrap();
    let store = super::super::LocalStore::open(root.path()).await.unwrap();
    let ns = Namespace::random().unwrap();
    let first = marker(&ns);
    let version = store.put_owner(&ns, &first, None).await.unwrap();
    let error = probe_namespace(&store, ns.clone()).await.unwrap_err();
    // Check the bytes before classifying the error: a refused probe must not remove this owner.
    assert_eq!(store.owner(&ns).await.unwrap().unwrap().version(), &version);
    assert!(matches!(error, BlobError::OwnerConflict));
    assert_eq!(
        error.to_string(),
        "the attachment owner marker changed or already exists"
    );
}
