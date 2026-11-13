// SPDX-License-Identifier: MIT OR Apache-2.0
//! Schema of the replenishment configuration tables:
//! orderpoint and putaway rule. Each test reads columns, constraints, indexes and the relation table
//! from `pg_catalog`, the creation defaults through the ORM, and the company check of the links.
//! The behaviour (computes, scheduler, selection) is later slices'.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_stock_repl", depends = ["stock"]);

/// A test stand-in for `ir.model.data` with the columns the product addon and the company create path
/// read (the real model lives in `erp-data`, out of reach of an addon crate; the product tests use the
/// same stand-in).
#[model(
    name = "ir.model.data",
    description = "Model Data",
    addon = "test_stock_repl"
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

const PREFIX: &str = "The operation cannot be completed: ";

/// Ids every case starts from: two companies, products, warehouses, locations and storage
/// categories of each, and one package type.
struct Fixture {
    h: Harness,
    company: i64,
    product: i64,
    foreign_product: i64,
    warehouse: i64,
    foreign_warehouse: i64,
    location: i64,
    other_location: i64,
    foreign_location: i64,
    foreign_category: i64,
    foreign_route: i64,
    foreign_product_category: i64,
    foreign_package_type: i64,
    package_type: i64,
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

async fn create_in(
    tx: &mut Transaction,
    company: Option<i64>,
    model: &str,
    vals: RawVals,
) -> Result<Records> {
    let model = model.to_owned();
    tx.savepoint(async move |tx| {
        let env = match company {
            Some(company) => tx.env().with_company(company)?,
            None => tx.env(),
        };
        env.records(&model)?.create(tx, vec![vals]).await
    })
    .await
}

async fn create(tx: &mut Transaction, model: &str, vals: RawVals) -> i64 {
    create_in(tx, None, model, vals)
        .await
        .unwrap_or_else(|e| panic!("create {model}: {e:?}"))
        .rec_ids()[0]
        .get()
}

/// A storable-free product template of `company` (or shared) and its single variant.
async fn product(tx: &mut Transaction, company: i64, restricted: bool, name: &str) -> i64 {
    let mut vals = RawVals::new().set_cell("name", text(name));
    if restricted {
        vals = vals.set_cell("company_id", Cell::Id(company));
    }
    let template = create_in(tx, Some(company), "product.template", vals)
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

async fn location(tx: &mut Transaction, name: &str, company: Option<i64>) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", text(name))
        .set_cell("usage", text("internal"));
    if let Some(company) = company {
        vals = vals.set_cell("company_id", Cell::Id(company));
    }
    create(tx, "stock.location", vals).await
}

/// A warehouse of `company` with its two required locations.
async fn warehouse(tx: &mut Transaction, company: i64, name: &str, code: &str) -> i64 {
    let view = create(
        tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text(code))
            .set_cell("usage", text("view"))
            .set_cell("company_id", Cell::Id(company)),
    )
    .await;
    let stock = create(
        tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Stock"))
            .set_cell("usage", text("internal"))
            .set_cell("location_id", Cell::Id(view))
            .set_cell("company_id", Cell::Id(company)),
    )
    .await;
    create(
        tx,
        "stock.warehouse",
        RawVals::new()
            .set_cell("name", text(name))
            .set_cell("code", text(code))
            .set_cell("company_id", Cell::Id(company))
            .set_cell("view_location_id", Cell::Id(view))
            .set_cell("lot_stock_id", Cell::Id(stock)),
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
    let (company, other_company) = (companies[0], companies[1]);
    let product_id = product(&mut tx, company, false, "Widget").await;
    let foreign_product = product(&mut tx, other_company, true, "Foreign Widget").await;
    let warehouse_id = warehouse(&mut tx, company, "Main WH", "MWH").await;
    let foreign_warehouse = warehouse(&mut tx, other_company, "Other WH", "OWH").await;
    let location_id = location(&mut tx, "Bulk", None).await;
    let other_location = location(&mut tx, "Shelf", None).await;
    let foreign_location = location(&mut tx, "Foreign Shelf", Some(other_company)).await;
    let foreign_category = create(
        &mut tx,
        "stock.storage.category",
        RawVals::new()
            .set_cell("name", text("Foreign Pallets"))
            .set_cell("company_id", Cell::Id(other_company)),
    )
    .await;
    let foreign_route = create(
        &mut tx,
        "stock.route",
        RawVals::new()
            .set_cell("name", text("Foreign Route"))
            .set_cell("company_id", Cell::Id(other_company)),
    )
    .await;
    let foreign_product_category = create(
        &mut tx,
        "product.category",
        RawVals::new()
            .set_cell("name", text("Foreign Goods"))
            .set_cell("company_id", Cell::Id(other_company)),
    )
    .await;
    let foreign_package_type = create(
        &mut tx,
        "stock.package.type",
        RawVals::new()
            .set_cell("name", text("Foreign Box"))
            .set_cell("company_id", Cell::Id(other_company)),
    )
    .await;
    let package_type = create(
        &mut tx,
        "stock.package.type",
        RawVals::new().set_cell("name", text("Pallet")),
    )
    .await;
    tx.commit().await.unwrap();
    Fixture {
        h,
        company,
        product: product_id,
        foreign_product,
        warehouse: warehouse_id,
        foreign_warehouse,
        location: location_id,
        other_location,
        foreign_location,
        foreign_category,
        foreign_route,
        foreign_product_category,
        foreign_package_type,
        package_type,
    }
}

/// Column name to `(data type, nullable)`.
async fn columns(tx: &mut Transaction, table: &str) -> BTreeMap<String, (String, bool)> {
    tx.query(
        "SELECT column_name::text, data_type::text, is_nullable = 'YES' FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1",
        &[&table],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get(0), (row.get(1), row.get(2))))
    .collect()
}

