// SPDX-License-Identifier: MIT OR Apache-2.0
//! Packaging helpers, context-scoped barcodes and transactional references.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_sdk::prelude::*;

async fn unit(tx: &mut Transaction, name: &str, root: i64, factor: i64) -> i64 {
    tx.env()
        .records("uom.uom")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("relative_uom_id", Cell::Id(root))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::from(factor))),
            ],
        )
        .await
        .unwrap()
        .ids()[0]
}
async fn product(tx: &mut Transaction, name: &str) -> (Records, Records) {
    let template = tx
        .env()
        .records("product.template")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap();
    let variants = tx
        .env()
        .records("product.product")
        .unwrap()
        .search(
            tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, template.ids()[0]).unwrap(),
        )
        .await
        .unwrap();
    (template, variants)
}
fn ids(cell: Cell) -> Result<Vec<i64>> {
    match cell {
        Cell::Ids(ids) => Ok(ids.iter().map(|id| id.get()).collect()),
        other => Err(Error::programming(format!(
            "Expected relation identities, got {other:?}."
        ))),
    }
}
async fn packaging(tx: &mut Transaction, product: i64, unit: i64, barcode: &str) -> Records {
    tx.env()
        .records("product.uom")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("product_id", Cell::Id(product))
                    .set_cell("uom_id", Cell::Id(unit))
                    .set_cell("barcode", Cell::Str(barcode.into())),
            ],
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn packaging_fields_and_main_unit_helpers() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let pack = unit(&mut tx, "Pack", root, 6).await;
    let (template, variant) = product(&mut tx, "Packaging").await;
    template
        .write(
            &mut tx,
            RawVals::new().set_cell(
                "uom_ids",
                Cell::Ids(vec![RecId::from_real(pack).unwrap()].into()),
            ),
        )
        .await
        .unwrap();
    variant
        .write(
            &mut tx,
            RawVals::new().set_cell(
                "extra_uom_ids",
                Cell::Ids(vec![RecId::from_real(pack).unwrap()].into()),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        ids(template.cell(&mut tx, "uom_ids").await.unwrap()).unwrap(),
        vec![pack]
    );
    assert_eq!(
        template.cell(&mut tx, "uom_name").await.unwrap(),
        Cell::Str("Units".into())
    );
    let main: Records = variant.call(&mut tx, "_get_main_uom", ()).await.unwrap();
    assert_eq!(main.ids(), vec![root]);
    let empty: Records = tx
        .env()
        .records("product.template")
        .unwrap()
        .call(&mut tx, "_get_available_uoms", ())
        .await
        .unwrap();
    assert!(empty.is_empty());
    template
        .write(
            &mut tx,
            RawVals::new().set_cell("type", Cell::Str("combo".into())),
        )
        .await
        .unwrap();
    let multiple: bool = variant
        .call(&mut tx, "_has_multiple_uoms", ())
        .await
        .unwrap();
    assert!(!multiple);
}

#[tokio::test]
async fn unit_screen_allowed_unit_is_the_packaging_unit() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let (_, p) = product(&mut tx, "Allowed").await;
    let row = packaging(&mut tx, p.ids()[0], root, "ALLOWED").await;
    let row = row.with_context([("active_model", Value::str("uom.uom"))]);
    assert_eq!(
        ids(row.cell(&mut tx, "allowed_uom_ids").await.unwrap()).unwrap(),
        vec![root]
    );
}

