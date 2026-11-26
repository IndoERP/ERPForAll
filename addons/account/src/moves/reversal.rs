// SPDX-License-Identifier: MIT OR Apache-2.0
//! Credit Note / Entry Reversal wizard (`account.move.reversal`).

use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::{Operation, ValueMap};

use crate::moves::{AccountMove, post};
use crate::reconcile::reconcile_lines;

/// Transient dialog for creating a credit note or reversing a posted journal entry.
#[model(
    name = "account.move.reversal",
    description = "Credit Note / Entry Reversal",
    kind = "transient",
    transient_max_age_seconds = 3600,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountMoveReversal {
    /// Posted entry or invoice being reversed.
    #[field(
        many2one,
        comodel = "account.move",
        required = true,
        ondelete = "cascade",
        string = "Journal Entry"
    )]
    pub move_id: Option<RecId>,
    /// Company of the source entry.
    #[field(
        many2one,
        comodel = "res.company",
        related = "move_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Refund mode: draft credit note (partial) or post & reconcile immediately (full).
    #[field(
        selection,
        selection = [
            ("refund", "Partial Refund (Draft Credit Note)"),
            ("cancel", "Full Refund (Post and Reconcile)"),
        ],
        required = true,
        default = "refund",
        string = "Credit Method"
    )]
    pub refund_method: Option<Arc<str>>,
    /// Reason printed on the credit note reference.
    #[field(char, string = "Reason")]
    pub reason: Option<Arc<str>>,
    /// Reversal / credit note date.
    #[field(date, required = true, string = "Reversal Date")]
    pub date: Option<Date>,
    /// Specific journal for the reversal; defaults to the source entry's journal.
    #[field(
        many2one,
        comodel = "account.journal",
        check_company = true,
        string = "Use Specific Journal"
    )]
    pub journal_id: Option<RecId>,
}

