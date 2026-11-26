//! Convert committed outbox events to jobs inside the cursor transaction.

use std::collections::BTreeSet;

use erp_fields::{Error, Result};
use sha2::{Digest, Sha256};

use super::{EnqueueRequest, JobKey, JobKindDecl, PrincipalPolicy, TriggerCoalesce};
use crate::{Orm, OutboxEvent, OutboxReplay, Transaction};

const JOB_CONSUMER: &str = "core.jobs";
const JOB_BATCH: u32 = 16;
const MAX_TRIGGER_REQUESTS: usize = 4096;

pub(super) fn validate(decl: &JobKindDecl) -> Result<()> {
    if decl.triggers.is_empty() {
        return Ok(());
    }
    let refuse = |reason| Error::programming(format!("job kind '{}' {reason}", decl.name));
    if decl.principal != PrincipalPolicy::System {
        return Err(refuse("requires the System principal for outbox delivery"));
    }
    if decl.schedule.is_some() || decl.triggers.len() > 32 {
        return Err(refuse("has a schedule or more than 32 outbox triggers"));
    }
    for trigger in decl.triggers {
        if trigger.model.is_empty()
            || trigger.model.len() > 128
            || trigger.kinds.is_empty()
            || trigger
                .kinds
                .iter()
                .any(|kind| !matches!(*kind, "create" | "write" | "unlink"))
            || trigger
                .fields
                .iter()
                .any(|field| field.is_empty() || field.len() > 256)
        {
            return Err(refuse(
                "has an invalid outbox trigger model, event kind or field",
            ));
        }
    }
    Ok(())
}

impl Orm {
    /// Owner-side service tick. The request ORM's active addon registry decides which linked
    /// job declarations may enqueue. Ordinary requests keep insert-only access to events.
    pub async fn deliver_outbox_jobs(&self, runtime: &Orm) -> Result<OutboxReplay> {
        let kinds: Vec<_> = super::JOB_KINDS
            .iter()
            .filter(|decl| {
                !decl.triggers.is_empty()
                    && (decl.addon == "core"
                        || runtime
                            .registry()
                            .addons()
                            .iter()
                            .any(|addon| addon.name == decl.addon))
            })
            .collect();
        if kinds.is_empty() {
            return Ok(OutboxReplay::Idle);
        }
        self.replay_outbox(JOB_CONSUMER, JOB_BATCH, move |tx, events| {
            Box::pin(async move { enqueue(tx, kinds, events).await })
        })
        .await
    }
}

async fn enqueue(
    tx: &mut Transaction,
    kinds: Vec<&JobKindDecl>,
    events: Vec<OutboxEvent>,
) -> Result<()> {
    let mut targets = BTreeSet::new();
    for event in &events {
        for decl in &kinds {
            for trigger in decl.triggers.iter().filter(|trigger| {
                trigger.model == event.model
                    && trigger.kinds.contains(&event.kind.as_str())
                    && (trigger.fields.is_empty()
                        || trigger
                            .fields
                            .iter()
                            .any(|field| event.field_names.iter().any(|name| name == field)))
            }) {
                match trigger.coalesce {
                    TriggerCoalesce::PerKind => {
                        targets.insert((
                            decl.name,
                            event.model.as_str(),
                            event.kind.as_str(),
                            None,
                        ));
                    }
                    TriggerCoalesce::PerRecord => {
                        for id in &event.record_ids {
                            targets.insert((
                                decl.name,
                                event.model.as_str(),
                                event.kind.as_str(),
                                Some(*id),
                            ));
                        }
                    }
                }
                if targets.len() > MAX_TRIGGER_REQUESTS {
                    return Err(Error::new(
                        erp_fields::ErrorKind::ResourceExhausted,
                        "outbox: one job delivery batch exceeds 4096 distinct targets",
                    ));
                }
            }
        }
    }
    let env = tx.env();
    let mut requests = Vec::new();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (job, model, kind, id) in targets {
        let target = format!("{job}\0{model}\0{kind}\0{}", id.unwrap_or(0));
        let mut key = String::from("outbox:");
        for byte in Sha256::digest(target) {
            key.push(HEX[usize::from(byte >> 4)] as char);
            key.push(HEX[usize::from(byte & 15)] as char);
        }
        // Group inside this batch. A running job for the same target has already read a snapshot
        // and could miss newer changes. A singleton serializes batches without suppressing one.
        requests.push(
            EnqueueRequest::new(job)
                .payload(&serde_json::json!({"model": model, "kind": kind, "record_id": id}))?
                .singleton(JobKey::new(&key)?),
        );
        if requests.len() == super::enqueue::MAX_ENQUEUE_BATCH {
            tx.enqueue_many(&env, std::mem::take(&mut requests)).await?;
        }
    }
    tx.enqueue_many(&env, requests).await?;
    Ok(())
}
