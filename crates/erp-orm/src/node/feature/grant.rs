//! Operator publication of the narrow maintenance-owned feature helpers.
use crate::Transaction;
use crate::grantee::Grant;
use erp_fields::{Error, ErrorKind, Result};

const FEATURE_GRANT: Grant = Grant {
    call: "Transaction::grant_node_feature_roles",
    tables: &["erp_node", "erp_node_feature", "erp_node_feature_digest"],
    grantee: "the node feature helper role",
    missing: |table| {
        format!(
            "the node feature table {table} is missing: run schema sync before publishing feature roles"
        )
    },
    owner: |role, table| {
        format!("role {role:?} owns {table}: node feature helper roles must not own the tables")
    },
};
pub(super) const FUNCTIONS: &[&str] = &[
    "public.erp_node_feature_snapshot(text)",
    "public.erp_node_feature_protect(text,integer,uuid,bytea,bigint,boolean)",
    "public.erp_node_feature_guard()",
    "public.erp_node_feature_member(text,integer,uuid,bytea)",
];

/// Body plus execution attributes are part of the helper identity; no name-only publication.
pub(super) const HELPER_IDENTITY_SQL: &str = "SELECT p.prosrc, \
    l.lanname='plpgsql' AND p.prokind='f' AND NOT p.proisstrict AND NOT p.proleakproof \
    AND p.proparallel='u' AND p.provolatile::text=$2 \
    AND p.proconfig IS NOT NULL AND p.proconfig=ARRAY['search_path=pg_catalog, public']::text[] \
    AND pg_catalog.pg_get_function_result(p.oid)=$3 \
    FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_language l ON l.oid=p.prolang \
    WHERE p.oid=pg_catalog.to_regprocedure($1)";

/// A row trigger must cover both INSERT and every UPDATE, without a bypassing WHEN/column list.
pub(super) const TRIGGER_IDENTITY_SQL: &str = "SELECT tgfoid='public.erp_node_feature_guard()'::regprocedure \
    AND tgtype=23 AND tgqual IS NULL AND tgnargs=0 AND tgattr=''::int2vector \
    AND NOT tgdeferrable AND NOT tginitdeferred AND (NOT $1::boolean OR tgenabled='A') \
    FROM pg_catalog.pg_trigger WHERE tgrelid='public.erp_node'::regclass \
    AND tgname='erp_node_feature_guard' AND NOT tgisinternal";

pub(super) const OWNER_IDENTITY_SQL: &str = "SELECT p.proowner::int8, p.prosecdef \
    AND NOT r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolcreaterole AND NOT r.rolcreatedb \
    AND NOT r.rolreplication AND NOT r.rolbypassrls \
    AND NOT has_schema_privilege(r.oid,'public','CREATE') \
    AND has_table_privilege(r.oid,'public.erp_node','SELECT') \
    AND has_table_privilege(r.oid,'public.erp_node_feature','SELECT') \
    AND has_table_privilege(r.oid,'public.erp_node_feature_digest','SELECT') \
    AND has_column_privilege(r.oid,'public.erp_node_feature','key','UPDATE') \
    AND NOT EXISTS(SELECT FROM aclexplode(coalesce(p.proacl,acldefault('f',p.proowner))) a WHERE a.grantee=0) \
    AND (NOT $2::boolean OR has_function_privilege(current_user,p.oid,'EXECUTE')) \
    AND (current_user::text=(SELECT pg_get_userbyid(relowner)::text FROM pg_class WHERE oid='public.erp_node'::regclass) \
         OR NOT pg_has_role(current_user::text,r.oid,'MEMBER')) \
    AND NOT EXISTS(SELECT FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
      WHERE n.nspname='public' AND c.relkind IN ('r','p','v','m') \
       AND c.relname NOT IN ('erp_node','erp_node_feature','erp_node_feature_digest') \
       AND (has_table_privilege(r.oid,c.oid,'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') \
         OR has_any_column_privilege(r.oid,c.oid,'SELECT,INSERT,UPDATE,REFERENCES'))) \
    FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_roles r ON r.oid=p.proowner \
    WHERE p.oid=pg_catalog.to_regprocedure($1)";

