// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bank Statements (`account.bank.statement`) and Bank Reconciliation
//! (`account.bank.statement.line`).

use std::str::FromStr;

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RateBook, RoundingTable};
use erp_sdk::Operation;
use erp_sdk::money::RateDateRule;
use erp_sdk::prelude::*;

use crate::moves::post::strip_default_context;
use crate::moves::post_moves;
use crate::reconcile::reconcile_lines;

const MAX_CSV_BYTES: usize = 1_048_576;
const MAX_CSV_ROWS: usize = 5_000;

/// One parsed row from a bank statement CSV snippet (`date,label,amount[,ref]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvStatementRow {
    pub date: Date,
    pub label: String,
    pub amount: Decimal,
    pub reference: Option<String>,
}

/// Splits one CSV line honoring RFC-4180 double-quoted fields and escaped `""`.
fn split_csv_line(line: &str) -> Result<Vec<String>> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(ch);
            }
        } else if ch == '"' && cur.trim().is_empty() {
            cur.clear();
            in_quotes = true;
        } else if ch == ',' {
            fields.push(cur.trim().to_owned());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    if in_quotes {
        return Err(Error::user("Unterminated quoted field in CSV line."));
    }
    fields.push(cur.trim().to_owned());
    Ok(fields)
}

/// Pure exact-decimal parser for bank statement CSV lines (`YYYY-MM-DD,Label,Amount[,Reference]`).
/// Skips empty lines and an optional header row starting with `date`.
pub fn parse_bank_statement_csv(raw: &str) -> Result<Vec<CsvStatementRow>> {
    if raw.len() > MAX_CSV_BYTES {
        return Err(Error::validation(format!(
            "CSV import exceeds the maximum size of {MAX_CSV_BYTES} bytes."
        )));
    }
    let mut rows = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cols = split_csv_line(trimmed)?;
        if idx == 0 && cols.first().is_some_and(|c| c.eq_ignore_ascii_case("date")) {
            continue;
        }
        if rows.len() >= MAX_CSV_ROWS {
            return Err(Error::validation(format!(
                "CSV import exceeds the maximum of {MAX_CSV_ROWS} transaction rows."
            )));
        }
        if cols.len() < 3 {
            return Err(Error::user(format!(
                "CSV line {} must have at least 3 columns (date,label,amount): {trimmed}",
                idx + 1
            )));
        }
        let date = Date::parse_odoo(&cols[0]).ok_or_else(|| {
            Error::user(format!(
                "CSV line {} has an invalid date {:?}; expected YYYY-MM-DD.",
                idx + 1,
                cols[0]
            ))
        })?;
        let label = cols[1].clone();
        if label.is_empty() {
            return Err(Error::user(format!(
                "CSV line {} has an empty label.",
                idx + 1
            )));
        }
        let amount = Decimal::from_str(&cols[2]).map_err(|_| {
            Error::user(format!(
                "CSV line {} has an invalid decimal amount {:?}.",
                idx + 1,
                cols[2]
            ))
        })?;
        let reference = cols.get(3).filter(|s| !s.is_empty()).cloned();
        rows.push(CsvStatementRow {
            date,
            label,
            amount,
            reference,
        });
    }
    if rows.is_empty() {
        return Err(Error::user(
            "Provide at least one transaction row (YYYY-MM-DD,Label,Amount).",
        ));
    }
    Ok(rows)
}

/// Pure exact-decimal balance split for one bank statement line.
/// Positive `amount` (bank inflow) debits the bank account (`+amount`) and credits the counterpart
/// (`-amount`). Negative `amount` (bank outflow) credits the bank account and debits counterpart.
pub fn compute_statement_entry_balances(amount: Decimal) -> (Decimal, Decimal) {
    (amount, -amount)
}

async fn resolve_default_today(tx: &mut Transaction, env: &Env) -> Result<Date> {
    if let Some(cid) = env.company_id()? {
        erp_addon_base::company_calendar::business_date(tx, cid).await
    } else {
        env.context_today()
    }
}

fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Bank statement query failed: {err}"))
}

