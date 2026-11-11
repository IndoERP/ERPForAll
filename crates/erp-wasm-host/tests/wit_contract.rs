//! The WIT contract in `wit/addon/`: it parses, carries the identity the host
//! declares, imports nothing but the ORM host, encodes to a valid component, and its type surface
//! matches the frozen golden of its version. Older minor goldens of the same major must stay
//! semver-compatible with the current files.
//!
//! The checks use the pinned wasm-tools libraries (`wit-parser`, `wit-component`, `wasmparser`;
//! exact versions in the workspace `Cargo.toml`), so CI needs no separately installed tool.
//!
//! `ERP_BLESS=1` writes the golden of the current version when it does not exist yet. A golden that
//! exists is never rewritten: a released version is frozen, and an unreleased one is deleted by hand
//! (visible in the diff) before it is blessed again.

#![allow(clippy::expect_used, clippy::panic, reason = "test assertions")]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use erp_wasm_host::{WIT_PACKAGE, WIT_VERSION, WIT_WORLD};
use wit_component::{DecodedWasm, StringEncoding, WitPrinter};
use wit_parser::{
    IndexMap, LiftLowerAbi, ManglingAndAbi, PackageId, Resolve, WorldId, WorldItem, WorldKey,
};

const GOLDEN_PREFIX: &str = "erpforall-addon-";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn contract_dir() -> PathBuf {
    workspace_root().join("wit/addon")
}

fn golden_dir() -> PathBuf {
    workspace_root().join("wit/golden")
}

fn current_version() -> semver::Version {
    semver::Version::parse(WIT_VERSION).expect("WIT_VERSION is a semver version")
}

/// The contract package resolved from `wit/addon/`.
fn load_contract() -> (Resolve, PackageId) {
    let mut resolve = Resolve::new();
    let (pkg, _sources) = resolve
        .push_dir(contract_dir())
        .unwrap_or_else(|e| panic!("wit/addon does not resolve: {e:#}"));
    (resolve, pkg)
}

fn world(resolve: &Resolve, pkg: PackageId) -> WorldId {
    resolve
        .select_world(&[pkg], Some(WIT_WORLD))
        .unwrap_or_else(|e| panic!("world `{WIT_WORLD}` not found: {e:#}"))
}

/// The package's type surface without doc comments: what the golden freezes.
fn surface(resolve: &Resolve, pkg: PackageId) -> String {
    let mut printer = WitPrinter::default();
    printer.emit_docs(false);
    printer
        .print(resolve, pkg, &[])
        .unwrap_or_else(|e| panic!("printing the package failed: {e:#}"));
    printer.output.to_string()
}

fn interface_names(resolve: &Resolve, items: &IndexMap<WorldKey, WorldItem>) -> BTreeSet<String> {
    items
        .iter()
        .map(|(key, item)| match item {
            WorldItem::Interface { id, .. } => resolve.id_of(*id).unwrap_or_else(|| {
                panic!(
                    "anonymous interface `{}` in the world",
                    resolve.name_world_key(key)
                )
            }),
            other => panic!(
                "world item `{}` is not an interface: {other:?}",
                resolve.name_world_key(key)
            ),
        })
        .collect()
}

/// Golden files: (version, path), sorted by version. Every file in `wit/golden/` must be a golden.
fn goldens() -> Vec<(semver::Version, PathBuf)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(golden_dir()).expect("wit/golden is readable") {
        let path = entry.expect("wit/golden entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("UTF-8 file name")
            .to_owned();
        let version = name
            .strip_prefix(GOLDEN_PREFIX)
            .and_then(|rest| rest.strip_suffix(".wit"))
            .and_then(|v| semver::Version::parse(v).ok())
            .unwrap_or_else(|| {
                panic!("wit/golden/{name} is not named {GOLDEN_PREFIX}<semver>.wit")
            });
        out.push((version, path));
    }
    out.sort();
    out
}

/// Load a golden file into `resolve`; its package must carry the version in its file name.
fn push_golden(resolve: &mut Resolve, version: &semver::Version, path: &Path) -> PackageId {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let pkg = resolve
        .push_str(path, &text)
        .unwrap_or_else(|e| panic!("{} does not resolve: {e:#}", path.display()));
    let name = &resolve.packages[pkg].name;
    assert_eq!(
        name.version.as_ref(),
        Some(version),
        "{} declares {name}",
        path.display()
    );
    pkg
}

#[test]
fn contract_resolves_with_the_declared_identity() {
    let (resolve, pkg) = load_contract();
    let name = &resolve.packages[pkg].name;
    assert_eq!(format!("{}:{}", name.namespace, name.name), WIT_PACKAGE);
    assert_eq!(name.version.as_ref(), Some(&current_version()));
    world(&resolve, pkg);
    assert!(
        !contract_dir().join("deps").exists(),
        "wit/addon/deps would pull in foreign packages; the contract stands alone"
    );
    assert_eq!(
        resolve.packages.len(),
        1,
        "only the contract package is resolved"
    );
}

#[test]
fn world_imports_only_the_orm_host() {
    let (resolve, pkg) = load_contract();
    let world = &resolve.worlds[world(&resolve, pkg)];
    let v = WIT_VERSION;
    let imports = interface_names(&resolve, &world.imports);
    let exports = interface_names(&resolve, &world.exports);
    assert_eq!(
        imports,
        BTreeSet::from([
            format!("{WIT_PACKAGE}/types@{v}"),
            format!("{WIT_PACKAGE}/orm-host@{v}")
        ])
    );
    assert_eq!(
        exports,
        BTreeSet::from([format!("{WIT_PACKAGE}/addon-guest@{v}")])
    );
}

