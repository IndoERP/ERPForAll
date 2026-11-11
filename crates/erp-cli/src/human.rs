//! Native composition of the real module, ORM, authentication and HTTP services.
mod addon_detail;
use crate::node::Node;
use crate::{
    CliError, Result,
    access::pool_with_size,
    args::{GrantVacuumRoleArgs, InitArgs, ServerArgs, VacuumTransientArgs},
};
use erp_fields::{Cell, RecId};
use erp_modules::{InitOptions, ModuleName, ModuleSystem};
use erp_orm::{Command, Orm, RawVals, SUPERUSER_ID, Signal, WasmMethodImpl};
use erp_security::auth::{Auth, AuthConfig, HashPool};
use erp_security::{resolver::Resolver, source::PolicySource};
use erp_server::{config::ServerConfig, web::WebApp};
use std::{path::Path, sync::Arc};

fn failed(message: impl Into<String>) -> CliError {
    CliError::Execution(message.into())
}
fn config(path: &Path) -> Result<Arc<ServerConfig>> {
    ServerConfig::load(path)
        .map(Arc::new)
        .map_err(|e| failed(e.to_string()))
}
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| failed("could not start the server runtime"))
}
fn modules(config: &ServerConfig, baseline: Option<erp_orm::Generations>) -> Result<Node> {
    Node::open(
        pool_with_size(config.owner_connection(), 4)?,
        config.addons(),
        baseline,
    )
}

pub(crate) fn initialize(args: &InitArgs) -> Result<String> {
    runtime()?.block_on(initialize_async(args))
}
pub(crate) async fn initialize_async(args: &InitArgs) -> Result<String> {
    let config = config(&args.config)?;
    let password = erp_server::config::read_secret(&args.admin_password_file)
        .map_err(|e| failed(e.to_string()))?;
    if args.admin_login.trim().is_empty()
        || args.admin_login.len() > 256
        || args.admin_login.chars().any(char::is_control)
        || password.len() < 12
        || password.chars().any(char::is_control)
    {
        return Err(failed(
            "initial administrator requires a nonempty login (at most 256 bytes) and a password of at least 12 bytes without control characters",
        ));
    }
    let node = modules(&config, None)?;
    let system = node.system();
    let owner_pool = pool_with_size(config.owner_connection(), 1)?;
    let owner = owner_pool
        .get()
        .await
        .map_err(|_| failed("could not inspect bootstrap state"))?;
    let exists: bool = owner
        .query_one(
            "SELECT to_regclass('public.ir_module_module') IS NOT NULL",
            &[],
        )
        .await
        .map_err(|_| failed("could not inspect bootstrap state"))?
        .try_get(0)
        .map_err(|_| failed("invalid bootstrap state"))?;
    drop(owner);
    if !exists {
        let initialized = system
            .initialize(InitOptions::new())
            .await
            .map_err(|e| CliError::ModuleOperation(Box::new(e)))?;
        if !initialized.post_commit.is_clean() {
            return Err(failed(
                "Database initialized but module post-commit work is incomplete; operator recovery is required",
            ));
        }
    }
    let loaded = node
        .load()
        .await
        .map_err(|e| CliError::ModuleOperation(Box::new(e)))?;
    if loaded.orm.signal().is_some() {
        return Err(failed(
            "Administrator activation cannot use an ORM that reads opening signals before its users-table lock",
        ));
    }
    // Take admission before opening the activation snapshot. A blocking lock SELECT in the
    // activation's REPEATABLE READ transaction would pin a pre-wait snapshot and could still
    // see zero active users after a competing initializer commits. The raw-only guard uses
    // READ COMMITTED; all record hooks retain their normal RR activation transaction.
    let mut activation_guard = loaded
        .orm
        .begin_opts(
            erp_orm::BeginOptions::new(SUPERUSER_ID)
                .isolation(erp_orm::Isolation::ReadCommitted)
                .on_primary(),
        )
        .await?;
    activation_guard
        .query(
            "SELECT pg_advisory_xact_lock($1,$2)",
            &[
                &erp_modules::OPERATION_LOCK.0,
                &erp_modules::OPERATION_LOCK.1,
            ],
        )
        .await?;
    let mut tx = loaded.orm.begin(SUPERUSER_ID).await?;
    // This must be the first operation on this connection: System resolution sends no query,
    // and this bootstrap ORM has no Signal. LOCK is a utility statement, so a waiter takes its
    // RR snapshot only at the count below. The lock stays with the effect transaction even if
    // the separate lifecycle admission connection is lost; user writes remain serialized.
    tx.batch("LOCK TABLE res_users IN SHARE ROW EXCLUSIVE MODE")
        .await?;
    let active = tx
        .query("SELECT count(*) FROM res_users WHERE active", &[])
        .await?;
    let active: i64 = active
        .first()
        .ok_or_else(|| failed("bootstrap user state missing"))?
        .try_get(0)
        .map_err(|_| failed("invalid bootstrap user state"))?;
    if active != 0 {
        return Err(failed(
            "Database already has active users; initial administrator creation refuses to replace existing credentials",
        ));
    }
    let rows = tx.query("SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company' AND model='res.company'", &[]).await?;
    let company: i64 = rows
        .first()
        .ok_or_else(|| failed("base company missing"))?
        .try_get(0)
        .map_err(|_| failed("invalid base company identity"))?;
    let rows = tx.query("SELECT res_id FROM ir_model_data WHERE module='base' AND name='group_system' AND model='res.groups'", &[]).await?;
    let group: i64 = rows
        .first()
        .ok_or_else(|| failed("base system group missing"))?
        .try_get(0)
        .map_err(|_| failed("invalid base system group identity"))?;
    let partners = tx.env().records("res.partner")?;
    let partner = partners
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(args.admin_login.clone().into()))
                    .set_cell("company_id", Cell::Id(company)),
            ],
        )
        .await?;
    let users = tx.env().records("res.users")?;
    users
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(args.admin_login.clone().into()))
                    .set_cell("login", Cell::Str(args.admin_login.clone().into()))
                    .set_cell("password", Cell::Str(password.as_str().into()))
                    .set_cell("partner_id", Cell::Id(partner.rec_ids()[0].get()))
                    .set_cell("company_id", Cell::Id(company))
                    .set_cell("lang", Cell::Str("en_US".into()))
                    .set_cell("tz", Cell::Str("UTC".into()))
                    .set_commands(
                        "company_ids",
                        [Command::set([RecId::from_real(company).ok_or_else(
                            || failed("invalid base company identity"),
                        )?])],
                    )
                    .set_commands(
                        "group_ids",
                        [Command::set([RecId::from_real(group).ok_or_else(
                            || failed("invalid base system group identity"),
                        )?])],
                    ),
            ],
        )
        .await?;
    grant_runtime(&mut tx, &config).await?;
    tx.commit().await?;
    activation_guard.rollback().await?;
    Ok(format!(
        "Database {} initialized with the supplied administrator; no default credentials were created.",
        config.database()
    ))
}

// Evaluate on the runtime connection, shared with startup publication admission. The UI asset
// tables are SELECT-only: any table- or column-level write right counts as a defect.
const MISSING_RUNTIME_GRANTS_SQL: &str = "
SELECT NOT has_schema_privilege('public','USAGE') OR EXISTS (
    SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname='public' AND CASE
        WHEN c.relkind IN ('r','p') AND c.relname IN ('installed_addon_identity','installed_addon_identity_audit','erp_node_feature','erp_node_feature_digest')
        THEN has_table_privilege(c.oid,'SELECT') OR has_table_privilege(c.oid,'INSERT')
            OR has_table_privilege(c.oid,'UPDATE') OR has_table_privilege(c.oid,'DELETE')
            OR has_table_privilege(c.oid,'TRUNCATE') OR has_table_privilege(c.oid,'REFERENCES')
            OR has_table_privilege(c.oid,'TRIGGER')
        WHEN c.relkind='S' AND c.relname='installed_addon_identity_audit_id_seq'
        THEN has_sequence_privilege(c.oid,'USAGE') OR has_sequence_privilege(c.oid,'SELECT')
            OR has_sequence_privilege(c.oid,'UPDATE')
        WHEN c.relkind IN ('r','p') AND c.relname = 'erp_schema_generated'
        THEN NOT has_table_privilege(c.oid,'SELECT')
        WHEN c.relkind IN ('r','p') AND c.relname NOT IN
            ('erp_signal_generation','ir_module_post_commit','erp_module_install_audit','erp_module_lifecycle_audit',
             'ir_module_wasm_audit','ir_module_package','erp_transient_vacuum_state','erp_schema_generated','erp_translated_verified',
             'erp_node','erp_node_detail','erp_node_rollout','erp_node_feature','erp_node_feature_digest','installed_addon_identity','installed_addon_identity_audit',
             'ir_ui_asset_blob','ir_ui_asset_bundle','uom_reference_fence','uom_reference_use',
             'erp_job','erp_job_run','erp_job_item_failure','ir_blob_store')
        THEN CASE WHEN c.relname='erp_auth_gc_state' THEN NOT (has_table_privilege(c.oid,'SELECT') AND has_table_privilege(c.oid,'UPDATE'))
        ELSE NOT (has_table_privilege(c.oid,'SELECT') AND has_table_privilege(c.oid,'INSERT')
            AND has_table_privilege(c.oid,'UPDATE') AND has_table_privilege(c.oid,'DELETE')) END
        WHEN c.relkind IN ('r','p') AND c.relname IN ('ir_ui_asset_blob','ir_ui_asset_bundle')
        THEN NOT has_table_privilege(c.oid,'SELECT') OR has_table_privilege(c.oid,'INSERT') OR has_table_privilege(c.oid,'UPDATE') OR has_table_privilege(c.oid,'DELETE')
            OR has_table_privilege(c.oid,'TRUNCATE') OR has_table_privilege(c.oid,'REFERENCES')
            OR has_table_privilege(c.oid,'TRIGGER')
            OR has_any_column_privilege(c.oid,'INSERT') OR has_any_column_privilege(c.oid,'UPDATE')
            OR has_any_column_privilege(c.oid,'REFERENCES')
        WHEN c.relkind='S' AND c.relname NOT IN ('erp_module_install_audit_id_seq','erp_module_lifecycle_audit_id_seq',
             'ir_module_wasm_audit_id_seq','ir_module_package_id_seq','installed_addon_identity_audit_id_seq','uom_reference_fence_id_seq','uom_reference_use_id_seq')
        THEN NOT (has_sequence_privilege(c.oid,'USAGE') AND has_sequence_privilege(c.oid,'SELECT'))
        ELSE FALSE
    END
)
-- The attachment store registry: the runtime role reads and registers store rows, and
-- never rewrites or removes one (namespace and fingerprint name every content key of the store).
-- Not created yet (base before a later release): nothing to check.
OR /*ir_blob_store*/ CASE WHEN to_regclass('public.ir_blob_store') IS NULL THEN FALSE ELSE EXISTS (
    SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname='public' AND c.relkind='r' AND c.relname='ir_blob_store' AND (
        NOT (has_table_privilege(c.oid,'SELECT') AND has_table_privilege(c.oid,'INSERT'))
        OR has_table_privilege(c.oid,'UPDATE') OR has_table_privilege(c.oid,'DELETE')
        OR has_table_privilege(c.oid,'TRUNCATE') OR has_any_column_privilege(c.oid,'UPDATE'))
    ) END /*end ir_blob_store*/
