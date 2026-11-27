// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicitly authorized external email. Each frozen message has one durable job per recipient.

mod worker;
pub(crate) use worker::link_kind;
pub use worker::{Delivery, Envelope, PreparedSender, Transport, TransportService};

use erp_sdk::jobs::EnqueueRequest;
use erp_sdk::prelude::*;
use erp_sdk::{Operation, serde_json::json};
use std::collections::BTreeSet;

pub const KIND: &str = "mail.send_external";

#[model(name = "mail.outgoing", description = "Outgoing Email", addon = "mail",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(name = "_message_recipient_unique", fields = ["message_id", "recipient"], message = "An email is queued once per recipient.")],
    indexes = [("_queue", "(company_id,state,id)")])]
pub struct MailOutgoing {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "cascade"
    )]
    pub company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "mail.message",
        required = true,
        ondelete = "cascade"
    )]
    pub message_id: Option<RecId>,
    #[field(char, required = true)]
    pub recipient: Option<Arc<str>>,
    #[field(char, required = true)]
    pub subject: Option<Arc<str>>,
    #[field(text, groups = ".")]
    pub body: Option<Arc<str>>,
    #[field(char)]
    pub source_model: Option<Arc<str>>,
    #[field(bigint)]
    pub source_id: erp_sdk::BigInt,
    #[field(selection, selection = [("pending","Pending"),("sending","Submitting to relay"),("accepted","Accepted by relay"),("failed","Failed"),("uncertain","Outcome uncertain")], required = true)]
    pub state: Option<Arc<str>>,
    #[field(integer)]
    pub attempts: i32,
    #[field(char, index = true)]
    pub job_uuid: Option<Arc<str>>,
    #[field(char, required = true)]
    pub smtp_message_id: Option<Arc<str>>,
    #[field(char)]
    pub last_error: Option<Arc<str>>,
    #[field(
        one2many,
        comodel = "mail.outgoing.attempt",
        inverse_name = "outgoing_id"
    )]
    pub attempt_ids: Vec<RecId>,
}

#[model(name = "mail.outgoing.attempt", description = "Outgoing Email Attempt", addon = "mail",
    company_access = parent(edge = "outgoing_id"),
    keys = [key(name = "_job_attempt", fields = ["job_uuid","attempt"], message = "Each job attempt is recorded once.")])]
pub struct MailOutgoingAttempt {
    #[field(
        many2one,
        comodel = "mail.outgoing",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub outgoing_id: Option<RecId>,
    #[field(char, required = true)]
    pub job_uuid: Option<Arc<str>>,
    #[field(integer, required = true)]
    pub attempt: i32,
    #[field(datetime, required = true)]
    pub started: Option<DateTime>,
    #[field(char, required = true)]
    pub outcome: Option<Arc<str>>,
    #[field(char)]
    pub detail: Option<Arc<str>>,
}

#[model(name = "mail.outgoing.attachment", description = "Outgoing Email Attachment Snapshot", addon = "mail",
    company_access = parent(edge = "outgoing_id"))]
pub struct MailOutgoingAttachment {
    #[field(
        many2one,
        comodel = "mail.outgoing",
        required = true,
        ondelete = "cascade",
        index = true
    )]
    pub outgoing_id: Option<RecId>,
    #[field(char, required = true)]
    pub filename: Option<Arc<str>>,
    #[field(char, required = true)]
    pub mime_type: Option<Arc<str>>,
    #[field(binary, required = true, groups = ".")]
    pub content: Option<Blob>,
}

pub struct OutgoingRequest<'a> {
    pub message_id: i64,
    pub company_id: i64,
    pub recipients: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub attachments: &'a [OutgoingAttachment],
}

pub struct OutgoingAttachment {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

fn recipients(request: &OutgoingRequest<'_>) -> Result<BTreeSet<String>> {
    if request.recipients.is_empty()
        || request.recipients.len() > 100
        || request.subject.trim().is_empty()
        || request.subject.len() > 998
        || request.subject.chars().any(char::is_control)
        || request.body.len() > crate::MAX_BODY_BYTES
        || request.attachments.len() > crate::MAX_ATTACHMENTS
    {
        return Err(Error::validation(
            "Invalid outgoing email subject, recipients, body or attachment count.",
        ));
    }
    let mut total = 0usize;
    for file in request.attachments {
        total = total.saturating_add(file.bytes.len());
        if total > 20 * 1024 * 1024
            || file.filename.trim().is_empty()
            || file.filename.len() > 255
            || file.filename.chars().any(char::is_control)
            || file.filename.contains(['/', '\\'])
            || file.mime_type.len() > 127
            || !file.mime_type.contains('/')
            || !file
                .mime_type
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-/".contains(&b))
        {
            return Err(Error::validation(
                "Invalid outgoing attachment or attachments exceed 20 MiB.",
            ));
        }
    }
    request
        .recipients
        .iter()
        .map(|address| {
            let (local, domain) = address.rsplit_once('@').ok_or_else(invalid_address)?;
            if local.is_empty()
                || domain.is_empty()
                || local.len() > 64
                || address.len() > 254
                || !local
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&b))
                || local.starts_with('.')
                || local.ends_with('.')
                || local.contains("..")
                || domain.split('.').any(|s| {
                    s.is_empty()
                        || s.len() > 63
                        || s.starts_with('-')
                        || s.ends_with('-')
                        || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                return Err(invalid_address());
            }
            Ok(format!("{local}@{}", domain.to_ascii_lowercase()))
        })
        .collect()
}

