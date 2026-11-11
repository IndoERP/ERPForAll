#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
//! Scheduling rules on the in-memory catalog (MOD §4.2) and the review follow-ups.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use erp_modules::CountryCode;
use erp_modules::discovery::AddonIndex;
use erp_modules::error::{CategoryMember, Error};
use erp_modules::graph::{DropReason, LoadMode, ModuleGraph, ModuleState};
use erp_modules::lifecycle::{
    ActionTarget, ActionType, Catalog, CategoryPath, InstallOptions, MAX_CATEGORY_DEPTH,
    MAX_DEPENDENCY_DEPTH, ModuleInfo, ModuleRow, StoredModule,
};
use erp_modules::manifest::{Manifest, ModuleName};
use erp_modules::version::SemverVersion;
use tempfile::tempdir;

fn n(name: &str) -> ModuleName {
    ModuleName::new(name).unwrap()
}

fn manifest(name: &str, body: &str) -> Arc<Manifest> {
    Arc::new(Manifest::from_toml(name, body).unwrap())
}

fn simple(name: &str, depends: &[&str]) -> Arc<Manifest> {
    let shortdesc = name.to_uppercase();
    manifest(
        name,
        &format!("name = \"{shortdesc}\"\ndepends = {depends:?}\n"),
    )
}

/// The spec fixture: `a` (base), `b` (a), `c` (a), `d` (b, c), `e` (b, d), plus `base`.
fn fixture() -> Vec<Arc<Manifest>> {
    vec![
        simple("base", &[]),
        simple("a", &["base"]),
        simple("b", &["a"]),
        simple("c", &["a"]),
        simple("d", &["b", "c"]),
        simple("e", &["b", "d"]),
    ]
}

fn catalog(manifests: &[Arc<Manifest>], installed: &[&str]) -> Catalog {
    let states: Vec<(&str, ModuleState)> = installed
        .iter()
        .map(|name| (*name, ModuleState::Installed))
        .collect();
    catalog_in(manifests, &states)
}

/// A catalog as loaded from a database: one stored row per manifest, in the given states
/// (`uninstalled`/`uninstallable` otherwise), with the manifests attached.
fn catalog_in(manifests: &[Arc<Manifest>], states: &[(&str, ModuleState)]) -> Catalog {
    let rows = manifests.iter().map(|m| {
        let state = states
            .iter()
            .find(|(name, _)| *name == m.name().as_str())
            .map_or(
                if m.installable() {
                    ModuleState::Uninstalled
                } else {
                    ModuleState::Uninstallable
                },
                |(_, state)| *state,
            );
        let mut stored = StoredModule::from_manifest(m, state).unwrap();
        if matches!(
            state,
            ModuleState::Installed | ModuleState::ToUpgrade | ModuleState::ToRemove
        ) {
            stored.latest_version = Some(SemverVersion::new(1, 0, 0));
        }
        ModuleRow::from_stored(stored).unwrap()
    });
    let mut cat = Catalog::from_rows(rows.collect::<Vec<_>>(), []).unwrap();
    for m in manifests {
        cat.attach_manifest(Arc::clone(m)).unwrap();
    }
    cat
}

fn state(cat: &Catalog, name: &str) -> ModuleState {
    cat.state(name).unwrap()
}

fn states(cat: &Catalog) -> Vec<(String, ModuleState)> {
    cat.rows()
        .map(|r| (r.name().to_string(), r.state()))
        .collect()
}

fn cc(code: &str) -> CountryCode {
    CountryCode::parse(code).unwrap()
}

fn opts() -> InstallOptions {
    InstallOptions::default()
}

#[test]
fn spec_mod_014_skipped_when_loading() {
    let mut manifests = HashMap::new();
    for m in [
        simple("base", &[]),
        manifest("x", "name = 'X'\ninstallable = false\n"),
        simple("y", &["base"]),
        simple("z", &["base"]),
        simple("w", &["x"]),
    ] {
        manifests.insert(m.name().clone(), m);
    }
    let states: HashMap<ModuleName, ModuleState> = [
        ("base", ModuleState::Installed),
        ("x", ModuleState::Uninstallable),
        ("y", ModuleState::ToInstall),
        ("z", ModuleState::Uninstalled),
        ("w", ModuleState::Installed),
    ]
    .into_iter()
    .map(|(k, v)| (n(k), v))
    .collect();

    let (graph, drops) = ModuleGraph::build(&manifests, &states, LoadMode::Load).unwrap();
    assert_eq!(graph.plain_load_names(), vec![n("base")]);
    assert_eq!(drops.get(&n("x")), Some(&DropReason::NotInstallable));
    assert_eq!(drops.get(&n("y")), Some(&DropReason::NotInstalled));
    assert_eq!(drops.get(&n("z")), Some(&DropReason::NotInstalled));
    assert!(matches!(
        drops.get(&n("w")),
        Some(DropReason::MissingDependency { dependency, .. }) if dependency.as_str() == "x"
    ));

    let (graph, drops) = ModuleGraph::build(&manifests, &states, LoadMode::Update).unwrap();
    assert_eq!(graph.plain_load_names(), vec![n("base"), n("y"), n("z")]);
    assert!(drops.get(&n("y")).is_none());
    assert!(drops.get(&n("w")).is_some());
}

/// Load mode: an installed module whose dependency is not installed is an inconsistent database
/// (re-review), every such pair is listed.
#[test]
fn load_mode_installed_module_with_uninstalled_dependency_is_an_error() {
    let mut manifests = HashMap::new();
    for m in [
        simple("base", &[]),
        simple("p", &["base"]),
        simple("q", &["base"]),
        simple("r", &["p", "q"]),
    ] {
        manifests.insert(m.name().clone(), m);
    }
    let states: HashMap<ModuleName, ModuleState> = [
        ("base", ModuleState::Installed),
        ("p", ModuleState::Uninstalled),
        ("q", ModuleState::ToInstall),
        ("r", ModuleState::Installed),
    ]
    .into_iter()
    .map(|(k, v)| (n(k), v))
    .collect();
    let err = ModuleGraph::build(&manifests, &states, LoadMode::Load).unwrap_err();
    match &err {
        Error::InconsistentStates { modules } => {
            let pairs: Vec<(&str, &str, ModuleState)> = modules
                .iter()
                .map(|m| (m.module.as_str(), m.dependency.as_str(), m.dependency_state))
                .collect();
            assert_eq!(
                pairs,
                [
                    ("r", "p", ModuleState::Uninstalled),
                    ("r", "q", ModuleState::ToInstall),
                ]
            );
        }
        other => panic!("expected InconsistentStates, got {other:?}"),
    }
    assert_eq!(
        err.to_string(),
        "Inconsistent module states in the database:\n  - r is installed but its dependency p is 'uninstalled'\n  - r is installed but its dependency q is 'to install'"
    );
}

