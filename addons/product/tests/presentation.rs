//! How product records present themselves: template attribute value names and the variants
//! action of the template form.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_addon_product::{ProductTemplate, ProductTemplateMethods};
use erp_sdk::prelude::*;

/// A template "Shirt" with one attribute line "Size" holding S and M (two variants).
async fn shirt(tx: &mut Transaction) -> (i64, Vec<i64>) {
    let env = tx.env().clone();
    let attr = env
        .records("product.attribute")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Size".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let mut values = Vec::new();
    for name in ["S", "M"] {
        values.push(
            env.records("product.attribute.value")
                .unwrap()
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", Cell::Str(name.into()))
                            .set_cell("attribute_id", Cell::Id(attr)),
                    ],
                )
                .await
                .unwrap()
                .rec_ids()[0],
        );
    }
    let tmpl = env
        .records("product.template")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Shirt".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    env.records("product.template.attribute.line")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl))
                    .set_cell("attribute_id", Cell::Id(attr))
                    .set_cell("value_ids", Cell::Ids(values.into())),
            ],
        )
        .await
        .unwrap();
    let ptavs = env
        .records("product.template.attribute.value")
        .unwrap()
        .search(
            tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, tmpl).unwrap(),
        )
        .await
        .unwrap()
        .rec_ids()
        .iter()
        .map(|id| id.get())
        .collect();
    (tmpl, ptavs)
}

/// when the context sets `display_attribute_name` to false
#[tokio::test]
async fn template_attribute_value_names() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, ptavs) = shirt(&mut tx).await;
    let model = tx
        .env()
        .records("product.template.attribute.value")
        .unwrap();
    let mut names = Vec::new();
    let mut bare = Vec::new();
    for id in &ptavs {
        names.push(
            model
                .browse(vec![*id])
                .unwrap()
                .cell(&mut tx, "display_name")
                .await
                .unwrap(),
        );
        bare.push(
            model
                .with_context([("display_attribute_name", Value::Bool(false))])
                .browse(vec![*id])
                .unwrap()
                .cell(&mut tx, "display_name")
                .await
                .unwrap(),
        );
    }
    names.sort_by_key(|c| format!("{c:?}"));
    bare.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(
        names,
        [Cell::Str("Size: M".into()), Cell::Str("Size: S".into())]
    );
    assert_eq!(bare, [Cell::Str("M".into()), Cell::Str("S".into())]);
}

/// admission by the web client are checked by `every_product_view_is_served_to_a_product_manager`
/// in erp-cli): the action opens the template's own variants with the template as default
#[tokio::test]
async fn the_variants_button_opens_the_templates_variants() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let (tmpl, _) = shirt(&mut tx).await;
    let set = RecordSet::<ProductTemplate>::try_from(
        tx.env()
            .records("product.template")
            .unwrap()
            .browse(vec![tmpl])
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        set.as_records()
            .cell(&mut tx, "product_variant_count")
            .await
            .unwrap(),
        Cell::Int(2)
    );
    let action = set.action_open_variants(&mut tx).await.unwrap();
    let Value::Map(action) = action else {
        panic!("an action map: {action:?}")
    };
    assert_eq!(
        action.get("res_model"),
        Some(&Value::str("product.product"))
    );
    let domain = Value::list([Value::list([
        Value::str("product_tmpl_id"),
        Value::str("="),
        Value::Int(tmpl),
    ])]);
    assert_eq!(action.get("domain"), Some(&domain));
    let Some(Value::Map(context)) = action.get("context") else {
        panic!("a context map")
    };
    // keys the web client's action normalizer admits: boolean `create` and default_<field>
    assert_eq!(context.len(), 2, "{context:?}");
    assert_eq!(context.get("create"), Some(&Value::Bool(false)));
    assert_eq!(
        context.get("default_product_tmpl_id"),
        Some(&Value::Int(tmpl))
    );
}

/// technical names ("Categ", "Lst Price", "Default Code")
#[test]
fn product_fields_carry_reference_labels() {
    let registry = erp_sdk::RegistryBuilder::new([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
    ])
    .build()
    .expect("real linked registry");
    let expected = [
        ("product.template", "categ_id", "Product Category"),
        ("product.template", "uom_id", "Unit"),
        ("product.template", "list_price", "Sales Price"),
        ("product.template", "standard_price", "Cost"),
        ("product.template", "default_code", "Internal Reference"),
        ("product.template", "type", "Product Type"),
        ("product.template", "is_storable", "Track Inventory"),
        (
            "product.template",
            "attribute_line_ids",
            "Product Attributes",
        ),
        ("product.template", "seller_ids", "Vendors"),
        (
            "product.template",
            "product_variant_count",
            "# Product Variants",
        ),
        ("product.product", "lst_price", "Sales Price"),
        ("product.product", "default_code", "Internal Reference"),
        ("product.product", "standard_price", "Cost"),
        ("product.product", "price_extra", "Variant Price Extra"),
        (
            "product.product",
            "product_template_attribute_value_ids",
            "Attribute Values",
        ),
        (
            "product.template.attribute.line",
            "attribute_id",
            "Attribute",
        ),
        ("product.template.attribute.line", "value_ids", "Values"),
        ("product.supplierinfo", "partner_id", "Vendor"),
        ("product.supplierinfo", "min_qty", "Quantity"),
        ("product.supplierinfo", "price", "Unit Price"),
        ("product.supplierinfo", "delay", "Lead Time"),
        ("product.pricelist.item", "min_quantity", "Min. Quantity"),
        ("product.pricelist.item", "applied_on", "Apply On"),
        ("product.pricelist.item", "base", "Based on"),
    ];
    let wrong: Vec<String> = expected
        .iter()
        .filter_map(|(model, field, label)| {
            let actual = &registry
                .model(model)
                .expect("product model")
                .field(field)
                .expect("declared field")
                .string;
            (actual != label).then(|| format!("{model}.{field}: {actual:?}, expected {label:?}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}
