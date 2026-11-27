// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;
use erp_orm::{
    BoxFuture, Operation,
    jobs::{ChunkResult, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl, RunOutcome},
};
use serde::Deserialize;
use std::sync::Mutex;

#[derive(Deserialize)]
struct ProbeArgs {
    relay: i64,
    recipient: Option<String>,
}

struct Probe;
erp_sdk::jobs::register_job_kind!(
    PROBE,
    JobKindDecl::new("mail", "mail.smtp_probe", HandlerRef::Native(&Probe)).max_attempts(1)
);

const REQUIRED: [erp_orm::jobs::JobKindRequirement; 1] = [erp_orm::jobs::JobKindRequirement::new(
    "mail.smtp_probe",
    "erp-cli",
)];
pub(crate) fn job_kinds() -> &'static [erp_orm::jobs::JobKindRequirement] {
    std::hint::black_box(&PROBE);
    &REQUIRED
}

impl JobHandler for Probe {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let ProbeArgs {
                relay: id,
                recipient,
            } = ctx.payload()?;
            if ctx.progress::<bool>()?.is_some() {
                return Err(JobError::permanent(
                    "smtp_probe",
                    "A previous attempt stopped. Inspect the relay before repeating the test.",
                ));
            }
            let slot = Arc::new(Mutex::new(None));
            let out = slot.clone();
            ctx.chunk(move |tx| {
                let out = out.clone();
                Box::pin(async move {
                    let env = tx.env();
                    let record = env.records("mail.smtp.relay")?.browse([id])?;
                    record.check_access(tx, Operation::Write).await?;
                    let erp_sdk::prelude::Cell::Id(company) = record.cell(tx, "company_id").await?
                    else {
                        return Err(Error::validation("SMTP relay needs a company.").into());
                    };
                    let config = env.generation_attachment::<RelayConfig>()?;
                    let relay = resolve(tx, &config, company).await?;
                    *out.lock()
                        .map_err(|_| Error::invariant("SMTP check interrupted."))? = Some(relay);
                    Ok(ChunkResult::more(true, 0, Some(1)))
                })
            })
            .await?;
            let relay = slot
                .lock()
                .map_err(|_| Error::invariant("SMTP check interrupted."))?
                .take()
                .ok_or_else(|| Error::invariant("Missing SMTP route."))?;
            let uuid = ctx.job().uuid();
            ctx.external(async move |cancel| {
                if cancel.is_cancelled() {
                    return Err(JobError::permanent("smtp_probe", "SMTP check cancelled."));
                }
                let result = if let Some(to) = recipient {
                    send(
                        &relay,
                        Message {
                            to,
                            subject: "ERPForAll outgoing mail test".into(),
                            text: "This email verifies the company's outgoing SMTP route.".into(),
                            attachments: vec![],
                            message_id: format!("smtp-test-{uuid}@erpforall.invalid"),
                        },
                    )
                    .await
                } else {
                    test_connection(&relay).await
                };
                match result {
                    Delivery::Accepted => Ok(()),
                    Delivery::Uncertain(msg)
                    | Delivery::Temporary(msg)
                    | Delivery::Permanent(msg) => Err(JobError::permanent("smtp_probe", msg)),
                }
            })
            .await?;
            ctx.chunk(|_| Box::pin(async { Ok(ChunkResult::done(true, 1)) }))
                .await?;
            Ok(RunOutcome::Done)
        })
    }
}
