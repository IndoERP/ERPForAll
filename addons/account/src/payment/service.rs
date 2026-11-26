// SPDX-License-Identifier: MIT OR Apache-2.0
//! The payment service: defaults of a draft, posting (entry built and posted through the posting
//! service), cancel, reset to draft, validation by hand, and the one transition function of a
//! payment's status and flags ([`refresh_payments`]). Each action asks the caller's write right on
//! the payments before any statement reads or locks them, and takes the payment rows `FOR UPDATE`
//! in id order.

use std::collections::BTreeMap;

use erp_sdk::Operation;
use erp_sdk::prelude::*;

use crate::moves::{POSTER_GROUP, post_moves};

fn db(error: impl std::fmt::Display) -> Error {
    Error::db(format!("A payment could not be read: {error}"))
}

fn sorted(records: &Records) -> Vec<i64> {
    let mut ids = records.ids();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Fills a new draft's date (today), company (the journal's), currency (the journal's, else the
/// company's) and method line (the journal's first line of the direction).
pub(super) async fn fill_defaults(
    tx: &mut Transaction,
    env: &Env,
    vals: &mut RawVals,
) -> Result<()> {
    let journal = match vals.get("journal_id") {
        Some(Cell::Id(id)) => *id,
        Some(Cell::Int(id)) => i64::from(*id),
        _ => return Ok(()), // the required check names the missing journal
    };
    let journal_records = env.records("account.journal")?.browse([journal])?;
    let company = crate::moves::id_cell(
        Some(&journal_records.cell(tx, "company_id").await?),
        "company_id",
    )?;
    if !vals.contains("company_id")
        && let Some(company) = company
    {
        vals.insert("company_id".into(), Cell::Id(company));
    }
    if !vals.contains("date") {
        let company =
            crate::moves::id_cell(vals.get("company_id"), "company_id")?.ok_or_else(|| {
                Error::validation("A payment needs a company before its date can be set.")
            })?;
        let days = erp_addon_base::company_calendar::business_days(tx, env, &[company]).await?;
        let day = days
            .get(&company)
            .ok_or_else(|| Error::missing(format!("Company {company} is unavailable.")))?;
        vals.insert("date".into(), Cell::Date(day.date));
    }
    if !vals.contains("currency_id") {
        let currency = match crate::moves::id_cell(
            Some(&journal_records.cell(tx, "currency_id").await?),
            "currency_id",
        )? {
            Some(currency) => Some(currency),
            None => match company {
                Some(company) => crate::moves::id_cell(
                    Some(
                        &env.records("res.company")?
                            .browse([company])?
                            .cell(tx, "currency_id")
                            .await?,
                    ),
                    "currency_id",
                )?,
                None => None,
            },
        };
        if let Some(currency) = currency {
            vals.insert("currency_id".into(), Cell::Id(currency));
        }
    }
    if !vals.contains("payment_method_line_id") {
        let direction = match vals.get("payment_type") {
            Some(Cell::Str(kind)) => kind.to_string(),
            _ => "inbound".to_owned(),
        };
        let lines = env
            .records("account.payment.method.line")?
            .search(
                tx,
                &Domain::and([
                    Domain::condition("journal_id", Operator::Eq, journal)?,
                    Domain::condition("payment_type", Operator::Eq, direction.as_str())?,
                ]),
            )
            .await?;
        if let Some(first) = lines.ids().first() {
            vals.insert("payment_method_line_id".into(), Cell::Id(*first));
        }
    }
    Ok(())
}

/// Refuses `what` on the payments of `ids` that have a journal entry, naming the first.
pub(super) async fn refuse_posted(tx: &mut Transaction, ids: &[i64], what: &str) -> Result<()> {
    let rows = tx
        .query(
            "SELECT COALESCE(name, id::text) FROM account_payment \
             WHERE id = ANY($1::int8[]) AND move_id IS NOT NULL ORDER BY id LIMIT 1",
            &[&ids],
        )
        .await?;
    if let Some(row) = rows.first() {
        let name: String = row.try_get(0).map_err(db)?;
        return Err(Error::user(format!(
            "Payment {name} is confirmed: {what} is not possible any more. A confirmed payment is \
             corrected by reversing its journal entry, which is not available in this version."
        )));
    }
    Ok(())
}

/// Refuses a change of `what` on the cancelled or rejected payments of `ids`, naming the first:
/// such a payment is reset to draft before it is changed. The state itself moves only through
/// the payment's actions, so Reset to Draft is not refused here.
pub(super) async fn refuse_ended(tx: &mut Transaction, ids: &[i64], what: &str) -> Result<()> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT COALESCE(name, 'Draft Payment'), state::text FROM account_payment \
             WHERE id = ANY($1::int8[]) AND state IN ('canceled', 'rejected') ORDER BY id LIMIT 1",
            &[&ids],
        )
        .await?;
    if let Some(row) = rows.first() {
        let name: String = row.try_get(0).map_err(db)?;
        let state: String = row.try_get(1).map_err(db)?;
        let state = if state == "canceled" {
            "cancelled"
        } else {
            "rejected"
        };
        return Err(Error::user(format!(
            "Payment {name} is {state}: {what} cannot be changed. Reset it to draft first."
        )));
    }
    Ok(())
}