/// Update mode: all scheduled modules left out are listed, each with its reason.
#[test]
fn update_mode_lists_every_dropped_scheduled_module() {
    let mut manifests = HashMap::new();
    for m in [
        simple("base", &[]),
        simple("u1", &["ghost"]),
        simple("u2", &["u1"]),
        simple("c1", &["c2"]),
        simple("c2", &["c1"]),
    ] {
        manifests.insert(m.name().clone(), m);
    }
    let states: HashMap<ModuleName, ModuleState> = [
        ("base", ModuleState::Installed),
        ("u1", ModuleState::ToInstall),
        ("u2", ModuleState::ToUpgrade),
        ("c1", ModuleState::ToInstall),
        ("c2", ModuleState::Installed),
    ]
    .into_iter()
    .map(|(k, v)| (n(k), v))
    .collect();
    let err = ModuleGraph::build(&manifests, &states, LoadMode::Update).unwrap_err();
    let Error::DroppedUpdateModules { modules } = &err else {
        panic!("expected DroppedUpdateModules, got {err:?}");
    };
    let names: Vec<&str> = modules.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(names, ["c1", "u1", "u2"]);
    assert!(matches!(modules[0].1, DropReason::DependencyCycle { .. }));
    assert!(matches!(
        &modules[2].1,
        DropReason::MissingDependency { dependency, .. } if dependency.as_str() == "u1"
    ));
    let text = err.to_string();
    for (module, reason) in modules {
        assert!(text.contains(&format!("  - {module}: {reason}")), "{text}");
    }
    assert!(
        !text.contains("MissingDependency"),
        "Debug text leaked: {text}"
    );
}

fn write_addon(dir: &std::path::Path, name: &str, body: &str) {
    let d = dir.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("manifest.toml"), body).unwrap();
}

/// `AddonIndex::lookup` reports what `discover_all` reports (re-review).
#[test]
fn lookup_reports_problems_and_invalidates_cache() {
    let mut index = AddonIndex::new(false);
    // invalid name: an error, not "not found"
    let err = index.lookup("Bad-Name").unwrap_err();
    assert!(matches!(err, Error::InvalidModuleName { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        "Invalid module name 'Bad-Name': must contain only ASCII alphanumeric characters or underscores"
    );

    // duplicate dev folders
    let one = tempdir().unwrap();
    let two = tempdir().unwrap();
    write_addon(one.path(), "dup", "name = 'Dup 1'\n");
    write_addon(two.path(), "dup", "name = 'Dup 2'\n");
    let mut dev = AddonIndex::new(true);
    dev.add_dev_path(one.path());
    dev.add_dev_path(two.path());
    let err = dev.lookup("dup").unwrap_err();
    match &err {
        Error::DuplicateAddon {
            name,
            source_a,
            source_b,
        } => {
            assert_eq!(name.as_str(), "dup");
            assert_eq!(source_a, &one.path().join("dup").display().to_string());
            assert_eq!(source_b, &two.path().join("dup").display().to_string());
        }
        other => panic!("expected DuplicateAddon, got {other:?}"),
    }
    assert_eq!(
        err.to_string(),
        format!(
            "Duplicate addon 'dup' found from multiple sources: '{}' and '{}'",
            one.path().join("dup").display(),
            two.path().join("dup").display()
        )
    );

    // a configured dev path that does not exist
    let mut missing = AddonIndex::new(true);
    let nope = one.path().join("nope");
    missing.add_dev_path(&nope);
    let err = missing.lookup("dup").unwrap_err();
    assert!(matches!(&err, Error::DiscoveryError { path, .. } if path == &nope));
    assert_eq!(
        err.to_string(),
        format!(
            "Discovery error at '{0}': configured dev path does not exist: {0}",
            nope.display()
        )
    );
    // dev folders are not inspected outside --dev mode
    let mut prod = AddonIndex::new(false);
    prod.add_dev_path(one.path());
    assert!(prod.lookup("dup").unwrap().is_none());

    // cache invalidated when sources change
    let mut idx = AddonIndex::new(true);
    assert!(idx.lookup("late").unwrap().is_none());
    let three = tempdir().unwrap();
    write_addon(three.path(), "late", "name = 'Late'\n");
    idx.add_dev_path(three.path());
    let first = idx.lookup("late").unwrap().unwrap();
    assert_eq!(first.shortdesc(), "Late");
    fs::write(three.path().join("late/manifest.toml"), "name = 'Later'\n").unwrap();
    // cached until invalidated
    assert!(Arc::ptr_eq(&first, &idx.lookup("late").unwrap().unwrap()));
    idx.invalidate_cache();
    assert_eq!(idx.lookup("late").unwrap().unwrap().shortdesc(), "Later");
    // registering a source clears the cache too
    let cached = idx.lookup("late").unwrap().unwrap();
    idx.add_embedded(
        Manifest::from_toml("other", "name = 'Other'\n").unwrap(),
        "embedded:other",
    )
    .unwrap();
    assert!(!Arc::ptr_eq(&cached, &idx.lookup("late").unwrap().unwrap()));
}

/// `system_specs`)
#[test]
fn spec_mod_016_category_chain_names() {
    let m = manifest(
        "crm_tool",
        "name = 'Tool'\ncategory = 'Sales & CRM/Tools'\n",
    );
    let path = CategoryPath::parse(m.category()).unwrap();
    let chain = path.chain();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].segments(), ["Sales & CRM"]);
    assert_eq!(chain[0].xml_id_name(), "module_category_sales_and_crm");
    assert_eq!(chain[1].name(), "Tools");
    assert_eq!(
        chain[1].xml_id_name(),
        "module_category_sales_and_crm_tools"
    );
    let cat = catalog(&[simple("base", &[]), m], &["base"]);
    assert_eq!(cat.get("crm_tool").unwrap().category(), &path);
}

