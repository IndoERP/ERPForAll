// SPDX-License-Identifier: MIT OR Apache-2.0
//! Manual, direct-warehouse Buy replenishment (/016/080).
use erp_addon_product::{SellerInfo, SellerQuery};
use erp_addon_stock::position::{PositionMeasure, PositionRequest, PositionScope, stock_position};
use erp_addon_stock::{StockRule, StockWarehouseOrderpoint};
use erp_sdk::Operation;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;

#[extend(StockRule, addon = "purchase_stock")]
struct PurchaseRule {
    #[field(selection, selection_add = [("buy", "Buy")], ondelete = [("buy","cascade")])]
    action: Option<Arc<str>>,
}

// Private workflow state. Caller-created orders and purchase lines retain ordinary model ACLs.
// The unique source prevents concurrent requests from attaching separate accepted RFQs.
#[model(
    name = "purchase.stock.replenishment.link",
    description = "Manual Replenishment Quotation Link",
    rec_name = "order_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [("_orderpoint", "UNIQUE (orderpoint_id)", "A rule has one current replenishment quotation.")]
)]
struct ReplenishmentLink {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "stock.warehouse.orderpoint",
        required = true,
        ondelete = "cascade"
    )]
    orderpoint_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "purchase.order",
        required = true,
        ondelete = "cascade"
    )]
    order_id: Option<RecId>,
}

fn db(err: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Could not read the replenishment configuration.").with_source(err)
}
async fn amount(tx: &mut Transaction, record: &Records, field: &str) -> Result<Decimal> {
    match record.cell(tx, field).await? {
        Cell::Decimal(v) => Ok(v),
        _ => Err(Error::validation(format!(
            "Replenishment needs a quantity in {field}."
        ))),
    }
}
async fn id(tx: &mut Transaction, record: &Records, field: &str) -> Result<i64> {
    match record.cell(tx, field).await? {
        Cell::Id(v) => Ok(v),
        _ => Err(Error::user(format!(
            "Select {field} on the replenishment configuration."
        ))),
    }
}

#[extend(StockWarehouseOrderpoint, addon = "purchase_stock")]
struct BuyOrderpoint {}

#[methods(StockWarehouseOrderpoint, addon = "purchase_stock")]
impl BuyOrderpoint {
    /// Create the vendor's draft quotation for a short manual Buy rule, or reopen its pending quotation.
    #[method]
    #[rpc(effect=write)]
    async fn action_replenish_buy(
        this: &RecordSet<StockWarehouseOrderpoint>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let point = this.as_records();
        point.ensure_one()?;
        point.check_access(tx, Operation::Write).await?;
        tx.savepoint(async |tx| replenish(tx, point).await).await
    }
}

