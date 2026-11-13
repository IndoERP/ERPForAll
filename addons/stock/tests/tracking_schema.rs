// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema of the stock tracking tables: lot,
//! lot-name scope, package, quant and serial position. Schema only: the rows below are written with
//! plain SQL or plain ORM creates, because the transition units that maintain them are later slices.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;

erp_sdk::addon!(name = "test_stock_track", depends = ["stock"]);

/// A test stand-in for `ir.model.data` with the columns the product addon and the company create path
/// read (the real model lives in `erp-data`, out of reach of an addon crate; the product tests use the
/// same stand-in).
#[model(
    name = "ir.model.data",
    description = "Model Data",
    addon = "test_stock_track"
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
    /// Read by the company create path.
    #[field(boolean)]
    pub noupdate: bool,
}

/// Ids every case starts from: one company with a currency, a second company, two products, two
/// locations, a partner.
struct Fixture {
    h: Harness,
    company: i64,
    other_company: i64,
    product: i64,
    other_product: i64,
    location: i64,
    other_location: i64,
    partner: i64,
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
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
    // the companies' currency: an existing one, else a new one (as the product tests do)
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
    // a harness starts without companies
    let company = create(
        &mut tx,
        "res.company",
        RawVals::new()
            .set_cell("name", text("Main Company"))
            .set_cell("currency_id", Cell::Id(currency)),
    )
    .await;
    let other_company = create(
        &mut tx,
        "res.company",
        RawVals::new()
            .set_cell("name", text("Other Company"))
            .set_cell("currency_id", Cell::Id(currency)),
    )
    .await;
    let mut products = Vec::new();
    for name in ["Widget", "Gadget"] {
        let env = tx.env().with_company(company).unwrap();
        let template = env
            .records("product.template")
            .unwrap()
            .create(&mut tx, vec![RawVals::new().set_cell("name", text(name))])
            .await
            .unwrap()
            .rec_ids()[0]
            .get();
        let rows = tx
            .query(
                "SELECT id FROM product_product WHERE product_tmpl_id = $1",
                &[&template],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "one variant of {name}");
        products.push(rows[0].get::<_, i64>(0));
    }
    let location = create(
        &mut tx,
        "stock.location",
        RawVals::new().set_cell("name", text("Stock")),
    )
    .await;
    let other_location = create(
        &mut tx,
        "stock.location",
        RawVals::new().set_cell("name", text("Shelf 2")),
    )
    .await;
    let partner = create(
        &mut tx,
        "res.partner",
        RawVals::new().set_cell("name", text("Consignor")),
    )
    .await;
    tx.commit().await.unwrap();
    Fixture {
        h,
        company,
        other_company,
        product: products[0],
        other_product: products[1],
        location,
        other_location,
        partner,
    }
}

/// `(column, data type, nullable)` of the named columns of `table`, in the order asked.
async fn columns(tx: &mut Transaction, table: &str, names: &[&str]) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    for name in names {
        let rows = tx
            .query(
                "SELECT data_type::text, is_nullable::text FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
                &[&table, name],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "column {table}.{name} exists once");
        let nullable: String = rows[0].get(1);
        out.push(((*name).to_owned(), rows[0].get(0), nullable == "YES"));
    }
    out
}

fn expect(spec: &[(&str, &str, bool)]) -> Vec<(String, String, bool)> {
    spec.iter()
        .map(|(n, t, null)| ((*n).to_owned(), (*t).to_owned(), *null))
        .collect()
}

/// The definition of index `name`, from `USING` on (the schema prefix before it varies per test
/// database).
async fn index_definition(tx: &mut Transaction, name: &str) -> String {
    let rows = tx
        .query(
            "SELECT pg_get_indexdef(c.oid) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = current_schema() AND c.relname = $1",
            &[&name],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "index {name} exists");
    let def: String = rows[0].get(0);
    def.split_once(" USING ")
        .expect("an index definition")
        .1
        .to_owned()
}

