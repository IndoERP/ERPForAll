//! The heartbeat role's privileges on the node tables (design §5).

use erp_fields::Result;

use crate::Transaction;
use crate::grantee::Grant;

const NODE_GRANT: Grant = Grant {
    call: "Transaction::grant_node_role",
    tables: &["erp_node", "erp_node_detail", "erp_node_rollout"],
    grantee: "the heartbeat role",
    missing: |table| {
        format!(
            "the node table {table} is missing: run the schema sync (Orm::sync_schema) before \
             granting the heartbeat role"
        )
    },
    owner: |role, table| {
        format!(
            "role {role:?} owns {table}: the heartbeat role must be another role (revoking from \
             the owner would remove what the schema sync needs)"
        )
    },
};

impl Transaction {
    /// Give the heartbeat role exactly what a membership needs: `SELECT, INSERT, UPDATE, DELETE`
    /// on `erp_node` and `erp_node_detail` (register, renew, purge, shut down) and `SELECT,
    /// DELETE` on `erp_node_rollout` (read the window, purge windows past the retention), after
    /// revoking everything else on the three tables. Opening and closing windows stays with the
    /// operator's role. The request role gets nothing on these tables: never call this for it.
    /// Granting twice gives the same privileges.
    ///
    /// Changing privileges is DDL: only a transaction of the `System` principal with full
    /// elevation may call this. The grant must restrict exactly the named role, so these are
    /// refused: `PUBLIC`, `NONE` and the predefined `pg_*` roles (the grant would reach every role
    /// or a whole class of them), a role that does not exist, the owner of a node table (revoking
    /// from it would remove what the schema sync needs), a superuser, a member of a table owner
    /// (directly or not, whether it inherits the owner's rights or may only `SET ROLE` to it), a
    /// role holding the `ADMIN OPTION` on a table owner or on a role leading to it, or with
    /// `CREATEROLE` (it can make itself a member), a role with `REPLICATION` (a base backup copies
    /// every table), and a role that reaches, through any of its memberships, a role with
    /// `SUPERUSER`, `CREATEROLE` or `REPLICATION` or one of `pg_write_all_data`,
    /// `pg_read_all_data`, `pg_execute_server_program`, `pg_write_server_files` or
    /// `pg_read_server_files` (privileges restrict none of them; the message names the role on the
    /// path). The checks hold when the grant runs: after changing the role, grant again.
    ///
    /// # Errors
    ///
    /// `Access` for any other principal; `Programming` for an invalid or refused role name (one of
    /// the cases above) and when a node table is missing (run the schema sync first); `Database`
    /// when a statement fails (this role may not grant).
    pub async fn grant_node_role(&mut self, role: &str) -> Result<()> {
        let role = self.check_grantee(&NODE_GRANT, role).await?;
        self.batch(&format!(
            "REVOKE ALL ON erp_node, erp_node_detail, erp_node_rollout FROM {role};\n\
             GRANT SELECT, INSERT, UPDATE, DELETE ON erp_node, erp_node_detail TO {role};\n\
             GRANT SELECT, DELETE ON erp_node_rollout TO {role}"
        ))
        .await
    }
}
