//! Partner fields, defaults, hierarchy and integrity test suite.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_orm::Records;
use erp_sdk::{Cell, Command, Domain, Operator, RawVals, RecId, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use erp_testkit::tokio_postgres::NoTls;

async fn partner_harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .expect("harness")
}

// Match base_currency_nodes.rs: separate pools and ORM state over the fixture database.
// Sharing the immutable registry does not share transaction caches, policies or pool state.
fn independent_partner_node(harness: &Harness) -> erp_orm::Orm {
    let manager = Manager::from_config(
        harness.db().config(),
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool = Pool::builder(manager)
        .max_size(2)
        .runtime(Runtime::Tokio1)
        .build()
        .unwrap();
    erp_orm::Orm::builder(std::sync::Arc::clone(harness.registry()), pool)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

fn p_rec(tx: &erp_orm::Transaction, model: &str) -> Records {
    tx.env().records(model).unwrap()
}

#[tokio::test]
async fn spec_partner_001_base_field_set() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = p_rec(&tx, "res.partner");
    let fields = partners.fields_get(&mut tx, None, None).await.unwrap();

    let expected_fields = [
        "name",
        "complete_name",
        "display_name",
        "parent_id",
        "parent_name",
        "child_ids",
        "ref",
        "lang",
        "tz",
        "vat",
        "has_vat",
        "website",
        "category_id",
        "active",
        "type",
        "street",
        "street2",
        "zip",
        "city",
        "email",
        "email_formatted",
        "phone",
        "is_company",
        "industry_id",
        "company_id",
        "color",
        "user_ids",
        "main_user_id",
        "partner_share",
        "commercial_partner_id",
        "commercial_company_name",
    ];

    for field_name in expected_fields {
        assert!(
            fields.contains_key(field_name),
            "expected field '{field_name}' in res.partner fields_get"
        );
    }

    assert!(
        !fields.contains_key("company_type"),
        "the reference ERP 20 has no company_type field on res.partner"
    );

    let is_company = &fields["is_company"];
    assert_eq!(is_company.get("readonly"), Some(&Value::Bool(true)));

    let complete_name = &fields["complete_name"];
    assert_eq!(complete_name.get("store"), Some(&Value::Bool(true)));

    let commercial_partner_id = &fields["commercial_partner_id"];
    assert_eq!(commercial_partner_id.get("store"), Some(&Value::Bool(true)));

    let partner_share = &fields["partner_share"];
    assert_eq!(partner_share.get("store"), Some(&Value::Bool(true)));
}

#[tokio::test]
async fn spec_partner_business_field_labels() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = p_rec(&tx, "res.partner");
    let partner_fields = partners
        .fields_get(
            &mut tx,
            Some(&["vat", "ref", "street2", "zip"]),
            Some(&["string"]),
        )
        .await
        .unwrap();

    let string_of =
        |fields: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, Value>>,
         name: &str|
         -> String {
            fields
                .get(name)
                .and_then(|attrs| attrs.get("string"))
                .and_then(|v| match v {
                    Value::Str(s) => Some(s.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        };

    assert_eq!(string_of(&partner_fields, "vat"), "Tax ID");
    assert_eq!(string_of(&partner_fields, "ref"), "Reference");
    assert_eq!(string_of(&partner_fields, "street2"), "Street 2");
    assert_eq!(string_of(&partner_fields, "zip"), "ZIP");

    let companies = p_rec(&tx, "res.company");
    let company_fields = companies
        .fields_get(&mut tx, Some(&["vat", "street2", "zip"]), Some(&["string"]))
        .await
        .unwrap();

    assert_eq!(string_of(&company_fields, "vat"), "Tax ID");
    assert_eq!(string_of(&company_fields, "street2"), "Street 2");
    assert_eq!(string_of(&company_fields, "zip"), "ZIP");
}

#[tokio::test]
async fn spec_partner_002_defaults_with_name_only() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();
    // The case requires a sole installed language, rather than an invented English default.
    p_rec(&tx, "res.lang")
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("French".into()))
                    .set_cell("code", Cell::Str("fr_FR".into()))
                    .set_cell("url_code", Cell::Str("fr".into()))
                    .set_cell("active", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap();

    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str("Test Company".into()));

    let partner = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![vals])
        .await
        .unwrap();
    let id = partner.ids()[0];

    assert_eq!(
        partner.value(&mut tx, "type").await.unwrap(),
        Value::Str("contact".into())
    );
    assert_eq!(
        partner.value(&mut tx, "active").await.unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        partner.value(&mut tx, "color").await.unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        partner.value(&mut tx, "is_company").await.unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        partner.value(&mut tx, "has_vat").await.unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        partner.value(&mut tx, "partner_share").await.unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        partner
            .value(&mut tx, "commercial_partner_id")
            .await
            .unwrap(),
        Value::Int(id)
    );
    assert_eq!(
        partner.value(&mut tx, "complete_name").await.unwrap(),
        Value::Str("Test Company".into())
    );
    assert_eq!(
        partner.value(&mut tx, "display_name").await.unwrap(),
        Value::Str("Test Company".into())
    );
    assert_eq!(
        partner.value(&mut tx, "company_id").await.unwrap(),
        Value::False
    );
    assert_eq!(
        partner.value(&mut tx, "parent_id").await.unwrap(),
        Value::False
    );
    assert_eq!(
        partner.value(&mut tx, "lang").await.unwrap(),
        Value::Str("fr_FR".into())
    );
    assert_eq!(
        partner.value(&mut tx, "category_id").await.unwrap(),
        Value::list([])
    );
}

#[tokio::test]
async fn spec_partner_003_contacts_require_name() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Fixture P root: GhostStep
    let mut ghost_vals = RawVals::new();
    ghost_vals.insert("name".into(), Cell::Str("GhostStep".into()));
    ghost_vals.insert("type".into(), Cell::Str("contact".into()));
    let ghost = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![ghost_vals])
        .await
        .unwrap();
    let ghost_id = ghost.ids()[0];

    // (a) creating {'type': 'contact'} with no name fails
    let mut a_vals = RawVals::new();
    a_vals.insert("type".into(), Cell::Str("contact".into()));
    // Core Validation failures require an explicit savepoint/rollback boundary before
    // continuing. Check the public contract without relying on a poisoned transaction.
    let a_res = tx
        .savepoint(async |tx: &mut erp_orm::Transaction| {
            p_rec(tx, "res.partner").create(tx, vec![a_vals]).await
        })
        .await;
    assert!(a_res.is_err(), "contact without name must fail");
    let err = a_res.err().unwrap();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The database refused the change: Contacts require a name"
    );

    // (b) creating {'type': 'invoice', 'parent_id': GhostStep} with no name succeeds
    let mut b_vals = RawVals::new();
    b_vals.insert("type".into(), Cell::Str("invoice".into()));
    b_vals.insert("parent_id".into(), Cell::Id(ghost_id));
    let b_res = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![b_vals])
        .await;
    assert!(
        b_res.is_ok(),
        "nameless invoice child must succeed: {:?}",
        b_res.err()
    );
    let b_partner = b_res.unwrap();

    // (c) writing type='contact' on (b) fails
    let mut c_vals = RawVals::new();
    c_vals.insert("type".into(), Cell::Str("contact".into()));
    let c_res = tx
        .savepoint(async |tx: &mut erp_orm::Transaction| b_partner.write(tx, c_vals).await)
        .await;
    assert!(
        c_res.is_err(),
        "writing type='contact' on nameless partner must fail"
    );
    let err = c_res.err().unwrap();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(err.message(), "Contacts require a name");
    assert_eq!(
        b_partner.value(&mut tx, "type").await.unwrap(),
        Value::str("invoice")
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut verify = h.begin().await.unwrap();
    let child = p_rec(&verify, "res.partner")
        .browse(b_partner.ids())
        .unwrap();
    assert_eq!(
        child.value(&mut verify, "type").await.unwrap(),
        Value::str("invoice")
    );
    drop(verify);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_004_default_get_company_and_type() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Create currency and companies C1 and C2
    let mut curr_vals = RawVals::new();
    curr_vals.insert("name".into(), Cell::Str("EUR".into()));
    curr_vals.insert("symbol".into(), Cell::Str("€".into()));
    curr_vals.insert("rounding".into(), Cell::Decimal("0.01".parse().unwrap()));
    let curr = p_rec(&tx, "res.currency")
        .create(&mut tx, vec![curr_vals])
        .await
        .unwrap()
        .ids()[0];
    let mut c1_vals = RawVals::new();
    c1_vals.insert("name".into(), Cell::Str("Company 1".into()));
    c1_vals.insert("currency_id".into(), Cell::Id(curr));
    let c1 = p_rec(&tx, "res.company")
        .create(&mut tx, vec![c1_vals])
        .await
        .unwrap()
        .ids()[0];

    let mut c2_vals = RawVals::new();
    c2_vals.insert("name".into(), Cell::Str("Company 2".into()));
    c2_vals.insert("currency_id".into(), Cell::Id(curr));
    let c2 = p_rec(&tx, "res.company")
        .create(&mut tx, vec![c2_vals])
        .await
        .unwrap()
        .ids()[0];

    // Parent Q with company_id = C1
    let mut q_vals = RawVals::new();
    q_vals.insert("name".into(), Cell::Str("Parent Q".into()));
    q_vals.insert("company_id".into(), Cell::Id(c1));
    let q = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![q_vals])
        .await
        .unwrap()
        .ids()[0];

    // default_get with default_parent_id = Q and default_company_id = C2
    let env_ctx = p_rec(&tx, "res.partner").with_context([
        ("default_parent_id".to_string(), Value::Int(q)),
        ("default_company_id".to_string(), Value::Int(c2)),
    ]);
    let defaults = env_ctx
        .default_get(&mut tx, &["company_id", "parent_id"])
        .await
        .unwrap();

    assert_eq!(
        defaults.get("company_id"),
        Some(&Value::Int(c1)),
        "parent's company C1 must win over default_company_id C2"
    );

    // default_get with default_type='lead' (unknown type)
    let env_lead = p_rec(&tx, "res.partner")
        .with_context([("default_type".to_string(), Value::Str("lead".into()))]);
    let defaults_lead = env_lead.default_get(&mut tx, &["type"]).await.unwrap();
    let lead_type = defaults_lead.get("type");
    assert!(
        lead_type.is_none()
            || lead_type == Some(&Value::False)
            || lead_type == Some(&Value::Str("".into())),
        "unknown default_type must be dropped or empty, got: {:?}",
        lead_type
    );
}

