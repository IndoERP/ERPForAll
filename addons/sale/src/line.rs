// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_product::{ProductProduct, ProductTemplate};
use erp_sdk::prelude::*;

use crate::order::{Header, ProtectedFields, given, given_id, headers, refuse_context_defaults};
use crate::product::ProductTemplateSale;

/// The kinds of a line without product.
pub const DISPLAY_TYPES: [&str; 3] = ["line_section", "line_subsection", "line_note"];

/// The line fields only the ORM and the operations set: the stored copies of the order's fields,
/// the down-payment flag the guards read, and the invoicing method, quantities and status. Refused
/// on create and write for every caller, also with sudo, through the order's line commands and as
/// `default_<field>` context keys.
const DERIVED_FIELDS: [&str; 10] = [
    "state",
    "company_id",
    "order_partner_id",
    "salesman_id",
    "is_downpayment",
    "invoice_item_link_ids",
    "qty_delivered_method",
    "qty_invoiced",
    "qty_to_invoice",
    "invoice_status",
];

/// Refuse a value for one of the [`DERIVED_FIELDS`].
fn refuse_derived(tx: &Transaction, vals: &RawVals) -> Result<()> {
    match DERIVED_FIELDS.iter().find(|field| vals.contains(field)) {
        Some(field) => Err(Error::validation(format!(
            "The field \"{}\" of a sales order line is set from its order and its operations, not written.",
            crate::order::label_of(tx, "sale.order.line", field)?
        ))),
        None => Ok(()),
    }
}

/// Refuse a change to the lines of a cancelled order, and, when `locked_too`, of a locked one
/// (V13). `action` completes "its lines cannot be ...".
fn refuse_closed(orders: &[Header], action: &str, locked_too: bool) -> Result<()> {
    for order in orders {
        let why = if order.state == "cancel" {
            "cancelled"
        } else if locked_too && order.locked {
            "locked"
        } else {
            continue;
        };
        return Err(Error::validation(format!(
            "The sales order {} is {why}; its lines cannot be {action}.",
            order.name
        )));
    }
    Ok(())
}

/// The orders of `lines`.
async fn line_orders(tx: &mut Transaction, env: &Env, lines: &[LineFacts]) -> Result<Vec<Header>> {
    let orders: BTreeSet<RecId> = lines.iter().filter_map(|line| line.order).collect();
    headers(tx, &env.records("sale.order")?.browse_ids(orders)).await
}

/// The sale display name of each product of `products`.
async fn product_names(
    tx: &mut Transaction,
    env: &Env,
    products: impl IntoIterator<Item = RecId>,
) -> Result<BTreeMap<RecId, Option<Arc<str>>>> {
    let variants = RecordSet::<ProductProduct>::try_from(
        env.records("product.product")?.browse_ids(products),
    )?;
    Ok(variants
        .as_records()
        .rec_ids()
        .iter()
        .copied()
        .zip(variants.mapped(tx, Base::DISPLAY_NAME).await?)
        .collect())
}

