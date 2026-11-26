// SPDX-License-Identifier: MIT OR Apache-2.0
//! The compiler host on WAT fixture modules: admission, containment, limits and typed errors.
//! The shipped compiler artefact is covered by `wasm_artifact.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_ui_compiler::BuildMode;
use erp_ui_compiler_cli::{ArchCompileRequest, Operation};
use erp_ui_compiler_host::{
    ArtifactDigest, ArtifactError, CancelToken, CompilerHostError, ExecutionLimits,
    HostExecutionError, HostResponse, InvalidLimit, LimitError, MAX_DEADLINE, MAX_FUEL,
    MAX_OUTPUT_BYTES, MAX_STDERR_IN_ERROR, WasmArtifact, WasmCompilerHost,
};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_of(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::parse(&hex(&Sha256::digest(bytes))).unwrap()
}

fn admit(wat: &str) -> Result<WasmArtifact, ArtifactError> {
    let wasm = wat::parse_str(wat).unwrap();
    let digest = digest_of(&wasm);
    WasmArtifact::admit(wasm, &digest)
}

/// An arch request; `schema_bytes` pads the host schema, which raises the request's fuel budget
/// (40 000 units per request byte) far above what a lowered deadline allows.
fn request(schema_bytes: usize) -> ArchCompileRequest {
    let source = "<kanban><templates><t t-name=\"card\"><div/></t></templates></kanban>";
    let schema = if schema_bytes == 0 {
        "{}".to_owned()
    } else {
        "s".repeat(schema_bytes)
    };
    ArchCompileRequest::new(
        Operation::Compile,
        BuildMode::Prod,
        "res.partner.kanban",
        hex(&Sha256::digest(source.as_bytes())),
        "en_US",
        schema,
        "1",
        source,
    )
    .unwrap()
}

fn run(wat: &str, limits: ExecutionLimits) -> Result<HostResponse, CompilerHostError> {
    let host = WasmCompilerHost::new(&admit(wat).unwrap(), limits);
    host.compile_arch(&request(0), &CancelToken::new())
}

/// A guest that loops forever without calling the host.
const SPIN: &str = r#"
(module
    (memory (export "memory") 1)
    (func (export "_start") (loop br 0)))
"#;

