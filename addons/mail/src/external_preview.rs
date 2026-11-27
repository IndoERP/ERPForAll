// SPDX-License-Identifier: MIT OR Apache-2.0
//! A wizard preview retains the rendered bytes the operator approved.
use crate::external_compose::{DocumentPdf, render_pdf};
use crate::outgoing::OutgoingAttachment;
use erp_addon_base::report_print;
use erp_sdk::prelude::*;
use erp_sdk::{Context, Operation, serde_json::json};

erp_sdk::rule_operator!(
    /// A PDF preview follows its business record's current read rules.
    pub EMAIL_PREVIEW_ACCESS = "email_preview_access" {
        addon: "mail", model: "mail.document.preview", field: "res_id", inherits: "mail.thread", operations: [Read],
    }
);

#[model(name = "mail.document.preview", description = "Email PDF Preview", addon = "mail",
    kind = "transient", transient_max_age_seconds = 3600, rec_name = "filename",
    keys = [key(name = "_draft_version", fields = ["wizard_model","wizard_id","model","res_id","report","language"], message = "This draft already has a PDF preview.")])]
pub struct EmailPreview {
    #[field(char, required = true)]
    pub wizard_model: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "wizard_model", required = true)]
    pub wizard_id: i64,
    #[field(char, required = true)]
    pub model: Option<Arc<str>>,
    #[field(many2one_reference, model_field = "model", required = true)]
    pub res_id: i64,
    #[field(char, required = true)]
    pub report: Option<Arc<str>>,
    #[field(char, required = true)]
    pub language: Option<Arc<str>>,
    #[field(char, required = true)]
    pub filename: Option<Arc<str>>,
    #[field(binary, required = true)]
    pub content: Option<Blob>,
}

async fn checked_preview(
    tx: &mut Transaction,
    record: &Records,
    document: &DocumentPdf<'_>,
) -> Result<Records> {
    let wizard = document.wizard.ensure_one()?;
    wizard.check_access(tx, Operation::Write).await?;
    wizard.lock_for_update(tx, true).await?;
    record.ensure_one()?;
    record.check_access(tx, Operation::Write).await?;
    let reference = report_print::find(tx, document.report).await?;
    report_print::check_records(tx, &reference, Some(record.model_name()), &record.ids()).await?;
    let wizard_id = wizard.ids()[0];
    let sender;
    let lang = match document.language {
        Some(lang) => lang,
        None => {
            sender = erp_sdk::auth::user_context(tx, wizard.env().user_id())
                .await?
                .ok_or_else(|| Error::missing("The sender account no longer exists."))?;
            sender.lang()
        }
    };
    let env = tx.env().with_context_replaced(Context::new());
    let id = record.ids()[0];
    let domain = Domain::and([
        Domain::condition("wizard_model", Operator::Eq, wizard.model_name())?,
        Domain::condition("wizard_id", Operator::Eq, wizard_id)?,
        Domain::condition("model", Operator::Eq, record.model_name())?,
        Domain::condition("res_id", Operator::Eq, id)?,
        Domain::condition("report", Operator::Eq, document.report)?,
        Domain::condition("language", Operator::Eq, lang)?,
    ]);
    let previews = env.records("mail.document.preview")?;
    let existing = previews.search(tx, &domain).await?;
    if !existing.ids().is_empty() {
        return Ok(existing);
    }
    let file = render_pdf(tx, record, &reference, lang).await?;
    if file.bytes.len() > 20 * 1024 * 1024 {
        return Err(Error::validation(
            "The PDF exceeds the 20 MiB email attachment limit.",
        ));
    }
    let vals = RawVals::new()
        .set_cell("wizard_model", Cell::Str(wizard.model_name().into()))
        .set_cell("wizard_id", Cell::Id(wizard_id))
        .set_cell("model", Cell::Str(record.model_name().into()))
        .set_cell("res_id", Cell::Id(id))
        .set_cell("report", Cell::Str(document.report.into()))
        .set_cell("language", Cell::Str(lang.into()))
        .set_cell("filename", Cell::Str(file.filename.into()))
        .set_cell("content", Cell::Bytes(file.bytes.into()));
    let saved = env
        .sudo(true)
        .records("mail.document.preview")?
        .create(tx, vec![vals])
        .await?;
    previews.browse(saved.ids())
}

/// Open the exact PDF subsequently used by this draft.
pub async fn open(
    tx: &mut Transaction,
    record: &Records,
    document: DocumentPdf<'_>,
) -> Result<Value> {
    let preview = checked_preview(tx, record, &document).await?;
    Ok(Value::Json(Arc::new(
        json!({"type":"ir.actions.act_window","name":"Preview PDF","res_model":"mail.document.preview","res_id":preview.ids()[0],"target":"new","views":[[false,"form"]]}),
    )))
}

pub(crate) async fn attachment(
    tx: &mut Transaction,
    record: &Records,
    document: DocumentPdf<'_>,
) -> Result<OutgoingAttachment> {
    let preview = checked_preview(tx, record, &document).await?;
    let Cell::Bytes(content) = preview.cell(tx, "content").await? else {
        return Err(Error::invariant("Missing document PDF."));
    };
    let Cell::Str(filename) = preview.cell(tx, "filename").await? else {
        return Err(Error::invariant("Missing PDF filename."));
    };
    Ok(OutgoingAttachment {
        filename: filename.to_string(),
        mime_type: "application/pdf".into(),
        bytes: content.as_bytes().to_vec(),
    })
}
