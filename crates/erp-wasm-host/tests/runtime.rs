//! Runtime behaviour of the WASM host with hand-written components.

#![allow(clippy::expect_used, clippy::panic, reason = "test assertions")]

use std::time::{Duration, Instant};

use erp_wasm_host::{HostError, Limits, TrapKind, WasmHost};

fn component(core_funcs: &str, exports: &str) -> Vec<u8> {
    let text = format!(
        r#"(component
  (core module $m
    (memory (export "mem") 1)
    {core_funcs})
  (core instance $i (instantiate $m))
  {exports})"#
    );
    wat::parse_str(text).expect("valid component text")
}

fn spin() -> Vec<u8> {
    component(
        r#"(func (export "spin") (loop $l (br $l)))"#,
        r#"(func (export "spin") (canon lift (core func $i "spin")))"#,
    )
}

fn echo() -> Vec<u8> {
    component(
        r#"(func (export "echo") (param i32) (result i32) local.get 0)
           (func (export "boom") unreachable)
           (func (export "grow") (drop (memory.grow (i32.const 1000))))"#,
        r#"(func (export "echo") (param "x" u32) (result u32) (canon lift (core func $i "echo")))
           (func (export "boom") (canon lift (core func $i "boom")))
           (func (export "grow") (canon lift (core func $i "grow")))"#,
    )
}