/// `sale.order.line`.
#[model(
    name = "sale.order.line",
    description = "Sales Order Line",
    order = "order_id, sequence, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny", parent = "order_id"),
    constraints = [
        (
            "_accountable_required_fields",
            "CHECK(display_type IS NOT NULL OR product_uom_id IS NOT NULL)",
            "A product or down-payment line needs a unit."
        ),
        (
            "_non_accountable_null_fields",
            "CHECK(display_type IS NULL OR (product_id IS NULL AND product_uom_id IS NULL AND price_unit = 0 AND product_uom_qty = 0 AND customer_lead = 0))",
            "A section or note line has no product, unit, price, quantity or lead time."
        ),
        (
            "_customer_lead_positive",
            "CHECK(customer_lead >= 0)",
            "The lead time of a line is a number of days, zero or more."
        ),
    ],
)]
pub struct SaleOrderLine {
    /// The order; the line is deleted with it.
    #[field(
        many2one,
        comodel = "sale.order",
        required = true,
        ondelete = "cascade",
        index = true,
        copy = false,
        check_company = false,
        string = "Order Reference"
    )]
    pub order_id: Option<RecId>,
    /// Position on the order.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// The order's company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "order_id.company_id",
        store = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// The order's customer.
    #[field(
        many2one,
        comodel = "res.partner",
        related = "order_id.partner_id",
        store = true,
        string = "Customer"
    )]
    pub order_partner_id: Option<RecId>,
    /// The order's salesperson.
    #[field(
        many2one,
        comodel = "res.users",
        related = "order_id.user_id",
        store = true,
        string = "Salesperson"
    )]
    pub salesman_id: Option<RecId>,
    /// The order's state.
    #[field(
        selection,
        selection = [
            ("draft", "Quotation"),
            ("sent", "Quotation Sent"),
            ("sale", "Sales Order"),
            ("cancel", "Cancelled"),
        ],
        related = "order_id.state",
        store = true,
        string = "Order Status"
    )]
    pub state: Option<Arc<str>>,
    /// Empty for a product line.
    #[field(
        selection,
        selection = [
            ("line_section", "Section"),
            ("line_subsection", "Subsection"),
            ("line_note", "Note"),
        ],
        string = "Display Type"
    )]
    pub display_type: Option<Arc<str>>,
    /// A down-payment line; set by the down-payment slice, read by the guards.
    #[field(boolean, default = false, copy = false, string = "Is a down payment")]
    pub is_downpayment: bool,
    /// The product variant.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// The description; the product's name when a product is chosen.
    #[field(
        text,
        required = true,
        compute = "_compute_name",
        store = true,
        readonly = false,
        string = "Description"
    )]
    pub name: Option<Arc<str>>,
    /// The ordered quantity; zero on section and note lines.
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 1,
        string = "Quantity"
    )]
    pub product_uom_qty: Decimal,
    /// The unit; the product's unit when a product is chosen.
    #[field(
        many2one,
        comodel = "uom.uom",
        compute = "_compute_product_uom_id",
        store = true,
        readonly = false,
        ondelete = "restrict",
        string = "Unit"
    )]
    pub product_uom_id: Option<RecId>,
    /// The units the product is sold in: its unit, plus its other units when several units are
    /// enabled (`product.product._get_available_uoms`).
    #[field(
        many2many,
        comodel = "uom.uom",
        compute = "_compute_allowed_uom_ids",
        string = "Allowed Units"
    )]
    pub allowed_uom_ids: Vec<RecId>,
    /// The unit price: from the order's pricelist or the product's sales price, editable.
    #[field(
        float,
        digits = "Product Price",
        required = true,
        default = 0,
        string = "Unit Price"
    )]
    pub price_unit: Decimal,
    /// The price the line was last priced at; differs from the unit price once a person set it.
    /// The default 0 lets the line form send it with the untouched default price (see `fill_line`).
    #[field(
        float,
        digits = "Product Price",
        copy = false,
        default = 0,
        string = "Technical Price Unit"
    )]
    pub technical_price_unit: Decimal,
    /// Discount in percent, applied once to the subtotal.
    #[field(float, digits = "Discount", default = 0, string = "Discount (%)")]
    pub discount: Decimal,
    /// The taxes; the product's sales taxes mapped by the order's fiscal position when not given.
    #[field(
        many2many,
        comodel = "account.tax",
        relation = "account_tax_sale_order_line_rel",
        column1 = "sale_order_line_id",
        column2 = "account_tax_id",
        check_company = true,
        string = "Taxes"
    )]
    pub tax_ids: Vec<RecId>,
    /// Optional Profit Center / Cost Center propagated to customer invoice lines.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "set null",
        index = "btree_not_null",
        string = "Profit / Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// The order's currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "order_id.currency_id",
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Amount without taxes, after the discount, in the order currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Subtotal"
    )]
    pub price_subtotal: Decimal,
    /// The taxes of the line, in the order currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Total Tax"
    )]
    pub price_tax: Decimal,
    /// Amount with taxes, in the order currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Total"
    )]
    pub price_total: Decimal,
    /// Days between confirmation and delivery.
    #[field(integer, required = true, default = 0, string = "Lead Time")]
    pub customer_lead: i32,
    /// How the delivered quantity is found; satellites add values.
    #[field(
        selection,
        selection = [("manual", "Manual")],
        default = "manual",
        compute = "_compute_qty_delivered_method",
        store = true,
        string = "Delivered Quantity Source"
    )]
    pub qty_delivered_method: Option<Arc<str>>,
    /// The delivered quantity; entered by hand while the method is manual.
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_qty_delivered",
        store = true,
        readonly = false,
        copy = false,
        string = "Delivery Quantity"
    )]
    pub qty_delivered: Decimal,
    /// The invoicing policy: the product's, or the company's for a line without product.
    #[field(
        selection,
        selection = [("order", "Ordered quantities"), ("delivery", "Delivered quantities")],
        compute = "_compute_invoice_policy",
        string = "Invoicing Policy"
    )]
    pub invoice_policy: Option<Arc<str>>,
    /// Private lineage rows for linked customer invoices and credit notes.
    #[field(
        one2many,
        comodel = "sale.invoice.item",
        inverse_name = "sale_line_id",
        readonly = true,
        copy = false,
        string = "Invoice Items"
    )]
    pub invoice_item_link_ids: Vec<RecId>,
    /// The invoiced quantity, from linked customer invoices less credit notes.
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_qty_invoiced",
        store = true,
        string = "Invoiced Quantity"
    )]
    pub qty_invoiced: Decimal,
    /// The quantity left to invoice on a confirmed order.
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_qty_to_invoice",
        store = true,
        string = "Quantity To Invoice"
    )]
    pub qty_to_invoice: Decimal,
    /// The invoice status of the line.
    #[field(
        selection,
        selection = [
            ("upselling", "Upselling Opportunity"),
            ("invoiced", "Fully Invoiced"),
            ("to invoice", "To Invoice"),
            ("no", "Nothing to Invoice"),
        ],
        compute = "_compute_invoice_status",
        store = true,
        string = "Invoice Status"
    )]
    pub invoice_status: Option<Arc<str>>,
    /// Unit cost of the product in the order currency (sales users only).
    #[field(
        float,
        digits = "Product Price",
        compute = "_compute_purchase_price",
        store = true,
        readonly = false,
        groups = "sales_team.group_sale_salesman",
        string = "Cost"
    )]
    pub purchase_price: Decimal,
    /// Line profit margin (`price_subtotal - purchase_price * product_uom_qty`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_margin",
        store = true,
        groups = "sales_team.group_sale_salesman",
        string = "Margin"
    )]
    pub margin: Decimal,
    /// Line profit margin percentage.
    #[field(
        float,
        digits = (16, 2),
        compute = "_compute_margin",
        store = true,
        groups = "sales_team.group_sale_salesman",
        string = "Margin (%)"
    )]
    pub margin_percent: Decimal,
}

