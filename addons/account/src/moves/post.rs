// SPDX-License-Identifier: MIT OR Apache-2.0
//! The state changes of journal entries: post, reset to draft and cancel (3;
//! reference `_post`, `button_draft`, `button_cancel` in,
//! 6920-6940, 7070-7082`).
//!
//! Posting a batch of plain entries is set-based: one read of the headers, one aggregate over their
//! lines, one lock check (two when entries are re-dated), one header lock, one statement that flips
//! the headers and their lines together, then the numbering step last (3 steps 2, 3, 5
//! and 7); for them the statement count does not grow with the batch. The draft invoices of a batch
//! are first rebuilt from their product lines by the invoice line sync, which writes each changed
//! line and each changed header on its own, and an undated customer invoice takes today's date
//! through the entry write: that part grows with the invoices that need it. The validations are
//! collected and refused together, as the reference does.
//!
//! The company posting policy is `reversal_only` for every company until the opt-in policy column
//! exists, so a posted entry never returns to draft or to cancelled: it is reversed.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::Operation;
use erp_sdk::prelude::*;
use erp_sdk::sequence::{FiscalYear, SequencePattern, SequenceReset};

use super::{AccountMove, TEXT_LINE_TYPES};
use crate::fiscal::PeriodVerdict;
use crate::fiscal::calendar::{add_months, next_day, previous_day};
use crate::fiscal::settings::FiscalCalendar;
use crate::journal::{AccountJournal, JournalSequence};
use crate::locking::{
    JournalClass, LockCheck, LockRefusal, LockVerdict, TaxImpact, check_lock_dates,
};
use crate::numbering::{LockWait, NumberRequest, assign_numbers};

/// Strips only `default_*` keys from `env`, preserving caller `allowed_company_ids`, `tz`, `lang`,
/// and `active_test`.
pub(crate) fn strip_default_context(env: &Env) -> Env {
    let mut ctx = env.context().clone();
    let keys: Vec<String> = ctx
        .default_field_names()
        .map(|k| format!("default_{k}"))
        .collect();
    for k in keys {
        ctx.remove(&k);
    }
    env.with_context_replaced(ctx)
}

/// The group whose members may post, reset and cancel entries (the reference asks for its
/// invoicing group, which this addon does not declare yet; the accounting users hold the rights
/// on entries, `security/ir.access.csv`).
pub const POSTER_GROUP: &str = "account.group_account_user";

/// What a caller may still change on a posted or cancelled entry: its reference. Every other
/// field is refused by name, the invoice fields included (due date, totals, payment state, invoice
/// lines). Observed: the reference refuses its posted readonly fields (lines, invoice lines,
/// dates, partner, currency, payment terms,
/// refused at `:4061-4071`) and leaves the rest to its views. Modified: a default-deny list in
/// the ORM (R1b; the payment fields of its list are written by the payment slice,
/// never by a caller, and `narration` does not exist yet).
pub const POSTED_MOVE_WRITABLE: [&str; 1] = ["ref"];

/// What a caller may still change on the items of a posted or cancelled entry: the label and the
/// due date (R1b). Observed: the reference protects the tax, fiscal and hash fields of
/// a posted item (
/// `:3829-3838`) and the invoice amounts through its views. Modified: everything else (quantity,
/// price, discount, product, subtotals, taxes, amounts, accounts) is refused by the ORM.
pub const POSTED_LINE_WRITABLE: [&str; 2] = ["name", "date_maturity"];

#[methods(AccountMove, addon = "account")]
impl AccountMove {
    /// Posts the draft entries: every check is collected and refused together, an entry dated in
    /// a soft-locked period is re-dated, then the entries and their lines become posted in one
    /// statement and unnamed entries take the next numbers of their journals (
    /// -044, -048, -054).
    #[method]
    #[rpc(effect = write, assistant = offer)]
    pub async fn action_post(this: &RecordSet<AccountMove>, tx: &mut Transaction) -> Result<()> {
        post_moves(tx, this.as_records()).await
    }

    /// Returns cancelled entries (or, when allowed by company policy, unlocked un-reconciled
    /// posted entries) to draft.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_draft(this: &RecordSet<AccountMove>, tx: &mut Transaction) -> Result<()> {
        reset_moves(tx, this.as_records()).await
    }

    /// Create and open a reversed draft entry or credit note for this posted entry.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_reverse(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rev_id = reverse_move(tx, this.as_records()).await?;
        let rev_type = this
            .env()
            .records("account.move")?
            .browse([rev_id])?
            .cell(tx, "move_type")
            .await?;
        let is_invoice = matches!(
            rev_type,
            Cell::Str(ref s)
                if matches!(
                    s.as_ref(),
                    "out_invoice" | "out_refund" | "in_invoice" | "in_refund"
                )
        );
        let mut view_id = Value::False;
        if is_invoice {
            let vrows = tx
                .query(
                    "SELECT res_id::int8 FROM ir_model_data \
                     WHERE module = 'account' AND name = 'view_invoice_form' LIMIT 1",
                    &[],
                )
                .await?;
            if let Some(vid) = vrows.first().and_then(|r| r.try_get::<_, i64>(0).ok()) {
                view_id = Value::Int(vid);
            }
        }
        let mut action = erp_sdk::ValueMap::new();
        action.insert("type".into(), Value::str("ir.actions.act_window"));
        action.insert(
            "name".into(),
            Value::str(if is_invoice {
                "Credit Note"
            } else {
                "Reversal"
            }),
        );
        action.insert("res_model".into(), Value::str("account.move"));
        action.insert("res_id".into(), Value::Int(rev_id));
        action.insert(
            "views".into(),
            Value::list([Value::list([view_id, Value::str("form")])]),
        );
        action.insert("target".into(), Value::str("current"));
        Ok(Value::Map(action.into()))
    }

    /// Extension hook called after [`reverse_move_with_options`] creates a reversal entry/credit
    /// note and its lines. Downstream addons (`sale`, `purchase`) override this to propagate
    /// order-line links (`sale_line_id`, `purchase.direct.bill.line`) without layering cycles.
    #[method]
    pub async fn _after_reverse_move(
        _this: &RecordSet<AccountMove>,
        _tx: &mut Transaction,
        _source_move_id: i64,
        _line_pairs: Value,
    ) -> Result<()> {
        Ok(())
    }

    /// Whether Reset to Draft is refused ([`reset_moves`]): every entry that is neither cancelled
    /// nor a posted entry in a company with `allow_posted_reset = true`, plus any entry with
    /// blocking consequences (matched items, payment, posted allocation/depreciation/settlement/
    /// disposal/bank statement). The matches are written by SQL; the reconciliation service drops
    /// this value with them. An unsaved copy answers for the entry it was made from.
    #[compute(depends = [
        "state",
        "company_id",
        "company_id.allow_posted_reset",
        "origin_payment_id",
        "line_ids.full_reconcile_id"
    ])]
    async fn _compute_reset_blocked(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountMove>,
    ) -> Result<()> {
        let states = this.mapped(tx, AccountMove::STATE).await?;
        let companies = this.mapped(tx, AccountMove::COMPANY_ID).await?;
        let comp_ids: BTreeSet<i64> = companies
            .iter()
            .filter_map(|c| c.map(|id| id.get()))
            .collect();
        let mut allow_reset_by_company = BTreeMap::<i64, bool>::new();
        if !comp_ids.is_empty() {
            let ids_vec: Vec<i64> = comp_ids.into_iter().collect();
            let rows = tx
                .query(
                    "SELECT id::int8, COALESCE(allow_posted_reset, false) \
                     FROM res_company WHERE id = ANY($1::int8[])",
                    &[&ids_vec],
                )
                .await?;
            for row in rows {
                let cid: i64 = row.try_get(0).map_err(db)?;
                let allow: bool = row.try_get(1).map_err(db)?;
                allow_reset_by_company.insert(cid, allow);
            }
        }
        let candidates: Vec<i64> = this
            .as_records()
            .rec_ids()
            .iter()
            .zip(&states)
            .zip(&companies)
            .filter(|((_, state), cid)| {
                state.as_deref() == Some("cancel")
                    || (state.as_deref() == Some("posted")
                        && cid
                            .and_then(|c| allow_reset_by_company.get(&c.get()).copied())
                            .unwrap_or(false))
            })
            .filter_map(|((id, _), _)| id.as_real().or(id.origin()).map(|row| row.get()))
            .collect();
        let blocked = if candidates.is_empty() {
            BTreeMap::new()
        } else {
            consequences(tx, &candidates).await?
        };
        for (index, ((id, state), cid)) in this
            .as_records()
            .rec_ids()
            .iter()
            .zip(&states)
            .zip(&companies)
            .enumerate()
        {
            let eligible = state.as_deref() == Some("cancel")
                || (state.as_deref() == Some("posted")
                    && cid
                        .and_then(|c| allow_reset_by_company.get(&c.get()).copied())
                        .unwrap_or(false));
            let free = eligible
                && id
                    .as_real()
                    .or(id.origin())
                    .is_some_and(|row| !blocked.contains_key(&row.get()));
            out.set(index, AccountMove::RESET_BLOCKED, !free)?;
        }
        Ok(())
    }

    /// Cancels draft entries. A posted entry is refused: the company posting policy is
    /// reversal-only.
    #[method]
    #[rpc(effect = write)]
    pub async fn button_cancel(this: &RecordSet<AccountMove>, tx: &mut Transaction) -> Result<()> {
        cancel_moves(tx, this.as_records()).await
    }
}