#[test]
fn spec_mod_017_button_install_marks_dependencies() {
    let mut cat = catalog(&fixture(), &["base"]);
    let action = cat.button_install(&[n("d")], &opts()).unwrap();
    for m in ["a", "b", "c", "d"] {
        assert_eq!(state(&cat, m), ModuleState::ToInstall, "{m}");
    }
    assert_eq!(state(&cat, "e"), ModuleState::Uninstalled);
    assert_eq!(state(&cat, "base"), ModuleState::Installed);
    assert_eq!(action.action_type, ActionType::ActWindow);
    assert_eq!(action.action_type.as_str(), "ir.actions.act_window");
    assert_eq!(action.res_model, "base.module.upgrade");
    assert_eq!(action.target, ActionTarget::New);
    assert_eq!(action.target.as_str(), "new");
    assert_eq!(action.view_mode, "form");
    assert_eq!(action.name, "Install");
    let changed: Vec<String> = cat
        .state_changes()
        .into_iter()
        .map(|(m, _)| m.to_string())
        .collect();
    assert_eq!(changed, ["a", "b", "c", "d"]);
}

#[test]
fn spec_mod_018_unknown_dependency() {
    let mut cat = catalog(&[simple("base", &[]), simple("x", &["ghost"])], &["base"]);
    let before = states(&cat);
    let err = cat.button_install(&[n("x")], &opts()).unwrap_err();
    assert!(
        matches!(&err, Error::UnknownDependency { module, dependency }
        if module.as_str() == "x" && dependency.as_str() == "ghost")
    );
    assert_eq!(
        err.to_string(),
        "You try to install module \"x\" that depends on module \"ghost\".\nBut the latter module is not available in your system."
    );
    assert_eq!(states(&cat), before);
}

#[test]
fn spec_mod_019_auto_install_glue_module() {
    let mut ms = fixture();
    ms.push(manifest(
        "glue",
        "name = 'Glue'\ndepends = ['a', 'c']\nauto_install = true\n",
    ));
    let mut cat = catalog(&ms, &["base", "a"]);
    cat.button_install(&[n("c")], &opts()).unwrap();
    assert_eq!(state(&cat, "c"), ModuleState::ToInstall);
    assert_eq!(state(&cat, "glue"), ModuleState::ToInstall);

    // auto_install on 'c' only: a non-trigger dependency 'z' is installed, not required
    let mut ms = fixture();
    ms.push(simple("z", &["base"]));
    ms.push(manifest(
        "glue",
        "name = 'Glue'\ndepends = ['a', 'c', 'z']\nauto_install = ['c']\n",
    ));
    let mut cat = catalog(&ms, &["base", "a"]);
    cat.button_install(&[n("c")], &opts()).unwrap();
    assert_eq!(state(&cat, "glue"), ModuleState::ToInstall);
    assert_eq!(state(&cat, "z"), ModuleState::ToInstall);

    // skip_auto_install
    let mut cat = catalog(&ms, &["base", "a"]);
    cat.button_install(&[n("c")], &opts().with_skip_auto_install(true))
        .unwrap();
    assert_eq!(state(&cat, "glue"), ModuleState::Uninstalled);
}

/// from res.company arrives with M3, until then the module system passes none and a
/// country-restricted auto-install is an error)
#[test]
fn spec_mod_020_auto_install_needs_new_trigger_and_country() {
    let mut ms = fixture();
    ms.push(simple("u", &["base"]));
    ms.push(manifest(
        "glue",
        "name = 'Glue'\ndepends = ['a']\nauto_install = true\n",
    ));
    ms.push(manifest(
        "l10n_x",
        "name = 'L10n X'\ndepends = ['a']\nauto_install = true\ncountries = ['be']\n",
    ));
    // no company in Belgium
    let none = opts().with_company_countries(Vec::<CountryCode>::new());
    let mut cat = catalog(&ms, &["base", "a"]);
    cat.button_install(&[n("u")], &none).unwrap();
    assert_eq!(state(&cat, "u"), ModuleState::ToInstall);
    assert_eq!(state(&cat, "glue"), ModuleState::Uninstalled);
    assert_eq!(state(&cat, "l10n_x"), ModuleState::Uninstalled);

    // with `a` itself to install: glue follows, l10n_x only with a Belgian company
    let mut cat = catalog(&ms, &["base"]);
    cat.button_install(&[n("a")], &none).unwrap();
    assert_eq!(state(&cat, "glue"), ModuleState::ToInstall);
    assert_eq!(state(&cat, "l10n_x"), ModuleState::Uninstalled);
    let mut cat = catalog(&ms, &["base"]);
    let be = opts().with_company_countries([cc("be")]);
    cat.button_install(&[n("a")], &be).unwrap();
    assert_eq!(state(&cat, "l10n_x"), ModuleState::ToInstall);

    // company countries not known (lead review R): the country-restricted module is an error,
    // not skipped as if no company matched; nothing is scheduled
    let mut cat = catalog(&ms, &["base"]);
    let before = states(&cat);
    let err = cat.button_install(&[n("a")], &opts()).unwrap_err();
    assert!(
        matches!(&err, Error::CompanyCountriesUnknown { module, countries }
            if module.as_str() == "l10n_x" && *countries == [cc("BE")]),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module \"l10n_x\" auto-installs only for companies in BE, but the companies' countries \
         are not known until res.company arrives (M3); install \"l10n_x\" by name in the same \
         operation to include it"
    );
    assert_eq!(states(&cat), before);
    // the remedy the message gives works through every entry point (button_immediate_install
    // takes no options): named, the module is no auto-install candidate
    let mut cat = catalog(&ms, &["base"]);
    cat.button_install(&[n("a"), n("l10n_x")], &opts()).unwrap();
    assert_eq!(state(&cat, "a"), ModuleState::ToInstall);
    assert_eq!(state(&cat, "l10n_x"), ModuleState::ToInstall);
    // a trigger that does not fire needs no countries
    let mut cat = catalog(&ms, &["base", "a"]);
    cat.button_install(&[n("u")], &opts()).unwrap();
    assert_eq!(state(&cat, "l10n_x"), ModuleState::Uninstalled);
}

