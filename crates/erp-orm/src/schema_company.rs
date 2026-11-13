// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit, transaction-owned predecessor migration.
use std::collections::BTreeSet;
use std::sync::Arc;

use erp_fields::{Error, ErrorKind, FieldType, Result, Storage};
use erp_registry::{FieldSpec, ModelSpec};
use erp_sql::{ddl, quote};

use crate::{Transaction, company_dependent};

/// Explicit ownership of a predecessor scalar, used by a trusted native pre-migration.
///
/// Construction validates the policy; execution checks companies against the tenant database.
/// This value never derives ownership from the installer's environment.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CompanyMigrationPolicy {
    owners: Owners,
}
#[derive(Debug, Clone)]
enum Owners {
    Companies(Vec<i64>),
    RowCompany(String),
}
impl CompanyMigrationPolicy {
    /// Maximum explicit owners per scalar. Bounds DDL expression size as well as output maps.
    pub const MAX_OWNERS: usize = 256;

    /// Assign every non-null predecessor scalar to this nonempty, duplicate-free company set.
    ///
    /// # Errors
    /// Programming for empty, duplicate, nonpositive or more than [`Self::MAX_OWNERS`] IDs.
    pub fn companies(ids: impl IntoIterator<Item = i64>) -> Result<Self> {
        let mut owners = BTreeSet::new();
        for id in ids {
            if id <= 0 || !owners.insert(id) || owners.len() > Self::MAX_OWNERS {
                return Err(Error::programming(
                    "invalid company migration policy: owners must be unique positive IDs within MAX_OWNERS",
                ));
            }
        }
        if owners.is_empty() {
            return Err(Error::programming(
                "invalid company migration policy: at least one company is required",
            ));
        }
        Ok(Self {
            owners: Owners::Companies(owners.into_iter().collect()),
        })
    }

    /// Assign each non-null predecessor scalar to its stored ordinary `res.company` Many2one.
    ///
    /// # Errors
    /// Programming for a noncanonical/oversized field name. Execution validates its declaration.
    pub fn row_company(field: &str) -> Result<Self> {
        if field.len() > erp_registry::ident::MAX_IDENTIFIER_LEN
            || !erp_registry::ident::is_valid_pg_name(field)
        {
            return Err(Error::programming(
                "invalid company migration policy: row-company field must be a PostgreSQL field identifier",
            ));
        }
        Ok(Self {
            owners: Owners::RowCompany(field.to_owned()),
        })
    }
}

fn read_error(error: tokio_postgres::Error) -> Error {
    Error::new(
        ErrorKind::Database,
        "could not read company migration admission",
    )
    .with_source(error)
}
fn invalid(model: &ModelSpec, field: &FieldSpec, detail: &str) -> Error {
    Error::validation(format!("{}.{}: {detail}", model.name, field.name))
}

/// SQL canonical scalar encoding. Exact numeric conversion stays inside PostgreSQL text.
fn scalar_expression(field: &FieldSpec) -> Result<String> {
    let col = quote(field.name);
    Ok(match field.ty.storage() {
        Storage::Decimal => format!("to_jsonb({col}::text)"),
        Storage::Date => format!("to_jsonb(to_char({col}, 'YYYY-MM-DD'))"),
        Storage::DateTime => format!(
            "to_jsonb(to_char({col}, 'YYYY-MM-DD HH24:MI:SS') || CASE WHEN extract(microseconds FROM {col})::int8 % 1000000 = 0 THEN '' ELSE to_char({col}, '.US') END)"
        ),
        Storage::Bool | Storage::Int | Storage::Float | Storage::Id | Storage::Str => {
            format!("to_jsonb({col})")
        }
        // Declaration admission excludes all other kinds before any DDL.
        _ => {
            return Err(Error::programming(
                "company migration target has unsupported scalar storage",
            ));
        }
    })
}

