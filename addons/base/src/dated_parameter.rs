//! `ir.dated.parameter`: statutory and policy values that change on a known date (hook H35,
//!
//! A row says "parameter `key`, for this company (or for every company), has this value from
//! `date_from` to `date_to`" (`date_to` empty: until replaced). Rates, thresholds and limits that a
//! rule or a report reads live here instead of in code, so a rule evaluated for a past date sees
//! the value that was in force then, and a later change never rewrites a past evaluation.
//!
//! # Storage rules
//!
//! - Periods of one key in one scope (one company, or the shared scope) never overlap: two typed
//!   exclusion constraints enforce it in the database, so concurrent writers cannot both commit
//!   . Both ends are inclusive (`bounds = "[]"`); an empty `date_to` is unbounded.
//! - A key is `lower_case_words` joined by dots, at most 128 bytes (`CHECK` and
//!   [`validate_key`]); a text value is at most 1024 bytes.
//! - The value is stored as text next to its kind and parsed strictly on write and on read:
//!   a decimal keeps its exact digits (no float is ever involved), an integer is an `i64`, a
//!   boolean is `true` or `false`.
//!
//! # Reading
//!
//! [`resolve`] and [`resolve_many`] answer for one company and date, nearest scope first: the
//! company's own row when one is in force, else the row of its parent, and so on up to the root
//! company of its hierarchy, else the shared row, else a `UserError` naming the key, company and
//! date (never a default). A branch therefore inherits what its root states and may override it.
//! The hierarchy walk is bounded ([`crate::money::MAX_COMPANY_DEPTH`]); a cycle is an error.
//!
//! The order of checks is the one of `RateBook::load`: the caller's model-level read right, then
//! the company (it must be one of the caller's allowed companies), then a read as the system
//! limited to those keys, that chain of companies and the shared scope. A record rule that filters
//! rows can thus never substitute a farther value for the nearer one. A [`ParamSnapshot`]
//! is a bounded, immutable, typed map for the evaluator of a rule or a report: bind its
//! [`ParamSnapshot::entries`] as variables. The reader reads at most `keys * (chain + 1) + 1` rows
//! in one search; a larger answer means the overlap constraint was bypassed and is an error.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::money::parse_decimal;
use erp_sdk::prelude::*;
use erp_sdk::{Load, Operation, Operator, SearchOptions};

use crate::money::{authorize_company, company_chain};

/// The most keys one [`resolve_many`] call reads.
pub const MAX_KEYS_PER_CALL: usize = 256;
/// The longest key, in bytes.
pub const MAX_KEY_BYTES: usize = 128;
/// The longest stored value, in bytes.
pub const MAX_VALUE_BYTES: usize = 1024;

/// `ir.dated.parameter`.
#[model(
    name = "ir.dated.parameter",
    description = "Dated Parameter",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "allow"),
    order = "key, date_from desc, id",
    constraints = [
        (
            "_key_format",
            "CHECK (key ~ '^[a-z0-9_]+([.][a-z0-9_]+)*$' AND char_length(key) <= 128)",
            "A parameter key is lower-case words joined by dots, at most 128 characters."
        ),
        (
            "_value_size",
            "CHECK (char_length(value) <= 1024)",
            "A parameter value is at most 1024 characters."
        ),
        (
            "_period_order",
            "CHECK (date_to IS NULL OR date_to >= date_from)",
            "The end of a parameter period cannot be before its start."
        ),
    ],
    exclusions = [
        exclude(
            name = "_no_overlap_company",
            equal = ["key", "company_id"],
            period = ("date_from", "date_to"),
            bounds = "[]",
            filter = "[('company_id', '!=', False)]",
            message = "This parameter already has a value for an overlapping period of this company."
        ),
        exclude(
            name = "_no_overlap_shared",
            equal = ["key"],
            period = ("date_from", "date_to"),
            bounds = "[]",
            filter = "[('company_id', '=', False)]",
            message = "This parameter already has a shared value for an overlapping period."
        ),
    ],
)]
pub struct IrDatedParameter {
    /// Parameter key, e.g. `payroll.pension_rate`.
    #[field(char, required = true, index = true)]
    pub key: Option<Arc<str>>,
    /// Company the value applies to; empty for every company.
    #[field(many2one, comodel = "res.company", ondelete = "cascade")]
    pub company_id: Option<RecId>,
    /// First day the value is in force.
    #[field(date, required = true)]
    pub date_from: Option<Date>,
    /// Last day the value is in force (inclusive); empty until replaced.
    #[field(date)]
    pub date_to: Option<Date>,
    /// How `value` is read.
    #[field(
        selection,
        selection = [
            ("decimal", "Decimal"),
            ("integer", "Integer"),
            ("boolean", "Boolean"),
            ("text", "Text")
        ],
        default = "decimal",
        required = true
    )]
    pub value_kind: Option<Arc<str>>,
    /// The value in the canonical text of its kind.
    #[field(char, required = true)]
    pub value: Option<Arc<str>>,
}