#[tokio::test]
async fn infinite_loop_is_trapped_within_the_deadline() {
    let host = WasmHost::new().expect("host");
    let comp = host.component(&spin()).expect("compile");
    let limits = Limits::default()
        .with_cpu(Duration::from_millis(50))
        .expect("limit");
    let mut store = host.store(&comp, limits);
    let start = Instant::now();
    let err = store.call_unit("spin").await.expect_err("must trap");
    assert!(
        matches!(err, HostError::Trap(TrapKind::Epoch)),
        "got {err:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn memory_growth_past_the_cap_traps() {
    let host = WasmHost::new().expect("host");
    let comp = host.component(&echo()).expect("compile");
    // 1000 pages = 62.5 MiB > the 1 MiB cap.
    let limits = Limits::default()
        .with_memory_bytes(1024 * 1024)
        .expect("limit");
    let mut store = host.store(&comp, limits);
    let err = store.call_unit("grow").await.expect_err("must trap");
    assert!(
        matches!(err, HostError::Trap(TrapKind::Memory)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn trap_is_sticky_and_leaves_other_stores_healthy() {
    let host = WasmHost::new().expect("host");
    let comp = host.component(&echo()).expect("compile");
    let mut bad = host.store(&comp, Limits::default());
    let mut good = host.store(&comp, Limits::default());

    let err = bad.call_unit("boom").await.expect_err("must trap");
    assert!(
        matches!(err, HostError::Trap(TrapKind::Other(_))),
        "got {err:?}"
    );
    assert!(bad.is_poisoned());
    let again = bad.call_u32("echo", 1).await.expect_err("poisoned");
    assert!(matches!(again, HostError::Poisoned), "got {again:?}");

    assert_eq!(good.call_u32("echo", 41).await.expect("healthy"), 41);
}

#[tokio::test]
async fn unknown_export_is_an_error_but_not_a_trap() {
    let host = WasmHost::new().expect("host");
    let comp = host.component(&echo()).expect("compile");
    let mut store = host.store(&comp, Limits::default());
    let err = store
        .call_unit("missing")
        .await
        .expect_err("no such export");
    assert!(
        matches!(&err, HostError::Export { name, .. } if name == "missing"),
        "got {err:?}"
    );
    assert!(!store.is_poisoned());
}

#[test]
fn cache_compiles_once_per_key() {
    let host = WasmHost::new().expect("host");
    let bytes = echo();
    host.component(&bytes).expect("first");
    host.component(&bytes).expect("second");
    assert_eq!(host.cache().compilations(), 1);

    // Another contract version of the same bytes is another key and compiles again.
    let other = host.key_for_version(&bytes, "2.0.0");
    host.component_with_key(&other, &bytes)
        .expect("other version");
    assert_eq!(host.cache().compilations(), 2);
}

#[test]
fn cache_rejects_bytes_that_do_not_match_the_key() {
    let host = WasmHost::new().expect("host");
    let a = echo();
    let key = host.key_for(&a);
    let err = host
        .component_with_key(&key, &spin())
        .expect_err("mismatch");
    assert!(matches!(err, HostError::DigestMismatch), "got {err:?}");
}

#[test]
fn invalid_component_bytes_fail_to_compile() {
    let host = WasmHost::new().expect("host");
    let err = host.component(b"not wasm").expect_err("garbage");
    assert!(matches!(err, HostError::Compile(_)), "got {err:?}");
}

#[test]
fn limits_reject_zero_and_values_above_the_maximum() {
    let zero = Limits::default()
        .with_cpu(Duration::ZERO)
        .expect_err("zero");
    assert_eq!(
        zero.to_string(),
        "invalid cpu time limit 0: must be between 1 and 2000"
    );
    let big = Limits::default()
        .with_memory_bytes(Limits::MAX_MEMORY_BYTES + 1)
        .expect_err("above max");
    assert!(big.to_string().starts_with("invalid memory limit"), "{big}");
    // A Duration whose millisecond count wraps u64 must not slip through.
    let wrapped = Duration::from_millis(u64::MAX) + Duration::from_secs(1);
    assert!(Limits::default().with_cpu(wrapped).is_err());
    assert!(
        Limits::default()
            .with_cpu(Limits::MAX_CPU + Duration::from_micros(900))
            .is_err()
    );
    assert!(Limits::default().with_batch_ids(0).is_err());
    assert!(Limits::default().with_stack_depth(65).is_err());
}

#[test]
fn payload_and_batch_limits_are_enforced() {
    let limits = Limits::default()
        .with_payload_bytes(10)
        .expect("limit")
        .with_batch_ids(3)
        .expect("limit");
    assert!(limits.check_payload(10).is_ok());
    let err = limits.check_payload(11).expect_err("too big");
    assert_eq!(err.to_string(), "payload bytes limit exceeded: 11 > 10");
    let ids: Vec<u32> = (0..7).collect();
    let sizes: Vec<usize> = limits.chunks(&ids).map(<[u32]>::len).collect();
    assert_eq!(sizes, [3, 3, 1]);
}

#[test]
fn stack_depth_is_bounded() {
    let host = WasmHost::new().expect("host");
    let comp = host.component(&echo()).expect("compile");
    let mut store = host.store(&comp, Limits::default().with_stack_depth(2).expect("limit"));
    store.enter_call().expect("1");
    store.enter_call().expect("2");
    let err = store.enter_call().expect_err("3rd");
    assert_eq!(err.to_string(), "stack depth limit exceeded: 3 > 2");
    store.exit_call();
    store.enter_call().expect("slot freed");
}

#[tokio::test]
async fn memory_cap_is_summed_across_memories() {
    // Two memories of 40 pages (2.5 MiB each) fit separately under 4 MiB, not together.
    let bytes = wat::parse_str(
        r#"(component
  (core module $m
    (memory 40) (memory 40)
    (func (export "noop")))
  (core instance $i (instantiate $m))
  (func (export "noop") (canon lift (core func $i "noop"))))"#,
    )
    .expect("component text");
    let host = WasmHost::new().expect("host");
    let comp = host.component(&bytes).expect("compile");
    let limits = Limits::default()
        .with_memory_bytes(4 * 1024 * 1024)
        .expect("limit");
    let mut store = host.store(&comp, limits);
    let err = store
        .call_unit("noop")
        .await
        .expect_err("second memory is over the cap");
    assert!(
        matches!(err, HostError::Trap(TrapKind::Memory)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn deadline_survives_dropping_the_host() {
    let host = WasmHost::new().expect("host");
    let comp = host.component(&spin()).expect("compile");
    let limits = Limits::default()
        .with_cpu(Duration::from_millis(50))
        .expect("limit");
    let mut store = host.store(&comp, limits);
    drop(host);
    let err = store.call_unit("spin").await.expect_err("must trap");
    assert!(
        matches!(err, HostError::Trap(TrapKind::Epoch)),
        "got {err:?}"
    );
}

#[test]
fn a_component_importing_a_denied_interface_is_refused_at_load() {
    let host = WasmHost::new().expect("host");
    for denied in [
        "wasi:filesystem/types@0.2.6",
        "wasi:sockets/tcp@0.2.6",
        "wasi:http/outgoing-handler@0.2.6",
        "somebody:else/api@1.0.0",
    ] {
        let bytes = wat::parse_str(format!(r#"(component (import "{denied}" (instance)))"#))
            .expect("component text");
        let err = host.component(&bytes).expect_err(denied);
        assert!(
            matches!(&err, HostError::ForbiddenImport { name } if name == denied),
            "{denied}: {err:?}"
        );
    }
}

#[test]
fn allowed_imports_load_and_the_allowlist_matches_by_interface_name() {
    let host = WasmHost::new().expect("host");
    let bytes = wat::parse_str(
        r#"(component
             (import "wasi:cli/exit@0.2.6" (instance))
             (import "wasi:io/error@0.2.6" (instance))
             (import "wasi:clocks/wall-clock@0.2.6" (instance)))"#,
    )
    .expect("component text");
    host.component(&bytes).expect("allowed imports load");
    for ok in [
        "wasi:cli/stderr@0.2.6",
        "wasi:random/random",
        "erpforall:addon/orm-host@1.0.0",
    ] {
        assert!(erp_wasm_host::import_allowed(ok), "{ok}");
    }
    for bad in [
        "wasi:filesystem/preopens@0.2.6",
        "wasi:sockets/network@0.2.6",
        "wasi:cli/environment-x@0.2.6",
        "wasi:cli",
        "",
    ] {
        assert!(!erp_wasm_host::import_allowed(bad), "{bad}");
    }
}

/// A component that exports `addon-guest` with a `describe` returning `text`, and imports one host
/// function it never calls (the shape of an addon component: it imports `orm-host` and WASI).
fn describing(text: &str) -> Vec<u8> {
    let len = text.len();
    let text = text.replace('"', "\\\"");
    let source = format!(
        r#"(component
  (import "erpforall:addon/unused-host-fn" (func))
  (core module $m
    (memory (export "mem") 1)
    (data (i32.const 8) "{text}")
    (func (export "describe") (result i32)
      (i32.store (i32.const 0) (i32.const 8))
      (i32.store (i32.const 4) (i32.const {len}))
      i32.const 0)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 0))
  (core instance $i (instantiate $m))
  (func $d (result string)
    (canon lift (core func $i "describe") (memory (core memory $i "mem")) (realloc (core func $i "realloc"))))
  (instance $g (export "describe" (func $d)))
  (export "erpforall:addon/addon-guest@1.0.0" (instance $g)))"#
    );
    wat::parse_str(source).expect("valid component text")
}

#[tokio::test]
async fn the_host_reads_describe_through_an_inspection_store() {
    let host = WasmHost::new().expect("host");
    let comp = host
        .component(&describing("{\"schema\":1}"))
        .expect("compile");
    // a plain store has no answer for the import the component declares
    let mut plain = host.store(&comp, Limits::default());
    let err = plain.call_describe().await.expect_err("unsatisfied import");
    assert!(matches!(err, HostError::Instantiate(_)), "got {err:?}");
    let mut store = host
        .inspection_store(&comp, Limits::default())
        .expect("inspection store");
    assert_eq!(
        store.call_describe().await.expect("describe"),
        "{\"schema\":1}"
    );
    assert!(!store.is_poisoned());
}

#[tokio::test]
async fn a_component_without_addon_guest_is_an_export_error() {
    let host = WasmHost::new().expect("host");
    let comp = host.component(&echo()).expect("compile");
    let mut store = host
        .inspection_store(&comp, Limits::default())
        .expect("inspection store");
    let err = store.call_describe().await.expect_err("no export");
    assert!(
        matches!(&err, HostError::Export { name, reason } if name == "describe" && reason.contains("addon-guest")),
        "got {err:?}"
    );
}

/// A component importing `wasi:clocks/wall-clock@<version>` whose exports (each `func(u32) -> u32`,
/// the argument ignored) return the low 32 bits of `now`'s seconds (`secs`), the nanoseconds of
/// `now` (`nanos`) and of `resolution` (`res_nanos`).
fn wall_clock_reader(version: &str) -> Vec<u8> {
    let text = format!(
        r#"(component
  (import "wasi:clocks/wall-clock@{version}" (instance $c
    (type $d (record (field "seconds" u64) (field "nanoseconds" u32)))
    (export "datetime" (type $dt (eq $d)))
    (type $f (func (result $dt)))
    (export "now" (func (type $f)))
    (export "resolution" (func (type $f)))))
  (core module $mem (memory (export "mem") 1))
  (core instance $memi (instantiate $mem))
  (alias core export $memi "mem" (core memory $mm))
  (core func $now (canon lower (func $c "now") (memory $mm)))
  (core func $res (canon lower (func $c "resolution") (memory $mm)))
  (core instance $ci (export "now" (func $now)) (export "resolution" (func $res)))
  (core module $m
    (import "c" "now" (func $now (param i32)))
    (import "c" "resolution" (func $res (param i32)))
    (import "mem" "mem" (memory 1))
    (func (export "secs") (param i32) (result i32)
      (call $now (i32.const 0)) (i32.wrap_i64 (i64.load (i32.const 0))))
    (func (export "nanos") (param i32) (result i32)
      (call $now (i32.const 0)) (i32.load (i32.const 8)))
    (func (export "res_nanos") (param i32) (result i32)
      (call $res (i32.const 0)) (i32.load (i32.const 8))))
  (core instance $i (instantiate $m (with "c" (instance $ci)) (with "mem" (instance $memi))))
  (func (export "secs") (param "x" u32) (result u32) (canon lift (core func $i "secs")))
  (func (export "nanos") (param "x" u32) (result u32) (canon lift (core func $i "nanos")))
  (func (export "res-nanos") (param "x" u32) (result u32) (canon lift (core func $i "res_nanos"))))"#
    );
    wat::parse_str(text).expect("valid component text")
}

/// A component importing `wasi:clocks/monotonic-clock@0.2.6` `now` and exporting `tick`, which calls it.
fn monotonic_reader() -> Vec<u8> {
    wat::parse_str(
        r#"(component
  (import "wasi:clocks/monotonic-clock@0.2.6" (instance $c
    (type $f (func (result u64)))
    (export "now" (func (type $f)))))
  (core func $now (canon lower (func $c "now")))
  (core instance $ci (export "now" (func $now)))
  (core module $m
    (import "c" "now" (func $now (result i64)))
    (func (export "tick") (drop (call $now))))
  (core instance $i (instantiate $m (with "c" (instance $ci))))
  (func (export "tick") (canon lift (core func $i "tick"))))"#,
    )
    .expect("valid component text")
}

fn unix_seconds() -> u32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs();
    // the guest returns the low 32 bits
    (secs & u64::from(u32::MAX)) as u32
}

#[tokio::test]
async fn the_wall_clock_answers_with_the_system_time() {
    let host = WasmHost::new().expect("host");
    // Any 0.2.x import is served, whatever version the guest toolchain was built against.
    for version in ["0.2.0", "0.2.3", "0.2.6"] {
        let comp = host
            .component(&wall_clock_reader(version))
            .expect("compile");
        let mut store = host.store(&comp, Limits::default());
        let before = unix_seconds();
        let secs = store.call_u32("secs", 0).await.expect("now");
        let after = unix_seconds();
        assert!(
            (before..=after).contains(&secs),
            "{version}: {secs} not in {before}..={after}"
        );
        // reported in whole microseconds
        let nanos = store.call_u32("nanos", 0).await.expect("now nanos");
        assert!(nanos < 1_000_000_000 && nanos % 1_000 == 0, "{nanos}");
        assert_eq!(
            store.call_u32("res-nanos", 0).await.expect("resolution"),
            1_000
        );
    }
}

#[tokio::test]
async fn the_monotonic_clock_still_traps() {
    let host = WasmHost::new().expect("host");
    let comp = host.component(&monotonic_reader()).expect("compile");
    // The inspection store answers each import it does not define with a trap.
    let mut store = host
        .inspection_store(&comp, Limits::default())
        .expect("store");
    let err = store.call_unit("tick").await.expect_err("must trap");
    assert!(
        matches!(err, HostError::Trap(TrapKind::Other(_))),
        "got {err:?}"
    );
    assert!(store.is_poisoned());
}

/// The wall clock survives `define_unknown_imports_as_traps`: the inspection store serves it too.
#[tokio::test]
async fn the_inspection_store_serves_the_wall_clock_not_a_trap() {
    let host = WasmHost::new().expect("host");
    let comp = host
        .component(&wall_clock_reader("0.2.6"))
        .expect("compile");
    let mut store = host
        .inspection_store(&comp, Limits::default())
        .expect("store");
    assert_eq!(
        store.call_u32("res-nanos", 0).await.expect("resolution"),
        1_000
    );
}
