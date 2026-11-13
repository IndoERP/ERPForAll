// SPDX-License-Identifier: MIT OR Apache-2.0
//! Journal entry numbering.
//!
//! Numbers come from dedicated chain rows, one per `(journal_id, kind, period_key)`, never from the
//! highest stored name as in the reference (
//! takes the first number by an `UPDATE ... SET name` under the unique index and retries in a
//! savepoint loop). [`assign_numbers`] is the allocation point the posting step calls last (
//! D2.3 step 7): it locks the chain rows in key order, raises their counters with one statement
//! and returns the numbers and rendered names; the caller writes them on its documents in the same
//! transaction. A rollback of that transaction rolls the counters back, so no number is lost.
//!
//! The pattern and reset period are declared by the caller (the journal,.2) as a
//! [`SequencePattern`]; this module renders names with it and resolves the period key from it.
//! The wait for a chain row held by another posting is the declared setting [`LOCK_WAIT_SETTING`].
//! Developer page.

use std::collections::BTreeMap;

use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::prelude::*;
use erp_sdk::sequence::{FiscalYear, SequencePattern};
use erp_sdk::serde_json;
use erp_sdk::settings::{ScopeKind, SettingDecl};
use erp_sdk::views::wire::SettingsScope;
use erp_sdk::{Condition, FieldType};

/// The chain model.
pub const CHAIN_MODEL: &str = "account.sequence.chain";

/// One number chain of a journal: its kind and reset period, and the next number it hands out.
///
/// Rows are created on first use by [`assign_numbers`]; the reference has no such table (its next
/// number is read from the highest move name),
/// so there are no default records to ship. Only the numbering service writes the rows; users get
/// read access.
#[model(
    name = "account.sequence.chain",
    description = "Journal Entry Number Chain",
    order = "journal_id, kind, period_key",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"),
    // every post rewrites a few rows of a small table: room for HOT updates (2)
    fillfactor = 50,
    constraints = [
        (
            "_chain_key_unique",
            "UNIQUE (journal_id, kind, period_key)",
            "A journal has one number chain per kind and period."
        ),
        (
            "_next_number_positive",
            "CHECK (next_number >= 1)",
            "The next number of a chain is at least 1."
        )
    ]
)]
pub struct AccountSequenceChain {
    /// The journal's company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        readonly = true
    )]
    pub company_id: Option<RecId>,
    /// The journal id. It becomes a many2one to `account.journal` when that model exists;
    /// the wiring slice changes the column.
    #[field(integer, required = true, readonly = true)]
    pub journal_id: i32,
    /// The sequence kind (2 amendment A5): addons add kinds with `selection_add`.
    #[field(
        selection,
        selection = [("document", "Document"), ("refund", "Refund"), ("payment", "Payment")],
        required = true,
        readonly = true
    )]
    pub kind: Option<Arc<str>>,
    /// The reset period: `never`, `2026`, `2026-07`, `2025-26` or `2025-26-07`.
    #[field(char, required = true, readonly = true)]
    pub period_key: Option<Arc<str>>,
    /// The number the next document of the chain gets.
    #[field(integer, required = true, default = 1, readonly = true)]
    pub next_number: i32,
    /// The last name the chain handed out.
    #[field(char, readonly = true)]
    pub last_name: Option<Arc<str>>,
}

/// Declared setting: how long a posting waits for a number chain another posting holds, in
/// milliseconds, before it is refused.
pub const LOCK_WAIT_SETTING: &str = "account.sequence_lock_wait_ms";
/// Default of [`LOCK_WAIT_SETTING`].
pub const LOCK_WAIT_DEFAULT_MS: i64 = 5_000;
/// Floor of [`LOCK_WAIT_SETTING`].
pub const LOCK_WAIT_FLOOR_MS: i64 = 100;
/// Hard bound of [`LOCK_WAIT_SETTING`]: above a minute a held chain is an incident, not
/// contention, and the wait must stay below the unit of work's statement limit.
pub const LOCK_WAIT_HARD_MS: i64 = 60_000;