/// Read-only startup validation runs on the request transaction; no role mutation is permitted.
pub(super) async fn validate_helpers(tx: &mut Transaction) -> Result<()> {
    let mut owner = None;
    for signature in FUNCTIONS {
        let (volatility, result) = expected_shape(signature)?;
        let rows = tx
            .query(HELPER_IDENTITY_SQL, &[signature, &volatility, &result])
            .await?;
        let row = rows.first().ok_or_else(|| {
            Error::programming(format!(
                "node feature helper {signature} is missing; run schema sync"
            ))
        })?;
        let actual: String = row.try_get(0).map_err(decoded)?;
        if actual != expected_source(signature)? || !row.try_get::<_, bool>(1).map_err(decoded)? {
            return Err(Error::programming(format!(
                "node feature helper {signature} differs from this binary"
            )));
        }
        let callable = *signature != "public.erp_node_feature_guard()";
        let rows = tx
            .query(OWNER_IDENTITY_SQL, &[signature, &callable])
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::programming("node feature helper owner is missing"))?;
        if !row.try_get::<_, bool>(1).map_err(decoded)? {
            return Err(Error::programming(format!(
                "node feature helper {signature} is not safely published"
            )));
        }
        let current: i64 = row.try_get(0).map_err(decoded)?;
        match owner {
            None => owner = Some(current),
            Some(previous) if previous == current => {}
            Some(_) => {
                return Err(Error::programming(
                    "node feature helpers must share one isolated maintenance owner",
                ));
            }
        }
    }
    let rows = tx.query(TRIGGER_IDENTITY_SQL, &[&true]).await?;
    match rows.as_slice() {
        [row] if row.try_get::<_, bool>(0).map_err(decoded)? => Ok(()),
        _ => Err(Error::programming(
            "node feature membership trigger differs from this binary",
        )),
    }
}

pub(super) fn expected_shape(signature: &str) -> Result<(&'static str, &'static str)> {
    match signature {
        "public.erp_node_feature_snapshot(text)" => Ok((
            "s",
            "TABLE(required_version integer, enabled boolean, generation bigint)",
        )),
        "public.erp_node_feature_member(text,integer,uuid,bytea)" => Ok((
            "v",
            "TABLE(required_version integer, enabled boolean, generation bigint, node_serving boolean)",
        )),
        "public.erp_node_feature_protect(text,integer,uuid,bytea,bigint,boolean)" => Ok((
            "v",
            "TABLE(required_version integer, enabled boolean, generation bigint)",
        )),
        "public.erp_node_feature_guard()" => Ok(("v", "trigger")),
        _ => Err(Error::programming(
            "unknown built-in feature helper signature",
        )),
    }
}

fn decoded(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::new(
        ErrorKind::Database,
        "could not decode node feature role publication",
    )
    .with_source(error)
}