-- The job queue tables: the runtime role holds SELECT, INSERT and DELETE on all
-- three, UPDATE only on the lifecycle columns of erp_job and on the error columns of
-- erp_job_item_failure, and nothing else (no table-level UPDATE, TRUNCATE, REFERENCES, TRIGGER or
-- MAINTAIN, no UPDATE on a history row). Effective rights count: PUBLIC and inherited roles too.
-- Not created yet (a legacy database before the schema sync): nothing to check.
OR /*erp_jobs*/ CASE WHEN to_regclass('public.erp_job') IS NULL THEN FALSE ELSE EXISTS (
    SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname='public' AND c.relkind='r'
      AND c.relname IN ('erp_job','erp_job_run','erp_job_item_failure') AND (
        NOT (has_table_privilege(c.oid,'SELECT') AND has_table_privilege(c.oid,'INSERT')
             AND has_table_privilege(c.oid,'DELETE'))
        OR has_table_privilege(c.oid,'UPDATE') OR has_table_privilege(c.oid,'TRUNCATE')
        OR has_table_privilege(c.oid,'REFERENCES') OR has_table_privilege(c.oid,'TRIGGER')
        OR has_table_privilege(c.oid,'MAINTAIN')
        OR EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped
            AND has_column_privilege(c.oid,a.attnum,'UPDATE')
            AND a.attname::text <> ALL (CASE c.relname
                WHEN 'erp_job' THEN ARRAY['state','failure_rank','priority','run_after','started_at','attempts',
                    'lease_owner','lease_epoch','lease_until','progress','items_done','items_failed','items_remaining',
                    'cancel_requested','last_error_code','last_error','last_error_at','dead_at']
                WHEN 'erp_job_item_failure' THEN ARRAY['error_code','error','failed_at']
                ELSE ARRAY[]::text[] END))
      )) END /*end erp_jobs*/
-- The event schema: the runtime role may use the schema, insert the columns
-- that have no database default of the outbox and the audit table, and use their id sequences; any other
-- right, on a table, a column, a partition or the schema, also through PUBLIC or an inherited role, is a
-- defect. Neither table installed: nothing to check.
-- The catalogs are read without a name lookup: to_regclass would itself fail without schema USAGE.
OR /*erp_event*/ CASE WHEN NOT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
        WHERE n.nspname='erp_event' AND c.relname IN ('ir_event_outbox','ir_audit_entry')) THEN FALSE ELSE
    NOT has_schema_privilege('erp_event','USAGE') OR has_schema_privilege('erp_event','CREATE')
    OR EXISTS (
        SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
        WHERE n.nspname='erp_event' AND CASE
            WHEN c.relkind='p' AND c.relname IN ('ir_event_outbox','ir_audit_entry')
            THEN has_table_privilege(c.oid,'SELECT') OR has_table_privilege(c.oid,'INSERT')
                OR has_table_privilege(c.oid,'UPDATE') OR has_table_privilege(c.oid,'DELETE')
                OR has_table_privilege(c.oid,'TRUNCATE') OR has_table_privilege(c.oid,'REFERENCES')
                OR has_table_privilege(c.oid,'TRIGGER') OR has_table_privilege(c.oid,'MAINTAIN')
                OR has_any_column_privilege(c.oid,'SELECT') OR has_any_column_privilege(c.oid,'UPDATE')
                OR has_any_column_privilege(c.oid,'REFERENCES')
                OR EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped
                    AND has_column_privilege(c.oid,a.attnum,'INSERT') = a.atthasdef)
            WHEN c.relkind='S' AND c.relname IN ('ir_event_outbox_id_seq','ir_audit_entry_id_seq')
            THEN NOT has_sequence_privilege(c.oid,'USAGE') OR has_sequence_privilege(c.oid,'SELECT')
                OR has_sequence_privilege(c.oid,'UPDATE')
            WHEN c.relkind IN ('r','p')
            THEN has_table_privilege(c.oid,'SELECT') OR has_table_privilege(c.oid,'INSERT')
                OR has_table_privilege(c.oid,'UPDATE') OR has_table_privilege(c.oid,'DELETE')
                OR has_table_privilege(c.oid,'TRUNCATE') OR has_table_privilege(c.oid,'REFERENCES')
                OR has_table_privilege(c.oid,'TRIGGER') OR has_table_privilege(c.oid,'MAINTAIN')
                OR has_any_column_privilege(c.oid,'SELECT') OR has_any_column_privilege(c.oid,'INSERT')
                OR has_any_column_privilege(c.oid,'UPDATE') OR has_any_column_privilege(c.oid,'REFERENCES')
            WHEN c.relkind='S'
            THEN has_sequence_privilege(c.oid,'USAGE') OR has_sequence_privilege(c.oid,'SELECT')
                OR has_sequence_privilege(c.oid,'UPDATE')
            ELSE FALSE
        END)
END /*end erp_event*/
";

// Effective privileges include PUBLIC and inherited roles, not just direct grants.
const TRANSLATION_MARK_LEAK_SQL: &str = "
SELECT CASE WHEN to_regclass('public.erp_translated_verified') IS NULL THEN false ELSE
    has_table_privilege('public.erp_translated_verified','SELECT') OR
    has_table_privilege('public.erp_translated_verified','INSERT') OR
    has_table_privilege('public.erp_translated_verified','UPDATE') OR
    has_table_privilege('public.erp_translated_verified','DELETE') END";

// Generated-column provenance is readable but owner-written. A direct
// revoke does not remove writes inherited through PUBLIC or another role. TRIGGER would let
// the runtime role attach code that runs on the owner's provenance writes, REFERENCES pins
// rows, and MAINTAIN (PostgreSQL 17+, requires 18) can lock the table.
const GENERATED_PROVENANCE_WRITE_LEAK_SQL: &str = "
SELECT CASE WHEN to_regclass('public.erp_schema_generated') IS NULL THEN NULL ELSE
    has_any_column_privilege('public.erp_schema_generated','INSERT') OR
    has_any_column_privilege('public.erp_schema_generated','UPDATE') OR
    has_any_column_privilege('public.erp_schema_generated','REFERENCES') OR
    has_table_privilege('public.erp_schema_generated','DELETE') OR
    has_table_privilege('public.erp_schema_generated','TRUNCATE') OR
    has_table_privilege('public.erp_schema_generated','TRIGGER') OR
    has_table_privilege('public.erp_schema_generated','MAINTAIN') END";

/// `erp audit maintain`: install or verify the audit table and refresh its partition window as the
/// owner role. No grants are changed.
pub(crate) fn audit_maintain(args: &ServerArgs) -> Result<String> {
    runtime()?.block_on(audit_maintain_async(args))
}

async fn audit_maintain_async(args: &ServerArgs) -> Result<String> {
    let config = config(&args.config)?;
    let orm = Orm::builder(
        crate::access::registry()?,
        pool_with_size(config.owner_connection(), 1)?,
    )
    .require_core_tables(erp_security::auth::core_tables())
    .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
    .build()?;
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(SUPERUSER_ID)
                .isolation(erp_orm::Isolation::ReadCommitted)
                .on_primary(),
        )
        .await?;
    let report = match tx.install_audit_schema().await {
        Ok(report) => report,
        Err(error) => {
            tx.rollback().await?;
            return Err(error.into());
        }
    };
    tx.commit().await?;
    Ok(format!(
        "Audit table {} for database {}: {} partition(s) created, {} already present (today plus {} days).",
        if report.created_table {
            "installed"
        } else {
            "verified"
        },
        config.database(),
        report.created_partitions.len(),
        report.existing_partitions.len(),
        erp_orm::audit::AUDIT_PREMAKE_DAYS
    ))
}

pub(crate) fn publish_grants(args: &ServerArgs) -> Result<String> {
    runtime()?.block_on(publish_grants_async(args))
}

pub(crate) async fn publish_grants_async(args: &ServerArgs) -> Result<String> {
    let config = config(&args.config)?;
    // Grant publication must never load/recover pending module operations or run hooks.
    // The linked registry supplies only the raw owner transaction and existing grantee checks.
    let orm = Orm::builder(
        crate::access::registry()?,
        pool_with_size(config.owner_connection(), 1)?,
    )
    .require_core_tables(erp_security::auth::core_tables())
    .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
    .build()?;
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(SUPERUSER_ID)
                .isolation(erp_orm::Isolation::ReadCommitted)
                .on_primary(),
        )
        .await?;
    let locked = tx
        .query(
            "SELECT pg_try_advisory_xact_lock($1,$2)",
            &[
                &erp_modules::OPERATION_LOCK.0,
                &erp_modules::OPERATION_LOCK.1,
            ],
        )
        .await?;
    let locked: bool = locked
        .first()
        .ok_or_else(|| failed("runtime grant lifecycle lock returned no state"))?
        .try_get(0)
        .map_err(|_| failed("invalid runtime grant lifecycle lock state"))?;
    if !locked {
        return Err(CliError::ModuleOperation(Box::new(
            erp_modules::Error::OperationInProgress,
        )));
    }
    let pending = tx
        .query(
            "SELECT EXISTS (SELECT 1 FROM ir_module_module WHERE state IN ('to install','to upgrade','to remove'))",
            &[],
        )
        .await?;
    let pending: bool = pending
        .first()
        .ok_or_else(|| failed("runtime grant lifecycle state missing"))?
        .try_get(0)
        .map_err(|_| failed("invalid runtime grant lifecycle state"))?;
    if pending {
        tx.rollback().await?;
        return Err(failed(
            "Pending module operations require operator recovery before publishing runtime grants",
        ));
    }
    // A database made by an older binary may predate the core tables the grants name (the install
    // audit, the WebAssembly lifecycle tables): create the missing ones first, as an authorized
    // install does, under the schema lock and the lifecycle lock held above.
    let ensured = async {
        tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await?;
        erp_orm::core_ddl_run(&mut tx, erp_orm::CorePhase::BeforeModels).await?;
        erp_orm::core_ddl_run(&mut tx, erp_orm::CorePhase::AfterModels).await?;
        Ok::<(), erp_fields::Error>(())
    }
    .await;
    if let Err(error) = ensured {
        tx.rollback().await?;
        return Err(error.into());
    }
    let publication = grant_runtime(&mut tx, &config).await;
    if let Err(error) = publication {
        tx.rollback().await?;
        return Err(error);
    }
    tx.commit().await?;
    let runtime_pool = pool_with_size(config.runtime_connection(), 1)?;
    let client = runtime_pool.get().await.map_err(|_| {
        failed("Runtime grants committed but runtime verification connection failed")
    })?;
    crate::uom_grants::verify(&client).await?;
    let missing: bool = client
        .query_one(MISSING_RUNTIME_GRANTS_SQL, &[])
        .await
        .map_err(|_| failed("Runtime grants committed but runtime verification query failed"))?
        .try_get(0)
        .map_err(|_| failed("invalid runtime publication grant state"))?;
    if missing {
        return Err(failed(
            "Runtime grants committed but schema, tables or sequences remain unusable; retry after module operations complete",
        ));
    }
    let mark_leaked: bool = client
        .query_one(TRANSLATION_MARK_LEAK_SQL, &[])
        .await
        .map_err(|_| {
            failed("Runtime grants committed but translation mark privilege verification failed")
        })?
        .try_get(0)
        .map_err(|_| failed("invalid translation mark privilege state"))?;
    if mark_leaked {
        return Err(failed(
            "Runtime grants committed, but the runtime role can still access erp_translated_verified through PUBLIC or another role; revoke that access, then publish again",
        ));
    }
    let generated_writes: Option<bool> = client
        .query_one(GENERATED_PROVENANCE_WRITE_LEAK_SQL, &[])
        .await
        .map_err(|_| {
            failed(
                "Runtime grants committed but generated provenance privilege verification failed",
            )
        })?
        .try_get(0)
        .map_err(|_| failed("invalid generated provenance privilege state"))?;
    match generated_writes {
        Some(false) => {}
        Some(true) => {
            return Err(failed(
                "Runtime grants committed, but the runtime role can still write erp_schema_generated through table or column grants, PUBLIC, or another role; revoke that access, then publish again",
            ));
        }
        None => {
            return Err(failed(
                "Runtime grants committed, but the table erp_schema_generated is missing; run the schema sync",
            ));
        }
    }
    // `has_table_privilege` counts PUBLIC and every role the runtime role reaches, which a plain
    // revoke from the role does not: the vacuum leases must be out of its reach.
    let leaked: Option<bool> = client
        .query_one(
            "SELECT CASE WHEN to_regclass('public.erp_transient_vacuum_state') IS NULL THEN NULL \
                ELSE has_table_privilege('public.erp_transient_vacuum_state','SELECT') \
                  OR has_table_privilege('public.erp_transient_vacuum_state','INSERT') \
                  OR has_table_privilege('public.erp_transient_vacuum_state','UPDATE') \
                  OR has_table_privilege('public.erp_transient_vacuum_state','DELETE') END",
            &[],
        )
        .await
        .map_err(|_| failed("Runtime grants committed but the vacuum state check failed"))?
        .try_get(0)
        .map_err(|_| failed("invalid vacuum state privilege check"))?;
    match leaked {
        Some(false) => {}
        Some(true) => {
            return Err(failed(
                "Runtime grants committed, but the runtime role can still read or write erp_transient_vacuum_state (through PUBLIC or a role it is a member of); revoke that access, then publish again",
            ));
        }
        None => {
            return Err(failed(
                "Runtime grants committed, but the table erp_transient_vacuum_state is missing; run the schema sync",
            ));
        }
    }
    Ok(format!(
        "Runtime grants published and verified for database {}.",
        config.database()
    ))
}

