//! Policy or membership changes inside a transaction (design note §3.4,
//! SEC-018), and principal resolution at a transaction's start (§3.2).
//!
//! Core code that changes those tables with SQL
//! calls [`Transaction::mark_policy_dirty`], which sets both (ruling b).
//!
//! The next engine call runs [`Transaction::ensure_policy_current`]: it **first flushes the pending
//! writes of every registered model** (Revision 3 M2: a rebuild or re-resolution must never read
//! the rows as they were before a buffered write), then rebuilds a private snapshot from the
//! transaction's own view (`policy_dirty`), re-resolves every subject resolved in the transaction
//! in one statement (either flag), clears the record memo and the flags. Decisions taken while it
//! runs (computes of the flushed writes) use the snapshot in force; any other decision under a
//! set flag is a `Programming` error, never a stale answer.

use std::num::NonZeroU64;
use std::sync::Arc;

use erp_fields::{Error, Result};
use erp_registry::ModelId;
use rustc_hash::FxHashMap;

use super::SourceKind;
use crate::env::Env;
use crate::principal::{PrincipalHint, PrincipalKind, Resolution, UserId};
use crate::transaction::Transaction;

/// The §3.4 state of a transaction.
#[derive(Debug, Default)]
pub(crate) struct RefreshState {
    /// A policy table changed: rebuild privately before the next decision.
    pub(crate) policy_dirty: bool,
    /// A membership changed: re-resolve the bound subjects before the next decision.
    pub(crate) principals_dirty: bool,
    /// `ensure_policy_current` is running (its flush may decide with the snapshot in force).
    pub(crate) refreshing: bool,
    /// The newest resolution of each subject resolved in this transaction.
    pub(crate) resolved: FxHashMap<UserId, Arc<Resolution>>,
    /// The last resolution epoch handed out (1 is a transaction's first resolution).
    pub(crate) epoch: u64,
}

impl RefreshState {
    /// Whether a decision now would use a stale policy or resolution.
    pub(crate) fn stale(&self) -> bool {
        (self.policy_dirty || self.principals_dirty) && !self.refreshing
    }

    fn next_epoch(&mut self) -> Result<NonZeroU64> {
        let next = self
            .epoch
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or_else(|| Error::programming("resolution epochs exhausted in one transaction"))?;
        self.epoch = next.get();
        Ok(next)
    }

    fn record(&mut self, subject: UserId, resolution: &Resolution) -> Result<()> {
        let epoch = self.next_epoch()?;
        self.resolved
            .insert(subject, Arc::new(resolution.with_epoch(epoch)));
        Ok(())
    }
}

impl Transaction {
    /// Mark the policy and the principals of this transaction stale after core code changed
    /// policy tables with SQL (the data loader, uninstall, migrations; their bump triggers move
    /// the `policy` generation for other transactions). The next engine call rebuilds the policy
    /// privately and re-resolves the bound principals (design note §3.4, ruling b: both flags,
    /// over-invalidation is the safe direction).
    pub fn mark_policy_dirty(&mut self) {
        self.refresh.policy_dirty = true;
        self.refresh.principals_dirty = true;
    }

    /// The registrations of `model` a write touching `fields` (`None`: a create or an unlink)
    /// triggers.
    fn source_kinds(&self, model: ModelId, fields: Option<&[&str]>) -> (bool, bool) {
        let mut policy = false;
        let mut membership = false;
        for reg in self.shared.policy_sources.get(&model).into_iter().flatten() {
            let hit = match fields {
                None => true,
                Some(fields) => fields.iter().any(|f| reg.watches_field(f)),
            };
            if hit {
                match reg.kind() {
                    SourceKind::Policy => policy = true,
                    SourceKind::Membership => membership = true,
                }
            }
        }
        (policy, membership)
    }

    /// Returns whether the write changes the policy or a membership ([`Transaction::
    /// policy_source_written`] marks it after the write).
    pub(crate) async fn before_policy_source_write(
        &mut self,
        model: ModelId,
        fields: Option<&[&str]>,
    ) -> Result<(bool, bool)> {
        let kinds = self.source_kinds(model, fields);
        if kinds.0 || kinds.1 {
            self.flush().await?;
        }
        Ok(kinds)
    }

    /// After the write: mark the transaction for the next engine call.
    pub(crate) fn policy_source_written(&mut self, (policy, membership): (bool, bool)) {
        self.refresh.policy_dirty |= policy;
        self.refresh.principals_dirty |= membership;
    }

