// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema of the stock operations tables: reference,
//! picking and move. Schema only: the rows below are written with plain SQL or plain ORM creates, because
//! the units that maintain counters, states, names and product-unit quantities are later slices.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_addon_uom::conversion::Unit;
use erp_addon_uom::stock::{ProductUnitPreview, product_quantity_sql};
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_stock_ops", depends = ["stock"]);

/// A test stand-in for `ir.model.data` with the columns the product addon and the company create path
/// read (the real model lives in `erp-data`, out of reach of an addon crate; the product tests use the
/// same stand-in).
#[model(
    name = "ir.model.data",
    description = "Model Data",
    addon = "test_stock_ops"
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

/// Ids every case starts from: two companies with one operation type each, a shared product, a unit,
/// two shared locations, a shared partner.
struct Fixture {
    h: Harness,
    company: i64,
    other_company: i64,
    product: i64,
    unit: i64,
    location: i64,
    other_location: i64,
    partner: i64,
    picking_type: i64,
    other_picking_type: i64,
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

/// An operation type of `company` between the two fixture locations.
async fn create_picking_type(
    tx: &mut Transaction,
    company: i64,
    name: &str,
    source: i64,
    dest: i64,
) -> i64 {
    create(
        tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("default_location_src_id", Cell::Id(source))
            .set_cell("default_location_dest_id", Cell::Id(dest)),
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
    // the acting user is the default responsible of a transfer; it belongs to both companies, as
    // an administrator does (res.users is scoped by company_ids)
    let uid = tx.env().uid();
    for cid in [company, other_company] {
        tx.execute(
            // creating a company may already have added the acting user
            "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&uid, &cid],
        )
        .await
        .unwrap();
    }
    let env = tx.env().with_company(company).unwrap();
    let template = env
        .records("product.template")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", text("Widget"))],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    let product: i64 = tx
        .query(
            "SELECT id FROM product_product WHERE product_tmpl_id = $1",
            &[&template],
        )
        .await
        .unwrap()[0]
        .get(0);
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
    let picking_type =
        create_picking_type(&mut tx, company, "Receipts", location, other_location).await;
    let other_picking_type = create_picking_type(
        &mut tx,
        other_company,
        "Receipts B",
        location,
        other_location,
    )
    .await;
    tx.commit().await.unwrap();
    Fixture {
        h,
        company,
        other_company,
        product,
        unit,
        location,
        other_location,
        partner,
        picking_type,
        other_picking_type,
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

/// PostgreSQL's own rendering of an index on `table` written as `body` (`(cols) WHERE ...`): a probe
/// index is created, read and dropped again, so the plan's definition is compared in the catalog's
/// normal form rather than as a hand-typed string.
async fn probe_definition(tx: &mut Transaction, table: &str, body: &str) -> String {
    let probe = format!("ops_probe_{table}");
    tx.batch(&format!("CREATE INDEX {probe} ON {table} {body}"))
        .await
        .unwrap();
    let out = index_definition(tx, &probe).await;
    tx.batch(&format!("DROP INDEX {probe}")).await.unwrap();
    out
}

/// Assert the field index of `table.column` is a plain btree, or a btree over non-null values when
/// `not_null`, in PostgreSQL's rendering of the same definition.
async fn assert_btree(tx: &mut Transaction, table: &str, column: &str, not_null: bool) {
    let body = if not_null {
        format!("({column}) WHERE {column} IS NOT NULL")
    } else {
        format!("({column})")
    };
    let expected = probe_definition(tx, table, &body).await;
    assert_eq!(
        index_definition(tx, &format!("{table}__{column}_index")).await,
        expected,
        "{table}.{column}"
    );
}

/// Assert the field index of `table.column` is a trigram GIN index over the column.
async fn assert_trigram(tx: &mut Transaction, table: &str, column: &str) {
    let def = index_definition(tx, &format!("{table}__{column}_index")).await;
    assert!(
        def.starts_with("gin (") && def.contains(column) && def.contains("gin_trgm_ops"),
        "{table}.{column}: {def}"
    );
}

/// The indexes of relation table `table` as `(is primary key, key columns in order)`.
async fn relation_indexes(tx: &mut Transaction, table: &str) -> Vec<(bool, Vec<String>)> {
    let rows = tx
        .query(
            "SELECT i.indisprimary, array_agg(a.attname::text ORDER BY k.ord) \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
             WHERE n.nspname = current_schema() AND c.relname = $1 \
             GROUP BY i.indexrelid, i.indisprimary ORDER BY i.indisprimary DESC",
            &[&table],
        )
        .await
        .unwrap();
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

/// Relation `table` has the two `bigint NOT NULL` columns, a primary key on both and one more index on
/// the reverse order. Which declaration creates the table (and so which column leads the primary key)
/// depends on registry order, so only the pair and the reversal are asserted.
async fn assert_relation(tx: &mut Transaction, table: &str, a: &str, b: &str) {
    assert_eq!(
        columns(tx, table, &[a, b]).await,
        expect(&[(a, "bigint", false), (b, "bigint", false)]),
        "{table}"
    );
    let indexes = relation_indexes(tx, table).await;
    assert_eq!(indexes.len(), 2, "{table}: {indexes:?}");
    let (primary, pk) = &indexes[0];
    let (other_primary, reverse) = &indexes[1];
    assert!(*primary && !*other_primary, "{table}: {indexes:?}");
    let mut pair = pk.clone();
    pair.sort();
    let mut wanted = vec![a.to_owned(), b.to_owned()];
    wanted.sort();
    assert_eq!(pair, wanted, "{table}");
    let reversed: Vec<String> = pk.iter().rev().cloned().collect();
    assert_eq!(*reverse, reversed, "{table}");
}

fn assert_violation(error: &Error, sqlstate: &str, constraint: &str, message: &str) {
    assert_eq!(error.sqlstate(), Some(sqlstate), "{error:?}");
    assert_eq!(error.constraint(), Some(constraint), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(error.message(), format!("{PREFIX}{message}"));
}

const PICK_NONNEG: &str = "The move counts of a transfer cannot be negative.";
const PICK_SUM: &str = "The state counts of a transfer must add up to its move count.";
const PICK_SUBSETS: &str = "A subset count of a transfer cannot exceed the count it belongs to.";

/// The declared message of a move check (the model's text, `addons/stock/src/stock_move.rs`).
fn move_message(constraint: &str) -> &'static str {
    match constraint {
        "stock_move_digits_range" => {
            "The Product Unit digits of a stock move must be between 0 and 28."
        }
        "stock_move_factor_nonzero" => "The unit factors of a stock move cannot be zero.",
        "stock_move_finite_product_uom_qty" => {
            "The demand of a stock move must be a finite number."
        }
        "stock_move_finite_quantity" => "The quantity of a stock move must be a finite number.",
        "stock_move_finite_product_qty" => {
            "The demand in the product unit of a stock move must be a finite number."
        }
        "stock_move_finite_quantity_product_uom" => {
            "The quantity in the product unit of a stock move must be a finite number."
        }
        "stock_move_finite_uom_factor" => {
            "The unit factor of a stock move must be a finite number."
        }
        "stock_move_finite_product_uom_factor" => {
            "The product unit factor of a stock move must be a finite number."
        }
        "stock_move_finite_price_unit" => "The unit price of a stock move must be a finite number.",
        "stock_move_finite_packaging_uom_qty" => {
            "The packaging quantity of a stock move must be a finite number."
        }
        "stock_move_open_line_version_nonneg" => {
            "The open line version of a stock move cannot be negative."
        }
        "stock_move_product_qty_conversion" => {
            "The demand in the product unit must be the demand converted with the stored unit factors and digits."
        }
        "stock_move_quantity_product_uom_conversion" => {
            "The quantity in the product unit must be the quantity converted with the stored unit factors and digits."
        }
        other => panic!("no move check {other}"),
    }
}

/// The values of a picking created through the ORM.
fn picking_vals(f: &Fixture, company: i64, picking_type: i64, name: &str) -> RawVals {
    RawVals::new()
        .set_cell("name", text(name))
        .set_cell("company_id", Cell::Id(company))
        .set_cell("picking_type_id", Cell::Id(picking_type))
        .set_cell("location_id", Cell::Id(f.location))
        .set_cell("location_dest_id", Cell::Id(f.other_location))
}

async fn create_picking(tx: &mut Transaction, company: i64, vals: RawVals) -> Result<i64> {
    tx.savepoint(async |tx| {
        Ok(tx
            .env()
            .with_company(company)?
            .records("stock.picking")?
            .create(tx, vec![vals])
            .await?
            .rec_ids()[0]
            .get())
    })
    .await
}

/// The values of a move of `company` through the ORM, with consistent product-unit columns (factor
/// one, Product Unit digits 2, zero quantities).
fn move_vals(f: &Fixture, company: i64) -> RawVals {
    RawVals::new()
        .set_cell("company_id", Cell::Id(company))
        .set_cell("product_id", Cell::Id(f.product))
        .set_cell("uom_id", Cell::Id(f.unit))
        .set_cell("location_id", Cell::Id(f.location))
        .set_cell("location_dest_id", Cell::Id(f.other_location))
        .set_cell("product_qty", dec("0"))
        .set_cell("quantity_product_uom", dec("0"))
        .set_cell("uom_factor", dec("1"))
        .set_cell("product_uom_factor", dec("1"))
        .set_cell("product_unit_digits", Cell::Int(2))
}

async fn create_move(tx: &mut Transaction, company: i64, vals: RawVals) -> Result<i64> {
    tx.savepoint(async |tx| {
        Ok(tx
            .env()
            .with_company(company)?
            .records("stock.move")?
            .create(tx, vec![vals])
            .await?
            .rec_ids()[0]
            .get())
    })
    .await
}

/// One move row written with plain SQL.
#[derive(Clone)]
struct MoveRow {
    demand: String,
    quantity: String,
    product_qty: String,
    quantity_product_uom: String,
    uom_factor: String,
    product_uom_factor: String,
    digits: i32,
    version: i32,
}

impl MoveRow {
    /// A consistent zero row: factors one, digits 2, version 0.
    fn zero() -> Self {
        Self {
            demand: "0".into(),
            quantity: "0".into(),
            product_qty: "0".into(),
            quantity_product_uom: "0".into(),
            uom_factor: "1".into(),
            product_uom_factor: "1".into(),
            digits: 2,
            version: 0,
        }
    }
}

const MOVE_INSERT: &str = "INSERT INTO stock_move \
    (company_id, product_id, uom_id, location_id, location_dest_id, date, state, procure_method, \
     open_line_version, product_uom_qty, quantity, product_qty, quantity_product_uom, uom_factor, \
     product_uom_factor, product_unit_digits) \
    VALUES ($1, $2, $3, $4, $5, now(), 'draft', 'make_to_stock', $6, $7::text::numeric, \
     $8::text::numeric, $9::text::numeric, $10::text::numeric, $11::text::numeric, \
     $12::text::numeric, $13)";

async fn insert_move(f: &Fixture, tx: &mut Transaction, row: &MoveRow) -> Result<u64> {
    tx.savepoint(async |tx| {
        tx.execute(
            MOVE_INSERT,
            &[
                &f.company,
                &f.product,
                &f.unit,
                &f.location,
                &f.other_location,
                &row.version,
                &row.demand,
                &row.quantity,
                &row.product_qty,
                &row.quantity_product_uom,
                &row.uom_factor,
                &row.product_uom_factor,
                &row.digits,
            ],
        )
        .await
    })
    .await
}

fn ids(cell: Cell) -> Vec<i64> {
    let Cell::Ids(ids) = cell else {
        panic!("an id list, got {cell:?}")
    };
    let mut out: Vec<i64> = ids.iter().map(|id| id.get()).collect();
    out.sort_unstable();
    out
}

fn rec_ids(ids: &[i64]) -> Vec<RecId> {
    ids.iter()
        .map(|id| RecId::from_real(*id).unwrap())
        .collect()
}

/// The reference keeps its supplied name, the name is required and readonly, and the table has the
/// stored relation tables of both of its many2many fields.
#[tokio::test]
async fn reference_name_and_relation_tables() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        columns(&mut tx, "stock_reference", &["name"]).await,
        expect(&[("name", "character varying", false)])
    );
    let spec = f.h.registry().model("stock.reference").unwrap();
    let name = spec.field("name").unwrap();
    assert!(name.required && name.readonly && name.store);
    let id = create(
        &mut tx,
        "stock.reference",
        RawVals::new().set_cell("name", text("SO/0042")),
    )
    .await;
    let stored: String = tx
        .query("SELECT name FROM stock_reference WHERE id = $1", &[&id])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored, "SO/0042");
    // without a name: the not-null refusal naming the field
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("stock.reference")?
                .create(tx, vec![RawVals::new()])
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "{PREFIX}Missing required field '{}' (name) for model '{}' (stock.reference)",
            name.string, spec.description
        )
    );
    for (table, a, b) in [
        ("stock_reference_move_rel", "reference_id", "move_id"),
        ("stock_reference_picking_rel", "reference_id", "picking_id"),
        ("stock_move_move_rel", "move_orig_id", "move_dest_id"),
        ("stock_route_move", "move_id", "route_id"),
        (
            "template_attribute_value_stock_move_rel",
            "move_id",
            "template_attribute_value_id",
        ),
    ] {
        assert_relation(&mut tx, table, a, b).await;
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// One reference relates to several moves, and the association reads the same from both sides (the
/// reference's `move_ids` and each move's `reference_ids` share `stock_reference_move_rel`); the
/// stored transfer side shares `stock_reference_picking_rel` with the picking's `reference_ids`.
#[tokio::test]
async fn reference_relates_to_several_moves_from_both_sides() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let mut moves = Vec::new();
    for _ in 0..3 {
        moves.push(
            create_move(&mut tx, f.company, move_vals(&f, f.company))
                .await
                .unwrap(),
        );
    }
    let other = create_move(&mut tx, f.company, move_vals(&f, f.company))
        .await
        .unwrap();
    let picking = create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, f.picking_type, "WH/IN/00001"),
    )
    .await
    .unwrap();
    let reference = create(
        &mut tx,
        "stock.reference",
        RawVals::new()
            .set_cell("name", text("PO/0007"))
            .set_commands("move_ids", [Command::set(rec_ids(&moves))])
            .set_commands("picking_ids", [Command::set(rec_ids(&[picking]))]),
    )
    .await;
    let references = tx.env().records("stock.reference").unwrap();
    let linked = ids(references
        .browse([reference])
        .unwrap()
        .cell(&mut tx, "move_ids")
        .await
        .unwrap());
    let mut expected = moves.clone();
    expected.sort_unstable();
    assert_eq!(linked, expected);
    let move_records = tx.env().records("stock.move").unwrap();
    for id in &moves {
        let back = ids(move_records
            .browse([*id])
            .unwrap()
            .cell(&mut tx, "reference_ids")
            .await
            .unwrap());
        assert_eq!(back, [reference], "move {id}");
    }
    let unlinked = ids(move_records
        .browse([other])
        .unwrap()
        .cell(&mut tx, "reference_ids")
        .await
        .unwrap());
    assert!(unlinked.is_empty());
    // the transfer side, from the picking
    let back = ids(tx
        .env()
        .records("stock.picking")
        .unwrap()
        .browse([picking])
        .unwrap()
        .cell(&mut tx, "reference_ids")
        .await
        .unwrap());
    assert_eq!(back, [reference]);
    // a second reference on one move: the association is many-to-many
    let second = create(
        &mut tx,
        "stock.reference",
        RawVals::new()
            .set_cell("name", text("SO/0008"))
            .set_commands("move_ids", [Command::set(rec_ids(&moves[..1]))]),
    )
    .await;
    let back = ids(move_records
        .browse([moves[0]])
        .unwrap()
        .cell(&mut tx, "reference_ids")
        .await
        .unwrap());
    let mut both = vec![reference, second];
    both.sort_unstable();
    assert_eq!(back, both);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The move chain is one self relation read from both sides: a move's destinations are the moves