fn settings() -> Result<Vec<SettingDecl>> {
    Ok(vec![
        SettingDecl::integer(
            LOCK_WAIT_SETTING,
            "Journal entry numbering: wait for a number chain in use (milliseconds)",
            ScopeKind::Global,
            LOCK_WAIT_DEFAULT_MS,
            (LOCK_WAIT_FLOOR_MS, LOCK_WAIT_HARD_MS),
        )?
        .block("Journal entry numbering")?
        .access(&["base.group_system"], &["base.group_system"])?,
    ])
}
erp_sdk::setting_declarations!(settings);

/// The wait for a held chain row, inside the bounds of [`LOCK_WAIT_SETTING`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockWait {
    millis: i64,
}

impl LockWait {
    /// A wait of `millis` milliseconds.
    ///
    /// # Errors
    ///
    /// `Validation` naming the setting when `millis` is outside its floor and hard bound.
    pub fn from_millis(millis: i64) -> Result<Self> {
        if !(LOCK_WAIT_FLOOR_MS..=LOCK_WAIT_HARD_MS).contains(&millis) {
            return Err(Error::validation(format!(
                "{LOCK_WAIT_SETTING} is {millis} ms; it must be between {LOCK_WAIT_FLOOR_MS} and \
                 {LOCK_WAIT_HARD_MS} ms"
            )));
        }
        Ok(Self { millis })
    }

    /// The configured wait (the setting's default when it was never set). The framework reads
    /// its own configuration with elevated rights; the poster needs no settings group.
    ///
    /// # Errors
    ///
    /// Settings read failures; a stored value outside the bounds.
    pub async fn from_settings(tx: &mut Transaction) -> Result<Self> {
        let env = tx.env().sudo(true);
        let read = read_values(
            &env,
            tx,
            SettingsScope::Global {},
            vec![LOCK_WAIT_SETTING.to_owned()],
        )
        .await?;
        let read = serde_json::to_value(&read)
            .map_err(|error| Error::internal(format!("{LOCK_WAIT_SETTING}: {error}")))?;
        let millis = read["values"]
            .as_array()
            .and_then(|values| values.iter().find(|v| v["key"] == LOCK_WAIT_SETTING))
            .and_then(|entry| entry["value"].as_i64())
            .ok_or_else(|| Error::internal(format!("{LOCK_WAIT_SETTING} is not an integer")))?;
        Self::from_millis(millis)
    }

    /// The wait in milliseconds.
    pub fn millis(self) -> i64 {
        self.millis
    }
}

/// One document to number.
#[derive(Debug, Clone)]
pub struct NumberRequest<'a> {
    /// The document's id: with `date` and `reference` it orders the batch.
    pub document_id: i64,
    /// The journal's company.
    pub company_id: i64,
    /// The journal.
    pub journal_id: i64,
    /// A kind declared on [`AccountSequenceChain::kind`].
    pub kind: &'a str,
    /// The journal's declared pattern for this kind; every request of a chain uses the same.
    pub pattern: &'a SequencePattern,
    /// The accounting date: it picks the period.
    pub date: Date,
    /// The fiscal year of the document's company containing `date`; required by the fiscal
    /// resets, ignored by the others.
    pub fiscal_year: Option<FiscalYear>,
    pub reference: Option<&'a str>,
}

/// A number given to a document.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AssignedNumber {
    /// The document.
    pub document_id: i64,
    /// The chain row the number belongs to.
    pub chain_id: i64,
    /// The number inside the chain.
    pub sequence_number: i64,
    /// The rendered name.
    pub name: String,
}

