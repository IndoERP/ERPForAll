//! Persistent units and model services. PostgreSQL owns the hierarchy; conversions load caller-visible state.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use erp_sdk::ErrorKind;
use erp_sdk::Operation;
use erp_sdk::money::{ExactRatio, Increment, MoneyError, RoundingMode, parse_decimal};
use erp_sdk::prelude::*;

use crate::conversion::{
    Unit, exact_decimal, packaging_quantity, precision_compare, precision_is_zero, precision_round,
    price_exact, quantity_exact, quantity_rounded, rpc_exact,
};
use crate::error::UomError;

/// Maximum accepted number of units in one ancestry path.
pub const MAX_HIERARCHY_DEPTH: usize = 256;
/// Maximum bytes accepted for an ORM-maintained ancestry path.
pub const MAX_HIERARCHY_PATH_BYTES: usize = 8192;

/// A unit in a dimensional tree, with an exact decimal ratio to its direct parent.
#[model(
    name = "uom.uom",
    description = "Unit of Measure",
    parent_store = true,
    parent_name = "relative_uom_id",
    order = "sequence, name",
    constraints = [(
        "_factor_gt_zero",
        "CHECK (relative_factor != 0)",
        "A unit of measure needs a conversion ratio other than 0."
    )],
)]
pub struct Uom {
    /// Translated, required unit label.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Display order derived from the relative ratio unless explicitly supplied.
    #[field(
        integer,
        compute = "_compute_sequence",
        store,
        precompute,
        readonly = false
    )]
    pub sequence: i32,
    /// Nonzero exact ratio to the direct parent; a root has ratio one.
    #[field(float, digits = 0, default = 1, required = true)]
    pub relative_factor: Decimal,
    /// Archived units remain available to explicit historical references.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Direct reference unit; the addon checks the complete cascading deletion closure.
    #[field(many2one, comodel = "uom.uom", ondelete = "cascade", index = true)]
    pub relative_uom_id: Option<RecId>,
    /// Direct child units.
    #[field(one2many, comodel = "uom.uom", inverse_name = "relative_uom_id")]
    pub related_uom_ids: Vec<RecId>,
    /// Exact absolute ratio to the root. A nonrepresentable product rejects the mutation.
    #[field(float, digits = 0, compute = "_compute_factor", store)]
    pub factor: Decimal,
    /// Slash-delimited ancestry, maintained by the framework parent store.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// Context-sensitive name, including the direct ratio when requested.
    #[field(char, compute = "_compute_uom_display_name")]
    pub display_name: Option<Arc<str>>,
}

fn factor_assignment_error() -> Error {
    Error::validation(
        "Field uom.uom.factor is maintained by the unit hierarchy and cannot be assigned.",
    )
}

fn factor_error() -> Error {
    Error::validation("Unit of measure factor cannot be represented exactly as a decimal.")
}

fn persisted_factor(value: &ExactRatio) -> Result<Decimal> {
    exact_decimal(value)
        .map_err(|error| factor_error().with_source(error))?
        .ok_or_else(|| {
            factor_error().with_source(UomError::Numeric(MoneyError::NumericLimit {
                operation: "uom persisted factor",
            }))
        })
}

fn numeric(error: erp_sdk::money::MoneyError) -> Error {
    UomError::from(error).into()
}

fn invalid_path(id: i64) -> Error {
    Error::validation(format!(
        "Unit of measure {id} has an invalid hierarchy path."
    ))
}

fn path_ids(id: i64, path: &str) -> Result<Vec<i64>> {
    if path.len() > MAX_HIERARCHY_PATH_BYTES {
        return Err(Error::validation(
            "Unit of measure hierarchy path exceeds 8192 bytes.",
        ));
    }
    let body = path.strip_suffix('/').ok_or_else(|| invalid_path(id))?;
    if body.split('/').count() > MAX_HIERARCHY_DEPTH {
        return Err(Error::validation(
            "Unit of measure hierarchy exceeds 256 levels.",
        ));
    }
    let mut ids = Vec::new();
    let mut seen = BTreeSet::new();
    for part in body.split('/') {
        if ids.len() == MAX_HIERARCHY_DEPTH {
            return Err(Error::validation(
                "Unit of measure hierarchy exceeds 256 levels.",
            ));
        }
        let next = part.parse::<i64>().map_err(|_| invalid_path(id))?;
        if next <= 0 || !seen.insert(next) {
            return Err(invalid_path(id));
        }
        ids.push(next);
    }
    if ids.last().copied() != Some(id) {
        return Err(invalid_path(id));
    }
    Ok(ids)
}