#[methods(IrDatedParameter, addon = "base")]
impl IrDatedParameter {
    /// A value must parse as its kind (a decimal keeps its digits exactly).
    #[constrains("value_kind", "value")]
    async fn _check_value(this: &RecordSet<IrDatedParameter>, tx: &mut Transaction) -> Result<()> {
        let records = this.as_records();
        let kinds = records.values(tx, "value_kind").await?;
        let values = records.values(tx, "value").await?;
        let keys = records.values(tx, "key").await?;
        for ((kind, value), key) in kinds.iter().zip(&values).zip(&keys) {
            // key, kind and value are required columns: anything but text here is a corrupt row
            let (Value::Str(key), Value::Str(kind), Value::Str(value)) = (key, kind, value) else {
                return Err(Error::validation(
                    "A dated parameter needs a key, a kind and a value.",
                ));
            };
            ParamValue::parse(ParamKind::parse(kind)?, value)
                .map_err(|e| Error::validation(format!("Parameter '{key}': {}", e.message())))?;
        }
        Ok(())
    }
}

/// How a parameter's text is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParamKind {
    /// An exact decimal (strict text, no exponent).
    Decimal,
    /// A signed 64-bit integer.
    Integer,
    /// `true` or `false`.
    Boolean,
    /// Any text.
    Text,
}

impl ParamKind {
    /// The selection value stored in `value_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decimal => "decimal",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Text => "text",
        }
    }

    /// Reads a stored selection value.
    ///
    /// # Errors
    ///
    /// `Validation` for an unknown kind.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "decimal" => Ok(Self::Decimal),
            "integer" => Ok(Self::Integer),
            "boolean" => Ok(Self::Boolean),
            "text" => Ok(Self::Text),
            other => Err(Error::validation(format!(
                "Unknown parameter kind '{other}'."
            ))),
        }
    }
}

/// A typed parameter value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParamValue {
    /// An exact decimal.
    Decimal(Decimal),
    /// An integer.
    Integer(i64),
    /// A boolean.
    Boolean(bool),
    /// Text.
    Text(Arc<str>),
}

impl ParamValue {
    /// Parses the stored `text` as `kind`, strictly.
    ///
    /// # Errors
    ///
    /// `Validation` when the text is not a valid value of the kind or is longer than
    /// [`MAX_VALUE_BYTES`].
    pub fn parse(kind: ParamKind, text: &str) -> Result<Self> {
        if text.len() > MAX_VALUE_BYTES {
            return Err(Error::validation(
                "A parameter value is at most 1024 characters.",
            ));
        }
        match kind {
            ParamKind::Decimal => Ok(Self::Decimal(parse_decimal(text)?)),
            ParamKind::Integer => {
                let parsed = text.parse::<i64>().map_err(|_| {
                    Error::validation("Invalid integer: expected digits with an optional '-'.")
                })?;
                if parsed.to_string() != text {
                    return Err(Error::validation(
                        "Invalid integer: write it without '+' or leading zeros.",
                    ));
                }
                Ok(Self::Integer(parsed))
            }
            ParamKind::Boolean => match text {
                "true" => Ok(Self::Boolean(true)),
                "false" => Ok(Self::Boolean(false)),
                _ => Err(Error::validation(
                    "Invalid boolean: expected 'true' or 'false'.",
                )),
            },
            ParamKind::Text => Ok(Self::Text(Arc::from(text))),
        }
    }

    /// The kind of the value.
    pub fn kind(&self) -> ParamKind {
        match self {
            Self::Decimal(_) => ParamKind::Decimal,
            Self::Integer(_) => ParamKind::Integer,
            Self::Boolean(_) => ParamKind::Boolean,
            Self::Text(_) => ParamKind::Text,
        }
    }
}

/// Checks a parameter key against the storage rule.
///
/// # Errors
///
/// `Validation` when the key is empty, longer than [`MAX_KEY_BYTES`], or not lower-case words
/// (`a-z`, `0-9`, `_`) joined by single dots.
pub fn validate_key(key: &str) -> Result<()> {
    let words_ok = !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key.split('.').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        });
    if words_ok {
        Ok(())
    } else {
        Err(Error::validation(format!(
            "Invalid parameter key '{}': use lower-case words joined by dots, at most {MAX_KEY_BYTES} bytes.",
            key.chars().take(64).collect::<String>()
        )))
    }
}

