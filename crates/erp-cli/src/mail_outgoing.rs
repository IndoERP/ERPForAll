// SPDX-License-Identifier: MIT OR Apache-2.0
//! Connect the mail addon's transaction-free submission phase to the configured relay.
use crate::smtp;
use erp_addon_mail::outgoing::{Delivery, Envelope, PreparedSender, Transport};
use erp_fields::Result;
use erp_orm::{BoxFuture, Transaction};

pub(crate) struct SmtpAdapter;
impl Transport for SmtpAdapter {
    fn prepare<'a>(
        &'a self,
        tx: &'a mut Transaction,
        company: i64,
    ) -> BoxFuture<'a, Result<Box<dyn PreparedSender>>> {
        Box::pin(async move {
            let config = tx.env().generation_attachment::<smtp::RelayConfig>()?;
            let relay = smtp::resolve(tx, &config, company).await?;
            Ok(Box::new(relay) as Box<dyn PreparedSender>)
        })
    }
}
impl PreparedSender for smtp::Relay {
    fn send(self: Box<Self>, envelope: Envelope) -> BoxFuture<'static, Delivery> {
        Box::pin(async move {
            let message = smtp::Message {
                to: envelope.recipient,
                text: envelope.body,
                subject: envelope.subject,
                message_id: envelope.message_id,
                attachments: envelope
                    .attachments
                    .into_iter()
                    .map(|a| smtp::Attachment {
                        filename: a.filename,
                        content_type: a.mime_type,
                        bytes: a.bytes,
                    })
                    .collect(),
            };
            match smtp::send(&self, message).await {
                smtp::Delivery::Accepted => Delivery::Accepted,
                smtp::Delivery::Temporary(s) => Delivery::Temporary(s),
                smtp::Delivery::Permanent(s) => Delivery::Permanent(s),
                smtp::Delivery::Uncertain(s) => Delivery::Uncertain(s),
            }
        })
    }
}

/// A stopped/revoked job may never enter its handler again. A submission whose result was not
/// persisted must stay visible for human review. Lock a bounded window across serving nodes.
pub(crate) async fn recover_stopped(owner: &erp_orm::Orm) -> Result<()> {
    let mut tx = owner.begin(erp_orm::SUPERUSER_ID).await?;
    tx.execute(
        "WITH stopped AS (
             SELECT m.id,m.job_uuid,m.state FROM mail_outgoing m
             WHERE m.state IN ('pending','sending') AND m.job_uuid IS NOT NULL
               AND NOT EXISTS (SELECT FROM erp_job j WHERE j.job_uuid=m.job_uuid::uuid
                               AND j.state IN ('running','ready','held'))
             ORDER BY m.id LIMIT 64 FOR UPDATE OF m SKIP LOCKED
         ), changed AS (
             UPDATE mail_outgoing m SET
                  state=CASE WHEN s.state='sending' THEN 'uncertain' ELSE 'failed' END,
                  last_error=CASE WHEN s.state='sending' THEN 'Job stopped before recording the relay response.'
                                  ELSE 'Job stopped before relay submission.' END
             FROM stopped s WHERE m.id=s.id RETURNING m.id,m.job_uuid,m.state,m.last_error
         )
         UPDATE mail_outgoing_attempt a SET outcome=c.state,detail=c.last_error
           FROM changed c WHERE a.outgoing_id=c.id AND a.job_uuid=c.job_uuid AND a.outcome='sending'",
        &[],
    ).await?;
    tx.commit().await
}

#[cfg(test)]
mod tests;