impl Transaction {
    /// Publish feature snapshot/protection helpers under an isolated NOLOGIN maintenance owner.
    ///
    /// The request role receives only EXECUTE; it never receives node/feature table access.
    /// The maintenance role receives narrowly scoped relation rights used by fixed helper bodies.
    /// Run as System/full with an operator database role allowed to transfer function ownership.
    ///
    /// # Errors
    ///
    /// `Access` without System/full; `Programming` for invalid/unsafe roles, a LOGIN function
    /// owner, role membership crossing this boundary, missing or changed helper bodies, or
    /// inherited request relation privileges; `Database` with its source for failed DDL/grants.
    /// Role checks are current when published; re-publish after any role-policy changes.
    pub async fn grant_node_feature_roles(
        &mut self,
        maintenance: &str,
        request: &str,
    ) -> Result<()> {
        if maintenance.len() > 63 || request.len() > 63 {
            return Err(Error::programming(
                "node feature role names must be at most 63 bytes",
            ));
        }
        let maintenance_sql = self.check_grantee(&FEATURE_GRANT, maintenance).await?;
        let request_sql = self.check_grantee(&FEATURE_GRANT, request).await?;
        let roles = self
            .query(
                "SELECT rolcanlogin, \
            pg_catalog.pg_has_role($2::text, $1::text, 'MEMBER'), \
            pg_catalog.pg_has_role($1::text, $2::text, 'MEMBER') \
            FROM pg_catalog.pg_roles WHERE rolname = $1",
                &[&maintenance, &request],
            )
            .await?;
        let role = roles
            .first()
            .ok_or_else(|| Error::programming("node feature maintenance role is missing"))?;
        if role.try_get::<_, bool>(0).map_err(decoded)? {
            return Err(Error::programming(
                "node feature function owner must be a NOLOGIN role",
            ));
        }
        if maintenance == request
            || role.try_get::<_, bool>(1).map_err(decoded)?
            || role.try_get::<_, bool>(2).map_err(decoded)?
        {
            return Err(Error::programming(
                "node feature maintenance and request roles must not reach each other",
            ));
        }
        let business = self.query("SELECT EXISTS(SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p','v','m') AND c.relname NOT IN ('erp_node','erp_node_feature','erp_node_feature_digest') AND (pg_catalog.has_table_privilege($1::text,c.oid,'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') OR pg_catalog.has_any_column_privilege($1::text,c.oid,'SELECT,INSERT,UPDATE,REFERENCES')))", &[&maintenance]).await?;
        if business
            .first()
            .ok_or_else(|| {
                Error::programming("node feature maintenance privilege state is missing")
            })?
            .try_get::<_, bool>(0)
            .map_err(decoded)?
        {
            return Err(Error::programming(
                "node feature maintenance role must not access business tables",
            ));
        }
        let heartbeat=self.query("SELECT EXISTS(SELECT FROM pg_catalog.pg_roles r WHERE r.oid<>(SELECT relowner FROM pg_class WHERE oid='public.erp_node'::regclass) AND r.rolname<>$1 AND NOT r.rolsuper AND NOT r.rolcreaterole AND (has_table_privilege(r.oid,'public.erp_node','INSERT,UPDATE') OR has_any_column_privilege(r.oid,'public.erp_node','INSERT,UPDATE')) AND pg_has_role(r.oid,(SELECT oid FROM pg_roles WHERE rolname=$1),'MEMBER'))", &[&maintenance]).await?;
        if heartbeat
            .first()
            .ok_or_else(|| Error::programming("node feature heartbeat privilege state is missing"))?
            .try_get::<_, bool>(0)
            .map_err(decoded)?
        {
            return Err(Error::programming(
                "heartbeat roles must not reach node feature helper ownership",
            ));
        }
        for signature in FUNCTIONS {
            let (volatility, result) = expected_shape(signature)?;
            let rows = self
                .query(HELPER_IDENTITY_SQL, &[signature, &volatility, &result])
                .await?;
            let row = rows.first().ok_or_else(|| {
                Error::programming(format!(
                    "node feature helper {signature} is missing; run schema sync"
                ))
            })?;
            let actual: String = row.try_get(0).map_err(decoded)?;
            if actual != expected_source(signature)?
                || !row.try_get::<_, bool>(1).map_err(decoded)?
            {
                return Err(Error::programming(format!(
                    "node feature helper {signature} differs from this binary"
                )));
            }
        }
        self.batch(&format!("GRANT USAGE, CREATE ON SCHEMA public TO {maintenance_sql}; \
            REVOKE ALL ON public.erp_node_feature, public.erp_node_feature_digest FROM {request_sql}; \
            REVOKE ALL ON public.erp_node_feature, public.erp_node_feature_digest FROM {maintenance_sql}; \
            GRANT SELECT ON public.erp_node, public.erp_node_feature, public.erp_node_feature_digest TO {maintenance_sql}; \
            GRANT UPDATE (key) ON public.erp_node_feature TO {maintenance_sql}")).await?;
        for signature in FUNCTIONS {
            // Signatures are fixed binary constants; only roles pass through identifier quoting.
            self.batch(&format!(
                "ALTER FUNCTION {signature} OWNER TO {maintenance_sql}; \
                ALTER FUNCTION {signature} SECURITY DEFINER; \
                REVOKE ALL ON FUNCTION {signature} FROM PUBLIC"
            ))
            .await?;
            if *signature != "public.erp_node_feature_guard()" {
                self.batch(&format!(
                    "GRANT EXECUTE ON FUNCTION {signature} TO {request_sql}"
                ))
                .await?;
            }
        }
        self.batch(&format!(
            "REVOKE CREATE ON SCHEMA public FROM {maintenance_sql}"
        ))
        .await?;
        let excess=self.query("SELECT has_schema_privilege($1::text,'public','CREATE') OR has_table_privilege($1::text,'public.erp_node','INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') OR has_any_column_privilege($1::text,'public.erp_node','INSERT,UPDATE,REFERENCES') OR has_table_privilege($1::text,'public.erp_node_feature','INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') OR EXISTS(SELECT FROM pg_attribute WHERE attrelid='public.erp_node_feature'::regclass AND attnum>0 AND NOT attisdropped AND attname<>'key' AND has_column_privilege($1::text,attrelid,attnum,'INSERT,UPDATE,REFERENCES')) OR has_table_privilege($1::text,'public.erp_node_feature_digest','INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') OR has_any_column_privilege($1::text,'public.erp_node_feature_digest','INSERT,UPDATE,REFERENCES')", &[&maintenance]).await?;
        if excess
            .first()
            .ok_or_else(|| {
                Error::programming("node feature maintenance privilege state is missing")
            })?
            .try_get::<_, bool>(0)
            .map_err(decoded)?
        {
            return Err(Error::programming(
                "node feature maintenance role has excess inherited privileges",
            ));
        }
        let trigger = self.query(TRIGGER_IDENTITY_SQL, &[&false]).await?;
        match trigger.first() {
            None => {
                self.batch(
                    "CREATE TRIGGER erp_node_feature_guard BEFORE INSERT OR UPDATE \
                ON public.erp_node FOR EACH ROW EXECUTE FUNCTION public.erp_node_feature_guard()",
                )
                .await?
            }
            Some(row) => {
                if !row.try_get::<_, bool>(0).map_err(decoded)? {
                    return Err(Error::programming(
                        "node feature membership trigger differs from this binary",
                    ));
                }
            }
        }
        self.batch("ALTER TABLE public.erp_node ENABLE ALWAYS TRIGGER erp_node_feature_guard")
            .await?;
        let inherited = self.query("SELECT EXISTS (SELECT FROM pg_catalog.pg_class c \
            WHERE c.oid IN ('public.erp_node'::regclass, 'public.erp_node_feature'::regclass, 'public.erp_node_feature_digest'::regclass) \
              AND (pg_catalog.has_table_privilege($1::text,c.oid,'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') \
                OR pg_catalog.has_any_column_privilege($1::text,c.oid,'SELECT,INSERT,UPDATE,REFERENCES')))", &[&request]).await?;
        let row = inherited
            .first()
            .ok_or_else(|| Error::programming("node feature request privilege state is missing"))?;
        if row.try_get::<_, bool>(0).map_err(decoded)? {
            return Err(Error::programming(
                "request role must not access node feature or membership tables through inherited grants",
            ));
        }
        Ok(())
    }
}

pub(super) fn expected_source(signature: &str) -> Result<&'static str> {
    let name = signature
        .split('(')
        .next()
        .ok_or_else(|| Error::programming("invalid built-in feature helper signature"))?;
    let header = format!("CREATE FUNCTION {name}(");
    let definition = include_str!("schema.sql")
        .split(&header)
        .nth(1)
        .ok_or_else(|| Error::programming("built-in feature helper definition is missing"))?;
    definition
        .split("$erp_fn$")
        .nth(1)
        .ok_or_else(|| Error::programming("built-in feature helper source delimiter is missing"))
}
