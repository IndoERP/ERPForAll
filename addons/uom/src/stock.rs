// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock-facing unit conversion: the rounding rule is named at every call and the factors and
//! digits a stored quantity was computed with travel together as one snapshot (6/C3).

use erp_sdk::money::{ExactRatio, Increment, RoundingMode};
use erp_sdk::prelude::*;

use crate::conversion::Unit;
use crate::error::UomError;
use crate::models::{load_unit, product_unit_digits};
use crate::references::{ReferencedUnit, register};

/// The rule a stock conversion rounds with. There is no default: every call names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockRounding {
    Up,
    HalfUp,
}

impl StockRounding {
    fn mode(self) -> RoundingMode {
        match self {
            Self::Up => RoundingMode::AwayFromZero,
            Self::HalfUp => RoundingMode::HalfAwayFromZero,
        }
    }
}

/// A pure conversion of a line quantity into the product unit: two units of one root, their exact
/// factors and the Product Unit digits. It reads no database and registers nothing, so it is for
/// calculation, previews and tests. A value stored with a stock row is a [`ProductUnitSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductUnitPreview {
    line_unit: i64,
    product_unit: i64,
    root: i64,
    source_factor: Decimal,
    target_factor: Decimal,
    digits: u32,
}

impl ProductUnitPreview {
    /// Build a preview from plain units and the Product Unit digits.
    ///
    /// # Errors
    ///
    /// [`UomError::IncompatibleRoots`] naming both units when their roots differ;
    /// [`UomError::InvalidDigits`] when `digits` is above 28.
    pub fn new(line: &Unit, product: &Unit, digits: u32) -> Result<Self, UomError> {
        if digits > 28 {
            return Err(UomError::InvalidDigits { digits });
        }
        if line.root() != product.root() {
            return Err(UomError::IncompatibleRoots {
                source_id: line.id(),
                target_id: product.id(),
            });
        }
        Ok(Self {
            line_unit: line.id(),
            product_unit: product.id(),
            root: line.root(),
            source_factor: line.factor(),
            target_factor: product.factor(),
            digits,
        })
    }

    /// The line's unit.
    pub fn line_unit(&self) -> i64 {
        self.line_unit
    }

    /// The product's unit.
    pub fn product_unit(&self) -> i64 {
        self.product_unit
    }

    /// The common reference root.
    pub fn root(&self) -> i64 {
        self.root
    }

    /// The line unit's exact factor.
    pub fn source_factor(&self) -> Decimal {
        self.source_factor
    }

    /// The product unit's exact factor.
    pub fn target_factor(&self) -> Decimal {
        self.target_factor
    }

    /// The Product Unit digits quantities are rounded to.
    pub fn digits(&self) -> u32 {
        self.digits
    }

    /// Line quantity to product unit, exact: no rounding.
    ///
    /// # Errors
    ///
    /// [`UomError::Numeric`] when the exact value exceeds the bounded arithmetic.
    pub fn line_to_product_exact(&self, qty: Decimal) -> Result<ExactRatio, UomError> {
        Self::scale(qty, self.source_factor, self.target_factor)
    }

    /// Product quantity to line unit, exact: no rounding.
    ///
    /// # Errors
    ///
    /// As [`ProductUnitPreview::line_to_product_exact`].
    pub fn product_to_line_exact(&self, qty: Decimal) -> Result<ExactRatio, UomError> {
        Self::scale(qty, self.target_factor, self.source_factor)
    }

    fn scale(
        qty: Decimal,
        numerator: Decimal,
        denominator: Decimal,
    ) -> Result<ExactRatio, UomError> {
        Ok(ExactRatio::from_decimal(qty)?
            .checked_mul(&ExactRatio::from_decimal(numerator)?)?
            .checked_div(&ExactRatio::from_decimal(denominator)?)?)
    }

    fn round(&self, exact: &ExactRatio, rule: StockRounding) -> Result<Decimal, UomError> {
        Ok(exact.quantize(&Increment::from_places(self.digits)?, rule.mode())?)
    }

