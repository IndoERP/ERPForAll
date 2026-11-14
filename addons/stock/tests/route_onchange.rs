// SPDX-License-Identifier: MIT OR Apache-2.0
//! Rule onchange uses real draft records and leaves stored data unchanged.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/route_fixture.rs"]
mod fixture;
use erp_sdk::prelude::*;
use erp_sdk::{ValueMap, web::Spec};
use fixture::*;

#[tokio::test]
async fn picking_type_replaces_both_draft_locations() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "A").await;
    let pt = picking_type(&mut tx, c, "Type").await;
    let distinct_destination = location(&mut tx, "Type destination", "internal", None).await;
    write(
        &mut tx,
        &pt,
        RawVals::new().set_cell(
            "default_location_dest_id",
            Cell::Id(distinct_destination.ids()[0]),
        ),
    )
    .await
    .unwrap();
    let source = pt.cell(&mut tx, "default_location_src_id").await.unwrap();
    let dest = pt.cell(&mut tx, "default_location_dest_id").await.unwrap();
    assert_ne!(
        source, dest,
        "the fixture must distinguish the two default fields"
    );
    let wrong = location(&mut tx, "Old", "internal", None).await;
    let model = tx.env().records("stock.rule").unwrap();
    {
        let r = model
            .onchange(
                &mut tx,
                ValueMap::from([
                    ("picking_type_id".into(), Value::Int(pt.ids()[0])),
                    ("location_src_id".into(), Value::Int(wrong.ids()[0])),
                    ("location_dest_id".into(), Value::Int(wrong.ids()[0])),
                ]),
                vec!["picking_type_id".into()],
                Spec::new()
                    .field("picking_type_id")
                    .field("location_src_id")
                    .field("location_dest_id"),
            )
            .await
            .unwrap();
        let response = r.dict().unwrap();
        let values = response.get("value").unwrap();
        let changed = values.dict().unwrap();
        for (field, expected) in [("location_src_id", &source), ("location_dest_id", &dest)] {
            let v = changed.get(field).unwrap();
            if let Cell::Id(id) = expected {
                assert_eq!(v, Value::Int(*id));
            } else {
                panic!("location expected");
            }
        }
    }
    assert_eq!(
        model
            .search_count(&mut tx, &Domain::constant(true), None)
            .await
            .unwrap(),
        0
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn route_company_aligns_draft_and_clears_only_incompatible_types() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let b = company(&mut tx, "B").await;
    let wa = warehouse(&mut tx, a, "WA", "WA").await;
    let wb = warehouse(&mut tx, b, "WB", "WB").await;
    let pa = picking_type(&mut tx, a, "Type A").await;
    let pb = picking_type(&mut tx, b, "Type B").await;
    let pn = picking_type(&mut tx, b, "Type without warehouse").await;
    write(
        &mut tx,
        &pa,
        RawVals::new().set_cell("warehouse_id", Cell::Id(wa.ids()[0])),
    )
    .await
    .unwrap();
    write(
        &mut tx,
        &pb,
        RawVals::new().set_cell("warehouse_id", Cell::Id(wb.ids()[0])),
    )
    .await
    .unwrap();
    let model = tx.env().records("stock.rule").unwrap();
    for (company, pt, cleared) in [
        (Some(a), &pb, true),
        (Some(a), &pa, false),
        (None, &pb, true),
        (None, &pn, false),
    ] {
        let route = create(
            &mut tx,
            "stock.route",
            RawVals::new()
                .set_cell("name", text("R"))
                .set_cell("company_id", company.map_or(Cell::Null, Cell::Id)),
        )
        .await
        .unwrap();
        let r = model
            .onchange(
                &mut tx,
                ValueMap::from([
                    ("route_id".into(), Value::Int(route.ids()[0])),
                    ("company_id".into(), Value::Int(b)),
                    ("picking_type_id".into(), Value::Int(pt.ids()[0])),
                ]),
                vec!["route_id".into()],
                Spec::new()
                    .field("route_id")
                    .field("company_id")
                    .field("picking_type_id"),
            )
            .await
            .unwrap();
        let response = r.dict().unwrap();
        let values = response.get("value").unwrap();
        let changed = values.dict().unwrap();
        if let Some(company) = company
            && company != b
        {
            assert_eq!(changed.get("company_id").unwrap(), Value::Int(company));
        }
        if cleared {
            assert_eq!(changed.get("picking_type_id").unwrap(), Value::False);
        } else {
            assert!(!changed.contains("picking_type_id"));
        }
    }
    assert_eq!(
        model
            .search_count(&mut tx, &Domain::constant(true), None)
            .await
            .unwrap(),
        0
    );
    assert_eq!(pb.cell(&mut tx, "company_id").await.unwrap(), Cell::Id(b));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