fn sequence_value(relative: Decimal) -> Result<i32> {
    let invalid = || Error::validation("Unit of measure sequence is outside the integer range.");
    let scaled = relative
        .checked_mul(Decimal::from(100))
        .ok_or_else(invalid)?;
    // Range-check the exact scaled value before either truncation or the display cap.
    // Otherwise values just below i32::MIN or huge positive values could be admitted.
    if scaled < Decimal::from(i32::MIN) || scaled > Decimal::from(i32::MAX) {
        return Err(invalid());
    }
    let integer = scaled
        .trunc()
        .to_string()
        .parse::<i32>()
        .map_err(|_| invalid())?;
    Ok(integer.min(1000))
}

fn singleton_id(this: &RecordSet<Uom>) -> Result<i64> {
    this.ensure_one()?;
    this.ids()
        .first()
        .copied()
        .ok_or_else(|| Error::programming("Expected one unit of measure."))
}

/// The decimals of the "Product Unit" precision, as configured now.
pub(crate) async fn product_unit_digits(env: &Env, tx: &mut Transaction) -> Result<u32> {
    erp_addon_base::decimal_precision::precision_get(env, tx, "Product Unit").await
}

async fn precision(env: &Env, tx: &mut Transaction) -> Result<Increment> {
    let digits = product_unit_digits(env, tx).await?;
    Increment::from_places(digits).map_err(numeric)
}

fn rounding_mode(name: &str) -> Result<RoundingMode> {
    if name.len() > 9 {
        return Err(Error::validation(
            "Unit of measure rounding method exceeds 9 bytes.",
        ));
    }
    match name {
        "HALF-UP" => Ok(RoundingMode::HalfAwayFromZero),
        "HALF-DOWN" => Ok(RoundingMode::HalfTowardZero),
        "HALF-EVEN" => Ok(RoundingMode::HalfEven),
        "UP" => Ok(RoundingMode::AwayFromZero),
        "DOWN" => Ok(RoundingMode::TowardZero),
        "CEILING" => Ok(RoundingMode::Ceiling),
        "FLOOR" => Ok(RoundingMode::Floor),
        other => Err(Error::validation(format!(
            "Unsupported unit of measure rounding method '{other}'."
        ))),
    }
}

#[methods(Uom, addon = "uom")]
impl Uom {
    /// Compute exact absolute ratios using recursively prefetched parent factors.
    #[compute(depends = ["relative_factor", "relative_uom_id.factor", "parent_path"])]
    fn _compute_factor(batch: &mut Batch<'_, Uom>) -> Result<()> {
        for index in batch.indices() {
            let id = batch.ids()[index].get();
            // Virtual form rows do not yet have a database-managed parent_path. Persisted
            // rows must have one; parent-store create publishes it before stored computes.
            if id > 0 {
                let path = batch.get(index, Uom::PARENT_PATH)?;
                let path = path.as_deref().ok_or_else(|| invalid_path(id))?;
                path_ids(id, path)?;
            }
            let relative = batch.get(index, Uom::RELATIVE_FACTOR)?;
            let mut exact = ExactRatio::from_decimal(relative).map_err(numeric)?;
            if let Some(parent) = batch.get(index, Uom::RELATIVE_UOM_ID)? {
                let parent_factor = batch.value_of(Uom::FACTOR, parent)?;
                if parent_factor.is_zero() {
                    return Err(Error::validation(format!(
                        "The conversion factor for unit of measure {} cannot be 0.",
                        parent.get()
                    )));
                }
                exact = exact
                    .checked_mul(&ExactRatio::from_decimal(parent_factor).map_err(numeric)?)
                    .map_err(numeric)?;
            }
            batch.set(index, Uom::FACTOR, persisted_factor(&exact)?)?;
        }
        Ok(())
    }