/// What the posting reads of one entry before it decides.
struct Header {
    id: i64,
    state: String,
    name: Option<String>,
    date: Date,
    company: i64,
    root_company: i64,
    journal: i64,
    journal_company: i64,
    journal_code: String,
    journal_type: String,
    journal_active: bool,
    journal_reset: String,
    journal_numbered_before: bool,
    currency_active: bool,
    currency_name: String,
    reference: Option<String>,
    move_type: String,
    payment: bool,
    allow_posted_reset: bool,
}

impl Header {
    fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("entry {name}"),
            None => format!("draft entry {}", self.id),
        }
    }

    fn journal_class(&self) -> JournalClass {
        match self.journal_type.as_str() {
            "sale" => JournalClass::Sale,
            "purchase" => JournalClass::Purchase,
            _ => JournalClass::Other,
        }
    }

    fn is_sale_document(&self) -> bool {
        matches!(
            self.move_type.as_str(),
            "out_invoice" | "out_refund" | "out_receipt"
        )
    }

    /// The numbering kind of the entry: a credit note asks for the refund chain and a payment's
    /// entry the payment chain, which the journal gives only when it keeps one (`R` or `P` before
    /// the code; else the document chain); everything else is a document (reference
    /// ).
    fn sequence_kind(&self) -> &'static str {
        if self.payment {
            "payment"
        } else if crate::invoice::is_refund(&self.move_type) {
            "refund"
        } else {
            "document"
        }
    }
}

/// What the posting reads of the lines of one entry.
#[derive(Default)]
struct LineFacts {
    accountable: i64,
    unbalanced: Option<String>,
    mixed_off_balance: bool,
    archived_accounts: Vec<i64>,
    foreign_accounts: Vec<i64>,
    /// A line carries taxes or is a tax line, so the entry reaches the tax report.
    taxed: bool,
}

/// Post the draft entries of `moves` (3). Atomic: a refusal leaves every entry as it
/// was.
///
/// # Errors
///
/// `Access` without [`POSTER_GROUP`] or write access to an entry; `UserError` with every collected
/// validation message; the lock and period refusals of [`check_lock_dates`]; numbering and
/// database errors.
pub async fn post_moves(tx: &mut Transaction, moves: &Records) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }
    authorize(tx, moves, "post").await?;
    let ids = sorted_ids(moves);
    tx.savepoint(async |tx| {
        tx.flush().await?;
        // The draft invoices of the batch are rebuilt from their product lines first: a price,
        // quantity, discount or tax written on an invoice line directly did not run the sync, and
        // the checks below read the stored totals. Then the invoice checks and the invoice date
        // default: the date they write is part of what the header read below sees. Entries that
        // are not invoices pass all three.
        crate::invoice::sync_lines(tx, moves.env(), &ids).await?;
        super::refresh_entry_totals(tx, &ids).await?;
        tx.flush().await?;
        crate::invoice::post::check_postable(tx, moves.env(), &ids).await?;
        crate::invoice::post::before_post(tx, moves.env(), &ids).await?;
        tx.flush().await?;
        let headers = read_headers(tx, &ids).await?;
        let facts = line_facts(tx, &ids).await?;
        let mut messages = Vec::new();
        let mut account_ids = BTreeSet::new();
        for header in &headers {
            if let Some(facts) = facts.get(&header.id) {
                account_ids.extend(facts.archived_accounts.iter().copied());
                account_ids.extend(facts.foreign_accounts.iter().copied());
            }
        }
        let account_names = account_names(tx, moves.env(), &account_ids).await?;
        for header in &headers {
            let facts = facts.get(&header.id).ok_or_else(|| {
                Error::invariant(format!("entry {} has no line aggregate", header.id))
            })?;
            for message in validate(header, facts, &account_names) {
                let message = if headers.len() > 1 {
                    format!("{}: {message}", header.label())
                } else {
                    message
                };
                if !messages.contains(&message) {
                    messages.push(message);
                }
            }
        }
        if !messages.is_empty() {
            return Err(Error::user(messages.join("\n")));
        }
        let taxed: BTreeSet<i64> = facts
            .iter()
            .filter(|(_, f)| f.taxed)
            .map(|(id, _)| *id)
            .collect();
        let dates = accounting_dates(tx, moves.env(), &headers, &taxed).await?;
        let locked = lock_headers(tx, &ids).await?;
        let unchanged = headers.len() == locked.len()
            && headers
                .iter()
                .zip(&locked)
                .all(|(h, (id, state, date))| h.id == *id && h.state == *state && h.date == *date);
        if !unchanged {
            return Err(Error::conflict(
                "A journal entry changed while it was being posted; post it again.",
            ));
        }
        let pairs: Vec<(i64, Date)> = headers
            .iter()
            .map(|h| h.id)
            .zip(dates.iter().copied())
            .collect();
        // Controlling: auto-assign default cost centers, enforce mandatory/locks/validity against
        // the resolved accounting dates, stamp intercompany trading partners, and check real-time
        // Budget AVC before posting.
        crate::controlling::apply_default_cost_centers(tx, &ids).await?;
        crate::controlling::validate_move_cost_centers(tx, &pairs).await?;
        stamp_trading_partners_for_moves(tx, &ids).await?;
        let avc_touched =
            crate::budget::avc::check_and_apply_move_avc(tx, moves.env(), &pairs).await?;

        set_state(tx, &pairs, "posted").await?;
        // the history of the entries (a no-op without a tracking addon such as the mail bridge)
        tx.track_external(
            moves,
            "state",
            headers
                .iter()
                .map(|h| (h.id, text_cell(&h.state), text_cell("posted"))),
        )?;
        tx.track_external(
            moves,
            "date",
            pairs
                .iter()
                .zip(&headers)
                .map(|((id, date), h)| (*id, Cell::Date(h.date), Cell::Date(*date))),
        )?;
        number(tx, moves, &headers, &dates).await?;
        create_analytic_lines_for_moves(tx, moves.env(), &ids).await?;
        crate::asset::cip::record_cip_lines_for_moves(tx, moves.env(), &ids).await?;
        if !avc_touched.is_empty() {
            crate::budget::refresh_budget_lines(tx, &avc_touched).await?;
        }
        crate::budget::refresh_matching_budget_lines_for_moves(tx, &ids).await?;
        invalidate(tx, moves).await
    })
    .await
}

async fn stamp_trading_partners_for_moves(tx: &mut Transaction, move_ids: &[i64]) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    let updated = tx
        .execute(
            "UPDATE account_move_line l \
             SET trading_partner_company_id = COALESCE(p.trading_partner_company_id, c.id) \
             FROM res_partner p \
             LEFT JOIN LATERAL ( \
                 SELECT rc.id FROM res_company rc WHERE rc.partner_id = p.id ORDER BY rc.id LIMIT 1 \
             ) c ON TRUE \
             WHERE l.move_id = ANY($1::int8[]) \
               AND l.partner_id = p.id \
               AND l.trading_partner_company_id IS NULL \
               AND COALESCE(p.trading_partner_company_id, c.id) IS NOT NULL \
               AND COALESCE(p.trading_partner_company_id, c.id) <> l.company_id",
            &[&move_ids],
        )
        .await?;
    if updated > 0 {
        tx.invalidate_model("account.move.line", Some(&["trading_partner_company_id"]))
            .await?;
    }
    Ok(())
}

async fn create_analytic_lines_for_moves(
    tx: &mut Transaction,
    env: &Env,
    move_ids: &[i64],
) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT l.analytic_account_id::int8, COALESCE(l.name, m.name, 'Journal Item')::text, \
                    l.date, l.partner_id::int8, -COALESCE(l.balance, 0), COALESCE(l.quantity, 0), \
                    m.name::text, l.id::int8, l.account_id::int8 \
             FROM account_move_line l \
             JOIN account_move m ON m.id = l.move_id \
             WHERE l.move_id = ANY($1::int8[]) \
               AND l.analytic_account_id IS NOT NULL \
               AND COALESCE(l.balance, 0) <> 0 \
             ORDER BY l.id",
            &[&move_ids],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut vals_list = Vec::with_capacity(rows.len());
    for row in rows {
        let aid: i64 = row.try_get(0).map_err(db)?;
        let label: String = row.try_get(1).map_err(db)?;
        let date: Date = row.try_get(2).map_err(db)?;
        let partner: Option<i64> = row.try_get(3).map_err(db)?;
        let amount: Decimal = row.try_get(4).map_err(db)?;
        let qty: Decimal = row.try_get(5).map_err(db)?;
        let mref: Option<String> = row.try_get(6).map_err(db)?;
        let ml_id: i64 = row.try_get(7).map_err(db)?;
        let gl_acc_id: Option<i64> = row.try_get(8).map_err(db)?;
        let mut v = RawVals::new();
        v.insert("account_id".into(), Cell::Id(aid));
        v.insert("name".into(), Cell::Str(label.into()));
        v.insert("date".into(), Cell::Date(date));
        v.insert("amount".into(), Cell::Decimal(amount));
        v.insert("unit_amount".into(), Cell::Decimal(qty));
        v.insert("cost_kind".into(), Cell::Str("primary".into()));
        v.insert("move_line_id".into(), Cell::Id(ml_id));
        if let Some(ga) = gl_acc_id {
            v.insert("general_account_id".into(), Cell::Id(ga));
        }
        if let Some(pid) = partner {
            v.insert("partner_id".into(), Cell::Id(pid));
        }
        if let Some(r) = mref {
            v.insert("ref".into(), Cell::Str(r.into()));
        }
        vals_list.push(v);
    }
    let clean_env = strip_default_context(env);
    clean_env
        .records("account.analytic.line")?
        .create(tx, vals_list)
        .await?;
    tx.flush().await?;
    Ok(())
}