#[test]
fn spec_mod_021_exclusions_and_exclusive_categories() {
    let ms = vec![
        simple("base", &[]),
        manifest("p", "name = 'P'\nexcludes = ['q']\n"),
        manifest("q", "name = 'Q'\n"),
    ];
    let mut cat = catalog(&ms, &["base", "q"]);
    let before = states(&cat);
    let err = cat.button_install(&[n("p")], &opts()).unwrap_err();
    assert!(
        matches!(&err, Error::IncompatibleModules { module, other, .. }
            if module.as_str() == "p" && other.as_str() == "q"),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "Modules \"P\" and \"Q\" are incompatible.");
    assert_eq!(states(&cat), before, "a refused install changes nothing");

    let ms = vec![
        simple("base", &[]),
        manifest("k1", "name = 'K1'\ncategory = 'K'\n"),
        manifest("k2", "name = 'K2'\ncategory = 'K/Sub'\n"),
    ];
    let mut cat = catalog(&ms, &["base", "k1"]);
    cat.set_exclusive(CategoryPath::parse("K").unwrap(), true);
    let err = cat.button_install(&[n("k2")], &opts()).unwrap_err();
    assert!(
        matches!(&err, Error::ExclusiveCategory { category, modules }
            if *category == CategoryPath::parse("K").unwrap()
                && modules.iter().map(|m: &CategoryMember| (m.module.as_str(), m.state)).collect::<Vec<_>>()
                    == [("k1", ModuleState::Installed), ("k2", ModuleState::ToInstall)]),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "You are trying to install incompatible modules in category \"K\":\n- K1 ({})\n- K2 ({})",
            ModuleState::Installed.label(),
            ModuleState::ToInstall.label()
        )
    );
    assert_eq!(state(&cat, "k2"), ModuleState::Uninstalled);

    // k2 depending on k1 is fine: all members are in k2's closure
    let ms = vec![
        simple("base", &[]),
        manifest("k1", "name = 'K1'\ncategory = 'K'\n"),
        manifest("k2", "name = 'K2'\ncategory = 'K'\ndepends = ['k1']\n"),
    ];
    let mut cat = catalog(&ms, &["base", "k1"]);
    cat.set_exclusive(CategoryPath::parse("K").unwrap(), true);
    cat.button_install(&[n("k2")], &opts()).unwrap();
    assert_eq!(state(&cat, "k2"), ModuleState::ToInstall);
}

#[test]
fn spec_mod_023_button_upgrade_closure_and_new_dependencies() {
    let mut cat = catalog(&fixture(), &["base", "a", "b", "c", "d", "e"]);
    let mut ms = fixture();
    ms[2] = simple("b", &["a", "n"]);
    ms.push(simple("n", &["base"]));
    cat.update_list(&ms).unwrap();
    let action = cat.button_upgrade(&[n("b")], &opts()).unwrap();
    assert_eq!(action.name, "Apply Schedule Upgrade");
    for m in ["b", "d", "e"] {
        assert_eq!(state(&cat, m), ModuleState::ToUpgrade, "{m}");
    }
    for m in ["a", "c", "base"] {
        assert_eq!(state(&cat, m), ModuleState::Installed, "{m}");
    }
    assert_eq!(state(&cat, "n"), ModuleState::ToInstall);
}

#[test]
fn spec_mod_024_upgrade_not_installed() {
    let mut cat = catalog(&[simple("base", &[]), simple("x", &["base"])], &["base"]);
    let err = cat.button_upgrade(&[n("x")], &opts()).unwrap_err();
    assert!(matches!(&err, Error::UpgradeNotInstalled { module } if module.as_str() == "x"));
    assert_eq!(
        err.to_string(),
        "Cannot upgrade module \u{201c}x\u{201d}. It is not installed."
    );
    assert_eq!(state(&cat, "x"), ModuleState::Uninstalled);
}

#[test]
fn spec_mod_025_upgrading_base_upgrades_every_installed_module() {
    let mut ms = fixture();
    ms.push(simple("studio_customization", &["base"]));
    let mut cat = catalog(&ms, &["base", "a", "c", "studio_customization"]);
    cat.button_upgrade(&[n("base")], &opts()).unwrap();
    for m in ["base", "a", "c"] {
        assert_eq!(state(&cat, m), ModuleState::ToUpgrade, "{m}");
    }
    assert_eq!(state(&cat, "studio_customization"), ModuleState::Installed);
    assert_eq!(state(&cat, "b"), ModuleState::Uninstalled);
}

#[test]
fn spec_mod_026_button_uninstall_rules() {
    let mut ms = fixture();
    ms.push(simple("rpc", &["base"]));
    ms.push(simple("x", &["base"]));
    let all = ["base", "a", "b", "c", "d", "e", "rpc"];
    let mut cat = catalog(&ms, &all);
    let action = cat.button_uninstall(&[n("b")]).unwrap();
    assert_eq!(action.name, "Uninstall");
    for m in ["b", "d", "e"] {
        assert_eq!(state(&cat, m), ModuleState::ToRemove, "{m}");
    }
    for m in ["base", "a", "c"] {
        assert_eq!(state(&cat, m), ModuleState::Installed, "{m}");
    }

    let mut cat = catalog(&ms, &all);
    for (module, listed) in [("base", "base"), ("rpc", "rpc")] {
        let err = cat.button_uninstall(&[n(module)]).unwrap_err();
        assert!(matches!(&err, Error::CannotUninstall { modules } if modules == &vec![n(listed)]));
        assert_eq!(
            err.to_string(),
            format!("Those modules cannot be uninstalled: {listed}")
        );
    }
    let err = cat.button_uninstall(&[n("x")]).unwrap_err();
    assert!(matches!(err, Error::AlreadyUninstalled));
    assert_eq!(
        err.to_string(),
        "One or more of the selected modules have already been uninstalled, if you believe this to be an error, you may try again later or contact support."
    );
    // several protected modules are all listed, sorted
    let err = cat
        .button_uninstall(&[n("rpc"), n("a"), n("base")])
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Those modules cannot be uninstalled: base, rpc"
    );
    assert!(cat.state_changes().is_empty());
}

