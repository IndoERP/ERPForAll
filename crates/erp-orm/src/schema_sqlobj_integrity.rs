// SPDX-License-Identifier: MIT OR Apache-2.0
//! B6 catalogue reads and in-transaction FK installation. Child of schema_sqlobj: shares its
//! private plan and provenance vocabulary, never a second lifecycle executor.
use super::*;

/// A portable installed fingerprint; catalogue OIDs never enter it. The deparser plus structural
/// flags retains actions, deferral, validation, ordered columns and the selected unique index.
pub(super) async fn observed(tx: &mut Transaction, object: &SqlObject) -> Result<Option<String>> {
    let (sql, table, name) = match object.kind() {
        SqlObjectKind::ForeignKey(f) => (
            "SELECT jsonb_build_array(pg_get_constraintdef(k.oid), k.contype::text, \
             k.convalidated, k.conenforced, k.condeferrable, k.condeferred, \
             k.confupdtype::text, k.confdeltype::text, k.confmatchtype::text, \
             (SELECT array_agg(jsonb_build_array(a.attname::text, format_type(a.atttypid,a.atttypmod), a.attnotnull, a.attgenerated::text) ORDER BY u.i) FROM unnest(k.conkey) WITH \
             ORDINALITY u(n,i) JOIN pg_attribute a ON a.attrelid=k.conrelid AND a.attnum=u.n), \
             tn.nspname, t.relname, \
             (SELECT array_agg(jsonb_build_array(a.attname::text, format_type(a.atttypid,a.atttypmod), a.attnotnull, a.attgenerated::text) ORDER BY u.i) FROM unnest(k.confkey) WITH \
             ORDINALITY u(n,i) JOIN pg_attribute a ON a.attrelid=k.confrelid AND a.attnum=u.n), \
             (SELECT array_agg(jsonb_build_array(a.attname::text, format_type(a.atttypid,a.atttypmod), a.attnotnull, a.attgenerated::text) ORDER BY u.i) FROM unnest(k.confdelsetcols) WITH \
             ORDINALITY u(n,i) JOIN pg_attribute a ON a.attrelid=k.conrelid AND a.attnum=u.n), \
             ni.nspname, ix.relname, pg_get_userbyid(l.relowner))::text \
             FROM pg_constraint k JOIN pg_class l ON l.oid=k.conrelid \
             JOIN pg_namespace ln ON ln.oid=l.relnamespace \
             LEFT JOIN pg_class t ON t.oid=k.confrelid LEFT JOIN pg_namespace tn ON tn.oid=t.relnamespace \
             LEFT JOIN pg_class ix ON ix.oid=k.conindid LEFT JOIN pg_namespace ni ON ni.oid=ix.relnamespace \
             WHERE ln.nspname=current_schema() AND l.relname=$1 AND k.conname=$2",
            f.model().table(),
            f.name(),
        ),
        SqlObjectKind::TableOptions(o) => (
            "SELECT jsonb_build_array(n.nspname,c.relname,c.relkind::text,c.relpersistence::text, \
             pg_get_userbyid(c.relowner))::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
             WHERE n.nspname=current_schema() AND c.relname=$1 AND $2::text=''",
            o.model().table(),
            "",
        ),
        _ => {
            return Err(refuse(
                object.id(),
                "an integrity catalogue read needs an FK or table options",
            ));
        }
    };
    let rows = tx.query(sql, &[&table, &name]).await?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => row
            .try_get(0)
            .map(Some)
            .map_err(|e| unreadable("integrity fingerprint", e)),
        _ => Err(refuse(
            object.id(),
            "integrity catalogue identity is not unique",
        )),
    }
}

