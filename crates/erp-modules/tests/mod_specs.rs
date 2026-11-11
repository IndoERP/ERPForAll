#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_modules::dependencies::ExternalDependencies;
use erp_modules::discovery::{AddonIndex, read_manifest_from_dir};
use erp_modules::error::Error;
use erp_modules::graph::{DropReason, LoadMode, ModuleGraph, ModuleState};
use erp_modules::manifest::{AutoInstall, Manifest, ModuleName};
use erp_modules::version::SemverVersion;
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;

fn make_manifest(name: &str, depends: Vec<&str>) -> Result<Arc<Manifest>, Error> {
    let toml = format!(
        r#"
name = "{name}"
depends = {:?}
"#,
        depends
    );
    Manifest::from_toml(name, &toml).map(Arc::new)
}

/// Helper to build states map where all given names have the given state.
fn make_states(names: &[&str], state: ModuleState) -> HashMap<ModuleName, ModuleState> {
    names
        .iter()
        .map(|&n| (ModuleName::new(n).unwrap(), state))
        .collect()
}

#[test]
fn test_default_manifest_values() {
    let toml = r#"
name = "My Module"
license = "MIT"
author = "Fapi"
"#;
    let manifest = Manifest::from_toml("my_module", toml).unwrap();

    assert_eq!(manifest.name().as_str(), "my_module");
    assert_eq!(manifest.shortdesc(), "My Module");
    assert_eq!(manifest.author(), "Fapi");
    assert_eq!(manifest.license(), "MIT");
    assert_eq!(manifest.depends(), &[ModuleName::new("base").unwrap()]);
    assert_eq!(manifest.version().to_string(), "1.0.0");
    assert!(manifest.installable());
    assert_eq!(*manifest.auto_install(), AutoInstall::Disabled);
    assert_eq!(manifest.category(), "Uncategorized");
    assert_eq!(manifest.sequence(), 100);
    assert!(manifest.data().is_empty());
    assert!(manifest.demo().is_empty());
    assert!(manifest.pre_init_hook().is_none());
    assert!(manifest.post_init_hook().is_none());
    assert!(manifest.uninstall_hook().is_none());
    assert!(!manifest.application());
    assert_eq!(manifest.icon(), "/base/static/description/icon.png");
    assert!(manifest.static_path().is_none());
    assert_eq!(manifest.description(), "");
    assert!(manifest.extra().is_empty());
}

#[test]
fn test_missing_author_and_license() {
    let toml = r#"
name = "Bare Module"
"#;
    let manifest = Manifest::from_toml("bare_module", toml).unwrap();

    assert_eq!(manifest.author(), "");
    assert_eq!(manifest.license(), "LGPL-3");
}

#[test]
fn test_manifest_is_immutable_and_cached() {
    let toml = r#"
name = "Base"
depends = []
"#;
    let base_manifest = Manifest::from_toml("base", toml).unwrap();

    let mut index = AddonIndex::new(true);

    // 1. Embedded source caching
    index
        .add_embedded(base_manifest.clone(), "embedded:base")
        .unwrap();

    let first_lookup = index.lookup("base").unwrap().expect("base found");
    let second_lookup = index.lookup("base").unwrap().expect("base found again");

    assert!(Arc::ptr_eq(&first_lookup, &second_lookup));
    assert_eq!(first_lookup.name().as_str(), "base");

    // 2. Package source caching
    let pkg_toml = r#"
name = "pkg_mod"
depends = ["base"]
"#;
    let pkg_manifest = Manifest::from_toml("pkg_mod", pkg_toml).unwrap();
    index
        .add_package(pkg_manifest, "package:pkg_mod.erpaddon")
        .unwrap();

    let pkg_1 = index.lookup("pkg_mod").unwrap().expect("pkg_mod found");
    let pkg_2 = index
        .lookup("pkg_mod")
        .unwrap()
        .expect("pkg_mod found again");
    assert!(Arc::ptr_eq(&pkg_1, &pkg_2));

    // 3. Dev folder source caching
    let dir = tempdir().unwrap();
    let dev_mod_dir = dir.path().join("dev_mod");
    fs::create_dir(&dev_mod_dir).unwrap();
    fs::write(
        dev_mod_dir.join("manifest.toml"),
        "name = \"dev_mod\"\ndepends = [\"base\"]\n",
    )
    .unwrap();

    index.add_dev_path(dir.path());
    let dev_1 = index.lookup("dev_mod").unwrap().expect("dev_mod found");
    let dev_2 = index
        .lookup("dev_mod")
        .unwrap()
        .expect("dev_mod found again");
    assert!(Arc::ptr_eq(&dev_1, &dev_2));

    // 4. Cached after on-disk change
    fs::write(
        dev_mod_dir.join("manifest.toml"),
        "name = \"dev_mod_changed_on_disk\"\ndepends = [\"base\"]\n",
    )
    .unwrap();
    let dev_3 = index
        .lookup("dev_mod")
        .unwrap()
        .expect("dev_mod found from cache");
    assert!(Arc::ptr_eq(&dev_1, &dev_3));
    assert_eq!(dev_3.shortdesc(), "dev_mod");
}

#[test]
fn test_missing_or_unparsable_manifest() {
    let dir = tempdir().unwrap();

    // 1. Directory without manifest.toml -> read_manifest_from_dir is Ok(None)
    let empty_dir = dir.path().join("empty_mod");
    fs::create_dir(&empty_dir).unwrap();
    let mod_empty = ModuleName::new("empty_mod").unwrap();
    let res1 = read_manifest_from_dir(&empty_dir, &mod_empty).unwrap();
    assert!(res1.is_none(), "missing manifest must return Ok(None)");

    let mut index = AddonIndex::new(true);
    index.add_dev_path(dir.path());
    let lookup_missing = index.lookup("empty_mod").unwrap();
    assert!(lookup_missing.is_none());

    // 2. Directory with unparsable manifest -> returns Err(Error::InvalidManifest)
    let bad_dir = dir.path().join("bad_mod");
    fs::create_dir(&bad_dir).unwrap();
    fs::write(bad_dir.join("manifest.toml"), "invalid = [ unclosed array").unwrap();
    let mod_bad = ModuleName::new("bad_mod").unwrap();
    let err_res = read_manifest_from_dir(&bad_dir, &mod_bad).unwrap_err();
    assert!(matches!(err_res, Error::InvalidManifest { .. }));
    let msg = err_res.to_string();
    assert_eq!(
        msg,
        "Module bad_mod: invalid manifest: TOML parse error at line 1, column 13\n  |\n1 | invalid = [ unclosed array\n  |             ^\ninvalid array\nexpected `]`\n"
    );

    // 3. Lookup on broken dev addon returns Err and never falls back to embedded copy
    let mut collision_index = AddonIndex::new(true);
    collision_index.add_dev_path(dir.path());
    let fallback_manifest = Manifest::from_toml("bad_mod", "name = 'Fallback'\n").unwrap();
    collision_index
        .add_embedded(fallback_manifest, "embedded:bad_mod")
        .unwrap();

    let lookup_err = collision_index.lookup("bad_mod").unwrap_err();
    assert!(matches!(lookup_err, Error::InvalidManifest { .. }));
    assert_eq!(
        lookup_err.to_string(),
        "Module bad_mod: invalid manifest: TOML parse error at line 1, column 13\n  |\n1 | invalid = [ unclosed array\n  |             ^\ninvalid array\nexpected `]`\n"
    );
}

