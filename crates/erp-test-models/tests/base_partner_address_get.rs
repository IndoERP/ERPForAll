//! Independently authored address selection and bounded-input acceptance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_orm::{Records, Transaction};
use erp_sdk::{Cell, Domain, ErrorKind, Operator, RawVals, Value, ValueMap};
use erp_test_models::base_users;
use erp_testkit::{DomainPolicy, Harness, Otherwise};

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}

fn partners(tx: &Transaction) -> Records {
    tx.env().records("res.partner").unwrap()
}

async fn create(tx: &mut Transaction, name: &str, kind: &str, parent: Option<i64>) -> i64 {
    let mut values = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("type", Cell::Str(kind.into()));
    if let Some(parent) = parent {
        values = values.set_cell("parent_id", Cell::Id(parent));
    }
    partners(tx).create(tx, vec![values]).await.unwrap().ids()[0]
}

async fn resolve(tx: &mut Transaction, records: &Records, preferences: &[&str]) -> ValueMap {
    erp_orm::call_method(
        tx,
        records,
        "address_get",
        (preferences
            .iter()
            .map(|kind| (*kind).to_owned())
            .collect::<Vec<_>>(),),
    )
    .await
    .unwrap()
}

fn expected(entries: &[(&str, i64)]) -> ValueMap {
    entries
        .iter()
        .map(|(key, id)| ((*key).to_owned(), Value::Int(*id)))
        .collect()
}

