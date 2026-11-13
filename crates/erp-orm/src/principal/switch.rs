// SPDX-License-Identifier: MIT OR Apache-2.0
//! The `with_user` engine rule (A4, slice P5): who may switch an
//! environment to another user, decided on the switching principal (the principal of the
//! environment `with_user` was called on, never the root actor of an impersonation chain).
//!
//! `with_user` stays synchronous and infallible. It computes a verdict when it builds the
//! environment and the environment carries it, as it carries a refused company context:
//!
//! - `System` switching, or a principal switching to its own subject: admitted, nothing carried;
//! - a principal that is not `System` switching to the superuser id: refused when built
//!   ([`Switch::Superuser`]): the environment keeps the switcher's principal without elevation, so
//!   it never reports `su()`, kind `System` or subject 1;
//! - a principal that is not `System` switching to another user: [`Switch::Pending`]: decided at
//!   the first use, from the switcher's own unelevated binding and `principal.impersonate`.
//!
//! The transaction's admission check of an environment ([`Transaction::check_env_block`], which
//! every engine entry runs before any memo, cache or decision) refuses a carried switch; every
//! async entry first resolves the pending switchers ([`Transaction::prepare_switch`]).

use std::sync::Arc;

use erp_fields::{Error, Result};

use super::{Principal, PrincipalBinding, UserId};
use crate::env::Env;
use crate::transaction::Transaction;

/// A `with_user` switch an environment carries until the transaction decides it. Every
/// derivation of the environment copies it; it is part of the environment's identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Switch {
    /// Refused when the environment was built: `switcher`, not `System`, asked for the superuser.
    Superuser {
        /// The subject of the principal that switched.
        switcher: UserId,
    },
    /// Decided at the first use: `switcher` (the switching principal without elevation) must hold
    /// `principal.impersonate`.
    Pending {
        /// The principal that switched, without elevation (a scope never carries the capability).
        switcher: Box<Principal>,
        /// The user it switched to.
        target: UserId,
    },
}

impl Switch {
    /// The subject whose resolution deciding this switch needs, if any.
    pub(crate) fn pending_switcher(&self) -> Option<UserId> {
        match self {
            Self::Pending { switcher, .. } => Some(switcher.subject()),
            Self::Superuser { .. } => None,
        }
    }
}

/// The refusal of a switch to the superuser id by `switcher`.
pub(crate) fn superuser_refusal(switcher: UserId) -> Error {
    Error::access(format!(
        "only the system principal may act as the superuser (user {switcher} called with_user(1))"
    ))
}

/// The refusal of a switch to `target` by `switcher`, which lacks the capability.
fn impersonation_refusal(switcher: UserId, target: UserId) -> Error {
    Error::access(format!(
        "user {switcher} may not act as user {target}: with_user needs the capability \
         'principal.impersonate'"
    ))
}

impl Transaction {
    /// Refuse a `with_user` switch `env` carries: the stored refusal of a switch to the superuser,
    /// or a pending switch whose switcher does not hold `principal.impersonate` (decided on the
    /// switcher's newest resolution in this transaction, as [`Transaction::binding`] trusts it).
    ///
    /// # Errors
    ///
    /// `Access` with the texts; `Programming` when this ORM was built without
    /// the impersonation capability, when the policy is stale, or when a resolver is installed and
    /// the switcher was not resolved in this transaction (an entry that did not run
    /// [`Transaction::prepare_switch`] first); the policy's errors.
    pub(crate) fn check_switches(&self, env: &Env) -> Result<()> {
        for switch in env.switches() {
            match switch {
                Switch::Superuser { switcher } => return Err(superuser_refusal(*switcher)),
                Switch::Pending { switcher, target } => self.check_pending(switcher, *target)?,
            }
        }
        Ok(())
    }

    fn check_pending(&self, switcher: &Principal, target: UserId) -> Result<()> {
        let Some(capability) = self.shared.impersonate else {
            return Err(Error::programming(format!(
                "user {} may not act as user {target}: with_user from a principal that is not the \
                 system needs the capability that authorizes impersonation, and this ORM was built \
                 without one (OrmBuilder::impersonate_capability)",
                switcher.subject()
            )));
        };
        if self.refresh.stale() {
            return Err(Error::programming(
                "the access policy of this transaction is stale: a policy or membership change is \
                 pending and a with_user switch was decided without \
                 Transaction::ensure_policy_current",
            ));
        }
        let policy = self.policy();
        let resolution = match self.resolved(switcher.subject()) {
            Some(newest) => Arc::clone(newest),
            None if policy.needs_resolved_principals() && self.shared.resolver.is_some() => {
                return Err(Error::programming(format!(
                    "user {} switched to user {target} with with_user, and the switching \
                     principal was not resolved by the principal resolver in this transaction: \
                     the operation decided without Transaction::prepare_access",
                    switcher.subject()
                )));
            }
            None => Arc::clone(switcher.resolution()),
        };
        if !resolution.is_resolved() && policy.needs_resolved_principals() {
            return Err(Error::programming(format!(
                "user {} switched to user {target} with with_user, and the switching principal \
                 was never resolved against the database: the installed access policy decides on \
                 the subject's groups, so the principal must come from the policy's resolver",
                switcher.subject()
            )));
        }
        let binding = PrincipalBinding::bind(policy.as_ref(), switcher, resolution)?;
        if policy.has_capability(&binding, capability)? {
            Ok(())
        } else {
            Err(impersonation_refusal(switcher.subject(), target))
        }
    }
}
