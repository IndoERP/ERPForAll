//! Money and currency services (hook H41;-D3).
//!
//! Everything here is exact decimal arithmetic through [`erp_sdk::money`]; no `f64` is involved
//! anywhere on the path.
//!
//! # Rate lookup
//!
//! [`RateBook::load`] reads the rates of a set of currencies for one company on one date in
//! **one rate statement** (plus one that finds the root company and one that reads the rate type). The rule, which the
//! book records so a booking can store it:
//!
//! Rates also belong to a **rate type** (`res.currency.rate.type`): every lookup reads
//! the rows of one type only, [`RateBook::load`] those of `spot`, [`RateBook::load_typed`] those of
//! the named type under that type's own date rule. No lookup falls back to another type.
//!
//! 1. Rates live on the **root company** of the given company, or on no company (shared).
//! 3. The company's own currency (the reference of its rates) converts at exactly one when no row
//!    names it; a stored row for it, if any, is used like any other. ([`RateOrigin::ReferenceCurrency`].)
//!    This fallback belongs to the untyped call and to rate types that enable it (`spot` only,
//!    seeded); [`RateBook::load_typed`] on any other type reports the rate as missing instead.
//! 5. When any other currency has no rate at all, a conversion that needs it fails with a `UserError`
//!    naming the currency, company and date.
//!
//! Converting between a currency and itself needs no rate (the factor is exactly one).
//!
//! # Access
//!
//! The caller needs read access to `res.currency` and `res.currency.rate` and the company must be
//! one of the caller's allowed companies.
//!
//! # Rounding

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use erp_sdk::access::PrincipalKind;
use erp_sdk::money::{
    CurrencyCode, ExactRatio, FxSnapshot, Increment, RateDateRule, RateOrigin, RateRef,
    RoundingMode,
};
use erp_sdk::prelude::*;
use erp_sdk::{Load, Operation};

pub const CURRENCY_ROUNDING_MODE: RoundingMode = RoundingMode::HalfAwayFromZero;

/// The most currencies one [`RateBook`] or [`RoundingTable`] loads.
pub const MAX_CURRENCIES_PER_CALL: usize = 256;

/// The deepest company hierarchy the root lookup follows; a deeper or cyclic one is an error.
pub const MAX_COMPANY_DEPTH: i64 = 32;

const CHAIN_SQL: &str = "WITH RECURSIVE up(id, parent_id, depth) AS (\
    SELECT id, parent_id, 0 FROM res_company WHERE id = $1::int8 \
    UNION ALL \
    SELECT c.id, c.parent_id, up.depth + 1 FROM res_company c JOIN up ON c.id = up.parent_id \
    WHERE up.depth < $2::int8) \
    SELECT up.id, up.parent_id, c.currency_id FROM up JOIN res_company c ON c.id = up.id \
    ORDER BY up.depth";

// One statement for all currencies; `<` or `<=` is the only difference between the two rules.
macro_rules! rates_sql {
    ($cmp:literal) => {
        concat!(
            "SELECT c.id, cur.name, b.id, b.rate, b.name, a.id, a.rate, a.name \
             FROM unnest($1::int8[]) AS c(id) \
             JOIN res_currency cur ON cur.id = c.id \
             LEFT JOIN LATERAL (SELECT r.id, r.rate, r.name FROM res_currency_rate r \
               WHERE r.currency_id = c.id AND r.rate_type_id = $4::int8 AND r.name ",
            $cmp,
            " $2::date AND (r.company_id IS NULL OR r.company_id = $3::int8) \
               ORDER BY r.company_id ASC NULLS LAST, r.name DESC LIMIT 1) b ON TRUE \
             LEFT JOIN LATERAL (SELECT r.id, r.rate, r.name FROM res_currency_rate r \
               WHERE r.currency_id = c.id AND r.rate_type_id = $4::int8 \
               AND (r.company_id IS NULL OR r.company_id = $3::int8) \
               ORDER BY r.company_id ASC NULLS LAST, r.name ASC LIMIT 1) a ON TRUE"
        )
    };
}
const RATES_BEFORE_SQL: &str = rates_sql!("<");
const RATES_ON_OR_BEFORE_SQL: &str = rates_sql!("<=");

