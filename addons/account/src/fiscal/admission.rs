// SPDX-License-Identifier: MIT OR Apache-2.0
//! The question every writer asks before it writes a dated row: is the period of this date open
//! for a row of this class (D8; R1d)?

use std::collections::BTreeMap;

use erp_sdk::prelude::*;

use super::PERIOD_LOCK_CLASS;
use super::ensure::require_company_access;

/// What kind of row a writer is about to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WriteClass {
    /// A journal entry or line (`account.move`). The stock status does not bind
    /// it: an accounting writer does not refuse `stock_locking` (point 4).
    Accounting,
    /// A stock fact that carries a quantity: a stock move, a correction of a done line, a replay
    /// or job start (through `stock_account`, G5). The fiscal-year and hard
    /// locks bind it as well as the stock status.
    Stock,
}

/// The answer for one `(company, date)`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeriodVerdict {
    /// The period is open for the class.
    Open {
        /// The period.
        period_id: i64,
    },
    /// No regular period contains the date: run [`super::ensure_periods`] outside the unit of
    /// work, then restart it once (R2).
    NoPeriod {
        /// The company.
        company_id: i64,
        /// The date.
        date: Date,
    },
    /// The period is `closed` by the fiscal-year lock. An accounting writer applies its re-dating
    /// policy or refuses; a stock writer refuses.
    FiscalLock {
        /// The period.
        period_id: i64,
        /// Its name.
        name: String,
        /// Its last day.
        date_to: Date,
    },
    /// The period is `hard_locking` or `hard_locked`: every writer refuses; nothing lifts it.
    HardLock {
        /// The period.
        period_id: i64,
        /// Its name.
        name: String,
        /// `hard_locking` or `hard_locked`.
        status: String,
    },
    /// Stock class only: the period is `stock_locking` or `stock_locked`.
    StockLock {
        /// The period.
        period_id: i64,
        /// Its name.
        name: String,
        /// `stock_locking` or `stock_locked`.
        status: String,
    },
}

impl PeriodVerdict {
    /// Whether the row may be written at its date.
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }

    /// The open period, or the refusal as an error naming the period and the lock.
    ///
    /// # Errors
    ///
    /// `Missing` for [`PeriodVerdict::NoPeriod`], `Validation` for a lock.
    pub fn into_result(self) -> Result<i64> {
        match self {
            Self::Open { period_id } => Ok(period_id),
            Self::NoPeriod { company_id, date } => Err(Error::missing(format!(
                "No fiscal period for company {company_id} on {date}; create it with \
                 ensure_periods and try again."
            ))),
            Self::FiscalLock { name, date_to, .. } => Err(Error::validation(format!(
                "The fiscal period {name} is closed by the fiscal-year lock (up to {date_to}); \
                 you cannot add or modify entries dated in it."
            ))),
            Self::HardLock { name, status, .. } => Err(Error::validation(format!(
                "The fiscal period {name} is {status} by the hard lock; nothing can be written in it."
            ))),
            Self::StockLock { name, status, .. } => Err(Error::validation(format!(
                "The fiscal period {name} is {status} by the stock lock; stock dated in it cannot be \
                 changed. Post a correction in an open period."
            ))),
        }
    }
}

/// Decide, for every `(company_id, date)` of `rows`, whether a row of `class` may be written at
/// that date. One verdict per input pair, in input order; set-based: three statements whatever the
/// batch size.
///
/// Before reading the statuses it takes `pg_advisory_xact_lock_shared(PERIOD_LOCK_CLASS, id)` on
/// each distinct period in id order, as the hard-lock triggers do (R1d): a lock
/// operation's T1 takes the exclusive lock and so waits for this transaction, and its drain waits
/// for every transaction that may have read `open`. Call it at the start of the
/// writer's unit of work, once per batch, and write in the same transaction.
///
/// The verdict is the one of the transaction's snapshot. At `REPEATABLE READ` a writer whose
/// snapshot predates a committed status change still reads the old status; the lock only orders
/// writers that ask after the change. A status change that must stop such writers drains them
/// first (the fiscal-year close through [`super::status::sync_lock_dates`] does not
/// yet).
///
/// What the period table cannot say is the caller's: the re-dating policy for a fiscal lock,
/// the backdating window (D3), the sale, purchase and tax soft locks and
/// their exceptions, the close-horizon checks of the valuation bridge.
///
/// Outside superuser mode every company of `rows` must be one of the caller's or a branch of one,
/// so the statuses of another company's periods are not readable here. Take
/// the companies from records the caller's own access check already admitted (the record API's
/// read or write of the rows being written), never from request input; in superuser mode this
/// check is the caller's.
///
/// # Errors
///
/// `Access` ([`super::ensure::NOT_ALLOWED`]) for a company the caller does not hold or one that
/// does not exist; any database error.
pub async fn period_admission(
    tx: &mut Transaction,
    class: WriteClass,
    rows: &[(i64, Date)],
) -> Result<Vec<PeriodVerdict>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let companies: Vec<i64> = rows.iter().map(|(c, _)| *c).collect();
    require_company_access(tx, &companies).await?;
    let dates: Vec<Date> = rows.iter().map(|(_, d)| *d).collect();
    let found = tx
        .query(
            "SELECT r.ord::int8, p.id::int8 FROM unnest($1::int8[], $2::date[]) \
             WITH ORDINALITY AS r(company_id, d, ord) \
             LEFT JOIN account_fiscal_period p ON p.company_id = r.company_id \
             AND p.kind = 'regular' AND r.d BETWEEN p.date_from AND p.date_to ORDER BY r.ord",
            &[&companies, &dates],
        )
        .await?;
    let mut period_of: Vec<Option<i64>> = Vec::with_capacity(rows.len());
    for row in &found {
        period_of.push(row.try_get(1).map_err(|e| Error::db(e.to_string()))?);
    }
    if period_of.len() != rows.len() {
        return Err(Error::internal(format!(
            "period admission read {} rows for {} dates",
            period_of.len(),
            rows.len()
        )));
    }
    let mut ids: Vec<i64> = period_of.iter().flatten().copied().collect();
    ids.sort_unstable();
    ids.dedup();
    let mut states: BTreeMap<i64, (String, String, String, Date)> = BTreeMap::new();
    if !ids.is_empty() {
        let keys: Vec<i32> = ids
            .iter()
            .map(|id| i32::try_from(*id).map_err(|_| Error::internal("period id beyond int4")))
            .collect::<Result<_>>()?;
        tx.query(
            "SELECT pg_advisory_xact_lock_shared($1::int4, k) FROM unnest($2::int4[]) \
             WITH ORDINALITY AS l(k, ord) ORDER BY ord",
            &[&PERIOD_LOCK_CLASS, &keys],
        )
        .await?;
        let read = tx
            .query(
                "SELECT id::int8, name, status, stock_lock_status, date_to \
                 FROM account_fiscal_period WHERE id = ANY($1::int8[])",
                &[&ids],
            )
            .await?;
        for row in &read {
            let get = |i: usize| -> Result<String> {
                row.try_get(i).map_err(|e| Error::db(e.to_string()))
            };
            let id: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
            let date_to: Date = row.try_get(4).map_err(|e| Error::db(e.to_string()))?;
            states.insert(id, (get(1)?, get(2)?, get(3)?, date_to));
        }
    }
    rows.iter()
        .zip(period_of)
        .map(|((company_id, date), period)| {
            let Some(period_id) = period else {
                return Ok(PeriodVerdict::NoPeriod {
                    company_id: *company_id,
                    date: *date,
                });
            };
            let (name, status, stock, date_to) = states
                .get(&period_id)
                .cloned()
                .ok_or_else(|| Error::internal(format!("period {period_id} vanished")))?;
            verdict(class, period_id, name, status, stock, date_to)
        })
        .collect()
}

