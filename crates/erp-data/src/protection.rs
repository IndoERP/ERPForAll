//! One protection per external ID, with two PostgreSQL foreign keys: one to the full binding
//! and one to the target record. A model gets a partition on its first protection declaration.
//! PostgreSQL's RI checks arbitrate a protect/delete race even under repeatable read, and run for
//! physical cascades the ORM never dispatches. There is no privileged unlink context flag.

use erp_fields::{Error, Result};
use erp_orm::{Env, PrincipalKind, Transaction};
use erp_registry::ModelSpec;
use erp_sql::{literal, quote, shorten_identifier};

use crate::model_data::{MODEL, XidBinding};

// Literal dots make these internal relations unavailable as ORM table names.
pub const BINDING_TABLE: &str = "erp_record_protection.binding";
const OWNER_TABLE: &str = "erp_record_protection.owner";

fn partition(model: &str) -> String {
    shorten_identifier(&format!("{BINDING_TABLE}_{model}"))
}

async fn schema(tx: &mut Transaction) -> Result<String> {
    let rows = tx.query("SELECT current_schema()::text", &[]).await?;
    rows[0]
        .try_get(0)
        .map_err(|err| Error::programming(format!("Protection requires a current schema: {err}")))
}

async fn table_exists(tx: &mut Transaction, relation: &str) -> Result<bool> {
    let rows = tx
        .query("SELECT to_regclass($1::text) IS NOT NULL", &[&relation])
        .await?;
    rows[0]
        .try_get(0)
        .map_err(|err| Error::db(format!("Could not check protection table: {err}")))
}

fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", quote(schema), quote(table))
}

async fn ensure_partition(
    tx: &mut Transaction,
    schema: &str,
    imd: &str,
    model: &ModelSpec,
) -> Result<()> {
    let parent = qualified(schema, BINDING_TABLE);
    let owners = qualified(schema, OWNER_TABLE);
    let child = qualified(schema, &partition(model.name));
    if table_exists(tx, &child).await? {
        return Ok(());
    }
    tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await?;
    let target = qualified(schema, &model.table);
    let target_kind = tx
        .query(
            "SELECT relkind::text FROM pg_catalog.pg_class WHERE oid = to_regclass($1::text)",
            &[&target],
        )
        .await?;
    let kind = target_kind
        .first()
        .map(|row| row.try_get::<_, &str>(0))
        .transpose()
        .map_err(|err| Error::db(format!("Could not check the protected target: {err}")))?;
    if kind != Some("r") {
        return Err(Error::validation(format!(
            "Model '{}' needs a regular PostgreSQL table for addon record protection.",
            model.name
        )));
    }
    if !table_exists(tx, &parent).await? {
        tx.batch(&format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS erp_record_protection_binding_identity \
                ON {imd} (id, module, name, model, res_id, noupdate); \
             CREATE TABLE {parent} (\
                 xid_id bigint NOT NULL, module varchar NOT NULL, name varchar NOT NULL, \
                 model varchar NOT NULL, res_id bigint NOT NULL, noupdate boolean NOT NULL, \
                 PRIMARY KEY (model, xid_id), \
                 CONSTRAINT erp_record_protection_binding \
                     FOREIGN KEY (xid_id, module, name, model, res_id, noupdate) \
                     REFERENCES {imd} (id, module, name, model, res_id, noupdate) \
                     ON DELETE RESTRICT ON UPDATE RESTRICT\
             ) PARTITION BY LIST (model); \
             CREATE INDEX ON {parent} (res_id); \
             CREATE TABLE {owners} (\
                 module varchar NOT NULL, model varchar NOT NULL, PRIMARY KEY (module, model)\
             )"
        ))
        .await?;
    }
    let model_name = literal(model.name);
    tx.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {child} PARTITION OF {parent} (\
                CONSTRAINT erp_record_protection_target FOREIGN KEY (res_id) \
                    REFERENCES {target} (id) ON DELETE RESTRICT ON UPDATE RESTRICT\
            ) FOR VALUES IN ({model_name})"
        ),
        &[],
    )
    .await?;
    Ok(())
}

