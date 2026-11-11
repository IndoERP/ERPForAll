//! Address synchronization acceptance; no Contacts-screen delivery implied.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_orm::{Records, Transaction};
use erp_sdk::{Cell, RawVals, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}
fn model(tx: &Transaction) -> Records {
    tx.env().records("res.partner").unwrap()
}
fn vals(name: &str, parent: Option<i64>, street: Option<&str>) -> RawVals {
    let mut v = RawVals::new().set_cell("name", Cell::Str(name.into()));
    if let Some(p) = parent {
        v.insert("parent_id".into(), Cell::Id(p));
    }
    if let Some(s) = street {
        v.insert("street".into(), Cell::Str(s.into()));
    }
    v
}
async fn create(tx: &mut Transaction, v: RawVals) -> Records {
    model(tx).create(tx, vec![v]).await.unwrap()
}
async fn string(tx: &mut Transaction, r: &Records, field: &str) -> Option<String> {
    match r.value(tx, field).await.unwrap() {
        Value::Str(s) => Some(s.to_string()),
        Value::False => None,
        v => panic!("expected text/false, got {v:?}"),
    }
}

fn full_address(mut values: RawVals, address: [&str; 4]) -> RawVals {
    for (field, value) in ["street", "street2", "zip", "city"]
        .into_iter()
        .zip(address)
    {
        values.insert(field.into(), Cell::Str(value.into()));
    }
    values
}
async fn assert_address(tx: &mut Transaction, records: &Records, expected: [Option<&str>; 4]) {
    for (field, expected) in ["street", "street2", "zip", "city"]
        .into_iter()
        .zip(expected)
    {
        assert_eq!(
            string(tx, records, field).await.as_deref(),
            expected,
            "{field}"
        );
    }
}

#[tokio::test]
async fn contact_creation_copies_parent_address_and_context_default() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        full_address(
            vals("Invoice parent", None, None),
            ["Invoice Child Street", "Suite", "1000", "Invoice city"],
        )
        .set_cell("type", Cell::Str("invoice".into())),
    )
    .await;
    let child = create(&mut tx, vals("Child", Some(parent.ids()[0]), None)).await;
    assert_eq!(
        string(&mut tx, &child, "street").await.as_deref(),
        Some("Invoice Child Street")
    );
    let context = model(&tx).with_context([("default_parent_id", Value::Int(parent.ids()[0]))]);
    let default_child = context
        .create(&mut tx, vec![vals("Default child", None, None)])
        .await
        .unwrap();
    assert_eq!(
        string(&mut tx, &default_child, "street").await.as_deref(),
        Some("Invoice Child Street")
    );
    for records in [&child, &default_child] {
        assert_address(
            &mut tx,
            records,
            [
                Some("Invoice Child Street"),
                Some("Suite"),
                Some("1000"),
                Some("Invoice city"),
            ],
        )
        .await;
    }
    let empty_parent = create(&mut tx, vals("Empty parent", None, None)).await;
    let empty_child = create(
        &mut tx,
        vals("Empty child", Some(empty_parent.ids()[0]), None),
    )
    .await;
    assert_address(&mut tx, &empty_child, [None; 4]).await;
}

#[tokio::test]
async fn different_own_address_becomes_other_and_is_preserved() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        full_address(
            vals("Parent", None, None),
            ["A2", "Suite2", "2000", "City2"],
        ),
    )
    .await;
    let child = create(
        &mut tx,
        full_address(
            vals("Child", Some(parent.ids()[0]), None),
            ["A3", "Suite3", "3000", "City3"],
        ),
    )
    .await;
    assert_address(
        &mut tx,
        &parent,
        [Some("A2"), Some("Suite2"), Some("2000"), Some("City2")],
    )
    .await;
    assert_address(
        &mut tx,
        &child,
        [Some("A3"), Some("Suite3"), Some("3000"), Some("City3")],
    )
    .await;
    assert_eq!(
        string(&mut tx, &child, "type").await.as_deref(),
        Some("other")
    );
    assert_eq!(
        string(&mut tx, &child, "street").await.as_deref(),
        Some("A3")
    );
    assert_eq!(
        string(&mut tx, &parent, "street").await.as_deref(),
        Some("A2")
    );
}

