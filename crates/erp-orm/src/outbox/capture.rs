//! Immutable names-only attribution, bound by the actual writer, never the flusher.

use super::{Envelope, EnvelopeParts, IdEntry, Kind, MAX_COMPANIES};
use crate::{Actor, Env, KeyKind, OperationIdentity, PrincipalKind, UserId};
use erp_fields::{Error, Result};

/// Discriminator for the namespace of `actor_id`; API key kinds survive impersonation.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ActorKind {
    User,
    System,
    Public,
    Link,
    ApiKeyUser,
    ApiKeyAgent,
    ApiKeyService,
}

impl ActorKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::Public => "public",
            Self::Link => "link",
            Self::ApiKeyUser => "api_key_user",
            Self::ApiKeyAgent => "api_key_agent",
            Self::ApiKeyService => "api_key_service",
        }
    }
}

/// Validated metadata shared by the envelopes of one actual mutation.
#[derive(Debug)]
pub(crate) struct CaptureContext {
    identity: OperationIdentity,
    principal_kind: PrincipalKind,
    subject: String,
    actor_kind: ActorKind,
    actor_id: i64,
    delegating_user_id: Option<i64>,
    company_ids: Vec<i64>,
}

impl CaptureContext {
    /// `company_ids` must be resolved by Transaction::companies for this effective environment.
    /// Missing execution identity and malformed company metadata are programming errors.
    pub(crate) fn from_env(env: &Env, company_ids: Vec<i64>) -> Result<Self> {
        let identity = env
            .operation_identity()
            .ok_or_else(|| {
                Error::programming("outbox: eventful mutation requires an OperationIdentity")
            })?
            .clone();
        if company_ids.len() > MAX_COMPANIES {
            return Err(super::limit("event list exceeds 256 entries"));
        }
        for (index, id) in company_ids.iter().enumerate() {
            if *id <= 0 || company_ids[..index].contains(id) {
                return Err(Error::programming(
                    "outbox: capture companies must be positive and unique",
                ));
            }
        }
        let principal = env.principal();
        let (actor_kind, actor_id, delegating_user_id) =
            actor_details(principal.kind(), principal.subject(), principal.actor())?;
        Ok(Self {
            identity,
            principal_kind: principal.kind(),
            subject: principal.subject().get().to_string(),
            actor_kind,
            actor_id,
            delegating_user_id,
            company_ids,
        })
    }

    /// Conservative per-cell charge includes a full context even though cells share its Arc.
    pub(crate) fn pending_charge(&self) -> Result<usize> {
        std::mem::size_of::<Self>()
            .checked_add(self.subject.capacity())
            .and_then(|n| n.checked_add(self.company_ids.capacity().checked_mul(8)?))
            // The two canonical UUID boxes retain 36 bytes each; include the Arc header.
            .and_then(|n| n.checked_add(72 + 2 * std::mem::size_of::<usize>()))
            .ok_or_else(|| super::limit("pending provenance accounting overflow"))
    }

    pub(crate) fn envelope(
        &self,
        kind: Kind,
        model: &str,
        record_ids: Vec<IdEntry>,
        field_names: Vec<String>,
        cascade_models: Vec<String>,
    ) -> Result<Envelope> {
        Envelope::new(EnvelopeParts {
            kind,
            model,
            record_ids,
            field_names,
            cascade_models,
            subject: Some(self.subject.clone()),
            actor_id: self.actor_id,
            actor_kind: self.actor_kind,
            principal_kind: self.principal_kind,
            operation_id: self.identity.operation().clone(),
            delegating_user_id: self.delegating_user_id,
            company_ids: self.company_ids.clone(),
            correlation_id: Some(self.identity.correlation().clone()),
            // This native execution has no event/job parent. Never invent a causation id.
            causation_id: None,
        })
    }
}

pub(crate) fn actor_details(
    kind: PrincipalKind,
    subject: UserId,
    actor: Option<&Actor>,
) -> Result<(ActorKind, i64, Option<i64>)> {
    match actor {
        None => match kind {
            PrincipalKind::User => Ok((ActorKind::User, subject.get(), None)),
            PrincipalKind::System => Ok((ActorKind::System, subject.get(), None)),
            PrincipalKind::Public => Ok((ActorKind::Public, subject.get(), None)),
            PrincipalKind::Agent | PrincipalKind::Service | PrincipalKind::Link => Err(
                Error::programming("outbox: this principal kind requires an explicit actor"),
            ),
        },
        Some(Actor::ApiKey { id, kind, .. }) => {
            let (tag, delegator) = match kind {
                KeyKind::User => (ActorKind::ApiKeyUser, None),
                KeyKind::Agent => (ActorKind::ApiKeyAgent, Some(subject.get())),
                KeyKind::Service => (ActorKind::ApiKeyService, None),
            };
            Ok((tag, id.get(), delegator))
        }
        Some(Actor::Link { id }) => Ok((ActorKind::Link, id.get(), None)),
        Some(Actor::Impersonator(original)) => {
            if matches!(original.actor(), Some(Actor::Impersonator(_))) {
                return Err(Error::programming(
                    "outbox: nested impersonator attribution is unsupported",
                ));
            }
            let (tag, id, _) =
                actor_details(original.kind(), original.subject(), original.actor())?;
            Ok((tag, id, Some(original.subject().get())))
        }
    }
}