#[test]
fn test_depends_defaults_to_base_and_cannot_be_emptied() {
    // 1. Regular module with empty depends defaults to ['base']
    let x_toml = r#"
name = "Module X"
depends = []
"#;
    let x_manifest = Manifest::from_toml("module_x", x_toml).unwrap();
    assert_eq!(
        x_manifest.depends(),
        &[ModuleName::new("base").unwrap()],
        "empty depends on regular module must default to ['base']"
    );

    // 2. Regular module with omitted depends defaults to ['base']
    let omitted_toml = r#"
name = "Omitted Depends"
"#;
    let omitted_manifest = Manifest::from_toml("module_omitted", omitted_toml).unwrap();
    assert_eq!(
        omitted_manifest.depends(),
        &[ModuleName::new("base").unwrap()],
        "omitted depends on regular module must default to ['base']"
    );

    // 3. 'base' module depends forced to []
    let base_toml = r#"
name = "Base"
depends = ["web"]
"#;
    let base_manifest = Manifest::from_toml("base", base_toml).unwrap();
    assert!(
        base_manifest.depends().is_empty(),
        "base module depends must always be forced to []"
    );

    // 4. Self dependency is an error
    let self_dep_toml = r#"
name = "Self Dep"
depends = ["self_dep"]
"#;
    let err_self = Manifest::from_toml("self_dep", self_dep_toml).unwrap_err();
    assert!(matches!(err_self, Error::SelfDependency { .. }));
    assert_eq!(err_self.to_string(), "Module 'self_dep' depends on itself");

    // 5. Duplicate dependencies is an error
    let dup_dep_toml = r#"
name = "Dup Dep"
depends = ["base", "base"]
"#;
    let err_dup = Manifest::from_toml("dup_dep", dup_dep_toml).unwrap_err();
    assert!(matches!(err_dup, Error::DuplicateDependency { .. }));
    assert_eq!(
        err_dup.to_string(),
        "Module 'dup_dep' has duplicate dependency 'base'"
    );

    // 6. Empty hook name is an error (not silently coerced to None)
    let empty_hook_toml = r#"
name = "Empty Hook"
pre_init_hook = ""
"#;
    let err_empty_hook = Manifest::from_toml("empty_hook", empty_hook_toml).unwrap_err();
    assert_eq!(
        err_empty_hook.to_string(),
        "Module empty_hook: invalid manifest: hook name cannot be empty"
    );

    // 7. Invalid characters in hook name is an error
    let bad_hook_toml = r#"
name = "Bad Hook"
post_init_hook = "bad-hook!"
"#;
    let err_bad_hook = Manifest::from_toml("bad_hook", bad_hook_toml).unwrap_err();
    assert_eq!(
        err_bad_hook.to_string(),
        "Module bad_hook: invalid manifest: hook name 'bad-hook!' must contain only alphanumeric characters or underscores"
    );

    // 8. Path with '..' in data is an error
    let dot_data_toml = r#"
name = "Dot Data"
data = ["../escaped.xml"]
"#;
    let err_dot_data = Manifest::from_toml("dot_data", dot_data_toml).unwrap_err();
    assert_eq!(
        err_dot_data.to_string(),
        "Module dot_data: invalid manifest: data path '../escaped.xml' cannot contain '..'"
    );

    // 9. Path with '..' in demo is an error
    let dot_demo_toml = r#"
name = "Dot Demo"
demo = ["demo/../../escaped.xml"]
"#;
    let err_dot_demo = Manifest::from_toml("dot_demo", dot_demo_toml).unwrap_err();
    assert_eq!(
        err_dot_demo.to_string(),
        "Module dot_demo: invalid manifest: demo path 'demo/../../escaped.xml' cannot contain '..'"
    );

    // 10. A country code that is not two letters is an error
    let dot_countries_toml = r#"
name = "Dot Countries"
countries = ["../escaped"]
"#;
    let err_dot_countries = Manifest::from_toml("dot_countries", dot_countries_toml).unwrap_err();
    assert_eq!(
        err_dot_countries.to_string(),
        "Module dot_countries: invalid manifest: Invalid country code '../escaped': expected two ASCII letters (ISO 3166-1 alpha-2)"
    );

    // 11. Absolute path in data is an error
    let abs_data_toml = r#"
name = "Abs Data"
data = ["/etc/x"]
"#;
    let err_abs_data = Manifest::from_toml("abs_data", abs_data_toml).unwrap_err();
    assert_eq!(
        err_abs_data.to_string(),
        "Module abs_data: invalid manifest: data path '/etc/x' cannot be absolute"
    );

    // 12. Path with legitimate '..' in filename like 'a..b.xml' is accepted
    let dotdot_name_toml = r#"
name = "Dotdot Name"
data = ["data/a..b.xml"]
"#;
    let dotdot_manifest = Manifest::from_toml("dotdot_name", dotdot_name_toml).unwrap();
    assert_eq!(dotdot_manifest.data(), &["data/a..b.xml"]);

    // 13. Self exclusion is an error
    let self_ex_toml = r#"
name = "Self Ex"
excludes = ["self_ex"]
"#;
    let err_self_ex = Manifest::from_toml("self_ex", self_ex_toml).unwrap_err();
    assert_eq!(err_self_ex.to_string(), "Module 'self_ex' excludes itself");

    // 14. Duplicate exclusion is an error
    let dup_ex_toml = r#"
name = "Dup Ex"
excludes = ["other", "other"]
"#;
    let err_dup_ex = Manifest::from_toml("dup_ex", dup_ex_toml).unwrap_err();
    assert_eq!(
        err_dup_ex.to_string(),
        "Module 'dup_ex' has duplicate exclusion 'other'"
    );
}

