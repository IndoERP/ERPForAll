//! Elevation evidence: what the hook records and how it is deduplicated and shown (
//! design `m3-audit` §5.4).

use std::sync::Arc;

use erp_fields::Result;
use erp_registry::Registry;
use serde_json::{Value, json};

use super::{AuditError, buffer::Attribution};
use crate::{BlockId, ElevationScope, FieldAllow, Operation, UserId};

/// Longest elevation reason (bytes).
const MAX_REASON_BYTES: usize = 128;

/// Why an elevation was opened: a stable dotted name such as `base.server_action.run_as_superuser`
/// (`^[a-z][a-z0-9_]*(\.[a-z0-9_]+)+$`, at most 128 bytes).
///
/// ```
/// use erp_orm::audit::ElevationReason;
/// assert!(ElevationReason::new("base.server_action.run_as_superuser").is_ok());
/// assert!(ElevationReason::new("NoDots").is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElevationReason(&'static str);

impl ElevationReason {
    /// Validate `reason`.
    ///
    /// # Errors
    ///
    /// `Programming` ([`AuditError::Elevation`]) when it does not match the pattern or is longer
    /// than 128 bytes.
    pub fn new(reason: &'static str) -> Result<Self> {
        let mut parts = reason.split('.');
        let first_ok = parts.next().is_some_and(|p| {
            let mut chars = p.chars();
            chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        });
        let rest: Vec<&str> = parts.collect();
        let rest_ok = !rest.is_empty()
            && rest.iter().all(|p| {
                !p.is_empty()
                    && p.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
        if !first_ok || !rest_ok || reason.len() > MAX_REASON_BYTES {
            return Err(AuditError::Elevation(format!(
                "elevation reason {reason:?} must be dotted lowercase names of at most \
                 {MAX_REASON_BYTES} bytes"
            ))
            .into());
        }
        Ok(Self(reason))
    }

    /// The reason text.
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// The flagged server action a `run_as_superuser` run executes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SuperuserAction {
    /// The flagged action.
    pub action_id: i64,
    /// The action the run started from (equal to `action_id` for a root run).
    pub root_action_id: i64,
    /// The groups whose members may run it (never empty).
    pub group_ids: Box<[i64]>,
    /// The user who last wrote the action (who set the flag), when known.
    pub flag_write_uid: Option<i64>,
    /// When the action was last written, microseconds since the Unix epoch UTC, when known.
    pub flag_write_date_utc_micros: Option<i64>,
}

/// Why a full elevation exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum FullCause {
    ServerAction(SuperuserAction),
    UnscopedSudo { addon: &'static str },
}

/// What the elevation grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ElevationGrant {
    /// A scoped block (`Transaction::elevated`).
    Scoped {
        scope: Arc<ElevationScope>,
        block: BlockId,
    },
    /// Superuser mode.
    Full { cause: FullCause },
}

/// The key two observations must share to be one entry: the scope by value, never the block.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum GrantKey {
    Scoped(Arc<ElevationScope>),
    Full(FullCause),
}

impl ElevationGrant {
    pub(crate) fn key(&self) -> GrantKey {
        match self {
            Self::Scoped { scope, .. } => GrantKey::Scoped(Arc::clone(scope)),
            Self::Full { cause } => GrantKey::Full(cause.clone()),
        }
    }
}

/// One elevation as the hook receives it.
#[derive(Debug, Clone)]
pub(crate) struct ElevationObservation {
    pub(crate) reason: ElevationReason,
    pub(crate) grant: ElevationGrant,
    pub(crate) original: Attribution,
    pub(crate) effective_subject: UserId,
}

fn op_name(op: Operation) -> &'static str {
    match op {
        Operation::Read => "read",
        Operation::Write => "write",
        Operation::Create => "create",
        Operation::Unlink => "unlink",
    }
}

pub(crate) fn grant_json(key: &GrantKey, block_seq: Option<u32>, registry: &Registry) -> Value {
    match key {
        GrantKey::Full(FullCause::ServerAction(action)) => json!({
            "kind": "full",
            "cause": {
                "kind": "server_action",
                "action_id": action.action_id,
                "root_action_id": action.root_action_id,
                "group_ids": action.group_ids,
                "flag_write_uid": action.flag_write_uid,
                "flag_write_date_utc_micros": action.flag_write_date_utc_micros,
            },
        }),
        GrantKey::Full(FullCause::UnscopedSudo { addon }) => json!({
            "kind": "full",
            "cause": {"kind": "unscoped_sudo", "addon": addon},
        }),
        GrantKey::Scoped(scope) => {
            let items: Vec<Value> = scope
                .items()
                .iter()
                .map(|item| {
                    let spec = registry.model_by_id(item.model());
                    let fields = match item.field_allow() {
                        FieldAllow::All => json!("*"),
                        FieldAllow::Named(ids) => Value::from(
                            ids.iter()
                                .map(|id| spec.field_by_id(*id).name.to_string())
                                .collect::<Vec<_>>(),
                        ),
                    };
                    json!({
                        "model": spec.name.to_string(),
                        "ops": item.ops().iter().map(op_name).collect::<Vec<_>>(),
                        "fields": fields,
                        "lift": item.lifted().iter().map(|l| l.as_str()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            json!({"kind": "scoped", "block": block_seq, "items": items})
        }
    }
}
