// SPDX-License-Identifier: MIT OR Apache-2.0
//! B8 preparation before any lifecycle effect. The fresh transaction retains all guard locks.
use super::*;

pub(super) enum GuardAuthority {
    Unchecked,
    Checked(crate::install::InstallAuthorization),
    Recheck(crate::install::InstallAuthorization),
}
pub(super) struct GuardPreparation {
    pub(super) removing: Vec<ModuleName>,
    pub(super) authority: GuardAuthority,
}

impl ModuleSystem {
    fn removal_names(catalog: &mut Catalog, request: &Request) -> Result<Vec<ModuleName>> {
        if let Request::Uninstall(names) = request {
            catalog.button_uninstall(names)?;
        }
        Ok(catalog
            .rows()
            .filter(|r| r.state() == ModuleState::ToRemove)
            .map(|r| r.name().clone())
            .collect())
    }
    pub(super) async fn prepare_sql_uninstall(
        &self,
        optx: &mut OperationTx,
        request: &Request,
        entries: &IndexMap<ModuleName, AddonEntry>,
        authorizer: Option<&dyn crate::install::InstallAuthorizer>,
    ) -> Result<GuardPreparation> {
        if self.parts.sql_guards.is_empty() {
            return Ok(GuardPreparation {
                removing: Vec::new(),
                authority: GuardAuthority::Unchecked,
            });
        }
        // The trusted primary authority checker is read-only by its existing contract. Run it
        // before catalog/guard diagnostics; unlike addon hooks it is safe in preparation.
        let previous = match authorizer {
            Some(a) => Some(a.authorize(&mut optx.tx).await?),
            None => None,
        };
        optx.verify_intact().await?;
        optx.tx.query(erp_sql::ddl::SCHEMA_LOCK, &[]).await?;
        let (mut catalog, _) = self.load_catalog(&mut optx.tx, entries).await?;
        let installed: Vec<_> = catalog
            .rows()
            .filter(|r| {
                matches!(
                    r.state(),
                    ModuleState::Installed | ModuleState::ToUpgrade | ModuleState::ToRemove
                )
            })
            .map(|r| r.name().clone())
            .collect();
        let removing = Self::removal_names(&mut catalog, request)?;
        let guards: Vec<_> = self
            .parts
            .sql_guards
            .iter()
            .filter(|g| removing.iter().any(|n| n.as_str() == g.addon))
            .collect();
        if guards.is_empty() {
            return Ok(GuardPreparation {
                removing: Vec::new(),
                authority: match previous {
                    Some(a) => GuardAuthority::Checked(a),
                    None => GuardAuthority::Unchecked,
                },
            });
        }
        let old = self.orm_for(&installed, entries)?;
        let registry = old
            .env(SUPERUSER_ID, erp_orm::Context::new())
            .registry()
            .clone();
        let schema = current_schema(&mut optx.tx).await?;
        let mut tables = BTreeSet::new();
        for guard in &guards {
            for table in guard.tables(&registry)? {
                tables.insert((schema.clone(), table));
            }
        }
        if tables.len() > self.parts.guard_limits.models {
            return Err(erp_fields::Error::programming(
                "SQL uninstall guard locked-table limit exceeded",
            )
            .into());
        }
        let tables: Vec<_> = tables.into_iter().collect();
        let mut identities = Vec::new();
        for (schema, table) in &tables {
            let name = format!("{}.{}", erp_sql::quote(schema), erp_sql::quote(table));
            let rows = optx
                .tx
                .query("SELECT pg_catalog.to_regclass($1)::oid::text", &[&name])
                .await?;
            let oid: Option<String> = rows[0].try_get(0).map_err(|e| {
                erp_fields::Error::db("Cannot read guard table identity").with_source(e)
            })?;
            identities.push((
                name,
                oid.ok_or_else(|| {
                    erp_fields::Error::programming("SQL uninstall guard table is missing")
                })?,
            ));
        }
        optx.tx
            .operation_reopen_locked(
                &optx.control,
                &tables,
                OPERATION_LOCK,
                erp_sql::ddl::SCHEMA_LOCK_KEY,
                (optx.limits.lock, optx.limits.statement),
            )
            .await?;
        let mut guard = read_caller_session(&mut optx.tx).await?.guard;
        guard.holds_lock = true;
        optx.guard = guard;
        set_timeouts(
            &mut optx.tx,
            millis(optx.limits.lock),
            millis(optx.limits.statement),
        )
        .await?;
        let (mut fresh, _) = self.load_catalog(&mut optx.tx, entries).await?;
        if Self::removal_names(&mut fresh, request)? != removing {
            return Err(erp_fields::Error::conflict(
                "SQL uninstall removal closure changed during lock-first opening",
            )
            .into());
        }
        for (name, oid) in identities {
            let rows = optx
                .tx
                .query("SELECT pg_catalog.to_regclass($1)::oid::text", &[&name])
                .await?;
            if rows[0]
                .try_get::<_, Option<String>>(0)
                .map_err(|e| {
                    erp_fields::Error::db("Cannot recheck guard table identity").with_source(e)
                })?
                .as_ref()
                != Some(&oid)
            {
                return Err(erp_fields::Error::conflict(
                    "SQL uninstall guard table identity changed during opening",
                )
                .into());
            }
        }
        optx.tx.operation_switch_orm(&optx.control, &old).await?;
        Ok(GuardPreparation {
            removing,
            authority: match previous {
                Some(a) => GuardAuthority::Recheck(a),
                None => GuardAuthority::Unchecked,
            },
        })
    }
    pub(super) async fn evaluate_sql_guards(
        &self,
        tx: &mut Transaction,
        removing: &[ModuleName],
    ) -> Result<()> {
        for guard in self
            .parts
            .sql_guards
            .iter()
            .filter(|g| removing.iter().any(|n| n.as_str() == g.addon))
        {
            guard.evaluate(tx, self.parts.guard_limits).await?;
        }
        Ok(())
    }
}
