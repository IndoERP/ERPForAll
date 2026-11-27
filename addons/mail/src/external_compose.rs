// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit external mail shares only the composer text and its selected documents.
use erp_addon_base::{attachments, report_job, report_print};
use erp_sdk::prelude::*;
use erp_sdk::{Operation, Records};

use crate::outgoing::{OutgoingAttachment, OutgoingRequest, enqueue};
use crate::{Body, PostRequest, PostSubtype, PostType, Posted};

#[model(name = "mail.document.template", description = "Document Email Template", addon = "mail",
    keys = [key(name = "_unique_code", fields = ["code"], message = "Use a unique document template code.")])]
pub struct DocumentTemplate {
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    #[field(char, required = true)]
    pub code: Option<Arc<str>>,
    #[field(char, required = true, translate = true)]
    pub subject: Option<Arc<str>>,
    #[field(text, required = true, translate = true)]
    pub body: Option<Arc<str>>,
}

/// Resolve an editable plain-text template in the recipient's language.
/// Only the named placeholders are interpreted; record text is never evaluated.
pub async fn document_text(
    tx: &mut Transaction,
    code: &str,
    language: Option<&str>,
    name: &str,
    total: &str,
) -> Result<(String, String)> {
    let env = tx.env();
    let env = match language {
        Some(lang) => env.with_context([("lang", Value::Str(lang.into()))]),
        None => env,
    };
    let templates = env
        .records("mail.document.template")?
        .search(tx, &Domain::condition("code", Operator::Eq, code)?)
        .await?;
    let template = templates.ensure_one()?;
    let render = |text: &str| {
        text.split("{document}")
            .map(|part| part.replace("{total}", total))
            .collect::<Vec<_>>()
            .join(name)
    };
    let subject = text_field(tx, template, "subject").await?;
    let body = text_field(tx, template, "body").await?;
    Ok((render(&subject), render(&body)))
}

/// Return an optional contact language, falling back to the sender's language if unset.
pub async fn partner_language(
    tx: &mut Transaction,
    partner: Option<i64>,
) -> Result<Option<String>> {
    let Some(id) = partner else { return Ok(None) };
    let partner = tx.env().records("res.partner")?.browse([id])?;
    partner.check_access(tx, Operation::Read).await?;
    match partner.cell(tx, "lang").await? {
        Cell::Str(lang) if !lang.is_empty() => Ok(Some(lang.to_string())),
        Cell::Null | Cell::Str(_) => Ok(None),
        _ => Err(Error::invariant("Invalid partner language.")),
    }
}

async fn text_field(tx: &mut Transaction, record: &Records, field: &str) -> Result<String> {
    match record.cell(tx, field).await? {
        Cell::Str(text) => Ok(text.to_string()),
        _ => Err(Error::validation(format!("Enter a value for {field}."))),
    }
}

/// An optional approved business report in the contact's language.
pub struct DocumentPdf<'a> {
    pub wizard: &'a Records,
    pub report: &'a str,
    pub language: Option<&'a str>,
}

pub(crate) async fn render_pdf(
    tx: &mut Transaction,
    record: &Records,
    report: &report_print::PrintReport,
    language: &str,
) -> Result<OutgoingAttachment> {
    let ids = record.ids();
    let filename = report_print::file_name(tx, report, &ids, "pdf").await?;
    let content = report_print::prepare_for_language(tx, report, &ids, Some(language)).await?;
    Ok(OutgoingAttachment {
        filename,
        mime_type: "application/pdf".into(),
        bytes: report_job::layout(content).await?,
    })
}

/// The explicit fields approved by the sender. The caller locks the wizard before sending.
pub struct Draft<'a> {
    pub email_to: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
    pub attachment_ids: Vec<i64>,
    pub document: Option<DocumentPdf<'a>>,
}

