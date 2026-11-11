//! what `addons/base` contributes to the access engine besides its tables: the
//! `PolicySourceWrite` registrations of its policy and membership models
//! and the capability mapping it ships as data
//! (`data/ir.capability.csv`,: the five M3 capabilities and every one a linked
//! crate declares).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use erp_data::{ConvertMode, CsvImportResult, parse_csv};
use erp_orm::{CAPABILITIES, POLICY_SOURCE_WRITES, SourceKind};
use erp_registry::RegistryBuilder;
use erp_sdk::Value;
use erp_security::rows::CapabilityRow;
use erp_testkit::Harness;
use support::map;

// erp-rpc declares `rpc.debug_errors`; link it so its declaration is in CAPABILITIES
use erp_rpc as _;

fn base_file(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../addons/base")
        .join(path)
}

/// (name, groups, description) of every row of `data/ir.capability.csv`, by its external id.
fn capability_rows() -> BTreeMap<String, (String, String, String)> {
    let content = std::fs::read_to_string(base_file("data/ir.capability.csv")).unwrap();
    let CsvImportResult::Loaded(import) =
        parse_csv(&content, "ir.capability.csv", "base", ConvertMode::Init).unwrap()
    else {
        panic!("the capability data loads");
    };
    assert_eq!(import.model(), "ir.capability");
    assert_eq!(import.fields(), ["id", "name", "groups", "description"]);
    import
        .rows()
        .iter()
        .map(|row| {
            let [id, name, groups, description] = row.values() else {
                panic!("four cells on line {}", row.line());
            };
            (
                id.clone(),
                (name.clone(), groups.clone(), description.clone()),
            )
        })
        .collect()
}

/// The base manifest names the data file (the data loader loads what it lists).
#[test]
fn the_base_manifest_lists_the_capability_data() {
    let name = erp_modules::ModuleName::new("base").unwrap();
    let manifest = erp_modules::read_manifest_from_dir(&base_file(""), &name)
        .unwrap()
        .expect("addons/base has a manifest");
    assert!(
        manifest
            .data()
            .iter()
            .any(|d| d == "data/ir.capability.csv")
    );
}

/// base maps the five M3 capabilities and every capability a linked crate
/// declares (a registered capability without a row is a policy build error), each once, to a
/// valid group spec.
#[test]
fn base_maps_every_core_capability_once() {
    let rows = capability_rows();
    let names: BTreeSet<&str> = rows.values().map(|(name, ..)| name.as_str()).collect();
    assert_eq!(names.len(), rows.len(), "one row per capability");
    for (name, groups, _) in rows.values() {
        CapabilityRow::new(name, groups).unwrap();
    }
    for adr_name in [
        "policy.explain_others",
        "policy.customize",
        "principal.impersonate",
        "audit.read",
        "company.all",
    ] {
        assert!(names.contains(adr_name), "{adr_name} has a row");
    }
    let declared: Vec<&str> = CAPABILITIES.iter().map(|c| c.name()).collect();
    assert!(declared.contains(&"rpc.debug_errors"), "erp-rpc is linked");
    for name in declared {
        assert!(
            names.contains(name),
            "the linked capability {name} has a row"
        );
    }
}

/// The rows load into `ir.capability` through the ORM and the policy loader reads them back.
#[tokio::test]
async fn the_capability_data_loads_into_the_policy() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .expect("harness");
    let rows = capability_rows();
    let mut tx = h.begin().await.unwrap();
    let capabilities = tx.env().records("ir.capability").unwrap();
    for (xmlid, (name, groups, description)) in &rows {
        let created = capabilities
            .create_values(
                &mut tx,
                &[map(&[
                    ("name", Value::str(name.as_str())),
                    ("groups", Value::str(groups.as_str())),
                    ("description", Value::str(description.as_str())),
                ])],
            )
            .await
            .unwrap();
        tx.execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', $1, 'ir.capability', $2)",
            &[xmlid, &created.ids()[0]],
        )
        .await
        .unwrap();
    }
    tx.flush().await.unwrap();
    let loaded = erp_security::loader::load_rows(&mut tx).await.unwrap();
    let read: BTreeSet<(&str, &str)> = loaded
        .capabilities()
        .iter()
        .map(|c| (c.name(), c.groups()))
        .collect();
    let shipped: BTreeSet<(&str, &str)> = rows
        .values()
        .map(|(name, groups, _)| (name.as_str(), groups.as_str()))
        .collect();
    assert_eq!(read, shipped);
    h.cleanup().await.unwrap();
}

/// DN §3.4: base registers its policy tables (every write, or the field-group writes of
/// `ir.model.fields`, or only creates and deletes of `ir.model`) and the membership columns of
/// `res.users` (and company deletions); each registration names fields its model has.
#[test]
fn base_registers_its_policy_and_membership_models() {
    let registry = RegistryBuilder::new([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([&erp_data::IR_MODEL_DATA])
        .build()
        .unwrap();
    // model -> (kind, [(field, whether a write of it counts)])
    type Registered<'r> = BTreeMap<&'r str, (SourceKind, Vec<(&'r str, bool)>)>;
    let registered: Registered<'_> = POLICY_SOURCE_WRITES
        .iter()
        .map(|w| {
            let spec = w.validate(&registry).unwrap().expect("installed with base");
            let fields = spec
                .fields
                .iter()
                .filter(|f| f.name != "id")
                .map(|f| (f.name, w.watches_field(f.name)))
                .collect();
            (w.model(), (w.kind(), fields))
        })
        .collect();
    let watched = |model: &str| -> Vec<&str> {
        registered[model]
            .1
            .iter()
            .filter(|(_, watched)| *watched)
            .map(|(name, _)| *name)
            .collect()
    };
    let kinds: BTreeMap<&str, SourceKind> = registered.iter().map(|(m, (k, _))| (*m, *k)).collect();
    assert_eq!(
        kinds,
        BTreeMap::from([
            ("ir.access", SourceKind::Policy),
            ("ir.capability", SourceKind::Policy),
            ("ir.model", SourceKind::Policy),
            ("ir.model.fields", SourceKind::Policy),
            ("res.company", SourceKind::Membership),
            ("res.groups", SourceKind::Policy),
            ("res.groups.privilege", SourceKind::Policy),
            ("res.users", SourceKind::Membership),
        ])
    );
    for model in [
        "ir.access",
        "res.groups",
        "res.groups.privilege",
        "ir.capability",
    ] {
        assert!(
            registered[model].1.iter().all(|(_, watched)| *watched),
            "every field of {model}"
        );
    }
    assert_eq!(watched("ir.model.fields"), ["groups"]);
    assert!(watched("ir.model").is_empty());
    assert_eq!(watched("res.company"), ["active"]);
    let mut users = watched("res.users");
    users.sort_unstable();
    assert_eq!(users, ["active", "company_id", "company_ids", "group_ids"]);
}