/// What the rules read of one line.
#[derive(Debug, Clone)]
pub(crate) struct LineFacts {
    pub(crate) id: RecId,
    pub(crate) order: Option<RecId>,
    /// The order's state, read from the order (not the line's stored copy).
    pub(crate) state: String,
    /// Whether the order is locked.
    pub(crate) locked: bool,
    pub(crate) display_type: Option<String>,
    pub(crate) downpayment: bool,
    pub(crate) product: Option<RecId>,
    pub(crate) unit: Option<RecId>,
    pub(crate) ordered: Decimal,
    pub(crate) delivered: Decimal,
    pub(crate) invoiced: Decimal,
    pub(crate) to_invoice: Decimal,
}

impl LineFacts {
    /// Whether something was delivered or invoiced on the line: it then keeps its product, its
    /// unit and at least that quantity, whatever the state of its order.
    fn downstream(&self) -> bool {
        self.delivered > Decimal::ZERO || self.invoiced > Decimal::ZERO
    }

    /// "Desk (S00001)" or "line 12 (S00001)".
    fn describe(
        &self,
        names: &BTreeMap<RecId, String>,
        orders: &BTreeMap<RecId, String>,
    ) -> String {
        let order = self
            .order
            .and_then(|order| orders.get(&order))
            .map_or("", String::as_str);
        match names.get(&self.id) {
            Some(name) => format!("{name} ({order})"),
            None => format!("line {} ({order})", self.id.get()),
        }
    }
}

fn text(value: Option<Arc<str>>) -> Option<String> {
    value.map(|value| value.to_string())
}

/// The facts of each line of `this`, in its order.
pub(crate) async fn facts(
    this: &RecordSet<SaleOrderLine>,
    tx: &mut Transaction,
) -> Result<Vec<LineFacts>> {
    let orders = this.mapped(tx, SaleOrderLine::ORDER_ID).await?;
    let heads: BTreeMap<RecId, Header> = headers(
        tx,
        &this
            .env()
            .records("sale.order")?
            .browse_ids(orders.iter().flatten().copied().collect::<BTreeSet<_>>()),
    )
    .await?
    .into_iter()
    .map(|head| (head.id, head))
    .collect();
    let kinds = this.mapped(tx, SaleOrderLine::DISPLAY_TYPE).await?;
    let downpayments = this.mapped(tx, SaleOrderLine::IS_DOWNPAYMENT).await?;
    let products = this.mapped(tx, SaleOrderLine::PRODUCT_ID).await?;
    let units = this.mapped(tx, SaleOrderLine::PRODUCT_UOM_ID).await?;
    let ordered = this.mapped(tx, SaleOrderLine::PRODUCT_UOM_QTY).await?;
    let delivered = this.mapped(tx, SaleOrderLine::QTY_DELIVERED).await?;
    let invoiced = this.mapped(tx, SaleOrderLine::QTY_INVOICED).await?;
    let to_invoice = this.mapped(tx, SaleOrderLine::QTY_TO_INVOICE).await?;
    let mut rows = Vec::with_capacity(orders.len());
    for (index, id) in this.as_records().rec_ids().iter().enumerate() {
        let head = orders[index]
            .and_then(|order| heads.get(&order))
            .ok_or_else(|| {
                Error::invariant(format!("sale.order.line {} has no order", id.get()))
            })?;
        rows.push(LineFacts {
            id: *id,
            order: orders[index],
            state: head.state.clone(),
            locked: head.locked,
            display_type: text(kinds[index].clone()),
            downpayment: downpayments[index],
            product: products[index],
            unit: units[index],
            ordered: ordered[index],
            delivered: delivered[index],
            invoiced: invoiced[index],
            to_invoice: to_invoice[index],
        });
    }
    Ok(rows)
}

/// The type, unit and invoicing policy of each product of `products`.
#[derive(Debug, Clone)]
pub(crate) struct ProductFacts {
    pub(crate) kind: Option<String>,
    pub(crate) unit: Option<RecId>,
    pub(crate) policy: Option<String>,
}

pub(crate) async fn product_facts(
    tx: &mut Transaction,
    env: &Env,
    products: impl IntoIterator<Item = RecId>,
) -> Result<BTreeMap<RecId, ProductFacts>> {
    let unique: BTreeSet<RecId> = products.into_iter().collect();
    if unique.is_empty() {
        return Ok(BTreeMap::new());
    }
    let variants = RecordSet::<ProductProduct>::try_from(
        env.records("product.product")?
            .browse_ids(unique.iter().copied()),
    )?;
    let templates = variants.mapped(tx, ProductProduct::PRODUCT_TMPL_ID).await?;
    let template_set = RecordSet::<ProductTemplate>::try_from(
        env.records("product.template")?
            .browse_ids(templates.iter().flatten().copied()),
    )?;
    let kinds = template_set.mapped(tx, ProductTemplate::TYPE).await?;
    let units = template_set.mapped(tx, ProductTemplate::UOM_ID).await?;
    let policies = template_set
        .mapped(tx, ProductTemplateSale::INVOICE_POLICY)
        .await?;
    let by_template: BTreeMap<RecId, ProductFacts> = template_set
        .as_records()
        .rec_ids()
        .iter()
        .zip(kinds.into_iter().zip(units).zip(policies))
        .map(|(template, ((kind, unit), policy))| {
            (
                *template,
                ProductFacts {
                    kind: text(kind),
                    unit,
                    policy: text(policy),
                },
            )
        })
        .collect();
    Ok(unique
        .iter()
        .zip(templates)
        .filter_map(|(variant, template)| Some((*variant, by_template.get(&template?)?.clone())))
        .collect())
}

/// A text per record id.
type Labels = BTreeMap<RecId, String>;