#[test]
fn test_auto_install_normalization() {
    // 1. auto_install = true with explicit depends
    let toml_x = r#"
name = "X"
depends = ["sale", "stock"]
auto_install = true
"#;
    let x = Manifest::from_toml("x", toml_x).unwrap();
    let mut expected_x = BTreeSet::new();
    expected_x.insert(ModuleName::new("sale").unwrap());
    expected_x.insert(ModuleName::new("stock").unwrap());
    assert_eq!(x.auto_install().to_set(), expected_x);

    // 2. auto_install = true with default depends
    let toml_def = r#"
name = "Def"
auto_install = true
"#;
    let def = Manifest::from_toml("def", toml_def).unwrap();
    let mut expected_def = BTreeSet::new();
    expected_def.insert(ModuleName::new("base").unwrap());
    assert_eq!(def.auto_install().to_set(), expected_def);

    // 3. auto_install = false
    let toml_false = r#"
name = "False Mod"
auto_install = false
"#;
    let false_mod = Manifest::from_toml("false_mod", toml_false).unwrap();
    assert_eq!(*false_mod.auto_install(), AutoInstall::Disabled);
    assert!(!false_mod.auto_install().is_enabled());

    // 4. auto_install = ["sale"] subset of depends
    let toml_y = r#"
name = "Y"
depends = ["sale", "stock"]
auto_install = ["sale"]
"#;
    let y = Manifest::from_toml("y", toml_y).unwrap();
    let mut expected_y = BTreeSet::new();
    expected_y.insert(ModuleName::new("sale").unwrap());
    assert_eq!(y.auto_install().to_set(), expected_y);

    // 5. auto_install with non-dependency trigger raises error
    let toml_z = r#"
name = "Z"
depends = ["sale", "stock"]
auto_install = ["crm"]
"#;
    let err_z = Manifest::from_toml("z", toml_z).unwrap_err();
    let msg = err_z.to_string();
    assert_eq!(
        msg,
        "auto_install triggers must be dependencies, found non-dependencies [crm] for module z"
    );
    match err_z {
        Error::AutoInstallTriggerMismatch {
            name,
            non_dependencies,
        } => {
            assert_eq!(name.as_str(), "z");
            assert_eq!(non_dependencies, vec![ModuleName::new("crm").unwrap()]);
        }
        other => panic!("expected AutoInstallTriggerMismatch, got {other:?}"),
    }
}

#[test]
fn test_version_adaptation_and_series_check() {
    // 1. '1.0' -> '1.0.0', installable=true
    let m1 = Manifest::from_toml("m1", "version = '1.0'").unwrap();
    assert_eq!(m1.version().to_string(), "1.0.0");
    assert!(m1.version().is_compatible());
    assert_eq!(m1.version().semver(), Some(&SemverVersion::new(1, 0, 0)));
    assert!(m1.installable());

    // 2. '1.2.3' -> '1.2.3', installable=true
    let m2 = Manifest::from_toml("m2", "version = '1.2.3'").unwrap();
    assert_eq!(m2.version().to_string(), "1.2.3");
    assert!(m2.version().is_compatible());
    assert_eq!(m2.version().semver(), Some(&SemverVersion::new(1, 2, 3)));
    assert!(m2.installable());

    // 3. '20.0' bare series -> '0.0.0', installable=true (D4 accepted)
    let m_bare = Manifest::from_toml("m_bare", "version = '20.0'").unwrap();
    assert_eq!(m_bare.version().to_string(), "0.0.0");
    assert!(m_bare.version().is_compatible());
    assert_eq!(
        m_bare.version().semver(),
        Some(&SemverVersion::new(0, 0, 0))
    );
    assert!(m_bare.installable());

    // 4. '20.0.1' (3-part with series) -> '1.0.0', installable=true
    let m_3series = Manifest::from_toml("m_3s", "version = '20.0.1'").unwrap();
    assert_eq!(m_3series.version().to_string(), "1.0.0");
    assert!(m_3series.version().is_compatible());
    assert_eq!(
        m_3series.version().semver(),
        Some(&SemverVersion::new(1, 0, 0))
    );
    assert!(m_3series.installable());

    // 5. '20.0.1.0' -> mapped to '1.0.0', installable=true
    let m3 = Manifest::from_toml("m3", "version = '20.0.1.0'").unwrap();
    assert_eq!(m3.version().to_string(), "1.0.0");
    assert!(m3.version().is_compatible());
    assert!(m3.installable());

    // 6. '20.0.1.2.3' (5 parts) -> mapped to '1.2.3', installable=true
    let m_5parts = Manifest::from_toml("m_5p", "version = '20.0.1.2.3'").unwrap();
    assert_eq!(m_5parts.version().to_string(), "1.2.3");
    assert!(m_5parts.version().is_compatible());
    assert!(m_5parts.installable());

    // 7. '19.0.1.0' -> incompatible series, installable=false
    let m4 = Manifest::from_toml("m4", "version = '19.0.1.0'").unwrap();
    assert_eq!(m4.version().raw(), "19.0.1.0");
    assert!(!m4.version().is_compatible());
    assert!(!m4.installable());

    // 8. '1.2.3.4' -> 4 parts without 20.0 series, incompatible, installable=false
    let m5 = Manifest::from_toml("m5", "version = '1.2.3.4'").unwrap();
    assert_eq!(m5.version().raw(), "1.2.3.4");
    assert!(!m5.version().is_compatible());
    assert!(!m5.installable());

    // 9. '1' -> invalid version (single component), raises error
    let err_single = Manifest::from_toml("m6", "version = '1'").unwrap_err();
    assert_eq!(
        err_single.to_string(),
        "Module m6: invalid manifest: invalid version '1': version must have 2 to 5 dot-separated integer components, got 1"
    );

    // 10. '1.2.3.4.5.6' -> 6 parts, raises error
    let err_six = Manifest::from_toml("m_six", "version = '1.2.3.4.5.6'").unwrap_err();
    assert_eq!(
        err_six.to_string(),
        "Module m_six: invalid manifest: invalid version '1.2.3.4.5.6': version must have 2 to 5 dot-separated integer components, got 6"
    );

    // 11. '+1.0' -> invalid non-digit character, raises error
    let err_plus = Manifest::from_toml("m_plus", "version = '+1.0'").unwrap_err();
    assert_eq!(
        err_plus.to_string(),
        "Module m_plus: invalid manifest: invalid version '+1.0': component '+1' contains non-ASCII-digit characters"
    );

    // 12. '020.0.1' -> leading zero, raises error
    let err_zero = Manifest::from_toml("m_zero", "version = '020.0.1'").unwrap_err();
    assert_eq!(
        err_zero.to_string(),
        "Module m_zero: invalid manifest: invalid version '020.0.1': component '020' has invalid leading zero"
    );

    // 13. 'a.b' -> invalid version (non-numeric), raises error
    let err_alpha = Manifest::from_toml("m7", "version = 'a.b'").unwrap_err();
    assert_eq!(
        err_alpha.to_string(),
        "Module m7: invalid manifest: invalid version 'a.b': component 'a' contains non-ASCII-digit characters"
    );
}

#[test]
fn test_external_dependencies_block_install() {
    // Positive check: binary that exists on PATH (e.g. 'sh')
    let positive_deps = ExternalDependencies {
        bin: vec!["sh".to_string()],
        python: vec!["any_python_pkg".to_string()],
    };
    let install = erp_modules::ModuleOperation::Install;
    assert!(
        positive_deps
            .check(&ModuleName::new("mod_sh").unwrap(), install)
            .is_ok()
    );

    // Python entries are ignored (n/a-by-construction)
    let python_only_deps = ExternalDependencies {
        bin: vec![],
        python: vec!["completely_bogus_nonexistent_python_package_xyz".to_string()],
    };
    assert!(
        python_only_deps
            .check(&ModuleName::new("mod_python").unwrap(), install)
            .is_ok(),
        "python entries must be ignored"
    );

    // Negative check: missing binary blocks install
    let ext_deps = ExternalDependencies {
        bin: vec!["nonexistent_tool_binary_xyz_456".to_string()],
        python: vec!["nonexistent_pkg>=1.0".to_string()],
    };

    let err = ext_deps
        .check(&ModuleName::new("x").unwrap(), install)
        .unwrap_err();
    let msg = err.to_string();
    assert_eq!(
        msg,
        "Unable to install module \"x\" because an external dependency is not met: nonexistent_tool_binary_xyz_456"
    );
    match err {
        Error::ExternalDependencyMissing {
            module, dependency, ..
        } => {
            assert_eq!(module.as_str(), "x");
            assert_eq!(dependency, "nonexistent_tool_binary_xyz_456");
        }
        other => panic!("expected ExternalDependencyMissing, got {other:?}"),
    }
}

