//! Shared fixtures of the `product` tests: a harness with `base`, `uom` and `product`, the seeded
//! default unit (`uom.product_uom_unit`) and a test addon that extends `product.product` the way a
//! valuation addon would.
//!
//! `erp-data`, which declares the real `ir.model.data`, is out of reach for an addon crate
//! (`scripts/check-layers.py`), so a stand-in with the same columns holds the external
//! id of the default unit. The real data install is covered by
//! `crates/erp-cli/tests/product_install.rs`.
#![allow(dead_code, reason = "each test binary uses a subset")]

use erp_addon_product::{ProductProduct, SellerQuery};
use erp_sdk::prelude::*;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_product", depends = ["product"]);

/// A test stand-in for `ir.model.data` with the columns the product addon reads.
#[model(
    name = "ir.model.data",
    description = "Model Data",
    addon = "test_product"
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
}

/// Probe cost: a later addon that supplies `standard_price` per row (Hook N6). It only answers when
/// the context key `test_cost_probe` holds a number, so the other tests see the default.
#[methods(ProductProduct, addon = "test_product")]
impl CostProbe {
    /// Exercise native seller dispatch without replacing the base eligibility implementation.
    #[method(override)]
    async fn _prepare_sellers(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
        query: SellerQuery,
        sup: Super,
    ) -> Result<Records> {
        let rows: Records = sup.call(tx, this.as_records(), (query,)).await?;
        match this.env().context().get("test_seller_probe") {
            Some(Value::Bool(true)) => Ok(rows.browse_ids([])),
            _ => Ok(rows),
        }
    }

    #[method(override)]
    async fn _get_standard_price(
        this: &RecordSet<ProductProduct>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Vec<Decimal>> {
        match this.env().context().get("test_cost_probe") {
            Some(Value::Int(cents)) => Ok(vec![Decimal::new(cents, 2); this.len()]),
            _ => sup.call(tx, this.as_records(), ()).await,
        }
    }
}

/// Extension marker type for [`CostProbe`] (the methods are attached to `product.product`).
#[extend(ProductProduct, addon = "test_product")]
pub struct CostProbe {}

/// The id of the seeded default unit.
pub async fn unit_id(h: &Harness) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT res_id FROM ir_model_data WHERE module = 'uom' AND name = 'product_uom_unit'",
            &[],
        )
        .await
        .unwrap();
    let id: i64 = rows[0].get(0);
    tx.rollback().await.unwrap();
    id
}

/// A harness with the default unit "Units" seeded under `uom.product_uom_unit`.
pub async fn base_harness() -> Harness {
    harness_with_policy(
        erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
            .build()
            .unwrap(),
    )
    .await
}

/// Product fixtures under a caller-supplied access policy.
pub async fn harness_with_policy(policy: erp_testkit::DomainPolicy) -> Harness {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &ADDON,
    ])
    .policy(policy)
    .build()
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.batch(include_str!("../../../uom/src/guard_sql.sql"))
        .await
        .unwrap();
    tx.execute("INSERT INTO uom_reference_fence(singleton) VALUES (1)", &[])
        .await
        .unwrap();
    let uom_model = tx.env().records("uom.uom").unwrap();
    let unit = uom_model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Units".into()))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::ONE)),
            ],
        )
        .await
        .unwrap();
    let data = tx.env().records("ir.model.data").unwrap();
    data.create(
        &mut tx,
        vec![
            RawVals::new()
                .set_cell("module", Cell::Str("uom".into()))
                .set_cell("name", Cell::Str("product_uom_unit".into()))
                .set_cell("model", Cell::Str("uom.uom".into()))
                .set_cell("res_id", Cell::Id(unit.rec_ids()[0].get())),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

/// The environment of the first company (a harness has no active company until one is named).
pub async fn company_env(tx: &mut Transaction) -> Env {
    let rows = tx
        .query("SELECT id FROM res_company ORDER BY id LIMIT 1", &[])
        .await
        .unwrap();
    let id: i64 = rows[0].get(0);
    tx.env().with_company(id).unwrap()
}

/// A company with a currency, for tests that need an active company.
pub async fn seed_company(h: &Harness, name: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let currency = tx.env().records("res.currency").unwrap();
    let existing = currency
        .search(&mut tx, &Domain::constant(true))
        .await
        .unwrap();
    let currency_id = if existing.is_empty() {
        currency
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
        existing.rec_ids()[0].get()
    };
    let company = tx
        .env()
        .records("res.company")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("currency_id", Cell::Id(currency_id)),
            ],
        )
        .await
        .unwrap()
        .rec_ids()[0]
        .get();
    tx.commit().await.unwrap();
    company
}

/// A many2one consumer of product display names for disclosure regressions.
#[model(
    name = "test_product.label_holder",
    description = "Product Label Holder",
    addon = "test_product"
)]
pub struct LabelHolder {
    #[field(char)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "product.product")]
    pub product_id: Option<RecId>,
}
