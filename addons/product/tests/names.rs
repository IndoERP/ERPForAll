// SPDX-License-Identifier: MIT OR Apache-2.0
//! Product label and category quick-create regressions.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod support;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

async fn product(tx: &mut Transaction) -> (Records, Records) {
    let templates = tx.env().records("product.template").unwrap();
    let template = templates
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Café Chair".into()))
                    .set_cell("default_code", Cell::Str("SKU".into())),
            ],
        )
        .await
        .unwrap();
    let variants = tx
        .env()
        .records("product.product")
        .unwrap()
        .search(
            tx,
            &Domain::condition("product_tmpl_id", Operator::Eq, template.rec_ids()[0].get())
                .unwrap(),
        )
        .await
        .unwrap();
    (template, variants)
}

#[tokio::test]
async fn template_label_obeys_both_code_context_switches() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let (template, _) = product(&mut tx).await;
    for (display, formatted, expected) in [
        (true, false, "[SKU] Café Chair"),
        (true, true, "Café Chair\t--SKU--"),
        (false, true, "Café Chair"),
        (false, false, "Café Chair"),
    ] {
        let rec = template.with_context([
            ("display_default_code", Value::Bool(display)),
            ("formatted_display_name", Value::Bool(formatted)),
        ]);
        assert_eq!(
            rec.cell(&mut tx, "display_name").await.unwrap(),
            Cell::Str(expected.into())
        );
    }
}

#[tokio::test]
async fn variant_formatted_code_can_be_disabled() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, variant) = product(&mut tx).await;
    assert_eq!(
        variant
            .with_context([("formatted_display_name", Value::Bool(true))])
            .cell(&mut tx, "display_name")
            .await
            .unwrap(),
        Cell::Str("Café Chair\t--SKU--".into())
    );
    assert_eq!(
        variant
            .with_context([
                ("formatted_display_name", Value::Bool(true)),
                ("display_default_code", Value::Bool(false))
            ])
            .cell(&mut tx, "display_name")
            .await
            .unwrap(),
        Cell::Str("Café Chair".into())
    );
}

#[tokio::test]
async fn category_quick_create_writes_name_and_returns_hierarchical_label() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tx.env().records("product.category").unwrap();
    let (parent, label) = cats.name_create(&mut tx, "Office").await.unwrap();
    assert_eq!(label.as_ref(), "Office");
    let (child, label) = cats
        .with_context([("default_parent_id", Value::Int(parent))])
        .name_create(&mut tx, "Chairs")
        .await
        .unwrap();
    assert_eq!(label.as_ref(), "Office / Chairs");
    assert_eq!(
        cats.browse(vec![child])
            .unwrap()
            .cell(&mut tx, "name")
            .await
            .unwrap(),
        Cell::Str("Chairs".into())
    );
}

#[tokio::test]
async fn flat_category_label_does_not_change_complete_name() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let cats = tx.env().records("product.category").unwrap();
    let parent = cats
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Office".into()))],
        )
        .await
        .unwrap();
    let child = cats
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Chairs".into()))
                    .set_cell("parent_id", Cell::Id(parent.rec_ids()[0].get())),
            ],
        )
        .await
        .unwrap()
        .with_context([("hierarchical_naming", Value::Bool(false))]);
    assert_eq!(
        child.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("Chairs".into())
    );
    assert_eq!(
        child.cell(&mut tx, "complete_name").await.unwrap(),
        Cell::Str("Office / Chairs".into())
    );
}

async fn vendor_fixture(
    tx: &mut Transaction,
    variant: &Records,
    company: i64,
    code: &str,
    name: &str,
) -> (i64, Records) {
    let partner = tx
        .env()
        .records("res.partner")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Vendor".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let currency = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse(vec![company])
        .unwrap()
        .cell(tx, "currency_id")
        .await
        .unwrap();
    let template = variant.cell(tx, "product_tmpl_id").await.unwrap();
    let seller = tx
        .env()
        .records("product.supplierinfo")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("partner_id", Cell::Id(partner))
                    .set_cell("product_tmpl_id", template)
                    .set_cell("product_id", Cell::Id(variant.rec_ids()[0].get()))
                    .set_cell("company_id", Cell::Null)
                    .set_cell("currency_id", currency)
                    .set_cell("product_code", Cell::Str(code.into()))
                    .set_cell("product_name", Cell::Str(name.into())),
            ],
        )
        .await
        .unwrap();
    (partner, seller)
}