/// `account.bank.statement`: periodic bank or cash statement grouping statement lines.
#[model(
    name = "account.bank.statement",
    description = "Bank Statement",
    order = "date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountBankStatement {
    /// Auto-numbered statement reference (`BNKST/2026/00001`).
    #[field(
        char,
        required = true,
        default = "New",
        copy = false,
        string = "Reference"
    )]
    pub name: Option<Arc<str>>,
    /// Bank, cash or credit-card journal.
    #[field(
        many2one,
        comodel = "account.journal",
        required = true,
        check_company = true,
        ondelete = "restrict",
        domain = [("type", "in", ["bank", "cash", "credit"])],
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// Owning company from the journal.
    #[field(
        many2one,
        comodel = "res.company",
        related = "journal_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency from the journal or its company.
    #[field(
        many2one,
        comodel = "res.currency",
        compute = "_compute_currency",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Statement date.
    #[field(date, required = true, string = "Date")]
    pub date: Option<Date>,
    /// Opening balance carried from the previous statement.
    #[field(monetary, currency_field = "currency_id", string = "Starting Balance")]
    pub balance_start: Decimal,
    /// Closing balance reported by the bank.
    #[field(monetary, currency_field = "currency_id", string = "Ending Balance")]
    pub balance_end_real: Decimal,
    /// Computed closing balance (`balance_start + sum(line_ids.amount)`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_balances",
        store = true,
        string = "Computed Balance"
    )]
    pub balance_end: Decimal,
    /// True when `balance_end == balance_end_real`.
    #[field(
        boolean,
        compute = "_compute_balances",
        store = true,
        string = "Balance Matches"
    )]
    pub is_complete: bool,
    /// Count of statement lines not yet reconciled.
    #[field(
        integer,
        compute = "_compute_balances",
        store = true,
        string = "Lines To Reconcile"
    )]
    pub unreconciled_count: i32,
    /// Open while reconciling; Posted once balanced and reconciled.
    #[field(
        selection,
        selection = [("open", "Open"), ("posted", "Posted")],
        required = true,
        readonly = true,
        copy = false,
        default = "open",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// Statement transactions.
    #[field(
        one2many,
        comodel = "account.bank.statement.line",
        inverse_name = "statement_id",
        string = "Transactions"
    )]
    pub line_ids: Vec<RecId>,
    /// Optional pasted CSV rows (`YYYY-MM-DD,Label,Amount[,Reference]`) imported via button.
    #[field(text, string = "Import CSV (date,label,amount,ref)")]
    pub csv_import_data: Option<Arc<str>>,
}

