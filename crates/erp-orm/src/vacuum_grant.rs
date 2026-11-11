//! Privileges on the transient vacuum state table (package D).
//!
//! The state table `erp_transient_vacuum_state` holds the cleanup leases. Only the maintenance
//! role that runs [`Orm::vacuum_transient`](crate::Orm::vacuum_transient) writes it; the request
//! role gets nothing, so a request can neither take a lease nor read or change a cleanup cycle.
//! The grantee checks are those of the other core-table grants ([`crate::grantee`]).

use erp_fields::Result;

use crate::Transaction;
use crate::grantee::Grant;

const VACUUM_GRANT: Grant = Grant {
    call: "Transaction::grant_vacuum_role",
    tables: &["erp_transient_vacuum_state"],
    grantee: "the maintenance role",
    missing: |table| {
        format!(
            "the vacuum table {table} is missing: run the schema sync (Orm::sync_schema) before \
             granting the maintenance role"
        )
    },
    owner: |role, table| {
        format!(
            "role {role:?} owns {table}: the maintenance role must be another role (revoking \
             from the owner would remove what the schema sync needs)"
        )
    },
};

const REVOKE_GRANT: Grant = Grant {
    call: "Transaction::revoke_vacuum_request_role",
    ..VACUUM_GRANT
};

impl Transaction {
    /// Give the maintenance role what the vacuum needs on its state table: `SELECT, INSERT,
    /// UPDATE, DELETE`, after revoking everything else. The rows of the transient models
    /// themselves are covered by the role's ordinary data privileges, which this call does not
    /// touch. Granting twice gives the same privileges. The grantee checks are those of
    /// [`Transaction::grant_node_role`].
    ///
    /// # Errors
    ///
    /// `Access` for any principal but `System` with full elevation; `Programming` for a refused
    /// role name or when the state table is missing (run the schema sync first); `Database` when
    /// a statement fails.
    pub async fn grant_vacuum_role(&mut self, role: &str) -> Result<()> {
        let role = self.check_grantee(&VACUUM_GRANT, role).await?;
        self.batch(&format!(
            "REVOKE ALL ON erp_transient_vacuum_state FROM {role};\n\
             GRANT SELECT, INSERT, UPDATE, DELETE ON erp_transient_vacuum_state TO {role}"
        ))
        .await
    }

    /// Take every privilege on the vacuum state table from the request (runtime) role. A blanket
    /// grant on all tables of the schema reaches the table, so the grant publication calls this
    /// after it.
    ///
    /// # Errors
    ///
    /// As [`Transaction::grant_vacuum_role`].
    pub async fn revoke_vacuum_request_role(&mut self, role: &str) -> Result<()> {
        let role = self.check_grantee(&REVOKE_GRANT, role).await?;
        self.batch(&format!(
            "REVOKE ALL ON erp_transient_vacuum_state FROM {role}"
        ))
        .await
    }
}