#[tokio::test]
async fn vendor_code_and_partner_reference_are_contextual_and_invalidated() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Labels").await;
    let mut tx = h.begin().await.unwrap();
    let (_, variant) = product(&mut tx).await;
    let (partner, seller) =
        vendor_fixture(&mut tx, &variant, company, "VSKU", "Vendor Chair").await;
    let selected = variant.with_context([("partner_id", Value::Int(partner))]);
    for (field, expected) in [
        ("display_name", "[VSKU] Vendor Chair"),
        ("code", "VSKU"),
        ("partner_ref", "[VSKU] Vendor Chair"),
    ] {
        assert_eq!(
            selected.cell(&mut tx, field).await.unwrap(),
            Cell::Str(expected.into())
        );
    }
    assert_eq!(
        variant.cell(&mut tx, "code").await.unwrap(),
        Cell::Str("SKU".into())
    );
    seller
        .write(
            &mut tx,
            RawVals::new().set_cell("product_code", Cell::Str("NEW".into())),
        )
        .await
        .unwrap();
    assert_eq!(
        selected.cell(&mut tx, "code").await.unwrap(),
        Cell::Str("NEW".into())
    );
    assert_eq!(
        selected.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("[NEW] Vendor Chair".into())
    );
}

async fn restricted_vendor_labels(deny_model: bool) {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow);
    let policy = if deny_model {
        policy.deny("product.supplierinfo", Operation::Read)
    } else {
        policy.rule("product.supplierinfo", Operation::Read, Domain::FALSE)
    }
    .build()
    .unwrap();
    let h = support::harness_with_policy(policy).await;
    let company = support::seed_company(&h, "Private Labels").await;
    let mut tx = h.begin().await.unwrap();
    let (_, variant) = product(&mut tx).await;
    let (partner, _) = vendor_fixture(
        &mut tx,
        &variant,
        company,
        "SECRET",
        "Hidden vendor description",
    )
    .await;
    // This harness intentionally has no installed group graph; seed only the user identity.
    let user: i64 = tx.query("INSERT INTO res_users (name, login, active) VALUES ('Reader', 'label-reader', true) RETURNING id", &[])
        .await.unwrap()[0].get(0);
    let holder = tx
        .env()
        .records("test_product.label_holder")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("product_id", Cell::Id(variant.rec_ids()[0].get()))],
        )
        .await
        .unwrap();
    let warm = variant.with_context([("partner_id", Value::Int(partner))]);
    assert_eq!(
        warm.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("[SECRET] Hidden vendor description".into())
    );
    let reader = tx
        .env()
        .with_user(user)
        .with_company(company)
        .unwrap()
        .with_context([
            ("partner_id", Value::Int(partner)),
            ("label_reader", Value::str("untrusted")),
        ]);
    let visible = variant.with_env(reader.clone());
    if deny_model {
        assert!(
            !reader
                .records("product.supplierinfo")
                .unwrap()
                .has_access(&mut tx, Operation::Read)
                .await
                .unwrap()
        );
    }
    // Probe the elevated relation before the direct user read can warm its cache.
    let rows = holder
        .with_env(reader.clone())
        .read(&mut tx, &["product_id"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["product_id"],
        Value::list([
            Value::Int(variant.rec_ids()[0].get()),
            Value::str("[SKU] Café Chair")
        ])
    );
    for (field, expected) in [
        ("display_name", "[SKU] Café Chair"),
        ("code", "SKU"),
        ("partner_ref", "[SKU] Café Chair"),
    ] {
        assert_eq!(
            visible.cell(&mut tx, field).await.unwrap(),
            Cell::Str(expected.into())
        );
    }
    // An indirect elevated read cannot poison the unrestricted consumer either.
    assert_eq!(
        warm.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("[SECRET] Hidden vendor description".into())
    );
}

#[tokio::test]
async fn supplier_acl_denial_never_leaks_through_a_label() {
    restricted_vendor_labels(true).await;
}