/// Assert the named columns exist with this data type and nullability, and that nothing else of
/// the table is NOT NULL besides `id` and the access log columns.
async fn assert_columns(tx: &mut Transaction, table: &str, spec: &[(&str, &str, bool)]) {
    let found = columns(tx, table).await;
    for (name, data_type, nullable) in spec {
        assert_eq!(
            found.get(*name),
            Some(&((*data_type).to_owned(), *nullable)),
            "{table}.{name}: {found:?}"
        );
    }
    let required: Vec<&str> = found
        .iter()
        .filter(|(_, (_, nullable))| !nullable)
        .map(|(name, _)| name.as_str())
        .filter(|name| {
            !["id", "create_uid", "write_uid", "create_date", "write_date"].contains(name)
        })
        .collect();
    let mut expected: Vec<&str> = spec
        .iter()
        .filter(|(_, _, nullable)| !nullable)
        .map(|(name, _, _)| *name)
        .collect();
    expected.sort_unstable();
    assert_eq!(required, expected, "{table}: NOT NULL columns");
}

/// Constraint name to definition, for unique and check constraints of a table.
async fn constraints(tx: &mut Transaction, table: &str) -> BTreeMap<String, String> {
    tx.query(
        "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = $1::text::regclass AND contype IN ('u', 'c')",
        &[&table],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
    .collect()
}

/// `(column, referenced table, ON DELETE action code)` of the single-column foreign keys of `table`
/// (`c` cascade, `n` set null, `r` restrict), ordered by column.
async fn foreign_keys(tx: &mut Transaction, table: &str) -> Vec<(String, String, String)> {
    tx.query(
        "SELECT a.attname::text, c.relname::text, k.confdeltype::text FROM pg_constraint k \
         JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = k.conkey[1] \
         JOIN pg_class c ON c.oid = k.confrelid \
         WHERE k.conrelid = $1::text::regclass AND k.contype = 'f' ORDER BY a.attname",
        &[&table],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1), r.get(2)))
    .collect()
}

/// The definition of index `name`, from `USING` on (the schema prefix varies per test database).
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

