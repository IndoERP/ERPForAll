//! Dual-mode addons: two fixture addons
//! extend one model with identical source natively and as a `wasm32-wasip2` component, and the
//! component's `describe()` equals the native `erp_sdk::describe()`.
//!
//! The component test is `#[ignore]`d in the workspace run and fails, never skips, when it runs
//! without the artefact: `scripts/ci.sh` step `wasm-dual-describe` builds it and sets
//! `ERP_WASM_DUAL_COMPONENT`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::collections::BTreeSet;

use erp_fields::serde_json::{self, Value};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimitsBuilder};

// Naming a symbol of each fixture keeps its registrations in this binary's link.
use erp_addon_test_wasm_dual_a::ADDON as ADDON_A;
use erp_addon_test_wasm_dual_b::ADDON as ADDON_B;

const MODEL: &str = "test.wasm.dual";

fn native() -> Value {
    // the fixtures are linked, so their registrations are in the program
    assert_eq!(
        (ADDON_A.name, ADDON_B.name),
        ("test_wasm_dual_a", "test_wasm_dual_b")
    );
    serde_json::from_str(&erp_sdk::describe()).unwrap()
}

fn entries(doc: &Value) -> &Vec<Value> {
    doc["contributions"].as_array().unwrap()
}

fn kinds_of(doc: &Value, addon: &str) -> BTreeSet<String> {
    entries(doc)
        .iter()
        .filter(|e| e["addon"] == addon)
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_native_description_lists_both_addons_and_temporal_fixture() {
    let doc = native();
    assert_eq!(doc["schema"], erp_sdk::DESCRIBE_SCHEMA);
    let base_kinds = kinds_of(&doc, "test_wasm_dual_a");
    let ext_kinds = kinds_of(&doc, "test_wasm_dual_b");
    let expect =
        |names: &[&str]| -> BTreeSet<String> { names.iter().map(|n| (*n).to_owned()).collect() };
    assert_eq!(
        base_kinds,
        expect(&["addon", "model", "method", "compute", "constraint"])
    );
    assert_eq!(
        ext_kinds,
        expect(&[
            "addon",
            "model",
            "extend",
            "method",
            "override",
            "compute_override",
            "onchange"
        ])
    );
    // Both addons contribute to the shared model; B also declares the JSON lifecycle fixture.
    let models: BTreeSet<&str> = entries(&doc)
        .iter()
        .filter(|e| e["kind"] != "addon")
        .map(|e| e["model"].as_str().unwrap())
        .collect();
    assert_eq!(models, BTreeSet::from([MODEL, "test.wasm.temporal"]));
    // the fields of the model and of the extension are in the details
    let field_names = |kind: &str| -> Vec<String> {
        entries(&doc)
            .iter()
            .filter(|e| e["kind"] == kind && e["model"] == MODEL)
            .flat_map(|e| e["detail"]["fields"].as_array().unwrap().clone())
            .map(|f| f["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(field_names("model"), ["name", "qty", "double_qty"]);
    assert_eq!(field_names("extend"), ["note", "loan_date", "returned_at"]);
    // the override chain: the same method name is defined by A and overridden by B
    let label = |kind: &str| {
        entries(&doc)
            .iter()
            .filter(|e| e["kind"] == kind && e["name"] == "label")
            .count()
    };
    assert_eq!((label("method"), label("override")), (1, 1));
}

#[test]
fn the_description_is_stable_across_calls() {
    assert_eq!(erp_sdk::describe(), erp_sdk::describe());
}

#[test]
fn the_fixture_sources_have_no_conditional_compilation() {
    struct Boundary {
        modules: BTreeSet<String>,
        conditional: bool,
    }
    impl<'ast> syn::visit::Visit<'ast> for Boundary {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            assert!(
                item.attrs.is_empty(),
                "fixture module inclusion must be unconditional"
            );
            assert!(
                item.content.is_none(),
                "inline/unlisted fixture modules are not portable-source inventory entries"
            );
            assert!(
                self.modules.insert(item.ident.to_string()),
                "duplicate fixture module"
            );
            syn::visit::visit_item_mod(self, item);
        }
        fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
            self.conditional |= attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr");
            syn::visit::visit_attribute(self, attr);
        }
        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            self.conditional |= mac.path.is_ident("cfg");
            syn::visit::visit_macro(self, mac);
        }
    }
    use syn::visit::Visit;
    for (name, source, modules) in [
        (
            "test_wasm_dual_a",
            include_str!("../addons/test_wasm_dual_a/src/lib.rs"),
            BTreeSet::new(),
        ),
        (
            "test_wasm_dual_b",
            include_str!("../addons/test_wasm_dual_b/src/lib.rs"),
            BTreeSet::from(["adversarial_abi1".to_owned()]),
        ),
    ] {
        let mut boundary = Boundary {
            modules: BTreeSet::new(),
            conditional: false,
        };
        boundary.visit_file(&syn::parse_file(source).unwrap());
        assert!(
            !boundary.conditional && !source.contains("cfg(") && !source.contains("cfg!("),
            "{name} portable business source uses conditional compilation"
        );
        assert_eq!(
            boundary.modules, modules,
            "{name}: inventory every portable source module explicitly; no unlisted module may bypass this guard"
        );
    }

    // This is the sole exception: ABI1 wire-admission infrastructure, included identically in
    // both builds. Inspect it explicitly instead of silently excluding an arbitrary module.
    let adversarial = include_str!("../addons/test_wasm_dual_b/src/adversarial_abi1.rs");
    let parsed = syn::parse_file(adversarial).unwrap();
    let mut boundary = Boundary {
        modules: BTreeSet::new(),
        conditional: false,
    };
    boundary.visit_file(&parsed);
    assert!(
        boundary.modules.is_empty(),
        "the explicit ABI1 boundary cannot hide more modules"
    );
    let helper = parsed
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "abi1_inject_native_on_super" => {
                Some(function)
            }
            _ => None,
        })
        .expect("the ABI1 Super injection helper must remain present");
    assert!(
        helper.attrs.is_empty(),
        "helper inclusion must be unconditional"
    );
    let methods = parsed
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Impl(methods) => Some(methods),
            _ => None,
        })
        .expect("the TemporalJson adversarial method impl must remain present");
    assert!(
        matches!(methods.self_ty.as_ref(), syn::Type::Path(path) if path.path.is_ident("TemporalJson"))
    );
    assert_eq!(methods.attrs.len(), 1);
    assert!(
        methods.attrs[0].path().is_ident("methods"),
        "registration must not depend on target cfg"
    );
    let names: BTreeSet<_> = methods
        .items
        .iter()
        .map(|item| {
            let syn::ImplItem::Fn(function) = item else {
                panic!("unexpected ABI1 impl item")
            };
            assert!(
                function
                    .attrs
                    .iter()
                    .all(|attr| !attr.path().is_ident("cfg") && !attr.path().is_ident("cfg_attr")),
                "method registration must be unconditional"
            );
            function.sig.ident.to_string()
        })
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "abi1_native_probe".to_owned(),
            "abi1_native_child_probe".to_owned(),
            "create".to_owned(),
            "write".to_owned()
        ])
    );
    fn branches(body: &syn::Block) -> BTreeSet<String> {
        body.stmts
            .iter()
            .filter_map(|stmt| match stmt {
                syn::Stmt::Expr(syn::Expr::Block(block), _) => Some(&block.attrs),
                _ => None,
            })
            .flatten()
            .filter(|attr| attr.path().is_ident("cfg"))
            .map(|attr| {
                attr.meta
                    .require_list()
                    .unwrap()
                    .tokens
                    .to_string()
                    .replace(' ', "")
            })
            .collect()
    }
    let expected = BTreeSet::from([
        "target_family=\"wasm\"".to_owned(),
        "not(target_family=\"wasm\")".to_owned(),
    ]);
    assert_eq!(branches(&helper.block), expected);
    for name in ["abi1_native_probe", "abi1_native_child_probe"] {
        let probe = methods
            .items
            .iter()
            .find_map(|item| match item {
                syn::ImplItem::Fn(function) if function.sig.ident == name => Some(function),
                _ => None,
            })
            .expect("every acknowledged ABI1 probe must remain present");
        assert_eq!(
            branches(&probe.block),
            expected,
            "{name}: explicit ABI1 branches"
        );
    }
    for required in [
        "tx.call_context().context_json",
        "Ok(false)",
        "orm_host::read_columns",
        "orm_host::call_method",
        "orm_host::write_columns",
        "orm_host::create_batch",
        "orm_host::write_batch",
        "abi1_native_probe requires a real WebAssembly component",
        "abi1_native_child_probe requires a real WebAssembly component",
        "abi1_inject_native_on_super(tx)?",
        "vals.insert(\"native_uuid\".into(), Cell::Null)",
        "sup.call(tx, this.as_records(),",
    ] {
        assert!(
            adversarial.contains(required),
            "missing acknowledged ABI1 fixture behavior: {required}"
        );
    }
}