#[tokio::test]
async fn contact_creation_fills_empty_parent_for_roots_with_and_without_vat() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for company in [false, true] {
        let mut v = vals("Empty", None, None);
        if company {
            v.insert("vat".into(), Cell::Str("X".into()));
        }
        let parent = create(&mut tx, v).await;
        let child = create(
            &mut tx,
            full_address(
                vals("Child", Some(parent.ids()[0]), None),
                ["A3", "Suite3", "3000", "City3"],
            ),
        )
        .await;
        for records in [&parent, &child] {
            assert_address(
                &mut tx,
                records,
                [Some("A3"), Some("Suite3"), Some("3000"), Some("City3")],
            )
            .await;
        }
        assert_eq!(
            string(&mut tx, &child, "type").await.as_deref(),
            Some("contact")
        );
        assert_eq!(
            string(&mut tx, &child, "street").await.as_deref(),
            Some("A3")
        );
        assert_eq!(
            string(&mut tx, &parent, "street").await.as_deref(),
            Some("A3")
        );
    }
}

#[tokio::test]
async fn ordered_parent_type_direct_child_and_upstream_changes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        full_address(
            vals("GhostStep", None, None),
            ["A1", "Suite1", "1000", "City"],
        ),
    )
    .await;
    let child = create(
        &mut tx,
        vals("Denis", None, None).set_cell("email", Cell::Str("denis@example.org".into())),
    )
    .await;
    let grandchild = create(&mut tx, vals("Grandchild", Some(child.ids()[0]), None)).await;
    child
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("parent_id", Cell::Id(parent.ids()[0]))
                .set_cell("phone", Cell::Str("123".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        string(&mut tx, &child, "street").await.as_deref(),
        Some("A1")
    );
    assert_eq!(
        string(&mut tx, &child, "email").await.as_deref(),
        Some("denis@example.org")
    );
    assert_eq!(
        string(&mut tx, &child, "phone").await.as_deref(),
        Some("123")
    );
    assert_eq!(
        string(&mut tx, &child, "type").await.as_deref(),
        Some("contact")
    );
    assert_eq!(string(&mut tx, &grandchild, "street").await, None);
    child
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("type", Cell::Str("invoice".into()))
                .set_cell("street", Cell::Str("Different".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        string(&mut tx, &child, "city").await.as_deref(),
        Some("City")
    );
    assert_eq!(
        string(&mut tx, &parent, "street").await.as_deref(),
        Some("A1")
    );
    assert_eq!(
        string(&mut tx, &grandchild, "street").await.as_deref(),
        Some("Different")
    );
    assert_address(
        &mut tx,
        &child,
        [
            Some("Different"),
            Some("Suite1"),
            Some("1000"),
            Some("City"),
        ],
    )
    .await;
    assert_address(
        &mut tx,
        &parent,
        [Some("A1"), Some("Suite1"), Some("1000"), Some("City")],
    )
    .await;
    assert_address(&mut tx, &grandchild, [Some("Different"), None, None, None]).await;
    child
        .write(
            &mut tx,
            RawVals::new().set_cell("type", Cell::Str("contact".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        string(&mut tx, &child, "street").await.as_deref(),
        Some("A1")
    );
    assert_eq!(
        string(&mut tx, &grandchild, "street").await.as_deref(),
        Some("Different")
    );
    assert_address(
        &mut tx,
        &child,
        [Some("A1"), Some("Suite1"), Some("1000"), Some("City")],
    )
    .await;
    assert_address(&mut tx, &grandchild, [Some("Different"), None, None, None]).await;
    let sibling = create(&mut tx, vals("Sibling", Some(parent.ids()[0]), None)).await;
    let auto_other = create(
        &mut tx,
        full_address(
            vals("Auto other", None, None),
            ["Own", "Own2", "9999", "Own city"],
        ),
    )
    .await;
    auto_other
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_id", Cell::Id(parent.ids()[0])),
        )
        .await
        .unwrap();
    assert_eq!(
        string(&mut tx, &auto_other, "type").await.as_deref(),
        Some("other")
    );
    let mut isolated = Vec::new();
    for ty in ["invoice", "delivery", "other"] {
        isolated.push(
            create(
                &mut tx,
                vals(ty, Some(parent.ids()[0]), Some(ty)).set_cell("type", Cell::Str(ty.into())),
            )
            .await,
        );
    }
    parent
        .write(
            &mut tx,
            full_address(RawVals::new(), ["A2", "Suite2", "2000", "City2"]),
        )
        .await
        .unwrap();
    for r in [&child, &sibling] {
        assert_address(
            &mut tx,
            r,
            [Some("A2"), Some("Suite2"), Some("2000"), Some("City2")],
        )
        .await;
    }
    assert_address(
        &mut tx,
        &auto_other,
        [Some("Own"), Some("Own2"), Some("9999"), Some("Own city")],
    )
    .await;
    for (r, ty) in isolated.iter().zip(["invoice", "delivery", "other"]) {
        assert_address(&mut tx, r, [Some(ty), None, None, None]).await;
    }
    assert_eq!(
        string(&mut tx, &grandchild, "street").await.as_deref(),
        Some("Different")
    );
    child
        .write(
            &mut tx,
            full_address(RawVals::new(), ["A3", "Suite3", "3000", "City3"]),
        )
        .await
        .unwrap();
    for r in [&parent, &child, &sibling, &grandchild] {
        assert_address(
            &mut tx,
            r,
            [Some("A3"), Some("Suite3"), Some("3000"), Some("City3")],
        )
        .await;
    }
}

#[tokio::test]
async fn parent_name_creates_root_without_renaming_existing_parent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let languages = tx.env().records("res.lang").unwrap();
    for (name, code, url) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        languages
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("code", Cell::Str(code.into()))
                        .set_cell("url_code", Cell::Str(url.into()))
                        .set_cell("active", Cell::Bool(true)),
                ],
            )
            .await
            .unwrap();
    }
    let records = model(&tx).with_context([("default_lang", Value::str("en_US"))]);
    let child = records
        .create(
            &mut tx,
            vec![
                full_address(
                    vals("Jean", None, None),
                    ["Rue", "Batiment", "75000", "Paris"],
                )
                .set_cell("parent_name", Cell::Str("Dupont".into()))
                .set_cell("vat", Cell::Str("FR123".into()))
                .set_cell("lang", Cell::Str("fr_FR".into())),
            ],
        )
        .await
        .unwrap();
    let parent = child.related(&mut tx, "parent_id").await.unwrap();
    assert!(
        parent
            .related(&mut tx, "parent_id")
            .await
            .unwrap()
            .is_empty()
    );
    for records in [&parent, &child] {
        assert_eq!(
            records.value(&mut tx, "lang").await.unwrap(),
            Value::str("fr_FR")
        );
        assert_address(
            &mut tx,
            records,
            [Some("Rue"), Some("Batiment"), Some("75000"), Some("Paris")],
        )
        .await;
    }

    assert_eq!(
        string(&mut tx, &parent, "name").await.as_deref(),
        Some("Dupont")
    );
    assert_eq!(
        string(&mut tx, &parent, "street").await.as_deref(),
        Some("Rue")
    );
    assert_eq!(
        string(&mut tx, &parent, "vat").await.as_deref(),
        Some("FR123")
    );
    let existing = create(&mut tx, vals("Existing", None, None)).await;
    create(
        &mut tx,
        vals("Second", Some(existing.ids()[0]), None)
            .set_cell("parent_name", Cell::Str("Must not rename".into())),
    )
    .await;
    assert_eq!(
        string(&mut tx, &existing, "name").await.as_deref(),
        Some("Existing")
    );
    let empty = create(
        &mut tx,
        vals("Empty name", None, None).set_cell("parent_name", Cell::Str("".into())),
    )
    .await;
    assert!(
        empty
            .related(&mut tx, "parent_id")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn five_contacts_address_write_obeys_warm_statement_contract() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        vals("Company", None, Some("Original")).set_cell("vat", Cell::Str("X".into())),
    )
    .await;
    let children = model(&tx)
        .create(
            &mut tx,
            (0..5)
                .map(|i| vals(&format!("Contact {i}"), Some(parent.ids()[0]), None))
                .collect(),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    children
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Warm Street".into())),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.capture_statements(true);
    children
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("New Street".into())),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    eprintln!(
        "address_write_statement_count={} statements={statements:#?}",
        statements.len()
    );
    assert!(
        statements.len() <= 14,
        "at most 14 statements, got {}: {statements:#?}",
        statements.len()
    );
    assert_eq!(
        string(&mut tx, &parent, "street").await.as_deref(),
        Some("New Street")
    );
    assert_eq!(
        children.values(&mut tx, "street").await.unwrap(),
        vec![Value::Str("New Street".into()); 5]
    );
}

