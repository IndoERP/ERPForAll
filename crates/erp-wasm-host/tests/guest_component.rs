//! A component built with the guest SDK targets the contract.
//!
//! CI builds `crates/erp-guest/examples/minimal_guest.rs` for `wasm32-wasip2` and runs this test
//! with `ERP_GUEST_COMPONENT` set to the artefact. The test is `#[ignore]`d in the workspace run and
//! fails, never skips, when it runs without the artefact (`scripts/ci.sh`, step
//! `guest-component`).

#![allow(clippy::expect_used, clippy::panic, reason = "test assertions")]

use std::collections::BTreeSet;

use erp_wasm_host::{WIT_PACKAGE, WIT_VERSION};
use wit_component::DecodedWasm;
use wit_parser::WorldItem;

#[test]
#[ignore = "needs the wasm32-wasip2 build of the minimal guest; run by scripts/ci.sh"]
fn the_minimal_guest_is_a_component_of_the_addon_world() {
    let path = std::env::var("ERP_GUEST_COMPONENT")
        .unwrap_or_else(|_| panic!("ERP_GUEST_COMPONENT must name the minimal guest component"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    wasmparser::Validator::new()
        .validate_all(&bytes)
        .unwrap_or_else(|e| panic!("{path} is not a valid component: {e}"));
    let DecodedWasm::Component(resolve, world) =
        wit_component::decode(&bytes).unwrap_or_else(|e| panic!("decoding {path} failed: {e:#}"))
    else {
        panic!("{path} decoded as a WIT package, not a component");
    };
    let names =
        |items: &wit_parser::IndexMap<wit_parser::WorldKey, WorldItem>| -> BTreeSet<String> {
            items
                .iter()
                .filter_map(|(key, item)| match item {
                    WorldItem::Interface { id, .. } => {
                        Some(resolve.id_of(*id).unwrap_or_else(|| {
                            panic!(
                                "anonymous interface `{}` in the component",
                                resolve.name_world_key(key)
                            )
                        }))
                    }
                    // Types a component re-exports alongside its interfaces carry no capability.
                    WorldItem::Type { .. } => None,
                    WorldItem::Function(f) => {
                        panic!("the component has a bare function `{}`", f.name)
                    }
                })
                .collect()
        };
    let world = &resolve.worlds[world];
    let contract = |interface: &str| format!("{WIT_PACKAGE}/{interface}@{WIT_VERSION}");
    assert_eq!(
        names(&world.exports),
        BTreeSet::from([contract("addon-guest")])
    );
    let imports = names(&world.imports);
    assert!(
        imports.contains(&contract("orm-host")),
        "orm-host is not imported: {imports:?}"
    );
    // The host answers WASI imports the Rust standard library adds with a deny-by-default context
    // ; anything else would be an undeclared capability.
    let undeclared: Vec<&String> = imports
        .iter()
        .filter(|name| {
            **name != contract("orm-host")
                && **name != contract("types")
                && !name.starts_with("wasi:")
        })
        .collect();
    assert!(
        undeclared.is_empty(),
        "the component imports interfaces outside the contract: {undeclared:?}"
    );
}