#[tokio::test]
async fn spec_partner_005_language_computation_and_propagation() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Activate de_DE and fr_FR
    for (name, code, url) in [("German", "de_DE", "de"), ("French", "fr_FR", "fr")] {
        let mut vals = RawVals::new();
        vals.insert("name".into(), Cell::Str(name.into()));
        vals.insert("code".into(), Cell::Str(code.into()));
        vals.insert("url_code".into(), Cell::Str(url.into()));
        vals.insert("active".into(), Cell::Bool(true));
        vals.insert("direction".into(), Cell::Str("ltr".into()));
        p_rec(&tx, "res.lang")
            .create(&mut tx, vec![vals])
            .await
            .unwrap();
    }

    // Root without lang and child
    let mut r1_vals = RawVals::new();
    r1_vals.insert("name".into(), Cell::Str("Root 1".into()));
    let r1 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![r1_vals])
        .await
        .unwrap();
    let r1_id = r1.ids()[0];

    let mut c1_vals = RawVals::new();
    c1_vals.insert("name".into(), Cell::Str("Child 1".into()));
    c1_vals.insert("parent_id".into(), Cell::Id(r1_id));
    let c1 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![c1_vals])
        .await
        .unwrap();

    let def_lang = r1.value(&mut tx, "lang").await.unwrap();
    assert_eq!(c1.value(&mut tx, "lang").await.unwrap(), def_lang);

    // Root with context default_lang='de_DE'
    let env_de = p_rec(&tx, "res.partner")
        .with_context([("default_lang".to_string(), Value::Str("de_DE".into()))]);
    let mut r2_vals = RawVals::new();
    r2_vals.insert("name".into(), Cell::Str("Root 2".into()));
    let r2 = env_de.create(&mut tx, vec![r2_vals]).await.unwrap();
    let r2_id = r2.ids()[0];
    assert_eq!(
        r2.value(&mut tx, "lang").await.unwrap(),
        Value::Str("de_DE".into())
    );

    // First child of r2
    let mut c2_vals = RawVals::new();
    c2_vals.insert("name".into(), Cell::Str("Child 2".into()));
    c2_vals.insert("parent_id".into(), Cell::Id(r2_id));
    let c2 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![c2_vals])
        .await
        .unwrap();
    assert_eq!(
        c2.value(&mut tx, "lang").await.unwrap(),
        Value::Str("de_DE".into())
    );

    // Write lang='fr_FR' on root r2
    let mut write_vals = RawVals::new();
    write_vals.insert("lang".into(), Cell::Str("fr_FR".into()));
    r2.write(&mut tx, write_vals).await.unwrap();

    // First child keeps de_DE
    assert_eq!(
        c2.value(&mut tx, "lang").await.unwrap(),
        Value::Str("de_DE".into())
    );

    // Second child gets fr_FR
    let mut c3_vals = RawVals::new();
    c3_vals.insert("name".into(), Cell::Str("Child 3".into()));
    c3_vals.insert("parent_id".into(), Cell::Id(r2_id));
    let c3 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![c3_vals])
        .await
        .unwrap();
    assert_eq!(
        c3.value(&mut tx, "lang").await.unwrap(),
        Value::Str("fr_FR".into())
    );

    // Explicit lang is kept
    let mut explicit_vals = RawVals::new();
    explicit_vals.insert("name".into(), Cell::Str("Explicit".into()));
    explicit_vals.insert("parent_id".into(), Cell::Id(r2_id));
    explicit_vals.insert("lang".into(), Cell::Str("de_DE".into()));
    let exp = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![explicit_vals])
        .await
        .unwrap();
    assert_eq!(
        exp.value(&mut tx, "lang").await.unwrap(),
        Value::Str("de_DE".into())
    );
}

#[tokio::test]
async fn spec_partner_006_tz_selection_and_default() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Partner created with context tz='Europe/Brussels'
    let env_tz = p_rec(&tx, "res.partner")
        .with_context([("tz".to_string(), Value::Str("Europe/Brussels".into()))]);
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str("Brussels Partner".into()));
    let p = env_tz.create(&mut tx, vec![vals]).await.unwrap();
    assert_eq!(
        p.value(&mut tx, "tz").await.unwrap(),
        Value::Str("Europe/Brussels".into())
    );

    // Selection contains every known zone, Etc/* sorted last
    let fields = p_rec(&tx, "res.partner")
        .fields_get(&mut tx, None, None)
        .await
        .unwrap();
    let tz_field = &fields["tz"];
    let selection = match tz_field.get("selection") {
        Some(Value::List(l)) => l,
        other => panic!("expected selection list, got: {:?}", other),
    };

    assert!(!selection.is_empty(), "tz selection must not be empty");

    let mut seen_etc = false;
    for item in selection.iter() {
        let key = match item {
            Value::List(pair) => match &pair[0] {
                Value::Str(s) => s.as_ref(),
                _ => panic!("key must be str"),
            },
            _ => panic!("pair must be list"),
        };
        if key.starts_with("Etc/") {
            seen_etc = true;
        } else {
            assert!(
                !seen_etc,
                "non-Etc timezone '{key}' appeared after Etc/* timezone in selection"
            );
        }
    }

    // tz_offset of partner without tz is "+0000"
    let mut p_no_tz_vals = RawVals::new();
    p_no_tz_vals.insert("name".into(), Cell::Str("No TZ".into()));
    let p_no_tz = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p_no_tz_vals])
        .await
        .unwrap();
    assert_eq!(
        p_no_tz.value(&mut tx, "tz_offset").await.unwrap(),
        Value::Str("+0000".into())
    );
}

#[tokio::test]
async fn invalid_partner_timezone_is_not_silently_presented_as_utc() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = p_rec(&tx, "res.partner");
    for zone in [Cell::Null, Cell::Str("".into()), Cell::Str("UTC".into())] {
        let unset = model
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("UTC offset fixture".into()))
                        .set_cell("tz", zone),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            unset.value(&mut tx, "tz_offset").await.unwrap(),
            Value::str("+0000")
        );
    }
    let invalid = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Invalid offset fixture".into()))
                    .set_cell("tz", Cell::Str("Invalid/Definitely_Not_A_Zone".into())),
            ],
        )
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| invalid.value(tx, "tz_offset").await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(error.message(), "res.partner.tz: unknown timezone");
}