/// The sale display name of each line that has a product, and each order's number.
async fn names(tx: &mut Transaction, lines: &[LineFacts], env: &Env) -> Result<(Labels, Labels)> {
    let mut names = BTreeMap::new();
    let line_set = RecordSet::<SaleOrderLine>::try_from(
        env.records("sale.order.line")?
            .browse_ids(lines.iter().map(|line| line.id)),
    )?;
    for (line, name) in lines
        .iter()
        .zip(line_set.mapped(tx, SaleOrderLine::NAME).await?)
    {
        if let Some(name) = name {
            names.insert(line.id, name.lines().next().unwrap_or_default().to_owned());
        }
    }
    let orders = env.records("sale.order")?.browse_ids(
        lines
            .iter()
            .filter_map(|line| line.order)
            .collect::<BTreeSet<_>>(),
    );
    let numbers = headers(tx, &orders)
        .await?
        .into_iter()
        .map(|order| (order.id, order.name))
        .collect();
    Ok((names, numbers))
}

/// A user error that names the lines of `refused`.
async fn refuse(
    tx: &mut Transaction,
    env: &Env,
    refused: &[LineFacts],
    message: impl FnOnce(String) -> String,
) -> Result<()> {
    if refused.is_empty() {
        return Ok(());
    }
    let (names, orders) = names(tx, refused, env).await?;
    let listed: Vec<String> = refused
        .iter()
        .map(|line| line.describe(&names, &orders))
        .collect();
    Err(Error::user(message(listed.join(", "))))
}

fn quantity(vals: &RawVals) -> Result<Option<Decimal>> {
    match vals.get("product_uom_qty") {
        None => Ok(None),
        Some(Cell::Decimal(value)) => Ok(Some(*value)),
        Some(Cell::Int(value)) => Ok(Some(Decimal::from(*value))),
        Some(Cell::Float(value)) => Decimal::try_from(*value)
            .map(Some)
            .map_err(|_| Error::validation(format!("The quantity {value} is not a number."))),
        Some(other) => Err(Error::validation(format!(
            "The quantity of a sales order line must be a number, not {other:?}."
        ))),
    }
}

/// Whether a create gives the line a description: an empty or null text (what the client sends for
/// an emptied field) is not one.
fn named(env: &Env, vals: &RawVals) -> bool {
    match vals.get("name") {
        Some(Cell::Str(name)) => !name.trim().is_empty(),
        Some(_) => false,
        None => env.context().contains("default_name"),
    }
}

#[methods(SaleOrderLine, addon = "sale")]
impl SaleOrderLine {
    /// Proposes taxes, pricelist price and line amounts when the product changes in a form.
    #[onchange("product_id")]
    async fn _onchange_product_id(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        crate::amounts::onchange_line_product(tx, this.as_records()).await
    }

    /// Reprices when quantity or unit changes (unless priced by hand) and refreshes line amounts.
    #[onchange("product_uom_id", "product_uom_qty")]
    async fn _onchange_qty_uom(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        crate::amounts::onchange_line_qty_uom(tx, this.as_records()).await
    }

    /// Refreshes line subtotal, tax and total when price, discount, taxes or display type changes.
    #[onchange("price_unit", "discount", "tax_ids", "display_type")]
    async fn _onchange_amounts(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        crate::amounts::onchange_line_amounts(tx, this.as_records()).await
    }

    /// The product's name, for lines that have a product.
    #[compute(depends = ["product_id"])]
    async fn _compute_name(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let products = this.mapped(tx, SaleOrderLine::PRODUCT_ID).await?;
        let names = product_names(tx, this.env(), products.iter().flatten().copied()).await?;
        for (index, product) in products.into_iter().enumerate() {
            if let Some(name) = product.and_then(|product| names.get(&product).cloned().flatten()) {
                out.set(index, SaleOrderLine::NAME, Some(name))?;
            }
        }
        Ok(())
    }

    /// The product's unit; a line without product keeps its unit.
    #[compute(depends = ["product_id"])]
    async fn _compute_product_uom_id(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let products = this.mapped(tx, SaleOrderLine::PRODUCT_ID).await?;
        let facts = product_facts(tx, this.env(), products.iter().flatten().copied()).await?;
        for (index, product) in products.into_iter().enumerate() {
            if let Some(facts) = product.and_then(|product| facts.get(&product)) {
                out.set(index, SaleOrderLine::PRODUCT_UOM_ID, facts.unit)?;
            }
        }
        Ok(())
    }

    /// The units the line's product is sold.
    #[compute(depends = ["product_id"])]
    async fn _compute_allowed_uom_ids(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let products = this.mapped(tx, SaleOrderLine::PRODUCT_ID).await?;
        let model = this.env().records("product.product")?;
        for (index, product) in products.into_iter().enumerate() {
            let units: Vec<RecId> = match product {
                Some(product) => {
                    let units: Records = model
                        .browse_ids([product])
                        .call(tx, "_get_available_uoms", ())
                        .await?;
                    units.rec_ids().to_vec()
                }
                None => Vec::new(),
            };
            out.set(index, SaleOrderLine::ALLOWED_UOM_IDS, units)?;
        }
        Ok(())
    }