#[tokio::test]
async fn unit_barcode_relation_filters_context_and_keeps_inverse_writes() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let (_, a) = product(&mut tx, "A").await;
    let (_, b) = product(&mut tx, "B").await;
    let row_a = packaging(&mut tx, a.ids()[0], root, "CTX-A").await;
    let row_b = packaging(&mut tx, b.ids()[0], root, "CTX-B").await;
    let u = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .browse(vec![root])
        .unwrap();
    assert_eq!(
        ids(u
            .with_context([("product_id", Value::Int(a.ids()[0]))])
            .cell(&mut tx, "product_uom_ids")
            .await
            .unwrap())
        .unwrap(),
        row_a.ids()
    );
    let both = u.with_context([
        ("product_id", Value::Int(a.ids()[0])),
        ("product_ids", Value::list([Value::Int(b.ids()[0])])),
    ]);
    assert_eq!(
        ids(both.cell(&mut tx, "product_uom_ids").await.unwrap()).unwrap(),
        vec![row_a.ids()[0], row_b.ids()[0]]
    );
    assert_eq!(
        ids(u.cell(&mut tx, "product_uom_ids").await.unwrap())
            .unwrap()
            .len(),
        2
    );
    let other = unit(&mut tx, "Other", root, 2).await;
    let moved = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .browse(vec![other])
        .unwrap();
    moved
        .write(
            &mut tx,
            RawVals::new().set_cell(
                "product_uom_ids",
                Cell::Ids(vec![RecId::from_real(row_a.ids()[0]).unwrap()].into()),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        row_a.cell(&mut tx, "uom_id").await.unwrap(),
        Cell::Id(other)
    );
    assert_eq!(
        ids(u
            .with_context([("product_id", Value::Int(a.ids()[0]))])
            .cell(&mut tx, "product_uom_ids")
            .await
            .unwrap())
        .unwrap(),
        Vec::<i64>::new()
    );
    let scoped = moved.with_context([("product_id", Value::Int(b.ids()[0]))]);
    scoped
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "product_uom_ids",
                [Command::create(
                    RawVals::new()
                        .set_cell("product_id", Cell::Id(b.ids()[0]))
                        .set_cell("barcode", Cell::Str("CTX-CREATED".into())),
                )],
            ),
        )
        .await
        .unwrap();
    let created = tx
        .env()
        .records("product.uom")
        .unwrap()
        .search(
            &mut tx,
            &Domain::condition("barcode", Operator::Eq, "CTX-CREATED").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        created.cell(&mut tx, "uom_id").await.unwrap(),
        Cell::Id(other)
    );
    scoped
        .write(
            &mut tx,
            RawVals::new().set_commands("product_uom_ids", [Command::set([])]),
        )
        .await
        .unwrap();
    assert_eq!(
        row_a.cell(&mut tx, "uom_id").await.unwrap(),
        Cell::Id(other)
    );
}

#[tokio::test]
async fn template_and_packaging_register_permanent_unit_use() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let pack = unit(&mut tx, "Protected", root, 6).await;
    let (_, p) = product(&mut tx, "Reference").await;
    let row = packaging(&mut tx, p.ids()[0], pack, "PROTECTED").await;
    let template_pack = unit(&mut tx, "Template command pack", root, 4).await;
    let variant_pack = unit(&mut tx, "Variant command pack", root, 5).await;
    let template = p.related(&mut tx, "product_tmpl_id").await.unwrap();
    template
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "uom_ids",
                [Command::link(RecId::from_real(template_pack).unwrap())],
            ),
        )
        .await
        .unwrap();
    p.write(
        &mut tx,
        RawVals::new().set_commands(
            "extra_uom_ids",
            [Command::link(RecId::from_real(variant_pack).unwrap())],
        ),
    )
    .await
    .unwrap();
    row.unlink(&mut tx).await.unwrap();
    let rows = tx
        .query("SELECT uom_id FROM uom_reference_use ORDER BY uom_id", &[])
        .await
        .unwrap();
    let used: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
    assert!(used.contains(&root));
    assert!(used.contains(&pack));
    assert!(used.contains(&template_pack));
    assert!(used.contains(&variant_pack));
}

#[tokio::test]
async fn base_unit_change_hooks_are_dispatchable() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let (_, p) = product(&mut tx, "Hook").await;
    let result: bool = p.call(&mut tx, "_update_uom", (root,)).await.unwrap();
    assert!(result);
    let warn: bool = p.call(&mut tx, "_trigger_uom_warning", ()).await.unwrap();
    assert!(!warn);
}