/// One parameter value in force for a company on a date, with the row it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedParam {
    /// The key asked for.
    pub key: Arc<str>,
    /// The typed value.
    pub value: ParamValue,
    /// The `ir.dated.parameter` row.
    pub record_id: i64,
    /// The company of the row; `None` for a shared row.
    pub company_id: Option<i64>,
    /// First day of the row's period.
    pub date_from: Date,
    /// Last day of the row's period, if bounded.
    pub date_to: Option<Date>,
}

/// The parameters in force for one company on one date, typed and immutable.
#[derive(Debug, Clone)]
pub struct ParamSnapshot {
    company_id: i64,
    date: Date,
    entries: BTreeMap<Arc<str>, ResolvedParam>,
}

impl ParamSnapshot {
    /// The company the snapshot is for.
    pub fn company_id(&self) -> i64 {
        self.company_id
    }

    /// The date the snapshot is for.
    pub fn date(&self) -> Date {
        self.date
    }

    /// The resolved parameter `key`.
    ///
    /// # Errors
    ///
    /// `UserError` naming the key, company and date when no row is in force (or the key was not
    /// requested when the snapshot was built).
    pub fn get(&self, key: &str) -> Result<&ResolvedParam> {
        self.entries.get(key).ok_or_else(|| {
            Error::user(format!(
                "No value of parameter '{key}' is in force for company {} on {}.",
                self.company_id, self.date
            ))
        })
    }

    /// `key` as an exact decimal.
    ///
    /// # Errors
    ///
    /// As [`ParamSnapshot::get`]; `Validation` when the parameter is not a decimal.
    pub fn decimal(&self, key: &str) -> Result<Decimal> {
        match &self.get(key)?.value {
            ParamValue::Decimal(d) => Ok(*d),
            other => Err(type_error(key, ParamKind::Decimal, other)),
        }
    }

    /// `key` as an integer.
    ///
    /// # Errors
    ///
    /// As [`ParamSnapshot::get`]; `Validation` when the parameter is not an integer.
    pub fn integer(&self, key: &str) -> Result<i64> {
        match &self.get(key)?.value {
            ParamValue::Integer(i) => Ok(*i),
            other => Err(type_error(key, ParamKind::Integer, other)),
        }
    }

    /// `key` as a boolean.
    ///
    /// # Errors
    ///
    /// As [`ParamSnapshot::get`]; `Validation` when the parameter is not a boolean.
    pub fn boolean(&self, key: &str) -> Result<bool> {
        match &self.get(key)?.value {
            ParamValue::Boolean(b) => Ok(*b),
            other => Err(type_error(key, ParamKind::Boolean, other)),
        }
    }

    /// `key` as text.
    ///
    /// # Errors
    ///
    /// As [`ParamSnapshot::get`]; `Validation` when the parameter is not text.
    pub fn text(&self, key: &str) -> Result<&str> {
        match &self.get(key)?.value {
            ParamValue::Text(t) => Ok(t),
            other => Err(type_error(key, ParamKind::Text, other)),
        }
    }

    /// Every resolved key with its value, in key order: what a rule or report binds as variables.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &ParamValue)> {
        self.entries.iter().map(|(k, v)| (k.as_ref(), &v.value))
    }
}

fn type_error(key: &str, wanted: ParamKind, found: &ParamValue) -> Error {
    Error::validation(format!(
        "Parameter '{key}' is {}, not {}.",
        found.kind().as_str(),
        wanted.as_str()
    ))
}