    /// Compute an integer display sequence with checked negative range conversion.
    #[compute(depends = ["relative_factor"])]
    fn _compute_sequence(batch: &mut Batch<'_, Uom>) -> Result<()> {
        for index in batch.indices() {
            batch.set(
                index,
                Uom::SEQUENCE,
                sequence_value(batch.get(index, Uom::RELATIVE_FACTOR)?)?,
            )?;
        }
        Ok(())
    }

    /// Add the direct reference ratio only in the explicitly requested display context.
    #[compute(depends = ["name", "relative_factor", "relative_uom_id.name"], depends_context = ["formatted_display_name"])]
    fn _compute_uom_display_name(batch: &mut Batch<'_, Uom>) -> Result<()> {
        // The specification uses context truthiness for this presentation option (§4.4).
        let formatted = batch
            .env()
            .context()
            .get("formatted_display_name")
            .is_some_and(|value| value.is_truthy());
        for index in batch.indices() {
            let name = batch.get(index, Uom::NAME)?;
            let name =
                name.ok_or_else(|| Error::validation("A unit of measure must have a name."))?;
            let value = if formatted && let Some(parent) = batch.get(index, Uom::RELATIVE_UOM_ID)? {
                let parent_name = batch.value_of(Uom::NAME, parent)?.ok_or_else(|| {
                    Error::validation("A reference unit of measure must have a name.")
                })?;
                let relative = batch.get(index, Uom::RELATIVE_FACTOR)?;
                let mut ratio = relative.normalize().to_string();
                if !ratio.contains('.') {
                    ratio.push_str(".0");
                }
                format!("{name}\t--{ratio} {parent_name}--")
            } else {
                name.to_string()
            };
            batch.set(index, Uom::DISPLAY_NAME, value.as_str())?;
        }
        Ok(())
    }

    /// A root is the reference unit, so its relative ratio is exactly one.
    #[constrains("relative_factor", "relative_uom_id")]
    async fn _check_factor(this: &RecordSet<Uom>, tx: &mut Transaction) -> Result<()> {
        for record in this.iter() {
            if record.get(tx, Uom::RELATIVE_UOM_ID).await?.is_none()
                && record.get(tx, Uom::RELATIVE_FACTOR).await? != Decimal::ONE
            {
                return Err(Error::user("Reference unit of measure is missing."));
            }
        }
        Ok(())
    }

    /// Publish all stored descendants inside an operation-owned savepoint.
    /// A compute failure restores the whole create, including inserted rows and paths.
    #[method(override)]
    async fn create(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        // An empty create is a zero-query no-op, including no savepoint SQL.
        if vals_list.is_empty() {
            return sup.call(tx, this.as_records(), (vals_list,)).await;
        }
        // Inspect the whole batch before super inserts any row. Factor is an output of
        // the hierarchy compute, even when a supplied value happens to match that output.
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        // a `default_factor` context key would be filled in by the base create after this check
        if vals_list.iter().any(|vals| vals.contains("factor"))
            || this.env().context().contains("default_factor")
        {
            return Err(factor_assignment_error());
        }
        if vals_list.iter().any(|vals| vals.contains("parent_path")) {
            return Err(Error::validation(
                "Field uom.uom.parent_path is maintained by the unit hierarchy and cannot be assigned.",
            ));
        }
        if vals_list.len() > crate::references::MAX_GUARD_IDENTITIES {
            return Err(Error::validation(
                "Unit reference guard input exceeds its limits or has invalid identities.",
            ));
        }
        crate::protection::acquire_fence(tx).await?;
        tx.savepoint(async |tx| sup.call(tx, this.as_records(), (vals_list,)).await)
            .await
            .map_err(crate::references::map_guard_error)
    }