impl Transaction {
    /// Convert a genuine scalar predecessor into a company map using explicit ownership.
    ///
    /// Call from a trusted native **pre** migration, on the target registry, before schema sync.
    /// The helper takes schema/table locks, preserves exact numbers and the column identity,
    /// and never commits. Existing maps are validated without reassigning their owners; an empty
    /// map normalizes to SQL NULL. Roll back the surrounding upgrade on any failure.
    ///
    /// # Errors
    /// Access for a non-elevated caller; Programming for unknown/unsupported declarations,
    /// incompatible predecessor types or unsafe catalog dependencies; Validation for invalid
    /// ownership, values and map limits. Database/lock errors retain their sources and SQLSTATE.
    pub async fn migrate_company_dependent(
        &mut self,
        model: &str,
        field: &str,
        policy: &CompanyMigrationPolicy,
    ) -> Result<()> {
        if !self.env().is_superuser() {
            return Err(Error::new(
                ErrorKind::Access,
                "company migration requires an elevated native migration transaction",
            ));
        }
        let registry = Arc::clone(self.registry());
        let spec = registry
            .model(model)
            .ok_or_else(|| Error::programming("company migration target model does not exist"))?;
        let field = spec
            .field(field)
            .ok_or_else(|| Error::programming("company migration target field does not exist"))?;
        if !spec.has_generic_table()
            || spec.partition.is_some()
            || !field.company_dependent
            || field.translate
            || field.generated().is_some()
            || field.column().is_none()
            || !company_dependent::supports(&field.ty)
        {
            return Err(Error::programming(format!(
                "{}.{}: company migration requires an ordinary stored company-dependent scalar on an unpartitioned table",
                spec.name, field.name
            )));
        }
        // No staged scalar writes may be flushed after the column has become a map.
        self.invalidate_all().await?;
        let (saved, timeout) = self.set_schema_limits(false).await?;
        self.take_schema_lock(&timeout).await?;
        let table = quote(&spec.table);
        self.batch(&format!("LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE"))
            .await?;
        let rows = self.query(
            "SELECT t.typname,a.attnum::int4,c.relkind::text,a.attgenerated::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_type t ON t.oid=a.atttypid WHERE c.oid=to_regclass($1) AND a.attname=$2 AND a.attnum>0 AND NOT a.attisdropped",
            &[&table, &field.name],
        ).await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::programming("company migration predecessor column is missing"))?;
        let udt: String = row.try_get(0).map_err(read_error)?;
        let attnum: i32 = row.try_get(1).map_err(read_error)?;
        let kind: String = row.try_get(2).map_err(read_error)?;
        let generated: String = row.try_get(3).map_err(read_error)?;
        if kind != "r" || !generated.is_empty() {
            return Err(Error::programming(
                "company migration predecessor must be an ordinary nongenerated table column",
            ));
        }
        let col = quote(field.name);
        if udt == "jsonb" {
            self.check_company_storage(spec, field).await?;
            self.execute(
                &format!("UPDATE {table} SET {col}=NULL WHERE {col}='{{}}'::jsonb"),
                &[],
            )
            .await?;
            self.batch(&format!(
                "ALTER TABLE {table} ALTER COLUMN {col} DROP NOT NULL"
            ))
            .await?;
        } else {
            let expected = field.ty.column_type().ok_or_else(|| {
                Error::programming("company migration target has no scalar column type")
            })?;
            if udt != expected.udt_name() {
                return Err(Error::programming(format!(
                    "{}.{}: incompatible company migration predecessor type '{udt}'; expected '{}'",
                    spec.name,
                    field.name,
                    expected.udt_name()
                )));
            }
            let expression = scalar_expression(field)?;
            let patch = self
                .company_migration_owners(spec, field, policy, &expression)
                .await?;
            self.check_company_scalar(spec, field, &expression, policy)
                .await?;
            let drops = self
                .company_migration_dependencies(spec, field, attnum)
                .await?;
            for statement in drops {
                self.batch(&statement).await?;
            }
            // SQL NULL carries no ownership; do not turn it into an explicit per-company null.
            self.batch(&format!("ALTER TABLE {table} ALTER COLUMN {col} DROP DEFAULT, ALTER COLUMN {col} DROP NOT NULL, ALTER COLUMN {col} TYPE jsonb USING CASE WHEN {col} IS NULL THEN NULL ELSE {patch} END")).await?;
        }
        self.check_company_storage(spec, field).await?;
        self.invalidate_all().await?;
        self.signal_schema_change().await?;
        self.set_timeouts(&saved.0, &saved.1).await
    }

    async fn company_migration_owners(
        &mut self,
        spec: &ModelSpec,
        field: &FieldSpec,
        policy: &CompanyMigrationPolicy,
        scalar: &str,
    ) -> Result<String> {
        let registry = Arc::clone(self.registry());
        let companies = registry
            .model("res.company")
            .filter(|m| m.has_generic_table())
            .ok_or_else(|| {
                Error::programming("company migration requires the res.company model")
            })?;
        let company_table = quote(&companies.table);
        let table = quote(&spec.table);
        let col = quote(field.name);
        match &policy.owners {
            Owners::Companies(ids) => {
                let found = self
                    .query(
                        &format!("SELECT id FROM {company_table} WHERE id=ANY($1) FOR KEY SHARE"),
                        &[ids],
                    )
                    .await?;
                if found.len() != ids.len() {
                    return Err(invalid(
                        spec,
                        field,
                        "company migration policy names an unknown company",
                    ));
                }
                // PostgreSQL functions admit at most 100 arguments. Groups of 32 keep each call
                // below that bound; MAX_OWNERS also bounds concatenation and rendered DDL bytes.
                let groups = ids
                    .chunks(32)
                    .map(|chunk| {
                        let pairs = chunk
                            .iter()
                            .map(|id| format!("'{id}', {scalar}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("jsonb_build_object({pairs})")
                    })
                    .collect::<Vec<_>>();
                Ok(format!("({})", groups.join(" || ")))
            }
            Owners::RowCompany(name) => {
                let owner = spec.field(name).ok_or_else(|| {
                    Error::programming("company migration row-company field does not exist")
                })?;
                if owner.company_dependent
                    || owner.translate
                    || owner.generated().is_some()
                    || owner.column().is_none()
                    || !matches!(&owner.ty, FieldType::Many2one { comodel, .. } if *comodel == "res.company")
                {
                    return Err(Error::programming(
                        "company migration row-company field must be an ordinary stored Many2one to res.company",
                    ));
                }
                let owner = quote(owner.name);
                let bad = self.query(&format!("SELECT EXISTS(SELECT FROM {table} t LEFT JOIN {company_table} c ON c.id=t.{owner} WHERE t.{col} IS NOT NULL AND c.id IS NULL)"), &[]).await?;
                if bad[0].try_get::<_, bool>(0).map_err(read_error)? {
                    return Err(invalid(
                        spec,
                        field,
                        "non-null predecessor value has no valid company owner",
                    ));
                }
                // Lock owners without copying an unbounded company-ID result into Rust.
                self.query(&format!("SELECT count(*) FROM (SELECT id FROM {company_table} WHERE id IN (SELECT {owner} FROM {table} WHERE {col} IS NOT NULL) FOR KEY SHARE) locked_owners"), &[]).await?;
                Ok(format!("jsonb_build_object({owner}::text, {scalar})"))
            }
        }
    }

    async fn check_company_scalar(
        &mut self,
        spec: &ModelSpec,
        field: &FieldSpec,
        expression: &str,
        policy: &CompanyMigrationPolicy,
    ) -> Result<()> {
        let owners = match &policy.owners {
            Owners::Companies(ids) => ids.len(),
            Owners::RowCompany(_) => 1,
        };
        // Each ID has at most 19 decimal bytes; budget JSON text conservatively before transfer.
        let limit = company_dependent::MAX_MAP_BYTES / owners - 128;
        let table = quote(&spec.table);
        let col = quote(field.name);
        let sql = format!(
            "WITH candidates AS MATERIALIZED (SELECT id, {expression} AS value FROM {table} WHERE {col} IS NOT NULL AND ($1::int8 IS NULL OR id>$1) ORDER BY id LIMIT 64), sized AS MATERIALIZED (SELECT id, value, octet_length(value::text)::int8 AS bytes FROM candidates), bounded AS (SELECT *,sum(LEAST(bytes,{limit})+32) OVER(ORDER BY id) AS total FROM sized) SELECT id,CASE WHEN bytes<={limit} THEN value ELSE NULL END,bytes>{limit} FROM bounded WHERE total<={limit}+32 OR id=(SELECT min(id) FROM candidates) ORDER BY id"
        );
        let mut after: Option<i64> = None;
        loop {
            let rows = self.query(&sql, &[&after]).await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let oversized: bool = row.try_get(2).map_err(read_error)?;
                if oversized {
                    return Err(invalid(
                        spec,
                        field,
                        "invalid predecessor value or company-map byte limit exceeded",
                    ));
                }
                let value: serde_json::Value = row
                    .try_get(1)
                    .map_err(|_| invalid(spec, field, "invalid predecessor value"))?;
                company_dependent::decode(&field.ty, &value)
                    .map_err(|_| invalid(spec, field, "invalid predecessor value"))?;
                after = Some(row.try_get(0).map_err(read_error)?);
            }
        }
        Ok(())
    }

    /// Catalog dependencies are admitted before DDL. Unknown/custom rules are never dropped.
    pub(crate) async fn company_migration_dependencies(
        &mut self,
        spec: &ModelSpec,
        field: &FieldSpec,
        attnum: i32,
    ) -> Result<Vec<String>> {
        let table = quote(&spec.table);
        let mut drops = Vec::new();
        let rows = self.query("SELECT conname,contype::text,cardinality(conkey),confrelid::regclass::text,confdeltype::text,conrelid=to_regclass($1),NOT condeferrable AND confupdtype='a' AND confmatchtype='s' AND cardinality(confkey)=1 AND EXISTS(SELECT FROM pg_attribute target_key WHERE target_key.attrelid=confrelid AND target_key.attnum=confkey[1] AND target_key.attname='id') FROM pg_constraint WHERE (conrelid=to_regclass($1) AND $2::int4=ANY(conkey)) OR (confrelid=to_regclass($1) AND $2::int4=ANY(confkey)) LIMIT 1025", &[&table, &attnum]).await?;
        if rows.len() > 1024 {
            return Err(Error::validation(
                "company migration dependency limit exceeded",
            ));
        }
        for row in rows {
            let name: String = row.try_get(0).map_err(read_error)?;
            let kind: String = row.try_get(1).map_err(read_error)?;
            let count: i32 = row.try_get(2).map_err(read_error)?;
            let local: bool = row.try_get(5).map_err(read_error)?;
            // Effective requiredness replaces physical NOT NULL, including PG18 catalog rows.
            if local && kind == "n" && count == 1 {
                continue;
            }
            let mut known_fk = false;
            if let FieldType::Many2one { comodel, ondelete } = &field.ty {
                let registry = self.registry();
                let target = registry.model(comodel).ok_or_else(|| {
                    Error::programming("company migration relation target is missing")
                })?;
                let target_name: String = row.try_get(3).map_err(read_error)?;
                let delete: String = row.try_get(4).map_err(read_error)?;
                let expected = match ondelete {
                    erp_fields::OnDelete::SetNull => "n",
                    erp_fields::OnDelete::Restrict => "r",
                    erp_fields::OnDelete::Cascade => "c",
                };
                known_fk = local
                    && name == ddl::foreign_key_name(&spec.table, field.name)
                    && kind == "f"
                    && count == 1
                    && row.try_get::<_, bool>(6).map_err(read_error)?
                    && target_name == target.table
                    && delete == expected;
            }
            if !known_fk {
                return Err(Error::programming(format!(
                    "{}.{}: company migration refuses dependent constraint '{name}'; migrate it explicitly",
                    spec.name, field.name
                )));
            }
            drops.push(format!(
                "ALTER TABLE {table} DROP CONSTRAINT {}",
                quote(&name)
            ));
        }
        let rows = self.query("SELECT c.relname, NOT i.indisunique AND NOT i.indisprimary AND i.indnkeyatts=1 AND i.indnatts=1 AND i.indexprs IS NULL AND am.amname='btree' AND (i.indpred IS NULL OR pg_get_expr(i.indpred,i.indrelid)=format('(%I IS NOT NULL)',a.attname)) AND NOT EXISTS(SELECT FROM unnest(i.indclass) x JOIN pg_opclass o ON o.oid=x WHERE NOT o.opcdefault) AS ordinary FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_am am ON am.oid=c.relam JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=$2::int4 WHERE i.indrelid=to_regclass($1) AND ($2::int4=ANY(i.indkey) OR EXISTS(SELECT FROM pg_depend d WHERE d.classid='pg_class'::regclass AND d.objid=i.indexrelid AND d.refobjid=i.indrelid AND d.refobjsubid=$2::int4)) LIMIT 1025", &[&table,&attnum]).await?;
        if rows.len() > 1024 {
            return Err(Error::validation(
                "company migration dependency limit exceeded",
            ));
        }
        for row in rows {
            let name: String = row.try_get(0).map_err(read_error)?;
            let ordinary: bool = row.try_get(1).map_err(read_error)?;
            if name != erp_registry::ident::index_name(&spec.table, field.name) || !ordinary {
                return Err(Error::programming(format!(
                    "{}.{}: company migration refuses dependent index '{name}'; migrate it explicitly",
                    spec.name, field.name
                )));
            }
            // Keep the ordinary index for PostgreSQL to rebuild under its declared name.
        }
        let views = self.query("SELECT DISTINCT v.relname FROM pg_depend d JOIN pg_rewrite r ON r.oid=d.objid AND d.classid='pg_rewrite'::regclass JOIN pg_class v ON v.oid=r.ev_class WHERE d.refobjid=to_regclass($1) AND d.refobjsubid=$2::int4 LIMIT 1", &[&table,&attnum]).await?;
        if let Some(view) = views.first() {
            let name: String = view.try_get(0).map_err(read_error)?;
            return Err(Error::programming(format!(
                "{}.{}: company migration refuses dependent view '{name}'; migrate it explicitly",
                spec.name, field.name
            )));
        }
        Ok(drops)
    }
}