#[tokio::test]
async fn packaging_actions_scope_the_product() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let (template, p) = product(&mut tx, "Action").await;
    let action: Value = template
        .call(&mut tx, "action_open_packaging_barcodes", ())
        .await
        .unwrap();
    let Value::Map(action) = action else {
        panic!("action map expected")
    };
    assert_eq!(action["res_model"], Value::str("product.uom"));
    assert_eq!(
        action["domain"],
        Value::list([Value::list([
            Value::str("product_id"),
            Value::str("in"),
            Value::list([Value::Int(p.ids()[0])])
        ])])
    );
    let Value::Map(ctx) = &action["context"] else {
        panic!("context map expected")
    };
    assert_eq!(ctx["default_product_id"], Value::Int(p.ids()[0]));
    let action: Value = p
        .call(&mut tx, "action_open_packaging_barcodes", ())
        .await
        .unwrap();
    let Value::Map(action) = action else {
        panic!("action map expected")
    };
    assert_eq!(
        action["domain"],
        Value::list([Value::list([
            Value::str("product_id"),
            Value::str("="),
            Value::Int(p.ids()[0])
        ])])
    );
}

#[tokio::test]
async fn allowed_units_include_applicable_sellers() {
    let h = support::base_harness().await;
    support::seed_company(&h, "Packaging company").await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let vendor_unit = unit(&mut tx, "Vendor pack", root, 8).await;
    let (template, variant) = product(&mut tx, "Seller units").await;
    let env = support::company_env(&mut tx).await;
    let partner = env
        .records("res.partner")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Vendor".into()))],
        )
        .await
        .unwrap();
    env.records("product.supplierinfo")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(partner.ids()[0]))
                    .set_cell("product_tmpl_id", Cell::Id(template.ids()[0]))
                    .set_cell("uom_id", Cell::Id(vendor_unit)),
            ],
        )
        .await
        .unwrap();
    let row = packaging(&mut tx, variant.ids()[0], root, "SELLER-UNIT").await;
    assert_eq!(
        ids(row.cell(&mut tx, "allowed_uom_ids").await.unwrap()).unwrap(),
        vec![root, vendor_unit]
    );
}

