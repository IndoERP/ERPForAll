// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stored hierarchy names settle without caller flushes.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::collections::BTreeMap;

use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
    ])
    .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .build()
    .await
    .unwrap()
}

fn vals(name: &str, usage: &str, parent: Option<i64>) -> RawVals {
    let vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("usage", Cell::Str(usage.into()));
    match parent {
        Some(id) => vals.set_cell("location_id", Cell::Id(id)),
        None => vals,
    }
}

/// Intentionally no savepoint: the usual stock fixture helper flushes around each create.
async fn create(tx: &mut Transaction, values: RawVals) -> Records {
    tx.env()
        .records("stock.location")
        .unwrap()
        .create(tx, vec![values])
        .await
        .unwrap()
}

/// Read SQL first in a fresh transaction: an ORM read must not repair the persisted assertion.
async fn assert_committed_names(h: &Harness, expected: &[(&str, &str)]) {
    let mut tx = h.begin().await.unwrap();
    let actual: BTreeMap<String, String> = tx
        .query("SELECT name, complete_name FROM stock_location", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let expected: BTreeMap<String, String> = expected
        .iter()
        .map(|(name, full)| ((*name).to_owned(), (*full).to_owned()))
        .collect();
    assert_eq!(actual, expected);
    tx.rollback().await.unwrap();
    drop(tx);
}

#[tokio::test]
async fn parent_and_child_commit_without_intermediate_flush_or_read() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, vals("Zone A", "internal", None)).await;
    create(&mut tx, vals("Row 1", "internal", Some(parent.ids()[0]))).await;
    tx.commit().await.unwrap();
    drop(tx);
    assert_committed_names(&h, &[("Zone A", "Zone A"), ("Row 1", "Zone A/Row 1")]).await;
    h.cleanup().await.unwrap();
}

/// Three pending levels and siblings share ancestors; no read settles a name before commit.
#[tokio::test]
async fn three_levels_and_siblings_commit_together() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, vals("Gudang", "internal", None)).await;
    let child = create(&mut tx, vals("Rak", "internal", Some(root.ids()[0]))).await;
    create(&mut tx, vals("Bín", "internal", Some(child.ids()[0]))).await;
    create(&mut tx, vals("Sibling", "internal", Some(root.ids()[0]))).await;
    create(&mut tx, vals("Outside", "internal", None)).await;
    tx.commit().await.unwrap();
    drop(tx);
    assert_committed_names(
        &h,
        &[
            ("Gudang", "Gudang"),
            ("Rak", "Gudang/Rak"),
            ("Bín", "Gudang/Rak/Bín"),
            ("Sibling", "Gudang/Sibling"),
            ("Outside", "Outside"),
        ],
    )
    .await;
    h.cleanup().await.unwrap();
}

/// A single create call constructs a three-level hierarchy through inverse relation commands.
#[tokio::test]
async fn nested_create_batch_commits_three_levels() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        vals("Root", "internal", None).set_commands(
            "child_ids",
            vec![Command::Create(
                vals("Child", "internal", None).set_commands(
                    "child_ids",
                    vec![Command::Create(vals("Leaf", "internal", None))],
                ),
            )],
        ),
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);
    assert_committed_names(
        &h,
        &[
            ("Root", "Root"),
            ("Child", "Root/Child"),
            ("Leaf", "Root/Child/Leaf"),
        ],
    )
    .await;
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn view_resets_prefix_for_its_pending_descendants() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, vals("Ancestor", "internal", None)).await;
    let view = create(&mut tx, vals("WH", "view", Some(root.ids()[0]))).await;
    create(&mut tx, vals("Shelf", "internal", Some(view.ids()[0]))).await;
    tx.commit().await.unwrap();
    drop(tx);
    assert_committed_names(
        &h,
        &[
            ("Ancestor", "Ancestor"),
            ("WH", "WH"),
            ("Shelf", "WH/Shelf"),
        ],
    )
    .await;
    h.cleanup().await.unwrap();
}

/// An early read is tested separately so it cannot hide a commit-only recomputation bug.
#[tokio::test]
async fn pending_leaf_name_is_readable_before_commit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, vals("Parent", "internal", None)).await;
    let child = create(&mut tx, vals("Child", "internal", Some(parent.ids()[0]))).await;
    let leaf = create(&mut tx, vals("Leaf", "internal", Some(child.ids()[0]))).await;
    assert_eq!(
        leaf.cell(&mut tx, "complete_name").await.unwrap(),
        Cell::Str("Parent/Child/Leaf".into())
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_committed_names(
        &h,
        &[
            ("Parent", "Parent"),
            ("Child", "Parent/Child"),
            ("Leaf", "Parent/Child/Leaf"),
        ],
    )
    .await;
    h.cleanup().await.unwrap();
}

/// Reparent an older subtree under a newer root, so ID order cannot stand in for ancestry.
#[tokio::test]
async fn renamed_and_reparented_subtree_commits_in_dependency_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(&mut tx, vals("Old", "internal", None)).await;
    let child = create(&mut tx, vals("Child", "internal", Some(root.ids()[0]))).await;
    create(&mut tx, vals("Leaf", "internal", Some(child.ids()[0]))).await;
    let new_root = create(&mut tx, vals("New", "internal", None)).await;
    let root_id = root.ids()[0];
    let new_root_id = new_root.ids()[0];
    assert!(root_id < new_root_id);
    tx.commit().await.unwrap();
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records("stock.location")
        .unwrap()
        .browse([root_id])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("name", Cell::Str("Renamed".into()))
                .set_cell("location_id", Cell::Id(new_root_id)),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_committed_names(
        &h,
        &[
            ("New", "New"),
            ("Renamed", "New/Renamed"),
            ("Child", "New/Renamed/Child"),
            ("Leaf", "New/Renamed/Child/Leaf"),
        ],
    )
    .await;
    h.cleanup().await.unwrap();
}
