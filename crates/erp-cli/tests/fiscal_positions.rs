// SPDX-License-Identifier: MIT OR Apache-2.0
//! Fiscal-position behavior through the real addon model/dispatch paths.
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
async fn matching_prioritizes_company_depth_sequence_id_and_archive() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let fr = country(&mut tx, "FR").await;
    let root = company(&mut tx, be, None).await;
    let child = company(&mut tx, be, Some(root)).await;
    let p = partner(&mut tx, fr).await;
    let criteria = || {
        RawVals::new()
            .set_cell("auto_apply", Cell::Bool(true))
            .set_cell("country_id", Cell::Id(fr))
    };
    let a = position(
        &mut tx,
        root,
        "Root",
        criteria().set_cell("sequence", Cell::Int(1)),
    )
    .await;
    assert_eq!(
        choose(&mut tx, child, p.ids()[0], None).await,
        Some(a.ids()[0])
    );
    let b = position(
        &mut tx,
        child,
        "Child",
        criteria().set_cell("sequence", Cell::Int(20)),
    )
    .await;
    let c = position(
        &mut tx,
        child,
        "Tie",
        criteria().set_cell("sequence", Cell::Int(20)),
    )
    .await;
    assert_eq!(
        choose(&mut tx, child, p.ids()[0], None).await,
        Some(b.ids()[0])
    );
    b.write(
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    assert_eq!(
        choose(&mut tx, child, p.ids()[0], None).await,
        Some(c.ids()[0])
    );
    c.write(&mut tx, RawVals::new().set_cell("sequence", Cell::Int(0)))
        .await
        .unwrap();
    assert_eq!(
        choose(&mut tx, child, p.ids()[0], None).await,
        Some(c.ids()[0])
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn zip_normalization_matching_and_vat_presence_are_explicit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let co = company(&mut tx, be, None).await;
    let p = partner(&mut tx, be).await;
    let fp = position(
        &mut tx,
        co,
        "ZIP",
        RawVals::new()
            .set_cell("auto_apply", Cell::Bool(true))
            .set_cell("vat_required", Cell::Bool(true))
            .set_cell("zip_from", text("100"))
            .set_cell("zip_to", text("2000")),
    )
    .await;
    assert_eq!(fp.cell(&mut tx, "zip_from").await.unwrap(), text("0100"));
    for vat in ["/", "NA", "na", ""] {
        p.write(
            &mut tx,
            RawVals::new()
                .set_cell("zip", text("1500"))
                .set_cell("vat", text(vat)),
        )
        .await
        .unwrap();
        assert_eq!(choose(&mut tx, co, p.ids()[0], None).await, None);
    }
    // Presence is the base predicate, not national validity: "n/a" is not one of
    // Community's placeholder values (res_partner.py::_compute_has_vat).
    p.write(&mut tx, RawVals::new().set_cell("vat", text("n/a")))
        .await
        .unwrap();
    assert_eq!(
        choose(&mut tx, co, p.ids()[0], None).await,
        Some(fp.ids()[0])
    );
    p.write(
        &mut tx,
        RawVals::new().set_cell("vat", text("BE0477472701")),
    )
    .await
    .unwrap();
    for boundary in ["0100", "2000"] {
        p.write(&mut tx, RawVals::new().set_cell("zip", text(boundary)))
            .await
            .unwrap();
        assert_eq!(
            choose(&mut tx, co, p.ids()[0], None).await,
            Some(fp.ids()[0])
        );
    }
    p.write(&mut tx, RawVals::new().set_cell("zip", text("2500")))
        .await
        .unwrap();
    assert_eq!(choose(&mut tx, co, p.ids()[0], None).await, None);
    p.write(&mut tx, RawVals::new().set_cell("zip", text("99")))
        .await
        .unwrap();
    assert_eq!(choose(&mut tx, co, p.ids()[0], None).await, None);
    refused(
        write(&mut tx, &fp, RawVals::new().set_cell("zip_to", Cell::Null))
            .await
            .unwrap_err(),
        "Set both zip_from and zip_to, with zip_to at least zip_from.",
    );
    refused(
        write(
            &mut tx,
            &fp,
            RawVals::new().set_cell("zip_from", text("9000")),
        )
        .await
        .unwrap_err(),
        "Set both zip_from and zip_to, with zip_to at least zip_from.",
    );
    p.write(&mut tx, RawVals::new().set_cell("country_id", Cell::Null))
        .await
        .unwrap();
    assert_eq!(choose(&mut tx, co, p.ids()[0], None).await, None);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_vat_normalization_allows_same_identity_and_refuses_conflicts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let fr = country(&mut tx, "FR").await;
    let co = company(&mut tx, be, None).await;
    let a = position(
        &mut tx,
        co,
        "French",
        RawVals::new()
            .set_cell("country_id", Cell::Id(fr))
            .set_cell("foreign_vat", text(" fr. 12-34 ")),
    )
    .await;
    assert_eq!(
        a.cell(&mut tx, "foreign_vat").await.unwrap(),
        text("FR1234")
    );
    let b = position(
        &mut tx,
        co,
        "Same VAT",
        RawVals::new()
            .set_cell("country_id", Cell::Id(fr))
            .set_cell("foreign_vat", text("FR1234")),
    )
    .await;
    refused(
        write(
            &mut tx,
            &b,
            RawVals::new().set_cell("foreign_vat", text("FR9876")),
        )
        .await
        .unwrap_err(),
        "Fiscal position \"French\" already uses a different foreign_vat for this country.",
    );
    refused(
        write(
            &mut tx,
            &b,
            RawVals::new().set_cell("country_id", Cell::Null),
        )
        .await
        .unwrap_err(),
        "Set country_id before assigning foreign_vat.",
    );
    refused(
        write(
            &mut tx,
            &b,
            RawVals::new().set_cell("foreign_vat", text(" .- ")),
        )
        .await
        .unwrap_err(),
        "foreign_vat must contain a tax identifier after normalization.",
    );
    b.write(&mut tx, RawVals::new().set_cell("foreign_vat", Cell::Null))
        .await
        .unwrap();
    assert_eq!(b.cell(&mut tx, "foreign_vat").await.unwrap(), Cell::Null);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn address_manual_and_group_exclusions_compose() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let fr = country(&mut tx, "FR").await;
    let co = company(&mut tx, be, None).await;
    let p = partner(&mut tx, be).await;
    let delivery = partner(&mut tx, fr).await;
    let state = tx
        .env()
        .records("res.country.state")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Test state"))
                    .set_cell("code", text("T"))
                    .set_cell("country_id", Cell::Id(fr)),
            ],
        )
        .await
        .unwrap();
    delivery
        .write(
            &mut tx,
            RawVals::new().set_cell("state_id", Cell::Id(state.ids()[0])),
        )
        .await
        .unwrap();
    let groups = tx.env().records("res.country.group").unwrap();
    let group = groups
        .create(
            &mut tx,
            vec![links("country_ids", &[fr]).set_cell("name", text("Test union"))],
        )
        .await
        .unwrap();
    let automatic = position(
        &mut tx,
        co,
        "Group",
        links("state_ids", &state.ids())
            .set_cell("country_group_id", Cell::Id(group.ids()[0]))
            .set_cell("auto_apply", Cell::Bool(true)),
    )
    .await;
    assert_eq!(
        choose(&mut tx, co, p.ids()[0], Some(delivery.ids()[0])).await,
        Some(automatic.ids()[0])
    );
    group
        .write(&mut tx, links("exclude_state_ids", &state.ids()))
        .await
        .unwrap();
    assert_eq!(
        choose(&mut tx, co, p.ids()[0], Some(delivery.ids()[0])).await,
        None
    );
    let manual = position(&mut tx, co, "Manual", RawVals::new()).await;
    p.clone()
        .with_env(tx.env().with_company(co).unwrap())
        .write(
            &mut tx,
            RawVals::new().set_cell("property_account_position_id", Cell::Id(manual.ids()[0])),
        )
        .await
        .unwrap();
    assert_eq!(
        choose(&mut tx, co, p.ids()[0], Some(delivery.ids()[0])).await,
        Some(manual.ids()[0])
    );
    delivery
        .clone()
        .with_env(tx.env().with_company(co).unwrap())
        .write(
            &mut tx,
            RawVals::new().set_cell("property_account_position_id", Cell::Id(automatic.ids()[0])),
        )
        .await
        .unwrap();
    assert_eq!(
        choose(&mut tx, co, p.ids()[0], Some(delivery.ids()[0])).await,
        Some(automatic.ids()[0])
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_vat_requires_state_in_home_jurisdiction_and_group_membership() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let fr = country(&mut tx, "FR").await;
    let co = company(&mut tx, be, None).await;
    let company = model(&tx, co, "res.company").browse([co]).unwrap();
    company
        .write(
            &mut tx,
            RawVals::new().set_cell("account_fiscal_country_id", Cell::Id(be)),
        )
        .await
        .unwrap();
    let state = tx
        .env()
        .records("res.country.state")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Test state"))
                    .set_cell("code", text("T"))
                    .set_cell("country_id", Cell::Id(be)),
            ],
        )
        .await
        .unwrap();
    let fp = position(
        &mut tx,
        co,
        "Home",
        RawVals::new().set_cell("country_id", Cell::Id(be)),
    )
    .await;
    refused(
        write(
            &mut tx,
            &fp,
            RawVals::new().set_cell("foreign_vat", text("BE1234")),
        )
        .await
        .unwrap_err(),
        "Set state_ids when foreign_vat is in the company's fiscal country.",
    );
    fp.write(
        &mut tx,
        links("state_ids", &state.ids()).set_cell("foreign_vat", text("BE1234")),
    )
    .await
    .unwrap();
    let group = tx
        .env()
        .records("res.country.group")
        .unwrap()
        .create(
            &mut tx,
            vec![links("country_ids", &[fr]).set_cell("name", text("Foreign group"))],
        )
        .await
        .unwrap();
    refused(
        write(
            &mut tx,
            &fp,
            RawVals::new().set_cell("country_group_id", Cell::Id(group.ids()[0])),
        )
        .await
        .unwrap_err(),
        "country_id must belong to country_group_id when foreign_vat is set.",
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn domestic_and_foreign_derived_fields_follow_mutations() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let fr = country(&mut tx, "FR").await;
    let co = company(&mut tx, be, None).await;
    let group = tx
        .env()
        .records("res.country.group")
        .unwrap()
        .create(
            &mut tx,
            vec![links("country_ids", &[be]).set_cell("name", text("Home group"))],
        )
        .await
        .unwrap();
    let general = position(
        &mut tx,
        co,
        "Group",
        RawVals::new()
            .set_cell("country_group_id", Cell::Id(group.ids()[0]))
            .set_cell("sequence", Cell::Int(10)),
    )
    .await;
    let exact = position(
        &mut tx,
        co,
        "Country",
        RawVals::new()
            .set_cell("country_id", Cell::Id(be))
            .set_cell("sequence", Cell::Int(10)),
    )
    .await;
    let company = model(&tx, co, "res.company").browse([co]).unwrap();
    assert_eq!(
        company
            .cell(&mut tx, "domestic_fiscal_position_id")
            .await
            .unwrap(),
        Cell::Id(exact.ids()[0])
    );
    assert_eq!(
        exact.cell(&mut tx, "is_domestic").await.unwrap(),
        Cell::Bool(true)
    );
    let t = tax(&mut tx, co, be, "Domestic tax").await;
    assert_eq!(
        t.cell(&mut tx, "is_domestic").await.unwrap(),
        Cell::Bool(true)
    );
    t.write(&mut tx, links("fiscal_position_ids", &general.ids()))
        .await
        .unwrap();
    assert_eq!(
        t.cell(&mut tx, "is_domestic").await.unwrap(),
        Cell::Bool(false)
    );
    general
        .write(&mut tx, RawVals::new().set_cell("sequence", Cell::Int(1)))
        .await
        .unwrap();
    assert_eq!(
        company
            .cell(&mut tx, "domestic_fiscal_position_id")
            .await
            .unwrap(),
        Cell::Id(general.ids()[0])
    );
    assert_eq!(
        t.cell(&mut tx, "is_domestic").await.unwrap(),
        Cell::Bool(true)
    );
    let foreign = position(
        &mut tx,
        co,
        "Foreign",
        RawVals::new()
            .set_cell("country_id", Cell::Id(fr))
            .set_cell("foreign_vat", text("FR1234")),
    )
    .await;
    assert_eq!(
        ids(&mut tx, &company, "multi_vat_foreign_country_ids").await,
        vec![fr]
    );
    foreign
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();
    assert!(
        ids(&mut tx, &company, "multi_vat_foreign_country_ids")
            .await
            .is_empty()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn batch_zip_edits_merge_each_rows_bounds_and_saved_maps_rollback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let co = company(&mut tx, be, None).await;
    let a = position(
        &mut tx,
        co,
        "A",
        RawVals::new()
            .set_cell("zip_from", text("10"))
            .set_cell("zip_to", text("99")),
    )
    .await;
    let b = position(
        &mut tx,
        co,
        "B",
        RawVals::new()
            .set_cell("zip_from", text("20"))
            .set_cell("zip_to", text("999")),
    )
    .await;
    model(&tx, co, "account.fiscal.position")
        .browse([a.ids()[0], b.ids()[0]])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("zip_from", text("1")))
        .await
        .unwrap();
    assert_eq!(a.cell(&mut tx, "zip_from").await.unwrap(), text("01"));
    assert_eq!(b.cell(&mut tx, "zip_from").await.unwrap(), text("001"));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn matching_eu_prefix_uses_invoice_address_and_other_prefix_uses_delivery() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let fr = country(&mut tx, "FR").await;
    let co = company(&mut tx, be, None).await;
    let group = tx
        .env()
        .records("res.country.group")
        .unwrap()
        .create(
            &mut tx,
            vec![links("country_ids", &[be, fr]).set_cell("name", text("EU fixture"))],
        )
        .await
        .unwrap();
    tx.execute("INSERT INTO ir_model_data(module,name,model,res_id,noupdate) VALUES('base','europe','res.country.group',$1,true)",&[&group.ids()[0]]).await.unwrap();
    let p = partner(&mut tx, be).await;
    let delivery = partner(&mut tx, fr).await;
    let home = position(
        &mut tx,
        co,
        "Home",
        RawVals::new()
            .set_cell("auto_apply", Cell::Bool(true))
            .set_cell("country_id", Cell::Id(be)),
    )
    .await;
    let abroad = position(
        &mut tx,
        co,
        "Abroad",
        RawVals::new()
            .set_cell("auto_apply", Cell::Bool(true))
            .set_cell("country_id", Cell::Id(fr)),
    )
    .await;
    model(&tx, co, "res.company")
        .browse([co])
        .unwrap()
        .related(&mut tx, "partner_id")
        .await
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("vat", text("BE0477472701")),
        )
        .await
        .unwrap();
    p.write(
        &mut tx,
        RawVals::new().set_cell("vat", text("BE0123456789")),
    )
    .await
    .unwrap();
    assert_eq!(
        choose(&mut tx, co, p.ids()[0], Some(delivery.ids()[0])).await,
        Some(home.ids()[0])
    );
    p.write(&mut tx, RawVals::new().set_cell("vat", text("FR123456789")))
        .await
        .unwrap();
    assert_eq!(
        choose(&mut tx, co, p.ids()[0], Some(delivery.ids()[0])).await,
        Some(abroad.ids()[0])
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalid_input_types_refuse_by_field_without_silent_defaults() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let co = company(&mut tx, be, None).await;
    let fp = position(&mut tx, co, "Typed input", RawVals::new()).await;
    for (field, cell, message) in [
        (
            "company_id",
            text("wrong"),
            "company_id must be a positive record id.",
        ),
        ("zip_from", Cell::Int(12), "zip_from must be text or empty."),
        (
            "foreign_vat",
            Cell::Int(12),
            "foreign_vat must be text or empty.",
        ),
    ] {
        refused(
            write(&mut tx, &fp, RawVals::new().set_cell(field, cell))
                .await
                .unwrap_err(),
            message,
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_batch_can_change_positions_sharing_one_foreign_vat_together() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let fr = country(&mut tx, "FR").await;
    let co = company(&mut tx, be, None).await;
    let a = position(
        &mut tx,
        co,
        "A",
        RawVals::new()
            .set_cell("country_id", Cell::Id(fr))
            .set_cell("foreign_vat", text("FR1234")),
    )
    .await;
    let b = position(
        &mut tx,
        co,
        "B",
        RawVals::new()
            .set_cell("country_id", Cell::Id(fr))
            .set_cell("foreign_vat", text("FR1234")),
    )
    .await;
    let both = model(&tx, co, "account.fiscal.position")
        .browse([a.ids()[0], b.ids()[0]])
        .unwrap();
    let third = position(
        &mut tx,
        co,
        "Third",
        RawVals::new()
            .set_cell("country_id", Cell::Id(fr))
            .set_cell("foreign_vat", text("FR1234")),
    )
    .await;
    refused(
        write(
            &mut tx,
            &both,
            RawVals::new().set_cell("foreign_vat", text("FR5678")),
        )
        .await
        .unwrap_err(),
        "Fiscal position \"Third\" already uses a different foreign_vat for this country.",
    );
    third.unlink(&mut tx).await.unwrap();
    both.write(
        &mut tx,
        RawVals::new().set_cell("foreign_vat", text("fr.56-78")),
    )
    .await
    .unwrap();
    assert_eq!(
        a.cell(&mut tx, "foreign_vat").await.unwrap(),
        text("FR5678")
    );
    assert_eq!(
        b.cell(&mut tx, "foreign_vat").await.unwrap(),
        text("FR5678")
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
