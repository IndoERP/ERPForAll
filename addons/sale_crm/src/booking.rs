// SPDX-License-Identifier: MIT OR Apache-2.0
//! Booked revenue is valued independently of the CRM opportunity's win amount.
use crate::SaleOrder;
use erp_sdk::prelude::*;

use crate::order::SalesOpportunity;

pub(super) fn refuse_override(values: &RawVals, context: &erp_sdk::Context) -> Result<()> {
    for field in ["crm_booking_amount", "crm_booking_currency_id"] {
        if values.contains(field) || context.contains(&format!("default_{field}")) {
            return Err(Error::validation(
                "Booking amounts and their currency are computed from confirmed sales orders.",
            ));
        }
    }
    Ok(())
}

#[extend(SaleOrder, addon = "sale_crm")]
pub struct SalesBooking {
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        store = true,
        string = "Booking Currency"
    )]
    pub crm_booking_currency_id: Option<RecId>,
    #[field(
        monetary,
        currency_field = "crm_booking_currency_id",
        compute = "_compute_crm_booking_amount",
        store = true,
        string = "Untaxed Booking"
    )]
    pub crm_booking_amount: Decimal,
}

#[methods(SaleOrder, addon = "sale_crm")]
impl SalesBooking {
    #[compute(depends = ["state", "opportunity_id", "currency_id", "crm_booking_currency_id", "currency_rate", "amount_untaxed"])]
    fn _compute_crm_booking_amount(batch: &mut Batch<'_, SaleOrder>) -> Result<()> {
        for i in batch.indices() {
            let booked = if batch.get(i, SaleOrder::STATE)?.as_deref() == Some("sale")
                && batch.get(i, SalesOpportunity::OPPORTUNITY_ID)?.is_some()
            {
                let currency = batch
                    .get(i, SaleOrder::CURRENCY_ID)?
                    .ok_or_else(|| Error::invariant("The order has no currency."))?;
                let company_currency = batch
                    .get(i, Self::CRM_BOOKING_CURRENCY_ID)?
                    .ok_or_else(|| Error::invariant("The booking has no company currency."))?;
                booking_in_company_currency(
                    batch.get(i, SaleOrder::AMOUNT_UNTAXED)?,
                    currency == company_currency,
                    batch.get(i, SaleOrder::CURRENCY_RATE)?,
                )?
            } else {
                Decimal::ZERO
            };
            // The ORM publishes a monetary compute at this company's currency increment.
            batch.set(i, Self::CRM_BOOKING_AMOUNT, booked)?;
        }
        Ok(())
    }
}

/// Sales stores foreign currency units per company currency unit on the order.
fn booking_in_company_currency(
    amount: Decimal,
    same_currency: bool,
    rate: Decimal,
) -> Result<Decimal> {
    if same_currency {
        return Ok(amount);
    }
    if rate <= Decimal::ZERO {
        return Err(Error::validation(
            "A foreign-currency booking needs a positive sales order exchange rate.",
        ));
    }
    amount
        .checked_div(rate)
        .ok_or_else(|| Error::validation("The booking exceeds the supported amount."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_sdk::Context;

    #[test]
    fn foreign_bookings_use_the_sales_order_rate_and_reject_missing_rates() {
        assert_eq!(
            booking_in_company_currency(120.into(), false, 2.into()).ok(),
            Some(60.into())
        );
        assert_eq!(
            booking_in_company_currency(120.into(), true, Decimal::ZERO).ok(),
            Some(120.into())
        );
        assert!(booking_in_company_currency(120.into(), false, Decimal::ZERO).is_err());
    }

    #[test]
    fn imports_and_defaults_cannot_set_derived_bookings() {
        let normal = RawVals::new().set_cell("partner_id", Cell::Id(3));
        assert!(refuse_override(&normal, &Context::new()).is_ok());
        for field in ["crm_booking_amount", "crm_booking_currency_id"] {
            assert!(
                refuse_override(&RawVals::new().set_cell(field, Cell::Null), &Context::new())
                    .is_err()
            );
            let mut context = Context::new();
            context.set(format!("default_{field}"), Value::Int(1));
            assert!(refuse_override(&normal, &context).is_err());
        }
    }
}