    /// Line quantity in the product unit, rounded once at the digits with the named rule.
    ///
    /// # Errors
    ///
    /// [`UomError::Numeric`] when the exact value or the rounded result exceeds the bounded
    /// arithmetic or the decimal range.
    pub fn line_to_product(&self, qty: Decimal, rule: StockRounding) -> Result<Decimal, UomError> {
        self.round(&self.line_to_product_exact(qty)?, rule)
    }

    /// Product quantity in the line unit, rounded once at the digits with the named rule.
    ///
    /// # Errors
    ///
    /// As [`ProductUnitPreview::line_to_product`].
    pub fn product_to_line(&self, qty: Decimal, rule: StockRounding) -> Result<Decimal, UomError> {
        self.round(&self.product_to_line_exact(qty)?, rule)
    }

    /// The product quantity of a line: exact `qty * source / target`, HALF-UP at the digits.
    /// This is the oracle [`product_quantity_sql`] is tested against.
    ///
    /// # Errors
    ///
    /// As [`ProductUnitPreview::line_to_product`].
    pub fn product_quantity(&self, qty: Decimal) -> Result<Decimal, UomError> {
        self.line_to_product(qty, StockRounding::HalfUp)
    }
}

/// The preview of two units that were registered as used in this transaction.
///
/// Only [`ProductUnitSnapshot::new`] (from [`ReferencedUnit`]s) and [`capture`] build one, so a
/// value stored with a stock row always names protected units. Conversions delegate to the
/// preview, reachable through [`ProductUnitSnapshot::preview`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductUnitSnapshot {
    preview: ProductUnitPreview,
}

impl ProductUnitSnapshot {
    /// Build a snapshot from two registered units and the Product Unit digits.
    ///
    /// # Errors
    ///
    /// As [`ProductUnitPreview::new`].
    pub fn new(
        line: &ReferencedUnit,
        product: &ReferencedUnit,
        digits: u32,
    ) -> Result<Self, UomError> {
        Ok(Self {
            preview: ProductUnitPreview::new(
                &Unit::new(line.id(), line.root(), line.factor())?,
                &Unit::new(product.id(), product.root(), product.factor())?,
                digits,
            )?,
        })
    }

    /// The pure conversion this snapshot carries.
    pub fn preview(&self) -> &ProductUnitPreview {
        &self.preview
    }

    /// The line's unit.
    pub fn line_unit(&self) -> i64 {
        self.preview.line_unit()
    }

    /// The product's unit.
    pub fn product_unit(&self) -> i64 {
        self.preview.product_unit()
    }

    /// The common reference root.
    pub fn root(&self) -> i64 {
        self.preview.root()
    }

    /// The line unit's exact factor.
    pub fn source_factor(&self) -> Decimal {
        self.preview.source_factor()
    }

    /// The product unit's exact factor.
    pub fn target_factor(&self) -> Decimal {
        self.preview.target_factor()
    }

    /// The Product Unit digits quantities are rounded to.
    pub fn digits(&self) -> u32 {
        self.preview.digits()
    }

    /// See [`ProductUnitPreview::line_to_product_exact`].
    ///
    /// # Errors
    ///
    /// As the preview.
    pub fn line_to_product_exact(&self, qty: Decimal) -> Result<ExactRatio, UomError> {
        self.preview.line_to_product_exact(qty)
    }

    /// See [`ProductUnitPreview::product_to_line_exact`].
    ///
    /// # Errors
    ///
    /// As the preview.
    pub fn product_to_line_exact(&self, qty: Decimal) -> Result<ExactRatio, UomError> {
        self.preview.product_to_line_exact(qty)
    }

    /// See [`ProductUnitPreview::line_to_product`].
    ///
    /// # Errors
    ///
    /// As the preview.
    pub fn line_to_product(&self, qty: Decimal, rule: StockRounding) -> Result<Decimal, UomError> {
        self.preview.line_to_product(qty, rule)
    }

    /// See [`ProductUnitPreview::product_to_line`].
    ///
    /// # Errors
    ///
    /// As the preview.
    pub fn product_to_line(&self, qty: Decimal, rule: StockRounding) -> Result<Decimal, UomError> {
        self.preview.product_to_line(qty, rule)
    }