pub(crate) async fn grant_runtime(
    tx: &mut erp_orm::Transaction,
    config: &ServerConfig,
) -> Result<()> {
    let connection = config.runtime_connection();
    let role = connection
        .get_user()
        .ok_or_else(|| failed("runtime role missing"))?;
    // This validates that the runtime role cannot inherit/escalate to an owner or superuser.
    tx.grant_signal_request_role(role).await?;
    let role_sql = erp_sql::quote(role);
    tx.batch(&format!("GRANT USAGE ON SCHEMA public TO {role_sql}; GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {role_sql}; GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO {role_sql}; REVOKE ALL ON erp_module_install_audit FROM {role_sql}; REVOKE ALL ON SEQUENCE erp_module_install_audit_id_seq FROM {role_sql}")).await?;
    // The validation marks may not exist until a translated model is first synchronized.
    let marks = tx
        .query(
            "SELECT pg_catalog.to_regclass('public.erp_translated_verified') IS NOT NULL",
            &[],
        )
        .await?;
    let [marks] = marks.as_slice() else {
        return Err(erp_fields::Error::programming(
            "Translation verification table state is missing",
        )
        .into());
    };
    let exists: bool = marks.try_get(0).map_err(|source| {
        erp_fields::Error::programming("Invalid translation verification table state")
            .with_source(source)
    })?;
    if exists {
        tx.batch(&format!(
            "REVOKE ALL ON public.erp_translated_verified FROM {role_sql}"
        ))
        .await?;
    }
    tx.batch(&format!("REVOKE ALL ON erp_node, erp_node_detail, erp_node_rollout, erp_node_feature, erp_node_feature_digest FROM {role_sql}; REVOKE INSERT, DELETE, TRUNCATE, REFERENCES, TRIGGER ON erp_auth_gc_state FROM {role_sql}")).await?;
    let helper_owner = tx.query("SELECT r.rolname::text FROM pg_proc p JOIN pg_roles r ON r.oid=p.proowner WHERE p.oid=to_regprocedure('public.erp_node_feature_member(text,integer,uuid,bytea)') AND p.prosecdef AND NOT r.rolcanlogin", &[]).await?;
    if let Some(owner) = helper_owner.first() {
        let owner: String = owner.try_get(0).map_err(|error| {
            erp_fields::Error::db("cannot decode login feature helper owner").with_source(error)
        })?;
        tx.grant_node_feature_roles(&owner, role).await?;
    }
    if let Some(node_connection) = config.configured_node_connection() {
        let node_role = node_connection
            .get_user()
            .ok_or_else(|| failed("heartbeat role missing"))?;
        let related = tx
            .query(
                crate::server_runtime::NODE_ROLE_MEMBERSHIP_SQL,
                &[&role, &node_role],
            )
            .await?;
        let related: bool = related
            .first()
            .ok_or_else(|| failed("serving role membership state missing"))?
            .try_get(0)
            .map_err(|_| failed("invalid serving role membership state"))?;
        if related {
            return Err(erp_fields::Error::programming(
                "request and heartbeat roles must not be members of each other",
            )
            .into());
        }
        tx.grant_node_role(node_role).await?;
        tx.batch(&format!(
            "GRANT USAGE ON SCHEMA public TO {}",
            erp_sql::quote(node_role)
        ))
        .await?;
    }
    // A module-initialized legacy database can still need its first administrator.
    // Audit storage is created by authorized lifecycle operations, not ordinary load.
    // Grant publication must permit both objects absent, revoke both when present,
    // and refuse a damaged half-present schema rather than leave private access behind.
    let audit = tx.query(
        "SELECT pg_catalog.to_regclass('public.erp_module_lifecycle_audit') IS NOT NULL, pg_catalog.to_regclass('public.erp_module_lifecycle_audit_id_seq') IS NOT NULL",
        &[],
    ).await?;
    let [audit] = audit.as_slice() else {
        return Err(
            erp_fields::Error::programming("Lifecycle audit storage state is missing").into(),
        );
    };
    let table_exists: bool = audit.try_get(0).map_err(|source| {
        erp_fields::Error::programming("Invalid lifecycle audit table state").with_source(source)
    })?;
    let sequence_exists: bool = audit.try_get(1).map_err(|source| {
        erp_fields::Error::programming("Invalid lifecycle audit sequence state").with_source(source)
    })?;
    match (table_exists, sequence_exists) {
        // Expected legacy absence: there is no private object to grant or revoke.
        (false, false) => {}
        (true, true) => tx.batch(&format!("REVOKE ALL ON erp_module_lifecycle_audit FROM {role_sql}; REVOKE ALL ON SEQUENCE erp_module_lifecycle_audit_id_seq FROM {role_sql}")).await?,
        (true, false) | (false, true) => return Err(erp_fields::Error::programming(
            "Lifecycle audit storage is incomplete; operator recovery is required",
        ).into()),
    }
    tx.batch(&format!(
        "REVOKE ALL ON public.erp_schema_generated FROM {role_sql}; GRANT SELECT ON public.erp_schema_generated TO {role_sql}"
    ))
    .await?;
    // Identity storage is created by schema-sync. Legacy absence is valid; a partial
    // schema is not. Effective PUBLIC/inherited privileges are checked again as runtime.
    let identity = tx.query("SELECT pg_catalog.to_regclass('public.installed_addon_identity') IS NOT NULL, pg_catalog.to_regclass('public.installed_addon_identity_audit') IS NOT NULL, pg_catalog.to_regclass('public.installed_addon_identity_audit_id_seq') IS NOT NULL", &[]).await?;
    let [identity] = identity.as_slice() else {
        return Err(
            erp_fields::Error::programming("Installed identity storage state is missing").into(),
        );
    };
    let mut present = [false; 3];
    for (column, value) in present.iter_mut().enumerate() {
        *value = identity.try_get(column).map_err(|source| {
            erp_fields::Error::programming("Invalid installed identity storage state")
                .with_source(source)
        })?;
    }
    match present {
        [false, false, false] => {} // Expected legacy absence; never enroll on publication.
        [true, true, true] => tx.batch(&format!("REVOKE ALL ON public.installed_addon_identity, public.installed_addon_identity_audit FROM {role_sql}; REVOKE ALL ON SEQUENCE public.installed_addon_identity_audit_id_seq FROM {role_sql}")).await?,
        _ => return Err(erp_fields::Error::programming("Installed identity storage is incomplete; operator recovery is required").into()),
    }
    tx.grant_signal_request_role(role).await?;
    // The job queue: the blanket grant above gave the three tables full CRUD; narrow it.
    tx.grant_jobs_runtime_role(role).await?;
    // The attachment store registry: the runtime role registers a store row on the
    // first upload (SELECT, INSERT) and never rewrites or removes one; its namespace and
    // fingerprint name every content key. The blanket grant above gave full CRUD; narrow it.
    let blob_store = tx
        .query(
            "SELECT pg_catalog.to_regclass('public.ir_blob_store') IS NOT NULL",
            &[],
        )
        .await?;
    let blob_store_installed: bool = blob_store
        .first()
        .ok_or_else(|| failed("attachment store registry state missing"))?
        .try_get(0)
        .map_err(|_| failed("invalid attachment store registry state"))?;
    if blob_store_installed {
        tx.batch(&format!(
            "REVOKE UPDATE, DELETE, TRUNCATE ON public.ir_blob_store FROM {role_sql}"
        ))
        .await?;
    }
    // Every publication installs or verifies the audit table and refreshes its partition window
    // , then grants the runtime role its insert-only rights on every installed event
    // table. The outbox table is installed by its own lifecycle step; the grant covers it when it
    // exists.
    tx.install_audit_schema().await?;
    tx.grant_event_runtime_role(role).await?;
    // The transient vacuum leases belong to the maintenance role only.
    tx.revoke_vacuum_request_role(role).await?;
    erp_modules::revoke_post_commit_access(tx, role).await?;
    // The WebAssembly addon audit is insert-only and closed to the runtime role; the package
    // store is readable but written only by the lifecycle.
    erp_modules::wasm_lifecycle::revoke_wasm_lifecycle_runtime_writes(tx, role).await?;
    erp_addon_web::assets::store::grant_runtime_read(tx, role).await?;
    crate::uom_grants::publish(tx, role).await?;
    // Check reachable and effective rights only after every grant/revocation in this transaction.
    let rows = tx
        .query(crate::server_runtime::REQUEST_NODE_PRIVILEGE_SQL, &[&role])
        .await?;
    let leaked: bool = rows
        .first()
        .ok_or_else(|| failed("runtime node privilege state missing"))?
        .try_get(0)
        .map_err(|_| failed("invalid runtime node privilege state"))?;
    if leaked {
        return Err(failed(
            "request role must not access node membership tables",
        ));
    }
    if let Some(node_connection) = config.configured_node_connection() {
        let node_role = node_connection
            .get_user()
            .ok_or_else(|| failed("heartbeat role missing"))?;
        let rows = tx
            .query(
                crate::server_runtime::HEARTBEAT_BUSINESS_PRIVILEGE_SQL,
                &[&node_role],
            )
            .await?;
        let leaked: bool = rows
            .first()
            .ok_or_else(|| failed("heartbeat privilege state missing"))?
            .try_get(0)
            .map_err(|_| failed("invalid heartbeat privilege state"))?;
        if leaked {
            return Err(failed(
                "heartbeat role must not access business or authentication tables",
            ));
        }
    }
    Ok(())
}

/// The ORM builder every server-composed ORM starts from: the base models' implementations and
/// constraints, the external-id and attachment unlink hooks, and the installed packages' WebAssembly method layers,
/// computes and constraints. The request ORM adds its policy, resolver, RPC and signal on top; the
/// maintenance commands (`erp db vacuum-transient`) use it as it is, so both run the same
/// overrides and unlink hooks.
fn server_orm_builder(
    registry: Arc<erp_registry::Registry>,
    pool: deadpool_postgres::Pool,
    wasm_methods: Vec<WasmMethodImpl>,
    checks: erp_modules::PackageChecks,
) -> erp_orm::OrmBuilder {
    Orm::builder(registry, pool)
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .impls(erp_modules::base_models::IMPLS.iter().copied())
        .constraints(erp_modules::base_models::CONSTRAINTS.iter().copied())
        .unlink_hook(erp_data::model_data::UNLINK_HOOK)
        .unlink_hook(erp_addon_base::attachments::UNLINK_HOOK)
        .wasm_methods(wasm_methods)
        .computes(checks.computes)
        .constraints(checks.constraints)
}

/// Keep the loaded generation's immutable H18 table when applying request security.
/// Rebuilding a policy-bound ORM does not implicitly copy generation attachments.
fn server_request_orm_builder(
    source: &Orm,
    pool: deadpool_postgres::Pool,
    wasm_methods: Vec<WasmMethodImpl>,
    checks: erp_modules::PackageChecks,
) -> Result<erp_orm::OrmBuilder> {
    let table = source
        .env(SUPERUSER_ID, erp_orm::Context::new())
        .generation_attachment::<erp_sdk::views::PublishedViewTypes>()?;
    table
        .require_registry(source.registry())
        .map_err(|error| erp_fields::Error::programming(error.to_string()))?;
    let registry = Arc::clone(source.registry());
    Ok(
        server_orm_builder(Arc::clone(&registry), pool, wasm_methods, checks)
            .generation_attachment(erp_orm::GenerationAttachment::new(registry, table)),
    )
}