#[tokio::test]
async fn spec_partner_007_website_normalisation() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    let cases = [
        ("example.com", "http://example.com"),
        ("example.com/page", "http://example.com/page"),
        ("https://example.com", "https://example.com"),
    ];

    for (input, expected) in cases {
        let mut vals = RawVals::new();
        vals.insert("name".into(), Cell::Str("Web Partner".into()));
        vals.insert("website".into(), Cell::Str(input.into()));
        let p = p_rec(&tx, "res.partner")
            .create(&mut tx, vec![vals])
            .await
            .unwrap();
        assert_eq!(
            p.value(&mut tx, "website").await.unwrap(),
            Value::Str(expected.into())
        );
    }

    // Write website "example.org"
    let mut p_vals = RawVals::new();
    p_vals.insert("name".into(), Cell::Str("Target".into()));
    p_vals.insert("website".into(), Cell::Str("https://example.com".into()));
    let p = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p_vals])
        .await
        .unwrap();

    let mut w_vals = RawVals::new();
    w_vals.insert("website".into(), Cell::Str("example.org".into()));
    p.write(&mut tx, w_vals).await.unwrap();
    assert_eq!(
        p.value(&mut tx, "website").await.unwrap(),
        Value::Str("http://example.org".into())
    );

    // Write website False / empty
    let mut empty_vals = RawVals::new();
    empty_vals.insert("website".into(), Cell::Null);
    p.write(&mut tx, empty_vals).await.unwrap();
    assert_eq!(p.value(&mut tx, "website").await.unwrap(), Value::False);
}

#[tokio::test]
async fn spec_partner_015_default_order() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Create "B", "A" (older), "A" (newer), and "A, X" (child X of older "A")
    let mut b_vals = RawVals::new();
    b_vals.insert("name".into(), Cell::Str("B".into()));
    let p_b = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![b_vals])
        .await
        .unwrap();

    let mut a1_vals = RawVals::new();
    a1_vals.insert("name".into(), Cell::Str("A".into()));
    let p_a1 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![a1_vals])
        .await
        .unwrap();

    let mut a2_vals = RawVals::new();
    a2_vals.insert("name".into(), Cell::Str("A".into()));
    let p_a2 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![a2_vals])
        .await
        .unwrap();

    let mut ax_vals = RawVals::new();
    ax_vals.insert("name".into(), Cell::Str("X".into()));
    ax_vals.insert("parent_id".into(), Cell::Id(p_a1.ids()[0]));
    let p_ax = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![ax_vals])
        .await
        .unwrap();

    let all_ids = [p_b.ids()[0], p_a1.ids()[0], p_a2.ids()[0], p_ax.ids()[0]];

    let domain = Domain::condition("id", Operator::In, all_ids.to_vec()).unwrap();
    let search_res = p_rec(&tx, "res.partner")
        .search(&mut tx, &domain)
        .await
        .unwrap();

    // Expected order: newer "A" (a2), older "A" (a1), "A, X" (ax), "B" (b)
    let expected = vec![p_a2.ids()[0], p_a1.ids()[0], p_ax.ids()[0], p_b.ids()[0]];
    assert_eq!(search_res.ids(), expected.as_slice());
}

#[tokio::test]
async fn spec_partner_016_commercial_partner_topmost_ancestor() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // p0 root
    let mut p0_v = RawVals::new();
    p0_v.insert("name".into(), Cell::Str("p0".into()));
    let p0 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p0_v])
        .await
        .unwrap();
    let p0_id = p0.ids()[0];

    // p1 child of p0
    let mut p1_v = RawVals::new();
    p1_v.insert("name".into(), Cell::Str("p1".into()));
    p1_v.insert("parent_id".into(), Cell::Id(p0_id));
    let p1 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p1_v])
        .await
        .unwrap();
    let p1_id = p1.ids()[0];

    // p12 child of p1
    let mut p12_v = RawVals::new();
    p12_v.insert("name".into(), Cell::Str("p12".into()));
    p12_v.insert("parent_id".into(), Cell::Id(p1_id));
    let p12 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p12_v])
        .await
        .unwrap();

    // p2 child of p0
    let mut p2_v = RawVals::new();
    p2_v.insert("name".into(), Cell::Str("p2".into()));
    p2_v.insert("parent_id".into(), Cell::Id(p0_id));
    let p2 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p2_v])
        .await
        .unwrap();
    let p2_id = p2.ids()[0];

    // p21 child of p2
    let mut p21_v = RawVals::new();
    p21_v.insert("name".into(), Cell::Str("p21".into()));
    p21_v.insert("parent_id".into(), Cell::Id(p2_id));
    let p21 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p21_v])
        .await
        .unwrap();

    // p3 root
    let mut p3_v = RawVals::new();
    p3_v.insert("name".into(), Cell::Str("p3".into()));
    let p3 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p3_v])
        .await
        .unwrap();
    let p3_id = p3.ids()[0];

    assert_eq!(
        p0.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(p0_id)
    );
    assert_eq!(
        p1.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(p0_id)
    );
    assert_eq!(
        p12.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(p0_id)
    );
    assert_eq!(
        p2.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(p0_id)
    );
    assert_eq!(
        p21.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(p0_id)
    );
    assert_eq!(
        p3.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(p3_id)
    );
}

#[tokio::test]
async fn spec_partner_017_commercial_partner_subtree_recompute() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // company_1 and company_2 roots
    let mut c1_v = RawVals::new();
    c1_v.insert("name".into(), Cell::Str("company 1".into()));
    let c1 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![c1_v])
        .await
        .unwrap();
    let c1_id = c1.ids()[0];

    let mut c2_v = RawVals::new();
    c2_v.insert("name".into(), Cell::Str("company 2".into()));
    let c2 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![c2_v])
        .await
        .unwrap();
    let c2_id = c2.ids()[0];

    // contact under company_1
    let mut ct_v = RawVals::new();
    ct_v.insert("name".into(), Cell::Str("contact".into()));
    ct_v.insert("parent_id".into(), Cell::Id(c1_id));
    let ct = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![ct_v])
        .await
        .unwrap();
    let ct_id = ct.ids()[0];

    // delivery child and contact child under contact
    let mut del_v = RawVals::new();
    del_v.insert("name".into(), Cell::Str("delivery".into()));
    del_v.insert("type".into(), Cell::Str("delivery".into()));
    del_v.insert("parent_id".into(), Cell::Id(ct_id));
    let del = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![del_v])
        .await
        .unwrap();

    let mut sub_v = RawVals::new();
    sub_v.insert("name".into(), Cell::Str("subcontact".into()));
    sub_v.insert("parent_id".into(), Cell::Id(ct_id));
    let sub = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![sub_v])
        .await
        .unwrap();

    assert_eq!(
        ct.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(c1_id)
    );
    assert_eq!(
        del.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(c1_id)
    );
    assert_eq!(
        sub.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(c1_id)
    );

    // Moving contact under company_2
    let mut move_v = RawVals::new();
    move_v.insert("parent_id".into(), Cell::Id(c2_id));
    ct.write(&mut tx, move_v).await.unwrap();

    assert_eq!(
        ct.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(c2_id)
    );
    assert_eq!(
        ct.value(&mut tx, "commercial_company_name").await.unwrap(),
        Value::Str("company 2".into())
    );
    assert_eq!(
        del.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(c2_id)
    );
    assert_eq!(
        sub.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(c2_id)
    );

    // Clearing contact's parent_id
    let mut clear_v = RawVals::new();
    clear_v.insert("parent_id".into(), Cell::Null);
    ct.write(&mut tx, clear_v).await.unwrap();

    assert_eq!(
        ct.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(ct_id)
    );
    assert_eq!(
        del.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(ct_id)
    );
    assert_eq!(
        sub.value(&mut tx, "commercial_partner_id").await.unwrap(),
        Value::Int(ct_id)
    );
}

