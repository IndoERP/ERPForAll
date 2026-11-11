//! Commercial ancestry of cache-only partners, including default-derived records.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_orm::{Records, Transaction};
use erp_sdk::{Cell, Domain, RawVals, RecId, Value};
use erp_test_models::base_users;
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .expect("partner harness")
}

fn named(name: &str) -> RawVals {
    RawVals::new().set_cell("name", Cell::Str(name.into()))
}

async fn assert_ancestry(
    tx: &mut Transaction,
    partner: &Records,
    root: RecId,
    root_name: &str,
    complete_name: &str,
) {
    let before = tx.query_count();
    assert_eq!(
        partner.cell(tx, "commercial_partner_id").await.unwrap(),
        Cell::Id(root.get())
    );
    if root.is_new() {
        assert_eq!(
            tx.query_count(),
            before,
            "virtual commercial ancestry is cache-only"
        );
    }
    assert_eq!(
        partner.value(tx, "commercial_company_name").await.unwrap(),
        Value::str(root_name)
    );
    assert_eq!(
        partner.value(tx, "complete_name").await.unwrap(),
        Value::str(complete_name)
    );
    assert_eq!(
        partner.value(tx, "display_name").await.unwrap(),
        Value::str(complete_name)
    );
}

#[tokio::test]
async fn new_root_keeps_its_virtual_commercial_identity_without_persisting() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let before = partners
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let root = tx
        .new_record(&partners, named("Draft root"), None, None)
        .await
        .unwrap();
    let id = root.rec_ids()[0];
    assert!(id.is_new());
    assert_eq!(id.origin(), None);
    assert_ancestry(&mut tx, &root, id, "Draft root", "Draft root").await;

    let default_model = partners.with_context([
        ("default_name", Value::str("Default root")),
        ("default_type", Value::str("delivery")),
    ]);
    let defaults = default_model
        .default_get(&mut tx, &["name", "type"])
        .await
        .unwrap();
    assert_eq!(
        defaults.len(),
        2,
        "default_get only returns requested fields"
    );
    let name = match defaults.get("name") {
        Some(Value::Str(name)) => name.clone(),
        other => panic!("expected the context name default, got {other:?}"),
    };
    let kind = match defaults.get("type") {
        Some(Value::Str(kind)) => kind.clone(),
        other => panic!("expected the context type default, got {other:?}"),
    };
    let default_root = tx
        .new_record(
            &default_model,
            RawVals::new()
                .set_cell("name", Cell::Str(name))
                .set_cell("type", Cell::Str(kind)),
            None,
            None,
        )
        .await
        .unwrap();
    let default_id = default_root.rec_ids()[0];
    assert_ne!(default_id, id);
    assert_ancestry(
        &mut tx,
        &default_root,
        default_id,
        "Default root",
        "Default root",
    )
    .await;
    assert_eq!(
        default_root.value(&mut tx, "type").await.unwrap(),
        Value::str("delivery")
    );

    tx.flush().await.unwrap();
    assert_eq!(
        partners
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut verify = h.begin().await.unwrap();
    let persisted = verify.env().records("res.partner").unwrap();
    assert_eq!(
        persisted
            .search_count(&mut verify, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    let saved = persisted
        .create(&mut verify, vec![named("Saved after draft")])
        .await
        .unwrap();
    assert!(saved.rec_ids()[0].as_real().is_some());
    assert_ancestry(
        &mut verify,
        &saved,
        saved.rec_ids()[0],
        "Saved after draft",
        "Saved after draft",
    )
    .await;
    verify.commit().await.unwrap();
    drop(verify);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn new_descendants_follow_new_roots_and_recompute_after_parent_changes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("res.partner").unwrap();
    let saved = partners
        .create(&mut tx, vec![named("Saved root")])
        .await
        .unwrap();
    let before = partners
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let root = tx
        .new_record(&partners, named("Draft root"), None, None)
        .await
        .unwrap();
    let other = tx
        .new_record(&partners, named("Other root"), None, None)
        .await
        .unwrap();
    let child = tx
        .new_record(
            &partners,
            named("Child").set_cell("parent_id", Cell::Id(root.rec_ids()[0].get())),
            None,
            None,
        )
        .await
        .unwrap();
    let grandchild = tx
        .new_record(
            &partners,
            named("Grandchild").set_cell("parent_id", Cell::Id(child.rec_ids()[0].get())),
            None,
            None,
        )
        .await
        .unwrap();
    assert_ancestry(
        &mut tx,
        &child,
        root.rec_ids()[0],
        "Draft root",
        "Draft root, Child",
    )
    .await;
    assert_ancestry(
        &mut tx,
        &grandchild,
        root.rec_ids()[0],
        "Draft root",
        "Draft root, Grandchild",
    )
    .await;

    child
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_id", Cell::Id(other.rec_ids()[0].get())),
        )
        .await
        .unwrap();
    assert_ancestry(
        &mut tx,
        &child,
        other.rec_ids()[0],
        "Other root",
        "Other root, Child",
    )
    .await;
    assert_ancestry(
        &mut tx,
        &grandchild,
        other.rec_ids()[0],
        "Other root",
        "Other root, Grandchild",
    )
    .await;
    child
        .write(&mut tx, RawVals::new().set_cell("parent_id", Cell::Null))
        .await
        .unwrap();
    assert_ancestry(&mut tx, &child, child.rec_ids()[0], "Child", "Child").await;
    assert_ancestry(
        &mut tx,
        &grandchild,
        child.rec_ids()[0],
        "Child",
        "Child, Grandchild",
    )
    .await;
    child
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_id", Cell::Id(saved.rec_ids()[0].get())),
        )
        .await
        .unwrap();
    assert_ancestry(
        &mut tx,
        &child,
        saved.rec_ids()[0],
        "Saved root",
        "Saved root, Child",
    )
    .await;
    assert_ancestry(
        &mut tx,
        &grandchild,
        saved.rec_ids()[0],
        "Saved root",
        "Saved root, Grandchild",
    )
    .await;
    tx.flush().await.unwrap();
    assert_eq!(
        partners
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut verify = h.begin().await.unwrap();
    let persisted = verify.env().records("res.partner").unwrap();
    assert_eq!(
        persisted
            .search_count(&mut verify, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );
    drop(verify);
    h.cleanup().await.unwrap();
}