/// Return the cancelled entries of `moves` to draft; a posted entry is refused (
/// reversal-only), so no lock date or number is touched (the reference checks the lock dates
/// only when an entry leaves `posted`).
///
/// # Errors
///
/// `Access` as [`post_moves`]; `UserError` for a draft entry or a posted one.
pub async fn reset_moves(tx: &mut Transaction, moves: &Records) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }
    authorize(tx, moves, "reset to draft").await?;
    let ids = sorted_ids(moves);
    tx.savepoint(async |tx| {
        tx.flush().await?;
        let states = lock_headers(tx, &ids).await?;
        let posted: Vec<i64> = states
            .iter()
            .filter(|(_, state, _)| state == "posted")
            .map(|(id, _, _)| *id)
            .collect();
        if !posted.is_empty() {
            let disallowed: i64 = tx
                .query(
                    "SELECT count(*)::int8 FROM account_move m \
                     JOIN res_company c ON c.id = m.company_id \
                     WHERE m.id = ANY($1::int8[]) AND COALESCE(c.allow_posted_reset, false) IS FALSE",
                    &[&posted],
                )
                .await?[0]
                .try_get(0)
                .map_err(db)?;
            if disallowed > 0 {
                return Err(posted_refusal(tx, &posted, "reset to draft").await?);
            }
            let closed = read_closed(tx, &posted).await?;
            refuse_locked(tx, &closed, "reset to draft", LockScope::Tax, None).await?;
            refuse_consequences(tx, &posted, "reset to draft").await?;
            crate::asset::cip::remove_cip_lines_for_moves(tx, &posted).await?;
            let deleted_analytic = tx
                .execute(
                    "DELETE FROM account_analytic_line a \
                     USING account_move_line l \
                     WHERE a.move_line_id = l.id AND l.move_id = ANY($1::int8[])",
                    &[&posted],
                )
                .await?;
            if deleted_analytic > 0 {
                tx.invalidate_model("account.analytic.line", None).await?;
                tx.invalidate_model(
                    "account.analytic.account",
                    Some(&[
                        "debit",
                        "credit",
                        "balance",
                        "subtree_debit",
                        "subtree_credit",
                        "subtree_balance",
                    ]),
                )
                .await?;
            }
        }
        if states
            .iter()
            .any(|(_, state, _)| state != "cancel" && state != "posted")
        {
            return Err(Error::user(
                "Only a posted or cancelled entry can go back to draft; the selection holds a draft.",
            ));
        }
        refuse_consequences(tx, &ids, "reset to draft").await?;
        let pairs: Vec<(i64, Date)> = states.iter().map(|(id, _, date)| (*id, *date)).collect();
        set_state(tx, &pairs, "draft").await?;
        if !posted.is_empty() {
            crate::budget::avc::restore_commitments_and_refresh_for_reset_moves(tx, &posted)
                .await?;
            crate::budget::refresh_matching_budget_lines_for_moves(tx, &posted).await?;
        }
        tx.track_external(
            moves,
            "state",
            states
                .iter()
                .map(|(id, state, _)| (*id, text_cell(state), text_cell("draft"))),
        )?;
        invalidate(tx, moves).await
    })
    .await
}

/// Cancel the draft entries of `moves`; a posted entry is refused (reversal-only).
/// Reconciliation, payments and scheduled posting do not exist yet, so the reference's clean-up
/// of those has nothing to act on.
///
/// # Errors
///
/// `Access` as [`post_moves`]; `UserError` for an entry that is not a draft.
pub async fn cancel_moves(tx: &mut Transaction, moves: &Records) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }
    authorize(tx, moves, "cancel").await?;
    let ids = sorted_ids(moves);
    tx.savepoint(async |tx| {
        tx.flush().await?;
        let states = lock_headers(tx, &ids).await?;
        let posted: Vec<i64> = states
            .iter()
            .filter(|(_, state, _)| state == "posted")
            .map(|(id, _, _)| *id)
            .collect();
        if !posted.is_empty() {
            return Err(posted_refusal(tx, &posted, "cancel").await?);
        }
        if states.iter().any(|(_, state, _)| state != "draft") {
            return Err(Error::user(
                "Cancelling applies to draft entries only; the selection holds an entry that is not a draft.",
            ));
        }
        refuse_consequences(tx, &ids, "cancel").await?;
        let pairs: Vec<(i64, Date)> = states.iter().map(|(id, _, date)| (*id, *date)).collect();
        set_state(tx, &pairs, "cancel").await?;
        tx.track_external(
            moves,
            "state",
            states
                .iter()
                .map(|(id, state, _)| (*id, text_cell(state), text_cell("cancel"))),
        )?;
        invalidate(tx, moves).await
    })
    .await
}

/// Cancel the draft entries `drafts` that belong to a commercial document being cancelled (a
/// sales or purchase order), through [`cancel_moves`]. The whole entry is cancelled, also when it
/// carries items of another order or items linked to no order: those orders' invoiced quantities
/// recompute, and an accountant can reset the entry to draft. The order's own write right is the
/// caller's to check before; a caller without accounting rights is elevated for this one state
/// change only, so a user of the sales or purchase app can cancel an order whose only invoices
/// are drafts. Nothing is returned to the caller.
///
/// # Errors
///
/// The errors of [`cancel_moves`]: an entry that is no longer a draft is refused.
pub async fn cancel_document_drafts(tx: &mut Transaction, env: &Env, drafts: &[i64]) -> Result<()> {
    if drafts.is_empty() {
        return Ok(());
    }
    let env = if env.su() || tx.has_group(env, POSTER_GROUP).await? {
        env.clone()
    } else {
        env.sudo(true)
    };
    cancel_moves(
        tx,
        &env.records("account.move")?
            .browse(drafts.iter().copied())?,
    )
    .await
}

/// Asks the caller's `operation` right on the entries of `move_ids`. It runs before any statement
/// reads or locks an entry by a caller-supplied id: without it the refusal text would give the
/// state and number of an entry the caller may not read (another company's), and a caller without
/// rights could hold its header until its own transaction ends.
async fn authorize_entries(
    tx: &mut Transaction,
    env: &Env,
    move_ids: &[i64],
    operation: Operation,
) -> Result<()> {
    env.records("account.move")?
        .browse(move_ids.iter().copied())?
        .check_access(tx, operation)
        .await
}

/// Which lock dates a change of a posted entry asks before the posted-entry refusal, so the
/// refusal of a change in a locked period names the lock date. Observed: the reference checks the
/// fiscal dates on the entry and the tax date on its tax-report lines when a posted entry's date
/// or name changes, it is deleted or leaves `posted`, and when a line's protected fields change.
/// A cancelled or draft entry asks no lock date (a draft is re-dated when it is posted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockScope {
    /// The change touches no lock-protected field: only the posted-entry refusal applies.
    Unprotected,
    /// The fiscal-year, sale or purchase (by journal) and hard lock dates.
    Fiscal,
    /// [`Self::Fiscal`] and, for an entry with a line that reaches the tax report, the tax lock
    /// date (the scope of a date, a name, an amount, a tax, an added or a deleted line).
    Tax,
    /// Unmatching: the lock dates of [`Self::Tax`] that an administrator can lower again, not the
    /// hard lock (a match made into a hard-locked period must stay removable, so in such a period
    /// the fiscal-year date, which cannot be lowered across it, does not bind either), and a date
    /// without a fiscal period refuses.
    Unmatch,
}

/// The fields of a journal item whose change a lock date protects (the reference's fiscal list,
/// with this addon's `debit`, `credit` and tax distribution line, which set the balance and the
/// tax report). The first [`LINE_TAX_FIELDS`] also reach the tax report.
const LINE_LOCKED_FIELDS: [&str; 12] = [
    "balance",
    "debit",
    "credit",
    "tax_ids",
    "tax_line_id",
    "tax_repartition_line_id",
    "tax_tag_ids",
    "account_id",
    "journal_id",
    "amount_currency",
    "currency_id",
    "partner_id",
];
const LINE_TAX_FIELDS: usize = 7;

/// The lock scope of a change of `fields` on the items of a posted entry.
pub(crate) fn line_lock_scope(fields: &[&str]) -> LockScope {
    let protected = |range: &[&str]| fields.iter().any(|f| range.contains(f));
    if protected(&LINE_LOCKED_FIELDS[..LINE_TAX_FIELDS]) {
        LockScope::Tax
    } else if protected(&LINE_LOCKED_FIELDS) {
        LockScope::Fiscal
    } else {
        LockScope::Unprotected
    }
}

/// Refuses `what` on the entries of `move_ids` that are posted or cancelled (
/// -066, -069, -070), after the caller's `operation` right on those entries was checked. A posted
/// entry is first checked against the lock dates of `scope` at its date and, when the change
/// gives one, at `new_date`, so a change in a locked period names the lock date.
///
/// # Errors
///
/// `Access` when the caller lacks `operation` on one of the entries; `Validation` naming the
/// first posted entry in a locked period and its lock dates; `UserError` naming the first posted
/// or cancelled entry; database errors.
pub(crate) async fn refuse_closed_entries(
    tx: &mut Transaction,
    env: &Env,
    move_ids: &[i64],
    operation: Operation,
    what: &str,
    scope: LockScope,
    new_date: Option<Date>,
) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    authorize_entries(tx, env, move_ids, operation).await?;
    closed_refusal(tx, move_ids, what, scope, new_date).await
}

