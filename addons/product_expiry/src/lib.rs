// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `product_expiry` for ERPForAll: product expiration dates, lot lifecycle dates,
//! expiry alerts and delivery slip settings.

use std::collections::BTreeMap;

use erp_addon_product::ProductTemplate;
use erp_addon_stock::{StockLot, StockMoveLine};
use erp_sdk::prelude::*;
use erp_sdk::settings::{ScopeKind, SettingDecl};

erp_sdk::addon!(name = "product_expiry", depends = ["base", "stock"]);

const MICROS_PER_DAY: i64 = 86_400_000_000;
const ADMIN: &str = "base.group_system";
const READ: &[&str] = &["stock.group_stock_manager", ADMIN];

/// Company setting controlling whether lot expiration dates appear on delivery slips.
pub const DISPLAY_EXPIRATION_ON_DELIVERY_SLIP: &str =
    "product_expiry.display_expiration_on_delivery_slip";

fn settings() -> Result<Vec<SettingDecl>> {
    Ok(vec![
        SettingDecl::boolean(
            DISPLAY_EXPIRATION_ON_DELIVERY_SLIP,
            "Display expiration dates on delivery slips",
            ScopeKind::Company,
            false,
        )?
        .app("stock")?
        .block("Traceability")?
        .sequence(60)
        .help("Show lot expiration dates alongside lot numbers on printed delivery slips.")?
        .access(READ, &[ADMIN])?,
    ])
}
erp_sdk::setting_declarations!(settings);

/// Add `days` to a `DateTime`, saturating only within valid microsecond bounds.
pub fn add_days(base: DateTime, days: i32) -> Result<DateTime> {
    let delta = i64::from(days)
        .checked_mul(MICROS_PER_DAY)
        .ok_or_else(|| Error::validation("Expiry day offset exceeds supported date range."))?;
    let micros = base
        .micros()
        .checked_add(delta)
        .ok_or_else(|| Error::validation("Computed expiry date exceeds supported date range."))?;
    Ok(DateTime::from_micros(micros))
}

/// Computed lifecycle dates for a lot or serial number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpiryDates {
    /// Date after which the lot is expired.
    pub expiration_date: Option<DateTime>,
    /// Date after which the lot begins to lose quality.
    pub use_date: Option<DateTime>,
    /// Date after which the lot should be removed from stock.
    pub removal_date: Option<DateTime>,
    /// Date on which an expiry alert activates.
    pub alert_date: Option<DateTime>,
}

/// Derive secondary lifecycle dates (`use_date`, `removal_date`, `alert_date`) from an
/// explicit `expiration_date` and product template offsets.
pub fn compute_expiry_dates_from_expiration(
    exp: DateTime,
    use_time: i32,
    removal_time: i32,
    alert_time: i32,
) -> Result<ExpiryDates> {
    let use_date = if use_time > 0 {
        Some(add_days(exp, -use_time)?)
    } else {
        None
    };
    let removal_date = if removal_time > 0 {
        Some(add_days(exp, -removal_time)?)
    } else {
        None
    };
    let alert_date = if alert_time > 0 {
        Some(add_days(exp, -alert_time)?)
    } else {
        None
    };
    Ok(ExpiryDates {
        expiration_date: Some(exp),
        use_date,
        removal_date,
        alert_date,
    })
}

/// Derive lot lifecycle dates from a receipt/creation timestamp and product template durations.
///
/// - `expiration_date` = `base + expiration_time` (days)
/// - `use_date` = `expiration_date - use_time` (days) when `use_time > 0`
/// - `removal_date` = `expiration_date - removal_time` (days) when `removal_time > 0`
/// - `alert_date` = `expiration_date - alert_time` (days) when `alert_time > 0`
pub fn compute_expiry_dates(
    base: DateTime,
    expiration_time: i32,
    use_time: i32,
    removal_time: i32,
    alert_time: i32,
) -> Result<ExpiryDates> {
    if expiration_time <= 0 {
        return Ok(ExpiryDates::default());
    }
    let exp = add_days(base, expiration_time)?;
    compute_expiry_dates_from_expiration(exp, use_time, removal_time, alert_time)
}

