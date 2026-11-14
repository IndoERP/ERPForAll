// SPDX-License-Identifier: MIT OR Apache-2.0
//! The quant engine: signed changes of balances through
//! `erp_addon_stock::apply_quant_changes`, availability reads, the guards of the quant model and the
//! location quant guards. Every compared value is read back through the engine or the database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use erp_addon_stock::{
    QuantBalance, QuantChange, QuantFilter, QuantKey, apply_quant_changes, available_quantity,
    gather_quants,
};
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::{Harness, HarnessError};

erp_sdk::addon!(name = "test_stock_quant_engine", depends = ["stock"]);

/// A test stand-in for `ir.model.data` with the columns the product addon and the company create path
/// read (the real model lives in `erp-data`, out of reach of an addon crate; the product tests use the
/// same stand-in).
#[model(
    name = "ir.model.data",
    description = "Model Data",
    addon = "test_stock_quant_engine"
)]
pub struct ModelData {
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    #[field(char, required = true)]
    pub model: Option<Arc<str>>,
    #[field(char, required = true)]
    pub module: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "model")]
    pub res_id: i64,
    #[field(boolean)]
    pub noupdate: bool,
}

/// Fixture of spec §3: storable products P and Q, a consumable, internal location L with child C,
/// location X outside L, a view location, a vendor location, lots A and B of P, lot Z of Q, package K,
/// owner O, and a second company.
struct Fixture {
    h: Harness,
    company: i64,
    other_company: i64,
    p: i64,
    q: i64,
    consumable: i64,
    l: i64,
    c: i64,
    x: i64,
    view: i64,
    vendor: i64,
    lot_a: i64,
    lot_b: i64,
    lot_z: i64,
    k: i64,
    o: i64,
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

fn dec(value: &str) -> Decimal {
    value.parse().unwrap()
}

async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    tx.env()
        .records(model)
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap_or_else(|e| panic!("create {model}: {e:?}"))
        .rec_ids()[0]
        .get()
}

async fn product(tx: &mut Transaction, company: i64, name: &str, storable: bool) -> i64 {
    let env = tx.env().with_company(company).unwrap();
    let template = env
        .records("product.template")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("is_storable", Cell::Bool(storable)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    tx.query(
        "SELECT id FROM product_product WHERE product_tmpl_id = $1",
        &[&template],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn location(tx: &mut Transaction, name: &str, parent: Option<i64>, usage: &str) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("usage", text(usage));
    if let Some(parent) = parent {
        vals = vals.set_cell("location_id", Cell::Id(parent));
    }
    create(tx, "stock.location", vals).await
}

async fn lot(tx: &mut Transaction, product: i64, company: i64, name: &str) -> i64 {
    create(
        tx,
        "stock.lot",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("product_id", Cell::Id(product))
            .set_cell("company_id", Cell::Id(company)),
    )
    .await
}

async fn fixture() -> Fixture {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_stock::ADDON,
        &ADDON,
    ])
    .require_sql_objects([&erp_addon_stock::schema_guards::MANIFEST])
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true))
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch(include_str!("../../uom/src/guard_sql.sql"))
        .await
        .unwrap();
    tx.execute("INSERT INTO uom_reference_fence(singleton) VALUES (1)", &[])
        .await
        .unwrap();
    let unit = create(
        &mut tx,
        "uom.uom",
        RawVals::new()
            .set_cell("name", text("Units"))
            .set_cell("relative_factor", Cell::Decimal(Decimal::ONE)),
    )
    .await;
    create(
        &mut tx,
        "ir.model.data",
        RawVals::new()
            .set_cell("module", text("uom"))
            .set_cell("name", text("product_uom_unit"))
            .set_cell("model", text("uom.uom"))
            .set_cell("res_id", Cell::Id(unit)),
    )
    .await;
    let existing = tx
        .query("SELECT id FROM res_currency ORDER BY id LIMIT 1", &[])
        .await
        .unwrap();
    let currency = match existing.first() {
        Some(row) => row.get::<_, i64>(0),
        None => {
            create(
                &mut tx,
                "res.currency",
                RawVals::new()
                    .set_cell("name", text("USD"))
                    .set_cell("symbol", text("$"))
                    .set_cell("rounding", Cell::Decimal(Decimal::new(1, 2))),
            )
            .await
        }
    };
    let mut companies = Vec::new();
    for name in ["Main Company", "Other Company"] {
        companies.push(
            create(
                &mut tx,
                "res.company",
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("currency_id", Cell::Id(currency)),
            )
            .await,
        );
    }
    let company = companies[0];
    let p = product(&mut tx, company, "P", true).await;
    let q = product(&mut tx, company, "Q", true).await;
    let consumable = product(&mut tx, company, "Consumable", false).await;
    let l = location(&mut tx, "L", None, "internal").await;
    let c = location(&mut tx, "C", Some(l), "internal").await;
    let x = location(&mut tx, "X", None, "internal").await;
    let view = location(&mut tx, "viewloc1", None, "view").await;
    let vendor = location(&mut tx, "Vendors", None, "supplier").await;
    let lot_a = lot(&mut tx, p, company, "A").await;
    let lot_b = lot(&mut tx, p, company, "B").await;
    let lot_z = lot(&mut tx, q, company, "Z").await;
    let k = create(
        &mut tx,
        "stock.package",
        RawVals::new().set_cell("name", text("K")),
    )
    .await;
    let o = create(
        &mut tx,
        "res.partner",
        RawVals::new().set_cell("name", text("O")),
    )
    .await;
    tx.commit().await.unwrap();
    Fixture {
        h,
        company,
        other_company: companies[1],
        p,
        q,
        consumable,
        l,
        c,
        x,
        view,
        vendor,
        lot_a,
        lot_b,
        lot_z,
        k,
        o,
    }
}

impl Fixture {
    /// The key of `product` at `location` of the main company, without lot, package or owner.
    fn key(&self, product: i64, location: i64) -> QuantKey {
        QuantKey::new(self.company, product, location)
    }

    /// A filter of `product` at `location` of the main company.
    fn filter(&self, product: i64, location: i64, strict: bool) -> QuantFilter {
        QuantFilter::new(self.company, product, location).strict(strict)
    }
}

fn change(key: QuantKey, quantity: &str, reserved: &str) -> QuantChange {
    QuantChange::new(key, dec(quantity), dec(reserved))
}

async fn apply(tx: &mut Transaction, changes: &[QuantChange]) -> Vec<QuantBalance> {
    apply_quant_changes(tx, changes)
        .await
        .unwrap_or_else(|e| panic!("apply {changes:?}: {e:?}"))
}

