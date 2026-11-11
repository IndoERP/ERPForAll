//! UoM runtime publication and admission.

use erp_fields::{Error, Result};
use erp_orm::Transaction;

const STATE_SQL: &str = "SELECT EXISTS(SELECT 1 FROM public.ir_module_module WHERE name='uom' AND state='installed'), (SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relname IN ('uom_reference_fence','uom_reference_use','uom_reference_fence_id_seq','uom_reference_use_id_seq')), (SELECT count(*) FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public' AND p.proname IN ('erp_uom_fence','erp_uom_resolve','erp_uom_descendants','erp_uom_register','erp_uom_before_mutation','erp_uom_after_mutation','erp_uom_private_state'))";
const UNSAFE_SQL: &str = include_str!("uom_grants.sql");
const INCOMPLETE: &str =
    "Unit reference guard installation is incomplete; operator recovery is required.";
const UNSAFE: &str = "Unit reference runtime privileges are unsafe or incomplete; publish grants and reconcile inherited access.";

fn installed(row: &tokio_postgres::Row) -> Result<bool> {
    let enabled: bool = row
        .try_get(0)
        .map_err(|e| Error::db("Cannot decode UoM installation state.").with_source(e))?;
    let relations: i64 = row
        .try_get(1)
        .map_err(|e| Error::db("Cannot decode UoM relation inventory.").with_source(e))?;
    let functions: i64 = row
        .try_get(2)
        .map_err(|e| Error::db("Cannot decode UoM function inventory.").with_source(e))?;
    match (enabled, relations, functions) {
        (false, 0, 0) => Ok(false),
        (true, 4, 7) => Ok(true),
        _ => Err(Error::programming(INCOMPLETE)),
    }
}

pub(crate) async fn publish(tx: &mut Transaction, role: &str) -> Result<()> {
    let rows = tx.query(STATE_SQL, &[]).await?;
    let [row] = rows.as_slice() else {
        return Err(Error::programming(INCOMPLETE));
    };
    if !installed(row)? {
        return Ok(());
    }
    let grantee = erp_sql::quote(role);
    // A table REVOKE does not remove column grants. Revoke those explicitly too.
    let columns=tx.query("SELECT c.relname::text,a.attname::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid WHERE n.nspname='public' AND c.relname IN ('uom_reference_fence','uom_reference_use') AND a.attnum>0 AND NOT a.attisdropped ORDER BY c.relname,a.attnum LIMIT 257",&[]).await?;
    if columns.len() > 256 {
        return Err(Error::programming(
            "Unit guard column inventory exceeds its lifecycle limit.",
        ));
    }
    let mut sql = format!(
        "REVOKE ALL ON public.uom_reference_fence,public.uom_reference_use FROM {grantee}; REVOKE ALL ON SEQUENCE public.uom_reference_fence_id_seq,public.uom_reference_use_id_seq FROM {grantee};"
    );
    for row in columns {
        let table: String = row
            .try_get(0)
            .map_err(|e| Error::db("Cannot decode unit guard table.").with_source(e))?;
        let column: String = row
            .try_get(1)
            .map_err(|e| Error::db("Cannot decode unit guard column.").with_source(e))?;
        let table = erp_sql::quote(&table);
        let column = erp_sql::quote(&column);
        sql.push_str(&format!("REVOKE SELECT({column}),INSERT({column}),UPDATE({column}),REFERENCES({column}) ON public.{table} FROM {grantee};"));
    }
    sql.push_str(&format!("GRANT SELECT ON public.uom_reference_fence,public.uom_reference_use TO {grantee}; GRANT UPDATE(singleton) ON public.uom_reference_fence TO {grantee};"));
    for signature in [
        "erp_uom_fence()",
        "erp_uom_resolve(bigint[])",
        "erp_uom_descendants(bigint[])",
        "erp_uom_register(bigint[])",
        "erp_uom_before_mutation()",
        "erp_uom_after_mutation()",
        "erp_uom_private_state()",
    ] {
        sql.push_str(&format!("REVOKE ALL ON FUNCTION public.{signature} FROM {grantee}; REVOKE ALL ON FUNCTION public.{signature} FROM PUBLIC;"));
    }
    sql.push_str(&format!(
        "GRANT EXECUTE ON FUNCTION public.erp_uom_register(bigint[]) TO {grantee};"
    ));
    tx.batch(&sql).await?;
    let rows = tx.query(UNSAFE_SQL, &[&role]).await?;
    let [row] = rows.as_slice() else {
        return Err(Error::programming(UNSAFE));
    };
    let unsafe_grants: bool = row
        .try_get(0)
        .map_err(|e| Error::db("Cannot decode UoM privilege state.").with_source(e))?;
    if unsafe_grants {
        return Err(Error::programming(UNSAFE));
    }
    Ok(())
}

