//! Product Variant and Attribute generation tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_addon_product::ProductTemplate;
mod support;

use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
    let h = support::base_harness().await;

    let mut tx = h.begin().await.unwrap();
    tx.commit().await.unwrap();

    h
}

#[tokio::test]
async fn test_variant_generation_always_matrix() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    // 1. Attribute Size (S, M, L) - mode 'always'
    let size_attr = attr_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Size".into()))
                    .set_cell("create_variant", Cell::Str("always".into())),
            ],
        )
        .await
        .unwrap();
    let size_attr_id = size_attr.rec_ids()[0].get();

    let val_s = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("S".into()))
                    .set_cell("attribute_id", Cell::Id(size_attr_id)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let val_m = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("M".into()))
                    .set_cell("attribute_id", Cell::Id(size_attr_id)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let val_l = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("L".into()))
                    .set_cell("attribute_id", Cell::Id(size_attr_id)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    // 2. Attribute Color (Red, Blue) - mode 'always'
    let col_attr = attr_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Color".into()))
                    .set_cell("create_variant", Cell::Str("always".into())),
            ],
        )
        .await
        .unwrap();
    let col_attr_id = col_attr.rec_ids()[0].get();

    let val_red = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Red".into()))
                    .set_cell("attribute_id", Cell::Id(col_attr_id)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let val_blue = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Blue".into()))
                    .set_cell("attribute_id", Cell::Id(col_attr_id)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    // 3. Template Shirt
    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("T-Shirt".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // 4. Lines on template
    ptal_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(size_attr_id))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(
                            vec![
                                RecId::from_real(val_s).unwrap(),
                                RecId::from_real(val_m).unwrap(),
                                RecId::from_real(val_l).unwrap(),
                            ]
                            .into(),
                        ),
                    ),
            ],
        )
        .await
        .unwrap();

    ptal_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(col_attr_id))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(
                            vec![
                                RecId::from_real(val_red).unwrap(),
                                RecId::from_real(val_blue).unwrap(),
                            ]
                            .into(),
                        ),
                    ),
            ],
        )
        .await
        .unwrap();

    // Verify exactly 3 x 2 = 6 variants exist
    let d = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variants = prod_model.search(&mut tx, &d).await.unwrap();
    assert_eq!(
        variants.len(),
        6,
        "Expected exactly 6 variants from 3x2 attribute lines"
    );
}

#[tokio::test]
async fn test_variant_generation_dynamic_deferred() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let dyn_attr = attr_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Custom Inscription".into()))
                    .set_cell("create_variant", Cell::Str("dynamic".into())),
            ],
        )
        .await
        .unwrap();
    let dyn_attr_id = dyn_attr.rec_ids()[0].get();

    let val1 = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Engraved".into()))
                    .set_cell("attribute_id", Cell::Id(dyn_attr_id)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Ring".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    ptal_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(dyn_attr_id))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(vec![RecId::from_real(val1).unwrap()].into()),
                    ),
            ],
        )
        .await
        .unwrap();

    // Mode dynamic: no Cartesian variants generated up front
    let d = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variants = prod_model.search(&mut tx, &d).await.unwrap();
    assert_eq!(
        variants.len(),
        0,
        "No variants should be generated up front for dynamic attributes"
    );
}

#[tokio::test]
async fn test_attribute_line_empty_values_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();

    let attr = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Material".into()))],
        )
        .await
        .unwrap();
    let attr_id = attr.rec_ids()[0].get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Box".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    let err = ptal_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(attr_id))
                    .set_cell("value_ids", Cell::Ids(vec![].into())),
            ],
        )
        .await;

    assert!(
        err.is_err(),
        "Empty value_ids on active line must be refused"
    );
}

#[tokio::test]
async fn test_attribute_line_mismatched_value_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();

    let attr1 = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Attr1".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let attr2 = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Attr2".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    let val_of_attr2 = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Val2".into()))
                    .set_cell("attribute_id", Cell::Id(attr2)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Prod".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    // Adding val_of_attr2 to line of attr1 must fail
    let err = ptal_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl))
                    .set_cell("attribute_id", Cell::Id(attr1))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(vec![RecId::from_real(val_of_attr2).unwrap()].into()),
                    ),
            ],
        )
        .await;

    assert!(
        err.is_err(),
        "Value belonging to different attribute must be refused"
    );
}