/// Number a batch of documents (3 step 7). Call it last in the posting transaction:
/// the chain rows stay locked until the transaction ends.
///
/// Inside a chain the documents are numbered in the reference's order `(date, reference, id)`
/// (a missing reference sorts as empty text,
/// compared by code point). Chains are locked in `(journal_id, kind, period_key)` order (
/// D2.1 step 4); a missing chain row is created first. The chain lock waits at most `wait`;
/// [`Transaction`]'s own `lock_timeout` applies again afterwards. The result is in request order.
///
/// Under `REPEATABLE READ`, a posting that waited for a chain row which its holder then advanced
/// and committed fails with `Concurrency`/`40001`: `Orm::run` runs the unit again and the new
/// attempt takes the next number.
///
/// # Errors
///
/// - `Validation`: a kind that is not declared, one chain asked with two patterns or two
///   companies, a document listed twice, a counter beyond the column's range, a render error;
/// - `Lock` with [`Condition::LockTimeout`] naming the chains and the setting, when a chain row
///   is not free within `wait` (never retried);
/// - `Concurrency` (`40001`) as above; any database error.
pub async fn assign_numbers(
    tx: &mut Transaction,
    wait: LockWait,
    requests: &[NumberRequest<'_>],
) -> Result<Vec<AssignedNumber>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let chains = group_requests(tx, requests)?;
    let keys: Vec<(&ChainKey, i64)> = chains.iter().map(|(k, g)| (k, g.company_id)).collect();
    let locked = lock_chains(tx, wait, &keys).await?;
    // The wait applies to the lock statements only: the caller's own timeout is back before any
    // other step, so a refusal below never leaves the numbering wait in force.
    restore_lock_wait(tx, &locked.previous_wait).await?;

    let mut assigned = Vec::with_capacity(requests.len());
    let (mut ids, mut next, mut last) = (Vec::new(), Vec::new(), Vec::new());
    for ((key, group), row) in chains.iter().zip(&locked.rows) {
        if row.company_id != group.company_id {
            return Err(Error::validation(format!(
                "the number chain {key} belongs to company {}, not {}",
                row.company_id, group.company_id
            )));
        }
        let mut number = i64::from(row.next_number);
        let mut last_name = String::new();
        for request in &group.requests {
            last_name =
                request
                    .pattern
                    .render(number, request.date, request.fiscal_year.as_ref())?;
            assigned.push(AssignedNumber {
                document_id: request.document_id,
                chain_id: row.id,
                sequence_number: number,
                name: last_name.clone(),
            });
            number += 1;
        }
        let number = i32::try_from(number).map_err(|_| {
            Error::validation(format!("the number chain {key} has no numbers left"))
        })?;
        ids.push(row.id);
        next.push(number);
        last.push(last_name);
    }
    let updated = tx
        .execute(
            "UPDATE account_sequence_chain AS c SET next_number = v.next, last_name = v.last \
             FROM unnest($1::int8[], $2::int4[], $3::text[]) AS v(id, next, last) WHERE c.id = v.id",
            &[&ids, &next, &last],
        )
        .await?;
    if updated != ids.len() as u64 {
        return Err(Error::internal(format!(
            "numbering updated {updated} of {} locked chain rows",
            ids.len()
        )));
    }
    tx.invalidate_model(CHAIN_MODEL, Some(&["next_number", "last_name"]))
        .await?;

    let order: BTreeMap<i64, usize> = requests
        .iter()
        .enumerate()
        .map(|(i, r)| (r.document_id, i))
        .collect();
    assigned.sort_by_key(|a| order.get(&a.document_id).copied());
    Ok(assigned)
}

/// The key of a chain row. Field order is the lock order; `String` compares by byte, which is
/// code-point order for UTF-8 and the `COLLATE "C"` order of the lock statement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ChainKey {
    journal_id: i64,
    kind: String,
    period_key: String,
}

impl std::fmt::Display for ChainKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "(journal {}, kind {}, period {})",
            self.journal_id, self.kind, self.period_key
        )
    }
}

struct ChainGroup<'r, 'a> {
    company_id: i64,
    pattern: &'a SequencePattern,
    requests: Vec<&'r NumberRequest<'a>>,
}