async fn refused(tx: &mut Transaction, changes: &[QuantChange]) -> Error {
    apply_quant_changes(tx, changes).await.unwrap_err()
}

async fn available(tx: &mut Transaction, filter: QuantFilter) -> (Decimal, Decimal) {
    (
        available_quantity(tx, &filter, false).await.unwrap(),
        available_quantity(tx, &filter, true).await.unwrap(),
    )
}

/// `(quantity, reserved)` of the stored key, `None` when no row holds it.
async fn stored(tx: &mut Transaction, key: QuantKey) -> Option<(String, String)> {
    tx.query(
        "SELECT trim_scale(quantity)::text, trim_scale(reserved_quantity)::text FROM stock_quant \
          WHERE company_id = $1 AND product_id = $2 AND location_id = $3 \
            AND lot_id IS NOT DISTINCT FROM $4 AND package_id IS NOT DISTINCT FROM $5 \
            AND owner_id IS NOT DISTINCT FROM $6",
        &[
            &key.company_id,
            &key.product_id,
            &key.location_id,
            &key.lot_id,
            &key.package_id,
            &key.owner_id,
        ],
    )
    .await
    .unwrap()
    .first()
    .map(|r| (r.get(0), r.get(1)))
}

async fn quant_rows(tx: &mut Transaction, product: i64) -> i64 {
    tx.query(
        "SELECT count(*) FROM stock_quant WHERE product_id = $1",
        &[&product],
    )
    .await
    .unwrap()[0]
        .get(0)
}

fn d1() -> DateTime {
    DateTime::from_parts(Date::from_ymd(2026, 5, 20).unwrap(), 10, 0, 0, 0).unwrap()
}

fn d2() -> DateTime {
    DateTime::from_parts(Date::from_ymd(2026, 3, 1).unwrap(), 10, 0, 0, 0).unwrap()
}

