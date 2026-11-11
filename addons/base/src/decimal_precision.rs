//! `decimal.precision`: named decimal precision definitions and their lookup (FIELD §4.3,
//!
//!   for callers that must not
//!   accept the default.
//!   Nothing else is exposed:
//!   the caller passes one bounded name and gets one integer back.
//! - [`field_scale`] resolves the decimals of one field of one model: a fixed `digits=(p, s)`, a
//!   named precision through the table, an unrounded `numeric`, or the currency a monetary field
//!   follows. It reads the registry and at most one row.

use erp_sdk::prelude::*;
use erp_sdk::{Digits, FieldType, Load, Operator, SearchOptions};

pub const DEFAULT_PRECISION_DIGITS: u32 = 2;

/// The longest usage name the lookup accepts.
pub const MAX_USAGE_BYTES: usize = 128;

/// `decimal.precision`.
#[model(
    name = "decimal.precision",
    description = "Decimal Precision",
    constraints = [
        ("_name_uniq", "unique (name)", "Only one value can be defined for each given usage!"),
        (
            "_digits_range",
            "CHECK (digits >= 0 AND digits <= 28)",
            "The digits of a decimal precision must be between 0 and 28."
        ),
    ],
)]
pub struct DecimalPrecision {
    /// Application name (e.g. "Product Price", "Account").
    #[field(char, required = true, index = true)]
    pub name: Option<Arc<str>>,
    /// Number of decimal digits.
    #[field(integer, default = 2, required = true)]
    pub digits: i32,
}

#[methods(DecimalPrecision)]
impl DecimalPrecision {
    /// Native metadata adapter for the ORM's named field descriptions.
    ///
    /// Uses the same bounded, digits-only lookup as [`precision_get`]. This method is not
    /// exported over RPC; target-model field access remains the ORM caller's responsibility.
    #[method]
    async fn _precision_get(
        this: &RecordSet<DecimalPrecision>,
        tx: &mut Transaction,
        usage: String,
    ) -> Result<u32> {
        precision_get(this.env(), tx, &usage).await
    }
}

/// The decimals of one field, as [`field_scale`] resolves them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FieldScale {
    /// Amounts are rounded half away from zero to this many decimals.
    Places(u32),
    /// A `numeric` column without rounding (`digits = 0`).
    Unrounded,
    /// A monetary field: the scale is the one of the currency in this field of the same record
    /// (`res.currency.rounding`, see [`crate::money::CurrencyRounding`]).
    Currency {
        /// The many2one field naming the currency.
        currency_field: String,
    },
}

/// The configured digits of `usage`, or `None` when no `decimal.precision` row names it.
///
/// One search with a limit of one, read as the system (see the module documentation).
///
/// # Errors
///
/// `Validation` for an empty or oversized usage name or a stored digits value outside `0..=28`;
/// database errors.
pub async fn precision_defined(
    env: &Env,
    tx: &mut Transaction,
    usage: &str,
) -> Result<Option<u32>> {
    if usage.is_empty() || usage.len() > MAX_USAGE_BYTES {
        return Err(Error::validation(
            "A decimal precision usage must be 1 to 128 bytes long.",
        ));
    }
    let model = env.sudo(true).records("decimal.precision")?;
    let domain = Domain::condition("name", Operator::Eq, usage)?;
    let opts = SearchOptions {
        limit: Some(1),
        ..SearchOptions::default()
    };
    let found = model.search_with(tx, &domain, &opts).await?;
    let rows = found.read_with(tx, &["digits"], Load::Raw).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    match row.get("digits") {
        Some(Value::Int(d)) if (0..=28).contains(d) => Ok(Some(*d as u32)),
        other => Err(Error::validation(format!(
            "Stored digits of decimal precision '{usage}' are invalid: {other:?}."
        ))),
    }
}

/// # Errors
///
/// As [`precision_defined`].
pub async fn precision_get(env: &Env, tx: &mut Transaction, usage: &str) -> Result<u32> {
    Ok(precision_defined(env, tx, usage)
        .await?
        .unwrap_or(DEFAULT_PRECISION_DIGITS))
}

/// The decimals of `model.field`: the per-model decimal lookup of hook H41.
///
/// A fixed `digits=(p, s)` gives `s`; a named precision is resolved through [`precision_get`];
/// `digits=0` is [`FieldScale::Unrounded`]; a monetary field is [`FieldScale::Currency`].
///
/// # Errors
///
/// `Programming` for an unknown model or field and for a field with no decimal notion (a float
/// declared without `digits`, which is a binary `f64`, or any non-numeric type); the errors of
/// [`precision_get`].
pub async fn field_scale(
    env: &Env,
    tx: &mut Transaction,
    model: &str,
    field: &str,
) -> Result<FieldScale> {
    let registry = tx.registry().clone();
    let spec = registry
        .model(model)
        .ok_or_else(|| Error::programming(format!("unknown model '{model}'")))?;
    let field_spec = spec
        .field(field)
        .ok_or_else(|| Error::programming(format!("unknown field '{model}.{field}'")))?;
    match &field_spec.ty {
        FieldType::Float {
            digits: Some(Digits::Fixed { scale, .. }),
        } => Ok(FieldScale::Places(*scale)),
        FieldType::Float {
            digits: Some(Digits::Unrounded),
        } => Ok(FieldScale::Unrounded),
        FieldType::Float {
            digits: Some(Digits::Named(usage)),
        } => Ok(FieldScale::Places(precision_get(env, tx, usage).await?)),
        FieldType::Monetary {
            currency_field: Some(currency_field),
        } => Ok(FieldScale::Currency {
            currency_field: currency_field.clone(),
        }),
        _ => Err(Error::programming(format!(
            "field '{model}.{field}' has no decimal precision: declare float digits or a monetary \
             currency field"
        ))),
    }
}