/// Post once and persist the immutable bodies and attachments in the same business transaction.
pub async fn send(tx: &mut Transaction, record: &Records, draft: Draft<'_>) -> Result<()> {
    record.ensure_one()?;
    record.check_access(tx, Operation::Write).await?;
    if !record.env().su() && !tx.has_group(record.env(), "base.group_user").await? {
        return Err(Error::access(
            "Only internal users may email business records.",
        ));
    }
    if !record.spec().inherits("mail.thread") || record.model_name() == "discuss.channel" {
        return Err(Error::validation(
            "Choose a business record with a message thread.",
        ));
    }
    let company = if record.spec().field("company_id").is_some() {
        match record.cell(tx, "company_id").await? {
            Cell::Id(id) => Some(id),
            Cell::Null => None,
            _ => return Err(Error::invariant("Invalid document company.")),
        }
    } else {
        None
    }
    .or(record.env().company_id()?)
    .ok_or_else(|| Error::user("Choose a sender company."))?;
    let recipients = draft
        .email_to
        .split(',')
        .map(|s| s.trim().to_owned())
        .collect::<Vec<_>>();
    if recipients.is_empty()
        || recipients.iter().any(String::is_empty)
        || draft.subject.trim().is_empty()
        || draft.body.trim().is_empty()
    {
        return Err(Error::validation(
            "Enter a recipient, subject, and message.",
        ));
    }
    tx.savepoint(async |tx| {
        let mut files = Vec::new();
        if let Some(document) = draft.document {
            files.push(crate::external_preview::attachment(tx, record, document).await?);
        }
        let mut request = PostRequest::new(Body::Text(draft.body.to_owned()));
        request.subject = Some(draft.subject.to_owned());
        request.message_type = PostType::EmailOutgoing;
        request.external_delivery = true;
        request.subtype = PostSubtype::Comment;
        request.attachment_ids.clone_from(&draft.attachment_ids);
        let posted: Posted = record.call(tx, "message_post", (request,)).await?;
        // Posting first adopts and verifies every selected attachment belongs to this record.
        let mut remaining = 20 * 1024 * 1024u64;
        for file in &files {
            remaining = remaining
                .checked_sub(file.bytes.len() as u64)
                .ok_or_else(|| Error::validation("Email attachments exceed 20 MiB."))?;
        }
        for id in draft.attachment_ids {
            let handle = attachments::open(tx, id).await?;
            let bytes = handle.read_to_vec(remaining).await?;
            remaining -= bytes.len() as u64;
            let rows = tx
                .query("SELECT name FROM ir_attachment WHERE id=$1::bigint", &[&id])
                .await?;
            let filename = rows
                .first()
                .and_then(|r| r.try_get::<_, String>(0).ok())
                .ok_or_else(|| Error::invariant("Missing email attachment filename."))?;
            files.push(OutgoingAttachment {
                filename,
                mime_type: handle.mimetype().to_owned(),
                bytes,
            });
        }
        enqueue(
            tx,
            OutgoingRequest {
                message_id: posted.message_id,
                company_id: company,
                recipients: &recipients,
                subject: draft.subject,
                body: draft.body,
                attachments: &files,
            },
        )
        .await?;
        Ok(())
    })
    .await
}

#[model(
    name = "mail.external.compose",
    description = "Send External Email",
    addon = "mail",
    kind = "transient",
    transient_max_age_seconds = 3600
)]
pub struct ExternalCompose {
    #[field(char, required = true)]
    pub model: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "model", required = true)]
    pub res_id: i64,
    #[field(
        many2many,
        comodel = "res.partner",
        relation = "mail_external_compose_recipient_rel",
        column1 = "compose_id",
        column2 = "partner_id",
        string = "Contacts"
    )]
    pub recipient_ids: Vec<RecId>,
    #[field(char, string = "Other email addresses")]
    pub email_to: Option<Arc<str>>,
    #[field(char, required = true)]
    pub subject: Option<Arc<str>>,
    #[field(text, required = true)]
    pub body: Option<Arc<str>>,
    #[field(
        many2many,
        comodel = "ir.attachment",
        relation = "mail_external_compose_attachment_rel",
        column1 = "compose_id",
        column2 = "attachment_id",
        string = "Files from this record"
    )]
    pub attachment_ids: Vec<RecId>,
}

#[methods(ExternalCompose, addon = "mail")]
impl ExternalCompose {
    #[method]
    #[rpc(effect = write)]
    async fn action_send_email(
        this: &RecordSet<ExternalCompose>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?.as_records();
        wizard.check_access(tx, Operation::Write).await?;
        wizard.lock_for_update(tx, true).await?;
        let model = text_field(tx, wizard, "model").await?;
        let id = this.get(tx, ExternalCompose::RES_ID).await?;
        let record = wizard
            .env()
            .with_context_replaced(erp_sdk::Context::new())
            .records(&model)?
            .browse([id])?;
        let subject = text_field(tx, wizard, "subject").await?;
        let body = text_field(tx, wizard, "body").await?;
        let contacts = wizard.related(tx, "recipient_ids").await?;
        contacts.check_access(tx, Operation::Read).await?;
        let mut addresses = contacts
            .read(tx, &["email"])
            .await?
            .into_iter()
            .map(|row| match row.get("email") {
                Some(Value::Str(email)) if !email.trim().is_empty() => Ok(email.to_string()),
                _ => Err(Error::user("Each selected contact needs an email address.")),
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(email) = this
            .get(tx, ExternalCompose::EMAIL_TO)
            .await?
            .filter(|s| !s.trim().is_empty())
        {
            addresses.push(email.to_string());
        }
        let email = addresses.join(",");
        let attachments = wizard.related(tx, "attachment_ids").await?.ids();
        send(
            tx,
            &record,
            Draft {
                email_to: &email,
                subject: &subject,
                body: &body,
                attachment_ids: attachments,
                document: None,
            },
        )
        .await?;
        wizard.unlink(tx).await?;
        Ok(Value::Json(Arc::new(
            erp_sdk::serde_json::json!({"type":"ir.actions.act_window_close"}),
        )))
    }
}