fn invalid_address() -> Error {
    Error::validation("Enter a valid outgoing email address.")
}

pub(super) fn invalid_row() -> Error {
    Error::invariant("Invalid outgoing email record.")
}

/// Queue an authorized record-thread message together with its approved attachment bytes.
/// Call in the transaction that posts the message. A repeat returns the original frozen deliveries.
pub async fn enqueue(tx: &mut Transaction, request: OutgoingRequest<'_>) -> Result<Vec<i64>> {
    let addresses = recipients(&request)?;
    let env = tx.env();
    if env.company_id()? != Some(request.company_id) {
        return Err(Error::access("Select the company that owns this email."));
    }
    tx.savepoint(async |tx| enqueue_message(tx, &env, &request, &addresses).await)
        .await
}

async fn enqueue_message(
    tx: &mut Transaction,
    env: &Env,
    request: &OutgoingRequest<'_>,
    addresses: &BTreeSet<String>,
) -> Result<Vec<i64>> {
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT msg.model::text, msg.res_id::bigint
           FROM mail_message msg LEFT JOIN mail_message_subtype st ON st.id=msg.subtype_id
          WHERE msg.id=$1::bigint AND msg.message_type='email_outgoing'
            AND NOT COALESCE(msg.is_internal,false) AND NOT COALESCE(st.internal,false)
          FOR UPDATE OF msg",
            &[&request.message_id],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::access("Only explicitly addressed outgoing messages can be emailed.")
    })?;
    let model: String = row.try_get(0).map_err(|_| invalid_row())?;
    let record: i64 = row.try_get(1).map_err(|_| invalid_row())?;
    if model == "discuss.channel" {
        return Err(Error::access(
            "Private conversations cannot be emailed externally.",
        ));
    }
    env.records(&model)?
        .browse([record])?
        .check_access(tx, Operation::Write)
        .await?;
    let existing = tx.query(
        "SELECT id::bigint, recipient::text FROM mail_outgoing WHERE message_id=$1::bigint ORDER BY id",
        &[&request.message_id],
    ).await?;
    if !existing.is_empty() {
        let saved = existing
            .iter()
            .map(|r| r.try_get::<_, String>(1).map_err(|_| invalid_row()))
            .collect::<Result<BTreeSet<_>>>()?;
        if &saved != addresses {
            return Err(Error::validation(
                "A queued message already has a different recipient list.",
            ));
        }
        return existing
            .iter()
            .map(|r| r.try_get(0).map_err(|_| invalid_row()))
            .collect();
    }

    let recipients: Vec<_> = addresses.iter().cloned().collect();
    let rows = tx.query(
        "INSERT INTO mail_outgoing (message_id, company_id, source_model, source_id, recipient, subject, body, state, attempts,smtp_message_id)
         SELECT $1::bigint,$2::bigint,$3::text,$4::bigint,address,$6::text,$7::text,'pending',0,uuidv7()::text || '@erpforall.invalid' FROM unnest($5::text[]) AS address
         RETURNING id::bigint",
        &[&request.message_id,&request.company_id,&model,&record,&recipients,&request.subject,&request.body],
    ).await?;
    let ids = rows
        .iter()
        .map(|row| row.try_get::<_, i64>(0).map_err(|_| invalid_row()))
        .collect::<Result<Vec<_>>>()?;
    let first = ids.first().ok_or_else(invalid_row)?;
    // A shared set of immutable attachment bytes per message, even with many recipients.
    for file in request.attachments {
        tx.execute(
            "INSERT INTO mail_outgoing_attachment (outgoing_id, filename, mime_type, content) VALUES ($1::bigint,$2::text,$3::text,$4::bytea)",
            &[first, &file.filename, &file.mime_type, &file.bytes],
        ).await?;
    }
    let requests = ids
        .iter()
        .map(|id| EnqueueRequest::new(KIND).payload(&json!({"id":id})))
        .collect::<Result<Vec<_>>>()?;
    let jobs = tx.enqueue_many(env, requests).await?;
    let uuids = jobs
        .iter()
        .map(|j| j.job().uuid().to_string())
        .collect::<Vec<_>>();
    tx.execute(
        "UPDATE mail_outgoing m SET job_uuid=p.job_uuid FROM unnest($1::bigint[],$2::text[]) p(id,job_uuid) WHERE m.id=p.id",
        &[&ids,&uuids],
    ).await?;
    Ok(ids)
}

