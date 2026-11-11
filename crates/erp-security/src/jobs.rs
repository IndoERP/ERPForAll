//! The security side of the job queue (2, D9, D10.6): who a job acts as, who may
//! administer jobs, and the core kinds the queue keeps for itself.
//!
//! `erp-orm` owns the queue but cannot declare a capability or register a job kind (the
//! workspace allows `linkme` registrations only in the crates above it), so this module does:
//!
//! - [`SecurityJobPrincipalResolver`] decides at every chunk which principal a stored one is now;
//! - [`JOBS_ADMIN`] is the capability behind hold, resume, cancel, requeue and discard (base maps
//!   it to the system administrators group in `ir.capability`);
//! - the kinds `core.jobs_history_retention` and `core.jobs_dead_expiry`, and [`core_job_kinds`]
//!   for `OrmBuilder::require_job_kinds`.

use erp_orm::jobs::{JobKindRequirement, JobPrincipalResolver, PrincipalRefusal, StoredPrincipal};
use erp_orm::{BoxFuture, Principal, PrincipalKind, Transaction};

erp_orm::capability!(
    /// Hold, resume, cancel, requeue and discard background jobs. Give it to the
    /// ORM with `OrmBuilder::job_admin_capability(&erp_security::jobs::JOBS_ADMIN)`.
    pub JOBS_ADMIN = "jobs.admin"
);

erp_orm::register_job_kind!(
    HISTORY_RETENTION_KIND,
    erp_orm::jobs::history_retention_decl()
);

erp_orm::register_job_kind!(DEAD_EXPIRY_KIND, erp_orm::jobs::dead_expiry_decl());

const REQUIRED: [JobKindRequirement; 2] = [
    JobKindRequirement::new("core.jobs_history_retention", "erp-security"),
    JobKindRequirement::new("core.jobs_dead_expiry", "erp-security"),
];

/// The core job kinds, for `OrmBuilder::require_job_kinds` at the build site of every binary that
/// runs jobs:. Calling it also keeps the registrations in the binary (it references them).
pub fn core_job_kinds() -> &'static [JobKindRequirement] {
    // the references pull the registrations' object file into the binary
    std::hint::black_box(&HISTORY_RETENTION_KIND);
    std::hint::black_box(&DEAD_EXPIRY_KIND);
    &REQUIRED
}

/// Decides which [`Principal`] a stored job principal is now (3).
///
/// The chunk's transaction is opened for the stored subject, so the ORM's principal resolver
/// has already read the subject's active state, groups and companies in the same snapshot (an
/// archived or deleted subject never gets this far: the open fails and the job dead-letters
/// with `PrincipalRevoked`). What this adds is what only a job has: the stored company context
/// must still be among the subject's companies, and the kind of principal must be one jobs can
/// run. It returns the stored subject or a refusal, never another principal and never the
/// system.
#[derive(Debug, Clone, Copy, Default)]
pub struct SecurityJobPrincipalResolver;

impl JobPrincipalResolver for SecurityJobPrincipalResolver {
    fn admit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        stored: &'a StoredPrincipal,
    ) -> BoxFuture<'a, Result<Principal, PrincipalRefusal>> {
        Box::pin(async move {
            if stored.kind != PrincipalKind::User {
                return Err(PrincipalRefusal::new("unsupported_kind"));
            }
            let principal = tx.env().principal().clone();
            if principal.subject() != stored.subject {
                return Err(PrincipalRefusal::new("subject_unavailable"));
            }
            if stored.companies.is_empty() {
                return Ok(principal);
            }
            if !principal.resolution().is_resolved() {
                return Err(PrincipalRefusal::new("principal_unresolved"));
            }
            principal
                .with_companies(stored.companies.clone())
                .map_err(|_| PrincipalRefusal::new("company_access_removed"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_kinds_are_registered_and_required() {
        let registered: Vec<&str> = erp_orm::jobs::JOB_KINDS.iter().map(|k| k.name).collect();
        for need in core_job_kinds() {
            assert!(registered.contains(&need.kind()), "{}", need.kind());
        }
    }
}
