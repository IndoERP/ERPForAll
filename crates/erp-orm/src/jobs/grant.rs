//! The runtime role's privileges on the queue's tables ('s runtime grant contract;
//! ).
//!
//! The queue is written by the processes that serve requests (enqueue) and, in a single-role
//! deployment, by the runner of the same process (claim, lease, complete, retention). The
//! database role is not the principal boundary (the runtime role already writes every business
//! table); the boundary is the enqueue API. The grant is still narrow where narrowing costs
//! nothing: a job's identity, kind, payload and stored principal are written once at enqueue and
//! no privilege lets the runtime role change them afterwards, so a schedule whose user changes
//! is a delete and an insert, never an update of `run_as_*`.

use erp_fields::Result;

use crate::Transaction;
use crate::grantee::Grant;

/// Columns of `erp_job` the runtime role may update: everything the lifecycle moves (state,
/// lease, epoch, attempts, progress, counters, errors) and the schedule an operator or a cron
/// write changes (priority, `run_after`). Never the identity, kind, payload, keys or stored
/// principal columns.
pub const JOB_UPDATABLE_COLUMNS: &[&str] = &[
    "state",
    "failure_rank",
    "priority",
    "run_after",
    "started_at",
    "attempts",
    "lease_owner",
    "lease_epoch",
    "lease_until",
    "progress",
    "items_done",
    "items_failed",
    "items_remaining",
    "cancel_requested",
    "last_error_code",
    "last_error",
    "last_error_at",
    "dead_at",
];

/// The tables the grant covers.
pub const JOB_TABLES: &[&str] = &["erp_job", "erp_job_run", "erp_job_item_failure"];

const JOBS_GRANT: Grant = Grant {
    call: "Transaction::grant_jobs_runtime_role",
    tables: JOB_TABLES,
    grantee: "the runtime role",
    missing: |table| {
        format!(
            "the job table {table} is missing: run the schema sync (Orm::sync_schema) before \
             granting the runtime role"
        )
    },
    owner: |role, table| {
        format!(
            "role {role:?} owns {table}: the runtime role must be another role (revoking from \
             the owner would remove what the schema sync needs)"
        )
    },
};

/// The statements of the grant for the already quoted `role`.
pub(crate) fn grant_sql(role: &str) -> String {
    let updatable = JOB_UPDATABLE_COLUMNS.join(", ");
    format!(
        "REVOKE ALL ON erp_job, erp_job_run, erp_job_item_failure FROM {role};\n\
         GRANT SELECT, INSERT, DELETE ON erp_job TO {role};\n\
         GRANT UPDATE ({updatable}) ON erp_job TO {role};\n\
         GRANT SELECT, INSERT, DELETE ON erp_job_run TO {role};\n\
         GRANT SELECT, INSERT, DELETE ON erp_job_item_failure TO {role};\n\
         GRANT UPDATE (error_code, error, failed_at) ON erp_job_item_failure TO {role}"
    )
}

impl Transaction {
    /// Give the runtime role what the queue needs: `SELECT, INSERT, DELETE` on `erp_job` and
    /// `UPDATE` of the lifecycle columns only ([`JOB_UPDATABLE_COLUMNS`]), `SELECT, INSERT,
    /// DELETE` on `erp_job_run` (history, trimmed by the retention job), `SELECT, INSERT,
    /// DELETE` and `UPDATE` of the error columns on `erp_job_item_failure`, after revoking
    /// everything else on the three tables. `PUBLIC` gets nothing (the DDL revokes it). Granting
    /// twice gives the same privileges. The grantee checks are those of
    /// [`Transaction::grant_node_role`].
    ///
    /// Changing privileges is DDL: only a transaction of the `System` principal with full
    /// elevation may call this.
    ///
    /// # Errors
    ///
    /// `Access` for any other principal; `Programming` for a refused role name or when a job
    /// table is missing (run the schema sync first); `Database` when a statement fails.
    pub async fn grant_jobs_runtime_role(&mut self, role: &str) -> Result<()> {
        let role = self.check_grantee(&JOBS_GRANT, role).await?;
        self.batch(&grant_sql(&role)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grant_never_updates_identity_kind_payload_keys_or_principal_columns() {
        for forbidden in [
            "id",
            "job_uuid",
            "kind",
            "kind_version",
            "enqueued_at",
            "singleton_key",
            "dedup_key",
            "payload",
            "max_attempts",
            "run_as_kind",
            "run_as_user",
            "run_as_key",
            "run_as_delegating_user",
            "run_as_companies",
            "run_as_company",
            "enqueued_by_kind",
            "enqueued_by_user",
            "correlation_id",
            "causation_operation",
        ] {
            assert!(!JOB_UPDATABLE_COLUMNS.contains(&forbidden), "{forbidden}");
        }
    }

    #[test]
    fn every_updatable_column_is_a_column_of_the_table() {
        for column in JOB_UPDATABLE_COLUMNS {
            assert!(
                crate::jobs::schema::JOB_DDL.contains(&format!("\n    {column} ")),
                "{column}"
            );
        }
    }
}