#[tokio::test]
async fn signed_changes_return_the_new_balance() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let key = f.key(f.p, f.l);
    let first = apply(&mut tx, &[change(key, "1", "0")]).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].available(), dec("1"));
    assert!(first[0].id.is_some());
    assert_eq!(stored(&mut tx, key).await, Some(("1".into(), "0".into())));
    // an existing balance: the total is returned, with a date
    let second = apply(&mut tx, &[change(key, "2", "0")]).await;
    assert_eq!(second[0].available(), dec("3"));
    assert_eq!(second[0].id, first[0].id);
    assert_eq!(second[0].in_date, first[0].in_date);
    // a negative balance can be created; the default read clamps it
    let other = f.key(f.q, f.l);
    let negative = apply(&mut tx, &[change(other, "-1", "0")]).await;
    assert_eq!(negative[0].quantity, dec("-1"));
    assert_eq!(
        stored(&mut tx, other).await,
        Some(("-1".into(), "0".into()))
    );
    assert_eq!(
        available(&mut tx, f.filter(f.q, f.l, false)).await,
        (dec("0"), dec("-1"))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// names the field and digits; the ORM write path of other quantity fields is the ORM's)
#[tokio::test]
async fn refused_changes_name_their_reason_and_store_nothing() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let digits: u32 = tx
        .env()
        .records("decimal.precision")
        .unwrap()
        .call::<(String,), u32>(&mut tx, "_precision_get", ("Product Unit".to_owned(),))
        .await
        .unwrap();
    assert!(digits < 7, "the refused delta below has 7 decimal digits");
    let cases = [
        (
            change(f.key(f.p, f.l), "0", "0"),
            ErrorKind::Validation,
            "Set a quantity, a reserved quantity or both.".to_owned(),
        ),
        (
            change(f.key(f.consumable, f.l), "1", "0"),
            ErrorKind::Validation,
            "Stock is kept only for storable products, not for consumables or services.".to_owned(),
        ),
        (
            change(f.key(f.p, f.view), "1", "0"),
            ErrorKind::Validation,
            "Products cannot be taken from or delivered to viewloc1: it is a view location."
                .to_owned(),
        ),
        (
            change(f.key(f.p, f.l).lot_id(Some(f.lot_z)), "1", "0"),
            ErrorKind::Validation,
            "Lot or serial number Z belongs to a different product.".to_owned(),
        ),
        (
            change(f.key(f.p, f.l), "0.0000001", "0"),
            ErrorKind::Validation,
            format!(
                "The quantity change 0.0000001 of a stock balance has more decimal digits than the \
                 Product Unit precision allows ({digits})."
            ),
        ),
    ];
    for (change, kind, message) in cases {
        let error = refused(&mut tx, &[change]).await;
        assert_eq!(error.kind(), kind, "{change:?}: {error:?}");
        assert_eq!(error.message(), message, "{change:?}");
    }
    for product in [f.p, f.q, f.consumable] {
        assert_eq!(quant_rows(&mut tx, product).await, 0);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Company consistency of the key parts: a balance of "Main Company" cannot name a product, location,
/// lot or owner of "Other Company"; a shared location and partner are accepted. No spec case yet.
#[tokio::test]
async fn key_parts_of_another_company_are_refused() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let foreign_product = product(&mut tx, f.other_company, "Foreign", true).await;
    tx.execute(
        "UPDATE product_template SET company_id = $1 \
          WHERE id = (SELECT product_tmpl_id FROM product_product WHERE id = $2)",
        &[&f.other_company, &foreign_product],
    )
    .await
    .unwrap();
    let foreign_location = create(
        &mut tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Foreign Shelf"))
            .set_cell("company_id", Cell::Id(f.other_company)),
    )
    .await;
    let foreign_lot = lot(&mut tx, f.p, f.other_company, "FL").await;
    let foreign_owner = create(
        &mut tx,
        "res.partner",
        RawVals::new()
            .set_cell("name", text("Foreign Owner"))
            .set_cell("company_id", Cell::Id(f.other_company)),
    )
    .await;
    let base = f.key(f.p, f.l);
    for (key, what, id) in [
        (f.key(foreign_product, f.l), "product", foreign_product),
        (f.key(f.p, foreign_location), "location", foreign_location),
        (base.lot_id(Some(foreign_lot)), "lot", foreign_lot),
        (base.owner_id(Some(foreign_owner)), "owner", foreign_owner),
    ] {
        let error = refused(&mut tx, &[change(key, "1", "0")]).await;
        assert_eq!(error.kind(), ErrorKind::Validation, "{what}");
        assert_eq!(
            error.message(),
            format!(
                "A stock balance cannot use {what} {id}: it belongs to another company than the \
                 balance."
            )
        );
    }
    let shared = apply(&mut tx, &[change(base.owner_id(Some(f.o)), "1", "0")]).await;
    assert_eq!(shared.len(), 1);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// serial tracking mode itself is the product field declares)
#[tokio::test]
async fn earliest_incoming_date_wins() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    // received with D1, then with the older D2: D2
    let a = f.key(f.p, f.l).lot_id(Some(f.lot_a));
    let first = apply(&mut tx, &[change(a, "1", "0").in_date(Some(d1()))]).await;
    assert_eq!(first[0].in_date, d1());
    assert_eq!(first[0].available(), dec("1"));
    let second = apply(&mut tx, &[change(a, "1", "0").in_date(Some(d2()))]).await;
    assert_eq!(second[0].quantity, dec("2"));
    assert_eq!(second[0].in_date, d2());
    // received with D2, then with the later D1: D2 stays
    let b = f.key(f.p, f.l).lot_id(Some(f.lot_b));
    apply(&mut tx, &[change(b, "1", "0").in_date(Some(d2()))]).await;
    let later = apply(&mut tx, &[change(b, "1", "0").in_date(Some(d1()))]).await;
    assert_eq!((later[0].quantity, later[0].in_date), (dec("2"), d2()));
    let gathered = gather_quants(
        &mut tx,
        &f.filter(f.p, f.l, true).lot_id(Some(f.lot_b)).strict(true),
    )
    .await
    .unwrap();
    assert_eq!(gathered.len(), 1);
    assert_eq!(gathered[0].in_date, d2());
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// one key holds one row)
#[tokio::test]
async fn untracked_availability_sums_signed_balances() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let broad = f.filter(f.p, f.l, false);
    assert_eq!(available(&mut tx, broad).await, (dec("0"), dec("0")));
    let base = f.key(f.p, f.l);
    apply(&mut tx, &[change(base, "1", "0")]).await;
    assert_eq!(available(&mut tx, broad).await.0, dec("1"));
    tx.rollback().await.unwrap();
    drop(tx);
    // (10, 9) and (1, 1): 1
    let mut tx = f.h.begin().await.unwrap();
    let packaged = base.package_id(Some(f.k));
    apply(
        &mut tx,
        &[change(base, "10", "9"), change(packaged, "1", "1")],
    )
    .await;
    assert_eq!(available(&mut tx, broad).await.0, dec("1"));
    tx.rollback().await.unwrap();

    // products are isolated: P 2, Q 7
    let mut tx = f.h.begin().await.unwrap();
    apply(
        &mut tx,
        &[change(base, "2", "0"), change(f.key(f.q, f.l), "7", "0")],
    )
    .await;
    assert_eq!(available(&mut tx, broad).await.0, dec("2"));
    tx.rollback().await.unwrap();

    // three of 1 and one of -3: 0 both ways
    let mut tx = f.h.begin().await.unwrap();
    let owned = |package: Option<i64>| base.package_id(package).owner_id(Some(f.o));
    apply(
        &mut tx,
        &[
            change(base, "1", "0"),
            change(packaged, "1", "0"),
            change(owned(None), "1", "0"),
            change(owned(Some(f.k)), "-3", "0"),
        ],
    )
    .await;
    assert_eq!(available(&mut tx, broad).await, (dec("0"), dec("0")));
    tx.rollback().await.unwrap();

    // (10, reserved 20) and (5, 0): 0 and -5
    let mut tx = f.h.begin().await.unwrap();
    apply(
        &mut tx,
        &[change(base, "10", "20"), change(packaged, "5", "0")],
    )
    .await;
    assert_eq!(available(&mut tx, broad).await, (dec("0"), dec("-5")));
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn broad_and_strict_reads_select_their_keys() {
    let f = fixture().await;
    let p = |location| f.key(f.p, location);
    // 013: 1 at L, 1 at C
    let mut tx = f.h.begin().await.unwrap();
    apply(
        &mut tx,
        &[change(p(f.l), "1", "0"), change(p(f.c), "1", "0")],
    )
    .await;
    assert_eq!(
        available(&mut tx, f.filter(f.p, f.l, false)).await.0,
        dec("2")
    );
    assert_eq!(
        available(&mut tx, f.filter(f.p, f.c, false)).await.0,
        dec("1")
    );
    tx.rollback().await.unwrap();
    // 014: 1 at L, 2 at C, strict at L
    let mut tx = f.h.begin().await.unwrap();
    apply(
        &mut tx,
        &[change(p(f.l), "1", "0"), change(p(f.c), "2", "0")],
    )
    .await;
    assert_eq!(
        available(&mut tx, f.filter(f.p, f.l, true)).await.0,
        dec("1")
    );
    tx.rollback().await.unwrap();

    let base = p(f.l);
    let packaged = base.package_id(Some(f.k));
    let owned = base.owner_id(Some(f.o));
    // 018, 020: 7 loose and unowned, 2 in K, 2 owned by O; broad filters narrow
    let mut tx = f.h.begin().await.unwrap();
    apply(
        &mut tx,
        &[
            change(base, "7", "0"),
            change(packaged, "2", "0"),
            change(owned, "2", "0"),
        ],
    )
    .await;
    let broad = f.filter(f.p, f.l, false);
    let broad_package = broad.package_id(Some(f.k));
    let broad_owner = broad.owner_id(Some(f.o));
    assert_eq!(available(&mut tx, broad_package).await.0, dec("2"));
    assert_eq!(available(&mut tx, broad_owner).await.0, dec("2"));
    tx.rollback().await.unwrap();
    // 019, 021: 2 loose and unowned, 7 in K, 7 owned by O; strict absent parts mean none
    let mut tx = f.h.begin().await.unwrap();
    apply(
        &mut tx,
        &[
            change(base, "2", "0"),
            change(packaged, "7", "0"),
            change(owned, "7", "0"),
        ],
    )
    .await;
    assert_eq!(
        available(&mut tx, f.filter(f.p, f.l, true)).await.0,
        dec("2")
    );
    let strict_owner = f.filter(f.p, f.l, true).owner_id(Some(f.o));
    assert_eq!(available(&mut tx, strict_owner).await.0, dec("7"));
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn tracked_availability_nets_each_lot() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let base = f.key(f.p, f.l);
    let a = base.lot_id(Some(f.lot_a));
    let b = base.lot_id(Some(f.lot_b));
    let tracked = f.filter(f.p, f.l, false).tracked(true);
    let of_a = tracked.lot_id(Some(f.lot_a));
    // A (10, reserved 20): 0 and -10
    apply(&mut tx, &[change(a, "10", "20")]).await;
    assert_eq!(available(&mut tx, of_a).await, (dec("0"), dec("-10")));
    tx.rollback().await.unwrap();

    // A 2, no lot 3, B 7: broad A reads 5, strict A reads 2; the tracked update left the untracked 3
    let mut tx = f.h.begin().await.unwrap();
    let after = apply(
        &mut tx,
        &[
            change(base, "3", "0"),
            change(a, "2", "0"),
            change(b, "7", "0"),
        ],
    )
    .await;
    let of_key = |key: QuantKey| after.iter().find(|b| b.key == key).unwrap().quantity;
    assert_eq!((of_key(a), of_key(base)), (dec("2"), dec("3")));
    assert_eq!(stored(&mut tx, base).await, Some(("3".into(), "0".into())));
    assert_eq!(available(&mut tx, of_a).await.0, dec("5"));
    let strict_a = of_a.strict(true);
    assert_eq!(available(&mut tx, strict_a).await.0, dec("2"));
    tx.rollback().await.unwrap();

    // A (2, reserved 5) and B 4: 4 clamped per lot, 1 signed
    let mut tx = f.h.begin().await.unwrap();
    apply(&mut tx, &[change(a, "2", "5"), change(b, "4", "0")]).await;
    assert_eq!(available(&mut tx, tracked).await, (dec("4"), dec("1")));
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// duplicate rows; the increase and the decrease are read back as the aggregate)
#[tokio::test]
async fn owner_and_package_keys_change_alone() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let base = f.key(f.p, f.l);
    let owned = base.owner_id(Some(f.o));
    let packaged = base.package_id(Some(f.k));
    apply(
        &mut tx,
        &[
            change(owned, "2", "0"),
            change(base, "7", "0"),
            change(packaged, "2", "0"),
        ],
    )
    .await;
    apply(
        &mut tx,
        &[change(owned, "1", "0"), change(packaged, "1", "0")],
    )
    .await;
    assert_eq!(stored(&mut tx, owned).await, Some(("3".into(), "0".into())));
    assert_eq!(
        stored(&mut tx, packaged).await,
        Some(("3".into(), "0".into()))
    );
    assert_eq!(stored(&mut tx, base).await, Some(("7".into(), "0".into())));
    tx.rollback().await.unwrap();

    let mut tx = f.h.begin().await.unwrap();
    let key = f.key(f.q, f.l);
    apply(&mut tx, &[change(key, "2", "0")]).await;
    apply(&mut tx, &[change(key, "1", "0")]).await;
    assert_eq!(
        available(&mut tx, f.filter(f.q, f.l, false)).await.0,
        dec("3")
    );
    apply(&mut tx, &[change(key, "-2", "0")]).await;
    assert_eq!(
        available(&mut tx, f.filter(f.q, f.l, false)).await.0,
        dec("1")
    );
    assert_eq!(quant_rows(&mut tx, f.q).await, 1);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// the delivery itself is's)
#[tokio::test]
async fn zero_keys_are_deleted_unless_counted() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let packaged = f.key(f.p, f.l).package_id(Some(f.k));
    apply(&mut tx, &[change(packaged, "1", "0")]).await;
    let gone = apply(&mut tx, &[change(packaged, "-1", "0")]).await;
    assert_eq!(gone.len(), 1);
    assert_eq!(gone[0].id, None);
    assert_eq!(gone[0].quantity, Decimal::ZERO);
    assert_eq!(stored(&mut tx, packaged).await, None);
    // a key with an assigned count stays, and its count with it
    let counted = f.key(f.q, f.l);
    let id = apply(&mut tx, &[change(counted, "1", "0")]).await[0]
        .id
        .unwrap();
    let user = tx.env().uid();
    tx.env()
        .records("stock.quant")
        .unwrap()
        .browse([id])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("user_id", Cell::Id(user))
                .set_cell("inventory_quantity", Cell::Decimal(dec("4"))),
        )
        .await
        .unwrap();
    let kept = apply(&mut tx, &[change(counted, "-1", "0")]).await;
    assert_eq!(kept[0].id, Some(id));
    let row = tx
        .query(
            "SELECT trim_scale(quantity)::text, trim_scale(inventory_quantity)::text, user_id FROM stock_quant WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    assert_eq!(row.len(), 1);
    assert_eq!(
        (
            row[0].get::<_, String>(0),
            row[0].get::<_, String>(1),
            row[0].get::<_, i64>(2)
        ),
        ("0".into(), "4".into(), user)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// no vendor row; the virtual-balance read is's)
#[tokio::test]
async fn virtual_locations_store_no_balance() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let out = apply(
        &mut tx,
        &[
            change(f.key(f.p, f.vendor), "-3", "0"),
            change(f.key(f.p, f.l), "3", "0"),
        ],
    )
    .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].key.location_id, f.l);
    assert_eq!(stored(&mut tx, f.key(f.p, f.vendor)).await, None);
    assert_eq!(quant_rows(&mut tx, f.p).await, 1);
    let gathered = gather_quants(&mut tx, &f.filter(f.p, f.vendor, false))
        .await
        .unwrap();
    assert!(gathered.is_empty());
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// `ReservationUnderflow` before any write and the counter is not clamped; the move validation that
/// releases a line is's)
#[tokio::test]
async fn reservation_underflow_is_refused_before_writing() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let key = f.key(f.p, f.l);
    let sibling = f.key(f.q, f.l);
    apply(&mut tx, &[change(key, "10", "3")]).await;
    let error = refused(
        &mut tx,
        &[change(sibling, "1", "0"), change(key, "-5", "-5")],
    )
    .await;
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "Reserved quantity of product {} at location {} would become negative (3 reserved, 5 \
             released). Nothing was changed. This is a defect; please report it.",
            f.p, f.l
        )
    );
    assert_eq!(stored(&mut tx, key).await, Some(("10".into(), "3".into())));
    assert_eq!(stored(&mut tx, sibling).await, None);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// 1: the engine locks and returns keys in the database's ascending order with absent