/// `erp db vacuum-transient`: the registry and the ORM are composed from the database the way the
/// server composes them (installed modules from the addons path in the configuration, packages,
/// WebAssembly method layers, unlink hooks), connected as the owner role. A module that is
/// unavailable or waiting for post-commit recovery stops the command: its transient models would
/// be missing from the registry, and a run that silently skipped them would look like success.
pub(crate) fn vacuum_transient(args: &VacuumTransientArgs) -> Result<String> {
    runtime()?.block_on(async {
        let config = config(&args.config)?;
        let owner_pool = pool_with_size(config.owner_connection(), 1)?;
        let owner = owner_pool
            .get()
            .await
            .map_err(|_| failed("could not inspect lifecycle state"))?;
        let pending: bool = owner
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM ir_module_module WHERE state IN ('to install','to upgrade','to remove'))",
                &[],
            )
            .await
            .map_err(|_| failed("could not inspect lifecycle state"))?
            .try_get(0)
            .map_err(|_| failed("invalid lifecycle state"))?;
        drop(owner);
        if pending {
            return Err(failed(
                "Pending module operations require operator recovery before the vacuum can compose the registry",
            ));
        }
        let node = modules(&config, None)?;
        let loaded = node
            .load()
            .await
            .map_err(|e| CliError::ModuleOperation(Box::new(e)))?;
        if !loaded.unavailable.is_empty() || !loaded.post_commit_pending.is_empty() {
            return Err(failed(
                "Installed modules are unavailable or require post-commit recovery: their transient models are not in the registry, so nothing was vacuumed; repair the modules (erp module commands) and run again",
            ));
        }
        let wasm_methods = node
            .wasm_methods(&loaded.orm)
            .map_err(|e| CliError::ModuleOperation(Box::new(e)))?;
        let checks = node
            .package_checks()
            .map_err(|e| CliError::ModuleOperation(Box::new(e)))?;
        let orm = server_orm_builder(
            Arc::clone(loaded.orm.registry()),
            pool_with_size(config.owner_connection(), 2)?,
            wasm_methods,
            checks,
        )
        .build()?;
        crate::db::vacuum_with(args, &orm).await
    })
}

pub(crate) fn grant_vacuum_role(args: &GrantVacuumRoleArgs) -> Result<String> {
    runtime()?.block_on(grant_vacuum_role_async(args))
}

/// `erp db grant-vacuum-role`: `SELECT, INSERT, UPDATE, DELETE` on the vacuum state table for the
/// maintenance role, as the owner. The runtime role is refused: it must never hold the table.
pub(crate) async fn grant_vacuum_role_async(args: &GrantVacuumRoleArgs) -> Result<String> {
    let config = config(&args.config)?;
    if config.runtime_connection().get_user() == Some(args.role.as_str()) {
        return Err(erp_fields::Error::validation(format!(
            "role {:?} is the runtime role of this deployment: it must not hold the vacuum state table",
            args.role
        ))
        .into());
    }
    let orm = Orm::builder(
        crate::access::registry()?,
        pool_with_size(config.owner_connection(), 1)?,
    )
    .require_core_tables(erp_security::auth::core_tables())
    .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
    .build()?;
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(SUPERUSER_ID)
                .isolation(erp_orm::Isolation::ReadCommitted)
                .on_primary(),
        )
        .await?;
    // a role the runtime role is a member of would hand the state table back to requests
    let runtime_role = config.runtime_connection().get_user().map(str::to_owned);
    if let Some(runtime_role) = runtime_role {
        let rows = tx
            .query(
                "SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1) \
                             AND EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $2) \
                        THEN pg_has_role($1, $2, 'MEMBER') ELSE false END",
                &[&runtime_role, &args.role],
            )
            .await?;
        let member: bool = rows
            .first()
            .ok_or_else(|| failed("role membership check returned no row"))?
            .try_get(0)
            .map_err(|_| failed("invalid role membership state"))?;
        if member {
            tx.rollback().await?;
            return Err(erp_fields::Error::validation(format!(
                "the runtime role {runtime_role:?} is a member of {:?}: granting the vacuum state table to it would reach requests",
                args.role
            ))
            .into());
        }
    }
    if let Err(error) = tx.grant_vacuum_role(&args.role).await {
        tx.rollback().await?;
        return Err(error.into());
    }
    tx.commit().await?;
    Ok(format!(
        "Role {} may now run the transient vacuum on database {}.",
        args.role,
        config.database()
    ))
}

async fn runtime_auth(
    runtime_pool: deadpool_postgres::Pool,
    policy_pool: deadpool_postgres::Pool,
    loaded: &Orm,
    baseline: erp_orm::Generations,
    wasm_methods: Vec<WasmMethodImpl>,
    checks: erp_modules::PackageChecks,
    admission: erp_orm::node::NodeAdmission,
) -> Result<Arc<Auth>> {
    let registry = loaded.registry();
    let source = PolicySource::build(Arc::clone(registry), policy_pool).await?;
    let resolver = Resolver::for_registry(registry)?;
    let provider: Arc<dyn erp_orm::PolicyProvider> = Arc::new(source.clone());
    let orm = server_request_orm_builder(loaded, runtime_pool, wasm_methods, checks)?
        .resolver(resolver)
        .policy(provider)
        .rpc_impls([
            &CATALOG_RPC,
            &addon_detail::DETAIL_RPC,
            &addon_detail::PREVIEW_RPC,
        ])
        .signal(Signal::new(baseline).with_refresh(NativeRefresh { source }))
        .build()?;
    Auth::with_membership(orm, AuthConfig::DEFAULT, HashPool::global(), admission)
        .await
        .map(Arc::new)
        .map_err(Into::into)
}

pub(crate) fn serve(args: &ServerArgs) -> Result<()> {
    // Before anything else: until the subscriber is installed every `tracing` event is dropped.
    // `RUST_LOG` defaults to `warn`; an unusable value stops the start.
    erp_server::logging::install_from_env().map_err(failed)?;
    let runtime = runtime()?;
    let mut shutdown_deadline = None;
    let result = runtime.block_on(async {
        let config = config(&args.config)?;
        let publication_pool = pool_with_size(config.runtime_connection(), 1)?;
        let startup = publication_pool
            .get()
            .await
            .map_err(|_| failed("could not read native lifecycle baseline"))?;
        let baseline = Signal::read(&startup).await?;
        drop(startup);
        let node = modules(&config, Some(baseline))?;
        let system = Arc::clone(node.system());
        // The catalogue RPC is a plain function pointer: it reads the server's module system
        // (and so its addon directory) from here. Set once per process, before serving.
        CATALOG_SYSTEM
            .set(Arc::clone(&system))
            .map_err(|_| failed("the addon catalogue was already bound in this process"))?;
        let asset_store =
            erp_server::assets::AssetStore::new(config.startup_limits().assets_memory_bytes())
                .map_err(|error| failed(error.to_string()))?;
        // Stored package addons are absent from folder discovery. Verify their archives and
        // lifecycle approvals before the immutable membership catalogue is constructed.
        node.lifecycle()
            .admit_installed_packages()
            .await
            .map_err(|error| CliError::ModuleOperation(Box::new(error)))?;
        let membership = crate::server_runtime::register(&config, Arc::clone(&system)).await?;
        let admission = membership.admission();
        let identity_addons = membership.identity().addons().digest();
        let startup_result: Result<_> = async {
            let lifecycle = Arc::new(NativeLifecycle {
                admission: admission.clone(),
                identity_addons,
                asset_store: Arc::clone(&asset_store),
                config: Arc::clone(&config),
                node,
                system,
                runtime_pool: pool_with_size(config.runtime_connection(), 8)?,
                policy_pool: pool_with_size(config.runtime_connection(), 2)?,
                publication_pool,
                installing: tokio::sync::Mutex::new(()),
                asset_snapshot: tokio::sync::Mutex::new(None),
                asset_refusal: AssetRefusalGate::default(),
                refresh_backoff: crate::node::RefreshBackoff::new(),
                pending_grants_run: std::sync::atomic::AtomicU32::new(0),
            });
            let (auth, rpc, assets) = lifecycle
                .compose_current(true)
                .await
                .map_err(CliError::from)?;
            let app = WebApp::new_with_store(Arc::clone(&config), auth, asset_store)
                .map_err(|e| failed(e.to_string()))?
                .with_rpc(rpc)
                .with_assets(crate::ui_assets::RUNTIME_ABI, assets)
                .map_err(|error| failed(error.to_string()))?
                .with_asset_authority(lifecycle.clone())
                .with_lifecycle(lifecycle);
            let listener =
                tokio::net::TcpListener::bind(config.bind().map_err(|e| failed(e.to_string()))?)
                    .await
                    .map_err(|_| failed("could not bind the configured HTTP listener"))?;
            Ok((Arc::new(app.with_admission(admission)), listener))
        }
        .await;
        let (app, listener) = match startup_result {
            Ok(started) => started,
            Err(error) => {
                return match tokio::time::timeout(config.shutdown_deadline(), membership.shutdown())
                    .await
                {
                    Ok(Ok(())) => Err(error),
                    Ok(Err(cleanup)) => Err(failed(format!(
                        "{error}; node startup cleanup failed: {cleanup}"
                    ))),
                    Err(_) => Err(failed(format!(
                        "{error}; node startup cleanup deadline expired"
                    ))),
                };
            }
        };
        crate::server_runtime::run(app, listener, config, membership, &mut shutdown_deadline).await
    });
    finish_server_runtime(runtime, shutdown_deadline);
    result
}

fn finish_server_runtime(runtime: tokio::runtime::Runtime, deadline: Option<tokio::time::Instant>) {
    let remaining = deadline.map_or(std::time::Duration::ZERO, |until| {
        until.saturating_duration_since(tokio::time::Instant::now())
    });
    // Started blocking jobs cannot be cancelled cooperatively. Do not wait outside the
    // server's absolute budget; a CLI process exit reclaims any work still running.
    runtime.shutdown_timeout(remaining);
}

// Pools belong to the replica, rather than to a registry generation. Old requests may drain
// while a replacement registry serves new work without multiplying connection allowances.
struct NativeLifecycle {
    admission: erp_orm::node::NodeAdmission,
    identity_addons: erp_orm::node::Digest,
    asset_store: Arc<erp_server::assets::AssetStore>,
    // The ORM is retained only for bounded cache-miss reads of private asset storage; warm
    // admission reads the runtime connection directly and creates no ORM environment.
    asset_snapshot:
        tokio::sync::Mutex<Option<(Orm, Arc<erp_server::assets::PublishedAssets>, String)>>,
    // The last refused request-path re-admission. It is checked before the snapshot lock, so a
    // persistent refusal costs each request one authority read, not a queued bounded reload.
    asset_refusal: AssetRefusalGate,
    config: Arc<ServerConfig>,
    /// The module system with the lifecycle of the package addons: every load of the installed
    /// set goes through it, so a node follows installs of WebAssembly addons by other nodes.
    node: Node,
    system: Arc<ModuleSystem>,
    runtime_pool: deadpool_postgres::Pool,
    policy_pool: deadpool_postgres::Pool,
    publication_pool: deadpool_postgres::Pool,
    installing: tokio::sync::Mutex<()>,
    /// Spaces out the rebuilds of the registry after one failed (review r1 M3).
    refresh_backoff: crate::node::RefreshBackoff,
    /// Consecutive pending-grant refresh results; see `PENDING_GRANTS_FREE_RETRIES`.
    pending_grants_run: std::sync::atomic::AtomicU32,
}
struct NativeRefresh {
    source: PolicySource,
}
impl erp_orm::Refresh for NativeRefresh {
    fn refresh<'a>(
        &'a self,
        changes: &'a erp_orm::Changes,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::Refreshed>> {
        Box::pin(async move {
            if changes.registry {
                return Ok(erp_orm::Refreshed::Superseded);
            }
            erp_orm::Refresh::refresh(&self.source, changes).await
        })
    }
}

/// Installation commits its schema before its grant publication commits. A node that refreshes in
/// that gap sees this transient state; it is not a rebuild failure and must not start a backoff.
const RUNTIME_GRANTS_PENDING: &str =
    "Runtime grants are not published; retry after installation completes";

/// Consecutive pending-grant results that are retried by the next request without a backoff. A
/// longer run is a standing grant fault, not an install gap, and backs off like any other failure.
/// The count is of requests, not time: many requests queued in one gap use the retries up faster.
const PENDING_GRANTS_FREE_RETRIES: u32 = 8;

