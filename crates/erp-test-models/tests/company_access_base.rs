//! Base company access migration (design `m3-company-a1.md` §7).
#![allow(clippy::unwrap_used, clippy::panic, reason = "tests")]

use erp_registry::{CompanyAccessState, RegistryBuilder, Restriction};
use erp_sdk::{ErrorKind, Value};
use erp_security::company_access::generate;
use erp_test_models::test_company_emit;
use erp_testkit::Harness;

fn vals(pairs: &[(&str, Value)]) -> std::collections::BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

#[test]
fn base_declares_its_company_scoped_models() {
    let registry = RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    assert_eq!(registry.company_access_gaps().count(), 0);
    let company = registry.model("res.company").unwrap();
    assert_eq!(
        company.field_by_id(company.parent_name.unwrap()).name,
        "parent_id"
    );
    assert_eq!(
        company.field_by_id(company.parent_path.unwrap()).name,
        "parent_path"
    );
    // contacts and users are company-scoped with their user-type escape
    for model in ["res.partner", "res.users"] {
        let spec = registry.model(model).unwrap();
        assert!(
            matches!(spec.company_access, CompanyAccessState::Declared(a) if matches!(a.restriction(), Restriction::Direct(_))),
            "{model}"
        );
    }
    let generated = generate(&registry).unwrap();
    let exact = "[('company_id', 'in', company_ids + [False])]";
    let ancestor = "['|', ('company_id', '=', False), ('company_id', 'parent_of', company_ids)]";
    for (model, expected) in [
        ("ir.default", exact),
        ("ir.sequence", exact),
        (
            "ir.sequence.date_range",
            "[('sequence_id', 'any', [('company_id', 'in', company_ids + [False])])]",
        ),
        ("ir.dated.parameter", ancestor),
        ("res.currency.rate", ancestor),
        ("ir.config.setting.scope", exact),
        (
            "ir.config.setting.value",
            "[('scope_id', 'any', [('company_id', 'in', company_ids + [False])])]",
        ),
        (
            "ir.config.setting.audit",
            "[('scope_id', 'any', [('company_id', 'in', company_ids + [False])])]",
        ),
    ] {
        let restriction = generated.iter().find(|g| g.model_name() == model).unwrap();
        assert_eq!(restriction.text(), expected, "{model}");
    }
}

/// PostgreSQL is in base_partner_users_scope.rs)
#[test]
fn base_contacts_and_users_generate_the_user_type_escape() {
    let registry = RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    assert_eq!(registry.company_access_gaps().count(), 0);
    let generated = generate(&registry).unwrap();
    let text = |model: &str| {
        generated
            .iter()
            .find(|g| g.model_name() == model)
            .unwrap_or_else(|| panic!("{model} generates no restriction"))
            .text()
            .to_owned()
    };
    assert_eq!(
        text("res.partner"),
        "['|', ('partner_share', '=', False), '|', ('company_id', '=', False), \
         ('company_id', 'parent_of', company_ids)]"
    );
    assert_eq!(
        text("res.users"),
        "['|', ('share', '=', False), ('company_ids', 'in', company_ids)]"
    );
}

#[test]
fn base_and_install_fixture_have_no_company_access_gaps() {
    let registry = RegistryBuilder::new([&erp_addon_base::ADDON, &test_company_emit::ADDON])
        .build()
        .unwrap();
    let gaps: Vec<_> = registry.company_access_gaps().map(|m| m.name).collect();
    assert!(gaps.is_empty(), "undeclared models: {gaps:?}");
}

#[tokio::test]
async fn res_company_keeps_a_parent_path() {
    let h = Harness::builder([&erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let currency: i64 = tx
        .query(
            "INSERT INTO res_currency (name, symbol, rounding, active) \
             VALUES ('TST', 'T', 0.01, true) RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let model = tx.env().records("res.company").unwrap();
    let root = model
        .create_values(
            &mut tx,
            &[vals(&[
                ("name", Value::str("Root")),
                ("currency_id", Value::Int(currency)),
            ])],
        )
        .await
        .unwrap();
    let branch = model
        .create_values(
            &mut tx,
            &[vals(&[
                ("name", Value::str("Branch")),
                ("currency_id", Value::Int(currency)),
                ("parent_id", Value::Int(root.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    assert_eq!(
        branch.value(&mut tx, "parent_path").await.unwrap(),
        Value::str(format!("{}/{}/", root.ids()[0], branch.ids()[0]))
    );
    let before = branch.value(&mut tx, "parent_path").await.unwrap();
    let err = branch
        .write_values(&mut tx, &vals(&[("parent_id", Value::False)]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert_eq!(err.message(), "The company hierarchy cannot be changed.");
    assert_eq!(branch.value(&mut tx, "parent_path").await.unwrap(), before);
    drop(tx);
    h.cleanup().await.unwrap();
}