#[tokio::test]
async fn spec_partner_034_downward_depth_first_then_ancestor_resolution() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Elmtree", "contact", None).await;
    let first = create(&mut tx, "Branch 1", "contact", Some(root)).await;
    let invoice = create(&mut tx, "Leaf 10", "invoice", Some(first)).await;
    let other = create(&mut tx, "Branch 11", "other", Some(first)).await;
    let first_delivery = create(&mut tx, "Leaf 111", "delivery", Some(other)).await;
    let second = create(&mut tx, "Branch 2", "contact", Some(root)).await;
    let second_delivery = create(&mut tx, "Leaf 21", "delivery", Some(second)).await;
    let second_contact = create(&mut tx, "Leaf 22", "contact", Some(second)).await;
    let third_contact = create(&mut tx, "Leaf 23", "contact", Some(second)).await;
    for (input, delivery, contact) in [
        (root, first_delivery, root),
        (first, first_delivery, first),
        (invoice, first_delivery, first),
        (other, first_delivery, first),
        (first_delivery, first_delivery, first),
        (second, second_delivery, second),
        (second_delivery, second_delivery, second),
        (second_contact, second_delivery, second_contact),
        (third_contact, second_delivery, third_contact),
    ] {
        let records = partners(&tx).browse([input]).unwrap();
        assert_eq!(
            resolve(
                &mut tx,
                &records,
                &["delivery", "invoice", "contact", "other"]
            )
            .await,
            expected(&[
                ("delivery", delivery),
                ("invoice", invoice),
                ("contact", contact),
                ("other", other)
            ])
        );
    }
    let root_records = partners(&tx).browse([root]).unwrap();
    assert_eq!(
        resolve(&mut tx, &root_records, &[]).await,
        expected(&[("contact", root)])
    );
    let delivery_records = partners(&tx).browse([first_delivery]).unwrap();
    assert_eq!(
        resolve(&mut tx, &delivery_records, &[]).await,
        expected(&[("contact", first)])
    );
    partners(&tx)
        .browse([other])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("type", Cell::Str("contact".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        resolve(&mut tx, &delivery_records, &[]).await,
        expected(&[("contact", other)])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_076_address_get_cold_fixture_statement_ceiling() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Selection company", "contact", None).await;
    partners(&tx)
        .browse([root])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("vat", Cell::Str("SELECT-1".into())),
        )
        .await
        .unwrap();
    let invoice = create(&mut tx, "Invoice", "invoice", Some(root)).await;
    let delivery = create(&mut tx, "Delivery", "delivery", Some(root)).await;
    create(&mut tx, "Contact 1", "contact", Some(root)).await;
    create(&mut tx, "Contact 2", "contact", Some(root)).await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let records = partners(&tx).browse([root]).unwrap();
    tx.capture_statements(true);
    let result = resolve(&mut tx, &records, &["delivery", "invoice", "contact"]).await;
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    assert_eq!(
        result,
        expected(&[
            ("delivery", delivery),
            ("invoice", invoice),
            ("contact", root)
        ])
    );
    eprintln!(
        "address_get_statement_count={} statements={statements:#?}",
        statements.len()
    );
    assert!(
        statements.len() <= 4,
        "address_get exceeded four statements: {statements:#?}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn preferences_support_unknown_types_duplicates_empty_and_multiple_inputs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = create(&mut tx, "First", "contact", None).await;
    let second = create(&mut tx, "Second", "contact", None).await;
    let records = partners(&tx).browse([second, first]).unwrap();
    assert_eq!(
        resolve(&mut tx, &records, &["unknown", "unknown"]).await,
        expected(&[("contact", second), ("unknown", second)])
    );
    let duplicates = partners(&tx).browse([second, second, first]).unwrap();
    assert_eq!(
        resolve(&mut tx, &duplicates, &["unknown"]).await,
        expected(&[("contact", second), ("unknown", second)])
    );
    let empty = partners(&tx);
    let result = resolve(&mut tx, &empty, &["invoice"]).await;
    assert_eq!(result.get("contact"), Some(&Value::False));
    assert_eq!(result.get("invoice"), Some(&Value::False));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn preference_admission_refuses_entry_and_byte_overflow() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = partners(&tx);
    for (preferences, message) in [
        (
            vec!["invoice".to_owned(); 257],
            "Partner address preferences exceed the 256-entry limit",
        ),
        (
            vec!["x".repeat(65537)],
            "Partner address preferences exceed the 65536-byte limit",
        ),
    ] {
        let error =
            erp_orm::call_method::<_, ValueMap>(&mut tx, &records, "address_get", (preferences,))
                .await
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), message);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn children_use_model_order_and_hide_archived_or_denied_records() {
    let domain = Domain::condition("ref", Operator::Ne, "blocked").unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("res.partner", erp_orm::Operation::Read, domain)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'Address reader', 'address-reader', true, false)").await.unwrap();
    let root = create(&mut tx, "Root", "contact", None).await;
    let older = create(&mut tx, "Same", "delivery", Some(root)).await;
    let newer = create(&mut tx, "Same", "delivery", Some(root)).await;
    let archived = create(&mut tx, "AAA archived", "delivery", Some(root)).await;
    partners(&tx)
        .browse([archived])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    let hidden = create(&mut tx, "AAA hidden", "invoice", Some(root)).await;
    partners(&tx)
        .browse([hidden])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("ref", Cell::Str("blocked".into())),
        )
        .await
        .unwrap();
    let reader = partners(&tx).with_user(2_i64).browse([root]).unwrap();
    assert_eq!(
        resolve(&mut tx, &reader, &["delivery", "invoice"]).await,
        expected(&[("contact", root), ("delivery", newer), ("invoice", root)])
    );
    assert_ne!(older, newer);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_boundary_blocks_child_subtrees_and_ancestor_escape() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Root", "contact", None).await;
    let child_company = create(&mut tx, "Company branch", "other", Some(root)).await;
    let invoice = create(&mut tx, "Invoice inside", "invoice", Some(child_company)).await;
    // Simulate an addon-refined stored company compute; the helper must obey the
    // stored result, rather than assume every descendant is a non-company.
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE res_partner SET is_company = true WHERE id = $1",
        &[&child_company],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let outer = partners(&tx).browse([root]).unwrap();
    assert_eq!(
        resolve(&mut tx, &outer, &["invoice"]).await,
        expected(&[("contact", root), ("invoice", root)])
    );
    let inner = partners(&tx).browse([child_company]).unwrap();
    assert_eq!(
        resolve(&mut tx, &inner, &["invoice", "delivery"]).await,
        expected(&[
            ("contact", child_company),
            ("invoice", invoice),
            ("delivery", child_company)
        ])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn multiple_noncontacts_refuse_ambiguous_singleton_fallback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = create(&mut tx, "Invoice first", "invoice", None).await;
    let second = create(&mut tx, "Invoice second", "invoice", None).await;
    let records = partners(&tx).browse([first, second]).unwrap();
    let error = erp_orm::call_method::<_, ValueMap>(
        &mut tx,
        &records,
        "address_get",
        (Vec::<String>::new(),),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "res.partner.address_get: expected one record for contact fallback, got 2"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn traversal_refuses_depth_overflow_without_unbounded_recursion() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Deep root", "contact", None).await;
    tx.flush().await.unwrap();
    // Integrity-corruption fixture bypasses the existing create-time hierarchy limit
    // so this test reaches address selection's independent admission boundary.
    tx.execute(
        "WITH source AS MATERIALIZED (SELECT nextval(pg_get_serial_sequence('res_partner', 'id')) AS id, n FROM generate_series(1,257) AS n), links AS (SELECT id, COALESCE(lag(id) OVER (ORDER BY n), $1) AS parent_id, n FROM source) INSERT INTO res_partner (id, name, type, active, parent_id, is_company, partner_share) SELECT id, 'deep-' || n::text, 'other', true, parent_id, false, false FROM links",
        &[&root],
    ).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let records = partners(&tx).browse([root]).unwrap();
    let error = erp_orm::call_method::<_, ValueMap>(
        &mut tx,
        &records,
        "address_get",
        (vec!["invoice".to_owned()],),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Partner address maximum depth of 256 exceeded."
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn traversal_refuses_visited_identity_overflow_before_unbounded_expansion() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Wide root", "contact", None).await;
    tx.flush().await.unwrap();
    // A broad hierarchy is legal to store but exceeds the bounded selection call.
    tx.execute(
        "INSERT INTO res_partner (name, type, active, parent_id, is_company, partner_share) SELECT 'wide-' || n::text, 'other', true, $1, false, false FROM generate_series(1,4096) AS n",
        &[&root],
    ).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let records = partners(&tx).browse([root]).unwrap();
    let error = erp_orm::call_method::<_, ValueMap>(
        &mut tx,
        &records,
        "address_get",
        (vec!["invoice".to_owned()],),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Partner address maximum visited nodes limit of 4096 exceeded."
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn preference_admission_accepts_exact_entry_and_byte_limits() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = partners(&tx);
    let duplicate_preferences = vec!["invoice".to_owned(); 256];
    let entries: ValueMap =
        erp_orm::call_method(&mut tx, &records, "address_get", (duplicate_preferences,))
            .await
            .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries.get("contact"), Some(&Value::False));
    assert_eq!(entries.get("invoice"), Some(&Value::False));
    let large_preference = "é".repeat(32768);
    let bytes: ValueMap = erp_orm::call_method(
        &mut tx,
        &records,
        "address_get",
        (vec![large_preference.clone()],),
    )
    .await
    .unwrap();
    assert_eq!(bytes.len(), 2);
    assert_eq!(bytes.get("contact"), Some(&Value::False));
    assert_eq!(bytes.get(&large_preference), Some(&Value::False));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn traversal_accepts_exact_depth_limit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Exact deep root", "contact", None).await;
    tx.flush().await.unwrap();
    // Root is depth zero. This independently injects exactly 256 child edges.
    tx.execute(
        "WITH source AS MATERIALIZED (SELECT nextval(pg_get_serial_sequence('res_partner', 'id')) AS id, n FROM generate_series(1,256) AS n), links AS (SELECT id, COALESCE(lag(id) OVER (ORDER BY n), $1) AS parent_id, n FROM source) INSERT INTO res_partner (id, name, type, active, parent_id, is_company, partner_share) SELECT id, 'exact-deep-' || n::text, 'other', true, parent_id, false, false FROM links",
        &[&root],
    ).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let records = partners(&tx).browse([root]).unwrap();
    assert_eq!(
        resolve(&mut tx, &records, &["invoice"]).await,
        expected(&[("contact", root), ("invoice", root)])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn traversal_accepts_exact_visited_identity_limit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, "Exact wide root", "contact", None).await;
    tx.flush().await.unwrap();
    // 4095 children plus the root are exactly 4096 distinct partner identities.
    tx.execute(
        "INSERT INTO res_partner (name, type, active, parent_id, is_company, partner_share) SELECT 'exact-wide-' || n::text, 'other', true, $1, false, false FROM generate_series(1,4095) AS n",
        &[&root],
    ).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let records = partners(&tx).browse([root]).unwrap();
    assert_eq!(
        resolve(&mut tx, &records, &["invoice"]).await,
        expected(&[("contact", root), ("invoice", root)])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn recursive_hierarchy_is_refused_instead_of_silently_using_fallback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = create(&mut tx, "Cycle first", "other", None).await;
    let second = create(&mut tx, "Cycle second", "other", Some(first)).await;
    tx.flush().await.unwrap();
    // Integrity-corruption fixture reaches the selection guard independently of
    // ordinary create/write hierarchy validation.
    tx.execute(
        "UPDATE res_partner SET parent_id = $1, is_company = false WHERE id = $2",
        &[&second, &first],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let records = partners(&tx).browse([first]).unwrap();
    let error = erp_orm::call_method::<_, ValueMap>(
        &mut tx,
        &records,
        "address_get",
        (vec!["invoice".to_owned()],),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "res.partner.address_get: recursive partner hierarchy"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nullable_address_type_uses_singleton_fallback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let record = partners(&tx)
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Untyped partner".into()))
                    .set_cell("type", Cell::Null),
            ],
        )
        .await
        .unwrap();
    let id = record.ids()[0];
    assert_eq!(
        resolve(&mut tx, &record, &["invoice", "contact"]).await,
        expected(&[("invoice", id), ("contact", id)])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn directly_denied_input_and_ancestor_return_access_instead_of_fallback() {
    let domain = Domain::condition("ref", Operator::Ne, "blocked").unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("res.partner", erp_orm::Operation::Read, domain)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    for through_child in [false, true] {
        let mut tx = h.begin().await.unwrap();
        tx.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'Address reader', 'address-reader', true, false)").await.unwrap();
        let hidden = create(&mut tx, "Hidden ancestor", "other", None).await;
        partners(&tx)
            .browse([hidden])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("ref", Cell::Str("blocked".into())),
            )
            .await
            .unwrap();
        let input = if through_child {
            create(&mut tx, "Visible invoice", "invoice", Some(hidden)).await
        } else {
            hidden
        };
        let reader = partners(&tx).with_user(2_i64).browse([input]).unwrap();
        let error = erp_orm::call_method::<_, ValueMap>(
            &mut tx,
            &reader,
            "address_get",
            (vec!["delivery".to_owned()],),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            format!(
                "You are not allowed to access 'Contact' (res.partner) records.\n\nRecords: res.partner({hidden},), User: 2"
            )
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}