pub(crate) async fn verify(client: &deadpool_postgres::Client) -> Result<()> {
    let row = client
        .query_one(STATE_SQL, &[])
        .await
        .map_err(|e| Error::db("Cannot inspect UoM installation state.").with_source(e))?;
    if !installed(&row)? {
        return Ok(());
    }
    let role: String = client
        .query_one("SELECT current_user::text", &[])
        .await
        .map_err(|e| Error::db("Cannot inspect UoM request role.").with_source(e))?
        .try_get(0)
        .map_err(|e| Error::db("Cannot decode UoM request role.").with_source(e))?;
    let unsafe_grants: bool = client
        .query_one(UNSAFE_SQL, &[&role])
        .await
        .map_err(|e| Error::db("Cannot inspect UoM runtime privileges.").with_source(e))?
        .try_get(0)
        .map_err(|e| Error::db("Cannot decode UoM runtime privileges.").with_source(e))?;
    if unsafe_grants {
        return Err(Error::programming(UNSAFE));
    }
    Ok(())
}

/// Missing minimum rights during the install/publication gap are retryable.
/// Extra rights remain an unsafe-state error from `verify`, never a successful admission.
pub(crate) async fn pending(client: &deadpool_postgres::Client) -> Result<bool> {
    let row = client
        .query_one(STATE_SQL, &[])
        .await
        .map_err(|e| Error::db("Cannot inspect UoM installation state.").with_source(e))?;
    if !installed(&row)? {
        return Ok(false);
    }
    client.query_one("SELECT NOT pg_catalog.has_table_privilege('public.uom_reference_fence','SELECT') OR NOT pg_catalog.has_table_privilege('public.uom_reference_use','SELECT') OR NOT pg_catalog.has_column_privilege('public.uom_reference_fence','singleton','UPDATE') OR NOT pg_catalog.has_function_privilege('public.erp_uom_register(bigint[])','EXECUTE')", &[]).await
        .map_err(|e| Error::db("Cannot inspect pending UoM runtime grants.").with_source(e))?
        .try_get(0).map_err(|e| Error::db("Cannot decode pending UoM runtime grants.").with_source(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_testkit::Harness;

    #[tokio::test]
    async fn publication_removes_blanket_writes_but_keeps_monotone_registration() {
        let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
            .build()
            .await
            .unwrap();
        let role = format!("{}_uom", h.db().name());
        let owner = h.db().client().await.unwrap();
        owner.batch_execute(&format!("CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",erp_sql::quote(&role))).await.unwrap();
        let mut tx = h.begin().await.unwrap();
        tx.batch(include_str!("../../../addons/uom/src/guard_sql.sql"))
            .await
            .unwrap();
        tx.batch("INSERT INTO uom_reference_fence(singleton) VALUES(1); CREATE TABLE ir_module_module(name text PRIMARY KEY,state text,latest_version text); INSERT INTO ir_module_module VALUES('uom','installed','1.1.0')").await.unwrap();
        let root: i64 = tx.query("INSERT INTO uom_uom(name,relative_factor,factor,active) VALUES(jsonb_build_object('en_US','Runtime root'),1,1,true) RETURNING id::bigint", &[]).await.unwrap()[0].get(0);
        tx.batch(&format!("GRANT USAGE ON SCHEMA public TO {0}; GRANT SELECT,INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA public TO {0}; GRANT USAGE,SELECT ON ALL SEQUENCES IN SCHEMA public TO {0}",erp_sql::quote(&role))).await.unwrap();
        publish(&mut tx, &role).await.unwrap();
        tx.commit().await.unwrap();
        let mut config = h.db().config().clone();
        config.user(&role).password("fixture-runtime-password");
        let pool = erp_orm::pool::create_pool(config, 2).unwrap();
        let request = pool.get().await.unwrap();
        verify(&request).await.unwrap();
        assert!(!pending(&request).await.unwrap());
        owner
            .batch_execute(&format!(
                "REVOKE SELECT ON uom_reference_use FROM {}",
                erp_sql::quote(&role)
            ))
            .await
            .unwrap();
        assert!(
            pending(&request).await.unwrap(),
            "install/publication gap must use the transient admission gate"
        );
        owner
            .batch_execute(&format!(
                "GRANT SELECT ON uom_reference_use TO {}",
                erp_sql::quote(&role)
            ))
            .await
            .unwrap();
        assert!(!pending(&request).await.unwrap());
        for statement in [
            "DELETE FROM uom_reference_fence",
            "TRUNCATE uom_reference_use",
            "UPDATE uom_reference_fence SET id=id",
            "INSERT INTO uom_reference_use(uom_id,root_id,factor) VALUES(1,1,1)",
            "SELECT nextval('uom_reference_use_id_seq')",
            "SELECT public.erp_uom_fence()",
            "SELECT * FROM public.erp_uom_resolve(ARRAY[]::bigint[])",
        ] {
            let error = request.batch_execute(statement).await.unwrap_err();
            assert_eq!(error.code().unwrap().code(), "42501");
            assert_eq!(
                error.as_db_error().unwrap().message(),
                if statement.contains("erp_uom_fence()") {
                    "permission denied for function erp_uom_fence"
                } else if statement.contains("erp_uom_resolve") {
                    "permission denied for function erp_uom_resolve"
                } else if statement.contains("nextval") {
                    "permission denied for sequence uom_reference_use_id_seq"
                } else if statement.contains("fence") {
                    "permission denied for table uom_reference_fence"
                } else {
                    "permission denied for table uom_reference_use"
                }
            );
        }
        let rows = request
            .query(
                "SELECT * FROM public.erp_uom_register(ARRAY[]::bigint[])",
                &[],
            )
            .await
            .unwrap();
        assert!(rows.is_empty());
        let registered = request
            .query_one(
                "SELECT uom_id,root_id,factor::text FROM public.erp_uom_register($1::bigint[])",
                &[&vec![root]],
            )
            .await
            .unwrap();
        assert_eq!(registered.get::<_, i64>(0), root);
        assert_eq!(registered.get::<_, i64>(1), root);
        assert_eq!(registered.get::<_, String>(2), "1");
        for grant in [
            "GRANT UPDATE(factor) ON uom_reference_use TO PUBLIC".to_owned(),
            format!(
                "CREATE ROLE {0} NOLOGIN NOINHERIT; GRANT UPDATE(factor) ON uom_reference_use TO {0}; GRANT {0} TO {1} WITH INHERIT FALSE, SET TRUE",
                erp_sql::quote(&format!("{role}_leak")),
                erp_sql::quote(&role)
            ),
        ] {
            owner.batch_execute(&grant).await.unwrap();
            assert!(
                !pending(&request).await.unwrap(),
                "leaked writes are unsafe, not pending publication"
            );
            let error = verify(&request).await.unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(error.message(), UNSAFE);
            if grant.contains("PUBLIC") {
                owner
                    .batch_execute("REVOKE UPDATE(factor) ON uom_reference_use FROM PUBLIC")
                    .await
                    .unwrap();
            } else {
                owner
                    .batch_execute(&format!(
                        "REVOKE {0} FROM {1}; DROP OWNED BY {0}; DROP ROLE {0}",
                        erp_sql::quote(&format!("{role}_leak")),
                        erp_sql::quote(&role)
                    ))
                    .await
                    .unwrap();
            }
            verify(&request).await.unwrap();
        }
        owner
            .batch_execute(&format!(
                "GRANT UPDATE(factor) ON uom_reference_use TO {}",
                erp_sql::quote(&role)
            ))
            .await
            .unwrap();
        let error = verify(&request).await.unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "Unit reference runtime privileges are unsafe or incomplete; publish grants and reconcile inherited access."
        );
        drop(request);
        pool.close();
        owner
            .batch_execute(&format!(
                "DROP OWNED BY {0}; DROP ROLE {0}",
                erp_sql::quote(&role)
            ))
            .await
            .unwrap();
        drop(owner);
        h.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn installed_addon_with_missing_guard_objects_is_refused() {
        let h = Harness::builder([&erp_addon_base::ADDON])
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        tx.batch("CREATE TABLE ir_module_module(name text PRIMARY KEY,state text); INSERT INTO ir_module_module VALUES('uom','installed')").await.unwrap();
        let error = publish(&mut tx, "unused_fixture_role").await.unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "Unit reference guard installation is incomplete; operator recovery is required."
        );
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}
