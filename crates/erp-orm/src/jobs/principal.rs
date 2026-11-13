//! The principal a job runs as: captured at enqueue, resolved again at every chunk.

use erp_fields::{Error, Result};

use crate::BoxFuture;
use crate::Transaction;
use crate::principal::{ApiKeyId, CompanyId, Principal, PrincipalKind, UserId};

/// What `erp_job.run_as_*` stores: kind, subject, key, delegating user and company context. No
/// credentials, no groups, no resolved policy: those are read again when a chunk starts.
///
/// A user principal that authenticated with a scoped API key is stored as the plain user: the
/// job runs with the user's own rights, not with the key's scope.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StoredPrincipal {
    /// `User` or `System` today (`Agent` and `Service` wait for).
    pub kind: PrincipalKind,
    /// The subject.
    pub subject: UserId,
    /// The API key of an agent or service principal.
    pub key: Option<ApiKeyId>,
    /// The user an agent acts for.
    pub delegating: Option<UserId>,
    /// The allowed companies, current first (at most 256).
    pub companies: Vec<CompanyId>,
    /// The current company.
    pub current_company: Option<CompanyId>,
}

/// Most companies a stored principal carries (the column's `CHECK`).
pub const MAX_STORED_COMPANIES: usize = 256;

impl StoredPrincipal {
    /// The stored form of `principal`, for [`PrincipalPolicy::Caller`](super::PrincipalPolicy)
    /// kinds.
    ///
    /// # Errors
    ///
    /// `Access` for a public or link principal (work started by a public request runs under the
    /// fixed principal of its kind), `Programming` for an agent or service principal until,
    /// and `Validation` for more than 256 companies.
    pub(crate) fn capture(principal: &Principal) -> Result<Self> {
        match principal.kind() {
            PrincipalKind::User | PrincipalKind::System => {}
            PrincipalKind::Public => return Err(super::error::public_principal("public")),
            PrincipalKind::Link => return Err(super::error::public_principal("link")),
            kind @ (PrincipalKind::Agent | PrincipalKind::Service) => {
                return Err(super::error::unsupported_principal(
                    kind.as_str(),
                    "a later release",
                ));
            }
        }
        let allowed = principal.companies().allowed();
        if allowed.len() > MAX_STORED_COMPANIES {
            return Err(Error::validation(format!(
                "a job can carry at most {MAX_STORED_COMPANIES} allowed companies (got {})",
                allowed.len()
            )));
        }
        Ok(Self {
            kind: principal.kind(),
            subject: principal.subject(),
            key: None,
            delegating: None,
            companies: allowed.to_vec(),
            current_company: principal.companies().current(),
        })
    }

    /// A stored principal of `kind` for `subject`, with no key, delegating user or companies
    /// (feature `test-faults`: the kinds that cannot be built yet are stored by hand in tests).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn for_tests(kind: PrincipalKind, subject: i64) -> Self {
        Self {
            kind,
            subject: UserId::new(subject).unwrap_or(crate::principal::SUPERUSER_ID),
            key: None,
            delegating: None,
            companies: Vec::new(),
            current_company: None,
        }
    }

    /// The column value of `run_as_kind`.
    pub(crate) fn kind_text(&self) -> &'static str {
        self.kind.as_str()
    }

    /// Parse the column `run_as_kind`.
    pub(crate) fn parse_kind(text: &str) -> Result<PrincipalKind> {
        match text {
            "user" => Ok(PrincipalKind::User),
            "system" => Ok(PrincipalKind::System),
            "agent" => Ok(PrincipalKind::Agent),
            "service" => Ok(PrincipalKind::Service),
            other => Err(Error::internal(format!(
                "erp_job holds the unknown principal kind '{other}'"
            ))),
        }
    }

    /// Rebuild from the stored columns.
    pub(crate) fn from_columns(
        kind: &str,
        subject: i64,
        companies: &[i64],
        current_company: Option<i64>,
    ) -> Result<Self> {
        let kind = Self::parse_kind(kind)?;
        let subject = UserId::new(subject)
            .map_err(|e| Error::internal(format!("erp_job holds an invalid subject: {e}")))?;
        let ids = |ids: &[i64]| -> Result<Vec<CompanyId>> {
            ids.iter()
                .map(|id| {
                    CompanyId::new(*id).map_err(|e| {
                        Error::internal(format!("erp_job holds an invalid company: {e}"))
                    })
                })
                .collect()
        };
        Ok(Self {
            kind,
            subject,
            key: None,
            delegating: None,
            companies: ids(companies)?,
            current_company: match current_company {
                Some(id) => Some(CompanyId::new(id).map_err(|e| {
                    Error::internal(format!("erp_job holds an invalid company: {e}"))
                })?),
                None => None,
            },
        })
    }
}

/// Why a stored principal cannot act any more. The reason is a fixed word, stored on the dead
/// job; it never names a group, a credential or a company.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrincipalRefusal {
    /// What changed: `subject_unavailable`, `company_access_removed`, `unsupported_kind` or
    /// `key_revoked`.
    pub reason: &'static str,
}

impl PrincipalRefusal {
    /// A refusal for `reason`.
    pub const fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}

/// Decides, inside the chunk's own transaction and snapshot, which [`Principal`] a stored one
/// is now (3). The chunk's transaction is opened for the stored subject, so the
/// ORM's [`PrincipalResolver`](crate::PrincipalResolver) has already read the subject's active
/// state, groups and companies; the job resolver checks what only a job has: that the stored
/// company context is still allowed, that the key of an agent or service principal is neither
/// revoked nor expired, and that the kind is one jobs can run. It never returns another
/// principal than the stored subject and never falls back to the system.
///
/// `erp-security` implements it; the runner refuses to start without one.
pub trait JobPrincipalResolver: Send + Sync + 'static {
    /// The principal `stored` acts as in `tx`, or why it can no longer act.
    fn admit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        stored: &'a StoredPrincipal,
    ) -> BoxFuture<'a, Result<Principal, PrincipalRefusal>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::PrincipalBuilder;

    #[test]
    fn a_user_principal_is_captured_with_its_company_context() {
        let user = UserId::new(7).unwrap();
        let principal = PrincipalBuilder::user(user).build().unwrap();
        let stored = StoredPrincipal::capture(&principal).unwrap();
        assert_eq!(stored.kind, PrincipalKind::User);
        assert_eq!(stored.subject, user);
        assert!(stored.companies.is_empty());
        assert_eq!(stored.current_company, None);
        assert_eq!(stored.kind_text(), "user");
    }

    #[test]
    fn the_stored_columns_round_trip() {
        let stored = StoredPrincipal::from_columns("user", 7, &[2, 1], Some(2)).unwrap();
        assert_eq!(
            stored.companies.iter().map(|c| c.get()).collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(stored.current_company.map(|c| c.get()), Some(2));
        assert_eq!(
            StoredPrincipal::from_columns("bogus", 7, &[], None)
                .unwrap_err()
                .message(),
            "erp_job holds the unknown principal kind 'bogus'"
        );
        assert!(StoredPrincipal::from_columns("user", 0, &[], None).is_err());
        assert!(StoredPrincipal::from_columns("user", 7, &[0], None).is_err());
    }
}