async fn arm(tx: &mut Transaction, env: &Env, id: i64) -> Result<()> {
    let job = tx
        .enqueue(env, EnqueueRequest::new(KIND).payload(&json!({"id":id}))?)
        .await?
        .job();
    tx.execute(
        "UPDATE mail_outgoing SET job_uuid=$2::text WHERE id=$1::bigint",
        &[&id, &job.uuid().to_string()],
    )
    .await?;
    Ok(())
}

#[methods(MailOutgoing, addon = "mail")]
impl MailOutgoing {
    #[method]
    #[rpc(effect=read)]
    pub async fn action_open_document(
        this: &RecordSet<MailOutgoing>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records
            .ensure_one()?
            .check_access(tx, Operation::Read)
            .await?;
        let Cell::Str(model) = records.cell(tx, "source_model").await? else {
            return Err(invalid_row());
        };
        let id = records.cell(tx, "source_id").await?;
        let Cell::BigInt(id) = id else {
            return Err(invalid_row());
        };
        open_form(tx, this.env(), &model, id.get()).await
    }

    #[method]
    #[rpc(effect=read)]
    pub async fn action_open_job(
        this: &RecordSet<MailOutgoing>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records();
        records
            .ensure_one()?
            .check_access(tx, Operation::Read)
            .await?;
        let id = records.ids()[0];
        let rows=tx.query(
            "SELECT 'ir.job'::text,j.id::bigint FROM mail_outgoing m JOIN erp_job j ON j.job_uuid=m.job_uuid::uuid WHERE m.id=$1::bigint
             UNION ALL SELECT 'ir.job.run'::text,r.id::bigint FROM mail_outgoing m JOIN erp_job_run r ON r.job_uuid=m.job_uuid::uuid WHERE m.id=$1::bigint LIMIT 1",
            &[&id],
        ).await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::user("The job history has expired."))?;
        open_form(
            tx,
            this.env(),
            row.try_get(0).map_err(|_| invalid_row())?,
            row.try_get(1).map_err(|_| invalid_row())?,
        )
        .await
    }

    /// Explicit review is required for an uncertain delivery; the UI explains duplicate risk.
    #[method]
    #[rpc(effect=write)]
    pub async fn action_retry(
        this: &RecordSet<MailOutgoing>,
        tx: &mut Transaction,
    ) -> Result<bool> {
        let env = this.env();
        if !env.su() && !tx.has_group(env, "base.group_system").await? {
            return Err(Error::access(
                "Outgoing email administration requires Settings access.",
            ));
        }
        this.as_records().check_access(tx, Operation::Read).await?;
        for id in this.as_records().ids() {
            let rows=tx.query("UPDATE mail_outgoing m SET state='pending',last_error=NULL,attempts=0 WHERE id=$1::bigint AND (state IN ('pending','sending','uncertain','failed') AND NOT EXISTS (SELECT FROM erp_job j WHERE j.job_uuid=m.job_uuid::uuid AND j.state IN ('running','ready','held'))) RETURNING id",&[&id]).await?;
            if rows.is_empty() {
                return Err(Error::user(
                    "This delivery is already active or accepted by its relay.",
                ));
            }
            arm(tx, env, id).await?;
        }
        Ok(true)
    }
}

async fn open_form(tx: &mut Transaction, env: &Env, model: &str, id: i64) -> Result<Value> {
    env.records(model)?
        .browse([id])?
        .check_access(tx, Operation::Read)
        .await?;
    Ok(Value::Json(Arc::new(
        json!({"type":"ir.actions.act_window","res_model":model,"res_id":id,"view_mode":"form","views":[[false,"form"]]}),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outgoing_recipient_validation_deduplicates_domains_and_preserves_local_parts() {
        let input = [
            "user@EXAMPLE.test",
            "user@example.test",
            "User@example.test",
        ]
        .map(String::from);
        let request = OutgoingRequest {
            message_id: 123,
            company_id: 1,
            recipients: &input,
            subject: "Quotation",
            body: "Attached.",
            attachments: &[],
        };
        let expected = BTreeSet::from([
            "User@example.test".to_owned(),
            "user@example.test".to_owned(),
        ]);
        assert_eq!(recipients(&request).ok(), Some(expected));
        for invalid in [
            "a..b@example.test",
            "user@example.test\r\nBcc: other@example.test",
            "user@-example.test",
            "name <user@example.test>",
        ] {
            let input = [invalid.to_owned()];
            assert!(
                recipients(&OutgoingRequest {
                    recipients: &input,
                    ..request
                })
                .is_err()
            );
        }
    }

    #[test]
    fn outgoing_validation_rejects_attachment_and_header_injection() {
        let input = ["customer@example.test".into()];
        let request = OutgoingRequest {
            message_id: 1,
            company_id: 1,
            recipients: &input,
            subject: "Header\nnext",
            body: "Body",
            attachments: &[],
        };
        assert!(recipients(&request).is_err());
        let invalid = [OutgoingAttachment {
            filename: "../a.pdf".into(),
            mime_type: "application/pdf".into(),
            bytes: vec![],
        }];
        assert!(
            recipients(&OutgoingRequest {
                subject: "Invoice",
                attachments: &invalid,
                ..request
            })
            .is_err()
        );
    }
}