fn group_requests<'r, 'a>(
    tx: &Transaction,
    requests: &'r [NumberRequest<'a>],
) -> Result<BTreeMap<ChainKey, ChainGroup<'r, 'a>>> {
    let kinds = declared_kinds(tx)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut chains: BTreeMap<ChainKey, ChainGroup<'r, 'a>> = BTreeMap::new();
    for request in requests {
        if !seen.insert(request.document_id) {
            return Err(Error::validation(format!(
                "document {} is listed twice in one numbering batch",
                request.document_id
            )));
        }
        if !kinds.iter().any(|k| k == request.kind) {
            return Err(Error::validation(format!(
                "sequence kind {:?} is not declared on {CHAIN_MODEL}.kind (declared: {})",
                request.kind,
                kinds.join(", ")
            )));
        }
        let period = request
            .pattern
            .period_key(request.date, request.fiscal_year.as_ref())?;
        let key = ChainKey {
            journal_id: request.journal_id,
            kind: request.kind.to_owned(),
            period_key: period.as_str().to_owned(),
        };
        let group = chains.entry(key.clone()).or_insert_with(|| ChainGroup {
            company_id: request.company_id,
            pattern: request.pattern,
            requests: Vec::new(),
        });
        if group.company_id != request.company_id {
            return Err(Error::validation(format!(
                "the number chain {key} is asked for companies {} and {} in one batch",
                group.company_id, request.company_id
            )));
        }
        if group.pattern != request.pattern {
            return Err(Error::validation(format!(
                "the number chain {key} is asked with two different patterns in one batch"
            )));
        }
        group.requests.push(request);
    }
    for group in chains.values_mut() {
        group.requests.sort_by(|a, b| {
            (a.date, a.reference.unwrap_or(""), a.document_id).cmp(&(
                b.date,
                b.reference.unwrap_or(""),
                b.document_id,
            ))
        });
    }
    Ok(chains)
}

/// The kinds declared on the chain model, `selection_add` of other addons included.
fn declared_kinds(tx: &Transaction) -> Result<Vec<String>> {
    let field = tx
        .registry()
        .model(CHAIN_MODEL)
        .and_then(|model| model.field("kind"))
        .ok_or_else(|| Error::programming(format!("{CHAIN_MODEL}.kind is not in the registry")))?;
    match &field.ty {
        FieldType::Selection {
            values: Some(values),
            ..
        } => Ok(values.iter().map(|(key, _)| key.clone()).collect()),
        other => Err(Error::programming(format!(
            "{CHAIN_MODEL}.kind must be a static selection, found {}",
            other.type_name()
        ))),
    }
}

struct LockedRow {
    id: i64,
    company_id: i64,
    next_number: i32,
}

struct Locked {
    rows: Vec<LockedRow>,
    previous_wait: String,
}

/// Create the missing chain rows (with the company of the request) and lock all of them in key
/// order, under `wait`. `keys` is sorted; the rows come back in its order, with the
/// transaction's own `lock_timeout` to restore.
async fn lock_chains(
    tx: &mut Transaction,
    wait: LockWait,
    keys: &[(&ChainKey, i64)],
) -> Result<Locked> {
    let companies: Vec<i64> = keys.iter().map(|(_, c)| *c).collect();
    let journals: Vec<i64> = keys.iter().map(|(k, _)| k.journal_id).collect();
    let kinds: Vec<&str> = keys.iter().map(|(k, _)| k.kind.as_str()).collect();
    let periods: Vec<&str> = keys.iter().map(|(k, _)| k.period_key.as_str()).collect();
    let rows = tx
        .query(
            "SELECT current_setting('lock_timeout'), set_config('lock_timeout', $1, true)",
            &[&wait.millis.to_string()],
        )
        .await?;
    let previous_wait: String = rows
        .first()
        .ok_or_else(|| Error::internal("current_setting returned no row"))?
        .try_get(0)
        .map_err(|e| Error::db(e.to_string()))?;
    let failed = |e: Error| lock_error(e, wait, keys);
    tx.execute(
        "INSERT INTO account_sequence_chain (company_id, journal_id, kind, period_key, next_number) \
         SELECT v.c, v.j, v.k, v.p, 1 FROM unnest($1::int8[], $2::int8[], $3::text[], $4::text[]) \
         AS v(c, j, k, p) ORDER BY v.j, v.k COLLATE \"C\", v.p COLLATE \"C\" \
         ON CONFLICT (journal_id, kind, period_key) DO NOTHING",
        &[&companies, &journals, &kinds, &periods],
    )
    .await
    .map_err(failed)?;
    let found = tx
        .query(
            "SELECT c.id::int8, c.company_id::int8, c.next_number, c.journal_id::int8, c.kind, \
             c.period_key FROM account_sequence_chain c \
             WHERE (c.journal_id, c.kind, c.period_key) IN \
             (SELECT * FROM unnest($1::int8[], $2::text[], $3::text[])) \
             ORDER BY c.journal_id, c.kind COLLATE \"C\", c.period_key COLLATE \"C\" FOR UPDATE OF c",
            &[&journals, &kinds, &periods],
        )
        .await
        .map_err(failed)?;
    // an internal error here must not leave the numbering wait in force either
    let parsed = (|| -> Result<Vec<LockedRow>> {
        if found.len() != keys.len() {
            return Err(Error::internal(format!(
                "numbering locked {} of {} chain rows",
                found.len(),
                keys.len()
            )));
        }
        found
            .iter()
            .zip(keys)
            .map(|(row, (key, _))| {
                let found = ChainKey {
                    journal_id: row.try_get(3).map_err(|e| Error::db(e.to_string()))?,
                    kind: row.try_get(4).map_err(|e| Error::db(e.to_string()))?,
                    period_key: row.try_get(5).map_err(|e| Error::db(e.to_string()))?,
                };
                if &found != *key {
                    return Err(Error::internal(format!(
                        "numbering locked chain {found} where {key} was expected"
                    )));
                }
                Ok(LockedRow {
                    id: row.try_get(0).map_err(|e| Error::db(e.to_string()))?,
                    company_id: row.try_get(1).map_err(|e| Error::db(e.to_string()))?,
                    next_number: row.try_get(2).map_err(|e| Error::db(e.to_string()))?,
                })
            })
            .collect()
    })();
    match parsed {
        Ok(rows) => Ok(Locked {
            rows,
            previous_wait,
        }),
        Err(error) => {
            restore_lock_wait(tx, &previous_wait).await?;
            Err(error)
        }
    }
}

