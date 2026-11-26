//! Explicit owner-side operations. The request database role keeps insert-only event grants.

use crate::{Result, args::OutboxAction, human};
use erp_orm::{OutboxCursor, OutboxRetention};

pub(crate) fn execute(action: &OutboxAction) -> Result<String> {
    human::runtime()?.block_on(async {
        let config = match action {
            OutboxAction::Status { server, .. }
            | OutboxAction::Hold(server)
            | OutboxAction::ReleaseHold(server)
            | OutboxAction::Retain { server, .. }
            | OutboxAction::Restore { server, .. } => human::config(&server.config)?,
        };
        let orm = human::owner_maintenance_orm(&config)?;
        match action {
            OutboxAction::Status { consumer, .. } => match orm.outbox_status(consumer).await? {
                Some(state) => {
                    let acknowledged = state
                        .acknowledgement
                        .map_or_else(|| "unregistered".to_owned(), cursor);
                    let acknowledgement_age = age(state.acknowledgement_age_seconds);
                    let next_event_age = age(state.next_event_age_seconds);
                    Ok(format!(
                        "Retention floor {}. Hold {}. Consumer {consumer} at {acknowledged}. Cursor age {acknowledgement_age}. Next committed event age {next_event_age}.",
                        cursor(state.floor),
                        if state.held { "active" } else { "released" }
                    ))
                }
                None => Ok("Outbox delivery is awaiting initialization.".to_owned()),
            },
            OutboxAction::Hold(_) => {
                orm.hold_outbox_retention(true).await?;
                Ok("Outbox retention hold active.".to_owned())
            }
            OutboxAction::ReleaseHold(_) => {
                orm.hold_outbox_retention(false).await?;
                Ok("Outbox retention hold released.".to_owned())
            }
            OutboxAction::Retain { days, limit, .. } => {
                match orm.retain_outbox(*days, *limit).await? {
                    OutboxRetention::NotInstalled => Err(human::failed(
                        "Install an eventful addon before running outbox retention.",
                    )),
                    OutboxRetention::Busy => Err(human::failed(
                        "Outbox delivery or retention is busy. Retry retention.",
                    )),
                    OutboxRetention::Held => {
                        Ok("Retention skipped while the outbox hold is active.".to_owned())
                    }
                    OutboxRetention::Purged { count, floor } => Ok(format!(
                        "Removed {count} acknowledged event(s). Retention floor {}.",
                        cursor(floor)
                    )),
                }
            }
            OutboxAction::Restore {
                consumer, xid, id, ..
            } => {
                let target = OutboxCursor { xid: *xid, id: *id };
                orm.restore_outbox_cursor(consumer, target).await?;
                Ok(format!(
                    "Restored outbox consumer {consumer} to {}.",
                    cursor(target)
                ))
            }
        }
    })
}

fn cursor(position: OutboxCursor) -> String {
    format!("xid {}, id {}", position.xid, position.id)
}

fn age(seconds: Option<i64>) -> String {
    seconds.map_or_else(|| "unavailable".to_owned(), |seconds| format!("{seconds}s"))
}