/// `system_specs`)
#[test]
fn spec_mod_041_reset_transient_states() {
    let mut cat = catalog_in(
        &fixture(),
        &[
            ("base", ModuleState::Installed),
            ("a", ModuleState::Installed),
            ("b", ModuleState::ToUpgrade),
            ("c", ModuleState::ToRemove),
            ("d", ModuleState::ToInstall),
        ],
    );
    let mut reset = cat.reset_state();
    reset.sort();
    assert_eq!(reset, vec![n("b"), n("c"), n("d")]);
    assert_eq!(state(&cat, "d"), ModuleState::Uninstalled);
    assert_eq!(state(&cat, "b"), ModuleState::Installed);
    assert_eq!(state(&cat, "c"), ModuleState::Installed);
    assert_eq!(state(&cat, "e"), ModuleState::Uninstalled);
}

#[test]
fn spec_mod_046_update_list_counts_and_syncs() {
    let mut ms = fixture();
    ms.push(simple("z", &["base"]));
    ms.push(simple("x", &["a", "c"]));
    let mut cat = catalog(&ms, &["base", "a", "c", "x"]);

    let mut ms2: Vec<Arc<Manifest>> = ms
        .iter()
        .filter(|m| m.name().as_str() != "x")
        .cloned()
        .collect();
    ms2.push(manifest(
        "x",
        "name = 'X'\nversion = '1.1'\ndepends = ['a', 'z']\n",
    ));
    ms2.push(simple("new_mod", &["base"]));
    let result = cat.update_list(&ms2).unwrap();
    assert_eq!((result.upgradable, result.added), (1, 1));
    assert_eq!(state(&cat, "new_mod"), ModuleState::Uninstalled);
    let deps: Vec<&str> = cat
        .get("x")
        .unwrap()
        .depends()
        .iter()
        .map(|d| d.name().as_str())
        .collect();
    assert_eq!(deps, ["a", "z"]);
    assert_eq!(state(&cat, "x"), ModuleState::Installed);
    let added: Vec<&str> = cat.added().map(|r| r.name().as_str()).collect();
    assert_eq!(added, ["new_mod"]);
    let dirty: Vec<&str> = cat.dirty().map(|r| r.name().as_str()).collect();
    assert_eq!(dirty, ["x"]);

    // uninstallable → uninstalled when installable again; never the reverse
    let mut cat = catalog(
        &[
            simple("base", &[]),
            manifest("u", "name='U'\ninstallable=false\n"),
        ],
        &["base"],
    );
    assert_eq!(state(&cat, "u"), ModuleState::Uninstallable);
    cat.update_list(&[simple("base", &[]), simple("u", &["base"])])
        .unwrap();
    assert_eq!(state(&cat, "u"), ModuleState::Uninstalled);
    // a module in no source keeps its row, without a manifest
    cat.update_list(&[simple("base", &[])]).unwrap();
    assert!(cat.get("u").unwrap().manifest().is_none());
}

/// Edge cases of `button_install`: dependency cycles, the nesting limit, refused requests.
#[test]
fn button_install_cycles_depth_and_refusals() {
    let ms = vec![
        simple("base", &[]),
        simple("c1", &["c2"]),
        simple("c2", &["c1"]),
    ];
    let mut cat = catalog(&ms, &["base"]);
    let err = cat.button_install(&[n("c1")], &opts()).unwrap_err();
    assert!(matches!(err, Error::DependencyRecursion));
    assert_eq!(err.to_string(), "Recursion error in modules dependencies!");
    assert!(
        cat.state_changes().is_empty(),
        "a refused install changes nothing"
    );

    // a chain m0 -> m1 -> ... -> base: the installed `base` is visited too, so the limit counts
    // every level down to it (`_state_update` checks the level before the state)
    let chain = |len: usize| -> Vec<Arc<Manifest>> {
        let mut ms = vec![simple("base", &[])];
        for i in 0..len {
            let dep = if i + 1 == len {
                "base".to_owned()
            } else {
                format!("m{}", i + 1)
            };
            ms.push(simple(&format!("m{i}"), &[dep.as_str()]));
        }
        ms
    };
    let ok_len = MAX_DEPENDENCY_DEPTH - 1;
    let mut cat = catalog(&chain(ok_len), &["base"]);
    cat.button_install(&[n("m0")], &opts()).unwrap();
    assert_eq!(
        state(&cat, &format!("m{}", ok_len - 1)),
        ModuleState::ToInstall
    );
    let mut cat = catalog(&chain(MAX_DEPENDENCY_DEPTH), &["base"]);
    let err = cat.button_install(&[n("m0")], &opts()).unwrap_err();
    assert!(matches!(err, Error::DependencyRecursion));
    assert!(cat.state_changes().is_empty());

    // requested module unknown to the catalog: all unknown names listed
    let mut cat = catalog(&fixture(), &["base"]);
    let err = cat
        .button_install(&[n("ghost"), n("a"), n("phantom")], &opts())
        .unwrap_err();
    assert!(
        matches!(&err, Error::UnknownModule { names } if names == &vec![n("ghost"), n("phantom")])
    );
    assert_eq!(err.to_string(), "Unknown module(s): ghost, phantom");

    let ms = vec![
        simple("base", &[]),
        manifest("u", "name = 'U'\ninstallable = false\n"),
        simple("v", &["u"]),
    ];
    let mut cat = catalog(&ms, &["base"]);
    let err = cat.button_install(&[n("u")], &opts()).unwrap_err();
    assert!(matches!(&err, Error::ModuleNotInstallable { module } if module.as_str() == "u"));
    assert_eq!(err.to_string(), "Module \"u\" is not installable.");
    let err = cat.button_install(&[n("v")], &opts()).unwrap_err();
    assert!(
        matches!(&err, Error::DependencyNotInstallable { module, dependency }
        if module.as_str() == "v" && dependency.as_str() == "u")
    );
    assert_eq!(
        err.to_string(),
        "You try to install module \"v\" that depends on module \"u\", which is not installable."
    );
    assert!(cat.state_changes().is_empty());

    // a row whose addon is in no source
    let mut cat = catalog(&fixture(), &["base"]);
    cat.update_list(&fixture()[..2]).unwrap();
    let err = cat.button_install(&[n("b")], &opts()).unwrap_err();
    assert!(matches!(&err, Error::ModuleUnavailable { module } if module.as_str() == "b"));
    assert_eq!(
        err.to_string(),
        "Module \"b\" is not available in any addon source."
    );

    // empty request: nothing changes; repeated and duplicated requests are idempotent
    let mut cat = catalog(&fixture(), &["base"]);
    assert_eq!(cat.button_install(&[], &opts()).unwrap().name, "Install");
    assert!(cat.state_changes().is_empty());
    cat.button_install(&[n("b"), n("b")], &opts()).unwrap();
    let first = states(&cat);
    cat.button_install(&[n("b")], &opts()).unwrap();
    assert_eq!(states(&cat), first);
    assert_eq!(state(&cat, "b"), ModuleState::ToInstall);
}