/// The code of currency `id` as stored; the error names both when it is not a valid code.
fn stored_code(id: i64, name: &str) -> Result<CurrencyCode> {
    CurrencyCode::new(name).map_err(|e| {
        Error::validation(format!("Currency {id} has an invalid code {name:?}: {e}")).with_source(e)
    })
}

fn db_error(what: &str, error: impl std::fmt::Display) -> Error {
    Error::db(format!("unexpected value in {what}: {error}"))
}

/// Checks that `company` is one of the caller's allowed companies.
///
/// Fails closed: only the system principal (install, data loading, tests, internal jobs) may name
/// no company and still use any; every other principal with an empty or foreign list is refused.
pub(crate) async fn authorize_company(tx: &mut Transaction, env: &Env, company: i64) -> Result<()> {
    let allowed = tx.companies(env).await?;
    let is_system = env.principal().kind() == PrincipalKind::System;
    if allowed.iter().any(|c| c.get() == company) || (allowed.is_empty() && is_system) {
        Ok(())
    } else {
        Err(Error::access(
            "One of the selected companies does not exist or is not one of yours.",
        ))
    }
}

pub(crate) async fn flush_named(tx: &mut Transaction, model: &str) -> Result<()> {
    let id = tx
        .registry()
        .model(model)
        .map(|m| m.id)
        .ok_or_else(|| Error::programming(format!("the registry has no {model} model")))?;
    tx.flush_model(id).await
}

/// A company and its ancestors, nearest first, ending at the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompanyChain {
    ids: Vec<i64>,
    root_id: i64,
    root_currency_id: i64,
}

impl CompanyChain {
    /// The company itself, then each ancestor up to and including the root.
    pub(crate) fn ids(&self) -> &[i64] {
        &self.ids
    }

    /// The root company id (the company itself when it has no parent).
    pub(crate) fn root_id(&self) -> i64 {
        self.root_id
    }

    /// The root company's currency: the reference of its rates.
    pub(crate) fn root_currency_id(&self) -> i64 {
        self.root_currency_id
    }
}

/// The chain from `company_id` up to its root, in one recursive statement bounded by
/// [`MAX_COMPANY_DEPTH`].
///
/// # Errors
///
/// `Missing` when the company does not exist; `Validation` when the hierarchy is deeper than the
/// bound or cyclic.
pub(crate) async fn company_chain(tx: &mut Transaction, company_id: i64) -> Result<CompanyChain> {
    flush_named(tx, "res.company").await?;
    let rows = tx
        .query(CHAIN_SQL, &[&company_id, &MAX_COMPANY_DEPTH])
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    let mut last: Option<(i64, Option<i64>, i64)> = None;
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(|e| db_error("res_company.id", e))?;
        let parent: Option<i64> = row
            .try_get(1)
            .map_err(|e| db_error("res_company.parent_id", e))?;
        let currency_id: i64 = row
            .try_get(2)
            .map_err(|e| db_error("res_company.currency_id", e))?;
        ids.push(id);
        last = Some((id, parent, currency_id));
    }
    let (root_id, parent, root_currency_id) =
        last.ok_or_else(|| Error::missing(format!("Company {company_id} does not exist.")))?;
    if parent.is_some() {
        return Err(Error::validation(format!(
            "The company hierarchy of company {company_id} is cyclic or deeper than \
             {MAX_COMPANY_DEPTH} levels."
        )));
    }
    Ok(CompanyChain {
        ids,
        root_id,
        root_currency_id,
    })
}

/// How one currency rounds: its code, increment and decimal places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrencyRounding {
    id: i64,
    code: CurrencyCode,
    increment: Increment,
    decimal_places: u32,
}

impl CurrencyRounding {
    /// The `res.currency` id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The currency code.
    pub fn code(&self) -> CurrencyCode {
        self.code
    }

    /// The rounding increment (`res.currency.rounding`, normalized).
    pub fn increment(&self) -> &Increment {
        &self.increment
    }

    /// Decimal places for display (`res.currency.decimal_places`).
    pub fn decimal_places(&self) -> u32 {
        self.decimal_places
    }

