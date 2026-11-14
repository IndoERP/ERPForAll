// SPDX-License-Identifier: MIT OR Apache-2.0
//! Attribute metadata, usage guards and custom-value identity.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;

async fn named(tx: &mut Transaction, env: &Env, model: &str, name: &str) -> Records {
    env.records(model)
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap()
}
async fn value(tx: &mut Transaction, env: &Env, attribute: i64, name: &str) -> Records {
    env.records("product.attribute.value")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("attribute_id", Cell::Id(attribute)),
            ],
        )
        .await
        .unwrap()
}
async fn line(
    tx: &mut Transaction,
    env: &Env,
    template: i64,
    attribute: i64,
    value: &Records,
) -> Records {
    env.records("product.template.attribute.line")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(template))
                    .set_cell("attribute_id", Cell::Id(attribute))
                    .set_cell("value_ids", Cell::Ids(value.rec_ids().to_vec().into())),
            ],
        )
        .await
        .unwrap()
}
fn user_error(error: Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn attribute_defaults_display_choices_and_order() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().clone();
    let a = named(&mut tx, &env, "product.attribute", "Zebra").await;
    let b = named(&mut tx, &env, "product.attribute", "Alpha").await;
    assert_eq!(a.cell(&mut tx, "sequence").await.unwrap(), Cell::Int(20));
    assert_eq!(a.cell(&mut tx, "active").await.unwrap(), Cell::Bool(true));
    assert_eq!(
        a.cell(&mut tx, "create_variant").await.unwrap(),
        Cell::Str("always".into())
    );
    assert_eq!(
        a.cell(&mut tx, "display_type").await.unwrap(),
        Cell::Str("radio".into())
    );
    let domain = Domain::condition("id", Operator::In, vec![a.ids()[0], b.ids()[0]]).unwrap();
    assert_eq!(
        env.records("product.attribute")
            .unwrap()
            .search(&mut tx, &domain)
            .await
            .unwrap()
            .ids(),
        [a.ids()[0], b.ids()[0]]
    );
    b.write(&mut tx, RawVals::new().set_cell("sequence", Cell::Int(2)))
        .await
        .unwrap();
    assert_eq!(
        env.records("product.attribute")
            .unwrap()
            .search(&mut tx, &domain)
            .await
            .unwrap()
            .ids(),
        [b.ids()[0], a.ids()[0]]
    );
    for display in [
        "radio", "pills", "select", "color", "image", "range", "multi",
    ] {
        a.write(
            &mut tx,
            RawVals::new()
                .set_cell("create_variant", Cell::Str("no_variant".into()))
                .set_cell("display_type", Cell::Str(display.into())),
        )
        .await
        .unwrap();
        assert_eq!(
            a.cell(&mut tx, "display_type").await.unwrap(),
            Cell::Str(display.into())
        );
    }
}

#[tokio::test]
async fn used_attribute_guards_count_lines_and_follow_template_activity() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Attributes").await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let a = named(&mut tx, &env, "product.attribute", "Size").await;
    let v = value(&mut tx, &env, a.ids()[0], "S").await;
    let t = named(&mut tx, &env, "product.template", "Shirt").await;
    line(&mut tx, &env, t.ids()[0], a.ids()[0], &v).await;
    line(&mut tx, &env, t.ids()[0], a.ids()[0], &v).await;
    assert_eq!(
        a.cell(&mut tx, "number_related_products").await.unwrap(),
        Cell::Int(2)
    );
    a.write(
        &mut tx,
        RawVals::new().set_cell("create_variant", Cell::Str("always".into())),
    )
    .await
    .unwrap();
    let e = tx
        .savepoint(async |tx| {
            a.write(
                tx,
                RawVals::new().set_cell("create_variant", Cell::Str("dynamic".into())),
            )
            .await
        })
        .await
        .unwrap_err();
    user_error(
        e,
        "Cannot change variant creation mode for attribute Size while active products use it: Shirt.",
    );
    let e = tx
        .savepoint(async |tx| a.unlink(tx).await)
        .await
        .unwrap_err();
    user_error(
        e,
        "Cannot delete attribute Size while active products use it: Shirt.",
    );
    let e = tx
        .savepoint(async |tx| a.action_archive(tx).await)
        .await
        .unwrap_err();
    user_error(
        e,
        "Products still use this attribute, so it cannot be archived. Remove it from those products first.",
    );
    t.action_archive(&mut tx).await.unwrap();
    assert_eq!(
        a.cell(&mut tx, "number_related_products").await.unwrap(),
        Cell::Int(0)
    );
    a.action_archive(&mut tx).await.unwrap();
    assert_eq!(a.cell(&mut tx, "active").await.unwrap(), Cell::Bool(false));
}

