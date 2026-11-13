// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owner publication and effective authority checks for.
use crate::Transaction;
use erp_fields::{Error, Result};

const UNSAFE: &str = "a later release: audit reader privileges or role isolation are unsafe";

/// Other deployment identities from which an audit reader must remain isolated.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AuditReaderRoles {
    pub(super) names: Vec<String>,
    pub(super) serving: Vec<String>,
}
impl AuditReaderRoles {
    /// Declare trusted runtime, owner and optional heartbeat identities.
    /// # Errors
    /// `Programming` for empty, oversized, NUL-containing or duplicate role names.
    pub fn new(runtime: &str, owner: &str, node: Option<&str>) -> Result<Self> {
        let mut names = vec![runtime.to_owned(), owner.to_owned()];
        if let Some(node) = node {
            names.push(node.to_owned());
        }
        names.sort();
        if names.iter().any(|name| !valid_name(name)) || names.windows(2).any(|p| p[0] == p[1]) {
            return Err(unsafe_role());
        }
        let mut serving = vec![runtime.to_owned()];
        if let Some(node) = node {
            serving.push(node.to_owned());
        }
        Ok(Self { names, serving })
    }
    pub(super) fn accepts(&self, role: &str) -> bool {
        valid_name(role)
            && role != "PUBLIC"
            && !role.starts_with("pg_")
            && !self.names.iter().any(|name| name == role)
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && !name.contains('\0')
}
pub(super) fn unsafe_role() -> Error {
    Error::programming(UNSAFE)
}

// Every membership path counts, including NOINHERIT, SET FALSE and ADMIN OPTION: its holder can
// widen membership options later. Catalogs are explicitly qualified; no search_path trust.
// $3 allows publication to reconcile direct managed ACLs before the final exact-rights check.
pub(super) const CHECK_SQL: &str = r"
WITH RECURSIVE reach(oid) AS (
 SELECT oid FROM pg_catalog.pg_roles WHERE rolname=$1
 UNION SELECT m.roleid FROM pg_catalog.pg_auth_members m JOIN reach r ON r.oid=m.member
), members(oid) AS (
 SELECT oid FROM pg_catalog.pg_roles WHERE rolname=$1
 UNION SELECT m.member FROM pg_catalog.pg_auth_members m JOIN members r ON r.oid=m.roleid
), audit AS (
 SELECT c.oid FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
 WHERE n.nspname='erp_event' AND c.relname='ir_audit_entry' AND c.relkind='p'
), candidates AS (
 SELECT c.*, n.nspname FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
 WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%'
)
SELECT NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname=$1 AND rolcanlogin)
 OR NOT EXISTS (SELECT FROM audit)
 OR EXISTS (SELECT FROM reach r JOIN pg_catalog.pg_roles p ON p.oid=r.oid
   WHERE p.rolsuper OR p.rolbypassrls OR p.rolcreaterole OR p.rolcreatedb OR p.rolreplication
      OR p.rolname=ANY($2::text[]) OR p.rolname LIKE 'pg\_%' ESCAPE '\')
 OR EXISTS (SELECT FROM members m JOIN pg_catalog.pg_roles p ON p.oid=m.oid WHERE p.rolname=ANY($4::text[]))
 OR EXISTS (SELECT FROM reach r CROSS JOIN pg_catalog.pg_database d WHERE d.datname=pg_catalog.current_database()
   AND (d.datdba=r.oid OR pg_catalog.has_database_privilege(r.oid,d.oid,'CREATE')
        OR pg_catalog.has_database_privilege(r.oid,d.oid,'TEMPORARY')))
 OR EXISTS (SELECT FROM reach r CROSS JOIN pg_catalog.pg_namespace n
   WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%'
   AND (n.nspowner=r.oid OR ((NOT $3 OR n.nspname<>'erp_event') AND pg_catalog.has_schema_privilege(r.oid,n.oid,'CREATE,USAGE WITH GRANT OPTION'))))
 OR EXISTS (SELECT FROM reach r CROSS JOIN candidates c
   WHERE c.relkind IN ('r','p','v','m','f','S') AND c.relowner=r.oid)
 OR EXISTS (SELECT FROM reach r CROSS JOIN candidates c
   WHERE (NOT $3 OR c.nspname<>'erp_event') AND CASE WHEN c.relkind='S' THEN
     pg_catalog.has_sequence_privilege(r.oid,c.oid,'USAGE,SELECT,UPDATE')
   WHEN c.relkind IN ('r','p','v','m','f') THEN
     pg_catalog.has_table_privilege(r.oid,c.oid,'INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER,MAINTAIN')
     OR pg_catalog.has_any_column_privilege(r.oid,c.oid,'INSERT,UPDATE,REFERENCES')
     OR pg_catalog.has_table_privilege(r.oid,c.oid,'SELECT WITH GRANT OPTION')
     OR pg_catalog.has_any_column_privilege(r.oid,c.oid,'SELECT WITH GRANT OPTION')
     OR (c.oid NOT IN (SELECT oid FROM audit) AND
         (pg_catalog.has_table_privilege(r.oid,c.oid,'SELECT') OR pg_catalog.has_any_column_privilege(r.oid,c.oid,'SELECT')))
   ELSE false END)
 OR EXISTS (SELECT FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace CROSS JOIN reach r
   WHERE n.nspname NOT IN ('pg_catalog','information_schema')
     AND (p.proowner=r.oid OR (p.prosecdef AND pg_catalog.has_function_privilege(r.oid,p.oid,'EXECUTE'))))
 OR EXISTS (SELECT FROM candidates c CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) a
   WHERE c.nspname='erp_event' AND a.grantee=0)
 OR EXISTS (SELECT FROM candidates c JOIN pg_catalog.pg_attribute at ON at.attrelid=c.oid
   CROSS JOIN LATERAL pg_catalog.aclexplode(at.attacl) a WHERE c.nspname='erp_event' AND a.grantee=0)
 OR (NOT $3 AND (NOT pg_catalog.has_schema_privilege($1,'erp_event','USAGE')
   OR NOT pg_catalog.has_table_privilege($1,(SELECT oid FROM audit),'SELECT')))