    /// # Errors
    ///
    /// `Validation` when the result is outside the decimal range.
    pub fn round(&self, amount: Decimal) -> Result<Decimal> {
        Ok(self.increment.quantize(amount, CURRENCY_ROUNDING_MODE)?)
    }

    /// `amount` rounded with an explicit mode.
    ///
    /// # Errors
    ///
    /// As [`CurrencyRounding::round`].
    pub fn round_with(&self, amount: Decimal, mode: RoundingMode) -> Result<Decimal> {
        Ok(self.increment.quantize(amount, mode)?)
    }

    /// Rounds the amount you
    /// pass: for two amounts use [`CurrencyRounding::compare_amounts`].
    ///
    /// # Errors
    ///
    /// As [`CurrencyRounding::round`].
    pub fn is_zero(&self, amount: Decimal) -> Result<bool> {
        Ok(self.increment.is_zero(amount, CURRENCY_ROUNDING_MODE)?)
    }

    /// # Errors
    ///
    /// As [`CurrencyRounding::round`].
    pub fn compare_amounts(&self, left: Decimal, right: Decimal) -> Result<Ordering> {
        Ok(self
            .increment
            .compare(left, right, CURRENCY_ROUNDING_MODE)?)
    }
}

/// The rounding of a set of currencies, loaded in one read.
#[derive(Debug, Clone, Default)]
pub struct RoundingTable {
    by_id: HashMap<i64, CurrencyRounding>,
}

impl RoundingTable {
    /// Loads the currencies `ids` (duplicates ignored).
    ///
    /// # Errors
    ///
    /// `Programming` for an empty or oversized set; `Access` without read access to
    /// `res.currency`; `Missing` naming the ids that do not exist; `Validation` for a stored
    /// rounding or code that is not valid.
    pub async fn load(env: &Env, tx: &mut Transaction, ids: &[RecId]) -> Result<Self> {
        let wanted = distinct_ids(ids)?;
        let records = env
            .records("res.currency")?
            .browse(wanted.iter().copied())?;
        let rows = records
            .read_with(tx, &["name", "rounding", "decimal_places"], Load::Raw)
            .await?;
        let mut by_id = HashMap::with_capacity(rows.len());
        for row in &rows {
            let id = match row.get("id") {
                Some(Value::Int(id)) => *id,
                other => {
                    return Err(Error::invariant(format!(
                        "currency read lacks an id: {other:?}"
                    )));
                }
            };
            let code = match row.get("name") {
                Some(Value::Str(name)) => stored_code(id, name)?,
                other => {
                    return Err(Error::validation(format!(
                        "Currency {id} has no valid code: {other:?}."
                    )));
                }
            };
            let increment = match row.get("rounding") {
                Some(Value::Decimal(rounding)) => Increment::new(*rounding)?,
                other => {
                    return Err(Error::validation(format!(
                        "Currency {code} has no valid rounding: {other:?}."
                    )));
                }
            };
            let decimal_places = match row.get("decimal_places") {
                Some(Value::Int(places)) if (0..=28).contains(places) => *places as u32,
                other => {
                    return Err(Error::validation(format!(
                        "Currency {code} has invalid decimal places: {other:?}."
                    )));
                }
            };
            by_id.insert(
                id,
                CurrencyRounding {
                    id,
                    code,
                    increment,
                    decimal_places,
                },
            );
        }
        let absent: Vec<i64> = wanted
            .iter()
            .copied()
            .filter(|id| !by_id.contains_key(id))
            .collect();
        if !absent.is_empty() {
            return Err(Error::missing(format!(
                "Currencies {absent:?} do not exist."
            )));
        }
        Ok(Self { by_id })
    }

    /// The rounding of currency `id`.
    ///
    /// # Errors
    ///
    /// `Programming` when `id` was not part of the loaded set.
    pub fn get(&self, id: RecId) -> Result<&CurrencyRounding> {
        self.by_id.get(&id.get()).ok_or_else(|| {
            Error::programming(format!(
                "currency {} was not loaded into this rounding table",
                id.get()
            ))
        })
    }
}