#[tokio::test]
async fn supplier_record_rule_never_leaks_through_a_label() {
    restricted_vendor_labels(false).await;
}

#[tokio::test]
async fn category_default_order_uses_parent_relation_order_then_name() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("product.category").unwrap();
    let roots = model
        .create(
            &mut tx,
            vec![
                RawVals::new().set_cell("name", Cell::Str("Z".into())),
                RawVals::new().set_cell("name", Cell::Str("A".into())),
            ],
        )
        .await
        .unwrap();
    let children = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Child A".into()))
                    .set_cell("parent_id", Cell::Id(roots.rec_ids()[0].get())),
                RawVals::new()
                    .set_cell("name", Cell::Str("Child Z".into()))
                    .set_cell("parent_id", Cell::Id(roots.rec_ids()[1].get())),
            ],
        )
        .await
        .unwrap();
    // many2one ordering follows the parent model order, not raw parent IDs.
    let found = model.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert_eq!(
        found.ids().as_slice(),
        &[
            roots.rec_ids()[1].get(),
            roots.rec_ids()[0].get(),
            children.rec_ids()[0].get(),
            children.rec_ids()[1].get()
        ]
    );
}

#[tokio::test]
async fn malformed_label_context_is_a_named_validation_error() {
    let h = support::base_harness().await;
    let mut tx = h.begin().await.unwrap();
    let (template, _) = product(&mut tx).await;
    let error = template
        .with_context([("display_default_code", Value::str("yes"))])
        .cell(&mut tx, "display_name")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Product label context display_default_code must be a boolean"
    );
}

#[tokio::test]
async fn vendor_labels_join_distinct_rows_and_never_widen_companies() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Own Labels").await;
    let foreign = support::seed_company(&h, "Foreign Labels").await;
    let mut tx = h.begin().await.unwrap();
    let (_, variant) = product(&mut tx).await;
    let (partner, seller) =
        vendor_fixture(&mut tx, &variant, company, "FIRST", "First Chair").await;
    let currency = seller.cell(&mut tx, "currency_id").await.unwrap();
    let tmpl = seller.cell(&mut tx, "product_tmpl_id").await.unwrap();
    let model = tx.env().records("product.supplierinfo").unwrap();
    let mut extra_ids = Vec::new();
    for (code, name) in [("SECOND", "Second Chair"), ("SECOND", "Second Chair")] {
        let row = model
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("partner_id", Cell::Id(partner))
                        .set_cell("product_tmpl_id", tmpl.clone())
                        .set_cell("product_id", Cell::Id(variant.rec_ids()[0].get()))
                        .set_cell("company_id", Cell::Null)
                        .set_cell("currency_id", currency.clone())
                        .set_cell("product_code", Cell::Str(code.into()))
                        .set_cell("product_name", Cell::Str(name.into())),
                ],
            )
            .await
            .unwrap();
        extra_ids.push(row.rec_ids()[0].get());
    }
    let visible = variant
        .with_env(tx.env().with_company(company).unwrap())
        .with_context([("partner_id", Value::Int(partner))]);
    assert_eq!(
        visible.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("[FIRST] First Chair, [SECOND] Second Chair".into())
    );
    model
        .browse(extra_ids.clone())
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("company_id", Cell::Id(foreign)),
        )
        .await
        .unwrap();
    assert_eq!(
        visible.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("[FIRST] First Chair".into())
    );
    // A supplied seller ID does not grant company visibility.
    let explicit = visible.with_context([("seller_id", Value::Int(extra_ids[0]))]);
    assert_eq!(
        explicit.cell(&mut tx, "display_name").await.unwrap(),
        Cell::Str("[SKU] Café Chair".into())
    );
    assert_eq!(
        visible.cell(&mut tx, "code").await.unwrap(),
        Cell::Str("FIRST".into())
    );
}

