// SPDX-License-Identifier: MIT OR Apache-2.0
//! Promotions, Loyalty Programs & Coupons (`loyalty.program`, `loyalty.rule`, `loyalty.reward`,
//! `loyalty.card`, and `sale.loyalty.coupon.wizard`).

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RoundingTable};
use erp_sdk::Operation;
use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;

use crate::order::{SaleOrder, strip_default_context};

/// Pure exact-decimal loyalty reward discount calculation.
/// For `discount_mode == "percent"`, computes `untaxed * discount / 100` (rounded to 2dp).
/// For `discount_mode == "per_order"`, caps the fixed reward at `untaxed`.
pub fn compute_loyalty_reward_discount(
    untaxed: Decimal,
    discount: Decimal,
    discount_mode: &str,
) -> Result<Decimal> {
    if untaxed <= Decimal::ZERO {
        return Err(Error::user(
            "Add at least one positive order line before redeeming a coupon.",
        ));
    }
    if discount <= Decimal::ZERO {
        return Err(Error::user("Reward discount must be greater than zero."));
    }
    match discount_mode {
        "percent" => {
            if discount > Decimal::from(100) {
                return Err(Error::user("Reward percentage cannot exceed 100%."));
            }
            let raw = untaxed
                .checked_mul(discount)
                .and_then(|v| v.checked_div(Decimal::from(100)))
                .ok_or_else(|| Error::validation("Reward discount calculation overflowed."))?;
            Ok(raw.round_dp(2))
        }
        "per_order" => Ok(discount.min(untaxed).round_dp(2)),
        other => Err(Error::validation(format!(
            "Unknown loyalty discount mode '{other}'."
        ))),
    }
}

fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Loyalty query failed: {err}"))
}

/// `loyalty.program`: coupon, promotion, or loyalty card program.
#[model(
    name = "loyalty.program",
    description = "Discount & Loyalty Program",
    order = "name, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny")
)]
pub struct LoyaltyProgram {
    /// Program name.
    #[field(char, required = true, string = "Program Name")]
    pub name: Option<Arc<str>>,
    /// Program category.
    #[field(
        selection,
        selection = [
            ("coupons", "Coupons"),
            ("promotion", "Promotions"),
            ("loyalty", "Loyalty Cards"),
        ],
        required = true,
        default = "coupons",
        string = "Program Type"
    )]
    pub program_type: Option<Arc<str>>,
    /// Whether this program applies automatically or requires a code.
    #[field(
        selection,
        selection = [("with_code", "Use a code"), ("auto", "Automatic")],
        required = true,
        default = "with_code",
        string = "Trigger"
    )]
    pub trigger: Option<Arc<str>>,
    /// Active flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
    /// Owning company.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Company currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Optional start date.
    #[field(date, string = "Start Date")]
    pub date_from: Option<Date>,
    /// Optional end date.
    #[field(date, string = "End Date")]
    pub date_to: Option<Date>,
    /// Conditional rules (minimum order amount/qty, promo code).
    #[field(
        one2many,
        comodel = "loyalty.rule",
        inverse_name = "program_id",
        string = "Rules"
    )]
    pub rule_ids: Vec<RecId>,
    /// Rewards granted when redeemed.
    #[field(
        one2many,
        comodel = "loyalty.reward",
        inverse_name = "program_id",
        string = "Rewards"
    )]
    pub reward_ids: Vec<RecId>,
    /// Issued coupons / loyalty cards.
    #[field(
        one2many,
        comodel = "loyalty.card",
        inverse_name = "program_id",
        string = "Coupons & Cards"
    )]
    pub card_ids: Vec<RecId>,
    /// Count of issued cards.
    #[field(integer, compute = "_compute_coupon_count", string = "Coupons")]
    pub coupon_count: i32,
}