fn distinct_ids(ids: &[RecId]) -> Result<Vec<i64>> {
    let wanted: BTreeSet<i64> = ids.iter().map(|id| id.get()).collect();
    if wanted.is_empty() {
        return Err(Error::programming("at least one currency is required"));
    }
    if wanted.len() > MAX_CURRENCIES_PER_CALL {
        return Err(Error::validation(format!(
            "At most {MAX_CURRENCIES_PER_CALL} currencies can be loaded at once."
        )));
    }
    Ok(wanted.into_iter().collect())
}

/// A rate type as the lookup reads it: identity, date rule and whether the earliest later rate
/// may stand in before the first stored one (`res.currency.rate.type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateTypeRef {
    id: i64,
    code: String,
    rule: RateDateRule,
    later_fallback: bool,
}

impl RateTypeRef {
    /// The id of the `res.currency.rate.type` row.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The stable code (`spot`, `tax`, ...).
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The date rule the type's lookup follows.
    pub fn rule(&self) -> RateDateRule {
        self.rule
    }

    /// Whether a date before the first stored rate may use the earliest later one.
    pub fn later_fallback(&self) -> bool {
        self.later_fallback
    }

    /// Reads the type `code`. An archived type is still read: archiving only hides it from
    /// pickers, its rates stay usable.
    ///
    /// # Errors
    ///
    /// `Missing` when no type has that code (an unknown type never maps to another one);
    /// `Validation` when its stored date rule is not one this lookup knows.
    pub async fn by_code(tx: &mut Transaction, code: &str) -> Result<Self> {
        flush_named(tx, "res.currency.rate.type").await?;
        let rows = tx
            .query(
                "SELECT id, code, date_rule, later_fallback FROM res_currency_rate_type \
                 WHERE code = $1",
                &[&code],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing(format!(
                "The currency rate type {code:?} does not exist."
            )));
        };
        let id: i64 = row.try_get(0).map_err(|e| db_error("rate_type.id", e))?;
        let code: String = row.try_get(1).map_err(|e| db_error("rate_type.code", e))?;
        let rule: String = row
            .try_get(2)
            .map_err(|e| db_error("rate_type.date_rule", e))?;
        let later_fallback: bool = row
            .try_get(3)
            .map_err(|e| db_error("rate_type.later_fallback", e))?;
        let rule = match rule.as_str() {
            "strictly_before" => RateDateRule::StrictlyBefore,
            "on_or_before" => RateDateRule::OnOrBefore,
            other => {
                return Err(Error::validation(format!(
                    "The currency rate type {code:?} has an unknown date rule {other:?}."
                )));
            }
        };
        Ok(Self {
            id,
            code,
            rule,
            later_fallback,
        })
    }
}

/// The id of the rate type `code`, for defaults (`Missing` when it does not exist).
pub(crate) async fn rate_type_id_by_code(tx: &mut Transaction, code: &str) -> Result<i64> {
    Ok(RateTypeRef::by_code(tx, code).await?.id)
}

#[derive(Debug, Clone)]
struct BookEntry {
    code: CurrencyCode,
    rate: Option<RateRef>,
}

/// The rates of a set of currencies for one company on one date (see the module documentation
/// for the rule). Immutable once loaded; derive any pair's [`FxSnapshot`] without further I/O.
#[derive(Debug, Clone)]
pub struct RateBook {
    company_id: i64,
    date: Date,
    rule: RateDateRule,
    rate_type: RateTypeRef,
    typed: bool,
    entries: HashMap<i64, BookEntry>,
}

impl RateBook {
    /// Reads the rates of `currencies` for `company` on `date`.
    ///
    /// # Errors
    ///
    /// `Access` without read access to the two models or for a company the caller may not use;
    /// `Missing` for an unknown company or currency; `Programming` or `Validation` for an empty
    /// or oversized currency set; `Validation` for a cyclic company hierarchy. A currency
    /// without any rate loads fine: only [`RateBook::snapshot`] needs it.
    pub async fn load(
        env: &Env,
        tx: &mut Transaction,
        company: RecId,
        date: Date,
        rule: RateDateRule,
        currencies: &[RecId],
    ) -> Result<Self> {
        Self::load_with(
            env,
            tx,
            company,
            date,
            Some(rule),
            crate::SPOT_RATE_TYPE,
            currencies,
        )
        .await
    }