/// Semantic batching contract: increasing the result page adds no per-product query train.
#[tokio::test]
async fn vendor_display_batch_query_count_does_not_grow_per_variant() {
    let h = support::base_harness().await;
    let company = support::seed_company(&h, "Batch Labels").await;
    let mut seed = h.begin().await.unwrap();
    let env = seed.env().with_company(company).unwrap();
    let partner = env
        .records("res.partner")
        .unwrap()
        .create(
            &mut seed,
            vec![RawVals::new().set_cell("name", Cell::Str("Batch Vendor".into()))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let currency = env
        .records("res.company")
        .unwrap()
        .browse([company])
        .unwrap()
        .cell(&mut seed, "currency_id")
        .await
        .unwrap();
    let templates = env
        .records("product.template")
        .unwrap()
        .create(
            &mut seed,
            (0..6)
                .map(|i| {
                    RawVals::new().set_cell("name", Cell::Str(format!("Batch product {i}").into()))
                })
                .collect(),
        )
        .await
        .unwrap();
    let template_ids = templates.ids().to_vec();
    let variants = env
        .records("product.product")
        .unwrap()
        .search(
            &mut seed,
            &Domain::condition("product_tmpl_id", Operator::In, template_ids).unwrap(),
        )
        .await
        .unwrap();
    let ids = variants.ids().to_vec();
    env.records("product.supplierinfo")
        .unwrap()
        .create(
            &mut seed,
            templates
                .rec_ids()
                .iter()
                .map(|id| {
                    RawVals::new()
                        .set_cell("product_tmpl_id", Cell::Id(id.get()))
                        .set_cell("partner_id", Cell::Id(partner))
                        .set_cell("currency_id", currency.clone())
                        .set_cell("product_name", Cell::Str("Vendor batch name".into()))
                        .set_cell("product_code", Cell::Str("VBATCH".into()))
                })
                .collect(),
        )
        .await
        .unwrap();
    seed.commit().await.unwrap();
    let mut counts = Vec::new();
    for page in [vec![ids[0]], ids] {
        let mut tx = h.begin().await.unwrap();
        let model = tx
            .env()
            .with_company(company)
            .unwrap()
            .with_context([("partner_id", Value::Int(partner))])
            .records("product.product")
            .unwrap()
            .browse(page.clone())
            .unwrap();
        let before = tx.query_count();
        let rows = model.read(&mut tx, &["display_name"]).await.unwrap();
        counts.push(tx.query_count() - before);
        assert_eq!(rows.len(), page.len());
        for row in rows {
            assert_eq!(
                row["display_name"],
                Value::str("[VBATCH] Vendor batch name")
            );
        }
    }
    assert_eq!(
        counts[0], counts[1],
        "supplier batch query counts must not depend on page cardinality"
    );
}

#[tokio::test]
async fn explicit_seller_respects_a_narrower_company_context() {
    let h = support::base_harness().await;
    let own = support::seed_company(&h, "Company A").await;
    let other = support::seed_company(&h, "Company B").await;
    let mut tx = h.begin().await.unwrap();
    let (_, variant) = product(&mut tx).await;
    let (partner, seller) =
        vendor_fixture(&mut tx, &variant, own, "OTHER", "Other company label").await;
    seller
        .write(
            &mut tx,
            RawVals::new().set_cell("company_id", Cell::Id(other)),
        )
        .await
        .unwrap();
    let env = tx.env().with_context([
        (
            "allowed_company_ids",
            Value::list([Value::Int(own), Value::Int(other)]),
        ),
        ("company_id", Value::Int(own)),
        ("partner_id", Value::Int(partner)),
        ("seller_id", Value::Int(seller.rec_ids()[0].get())),
    ]);
    let label = variant
        .with_env(env)
        .cell(&mut tx, "display_name")
        .await
        .unwrap();
    assert_eq!(label, Cell::Str("[SKU] Café Chair".into()));
}

#[tokio::test]
async fn category_quick_create_retains_create_permission_checks() {
    let h = support::harness_with_policy(
        erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
            .deny("product.category", Operation::Create)
            .build()
            .unwrap(),
    )
    .await;
    let mut tx = h.begin().await.unwrap();
    let user: i64 = tx.query("INSERT INTO res_users (name, login, active) VALUES ('Category Reader', 'category-reader', true) RETURNING id", &[]).await.unwrap()[0].get(0);
    let env = tx.env().with_user(user);
    let model = env.records("product.category").unwrap();
    let error = model
        .name_create(&mut tx, "Forbidden Category")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you create 'Product Category' (product.category) records."
    );
    assert_eq!(
        tx.env()
            .records("product.category")
            .unwrap()
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        0
    );
}