#[methods(AccountMoveReversal, addon = "account")]
impl AccountMoveReversal {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountMoveReversal>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let move_id = match this.env().context().get("default_move_id") {
            Some(Value::Int(id)) => Some(id),
            _ => None,
        };
        let mut company_id = this.env().company_id()?;
        if let Some(mid) = move_id {
            let m = this.env().records("account.move")?.browse([mid])?;
            if let Cell::Id(cid) = m.cell(tx, "company_id").await? {
                company_id = Some(cid);
            }
            if names.iter().any(|n| n == "journal_id")
                && !values.contains_key("journal_id")
                && let Cell::Id(jid) = m.cell(tx, "journal_id").await?
            {
                values.insert("journal_id".to_owned(), Value::Int(jid));
            }
        }
        if names.iter().any(|n| n == "date") && !values.contains_key("date") {
            let date = if let Some(cid) = company_id {
                erp_addon_base::company_calendar::business_date(tx, cid).await?
            } else {
                this.env().context_today()?
            };
            values.insert("date".to_owned(), Value::Date(date));
        }
        Ok(values)
    }

    /// Create the reversal / credit note; when `refund_method == "cancel"`, post and reconcile
    /// against the source invoice so it transitions to Reversed.
    #[method]
    #[rpc(effect = write)]
    pub async fn reverse_moves(
        this: &RecordSet<AccountMoveReversal>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        wizard
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        let move_id = wizard
            .get(tx, AccountMoveReversal::MOVE_ID)
            .await?
            .ok_or_else(|| Error::validation("Choose a posted entry or invoice to reverse."))?
            .get();
        let date = wizard.get(tx, AccountMoveReversal::DATE).await?;
        let journal_id = wizard
            .get(tx, AccountMoveReversal::JOURNAL_ID)
            .await?
            .map(|id| id.get());
        let reason = wizard.get(tx, AccountMoveReversal::REASON).await?;
        let method = wizard
            .get(tx, AccountMoveReversal::REFUND_METHOD)
            .await?
            .unwrap_or_else(|| Arc::from("refund"));

        // Strip wizard `default_*` context keys (such as `default_move_id`) while preserving
        // caller `allowed_company_ids`, `tz`, and `lang`.
        let env = post::strip_default_context(wizard.env());
        let source = env.records("account.move")?.browse([move_id])?;
        let rev_id =
            post::reverse_move_with_options(tx, &source, date, journal_id, reason.as_deref())
                .await?;

        if method.as_ref() == "cancel" {
            let rev_records = env.records("account.move")?.browse([rev_id])?;
            post::post_moves(tx, &rev_records).await?;
            // Reconcile open receivable/payable items shared between source and credit note.
            let rows = tx
                .query(
                    "SELECT l.id::int8, l.account_id::int8 \
                     FROM account_move_line l \
                     JOIN account_account a ON a.id = l.account_id \
                     LEFT JOIN account_open_item o ON o.line_id = l.id \
                     WHERE l.move_id = ANY($1::int8[]) \
                       AND a.reconcile = true \
                       AND l.full_reconcile_id IS NULL \
                       AND COALESCE(o.amount_residual, l.balance) <> 0 \
                     ORDER BY l.account_id, l.id",
                    &[&vec![move_id, rev_id]],
                )
                .await?;
            let mut by_account: std::collections::BTreeMap<i64, Vec<i64>> =
                std::collections::BTreeMap::new();
            for row in rows {
                let lid: i64 = row
                    .try_get(0)
                    .map_err(|e| Error::db(format!("Reversal line id unreadable: {e}")))?;
                let aid: i64 = row
                    .try_get(1)
                    .map_err(|e| Error::db(format!("Reversal account id unreadable: {e}")))?;
                by_account.entry(aid).or_default().push(lid);
            }
            let lines_model = env.records("account.move.line")?;
            for (_, ids) in by_account {
                if ids.len() >= 2 {
                    reconcile_lines(tx, &lines_model.browse(ids)?).await?;
                }
            }
        }

        wizard.as_records().unlink(tx).await?;

        // Decide whether to open the invoice form or the generic entry form.
        let rev_type = env
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
        let mut action_json = json!({
            "type": "ir.actions.act_window",
            "name": if is_invoice { "Credit Note" } else { "Reversal Entry" },
            "res_model": "account.move",
            "res_id": rev_id,
            "target": "current",
            "views": [[false, "form"]],
        });
        if is_invoice {
            let vrows = tx
                .query(
                    "SELECT res_id::int8 FROM ir_model_data \
                     WHERE module = 'account' AND name = 'view_invoice_form' LIMIT 1",
                    &[],
                )
                .await?;
            if let Some(view_id) = vrows.first().and_then(|r| r.try_get::<_, i64>(0).ok())
                && let Some(obj) = action_json.as_object_mut()
            {
                obj.insert("views".to_owned(), json!([[view_id, "form"]]));
            }
        }
        Ok(Value::Json(Arc::new(action_json)))
    }
}

#[methods(AccountMove, addon = "account")]
impl AccountMove {
    /// Opens the Credit Note / Entry Reversal wizard for a posted invoice, bill, or entry.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_open_reversal_wizard(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        let state = rec.get(tx, AccountMove::STATE).await?;
        if state.as_deref() != Some("posted") {
            return Err(Error::user(
                "Only posted entries or invoices can be reversed.",
            ));
        }
        let move_id = rec.as_records().ids()[0];
        let journal_id = rec.get(tx, AccountMove::JOURNAL_ID).await?.map(|j| j.get());
        let mut ctx = erp_sdk::serde_json::Map::new();
        ctx.insert("default_move_id".to_owned(), json!(move_id));
        if let Some(jid) = journal_id {
            ctx.insert("default_journal_id".to_owned(), json!(jid));
        }
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Credit Note",
            "res_model": "account.move.reversal",
            "target": "new",
            "views": [[false, "form"]],
            "context": ctx,
        }))))
    }
}