    /// Reads the rates of `currencies` of the rate type `rate_type_code` (the type metadata is read
    /// by plain SQL without access rules: it names no business data) for `company` on `date`,
    /// under that type's own date rule and fallback setting. Only rows of the type
    /// are read, root-company rows winning over shared ones inside it; no other type ever stands
    /// in. [`RateBook::load`] is this call for `spot` with an explicit rule.
    ///
    /// # Errors
    ///
    /// As [`RateBook::load`], and `Missing` for an unknown type (an archived type is still read).
    pub async fn load_typed(
        env: &Env,
        tx: &mut Transaction,
        company: RecId,
        date: Date,
        rate_type_code: &str,
        currencies: &[RecId],
    ) -> Result<Self> {
        Self::load_with(env, tx, company, date, None, rate_type_code, currencies).await
    }

    /// One body for both loads: `explicit_rule` is the untyped call's rule (and then the earliest
    /// later fallback stays on); `None` means the typed call, which takes rule and fallback from
    /// the type. The type is read after the access checks, so a caller without access learns
    /// nothing about which types exist.
    async fn load_with(
        env: &Env,
        tx: &mut Transaction,
        company: RecId,
        date: Date,
        explicit_rule: Option<RateDateRule>,
        rate_type_code: &str,
        currencies: &[RecId],
    ) -> Result<Self> {
        let wanted = distinct_ids(currencies)?;
        env.records("res.currency")?
            .check_access(tx, Operation::Read)
            .await?;
        env.records("res.currency.rate")?
            .check_access(tx, Operation::Read)
            .await?;
        authorize_company(tx, env, company.get()).await?;
        let chain = company_chain(tx, company.get()).await?;
        flush_named(tx, "res.currency").await?;
        flush_named(tx, "res.currency.rate").await?;
        let rate_type = RateTypeRef::by_code(tx, rate_type_code).await?;
        let typed = explicit_rule.is_none();
        let rule = explicit_rule.unwrap_or(rate_type.rule);
        let fallback = !typed || rate_type.later_fallback;
        let sql = match rule {
            RateDateRule::StrictlyBefore => RATES_BEFORE_SQL,
            RateDateRule::OnOrBefore => RATES_ON_OR_BEFORE_SQL,
            _ => {
                return Err(Error::programming(
                    "unsupported rate date rule: add its statement to money::RateBook",
                ));
            }
        };
        let rows = tx
            .query(sql, &[&wanted, &date, &chain.root_id(), &rate_type.id])
            .await?;
        let mut entries = HashMap::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.try_get(0).map_err(|e| db_error("res_currency.id", e))?;
            let name: String = row
                .try_get(1)
                .map_err(|e| db_error("res_currency.name", e))?;
            let code = stored_code(id, &name)?;
            let preceding = read_rate(row, 2, RateOrigin::Preceding)?;
            let rate = match preceding {
                Some(rate) => Some(rate),
                None if fallback => read_rate(row, 5, RateOrigin::LaterFallback)?,
                None => None,
            };
            // the reference currency of the company converts at one when no row names it
            let rate = match rate {
                None if id == chain.root_currency_id() => Some(RateRef::reference()),
                other => other,
            };
            entries.insert(id, BookEntry { code, rate });
        }
        let absent: Vec<i64> = wanted
            .iter()
            .copied()
            .filter(|id| !entries.contains_key(id))
            .collect();
        if !absent.is_empty() {
            return Err(Error::missing(format!(
                "Currencies {absent:?} do not exist."
            )));
        }
        Ok(Self {
            company_id: chain.root_id(),
            date,
            rule,
            rate_type,
            typed,
            entries,
        })
    }

    /// The root company whose rates were read.
    pub fn root_company_id(&self) -> i64 {
        self.company_id
    }

    /// The requested date.
    pub fn date(&self) -> Date {
        self.date
    }

    /// The rate type whose rows were read (`spot` for [`RateBook::load`]).
    pub fn rate_type(&self) -> &RateTypeRef {
        &self.rate_type
    }

    /// The conversion `from -> to`, with the rates it used.
    ///
    /// # Errors
    ///
    /// `Programming` for a currency that was not loaded; `UserError` when a needed currency has
    /// no rate at all (never a rate of one).
    pub fn snapshot(&self, from: RecId, to: RecId) -> Result<FxSnapshot> {
        let source = self.entry(from)?;
        if from == to {
            return Ok(FxSnapshot::identity(
                source.code,
                self.company_id,
                self.date,
                self.rule,
            ));
        }
        let target = self.entry(to)?;
        let from_rate = self.rate_of(source)?;
        let to_rate = self.rate_of(target)?;
        Ok(FxSnapshot::between(
            source.code,
            target.code,
            self.company_id,
            self.date,
            self.rule,
            from_rate,
            to_rate,
        )?)
    }

    fn entry(&self, id: RecId) -> Result<&BookEntry> {
        self.entries.get(&id.get()).ok_or_else(|| {
            Error::programming(format!(
                "currency {} was not loaded into this rate book",
                id.get()
            ))
        })
    }

    fn rate_of(&self, entry: &BookEntry) -> Result<RateRef> {
        entry.rate.ok_or_else(|| {
            let of_type = if self.typed {
                format!(" Rate type: {}.", self.rate_type.code)
            } else {
                String::new()
            };
            Error::user(format!(
                "No exchange rate is defined for currency {} for company {} on {}. Add a rate \
                 for this currency first.{of_type}",
                entry.code, self.company_id, self.date
            ))
        })
    }
}

