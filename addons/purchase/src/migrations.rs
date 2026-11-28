// SPDX-License-Identifier: MIT OR Apache-2.0
//! Purchase upgrade steps: backfill `purchase_order.currency_rate` before schema sync (1.11.0).

use std::collections::BTreeMap;

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RateBook};
use erp_sdk::ErrorKind;
use erp_sdk::migrations::SemverVersion;
use erp_sdk::money::{ExactRatio, Increment, RateDateRule};
use erp_sdk::prelude::*;

const BATCH_SIZE: i64 = 1024;

fn db_error(what: &str, error: &dyn std::fmt::Display) -> Error {
    Error::db(format!(
        "purchase 1.11.0 currency_rate backfill, {what}: {error}"
    ))
}

/// Derive the historical exchange rate of a pre-1.11 foreign-currency order when no rate row
/// exists for its currency/company/date (for example, orders created before the company changed
/// its base currency). Preserves `amount_total / currency_rate == amount_total_cc`, uses `1`
/// only when both amounts are zero, and refuses inconsistent rows with the missing-rate error.
fn resolve_legacy_rate(
    total: Option<Decimal>,
    total_cc: Option<Decimal>,
    precision: &Increment,
    missing_rate_error: &str,
) -> Result<Decimal> {
    let total = total.unwrap_or(Decimal::ZERO);
    let total_cc = total_cc.unwrap_or(Decimal::ZERO);
    if total.is_zero() && total_cc.is_zero() {
        return Ok(Decimal::ONE);
    }
    if (total > Decimal::ZERO && total_cc > Decimal::ZERO)
        || (total < Decimal::ZERO && total_cc < Decimal::ZERO)
    {
        let rate =
            ExactRatio::quotient(total, total_cc)?.quantize(precision, CURRENCY_ROUNDING_MODE)?;
        if rate <= Decimal::ZERO {
            return Err(Error::validation(
                "Purchase order exchange rate is below the supported precision.",
            ));
        }
        return Ok(rate);
    }
    Err(Error::user(missing_rate_error))
}