    /// Reject an ancestor edit atomically if any affected descendant cannot store its exact factor.
    #[method(override)]
    async fn write(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        // Delegate the empty no-op before opening a savepoint: even savepoint SQL
        // would violate's zero-query contract.
        if this.is_empty() {
            return sup.call(tx, this.as_records(), (vals,)).await;
        }
        this.as_records().check_access(tx, Operation::Write).await?;
        if vals.contains("factor") {
            return Err(factor_assignment_error());
        }
        crate::protection::before_write(this, tx, &vals).await?;
        tx.savepoint(async |tx| sup.call(tx, this.as_records(), (vals,)).await)
            .await
            .map_err(crate::references::map_guard_error)
    }

    /// Native exact quantity conversion; rounding is explicit at the RPC or caller boundary.
    ///
    /// # Errors
    /// Caller read failures, malformed hierarchies, incompatible roots and numeric limits.
    #[method]
    async fn _compute_quantity(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        quantity: Decimal,
        to_unit: i64,
    ) -> Result<ExactRatio> {
        let source = load_unit(this.env(), tx, singleton_id(this)?).await?;
        let target = load_unit(this.env(), tx, to_unit).await?;
        quantity_exact(&source, &target, quantity).map_err(Into::into)
    }

    /// Native exact inverse unit-price conversion.
    ///
    /// # Errors
    /// As `_compute_quantity`; roots are validated even for a zero price.
    #[method]
    async fn _compute_price(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        price: Decimal,
        to_unit: i64,
    ) -> Result<ExactRatio> {
        let source = load_unit(this.env(), tx, singleton_id(this)?).await?;
        let target = load_unit(this.env(), tx, to_unit).await?;
        price_exact(&source, &target, price).map_err(Into::into)
    }

    /// Whether two valid, caller-readable units have a common reference root.
    #[method]
    async fn _has_common_reference(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        other: i64,
    ) -> Result<bool> {
        let source = load_unit(this.env(), tx, singleton_id(this)?).await?;
        let target = load_unit(this.env(), tx, other).await?;
        Ok(source.root() == target.root())
    }

    /// Round a quantity to the requested packaging multiple using Product Unit precision.
    #[method]
    async fn _check_qty(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        quantity: Decimal,
        uom_id: i64,
        rounding_method: String,
    ) -> Result<Decimal> {
        let source = load_unit(this.env(), tx, singleton_id(this)?).await?;
        let target = load_unit(this.env(), tx, uom_id).await?;
        let precision = precision(this.env(), tx).await?;
        packaging_quantity(
            &source,
            &target,
            quantity,
            &precision,
            rounding_mode(&rounding_method)?,
        )
        .map_err(Into::into)
    }

    /// HALF-UP rounding at the current Product Unit precision.
    #[method]
    async fn round(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        quantity: Decimal,
    ) -> Result<Decimal> {
        let precision = precision(this.env(), tx).await?;
        precision_round(quantity, &precision, RoundingMode::HalfAwayFromZero).map_err(Into::into)
    }

    /// Round with a named mode at the current Product Unit precision.
    ///
    /// # Errors
    /// Invalid mode names, precision lookup failures and bounded numeric failures.
    #[method]
    async fn round_with(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        quantity: Decimal,
        rounding_method: String,
    ) -> Result<Decimal> {
        let mode = rounding_mode(&rounding_method)?;
        let precision = precision(this.env(), tx).await?;
        precision_round(quantity, &precision, mode).map_err(Into::into)
    }

    /// Compare separately rounded operands at Product Unit precision.
    #[method]
    async fn compare(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        left: Decimal,
        right: Decimal,
    ) -> Result<i32> {
        let precision = precision(this.env(), tx).await?;
        Ok(match precision_compare(left, right, &precision)? {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        })
    }

    /// Whether HALF-UP rounding at Product Unit precision produces zero.
    #[method]
    async fn is_zero(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        quantity: Decimal,
    ) -> Result<bool> {
        let precision = precision(this.env(), tx).await?;
        precision_is_zero(quantity, &precision).map_err(Into::into)
    }