/// `button_upgrade` refusals: unknown new dependency, missing external binary.
#[test]
fn button_upgrade_refusals() {
    let mut cat = catalog(&fixture(), &["base", "a", "b"]);
    let mut ms = fixture();
    ms[2] = simple("b", &["a", "ghost"]);
    cat.update_list(&ms).unwrap();
    let before = states(&cat);
    let err = cat.button_upgrade(&[n("b")], &opts()).unwrap_err();
    assert!(
        matches!(&err, Error::UpgradeUnknownDependency { module, dependency }
        if module.as_str() == "b" && dependency.as_str() == "ghost")
    );
    assert_eq!(
        err.to_string(),
        "You try to upgrade the module b that depends on the module: ghost.\nBut this module is not available in your system."
    );
    assert_eq!(states(&cat), before, "a refused upgrade changes nothing");

    let mut cat = catalog(&fixture(), &["base", "a"]);
    let mut ms = fixture();
    ms[1] = manifest(
        "a",
        "name = 'A'\n[external_dependencies]\nbin = ['erpforall-no-such-binary']\n",
    );
    cat.update_list(&ms).unwrap();
    let err = cat.button_upgrade(&[n("a")], &opts()).unwrap_err();
    assert!(
        matches!(&err, Error::ExternalDependencyMissing { operation, .. } if *operation == erp_modules::ModuleOperation::Upgrade)
    );
    assert_eq!(
        err.to_string(),
        "Unable to upgrade module \"a\" because an external dependency is not met: erpforall-no-such-binary"
    );
}

/// Catalog construction refuses duplicates and stored `unavailable`; category paths are validated.
#[test]
fn catalog_and_category_validation() {
    let row = ModuleRow::from_manifest(simple("base", &[])).unwrap();
    let err = Catalog::from_rows([row.clone(), row], []).unwrap_err();
    assert!(matches!(&err, Error::DuplicateModuleRow { module } if module.as_str() == "base"));
    assert_eq!(err.to_string(), "Module \"base\" is listed twice");

    let mut cat = Catalog::new();
    let err = cat
        .update_list(&[simple("base", &[]), simple("base", &[])])
        .unwrap_err();
    assert_eq!(err.to_string(), "Module \"base\" is listed twice");
    assert_eq!(cat.rows().count(), 0, "a refused list changes nothing");

    let info = ModuleInfo::from_manifest(&simple("x", &[]));
    let stored = StoredModule::new(
        n("x"),
        ModuleState::Unavailable,
        info,
        CategoryPath::parse("Uncategorized").unwrap(),
    );
    let err = ModuleRow::from_stored(stored).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Module \"x\": the state 'unavailable' is never stored"
    );

    for (input, message) in [
        ("", "Invalid module category '': the category is empty"),
        (
            "A//B",
            "Invalid module category 'A//B': a category segment is empty",
        ),
        (
            "A/",
            "Invalid module category 'A/': a category segment is empty",
        ),
    ] {
        let err = CategoryPath::parse(input).unwrap_err();
        assert!(matches!(err, Error::InvalidCategory { .. }));
        assert_eq!(err.to_string(), message);
    }
    let deep = vec!["c"; MAX_CATEGORY_DEPTH + 1].join("/");
    let err = CategoryPath::parse(&deep).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Invalid module category '{deep}': more than {MAX_CATEGORY_DEPTH} nested categories"
        )
    );
    assert!(CategoryPath::parse(&vec!["c"; MAX_CATEGORY_DEPTH].join("/")).is_ok());
    // the manifest refuses a bad category when it is read
    let err =
        Manifest::from_toml("bad_cat", "name = 'B'\ncategory = 'Sales//Tools'\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Module bad_cat: invalid manifest: Invalid module category 'Sales//Tools': a category segment is empty"
    );
    let path = CategoryPath::parse("Ventes & Café/Outils Été").unwrap();
    assert_eq!(
        path.xml_id_name(),
        "module_category_ventes_and_café_outils_été"
    );
}

