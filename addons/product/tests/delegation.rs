//! Product Template and Variant delegation tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

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
async fn test_variant_delegation_inherits() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Acoustic Bloc Screens".into()))
                    .set_cell("list_price", Cell::Decimal(Decimal::from(250))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // Verify default variant was created automatically
    let variants = prod_model
        .search(
            &mut tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        variants.len(),
        1,
        "Exactly one variant should be created automatically"
    );

    // Reading inherited field 'name' on variant delegates to template
    let var_name = variants.cell(&mut tx, "name").await.unwrap();
    assert_eq!(var_name, Cell::Str("Acoustic Bloc Screens".into()));
}

#[tokio::test]
async fn test_auto_create_single_default_variant() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Office Chair".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    let variants = prod_model
        .search(
            &mut tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(variants.len(), 1);
}

#[tokio::test]
async fn test_synchronized_scalars_single_variant() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Desk Lamp".into()))
                    .set_cell("default_code", Cell::Str("LAMP-01".into()))
                    .set_cell("barcode", Cell::Str("1234567890128".into())),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    let variants = prod_model
        .search(
            &mut tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(variants.len(), 1);

    let var_code = variants.cell(&mut tx, "default_code").await.unwrap();
    assert_eq!(var_code, Cell::Str("LAMP-01".into()));

    let var_bc = variants.cell(&mut tx, "barcode").await.unwrap();
    assert_eq!(var_bc, Cell::Str("1234567890128".into()));
}

/// attribute part is in `test_variant_display_name_lists_attribute_values`)
#[tokio::test]
async fn test_variant_display_name_formatting() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Conference Table".into()))
                    .set_cell("default_code", Cell::Str("CONF-01".into())),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    let variants = prod_model
        .search(
            &mut tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap(),
        )
        .await
        .unwrap();
    let display_name = variants.cell(&mut tx, "display_name").await.unwrap();
    assert_eq!(display_name, Cell::Str("[CONF-01] Conference Table".into()));
}

/// attribute order then value order, leaving out values of single-value lines; the code prefix
/// comes from the variant's own internal reference
#[tokio::test]
async fn test_variant_display_name_lists_attribute_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    // Created in this order on purpose: the attribute sequence, not the creation order, decides.
    let mut attribute = async |name: &str, sequence: i32| {
        attr_model
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("sequence", Cell::Int(sequence)),
                ],
            )
            .await
            .unwrap()
            .rec_ids()[0]
            .get()
    };
    let color = attribute("Colour", 20).await;
    let size = attribute("Seat size", 10).await;
    let material = attribute("Frame", 30).await;
    let mut values = Vec::new();
    for (attr, name, sequence) in [
        (color, "Blue", 2),
        (color, "Red", 1),
        (size, "M", 2),
        (size, "S", 1),
        (material, "Oak", 1),
    ] {
        let id = val_model
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str(name.into()))
                        .set_cell("attribute_id", Cell::Id(attr))
                        .set_cell("sequence", Cell::Int(sequence)),
                ],
            )
            .await
            .unwrap()
            .rec_ids()[0]
            .get();
        values.push((attr, id));
    }
    let tmpl_id = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Chair".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    for attr in [color, size, material] {
        let ids: Vec<RecId> = values
            .iter()
            .filter(|(a, _)| *a == attr)
            .map(|(_, v)| RecId::from_real(*v).unwrap())
            .collect();
        ptal_model
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                        .set_cell("attribute_id", Cell::Id(attr))
                        .set_cell("value_ids", Cell::Ids(ids.into())),
                ],
            )
            .await
            .unwrap();
    }

    let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variants = prod_model.search(&mut tx, &domain).await.unwrap();
    assert_eq!(variants.len(), 4);
    let mut names = Vec::new();
    for id in variants.rec_ids() {
        let one = prod_model.browse(vec![id.get()]).unwrap();
        match one.cell(&mut tx, "display_name").await.unwrap() {
            Cell::Str(name) => names.push(name.to_string()),
            other => panic!("display_name is not text: {other:?}"),
        }
    }
    names.sort();
    assert_eq!(
        names,
        [
            "Chair (M, Blue)",
            "Chair (M, Red)",
            "Chair (S, Blue)",
            "Chair (S, Red)"
        ],
        "the single-value Frame line is left out and Seat size (sequence 10) comes first"
    );

    let first = prod_model
        .browse(vec![variants.rec_ids()[0].get()])
        .unwrap();
    let plain = match first.cell(&mut tx, "display_name").await.unwrap() {
        Cell::Str(name) => name.to_string(),
        other => panic!("display_name is not text: {other:?}"),
    };
    first
        .write(
            &mut tx,
            RawVals::new().set_cell("default_code", Cell::Str("CH-01".into())),
        )
        .await
        .unwrap();
    let coded = first.cell(&mut tx, "display_name").await.unwrap();
    assert_eq!(coded, Cell::Str(format!("[CH-01] {plain}").into()));
}