/// The parameters `keys` in force for `company` on `date`, in one search.
///
/// # Errors
///
/// `Validation` for an empty, oversized or malformed key list; `Access` when the caller may not
/// read the model or use the company; `UserError` naming the first key with no row in force;
/// a `Programming` invariant error when stored rows overlap (a bypassed constraint).
pub async fn resolve_many(
    env: &Env,
    tx: &mut Transaction,
    keys: &[&str],
    company: RecId,
    date: Date,
) -> Result<ParamSnapshot> {
    let wanted: BTreeSet<&str> = keys.iter().copied().collect();
    if wanted.is_empty() || wanted.len() > MAX_KEYS_PER_CALL {
        return Err(Error::validation(format!(
            "Between 1 and {MAX_KEYS_PER_CALL} parameter keys can be read at once."
        )));
    }
    for key in &wanted {
        validate_key(key)?;
    }
    // The caller's rights on the model, then the company, then a read as the system: a record rule
    // that filters rows cannot put a shared value in place of the company's own, as it could if
    // the search ran under the caller's rules (the same order as `RateBook::load`).
    env.records("ir.dated.parameter")?
        .check_access(tx, Operation::Read)
        .await?;
    authorize_company(tx, env, company.get()).await?;
    let chain = company_chain(tx, company.get()).await?;
    let model = env.sudo(true).records("ir.dated.parameter")?;
    let day = date.to_string();
    let in_force = Domain::condition("date_from", Operator::Le, day.clone())?
        & Domain::or(vec![
            Domain::condition("date_to", Operator::Eq, false)?,
            Domain::condition("date_to", Operator::Ge, day)?,
        ]);
    let scope = Domain::or(vec![
        Domain::condition("company_id", Operator::Eq, false)?,
        Domain::condition("company_id", Operator::In, chain.ids().to_vec())?,
    ]);
    let key_list: Vec<String> = wanted.iter().map(|k| (*k).to_owned()).collect();
    let domain = Domain::condition("key", Operator::In, key_list)? & in_force & scope;
    // at most one row per key in each scope: the company, each ancestor, and the shared scope
    let ceiling = wanted.len() * (chain.ids().len() + 1);
    let opts = SearchOptions {
        limit: Some(ceiling as u64 + 1),
        ..SearchOptions::default()
    };
    let found = model.search_with(tx, &domain, &opts).await?;
    let rows = found
        .read_with(
            tx,
            &[
                "key",
                "company_id",
                "date_from",
                "date_to",
                "value_kind",
                "value",
            ],
            Load::Raw,
        )
        .await?;
    if rows.len() > ceiling {
        return Err(Error::invariant(
            "ir.dated.parameter holds overlapping periods: the exclusion constraints were bypassed",
        ));
    }
    // nearest scope first: the company itself (rank 0), each ancestor up to the root, then shared
    let shared_rank = chain.ids().len();
    let mut best: BTreeMap<Arc<str>, (usize, ResolvedParam)> = BTreeMap::new();
    for row in &rows {
        let candidate = resolved_from_row(row)?;
        let rank = match candidate.company_id {
            None => shared_rank,
            Some(owner) => chain.ids().iter().position(|c| *c == owner).ok_or_else(|| {
                Error::invariant(format!(
                    "ir.dated.parameter row {} belongs to company {owner}, outside the chain read",
                    candidate.record_id
                ))
            })?,
        };
        match best.get(&candidate.key) {
            Some((current, _)) if *current == rank => {
                return Err(Error::invariant(format!(
                    "ir.dated.parameter holds two rows in force for '{}' in one scope",
                    candidate.key
                )));
            }
            Some((current, _)) if *current < rank => {}
            _ => {
                best.insert(candidate.key.clone(), (rank, candidate));
            }
        }
    }
    let entries: BTreeMap<Arc<str>, ResolvedParam> =
        best.into_iter().map(|(k, (_, v))| (k, v)).collect();
    for key in &wanted {
        if !entries.contains_key(*key) {
            return Err(Error::user(format!(
                "No value of parameter '{key}' is in force for company {} on {date}.",
                company.get()
            )));
        }
    }
    Ok(ParamSnapshot {
        company_id: company.get(),
        date,
        entries,
    })
}

/// One parameter in force for `company` on `date` (see [`resolve_many`]).
///
/// # Errors
///
/// As [`resolve_many`].
pub async fn resolve(
    env: &Env,
    tx: &mut Transaction,
    key: &str,
    company: RecId,
    date: Date,
) -> Result<ResolvedParam> {
    let snapshot = resolve_many(env, tx, &[key], company, date).await?;
    snapshot.get(key).cloned()
}

fn resolved_from_row(row: &std::collections::BTreeMap<String, Value>) -> Result<ResolvedParam> {
    let corrupt = |what: &str| {
        Error::validation(format!(
            "A stored ir.dated.parameter row has an invalid {what}."
        ))
    };
    let Some(Value::Int(record_id)) = row.get("id") else {
        return Err(corrupt("id"));
    };
    let Some(Value::Str(key)) = row.get("key") else {
        return Err(corrupt("key"));
    };
    let company_id = match row.get("company_id") {
        Some(Value::Int(id)) => Some(*id),
        Some(Value::False) | Some(Value::Bool(false)) | None => None,
        _ => return Err(corrupt("company")),
    };
    let Some(Value::Date(date_from)) = row.get("date_from") else {
        return Err(corrupt("start date"));
    };
    let date_to = match row.get("date_to") {
        Some(Value::Date(d)) => Some(*d),
        Some(Value::False) | Some(Value::Bool(false)) | None => None,
        _ => return Err(corrupt("end date")),
    };
    let (Some(Value::Str(kind)), Some(Value::Str(text))) =
        (row.get("value_kind"), row.get("value"))
    else {
        return Err(corrupt("value"));
    };
    Ok(ResolvedParam {
        key: key.clone(),
        value: ParamValue::parse(ParamKind::parse(kind)?, text)
            .map_err(|e| Error::validation(format!("Parameter '{key}': {}", e.message())))?,
        record_id: *record_id,
        company_id,
        date_from: *date_from,
        date_to,
    })
}