/// What a refusal reads of one posted or cancelled entry.
struct ClosedEntry {
    id: i64,
    state: String,
    name: Option<String>,
    company: i64,
    date: Date,
    journal_type: String,
    taxed: bool,
}

impl ClosedEntry {
    fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("{}", self.id))
    }

    fn journal_class(&self) -> JournalClass {
        match self.journal_type.as_str() {
            "sale" => JournalClass::Sale,
            "purchase" => JournalClass::Purchase,
            _ => JournalClass::Other,
        }
    }
}

/// The posted and cancelled entries of `move_ids`, in id order, in one read (a batch of drafts
/// reads nothing more).
async fn read_closed(tx: &mut Transaction, move_ids: &[i64]) -> Result<Vec<ClosedEntry>> {
    let rows = tx
        .query(
            "SELECT m.id::int8, m.state::text, m.name::text, m.company_id::int8, m.date, \
             j.type::text, EXISTS (SELECT 1 FROM account_move_line l WHERE l.move_id = m.id \
               AND (l.tax_line_id IS NOT NULL OR l.tax_repartition_line_id IS NOT NULL \
                    OR EXISTS (SELECT 1 FROM account_move_line_account_tax_rel r \
                               WHERE r.account_move_line_id = l.id))) \
             FROM account_move m JOIN account_journal j ON j.id = m.journal_id \
             WHERE m.id = ANY($1::int8[]) AND m.state <> 'draft' ORDER BY m.id",
            &[&move_ids],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(ClosedEntry {
                id: row.try_get(0).map_err(db)?,
                state: row.try_get(1).map_err(db)?,
                name: row.try_get(2).map_err(db)?,
                company: row.try_get(3).map_err(db)?,
                date: row.try_get(4).map_err(db)?,
                journal_type: row.try_get(5).map_err(db)?,
                taxed: row.try_get(6).map_err(db)?,
            })
        })
        .collect()
}

/// The SQL condition "an item of entry `m` is matched": a settled item carries its settlement, a
/// partly matched one its open-item row (both written only by the reconciliation service).
const MATCHED_ITEMS: &str = "EXISTS (SELECT 1 FROM account_move_line l WHERE l.move_id = m.id \
     AND (l.full_reconcile_id IS NOT NULL \
          OR EXISTS (SELECT 1 FROM account_open_item o WHERE o.line_id = l.id)))";

/// What else hangs on the entries of `move_ids`, by entry: matched items and the payment whose
/// entry it is. Entries with neither are left out.
async fn consequences(
    tx: &mut Transaction,
    move_ids: &[i64],
) -> Result<BTreeMap<i64, Vec<String>>> {
    let rows = tx
        .query(
            &format!(
                "SELECT m.id::int8, {MATCHED_ITEMS}, COALESCE(p.name, p.id::text), \
                        EXISTS (SELECT 1 FROM account_asset_depreciation_line dl WHERE dl.move_id = m.id AND dl.state = 'posted'), \
                        EXISTS (SELECT 1 FROM account_asset_settlement s WHERE s.move_id = m.id AND s.state = 'posted'), \
                        EXISTS (SELECT 1 FROM account_asset a WHERE a.disposal_move_id = m.id), \
                        EXISTS (SELECT 1 FROM account_bank_statement_line bsl WHERE bsl.move_id = m.id), \
                        EXISTS (SELECT 1 FROM account_asset_cip_line c \
                                JOIN account_move_line ml ON ml.id = c.move_line_id \
                                WHERE ml.move_id = m.id AND COALESCE(c.settled_amount, 0) <> 0), \
                        EXISTS (SELECT 1 FROM account_analytic_line al \
                                JOIN account_move_line ml ON ml.id = al.move_line_id \
                                JOIN account_cost_allocation_rule r ON r.sender_center_id = al.account_id \
                                JOIN account_cost_allocation_run run \
                                  ON run.cycle_id = r.cycle_id AND run.state = 'posted' \
                                 AND al.date >= run.date_from AND al.date <= run.date_to \
                                WHERE ml.move_id = m.id) \
                 FROM account_move m LEFT JOIN account_payment p ON p.id = m.origin_payment_id \
                 WHERE m.id = ANY($1::int8[]) \
                 ORDER BY m.id"
            ),
            &[&move_ids],
        )
        .await?;
    let mut found = BTreeMap::new();
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(db)?;
        let matched: bool = row.try_get(1).map_err(db)?;
        let payment: Option<String> = row.try_get(2).map_err(db)?;
        let has_depreciation: bool = row.try_get(3).map_err(db)?;
        let has_settlement: bool = row.try_get(4).map_err(db)?;
        let has_disposal: bool = row.try_get(5).map_err(db)?;
        let has_bank_stmt: bool = row.try_get(6).map_err(db)?;
        let has_settled_cip: bool = row.try_get(7).map_err(db)?;
        let has_allocation: bool = row.try_get(8).map_err(db)?;
        let mut reasons = Vec::new();
        if matched {
            reasons.push(
                "an item of it is matched with other items (unmatch it with Unreconcile first)"
                    .to_owned(),
            );
        }
        if let Some(payment) = payment {
            reasons.push(format!("it is the journal entry of payment {payment}"));
        }
        if has_depreciation {
            reasons.push("it is linked to a posted asset depreciation line".to_owned());
        }
        if has_settlement {
            reasons.push(
                "it is linked to a posted CIP settlement (reverse the settlement first)".to_owned(),
            );
        }
        if has_disposal {
            reasons.push("it is the disposal entry of a fixed asset".to_owned());
        }
        if has_bank_stmt {
            reasons.push("it is linked to a bank statement line".to_owned());
        }
        if has_settled_cip {
            reasons.push(
                "its CIP cost lines have already been settled (reverse the settlement first)"
                    .to_owned(),
            );
        }
        if has_allocation {
            reasons.push(
                "its controlling cost center items have been allocated by a posted cost allocation run (reverse the allocation run first)".to_owned(),
            );
        }
        if !reasons.is_empty() {
            found.insert(id, reasons);
        }
    }
    Ok(found)
}

/// Refuses `what` on a draft or cancelled entry of `move_ids` that has a matched item or is a
/// payment's entry (neither happens through the services today: matching takes posted items only
/// and a payment's entry is posted when it is created), naming the entry and what blocks it.
///
/// # Errors
///
/// `UserError` naming the first such entry; database errors.
pub(crate) async fn refuse_consequences(
    tx: &mut Transaction,
    move_ids: &[i64],
    what: &str,
) -> Result<()> {
    match consequences(tx, move_ids).await?.into_iter().next() {
        None => Ok(()),
        Some((id, reasons)) => Err(Error::user(format!(
            "You cannot {what} the entry {id}: {}.",
            reasons.join("; ")
        ))),
    }
}

/// The refusal of [`refuse_closed_entries`], for entries the caller was authorized on: a lock date
/// first (it names the lock), then the closed entry with what else hangs on it.
async fn closed_refusal(
    tx: &mut Transaction,
    move_ids: &[i64],
    what: &str,
    scope: LockScope,
    new_date: Option<Date>,
) -> Result<()> {
    let entries = read_closed(tx, move_ids).await?;
    let Some(first) = entries.first() else {
        return Ok(());
    };
    refuse_locked(tx, &entries, what, scope, new_date).await?;
    let mut reasons = consequences(tx, &[first.id])
        .await?
        .remove(&first.id)
        .unwrap_or_default();
    reasons.push(
        "a posted entry is corrected by Reverse Entry (or Reset to Draft when enabled in company settings), \
         a cancelled one by resetting it to draft first"
            .to_owned(),
    );
    Err(Error::user(format!(
        "You cannot {what} the {} entry {}: {}.",
        first.state,
        first.label(),
        reasons.join("; ")
    )))
}

/// The first posted entry of `entries` dated, or re-dated to `new_date`, on or before a lock date
/// of `scope`, or in a fiscal period that is not open, with the refusal.
async fn first_locked<'a>(
    tx: &mut Transaction,
    entries: &'a [ClosedEntry],
    scope: LockScope,
    new_date: Option<Date>,
) -> Result<Option<(&'a ClosedEntry, LockRefusal)>> {
    if scope == LockScope::Unprotected {
        return Ok(None);
    }
    let mut checks = Vec::new();
    let mut owners = Vec::new();
    for entry in entries.iter().filter(|e| e.state == "posted") {
        let tax = if matches!(scope, LockScope::Tax | LockScope::Unmatch) && entry.taxed {
            TaxImpact::Report
        } else {
            TaxImpact::None
        };
        let moved = new_date.filter(|date| *date != entry.date);
        for date in std::iter::once(entry.date).chain(moved) {
            let check = LockCheck::new(entry.company, date, entry.journal_class(), tax)?;
            checks.push(if scope == LockScope::Unmatch {
                check.without_hard_lock()
            } else {
                check
            });
            owners.push(entry);
        }
    }
    for (entry, verdict) in owners.into_iter().zip(check_lock_dates(tx, &checks).await?) {
        match verdict {
            LockVerdict::Allowed => {}
            // posting creates the period of every posted date, so none is missing in practice; on
            // the write routes the refusal of the closed entry that follows still refuses the
            // change, while unmatching has no such refusal and fails closed
            LockVerdict::Refused(LockRefusal::Period(PeriodVerdict::NoPeriod { .. }))
                if scope != LockScope::Unmatch => {}
            LockVerdict::Refused(refusal) => return Ok(Some((entry, refusal))),
        }
    }
    Ok(None)
}