/// Writes the 20 bytes at 0 to stdout once per call of `$write20`.
const WRITER_PRELUDE: &str = r#"
    (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
    (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
    (memory (export "memory") 1)
    (data (i32.const 0) "12345678901234567890")
    (data (i32.const 100) "\00\00\00\00\14\00\00\00")
"#;

#[test]
fn the_digest_is_checked_before_the_bytes_are_parsed() {
    let garbage = b"definitely not wasm".to_vec();
    let wrong = ArtifactDigest::parse(&"0".repeat(64)).unwrap();
    let err = WasmArtifact::admit(garbage.clone(), &wrong).unwrap_err();
    let actual = hex(&Sha256::digest(&garbage));
    assert_eq!(
        err,
        ArtifactError::DigestMismatch {
            expected: "0".repeat(64),
            actual: actual.clone(),
        }
    );
    assert_eq!(
        err.to_string(),
        format!(
            "artifact digest mismatch: expected sha256 {}, got {actual}",
            "0".repeat(64)
        )
    );
    // With the right digest the same bytes reach the parser, which refuses them.
    let err = WasmArtifact::admit(garbage.clone(), &digest_of(&garbage)).unwrap_err();
    let ArtifactError::InvalidModule(message) = &err else {
        panic!("expected InvalidModule, got {err:?}");
    };
    assert!(!message.is_empty());
    assert_eq!(err.to_string(), format!("invalid WASM artifact: {message}"));
    // An admitted artifact keeps the pinned digest.
    let wasm = wat::parse_str(SPIN).unwrap();
    let digest = digest_of(&wasm);
    assert_eq!(
        WasmArtifact::admit(wasm, &digest).unwrap().digest(),
        &digest
    );
}

#[test]
fn pinned_digests_are_validated() {
    for bad in ["", "abc", &"A".repeat(64), &"0".repeat(63), &"g".repeat(64)] {
        let err = ArtifactDigest::parse(bad).unwrap_err();
        assert_eq!(
            err,
            ArtifactError::InvalidDigest {
                value: bad.to_owned()
            }
        );
        assert_eq!(
            err.to_string(),
            format!(
                "artifact digest must be 64 lowercase hexadecimal characters (SHA-256), got '{bad}'"
            )
        );
    }
    assert_eq!(
        ArtifactDigest::parse(&"a1".repeat(32)).unwrap().as_str(),
        "a1".repeat(32)
    );
}

#[test]
fn forbidden_imports_are_refused() {
    let cases = [
        (
            r#"(import "wasi_snapshot_preview1" "path_open" (func (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))"#,
            "wasi_snapshot_preview1",
            "path_open",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "sock_accept" (func (param i32 i32 i32) (result i32)))"#,
            "wasi_snapshot_preview1",
            "sock_accept",
        ),
        (
            r#"(import "wasi_snapshot_preview1" "clock_time_get" (func (param i32 i64 i32) (result i32)))"#,
            "wasi_snapshot_preview1",
            "clock_time_get",
        ),
        (
            r#"(import "env" "anything" (func (param i32) (result i32)))"#,
            "env",
            "anything",
        ),
        // An allowed name, but not a function.
        (
            r#"(import "wasi_snapshot_preview1" "fd_write" (global i32))"#,
            "wasi_snapshot_preview1",
            "fd_write",
        ),
    ];
    for (import, module, name) in cases {
        let wat =
            format!(r#"(module {import} (memory (export "memory") 1) (func (export "_start")))"#);
        let err = admit(&wat).unwrap_err();
        assert_eq!(
            err,
            ArtifactError::ForbiddenImport {
                module: module.to_owned(),
                name: name.to_owned(),
            }
        );
        assert_eq!(
            err.to_string(),
            format!(
                "forbidden WASM import '{module}.{name}': only functions fd_write, fd_read, proc_exit, args_sizes_get, args_get, environ_sizes_get, environ_get, random_get of 'wasi_snapshot_preview1' are allowed"
            )
        );
    }
}

#[test]
fn required_exports_are_checked() {
    let no_start = r#"(module (memory (export "memory") 1))"#;
    let err = admit(no_start).unwrap_err();
    assert_eq!(
        err,
        ArtifactError::MissingExport {
            name: "_start",
            kind: "function () -> ()",
        }
    );
    assert_eq!(
        err.to_string(),
        "missing required WASM export '_start' (a function () -> ())"
    );
    let wrong_start =
        r#"(module (memory (export "memory") 1) (func (export "_start") (param i32)))"#;
    assert_eq!(
        admit(wrong_start).unwrap_err(),
        ArtifactError::MissingExport {
            name: "_start",
            kind: "function () -> ()",
        }
    );
    let no_memory = r#"(module (func (export "_start")))"#;
    let err = admit(no_memory).unwrap_err();
    assert_eq!(
        err,
        ArtifactError::MissingExport {
            name: "memory",
            kind: "memory",
        }
    );
    assert_eq!(
        err.to_string(),
        "missing required WASM export 'memory' (a memory)"
    );
}

#[test]
fn a_start_section_is_refused() {
    let wat =
        r#"(module (memory (export "memory") 1) (func $s) (start $s) (func (export "_start")))"#;
    let err = admit(wat).unwrap_err();
    let ArtifactError::InvalidModule(message) = &err else {
        panic!("expected InvalidModule, got {err:?}");
    };
    assert!(message.contains("start"), "{message}");
    assert_eq!(err.to_string(), format!("invalid WASM artifact: {message}"));
}

#[test]
fn fuel_exhaustion_is_typed() {
    let limits = ExecutionLimits::default().lowered_fuel(10_000).unwrap();
    let err = run(SPIN, limits).unwrap_err();
    assert_eq!(
        err,
        CompilerHostError::Limit(LimitError::FuelExhausted { budget: 10_000 })
    );
    assert_eq!(
        err.to_string(),
        "limit error: WASM fuel exhausted (budget: 10000 units)"
    );
}

#[test]
fn a_denied_memory_growth_is_sticky() {
    // The guest ignores the -1 from memory.grow and exits cleanly: the run is still a limit.
    let wat = r#"
    (module
        (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
        (memory (export "memory") 1)
        (func (export "_start")
            (drop (memory.grow (i32.const 10)))
            (call $exit (i32.const 0))))
    "#;
    let limits = ExecutionLimits::default()
        .lowered_memory_bytes(65_536)
        .unwrap();
    let err = run(wat, limits).unwrap_err();
    assert_eq!(
        err,
        CompilerHostError::Limit(LimitError::MemoryExhausted { cap_bytes: 65_536 })
    );
    assert_eq!(
        err.to_string(),
        "limit error: WASM linear memory cap exceeded (cap: 65536 bytes)"
    );
    // An initial memory over the cap fails at instantiation with the same limit, not as an
    // instantiation error.
    let big = r#"(module (memory (export "memory") 2) (func (export "_start")))"#;
    assert_eq!(
        run(big, limits).unwrap_err(),
        CompilerHostError::Limit(LimitError::MemoryExhausted { cap_bytes: 65_536 })
    );
}

#[test]
fn the_output_cap_stops_the_guest() {
    // One 20-byte write over a 10-byte cap, then a loop: if the write did not stop the guest,
    // the loop would run until the 30 s deadline.
    let wat = format!(
        r#"(module {WRITER_PRELUDE}
            (func (export "_start")
                (drop (call $fd_write (i32.const 1) (i32.const 100) (i32.const 1) (i32.const 200)))
                (loop br 0)))"#
    );
    let limits = ExecutionLimits::default().lowered_output_bytes(10).unwrap();
    let host = WasmCompilerHost::new(&admit(&wat).unwrap(), limits);
    let started = Instant::now();
    let err = host
        .compile_arch(&request(1024 * 1024), &CancelToken::new())
        .unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(
        err,
        CompilerHostError::Limit(LimitError::OutputExhausted { max_bytes: 10 })
    );
    assert_eq!(
        err.to_string(),
        "limit error: WASM output limit exceeded (maximum: 10 bytes)"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the guest kept running after the cap: {elapsed:?}"
    );
}

#[test]
fn traps_are_typed() {
    let wat = r#"(module (memory (export "memory") 1) (func (export "_start") unreachable))"#;
    let err = run(wat, ExecutionLimits::default()).unwrap_err();
    assert_eq!(
        err,
        CompilerHostError::HostExecution(HostExecutionError::Trap(
            "wasm `unreachable` instruction executed".to_owned()
        ))
    );
    assert_eq!(
        err.to_string(),
        "host execution error: WASM execution trapped: wasm `unreachable` instruction executed"
    );
}

#[test]
fn a_failed_instantiation_is_not_a_trap() {
    // An allowed import name with a signature the host does not provide.
    let wat = r#"
    (module
        (import "wasi_snapshot_preview1" "fd_write" (func (param i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "_start")))
    "#;
    let err = run(wat, ExecutionLimits::default()).unwrap_err();
    let CompilerHostError::HostExecution(HostExecutionError::Instantiate(message)) = &err else {
        panic!("expected Instantiate, got {err:?}");
    };
    assert!(
        message.starts_with("imported function (wasi_snapshot_preview1,fd_write) type mismatch."),
        "{message}"
    );
    assert_eq!(
        err.to_string(),
        format!("host execution error: WASM instantiation failed: {message}")
    );
}

#[test]
fn cancellation_stops_a_loop_that_makes_no_host_calls() {
    let host = WasmCompilerHost::new(&admit(SPIN).unwrap(), ExecutionLimits::default());
    // About 40 billion fuel units: without fuel slicing this would not end for minutes.
    let request = request(1024 * 1024);
    let cancel = CancelToken::new();
    let started = Instant::now();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            cancel.cancel();
        })
    };
    let err = host.compile_arch(&request, &cancel).unwrap_err();
    let elapsed = started.elapsed();
    canceller.join().unwrap();
    assert_eq!(
        err,
        CompilerHostError::HostExecution(HostExecutionError::Cancelled)
    );
    assert_eq!(
        err.to_string(),
        "host execution error: WASM execution was cancelled"
    );
    assert!(
        elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(10),
        "{elapsed:?}"
    );
    // A token cancelled before the run stops it before any guest code runs.
    let err = host.compile_arch(&request, &cancel).unwrap_err();
    assert_eq!(
        err,
        CompilerHostError::HostExecution(HostExecutionError::Cancelled)
    );
}