/// whose origins include it.
#[tokio::test]
async fn move_chain_reads_from_both_sides() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let first = create_move(&mut tx, f.company, move_vals(&f, f.company))
        .await
        .unwrap();
    let second = create_move(
        &mut tx,
        f.company,
        move_vals(&f, f.company).set_commands("move_orig_ids", [Command::set(rec_ids(&[first]))]),
    )
    .await
    .unwrap();
    let moves = tx.env().records("stock.move").unwrap();
    let dest = ids(moves
        .browse([first])
        .unwrap()
        .cell(&mut tx, "move_dest_ids")
        .await
        .unwrap());
    assert_eq!(dest, [second]);
    let row = tx
        .query(
            "SELECT move_orig_id, move_dest_id FROM stock_move_move_rel",
            &[],
        )
        .await
        .unwrap();
    let got: Vec<(i64, i64)> = row.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [(first, second)]);
    let spec = f.h.registry().model("stock.move").unwrap();
    for field in ["move_dest_ids", "move_orig_ids", "price_unit"] {
        assert!(!spec.field(field).unwrap().copy, "{field}");
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

const COUNTERS: [&str; 14] = [
    "move_count",
    "draft_count",
    "waiting_count",
    "confirmed_count",
    "partially_available_count",
    "assigned_count",
    "done_count",
    "cancel_count",
    "done_scrap_count",
    "cancel_scrap_count",
    "assigned_zero_demand_count",
    "open_positive_demand_count",
    "confirmed_positive_demand_count",
    "mto_nonscrap_count",
];

/// The picking columns, its ORM defaults (no name default), the counter vector at zero, the readonly
/// flags and its indexes, the partial one with the plan's predicate.
#[tokio::test]
async fn picking_columns_defaults_and_indexes() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let mut spec_cols = vec![
        ("name", "character varying", false),
        ("origin", "character varying", true),
        ("note", "text", true),
        ("backorder_id", "bigint", true),
        ("return_id", "bigint", true),
        ("move_type", "character varying", false),
        ("state", "character varying", false),
        ("priority", "character varying", true),
        ("scheduled_date", "timestamp without time zone", true),
        ("date_deadline", "timestamp without time zone", true),
        ("has_deadline_issue", "boolean", true),
        ("date_done", "timestamp without time zone", true),
        ("location_id", "bigint", false),
        ("location_dest_id", "bigint", false),
        ("picking_type_id", "bigint", false),
        ("partner_id", "bigint", true),
        ("owner_id", "bigint", true),
        ("company_id", "bigint", false),
        ("user_id", "bigint", true),
        ("printed", "boolean", true),
        ("is_locked", "boolean", true),
        ("shipping_weight", "numeric", true),
        ("show_return", "boolean", true),
    ];
    for counter in COUNTERS {
        spec_cols.push((counter, "integer", false));
    }
    let names: Vec<&str> = spec_cols.iter().map(|c| c.0).collect();
    assert_eq!(
        columns(&mut tx, "stock_picking", &names).await,
        expect(&spec_cols)
    );
    let spec = f.h.registry().model("stock.picking").unwrap();
    for field in COUNTERS.iter().copied().chain([
        "state",
        "backorder_id",
        "return_id",
        "date_done",
        "reference_ids",
    ]) {
        assert!(spec.field(field).unwrap().readonly, "{field} is readonly");
    }
    // no default name: a picking without one is refused
    let error = create_picking(
        &mut tx,
        f.company,
        RawVals::new()
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("picking_type_id", Cell::Id(f.picking_type))
            .set_cell("location_id", Cell::Id(f.location))
            .set_cell("location_dest_id", Cell::Id(f.other_location)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "{PREFIX}Missing required field '{}' (name) for model '{}' (stock.picking)",
            spec.field("name").unwrap().string,
            spec.description
        )
    );
    let id = create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, f.picking_type, "WH/IN/00001"),
    )
    .await
    .unwrap();
    let row = tx
        .query(
            "SELECT state, move_type, priority, is_locked::text, show_return::text, \
             (scheduled_date IS NOT NULL)::text, user_id::text FROM stock_picking WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    let got: Vec<String> = (0..7).map(|i| row[0].get(i)).collect();
    let uid = tx.env().uid().to_string();
    assert_eq!(
        got,
        ["draft", "direct", "0", "true", "true", "true", uid.as_str()]
    );
    for counter in COUNTERS {
        let value: i32 = tx
            .query(
                &format!("SELECT {counter} FROM stock_picking WHERE id = $1"),
                &[&id],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(value, 0, "{counter}");
    }
    for field in ["name", "origin"] {
        assert_trigram(&mut tx, "stock_picking", field).await;
    }
    for (field, not_null) in [
        ("backorder_id", true),
        ("return_id", true),
        ("partner_id", true),
        ("owner_id", true),
        ("user_id", true),
        ("scheduled_date", false),
        ("picking_type_id", false),
        ("location_id", false),
        ("location_dest_id", false),
    ] {
        assert_btree(&mut tx, "stock_picking", field, not_null).await;
    }
    // the company leads the reference key, so it has no index of its own (N1)
    let own_company_index: i64 = tx
        .query(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = current_schema() \
             AND tablename = 'stock_picking' AND indexname = 'stock_picking__company_id_index'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(own_company_index, 0);
    let probe = probe_definition(
        &mut tx,
        "stock_picking",
        "(picking_type_id, state) WHERE state NOT IN ('done', 'cancel')",
    )
    .await;
    assert_eq!(
        index_definition(&mut tx, "stock_picking_open_type_state").await,
        probe
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// A second reference in one company is refused whatever its operation type; the same reference in
/// another company is allowed.
#[tokio::test]
async fn picking_reference_is_unique_per_company() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let message = "Reference must be unique per company!";
    create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, f.picking_type, "REF"),
    )
    .await
    .unwrap();
    let error = create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, f.picking_type, "REF"),
    )
    .await
    .unwrap_err();
    assert_violation(&error, "23505", "stock_picking_name_uniq", message);
    // another operation type of the same company: still refused
    let delivery = create_picking_type(
        &mut tx,
        f.company,
        "Delivery Orders",
        f.location,
        f.other_location,
    )
    .await;
    let error = create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, delivery, "REF"),
    )
    .await
    .unwrap_err();
    assert_violation(&error, "23505", "stock_picking_name_uniq", message);
    // the other company, with its own type: allowed
    let other = create_picking(
        &mut tx,
        f.other_company,
        picking_vals(&f, f.other_company, f.other_picking_type, "REF"),
    )
    .await
    .unwrap();
    let rows = tx
        .query(
            "SELECT company_id FROM stock_picking WHERE name = 'REF' ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    let got: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, [f.company, f.other_company]);
    assert!(other > 0);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Set the named counters of picking `id` (the others stay as they are) in a savepoint.
async fn set_counters(tx: &mut Transaction, id: i64, values: &[(&str, i32)]) -> Result<u64> {
    let sets: Vec<String> = values.iter().map(|(c, v)| format!("{c} = {v}")).collect();
    let sql = format!("UPDATE stock_picking SET {} WHERE id = $1", sets.join(", "));
    tx.savepoint(async |tx| tx.execute(&sql, &[&id]).await)
        .await
}

/// The counter vector's checks (3): no count is negative, the seven state counts add up
/// to the move count, and each subset count stays within its set; the boundaries are accepted.
#[tokio::test]
async fn picking_counter_vector_checks() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let id = create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, f.picking_type, "WH/IN/00001"),
    )
    .await
    .unwrap();
    // every counter refuses -1 (the others at zero)
    for counter in COUNTERS {
        let error = set_counters(&mut tx, id, &[(counter, -1)])
            .await
            .unwrap_err();
        assert_violation(&error, "23514", "stock_picking_counts_nonneg", PICK_NONNEG);
    }
    // a state count without its move, and a move without a state
    for values in [
        &[("draft_count", 1)][..],
        &[("move_count", 1)][..],
        &[("move_count", 2), ("done_count", 1)][..],
    ] {
        let error = set_counters(&mut tx, id, values).await.unwrap_err();
        assert_violation(&error, "23514", "stock_picking_counts_sum", PICK_SUM);
    }
    // each subset beyond its set (the sum holds)
    let one_each: Vec<(&str, i32)> = vec![
        ("move_count", 7),
        ("draft_count", 1),
        ("waiting_count", 1),
        ("confirmed_count", 1),
        ("partially_available_count", 1),
        ("assigned_count", 1),
        ("done_count", 1),
        ("cancel_count", 1),
    ];
    set_counters(&mut tx, id, &one_each).await.unwrap();
    // open moves: 7 - done 1 - cancel 1 = 5; moves not known to be scrap once both scrap counts
    // are at their boundary 1: 7 - 1 - 1 = 5. Open positive comes before confirmed positive,
    // which it bounds.
    for (counter, over) in [
        ("done_scrap_count", 2),
        ("cancel_scrap_count", 2),
        ("assigned_zero_demand_count", 2),
        ("open_positive_demand_count", 6),
        ("confirmed_positive_demand_count", 2),
        ("mto_nonscrap_count", 6),
    ] {
        let error = set_counters(&mut tx, id, &[(counter, over)])
            .await
            .unwrap_err();
        assert_violation(
            &error,
            "23514",
            "stock_picking_counts_subsets",
            PICK_SUBSETS,
        );
        // the boundary: equal to its set
        set_counters(&mut tx, id, &[(counter, over - 1)])
            .await
            .unwrap_or_else(|e| panic!("{counter} = {}: {e:?}", over - 1));
    }
    // confirmed positive (1) is within open positive: open positive 0 is refused
    let error = set_counters(&mut tx, id, &[("open_positive_demand_count", 0)])
        .await
        .unwrap_err();
    assert_violation(
        &error,
        "23514",
        "stock_picking_counts_subsets",
        PICK_SUBSETS,
    );
    // the ORM path reports the same check with its kind and message
    let error = create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, f.picking_type, "WH/IN/00002")
            .set_cell("move_count", Cell::Int(1)),
    )
    .await
    .unwrap_err();
    assert_violation(&error, "23514", "stock_picking_counts_sum", PICK_SUM);
    // the shipping weight is a finite number; a finite value is accepted
    for value in ["NaN", "Infinity", "-Infinity"] {
        let sql =
            format!("UPDATE stock_picking SET shipping_weight = '{value}'::numeric WHERE id = $1");
        let error = tx
            .savepoint(async |tx| tx.execute(&sql, &[&id]).await)
            .await
            .unwrap_err();
        assert_violation(
            &error,
            "23514",
            "stock_picking_finite_shipping_weight",
            "The shipping weight of a transfer must be a finite number.",
        );
    }
    tx.savepoint(async |tx| {
        tx.execute(
            "UPDATE stock_picking SET shipping_weight = 12.5 WHERE id = $1",
            &[&id],
        )
        .await
    })
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The move columns, its ORM defaults, readonly flags and indexes, the partial one with the plan's
/// predicate and no plain index on `state`.
#[tokio::test]
async fn move_columns_defaults_and_indexes() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let spec_cols = [
        ("sequence", "integer", true),
        ("priority", "character varying", true),
        ("date", "timestamp without time zone", false),
        ("date_deadline", "timestamp without time zone", true),
        ("company_id", "bigint", false),
        ("product_id", "bigint", false),
        ("description_picking_manual", "text", true),
        ("product_uom_qty", "numeric", false),
        ("uom_id", "bigint", false),
        ("location_id", "bigint", false),
        ("location_dest_id", "bigint", false),
        ("forecasted_location_id", "bigint", true),
        ("partner_id", "bigint", true),
        ("picking_id", "bigint", true),
        ("state", "character varying", false),
        ("picked", "boolean", true),
        ("price_unit", "numeric", true),
        ("origin", "character varying", true),
        ("procure_method", "character varying", false),
        ("is_scrap", "boolean", true),
        ("rule_id", "bigint", true),
        ("propagate_cancel", "boolean", true),
        ("picking_type_id", "bigint", true),
        ("is_inventory", "boolean", true),
        ("inventory_name", "character varying", true),
        ("origin_returned_move_id", "bigint", true),
        ("restrict_partner_id", "bigint", true),
        ("warehouse_id", "bigint", true),
        ("quantity", "numeric", false),
        ("additional", "boolean", true),
        ("reference", "character varying", true),
        ("next_serial", "character varying", true),
        ("next_serial_count", "integer", true),
        ("reservation_date", "date", true),
        ("packaging_uom_id", "bigint", true),
        ("packaging_uom_qty", "numeric", true),
        ("should_replenish_scrapped", "boolean", true),
        ("open_line_version", "integer", false),
        ("done_at", "timestamp without time zone", true),
        ("product_qty", "numeric", false),
        ("quantity_product_uom", "numeric", false),
        ("uom_factor", "numeric", false),
        ("product_uom_factor", "numeric", false),
        ("product_unit_digits", "integer", false),
    ];
    let names: Vec<&str> = spec_cols.iter().map(|c| c.0).collect();
    assert_eq!(
        columns(&mut tx, "stock_move", &names).await,
        expect(&spec_cols)
    );
    let spec = f.h.registry().model("stock.move").unwrap();
    for field in [
        "state",
        "date_deadline",
        "description_picking_manual",
        "is_scrap",
        "inventory_name",
        "quantity",
        "reference",
        "open_line_version",
        "done_at",
        "product_qty",
        "quantity_product_uom",
        "uom_factor",
        "product_uom_factor",
        "product_unit_digits",
    ] {
        assert!(spec.field(field).unwrap().readonly, "{field} is readonly");
    }
    // the product-unit quantities have no default: the writer supplies them
    for field in ["product_qty", "quantity_product_uom", "done_at"] {
        assert!(spec.field(field).unwrap().default.is_none(), "{field}");
    }
    let id = create_move(&mut tx, f.company, move_vals(&f, f.company))
        .await
        .unwrap();
    let row = tx
        .query(
            "SELECT sequence::text, state, procure_method, propagate_cancel::text, \
             open_line_version::text, quantity::text, product_uom_qty::text, priority, picked::text, \
             additional::text, (done_at IS NULL)::text FROM stock_move WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    let got: Vec<String> = (0..11).map(|i| row[0].get(i)).collect();
    assert_eq!(
        got,
        [
            "10",
            "draft",
            "make_to_stock",
            "true",
            "0",
            "0",
            "0",
            "0",
            "false",
            "false",
            "true"
        ]
    );
    for (field, not_null) in [
        ("date", false),
        ("company_id", false),
        ("product_id", false),
        ("location_id", false),
        ("location_dest_id", false),
        ("forecasted_location_id", false),
        ("picking_id", false),
        ("origin_returned_move_id", false),
        ("partner_id", true),
        ("picking_type_id", true),
        ("restrict_partner_id", true),
        ("rule_id", true),
        ("warehouse_id", true),
        ("packaging_uom_id", true),
        ("uom_id", false),
    ] {
        assert_btree(&mut tx, "stock_move", field, not_null).await;
    }
    let probe = probe_definition(
        &mut tx,
        "stock_move",
        "(product_id, location_id, location_dest_id, company_id) WHERE state NOT IN ('done', 'cancel')",
    )
    .await;
    assert_eq!(
        index_definition(&mut tx, "stock_move_open_product_location").await,
        probe
    );
    // key list exactly `(state)`; the partial index's predicate also reads `((state)::text ...)`
    let state_indexes: i64 = tx
        .query(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = current_schema() \
             AND tablename = 'stock_move' AND indexdef ~ 'USING \\w+ \\(state\\)'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(state_indexes, 0, "no plain index on stock_move.state");
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The move's checks refuse each violation with their own constraint (the finite, factor and digits
/// checks sort before the conversion checks, so a NaN, a zero factor or bad digits is reported as
/// such, never as a division error), and accept the boundaries.
#[tokio::test]
async fn move_checks_refuse_and_accept_boundaries() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let row = MoveRow::zero;
    let mut cases: Vec<(MoveRow, &str)> = Vec::new();
    for bad in ["NaN", "Infinity", "-Infinity"] {
        cases.push((
            MoveRow {
                demand: bad.into(),
                ..row()
            },
            "stock_move_finite_product_uom_qty",
        ));
        cases.push((
            MoveRow {
                quantity: bad.into(),
                ..row()
            },
            "stock_move_finite_quantity",
        ));
        cases.push((
            MoveRow {
                product_qty: bad.into(),
                ..row()
            },
            "stock_move_finite_product_qty",
        ));
        cases.push((
            MoveRow {
                quantity_product_uom: bad.into(),
                ..row()
            },
            "stock_move_finite_quantity_product_uom",
        ));
    }
    // a NaN or infinite factor is nonzero for PostgreSQL; the finite check reports it
    for bad in ["NaN", "Infinity", "-Infinity"] {
        cases.push((
            MoveRow {
                uom_factor: bad.into(),
                ..row()
            },
            "stock_move_finite_uom_factor",
        ));
        cases.push((
            MoveRow {
                product_uom_factor: bad.into(),
                ..row()
            },
            "stock_move_finite_product_uom_factor",
        ));
    }
    // a zero factor is reported by the factor check (23514), never as a division by zero (22012)
    for bad in ["0", "0.000"] {
        cases.push((
            MoveRow {
                uom_factor: bad.into(),
                ..row()
            },
            "stock_move_factor_nonzero",
        ));
        cases.push((
            MoveRow {
                product_uom_factor: bad.into(),
                ..row()
            },
            "stock_move_factor_nonzero",
        ));
    }
    for bad in [-1, 29] {
        cases.push((
            MoveRow {
                digits: bad,
                ..row()
            },
            "stock_move_digits_range",
        ));
    }
    cases.push((
        MoveRow {
            version: -1,
            ..row()
        },
        "stock_move_open_line_version_nonneg",
    ));
    // 1 unit of factor 3 is 3 product units of factor 1
    cases.push((
        MoveRow {
            demand: "1".into(),
            uom_factor: "3".into(),
            quantity_product_uom: "0".into(),
            ..row()
        },
        "stock_move_product_qty_conversion",
    ));
    cases.push((
        MoveRow {
            quantity: "1".into(),
            uom_factor: "3".into(),
            demand: "1".into(),
            product_qty: "3".into(),
            ..row()
        },
        "stock_move_quantity_product_uom_conversion",
    ));
    for (case, constraint) in &cases {
        let error = insert_move(&f, &mut tx, case).await.unwrap_err();
        assert_violation(&error, "23514", constraint, move_message(constraint));
    }
    // boundaries: digits 0 and 28, version 0, a tiny factor, negative quantities and factors (a
    // stored unit may have a negative factor)
    let accepted = [
        MoveRow { digits: 0, ..row() },
        MoveRow {
            digits: 28,
            ..row()
        },
        MoveRow {
            demand: "-2".into(),
            quantity: "-2".into(),
            product_qty: "-6".into(),
            quantity_product_uom: "-6".into(),
            uom_factor: "3".into(),
            ..row()
        },
        MoveRow {
            demand: "1".into(),
            quantity: "0".into(),
            product_qty: "0.00".into(),
            quantity_product_uom: "0".into(),
            uom_factor: "0.000000001".into(),
            ..row()
        },
        MoveRow {
            demand: "2".into(),
            quantity: "2".into(),
            product_qty: "-6".into(),
            quantity_product_uom: "-6".into(),
            uom_factor: "-3".into(),
            ..row()
        },
        MoveRow {
            demand: "2".into(),
            quantity: "2".into(),
            product_qty: "6".into(),
            quantity_product_uom: "6".into(),
            uom_factor: "-3".into(),
            product_uom_factor: "-1".into(),
            ..row()
        },
    ];
    for case in &accepted {
        assert_eq!(insert_move(&f, &mut tx, case).await.unwrap(), 1);
    }
    // the ORM path: the conversion check with its kind and message
    let constraint = "stock_move_product_qty_conversion";
    let error = create_move(
        &mut tx,
        f.company,
        move_vals(&f, f.company)
            .set_cell("product_uom_qty", dec("1"))
            .set_cell("product_qty", dec("2")),
    )
    .await
    .unwrap_err();
    assert_violation(&error, "23514", constraint, move_message(constraint));
    // unit price and packaging quantity are finite numbers; finite values are accepted
    insert_move(&f, &mut tx, &MoveRow::zero()).await.unwrap();
    let last: i64 = tx
        .query("SELECT max(id) FROM stock_move", &[])
        .await
        .unwrap()[0]
        .get(0);
    for (column, constraint) in [
        ("price_unit", "stock_move_finite_price_unit"),
        ("packaging_uom_qty", "stock_move_finite_packaging_uom_qty"),
    ] {
        for value in ["NaN", "Infinity", "-Infinity"] {
            let sql = format!("UPDATE stock_move SET {column} = '{value}'::numeric WHERE id = $1");
            let error = tx
                .savepoint(async |tx| tx.execute(&sql, &[&last]).await)
                .await
                .unwrap_err();
            assert_violation(&error, "23514", constraint, move_message(constraint));
        }
        let sql = format!("UPDATE stock_move SET {column} = 3.75 WHERE id = $1");
        tx.savepoint(async |tx| tx.execute(&sql, &[&last]).await)
            .await
            .unwrap();
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The two conversion checks are, as declared on the model, exactly the unit service's SQL form of
/// the conversion (`product_quantity_sql`) over the move's own columns, so the database and the
/// service compute one expression and the naive `round(qty * factor / factor, digits)`, which
/// double-rounds near ties (`addons/uom/tests/stock_snapshot.rs:380-388`), is not used. The declared
/// text is read from the registry, not from the catalog's deparsed form.
#[tokio::test]
async fn conversion_checks_are_the_unit_service_expression() {
    let f = fixture().await;
    let spec = f.h.registry().model("stock.move").unwrap();
    for (constraint, column, qty) in [
        (
            "stock_move_product_qty_conversion",
            "product_qty",
            "product_uom_qty",
        ),
        (
            "stock_move_quantity_product_uom_conversion",
            "quantity_product_uom",
            "quantity",
        ),
    ] {
        let expr = product_quantity_sql(
            qty,
            "uom_factor",
            "product_uom_factor",
            "product_unit_digits",
        )
        .unwrap();
        let declared = match spec
            .table_object(constraint)
            .unwrap_or_else(|| panic!("{constraint} is declared"))
            .kind
        {
            erp_sdk::__private::TableObjectKind::Constraint { definition, .. } => definition,
            ref other => panic!("{constraint} is a CHECK constraint, got {other:?}"),
        };
        assert_eq!(
            declared,
            format!("CHECK ({column} = {expr})"),
            "{constraint}"
        );
    }
    f.h.cleanup().await.unwrap();
}

/// One conversion of the C3 corpus.
struct Conversion {
    qty: String,
    move_factor: String,
    product_factor: String,
    digits: u32,
}

/// The corpus: ratios with a finite decimal form (3, 12, 0.125, 10^6) and without one (1/3, 1/12 and 7
/// written as long decimals), large factors, negative quantities, values on and near HALF-UP ties, and
/// the near-tie shape of the unit service's own corpus (`addons/uom/tests/stock_snapshot.rs`): a ratio
/// `5 * 10^-(digits+1)` off by one in the last place of a long product factor.
fn corpus() -> Vec<Conversion> {
    let mut rows = Vec::new();
    let factors = [
        "1",
        "3",
        "-3",
        "7",
        "12",
        "0.125",
        "0.3333333333",
        "0.08333333333333",
        "1000000",
        "0.000001",
        "14.285714285714285714",
    ];
    let quantities = [
        "1", "2.5", "-1", "-2.5", "0.005", "-0.005", "0.015", "10", "123.456", "1000000", "0.0001",
        "7", "3", "0.125", "-0.375",
    ];
    for move_factor in factors {
        for product_factor in factors {
            for qty in quantities {
                for digits in [0, 2, 6] {
                    rows.push(Conversion {
                        qty: qty.to_owned(),
                        move_factor: move_factor.to_owned(),
                        product_factor: product_factor.to_owned(),
                        digits,
                    });
                }
            }
        }
    }
    for digits in 0..=6u32 {
        for a in [16u32, 18, 20] {
            let source = 5 * 10i128.pow(a);
            let power = 10i128.pow(a + digits + 1);
            for target in [power + 1, power - 1] {
                for qty in ["1", "-1"] {
                    rows.push(Conversion {
                        qty: qty.to_owned(),
                        move_factor: source.to_string(),
                        product_factor: target.to_string(),
                        digits,
                    });
                }
            }
        }
    }
    rows
}

/// C3 on the fallback shape: for every corpus row, the value of the unit service
/// (`ProductUnitPreview::product_quantity`: exact `qty * move factor / product factor`, HALF-UP at
/// the digits) is written as `product_qty` and `quantity_product_uom`, and the move's two conversion
/// checks must accept it. Every refused row is listed, with the naive `round(q * f / f, d)` value
/// beside it for comparison only; none is adjusted or dropped. No spec tag: asks for generated
/// columns, which this slice does not ship.
#[tokio::test]
async fn product_unit_check_agrees_with_the_unit_service() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let rows = corpus();
    let mut differences = Vec::new();
    for c in &rows {
        let preview = ProductUnitPreview::new(
            &Unit::new(1, 1, Decimal::from_str_exact(&c.move_factor).unwrap()).unwrap(),
            &Unit::new(2, 1, Decimal::from_str_exact(&c.product_factor).unwrap()).unwrap(),
            c.digits,
        )
        .unwrap();
        let expected = preview
            .product_quantity(Decimal::from_str_exact(&c.qty).unwrap())
            .unwrap()
            .to_string();
        let digits = i32::try_from(c.digits).unwrap();
        let row = MoveRow {
            demand: c.qty.clone(),
            quantity: c.qty.clone(),
            product_qty: expected.clone(),
            quantity_product_uom: expected.clone(),
            uom_factor: c.move_factor.clone(),
            product_uom_factor: c.product_factor.clone(),
            digits,
            version: 0,
        };
        if let Err(error) = insert_move(&f, &mut tx, &row).await {
            let pg: String = tx
                .query(
                    "SELECT round($1::text::numeric * $2::text::numeric / $3::text::numeric, $4)::text",
                    &[&c.qty, &c.move_factor, &c.product_factor, &digits],
                )
                .await
                .unwrap()[0]
                .get(0);
            differences.push(format!(
                "qty {} * {} / {} at {} digits: service {expected}, PostgreSQL {pg} ({:?})",
                c.qty,
                c.move_factor,
                c.product_factor,
                c.digits,
                error.constraint()
            ));
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
    assert!(
        differences.is_empty(),
        "the conversion check refuses the unit service's value on {} of {} rows:\n{}",
        differences.len(),
        rows.len(),
        differences.join("\n")
    );
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
async fn picking_and_move_links_must_share_the_row_company() {
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
    let foreign_picking = create_picking(
        &mut tx,
        f.other_company,
        picking_vals(&f, f.other_company, f.other_picking_type, "B/IN/00001"),
    )
    .await
    .unwrap();
    let foreign_move = create_move(
        &mut tx,
        f.other_company,
        move_vals(&f, f.other_company).set_cell("reference", text("B/IN/00001")),
    )
    .await
    .unwrap();
    let foreign_product = restricted_product(&mut tx, f.other_company, "Foreign Widget").await;
    let spec = f.h.registry().model("stock.picking").unwrap();
    let label = |field: &str| spec.field(field).unwrap().string.clone();
    for (field, target, value) in [
        ("picking_type_id", f.other_picking_type, "Receipts B"),
        ("location_id", foreign_location, "Shelf B"),
        ("location_dest_id", foreign_location, "Shelf B"),
        ("partner_id", foreign_partner, "Foreign Partner"),
        ("owner_id", foreign_partner, "Foreign Partner"),
        ("backorder_id", foreign_picking, "B/IN/00001"),
        ("return_id", foreign_picking, "B/IN/00001"),
    ] {
        let error = create_picking(
            &mut tx,
            f.company,
            picking_vals(&f, f.company, f.picking_type, "WH/IN/X")
                .set_cell(field, Cell::Id(target)),
        )
        .await
        .unwrap_err();
        assert_crossover(&error, field, &label(field), value);
    }
    let move_spec = f.h.registry().model("stock.move").unwrap();
    let move_label = |field: &str| move_spec.field(field).unwrap().string.clone();
    for (field, target, value) in [
        ("product_id", foreign_product, "Foreign Widget"),
        ("location_id", foreign_location, "Shelf B"),
        ("location_dest_id", foreign_location, "Shelf B"),
        ("forecasted_location_id", foreign_location, "Shelf B"),
        ("partner_id", foreign_partner, "Foreign Partner"),
        ("restrict_partner_id", foreign_partner, "Foreign Partner"),
        ("picking_id", foreign_picking, "B/IN/00001"),
        ("picking_type_id", f.other_picking_type, "Receipts B"),
        ("origin_returned_move_id", foreign_move, "B/IN/00001"),
    ] {
        let error = create_move(
            &mut tx,
            f.company,
            move_vals(&f, f.company).set_cell(field, Cell::Id(target)),
        )
        .await
        .unwrap_err();
        assert_crossover(&error, field, &move_label(field), value);
    }
    // shared targets and the row company's own type: accepted
    let own = create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, f.picking_type, "WH/IN/00001")
            .set_cell("partner_id", Cell::Id(f.partner))
            .set_cell("owner_id", Cell::Id(f.partner)),
    )
    .await
    .unwrap();
    create_move(
        &mut tx,
        f.company,
        move_vals(&f, f.company)
            .set_cell("picking_id", Cell::Id(own))
            .set_cell("picking_type_id", Cell::Id(f.picking_type))
            .set_cell("partner_id", Cell::Id(f.partner))
            .set_cell("restrict_partner_id", Cell::Id(f.partner)),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deleting_a_partner_who_owns_a_transfer_or_restricts_a_move_is_refused() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let picking = create_picking(
        &mut tx,
        f.company,
        picking_vals(&f, f.company, f.picking_type, "OWN/1")
            .set_cell("owner_id", Cell::Id(f.partner)),
    )
    .await
    .unwrap();
    let blocked = |model: &str, description: &str| {
        format!(
            "{PREFIX}Another model is using the record you are trying to delete.\n\nThe \
             troublemaker is: '{description}' ({model})"
        )
    };
    let delete = async |tx: &mut Transaction| {
        tx.savepoint(async |tx| {
            tx.execute("DELETE FROM res_partner WHERE id = $1", &[&f.partner])
                .await
        })
        .await
    };
    let error = delete(&mut tx).await.unwrap_err();
    assert_eq!(error.sqlstate(), Some("23001"), "{error:?}");
    assert!(
        error
            .message()
            .starts_with(&blocked("stock.picking", "Transfer")),
        "{}",
        error.message()
    );
    let owner: Option<i64> = tx
        .query(
            "SELECT owner_id FROM stock_picking WHERE id = $1",
            &[&picking],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(owner, Some(f.partner));
    tx.execute(
        "UPDATE stock_picking SET owner_id = NULL WHERE id = $1",
        &[&picking],
    )
    .await
    .unwrap();
    let mv = create_move(
        &mut tx,
        f.company,
        move_vals(&f, f.company).set_cell("restrict_partner_id", Cell::Id(f.partner)),
    )
    .await
    .unwrap();
    let error = delete(&mut tx).await.unwrap_err();
    assert_eq!(error.sqlstate(), Some("23001"), "{error:?}");
    assert!(
        error
            .message()
            .starts_with(&blocked("stock.move", "Stock Move")),
        "{}",
        error.message()
    );
    tx.execute(
        "UPDATE stock_move SET restrict_partner_id = NULL WHERE id = $1",
        &[&mv],
    )
    .await
    .unwrap();
    assert_eq!(delete(&mut tx).await.unwrap(), 1);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