#[tokio::test]
async fn spec_partner_018_has_vat_and_is_company() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Roots with vat BE0477472701, /, na, NA, empty, none
    let vat_samples = [
        ("Root Real", Some("BE0477472701"), true, true),
        ("Root Slash", Some("/"), false, false),
        ("Root na", Some("na"), false, false),
        ("Root NA", Some("NA"), false, false),
        ("Root Empty", Some(""), false, false),
        ("Root None", None, false, false),
    ];

    let mut real_id = 0;
    for (name, vat, exp_has_vat, exp_is_company) in vat_samples {
        let mut v = RawVals::new();
        v.insert("name".into(), Cell::Str(name.into()));
        if let Some(vat_str) = vat {
            v.insert("vat".into(), Cell::Str(vat_str.into()));
        }
        let p = p_rec(&tx, "res.partner")
            .create(&mut tx, vec![v])
            .await
            .unwrap();
        if exp_has_vat {
            real_id = p.ids()[0];
        }
        assert_eq!(
            p.value(&mut tx, "has_vat").await.unwrap(),
            Value::Bool(exp_has_vat),
            "has_vat mismatch for {name}"
        );
        assert_eq!(
            p.value(&mut tx, "is_company").await.unwrap(),
            Value::Bool(exp_is_company),
            "is_company mismatch for {name}"
        );
    }

    // Child with vat (synced/set): commercial_partner_id != self, so is_company is false even with VAT
    let mut child_v = RawVals::new();
    child_v.insert("name".into(), Cell::Str("Child With Vat".into()));
    child_v.insert("parent_id".into(), Cell::Id(real_id));
    child_v.insert("vat".into(), Cell::Str("BE0477472701".into()));
    let child = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![child_v])
        .await
        .unwrap();
    assert_eq!(
        child.value(&mut tx, "has_vat").await.unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        child.value(&mut tx, "is_company").await.unwrap(),
        Value::Bool(false)
    );

    // Root created with {'name': 'Co', 'is_company': True} and no vat
    let mut co_v = RawVals::new();
    co_v.insert("name".into(), Cell::Str("Co".into()));
    co_v.insert("is_company".into(), Cell::Bool(true));
    let co = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![co_v])
        .await
        .unwrap();
    assert_eq!(
        co.value(&mut tx, "is_company").await.unwrap(),
        Value::Bool(false),
        "is_company without vat must recompute to false"
    );

    // Searching has_vat = True returns only partners with real tax ID
    let has_vat_domain = Domain::condition("has_vat", Operator::Eq, true).unwrap();
    let has_vat_partners = p_rec(&tx, "res.partner")
        .search(&mut tx, &has_vat_domain)
        .await
        .unwrap();

    let ids = has_vat_partners.ids();
    assert!(ids.contains(&real_id));
    assert!(ids.contains(&child.ids()[0]));
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn spec_partner_019_vat_kept_as_given_without_country() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    let mut v = RawVals::new();
    v.insert("name".into(), Cell::Str("No Country Partner".into()));
    let p = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![v])
        .await
        .unwrap();

    let mut write_v = RawVals::new();
    write_v.insert("vat".into(), Cell::Str("be 0477.472.701".into()));
    p.write(&mut tx, write_v).await.unwrap();

    assert_eq!(
        p.value(&mut tx, "vat").await.unwrap(),
        Value::Str("be 0477.472.701".into())
    );
}

#[tokio::test]
async fn spec_partner_035_self_parent_and_cycles_refused() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // p1 Elmtree; p2 child of p1; p3 child of p2; p3b child of p2
    let mut p1_v = RawVals::new();
    p1_v.insert("name".into(), Cell::Str("Elmtree".into()));
    let p1 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p1_v])
        .await
        .unwrap();
    let p1_id = p1.ids()[0];

    let mut p2_v = RawVals::new();
    p2_v.insert("name".into(), Cell::Str("p2".into()));
    p2_v.insert("parent_id".into(), Cell::Id(p1_id));
    let p2 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p2_v])
        .await
        .unwrap();
    let p2_id = p2.ids()[0];

    let mut p3_v = RawVals::new();
    p3_v.insert("name".into(), Cell::Str("p3".into()));
    p3_v.insert("parent_id".into(), Cell::Id(p2_id));
    let p3 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p3_v])
        .await
        .unwrap();
    let p3_id = p3.ids()[0];

    let mut p3b_v = RawVals::new();
    p3b_v.insert("name".into(), Cell::Str("p3b".into()));
    p3b_v.insert("parent_id".into(), Cell::Id(p2_id));
    let p3b = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p3b_v])
        .await
        .unwrap();
    let p3b_id = p3b.ids()[0];

    // 1. p1.parent_id = p3 -> cycle
    let mut cyc1 = RawVals::new();
    cyc1.insert("parent_id".into(), Cell::Id(p3_id));
    let res1 = tx.savepoint(async |tx| p1.write(tx, cyc1).await).await;
    assert!(res1.is_err());
    let err1 = res1.unwrap_err();
    assert_eq!(err1.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err1.message(),
        "You cannot create recursive Partner hierarchies."
    );

    // 2. p2.parent_id = p3 -> cycle
    let mut cyc2 = RawVals::new();
    cyc2.insert("parent_id".into(), Cell::Id(p3_id));
    let res2 = tx.savepoint(async |tx| p2.write(tx, cyc2).await).await;
    assert!(res2.is_err());
    let err2 = res2.unwrap_err();
    assert_eq!(err2.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err2.message(),
        "You cannot create recursive Partner hierarchies."
    );

    // 3. p3.parent_id = p3 -> self-parent cycle
    let mut cyc3 = RawVals::new();
    cyc3.insert("parent_id".into(), Cell::Id(p3_id));
    let res3 = tx.savepoint(async |tx| p3.write(tx, cyc3).await).await;
    assert!(res3.is_err());
    let err3 = res3.unwrap_err();
    assert_eq!(err3.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err3.message(),
        "You cannot create recursive Partner hierarchies."
    );

    // 4. (p3 + p1).parent_id = p2 -> p1 is already parent of p2, so setting p1.parent_id = p2 creates cycle
    let p3_and_p1 = p_rec(&tx, "res.partner").browse([p3_id, p1_id]).unwrap();
    let mut cyc4 = RawVals::new();
    cyc4.insert("parent_id".into(), Cell::Id(p2_id));
    let res4 = tx
        .savepoint(async |tx| p3_and_p1.write(tx, cyc4).await)
        .await;
    assert!(res4.is_err());
    let err4 = res4.unwrap_err();
    assert_eq!(err4.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err4.message(),
        "You cannot create recursive Partner hierarchies."
    );

    // 5. Two update commands in same transaction making p3 and p3b parents of each other (H1)
    let res_mutual = tx
        .savepoint(async |tx| {
            let mut cyc5a = RawVals::new();
            cyc5a.insert("parent_id".into(), Cell::Id(p3b_id));
            p3.write(tx, cyc5a).await?;

            let mut cyc5b = RawVals::new();
            cyc5b.insert("parent_id".into(), Cell::Id(p3_id));
            p3b.write(tx, cyc5b).await
        })
        .await;
    assert!(res_mutual.is_err());
    let err_mutual = res_mutual.unwrap_err();
    assert_eq!(err_mutual.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err_mutual.message(),
        "You cannot create recursive Partner hierarchies."
    );

    // Nested update commands exercise the x2many path, not only separate direct writes.
    let mut first_update = RawVals::new();
    first_update.insert("parent_id".into(), Cell::Id(p3b_id));
    let mut second_update = RawVals::new();
    second_update.insert("parent_id".into(), Cell::Id(p3_id));
    let nested = RawVals::new().set_commands(
        "child_ids",
        [
            Command::update(RecId::from_real(p3_id).unwrap(), first_update),
            Command::update(RecId::from_real(p3b_id).unwrap(), second_update),
        ],
    );
    let err = tx
        .savepoint(async |tx| p2.write(tx, nested).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "You cannot create recursive Partner hierarchies."
    );

    // 6. Non-hierarchy write on p1 + p2 + p3 together succeeds
    let all = p_rec(&tx, "res.partner")
        .browse([p1_id, p2_id, p3_id])
        .unwrap();
    let mut non_h = RawVals::new();
    non_h.insert("phone".into(), Cell::Str("123-456".into()));
    all.write(&mut tx, non_h).await.unwrap();
    assert_eq!(
        p1.value(&mut tx, "phone").await.unwrap(),
        Value::Str("123-456".into())
    );
    assert_eq!(
        p2.value(&mut tx, "phone").await.unwrap(),
        Value::Str("123-456".into())
    );
    assert_eq!(
        p3.value(&mut tx, "phone").await.unwrap(),
        Value::Str("123-456".into())
    );

    // 7. Tree remains unchanged: p1 has no parent, p2 has p1, p3 has p2, p3b has p2
    assert_eq!(p1.value(&mut tx, "parent_id").await.unwrap(), Value::False);
    assert_eq!(
        p2.value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(p1_id)
    );
    assert_eq!(
        p3.value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(p2_id)
    );
    assert_eq!(
        p3b.value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(p2_id)
    );
}