#[test]
fn the_deadline_stops_a_loop_that_makes_no_host_calls() {
    let deadline = Duration::from_millis(200);
    let limits = ExecutionLimits::default()
        .lowered_deadline(deadline)
        .unwrap();
    let host = WasmCompilerHost::new(&admit(SPIN).unwrap(), limits);
    let started = Instant::now();
    let err = host
        .compile_arch(&request(1024 * 1024), &CancelToken::new())
        .unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(
        err,
        CompilerHostError::Limit(LimitError::DeadlineExceeded { deadline })
    );
    assert_eq!(
        err.to_string(),
        "limit error: WASM execution deadline exceeded (deadline: 200ms)"
    );
    assert!(
        elapsed >= deadline && elapsed < Duration::from_secs(10),
        "{elapsed:?}"
    );
}

#[test]
fn repeated_runs_give_identical_bytes_and_fuel() {
    // Echoes the first four request bytes and four random bytes (the deterministic random_get).
    let wat = r#"
    (module
        (import "wasi_snapshot_preview1" "fd_read" (func $read (param i32 i32 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_write" (func $write (param i32 i32 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "random_get" (func $random (param i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
        (memory (export "memory") 1)
        (func (export "_start")
            (i32.store (i32.const 0) (i32.const 100))
            (i32.store (i32.const 4) (i32.const 4))
            (drop (call $read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 50)))
            (drop (call $random (i32.const 104) (i32.const 4)))
            (i32.store (i32.const 10) (i32.const 100))
            (i32.store (i32.const 14) (i32.const 8))
            (drop (call $write (i32.const 1) (i32.const 10) (i32.const 1) (i32.const 60)))
            (call $exit (i32.const 0))))
    "#;
    let host = WasmCompilerHost::new(&admit(wat).unwrap(), ExecutionLimits::default());
    let runs: Vec<HostResponse> = (0..3)
        .map(|_| host.compile_arch(&request(0), &CancelToken::new()).unwrap())
        .collect();
    assert_eq!(&runs[0].bytes()[..4], b"ERPU");
    assert_eq!(runs[0].bytes().len(), 8);
    assert!(runs[0].is_clean());
    assert_eq!(runs[0].exit_status(), 0);
    assert!(runs[0].fuel_used() > 0);
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[1], runs[2]);
}

#[test]
fn exit_statuses_map_to_results() {
    let exit_with = |status: i32, fd: i32| {
        format!(
            r#"(module {WRITER_PRELUDE}
                (func (export "_start")
                    (drop (call $fd_write (i32.const {fd}) (i32.const 100) (i32.const 1) (i32.const 200)))
                    (call $exit (i32.const {status}))))"#
        )
    };
    // 1: compiler errors; stdout is the result.
    let response = run(&exit_with(1, 1), ExecutionLimits::default()).unwrap();
    assert!(!response.is_clean());
    assert_eq!(response.exit_status(), 1);
    assert_eq!(response.bytes(), b"12345678901234567890");
    // 2 (a refused request) and anything else: an error with the guest's stderr.
    for status in [2, 3, -1] {
        let err = run(&exit_with(status, 2), ExecutionLimits::default()).unwrap_err();
        assert_eq!(
            err,
            CompilerHostError::HostExecution(HostExecutionError::ExitStatus {
                status,
                stderr: "12345678901234567890".to_owned(),
            })
        );
        assert_eq!(
            err.to_string(),
            format!(
                "host execution error: compiler exited with status {status}: 12345678901234567890"
            )
        );
    }
    // Returning from _start is a clean exit.
    let wat = format!(
        r#"(module {WRITER_PRELUDE}
            (func (export "_start")
                (drop (call $fd_write (i32.const 1) (i32.const 100) (i32.const 1) (i32.const 200)))))"#
    );
    let response = run(&wat, ExecutionLimits::default()).unwrap();
    assert!(response.is_clean());
    assert_eq!(response.bytes(), b"12345678901234567890");
}