#[methods(AccountBankStatement, addon = "account")]
impl AccountBankStatement {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let env = this.env();
        if names.iter().any(|n| n == "date") && !values.contains_key("date") {
            let today = resolve_default_today(tx, env).await?;
            values.insert("date".to_owned(), Value::Date(today));
        }
        if let Some(cid) = env.company_id()?
            && names.iter().any(|n| n == "journal_id")
            && !values.contains_key("journal_id")
        {
            let jrows = tx
                .query(
                    "SELECT id::int8 FROM account_journal \
                     WHERE company_id = $1 AND type IN ('bank', 'cash') AND active = true \
                     ORDER BY CASE type WHEN 'bank' THEN 0 ELSE 1 END, sequence, id LIMIT 1",
                    &[&cid],
                )
                .await?;
            if let Some(row) = jrows.first() {
                let jid: i64 = row.try_get(0).map_err(db)?;
                values.insert("journal_id".to_owned(), Value::Int(jid));
            }
        }
        Ok(values)
    }

    #[compute(candidate, depends = ["journal_id"])]
    async fn _compute_currency(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut ReadTx<'_>,
        out: &mut Assign<'_, AccountBankStatement>,
    ) -> Result<()> {
        let journals = tx.mapped(this, AccountBankStatement::JOURNAL_ID).await?;
        for (i, journal) in journals.into_iter().enumerate() {
            let mut currency_id = None;
            if let Some(jid) = journal {
                let rows = tx
                    .query(
                        "SELECT COALESCE(j.currency_id, c.currency_id)::int8 \
                         FROM account_journal j JOIN res_company c ON c.id = j.company_id \
                         WHERE j.id = $1",
                        &[&jid.get()],
                    )
                    .await?;
                if let Some(row) = rows.first() {
                    let cid: Option<i64> = row.try_get(0).map_err(db)?;
                    currency_id = cid.and_then(RecId::from_real);
                }
            }
            out.set(i, AccountBankStatement::CURRENCY_ID, currency_id)?;
        }
        Ok(())
    }

    #[compute(depends = [
        "balance_start",
        "balance_end_real",
        "line_ids",
        "line_ids.amount",
        "line_ids.is_reconciled",
    ])]
    async fn _compute_balances(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountBankStatement>,
    ) -> Result<()> {
        let starts = this.mapped(tx, AccountBankStatement::BALANCE_START).await?;
        let reals = this
            .mapped(tx, AccountBankStatement::BALANCE_END_REAL)
            .await?;
        let rec_ids = this.as_records().rec_ids();
        for (i, ((rec_id, start), real_end)) in rec_ids.iter().zip(starts).zip(reals).enumerate() {
            let (line_sum, unrec_count) = if let Some(real) = rec_id.as_real() {
                let rows = tx
                    .query(
                        "SELECT COALESCE(SUM(amount), 0), \
                                count(*) FILTER (WHERE NOT COALESCE(is_reconciled, false))::int4 \
                         FROM account_bank_statement_line WHERE statement_id = $1",
                        &[&real.get()],
                    )
                    .await?;
                if let Some(row) = rows.first() {
                    (
                        row.try_get::<_, Decimal>(0).map_err(db)?,
                        row.try_get::<_, i32>(1).map_err(db)?,
                    )
                } else {
                    (Decimal::ZERO, 0)
                }
            } else {
                (Decimal::ZERO, 0)
            };
            let computed_end = start + line_sum;
            let matches = computed_end == real_end;
            out.set(i, AccountBankStatement::BALANCE_END, computed_end)?;
            out.set(i, AccountBankStatement::IS_COMPLETE, matches)?;
            out.set(i, AccountBankStatement::UNRECONCILED_COUNT, unrec_count)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let mut today_cache: Option<Date> = None;
        for vals in &mut vals_list {
            if !this.env().su()
                && let Some(Cell::Str(st)) = vals.get("state")
                && st.as_ref() != "open"
            {
                return Err(Error::validation(
                    "Bank statements must start in Open status.",
                ));
            }
            if !vals.contains("date") {
                let d = match today_cache {
                    Some(d) => d,
                    None => {
                        let d = resolve_default_today(tx, this.env()).await?;
                        today_cache = Some(d);
                        d
                    }
                };
                vals.insert("date".into(), Cell::Date(d));
            }
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let date = match vals.get("date") {
                    Some(Cell::Date(d)) => *d,
                    _ => resolve_default_today(tx, this.env()).await?,
                };
                let year = date.ymd().0;
                // Use atomic PG serial sequence instead of non-atomic count(*) + 1.
                let next: i64 = tx
                    .query(
                        "SELECT nextval(pg_get_serial_sequence('account_bank_statement', 'id'))::int8",
                        &[],
                    )
                    .await?
                    .first()
                    .ok_or_else(|| Error::invariant("Missing nextval row for bank statement."))?
                    .try_get(0)
                    .map_err(db)?;
                vals.insert(
                    "name".into(),
                    Cell::Str(format!("BNKST/{year}/{next:05}").into()),
                );
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            if vals.contains("state") {
                return Err(Error::validation(
                    "Bank statement status must be changed through Post or Reopen actions.",
                ));
            }
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let posted = tx
                    .query(
                        "SELECT id::int8 FROM account_bank_statement \
                         WHERE id = ANY($1::int8[]) AND state = 'posted' LIMIT 1",
                        &[&ids],
                    )
                    .await?;
                if !posted.is_empty() {
                    return Err(Error::user(
                        "A posted bank statement cannot be modified; reopen it first.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let blocked = tx
                    .query(
                        "SELECT s.id::int8 FROM account_bank_statement s \
                         WHERE s.id = ANY($1::int8[]) \
                           AND (s.state = 'posted' OR EXISTS ( \
                                SELECT 1 FROM account_bank_statement_line l \
                                WHERE l.statement_id = s.id AND COALESCE(l.is_reconciled, false) = true \
                           )) \
                         LIMIT 1",
                        &[&ids],
                    )
                    .await?;
                if !blocked.is_empty() {
                    return Err(Error::user(
                        "Cannot delete a posted bank statement or one with reconciled transactions.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Parses pasted CSV rows (`YYYY-MM-DD,Label,Amount[,Ref]`) and appends them as statement lines.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_import_csv(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let stmt = this.ensure_one()?;
        stmt.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let sid = stmt.as_records().ids()[0];
        let srows = tx
            .query(
                "SELECT state::text, journal_id::int8, COALESCE(csv_import_data, '')::text \
                 FROM account_bank_statement WHERE id = $1 FOR UPDATE",
                &[&sid],
            )
            .await?;
        let Some(srow) = srows.first() else {
            return Err(Error::missing("Bank statement not found."));
        };
        let state: String = srow.try_get(0).map_err(db)?;
        if state != "open" {
            return Err(Error::user(
                "Only open statements can import new CSV transactions.",
            ));
        }
        let journal_id: Option<i64> = srow.try_get(1).map_err(db)?;
        let journal_id = journal_id
            .ok_or_else(|| Error::validation("Choose a journal for this bank statement."))?;
        let raw: String = srow.try_get(2).map_err(db)?;
        let parsed = parse_bank_statement_csv(&raw)?;
        let mut line_vals = Vec::with_capacity(parsed.len());
        for row in parsed {
            let mut v = RawVals::new();
            v.insert("statement_id".into(), Cell::Id(sid));
            v.insert("journal_id".into(), Cell::Id(journal_id));
            v.insert("date".into(), Cell::Date(row.date));
            v.insert("payment_ref".into(), Cell::Str(row.label.into()));
            v.insert("amount".into(), Cell::Decimal(row.amount));
            if let Some(r) = row.reference {
                v.insert("ref".into(), Cell::Str(r.into()));
            }
            line_vals.push(v);
        }
        strip_default_context(stmt.env())
            .records("account.bank.statement.line")?
            .create(tx, line_vals)
            .await?;
        let uid = stmt.env().uid();
        tx.execute(
            "UPDATE account_bank_statement SET csv_import_data = NULL, \
             write_uid = $2, write_date = now() AT TIME ZONE 'UTC' WHERE id = $1",
            &[&sid, &uid],
        )
        .await?;
        refresh_statement_totals(tx, sid).await?;
        tx.invalidate_records(
            stmt.as_records(),
            Some(&[
                "csv_import_data",
                "line_ids",
                "balance_end",
                "is_complete",
                "unreconciled_count",
            ]),
        )
        .await
    }

    /// Reconciles every open line in this statement that already has a matched invoice or
    /// counterpart account set.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_reconcile_all(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let stmt = this.ensure_one()?;
        stmt.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let sid = stmt.as_records().ids()[0];
        let srows = tx
            .query(
                "SELECT state::text FROM account_bank_statement WHERE id = $1 FOR UPDATE",
                &[&sid],
            )
            .await?;
        let Some(srow) = srows.first() else {
            return Err(Error::missing("Bank statement not found."));
        };
        if srow.try_get::<_, String>(0).map_err(db)? != "open" {
            return Err(Error::user(
                "Only open bank statements can reconcile transactions.",
            ));
        }
        let rows = tx
            .query(
                "SELECT id::int8 FROM account_bank_statement_line \
                 WHERE statement_id = $1 AND NOT COALESCE(is_reconciled, false) \
                 ORDER BY date, id",
                &[&sid],
            )
            .await?;
        if rows.is_empty() {
            return Err(Error::user(
                "All transactions on this statement are already reconciled.",
            ));
        }
        let ids: Vec<i64> = rows
            .iter()
            .map(|r| r.try_get::<_, i64>(0).map_err(db))
            .collect::<Result<_>>()?;
        let lines = stmt
            .env()
            .records("account.bank.statement.line")?
            .browse(ids)?;
        for line in lines.iter() {
            let _: () = line.call(tx, "action_reconcile_line", ()).await?;
        }
        refresh_statement_totals(tx, sid).await?;
        tx.invalidate_records(
            stmt.as_records(),
            Some(&["balance_end", "is_complete", "unreconciled_count"]),
        )
        .await
    }

    /// Validates that the statement balances (`balance_end == balance_end_real`) and all lines
    /// are reconciled, then posts/locks the statement.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_post_statement(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let stmt = this.ensure_one()?;
        stmt.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let sid = stmt.as_records().ids()[0];
        let rows = tx
            .query(
                "SELECT state::text FROM account_bank_statement WHERE id = $1 FOR UPDATE",
                &[&sid],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Bank statement not found."));
        };
        let state: String = row.try_get(0).map_err(db)?;
        if state != "open" {
            return Err(Error::user("Only open bank statements can be posted."));
        }
        refresh_statement_totals(tx, sid).await?;
        let rows = tx
            .query(
                "SELECT COALESCE(balance_end, 0), COALESCE(balance_end_real, 0), \
                        COALESCE(unreconciled_count, 0) \
                 FROM account_bank_statement WHERE id = $1",
                &[&sid],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::missing("Bank statement not found."))?;
        let end_computed: Decimal = row.try_get(0).map_err(db)?;
        let end_real: Decimal = row.try_get(1).map_err(db)?;
        let unrec: i32 = row.try_get(2).map_err(db)?;
        if end_computed != end_real {
            let diff = end_real - end_computed;
            return Err(Error::user(format!(
                "Ending balance ({end_real}) does not match computed balance ({end_computed}); \
                 difference is {diff}."
            )));
        }
        if unrec > 0 {
            return Err(Error::user(format!(
                "{unrec} statement line(s) are not yet reconciled; reconcile all transactions \
                 before posting the statement."
            )));
        }
        let uid = stmt.env().uid();
        let updated = tx
            .execute(
                "UPDATE account_bank_statement SET state = 'posted', \
                 write_uid = $2, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1 AND state = 'open'",
                &[&sid, &uid],
            )
            .await?;
        if updated != 1 {
            return Err(Error::user(
                "Bank statement state changed concurrently; reload and retry.",
            ));
        }
        tx.invalidate_records(stmt.as_records(), Some(&["state"]))
            .await
    }

    /// Reopens a posted statement for additional adjustments (managers only, honoring lock dates).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_reopen(
        this: &RecordSet<AccountBankStatement>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let stmt = this.ensure_one()?;
        stmt.as_records().check_access(tx, Operation::Write).await?;
        if !stmt.env().su()
            && !tx
                .has_group(stmt.env(), "account.group_account_manager")
                .await?
        {
            return Err(Error::access(
                "Only Accounting Managers can reopen a posted bank statement.",
            ));
        }
        tx.flush().await?;
        let sid = stmt.as_records().ids()[0];
        let rows = tx
            .query(
                "SELECT state::text, company_id::int8, date \
                 FROM account_bank_statement WHERE id = $1 FOR UPDATE",
                &[&sid],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::missing("Bank statement not found."));
        };
        let state: String = row.try_get(0).map_err(db)?;
        let company_id: i64 = row.try_get(1).map_err(db)?;
        let date: Date = row.try_get(2).map_err(db)?;
        if state != "posted" {
            return Err(Error::user("Only posted bank statements can be reopened."));
        }
        // Respect company hard / fiscal lock dates on statement date.
        let lock_rows = tx
            .query(
                "SELECT hard_lock_date, fiscalyear_lock_date FROM res_company WHERE id = $1",
                &[&company_id],
            )
            .await?;
        if let Some(lrow) = lock_rows.first() {
            let hard: Option<Date> = lrow.try_get(0).map_err(db)?;
            let fy: Option<Date> = lrow.try_get(1).map_err(db)?;
            if hard.is_some_and(|d| date <= d) || fy.is_some_and(|d| date <= d) {
                return Err(Error::validation(format!(
                    "Cannot reopen bank statement dated {date}: period is locked."
                )));
            }
        }
        let uid = stmt.env().uid();
        tx.execute(
            "UPDATE account_bank_statement SET state = 'open', \
             write_uid = $2, write_date = now() AT TIME ZONE 'UTC' \
             WHERE id = $1 AND state = 'posted'",
            &[&sid, &uid],
        )
        .await?;
        tx.invalidate_records(stmt.as_records(), Some(&["state"]))
            .await
    }
}

/// `account.bank.statement.line`: a single bank transaction that can be matched against an
/// open invoice/bill or booked to a counterpart account in one click.
#[model(
    name = "account.bank.statement.line",
    description = "Bank Statement Line",
    order = "date desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountBankStatementLine {
    /// Parent bank statement (optional for standalone bank feed transactions).
    #[field(
        many2one,
        comodel = "account.bank.statement",
        ondelete = "cascade",
        index = "btree_not_null",
        string = "Statement"
    )]
    pub statement_id: Option<RecId>,
    /// Bank, cash or credit-card journal.
    #[field(
        many2one,
        comodel = "account.journal",
        required = true,
        check_company = true,
        ondelete = "restrict",
        domain = [("type", "in", ["bank", "cash", "credit"])],
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// Owning company from the journal.
    #[field(
        many2one,
        comodel = "res.company",
        related = "journal_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency from the journal or its company.
    #[field(
        many2one,
        comodel = "res.currency",
        compute = "_compute_currency",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Transaction date.
    #[field(date, required = true, string = "Date")]
    pub date: Option<Date>,
    /// Bank statement description / label.
    #[field(char, required = true, string = "Label")]
    pub payment_ref: Option<Arc<str>>,
    /// Optional bank reference.
    #[field(char, string = "Reference")]
    pub r#ref: Option<Arc<str>>,
    /// Optional customer or vendor partner.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        ondelete = "restrict",
        string = "Partner"
    )]
    pub partner_id: Option<RecId>,
    /// Transaction amount: positive = inflow (deposit), negative = outflow (withdrawal).
    #[field(monetary, currency_field = "currency_id", string = "Amount")]
    pub amount: Decimal,
    /// Open invoice or vendor bill to match and settle on reconciliation.
    #[field(
        many2one,
        comodel = "account.move",
        check_company = true,
        ondelete = "set null",
        domain = [
            ("state", "=", "posted"),
            ("payment_state", "in", ["not_paid", "partial"]),
            ("move_type", "in", ["out_invoice", "out_refund", "in_invoice", "in_refund"]),
        ],
        string = "Match Invoice/Bill"
    )]
    pub matched_move_id: Option<RecId>,
    /// Counterpart account (e.g. Bank Charges, Revenue, Suspense) when not matching an invoice.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Counterpart Account"
    )]
    pub counterpart_account_id: Option<RecId>,
    /// Posted journal entry created when this line was reconciled.
    #[field(
        many2one,
        comodel = "account.move",
        readonly = true,
        copy = false,
        ondelete = "set null",
        string = "Journal Entry"
    )]
    pub move_id: Option<RecId>,
    /// True once reconciled into a posted journal entry.
    #[field(
        boolean,
        readonly = true,
        copy = false,
        default = false,
        string = "Reconciled"
    )]
    pub is_reconciled: bool,
}

