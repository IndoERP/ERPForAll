//! Native addon skeletons and manifest integration tests for the M7 inventory family.
//!
//! Stock remains below accounting. The valuation bridge additionally loads account/analytic;
//! unknown dependency targets still fail loudly.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_sdk::AddonDescriptor;
use erp_testkit::Harness;
use std::collections::{BTreeSet, HashMap};

/// Descriptors of the 12 M7 addons.
fn m7_addons() -> Vec<&'static AddonDescriptor> {
    vec![
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
        &erp_addon_delivery::ADDON,
        &erp_addon_stock_delivery::ADDON,
        &erp_addon_product_expiry::ADDON,
        &erp_addon_stock_sms::ADDON,
        &erp_addon_stock_account::ADDON,
        &erp_addon_stock_landed_costs::ADDON,
        &erp_addon_purchase_stock::ADDON,
        &erp_addon_sale_stock::ADDON,
        &erp_addon_stock_dropshipping::ADDON,
    ]
}

/// `base`, `web`, `uom`, `product` and the 12 M7 addons, the set every test composes.
fn all_addons() -> Vec<&'static AddonDescriptor> {
    let mut all = vec![
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ];
    all.extend(m7_addons());
    all
}

/// The `manifest.toml` text of each M7 addon next to its name.
fn manifests() -> [(&'static str, &'static str); 12] {
    [
        ("barcodes", include_str!("../../barcodes/manifest.toml")),
        (
            "barcodes_gs1_nomenclature",
            include_str!("../../barcodes_gs1_nomenclature/manifest.toml"),
        ),
        ("stock", include_str!("../../stock/manifest.toml")),
        ("delivery", include_str!("../../delivery/manifest.toml")),
        (
            "stock_delivery",
            include_str!("../../stock_delivery/manifest.toml"),
        ),
        (
            "product_expiry",
            include_str!("../../product_expiry/manifest.toml"),
        ),
        ("stock_sms", include_str!("../../stock_sms/manifest.toml")),
        (
            "stock_account",
            include_str!("../../stock_account/manifest.toml"),
        ),
        (
            "stock_landed_costs",
            include_str!("../../stock_landed_costs/manifest.toml"),
        ),
        (
            "purchase_stock",
            include_str!("../../purchase_stock/manifest.toml"),
        ),
        ("sale_stock", include_str!("../../sale_stock/manifest.toml")),
        (
            "stock_dropshipping",
            include_str!("../../stock_dropshipping/manifest.toml"),
        ),
    ]
}

