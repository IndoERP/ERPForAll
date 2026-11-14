//! Product Barcode tests.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn harness() -> Harness {
    let h = support::base_harness().await;

    let mut tx = h.begin().await.unwrap();

    // Ensure currency exists
    let curr_model = tx.env().records("res.currency").unwrap();
    let d = Domain::constant(true);
    let currs = curr_model.search(&mut tx, &d).await.unwrap();
    let curr_id = if currs.is_empty() {
        curr_model
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("USD".into()))
                        .set_cell("symbol", Cell::Str("$".into()))
                        .set_cell("rounding", Cell::Decimal(Decimal::new(1, 2))),
                ],
            )
            .await
            .unwrap()
            .rec_ids()[0]
            .get()
    } else {
        currs.rec_ids()[0].get()
    };

    // Ensure company records exist
    let comp_model = tx.env().records("res.company").unwrap();
    let _comp_a = comp_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Company A".into()))
                    .set_cell("currency_id", Cell::Id(curr_id)),
            ],
        )
        .await
        .unwrap();
    let _comp_b = comp_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Company B".into()))
                    .set_cell("currency_id", Cell::Id(curr_id)),
            ],
        )
        .await
        .unwrap();

    tx.commit().await.unwrap();
    h
}

#[tokio::test]
async fn test_blank_barcodes_allowed() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("BlankBarcodes".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // Create 2 products with empty or null barcodes
    let p1 = prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("barcode", Cell::Null),
            ],
        )
        .await;
    assert!(p1.is_ok(), "Null barcode should be allowed");

    let p2 = prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("barcode", Cell::Str("".into())),
            ],
        )
        .await;
    assert!(
        p2.is_ok(),
        "Empty string barcode should be allowed on multiple records"
    );
}

#[tokio::test]
async fn test_duplicate_barcode_rejected_same_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let comp_model = tx.env().records("res.company").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let comp = comp_model
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap();
    let comp_id = comp.rec_ids()[0].get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("T1".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("company_id", Cell::Id(comp_id))
                    .set_cell("barcode", Cell::Str("11223344".into())),
            ],
        )
        .await
        .unwrap();

    // Attempt to create second product in same company with same barcode
    let err = prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("company_id", Cell::Id(comp_id))
                    .set_cell("barcode", Cell::Str("11223344".into())),
            ],
        )
        .await;

    assert!(
        err.is_err(),
        "Duplicate barcode in same company must be rejected"
    );
}

#[tokio::test]
async fn test_identical_barcode_different_companies() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let comp_model = tx.env().records("res.company").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let comps = comp_model
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap();
    assert!(comps.len() >= 2);
    let comp_a = comps.rec_ids()[0].get();
    let comp_b = comps.rec_ids()[1].get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("SharedBC".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    let p1 = prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("company_id", Cell::Id(comp_a))
                    .set_cell("barcode", Cell::Str("BAR-SHARED".into())),
            ],
        )
        .await;
    assert!(p1.is_ok());

    // company_id is delegated to the template: different companies require different families.
    let second_template = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Other company family".into()))],
        )
        .await
        .unwrap();
    let second_template_id = second_template.rec_ids()[0].get();
    let p2 = prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(second_template_id))
                    .set_cell("company_id", Cell::Id(comp_b))
                    .set_cell("barcode", Cell::Str("BAR-SHARED".into())),
            ],
        )
        .await;
    assert!(
        p2.is_ok(),
        "Identical barcode across different companies must be allowed"
    );
}

#[tokio::test]
async fn test_shared_product_barcode_conflicts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let comp_model = tx.env().records("res.company").unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let comp = comp_model
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap();
    let comp_id = comp.rec_ids()[0].get();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("GlobalProd".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // Shared product with company_id = False / Null
    prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("barcode", Cell::Str("GLOBAL-01".into())),
            ],
        )
        .await
        .unwrap();

    // Company-specific product assigning same barcode must fail
    let err = prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("company_id", Cell::Id(comp_id))
                    .set_cell("barcode", Cell::Str("GLOBAL-01".into())),
            ],
        )
        .await;

    assert!(
        err.is_err(),
        "Company-specific barcode cannot collide with global shared barcode"
    );
}

#[tokio::test]
async fn test_template_single_variant_mirror_barcode() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("SingleMirror".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // Writing barcode on single-variant template mirrors to variant
    let res = tmpl
        .write(
            &mut tx,
            RawVals::new().set_cell("barcode", Cell::Str("MIRROR-01".into())),
        )
        .await;
    assert!(
        res.is_ok(),
        "Single variant template should mirror barcode without uniqueness error"
    );

    let d = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variant = prod_model.search(&mut tx, &d).await.unwrap();
    assert_eq!(variant.len(), 1);
    let bc = variant.cell(&mut tx, "barcode").await.unwrap();
    assert_eq!(
        bc,
        Cell::Str("MIRROR-01".into()),
        "Variant should have mirrored barcode"
    );
}

