// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company routing policy. Secret material is confined to the deployment's profile files.
use erp_sdk::{Operation, jobs::EnqueueRequest, prelude::*, serde_json::json};

#[model(name="mail.smtp.relay",description="Outgoing SMTP Relay",addon="mail",
    company_access=direct(field="company_id",branch="exact",empty="deny"),
    keys=[key(name="_company",fields=["company_id"],message="A company has one outgoing SMTP route.")])]
pub struct MailSmtpRelay {
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "cascade"
    )]
    pub company_id: Option<RecId>,
    /// The reserved deployment alias "default" is selected explicitly like any other alias.
    #[field(char, required = true)]
    pub profile: Option<Arc<str>>,
    #[field(char, required = true)]
    pub sender: Option<Arc<str>>,
    #[field(char)]
    pub reply_to: Option<Arc<str>>,
    #[field(char)]
    pub test_recipient: Option<Arc<str>>,
}

#[methods(MailSmtpRelay, addon = "mail")]
impl MailSmtpRelay {
    #[method]
    #[rpc(effect=write)]
    pub async fn action_test_connection(
        this: &RecordSet<MailSmtpRelay>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        probe(this, tx, false).await
    }

    #[method]
    #[rpc(effect=write)]
    pub async fn action_send_test(
        this: &RecordSet<MailSmtpRelay>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        probe(this, tx, true).await
    }
}

async fn probe(this: &RecordSet<MailSmtpRelay>, tx: &mut Transaction, send: bool) -> Result<Value> {
    let row = this.as_records().ensure_one()?;
    row.check_access(tx, Operation::Write).await?;
    let recipient = if send {
        let to = this
            .get(tx, MailSmtpRelay::TEST_RECIPIENT)
            .await?
            .ok_or_else(|| {
                Error::validation("Enter a test recipient before requesting a test email.")
            })?;
        if to.trim().is_empty() {
            return Err(Error::validation(
                "Enter a test recipient before requesting a test email.",
            ));
        }
        Some(to.to_string())
    } else {
        None
    };
    let job = tx
        .enqueue(
            this.env(),
            EnqueueRequest::new("mail.smtp_probe")
                .payload(&json!({"relay":row.ids()[0],"recipient":recipient}))?,
        )
        .await?
        .job();
    Ok(Value::Json(Arc::new(
        json!({"type":"ir.actions.client","tag":"display_notification","params":{
            "type":"info","message":format!("SMTP check queued as job {}. Inspect Job Runs for its result.",job.id())
        }}),
    )))
}