/// Names of the indexes of `table` that are not the primary key or a constraint's.
async fn plain_indexes(tx: &mut Transaction, table: &str) -> Vec<String> {
    tx.query(
        "SELECT i.indexname::text FROM pg_indexes i WHERE i.schemaname = current_schema() \
         AND i.tablename = $1 AND NOT EXISTS (SELECT 1 FROM pg_constraint k \
         WHERE k.conname = i.indexname AND k.conrelid = $1::text::regclass) \
         ORDER BY i.indexname",
        &[&table],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect()
}

async fn cell(tx: &mut Transaction, record: &Records, field: &str) -> Cell {
    record.cell(tx, field).await.unwrap()
}

fn orderpoint_vals(f: &Fixture) -> RawVals {
    RawVals::new()
        .set_cell("name", text("OP/00001"))
        .set_cell("warehouse_id", Cell::Id(f.warehouse))
        .set_cell("location_id", Cell::Id(f.location))
        .set_cell("product_id", Cell::Id(f.product))
        .set_cell("company_id", Cell::Id(f.company))
}

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

/// environment comes with)
#[tokio::test]
async fn orderpoint_table_columns_indexes_and_creation_defaults() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_warehouse_orderpoint",
        &[
            ("name", "character varying", false),
            ("trigger", "character varying", false),
            ("active", "boolean", true),
            ("snoozed_until", "date", true),
            ("warehouse_id", "bigint", false),
            ("location_id", "bigint", false),
            ("product_id", "bigint", false),
            ("product_min_qty", "numeric", false),
            ("product_max_qty", "numeric", false),
            ("daily_demand", "numeric", false),
            ("min_max_based_on", "character varying", false),
            ("min_max_based_on_factor", "integer", false),
            ("replenishment_uom_id", "bigint", true),
            ("company_id", "bigint", false),
            ("route_id", "bigint", true),
            ("qty_to_order_manual", "numeric", true),
        ],
    )
    .await;
    let found = constraints(&mut tx, "stock_warehouse_orderpoint").await;
    // the business key is a declared key (S3): the registry's unique form, which
    // reads a NULL as a value (every part is required here, so it changes nothing)
    assert_eq!(
        found.get("stock_warehouse_orderpoint_product_location_check"),
        Some(&"UNIQUE NULLS NOT DISTINCT (product_id, location_id, company_id)".to_owned()),
        "{found:?}"
    );
    for finite in [
        "daily_demand",
        "product_max_qty",
        "product_min_qty",
        "qty_to_order_manual",
    ] {
        assert!(
            found.contains_key(&format!("stock_warehouse_orderpoint_finite_{finite}")),
            "{finite}: {found:?}"
        );
    }
    // the declared indexes, and the framework's default one on the two optional links; the unique
    // key leads with the product, so the product has none of its own, and no scheduler index
    // exists yet (DEFERRED_PERFORMANCE)
    let mut expected: Vec<String> = [
        "company_id",
        "location_id",
        "replenishment_uom_id",
        "route_id",
        "warehouse_id",
    ]
    .iter()
    .map(|field| format!("stock_warehouse_orderpoint__{field}_index"))
    .collect();
    expected.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        plain_indexes(&mut tx, "stock_warehouse_orderpoint").await,
        expected
    );
    let fk = |column: &str, table: &str, action: &str| {
        (column.to_owned(), table.to_owned(), action.to_owned())
    };
    assert_eq!(
        foreign_keys(&mut tx, "stock_warehouse_orderpoint").await,
        [
            fk("company_id", "res_company", "r"),
            fk("create_uid", "res_users", "n"),
            fk("location_id", "stock_location", "c"),
            fk("product_id", "product_product", "c"),
            fk("replenishment_uom_id", "uom_uom", "n"),
            fk("route_id", "stock_route", "n"),
            fk("warehouse_id", "stock_warehouse", "c"),
            fk("write_uid", "res_users", "n"),
        ]
    );
    for field in ["company_id", "location_id", "warehouse_id"] {
        assert_eq!(
            index_definition(
                &mut tx,
                &format!("stock_warehouse_orderpoint__{field}_index")
            )
            .await,
            format!("btree ({field})")
        );
    }

    let rule = create_in(
        &mut tx,
        Some(f.company),
        "stock.warehouse.orderpoint",
        orderpoint_vals(&f),
    )
    .await
    .unwrap();
    assert_eq!(cell(&mut tx, &rule, "trigger").await, text("auto"));
    assert_eq!(cell(&mut tx, &rule, "active").await, Cell::Bool(true));
    assert_eq!(
        cell(&mut tx, &rule, "min_max_based_on").await,
        text("one_month")
    );
    assert_eq!(
        cell(&mut tx, &rule, "min_max_based_on_factor").await,
        Cell::Int(100)
    );
    for field in ["product_min_qty", "product_max_qty", "daily_demand"] {
        assert_eq!(
            cell(&mut tx, &rule, field).await,
            Cell::Decimal(Decimal::ZERO),
            "{field}"
        );
    }
    assert_eq!(cell(&mut tx, &rule, "snoozed_until").await, Cell::Null);
    assert_eq!(cell(&mut tx, &rule, "route_id").await, Cell::Null);
    // the other trigger value is accepted
    rule.write(&mut tx, RawVals::new().set_cell("trigger", text("manual")))
        .await
        .unwrap();
    assert_eq!(cell(&mut tx, &rule, "trigger").await, text("manual"));
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn orderpoint_refuses_a_second_rule_for_one_product_and_location() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let first = create_in(
        &mut tx,
        Some(f.company),
        "stock.warehouse.orderpoint",
        orderpoint_vals(&f),
    )
    .await
    .unwrap();
    let error = create_in(
        &mut tx,
        Some(f.company),
        "stock.warehouse.orderpoint",
        orderpoint_vals(&f).set_cell("name", text("OP/00002")),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.message(),
        format!("{PREFIX}A replenishment rule already exists for this product on this location.")
    );
    assert_eq!(
        error.constraint(),
        Some("stock_warehouse_orderpoint_product_location_check")
    );
    // the original row is still there and is the only one
    let rows = tx
        .query("SELECT id, name FROM stock_warehouse_orderpoint", &[])
        .await
        .unwrap();
    let got: Vec<(i64, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [(first.rec_ids()[0].get(), "OP/00001".to_owned())]);
    // another location is a different key
    create_in(
        &mut tx,
        Some(f.company),
        "stock.warehouse.orderpoint",
        orderpoint_vals(&f)
            .set_cell("name", text("OP/00003"))
            .set_cell("location_id", Cell::Id(f.other_location)),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Company consistency of the orderpoint links (
/// declares `_check_company_auto` and `check_company` on warehouse, location and product; the route
/// is checked by the registry default): a rule of "Main Company" is refused with a
/// warehouse, location, product or route of "Other Company". No spec case covers the links yet, so
/// the test carries no tag.
#[tokio::test]
async fn orderpoint_links_must_share_the_rule_company() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    for (field, label, target, value) in [
        ("warehouse_id", "Warehouse", f.foreign_warehouse, "Other WH"),
        (
            "location_id",
            "Location",
            f.foreign_location,
            "Foreign Shelf",
        ),
        ("product_id", "Product", f.foreign_product, "Foreign Widget"),
        // checked by the registry default (the route has a company link)
        ("route_id", "Route", f.foreign_route, "Foreign Route"),
    ] {
        let error = create_in(
            &mut tx,
            Some(f.company),
            "stock.warehouse.orderpoint",
            orderpoint_vals(&f).set_cell(field, Cell::Id(target)),
        )
        .await
        .unwrap_err();
        assert_crossover(&error, field, label, value);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

fn default_relation(model_table: &str, comodel_table: &str) -> (String, String, String) {
    let mut tables = [model_table, comodel_table];
    tables.sort_unstable();
    (
        format!("{}_{}_rel", tables[0], tables[1]),
        format!("{model_table}_id"),
        format!("{comodel_table}_id"),
    )
}

#[tokio::test]
async fn putaway_rule_table_relation_and_creation_defaults() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    assert_columns(
        &mut tx,
        "stock_putaway_rule",
        &[
            ("product_id", "bigint", true),
            ("category_id", "bigint", true),
            ("location_in_id", "bigint", false),
            ("location_out_id", "bigint", false),
            ("sequence", "integer", true),
            ("company_id", "bigint", false),
            ("storage_category_id", "bigint", true),
            ("active", "boolean", true),
            ("sublocation", "character varying", true),
        ],
    )
    .await;
    assert!(
        constraints(&mut tx, "stock_putaway_rule").await.is_empty(),
        "no unique or check constraint"
    );
    for (field, expected) in [
        (
            "product_id",
            "btree (product_id) WHERE (product_id IS NOT NULL)",
        ),
        (
            "category_id",
            "btree (category_id) WHERE (category_id IS NOT NULL)",
        ),
        ("location_in_id", "btree (location_in_id)"),
        ("company_id", "btree (company_id)"),
    ] {
        assert_eq!(
            index_definition(&mut tx, &format!("stock_putaway_rule__{field}_index")).await,
            expected
        );
    }

    // every plain index: the declared ones and the framework's default on the links without a
    // declaration (`location_out_id`, `storage_category_id`)
    let mut expected: Vec<String> = [
        "category_id",
        "company_id",
        "location_in_id",
        "location_out_id",
        "product_id",
        "storage_category_id",
    ]
    .iter()
    .map(|field| format!("stock_putaway_rule__{field}_index"))
    .collect();
    expected.sort_unstable();
    assert_eq!(plain_indexes(&mut tx, "stock_putaway_rule").await, expected);
    let fk = |column: &str, table: &str, action: &str| {
        (column.to_owned(), table.to_owned(), action.to_owned())
    };
    assert_eq!(
        foreign_keys(&mut tx, "stock_putaway_rule").await,
        [
            fk("category_id", "product_category", "c"),
            fk("company_id", "res_company", "r"),
            fk("create_uid", "res_users", "n"),
            fk("location_in_id", "stock_location", "c"),
            fk("location_out_id", "stock_location", "c"),
            fk("product_id", "product_product", "c"),
            fk("storage_category_id", "stock_storage_category", "c"),
            fk("write_uid", "res_users", "n"),
        ]
    );

    // the relation table
    let (relation, own, other) = default_relation("stock_putaway_rule", "stock_package_type");
    assert_columns(
        &mut tx,
        &relation,
        &[(&own, "bigint", false), (&other, "bigint", false)],
    )
    .await;
    let got = foreign_keys(&mut tx, &relation).await;
    let mut expected = vec![
        (own.clone(), "stock_putaway_rule".to_owned(), "c".to_owned()),
        (
            other.clone(),
            "stock_package_type".to_owned(),
            "c".to_owned(),
        ),
    ];
    expected.sort();
    assert_eq!(got, expected);

    let rule = create_in(
        &mut tx,
        Some(f.company),
        "stock.putaway.rule",
        RawVals::new()
            .set_cell("location_in_id", Cell::Id(f.location))
            .set_cell("location_out_id", Cell::Id(f.other_location))
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell(
                "package_type_ids",
                Cell::Ids(vec![RecId::from_real(f.package_type).unwrap()].into()),
            ),
    )
    .await
    .unwrap();
    assert_eq!(cell(&mut tx, &rule, "active").await, Cell::Bool(true));
    assert_eq!(cell(&mut tx, &rule, "sublocation").await, text("no"));
    assert_eq!(cell(&mut tx, &rule, "sequence").await, Cell::Int(0));
    // a rule with neither product nor category is legal (it applies to every product)
    assert_eq!(cell(&mut tx, &rule, "product_id").await, Cell::Null);
    assert_eq!(cell(&mut tx, &rule, "category_id").await, Cell::Null);
    let rows = tx
        .query(&format!("SELECT {own}, {other} FROM {relation}"), &[])
        .await
        .unwrap();
    let pairs: Vec<(i64, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(pairs, [(rule.rec_ids()[0].get(), f.package_type)]);
    create_in(
        &mut tx,
        Some(f.company),
        "stock.putaway.rule",
        RawVals::new()
            .set_cell("location_in_id", Cell::Id(f.location))
            .set_cell("location_out_id", Cell::Id(f.other_location))
            .set_cell("company_id", Cell::Id(f.company)),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// Company consistency of the putaway rule links (
/// declares `_check_company_auto` and `check_company` on product, both locations, storage category
/// and package types; the product category is checked by the registry default): a rule of
/// "Main Company" is refused with a product, location, storage category, product category or package
/// type of "Other Company". No spec case covers the links yet, so the test carries no tag.
#[tokio::test]
async fn putaway_rule_links_must_share_the_rule_company() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let vals = || {
        RawVals::new()
            .set_cell("location_in_id", Cell::Id(f.location))
            .set_cell("location_out_id", Cell::Id(f.other_location))
            .set_cell("company_id", Cell::Id(f.company))
    };
    for (field, label, target, value) in [
        ("product_id", "Product", f.foreign_product, "Foreign Widget"),
        (
            "location_in_id",
            "When product arrives in",
            f.foreign_location,
            "Foreign Shelf",
        ),
        (
            "location_out_id",
            "Store to sublocation",
            f.foreign_location,
            "Foreign Shelf",
        ),
        (
            "storage_category_id",
            "Storage Category",
            f.foreign_category,
            "Foreign Pallets",
        ),
        // checked by the registry default (the category has a company link)
        (
            "category_id",
            "Product Category",
            f.foreign_product_category,
            "Foreign Goods",
        ),
    ] {
        let error = create_in(
            &mut tx,
            Some(f.company),
            "stock.putaway.rule",
            vals().set_cell(field, Cell::Id(target)),
        )
        .await
        .unwrap_err();
        assert_crossover(&error, field, label, value);
    }
    // the many2many is checked too (`check_company = true`, as)
    let error = create_in(
        &mut tx,
        Some(f.company),
        "stock.putaway.rule",
        vals().set_cell(
            "package_type_ids",
            Cell::Ids(vec![RecId::from_real(f.foreign_package_type).unwrap()].into()),
        ),
    )
    .await
    .unwrap_err();
    assert_crossover(&error, "package_type_ids", "Package Type", "Foreign Box");
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

/// The four orderpoint quantities are finite numbers whatever writes them (
/// S4); the sibling stock tables carry the same checks. No spec case covers the finite checks, so the
/// test carries no tag.
#[tokio::test]
async fn orderpoint_quantities_must_be_finite() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let rule = create_in(
        &mut tx,
        Some(f.company),
        "stock.warehouse.orderpoint",
        orderpoint_vals(&f),
    )
    .await
    .unwrap();
    let id = rule.rec_ids()[0].get();
    for (column, label) in [
        ("product_min_qty", "minimum"),
        ("product_max_qty", "maximum"),
        ("daily_demand", "daily demand"),
        ("qty_to_order_manual", "manual order quantity"),
    ] {
        for bad in ["NaN", "Infinity", "-Infinity"] {
            let sql = format!(
                "UPDATE stock_warehouse_orderpoint SET {column} = $1::text::numeric WHERE id = $2"
            );
            let error = tx
                .savepoint(async |tx| tx.execute(&sql, &[&bad, &id]).await)
                .await
                .expect_err(&sql);
            assert_eq!(error.sqlstate(), Some("23514"), "{column} {bad}: {error:?}");
            assert_eq!(
                error.constraint(),
                Some(format!("stock_warehouse_orderpoint_finite_{column}").as_str()),
                "{column} {bad}: {error:?}"
            );
            assert_eq!(
                error.message(),
                format!("{PREFIX}The {label} of a replenishment rule must be a finite number."),
                "{column} {bad}"
            );
        }
    }
    // a finite value, a negative one and an empty manual quantity stay legal
    tx.execute(
        "UPDATE stock_warehouse_orderpoint SET product_min_qty = -1.5, qty_to_order_manual = NULL WHERE id = $1",
        &[&id],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
