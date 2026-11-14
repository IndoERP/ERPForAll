// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/fpos.rs"]
mod support;
use erp_sdk::prelude::*;
use support::*;

#[tokio::test]
async fn tax_mapping_reads_both_relation_directions_and_refreshes_after_edits() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let co = company(&mut tx, be, None).await;
    let a = position(&mut tx, co, "A", RawVals::new()).await;
    let b = position(&mut tx, co, "B", RawVals::new()).await;
    let source = tax(&mut tx, co, be, "Source").await;
    let dest = tax(&mut tx, co, be, "Destination").await;
    let second = tax(&mut tx, co, be, "Second").await;
    let map = |rec: &Records| rec.clone();
    let empty = model(&tx, co, "account.fiscal.position");
    let unchanged: Vec<i64> = empty
        .call(&mut tx, "map_tax", (vec![source.ids()[0], dest.ids()[0]],))
        .await
        .unwrap();
    assert_eq!(unchanged, vec![source.ids()[0], dest.ids()[0]]);
    let global: Vec<i64> = a
        .call(&mut tx, "map_tax", (vec![source.ids()[0]],))
        .await
        .unwrap();
    assert_eq!(global, source.ids());
    dest.write(&mut tx, links("fiscal_position_ids", &a.ids()))
        .await
        .unwrap();
    dest.write(&mut tx, links("original_tax_ids", &source.ids()))
        .await
        .unwrap();
    a.write(&mut tx, links("tax_ids", &[dest.ids()[0], second.ids()[0]]))
        .await
        .unwrap();
    second
        .write(&mut tx, links("original_tax_ids", &source.ids()))
        .await
        .unwrap();
    assert_eq!(ids(&mut tx, &dest, "fiscal_position_ids").await, a.ids());
    assert_eq!(
        ids(&mut tx, &source, "replacing_tax_ids").await,
        vec![dest.ids()[0], second.ids()[0]]
    );
    let computed = a.cell(&mut tx, "tax_map").await.unwrap();
    let expected =
        erp_sdk::serde_json::json!({source.ids()[0].to_string():[dest.ids()[0],second.ids()[0]]});
    assert_eq!(computed, Cell::Json(Arc::new(expected)));
    let self_mapped: Vec<i64> = a.call(&mut tx, "map_tax", (dest.ids(),)).await.unwrap();
    assert_eq!(self_mapped, dest.ids());
    let mapped: Vec<i64> = map(&a)
        .call(&mut tx, "map_tax", (vec![source.ids()[0], dest.ids()[0]],))
        .await
        .unwrap();
    assert_eq!(mapped, vec![dest.ids()[0], second.ids()[0]]);
    let removed: Vec<i64> = b.call(&mut tx, "map_tax", (dest.ids(),)).await.unwrap();
    assert!(removed.is_empty());
    second
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    let mapped: Vec<i64> = a.call(&mut tx, "map_tax", (source.ids(),)).await.unwrap();
    assert_eq!(mapped, dest.ids());
    dest.write(&mut tx, links("original_tax_ids", &[]))
        .await
        .unwrap();
    let unmapped: Vec<i64> = a.call(&mut tx, "map_tax", (source.ids(),)).await.unwrap();
    assert_eq!(unmapped, source.ids());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn multiple_sources_map_independently_and_universal_links_survive() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let co = company(&mut tx, be, None).await;
    let a = position(&mut tx, co, "A", RawVals::new()).await;
    let b = position(&mut tx, co, "B", RawVals::new()).await;
    let first = tax(&mut tx, co, be, "First source").await;
    let second = tax(&mut tx, co, be, "Second source").await;
    let first_dest = tax(&mut tx, co, be, "First destination").await;
    let second_dest = tax(&mut tx, co, be, "Second destination").await;
    for (source, dest) in [(&first, &first_dest), (&second, &second_dest)] {
        dest.write(
            &mut tx,
            links("fiscal_position_ids", &[a.ids()[0], b.ids()[0]]),
        )
        .await
        .unwrap();
        dest.write(&mut tx, links("original_tax_ids", &source.ids()))
            .await
            .unwrap();
    }
    for fp in [&a, &b] {
        let mapped: Vec<i64> = fp
            .call(&mut tx, "map_tax", (vec![first.ids()[0], second.ids()[0]],))
            .await
            .unwrap();
        assert_eq!(mapped, vec![first_dest.ids()[0], second_dest.ids()[0]]);
        let universal: Vec<i64> = fp
            .call(&mut tx, "map_tax", (first_dest.ids(),))
            .await
            .unwrap();
        assert_eq!(universal, first_dest.ids());
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn account_mapping_is_derived_and_duplicate_pairs_are_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let co = company(&mut tx, be, None).await;
    let fp = position(&mut tx, co, "A", RawVals::new()).await;
    let mut acc = Vec::new();
    for code in ["4000", "4100", "5000"] {
        acc.push(
            model(&tx, co, "account.account")
                .create(
                    &mut tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text(code))
                            .set_cell("code", text(code))
                            .set_cell("account_type", text("income")),
                    ],
                )
                .await
                .unwrap()
                .ids()[0],
        );
    }
    let vals = || {
        RawVals::new()
            .set_cell("position_id", Cell::Id(fp.ids()[0]))
            .set_cell("account_src_id", Cell::Id(acc[0]))
            .set_cell("account_dest_id", Cell::Id(acc[1]))
    };
    let mapping = model(&tx, co, "account.fiscal.position.account")
        .create(&mut tx, vec![vals()])
        .await
        .unwrap();
    let mapped: i64 = fp.call(&mut tx, "map_account", (acc[0],)).await.unwrap();
    assert_eq!(mapped, acc[1]);
    let unchanged: i64 = fp.call(&mut tx, "map_account", (acc[2],)).await.unwrap();
    assert_eq!(unchanged, acc[2]);
    let rec = model(&tx, co, "account.fiscal.position.account");
    let error = tx
        .savepoint(async |tx| rec.create(tx, vec![vals()]).await)
        .await
        .unwrap_err();
    refused(
        error,
        "The database refused the change: This fiscal position already contains this source and destination account pair.",
    );
    mapping
        .write(
            &mut tx,
            RawVals::new().set_cell("account_dest_id", Cell::Id(acc[2])),
        )
        .await
        .unwrap();
    let mapped: i64 = fp.call(&mut tx, "map_account", (acc[0],)).await.unwrap();
    assert_eq!(mapped, acc[2]);
    mapping.unlink(&mut tx).await.unwrap();
    let original: i64 = fp.call(&mut tx, "map_account", (acc[0],)).await.unwrap();
    assert_eq!(original, acc[0]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn tax_picker_uses_the_position_context_before_limiting_results() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let co = company(&mut tx, be, None).await;
    let a = position(&mut tx, co, "A", RawVals::new()).await;
    let b = position(&mut tx, co, "B", RawVals::new()).await;
    let own = tax(&mut tx, co, be, "Own tax").await;
    let other = tax(&mut tx, co, be, "Other tax").await;
    own.write(&mut tx, links("fiscal_position_ids", &a.ids()))
        .await
        .unwrap();
    other
        .write(&mut tx, links("fiscal_position_ids", &b.ids()))
        .await
        .unwrap();
    let rec = model(&tx, co, "account.tax").with_env(
        tx.env()
            .with_company(co)
            .unwrap()
            .with_context([("dynamic_fiscal_position_id", Value::Int(a.ids()[0]))]),
    );
    let spec =
        erp_sdk::web::Spec::from_value(&Value::Map(Arc::new(erp_sdk::ValueMap::from_iter([(
            "display_name".into(),
            Value::Map(Arc::new(erp_sdk::ValueMap::new())),
        )]))))
        .unwrap();
    let args = erp_sdk::web::WebNameSearch {
        name: "tax".into(),
        domain: Domain::TRUE,
        operator: Operator::ILike,
        limit: Some(100),
        specification: spec,
    };
    let rows = rec.web_name_search(&mut tx, args.clone()).await.unwrap();
    assert_eq!(rows.len(), 1);
    let rendered = format!("{rows:?}");
    assert!(rendered.to_string().contains("Own tax"));
    assert!(!rendered.to_string().contains("Other tax"));
    let malformed = rec.with_env(
        tx.env()
            .with_company(co)
            .unwrap()
            .with_context([("dynamic_fiscal_position_id", Value::Str("wrong".into()))]),
    );
    refused(
        malformed.web_name_search(&mut tx, args).await.unwrap_err(),
        "dynamic_fiscal_position_id must be false or a positive record id.",
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn partner_manual_positions_are_company_dependent_and_clearable() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let root = company(&mut tx, be, None).await;
    let branch = company(&mut tx, be, Some(root)).await;
    let p = partner(&mut tx, be).await;
    let a = position(&mut tx, root, "Root manual", RawVals::new()).await;
    let b = position(&mut tx, branch, "Branch manual", RawVals::new()).await;
    let pa = p.with_env(tx.env().with_company(root).unwrap());
    let pb = p.with_env(tx.env().with_company(branch).unwrap());
    pa.write(
        &mut tx,
        RawVals::new().set_cell("property_account_position_id", Cell::Id(a.ids()[0])),
    )
    .await
    .unwrap();
    pb.write(
        &mut tx,
        RawVals::new().set_cell("property_account_position_id", Cell::Id(b.ids()[0])),
    )
    .await
    .unwrap();
    assert_eq!(
        choose(&mut tx, root, p.ids()[0], None).await,
        Some(a.ids()[0])
    );
    assert_eq!(
        choose(&mut tx, branch, p.ids()[0], None).await,
        Some(b.ids()[0])
    );
    pb.write(
        &mut tx,
        RawVals::new().set_cell("property_account_position_id", Cell::Null),
    )
    .await
    .unwrap();
    assert_eq!(choose(&mut tx, branch, p.ids()[0], None).await, None);
    assert_eq!(
        choose(&mut tx, root, p.ids()[0], None).await,
        Some(a.ids()[0])
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