impl Transaction {
    /// Recognize the exact supported old/new core CHECK. No DDL here and no unknown-catalogue adoption.
    pub(super) async fn plan_integrity_catalog(
        &mut self,
        provenance: bool,
        needed: bool,
    ) -> Result<bool> {
        if !provenance || !needed {
            return Ok(false);
        }
        let rows = self.query("SELECT pg_get_constraintdef(oid), convalidated, conenforced FROM pg_constraint \
            WHERE conrelid=to_regclass('erp_schema_sql_object') AND conname='erp_schema_sql_object_kind_check' AND contype='c'", &[]).await?;
        let Some(row) = rows.first().filter(|_| rows.len() == 1) else {
            return Err(Error::programming(
                "SQL object catalogue: supported kind CHECK is missing or ambiguous",
            ));
        };
        let definition: String = row.try_get(0).map_err(|e| unreadable("kind CHECK", e))?;
        let valid: bool = row
            .try_get(1)
            .map_err(|e| unreadable("kind CHECK validation", e))?;
        let enforced: bool = row
            .try_get(2)
            .map_err(|e| unreadable("kind CHECK enforcement", e))?;
        let old =
            "CHECK ((kind = ANY (ARRAY['function'::text, 'trigger'::text, 'condition'::text])))";
        let new = "CHECK ((kind = ANY (ARRAY['function'::text, 'trigger'::text, 'condition'::text, 'foreign_key'::text, 'table_options'::text])))";
        if !valid || !enforced || (definition != old && definition != new) {
            return Err(Error::programming(
                "SQL object catalogue: unknown kind CHECK shape; explicit migration required",
            ));
        }
        Ok(definition == old)
    }

    pub(super) async fn install_integrity_catalog(
        &mut self,
        sync: &mut SqlObjectSync<'_>,
    ) -> Result<()> {
        if sync.upgrade_integrity {
            self.batch("ALTER TABLE erp_schema_sql_object DROP CONSTRAINT erp_schema_sql_object_kind_check; \
                ALTER TABLE erp_schema_sql_object ADD CONSTRAINT erp_schema_sql_object_kind_check \
                CHECK (kind IN ('function','trigger','condition','foreign_key','table_options'))").await?;
            sync.ran = true;
        }
        Ok(())
    }

    /// Install only fresh FKs. Shape changes were refused by S0; every constraint is validated
    /// before provenance is recorded. Large/pending paths are refused before mutation by schema.rs.
    pub(crate) async fn install_sql_object_constraints(
        &mut self,
        sync: &mut SqlObjectSync<'_>,
    ) -> Result<()> {
        for item in &sync.items {
            if item.phase != Phase::Integrity || item.action == Action::Unchanged {
                continue;
            }
            let SqlObjectKind::ForeignKey(f) = item.object.kind() else {
                return Err(refuse(
                    item.object.id(),
                    "integrity phase contains a non-FK object",
                ));
            };
            let sql = format!(
                "{};\n{}",
                sqlobj::create_foreign_key(f, true),
                erp_sql::ddl::validate_constraint(f.model().table(), f.name())
            );
            self.sql_object_ddl(item.object.id(), &sql).await?;
            let expected = match f.target_key() {
                erp_registry::SqlForeignKeyTarget::PrimaryKey => {
                    // PostgreSQL chooses this identity (including truncation/collision suffixes).
                    // The ERP identifier-shortening algorithm must not guess its name.
                    let rows = self.query("SELECT i.relname::text FROM pg_constraint p JOIN pg_class i ON i.oid=p.conindid WHERE p.conrelid=to_regclass($1) AND p.contype='p'", &[&erp_sql::quote(f.target().table())]).await?;
                    rows.first()
                        .filter(|_| rows.len() == 1)
                        .ok_or_else(|| {
                            refuse(
                                item.object.id(),
                                "target primary key is missing or ambiguous",
                            )
                        })?
                        .try_get::<_, String>(0)
                        .map_err(|e| unreadable("target primary key", e))?
                }
                erp_registry::SqlForeignKeyTarget::Key(name) => self
                    .registry()
                    .model(f.target().model())
                    .and_then(|m| m.business_key(name))
                    .map(|k| k.full_name.clone())
                    .ok_or_else(|| refuse(item.object.id(), "target unique key is not resolved"))?,
            };
            let found = self.query("SELECT i.relname::text FROM pg_constraint c JOIN pg_class i ON i.oid=c.conindid WHERE c.conrelid=to_regclass($1) AND c.conname=$2", &[&erp_sql::quote(f.model().table()), &f.name()]).await?;
            let actual: String = found
                .first()
                .filter(|_| found.len() == 1)
                .ok_or_else(|| refuse(item.object.id(), "installed target unique key is missing"))?
                .try_get(0)
                .map_err(|e| unreadable("target unique key", e))?;
            if actual != expected {
                return Err(refuse(
                    item.object.id(),
                    "target unique key differs; explicit migration required",
                ));
            }

            sync.ran = true;
        }
        Ok(())
    }
}