#[extend(ProductTemplate, addon = "product_expiry")]
pub struct ProductTemplateExpiry {
    /// Enable expiration date tracking on lots and serial numbers of this product.
    #[field(
        boolean,
        default = false,
        groups = "stock.group_production_lot",
        string = "Expiration Date"
    )]
    pub use_expiration_date: bool,
    /// Days after receipt until the product expires.
    #[field(
        integer,
        default = 0,
        groups = "stock.group_production_lot",
        string = "Expiration Time (Days)"
    )]
    pub expiration_time: i32,
    /// Days before the expiration date after which goods start deteriorating.
    #[field(
        integer,
        default = 0,
        groups = "stock.group_production_lot",
        string = "Best Before Time (Days)"
    )]
    pub use_time: i32,
    /// Days before the expiration date when goods should be removed from stock.
    #[field(
        integer,
        default = 0,
        groups = "stock.group_production_lot",
        string = "Removal Time (Days)"
    )]
    pub removal_time: i32,
    /// Days before the expiration date when an alert should be raised on the lot.
    #[field(
        integer,
        default = 0,
        groups = "stock.group_production_lot",
        string = "Alert Time (Days)"
    )]
    pub alert_time: i32,
}

#[methods(ProductTemplate, addon = "product_expiry")]
impl ProductTemplateExpiry {
    #[constrains("expiration_time", "use_time", "removal_time", "alert_time")]
    async fn _check_nonnegative_expiry_durations(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let exp_times = this
            .mapped(tx, ProductTemplateExpiry::EXPIRATION_TIME)
            .await?;
        let use_times = this.mapped(tx, ProductTemplateExpiry::USE_TIME).await?;
        let rem_times = this.mapped(tx, ProductTemplateExpiry::REMOVAL_TIME).await?;
        let alert_times = this.mapped(tx, ProductTemplateExpiry::ALERT_TIME).await?;
        for (((exp_t, use_t), rem_t), alert_t) in exp_times
            .into_iter()
            .zip(use_times)
            .zip(rem_times)
            .zip(alert_times)
        {
            if exp_t < 0 || use_t < 0 || rem_t < 0 || alert_t < 0 {
                return Err(Error::validation(
                    "Product expiration, best-before, removal, and alert durations cannot be negative.",
                ));
            }
        }
        Ok(())
    }
}

#[extend(StockLot, addon = "product_expiry")]
pub struct StockLotExpiry {
    /// Date after which the lot or serial number is expired and should not be shipped.
    #[field(datetime, string = "Expiration Date")]
    pub expiration_date: Option<DateTime>,
    /// Date after which the goods start losing quality.
    #[field(datetime, string = "Best Before Date")]
    pub use_date: Option<DateTime>,
    /// Date after which the goods should be removed from stock.
    #[field(datetime, string = "Removal Date")]
    pub removal_date: Option<DateTime>,
    /// Date on which an expiry alert becomes active for this lot.
    #[field(datetime, string = "Alert Date")]
    pub alert_date: Option<DateTime>,
    /// True once the lot's alert date or expiration date has been reached.
    #[field(
        boolean,
        compute = "_compute_product_expiry_alert",
        string = "Expiration Alert"
    )]
    pub product_expiry_alert: bool,
    /// True once the lot's expiration date is in the past.
    #[field(boolean, compute = "_compute_product_expiry_alert", string = "Expired")]
    pub product_expiry_reminded: bool,
}

#[methods(StockLot, addon = "product_expiry")]
impl StockLotExpiry {
    #[compute(depends = ["alert_date", "expiration_date"])]
    async fn _compute_product_expiry_alert(
        this: &RecordSet<StockLot>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockLot>,
    ) -> Result<()> {
        let now = DateTime::now();
        let alerts = this.mapped(tx, StockLotExpiry::ALERT_DATE).await?;
        let expiries = this.mapped(tx, StockLotExpiry::EXPIRATION_DATE).await?;
        for (index, (alert_dt, exp_dt)) in alerts.into_iter().zip(expiries).enumerate() {
            let expired = exp_dt.is_some_and(|dt| dt <= now);
            let alerted = expired || alert_dt.is_some_and(|dt| dt <= now);
            out.set(index, StockLotExpiry::PRODUCT_EXPIRY_ALERT, alerted)?;
            out.set(index, StockLotExpiry::PRODUCT_EXPIRY_REMINDED, expired)?;
        }
        Ok(())
    }

