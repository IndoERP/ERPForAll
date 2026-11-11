//! Pure exact unit conversions. Loading a unit and its root with caller access belongs to the model.
//!
//! The native arithmetic keeps ratios exact until an explicit rounding boundary. Model storage
//! uses [`exact_decimal`] and rejects any result that cannot be represented without digit loss.

use std::cmp::Ordering;
use std::sync::Arc;

use erp_sdk::money::{ExactRatio, Increment, MoneyError, RoundingMode};
use erp_sdk::{Decimal, Value};

use crate::error::UomError;

/// A resolved unit identity, reference root and nonzero absolute conversion factor.
///
/// The model service resolves the root and enforces the hierarchy's root-factor constraint before
/// constructing this immutable value. It has no process cache or database authority of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unit {
    id: i64,
    root: i64,
    factor: Decimal,
}

impl Unit {
    /// Validate resolved unit metadata. Negative nonzero factors remain supported.
    ///
    /// # Errors
    ///
    /// [`UomError::InvalidUnitId`], [`UomError::InvalidRootId`] or [`UomError::ZeroFactor`].
    pub fn new(id: i64, root: i64, factor: Decimal) -> Result<Self, UomError> {
        if id <= 0 {
            return Err(UomError::InvalidUnitId { id });
        }
        if root <= 0 {
            return Err(UomError::InvalidRootId { id: root });
        }
        if factor.is_zero() {
            return Err(UomError::ZeroFactor { id });
        }
        Ok(Self { id, root, factor })
    }

    /// The unit's record identifier.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Its resolved reference root's record identifier.
    pub fn root(&self) -> i64 {
        self.root
    }

    /// The exact persisted absolute factor relative to the root.
    pub fn factor(&self) -> Decimal {
        self.factor
    }
}

fn common_root(source: &Unit, target: &Unit) -> Result<(), UomError> {
    if source.root != target.root {
        return Err(UomError::IncompatibleRoots {
            source_id: source.id,
            target_id: target.id,
        });
    }
    Ok(())
}

/// Convert a quantity while retaining its exact ratio.
///
/// Roots are checked before zero and same-unit shortcuts (BUG-087).
///
/// # Errors
///
/// [`UomError::IncompatibleRoots`] for distinct dimensions; [`UomError::Numeric`] for bounded
/// arithmetic failure.
///
/// ```
/// use erp_addon_uom::conversion::{Unit, quantity_exact};
/// use erp_sdk::Decimal;
/// let one = Unit::new(1, 1, Decimal::ONE)?;
/// let six = Unit::new(2, 1, Decimal::from(6))?;
/// assert_eq!(quantity_exact(&six, &one, Decimal::from(2))?.to_string(), "12");
/// # Ok::<(), erp_addon_uom::error::UomError>(())
/// ```
pub fn quantity_exact(
    source: &Unit,
    target: &Unit,
    quantity: Decimal,
) -> Result<ExactRatio, UomError> {
    common_root(source, target)?;
    let quantity = ExactRatio::from_decimal(quantity)?;
    if source.id == target.id || quantity.is_zero() {
        return Ok(quantity);
    }
    Ok(quantity
        .checked_mul(&ExactRatio::from_decimal(source.factor)?)?
        .checked_div(&ExactRatio::from_decimal(target.factor)?)?)
}

/// Convert a unit price using the inverse quantity ratio, without rounding.
///
/// # Errors
///
/// As [`quantity_exact`], including root validation for zero prices.
pub fn price_exact(source: &Unit, target: &Unit, price: Decimal) -> Result<ExactRatio, UomError> {
    common_root(source, target)?;
    let price = ExactRatio::from_decimal(price)?;
    if source.id == target.id || price.is_zero() {
        return Ok(price);
    }
    Ok(price
        .checked_mul(&ExactRatio::from_decimal(target.factor)?)?
        .checked_div(&ExactRatio::from_decimal(source.factor)?)?)
}

/// Convert and round a quantity at the caller's named precision boundary.
///
/// The model supplies Product Unit precision and defaults the mode to UP
/// ([`RoundingMode::AwayFromZero`]); this primitive requires both explicitly.
///
/// # Errors
///
/// As [`quantity_exact`], with numeric failure when the rounded result exceeds Decimal's range.
pub fn quantity_rounded(
    source: &Unit,
    target: &Unit,
    quantity: Decimal,
    precision: &Increment,
    mode: RoundingMode,
) -> Result<Decimal, UomError> {
    Ok(quantity_exact(source, target, quantity)?.quantize(precision, mode)?)
}

/// Round a quantity at an explicit Product Unit precision.
///
/// # Errors
///
/// [`UomError::Numeric`] when the rounded value is outside the decimal boundary.
pub fn precision_round(
    value: Decimal,
    precision: &Increment,
    mode: RoundingMode,
) -> Result<Decimal, UomError> {
    Ok(precision.quantize(value, mode)?)
}