#[test]
fn contract_encodes_to_a_valid_component_and_round_trips() {
    let (resolve, pkg) = load_contract();
    let bytes = wit_component::encode(&resolve, pkg, false)
        .unwrap_or_else(|e| panic!("encoding the package failed: {e:#}"));
    wasmparser::Validator::new()
        .validate_all(&bytes)
        .unwrap_or_else(|e| panic!("encoded package is not a valid component: {e}"));
    let DecodedWasm::WitPackage(decoded, decoded_pkg) =
        wit_component::decode(&bytes).unwrap_or_else(|e| panic!("decoding failed: {e:#}"))
    else {
        panic!("the encoded package decoded as a component, not a WIT package");
    };
    assert_eq!(surface(&decoded, decoded_pkg), surface(&resolve, pkg));
}

#[test]
fn a_component_targeting_the_world_is_valid() {
    let (resolve, pkg) = load_contract();
    let world = world(&resolve, pkg);
    let mut module =
        wit_component::dummy_module(&resolve, world, ManglingAndAbi::Legacy(LiftLowerAbi::Sync));
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        StringEncoding::UTF8,
        false,
    )
    .unwrap_or_else(|e| panic!("embedding the world failed: {e:#}"));
    let component = wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&module)
        .unwrap_or_else(|e| panic!("the dummy module was refused: {e:#}"))
        .encode()
        .unwrap_or_else(|e| panic!("componentizing the dummy module failed: {e:#}"));
    wasmparser::Validator::new()
        .validate_all(&component)
        .unwrap_or_else(|e| panic!("the component is not valid: {e}"));
}

#[test]
fn surface_matches_the_golden_of_its_version() {
    let (resolve, pkg) = load_contract();
    let actual = surface(&resolve, pkg);
    let path = golden_dir().join(format!("{GOLDEN_PREFIX}{WIT_VERSION}.wit"));
    if !path.exists() && std::env::var("ERP_BLESS").is_ok_and(|v| v == "1") {
        fs::write(&path, &actual).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; a new version needs its golden (ERP_BLESS=1 writes a missing one)",
            path.display()
        )
    });
    assert!(
        actual == expected,
        "the contract's types differ from the frozen golden {}.\nA released version never changes: \
         bump WIT_VERSION and the package version (minor when only functions, interfaces or imports \
         were added, major otherwise) and bless the new golden.\n--- golden\n{expected}\n--- now\n{actual}",
        path.display()
    );
}

#[test]
fn older_goldens_stay_within_the_compatibility_window() {
    let current = current_version();
    let all = goldens();
    // Every minor of the current major keeps its golden, so the compatibility evidence for each
    // release a guest may have been built against cannot be dropped unnoticed.
    for minor in 0..=current.minor {
        let release = semver::Version::new(current.major, minor, 0);
        assert!(
            all.iter().any(|(v, _)| *v == release),
            "wit/golden has no golden for {release}; a released minor's golden is never removed"
        );
    }
    for (version, path) in &all {
        assert!(
            *version <= current,
            "{} is newer than the contract {current}",
            path.display()
        );
        assert!(
            version.major + 1 >= current.major,
            "{} is older than the previous major; drop it from the window",
            path.display()
        );
        if *version == current || version.major != current.major {
            continue;
        }
        let (mut resolve, pkg) = load_contract();
        let new_world = world(&resolve, pkg);
        let old_pkg = push_golden(&mut resolve, version, path);
        let old_world = resolve
            .select_world(&[old_pkg], Some(WIT_WORLD))
            .unwrap_or_else(|e| panic!("{}: world `{WIT_WORLD}`: {e:#}", path.display()));
        wit_component::semver_check(resolve, old_world, new_world).unwrap_or_else(|e| {
            panic!("{current} is not a compatible upgrade of {version}: {e:#}")
        });
    }
}

/// The compatibility check used above treats an added function as compatible and an added record
/// field as a break. Checked on two tiny packages, so the rule is proven, not assumed.
#[test]
fn semver_check_accepts_added_functions_and_refuses_changed_records() {
    fn pair(old: &str, new: &str) -> Result<(), String> {
        let mut resolve = Resolve::new();
        let old_pkg = resolve
            .push_str("old.wit", old)
            .expect("old fixture resolves");
        let new_pkg = resolve
            .push_str("new.wit", new)
            .expect("new fixture resolves");
        let old_world = resolve
            .select_world(&[old_pkg], Some("w"))
            .expect("old world");
        let new_world = resolve
            .select_world(&[new_pkg], Some("w"))
            .expect("new world");
        wit_component::semver_check(resolve, old_world, new_world).map_err(|e| format!("{e:#}"))
    }
    let base = "package t:p@1.0.0; interface i { record r { a: u32 } f: func(x: r); } world w { import i; }";
    let more_funcs = "package t:p@1.1.0; interface i { record r { a: u32 } f: func(x: r); g: func(); } world w { import i; }";
    let wider_record = "package t:p@1.1.0; interface i { record r { a: u32, b: u32 } f: func(x: r); } world w { import i; }";
    assert!(
        pair(base, more_funcs).is_ok(),
        "an added function is a minor change"
    );
    // The refusal comes from wasmparser's type check and its wording is not stable across
    // releases, so only the outcome is asserted here.
    assert!(
        pair(base, wider_record).is_err(),
        "an added record field must be a major change"
    );
}