/// Refuses `what` when [`first_locked`] finds a posted entry in a locked period, naming the entry
/// and its lock dates.
async fn refuse_locked(
    tx: &mut Transaction,
    entries: &[ClosedEntry],
    what: &str,
    scope: LockScope,
    new_date: Option<Date>,
) -> Result<()> {
    match first_locked(tx, entries, scope, new_date).await? {
        None => Ok(()),
        Some((entry, refusal)) => Err(Error::validation(format!(
            "You cannot {what} the posted entry {}: {refusal}",
            entry.label()
        ))
        .with_source(refusal)),
    }
}

/// Refuses `what` on the journal items `line_ids` when the posted entry of one of them is dated on
/// or before a lock date an administrator can lower again (fiscal year, sale or purchase by
/// journal, and the tax lock date for an entry that reaches the tax report), in a fiscal period
/// closed by the fiscal-year lock, or on a date without a fiscal period. The hard lock does not
/// bind it, nor the fiscal-year date in a hard-locked period: matching into a hard-locked period
/// is allowed, so its undoing must stay possible.
/// Unmatching asks it for its whole match walk (every item of a settlement is in it): matches of
/// a locked period stay as they are until the lock is lowered (stricter than the reference, which
/// unmatches and dates the follow-up entries on the first open day). The refused entry is named
/// only when the caller (`env`, never elevated) may read it.
///
/// # Errors
///
/// `Validation` naming the entry and its lock dates (neither for an entry the caller cannot
/// read); database errors.
pub(crate) async fn refuse_locked_items(
    tx: &mut Transaction,
    env: &Env,
    line_ids: &[i64],
    what: &str,
) -> Result<()> {
    let rows = tx
        .query(
            "SELECT DISTINCT move_id::int8 FROM account_move_line WHERE id = ANY($1::int8[]) \
             ORDER BY 1",
            &[&line_ids],
        )
        .await?;
    let move_ids = rows
        .iter()
        .map(|row| row.try_get(0).map_err(db))
        .collect::<Result<Vec<i64>>>()?;
    let entries = read_closed(tx, &move_ids).await?;
    let Some((entry, refusal)) = first_locked(tx, &entries, LockScope::Unmatch, None).await? else {
        return Ok(());
    };
    let readable = !env
        .sudo(false)
        .records("account.move")?
        .browse([entry.id])?
        .filtered_access(tx, Operation::Read)
        .await?
        .is_empty();
    if !readable {
        // neither the entry's name nor its date (the refusal's text carries it)
        return Err(Error::validation(format!(
            "You cannot {what} a posted entry you cannot open: it is in a locked period."
        )));
    }
    Err(Error::validation(format!(
        "You cannot {what} the posted entry {}: {refusal}",
        entry.label()
    ))
    .with_source(refusal))
}

/// The refusal of Reset to Draft or Cancel (`what`) of posted entries: the first one is named with
/// everything that blocks it (its lock dates, matched items, its payment, the posting policy).
async fn posted_refusal(tx: &mut Transaction, posted: &[i64], what: &str) -> Result<Error> {
    let entries = read_closed(tx, posted).await?;
    let first = entries
        .first()
        .ok_or_else(|| Error::invariant("a posted entry was not read back"))?;
    let mut reasons = Vec::new();
    if let Some((_, refusal)) =
        first_locked(tx, std::slice::from_ref(first), LockScope::Tax, None).await?
    {
        reasons.push(refusal.to_string().trim_end_matches('.').to_owned());
    }
    reasons.extend(
        consequences(tx, &[first.id])
            .await?
            .remove(&first.id)
            .unwrap_or_default(),
    );
    reasons.push(REVERSAL_ONLY.to_owned());
    Ok(Error::user(format!(
        "You cannot {what} the posted entry {}: {}.",
        first.label(),
        reasons.join("; ")
    )))
}

/// The fields of `vals` (values and commands) that `writable` does not list, sorted.
pub(crate) fn closed_fields<'a>(vals: &'a RawVals, writable: &[&str]) -> Vec<&'a str> {
    let mut fields: Vec<&str> = vals
        .iter()
        .map(|(field, _)| field)
        .chain(vals.iter_commands().map(|(field, _)| field))
        .filter(|field| !writable.contains(field))
        .collect();
    fields.sort_unstable();
    fields.dedup();
    fields
}

/// Takes the headers of `move_ids` in id order, the order posting locks them in, with a version
/// update, then refuses when one of them is posted or cancelled. A lock alone would not do: a
/// poster whose snapshot is older than a committed item change then sees an unchanged header
/// and posts without the item. With the version update, whichever of the two commits second
/// fails with a serialization error at its header lock, and its retry sees the other's result.
/// The caller's `operation` right on the entries is checked first, before the lock is taken.
///
/// # Errors
///
/// As [`refuse_closed_entries`]; a serialization failure when a concurrent transaction changed one
/// of the headers since this one began.
pub(crate) async fn hold_draft_entries(
    tx: &mut Transaction,
    env: &Env,
    move_ids: &[i64],
    operation: Operation,
    what: &str,
    scope: LockScope,
) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    authorize_entries(tx, env, move_ids, operation).await?;
    tx.execute(
        "WITH held AS (SELECT id FROM account_move WHERE id = ANY($1::int8[]) \
                       ORDER BY id FOR NO KEY UPDATE) \
         UPDATE account_move m SET write_date = m.write_date FROM held WHERE m.id = held.id",
        &[&move_ids],
    )
    .await?;
    closed_refusal(tx, move_ids, what, scope, None).await
}

