// SPDX-License-Identifier: MIT OR Apache-2.0
//! A fenced database chunk reserves the delivery; the relay runs outside the transaction.

use super::{KIND, OutgoingAttachment, invalid_row};
use erp_sdk::jobs::{
    ChunkResult, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl, RunOutcome,
};
use erp_sdk::prelude::*;
use erp_sdk::{BoxFuture, Operation};
use std::sync::Mutex;
use std::time::Duration;

pub struct Envelope {
    pub recipient: String,
    pub subject: String,
    pub body: String,
    pub message_id: String,
    pub attachments: Vec<OutgoingAttachment>,
}

#[derive(Clone, Copy)]
pub enum Delivery {
    Accepted,
    Temporary(&'static str),
    Permanent(&'static str),
    Uncertain(&'static str),
}

pub trait PreparedSender: Send + Sync {
    fn send(self: Box<Self>, message: Envelope) -> BoxFuture<'static, Delivery>;
}
pub trait Transport: Send + Sync {
    fn prepare<'a>(
        &'a self,
        tx: &'a mut Transaction,
        company: i64,
    ) -> BoxFuture<'a, Result<Box<dyn PreparedSender>>>;
}
pub struct TransportService(pub Arc<dyn Transport>);

struct SendExternal;
erp_sdk::jobs::register_job_kind!(
    SEND_KIND,
    JobKindDecl::new("mail", KIND, HandlerRef::Native(&SendExternal)).max_attempts(5)
);

enum Reservation {
    Send(Box<dyn PreparedSender>, Envelope, i32),
    Finished,
}

/// Outcomes that must not be automatically resubmitted are persisted even if the worker exits or
/// loses its lease between SMTP DATA and the subsequent commit.
async fn reserve(tx: &mut Transaction, id: i64, job: &str) -> Result<Reservation> {
    let rows = tx.query(
        "SELECT company_id::bigint, recipient::text, subject::text, body::text, message_id::bigint,
                source_model::text, source_id::bigint, state::text, attempts::integer, smtp_message_id::text
           FROM mail_outgoing WHERE id=$1::bigint AND job_uuid=$2::text FOR UPDATE",
        &[&id,&job],
    ).await?;
    let Some(r) = rows.first() else {
        return Ok(Reservation::Finished);
    };
    let state: String = r.try_get(7).map_err(|_| invalid_row())?;
    if state == "sending" {
        record_result(
            tx,
            id,
            "uncertain",
            Some("Worker stopped before recording the relay response."),
        )
        .await?;
        return Ok(Reservation::Finished);
    }
    if state != "pending" {
        return Ok(Reservation::Finished);
    }
    let company: i64 = r.try_get(0).map_err(|_| invalid_row())?;
    let attempts: i32 = r.try_get(8).map_err(|_| invalid_row())?;
    let model: String = r.try_get(5).map_err(|_| invalid_row())?;
    let record: i64 = r.try_get(6).map_err(|_| invalid_row())?;
    let env = tx.env();
    let attempt = attempts + 1;
    tx.execute(
        "WITH submitting AS (
            UPDATE mail_outgoing SET state='sending', attempts=attempts+1, last_error=NULL
            WHERE id=$1::bigint RETURNING id,job_uuid,attempts
         ) INSERT INTO mail_outgoing_attempt(outgoing_id,job_uuid,attempt,started,outcome)
           SELECT id,job_uuid,attempts,clock_timestamp(),'sending' FROM submitting",
        &[&id],
    )
    .await?;
    // Identify the failed step without persisting provider output, SQL or record names.
    let mut refusal = "The sender no longer has access to the selected company.";
    let relay = tx
        .savepoint(async |tx| {
            if env.company_id()? != Some(company) {
                return Err(Error::access(
                    "The sender no longer has access to the selected company.",
                ));
            }
            refusal = "The source record is unavailable or the sender lost write access.";
            env.records(&model)?
                .browse([record])?
                .check_access(tx, Operation::Write)
                .await?;
            refusal = "No outbound mail transport is installed on this server.";
            let transport = env.generation_attachment::<TransportService>()?;
            refusal = "Check neutralization, the SMTP profile and the company's sender address.";
            transport.0.prepare(tx, company).await
        })
        .await;
    let relay = match relay {
        Ok(relay) => relay,
        Err(e) if e.retries_transaction() || e.kind() == erp_sdk::ErrorKind::Database => {
            return Err(e);
        }
        Err(_) => {
            record_result(tx, id, "failed", Some(refusal)).await?;
            return Ok(Reservation::Finished);
        }
    };
    let message: i64 = r.try_get(4).map_err(|_| invalid_row())?;
    let files = tx
        .query(
            "SELECT a.filename::text, a.mime_type::text, a.content
           FROM mail_outgoing_attachment a JOIN mail_outgoing sibling ON sibling.id=a.outgoing_id
          WHERE sibling.message_id=$1::bigint ORDER BY a.id",
            &[&message],
        )
        .await?;
    let attachments = files
        .iter()
        .map(|file| {
            Ok(OutgoingAttachment {
                filename: file.try_get(0).map_err(|_| invalid_row())?,
                mime_type: file.try_get(1).map_err(|_| invalid_row())?,
                bytes: file.try_get(2).map_err(|_| invalid_row())?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let envelope = Envelope {
        recipient: r.try_get(1).map_err(|_| invalid_row())?,
        subject: r.try_get(2).map_err(|_| invalid_row())?,
        body: r.try_get(3).map_err(|_| invalid_row())?,
        message_id: r.try_get(9).map_err(|_| invalid_row())?,
        attachments,
    };
    Ok(Reservation::Send(relay, envelope, attempt))
}

async fn record_result(
    tx: &mut Transaction,
    id: i64,
    state: &str,
    reason: Option<&str>,
) -> Result<()> {
    tx.execute(
        "WITH saved AS (
            UPDATE mail_outgoing SET state=$2::text,last_error=$3::text WHERE id=$1::bigint
            RETURNING id,job_uuid,attempts
         ) UPDATE mail_outgoing_attempt a SET outcome=$2::text,detail=$3::text
             FROM saved m WHERE a.outgoing_id=m.id AND a.job_uuid=m.job_uuid AND a.attempt=m.attempts",
        &[&id,&state,&reason],
    ).await?;
    Ok(())
}

impl JobHandler for SendExternal {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let payload: erp_sdk::serde_json::Value = ctx.payload()?;
            let id = payload["id"]
                .as_i64()
                .ok_or_else(|| JobError::permanent("payload", "Invalid outgoing email job."))?;
            let job = ctx.job().uuid().to_string();
            let handoff = Arc::new(Mutex::new(None));
            let slot = Arc::clone(&handoff);
            let report = ctx
                .chunk(move |tx| {
                    let job = job.clone();
                    let slot = Arc::clone(&slot);
                    Box::pin(async move {
                        match reserve(tx, id, &job).await? {
                            Reservation::Finished => Ok(ChunkResult::done((), 0)),
                            Reservation::Send(relay, message, attempt) => {
                                *slot.lock().map_err(|_| invalid_row())? =
                                    Some((relay, message, attempt));
                                Ok(ChunkResult::more((), 0, Some(1)))
                            }
                        }
                    })
                })
                .await?;
            if report.finished {
                return Ok(RunOutcome::Done);
            }
            let (relay, message, attempt) = handoff
                .lock()
                .map_err(|_| invalid_row())?
                .take()
                .ok_or_else(invalid_row)?;
            let outcome = ctx
                .external(async move |token| {
                    if token.is_cancelled() {
                        return Err(JobError::transient(
                            "cancelled",
                            "Email submission was cancelled.",
                        ));
                    }
                    Ok(relay.send(message).await)
                })
                .await?;
            let (state, reason) = match outcome {
                Delivery::Accepted => ("accepted", None),
                Delivery::Uncertain(reason) => ("uncertain", Some(reason)),
                Delivery::Permanent(reason) => ("failed", Some(reason)),
                Delivery::Temporary(reason) if attempt >= 5 => ("failed", Some(reason)),
                Delivery::Temporary(reason) => ("pending", Some(reason)),
            };
            ctx.chunk(move |tx| {
                Box::pin(async move {
                    record_result(tx, id, state, reason).await?;
                    Ok(if state == "pending" {
                        ChunkResult::more((), 0, Some(1))
                    } else {
                        ChunkResult::done((), 1)
                    })
                })
            })
            .await?;
            if state == "pending" {
                let seconds = 30u64 << (attempt - 1).clamp(0, 4);
                return Ok(RunOutcome::RescheduleAfter(Duration::from_secs(seconds)));
            }
            Ok(RunOutcome::Done)
        })
    }
}

pub(crate) fn link_kind() {
    std::hint::black_box(&SEND_KIND);
}