async fn replenish(tx: &mut Transaction, point: &Records) -> Result<Value> {
    tx.flush().await?;
    let point_id = point.ensure_one()?.int()?;
    // An explicit duplicate click waits for the first decision. Under repeatable read,
    // a conflicting private-link write retries through the normal transaction runner.
    if tx
        .query(
            "SELECT id FROM stock_warehouse_orderpoint WHERE id=$1 FOR NO KEY UPDATE",
            &[&point_id],
        )
        .await?
        .is_empty()
    {
        return Err(Error::user("The reordering rule is no longer available."));
    }
    let company = id(tx, point, "company_id").await?;
    let caller = point.env();
    if !caller.su()
        && !tx
            .companies(caller)
            .await?
            .iter()
            .any(|c| c.get() == company)
    {
        return Err(Error::access("Select the reordering rule's company."));
    }
    let env = caller.with_company(company)?;
    let orders = env.records("purchase.order")?;
    orders.check_access(tx, Operation::Create).await?;
    env.records("purchase.order.line")?
        .check_access(tx, Operation::Create)
        .await?;
    if point.cell(tx, "active").await? != Cell::Bool(true) {
        return Err(Error::user("Choose an active reordering rule."));
    }
    let links=tx.query("SELECT order_id FROM purchase_stock_replenishment_link WHERE orderpoint_id=$1 AND company_id=$2",&[&point_id,&company]).await?;
    if let Some(link) = links.first() {
        let old = orders.browse([link.try_get::<_, i64>(0).map_err(db)?])?;
        if matches!(old.cell(tx,"state").await?,Cell::Str(ref state) if matches!(state.as_ref(),"draft"|"sent"|"to approve"))
        {
            return open_rfq(tx, &old).await;
        }
    }
    let stock = id(tx, point, "location_id").await?;
    let wh = id(tx, point, "warehouse_id").await?;
    let route = id(tx, point, "route_id").await?;
    direct_buy_route(tx, &env, company, wh, stock, route).await?;
    let product = id(tx, point, "product_id").await?;
    let unit = id(
        tx,
        &env.records("product.product")?.browse([product])?,
        "uom_id",
    )
    .await?;
    if !matches!(point.cell(tx, "replenishment_uom_id").await?, Cell::Null) {
        return Err(Error::user(
            "Replenishment multiples require a later release. Clear the multiple for a direct Buy quotation.",
        ));
    }
    let forecast = forecast(tx, &env, company, stock, product).await?;
    let min = amount(tx, point, "product_min_qty").await?;
    let max = amount(tx, point, "product_max_qty").await?;
    if forecast >= min {
        return Err(Error::user(
            "Stock already meets this rule's minimum. No request for quotation is needed.",
        ));
    }
    let manual = match point.cell(tx, "qty_to_order_manual").await? {
        Cell::Null => Decimal::ZERO,
        Cell::Decimal(value) => value,
        _ => {
            return Err(Error::invariant(
                "The manual replenishment quantity has an unexpected type.",
            ));
        }
    };
    let qty = if manual.is_zero() {
        max.max(min)
            .checked_sub(forecast)
            .ok_or_else(|| Error::validation("Replenishment quantity is too large."))?
    } else {
        manual
    };
    if qty <= Decimal::ZERO {
        return Err(Error::validation(
            "The replenishment quantity must be positive.",
        ));
    }
    let product_record = env.records("product.product")?.browse([product])?;
    if product_record.cell(tx, "type").await? != Cell::Str("consu".into()) {
        return Err(Error::user(
            "Only stockable goods can be replenished through a direct Buy rule.",
        ));
    }
    let query = SellerQuery::default()
        .with_quantity(Some(qty))
        .with_unit(unit)?;
    let vendor: Option<SellerInfo> = product_record
        .call(tx, "_select_seller_exact", (query,))
        .await?;
    let vendor = vendor.ok_or_else(|| {
        Error::user("No eligible vendor price covers this product, quantity and company.")
    })?;
    let Some(Value::Int(partner)) = vendor.values().get("partner_id") else {
        return Err(Error::invariant(
            "The selected vendor price is missing its supplier.",
        ));
    };
    let line = RawVals::new()
        .set_cell("product_id", Cell::Id(product))
        .set_cell("uom_id", Cell::Id(unit))
        .set_cell("product_qty", Cell::Decimal(qty));
    let vals = RawVals::new()
        .set_cell("partner_id", Cell::Id(*partner))
        .set_cell("company_id", Cell::Id(company))
        .set_cell("origin", point.cell(tx, "name").await?)
        .set_commands("order_line", [Command::Create(line)]);
    let order = orders.create(tx, vec![vals]).await?;
    let order_id = order.ensure_one()?.int()?;
    tx.execute("INSERT INTO purchase_stock_replenishment_link (company_id,orderpoint_id,order_id) VALUES ($1,$2,$3) ON CONFLICT (orderpoint_id) DO UPDATE SET order_id=EXCLUDED.order_id,company_id=EXCLUDED.company_id",&[&company,&point_id,&order_id]).await?;
    open_rfq(tx, &order).await
}