/// Refuses changing `batch_payment_id` on reconciled, cancelled or rejected payments, and refuses
/// attaching to or detaching from a batch payment that has already been sent or reconciled.
pub(super) async fn refuse_batch_change(
    tx: &mut Transaction,
    ids: &[i64],
    new_batch: Option<&Cell>,
) -> Result<()> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT COALESCE(p.name, 'Draft Payment'), p.state::text, \
                    b.name::text, b.state::text \
             FROM account_payment p \
             LEFT JOIN account_batch_payment b ON b.id = p.batch_payment_id \
             WHERE p.id = ANY($1::int8[]) \
             ORDER BY p.id",
            &[&ids],
        )
        .await?;
    for row in &rows {
        let name: String = row.try_get(0).map_err(db)?;
        let state: String = row.try_get(1).map_err(db)?;
        if matches!(state.as_str(), "reconciled" | "canceled" | "rejected") {
            let label = if state == "canceled" {
                "cancelled"
            } else {
                state.as_str()
            };
            return Err(Error::user(format!(
                "Payment {name} is {label}: batch_payment_id cannot be changed."
            )));
        }
        let existing_batch_name: Option<String> = row.try_get(2).map_err(db)?;
        let existing_batch_state: Option<String> = row.try_get(3).map_err(db)?;
        if matches!(existing_batch_state.as_deref(), Some("sent" | "reconciled")) {
            let bname = existing_batch_name.as_deref().unwrap_or("Batch");
            return Err(Error::user(format!(
                "Payment {name} belongs to finalized batch payment {bname} and cannot be reassigned."
            )));
        }
    }
    if let Some(Cell::Id(target_id)) = new_batch {
        let target = tx
            .query(
                "SELECT name::text, state::text FROM account_batch_payment WHERE id = $1",
                &[target_id],
            )
            .await?;
        if let Some(row) = target.first() {
            let bname: String = row.try_get(0).map_err(db)?;
            let bstate: String = row.try_get(1).map_err(db)?;
            if matches!(bstate.as_str(), "sent" | "reconciled") {
                return Err(Error::user(format!(
                    "Batch payment {bname} is {bstate}: payments can only be added to a draft batch."
                )));
            }
        }
    }
    Ok(())
}