/// Compare independently HALF-UP rounded operands, rather than rounding their difference.
///
/// # Errors
///
/// As [`precision_round`].
pub fn precision_compare(
    left: Decimal,
    right: Decimal,
    precision: &Increment,
) -> Result<Ordering, UomError> {
    Ok(precision.compare(left, right, RoundingMode::HalfAwayFromZero)?)
}

/// Whether a HALF-UP rounded quantity is zero at the given precision.
///
/// # Errors
///
/// As [`precision_round`].
pub fn precision_is_zero(value: Decimal, precision: &Increment) -> Result<bool, UomError> {
    Ok(precision.is_zero(value, RoundingMode::HalfAwayFromZero)?)
}

/// Round a product quantity to an integer multiple of the converted one-pack quantity.
///
/// One pack is first converted with UP at Product Unit precision. The number of packs is then
/// rounded using `mode`. Equal units preserve the quantity after common-root validation.
///
/// # Errors
///
/// As [`quantity_rounded`]; [`UomError::RoundedZeroPack`] if the converted pack becomes zero;
/// [`UomError::Numeric`] if the final exact product cannot fit Decimal without loss.
pub fn packaging_quantity(
    packaging: &Unit,
    target: &Unit,
    quantity: Decimal,
    precision: &Increment,
    mode: RoundingMode,
) -> Result<Decimal, UomError> {
    common_root(packaging, target)?;
    if packaging.id == target.id {
        return Ok(quantity);
    }
    let one_pack = quantity_rounded(
        packaging,
        target,
        Decimal::ONE,
        precision,
        RoundingMode::AwayFromZero,
    )?;
    // With validated nonzero factors and UP this is defensive: a future configurable one-pack
    // boundary must never turn a zero pack into division by zero or a silent result.
    if one_pack.is_zero() {
        return Err(UomError::RoundedZeroPack {
            source_id: packaging.id,
            target_id: target.id,
        });
    }
    let packs = ExactRatio::quotient(quantity, one_pack)?.quantize(&Increment::unit(), mode)?;
    let result =
        ExactRatio::from_decimal(packs)?.checked_mul(&ExactRatio::from_decimal(one_pack)?)?;
    exact_decimal(&result)?.ok_or(UomError::Numeric(MoneyError::NumericLimit {
        operation: "uom packaging",
    }))
}

/// Represent an exact ratio as a Decimal only if no digit is lost.
///
/// `None` means the value is within Decimal's magnitude but does not terminate in a representable
/// coefficient/scale. Storage callers must reject `None`; it is never a rounded storage value.
/// Every returned candidate is converted back to a ratio and checked for exact equality.
///
/// # Errors
///
/// [`UomError::Numeric`] for a value outside Decimal's magnitude, or an unexpected primitive error.
pub fn exact_decimal(value: &ExactRatio) -> Result<Option<Decimal>, UomError> {
    let maximum = ExactRatio::from_decimal(Decimal::MAX)?;
    let minimum = ExactRatio::from_decimal(Decimal::MIN)?;
    if value > &maximum || value < &minimum {
        return Err(UomError::Numeric(MoneyError::NumericLimit {
            operation: "uom exact decimal",
        }));
    }
    for places in 0..=28 {
        let precision = Increment::from_places(places)?;
        match value.quantize(&precision, RoundingMode::TowardZero) {
            Ok(candidate) => {
                if ExactRatio::from_decimal(candidate)? == *value {
                    return Ok(Some(candidate));
                }
            }
            Err(MoneyError::NumericLimit { .. }) => {
                // Magnitude was checked above. A Decimal at this scale cannot fit its
                // coefficient, and every finer scale needs an even larger coefficient.
                // Thus there is no exact remaining candidate; this is representability
                // classification, not a rounded fallback or a suppressed storage overflow.
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

/// Lossless exact RPC representation: a Decimal when representable, otherwise canonical
/// numerator/denominator strings. The ratio pair also supports native results beyond Decimal's
/// magnitude; it does not weaken the stricter persisted-factor boundary.
///
/// # Errors
///
/// [`UomError::Numeric`] for unexpected exact-number primitive failure.
pub fn rpc_exact(value: &ExactRatio) -> Result<Value, UomError> {
    match exact_decimal(value) {
        Ok(Some(decimal)) => Ok(Value::Decimal(decimal)),
        Ok(None)
        | Err(UomError::Numeric(MoneyError::NumericLimit {
            operation: "uom exact decimal",
        })) => Ok(Value::Map(Arc::new(
            [
                ("numerator".to_owned(), Value::str(value.numerator_string())),
                (
                    "denominator".to_owned(),
                    Value::str(value.denominator_string()),
                ),
            ]
            .into_iter()
            .collect(),
        ))),
        Err(error) => Err(error),
    }
}