/// Backfill `purchase_order.currency_rate` before schema sync adds the stored computed column.
///
/// Uses the exact same `RateBook` lookup as `PurchaseOrder::_compute_currency_rate` whenever a
/// spot exchange rate exists. For historical pre-1.11 foreign-currency orders whose currency has
/// no rate row on the order date (e.g. created under a former company currency), falls back to
/// the stored `amount_total / amount_total_cc` ratio so upgrades preserve existing totals without
/// relaxing strict rate checks on new or edited orders.
#[erp_sdk::migration(addon = "purchase", version = "1.11.0", stage = pre)]
async fn backfill_order_currency_rate(tx: &mut Transaction, _from: &SemverVersion) -> Result<()> {
    let table_rows = tx
        .query("SELECT to_regclass('purchase_order') IS NOT NULL", &[])
        .await?;
    let table_exists: bool = table_rows
        .first()
        .ok_or_else(|| Error::internal("Purchase migration table probe returned no row."))?
        .try_get(0)
        .map_err(|e| db_error("table probe", &e))?;
    if !table_exists {
        return Ok(());
    }

    let col_rows = tx
        .query(
            "SELECT EXISTS ( \
                SELECT 1 FROM information_schema.columns \
                WHERE table_schema = current_schema() \
                  AND table_name = 'purchase_order' \
                  AND column_name = 'currency_rate' \
            )",
            &[],
        )
        .await?;
    let col_exists: bool = col_rows
        .first()
        .ok_or_else(|| Error::internal("Purchase migration column probe returned no row."))?
        .try_get(0)
        .map_err(|e| db_error("column probe", &e))?;
    if col_exists {
        return Ok(());
    }

    tx.execute(
        "ALTER TABLE purchase_order ADD COLUMN currency_rate numeric",
        &[],
    )
    .await?;

    let env = tx.env().sudo(true);
    let precision = Increment::from_places(12)?;
    let mut rates: BTreeMap<(RecId, RecId, RecId, Date), std::result::Result<Decimal, String>> =
        BTreeMap::new();
    let mut last_id: i64 = 0;

    loop {
        let rows = tx
            .query(
                "SELECT po.id::int8, po.company_id::int8, c.currency_id::int8, \
                        po.currency_id::int8, po.date_order, po.amount_total, po.amount_total_cc \
                 FROM purchase_order po \
                 LEFT JOIN res_company c ON c.id = po.company_id \
                 WHERE po.id > $1 \
                 ORDER BY po.id \
                 LIMIT $2",
                &[&last_id, &BATCH_SIZE],
            )
            .await?;
        if rows.is_empty() {
            break;
        }

        let mut ids = Vec::with_capacity(rows.len());
        let mut values = Vec::with_capacity(rows.len());

        for row in &rows {
            let id: i64 = row.try_get(0).map_err(|e| db_error("id", &e))?;
            let company = row
                .try_get::<_, Option<i64>>(1)
                .map_err(|e| db_error("company_id", &e))?
                .and_then(RecId::from_real);
            let reference = row
                .try_get::<_, Option<i64>>(2)
                .map_err(|e| db_error("company.currency_id", &e))?
                .and_then(RecId::from_real);
            let currency = row
                .try_get::<_, Option<i64>>(3)
                .map_err(|e| db_error("currency_id", &e))?
                .and_then(RecId::from_real);
            let date: Option<DateTime> = row.try_get(4).map_err(|e| db_error("date_order", &e))?;
            let total: Option<Decimal> =
                row.try_get(5).map_err(|e| db_error("amount_total", &e))?;
            let total_cc: Option<Decimal> = row
                .try_get(6)
                .map_err(|e| db_error("amount_total_cc", &e))?;

            let rate = match (company, reference, currency, date) {
                (_, Some(reference), Some(currency), _) if currency == reference => Decimal::ONE,
                (Some(company), Some(reference), Some(currency), Some(date)) => {
                    let key = (company, reference, currency, date.date());
                    let looked_up = if let Some(cached) = rates.get(&key) {
                        cached.clone()
                    } else {
                        let book = RateBook::load(
                            &env,
                            tx,
                            company,
                            date.date(),
                            RateDateRule::StrictlyBefore,
                            &[reference, currency],
                        )
                        .await?;
                        let outcome = match book.snapshot(reference, currency) {
                            Ok(snapshot) => {
                                let factor = snapshot
                                    .factor()?
                                    .quantize(&precision, CURRENCY_ROUNDING_MODE)?;
                                if factor <= Decimal::ZERO {
                                    return Err(Error::validation(
                                        "Purchase order exchange rate is below the supported precision.",
                                    ));
                                }
                                Ok(factor)
                            }
                            Err(err) if err.kind() == ErrorKind::UserError => Err(err.to_string()),
                            Err(err) => return Err(err),
                        };
                        rates.insert(key, outcome.clone());
                        outcome
                    };
                    match looked_up {
                        Ok(rate) => rate,
                        Err(missing_msg) => {
                            resolve_legacy_rate(total, total_cc, &precision, &missing_msg)?
                        }
                    }
                }
                _ => Decimal::ZERO,
            };

            ids.push(id);
            values.push(rate);
            last_id = id;
        }

        tx.execute(
            "UPDATE purchase_order po \
             SET currency_rate = v.rate \
             FROM unnest($1::int8[], $2::numeric[]) AS v(id, rate) \
             WHERE po.id = v.id",
            &[&ids, &values],
        )
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_rate_derives_exact_ratio_from_stored_totals() {
        let precision = Increment::from_places(12).unwrap();
        let rate = resolve_legacy_rate(
            Some(Decimal::from(100)),
            Some(Decimal::from(1_600_000)),
            &precision,
            "missing",
        )
        .unwrap();
        assert_eq!(rate, Decimal::new(625, 7)); // 0.0000625
    }

    #[test]
    fn legacy_rate_accepts_same_currency_ratio_one() {
        let precision = Increment::from_places(12).unwrap();
        let rate = resolve_legacy_rate(
            Some(Decimal::new(12550, 2)),
            Some(Decimal::new(12550, 2)),
            &precision,
            "missing",
        )
        .unwrap();
        assert_eq!(rate, Decimal::ONE);
    }

    #[test]
    fn legacy_rate_returns_one_for_zero_amount_orders() {
        let precision = Increment::from_places(12).unwrap();
        let rate = resolve_legacy_rate(
            Some(Decimal::ZERO),
            Some(Decimal::ZERO),
            &precision,
            "missing",
        )
        .unwrap();
        assert_eq!(rate, Decimal::ONE);
    }

    #[test]
    fn legacy_rate_refuses_inconsistent_nonzero_and_zero_totals() {
        let precision = Increment::from_places(12).unwrap();
        let err = resolve_legacy_rate(
            Some(Decimal::from(250)),
            Some(Decimal::ZERO),
            &precision,
            "No exchange rate is defined for currency USD",
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError);
        assert!(err.to_string().contains("USD"));
    }
}