#[tokio::test]
async fn test_partner_hierarchy_concurrency_fence_40001() {
    let h = partner_harness().await;
    let mut setup = h.begin().await.unwrap();
    let mut p1_v = RawVals::new();
    p1_v.insert("name".into(), Cell::Str("P1_conc".into()));
    let p1 = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![p1_v])
        .await
        .unwrap();
    let p1_id = p1.ids()[0];

    let mut p2_v = RawVals::new();
    p2_v.insert("name".into(), Cell::Str("P2_conc".into()));
    let p2 = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![p2_v])
        .await
        .unwrap();
    let p2_id = p2.ids()[0];
    setup.commit().await.unwrap();

    // The nodes have independent ORM state and pools; only PostgreSQL owns the fence.
    // This exercises runtime isolation, not separate OS processes or process-kill recovery.
    let node_a = independent_partner_node(&h);
    let node_b = independent_partner_node(&h);
    let mut tx_a = node_a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let mut tx_b = node_b.begin(erp_orm::SUPERUSER_ID).await.unwrap();

    // Transaction A sets p1.parent_id = p2 (acquires fence)
    let mut v_a = RawVals::new();
    v_a.insert("parent_id".into(), Cell::Id(p2_id));
    let p1_a = p_rec(&tx_a, "res.partner").browse([p1_id]).unwrap();
    p1_a.write(&mut tx_a, v_a).await.unwrap();

    // Transaction B concurrently attempts to set p2.parent_id = p1
    let pid_b = erp_testkit::backend_pid(&mut tx_b).await.unwrap();
    let mut v_b = RawVals::new();
    v_b.insert("parent_id".into(), Cell::Id(p1_id));
    let p2_b = p_rec(&tx_b, "res.partner").browse([p2_id]).unwrap();

    let waiting_b = tokio::spawn(async move {
        let res = p2_b.write(&mut tx_b, v_b).await;
        (res, tx_b)
    });

    erp_testkit::wait_until_blocked(&h, pid_b, std::time::Duration::from_secs(10))
        .await
        .unwrap();

    // Transaction A commits, releasing the lock.
    tx_a.commit().await.unwrap();

    // Transaction B wakes up and must receive SQLSTATE 40001 (serialization failure)
    let (res_b, mut tx_b) = waiting_b.await.unwrap();
    assert!(
        res_b.is_err(),
        "Transaction B must fail due to concurrent update"
    );
    let err_b = res_b.unwrap_err();
    assert_eq!(err_b.kind(), erp_fields::ErrorKind::Concurrency);
    assert_eq!(err_b.sqlstate(), Some("40001"));
    assert_eq!(
        err_b.message(),
        "could not serialize access due to concurrent update"
    );

    tx_b.rollback().await.unwrap();
    drop(tx_a);
    drop(tx_b);

    // Verify in database: p1 has parent p2, p2 has NO parent (no cycle admitted!)
    let mut check_tx = node_b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let p1_check = p_rec(&check_tx, "res.partner").browse([p1_id]).unwrap();
    let p2_check = p_rec(&check_tx, "res.partner").browse([p2_id]).unwrap();
    assert_eq!(
        p1_check.value(&mut check_tx, "parent_id").await.unwrap(),
        Value::Int(p2_id)
    );
    assert_eq!(
        p2_check.value(&mut check_tx, "parent_id").await.unwrap(),
        Value::False
    );
}

#[tokio::test]
async fn test_partner_hierarchy_archived_ancestor_cycle() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // A is active root
    let mut a_v = RawVals::new();
    a_v.insert("name".into(), Cell::Str("A_arch".into()));
    let a = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![a_v])
        .await
        .unwrap();
    let a_id = a.ids()[0];

    // B is child of A, but archived (active = false)
    let mut b_v = RawVals::new();
    b_v.insert("name".into(), Cell::Str("B_arch".into()));
    b_v.insert("parent_id".into(), Cell::Id(a_id));
    b_v.insert("active".into(), Cell::Bool(false));
    let b = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![b_v])
        .await
        .unwrap();
    let b_id = b.ids()[0];

    // C is active child of B
    let mut c_v = RawVals::new();
    c_v.insert("name".into(), Cell::Str("C_arch".into()));
    c_v.insert("parent_id".into(), Cell::Id(b_id));
    let c = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![c_v])
        .await
        .unwrap();
    let c_id = c.ids()[0];

    // Now attempt to make A child of C: A -> C -> B (archived) -> A => cycle!
    let mut cyc = RawVals::new();
    cyc.insert("parent_id".into(), Cell::Id(c_id));
    let res = a.write(&mut tx, cyc).await;
    assert!(res.is_err(), "Must detect cycle through archived partner B");
    let err = res.unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "You cannot create recursive Partner hierarchies."
    );
}

#[tokio::test]
async fn test_partner_hierarchy_inaccessible_ancestor_cycle_is_opaque() {
    let hidden_read = Domain::condition("name", Operator::Ne, "Hidden ancestor").unwrap();
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule("res.partner", erp_orm::Operation::Read, hidden_read)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let mut root = RawVals::new();
    root.insert("name".into(), Cell::Str("Visible root".into()));
    let root_id = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![root])
        .await
        .unwrap()
        .ids()[0];
    let mut hidden = RawVals::new();
    hidden.insert("name".into(), Cell::Str("Hidden ancestor".into()));
    hidden.insert("parent_id".into(), Cell::Id(root_id));
    let hidden_id = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![hidden])
        .await
        .unwrap()
        .ids()[0];
    let mut leaf = RawVals::new();
    leaf.insert("name".into(), Cell::Str("Visible leaf".into()));
    leaf.insert("parent_id".into(), Cell::Id(hidden_id));
    let leaf_id = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![leaf])
        .await
        .unwrap()
        .ids()[0];
    // Record-rule denial evidence resolves the subject's stored name.
    setup.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (99999, 'Hierarchy reader', 'hierarchy-reader', true, false)").await.unwrap();
    setup.commit().await.unwrap();

    let mut user = h.begin_as(99999).await.unwrap();
    // Establish that this principal really cannot read the intermediate ancestor.
    let hidden = p_rec(&user, "res.partner").browse([hidden_id]).unwrap();
    let denied = hidden
        .check_access(&mut user, erp_orm::Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        denied.message(),
        format!(
            "Your access rights do not let you access 'Contact' (res.partner) records.\n\nRecords: res.partner({hidden_id},), User: 99999"
        )
    );
    let root = p_rec(&user, "res.partner").browse([root_id]).unwrap();
    let mut cycle = RawVals::new();
    cycle.insert("parent_id".into(), Cell::Id(leaf_id));
    let err = user
        .savepoint(async |tx| root.write(tx, cycle).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "You cannot create recursive Partner hierarchies."
    );
    assert_eq!(
        root.value(&mut user, "parent_id").await.unwrap(),
        Value::False
    );
}

