//! A node-limit refusal of `erp-html` is a typed failure when the sanitiser runs as a
//! `wasm32-wasip2` component (/D3).
//!
//! The guest is `crates/erp-html/probes/wasip2_typed_failure.rs`, a command component. The host
//! side is a plain wasmtime component linker with the same rule as `WasmHost`: every import the
//! component declares is answered by a function that traps when called, so the guest has no
//! ambient authority. The test calls the `wasi:cli/run` export and requires `Ok`.
//! `scripts/html-wasip2-typed-failure.sh` (CI step `html-wasip2-typed-failure`) builds the
//! component and runs this test; without the artefact the test fails, never skips.

#![allow(clippy::expect_used, clippy::panic, reason = "test assertions")]

use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, Engine, Store};

const ARTEFACT: &str = "ERP_HTML_WASIP2_TYPED_FAILURE_WASM";

#[test]
#[ignore = "needs the wasm32-wasip2 build of the guest: scripts/html-wasip2-typed-failure.sh"]
fn node_limit_refusal_is_a_typed_failure_in_a_wasip2_component() {
    let path = std::env::var(ARTEFACT)
        .unwrap_or_else(|_| panic!("{ARTEFACT} must name the built component"));
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("engine");
    let component = Component::from_file(&engine, &path).expect("the component loads");
    let mut linker: Linker<()> = Linker::new(&engine);
    linker
        .define_unknown_imports_as_traps(&component)
        .expect("imports are answered by traps");
    let mut store = Store::new(&engine, ());
    let instance = linker
        .instantiate(&mut store, &component)
        .expect("the component instantiates");
    let run_interface = component
        .component_type()
        .exports(&engine)
        .map(|(name, _)| name.to_owned())
        .find(|name| name.starts_with("wasi:cli/run@"))
        .expect("the component exports wasi:cli/run");
    let (_, interface) = instance
        .get_export(&mut store, None, &run_interface)
        .expect("the run interface is exported");
    let (_, run) = instance
        .get_export(&mut store, Some(&interface), "run")
        .expect("run is exported");
    let run = instance
        .get_func(&mut store, run)
        .expect("run is a function");
    let mut results = [Val::Bool(false)];
    run.call(&mut store, &[], &mut results)
        .expect("the guest runs to its end without a trap");
    assert_eq!(
        results[0],
        Val::Result(Ok(None)),
        "the guest reports success only for the typed node-limit refusal with its message"
    );
}