/// Records a failed rebuild in `backoff`. The first `PENDING_GRANTS_FREE_RETRIES` consecutive
/// pending-grants results start no backoff; `pending_run` counts them and any other failure
/// resets it. Returns the wait a recorded failure started.
fn record_refresh_failure(
    backoff: &crate::node::RefreshBackoff,
    pending_run: &std::sync::atomic::AtomicU32,
    error: &erp_fields::Error,
    now: std::time::Instant,
) -> Option<std::time::Duration> {
    use std::sync::atomic::Ordering::Relaxed;
    let pending = error.kind() == erp_fields::ErrorKind::Concurrency
        && error.message() == RUNTIME_GRANTS_PENDING;
    if !pending {
        pending_run.store(0, Relaxed);
    } else if pending_run.fetch_add(1, Relaxed) < PENDING_GRANTS_FREE_RETRIES {
        return None;
    }
    Some(backoff.failed(now))
}

impl NativeLifecycle {
    // Use the same complete producer as registration, including intrinsic native descriptors.
    async fn check_catalogue_identity(&self) -> erp_fields::Result<()> {
        let system = Arc::clone(&self.system);
        let current_addons = tokio::task::spawn_blocking(move || {
            system.with_index(crate::server_runtime::native_addon_identity)
        })
        .await
        .map_err(|error| {
            erp_fields::Error::programming(
                "addon identity task failed during authentication composition",
            )
            .with_source(error)
        })?
        .map_err(module_error)?;
        if current_addons.digest() != self.identity_addons {
            return Err(erp_fields::Error::programming(
                "addon catalogue identity changed; drain and restart node membership before authentication publication",
            ));
        }
        Ok(())
    }

    /// The required audit sink: the server serves only with an
    /// audit table the request role can insert into and at least one day of partitions ahead.
    /// With a single day left it serves and logs an error; the operator refreshes the window with
    /// `erp audit maintain` or a grant publication.
    async fn admit_audit_store(&self, auth: &Auth) -> erp_fields::Result<()> {
        let state = auth.orm().check_audit_store().await?;
        audit_admission(&state)?;
        if state != erp_orm::audit::AuditStoreState::Ready {
            tracing::error!(
                state = %state.describe(),
                "audit partition window is short: run `erp audit maintain` or publish the runtime grants"
            );
        }
        if self.config.runtime_connection().get_user() == self.config.owner_connection().get_user()
        {
            tracing::warn!(
                "the runtime and owner connections use the same role: the audit trail is not \
                 protected from the request role by privileges"
            );
        }
        Ok(())
    }

    async fn compose_current(
        &self,
        startup: bool,
    ) -> erp_fields::Result<(
        Arc<Auth>,
        Arc<erp_rpc::RpcService>,
        erp_server::web::AssetAdmission,
    )> {
        let client = self
            .publication_pool
            .get()
            .await
            .map_err(|_| erp_fields::Error::db("could not read registry baseline"))?;
        // Read BEFORE the registry and policy are built: a concurrent install must supersede
        // this candidate rather than being mislabeled as already applied.
        let baseline = Signal::read(&client).await?;
        // Native installation commits schema before its publication/grant phase. Another node
        // must stay in maintenance until the nonowner role can use the schema, ordinary tables
        // and sequences. CASE prevents sequence checks from running on table/index OIDs.
        if crate::uom_grants::pending(&client).await? {
            return Err(erp_fields::Error::new(
                erp_fields::ErrorKind::Concurrency,
                RUNTIME_GRANTS_PENDING,
            ));
        }
        crate::uom_grants::verify(&client).await?;
        let missing_grants: bool = client
            .query_one(MISSING_RUNTIME_GRANTS_SQL, &[])
            .await
            .map_err(|error| {
                erp_fields::Error::db("could not inspect runtime publication grants")
                    .with_source(error)
            })?
            .try_get(0)
            .map_err(|error| {
                erp_fields::Error::db("invalid runtime publication grant state").with_source(error)
            })?;
        if missing_grants {
            return Err(erp_fields::Error::new(
                erp_fields::ErrorKind::Concurrency,
                RUNTIME_GRANTS_PENDING,
            ));
        }
        let mark_leaked: bool = client
            .query_one(TRANSLATION_MARK_LEAK_SQL, &[])
            .await
            .map_err(|error| {
                erp_fields::Error::db("could not inspect translation mark privileges")
                    .with_source(error)
            })?
            .try_get(0)
            .map_err(|error| {
                erp_fields::Error::db("invalid translation mark privilege state").with_source(error)
            })?;
        if mark_leaked {
            return Err(erp_fields::Error::new(
                erp_fields::ErrorKind::Concurrency,
                "Runtime role can access erp_translated_verified; revoke the access and publish grants",
            ));
        }
        let pending: bool = client.query_one("SELECT EXISTS (SELECT 1 FROM ir_module_module WHERE state IN ('to install','to upgrade','to remove'))", &[]).await
            .map_err(|error| erp_fields::Error::db("could not inspect lifecycle state").with_source(error))?.try_get(0)
            .map_err(|error| erp_fields::Error::db("invalid lifecycle state").with_source(error))?;
        drop(client);
        if pending {
            return Err(erp_fields::Error::new(
                erp_fields::ErrorKind::Concurrency,
                "Pending module operations require operator recovery",
            ));
        }
        self.check_catalogue_identity().await?;
        let loaded = self.node.load().await.map_err(module_error)?;
        // Loading re-admits database packages and may replace or remove index entries. Names
        // alone cannot bind a same-name replacement to this membership's original bytes.
        self.check_catalogue_identity().await?;
        // The request ORM is built here with the runtime role, so it gets the WebAssembly method
        // layers of the loaded packages from the same components the owner-side ORM runs.
        let wasm_methods = self.node.wasm_methods(&loaded.orm).map_err(module_error)?;
        let checks = self.node.package_checks().map_err(module_error)?;
        if !loaded.unavailable.is_empty() || !loaded.post_commit_pending.is_empty() {
            return Err(erp_fields::Error::new(
                erp_fields::ErrorKind::Concurrency,
                "Installed modules are unavailable or require post-commit recovery",
            ));
        }
        let asset_admission = if !loaded
            .orm
            .registry()
            .addons()
            .iter()
            .any(|addon| addon.name == "web")
        {
            Err(erp_fields::Error::validation(
                "Web addon is not installed; no client assets may be served",
            ))
        } else if startup {
            match self.node.admit_ui_assets(&loaded, &baseline).await {
                Ok(()) => published_ui_assets(&loaded.orm, &self.asset_store).await,
                Err(error) => Err(error),
            }
        } else {
            // Registry publication only fetches committed bytes. It never retries assembly.
            published_ui_assets(&loaded.orm, &self.asset_store).await
        };
        let asset_admission = match asset_admission {
            Ok(assets) => erp_server::web::AssetAdmission::Published(assets),
            Err(error) => {
                // HTTP clients only see a fixed 503 text; the detail is kept here for the operator.
                tracing::error!(%error, startup, "UI asset admission failed; web assets answer 503");
                erp_server::web::AssetAdmission::Broken(Arc::from(error.to_string()))
            }
        };
        *self.asset_snapshot.lock().await = match &asset_admission {
            erp_server::web::AssetAdmission::Published(assets) => {
                // A fresh admitted generation supersedes any remembered request-path refusal.
                self.asset_refusal.clear()?;
                Some((loaded.orm.clone(), Arc::clone(assets), String::new()))
            }
            erp_server::web::AssetAdmission::Broken(_) => None,
            _ => {
                return Err(erp_fields::Error::programming(
                    "Unknown asset admission state",
                ));
            }
        };
        let auth = runtime_auth(
            self.runtime_pool.clone(),
            self.policy_pool.clone(),
            &loaded.orm,
            baseline,
            wasm_methods,
            checks,
            self.admission.clone(),
        )
        .await
        .map_err(cli_field_error)?;
        self.admit_audit_store(&auth).await?;
        let rpc = runtime_rpc(&auth, &self.config).await?;
        Ok((auth, rpc, asset_admission))
    }
}

/// Whether the server may serve with this audit store state: the
/// table has to be installed with its shape, insertable by the request role, with a partition for
/// today. One day ahead is served (with an error log); none is refused.
fn audit_admission(state: &erp_orm::audit::AuditStoreState) -> erp_fields::Result<()> {
    if state.allows_writes() {
        return Ok(());
    }
    Err(erp_fields::Error::new(
        erp_fields::ErrorKind::Concurrency,
        format!(
            "audit: the audit store is not ready ({}); publish the runtime grants as the owner \
             role (`erp db publish-grants`) or run `erp audit maintain`",
            state.describe()
        ),
    ))
}

struct SessionAuthorizer<'a> {
    auth: &'a Auth,
    secret: &'a erp_security::auth::SessionSecret,
}
impl erp_modules::install::InstallAuthorizer for SessionAuthorizer<'_> {
    fn authorize<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_modules::install::InstallAuthorization>>
    {
        Box::pin(async move {
            let authorization = self.auth.check_session_in(tx, self.secret).await?;
            if !authorization.has_groups("base.group_system")? {
                return Err(erp_fields::Error::access(
                    "Module installation requires system administration authority",
                ));
            }
            erp_modules::install::InstallAuthorization::new(
                authorization.principal().clone(),
                authorization.session_id().to_text(),
            )
        })
    }
}
impl erp_server::web::WebLifecycle for NativeLifecycle {
    fn refresh<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_server::web::Publication>> {
        Box::pin(async move {
            let _loading = self.installing.lock().await;
            if !auth.orm().signal().is_some_and(Signal::is_superseded) {
                return Err(erp_fields::Error::programming(
                    "Registry refresh requested without a superseded application",
                ));
            }
            // A rebuild that failed (a stored package that does not verify, a component that does
            // not compile) is not tried again on every request: wait out the backoff first.
            let now = std::time::Instant::now();
            if let Err(wait) = self.refresh_backoff.check(now) {
                return Err(erp_fields::Error::new(
                    erp_fields::ErrorKind::Concurrency,
                    format!(
                        "The registry could not be rebuilt and the next attempt is in {} s; the node stays in maintenance until it succeeds",
                        wait.as_secs() + 1
                    ),
                ));
            }
            match self.compose_current(false).await {
                Ok((auth, rpc, assets)) => {
                    self.refresh_backoff.succeeded();
                    self.pending_grants_run
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                    Ok(
                        erp_server::web::Publication::new(auth, rpc, Vec::new())
                            .with_assets(assets),
                    )
                }
                Err(error) => {
                    match record_refresh_failure(
                        &self.refresh_backoff,
                        &self.pending_grants_run,
                        &error,
                        std::time::Instant::now(),
                    ) {
                        Some(wait) => {
                            tracing::error!(%error, retry_in_s = wait.as_secs(), "registry rebuild failed; backing off")
                        }
                        None => {
                            tracing::warn!(%error, "registry rebuild waits for runtime grant publication; no backoff")
                        }
                    }
                    Err(error)
                }
            }
        })
    }
    fn install<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a erp_security::auth::SessionSecret,
        names: Vec<String>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_server::web::Publication>> {
        Box::pin(async move {
            self.mutate_native(auth, secret, names, NativeOperation::Install)
                .await
                .map_err(install_failure_bridge)
        })
    }
    fn upgrade<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a erp_security::auth::SessionSecret,
        names: Vec<String>,
    ) -> erp_orm::BoxFuture<
        'a,
        std::result::Result<erp_server::web::Publication, erp_server::web::LifecycleFailure>,
    > {
        Box::pin(async move {
            self.mutate_native(auth, secret, names, NativeOperation::Upgrade)
                .await
        })
    }
    fn force_demo<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a erp_security::auth::SessionSecret,
    ) -> erp_orm::BoxFuture<
        'a,
        std::result::Result<erp_server::web::Publication, erp_server::web::LifecycleFailure>,
    > {
        Box::pin(async move {
            self.mutate_native(auth, secret, Vec::new(), NativeOperation::ForceDemo)
                .await
        })
    }
    fn uninstall<'a>(
        &'a self,
        auth: &'a Arc<Auth>,
        secret: &'a erp_security::auth::SessionSecret,
        names: Vec<String>,
        impact: String,
    ) -> erp_orm::BoxFuture<
        'a,
        std::result::Result<erp_server::web::Publication, erp_server::web::LifecycleFailure>,
    > {
        Box::pin(async move {
            self.mutate_native(auth, secret, names, NativeOperation::Uninstall { impact })
                .await
        })
    }
}
enum NativeOperation {
    Install,
    Upgrade,
    /// Load the demo data of every installed addon; carries no addon names.
    ForceDemo,
    Uninstall {
        impact: String,
    },
}