#[tokio::test]
async fn value_metadata_names_and_attribute_context_are_distinct() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().clone();
    let a = named(&mut tx, &env, "product.attribute", "Colour").await;
    let v = value(&mut tx, &env, a.ids()[0], "Blå").await;
    assert_eq!(v.cell(&mut tx, "active").await.unwrap(), Cell::Bool(true));
    assert_eq!(
        v.cell(&mut tx, "is_custom").await.unwrap(),
        Cell::Bool(false)
    );
    assert_eq!(
        v.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("Colour: Blå".into())
    );
    assert_eq!(
        v.with_context([("show_attribute", Value::Bool(false))])
            .cell(&mut tx, "display_name")
            .await
            .unwrap(),
        Cell::Str("Blå".into())
    );
    assert_eq!(
        v.with_context([("display_attribute_name", Value::Bool(false))])
            .cell(&mut tx, "display_name")
            .await
            .unwrap(),
        Cell::Str("Colour: Blå".into())
    );
    assert!(matches!(
        v.cell(&mut tx, "color").await.unwrap(),
        Cell::Int(1..=11)
    ));
    a.write(
        &mut tx,
        RawVals::new().set_cell("display_type", Cell::Str("range".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        v.cell(&mut tx, "display_type").await.unwrap(),
        Cell::Str("range".into())
    );
    v.write(
        &mut tx,
        RawVals::new()
            .set_cell("is_custom", Cell::Bool(true))
            .set_cell("html_color", Cell::Str("#123456".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        v.cell(&mut tx, "html_color").await.unwrap(),
        Cell::Str("#123456".into())
    );
}

#[tokio::test]
async fn value_usage_price_flags_and_reassignment() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Value use").await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let a = named(&mut tx, &env, "product.attribute", "Size").await;
    let other = named(&mut tx, &env, "product.attribute", "Other").await;
    let v = value(&mut tx, &env, a.ids()[0], "S").await;
    assert_eq!(
        v.cell(&mut tx, "is_used_on_products").await.unwrap(),
        Cell::Bool(false)
    );
    let t = named(&mut tx, &env, "product.template", "Shirt").await;
    let l = line(&mut tx, &env, t.ids()[0], a.ids()[0], &v).await;
    assert_eq!(
        v.cell(&mut tx, "is_used_on_products").await.unwrap(),
        Cell::Bool(true)
    );
    assert_eq!(
        v.cell(&mut tx, "default_extra_price_changed")
            .await
            .unwrap(),
        Cell::Bool(false)
    );
    v.write(
        &mut tx,
        RawVals::new().set_cell("attribute_id", Cell::Id(a.ids()[0])),
    )
    .await
    .unwrap();
    let e = tx
        .savepoint(async |tx| {
            v.write(
                tx,
                RawVals::new().set_cell("attribute_id", Cell::Id(other.ids()[0])),
            )
            .await
        })
        .await
        .unwrap_err();
    user_error(
        e,
        "Cannot change attribute for value Size: S while active products use it: Shirt.",
    );
    let e = tx
        .savepoint(async |tx| v.unlink(tx).await)
        .await
        .unwrap_err();
    user_error(
        e,
        "Cannot delete value Size: S while active products use it: Shirt.",
    );
    v.write(
        &mut tx,
        RawVals::new().set_cell("default_extra_price", Cell::Decimal(Decimal::ONE)),
    )
    .await
    .unwrap();
    assert_eq!(
        v.cell(&mut tx, "default_extra_price_changed")
            .await
            .unwrap(),
        Cell::Bool(true)
    );
    let ptavs = l
        .related(&mut tx, "product_template_value_ids")
        .await
        .unwrap();
    assert_eq!(
        ptavs.cell(&mut tx, "price_extra").await.unwrap(),
        Cell::Decimal(Decimal::ZERO)
    );
    ptavs
        .write(
            &mut tx,
            RawVals::new().set_cell("price_extra", Cell::Decimal(Decimal::ONE)),
        )
        .await
        .unwrap();
    assert_eq!(
        v.cell(&mut tx, "default_extra_price_changed")
            .await
            .unwrap(),
        Cell::Bool(false)
    );
    let unused = value(&mut tx, &env, other.ids()[0], "Unused").await;
    unused.unlink(&mut tx).await.unwrap();
    assert!(unused.exists(&mut tx).await.unwrap().is_empty());
}

#[tokio::test]
async fn custom_text_names_trim_and_restrict_template_value_deletion() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Custom text").await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let a = named(&mut tx, &env, "product.attribute", "Engraving").await;
    let v = value(&mut tx, &env, a.ids()[0], "Text").await;
    let t = named(&mut tx, &env, "product.template", "Ring").await;
    let l = line(&mut tx, &env, t.ids()[0], a.ids()[0], &v).await;
    let ptav = l
        .related(&mut tx, "product_template_value_ids")
        .await
        .unwrap();
    let custom = env
        .records("product.attribute.custom.value")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell(
                        "custom_product_template_attribute_value_id",
                        Cell::Id(ptav.ids()[0]),
                    )
                    .set_cell("custom_value", Cell::Str("  Héllo \n".into())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        custom.cell(&mut tx, "name").await.unwrap(),
        Cell::Str("Engraving: Text: Héllo".into())
    );
    for text in ["", " \t\n"] {
        custom
            .write(
                &mut tx,
                RawVals::new().set_cell("custom_value", Cell::Str(text.into())),
            )
            .await
            .unwrap();
        assert_eq!(
            custom.cell(&mut tx, "name").await.unwrap(),
            Cell::Str("Engraving: Text: ".into())
        );
    }
    let e = tx
        .savepoint(async |tx| ptav.unlink(tx).await)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.sqlstate(), Some("23001"));
    assert_eq!(
        e.message(),
        "The database refused the change: Other records still point to the record you want to delete.\n\nReferring model: 'Custom Attribute Value' (product.attribute.custom.value)\nConstraint: 'Custom Product Template Attribute Value' (custom_product_template_attribute_value_id)\nArchive the record instead of deleting it."
    );
}

