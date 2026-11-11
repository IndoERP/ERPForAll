//! Group membership checks of an environment's user (`env.user.has_groups(spec)`).
//!
//! The policy decides ([`crate::AccessPolicy::has_groups`]) on the groups the transaction
//! resolved for the environment's subject, so asking costs no statement for a user the
//! transaction already resolved, and one statement (the principal resolver's) for the first
//! question about another user.

use erp_fields::Result;

use crate::env::Env;
use crate::principal::{Capability, GroupId, QualifiedXmlId};
use crate::transaction::Transaction;

impl Transaction {
    /// `base.group_no_one` counts only when `env` is in debug mode
    /// ([`Env::with_debug`]). Superuser mode does not change the answer: it is about the user.
    ///
    /// ```
    /// # async fn demo(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let env = tx.env();
    /// if tx.has_groups(&env, "base.group_user,!base.group_portal").await? {
    ///     // an internal user
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// A spec that does not parse or names a group the policy does not have (never `false`: a
    /// typo must not silently deny or allow); the resolver's and the policy's errors.
    pub async fn has_groups(&mut self, env: &Env, spec: &str) -> Result<bool> {
        self.prepare_access(env).await?;
        let binding = self.binding(env)?;
        self.policy().has_groups(&binding, spec)
    }

    /// # Errors
    ///
    /// `Programming` when `xmlid` is not one `module.name`; the errors of
    /// [`Transaction::has_groups`].
    pub async fn has_group(&mut self, env: &Env, xmlid: &str) -> Result<bool> {
        QualifiedXmlId::parse(xmlid)?;
        self.has_groups(env, xmlid).await
    }

    /// An empty list is never satisfied;
    /// `base.group_no_one` counts only in debug mode ([`Env::with_debug`]); superuser mode does
    /// not change the answer. To check many records, take a [`GroupMembership`] once
    /// ([`Transaction::group_membership`]).
    ///
    /// ```
    /// # async fn demo(tx: &mut erp_orm::Transaction, groups: &[erp_orm::GroupId]) -> erp_fields::Result<()> {
    /// let env = tx.env();
    /// if !groups.is_empty() && !tx.has_any_group(&env, groups).await? {
    ///     // the record is not for this user
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// The resolver's and the policy's errors.
    pub async fn has_any_group(&mut self, env: &Env, groups: &[GroupId]) -> Result<bool> {
        self.group_membership(env).await?.has_any(groups)
    }

    /// The group membership of `env`'s user, for many checks without a statement or an allocation
    /// each: the policy and the binding of this transaction, taken once (the principal resolved,
    /// as every access check does first).
    ///
    /// It answers for the policy and membership the transaction had when it was taken: take it
    /// for one operation, not across writes to groups or memberships.
    ///
    /// # Errors
    ///
    /// The resolver's errors; those of the binding (a closed elevation block, an unresolved
    /// principal).
    pub async fn group_membership<'e>(&mut self, env: &'e Env) -> Result<GroupMembership<'e>> {
        self.prepare_access(env).await?;
        let binding = self.binding(env)?;
        Ok(GroupMembership {
            policy: std::sync::Arc::clone(self.policy()),
            binding,
        })
    }
}

/// The group membership of one environment's user in one transaction
/// ([`Transaction::group_membership`]).
#[derive(Clone)]
#[must_use = "a group membership answers nothing until it is asked (`has_any`)"]
pub struct GroupMembership<'e> {
    policy: std::sync::Arc<dyn crate::AccessPolicy>,
    binding: crate::principal::PrincipalBinding<'e>,
}

impl std::fmt::Debug for GroupMembership<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupMembership")
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}

impl GroupMembership<'_> {
    /// Whether the user is in at least one of `groups` (as [`Transaction::has_any_group`]): no
    /// statement, no allocation.
    ///
    /// # Errors
    ///
    /// A policy that does not evaluate groups (`Programming`).
    pub fn has_any(&self, groups: &[GroupId]) -> Result<bool> {
        self.policy.has_any_group(&self.binding, groups)
    }
}

impl Transaction {
    /// Whether the user of `env` holds the capability `cap`: the groups
    /// the `ir_capability` table maps it to, against the user's groups. Capabilities belong to the
    /// user: superuser mode and elevation blocks never grant one.
    ///
    /// ```
    /// # async fn demo(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// erp_orm::capability!(APPROVE = "my_addon.approve");
    /// let env = tx.env();
    /// if !tx.has_capability(&env, &APPROVE).await? {
    ///     return Err(erp_fields::Error::access("You may not approve."));
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// `Programming` for a value `capability!` did not create, and for a capability without an
    /// `ir_capability` row (never `false`: a missing mapping must not look like a refusal);
    /// `Access` when the mapped group spec names a group that does not exist; the resolver's and
    /// the policy's errors.
    pub async fn has_capability(&mut self, env: &Env, cap: &Capability) -> Result<bool> {
        self.prepare_access(env).await?;
        let binding = self.binding(env)?;
        self.policy().has_capability(&binding, cap)
    }
}
