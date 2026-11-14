// SPDX-License-Identifier: MIT OR Apache-2.0
//! Operation-type lifecycle and policy integration tests.
//! Real ORM calls exercise defaults, numbering rows, favorites, reservation transitions and counters.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "tests and shared fixture"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_stock_optype", depends = ["stock"]);

/// A test stand-in for `ir.model.data` with the columns the product addon and the company create path
/// read (the real model lives in `erp-data`, out of reach of an addon crate; the product tests use the
/// same stand-in).
#[model(
    name = "ir.model.data",
    description = "Model Data",
    addon = "test_stock_optype"
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
const PREFIX: &str = "The database refused the change: ";

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
    fixture_with_policy(None).await
}
async fn fixture_with_policy(policy: Option<erp_testkit::FieldPolicy>) -> Fixture {
    let builder = Harness::builder([
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
    .schema_sync_options(erp_testkit::SchemaSyncOptions::inline().lifecycle(true));
    let builder = match policy {
        Some(policy) => builder.policy(policy),
        None => builder,
    };
    let h = builder.build().await.unwrap();
    seed_fixture(h).await
}
async fn seed_fixture(h: Harness) -> Fixture {
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
    // Harness supplies a system principal, not an installed active user. Favorites target an
    // actual user row, so create that prerequisite explicitly instead of dangling relation ids.
    tx.execute("INSERT INTO res_users (id,name,login,active,company_id,share) VALUES (1,'Root fixture','root-fixture',true,$1,false) ON CONFLICT (id) DO UPDATE SET active=true,company_id=$1",&[&company]).await.unwrap();
    tx.execute("INSERT INTO res_users (id,name,login,active,company_id,share) VALUES (7,'Scoped reader','scoped-reader',true,$1,false) ON CONFLICT (id) DO UPDATE SET active=true,company_id=$1",&[&company]).await.unwrap();
    tx.execute(
        "INSERT INTO res_company_users_rel(user_id,cid) VALUES(7,$1) ON CONFLICT DO NOTHING",
        &[&company],
    )
    .await
    .unwrap();
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

fn row(tx: &Transaction, model: &str, id: i64) -> Records {
    tx.env().records(model).unwrap().browse([id]).unwrap()
}
async fn update(tx: &mut Transaction, records: &Records, vals: RawVals) -> Result<()> {
    let records = records.clone();
    tx.savepoint(async move |tx| records.write(tx, vals).await)
        .await
}
async fn wh(tx: &mut Transaction, f: &Fixture) -> i64 {
    create(
        tx,
        "stock.warehouse",
        RawVals::new()
            .set_cell("name", text("Warehouse Alpha"))
            .set_cell("code", text("WHA"))
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("view_location_id", Cell::Id(f.location))
            .set_cell("lot_stock_id", Cell::Id(f.location)),
    )
    .await
}
async fn defaults_fixture(tx: &mut Transaction, f: &Fixture) -> (i64, i64, i64) {
    let warehouse = wh(tx, f).await;
    let supplier = create(
        tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Vendors"))
            .set_cell("usage", text("supplier")),
    )
    .await;
    let customer = create(
        tx,
        "stock.location",
        RawVals::new()
            .set_cell("name", text("Customers"))
            .set_cell("usage", text("customer")),
    )
    .await;
    for (name, id) in [
        ("stock_location_suppliers", supplier),
        ("stock_location_customers", customer),
    ] {
        create(
            tx,
            "ir.model.data",
            RawVals::new()
                .set_cell("module", text("stock"))
                .set_cell("name", text(name))
                .set_cell("model", text("stock.location"))
                .set_cell("res_id", Cell::Id(id)),
        )
        .await;
    }
    (warehouse, supplier, customer)
}
async fn default_case(code: &str) {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let (warehouse, supplier, customer) = defaults_fixture(&mut tx, &f).await;
    let id = create(
        &mut tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text(code))
            .set_cell("sequence", Cell::Int(10))
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("warehouse_id", Cell::Id(warehouse))
            .set_cell("code", text(code)),
    )
    .await;
    let r = row(&tx, "stock.picking.type", id);
    assert_eq!(
        r.cell(&mut tx, "default_location_src_id").await.unwrap(),
        Cell::Id(if code == "incoming" {
            supplier
        } else {
            f.location
        })
    );
    assert_eq!(
        r.cell(&mut tx, "default_location_dest_id").await.unwrap(),
        Cell::Id(if code == "outgoing" {
            customer
        } else {
            f.location
        })
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn incoming_defaults() {
    default_case("incoming").await;
}
#[tokio::test]
async fn outgoing_defaults() {
    default_case("outgoing").await;
}
#[tokio::test]
async fn internal_defaults() {
    default_case("internal").await;
}
#[tokio::test]
async fn code_change_recomputes_locations_and_explicit_override_wins() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let (warehouse, _, customer) = defaults_fixture(&mut tx, &f).await;
    let r = row(&tx, "stock.picking.type", f.picking_type);
    update(
        &mut tx,
        &r,
        RawVals::new()
            .set_cell("warehouse_id", Cell::Id(warehouse))
            .set_cell("code", text("outgoing")),
    )
    .await
    .unwrap();
    assert_eq!(
        r.cell(&mut tx, "default_location_src_id").await.unwrap(),
        Cell::Id(f.location)
    );
    assert_eq!(
        r.cell(&mut tx, "default_location_dest_id").await.unwrap(),
        Cell::Id(customer)
    );
    update(
        &mut tx,
        &r,
        RawVals::new()
            .set_cell("code", text("incoming"))
            .set_cell("default_location_src_id", Cell::Id(f.other_location)),
    )
    .await
    .unwrap();
    assert_eq!(
        r.cell(&mut tx, "default_location_src_id").await.unwrap(),
        Cell::Id(f.other_location)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn sequence_create_rename_and_independent_copy() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let warehouse = wh(&mut tx, &f).await;
    let id = create(
        &mut tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text("Custom"))
            .set_cell("sequence", Cell::Int(10))
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("warehouse_id", Cell::Id(warehouse))
            .set_cell("default_location_src_id", Cell::Id(f.location))
            .set_cell("default_location_dest_id", Cell::Id(f.other_location))
            .set_cell("sequence_code", text("CUSTOM")),
    )
    .await;
    let r = row(&tx, "stock.picking.type", id);
    let Cell::Id(sid) = r.cell(&mut tx, "sequence_id").await.unwrap() else {
        panic!("missing sequence")
    };
    let seq = row(&tx, "ir.sequence", sid);
    for (field, expected) in [
        ("name", text("CUSTOM numbering (Warehouse Alpha)")),
        ("prefix", text("CUSTOM")),
        ("padding", Cell::Int(5)),
        ("implementation", text("standard")),
        ("company_id", Cell::Id(f.company)),
    ] {
        assert_eq!(seq.cell(&mut tx, field).await.unwrap(), expected, "{field}");
    }
    update(
        &mut tx,
        &seq,
        RawVals::new()
            .set_cell("suffix", text("/X"))
            .set_cell("padding", Cell::Int(7)),
    )
    .await
    .unwrap();
    update(
        &mut tx,
        &r,
        RawVals::new().set_cell("sequence_code", text("NEWPREFIX")),
    )
    .await
    .unwrap();
    assert_eq!(
        seq.cell(&mut tx, "prefix").await.unwrap(),
        text("NEWPREFIX")
    );
    assert_eq!(
        seq.cell(&mut tx, "name").await.unwrap(),
        text("NEWPREFIX numbering (Warehouse Alpha)")
    );
    assert_eq!(seq.cell(&mut tx, "padding").await.unwrap(), Cell::Int(7));
    assert_eq!(seq.cell(&mut tx, "suffix").await.unwrap(), text("/X"));
    let copied = r.copy(&mut tx, None).await.unwrap();
    assert_eq!(
        copied.cell(&mut tx, "sequence_code").await.unwrap(),
        text("NEWPREFIX (copy)")
    );
    assert_ne!(
        copied.cell(&mut tx, "sequence_id").await.unwrap(),
        Cell::Id(sid)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn reservation_bounds_are_named_and_atomic() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let r = row(&tx, "stock.picking.type", f.picking_type);
    for field in [
        "reservation_days_before",
        "reservation_days_before_priority",
    ] {
        for days in [-1, 366] {
            let previous = r.cell(&mut tx, "reservation_method").await.unwrap();
            let previous_days = r.cell(&mut tx, field).await.unwrap();
            let err = update(
                &mut tx,
                &r,
                RawVals::new()
                    .set_cell("reservation_method", text("by_date"))
                    .set_cell(field, Cell::Int(days)),
            )
            .await
            .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Validation);
            assert_eq!(
                err.message(),
                format!("stock.picking.type.{field} must be between 0 and 365 days.")
            );
            assert_eq!(
                r.cell(&mut tx, "reservation_method").await.unwrap(),
                previous
            );
            assert_eq!(r.cell(&mut tx, field).await.unwrap(), previous_days);
        }
        for days in [0, 365] {
            update(
                &mut tx,
                &r,
                RawVals::new()
                    .set_cell("reservation_method", text("by_date"))
                    .set_cell(field, Cell::Int(days)),
            )
            .await
            .unwrap();
        }
    }
    for field in [
        "reservation_days_before",
        "reservation_days_before_priority",
    ] {
        for value in [Cell::Null, Cell::Str("wrong".into())] {
            let before = r.cell(&mut tx, field).await.unwrap();
            let err = update(&mut tx, &r, RawVals::new().set_cell(field, value))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Validation);
            assert_eq!(
                err.message(),
                format!("stock.picking.type.{field} must be between 0 and 365 days.")
            );
            assert_eq!(r.cell(&mut tx, field).await.unwrap(), before);
        }
    }
    let err = update(
        &mut tx,
        &r,
        RawVals::new().set_cell("reservation_method", text("manual")),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "stock.picking.type reservation lead times must be zero unless reservation_method is by_date."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
fn move_values(f: &Fixture, state: &str, priority: &str, pt: i64) -> RawVals {
    RawVals::new()
        .set_cell("company_id", Cell::Id(f.company))
        .set_cell("product_id", Cell::Id(f.product))
        .set_cell("uom_id", Cell::Id(f.unit))
        .set_cell("location_id", Cell::Id(f.location))
        .set_cell("location_dest_id", Cell::Id(f.other_location))
        .set_cell("product_qty", dec("0"))
        .set_cell("quantity_product_uom", dec("0"))
        .set_cell("uom_factor", dec("1"))
        .set_cell("product_uom_factor", dec("1"))
        .set_cell("product_unit_digits", Cell::Int(2))
        .set_cell("picking_type_id", Cell::Id(pt))
        .set_cell("state", text(state))
        .set_cell("priority", text(priority))
        .set_cell(
            "date",
            Cell::DateTime(DateTime::parse_odoo("2028-03-01 10:00:00").unwrap()),
        )
        .set_cell(
            "reservation_date",
            Cell::Date(Date::from_ymd(2028, 1, 1).unwrap()),
        )
}
#[tokio::test]
async fn reservation_switch_updates_open_moves_and_preserves_closed_or_assigned() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let mut moves = Vec::new();
    for state in [
        "draft",
        "confirmed",
        "waiting",
        "partially_available",
        "assigned",
        "done",
        "cancel",
    ] {
        for priority in ["0", "1"] {
            let id = create(
                &mut tx,
                "stock.move",
                move_values(&f, state, priority, f.picking_type),
            )
            .await;
            moves.push((row(&tx, "stock.move", id), state, priority));
        }
    }
    let r = row(&tx, "stock.picking.type", f.picking_type);
    update(
        &mut tx,
        &r,
        RawVals::new()
            .set_cell("reservation_method", text("by_date"))
            .set_cell("reservation_days_before", Cell::Int(3))
            .set_cell("reservation_days_before_priority", Cell::Int(0)),
    )
    .await
    .unwrap();
    for (m, state, p) in &moves {
        let expected = if ["assigned", "done", "cancel"].contains(state) {
            Date::from_ymd(2028, 1, 1).unwrap()
        } else if *p == "1" {
            Date::from_ymd(2028, 3, 1).unwrap()
        } else {
            Date::from_ymd(2028, 2, 27).unwrap()
        };
        assert_eq!(
            m.cell(&mut tx, "reservation_date").await.unwrap(),
            Cell::Date(expected),
            "{state}/{p}"
        );
    }
    update(
        &mut tx,
        &r,
        RawVals::new()
            .set_cell("reservation_method", text("manual"))
            .set_cell("reservation_days_before", Cell::Int(0))
            .set_cell("reservation_days_before_priority", Cell::Int(0)),
    )
    .await
    .unwrap();
    for (m, state, _) in &moves {
        assert_eq!(
            m.cell(&mut tx, "reservation_date").await.unwrap(),
            if ["assigned", "done", "cancel"].contains(state) {
                Cell::Date(Date::from_ymd(2028, 1, 1).unwrap())
            } else {
                Cell::Null
            }
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn favorites_are_current_user_computed_and_searchable() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let r = row(&tx, "stock.picking.type", f.picking_type);
    let uid = tx.env().uid();
    update(
        &mut tx,
        &r,
        RawVals::new().set_commands(
            "favorite_user_ids",
            [Command::Link(RecId::from_real(uid).unwrap())],
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        r.cell(&mut tx, "is_favorite").await.unwrap(),
        Cell::Bool(true)
    );
    let model = tx.env().records("stock.picking.type").unwrap();
    let found = model
        .search(&mut tx, &Domain::field("is_favorite").unwrap().eq(true))
        .await
        .unwrap();
    assert!(found.ids().contains(&f.picking_type));
    let found = model
        .search(&mut tx, &Domain::field("is_favorite").unwrap().eq(false))
        .await
        .unwrap();
    assert!(!found.ids().contains(&f.picking_type));
    update(
        &mut tx,
        &r,
        RawVals::new().set_cell("is_favorite", Cell::Bool(false)),
    )
    .await
    .unwrap();
    assert_eq!(
        r.cell(&mut tx, "is_favorite").await.unwrap(),
        Cell::Bool(false)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn basic_counter_hook_counts_transfer_states_lateness_and_backorders() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let mut first = None;
    for (state, n) in [
        ("waiting", 5),
        ("assigned", 8),
        ("draft", 3),
        ("done", 2),
        ("cancel", 1),
    ] {
        for i in 0..n {
            let mut vals = RawVals::new()
                .set_cell("name", text(&format!("{state}/{i}")))
                .set_cell("company_id", Cell::Id(f.company))
                .set_cell("picking_type_id", Cell::Id(f.picking_type))
                .set_cell("location_id", Cell::Id(f.location))
                .set_cell("location_dest_id", Cell::Id(f.other_location))
                .set_cell("state", text(state))
                .set_cell(
                    "scheduled_date",
                    Cell::DateTime(DateTime::parse_odoo("2099-01-01 00:00:00").unwrap()),
                );
            if state == "assigned" && i < 2 {
                vals = vals.set_cell("has_deadline_issue", Cell::Bool(true));
            }
            if state == "assigned" && i == 0 {
                vals = vals.set_cell("backorder_id", Cell::Id(first.unwrap()));
            }
            let id = create(&mut tx, "stock.picking", vals).await;
            if first.is_none() {
                first = Some(id);
            }
        }
    }
    let r = row(&tx, "stock.picking.type", f.picking_type);
    for (field, n) in [
        ("count_picking_waiting", 5),
        ("count_picking_ready", 8),
        ("count_picking_late", 2),
        ("count_picking_backorders", 1),
        ("count_picking_draft", 3),
        ("count_picking", 13),
    ] {
        assert_eq!(
            r.cell(&mut tx, field).await.unwrap(),
            Cell::Int(n),
            "{field}"
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn explicit_sequence_is_retained_and_failed_create_leaves_no_sequence() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let sid = create(
        &mut tx,
        "ir.sequence",
        RawVals::new()
            .set_cell("name", text("Custom sequence"))
            .set_cell("prefix", text("CUSTOM/"))
            .set_cell("padding", Cell::Int(9))
            .set_cell("company_id", Cell::Id(f.company)),
    )
    .await;
    let id = create(
        &mut tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text("Explicit"))
            .set_cell("sequence", Cell::Int(1))
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("sequence_id", Cell::Id(sid))
            .set_cell("default_location_src_id", Cell::Id(f.location))
            .set_cell("default_location_dest_id", Cell::Id(f.other_location)),
    )
    .await;
    let r = row(&tx, "stock.picking.type", id);
    assert_eq!(r.cell(&mut tx, "sequence_id").await.unwrap(), Cell::Id(sid));
    let before: i64 = tx
        .query("SELECT count(*)::int8 FROM ir_sequence", &[])
        .await
        .unwrap()[0]
        .get(0);
    let vals = RawVals::new()
        .set_cell("name", text("Invalid bound"))
        .set_cell("sequence", Cell::Int(1))
        .set_cell("company_id", Cell::Id(f.company))
        .set_cell("sequence_code", text("FAIL/"))
        .set_cell("reservation_method", text("by_date"))
        .set_cell("reservation_days_before", Cell::Int(366))
        .set_cell("default_location_src_id", Cell::Id(f.location))
        .set_cell("default_location_dest_id", Cell::Id(f.other_location));
    let err = tx
        .savepoint(async |tx| {
            tx.env()
                .records("stock.picking.type")?
                .create(tx, vec![vals])
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "stock.picking.type.reservation_days_before must be between 0 and 365 days."
    );
    let after: i64 = tx
        .query("SELECT count(*)::int8 FROM ir_sequence", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(before, after);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn favorites_cache_varies_by_user_and_mutation_preserves_other_members() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let uid: i64=tx.query("INSERT INTO res_users (id,name,login,active,company_id,share) VALUES (99,'Favorite reader','optype-favorite-reader',true,$1,false) RETURNING id::int8",&[&f.company]).await.unwrap()[0].get(0);
    tx.execute(
        "INSERT INTO res_company_users_rel(user_id,cid) VALUES($1,$2)",
        &[&uid, &f.company],
    )
    .await
    .unwrap();
    let own = row(&tx, "stock.picking.type", f.picking_type);
    let other = own.with_user(uid);
    update(
        &mut tx,
        &own,
        RawVals::new().set_cell("is_favorite", Cell::Bool(true)),
    )
    .await
    .unwrap();
    assert_eq!(
        own.cell(&mut tx, "is_favorite").await.unwrap(),
        Cell::Bool(true)
    );
    assert_eq!(
        other.cell(&mut tx, "is_favorite").await.unwrap(),
        Cell::Bool(false)
    );
    update(
        &mut tx,
        &other,
        RawVals::new().set_cell("is_favorite", Cell::Bool(true)),
    )
    .await
    .unwrap();
    assert_eq!(
        own.cell(&mut tx, "is_favorite").await.unwrap(),
        Cell::Bool(true)
    );
    assert_eq!(
        other.cell(&mut tx, "is_favorite").await.unwrap(),
        Cell::Bool(true)
    );
    for _ in 0..2 {
        update(
            &mut tx,
            &own,
            RawVals::new().set_cell("is_favorite", Cell::Bool(false)),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        own.cell(&mut tx, "is_favorite").await.unwrap(),
        Cell::Bool(false)
    );
    assert_eq!(
        other.cell(&mut tx, "is_favorite").await.unwrap(),
        Cell::Bool(true)
    );
    for (op, values, expected) in [
        (Operator::In, vec![true], false),
        (Operator::NotIn, vec![true], true),
        (Operator::In, vec![true, false], true),
        (Operator::In, vec![], false),
    ] {
        let found = own
            .search(
                &mut tx,
                &Domain::field("is_favorite").unwrap().op(op, values),
            )
            .await
            .unwrap();
        assert_eq!(found.ids().contains(&f.picking_type), expected);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn denied_field_precedes_side_effects_and_dependent_write_failure_rolls_back() {
    let policy = erp_testkit::FieldPolicy::new()
        .deny("stock.picking.type", "sequence_code", FieldOp::Write)
        .deny("stock.move", "reservation_date", FieldOp::Write);
    let f = fixture_with_policy(Some(policy)).await;
    let mut tx = f.h.begin().await.unwrap();
    let r = row(&tx, "stock.picking.type", f.picking_type).with_user(7);
    let error = update(
        &mut tx,
        &r,
        RawVals::new().set_cell("sequence_code", Cell::Int(42)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Writing stock.picking.type.sequence_code is not allowed."
    );
    let mid = create(
        &mut tx,
        "stock.move",
        move_values(&f, "confirmed", "0", f.picking_type),
    )
    .await;
    let error = update(
        &mut tx,
        &r,
        RawVals::new()
            .set_cell("reservation_method", text("by_date"))
            .set_cell("reservation_days_before", Cell::Int(3)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"reservation_date\" on Stock Move (stock.move). Please contact your system administrator.\n\nOperation: write"
    );
    let root = row(&tx, "stock.picking.type", f.picking_type);
    assert_eq!(
        root.cell(&mut tx, "reservation_method").await.unwrap(),
        text("at_confirm")
    );
    assert_eq!(
        root.cell(&mut tx, "reservation_days_before").await.unwrap(),
        Cell::Int(0)
    );
    assert_eq!(
        row(&tx, "stock.move", mid)
            .cell(&mut tx, "reservation_date")
            .await
            .unwrap(),
        Cell::Date(Date::from_ymd(2028, 1, 1).unwrap())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
async fn restricted_fixture() -> Fixture {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "stock.move",
            erp_sdk::Operation::Read,
            Domain::field("priority").unwrap().eq("0"),
        )
        .rule(
            "stock.picking",
            erp_sdk::Operation::Read,
            Domain::field("name").unwrap().ne("Hidden transfer"),
        )
        .build()
        .unwrap();
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
    .policy(policy)
    .build()
    .await
    .unwrap();
    seed_fixture(h).await
}
#[tokio::test]
async fn hidden_moves_refuse_rescheduling_without_partial_policy_change() {
    let f = restricted_fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    create(
        &mut tx,
        "stock.move",
        move_values(&f, "confirmed", "1", f.picking_type),
    )
    .await;
    let r = row(&tx, "stock.picking.type", f.picking_type).with_user(7);
    let err = update(
        &mut tx,
        &r,
        RawVals::new().set_cell("reservation_method", text("by_date")),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err:?}");
    assert_eq!(
        err.message(),
        "You cannot change this reservation method because some affected stock moves are not accessible."
    );
    assert_eq!(
        row(&tx, "stock.picking.type", f.picking_type)
            .cell(&mut tx, "reservation_method")
            .await
            .unwrap(),
        text("at_confirm")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn counters_respect_transfer_record_rules_and_invalidate_after_writes() {
    let f = restricted_fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    for name in ["Visible transfer", "Hidden transfer"] {
        create(
            &mut tx,
            "stock.picking",
            RawVals::new()
                .set_cell("name", text(name))
                .set_cell("company_id", Cell::Id(f.company))
                .set_cell("picking_type_id", Cell::Id(f.picking_type))
                .set_cell("location_id", Cell::Id(f.location))
                .set_cell("location_dest_id", Cell::Id(f.other_location))
                .set_cell("state", text("assigned")),
        )
        .await;
    }
    let all = row(&tx, "stock.picking.type", f.picking_type);
    let caller = all.with_user(7);
    assert_eq!(
        all.cell(&mut tx, "count_picking_ready").await.unwrap(),
        Cell::Int(2)
    );
    assert_eq!(
        caller.cell(&mut tx, "count_picking_ready").await.unwrap(),
        Cell::Int(1)
    );
    let p = tx
        .env()
        .records("stock.picking")
        .unwrap()
        .search(
            &mut tx,
            &Domain::field("name").unwrap().eq("Visible transfer"),
        )
        .await
        .unwrap();
    update(&mut tx, &p, RawVals::new().set_cell("state", text("done")))
        .await
        .unwrap();
    assert_eq!(
        caller.cell(&mut tx, "count_picking_ready").await.unwrap(),
        Cell::Int(0)
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn independent_user_favorite_writes_preserve_both_memberships() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    for uid in [77_i64, 78] {
        tx.execute("INSERT INTO res_users(id,name,login,active,company_id,share) VALUES($1,$2,$2,true,$3,false)",&[&uid,&format!("favorite-{uid}"),&f.company]).await.unwrap();
        tx.execute(
            "INSERT INTO res_company_users_rel(user_id,cid) VALUES($1,$2)",
            &[&uid, &f.company],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    drop(tx);
    let id = f.picking_type;
    let orm1 = f.h.orm().clone();
    let orm2 = f.h.orm().clone();
    let context = erp_sdk::Context::new();
    let a = orm1.run(
        erp_sdk::UserId::new(77).unwrap(),
        context.clone(),
        async |tx| {
            tx.env()
                .records("stock.picking.type")?
                .browse([id])?
                .write(tx, RawVals::new().set_cell("is_favorite", Cell::Bool(true)))
                .await
        },
    );
    let b = orm2.run(erp_sdk::UserId::new(78).unwrap(), context, async |tx| {
        tx.env()
            .records("stock.picking.type")?
            .browse([id])?
            .write(tx, RawVals::new().set_cell("is_favorite", Cell::Bool(true)))
            .await
    });
    let (a, b) = tokio::join!(a, b);
    a.unwrap();
    b.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    let rows=tx.query("SELECT user_id::int8 FROM picking_type_favorite_user_rel WHERE picking_type_id=$1 ORDER BY user_id",&[&id]).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.get::<_, i64>(0)).collect::<Vec<_>>(),
        vec![77, 78]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn batch_defaults_stay_with_each_company_and_explicit_values() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let (wa, supplier, customer) = defaults_fixture(&mut tx, &f).await;
    let wb = create(
        &mut tx,
        "stock.warehouse",
        RawVals::new()
            .set_cell("name", text("Other warehouse"))
            .set_cell("code", text("OW"))
            .set_cell("company_id", Cell::Id(f.other_company))
            .set_cell("view_location_id", Cell::Id(f.other_location))
            .set_cell("lot_stock_id", Cell::Id(f.other_location)),
    )
    .await;
    let values = vec![
        RawVals::new()
            .set_cell("name", text("Batch receipt"))
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("code", text("incoming")),
        RawVals::new()
            .set_cell("name", text("Batch delivery"))
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("code", text("outgoing")),
        RawVals::new()
            .set_cell("name", text("Batch internal"))
            .set_cell("company_id", Cell::Id(f.other_company))
            .set_cell("warehouse_id", Cell::Id(wb))
            .set_cell("code", text("internal")),
    ];
    let types = tx
        .env()
        .records("stock.picking.type")
        .unwrap()
        .create(&mut tx, values)
        .await
        .unwrap();
    assert_eq!(
        types.cells(&mut tx, "warehouse_id").await.unwrap(),
        vec![Cell::Id(wa), Cell::Id(wa), Cell::Id(wb)]
    );
    assert_eq!(
        types
            .cells(&mut tx, "default_location_src_id")
            .await
            .unwrap(),
        vec![
            Cell::Id(supplier),
            Cell::Id(f.location),
            Cell::Id(f.other_location)
        ]
    );
    assert_eq!(
        types
            .cells(&mut tx, "default_location_dest_id")
            .await
            .unwrap(),
        vec![
            Cell::Id(f.location),
            Cell::Id(customer),
            Cell::Id(f.other_location)
        ]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
#[tokio::test]
async fn later_create_failure_rolls_back_generated_sequence() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let before: i64 = tx
        .query("SELECT count(*)::int8 FROM ir_sequence", &[])
        .await
        .unwrap()[0]
        .get(0);
    let vals = RawVals::new()
        .set_cell("name", text("Later failure"))
        .set_cell("company_id", Cell::Id(f.company))
        .set_cell("default_location_src_id", Cell::Id(f.location))
        .set_cell("default_location_dest_id", Cell::Id(f.other_location))
        .set_cell("sequence_code", text("FAIL/"))
        .set_cell("bogus", Cell::Int(1));
    let error = tx
        .savepoint(async |tx| {
            tx.env()
                .records("stock.picking.type")?
                .create(tx, vec![vals])
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Invalid field 'bogus' in 'stock.picking.type'"
    );
    let after: i64 = tx
        .query("SELECT count(*)::int8 FROM ir_sequence", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(before, after);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn unrelated_edits_do_not_require_reservation_field_read_access() {
    let policy = erp_testkit::FieldPolicy::new()
        .deny("stock.picking.type", "reservation_method", FieldOp::Read)
        .deny(
            "stock.picking.type",
            "reservation_days_before",
            FieldOp::Read,
        )
        .deny(
            "stock.picking.type",
            "reservation_days_before_priority",
            FieldOp::Read,
        );
    let f = fixture_with_policy(Some(policy)).await;
    let mut tx = f.h.begin().await.unwrap();
    let caller = row(&tx, "stock.picking.type", f.picking_type).with_user(7);
    update(
        &mut tx,
        &caller,
        RawVals::new().set_cell("name", text("Renamed operation")),
    )
    .await
    .unwrap();
    assert_eq!(
        caller.cell(&mut tx, "name").await.unwrap(),
        text("Renamed operation")
    );
    let error = caller
        .cell(&mut tx, "reservation_method")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"reservation_method\" on Picking Type (stock.picking.type). Please contact your system administrator.\n\nOperation: read"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

async fn fixture_with_domains(policy: erp_testkit::DomainPolicy) -> Fixture {
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
    .policy(policy)
    .build()
    .await
    .unwrap();
    seed_fixture(h).await
}

#[tokio::test]
async fn default_prerequisites_fail_by_name_without_orphan_sequences() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let vals = RawVals::new()
        .set_cell("name", text("Missing prerequisites"))
        .set_cell("company_id", Cell::Id(f.company))
        .set_cell("sequence_code", text("MISSING/"));
    let model = tx.env().records("stock.picking.type").unwrap();
    let error = model.create(&mut tx, vec![vals.clone()]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "No warehouse is available for the operation type company; warehouse provisioning is not supported until a later release."
    );
    let warehouse = wh(&mut tx, &f).await;
    let error = model
        .create(
            &mut tx,
            vec![vals.set_cell("warehouse_id", Cell::Id(warehouse))],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Required stock location stock.stock_location_suppliers is missing; install or repair Inventory default data."
    );
    let count: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM ir_sequence WHERE prefix='MISSING/'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_lookups_retain_warehouse_and_location_record_rules() {
    for denied in ["stock.warehouse", "stock.location"] {
        let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
            .deny(denied, erp_sdk::Operation::Read)
            .build()
            .unwrap();
        let f = fixture_with_domains(policy).await;
        let mut tx = f.h.begin().await.unwrap();
        let (warehouse, _, _) = defaults_fixture(&mut tx, &f).await;
        let caller = tx.env().records("stock.picking.type").unwrap().with_user(7);
        let error = caller
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", text("Denied default"))
                        .set_cell("company_id", Cell::Id(f.company))
                        .set_cell("warehouse_id", Cell::Id(warehouse))
                        .set_cell("sequence_code", text("DENIED/")),
                ],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
        assert!(error.message().contains(denied), "{}", error.message());
        let count: i64 = tx
            .query(
                "SELECT count(*)::int8 FROM ir_sequence WHERE prefix='DENIED/'",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(count, 0);
        tx.rollback().await.unwrap();
        drop(tx);
        f.h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn sequence_copy_overrides_and_warehouse_free_configuration_are_explicit() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let id = create(
        &mut tx,
        "stock.picking.type",
        RawVals::new()
            .set_cell("name", text("Warehouse free"))
            .set_cell("company_id", Cell::Id(f.company))
            .set_cell("default_location_src_id", Cell::Id(f.location))
            .set_cell("default_location_dest_id", Cell::Id(f.other_location))
            .set_cell("sequence_code", text("FREE/")),
    )
    .await;
    let original = row(&tx, "stock.picking.type", id);
    let Cell::Id(sid) = original.cell(&mut tx, "sequence_id").await.unwrap() else {
        panic!("no sequence")
    };
    assert_eq!(
        row(&tx, "ir.sequence", sid)
            .cell(&mut tx, "name")
            .await
            .unwrap(),
        text("FREE/ numbering")
    );
    let mut defaults = erp_sdk::ValueMap::new();
    defaults.insert("sequence_id".into(), erp_sdk::Value::Int(sid));
    let linked = original.copy(&mut tx, Some(defaults)).await.unwrap();
    assert_eq!(
        linked.cell(&mut tx, "sequence_id").await.unwrap(),
        Cell::Id(sid)
    );
    assert_eq!(
        linked.cell(&mut tx, "sequence_code").await.unwrap(),
        text("FREE/")
    );
    let mut defaults = erp_sdk::ValueMap::new();
    defaults.insert("sequence_code".into(), erp_sdk::Value::str("OVERRIDE/"));
    let independent = original.copy(&mut tx, Some(defaults)).await.unwrap();
    assert_eq!(
        independent.cell(&mut tx, "sequence_code").await.unwrap(),
        text("OVERRIDE/")
    );
    assert_ne!(
        independent.cell(&mut tx, "sequence_id").await.unwrap(),
        Cell::Id(sid)
    );
    update(
        &mut tx,
        &original,
        RawVals::new()
            .set_cell("sequence_code", text("RETAIN/"))
            .set_cell("sequence_id", Cell::Null),
    )
    .await
    .unwrap();
    assert_eq!(
        original.cell(&mut tx, "sequence_id").await.unwrap(),
        Cell::Id(sid)
    );
    assert_eq!(
        row(&tx, "ir.sequence", sid)
            .cell(&mut tx, "prefix")
            .await
            .unwrap(),
        text("RETAIN/")
    );
    let no_sequence = row(&tx, "stock.picking.type", f.picking_type);
    let error = update(
        &mut tx,
        &no_sequence,
        RawVals::new().set_cell("sequence_code", text("NO-LINK/")),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Set a reference sequence before editing an operation type sequence prefix."
    );
    let other_type = row(&tx, "stock.picking.type", f.other_picking_type);
    let error = update(
        &mut tx,
        &other_type,
        RawVals::new().set_cell("sequence_id", Cell::Id(sid)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert!(error.message().contains("company"), "{}", error.message());
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn reservation_dates_cross_years_and_remain_scoped_to_each_type() {
    let f = fixture().await;
    let mut tx = f.h.begin().await.unwrap();
    let foreign_move = create(
        &mut tx,
        "stock.move",
        move_values(&f, "confirmed", "0", f.other_picking_type)
            .set_cell("company_id", Cell::Id(f.other_company)),
    )
    .await;
    let second = create_picking_type(
        &mut tx,
        f.company,
        "Second policy",
        f.location,
        f.other_location,
    )
    .await;
    let mut moves = Vec::new();
    for (type_id, normal, priority) in [(f.picking_type, 2, 7), (second, 5, 1)] {
        for p in ["0", "1"] {
            let id = create(
                &mut tx,
                "stock.move",
                move_values(&f, "confirmed", p, type_id).set_cell(
                    "date",
                    Cell::DateTime(DateTime::parse_odoo("2028-01-01 11:30:00").unwrap()),
                ),
            )
            .await;
            moves.push((id, if p == "0" { normal } else { priority }));
        }
        let operation_type = row(&tx, "stock.picking.type", type_id);
        update(
            &mut tx,
            &operation_type,
            RawVals::new()
                .set_cell("reservation_method", text("by_date"))
                .set_cell("reservation_days_before", Cell::Int(normal))
                .set_cell("reservation_days_before_priority", Cell::Int(priority)),
        )
        .await
        .unwrap();
    }
    for (id, days) in moves {
        let expected = match days {
            1 => Date::from_ymd(2027, 12, 31),
            2 => Date::from_ymd(2027, 12, 30),
            5 => Date::from_ymd(2027, 12, 27),
            7 => Date::from_ymd(2027, 12, 25),
            _ => unreachable!(),
        }
        .unwrap();
        assert_eq!(
            row(&tx, "stock.move", id)
                .cell(&mut tx, "reservation_date")
                .await
                .unwrap(),
            Cell::Date(expected)
        );
    }
    assert_eq!(
        row(&tx, "stock.picking.type", f.other_picking_type)
            .cell(&mut tx, "reservation_method")
            .await
            .unwrap(),
        text("at_confirm")
    );
    assert_eq!(
        row(&tx, "stock.move", foreign_move)
            .cell(&mut tx, "reservation_date")
            .await
            .unwrap(),
        Cell::Date(Date::from_ymd(2028, 1, 1).unwrap())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn hidden_favorites_cannot_be_read_searched_or_changed() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .deny("stock.picking.type", erp_sdk::Operation::Read)
        .deny("stock.picking.type", erp_sdk::Operation::Write)
        .build()
        .unwrap();
    let f = fixture_with_domains(policy).await;
    let mut tx = f.h.begin().await.unwrap();
    let root = row(&tx, "stock.picking.type", f.picking_type);
    update(
        &mut tx,
        &root,
        RawVals::new().set_commands(
            "favorite_user_ids",
            [Command::Link(RecId::from_real(7).unwrap())],
        ),
    )
    .await
    .unwrap();
    let caller = root.with_user(7);
    let error = caller.cell(&mut tx, "is_favorite").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert!(
        error.message().contains("stock.picking.type"),
        "{}",
        error.message()
    );
    let error = caller
        .search(&mut tx, &Domain::field("is_favorite").unwrap().eq(true))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you read 'Picking Type' (stock.picking.type) records."
    );
    let error = update(
        &mut tx,
        &caller,
        RawVals::new().set_cell("is_favorite", Cell::Bool(false)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert!(
        error.message().contains("stock.picking.type"),
        "{}",
        error.message()
    );
    assert_eq!(
        root.cell(&mut tx, "favorite_user_ids").await.unwrap(),
        Cell::Ids(vec![RecId::from_real(7).unwrap()].into())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn counters_batch_types_empty_groups_past_dates_and_company_rules() {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "stock.picking",
            erp_sdk::Operation::Read,
            Domain::field("company_id.name").unwrap().eq("Main Company"),
        )
        .build()
        .unwrap();
    let f = fixture_with_domains(policy).await;
    let mut tx = f.h.begin().await.unwrap();
    for (type_id, company, n) in [
        (f.picking_type, f.company, 2),
        (f.other_picking_type, f.other_company, 3),
    ] {
        for i in 0..n {
            create(
                &mut tx,
                "stock.picking",
                RawVals::new()
                    .set_cell("name", text(&format!("company {company}/{i}")))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("picking_type_id", Cell::Id(type_id))
                    .set_cell("location_id", Cell::Id(f.location))
                    .set_cell("location_dest_id", Cell::Id(f.other_location))
                    .set_cell("state", text("confirmed"))
                    .set_cell(
                        "scheduled_date",
                        Cell::DateTime(DateTime::parse_odoo("1970-01-01 12:00:00").unwrap()),
                    ),
            )
            .await;
        }
    }
    let types = tx
        .env()
        .records("stock.picking.type")
        .unwrap()
        .browse([f.picking_type, f.other_picking_type])
        .unwrap();
    assert_eq!(
        types.cells(&mut tx, "count_picking_late").await.unwrap(),
        vec![Cell::Int(2), Cell::Int(3)]
    );
    assert_eq!(
        types
            .with_user(7)
            .cells(&mut tx, "count_picking_late")
            .await
            .unwrap(),
        vec![Cell::Int(2), Cell::Int(0)]
    );
    assert_eq!(
        types.cells(&mut tx, "count_picking_draft").await.unwrap(),
        vec![Cell::Int(0), Cell::Int(0)]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn counter_provider_field_denial_is_not_a_zero_fallback() {
    let policy =
        erp_testkit::FieldPolicy::new().deny("stock.picking", "picking_type_id", FieldOp::Read);
    let f = fixture_with_policy(Some(policy)).await;
    let mut tx = f.h.begin().await.unwrap();
    let caller = row(&tx, "stock.picking.type", f.picking_type).with_user(7);
    let error = caller
        .cell(&mut tx, "count_picking_ready")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"picking_type_id\" on Transfer (stock.picking). Please contact your system administrator.\n\nOperation: read"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
