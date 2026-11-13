// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema of the move line tables: the operation
//! line, its serial child rows and its correction rows. Schema only: rows are written with plain SQL (or
//! plain ORM creates), because the statements that maintain open lines, children and corrections are
//! later slices.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_addon_uom::stock::product_quantity_sql;
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_stock_lines", depends = ["stock"]);

/// A test stand-in for `ir.model.data` with the columns the product addon and the company create path
/// read (the real model lives in `erp-data`, out of reach of an addon crate; the product tests use the
/// same stand-in).
#[model(
    name = "ir.model.data",
    description = "Model Data",
    addon = "test_stock_lines"
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

/// The prefix the ORM puts before a declared constraint's message.
const PREFIX: &str = "The operation cannot be completed: ";

const LINE: &str = "stock.move.line";
const SERIAL: &str = "stock.move.line.serial";
const CORRECTION: &str = "stock.move.line.correction";

/// Ids every case starts from: two companies, two shared products with a lot each (and a second lot of
/// the first), a unit, two shared locations, a shared partner, one move per company of the first
/// product and a second move of the main company.
struct Fixture {
    h: Harness,
    company: i64,
    other_company: i64,
    product: i64,
    other_product: i64,
    unit: i64,
    location: i64,
    other_location: i64,
    partner: i64,
    lot: i64,
    second_lot: i64,
    other_product_lot: i64,
    mv: i64,
    second_move: i64,
    foreign_move: i64,
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

fn dec(value: &str) -> Cell {
    Cell::Decimal(Decimal::from_str_exact(value).unwrap())
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

/// The variant of a new shared product template.
async fn shared_product(tx: &mut Transaction, company: i64, name: &str) -> i64 {
    let template = tx
        .env()
        .with_company(company)
        .unwrap()
        .records("product.template")
        .unwrap()
        .create(tx, vec![RawVals::new().set_cell("name", text(name))])
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

/// The values of a move of `company` through the ORM, with consistent product-unit columns.
fn move_vals(company: i64, product: i64, unit: i64, location: i64, dest: i64) -> RawVals {
    RawVals::new()
        .set_cell("company_id", Cell::Id(company))
        .set_cell("product_id", Cell::Id(product))
        .set_cell("uom_id", Cell::Id(unit))
        .set_cell("location_id", Cell::Id(location))
        .set_cell("location_dest_id", Cell::Id(dest))
        .set_cell("product_qty", dec("0"))
        .set_cell("quantity_product_uom", dec("0"))
        .set_cell("uom_factor", dec("1"))
        .set_cell("product_uom_factor", dec("1"))
        .set_cell("product_unit_digits", Cell::Int(2))
}

async fn create_in(tx: &mut Transaction, company: i64, model: &str, vals: RawVals) -> Result<i64> {
    tx.savepoint(async |tx| {
        Ok(tx
            .env()
            .with_company(company)?
            .records(model)?
            .create(tx, vec![vals])
            .await?
            .rec_ids()[0]
            .get())
    })
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
    let uid = tx.env().uid();
    for cid in [company, other_company] {
        tx.execute(
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&uid, &cid],
        )
        .await
        .unwrap();
    }
    let product = shared_product(&mut tx, company, "Widget").await;
    let other_product = shared_product(&mut tx, company, "Gadget").await;
    let location = create(
        &mut tx,
        "stock.location",
        RawVals::new().set_cell("name", text("Stock")),
    )
    .await;
    let other_location = create(
        &mut tx,
        "stock.location",
        RawVals::new().set_cell("name", text("Customers")),
    )
    .await;
    let partner = create(
        &mut tx,
        "res.partner",
        RawVals::new().set_cell("name", text("Consignor")),
    )
    .await;
    let mut lots = Vec::new();
    for (p, name) in [
        (product, "SN-1"),
        (product, "SN-2"),
        (other_product, "SN-G"),
    ] {
        lots.push(
            create(
                &mut tx,
                "stock.lot",
                RawVals::new()
                    .set_cell("name", text(name))
                    .set_cell("product_id", Cell::Id(p))
                    .set_cell("company_id", Cell::Id(company)),
            )
            .await,
        );
    }
    let mut moves = Vec::new();
    for c in [company, company, other_company] {
        moves.push(
            create_in(
                &mut tx,
                c,
                "stock.move",
                move_vals(c, product, unit, location, other_location),
            )
            .await
            .unwrap(),
        );
    }
    tx.commit().await.unwrap();
    Fixture {
        h,
        company,
        other_company,
        product,
        other_product,
        unit,
        location,
        other_location,
        partner,
        lot: lots[0],
        second_lot: lots[1],
        other_product_lot: lots[2],
        mv: moves[0],
        second_move: moves[1],
        foreign_move: moves[2],
    }
}

/// An `INSERT … RETURNING id` of `table` with `defaults`, each replaced by an entry of `over` of the same
/// column or extended by it. Values are SQL literals (test values only).
fn insert_sql(table: &str, defaults: Vec<(&str, String)>, over: &[(&str, &str)]) -> String {
    let mut cols = defaults;
    for (c, v) in over {
        match cols.iter_mut().find(|(n, _)| n == c) {
            Some(slot) => slot.1 = (*v).to_owned(),
            None => cols.push((c, (*v).to_owned())),
        }
    }
    let names: Vec<&str> = cols.iter().map(|(n, _)| *n).collect();
    let values: Vec<&str> = cols.iter().map(|(_, v)| v.as_str()).collect();
    format!(
        "INSERT INTO {table} ({}) VALUES ({}) RETURNING id",
        names.join(", "),
        values.join(", ")
    )
}

/// A consistent fresh open plain line of `mv` (the fixture's product, unit and locations, quantity 0).
fn line_sql(f: &Fixture, mv: i64, over: &[(&str, &str)]) -> String {
    insert_sql(
        "stock_move_line",
        vec![
            ("company_id", f.company.to_string()),
            ("product_id", f.product.to_string()),
            ("move_id", mv.to_string()),
            ("uom_id", f.unit.to_string()),
            ("location_id", f.location.to_string()),
            ("location_dest_id", f.other_location.to_string()),
            ("quantity", "0".into()),
            ("quantity_product_uom", "0".into()),
            ("uom_factor", "1".into()),
            ("product_uom_factor", "1".into()),
            ("product_unit_digits", "2".into()),
            ("membership", "'open'".into()),
            ("active", "true".into()),
            ("picked", "false".into()),
            ("correction_seq", "0".into()),
            ("child_mode", "false".into()),
        ],
        over,
    )
}

/// An active child of `line` on `mv` that follows its line.
fn child_sql(line: i64, mv: i64, over: &[(&str, &str)]) -> String {
    insert_sql(
        "stock_move_line_serial",
        vec![
            ("line_id", line.to_string()),
            ("move_id", mv.to_string()),
            ("active", "true".into()),
        ],
        over,
    )
}

/// A line-scope `edit` row `seq` 1 of `line` (the fixture's company and product).
fn correction_sql(f: &Fixture, line: i64, over: &[(&str, &str)]) -> String {
    insert_sql(
        "stock_move_line_correction",
        vec![
            ("company_id", f.company.to_string()),
            ("product_id", f.product.to_string()),
            ("move_line_id", line.to_string()),
            ("seq", "1".into()),
            ("kind", "'edit'".into()),
            ("effective_at", "now()".into()),
            ("recorded_at", "now()".into()),
        ],
        over,
    )
}

/// Run one insert in a savepoint and return its id.
async fn insert(tx: &mut Transaction, sql: &str) -> Result<i64> {
    tx.savepoint(async |tx| Ok(tx.query(sql, &[]).await?[0].get::<_, i64>(0)))
        .await
}

async fn refused(tx: &mut Transaction, sql: &str) -> Error {
    insert(tx, sql).await.expect_err(sql)
}

/// Assert a check or unique refusal with the declared message.
fn assert_violation(error: &Error, sqlstate: &str, constraint: &str, message: &str) {
    assert_eq!(error.sqlstate(), Some(sqlstate), "{error:?}");
    assert_eq!(error.constraint(), Some(constraint), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        format!("{PREFIX}{message}"),
        "{constraint}"
    );
}

/// Assert a refusal by the foreign key `constraint` of `model`'s table. The ORM's mapper uses its
/// generic blocked-delete text for every foreign-key violation (`crates/erp-orm/src/dberror.rs`), so the
/// text names the model of the table holding the key; the D5.4 typed errors are's.
fn assert_foreign_key(f: &Fixture, error: &Error, model: &str, constraint: &str) {
    let description = f.h.registry().model(model).unwrap().description.clone();
    assert_eq!(error.sqlstate(), Some("23503"), "{error:?}");
    assert_eq!(error.constraint(), Some(constraint), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "{PREFIX}Another model is using the record you are trying to delete.\n\nThe troublemaker \
             is: '{description}' ({model})\nThanks to the following constraint: '{constraint}'\nHow \
             about archiving the record instead?"
        )
    );
}

/// The declared message of table object `name` (as declared, `_…`) of `model` (the model's own text);
/// the registry names it with its table prefix.
fn message(f: &Fixture, model: &str, name: &str) -> &'static str {
    let spec = f.h.registry().model(model).unwrap();
    let full = format!("{}{name}", spec.table);
    match spec
        .table_object(&full)
        .unwrap_or_else(|| panic!("{model} declares {full}"))
        .kind
    {
        erp_sdk::__private::TableObjectKind::Constraint { message, .. }
        | erp_sdk::__private::TableObjectKind::UniqueIndex { message, .. } => message,
        erp_sdk::__private::TableObjectKind::Key(key) => key.message,
        ref other => panic!("{name}: no message on {other:?}"),
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
/// database), and whether it is unique.
async fn index_definition(tx: &mut Transaction, name: &str) -> (bool, String) {
    let rows = tx
        .query(
            "SELECT i.indisunique, pg_get_indexdef(c.oid) FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace JOIN pg_index i ON i.indexrelid = c.oid \
             WHERE n.nspname = current_schema() AND c.relname = $1",
            &[&name],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "index {name} exists");
    let def: String = rows[0].get(1);
    (
        rows[0].get(0),
        def.split_once(" USING ")
            .expect("an index definition")
            .1
            .to_owned(),
    )
}

/// PostgreSQL's own rendering of an index on `table` written as `body`: a probe index is created, read
/// and dropped again, so the plan's definition is compared in the catalog's normal form. Run before any
/// row exists, so a unique probe cannot fail on data.
async fn probe_definition(
    tx: &mut Transaction,
    table: &str,
    body: &str,
    unique: bool,
) -> (bool, String) {
    let probe = format!("lines_probe_{table}");
    let kind = if unique { "UNIQUE INDEX" } else { "INDEX" };
    tx.batch(&format!("CREATE {kind} {probe} ON {table} {body}"))
        .await
        .unwrap();
    let out = index_definition(tx, &probe).await;
    tx.batch(&format!("DROP INDEX {probe}")).await.unwrap();
    out
}

/// Assert the field index of `table.column` is a plain btree, or a btree over non-null values.
async fn assert_btree(tx: &mut Transaction, table: &str, column: &str, not_null: bool) {
    let body = if not_null {
        format!("({column}) WHERE {column} IS NOT NULL")
    } else {
        format!("({column})")
    };
    let expected = probe_definition(tx, table, &body, false).await;
    assert_eq!(
        index_definition(tx, &format!("{table}__{column}_index")).await,
        expected,
        "{table}.{column}"
    );
}

const OPEN_KEY: &str = "(move_id, product_id, location_id, location_dest_id, lot_id, lot_name, package_id, \
     result_package_id, owner_id, uom_id, picked) NULLS NOT DISTINCT \
     WHERE membership = 'open' AND done_at IS NULL";

/// group-by through the move are's)
/// The line columns, readonly flags, ORM defaults and indexes: the unique open-line key and the
/// reservation index with the plan's predicates, field btrees.
#[tokio::test]
async fn line_columns_defaults_and_indexes() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let probe_key = probe_definition(&mut tx, "stock_move_line", OPEN_KEY, true).await;
    let probe_reservation = probe_definition(
        &mut tx,
        "stock_move_line",
        "(company_id, product_id, location_id, lot_id, package_id, owner_id) \
         WHERE membership = 'open' AND active",
        false,
    )
    .await;
    assert_eq!(
        index_definition(&mut tx, "stock_move_line_open_key").await,
        probe_key
    );
    assert!(probe_key.0, "the open-line key is unique");
    assert_eq!(
        index_definition(&mut tx, "stock_move_line_reservation").await,
        probe_reservation
    );
    let spec_cols = [
        ("move_id", "bigint", false),
        ("picking_id", "bigint", true),
        ("company_id", "bigint", false),
        ("product_id", "bigint", false),
        ("uom_id", "bigint", false),
        ("quantity", "numeric", false),
        ("quantity_product_uom", "numeric", false),
        ("uom_factor", "numeric", false),
        ("product_uom_factor", "numeric", false),
        ("product_unit_digits", "integer", false),
        ("picked", "boolean", false),
        ("package_id", "bigint", true),
        ("lot_id", "bigint", true),
        ("lot_name", "character varying", true),
        ("result_package_id", "bigint", true),
        ("owner_id", "bigint", true),
        ("location_id", "bigint", false),
        ("location_dest_id", "bigint", false),
        ("is_entire_pack", "boolean", true),
        ("membership", "character varying", false),
        ("active", "boolean", false),
        ("done_at", "timestamp without time zone", true),
        ("correction_seq", "integer", false),
        ("child_mode", "boolean", false),
    ];
    let names: Vec<&str> = spec_cols.iter().map(|c| c.0).collect();
    assert_eq!(
        columns(&mut tx, "stock_move_line", &names).await,
        expect(&spec_cols)
    );
    // no stored copies of the move's state or date (5)
    let copies: i64 = tx
        .query(
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = current_schema() \
             AND table_name = 'stock_move_line' AND column_name IN ('state', 'date')",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(copies, 0);
    let spec = f.h.registry().model(LINE).unwrap();
    for field in [
        "company_id",
        "membership",
        "done_at",
        "correction_seq",
        "child_mode",
        "quantity_product_uom",
        "uom_factor",
        "product_uom_factor",
        "product_unit_digits",
    ] {
        assert!(spec.field(field).unwrap().readonly, "{field} is readonly");
    }
    for field in ["quantity_product_uom", "done_at"] {
        assert!(spec.field(field).unwrap().default.is_none(), "{field}");
    }
    let id = create_in(
        &mut tx,
        f.company,
        LINE,
        RawVals::new()
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("product_id", Cell::Id(f.product))
            .set_cell("move_id", Cell::Id(f.mv))
            .set_cell("uom_id", Cell::Id(f.unit))
            .set_cell("location_id", Cell::Id(f.location))
            .set_cell("location_dest_id", Cell::Id(f.other_location))
            .set_cell("quantity_product_uom", dec("0"))
            .set_cell("uom_factor", dec("1"))
            .set_cell("product_uom_factor", dec("1"))
            .set_cell("product_unit_digits", Cell::Int(2)),
    )
    .await
    .unwrap();
    let row = tx
        .query(
            "SELECT membership, active::text, picked::text, correction_seq::text, child_mode::text, \
             quantity::text, (done_at IS NULL)::text FROM stock_move_line WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    let got: Vec<String> = (0..7).map(|i| row[0].get(i)).collect();
    assert_eq!(got, ["open", "true", "false", "0", "false", "0", "true"]);
    for (field, not_null) in [
        ("move_id", false),
        ("picking_id", false),
        ("company_id", false),
        ("product_id", false),
        ("location_id", false),
        ("location_dest_id", false),
        ("uom_id", false),
        ("lot_id", true),
        ("package_id", true),
        ("result_package_id", true),
        ("owner_id", true),
    ] {
        assert_btree(&mut tx, "stock_move_line", field, not_null).await;
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The child and correction columns, their indexes (the two active-child lot indexes with the plan's
/// predicates) and the correction readonly-free shape.
#[tokio::test]
async fn serial_and_correction_columns_and_indexes() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    for (name, body) in [
        (
            "stock_move_line_serial_move_lot",
            "(move_id, lot_id) WHERE active AND lot_id IS NOT NULL",
        ),
        (
            "stock_move_line_serial_move_lot_name",
            "(move_id, lot_name) WHERE active AND lot_id IS NULL AND lot_name IS NOT NULL",
        ),
    ] {
        let expected = probe_definition(&mut tx, "stock_move_line_serial", body, true).await;
        assert_eq!(index_definition(&mut tx, name).await, expected, "{name}");
    }
    let serial_cols = [
        ("line_id", "bigint", false),
        ("move_id", "bigint", false),
        ("lot_id", "bigint", true),
        ("lot_name", "character varying", true),
        ("active", "boolean", false),
        ("location_id", "bigint", true),
        ("location_dest_id", "bigint", true),
        ("package_id", "bigint", true),
        ("result_package_id", "bigint", true),
        ("owner_id", "bigint", true),
    ];
    let names: Vec<&str> = serial_cols.iter().map(|c| c.0).collect();
    assert_eq!(
        columns(&mut tx, "stock_move_line_serial", &names).await,
        expect(&serial_cols)
    );
    let correction_cols = [
        ("company_id", "bigint", false),
        ("product_id", "bigint", false),
        ("move_line_id", "bigint", false),
        ("serial_child_id", "bigint", true),
        ("seq", "integer", false),
        ("kind", "character varying", false),
        ("old_quantity", "numeric", true),
        ("new_quantity", "numeric", true),
        ("old_location_id", "bigint", true),
        ("new_location_id", "bigint", true),
        ("old_location_dest_id", "bigint", true),
        ("new_location_dest_id", "bigint", true),
        ("old_lot_id", "bigint", true),
        ("new_lot_id", "bigint", true),
        ("old_package_id", "bigint", true),
        ("new_package_id", "bigint", true),
        ("old_result_package_id", "bigint", true),
        ("new_result_package_id", "bigint", true),
        ("old_owner_id", "bigint", true),
        ("new_owner_id", "bigint", true),
        ("quantity_delta_old_key", "numeric", true),
        ("quantity_delta_new_key", "numeric", true),
        ("effective_at", "timestamp without time zone", false),
        ("recorded_at", "timestamp without time zone", false),
        ("cause", "character varying", true),
    ];
    let names: Vec<&str> = correction_cols.iter().map(|c| c.0).collect();
    assert_eq!(
        columns(&mut tx, "stock_move_line_correction", &names).await,
        expect(&correction_cols)
    );
    // narrow rows: no log columns on either table
    let log: i64 = tx
        .query(
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = current_schema() \
             AND table_name IN ('stock_move_line_serial', 'stock_move_line_correction') \
             AND column_name IN ('create_uid', 'create_date', 'write_uid', 'write_date')",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(log, 0);
    for (field, not_null) in [
        ("line_id", false),
        ("move_id", false),
        ("lot_id", true),
        ("location_id", true),
        ("location_dest_id", true),
        ("package_id", true),
        ("result_package_id", true),
        ("owner_id", true),
    ] {
        assert_btree(&mut tx, "stock_move_line_serial", field, not_null).await;
    }
    for (field, not_null) in [("move_line_id", false), ("serial_child_id", true)] {
        assert_btree(&mut tx, "stock_move_line_correction", field, not_null).await;
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// ORM, and never collide; the logical-row read is's)
/// Both tables draw their id from `stock_move_line_logical_id_seq` (3).
#[tokio::test]
async fn lines_and_children_share_one_id_sequence() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    for table in ["stock_move_line", "stock_move_line_serial"] {
        let default: String = tx
            .query(
                "SELECT column_default FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = $1 AND column_name = 'id'",
                &[&table],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            default, "nextval('stock_move_line_logical_id_seq'::regclass)",
            "{table}"
        );
    }
    let mut ids = Vec::new();
    let line = insert(&mut tx, &line_sql(&f, f.mv, &[("child_mode", "true")]))
        .await
        .unwrap();
    ids.push(line);
    for _ in 0..3 {
        ids.push(insert(&mut tx, &child_sql(line, f.mv, &[])).await.unwrap());
    }
    ids.push(
        insert(&mut tx, &line_sql(&f, f.second_move, &[]))
            .await
            .unwrap(),
    );
    // the ORM's create of a child draws from the same sequence
    ids.push(
        create_in(
            &mut tx,
            f.company,
            SERIAL,
            RawVals::new()
                .set_cell("line_id", Cell::Id(line))
                .set_cell("move_id", Cell::Id(f.mv)),
        )
        .await
        .unwrap(),
    );
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, ids, "ids are distinct and follow the insert order");
    let both: i64 = tx
        .query(
            "SELECT count(*) FROM stock_move_line l JOIN stock_move_line_serial s ON s.id = l.id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(both, 0, "no id names a line and a child");
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// the ORM's prefix; the ORM-level check is's)
/// The line's checks refuse each violation with their own constraint and accept the boundaries: a
/// zero quantity, a reopened row deactivated with quantity zero, a done line with its done time.
#[tokio::test]
async fn line_checks_refuse_and_accept_boundaries() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let lot = f.lot.to_string();
    let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
        (
            vec![("quantity", "-1"), ("quantity_product_uom", "-1")],
            "_quantity_nonneg",
        ),
        (vec![("quantity", "'NaN'")], "_finite_quantity"),
        (
            vec![("quantity_product_uom", "'Infinity'")],
            "_finite_quantity_product_uom",
        ),
        (vec![("uom_factor", "'NaN'")], "_finite_uom_factor"),
        (
            vec![("product_uom_factor", "'-Infinity'")],
            "_finite_product_uom_factor",
        ),
        (vec![("uom_factor", "0")], "_factor_nonzero"),
        (vec![("product_unit_digits", "29")], "_digits_range"),
        (vec![("product_unit_digits", "-1")], "_digits_range"),
        (vec![("correction_seq", "-1")], "_correction_seq_nonneg"),
        (vec![("membership", "'done'")], "_done_has_done_at"),
        (vec![("active", "false")], "_inactive_reopened"),
        (
            vec![
                ("active", "false"),
                ("membership", "'done'"),
                ("done_at", "now()"),
            ],
            "_inactive_reopened",
        ),
        (
            vec![
                ("active", "false"),
                ("done_at", "now()"),
                ("quantity", "1"),
                ("quantity_product_uom", "1"),
            ],
            "_inactive_reopened",
        ),
        (
            vec![("child_mode", "true"), ("lot_id", lot.as_str())],
            "_child_mode_no_lot",
        ),
        (
            vec![("child_mode", "true"), ("lot_name", "'SN-9'")],
            "_child_mode_no_lot",
        ),
        (
            vec![("quantity", "1"), ("quantity_product_uom", "2")],
            "_quantity_product_uom_conversion",
        ),
    ];
    for (over, name) in cases {
        let error = refused(&mut tx, &line_sql(&f, f.mv, &over)).await;
        assert_violation(
            &error,
            "23514",
            &format!("stock_move_line{name}"),
            message(&f, LINE, name),
        );
    }
    assert_eq!(
        message(&f, LINE, "_quantity_nonneg"),
        "You can not enter negative quantities."
    );
    // accepted: zero, digits 0 and 28, a done line with its time, a deactivated reopened row at zero,
    // a converted quantity
    for over in [
        vec![("product_unit_digits", "0")],
        vec![("product_unit_digits", "28")],
        vec![("membership", "'done'"), ("done_at", "now()")],
        vec![("active", "false"), ("done_at", "now()")],
        vec![
            ("quantity", "1.25"),
            ("quantity_product_uom", "2.50"),
            ("product_uom_factor", "0.5"),
        ],
        vec![("child_mode", "true")],
    ] {
        let id = insert(&mut tx, &line_sql(&f, f.mv, &over))
            .await
            .unwrap_or_else(|e| panic!("{over:?}: {e:?}"));
        // the fresh ones share one open-line key: each case alone
        tx.execute("DELETE FROM stock_move_line WHERE id = $1", &[&id])
            .await
            .unwrap();
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The line's conversion check is the text of the unit service's SQL builder for the line's columns
/// (as the move's is; the corpus test of `operations_schema.rs` runs that same expression against the
/// unit service), read from the registry.
#[tokio::test]
async fn line_conversion_check_is_the_unit_service_expression() {
    let f = fixture().await;
    let expr = product_quantity_sql(
        "quantity",
        "uom_factor",
        "product_uom_factor",
        "product_unit_digits",
    )
    .unwrap();
    let declared = match f
        .h
        .registry()
        .model(LINE)
        .unwrap()
        .table_object("stock_move_line_quantity_product_uom_conversion")
        .expect("declared")
        .kind
    {
        erp_sdk::__private::TableObjectKind::Constraint { definition, .. } => definition,
        ref other => panic!("a CHECK constraint, got {other:?}"),
    };
    assert_eq!(declared, format!("CHECK (quantity_product_uom = {expr})"));
    f.h.cleanup().await.unwrap();
}

/// arbitrates the `INSERT … ON CONFLICT … DO UPDATE` form, which adds to the existing line and returns
/// its id; the RPC create is's)
/// fresh line of that key; the reset unit is's)
/// index, and the reservation index's predicate reads open membership and the active flag only; the
/// reset and redo are's)
#[tokio::test]
async fn open_line_key_admits_one_fresh_line_per_key() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let fresh = insert(
        &mut tx,
        &line_sql(
            &f,
            f.mv,
            &[("quantity", "2"), ("quantity_product_uom", "2")],
        ),
    )
    .await
    .unwrap();
    // same key, NULL lot, packages and owner on both: refused (NULLS NOT DISTINCT)
    let error = refused(&mut tx, &line_sql(&f, f.mv, &[])).await;
    assert_violation(
        &error,
        "23505",
        "stock_move_line_open_key",
        message(&f, LINE, "_open_key"),
    );
    // another key part (picked, the lot, the move): accepted
    let lot = f.lot.to_string();
    let second_move = f.second_move;
    for sql in [
        line_sql(&f, f.mv, &[("picked", "true")]),
        line_sql(&f, f.mv, &[("lot_id", lot.as_str())]),
        line_sql(&f, second_move, &[]),
    ] {
        insert(&mut tx, &sql).await.unwrap();
    }
    // done and reopened lines of the same key are outside the index
    for over in [
        vec![("membership", "'done'"), ("done_at", "now()")],
        vec![("done_at", "now()")],
        vec![("done_at", "now()")],
    ] {
        insert(&mut tx, &line_sql(&f, f.mv, &over)).await.unwrap();
    }
    // with a reopened line of the key present, the fresh line is still the only one the index holds
    let error = refused(&mut tx, &line_sql(&f, f.mv, &[])).await;
    assert_eq!(error.constraint(), Some("stock_move_line_open_key"));
    // the upsert form.1 arbitrates on the index and adds to the fresh line
    let upsert = format!(
        "{} ON CONFLICT {} DO UPDATE SET quantity = stock_move_line.quantity + EXCLUDED.quantity, \
         quantity_product_uom = stock_move_line.quantity_product_uom + EXCLUDED.quantity_product_uom \
         RETURNING id, quantity::text",
        line_sql(
            &f,
            f.mv,
            &[("quantity", "3"), ("quantity_product_uom", "3")]
        )
        .trim_end_matches(" RETURNING id"),
        OPEN_KEY.replace(" NULLS NOT DISTINCT", "")
    );
    let row = tx.query(&upsert, &[]).await.unwrap();
    assert_eq!(
        (row[0].get::<_, i64>(0), row[0].get::<_, String>(1)),
        (fresh, "5".to_owned())
    );
    let open: i64 = tx
        .query(
            "SELECT count(*) FROM stock_move_line WHERE move_id = $1 AND membership = 'open' \
             AND done_at IS NULL AND NOT picked AND lot_id IS NULL",
            &[&f.mv],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(open, 1);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// connections inserting one fresh key meet on the unique open-line key, and the second fails with
/// `23505` on `stock_move_line_open_key`; the fault-injection switch and the `OpenLineKeyConflict`
/// mapping are's)
/// Two cases: the second connection's snapshot predates the first's commit; and the second inserts
/// while the first is still open and commits concurrently.
#[tokio::test]
async fn open_line_key_holds_across_two_connections() {
    let f = fixture().await;
    let sql = line_sql(&f, f.mv, &[]);
    // case A: snapshot taken before the first commit, insert after it
    let mut first = f.h.begin().await.unwrap();
    first.query(&sql, &[]).await.unwrap();
    let mut second = f.h.begin().await.unwrap();
    let before: i64 = second
        .query("SELECT count(*) FROM stock_move_line", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(before, 0, "the second snapshot predates the first commit");
    first.commit().await.unwrap();
    let error = second.query(&sql, &[]).await.unwrap_err();
    assert_eq!(error.sqlstate(), Some("23505"), "{error:?}");
    assert_eq!(error.constraint(), Some("stock_move_line_open_key"));
    second.rollback().await.unwrap();
    drop(second);
    // case B: an insert on another move's key waits on the open first transaction, which commits
    let sql = line_sql(&f, f.second_move, &[]);
    let mut first = f.h.begin().await.unwrap();
    first.query(&sql, &[]).await.unwrap();
    let mut second = f.h.begin().await.unwrap();
    second
        .query("SELECT count(*) FROM stock_move_line", &[])
        .await
        .unwrap();
    let (result, ()) = tokio::join!(second.query(&sql, &[]), async move {
        first.commit().await.unwrap();
    });
    let error = result.unwrap_err();
    assert_eq!(error.sqlstate(), Some("23505"), "{error:?}");
    assert_eq!(error.constraint(), Some("stock_move_line_open_key"));
    second.rollback().await.unwrap();
    drop(second);
    let mut check = f.h.begin().await.unwrap();
    let lines: i64 = check
        .query("SELECT count(*) FROM stock_move_line", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(lines, 2, "one line per key");
    check.rollback().await.unwrap();
    drop(check);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn serial_children_hold_each_lot_once_per_move() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let line = insert(&mut tx, &line_sql(&f, f.mv, &[("child_mode", "true")]))
        .await
        .unwrap();
    let other_line = insert(
        &mut tx,
        &line_sql(&f, f.mv, &[("child_mode", "true"), ("picked", "true")]),
    )
    .await
    .unwrap();
    let lot = f.lot.to_string();
    insert(&mut tx, &child_sql(line, f.mv, &[("lot_id", lot.as_str())]))
        .await
        .unwrap();
    // the same lot on another line of the same move: refused
    let error = refused(
        &mut tx,
        &child_sql(other_line, f.mv, &[("lot_id", lot.as_str())]),
    )
    .await;
    assert_violation(
        &error,
        "23505",
        "stock_move_line_serial_move_lot",
        message(&f, SERIAL, "_move_lot"),
    );
    // inactive, or on another move: accepted
    let second_line = insert(
        &mut tx,
        &line_sql(&f, f.second_move, &[("child_mode", "true")]),
    )
    .await
    .unwrap();
    insert(
        &mut tx,
        &child_sql(line, f.mv, &[("lot_id", lot.as_str()), ("active", "false")]),
    )
    .await
    .unwrap();
    insert(
        &mut tx,
        &child_sql(second_line, f.second_move, &[("lot_id", lot.as_str())]),
    )
    .await
    .unwrap();
    // an unresolved name, twice in one move: refused; a name beside a set lot is not unresolved
    insert(&mut tx, &child_sql(line, f.mv, &[("lot_name", "'SN-NEW'")]))
        .await
        .unwrap();
    let error = refused(
        &mut tx,
        &child_sql(other_line, f.mv, &[("lot_name", "'SN-NEW'")]),
    )
    .await;
    assert_violation(
        &error,
        "23505",
        "stock_move_line_serial_move_lot_name",
        message(&f, SERIAL, "_move_lot_name"),
    );
    let second_lot = f.second_lot.to_string();
    insert(
        &mut tx,
        &child_sql(
            line,
            f.mv,
            &[("lot_id", second_lot.as_str()), ("lot_name", "'SN-NEW'")],
        ),
    )
    .await
    .unwrap();
    // units not identified yet: any number of them
    for _ in 0..2 {
        insert(&mut tx, &child_sql(line, f.mv, &[])).await.unwrap();
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn composite_keys_hold_lines_to_their_move_lot_and_line() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let foreign_lot = f.other_product_lot.to_string();
    let error = refused(
        &mut tx,
        &line_sql(&f, f.mv, &[("lot_id", foreign_lot.as_str())]),
    )
    .await;
    assert_foreign_key(&f, &error, LINE, "stock_move_line_lot_product");
    let other_company = f.other_company.to_string();
    let other_product = f.other_product.to_string();
    for over in [
        vec![("company_id", other_company.as_str())],
        vec![("product_id", other_product.as_str())],
    ] {
        let error = refused(&mut tx, &line_sql(&f, f.mv, &over)).await;
        assert_foreign_key(&f, &error, LINE, "stock_move_line_move_company_product");
    }
    // a line of the other company's move, with the main company: refused the same way
    let error = refused(&mut tx, &line_sql(&f, f.foreign_move, &[])).await;
    assert_foreign_key(&f, &error, LINE, "stock_move_line_move_company_product");
    let line = insert(&mut tx, &line_sql(&f, f.mv, &[("child_mode", "true")]))
        .await
        .unwrap();
    let error = refused(&mut tx, &child_sql(line, f.second_move, &[])).await;
    assert_foreign_key(&f, &error, SERIAL, "stock_move_line_serial_line_move");
    let child = insert(&mut tx, &child_sql(line, f.mv, &[])).await.unwrap();
    // re-pointing the line to another move of the same company and product carries its children
    tx.execute(
        "UPDATE stock_move_line SET move_id = $1 WHERE id = $2",
        &[&f.second_move, &line],
    )
    .await
    .unwrap();
    let moved: i64 = tx
        .query(
            "SELECT move_id FROM stock_move_line_serial WHERE id = $1",
            &[&child],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(moved, f.second_move);
    // corrections
    let other_line = insert(&mut tx, &line_sql(&f, f.mv, &[("child_mode", "true")]))
        .await
        .unwrap();
    let child_text = child.to_string();
    let error = refused(
        &mut tx,
        &correction_sql(
            &f,
            other_line,
            &[
                ("serial_child_id", child_text.as_str()),
                ("old_quantity", "1"),
                ("new_quantity", "1"),
            ],
        ),
    )
    .await;
    assert_foreign_key(
        &f,
        &error,
        CORRECTION,
        "stock_move_line_correction_child_line",
    );
    let error = refused(
        &mut tx,
        &correction_sql(&f, line, &[("company_id", other_company.as_str())]),
    )
    .await;
    assert_foreign_key(
        &f,
        &error,
        CORRECTION,
        "stock_move_line_correction_line_company_product",
    );
    for (column, name) in [
        ("old_lot_id", "stock_move_line_correction_old_lot_product"),
        ("new_lot_id", "stock_move_line_correction_new_lot_product"),
    ] {
        let error = refused(
            &mut tx,
            &correction_sql(&f, line, &[(column, foreign_lot.as_str())]),
        )
        .await;
        assert_foreign_key(&f, &error, CORRECTION, name);
    }
    // a child-scope row of the child's own line is accepted; the child can no longer change line
    insert(
        &mut tx,
        &correction_sql(
            &f,
            line,
            &[
                ("serial_child_id", child_text.as_str()),
                ("old_quantity", "1"),
                ("new_quantity", "1"),
            ],
        ),
    )
    .await
    .unwrap();
    let error = tx
        .savepoint(async |tx| {
            tx.execute(
                "UPDATE stock_move_line_serial SET line_id = $1, move_id = $2 WHERE id = $3",
                &[&other_line, &f.mv, &child],
            )
            .await
        })
        .await
        .unwrap_err();
    assert_foreign_key(
        &f,
        &error,
        CORRECTION,
        "stock_move_line_correction_child_line",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
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

/// No spec case covers these links yet, so the test carries no tag.
#[tokio::test]
async fn line_links_must_share_the_row_company() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let foreign_location = create(
        &mut tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Shelf B"))
            .set_cell("company_id", Cell::Id(f.other_company)),
    )
    .await;
    let foreign_partner = create(
        &mut tx,
        "res.partner",
        RawVals::new()
            .set_cell("name", text("Foreign Partner"))
            .set_cell("company_id", Cell::Id(f.other_company)),
    )
    .await;
    let picking_type = create(
        &mut tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text("Receipts B"))
            .set_cell("company_id", Cell::Id(f.other_company))
            .set_cell("default_location_src_id", Cell::Id(f.location))
            .set_cell("default_location_dest_id", Cell::Id(f.other_location)),
    )
    .await;
    let foreign_picking = create_in(
        &mut tx,
        f.other_company,
        "stock.picking",
        RawVals::new()
            .set_cell("name", text("B/IN/00001"))
            .set_cell("company_id", Cell::Id(f.other_company))
            .set_cell("picking_type_id", Cell::Id(picking_type))
            .set_cell("location_id", Cell::Id(f.location))
            .set_cell("location_dest_id", Cell::Id(f.other_location)),
    )
    .await
    .unwrap();
    let vals = || {
        RawVals::new()
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("product_id", Cell::Id(f.product))
            .set_cell("move_id", Cell::Id(f.mv))
            .set_cell("uom_id", Cell::Id(f.unit))
            .set_cell("location_id", Cell::Id(f.location))
            .set_cell("location_dest_id", Cell::Id(f.other_location))
            .set_cell("quantity_product_uom", dec("0"))
            .set_cell("uom_factor", dec("1"))
            .set_cell("product_uom_factor", dec("1"))
            .set_cell("product_unit_digits", Cell::Int(2))
    };
    let spec = f.h.registry().model(LINE).unwrap();
    let label = |field: &str| spec.field(field).unwrap().string.clone();
    for (field, target, value) in [
        ("location_id", foreign_location, "Shelf B"),
        ("location_dest_id", foreign_location, "Shelf B"),
        ("owner_id", foreign_partner, "Foreign Partner"),
        ("picking_id", foreign_picking, "B/IN/00001"),
    ] {
        let error = create_in(
            &mut tx,
            f.company,
            LINE,
            vals().set_cell(field, Cell::Id(target)),
        )
        .await
        .unwrap_err();
        assert_crossover(&error, field, &label(field), value);
    }
    create_in(
        &mut tx,
        f.company,
        LINE,
        vals().set_cell("owner_id", Cell::Id(f.partner)),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// and `seq` starts at 1; the counter statement and the tuple chain are's)
/// 's)
/// 's)
#[tokio::test]
async fn corrections_have_one_identity_per_line_and_typed_kinds() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let line = insert(&mut tx, &line_sql(&f, f.mv, &[("child_mode", "true")]))
        .await
        .unwrap();
    let child = insert(&mut tx, &child_sql(line, f.mv, &[]))
        .await
        .unwrap()
        .to_string();
    let child = child.as_str();
    insert(&mut tx, &correction_sql(&f, line, &[]))
        .await
        .unwrap();
    // seq 1 again, now in the child scope: refused by the line's identity
    let child_scope = [
        ("serial_child_id", child),
        ("old_quantity", "1"),
        ("new_quantity", "0"),
        ("kind", "'remove_serial'"),
    ];
    let error = refused(&mut tx, &correction_sql(&f, line, &child_scope)).await;
    assert_violation(
        &error,
        "23505",
        "stock_move_line_correction_identity",
        message(&f, CORRECTION, "_identity"),
    );
    let checks: Vec<(Vec<(&str, &str)>, &str)> = vec![
        (vec![("seq", "0")], "_seq_positive"),
        (vec![("old_quantity", "'NaN'")], "_finite_old_quantity"),
        (vec![("new_quantity", "'Infinity'")], "_finite_new_quantity"),
        (
            vec![("quantity_delta_old_key", "'NaN'")],
            "_finite_quantity_delta_old_key",
        ),
        (
            vec![("quantity_delta_new_key", "'-Infinity'")],
            "_finite_quantity_delta_new_key",
        ),
        (
            vec![
                ("seq", "2"),
                ("serial_child_id", child),
                ("old_quantity", "1"),
                ("new_quantity", "2"),
            ],
            "_child_quantity_unit",
        ),
        (
            vec![("seq", "2"), ("serial_child_id", child)],
            "_child_quantity_unit",
        ),
        (
            vec![("seq", "2"), ("kind", "'remove_serial'")],
            "_serial_kind_scope",
        ),
        (
            vec![
                ("seq", "2"),
                ("kind", "'add_serial'"),
                ("serial_child_id", child),
                ("old_quantity", "1"),
                ("new_quantity", "0"),
            ],
            "_serial_kind_scope",
        ),
        (
            vec![
                ("seq", "2"),
                ("kind", "'remove_serial'"),
                ("serial_child_id", child),
                ("old_quantity", "0"),
                ("new_quantity", "1"),
            ],
            "_serial_kind_scope",
        ),
    ];
    for (over, name) in checks {
        let error = refused(&mut tx, &correction_sql(&f, line, &over)).await;
        assert_violation(
            &error,
            "23514",
            &format!("stock_move_line_correction{name}"),
            message(&f, CORRECTION, name),
        );
    }
    // the next seq in the child scope, then an add back: accepted
    for (seq, kind, old, new) in [
        ("2", "'remove_serial'", "1", "0"),
        ("3", "'add_serial'", "0", "1"),
    ] {
        insert(
            &mut tx,
            &correction_sql(
                &f,
                line,
                &[
                    ("seq", seq),
                    ("kind", kind),
                    ("serial_child_id", child),
                    ("old_quantity", old),
                    ("new_quantity", new),
                ],
            ),
        )
        .await
        .unwrap();
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// `(name, oid, definition)` of the line tables' composite keys and their unique targets.
async fn line_keys(tx: &mut Transaction) -> Vec<(String, u32, String)> {
    let names = vec![
        "stock_move_id_company_product",
        "stock_move_line_id_move",
        "stock_move_line_id_company_product",
        "stock_move_line_lot_product",
        "stock_move_line_move_company_product",
        "stock_move_line_serial_id_line",
        "stock_move_line_serial_line_move",
        "stock_move_line_correction_identity",
        "stock_move_line_correction_line_company_product",
        "stock_move_line_correction_child_line",
        "stock_move_line_correction_old_lot_product",
        "stock_move_line_correction_new_lot_product",
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

/// Every composite key exists with its unique target (phase 2 adds them in source order, so the targets
/// are created first on a fresh install), and a second synchronization changes nothing (same oids, the
/// open-line key index kept).
#[tokio::test]
async fn line_keys_are_synchronized_and_stable() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let fresh = line_keys(&mut tx).await;
    let got: Vec<(String, String)> = fresh
        .iter()
        .map(|(n, _, d)| (n.clone(), d.clone()))
        .collect();
    let want: Vec<(String, String)> = [
        (
            "stock_move_id_company_product",
            "UNIQUE (id, company_id, product_id)",
        ),
        (
            "stock_move_line_correction_child_line",
            "FOREIGN KEY (serial_child_id, move_line_id) REFERENCES stock_move_line_serial(id, line_id)",
        ),
        (
            "stock_move_line_correction_identity",
            "UNIQUE NULLS NOT DISTINCT (move_line_id, seq)",
        ),
        (
            "stock_move_line_correction_line_company_product",
            "FOREIGN KEY (move_line_id, company_id, product_id) REFERENCES stock_move_line(id, company_id, product_id)",
        ),
        (
            "stock_move_line_correction_new_lot_product",
            "FOREIGN KEY (new_lot_id, product_id) REFERENCES stock_lot(id, product_id)",
        ),
        (
            "stock_move_line_correction_old_lot_product",
            "FOREIGN KEY (old_lot_id, product_id) REFERENCES stock_lot(id, product_id)",
        ),
        (
            "stock_move_line_id_company_product",
            "UNIQUE (id, company_id, product_id)",
        ),
        ("stock_move_line_id_move", "UNIQUE (id, move_id)"),
        (
            "stock_move_line_lot_product",
            "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot(id, product_id)",
        ),
        (
            "stock_move_line_move_company_product",
            "FOREIGN KEY (move_id, company_id, product_id) REFERENCES stock_move(id, company_id, product_id)",
        ),
        ("stock_move_line_serial_id_line", "UNIQUE (id, line_id)"),
        (
            "stock_move_line_serial_line_move",
            "FOREIGN KEY (line_id, move_id) REFERENCES stock_move_line(id, move_id) ON UPDATE CASCADE ON DELETE CASCADE",
        ),
    ]
    .iter()
    .map(|(n, d)| ((*n).to_owned(), (*d).to_owned()))
    .collect();
    assert_eq!(got, want);
    let key_index = "SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = current_schema() AND c.relname = 'stock_move_line_open_key'";
    let before: u32 = tx.query(key_index, &[]).await.unwrap()[0].get(0);
    tx.init_models(&[]).await.unwrap();
    assert_eq!(line_keys(&mut tx).await, fresh);
    let after: u32 = tx.query(key_index, &[]).await.unwrap()[0].get(0);
    assert_eq!(before, after, "the open-line key index is kept");
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Deleting a child-mode line removes its serial rows: the line's own key and the field's key both
/// cascade (the composite `_line_move` key used to have no `ON DELETE`, so it could refuse first).
#[tokio::test]
async fn deleting_a_line_removes_its_serial_rows() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let line = insert(&mut tx, &line_sql(&f, f.mv, &[("child_mode", "true")]))
        .await
        .unwrap();
    let child = insert(&mut tx, &child_sql(line, f.mv, &[])).await.unwrap();
    tx.execute("DELETE FROM stock_move_line WHERE id = $1", &[&line])
        .await
        .unwrap_or_else(|e| panic!("delete of a line with a serial row: {e:?}"));
    let left = tx
        .query(
            "SELECT count(*) FROM stock_move_line_serial WHERE id = $1",
            &[&child],
        )
        .await
        .unwrap()[0]
        .get::<_, i64>(0);
    assert_eq!(left, 0, "the serial row goes with its line");
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