#[tokio::test]
async fn deleting_a_value_with_archived_variants_archives_its_identity() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Archive retention").await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let a = named(&mut tx, &env, "product.attribute", "Size").await;
    let v = value(&mut tx, &env, a.ids()[0], "S").await;
    let t = named(&mut tx, &env, "product.template", "Shirt").await;
    line(&mut tx, &env, t.ids()[0], a.ids()[0], &v).await;
    t.action_archive(&mut tx).await.unwrap();
    v.unlink(&mut tx).await.unwrap();
    assert_eq!(v.exists(&mut tx).await.unwrap().ids(), v.ids());
    assert_eq!(v.cell(&mut tx, "active").await.unwrap(), Cell::Bool(false));
    assert!(
        env.records("product.attribute.value")
            .unwrap()
            .search(
                &mut tx,
                &Domain::condition("id", Operator::Eq, v.ids()[0]).unwrap()
            )
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn draft_price_flag_compares_with_origin_without_writing_prices() {
    use erp_addon_product::ProductAttributeValue;
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().clone();
    let a = named(&mut tx, &env, "product.attribute", "Size").await;
    let v = value(&mut tx, &env, a.ids()[0], "S").await;
    let original = RecordSet::<ProductAttributeValue>::try_from(v).unwrap();
    let draft = env
        .model::<ProductAttributeValue>()
        .unwrap()
        .new_with(
            &mut tx,
            Vals::<ProductAttributeValue>::new()
                .set(ProductAttributeValue::DEFAULT_EXTRA_PRICE, Decimal::ONE),
            Some(&original),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        draft
            .cell(&mut tx, "default_extra_price_changed")
            .await
            .unwrap(),
        Cell::Bool(true)
    );
    assert_eq!(
        original.cell(&mut tx, "default_extra_price").await.unwrap(),
        Cell::Decimal(Decimal::ZERO)
    );
}

#[tokio::test]
async fn changed_price_flag_excludes_foreign_company_extras() {
    let h = support::base_harness().await;
    let a_company = support::seed_company(&h, "Company A").await;
    let b_company = support::seed_company(&h, "Company B").await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_company(a_company).unwrap();
    let other = tx.env().with_company(b_company).unwrap();
    let a = named(&mut tx, &env, "product.attribute", "Size").await;
    let v = value(&mut tx, &env, a.ids()[0], "S").await;
    let t = env
        .records("product.template")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Foreign".into()))
                    .set_cell("company_id", Cell::Id(b_company)),
            ],
        )
        .await
        .unwrap();
    let l = line(&mut tx, &other, t.ids()[0], a.ids()[0], &v).await;
    let ptavs = l
        .related(&mut tx, "product_template_value_ids")
        .await
        .unwrap();
    ptavs
        .write(
            &mut tx,
            RawVals::new().set_cell("price_extra", Cell::Decimal(Decimal::ONE)),
        )
        .await
        .unwrap();
    assert_eq!(
        v.cell(&mut tx, "default_extra_price_changed")
            .await
            .unwrap(),
        Cell::Bool(false)
    );
    let bv = other
        .records("product.attribute.value")
        .unwrap()
        .browse(v.ids())
        .unwrap();
    assert_eq!(
        bv.cell(&mut tx, "default_extra_price_changed")
            .await
            .unwrap(),
        Cell::Bool(true)
    );
    assert_eq!(
        v.cell(&mut tx, "default_extra_price_changed")
            .await
            .unwrap(),
        Cell::Bool(false)
    );
}