#[test]
fn test_graph_order_by_depth_then_name() {
    // Given: module1←base, module2←module1, module3←module1, module4←{module2,module3}, module5←{module2,module4}
    let mut manifests = HashMap::new();
    manifests.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module1").unwrap(),
        make_manifest("module1", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module2").unwrap(),
        make_manifest("module2", vec!["module1"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module3").unwrap(),
        make_manifest("module3", vec!["module1"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module4").unwrap(),
        make_manifest("module4", vec!["module2", "module3"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module5").unwrap(),
        make_manifest("module5", vec!["module2", "module4"]).unwrap(),
    );

    let states = make_states(
        &[
            "base", "module1", "module2", "module3", "module4", "module5",
        ],
        ModuleState::Installed,
    );

    // 1. Single round build
    let (graph, drops) = ModuleGraph::build(&manifests, &states, LoadMode::Load).unwrap();
    assert!(drops.is_empty());
    assert_eq!(
        graph.module_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
            ModuleName::new("module4").unwrap(),
            ModuleName::new("module5").unwrap(),
        ]
    );

    // Diamond with tied depths: module2 and module3 both have depth 2; sorted by name
    let n2 = graph.get(&ModuleName::new("module2").unwrap()).unwrap();
    let n3 = graph.get(&ModuleName::new("module3").unwrap()).unwrap();
    assert_eq!(n2.depth(), 2);
    assert_eq!(n3.depth(), 2);

    // Tie-break with insertion order != name order: insert module3 before module2 using IndexMap
    let mut reverse_insert_m = IndexMap::new();
    reverse_insert_m.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    reverse_insert_m.insert(
        ModuleName::new("module1").unwrap(),
        make_manifest("module1", vec!["base"]).unwrap(),
    );
    reverse_insert_m.insert(
        ModuleName::new("module3").unwrap(),
        make_manifest("module3", vec!["module1"]).unwrap(),
    );
    reverse_insert_m.insert(
        ModuleName::new("module2").unwrap(),
        make_manifest("module2", vec!["module1"]).unwrap(),
    );
    let mut reverse_s = IndexMap::new();
    reverse_s.insert(ModuleName::new("base").unwrap(), ModuleState::Installed);
    reverse_s.insert(ModuleName::new("module1").unwrap(), ModuleState::Installed);
    reverse_s.insert(ModuleName::new("module3").unwrap(), ModuleState::Installed);
    reverse_s.insert(ModuleName::new("module2").unwrap(), ModuleState::Installed);

    let (rev_graph, _) = ModuleGraph::build(&reverse_insert_m, &reverse_s, LoadMode::Load).unwrap();
    assert_eq!(
        rev_graph.module_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
        ]
    );

    // 2. Multi-round extend accumulates earlier rounds
    let mut incremental_graph = ModuleGraph::new(LoadMode::Load);

    let mut round1_m = HashMap::new();
    round1_m.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    round1_m.insert(
        ModuleName::new("module1").unwrap(),
        make_manifest("module1", vec!["base"]).unwrap(),
    );
    let round1_s = make_states(&["base", "module1"], ModuleState::Installed);
    incremental_graph.extend(&round1_m, &round1_s).unwrap();
    assert_eq!(incremental_graph.len(), 2);

    let mut round2_m = HashMap::new();
    round2_m.insert(
        ModuleName::new("module2").unwrap(),
        make_manifest("module2", vec!["module1"]).unwrap(),
    );
    round2_m.insert(
        ModuleName::new("module3").unwrap(),
        make_manifest("module3", vec!["module1"]).unwrap(),
    );
    let round2_s = make_states(&["module2", "module3"], ModuleState::Installed);
    incremental_graph.extend(&round2_m, &round2_s).unwrap();
    assert_eq!(incremental_graph.len(), 4);

    let mut round3_m = HashMap::new();
    round3_m.insert(
        ModuleName::new("module4").unwrap(),
        make_manifest("module4", vec!["module2", "module3"]).unwrap(),
    );
    round3_m.insert(
        ModuleName::new("module5").unwrap(),
        make_manifest("module5", vec!["module2", "module4"]).unwrap(),
    );
    let round3_s = make_states(&["module4", "module5"], ModuleState::Installed);
    incremental_graph.extend(&round3_m, &round3_s).unwrap();
    assert_eq!(incremental_graph.len(), 6);

    assert_eq!(
        incremental_graph.module_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
            ModuleName::new("module4").unwrap(),
            ModuleName::new("module5").unwrap(),
        ]
    );

    // 3. Extend round that drops a module preserves earlier rounds without corruption
    let mut round4_m = HashMap::new();
    round4_m.insert(
        ModuleName::new("dropped_mod").unwrap(),
        make_manifest("dropped_mod", vec!["nonexistent_dep"]).unwrap(),
    );
    round4_m.insert(
        ModuleName::new("cascade_dropped").unwrap(),
        make_manifest("cascade_dropped", vec!["dropped_mod"]).unwrap(),
    );
    let round4_s = make_states(&["dropped_mod", "cascade_dropped"], ModuleState::Installed);
    let drops4 = incremental_graph.extend(&round4_m, &round4_s).unwrap();
    assert_eq!(incremental_graph.len(), 6);
    assert_eq!(drops4.len(), 2);
    assert!(
        drops4
            .get(&ModuleName::new("dropped_mod").unwrap())
            .is_some()
    );
    assert!(
        drops4
            .get(&ModuleName::new("cascade_dropped").unwrap())
            .is_some()
    );
    assert_eq!(
        incremental_graph.module_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
            ModuleName::new("module4").unwrap(),
            ModuleName::new("module5").unwrap(),
        ]
    );

    // 4. In update mode, extend failure does not mutate graph accumulators
    let mut update_graph = ModuleGraph::new(LoadMode::Update);
    let mut base_m = HashMap::new();
    base_m.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    let base_s = make_states(&["base"], ModuleState::Installed);
    update_graph.extend(&base_m, &base_s).unwrap();
    assert_eq!(update_graph.len(), 1);

    let mut fail_m = HashMap::new();
    fail_m.insert(
        ModuleName::new("fail_mod").unwrap(),
        make_manifest("fail_mod", vec!["nonexistent_xyz"]).unwrap(),
    );
    let mut fail_s = HashMap::new();
    fail_s.insert(ModuleName::new("fail_mod").unwrap(), ModuleState::ToInstall);
    let err = update_graph.extend(&fail_m, &fail_s).unwrap_err();
    assert!(matches!(&err, Error::DroppedUpdateModules { modules } if modules.len() == 1));
    assert_eq!(
        err.to_string(),
        "Modules marked to install, upgrade or remove cannot be loaded:\n  - fail_mod: missing dependency 'nonexistent_xyz' (fail_mod -> nonexistent_xyz)"
    );
    assert_eq!(update_graph.len(), 1);

    // After failed extend, fail_mod must not be remembered: extending with a module
    // depending on fail_mod drops it as MissingDependency (fail_mod must not be in manifests)
    let mut retry_m = HashMap::new();
    retry_m.insert(
        ModuleName::new("retry_mod").unwrap(),
        make_manifest("retry_mod", vec!["fail_mod"]).unwrap(),
    );
    let retry_s = make_states(&["retry_mod"], ModuleState::Installed);
    let drops = update_graph.extend(&retry_m, &retry_s).unwrap();
    assert_eq!(update_graph.len(), 1);
    assert!(!update_graph.contains(&ModuleName::new("retry_mod").unwrap()));
    assert_eq!(drops.len(), 1);
    assert_eq!(drops.get(&ModuleName::new("fail_mod").unwrap()), None);
    let drop = drops
        .get(&ModuleName::new("retry_mod").unwrap())
        .expect("retry_mod dropped");
    match drop {
        DropReason::MissingDependency {
            dependency,
            cascade_chain,
        } => {
            assert_eq!(dependency, &ModuleName::new("fail_mod").unwrap());
            assert_eq!(
                cascade_chain,
                &[
                    ModuleName::new("retry_mod").unwrap(),
                    ModuleName::new("fail_mod").unwrap(),
                ]
            );
        }
        other => panic!("expected MissingDependency, got {other:?}"),
    }
}

#[test]
fn test_missing_dependency_drops_module_and_dependents() {
    // Fixture but module4 is missing, and module6 depends on module5
    let mut manifests = HashMap::new();
    manifests.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module1").unwrap(),
        make_manifest("module1", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module2").unwrap(),
        make_manifest("module2", vec!["module1"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module3").unwrap(),
        make_manifest("module3", vec!["module1"]).unwrap(),
    );
    // module4 is intentionally omitted
    manifests.insert(
        ModuleName::new("module5").unwrap(),
        make_manifest("module5", vec!["module2", "module4"]).unwrap(),
    );
    // module6 depends on module5 -> transitive cascade drop
    manifests.insert(
        ModuleName::new("module6").unwrap(),
        make_manifest("module6", vec!["module5"]).unwrap(),
    );

    let states = make_states(
        &[
            "base", "module1", "module2", "module3", "module5", "module6",
        ],
        ModuleState::Installed,
    );

    let (graph, drops) = ModuleGraph::build(&manifests, &states, LoadMode::Load).unwrap();
    assert_eq!(
        graph.module_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
        ]
    );
    assert!(!graph.contains(&ModuleName::new("module5").unwrap()));
    assert!(!graph.contains(&ModuleName::new("module6").unwrap()));

    // Assert DropReport contains cascade chains
    let m5_name = ModuleName::new("module5").unwrap();
    let m6_name = ModuleName::new("module6").unwrap();
    let m4_name = ModuleName::new("module4").unwrap();

    let drop_m5 = drops.get(&m5_name).expect("module5 dropped");
    match drop_m5 {
        DropReason::MissingDependency {
            dependency,
            cascade_chain,
        } => {
            assert_eq!(dependency, &m4_name);
            assert_eq!(cascade_chain, &[m5_name.clone(), m4_name.clone()]);
        }
        other => panic!("expected MissingDependency, got {other:?}"),
    }

    let drop_m6 = drops.get(&m6_name).expect("module6 dropped");
    match drop_m6 {
        DropReason::MissingDependency {
            dependency,
            cascade_chain,
        } => {
            assert_eq!(dependency, &m5_name);
            assert_eq!(
                cascade_chain,
                &[m6_name.clone(), m5_name.clone(), m4_name.clone()]
            );
        }
        other => panic!("expected MissingDependency, got {other:?}"),
    }

    // 2. Fixture of module6 without a manifest, and module7 depends on module6
    let mut m_no_manifest = HashMap::new();
    m_no_manifest.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    m_no_manifest.insert(
        ModuleName::new("module7").unwrap(),
        make_manifest("module7", vec!["module6"]).unwrap(),
    );
    let s_no_manifest = make_states(&["base", "module7"], ModuleState::Installed);
    let (g_no_m, drops_no_m) =
        ModuleGraph::build(&m_no_manifest, &s_no_manifest, LoadMode::Load).unwrap();
    assert_eq!(
        g_no_m.module_names(),
        vec![ModuleName::new("base").unwrap()]
    );
    let m7_name = ModuleName::new("module7").unwrap();
    match drops_no_m.get(&m7_name).expect("module7 dropped") {
        DropReason::MissingDependency {
            dependency,
            cascade_chain,
        } => {
            assert_eq!(dependency, &m6_name);
            assert_eq!(cascade_chain, &[m7_name, m6_name]);
        }
        other => panic!("expected MissingDependency, got {other:?}"),
    }
}

#[test]
fn test_dependency_cycle_drops_the_cycle() {
    // 1. Cycle between 3, 4, 5 and a tail depending on 5
    let mut manifests = HashMap::new();
    manifests.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module1").unwrap(),
        make_manifest("module1", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module2").unwrap(),
        make_manifest("module2", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module3").unwrap(),
        make_manifest("module3", vec!["module1", "module5"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module4").unwrap(),
        make_manifest("module4", vec!["module2", "module3"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module5").unwrap(),
        make_manifest("module5", vec!["module2", "module4"]).unwrap(),
    );
    // tail depends on module5 (cycle member)
    manifests.insert(
        ModuleName::new("tail").unwrap(),
        make_manifest("tail", vec!["module5"]).unwrap(),
    );

    let states = make_states(
        &[
            "base", "module1", "module2", "module3", "module4", "module5", "tail",
        ],
        ModuleState::Installed,
    );

    let (graph, drops) = ModuleGraph::build(&manifests, &states, LoadMode::Load).unwrap();
    // Cycle between 3, 4, 5 and dependent tail are dropped.
    assert_eq!(
        graph.module_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
        ]
    );

    let m3_name = ModuleName::new("module3").unwrap();
    let m4_name = ModuleName::new("module4").unwrap();
    let m5_name = ModuleName::new("module5").unwrap();
    let tail_name = ModuleName::new("tail").unwrap();

    assert!(!graph.contains(&tail_name));

    let drop_m3 = drops.get(&m3_name).expect("module3 dropped");
    match drop_m3 {
        DropReason::DependencyCycle {
            members,
            cascade_chain,
        } => {
            assert_eq!(
                members,
                &[m3_name.clone(), m4_name.clone(), m5_name.clone()]
            );
            assert_eq!(cascade_chain, std::slice::from_ref(&m3_name));
        }
        other => panic!("expected DependencyCycle, got {other:?}"),
    }

    let drop_tail = drops.get(&tail_name).expect("tail dropped");
    match drop_tail {
        DropReason::DependencyCycle {
            members,
            cascade_chain,
        } => {
            assert_eq!(
                members,
                &[m3_name.clone(), m4_name.clone(), m5_name.clone()]
            );
            assert_eq!(
                cascade_chain,
                &[
                    tail_name.clone(),
                    m5_name.clone(),
                    m4_name.clone(),
                    m3_name.clone(),
                ]
            );
        }
        other => panic!("expected DependencyCycle, got {other:?}"),
    }

    // 2. 2-cycle
    let mut cycle2_m = HashMap::new();
    cycle2_m.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    cycle2_m.insert(
        ModuleName::new("a").unwrap(),
        make_manifest("a", vec!["b"]).unwrap(),
    );
    cycle2_m.insert(
        ModuleName::new("b").unwrap(),
        make_manifest("b", vec!["a"]).unwrap(),
    );
    let cycle2_s = make_states(&["base", "a", "b"], ModuleState::Installed);

    let (g2, drops2) = ModuleGraph::build(&cycle2_m, &cycle2_s, LoadMode::Load).unwrap();
    assert_eq!(g2.module_names(), vec![ModuleName::new("base").unwrap()]);
    assert!(drops2.get(&ModuleName::new("a").unwrap()).is_some());
    assert!(drops2.get(&ModuleName::new("b").unwrap()).is_some());

    // 3. Deep cycle with tail
    let mut deep_m = HashMap::new();
    deep_m.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    deep_m.insert(
        ModuleName::new("c1").unwrap(),
        make_manifest("c1", vec!["c5"]).unwrap(),
    );
    deep_m.insert(
        ModuleName::new("c2").unwrap(),
        make_manifest("c2", vec!["c1"]).unwrap(),
    );
    deep_m.insert(
        ModuleName::new("c3").unwrap(),
        make_manifest("c3", vec!["c2"]).unwrap(),
    );
    deep_m.insert(
        ModuleName::new("c4").unwrap(),
        make_manifest("c4", vec!["c3"]).unwrap(),
    );
    deep_m.insert(
        ModuleName::new("c5").unwrap(),
        make_manifest("c5", vec!["c4"]).unwrap(),
    );
    deep_m.insert(
        ModuleName::new("tail1").unwrap(),
        make_manifest("tail1", vec!["c3"]).unwrap(),
    );
    deep_m.insert(
        ModuleName::new("tail2").unwrap(),
        make_manifest("tail2", vec!["tail1"]).unwrap(),
    );

    let deep_s = make_states(
        &["base", "c1", "c2", "c3", "c4", "c5", "tail1", "tail2"],
        ModuleState::Installed,
    );
    let (g_deep, drops_deep) = ModuleGraph::build(&deep_m, &deep_s, LoadMode::Load).unwrap();
    assert_eq!(
        g_deep.module_names(),
        vec![ModuleName::new("base").unwrap()]
    );

    let c1_name = ModuleName::new("c1").unwrap();
    let c2_name = ModuleName::new("c2").unwrap();
    let c3_name = ModuleName::new("c3").unwrap();
    let c4_name = ModuleName::new("c4").unwrap();
    let c5_name = ModuleName::new("c5").unwrap();
    let tail1_name = ModuleName::new("tail1").unwrap();
    let tail2_name = ModuleName::new("tail2").unwrap();
    match drops_deep.get(&c1_name).unwrap() {
        DropReason::DependencyCycle {
            members,
            cascade_chain,
        } => {
            assert_eq!(
                members,
                &[
                    c1_name.clone(),
                    c2_name.clone(),
                    c3_name.clone(),
                    c4_name.clone(),
                    c5_name.clone(),
                ]
            );
            assert_eq!(cascade_chain, std::slice::from_ref(&c1_name));
        }
        other => panic!("expected DependencyCycle, got {other:?}"),
    }
    match drops_deep.get(&tail2_name).unwrap() {
        DropReason::DependencyCycle {
            members,
            cascade_chain,
        } => {
            assert_eq!(
                members,
                &[
                    c1_name.clone(),
                    c2_name.clone(),
                    c3_name.clone(),
                    c4_name.clone(),
                    c5_name.clone(),
                ]
            );
            assert_eq!(
                cascade_chain,
                &[
                    tail2_name.clone(),
                    tail1_name.clone(),
                    c3_name.clone(),
                    c2_name.clone(),
                    c1_name.clone(),
                ]
            );
        }
        other => panic!("expected DependencyCycle, got {other:?}"),
    }

    // 4. Self-loop note:
    // In manifest.rs, self-dependencies are rejected at parse time (Error::SelfDependency).
    // In graph.rs, detect_cycles identifies any single-node SCC with a self-loop (m.depends().contains(m)) as a cycle.
}

#[test]
fn test_test_modules_placed_after_last_dependency() {
    let mut manifests = HashMap::new();
    manifests.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("test_z").unwrap(),
        make_manifest("test_z", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("test_a").unwrap(),
        make_manifest("test_a", vec!["test_z"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module1").unwrap(),
        make_manifest("module1", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("test_c").unwrap(),
        make_manifest("test_c", vec!["module1"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module2").unwrap(),
        make_manifest("module2", vec!["module1"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module3").unwrap(),
        make_manifest("module3", vec!["module1"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module4").unwrap(),
        make_manifest("module4", vec!["module2", "module3"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("test_b").unwrap(),
        make_manifest("test_b", vec!["test_z", "module4"]).unwrap(),
    );

    let states = make_states(
        &[
            "base", "test_z", "test_a", "module1", "test_c", "module2", "module3", "module4",
            "test_b",
        ],
        ModuleState::Installed,
    );

    let (graph, _) = ModuleGraph::build(&manifests, &states, LoadMode::Load).unwrap();
    assert_eq!(
        graph.module_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("test_z").unwrap(),
            ModuleName::new("test_a").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("test_c").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
            ModuleName::new("module4").unwrap(),
            ModuleName::new("test_b").unwrap(),
        ]
    );
}

#[test]
fn test_update_mode_phases_and_two_orders() {
    // Part 1: Standard update fixture
    // base, module1, module2, module3 installed, module4, module6 to upgrade, module5 to install
    let mut manifests = HashMap::new();
    manifests.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module1").unwrap(),
        make_manifest("module1", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module2").unwrap(),
        make_manifest("module2", vec!["module1"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module3").unwrap(),
        make_manifest("module3", vec!["module1"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module4").unwrap(),
        make_manifest("module4", vec!["module2", "module3"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module5").unwrap(),
        make_manifest("module5", vec!["module3"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("module6").unwrap(),
        make_manifest("module6", vec!["module2", "module4"]).unwrap(),
    );

    let mut states = HashMap::new();
    states.insert(ModuleName::new("base").unwrap(), ModuleState::Installed);
    states.insert(ModuleName::new("module1").unwrap(), ModuleState::Installed);
    states.insert(ModuleName::new("module2").unwrap(), ModuleState::Installed);
    states.insert(ModuleName::new("module3").unwrap(), ModuleState::Installed);
    states.insert(ModuleName::new("module4").unwrap(), ModuleState::ToUpgrade);
    states.insert(ModuleName::new("module6").unwrap(), ModuleState::ToUpgrade);
    states.insert(ModuleName::new("module5").unwrap(), ModuleState::ToInstall);

    let (graph, _) = ModuleGraph::build(&manifests, &states, LoadMode::Update).unwrap();

    let base = graph.get(&ModuleName::new("base").unwrap()).unwrap();
    let m1 = graph.get(&ModuleName::new("module1").unwrap()).unwrap();
    let m2 = graph.get(&ModuleName::new("module2").unwrap()).unwrap();
    let m3 = graph.get(&ModuleName::new("module3").unwrap()).unwrap();
    let m4 = graph.get(&ModuleName::new("module4").unwrap()).unwrap();
    let m6 = graph.get(&ModuleName::new("module6").unwrap()).unwrap();
    let m5 = graph.get(&ModuleName::new("module5").unwrap()).unwrap();

    assert_eq!(base.phase(), 0);
    assert_eq!(m1.phase(), 1);
    assert_eq!(m2.phase(), 1);
    assert_eq!(m3.phase(), 1);
    assert_eq!(m4.phase(), 1);
    assert_eq!(m6.phase(), 1);
    assert_eq!(m5.phase(), 2);

    // Assert update phase order
    assert_eq!(
        graph.phase_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
            ModuleName::new("module4").unwrap(),
            ModuleName::new("module6").unwrap(),
            ModuleName::new("module5").unwrap(),
        ]
    );

    // Assert plain-load order (depth, order_name) separates from update phase order
    assert_eq!(
        graph.plain_load_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
            ModuleName::new("module4").unwrap(),
            ModuleName::new("module5").unwrap(),
            ModuleName::new("module6").unwrap(),
        ]
    );

    // Part 2: Corner case
    // Installed module4 gains a new dependency module3 that is to install:
    // updating order is: base (0) | module1, module2 (1) | module3 (2) | module4 (3)
    let mut corner_m = HashMap::new();
    corner_m.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    corner_m.insert(
        ModuleName::new("module1").unwrap(),
        make_manifest("module1", vec!["base"]).unwrap(),
    );
    corner_m.insert(
        ModuleName::new("module2").unwrap(),
        make_manifest("module2", vec!["module1"]).unwrap(),
    );
    corner_m.insert(
        ModuleName::new("module3").unwrap(),
        make_manifest("module3", vec!["module1"]).unwrap(),
    );
    corner_m.insert(
        ModuleName::new("module4").unwrap(),
        make_manifest("module4", vec!["module2", "module3"]).unwrap(),
    );

    let mut corner_s = HashMap::new();
    corner_s.insert(ModuleName::new("base").unwrap(), ModuleState::ToUpgrade);
    corner_s.insert(ModuleName::new("module1").unwrap(), ModuleState::ToUpgrade);
    corner_s.insert(ModuleName::new("module2").unwrap(), ModuleState::ToUpgrade);
    corner_s.insert(ModuleName::new("module3").unwrap(), ModuleState::ToInstall);
    corner_s.insert(ModuleName::new("module4").unwrap(), ModuleState::ToUpgrade);

    let (corner_graph, _) = ModuleGraph::build(&corner_m, &corner_s, LoadMode::Update).unwrap();
    let cb = corner_graph.get(&ModuleName::new("base").unwrap()).unwrap();
    let c1 = corner_graph
        .get(&ModuleName::new("module1").unwrap())
        .unwrap();
    let c2 = corner_graph
        .get(&ModuleName::new("module2").unwrap())
        .unwrap();
    let c3 = corner_graph
        .get(&ModuleName::new("module3").unwrap())
        .unwrap();
    let c4 = corner_graph
        .get(&ModuleName::new("module4").unwrap())
        .unwrap();

    assert_eq!(cb.phase(), 0);
    assert_eq!(c1.phase(), 1);
    assert_eq!(c2.phase(), 1);
    assert_eq!(c3.phase(), 2);
    assert_eq!(c4.phase(), 3);

    assert_eq!(
        corner_graph.phase_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
            ModuleName::new("module4").unwrap(),
        ]
    );

    // Override order stays plain-load order (depth, order_name)
    assert_eq!(
        corner_graph.plain_load_names(),
        vec![
            ModuleName::new("base").unwrap(),
            ModuleName::new("module1").unwrap(),
            ModuleName::new("module2").unwrap(),
            ModuleName::new("module3").unwrap(),
            ModuleName::new("module4").unwrap(),
        ]
    );

    // Part 3: Dropping a module marked to install or upgrade in update mode returns an error
    let mut err_m = HashMap::new();
    err_m.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    err_m.insert(
        ModuleName::new("to_install_bad").unwrap(),
        make_manifest("to_install_bad", vec!["nonexistent_dep"]).unwrap(),
    );
    let mut err_s = HashMap::new();
    err_s.insert(ModuleName::new("base").unwrap(), ModuleState::Installed);
    err_s.insert(
        ModuleName::new("to_install_bad").unwrap(),
        ModuleState::ToInstall,
    );

    let update_err = ModuleGraph::build(&err_m, &err_s, LoadMode::Update).unwrap_err();
    match &update_err {
        Error::DroppedUpdateModules { modules } => {
            assert_eq!(modules.len(), 1);
            assert_eq!(modules[0].0.as_str(), "to_install_bad");
            assert!(matches!(
                &modules[0].1,
                DropReason::MissingDependency { dependency, .. } if dependency.as_str() == "nonexistent_dep"
            ));
        }
        other => panic!("expected DroppedUpdateModules error, got {other:?}"),
    }
    assert_eq!(
        update_err.to_string(),
        "Modules marked to install, upgrade or remove cannot be loaded:\n  - to_install_bad: missing dependency 'nonexistent_dep' (to_install_bad -> nonexistent_dep)"
    );
}

#[test]
fn test_non_installable_and_not_installed_modules_skipped_when_loading() {
    // Given: DB rows x: uninstallable, y: to install, z: uninstalled, w: installed (w depends on x)
    let mut manifests = HashMap::new();
    manifests.insert(
        ModuleName::new("base").unwrap(),
        make_manifest("base", vec![]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("x").unwrap(),
        make_manifest("x", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("y").unwrap(),
        make_manifest("y", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("z").unwrap(),
        make_manifest("z", vec!["base"]).unwrap(),
    );
    manifests.insert(
        ModuleName::new("w").unwrap(),
        make_manifest("w", vec!["x"]).unwrap(),
    );

    // Extra coverage beyond the case's DB states: manifest installable = false
    // is also verified to be dropped with DropReason::NotInstallable.
    let toml_non_inst = r#"
name = "x_non_inst"
installable = false
depends = ["base"]
"#;
    let m_non_inst = Manifest::from_toml("x_non_inst", toml_non_inst)
        .map(Arc::new)
        .unwrap();
    manifests.insert(ModuleName::new("x_non_inst").unwrap(), m_non_inst);

    let mut states = HashMap::new();
    states.insert(ModuleName::new("base").unwrap(), ModuleState::Installed);
    states.insert(ModuleName::new("x").unwrap(), ModuleState::Uninstallable);
    states.insert(ModuleName::new("y").unwrap(), ModuleState::ToInstall);
    states.insert(ModuleName::new("z").unwrap(), ModuleState::Uninstalled);
    states.insert(ModuleName::new("w").unwrap(), ModuleState::Installed);
    states.insert(
        ModuleName::new("x_non_inst").unwrap(),
        ModuleState::Installed,
    );

    // When: the graph is built in load mode
    let (graph_load, drops_load) = ModuleGraph::build(&manifests, &states, LoadMode::Load).unwrap();

    // Then: x, y, z and w are all excluded (w because its dependency was removed)
    assert!(!graph_load.contains(&ModuleName::new("x").unwrap()));
    assert!(!graph_load.contains(&ModuleName::new("y").unwrap()));
    assert!(!graph_load.contains(&ModuleName::new("z").unwrap()));
    assert!(!graph_load.contains(&ModuleName::new("w").unwrap()));
    assert!(!graph_load.contains(&ModuleName::new("x_non_inst").unwrap()));
    assert_eq!(
        graph_load.module_names(),
        vec![ModuleName::new("base").unwrap()]
    );

    // Drop reasons in load mode (typed DropReport):
    // x dropped because state is Uninstallable
    let x_drop = drops_load
        .get(&ModuleName::new("x").unwrap())
        .expect("x dropped");
    assert!(matches!(x_drop, DropReason::NotInstallable));

    // x_non_inst dropped because manifest installable is false (& §5)
    let x_ni_drop = drops_load
        .get(&ModuleName::new("x_non_inst").unwrap())
        .expect("x_non_inst dropped");
    assert!(matches!(x_ni_drop, DropReason::NotInstallable));

    // w dropped because dependency x was removed (cascade drop)
    let w_drop = drops_load
        .get(&ModuleName::new("w").unwrap())
        .expect("w dropped");
    assert!(matches!(
        w_drop,
        DropReason::MissingDependency {
            dependency,
            cascade_chain,
        } if dependency.as_str() == "x"
            && cascade_chain == &[ModuleName::new("w").unwrap(), ModuleName::new("x").unwrap()]
    ));

    // y dropped because state is ToInstall in load mode
    let y_drop = drops_load
        .get(&ModuleName::new("y").unwrap())
        .expect("y dropped");
    assert!(matches!(y_drop, DropReason::NotInstalled));

    // z dropped because state is Uninstalled in load mode
    let z_drop = drops_load
        .get(&ModuleName::new("z").unwrap())
        .expect("z dropped");
    assert!(matches!(z_drop, DropReason::NotInstalled));

    // When: the graph is built in update mode
    let (graph_update, drops_update) =
        ModuleGraph::build(&manifests, &states, LoadMode::Update).unwrap();

    // Then: in update mode, y is kept (present in graph, absent from drops)
    let mod_y = ModuleName::new("y").unwrap();
    let mod_base = ModuleName::new("base").unwrap();
    assert!(graph_update.contains(&mod_y));
    assert!(drops_update.get(&mod_y).is_none());
    assert!(graph_update.contains(&mod_base));
    assert!(drops_update.get(&mod_base).is_none());

    let mod_x = ModuleName::new("x").unwrap();
    let mod_x_non_inst = ModuleName::new("x_non_inst").unwrap();
    let mod_w = ModuleName::new("w").unwrap();
    assert!(!graph_update.contains(&mod_x));
    assert!(!graph_update.contains(&mod_x_non_inst));
    assert!(!graph_update.contains(&mod_w));
    assert!(matches!(
        drops_update.get(&mod_x),
        Some(DropReason::NotInstallable)
    ));
    assert!(matches!(
        drops_update.get(&mod_x_non_inst),
        Some(DropReason::NotInstallable)
    ));
    assert!(matches!(
        drops_update.get(&mod_w),
        Some(DropReason::MissingDependency {
            dependency,
            cascade_chain,
        }) if dependency.as_str() == "x"
            && cascade_chain == &[ModuleName::new("w").unwrap(), ModuleName::new("x").unwrap()]
    ));
}

#[test]
fn test_strict_manifest_schema_and_duplicate_sources() {
    // 1. deny_unknown_fields rejects typo keys
    let typo_toml = r#"
name = "Typo"
typokey = "unknown"
"#;
    let err_typo = Manifest::from_toml("typo", typo_toml).unwrap_err();
    assert!(matches!(err_typo, Error::InvalidManifest { .. }));

    // 2. post_load is rejected citing
    let post_load_toml = r#"
name = "PostLoad"
post_load = "custom_func"
"#;
    let err_post = Manifest::from_toml("post_load_mod", post_load_toml).unwrap_err();
    assert_eq!(
        err_post.to_string(),
        "Module post_load_mod: invalid manifest: post_load is not supported"
    );

    // 3. sequence out of i32 range is rejected
    let seq_overflow_toml = r#"
name = "Seq Overflow"
sequence = 999999999999999999
"#;
    let err_seq = Manifest::from_toml("seq_mod", seq_overflow_toml).unwrap_err();
    assert!(matches!(err_seq, Error::InvalidManifest { .. }));

    // 4. [extra] table is accepted
    let extra_toml = r#"
name = "Extra Test"
[extra]
custom_tool_setting = true
"#;
    let manifest_extra = Manifest::from_toml("extra_test", extra_toml).unwrap();
    assert_eq!(
        manifest_extra
            .extra()
            .get("custom_tool_setting")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    // 5. Duplicate addon registration within the same source or across conflicting sources
    let mut index = AddonIndex::new(false);
    let m_a = Manifest::from_toml("addon_x", "name = 'X'\n").unwrap();
    let m_b = Manifest::from_toml("addon_x", "name = 'X 2'\n").unwrap();

    index
        .add_package(m_a.clone(), "store:addon_x_1.erpaddon")
        .unwrap();
    let dup_err = index
        .add_package(m_b.clone(), "store:addon_x_2.erpaddon")
        .unwrap_err();
    assert!(matches!(dup_err, Error::DuplicateAddon { .. }));

    let cross_err = index.add_embedded(m_b, "embedded:addon_x").unwrap_err();
    assert!(matches!(cross_err, Error::DuplicateAddon { .. }));
}

/// An addon folder whose name is not valid UTF-8 cannot give a module name: discovery names the
/// path instead of reading it as an empty name. Not on macOS: APFS refuses such a name at
/// `create_dir` (EILSEQ), so the case cannot arise there.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn addon_folder_with_a_non_utf8_name_is_a_discovery_error() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let tmp = tempdir().unwrap();
    let bad = tmp.path().join(OsStr::from_bytes(b"bad\xffname"));
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("manifest.toml"), "name = 'Bad'\n").unwrap();

    let mut index = AddonIndex::new(true);
    index.add_dev_path(tmp.path());
    let err = index.discover_all().unwrap_err();
    assert!(
        matches!(&err, Error::DiscoveryError { path, message }
            if *path == bad && message == "the addon directory name is not valid UTF-8"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "Discovery error at '{}': the addon directory name is not valid UTF-8",
            bad.display()
        )
    );
}