/// The memo of the posted payments of `ids` becomes their entries' reference (the entry write
/// keeps its own rights and rules).
pub(super) async fn memo_to_entries(tx: &mut Transaction, env: &Env, ids: &[i64]) -> Result<()> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT move_id::int8, memo::text FROM account_payment \
             WHERE id = ANY($1::int8[]) AND move_id IS NOT NULL ORDER BY id",
            &[&ids],
        )
        .await?;
    for row in &rows {
        let entry: i64 = row.try_get(0).map_err(db)?;
        let memo: Option<String> = row.try_get(1).map_err(db)?;
        let cell = memo.map_or(Cell::Null, |m| Cell::Str(m.into()));
        env.records("account.move")?
            .browse([entry])?
            .write(tx, RawVals::new().set_cell("ref", cell))
            .await?;
    }
    Ok(())
}

/// What posting reads of one payment, after its lock.
struct Draft {
    id: i64,
    date: Date,
    journal: i64,
    company: i64,
    root_transfer: Option<i64>,
    currency: i64,
    company_currency: i64,
    amount: Decimal,
    inbound: bool,
    customer: bool,
    partner: Option<i64>,
    memo: Option<String>,
    line: Option<i64>,
    line_journal: Option<i64>,
    line_type: Option<String>,
    line_name: Option<String>,
    line_account: Option<i64>,
    destination: Option<i64>,
}

async fn authorize(tx: &mut Transaction, payments: &Records) -> Result<()> {
    let env = payments.env().clone();
    if !env.su() && !tx.has_group(&env, POSTER_GROUP).await? {
        return Err(Error::access(format!(
            "Only accounting users may confirm, cancel or reset payments ({POSTER_GROUP})."
        )));
    }
    payments.check_access(tx, Operation::Write).await
}

/// Locks the payments of `ids` in id order and returns their states.
async fn lock(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<(i64, String, String)>> {
    let rows = tx
        .query(
            "SELECT id::int8, state::text, COALESCE(name, 'Draft Payment') FROM account_payment \
             WHERE id = ANY($1::int8[]) ORDER BY id FOR UPDATE",
            &[&ids],
        )
        .await?;
    if rows.len() != ids.len() {
        return Err(Error::missing("Some of the payments no longer exist."));
    }
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get(0).map_err(db)?,
                row.try_get(1).map_err(db)?,
                row.try_get(2).map_err(db)?,
            ))
        })
        .collect()
}