    /// Manual, except for goods lines, whose method `_get_consu_qty_delivered_method` gives.
    #[compute(depends = ["product_id"])]
    async fn _compute_qty_delivered_method(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let products = this.mapped(tx, SaleOrderLine::PRODUCT_ID).await?;
        let facts = product_facts(tx, this.env(), products.iter().flatten().copied()).await?;
        let goods: Vec<RecId> = this
            .as_records()
            .rec_ids()
            .iter()
            .zip(&products)
            .filter(|(_, product)| {
                product
                    .and_then(|product| facts.get(&product))
                    .is_some_and(|facts| facts.kind.as_deref() == Some("consu"))
            })
            .map(|(line, _)| *line)
            .collect();
        let methods: BTreeMap<RecId, String> = if goods.is_empty() {
            BTreeMap::new()
        } else {
            this.as_records()
                .browse_ids(goods)
                .call(tx, "_get_consu_qty_delivered_method", ())
                .await?
        };
        for (index, line) in this.as_records().rec_ids().iter().enumerate() {
            let method = methods.get(line).map_or("manual", String::as_str);
            out.set(
                index,
                SaleOrderLine::QTY_DELIVERED_METHOD,
                Some(Arc::<str>::from(method)),
            )?;
        }
        Ok(())
    }

    /// The delivered quantity of the lines whose method is not manual, from
    /// `_prepare_qty_delivered`; a manual line keeps the quantity entered by hand.
    #[compute(depends = ["qty_delivered_method"])]
    async fn _compute_qty_delivered(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let quantities: BTreeMap<RecId, Decimal> = this
            .as_records()
            .call(tx, "_prepare_qty_delivered", ())
            .await?;
        for (index, line) in this.as_records().rec_ids().iter().enumerate() {
            if let Some(quantity) = quantities.get(line) {
                out.set(index, SaleOrderLine::QTY_DELIVERED, *quantity)?;
            }
        }
        Ok(())
    }

    /// The product's policy, else the company's.
    #[compute(depends = ["product_id", "company_id"])]
    async fn _compute_invoice_policy(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let products = this.mapped(tx, SaleOrderLine::PRODUCT_ID).await?;
        let companies = this.mapped(tx, SaleOrderLine::COMPANY_ID).await?;
        let facts = product_facts(tx, this.env(), products.iter().flatten().copied()).await?;
        let company_policy =
            crate::product::company_policies(tx, this.env(), companies.iter().flatten().copied())
                .await?;
        for (index, (product, company)) in products.into_iter().zip(companies).enumerate() {
            let policy = match product.and_then(|product| facts.get(&product)) {
                Some(facts) => facts.policy.clone(),
                None => company.and_then(|company| company_policy.get(&company).cloned()),
            };
            out.set(
                index,
                SaleOrderLine::INVOICE_POLICY,
                policy.map(Arc::<str>::from),
            )?;
        }
        Ok(())
    }

    /// The invoiced quantity from `_prepare_qty_invoiced`.
    #[compute(depends = ["invoice_item_link_ids", "invoice_item_link_ids.invoice_item_id.quantity", "invoice_item_link_ids.invoice_item_id.move_id.state", "invoice_item_link_ids.invoice_item_id.move_id.move_type"])]
    async fn _compute_qty_invoiced(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let quantities: BTreeMap<RecId, Decimal> = this
            .as_records()
            .call(tx, "_prepare_qty_invoiced", ())
            .await?;
        for (index, line) in this.as_records().rec_ids().iter().enumerate() {
            let quantity = quantities.get(line).copied().unwrap_or_default();
            out.set(index, SaleOrderLine::QTY_INVOICED, quantity)?;
        }
        Ok(())
    }

    /// On a confirmed order, the ordered (policy `order`) or delivered (policy `delivery`)
    /// quantity less the invoiced one; zero otherwise (
    #[compute(depends = ["state", "product_uom_qty", "qty_delivered", "qty_invoiced", "invoice_policy"])]
    async fn _compute_qty_to_invoice(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        // not `facts`: it reads the quantity this compute assigns
        let states = this.mapped(tx, SaleOrderLine::STATE).await?;
        let kinds = this.mapped(tx, SaleOrderLine::DISPLAY_TYPE).await?;
        let policies = this.mapped(tx, SaleOrderLine::INVOICE_POLICY).await?;
        let ordered = this.mapped(tx, SaleOrderLine::PRODUCT_UOM_QTY).await?;
        let delivered = this.mapped(tx, SaleOrderLine::QTY_DELIVERED).await?;
        let invoiced = this.mapped(tx, SaleOrderLine::QTY_INVOICED).await?;
        let rows = states
            .into_iter()
            .zip(kinds)
            .zip(policies)
            .zip(ordered.into_iter().zip(delivered).zip(invoiced));
        for (index, (((state, kind), policy), ((ordered, delivered), invoiced))) in rows.enumerate()
        {
            let open = state.as_deref() == Some("sale") && kind.is_none();
            let left = |base: Decimal| {
                base.checked_sub(invoiced).ok_or_else(|| {
                    Error::validation("A sales order line quantity is out of range.")
                })
            };
            let quantity = match (open, policy.as_deref()) {
                (true, Some("delivery")) => left(delivered)?,
                (true, _) => left(ordered)?,
                (false, _) => Decimal::ZERO,
            };
            out.set(index, SaleOrderLine::QTY_TO_INVOICE, quantity)?;
        }
        Ok(())
    }

