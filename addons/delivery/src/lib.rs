// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `delivery` for ERPForAll: shipping carriers, delivery pricing rules,
//! exact-decimal rate calculation with margins and free-over thresholds.

use erp_sdk::{Operation, prelude::*};

erp_sdk::addon!(
    name = "delivery",
    exact_only = true,
    depends = ["base", "product"]
);

/// Shipment totals used to evaluate a carrier's delivery rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShipmentMetrics {
    /// Total shipment weight in kilograms.
    pub weight: Decimal,
    /// Total shipment volume in cubic meters.
    pub volume: Decimal,
    /// Total order/transfer goods value.
    pub order_price: Decimal,
    /// Total number of units shipped.
    pub quantity: Decimal,
}

impl ShipmentMetrics {
    fn variable_value(&self, var: &str) -> Result<Decimal> {
        match var {
            "weight" => Ok(self.weight),
            "volume" => Ok(self.volume),
            "price" => Ok(self.order_price),
            "quantity" => Ok(self.quantity),
            other => Err(Error::validation(format!(
                "Unknown delivery pricing variable '{other}'."
            ))),
        }
    }
}

/// One ordered rule belonging to a rule-based delivery carrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceRuleSpec {
    /// Metric tested against `max_value` (`weight`, `volume`, `price`, `quantity`).
    pub variable: String,
    /// Comparison operator (`==`, `<=`, `<`, `>=`, `>`).
    pub operator: String,
    /// Threshold compared against `variable`.
    pub max_value: Decimal,
    /// Fixed base charge when this rule matches.
    pub list_base_price: Decimal,
    /// Per-unit rate multiplied by `variable_factor`.
    pub list_price: Decimal,
    /// Metric multiplied by `list_price`.
    pub variable_factor: String,
}

/// Carrier pricing configuration read from `delivery.carrier` and its rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarrierPricing {
    /// Carrier display name (for clear error messages).
    pub name: String,
    /// Pricing mode (`fixed` or `base_on_rule`).
    pub delivery_type: String,
    /// Base price when `delivery_type == "fixed"`.
    pub fixed_price: Decimal,
    /// Whether shipments above `free_over_amount` ship free.
    pub free_over: bool,
    /// Order value threshold for free shipping.
    pub free_over_amount: Decimal,
    /// Percentage margin added on top of the base rate (e.g. `10` = +10%).
    pub margin_percent: Decimal,
    /// Fixed margin added after percentage margin.
    pub fixed_margin: Decimal,
    /// Ordered pricing rules when `delivery_type == "base_on_rule"`.
    pub rules: Vec<PriceRuleSpec>,
}

fn rule_matches(lhs: Decimal, op: &str, rhs: Decimal) -> Result<bool> {
    match op {
        "==" => Ok(lhs == rhs),
        "<=" => Ok(lhs <= rhs),
        "<" => Ok(lhs < rhs),
        ">=" => Ok(lhs >= rhs),
        ">" => Ok(lhs > rhs),
        other => Err(Error::validation(format!(
            "Unknown delivery rule operator '{other}'."
        ))),
    }
}

/// Compute the exact shipping price for a carrier and shipment metrics, rounded to `scale`
/// decimal places.
pub fn rate_shipment(
    carrier: &CarrierPricing,
    metrics: &ShipmentMetrics,
    scale: u32,
) -> Result<Decimal> {
    if carrier.free_over && metrics.order_price >= carrier.free_over_amount {
        return Ok(Decimal::ZERO);
    }
    let base = match carrier.delivery_type.as_str() {
        "fixed" => carrier.fixed_price,
        "base_on_rule" => {
            let mut matched = None;
            for rule in &carrier.rules {
                let lhs = metrics.variable_value(&rule.variable)?;
                if rule_matches(lhs, &rule.operator, rule.max_value)? {
                    let factor = metrics.variable_value(&rule.variable_factor)?;
                    matched = Some(rule.list_base_price + (rule.list_price * factor));
                    break;
                }
            }
            matched.ok_or_else(|| {
                Error::user(format!(
                    "No pricing rule on carrier '{}' matches this shipment.",
                    carrier.name
                ))
            })?
        }
        other => {
            return Err(Error::validation(format!(
                "Unsupported delivery carrier provider '{other}'."
            )));
        }
    };
    let hundred = Decimal::from(100);
    let with_margin =
        base * (Decimal::ONE + (carrier.margin_percent / hundred)) + carrier.fixed_margin;
    if with_margin < Decimal::ZERO {
        return Err(Error::validation(format!(
            "Computed shipping rate for carrier '{}' cannot be negative.",
            carrier.name
        )));
    }
    Ok(with_margin.round_dp(scale))
}