use erp_addon_product::ProductProduct;
#[extend(ProductProduct, addon = "test_product")]
struct UnitChangeProbe {}
#[methods(ProductProduct, addon = "test_product")]
impl UnitChangeProbe {
    #[method(override)]
    async fn _update_uom(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
        target: i64,
        sup: Super,
    ) -> Result<bool> {
        if let Some(Value::Str(mode)) = this.env().context().get("test_unit_hook") {
            if this.env().context().get("skip_uom_conversion") != Some(Value::Bool(true)) {
                return Err(Error::programming("Unit probe needs skip_uom_conversion."));
            }
            this.as_records()
                .write(
                    tx,
                    RawVals::new().set_cell("default_code", Cell::Str("hook-called".into())),
                )
                .await?;
            if mode.as_ref() == "refuse" {
                return Err(Error::user("Unit probe refused replacement."));
            }
            for row in this.iter() {
                if row.cell(tx, "uom_id").await? == Cell::Id(target) {
                    return Err(Error::programming(
                        "Unit probe must run before template write.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (target,)).await
    }
    #[method(override)]
    async fn _trigger_uom_warning(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<bool> {
        if this.env().context().get("test_unit_warning") == Some(Value::Bool(true)) {
            return Ok(true);
        }
        sup.call(tx, this.as_records(), ()).await
    }
}

#[tokio::test]
async fn unit_change_dispatches_before_write_and_rolls_back_refusal() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let target = unit(&mut tx, "Dozen", root, 12).await;
    let (t, p) = product(&mut tx, "Changing unit").await;
    let refusal = tx
        .savepoint(async |tx| {
            t.with_context([("test_unit_hook", Value::str("refuse"))])
                .write(tx, RawVals::new().set_cell("uom_id", Cell::Id(target)))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(refusal.kind(), erp_sdk::ErrorKind::UserError);
    assert_eq!(refusal.message(), "Unit probe refused replacement.");
    assert_eq!(p.cell(&mut tx, "uom_id").await.unwrap(), Cell::Id(root));
    assert_eq!(p.cell(&mut tx, "default_code").await.unwrap(), Cell::Null);
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM uom_reference_use WHERE uom_id = $1",
            &[&target]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    t.with_context([("test_unit_hook", Value::str("record"))])
        .write(&mut tx, RawVals::new().set_cell("uom_id", Cell::Id(target)))
        .await
        .unwrap();
    assert_eq!(p.cell(&mut tx, "uom_id").await.unwrap(), Cell::Id(target));
    assert_eq!(
        p.cell(&mut tx, "default_code").await.unwrap(),
        Cell::Str("hook-called".into())
    );
    // A failing override proves no hook runs on an unchanged assignment.
    t.with_context([("test_unit_hook", Value::str("refuse"))])
        .write(&mut tx, RawVals::new().set_cell("uom_id", Cell::Id(target)))
        .await
        .unwrap();
}

#[tokio::test]
async fn replacement_variants_inherit_extra_packaging() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let pack = unit(&mut tx, "Replacement pack", root, 3).await;
    let (t, p) = product(&mut tx, "Replacement").await;
    p.write(
        &mut tx,
        RawVals::new().set_cell(
            "extra_uom_ids",
            Cell::Ids(vec![RecId::from_real(pack).unwrap()].into()),
        ),
    )
    .await
    .unwrap();
    let attr = tx
        .env()
        .records("product.attribute")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Size".into()))],
        )
        .await
        .unwrap();
    let values = tx
        .env()
        .records("product.attribute.value")
        .unwrap()
        .create(
            &mut tx,
            ["Small", "Large"]
                .into_iter()
                .map(|name| {
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("attribute_id", Cell::Id(attr.ids()[0]))
                })
                .collect(),
        )
        .await
        .unwrap();
    tx.env()
        .records("product.template.attribute.line")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(t.ids()[0]))
                    .set_cell("attribute_id", Cell::Id(attr.ids()[0]))
                    .set_cell("value_ids", Cell::Ids(values.rec_ids().to_vec().into())),
            ],
        )
        .await
        .unwrap();
    let current = tx
        .env()
        .records("product.product")
        .unwrap()
        .search(
            &mut tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, t.ids()[0]).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(current.len(), 2);
    for row in current.iter() {
        assert_eq!(
            ids(row.cell(&mut tx, "extra_uom_ids").await.unwrap()).unwrap(),
            vec![pack]
        );
    }
}

#[tokio::test]
async fn unit_warning_is_conditional_and_does_not_change_stored_units() {
    use erp_sdk::ValueMap;
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let target = unit(&mut tx, "Warning pack", root, 6).await;
    let (t, _) = product(&mut tx, "Warning").await;
    let spec_value = Value::Map(Arc::new(ValueMap::from([(
        "uom_id".into(),
        Value::Map(Arc::new(ValueMap::new())),
    )])));
    let spec = erp_sdk::web::Spec::from_value(&spec_value).unwrap();
    let mut run = async |record: Records, id: i64| {
        record
            .onchange(
                &mut tx,
                ValueMap::from([("uom_id".into(), Value::Int(id))]),
                vec!["uom_id".into()],
                spec.clone(),
            )
            .await
            .unwrap()
    };
    let base = run(t.clone(), target).await;
    let unchanged = run(
        t.with_context([("test_unit_warning", Value::Bool(true))]),
        root,
    )
    .await;
    let warning = run(
        t.with_context([("test_unit_warning", Value::Bool(true))]),
        target,
    )
    .await;
    for value in [base, unchanged] {
        let Value::Map(result) = value else {
            panic!("onchange map expected")
        };
        assert!(!result.contains_key("warning"));
    }
    let Value::Map(result) = warning else {
        panic!("onchange map expected")
    };
    let Value::Map(warning) = &result["warning"] else {
        panic!("warning expected")
    };
    assert_eq!(warning["title"], Value::str("Changing the product unit"));
    assert_eq!(
        warning["message"],
        Value::str(
            "Existing documents will replace Units with Warning pack one for one. Their quantities will stay the same. Review the affected documents before saving."
        )
    );
    assert_eq!(t.cell(&mut tx, "uom_id").await.unwrap(), Cell::Id(root));
}

#[tokio::test]
async fn draft_unit_has_no_persisted_barcode_rows() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("uom.uom").unwrap();
    let draft = tx
        .new_record(&model, RawVals::new(), None, None)
        .await
        .unwrap();
    assert!(
        ids(draft.cell(&mut tx, "product_uom_ids").await.unwrap())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unit_actions_keep_the_unit_domain_and_validate_context() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let (_, p) = product(&mut tx, "Unit action").await;
    let u = tx.env().records("uom.uom").unwrap().browse([root]).unwrap();
    for (ctx, expected) in [
        (
            vec![("product_id", Value::Int(p.ids()[0]))],
            Value::list([Value::list([
                Value::str("uom_id"),
                Value::str("="),
                Value::Int(root),
            ])]),
        ),
        (
            vec![("product_ids", Value::list([Value::Int(p.ids()[0])]))],
            Value::list([
                Value::list([Value::str("uom_id"), Value::str("="), Value::Int(root)]),
                Value::list([
                    Value::str("product_id"),
                    Value::str("in"),
                    Value::list([Value::Int(p.ids()[0])]),
                ]),
            ]),
        ),
    ] {
        let action: Value = u
            .with_context(ctx)
            .call(&mut tx, "action_open_packaging_barcodes", ())
            .await
            .unwrap();
        let Value::Map(action) = action else {
            panic!("action map expected")
        };
        assert_eq!(action["domain"], expected);
        let Value::Map(context) = &action["context"] else {
            panic!("context map expected")
        };
        assert_eq!(context["default_uom_id"], Value::Int(root));
    }
    let error = tx
        .savepoint(async |tx| {
            u.with_context([("product_ids", Value::str("bad"))])
                .cell(tx, "product_uom_ids")
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Packaging context product_ids must be an integer list."
    );
}

#[tokio::test]
async fn a_template_without_variants_has_no_packaging_creation_fallback() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = tx
        .env()
        .records("product.template")
        .unwrap()
        .with_context([("create_product_product", Value::Bool(false))])
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("No variants".into()))],
        )
        .await
        .unwrap();
    let error = t
        .call::<(), Value>(&mut tx, "action_open_packaging_barcodes", ())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "This product has no visible variant for packaging barcodes."
    );
}