#[methods(AccountBankStatementLine, addon = "account")]
impl AccountBankStatementLine {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountBankStatementLine>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let env = this.env();
        if names.iter().any(|n| n == "date") && !values.contains_key("date") {
            let today = resolve_default_today(tx, env).await?;
            values.insert("date".to_owned(), Value::Date(today));
        }
        if let Some(cid) = env.company_id()?
            && names.iter().any(|n| n == "journal_id")
            && !values.contains_key("journal_id")
        {
            let jrows = tx
                .query(
                    "SELECT id::int8 FROM account_journal \
                     WHERE company_id = $1 AND type IN ('bank', 'cash') AND active = true \
                     ORDER BY CASE type WHEN 'bank' THEN 0 ELSE 1 END, sequence, id LIMIT 1",
                    &[&cid],
                )
                .await?;
            if let Some(row) = jrows.first() {
                let jid: i64 = row.try_get(0).map_err(db)?;
                values.insert("journal_id".to_owned(), Value::Int(jid));
            }
        }
        Ok(values)
    }

    #[compute(candidate, depends = ["journal_id"])]
    async fn _compute_currency(
        this: &RecordSet<AccountBankStatementLine>,
        tx: &mut ReadTx<'_>,
        out: &mut Assign<'_, AccountBankStatementLine>,
    ) -> Result<()> {
        let journals = tx
            .mapped(this, AccountBankStatementLine::JOURNAL_ID)
            .await?;
        for (i, journal) in journals.into_iter().enumerate() {
            let mut cur = None;
            if let Some(jid) = journal {
                let rows = tx
                    .query(
                        "SELECT COALESCE(j.currency_id, c.currency_id)::int8 \
                         FROM account_journal j JOIN res_company c ON c.id = j.company_id \
                         WHERE j.id = $1",
                        &[&jid.get()],
                    )
                    .await?;
                if let Some(row) = rows.first() {
                    let cid: Option<i64> = row.try_get(0).map_err(db)?;
                    cur = cid.and_then(RecId::from_real);
                }
            }
            out.set(i, AccountBankStatementLine::CURRENCY_ID, cur)?;
        }
        Ok(())
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<AccountBankStatementLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let mut stmt_ids = Vec::new();
        let mut today_cache: Option<Date> = None;
        for vals in &mut vals_list {
            if !this.env().su() && (vals.contains("is_reconciled") || vals.contains("move_id")) {
                return Err(Error::validation(
                    "Bank statement line reconciliation state is managed by the Reconcile action.",
                ));
            }
            if !vals.contains("date") {
                let d = match today_cache {
                    Some(d) => d,
                    None => {
                        let d = resolve_default_today(tx, this.env()).await?;
                        today_cache = Some(d);
                        d
                    }
                };
                vals.insert("date".into(), Cell::Date(d));
            }
            if let Some(Cell::Id(sid)) = vals.get("statement_id") {
                stmt_ids.push(*sid);
                let rows = tx
                    .query(
                        "SELECT journal_id::int8, state::text FROM account_bank_statement WHERE id = $1",
                        &[sid],
                    )
                    .await?;
                if let Some(row) = rows.first() {
                    let jid: i64 = row.try_get(0).map_err(db)?;
                    let st: String = row.try_get(1).map_err(db)?;
                    if !this.env().su() && st == "posted" {
                        return Err(Error::user(
                            "Cannot add transactions to a posted bank statement.",
                        ));
                    }
                    if !vals.contains("journal_id") {
                        vals.insert("journal_id".into(), Cell::Id(jid));
                    }
                }
            }
        }
        let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
        stmt_ids.sort_unstable();
        stmt_ids.dedup();
        for sid in stmt_ids {
            refresh_statement_totals(tx, sid).await?;
        }
        Ok(created)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<AccountBankStatementLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            if vals.contains("is_reconciled") || vals.contains("move_id") {
                return Err(Error::validation(
                    "Bank statement line reconciliation state is managed by the Reconcile action.",
                ));
            }
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let blocked = tx
                    .query(
                        "SELECT l.id::int8 FROM account_bank_statement_line l \
                         LEFT JOIN account_bank_statement s ON s.id = l.statement_id \
                         WHERE l.id = ANY($1::int8[]) \
                           AND (COALESCE(l.is_reconciled, false) = true OR COALESCE(s.state, 'open') = 'posted') \
                         LIMIT 1",
                        &[&ids],
                    )
                    .await?;
                if !blocked.is_empty() {
                    return Err(Error::user(
                        "Cannot modify a reconciled bank transaction or a line on a posted statement.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), (vals,)).await
    }

    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountBankStatementLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        if !this.env().su() {
            let ids: Vec<i64> = this.as_records().ids().to_vec();
            if !ids.is_empty() {
                tx.flush().await?;
                let blocked = tx
                    .query(
                        "SELECT l.id::int8 FROM account_bank_statement_line l \
                         LEFT JOIN account_bank_statement s ON s.id = l.statement_id \
                         WHERE l.id = ANY($1::int8[]) \
                           AND (COALESCE(l.is_reconciled, false) = true OR COALESCE(s.state, 'open') = 'posted') \
                         LIMIT 1",
                        &[&ids],
                    )
                    .await?;
                if !blocked.is_empty() {
                    return Err(Error::user(
                        "Cannot delete a reconciled bank transaction or a line on a posted statement.",
                    ));
                }
            }
        }
        sup.call(tx, this.as_records(), ()).await
    }

    /// Reconciles one statement line: creates & posts a balanced bank journal entry, and when
    /// `matched_move_id` is chosen, reconciles its counterpart item against the invoice/bill's
    /// open receivable/payable item so the invoice becomes Paid or Partially Paid.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_reconcile_line(
        this: &RecordSet<AccountBankStatementLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let line = this.ensure_one()?;
        line.as_records().check_access(tx, Operation::Write).await?;
        tx.flush().await?;
        let lid = line.as_records().ids()[0];
        let base_env = strip_default_context(line.env());

        tx.savepoint(async |tx| {
            let rows = tx
                .query(
                    "SELECT l.statement_id::int8, l.journal_id::int8, j.company_id::int8, \
                            j.default_account_id::int8, COALESCE(j.currency_id, c.currency_id)::int8, \
                            c.currency_id::int8, l.date, l.payment_ref::text, l.partner_id::int8, \
                            COALESCE(l.amount, 0), l.matched_move_id::int8, \
                            l.counterpart_account_id::int8, COALESCE(l.is_reconciled, false) \
                     FROM account_bank_statement_line l \
                     JOIN account_journal j ON j.id = l.journal_id \
                     JOIN res_company c ON c.id = j.company_id \
                     WHERE l.id = $1 FOR UPDATE OF l",
                    &[&lid],
                )
                .await?;
            let Some(row) = rows.first() else {
                return Err(Error::missing("Bank statement line not found."));
            };
            let statement_id: Option<i64> = row.try_get(0).map_err(db)?;
            let journal_id: i64 = row.try_get(1).map_err(db)?;
            let company_id: i64 = row.try_get(2).map_err(db)?;
            let bank_account_id: Option<i64> = row.try_get(3).map_err(db)?;
            let currency_id: i64 = row.try_get(4).map_err(db)?;
            let company_currency_id: i64 = row.try_get(5).map_err(db)?;
            let date: Date = row.try_get(6).map_err(db)?;
            let label: String = row.try_get(7).map_err(db)?;
            let mut partner_id: Option<i64> = row.try_get(8).map_err(db)?;
            let amount: Decimal = row.try_get(9).map_err(db)?;
            let matched_move_id: Option<i64> = row.try_get(10).map_err(db)?;
            let counterpart_account_opt: Option<i64> = row.try_get(11).map_err(db)?;
            let is_reconciled: bool = row.try_get(12).map_err(db)?;

            if is_reconciled {
                return Err(Error::user(
                    "This bank statement line is already reconciled.",
                ));
            }
            if amount.is_zero() {
                return Err(Error::user(
                    "A bank statement line with a zero amount cannot be reconciled.",
                ));
            }
            let bank_account = bank_account_id
                .ok_or_else(|| Error::user("The bank journal has no default account configured."))?;
            let env = base_env.with_company(company_id)?;

            // Convert journal currency amount to company currency via RateBook/RoundingTable.
            let company_amount = if currency_id == company_currency_id {
                amount
            } else {
                let from_rec = RecId::from_real(currency_id)
                    .ok_or_else(|| Error::invariant("Invalid bank journal currency id."))?;
                let to_rec = RecId::from_real(company_currency_id)
                    .ok_or_else(|| Error::invariant("Invalid company currency id."))?;
                let co_rec = RecId::from_real(company_id)
                    .ok_or_else(|| Error::invariant("Invalid company id."))?;
                let currencies = [from_rec, to_rec];
                let rounding = RoundingTable::load(&env, tx, &currencies).await?;
                let book = RateBook::load(
                    &env,
                    tx,
                    co_rec,
                    date,
                    RateDateRule::OnOrBefore,
                    &currencies,
                )
                .await?;
                book.snapshot(from_rec, to_rec)?
                    .convert_exact(amount)?
                    .quantize(rounding.get(to_rec)?.increment(), CURRENCY_ROUNDING_MODE)?
            };
            let (bank_bal, counterpart_bal) = compute_statement_entry_balances(company_amount);
            let (bank_cur, counterpart_cur) = compute_statement_entry_balances(amount);

            let mut target_open_line_ids: Vec<i64> = Vec::new();
            let counterpart_account = if let Some(mid) = matched_move_id {
                let mrows = tx
                    .query(
                        "SELECT l.id::int8, l.account_id::int8, m.partner_id::int8, \
                                m.state::text, m.company_id::int8, l.currency_id::int8, \
                                COALESCE(o.amount_residual, l.balance) \
                         FROM account_move_line l \
                         JOIN account_move m ON m.id = l.move_id \
                         LEFT JOIN account_open_item o ON o.line_id = l.id \
                         WHERE l.move_id = $1 AND l.display_type = 'payment_term' \
                           AND l.full_reconcile_id IS NULL \
                           AND COALESCE(o.amount_residual, l.balance) <> 0 \
                         ORDER BY l.id",
                        &[&mid],
                    )
                    .await?;
                if mrows.is_empty() {
                    return Err(Error::user(
                        "The selected invoice or bill has no open receivable/payable items left to reconcile.",
                    ));
                }
                let mstate: String = mrows[0].try_get(3).map_err(db)?;
                let mcompany: i64 = mrows[0].try_get(4).map_err(db)?;
                if mstate != "posted" {
                    return Err(Error::user(
                        "Only posted invoices or bills can be matched on a bank statement line.",
                    ));
                }
                if mcompany != company_id {
                    return Err(Error::user(
                        "The matched invoice or bill belongs to another company.",
                    ));
                }
                let first_acc: i64 = mrows[0].try_get(1).map_err(db)?;
                if partner_id.is_none() {
                    partner_id = mrows[0].try_get(2).map_err(db)?;
                }
                let mut has_opposite_sign = false;
                for mr in &mrows {
                    let lcur: i64 = mr.try_get(5).map_err(db)?;
                    let lres: Decimal = mr.try_get(6).map_err(db)?;
                    if lcur != currency_id
                        && lcur != company_currency_id
                        && currency_id != company_currency_id
                    {
                        return Err(Error::user(
                            "The matched invoice/bill currency does not match this bank journal currency.",
                        ));
                    }
                    if (counterpart_bal > Decimal::ZERO && lres < Decimal::ZERO)
                        || (counterpart_bal < Decimal::ZERO && lres > Decimal::ZERO)
                    {
                        has_opposite_sign = true;
                    }
                    target_open_line_ids.push(mr.try_get(0).map_err(db)?);
                }
                if !has_opposite_sign {
                    return Err(Error::user(
                        "The bank statement line sign does not oppose the matched invoice/bill balance (check inflow vs outflow).",
                    ));
                }
                first_acc
            } else if let Some(acc) = counterpart_account_opt {
                acc
            } else {
                return Err(Error::user(
                    "Choose either a 'Match Invoice/Bill' or a 'Counterpart Account' before reconciling.",
                ));
            };

            let mut mvals = RawVals::new();
            mvals.insert("move_type".into(), Cell::Str("entry".into()));
            mvals.insert("journal_id".into(), Cell::Id(journal_id));
            mvals.insert("company_id".into(), Cell::Id(company_id));
            mvals.insert("currency_id".into(), Cell::Id(currency_id));
            mvals.insert("date".into(), Cell::Date(date));
            mvals.insert("ref".into(), Cell::Str(label.as_str().into()));
            if let Some(pid) = partner_id {
                mvals.insert("partner_id".into(), Cell::Id(pid));
            }

            let unchecked_moves = crate::moves::unchecked(&env.records("account.move")?);
            let created_move = unchecked_moves.create(tx, vec![mvals]).await?;
            let move_id = created_move.ids()[0];

            let mut bank_line = RawVals::new();
            bank_line.insert("move_id".into(), Cell::Id(move_id));
            bank_line.insert("account_id".into(), Cell::Id(bank_account));
            bank_line.insert("name".into(), Cell::Str(label.as_str().into()));
            bank_line.insert("currency_id".into(), Cell::Id(currency_id));
            bank_line.insert("balance".into(), Cell::Decimal(bank_bal));
            bank_line.insert("amount_currency".into(), Cell::Decimal(bank_cur));
            if let Some(pid) = partner_id {
                bank_line.insert("partner_id".into(), Cell::Id(pid));
            }

            let mut cp_line = RawVals::new();
            cp_line.insert("move_id".into(), Cell::Id(move_id));
            cp_line.insert("account_id".into(), Cell::Id(counterpart_account));
            cp_line.insert("name".into(), Cell::Str(label.as_str().into()));
            cp_line.insert("currency_id".into(), Cell::Id(currency_id));
            cp_line.insert("balance".into(), Cell::Decimal(counterpart_bal));
            cp_line.insert("amount_currency".into(), Cell::Decimal(counterpart_cur));
            if let Some(pid) = partner_id {
                cp_line.insert("partner_id".into(), Cell::Id(pid));
            }

            let unchecked_lines = crate::moves::unchecked(&env.records("account.move.line")?);
            let created_lines = unchecked_lines.create(tx, vec![bank_line, cp_line]).await?;
            let cp_line_id = created_lines.ids()[1];

            let move_rec = env.records("account.move")?.browse([move_id])?;
            post_moves(tx, &move_rec).await?;

            if !target_open_line_ids.is_empty() {
                target_open_line_ids.push(cp_line_id);
                target_open_line_ids.sort_unstable();
                target_open_line_ids.dedup();
                let to_rec = env
                    .records("account.move.line")?
                    .browse(target_open_line_ids)?;
                reconcile_lines(tx, &to_rec).await?;
            }

            let uid = line.env().uid();
            tx.execute(
                "UPDATE account_bank_statement_line \
                 SET move_id = $2, is_reconciled = true, counterpart_account_id = $3, \
                     partner_id = COALESCE(partner_id, $4), write_uid = $5, \
                     write_date = now() AT TIME ZONE 'UTC' \
                 WHERE id = $1",
                &[&lid, &move_id, &counterpart_account, &partner_id, &uid],
            )
            .await?;
            tx.invalidate_records(
                line.as_records(),
                Some(&[
                    "move_id",
                    "is_reconciled",
                    "counterpart_account_id",
                    "partner_id",
                ]),
            )
            .await?;
            if let Some(sid) = statement_id {
                refresh_statement_totals(tx, sid).await?;
            }
            Ok(())
        })
        .await
    }
}