#[methods(LoyaltyProgram, addon = "sale")]
impl LoyaltyProgram {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<LoyaltyProgram>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "company_id")
            && !values.contains_key("company_id")
            && let Some(cid) = this.env().company_id()?
        {
            values.insert("company_id".to_owned(), Value::Int(cid));
        }
        Ok(values)
    }

    #[compute(depends = ["card_ids"])]
    async fn _compute_coupon_count(
        this: &RecordSet<LoyaltyProgram>,
        tx: &mut Transaction,
        out: &mut Assign<'_, LoyaltyProgram>,
    ) -> Result<()> {
        tx.flush().await?;
        let real_ids: Vec<i64> = this
            .as_records()
            .rec_ids()
            .iter()
            .filter_map(|r| r.as_real().map(|id| id.get()))
            .collect();
        let mut counts = std::collections::BTreeMap::<i64, i32>::new();
        if !real_ids.is_empty() {
            let rows = tx
                .query(
                    "SELECT program_id::int8, count(*)::int4 FROM loyalty_card \
                     WHERE program_id = ANY($1) GROUP BY program_id",
                    &[&real_ids],
                )
                .await?;
            for row in rows {
                let pid: i64 = row.try_get(0).map_err(db)?;
                let cnt: i32 = row.try_get(1).map_err(db)?;
                counts.insert(pid, cnt);
            }
        }
        for (i, rid) in this.as_records().rec_ids().iter().enumerate() {
            let cnt = rid
                .as_real()
                .and_then(|r| counts.get(&r.get()).copied())
                .unwrap_or(0);
            out.set(i, LoyaltyProgram::COUPON_COUNT, cnt)?;
        }
        Ok(())
    }

    /// Issues a fresh coupon code (`COUP-00001`) with 1 point for immediate use on orders.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_generate_coupon(
        this: &RecordSet<LoyaltyProgram>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let prog = this.ensure_one()?;
        prog.as_records().check_access(tx, Operation::Write).await?;
        let card_model = prog.env().records("loyalty.card")?;
        card_model.check_access(tx, Operation::Create).await?;
        let pid = prog.as_records().ids()[0];
        let rows = tx
            .query(
                "SELECT nextval(pg_get_serial_sequence('loyalty_card', 'id'))::int8",
                &[],
            )
            .await?;
        let next: i64 = rows
            .first()
            .ok_or_else(|| Error::invariant("Failed to allocate next loyalty coupon number."))?
            .try_get(0)
            .map_err(db)?;
        let code = format!("COUP-{next:05}");
        let mut vals = RawVals::new();
        vals.insert("program_id".into(), Cell::Id(pid));
        vals.insert("code".into(), Cell::Str(code.as_str().into()));
        vals.insert("points".into(), Cell::Decimal(Decimal::ONE));
        card_model.create(tx, vec![vals]).await?;
        tx.invalidate_records(prog.as_records(), Some(&["card_ids", "coupon_count"]))
            .await?;
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.client",
            "tag": "display_notification",
            "params": {
                "message": format!("Generated coupon code {code}."),
                "type": "success",
            },
        }))))
    }
}

/// `loyalty.rule`: eligibility rule (minimum quantity/amount or promo code).
#[model(
    name = "loyalty.rule",
    description = "Loyalty Rule",
    order = "id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny")
)]
pub struct LoyaltyRule {
    /// Parent program.
    #[field(
        many2one,
        comodel = "loyalty.program",
        required = true,
        ondelete = "cascade",
        string = "Program"
    )]
    pub program_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "program_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Optional promo code that activates this rule.
    #[field(char, string = "Promo Code")]
    pub code: Option<Arc<str>>,
    /// Minimum product quantity required on the order.
    #[field(float, digits = (16, 2), default = 0, string = "Minimum Quantity")]
    pub minimum_qty: Decimal,
    /// Minimum untaxed order amount required.
    #[field(monetary, currency_field = "currency_id", string = "Minimum Purchase")]
    pub minimum_amount: Decimal,
    /// Points granted per qualifying order.
    #[field(float, digits = (16, 2), default = 1, string = "Grant Points")]
    pub reward_point_amount: Decimal,
}

#[methods(LoyaltyRule, addon = "sale")]
impl LoyaltyRule {}