#[tokio::test]
async fn test_default_template_list_price() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Sample Product".into()))],
        )
        .await
        .unwrap();
    let price = tmpl.cell(&mut tx, "list_price").await.unwrap();
    assert_eq!(price, Cell::Decimal(Decimal::ONE));
}

/// follows a later change of the template list_price (the stored value depends on both)
#[tokio::test]
async fn test_variant_lst_price_compute() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let attr_model = tx.env().records("product.attribute").unwrap();
    let val_model = tx.env().records("product.attribute.value").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let ptal_model = tx.env().records("product.template.attribute.line").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Monitor".into()))
                    .set_cell("list_price", Cell::Decimal(Decimal::new(19999, 2))),
            ],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    let attr = attr_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Panel".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let value = val_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("OLED".into()))
                    .set_cell("attribute_id", Cell::Id(attr))
                    .set_cell("default_extra_price", Cell::Decimal(Decimal::new(1000, 2))),
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
                    .set_cell("attribute_id", Cell::Id(attr))
                    .set_cell(
                        "value_ids",
                        Cell::Ids(vec![RecId::from_real(value).unwrap()].into()),
                    ),
            ],
        )
        .await
        .unwrap();

    let domain = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variants = prod_model.search(&mut tx, &domain).await.unwrap();
    assert_eq!(variants.len(), 1);
    let extra = variants.cell(&mut tx, "price_extra").await.unwrap();
    assert_eq!(extra, Cell::Decimal(Decimal::new(1000, 2)));
    let list_price = tmpl.cell(&mut tx, "list_price").await.unwrap();
    let (Cell::Decimal(list_price), Cell::Decimal(extra)) = (list_price, extra) else {
        panic!("decimal cells expected")
    };
    let lst_price = variants.cell(&mut tx, "lst_price").await.unwrap();
    assert_eq!(lst_price, Cell::Decimal(list_price + extra));

    // M3: a later change of the template price reaches the stored variant price
    tmpl.write(
        &mut tx,
        RawVals::new().set_cell("list_price", Cell::Decimal(Decimal::new(30000, 2))),
    )
    .await
    .unwrap();
    let after = variants.cell(&mut tx, "lst_price").await.unwrap();
    assert_eq!(after, Cell::Decimal(Decimal::new(30000, 2) + extra));
}

/// Template `is_storable` is reset when the type is not `consu` (spec field table, product.md 4.1)
#[tokio::test]
async fn test_is_storable_reset_for_non_goods() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Stocked".into()))
                    .set_cell("is_storable", Cell::Bool(true)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        tmpl.cell(&mut tx, "type").await.unwrap(),
        Cell::Str("consu".into())
    );
    assert_eq!(
        tmpl.cell(&mut tx, "is_storable").await.unwrap(),
        Cell::Bool(true)
    );
    tmpl.write(
        &mut tx,
        RawVals::new().set_cell("type", Cell::Str("service".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        tmpl.cell(&mut tx, "is_storable").await.unwrap(),
        Cell::Bool(false),
        "a service cannot be tracked in inventory"
    );
}

/// A template created without a unit takes `uom.product_uom_unit`, found by its external id
#[tokio::test]
async fn test_default_unit_is_the_seeded_unit_by_external_id() {
    let h = harness().await;
    let unit = support::unit_id(&h).await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let uom_model = tx.env().records("uom.uom").unwrap();
    // another unit exists and sorts first: the default is still the one named by the external id
    uom_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("AAA other root".into()))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::ONE)),
            ],
        )
        .await
        .unwrap();
    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Defaulted".into()))],
        )
        .await
        .unwrap();
    assert_eq!(tmpl.cell(&mut tx, "uom_id").await.unwrap(), Cell::Id(unit));
}

/// The default unit is never invented: without the external id the create is refused
#[tokio::test]
async fn test_missing_default_unit_fails_loudly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute("DELETE FROM ir_model_data", &[]).await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let err = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("NoUnit".into()))],
        )
        .await
        .unwrap_err();
    assert!(
        err.message().contains("uom.product_uom_unit")
            || err.message().contains("product_uom_unit"),
        "unexpected error: {}",
        err.message()
    );
}