/// The database ids of `moves`, sorted and without repeats: every statement below reads and
/// writes in this order.
fn sorted_ids(moves: &Records) -> Vec<i64> {
    let mut ids = moves.ids();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The posting-policy part of the refusal of Reset to Draft and Cancel on a posted entry.
const REVERSAL_ONLY: &str = "this company's posting policy only allows a reversal (use Reverse Entry, \
     or enable 'Allow Resetting Posted Entries' in Company Accounting settings)";

/// Create a reversed draft journal entry (or credit note) for a posted `account.move`.
/// Negates every line's `balance` and `amount_currency` and applies `is_storno` when enabled
/// on the company. Returns the created reversal entry id.
pub async fn reverse_move(tx: &mut Transaction, moves: &Records) -> Result<i64> {
    reverse_move_with_options(tx, moves, None, None, None).await
}

/// Create a reversed draft journal entry (or credit note) for a posted `account.move` with
/// optional override date, journal, and reason text. Copies product line taxes so credit notes
/// match the original invoice's tax breakdown.
pub async fn reverse_move_with_options(
    tx: &mut Transaction,
    moves: &Records,
    override_date: Option<Date>,
    override_journal: Option<i64>,
    reason: Option<&str>,
) -> Result<i64> {
    authorize(tx, moves, "reverse").await?;
    let source_id = moves.ensure_one()?.int()?;
    let env = moves.env().clone();
    let reason_owned = reason
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    tx.savepoint(async move |tx| {
        tx.flush().await?;
        let header_rows = tx
            .query(
                "SELECT m.state::text, m.name::text, m.move_type::text, m.journal_id::int8, \
                        m.company_id::int8, m.currency_id::int8, m.partner_id::int8, m.date, \
                        COALESCE(c.account_storno, false) \
                 FROM account_move m \
                 JOIN res_company c ON c.id = m.company_id \
                 WHERE m.id = $1",
                &[&source_id],
            )
            .await?;
        let Some(hrow) = header_rows.first() else {
            return Err(Error::missing("Journal entry not found."));
        };
        let state: String = hrow.try_get(0).map_err(db)?;
        if state != "posted" {
            return Err(Error::user("Only posted journal entries can be reversed."));
        }
        let orig_name: Option<String> = hrow.try_get(1).map_err(db)?;
        let orig_type: String = hrow.try_get(2).map_err(db)?;
        let orig_journal_id: i64 = hrow.try_get(3).map_err(db)?;
        let company_id: i64 = hrow.try_get(4).map_err(db)?;
        let currency_id: i64 = hrow.try_get(5).map_err(db)?;
        let partner_id: Option<i64> = hrow.try_get(6).map_err(db)?;
        let _orig_date: Date = hrow.try_get(7).map_err(db)?;
        let storno: bool = hrow.try_get(8).map_err(db)?;

        let journal_id = override_journal.unwrap_or(orig_journal_id);
        let rev_type = match orig_type.as_str() {
            "out_invoice" => "out_refund",
            "out_refund" => "out_invoice",
            "in_invoice" => "in_refund",
            "in_refund" => "in_invoice",
            _ => "entry",
        };
        let base_label = format!("Reversal of {}", orig_name.as_deref().unwrap_or("Entry"));
        let ref_label = match &reason_owned {
            Some(r) => format!("{base_label}: {r}"),
            None => base_label,
        };
        let rev_date = match override_date {
            Some(d) => d,
            None => erp_addon_base::company_calendar::business_date(tx, company_id).await?,
        };

        let mut mvals = RawVals::new();
        mvals.insert("move_type".into(), Cell::Str(rev_type.into()));
        mvals.insert("journal_id".into(), Cell::Id(journal_id));
        mvals.insert("company_id".into(), Cell::Id(company_id));
        mvals.insert("currency_id".into(), Cell::Id(currency_id));
        mvals.insert("date".into(), Cell::Date(rev_date));
        if matches!(
            rev_type,
            "out_refund" | "in_refund" | "out_invoice" | "in_invoice"
        ) {
            mvals.insert("invoice_date".into(), Cell::Date(rev_date));
        }
        if let Some(pid) = partner_id {
            mvals.insert("partner_id".into(), Cell::Id(pid));
        }
        mvals.insert("ref".into(), Cell::Str(ref_label.into()));

        let unchecked_moves = super::unchecked(&env.records("account.move")?);
        let created = unchecked_moves.create(tx, vec![mvals]).await?;
        let rev_id = created
            .ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::invariant("Created reversal entry returned no id."))?;

        let line_rows = tx
            .query(
                "SELECT l.id::int8, l.account_id::int8, l.partner_id::int8, l.name, l.display_type::text, \
                        l.currency_id::int8, COALESCE(l.balance, 0), COALESCE(l.amount_currency, 0), \
                        l.product_id::int8, l.product_uom_id::int8, COALESCE(l.quantity, 0), \
                        COALESCE(l.price_unit, 0), COALESCE(l.discount, 0), \
                        COALESCE(ARRAY(SELECT r.account_tax_id::int8 FROM account_move_line_account_tax_rel r \
                                       WHERE r.account_move_line_id = l.id ORDER BY r.account_tax_id), '{}'), \
                        l.analytic_account_id::int8, l.trading_partner_company_id::int8, \
                        CASE WHEN a.asset_type = 'cip' AND a.state IN ('draft', 'cip') THEN l.asset_id::int8 ELSE NULL END \
                 FROM account_move_line l \
                 LEFT JOIN account_asset a ON a.id = l.asset_id \
                 WHERE l.move_id = $1 ORDER BY l.sequence, l.id",
                &[&source_id],
            )
            .await?;

        let unchecked_lines = super::unchecked(&env.records("account.move.line")?);
        let mut new_lines = Vec::new();
        let mut src_line_ids: Vec<i64> = Vec::new();
        for lrow in line_rows {
            let src_line_id: i64 = lrow.try_get(0).map_err(db)?;
            let display_type: String = lrow.try_get(4).map_err(db)?;
            if rev_type != "entry" && display_type != "product" {
                continue;
            }
            let account_id: Option<i64> = lrow.try_get(1).map_err(db)?;
            let l_partner: Option<i64> = lrow.try_get(2).map_err(db)?;
            let name: Option<String> = lrow.try_get(3).map_err(db)?;
            let l_cur: i64 = lrow.try_get(5).map_err(db)?;
            let balance: Decimal = lrow.try_get(6).map_err(db)?;
            let amt_cur: Decimal = lrow.try_get(7).map_err(db)?;
            let product_id: Option<i64> = lrow.try_get(8).map_err(db)?;
            let uom_id: Option<i64> = lrow.try_get(9).map_err(db)?;
            let qty: Decimal = lrow.try_get(10).map_err(db)?;
            let price_unit: Decimal = lrow.try_get(11).map_err(db)?;
            let discount: Decimal = lrow.try_get(12).map_err(db)?;
            let tax_ids: Vec<i64> = lrow.try_get(13).map_err(db)?;
            let analytic_id: Option<i64> = lrow.try_get(14).map_err(db)?;
            let trading_partner_id: Option<i64> = lrow.try_get(15).map_err(db)?;
            let active_cip_asset_id: Option<i64> = lrow.try_get(16).map_err(db)?;

            let mut lvals = RawVals::new();
            lvals.insert("move_id".into(), Cell::Id(rev_id));
            if let Some(aid) = account_id {
                lvals.insert("account_id".into(), Cell::Id(aid));
            }
            if let Some(pid) = l_partner {
                lvals.insert("partner_id".into(), Cell::Id(pid));
            }
            if let Some(cc_id) = analytic_id {
                lvals.insert("analytic_account_id".into(), Cell::Id(cc_id));
            }
            if let Some(tp_id) = trading_partner_id {
                lvals.insert("trading_partner_company_id".into(), Cell::Id(tp_id));
            }
            if let Some(cip_id) = active_cip_asset_id {
                lvals.insert("asset_id".into(), Cell::Id(cip_id));
            }
            if let Some(lbl) = name {
                lvals.insert("name".into(), Cell::Str(lbl.into()));
            }
            lvals.insert("display_type".into(), Cell::Str(display_type.into()));
            lvals.insert("currency_id".into(), Cell::Id(l_cur));
            if rev_type == "entry" {
                lvals.insert("balance".into(), Cell::Decimal(-balance));
                lvals.insert("amount_currency".into(), Cell::Decimal(-amt_cur));
                if storno {
                    lvals.insert("is_storno".into(), Cell::Bool(true));
                }
            } else {
                if let Some(pid) = product_id {
                    lvals.insert("product_id".into(), Cell::Id(pid));
                }
                if let Some(uid) = uom_id {
                    lvals.insert("product_uom_id".into(), Cell::Id(uid));
                }
                lvals.insert("quantity".into(), Cell::Decimal(qty));
                lvals.insert("price_unit".into(), Cell::Decimal(price_unit));
                lvals.insert("discount".into(), Cell::Decimal(discount));
                if !tax_ids.is_empty() {
                    let rec_ids: Vec<RecId> =
                        tax_ids.into_iter().filter_map(RecId::from_real).collect();
                    lvals.insert_commands(
                        "tax_ids".into(),
                        vec![erp_sdk::Command::Set(rec_ids)],
                    );
                }
            }
            src_line_ids.push(src_line_id);
            new_lines.push(lvals);
        }
        let mut line_pairs: Vec<Value> = Vec::new();
        if !new_lines.is_empty() {
            let created_lines = unchecked_lines.create(tx, new_lines).await?;
            for (src_id, rev_line_id) in src_line_ids.into_iter().zip(created_lines.ids()) {
                line_pairs.push(Value::list([Value::Int(src_id), Value::Int(rev_line_id)]));
            }
        }
        if rev_type != "entry" {
            crate::invoice::sync_lines(tx, &env, &[rev_id]).await?;
        } else {
            super::refresh_entry_totals(tx, &[rev_id]).await?;
            super::check_balanced(tx, &[rev_id]).await?;
        }
        let rev_move = env.records("account.move")?.browse([rev_id])?;
        let _: () = rev_move
            .call(tx, "_after_reverse_move", (source_id, Value::list(line_pairs)))
            .await?;
        Ok(rev_id)
    })
    .await
}

async fn authorize(tx: &mut Transaction, moves: &Records, what: &str) -> Result<()> {
    let env = moves.env().clone();
    if !env.su() && !tx.has_group(&env, POSTER_GROUP).await? {
        return Err(Error::access(format!(
            "Only accounting users may {what} journal entries ({POSTER_GROUP})."
        )));
    }
    moves.check_access(tx, Operation::Write).await
}

fn db(error: impl std::fmt::Display) -> Error {
    Error::db(format!(
        "A journal entry could not be read for posting: {error}"
    ))
}

async fn read_headers(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<Header>> {
    // `journal_numbered_before` stands for the reference's "the journal has a highest name"
    // : a posted entry exists in the journal.
    let rows = tx
        .query(
            "SELECT m.id::int8, m.state::text, m.name::text, m.date, m.company_id::int8, \
             NULLIF(split_part(co.parent_path, '/', 1), '')::int8, \
             m.journal_id::int8, j.company_id::int8, j.code::text, j.type::text, j.active, \
             j.sequence_reset::text, \
             EXISTS (SELECT 1 FROM account_move p WHERE p.journal_id = m.journal_id \
                     AND p.state = 'posted'), \
             cur.active, cur.name::text, m.ref::text, m.move_type::text, \
             m.origin_payment_id IS NOT NULL, COALESCE(co.allow_posted_reset, false) \
             FROM account_move m \
             JOIN res_company co ON co.id = m.company_id \
             JOIN account_journal j ON j.id = m.journal_id \
             JOIN res_currency cur ON cur.id = m.currency_id \
             WHERE m.id = ANY($1::int8[]) ORDER BY m.id",
            &[&ids],
        )
        .await?;
    if rows.len() != ids.len() {
        return Err(Error::missing(
            "Some of the journal entries to post no longer exist.",
        ));
    }
    let mut headers = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(db)?;
        let root: Option<i64> = row.try_get(5).map_err(db)?;
        headers.push(Header {
            id,
            state: row.try_get(1).map_err(db)?,
            name: row.try_get(2).map_err(db)?,
            date: row.try_get(3).map_err(db)?,
            company: row.try_get(4).map_err(db)?,
            root_company: root.ok_or_else(|| {
                Error::internal(format!("The company of entry {id} has no company path."))
            })?,
            journal: row.try_get(6).map_err(db)?,
            journal_company: row.try_get(7).map_err(db)?,
            journal_code: row.try_get(8).map_err(db)?,
            journal_type: row.try_get(9).map_err(db)?,
            journal_active: row.try_get(10).map_err(db)?,
            journal_reset: row.try_get(11).map_err(db)?,
            journal_numbered_before: row.try_get(12).map_err(db)?,
            currency_active: row.try_get(13).map_err(db)?,
            currency_name: row.try_get(14).map_err(db)?,
            reference: row.try_get(15).map_err(db)?,
            move_type: row.try_get(16).map_err(db)?,
            payment: row.try_get(17).map_err(db)?,
            allow_posted_reset: row.try_get(18).map_err(db)?,
        });
    }
    Ok(headers)
}

