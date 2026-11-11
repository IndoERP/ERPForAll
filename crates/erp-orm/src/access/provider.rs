//! Where policy snapshots and resolved principals come from (seam 2):
//! the transaction does not know `erp-security`; it asks a [`PolicyProvider`] for the snapshot to
//! pin and a [`PrincipalResolver`] for principals.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use erp_fields::Result;
use erp_sql::param::Param;
use tokio_postgres::Row;

use super::AccessPolicy;
use super::static_policy::{StaticPolicy, StaticSnapshot};
use crate::dispatch::BoxFuture;
use crate::principal::{Principal, PrincipalHint, Resolution, UserId};
use crate::transaction::Transaction;

/// Supplies the policy snapshot a transaction pins (DN §2.3, §3.2, §3.4).
///
/// `erp-security`'s `PolicySource` implements it (and SIGNAL's `Refresh` for the `policy` key);
/// [`FixedPolicy`] serves one snapshot for ever (static and test policies).
pub trait PolicyProvider: Send + Sync + 'static {
    /// The current shared snapshot. A transaction pins it when it begins (after the signal read
    /// applied any newer generation) and decides with it until it ends or rebuilds privately.
    fn current(&self) -> Arc<dyn AccessPolicy>;

    /// A private snapshot built from `tx`'s own uncommitted view (SEC-018), after an ORM write
    /// or a core SQL change to a policy table in that transaction. Never published; its origin
    /// is [`super::SnapshotOrigin::Private`] and its [`crate::SnapshotId`] is its own.
    ///
    /// # Errors
    ///
    /// A failed build fails the operation that needed it (the transaction never decides with a
    /// stale snapshot after its own policy write).
    fn rebuild_private<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, Result<Arc<dyn AccessPolicy>>>;
}

impl fmt::Debug for dyn PolicyProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PolicyProvider")
    }
}

/// A provider of one snapshot that never changes: a policy that is not read from the database
/// (static and test policies). Its private rebuild returns the same snapshot, since a
/// transaction's writes cannot change it.
pub struct FixedPolicy(Arc<dyn AccessPolicy>);

impl FixedPolicy {
    /// The provider of `policy`.
    pub fn new(policy: Arc<dyn AccessPolicy>) -> Self {
        Self(policy)
    }
}

impl fmt::Debug for FixedPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FixedPolicy")
            .field(&self.0.snapshot())
            .finish()
    }
}

impl PolicyProvider for FixedPolicy {
    fn current(&self) -> Arc<dyn AccessPolicy> {
        Arc::clone(&self.0)
    }

    fn rebuild_private<'a>(
        &'a self,
        _tx: &'a mut Transaction,
    ) -> BoxFuture<'a, Result<Arc<dyn AccessPolicy>>> {
        Box::pin(async move { Ok(Arc::clone(&self.0)) })
    }
}

/// What [`crate::OrmBuilder::policy`] accepts: a [`StaticPolicy`], any [`AccessPolicy`]
/// (served by a [`FixedPolicy`]), or a provider (`Arc<dyn PolicyProvider>`).
pub trait IntoPolicyProvider {
    /// The provider.
    ///
    /// # Errors
    ///
    /// A static snapshot that cannot be built (snapshot ids exhausted).
    fn into_policy_provider(self) -> Result<Arc<dyn PolicyProvider>>;
}

impl IntoPolicyProvider for StaticPolicy {
    fn into_policy_provider(self) -> Result<Arc<dyn PolicyProvider>> {
        Ok(Arc::new(FixedPolicy::new(Arc::new(StaticSnapshot::new(
            self,
        )?))))
    }
}

impl<P: AccessPolicy> IntoPolicyProvider for P {
    fn into_policy_provider(self) -> Result<Arc<dyn PolicyProvider>> {
        Ok(Arc::new(FixedPolicy::new(Arc::new(self))))
    }
}

impl IntoPolicyProvider for Arc<dyn PolicyProvider> {
    fn into_policy_provider(self) -> Result<Arc<dyn PolicyProvider>> {
        Ok(self)
    }
}

/// The statement a [`PrincipalResolver`] reads a principal with: one statement, pipelined at
/// `BEGIN` once lands (DN §3.2), sent right after it until then.
#[derive(Debug, Clone)]
pub struct ResolverStatement {
    sql: Cow<'static, str>,
    params: Vec<Param>,
}

impl ResolverStatement {
    /// `sql` with its parameters (`$1…$n`, each cast in the text).
    pub fn new(sql: impl Into<Cow<'static, str>>, params: Vec<Param>) -> Self {
        Self {
            sql: sql.into(),
            params,
        }
    }

    /// The text.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The parameters.
    pub fn params(&self) -> &[Param] {
        &self.params
    }
}

/// Resolves principals against the database (DN §2.3; `erp-security` implements it).
///
/// - [`PrincipalResolver::statement`] and [`PrincipalResolver::finish`] resolve the principal a
///   transaction begins with: one statement reading the subject (and a link's creator), its
///   direct groups and its companies; `finish` closes the groups through
///   [`AccessPolicy::groups`] and validates the companies.
/// - [`PrincipalResolver::re_resolve`] re-reads the subjects bound in a transaction after a
///   membership change in it (SEC-018): one statement (`uid = ANY($1)`).
///
/// An inactive or missing subject, delegating user, service user or link creator is an `Access`
/// error naming the principal kind (R-S15), never a principal without groups.
pub trait PrincipalResolver: Send + Sync + 'static {
    /// The statement resolving `hint`.
    ///
    /// # Errors
    ///
    /// A hint this resolver does not support (a later slice's principal kind).
    fn statement(&self, hint: &PrincipalHint) -> Result<ResolverStatement>;

    /// The principal of `hint` from the rows of its statement, its groups closed under `policy`.
    ///
    /// # Errors
    ///
    /// `Access` for an inactive or missing subject (naming its kind) or companies outside the
    /// subject's; the policy's error for an unknown group.
    fn finish(
        &self,
        hint: &PrincipalHint,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> Result<Principal>;

    /// The newest resolution of each of `subjects`, read in `tx` (its own uncommitted view), in
    /// the order given.
    ///
    /// # Errors
    ///
    /// As [`PrincipalResolver::finish`]; the statement's error.
    fn re_resolve<'a>(
        &'a self,
        tx: &'a mut Transaction,
        subjects: &'a [UserId],
    ) -> BoxFuture<'a, Result<Vec<Resolution>>>;
}

impl fmt::Debug for dyn PrincipalResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrincipalResolver")
    }
}