#[tokio::test]
async fn parent_name_preserves_explicit_language_over_context_default() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let languages = tx.env().records("res.lang").unwrap();
    for (name, code, url) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        languages
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("code", Cell::Str(code.into()))
                        .set_cell("url_code", Cell::Str(url.into()))
                        .set_cell("active", Cell::Bool(true)),
                ],
            )
            .await
            .unwrap();
    }
    let records = model(&tx).with_context([("default_lang", Value::str("en_US"))]);
    let child = records
        .create(
            &mut tx,
            vec![
                vals("Jean", None, Some("Rue"))
                    .set_cell("parent_name", Cell::Str("Dupont".into()))
                    .set_cell("lang", Cell::Str("fr_FR".into())),
            ],
        )
        .await
        .unwrap();
    let parent = child.related(&mut tx, "parent_id").await.unwrap();
    assert_eq!(
        child.value(&mut tx, "lang").await.unwrap(),
        Value::str("fr_FR")
    );
    assert_eq!(
        parent.value(&mut tx, "lang").await.unwrap(),
        Value::str("fr_FR")
    );
    assert!(
        parent
            .related(&mut tx, "parent_id")
            .await
            .unwrap()
            .is_empty()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caller_skip_flags_do_not_disable_address_synchronization() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, vals("Parent", None, Some("Inherited"))).await;
    let context = model(&tx).with_context([("_partners_skip_fields_sync", Value::Bool(true))]);
    let child = context
        .create(&mut tx, vec![vals("Child", Some(parent.ids()[0]), None)])
        .await
        .unwrap();
    assert_eq!(
        string(&mut tx, &child, "street").await.as_deref(),
        Some("Inherited")
    );
    parent
        .with_context([("_partners_skip_fields_sync", Value::Bool(true))])
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Changed".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        string(&mut tx, &child, "street").await.as_deref(),
        Some("Changed")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn parent_name_reparents_nested_children_to_the_new_root_and_honors_default_parent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let child = create(
        &mut tx,
        vals("Person", None, Some("Home"))
            .set_cell("parent_name", Cell::Str("Root".into()))
            .set_commands(
                "child_ids",
                [erp_orm::Command::create(vals("Nested", None, None))],
            ),
    )
    .await;
    let root = child.related(&mut tx, "parent_id").await.unwrap();
    let siblings = root.related(&mut tx, "child_ids").await.unwrap();
    assert_eq!(siblings.ids().len(), 2);
    assert!(siblings.ids().contains(&child.ids()[0]));
    assert!(
        child
            .related(&mut tx, "child_ids")
            .await
            .unwrap()
            .is_empty()
    );
    let existing = create(&mut tx, vals("Existing default", None, None)).await;
    let count_before = model(&tx)
        .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
        .await
        .unwrap();
    let records = model(&tx).with_context([("default_parent_id", Value::Int(existing.ids()[0]))]);
    let default_child = records
        .create(
            &mut tx,
            vec![
                vals("Default child", None, None)
                    .set_cell("parent_name", Cell::Str("Ignored root".into())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        default_child
            .related(&mut tx, "parent_id")
            .await
            .unwrap()
            .ids(),
        existing.ids()
    );
    assert_eq!(
        string(&mut tx, &existing, "name").await.as_deref(),
        Some("Existing default")
    );
    assert_eq!(
        model(&tx)
            .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
            .await
            .unwrap(),
        count_before + 1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_parent_name_nested_create_cannot_commit_a_partial_root() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let error = model(&tx)
        .create(
            &mut tx,
            vec![
                vals("Person", None, None)
                    .set_cell("parent_name", Cell::Str("Must rollback".into()))
                    .set_commands(
                        "child_ids",
                        [erp_orm::Command::create(
                            RawVals::new().set_cell("type", Cell::Str("contact".into())),
                        )],
                    ),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The operation cannot be completed: Contacts require a name"
    );
    let driver = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<erp_testkit::tokio_postgres::Error>()
        .unwrap();
    let raw_message = driver.as_db_error().unwrap().message();
    let expected_commit =
        format!("transaction rolled back: an earlier statement failed ({raw_message})");
    let commit_error = tx.commit().await.unwrap_err();
    // An aborted PostgreSQL transaction takes precedence over the ORM poison.
    assert_eq!(commit_error.kind(), erp_sdk::ErrorKind::Database);
    assert_eq!(commit_error.message(), expected_commit);
    drop(tx);
    let mut verify = h.begin().await.unwrap();
    let roots = model(&verify)
        .search(
            &mut verify,
            &erp_sdk::Domain::condition("name", erp_sdk::Operator::Eq, "Must rollback").unwrap(),
        )
        .await
        .unwrap();
    assert!(roots.is_empty());
    drop(verify);
    h.cleanup().await.unwrap();
}

// Focused ADDRESS cross-node proof; does not claim the full INTEGRATE-owned case081.
#[tokio::test]
async fn independent_node_observes_committed_addresses_and_no_aborted_changes() {
    use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
    use erp_testkit::tokio_postgres::NoTls;
    let h = harness().await;
    let manager = Manager::from_config(
        h.db().config(),
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
    let node = erp_orm::Orm::builder(std::sync::Arc::clone(h.registry()), pool)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let parent = create(&mut setup, vals("Node parent", None, Some("Committed"))).await;
    let child = create(&mut setup, vals("Node child", Some(parent.ids()[0]), None)).await;
    let ids = [parent.ids()[0], child.ids()[0]];
    setup.commit().await.unwrap();
    let mut reader = node.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = model(&reader).browse(ids).unwrap();
    assert_eq!(
        records.values(&mut reader, "street").await.unwrap(),
        vec![Value::str("Committed"); 2]
    );
    reader.commit().await.unwrap();
    let mut aborted = h.begin().await.unwrap();
    model(&aborted)
        .browse([ids[1]])
        .unwrap()
        .write(
            &mut aborted,
            RawVals::new().set_cell("street", Cell::Str("Aborted".into())),
        )
        .await
        .unwrap();
    aborted.flush().await.unwrap();
    aborted.rollback().await.unwrap();
    drop(aborted);
    let mut check = node.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = model(&check).browse(ids).unwrap();
    assert_eq!(
        records.values(&mut check, "street").await.unwrap(),
        vec![Value::str("Committed"); 2]
    );
    check.commit().await.unwrap();
    drop(node);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn context_parent_name_is_consumed_and_cannot_rename_an_existing_parent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let context = model(&tx).with_context([("default_parent_name", Value::str("Context root"))]);
    let child = context
        .create(
            &mut tx,
            vec![vals("Context child", None, Some("Context street"))],
        )
        .await
        .unwrap();
    let parent = child.related(&mut tx, "parent_id").await.unwrap();
    assert_eq!(
        string(&mut tx, &parent, "name").await.as_deref(),
        Some("Context root")
    );
    let second = context
        .with_context([
            ("default_parent_id", Value::Int(parent.ids()[0])),
            ("default_parent_name", Value::str("Must not rename")),
        ])
        .create(&mut tx, vec![vals("Existing parent child", None, None)])
        .await
        .unwrap();
    assert_eq!(
        second.related(&mut tx, "parent_id").await.unwrap().ids(),
        parent.ids()
    );
    assert_eq!(
        string(&mut tx, &parent, "name").await.as_deref(),
        Some("Context root")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn synchronization_depth_fixture(tx: &mut Transaction, edges: i32) -> (Records, Records) {
    let root = create(tx, vals("Depth root", None, Some("Old"))).await;
    tx.flush().await.unwrap();
    tx.execute("WITH source AS MATERIALIZED (SELECT nextval(pg_get_serial_sequence('res_partner', 'id')) AS id, n FROM generate_series(1,$2::int4) AS n), links AS (SELECT id, COALESCE(lag(id) OVER (ORDER BY n), $1) AS parent_id, n FROM source) INSERT INTO res_partner (id, name, type, active, parent_id, is_company, street) SELECT id, 'sync-depth-' || n::text, 'contact', true, parent_id, false, 'Old' FROM links", &[&root.ids()[0], &edges]).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let rows = tx
        .query(
            "SELECT id FROM res_partner WHERE name = $1",
            &[&format!("sync-depth-{edges}")],
        )
        .await
        .unwrap();
    let leaf = model(tx).browse([rows[0].get::<_, i64>(0)]).unwrap();
    (root, leaf)
}

#[tokio::test]
async fn synchronization_accepts_exact_256_edge_depth() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (root, leaf) = synchronization_depth_fixture(&mut tx, 256).await;
    leaf.write(
        &mut tx,
        RawVals::new().set_cell("street", Cell::Str("At limit".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        string(&mut tx, &root, "street").await.as_deref(),
        Some("At limit")
    );
    assert_eq!(
        string(&mut tx, &leaf, "street").await.as_deref(),
        Some("At limit")
    );
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn synchronization_refuses_257_edge_depth_and_cannot_commit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, leaf) = synchronization_depth_fixture(&mut tx, 257).await;
    let error = leaf
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Beyond limit".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Partner address maximum depth of 256 exceeded."
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), error.kind());
    assert_eq!(commit_error.message(), error.message());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn batch_parent_name_keeps_distinct_language_vat_and_address_maps() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let languages = tx.env().records("res.lang").unwrap();
    for (name, code, url) in [("English", "en_US", "en"), ("French", "fr_FR", "fr")] {
        languages
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("code", Cell::Str(code.into()))
                        .set_cell("url_code", Cell::Str(url.into()))
                        .set_cell("active", Cell::Bool(true)),
                ],
            )
            .await
            .unwrap();
    }
    let inputs = [
        (
            "First child",
            "First root",
            "fr_FR",
            "FR111",
            ["First street", "First suite", "11111", "First city"],
        ),
        (
            "Second child",
            "Second root",
            "en_US",
            "EN222",
            ["Second street", "Second suite", "22222", "Second city"],
        ),
    ];
    let records = model(&tx).with_context([("default_lang", Value::str("en_US"))]);
    let children = records
        .create(
            &mut tx,
            inputs
                .iter()
                .map(|(child, parent, lang, vat, address)| {
                    full_address(vals(child, None, None), *address)
                        .set_cell("parent_name", Cell::Str((*parent).into()))
                        .set_cell("lang", Cell::Str((*lang).into()))
                        .set_cell("vat", Cell::Str((*vat).into()))
                })
                .collect(),
        )
        .await
        .unwrap();
    assert_eq!(children.ids().len(), 2);
    let mut parent_ids = Vec::new();
    for (child, (child_name, parent_name, lang, vat, address)) in children.iter().zip(inputs) {
        let parent = child.related(&mut tx, "parent_id").await.unwrap();
        assert_eq!(parent.ids().len(), 1);
        parent_ids.push(parent.ids()[0]);
        assert!(
            parent
                .related(&mut tx, "parent_id")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            string(&mut tx, &child, "name").await.as_deref(),
            Some(child_name)
        );
        assert_eq!(
            string(&mut tx, &parent, "name").await.as_deref(),
            Some(parent_name)
        );
        for record in [&child, &parent] {
            assert_eq!(string(&mut tx, record, "lang").await.as_deref(), Some(lang));
            assert_eq!(string(&mut tx, record, "vat").await.as_deref(), Some(vat));
            assert_address(&mut tx, record, address.map(Some)).await;
        }
    }
    assert_ne!(parent_ids[0], parent_ids[1]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn explicit_contact_address_clearing_flows_up_to_siblings_and_direct_child() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        full_address(
            vals("Root", None, None),
            ["Street", "Suite", "12345", "City"],
        ),
    )
    .await;
    let child = create(&mut tx, vals("Child", Some(parent.ids()[0]), None)).await;
    let sibling = create(&mut tx, vals("Sibling", Some(parent.ids()[0]), None)).await;
    let grandchild = create(&mut tx, vals("Grandchild", Some(child.ids()[0]), None)).await;
    let other = create(
        &mut tx,
        vals("Other", Some(parent.ids()[0]), None)
            .set_cell("type", Cell::Str("other".into()))
            .set_cell("zip", Cell::Str("Keep".into())),
    )
    .await;
    child
        .write(&mut tx, RawVals::new().set_cell("zip", Cell::Null))
        .await
        .unwrap();
    for record in [&parent, &child, &sibling, &grandchild] {
        assert_address(
            &mut tx,
            record,
            [Some("Street"), Some("Suite"), None, Some("City")],
        )
        .await;
    }
    assert_eq!(
        string(&mut tx, &other, "zip").await.as_deref(),
        Some("Keep")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn quick_created_contact_keeps_email_during_parent_address_assignment() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(
        &mut tx,
        full_address(
            vals("Root", None, None),
            ["Street", "Suite", "12345", "City"],
        ),
    )
    .await;
    let records = model(&tx);
    let (child_id, _) = records
        .name_create(&mut tx, "Denis Bladesmith <denis.bladesmith@example.org>")
        .await
        .unwrap();
    let child = records.browse([child_id]).unwrap();
    let grandchild = create(&mut tx, vals("Grandchild", Some(child_id), None)).await;
    child
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("parent_id", Cell::Id(parent.ids()[0]))
                .set_cell("phone", Cell::Str("123".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        string(&mut tx, &child, "email").await.as_deref(),
        Some("denis.bladesmith@example.org")
    );
    assert_eq!(
        string(&mut tx, &child, "phone").await.as_deref(),
        Some("123")
    );
    assert_eq!(
        string(&mut tx, &child, "type").await.as_deref(),
        Some("contact")
    );
    assert_address(
        &mut tx,
        &child,
        [Some("Street"), Some("Suite"), Some("12345"), Some("City")],
    )
    .await;
    assert_address(&mut tx, &grandchild, [None; 4]).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn batch_create_finishes_each_contacts_upstream_phase_before_the_next_contact() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, vals("Empty root", None, None)).await;
    let children = model(&tx)
        .create(
            &mut tx,
            vec![
                vals("First", Some(parent.ids()[0]), Some("First address")),
                vals("Second", Some(parent.ids()[0]), Some("Second address")),
            ],
        )
        .await
        .unwrap();
    for child in children.iter() {
        assert_eq!(
            string(&mut tx, &child, "type").await.as_deref(),
            Some("contact")
        );
        assert_eq!(
            string(&mut tx, &child, "street").await.as_deref(),
            Some("First address")
        );
    }
    assert_eq!(
        string(&mut tx, &parent, "street").await.as_deref(),
        Some("First address")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn synchronization_admits_4096_distinct_nodes_then_refuses_one_more() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, vals("Wide sync root", None, Some("Old"))).await;
    tx.flush().await.unwrap();
    let child_count = 4095_i32;
    tx.execute(
        "INSERT INTO res_partner (name, type, active, parent_id, is_company, street) SELECT 'sync-width-' || n::text, 'contact', true, $1, false, 'Old' FROM generate_series(1,$2::int4) AS n",
        &[&root.ids()[0], &child_count],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    root.write(
        &mut tx,
        RawVals::new().set_cell("street", Cell::Str("At visited limit".into())),
    )
    .await
    .unwrap();
    let children = root.related(&mut tx, "child_ids").await.unwrap();
    assert_eq!(children.ids().len(), 4095);
    assert_eq!(
        children.values(&mut tx, "street").await.unwrap(),
        vec![Value::str("At visited limit"); 4095]
    );
    assert_eq!(
        string(&mut tx, &root, "street").await.as_deref(),
        Some("At visited limit")
    );
    tx.flush().await.unwrap();
    tx.execute(
        "INSERT INTO res_partner (name, type, active, parent_id, is_company, street) VALUES ('sync-width-extra', 'contact', true, $1, false, 'Old')",
        &[&root.ids()[0]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let error = root
        .write(
            &mut tx,
            RawVals::new().set_cell("street", Cell::Str("Beyond visited limit".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Partner address maximum visited nodes limit of 4096 exceeded."
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), error.kind());
    assert_eq!(commit_error.message(), error.message());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn independent_bulk_create_does_not_charge_input_fields_as_address_copies() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let inputs = (0..4096)
        .map(|index| {
            RawVals::new()
                .set_cell("name", Cell::Str(format!("Independent {index}").into()))
                .set_cell("ref", Cell::Str("Bulk reference".into()))
                .set_cell("vat", Cell::Str("BULK".into()))
                .set_cell("website", Cell::Str("https://example.org".into()))
                .set_cell("active", Cell::Bool(true))
                .set_cell("type", Cell::Str("contact".into()))
                .set_cell("street", Cell::Str("Bulk street".into()))
                .set_cell("street2", Cell::Str("Bulk suite".into()))
                .set_cell("zip", Cell::Str("12345".into()))
                .set_cell("city", Cell::Str("Bulk city".into()))
                .set_cell("email", Cell::Str("bulk@example.org".into()))
                .set_cell("phone", Cell::Str("123".into()))
                .set_cell("industry_id", Cell::Null)
                .set_cell("company_id", Cell::Null)
                .set_cell("color", Cell::Int(1))
                .set_cell("lang", Cell::Null)
                .set_cell("tz", Cell::Null)
        })
        .collect();
    let records = model(&tx).create(&mut tx, inputs).await.unwrap();
    assert_eq!(records.len(), 4096);
    assert_eq!(
        records.cells(&mut tx, "parent_id").await.unwrap(),
        vec![Cell::Null; 4096]
    );
    let ids = records.ids();
    tx.commit().await.unwrap();
    let mut verify = h.begin().await.unwrap();
    let stored = model(&verify).browse(ids).unwrap();
    assert_eq!(
        stored.values(&mut verify, "street").await.unwrap(),
        vec![Value::str("Bulk street"); 4096]
    );
    assert_eq!(
        stored.values(&mut verify, "ref").await.unwrap(),
        vec![Value::str("Bulk reference"); 4096]
    );
    drop(verify);
    h.cleanup().await.unwrap();
}

// The R2 production fix already has historical RED/GREEN evidence. These added
// variants complete coverage; they do not claim another failing pre-fix run.
#[tokio::test]
async fn parent_name_does_not_replace_an_invalid_native_parent_cell() {
    let h = harness().await;
    for cell in [
        Cell::Int(42),
        Cell::Bool(false),
        Cell::Str("42".into()),
        Cell::Float(42.0),
        Cell::Ids(Vec::new().into()),
    ] {
        for name_source in ["explicit", "absent", "context"] {
            let mut tx = h.begin().await.unwrap();
            let records = model(&tx);
            let before = records
                .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
                .await
                .unwrap();
            let mut values = vals("Invalid child", None, None).set_cell("parent_id", cell.clone());
            let caller = match name_source {
                "explicit" => {
                    values.insert(
                        "parent_name".into(),
                        Cell::Str("Must not be created".into()),
                    );
                    records.clone()
                }
                "context" => records
                    .with_context([("default_parent_name", Value::str("Must not be created"))]),
                "absent" => records.clone(),
                _ => unreachable!(),
            };
            let error = caller.create(&mut tx, vec![values]).await.unwrap_err();
            assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
            assert_eq!(
                error.message(),
                format!("invalid value {cell:?} for field res.partner.parent_id of type many2one"),
                "parent_name source: {name_source}"
            );
            assert_eq!(
                records
                    .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
                    .await
                    .unwrap(),
                before
            );
            assert!(!tx.is_poisoned(), "pre-mutation Programming refusal");
            tx.commit().await.unwrap();
            drop(tx);
            // A count read through the rejecting transaction alone cannot prove persistence.
            let mut verify = h.begin().await.unwrap();
            assert_eq!(
                model(&verify)
                    .search_count(&mut verify, &erp_sdk::Domain::TRUE, None)
                    .await
                    .unwrap(),
                before
            );
        }
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn valid_native_parent_cells_and_defaults_persist_without_renaming() {
    #[derive(Clone, Copy, Debug)]
    enum ParentInput {
        Absent,
        Null,
        ExplicitId,
        DefaultId,
        NullOverDefault,
    }
    let h = harness().await;
    for input in [
        ParentInput::Absent,
        ParentInput::Null,
        ParentInput::ExplicitId,
        ParentInput::DefaultId,
        ParentInput::NullOverDefault,
    ] {
        for name_source in ["explicit", "absent", "context"] {
            let mut tx = h.begin().await.unwrap();
            let existing = create(&mut tx, vals("Existing root", None, None)).await;
            let existing_id = existing.ids()[0];
            let child_name = format!("Valid child {input:?} {name_source}");
            let root_name = format!("Requested root {input:?} {name_source}");
            let mut values = vals(&child_name, None, None);
            let mut caller = model(&tx);
            match input {
                ParentInput::Absent => {}
                ParentInput::Null => {
                    values.insert("parent_id".into(), Cell::Null);
                }
                ParentInput::ExplicitId => {
                    values.insert("parent_id".into(), Cell::Id(existing_id));
                }
                ParentInput::DefaultId | ParentInput::NullOverDefault => {
                    caller = caller.with_context([("default_parent_id", Value::Int(existing_id))]);
                    if matches!(input, ParentInput::NullOverDefault) {
                        values.insert("parent_id".into(), Cell::Null);
                    }
                }
            }
            match name_source {
                "explicit" => {
                    values.insert("parent_name".into(), Cell::Str(root_name.clone().into()));
                }
                "context" => {
                    caller = caller.with_context([("default_parent_name", Value::str(&root_name))]);
                }
                "absent" => {}
                _ => unreachable!(),
            }
            let child = caller.create(&mut tx, vec![values]).await.unwrap();
            let parent = child.related(&mut tx, "parent_id").await.unwrap();
            let uses_existing = matches!(input, ParentInput::ExplicitId | ParentInput::DefaultId);
            let creates_root = !uses_existing && name_source != "absent";
            if uses_existing {
                assert_eq!(parent.ids(), existing.ids());
            } else if creates_root {
                assert_eq!(parent.len(), 1);
                assert_ne!(parent.ids(), existing.ids());
            } else {
                assert!(parent.is_empty());
            }
            let child_id = child.ids()[0];
            let parent_ids = parent.ids();
            tx.commit().await.unwrap();
            drop(tx);
            let mut verify = h.begin().await.unwrap();
            let stored = model(&verify).browse([child_id]).unwrap();
            assert_eq!(string(&mut verify, &stored, "name").await, Some(child_name));
            let stored_parent = stored.related(&mut verify, "parent_id").await.unwrap();
            assert_eq!(stored_parent.ids(), parent_ids);
            if !stored_parent.is_empty() {
                assert_eq!(
                    string(&mut verify, &stored_parent, "name").await,
                    Some(if creates_root {
                        root_name
                    } else {
                        "Existing root".to_owned()
                    })
                );
            }
            let original = model(&verify).browse([existing_id]).unwrap();
            assert_eq!(
                string(&mut verify, &original, "name").await.as_deref(),
                Some("Existing root")
            );
        }
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn malformed_parent_after_an_earlier_batch_root_poisons_and_rolls_back() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = model(&tx);
    let before = records
        .search_count(&mut tx, &erp_sdk::Domain::TRUE, None)
        .await
        .unwrap();
    let error = records
        .create(
            &mut tx,
            vec![
                vals("Earlier child", None, None).set_cell(
                    "parent_name",
                    Cell::Str("Earlier root must rollback".into()),
                ),
                vals("Later invalid child", None, None)
                    .set_cell("parent_name", Cell::Str("Later root must not exist".into()))
                    .set_cell("parent_id", Cell::Int(42)),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "invalid value Int(42) for field res.partner.parent_id of type many2one"
    );
    // Show that this is genuinely a partial-mutation fixture, not a batch that was
    // rejected wholesale before its first parent was created.
    let earlier = records
        .search(
            &mut tx,
            &erp_sdk::Domain::condition(
                "name",
                erp_sdk::Operator::Eq,
                "Earlier root must rollback",
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(earlier.len(), 1);
    assert!(tx.is_poisoned());
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), error.kind());
    assert_eq!(commit_error.message(), error.message());
    tx.rollback().await.unwrap();
    drop(tx);
    let mut verify = h.begin().await.unwrap();
    assert_eq!(
        model(&verify)
            .search_count(&mut verify, &erp_sdk::Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    drop(verify);
    h.cleanup().await.unwrap();
}