async fn read_drafts(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<Draft>> {
    let rows = tx
        .query(
            "SELECT p.id::int8, p.date, p.journal_id::int8, \
             p.company_id::int8, root.transfer_account_id::int8, p.currency_id::int8, \
             co.currency_id::int8, COALESCE(p.amount, 0), p.payment_type = 'inbound', \
             p.partner_type = 'customer', COALESCE(pa.commercial_partner_id, pa.id)::int8, \
             p.memo::text, p.payment_method_line_id::int8, ml.journal_id::int8, \
             pm.payment_type::text, ml.name::text, ml.payment_account_id::int8, \
             p.destination_account_id::int8 \
             FROM account_payment p \
             JOIN account_journal j ON j.id = p.journal_id \
             JOIN res_company co ON co.id = p.company_id \
             JOIN res_company root ON root.id = \
                  COALESCE(NULLIF(split_part(co.parent_path, '/', 1), '')::int8, co.id) \
             LEFT JOIN res_partner pa ON pa.id = p.partner_id \
             LEFT JOIN account_payment_method_line ml ON ml.id = p.payment_method_line_id \
             LEFT JOIN account_payment_method pm ON pm.id = ml.payment_method_id \
             WHERE p.id = ANY($1::int8[]) ORDER BY p.id",
            &[&ids],
        )
        .await?;
    let mut drafts = Vec::with_capacity(rows.len());
    for row in &rows {
        drafts.push(Draft {
            id: row.try_get(0).map_err(db)?,
            date: row.try_get(1).map_err(db)?,
            journal: row.try_get(2).map_err(db)?,
            company: row.try_get(3).map_err(db)?,
            root_transfer: row.try_get(4).map_err(db)?,
            currency: row.try_get(5).map_err(db)?,
            company_currency: row.try_get(6).map_err(db)?,
            amount: row.try_get(7).map_err(db)?,
            inbound: row.try_get(8).map_err(db)?,
            customer: row.try_get(9).map_err(db)?,
            partner: row.try_get(10).map_err(db)?,
            memo: row.try_get(11).map_err(db)?,
            line: row.try_get(12).map_err(db)?,
            line_journal: row.try_get(13).map_err(db)?,
            line_type: row.try_get(14).map_err(db)?,
            line_name: row.try_get(15).map_err(db)?,
            line_account: row.try_get(16).map_err(db)?,
            destination: row.try_get(17).map_err(db)?,
        });
    }
    Ok(drafts)
}

/// Posts the draft payments of `payments` as one unit:
/// each gets an entry in its journal with a liquidity item on the outstanding account (the method
/// line's, else the root company's transfer account) and a counterpart item on the destination
/// account (given, else the company's receivable or payable account), labelled with the method
/// line and the memo; the entries are posted by [`post_moves`] in the journal's payment chain.
/// Payments already posted are left as they are; cancelled and rejected ones are refused, and so
/// is a draft that already has an entry.
///
/// # Errors
///
/// `Access` without [`POSTER_GROUP`] or write access; `UserError` naming the payment for a missing
/// or foreign method line, a missing outstanding or destination account, a cancelled or rejected
/// payment; the posting refusals.
pub async fn post_payments(tx: &mut Transaction, payments: &Records) -> Result<()> {
    if payments.is_empty() {
        return Ok(());
    }
    authorize(tx, payments).await?;
    let ids = sorted(payments);
    let env = payments.env().clone();
    tx.savepoint(async |tx| {
        tx.flush().await?;
        let states = lock(tx, &ids).await?;
        for (_, state, name) in &states {
            if matches!(state.as_str(), "canceled" | "rejected") {
                return Err(Error::user(format!(
                    "Payment {name} is cancelled or rejected; reset it to draft before confirming it."
                )));
            }
        }
        let todo: Vec<i64> = states
            .iter()
            .filter(|(_, state, _)| state == "draft")
            .map(|(id, _, _)| *id)
            .collect();
        if todo.is_empty() {
            return Ok(());
        }
        // a draft never has an entry: one that has is never booked a second time
        refuse_posted(tx, &todo, "confirming it again").await?;
        let drafts = read_drafts(tx, &todo).await?;
        let mut entries = Vec::with_capacity(drafts.len());
        let mut accounts = Vec::with_capacity(drafts.len());
        for draft in &drafts {
            let (vals, outstanding, destination) = entry_vals(tx, &env, draft).await?;
            entries.push(vals);
            accounts.push((outstanding, destination));
        }
        let created = env.records("account.move")?.create(tx, entries).await?;
        tx.flush().await?;
        let moves = created.ids();
        if moves.len() != drafts.len() {
            return Err(Error::invariant(
                "the payment entries created do not match the payments",
            ));
        }
        let pays: Vec<i64> = drafts.iter().map(|d| d.id).collect();
        tx.execute(
            "UPDATE account_move m SET origin_payment_id = v.payment \
             FROM unnest($1::int8[], $2::int8[]) AS v(id, payment) WHERE m.id = v.id",
            &[&moves, &pays],
        )
        .await?;
        tx.invalidate_model("account.move", Some(&["origin_payment_id"]))
            .await?;
        post_moves(tx, &created).await?;
        tx.flush().await?;
        let outstanding: Vec<i64> = accounts.iter().map(|a| a.0).collect();
        let destination: Vec<i64> = accounts.iter().map(|a| a.1).collect();
        tx.execute(
            "UPDATE account_payment p SET move_id = v.move, name = m.name, state = 'paid', \
             outstanding_account_id = v.outstanding, destination_account_id = v.destination \
             FROM unnest($1::int8[], $2::int8[], $3::int8[], $4::int8[]) \
                  AS v(id, move, outstanding, destination) \
             JOIN account_move m ON m.id = v.move WHERE p.id = v.id",
            &[&pays, &moves, &outstanding, &destination],
        )
        .await?;
        tx.track_external(
            payments,
            "state",
            pays.iter().map(|id| (*id, text("draft"), text("paid"))),
        )?;
        refresh_payments(tx, &moves).await?;
        invalidate(tx).await
    })
    .await
}

fn text(value: &str) -> Cell {
    Cell::Str(value.into())
}

/// The entry of one draft payment, with its outstanding and destination accounts.
async fn entry_vals(tx: &mut Transaction, env: &Env, d: &Draft) -> Result<(RawVals, i64, i64)> {
    // the journal's name as the caller reads it (language and fallback of the ORM)
    let journal = env
        .records("account.journal")?
        .browse([d.journal])?
        .cell(tx, "display_name")
        .await?;
    let journal = match journal {
        Cell::Str(name) => name.to_string(),
        _ => format!("journal {}", d.journal),
    };
    let label = format!("payment of {} on {journal}", d.date);
    let direction = if d.inbound { "inbound" } else { "outbound" };
    if d.line.is_none() {
        return Err(Error::user(format!(
            "The {label} has no payment method: choose one of the journal's {direction} methods."
        )));
    }
    if d.line_journal != Some(d.journal) || d.line_type.as_deref() != Some(direction) {
        return Err(Error::user(format!(
            "The payment method of the {label} is not one of the journal's {direction} methods; \
             choose the method again."
        )));
    }
    let Some(outstanding) = d.line_account.or(d.root_transfer) else {
        return Err(Error::user(format!(
            "No outstanding account could be found to make the {label}: set one on the journal's \
             payment method, or a transfer account on the company."
        )));
    };
    let destination = match d.destination {
        Some(account) => account,
        None => {
            let wanted = if d.customer {
                "asset_receivable"
            } else {
                "liability_payable"
            };
            crate::invoice::partner_account(tx, env, d.company, wanted).await?
        }
    };
    let name = match d.memo.as_deref().filter(|m| !m.is_empty()) {
        Some(memo) => format!("{}: {memo}", d.line_name.as_deref().unwrap_or_default()),
        None => d.line_name.clone().unwrap_or_default(),
    };
    let amount = if d.inbound { d.amount } else { -d.amount };
    let partner = d.partner.map_or(Cell::Null, Cell::Id);
    let item = |account: i64, amount: Decimal| {
        let mut vals = RawVals::new()
            .set_cell("name", Cell::Str(name.as_str().into()))
            .set_cell("account_id", Cell::Id(account))
            .set_cell("partner_id", partner.clone())
            .set_cell("currency_id", Cell::Id(d.currency))
            .set_cell("date_maturity", Cell::Date(d.date))
            .set_cell("amount_currency", Cell::Decimal(amount));
        if d.currency == d.company_currency {
            vals.insert("balance".into(), Cell::Decimal(amount));
        }
        Command::create(vals)
    };
    let vals = RawVals::new()
        .set_cell("move_type", text("entry"))
        .set_cell("journal_id", Cell::Id(d.journal))
        .set_cell("company_id", Cell::Id(d.company))
        .set_cell("currency_id", Cell::Id(d.currency))
        .set_cell("date", Cell::Date(d.date))
        .set_cell("partner_id", partner.clone())
        .set_cell(
            "ref",
            d.memo
                .as_deref()
                .map_or(Cell::Null, |m| Cell::Str(m.into())),
        )
        .set_commands(
            "line_ids",
            vec![item(outstanding, amount), item(destination, -amount)],
        );
    Ok((vals, outstanding, destination))
}

/// Cancels the payments of `payments` (partial): a draft, rejected or cancelled
/// payment becomes cancelled; a confirmed one is refused (its posted entry is undone by
/// a reversal, not by cancelling it).
///
/// # Errors
///
/// As [`post_payments`]; `UserError` for a confirmed payment.
pub async fn cancel_payments(tx: &mut Transaction, payments: &Records) -> Result<()> {
    set_states(
        tx,
        payments,
        "canceled",
        &["draft", "rejected", "canceled"],
        "cancelling it",
    )
    .await
}

/// Returns cancelled or rejected payments of `payments` to draft; a confirmed one is refused
/// (under the default reversal-only policy).
///
/// # Errors
///
/// As [`cancel_payments`].
pub async fn draft_payments(tx: &mut Transaction, payments: &Records) -> Result<()> {
    set_states(
        tx,
        payments,
        "draft",
        &["draft", "rejected", "canceled"],
        "resetting it to draft",
    )
    .await
}

async fn set_states(
    tx: &mut Transaction,
    payments: &Records,
    target: &str,
    from: &[&str],
    what: &str,
) -> Result<()> {
    if payments.is_empty() {
        return Ok(());
    }
    authorize(tx, payments).await?;
    let ids = sorted(payments);
    tx.savepoint(async |tx| {
        tx.flush().await?;
        let states = lock(tx, &ids).await?;
        refuse_posted(tx, &ids, what).await?;
        if let Some((_, state, name)) = states.iter().find(|(_, s, _)| !from.contains(&s.as_str()))
        {
            return Err(Error::user(format!(
                "Payment {name} is {state}: {what} is not possible."
            )));
        }
        let changed: Vec<(i64, String)> = states
            .iter()
            .filter(|(_, s, _)| s != target)
            .map(|(id, s, _)| (*id, s.clone()))
            .collect();
        let changed_ids: Vec<i64> = changed.iter().map(|(id, _)| *id).collect();
        tx.execute(
            "UPDATE account_payment SET state = $2 WHERE id = ANY($1::int8[])",
            &[&changed_ids, &target],
        )
        .await?;
        tx.track_external(
            payments,
            "state",
            changed.iter().map(|(id, s)| (*id, text(s), text(target))),
        )?;
        invalidate(tx).await
    })
    .await
}

/// Marks the paid payments of `payments` as reconciled by hand: refused
/// when the outstanding account is a bank or cash account, or the payment is not paid.
///
/// # Errors
///
/// As [`post_payments`]; `UserError` naming the payment.
pub(super) async fn validate_payments(tx: &mut Transaction, payments: &Records) -> Result<()> {
    if payments.is_empty() {
        return Ok(());
    }
    authorize(tx, payments).await?;
    let ids = sorted(payments);
    tx.savepoint(async |tx| {
        tx.flush().await?;
        lock(tx, &ids).await?;
        let rows = tx
            .query(
                "SELECT p.id::int8, COALESCE(p.name, 'Draft Payment'), p.state::text, \
                 COALESCE(a.account_type = 'asset_cash', false) \
                 FROM account_payment p LEFT JOIN account_account a ON a.id = p.outstanding_account_id \
                 WHERE p.id = ANY($1::int8[]) ORDER BY p.id",
                &[&ids],
            )
            .await?;
        for row in &rows {
            let name: String = row.try_get(1).map_err(db)?;
            let state: String = row.try_get(2).map_err(db)?;
            let cash: bool = row.try_get(3).map_err(db)?;
            if cash {
                return Err(Error::user(format!(
                    "Payment {name} books to a bank or cash account; such payments cannot be \
                     marked as reconciled."
                )));
            }
            if state != "paid" {
                return Err(Error::user(format!(
                    "Payment {name} must be paid to be marked as reconciled; it is {state}."
                )));
            }
        }
        tx.execute(
            "UPDATE account_payment SET state = 'reconciled' WHERE id = ANY($1::int8[])",
            &[&ids],
        )
        .await?;
        tx.track_external(
            payments,
            "state",
            ids.iter().map(|id| (*id, text("paid"), text("reconciled"))),
        )?;
        invalidate(tx).await
    })
    .await
}

/// The one transition function of the status and flags of the confirmed payments whose entries
/// are among `moves` or matched with their items (reference
/// `_compute_state`, `_compute_reconciliation_status`, observed): `is_matched` when the liquidity
/// item is settled or books on the journal's own account; `is_reconciled` when the counterpart
/// items on reconcilable accounts are settled; Reconciled when the liquidity item is settled, or
/// when the outstanding account is not a bank or cash account and every invoice (or every bill)
/// the payment is matched with is paid; else Paid. A zero payment has both flags. Runs in the unit
/// that matched, unmatched or posted, after the invoices' status.
///
/// # Errors
///
/// Database errors.
pub async fn refresh_payments(tx: &mut Transaction, moves: &[i64]) -> Result<()> {
    // per line: the residual in the payment currency when it is foreign, else the company's
    let rows = tx
        .query(
            "WITH touched AS ( \
                 SELECT unnest($1::int8[]) AS move_id \
                 UNION SELECT cl.move_id FROM account_partial_reconcile pr \
                 JOIN account_move_line src ON src.id IN (pr.debit_move_id, pr.credit_move_id) \
                 JOIN account_move_line cl ON cl.id IN (pr.debit_move_id, pr.credit_move_id) \
                 WHERE src.move_id = ANY($1::int8[])), \
             pay AS ( \
                 SELECT p.id, p.move_id, p.state, p.amount, p.outstanding_account_id, \
                        p.destination_account_id, j.default_account_id, \
                        p.currency_id <> co.currency_id AS foreign_currency, \
                        COALESCE(oa.account_type = 'asset_cash', false) AS cash \
                 FROM account_payment p JOIN touched t ON t.move_id = p.move_id \
                 JOIN account_journal j ON j.id = p.journal_id \
                 JOIN res_company co ON co.id = p.company_id \
                 LEFT JOIN account_account oa ON oa.id = p.outstanding_account_id \
                 WHERE p.state IN ('paid', 'reconciled')) \
             SELECT pay.id::int8, pay.state::text, COALESCE(pay.amount, 0) = 0, \
                    pay.default_account_id IS NOT NULL \
                        AND pay.default_account_id = pay.outstanding_account_id, pay.cash, \
                    COALESCE(SUM(r.residual) FILTER (WHERE l.account_id = pay.outstanding_account_id), 0), \
                    COALESCE(SUM(r.residual) FILTER (WHERE l.account_id = pay.destination_account_id \
                                                     AND a.reconcile), 0) \
             FROM pay JOIN account_move_line l ON l.move_id = pay.move_id \
             JOIN account_account a ON a.id = l.account_id \
             LEFT JOIN account_open_item o ON o.line_id = l.id \
             CROSS JOIN LATERAL (SELECT CASE \
                 WHEN o.line_id IS NOT NULL THEN \
                      CASE WHEN pay.foreign_currency THEN o.amount_residual_currency ELSE o.amount_residual END \
                 WHEN l.full_reconcile_id IS NOT NULL THEN 0 \
                 WHEN pay.foreign_currency THEN l.amount_currency ELSE l.balance END AS residual) r \
             GROUP BY pay.id, pay.state, pay.amount, pay.default_account_id, \
                      pay.outstanding_account_id, pay.cash \
             ORDER BY pay.id",
            &[&moves],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.try_get::<_, i64>(0).map_err(db))
        .collect::<Result<_>>()?;
    let documents = matched_documents(tx, &ids).await?;
    let mut states = Vec::with_capacity(rows.len());
    let mut reconciled = Vec::with_capacity(rows.len());
    let mut matched = Vec::with_capacity(rows.len());
    let mut changes = Vec::new();
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(db)?;
        let before: String = row.try_get(1).map_err(db)?;
        let zero: bool = row.try_get(2).map_err(db)?;
        let on_journal_account: bool = row.try_get(3).map_err(db)?;
        let cash: bool = row.try_get(4).map_err(db)?;
        let liquidity: Decimal = row.try_get(5).map_err(db)?;
        let counterpart: Decimal = row.try_get(6).map_err(db)?;
        let docs = documents.get(&id);
        let state = payment_state(liquidity.is_zero(), cash, docs);
        reconciled.push(zero || counterpart.is_zero());
        matched.push(zero || on_journal_account || liquidity.is_zero());
        if before != state {
            changes.push((id, before, state));
        }
        states.push(state.to_owned());
    }
    tx.execute(
        "UPDATE account_payment p SET state = v.state, is_reconciled = v.reconciled, \
         is_matched = v.matched \
         FROM unnest($1::int8[], $2::text[], $3::bool[], $4::bool[]) \
              AS v(id, state, reconciled, matched) WHERE p.id = v.id",
        &[&ids, &states, &reconciled, &matched],
    )
    .await?;
    if !changes.is_empty() {
        let records = tx
            .env()
            .records("account.payment")?
            .browse(ids.iter().copied())?;
        tx.track_external(
            &records,
            "state",
            changes
                .iter()
                .map(|(id, before, after)| (*id, text(before), text(after))),
        )?;
    }
    invalidate(tx).await
}