fn lifecycle_module_error(error: erp_modules::Error) -> erp_server::web::LifecycleFailure {
    match error {
        erp_modules::Error::CommitOutcomeUnknown { source } => {
            erp_server::web::LifecycleFailure::Unknown(source)
        }
        other => erp_server::web::LifecycleFailure::Refused(module_error(other)),
    }
}
fn install_failure_bridge(failure: erp_server::web::LifecycleFailure) -> erp_fields::Error {
    match failure {
        erp_server::web::LifecycleFailure::Refused(error) => error,
        other => erp_fields::Error::programming(
            "Native installation requires outcome reconciliation or publication recovery",
        )
        .with_source(other),
    }
}
fn committed(
    stage: erp_server::web::PublicationStage,
    error: erp_fields::Error,
) -> erp_server::web::LifecycleFailure {
    erp_server::web::LifecycleFailure::Committed { stage, error }
}
impl NativeLifecycle {
    async fn mutate_native(
        &self,
        auth: &Arc<Auth>,
        secret: &erp_security::auth::SessionSecret,
        names: Vec<String>,
        operation: NativeOperation,
    ) -> std::result::Result<erp_server::web::Publication, erp_server::web::LifecycleFailure> {
        use erp_server::web::LifecycleFailure;
        use erp_server::web::{AssetAdmission, Publication, PublicationStage};
        // Before the engine runs nothing can have committed: these are refusals.
        let _installing = self.installing.try_lock().map_err(|_| {
            LifecycleFailure::Refused(erp_fields::Error::new(
                erp_fields::ErrorKind::Concurrency,
                "A native installation is already running",
            ))
        })?;
        let modules = if matches!(operation, NativeOperation::ForceDemo) {
            Vec::new()
        } else {
            addon_detail::modules(names).map_err(LifecycleFailure::Refused)?
        };
        if let NativeOperation::Uninstall { impact } = &operation {
            let digest = impact.strip_prefix("uninstall-v1:").ok_or_else(|| {
                LifecycleFailure::Refused(erp_fields::Error::validation(
                    "Invalid uninstall impact fingerprint",
                ))
            })?;
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(LifecycleFailure::Refused(erp_fields::Error::validation(
                    "Invalid uninstall impact fingerprint",
                )));
            }
            // Source preflight can refuse before the engine reaches its authorizer. Admit
            // the primary administrator session before any composition diagnostic is exposed.
            let mut begun = auth
                .begin_session(
                    secret,
                    erp_orm::CredentialOptions::new().read_only("native uninstall admission"),
                )
                .await
                .map_err(LifecycleFailure::Refused)?;
            let records = begun
                .tx
                .env()
                .records("ir.module.module")
                .map_err(LifecycleFailure::Refused)?;
            addon_detail::authorize(&mut begun.tx, &records)
                .await
                .map_err(LifecycleFailure::Refused)?;
            begun
                .tx
                .rollback()
                .await
                .map_err(LifecycleFailure::Refused)?;
        }
        let authorizer = SessionAuthorizer { auth, secret };
        let outcome = match &operation {
            NativeOperation::Install => self.system.install_authorized(&modules, &authorizer).await,
            NativeOperation::Upgrade => self.system.upgrade_authorized(&modules, &authorizer).await,
            NativeOperation::ForceDemo => self.system.force_demo_authorized(&authorizer).await,
            NativeOperation::Uninstall { impact } => {
                self.system
                    .uninstall_authorized(&modules, impact, &authorizer)
                    .await
            }
        }
        .map_err(lifecycle_module_error)?;
        if !outcome.post_commit.is_clean() {
            return Err(committed(
                PublicationStage::PostCommit,
                erp_fields::Error::programming("Native lifecycle post-commit work is incomplete"),
            ));
        }
        let grant_error = |error| committed(PublicationStage::RuntimeGrants, error);
        let mut tx = outcome.orm.begin(SUPERUSER_ID).await.map_err(grant_error)?;
        grant_runtime(&mut tx, &self.config)
            .await
            .map_err(cli_field_error)
            .map_err(grant_error)?;
        let installed = match &operation {
            NativeOperation::Install => {
                // Installation retains its historical response: every currently installed addon.
                let rows = tx
                    .query(
                        "SELECT name FROM ir_module_module WHERE state='installed' ORDER BY name",
                        &[],
                    )
                    .await
                    .map_err(grant_error)?;
                rows.into_iter()
                    .map(|row| {
                        row.try_get::<_, String>(0).map_err(|error| {
                            erp_fields::Error::db("Invalid installed addon name").with_source(error)
                        })
                    })
                    .collect::<erp_fields::Result<Vec<_>>>()
                    .map_err(grant_error)?
            }
            NativeOperation::Upgrade => outcome
                .installed
                .iter()
                .map(|name| name.as_str().to_owned())
                .collect(),
            NativeOperation::ForceDemo | NativeOperation::Uninstall { .. } => Vec::new(),
        };
        // The engine commit is already known. Any grant commit uncertainty is a committed
        // RuntimeGrants failure, never an unknown engine outcome or a reason to replay.
        tx.commit().await.map_err(grant_error)?;
        let (runtime, rpc, assets) = self.compose_current(false).await.map_err(|error| {
            // The readiness check shares composition with startup. A runtime role whose
            // grants are still incomplete is a grant-stage failure, not a build failure.
            let stage = if error.message() == RUNTIME_GRANTS_PENDING {
                PublicationStage::RuntimeGrants
            } else {
                PublicationStage::Recomposition
            };
            committed(stage, error)
        })?;
        match &assets {
            AssetAdmission::Published(_) => {}
            AssetAdmission::Broken(reason) => {
                return Err(committed(
                    PublicationStage::Assets,
                    erp_fields::Error::programming(reason.to_string()),
                ));
            }
            _ => {
                return Err(committed(
                    PublicationStage::Assets,
                    erp_fields::Error::programming("Unknown native asset admission state"),
                ));
            }
        }
        Ok(match operation {
            NativeOperation::Install => Publication::new(runtime, rpc, installed),
            NativeOperation::Upgrade => Publication::upgrade(
                runtime,
                rpc,
                outcome
                    .upgraded
                    .iter()
                    .map(|name| name.as_str().to_owned())
                    .collect(),
                installed,
            ),
            NativeOperation::ForceDemo => {
                let failed: Vec<(String, String)> = outcome
                    .data_notices
                    .iter()
                    .filter_map(|notice| match notice.kind() {
                        erp_data::load::NoticeKind::DemoFailed { error } => {
                            Some((notice.module().to_owned(), error.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                // only addons that have demo files (a failed one is in `failed`)
                let loaded = outcome
                    .admitted_addons()
                    .iter()
                    .filter(|entry| !entry.manifest().demo().is_empty())
                    .map(|entry| entry.name().as_str().to_owned())
                    .filter(|name| !failed.iter().any(|(module, _)| module == name))
                    .collect();
                Publication::force_demo(runtime, rpc, loaded, failed)
            }
            NativeOperation::Uninstall { .. } => Publication::uninstall(
                runtime,
                rpc,
                outcome
                    .uninstalled
                    .iter()
                    .map(|name| name.as_str().to_owned())
                    .collect(),
            ),
        }
        .with_assets(assets))
    }
}

fn module_error(error: erp_modules::Error) -> erp_fields::Error {
    match error {
        erp_modules::Error::Orm(error) => error,
        erp_modules::Error::UnknownModule { names } => erp_fields::Error::validation(format!(
            "Unknown linked addons: {}",
            names
                .iter()
                .map(ModuleName::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        other => erp_fields::Error::new(erp_fields::ErrorKind::UserError, other.to_string())
            .with_source(other),
    }
}
fn cli_field_error(error: CliError) -> erp_fields::Error {
    match error {
        CliError::Access(error) => *error,
        other => erp_fields::Error::new(erp_fields::ErrorKind::Programming, other.to_string())
            .with_source(other),
    }
}
struct NativeHost {
    languages: Vec<String>,
}
impl erp_rpc::RpcHost for NativeHost {
    fn language_active(&self, code: &str) -> erp_fields::Result<bool> {
        Ok(self.languages.iter().any(|language| language == code))
    }
    fn has_capability(
        &self,
        _: &erp_orm::Principal,
        _: &erp_orm::Capability,
    ) -> erp_fields::Result<bool> {
        // This composition does not enable rpc.error_debug. Refuse any future accidental use
        // until a transaction-bound capability provider is supplied; never invent authority.
        Err(erp_fields::Error::programming(
            "Native browser error-debug capability source is not configured",
        ))
    }
}
async fn runtime_rpc(
    auth: &Arc<Auth>,
    config: &ServerConfig,
) -> erp_fields::Result<Arc<erp_rpc::RpcService>> {
    let mut tx = auth.orm().begin(SUPERUSER_ID).await?;
    let languages = erp_security::auth::context::installed_languages(&mut tx).await?;
    tx.rollback().await?;
    Ok(Arc::new(erp_rpc::RpcService::new(
        auth.orm().clone(),
        NativeHost { languages },
        erp_rpc::RpcConfig::builder()
            .body_bytes(config.startup_limits().rpc_body_bytes())
            .result_bytes(config.startup_limits().rpc_result_bytes())
            .build()?,
    )))
}

// The catalogue binder is part of native composition. The offered set is the module system's
// view of the addon directory (linked natives plus data-only folders); lifecycle state always
// comes from PostgreSQL.
static CATALOG_SYSTEM: std::sync::OnceLock<Arc<ModuleSystem>> = std::sync::OnceLock::new();
static CATALOG_RPC: erp_orm::rpc::RpcImpl = erp_orm::rpc::RpcImpl {
    addon: "base",
    model: "ir.module.module",
    method: "erp_available_addons",
    decl: erp_orm::rpc::RpcDecl::Expose {
        effect: erp_orm::rpc::Effect::Read,
        receiver: erp_orm::rpc::Receiver::Model,
        params: &[],
        bind: catalog,
    },
    file: file!(),
    line: line!(),
};
/// Direct dependencies by technical name, sorted, as the catalogue's `depends` field lists them.
fn sorted_depends(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = names.collect();
    names.sort();
    names
}
fn catalog<'a>(
    tx: &'a mut erp_orm::Transaction,
    records: erp_orm::Records,
    _: erp_orm::rpc::RpcArgs,
) -> erp_orm::BoxFuture<'a, std::result::Result<erp_fields::Value, erp_orm::rpc::BindError>> {
    Box::pin(async move {
        addon_detail::authorize(tx, &records).await?;
        let env = records.env();
        let system = CATALOG_SYSTEM.get().ok_or_else(|| {
            erp_fields::Error::programming("The addon catalogue is not bound to a module system")
        })?;
        let catalog = system.human_catalog().await.map_err(module_error)?;
        for (name, reason) in &catalog.refused {
            // Loud, never a silent skip: an operator sees why an addon of the directory is absent.
            eprintln!("addon catalogue: {name} is not offered for installation: {reason}");
        }
        let names: Vec<&str> = catalog.offered.iter().map(|m| m.name().as_str()).collect();
        let rows = tx
            .query(
                "SELECT name,state FROM ir_module_module WHERE name=ANY($1)",
                &[&names],
            )
            .await?;
        let mut states = std::collections::HashMap::new();
        for row in rows {
            let name: String = row
                .try_get(0)
                .map_err(|error| erp_fields::Error::db(error.to_string()).with_source(error))?;
            let state: String = row
                .try_get(1)
                .map_err(|error| erp_fields::Error::db(error.to_string()).with_source(error))?;
            states.insert(name, state);
        }
        let mut entries = Vec::new();
        for manifest in &catalog.offered {
            // An addon of the directory that the database has not listed yet is simply not
            // installed; installing it registers it.
            let state = match states.get(manifest.name().as_str()).map(String::as_str) {
                None | Some("uninstalled") => "uninstalled",
                Some("installed") => "installed",
                Some(_) => return Err(erp_fields::Error::new(
                    erp_fields::ErrorKind::Concurrency,
                    "Addon catalogue is in a transitional state; retry after lifecycle recovery",
                )
                .into()),
            };
            entries.push(serde_json::json!({
                "name": manifest.name().as_str(),
                "label": manifest.shortdesc(),
                "version": manifest.version().to_string(),
                "state": state,
                "installable": manifest.installable(),
                "summary": manifest.summary(),
                "category": manifest.category(),
                "author": manifest.author(),
                "application": manifest.application(),
                "website": manifest.website(),
                "license": manifest.license(),
                "depends": sorted_depends(manifest.depends().iter().map(|name| name.as_str().to_owned())),
            }));
        }
        // An uninstalled row
        // the directory does not offer stays out (native code that is not linked, a package that
        // was removed): the web client cannot install it; the operator's `erp addon install` does.
        for row in tx
            .query(
                "WITH RECURSIVE path(id, parent_id, name, depth) AS ( \
                     SELECT id, parent_id, name::text, 1 FROM ir_module_category \
                     UNION ALL \
                     SELECT path.id, c.parent_id, c.name || '/' || path.name, path.depth + 1 \
                     FROM path JOIN ir_module_category c ON c.id = path.parent_id \
                     WHERE path.depth < 16) \
                 SELECT m.name, m.shortdesc, COALESCE(m.latest_version, ''), m.state, \
                        COALESCE(m.summary, ''), COALESCE(m.author, ''), m.application, \
                        COALESCE((SELECT p.name FROM path p \
                                  WHERE p.id = m.category_id AND p.parent_id IS NULL), ''), \
                        COALESCE(m.website, ''), COALESCE(m.license, ''), \
                        ARRAY(SELECT d.name::text FROM ir_module_module_dependency d \
                              WHERE d.module_id = m.id ORDER BY d.name) \
                 FROM ir_module_module m \
                 WHERE NOT (m.name = ANY($1)) AND m.state NOT IN ('uninstalled', 'uninstallable') \
                 ORDER BY m.name",
                &[&names],
            )
            .await?
        {
            let column = |index: usize| -> std::result::Result<String, erp_fields::Error> {
                row.try_get::<_, Option<String>>(index)
                    .map_err(|error| erp_fields::Error::db(error.to_string()).with_source(error))?
                    .ok_or_else(|| {
                        erp_fields::Error::programming(format!(
                            "ir.module.module column {index} is null for a listed addon"
                        ))
                    })
            };
            let name = column(0)?;
            let state = match column(3)?.as_str() {
                "installed" => "installed",
                _ => {
                    return Err(erp_fields::Error::new(
                        erp_fields::ErrorKind::Concurrency,
                        "Addon catalogue is in a transitional state; retry after lifecycle recovery",
                    )
                    .into());
                }
            };
            let application: bool = row
                .try_get::<_, Option<bool>>(6)
                .map_err(|error| erp_fields::Error::db(error.to_string()).with_source(error))?
                .ok_or_else(|| {
                    erp_fields::Error::programming(format!(
                        "ir.module.module.application is null for addon {name}"
                    ))
                })?;
            entries.push(serde_json::json!({
                "label": column(1)?,
                "version": column(2)?,
                "state": state,
                "installable": false,
                "summary": column(4)?,
                "category": column(7)?,
                "author": column(5)?,
                "application": application,
                "website": column(8)?,
                "license": column(9)?,
                "depends": row
                    .try_get::<_, Vec<String>>(10)
                    .map_err(|error| erp_fields::Error::db(error.to_string()).with_source(error))?,
                "name": name,
            }));
        }
        let value = serde_json::json!({"wire_major":1,"registry_version":env.registry().generation().to_string(),"addons":entries});
        let document: erp_views::wire::AddonCatalog =
            serde_json::from_value(value).map_err(|_| {
                erp_fields::Error::programming("Native catalogue failed its wire contract")
            })?;
        Ok(erp_fields::Value::Json(Arc::new(
            serde_json::to_value(document).map_err(|_| {
                erp_fields::Error::programming("Native catalogue serialization failed")
            })?,
        )))
    })
}

// Read only committed tenant-local assets for registry publication. No source collection or
// compiler is reachable from this function or the HTTP asset handler.
async fn published_ui_assets(
    orm: &Orm,
    store: &Arc<erp_server::assets::AssetStore>,
) -> erp_fields::Result<Arc<erp_server::assets::PublishedAssets>> {
    let mut tx = orm
        .begin_opts(erp_orm::BeginOptions::new(SUPERUSER_ID).read_only("UI asset publication"))
        .await?;
    let identity = erp_addon_web::assets::store::current_identity(&mut tx, "en_US", "main")
        .await?
        .ok_or_else(|| {
            erp_fields::Error::validation(
                "UI assets are not published; operator startup admission is required",
            )
        })?;
    let mut generation = erp_server::assets::AssetGeneration::new(&identity.1, identity.2.clone())
        .map_err(|error| erp_fields::Error::validation(error.to_string()).with_source(error))?;
    let limit = usize::try_from(store.limit()).map_err(|error| {
        erp_fields::Error::validation("assets.memory_bytes cannot be represented on this host")
            .with_source(error)
    })?;
    let retained = erp_addon_web::assets::store::retained_bounded(
        &mut tx,
        "en_US",
        "main",
        crate::ui_assets::RUNTIME_ABI,
        limit,
    )
    .await?;
    let bundle = retained
        .iter()
        .find(|bundle| bundle.key() == identity.2)
        .ok_or_else(|| {
            erp_fields::Error::validation("current UI asset generation is not retained")
        })?;
    for retained_bundle in &retained {
        if retained_bundle.mode() != "prod" {
            return Err(erp_fields::Error::validation(
                "development UI assets cannot enter serving retention",
            ));
        }
        for blob in retained_bundle.blobs() {
            let source = erp_server::assets::AssetSource::with_encodings(
                &format!("{}.{}", blob.sha256(), blob.extension()),
                blob.media_type(),
                blob.bytes().to_vec(),
                blob.gzip().to_vec(),
                blob.brotli().to_vec(),
            )
            .map_err(|error| erp_fields::Error::validation(error.to_string()).with_source(error))?;
            generation = generation.asset(source).map_err(|error| {
                erp_fields::Error::validation(error.to_string()).with_source(error)
            })?;
        }
    }
    for (specifier, url) in bundle.imports() {
        generation = generation
            .import(specifier, url)
            .map_err(|error| erp_fields::Error::validation(error.to_string()).with_source(error))?;
    }
    for (path, url) in bundle.static_files() {
        generation = generation
            .static_file(path, url)
            .map_err(|error| erp_fields::Error::validation(error.to_string()).with_source(error))?;
    }
    let published = generation
        .build(store)
        .map_err(|error| erp_fields::Error::validation(error.to_string()).with_source(error))?;
    tx.rollback().await?;
    Ok(Arc::new(published))
}

/// How long a refused request-path asset re-admission is answered from memory before one
/// request retries it against the same durable authority row. The retry lets a replica recover
/// once old pinned generations drain, without a reload per request in between.
const ASSET_REFUSAL_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// One refused re-admission: the authority row it was refused for and the refusal itself.
struct AssetRefusal {
    key: String,
    retention: String,
    at: std::time::Instant,
    kind: erp_fields::ErrorKind,
    message: String,
}

/// Remembers the last refused request-path asset re-admission (PERF.SERVE-ASSETS lead r1 MUST-2).
///
/// While the authority row (generation key and retention list) is unchanged and the retry
/// interval has not passed, [`Self::refused`] returns the same refusal, so a persistent
/// `assets.memory_bytes` refusal costs one authority read per request instead of a full bounded
/// reload serialized under the snapshot lock. A changed row, an elapsed interval or a successful
/// admission ([`Self::clear`]) lets the next request try again.
#[derive(Default)]
struct AssetRefusalGate(std::sync::Mutex<Option<AssetRefusal>>);

impl AssetRefusalGate {
    fn lock(&self) -> erp_fields::Result<std::sync::MutexGuard<'_, Option<AssetRefusal>>> {
        self.0
            .lock()
            .map_err(|_| erp_fields::Error::programming("asset refusal gate lock poisoned"))
    }

    /// The remembered refusal for this authority row, while its retry interval lasts.
    fn refused(
        &self,
        key: &str,
        retention: &str,
        now: std::time::Instant,
    ) -> erp_fields::Result<Option<erp_fields::Error>> {
        Ok(self
            .lock()?
            .as_ref()
            .filter(|refusal| {
                refusal.key == key
                    && refusal.retention == retention
                    && now.saturating_duration_since(refusal.at) < ASSET_REFUSAL_RETRY
            })
            .map(|refusal| erp_fields::Error::new(refusal.kind, refusal.message.clone())))
    }

    fn record(
        &self,
        key: &str,
        retention: &str,
        error: &erp_fields::Error,
        now: std::time::Instant,
    ) -> erp_fields::Result<()> {
        *self.lock()? = Some(AssetRefusal {
            key: key.to_owned(),
            retention: retention.to_owned(),
            at: now,
            kind: error.kind(),
            message: error.message().to_owned(),
        });
        Ok(())
    }

    fn clear(&self) -> erp_fields::Result<()> {
        *self.lock()? = None;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "asset refusal gate tests")]
mod asset_refusal_gate_tests {
    use super::*;

    #[test]
    fn a_refused_readmission_is_answered_from_memory_until_the_row_or_interval_changes() {
        let gate = AssetRefusalGate::default();
        let start = std::time::Instant::now();
        assert!(gate.refused("k1", "k1", start).unwrap().is_none());
        let refusal = erp_fields::Error::new(
            erp_fields::ErrorKind::Validation,
            "asset publication refused: assets.memory_bytes admission limit exceeded",
        );
        gate.record("k1", "k1", &refusal, start).unwrap();
        let again = gate
            .refused("k1", "k1", start + std::time::Duration::from_secs(29))
            .unwrap()
            .unwrap();
        assert_eq!(again.kind(), refusal.kind());
        assert_eq!(again.message(), refusal.message());
        // A different generation key or retention list is a different authority row: it retries.
        assert!(gate.refused("k2", "k1", start).unwrap().is_none());
        assert!(gate.refused("k1", "k1,k2", start).unwrap().is_none());
        // Once the interval passes, one request retries the same row.
        assert!(
            gate.refused("k1", "k1", start + ASSET_REFUSAL_RETRY)
                .unwrap()
                .is_none()
        );
        gate.clear().unwrap();
        assert!(gate.refused("k1", "k1", start).unwrap().is_none());
    }
}

impl erp_server::web::AssetAuthority for NativeLifecycle {
    fn admit<'a>(
        &'a self,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Arc<erp_server::assets::PublishedAssets>>> {
        Box::pin(async move {
            let client = self.publication_pool.get().await.map_err(|error| {
                erp_fields::Error::db("asset authority connection unavailable").with_source(error)
            })?;
            let rows=client.query("SELECT m.bundle_key,CASE WHEN octet_length(m.runtime_version)<=16 THEN m.runtime_version ELSE NULL END,(SELECT string_agg(h.bundle_key,',' ORDER BY h.bundle_key) FROM (SELECT bundle_key FROM ir_ui_asset_bundle WHERE language=m.language AND lazy_group=m.lazy_group AND runtime_version=m.runtime_version AND (expires_at IS NULL OR expires_at>CURRENT_TIMESTAMP) ORDER BY bundle_key LIMIT 257) h) FROM ir_ui_asset_bundle m WHERE m.language='en_US' AND m.lazy_group='main' AND m.expires_at IS NULL",&[]).await
                .map_err(|error| erp_fields::Error::db("asset authority read failed").with_source(error))?;
            let [row] = rows.as_slice() else {
                return Err(erp_fields::Error::validation(
                    "current asset authority is missing or ambiguous",
                ));
            };
            let key: String = row.try_get(0).map_err(|error| {
                erp_fields::Error::db("invalid asset generation key").with_source(error)
            })?;
            let runtime: String = row.try_get(1).map_err(|error| {
                erp_fields::Error::db("invalid asset runtime identity").with_source(error)
            })?;
            let retention: String = row.try_get(2).map_err(|error| {
                erp_fields::Error::db("invalid asset retention state").with_source(error)
            })?;
            if retention.len() > 256 * 65 {
                return Err(erp_fields::Error::validation(
                    "retained asset generations exceed bounded admission",
                ));
            }
            if runtime != crate::ui_assets::RUNTIME_ABI {
                return Err(erp_fields::Error::validation(format!(
                    "published asset runtime '{runtime}' is incompatible with running '{}'",
                    crate::ui_assets::RUNTIME_ABI
                )));
            }
            drop(client);
            // A remembered refusal for this exact authority row answers at once: requests do not
            // queue on the snapshot lock to repeat a reload that just failed (lead r1 MUST-2).
            if let Some(error) =
                self.asset_refusal
                    .refused(&key, &retention, std::time::Instant::now())?
            {
                return Err(error);
            }
            let mut snapshot = self.asset_snapshot.lock().await;
            let Some((orm, assets, old_retention)) = snapshot.as_ref() else {
                return Err(erp_fields::Error::validation(
                    "asset startup admission has not succeeded",
                ));
            };
            if assets.key() == key && old_retention == &retention {
                return Ok(Arc::clone(assets));
            }
            // Requests that queued behind a failed reload see its outcome instead of retrying it.
            if let Some(error) =
                self.asset_refusal
                    .refused(&key, &retention, std::time::Instant::now())?
            {
                return Err(error);
            }
            let current = match published_ui_assets(orm, &self.asset_store).await {
                Ok(current) => current,
                Err(error) => {
                    // HTTP clients get a fixed 503 text; the detail is logged once per retry window.
                    tracing::error!(
                        %error,
                        key = %key,
                        retry_in_s = ASSET_REFUSAL_RETRY.as_secs(),
                        "UI asset re-admission failed; web assets answer 503"
                    );
                    self.asset_refusal.record(
                        &key,
                        &retention,
                        &error,
                        std::time::Instant::now(),
                    )?;
                    return Err(error);
                }
            };
            if current.key() != key {
                return Err(erp_fields::Error::new(
                    erp_fields::ErrorKind::Concurrency,
                    "asset generation changed during admission; retry a safe navigation",
                ));
            }
            self.asset_refusal.clear()?;
            let orm = orm.clone();
            *snapshot = Some((orm, Arc::clone(&current), retention));
            Ok(current)
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "runtime shutdown regression"
)]
mod runtime_shutdown_tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn server_runtime_boundary_returns_without_waiting_for_a_blocking_job() {
        let runtime = runtime().unwrap();
        let (started, ready) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let (job_done, completed) = mpsc::channel();
        let _job = runtime.spawn_blocking(move || {
            started.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(10)).unwrap();
            job_done.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let (returned, stopped) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            finish_server_runtime(runtime, Some(tokio::time::Instant::now()));
            returned.send(()).unwrap();
        });
        // The job stays held until after observing whether runtime ownership returned.
        let bounded_return = stopped.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(5)).unwrap();
        owner.join().unwrap();
        assert!(
            bounded_return.is_ok(),
            "server runtime waited for blocking work beyond its absolute shutdown deadline"
        );
    }
}

#[cfg(test)]
mod hierarchy_tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]