/// parts last, whatever order the changes came in.
#[tokio::test]
async fn keys_are_processed_in_database_order() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let base = f.key(f.p, f.l);
    let keys = [
        base.owner_id(Some(f.o)),
        base,
        base.lot_id(Some(f.lot_b)).package_id(Some(f.k)),
        base.lot_id(Some(f.lot_a)),
        base.package_id(Some(f.k)).owner_id(Some(f.o)),
        f.key(f.p, f.c),
    ];
    let changes: Vec<QuantChange> = keys.iter().map(|k| change(*k, "1", "0")).collect();
    let out = apply(&mut tx, &changes).await;
    let engine: Vec<i64> = out.iter().map(|b| b.id.unwrap()).collect();
    let database: Vec<i64> = tx
        .query(
            "SELECT id FROM stock_quant WHERE product_id = $1 \
              ORDER BY company_id, product_id, location_id, lot_id, package_id, owner_id",
            &[&f.p],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(engine, database);
    // gather puts lot-bearing balances first
    let gathered = gather_quants(&mut tx, &f.filter(f.p, f.l, false))
        .await
        .unwrap();
    assert_eq!(gathered.len(), 6);
    assert!(gathered[..2].iter().all(|b| b.key.lot_id.is_some()));
    assert!(gathered[2..].iter().all(|b| b.key.lot_id.is_none()));
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// transaction's change count as they were; grouped reads are a later slice)
#[tokio::test]
async fn reads_write_nothing() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let id = apply(&mut tx, &[change(f.key(f.p, f.l), "5", "1")]).await[0]
        .id
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    let before: String = tx
        .query("SELECT xmin::text FROM stock_quant WHERE id = $1", &[&id])
        .await
        .unwrap()[0]
        .get(0);
    let changes = tx.change_count();
    available(&mut tx, f.filter(f.p, f.l, false)).await;
    available(&mut tx, f.filter(f.p, f.l, true)).await;
    gather_quants(&mut tx, &f.filter(f.p, f.l, false))
        .await
        .unwrap();
    assert_eq!(tx.change_count(), changes);
    let after: String = tx
        .query("SELECT xmin::text FROM stock_quant WHERE id = $1", &[&id])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(before, after);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The model refuses generic writes of balances by name (9) and keeps count state
/// writable.
#[tokio::test]
async fn quant_model_refuses_generic_writes() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let id = apply(&mut tx, &[change(f.key(f.p, f.l), "10", "20")]).await[0]
        .id
        .unwrap();
    let quants = tx.env().records("stock.quant").unwrap();
    let quant = quants.browse([id]).unwrap();
    // the per-row available quantity keeps the deficit
    let available = quant.cells(&mut tx, "available_quantity").await.unwrap();
    assert_eq!(available, vec![Cell::Decimal(dec("-10"))]);
    let error = tx
        .savepoint(async |tx| quant.copy(tx, None).await)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "A stock quant cannot be copied. Stock changes through stock moves or an inventory adjustment."
    );
    let error = tx
        .savepoint(async |tx| {
            quants
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("company_id", Cell::Id(f.company))
                            .set_cell("product_id", Cell::Id(f.q))
                            .set_cell("location_id", Cell::Id(f.l))
                            .set_cell("quantity", Cell::Decimal(Decimal::ONE)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Stock quants cannot be created directly: balances change only through stock moves and \
         inventory adjustments."
    );
    for (field, value) in [
        ("quantity", Cell::Decimal(Decimal::ONE)),
        ("reserved_quantity", Cell::Decimal(Decimal::ZERO)),
        ("location_id", Cell::Id(f.x)),
        ("lot_id", Cell::Id(f.lot_a)),
    ] {
        let error = tx
            .savepoint(async |tx| {
                quant
                    .write(tx, RawVals::new().set_cell(field, value.clone()))
                    .await
            })
            .await
            .unwrap_err();
        let label = tx
            .registry()
            .model("stock.quant")
            .unwrap()
            .field(field)
            .unwrap()
            .string
            .clone();
        assert_eq!(error.kind(), ErrorKind::UserError, "{field}");
        assert_eq!(
            error.message(),
            format!(
                "{label} of a stock quant cannot be changed directly: balances change only \
                 through stock moves and inventory adjustments."
            )
        );
    }
    quant
        .write(
            &mut tx,
            RawVals::new().set_cell("inventory_quantity", Cell::Decimal(dec("8"))),
        )
        .await
        .unwrap();
    assert_eq!(
        stored(&mut tx, f.key(f.p, f.l)).await,
        Some(("10".into(), "20".into()))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn locations_with_stock_keep_their_usage_and_stay_active() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    apply(&mut tx, &[change(f.key(f.p, f.c), "1", "0")]).await;
    let locations = tx.env().records("stock.location").unwrap();
    let browse = |id: i64| locations.browse([id]).unwrap();
    let write = async |tx: &mut Transaction, id: i64, vals: RawVals| {
        tx.savepoint(async |tx| browse(id).write(tx, vals).await)
            .await
            .unwrap_err()
    };
    let error = write(&mut tx, f.c, RawVals::new().set_cell("usage", text("view"))).await;
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "A location that holds products cannot become a view location."
    );
    let error = write(
        &mut tx,
        f.c,
        RawVals::new().set_cell("usage", text("inventory")),
    )
    .await;
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "A location that holds stock must stay an internal location."
    );
    let error = write(
        &mut tx,
        f.l,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await;
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Locations L/C still hold products. Empty them before archiving."
    );
    let usage = tx
        .query(
            "SELECT usage, active FROM stock_location WHERE id = $1",
            &[&f.c],
        )
        .await
        .unwrap();
    assert_eq!(
        (usage[0].get::<_, String>(0), usage[0].get::<_, bool>(1)),
        ("internal".into(), true)
    );
    // an empty location changes freely
    browse(f.x)
        .write(&mut tx, RawVals::new().set_cell("usage", text("transit")))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

async fn until_a_session_waits(h: &Harness) {
    let client = h.db().client().await.unwrap();
    let started = Instant::now();
    loop {
        let waiting: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if waiting > 0 {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no session waited for a lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// through the engine; the receipts as move lines are's)
#[tokio::test]
async fn opposite_first_receipts_commit_with_one_row_per_key() {
    let f = fixture().await;
    let at_l = f.key(f.p, f.l);
    let at_c = f.key(f.p, f.c);
    let mut a = f.h.begin().await.unwrap();
    apply(&mut a, &[change(at_l, "1", "0"), change(at_c, "1", "0")]).await;
    let attempts = AtomicUsize::new(0);
    let second = f.h.run(async |tx| {
        attempts.fetch_add(1, Ordering::SeqCst);
        apply_quant_changes(tx, &[change(at_c, "2", "0"), change(at_l, "2", "0")]).await
    });
    let release = async {
        until_a_session_waits(&f.h).await;
        a.commit().await.unwrap();
    };
    let (second, ()) = tokio::join!(second, release);
    let second = second.unwrap();
    assert_eq!(second.len(), 2);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "the waiting attempt fails with 40001 after the commit and the next one applies"
    );
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(stored(&mut tx, at_l).await, Some(("3".into(), "0".into())));
    assert_eq!(stored(&mut tx, at_c).await, Some(("3".into(), "0".into())));
    assert_eq!(quant_rows(&mut tx, f.p).await, 2);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// orders, the second waiting on the first; line validation is's)
#[tokio::test]
async fn opposite_transfers_do_not_deadlock() {
    let f = fixture().await;
    let at_l = f.key(f.p, f.l);
    let at_x = f.key(f.p, f.x);
    let mut tx = f.h.begin().await.unwrap();
    apply(&mut tx, &[change(at_l, "10", "0"), change(at_x, "10", "0")]).await;
    tx.commit().await.unwrap();
    let mut a = f.h.begin().await.unwrap();
    apply(&mut a, &[change(at_l, "-3", "0"), change(at_x, "3", "0")]).await;
    let second = f.h.run(async |tx| {
        apply_quant_changes(tx, &[change(at_x, "-4", "0"), change(at_l, "4", "0")]).await
    });
    let release = async {
        until_a_session_waits(&f.h).await;
        a.commit().await.unwrap();
    };
    let (second, ()) = tokio::join!(second, release);
    second.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(stored(&mut tx, at_l).await, Some(("11".into(), "0".into())));
    assert_eq!(stored(&mut tx, at_x).await, Some(("9".into(), "0".into())));
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// a quant with an on-hand or reserved quantity is refused for every caller, the superuser too, by
/// .9, where the reference exempts the superuser and lets a stock manager zero it through
/// an inventory adjustment; the manager and non-manager split comes with the stock
/// access rows; a zero quant is deleted by whoever the access allows)
#[tokio::test]
async fn quants_with_a_balance_are_not_deleted() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let on_hand = f.key(f.p, f.l);
    let reserved_only = f.key(f.q, f.l);
    let counted = f.key(f.p, f.c);
    let user = tx.env().uid();
    apply(
        &mut tx,
        &[
            change(on_hand, "1", "0"),
            change(reserved_only, "1", "1"),
            change(counted, "1", "0"),
        ],
    )
    .await;
    apply(&mut tx, &[change(reserved_only, "-1", "0")]).await;
    let quants = tx.env().records("stock.quant").unwrap();
    let id_of = async |tx: &mut Transaction, key: QuantKey| -> i64 {
        gather_quants(
            tx,
            &f.filter(key.product_id, key.location_id, true).strict(true),
        )
        .await
        .unwrap()[0]
            .id
            .unwrap()
    };
    let (a, b, c) = (
        id_of(&mut tx, on_hand).await,
        id_of(&mut tx, reserved_only).await,
        id_of(&mut tx, counted).await,
    );
    // the count keeps the zero key when the engine empties it
    quants
        .browse([c])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("user_id", Cell::Id(user)))
        .await
        .unwrap();
    apply(&mut tx, &[change(counted, "-1", "0")]).await;
    for ids in [vec![a], vec![b], vec![a, c]] {
        let error = tx
            .savepoint(async |tx| quants.browse(ids.clone()).unwrap().unlink(tx).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{ids:?}");
        assert_eq!(
            error.message(),
            "A stock quant with an on-hand or reserved quantity cannot be deleted: its balance \
             changes only through stock moves or an inventory adjustment."
        );
    }
    assert_eq!(
        stored(&mut tx, on_hand).await,
        Some(("1".into(), "0".into()))
    );
    assert_eq!(
        stored(&mut tx, reserved_only).await,
        Some(("0".into(), "1".into()))
    );
    // a zero quant is deleted
    quants.browse([c]).unwrap().unlink(&mut tx).await.unwrap();
    assert_eq!(stored(&mut tx, counted).await, None);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// RPC layer uses and through a typed write)
/// RPC layer uses (`null`, `0`) and through a typed write (`Null`, `Int(0)`))
#[tokio::test]
async fn converted_values_cannot_archive_or_retype_a_location_with_stock() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    apply(&mut tx, &[change(f.key(f.p, f.c), "1", "0")]).await;
    let locations = tx.env().records("stock.location").unwrap();
    let archive = "Locations L/C still hold products. Empty them before archiving.";
    let retype = "A location that holds stock must stay an internal location.";
    let active = "active".to_owned();
    let usage = "usage".to_owned();
    // what JSON `null` and `0` become on the RPC write path
    for (name, value, location, message) in [
        (&active, Value::False, f.l, archive),
        (&active, Value::Int(0), f.l, archive),
        (&usage, Value::False, f.c, retype),
    ] {
        let error = tx
            .savepoint(async |tx| {
                locations
                    .browse([location])
                    .unwrap()
                    .write_values(tx, [(name, &value)])
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{name} {value:?}");
        assert_eq!(error.message(), message, "{name} {value:?}");
    }
    // a typed caller's cells reach the override unconverted
    for (name, cell, location, message) in [
        ("active", Cell::Null, f.l, archive),
        ("active", Cell::Int(0), f.l, archive),
        ("usage", Cell::Null, f.c, retype),
    ] {
        let error = tx
            .savepoint(async |tx| {
                locations
                    .browse([location])
                    .unwrap()
                    .write(tx, RawVals::new().set_cell(name, cell.clone()))
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{name} {cell:?}");
        assert_eq!(error.message(), message, "{name} {cell:?}");
    }
    let row = tx
        .query(
            "SELECT bool_and(active), bool_and(usage = 'internal') FROM stock_location \
              WHERE id = ANY($1)",
            &[&vec![f.l, f.c]],
        )
        .await
        .unwrap();
    assert_eq!(
        (row[0].get::<_, bool>(0), row[0].get::<_, bool>(1)),
        (true, true)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// 3(a): A delivers the last unit of a key (its zero row is deleted, uncommitted); B,
/// already begun, receives into the same key and waits on A; A commits. B's first attempt sees the
/// deleted row in its snapshot and is retried as a serialization failure; the retry applies. Review
/// cycle 12, finding M1.
#[tokio::test]
async fn a_key_deleted_by_a_concurrent_transaction_is_retried() {
    let f = fixture().await;
    let key = f.key(f.p, f.l);
    let mut tx = f.h.begin().await.unwrap();
    apply(&mut tx, &[change(key, "1", "0")]).await;
    tx.commit().await.unwrap();
    let mut a = f.h.begin().await.unwrap();
    let delivered = apply(&mut a, &[change(key, "-1", "0")]).await;
    assert_eq!(delivered[0].id, None, "A deleted the zero key");
    let attempts = AtomicUsize::new(0);
    let second = f.h.run(async |tx| {
        attempts.fetch_add(1, Ordering::SeqCst);
        apply_quant_changes(tx, &[change(key, "2", "0")]).await
    });
    let release = async {
        until_a_session_waits(&f.h).await;
        a.commit().await.unwrap();
    };
    let (second, ()) = tokio::join!(second, release);
    let second = second.unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].quantity, dec("2"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(stored(&mut tx, key).await, Some(("2".into(), "0".into())));
    assert_eq!(quant_rows(&mut tx, f.p).await, 1);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The generic upsert by the declared key is refused: it would run no create or write override.
/// Review cycle 12, finding S1.
#[tokio::test]
async fn quant_upsert_is_refused() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let quants = tx.env().records("stock.quant").unwrap();
    let row = RawVals::new()
        .set_cell("company_id", Cell::Id(f.company))
        .set_cell("product_id", Cell::Id(f.p))
        .set_cell("location_id", Cell::Id(f.l))
        .set_cell("lot_id", Cell::Null)
        .set_cell("package_id", Cell::Null)
        .set_cell("owner_id", Cell::Null)
        .set_cell("in_date", Cell::DateTime(d1()))
        .set_cell("quantity", Cell::Decimal(dec("5")));
    let error = tx
        .savepoint(async |tx| {
            quants
                .upsert(
                    tx,
                    erp_sdk::UpsertCall::new("_key")
                        .with_rows([row])
                        .with_accumulate("quantity"),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
    assert_eq!(
        error.message(),
        "Stock quants are not created or updated by key: balances change only through stock moves \
         and inventory adjustments."
    );
    assert_eq!(quant_rows(&mut tx, f.p).await, 0);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Divergent from the reference (review cycle 12, S3): it blocks a usage change only on a positive
/// quantity; balances at virtual usages are not stored here (1), so a negative or
/// reserved-only balance would be frozen there, and it blocks the change too.
#[tokio::test]
async fn usage_and_company_changes_are_blocked_by_any_balance() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    // C holds -2; X holds 0 on hand with 1 reserved
    apply(
        &mut tx,
        &[
            change(f.key(f.p, f.c), "-2", "0"),
            change(f.key(f.p, f.x), "1", "1"),
        ],
    )
    .await;
    apply(&mut tx, &[change(f.key(f.p, f.x), "-1", "0")]).await;
    assert_eq!(
        stored(&mut tx, f.key(f.p, f.x)).await,
        Some(("0".into(), "1".into()))
    );
    let locations = tx.env().records("stock.location").unwrap();
    for location in [f.c, f.x] {
        let error = tx
            .savepoint(async |tx| {
                locations
                    .browse([location])
                    .unwrap()
                    .write(tx, RawVals::new().set_cell("usage", text("inventory")))
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{location}");
        assert_eq!(
            error.message(),
            "A location that holds stock must stay an internal location."
        );
    }
    // the company of a location never changes; writing the one it has is accepted
    let current: Option<i64> = tx
        .query(
            "SELECT company_id FROM stock_location WHERE id = $1",
            &[&f.l],
        )
        .await
        .unwrap()[0]
        .get(0);
    let other = if current == Some(f.other_company) {
        f.company
    } else {
        f.other_company
    };
    for value in [Cell::Id(other), Cell::Null] {
        if current.is_none() && value == Cell::Null {
            continue;
        }
        let error = tx
            .savepoint(async |tx| {
                locations
                    .browse([f.l])
                    .unwrap()
                    .write(tx, RawVals::new().set_cell("company_id", value.clone()))
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError, "{value:?}");
        assert_eq!(
            error.message(),
            "The company of this record cannot be changed once it exists. Archive it and create a new record for the other company."
        );
    }
    locations
        .browse([f.l])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("company_id", current.map_or(Cell::Null, Cell::Id)),
        )
        .await
        .unwrap();
    let after: Option<i64> = tx
        .query(
            "SELECT company_id FROM stock_location WHERE id = $1",
            &[&f.l],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(after, current);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The engine and the guards read by SQL and see ORM writes of the same transaction that are not
/// flushed yet: a location made a view refuses a balance at once (the engine's savepoint flushes), a
/// child moved to another parent counts there in the availability read, and archiving its new parent
/// is refused. Review cycle 12, finding S4.
#[tokio::test]
async fn engine_reads_see_pending_orm_writes() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let locations = tx.env().records("stock.location").unwrap();
    let view = create(
        &mut tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Soon a view"))
            .set_cell("usage", text("internal")),
    )
    .await;
    tx.flush().await.unwrap();
    locations
        .browse([view])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("usage", text("view")))
        .await
        .unwrap();
    let error = refused(&mut tx, &[change(f.key(f.p, view), "1", "0")]).await;
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        "Products cannot be taken from or delivered to Soon a view: it is a view location."
    );
    assert_eq!(quant_rows(&mut tx, f.p).await, 0);
    // C (holding 1) moves from L to X; nothing flushed in between
    apply(&mut tx, &[change(f.key(f.p, f.c), "1", "0")]).await;
    locations
        .browse([f.c])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("location_id", Cell::Id(f.x)),
        )
        .await
        .unwrap();
    assert_eq!(
        available(&mut tx, f.filter(f.p, f.l, false)).await.0,
        dec("0")
    );
    // the availability read flushed; move C back and forth again so the archive guard alone meets a
    // pending parent (no savepoint here: it would flush first)
    for parent in [f.l, f.x] {
        locations
            .browse([f.c])
            .unwrap()
            .write(
                &mut tx,
                RawVals::new().set_cell("location_id", Cell::Id(parent)),
            )
            .await
            .unwrap();
    }
    let error = locations
        .browse([f.x])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
    assert_eq!(
        error.message(),
        "Locations X/C still hold products. Empty them before archiving."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// 4: a later engine call of one transaction locks only keys above the highest key it
/// holds, or keys it holds already; a lower new key is refused before any lock, naming both keys.
/// Review cycle 12, finding S5.
#[tokio::test]
async fn a_later_call_below_a_held_key_is_refused() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let high = f.key(f.p, f.x);
    let low = f.key(f.p, f.l);
    apply(&mut tx, &[change(high, "1", "0")]).await;
    let error = refused(&mut tx, &[change(low, "1", "0")]).await;
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "Stock balance keys are locked in ascending order within a transaction: key (company \
             {c}, product {p}, location {l}) is below key (company {c}, product {p}, location {x}), \
             which an earlier call of this transaction locked. Pass both changes in one call, or \
             call in key order.",
            c = f.company,
            p = f.p,
            l = f.l,
            x = f.x
        )
    );
    assert_eq!(stored(&mut tx, low).await, None);
    // a held key and a key above it are accepted
    apply(
        &mut tx,
        &[change(high, "1", "0"), change(f.key(f.q, f.l), "1", "0")],
    )
    .await;
    assert_eq!(stored(&mut tx, high).await, Some(("2".into(), "0".into())));
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// A key naming a package that does not exist is refused by name before any lock. Review cycle 12,
/// finding N6.
#[tokio::test]
async fn a_missing_package_is_refused_by_name() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let missing: i64 = tx
        .query("SELECT COALESCE(max(id), 0) + 1000 FROM stock_package", &[])
        .await
        .unwrap()[0]
        .get(0);
    let key = f.key(f.p, f.l).package_id(Some(missing));
    let error = refused(&mut tx, &[change(key, "1", "0")]).await;
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert_eq!(
        error.message(),
        format!("A stock balance change names package {missing}, which does not exist.")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// A key locked inside a savepoint that then rolled back is no longer held (the rollback released
/// its row lock), so naming it again below a higher held key is refused; a key locked inside a
/// savepoint that was released stays held. Review cycle 12 round 2, finding S-1.
#[tokio::test]
async fn a_key_locked_in_a_rolled_back_savepoint_is_not_held() {
    let f = fixture().await;
    let low = f.key(f.p, f.l);
    let high = f.key(f.p, f.x);
    let mut tx = f.h.begin().await.unwrap();
    let undone = tx
        .savepoint(async |tx| {
            apply_quant_changes(tx, &[change(low, "1", "0")]).await?;
            Err::<(), _>(Error::user("undo the receipt"))
        })
        .await;
    assert!(undone.is_err());
    apply(&mut tx, &[change(high, "1", "0")]).await;
    let error = refused(&mut tx, &[change(low, "1", "0")]).await;
    assert_eq!(error.kind(), ErrorKind::Programming, "{error:?}");
    assert!(
        error
            .message()
            .starts_with("Stock balance keys are locked in ascending order"),
        "{error:?}"
    );
    assert_eq!(stored(&mut tx, low).await, None);
    tx.rollback().await.unwrap();
    drop(tx);
    // a released savepoint keeps its locks: the key stays held
    let mut tx = f.h.begin().await.unwrap();
    tx.savepoint(async |tx| apply_quant_changes(tx, &[change(low, "1", "0")]).await)
        .await
        .unwrap();
    apply(&mut tx, &[change(high, "1", "0")]).await;
    apply(&mut tx, &[change(low, "1", "0")]).await;
    assert_eq!(stored(&mut tx, low).await, Some(("2".into(), "0".into())));
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Round 1 S6, the guard after the receipt: A receives into empty location X and has not committed;
/// B makes X a view. B's guard must not pass on a snapshot that cannot see A's quant: it is retried
/// and, once A committed, refused. No balance ends at a view location.
#[tokio::test]
async fn a_location_guard_retries_past_a_concurrent_receipt() {
    let f = fixture().await;
    let mut a = f.h.begin().await.unwrap();
    apply(&mut a, &[change(f.key(f.p, f.x), "1", "0")]).await;
    let attempts = AtomicUsize::new(0);
    let guard = f.h.run(async |tx| {
        attempts.fetch_add(1, Ordering::SeqCst);
        tx.env()
            .records("stock.location")?
            .browse([f.x])?
            .write(tx, RawVals::new().set_cell("usage", text("view")))
            .await
    });
    let release = async {
        let started = Instant::now();
        while attempts.load(Ordering::SeqCst) < 2 && started.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        a.commit().await.unwrap();
    };
    let (guard, ()) = tokio::join!(guard, release);
    let error = orm_error(guard.expect_err("the guard passed although X holds a balance"));
    assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
    assert_eq!(
        error.message(),
        "A location that holds products cannot become a view location."
    );
    assert!(attempts.load(Ordering::SeqCst) >= 2);
    let mut tx = f.h.begin().await.unwrap();
    let usage: String = tx
        .query("SELECT usage FROM stock_location WHERE id = $1", &[&f.x])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(usage, "internal");
    assert_eq!(
        stored(&mut tx, f.key(f.p, f.x)).await,
        Some(("1".into(), "0".into()))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Round 1 S6, the receipt after the guard: B makes empty X a view and has not committed; A receives
/// into X. A waits for B, is retried on a fresh snapshot after B committed and refused.
#[tokio::test]
async fn a_receipt_behind_a_location_guard_is_retried_and_refused() {
    let f = fixture().await;
    let mut b = f.h.begin().await.unwrap();
    b.env()
        .records("stock.location")
        .unwrap()
        .browse([f.x])
        .unwrap()
        .write(&mut b, RawVals::new().set_cell("usage", text("view")))
        .await
        .unwrap();
    let attempts = AtomicUsize::new(0);
    let receipt = f.h.run(async |tx| {
        attempts.fetch_add(1, Ordering::SeqCst);
        apply_quant_changes(tx, &[change(f.key(f.p, f.x), "1", "0")]).await
    });
    let release = async {
        until_a_session_waits(&f.h).await;
        b.commit().await.unwrap();
    };
    let (receipt, ()) = tokio::join!(receipt, release);
    let error = orm_error(receipt.expect_err("the receipt stored a balance at a view location"));
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        "Products cannot be taken from or delivered to X: it is a view location."
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(quant_rows(&mut tx, f.p).await, 0);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Round 1 S6, the tracking read: A receives a lot quant of P and has not committed; B checks P's
/// change from lot to serial. B is retried and, once A committed, refused for the lot it holds.
#[tokio::test]
async fn a_tracking_check_retries_past_a_concurrent_lot_receipt() {
    use erp_addon_stock::tracking::{Tracking, TrackingChange, check_tracking_change};
    let f = fixture().await;
    let mut a = f.h.begin().await.unwrap();
    let lot_key = f.key(f.p, f.l).lot_id(Some(f.lot_a));
    apply(&mut a, &[change(lot_key, "1", "0")]).await;
    let attempts = AtomicUsize::new(0);
    let check = f.h.run(async |tx| {
        attempts.fetch_add(1, Ordering::SeqCst);
        check_tracking_change(
            tx,
            &[TrackingChange {
                product_id: f.p,
                from: Tracking::Lot,
                to: Tracking::Serial,
            }],
        )
        .await
    });
    let release = async {
        let started = Instant::now();
        while attempts.load(Ordering::SeqCst) < 2 && started.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        a.commit().await.unwrap();
    };
    let (check, ()) = tokio::join!(check, release);
    let error =
        orm_error(check.expect_err("the tracking check passed although P holds a lot quant"));
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert!(error.message().starts_with("Tracking of "), "{error:?}");
    assert!(attempts.load(Ordering::SeqCst) >= 2);
    f.h.cleanup().await.unwrap();
}

/// The ORM error a harness unit of work ended with.
fn orm_error(error: HarnessError) -> Error {
    match error {
        HarnessError::Orm(error) => error,
        other => panic!("not an ORM error: {other:?}"),
    }
}