#[tokio::test]
async fn test_template_multi_variants_forbids_template_barcode() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("MultiTmpl".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // Create second variant for template so it has 2 variants
    prod_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("product_tmpl_id", Cell::Id(tmpl_id))],
        )
        .await
        .unwrap();

    // Set template barcode
    tmpl.write(
        &mut tx,
        RawVals::new().set_cell("barcode", Cell::Str("TMPL-PARENT".into())),
    )
    .await
    .unwrap();

    // Child variant attempting to use template barcode must be rejected
    let d = Domain::condition("product_tmpl_id", Operator::Eq, tmpl_id).unwrap();
    let variants = prod_model.search(&mut tx, &d).await.unwrap();
    let v1 = prod_model
        .browse(vec![variants.rec_ids()[0].get()])
        .unwrap();

    let err = v1
        .write(
            &mut tx,
            RawVals::new().set_cell("barcode", Cell::Str("TMPL-PARENT".into())),
        )
        .await;
    assert!(
        err.is_err(),
        "Variant on multi-variant template cannot share template barcode"
    );
}

#[tokio::test]
async fn test_batch_edit_barcode_collision() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("BatchTmpl".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // Attempt batch create with duplicate barcode in the batch
    let err = prod_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("barcode", Cell::Str("BATCH-DUP".into())),
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("barcode", Cell::Str("BATCH-DUP".into())),
            ],
        )
        .await;

    assert!(
        err.is_err(),
        "Duplicate barcode in single batch creation must be rejected"
    );
}

#[tokio::test]
async fn test_skip_barcode_uniqueness_check() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tmpl_model = tx.env().records("product.template").unwrap();
    let prod_model = tx.env().records("product.product").unwrap();

    let tmpl = tmpl_model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("SkipTmpl".into()))],
        )
        .await
        .unwrap();
    let tmpl_id = tmpl.rec_ids()[0].get();

    // Set context flag skip_barcode_uniqueness_check
    let prod_model_ctx =
        prod_model.with_context([("skip_barcode_uniqueness_check", Value::Bool(true))]);

    let res = prod_model_ctx
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("barcode", Cell::Str("SKIP-DUP".into())),
                RawVals::new()
                    .set_cell("product_tmpl_id", Cell::Id(tmpl_id))
                    .set_cell("barcode", Cell::Str("SKIP-DUP".into())),
            ],
        )
        .await;

    let error = res.expect_err("barcode uniqueness cannot be skipped");
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert!(
        error.message().contains("skip_barcode_uniqueness_check"),
        "{error}"
    );
}

/// the actual second result is asserted; this is not a two-node variant-generation test)
#[tokio::test]
async fn test_concurrent_barcode_assignments_are_serialized() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    let tm = setup.env().records("product.template").unwrap();
    let templates = tm
        .create(
            &mut setup,
            vec![
                RawVals::new().set_cell("name", Cell::Str("T1".into())),
                RawVals::new().set_cell("name", Cell::Str("T2".into())),
            ],
        )
        .await
        .unwrap();
    let ids: Vec<i64> = templates.rec_ids().iter().map(|id| id.get()).collect();
    setup.commit().await.unwrap();
    let (mut one, mut two) = h.begin_pair().await.unwrap();
    let pid = erp_testkit::backend_pid(&mut two).await.unwrap();
    let first = one
        .env()
        .records("product.template")
        .unwrap()
        .browse(vec![ids[0]])
        .unwrap();
    let second = two
        .env()
        .records("product.template")
        .unwrap()
        .browse(vec![ids[1]])
        .unwrap();
    first
        .write(
            &mut one,
            RawVals::new().set_cell("barcode", Cell::Str("CONCURRENT-BAR-01".into())),
        )
        .await
        .unwrap();
    let waiting = tokio::spawn(async move {
        let result = second
            .write(
                &mut two,
                RawVals::new().set_cell("barcode", Cell::Str("CONCURRENT-BAR-01".into())),
            )
            .await;
        (result, two)
    });
    erp_testkit::wait_until_blocked(&h, pid, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    one.commit().await.unwrap();
    let (result, mut two) = waiting.await.unwrap();
    let error = result.expect_err("stale snapshot must fail");
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Concurrency, "{error}");
    assert_eq!(error.sqlstate(), Some("40001"));
    two.rollback().await.unwrap();
    let mut fresh = h.begin().await.unwrap();
    let second = fresh
        .env()
        .records("product.template")
        .unwrap()
        .browse(vec![ids[1]])
        .unwrap();
    let error = second
        .write(
            &mut fresh,
            RawVals::new().set_cell("barcode", Cell::Str("CONCURRENT-BAR-01".into())),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert!(error.message().contains("CONCURRENT-BAR-01"), "{error}");
}