/// One aggregate over the lines of all the entries: accountable lines, the balance, archived
/// accounts, accounts none of whose companies is the entry's company or one of its parents
/// (reference), and whether a line has taxes or is a tax line (the
/// reference's tax-report impact; tax tags do not exist yet).
async fn line_facts(tx: &mut Transaction, ids: &[i64]) -> Result<BTreeMap<i64, LineFacts>> {
    let text_types: Vec<&str> = TEXT_LINE_TYPES.to_vec();
    let rows = tx
        .query(
            "SELECT m.id::int8, \
             count(l.id) FILTER (WHERE l.display_type IS NULL \
                                 OR l.display_type <> ALL($2::text[]))::int8, \
             CASE WHEN COALESCE(sum(l.balance), 0) <> 0 THEN sum(l.balance)::text END, \
             COALESCE(array_agg(DISTINCT l.account_id::int8) \
                      FILTER (WHERE a.active IS FALSE), '{}'), \
             COALESCE(array_agg(DISTINCT l.account_id::int8) FILTER (WHERE l.account_id IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM account_account_res_company_rel r \
                 WHERE r.account_account_id = l.account_id \
                   AND r.res_company_id = ANY (string_to_array(rtrim(co.parent_path, '/'), '/')::int8[]))), \
               '{}'), \
             COALESCE(bool_or(l.tax_line_id IS NOT NULL OR l.tax_repartition_line_id IS NOT NULL \
               OR EXISTS (SELECT 1 FROM account_move_line_account_tax_rel r \
                          WHERE r.account_move_line_id = l.id)), false), \
             COALESCE(bool_or(a.account_type = 'off_balance') \
                      AND bool_or(a.account_type <> 'off_balance'), false) \
             FROM account_move m \
             JOIN res_company co ON co.id = m.company_id \
             LEFT JOIN account_move_line l ON l.move_id = m.id \
             LEFT JOIN account_account a ON a.id = l.account_id \
             WHERE m.id = ANY($1::int8[]) GROUP BY m.id ORDER BY m.id",
            &[&ids, &text_types],
        )
        .await?;
    let mut facts = BTreeMap::new();
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(db)?;
        facts.insert(
            id,
            LineFacts {
                accountable: row.try_get(1).map_err(db)?,
                unbalanced: row.try_get(2).map_err(db)?,
                archived_accounts: row.try_get(3).map_err(db)?,
                foreign_accounts: row.try_get(4).map_err(db)?,
                taxed: row.try_get(5).map_err(db)?,
                mixed_off_balance: row.try_get(6).map_err(db)?,
            },
        );
    }
    Ok(facts)
}

/// The names of the accounts a refusal lists, in one read.
async fn account_names(
    tx: &mut Transaction,
    env: &Env,
    ids: &BTreeSet<i64>,
) -> Result<BTreeMap<i64, String>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let accounts = env
        .records("account.account")?
        .sudo(true)
        .with_context([("active_test", Value::Bool(false))])
        .browse(ids.iter().copied())?;
    let names = accounts.cells(tx, "name").await?;
    Ok(accounts
        .ids()
        .into_iter()
        .zip(names)
        .map(|(id, name)| {
            let name = match name {
                Cell::Str(name) => name.to_string(),
                _ => format!("account {id}"),
            };
            (id, name)
        })
        .collect())
}

/// The collected refusals of one entry, in the reference's order.
fn validate(
    header: &Header,
    facts: &LineFacts,
    account_names: &BTreeMap<i64, String>,
) -> Vec<String> {
    let mut messages = Vec::new();
    if header.state != "draft" {
        messages.push(format!(
            "The {} (id {}) is not a draft, so it cannot be posted again.",
            header.label(),
            header.id
        ));
    }
    if header.state == "draft"
        && !header.allow_posted_reset
        && let Some(name) = &header.name
    {
        // Under reversal-only policy, a posted entry never returns to draft, so a draft with a
        // name was named by hand. Such a name is not posted: it could repeat a posted number or
        // take one the journal's chain gives out later.
        messages.push(format!(
            "The draft carries the number {name:?}, written by hand: clear it, an entry takes \
             its number from its journal when it is posted."
        ));
    }
    if facts.accountable == 0 {
        messages
            .push("There is nothing to post: the entry has no line with an account.".to_owned());
    }
    if !header.journal_active {
        messages.push(format!(
            "The journal {} is archived; entries are no longer posted in it.",
            header.journal_code
        ));
    }
    if !header.currency_active {
        messages.push(format!(
            "The currency {} is archived; activate it before posting.",
            header.currency_name
        ));
    }
    if !facts.archived_accounts.is_empty() {
        messages.push(format!(
            "A line of this entry uses an archived account ({}); you cannot post it.",
            names(&facts.archived_accounts, account_names)
        ));
    }
    if !facts.foreign_accounts.is_empty() {
        messages.push(format!(
            "These accounts belong to no company of the entry's company tree: {}.",
            names(&facts.foreign_accounts, account_names)
        ));
    }
    if facts.mixed_off_balance {
        messages.push(
            "Off-balance accounts cannot be mixed with ordinary accounts in one journal entry."
                .to_owned(),
        );
    }
    if let Some(difference) = &facts.unbalanced {
        messages.push(format!(
            "Debits and credits differ by {difference}; balance the entry before posting."
        ));
    }
    messages
}

