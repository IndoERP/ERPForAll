// SPDX-License-Identifier: MIT OR Apache-2.0
//! B8 restrictive removal. Child of schema_sqlobj; shares its provenance and catalog readers.
use super::*;

/// An owner-verified removal plan; statements and exact metadata keys remain private.
#[derive(Debug)]
#[non_exhaustive]
pub struct SqlObjectRemoval {
    transaction: Option<String>,
    statements: Vec<String>,
    addons: Vec<String>,
    keys: Vec<String>,
}

impl Transaction {
    /// Plan native-object removal without DDL. Caller holds lifecycle/schema exclusion.
    /// # Errors
    /// UserError for surviving dependencies, Programming for drift; database errors propagate.
    #[doc(hidden)]
    pub async fn plan_sql_object_removal(
        &mut self,
        control: &crate::OperationControl,
        old: &Registry,
        new: &Registry,
        options: &crate::SchemaSyncOptions,
    ) -> Result<SqlObjectRemoval> {
        self.check_control(control, "Transaction::plan_sql_object_removal")?;
        self.require_system_full("Transaction::plan_sql_object_removal")?;
        if !self.is_operation_managed() {
            return Err(Error::programming(
                "SQL object removal needs module operation control",
            ));
        }
        let graph = old.sql_objects();
        let mut plan = SqlObjectRemoval {
            transaction: None,
            statements: Vec::new(),
            addons: Vec::new(),
            keys: Vec::new(),
        };
        let removed_check = old.models().iter().any(|before| {
            new.model(before.name).is_some_and(|kept| {
                before.table_objects.iter().any(|o| {
                    matches!(o.kind, erp_registry::TableObjectKind::Constraint { .. })
                        && kept.table_object(&o.full_name).is_none()
                })
            })
        });
        if graph.objects().is_empty() && !removed_check {
            return Ok(plan);
        }
        plan.transaction = Some(
            self.query("SELECT pg_catalog.pg_current_xact_id()::text", &[])
                .await?[0]
                .try_get(0)
                .map_err(|e| unreadable("removal transaction identity", e))?,
        );
        if !graph.objects().is_empty() {
            self.verify_sql_object_publication(old, options).await?;
        }
        let removed = |o: &SqlObject| new.sql_objects().get(o.owner(), o.id().key()).is_none();
        for survivor in graph.objects().iter().filter(|o| !removed(o)) {
            for reference in survivor.references() {
                let target = match reference {
                    SqlReference::Object(id)
                        if new.sql_objects().get(id.addon(), id.key()).is_none() =>
                    {
                        Some(id.to_string())
                    }
                    SqlReference::Model(m)
                        if new.model(m.model()).is_none_or(|kept| {
                            old.model(m.model()).is_none_or(|before| {
                                before
                                    .column_fields()
                                    .any(|f| !kept.column_fields().any(|k| k.name == f.name))
                            })
                        }) =>
                    {
                        Some(format!("model {}", m.model()))
                    }
                    SqlReference::Object(_) | SqlReference::Model(_) => None,
                    _ => {
                        return Err(refuse(
                            survivor.id(),
                            "removal reference kind is unsupported",
                        ));
                    }
                };
                if let Some(target) = target {
                    return Err(Error::user(format!(
                        "cannot remove SQL object '{target}': required by '{}'",
                        survivor.id()
                    )));
                }
            }
        }
        let schema = self
            .query("SELECT pg_catalog.current_schema()::text", &[])
            .await?;
        let schema: String = schema[0]
            .try_get(0)
            .map_err(|e| unreadable("removal schema", e))?;
        // Validate every recorded object before constructing any destructive statement. Existing
        // B3 publication checks owners/shapes/grants; these compare physical body/enabled hashes.
        for object in graph.objects() {
            let observed = match object.kind() {
                SqlObjectKind::Function(f) => Some(
                    self.installed(
                        object.id(),
                        "SELECT pg_catalog.pg_get_functiondef(pg_catalog.to_regprocedure($1))",
                        &[&sqlobj::signature(f)],
                    )
                    .await?,
                ),
                SqlObjectKind::Trigger(t) => {
                    let table = format!(
                        "{}.{}",
                        erp_sql::quote(&schema),
                        erp_sql::quote(t.model().table())
                    );
                    Some(self.installed(object.id(),"SELECT pg_catalog.pg_get_triggerdef(t.oid) || ' enabled=' || t.tgenabled::text FROM pg_catalog.pg_trigger t WHERE t.tgrelid=pg_catalog.to_regclass($1) AND t.tgname=$2",&[&table,&t.name()]).await?)
                }
                SqlObjectKind::ForeignKey(_) | SqlObjectKind::TableOptions(_) => Some(sha256(
                    integrity::observed(self, object)
                        .await?
                        .ok_or_else(|| refuse(object.id(), "installed integrity is missing"))?
                        .as_bytes(),
                )),
                SqlObjectKind::Condition(_) => None,
                SqlObjectKind::TableGrant(_) => continue,
                _ => return Err(refuse(object.id(), "removal kind is unsupported")),
            };
            let rows = self
                .query(
                    "SELECT installed_sha256 FROM erp_schema_sql_object WHERE addon=$1 AND key=$2",
                    &[&object.owner(), &object.id().key()],
                )
                .await?;
            let row = rows
                .first()
                .filter(|_| rows.len() == 1)
                .ok_or_else(|| refuse(object.id(), "removal provenance is missing or ambiguous"))?;
            let installed: Option<Vec<u8>> = row
                .try_get(0)
                .map_err(|e| unreadable("removal fingerprint", e))?;
            if installed != observed {
                return Err(refuse(
                    object.id(),
                    "installed definition differs; reconcile through addon upgrade",
                ));
            }
        }
        for object in graph.objects().iter().rev().filter(|o| removed(o)) {
            if let SqlObjectKind::TableGrant(policy) = object.kind() {
                if new.models().iter().any(|m|m.has_table() && m.table==policy.model().table()) && !new.sql_objects().objects().iter().any(|o|matches!(o.kind(),SqlObjectKind::TableGrant(kept) if kept.model().table()==policy.model().table() && kept.binding()==policy.binding() && kept.ops()==policy.ops())) {
                    return Err(Error::user(format!("cannot remove SQL object '{}': required by 'model {}'",object.id(),policy.model().model())));
                }
                continue;
            }
            if let Some(sql) = sqlobj::drop_object(object, &schema)? {
                plan.statements.push(sql);
            }
            plan.addons.push(object.owner().to_owned());
            plan.keys.push(object.id().key().to_owned());
        }
        // CHECK declarations use the existing table-object provenance comment, not B3 rows.
        for before in old.models() {
            let Some(kept) = new.model(before.name) else {
                continue;
            };
            for object in &before.table_objects {
                if kept.table_object(&object.full_name).is_some() {
                    continue;
                }
                if let erp_registry::TableObjectKind::Constraint { definition, .. } = object.kind {
                    let table = format!(
                        "{}.{}",
                        erp_sql::quote(&schema),
                        erp_sql::quote(&before.table)
                    );
                    let rows = self.query("SELECT pg_catalog.obj_description(c.oid,'pg_constraint')=$3 AND c.convalidated AND c.conenforced FROM pg_catalog.pg_constraint c WHERE c.conrelid=pg_catalog.to_regclass($1) AND c.conname=$2",&[&table,&object.full_name,&definition]).await?;
                    if rows.len() != 1
                        || !rows[0]
                            .try_get::<_, Option<bool>>(0)
                            .map_err(|e| unreadable("removed constraint", e))?
                            .unwrap_or(false)
                    {
                        return Err(Error::programming(format!(
                            "removed table constraint '{}' is missing or drifted",
                            object.full_name
                        )));
                    }
                    plan.statements.insert(
                        0,
                        format!(
                            "ALTER TABLE {table} DROP CONSTRAINT {} RESTRICT",
                            erp_sql::quote(&object.full_name)
                        ),
                    );
                }
            }
        }
        Ok(plan)
    }

