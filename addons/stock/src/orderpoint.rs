// SPDX-License-Identifier: MIT OR Apache-2.0
//! Replenishment rule configuration: the table only. Computed state (quantity to
//! order, deadline) belongs to `stock_orderpoint_state` and the computes, the scheduler and the
//! name from the `stock.orderpoint` sequence belong to. Observed in.

use erp_sdk::prelude::*;

/// `stock.warehouse.orderpoint`: keep the forecast of one product at one location between a
/// minimum and a maximum.
#[model(
    name = "stock.warehouse.orderpoint",
    description = "Minimum Inventory Rule",
    order = "location_id, company_id, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_product_location_check",
        fields = ["product_id", "location_id", "company_id"],
        message = "A replenishment rule already exists for this product on this location."
    )],
    constraints = [
        (
            "_finite_product_min_qty",
            "CHECK (product_min_qty <> 'NaN' AND product_min_qty <> 'Infinity' AND product_min_qty <> '-Infinity')",
            "The minimum of a replenishment rule must be a finite number."
        ),
        (
            "_finite_product_max_qty",
            "CHECK (product_max_qty <> 'NaN' AND product_max_qty <> 'Infinity' AND product_max_qty <> '-Infinity')",
            "The maximum of a replenishment rule must be a finite number."
        ),
        (
            "_finite_daily_demand",
            "CHECK (daily_demand <> 'NaN' AND daily_demand <> 'Infinity' AND daily_demand <> '-Infinity')",
            "The daily demand of a replenishment rule must be a finite number."
        ),
        (
            "_finite_qty_to_order_manual",
            "CHECK (qty_to_order_manual <> 'NaN' AND qty_to_order_manual <> 'Infinity' AND qty_to_order_manual <> '-Infinity')",
            "The manual order quantity of a replenishment rule must be a finite number."
        ),
    ],
)]
pub struct StockWarehouseOrderpoint {
    #[field(char, required = true, copy = false)]
    pub name: Option<Arc<str>>,
    /// Whether the scheduler orders by itself or only suggests.
    #[field(
        selection,
        selection = [("auto", "Auto"), ("manual", "Manual")],
        default = "auto",
        required = true,
        string = "Trigger"
    )]
    pub trigger: Option<Arc<str>>,
    /// Archived rules are hidden without being removed.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Hidden until the next scheduler run after this date.
    #[field(date, string = "Snoozed")]
    pub snoozed_until: Option<Date>,
    #[field(
        many2one,
        comodel = "stock.warehouse",
        required = true,
        ondelete = "cascade",
        index = true,
        check_company = true
    )]
    pub warehouse_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "cascade",
        index = true,
        check_company = true
    )]
    pub location_id: Option<RecId>,
    /// Product kept stocked. The unique key leads with it, so it
    /// has no index of its own (`index = false` turns the framework's default off).
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "cascade",
        index = false,
        check_company = true
    )]
    pub product_id: Option<RecId>,
    /// Forecast level that triggers a replenishment.
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 0,
        string = "Min Quantity"
    )]
    pub product_min_qty: Decimal,
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 0,
        string = "Max Quantity"
    )]
    pub product_max_qty: Decimal,
    /// Daily demand used to size the maximum.
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 0,
        string = "Daily Demand"
    )]
    pub daily_demand: Decimal,
    /// How the daily demand is estimated.
    #[field(
        selection,
        selection = [
            ("one_week", "Last 7 days"),
            ("one_month", "Last 30 days"),
            ("three_months", "Last 3 months"),
            ("one_year", "Last 12 months"),
            ("last_year", "Same month last year"),
            ("last_year_2", "Next month last year"),
            ("last_year_3", "After next month last year"),
            ("last_year_quarter", "Last year quarter"),
            ("custom", "Custom Demand")
        ],
        default = "one_month",
        required = true,
        string = "Based on"
    )]
    pub min_max_based_on: Option<Arc<str>>,
    /// Percentage applied to the estimated demand.
    #[field(integer, default = 100, required = true)]
    pub min_max_based_on_factor: i32,
    /// Orders are rounded up to a multiple of this unit; empty keeps the quantity as it is.
    /// Registry default applies: the unit has no company scope, so nothing is compared.
    #[field(many2one, comodel = "uom.uom", string = "Multiple")]
    pub replenishment_uom_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    #[field(many2one, comodel = "stock.route", string = "Route")]
    pub route_id: Option<RecId>,
    /// Quantity to order set by hand.
    #[field(float, digits = "Product Unit", string = "To Order Manual")]
    pub qty_to_order_manual: Decimal,
}