/// Assert `loaded` (registry load order) lists every name of `expected` once, and every addon
/// after all of its dependencies.
fn assert_dependency_order(loaded: &[&'static AddonDescriptor], expected: &[&AddonDescriptor]) {
    let position: HashMap<&str, usize> = loaded
        .iter()
        .enumerate()
        .map(|(index, addon)| (addon.name, index))
        .collect();
    assert_eq!(
        position.len(),
        loaded.len(),
        "an addon is loaded twice in the registry"
    );
    let loaded_names: BTreeSet<&str> = position.keys().copied().collect();
    let expected_names: BTreeSet<&str> = expected.iter().map(|addon| addon.name).collect();
    assert_eq!(
        loaded_names, expected_names,
        "registry must hold exactly the composed addons"
    );
    for addon in loaded {
        for dep in addon.depends {
            assert!(
                position.contains_key(dep),
                "{} depends on unknown addon {dep}",
                addon.name
            );
            assert!(
                position[dep] < position[addon.name],
                "{} (position {}) must load after its dependency {dep} (position {})",
                addon.name,
                position[addon.name],
                position[dep]
            );
        }
    }
}

/// acyclic; the N8 boundary at descriptor level; the module graph order by depth then name is
/// not asserted here)
#[test]
fn inventory_addon_descriptors_and_manifest_graph() {
    let addons = m7_addons();
    assert_eq!(
        addons.len(),
        12,
        "all 12 M7 inventory addons have descriptors"
    );

    let mut deps_map: HashMap<&str, &[&str]> = HashMap::new();
    for desc in &addons {
        assert!(
            deps_map.insert(desc.name, desc.depends).is_none(),
            "duplicate descriptor name {}",
            desc.name
        );
        assert!(
            !desc.depends.is_empty(),
            "addon {} must declare dependencies",
            desc.name
        );
    }

    // an unknown dependency (a typo, or an addon of a later milestone) fails loudly
    let known: BTreeSet<&str> = deps_map
        .keys()
        .copied()
        .chain(["base", "web", "uom", "product", "analytic", "account"])
        .collect();
    for (name, deps) in &deps_map {
        for dep in *deps {
            assert!(
                known.contains(dep),
                "{name} depends on {dep}, which is neither an M7 addon nor base/web/uom/product: add it to \
                 the design note's deferred-dependency table and to this test on purpose"
            );
        }
    }

    // acyclic: depth-first walk, every edge is followed (all targets are known here)
    fn visit<'a>(
        node: &'a str,
        deps_map: &HashMap<&'a str, &'a [&'a str]>,
        done: &mut BTreeSet<&'a str>,
        stack: &mut Vec<&'a str>,
    ) {
        assert!(
            !stack.contains(&node),
            "dependency cycle: {} -> {node}",
            stack.join(" -> ")
        );
        if !done.insert(node) {
            return;
        }
        stack.push(node);
        for dep in deps_map.get(node).copied().unwrap_or(&[]) {
            visit(dep, deps_map, done, stack);
        }
        stack.pop();
    }
    let mut done = BTreeSet::new();
    for name in deps_map.keys() {
        visit(name, &deps_map, &mut done, &mut Vec::new());
    }

    // Boundary 4.1 / N8, descriptor level only: stock is below stock_account. This says nothing
    // about fields; the field-level claim belongs to.
    let transitive = |start: &str| -> BTreeSet<&str> {
        let mut seen = BTreeSet::new();
        let mut todo = vec![start];
        while let Some(node) = todo.pop() {
            for dep in deps_map.get(node).copied().unwrap_or(&[]) {
                if seen.insert(*dep) {
                    todo.push(dep);
                }
            }
        }
        seen
    };
    assert!(
        !transitive("stock").contains("stock_account"),
        "N8: stock must not depend, even transitively, on stock_account"
    );
    assert!(transitive("stock_account").contains("stock"));
    assert!(transitive("stock_account").contains("account"));
    assert!(!transitive("stock").contains("account"));
    assert!(deps_map["stock"].contains(&"product"));
    assert!(deps_map["stock_landed_costs"].contains(&"purchase_stock"));
    assert!(deps_map["stock_delivery"].contains(&"sale_stock"));
}

/// the registry's topological order, not the module graph's depth-then-name order)
#[test]
fn inventory_addons_registry_builder() {
    let all = all_addons();
    let registry = Harness::builder(all.iter().copied())
        .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
        .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
        .registry_only()
        .expect("clean registry build with all 12 M7 inventory addons");
    assert_dependency_order(registry.addons(), &all);
}

#[test]
fn inventory_addons_registry_order_independent_of_input_order() {
    let mut reversed = all_addons();
    reversed.reverse();
    let registry = Harness::builder(reversed.iter().copied())
        .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
        .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
        .registry_only()
        .expect("registry build from addons given in reverse order");
    assert_dependency_order(registry.addons(), &reversed);
}

/// The `depends` of each M7 `manifest.toml` equals the `depends` of its `erp_sdk::addon!`
/// descriptor (ignoring `base`, which the manifest parser always adds), so the two sources cannot
/// drift; the license is `MIT OR Apache-2.0`. The file is read as plain TOML: the strict manifest
/// parser (`erp-modules`) is not reachable from an addon crate (`scripts/check-layers.py`).
#[test]
fn inventory_manifest_depends_match_descriptors() {
    let descriptors: HashMap<&str, &AddonDescriptor> =
        m7_addons().into_iter().map(|d| (d.name, d)).collect();
    assert_eq!(descriptors.len(), manifests().len());
    for (name, text) in manifests() {
        let table: toml::Table = text.parse().expect("manifest.toml is valid TOML");
        let depends = table
            .get("depends")
            .and_then(toml::Value::as_array)
            .expect("manifest has a `depends` array");
        let mut from_manifest: BTreeSet<&str> = depends
            .iter()
            .map(|d| d.as_str().expect("depends entries are strings"))
            .collect();
        from_manifest.remove("base");
        let descriptor = descriptors
            .get(name)
            .expect("a descriptor for the manifest");
        let mut from_descriptor: BTreeSet<&str> = descriptor.depends.iter().copied().collect();
        from_descriptor.remove("base");
        assert_eq!(
            from_manifest, from_descriptor,
            "manifest.toml and lib.rs disagree on depends of {name}"
        );
        assert_eq!(
            table.get("license").and_then(toml::Value::as_str),
            Some("MIT OR Apache-2.0"),
            "license of {name}"
        );
    }
}

/// database, the addons present in dependency order; no `ir.module.module` state and no
/// `button_immediate_install` is exercised)
#[tokio::test]
async fn inventory_addons_testkit_harness_install() {
    let all = all_addons();
    let h = Harness::builder(all.iter().copied())
        .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
        .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
        .build()
        .await
        .expect("throwaway database harness with M7 inventory addon set");

    assert_dependency_order(h.registry().addons(), &all);

    let mut tx = h.begin().await.expect("begin transaction on harness");
    let env = tx.env();
    assert_eq!(env.uid(), 1, "superuser environment");

    tx.rollback().await.expect("clean rollback");
    h.cleanup().await.expect("clean database cleanup");
}