    /// Convert a strict decimal-string quantity for RPC, rounded UP by default.
    ///
    /// An exact request returns a Decimal when representable and otherwise numerator/denominator
    /// strings. Rounded responses name their rounding and precision. Input uses the shared strict
    /// money decimal grammar (at most 64 bytes, no exponent or binary-float transport).
    ///
    /// # Errors
    /// Invalid decimal text or rounding names, access failures and the native conversion errors.
    #[method]
    #[rpc(effect = read)]
    async fn convert(
        this: &RecordSet<Uom>,
        tx: &mut Transaction,
        quantity: String,
        to_unit: i64,
        #[arg(default = false)] exact: bool,
        #[arg(default = String::from("UP"))] rounding_method: String,
    ) -> Result<Value> {
        let quantity = parse_decimal(&quantity).map_err(numeric)?;
        let mode = rounding_mode(&rounding_method)?;
        let source = load_unit(this.env(), tx, singleton_id(this)?).await?;
        let target = load_unit(this.env(), tx, to_unit).await?;
        if exact {
            return rpc_exact(&quantity_exact(&source, &target, quantity)?).map_err(Into::into);
        }
        let precision = precision(this.env(), tx).await?;
        let value = quantity_rounded(&source, &target, quantity, &precision, mode)?;
        Ok(Value::Map(Arc::new(
            [
                ("value".to_owned(), Value::Decimal(value)),
                ("rounding".to_owned(), Value::str(rounding_method)),
                (
                    "digits".to_owned(),
                    Value::Int(i64::from(precision.scale())),
                ),
            ]
            .into_iter()
            .collect(),
        )))
    }
}

/// Resolve and validate one unit and its bounded ancestry using caller read access.
///
/// Every ancestor's original relative ratio participates in the exact product. The stored absolute
/// factor must match that product; missing rows and malformed paths never become implicit roots.
/// State is transaction-owned, with no process-global unit cache.
///
/// # Errors
/// `Missing` for an absent unit, caller access errors, `Validation` for malformed paths,
/// inconsistent parents, invalid roots and nonrepresentable factors; numeric primitive errors.
pub async fn load_unit(env: &Env, tx: &mut Transaction, id: i64) -> Result<Unit> {
    if id <= 0 {
        return Err(UomError::InvalidUnitId { id }.into());
    }
    let units = env.records("uom.uom")?;
    let record = units.browse([id])?;
    if record.exists(tx).await?.is_empty() {
        return Err(Error::new(
            ErrorKind::Missing,
            format!("Unit of measure {id} does not exist."),
        ));
    }
    let path = record.cell(tx, "parent_path").await?;
    let Cell::Str(path) = path else {
        return Err(invalid_path(id));
    };
    let ids = path_ids(id, &path)?;
    let ancestors = units.browse(ids.iter().copied())?;
    let parents = ancestors.cells(tx, "relative_uom_id").await?;
    let relatives = ancestors.cells(tx, "relative_factor").await?;
    let factors = ancestors.cells(tx, "factor").await?;
    let mut product = ExactRatio::from_i64(1);
    let mut prior = None;
    for (index, ancestor_id) in ids.iter().copied().enumerate() {
        let parent = match &parents[index] {
            Cell::Null => None,
            Cell::Id(parent) => Some(*parent),
            _ => return Err(invalid_path(id)),
        };
        if parent != prior {
            return Err(invalid_path(id));
        }
        let Cell::Decimal(relative) = relatives[index] else {
            return Err(invalid_path(id));
        };
        if prior.is_none() && relative != Decimal::ONE {
            return Err(Error::user("Reference unit of measure is missing."));
        }
        if relative.is_zero() {
            return Err(UomError::ZeroFactor { id: ancestor_id }.into());
        }
        product = product
            .checked_mul(&ExactRatio::from_decimal(relative).map_err(numeric)?)
            .map_err(numeric)?;
        let exact = persisted_factor(&product)?;
        if factors[index] != Cell::Decimal(exact) {
            return Err(Error::validation(format!(
                "Unit of measure {ancestor_id} has an inconsistent stored factor."
            )));
        }
        prior = Some(ancestor_id);
    }
    let root = ids.first().copied().ok_or_else(|| invalid_path(id))?;
    Unit::new(id, root, persisted_factor(&product)?).map_err(Into::into)
}