#[tokio::test]
async fn test_variant_exclusion_pruning() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();
    let ptav_model = tx
        .env()
        .records("product.template.attribute.value")
        .unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    // Color: Red, Blue
    let col = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Color".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let v_red = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Red".into()))
                    .set_cell("attribute_id", Cell::Id(col)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let v_blue = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Blue".into()))
                    .set_cell("attribute_id", Cell::Id(col)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    // Size: M, XL
    let sz = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Size".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let v_m = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("M".into()))
                    .set_cell("attribute_id", Cell::Id(sz)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let v_xl = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("XL".into()))
                    .set_cell("attribute_id", Cell::Id(sz)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Polo".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    ptal_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(col))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(
                            vec![
                                RecId::from_real(v_red).unwrap(),
                                RecId::from_real(v_blue).unwrap(),
                            ]
                            .into(),
                        ),
                    ),
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(sz))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(
                            vec![
                                RecId::from_real(v_m).unwrap(),
                                RecId::from_real(v_xl).unwrap(),
                            ]
                            .into(),
                        ),
                    ),
            ],
        )
        .await
        .unwrap();

    // Find PTAV for Red and PTAV for XL
    let d_red = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap()
        & Domain::condition("product_attribute_value_id", Operator::Eq, v_red).unwrap();
    let ptav_red = ptav_model.search(&mut tx, &d_red).await.unwrap();

    let d_xl = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap()
        & Domain::condition("product_attribute_value_id", Operator::Eq, v_xl).unwrap();
    let ptav_xl = ptav_model.search(&mut tx, &d_xl).await.unwrap();
    let ptav_xl_id = ptav_xl.rec_ids()[0].get();

    // Declare exclusion: Red excludes XL
    ptav_red
        .write(
            &mut tx,
            RawVals::new().set_cell(
                "excluded_value_ids",
                Cell::Ids(vec![RecId::from_real(ptav_xl_id).unwrap()].into()),
            ),
        )
        .await
        .unwrap();

    // Trigger matrix sync
    let tmpl_rec = tmpl_model.browse(vec![tmpl_id]).unwrap();
    let tmpl_set = RecordSet::<ProductTemplate>::try_from(tmpl_rec).unwrap();
    ProductTemplate::create_variant_matrix(&tmpl_set, &mut tx)
        .await
        .unwrap();

    // Without exclusion: 2x2 = 4 variants. With Red-XL excluded: exactly 3 variants.
    let d_var = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variants = prod_model.search(&mut tx, &d_var).await.unwrap();
    assert_eq!(
        variants.len(),
        3,
        "Expected 3 variants after Red-XL combination excluded"
    );
}

#[tokio::test]
async fn test_combination_indices_serialization() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();
    let ptav_model = tx
        .env()
        .records("product.template.attribute.value")
        .unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Test".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // Create 3 attributes with 1 value each
    let a1 = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("A1".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let v1 = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("V1".into()))
                    .set_cell("attribute_id", Cell::Id(a1)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    let a2 = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("A2".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let v2 = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("V2".into()))
                    .set_cell("attribute_id", Cell::Id(a2)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    let a3 = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("A3".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let v3 = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("V3".into()))
                    .set_cell("attribute_id", Cell::Id(a3)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    ptal_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(a1))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(vec![RecId::from_real(v1).unwrap()].into()),
                    ),
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(a2))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(vec![RecId::from_real(v2).unwrap()].into()),
                    ),
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(a3))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(vec![RecId::from_real(v3).unwrap()].into()),
                    ),
            ],
        )
        .await
        .unwrap();

    let d_ptav = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let ptavs = ptav_model.search(&mut tx, &d_ptav).await.unwrap();
    let mut ids: Vec<i64> = ptavs.rec_ids().iter().map(|r| r.get()).collect();
    assert_eq!(ids.len(), 3);
    ids.sort_unstable();

    // The always-mode lines already generated the one variant of this combination; archive it
    // so the combination can be created again by hand (only active variants are unique).
    let generated = prod_model
        .search(
            &mut tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(generated.len(), 1, "one generated variant for 1x1x1 values");
    generated
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap();

    // Create a variant with PTAV IDs intentionally out of order (reverse)
    let rev_ids: Vec<RecId> = ids
        .iter()
        .rev()
        .map(|&id| RecId::from_real(id).unwrap())
        .collect();
    let prod = prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell(
                        "product_template_attribute_value_ids",
                        Cell::Ids(rev_ids.into()),
                    ),
            ],
        )
        .await
        .unwrap();

    let expected = format!("{},{},{}", ids[0], ids[1], ids[2]);
    let indices = prod.cell(&mut tx, "combination_indices").await.unwrap();
    assert_eq!(indices, Cell::Str(expected.into()));
}

#[tokio::test]
async fn test_dynamic_variant_on_demand_instantiation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();
    let ptav_model = tx
        .env()
        .records("product.template.attribute.value")
        .unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let dyn_attr = attr_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Dyn".into()))
                    .set_cell("create_variant", Cell::Str("dynamic".into())),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let val_gold = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Gold".into()))
                    .set_cell("attribute_id", Cell::Id(dyn_attr)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Watch".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    ptal_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("attribute_id", Cell::Id(dyn_attr))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(vec![RecId::from_real(val_gold).unwrap()].into()),
                    ),
            ],
        )
        .await
        .unwrap();

    // Initially 0 variants
    let d = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    assert_eq!(prod_model.search(&mut tx, &d).await.unwrap().len(), 0);

    // Find PTAV
    let d_ptav = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let ptavs = ptav_model.search(&mut tx, &d_ptav).await.unwrap();
    let ptav_id = ptavs.rec_ids()[0].get();

    // Call on-demand instantiation
    let tmpl_rec = tmpl_model.browse(vec![tmpl_id]).unwrap();
    let tmpl_set = RecordSet::<ProductTemplate>::try_from(tmpl_rec).unwrap();
    let created_var =
        ProductTemplate::create_or_get_dynamic_variant(&tmpl_set, &mut tx, &[ptav_id])
            .await
            .unwrap();
    assert_eq!(created_var.len(), 1);

    // Calling again returns existing variant without creating duplicate
    let second_call =
        ProductTemplate::create_or_get_dynamic_variant(&tmpl_set, &mut tx, &[ptav_id])
            .await
            .unwrap();
    assert_eq!(
        second_call.rec_ids()[0].get(),
        created_var.rec_ids()[0].get()
    );
    assert_eq!(prod_model.search(&mut tx, &d).await.unwrap().len(), 1);
}