fn verdict(
    class: WriteClass,
    period_id: i64,
    name: String,
    status: String,
    stock: String,
    date_to: Date,
) -> Result<PeriodVerdict> {
    // the database admits only these values (`_period_status_values`, `_period_stock_values`);
    // anything else is never read as open
    let unknown = |what: &str, value: &str| {
        Error::internal(format!(
            "fiscal period {name} (id {period_id}) has the unknown {what} {value:?}"
        ))
    };
    if !matches!(stock.as_str(), "open" | "stock_locking" | "stock_locked") {
        return Err(unknown("stock status", &stock));
    }
    let verdict = match status.as_str() {
        "hard_locking" | "hard_locked" => PeriodVerdict::HardLock {
            period_id,
            name,
            status,
        },
        "closed" => PeriodVerdict::FiscalLock {
            period_id,
            name,
            date_to,
        },
        "open" if class == WriteClass::Stock && stock != "open" => PeriodVerdict::StockLock {
            period_id,
            name,
            status: stock,
        },
        "open" => PeriodVerdict::Open { period_id },
        other => return Err(unknown("status", other)),
    };
    Ok(verdict)
}

/// [`period_admission`] for writers that refuse every lock: the open period of each row, or the
/// first refusal as an error.
///
/// # Errors
///
/// As [`PeriodVerdict::into_result`] for the first row that is not open; database errors.
pub async fn require_open_periods(
    tx: &mut Transaction,
    class: WriteClass,
    rows: &[(i64, Date)],
) -> Result<Vec<i64>> {
    period_admission(tx, class, rows)
        .await?
        .into_iter()
        .map(PeriodVerdict::into_result)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_sdk::ErrorKind;

    #[test]
    fn the_period_status_decides_by_class() {
        let today = Date::from_days(20_000);
        let v = |class, status: &str, stock: &str| {
            verdict(
                class,
                7,
                "2026/01".into(),
                status.into(),
                stock.into(),
                today,
            )
            .unwrap()
        };
        use WriteClass::{Accounting, Stock};
        assert!(v(Accounting, "open", "stock_locked").is_open());
        assert!(v(Accounting, "open", "stock_locking").is_open());
        assert!(v(Stock, "open", "open").is_open());
        assert!(matches!(
            v(Stock, "open", "stock_locking"),
            PeriodVerdict::StockLock { .. }
        ));
        assert!(matches!(
            v(Stock, "open", "stock_locked"),
            PeriodVerdict::StockLock { .. }
        ));
        for class in [Accounting, Stock] {
            assert!(matches!(
                v(class, "closed", "open"),
                PeriodVerdict::FiscalLock { .. }
            ));
            assert!(matches!(
                v(class, "hard_locking", "open"),
                PeriodVerdict::HardLock { .. }
            ));
            assert!(matches!(
                v(class, "hard_locked", "stock_locked"),
                PeriodVerdict::HardLock { .. }
            ));
        }
        // an unknown status or stock status is an internal error naming the period and the value,
        // for both classes (S1: never admitted as open)
        for class in [Accounting, Stock] {
            for (status, stock, value) in
                [("frozen", "open", "frozen"), ("open", "melted", "melted")]
            {
                let error = verdict(
                    class,
                    7,
                    "2026/01".into(),
                    status.into(),
                    stock.into(),
                    today,
                )
                .unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Internal, "{error:?}");
                assert!(
                    error.message().contains("2026/01") && error.message().contains(value),
                    "{error:?}"
                );
            }
        }
    }
}
