//! Product Category tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
    support::base_harness().await
}

#[tokio::test]
async fn test_category_hierarchy_parent_store() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cat_model = tx.env().records("product.category").unwrap();

    let root = cat_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("All".into()))],
        )
        .await
        .unwrap();
    let root_id = root.rec_ids()[0].get();

    let child = cat_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Electronics".into()))
                    .set_cell("parent_id", Cell::Id(root_id)),
            ],
        )
        .await
        .unwrap();

    let child_path = child.cell(&mut tx, "parent_path").await.unwrap();
    if let Cell::Str(path) = child_path {
        assert!(
            path.contains(&root_id.to_string()),
            "parent_path must contain parent id"
        );
    } else {
        panic!("expected string parent_path, got {child_path:?}");
    }
}

#[tokio::test]
async fn test_category_recursive_complete_name() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cat_model = tx.env().records("product.category").unwrap();

    let parent = cat_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Hardware".into()))],
        )
        .await
        .unwrap();
    let parent_id = parent.rec_ids()[0].get();

    let child = cat_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Laptops".into()))
                    .set_cell("parent_id", Cell::Id(parent_id)),
            ],
        )
        .await
        .unwrap();

    let comp_name = child.cell(&mut tx, "complete_name").await.unwrap();
    assert_eq!(comp_name, Cell::Str("Hardware / Laptops".into()));
}

#[tokio::test]
async fn test_category_hierarchical_naming_context() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cat_model = tx.env().records("product.category").unwrap();

    let parent = cat_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Office".into()))],
        )
        .await
        .unwrap();
    let parent_id = parent.rec_ids()[0].get();

    let child = cat_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Desks".into()))
                    .set_cell("parent_id", Cell::Id(parent_id)),
            ],
        )
        .await
        .unwrap();

    // Default hierarchical_naming = true
    let comp_name = child.cell(&mut tx, "complete_name").await.unwrap();
    assert_eq!(comp_name, Cell::Str("Office / Desks".into()));
}

#[tokio::test]
async fn test_category_cascade_deletion() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let cat_model = tx.env().records("product.category").unwrap();

    let parent = cat_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("ParentToDel".into()))],
        )
        .await
        .unwrap();
    let parent_id = parent.rec_ids()[0].get();

    let child = cat_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("ChildToDel".into()))
                    .set_cell("parent_id", Cell::Id(parent_id)),
            ],
        )
        .await
        .unwrap();
    let child_id = child.rec_ids()[0].get();

    parent.unlink(&mut tx).await.unwrap();

    let d = Domain::condition("id", Operator::Eq, child_id).unwrap();
    let remaining = cat_model.search(&mut tx, &d).await.unwrap();
    assert!(
        remaining.is_empty(),
        "Child category should be deleted by cascade"
    );
}