    /// Bring the policy and the principal resolutions up to date before a decision (design note
    /// §3.4): with a signal installed, open the transaction so its snapshot is pinned at the
    /// generation it read; when a flag is set, flush the pending writes of every registered model,
    /// rebuild privately (policy) and re-resolve the bound subjects (either flag) in one
    /// statement, then forget the memoized record checks. Cheap when nothing is pending.
    ///
    /// # Errors
    ///
    /// The flush's, the provider's and the resolver's errors; the transaction is then left
    /// stale (the flags stay set), so no later decision uses the old policy.
    pub(crate) async fn ensure_policy_current(&mut self) -> Result<()> {
        // a refresh whose future was dropped leaves `refreshing` set, which would hide the stale
        // flags: refuse every later call instead of deciding with the old policy
        self.check_not_cancelled()?;
        if self.refresh.refreshing {
            return Ok(());
        }
        if self.shared.signal.is_some() && self.signal_read && self.is_idle() {
            // the snapshot of the generation this transaction's BEGIN reads
            self.open().await?;
        }
        if !(self.refresh.policy_dirty || self.refresh.principals_dirty) {
            return Ok(());
        }
        self.refresh.refreshing = true;
        // dropped mid-refresh (a timeout): the policy and the resolutions are half rebuilt and
        // `refreshing` stays set, so the guard poisons the transaction (roll back to go on)
        let guard = self.guard();
        // boxed: the flush may compute, and a compute may search (and prepare access) again
        let outcome = Box::pin(self.refresh_now()).await;
        self.refresh.refreshing = false;
        guard.disarm();
        outcome
    }

    async fn refresh_now(&mut self) -> Result<()> {
        // the pending writes of every policy source and membership first (Revision 3 M2)
        let models: Vec<ModelId> = self.shared.policy_sources.keys().copied().collect();
        for model in models {
            self.flush_model(model).await?;
        }
        if self.refresh.policy_dirty {
            let provider = Arc::clone(&self.shared.policy);
            let private = provider.rebuild_private(self).await?;
            self.policy = private;
        }
        if let Some(resolver) = self.shared.resolver.clone() {
            let mut subjects: Vec<UserId> = self.refresh.resolved.keys().copied().collect();
            subjects.sort_unstable();
            if !subjects.is_empty() {
                let resolutions = resolver.re_resolve(self, &subjects).await?;
                if resolutions.len() != subjects.len() {
                    return Err(Error::programming(format!(
                        "the principal resolver re-resolved {} subjects for {} asked",
                        resolutions.len(),
                        subjects.len()
                    )));
                }
                for (subject, resolution) in subjects.iter().zip(&resolutions) {
                    self.refresh.record(*subject, resolution)?;
                }
            }
        }
        self.access.clear_memo();
        // the decisions remembered for x2many reads rest on the old policy too
        self.engine.readable.clear();
        self.refresh.policy_dirty = false;
        self.refresh.principals_dirty = false;
        Ok(())
    }

    /// [`Transaction::ensure_policy_current`], then the resolution of `env`'s subject when the
    /// policy decides on groups and the transaction has not resolved it yet (an environment from
    /// `with_user`, or a principal whose resolution the caller built: that one is a hint, never
    /// trusted when a resolver is installed): one statement through the resolver, recorded in
    /// the transaction so a later membership change re-reads it.
    ///
    /// # Errors
    ///
    /// As [`Transaction::ensure_policy_current`]; the resolver's errors.
    pub(crate) async fn prepare_access(&mut self, env: &Env) -> Result<()> {
        self.ensure_policy_current().await?;
        // the switchers of `with_user` switches first, before any early return (A4)
        self.resolve_switchers(env).await?;
        let principal = env.principal();
        // the subject whose companies bound the environment's (review S1): not the acting one
        // for a `System` principal made by `with_user(SUPERUSER_ID)`; none for the core's own
        let authority = principal.company_authority();
        let groups_needed =
            !principal.elevation().is_full() && self.policy.needs_resolved_principals();
        // companies are validated whatever the elevation; an environment that names none (a
        // principal not resolved yet, as from `with_user`) acts with the subject's default
        // companies, so the subject is resolved for it as well (review R2)
        let companies_needed = authority.is_some();
        if !(groups_needed || companies_needed) {
            return Ok(());
        }
        let subject = authority.map_or_else(|| principal.subject(), |a| a.subject());
        if self.refresh.resolved.contains_key(&subject) {
            return Ok(());
        }
        let Some(resolver) = self.shared.resolver.clone() else {
            return Ok(());
        };
        let resolutions = resolver.re_resolve(self, &[subject]).await?;
        let [resolution] = resolutions.as_slice() else {
            return Err(Error::programming(format!(
                "the principal resolver returned {} resolutions for user {subject}",
                resolutions.len()
            )));
        };
        self.refresh.record(subject, resolution)
    }

    /// Resolve the switching principals of the pending `with_user` switches `env` carries
    /// (A4), so the admission check ([`Transaction::check_env_block`]) decides them with
    /// the switcher's binding instead of refusing an unresolved switcher: brings the policy up to
    /// date, then reads every switcher the transaction has not resolved yet in one statement
    /// through the resolver. Nothing when `env` carries no pending switch, when the policy does
    /// not decide on groups, or without a resolver.
    ///
    /// # Errors
    ///
    /// As [`Transaction::ensure_policy_current`]; the resolver's errors.
    pub(crate) fn prepare_switch<'a>(&'a mut self, env: &'a Env) -> crate::transaction::Admit<'a> {
        if !env
            .switches()
            .iter()
            .any(|switch| switch.pending_switcher().is_some())
        {
            return crate::transaction::Admit::Ready(Some(Ok(())));
        }
        // a named future with an erased body: the refresh flushes and may read records again
        // through the entries that call this, so the body stays out of their future types
        crate::transaction::Admit::Pending(Box::pin(async move {
            self.ensure_policy_current().await?;
            self.resolve_switchers(env).await
        }))
    }