async fn refresh_statement_totals(tx: &mut Transaction, statement_id: i64) -> Result<()> {
    tx.execute(
        "UPDATE account_bank_statement s \
         SET balance_end = COALESCE(s.balance_start, 0) + COALESCE(t.total, 0), \
             is_complete = (COALESCE(s.balance_start, 0) + COALESCE(t.total, 0) = COALESCE(s.balance_end_real, 0)), \
             unreconciled_count = COALESCE(t.unrec, 0), \
             write_date = now() AT TIME ZONE 'UTC' \
         FROM (SELECT COALESCE(SUM(amount), 0) AS total, \
                      count(*) FILTER (WHERE NOT COALESCE(is_reconciled, false))::int4 AS unrec \
               FROM account_bank_statement_line WHERE statement_id = $1) t \
         WHERE s.id = $1",
        &[&statement_id],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{compute_statement_entry_balances, parse_bank_statement_csv};
    use erp_sdk::{Date, Decimal};
    use std::str::FromStr;

    #[test]
    fn csv_parser_parses_exact_decimals_and_skips_header() {
        let csv = "date,label,amount,ref\n\
                   2026-11-20,Customer Wire Transfer,1250.75,WIRE-01\n\
                   2026-11-21,Monthly Bank Fee,-15.50,\n";
        let rows = parse_bank_statement_csv(csv).expect("valid csv");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].date, Date::from_ymd(2026, 11, 20).unwrap());
        assert_eq!(rows[0].amount, Decimal::from_str("1250.75").unwrap());
        assert_eq!(rows[0].reference.as_deref(), Some("WIRE-01"));
        assert_eq!(rows[1].amount, Decimal::from_str("-15.50").unwrap());
        assert_eq!(rows[1].reference, None);
    }

    #[test]
    fn statement_entry_balances_sum_to_exact_zero() {
        let inflow = Decimal::from_str("500.25").unwrap();
        let (b1, c1) = compute_statement_entry_balances(inflow);
        assert_eq!(b1, inflow);
        assert_eq!(c1, -inflow);
        assert_eq!(b1 + c1, Decimal::ZERO);

        let outflow = Decimal::from_str("-42.10").unwrap();
        let (b2, c2) = compute_statement_entry_balances(outflow);
        assert_eq!(b2, outflow);
        assert_eq!(c2, -outflow);
        assert_eq!(b2 + c2, Decimal::ZERO);
    }
}