    /// Populate missing expiration/best-before/removal/alert dates from the product template
    /// when `use_expiration_date` is enabled.
    #[method(override)]
    async fn create(
        this: &RecordSet<StockLot>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let product_ids: Vec<i64> = vals_list
            .iter()
            .filter_map(|vals| match vals.get("product_id") {
                Some(Cell::Id(id)) => Some(*id),
                Some(Cell::Int(id)) => Some(i64::from(*id)),
                _ => None,
            })
            .collect();
        if !product_ids.is_empty() {
            let rows = tx
                .query(
                    "SELECT pp.id::int8, pt.use_expiration_date, pt.expiration_time, \
                            pt.use_time, pt.removal_time, pt.alert_time \
                     FROM product_product pp \
                     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
                     WHERE pp.id = ANY($1) AND pt.use_expiration_date IS TRUE",
                    &[&product_ids],
                )
                .await?;
            let mut rules: BTreeMap<i64, (i32, i32, i32, i32)> = BTreeMap::new();
            for row in rows {
                let decode = |e| Error::internal(format!("product expiry lookup: {e}"));
                let pid: i64 = row.try_get(0).map_err(decode)?;
                let exp_t: i32 = row.try_get(2).map_err(decode)?;
                let use_t: i32 = row.try_get(3).map_err(decode)?;
                let rem_t: i32 = row.try_get(4).map_err(decode)?;
                let alert_t: i32 = row.try_get(5).map_err(decode)?;
                rules.insert(pid, (exp_t, use_t, rem_t, alert_t));
            }
            let now = DateTime::now();
            for vals in &mut vals_list {
                let pid = match vals.get("product_id") {
                    Some(Cell::Id(id)) => Some(*id),
                    Some(Cell::Int(id)) => Some(i64::from(*id)),
                    _ => None,
                };
                if let Some((exp_t, use_t, rem_t, alert_t)) = pid.and_then(|id| rules.get(&id)) {
                    let dates = match vals.get("expiration_date") {
                        Some(Cell::DateTime(explicit_exp)) => compute_expiry_dates_from_expiration(
                            *explicit_exp,
                            *use_t,
                            *rem_t,
                            *alert_t,
                        )?,
                        _ => compute_expiry_dates(now, *exp_t, *use_t, *rem_t, *alert_t)?,
                    };
                    let set_if_missing =
                        |vals: &mut RawVals, key: &'static str, dt: Option<DateTime>| {
                            if let Some(dt) = dt
                                && matches!(vals.get(key), None | Some(Cell::Null))
                            {
                                vals.insert(key.into(), Cell::DateTime(dt));
                            }
                        };
                    set_if_missing(vals, "expiration_date", dates.expiration_date);
                    set_if_missing(vals, "use_date", dates.use_date);
                    set_if_missing(vals, "removal_date", dates.removal_date);
                    set_if_missing(vals, "alert_date", dates.alert_date);
                }
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}

#[extend(StockMoveLine, addon = "product_expiry")]
pub struct StockMoveLineExpiry {
    /// Expiration date recorded on this operation line (or copied from the selected lot).
    #[field(datetime, string = "Expiration Date")]
    pub expiration_date: Option<DateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_lot_expiry_lifecycle_dates_from_product_durations() {
        // 2026-01-01 00:00:00 UTC in microseconds
        let base = DateTime::from_micros(20_454 * MICROS_PER_DAY);
        let dates = compute_expiry_dates(base, 180, 30, 15, 60).expect("valid dates");
        let exp = dates.expiration_date.expect("expiration_date");
        assert_eq!(exp.micros() - base.micros(), 180 * MICROS_PER_DAY);
        assert_eq!(
            exp.micros() - dates.use_date.expect("use_date").micros(),
            30 * MICROS_PER_DAY
        );
        assert_eq!(
            exp.micros() - dates.removal_date.expect("removal_date").micros(),
            15 * MICROS_PER_DAY
        );
        assert_eq!(
            exp.micros() - dates.alert_date.expect("alert_date").micros(),
            60 * MICROS_PER_DAY
        );
    }

    #[test]
    fn zero_expiration_time_leaves_dates_empty() {
        let base = DateTime::from_micros(20_454 * MICROS_PER_DAY);
        let dates = compute_expiry_dates(base, 0, 10, 5, 5).expect("valid");
        assert_eq!(dates, ExpiryDates::default());
    }
}