    /// The line's invoice status, first match wins (
    /// something to invoice, `to invoice`; more delivered than ordered on an ordered-quantity
    /// policy, `upselling`; invoiced at least the ordered quantity, `invoiced`; else `no`.
    #[compute(depends = ["state", "product_uom_qty", "qty_delivered", "qty_to_invoice", "qty_invoiced", "invoice_policy"])]
    async fn _compute_invoice_status(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let lines = facts(this, tx).await?;
        let policies = this.mapped(tx, SaleOrderLine::INVOICE_POLICY).await?;
        let rec_ids = this.as_records().rec_ids().to_vec();
        let needs_delivery_check = lines.iter().zip(&policies).any(|(line, policy)| {
            line.state == "sale"
                && !line.downpayment
                && line.to_invoice.is_zero()
                && line.invoiced < line.ordered
                && policy.as_deref() == Some("delivery")
                && !line.delivered.is_zero()
        });
        let finished_deliveries: std::collections::BTreeSet<RecId> = if needs_delivery_check {
            this.as_records()
                .call(tx, "_stock_moves_all_finished", ())
                .await?
        } else {
            std::collections::BTreeSet::new()
        };
        for (index, ((line, policy), rec_id)) in
            lines.iter().zip(policies).zip(rec_ids).enumerate()
        {
            let status = if line.state != "sale" {
                "no"
            } else if line.downpayment && line.to_invoice.is_zero() {
                "invoiced"
            } else if !line.to_invoice.is_zero() {
                "to invoice"
            } else if policy.as_deref() == Some("order")
                && line.ordered >= Decimal::ZERO
                && line.delivered > line.ordered
            {
                "upselling"
            } else if line.invoiced >= line.ordered
                || (policy.as_deref() == Some("delivery")
                    && !line.delivered.is_zero()
                    && finished_deliveries.contains(&rec_id))
            {
                "invoiced"
            } else {
                "no"
            };
            out.set(
                index,
                SaleOrderLine::INVOICE_STATUS,
                Some(Arc::<str>::from(status)),
            )?;
        }
        Ok(())
    }

    /// Lines whose linked outgoing stock moves are all finished (`done` or `cancel`, with at
    /// least one `done`). Overridden by `sale_stock` so delivery-policy lines with partial
    /// returns and no remaining open deliveries transition to `invoiced`.
    #[method]
    async fn _stock_moves_all_finished(
        _this: &RecordSet<SaleOrderLine>,
        _tx: &mut Transaction,
    ) -> Result<std::collections::BTreeSet<RecId>> {
        Ok(std::collections::BTreeSet::new())
    }

    /// The delivered-quantity method of goods lines: `manual` in `sale`; `sale_stock` answers
    /// its own. Keyed by line.
    #[method]
    async fn _get_consu_qty_delivered_method(
        this: &RecordSet<SaleOrderLine>,
        _tx: &mut Transaction,
    ) -> Result<BTreeMap<RecId, String>> {
        Ok(this
            .as_records()
            .rec_ids()
            .iter()
            .map(|line| (*line, "manual".to_owned()))
            .collect())
    }

    /// The delivered quantity of the lines whose method is not manual, keyed by line. `sale`
    /// knows no other method and answers none: a manual line keeps its hand-entered quantity.
    #[method]
    async fn _prepare_qty_delivered(
        _this: &RecordSet<SaleOrderLine>,
        _tx: &mut Transaction,
    ) -> Result<BTreeMap<RecId, Decimal>> {
        Ok(BTreeMap::new())
    }

    /// The invoiced quantity of each line, keyed by line: invoices less credit notes once orders
    /// link to invoice items. Cancelled documents contribute nothing.
    #[method]
    async fn _prepare_qty_invoiced(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
    ) -> Result<BTreeMap<RecId, Decimal>> {
        crate::invoice_lineage::quantities(this, tx).await
    }

    /// The lines that may be invoiced without the others: all of
    /// them in `sale`; the discount, delivery and reward addons take theirs out.
    #[method]
    async fn _can_be_invoiced_alone(
        this: &RecordSet<SaleOrderLine>,
        _tx: &mut Transaction,
    ) -> Result<Records> {
        Ok(this.as_records().clone())
    }

    /// The line fields a locked order refuses to change (R7): the
    /// product, description, unit price, unit, quantity, discount, taxes and order. The analytic
    /// distribution joins with its field.
    #[method]
    async fn _get_protected_fields(
        _this: &RecordSet<SaleOrderLine>,
        _tx: &mut Transaction,
    ) -> Result<ProtectedFields<SaleOrderLine>> {
        Ok(ProtectedFields::new()
            .with(SaleOrderLine::PRODUCT_ID)
            .with(SaleOrderLine::NAME)
            .with(SaleOrderLine::PRICE_UNIT)
            .with(SaleOrderLine::PRODUCT_UOM_ID)
            .with(SaleOrderLine::PRODUCT_UOM_QTY)
            .with(SaleOrderLine::DISCOUNT)
            .with(SaleOrderLine::TAX_IDS)
            .with(SaleOrderLine::ANALYTIC_ACCOUNT_ID)
            .with(SaleOrderLine::ORDER_ID))
    }

    /// A changed quantity on lines of a confirmed order, after `write` refused one below the
    /// delivered or invoiced quantity. Nothing to do in `sale`.
    // shortcut: the reference also posts the old and new quantities on the order's chatter; add
    // the note when the order chatter messages are specified.
    #[method]
    async fn _update_line_quantity(
        _this: &RecordSet<SaleOrderLine>,
        _tx: &mut Transaction,
        _quantity: Decimal,
    ) -> Result<()> {
        Ok(())
    }

    /// The lines that may not be deleted: product lines of a confirmed order and product lines
    /// with a delivered or invoiced quantity, whatever the order's state; sections and notes
    /// always may be, and down payments while they have no invoice line.
    #[method]
    async fn _check_line_unlink(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
    ) -> Result<Records> {
        let kept = facts(this, tx)
            .await?
            .into_iter()
            .filter(|line| {
                line.display_type.is_none()
                    && !line.downpayment
                    && (line.state == "sale" || line.downstream())
            })
            .map(|line| line.id);
        Ok(this.as_records().browse_ids(kept))
    }