";

impl Transaction {
    /// Publish exact parent-only SELECT rights for a dedicated audit reader.
    /// Direct event ACL drift is reconciled, including column and partition grants. PUBLIC,
    /// inherited or unrelated excess rights are refused; shared privileges are never rewritten.
    /// # Errors
    /// `Access` unless System/Full; `Programming` for unsafe roles or effective privileges;
    /// `Database` when publication cannot inspect or change the owned event objects.
    pub async fn grant_audit_reader_role(
        &mut self,
        role: &str,
        isolated: &AuditReaderRoles,
    ) -> Result<()> {
        self.require_system_full("Transaction::grant_audit_reader_role")?;
        if !isolated.accepts(role) {
            return Err(unsafe_role());
        }
        self.check_reader_role(role, isolated, true).await?;
        let role_sql = erp_sql::quote(role);
        self.batch(&format!("REVOKE ALL ON SCHEMA erp_event FROM {role_sql}; REVOKE ALL ON ALL TABLES IN SCHEMA erp_event FROM {role_sql}; REVOKE ALL ON ALL SEQUENCES IN SCHEMA erp_event FROM {role_sql}")).await?;
        // Table REVOKE does not remove column privileges. Reconcile those separately, also on
        // partitions outside the current premake window. Identifiers come from the catalog.
        let relations = self.query("SELECT c.relname::text, array_agg(a.attname::text ORDER BY a.attnum) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped WHERE n.nspname='erp_event' AND c.relkind IN ('r','p','v','m','f') GROUP BY c.oid,c.relname", &[]).await?;
        for row in relations {
            let table: String = row.try_get(0).map_err(|_| unsafe_role())?;
            let columns: Vec<String> = row.try_get(1).map_err(|_| unsafe_role())?;
            let columns = columns
                .iter()
                .map(|c| erp_sql::quote(c))
                .collect::<Vec<_>>()
                .join(",");
            self.batch(&format!("REVOKE SELECT ({columns}), INSERT ({columns}), UPDATE ({columns}), REFERENCES ({columns}) ON erp_event.{} FROM {role_sql}", erp_sql::quote(&table))).await?;
        }
        self.batch(&format!("GRANT USAGE ON SCHEMA erp_event TO {role_sql}; GRANT SELECT ON erp_event.ir_audit_entry TO {role_sql}")).await?;
        self.check_reader_role(role, isolated, false).await
    }
    async fn check_reader_role(
        &mut self,
        role: &str,
        isolated: &AuditReaderRoles,
        reconcile: bool,
    ) -> Result<()> {
        let rows = self
            .query(
                CHECK_SQL,
                &[&role, &isolated.names, &reconcile, &isolated.serving],
            )
            .await?;
        let [row] = rows.as_slice() else {
            return Err(unsafe_role());
        };
        if row.try_get::<_, bool>(0).map_err(|_| unsafe_role())? {
            return Err(unsafe_role());
        }
        Ok(())
    }
}