#[test]
fn stderr_in_errors_is_bounded() {
    // 10 000 bytes of stderr (500 writes of 20 bytes), then exit 2.
    let wat = format!(
        r#"(module {WRITER_PRELUDE}
            (func (export "_start") (local $i i32)
                (loop $l
                    (drop (call $fd_write (i32.const 2) (i32.const 100) (i32.const 1) (i32.const 200)))
                    (local.set $i (i32.add (local.get $i) (i32.const 1)))
                    (br_if $l (i32.lt_u (local.get $i) (i32.const 500))))
                (call $exit (i32.const 2))))"#
    );
    let err = run(&wat, ExecutionLimits::default()).unwrap_err();
    let CompilerHostError::HostExecution(HostExecutionError::ExitStatus { status, stderr }) = err
    else {
        panic!("expected ExitStatus, got {err:?}");
    };
    assert_eq!(status, 2);
    let expected_head: String = "12345678901234567890"
        .repeat(500)
        .chars()
        .take(MAX_STDERR_IN_ERROR)
        .collect();
    assert_eq!(stderr, format!("{expected_head} [truncated]"));
}

#[test]
fn guest_pointers_out_of_range_get_efault() {
    // fd_write with the iovec array at -8 (0xFFFFFFF8 as u32: past the 64 KiB memory), and with
    // an iovec whose buffer runs past the end; the guest exits with each errno in turn.
    let wat = r#"
    (module
        (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "random_get" (func $random (param i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
        (memory (export "memory") 1)
        (data (i32.const 100) "\f0\ff\00\00\20\00\00\00")
        (func (export "_start") (local $e i32)
            (local.set $e (call $fd_write (i32.const 1) (i32.const -8) (i32.const 1) (i32.const 200)))
            (if (i32.ne (local.get $e) (i32.const 21)) (then (call $exit (i32.const 100))))
            (local.set $e (call $fd_write (i32.const 1) (i32.const 100) (i32.const 1) (i32.const 200)))
            (if (i32.ne (local.get $e) (i32.const 21)) (then (call $exit (i32.const 101))))
            (local.set $e (call $random (i32.const -1) (i32.const 4)))
            (if (i32.ne (local.get $e) (i32.const 21)) (then (call $exit (i32.const 102))))
            (local.set $e (call $fd_write (i32.const 7) (i32.const 100) (i32.const 1) (i32.const 200)))
            (if (i32.ne (local.get $e) (i32.const 8)) (then (call $exit (i32.const 103))))
            (local.set $e (call $fd_write (i32.const 1) (i32.const 100) (i32.const 100000) (i32.const 200)))
            (if (i32.ne (local.get $e) (i32.const 28)) (then (call $exit (i32.const 104))))
            (call $exit (i32.const 0))))
    "#;
    let response = run(wat, ExecutionLimits::default()).unwrap();
    assert!(response.is_clean());
    assert!(response.bytes().is_empty());
}

#[test]
fn limits_can_only_be_lowered() {
    let defaults = ExecutionLimits::default();
    assert_eq!(defaults.memory_bytes(), 1024 * 1024 * 1024);
    assert_eq!(defaults.output_bytes(), MAX_OUTPUT_BYTES);
    assert_eq!(defaults.deadline(), MAX_DEADLINE);
    assert_eq!(
        defaults.fuel_for(1000),
        erp_ui_compiler_cli::wasm_fuel_budget(1000)
    );

    let err = defaults.lowered_memory_bytes(0).unwrap_err();
    assert_eq!(
        err,
        InvalidLimit::Memory {
            value: 0,
            max: 1024 * 1024 * 1024
        }
    );
    assert_eq!(
        err.to_string(),
        "memory limit cannot be set to 0 bytes: it must be 1..=1073741824"
    );
    assert_eq!(
        defaults
            .lowered_memory_bytes(1024 * 1024 * 1024 + 1)
            .unwrap_err(),
        InvalidLimit::Memory {
            value: 1024 * 1024 * 1024 + 1,
            max: 1024 * 1024 * 1024
        }
    );
    let err = defaults.lowered_fuel(0).unwrap_err();
    assert_eq!(
        err,
        InvalidLimit::Fuel {
            value: 0,
            max: MAX_FUEL
        }
    );
    assert_eq!(
        err.to_string(),
        format!("fuel limit cannot be set to 0 units: it must be 1..={MAX_FUEL}")
    );
    let err = defaults.lowered_output_bytes(0).unwrap_err();
    assert_eq!(
        err,
        InvalidLimit::Output {
            value: 0,
            max: MAX_OUTPUT_BYTES
        }
    );
    assert_eq!(
        err.to_string(),
        "output limit cannot be set to 0 bytes: it must be 1..=1048576"
    );
    assert_eq!(
        defaults
            .lowered_output_bytes(MAX_OUTPUT_BYTES + 1)
            .unwrap_err(),
        InvalidLimit::Output {
            value: MAX_OUTPUT_BYTES + 1,
            max: MAX_OUTPUT_BYTES
        }
    );
    let err = defaults.lowered_deadline(Duration::ZERO).unwrap_err();
    assert_eq!(
        err,
        InvalidLimit::Deadline {
            value: Duration::ZERO,
            max: MAX_DEADLINE
        }
    );
    assert_eq!(
        err.to_string(),
        "deadline cannot be set to 0ns: it must be above zero and at most 30s"
    );
    assert!(
        defaults
            .lowered_deadline(MAX_DEADLINE + Duration::from_nanos(1))
            .is_err()
    );

    // Lowered once, a limit cannot be raised again, not even back to its ceiling.
    let lowered = defaults
        .lowered_fuel(1000)
        .unwrap()
        .lowered_deadline(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        lowered.lowered_fuel(1001).unwrap_err(),
        InvalidLimit::Fuel {
            value: 1001,
            max: 1000
        }
    );
    assert_eq!(
        lowered.lowered_deadline(MAX_DEADLINE).unwrap_err(),
        InvalidLimit::Deadline {
            value: MAX_DEADLINE,
            max: Duration::from_secs(1)
        }
    );
    // A lowered fuel cap only lowers the per-request budget.
    assert_eq!(lowered.fuel_for(1000), 1000);
    let small = defaults.lowered_fuel(MAX_FUEL - 1).unwrap();
    assert_eq!(
        small.fuel_for(10),
        erp_ui_compiler_cli::wasm_fuel_budget(10)
    );
}

/// The host and its artifact can be shared across threads (the host rustdoc says so).
#[test]
fn the_host_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<WasmCompilerHost>();
    assert_send_sync::<WasmArtifact>();
    assert_send_sync::<CancelToken>();
}