fn component_describe(path: &str) -> String {
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config).unwrap();
    let component = Component::from_file(&engine, path).unwrap();
    let limits = StoreLimitsBuilder::new()
        .memory_size(64 * 1024 * 1024)
        .build();
    let mut store = Store::new(&engine, limits);
    store.limiter(|limits| limits);
    store.set_fuel(500_000_000).unwrap();
    let mut linker = Linker::new(&engine);
    // the only imports are orm-host (never called by describe) and WASI: none is provided
    linker.define_unknown_imports_as_traps(&component).unwrap();
    let instance = linker.instantiate(&mut store, &component).unwrap();
    let interface = instance
        .get_export_index(&mut store, None, "erpforall:addon/addon-guest@1.0.0")
        .expect("the component exports addon-guest");
    let function = instance
        .get_export_index(&mut store, Some(&interface), "describe")
        .expect("addon-guest exports describe");
    let describe = instance
        .get_typed_func::<(), (String,)>(&mut store, function)
        .unwrap();
    let (first,) = describe.call(&mut store, ()).unwrap();
    // registrations are collected once at instantiation: a second call answers the same
    let (second,) = describe.call(&mut store, ()).unwrap();
    assert_eq!(
        first, second,
        "describe changed between calls on one instance"
    );
    first
}

#[test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_dual_b; run by scripts/ci.sh"]
fn the_component_describes_what_the_native_build_describes() {
    let path = std::env::var("ERP_WASM_DUAL_COMPONENT")
        .unwrap_or_else(|_| panic!("ERP_WASM_DUAL_COMPONENT must name the dual-mode component"));
    let from_component = component_describe(&path);
    let from_native = erp_sdk::describe();
    assert_eq!(from_component, from_native);
    let doc: Value = serde_json::from_str(&from_component).unwrap();
    assert!(
        entries(&doc).len() > 8,
        "the component described too little: {from_component}"
    );
}