/// `loyalty.reward`: discount reward granted by a loyalty program.
#[model(
    name = "loyalty.reward",
    description = "Loyalty Reward",
    order = "id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny")
)]
pub struct LoyaltyReward {
    /// Parent program.
    #[field(
        many2one,
        comodel = "loyalty.program",
        required = true,
        ondelete = "cascade",
        string = "Program"
    )]
    pub program_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "program_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Description shown on the discount line of the sales order.
    #[field(char, required = true, string = "Description")]
    pub description: Option<Arc<str>>,
    /// Reward mode.
    #[field(
        selection,
        selection = [
            ("percent", "Percent (%)"),
            ("per_order", "Fixed Amount on Order"),
        ],
        required = true,
        default = "percent",
        string = "Discount Mode"
    )]
    pub discount_mode: Option<Arc<str>>,
    /// Discount percentage or fixed amount.
    #[field(float, digits = (16, 2), default = 10, string = "Discount")]
    pub discount: Decimal,
    /// Points consumed on redemption.
    #[field(float, digits = (16, 2), default = 1, string = "Required Points")]
    pub required_points: Decimal,
}

#[methods(LoyaltyReward, addon = "sale")]
impl LoyaltyReward {}

/// `loyalty.card`: issued coupon or customer loyalty card with point balance.
#[model(
    name = "loyalty.card",
    description = "Coupon / Loyalty Card",
    order = "id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"),
    keys = [key(
        name = "_loyalty_card_code_uniq",
        fields = ["code"],
        message = "This coupon code already exists."
    )]
)]
pub struct LoyaltyCard {
    /// Unique coupon or card code.
    #[field(char, required = true, index = true, string = "Code")]
    pub code: Option<Arc<str>>,
    /// Parent program.
    #[field(
        many2one,
        comodel = "loyalty.program",
        required = true,
        ondelete = "cascade",
        string = "Program"
    )]
    pub program_id: Option<RecId>,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "program_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Optional customer owner.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        string = "Customer"
    )]
    pub partner_id: Option<RecId>,
    /// Remaining redeemable points.
    #[field(float, digits = (16, 2), default = 1, string = "Balance (Points)")]
    pub points: Decimal,
    /// Optional expiration date.
    #[field(date, string = "Expiration Date")]
    pub expiration_date: Option<Date>,
    /// Active flag.
    #[field(boolean, default = true, string = "Active")]
    pub active: bool,
}

#[methods(LoyaltyCard, addon = "sale")]
impl LoyaltyCard {
    #[method(override)]
    async fn write(
        this: &RecordSet<LoyaltyCard>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let env = this.env();
        if !env.su() && !tx.has_group(env, crate::LOCK_GROUP).await? {
            return Err(Error::access(
                "Only a Sales Administrator can modify coupons or loyalty cards directly.",
            ));
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }
}

/// `sale.loyalty.coupon.wizard`: modal dialog to redeem a coupon or promo code on a sales order.
#[model(
    name = "sale.loyalty.coupon.wizard",
    description = "Enter Promotion or Coupon Code",
    kind = "transient",
    transient_max_age_seconds = 3600,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct SaleLoyaltyCouponWizard {
    /// Target sales order.
    #[field(
        many2one,
        comodel = "sale.order",
        required = true,
        ondelete = "cascade",
        string = "Sales Order"
    )]
    pub order_id: Option<RecId>,
    /// Company from the order.
    #[field(
        many2one,
        comodel = "res.company",
        related = "order_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Coupon code or promo code entered by the user.
    #[field(char, required = true, string = "Code")]
    pub coupon_code: Option<Arc<str>>,
}