    #[tokio::test]
    async fn server_builder_requires_partner_hierarchy_fence() {
        // The shared server builder installs module-model callbacks, so its fixture
        // needs the module declarations that a real server registry contains.
        let registry = std::sync::Arc::new(
            erp_registry::RegistryBuilder::new([
                &erp_addon_base::ADDON,
                &erp_data::model_data::ADDON,
            ])
            .with_decls(
                erp_modules::base_models::DECLS
                    .iter()
                    .copied()
                    .chain([&erp_data::IR_MODEL_DATA]),
            )
            .build()
            .unwrap(),
        );
        let pool = crate::access::pool_with_size(tokio_postgres::Config::new(), 1).unwrap();
        let orm = super::server_orm_builder(
            registry,
            pool,
            Vec::new(),
            erp_modules::PackageChecks::default(),
        )
        .build()
        .unwrap();
        assert!(
            orm.required_core_tables().iter().any(|entry| {
                entry.table() == "res_partner_hierarchy_fence" && entry.owner() == "base"
            }),
            "server and maintenance ORM builders must retain the base fence linker guard"
        );
    }
}

#[cfg(test)]
mod refresh_backoff_tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]
    use super::*;
    use std::{sync::atomic::AtomicU32, time::Instant};

    /// The install-to-grant-publication gap must not lock a replica out for a backoff window.
    #[test]
    fn pending_grant_publication_does_not_start_a_refresh_backoff() {
        let backoff = crate::node::RefreshBackoff::new();
        let run = AtomicU32::new(0);
        let now = Instant::now();
        let pending =
            erp_fields::Error::new(erp_fields::ErrorKind::Concurrency, RUNTIME_GRANTS_PENDING);
        for _ in 0..PENDING_GRANTS_FREE_RETRIES {
            assert_eq!(record_refresh_failure(&backoff, &run, &pending, now), None);
            assert!(
                backoff.check(now).is_ok(),
                "pending grants must not back off"
            );
        }
    }

    /// A grant fault that never clears is not an install gap: it backs off after the free retries.
    #[test]
    fn standing_pending_grants_back_off_after_the_free_retries() {
        let backoff = crate::node::RefreshBackoff::new();
        let run = AtomicU32::new(0);
        let now = Instant::now();
        let pending =
            erp_fields::Error::new(erp_fields::ErrorKind::Concurrency, RUNTIME_GRANTS_PENDING);
        for _ in 0..PENDING_GRANTS_FREE_RETRIES {
            assert_eq!(record_refresh_failure(&backoff, &run, &pending, now), None);
        }
        assert!(record_refresh_failure(&backoff, &run, &pending, now).is_some());
        assert!(backoff.check(now).is_err());
    }

    /// Another failure ends the run, so a later install gap gets its free retries again.
    #[test]
    fn another_failure_resets_the_pending_run() {
        let backoff = crate::node::RefreshBackoff::new();
        let run = AtomicU32::new(0);
        let now = Instant::now();
        let pending =
            erp_fields::Error::new(erp_fields::ErrorKind::Concurrency, RUNTIME_GRANTS_PENDING);
        for _ in 0..PENDING_GRANTS_FREE_RETRIES {
            record_refresh_failure(&backoff, &run, &pending, now);
        }
        let other = erp_fields::Error::programming("rebuild failed");
        assert!(record_refresh_failure(&backoff, &run, &other, now).is_some());
        assert_eq!(record_refresh_failure(&backoff, &run, &pending, now), None);
    }

    #[test]
    fn other_rebuild_failures_still_back_off() {
        let backoff = crate::node::RefreshBackoff::new();
        let now = Instant::now();
        let leaked = erp_fields::Error::new(
            erp_fields::ErrorKind::Concurrency,
            "Runtime role can access erp_translated_verified; revoke the access and publish grants",
        );
        assert!(record_refresh_failure(&backoff, &AtomicU32::new(0), &leaked, now).is_some());
        assert!(backoff.check(now).is_err());
        let other = crate::node::RefreshBackoff::new();
        let same_text_other_kind =
            erp_fields::Error::new(erp_fields::ErrorKind::Programming, RUNTIME_GRANTS_PENDING);
        assert!(
            record_refresh_failure(&other, &AtomicU32::new(0), &same_text_other_kind, now)
                .is_some()
        );
    }
}