fn read_rate(row: &erp_sdk::Row, first: usize, origin: RateOrigin) -> Result<Option<RateRef>> {
    let id: Option<i64> = row
        .try_get(first)
        .map_err(|e| db_error("res_currency_rate.id", e))?;
    let rate: Option<Decimal> = row
        .try_get(first + 1)
        .map_err(|e| db_error("res_currency_rate.rate", e))?;
    let day: Option<Date> = row
        .try_get(first + 2)
        .map_err(|e| db_error("res_currency_rate.name", e))?;
    match (id, rate, day) {
        (None, None, None) => Ok(None),
        (Some(id), Some(rate), Some(day)) => Ok(Some(RateRef::new(rate, id, day, origin)?)),
        _ => Err(Error::invariant(
            "a res_currency_rate row came back with some of id, rate and name null",
        )),
    }
}

/// A converted amount with the evidence of how it was computed.
#[derive(Debug, Clone)]
pub struct Conversion {
    amount: Decimal,
    exact: ExactRatio,
    snapshot: FxSnapshot,
}

impl Conversion {
    /// The amount rounded to the target currency.
    pub fn amount(&self) -> Decimal {
        self.amount
    }

    /// The exact amount before rounding (a quotient such as `1/3` stays a quotient).
    pub fn exact(&self) -> &ExactRatio {
        &self.exact
    }

    /// The rates and direction that were applied; store this with a booking.
    pub fn snapshot(&self) -> &FxSnapshot {
        &self.snapshot
    }
}

/// # Errors
///
/// As [`RateBook::load`], [`RoundingTable::load`] and [`RateBook::snapshot`].
pub async fn convert(
    env: &Env,
    tx: &mut Transaction,
    amount: Decimal,
    from: RecId,
    to: RecId,
    company: RecId,
    date: Date,
) -> Result<Decimal> {
    Ok(convert_with(
        env,
        tx,
        amount,
        from,
        to,
        company,
        date,
        RateDateRule::StrictlyBefore,
        CURRENCY_ROUNDING_MODE,
    )
    .await?
    .amount())
}

/// [`convert`] with an explicit date rule and rounding mode, returning the evidence.
///
/// # Errors
///
/// As [`convert`].
#[allow(
    clippy::too_many_arguments,
    reason = "the explicit rule and mode are the point"
)]
pub async fn convert_with(
    env: &Env,
    tx: &mut Transaction,
    amount: Decimal,
    from: RecId,
    to: RecId,
    company: RecId,
    date: Date,
    rule: RateDateRule,
    mode: RoundingMode,
) -> Result<Conversion> {
    let book = RateBook::load(env, tx, company, date, rule, &[from, to]).await?;
    let snapshot = book.snapshot(from, to)?;
    let table = RoundingTable::load(env, tx, &[to]).await?;
    let exact = snapshot.convert_exact(amount)?;
    let rounded = exact.quantize(table.get(to)?.increment(), mode)?;
    Ok(Conversion {
        amount: rounded,
        exact,
        snapshot,
    })
}