#[methods(SaleLoyaltyCouponWizard, addon = "sale")]
impl SaleLoyaltyCouponWizard {
    #[method]
    #[rpc(effect = write)]
    pub async fn action_apply(
        this: &RecordSet<SaleLoyaltyCouponWizard>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        wizard
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        let order_id = wizard
            .get(tx, SaleLoyaltyCouponWizard::ORDER_ID)
            .await?
            .ok_or_else(|| Error::validation("Choose a sales order."))?
            .get();
        let raw_code = wizard
            .get(tx, SaleLoyaltyCouponWizard::COUPON_CODE)
            .await?
            .unwrap_or_default();
        let code = raw_code.trim();
        if code.is_empty() {
            return Err(Error::user("Enter a coupon or promotion code."));
        }

        let env = strip_default_context(wizard.env());
        let order_rec = env.records("sale.order")?.browse([order_id])?;
        order_rec.check_access(tx, Operation::Write).await?;
        let code_owned = code.to_owned();
        let uid = env.uid();

        let (desc, disc_amount) = tx
            .savepoint(async |tx| {
                tx.flush().await?;
                order_rec.lock_for_update(tx, true).await?;

                let state = match order_rec.cell(tx, "state").await? {
                    Cell::Str(s) => s.to_string(),
                    other => {
                        return Err(Error::invariant(format!(
                            "sale.order.state returned {other:?}"
                        )));
                    }
                };
                if state != "draft" && state != "sent" {
                    return Err(Error::user(
                        "Coupons can only be redeemed on draft or sent quotations.",
                    ));
                }
                if order_rec.cell(tx, "locked").await? == Cell::Bool(true) {
                    return Err(Error::user(
                        "This sales order is locked and cannot accept a coupon.",
                    ));
                }
                let untaxed = match order_rec.cell(tx, "amount_untaxed").await? {
                    Cell::Decimal(d) => d,
                    Cell::Null => Decimal::ZERO,
                    other => {
                        return Err(Error::invariant(format!(
                            "sale.order.amount_untaxed returned {other:?}"
                        )));
                    }
                };
                let company_id = match order_rec.cell(tx, "company_id").await? {
                    Cell::Id(id) => id,
                    other => {
                        return Err(Error::invariant(format!(
                            "Sales order {order_id} has no valid company ({other:?})."
                        )));
                    }
                };
                let currency_id = match order_rec.cell(tx, "currency_id").await? {
                    Cell::Id(id) => id,
                    other => {
                        return Err(Error::invariant(format!(
                            "Sales order {order_id} has no valid currency ({other:?})."
                        )));
                    }
                };
                let order_partner_id = match order_rec.cell(tx, "partner_id").await? {
                    Cell::Id(id) => Some(id),
                    Cell::Null => None,
                    other => {
                        return Err(Error::invariant(format!(
                            "sale.order.partner_id returned {other:?}"
                        )));
                    }
                };
                let qty_rows = tx
                    .query(
                        "SELECT COALESCE(SUM(product_uom_qty), 0) FROM sale_order_line \
                         WHERE order_id = $1 AND display_type IS NULL \
                           AND NOT COALESCE(is_downpayment, false) AND product_uom_qty > 0",
                        &[&order_id],
                    )
                    .await?;
                let order_qty: Decimal = qty_rows
                    .first()
                    .ok_or_else(|| Error::invariant("Missing sum row for order quantity."))?
                    .try_get(0)
                    .map_err(db)?;

                let today = env.context_today()?;

                // Try matching an active coupon card first, then fall back to a promo rule code.
                let card_rows = tx
                    .query(
                        "SELECT c.id::int8, c.program_id::int8, COALESCE(c.points, 0), \
                                c.expiration_date, c.partner_id::int8, p.date_from, p.date_to \
                         FROM loyalty_card c \
                         JOIN loyalty_program p ON p.id = c.program_id \
                         WHERE lower(c.code) = lower($1) AND c.active = true AND p.active = true \
                           AND p.company_id = $2 \
                         LIMIT 1 FOR UPDATE OF c",
                        &[&code_owned, &company_id],
                    )
                    .await?;

                let (program_id, card_opt): (i64, Option<(i64, Decimal)>) =
                    if let Some(crow) = card_rows.first() {
                        let cid: i64 = crow.try_get(0).map_err(db)?;
                        let pid: i64 = crow.try_get(1).map_err(db)?;
                        let pts: Decimal = crow.try_get(2).map_err(db)?;
                        let exp: Option<Date> = crow.try_get(3).map_err(db)?;
                        let card_partner: Option<i64> = crow.try_get(4).map_err(db)?;
                        let p_from: Option<Date> = crow.try_get(5).map_err(db)?;
                        let p_to: Option<Date> = crow.try_get(6).map_err(db)?;
                        if let Some(cp) = card_partner
                            && order_partner_id != Some(cp)
                        {
                            return Err(Error::user(
                                "This coupon belongs to a different customer.",
                            ));
                        }
                        if let Some(e) = exp
                            && today > e
                        {
                            return Err(Error::user(format!(
                                "Coupon {code_owned} expired on {e}."
                            )));
                        }
                        if let Some(df) = p_from
                            && today < df
                        {
                            return Err(Error::user(format!("This promotion starts on {df}.")));
                        }
                        if let Some(dt) = p_to
                            && today > dt
                        {
                            return Err(Error::user(format!("This promotion ended on {dt}.")));
                        }
                        // Enforce any program eligibility rules (minimum qty / amount).
                        let prog_rules = tx
                            .query(
                                "SELECT COALESCE(minimum_qty, 0), COALESCE(minimum_amount, 0) \
                                 FROM loyalty_rule WHERE program_id = $1 ORDER BY id",
                                &[&pid],
                            )
                            .await?;
                        if !prog_rules.is_empty() {
                            let mut matched = false;
                            let mut first_min_qty = Decimal::ZERO;
                            let mut first_min_amt = Decimal::ZERO;
                            for (idx, rr) in prog_rules.iter().enumerate() {
                                let mq: Decimal = rr.try_get(0).map_err(db)?;
                                let ma: Decimal = rr.try_get(1).map_err(db)?;
                                if idx == 0 {
                                    first_min_qty = mq;
                                    first_min_amt = ma;
                                }
                                if order_qty >= mq && untaxed >= ma {
                                    matched = true;
                                    break;
                                }
                            }
                            if !matched {
                                return Err(Error::user(format!(
                                    "Order does not meet eligibility rules for '{code_owned}' \
                                     (minimum quantity {first_min_qty}, minimum amount {first_min_amt})."
                                )));
                            }
                        }
                        (pid, Some((cid, pts)))
                    } else {
                        let rule_rows = tx
                            .query(
                                "SELECT r.program_id::int8, COALESCE(r.minimum_qty, 0), \
                                        COALESCE(r.minimum_amount, 0), p.date_from, p.date_to \
                                 FROM loyalty_rule r \
                                 JOIN loyalty_program p ON p.id = r.program_id \
                                 WHERE lower(r.code) = lower($1) AND p.active = true \
                                   AND p.company_id = $2 \
                                 LIMIT 1",
                                &[&code_owned, &company_id],
                            )
                            .await?;
                        let Some(rrow) = rule_rows.first() else {
                            return Err(Error::user(format!(
                                "Coupon or promo code '{code_owned}' was not found or is inactive."
                            )));
                        };
                        let pid: i64 = rrow.try_get(0).map_err(db)?;
                        let min_qty: Decimal = rrow.try_get(1).map_err(db)?;
                        let min_amt: Decimal = rrow.try_get(2).map_err(db)?;
                        let p_from: Option<Date> = rrow.try_get(3).map_err(db)?;
                        let p_to: Option<Date> = rrow.try_get(4).map_err(db)?;
                        if let Some(df) = p_from
                            && today < df
                        {
                            return Err(Error::user(format!("This promotion starts on {df}.")));
                        }
                        if let Some(dt) = p_to
                            && today > dt
                        {
                            return Err(Error::user(format!("This promotion ended on {dt}.")));
                        }
                        if order_qty < min_qty {
                            return Err(Error::user(format!(
                                "Minimum quantity for '{code_owned}' is {min_qty}; current order quantity is {order_qty}."
                            )));
                        }
                        if untaxed < min_amt {
                            return Err(Error::user(format!(
                                "Minimum order amount for '{code_owned}' is {min_amt}; current untaxed total is {untaxed}."
                            )));
                        }
                        (pid, None)
                    };

                let reward_rows = tx
                    .query(
                        "SELECT description::text, discount_mode::text, COALESCE(discount, 0), \
                                COALESCE(required_points, 1) \
                         FROM loyalty_reward WHERE program_id = $1 ORDER BY id LIMIT 1",
                        &[&program_id],
                    )
                    .await?;
                let Some(rw) = reward_rows.first() else {
                    return Err(Error::user(
                        "This promotion program has no reward configured yet.",
                    ));
                };
                let desc: String = rw.try_get(0).map_err(db)?;
                let mode: String = rw.try_get(1).map_err(db)?;
                let disc_val: Decimal = rw.try_get(2).map_err(db)?;
                let req_pts: Decimal = rw.try_get(3).map_err(db)?;

                let cur_rec = RecId::from_real(currency_id)
                    .ok_or_else(|| Error::invariant("Invalid order currency id."))?;
                let rounding = RoundingTable::load(&env, tx, &[cur_rec]).await?;
                let inc = rounding.get(cur_rec)?.increment();
                let raw_disc = compute_loyalty_reward_discount(untaxed, disc_val, &mode)?;
                let disc_amount = inc.quantize(raw_disc, CURRENCY_ROUNDING_MODE)?;

                let uom_id = crate::discount::default_unit_uom_id(tx).await?;
                let mut lvals = RawVals::new();
                lvals.insert("order_id".into(), Cell::Id(order_id));
                lvals.insert(
                    "name".into(),
                    Cell::Str(format!("{desc} [{code_owned}]").into()),
                );
                lvals.insert("product_uom_id".into(), Cell::Id(uom_id));
                lvals.insert("product_uom_qty".into(), Cell::Decimal(Decimal::ONE));
                lvals.insert("price_unit".into(), Cell::Decimal(-disc_amount));
                env.records("sale.order.line")?
                    .create(tx, vec![lvals])
                    .await?;

                if let Some((cid, pts)) = card_opt {
                    if pts < req_pts {
                        return Err(Error::user(format!(
                            "Coupon {code_owned} has {pts} point(s), but {req_pts} point(s) are required."
                        )));
                    }
                    let new_pts = pts
                        .checked_sub(req_pts)
                        .ok_or_else(|| Error::validation("Coupon point calculation overflowed."))?;
                    let still_active = new_pts > Decimal::ZERO;
                    tx.execute(
                        "UPDATE loyalty_card SET points = $2, active = $3, write_uid = $4, \
                         write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
                        &[&cid, &new_pts, &still_active, &uid],
                    )
                    .await?;
                    let card_rec = env.sudo(true).records("loyalty.card")?.browse([cid])?;
                    tx.invalidate_records(&card_rec, Some(&["points", "active"]))
                        .await?;
                }

                wizard.as_records().unlink(tx).await?;
                Ok((desc, disc_amount))
            })
            .await?;

        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.client",
            "tag": "display_notification",
            "params": {
                "message": format!("Applied '{desc}' (-{disc_amount}) using code {code}."),
                "type": "success",
                "next": {"type": "ir.actions.act_window_close"},
            },
        }))))
    }
}

#[methods(SaleOrder, addon = "sale")]
impl SaleOrder {
    /// Opens the Coupon / Promo Code wizard for this sales order.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_open_coupon_wizard(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Write).await?;
        let oid = rec.as_records().ids()[0];
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Coupon Code",
            "res_model": "sale.loyalty.coupon.wizard",
            "target": "new",
            "views": [[false, "form"]],
            "context": {"default_order_id": oid},
        }))))
    }
}

#[cfg(test)]
mod tests {
    use super::compute_loyalty_reward_discount;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn loyalty_reward_computes_exact_percent_and_capped_fixed_discount() {
        let untaxed = Decimal::from_str("120.00").unwrap();
        assert_eq!(
            compute_loyalty_reward_discount(untaxed, Decimal::from_str("10").unwrap(), "percent")
                .unwrap(),
            Decimal::from_str("12.00").unwrap()
        );
        assert_eq!(
            compute_loyalty_reward_discount(
                untaxed,
                Decimal::from_str("200").unwrap(),
                "per_order"
            )
            .unwrap(),
            Decimal::from_str("120.00").unwrap()
        );
    }
}