/// Run `sql` in a savepoint and return the error it must raise.
async fn refused(tx: &mut Transaction, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Error {
    tx.savepoint(async |tx| tx.execute(sql, params).await)
        .await
        .expect_err(sql)
}

fn assert_violation(error: &Error, sqlstate: &str, constraint: &str, message: &str) {
    assert_eq!(error.sqlstate(), Some(sqlstate), "{error:?}");
    assert_eq!(error.constraint(), Some(constraint), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        format!("The operation cannot be completed: {message}")
    );
}

const QUANT_INSERT: &str = "INSERT INTO stock_quant \
    (company_id, product_id, location_id, owner_id, quantity, reserved_quantity, in_date) \
    VALUES ($1, $2, $3, $4, $5::text::numeric, $6::text::numeric, now())";

/// Insert one quant row of the fixture's company, product and location, without lot and package.
async fn quant(
    f: &Fixture,
    tx: &mut Transaction,
    owner: Option<i64>,
    qty: &str,
    reserved: &str,
) -> Result<u64> {
    tx.savepoint(async |tx| {
        tx.execute(
            QUANT_INSERT,
            &[&f.company, &f.product, &f.location, &owner, &qty, &reserved],
        )
        .await
    })
    .await
}

/// included, and a key differing in one part is a second row; no reservation unit runs)
#[tokio::test]
async fn quant_key_admits_one_row_per_key_with_null_parts() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(quant(&f, &mut tx, None, "10", "0").await.unwrap(), 1);
    let error = quant(&f, &mut tx, None, "3", "0").await.unwrap_err();
    assert_violation(
        &error,
        "23505",
        "stock_quant_key",
        "A stock balance already exists for this company, product, location, lot, package and owner.",
    );
    // the owner is a key part: consigned stock is its own row
    assert_eq!(
        quant(&f, &mut tx, Some(f.partner), "3", "0").await.unwrap(),
        1
    );
    let rows = tx
        .query(
            "SELECT quantity::text, owner_id FROM stock_quant ORDER BY owner_id NULLS FIRST",
            &[],
        )
        .await
        .unwrap();
    let got: Vec<(String, Option<i64>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        got,
        [("10".to_owned(), None), ("3".to_owned(), Some(f.partner))]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// unit races)
#[tokio::test]
async fn quant_reserved_counter_is_never_negative_and_quantities_are_finite() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let error = quant(&f, &mut tx, None, "1", "-0.001").await.unwrap_err();
    assert_violation(
        &error,
        "23514",
        "stock_quant_reserved_nonneg",
        "The reserved quantity of a stock balance cannot be negative.",
    );
    // NaN passes `reserved_quantity >= 0` in PostgreSQL; the finite check reports it first
    for (qty, reserved, constraint, column) in [
        (
            "1",
            "NaN",
            "stock_quant_finite_reserved_quantity",
            "reserved quantity",
        ),
        (
            "1",
            "Infinity",
            "stock_quant_finite_reserved_quantity",
            "reserved quantity",
        ),
        ("NaN", "0", "stock_quant_finite_quantity", "quantity"),
        ("-Infinity", "0", "stock_quant_finite_quantity", "quantity"),
    ] {
        let error = quant(&f, &mut tx, None, qty, reserved).await.unwrap_err();
        assert_violation(
            &error,
            "23514",
            constraint,
            &format!("The {column} of a stock balance must be a finite number."),
        );
    }
    let error = refused(
        &mut tx,
        "INSERT INTO stock_quant (company_id, product_id, location_id, quantity, reserved_quantity, \
         in_date, inventory_quantity) VALUES ($1, $2, $3, 0, 0, now(), 'NaN')",
        &[&f.company, &f.product, &f.location],
    )
    .await;
    assert_violation(
        &error,
        "23514",
        "stock_quant_finite_inventory_quantity",
        "The counted quantity of a stock balance must be a finite number.",
    );
    assert_eq!(quant(&f, &mut tx, None, "-2", "0").await.unwrap(), 1);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The quant columns: the company is stored and required and has no compute or relation behind it
/// (B6); quantities are unconstrained `numeric` (1); the covering index of the
/// removal order and the not-null indexes exist.
#[tokio::test]
async fn quant_columns_company_and_indexes() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        columns(
            &mut tx,
            "stock_quant",
            &[
                "company_id",
                "product_id",
                "location_id",
                "lot_id",
                "package_id",
                "owner_id",
                "quantity",
                "reserved_quantity",
                "in_date",
                "inventory_quantity",
                "user_id",
            ]
        )
        .await,
        expect(&[
            ("company_id", "bigint", false),
            ("product_id", "bigint", false),
            ("location_id", "bigint", false),
            ("lot_id", "bigint", true),
            ("package_id", "bigint", true),
            ("owner_id", "bigint", true),
            ("quantity", "numeric", false),
            ("reserved_quantity", "numeric", false),
            ("in_date", "timestamp without time zone", false),
            ("inventory_quantity", "numeric", true),
            ("user_id", "bigint", true),
        ])
    );
    let spec = f.h.registry().model("stock.quant").unwrap();
    let company = spec.field("company_id").unwrap();
    assert!(company.store && company.required);
    assert_eq!((company.compute, company.related), (None, None));
    assert_eq!(
        index_definition(&mut tx, "stock_quant_removal_order").await,
        "btree (company_id, product_id, location_id, in_date, id) INCLUDE (quantity, reserved_quantity, lot_id, package_id, owner_id)"
    );
    for (field, expected) in [
        ("product_id", "btree (product_id)"),
        ("location_id", "btree (location_id)"),
        ("lot_id", "btree (lot_id) WHERE (lot_id IS NOT NULL)"),
        (
            "package_id",
            "btree (package_id) WHERE (package_id IS NOT NULL)",
        ),
        ("owner_id", "btree (owner_id) WHERE (owner_id IS NOT NULL)"),
    ] {
        assert_eq!(
            index_definition(&mut tx, &format!("stock_quant__{field}_index")).await,
            expected
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

const POSITION_INSERT: &str = "INSERT INTO stock_serial_position \
    (company_id, product_id, lot_id, location_id, quantity) VALUES ($1, $2, $3, $4, $5::text::numeric)";

async fn lot(tx: &mut Transaction, product: i64, company: Option<i64>, name: &str) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("product_id", Cell::Id(product));
    if let Some(company) = company {
        vals = vals.set_cell("company_id", Cell::Id(company));
    }
    create(tx, "stock.lot", vals).await
}

#[tokio::test]
async fn serial_position_refuses_more_than_one() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let sn = lot(&mut tx, f.product, Some(f.company), "SN001").await;
    let error = refused(
        &mut tx,
        POSITION_INSERT,
        &[&f.company, &f.product, &sn, &f.location, &"2"],
    )
    .await;
    assert_violation(
        &error,
        "23514",
        "stock_serial_position_max_one",
        "A serial number cannot be more than once at one location.",
    );
    // NaN would fail `quantity <= 1` too; the finite check sorts first and names the real fault
    let error = refused(
        &mut tx,
        POSITION_INSERT,
        &[&f.company, &f.product, &sn, &f.location, &"NaN"],
    )
    .await;
    assert_violation(
        &error,
        "23514",
        "stock_serial_position_finite_quantity",
        "The quantity of a serial position must be a finite number.",
    );
    // one row per (company, product, lot, location)
    tx.execute(
        POSITION_INSERT,
        &[&f.company, &f.product, &sn, &f.location, &"1"],
    )
    .await
    .unwrap();
    let error = refused(
        &mut tx,
        POSITION_INSERT,
        &[&f.company, &f.product, &sn, &f.location, &"0"],
    )
    .await;
    assert_violation(
        &error,
        "23505",
        "stock_serial_position_key",
        "A serial number has one position per company and location.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// runs)
#[tokio::test]
async fn serial_position_accepts_whole_unit_boundary() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let sn = lot(&mut tx, f.product, Some(f.company), "SN001").await;
    let mut locations = vec![f.location, f.other_location];
    let third = create(
        &mut tx,
        "stock.location",
        RawVals::new().set_cell("name", text("Shelf 3")),
    )
    .await;
    locations.push(third);
    for (location, qty) in locations.iter().zip(["-1", "0", "1"]) {
        tx.execute(
            POSITION_INSERT,
            &[&f.company, &f.product, &sn, location, &qty],
        )
        .await
        .unwrap();
    }
    let rows = tx
        .query(
            "SELECT quantity::text FROM stock_serial_position ORDER BY quantity",
            &[],
        )
        .await
        .unwrap();
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["-1", "0", "1"]);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// no move completion runs)
#[tokio::test]
async fn serial_position_has_no_lower_bound() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let sn = lot(&mut tx, f.product, Some(f.company), "SN001").await;
    tx.execute(
        POSITION_INSERT,
        &[&f.company, &f.product, &sn, &f.location, &"-2"],
    )
    .await
    .unwrap();
    let qty: String = tx
        .query("SELECT quantity::text FROM stock_serial_position", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(qty, "-2");
    let columns = columns(
        &mut tx,
        "stock_serial_position",
        &[
            "company_id",
            "product_id",
            "lot_id",
            "location_id",
            "quantity",
        ],
    )
    .await;
    assert_eq!(
        columns,
        expect(&[
            ("company_id", "bigint", false),
            ("product_id", "bigint", false),
            ("lot_id", "bigint", false),
            ("location_id", "bigint", false),
            ("quantity", "numeric", false),
        ])
    );
    // narrow row: no access log columns
    let log = tx
        .query(
            "SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() \
             AND table_name IN ('stock_serial_position', 'stock_lot_name_scope') \
             AND column_name IN ('create_uid', 'create_date', 'write_uid', 'write_date')",
            &[],
        )
        .await
        .unwrap();
    assert!(log.is_empty(), "{} access log columns", log.len());
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

const LOT_MESSAGE: &str = "The combination of lot/serial number and product must be unique within a company including when no company is defined.";

/// the shared/company pairs need the scope-row statement, and the message does not list the
/// product and name yet)
#[tokio::test]
async fn lot_key_refuses_same_company_and_shared_duplicates() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    for company in [Some(f.company), None] {
        lot(&mut tx, f.product, company, "SN001").await;
        let error = tx
            .savepoint(async |tx| {
                let mut vals = RawVals::new()
                    .set_cell("name", text("SN001"))
                    .set_cell("product_id", Cell::Id(f.product));
                if let Some(company) = company {
                    vals = vals.set_cell("company_id", Cell::Id(company));
                }
                tx.env().records("stock.lot")?.create(tx, vec![vals]).await
            })
            .await
            .unwrap_err();
        assert_violation(&error, "23505", "stock_lot_name_unique", LOT_MESSAGE);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// scope-row statement is not exercised)
#[tokio::test]
async fn lot_name_may_repeat_across_companies() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let a = lot(&mut tx, f.product, Some(f.company), "SN001").await;
    let b = lot(&mut tx, f.product, Some(f.other_company), "SN001").await;
    let rows = tx
        .query(
            "SELECT company_id FROM stock_lot WHERE id = ANY($1) ORDER BY id",
            &[&vec![a, b]],
        )
        .await
        .unwrap();
    let got: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, [f.company, f.other_company]);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// statement is not exercised)
#[tokio::test]
async fn lot_name_may_repeat_across_products() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    lot(&mut tx, f.product, Some(f.company), "SN001").await;
    lot(&mut tx, f.other_product, Some(f.company), "SN001").await;
    let count: i64 = tx
        .query("SELECT count(*) FROM stock_lot WHERE name = 'SN001'", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 2);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// neither; the message does not list the product and name yet)
#[tokio::test]
async fn lot_batch_duplicate_is_refused_whole() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let vals = || {
        RawVals::new()
            .set_cell("name", text("SN001"))
            .set_cell("product_id", Cell::Id(f.product))
            .set_cell("company_id", Cell::Id(f.company))
    };
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("stock.lot")?
                .create(tx, vec![vals(), vals()])
                .await
        })
        .await
        .unwrap_err();
    assert_violation(&error, "23505", "stock_lot_name_unique", LOT_MESSAGE);
    let count: i64 = tx
        .query("SELECT count(*) FROM stock_lot", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The variant of a new product template restricted to `company`.
async fn restricted_product(tx: &mut Transaction, company: i64, name: &str) -> i64 {
    let env = tx.env().with_company(company).unwrap();
    let template = env
        .records("product.template")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("company_id", Cell::Id(company)),
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

fn crossover(lot: &str, product: &str) -> String {
    format!(
        "Uh-oh! You’ve got some company inconsistencies here:\n\
         - “{lot}” belongs to company “Main Company” while “Product” (product_id: '{product}') \
         belongs to another company.\n\
         To avoid a mess, no company crossover is allowed!"
    )
}

/// Company consistency of a lot and its product (48`,
/// `_check_company_auto` and `check_company` on `product_id`): a lot of one company cannot be created
/// with, or moved to, a product restricted to another company; a shared product is accepted. No spec
/// case covers this rule yet, so the test carries no tag.
#[tokio::test]
async fn lot_product_must_share_the_lot_company() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let foreign = restricted_product(&mut tx, f.other_company, "Foreign Widget").await;
    // create: refused
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .with_company(f.company)?
                .records("stock.lot")?
                .create(
                    tx,
                    vec![
                        RawVals::new()
                            .set_cell("name", text("SN-X"))
                            .set_cell("product_id", Cell::Id(foreign))
                            .set_cell("company_id", Cell::Id(f.company)),
                    ],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
    assert_eq!(error.message(), crossover("SN-X", "Foreign Widget"));
    // the shared fixture product: accepted
    let shared = lot(&mut tx, f.product, Some(f.company), "SN-S").await;
    // write to the restricted product: refused, the lot keeps its product
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .with_company(f.company)?
                .records("stock.lot")?
                .browse([shared])?
                .write(tx, RawVals::new().set_cell("product_id", Cell::Id(foreign)))
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError, "{error:?}");
    assert_eq!(error.message(), crossover("SN-S", "Foreign Widget"));
    let product: i64 = tx
        .query("SELECT product_id FROM stock_lot WHERE id = $1", &[&shared])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(product, f.product);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// One target of "Other Company" for every company-checked link of a quant or serial position:
/// `(field, label, target id, target display name)`.
async fn foreign_targets(
    f: &Fixture,
    tx: &mut Transaction,
) -> Vec<(&'static str, &'static str, i64, &'static str)> {
    let product = restricted_product(tx, f.other_company, "Foreign Widget").await;
    let lot = lot(tx, f.product, Some(f.other_company), "LOT-B").await;
    let location = create(
        tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Shelf B"))
            .set_cell("company_id", Cell::Id(f.other_company)),
    )
    .await;
    let owner = create(
        tx,
        "res.partner",
        RawVals::new()
            .set_cell("name", text("Foreign Partner"))
            .set_cell("company_id", Cell::Id(f.other_company)),
    )
    .await;
    // a user allowed in "Other Company" only (res.users is scoped by company_ids)
    let user: i64 = tx
        .query(
            "INSERT INTO res_users (name, login, active, share, company_id) \
             VALUES ('Foreign User', 'foreign-user', true, false, $1) RETURNING id",
            &[&f.other_company],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.execute(
        "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
        &[&user, &f.other_company],
    )
    .await
    .unwrap();
    vec![
        ("product_id", "Product", product, "Foreign Widget"),
        ("lot_id", "Lot/Serial Number", lot, "LOT-B"),
        ("location_id", "Location", location, "Shelf B"),
        ("owner_id", "Owner", owner, "Foreign Partner"),
        ("user_id", "Assigned To", user, "Foreign User"),
    ]
}

/// Assert `error` is the company-crossover refusal naming `field` and its target.
fn assert_crossover(error: &Error, field: &str, label: &str, value: &str) {
    assert_eq!(error.kind(), ErrorKind::UserError, "{field}: {error:?}");
    let message = error.message();
    assert!(
        message.starts_with("Uh-oh! You’ve got some company inconsistencies here:\n- “"),
        "{field}: {message}"
    );
    assert!(
        message.contains(&format!(
            "belongs to company “Main Company” while “{label}” ({field}: '{value}') belongs to \
             another company.\n"
        )),
        "{field}: {message}"
    );
    assert!(
        message.ends_with("To avoid a mess, no company crossover is allowed!"),
        "{field}: {message}"
    );
}

/// Company consistency of the quant and serial position links (68,77`
/// declare it on product, lot and owner; the location is consistent there because the quant company is the
/// location's): a row of "Main Company" is refused with a product, lot, location, owner or assigned user of
/// "Other Company", and accepted with shared ones and a lot of its own company. No spec case covers the quant
/// links yet, so the test carries no tag.
#[tokio::test]
async fn quant_and_serial_position_links_must_share_the_row_company() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let own_lot = lot(&mut tx, f.product, Some(f.company), "LOT-A").await;
    let quant_vals = || {
        RawVals::new()
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("product_id", Cell::Id(f.product))
            .set_cell("location_id", Cell::Id(f.location))
            .set_cell("lot_id", Cell::Id(own_lot))
            .set_cell("owner_id", Cell::Id(f.partner))
            .set_cell("quantity", Cell::Decimal(Decimal::ONE))
    };
    let position_vals = || {
        RawVals::new()
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("product_id", Cell::Id(f.product))
            .set_cell("lot_id", Cell::Id(own_lot))
            .set_cell("location_id", Cell::Id(f.location))
            .set_cell("quantity", Cell::Decimal(Decimal::ONE))
    };
    for (field, label, target, value) in foreign_targets(&f, &mut tx).await {
        // the lot belongs to the row's product (composite key, S1): a foreign
        // product comes with a lot of its own in the product's company (a shared lot of a company
        // product is refused at its create), or the database refuses the row before the company
        // check runs
        let lot_id = if field == "product_id" {
            lot(&mut tx, target, Some(f.other_company), "LOT-F").await
        } else {
            own_lot
        };
        let error = tx
            .savepoint(async |tx| {
                tx.env()
                    .with_company(f.company)?
                    .records("stock.quant")?
                    .create(
                        tx,
                        vec![
                            quant_vals()
                                .set_cell("lot_id", Cell::Id(lot_id))
                                .set_cell(field, Cell::Id(target)),
                        ],
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_crossover(&error, field, label, value);
        if matches!(field, "product_id" | "lot_id" | "location_id") {
            let error = tx
                .savepoint(async |tx| {
                    tx.env()
                        .with_company(f.company)?
                        .records("stock.serial.position")?
                        .create(
                            tx,
                            vec![
                                position_vals()
                                    .set_cell("lot_id", Cell::Id(lot_id))
                                    .set_cell(field, Cell::Id(target)),
                            ],
                        )
                        .await
                })
                .await
                .unwrap_err();
            // the serial position's lot field has no label of its own
            let label = if field == "lot_id" { "Lot" } else { label };
            assert_crossover(&error, field, label, value);
        }
    }
    // shared product, location and owner, and a lot of the row's company: accepted
    let env = tx.env().with_company(f.company).unwrap();
    env.records("stock.quant")
        .unwrap()
        .create(&mut tx, vec![quant_vals()])
        .await
        .unwrap();
    env.records("stock.serial.position")
        .unwrap()
        .create(&mut tx, vec![position_vals()])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

const SCOPE_INSERT: &str = "INSERT INTO stock_lot_name_scope (product_id, name, global_count, company_count) \
    VALUES ($1, $2, $3, $4)";

/// a second shared lot, and one scope row exists per product and name; the lot write statement that
/// maintains the counts is's)
#[tokio::test]
async fn lot_name_scope_rule() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let name = "unique";
    let rule = "A lot without a company cannot share its product and name with another lot.";
    for (global, company) in [(1_i32, 1_i32), (2, 0)] {
        let error = refused(
            &mut tx,
            SCOPE_INSERT,
            &[&f.product, &name, &global, &company],
        )
        .await;
        assert_violation(&error, "23514", "stock_lot_name_scope_rule", rule);
    }
    // a negative count would pass the rule check; its own check refuses it
    let error = refused(&mut tx, SCOPE_INSERT, &[&f.product, &name, &1_i32, &-1_i32]).await;
    assert_violation(
        &error,
        "23514",
        "stock_lot_name_scope_counts_nonneg",
        "Lot name counts cannot be negative.",
    );
    // the accepted states, kept at zero counts
    for (product, global, company) in [(f.product, 0_i32, 3_i32), (f.other_product, 1, 0)] {
        tx.execute(SCOPE_INSERT, &[&product, &name, &global, &company])
            .await
            .unwrap();
    }
    let error = refused(&mut tx, SCOPE_INSERT, &[&f.product, &name, &0_i32, &0_i32]).await;
    assert_violation(
        &error,
        "23505",
        "stock_lot_name_scope_key",
        "A lot name has one scope row per product.",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn package_parent_path_and_children() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let a = create(
        &mut tx,
        "stock.package",
        RawVals::new().set_cell("name", text("PALLET-1")),
    )
    .await;
    let b = create(
        &mut tx,
        "stock.package",
        RawVals::new().set_cell("name", text("BOX-001")),
    )
    .await;
    let packages = tx.env().records("stock.package").unwrap();
    packages
        .browse([b])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("parent_package_id", Cell::Id(a)),
        )
        .await
        .unwrap();
    let path = |cell: Cell| match cell {
        Cell::Str(s) => s.to_string(),
        other => panic!("parent_path is text, got {other:?}"),
    };
    let a_path = path(
        packages
            .browse([a])
            .unwrap()
            .cell(&mut tx, "parent_path")
            .await
            .unwrap(),
    );
    let b_path = path(
        packages
            .browse([b])
            .unwrap()
            .cell(&mut tx, "parent_path")
            .await
            .unwrap(),
    );
    assert_eq!(b_path, format!("{a_path}{b}/"));
    let children = packages
        .browse([a])
        .unwrap()
        .cell(&mut tx, "child_package_ids")
        .await
        .unwrap();
    let Cell::Ids(children) = children else {
        panic!("child_package_ids is an id list, got {children:?}")
    };
    let children: Vec<i64> = children.iter().map(|id| id.get()).collect();
    assert_eq!(children, [b]);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// fallback and the sequence default are's)
#[tokio::test]
async fn package_keeps_explicit_name_and_numeric_weight() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let id = create(
        &mut tx,
        "stock.package",
        RawVals::new()
            .set_cell("name", text("BOX-001"))
            .set_cell("shipping_weight", Cell::Decimal(Decimal::new(125, 2))),
    )
    .await;
    let row = tx
        .query(
            "SELECT name, shipping_weight::text FROM stock_package WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    let got: (String, String) = (row[0].get(0), row[0].get(1));
    assert_eq!(got, ("BOX-001".to_owned(), "1.25".to_owned()));
    assert_eq!(
        columns(
            &mut tx,
            "stock_package",
            &[
                "name",
                "package_type_id",
                "parent_package_id",
                "package_dest_id",
                "parent_path",
                "shipping_weight",
                "pack_date"
            ]
        )
        .await,
        expect(&[
            ("name", "character varying", false),
            ("package_type_id", "bigint", true),
            ("parent_package_id", "bigint", true),
            ("package_dest_id", "bigint", true),
            ("parent_path", "character varying", true),
            ("shipping_weight", "numeric", true),
            ("pack_date", "date", true),
        ])
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The message the database error mapper gives a `23503` on `constraint` of `model` (described
/// `description`): the mapper reads every foreign-key violation as a blocked delete and does not
/// use a declared constraint's message (`crates/erp-orm/src/dberror.rs`, the
/// `FOREIGN_KEY_VIOLATION` branch).
fn foreign_key_message(description: &str, model: &str, constraint: &str) -> String {
    format!(
        "The operation cannot be completed: Another model is using the record you are trying to \
         delete.\n\nThe troublemaker is: '{description}' ({model})\nThanks to the following \
         constraint: '{constraint}'\nHow about archiving the record instead?"
    )
}

fn assert_foreign_key(error: &Error, description: &str, model: &str, constraint: &str) {
    assert_eq!(error.sqlstate(), Some("23503"), "{error:?}");
    assert_eq!(error.constraint(), Some(constraint), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        foreign_key_message(description, model, constraint)
    );
}

const LOT_QUANT_INSERT: &str = "INSERT INTO stock_quant \
    (company_id, product_id, location_id, lot_id, quantity, reserved_quantity, in_date) \
    VALUES ($1, $2, $3, $4, 1, 0, now())";

/// is mapped by a later slice)
///
/// The composite key `(lot_id, product_id) → stock_lot (id, product_id)` of the quant and the serial
/// position (S1): a lot of another product is refused on both, the lot of the
/// row's product is accepted, an untracked quant (no lot) is accepted, and a lot's product cannot
/// change while a quant or position names it.
#[tokio::test]
async fn quant_and_serial_position_lot_must_belong_to_the_row_product() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    // a lot of the second product
    let foreign = lot(&mut tx, f.other_product, Some(f.company), "LOT-G").await;
    let error = refused(
        &mut tx,
        LOT_QUANT_INSERT,
        &[&f.company, &f.product, &f.location, &Some(foreign)],
    )
    .await;
    assert_foreign_key(&error, "Quants", "stock.quant", "stock_quant_lot_product");
    let error = refused(
        &mut tx,
        POSITION_INSERT,
        &[&f.company, &f.product, &foreign, &f.location, &"1"],
    )
    .await;
    assert_foreign_key(
        &error,
        "Serial Number Position",
        "stock.serial.position",
        "stock_serial_position_lot_product",
    );
    let stored: i64 = tx
        .query(
            "SELECT (SELECT count(*) FROM stock_quant) + (SELECT count(*) FROM stock_serial_position)",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, 0, "no refused row is stored");
    // the lot of the row's own product, and no lot at all: accepted
    let own = lot(&mut tx, f.product, Some(f.company), "LOT-W").await;
    tx.execute(
        LOT_QUANT_INSERT,
        &[&f.company, &f.product, &f.location, &Some(own)],
    )
    .await
    .unwrap();
    tx.execute(
        LOT_QUANT_INSERT,
        &[&f.company, &f.product, &f.location, &None::<i64>],
    )
    .await
    .unwrap();
    tx.execute(
        POSITION_INSERT,
        &[&f.company, &f.product, &own, &f.location, &"1"],
    )
    .await
    .unwrap();
    // the lot's product cannot change while a quant and a position name it; which of the two keys
    // PostgreSQL checks first is its trigger order, so either refusal is accepted, each in full
    let error = refused(
        &mut tx,
        "UPDATE stock_lot SET product_id = $1 WHERE id = $2",
        &[&f.other_product, &own],
    )
    .await;
    match error.constraint() {
        Some("stock_serial_position_lot_product") => assert_foreign_key(
            &error,
            "Serial Number Position",
            "stock.serial.position",
            "stock_serial_position_lot_product",
        ),
        _ => assert_foreign_key(&error, "Quants", "stock.quant", "stock_quant_lot_product"),
    }
    // nor while only a serial position names it
    tx.execute("DELETE FROM stock_quant WHERE lot_id = $1", &[&own])
        .await
        .unwrap();
    let error = refused(
        &mut tx,
        "UPDATE stock_lot SET product_id = $1 WHERE id = $2",
        &[&f.other_product, &own],
    )
    .await;
    assert_foreign_key(
        &error,
        "Serial Number Position",
        "stock.serial.position",
        "stock_serial_position_lot_product",
    );
    // and only a quant
    tx.execute(
        "DELETE FROM stock_serial_position WHERE lot_id = $1",
        &[&own],
    )
    .await
    .unwrap();
    tx.execute(
        LOT_QUANT_INSERT,
        &[&f.company, &f.product, &f.other_location, &Some(own)],
    )
    .await
    .unwrap();
    let error = refused(
        &mut tx,
        "UPDATE stock_lot SET product_id = $1 WHERE id = $2",
        &[&f.other_product, &own],
    )
    .await;
    assert_foreign_key(&error, "Quants", "stock.quant", "stock_quant_lot_product");
    let product: i64 = tx
        .query("SELECT product_id FROM stock_lot WHERE id = $1", &[&own])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(product, f.product, "the lot keeps its product");
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// `(name, oid, definition)` of the lot-product key constraints and the single-column lot foreign
/// keys, by name.
async fn lot_product_constraints(tx: &mut Transaction) -> Vec<(String, u32, String)> {
    let names = vec![
        "stock_lot_id_product",
        "stock_quant_lot_product",
        "stock_serial_position_lot_product",
        "stock_quant_lot_id_fkey",
        "stock_serial_position_lot_id_fkey",
    ];
    tx.query(
        "SELECT k.conname::text, k.oid, pg_get_constraintdef(k.oid) FROM pg_constraint k \
         JOIN pg_namespace n ON n.oid = k.connamespace \
         WHERE n.nspname = current_schema() AND k.conname::text = ANY($1) ORDER BY k.conname",
        &[&names],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1), r.get(2)))
    .collect()
}

/// The composite lot-product key on a fresh install and on tables that exist without it
/// (S1, verify items 2 and 3): the lot's `UNIQUE (id, product_id)` is created
/// before the two foreign keys that reference it (else adding them fails), the many2one foreign-key
/// reconcile keeps the single-column `lot_id` keys beside the composite one, and a second
/// synchronization changes nothing (same constraint oids).
#[tokio::test]
async fn lot_product_key_is_synchronized_and_stable() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let want: Vec<(String, String)> = [
        ("stock_lot_id_product", "UNIQUE (id, product_id)"),
        (
            "stock_quant_lot_id_fkey",
            "FOREIGN KEY (lot_id) REFERENCES stock_lot(id) ON DELETE RESTRICT",
        ),
        (
            "stock_quant_lot_product",
            "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot(id, product_id)",
        ),
        (
            "stock_serial_position_lot_id_fkey",
            "FOREIGN KEY (lot_id) REFERENCES stock_lot(id) ON DELETE RESTRICT",
        ),
        (
            "stock_serial_position_lot_product",
            "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot(id, product_id)",
        ),
    ]
    .iter()
    .map(|(n, d)| ((*n).to_owned(), (*d).to_owned()))
    .collect();
    let shape = |rows: &[(String, u32, String)]| -> Vec<(String, String)> {
        rows.iter()
            .map(|(n, _, d)| (n.clone(), d.clone()))
            .collect()
    };
    // fresh install (the harness synchronized every table from nothing)
    let fresh = lot_product_constraints(&mut tx).await;
    assert_eq!(shape(&fresh), want);
    // a second synchronization is a no-op: nothing dropped or added again
    tx.init_models(&[]).await.unwrap();
    assert_eq!(lot_product_constraints(&mut tx).await, fresh);
    // existing tables without the key (the foreign keys go first: they depend on the unique)
    tx.batch(
        "ALTER TABLE stock_quant DROP CONSTRAINT stock_quant_lot_product; \
         ALTER TABLE stock_serial_position DROP CONSTRAINT stock_serial_position_lot_product; \
         ALTER TABLE stock_lot DROP CONSTRAINT stock_lot_id_product",
    )
    .await
    .unwrap();
    let dropped = lot_product_constraints(&mut tx).await;
    assert_eq!(
        dropped.iter().map(|(n, ..)| n.as_str()).collect::<Vec<_>>(),
        [
            "stock_quant_lot_id_fkey",
            "stock_serial_position_lot_id_fkey"
        ]
    );
    tx.init_models(&[]).await.unwrap();
    let restored = lot_product_constraints(&mut tx).await;
    assert_eq!(shape(&restored), want);
    // the single-column keys were kept, not dropped and added again
    for (name, oid, _) in &dropped {
        assert!(
            restored.iter().any(|(n, o, _)| n == name && o == oid),
            "{name} kept"
        );
    }
    tx.init_models(&[]).await.unwrap();
    assert_eq!(lot_product_constraints(&mut tx).await, restored);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