#[tokio::test]
async fn test_partner_hierarchy_depth_and_visited_bounds() {
    let h = partner_harness().await;

    // 1. Test tightening context limit: set max_depth = 3
    let mut ctx = erp_orm::Context::new();
    ctx.set("partner_hierarchy_max_depth", Value::Int(3));
    let mut tx_tight = h.orm().begin_with(h.principal(), ctx).await.unwrap();

    // Create a chain of length 4: p1 <- p2 <- p3 <- p4
    let mut v1 = RawVals::new();
    v1.insert("name".into(), Cell::Str("b_p1".into()));
    let p1 = p_rec(&tx_tight, "res.partner")
        .create(&mut tx_tight, vec![v1])
        .await
        .unwrap();
    let p1_id = p1.ids()[0];

    let mut v2 = RawVals::new();
    v2.insert("name".into(), Cell::Str("b_p2".into()));
    v2.insert("parent_id".into(), Cell::Id(p1_id));
    let p2 = p_rec(&tx_tight, "res.partner")
        .create(&mut tx_tight, vec![v2])
        .await
        .unwrap();
    let p2_id = p2.ids()[0];

    let mut v3 = RawVals::new();
    v3.insert("name".into(), Cell::Str("b_p3".into()));
    v3.insert("parent_id".into(), Cell::Id(p2_id));
    let p3 = p_rec(&tx_tight, "res.partner")
        .create(&mut tx_tight, vec![v3])
        .await
        .unwrap();
    let p3_id = p3.ids()[0];

    let mut v4 = RawVals::new();
    v4.insert("name".into(), Cell::Str("b_p4".into()));
    v4.insert("parent_id".into(), Cell::Id(p3_id));
    let p4 = p_rec(&tx_tight, "res.partner")
        .create(&mut tx_tight, vec![v4])
        .await
        .unwrap();
    let p4_id = p4.ids()[0];

    // p5 child of p4 (depth 4 from p1) -> exceeds tightened depth of 3!
    let mut v5 = RawVals::new();
    v5.insert("name".into(), Cell::Str("b_p5".into()));
    v5.insert("parent_id".into(), Cell::Id(p4_id));
    let res = p_rec(&tx_tight, "res.partner")
        .create(&mut tx_tight, vec![v5])
        .await;
    assert!(res.is_err());
    let err = res.unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Partner hierarchy maximum depth of 3 exceeded."
    );
    tx_tight.rollback().await.unwrap();

    // 2. Test tightening-only refusal: context requesting depth 300 (> ceiling 256)
    let mut ctx_loose = erp_orm::Context::new();
    ctx_loose.set("partner_hierarchy_max_depth", Value::Int(300));
    let mut tx_loose = h.orm().begin_with(h.principal(), ctx_loose).await.unwrap();
    let res_loose = erp_addon_base::partner_hierarchy::resolve_hierarchy_limits(&tx_loose);
    assert!(res_loose.is_err());
    let err_loose = res_loose.unwrap_err();
    assert_eq!(err_loose.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err_loose.message(),
        "partner_hierarchy_max_depth (300) cannot exceed ceiling of 256"
    );
    tx_loose.rollback().await.unwrap();

    // 3. Test tightening context limit: set max_visited = 3
    let mut ctx_v = erp_orm::Context::new();
    ctx_v.set("partner_hierarchy_max_visited", Value::Int(3));
    let mut tx_v = h.orm().begin_with(h.principal(), ctx_v).await.unwrap();

    // Sibling scale test (M2): multiple siblings under 1 parent visit only 1 DB node (parent), so <= 3 passes
    let mut v_parent = RawVals::new();
    v_parent.insert("name".into(), Cell::Str("parent_sib".into()));
    let psib = p_rec(&tx_v, "res.partner")
        .create(&mut tx_v, vec![v_parent])
        .await
        .unwrap();
    let psib_id = psib.ids()[0];

    let siblings: Vec<_> = (0..10)
        .map(|i| {
            let mut child = RawVals::new();
            child.insert("name".into(), Cell::Str(format!("child_sib_{i}").into()));
            child
        })
        .collect();
    let siblings = p_rec(&tx_v, "res.partner")
        .create(&mut tx_v, siblings)
        .await
        .unwrap();
    let mut parent = RawVals::new();
    parent.insert("parent_id".into(), Cell::Id(psib_id));
    siblings.write(&mut tx_v, parent).await.unwrap();
    assert_eq!(siblings.ids().len(), 10);

    // Visited exceedance: chain with 4 ancestor database nodes visits 4 nodes > limit of 3
    // Chain: n1 <- n2 <- n3 <- n4
    let mut vn1 = RawVals::new();
    vn1.insert("name".into(), Cell::Str("n1".into()));
    let n1 = p_rec(&tx_v, "res.partner")
        .create(&mut tx_v, vec![vn1])
        .await
        .unwrap();
    let n1_id = n1.ids()[0];

    let mut vn2 = RawVals::new();
    vn2.insert("name".into(), Cell::Str("n2".into()));
    vn2.insert("parent_id".into(), Cell::Id(n1_id));
    let n2 = p_rec(&tx_v, "res.partner")
        .create(&mut tx_v, vec![vn2])
        .await
        .unwrap();
    let n2_id = n2.ids()[0];

    let mut vn3 = RawVals::new();
    vn3.insert("name".into(), Cell::Str("n3".into()));
    vn3.insert("parent_id".into(), Cell::Id(n2_id));
    let n3 = p_rec(&tx_v, "res.partner")
        .create(&mut tx_v, vec![vn3])
        .await
        .unwrap();
    let n3_id = n3.ids()[0];

    let mut vn4 = RawVals::new();
    vn4.insert("name".into(), Cell::Str("n4".into()));
    vn4.insert("parent_id".into(), Cell::Id(n3_id));
    let n4 = p_rec(&tx_v, "res.partner")
        .create(&mut tx_v, vec![vn4])
        .await
        .unwrap();
    let n4_id = n4.ids()[0];

    // n5 with parent n4 traverses n4, n3, n2, n1 (4 DB nodes) > 3 limit
    let mut vn5 = RawVals::new();
    vn5.insert("name".into(), Cell::Str("n5".into()));
    vn5.insert("parent_id".into(), Cell::Id(n4_id));
    let res_v = p_rec(&tx_v, "res.partner")
        .create(&mut tx_v, vec![vn5])
        .await;
    assert!(res_v.is_err());
    let err_v = res_v.unwrap_err();
    assert_eq!(err_v.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err_v.message(),
        "Partner hierarchy maximum visited nodes limit of 3 exceeded."
    );
    tx_v.rollback().await.unwrap();

    // 4. Test tightening-only refusal: context requesting visited 5000 (> ceiling 4096)
    let mut ctx_v_loose = erp_orm::Context::new();
    ctx_v_loose.set("partner_hierarchy_max_visited", Value::Int(5000));
    let mut tx_v_loose = h
        .orm()
        .begin_with(h.principal(), ctx_v_loose)
        .await
        .unwrap();
    let res_vl = erp_addon_base::partner_hierarchy::resolve_hierarchy_limits(&tx_v_loose);
    assert!(res_vl.is_err());
    let err_vl = res_vl.unwrap_err();
    assert_eq!(err_vl.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err_vl.message(),
        "partner_hierarchy_max_visited (5000) cannot exceed ceiling of 4096"
    );
    tx_v_loose.rollback().await.unwrap();
}

#[tokio::test]
async fn test_partner_hierarchy_visited_budget_charges_only_database_ancestors() {
    let h = partner_harness().await;
    let mut setup = h.begin().await.unwrap();
    let mut ids = Vec::new();
    for index in 0..5 {
        let mut values =
            RawVals::new().set_cell("name", Cell::Str(format!("Budget node {index}").into()));
        if let Some(parent) = ids.last() {
            values.insert("parent_id".into(), Cell::Id(*parent));
        }
        let created = p_rec(&setup, "res.partner")
            .create(&mut setup, vec![values])
            .await
            .unwrap();
        ids.push(created.ids()[0]);
    }
    setup.commit().await.unwrap();

    let mut context = erp_orm::Context::new();
    context.set("partner_hierarchy_max_visited", Value::Int(1));
    let mut tx = h.orm().begin_with(h.principal(), context).await.unwrap();
    // The actual registered constraint builds its in-flight edge map from the entire batch.
    // With every node present no database ancestor edge is charged. Leaving the root out
    // charges one distinct edge, reused by every path, rather than each in-flight identity.
    for starts in [&ids[..], &ids[1..]] {
        p_rec(&tx, "res.partner")
            .browse(starts.iter().copied())
            .unwrap()
            .call::<(), ()>(&mut tx, "_check_parent_recursion", ())
            .await
            .unwrap();
    }
    for edge in ids.windows(2) {
        assert_eq!(
            p_rec(&tx, "res.partner")
                .browse([edge[1]])
                .unwrap()
                .value(&mut tx, "parent_id")
                .await
                .unwrap(),
            Value::Int(edge[0])
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_partner_hierarchy_record_context_tightens_depth() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut root = RawVals::new();
    root.insert("name".into(), Cell::Str("Context root".into()));
    let root_id = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![root])
        .await
        .unwrap()
        .ids()[0];
    let mut branch = RawVals::new();
    branch.insert("name".into(), Cell::Str("Context branch".into()));
    branch.insert("parent_id".into(), Cell::Id(root_id));
    let branch_id = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![branch])
        .await
        .unwrap()
        .ids()[0];
    let mut leaf = RawVals::new();
    leaf.insert("name".into(), Cell::Str("Context leaf".into()));
    let leaf = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![leaf])
        .await
        .unwrap();
    let tightened = leaf.with_context([("partner_hierarchy_max_depth", Value::Int(1))]);
    let mut parent = RawVals::new();
    parent.insert("parent_id".into(), Cell::Id(branch_id));
    let err = tx
        .savepoint(async |tx| tightened.write(tx, parent.clone()).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Partner hierarchy maximum depth of 1 exceeded."
    );
    // Tightening one derived recordset does not mutate the transaction's caller context.
    leaf.write(&mut tx, parent).await.unwrap();
    assert_eq!(
        leaf.value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(branch_id)
    );
}