    /// See [`ProductUnitPreview::product_quantity`].
    ///
    /// # Errors
    ///
    /// As the preview.
    pub fn product_quantity(&self, qty: Decimal) -> Result<Decimal, UomError> {
        self.preview.product_quantity(qty)
    }
}

/// Register both units as used in this transaction and capture their snapshot with the
/// current Product Unit digits.
///
/// Call it in the plan phase, after the prelude and before quant keys.
/// Both units are read and their roots compared before anything is written, so a refusal
/// (different roots) leaves no marker even if the caller catches the error and commits.
///
/// # Errors
///
/// Everything [`register`] returns, a database error reading the digits, and the errors of
/// [`ProductUnitSnapshot::new`].
pub async fn capture(
    env: &Env,
    tx: &mut Transaction,
    line_unit: i64,
    product_unit: i64,
) -> Result<ProductUnitSnapshot> {
    // Units, roots and digits are read and validated on a read-only path first, so a refusal
    // for a root mismatch, a bad digits setting or a failed read writes nothing, also when the
    // caller catches the error and commits. A later failure of `register` itself is rolled back
    // with the transaction like any other error of it.
    let (line_read, product_read) = (
        load_unit(env, tx, line_unit).await?,
        load_unit(env, tx, product_unit).await?,
    );
    let digits = product_unit_digits(env, tx).await?;
    ProductUnitPreview::new(&line_read, &product_read, digits)?;
    let units = register(env, tx, &[line_unit, product_unit]).await?;
    let pick = |id: i64| {
        units
            .iter()
            .find(|unit| unit.id() == id)
            .ok_or_else(|| Error::programming("Unit reference guard omitted a requested unit."))
    };
    let (line, product) = (pick(line_unit)?, pick(product_unit)?);
    Ok(ProductUnitSnapshot::new(line, product, digits)?)
}

fn identifier(text: &str) -> Result<&str, UomError> {
    let bytes = text.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0] == b'_' || bytes[0].is_ascii_lowercase())
        && bytes
            .iter()
            .all(|b| *b == b'_' || b.is_ascii_lowercase() || b.is_ascii_digit());
    if valid {
        Ok(text)
    } else {
        Err(UomError::InvalidSqlIdentifier {
            identifier: text.to_owned(),
        })
    }
}

/// The SQL of [`ProductUnitPreview::product_quantity`] over four same-row `numeric`/`integer`
/// columns, for a `GENERATED ALWAYS AS (...) STORED` column. The columns must be `NOT NULL`
/// (a NULL input stores NULL), the factors nonzero (a zero target is a division error) and
/// `digits` an integer from 0 to 28; add the NaN checks.2 on the quantity.
///
/// Builtins only and IMMUTABLE. `n = qty * source * 10^digits` is exact; `div(n, target)` is
/// the exact truncated quotient; one is added in the direction of the sign when twice the
/// remainder reaches the divisor (ties away from zero). The scale factor is `10^-digits`, and
/// the outer `round(..., digits)` only fixes the displayed scale (the value already has at most
/// `digits` decimals). Rounding the division first
/// (`round(qty * source / target, digits)`) can double-round near ties.
///
/// # Errors
///
/// [`UomError::InvalidSqlIdentifier`] when a name is not `^[a-z_][a-z0-9_]{0,62}$`.
pub fn product_quantity_sql(
    qty: &str,
    source: &str,
    target: &str,
    digits: &str,
) -> Result<String, UomError> {
    // Quoted, so a name that is also a keyword or function (`user`, `current_date`) stays a column.
    let (q, s, t, d) = (
        format!("\"{}\"", identifier(qty)?),
        format!("\"{}\"", identifier(source)?),
        format!("\"{}\"", identifier(target)?),
        format!("\"{}\"", identifier(digits)?),
    );
    let n = format!("({q} * {s} * (10::numeric ^ {d}::numeric))");
    Ok(format!(
        "round((div({n}, {t}) + CASE WHEN 2 * abs({n} - div({n}, {t}) * {t}) >= abs({t}) \
         THEN sign({n}) * sign({t}) ELSE 0 END) * (10::numeric ^ (-{d}::numeric)), {d})"
    ))
}
