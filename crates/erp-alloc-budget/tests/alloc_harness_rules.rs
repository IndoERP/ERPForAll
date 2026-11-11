//! The rules the harness and `measure` enforce on a binary that is wired right (lesson):
//! `measure` refuses a thread of the test's own and a call inside another `measure`, and a run
//! that selects several tests runs each in a process of its own.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::path::PathBuf;
use std::process::{Command, Stdio};

use erp_alloc_budget::{self as alloc, AllocError};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    measure_on_a_thread_of_the_test_is_refused,
    measure_inside_measure_is_refused,
    a_run_of_several_tests_starts_a_process_for_each,
    process_probe_first,
    process_probe_second,
);

fn measure_on_a_thread_of_the_test_is_refused() {
    let outcome = std::thread::spawn(|| alloc::measure(|| ()).map(|(s, ())| s))
        .join()
        .unwrap();
    assert_eq!(outcome, Err(AllocError::OffHarnessThread));
    assert_eq!(
        AllocError::OffHarnessThread.to_string(),
        "measure() was called on a thread the test started; call it from the test function \
         itself, which the harness runs on the main thread"
    );
    // the harness thread itself still counts
    assert!(alloc::measure(|| Box::new(1u8)).is_ok());
}

fn measure_inside_measure_is_refused() {
    let (_, inner) = alloc::measure(|| alloc::measure(|| ()).map(|(s, ())| s)).unwrap();
    assert_eq!(inner, Err(AllocError::Nested));
    assert_eq!(
        AllocError::Nested.to_string(),
        "measure() was called inside another measure(); measure the two parts one after the other"
    );
    // the refusal left nothing behind: the next measurement runs
    assert!(alloc::measure(|| ()).is_ok());
}

/// Set by [`a_run_of_several_tests_starts_a_process_for_each`] for the run it starts: where the
/// probes write their process ids.
const PROBE_DIR: &str = "ERP_ALLOC_HARNESS_PROBE_DIR";

/// Writes this process's id to `<PROBE_DIR>/<name>` when the variable is set (a probe run), and
/// does nothing otherwise (an ordinary run of the binary).
fn probe(name: &str) {
    if let Some(dir) = std::env::var_os(PROBE_DIR) {
        std::fs::write(
            PathBuf::from(dir).join(name),
            std::process::id().to_string(),
        )
        .unwrap();
    }
}

fn process_probe_first() {
    probe("first");
}

fn process_probe_second() {
    probe("second");
}

fn a_run_of_several_tests_starts_a_process_for_each() {
    let dir = std::env::temp_dir().join(format!("erp-alloc-harness-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // `process_probe` selects the two probes (not this test), as `cargo test` would select them.
    // Under `cargo test` this test runs in a child started for it alone: the run below must not
    // take that over (the guard is an argument, not inherited; the old guard variable is removed
    // in case a caller still sets it)
    let started = Command::new(std::env::current_exe().unwrap())
        .arg("process_probe")
        .env(PROBE_DIR, &dir)
        .env_remove("ERP_ALLOC_HARNESS_CHILD")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let run_pid = started.id();
    let run = started.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        stdout.contains("running 2 tests (each in its own process)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("test result: ok. 2 passed; 0 failed"),
        "{stdout}"
    );
    let pid = |name: &str| -> u32 {
        std::fs::read_to_string(dir.join(name))
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .parse()
            .unwrap()
    };
    let (first, second) = (pid("first"), pid("second"));
    std::fs::remove_dir_all(&dir).unwrap();
    // each probe ran in a process of its own, neither in the run's process nor in this one
    assert_ne!(first, second);
    for probe in [first, second] {
        assert_ne!(probe, run_pid);
        assert_ne!(probe, std::process::id());
    }
    // one selected test runs in the process itself
    let dir_one = dir.with_extension("one");
    std::fs::create_dir_all(&dir_one).unwrap();
    let one = Command::new(std::env::current_exe().unwrap())
        .args(["process_probe_first", "--exact"])
        .env(PROBE_DIR, &dir_one)
        .env_remove("ERP_ALLOC_HARNESS_CHILD")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&one.stdout);
    assert!(one.status.success(), "{stdout}");
    assert!(stdout.contains("running 1 tests (main thread)"), "{stdout}");
    std::fs::remove_dir_all(&dir_one).unwrap();
    // a child started for one test that finds several selected runs none of them
    let refused = Command::new(std::env::current_exe().unwrap())
        .args(["process_probe", "--erp-child-of=1"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("a child started by process 1 for one test found 2 selected"),
        "{stderr}"
    );
}