    /// The resolution half of [`Transaction::prepare_switch`], for a caller that brought the
    /// policy up to date itself ([`Transaction::prepare_access`]).
    async fn resolve_switchers(&mut self, env: &Env) -> Result<()> {
        let mut subjects: Vec<UserId> = env
            .switches()
            .iter()
            .filter_map(crate::principal::switch::Switch::pending_switcher)
            .collect();
        if subjects.is_empty() || !self.policy.needs_resolved_principals() {
            return Ok(());
        }
        let Some(resolver) = self.shared.resolver.clone() else {
            return Ok(());
        };
        subjects.sort_unstable();
        subjects.dedup();
        subjects.retain(|subject| !self.refresh.resolved.contains_key(subject));
        if subjects.is_empty() {
            return Ok(());
        }
        let resolutions = resolver.re_resolve(self, &subjects).await?;
        if resolutions.len() != subjects.len() {
            return Err(Error::programming(format!(
                "the principal resolver resolved {} switching users for {} asked",
                resolutions.len(),
                subjects.len()
            )));
        }
        for (subject, resolution) in subjects.iter().zip(&resolutions) {
            self.refresh.record(*subject, resolution)?;
        }
        Ok(())
    }

    /// [`Transaction::prepare_access`] for the non-superuser form of `env` when `domain` has an
    /// `access` condition: the optimizer resolves it against that environment even
    /// when `env` is in superuser mode, and cannot await.
    ///
    /// # Errors
    ///
    /// As [`Transaction::prepare_access`].
    pub(crate) async fn prepare_access_for_domain(
        &mut self,
        env: &Env,
        domain: &erp_domain::Domain,
    ) -> Result<()> {
        if super::engine::mentions_access(domain) {
            self.prepare_access(&env.sudo(false)).await?;
        }
        Ok(())
    }

    /// Resolve the principal a transaction begins with (design note §3.2): one statement
    /// pipelined with `BEGIN`; the environment then carries the resolved
    /// principal.
    pub(crate) async fn resolve_begin(&mut self, hint: PrincipalHint) -> Result<()> {
        let Some(resolver) = self.shared.resolver.clone() else {
            return Ok(());
        };
        if hint.kind() == PrincipalKind::System {
            return Ok(());
        }
        let statement = resolver.statement(&hint)?;
        // `BEGIN`, the limits, the signal read and this statement leave in one flight
        let rows = self.begin_resolving(&statement).await?;
        let policy = Arc::clone(&self.policy);
        let principal = resolver.finish(&hint, &rows, policy.as_ref())?;
        if principal.subject() != hint.subject() {
            return Err(Error::programming(format!(
                "the principal resolver resolved user {} for a transaction of user {}",
                principal.subject(),
                hint.subject()
            )));
        }
        let resolution = Arc::clone(principal.resolution());
        self.refresh.record(principal.subject(), &resolution)?;
        self.replace_principal(principal);
        Ok(())
    }

    /// The opening flight of [`crate::Orm::begin_credential`]: `BEGIN`, the limits, the signal read
    /// and the check's statement in one round trip, then the check's verdict.
    pub(crate) async fn begin_credential_flight<C: super::CredentialCheck>(
        mut self,
        statement: &super::ResolverStatement,
        check: &C,
    ) -> Result<super::CredentialBegin<C::Accepted, C::Refused>> {
        let rows = match self.begin_resolving(statement).await {
            Ok(rows) => rows,
            Err(error) => return Err(self.discard_after(error).await),
        };
        let policy = Arc::clone(&self.policy);
        let verdict = match check.finish(&rows, policy.as_ref()) {
            Ok(verdict) => verdict,
            Err(error) => return Err(self.discard_after(error).await),
        };
        match verdict {
            super::CredentialVerdict::Accepted { principal, value } => {
                if principal.kind() == PrincipalKind::System || principal.elevation().is_full() {
                    let error = Error::programming(format!(
                        "a credential check accepted a {} principal with {:?} elevation; a \
                         credential acts as its subject, never as the system",
                        principal.kind().as_str(),
                        principal.elevation()
                    ));
                    return Err(self.discard_after(error).await);
                }
                let resolution = Arc::clone(principal.resolution());
                if let Err(error) = self.refresh.record(principal.subject(), &resolution) {
                    return Err(self.discard_after(error).await);
                }
                self.replace_principal(principal);
                Ok(super::CredentialBegin::Open { tx: self, value })
            }
            super::CredentialVerdict::Refused(reason) => {
                self.rollback().await?;
                Ok(super::CredentialBegin::Refused(reason))
            }
        }
    }

    /// The newest resolution of `subject` read in this transaction.
    pub(crate) fn resolved(&self, subject: UserId) -> Option<&Arc<Resolution>> {
        self.refresh.resolved.get(&subject)
    }
}