#[tokio::test]
async fn draft_template_available_units_preserve_relational_origins() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let pack = unit(&mut tx, "Draft packaging choice", root, 6).await;
    let model = tx
        .env()
        .records("product.template")
        .unwrap()
        .with_context([("test_multi_uom", Value::Bool(true))]);
    let draft = tx
        .new_record(
            &model,
            RawVals::new()
                .set_cell("uom_id", Cell::Id(root))
                .set_commands("uom_ids", [Command::link(RecId::from_real(pack).unwrap())]),
            None,
            None,
        )
        .await
        .unwrap();
    let available: Records = draft
        .call(&mut tx, "_get_available_uoms", ())
        .await
        .unwrap();
    assert_eq!(available.ids(), vec![root, pack]);
}

#[tokio::test]
async fn committed_catalog_use_fences_an_independent_stale_transaction() {
    let h = support::base_harness().await;
    let root = support::unit_id(&h).await;
    let mut seed = h.begin().await.unwrap();
    let pack = unit(&mut seed, "Concurrent catalog unit", root, 6).await;
    seed.commit().await.unwrap();
    drop(seed);
    let mut stale = h.begin().await.unwrap();
    stale
        .query("SELECT singleton FROM uom_reference_fence", &[])
        .await
        .unwrap();
    let mut writer = h.begin().await.unwrap();
    writer
        .env()
        .records("product.template")
        .unwrap()
        .create(
            &mut writer,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Committed catalog use".into()))
                    .set_cell("uom_id", Cell::Id(pack)),
            ],
        )
        .await
        .unwrap();
    writer.commit().await.unwrap();
    drop(writer);
    let error = stale
        .env()
        .records("uom.uom")
        .unwrap()
        .browse([pack])
        .unwrap()
        .write(
            &mut stale,
            RawVals::new().set_cell("relative_factor", Cell::Decimal(Decimal::from(7))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    assert_eq!(
        error.message(),
        "could not serialize access due to concurrent update"
    );
    stale.rollback().await.unwrap();
    drop(stale);
    let mut fresh = h.begin().await.unwrap();
    let error = fresh
        .env()
        .records("uom.uom")
        .unwrap()
        .browse([pack])
        .unwrap()
        .write(
            &mut fresh,
            RawVals::new().set_cell("relative_factor", Cell::Decimal(Decimal::from(7))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "A referenced unit of measure would change. Create a new unit instead."
    );
}