fn names(ids: &[i64], account_names: &BTreeMap<i64, String>) -> String {
    ids.iter()
        .map(|id| {
            account_names
                .get(id)
                .cloned()
                .unwrap_or_else(|| format!("account {id}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The re-dated entries are checked again and a
/// refusal of the new date (a period that is not open) is final. The entries of `taxed` reach
/// the tax report, so the tax lock date binds them too.
///
/// A re-dated entry keeps its company-currency balances: they are not converted again at the new
/// date. Observed: the reference's balance of a plain entry is the amount entered
/// and an invoice converts at its
/// invoice date; neither follows the
/// accounting date. A draft invoice whose date is changed by hand is converted again by the
/// invoice line sync.
async fn accounting_dates(
    tx: &mut Transaction,
    env: &Env,
    headers: &[Header],
    taxed: &BTreeSet<i64>,
) -> Result<Vec<Date>> {
    let companies: Vec<_> = headers
        .iter()
        .map(|h| h.company)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let days = erp_addon_base::company_calendar::business_days(tx, env, &companies).await?;
    let check = |header: &Header, date: Date| {
        let tax = if taxed.contains(&header.id) {
            TaxImpact::Report
        } else {
            TaxImpact::None
        };
        LockCheck::new(header.company, date, header.journal_class(), tax)
    };
    let checks: Vec<LockCheck> = headers
        .iter()
        .map(|h| check(h, h.date))
        .collect::<Result<_>>()?;
    ensure_periods_for(tx, env, &checks).await?;
    let verdicts = check_lock_dates(tx, &checks).await?;
    let mut dates = Vec::with_capacity(headers.len());
    let mut moved = Vec::new();
    for (index, (header, verdict)) in headers.iter().zip(verdicts).enumerate() {
        match verdict {
            LockVerdict::Allowed => dates.push(header.date),
            LockVerdict::Refused(LockRefusal::Dates { violations, .. }) => {
                let latest = violations
                    .iter()
                    .map(|v| v.through())
                    .max()
                    .ok_or_else(|| Error::invariant("a lock refusal names no lock date"))?;
                let reset = SequenceReset::parse(&header.journal_reset)?;
                dates.push(shifted_date(
                    next_day(latest),
                    days.get(&header.company)
                        .ok_or_else(|| {
                            Error::missing(format!("Company {} is unavailable.", header.company))
                        })?
                        .date,
                    reset,
                    header.journal_numbered_before,
                    header.is_sale_document(),
                )?);
                moved.push(index);
            }
            refused => {
                refused.into_result()?;
                return Err(Error::invariant(
                    "a refused lock verdict converted to success",
                ));
            }
        }
    }
    if moved.is_empty() {
        return Ok(dates);
    }
    let rechecks: Vec<LockCheck> = headers
        .iter()
        .zip(&dates)
        .enumerate()
        .filter(|(index, _)| moved.contains(index))
        .map(|(_, (header, date))| check(header, *date))
        .collect::<Result<_>>()?;
    ensure_periods_for(tx, env, &rechecks).await?;
    for verdict in check_lock_dates(tx, &rechecks).await? {
        verdict.into_result()?;
    }
    Ok(dates)
}

/// Posting creates the fiscal periods it needs (a system step, R2): the first entry of
/// a month must not be refused because nobody generated its period. Locks and a date beyond the
/// horizon setting still refuse.
async fn ensure_periods_for(tx: &mut Transaction, env: &Env, checks: &[LockCheck]) -> Result<()> {
    let rows: Vec<(i64, Date)> = checks.iter().map(|c| (c.company, c.date)).collect();
    crate::fiscal::ensure::ensure_for_rows(tx, env, &rows).await?;
    Ok(())
}

pub(crate) fn shifted_date(
    start: Date,
    today: Date,
    reset: SequenceReset,
    numbered_before: bool,
    sale: bool,
) -> Result<Date> {
    let (year, month, _) = start.ymd();
    let (today_year, today_month, _) = today.ymd();
    let month_end = previous_day(add_months(first_of_month(start)?, 1)?);
    let year_end = Date::from_ymd(year, 12, 31)
        .ok_or_else(|| Error::validation(format!("{year} has no 31 December")))?;
    let monthly = !numbered_before || matches!(reset, SequenceReset::Month);
    if sale {
        return Ok(if monthly {
            today.min(month_end)
        } else if reset == SequenceReset::Year {
            today.min(year_end)
        } else {
            start
        });
    }
    let monthly = monthly || reset == SequenceReset::YearRangeMonth;
    Ok(if monthly {
        if (today_year, today_month) > (year, month) {
            month_end
        } else {
            start.max(today)
        }
    } else if reset == SequenceReset::Year {
        if today_year > year {
            year_end
        } else {
            start.max(today)
        }
    } else {
        start
    })
}

fn first_of_month(date: Date) -> Result<Date> {
    let (year, month, _) = date.ymd();
    Date::from_ymd(year, month, 1)
        .ok_or_else(|| Error::validation(format!("{year}-{month:02} has no first day")))
}

/// Lock the headers in id order (1 step 2) and return their state and date. At
/// `REPEATABLE READ` a header another transaction changed and committed since this one began is
/// a serialization failure, retried by the unit of work.
async fn lock_headers(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<(i64, String, Date)>> {
    let rows = tx
        .query(
            "SELECT id::int8, state::text, date FROM account_move \
             WHERE id = ANY($1::int8[]) ORDER BY id FOR NO KEY UPDATE",
            &[&ids],
        )
        .await?;
    if rows.len() != ids.len() {
        return Err(Error::missing(
            "Some of the journal entries no longer exist.",
        ));
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

fn text_cell(text: &str) -> Cell {
    Cell::Str(text.into())
}

/// One statement flips the headers of `pairs` (entry, accounting date) and their lines to `state`
/// and writes the accounting date on both (3 step 5).
async fn set_state(tx: &mut Transaction, pairs: &[(i64, Date)], state: &str) -> Result<()> {
    let (ids, dates): (Vec<i64>, Vec<Date>) = pairs.iter().copied().unzip();
    let uid = tx.env().uid();
    let rows = tx
        .query(
            "WITH v AS (SELECT * FROM unnest($1::int8[], $2::date[]) AS v(id, date)), \
             m AS (UPDATE account_move SET state = $3, date = v.date, write_uid = $4, \
                   write_date = (now() AT TIME ZONE 'UTC') \
                   FROM v WHERE account_move.id = v.id RETURNING account_move.id), \
             l AS (UPDATE account_move_line SET parent_state = $3, date = v.date, write_uid = $4, \
                   write_date = (now() AT TIME ZONE 'UTC') \
                   FROM v WHERE account_move_line.move_id = v.id RETURNING 1) \
             SELECT (SELECT count(*) FROM m)::int8",
            &[&ids, &dates, &state, &uid],
        )
        .await?;
    let changed: i64 = rows
        .first()
        .ok_or_else(|| Error::internal("The state change returned no row."))?
        .try_get(0)
        .map_err(db)?;
    if usize::try_from(changed).ok() != Some(ids.len()) {
        return Err(Error::internal(format!(
            "The state change wrote {changed} of {} journal entries.",
            ids.len()
        )));
    }
    Ok(())
}

/// Number the entries, last in the posting (3 step 7): one declaration read per
/// journal, one calendar read per root company, one allocation for the batch and one statement
/// writing the names. Every entry is unnamed here: a named draft was refused with the
/// validations. Credit notes are numbered in the refund chain of a journal that keeps one.
/// `dates` is in `headers` order.
async fn number(
    tx: &mut Transaction,
    moves: &Records,
    headers: &[Header],
    dates: &[Date],
) -> Result<()> {
    let env = moves.env();
    let unnamed: Vec<(&Header, Date)> = headers
        .iter()
        .zip(dates.iter().copied())
        .filter(|(h, _)| h.name.is_none())
        .collect();
    if unnamed.is_empty() {
        return Ok(());
    }
    let journal_ids: BTreeSet<i64> = unnamed.iter().map(|(h, _)| h.journal).collect();
    let journals = env
        .records("account.journal")?
        .sudo(true)
        .with_context([("active_test", Value::Bool(false))])
        .browse(journal_ids.iter().copied())?;
    let journals = RecordSet::<AccountJournal>::try_from(journals)?;
    let mut patterns: BTreeMap<(i64, &'static str), (&'static str, SequencePattern)> =
        BTreeMap::new();
    for journal in journals.iter() {
        let id = journal
            .as_records()
            .ids()
            .first()
            .copied()
            .ok_or_else(|| Error::invariant("a journal record has no id"))?;
        let declaration = JournalSequence::of(tx, &journal).await?;
        let kinds: BTreeSet<&'static str> = unnamed
            .iter()
            .filter(|(h, _)| h.journal == id)
            .map(|(h, _)| h.sequence_kind())
            .collect();
        for kind in kinds {
            patterns.insert((id, kind), declaration.pattern(kind)?);
        }
    }
    let mut calendars: BTreeMap<i64, FiscalCalendar> = BTreeMap::new();
    for (header, _) in &unnamed {
        if let std::collections::btree_map::Entry::Vacant(slot) =
            calendars.entry(header.root_company)
        {
            slot.insert(FiscalCalendar::read(tx, header.root_company).await?);
        }
    }
    let mut requests = Vec::with_capacity(unnamed.len());
    for (header, date) in &unnamed {
        let (kind, pattern) = patterns
            .get(&(header.journal, header.sequence_kind()))
            .ok_or_else(|| Error::invariant("a journal pattern was not read"))?;
        let calendar = calendars
            .get(&header.root_company)
            .ok_or_else(|| Error::invariant("a fiscal calendar was not read"))?;
        let fiscal_year: FiscalYear = calendar.year_end().fiscal_year_of(*date)?;
        requests.push(NumberRequest {
            document_id: header.id,
            company_id: header.journal_company,
            journal_id: header.journal,
            kind,
            pattern,
            date: *date,
            fiscal_year: Some(fiscal_year),
            reference: header.reference.as_deref(),
        });
    }
    let wait = LockWait::from_settings(tx).await?;
    let assigned = assign_numbers(tx, wait, &requests).await?;
    let ids: Vec<i64> = assigned.iter().map(|a| a.document_id).collect();
    let names: Vec<&str> = assigned.iter().map(|a| a.name.as_str()).collect();
    let written = tx
        .execute(
            "UPDATE account_move SET name = v.name \
             FROM unnest($1::int8[], $2::text[]) AS v(id, name) WHERE account_move.id = v.id",
            &[&ids, &names],
        )
        .await?;
    if usize::try_from(written).ok() != Some(ids.len()) {
        return Err(Error::internal(format!(
            "Numbering wrote {written} of {} entry names.",
            ids.len()
        )));
    }
    // the number in the entry's history; every entry was unnamed before (checked above)
    tx.track_external(
        moves,
        "name",
        assigned
            .iter()
            .map(|a| (a.document_id, Cell::Null, text_cell(&a.name))),
    )
}

async fn invalidate(tx: &mut Transaction, moves: &Records) -> Result<()> {
    moves.modified_external(tx, &["state"]).await?;
    // the display name is computed from the state and the number, which change here by SQL
    tx.invalidate_records(moves, Some(&["state", "date", "name", "display_name"]))
        .await?;
    tx.invalidate_model("account.move.line", Some(&["parent_state", "date"]))
        .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::from_ymd(y, m, day).unwrap()
    }

    #[test]
    fn redating_follows_the_journal_reset() {
        use SequenceReset::{Month, Never, Year};
        // a later month: the end of the start month
        assert_eq!(
            shifted_date(d(2024, 2, 1), d(2026, 11, 16), Month, true, false).unwrap(),
            d(2024, 2, 29)
        );
        // the same month: today, never before the start
        assert_eq!(
            shifted_date(d(2026, 11, 1), d(2026, 11, 16), Month, true, false).unwrap(),
            d(2026, 11, 16)
        );
        assert_eq!(
            shifted_date(d(2026, 11, 20), d(2026, 11, 16), Month, true, false).unwrap(),
            d(2026, 11, 20)
        );
        // a yearly journal: the year end when today is in a later year
        assert_eq!(
            shifted_date(d(2024, 2, 1), d(2026, 11, 16), Year, true, false).unwrap(),
            d(2024, 12, 31)
        );
        // a journal that never numbered an entry follows the monthly rule
        assert_eq!(
            shifted_date(d(2024, 2, 1), d(2026, 11, 16), Year, false, false).unwrap(),
            d(2024, 2, 29)
        );
        // no reset: the start date
        assert_eq!(
            shifted_date(d(2024, 2, 1), d(2026, 11, 16), Never, true, false).unwrap(),
            d(2024, 2, 1)
        );
        // sales documents never go past today
        assert_eq!(
            shifted_date(d(2026, 11, 1), d(2026, 11, 16), Month, true, true).unwrap(),
            d(2026, 11, 16)
        );
        assert_eq!(
            shifted_date(d(2024, 2, 1), d(2026, 11, 16), Month, true, true).unwrap(),
            d(2024, 2, 29)
        );
    }
}