#[cfg(test)]
mod h18_request_composition_tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]
    use erp_sdk::views::{InstalledClosure, OwnerBoundViewTypes, PublishedViewTypes};
    use std::sync::Arc;

    /// table attached to the exact loaded registry, rather than rebuilding without it)
    #[tokio::test]
    async fn request_builder_preserves_the_loaded_view_type_publication() {
        let registry = Arc::new(
            erp_registry::RegistryBuilder::new([
                &erp_addon_base::ADDON,
                &erp_addon_web::ADDON,
                &erp_data::model_data::ADDON,
            ])
            .with_decls(
                erp_modules::base_models::DECLS
                    .iter()
                    .copied()
                    .chain([&erp_data::IR_MODEL_DATA]),
            )
            .build()
            .unwrap(),
        );
        let pairs: Vec<_> = registry
            .addons()
            .iter()
            .map(|addon| (addon.name, addon.depends))
            .collect();
        let table = Arc::new(
            PublishedViewTypes::publish(
                OwnerBoundViewTypes::bind(
                    registry.clone(),
                    &erp_addon_web::view_type_contributions(),
                )
                .unwrap(),
                InstalledClosure::new(&pairs).unwrap(),
                |_, _| false,
            )
            .unwrap(),
        );
        let pool = crate::access::pool_with_size(tokio_postgres::Config::new(), 1).unwrap();
        let source = super::server_orm_builder(
            registry.clone(),
            pool.clone(),
            Vec::new(),
            erp_modules::PackageChecks::default(),
        )
        .generation_attachment(erp_orm::GenerationAttachment::new(
            registry.clone(),
            table.clone(),
        ))
        .build()
        .unwrap();
        let request = super::server_request_orm_builder(
            &source,
            pool,
            Vec::new(),
            erp_modules::PackageChecks::default(),
        )
        .unwrap()
        .build()
        .unwrap();
        let request_table = request
            .env(erp_orm::SUPERUSER_ID, erp_orm::Context::new())
            .generation_attachment::<PublishedViewTypes>()
            .unwrap();
        request_table.require_registry(request.registry()).unwrap();
        assert!(Arc::ptr_eq(&table, &request_table));
        assert_eq!(
            request_table
                .declarations()
                .map(|(_, declaration)| declaration.key())
                .collect::<Vec<_>>(),
            ["form", "list", "search"]
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "typed lifecycle error assertions"
)]
mod lifecycle_failure_tests {
    use super::*;
    use erp_server::web::{LifecycleFailure, PublicationStage};
    use std::error::Error as _;

    #[test]
    fn engine_uncertainty_survives_native_install_compatibility_bridge() {
        let failure = lifecycle_module_error(erp_modules::Error::CommitOutcomeUnknown {
            source: erp_fields::Error::db("lost commit acknowledgement"),
        });
        let error = install_failure_bridge(failure);
        let Some(LifecycleFailure::Unknown(source)) = error
            .source()
            .and_then(|source| source.downcast_ref::<LifecycleFailure>())
        else {
            panic!("install bridge must retain exact typed unknown source")
        };
        assert_eq!(source.kind(), erp_fields::ErrorKind::Database);
        assert_eq!(source.message(), "lost commit acknowledgement");
        let error = install_failure_bridge(committed(
            PublicationStage::RuntimeGrants,
            erp_fields::Error::db("grant connection lost"),
        ));
        let Some(LifecycleFailure::Committed {
            stage: PublicationStage::RuntimeGrants,
            error: source,
        }) = error
            .source()
            .and_then(|source| source.downcast_ref::<LifecycleFailure>())
        else {
            panic!("grant failure must remain known committed")
        };
        assert_eq!(source.message(), "grant connection lost");
        let ordinary = install_failure_bridge(lifecycle_module_error(erp_modules::Error::Orm(
            erp_fields::Error::validation("ordinary refusal"),
        )));
        assert_eq!(ordinary.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(ordinary.message(), "ordinary refusal");
        assert!(ordinary.source().is_none());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "typed admission assertions")]
mod audit_admission_tests {
    use super::*;
    use erp_orm::audit::AuditStoreState;

    /// request role can insert into and a partition for today; one day ahead serves, none refuses,
    /// and every refusal names the state and the fix.
    #[test]
    fn startup_refuses_every_state_that_cannot_take_a_row() {
        audit_admission(&AuditStoreState::Ready).unwrap();
        audit_admission(&AuditStoreState::PartitionWindowShort { days_left: 1 }).unwrap();
        let cases = [
            (AuditStoreState::Unchecked, "the store was not checked yet"),
            (
                AuditStoreState::NotInstalled,
                "erp_event.ir_audit_entry is not installed",
            ),
            (
                AuditStoreState::NoInsertRight,
                "the connected role may not insert the audit columns",
            ),
            (
                AuditStoreState::PartitionWindowShort { days_left: 0 },
                "only 0 day(s) of partitions ahead of today",
            ),
        ];
        for (state, detail) in cases {
            let error = audit_admission(&state).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Concurrency);
            assert_eq!(
                error.message(),
                format!(
                    "audit: the audit store is not ready ({detail}); publish the runtime grants \
                     as the owner role (`erp db publish-grants`) or run `erp audit maintain`"
                )
            );
        }
    }
}