    /// Execute a previously verified plan inside the same destructive module transaction.
    /// # Errors
    /// Database/dependency failure: caller rolls back the whole lifecycle, never uses CASCADE.
    #[doc(hidden)]
    pub async fn apply_sql_object_removal(
        &mut self,
        control: &crate::OperationControl,
        plan: SqlObjectRemoval,
    ) -> Result<()> {
        self.check_control(control, "Transaction::apply_sql_object_removal")?;
        self.require_system_full("Transaction::apply_sql_object_removal")?;
        if !self.is_operation_managed() {
            return Err(Error::programming(
                "SQL object removal needs module operation control",
            ));
        }
        if let Some(expected) = &plan.transaction {
            let actual: String = self
                .query("SELECT pg_catalog.pg_current_xact_id()::text", &[])
                .await?[0]
                .try_get(0)
                .map_err(|e| unreadable("removal transaction identity", e))?;
            if &actual != expected {
                return Err(Error::programming(
                    "SQL object removal plan belongs to another transaction",
                ));
            }
        }
        for sql in &plan.statements {
            self.batch(sql).await?;
        }
        if !plan.addons.is_empty() {
            self.execute("DELETE FROM erp_schema_sql_object_dependency d USING ROWS FROM (pg_catalog.unnest($1::text[]),pg_catalog.unnest($2::text[])) AS u(a,k) WHERE d.addon=u.a AND d.key=u.k",&[&plan.addons,&plan.keys]).await?;
            self.execute("DELETE FROM erp_schema_sql_object o USING ROWS FROM (pg_catalog.unnest($1::text[]),pg_catalog.unnest($2::text[])) AS u(a,k) WHERE o.addon=u.a AND o.key=u.k",&[&plan.addons,&plan.keys]).await?;
        }
        Ok(())
    }
}