#[tokio::test]
async fn test_partner_hierarchy_record_context_rejects_invalid_limits() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = p_rec(&tx, "res.partner")
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Limit root".into()))],
        )
        .await
        .unwrap();
    let leaf = p_rec(&tx, "res.partner")
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Limit leaf".into()))],
        )
        .await
        .unwrap();
    for (key, ceiling) in [
        ("partner_hierarchy_max_depth", 256),
        ("partner_hierarchy_max_visited", 4096),
    ] {
        for (value, message) in [
            (Value::Int(0), format!("{key} must be greater than zero")),
            (Value::Int(-1), format!("{key} must be greater than zero")),
            (Value::Bool(false), format!("{key} must be an integer")),
            (Value::str("1"), format!("{key} must be an integer")),
            (
                Value::Int(i64::MAX),
                format!("{key} ({}) cannot exceed ceiling of {ceiling}", i64::MAX),
            ),
        ] {
            let constrained = leaf.with_context([(key, value)]);
            let values = RawVals::new().set_cell("parent_id", Cell::Id(root.ids()[0]));
            let error = tx
                .savepoint(async |tx| constrained.write(tx, values).await)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(error.message(), message);
            assert_eq!(
                leaf.value(&mut tx, "parent_id").await.unwrap(),
                Value::False
            );
        }
    }
    // Every refused derived context leaves the original operation usable.
    leaf.write(
        &mut tx,
        RawVals::new().set_cell("parent_id", Cell::Id(root.ids()[0])),
    )
    .await
    .unwrap();
    assert_eq!(
        leaf.value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(root.ids()[0])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_partner_hierarchy_fence_table_core_ddl() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Check table exists and has exactly 1 row with id = 1
    let rows = tx
        .query("SELECT id FROM res_partner_hierarchy_fence", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>(0), 1);

    // Verify fence update succeeds
    erp_addon_base::partner_hierarchy::acquire_hierarchy_fence(&mut tx)
        .await
        .unwrap();

    // Verify core table requirement is linked and valid
    erp_orm::core_ddl_validate(
        h.registry(),
        &[&erp_addon_base::partner_hierarchy::HIERARCHY_FENCE_REQUIREMENT],
    )
    .unwrap();
}

#[tokio::test]
async fn test_partner_hierarchy_unauthorized_write_returns_access_not_validation() {
    let h = erp_testkit::Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(erp_orm::StaticPolicy::SuperuserOnly)
        .build()
        .await
        .expect("harness");
    let mut setup = h.begin().await.unwrap();
    let mut p1_v = RawVals::new();
    p1_v.insert("name".into(), Cell::Str("P1_unauth".into()));
    let p1 = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![p1_v])
        .await
        .unwrap();
    let p1_id = p1.ids()[0];
    setup.commit().await.unwrap();

    // Begin as non-superuser user (uid = 99999) without partner write permissions
    let mut user_tx = h.begin_as(99999).await.unwrap();
    let p1_user = p_rec(&user_tx, "res.partner").browse([p1_id]).unwrap();

    // Attempting a self-cycle write as unauthorized user
    let mut cyc = RawVals::new();
    cyc.insert("parent_id".into(), Cell::Id(p1_id));
    let err = p1_user.write(&mut user_tx, cyc).await.unwrap_err();
    assert_eq!(
        err.kind(),
        erp_fields::ErrorKind::Access,
        "Denied write must return Access error without evaluating hierarchy (H2)"
    );
    assert_eq!(
        err.message(),
        "Your access rights do not let you write 'Contact' (res.partner) records."
    );
    let mut harmless = RawVals::new();
    harmless.insert("parent_id".into(), Cell::Null);
    let err = p1_user.write(&mut user_tx, harmless).await.unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Your access rights do not let you write 'Contact' (res.partner) records."
    );
}

#[tokio::test]
async fn denied_contact_name_write_authorizes_before_validation() {
    for deny_field in [false, true] {
        let h = if deny_field {
            Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
                .policy(erp_testkit::FieldPolicy::new().deny(
                    "res.partner",
                    "name",
                    erp_orm::FieldOp::Write,
                ))
                .build()
                .await
                .unwrap()
        } else {
            Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
                .policy(
                    erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
                        .rule("res.partner", erp_orm::Operation::Write, Domain::FALSE)
                        .build()
                        .unwrap(),
                )
                .build()
                .await
                .unwrap()
        };
        let mut setup = h.begin().await.unwrap();
        let id = p_rec(&setup, "res.partner")
            .create(
                &mut setup,
                vec![RawVals::new().set_cell("name", Cell::Str("Protected contact".into()))],
            )
            .await
            .unwrap()
            .ids()[0];
        setup.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (99999, 'Name writer', 'name-writer', true, false)").await.unwrap();
        setup.commit().await.unwrap();
        let mut user = h.begin_as(99999).await.unwrap();
        let contact = p_rec(&user, "res.partner").browse([id]).unwrap();
        let mut observed = Vec::new();
        for name in [
            Cell::Str("".into()),
            Cell::Null,
            Cell::Str("Allowed-looking name".into()),
        ] {
            let error = user
                .savepoint(async |tx| {
                    contact
                        .write(tx, RawVals::new().set_cell("name", name))
                        .await
                })
                .await
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
            assert!(!error.message().contains("Contacts require a name"));
            observed.push(error.message().to_owned());
        }
        assert_eq!(
            observed[0], observed[1],
            "denial must not disclose validity"
        );
        assert_eq!(
            contact.value(&mut user, "name").await.unwrap(),
            Value::str("Protected contact")
        );
        user.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn test_partner_hierarchy_field_denial_precedes_cycle_validation() {
    let policy =
        erp_testkit::FieldPolicy::new().deny("res.partner", "parent_id", erp_orm::FieldOp::Write);
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let mut vals = RawVals::new();
    vals.insert("name".into(), Cell::Str("Field-protected parent".into()));
    let partner_id = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![vals])
        .await
        .unwrap()
        .ids()[0];
    setup.commit().await.unwrap();

    let mut user = h.begin_as(99999).await.unwrap();
    let partner = p_rec(&user, "res.partner").browse([partner_id]).unwrap();
    partner
        .check_access(&mut user, erp_orm::Operation::Write)
        .await
        .unwrap();
    // Neither clearing the field nor assigning a cycle may expose structural validation to
    // a caller who is allowed to write the record but cannot write this particular field.
    for parent in [Cell::Null, Cell::Id(partner_id)] {
        let mut vals = RawVals::new();
        vals.insert("parent_id".into(), parent);
        let err = user
            .savepoint(async |tx| partner.write(tx, vals).await)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Access);
        assert_eq!(
            err.message(),
            "You do not have enough rights to access the field \"parent_id\" on Contact (res.partner). Please contact your system administrator.\n\nOperation: write"
        );
    }
    assert_eq!(
        partner.value(&mut user, "parent_id").await.unwrap(),
        Value::False
    );
}

#[tokio::test]
async fn test_partner_hierarchy_denied_create_checks_access_before_fence() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(erp_orm::StaticPolicy::SuperuserOnly)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let mut child = RawVals::new();
    child.insert("name".into(), Cell::Str("Existing child".into()));
    let child_id = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![child])
        .await
        .unwrap()
        .ids()[0];
    // A missing fence row must not change an unauthorized caller's refusal into a database error.
    setup
        .execute("DELETE FROM res_partner_hierarchy_fence", &[])
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut denied = h.begin_as(99999).await.unwrap();
    let mut vals = RawVals::new().set_commands(
        "child_ids",
        [Command::link(RecId::from_real(child_id).unwrap())],
    );
    vals.insert("name".into(), Cell::Str("Denied parent".into()));
    let err = p_rec(&denied, "res.partner")
        .create(&mut denied, vec![vals])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Your access rights do not let you create 'Contact' (res.partner) records."
    );
}

#[tokio::test]
async fn test_partner_hierarchy_missing_fence_refuses_structural_write() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut parent = RawVals::new();
    parent.insert("name".into(), Cell::Str("Missing-fence parent".into()));
    let parent_id = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![parent])
        .await
        .unwrap()
        .ids()[0];
    let mut child = RawVals::new();
    child.insert("name".into(), Cell::Str("Missing-fence child".into()));
    let child = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![child])
        .await
        .unwrap();
    tx.execute("DELETE FROM res_partner_hierarchy_fence", &[])
        .await
        .unwrap();
    let mut mutation = RawVals::new();
    mutation.insert("parent_id".into(), Cell::Id(parent_id));
    // Caller context must never disable the operation-derived structural fence hint.
    let forged = child.with_context([("__base_partner_hierarchy_fence", Value::Bool(false))]);
    let err = tx
        .savepoint(async |tx| forged.write(tx, mutation).await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Database);
    assert_eq!(
        err.message(),
        "res_partner_hierarchy_fence row missing: expected 1 row updated, got 0"
    );
    assert_eq!(
        child.value(&mut tx, "parent_id").await.unwrap(),
        Value::False
    );
    assert_eq!(
        child.env().context().get("__base_partner_hierarchy_fence"),
        None
    );
}