/// Whether the invoices, and the bills, a payment is matched with are all paid.
#[derive(Debug, Default, Clone, Copy)]
struct Documents {
    invoices: (usize, usize),
    bills: (usize, usize),
}

fn payment_state(liquidity_settled: bool, cash: bool, docs: Option<&Documents>) -> &'static str {
    if liquidity_settled {
        return "reconciled";
    }
    let all_paid = |(count, paid): (usize, usize)| count > 0 && count == paid;
    match docs {
        Some(d) if !cash && (all_paid(d.invoices) || all_paid(d.bills)) => "reconciled",
        _ => "paid",
    }
}

/// The invoices and bills matched with the items of each payment of `ids` (reference
/// `_compute_stat_buttons_from_reconciliation`, observed: the counterpart items on receivable
/// and payable accounts, matched with items of sale or purchase documents).
async fn matched_documents(tx: &mut Transaction, ids: &[i64]) -> Result<BTreeMap<i64, Documents>> {
    let rows = tx
        .query(
            "SELECT DISTINCT p.id::int8, inv.id::int8, inv.move_type::text, \
                    inv.payment_state = 'paid' \
             FROM account_payment p \
             JOIN account_move_line src ON src.move_id = p.move_id \
             JOIN account_account a ON a.id = src.account_id \
                  AND a.account_type IN ('asset_receivable', 'liability_payable') \
             JOIN account_partial_reconcile pr ON src.id IN (pr.debit_move_id, pr.credit_move_id) \
             JOIN account_move_line cl ON cl.id = CASE WHEN src.id = pr.debit_move_id \
                  THEN pr.credit_move_id ELSE pr.debit_move_id END \
             JOIN account_move inv ON inv.id = cl.move_id \
             WHERE p.id = ANY($1::int8[]) \
               AND inv.move_type IN ('out_invoice', 'out_refund', 'in_invoice', 'in_refund')",
            &[&ids],
        )
        .await?;
    let mut found: BTreeMap<i64, Documents> = BTreeMap::new();
    for row in &rows {
        let payment: i64 = row.try_get(0).map_err(db)?;
        let kind: String = row.try_get(2).map_err(db)?;
        let paid: bool = row.try_get(3).map_err(db)?;
        let docs = found.entry(payment).or_default();
        let slot = if kind.starts_with("out_") {
            &mut docs.invoices
        } else {
            &mut docs.bills
        };
        slot.0 += 1;
        slot.1 += usize::from(paid);
    }
    Ok(found)
}

async fn invalidate(tx: &mut Transaction) -> Result<()> {
    tx.invalidate_model(
        "account.payment",
        Some(&[
            "state",
            "name",
            "move_id",
            "is_reconciled",
            "is_matched",
            "outstanding_account_id",
            "destination_account_id",
            "display_name",
        ]),
    )
    .await
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