/// Set the marker on the exact binding just upserted by the owner. The FK on the target takes
/// the same lock as any other reference, including when a different owner already protects it.
pub(crate) async fn set(
    tx: &mut Transaction,
    env: &Env,
    binding: &XidBinding,
    protect: bool,
) -> Result<()> {
    let principal = tx.env().principal().clone();
    if principal.kind() != PrincipalKind::System || !principal.elevation().is_full() {
        return Err(Error::access(
            "Administrator access is required to change addon record protection.",
        ));
    }
    let schema = schema(tx).await?;
    let parent = qualified(&schema, BINDING_TABLE);
    let owners = qualified(&schema, OWNER_TABLE);
    let owner = binding
        .xid
        .module()
        .ok_or_else(|| Error::invariant("protected external ID has no owner"))?;
    let name = binding.xid.name();
    if !protect {
        if table_exists(tx, &parent).await? {
            tx.execute(
                &format!("DELETE FROM {parent} WHERE module = $1 AND name = $2"),
                &[&owner, &name],
            )
            .await?;
        }
        return Ok(());
    }
    let registry = env.registry();
    let model = registry
        .model(&binding.model)
        .filter(|model| {
            model.has_generic_table()
                && model.kind == erp_registry::ModelKind::Regular
                && model.partition.is_none()
        })
        .ok_or_else(|| {
            Error::validation(format!(
                "Model '{}' does not support addon record protection.",
                binding.model
            ))
        })?;
    let imd = registry
        .model(MODEL)
        .ok_or_else(|| Error::invariant("protected binding has no external-ID model"))?;
    let imd = qualified(&schema, &imd.table);
    ensure_partition(tx, &schema, &imd, model).await?;
    // Older rows can store NULL for the same ordinary-upgrade behavior as false. Give the
    // protected binding a concrete value so changing its upgrade policy also needs its owner.
    tx.execute(
        &format!(
            "UPDATE {imd} SET noupdate = false WHERE module = $1 AND name = $2 AND noupdate IS NULL"
        ),
        &[&owner, &name],
    )
    .await?;
    let rows = tx
        .query(
            &format!(
                "WITH binding AS (\
                     SELECT id, module, name, model, res_id, noupdate FROM {imd} \
                     WHERE module = $1 AND name = $2 AND model = $3 AND res_id = $4\
                 ), added AS (\
                     INSERT INTO {parent} (xid_id, module, name, model, res_id, noupdate) \
                     SELECT * FROM binding ON CONFLICT (model, xid_id) DO NOTHING\
                 ), activated AS (\
                     INSERT INTO {owners} (module, model) \
                     SELECT module, model FROM binding ON CONFLICT DO NOTHING\
                 ) SELECT EXISTS (SELECT 1 FROM binding)"
            ),
            &[&owner, &name, &binding.model, &binding.res_id],
        )
        .await?;
    if !rows[0]
        .try_get::<_, bool>(0)
        .map_err(|err| Error::db(format!("Could not verify protection binding: {err}")))?
    {
        return Err(Error::invariant(format!(
            "External ID {} was not bound before applying its protection.",
            binding.xid
        )));
    }
    Ok(())
}

/// Whether this addon has loaded protection for this model. An addon replacing a legacy
/// deletion guard uses this during the transition. Releasing individual bindings does not
/// turn the legacy guard back on.
pub async fn protection_declarations_loaded(
    tx: &mut Transaction,
    owner: &str,
    model: &str,
) -> Result<bool> {
    let owners = quote(OWNER_TABLE);
    if !table_exists(tx, &owners).await? {
        return Ok(false);
    }
    let rows = tx
        .query(
            &format!("SELECT 1 FROM {owners} WHERE module = $1 AND model = $2"),
            &[&owner, &model],
        )
        .await?;
    Ok(!rows.is_empty())
}

/// Release the bindings that the upgrade already selected as obsolete. A live owner's
/// protection survives regardless of which records the subsequent purge would cascade into.
pub(crate) async fn release_obsolete(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    let table = quote(BINDING_TABLE);
    if table_exists(tx, &table).await? {
        tx.execute(
            &format!("DELETE FROM {table} WHERE xid_id = ANY($1::bigint[])"),
            &[&ids],
        )
        .await?;
    }
    Ok(())
}

/// Release only the departing owners' protection before purging their data. If a model's
/// schema is also leaving, an external ID owned by a staying addon blocks dropping its table.
pub(crate) async fn release_uninstalled(tx: &mut Transaction, owners: &[&str]) -> Result<()> {
    let schema = schema(tx).await?;
    let parent = qualified(&schema, BINDING_TABLE);
    if !table_exists(tx, &parent).await? {
        return Ok(());
    }
    tx.execute(
        &format!("DELETE FROM {parent} WHERE module = ANY($1::varchar[])"),
        &[&owners],
    )
    .await?;
    let registry = std::sync::Arc::clone(tx.registry());
    for model in registry.models() {
        if !model.has_generic_table() || model.stays_without_addons(owners) {
            continue;
        }
        let child = qualified(&schema, &partition(model.name));
        if !table_exists(tx, &child).await? {
            continue;
        }
        let rows = tx
            .query(&format!("SELECT 1 FROM {child} LIMIT 1"), &[])
            .await?;
        if !rows.is_empty() {
            return Err(Error::validation(format!(
                "Cannot uninstall model '{}': an installed addon still protects its records.",
                model.name
            )));
        }
        tx.execute(&format!("DROP TABLE {child}"), &[]).await?;
    }
    Ok(())
}

pub(crate) async fn finish_uninstall(tx: &mut Transaction, owners: &[&str]) -> Result<()> {
    let table = quote(OWNER_TABLE);
    if table_exists(tx, &table).await? {
        tx.execute(
            &format!("DELETE FROM {table} WHERE module = ANY($1::varchar[])"),
            &[&owners],
        )
        .await?;
    }
    Ok(())
}
