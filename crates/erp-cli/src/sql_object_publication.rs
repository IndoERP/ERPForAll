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
        .chain(
            [
                "erp_record_protection.binding",
                "erp_record_protection.owner",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn exclusive_id_sequences(registry: &Registry) -> Vec<&str> {
    registry
        .sql_objects()
        .objects()
        .iter()
        .filter_map(|o| match o.kind() {
            SqlObjectKind::TableGrant(g) => g.exclusive_id_sequence(),
            _ => None,
        })
        .collect()
}

/// Grant ordinary objects individually, excluding restricted tables and their owned sequences.
pub(crate) async fn grant_ordinary(
    tx: &mut Transaction,
    role: &str,
    registry: &Registry,
) -> Result<()> {
    let restricted = restricted_tables(registry);
    let sequences = exclusive_id_sequences(registry);
    let rows = tx.query("WITH RECURSIVE restricted(oid) AS (SELECT c.oid FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relname=ANY($1::text[]) UNION SELECT i.inhrelid FROM pg_catalog.pg_inherits i JOIN restricted r ON r.oid=i.inhparent) SELECT c.relkind::text, n.nspname, c.relname FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p','v','m','f','S') AND c.oid NOT IN (SELECT oid FROM restricted) AND NOT (c.relkind='S' AND c.relname=ANY($2::text[])) AND NOT (c.relkind='S' AND EXISTS (SELECT 1 FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_class t ON t.oid=d.refobjid WHERE d.classid='pg_catalog.pg_class'::regclass AND d.refclassid='pg_catalog.pg_class'::regclass AND d.objid=c.oid AND d.deptype IN ('a','i') AND t.relnamespace=n.oid AND t.oid IN (SELECT oid FROM restricted))) ORDER BY c.oid", &[&restricted,&sequences]).await?;
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
    // Only the loader may set or release protection. Mail's compatibility guard has read-only
    // access to the owner ledger; deleting a business record runs its foreign keys without
    // granting the request role any access to the protection bindings.
    let protected = tx
        .query(
            "SELECT relname::text FROM pg_catalog.pg_class \
             WHERE oid = pg_catalog.to_regclass('public.\"erp_record_protection.owner\"') \
             OR oid IN (SELECT relid FROM pg_catalog.pg_partition_tree(\
                 pg_catalog.to_regclass('public.\"erp_record_protection.binding\"')))",
            &[],
        )
        .await?;
    let role = erp_sql::quote(role);
    let mut grants = String::new();
    for row in protected {
        let name: String = row.try_get(0).map_err(|source| {
            erp_fields::Error::db("invalid record protection grant state").with_source(source)
        })?;
        let table = format!("public.{}", erp_sql::quote(&name));
        grants.push_str(&format!("REVOKE ALL ON {table} FROM PUBLIC, {role};"));
        if name == "erp_record_protection.owner" {
            grants.push_str(&format!("GRANT SELECT ON {table} TO {role};"));
        }
    }
    if !grants.is_empty() {
        tx.batch(&grants).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