/// Read a carrier's pricing configuration and ordered rules from the database.
pub async fn load_carrier_pricing(tx: &mut Transaction, carrier_id: i64) -> Result<CarrierPricing> {
    fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
        Error::db("Delivery carrier pricing could not be read.").with_source(e)
    }
    tx.env()
        .records("delivery.carrier")?
        .browse(vec![carrier_id])?
        .check_access(tx, Operation::Read)
        .await?;
    let rows = tx
        .query(
            "SELECT name, delivery_type, COALESCE(fixed_price, 0), COALESCE(free_over, false), \
                    COALESCE(amount, 0), COALESCE(margin, 0), COALESCE(fixed_margin, 0) \
             FROM delivery_carrier WHERE id = $1",
            &[&carrier_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(Error::missing("Delivery carrier not found."));
    };
    let name: String = row.try_get(0).map_err(db_err)?;
    let delivery_type: String = row.try_get(1).map_err(db_err)?;
    let fixed_price: Decimal = row.try_get(2).map_err(db_err)?;
    let free_over: bool = row.try_get(3).map_err(db_err)?;
    let free_over_amount: Decimal = row.try_get(4).map_err(db_err)?;
    let margin_percent: Decimal = row.try_get(5).map_err(db_err)?;
    let fixed_margin: Decimal = row.try_get(6).map_err(db_err)?;

    let rule_rows = tx
        .query(
            "SELECT variable, operator, max_value, list_base_price, list_price, variable_factor \
             FROM delivery_price_rule \
             WHERE carrier_id = $1 ORDER BY sequence, id",
            &[&carrier_id],
        )
        .await?;
    let mut rules = Vec::with_capacity(rule_rows.len());
    for rr in rule_rows {
        rules.push(PriceRuleSpec {
            variable: rr.try_get(0).map_err(db_err)?,
            operator: rr.try_get(1).map_err(db_err)?,
            max_value: rr.try_get(2).map_err(db_err)?,
            list_base_price: rr.try_get(3).map_err(db_err)?,
            list_price: rr.try_get(4).map_err(db_err)?,
            variable_factor: rr.try_get(5).map_err(db_err)?,
        });
    }
    Ok(CarrierPricing {
        name,
        delivery_type,
        fixed_price,
        free_over,
        free_over_amount,
        margin_percent,
        fixed_margin,
        rules,
    })
}

/// `delivery.carrier`: shipping method with fixed or rule-based pricing.
#[model(
    name = "delivery.carrier",
    description = "Shipping Method",
    order = "sequence, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    constraints = [
        ("_nonneg_fixed_price", "CHECK (fixed_price >= 0)", "Fixed shipping price cannot be negative."),
        ("_nonneg_amount", "CHECK (amount >= 0)", "Free shipping threshold cannot be negative."),
    ]
)]
pub struct DeliveryCarrier {
    /// Shipping method name.
    #[field(char, required = true, string = "Delivery Method")]
    pub name: Option<Arc<str>>,
    /// Archived carriers stay hidden from normal selection.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Display order.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// Pricing mode (`fixed` or `base_on_rule`).
    #[field(
        selection,
        selection = [
            ("fixed", "Fixed Price"),
            ("base_on_rule", "Based on Rules")
        ],
        default = "fixed",
        required = true,
        string = "Provider"
    )]
    pub delivery_type: Option<Arc<str>>,
    /// Service product invoiced on sales orders for this carrier.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        check_company = true,
        string = "Delivery Product"
    )]
    pub product_id: Option<RecId>,
    /// Optional owning company; empty means shared across companies.
    #[field(many2one, comodel = "res.company", string = "Company")]
    pub company_id: Option<RecId>,
    /// Flat price when `delivery_type == "fixed"`.
    #[field(float, digits = "Product Price", default = 0, string = "Fixed Price")]
    pub fixed_price: Decimal,
    /// Offer free shipping when the order total reaches `amount`.
    #[field(boolean, default = false, string = "Free if order amount is above")]
    pub free_over: bool,
    /// Minimum order total for free shipping.
    #[field(float, digits = "Product Price", default = 0, string = "Amount")]
    pub amount: Decimal,
    /// Percentage markup applied to the calculated rate.
    #[field(float, digits = 0, default = 0, string = "Margin on Rate (%)")]
    pub margin: Decimal,
    /// Fixed markup added on top of the calculated rate.
    #[field(
        float,
        digits = "Product Price",
        default = 0,
        string = "Additional Margin"
    )]
    pub fixed_margin: Decimal,
    /// Ordered pricing rules when `delivery_type == "base_on_rule"`.
    #[field(
        one2many,
        comodel = "delivery.price.rule",
        inverse_name = "carrier_id",
        copy = true,
        string = "Pricing Rules"
    )]
    pub price_rule_ids: Vec<RecId>,
    /// Optional tracking URL template with `{ref}` placeholder.
    #[field(char, string = "Tracking URL Template")]
    pub tracking_label: Option<Arc<str>>,
    /// Customer-facing delivery notes.
    #[field(text, string = "Description")]
    pub carrier_description: Option<Arc<str>>,
}