    /// A line is not added to a cancelled or locked order and gives none of the derived fields; a
    /// section or note line is created without product, unit, price, quantity or lead time;
    /// a product line without description or unit takes the
    /// product's before the INSERT (both are needed there: the description is required, the unit
    /// by the `_accountable_required_fields` check, and their computes run after it).
    #[method(override)]
    async fn create(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        refuse_context_defaults(&env, "sales order line", &DERIVED_FIELDS)?;
        let mut orders = BTreeSet::new();
        let mut products = BTreeSet::new();
        for vals in &vals_list {
            refuse_derived(tx, vals)?;
            crate::amounts::refuse_derived(
                tx,
                &env,
                vals,
                "sale.order.line",
                &crate::amounts::LINE_DERIVED_ON_CREATE,
            )?;
            orders.extend(given_id(&env, vals, "order_id")?);
            products.extend(given_id(&env, vals, "product_id")?);
        }
        let order_ids: BTreeSet<i64> = orders.iter().map(|order| order.get()).collect();
        let order_heads = crate::amounts::heads(tx, &env, &order_ids).await?;
        let heads = headers(tx, &env.records("sale.order")?.browse_ids(orders)).await?;
        refuse_closed(&heads, "added", true)?;
        let units = product_facts(tx, &env, products.iter().copied()).await?;
        let names = product_names(tx, &env, products).await?;
        for vals in &mut vals_list {
            // a manual line starts at nothing delivered; its compute only fills the other methods
            if !given(&env, vals, "qty_delivered") {
                vals.insert("qty_delivered".into(), Cell::Decimal(Decimal::ZERO));
            }
            if let Some(order) = given_id(&env, vals, "order_id")? {
                let head = order_heads.get(&order.get()).ok_or_else(|| {
                    Error::invariant(format!("sales order {order:?} was not read"))
                })?;
                crate::amounts::fill_line(tx, &env, head, vals).await?;
            }
            if matches!(vals.get("display_type"), Some(Cell::Str(_))) {
                vals.insert("product_id".into(), Cell::Null);
                vals.insert("product_uom_id".into(), Cell::Null);
                vals.insert("price_unit".into(), Cell::Decimal(Decimal::ZERO));
                vals.insert("product_uom_qty".into(), Cell::Decimal(Decimal::ZERO));
                vals.insert("customer_lead".into(), Cell::Int(0));
                continue;
            }
            let Some(product) = given_id(&env, vals, "product_id")? else {
                if !named(&env, vals) {
                    return Err(Error::validation(
                        "A line needs a product or a description.",
                    ));
                }
                continue;
            };
            if !given(&env, vals, "product_uom_id")
                && let Some(unit) = units.get(&product).and_then(|facts| facts.unit)
            {
                vals.insert("product_uom_id".into(), Cell::Id(unit.get()));
            }
            if !named(&env, vals)
                && let Some(name) = names.get(&product).cloned().flatten()
            {
                vals.insert("name".into(), Cell::Str(name));
            }
        }
        let lines = sup.call(tx, this.as_records(), (vals_list,)).await?;
        crate::amounts::sync_lines(tx, &env, &lines).await?;
        Ok(lines)
    }

