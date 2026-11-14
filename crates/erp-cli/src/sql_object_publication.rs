// SPDX-License-Identifier: MIT OR Apache-2.0
//! Shared node configuration and generic grant exclusions for declared SQL objects.
use crate::{Result, human::failed};
use erp_orm::{SchemaSyncOptions, SqlRoleBindings, Transaction};
use erp_registry::{Registry, SqlObjectKind};
use erp_server::config::ServerConfig;

pub(crate) fn options(config: &ServerConfig) -> Result<SchemaSyncOptions> {
    let runtime = config.runtime_connection();
    let request = runtime
        .get_user()
        .ok_or_else(|| failed("runtime role missing"))?;
    let mut roles = SqlRoleBindings::new().request_owned(request.to_owned());
    if let Some(role) = config.sql_object_definer_role() {
        roles = roles.definer_owned(role.to_owned());
    }
    if let Some(role) = config.sql_object_maintenance_role() {
        roles = roles.maintenance_owned(role.to_owned());
    }
    Ok(SchemaSyncOptions::new().sql_roles(roles))
}

pub(crate) fn restricted_tables(registry: &Registry) -> Vec<String> {
    registry
        .sql_objects()
        .objects()
        .iter()
        .filter_map(|object| match object.kind() {
            SqlObjectKind::TableGrant(grant) => Some(grant.model().table().to_owned()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Grant ordinary objects individually, excluding restricted tables and their owned sequences.
pub(crate) async fn grant_ordinary(
    tx: &mut Transaction,
    role: &str,
    registry: &Registry,
) -> Result<()> {
    let restricted = restricted_tables(registry);
    let rows = tx.query("WITH RECURSIVE restricted(oid) AS (SELECT c.oid FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relname=ANY($1::text[]) UNION SELECT i.inhrelid FROM pg_catalog.pg_inherits i JOIN restricted r ON r.oid=i.inhparent) SELECT c.relkind::text, n.nspname, c.relname FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p','v','m','f','S') AND c.oid NOT IN (SELECT oid FROM restricted) AND NOT (c.relkind='S' AND EXISTS (SELECT 1 FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_class t ON t.oid=d.refobjid WHERE d.classid='pg_catalog.pg_class'::regclass AND d.refclassid='pg_catalog.pg_class'::regclass AND d.objid=c.oid AND d.deptype IN ('a','i') AND t.relnamespace=n.oid AND t.oid IN (SELECT oid FROM restricted))) ORDER BY c.oid", &[&restricted]).await?;
    let mut sql = format!("GRANT USAGE ON SCHEMA public TO {};", erp_sql::quote(role));
    for row in rows {
        let kind: String = row.try_get(0).map_err(|e| {
            erp_fields::Error::db("invalid ordinary grant object kind").with_source(e)
        })?;
        let schema: String = row
            .try_get(1)
            .map_err(|e| erp_fields::Error::db("invalid ordinary grant schema").with_source(e))?;
        let name: String = row
            .try_get(2)
            .map_err(|e| erp_fields::Error::db("invalid ordinary grant identity").with_source(e))?;
        let (ops, object) = if kind == "S" {
            ("USAGE, SELECT", "SEQUENCE")
        } else {
            ("SELECT, INSERT, UPDATE, DELETE", "TABLE")
        };
        sql.push_str(&format!(
            "GRANT {ops} ON {object} {}.{} TO {};",
            erp_sql::quote(&schema),
            erp_sql::quote(&name),
            erp_sql::quote(role)
        ));
    }
    tx.batch(&sql).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