/// `delivery.price.rule`: one conditional rate rule on a `delivery.carrier`.
#[model(
    name = "delivery.price.rule",
    description = "Delivery Price Rule",
    order = "sequence, id",
    company_access = parent(edge = "carrier_id")
)]
pub struct DeliveryPriceRule {
    /// Owning delivery carrier.
    #[field(
        many2one,
        comodel = "delivery.carrier",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Carrier"
    )]
    pub carrier_id: Option<RecId>,
    /// Evaluation order (lowest first).
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// Human-readable rule summary.
    #[field(char, compute = "_compute_name", store = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Metric evaluated by the rule condition.
    #[field(
        selection,
        selection = [
            ("weight", "Weight"),
            ("volume", "Volume"),
            ("price", "Price"),
            ("quantity", "Quantity")
        ],
        default = "weight",
        required = true,
        string = "Variable"
    )]
    pub variable: Option<Arc<str>>,
    /// Comparison operator.
    #[field(
        selection,
        selection = [
            ("==", "="),
            ("<=", "<="),
            ("<", "<"),
            (">=", ">="),
            (">", ">")
        ],
        default = "<=",
        required = true,
        string = "Operator"
    )]
    pub operator: Option<Arc<str>>,
    /// Condition threshold.
    #[field(
        float,
        digits = 0,
        default = 0,
        required = true,
        string = "Maximum Value"
    )]
    pub max_value: Decimal,
    /// Fixed base charge when matched.
    #[field(
        float,
        digits = "Product Price",
        default = 0,
        required = true,
        string = "Sale Base Price"
    )]
    pub list_base_price: Decimal,
    /// Variable unit charge multiplied by `variable_factor`.
    #[field(
        float,
        digits = "Product Price",
        default = 0,
        required = true,
        string = "Sale Price per Unit"
    )]
    pub list_price: Decimal,
    /// Metric multiplied by `list_price`.
    #[field(
        selection,
        selection = [
            ("weight", "Weight"),
            ("volume", "Volume"),
            ("price", "Price"),
            ("quantity", "Quantity")
        ],
        default = "weight",
        required = true,
        string = "Variable Factor"
    )]
    pub variable_factor: Option<Arc<str>>,
}

