//! Dataset checksums are computed over these tables by `bench_gen.table_checksums()`.

use erp_sdk::DefaultContext;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "erp_bench");

/// `bench.tag`.
#[model(
    name = "bench.tag",
    description = "Bench Tag",
    order = "name, id",
    addon = "erp_bench",
    unique_indexes = [("_code_unique", "(code)", "The code of a bench tag must be unique.")]
)]
pub struct Tag {
    #[field(char, required)]
    pub code: Option<Arc<str>>,
    #[field(char, required)]
    pub name: Option<Arc<str>>,
}

/// `bench.partner`.
#[model(
    name = "bench.partner",
    description = "Bench Partner",
    order = "name, id",
    addon = "erp_bench",
    unique_indexes = [("_code_unique", "(code)", "The code of a bench partner must be unique.")]
)]
pub struct Partner {
    #[field(char, required)]
    pub code: Option<Arc<str>>,
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(char, index)]
    pub r#ref: Option<Arc<str>>,
    #[field(
        selection,
        selection = [
            ("id", "Indonesia"), ("sg", "Singapore"), ("my", "Malaysia"), ("th", "Thailand"),
            ("vn", "Vietnam"), ("ph", "Philippines"), ("jp", "Japan"), ("de", "Germany"),
            ("fr", "France"), ("us", "United States")
        ]
    )]
    pub country: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
}

fn now(_ctx: &DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}

/// `bench.order`: `amount_total = sum(line_ids.price_subtotal)` stored.
#[model(
    name = "bench.order",
    description = "Bench Order",
    order = "date_order desc, id desc",
    addon = "erp_bench",
    unique_indexes = [("_code_unique", "(code)", "The code of a bench order must be unique.")]
)]
pub struct Order {
    #[field(char, required)]
    pub code: Option<Arc<str>>,
    #[field(char, required)]
    pub name: Option<Arc<str>>,
    #[field(many2one, comodel = "bench.partner", required, index)]
    pub partner_id: Many2one<Partner>,
    #[field(char, related = "partner_id.name")]
    pub partner_name: Option<Arc<str>>,
    #[field(datetime, default_fn = now)]
    pub date_order: Option<DateTime>,
    #[field(
        selection,
        selection = [
            ("draft", "Draft"), ("confirmed", "Confirmed"), ("done", "Done"), ("cancel", "Cancelled")
        ],
        default = "draft",
        required
    )]
    pub state: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "bench.tag",
        relation = "bench_order_bench_tag_rel",
        column1 = "bench_order_id",
        column2 = "bench_tag_id"
    )]
    pub tag_ids: X2many<Tag>,
    #[field(one2many, comodel = "bench.order.line", inverse_name = "order_id")]
    pub line_ids: X2many<OrderLine>,
    #[field(float, compute = "_compute_amount_total", store)]
    pub amount_total: f64,
}

#[methods(Order, addon = "erp_bench")]
impl Order {
    #[compute(depends = ["line_ids.price_subtotal"])]
    fn _compute_amount_total(batch: &mut Batch<'_, Order>) -> Result<()> {
        for i in batch.indices() {
            let lines = batch.get(i, Order::LINE_IDS)?;
            let total: f64 = batch
                .values_of(OrderLine::PRICE_SUBTOTAL, lines.ids())?
                .iter()
                .sum();
            batch.set(i, Order::AMOUNT_TOTAL, total)?;
        }
        Ok(())
    }
}

/// `bench.order.line`: `price_subtotal = qty * price_unit` stored.
#[model(
    name = "bench.order.line",
    description = "Bench Order Line",
    order = "order_id, id",
    addon = "erp_bench",
    unique_indexes = [("_code_unique", "(code)", "The code of a bench order line must be unique.")],
    constraints = [(
        "_qty_non_negative",
        "CHECK (qty >= 0)",
        "The quantity of an order line cannot be negative."
    )]
)]
pub struct OrderLine {
    #[field(char, required)]
    pub code: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "bench.order",
        required,
        ondelete = "cascade",
        index
    )]
    pub order_id: Many2one<Order>,
    #[field(char, required)]
    pub product_name: Option<Arc<str>>,
    #[field(float, default = 1.0)]
    pub qty: f64,
    #[field(float)]
    pub price_unit: f64,
    #[field(float, compute = "_compute_price_subtotal", store)]
    pub price_subtotal: f64,
}

#[methods(OrderLine, addon = "erp_bench")]
impl OrderLine {
    #[compute(depends = ["qty", "price_unit"])]
    fn _compute_price_subtotal(batch: &mut Batch<'_, OrderLine>) -> Result<()> {
        for i in batch.indices() {
            let qty = batch.get(i, OrderLine::QTY)?;
            let price = batch.get(i, OrderLine::PRICE_UNIT)?;
            batch.set(i, OrderLine::PRICE_SUBTOTAL, qty * price)?;
        }
        Ok(())
    }
}