async fn restore_lock_wait(tx: &mut Transaction, previous: &str) -> Result<()> {
    tx.query("SELECT set_config('lock_timeout', $1, true)", &[&previous])
        .await?;
    Ok(())
}

/// A lock-timeout failure named for the poster; every other error unchanged.
fn lock_error(error: Error, wait: LockWait, keys: &[(&ChainKey, i64)]) -> Error {
    if error.condition() != Some(Condition::LockTimeout) {
        return error;
    }
    let shown: Vec<String> = keys.iter().take(3).map(|(k, _)| k.to_string()).collect();
    let more = keys.len().saturating_sub(shown.len());
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    let (kind, sqlstate) = (error.kind(), error.sqlstate().map(str::to_owned));
    let message = format!(
        "another posting holds the number chain {}{more}; it was not free within {} ms \
         (setting {LOCK_WAIT_SETTING}). Post again once that posting has finished.",
        shown.join(", "),
        wait.millis
    );
    Error::database(kind, sqlstate, message, error).with_condition(Condition::LockTimeout)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_sdk::settings::SettingKind;

    /// The lock wait is a declared global setting with a default, a floor and a hard bound.
    #[test]
    fn the_lock_wait_is_a_bounded_declared_setting() {
        let declarations = settings().unwrap();
        assert_eq!(declarations.len(), 1);
        let setting = &declarations[0];
        assert_eq!(setting.key(), LOCK_WAIT_SETTING);
        assert_eq!(setting.default_value(), &Value::Int(LOCK_WAIT_DEFAULT_MS));
        let SettingKind::Integer { min, max } = setting.kind() else {
            panic!("integer setting expected");
        };
        assert_eq!((*min, *max), (LOCK_WAIT_FLOOR_MS, LOCK_WAIT_HARD_MS));
        assert_eq!(setting.write_groups(), &["base.group_system"]);
    }

    #[test]
    fn a_lock_wait_outside_the_bounds_is_refused_by_name() {
        for millis in [0, LOCK_WAIT_FLOOR_MS - 1, LOCK_WAIT_HARD_MS + 1, -5] {
            let error = LockWait::from_millis(millis).unwrap_err();
            assert!(
                error.message().contains(LOCK_WAIT_SETTING),
                "{}",
                error.message()
            );
        }
        for millis in [LOCK_WAIT_FLOOR_MS, LOCK_WAIT_DEFAULT_MS, LOCK_WAIT_HARD_MS] {
            assert_eq!(LockWait::from_millis(millis).unwrap().millis(), millis);
        }
    }
}