#[methods(DeliveryPriceRule, addon = "delivery")]
impl DeliveryPriceRule {
    #[compute(depends = ["variable", "operator", "max_value", "list_base_price", "list_price", "variable_factor"])]
    async fn _compute_name(
        this: &RecordSet<DeliveryPriceRule>,
        tx: &mut Transaction,
        out: &mut Assign<'_, DeliveryPriceRule>,
    ) -> Result<()> {
        let vars = this.mapped(tx, DeliveryPriceRule::VARIABLE).await?;
        let ops = this.mapped(tx, DeliveryPriceRule::OPERATOR).await?;
        let maxs = this.mapped(tx, DeliveryPriceRule::MAX_VALUE).await?;
        let bases = this.mapped(tx, DeliveryPriceRule::LIST_BASE_PRICE).await?;
        let prices = this.mapped(tx, DeliveryPriceRule::LIST_PRICE).await?;
        let factors = this.mapped(tx, DeliveryPriceRule::VARIABLE_FACTOR).await?;
        for (idx, (((((var, op), max_v), base), price), factor)) in vars
            .into_iter()
            .zip(ops)
            .zip(maxs)
            .zip(bases)
            .zip(prices)
            .zip(factors)
            .enumerate()
        {
            let v = var.as_deref().unwrap_or("weight");
            let o = op.as_deref().unwrap_or("<=");
            let f = factor.as_deref().unwrap_or("weight");
            let label = format!("if {v} {o} {max_v} then {base} + {price} * {f}");
            out.set(idx, DeliveryPriceRule::NAME, Some(Arc::<str>::from(label)))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal literal")
    }

    #[test]
    fn fixed_carrier_respects_free_over_threshold_and_margins() {
        let carrier = CarrierPricing {
            name: "Standard".into(),
            delivery_type: "fixed".into(),
            fixed_price: dec("15.00"),
            free_over: true,
            free_over_amount: dec("500.00"),
            margin_percent: dec("10"),
            fixed_margin: dec("2.50"),
            rules: vec![],
        };
        let small = ShipmentMetrics {
            order_price: dec("100.00"),
            ..Default::default()
        };
        assert_eq!(rate_shipment(&carrier, &small, 2).unwrap(), dec("19.00"));

        let large = ShipmentMetrics {
            order_price: dec("500.00"),
            ..Default::default()
        };
        assert_eq!(rate_shipment(&carrier, &large, 2).unwrap(), dec("0.00"));
    }

    #[test]
    fn rule_based_carrier_evaluates_ordered_tiers_with_exact_decimals() {
        let carrier = CarrierPricing {
            name: "Express".into(),
            delivery_type: "base_on_rule".into(),
            fixed_price: Decimal::ZERO,
            free_over: false,
            free_over_amount: Decimal::ZERO,
            margin_percent: dec("10"),
            fixed_margin: Decimal::ZERO,
            rules: vec![
                PriceRuleSpec {
                    variable: "weight".into(),
                    operator: "<=".into(),
                    max_value: dec("5"),
                    list_base_price: dec("20"),
                    list_price: dec("2"),
                    variable_factor: "weight".into(),
                },
                PriceRuleSpec {
                    variable: "weight".into(),
                    operator: ">".into(),
                    max_value: dec("5"),
                    list_base_price: dec("35"),
                    list_price: dec("4"),
                    variable_factor: "weight".into(),
                },
            ],
        };
        // 3 kg -> (20 + 2*3) * 1.10 = 26 * 1.10 = 28.60
        let light = ShipmentMetrics {
            weight: dec("3"),
            ..Default::default()
        };
        assert_eq!(rate_shipment(&carrier, &light, 2).unwrap(), dec("28.60"));

        // 8 kg -> (35 + 4*8) * 1.10 = 67 * 1.10 = 73.70
        let heavy = ShipmentMetrics {
            weight: dec("8"),
            ..Default::default()
        };
        assert_eq!(rate_shipment(&carrier, &heavy, 2).unwrap(), dec("73.70"));
    }

    #[test]
    fn rule_based_carrier_refuses_unmatched_shipment() {
        let carrier = CarrierPricing {
            name: "Parcel".into(),
            delivery_type: "base_on_rule".into(),
            fixed_price: Decimal::ZERO,
            free_over: false,
            free_over_amount: Decimal::ZERO,
            margin_percent: Decimal::ZERO,
            fixed_margin: Decimal::ZERO,
            rules: vec![PriceRuleSpec {
                variable: "weight".into(),
                operator: "<=".into(),
                max_value: dec("10"),
                list_base_price: dec("15"),
                list_price: Decimal::ZERO,
                variable_factor: "weight".into(),
            }],
        };
        let too_heavy = ShipmentMetrics {
            weight: dec("25"),
            ..Default::default()
        };
        let err = rate_shipment(&carrier, &too_heavy, 2).expect_err("must refuse unmatched weight");
        assert!(err.message().contains("No pricing rule"));
    }
}