/// Review H3: in update mode a module marked `to remove` that falls out of the graph is an error
/// (it would stay `to remove` while the operation reports success).
#[test]
fn update_mode_dropped_to_remove_module_is_an_error() {
    let mut manifests = HashMap::new();
    for m in [
        simple("base", &[]),
        simple("p", &["base"]),
        simple("r", &["p", "ghost"]),
    ] {
        manifests.insert(m.name().clone(), m);
    }
    let states: HashMap<ModuleName, ModuleState> = [
        ("base", ModuleState::Installed),
        ("p", ModuleState::Installed),
        ("r", ModuleState::ToRemove),
    ]
    .into_iter()
    .map(|(k, v)| (n(k), v))
    .collect();
    let err = ModuleGraph::build(&manifests, &states, LoadMode::Update).unwrap_err();
    assert!(
        matches!(&err, Error::DroppedUpdateModules { modules } if modules.len() == 1),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Modules marked to install, upgrade or remove cannot be loaded:\n  - r: missing dependency 'ghost' (r -> ghost)"
    );
}

/// Review H3: `button_uninstall` refuses a module (asked for or pulled in as a dependent) that no
/// addon source provides or whose manifest is not installable: the runner could not remove its
/// tables.
#[test]
fn button_uninstall_refuses_modules_without_an_installable_source() {
    let mut cat = catalog(&fixture(), &["base", "a", "b", "c"]);
    // c's source vanished
    let without_c: Vec<Arc<Manifest>> = fixture()
        .into_iter()
        .filter(|m| m.name().as_str() != "c")
        .collect();
    cat.update_list(&without_c).unwrap();
    for asked in [n("c"), n("a")] {
        let err = cat
            .button_uninstall(std::slice::from_ref(&asked))
            .unwrap_err();
        assert!(
            matches!(&err, Error::UninstallUnavailable { module } if module.as_str() == "c"),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "Module \"c\" cannot be uninstalled: it is not available in any addon source (restore its source first)"
        );
    }
    assert!(cat.state_changes().is_empty());
    // b's manifest says installable = false
    let mut ms = fixture();
    ms[2] = manifest("b", "name = 'B'\ndepends = ['a']\ninstallable = false\n");
    let mut cat = catalog(&ms, &["base", "a", "b"]);
    let err = cat.button_uninstall(&[n("b")]).unwrap_err();
    assert!(
        matches!(&err, Error::UninstallNotInstallable { module } if module.as_str() == "b"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module \"b\" cannot be uninstalled: its manifest says installable = false"
    );
    assert!(cat.state_changes().is_empty());
}

// ---- integration review follow-ups (B1..B6) ------------------------------------------------

/// B2: attaching a manifest to a module the catalog does not know is an error, not a no-op.
#[test]
fn attach_manifest_of_unknown_module_is_an_error() {
    let mut cat = catalog(&fixture(), &["base"]);
    let err = cat.attach_manifest(simple("ghost", &["base"])).unwrap_err();
    assert!(
        matches!(&err, Error::UnknownModule { names } if names == &[n("ghost")]),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "Unknown module(s): ghost");
    assert!(cat.get("ghost").is_none());
}

/// B2: the upstream closure holds module names, the module itself included.
#[test]
fn upstream_closure_is_typed_and_includes_the_module() {
    let cat = catalog(&fixture(), &["base"]);
    let closure = cat.upstream_closure(&n("d"));
    let names: Vec<&str> = closure.iter().map(ModuleName::as_str).collect();
    assert_eq!(names, ["a", "b", "base", "c", "d"]);
}

/// B3: a stored row that excludes itself is refused.
#[test]
fn stored_row_that_excludes_itself_is_refused() {
    let m = simple("x", &[]);
    let mut stored = StoredModule::from_manifest(&m, ModuleState::Uninstalled).unwrap();
    stored.excludes.insert(n("x"));
    let err = ModuleRow::from_stored(stored).unwrap_err();
    assert!(
        matches!(&err, Error::SelfExclusion { module } if module.as_str() == "x"),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "Module 'x' excludes itself");
}

#[test]
fn stored_uninstalled_row_with_a_version_is_refused() {
    let m = simple("x", &[]);
    let mut stored = StoredModule::from_manifest(&m, ModuleState::Uninstalled).unwrap();
    stored.latest_version = Some(SemverVersion::new(1, 2, 3));
    let err = ModuleRow::from_stored(stored.clone()).unwrap_err();
    assert!(
        matches!(&err, Error::InvalidModuleRow { module, .. } if module.as_str() == "x"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module \"x\": inconsistent stored row: state is 'uninstalled' but an installed version is stored"
    );
    stored.state = ModuleState::Installed;
    let row = ModuleRow::from_stored(stored).unwrap();
    assert_eq!(row.latest_version(), Some(SemverVersion::new(1, 2, 3)));
}

/// B4: country codes are two ASCII letters, normalized upper case, wherever they come from.
#[test]
fn country_codes_are_validated_and_normalized() {
    assert_eq!(cc("be").as_str(), "BE");
    assert_eq!(cc("bE"), cc("Be"));
    for bad in ["", "B", "BEL", "B1", "b ", " be", "é", "éa", "../"] {
        let err = CountryCode::parse(bad).unwrap_err();
        assert!(
            matches!(&err, Error::InvalidCountryCode { code } if code == bad),
            "{bad:?}: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "Invalid country code '{bad}': expected two ASCII letters (ISO 3166-1 alpha-2)"
            )
        );
    }
    // a manifest with a bad code is refused when it is read
    let err = Manifest::from_toml("bad_cc", "name = 'B'\ncountries = ['be', '../escaped']\n")
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Module bad_cc: invalid manifest: Invalid country code '../escaped': expected two ASCII letters (ISO 3166-1 alpha-2)"
    );
    // and a good one is stored normalized, on the manifest and on the row
    let m = manifest("good_cc", "name = 'G'\ncountries = ['be', 'Fr']\n");
    assert_eq!(m.countries(), [cc("BE"), cc("FR")]);
    let row = ModuleRow::from_manifest(Arc::clone(&m)).unwrap();
    assert!(row.countries().contains(&cc("fr")));
}