#[tokio::test]
async fn test_partner_hierarchy_leaf_create_and_unlink_skip_fence() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = p_rec(&tx, "res.partner")
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Unfenced root".into()))],
        )
        .await
        .unwrap();
    let parent_id = parent.ids()[0];
    tx.execute("DELETE FROM res_partner_hierarchy_fence", &[])
        .await
        .unwrap();
    let child = p_rec(&tx, "res.partner")
        .with_context([("__base_partner_hierarchy_fence", Value::Bool(true))])
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Unfenced leaf".into()))
                    .set_cell("parent_id", Cell::Id(parent_id)),
            ],
        )
        .await
        .unwrap();
    // Internal derivation must not replace the caller's environment on create results.
    assert_eq!(
        child.env().context().get("__base_partner_hierarchy_fence"),
        Some(Value::Bool(true))
    );
    child
        .write(
            &mut tx,
            RawVals::new().set_cell("phone", Cell::Str("123".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        child.value(&mut tx, "parent_id").await.unwrap(),
        Value::Int(parent_id)
    );
    parent.unlink(&mut tx).await.unwrap();
    assert_eq!(
        child.value(&mut tx, "parent_id").await.unwrap(),
        Value::False
    );
}

#[tokio::test]
async fn test_partner_hierarchy_orm_run_retry_on_40001() {
    let h = partner_harness().await;
    let mut setup = h.begin().await.unwrap();
    let mut p1_v = RawVals::new();
    p1_v.insert("name".into(), Cell::Str("P1_retry".into()));
    let p1 = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![p1_v])
        .await
        .unwrap();
    let p1_id = p1.ids()[0];

    let mut p2_v = RawVals::new();
    p2_v.insert("name".into(), Cell::Str("P2_retry".into()));
    let p2 = p_rec(&setup, "res.partner")
        .create(&mut setup, vec![p2_v])
        .await
        .unwrap();
    let p2_id = p2.ids()[0];
    setup.commit().await.unwrap();

    let mut holder = h.begin().await.unwrap();
    // Holder acquires fence
    erp_addon_base::partner_hierarchy::acquire_hierarchy_fence(&mut holder)
        .await
        .unwrap();

    let orm = h.orm().clone();
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let (started, mut first_attempt) = tokio::sync::mpsc::unbounded_channel();
    let (res, ()) = tokio::join!(
        orm.run(erp_orm::SUPERUSER_ID, erp_orm::Context::new(), async |tx| {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let pid = erp_testkit::backend_pid(tx).await.unwrap();
            // This row is inserted before the conflicting statement in each attempt. Replaying
            // the callback without rolling back the first attempt would preserve two copies.
            let mut marker = RawVals::new();
            marker.insert("name".into(), Cell::Str("Retry attempt marker".into()));
            p_rec(tx, "res.partner").create(tx, vec![marker]).await?;
            if attempt == 0 {
                started.send(pid).unwrap();
            }
            let mut v = RawVals::new();
            v.insert("parent_id".into(), Cell::Id(p2_id));
            let p1_rec = p_rec(tx, "res.partner").browse([p1_id]).unwrap();
            p1_rec.write(tx, v).await
        }),
        async {
            let pid =
                tokio::time::timeout(std::time::Duration::from_secs(10), first_attempt.recv())
                    .await
                    .expect("first retry attempt did not reach the fence")
                    .expect("retry observer channel closed");
            erp_testkit::wait_until_blocked(&h, pid, std::time::Duration::from_secs(10))
                .await
                .unwrap();
            holder.commit().await.unwrap();
        }
    );
    res.unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);

    // Verify in DB that p1 has parent p2
    let mut check_tx = h.begin().await.unwrap();
    let p1_check = p_rec(&check_tx, "res.partner").browse([p1_id]).unwrap();
    assert_eq!(
        p1_check.value(&mut check_tx, "parent_id").await.unwrap(),
        Value::Int(p2_id)
    );
    let markers = check_tx
        .query(
            "SELECT count(*) FROM res_partner WHERE name = $1",
            &[&"Retry attempt marker"],
        )
        .await
        .unwrap();
    assert_eq!(markers[0].get::<_, i64>(0), 1);
}

#[tokio::test]
async fn spec_partner_036_cycle_check_costs_nothing_without_parents() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    let mut p1_v = RawVals::new();
    p1_v.insert("name".into(), Cell::Str("Root Without Parent".into()));
    let p1 = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![p1_v])
        .await
        .unwrap();

    // Call the actual registered constraint; unrelated writes cannot prove this contract.
    let before = tx.query_count();
    p_rec(&tx, "res.partner")
        .call::<(), ()>(&mut tx, "_check_parent_recursion", ())
        .await
        .unwrap();
    p1.call::<(), ()>(&mut tx, "_check_parent_recursion", ())
        .await
        .unwrap();
    assert_eq!(
        tx.query_count() - before,
        0,
        "empty/root cycle checks must not query"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_041_archived_partners_and_active_children() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Fixture P: GhostStep with Existing Contact
    let mut ghost_v = RawVals::new();
    ghost_v.insert("name".into(), Cell::Str("GhostStep".into()));
    let ghost = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![ghost_v])
        .await
        .unwrap();
    let ghost_id = ghost.ids()[0];

    let mut contact_v = RawVals::new();
    contact_v.insert("name".into(), Cell::Str("Existing Contact".into()));
    contact_v.insert("parent_id".into(), Cell::Id(ghost_id));
    let contact = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![contact_v])
        .await
        .unwrap();
    let contact_id = contact.ids()[0];

    // Archive Existing Contact
    let mut arch_v = RawVals::new();
    arch_v.insert("active".into(), Cell::Bool(false));
    contact.write(&mut tx, arch_v).await.unwrap();

    // Standard search does not find it
    let domain = Domain::condition("name", Operator::Eq, "Existing Contact").unwrap();
    let found = p_rec(&tx, "res.partner")
        .search(&mut tx, &domain)
        .await
        .unwrap();
    assert!(
        found.is_empty(),
        "archived partner must be hidden by default"
    );

    // GhostStep's child_ids does not include it
    let child_ids = ghost.value(&mut tx, "child_ids").await.unwrap();
    match child_ids {
        Value::List(l) => assert!(
            !l.iter().any(|v| *v == Value::Int(contact_id)),
            "child_ids must not include archived child"
        ),
        _ => panic!("child_ids must be list"),
    }

    // search with active_test=False finds it
    let env_no_active =
        p_rec(&tx, "res.partner").with_context([("active_test".to_string(), Value::Bool(false))]);
    let found_inactive = env_no_active.search(&mut tx, &domain).await.unwrap();
    assert_eq!(found_inactive.ids(), &[contact_id]);
}

#[tokio::test]
async fn spec_partner_045_deleting_parent_detaches_children() {
    let h = partner_harness().await;
    let mut tx = h.begin().await.unwrap();

    // Fixture P: GhostStep with Existing Contact
    let mut ghost_v = RawVals::new();
    ghost_v.insert("name".into(), Cell::Str("GhostStep".into()));
    let ghost = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![ghost_v])
        .await
        .unwrap();
    let ghost_id = ghost.ids()[0];

    let mut contact_v = RawVals::new();
    contact_v.insert("name".into(), Cell::Str("Existing Contact".into()));
    contact_v.insert("parent_id".into(), Cell::Id(ghost_id));
    let contact = p_rec(&tx, "res.partner")
        .create(&mut tx, vec![contact_v])
        .await
        .unwrap();
    let contact_id = contact.ids()[0];

    // Deleting GhostStep
    ghost.unlink(&mut tx).await.unwrap();

    // Existing Contact remains, parent_id is NULL/False, commercial_partner_id is self, complete_name is "Existing Contact"
    let reloaded = p_rec(&tx, "res.partner").browse([contact_id]).unwrap();
    assert_eq!(
        reloaded.value(&mut tx, "parent_id").await.unwrap(),
        Value::False
    );
    assert_eq!(
        reloaded
            .value(&mut tx, "commercial_partner_id")
            .await
            .unwrap(),
        Value::Int(contact_id)
    );
    assert_eq!(
        reloaded.value(&mut tx, "complete_name").await.unwrap(),
        Value::Str("Existing Contact".into())
    );
}
