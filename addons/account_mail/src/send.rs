// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::AccountMoveThread;
use erp_addon_account::moves::AccountMove;
use erp_addon_mail::external_compose::{self, DocumentPdf, Draft};
use erp_sdk::prelude::*;
use erp_sdk::{Context, Operation, serde_json::json};

#[model(
    name = "account.invoice.email",
    description = "Email Customer Invoice",
    addon = "account_mail",
    kind = "transient",
    transient_max_age_seconds = 3600,
    company_access = parent(edge = "move_id")
)]
pub struct InvoiceEmail {
    #[field(
        many2one,
        comodel = "account.move",
        required = true,
        ondelete = "cascade",
        string = "Invoice"
    )]
    pub move_id: Option<RecId>,
    #[field(char, required = true, string = "To")]
    pub email_to: Option<Arc<str>>,
    #[field(char, required = true)]
    pub subject: Option<Arc<str>>,
    #[field(text, required = true, string = "Message")]
    pub body: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "ir.attachment",
        relation = "account_invoice_email_attachment_rel",
        column1 = "wizard_id",
        column2 = "attachment_id",
        string = "Additional files"
    )]
    pub attachment_ids: Vec<RecId>,
}

async fn customer_invoice(tx: &mut Transaction, record: &Records) -> Result<i64> {
    record.check_access(tx, Operation::Write).await?;
    if record.cell(tx, "move_type").await? != Cell::Str("out_invoice".into())
        || record.cell(tx, "state").await? != Cell::Str("posted".into())
    {
        return Err(Error::user(
            "Post the customer invoice before sending it by email.",
        ));
    }
    match record.cell(tx, "partner_id").await? {
        Cell::Id(id) => Ok(id),
        _ => Err(Error::validation("Choose a customer for the invoice.")),
    }
}

#[methods(AccountMove, addon = "account_mail")]
impl AccountMoveThread {
    #[method]
    #[rpc(effect = read)]
    async fn action_open_invoice_email(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let record = this.ensure_one()?.as_records();
        let partner_id = customer_invoice(tx, record).await?;
        let partner = this.env().records("res.partner")?.browse([partner_id])?;
        let lang = external_compose::partner_language(tx, Some(partner_id)).await?;
        let email = match partner.cell(tx, "email").await? {
            Cell::Str(text) => text.to_string(),
            Cell::Null => String::new(),
            _ => return Err(Error::invariant("Invalid customer email.")),
        };
        let Cell::Str(name) = record.cell(tx, "name").await? else {
            return Err(Error::validation("The customer invoice needs a number."));
        };
        let Cell::Decimal(total) = record.cell(tx, "amount_total").await? else {
            return Err(Error::invariant("Invalid invoice total."));
        };
        let (subject, body) = external_compose::document_text(
            tx,
            "customer_invoice",
            lang.as_deref(),
            &name,
            &total.to_string(),
        )
        .await?;
        Ok(Value::Json(Arc::new(
            json!({"type":"ir.actions.act_window","name":"Email customer invoice","res_model":"account.invoice.email","target":"new","views":[[false,"form"]],"context":{"default_move_id":record.ids()[0],"default_email_to":email,"default_subject":subject,"default_body":body}}),
        )))
    }
}

#[methods(InvoiceEmail, addon = "account_mail")]
impl InvoiceEmail {
    #[method]
    #[rpc(effect = write)]
    async fn action_preview_pdf(
        this: &RecordSet<InvoiceEmail>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        let move_id = wizard
            .get(tx, InvoiceEmail::MOVE_ID)
            .await?
            .ok_or_else(|| Error::validation("Choose an invoice."))?
            .get();
        let record = wizard.env().records("account.move")?.browse([move_id])?;
        let partner = customer_invoice(tx, &record).await?;
        let lang = external_compose::partner_language(tx, Some(partner)).await?;
        erp_addon_mail::external_preview::open(
            tx,
            &record,
            DocumentPdf {
                wizard: wizard.as_records(),
                report: "account.action_report_invoice",
                language: lang.as_deref(),
            },
        )
        .await
    }
    #[method]
    #[rpc(effect = write)]
    async fn action_send_email(
        this: &RecordSet<InvoiceEmail>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        wizard
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        wizard.as_records().lock_for_update(tx, true).await?;
        let id = wizard
            .get(tx, InvoiceEmail::MOVE_ID)
            .await?
            .ok_or_else(|| Error::validation("Choose an invoice."))?
            .get();
        let env = wizard.env().with_context_replaced(Context::new());
        let record = env.records("account.move")?.browse([id])?;
        record.lock_for_update(tx, true).await?;
        let partner = customer_invoice(tx, &record).await?;
        let lang = external_compose::partner_language(tx, Some(partner)).await?;
        let email = wizard
            .get(tx, InvoiceEmail::EMAIL_TO)
            .await?
            .unwrap_or_default();
        let subject = wizard
            .get(tx, InvoiceEmail::SUBJECT)
            .await?
            .unwrap_or_default();
        let body = wizard
            .get(tx, InvoiceEmail::BODY)
            .await?
            .unwrap_or_default();
        let attachment_ids = wizard
            .as_records()
            .related(tx, "attachment_ids")
            .await?
            .ids();
        external_compose::send(
            tx,
            &record,
            Draft {
                email_to: &email,
                subject: &subject,
                body: &body,
                attachment_ids,
                document: Some(DocumentPdf {
                    wizard: wizard.as_records(),
                    report: "account.action_report_invoice",
                    language: lang.as_deref(),
                }),
            },
        )
        .await?;
        wizard.as_records().unlink(tx).await?;
        Ok(Value::Json(Arc::new(
            json!({"type":"ir.actions.act_window_close"}),
        )))
    }
}