#[test]
fn category_segments_refuse_slash_and_padding() {
    for (segments, message) in [
        (
            vec!["A/B"],
            "Invalid module category 'A/B': a category segment contains '/'",
        ),
        (
            vec!["A", " B"],
            "Invalid module category 'A/ B': a category segment has leading or trailing whitespace",
        ),
        (
            vec!["A\t"],
            "Invalid module category 'A\t': a category segment has leading or trailing whitespace",
        ),
    ] {
        let err = CategoryPath::from_segments(segments.into_iter().map(str::to_owned).collect())
            .unwrap_err();
        assert!(matches!(err, Error::InvalidCategory { .. }), "{err:?}");
        assert_eq!(err.to_string(), message);
    }
    let err = CategoryPath::parse("Sales / Tools").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Invalid module category 'Sales / Tools': a category segment has leading or trailing whitespace"
    );
    let path = CategoryPath::parse("Sales & CRM/Tools").unwrap();
    assert_eq!(path.to_string(), "Sales & CRM/Tools");
    assert_eq!(
        CategoryPath::parse(&path.to_string()).unwrap(),
        path,
        "Display and parse round-trip"
    );
}

#[test]
fn update_list_refuses_categories_sharing_an_external_id() {
    let a_b = CategoryPath::parse("A/B").unwrap();
    let a_space_b = CategoryPath::parse("A B").unwrap();
    assert_eq!(a_b.xml_id_name(), a_space_b.xml_id_name());

    let ms = vec![
        simple("base", &[]),
        manifest("m1", "name = 'M1'\ncategory = 'A/B'\n"),
        manifest("m2", "name = 'M2'\ncategory = 'A B'\n"),
    ];
    let mut cat = Catalog::new();
    let err = cat.update_list(&ms).unwrap_err();
    assert!(
        matches!(&err, Error::CategoryIdCollision { first, second, xml_id }
            if *first == a_b && *second == a_space_b && xml_id == "module_category_a_b"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Module categories \"A/B\" and \"A B\" would share the external id \
         'base.module_category_a_b'; rename one of them"
    );
    assert_eq!(cat.rows().count(), 0, "a refused list changes nothing");

    // the same collision against a row the list does not mention
    let mut cat = Catalog::new();
    cat.update_list(&ms[..2]).unwrap();
    let err = cat.update_list(&ms[2..]).unwrap_err();
    assert!(matches!(&err, Error::CategoryIdCollision { .. }), "{err:?}");
    assert_eq!(cat.rows().count(), 2);

    // paths that merely share ancestors are fine
    let mut cat = Catalog::new();
    cat.update_list(&[
        manifest("p1", "name = 'P1'\ncategory = 'A/B'\n"),
        manifest("p2", "name = 'P2'\ncategory = 'A/C'\n"),
    ])
    .unwrap();
}

/// B6: the initial install set takes the options that hold `skip_auto_install`.
#[test]
fn seed_initial_follows_the_options() {
    let ms = vec![
        simple("base", &[]),
        manifest(
            "glue",
            "name = 'Glue'\ndepends = ['base']\nauto_install = true\n",
        ),
    ];
    let mut cat = catalog_in(&ms, &[]);
    let marked = cat
        .seed_initial(&InstallOptions::new().with_skip_auto_install(true))
        .unwrap();
    assert_eq!(marked, [n("base")]);
    assert_eq!(state(&cat, "glue"), ModuleState::Uninstalled);

    let mut cat = catalog_in(&ms, &[]);
    let marked = cat.seed_initial(&InstallOptions::new()).unwrap();
    assert_eq!(marked, [n("base"), n("glue")]);
    assert_eq!(state(&cat, "glue"), ModuleState::ToInstall);
}

#[test]
fn spec_mod_058_force_demo_refuses_what_it_would_install_or_stamp() {
    // happy path: installed native rows are scheduled directly, demo is set on every row, no
    // dependency closure and no version change
    let ms = vec![
        simple("base", &[]),
        simple("a", &["base"]),
        simple("u", &["a"]),
    ];
    let mut cat = catalog(&ms, &["base", "a"]);
    cat.force_demo().unwrap();
    for name in ["base", "a"] {
        assert_eq!(cat.state(name), Some(ModuleState::ToUpgrade), "{name}");
    }
    assert_eq!(cat.state("u"), Some(ModuleState::Uninstalled));
    assert!(cat.rows().all(|r| r.demo()));

    // a pending operation, whoever asks
    for state in [
        ModuleState::ToInstall,
        ModuleState::ToUpgrade,
        ModuleState::ToRemove,
    ] {
        let mut cat = catalog_in(
            &ms,
            &[
                ("base", ModuleState::Installed),
                ("a", ModuleState::Installed),
                ("u", state),
            ],
        );
        let before = cat
            .rows()
            .map(|r| (r.name().clone(), r.state(), r.demo()))
            .collect::<Vec<_>>();
        let err = cat.force_demo().unwrap_err();
        assert!(
            matches!(&err, Error::ForceDemoPending { modules } if modules == &[n("u")]),
            "{state:?}: {err:?}"
        );
        let after = cat
            .rows()
            .map(|r| (r.name().clone(), r.state(), r.demo()))
            .collect::<Vec<_>>();
        assert_eq!(after, before, "a refused request changes nothing");
    }

    // (b) the manifest on disk is newer than the installed version: nothing is stamped
    let newer = vec![
        simple("base", &[]),
        manifest("a", "name = \"A\"\nversion = \"1.1\"\n"),
    ];
    let mut cat = catalog(&newer, &["base", "a"]);
    let err = cat.force_demo().unwrap_err();
    assert!(
        matches!(&err, Error::ForceDemoVersionMismatch { modules } if modules == &[n("a")]),
        "{err:?}"
    );
    assert_eq!(cat.state("a"), Some(ModuleState::Installed));
    assert!(!cat.get("a").unwrap().demo());

    // (c) a dependency added on disk and not installed: the closure would install it
    let grown = vec![
        simple("base", &[]),
        simple("a", &["base"]),
        simple("u", &["base"]),
        simple("g", &["a", "u"]),
    ];
    let mut cat = catalog(&grown, &["base", "a", "g"]);
    let err = cat.force_demo().unwrap_err();
    assert!(
        matches!(&err, Error::ForceDemoMissingDependency { module, dependency } if module == &n("g") && dependency == &n("u")),
        "{err:?}"
    );
    assert_eq!(cat.state("u"), Some(ModuleState::Uninstalled));
}
