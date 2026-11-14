// SPDX-License-Identifier: MIT OR Apache-2.0
//! Route lifecycle regressions through the native ORM chain.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/route_fixture.rs"]
mod fixture;
use erp_sdk::prelude::*;
use erp_sdk::{Command, ErrorKind};
use fixture::*;

async fn route(tx: &mut Transaction, name: &str, company: Option<i64>) -> Records {
    create(
        tx,
        "stock.route",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("company_id", company.map_or(Cell::Null, Cell::Id)),
    )
    .await
    .unwrap()
}
async fn rule(
    tx: &mut Transaction,
    route: &Records,
    dest: &Records,
    pt: &Records,
    company: i64,
    name: &str,
) -> Result<Records> {
    create(
        tx,
        "stock.rule",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("route_id", Cell::Id(route.ids()[0]))
            .set_cell("location_dest_id", Cell::Id(dest.ids()[0]))
            .set_cell("picking_type_id", Cell::Id(pt.ids()[0])),
    )
    .await
}

#[tokio::test]
async fn route_defaults_and_selectability_persist() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let r = create(
        &mut tx,
        "stock.route",
        RawVals::new()
            .set_cell("name", text("R"))
            .set_cell("product_selectable", Cell::Bool(true))
            .set_cell("product_categ_selectable", Cell::Bool(true))
            .set_cell("warehouse_selectable", Cell::Bool(true))
            .set_cell("package_type_selectable", Cell::Bool(true)),
    )
    .await
    .unwrap();
    for f in [
        "active",
        "product_selectable",
        "product_categ_selectable",
        "warehouse_selectable",
        "package_type_selectable",
    ] {
        assert_eq!(r.cell(&mut tx, f).await.unwrap(), Cell::Bool(true));
    }
    assert_eq!(r.cell(&mut tx, "sequence").await.unwrap(), Cell::Int(0));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cascade_includes_archived_rules_but_excludes_inactive_destinations() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "A").await;
    let pt = picking_type(&mut tx, c, "Type").await;
    let live = location(&mut tx, "Live", "internal", None).await;
    let inactive = location(&mut tx, "Inactive", "internal", None).await;
    write(
        &mut tx,
        &inactive,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    let r = route(&mut tx, "R", None).await;
    let a = rule(&mut tx, &r, &live, &pt, c, "A").await.unwrap();
    let b = rule(&mut tx, &r, &inactive, &pt, c, "B").await.unwrap();
    let other = route(&mut tx, "Other", None).await;
    let unrelated = rule(&mut tx, &other, &live, &pt, c, "Unrelated")
        .await
        .unwrap();
    for active_test in [true, false] {
        let caller = r.with_context([("active_test", Value::Bool(active_test))]);
        for active in [false, true] {
            write(
                &mut tx,
                &caller,
                RawVals::new().set_cell("active", Cell::Bool(active)),
            )
            .await
            .unwrap();
            assert_eq!(a.cell(&mut tx, "active").await.unwrap(), Cell::Bool(active));
            assert_eq!(b.cell(&mut tx, "active").await.unwrap(), Cell::Bool(true));
            assert_eq!(
                unrelated.cell(&mut tx, "active").await.unwrap(),
                Cell::Bool(true)
            );
        }
    }
    // An inactive destination also keeps a previously archived rule archived on restore.
    write(
        &mut tx,
        &b,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    write(
        &mut tx,
        &r,
        RawVals::new().set_cell("active", Cell::Bool(true)),
    )
    .await
    .unwrap();
    assert_eq!(b.cell(&mut tx, "active").await.unwrap(), Cell::Bool(false));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn disabling_warehouse_selection_clears_simultaneously_supplied_links() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "A").await;
    let w = warehouse(&mut tx, c, "W", "W").await;
    let r = route(&mut tx, "R", Some(c)).await;
    for commands in [
        vec![Command::Link(w.rec_ids()[0])],
        vec![Command::Set(w.rec_ids().to_vec())],
    ] {
        write(
            &mut tx,
            &r,
            RawVals::new()
                .set_cell("warehouse_selectable", Cell::Bool(true))
                .set_commands("warehouse_ids", commands.clone()),
        )
        .await
        .unwrap();
        assert!(
            !r.related(&mut tx, "warehouse_ids")
                .await
                .unwrap()
                .is_empty()
        );
        write(
            &mut tx,
            &r,
            RawVals::new()
                .set_cell("warehouse_selectable", Cell::Bool(false))
                .set_commands("warehouse_ids", commands),
        )
        .await
        .unwrap();
        assert!(
            r.related(&mut tx, "warehouse_ids")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(w.related(&mut tx, "route_ids").await.unwrap().is_empty());
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn route_company_validation_is_atomic_and_shared_routes_are_allowed() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let b = company(&mut tx, "B").await;
    let pt = picking_type(&mut tx, b, "Type B").await;
    let dest = location(&mut tx, "Destination", "internal", None).await;
    let r = route(&mut tx, "R", None).await;
    rule(&mut tx, &r, &dest, &pt, b, "X").await.unwrap();
    let good = route(&mut tx, "Good", None).await;
    let both = good.union(&r).unwrap();
    let e = write(
        &mut tx,
        &both,
        RawVals::new()
            .set_cell("company_id", Cell::Id(a))
            .set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "The rule \"X\" belongs to company B, but its route belongs to company A. Give the rule and the route the same company."
    );
    for row in [&good, &r] {
        assert_eq!(row.cell(&mut tx, "company_id").await.unwrap(), Cell::Null);
        assert_eq!(row.cell(&mut tx, "active").await.unwrap(), Cell::Bool(true));
    }
    write(
        &mut tx,
        &r,
        RawVals::new().set_cell("company_id", Cell::Id(b)),
    )
    .await
    .unwrap();
    write(
        &mut tx,
        &r,
        RawVals::new().set_cell("company_id", Cell::Null),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rule_company_accepts_shared_same_and_ancestor_routes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let b = company(&mut tx, "B").await;
    let c = company(&mut tx, "C").await;
    tx.query("UPDATE res_company SET parent_id=$1::bigint,parent_path=($1::bigint)::text || '/' || id::text || '/' WHERE id=$2", &[&a,&b]).await.unwrap();
    let pt = picking_type(&mut tx, b, "Type B").await;
    let dest = location(&mut tx, "Destination", "internal", None).await;
    for company in [None, Some(a), Some(b)] {
        let r = route(&mut tx, "Allowed", company).await;
        rule(&mut tx, &r, &dest, &pt, b, "X").await.unwrap();
    }
    let bad = route(&mut tx, "Other", Some(c)).await;
    let e = rule(&mut tx, &bad, &dest, &pt, b, "X").await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "The rule \"X\" belongs to company B, but its route belongs to company C. Choose a route of the rule's company or of one of its parent companies."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn child_company_rule_through_route_lines_is_accepted_like_the_rule_form() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = company(&mut tx, "P").await;
    let child = company(&mut tx, "C").await;
    tx.query("UPDATE res_company SET parent_id=$1::bigint,parent_path=($1::bigint)::text || '/' || id::text || '/' WHERE id=$2", &[&parent,&child]).await.unwrap();
    let pt = picking_type(&mut tx, child, "Type C").await;
    let dest = location(&mut tx, "Destination", "internal", None).await;
    let r = route(&mut tx, "R", Some(parent)).await;
    write(
        &mut tx,
        &r,
        RawVals::new().set_commands(
            "rule_ids",
            [Command::Create(
                RawVals::new()
                    .set_cell("name", text("X"))
                    .set_cell("company_id", Cell::Id(child))
                    .set_cell("picking_type_id", Cell::Id(pt.ids()[0]))
                    .set_cell("location_dest_id", Cell::Id(dest.ids()[0])),
            )],
        ),
    )
    .await
    .unwrap();
    let rules = r.related(&mut tx, "rule_ids").await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(
        rules.cell(&mut tx, "company_id").await.unwrap(),
        Cell::Id(child)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn non_boolean_archive_and_selection_values_are_refused_by_name() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let r = route(&mut tx, "R", None).await;
    for (field, stored) in [("active", true), ("warehouse_selectable", false)] {
        for value in [Cell::Null, text("yes"), Cell::Int(0)] {
            let error = write(&mut tx, &r, RawVals::new().set_cell(field, value))
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming, "{field}: {error}");
            assert_eq!(
                error.message(),
                format!("stock.route.{field} expects true or false.")
            );
            assert_eq!(r.cell(&mut tx, field).await.unwrap(), Cell::Bool(stored));
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn lifecycle_upsert_fails_explicitly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for model in ["stock.route", "stock.rule"] {
        let rows = tx.env().records(model).unwrap();
        let e = tx
            .savepoint(async |tx| {
                rows.upsert(
                    tx,
                    erp_sdk::UpsertCall::new("_generated_role_uniq")
                        .with_rows([RawVals::new().set_cell("name", text("X"))]),
                )
                .await
            })
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UserError);
        assert_eq!(
            e.message(),
            format!(
                "{model} cannot be updated by key. Use create or write so route lifecycle validation runs."
            )
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn field_denial_precedes_cascade_and_company_diagnostics() {
    let h = erp_testkit::Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
    ])
    .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .policy(erp_testkit::FieldPolicy::new().deny("stock.route", "active", FieldOp::Write))
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let r = route(&mut tx, "R", None).await;
    let caller = r.with_user(7);
    let e = write(
        &mut tx,
        &caller,
        RawVals::new()
            .set_cell("active", Cell::Bool(false))
            .set_cell("company_id", Cell::Id(a)),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Access);
    assert_eq!(e.message(), "Writing stock.route.active is not allowed.");
    assert_eq!(r.cell(&mut tx, "active").await.unwrap(), Cell::Bool(true));
    assert_eq!(r.cell(&mut tx, "company_id").await.unwrap(), Cell::Null);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn relation_command_company_failure_rolls_back_the_route() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "A").await;
    let b = company(&mut tx, "B").await;
    let pt = picking_type(&mut tx, b, "Type B").await;
    let dest = location(&mut tx, "Destination", "internal", None).await;
    let r = route(&mut tx, "R", Some(a)).await;
    let e = write(
        &mut tx,
        &r,
        RawVals::new()
            .set_cell("name", text("Changed"))
            .set_commands(
                "rule_ids",
                [Command::Create(
                    RawVals::new()
                        .set_cell("name", text("X"))
                        .set_cell("company_id", Cell::Id(b))
                        .set_cell("picking_type_id", Cell::Id(pt.ids()[0]))
                        .set_cell("location_dest_id", Cell::Id(dest.ids()[0])),
                )],
            ),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(
        e.message(),
        "The rule \"X\" belongs to company B, but its route belongs to company A. Choose a route of the rule's company or of one of its parent companies."
    );
    assert_eq!(r.cell(&mut tx, "name").await.unwrap(), text("R"));
    assert!(r.related(&mut tx, "rule_ids").await.unwrap().is_empty());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn child_field_denial_rolls_back_a_direct_route_write() {
    let h = erp_testkit::Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
    ])
    .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .policy(erp_testkit::FieldPolicy::new().deny("stock.rule", "active", FieldOp::Write))
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let c = company(&mut tx, "A").await;
    let pt = picking_type(&mut tx, c, "Type").await;
    let dest = location(&mut tx, "Destination", "internal", None).await;
    let r = route(&mut tx, "R", None).await;
    let child = rule(&mut tx, &r, &dest, &pt, c, "X").await.unwrap();
    let expected = child
        .with_user(7)
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(expected.kind(), ErrorKind::Access);
    // No caller-owned savepoint: the override itself must undo the parent mutation.
    let error = r
        .with_user(7)
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(error.message(), expected.message());
    assert_eq!(r.cell(&mut tx, "active").await.unwrap(), Cell::Bool(true));
    assert_eq!(
        child.cell(&mut tx, "active").await.unwrap(),
        Cell::Bool(true)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn inaccessible_company_labels_never_become_validation_diagnostics() {
    let h = erp_testkit::Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
    ])
    .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .policy(erp_testkit::FieldPolicy::new().deny("res.company", "name", FieldOp::Read))
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let a = company(&mut tx, "Private A").await;
    let b = company(&mut tx, "Private B").await;
    let pt = picking_type(&mut tx, b, "Type").await;
    let dest = location(&mut tx, "Destination", "internal", None).await;
    let r = route(&mut tx, "R", Some(a)).await;
    tx.execute("INSERT INTO res_users(id,name,login,active,company_id,share) VALUES(7,'Scoped route reader','scoped-route-reader',true,$1,false)",&[&b]).await.unwrap();
    let companies = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([b])
        .unwrap()
        .with_user(7);
    let expected = companies.cell(&mut tx, "name").await.unwrap_err();
    assert_eq!(expected.kind(), ErrorKind::Access);
    let model = tx.env().records("stock.rule").unwrap().with_user(7);
    let error = tx
        .savepoint(async |tx| {
            model
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("X"))
                            .set_cell("company_id", Cell::Id(b))
                            .set_cell("route_id", Cell::Id(r.ids()[0]))
                            .set_cell("location_dest_id", Cell::Id(dest.ids()[0]))
                            .set_cell("picking_type_id", Cell::Id(pt.ids()[0])),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(error.message(), expected.message());
    assert!(!error.message().contains("Private A"));
    assert!(!error.message().contains("Private B"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn disabling_selectability_does_not_swallow_a_malformed_relation_cell() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let r = route(&mut tx, "R", None).await;
    let error = write(
        &mut tx,
        &r,
        RawVals::new()
            .set_cell("warehouse_selectable", Cell::Bool(false))
            .set_cell("warehouse_ids", text("malformed")),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "stock.route.warehouse_ids expects a relation id list."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