#[tokio::test]
async fn multi_display_onchange_sets_unused_attribute_mode() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("product.attribute").unwrap();
    let response = records
        .onchange(
            &mut tx,
            erp_sdk::ValueMap::from([
                ("name".to_owned(), Value::str("Choices")),
                ("display_type".to_owned(), Value::str("multi")),
                ("create_variant".to_owned(), Value::str("always")),
            ]),
            vec!["display_type".to_owned()],
            erp_sdk::web::Spec::new()
                .field("display_type")
                .field("create_variant"),
        )
        .await
        .unwrap();
    let result = response.dict().unwrap();
    let changed = result.get("value").unwrap();
    let values = changed.dict().unwrap();
    assert_eq!(
        values.get("create_variant").unwrap(),
        Value::str("no_variant")
    );
    assert_eq!(
        records
            .search_count(&mut tx, &Domain::constant(true), None)
            .await
            .unwrap(),
        0
    );
}

/// Context flags use the product label boundary's strict boolean contract.
#[tokio::test]
async fn attribute_value_label_rejects_non_boolean_context_after_cache_warm() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().clone();
    let a = named(&mut tx, &env, "product.attribute", "Size").await;
    let v = value(&mut tx, &env, a.ids()[0], "S").await;
    assert_eq!(
        v.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("Size: S".into())
    );
    for bad in [
        Value::str("false"),
        Value::Int(1),
        Value::list([Value::Bool(true)]),
    ] {
        let invalid = v.with_context([("show_attribute", bad)]);
        let error = tx
            .savepoint(async |tx| invalid.cell(tx, "display_name").await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Product label context show_attribute must be a boolean"
        );
    }
    assert_eq!(
        v.with_context([("show_attribute", Value::Bool(false))])
            .cell(&mut tx, "display_name")
            .await
            .unwrap(),
        Cell::Str("S".into())
    );
}

/// templates of the attribute's lines)
#[tokio::test]
async fn attribute_read_without_a_field_list_lists_its_templates() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().clone();
    let a = named(&mut tx, &env, "product.attribute", "Read size").await;
    let v = value(&mut tx, &env, a.ids()[0], "S").await;
    let t = named(&mut tx, &env, "product.template", "Read shirt").await;
    line(&mut tx, &env, t.ids()[0], a.ids()[0], &v).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let attribute = tx
        .env()
        .records("product.attribute")
        .unwrap()
        .browse(a.ids().to_vec())
        .unwrap();
    let all = attribute.read(&mut tx, &[]).await.unwrap();
    assert_eq!(all[0]["name"], Value::str("Read size"));
    let named_read = attribute
        .read(&mut tx, &["product_tmpl_ids"])
        .await
        .unwrap();
    assert_eq!(
        all[0]["product_tmpl_ids"],
        named_read[0]["product_tmpl_ids"]
    );
    assert_eq!(
        attribute.cell(&mut tx, "product_tmpl_ids").await.unwrap(),
        Cell::Ids(t.rec_ids().to_vec().into())
    );
    tx.rollback().await.unwrap();
}