async fn direct_buy_route(
    tx: &mut Transaction,
    env: &Env,
    company: i64,
    warehouse: i64,
    stock: i64,
    route: i64,
) -> Result<()> {
    let eligible = tx
        .query(
            "SELECT id FROM stock_warehouse WHERE company_id=$1 AND active ORDER BY id LIMIT 2",
            &[&company],
        )
        .await?;
    let warehouses = eligible
        .iter()
        .map(|row| row.try_get::<_, i64>(0).map_err(db))
        .collect::<Result<Vec<_>>>()?;
    env.records("stock.warehouse")?
        .browse(warehouses.iter().copied())?
        .check_access(tx, Operation::Read)
        .await?;
    if warehouses != [warehouse] {
        return Err(Error::user(
            "Direct Buy replenishment requires one active receiving warehouse in this company.",
        ));
    }
    let wh = env.records("stock.warehouse")?.browse([warehouse])?;
    let operation = id(tx, &wh, "in_type_id").await?;
    if wh.cell(tx, "active").await? != Cell::Bool(true)
        || wh.cell(tx, "reception_steps").await? != Cell::Str("one_step".into())
        || id(tx, &wh, "lot_stock_id").await? != stock
        || id(tx, &wh, "company_id").await? != company
    {
        return Err(Error::user(
            "Choose the stock location of an active one-step receiving warehouse.",
        ));
    }
    let incoming = env.records("stock.picking.type")?.browse([operation])?;
    if incoming.cell(tx, "active").await? != Cell::Bool(true)
        || incoming.cell(tx, "code").await? != Cell::Str("incoming".into())
        || id(tx, &incoming, "default_location_dest_id").await? != stock
    {
        return Err(Error::user(
            "Configure this warehouse's active incoming operation to deliver directly to stock.",
        ));
    }
    let supplier = id(tx, &incoming, "default_location_src_id").await?;
    let source = env.records("stock.location")?.browse([supplier])?;
    if id(tx, &incoming, "company_id").await? != company
        || source.cell(tx, "usage").await? != Cell::Str("supplier".into())
    {
        return Err(Error::user(
            "Configure a supplier source on this company's receiving operation.",
        ));
    }
    let route_record = env.records("stock.route")?.browse([route])?;
    if matches!(route_record.cell(tx,"company_id").await?,Cell::Id(c) if c != company) {
        return Err(Error::access("The Buy route belongs to another company."));
    }
    let active = route_record.cell(tx, "active").await?;
    let rules = env.records("stock.rule")?;
    let owner = Domain::field("company_id")?;
    let d = Domain::field("route_id")?.eq(route)
        & Domain::field("active")?.eq(true)
        & Domain::field("location_dest_id")?.eq(stock)
        & (owner.eq(company) | owner.eq(false));
    let all = env
        .sudo(true)
        .records("stock.rule")?
        .search_count(tx, &d, Some(2))
        .await?;
    let visible = rules.search_count(tx, &d, Some(2)).await?;
    if visible != all {
        return Err(Error::access(
            "Complete access to this route's receiving rules is required.",
        ));
    }
    if all != 1 {
        return Err(Error::user(
            "The selected route needs one active rule for this stock location.",
        ));
    }
    let matched = rules.search(tx, &d).await?;
    if active != Cell::Bool(true)
        || matched.len() != 1
        || matched.cell(tx, "action").await? != Cell::Str("buy".into())
        || id(tx, &matched, "picking_type_id").await? != operation
        || matches!(matched.cell(tx,"location_src_id").await?,Cell::Id(loc) if loc != supplier)
        || matches!(matched.cell(tx,"warehouse_id").await?,Cell::Id(wh) if wh != warehouse)
        || matched.cell(tx, "partner_address_id").await? != Cell::Null
    {
        return Err(Error::user(
            "Configure one active Buy rule on the selected route for this warehouse's receiving operation and stock location.",
        ));
    }
    if let Cell::Id(owner) = matched.cell(tx, "company_id").await?
        && owner != company
    {
        return Err(Error::access("The Buy rule belongs to another company."));
    }
    Ok(())
}

async fn forecast(
    tx: &mut Transaction,
    env: &Env,
    company: i64,
    stock: i64,
    product: i64,
) -> Result<Decimal> {
    let request = PositionRequest::new(
        [product],
        Some(&[company]),
        PositionScope::locations([stock], true)?,
    )?;
    let batch = stock_position(env, tx, &request).await?;
    // A report can show a partial view. An RFQ must be sized from all facts in this admitted scope.
    let all = env
        .sudo(true)
        .with_context([("active_test", Value::Bool(false))]);
    let visible = env.with_context([("active_test", Value::Bool(false))]);
    for source in batch.drill_sources(product, PositionMeasure::Forecasted)? {
        let count = visible
            .records(source.model())?
            .search_count(tx, source.domain(), None)
            .await?;
        if all
            .records(source.model())?
            .search_count(tx, source.domain(), Some(count + 1))
            .await?
            != count
        {
            return Err(Error::access(
                "Complete stock visibility is required to replenish this location.",
            ));
        }
    }
    batch
        .position(product)
        .map(|p| p.value(PositionMeasure::Forecasted))
        .ok_or_else(|| Error::access("The replenishment product is unavailable."))
}

async fn open_rfq(tx: &mut Transaction, order: &Records) -> Result<Value> {
    order.check_access(tx, Operation::Read).await?;
    let rows=tx.query("SELECT res_id FROM ir_model_data WHERE module='purchase' AND name='purchase_order_form' AND model='ir.ui.view'",&[]).await?;
    let [row] = rows.as_slice() else {
        return Err(Error::invariant("The purchase order form is missing."));
    };
    let view: i64 = row.try_get(0).map_err(db)?;
    Ok(Value::Json(Arc::new(
        json!({"type":"ir.actions.act_window","name":"Request for Quotation","res_model":"purchase.order","res_id":order.ensure_one()?.int()?,"views":[[view,"form"]],"target":"current"}),
    )))
}
