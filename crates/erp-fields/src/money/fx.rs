//! An FX snapshot: the rates a conversion used, with their identity (H41).
//!
//! Dated FX *lookup* is a base service (it reads the database); this type is the pure result so
//! an accounting adapter can store and replay exactly what was applied, and a kernel can convert
//! without any database access.

use rust_decimal::Decimal;

use super::amount::CurrencyCode;
use super::error::MoneyError;
use super::ratio::ExactRatio;
use crate::Date;

/// Which stored rates are candidates for a requested date.
///
/// [`RateDateRule::OnOrBefore`] also accepts a rate
/// dated on the requested day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RateDateRule {
    StrictlyBefore,
    /// `rate.date <= requested date`.
    OnOrBefore,
}

/// Where a rate came from relative to the requested date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RateOrigin {
    /// The latest rate allowed by the [`RateDateRule`].
    Preceding,
    /// Callers that must not accept this inspect the origin.
    LaterFallback,
    /// The company's own currency, which is the reference of its rates and so converts at exactly
    /// one by definition; no stored row is involved and none is needed.
    ReferenceCurrency,
}

/// One stored rate and its identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateRef {
    rate: Decimal,
    rate_id: Option<i64>,
    rate_date: Option<Date>,
    origin: RateOrigin,
}

impl RateRef {
    /// A rate and where it came from.
    ///
    /// # Errors
    ///
    /// [`MoneyError::InvalidRate`] when `rate` is not strictly positive.
    pub fn new(
        rate: Decimal,
        rate_id: i64,
        rate_date: Date,
        origin: RateOrigin,
    ) -> Result<Self, MoneyError> {
        if rate.is_sign_negative() || rate.is_zero() {
            return Err(MoneyError::InvalidRate);
        }
        Ok(Self {
            rate,
            rate_id: Some(rate_id),
            rate_date: Some(rate_date),
            origin,
        })
    }

    /// The rate of the reference currency itself: exactly one, no stored row
    /// ([`RateOrigin::ReferenceCurrency`]).
    pub fn reference() -> Self {
        Self {
            rate: Decimal::ONE,
            rate_id: None,
            rate_date: None,
            origin: RateOrigin::ReferenceCurrency,
        }
    }

    /// The rate.
    pub fn rate(&self) -> Decimal {
        self.rate
    }

    /// The id of the stored `res.currency.rate` row; `None` for the reference currency.
    pub fn rate_id(&self) -> Option<i64> {
        self.rate_id
    }

    /// The date of the stored rate; `None` for the reference currency.
    pub fn rate_date(&self) -> Option<Date> {
        self.rate_date
    }

    /// Whether the rate precedes the requested date or is the later fallback.
    pub fn origin(&self) -> RateOrigin {
        self.origin
    }
}

/// The conversion `from -> to` for one company on one date, with the rates it used.
///
/// Direction is fixed and explicit: `amount_in_to = amount_in_from * to_rate / from_rate`.
///
/// ```
/// use erp_fields::money::{
///     CurrencyCode, FxSnapshot, RateDateRule, RateOrigin, RateRef, parse_decimal,
/// };
/// use erp_fields::{Date, Decimal};
///
/// let usd = CurrencyCode::new("USD")?;
/// let eur = CurrencyCode::new("EUR")?;
/// let day = Date::from_ymd(2026, 6, 1).expect("a real date");
/// let usd_rate = RateRef::new(Decimal::ONE, 1, day, RateOrigin::Preceding)?;
/// let eur_rate = RateRef::new(parse_decimal("0.85")?, 2, day, RateOrigin::Preceding)?;
/// let fx = FxSnapshot::between(usd, eur, 1, day, RateDateRule::StrictlyBefore, usd_rate, eur_rate)?;
/// assert_eq!(fx.convert_exact(parse_decimal("100")?)?.to_string(), "85");
/// # Ok::<(), erp_fields::money::MoneyError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FxSnapshot {
    from: CurrencyCode,
    to: CurrencyCode,
    company_id: i64,
    date: Date,
    rule: RateDateRule,
    legs: Option<(RateRef, RateRef)>,
}

impl FxSnapshot {
    /// The identity conversion of one currency (no rate is read or recorded).
    pub fn identity(
        currency: CurrencyCode,
        company_id: i64,
        date: Date,
        rule: RateDateRule,
    ) -> Self {
        Self {
            from: currency,
            to: currency,
            company_id,
            date,
            rule,
            legs: None,
        }
    }

    /// A conversion between two different currencies.
    ///
    /// # Errors
    ///
    /// [`MoneyError::InvalidRate`] when both codes are equal: a rate pair between a currency and
    /// itself is a caller bug (use [`FxSnapshot::identity`]).
    pub fn between(
        from: CurrencyCode,
        to: CurrencyCode,
        company_id: i64,
        date: Date,
        rule: RateDateRule,
        from_rate: RateRef,
        to_rate: RateRef,
    ) -> Result<Self, MoneyError> {
        if from == to {
            return Err(MoneyError::InvalidRate);
        }
        Ok(Self {
            from,
            to,
            company_id,
            date,
            rule,
            legs: Some((from_rate, to_rate)),
        })
    }

    /// Source currency.
    pub fn from_currency(&self) -> CurrencyCode {
        self.from
    }

    /// Target currency.
    pub fn to_currency(&self) -> CurrencyCode {
        self.to
    }

    /// The (root) company whose rates were read.
    pub fn company_id(&self) -> i64 {
        self.company_id
    }

    /// The requested date.
    pub fn date(&self) -> Date {
        self.date
    }

    /// The date rule that selected the rates.
    pub fn rule(&self) -> RateDateRule {
        self.rule
    }

    /// The source and target rates; `None` for the identity conversion.
    pub fn rates(&self) -> Option<(&RateRef, &RateRef)> {
        self.legs.as_ref().map(|(a, b)| (a, b))
    }

    /// The multiplier from `from` to `to` as an exact ratio (`1` for the identity).
    ///
    /// # Errors
    ///
    /// [`MoneyError::NumericLimit`] only for working values beyond the bound.
    pub fn factor(&self) -> Result<ExactRatio, MoneyError> {
        match &self.legs {
            None => Ok(ExactRatio::from_i64(1)),
            Some((from_rate, to_rate)) => ExactRatio::quotient(to_rate.rate, from_rate.rate),
        }
    }

    /// `amount` converted, exactly and unrounded; quantize it to the target currency's increment.
    ///
    /// # Errors
    ///
    /// As [`FxSnapshot::factor`] and [`ExactRatio::checked_mul`].
    pub fn convert_exact(&self, amount: Decimal) -> Result<ExactRatio, MoneyError> {
        ExactRatio::from_decimal(amount)?.checked_mul(&self.factor()?)
    }
}