    /// The guards of a line write, in this order (B3): the derived fields are never
    /// written; a line of a cancelled order does not change; the type of a line does not
    /// change (a subsection may become a section); the product changes only while the line
    /// allows it; a new quantity on a confirmed order goes through `_update_line_quantity`; a
    /// locked order refuses its protected line fields.
    #[method(override)]
    async fn write(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        refuse_derived(tx, &vals)?;
        crate::amounts::refuse_derived(
            tx,
            this.env(),
            &vals,
            "sale.order.line",
            &crate::amounts::LINE_DERIVED,
        )?;
        let records = this.as_records();
        if records.is_empty() || vals.is_empty() {
            return sup.call(tx, records, (vals,)).await;
        }
        let env = this.env().clone();
        let lines = facts(this, tx).await?;
        // a line never changes its order, directly or through another order's `order_line`
        // commands (which write the inverse here): both orders' states, locks and totals would
        // have to be checked and kept, and the reference has no flow that moves a line
        if let Some(target) = vals.get("order_id") {
            let target = match target {
                Cell::Id(id) => RecId::from_real(*id),
                _ => None,
            };
            let moved: Vec<LineFacts> = lines
                .iter()
                .filter(|line| line.order != target)
                .cloned()
                .collect();
            refuse(tx, &env, &moved, |listed| {
                format!(
                    "A sales order line cannot move to another order; delete it and add a new one: {listed}."
                )
            })
            .await?;
        }
        refuse_closed(&line_orders(tx, &env, &lines).await?, "changed", false)?;
        // a new product, unit or quantity prices the line again unless its price is given or was
        // set by hand; a new product takes its taxes unless they are given
        let reprice = !vals.contains("price_unit")
            && ["product_id", "product_uom_id", "product_uom_qty"]
                .iter()
                .any(|field| vals.contains(field));
        let retax = vals.contains("product_id")
            && !vals.contains("tax_ids")
            && vals.commands("tax_ids").is_none();
        let manual = if reprice {
            crate::amounts::manual_prices(tx, records).await?
        } else {
            BTreeSet::new()
        };
        if let Some(new_type) = vals.get("display_type") {
            let new_type = match new_type {
                Cell::Str(kind) => Some(kind.to_string()),
                _ => None,
            };
            let changed: Vec<LineFacts> = lines
                .iter()
                .filter(|line| {
                    // a subsection may become a section
                    let promoted = line.display_type.as_deref() == Some("line_subsection")
                        && new_type.as_deref() == Some("line_section");
                    line.display_type != new_type && !promoted
                })
                .cloned()
                .collect();
            refuse(tx, &env, &changed, |listed| {
                format!(
                    "The kind of a sales order line cannot change; delete the line and add one of the kind you need: {listed}."
                )
            })
            .await?;
        }
        // product and unit stay what was delivered or invoiced
        for (field, what) in [("product_id", "product"), ("product_uom_id", "unit")] {
            let Some(value) = vals.get(field) else {
                continue;
            };
            let value = match value {
                Cell::Id(id) => RecId::from_real(*id),
                _ => None,
            };
            let frozen: Vec<LineFacts> = lines
                .iter()
                .filter(|line| {
                    (if field == "product_id" {
                        line.product
                    } else {
                        line.unit
                    }) != value
                })
                .filter(|line| {
                    line.downpayment
                        || line.state == "cancel"
                        || line.downstream()
                        || (line.state == "sale" && line.locked)
                })
                .cloned()
                .collect();
            refuse(tx, &env, &frozen, |listed| {
                format!("The {what} of these sales order lines can no longer be changed: {listed}.")
            })
            .await?;
        }
        if let Some(new_quantity) = quantity(&vals)? {
            // stricter than the reference, which checks only the delivered quantity: an invoiced
            // quantity is a consequence too, and the floor holds whatever the order's state; a
            // raise or an unchanged quantity passes, even while still below the floor
            let below: Vec<LineFacts> = lines
                .iter()
                .filter(|line| {
                    let floor = line.delivered.max(line.invoiced);
                    line.downstream() && new_quantity < floor && new_quantity < line.ordered
                })
                .cloned()
                .collect();
            if !below.is_empty() {
                let (names, orders) = names(tx, &below, &env).await?;
                let listed: Vec<String> = below
                    .iter()
                    .map(|line| {
                        format!(
                            "{}: {} delivered, {} invoiced",
                            line.describe(&names, &orders),
                            line.delivered.normalize(),
                            line.invoiced.normalize()
                        )
                    })
                    .collect();
                let hint = if below.iter().any(|line| line.delivered >= line.invoiced) {
                    "return the goods or correct the delivered quantity first"
                } else {
                    "correct the invoice or credit the rest first"
                };
                return Err(Error::user(format!(
                    "The quantity of a sales order line cannot go below what was delivered or invoiced ({} asked); {hint}: {}.",
                    new_quantity.normalize(),
                    listed.join("; ")
                )));
            }
            let changed: Vec<RecId> = lines
                .iter()
                .filter(|line| line.state == "sale" && line.ordered != new_quantity)
                .map(|line| line.id)
                .collect();
            if !changed.is_empty() {
                records
                    .browse_ids(changed)
                    .call::<_, ()>(tx, "_update_line_quantity", (new_quantity,))
                    .await?;
            }
        }
        let locked_lines: Vec<&LineFacts> = lines.iter().filter(|line| line.locked).collect();
        if !locked_lines.is_empty() {
            let protected: ProtectedFields<SaleOrderLine> =
                records.call(tx, "_get_protected_fields", ()).await?;
            let all_downpayments = lines.iter().all(|line| line.downpayment);
            let written: BTreeSet<&str> = vals
                .iter()
                .map(|(name, _)| name)
                .chain(vals.iter_commands().map(|(name, _)| name))
                .filter(|name| protected.contains(name))
                .filter(|name| *name != "name" || !all_downpayments)
                .collect();
            if !written.is_empty() {
                let labels: Vec<String> = written
                    .iter()
                    .map(|name| crate::order::label_of(tx, "sale.order.line", name))
                    .collect::<Result<_>>()?;
                let (_, numbers) = names(tx, &lines, &env).await?;
                let orders: BTreeSet<&str> = locked_lines
                    .iter()
                    .filter_map(|line| line.order.and_then(|order| numbers.get(&order)))
                    .map(String::as_str)
                    .collect();
                return Err(Error::user(format!(
                    "The sales order {} is locked. These fields of its lines cannot be changed: {}.",
                    orders.into_iter().collect::<Vec<_>>().join(", "),
                    labels.join(", ")
                )));
            }
        }
        sup.call(tx, records, (vals,)).await?;
        if reprice || retax {
            crate::amounts::reprice(tx, &env, records, &manual, reprice, retax).await?;
        }
        crate::amounts::sync_lines(tx, &env, records).await
    }

    /// Lines of a cancelled or locked order are not deleted; product lines of a confirmed order
    /// are not deleted either: their quantity is set to 0 instead.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let env = this.env().clone();
        let lines = facts(this, tx).await?;
        refuse_closed(&line_orders(tx, &env, &lines).await?, "deleted", true)?;
        let kept: Records = this.as_records().call(tx, "_check_line_unlink", ()).await?;
        if !kept.is_empty() {
            let set = RecordSet::<SaleOrderLine>::try_from(kept)?;
            let lines = facts(&set, tx).await?;
            refuse(tx, &env, &lines, |listed| {
                format!(
                    "A line of a confirmed sales order, or one with deliveries or invoices, is kept; set its quantity to 0 instead, or to what was delivered or invoiced: {listed}."
                )
            })
            .await?;
        }
        let orders: BTreeSet<i64> = facts(this, tx)
            .await?
            .into_iter()
            .filter_map(|line| line.order.map(RecId::get))
            .collect();
        sup.call(tx, this.as_records(), ()).await?;
        crate::amounts::sync_orders(tx, this.env(), &orders).await
    }
}
